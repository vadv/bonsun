# bosun-API: proto sketch P0 (9 RPC)

Date: 2026-05-23
Status: draft for discussion

После research 2026-05-22 ([bosun-api-in-chiit-server.md](../research/2026-05-22-bosun-api-in-chiit-server.md)) и 2026-05-23 ([codegen-and-scratch.md](../research/2026-05-23-chiit-server-codegen-and-scratch.md)) и закрытия open-questions пользователем:

- Bundle storage → PostgreSQL (`bytea` в bundle blobs table).
- Secrets rotation → нет, как в chiit.
- Web UI → нет, оператор через CLI с admin-token / Keycloak.
- ECDSA-ключи bosun-агентов → совместная таблица с chiit для плавного переезда.
- Sensitivity-маскирование на streaming → не используем (выключаем для `Subscribe`).
- Stream interceptor для auth → protoc-gen-scratch поддерживает.
- TLS на gRPC порту → не важен (warden закрывает в кластере).

Этот документ — proto sketch P0 ручек bosun-API. Реализуется внутри `chiit-server/api/` рядом с существующими `chiit/api.proto` и `pg_shard_manager/api.proto`.

## Policy: bosun-server = chiit-server (бесшовный переход)

Пользователь 2026-05-23:

> «политика: bosun-сервер и chiit-сервер — это одно и то же. Нам нужен бесшовный переход клиентов.»

То есть **никакого отдельного «bosun-server»** как сущности нет. Есть только chiit-server, в котором появляется новый API namespace `BosunAPI`. Один и тот же процесс, один deployment, одна команда сопровождения.

Что это значит конкретно:

- **Один и тот же `host_id` (fqdn)** в `chiit_validators` (или как там она называется). chiit-client и bosun-client делят одну запись на сервере.
- **Общая ECDSA-ключевая инфраструктура.** Если host уже зарегистрирован как chiit-client с действующей подписью — `Bootstrap(host, registration_token)` от bosun-client'а **не** генерирует новый key. Сервер видит существующего host'а, возвращает существующий cert (или просит rotate'нуться если у chiit cert уже expired). Это и есть «бесшовный переход».
- **Общий audit log, severity, canary state, bundle storage.** Одна нода может в transition period иметь и chiit-cron, и bosun-systemd-unit — оба обращаются к тому же серверу через свои API.
- **Никаких миграционных скриптов «переключить host с chiit на bosun»** — оператор просто останавливает один агент и запускает другой. Сервер ничего специального не делает.

Bootstrap-flow (решение 2026-05-23):

```
bosun-client при первом старте:
  1. Если /etc/bosun/client.pem уже есть и валиден → пропустить Bootstrap.
  2. Если /etc/chiit/client.pem есть и валиден (legacy chiit-key) →
     скопировать его как /etc/bosun/client.pem (тот же приватный ключ
     обслуживает chiit-client и bosun-client). Bootstrap НЕ нужен.
  3. Иначе → Bootstrap(host, registration_token, public_key_pem).
  4. Сервер проверяет registration_token (валиден / не expired).
     Если валиден — принимает Bootstrap независимо от того, есть ли
     уже запись под этим host в общей таблице ECDSA-ключей:
        - Если есть chiit-key → ротируется на присланный public_key.
        - Если нет — fresh registration.
     В обоих случаях сервер подписывает новый cert на присланный
     public_key и возвращает PEM в BootstrapOut.
  5. Audit-log запись: «host X re-registered, replaced key Y with key Z».
```

Шаг (2) — реальный бесшовный переход. Оператор просто ставит bosun-client рядом с chiit-client'ом на ноду; bosun использует тот же ECDSA-key. Никакой extra процедуры — ни registration_token не нужен, ни администратор не задействован. Никаких `force_new` флагов и спецсемантики — `registration_token` (когда дойдёт до шага 3) сам является доказательством полномочий ротировать ключ.

## Файл `chiit-server/api/bosun/v1/bosun.proto`

```protobuf
syntax = "proto3";

package ozon.infrastructure.cloudozon.chiit.bosun.v1;

option go_package = "gitlab.ozon.ru/infrastructure/cloudozon/chiit/chiit-server/api/bosun/v1;bosunv1";

import "google/api/annotations.proto";
import "google/protobuf/timestamp.proto";
import "google/protobuf/duration.proto";
// При переиспользовании 1:1 message-типов из chiit/api.proto:
import "chiit/api.proto";  // VaultOut, CertificateData, и т.п.

// BosunAPI — control plane для bosun-client (Rust SCM-агент).
// Реализован как новый namespace в chiit-server. Auth:
// - Bootstrap — registration_token (одноразовый Vault-secret).
// - Все остальные agent-RPC — ECDSA-подпись (host, createdAt, sign) поверх той же
//   таблицы что у chiit-агентов (плавный переезд).
// - IssueCommand — admin-token (Vault) или Keycloak JWT.
service BosunAPI {

  // --- BOOTSTRAP ---

  // Регистрация новой ноды. Клиент передаёт registration_token (выдан оператором
  // или предустановлен в image), плюс host и желаемое имя. Сервер:
  //   1. Проверяет токен в `validator-registration-token` таблице.
  //   2. Генерирует ECDSA-keypair (или принимает public key от клиента — TODO).
  //   3. Кладёт public key в общую таблицу с chiit-агентами.
  //   4. Возвращает signed cert и (если генерирует server-side) private key.
  rpc Bootstrap(BootstrapIn) returns (BootstrapOut);

  // --- SESSION ---

  // Server-streaming подписка на команды. Клиент один раз открывает stream;
  // сервер push'ит Command сообщения по мере появления в commands_queue для
  // этого host. ECDSA-валидация на каждый Subscribe — через scratch
  // stream-interceptor. При disconnect клиент переподключается; сервер по
  // host_id запоминает где остановилась дотавка commands.
  rpc Subscribe(SubscribeIn) returns (stream Command);

  // Лёгкий ping каждые 30s. Клиент репортит свой state (current bundle version,
  // pending defers count, last_apply_at). Сервер обновляет active_sessions
  // таблицу — оператор через GetNodeStatus видит свежие данные.
  rpc Heartbeat(HeartbeatIn) returns (HeartbeatOut);

  // Результат apply'я бандла. Клиент шлёт после каждого bosun apply (как
  // local, так и server-managed). Сервер пишет в audit_log + обновляет
  // active_sessions.last_apply.
  rpc ReportApplyResult(ReportApplyResultIn) returns (ReportApplyResultOut);

  // --- SECRETS / PKI ---

  rpc VaultGet(VaultGetIn) returns (ozon.infrastructure.cloudozon.chiit.VaultOut);

  rpc GetCert(GetCertIn) returns (ozon.infrastructure.cloudozon.chiit.CertificateData);

  // --- INVENTORY ---

  rpc StorageHostInventoryGet(StorageHostInventoryGetIn) returns (StorageHostInventoryGetOut);

  // --- BUNDLE ---

  // Манифест бандла: версия, sha256, signature, какие тэги задействованы.
  // Сам tar.gz клиент достаёт через GetBundleBlob (отдельный stream, P1).
  rpc GetBundleManifest(GetBundleManifestIn) returns (GetBundleManifestOut);

  // --- OPERATOR ---

  // Оператор pushes команду на список target host'ов. Аутентифицируется
  // admin-token или Keycloak. Сервер кладёт записи в commands_queue, активный
  // Subscribe stream подписан и подбирает.
  rpc IssueCommand(IssueCommandIn) returns (IssueCommandOut);
}

// ============================================================================
// Bootstrap
// ============================================================================

message BootstrapIn {
  string host = 1;                           // fqdn ноды
  string registration_token = 2;             // одноразовый Vault-secret (или из bootstrap-secret per-park)
  string platform = 3;                       // "linux/amd64", "linux/musl-amd64", "darwin/arm64"
  string bosun_version = 4;                  // "0.1.0"
  bytes  public_key_pem = 5;                 // public ECDSA key, сгенерированный клиентом (P-256, X9.62)
  string desired_severity = 6;               // optional override для канареечной классификации
}

// Поведение Bootstrap для уже-зарегистрированного host'а (решение
// 2026-05-23): если registration_token валиден, сервер принимает
// присланный public_key_pem и ротирует cert — даже если в общей таблице
// уже лежал chiit'овый ключ. Это и есть «бесшовный переход»:
// оператор останавливает chiit-agent, поднимает bosun-agent, тот
// генерит свой keypair, Bootstrap с тем же registration_token — и
// получает свежий cert без админ-интервенции. Никаких extra полей в
// BootstrapIn не нужно.

message BootstrapOut {
  bytes  cert_pem = 1;                       // ECDSA-cert подписанный chiit CA, validity 1y
  string cert_serial = 2;                    // for audit
  google.protobuf.Timestamp cert_not_after = 3;
  // Сразу bootstrap'а возвращаем минимум для первого apply:
  uint64 initial_bundle_version = 4;         // если задана nodewise политика — версия для этой ноды
  string severity_class = 5;                 // "low" | "medium" | "high"
}

// ============================================================================
// Session
// ============================================================================

message SubscribeIn {
  string host = 1;                           // fqdn ноды (как в ECDSA сигнатуре)
  string bosun_version = 2;
  string current_bundle_version = 3;         // если уже что-то применено
  repeated string capabilities = 4;          // "runr.service", "pg_sql.exec" — фильтрация команд по supported primitives

  // Hint для pod-affinity. Сервер хеширует это поле для определения
  // целевого pod'а. Если пустое — fallback на hash(host). Обычно
  // равно database_name (cluster_name) — все ноды одного кластера
  // attached к одному pod'у, что даёт locality для group-targeting.
  string sharding_key = 5;
}

message Command {
  string command_id = 1;                     // UUID для idempotency и audit-tracking
  google.protobuf.Timestamp issued_at = 2;
  string issued_by = 3;                      // оператор: Keycloak sub или admin-token name

  oneof payload {
    ApplyBundleCommand apply_bundle = 10;
    RunTaskCommand     run_task     = 11;
    FlushFactsCommand  flush_facts  = 12;

    // Server → client управляющие сообщения. См. раздел «Pod redirect».
    RedirectCommand    redirect     = 20;
  }
}

message RedirectCommand {
  string target_pod_addr = 1;                // "chiit-server-3.chiit-server-headless.infra.svc:8443"
  string reason = 2;                         // "hash(database_name) routes to pod-3" / "pod-1 shutting down"
  google.protobuf.Duration grace = 3;        // в течение какого времени клиент должен переподключиться
}

message ApplyBundleCommand {
  uint64 bundle_version = 1;
  bool   dry_run = 2;
  repeated string tags = 3;                  // --tags=production,canary
  google.protobuf.Duration deadline = 4;
}

message RunTaskCommand {
  string task_name = 1;                      // "flush_pg_stat_statements", "vacuum_full:ledger"
  map<string, string> args = 2;
  google.protobuf.Duration deadline = 3;
}

message FlushFactsCommand {
  repeated string facts = 1;                 // ["installed_packages", "pg_users_with_passwords"]
}

message HeartbeatIn {
  string host = 1;
  string current_bundle_version = 2;
  uint32 pending_defers = 3;
  google.protobuf.Timestamp last_apply_at = 4;
  string last_apply_result = 5;              // "success" | "partial" | "failed" | "deferred"
  string bosun_version = 6;
}

message HeartbeatOut {
  // Сервер может попросить клиента ускориться (новые команды появились).
  bool kick_subscribe = 1;
}

message ReportApplyResultIn {
  string host = 1;
  string command_id = 2;                     // если apply вызван командой; пусто если local apply
  uint64 bundle_version = 3;
  google.protobuf.Timestamp started_at = 4;
  google.protobuf.Timestamp finished_at = 5;
  int32  exit_code = 6;                      // 0 / 1 / 2 / 130 (см. bosun exit codes)
  uint32 resources_changed = 7;
  uint32 resources_unchanged = 8;
  uint32 resources_failed = 9;
  uint32 resources_deferred = 10;
  uint32 resources_interrupted = 11;
  repeated ResourceFailure failures = 12;    // первые N failed/interrupted с деталями
  string log_excerpt = 13;                   // последние ~4KB логов на случай постмортема
}

message ResourceFailure {
  string resource_id = 1;
  string resource_kind = 2;                  // "apt.package", "service.unit"
  string reason = 3;                         // "DpkgLocked", "RunrUnavailable", "Apply"
  string excerpt = 4;
  bool   is_deferrable = 5;
}

message ReportApplyResultOut {
  // Пока пусто; в будущем сервер может попросить переотправить с дополнительными
  // деталями или ack rate-limit hint.
}

// ============================================================================
// Secrets
// ============================================================================

message VaultGetIn {
  string host = 1;                           // для авторизации (host has access only to его cluster's секреты)
  string path = 2;                           // "infra/postgresql/<dbname>/<env>:<key>" (новый формат) или legacy
}

message GetCertIn {
  string host = 1;
  int64  cert_id = 2;                        // ID в chiit cert_manager
  string cert_type = 3;                      // "certificate" | "ca_bundle" | "private_key"
}

// ============================================================================
// Inventory
// ============================================================================

message StorageHostInventoryGetIn {
  string host = 1;                           // обычно equals to authenticated host, можно lookup чужой при наличии prv
}

message StorageHostInventoryGetOut {
  // payload как в существующем chiit storage-inventory:
  string cluster_name = 1;
  string patroni_cluster = 2;
  string etcd_cluster = 3;
  string severity_class = 4;
  string env = 5;                            // "production" | "staging"
  map<string, string> extra = 6;             // ad-hoc fields, чтобы не править proto на каждое поле
}

// ============================================================================
// Bundle
// ============================================================================

message GetBundleManifestIn {
  string host = 1;
  uint64 version = 2;                        // 0 = "latest для этого host (по severity + canary rollout)"
}

message GetBundleManifestOut {
  uint64 version = 1;
  bytes  sha256 = 2;                         // 32 байта
  bytes  signature = 3;                      // ed25519 sig поверх (version || sha256)
  uint64 size_bytes = 4;
  repeated string tags = 5;                  // тэги активные в этом bundle
  google.protobuf.Timestamp published_at = 6;
  // Сам blob достаётся отдельным RPC GetBundleBlob (P1, streaming):
  string blob_ref = 7;                       // opaque token: "pg-row:bundle_id=NNN"
}

// Bundle публикация — RPC для CI/CD автоматизации, не для ручного
// оператора. CLI типа `bosun bundle publish` не существует и не
// планируется. Каждый вызов = новая immutable строка в bosun_bundles.
message PublishBundleIn {
  bytes  blob = 1;                            // tar.gz, до ~50MB
  bytes  signature = 2;                       // ed25519 поверх sha256(blob)
  repeated string tags = 3;
  string published_by = 4;                    // service-account name (CI), для audit_log
}

message PublishBundleOut {
  uint64 version = 1;
  bytes  sha256 = 2;
}

// Стриминг blob'а bundle'а. Отдельный RPC, потому что bytea до 50MB не
// влезает в один gRPC message (default max_msg_size = 4MB). Клиент
// перед этим вызовом получает sha256+signature через GetBundleManifest;
// верифицирует blob после получения последнего chunk'а.
message GetBundleBlobIn {
  uint64 version = 1;
}

message BundleChunk {
  uint32 chunk_index = 1;                     // 0-indexed
  bytes  data = 2;                            // обычно 256KB
  bool   is_last = 3;
}

// ============================================================================
// Operator commands
// ============================================================================

message IssueCommandIn {
  // Auth: admin-token header ИЛИ x-bearer-token (Keycloak JWT).
  // Сервер кладёт записи в commands_queue для каждого host в target.
  Target target = 1;

  oneof payload {
    ApplyBundleCommand apply_bundle = 10;
    RunTaskCommand     run_task     = 11;
    FlushFactsCommand  flush_facts  = 12;
  }
}

message Target {
  // Хотя бы один из полей должен быть задан.
  repeated string hosts = 1;                 // явный список
  repeated string clusters = 2;              // expanded через storage-inventory
  string severity_class = 3;                 // "low" / "medium" / "high"
  string canary_percent = 4;                 // "10" → hash(cluster) % 100 < 10
  bool   all = 5;                            // эскалация в "*" — только для emergency-shutdown
}

message IssueCommandOut {
  string command_id = 1;                     // root-id, под ним группируются per-host записи
  uint32 hosts_queued = 2;
  google.protobuf.Timestamp issued_at = 3;
}
```

## Rollout как первоклассный концепт (дополнение от 2026-05-23)

`IssueCommand` отдаёт payload одному или нескольким хостам сразу — это OK для `RunTask` на 5-10 хостах. Для массовых apply на 60k нод нужно orchestrated rollout с rate limit и failure threshold (пользователь 2026-05-23):

> «20% — это означает что раскатка будет длиться очень долго, поэтому оператор должен указывать время за которое раскатываются эти 20%. Запускается раскатка с определённым процентом приемлемых фейлов. Если количество ошибок превышает этот уровень — мы останавливаем раскатку.»

### Дополнительные RPC

```protobuf
service BosunAPI {
  // ... существующие ...

  // Создать orchestrated rollout. В отличие от IssueCommand — сервер сам
  // дозирует команды во времени и следит за failure rate.
  rpc IssueRollout(IssueRolloutIn) returns (IssueRolloutOut);

  // Статус rollout'а: сколько dispatched, succeeded, failed; текущая stage.
  rpc GetRolloutStatus(GetRolloutStatusIn) returns (GetRolloutStatusOut);

  // Управление в полёте.
  rpc PauseRollout(RolloutControlIn) returns (RolloutControlOut);
  rpc ResumeRollout(RolloutControlIn) returns (RolloutControlOut);
  rpc AbortRollout(RolloutControlIn) returns (RolloutControlOut);
}

message IssueRolloutIn {
  // Те же auth-требования что у IssueCommand (admin-token / Keycloak JWT).
  Target target = 1;                          // существующий Target с canary_percent

  oneof payload {
    ApplyBundleCommand apply_bundle = 10;
    RunTaskCommand     run_task     = 11;
  }

  // Распределение команд во времени. `over_duration` — за какое время раскатать
  // весь expanded target. Сервер вычисляет rate = total_hosts / over_duration
  // и dispatch'ит хосты равномерно (с jitter).
  google.protobuf.Duration over_duration = 20;

  // Опциональный hard cap на одновременно «в полёте» команд (acked_at IS NULL).
  // 0 = не ограничивать. Полезно если over_duration слишком короткий и dispatch
  // обгоняет реальную скорость выполнения.
  uint32 max_in_flight = 21;

  // Acceptable failure threshold. Если процент failed > этого — rollout
  // переходит в state=halted и больше команд не выпускает (existing pending
  // продолжают выполняться). Расчёт failed_rate = failed / (succeeded + failed),
  // делается после каждой ack'нутой команды.
  // failure_rate=0.0 — halt при первом же failed.
  // failure_rate=1.0 — не halt'ить никогда.
  float max_failure_rate = 22;

  // Что считать failure: только exit_code != 0 (strict), или ещё partial
  // (exit=1 c resources_failed > 0)? Default — strict.
  FailureMode failure_mode = 23;

  // Опциональное delay между фазами (low → medium → high severity).
  // 0 = phases не используются, размазываем target равномерно.
  google.protobuf.Duration severity_phase_delay = 24;
}

enum FailureMode {
  FAILURE_MODE_UNSPECIFIED = 0;
  FAILURE_MODE_STRICT      = 1;  // только exit_code != 0
  FAILURE_MODE_PARTIAL     = 2;  // exit_code != 0 ИЛИ resources_failed > 0
}

message IssueRolloutOut {
  string rollout_id = 1;
  google.protobuf.Timestamp scheduled_to_start = 2;
  uint32 total_targets = 3;
}

message GetRolloutStatusIn {
  string rollout_id = 1;
}

message GetRolloutStatusOut {
  string rollout_id = 1;
  RolloutState state = 2;
  string halt_reason = 3;                     // если halted

  uint32 total_targets = 10;
  uint32 dispatched_targets = 11;             // успешно dispatched в commands_queue
  uint32 acked_targets = 12;                  // прислали ReportApplyResult
  uint32 succeeded_targets = 13;
  uint32 failed_targets = 14;
  uint32 in_flight_targets = 15;              // dispatched - acked

  float current_failure_rate = 20;

  google.protobuf.Timestamp started_at = 30;
  google.protobuf.Timestamp scheduled_completion = 31;
  google.protobuf.Timestamp halted_at = 32;
  google.protobuf.Timestamp completed_at = 33;
}

enum RolloutState {
  ROLLOUT_STATE_UNSPECIFIED = 0;
  ROLLOUT_STATE_PENDING     = 1;  // создан, ещё не начат
  ROLLOUT_STATE_RUNNING     = 2;
  ROLLOUT_STATE_PAUSED      = 3;  // operator paused
  ROLLOUT_STATE_HALTED      = 4;  // failure_rate threshold пробит
  ROLLOUT_STATE_ABORTED     = 5;  // operator aborted
  ROLLOUT_STATE_COMPLETED   = 6;
}

message RolloutControlIn {
  string rollout_id = 1;
  string reason = 2;                          // для audit log
}

message RolloutControlOut {
  RolloutState new_state = 1;
}
```

### State machine

```
PENDING  → RUNNING (на первый dispatch)
RUNNING  → PAUSED (PauseRollout) → RUNNING (ResumeRollout)
RUNNING  → HALTED (failure_rate exceeded)
RUNNING / PAUSED / HALTED → ABORTED (AbortRollout, operator escalation)
RUNNING  → COMPLETED (все targets ack'нуты)
```

Из HALTED можно ResumeRollout (operator явно решает что фейлы ок); из ABORTED — нельзя.

### Background worker

В `internal/bosun/rollouts/dispatcher.go`:

```go
// Каждые N секунд (5? 10?) сервер:
// 1. Берёт все RUNNING rollouts.
// 2. Для каждого считает: сколько в commands_queue ещё pending,
//    сколько acked, какой failure_rate.
// 3. Если failure_rate > max_failure_rate → переводит в HALTED.
// 4. Иначе вычисляет: сколько новых targets нужно dispatch'нуть к этому
//    моменту по графику. rate = total / over_duration.
//    Если уже dispatched больше — пропускает.
//    Если меньше — dispatch'ит batch (с учётом max_in_flight).
```

dispatch batch на batch вставляет в `bosun_commands_queue`. Pod_Y, который держит Subscribe-стрим этого host'а, увидит запись через polling (см. ниже про command-dispatch loop) — обычно в пределах 500 мс.

### Расширения PG schema

```sql
CREATE TABLE bosun_rollouts (
    rollout_id            UUID PRIMARY KEY,
    issued_at             TIMESTAMPTZ NOT NULL,
    issued_by             TEXT NOT NULL,
    target_spec           JSONB NOT NULL,                -- сериализованный Target
    payload               JSONB NOT NULL,                -- сериализованный oneof
    over_duration_sec     INT NOT NULL,
    max_in_flight         INT NOT NULL,
    max_failure_rate      REAL NOT NULL,
    failure_mode          TEXT NOT NULL,
    severity_phase_delay_sec INT NOT NULL,

    state                 TEXT NOT NULL DEFAULT 'pending', -- enum
    halt_reason           TEXT,

    total_targets         INT NOT NULL,
    dispatched_targets    INT NOT NULL DEFAULT 0,
    acked_targets         INT NOT NULL DEFAULT 0,
    succeeded_targets     INT NOT NULL DEFAULT 0,
    failed_targets        INT NOT NULL DEFAULT 0,

    scheduled_completion  TIMESTAMPTZ NOT NULL,
    started_at            TIMESTAMPTZ,
    halted_at             TIMESTAMPTZ,
    completed_at          TIMESTAMPTZ
);

-- Привязка commands_queue к rollout'у (опциональная, для аудита и сводок):
ALTER TABLE bosun_commands_queue ADD COLUMN rollout_id UUID
    REFERENCES bosun_rollouts(rollout_id);
CREATE INDEX ON bosun_commands_queue(rollout_id) WHERE rollout_id IS NOT NULL;

-- Чтобы worker'у быстро находить running rollouts:
CREATE INDEX ON bosun_rollouts(state) WHERE state IN ('pending', 'running');
```

### Сравнение IssueCommand vs IssueRollout

| Аспект | IssueCommand | IssueRollout |
|---|---|---|
| Use case | task на 5-10 хостов | apply bundle на 1k-60k хостов |
| Время dispatch | мгновенно | размазано по `over_duration` |
| Failure threshold | нет | `max_failure_rate` halt |
| Управление в полёте | нет | Pause / Resume / Abort |
| State tracking | per-host через ReportApplyResult | агрегированный через GetRolloutStatus |
| PG-таблицы | bosun_commands_queue | + bosun_rollouts |

IssueCommand остаётся для «срочных» точечных операций (например `bosun status` на одной ноде для диагностики) или для ad-hoc tasks. IssueRollout — для всего что трогает заметное количество хостов.

## Pod redirect: consistent hashing + shutdown drain

Пользователь 2026-05-23:

> «При подключении клиента должна быть команда Redirect, а также при шатдауне сервера. Мы знаем все поды: chiit и bosun сервера — это всё одно и то же. Поэтому при подключении клиент должен редиректиться на подик, который соответствует хэшу от database name.»

Это даёт два важных свойства:

1. **Locality / cache hit.** Все ноды одного кластера attached к одному pod'у → pod держит in-memory cache для этого кластера (severity, inventory, last-known-bundle) с высоким hit-rate.
2. **Graceful shutdown.** При SIGTERM pod redirect'ит свои активные сессии на остальных pods, не теряя in-flight commands.

### Pod discovery

В PG появляется таблица `bosun_pods` — каждый pod при старте записывает себя:

```sql
CREATE TABLE bosun_pods (
    pod_id         TEXT PRIMARY KEY,             -- $HOSTNAME из k8s downward API
    addr           TEXT NOT NULL,                -- "chiit-server-2.chiit-server-headless.infra.svc:8443"
    started_at     TIMESTAMPTZ NOT NULL,
    last_ping_at   TIMESTAMPTZ NOT NULL,         -- сам себя пингует каждые 5s
    draining       BOOLEAN NOT NULL DEFAULT FALSE -- TRUE если получили SIGTERM
);

CREATE INDEX ON bosun_pods(draining, last_ping_at);
```

Каждый pod держит в памяти snapshot этой таблицы и refresh'ит её через `SELECT * FROM bosun_pods` каждые 5 секунд. Список не-draining pods сортируется по pod_id — это и есть consistent ring для hashing. PG `LISTEN/NOTIFY` намеренно НЕ используется (см. раздел «Command dispatch без LISTEN/NOTIFY») — у этого транспорта в PG нет at-least-once гарантий и нет cross-replica доставки.

### Hash function

Простой `crc32(sharding_key) % len(active_pods)`. Для `SubscribeIn.sharding_key`:

- Если задан (обычно cluster_name из storage-inventory) — используется как-есть.
- Если пустой — fallback на `host`.

Не consistent-hash (типа ring или jump-hash), а простое modulo. На scale-up/down какая-то часть клиентов получит redirect — это OK при rolling-deploy одного pod'а за раз.

### Subscribe flow

```
client → Subscribe(host=N1, sharding_key="ledger-cluster")
   ↓ pod_A handler:
       target_pod_idx = crc32("ledger-cluster") % active_pods_count
       if target_pod_idx != self.pod_idx:
           Send(Command{redirect: {target_pod_addr: pods[target_pod_idx].addr,
                                   reason: "hash routes to pod_C",
                                   grace: 5s}})
           Close stream gracefully.
       else:
           // обычный flow: register session, начать слушать pub/sub.
client получает Command::Redirect → disconnect → dial(target_pod_addr).
```

### Shutdown drain

При получении SIGTERM:

```
1. Pod выставляет `bosun_pods.draining=TRUE`. Другие pods увидят флаг на следующем 5-секундном `bosun_pods` refresh (≤5s lag — приемлемо: новые Subscribe'ы за это окно могут попасть на draining pod, но он их сразу redirect'нет).
2. Active pods пересчитывают active_pods_count БЕЗ этого pod'а.
3. Драинящийся pod проходит по своим live sessions и каждому шлёт
   Command::Redirect с target_pod_addr (по новому hashing).
4. Ждёт `drain_timeout = 30s` (решение 2026-05-23) пока клиенты переподключатся.
5. Закрывает все оставшиеся streams (force).
6. Удаляет себя из `bosun_pods` и выходит.
```

При новом подключении в это окно — pod_A видит drained pod_C в списке и НЕ направит туда нового клиента (active_pods исключает draining).

### Reshuffle на scale-up

При добавлении нового pod_D:

- `bosun_pods` получает запись pod_D с `started_at = NOW()`.
- **Pod НЕ сразу включается** в active_pods для hashing. Решение 2026-05-23: новый pod становится «mature» только после **20 секунд** непрерывного пинга (`NOW() - started_at >= 20s` и не draining).
- До этого pod_D принимает Subscribe запросы как обычный pod (только если кто-то напрямую в него зайдёт через DNS-round-robin), но в `Target(shardingKey)` его не выбирают.
- После 20 секунд pod_D считается готовым; hashing начинает routing'ть на него.
- Существующие сессии на других pods **остаются** на своих местах — только новые / переподключающиеся идут по новому hash. «Soft» reshuffle.

Защита от шторма: если pod_D — flaky (crashloop, перезапускается каждые 5s), он никогда не успеет до mature-status и не вызовет волну redirect'ов на всех остальных pods. Constant=20s — порог, при котором ребалансировка происходит, только когда новый pod действительно стабилен.

Параметр зафиксирован как константа в коде (`mature_threshold = 20 * time.Second`); можно вынести в RT-config если потребуется тюнинг.

### Реализация в коде

`internal/bosun/pods/registry.go`:

```go
// PodRegistry — snapshot active pods, обновляется через goroutine.
type PodRegistry struct {
    mu           sync.RWMutex
    selfPodID    string
    selfPodIdx   int       // позиция в sorted-by-pod_id списке
    activePods   []PodInfo // не-draining
}

// Target возвращает target pod для sharding key. Если совпадает с self —
// пустой PodInfo (значит "оставайся здесь").
func (r *PodRegistry) Target(shardingKey string) PodInfo { ... }
```

`internal/bosun/subscribe.go::onConnect`:

```go
target := pods.Target(req.ShardingKey)
if target.PodID != "" && target.PodID != pods.SelfPodID() {
    return stream.Send(&Command{
        Payload: &Command_Redirect{
            Redirect: &RedirectCommand{
                TargetPodAddr: target.Addr,
                Reason:        fmt.Sprintf("hash routes to %s", target.PodID),
                Grace:         durationpb.New(5 * time.Second),
            },
        },
    })
}
// обычный flow
```

`cmd/server/main.go`:

```go
// SIGTERM handler
go func() {
    <-shutdownCh
    pods.MarkDraining()                 // UPDATE bosun_pods SET draining=TRUE
    sessions.RedirectAll(drainTimeout)  // каждому live stream шлём Redirect
    cancelAll()                         // через drainTimeout — force close
    pods.Deregister()                   // DELETE FROM bosun_pods
    os.Exit(0)
}()
```

## Реализационные заметки

### Файлы которые надо создать

```
chiit-server/
├── api/
│   └── bosun/
│       └── v1/
│           └── bosun.proto            # ← новый файл, выше
├── internal/
│   ├── bosun/
│   │   ├── server.go                  # реализация BosunAPI; struct с зависимостями
│   │   ├── bootstrap.go               # rpc Bootstrap
│   │   ├── subscribe.go               # rpc Subscribe (stream)
│   │   ├── heartbeat.go               # rpc Heartbeat
│   │   ├── report_apply.go            # rpc ReportApplyResult
│   │   ├── vault.go                   # rpc VaultGet (тонкая обёртка над internal/vault)
│   │   ├── cert.go                    # rpc GetCert (обёртка над internal/cert)
│   │   ├── inventory.go               # rpc StorageHostInventoryGet
│   │   ├── bundle.go                  # rpc GetBundleManifest + БД-storage
│   │   ├── issue_command.go           # rpc IssueCommand
│   │   ├── interceptors.go            # ECDSA stream interceptor для Subscribe
│   │   └── sessions.go                # in-memory map active subscribe streams
│   └── bundle_storage/                # новый пакет под bundle blobs в PG
│       ├── postgres.go                # CRUD над bytea
│       └── migrations/
│           └── 20260523_bundles.sql
└── cmd/server/main.go                 # +добавить scratch.RegisterService(bosunSvc)
```

### Сессии и Subscribe stream

В отличие от chiit (unary RPC), bosun использует server-streaming `Subscribe`. Один live-stream держит соединение на минуты-часы. В `internal/bosun/sessions.go`:

```go
type SessionMap struct {
    mu       sync.RWMutex
    sessions map[string]*Session // key = host
}

type Session struct {
    Host           string
    Stream         BosunAPI_SubscribeServer
    Cancel         context.CancelFunc
    Capabilities   []string
    BosunVersion   string
    ConnectedAt    time.Time
    LastHeartbeat  time.Time
}
```

При новом Subscribe старая сессия для того же host отменяется (`session.Cancel()`) и заменяется новой. Сервер использует `host` как primary key — дубликаты не нужны.

`IssueCommand` пишет в `commands_queue` таблицу PG. Доставка — через polling-loop pod'а, не через PG `LISTEN/NOTIFY` (см. ниже «Command dispatch без LISTEN/NOTIFY»).

### Command dispatch без LISTEN/NOTIFY

PG `LISTEN/NOTIFY` имеет известные ограничения для нашего use-case (DBA-perspective):

- Notification теряется если LISTEN'ер отвалился между COMMIT и delivery (нет at-least-once).
- Очередь memory-backed (`pg_notification_queue_usage`), при backpressure начинает блокировать COMMIT.
- Не доставляется на hot-standby — replication-aware дispатча нет.
- Нет ack'ов и нет per-message re-delivery.

Вместо `LISTEN/NOTIFY` — простой polling от каждого pod'а:

```go
// Goroutine в pod'е, запускается при boot, останавливается при SIGTERM.
for {
    select {
    case <-ctx.Done():
        return
    case <-trigger:  // канал, в который пишут при новых connections (чтобы не ждать 500ms на первый command)
    case <-time.After(500 * time.Millisecond):
    }

    rows, _ := db.Query(`
        SELECT command_id, host, payload
        FROM bosun_commands_queue
        WHERE host = ANY($1)
          AND delivered_at IS NULL
        ORDER BY issued_at
        LIMIT 100
    `, sessions.AttachedHostsSnapshot())

    for _, row := range rows {
        // Send в Subscribe stream
        if err := sessions.Send(row.Host, row.Payload); err != nil {
            continue   // stream закрыт — оставляем delivered_at IS NULL, следующий цикл вернётся
        }
        db.Exec(`
            UPDATE bosun_commands_queue
            SET delivered_at = NOW()
            WHERE command_id = $1
              AND delivered_at IS NULL
        `, row.CommandID)
    }
}
```

Поля и инварианты:

- **`$my_attached_hosts`** = keys текущего in-memory map'а Subscribe-стримов pod'а. У каждого pod'а свой set; на pod_redirect host попадает ровно к одному pod'у (consistent hashing), пересечений между pod'ами нет в нормальном режиме.
- **`UPDATE ... WHERE delivered_at IS NULL`** — защита от двойной доставки на edge-cases reshuffle (короткое окно когда host попадает в `attached_hosts` двух pod'ов). Если оба попытаются обновить — один из UPDATE'ов вернёт rowcount=0, та сторона ничего не отправит.
- **`FOR UPDATE SKIP LOCKED` НЕ нужен** — partitioning по host'у уже native, не несколько worker'ов спорят за одну очередь.
- **Latency:** до 500 мс между `IssueCommand` и Send. Для operator-команд приемлемо — это не realtime control.
- **PG QPS:** N_pods × 2 QPS = 4-10 QPS поллинга на нормальном развёртывании. С индексом `bosun_commands_queue(host, delivered_at) WHERE delivered_at IS NULL` — мизерная нагрузка.

Для `bosun_pods` snapshot — отдельный 5-секундный polling. Изменения `draining` field видны pod'ам с ≤5s lag, что приемлемо в shutdown-flow (новые Subscribe'ы которые попадут на draining pod за это окно — сразу redirect'нутся).

### ECDSA-key revocation invalidation

chiit-server держит ARC-cache для ECDSA public keys в `internal/validator/validator.go` с TTL ~60s (баланс PG load — без cache был бы SELECT chiit_validators на каждый запрос от 60k клиентов). Race window для revocation: оператор revoke'нул скомпрометированный ключ → 60s старый ключ всё ещё валиден на pods'ах.

Решение: **polling revocations через timestamp filter**, без `LISTEN/NOTIFY`.

```go
// Отдельный goroutine в каждом pod'е, рядом с командным dispatch loop.
for {
    select {
    case <-ctx.Done():
        return
    case <-time.After(5 * time.Second):
    }

    rows, _ := db.Query(`
        SELECT host
        FROM chiit_validators
        WHERE revoked_at IS NOT NULL
          AND revoked_at > $1
    `, lastCheckTs)

    for _, row := range rows {
        ecdsaCache.Invalidate(row.Host)  // drop entry
    }
    lastCheckTs = time.Now()
}
```

Window для revocation — ≤5s (вместо 60s). PG-нагрузка минимальна благодаря фильтру по `revoked_at > $last_check_ts` (нужен индекс `chiit_validators(revoked_at) WHERE revoked_at IS NOT NULL`). ARC-cache TTL остаётся 60s для штатных reads — баланс PG load сохраняется.

Случай compromised-key-without-revoke (атакующий получил приватный ключ, оператор не знает) — не решается cache, это другой класс проблем (anomaly detection через unusual traffic / IP-источник / т.п.), out of scope текущего design'а.

### PG недоступен

Решение: pod возвращает ошибку (`codes.Unavailable` в gRPC status) на любой запрос требующий PG, не пытается retry в pod'е. Клиент (bosun-client) делает свой retry с экспоненциальным backoff и продолжает работать.

Обоснование: PG равно недоступен всем pod'ам — circuit breaker / cache last-known-state в одном pod'е не помогает. Любая попытка усложнить failover'ом ведёт к inconsistent state. При reset PG все pods восстанавливаются автоматически на следующем polling-цикле.

Healthcheck pod'а должен fail'ить если PG недоступен — k8s liveness probe выведет pod из ротации, новые Subscribe'ы пойдут на живых соседей.

### Bundle blobs в PG

```sql
-- Bundle immutable: только INSERT, никогда не UPDATE/DELETE. Каждая
-- публикация создаёт новую запись с BIGSERIAL version. Если bundle
-- оказался "плохим" — публикуется новая версия, старая остаётся в
-- таблице ради audit/forensics.
CREATE TABLE bosun_bundles (
    version       BIGSERIAL PRIMARY KEY,
    sha256        BYTEA NOT NULL UNIQUE,
    blob          BYTEA NOT NULL,             -- tar.gz, до ~50MB на пакет
    signature     BYTEA NOT NULL,             -- ed25519
    tags          TEXT[] NOT NULL,
    published_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_by  TEXT NOT NULL               -- service-account из CI / Keycloak sub
);
```

CLI оператора `bosun bundle publish` **не существует** — bundle загружается через CI/CD автоматизацию (отдельный orchestrator, который вызывает `PublishBundle` от имени service-account'а). Никаких ручных операций оператора с blob'ом.

`GetBundleManifest` отдаёт metadata + `blob_ref="pg-row:<version>"`. Сам blob — отдельным RPC `GetBundleBlob(version) returns (stream BundleChunk)` (P1, потом).

### Commands queue

```sql
CREATE TABLE bosun_commands_queue (
    command_id    UUID PRIMARY KEY,
    host          TEXT NOT NULL,
    payload       JSONB NOT NULL,             -- сериализованный Command.payload
    issued_at     TIMESTAMPTZ NOT NULL,
    issued_by     TEXT NOT NULL,
    delivered_at  TIMESTAMPTZ,                -- когда head Subscribe доставил клиенту
    acked_at      TIMESTAMPTZ                 -- когда клиент репортнул ReportApplyResult
);

CREATE INDEX ON bosun_commands_queue(host) WHERE delivered_at IS NULL;
```

### Heartbeats и sessions: две отдельные таблицы

По решению пользователя 2026-05-23, heartbeat tracking — **отдельная таблица** от session-state. Heartbeat — write-heavy путь (2k QPS на 60k клиентах × 30s); session-state меняется редко (только connect/disconnect/migration). Хранить их раздельно даёт независимый sizing/indexing/retention.

```sql
-- Heartbeat tracking. Минимальная таблица — последний heartbeat + дата
-- первого появления host'а. Поля будут расширяться (last bundle version,
-- pending defers, last apply result — но это позже, по мере появления
-- сценариев).
CREATE TABLE bosun_heartbeats (
    host          TEXT PRIMARY KEY,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),   -- когда host первый раз появился
    heartbeat_at  TIMESTAMPTZ NOT NULL                  -- последний полученный Heartbeat
);

CREATE INDEX ON bosun_heartbeats(heartbeat_at);          -- для cleanup dead nodes + queries «кто не отвечает»

-- Session state (где живёт stream сейчас + last apply). Это reader-oriented
-- таблица — обновляется только при connect/disconnect/apply, не на каждый
-- heartbeat.
CREATE TABLE bosun_active_sessions (
    host                   TEXT PRIMARY KEY,
    pod_id                 TEXT NOT NULL,                -- replica chiit-server, которая держит Subscribe stream
    bosun_version          TEXT,
    connected_at           TIMESTAMPTZ NOT NULL,
    disconnected_at        TIMESTAMPTZ,                  -- NULL если активен
    current_bundle_version BIGINT,
    last_apply_at          TIMESTAMPTZ,
    last_apply_result      TEXT                          -- "success" / "partial" / "failed" / "deferred"
);
```

#### Handler logic

**Heartbeat handler** (`internal/bosun/heartbeat.go`):

```go
// Hot path. Один UPSERT с COALESCE на created_at — чтобы первое появление
// зафиксировать раз и навсегда.
const upsertHeartbeat = `
INSERT INTO bosun_heartbeats(host, created_at, heartbeat_at)
VALUES ($1, NOW(), NOW())
ON CONFLICT (host) DO UPDATE
SET heartbeat_at = EXCLUDED.heartbeat_at
RETURNING created_at, heartbeat_at;
`
```

**Subscribe connect** (`internal/bosun/subscribe.go`):

```go
// При успешной аутентификации ECDSA + установке stream:
// 1. Записать в active_sessions с pod_id.
// 2. NOT touching bosun_heartbeats — это handle сам Heartbeat RPC.
```

**ReportApplyResult** (`internal/bosun/report_apply.go`):

```go
// Обновляет active_sessions.last_apply_*. Если host не в active_sessions
// (например пришёл от ноды без активного Subscribe — local-mode apply
// репортнул) — INSERT с pod_id='__local__'.
```

#### Расширения для будущего

Поля которые пользователь обозначил как «дальше будут расширяться» — кандидаты:
- `pending_defers` — счётчик из heartbeat payload.
- `bosun_version` — на случай если хотим алертить на ноды с устаревшим бинарём.
- `current_bundle_version` — для quick lookup без JOIN с active_sessions.
- `severity_class` — закэшированный из последнего bootstrap'а.

Добавлять буду по мере появления конкретных сценариев (alerting, диагностический dashboard, mass-targeting), не превентивно.

#### Multi-pod dispatch

При connect клиента pod_X пишет себя в `bosun_active_sessions.pod_id`. Если оператор IssueCommand'ит на host который attached к pod_Y — pod_Y подбирает через свой polling-loop (см. «Command dispatch без LISTEN/NOTIFY») в пределах 500 мс.

### Bundle blob дispатч

Server-side handler `GetBundleBlob`:

```go
func (s *bosunSrv) GetBundleBlob(req *bosunv1.GetBundleBlobIn, stream bosunv1.BosunAPI_GetBundleBlobServer) error {
    // SELECT blob FROM bosun_bundles WHERE version=$1.
    // Стриминг через io.Copy chunks по 256KB, чтобы не держать 50MB в Go heap.
    // chiit-server держит LRU memory-cache последних N версий (default N=3) —
    // при cold-cache rollout 60k нод не идёт прямо в PG.
    ...
}
```

Client-side (bosun-client Rust) before `bosun apply`:

```
1. GetBundleManifest(version) → sha256, signature, tags
2. Если /var/lib/bosun/bundles/<version>.sha256 совпадает с manifest.sha256:
       blob уже cached, skip download.
3. Иначе: GetBundleBlob(version), читать chunks в tempfile.
4. После последнего chunk'а — проверить sha256(tempfile) == manifest.sha256.
5. verify ed25519 signature manifest.signature над manifest.sha256
   (public key chiit CA из /etc/bosun/ca.pem).
6. atomic rename tempfile → /var/lib/bosun/bundles/<version>.tar.gz,
   записать /var/lib/bosun/bundles/<version>.sha256.
7. bosun apply --bundle=/var/lib/bosun/bundles/<version>/.
```

PG-нагрузка при cold-cache initial rollout: 60k × 50MB = ~3TB чтения. Memory-cache на server'е (LRU N=3) даёт ~99% cache-hit rate если все ноды качают одну и ту же версию через rollout.

### Open follow-ups (для P1)

- `PublishBundle(PublishBundleIn) returns (PublishBundleOut)` — INSERT-only, вызывается CI/CD service-account'ом, не оператором. (CLI оператора не предусмотрен.)
- `GetRSAPairs`, `GetTalosKeys`, `BootstrapBucket` — повторяют chiit handlers 1:1.
- `GetSeverity`, `GetDatabaseList`, `GetMasterOfPatroniCluster` — targeting helpers.
- `GetPersonRoles` — RBAC через Hallpass.
- `ReportLogs(stream LogLine)` — стриминг детальных логов для пост-мортема (опционально).
- `RotateCert`, `RevokeCert` — для security-операций (manual).

### Open questions

1. **Bootstrap: cert генерирует server или client?** Сейчас в proto `BootstrapIn.public_key_pem` — клиент генерирует key, server подписывает. Альтернатива — server генерирует и отдаёт private key (проще для клиента, но private key в network).
2. **Subscribe stream timeout.** chiit-server, скорее всего, имеет default 5-10 min timeout. Для long-lived sessions надо настроить keepalive (`grpc.KeepaliveParams`) — какие значения у chiit-server?
3. **`Target.canary_percent`** — это string чтобы поддержать "10.5" в будущем, но сейчас integer 0-100 хватит. Сделать `uint32 canary_percent`?
4. **`ReportApplyResult.log_excerpt`** — 4KB достаточно для большинства failure post-mortem'ов, но для интерактивного debug'а оператор хочет больше. Сделать ли `ReportLogs(stream LogLine)` P0 или отложить?
