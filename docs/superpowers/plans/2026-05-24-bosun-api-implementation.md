# BosunAPI Implementation Plan (chiit-server)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Реализовать BosunAPI как новый gRPC namespace в существующем chiit-server (Go), согласно спецификации `docs/superpowers/specs/2026-05-23-bosun-final-schema.md` (v2).

**Architecture:** Третий сервис в chiit-server, рядом с `ChiitServer` и `PgShardManagerV1`. Pull-модель (клиент инициирует), background rollout-worker с leader election через `bosun_leader` heartbeat + fencing epoch, immutable bundle в PG bytea, LRU 5 на pod, ECDSA-подпись запросов (как в chiit), admin-token / Keycloak JWT для operator/CI ручек.

**Tech Stack:**
- Go (chiit-server) + scratch framework + protoc-gen-scratch для codegen.
- PostgreSQL 17 через `gitlab.ozon.ru/platform/go/database-pg` (pgx/v4 под капотом).
- Тесты: `testify/assert`+`require`, `minimock/v3` для интерфейсов, `fixtures.Prepare(t)` для integration с реальной PG.
- buf + protoc-gen-* для генерации.

**Spec reference:** `docs/superpowers/specs/2026-05-23-bosun-final-schema.md` — единственный источник истины. План его реализует буквально.

## Soft constraints

- Каждая таска включает тесты, где это применимо.
- Pure-функции (rollout math) — обязательное TDD-покрытие.
- Handlers через `convertError`/`status.Error` как в chiit-стиле.
- Auth — per-handler (как в chiit), не interceptor.
- Никаких CHECK constraints в DDL (по решению пользователя — validation в server-коде).
- Bundle blob hard limit 5 MiB enforced на server и в server-validation, без DB constraint.
- Используем существующие проектные паттерны: `Implementation` struct + `NewBosunAPI(ctx, app *scratch.App)` constructor + `GetDescription()` метод.
- Pretty-imports под алиасом `desc` для proto-генерата.

## Что НЕ в этом плане

- Frontend / Web UI (ничего).
- Bundle distribution через S3/OCI (только PG bytea).
- mTLS (только TLS + ECDSA-sig).
- Self-upgrade клиента.
- CI/CD pipeline для PublishBundle (предполагается что CI-команда сама делает gRPC call с admin-token).

## File Structure

```
chiit-server/
├── api/
│   └── bosun/
│       └── api.proto                          ← NEW (Task 1)
├── migrations/
│   └── 24_bosun_init.sql                      ← NEW (Task 2)
├── internal/
│   ├── repository/
│   │   ├── types.go                            ← MODIFY (расширить Manager)
│   │   ├── bosun_clients.go                    ← NEW (Task 3)
│   │   ├── bosun_clients_test.go               ← NEW (Task 3)
│   │   ├── bosun_bundles.go                    ← NEW (Task 4)
│   │   ├── bosun_bundles_test.go               ← NEW (Task 4)
│   │   ├── bosun_rollouts.go                   ← NEW (Task 5)
│   │   ├── bosun_rollouts_test.go              ← NEW (Task 5)
│   │   ├── bosun_pods.go                       ← NEW (Task 6)
│   │   ├── bosun_leader.go                     ← NEW (Task 6)
│   │   └── bosun_audit.go                      ← NEW (Task 6)
│   └── app/ozon/infrastructure/cloudozon/bosun/
│       ├── service.go                          ← NEW (Task 9)
│       ├── utils.go                            ← NEW (Task 9)
│       ├── audit.go                            ← NEW (Task 9)
│       ├── metrics.go                          ← NEW (Task 9)
│       ├── bootstrap.go                        ← NEW (Task 10)
│       ├── get_target_version.go               ← NEW (Task 11)
│       ├── report_apply_result.go              ← NEW (Task 12)
│       ├── bundle_cache.go                     ← NEW (Task 13)
│       ├── bundle_cache_test.go                ← NEW (Task 13)
│       ├── get_bundle_manifest.go              ← NEW (Task 13)
│       ├── get_bundle_blob.go                  ← NEW (Task 13)
│       ├── subscribe.go                        ← NEW (Task 14)
│       ├── vault_get.go                        ← NEW (Task 15)
│       ├── get_cert.go                         ← NEW (Task 15)
│       ├── storage_host_inventory_get.go       ← NEW (Task 15)
│       ├── publish_bundle.go                   ← NEW (Task 16)
│       ├── set_target_version.go               ← NEW (Task 17)
│       ├── count_by_version.go                 ← NEW (Task 17)
│       ├── get_client_state.go                 ← NEW (Task 17)
│       ├── rollout_math.go                     ← NEW (Task 18)
│       ├── rollout_math_test.go                ← NEW (Task 18)
│       ├── pod_hashing.go                      ← NEW (Task 19)
│       ├── pod_hashing_test.go                 ← NEW (Task 19)
│       ├── rollout_leader.go                   ← NEW (Task 20)
│       ├── rollout_worker.go                   ← NEW (Task 21)
│       ├── issue_rollout.go                    ← NEW (Task 22)
│       ├── get_rollout_status.go               ← NEW (Task 22)
│       ├── rollout_control.go                  ← NEW (Task 22, Pause/Resume/Abort)
│       ├── kick_rollout.go                     ← NEW (Task 22)
│       └── pod_registry.go                     ← NEW (Task 23)
└── cmd/chiit-server/
    └── main.go                                 ← MODIFY (Task 24)
```

## Test invocation reference

- Unit-тесты Go: `go test ./...`
- Integration (с PG): `go test -tags integration ./internal/repository/...` — требует env `POSTGRES_DB`, `POSTGRES_HOST`, `POSTGRES_USER`, `POSTGRES_PASSWORD`.
- Lint/codegen: `make generate` (после правки proto).

---

## Task 1: Proto-файл BosunAPI

**Files:**
- Create: `api/bosun/api.proto`

- [ ] **Step 1: Создать файл `api/bosun/api.proto`**

Полный контент файла:

```protobuf
syntax = "proto3";

package ozon.infrastructure.cloudozon.bosun;

option go_package = "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun;bosun";
option csharp_namespace = "Ozon.Infrastructure.Cloudozon.bosun";

import "google/api/annotations.proto";
import "google/protobuf/timestamp.proto";
import "google/protobuf/duration.proto";
import "google/protobuf/empty.proto";
import "protoc-gen-openapiv2/options/annotations.proto";
import "api/chiit/api.proto";   // VaultOut, CertificateData
import "gitlab.ozon.ru/infrastructure/postgresql/storage-inventory/api/inventory/inventory.api.proto";

service BosunAPI {
  // --- BOOTSTRAP ---
  rpc Bootstrap(BootstrapIn) returns (BootstrapOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Регистрация новой ноды bosun-client",
      description: "Регистрация ноды без существующего chiit ECDSA-ключа",
      tags: "Bootstrap",
      security: {
        security_requirement: {
          key: "validator-registration-token";
          value: {};
        },
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/bootstrap"
      body: "*"
    };
  }

  // --- PULL MODEL (auth = ECDSA-sig) ---
  rpc GetTargetVersion(GetTargetVersionIn) returns (GetTargetVersionOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Pull целевой версии bundle для клиента",
      description: "Heartbeat + pull. Возвращает target_version и pod_redirect.",
      tags: "Pull"
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/target-version"
      body: "*"
    };
  }
  rpc Subscribe(SubscribeIn) returns (stream Kick) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Kick-канал для ускорения следующего pull",
      description: "Server-stream, в обычное время молчит",
      tags: "Pull"
    };
  }
  rpc ReportApplyResult(ReportApplyResultIn) returns (ReportApplyResultOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Отчёт о результате apply bundle на клиенте",
      description: "Идемпотентный отчёт. Apply сам по себе идемпотентен.",
      tags: "Pull"
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/apply-result"
      body: "*"
    };
  }
  rpc GetBundleManifest(GetBundleManifestIn) returns (GetBundleManifestOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Manifest для конкретной версии bundle",
      description: "sha256 + signature + size",
      tags: "Bundle"
    };
    option (google.api.http) = {
      get: "/api/bosun/v1/bundle/{version}/manifest"
    };
  }
  rpc GetBundleBlob(GetBundleBlobIn) returns (stream BundleChunk) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Стриминг blob'а bundle клиенту",
      description: "Server-stream, chunks по ~64KB",
      tags: "Bundle"
    };
  }

  // --- AGENT-FACING reuse из chiit (auth = ECDSA-sig) ---
  rpc VaultGet(VaultGetIn) returns (chiit.VaultOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Прокси на chiit VaultGet",
      tags: "Proxy"
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/vault/get"
      body: "*"
    };
  }
  rpc GetCert(GetCertIn) returns (chiit.CertificateData) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Прокси на chiit GetCert",
      tags: "Proxy"
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/cert/get"
      body: "*"
    };
  }
  rpc StorageHostInventoryGet(StorageHostInventoryGetIn)
      returns (StorageHostInventoryGetOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Прокси на storage-inventory",
      tags: "Proxy"
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/inventory/get"
      body: "*"
    };
  }

  // --- CI: bundle publication (auth = admin-token) ---
  rpc PublishBundle(PublishBundleIn) returns (PublishBundleOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Публикация нового bundle (CI service-account)",
      description: "Immutable. sha256+signature+size проверяются на сервере.",
      tags: "Bundle",
      security: {
        security_requirement: {
          key: "admin-token";
          value: {};
        },
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/bundle/publish"
      body: "*"
    };
  }

  // --- OPERATOR (auth = admin-token или Keycloak JWT) ---
  rpc SetTargetVersion(SetTargetVersionIn) returns (SetTargetVersionOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Точечный override target_version на хостах",
      description: "Минуя rollout-постепенность. Для emergency и debug.",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/target-version/set"
      body: "*"
    };
  }
  rpc IssueRollout(IssueRolloutIn) returns (IssueRolloutOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Запустить постепенный rollout",
      description: "Server-driven gradual с failure-rate halt",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/rollout/issue"
      body: "*"
    };
  }
  rpc GetRolloutStatus(GetRolloutStatusIn) returns (GetRolloutStatusOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Текущий status и счётчики rollout'а",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      get: "/api/bosun/v1/rollout/{rollout_id}/status"
    };
  }
  rpc PauseRollout(RolloutControlIn) returns (RolloutControlOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Поставить rollout на паузу",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/rollout/{rollout_id}/pause"
      body: "*"
    };
  }
  rpc ResumeRollout(RolloutControlIn) returns (RolloutControlOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Снять паузу с rollout'а",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/rollout/{rollout_id}/resume"
      body: "*"
    };
  }
  rpc AbortRollout(RolloutControlIn) returns (RolloutControlOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Прервать rollout (необратимо)",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/rollout/{rollout_id}/abort"
      body: "*"
    };
  }
  rpc KickRollout(KickRolloutIn) returns (KickRolloutOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Ускорить раскатку через Subscribe Kick",
      description: "Push Kick по live Subscribe-stream'ам матчинговым клиентам",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/rollout/{rollout_id}/kick"
      body: "*"
    };
  }
  rpc GetClientState(GetClientStateIn) returns (GetClientStateOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Полный state одного клиента (для debug)",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      get: "/api/bosun/v1/client/{host}/state"
    };
  }
  rpc CountByVersion(CountByVersionIn) returns (CountByVersionOut) {
    option (grpc.gateway.protoc_gen_openapiv2.options.openapiv2_operation) = {
      summary: "Распределение fleet по current_version",
      tags: "Operator",
      security: {
        security_requirement: {key: "admin-token"; value: {};},
        security_requirement: {key: "bearer-token"; value: {};},
      }
    };
    option (google.api.http) = {
      post: "/api/bosun/v1/clients/count-by-version"
      body: "*"
    };
  }
}

// =========================================================
// Common
// =========================================================

message Target {
  repeated string hosts            = 1;   // explicit list
  repeated string clusters         = 2;   // resolved через inventory
  string          severity_class   = 3;   // 'low'/'medium'/'high'/'test'
  string          environment      = 4;   // 'staging' | 'production'
}

message ApplyOutcome {
  uint64 applied_version = 1;
  bool   success         = 2;
  int32  exit_code       = 3;
  string error_excerpt   = 4;
}

// =========================================================
// Bootstrap
// =========================================================
message BootstrapIn {
  string registration_token = 1;
  string host               = 2;
  bytes  client_public_key  = 3;
  string bosun_version      = 4;
}

message BootstrapOut {
  bytes  client_certificate = 1;
  bytes  ca_chain           = 2;
}

// =========================================================
// Pull
// =========================================================
message GetTargetVersionIn {
  string host                   = 1;
  uint64 current_version        = 2;   // что клиент уже успешно применил локально
  uint64 last_attempted_version = 3;   // что пытался применить последний раз
  string sharding_key           = 4;   // для consistent hashing redirect
  string bosun_version          = 5;
  string created_at             = 6;   // RFC3339, для ECDSA-sig
  bytes  sign                   = 7;   // ECDSA-подпись host+created_at
}

message GetTargetVersionOut {
  uint64 target_version       = 1;     // 0 = nothing to do
  string target_rollout_id    = 2;     // optional, для accounting
  uint32 pull_jitter_seconds  = 3;     // клиент рандомно ждёт перед apply
  bool   pod_redirect         = 10;
  string redirect_addr        = 11;
}

message SubscribeIn {
  string host       = 1;
  string created_at = 2;
  bytes  sign       = 3;
}

message Kick {
  google.protobuf.Timestamp issued_at = 1;
  string reason                       = 2;   // 'rollout_started' | 'operator_kick'
}

message ReportApplyResultIn {
  string       host         = 1;
  ApplyOutcome outcome      = 2;
  string       created_at   = 3;
  bytes        sign         = 4;
  string       bosun_version = 5;
}

message ReportApplyResultOut {
  // empty
}

message GetBundleManifestIn {
  string host       = 1;
  uint64 version    = 2;
  string created_at = 3;
  bytes  sign       = 4;
}

message GetBundleManifestOut {
  uint64                    version      = 1;
  bytes                     sha256       = 2;
  bytes                     signature    = 3;
  int64                     size_bytes   = 4;
  google.protobuf.Timestamp published_at = 5;
}

message GetBundleBlobIn {
  string host       = 1;
  uint64 version    = 2;
  string created_at = 3;
  bytes  sign       = 4;
}

message BundleChunk {
  bytes data = 1;
}

// =========================================================
// Proxy
// =========================================================
message VaultGetIn {
  string host       = 1;
  string path       = 2;
  string created_at = 3;
  bytes  sign       = 4;
}

message GetCertIn {
  string host       = 1;
  string sni        = 2;
  string created_at = 3;
  bytes  sign       = 4;
}

message StorageHostInventoryGetIn {
  string host       = 1;
  string created_at = 2;
  bytes  sign       = 3;
}

message StorageHostInventoryGetOut {
  ozon.infrastructure.postgresql.storage_inventory.api.StorageHostInventory inventory = 1;
}

// =========================================================
// CI: PublishBundle
// =========================================================
message PublishBundleIn {
  bytes           blob       = 1;   // tar.gz, max 5 MiB enforced
  bytes           sha256     = 2;
  bytes           signature  = 3;
  repeated string tags       = 4;
}

message PublishBundleOut {
  uint64 version = 1;
}

// =========================================================
// Operator
// =========================================================
message SetTargetVersionIn {
  Target target         = 1;
  uint64 target_version = 2;   // если 0 — сброс к NULL
  string reason         = 3;
}

message SetTargetVersionOut {
  uint32 hosts_updated = 1;
}

message IssueRolloutIn {
  uint64                    target_version    = 1;
  Target                    target            = 2;
  google.protobuf.Duration  over_duration     = 3;
  float                     max_failure_rate  = 4;
  uint32                    min_evaluated     = 5;   // before halt
  uint32                    max_batch_size    = 6;   // per worker tick
  string                    reason            = 7;
}

message IssueRolloutOut {
  string rollout_id    = 1;
  uint32 total_targets = 2;
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

message GetRolloutStatusIn {
  string rollout_id = 1;
}

message GetRolloutStatusOut {
  string                    rollout_id            = 1;
  RolloutState              state                 = 2;
  string                    halt_reason           = 3;
  uint32                    total_targets         = 10;
  uint32                    dispatched_targets    = 11;
  uint32                    succeeded_targets     = 12;
  uint32                    failed_targets        = 13;
  uint32                    pending_targets       = 14;
  float                     current_failure_rate  = 20;
  int64                     elapsed_active_sec    = 21;
  google.protobuf.Timestamp issued_at             = 30;
  google.protobuf.Timestamp started_at            = 31;
  google.protobuf.Timestamp halted_at             = 32;
  google.protobuf.Timestamp completed_at          = 33;
}

message RolloutControlIn {
  string rollout_id = 1;
  string reason     = 2;
}

message RolloutControlOut {
  RolloutState new_state = 1;
}

message KickRolloutIn {
  string rollout_id = 1;
}

message KickRolloutOut {
  uint32 kicked = 1;
}

message GetClientStateIn {
  string host = 1;
}

message GetClientStateOut {
  string                    host                    = 1;
  uint64                    target_version          = 2;
  string                    target_rollout_id       = 3;
  google.protobuf.Timestamp target_set_at           = 4;
  uint64                    current_version         = 5;
  google.protobuf.Timestamp current_set_at          = 6;
  uint64                    last_attempted_version  = 7;
  bool                      last_attempt_success    = 8;
  google.protobuf.Timestamp last_attempt_at         = 9;
  int32                     last_attempt_exit_code  = 10;
  string                    last_attempt_error      = 11;
  google.protobuf.Timestamp last_seen_at            = 12;
  string                    bosun_version           = 13;
}

message CountByVersionIn {
  Target target = 1;
}

message CountByVersionOut {
  map<uint64, uint32> count_by_current_version = 1;   // version → host count
  uint32              total                    = 2;
  uint32              never_applied            = 3;   // current_version IS NULL
}
```

- [ ] **Step 2: Запустить codegen**

Команда: `make generate`

Что должно появиться:
- `api/bosun/api.pb.go`
- `api/bosun/api_grpc.pb.go`
- `api/bosun/api.pb.gw.go`
- `api/bosun/api.pb.scratch.go`  ← важно: scratch будет искать `Implementation` в указанном `implementation_import`. Адресуй на `internal/app/ozon/infrastructure/cloudozon/bosun`.
- `api/bosun/api.pb.sensitivity.go`
- `api/bosun/api_vtproto.pb.go`
- `api/bosun/api.swagger.json` (вероятно объединится с общим)

Если scratch ищет implementation в неправильном месте — нужно скорректировать `buf.gen.yaml` (per-directory option overrides). Открой `buf.gen.yaml` и добавь scratch-плагин с `implementation_import=...internal/app/ozon/infrastructure/cloudozon/bosun, implementation_name=Implementation` если общий путь не подходит.

- [ ] **Step 3: Коммит**

```bash
git add api/bosun/ buf.gen.yaml
git commit -m "feat(bosun): add BosunAPI proto"
```

---

## Task 2: PG migration со схемой

**Files:**
- Create: `migrations/24_bosun_init.sql`

- [ ] **Step 1: Создать миграцию**

```sql
-- +goose Up
-- +goose StatementBegin

-- bosun_clients: одна строка на host.
CREATE TABLE bosun_clients (
    host                    TEXT PRIMARY KEY,
    target_version          BIGINT,
    target_rollout_id       UUID,
    target_set_at           TIMESTAMPTZ,
    current_version         BIGINT,
    current_set_at          TIMESTAMPTZ,
    last_attempted_version  BIGINT,
    last_attempt_success    BOOLEAN,
    last_attempt_at         TIMESTAMPTZ,
    last_attempt_exit_code  INT,
    last_attempt_error      TEXT,
    last_seen_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    bosun_version           TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX bosun_clients_target_rollout_id_idx
    ON bosun_clients(target_rollout_id)
    WHERE target_rollout_id IS NOT NULL;

CREATE INDEX bosun_clients_current_version_idx
    ON bosun_clients(current_version);

-- bosun_bundles: immutable, append-only.
CREATE TABLE bosun_bundles (
    version       BIGSERIAL PRIMARY KEY,
    sha256        BYTEA NOT NULL UNIQUE,
    blob          BYTEA NOT NULL,
    size_bytes    BIGINT NOT NULL,
    signature     BYTEA NOT NULL,
    tags          TEXT[] NOT NULL DEFAULT '{}',
    published_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_by  TEXT NOT NULL
);

-- bosun_pods: registry для consistent hashing redirect.
CREATE TABLE bosun_pods (
    pod_id         TEXT PRIMARY KEY,
    addr           TEXT NOT NULL,
    started_at     TIMESTAMPTZ NOT NULL,
    last_ping_at   TIMESTAMPTZ NOT NULL,
    draining       BOOLEAN NOT NULL DEFAULT FALSE
);

-- bosun_rollouts: одна строка на rollout, без per-host ledger.
CREATE TABLE bosun_rollouts (
    rollout_id          UUID PRIMARY KEY,
    target_version      BIGINT NOT NULL REFERENCES bosun_bundles(version),
    target_spec         JSONB NOT NULL,
    target_snapshot     JSONB NOT NULL,
    total_targets       INT NOT NULL,
    over_duration_sec   INT NOT NULL,
    max_failure_rate    REAL NOT NULL,
    max_batch_size      INT NOT NULL,
    min_evaluated       INT NOT NULL,
    state               TEXT NOT NULL DEFAULT 'pending',
    halt_reason         TEXT,
    issued_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    issued_by           TEXT NOT NULL,
    started_at          TIMESTAMPTZ,
    paused_at           TIMESTAMPTZ,
    total_paused_sec    INT NOT NULL DEFAULT 0,
    halted_at           TIMESTAMPTZ,
    aborted_at          TIMESTAMPTZ,
    completed_at        TIMESTAMPTZ,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX bosun_rollouts_state_idx
    ON bosun_rollouts(state)
    WHERE state IN ('pending', 'running', 'paused');

-- bosun_leader: lease + fencing epoch.
CREATE TABLE bosun_leader (
    role            TEXT PRIMARY KEY,
    pod_id          TEXT NOT NULL,
    epoch           BIGINT NOT NULL,
    acquired_at     TIMESTAMPTZ NOT NULL,
    last_heartbeat  TIMESTAMPTZ NOT NULL,
    expires_at      TIMESTAMPTZ NOT NULL
);

-- bosun_audit: оператор-actions + критичные системные события.
CREATE TABLE bosun_audit (
    audit_id      BIGSERIAL PRIMARY KEY,
    happened_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    event_type    TEXT NOT NULL,
    actor         TEXT NOT NULL,
    actor_kind    TEXT NOT NULL,
    rollout_id    UUID,
    host          TEXT,
    payload       JSONB NOT NULL
);

CREATE INDEX bosun_audit_happened_at_idx ON bosun_audit(happened_at);
CREATE INDEX bosun_audit_rollout_id_idx
    ON bosun_audit(rollout_id) WHERE rollout_id IS NOT NULL;

-- +goose StatementEnd
```

- [ ] **Step 2: Verify через repository.Init** (через integration test позднее)

- [ ] **Step 3: Коммит**

```bash
git add migrations/24_bosun_init.sql
git commit -m "feat(bosun): add PG schema migration"
```

---

## Task 3: Repository — bosun_clients

**Files:**
- Modify: `internal/repository/types.go` (расширить `Manager` interface)
- Create: `internal/repository/bosun_clients.go`
- Create: `internal/repository/bosun_clients_test.go`

- [ ] **Step 1: Расширить Manager interface**

В `internal/repository/types.go` добавь:

```go
type bosunClients interface {
    GetBosunClient(ctx context.Context, host string) (*BosunClient, error)
    UpsertBosunClientLazy(ctx context.Context, host, bosunVersion string) error
    UpdateBosunClientLastSeen(ctx context.Context, host, bosunVersion string) error
    UpdateBosunClientApplyResult(ctx context.Context, in UpdateBosunApplyResultIn) error
    SetBosunClientTarget(ctx context.Context, hosts []string, targetVersion int64,
                         rolloutID *uuid.UUID, setNullIfZero bool) (int64, error)
    CountBosunByCurrentVersion(ctx context.Context, target *BosunTargetFilter) (map[int64]int, int, int, error)
    SelectBosunHostsForRollout(ctx context.Context, snapshotHosts []string, rolloutID uuid.UUID,
                                exclude bool, limit int) ([]string, error)
    CountBosunRolloutStatus(ctx context.Context, rolloutID uuid.UUID, targetVersion int64,
                            cohort []string) (BosunRolloutCounters, error)
}

type Manager interface {
    // ... existing interfaces (whiteList, canary, alertEvents, talosRepository, и т.д.)
    bosunClients
    bosunBundles
    bosunRollouts
    bosunPods
    bosunLeader
    bosunAudit
}
```

И добавь типы (там же или в bosun_clients.go):

```go
type BosunClient struct {
    Host                  string
    TargetVersion         *int64
    TargetRolloutID       *uuid.UUID
    TargetSetAt           *time.Time
    CurrentVersion        *int64
    CurrentSetAt          *time.Time
    LastAttemptedVersion  *int64
    LastAttemptSuccess    *bool
    LastAttemptAt         *time.Time
    LastAttemptExitCode   *int32
    LastAttemptError      *string
    LastSeenAt            time.Time
    BosunVersion          string
    CreatedAt             time.Time
}

type UpdateBosunApplyResultIn struct {
    Host         string
    Outcome      ApplyOutcome
    BosunVersion string
}

type ApplyOutcome struct {
    AppliedVersion int64
    Success        bool
    ExitCode       int32
    ErrorExcerpt   string
}

type BosunTargetFilter struct {
    Hosts          []string
    Clusters       []string
    SeverityClass  string
    Environment    string
}

type BosunRolloutCounters struct {
    Dispatched int
    Succeeded  int
    Failed     int
    Pending    int
}
```

- [ ] **Step 2: Реализовать bosun_clients.go**

Файл `internal/repository/bosun_clients.go`:

```go
package repository

import (
    "context"
    "fmt"
    "time"

    "github.com/google/uuid"
    "gitlab.ozon.ru/platform/go/database-pg/roles"
)

// GetBosunClient — основной read путь.
func (r *Repository) GetBosunClient(ctx context.Context, host string) (*BosunClient, error) {
    const q = `
        SELECT host, target_version, target_rollout_id, target_set_at,
               current_version, current_set_at,
               last_attempted_version, last_attempt_success, last_attempt_at,
               last_attempt_exit_code, last_attempt_error,
               last_seen_at, bosun_version, created_at
          FROM bosun_clients
         WHERE host = $1`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    var c BosunClient
    err := pool.QueryRow(ctx, q, host).Scan(
        &c.Host, &c.TargetVersion, &c.TargetRolloutID, &c.TargetSetAt,
        &c.CurrentVersion, &c.CurrentSetAt,
        &c.LastAttemptedVersion, &c.LastAttemptSuccess, &c.LastAttemptAt,
        &c.LastAttemptExitCode, &c.LastAttemptError,
        &c.LastSeenAt, &c.BosunVersion, &c.CreatedAt,
    )
    if err != nil {
        return nil, setAPIErrors(err)
    }
    return &c, nil
}

// UpsertBosunClientLazy — INSERT для legacy chiit-ноды (без Bootstrap).
// На первом аутентифицированном RPC.
func (r *Repository) UpsertBosunClientLazy(ctx context.Context, host, bosunVersion string) error {
    const q = `
        INSERT INTO bosun_clients (host, last_seen_at, bosun_version)
        VALUES ($1, NOW(), $2)
        ON CONFLICT (host) DO NOTHING`
    _, err := r.balancer.PickPool(ctx, roles.RoleMaster).Exec(ctx, q, host, bosunVersion)
    return setAPIErrors(err)
}

// UpdateBosunClientLastSeen — diff-update: только если данные устарели
// или bosun_version изменился. Снимает hot-path 2k indexed UPDATE/sec.
func (r *Repository) UpdateBosunClientLastSeen(ctx context.Context, host, bosunVersion string) error {
    const q = `
        UPDATE bosun_clients
           SET last_seen_at = NOW(),
               bosun_version = $2
         WHERE host = $1
           AND (last_seen_at < NOW() - INTERVAL '5 minutes'
                OR bosun_version IS DISTINCT FROM $2)`
    _, err := r.balancer.PickPool(ctx, roles.RoleMaster).Exec(ctx, q, host, bosunVersion)
    return setAPIErrors(err)
}

// UpdateBosunClientApplyResult — единственный путь, который пишет
// current_version + last_attempt_*. Идемпотентен по последнему report'у.
func (r *Repository) UpdateBosunClientApplyResult(ctx context.Context, in UpdateBosunApplyResultIn) error {
    const q = `
        UPDATE bosun_clients
           SET last_attempted_version = $2,
               last_attempt_success   = $3,
               last_attempt_at        = NOW(),
               last_attempt_exit_code = $4,
               last_attempt_error     = $5,
               current_version        = CASE WHEN $3 THEN $2 ELSE current_version END,
               current_set_at         = CASE WHEN $3 THEN NOW() ELSE current_set_at END,
               last_seen_at           = NOW(),
               bosun_version          = COALESCE(NULLIF($6, ''), bosun_version)
         WHERE host = $1`
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)
    tag, err := pool.Exec(ctx, q,
        in.Host, in.Outcome.AppliedVersion, in.Outcome.Success,
        in.Outcome.ExitCode, in.Outcome.ErrorExcerpt, in.BosunVersion)
    if err != nil {
        return setAPIErrors(err)
    }
    if tag.RowsAffected() == 0 {
        return ErrNotFound
    }
    return nil
}

// SetBosunClientTarget — UPDATE для SetTargetVersion override или для rollout worker batch.
// rolloutID = nil → override (target_rollout_id = NULL).
// setNullIfZero = true → targetVersion == 0 ставит target_version = NULL (явный сброс).
func (r *Repository) SetBosunClientTarget(ctx context.Context, hosts []string,
    targetVersion int64, rolloutID *uuid.UUID, setNullIfZero bool) (int64, error) {
    if len(hosts) == 0 {
        return 0, nil
    }
    var q string
    var args []any
    if setNullIfZero && targetVersion == 0 {
        q = `UPDATE bosun_clients
                SET target_version = NULL,
                    target_rollout_id = NULL,
                    target_set_at = NOW()
              WHERE host = ANY($1)`
        args = []any{hosts}
    } else {
        q = `UPDATE bosun_clients
                SET target_version = $1,
                    target_rollout_id = $2,
                    target_set_at = NOW()
              WHERE host = ANY($3)`
        args = []any{targetVersion, rolloutID, hosts}
    }
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)
    tag, err := pool.Exec(ctx, q, args...)
    if err != nil {
        return 0, setAPIErrors(err)
    }
    return tag.RowsAffected(), nil
}

// SelectBosunHostsForRollout — выбрать следующий батч hosts из snapshot'а,
// которые ещё не закрыты в этом rollout'е.
func (r *Repository) SelectBosunHostsForRollout(ctx context.Context, snapshotHosts []string,
    rolloutID uuid.UUID, exclude bool, limit int) ([]string, error) {
    if len(snapshotHosts) == 0 || limit <= 0 {
        return nil, nil
    }
    const q = `
        SELECT host
          FROM bosun_clients
         WHERE host = ANY($1)
           AND (target_rollout_id IS NULL OR target_rollout_id <> $2)
         ORDER BY host
         LIMIT $3`
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)
    rows, err := pool.Query(ctx, q, snapshotHosts, rolloutID, limit)
    if err != nil {
        return nil, setAPIErrors(err)
    }
    defer rows.Close()
    var hosts []string
    for rows.Next() {
        var h string
        if err := rows.Scan(&h); err != nil {
            return nil, setAPIErrors(err)
        }
        hosts = append(hosts, h)
    }
    return hosts, nil
}

// CountBosunRolloutStatus — агрегаты для rollout worker и GetRolloutStatus.
func (r *Repository) CountBosunRolloutStatus(ctx context.Context, rolloutID uuid.UUID,
    targetVersion int64, cohort []string) (BosunRolloutCounters, error) {
    const q = `
        SELECT
            COUNT(*) FILTER (WHERE target_rollout_id = $1)                                                                        AS dispatched,
            COUNT(*) FILTER (WHERE target_rollout_id = $1 AND current_version = $2)                                                AS succeeded,
            COUNT(*) FILTER (WHERE target_rollout_id = $1
                              AND last_attempted_version = $2
                              AND last_attempt_success = false
                              AND (current_version IS NULL OR current_version <> $2))                                              AS failed
          FROM bosun_clients
         WHERE host = ANY($3)`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    var c BosunRolloutCounters
    err := pool.QueryRow(ctx, q, rolloutID, targetVersion, cohort).Scan(
        &c.Dispatched, &c.Succeeded, &c.Failed)
    if err != nil {
        return c, setAPIErrors(err)
    }
    c.Pending = c.Dispatched - c.Succeeded - c.Failed
    return c, nil
}

// CountBosunByCurrentVersion — для CountByVersion RPC.
// Возвращает (counters by current_version, total, never_applied).
func (r *Repository) CountBosunByCurrentVersion(ctx context.Context,
    target *BosunTargetFilter) (map[int64]int, int, int, error) {
    cohort, err := r.expandBosunTarget(ctx, target)
    if err != nil {
        return nil, 0, 0, err
    }
    const q = `
        SELECT current_version, COUNT(*)
          FROM bosun_clients
         WHERE host = ANY($1)
         GROUP BY current_version`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    rows, err := pool.Query(ctx, q, cohort)
    if err != nil {
        return nil, 0, 0, setAPIErrors(err)
    }
    defer rows.Close()
    result := make(map[int64]int)
    total := 0
    never := 0
    for rows.Next() {
        var v *int64
        var n int
        if err := rows.Scan(&v, &n); err != nil {
            return nil, 0, 0, setAPIErrors(err)
        }
        if v == nil {
            never += n
        } else {
            result[*v] = n
        }
        total += n
    }
    return result, total, never, nil
}

// expandBosunTarget — резолвит Target в host-list через inventory.
// TODO: реализовать; для P0 можно поддержать только hosts.
func (r *Repository) expandBosunTarget(ctx context.Context, t *BosunTargetFilter) ([]string, error) {
    if t == nil {
        return nil, fmt.Errorf("target is required")
    }
    if len(t.Hosts) > 0 {
        return t.Hosts, nil
    }
    // TODO P0: severity_class и clusters через inventory сервис
    return nil, fmt.Errorf("expandBosunTarget: severity/clusters not yet implemented")
}

var _ = time.Now  // keep import
```

- [ ] **Step 3: Integration test для bosun_clients**

Файл `internal/repository/bosun_clients_test.go` (build tag `//go:build integration`):

```go
//go:build integration

package repository

import (
    "context"
    "testing"
    "time"

    "github.com/google/uuid"
    "github.com/stretchr/testify/assert"
    "github.com/stretchr/testify/require"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository/fixtures"
)

func TestBosunClient_LazyInsertAndUpdate(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)

    require.NoError(t, repo.UpsertBosunClientLazy(ctx, "h1", "v1.0"))

    // повторный INSERT — no-op
    require.NoError(t, repo.UpsertBosunClientLazy(ctx, "h1", "v1.0"))

    c, err := repo.GetBosunClient(ctx, "h1")
    require.NoError(t, err)
    assert.Equal(t, "h1", c.Host)
    assert.Equal(t, "v1.0", c.BosunVersion)
    assert.Nil(t, c.TargetVersion)
    assert.Nil(t, c.CurrentVersion)
}

func TestBosunClient_UpdateLastSeenDiffsOnly(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)
    require.NoError(t, repo.UpsertBosunClientLazy(ctx, "h2", "v1.0"))

    // первый UPDATE — не сработает (last_seen_at свеж, bosun_version тот же)
    before, err := repo.GetBosunClient(ctx, "h2")
    require.NoError(t, err)
    require.NoError(t, repo.UpdateBosunClientLastSeen(ctx, "h2", "v1.0"))
    after, err := repo.GetBosunClient(ctx, "h2")
    require.NoError(t, err)
    assert.Equal(t, before.LastSeenAt.Unix(), after.LastSeenAt.Unix())

    // diff bosun_version → должно обновиться
    require.NoError(t, repo.UpdateBosunClientLastSeen(ctx, "h2", "v1.1"))
    after, err = repo.GetBosunClient(ctx, "h2")
    require.NoError(t, err)
    assert.Equal(t, "v1.1", after.BosunVersion)
}

func TestBosunClient_ApplyResultUpdatesCurrentOnSuccess(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)
    require.NoError(t, repo.UpsertBosunClientLazy(ctx, "h3", "v1.0"))

    // failed apply — current_version остаётся nil
    require.NoError(t, repo.UpdateBosunClientApplyResult(ctx, UpdateBosunApplyResultIn{
        Host: "h3",
        Outcome: ApplyOutcome{AppliedVersion: 42, Success: false, ExitCode: 1, ErrorExcerpt: "boom"},
        BosunVersion: "v1.0",
    }))
    c, err := repo.GetBosunClient(ctx, "h3")
    require.NoError(t, err)
    assert.Nil(t, c.CurrentVersion)
    require.NotNil(t, c.LastAttemptedVersion)
    assert.Equal(t, int64(42), *c.LastAttemptedVersion)
    require.NotNil(t, c.LastAttemptSuccess)
    assert.False(t, *c.LastAttemptSuccess)

    // successful apply — current_version обновляется
    require.NoError(t, repo.UpdateBosunClientApplyResult(ctx, UpdateBosunApplyResultIn{
        Host: "h3",
        Outcome: ApplyOutcome{AppliedVersion: 43, Success: true, ExitCode: 0, ErrorExcerpt: ""},
        BosunVersion: "v1.0",
    }))
    c, err = repo.GetBosunClient(ctx, "h3")
    require.NoError(t, err)
    require.NotNil(t, c.CurrentVersion)
    assert.Equal(t, int64(43), *c.CurrentVersion)
}

func TestBosunClient_SetTargetAndCount(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)
    rid := uuid.New()
    for _, h := range []string{"r1", "r2", "r3"} {
        require.NoError(t, repo.UpsertBosunClientLazy(ctx, h, "v"))
    }

    n, err := repo.SetBosunClientTarget(ctx, []string{"r1", "r2"}, 42, &rid, false)
    require.NoError(t, err)
    assert.Equal(t, int64(2), n)

    counters, err := repo.CountBosunRolloutStatus(ctx, rid, 42, []string{"r1", "r2", "r3"})
    require.NoError(t, err)
    assert.Equal(t, 2, counters.Dispatched)
    assert.Equal(t, 0, counters.Succeeded)
    assert.Equal(t, 0, counters.Failed)
    assert.Equal(t, 2, counters.Pending)
}

var _ = time.Now  // keep import
```

- [ ] **Step 4: Запустить тесты**

```bash
go test -tags integration ./internal/repository/... -run TestBosunClient
```

Ожидается: 4/4 PASS.

- [ ] **Step 5: Коммит**

```bash
git add internal/repository/types.go internal/repository/bosun_clients.go internal/repository/bosun_clients_test.go
git commit -m "feat(bosun): add bosun_clients repository layer"
```

---

## Task 4: Repository — bosun_bundles

**Files:**
- Create: `internal/repository/bosun_bundles.go`
- Create: `internal/repository/bosun_bundles_test.go`

- [ ] **Step 1: Расширить Manager interface**

Добавь в `types.go`:

```go
type bosunBundles interface {
    InsertBosunBundle(ctx context.Context, in InsertBosunBundleIn) (int64, error)
    GetBosunBundleManifest(ctx context.Context, version int64) (*BosunBundleManifest, error)
    GetBosunBundleBlob(ctx context.Context, version int64) ([]byte, *BosunBundleManifest, error)
    LatestBosunBundleVersion(ctx context.Context) (int64, error)
}

type InsertBosunBundleIn struct {
    Blob        []byte
    SHA256      []byte
    Signature   []byte
    Tags        []string
    SizeBytes   int64
    PublishedBy string
}

type BosunBundleManifest struct {
    Version     int64
    SHA256      []byte
    Signature   []byte
    SizeBytes   int64
    Tags        []string
    PublishedAt time.Time
    PublishedBy string
}
```

- [ ] **Step 2: Реализовать bosun_bundles.go**

```go
package repository

import (
    "context"

    "gitlab.ozon.ru/platform/go/database-pg/roles"
)

func (r *Repository) InsertBosunBundle(ctx context.Context, in InsertBosunBundleIn) (int64, error) {
    const q = `
        INSERT INTO bosun_bundles (sha256, blob, size_bytes, signature, tags, published_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING version`
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)
    var version int64
    err := pool.QueryRow(ctx, q,
        in.SHA256, in.Blob, in.SizeBytes, in.Signature, in.Tags, in.PublishedBy,
    ).Scan(&version)
    if err != nil {
        return 0, setAPIErrors(err)
    }
    return version, nil
}

func (r *Repository) GetBosunBundleManifest(ctx context.Context, version int64) (*BosunBundleManifest, error) {
    const q = `
        SELECT version, sha256, signature, size_bytes, tags, published_at, published_by
          FROM bosun_bundles
         WHERE version = $1`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    var m BosunBundleManifest
    err := pool.QueryRow(ctx, q, version).Scan(
        &m.Version, &m.SHA256, &m.Signature, &m.SizeBytes,
        &m.Tags, &m.PublishedAt, &m.PublishedBy)
    if err != nil {
        return nil, setAPIErrors(err)
    }
    return &m, nil
}

func (r *Repository) GetBosunBundleBlob(ctx context.Context, version int64) ([]byte, *BosunBundleManifest, error) {
    const q = `
        SELECT version, sha256, signature, size_bytes, tags, published_at, published_by, blob
          FROM bosun_bundles
         WHERE version = $1`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    var m BosunBundleManifest
    var blob []byte
    err := pool.QueryRow(ctx, q, version).Scan(
        &m.Version, &m.SHA256, &m.Signature, &m.SizeBytes,
        &m.Tags, &m.PublishedAt, &m.PublishedBy, &blob)
    if err != nil {
        return nil, nil, setAPIErrors(err)
    }
    return blob, &m, nil
}

func (r *Repository) LatestBosunBundleVersion(ctx context.Context) (int64, error) {
    const q = `SELECT COALESCE(MAX(version), 0) FROM bosun_bundles`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    var v int64
    if err := pool.QueryRow(ctx, q).Scan(&v); err != nil {
        return 0, setAPIErrors(err)
    }
    return v, nil
}
```

- [ ] **Step 3: Integration tests**

`internal/repository/bosun_bundles_test.go`:

```go
//go:build integration

package repository

import (
    "context"
    "crypto/sha256"
    "testing"

    "github.com/stretchr/testify/assert"
    "github.com/stretchr/testify/require"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository/fixtures"
)

func TestBosunBundle_PublishAndFetch(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)

    blob := []byte("fake bundle contents")
    hash := sha256.Sum256(blob)

    version, err := repo.InsertBosunBundle(ctx, InsertBosunBundleIn{
        Blob: blob, SHA256: hash[:],
        Signature: []byte("sig"), Tags: []string{"test"},
        SizeBytes: int64(len(blob)), PublishedBy: "ci",
    })
    require.NoError(t, err)
    assert.Greater(t, version, int64(0))

    m, err := repo.GetBosunBundleManifest(ctx, version)
    require.NoError(t, err)
    assert.Equal(t, version, m.Version)
    assert.Equal(t, hash[:], m.SHA256)
    assert.Equal(t, int64(len(blob)), m.SizeBytes)

    gotBlob, _, err := repo.GetBosunBundleBlob(ctx, version)
    require.NoError(t, err)
    assert.Equal(t, blob, gotBlob)
}

func TestBosunBundle_UniqueSha(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)
    blob := []byte("dup")
    hash := sha256.Sum256(blob)
    _, err := repo.InsertBosunBundle(ctx, InsertBosunBundleIn{
        Blob: blob, SHA256: hash[:], Signature: []byte("s"),
        Tags: []string{}, SizeBytes: int64(len(blob)), PublishedBy: "ci",
    })
    require.NoError(t, err)
    _, err = repo.InsertBosunBundle(ctx, InsertBosunBundleIn{
        Blob: blob, SHA256: hash[:], Signature: []byte("s"),
        Tags: []string{}, SizeBytes: int64(len(blob)), PublishedBy: "ci",
    })
    require.ErrorIs(t, err, ErrAlreadyExists)
}
```

- [ ] **Step 4: Запустить и коммит**

```bash
go test -tags integration ./internal/repository/... -run TestBosunBundle
git add internal/repository/types.go internal/repository/bosun_bundles.go internal/repository/bosun_bundles_test.go
git commit -m "feat(bosun): add bosun_bundles repository"
```

---

## Task 5: Repository — bosun_rollouts

**Files:**
- Create: `internal/repository/bosun_rollouts.go`
- Create: `internal/repository/bosun_rollouts_test.go`

- [ ] **Step 1: Расширить Manager interface**

В `types.go`:

```go
type bosunRollouts interface {
    InsertBosunRollout(ctx context.Context, in InsertBosunRolloutIn) (BosunRollout, error)
    GetBosunRollout(ctx context.Context, rolloutID uuid.UUID) (BosunRollout, error)
    ListRunningBosunRollouts(ctx context.Context) ([]BosunRollout, error)
    UpdateBosunRolloutState(ctx context.Context, rolloutID uuid.UUID,
        state, haltReason string, setStartedAt, setHaltedAt, setAbortedAt, setCompletedAt bool) error
    UpdateBosunRolloutPause(ctx context.Context, rolloutID uuid.UUID,
        toPaused bool) error
}

type InsertBosunRolloutIn struct {
    RolloutID       uuid.UUID
    TargetVersion   int64
    TargetSpec      []byte   // JSON-marshalled Target
    TargetSnapshot  []byte   // JSON-marshalled []string
    TotalTargets    int
    OverDurationSec int
    MaxFailureRate  float32
    MaxBatchSize    int
    MinEvaluated    int
    IssuedBy        string
}

type BosunRollout struct {
    RolloutID       uuid.UUID
    TargetVersion   int64
    TargetSpec      []byte
    TargetSnapshot  []byte
    TotalTargets    int
    OverDurationSec int
    MaxFailureRate  float32
    MaxBatchSize    int
    MinEvaluated    int
    State           string
    HaltReason      *string
    IssuedAt        time.Time
    IssuedBy        string
    StartedAt       *time.Time
    PausedAt        *time.Time
    TotalPausedSec  int
    HaltedAt        *time.Time
    AbortedAt       *time.Time
    CompletedAt     *time.Time
    UpdatedAt       time.Time
}
```

- [ ] **Step 2: Реализовать bosun_rollouts.go**

```go
package repository

import (
    "context"
    "fmt"

    "github.com/google/uuid"
    "gitlab.ozon.ru/platform/go/database-pg/roles"
)

func (r *Repository) InsertBosunRollout(ctx context.Context, in InsertBosunRolloutIn) (BosunRollout, error) {
    const q = `
        INSERT INTO bosun_rollouts (
            rollout_id, target_version, target_spec, target_snapshot, total_targets,
            over_duration_sec, max_failure_rate, max_batch_size, min_evaluated,
            state, issued_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', $10)
        RETURNING rollout_id, target_version, target_spec, target_snapshot, total_targets,
                  over_duration_sec, max_failure_rate, max_batch_size, min_evaluated,
                  state, halt_reason, issued_at, issued_by,
                  started_at, paused_at, total_paused_sec,
                  halted_at, aborted_at, completed_at, updated_at`
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)
    var out BosunRollout
    err := pool.QueryRow(ctx, q,
        in.RolloutID, in.TargetVersion, in.TargetSpec, in.TargetSnapshot, in.TotalTargets,
        in.OverDurationSec, in.MaxFailureRate, in.MaxBatchSize, in.MinEvaluated,
        in.IssuedBy,
    ).Scan(&out.RolloutID, &out.TargetVersion, &out.TargetSpec, &out.TargetSnapshot,
        &out.TotalTargets, &out.OverDurationSec, &out.MaxFailureRate,
        &out.MaxBatchSize, &out.MinEvaluated,
        &out.State, &out.HaltReason, &out.IssuedAt, &out.IssuedBy,
        &out.StartedAt, &out.PausedAt, &out.TotalPausedSec,
        &out.HaltedAt, &out.AbortedAt, &out.CompletedAt, &out.UpdatedAt)
    if err != nil {
        return out, setAPIErrors(err)
    }
    return out, nil
}

func (r *Repository) GetBosunRollout(ctx context.Context, rolloutID uuid.UUID) (BosunRollout, error) {
    const q = `
        SELECT rollout_id, target_version, target_spec, target_snapshot, total_targets,
               over_duration_sec, max_failure_rate, max_batch_size, min_evaluated,
               state, halt_reason, issued_at, issued_by,
               started_at, paused_at, total_paused_sec,
               halted_at, aborted_at, completed_at, updated_at
          FROM bosun_rollouts WHERE rollout_id = $1`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    var out BosunRollout
    err := pool.QueryRow(ctx, q, rolloutID).Scan(
        &out.RolloutID, &out.TargetVersion, &out.TargetSpec, &out.TargetSnapshot,
        &out.TotalTargets, &out.OverDurationSec, &out.MaxFailureRate,
        &out.MaxBatchSize, &out.MinEvaluated,
        &out.State, &out.HaltReason, &out.IssuedAt, &out.IssuedBy,
        &out.StartedAt, &out.PausedAt, &out.TotalPausedSec,
        &out.HaltedAt, &out.AbortedAt, &out.CompletedAt, &out.UpdatedAt)
    if err != nil {
        return out, setAPIErrors(err)
    }
    return out, nil
}

func (r *Repository) ListRunningBosunRollouts(ctx context.Context) ([]BosunRollout, error) {
    const q = `
        SELECT rollout_id, target_version, target_spec, target_snapshot, total_targets,
               over_duration_sec, max_failure_rate, max_batch_size, min_evaluated,
               state, halt_reason, issued_at, issued_by,
               started_at, paused_at, total_paused_sec,
               halted_at, aborted_at, completed_at, updated_at
          FROM bosun_rollouts
         WHERE state IN ('pending', 'running')`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    rows, err := pool.Query(ctx, q)
    if err != nil {
        return nil, setAPIErrors(err)
    }
    defer rows.Close()
    var result []BosunRollout
    for rows.Next() {
        var b BosunRollout
        if err := rows.Scan(
            &b.RolloutID, &b.TargetVersion, &b.TargetSpec, &b.TargetSnapshot,
            &b.TotalTargets, &b.OverDurationSec, &b.MaxFailureRate,
            &b.MaxBatchSize, &b.MinEvaluated,
            &b.State, &b.HaltReason, &b.IssuedAt, &b.IssuedBy,
            &b.StartedAt, &b.PausedAt, &b.TotalPausedSec,
            &b.HaltedAt, &b.AbortedAt, &b.CompletedAt, &b.UpdatedAt); err != nil {
            return nil, setAPIErrors(err)
        }
        result = append(result, b)
    }
    return result, nil
}

func (r *Repository) UpdateBosunRolloutState(ctx context.Context, rolloutID uuid.UUID,
    state, haltReason string, setStartedAt, setHaltedAt, setAbortedAt, setCompletedAt bool) error {

    setClauses := []string{"state = $2", "updated_at = NOW()"}
    if haltReason != "" {
        setClauses = append(setClauses, "halt_reason = $3")
    }
    if setStartedAt {
        setClauses = append(setClauses, "started_at = COALESCE(started_at, NOW())")
    }
    if setHaltedAt {
        setClauses = append(setClauses, "halted_at = NOW()")
    }
    if setAbortedAt {
        setClauses = append(setClauses, "aborted_at = NOW()")
    }
    if setCompletedAt {
        setClauses = append(setClauses, "completed_at = NOW()")
    }
    q := fmt.Sprintf(`UPDATE bosun_rollouts SET %s WHERE rollout_id = $1`,
        joinComma(setClauses))
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)
    var err error
    if haltReason != "" {
        _, err = pool.Exec(ctx, q, rolloutID, state, haltReason)
    } else {
        _, err = pool.Exec(ctx, q, rolloutID, state)
    }
    return setAPIErrors(err)
}

func (r *Repository) UpdateBosunRolloutPause(ctx context.Context, rolloutID uuid.UUID, toPaused bool) error {
    var q string
    if toPaused {
        q = `UPDATE bosun_rollouts
                SET state = 'paused', paused_at = NOW(), updated_at = NOW()
              WHERE rollout_id = $1 AND state = 'running'`
    } else {
        q = `UPDATE bosun_rollouts
                SET state = 'running',
                    total_paused_sec = total_paused_sec
                      + EXTRACT(EPOCH FROM (NOW() - paused_at))::int,
                    paused_at = NULL,
                    updated_at = NOW()
              WHERE rollout_id = $1 AND state = 'paused'`
    }
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)
    tag, err := pool.Exec(ctx, q, rolloutID)
    if err != nil {
        return setAPIErrors(err)
    }
    if tag.RowsAffected() == 0 {
        return ErrNotFound
    }
    return nil
}

func joinComma(parts []string) string {
    s := ""
    for i, p := range parts {
        if i > 0 {
            s += ", "
        }
        s += p
    }
    return s
}
```

- [ ] **Step 3: Integration test (минимальный)**

`internal/repository/bosun_rollouts_test.go`:

```go
//go:build integration

package repository

import (
    "context"
    "encoding/json"
    "testing"

    "github.com/google/uuid"
    "github.com/stretchr/testify/assert"
    "github.com/stretchr/testify/require"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository/fixtures"
)

func TestBosunRollout_LifecycleTransitions(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)

    // публикуем bundle, чтобы FK работал
    _, err := repo.InsertBosunBundle(ctx, InsertBosunBundleIn{
        Blob: []byte("b"), SHA256: make([]byte, 32),
        Signature: []byte("s"), Tags: []string{}, SizeBytes: 1, PublishedBy: "ci",
    })
    require.NoError(t, err)

    rid := uuid.New()
    spec, _ := json.Marshal(map[string]any{"severity_class": "low"})
    snap, _ := json.Marshal([]string{"h1", "h2", "h3"})
    in := InsertBosunRolloutIn{
        RolloutID: rid, TargetVersion: 1,
        TargetSpec: spec, TargetSnapshot: snap, TotalTargets: 3,
        OverDurationSec: 1800, MaxFailureRate: 0.05,
        MaxBatchSize: 100, MinEvaluated: 20, IssuedBy: "op@example.com",
    }
    r, err := repo.InsertBosunRollout(ctx, in)
    require.NoError(t, err)
    assert.Equal(t, "pending", r.State)

    require.NoError(t, repo.UpdateBosunRolloutState(ctx, rid, "running", "", true, false, false, false))
    r, err = repo.GetBosunRollout(ctx, rid)
    require.NoError(t, err)
    assert.Equal(t, "running", r.State)
    assert.NotNil(t, r.StartedAt)

    require.NoError(t, repo.UpdateBosunRolloutPause(ctx, rid, true))
    r, _ = repo.GetBosunRollout(ctx, rid)
    assert.Equal(t, "paused", r.State)
    assert.NotNil(t, r.PausedAt)

    require.NoError(t, repo.UpdateBosunRolloutPause(ctx, rid, false))
    r, _ = repo.GetBosunRollout(ctx, rid)
    assert.Equal(t, "running", r.State)
    assert.Nil(t, r.PausedAt)
    assert.GreaterOrEqual(t, r.TotalPausedSec, 0)
}
```

- [ ] **Step 4: Запустить и коммит**

```bash
go test -tags integration ./internal/repository/... -run TestBosunRollout
git add internal/repository/types.go internal/repository/bosun_rollouts.go internal/repository/bosun_rollouts_test.go
git commit -m "feat(bosun): add bosun_rollouts repository"
```

---

## Task 6: Repository — bosun_pods, bosun_leader, bosun_audit

**Files:**
- Create: `internal/repository/bosun_pods.go`
- Create: `internal/repository/bosun_leader.go`
- Create: `internal/repository/bosun_audit.go`

- [ ] **Step 1: Manager extensions**

```go
type bosunPods interface {
    UpsertBosunPod(ctx context.Context, podID, addr string, draining bool) error
    ListActiveBosunPods(ctx context.Context, matureThreshold time.Duration) ([]BosunPod, error)
    MarkBosunPodDraining(ctx context.Context, podID string) error
}

type BosunPod struct {
    PodID      string
    Addr       string
    StartedAt  time.Time
    LastPingAt time.Time
    Draining   bool
}

type bosunLeader interface {
    TryAcquireBosunLeader(ctx context.Context, role, podID string,
        leaseSec int) (currentPodID string, epoch int64, acquired bool, err error)
    HeartbeatBosunLeader(ctx context.Context, role, podID string,
        epoch int64, leaseSec int) (kept bool, err error)
}

type bosunAudit interface {
    InsertBosunAudit(ctx context.Context, in InsertBosunAuditIn) error
}

type InsertBosunAuditIn struct {
    EventType string
    Actor     string
    ActorKind string  // 'operator' | 'ci' | 'system'
    RolloutID *uuid.UUID
    Host      *string
    Payload   []byte  // JSON
}
```

- [ ] **Step 2: bosun_pods.go**

```go
package repository

import (
    "context"
    "time"

    "gitlab.ozon.ru/platform/go/database-pg/roles"
)

func (r *Repository) UpsertBosunPod(ctx context.Context, podID, addr string, draining bool) error {
    const q = `
        INSERT INTO bosun_pods (pod_id, addr, started_at, last_ping_at, draining)
        VALUES ($1, $2, NOW(), NOW(), $3)
        ON CONFLICT (pod_id) DO UPDATE
           SET addr = EXCLUDED.addr,
               last_ping_at = NOW(),
               draining = EXCLUDED.draining`
    _, err := r.balancer.PickPool(ctx, roles.RoleMaster).Exec(ctx, q, podID, addr, draining)
    return setAPIErrors(err)
}

func (r *Repository) ListActiveBosunPods(ctx context.Context, matureThreshold time.Duration) ([]BosunPod, error) {
    const q = `
        SELECT pod_id, addr, started_at, last_ping_at, draining
          FROM bosun_pods
         WHERE last_ping_at > NOW() - INTERVAL '60 seconds'
           AND draining = false
           AND started_at < NOW() - $1::interval
         ORDER BY pod_id`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    rows, err := pool.Query(ctx, q, matureThreshold)
    if err != nil {
        return nil, setAPIErrors(err)
    }
    defer rows.Close()
    var pods []BosunPod
    for rows.Next() {
        var p BosunPod
        if err := rows.Scan(&p.PodID, &p.Addr, &p.StartedAt, &p.LastPingAt, &p.Draining); err != nil {
            return nil, setAPIErrors(err)
        }
        pods = append(pods, p)
    }
    return pods, nil
}

func (r *Repository) MarkBosunPodDraining(ctx context.Context, podID string) error {
    const q = `UPDATE bosun_pods SET draining = true WHERE pod_id = $1`
    _, err := r.balancer.PickPool(ctx, roles.RoleMaster).Exec(ctx, q, podID)
    return setAPIErrors(err)
}
```

- [ ] **Step 3: bosun_leader.go**

```go
package repository

import (
    "context"
    "fmt"
    "hash/fnv"

    "gitlab.ozon.ru/platform/go/database-pg/roles"
)

// TryAcquireBosunLeader — atomic try-acquire через pg_try_advisory_xact_lock.
// Возвращает (текущий pod_id, epoch, acquired, err).
// acquired == true только если self стал/остался лидером.
func (r *Repository) TryAcquireBosunLeader(ctx context.Context, role, podID string,
    leaseSec int) (string, int64, bool, error) {

    lockKey := int64(fnvHash("bosun_leader:" + role))
    pool := r.balancer.PickPool(ctx, roles.RoleMaster)

    tx, err := pool.Begin(ctx)
    if err != nil {
        return "", 0, false, setAPIErrors(err)
    }
    defer tx.Rollback(ctx)

    // advisory mutex для UPSERT'а
    var got bool
    if err := tx.QueryRow(ctx,
        `SELECT pg_try_advisory_xact_lock($1)`, lockKey).Scan(&got); err != nil {
        return "", 0, false, setAPIErrors(err)
    }
    if !got {
        return "", 0, false, nil
    }

    var currentPodID *string
    var currentEpoch *int64
    var expired bool
    err = tx.QueryRow(ctx, `
        SELECT pod_id, epoch, expires_at < NOW()
          FROM bosun_leader WHERE role = $1`, role).Scan(&currentPodID, &currentEpoch, &expired)
    if err != nil && err.Error() != "no rows in result set" {
        return "", 0, false, setAPIErrors(err)
    }

    newEpoch := int64(1)
    if currentEpoch != nil {
        if *currentPodID == podID {
            // продлеваем своё лидерство; epoch не меняется
            newEpoch = *currentEpoch
        } else if expired {
            newEpoch = *currentEpoch + 1
        } else {
            // чужой активный лидер
            if err := tx.Commit(ctx); err != nil {
                return "", 0, false, setAPIErrors(err)
            }
            return *currentPodID, *currentEpoch, false, nil
        }
    }

    const upsert = `
        INSERT INTO bosun_leader (role, pod_id, epoch, acquired_at, last_heartbeat, expires_at)
        VALUES ($1, $2, $3, NOW(), NOW(), NOW() + ($4 || ' seconds')::interval)
        ON CONFLICT (role) DO UPDATE
          SET pod_id          = EXCLUDED.pod_id,
              epoch           = EXCLUDED.epoch,
              acquired_at     = CASE WHEN bosun_leader.pod_id = EXCLUDED.pod_id
                                      THEN bosun_leader.acquired_at
                                      ELSE EXCLUDED.acquired_at END,
              last_heartbeat  = EXCLUDED.last_heartbeat,
              expires_at      = EXCLUDED.expires_at`
    if _, err := tx.Exec(ctx, upsert, role, podID, newEpoch, leaseSec); err != nil {
        return "", 0, false, setAPIErrors(err)
    }
    if err := tx.Commit(ctx); err != nil {
        return "", 0, false, setAPIErrors(err)
    }
    return podID, newEpoch, true, nil
}

// HeartbeatBosunLeader — продлевает lease только если pod ещё лидер по epoch.
func (r *Repository) HeartbeatBosunLeader(ctx context.Context, role, podID string,
    epoch int64, leaseSec int) (bool, error) {
    const q = `
        UPDATE bosun_leader
           SET last_heartbeat = NOW(),
               expires_at = NOW() + ($4 || ' seconds')::interval
         WHERE role = $1 AND pod_id = $2 AND epoch = $3`
    tag, err := r.balancer.PickPool(ctx, roles.RoleMaster).Exec(ctx, q, role, podID, epoch, leaseSec)
    if err != nil {
        return false, setAPIErrors(err)
    }
    return tag.RowsAffected() == 1, nil
}

// CheckLeaderEpoch — fencing check перед mutation batch'ем.
func (r *Repository) CheckLeaderEpoch(ctx context.Context, role string, epoch int64) (bool, error) {
    const q = `SELECT epoch FROM bosun_leader WHERE role = $1`
    pool := r.balancer.PickPool(ctx, roles.RoleReadFallbackWrite)
    var dbEpoch int64
    if err := pool.QueryRow(ctx, q, role).Scan(&dbEpoch); err != nil {
        return false, setAPIErrors(err)
    }
    return dbEpoch == epoch, nil
}

func fnvHash(s string) uint32 {
    h := fnv.New32a()
    _, _ = h.Write([]byte(s))
    return h.Sum32()
}

var _ = fmt.Sprintf
```

- [ ] **Step 4: bosun_audit.go**

```go
package repository

import (
    "context"

    "gitlab.ozon.ru/platform/go/database-pg/roles"
)

func (r *Repository) InsertBosunAudit(ctx context.Context, in InsertBosunAuditIn) error {
    const q = `
        INSERT INTO bosun_audit (event_type, actor, actor_kind, rollout_id, host, payload)
        VALUES ($1, $2, $3, $4, $5, $6)`
    _, err := r.balancer.PickPool(ctx, roles.RoleMaster).Exec(ctx, q,
        in.EventType, in.Actor, in.ActorKind, in.RolloutID, in.Host, in.Payload)
    return setAPIErrors(err)
}
```

- [ ] **Step 5: Запустить smoke**

Минимальный smoke (integration):

```go
func TestBosunPodAndAudit(t *testing.T) {
    ctx := context.Background()
    repo := fixtures.Prepare(t)

    require.NoError(t, repo.UpsertBosunPod(ctx, "pod-1", "10.0.0.1:9000", false))
    require.NoError(t, repo.InsertBosunAudit(ctx, InsertBosunAuditIn{
        EventType: "bosun.rollout.issue", Actor: "test", ActorKind: "operator",
        Payload: []byte(`{}`),
    }))
}
```

- [ ] **Step 6: Коммит**

```bash
git add internal/repository/types.go internal/repository/bosun_pods.go internal/repository/bosun_leader.go internal/repository/bosun_audit.go
git commit -m "feat(bosun): add bosun_pods/leader/audit repositories"
```

---

## Task 7: Bundle cache (LRU + singleflight)

**Files:**
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/bundle_cache.go`
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/bundle_cache_test.go`

- [ ] **Step 1: LRU + singleflight**

```go
package bosun

import (
    "context"
    "container/list"
    "sync"

    "golang.org/x/sync/singleflight"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

const bundleLRUCapacity = 5

// BundleCache: in-memory LRU 5 bundle blobs, плюс singleflight на
// одновременные cold-cache запросы.
type BundleCache struct {
    repo repository.Manager
    mu   sync.Mutex
    list *list.List
    idx  map[int64]*list.Element
    sf   singleflight.Group
}

type bundleEntry struct {
    version  int64
    blob     []byte
    manifest *repository.BosunBundleManifest
}

func NewBundleCache(repo repository.Manager) *BundleCache {
    return &BundleCache{
        repo: repo,
        list: list.New(),
        idx:  make(map[int64]*list.Element),
    }
}

func (c *BundleCache) GetManifest(ctx context.Context, version int64) (*repository.BosunBundleManifest, error) {
    if entry := c.peek(version); entry != nil {
        return entry.manifest, nil
    }
    // manifest read дёшев — без singleflight, но через repo
    return c.repo.GetBosunBundleManifest(ctx, version)
}

func (c *BundleCache) GetBlob(ctx context.Context, version int64) ([]byte, *repository.BosunBundleManifest, error) {
    if entry := c.peek(version); entry != nil {
        return entry.blob, entry.manifest, nil
    }
    res, err, _ := c.sf.Do(keyFor(version), func() (any, error) {
        blob, manifest, err := c.repo.GetBosunBundleBlob(ctx, version)
        if err != nil {
            return nil, err
        }
        c.put(version, blob, manifest)
        return []any{blob, manifest}, nil
    })
    if err != nil {
        return nil, nil, err
    }
    pair := res.([]any)
    return pair[0].([]byte), pair[1].(*repository.BosunBundleManifest), nil
}

func (c *BundleCache) peek(version int64) *bundleEntry {
    c.mu.Lock()
    defer c.mu.Unlock()
    if e, ok := c.idx[version]; ok {
        c.list.MoveToFront(e)
        return e.Value.(*bundleEntry)
    }
    return nil
}

func (c *BundleCache) put(version int64, blob []byte, m *repository.BosunBundleManifest) {
    c.mu.Lock()
    defer c.mu.Unlock()
    if e, ok := c.idx[version]; ok {
        c.list.MoveToFront(e)
        e.Value = &bundleEntry{version, blob, m}
        return
    }
    e := c.list.PushFront(&bundleEntry{version, blob, m})
    c.idx[version] = e
    for c.list.Len() > bundleLRUCapacity {
        back := c.list.Back()
        if back == nil {
            return
        }
        c.list.Remove(back)
        delete(c.idx, back.Value.(*bundleEntry).version)
    }
}

func keyFor(version int64) string {
    return "blob:" + intToString(version)
}

func intToString(v int64) string {
    // быстрая конверсия без fmt
    return strconv_itoa64(v)
}

// strconv_itoa64 — placeholder, использовать strconv.FormatInt(v, 10).
```

- [ ] **Step 2: Unit-тест LRU + singleflight (table-driven)**

```go
package bosun

import (
    "context"
    "sync"
    "sync/atomic"
    "testing"

    "github.com/stretchr/testify/assert"
    "github.com/stretchr/testify/require"
)

type fakeRepo struct {
    callCount int32
    blobs     map[int64][]byte
}

func (f *fakeRepo) GetBosunBundleBlob(ctx context.Context, v int64) ([]byte, *repository.BosunBundleManifest, error) {
    atomic.AddInt32(&f.callCount, 1)
    return f.blobs[v], &repository.BosunBundleManifest{Version: v}, nil
}
// ... stub других методов Manager interface через minimock или embed dummy

func TestBundleCache_LRU(t *testing.T) { /* eviction после 5 */ }
func TestBundleCache_Singleflight(t *testing.T) {
    // 10 goroutine одновременно запрашивают одну version → repo.Get вызывается 1 раз
}
```

(Для production реализации embedded `repository.Manager` через `minimock` или просто отдельный интерфейс `BundleStore`.)

- [ ] **Step 3: Запустить**

```bash
go test ./internal/app/ozon/infrastructure/cloudozon/bosun/...
```

- [ ] **Step 4: Коммит**

```bash
git commit -m "feat(bosun): add LRU+singleflight bundle cache"
```

---

## Task 8: Pod hashing (Jump Consistent Hash)

**Files:**
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/pod_hashing.go`
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/pod_hashing_test.go`

- [ ] **Step 1: Jump Consistent Hash**

```go
package bosun

import "hash/fnv"

// JumpHash — Lamping & Veach 2014, O(log N) compute, минимальный reshuffle при scale event.
func JumpHash(key uint64, numBuckets int32) int32 {
    if numBuckets <= 0 {
        return -1
    }
    var b, j int64 = -1, 0
    for j < int64(numBuckets) {
        b = j
        key = key*2862933555777941757 + 1
        j = int64(float64(b+1) * (float64(int64(1)<<31) / float64((key>>33)+1)))
    }
    return int32(b)
}

func HashKey(shardingKey string) uint64 {
    h := fnv.New64a()
    _, _ = h.Write([]byte(shardingKey))
    return h.Sum64()
}

func PodForHost(shardingKey string, podIDs []string) string {
    if len(podIDs) == 0 {
        return ""
    }
    idx := JumpHash(HashKey(shardingKey), int32(len(podIDs)))
    return podIDs[idx]
}
```

- [ ] **Step 2: Тесты**

```go
package bosun

import "testing"

func TestJumpHash_Stable(t *testing.T) {
    if JumpHash(123, 10) != JumpHash(123, 10) {
        t.Fail()
    }
}

func TestJumpHash_MinimalReshuffle(t *testing.T) {
    // 1000 keys, N=10 → assignments_a; N=11 → assignments_b
    // ожидаем что переехало ~1/11 ключей (отклонение ±2× допустимо)
    moved := 0
    for k := uint64(0); k < 1000; k++ {
        if JumpHash(k, 10) != JumpHash(k, 11) {
            moved++
        }
    }
    // примерно 1000/11 ≈ 90
    if moved < 50 || moved > 160 {
        t.Fatalf("Jump hash reshuffle aberrant: %d", moved)
    }
}

func TestPodForHost(t *testing.T) {
    pods := []string{"p1", "p2", "p3"}
    got := PodForHost("cluster-a.host-1", pods)
    if got == "" {
        t.Fail()
    }
}
```

- [ ] **Step 3: Запустить и коммит**

```bash
go test ./internal/app/ozon/infrastructure/cloudozon/bosun/... -run Jump
git commit -m "feat(bosun): jump consistent hashing for pod redirect"
```

---

## Task 9: Service skeleton (service.go, utils.go, audit.go, metrics.go)

**Files:**
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/service.go`
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/utils.go`
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/audit.go`
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/metrics.go`

- [ ] **Step 1: service.go**

```go
package bosun

import (
    "context"

    "gitlab.ozon.ru/platform/scratch"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/config"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/keycloak"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/validator"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/vault"
)

type Implementation struct {
    desc.UnimplementedBosunAPIServer
    repo       repository.Manager
    config     *config.Getter
    vaultCache vault.Cache
    validator  validator.Validator
    keycloak   keycloak.Client

    podID       string
    bundleCache *BundleCache

    // Subscribe stream registry (если feature on)
    subscribers *SubscriberRegistry

    // rollout worker и leader
    rolloutLeader *RolloutLeader
}

func NewBosunAPI(ctx context.Context, app *scratch.App) *Implementation {
    cfg, err := config.GetConfig(ctx, app)
    if err != nil {
        panic("bosun: config: " + err.Error())
    }
    balancer, err := repository.NewBalancer(ctx, cfg)
    if err != nil {
        panic("bosun: balancer: " + err.Error())
    }
    repo, err := repository.New(ctx, balancer, cfg)
    if err != nil {
        panic("bosun: repo: " + err.Error())
    }
    vc, err := vault.NewCommonCache(ctx, cfg)
    if err != nil {
        panic("bosun: vault: " + err.Error())
    }
    val, err := validator.New(ctx, repo, cfg)
    if err != nil {
        panic("bosun: validator: " + err.Error())
    }
    kc, err := keycloak.New(cfg)
    if err != nil {
        panic("bosun: keycloak: " + err.Error())
    }

    podID := cfg.GetPodID()
    impl := &Implementation{
        repo:        repo,
        config:      cfg,
        vaultCache:  vc,
        validator:   val,
        keycloak:    kc,
        podID:       podID,
        bundleCache: NewBundleCache(repo),
        subscribers: NewSubscriberRegistry(),
    }

    // регистрация pod'а в pod registry (для consistent hashing)
    if err := repo.UpsertBosunPod(ctx, podID, cfg.GetSelfAddr(), false); err != nil {
        // не fatal — pod зарегистрируется через heartbeat
    }

    // background: pod heartbeat
    go impl.runPodHeartbeat(ctx)

    // background: rollout worker leader election + loop
    impl.rolloutLeader = NewRolloutLeader(repo, podID)
    go impl.rolloutLeader.Run(ctx)
    go impl.runRolloutWorker(ctx)

    return impl
}

func (i *Implementation) GetDescription() scratch.ServiceDesc {
    return desc.NewBosunAPIServiceDesc(i)
}
```

- [ ] **Step 2: utils.go**

```go
package bosun

import (
    "context"
    "fmt"

    "google.golang.org/grpc/codes"
    "google.golang.org/grpc/status"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/config"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

// agentAuth — проверка ECDSA-подписи как в chiit.
func (i *Implementation) agentAuth(ctx context.Context, host, createdAt string, sign []byte) error {
    if host == "" {
        return status.Error(codes.InvalidArgument, "host required")
    }
    if err := i.validator.Validate(ctx, host, createdAt, sign); err != nil {
        return status.Error(codes.PermissionDenied, "auth failed: "+err.Error())
    }
    return nil
}

// ciAuth — проверка admin-token для CI service-account ручек.
func (i *Implementation) ciAuth(ctx context.Context) (actor string, err error) {
    actor, err = i.checkAdminTokenFromContext(ctx)
    if err != nil {
        return "", status.Error(codes.PermissionDenied, err.Error())
    }
    return actor, nil
}

// operatorAuth — admin-token ИЛИ Keycloak JWT (bearer-token).
// Возвращает (actor, actorKind, err).
func (i *Implementation) operatorAuth(ctx context.Context) (string, string, error) {
    if actor, err := i.checkAdminTokenFromContext(ctx); err == nil {
        return actor, "operator", nil
    }
    if actor, err := i.checkBearerTokenFromContext(ctx); err == nil {
        return actor, "operator", nil
    }
    return "", "", status.Error(codes.PermissionDenied, "operator auth required")
}

// checkAdminTokenFromContext / checkBearerTokenFromContext —
// аналоги chiit-сервера. Можно реиспользовать пакет если он экспортирован,
// иначе скопируй логику.

func (i *Implementation) checkAdminTokenFromContext(ctx context.Context) (string, error) {
    token := getHeaderFromContext(ctx, config.AdminTokenHeader)
    if token == "" {
        return "", fmt.Errorf("no admin token")
    }
    for _, allowed := range i.config.AdminAPIKeys() {
        if token == allowed {
            return "admin-token", nil
        }
    }
    return "", fmt.Errorf("invalid admin token")
}

func (i *Implementation) checkBearerTokenFromContext(ctx context.Context) (string, error) {
    token := getHeaderFromContext(ctx, config.BearerTokenHeader)
    if token == "" {
        return "", fmt.Errorf("no bearer")
    }
    claims, err := i.keycloak.Validate(ctx, token)
    if err != nil {
        return "", err
    }
    return claims.Username, nil
}

// convertError — копия из chiit helper'а.
func convertError(err error) error {
    if err == nil {
        return nil
    }
    switch err {
    case repository.ErrNotFound:
        return status.Error(codes.NotFound, "not found")
    case repository.ErrAlreadyExists:
        return status.Error(codes.AlreadyExists, "already exists")
    default:
        return status.Error(codes.Internal, err.Error())
    }
}

// getHeaderFromContext: skopiruj iz chiit-server/internal/app/.../utils.go.
func getHeaderFromContext(ctx context.Context, headerKey string) string {
    // см. chiit utils.go (metadata + ctx.Value)
    return ""
}
```

- [ ] **Step 3: audit.go**

```go
package bosun

import (
    "context"
    "encoding/json"

    "github.com/google/uuid"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

func (i *Implementation) audit(ctx context.Context, eventType, actor, actorKind string,
    rolloutID *uuid.UUID, host *string, payload any) error {
    p, _ := json.Marshal(payload)
    return i.repo.InsertBosunAudit(ctx, repository.InsertBosunAuditIn{
        EventType: eventType,
        Actor:     actor,
        ActorKind: actorKind,
        RolloutID: rolloutID,
        Host:      host,
        Payload:   p,
    })
}
```

- [ ] **Step 4: metrics.go**

```go
package bosun

import (
    "github.com/prometheus/client_golang/prometheus"
    "github.com/prometheus/client_golang/prometheus/promauto"
)

var (
    metricGetTargetVersionTotal = promauto.NewCounterVec(
        prometheus.CounterOpts{Name: "bosun_get_target_version_requests_total"},
        []string{"result"})
    metricGetTargetVersionDBWrite = promauto.NewCounter(
        prometheus.CounterOpts{Name: "bosun_get_target_version_db_write_total"})
    metricReportApplyResult = promauto.NewCounterVec(
        prometheus.CounterOpts{Name: "bosun_report_apply_result_total"},
        []string{"success"})
    metricRolloutState = promauto.NewGaugeVec(
        prometheus.GaugeOpts{Name: "bosun_rollout_state"},
        []string{"rollout_id", "state"})
    metricRolloutFailureRate = promauto.NewGaugeVec(
        prometheus.GaugeOpts{Name: "bosun_rollout_failure_rate"},
        []string{"rollout_id"})
    metricRolloutDispatched = promauto.NewCounterVec(
        prometheus.CounterOpts{Name: "bosun_rollout_dispatched_total"},
        []string{"rollout_id"})
    metricBundleCacheHit = promauto.NewCounterVec(
        prometheus.CounterOpts{Name: "bosun_bundle_cache_hit_total"},
        []string{"result"})
    metricBundleDownloadBytes = promauto.NewCounter(
        prometheus.CounterOpts{Name: "bosun_bundle_download_bytes_total"})
    metricActiveStreams = promauto.NewGauge(
        prometheus.GaugeOpts{Name: "bosun_active_streams"})
    metricPodRedirect = promauto.NewCounter(
        prometheus.CounterOpts{Name: "bosun_pod_redirect_total"})
    metricLeaderCurrent = promauto.NewGaugeVec(
        prometheus.GaugeOpts{Name: "bosun_leader_current"},
        []string{"role", "pod_id"})
    metricAuthFailures = promauto.NewCounterVec(
        prometheus.CounterOpts{Name: "bosun_auth_failures_total"},
        []string{"reason"})
    metricPublishBundle = promauto.NewCounterVec(
        prometheus.CounterOpts{Name: "bosun_publish_bundle_total"},
        []string{"result"})
)
```

- [ ] **Step 5: Pod heartbeat goroutine**

В service.go добавь:

```go
func (i *Implementation) runPodHeartbeat(ctx context.Context) {
    ticker := time.NewTicker(30 * time.Second)
    defer ticker.Stop()
    for {
        select {
        case <-ctx.Done():
            _ = i.repo.MarkBosunPodDraining(ctx, i.podID)
            return
        case <-ticker.C:
            _ = i.repo.UpsertBosunPod(ctx, i.podID, i.config.GetSelfAddr(), false)
        }
    }
}
```

- [ ] **Step 6: Коммит**

```bash
git add internal/app/ozon/infrastructure/cloudozon/bosun/
git commit -m "feat(bosun): service skeleton + auth + metrics + audit helpers"
```

---

## Task 10: Bootstrap handler

**File:** `internal/app/ozon/infrastructure/cloudozon/bosun/bootstrap.go`

- [ ] **Step 1**

```go
package bosun

import (
    "context"
    "fmt"

    "google.golang.org/grpc/codes"
    "google.golang.org/grpc/status"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/config"
)

func (i *Implementation) Bootstrap(ctx context.Context, req *desc.BootstrapIn) (*desc.BootstrapOut, error) {
    if req.Host == "" || len(req.ClientPublicKey) == 0 {
        return nil, status.Error(codes.InvalidArgument, "host and client_public_key required")
    }
    // Сверяем registration_token (как CreateClient в chiit).
    expected := config.HashRegistrationSecret(req.Host, i.config.ClientRegistrationSecret())
    if req.RegistrationToken != expected {
        _ = i.audit(ctx, "bosun.bootstrap.denied", req.Host, "system",
            nil, &req.Host, map[string]any{"reason": "invalid_token"})
        return nil, status.Error(codes.PermissionDenied, "invalid registration token")
    }

    // INSERT в chiit_validators (общая таблица, как в chiit).
    if err := i.repo.CreateClient(ctx, req.Host, req.ClientPublicKey); err != nil {
        return nil, convertError(err)
    }

    // Lazy INSERT в bosun_clients (но не обязательно — первый GetTargetVersion сделает).
    _ = i.repo.UpsertBosunClientLazy(ctx, req.Host, req.BosunVersion)

    // Сертификат — из existing chiit certificate flow (NewCertForHost или подобное).
    cert, chain, err := i.issueCertForHost(ctx, req.Host, req.ClientPublicKey)
    if err != nil {
        return nil, convertError(err)
    }

    _ = i.audit(ctx, "bosun.bootstrap.success", "bootstrap", "system",
        nil, &req.Host, map[string]any{})

    return &desc.BootstrapOut{
        ClientCertificate: cert,
        CaChain:           chain,
    }, nil
}

// issueCertForHost — реиспользуй internal/cert_manager или эквивалент.
func (i *Implementation) issueCertForHost(ctx context.Context, host string, pubKey []byte) ([]byte, []byte, error) {
    // TODO P0: вызвать chiit-server'ную логику cert issue
    return nil, nil, fmt.Errorf("issueCertForHost: not yet wired")
}
```

- [ ] **Step 2: Коммит**

```bash
git add internal/app/ozon/infrastructure/cloudozon/bosun/bootstrap.go
git commit -m "feat(bosun): Bootstrap handler"
```

---

## Task 11: GetTargetVersion + lazy insert + pod redirect

**File:** `internal/app/ozon/infrastructure/cloudozon/bosun/get_target_version.go`

- [ ] **Step 1**

```go
package bosun

import (
    "context"
    "math/rand"
    "time"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

const podMatureThreshold = 60 * time.Second
const defaultPullJitter = uint32(15)

func (i *Implementation) GetTargetVersion(ctx context.Context, req *desc.GetTargetVersionIn) (*desc.GetTargetVersionOut, error) {
    if err := i.agentAuth(ctx, req.Host, req.CreatedAt, req.Sign); err != nil {
        metricAuthFailures.WithLabelValues("ecdsa").Inc()
        return nil, err
    }
    metricGetTargetVersionTotal.WithLabelValues("ok").Inc()

    // pod redirect: проверить, не должен ли клиент идти на другой pod.
    pods, _ := i.repo.ListActiveBosunPods(ctx, podMatureThreshold)
    if len(pods) > 1 {
        podIDs := make([]string, 0, len(pods))
        for _, p := range pods {
            podIDs = append(podIDs, p.PodID)
        }
        targetPod := PodForHost(req.ShardingKey, podIDs)
        if targetPod != "" && targetPod != i.podID {
            for _, p := range pods {
                if p.PodID == targetPod {
                    metricPodRedirect.Inc()
                    return &desc.GetTargetVersionOut{
                        PodRedirect:  true,
                        RedirectAddr: p.Addr,
                    }, nil
                }
            }
        }
    }

    // lazy insert для legacy-нод
    _ = i.repo.UpsertBosunClientLazy(ctx, req.Host, req.BosunVersion)

    // diff-update last_seen_at
    if err := i.repo.UpdateBosunClientLastSeen(ctx, req.Host, req.BosunVersion); err == nil {
        metricGetTargetVersionDBWrite.Inc()
    }

    c, err := i.repo.GetBosunClient(ctx, req.Host)
    if err != nil {
        return nil, convertError(err)
    }

    out := &desc.GetTargetVersionOut{
        PullJitterSeconds: defaultPullJitter,
    }
    if c.TargetVersion != nil {
        out.TargetVersion = uint64(*c.TargetVersion)
    }
    if c.TargetRolloutID != nil {
        out.TargetRolloutId = c.TargetRolloutID.String()
    }
    if rand.Intn(100) < 5 {
        // редкий debug-jitter для разноса синхронной волны
        out.PullJitterSeconds = defaultPullJitter + uint32(rand.Intn(15))
    }
    return out, nil
}
```

- [ ] **Step 2: Коммит**

---

## Task 12: ReportApplyResult

**File:** `internal/app/ozon/infrastructure/cloudozon/bosun/report_apply_result.go`

- [ ] **Step 1**

```go
package bosun

import (
    "context"
    "strconv"

    "google.golang.org/grpc/codes"
    "google.golang.org/grpc/status"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

func (i *Implementation) ReportApplyResult(ctx context.Context, req *desc.ReportApplyResultIn) (*desc.ReportApplyResultOut, error) {
    if err := i.agentAuth(ctx, req.Host, req.CreatedAt, req.Sign); err != nil {
        return nil, err
    }
    if req.Outcome == nil || req.Outcome.AppliedVersion == 0 {
        return nil, status.Error(codes.InvalidArgument, "outcome.applied_version required")
    }

    in := repository.UpdateBosunApplyResultIn{
        Host: req.Host,
        Outcome: repository.ApplyOutcome{
            AppliedVersion: int64(req.Outcome.AppliedVersion),
            Success:        req.Outcome.Success,
            ExitCode:       req.Outcome.ExitCode,
            ErrorExcerpt:   truncate(req.Outcome.ErrorExcerpt, 4096),
        },
        BosunVersion: req.BosunVersion,
    }
    if err := i.repo.UpdateBosunClientApplyResult(ctx, in); err != nil {
        if err == repository.ErrNotFound {
            // lazy create на всякий случай
            _ = i.repo.UpsertBosunClientLazy(ctx, req.Host, req.BosunVersion)
            _ = i.repo.UpdateBosunClientApplyResult(ctx, in)
        } else {
            return nil, convertError(err)
        }
    }

    metricReportApplyResult.WithLabelValues(strconv.FormatBool(req.Outcome.Success)).Inc()
    return &desc.ReportApplyResultOut{}, nil
}

func truncate(s string, n int) string {
    if len(s) <= n {
        return s
    }
    return s[:n]
}
```

- [ ] **Step 2: Коммит**

---

## Task 13: GetBundleManifest + GetBundleBlob

**Files:**
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/get_bundle_manifest.go`
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/get_bundle_blob.go`

- [ ] **Step 1: GetBundleManifest**

```go
package bosun

import (
    "context"

    "google.golang.org/protobuf/types/known/timestamppb"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

func (i *Implementation) GetBundleManifest(ctx context.Context, req *desc.GetBundleManifestIn) (*desc.GetBundleManifestOut, error) {
    if err := i.agentAuth(ctx, req.Host, req.CreatedAt, req.Sign); err != nil {
        return nil, err
    }
    m, err := i.bundleCache.GetManifest(ctx, int64(req.Version))
    if err != nil {
        return nil, convertError(err)
    }
    return &desc.GetBundleManifestOut{
        Version:     uint64(m.Version),
        Sha256:      m.SHA256,
        Signature:   m.Signature,
        SizeBytes:   m.SizeBytes,
        PublishedAt: timestamppb.New(m.PublishedAt),
    }, nil
}
```

- [ ] **Step 2: GetBundleBlob (streaming)**

```go
package bosun

import (
    "google.golang.org/grpc/codes"
    "google.golang.org/grpc/status"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

const chunkSize = 64 * 1024  // 64 KiB

func (i *Implementation) GetBundleBlob(req *desc.GetBundleBlobIn, stream desc.BosunAPI_GetBundleBlobServer) error {
    ctx := stream.Context()
    if err := i.agentAuth(ctx, req.Host, req.CreatedAt, req.Sign); err != nil {
        return err
    }
    blob, _, err := i.bundleCache.GetBlob(ctx, int64(req.Version))
    if err != nil {
        return convertError(err)
    }
    if blob == nil {
        return status.Error(codes.NotFound, "bundle blob not found")
    }
    metricBundleCacheHit.WithLabelValues("hit").Inc()  // (loose, can split miss-path)
    metricBundleDownloadBytes.Add(float64(len(blob)))

    for off := 0; off < len(blob); off += chunkSize {
        end := off + chunkSize
        if end > len(blob) {
            end = len(blob)
        }
        if err := stream.Send(&desc.BundleChunk{Data: blob[off:end]}); err != nil {
            return err
        }
    }
    return nil
}
```

- [ ] **Step 3: Коммит**

---

## Task 14: Subscribe stream (feature-flag)

**File:** `internal/app/ozon/infrastructure/cloudozon/bosun/subscribe.go`

- [ ] **Step 1: Registry + handler**

```go
package bosun

import (
    "sync"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

type SubscriberRegistry struct {
    mu  sync.Mutex
    chs map[string]chan *desc.Kick
}

func NewSubscriberRegistry() *SubscriberRegistry {
    return &SubscriberRegistry{chs: make(map[string]chan *desc.Kick)}
}

func (s *SubscriberRegistry) Add(host string) chan *desc.Kick {
    s.mu.Lock()
    defer s.mu.Unlock()
    if old, ok := s.chs[host]; ok {
        close(old)
    }
    ch := make(chan *desc.Kick, 1)
    s.chs[host] = ch
    metricActiveStreams.Set(float64(len(s.chs)))
    return ch
}

func (s *SubscriberRegistry) Remove(host string, ch chan *desc.Kick) {
    s.mu.Lock()
    defer s.mu.Unlock()
    if cur, ok := s.chs[host]; ok && cur == ch {
        delete(s.chs, host)
    }
    metricActiveStreams.Set(float64(len(s.chs)))
}

func (s *SubscriberRegistry) Kick(host string, k *desc.Kick) bool {
    s.mu.Lock()
    defer s.mu.Unlock()
    ch, ok := s.chs[host]
    if !ok {
        return false
    }
    select {
    case ch <- k:
        return true
    default:
        return false  // coalesce: уже есть pending Kick
    }
}

func (i *Implementation) Subscribe(req *desc.SubscribeIn, stream desc.BosunAPI_SubscribeServer) error {
    if !i.config.BosunSubscribeEnabled() {
        // feature-flag off — закрываем сразу
        return nil
    }
    ctx := stream.Context()
    if err := i.agentAuth(ctx, req.Host, req.CreatedAt, req.Sign); err != nil {
        return err
    }
    ch := i.subscribers.Add(req.Host)
    defer i.subscribers.Remove(req.Host, ch)

    for {
        select {
        case <-ctx.Done():
            return nil
        case kick, ok := <-ch:
            if !ok {
                return nil
            }
            if err := stream.Send(kick); err != nil {
                return err
            }
        }
    }
}
```

- [ ] **Step 2: Коммит**

---

## Task 15: Proxy handlers (VaultGet, GetCert, StorageHostInventoryGet)

- [ ] **Step 1: vault_get.go**

```go
package bosun

import (
    "context"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
    chiit_desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server"
)

func (i *Implementation) VaultGet(ctx context.Context, req *desc.VaultGetIn) (*chiit_desc.VaultOut, error) {
    if err := i.agentAuth(ctx, req.Host, req.CreatedAt, req.Sign); err != nil {
        return nil, err
    }
    val, err := i.vaultCache.Get(ctx, req.Path)
    if err != nil {
        return nil, convertError(err)
    }
    return &chiit_desc.VaultOut{Value: val}, nil
}
```

Аналогично get_cert.go и storage_host_inventory_get.go — реиспользуют chiit-server'ные cert_manager и inventory proxy.

- [ ] **Step 2: Коммит**

---

## Task 16: PublishBundle

**File:** `internal/app/ozon/infrastructure/cloudozon/bosun/publish_bundle.go`

- [ ] **Step 1**

```go
package bosun

import (
    "context"
    "crypto/sha256"
    "encoding/hex"

    "google.golang.org/grpc/codes"
    "google.golang.org/grpc/status"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

const maxBundleSize = 5 * 1024 * 1024  // 5 MiB

func (i *Implementation) PublishBundle(ctx context.Context, req *desc.PublishBundleIn) (*desc.PublishBundleOut, error) {
    actor, err := i.ciAuth(ctx)
    if err != nil {
        return nil, err
    }
    if len(req.Blob) == 0 {
        return nil, status.Error(codes.InvalidArgument, "blob required")
    }
    if len(req.Blob) > maxBundleSize {
        metricPublishBundle.WithLabelValues("too_large").Inc()
        return nil, status.Error(codes.InvalidArgument, "bundle exceeds 5 MiB")
    }
    actualHash := sha256.Sum256(req.Blob)
    if !equalBytes(actualHash[:], req.Sha256) {
        metricPublishBundle.WithLabelValues("sha_mismatch").Inc()
        return nil, status.Errorf(codes.InvalidArgument,
            "sha256 mismatch: got %s, expected %s",
            hex.EncodeToString(actualHash[:]), hex.EncodeToString(req.Sha256))
    }
    // TODO: signature verification против CI public key.

    version, err := i.repo.InsertBosunBundle(ctx, repository.InsertBosunBundleIn{
        Blob: req.Blob, SHA256: req.Sha256, Signature: req.Signature,
        Tags: req.Tags, SizeBytes: int64(len(req.Blob)), PublishedBy: actor,
    })
    if err != nil {
        return nil, convertError(err)
    }
    _ = i.audit(ctx, "bosun.bundle.publish", actor, "ci", nil, nil,
        map[string]any{"version": version, "size_bytes": len(req.Blob), "tags": req.Tags})
    metricPublishBundle.WithLabelValues("ok").Inc()
    return &desc.PublishBundleOut{Version: uint64(version)}, nil
}

func equalBytes(a, b []byte) bool {
    if len(a) != len(b) {
        return false
    }
    for i := range a {
        if a[i] != b[i] {
            return false
        }
    }
    return true
}
```

- [ ] **Step 2: Коммит**

---

## Task 17: Operator: SetTargetVersion + CountByVersion + GetClientState

- [ ] **Step 1: set_target_version.go**

```go
package bosun

import (
    "context"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

func (i *Implementation) SetTargetVersion(ctx context.Context, req *desc.SetTargetVersionIn) (*desc.SetTargetVersionOut, error) {
    actor, kind, err := i.operatorAuth(ctx)
    if err != nil {
        return nil, err
    }
    hosts, err := i.expandTarget(ctx, req.Target)
    if err != nil {
        return nil, convertError(err)
    }
    n, err := i.repo.SetBosunClientTarget(ctx, hosts, int64(req.TargetVersion), nil, true)
    if err != nil {
        return nil, convertError(err)
    }
    _ = i.audit(ctx, "bosun.target.override", actor, kind, nil, nil,
        map[string]any{"hosts": hosts, "target_version": req.TargetVersion, "reason": req.Reason})
    return &desc.SetTargetVersionOut{HostsUpdated: uint32(n)}, nil
}

// expandTarget — реиспользует repository.expandBosunTarget.
func (i *Implementation) expandTarget(ctx context.Context, t *desc.Target) ([]string, error) {
    // Преобразовать desc.Target → repository.BosunTargetFilter, вызвать repository.
    // Пока что P0: только hosts.
    return t.GetHosts(), nil
}
```

- [ ] **Step 2: count_by_version.go**

```go
package bosun

import (
    "context"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

func (i *Implementation) CountByVersion(ctx context.Context, req *desc.CountByVersionIn) (*desc.CountByVersionOut, error) {
    if _, _, err := i.operatorAuth(ctx); err != nil {
        return nil, err
    }
    filter, err := i.targetToFilter(req.Target)
    if err != nil {
        return nil, err
    }
    counts, total, never, err := i.repo.CountBosunByCurrentVersion(ctx, filter)
    if err != nil {
        return nil, convertError(err)
    }
    out := &desc.CountByVersionOut{
        CountByCurrentVersion: make(map[uint64]uint32, len(counts)),
        Total:        uint32(total),
        NeverApplied: uint32(never),
    }
    for v, n := range counts {
        out.CountByCurrentVersion[uint64(v)] = uint32(n)
    }
    return out, nil
}
```

- [ ] **Step 3: get_client_state.go**

```go
package bosun

import (
    "context"

    "google.golang.org/protobuf/types/known/timestamppb"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

func (i *Implementation) GetClientState(ctx context.Context, req *desc.GetClientStateIn) (*desc.GetClientStateOut, error) {
    if _, _, err := i.operatorAuth(ctx); err != nil {
        return nil, err
    }
    c, err := i.repo.GetBosunClient(ctx, req.Host)
    if err != nil {
        return nil, convertError(err)
    }
    out := &desc.GetClientStateOut{
        Host: c.Host,
        LastSeenAt: timestamppb.New(c.LastSeenAt),
        BosunVersion: c.BosunVersion,
    }
    if c.TargetVersion != nil {
        out.TargetVersion = uint64(*c.TargetVersion)
    }
    if c.TargetRolloutID != nil {
        out.TargetRolloutId = c.TargetRolloutID.String()
    }
    if c.CurrentVersion != nil {
        out.CurrentVersion = uint64(*c.CurrentVersion)
    }
    if c.LastAttemptedVersion != nil {
        out.LastAttemptedVersion = uint64(*c.LastAttemptedVersion)
    }
    if c.LastAttemptSuccess != nil {
        out.LastAttemptSuccess = *c.LastAttemptSuccess
    }
    if c.LastAttemptAt != nil {
        out.LastAttemptAt = timestamppb.New(*c.LastAttemptAt)
    }
    if c.LastAttemptExitCode != nil {
        out.LastAttemptExitCode = *c.LastAttemptExitCode
    }
    if c.LastAttemptError != nil {
        out.LastAttemptError = *c.LastAttemptError
    }
    return out, nil
}
```

- [ ] **Step 4: Коммит**

---

## Task 18: Rollout math (pure + tests)

**Files:**
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/rollout_math.go`
- Create: `internal/app/ozon/infrastructure/cloudozon/bosun/rollout_math_test.go`

- [ ] **Step 1: Pure functions**

```go
package bosun

import "time"

// ElapsedActive — активное время с момента старта rollout'а, без пауз.
// now := clock.Now()
func ElapsedActive(startedAt time.Time, totalPausedSec int, pausedAt *time.Time, now time.Time) time.Duration {
    if startedAt.IsZero() {
        return 0
    }
    base := now.Sub(startedAt) - time.Duration(totalPausedSec)*time.Second
    if pausedAt != nil {
        base -= now.Sub(*pausedAt)
    }
    if base < 0 {
        return 0
    }
    return base
}

// ExpectedDispatched — сколько hosts должно быть dispatched к этому моменту.
// total * elapsed / over.
func ExpectedDispatched(totalTargets, overDurationSec int, elapsed time.Duration) int {
    if overDurationSec == 0 {
        return totalTargets
    }
    e := int(elapsed.Seconds())
    if e >= overDurationSec {
        return totalTargets
    }
    return totalTargets * e / overDurationSec
}

// BatchSize — сколько dispatch'нуть на этом тике, с учётом cap'а.
func BatchSize(expected, dispatched, maxBatch int) int {
    diff := expected - dispatched
    if diff <= 0 {
        return 0
    }
    if diff > maxBatch {
        return maxBatch
    }
    return diff
}

// FailureRate — fraction failed из evaluated.
func FailureRate(succeeded, failed int) float32 {
    evaluated := succeeded + failed
    if evaluated == 0 {
        return 0
    }
    return float32(failed) / float32(evaluated)
}

// ShouldHalt — стоит ли остановить rollout по failure rate.
func ShouldHalt(succeeded, failed, minEvaluated int, maxRate float32) bool {
    evaluated := succeeded + failed
    if evaluated < minEvaluated {
        return false
    }
    return FailureRate(succeeded, failed) > maxRate
}

// IsCompleted — succeeded+failed достигли total.
func IsCompleted(succeeded, failed, total int) bool {
    return succeeded+failed >= total
}
```

- [ ] **Step 2: Unit-тесты**

```go
package bosun

import (
    "testing"
    "time"

    "github.com/stretchr/testify/assert"
)

func TestElapsedActive(t *testing.T) {
    now := time.Date(2026, 5, 24, 12, 0, 0, 0, time.UTC)
    started := now.Add(-1 * time.Hour)

    // без паузы
    e := ElapsedActive(started, 0, nil, now)
    assert.Equal(t, time.Hour, e)

    // прошлые паузы накоплены в total_paused_sec
    e = ElapsedActive(started, 600, nil, now)
    assert.Equal(t, 50*time.Minute, e)

    // сейчас на паузе
    pausedAt := now.Add(-10 * time.Minute)
    e = ElapsedActive(started, 0, &pausedAt, now)
    assert.Equal(t, 50*time.Minute, e)
}

func TestExpectedDispatched(t *testing.T) {
    tests := []struct {
        name   string
        total  int
        over   int
        elapsed time.Duration
        want   int
    }{
        {"half way", 100, 1000, 500 * time.Second, 50},
        {"finished", 100, 1000, 2000 * time.Second, 100},
        {"start", 100, 1000, 0, 0},
    }
    for _, tt := range tests {
        t.Run(tt.name, func(t *testing.T) {
            assert.Equal(t, tt.want, ExpectedDispatched(tt.total, tt.over, tt.elapsed))
        })
    }
}

func TestBatchSize(t *testing.T) {
    assert.Equal(t, 0, BatchSize(50, 50, 100))
    assert.Equal(t, 30, BatchSize(80, 50, 100))
    assert.Equal(t, 100, BatchSize(200, 50, 100))
}

func TestFailureRate(t *testing.T) {
    assert.InDelta(t, 0.0, FailureRate(0, 0), 0.001)
    assert.InDelta(t, 0.1, FailureRate(90, 10), 0.001)
}

func TestShouldHalt(t *testing.T) {
    // мало evaluated — не halt
    assert.False(t, ShouldHalt(5, 5, 20, 0.05))
    // достаточно evaluated, failure rate в пределах
    assert.False(t, ShouldHalt(90, 5, 20, 0.06))
    // достаточно evaluated, превышен порог
    assert.True(t, ShouldHalt(90, 10, 20, 0.05))
}

func TestIsCompleted(t *testing.T) {
    assert.True(t, IsCompleted(50, 50, 100))
    assert.False(t, IsCompleted(50, 49, 100))
}
```

- [ ] **Step 3: Запустить**

```bash
go test ./internal/app/ozon/infrastructure/cloudozon/bosun/... -run "TestElapsed|TestExpected|TestBatch|TestFailure|TestShould|TestIsCompleted"
```

Ожидается: 6/6 PASS.

- [ ] **Step 4: Коммит**

```bash
git add internal/app/ozon/infrastructure/cloudozon/bosun/rollout_math*.go
git commit -m "feat(bosun): pure rollout math + unit tests"
```

---

## Task 19: (объединено с Task 8 — Jump hash)

Уже сделано в Task 8.

---

## Task 20: Rollout leader

**File:** `internal/app/ozon/infrastructure/cloudozon/bosun/rollout_leader.go`

- [ ] **Step 1**

```go
package bosun

import (
    "context"
    "sync"
    "time"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

const (
    leaderRole       = "rollout_worker"
    leaderLeaseSec   = 30
    leaderTickPeriod = 10 * time.Second
)

type RolloutLeader struct {
    repo  repository.Manager
    podID string

    mu    sync.RWMutex
    isLeader bool
    epoch    int64
}

func NewRolloutLeader(repo repository.Manager, podID string) *RolloutLeader {
    return &RolloutLeader{repo: repo, podID: podID}
}

func (l *RolloutLeader) Run(ctx context.Context) {
    ticker := time.NewTicker(leaderTickPeriod)
    defer ticker.Stop()
    for {
        select {
        case <-ctx.Done():
            return
        case <-ticker.C:
            l.tick(ctx)
        }
    }
}

func (l *RolloutLeader) tick(ctx context.Context) {
    if !l.IsLeader() {
        currentPod, epoch, acquired, err := l.repo.TryAcquireBosunLeader(ctx,
            leaderRole, l.podID, leaderLeaseSec)
        if err != nil {
            return
        }
        if acquired {
            l.setLeader(true, epoch)
            metricLeaderCurrent.WithLabelValues(leaderRole, l.podID).Set(1)
        } else {
            metricLeaderCurrent.WithLabelValues(leaderRole, currentPod).Set(1)
            metricLeaderCurrent.WithLabelValues(leaderRole, l.podID).Set(0)
        }
        return
    }
    kept, err := l.repo.HeartbeatBosunLeader(ctx, leaderRole, l.podID,
        l.Epoch(), leaderLeaseSec)
    if err != nil || !kept {
        l.setLeader(false, 0)
        metricLeaderCurrent.WithLabelValues(leaderRole, l.podID).Set(0)
    }
}

func (l *RolloutLeader) IsLeader() bool {
    l.mu.RLock()
    defer l.mu.RUnlock()
    return l.isLeader
}

func (l *RolloutLeader) Epoch() int64 {
    l.mu.RLock()
    defer l.mu.RUnlock()
    return l.epoch
}

func (l *RolloutLeader) setLeader(isLeader bool, epoch int64) {
    l.mu.Lock()
    defer l.mu.Unlock()
    l.isLeader = isLeader
    l.epoch = epoch
}
```

- [ ] **Step 2: Коммит**

---

## Task 21: Rollout worker

**File:** `internal/app/ozon/infrastructure/cloudozon/bosun/rollout_worker.go`

- [ ] **Step 1**

```go
package bosun

import (
    "context"
    "encoding/json"
    "fmt"
    "time"

    "github.com/google/uuid"

    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

const rolloutTickPeriod = 60 * time.Second

func (i *Implementation) runRolloutWorker(ctx context.Context) {
    ticker := time.NewTicker(rolloutTickPeriod)
    defer ticker.Stop()
    for {
        select {
        case <-ctx.Done():
            return
        case <-ticker.C:
            if !i.rolloutLeader.IsLeader() {
                continue
            }
            i.tickRollouts(ctx, i.rolloutLeader.Epoch())
        }
    }
}

func (i *Implementation) tickRollouts(ctx context.Context, epoch int64) {
    rollouts, err := i.repo.ListRunningBosunRollouts(ctx)
    if err != nil {
        return
    }
    now := time.Now()
    for _, r := range rollouts {
        if r.State == "pending" {
            // первый тик: переводим в running
            ok, err := i.repo.(interface {
                CheckLeaderEpoch(ctx context.Context, role string, epoch int64) (bool, error)
            }).CheckLeaderEpoch(ctx, leaderRole, epoch)
            if err != nil || !ok {
                return
            }
            _ = i.repo.UpdateBosunRolloutState(ctx, r.RolloutID, "running", "",
                true, false, false, false)
            continue
        }
        if r.State != "running" {
            continue
        }
        i.tickOneRollout(ctx, r, epoch, now)
    }
}

func (i *Implementation) tickOneRollout(ctx context.Context, r repository.BosunRollout, epoch int64, now time.Time) {
    var snapshot []string
    if err := json.Unmarshal(r.TargetSnapshot, &snapshot); err != nil || len(snapshot) == 0 {
        return
    }

    counters, err := i.repo.CountBosunRolloutStatus(ctx, r.RolloutID, r.TargetVersion, snapshot)
    if err != nil {
        return
    }
    rate := FailureRate(counters.Succeeded, counters.Failed)
    metricRolloutFailureRate.WithLabelValues(r.RolloutID.String()).Set(float64(rate))

    if ShouldHalt(counters.Succeeded, counters.Failed, r.MinEvaluated, r.MaxFailureRate) {
        reason := fmt.Sprintf("failure_rate=%.4f succeeded=%d failed=%d min=%d max=%.4f",
            rate, counters.Succeeded, counters.Failed, r.MinEvaluated, r.MaxFailureRate)
        _ = i.repo.UpdateBosunRolloutState(ctx, r.RolloutID, "halted", reason,
            false, true, false, false)
        _ = i.audit(ctx, "bosun.rollout.halt", "system", "system",
            &r.RolloutID, nil, map[string]any{"reason": reason})
        return
    }

    if IsCompleted(counters.Succeeded, counters.Failed, r.TotalTargets) {
        _ = i.repo.UpdateBosunRolloutState(ctx, r.RolloutID, "completed", "",
            false, false, false, true)
        _ = i.audit(ctx, "bosun.rollout.complete", "system", "system",
            &r.RolloutID, nil, map[string]any{
                "succeeded": counters.Succeeded, "failed": counters.Failed})
        return
    }

    var startedAt time.Time
    if r.StartedAt != nil {
        startedAt = *r.StartedAt
    }
    elapsed := ElapsedActive(startedAt, r.TotalPausedSec, r.PausedAt, now)
    expected := ExpectedDispatched(r.TotalTargets, r.OverDurationSec, elapsed)
    batch := BatchSize(expected, counters.Dispatched, r.MaxBatchSize)
    if batch == 0 {
        return
    }

    hosts, err := i.repo.SelectBosunHostsForRollout(ctx, snapshot, r.RolloutID, true, batch)
    if err != nil || len(hosts) == 0 {
        return
    }

    if ok, err := i.repo.(interface {
        CheckLeaderEpoch(context.Context, string, int64) (bool, error)
    }).CheckLeaderEpoch(ctx, leaderRole, epoch); err != nil || !ok {
        return
    }

    n, err := i.repo.SetBosunClientTarget(ctx, hosts, r.TargetVersion, &r.RolloutID, false)
    if err != nil {
        return
    }
    metricRolloutDispatched.WithLabelValues(r.RolloutID.String()).Add(float64(n))
}
```

- [ ] **Step 2: Коммит**

---

## Task 22: Operator: IssueRollout + GetRolloutStatus + Pause/Resume/Abort + KickRollout

- [ ] **Step 1: issue_rollout.go**

```go
package bosun

import (
    "context"
    "encoding/json"

    "github.com/google/uuid"
    "google.golang.org/grpc/codes"
    "google.golang.org/grpc/status"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/repository"
)

func (i *Implementation) IssueRollout(ctx context.Context, req *desc.IssueRolloutIn) (*desc.IssueRolloutOut, error) {
    actor, kind, err := i.operatorAuth(ctx)
    if err != nil {
        return nil, err
    }
    if req.TargetVersion == 0 || req.Target == nil {
        return nil, status.Error(codes.InvalidArgument, "target_version and target required")
    }
    if req.MaxFailureRate < 0 || req.MaxFailureRate > 1 {
        return nil, status.Error(codes.InvalidArgument, "max_failure_rate must be in [0,1]")
    }

    hosts, err := i.expandTarget(ctx, req.Target)
    if err != nil {
        return nil, convertError(err)
    }
    if len(hosts) == 0 {
        return nil, status.Error(codes.InvalidArgument, "target matched 0 hosts")
    }

    rid := uuid.New()
    spec, _ := json.Marshal(req.Target)
    snap, _ := json.Marshal(hosts)
    overSec := 30 * 60  // default 30m
    if req.OverDuration != nil {
        overSec = int(req.OverDuration.AsDuration().Seconds())
    }

    in := repository.InsertBosunRolloutIn{
        RolloutID: rid, TargetVersion: int64(req.TargetVersion),
        TargetSpec: spec, TargetSnapshot: snap, TotalTargets: len(hosts),
        OverDurationSec: overSec, MaxFailureRate: req.MaxFailureRate,
        MaxBatchSize: int(maxOrDefault(req.MaxBatchSize, 100)),
        MinEvaluated: int(maxOrDefault(req.MinEvaluated, 20)),
        IssuedBy:     actor,
    }
    r, err := i.repo.InsertBosunRollout(ctx, in)
    if err != nil {
        return nil, convertError(err)
    }
    _ = i.audit(ctx, "bosun.rollout.issue", actor, kind, &rid, nil,
        map[string]any{"target_version": req.TargetVersion, "total_targets": len(hosts),
            "over_duration_sec": overSec, "max_failure_rate": req.MaxFailureRate})
    return &desc.IssueRolloutOut{
        RolloutId: r.RolloutID.String(),
        TotalTargets: uint32(r.TotalTargets),
    }, nil
}

func maxOrDefault(v, def uint32) uint32 {
    if v == 0 {
        return def
    }
    return v
}
```

- [ ] **Step 2: get_rollout_status.go**

```go
package bosun

import (
    "context"
    "encoding/json"

    "github.com/google/uuid"
    "google.golang.org/protobuf/types/known/timestamppb"
    "time"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

func (i *Implementation) GetRolloutStatus(ctx context.Context, req *desc.GetRolloutStatusIn) (*desc.GetRolloutStatusOut, error) {
    if _, _, err := i.operatorAuth(ctx); err != nil {
        return nil, err
    }
    rid, err := uuid.Parse(req.RolloutId)
    if err != nil {
        return nil, convertError(err)
    }
    r, err := i.repo.GetBosunRollout(ctx, rid)
    if err != nil {
        return nil, convertError(err)
    }
    var snapshot []string
    _ = json.Unmarshal(r.TargetSnapshot, &snapshot)
    counters, _ := i.repo.CountBosunRolloutStatus(ctx, rid, r.TargetVersion, snapshot)

    out := &desc.GetRolloutStatusOut{
        RolloutId: r.RolloutID.String(),
        State:     stateToProto(r.State),
        TotalTargets: uint32(r.TotalTargets),
        DispatchedTargets: uint32(counters.Dispatched),
        SucceededTargets: uint32(counters.Succeeded),
        FailedTargets: uint32(counters.Failed),
        PendingTargets: uint32(counters.Pending),
        CurrentFailureRate: FailureRate(counters.Succeeded, counters.Failed),
        IssuedAt: timestamppb.New(r.IssuedAt),
    }
    if r.HaltReason != nil {
        out.HaltReason = *r.HaltReason
    }
    if r.StartedAt != nil {
        out.StartedAt = timestamppb.New(*r.StartedAt)
        elapsed := ElapsedActive(*r.StartedAt, r.TotalPausedSec, r.PausedAt, time.Now())
        out.ElapsedActiveSec = int64(elapsed.Seconds())
    }
    if r.HaltedAt != nil {
        out.HaltedAt = timestamppb.New(*r.HaltedAt)
    }
    if r.CompletedAt != nil {
        out.CompletedAt = timestamppb.New(*r.CompletedAt)
    }
    return out, nil
}

func stateToProto(s string) desc.RolloutState {
    switch s {
    case "pending":   return desc.RolloutState_ROLLOUT_STATE_PENDING
    case "running":   return desc.RolloutState_ROLLOUT_STATE_RUNNING
    case "paused":    return desc.RolloutState_ROLLOUT_STATE_PAUSED
    case "halted":    return desc.RolloutState_ROLLOUT_STATE_HALTED
    case "aborted":   return desc.RolloutState_ROLLOUT_STATE_ABORTED
    case "completed": return desc.RolloutState_ROLLOUT_STATE_COMPLETED
    }
    return desc.RolloutState_ROLLOUT_STATE_UNSPECIFIED
}
```

- [ ] **Step 3: rollout_control.go**

```go
package bosun

import (
    "context"

    "github.com/google/uuid"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

func (i *Implementation) PauseRollout(ctx context.Context, req *desc.RolloutControlIn) (*desc.RolloutControlOut, error) {
    return i.rolloutControl(ctx, req, "pause")
}

func (i *Implementation) ResumeRollout(ctx context.Context, req *desc.RolloutControlIn) (*desc.RolloutControlOut, error) {
    return i.rolloutControl(ctx, req, "resume")
}

func (i *Implementation) AbortRollout(ctx context.Context, req *desc.RolloutControlIn) (*desc.RolloutControlOut, error) {
    return i.rolloutControl(ctx, req, "abort")
}

func (i *Implementation) rolloutControl(ctx context.Context, req *desc.RolloutControlIn, op string) (*desc.RolloutControlOut, error) {
    actor, kind, err := i.operatorAuth(ctx)
    if err != nil {
        return nil, err
    }
    rid, err := uuid.Parse(req.RolloutId)
    if err != nil {
        return nil, convertError(err)
    }
    var newState string
    switch op {
    case "pause":
        if err := i.repo.UpdateBosunRolloutPause(ctx, rid, true); err != nil {
            return nil, convertError(err)
        }
        newState = "paused"
    case "resume":
        if err := i.repo.UpdateBosunRolloutPause(ctx, rid, false); err != nil {
            return nil, convertError(err)
        }
        newState = "running"
    case "abort":
        if err := i.repo.UpdateBosunRolloutState(ctx, rid, "aborted", "",
            false, false, true, false); err != nil {
            return nil, convertError(err)
        }
        newState = "aborted"
    }
    _ = i.audit(ctx, "bosun.rollout."+op, actor, kind, &rid, nil,
        map[string]any{"reason": req.Reason})
    return &desc.RolloutControlOut{NewState: stateToProto(newState)}, nil
}
```

- [ ] **Step 4: kick_rollout.go**

```go
package bosun

import (
    "context"
    "encoding/json"

    "github.com/google/uuid"
    "google.golang.org/protobuf/types/known/timestamppb"
    "time"

    desc "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/api/bosun"
)

func (i *Implementation) KickRollout(ctx context.Context, req *desc.KickRolloutIn) (*desc.KickRolloutOut, error) {
    if _, _, err := i.operatorAuth(ctx); err != nil {
        return nil, err
    }
    rid, err := uuid.Parse(req.RolloutId)
    if err != nil {
        return nil, convertError(err)
    }
    r, err := i.repo.GetBosunRollout(ctx, rid)
    if err != nil {
        return nil, convertError(err)
    }
    if !i.config.BosunSubscribeEnabled() {
        return &desc.KickRolloutOut{Kicked: 0}, nil
    }
    var snapshot []string
    _ = json.Unmarshal(r.TargetSnapshot, &snapshot)
    var kicked uint32
    for _, h := range snapshot {
        if i.subscribers.Kick(h, &desc.Kick{
            IssuedAt: timestamppb.New(time.Now()),
            Reason:   "operator_kick",
        }) {
            kicked++
        }
    }
    return &desc.KickRolloutOut{Kicked: kicked}, nil
}
```

- [ ] **Step 5: Коммит**

---

## Task 23: Pod registry (вспомогательное)

Уже частично сделано в Task 9 (runPodHeartbeat в service.go). Здесь — если нужны дополнительные методы.

Skip / merged into Task 9.

---

## Task 24: Main.go registration + smoke test

**Files:**
- Modify: `cmd/chiit-server/main.go`

- [ ] **Step 1: Регистрация BosunAPI**

```go
package main

import (
    "context"

    "gitlab.ozon.ru/platform/scratch"
    "gitlab.ozon.ru/platform/tracer-go/logger"
    "google.golang.org/grpc"
    "google.golang.org/grpc/credentials/insecure"

    chiit_server "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit"
    chiit_server1 "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/app/ozon/infrastructure/cloudozon/pg_shard_manager"
    chiit_server2 "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/app/ozon/infrastructure/cloudozon/bosun"   // NEW
    "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/clients/itc"
    _ "gitlab.ozon.ru/infrastructure/cloudozon/chiit-server/internal/config"
)

func main() {
    a, err := scratch.New()
    if err != nil {
        logger.Fatalf(context.Background(), "can't create app: %s", err)
    }
    ctx := context.Background()
    itcFabric, err := itc.DecoratedClientFabric(ctx, a, grpc.WithTransportCredentials(insecure.NewCredentials()))
    if err != nil {
        logger.Fatalf(ctx, "cannot create ITC client: %s", err)
    }
    if err := a.Run(
        chiit_server.NewChiitServer(ctx, a, itcFabric),
        chiit_server1.NewPgShardManagerV1(ctx, a),
        chiit_server2.NewBosunAPI(ctx, a),                                  // NEW
    ); err != nil {
        logger.Fatalf(ctx, "can't run app: %s", err)
    }
}
```

- [ ] **Step 2: Полный build**

```bash
go build ./...
```

Ожидается: zero errors.

- [ ] **Step 3: Прогон всех тестов**

```bash
go test ./...
go test -tags integration ./internal/repository/... 
```

Ожидается: всё PASS.

- [ ] **Step 4: Manual smoke (через grpcurl или CI runbook)**

```
# 1. Запускаем chiit-server с миграциями (CI_AUTO_MIGRATION=true)
# 2. Публикуем тестовый bundle через PublishBundle (с admin-token)
grpcurl -plaintext \
  -H "admin-token: $ADMIN_TOKEN" \
  -d '{"blob":"AAA...","sha256":"...","signature":"...","tags":["test"]}' \
  localhost:9000 \
  ozon.infrastructure.cloudozon.bosun.BosunAPI/PublishBundle
# → возврат version=1

# 3. Issue rollout на тестовый host
grpcurl -plaintext -H "admin-token: $ADMIN_TOKEN" \
  -d '{"target_version":1,"target":{"hosts":["test-host-1"]},"over_duration":"1800s","max_failure_rate":0.1}' \
  localhost:9000 \
  ozon.infrastructure.cloudozon.bosun.BosunAPI/IssueRollout
# → возврат rollout_id, total_targets=1

# 4. GetClientState — увидим target_version=1, target_rollout_id=...
grpcurl -plaintext -H "admin-token: $ADMIN_TOKEN" \
  -d '{"host":"test-host-1"}' \
  localhost:9000 \
  ozon.infrastructure.cloudozon.bosun.BosunAPI/GetClientState
```

- [ ] **Step 5: Финальный коммит**

```bash
git commit -m "feat(bosun): wire BosunAPI into chiit-server main"
```

---

## Verification checklist

После выполнения всех task'ов проверь:

- [ ] `make generate` отрабатывает без ошибок.
- [ ] `go build ./...` — zero errors.
- [ ] `go test ./...` — все unit PASS.
- [ ] `go test -tags integration ./internal/repository/...` — все integration PASS (нужны PG env переменные).
- [ ] `go vet ./...` — clean.
- [ ] Все 14 RPC из proto имеют handler в `internal/app/.../bosun/`.
- [ ] 24-я миграция применилась успешно при первом старте сервера с CI_AUTO_MIGRATION=true.
- [ ] Pod heartbeat и rollout leader heartbeat видны в `bosun_pods` и `bosun_leader` при запущенном сервере.
- [ ] PublishBundle → IssueRollout → manual GetTargetVersion → ReportApplyResult → GetClientState — полный цикл работает на dev-окружении.
- [ ] При двух работающих pod'ах: один лидер, второй candidate; pkill -STOP лидера → второй перехватывает роль в течение 60 секунд; epoch инкрементируется.

---

## Открытые TODO для P0 (внутри плана)

Следующие куски не имеют полной имплементации и помечены как TODO:

1. **`issueCertForHost`** в Bootstrap — нужно подключить к chiit-server cert manager API (см. `internal/cert_manager`).
2. **`getHeaderFromContext`** в utils.go — реиспользовать копию из chiit-server'ного utils (есть в `internal/app/ozon/infrastructure/cloudozon/chiit/utils.go`).
3. **Signature verification** в PublishBundle — против известного CI public key. Решить, откуда брать ключ (vault path / config).
4. **expandTarget** для severity_class и clusters — нужно подключить к storage-inventory через chiit прокси.
5. **`config.BosunSubscribeEnabled()`, `config.GetPodID()`, `config.GetSelfAddr()`, `config.HashRegistrationSecret()`** — добавить в getter.go или RT-config.

Эти точки реализуются в момент implementation, исходя из реальной структуры сервера, в одной с handler'ом коммите.

---

## Что НЕ покрыто этим планом

- Frontend / Web UI.
- Bundle distribution через S3/OCI.
- Self-upgrade клиента.
- mTLS.
- Per-host bosun_rollout_targets ledger.
- Backoff клиента на failed apply.
- Durable outbox / apply_run_id.
- CHECK constraints в DDL.
- bosun-client (Rust) implementation — отдельный план в `bosun-client/`.
