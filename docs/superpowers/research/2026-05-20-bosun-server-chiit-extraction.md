# Bosun-server: что вынести из chiit-server (расширение BonsunV1)

Date: 2026-05-20
Author: research-агент по заданию пользователя
Status: draft, требует решения

## TL;DR

Из 31 RPC текущего `ChiitServer` для bosun-server полезны ~12: пять уже
зафиксированных (`VaultGet`, `GetCert`, `GetRSAPairs`, `GetTalosKeys`,
`BootstrapBucket`) плюс семь новых кандидатов на BonsunV1 — `GetSeverity`,
`GetPersonRoles`, `GetMasterOfPatroniCluster`, `GetWhiteListForResource`,
`GetDatabaseList`, `StorageHostInventoryGet`, `StorageInventoryGroupGet`.
Все они уже кэшируются в chiit-server поверх PG/storage-inventory/Hallpass и
переписывание их сэкономит ~3 недели работы. Discovery подов bosun-server и
ECDSA-аутентификацию клиентов в BonsunV1 выносить нельзя — это часть нового
control plane. Финальный scope bosun-server: 4–5 недель (минимум), 6–7
(медианно), 9–10 (с production-ready audit pipeline и canary mid-flight
controls).

## Контекст

Параллельный декомпозиционный документ уже зафиксировал решение: bosun-server
живёт в Rust, chiit-server остаётся как есть (`2026-05-20-bosun-server-language-choice.md`,
вариант E — гибрид). Поверх этого согласовано: bosun-server и chiit-server
живут в **одном поде** (sidecar-pair) и общаются через 127.0.0.1 loopback,
чтобы bosun-server (Rust, новый) переиспользовал готовые куски legacy
chiit-server (Go, проверен на прод). Часть API уже вынесена в `BonsunV1`:
VaultGet, GetCert, GetRSAPairs, GetTalosKeys, BootstrapBucket. Сейчас
смотрим что ещё имеет смысл туда добавить.

## Анализ функций chiit-server

Полная инвентаризация делается по `chiit-server/api/chiit/api.proto` и
`internal/app/ozon/infrastructure/cloudozon/chiit/*.go`. Все 31 RPC и
несколько background-задач + HTTP endpoint'ы.

### Public RPC (31 штука)

| # | RPC                            | Категория | Сложность impl в bosun-server (если делать самим) |
|---|--------------------------------|-----------|--------------------------------------------------|
| 1 | `Build`                        | C         | n/a (chiit-client legacy) |
| 2 | `GetBuilds`                    | C         | n/a |
| 3 | `Release`                      | C         | n/a |
| 4 | `Report`                       | C         | n/a |
| 5 | `GetRelease`                   | C         | n/a (bosun использует server-push) |
| 6 | `GetReports`                   | C         | n/a |
| 7 | `GetLastReport`                | C         | n/a |
| 8 | `CreateClient`                 | C         | n/a (bosun делает bootstrap сам) |
| 9 | `DeleteClient`                 | C         | n/a |
|10 | `ListClients`                  | C         | n/a |
|11 | `VaultGet`                     | A (уже)   | 1–2 недели (vaultrs + cache + sign-validation) |
|12 | `StorageInventoryGet`          | B         | 2 дня (HTTP-обёртка) |
|13 | `StorageInventoryGroupGet`     | B         | 2 дня |
|14 | `StorageInventoryList`         | B         | 1 день |
|15 | `UpdateClientKey`              | C         | n/a |
|16 | `DropCache`                    | C         | n/a (нет state-cache в bosun) |
|17 | `GetWardenHosts` (legacy)      | C         | n/a (deprecated) |
|18 | `GetWardenHostsForService`     | B         | 1 неделя (warden-client) |
|19 | `GetSeverity`                  | B (важно) | 3 дня (PG-запрос + ARC-cache) |
|20 | `GetPersonRoles`               | B         | 3 дня |
|21 | `SyncPersonAndSeverity`        | C         | n/a (background job в chiit) |
|22 | `GetDatabaseList`              | B         | 2 дня |
|23 | `GetCert`                      | A (уже)   | 2 недели (cert-manager-gateway client + cache) |
|24 | `SetSilence`                   | D         | в bosun делает оператор-CLI |
|25 | `DeleteSilence`                | D         | в bosun делает оператор-CLI |
|26 | `GetTalosKeys`                 | A (уже)   | 3 дня (talos JWKS fetch) |
|27 | `GetRSAPairs`                  | A (уже)   | 3 дня (RSA gen + PG cache) |
|28 | `GetMasterOfPatroniCluster`    | B         | 3 дня (warden discovery + role detection) |
|29 | `GetWhiteListForResource`      | B         | 5 дней (Hallpass-client + sync routine) |
|30 | `StorageHostInventoryGet`      | B         | 2 дня |
|31 | `BootstrapBucket`              | A (уже)   | 1–2 недели (pg-backup-manager grpc client) |

И из `PgShardManagerV1`:

| # | RPC                | Категория | Сложность |
|---|--------------------|-----------|-----------|
| 1 | `GetPostgresSecret`| B (опц.)  | 3 дня (Vault path конкретный) |
| 2 | `GetPersonRoles`   | дубль PerRolesV1 | — |

### Background-задачи chiit-server

Из `service.go`, lines 196-202:

- `runUpdateReleaseCanaryCache` — каждые 5s обновляет `releaseCache` и
  `canaryCache` из PG. Категория **C** (release-flow chiit-client'a).
- `runManagePartitions` — каждые 5min создаёт партиции `reports` и дропает
  старые. Категория **C** (chiit-таблицы).
- `runWatchWardenHosts` — каждые 30s обновляет список warden-эндпоинтов
  для сервисов из конфига. Категория **B**, можно переиспользовать.
- `localStorageRun` — HTTP server на ProxyPort для upload/download
  bundle binary файлов. Категория **C** (legacy chiit-bundle distribution).
- `proxyRun` — HTTP/gRPC-gateway proxy с whitelisted-URL и keycloak-auth.
  Категория **C** (frontend для chiit-CLI). bosun-server делает свой
  proxy сам.
- `cleanupFilesAndBuilds` — каждый час удаляет старые builds/files
  из PG. Категория **C**.
- `runCertCache` — fetches cert manager certs. Категория **A** (через
  GetCert уже покрыто).
- `runITCSync` — каждый час syncит ресурсы из ITC API (severity +
  persons). Категория **B** (это backbone того, что отдаёт `GetSeverity`
  и `GetPersonRoles`).
- `routineUpdateWhiteListCache` — каждую минуту синхронит white-list из
  Hallpass. Категория **B** (backbone для `GetWhiteListForResource`).
- `fetchAndCleanupAlerts` — подписка на алерт-databus + cleanup в 00:00 UTC.
  Категория **B** (опционально — даёт circuit breaker для деплоев).

### Storage / data layer

`chiit-server/migrations/` — 23 миграции. Релевантные таблицы для bosun
(если он будет смотреть в ту же PG-схему):

- `itc_severity (name, environment, severity)` — категория **B**, нужно
  bosun для targeting `apply on severity:low`.
- `itc_store_persons` — категория **B**, для RBAC.
- `rsa_keys (id, environment, public, private)` — категория **A**,
  получаются через GetRSAPairs.
- `certificates (cert_id, version, ...)` — категория **A**, через GetCert.
- `talos_keys (environment, key_id, certificate_pub)` — категория **A**,
  через GetTalosKeys.
- `white_list (hall_pass_id, resource_name)` — категория **B**.
- `canary (canary, percent, environment)` — категория **C** (chiit-release).
- `clients (host, client_public_key)` — категория **C**, bosun делает
  свой `nodes`.
- `builds (id, checksum, url)`, `releases (build_id, environment)`,
  `reports (build_id, host, ...)`, `files (path, content)` — **C**
  (chiit-bundle pipeline).
- `alert_events (alert_id, ...)` — **B** (опционально).

## Категория A. Must-have для bosun-server (через BonsunV1)

Эти функции bosun-server **обязан** получить, чтобы работать на 60k
клиентов. Зафиксированы для BonsunV1 в предыдущем дизайне.

### A.1 `VaultGet` — секреты для bosun-client

bosun-client не имеет прямого доступа к Vault. Запрашивает у
bosun-server, тот ходит в Vault через свой токен и возвращает
конкретный (path, key). Реализовано в
`internal/app/.../chiit/vault_get.go` поверх ARC-cache в
`internal/vault/cache.go`. Включает ECDSA-валидацию подписи клиента.

Rationale для переиспользования: уже работает, hashicorp/vault/api
клиент + Kubernetes auth (`internal/vault/kube.go:24`), полиси
прописаны, кэш на ARC. Переписывать на `vaultrs` имеет смысл, но это
1–2 недели вместо «дёрнули у соседа».

### A.2 `GetCert` — TLS-сертификат для bosun-client

bosun-client при apply bundle'а может потребовать TLS-cert (например,
для подключения к Patroni-кластеру в режиме mTLS). chiit-server
запрашивает у Cert-Manager-Gateway, кеширует в PG (`certificates`
таблица). Реализовано в `internal/app/.../chiit/get_cert.go`.

Rationale: Cert-Manager-Gateway имеет проприетарный protobuf-контракт.
Реализация Rust-клиента — это переписывание прокси через `tonic` (~2
недели), хотя сам код не сложный.

### A.3 `GetRSAPairs` — стабильные RSA-пары для bootstrap

Используется когда чувствительный workflow требует стабильной пары
private/public RSA (например, signing config'ов patroni для
online-upgrade). chiit-server генерирует пару на лету (4096-bit),
сохраняет в PG, кеширует на час в ARC (`internal/app/.../chiit/get_rsa_pairs.go:79`).

Rationale: RSA generation сам по себе тривиален на Rust (`rsa` крейт уже
есть в workspace), но интеграция с PG-state (`rsa_keys` таблица),
ARC-cache и ECDSA-валидация запроса — суммарно ~3 дня. Дешевле
переиспользовать.

### A.4 `GetTalosKeys` — JWKS для аутентификации в Talos

Talos — внутренний identity-провайдер Ozon. chiit-server каждые
`TalosFetchTimeout*2 + 1s` идёт по URL (per-env), парсит JWKS,
извлекает публичные ключи в PEM. Реализовано в
`internal/app/.../chiit/talos.go:65`.

Rationale: HTTP + JWKS parse + PEM encode. На Rust ~3 дня, но не
блокирующее. Через chiit-server проще.

### A.5 `BootstrapBucket` — S3-bucket для Patroni-кластеров

bosun применяет bundle'ы, которые поднимают новый Patroni-кластер.
Кластеру нужен S3-bucket для basebackup'ов. chiit-server проксирует
запрос в `pg-backup-manager` (отдельный grpc-сервис) и возвращает
access-keys. Реализовано в
`internal/app/.../chiit/bootstrap_bucket.go:12`.

Rationale: pg-backup-manager grpc-client + ECDSA-валидация. Переписать
на Rust — 1–2 недели, но никаких преимуществ от Rust-версии нет, чистая
проксирующая логика.

## Категория B. Nice-to-have в BonsunV1 (новые предложения)

Эти RPC bosun-server теоретически может реализовать сам, но
переиспользование в chiit-server даёт значительную экономию времени.

### B.1 `GetSeverity` + `GetDatabaseList` — severity-targeting кластеров

**Что даёт bosun-server:** список баз по severity-классу — основа
targeting'а в bundle.tags (`apply on severity:low`). Также сам
severity-класс для конкретной базы (запрос «какой severity у `address-api`
в production?»).

**Где реализовано:**
- `internal/app/.../chiit/get_severity.go:31` — ARC-cache (size,
  TTL из конфига) + PG-запрос `select severity from itc_severity`.
- `internal/app/.../chiit/get_database_list.go:12` →
  `internal/repository/get_databases_by_severity.go:10` —
  `select name from itc_severity where environment = $1 and severity = $2`.

**Backbone-задача:** `runITCSync` (каждый час syncит данные из ITC API
с jitter'ом для разных подов). Если bosun-server без этого, надо
дублировать в Rust + поддерживать ITC-клиент (`internal/clients/itc/`).

**Сложность impl в bosun-server:** PG-запрос + ARC-cache (3 дня). ITC
sync routine — отдельная неделя (нужен ITC SDK). **Экономия от
переиспользования: ~1.5 недели.**

**Trade-offs:** coupling по PG-схеме (`itc_severity` таблица). Если в
будущем разделим базы — придётся откатывать. Сейчас приемлимо, потому
что severity = source of truth идёт из ITC, который уже один.

### B.2 `GetPersonRoles` — RBAC и контакты в bundle

**Что даёт bosun-server:** список персон с их ролями в базе (LEADER,
DEVELOPER, OWNER, и т.д.). Нужно для: notifications (кому слать
report о failed apply), RBAC (кто может triggerить task на этом
кластере), audit (кто owner / responsible team).

**Где реализовано:**
`internal/app/.../chiit/get_person_roles.go:29` — ARC-cache + PG-запрос
поверх `itc_store_persons`. Sync через тот же `runITCSync`.

**Сложность impl в bosun-server:** 3 дня. **Экономия: ~3 дня.**

### B.3 `GetMasterOfPatroniCluster` — какой нод сейчас master

**Что даёт bosun-server:** при apply task на кластер нужно различать
master/replica. Запрос «кто сейчас master у `pg-25156`?» — отдаёт
hostname.

**Где реализовано:**
`internal/app/.../chiit/get_master_of_patroni_cluster.go:43` — HTTP
GET к warden (`https://warden-{prod,stg}.o3.ru/endpoints?service=...`),
парсит response, ищет `Role: "master"`. Кэш 3s.

**Сложность impl в bosun-server:** warden-client (~1 неделя) + HTTP +
parse + cache (3 дня). Уже есть `bosun-systemd-client` крейт, можно
расширить на warden. **Экономия: ~2 недели.**

**Trade-offs:** warden discovery — внешнее знание, нужно понимать его
endpoints. chiit-server уже знает.

### B.4 `GetWhiteListForResource` — кто имеет доступ к ресурсу

**Что даёт bosun-server:** white-list по hall-pass-id для конкретной
базы. Используется для: «кому разрешено triggerить task на этой базе»
(в дополнение к severity и person-role).

**Где реализовано:**
`internal/app/.../chiit/get_white_list_for_resource.go:19` +
`internal/app/.../chiit/white_list.go` — Hallpass grpc-client +
storage-inventory lookup + cache.

**Сложность impl в bosun-server:** Hallpass-client (~3 дня) +
sync-routine + storage-inventory lookup (5 дней). **Экономия: ~1
неделя.**

### B.5 `StorageHostInventoryGet` + `StorageInventoryGroupGet` — inventory lookup

**Что даёт bosun-server:** при apply bundle'а в decision-логике
bundle'а (Starlark) хочется знать про конкретный хост или группу из
storage-inventory: какой кластер, какие порты, какой patroni-cluster и
т.д. Также используется для target-резолва: «apply на группе
pg-25156».

**Где реализовано:**
`internal/app/.../chiit/storage_host_inventory_get.go:14` и
`storage_inventory_group_get.go` — тонкая обёртка над gRPC-клиентом
storage-inventory с ARC-cache fallback при отказе SI.

**Сложность impl в bosun-server:** ~3 дня на каждый RPC. Storage-
inventory gRPC схема уже зафиксирована. **Экономия: 1 неделя.**

**Важно:** storage-inventory schema — большой proto (см.
`/internal/pb/.../inventory/`). Тащить его в Rust workspace — лишняя
сериализация. Через chiit-server proto уже скомпилировано.

### B.6 `GetWardenHostsForService` — pod discovery соседних сервисов

**Что даёт bosun-server:** список IP:port подов для конкретного
сервиса в k8s. Например, «куда сейчас деплоится bosun-server-replica-1»
— чтобы forward пользовательский HTTP-запрос «status of cluster X» к
поду, который держит стрим этого кластера.

**Где реализовано:**
`internal/app/.../chiit/get_warden_hosts_for_service.go:13` — лежит
в `wardenHostCustom` map, заполняется в `runWatchWardenHosts` для
сервисов из `WardenWatchServices` config.

**Сложность impl в bosun-server:** warden grpc-client (~1 неделя),
watch routine, конфиг target services. **Экономия: ~1 неделя.**

**Trade-off:** bosun-server и chiit-server **в одном поде** — этот RPC
теряет смысл для self-discovery (zip-pod знает свой IP), но остаётся
полезным для look-up других сервисов (например, для `bosun-server`
peers — peer-discovery, ср. `bosun-server-client-architecture.md`
"Peer-discovery через PG").

В bosun-server peer-discovery предложен через PG-таблицу `servers`
(`{pod_id, ip, port, last_heartbeat, dc}`). Это **не нужно делать через
warden** — PG уже есть. RPC `GetWardenHostsForService` остаётся для
discovery других **сервисов** (cert-manager, pg-backup-manager, и т.д.),
а не bosun-server peers.

## Категория C. Out-of-scope (не нужно bosun-server)

Эти функции относятся к chiit-client legacy flow и в bosun не
переносятся. bosun-server делает то же самое, но через свой push-based
протокол.

### C.1 Bundle distribution legacy

- `Build` / `GetBuilds` / `Release` / `GetRelease` — chiit-bundle
  pipeline (binary встраивается в chiit-client). У bosun bundle =
  отдельный tar.gz в S3/OCI, разные команды.
- `Report` / `GetReports` / `GetLastReport` — chiit-report flow. У
  bosun status update идёт через gRPC bi-di stream (см.
  `bosun-server-client-architecture.md` "Командный flow").
- `localStorage` HTTP server (`local_storage.go`) — chiit binary
  upload/download. У bosun bundles в S3/OCI.

### C.2 Client registration legacy

- `CreateClient` / `DeleteClient` / `ListClients` / `UpdateClientKey`
  — chiit регистрация. У bosun свой `bootstrap` flow с одним shared
  secret и ECDSA cert.
- `clients` PG-таблица — chiit-specific. У bosun `nodes`.

### C.3 Canary chiit-bundle release

- `canary` PG-таблица — chiit release-canary (high/medium/low percent).
  В bosun canary — это **operator decision** для конкретного task'а,
  не глобальный percent (см. `concepts/canary-rollout-hash.md`).
- `getEffectiveCanary` (`get_release.go:124`) — chiit-логика.

### C.4 Silence integration

- `SetSilence` / `DeleteSilence` — оператор задаёт silence для
  PG-хоста через chiit-server, тот идёт в duty-calendar API.
  У bosun это **оператор-CLI команда** на стороне bosun-server-cli,
  не часть control-plane API.

### C.5 SyncPersonAndSeverity (manual trigger)

- Endpoint для ручного запуска ITC-sync. В bosun не нужен — sync
  идёт background-job'ом, оператор не должен дёргать API.

### C.6 DropCache

- В bosun caches per-process, drop через рестарт пода. Глобальной
  команды очистки не нужно.

### C.7 GetWardenHosts (без аргументов, legacy)

- Hardcoded `chiit-server.infrastructure:http` — bosun так не работает.

### C.8 Build/Release management UI

- chiit-server-cli (`cmd/chiit-server-cli/`) — CLI для оператора:
  Build, Release, Reports. У bosun свой bosun-server-cli с другими
  командами (`apply`, `rollout`, `status`).

## Категория D. bosun-server делает сам (новое)

Эти функции **специфичны для bosun-server** и не должны идти в
BonsunV1. Они новые, не legacy chiit.

### D.1 gRPC bi-directional stream protocol для bosun-clients

- `subscribe(host_info, version)` — клиент инициирует long-lived
  stream.
- `command(apply_bundle{...} | run_task{...})` — server push.
- `status_update(command_id, phase, details)` — client status.
- `heartbeat_ping` / `heartbeat_pong` — application-level liveness.

Это **сердце bosun-server**. Реализуется на tonic, mTLS,
poll-of-commands-queue из PG.

### D.2 Bootstrap protocol

- HTTP POST `{secret, hostname, platform}` → ECDSA cert + node_id.
- bosun-server в одной транзакции: validate secret, generate ECDSA
  keypair (через rcgen), INSERT INTO nodes.
- Cert хранится в PG (`nodes.cert_pem`).

См. `bosun-server-client-architecture.md` "Bootstrap-secret". В chiit
этого нет (там клиент сам генерирует ключ и регистрируется через
CreateClient + admin-token).

### D.3 Commands queue + dispatcher

- `commands_queue` PG-таблица с polling каждую секунду
  (`PG NOTIFY/LISTEN` отвергнут).
- Под помечает `dispatched_to_pod_id`, шлёт по stream, обновляет
  status при ack/complete/fail.
- Reaper: если pod упал — другой подбирает зависшие dispatched
  команды.

В chiit этого вообще нет (там pull-based, без queue).

### D.4 Node affinity / peer discovery

- `nodes`, `node_affinity`, `servers` таблицы в PG.
- Heartbeat от bosun-server pod'ов в `servers` каждые 5s.
- Cross-DC redirect через `hello_ack { redirect_to: pod_id }`.

См. `bosun-server-client-architecture.md` секция "Multi-pod + общее
состояние".

### D.5 Audit log + node status

- `audit_log` — только failures + важные события (не routine success).
- `node_status` — UPSERT на каждый report, одна строка на ноду.

### D.6 Bundle registry + distribution

- `bundle_registry` в PG (метаданные).
- Сам tar.gz в S3/CDN, не в PG.
- При apply — клиент скачивает + verify signature.

### D.7 Targeting resolver

- Резолвит `cluster:foo`, `severity:low`, `tag:billing` → список
  node_id из `nodes`.
- Может комбинировать с `GetSeverity` (B.1) и storage-inventory (B.5).

## Предложение BonsunV1 API expansion

Финальный proto sketch. Существующие 5 RPC + 7 новых из категории B.

```protobuf
syntax = "proto3";
package ozon.infrastructure.cloudozon.bosun;

service BonsunV1 {
  // === A. Already agreed ===
  rpc VaultGet(VaultGetIn) returns (VaultGetOut);
  rpc GetCert(GetCertIn) returns (GetCertOut);
  rpc GetRSAPairs(GetRSAPairsIn) returns (GetRSAPairsOut);
  rpc GetTalosKeys(GetTalosKeysIn) returns (GetTalosKeysOut);
  rpc BootstrapBucket(BootstrapBucketIn) returns (BootstrapBucketOut);

  // === B.1 Severity / databases ===
  rpc GetSeverity(GetSeverityIn) returns (GetSeverityOut);
  rpc GetDatabaseList(GetDatabaseListIn) returns (GetDatabaseListOut);

  // === B.2 Person roles ===
  rpc GetPersonRoles(GetPersonRolesIn) returns (GetPersonRolesOut);

  // === B.3 Patroni master discovery ===
  rpc GetMasterOfPatroniCluster(GetMasterOfPatroniClusterIn)
      returns (GetMasterOfPatroniClusterOut);

  // === B.4 White-list ===
  rpc GetWhiteListForResource(GetWhiteListForResourceIn)
      returns (GetWhiteListForResourceOut);

  // === B.5 Storage-inventory lookups ===
  rpc StorageHostInventoryGet(StorageHostInventoryGetIn)
      returns (StorageHostInventoryGetOut);
  rpc StorageInventoryGroupGet(StorageInventoryGroupGetIn)
      returns (StorageInventoryGroupGetOut);

  // === B.6 Warden discovery (для сторонних сервисов, не peer) ===
  rpc GetWardenHostsForService(GetWardenHostsForServiceIn)
      returns (GetWardenHostsForServiceOut);
}

// Сигнатуры messages большинства методов уже зафиксированы в
// chiit-server/api/chiit/api.proto (lines 380-710). Переиспользуем
// один-в-один с переименованием package'а.
```

### Per-RPC rationale (краткое резюме)

- **VaultGet** — bosun-client не имеет токена Vault, обращается к
  bosun-server, тот к chiit-server, тот к Vault. ECDSA-валидация
  сохраняется. Trade-off: один лишний loopback hop, ~50µs latency.
- **GetCert / GetRSAPairs / GetTalosKeys** — все требуют PG-state +
  cache + интеграцию с внешним сервисом (Cert-Manager-Gateway, Talos
  JWKS). Чистая выгода от reuse.
- **BootstrapBucket** — нужен новым Patroni-кластерам, поднимаемым
  через bundle apply. Без него bosun не может настроить basebackup.
- **GetSeverity / GetDatabaseList / GetPersonRoles** — backbone
  targeting'а. ITC-sync routine остаётся в chiit (1 sync на под, не
  два).
- **GetMasterOfPatroniCluster** — нужно для tasks типа «pg_repack на
  master» или «failover к replica». Без него bosun-client не знает
  роли.
- **GetWhiteListForResource** — RBAC для tasks.
- **StorageHostInventoryGet / Group** — bundle Starlark должен иметь
  доступ к inventory: `inv.cluster_name`, `inv.patroni_cluster`.
  Прокси через chiit-server проще, чем тащить полный
  storage-inventory proto в Rust workspace.
- **GetWardenHostsForService** — для discovery cert-manager,
  pg-backup-manager и других сервисов из bosun-server. **Не для
  bosun-server peer-discovery** — там PG.

## One-pod архитектура

bosun-server (Rust) и chiit-server (Go) деплоятся как контейнеры в
одном k8s Pod'е. Общение через 127.0.0.1 loopback (gRPC).

### Pros

- **Zero network latency** между bosun и chiit. ~10-50µs loopback vs
  500µs-2ms через k8s Service.
- **Atomic deployment**: chiit рестартится — bosun тоже видит
  рестарт (его loopback закрывается, он переходит в "chiit
  unavailable" state).
- **Shared lifecycle**: оба компонента работают в одной HA-системе.
  Если один умер — pod нездоров, k8s killit весь.
- **Shared logging/monitoring**: один pod = один Prometheus target,
  одна Mimir-метрика, один Loki-лог.
- **Простая network policy**: не нужно открывать порты между
  bosun-server и chiit-server в `NetworkPolicy`.

### Cons

- **Coupling по resource quota**: CPU/memory шарятся на уровне pod'а.
  Если chiit-server съест RAM — bosun-server OOM'нется тоже.
  Mitigation: per-container `resources.limits`.
- **Coupling по rolling update**: при апдейте chiit-server-а bosun
  тоже рестартится (Pod recreate). Это **не страшно**, потому что 60k
  клиентов reconnect'ятся через LB к другому поду.
- **Один и тот же k8s ServiceAccount**: bosun и chiit будут с одним
  SA. Если разделение прав важно — придётся pod splitting.
- **Не masштабируется независимо**: если bosun нужно 4 реплики, а
  chiit — 2, придётся либо разделить, либо overprovision.

### k8s deploy implications

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: bosun-control-plane
spec:
  replicas: 4
  selector:
    matchLabels:
      app: bosun-control-plane
  template:
    spec:
      containers:
        - name: bosun-server
          image: bosun-server:1.0.0
          ports:
            - containerPort: 50051   # gRPC for bosun-clients
            - containerPort: 8080    # bootstrap HTTP
          env:
            - name: CHIIT_LOOPBACK
              value: "127.0.0.1:50052"
          resources:
            limits:
              cpu: "2"
              memory: "1Gi"

        - name: chiit-server
          image: chiit-server:legacy
          ports:
            - containerPort: 50052   # gRPC, only loopback
          resources:
            limits:
              cpu: "1"
              memory: "1.5Gi"

      readinessProbe:
        # combined: оба контейнера должны быть ready
        ...
```

### Health check semantics

- bosun-server livenessProbe: проверяет, что accept'ит gRPC stream'ы
  (TCP connect на 50051).
- bosun-server readinessProbe: проверяет, что loopback chiit-server'a
  доступен (gRPC call `GetTalosKeys` за 100ms).
- chiit-server livenessProbe: его собственный health endpoint.
- chiit-server readinessProbe: PG-connection + Vault-token валиден.

При loopback failure bosun-server **не теряет существующие streams**,
но новые connect'ы он отклоняет (readiness = false), что заставляет k8s
не routit'ить трафик.

### Rolling update flow

1. K8s апдейтит Pod (либо bosun-server-, либо chiit-server-image).
2. Pod помечается как Terminating. Existing bosun-clients получают
   `goAway` от gRPC, reconnect'ятся через LB.
3. Новый Pod стартует. bosun-server ждёт chiit-server (readinessProbe
   с loopback check).
4. После того как оба контейнера ready — pod accept'ит новые
   streams.

Время recovery: ~10-30s на pod, последовательно через все 4 реплики
(MaxSurge=1, MaxUnavailable=0) = 1-2 минуты на полный rolling.

### Vault token sharing

Оба контейнера используют один k8s ServiceAccount для Vault auth.
chiit-server делает kubernetes-auth (см.
`internal/vault/kube.go:24`). bosun-server **может не делать свой
Vault-токен вообще** — все Vault-обращения проходят через chiit-server
loopback. Это упрощает Vault policy management.

## Итоговый scope bosun-server

После всех переносов в BonsunV1, остаётся реализовать в bosun-server
(Rust):

### Минимальный MVP (4-5 недель)

| Часть                                | Дней работы | Кто делает |
|--------------------------------------|-------------|------------|
| Workspace setup (Cargo, lints)       | 1           | senior |
| bosun-proto crate (shared с client)  | 2           | senior |
| Loopback grpc-client → chiit-server  | 3           | mid |
| Bootstrap HTTP endpoint              | 5           | mid |
| `nodes` PG schema + migrations       | 2           | mid |
| gRPC bi-di stream skeleton           | 7           | senior |
| Heartbeat ping/pong                  | 2           | mid |
| `commands_queue` poll + dispatch     | 5           | senior |
| Basic audit_log + node_status        | 3           | mid |
| Single-pod deploy + manifests        | 2           | devops |
| Smoke tests (1 client, 1 cluster)    | 3           | mid |
| **Итого MVP**                        | **35 дней (5 недель)** | 2 разработчика |

В этом MVP **нет**:
- multi-pod load distribution (только 1 реплика, sticky-session
  через `node_affinity`).
- bundle registry / S3 distribution (заглушка: hardcoded URL).
- canary mid-flight controls (только static % override).
- production-ready audit pipeline (логи в STDOUT, не в Mimir/Loki).
- proper Prometheus metrics (только базовые counters).

### Median scope (6-7 недель)

К MVP добавляются:

| Часть                                | Дней работы |
|--------------------------------------|-------------|
| Multi-pod peer discovery (`servers`) | 5           |
| Cross-DC routing (`redirect_to`)     | 5           |
| Bundle registry (PG metadata)        | 5           |
| S3-based bundle distribution         | 7           |
| Cosign signature verification        | 5           |
| Production Prometheus metrics        | 3           |
| OpenTelemetry tracing                | 3           |
| Targeting resolver (cluster/severity/tag) | 5      |
| **+15 дней (3 недели)** к MVP        |             |

### Maximum scope (9-10 недель)

Добавляется:

| Часть                                | Дней работы |
|--------------------------------------|-------------|
| Canary mid-flight controls (pause/resume/rollback) | 7 |
| `audit_log` retention + archive (DWH export) | 5 |
| RBAC через KeyCloak JWT              | 5           |
| Operator-CLI (bosun-server-cli)      | 5           |
| Migration support (chiit-client → bosun-client) | 5 |
| Load testing harness (60k clients)   | 5           |
| HA storyboard / runbook              | 3           |
| Hardening (review, fuzzing, gradual rollout) | 5    |
| **+35 дней (~7 недель)** к median    |             |

### Summary

- **Минимум:** 4-5 недель, 2 разработчика. Pre-production, для дев-кластера.
- **Медиана:** 6-7 недель, 2-3 разработчика. Production-ready для одного
  DC, до 10k клиентов.
- **Максимум:** 9-10 недель, 3 разработчика. Production-ready для всех
  60k клиентов с canary, rollback, retention, RBAC.

## Open questions

### Q1. Действительно ли BonsunV1 → chiit-server gRPC даст нужную perf?

**Сценарий нагрузки:** 60k bosun-clients × 1 heartbeat / 10s = 6k
heartbeat/sec. Heartbeat **не идёт в chiit-server** — он обрабатывается
bosun-server напрямую.

Что идёт в chiit-server:
- VaultGet — на bootstrap клиента (раз в жизни ноды), и при
  apply bundle'a (~раз в день на ноду). 60k × 1/day ≈ 0.7 QPS, peak
  ~50-100 QPS.
- GetSeverity / GetPersonRoles — на каждом apply bundle'a, ~0.7 QPS,
  peak 50-100 QPS.
- GetCert — реже, при rotate (раз в год на ноду). ~0 QPS.
- GetMasterOfPatroniCluster — на каждый task: ~10-100 QPS peak.

Итого: **~200-500 QPS** к chiit-server в peak. Это **значительно меньше**
текущей нагрузки от chiit-clients (40k клиентов × poll каждые 30s = 1300
QPS). chiit-server держит.

Если bosun масштабируется до 4 реплик, и на каждой свой chiit-server
sidecar, нагрузка распределяется: ~50-125 QPS на sidecar. Тривиально.

**Risk class:** если кто-то в bundle'е напишет `for cluster in
inv.list_all_clusters(): get_severity(cluster)` — может выстрелить
до 5k QPS. Mitigation: rate-limit на bosun-server side для proxy
RPC's.

### Q2. Что если chiit-server downscale'нется — bosun-server тоже падает?

В same-pod architecture chiit-server **всегда** есть. Если он крашится
— k8s рестартит pod (бьёт по всем 4 репликам последовательно).

Если бы bosun-server обращался к chiit-server через k8s Service (другой
pod) — тогда chiit-server downscale = bosun-server теряет VaultGet и
прочее. В same-pod архитектуре это невозможно.

**Risk:** если chiit-server liveness fail'ит дольше чем readiness
delay (например, PG temporarily down) — bosun-server тоже не accept'ит
client traffic. Это **корректное поведение**: если bosun-server не
может получить cert/vault, он не может корректно обслужить клиента.
Лучше отбросить, чем дать корректное-выглядящее ошибочное.

### Q3. Кэш-инвалидация для shared state (severity, canary)

chiit-server кэширует severity на стороне сервера (per-pod ARC).
bosun-server по идее **не должен кэшировать** результат GetSeverity
— просто прокидывает к chiit-server'у через loopback. Loopback hop
~50µs, на это можно не экономить.

**Если** нужно кэшировать в bosun-server (например, в hot path):
TTL 1 минута, никакого active invalidation. Severity меняется крайне
редко (только при ручном изменении ITC).

### Q4. Auth model для loopback

Варианты:

1. **No auth (плоский localhost trust).** Loopback недоступен извне
   pod'а. Проще всего, нет cert management.
2. **mTLS на loopback.** Усложнение, нет видимой выгоды.
3. **Shared token в env-var.** ChiitToken попадает в обе контейнера
   через k8s Secret. bosun-server передаёт `Authorization: Bearer
   <token>` в каждый RPC. chiit-server валидирует.

**Recommendation:** вариант 1 (no auth). Loopback не виден из
network, threat model не требует. Если нужна аудитная привязка
"какой client triggered VaultGet" — bosun-server передаёт `host` и
`sign` от клиента в RPC body, chiit-server валидирует ECDSA
(`validator.Validate` уже это делает).

### Q5. Что если chiit-server полностью убирается?

Долгосрочно (3-5 лет) chiit-client должен быть полностью заменён на
bosun-client. После этого chiit-server теряет потребителей и может
быть выведен. Вопрос: что делать с BonsunV1 RPC'ями, которые
проксируются через chiit-server?

**Ответ:** перенести логику в bosun-server. Это **отдельный проект**,
не блокирует текущий MVP. По мере декомиссии chiit-server'a:

1. Сначала переносим backbone-задачи (`runITCSync`,
   `routineUpdateWhiteListCache`) — они уже работают на стороне
   bosun-server-pod'a, просто в другом контейнере.
2. Затем переписываем тонкие RPC-обёртки (GetSeverity →
   `bosun-pg-repo.get_severity`).
3. Vault-интеграцию переносим через `vaultrs` крейт.
4. Cert-Manager-Gateway client — переписываем на Rust через
   `tonic-build` и общий proto.
5. pg-backup-manager — тоже.

Это **estimated +6 недель** работы. Но **не сейчас.** Сейчас экономия
из same-pod архитектуры реальная.

### Q6. Что насчёт chiit-server CLI и admin-token RPC?

chiit-server имеет admin-token-защищённые endpoints (Release,
DeleteClient, UpdateClientKey, DropCache, GetLastReport). Они для
оператора, не для клиентов. Эти endpoint'ы **не идут в BonsunV1** —
оператор продолжает работать через chiit-server-cli.

bosun-server имеет свой operator-CLI (bosun-server-cli) с
bosun-specific commands (apply, rollout, status). Они общаются с
bosun-server через mTLS или admin-grpc-stream.

## Связь с предыдущим documents

- `2026-05-20-bosun-server-language-choice.md` — выбор Rust + hybrid.
  Этот документ ортогонален: тут оптимизируем scope.
- `bosun-server-client-architecture.md` — vision двух-компонентной
  архитектуры. Здесь конкретизируем что из chiit-server переиспользуем.
- `chiit-server.md`, `chiit-client.md`, `canary-rollout-hash.md` — фон.

## Sources

- `chiit-server/api/chiit/api.proto` (lines 14-378) — полный API
  surface.
- `chiit-server/api/pg_shard_manager/api.proto` (lines 13-90) —
  второй сервис.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/service.go`
  (lines 30-205) — Implementation struct + background routines.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_release.go`
  (lines 28-138) — release-cache + canary lookup.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_severity.go`
  (lines 17-43) — severity-cache.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_warden_hosts.go`
  (lines 21-101) — warden watch + lookup.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_master_of_patroni_cluster.go`
  (lines 28-115) — master discovery с retry.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_white_list_for_resource.go`
  (lines 19-103) — Hallpass + storage-inventory.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_database_list.go`
  (lines 12-18) — severity-database query.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/storage_inventory_get.go`
  (lines 17-55) — SI get host.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/storage_host_inventory_get.go`
  (lines 14-41) — SI get cached.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/bootstrap_bucket.go`
  (lines 12-28) — pg-backup-manager proxy.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_rsa_pairs.go`
  (lines 48-98) — RSA gen + cache.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/talos.go`
  (lines 27-111) — talos JWKS fetch.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/get_cert.go`
  (lines 16-29) — cert-manager proxy.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/white_list.go`
  (lines 17-107) — Hallpass cache + sync routine.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/itc_client_sync.go`
  (lines 36-103) — ITC hourly sync with jitter.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/sync_person_and_severity.go`
  (lines 27-138) — sync with retry/backoff.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/manage_partitions.go`
  (lines 11-32) — partition routine.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/cleanup.go`
  (lines 10-26) — cleanup routine.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/local_storage.go`
  (lines 25-220) — chiit binary file storage.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/proxy_run.go`
  (lines 14-43) и `internal/proxy/{proxy,http,warden}.go` — gRPC-gateway.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/alerts.go`
  (lines 17-117) — alert events handling.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/vault_get.go`
  (lines 18-35) — Vault proxy.
- `chiit-server/internal/app/ozon/infrastructure/cloudozon/chiit/create_client.go`
  (lines 20-49) — chiit client registration with sha256 token.
- `chiit-server/internal/validator/validator.go` (lines 24-88) —
  ECDSA-validation pipeline.
- `chiit-server/internal/ecsda/sign.go` (lines 11-44) — sign/verify
  helpers.
- `chiit-server/internal/hallpass/client.go` (lines 17-65) — Hallpass
  grpc-client.
- `chiit-server/internal/keycloak/keycloak.go` (lines 36-87) — JWT
  parser.
- `chiit-server/internal/vault/kube.go` (lines 24-100) — Kubernetes
  auth.
- `chiit-server/internal/repository/types.go` (lines 19-86) — Manager
  interface.
- `chiit-server/internal/repository/get_databases_by_severity.go`
  (lines 10-28) — query.
- `chiit-server/migrations/01_init_schema.sql`,
  `chiit-server/migrations/10_severity_01.sql`,
  `chiit-server/migrations/14_add_cert_cache.sql`,
  `chiit-server/migrations/17_rsa_keys.sql`,
  `chiit-server/migrations/18_hallpass.sql`,
  `chiit-server/migrations/20_canary.sql`,
  `chiit-server/migrations/22_talos_keys.sql` — schema.
- `postgres-chiit/lib/warden/warden.go` (lines 33-204) — warden client
  как клиент chiit использует.
- `.claude-memory-compiler/knowledge/concepts/chiit-server.md` — wiki
  обзор.
- `.claude-memory-compiler/knowledge/concepts/bosun-server-client-architecture.md`
  (lines 17-247) — vision bosun-server.
- `.claude-memory-compiler/knowledge/concepts/canary-rollout-hash.md` —
  rollout formula.
- `.claude-memory-compiler/knowledge/concepts/storage-inventory.md` —
  inventory сервис.
- `docs/superpowers/research/2026-05-20-bosun-server-language-choice.md`
  — обоснование Rust + hybrid.
