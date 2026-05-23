# bosun-API: proto sketch P0 (pull-модель)

Date: 2026-05-23
Status: draft for discussion

После цепочки research/правок к концу дня 2026-05-23 модель упростилась радикально. Ключевая идея от пользователя: bundle меняется редко (10–15 раз/неделю), нет realtime-требований, серверу не нужна сложная rollout-машина. Клиент сам в цикле спрашивает «какая моя целевая версия?», если расходится — скачивает и применяет, потом репортит. Сервер один раз в редкий момент может «толкнуть» клиента ускорить проверку.

Это убирает целый класс проблем: command queue, leader-elected dispatcher, NOTIFY/LISTEN, multi-pod state synchronization, expansion target staleness, rollout state machine.

Старая version с push-моделью и rollouts удалена. Её можно посмотреть в истории git до коммита 08e6ea6.

## Архитектурные решения (зафиксированы 2026-05-22..23)

- **Server-side:** новый API namespace `BosunAPI` внутри существующего chiit-server (Go). Никакого отдельного процесса, общая команда, общий deployment, общая PG.
- **Auth:** ECDSA-ключи общая таблица `chiit_validators` с chiit-агентами. Бесшовный переход: bosun-client при старте проверяет `/etc/chiit/client.pem`, использует тот же приватный ключ — Bootstrap НЕ нужен.
- **Bundle storage:** PG bytea, immutable (только INSERT, никогда не UPDATE/DELETE).
- **Кэширование:** агрессивное, иммутабельность даёт нам право. LRU 5 bundle'ов в каждом pod'е.
- **Pull-модель:** клиент сам инициирует. Сервер только отвечает на запросы и опционально «kick'ает» streaming канал.
- **Тайминги увеличены:** клиент проверяет target_version раз в 30 секунд (это и есть heartbeat). Operator commands НЕ нужны для apply-flow.
- **Web UI:** нет. Operator через CLI с admin-token / Keycloak JWT.
- **Bundle upload:** только через CI/CD автоматизацию, CLI оператора не предусмотрен.

## Файл `chiit-server/api/bosun/v1/bosun.proto`

```protobuf
syntax = "proto3";

package ozon.infrastructure.cloudozon.chiit.bosun.v1;

option go_package = "gitlab.ozon.ru/infrastructure/cloudozon/chiit/chiit-server/api/bosun/v1;bosunv1";

import "google/protobuf/timestamp.proto";
import "chiit/api.proto";  // VaultOut, CertificateData

// BosunAPI — control plane для bosun-client (Rust SCM-агент).
// Реализован как новый namespace в chiit-server. Auth:
// - Bootstrap — registration_token (одноразовый Vault-secret).
// - Все остальные agent-RPC — ECDSA-подпись (host, createdAt, sign) поверх
//   общей таблицы с chiit-агентами.
// - IssueOperatorAction — admin-token (Vault) или Keycloak JWT.
service BosunAPI {

  // --- BOOTSTRAP ---

  // Регистрация новой ноды. Только если на ноде нет ни /etc/bosun/client.pem,
  // ни legacy /etc/chiit/client.pem. Server подписывает cert на присланный
  // public key, кладёт запись в chiit_validators.
  rpc Bootstrap(BootstrapIn) returns (BootstrapOut);

  // --- PULL MODEL ---

  // Главный endpoint клиента. Раз в 30 секунд клиент спрашивает свой
  // target_version. Если не совпадает с локальным — клиент скачает bundle
  // (GetBundleManifest + GetBundleBlob) и применит. Также может вернуться
  // bundle_missing если оператор задал target на снятую/несуществующую
  // версию — клиент остаётся на текущей.
  rpc GetTargetVersion(GetTargetVersionIn) returns (GetTargetVersionOut);

  // Server-streaming kick-channel. Открывается ОДИН stream на жизнь
  // bosun-client'а. Server опционально шлёт Kick{} когда хочет ускорить
  // pull-loop клиента (например после массового UPDATE target_version у
  // оператора). Клиент на Kick делает GetTargetVersion вне очереди.
  // На отвал stream'а — exponential backoff reconnect.
  rpc Subscribe(SubscribeIn) returns (stream Kick);

  // Клиент после успешного / неуспешного apply'я.
  rpc ReportApplyResult(ReportApplyResultIn) returns (ReportApplyResultOut);

  // --- BUNDLE ---

  rpc GetBundleManifest(GetBundleManifestIn) returns (GetBundleManifestOut);

  // Стрим chunks по 256KB. LRU 5 bundle'ов в каждом pod'е — cache hit
  // отдаёт chunks без обращения к PG.
  rpc GetBundleBlob(GetBundleBlobIn) returns (stream BundleChunk);

  // --- SECRETS / PKI (тонкие обёртки над chiit-handler'ами) ---

  rpc VaultGet(VaultGetIn) returns (.chiit.VaultOut);
  rpc GetCert(GetCertIn) returns (.chiit.CertificateData);

  // --- INVENTORY ---

  rpc StorageHostInventoryGet(StorageHostInventoryGetIn) returns (StorageHostInventoryGetOut);

  // --- OPERATOR (admin-token / Keycloak JWT) ---

  // Точечный override: жёстко установить target_version на список host'ов
  // (минуя rollout-постепенность). Реализация = UPDATE bosun_clients
  // SET target_version=... WHERE host IN (...). Для emergency или ручной
  // отладки одной ноды.
  rpc SetTargetVersion(SetTargetVersionIn) returns (SetTargetVersionOut);

  // Постепенный rollout с failure-rate halt. Сервер сам в фоновом цикле
  // расширяет cohort: батчами апдейтит target_version на части хостов из
  // селектора, считает actual failure rate через bosun_clients и при
  // превышении max_failure_rate переходит в state=halted (UPDATE'ы
  // прекращаются, уже dispatched клиенты продолжают применять). Клиенты
  // сами пулят и катят в своём pull-loop'е — никакого push-канала.
  rpc IssueRollout(IssueRolloutIn) returns (IssueRolloutOut);
  rpc GetRolloutStatus(GetRolloutStatusIn) returns (GetRolloutStatusOut);
  rpc PauseRollout(RolloutControlIn) returns (RolloutControlOut);
  rpc ResumeRollout(RolloutControlIn) returns (RolloutControlOut);
  rpc AbortRollout(RolloutControlIn) returns (RolloutControlOut);

  // Только для случаев когда оператор хочет ускорить раскатку: server
  // пройдёт по live Subscribe-stream'ам матчингу target и отправит Kick.
  rpc KickRollout(KickRolloutIn) returns (KickRolloutOut);

  // Read-only обзор для оператора.
  rpc GetClientState(GetClientStateIn) returns (GetClientStateOut);
  rpc CountByVersion(CountByVersionIn) returns (CountByVersionOut);
}

// ============================================================================
// Bootstrap
// ============================================================================

message BootstrapIn {
  string host = 1;                           // fqdn
  string registration_token = 2;             // shared per-park secret (как у chiit)
  string platform = 3;                       // "linux/amd64", и т.п.
  string bosun_version = 4;
  bytes  public_key_pem = 5;                 // ECDSA P-256, сгенерированный клиентом
}

message BootstrapOut {
  bytes  cert_pem = 1;                       // ECDSA cert, подписан chiit CA
  string cert_serial = 2;
  google.protobuf.Timestamp cert_not_after = 3;
}

// ============================================================================
// Pull-модель: target version + report
// ============================================================================

message GetTargetVersionIn {
  string host = 1;                           // как в ECDSA-сигнатуре
  string bosun_version = 2;
  uint64 current_version = 3;                // что у клиента сейчас (0 = ничего не применено)
  string sharding_key = 4;                   // обычно cluster_name; для pod redirect
}

message GetTargetVersionOut {
  uint64 target_version = 1;                 // 0 = «никакой target не задан, ничего не делай»
  bool   pod_redirect = 10;                  // true если этот pod не должен обслуживать клиента
  string redirect_addr = 11;                 // куда переподключиться
}

message Kick {
  // Пустой message; событие = «иди сейчас сделай GetTargetVersion». Не несёт
  // payload'а намеренно: серверу нужно только триггернуть, клиент сам
  // запросит свежий target.
}

message SubscribeIn {
  string host = 1;
  string sharding_key = 2;                   // тот же что в GetTargetVersion, для pod redirect
}

message ReportApplyResultIn {
  string host = 1;
  uint64 applied_version = 2;                // версия которую клиент только что прокатил
  bool   success = 3;                        // true = applied, false = failed
  google.protobuf.Timestamp finished_at = 4;
  int32  exit_code = 5;                      // bosun apply exit code, 0..130
  string error_excerpt = 6;                  // первые ~256 байт error message на failed (без полного лога — для запроса логов есть отдельный канал в будущем)
}

message ReportApplyResultOut {
  // Server подтверждает receipt; UPDATE bosun_clients уже выполнен.
}

// ============================================================================
// Bundle
// ============================================================================

message GetBundleManifestIn {
  string host = 1;
  uint64 version = 2;
}

message GetBundleManifestOut {
  uint64 version = 1;
  bytes  sha256 = 2;                         // 32 байта
  bytes  signature = 3;                      // ed25519 поверх sha256
  uint64 size_bytes = 4;
  repeated string tags = 5;
  google.protobuf.Timestamp published_at = 6;
}

message GetBundleBlobIn {
  uint64 version = 1;
}

message BundleChunk {
  uint32 chunk_index = 1;
  bytes  data = 2;                            // 256KB
  bool   is_last = 3;
}

// ============================================================================
// Secrets / Inventory (тонкие обёртки)
// ============================================================================

message VaultGetIn {
  string host = 1;
  string path = 2;
}

message GetCertIn {
  string host = 1;
  int64  cert_id = 2;
  string cert_type = 3;
}

message StorageHostInventoryGetIn {
  string host = 1;
}

message StorageHostInventoryGetOut {
  string cluster_name = 1;
  string patroni_cluster = 2;
  string etcd_cluster = 3;
  string severity_class = 4;
  string env = 5;
  map<string, string> extra = 6;
}

// ============================================================================
// Operator
// ============================================================================

message SetTargetVersionIn {
  // Auth: admin-token header ИЛИ x-bearer-token (Keycloak JWT).
  uint64 target_version = 1;
  Target target = 2;
  string reason = 3;                         // для audit_log
}

message Target {
  repeated string hosts = 1;
  repeated string clusters = 2;              // expanded через storage-inventory
  string severity_class = 3;                 // "low" / "medium" / "high"
}

message SetTargetVersionOut {
  uint32 hosts_updated = 1;
}

message IssueRolloutIn {
  uint64 target_version = 1;                 // версия в bosun_bundles
  Target target = 2;                          // селектор cohort'а
  google.protobuf.Duration over_duration = 3; // за это время раскатать
  float  max_failure_rate = 4;                // 0.0..1.0; превышение → state=halted
  string issued_by = 5;                       // для audit (или взять из auth-контекста)
  string reason = 6;
}

message IssueRolloutOut {
  string rollout_id = 1;
  uint32 total_targets = 2;
  google.protobuf.Timestamp scheduled_completion = 3;
}

message GetRolloutStatusIn {
  string rollout_id = 1;
}

message GetRolloutStatusOut {
  string rollout_id = 1;
  RolloutState state = 2;
  string halt_reason = 3;

  uint32 total_targets = 10;
  uint32 dispatched_targets = 11;             // UPDATE'нутых в bosun_clients.target_version
  uint32 succeeded_targets = 12;              // last_applied_version == rollout.target_version
  uint32 failed_targets = 13;                 // last_apply_success=false на этой версии

  float  current_failure_rate = 20;

  google.protobuf.Timestamp issued_at = 30;
  google.protobuf.Timestamp started_at = 31;
  google.protobuf.Timestamp halted_at = 32;
  google.protobuf.Timestamp completed_at = 33;
}

enum RolloutState {
  ROLLOUT_STATE_UNSPECIFIED = 0;
  ROLLOUT_STATE_PENDING     = 1;
  ROLLOUT_STATE_RUNNING     = 2;
  ROLLOUT_STATE_PAUSED      = 3;
  ROLLOUT_STATE_HALTED      = 4;
  ROLLOUT_STATE_ABORTED     = 5;
  ROLLOUT_STATE_COMPLETED   = 6;
}

message RolloutControlIn {
  string rollout_id = 1;
  string reason = 2;
}

message RolloutControlOut {
  RolloutState new_state = 1;
}

message KickRolloutIn {
  Target target = 1;                         // тот же селектор
}

message KickRolloutOut {
  uint32 clients_kicked = 1;                 // только тех у кого есть live Subscribe
}

message GetClientStateIn {
  string host = 1;
}

message GetClientStateOut {
  string host = 1;
  uint64 target_version = 2;
  uint64 last_applied_version = 3;
  google.protobuf.Timestamp last_applied_at = 4;
  google.protobuf.Timestamp last_seen_at = 5;
}

message CountByVersionIn {
  Target target = 1;                         // ограничение query'я, опционально
}

message CountByVersionOut {
  // Сколько клиентов на какой last_applied_version.
  map<uint64, uint32> applied = 1;
  // Сколько клиентов с заданным target_version который ещё НЕ применён.
  map<uint64, uint32> pending = 2;
}
```

## PG schema (минимальная)

```sql
-- Главная таблица. Всё про конкретного клиента — в одной строке.
CREATE TABLE bosun_clients (
    host                  TEXT PRIMARY KEY,
    target_version        BIGINT,                                       -- что оператор задал; NULL = «ничего не делай»
    last_applied_version  BIGINT,                                       -- что клиент реально прокатил
    last_applied_success  BOOLEAN,                                      -- true=success, false=failed, NULL=ничего не пробовали
    last_applied_at       TIMESTAMPTZ,
    last_seen_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),           -- любой контакт (GetTargetVersion / Subscribe / Report)
    bosun_version         TEXT,                                          -- последняя репортнутая версия бинаря
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Для оператора: «сколько ещё не применили target=N в severity:low»
CREATE INDEX ON bosun_clients(target_version, last_applied_version);
-- Для админских query'ев «когда последний раз ноду видели»
CREATE INDEX ON bosun_clients(last_seen_at);

-- План раскатки. Одна строка на rollout. Никаких per-host записей —
-- расширение target_version у клиентов идёт через UPDATE bosun_clients.
CREATE TABLE bosun_rollouts (
    rollout_id          UUID PRIMARY KEY,
    target_version      BIGINT NOT NULL REFERENCES bosun_bundles(version),
    target_spec         JSONB NOT NULL,                                 -- сериализованный Target (hosts/clusters/severity)
    over_duration_sec   INT NOT NULL,
    max_failure_rate    REAL NOT NULL,
    state               TEXT NOT NULL DEFAULT 'pending',                -- enum
    halt_reason         TEXT,
    issued_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    issued_by           TEXT NOT NULL,
    started_at          TIMESTAMPTZ,
    halted_at           TIMESTAMPTZ,
    completed_at        TIMESTAMPTZ
);

CREATE INDEX ON bosun_rollouts(state) WHERE state IN ('pending', 'running');

-- Bundle blob immutable
CREATE TABLE bosun_bundles (
    version       BIGSERIAL PRIMARY KEY,
    sha256        BYTEA NOT NULL UNIQUE,
    blob          BYTEA NOT NULL,
    signature     BYTEA NOT NULL,
    tags          TEXT[] NOT NULL,
    published_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_by  TEXT NOT NULL
);

-- Реестр pods для consistent hashing redirect
CREATE TABLE bosun_pods (
    pod_id         TEXT PRIMARY KEY,
    addr           TEXT NOT NULL,
    started_at     TIMESTAMPTZ NOT NULL,
    last_ping_at   TIMESTAMPTZ NOT NULL,
    draining       BOOLEAN NOT NULL DEFAULT FALSE
);
```

`bosun_clients` обновляется на четырёх путях:
- `GetTargetVersion` → `UPDATE last_seen_at = NOW(), bosun_version = $1`.
- `ReportApplyResult(success=true)` → `UPDATE last_applied_version, last_applied_at, last_seen_at`.
- `Bootstrap` → `INSERT ON CONFLICT DO UPDATE last_seen_at, created_at` (если новый host).
- `SetTargetVersion` → `UPDATE target_version` для матча Target.

Никаких bosun_heartbeats, bosun_active_sessions, bosun_commands_queue, bosun_rollouts. Если позже понадобится дополнительная метрика — добавляется колонкой в `bosun_clients`.

## Lifecycle bosun-client

1. **Старт.** Если есть `/etc/bosun/client.pem` — использовать. Если есть `/etc/chiit/client.pem` (legacy) — скопировать его как bosun.pem (тот же приватный ключ). Если нет — Bootstrap с registration_token.
2. **Subscribe stream.** Один gRPC server-stream на жизнь процесса. На отвал — exp backoff reconnect.
3. **Pull-loop.** Каждые 30 секунд:
   - `GetTargetVersion(host, current_version, sharding_key)` → `target_version`.
   - Если `pod_redirect = true` → переподключиться по `redirect_addr` (новый Subscribe, новый pull-loop).
   - Если `target_version == 0` или `target_version == current_version` → ничего не делать.
   - Иначе:
     - `GetBundleManifest(target_version)` → sha256+signature.
     - Если локальный cache соответствует — переходить к apply.
     - Иначе `GetBundleBlob(target_version)` → стрим chunks → verify sha256+signature → tempfile rename.
     - `bosun apply --bundle=...`.
     - `ReportApplyResult(applied_version, success, exit_code, error_excerpt)`.
4. **Kick-channel.** Между тиками 30s клиент слушает Subscribe stream. Server может прислать `Kick{}` — клиент сразу делает GetTargetVersion (не ждёт 30 секунд).
5. **Завершение.** Закрыть stream gracefully. На systemd stop — без специальных действий, переподключится при следующем старте.

## Lifecycle оператора (rollout)

Постепенная раскатка новой версии bundle на 1500 хостов severity=low с автоматическим halt при превышении failure rate:

```
1. CI публикует bundle: PublishBundle(blob, sig, tags) → version=42

2. Оператор:
   bosun rollout --version=42 --severity=low \
                 --over 30m --max-failure-rate 0.05

   → IssueRollout(target_version=42,
                  Target{severity_class="low"},
                  over_duration=30m, max_failure_rate=0.05)
   → INSERT bosun_rollouts (state=pending)
   → возврат rollout_id, total_targets=1500

3. Background worker (один на chiit-server, leader-elected через
   advisory_lock) каждую минуту:
   a) Берёт running rollouts.
   b) Для каждого считает по bosun_clients:
      - dispatched = COUNT(target_version >= rollout.target_version
                           AND host IN cohort)
      - succeeded  = COUNT(last_applied_version = rollout.target_version
                           AND last_applied_success = true
                           AND host IN cohort)
      - failed     = COUNT(last_applied_version = rollout.target_version
                           AND last_applied_success = false
                           AND host IN cohort)
      - failure_rate = failed / (succeeded + failed)
   c) Если failure_rate > max_failure_rate:
      UPDATE bosun_rollouts SET state='halted',
                                halted_at=NOW(),
                                halt_reason='...'
      (новые батчи не dispatch'ятся; уже dispatched клиенты дойдут сами)
   d) Иначе:
      expected = total_targets * elapsed / over_duration
      diff = expected - dispatched
      Если diff > 0:
        SELECT host FROM bosun_clients
        WHERE host IN cohort
          AND (target_version IS NULL OR target_version < $target)
        LIMIT diff
      UPDATE bosun_clients SET target_version=$target WHERE host IN (...)
      INSERT в audit_log

4. Клиенты в pull-loop'е (раз в 30s) видят новый target_version, скачивают
   bundle (GetBundleManifest+Blob), применяют, репортят результат через
   ReportApplyResult.

5. Оператор мониторит:
   bosun rollout-status <rollout_id>
   → GetRolloutStatus → dispatched=600/1500, succeeded=540, failed=12,
                        failure_rate=2.2%, ETA через ~18m

6. Если оператор хочет вмешаться:
   bosun rollout-pause / --resume / --abort
   → PauseRollout/ResumeRollout/AbortRollout
   → UPDATE bosun_rollouts SET state='paused' / 'running' / 'aborted'
   Worker сам видит изменение state и реагирует.

7. Severity-aware multi-stage: оператор делает несколько rollout'ов подряд
   (low → medium → high), либо запускает их параллельно с разными
   max_failure_rate. Один rollout = один severity класс.
```

Failure-rate halt автоматический (server-side), при превышении max_failure_rate worker сам переходит в halted и больше target_version'ов не апдейтит. Уже dispatched клиенты доедут (они уже знают target=42 из своего pull). Если оператор хочет вернуть назад — Abort + новый rollout с target_version=41.

Альтернатива (ручной точечный override): `SetTargetVersion(host=N1, target_version=41)` — без rollout state-machine, мгновенный UPDATE.

### State machine rollout

```
pending → running (старт background worker'ом)
running → paused (PauseRollout) → running (ResumeRollout)
running → halted (failure_rate > max) → running (ResumeRollout с оператором по своему решению)
running / paused / halted → aborted (AbortRollout, без возврата)
running → completed (когда succeeded + failed == total_targets)
```

## Pod redirect (без изменений)

Consistent hashing `crc32(sharding_key) % active_pods`. Если клиент попал не на свой pod — в ответе GetTargetVersion возвращается `pod_redirect=true` + `redirect_addr`. Клиент переподключается. Реестр `bosun_pods` рефрешится раз в 30 секунд, mature_threshold нового pod'а — 60 секунд.

## Bundle blob дispатч

Server-side handler `GetBundleBlob`:

```go
func (s *bosunSrv) GetBundleBlob(req *bosunv1.GetBundleBlobIn, stream bosunv1.BosunAPI_GetBundleBlobServer) error {
    // LRU memory-cache последних 5 версий в каждом pod'е (bundle = небольшие
    // tar.gz с starlark/jinja-text, помещаются в RAM целиком). Cache hit
    // отдаёт chunks без обращения к PG. Cache miss — SELECT blob FROM
    // bosun_bundles WHERE version=$1, помещаем в LRU, стримим chunks.
    // На rollout одной версии в PG идёт ровно один SELECT на pod, не на
    // клиента.
    ...
}
```

Client-side (bosun-client Rust):

```
1. GetBundleManifest(version) → sha256, signature
2. Если /var/lib/bosun/bundles/<version>.sha256 совпадает — пропустить download.
3. Иначе GetBundleBlob → собирать chunks в tempfile.
4. Verify sha256 + ed25519 signature над manifest'ом (CA = chiit CA / /etc/bosun/ca.pem).
5. Atomic rename tempfile → /var/lib/bosun/bundles/<version>.tar.gz.
6. bosun apply --bundle=...
```

PG-нагрузка: ровно N_pods SELECT'ов в PG на одну новую версию (не 60k).

## Реализационные заметки

### Файлы в chiit-server

```
chiit-server/
├── api/bosun/v1/bosun.proto                       # выше
├── internal/bosun/
│   ├── server.go                                  # реализация BosunAPI
│   ├── bootstrap.go
│   ├── pull.go                                    # rpc GetTargetVersion + ReportApplyResult
│   ├── subscribe.go                               # stream Kick
│   ├── bundle.go                                  # GetBundleManifest + GetBundleBlob + LRU cache
│   ├── operator.go                                # SetTargetVersion + KickRollout + GetClientState + CountByVersion
│   ├── secrets.go                                 # VaultGet + GetCert (обёртки)
│   ├── inventory.go                               # StorageHostInventoryGet (обёртка)
│   ├── pods.go                                    # pod registry для redirect
│   └── interceptors.go                            # ECDSA + admin-token + JWT
├── internal/bundle_storage/                       # CRUD над bosun_bundles
├── migrations/                                    # 20260523_bosun.sql (3 таблицы)
└── cmd/server/main.go                             # +RegisterService(bosunSvc)
```

### Server-side тайминги

| Параметр | Значение | Зачем |
|---|---|---|
| Client pull-loop | 30 sec | heartbeat + target version check одновременно |
| Bundle LRU в pod'е | 5 версий | bundle = текст, ~50 MB max, RAM дёшев |
| Pod registry refresh | 30 sec | список меняется редко |
| Pod mature-threshold | 60 sec | защита от crashloop reshuffle |
| Pod drain timeout | 30 sec | при SIGTERM на graceful shutdown |
| ECDSA cache | TTL 60 sec (как у chiit) | без отдельной revocation push-инвалидации |

### Что убрано из P0 (по сравнению с прошлой push-моделью)

- `commands_queue` таблица + polling-loop в pod'е — rollout идёт через UPDATE bosun_clients.target_version, не через INSERT в очередь.
- `bosun_heartbeats` отдельная таблица — объединена в `bosun_clients.last_seen_at`.
- `bosun_active_sessions` отдельная таблица — pod_id больше не персистится (Subscribe-stream — in-memory map в pod'е, на reconnect Subscribe просто новый stream).
- `IssueCommand`, `ApplyBundleCommand`, `RunTaskCommand`, `FlushFactsCommand` — для P0 не нужны (apply идёт через pull, не через push-команды).

### Что сохраняется как rollout-state-machine

`bosun_rollouts` остаётся (одна строка на rollout, не per-host), `IssueRollout` + `GetRolloutStatus` + `Pause/Resume/Abort` остаются. Background worker (один лидер на chiit-server через advisory_lock) расширяет cohort через UPDATE bosun_clients, считает actual failure rate из агрегата bosun_clients и при превышении max_failure_rate переходит в halted. Никакого per-host commands_queue.

## Open follow-ups (P1)

- `PublishBundle(blob, sig, tags) → version` — RPC для CI/CD. В P0 описано в proto sketch выше, реализация очевидна.
- `RotateCert`, `RevokeCert` — manual ops для security.
- `GetRSAPairs`, `GetTalosKeys`, `BootstrapBucket` — повторяют chiit handlers 1:1, добавятся когда понадобятся в Starlark glue.
- `GetSeverity`, `GetDatabaseList`, `GetMasterOfPatroniCluster`, `GetWhiteListForResource` — нужны когда автор bundle'а начнёт использовать в Starlark.
- `GetPersonRoles` (Hallpass) — для админских инструментов.
- `RunTask` / ad-hoc one-off команды — если возникнет use case «pg_repack на одной ноде из CLI оператора».

## Что осталось решить (open questions)

1. **Subscribe stream auth.** protoc-gen-scratch поддерживает stream-interceptor (research 2026-05-23) — подтвердить что наш ECDSA interceptor работает на нём.
2. **bosun-clients cleanup.** Host который decommission'ировался — навсегда остаётся в таблице или есть TTL по `last_seen_at`? Пока ничего не чистим (60k rows = тривиально для PG).
3. **CountByVersion для больших cohort'ов.** Если оператор сделает CountByVersion(Target{все 60k}) — нужен индекс. Сейчас покрыто `(target_version, last_applied_version)` и `(last_seen_at)`. Должно хватить, но проверить на реальных query plan'ах.
4. **GetTargetVersion latency budget.** При 60k клиентов × один запрос/30s = 2k QPS. С учётом ECDSA-verify это ~5-10ms/request → один pod держит 200-400 QPS. Дольше — нагрузка на PG (для UPDATE last_seen_at). Возможно нужен batch update last_seen_at (например раз в 5 минут вместо каждого запроса). Это micro-optimization — посмотреть в проде.
