# DevOps review архитектурного плана bosun-server (внутри chiit-server)

Date: 2026-05-23
Reviewer: DevOps-engineer, perspective production-ops (60k PG-нод, k8s+KVM)

## TL;DR

План архитектурно складный — pod redirect через consistent hashing, rollout state machine, отдельная таблица heartbeat'ов. Но для production ops пять дыр критичны: (1) нет single-writer-гарантии для rollout dispatcher (один advisory_lock на все pods), (2) bytea-storage для bundles ведёт к bloat и replication-lag на 60k× ApplyBundle (toast-таблица в WAL), (3) NOTIFY на каждый INSERT в commands_queue ломает PG при burst-rollout (нет throttling), (4) failover-сценарий при свалившейся PG не описан совсем — клиенты будут думать что сервер живой, (5) кэш ECDSA public keys инвалидируется только через 30s, а revoke ключа в Bootstrap-flow перерасписывает таблицу — race window открыт на полминуты. Топ-10 в конце документа.

## Findings по категориям

### A. Deployment & lifecycle

- **Issue:** Migration ordering vs deployment. chiit-server использует goose через init-container (`values_production.yaml: migration.pgToolsMigrate`), мигратор гонится отдельно от deploy'а pod'ов. Если pod_2 уже на новой версии и зашёл в Subscribe (SELECT FROM bosun_active_sessions), а миграция ещё не доехала — `relation does not exist`.
  **Что нужно добавить:** Все добавляемые таблицы (`bosun_pods`, `bosun_heartbeats`, `bosun_active_sessions`, `bosun_commands_queue`, `bosun_bundles`, `bosun_rollouts`) должны быть expand-only (`CREATE TABLE IF NOT EXISTS`). Schema-изменения через expand-migrate-contract в три деплоя.

- **Issue:** Первый rolling-deploy chiit-server с BosunAPI. pod_0 на старой версии, pod_1 на новой — pod_1 уже в `bosun_pods`, но клиенты прилетают и на pod_0 через DNS round-robin. На pod_0 нет BosunAPI handler'а — gRPC unimplemented.
  **Что нужно добавить:** Feature-flag в realtime-config `bosun_enabled: false` при первом deploy. Оператор включает после того как все pods на новой версии.

- **Issue:** `values.yaml` не выставляет `terminationGracePeriodSeconds`, k8s default 30s — **совпадает с drain_timeout 30s** из спеки (`2026-05-23-bosun-api-p0-proto.md:618`). SIGTERM → 30s drain → SIGKILL ровно в этот же момент. Клиенты не успевают переподключиться при network jitter.
  **Что нужно добавить:** `terminationGracePeriodSeconds: 60` + `preStop` с `sleep 5` (даёт service-discovery время убрать pod из endpoints). Drain_timeout 45s, буфер 15s.

- **Issue:** HPA триггеры не описаны. CPU не подойдёт — Subscribe idle. Bottleneck — memory + open FDs.
  **Что нужно добавить:** Custom HPA на `bosun_active_streams_per_pod`, порог 25000/pod. `replicas: 3` min — иначе один pod упадёт и весь парк на оставшийся.

- **Issue:** Rollback с фантомами. `kubectl rollout undo` оставляет в `bosun_active_sessions` записи с `pod_id=chiit-server-X-new`. Старая версия не чистит — оператор через `GetNodeStatus` видит «нода активна на pod X», а pod'а нет.
  **Что нужно добавить:** Reaper goroutine каждые 60s удаляет `bosun_active_sessions` где `pod_id NOT IN (SELECT pod_id FROM bosun_pods)`.

- **Pro:** Mature-threshold 20s для нового pod'а в hashing-ring — защита от flap-loop'ов (`2026-05-23-bosun-api-p0-proto.md:630`).

### B. Observability

- **Issue:** В спеке нет раздела про метрики. chiit-server использует Mimir через scratch — instrumentation готова, но что именно exposes bosun-handler не указано.
  **Что нужно добавить:** Minimum set:
  - `bosun_active_streams{pod_id}` gauge
  - `bosun_subscribe_connects_total{pod_id, outcome=ok/auth_failed/redirect_sent}` counter
  - `bosun_command_dispatch_latency_seconds` histogram (NOTIFY → Send)
  - `bosun_command_ack_latency_seconds` histogram (IssueCommand → ReportApplyResult)
  - `bosun_heartbeat_age_seconds` summary
  - `bosun_rollout_failure_rate{rollout_id}` gauge
  - `bosun_pg_notify_queue_usage` (через `pg_notification_queue_usage()`)

- **Issue:** Tracing для streaming Subscribe. `2026-05-23-chiit-server-codegen-and-scratch.md:420` рекомендует «один span на session», но per-command линковка не описана.
  **Что нужно добавить:** Каждый Command получает trace_id/span_id, attached к command_id. bosun-client продолжает trace и шлёт trace_id обратно в ReportApplyResult. Сервер связывает три trace'а: оператор-IssueCommand → server-Send → client-Apply.

- **Issue:** SLO не описано. Что считать failure stream? Закрылся через 5s — атака; через 5min — pod restart.
  **Что нужно добавить:** SLO: `p99 stream duration ≥ 15m`, `p99 reconnect-after-redirect ≤ 5s`. Алерт: `rate(bosun_subscribe_connects_total{outcome=auth_failed}[5m]) > 10/s`.

- **Issue:** Rollout halt → как оператор узнаёт. HALTED state есть, но push-notification не описан.
  **Что нужно добавить:** Counter `bosun_rollouts_halted_total{halt_reason}` с алертом немедленно (`for: 0s`).

- **Issue:** Dashboard для оператора отсутствует. `GetNodeStatus` per-host, `GetRolloutStatus` per-rollout. Fleet-wide view («% нод на bundle_version<42», «top hosts with pending defers») нет.
  **Что нужно добавить:** Grafana dashboards: Fleet health, Rollout progress, Bundle version distribution. Источник — те же таблицы + pre-aggregated materialized views.

- **Issue:** JSON-логи. chiit-server использует `tracer-go/logger` (`logger.Infof(ctx, ...)`) — trace-injection auto, но JSON-fields не enforce'ены.
  **Что нужно добавить:** Для bosun-handler'ов всегда `host`, `command_id`, `rollout_id`, `pod_id`, `event_kind`. Loki query `{app="chiit-server"} | json | command_id="X"`.

- **Pro:** scratch даёт channelz для gRPC streams diagnostics (`2026-05-23-chiit-server-codegen-and-scratch.md:439`).

### C. Capacity planning

- **Issue (P0):** Bundle blob в bytea — антипаттерн на scale. INSERT 50MB → TOAST → 50MB в WAL → replication lag. Rate 1 bundle/day × 50MB = 1.5GB/month WAL. На год = 18GB storage + замусоренный WAL.
  **Что нужно добавить:**
  - `cleanup_old_bundles(retention_days int)` функция (паттерн уже есть в `migrations/13_cleanup.sql`), cron-job ежедневно.
  - Retention: 30 дней + last 5 versions + все versions в active rollouts.
  - `ALTER TABLE bosun_bundles ALTER COLUMN blob SET STORAGE EXTERNAL` (без compression).
  - Метрика `bosun_bundles_total_size_bytes`, алерт > 100GB.

- **Issue (P0):** NOTIFY queue overflow при burst rollout. Batch 1500 hosts → 1500 NOTIFY events. PG queues notifications в shared 8GB. Если consumers не успевают — backend блокирует новые NOTIFY.
  **Что нужно добавить:** Throttling в `internal/bosun/rollouts/dispatcher.go`:
  - max 100 NOTIFY/sec.
  - Pre-flight `SELECT pg_notification_queue_usage()` > 0.5 → sleep 100ms.
  - Альтернатива: pull-model polling каждые 500ms на pod-side вместо NOTIFY. 3 pods × 2/sec = 6 QPS — ничего.

- **Issue:** bosun_heartbeats — 2k QPS UPSERT. 2k dead tuples/sec → autovacuum nagрузка.
  **Что нужно добавить:** `ALTER TABLE bosun_heartbeats SET (fillfactor = 70, autovacuum_vacuum_scale_factor = 0.01, autovacuum_vacuum_cost_limit = 2000)` — HOT-updates + агрессивный autovacuum. Альтернатива: heartbeat в Redis (TTL 90s, no persistence) — 0 нагрузки на PG.

- **Issue:** Memory budget. 60k streams / 3 pods = 20k/pod. Go gRPC ~96 KB/stream (`project_bosun_server_language_choice.md:44`) = **1.92 GB heap**. Plus Session struct, runtime, GC overhead ×2 = **~4 GB/pod**.
  **Что нужно добавить:**
  ```yaml
  resources:
    requests: {memory: "3Gi", cpu: "500m"}
    limits:   {memory: "6Gi", cpu: "2000m"}
  ```
  Plus `grpc.WithReadBufferSize(16*1024)` (default 32KB) — экономит ~16KB/connection.

- **Issue:** FDs. 20k streams × 2 (socket + LISTEN) = 40k FDs/pod. Default ulimit 1024.
  **Что нужно добавить:** `securityContext.sysctls: [{name: "net.core.somaxconn", value: "65535"}]` + `ulimit -n 100000` в entrypoint.

- **Pro:** `bosun_pods` self-ping каждые 5s — компактно, не масштабируется с числом клиентов.

### D. Incident response

- **Issue (P0):** PG-failover не описан. UPSERT в `bosun_active_sessions` провалился, `LISTEN bosun_command_<host>` обрыв. Server logs кричат, но клиент думает stream живой.
  **Что нужно добавить:**
  - Heartbeat handler при PG-error возвращает `HeartbeatOut{server_unhealthy: true}` (новое поле) — клиент закрывает stream и реконнектится через 30s+jitter.
  - В Subscribe loop при LISTEN connection lost — послать клиенту `Command::ServerDraining` и Close.
  - `bosun_pg_alive` gauge exposed через `/ready` — при PG down k8s через 3 failed checks убирает pod из endpoints.

- **Issue:** Pod застрял на SIGTERM. k8s force-kill через `terminationGracePeriodSeconds`. Записи в `bosun_pods` (last_ping_at 5-10 секунд назад) остаются — другие pods считают его живым ещё ~30s.
  **Что нужно добавить:** Pod health detection — все pods чекают `bosun_pods` каждые 5s, если `NOW() - last_ping_at > 15s` → DELETE с advisory_lock (защита от double-delete двумя pods).

- **Issue:** Vault недоступен. VaultGet возвращает Internal. Что делает клиент — не описано.
  **Что нужно добавить:**
  - Server возвращает `code=Unavailable` (не Internal).
  - bosun-client retries 3× exp backoff (5s, 30s, 2m).
  - После 3 fails → ReportApplyResult `outcome=DEFERRED, reason=VaultUnavailable`.

- **Issue:** Network partition pod ↔ PG. Pod без PG, но клиенты подключены. Streams живые, команды не доходят.
  **Что нужно добавить:** `bosun_pg_alive` false 2 цикла подряд → `/ready=503`. k8s убирает из endpoints, активные streams не убиваются (новых не получит). Запускается graceful drain.

- **Issue:** Все 3 pod'а драинят одновременно (recreate deployment). `len(active_pods) = 0`, hashing ломается, клиенты получают Redirect → самих pods → бесконечный loop.
  **Что нужно добавить:** При `len(active_pods) <= 1` НЕ слать Redirect, оставить на текущем pod'е даже если draining.

- **Pro:** Draining через DB-record (pull model) работает даже когда NOTIFY broken (`2026-05-23-bosun-api-p0-proto.md:611-621`).

### E. Security / compliance

- **Issue (P0):** ECDSA replay window. `2026-05-23-chiit-server-codegen-and-scratch.md:270` использует `ClientMaxDiff` — default не указан в спеке. На 60k клиентов compromised host может за окно послать сотни запросов с одной подписью.
  **Что нужно добавить:**
  - Уточнить `ClientMaxDiff` в спеке, рекомендация 60s.
  - Per-host rate limit interceptor: 5 req/sec.
  - Subscribe stream — auth once на установлении, не на каждое сообщение.

- **Issue:** Key revocation race. ECDSA validator кеширует public_keys ~30s. DELETE из chiit_validators — окно компрометации 30s.
  **Что нужно добавить:** RPC `RevokeNode(host)`:
  - DELETE FROM chiit_validators
  - INSERT INTO chiit_revoked_keys (для audit)
  - Cache invalidate через NOTIFY на все pods.
  - Если активный Subscribe от host_X есть — Close немедленно.

- **Issue:** Secrets в bundle. PublishBundle валидирует только sha256 и signature. Inline password в Starlark не блокируется.
  **Что нужно добавить:** Pre-publish scan на patterns: `BEGIN PRIVATE KEY`, `password=`, `secret=`, `BEGIN RSA`, `aws_secret_access_key`, `eyJ` (JWT prefix). Список в realtime-config. Найдено → `InvalidArgument: bundle contains potential secrets`.

- **Issue:** mTLS на gRPC :82. `2026-05-23-bosun-api-p0-proto.md:14` — «warden закрывает в кластере». Но 60k клиентов **вне k8s** (физика, KVM). Warden между ними не действует.
  **Что нужно добавить:** TLS termination на ingress с self-signed CA chiit-server'а. `BootstrapOut.ca_bundle_pem` уже есть в proto (помечен P0 в `chiit-server/api/bosun/v1/bosun.proto` sketch L149-156), но добавлен только в research — в proto его нет.

- **Issue:** Audit granularity. `chiit_audit_log` упомянут в диаграмме (`2026-05-23-bosun-deployment-diagrams.md:51`), но что туда пишется при IssueCommand / Bootstrap / PublishBundle — не описано.
  **Что нужно добавить:** Список событий:
  - `bosun.bootstrap.success {host, key_fingerprint, replaced_key_fingerprint, token_id, ip}`.
  - `bosun.issue_command {operator, command_id, target_hosts, payload_kind}`.
  - `bosun.issue_rollout {operator, rollout_id, target, payload, failure_rate}`.
  - `bosun.bundle.publish {ci_account, version, sha256, size, tags}`.
  - Retention 90 days minimum (Ozon compliance может потребовать 3 года).

- **Issue:** GDPR. `issued_by` = AD-login оператора (PII). Hostname — не PII.
  **Что нужно добавить:** Либо pseudonymize `issued_by` через hash + lookup с retention 30d, либо explicit statement «Ozon — RU, не EU GDPR subject».

### F. Operational tooling

- **Issue (P0):** Debug operator-стороны не описан. `GetNodeStatus` отдаёт last_apply, но «история команд на host_X за 24h», «hosts c reason=DpkgLocked за час», «pending defers для cluster Y» — нет.
  **Что нужно добавить:** RPC для оператора:
  - `GetCommandHistory(host, since, kind) -> stream Command`.
  - `QueryFailures(filter, since) -> stream Failure`.
  - `GetPendingDefers(cluster_name) -> map<host, defers>`.

- **Issue:** Bulk-debug «все ноды кластера X». IssueRollout dry_run — не для debug.
  **Что нужно добавить:** `ListNodes(target_selector, fields[]) -> stream NodeSummary` — read-only.

- **Issue:** PDB не описан. `kubectl rollout restart` идёт one-by-one, но `kubectl delete pod --all` снесёт все одновременно. drain_timeout не успеет.
  **Что нужно добавить:** `PodDisruptionBudget minAvailable: 2`. Hard delete `--force` обходит — сознательный выбор оператора.

- **Issue:** Просмотр live streams. Сейчас только через PG (`SELECT * FROM bosun_active_sessions`) или channelz.
  **Что нужно добавить:** `ListActiveStreams(pod_id_filter) -> stream StreamInfo`.

- **Issue:** Emergency stop. AbortRollout per-rollout, но что если их 5 одновременно?
  **Что нужно добавить:** Global pause flag в realtime-config `bosun_dispatch_paused: true`. Dispatcher worker читает каждую итерацию.

- **Pro:** Bundle CLI отсутствует — правильно, ручная заливка антипаттерн (`2026-05-23-bosun-api-p0-proto.md:309-317`).

### G. Failure modes / chaos testing

- **Issue:** Chaos checklist не описан.
  **Что нужно добавить:** Pre-go-live:
  1. pod crash mid-rollout — consistent hashing rebalance, idempotency через command_id.
  2. PG primary failover — streams ловят LISTEN error, шлют ServerDraining клиентам.
  3. Spam Bootstrap с broken ECDSA — rate-limit 5 Bootstrap/min per IP.
  4. rollout failure_rate=0 на 60k и первый же fail → halt ≤ 5s.
  5. Bundle 50MB × 100 concurrent GetBundleBlob → PG bytea-stream pool не сломается.
  6. Network partition pod ↔ PG → readiness false → uplifted from endpoints.

- **Issue:** Idempotency для дубликатов ReportApplyResult. command_id UUID есть, но обработка дубля не описана.
  **Что нужно добавить:** UPDATE `bosun_commands_queue.acked_at` через `WHERE acked_at IS NULL`. Если уже acked — log в audit как duplicate, no-op. Для `bosun_rollouts.acked_targets++` — `UPDATE WHERE command_id NOT IN (already_acked_set)`.

- **Issue:** IssueCommand на 60k hosts батчем. 60k INSERT блокирует, 60k NOTIFY переполнят queue.
  **Что нужно добавить:** Chunked INSERT (1000-host batches с commit между). IssueCommandOut `{accepted=true, total_hosts=60000, status_uri}` — 202 Accepted, polling. IssueRollout уже решает через dispatcher, но IssueCommand на 60k тоже опасен.

### H. Migration plan chiit → bosun

- **Issue (P0):** Migration plan 60k нод не описан. Диаграмма `2026-05-23-bosun-deployment-diagrams.md:434-499` показывает только одну ноду.
  **Что нужно добавить:**
  - **Phase 0 (готовность):** chiit-server v2 deployed. 0 нод на bosun. Все 60k на chiit-client.
  - **Phase 1 (canary 1%):** 600 нод severity=low в staging. Apt-package доставляет bosun-binary, токен уже на ноде. Оператор stop chiit → start bosun. 24h observation: ack-rate, failure-rate, GC pauses.
  - **Phase 2 (5%):** 3000 нод mix-severity. 1 неделя observation.
  - **Phase 3 (20% → 60% → 100%):** Постепенно, 1 неделя между фазами.
  - **Откат:** `systemctl start chiit-client` — chiit-client всё ещё установлен, chiit-server видит обоих.
  - **Decommission deadline:** 3 месяца после 100% — chiit-client удаляется из apt-repo.

- **Issue:** Параллельная работа chiit-cron и bosun-systemd. «Один ECDSA-key через chiit_validators», но что если оба делают apt-install/remove одновременно?
  **Что нужно добавить:** bosun-binary при старте чекает `systemctl is-active chiit-client` → если активен, refuse start. Enforce exclusivity.

- **Pro:** Бесшовный bootstrap через тот же registration_token — большой плюс UX (`2026-05-23-bosun-deployment-diagrams.md:498`).

### I. Capacity / cost

- **Issue:** PG sizing не описан. После bosun-API:
  - `bosun_heartbeats`: 60k rows × 100 bytes = 6 MB, UPDATE 2k/sec → ~5GB WAL/day.
  - `bosun_commands_queue`: peak 60k pending × 4KB JSONB = 240 MB peak.
  - `bosun_bundles`: 50 MB × ~30 versions = 1.5GB, growing.
  - `bosun_active_sessions`: 60k × 200 bytes = 12 MB.
  - Total steady-state: 2GB data, 10GB indexes, 5GB/day WAL.
  **Что нужно добавить:** Min PG sizing 8 cores / 32GB RAM / 500GB SSD / 100GB WAL retention. Real sizing — после Phase 1.

- **Issue:** Network bandwidth при burst rollout. 1500 clients одновременно GetBundleBlob (50MB) = **750 GB transfer**. Pod outbound limit 1 Gbps → 1.5 часа.
  **Что нужно добавить:** CDN-fronting GetBundleBlob через S3 / nginx-cache. `blob_ref = "s3://chiit-bundles/sha256=XXX"`. PG-bytea только для immutable record / forensics.

- **Issue:** Pod budget не описан.
  **Что нужно добавить:** `replicas: 3` min, `autoscaling.maxReplicas: 6`, custom HPA metric (bosun_active_streams_per_pod).

### J. Что упустил архитектор

Пункты не описанные в спеках, критичные для production-ops:

1. **`GetBundleBlob` не в P0.** В `2026-05-23-bosun-api-p0-proto.md:861-870` он в P1 follow-ups. Без него `ApplyBundle` — пустая команда. Это блокирует MVP.

2. **Versioning proto.** `bosun.v1` — что про v2? Как мигрировать активные streams? Нужен план параллельного `v2` с deprecation period.

3. **PG connection pool sizing.** chiit-server `pool_size: 5` (`values.yaml`). На 20k streams × LISTEN — нужен отдельный LISTEN pool с 20k+ connections. PG default `max_connections=100`. PgBouncer? Shared LISTEN на один pool через demux?

4. **Backpressure в Subscribe.** Медленный клиент → буферы gRPC растут → pod OOM. Нет rate-limit per-host (max 1 unacked command в полёте).

5. **Stale active_sessions cleanup.** Pod Y умер без `disconnected_at`. Reaper job нужен — в плане не указан.

6. **Scheduled tasks.** chiit-server имеет partition management (`migrations/06_manage_partitions.sql`). bosun нужно: cleanup bundles, expire rollouts, vacuum heartbeats, prune audit_log. Где запускается? k8s CronJob? Sidecar?

7. **Auth CI service-account.** `published_by` (`2026-05-23-bosun-api-p0-proto.md:319-323`) — какой механизм? admin-token shared? Keycloak service-account?

8. **Time skew.** ECDSA `createdAt` валидируется против `time.Now()`. NTP-broken нода → все запросы `ErrClientTokenTooOld`. Нужна метрика `bosun_client_time_skew_seconds` и алерт.

9. **DNS для bosun-client вне k8s.** `chiit-server-headless.infra.svc:8443` — internal DNS, нерезолвится с 60k физика/KVM нод. Нужен external `chiit-server.s.o3.ru:8443`.

10. **startupProbe vs mature-threshold race.** `values.yaml startupProbe: initialDelaySeconds: 0` — pod в endpoints через 5-10s. Mature threshold 20s — между этим окном клиенты идут на pod_new, но в hashing-ring его ещё нет → бесполезные Redirect'ы.

11. **CLI оператора.** `chiit-server-cli` есть. Новый `bosun-cli`? Extension? Operator tooling не описан.

12. **Disaster Recovery.** chiit-PG — WAL-G backups? RPO/RTO? rollouts in-flight теряются при disaster — playbook recovery?

## Топ-10 must-fix перед началом реализации

1. **P0** — single-writer гарантия для rollout dispatcher. Использовать `pg_try_advisory_lock(BIGINT 'bosun.dispatcher')` в worker'е каждом цикле. Без этого 3 pods будут параллельно dispatch'ить один rollout → дубли в commands_queue.

2. **P0** — bundle blob storage policy. Либо переезд на S3/OCI с PG-метаданными, либо строгая cleanup-policy + WAL impact оценка. Без этого через год PG bloat станет блокером.

3. **P0** — migration plan chiit → bosun (раздел H выше). Без явного rollout-плана 60k нод нельзя начинать.

4. **P0** — NOTIFY throttling в dispatcher. 1500 NOTIFY/sec может переполнить pg_notification queue (8GB shared). Должен быть rate-limiter.

5. **P0** — `GetBundleBlob` поднять в P0 scope. Без него `ApplyBundle` не работает.

6. **P0** — PG-failure handling в Subscribe/Heartbeat. Сейчас не описано что клиент видит при PG down. Нужно явное протокольное поведение (ServerDraining, Unavailable).

7. **P1** — Reaper для stale `bosun_active_sessions`. Запуск каждые 60s, чистит записи где pod_id не в `bosun_pods`.

8. **P1** — k8s deployment spec: `terminationGracePeriodSeconds: 60`, `PodDisruptionBudget minAvailable: 2`, `replicas >= 3`, custom HPA metric.

9. **P1** — observability backbone: метрики, dashboards, SLO, алерты на halt и auth_failed rate.

10. **P2** — operator emergency tooling: global pause flag, RevokeNode RPC, ListActiveStreams RPC.

## Что особенно хорошо

1. **Bosun-server = chiit-server.** Одно процессное пространство, общая команда, общая auth-инфраструктура — это снимает 80% операционной сложности по сравнению с отдельным Rust-процессом. Не нужны два set'а dashboards, runbooks, on-call procedures (`project_bosun_server_language_choice.md`).

2. **Pod redirect через consistent hashing.** Locality для cluster-bound операций даст хороший cache hit-rate. И механика graceful shutdown через DB-record `draining=TRUE` элегантна и работает cross-replica (`2026-05-23-bosun-api-p0-proto.md:611-621`).

3. **Mature-threshold 20s.** Защита от crashloop'ов нового pod'а с reshuffle-штормом — это вдумчивое решение которое легко упустить (`2026-05-23-bosun-api-p0-proto.md:630-635`).

4. **Rollout state machine с failure_rate halt.** PENDING → RUNNING → HALTED с явным `max_failure_rate` — это лучше чем chiit'овый `canary_hash%` на стороне клиента. Сервер видит fleet, может остановить раскатку до того как сломается весь парк (`2026-05-23-bosun-api-p0-proto.md:475-481`).

5. **Bundle публикация только через CI/CD.** Ручной оператор не имеет CLI для upload — это исключает класс инцидентов "оператор залил кривой bundle вне версионирования" (`2026-05-23-bosun-api-p0-proto.md:309-317`).

6. **Heartbeat и Active sessions — раздельные таблицы.** Разные write patterns, разные retention, разные индексы. Хороший proactive выбор (`2026-05-23-bosun-api-p0-proto.md:784-813`).

7. **Бесшовный bootstrap через ECDSA-key rotation.** Один и тот же `registration_token`, одна и та же таблица `chiit_validators`, плавный переход chiit → bosun без admin-intervention. UX переезда сильно лучше любого ручного re-registration (`2026-05-23-bosun-deployment-diagrams.md:430-499`).

## Open questions для архитектора

1. **Где хранить bundle blobs в production?** PG-bytea на 50MB — это технический debt. S3 / OCI registry / nginx-cached HTTP? Без ответа нельзя считать бюджет PG.

2. **NOTIFY vs polling.** PG NOTIFY на burst 1500 events/sec — рискованно (queue limit 8GB shared). Альтернатива — pods polling каждые 500ms. Что выбираем?

3. **HPA метрика и pod-cap.** Какой максимум streams на pod? 20k? 30k? Это определит memory budget и HPA upper bound.

4. **PG sizing и whether отдельный кластер.** bosun-нагрузка может потребовать выделения отдельного PG для chiit-server'а. Или ужать в общий и tolerate 5GB/day WAL?

5. **TLS termination для bosun-client'ов вне k8s.** Сейчас insecure :82. Где терминируем TLS для 60k external clients? Ingress / dedicated LB?

6. **CI auth для PublishBundle.** admin-token / Keycloak service-account? Static or rotated?

7. **Migration plan deadline.** Когда chiit-client считается decommissioned? Через 3 мес после 100% bosun? Через 6?

8. **Disaster Recovery.** Если PG cluster сгорел целиком (DC down) — RPO/RTO? Все rollouts in-flight теряются?

9. **GDPR / audit retention.** 90 days enough? У Ozon RU compliance — 3 года для аудита изменений на production-инфре?

10. **Operator CLI roadmap.** Когда появится `bosun-cli` для эмержанси-команд (RevokeNode, PauseGlobal, ListStreams)? До Phase 1 migration или после?

11. **Multi-DC / multi-region.** chiit-server сейчас single-DC. bosun-парк может быть в нескольких DC. Cross-DC latency для Subscribe? Отдельные pod'ы в каждом DC?

12. **Что считается «pod ready» для consistent hashing.** k8s readinessProbe vs mature-threshold 20s — два разных условия. Нужно ли их синхронизировать?
