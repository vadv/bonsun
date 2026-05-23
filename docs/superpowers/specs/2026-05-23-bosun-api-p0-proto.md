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
}

message Command {
  string command_id = 1;                     // UUID для idempotency и audit-tracking
  google.protobuf.Timestamp issued_at = 2;
  string issued_by = 3;                      // оператор: Keycloak sub или admin-token name

  oneof payload {
    ApplyBundleCommand apply_bundle = 10;
    RunTaskCommand     run_task     = 11;
    FlushFactsCommand  flush_facts  = 12;
  }
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

`IssueCommand` пишет в `commands_queue` таблицу PG; голова `Subscribe` подписана на `LISTEN bosun_command_<host>` (`NOTIFY` в момент INSERT). На notification head делает SELECT pending commands и шлёт по stream через `Send(...)`.

### Bundle blobs в PG

```sql
CREATE TABLE bosun_bundles (
    version       BIGSERIAL PRIMARY KEY,
    sha256        BYTEA NOT NULL UNIQUE,
    blob          BYTEA NOT NULL,             -- tar.gz, до ~50MB на пакет
    signature     BYTEA NOT NULL,             -- ed25519
    tags          TEXT[] NOT NULL,
    published_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_by  TEXT NOT NULL,              -- operator login
    retracted_at  TIMESTAMPTZ
);

CREATE INDEX ON bosun_bundles(retracted_at) WHERE retracted_at IS NULL;
```

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

### Active sessions

```sql
CREATE TABLE bosun_active_sessions (
    host                  TEXT PRIMARY KEY,
    bosun_version         TEXT,
    pod_id                TEXT NOT NULL,      -- какой replica chiit-server держит stream (для multi-pod)
    connected_at          TIMESTAMPTZ NOT NULL,
    last_heartbeat        TIMESTAMPTZ NOT NULL,
    current_bundle_version BIGINT,
    last_apply_at         TIMESTAMPTZ,
    last_apply_result     TEXT
);

CREATE INDEX ON bosun_active_sessions(last_heartbeat);   -- для cleanup dead sessions
```

При connect клиента pod_X пишет себя в `pod_id`. Если оператор IssueCommand'ит на host который attached к pod_Y, pod_X получает NOTIFY (PG LISTEN/NOTIFY работает cross-replica).

### Open follow-ups (для P1)

- `GetBundleBlob(version) returns (stream BundleChunk)` — отдельный streaming RPC для самого blob, 4KB chunks.
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
