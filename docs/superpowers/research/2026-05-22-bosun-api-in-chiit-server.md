# Bosun gRPC API в chiit-server: полный список ручек

Date: 2026-05-22
Author: research-агент по заданию пользователя
Status: draft, требует обсуждения с пользователем

## TL;DR

Решение пользователя 2026-05-22: bosun-server отменён как отдельный
процесс; функциональность реализуется как **новый gRPC namespace** в
существующем chiit-server (Go). chiit-client живёт со своими старыми
ручками, bosun-client (Rust) ходит за параллельным API
`BosunAPI` через тот же грпц-сервер.

Полный список — **27 RPC в 11 категориях**:

| Категория | RPC count | Must-have для MVP |
|---|---|---|
| Bootstrap & identity | 3 | да (2 из 3) |
| Session control plane | 4 | да (3 из 4) |
| Secrets / PKI | 5 | да (2 из 5) |
| Inventory & targeting | 5 | да (1 из 5) |
| Bundle distribution | 3 | да (1 из 3) |
| Reporting / audit | 3 | да (2 из 3) |
| RBAC / persons | 1 | нет |
| Observability | 2 | нет |
| Discovery / peers | 1 | нет |

Топ-5 must-have для MVP: `Bootstrap`, `Subscribe` (server-stream),
`Heartbeat`, `ReportApplyResult`, `GetBundleManifest`.

Архитектура: bosun-client держит один long-lived stream через
`Subscribe`. Остальные ручки — unary lookups. Auth везде через mTLS
поверх ECDSA-cert, выданного на `Bootstrap`. Большинство ручек делегируется
в существующие handler'ы chiit-server c минимальной transformation —
прямое переиспользование dataflow и кэшей.

## Контекст и опорные точки в коде

### Как работает chiit-client сегодня (для аналогии)

Полный контракт между chiit-client и chiit-server описан в файле
`chiit/lib/utils/server/types.go:20-38` — интерфейс `Chiit`. Реализации
по RPC лежат рядом:

- `chiit/lib/utils/server/create.go:21` — `CreateClient(token)`
  (bootstrap, обмен shared-token на ECDSA-cert).
- `chiit/lib/utils/server/vault.go:33` — `GetVault(path, key)`.
- `chiit/lib/utils/server/cert_manager.go:34` — `GetCertByID(certId)`.
- `chiit/lib/utils/server/rsa.go:18` — `GetRSA(hash, env)`.
- `chiit/lib/utils/server/talos.go:23` — `GetTalosKeys(env)`.
- `chiit/lib/utils/server/bucket.go:23` — `BootstrapBucketByClusterName`.
- `chiit/lib/utils/server/severity.go:26` — `GetSeverity(database, env)`.
- `chiit/lib/utils/server/person.go:31` — `GetPersonRoles(database, env)`.
- `chiit/lib/utils/server/patroni_master.go:17` —
  `GetPatroniMaster(env, cluster)`.
- `chiit/lib/utils/server/white_list.go:14` —
  `GetWhiteListIDs(env)`.
- `chiit/lib/utils/server/service.go:20` —
  `GetServicePod(serviceName)` (warden discovery).
- `chiit/lib/utils/server/upload.go:16` —
  `UploadReport(report)`.
- `chiit/lib/utils/server/release.go:22` — `GetRelease/GetReleaseByCanary`
  (bundle distribution).

Все 14 ручек — server-handler'ы в chiit-server лежат под
`chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/*.go`,
а proto — в `chiit-server/api/chiit/api.proto`. У всех — ARC/in-memory
кэш + delegation в backend (PG, Vault, cert-manager, Hallpass).

### Что нужно bosun-client (отличия)

bosun-client (Rust) делает то же самое, что chiit-client, плюс несколько
новых:

- **Server-push команды** (apply_bundle, run_task) через долгий
  bi-directional stream — этого у chiit-client нет (там pull-режим).
- **Bundle delivery** — у chiit бандл встроен в бинарь, у bosun bundle —
  отдельный артефакт (tar.gz + cosign signature).
- **Heartbeat** — application-level liveness, не пакетный pull.
- **Multi-tenant audit log** — у chiit отдельные таблицы и handler-ы для
  reports; bosun хочет более structured stream событий.

То есть от chiit-server-flow можно переиспользовать почти всё, **но
накрыть это новым proto-namespace `BosunAPI`** — чтобы:

1. Отдельная эволюция, отдельный proto package, отдельный go-generated
   stub. Не ломаем chiit-client при изменении bosun-проtoa.
2. Отдельная авторизация: bosun mTLS поверх ECDSA, chiit — HTTP-подписи.
3. Отдельные метрики и rate-limiting — bosun-нагрузка на 60k стримов
   принципиально другая, чем 60k pull-запросов раз в 30s.

## Категория 1. Bootstrap & identity (must-have)

### 1.1 `Bootstrap(BootstrapIn) → BootstrapOut`

Use case: первый запуск bosun-client'а на ноде. Не имеет cert'а, имеет
только shared bootstrap-secret. Запросом обменивает secret + hostname
на персональный ECDSA-cert.

Сейчас в chiit-client это два шага:
1. `os.ReadFile(/etc/chiit/token)` — secret из cloud-init/baseline
   (`postgres-chiit/lib/vars/register.go:31`).
2. POST на `/api/v1/validator/create` с body `{host, clientPublicKey}` и
   header `validator-registration-token: sha256(host:token)`. Сервер хранит
   public key в `clients` таблице. Реализовано в
   `chiit/lib/utils/server/create.go:113`. У клиента нет cert'а, у
   него есть свой private key — server держит public.

У bosun меняем модель: cert генерирует **сервер**, отдаёт PEM в response,
клиент сразу пишет на диск и использует через mTLS на следующем
подключении. Plain shared-secret rotation теперь не нужен — у клиента
есть честный cert.

```protobuf
message BootstrapIn {
  string hostname = 1;
  string bootstrap_secret = 2;   // shared, ротируется ручкой админа
  string platform = 3;           // "linux/amd64", из uname
  string bosun_version = 4;      // "1.0.0"
  Environment environment = 5;   // staging/production
  // Опционально: client_public_key для случая когда клиент сам сгенерировал
  // (по модели chiit). Если пусто — server генерирует пару.
  string client_public_key_pem = 6;
}

message BootstrapOut {
  uint64 node_id = 1;
  string cert_pem = 2;        // ECDSA cert, выданный server'ом
  string ca_bundle_pem = 3;   // bundle для проверки server'ом своих cert'ов
  // Если client сам не давал public key:
  string private_key_pem = 4; // ECDSA private key — клиент сохраняет 0600
}
```

Cost/complexity: ~3 дня. Reuse 60%: можно переиспользовать
`internal/ecsda/sign.go` (валидация подписей) и `clients` PG-схему
(переименовать в `nodes`, добавить cert_pem column).

Open questions:
- Где валидируется bootstrap_secret? Hardcoded в config chiit-server'а
  или динамическая таблица `bootstrap_secrets {secret, env, valid_until}`?
- Что делать при повторном Bootstrap для уже зарегистрированной ноды?
  В chiit `create.go:32` есть warning и `os.Remove(token)`. Здесь —
  rebake cert (новая запись, старый cert invalid)?

### 1.2 `RotateCert(RotateCertIn) → BootstrapOut`

Use case: cert протух через 1 год, клиент сам себя продлевает (плановая
ротация). Аутентификация поверх старого mTLS-cert.

```protobuf
message RotateCertIn {
  uint64 node_id = 1;
  string current_cert_fingerprint = 2;  // подпись cert'а, который сейчас на диске
}
```

В chiit отсутствует. Не must-have для MVP — на MVP cert валиден 10 лет.

Cost/complexity: ~1 день, после Bootstrap.

### 1.3 `RevokeCert(RevokeCertIn) → google.protobuf.Empty`

Use case: оператор увидел утечку cert'а, форсит revoke. После revoke
клиент при следующем connect получает `UNAUTHENTICATED` и должен
сделать Bootstrap заново.

Не must-have для MVP, делается админ-tool'ом напрямую в PG.

## Категория 2. Session control plane (must-have)

### 2.1 `Subscribe(SubscribeIn) returns (stream Command)`

Use case: **главный канал**, через который bosun-server push'ит команды
клиенту. Открыт постоянно. Клиент держит один stream на под bosun-server,
переподключается с экспоненциальным backoff при обрыве.

```protobuf
message SubscribeIn {
  uint64 node_id = 1;
  string bosun_version = 2;
  HostInfo host_info = 3;    // hostname, cluster, severity-tag etc.
  repeated string tags = 4;  // user-defined tags ноды
}

message Command {
  uint64 command_id = 1;
  oneof kind {
    ApplyBundle apply_bundle = 2;
    RunTask run_task = 3;
    FlushFacts flush_facts = 4;
    StatusQuery status_query = 5;
    Cancel cancel = 6;
  }
  google.protobuf.Timestamp issued_at = 10;
}

message ApplyBundle {
  string bundle_name = 1;
  string bundle_version = 2;
  string cdn_url = 3;
  string sha256 = 4;
  string cosign_signature = 5;  // или путь к ней в OCI sigstore
  repeated string tags = 6;     // target tags (apply on tag:billing)
}

message RunTask {
  string task_name = 1;
  google.protobuf.Struct args = 2;
}

message FlushFacts {
  repeated string fact_names = 1;  // pусто = все
}

message StatusQuery {
  // server хочет немедленный snapshot состояния ноды
}

message Cancel {
  uint64 cancel_command_id = 1;
}
```

В chiit нет аналога — там pull раз в 30s через `GetRelease`. Это
архитектурно главное отличие BosunAPI.

Cost/complexity: 1–2 недели. Это сердце control plane. Полагается на
`commands_queue` PG-table (см. `bosun-server-client-architecture.md`).

Open questions:
- Backpressure: если клиент медленный, server queue растёт. На какую
  длину blocker'ить?
- Дедупликация: при reconnect клиент должен сказать "у меня есть
  command_id N", чтобы server не дослал дубль.

### 2.2 `Heartbeat(HeartbeatIn) returns (HeartbeatOut)`

Use case: application-level liveness вне stream'а. Если stream живой,
heartbeat не нужен; если стрим закрыт reconnect-логикой — heartbeat
показывает что клиент ещё жив (для UI «когда последний раз видели
ноду»).

```protobuf
message HeartbeatIn {
  uint64 node_id = 1;
  HostInfo host_info = 2;
  google.protobuf.Timestamp client_now = 3;
}

message HeartbeatOut {
  google.protobuf.Timestamp server_now = 1;
  // Hint клиенту что стрим лучше переоткрыть (например, под рестартует)
  bool reconnect_recommended = 2;
}
```

Cost/complexity: 1 день, поверх `node_status` UPSERT.

Open questions:
- Период? 10s / 30s / 60s? У chiit нет HB, у k8s default 10s. Скорее
  30s — достаточно агрессивно для UX без перегрузки PG.

### 2.3 `ReportApplyResult(ReportApplyResultIn) → google.protobuf.Empty`

Use case: клиент сообщает результат apply (success/failed/deferred).
В chiit это `UploadReport` (`chiit/lib/utils/server/upload.go:16` +
handler `chiit-server/internal/app/.../chiit/report.go`). Мы расширяем
с более structured данными.

```protobuf
message ReportApplyResultIn {
  uint64 node_id = 1;
  uint64 command_id = 2;        // которую command'у завершили
  string bundle_name = 3;
  string bundle_version = 4;
  ApplyOutcome outcome = 5;
  google.protobuf.Duration duration = 6;
  uint32 resources_total = 7;
  uint32 resources_changed = 8;
  uint32 resources_failed = 9;
  repeated ResourceError errors = 10;  // первые N failed primitives
  string severity = 11;
  // По-прежнему пишется в audit_log + node_status (UPSERT)
}

enum ApplyOutcome {
  APPLY_OUTCOME_UNSPECIFIED = 0;
  SUCCESS = 1;
  FAILED = 2;
  DEFERRED = 3;
  NO_CHANGE = 4;
}

message ResourceError {
  string resource_kind = 1;
  string resource_id = 2;
  string error_message = 3;
  string stage = 4;  // "plan" / "apply" / "verify"
}
```

Cost/complexity: 3 дня. Reuse:
- PG-table `reports` существует, нужна модификация под bundle/version
  модель bosun.
- `chiit-server/internal/app/.../chiit/report.go` — handler, можно
  взять как шаблон, добавить новые поля.

Open questions:
- Стоит ли логи apply'а отправлять inline в report (как chiit)
  или гнать отдельным `StreamLogs` RPC? Сейчас chiit держит до
  `Report.Report` поле bytes — но при 60k нод × ~10kB лог × 30s
  rollout это уже Gigabytes/min. Лучше — failed-only inline, success
  без логов.

### 2.4 `ReportLogs(ReportLogsIn) → ReportLogsOut` (server-streaming не нужен)

Use case: только при `outcome=FAILED` или `outcome=DEFERRED` клиент
дозагружает полный лог как chunked bytes (для последующего анализа).

```protobuf
message ReportLogsIn {
  uint64 node_id = 1;
  uint64 command_id = 2;
  string compression = 3;  // "zstd" | "gzip" | "none"
  bytes chunk = 4;
  uint32 chunk_index = 5;
  bool last_chunk = 6;
}

message ReportLogsOut {
  string upload_id = 1;
}
```

Cost/complexity: 5 дней (chunk-aware storage + лимит размера). Не
must-have для MVP — можно ограничиться короткими error_message в
ReportApplyResult.

## Категория 3. Secrets / PKI (must-have частично)

### 3.1 `VaultGet(VaultGetIn) → VaultOut` (must-have)

Use case: bosun-client при apply'е bundle'а нуждается в секрете из
Vault (пароли БД, API-ключи). Сам в Vault не ходит — у него нет
токена, у него есть mTLS-cert.

Полное соответствие с chiit:
- Handler: `chiit-server/internal/app/.../chiit/vault_get.go`.
- Подпись в chiit поверх ECDSA, у нас — mTLS (cert subject = node_id,
  валидируется на server-stream interceptor).
- ARC-cache в `chiit-server/internal/vault/cache.go`.

```protobuf
message VaultGetIn {
  string path = 1;   // "infra/postgresql/billing/production"
  string key = 2;    // "postgres_password"
}

message VaultOut {
  string data = 1;
  google.protobuf.Timestamp cached_at = 2;  // для diagnostics
}
```

Cost/complexity: 1 день — это **тонкая обёртка** над существующим
`VaultGet` handler'ом.

Open questions:
- Whitelist путей: bosun-server открывает Vault для произвольных путей?
  Или есть policy "path под `infra/postgresql/<owner_of_node>/..."?
  Если последнее — нужно резолвить owner_of_node по node_id.

### 3.2 `GetCert(GetCertIn) → CertificateData` (must-have)

Use case: bosun-client просит у server TLS-cert для подключения к
конкретному ресурсу (Patroni mTLS, Vault, прочее).

Соответствие 1:1 с chiit:
- Handler: `chiit-server/internal/app/.../chiit/cert_manager.go`.
- PG-table `certificates` хранит выданные cert'ы.
- Backend: Cert-Manager-Gateway (proprietary Ozon proto).

```protobuf
message GetCertIn {
  uint64 cert_id = 1;
  string purpose = 2;  // "patroni-mtls" | "vault-tls" etc., для аудита
}

// CertificateData переиспользуется один-в-один из chiit/api.proto:423
message CertificateData {
  string cert = 1;
  string ca_bundle = 2;
  string private_key = 3;
}
```

Cost/complexity: 1 день. Reuse прямой.

### 3.3 `GetRSAPairs(GetRSAPairsIn) → GetRSAPairsOut` (nice-to-have)

Use case: bosun bundle хочет стабильную RSA-пару для подписи
конфигов (например, Patroni). Server генерирует на лету, persistит в
`rsa_keys` PG-table, кэширует на час.

Соответствие 1:1 с chiit `GetRSAPairs` (proto:327, handler:
`get_rsa_pairs.go`).

```protobuf
message GetRSAPairsIn {
  int64 hash = 1;          // sha256-based hash для namespacing
  Environment environment = 2;
}

message GetRSAPairsOut {
  string public = 1;   // PEM
  string private = 2;  // PEM
}
```

Cost/complexity: 1 день. Не на MVP — стартуем без неё, добавляем когда
понадобится подписывать Patroni HBA.

### 3.4 `GetTalosKeys(GetTalosKeysIn) → GetTalosKeysOut` (nice-to-have)

Use case: bosun-client должен аутентифицироваться в Talos
(Ozon internal identity provider). Server отдаёт JWKS-keys как PEM.

Соответствие 1:1 с chiit (proto:317, handler: `get_talos_keys.go`).

```protobuf
message GetTalosKeysIn {
  Environment environment = 1;
}

message GetTalosKeysOut {
  repeated TalosKey keys = 1;
}

message TalosKey {
  string key = 1;
  string certificate = 2;
}
```

Cost/complexity: 0.5 дня.

### 3.5 `BootstrapBucket(BootstrapBucketIn) → BootstrapBucketOut` (nice-to-have)

Use case: при bootstrap нового Patroni-кластера через bosun bundle —
нужен S3-bucket для basebackup'ов. chiit-server проксирует
в pg-backup-manager.

Соответствие 1:1 с chiit (proto:368, handler: `bootstrap_bucket.go`).

```protobuf
message BootstrapBucketIn {
  string patroni_cluster_name = 1;
  int64 expire_days = 2;
}

message BootstrapBucketOut {
  string bucket_name = 1;
  string access_key_owner = 2;
  string secret_key_owner = 3;
  string access_key_read_only = 4;
  string secret_key_read_only = 5;
}
```

Cost/complexity: 1 день. Должны переиспользовать существующий
pg-backup-manager grpc-client из chiit-server (`internal/clients/`).

## Категория 4. Inventory & targeting (must-have частично)

### 4.1 `StorageHostInventoryGet(...) → StorageHostInventoryGetOut` (must-have)

Use case: bundle Starlark обращается к `inv.cluster_name`,
`inv.patroni_cluster`, `inv.postgres_version`. Эти поля приходят из
storage-inventory (отдельный сервис Ozon). chiit-server проксирует.

Соответствие 1:1 с chiit `StorageHostInventoryGet` (proto:357, handler:
`storage_host_inventory_get.go`).

```protobuf
message StorageHostInventoryGetIn {
  string host = 1;
}

message StorageHostInventoryGetOut {
  bool cache = 1;
  ozon.infrastructure.postgresql.inventory.GetHostInventoryOut response = 2;
}
```

В postgres-chiit это loaded единожды на старте через
`postgres-chiit/lib/vars/manager.go:177` → `mergeStorageInventory`.
У bosun будет аналогично — один lookup на старте apply, кэшируется.

Cost/complexity: 0.5 дня (тонкий проброс).

Open questions:
- Какой proto package у `inventory.InventoryItem`? В chiit это
  `gitlab.ozon.ru/infrastructure/postgresql/storage-inventory`. Для
  bosun-client (Rust) придётся либо генерить Rust-stub для inventory
  proto, либо завернуть response в `google.protobuf.Struct`. Скорее
  второе — bosun-client не должен знать схему inventory'а.

### 4.2 `StorageInventoryGroupGet(...) → ...` (nice-to-have)

Use case: bosun-server (при таргетинге `apply on cluster:foo`) хочет
получить список всех хостов в группе.

Соответствие 1:1 с chiit (proto:170).

Cost/complexity: 0.5 дня.

### 4.3 `GetSeverity(GetSeverityIn) → GetSeverityOut` (nice-to-have)

Use case: bosun-server резолвит таргетинг `apply on severity:low`
в список node_id. Запрос «какой severity у базы X в env Y».

Соответствие 1:1 с chiit (proto:243, handler: `get_severity.go`,
backbone: `runITCSync`).

```protobuf
message GetSeverityIn {
  string database = 1;
  Environment environment = 2;
}

message GetSeverityOut {
  Severity severity = 1;  // UNKNOWN/LOW/MEDIUM/HIGH
}
```

Cost/complexity: 0.5 дня.

Замечание: для bosun **этот RPC обычно нужен на server-стороне**, не на
client-стороне. То есть bosun-server, обрабатывая запрос оператора
«apply на severity:low», сам зовёт `GetSeverity` для своего внутреннего
резолва. bosun-client его не зовёт. Поэтому либо: (a) выделить его в
`internal-loopback` API для server-side ходоков, (b) оставить в общем
API — RPC хочет authn anyway.

### 4.4 `GetDatabaseList(GetDatabaseListIn) → GetDatabaseListOut` (nice-to-have)

Use case: то же что 4.3 — для server-side targeting'а. Также может
нужно bundle'у Starlark'a для генерации мульти-cluster конфигов.

Соответствие 1:1 с chiit (proto:273).

```protobuf
message GetDatabaseListIn {
  Environment environment = 1;
  Severity severity = 2;
  uint32 hash_mod = 3;  // для canary subsetting'а
}

message GetDatabaseListOut {
  repeated string list = 1;
}
```

Cost/complexity: 0.5 дня.

### 4.5 `GetMasterOfPatroniCluster(...) → ...` (nice-to-have)

Use case: bundle task'а `pg_repack` хочет знать кто master. bundle
зовёт через bosun-client → ходит в bosun-server → ходит в warden.

Соответствие 1:1 с chiit (proto:337, handler:
`get_master_of_patroni_cluster.go`). Cache 3s.

```protobuf
message GetMasterOfPatroniClusterIn {
  Environment environment = 1;
  string patroni_cluster = 2;
}

message GetMasterOfPatroniClusterOut {
  string host = 1;  // имя master'а; пусто если ни один не лидер
}
```

Cost/complexity: 0.5 дня.

## Категория 5. Bundle distribution (must-have частично)

### 5.1 `GetBundleManifest(GetBundleManifestIn) → GetBundleManifestOut` (must-have)

Use case: bosun-client при получении `ApplyBundle` команды (через
Subscribe stream) валидирует metadata — что sha256 совпадает с
зарегистрированным, signature свежая, bundle не retracted.

Это **новая логика**, в chiit нет. У chiit бандл встроен в бинарь и
distributed через `GetRelease` + p2p HTTP-сервер
(`postgres-chiit/cmd/upgrade.go:88`).

```protobuf
message GetBundleManifestIn {
  string bundle_name = 1;
  string bundle_version = 2;  // если пусто — latest published
}

message GetBundleManifestOut {
  string bundle_name = 1;
  string bundle_version = 2;
  string sha256 = 3;
  string cosign_signature = 4;
  string cdn_url = 5;
  string fallback_cdn_url = 6;  // на случай мёртвого CDN
  google.protobuf.Timestamp uploaded_at = 7;
  string uploaded_by = 8;       // adLogin оператора
  bool retracted = 9;
  string retract_reason = 10;
}
```

В PG нужна новая таблица `bundle_registry` (см.
`bosun-server-client-architecture.md` секция PG-таблицы).

Cost/complexity: 1 неделя (нужна новая PG-схема + bundle upload
pipeline в chiit-server-cli).

Open questions:
- CDN хранение: где tar.gz лежит? S3? Local artifactory? OCI registry?
  Зависит от инфры Ozon.
- Сосуществование с chiit `releases` таблицей? Не пересекаются, но
  оператор должен видеть оба в UI.

### 5.2 `ListBundles(ListBundlesIn) → ListBundlesOut` (для оператора)

Use case: UI/CLI для оператора — посмотреть какие bundles есть и их
версии.

```protobuf
message ListBundlesIn {
  string name_prefix = 1;
  uint32 limit = 2;
}

message ListBundlesOut {
  repeated BundleSummary bundles = 1;
}

message BundleSummary {
  string name = 1;
  string latest_version = 2;
  uint32 total_versions = 3;
  google.protobuf.Timestamp last_uploaded_at = 4;
}
```

Не must-have. Используется только UI оператора. Cost: 2 дня.

### 5.3 `RetractBundle(RetractBundleIn) → google.protobuf.Empty` (для оператора)

Use case: оператор обнаружил баг в bundle, помечает версию retracted.
bosun-client при попытке apply ретрактнутой версии должен падать.

Не must-have для MVP. Cost: 1 день.

## Категория 6. Reporting / audit (must-have частично)

Часть already covered в категории 2 (ReportApplyResult, ReportLogs).
Добавляем server-side API для UI.

### 6.1 `GetNodeStatus(GetNodeStatusIn) → GetNodeStatusOut` (для UI)

Use case: UI хочет показать «текущее состояние ноды X» — последний
apply, версия, last_seen.

```protobuf
message GetNodeStatusIn {
  oneof selector {
    uint64 node_id = 1;
    string hostname = 2;
  }
}

message GetNodeStatusOut {
  uint64 node_id = 1;
  string hostname = 2;
  string last_applied_bundle = 3;
  string last_applied_version = 4;
  ApplyOutcome last_outcome = 5;
  google.protobuf.Timestamp last_seen_at = 6;
  google.protobuf.Timestamp last_apply_at = 7;
}
```

Cost: 2 дня. Не must-have для bosun-client (он сам это знает локально).

### 6.2 `GetAuditLog(GetAuditLogIn) → GetAuditLogOut` (для UI)

Use case: UI «покажи failed apply'ы по cluster billing за последние
24h».

```protobuf
message GetAuditLogIn {
  string node_hostname_like = 1;
  string bundle_name_like = 2;
  google.protobuf.Timestamp from = 3;
  google.protobuf.Timestamp to = 4;
  uint32 limit = 5;
}

message GetAuditLogOut {
  repeated AuditEntry entries = 1;
}
```

Cost: 3 дня. Не на MVP, добавляется когда UI начнём писать.

### 6.3 `IssueCommand(IssueCommandIn) → IssueCommandOut` (для оператора, важно)

Use case: оператор через UI/CLI запускает «apply bundle X на target
Y». Server должен:
1. Резолвить target (cluster:foo, severity:low, tag:bar) → список
   node_id.
2. INSERT в `commands_queue` по каждому node_id.
3. Вернуть command_id оператору для tracking.

```protobuf
message IssueCommandIn {
  oneof command {
    ApplyBundle apply_bundle = 1;
    RunTask run_task = 2;
    FlushFacts flush_facts = 3;
  }
  TargetSelector target = 10;
  string issued_by = 11;  // adLogin оператора, из mTLS-cert
  bool dry_run = 12;
}

message TargetSelector {
  oneof selector {
    string cluster = 1;       // "billing"
    string severity = 2;      // "low"
    string tag = 3;           // "canary"
    string hostname_glob = 4; // "*pg-25*"
    NodeList nodes = 5;
  }
}

message IssueCommandOut {
  uint64 batch_id = 1;
  uint32 affected_nodes = 2;
  repeated uint64 command_ids = 3;
}
```

Это **новый API** для оператора. В chiit аналога нет (там оператор пишет
release через CLI). Cost: 1 неделя (включая targeting resolver).

## Категория 7. RBAC / persons (nice-to-have)

### 7.1 `GetPersonRoles(GetPersonRolesIn) → GetPersonRolesOut`

Use case: bundle хочет в `inv.persons` получить список людей с ролями
для notifications или RBAC-проверки. server-side при `IssueCommand`
проверяет что operator имеет роль ≥ OPERATOR в target db.

Соответствие 1:1 с chiit (proto:253, handler: `get_person_roles.go`).

```protobuf
message GetPersonRolesIn {
  string database = 1;
  Environment environment = 2;
}

message GetPersonRolesOut {
  repeated Person person = 1;
}

message Person {
  string ad_login = 1;
  ITCPersonRole role = 2;
}
```

Cost: 0.5 дня. Не must-have.

## Категория 8. Observability (nice-to-have)

### 8.1 `ReportMetrics(ReportMetricsIn) → google.protobuf.Empty`

Use case: bosun-client отчитывается агрегированные метрики на server
(вместо Prometheus pull). Текущий chiit/bosun использует Prometheus
textfile (`postgres-chiit/cmd/run.go:267`); это можно оставить и не
делать этот RPC.

```protobuf
message ReportMetricsIn {
  uint64 node_id = 1;
  google.protobuf.Timestamp ts = 2;
  repeated MetricSample samples = 3;
}

message MetricSample {
  string name = 1;
  map<string, string> labels = 2;
  double value = 3;
}
```

Cost: 5 дней (push + Mimir/Prometheus integration). Не нужно если
оставим Prometheus textfile.

### 8.2 `ReportEvent(ReportEventIn) → google.protobuf.Empty`

Use case: nodal события (cert rotated, bundle pulled, defer scheduled).
Дополнение к ReportApplyResult, для observability дашбордов.

Cost: 2 дня. Не на MVP.

## Категория 9. Discovery / peers (информационно)

### 9.1 Discovery подов bosun-server'a — не gRPC

bosun-client должен **сам найти** живой под bosun-server'a при старте
и при обрыве стрима. У chiit это делается через `warden` discovery
(`postgres-chiit/lib/warden/warden.go:46`). У bosun будет также:

- На старте: DNS lookup на `bosun-control-plane.infrastructure:443`
  (k8s Service). L4 балансер распределяет TCP.
- При hello_ack `redirect_to: pod_id` — клиент идёт на specific pod
  через `pod_id.bosun.svc.cluster.local`.

Поэтому здесь **нет RPC** — это infrastructure-level. Но в chiit-server
remain нужны pods discovery handlers (для самого chiit-client'a).

`GetWardenHostsForService` (chiit proto:233, handler:
`get_warden_hosts_for_service.go`) — не для bosun, оставляем chiit'у.

## Категория 10. Команда вспомогательные (опционально)

### 10.1 `GetWhiteListForResource(...) → ...`

Use case: bundle Starlark проверяет «id оператора в white-list для
этого ресурса?». Соответствие 1:1 с chiit (proto:347, handler:
`get_white_list_for_resource.go`).

Cost: 0.5 дня. Не must-have.

## Полный proto sketch (consolidated)

```protobuf
syntax = "proto3";
package ozon.infrastructure.cloudozon.chiit.bosun.v1;

option go_package = "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/bosun;bosun_v1";

import "google/protobuf/timestamp.proto";
import "google/protobuf/duration.proto";
import "google/protobuf/empty.proto";
import "google/protobuf/struct.proto";
import "gitlab.ozon.ru/infrastructure/postgresql/storage-inventory/api/inventory/inventory.api.proto";

service BosunAPI {
  // --- 1. Bootstrap & identity (must-have для MVP) ---
  rpc Bootstrap(BootstrapIn) returns (BootstrapOut);
  rpc RotateCert(RotateCertIn) returns (BootstrapOut);
  rpc RevokeCert(RevokeCertIn) returns (google.protobuf.Empty);

  // --- 2. Session control plane (must-have) ---
  rpc Subscribe(SubscribeIn) returns (stream Command);
  rpc Heartbeat(HeartbeatIn) returns (HeartbeatOut);
  rpc ReportApplyResult(ReportApplyResultIn) returns (google.protobuf.Empty);
  rpc ReportLogs(ReportLogsIn) returns (ReportLogsOut);

  // --- 3. Secrets / PKI ---
  rpc VaultGet(VaultGetIn) returns (VaultOut);                            // must-have
  rpc GetCert(GetCertIn) returns (CertificateData);                        // must-have
  rpc GetRSAPairs(GetRSAPairsIn) returns (GetRSAPairsOut);                 // nice
  rpc GetTalosKeys(GetTalosKeysIn) returns (GetTalosKeysOut);              // nice
  rpc BootstrapBucket(BootstrapBucketIn) returns (BootstrapBucketOut);     // nice

  // --- 4. Inventory & targeting ---
  rpc StorageHostInventoryGet(StorageHostInventoryGetIn)
      returns (StorageHostInventoryGetOut);                                // must-have
  rpc StorageInventoryGroupGet(StorageInventoryGroupGetIn)
      returns (StorageInventoryGroupGetOut);                               // nice
  rpc GetSeverity(GetSeverityIn) returns (GetSeverityOut);                 // nice
  rpc GetDatabaseList(GetDatabaseListIn) returns (GetDatabaseListOut);     // nice
  rpc GetMasterOfPatroniCluster(GetMasterOfPatroniClusterIn)
      returns (GetMasterOfPatroniClusterOut);                              // nice

  // --- 5. Bundle distribution ---
  rpc GetBundleManifest(GetBundleManifestIn) returns (GetBundleManifestOut); // must-have
  rpc ListBundles(ListBundlesIn) returns (ListBundlesOut);                   // оператор
  rpc RetractBundle(RetractBundleIn) returns (google.protobuf.Empty);        // оператор

  // --- 6. Reporting / audit (UI и оператор) ---
  rpc GetNodeStatus(GetNodeStatusIn) returns (GetNodeStatusOut);
  rpc GetAuditLog(GetAuditLogIn) returns (GetAuditLogOut);
  rpc IssueCommand(IssueCommandIn) returns (IssueCommandOut);               // ключевой для оператора

  // --- 7. RBAC ---
  rpc GetPersonRoles(GetPersonRolesIn) returns (GetPersonRolesOut);         // nice

  // --- 8. Observability (опц) ---
  rpc ReportMetrics(ReportMetricsIn) returns (google.protobuf.Empty);
  rpc ReportEvent(ReportEventIn) returns (google.protobuf.Empty);

  // --- 10. Вспомогательные ---
  rpc GetWhiteListForResource(GetWhiteListForResourceIn)
      returns (GetWhiteListForResourceOut);
}

// ----- Сообщения подробно см. секции 1-10 выше -----
// Большинство messages можно переиспользовать из существующего
// chiit-server/api/chiit/api.proto (lines 380-710) с переименованием
// package'а.

enum Environment {
  ENVIRONMENT_UNSPECIFIED = 0;
  STAGING = 1;
  PRODUCTION = 2;
}

enum Severity {
  SEVERITY_UNSPECIFIED = 0;
  LOW = 1;
  MEDIUM = 2;
  HIGH = 3;
  SEVERITY_UNKNOWN = 4;
}

enum ITCPersonRole {
  ITC_PERSON_ROLE_UNSPECIFIED = 0;
  LEADER = 1;
  DEVELOPER = 2;
  OWNER = 3;
  ADMINISTRATOR = 6;
  OPERATOR = 7;
  // соответствие chiit/api.proto:402
}

message HostInfo {
  string hostname = 1;
  string cluster = 2;
  string patroni_cluster = 3;
  Severity severity = 4;
  string ozon_env = 5;
  string dc = 6;          // datacenter
  string platform = 7;    // "linux/amd64"
}
```

## Приоритизация

### Must-have для MVP (P0)

1. **Bootstrap** — без него нода не может зарегистрироваться.
2. **Subscribe** — основной канал команд.
3. **Heartbeat** — нужен для UX, иначе UI ничего не показывает.
4. **ReportApplyResult** — без него сервер не знает результаты.
5. **GetBundleManifest** — без него bosun-client не валидирует bundle.
6. **VaultGet** — без него bundle'ы с секретами не работают.
7. **GetCert** — без него bundle'ы с TLS не работают.
8. **StorageHostInventoryGet** — без него bundle Starlark не имеет
   inventory.
9. **IssueCommand** — без него оператор не может ничего запустить.

Итого 9 RPC, ~4 недели работы при reuse существующих handler'ов
chiit-server.

### Nice-to-have (P1, после MVP)

- ReportLogs (failed-only chunked log upload).
- GetSeverity / GetDatabaseList / GetPersonRoles (для server-side
  targeting и notifications).
- GetRSAPairs / GetTalosKeys / BootstrapBucket (для специфичных
  bundle-сценариев).
- StorageInventoryGroupGet.
- GetMasterOfPatroniCluster.
- GetNodeStatus / GetAuditLog (для UI).
- RotateCert.

Ещё ~2-3 недели.

### Out-of-scope (хоронить или оставить chiit-client'у)

- `Build`, `GetBuilds`, `Release`, `GetRelease`, `GetReports`,
  `GetLastReport`, `CreateClient`, `DeleteClient`, `ListClients`,
  `UpdateClientKey` — chiit-client legacy flow, остаётся как есть.
- `GetWardenHosts` (proto:223, hardcoded service) — deprecated.
- `SetSilence` / `DeleteSilence` — оператор-level, не bosun-client'у.
- `SyncPersonAndSeverity` (proto:263) — manual trigger, в bosun
  не нужен.
- `DropCache` (proto:207) — в bosun caches per-process, drop через
  рестарт пода.
- `GetWardenHostsForService` — оставляем chiit-client'у;
  bosun-client'у не нужен (он подключается напрямую к
  `bosun-control-plane` через DNS).

### Особые случаи

- `GetWhiteListForResource` — переноситcя как nice-to-have, но
  большую часть RBAC-логики делает сам chiit-server при
  `IssueCommand`. bosun-client его прямо не зовёт.
- `ReportMetrics` — оставить Prometheus textfile, не делать новый
  push-канал.

## Архитектурные решения

### Один gRPC server или два?

chiit-server уже слушает gRPC на свой порт + REST-gateway. Добавим
**второй gRPC service** `BosunAPI` поверх того же `grpc.Server`:

```go
// chiit-server/cmd/main.go (или где-то рядом)
grpcServer := grpc.NewServer(opts...)
chiit_v1.RegisterChiitServerServer(grpcServer, chiitImpl)
bosun_v1.RegisterBosunAPIServer(grpcServer, bosunImpl)
```

Pros:
- Минимум инфраструктурных изменений.
- Один и тот же authn middleware (mTLS — для bosun, ECDSA-sign — для
  chiit, разные interceptor'ы по path).
- Один Mimir target, один Loki source.

Cons:
- Если bosun-нагрузка задавит CPU — chiit тоже страдает. Mitigation:
  rate-limit interceptor по client-id.
- gRPC interceptor chain должен поддерживать оба способа auth.
  Решается через path-routing внутри interceptor'а.

### Auth/authz

- `BosunAPI` использует **mTLS** только. Cert выдан на `Bootstrap`.
  Subject CN = `node-{node_id}` или hostname.
- Interceptor валидирует cert против `nodes.cert_pem` в PG, кэширует
  на 30s (ARC). На `RevokeCert` — invalidate cache.
- `IssueCommand` имеет дополнительную проверку: cert операторский (CN
  = `operator-{adLogin}`), не node'овый. Issued через отдельный
  Bootstrap-flow для CLI.

### PG-схема

Добавляются таблицы (в migrations):

- `nodes (node_id PK, hostname, cluster, cert_pem, registered_at,
  bootstrap_secret_id FK)` — заменяет/расширяет существующий
  `clients`.
- `commands_queue (id PK, target_node_id, command_kind, payload_json,
  status, ...)` — новая.
- `node_status (node_id PK, last_applied_bundle, last_applied_version,
  last_outcome, last_seen_at)` — новая.
- `audit_log (id PK, ts, node_id, command_id, kind, outcome, ...)` —
  новая.
- `bundle_registry (name, version PK, sha256, signature, cdn_url,
  uploaded_at, ...)` — новая.

Поверх migration framework chiit-server'a (он уже использует liquibase
или goose — TBD по факту проверки `migrations/`).

### Per-pod vs cluster-wide

`Subscribe` стримы привязаны к конкретному поду (через
`node_affinity { node_id, current_server_pod_id }`). `IssueCommand`
ходит в `commands_queue`, любой под poll'ит и dispatch'ит. Reaper
переразбирает на себя зависшие команды от мёртвых подов.

Это совпадает с `bosun-server-client-architecture.md` дизайном.

### Сосуществование с chiit-client

chiit-client живёт со своими `/api/v1/*` REST-ручками (proto:14-378) и
HTTP-bootstrap'ом. Никаких ломок. У него ничего не отбираем.

bosun-client использует **только** `BosunAPI` через gRPC + mTLS. Не
зовёт chiit-API напрямую (даже если оба сервиса в одном процессе) —
это enforce'им через отдельные interceptor'ы.

## Cost / complexity summary

| Группа | RPC count | Effort |
|---|---|---|
| Bootstrap & identity (P0) | 1 (+ 2 P1) | 3 дня |
| Session control plane (P0) | 3 (+ 1 P1) | 2 недели |
| Secrets / PKI (P0+P1) | 2 must, 3 nice | 1 неделя |
| Inventory & targeting | 1 must, 4 nice | 4 дня |
| Bundle distribution (P0) | 1 must, 2 oper | 1.5 недели |
| Reporting / audit | 0 must для client, 3 для UI | 1 неделя |
| RBAC | 1 nice | 0.5 дня |
| Observability | 0 | 0 |
| PG migrations | n/a | 1 неделя |
| Interceptors / Bootstrap-secret rotation | n/a | 3 дня |
| **Итого MVP (P0)** | **9 RPC** | **~5–6 недель** |
| **С P1** | **+9 RPC** | **+2.5–3 недели** |

Существенно дешевле чем bosun-server-как-отдельный-Rust-процесс
(там было ~14–22 недели по research'у 2026-05-20).

## Открытые вопросы для пользователя

1. **Где хранить bundle tar.gz?** S3 / OCI registry / локальный
   artifactory chiit-server'a? Без этого `GetBundleManifest` half-baked.
2. **Bootstrap-secret rotation:** один shared secret на парк (legacy
   chiit) или per-env (`prod`, `staging`) или динамическая таблица
   `bootstrap_secrets`? Чем сильнее динамика — тем больше операционных
   расходов.
3. **Auth для оператора (IssueCommand):** отдельный operator-Bootstrap
   с keycloak SSO? Или operator cert выдаётся вручную админом? У
   chiit это `admin-token` в header (proto:430).
4. **Inventory schema в Rust:** генерим Rust-stub для
   `storage-inventory` proto или заворачиваем response в
   `google.protobuf.Struct`? Первый вариант чище, второй проще.
5. **Heartbeat период:** 10s / 30s / 60s? 30s даёт UX «pod was up 30s
   ago», 10s — «5s ago». 10s × 60k = 6k QPS на PG UPSERT — выдержит,
   но не бесплатно.

## Что дальше

После одобрения этого списка пользователем:

1. Создать proto-файл `chiit-server/api/bosun/api.proto` с full
   message definitions (сейчас в research только sketch'и).
2. Дизайн PG-миграций (`nodes`, `commands_queue`, `bundle_registry`,
   `node_status`, `audit_log`).
3. Дизайн Rust gRPC-клиента в bosun-client (`bosun-control-plane-client`
   крейт): tonic-based, mTLS, reconnect с backoff, stream demuxer.
4. Implementation plan по этапам: первый — Bootstrap+Subscribe+
   ReportApplyResult; второй — VaultGet+GetCert+StorageInventoryGet;
   третий — GetBundleManifest+IssueCommand.

## Связанные документы

- [2026-05-20 bosun-server vs Rust](2026-05-20-bosun-server-language-choice.md)
  — почему изначально планировали Rust (теперь отменено).
- [2026-05-20 chiit-server extraction](2026-05-20-bosun-server-chiit-extraction.md)
  — анализ 31 RPC chiit-server'a, что переносить в bosun-server.
  Этот документ его дополняет: тот же список RPC, но теперь не
  «вынести в новый сервис», а «накрыть новым proto-namespace».
- [bosun-server-client-architecture](../../../.claude-memory-compiler/knowledge/concepts/bosun-server-client-architecture.md)
  — vision двухкомпонентной архитектуры (PG-таблицы, audit-модель,
  bootstrap-flow). Принципиально не меняется: server теперь не
  отдельный pod, а namespace в chiit-server'е.
