# Архитектор-review плана bosun-server (внутри chiit-server)

Date: 2026-05-23
Reviewer: Staff engineer, projects architect perspective
Scope: P0 proto sketch + deployment diagrams + два предшествующих research'а

## TL;DR

План реализуем и operationally здравый, но содержит четыре долгоиграющих риска. Bundle blob в PG без archival к 2-3 годам станет SLO-проблемой. `BosunAPI` как fat-service с 14-24 RPC и тремя разными natures of operations (session / atomic / orchestration) хоронит будущее разделение. Coupling «один процесс с chiit-server» делает аварии бимодальными — любой incident в chiit бьёт по 60k bosun-стримам. Versioning без deprecation policy и три stringly-typed enum'а в P0-proto превратятся в breaking change в первом же `v2`. Pod redirect через `crc32 % active_pods` — не consistent hashing, каждый scale-event подбрасывает заметную долю клиентов.

## Findings по категориям

### A. Domain boundaries / cohesion

В `BosunAPI` смешаны **четыре домена**, каждый со своей моделью данных и lifecycle:

1. **Identity / PKI**: `Bootstrap`, `RotateCert`, `RevokeCert`. Lifecycle: weeks/months.
2. **Session**: `Subscribe`, `Heartbeat`, `ReportApplyResult`. Write-heavy hot path, minutes/hours per session.
3. **Orchestration**: `IssueCommand`, `IssueRollout`, `Pause/Resume/Abort/GetRolloutStatus`. State-machine, hours/days.
4. **Bundle catalog**: `GetBundleManifest`, `PublishBundle`, `GetBundleBlob`, ... INSERT-only registry, lifecycle forever.

Плюс **proxy-ручки** (`VaultGet`, `GetCert`, `StorageHostInventoryGet`) — это не bosun-домен, а thin delegates в chiit handlers. Их слияние в один service создаёт впечатление, что bosun «владеет» Vault и inventory'ем.

В `internal/bosun/` запланирован один пакет на всё: `bootstrap.go`, `subscribe.go`, `heartbeat.go`, `report_apply.go`, `vault.go`, `cert.go`, `inventory.go`, `bundle.go`, `issue_command.go`, `interceptors.go`, `sessions.go`. Через 6-12 месяцев и +10 RPC он превратится в монолитный пакет без семантических границ.

**Предложение**: разделить с самого start на `internal/bosun/sessions/`, `rollouts/`, `bundles/`, `proxy/`. Identity — thin layer над `internal/validator/`. Дальше — split proto на `BosunSessionAPI`, `BosunRolloutAPI`, `BosunBundleAPI`, `BosunOpsAPI`, `BosunInventoryAPI` (см. F).

Аргумент против сейчас: «один service = один auth». Но уже сейчас оператор (admin-token / Keycloak JWT) и агент (ECDSA) — разные auth-модели, склеенные fallback'ом внутри каждого handler'а. Это **не упрощение**, это сложность, размазанная по коду. Split упростил бы interceptor-маршрутизацию.

### B. Contract stability / evolution

Package versioning есть (`...chiit.bosun.v1`), но **deprecation policy отсутствует**. Это критично — bosun-client живёт apt-package'ом на 60k нод, через 18 месяцев в полях три поколения бинарей.

**Конкретные хрупкости в P0:**

1. **`oneof Command.payload`** — добавление 5-го варианта (`DrainNodeCommand`, `RotateAgentCertCommand`) ломает старые клиенты (prost даст `None`, клиент молча игнорирует). Нужно: либо явный protocol «`UnsupportedCommand` reply через `ReportApplyResult`», либо `unknown_payload_type_seen` counter в Heartbeat.

2. **`enum RolloutState`** — добавление `SUSPENDED_FOR_MAINTENANCE` ломает CLI операторов. Best practice: задокументировать «unknown = treat as terminal», ввести parallel `state_class` (`active / paused / terminal`).

3. **`enum FailureMode { STRICT, PARTIAL }`** — двух значений мало уже сейчас, нужен `HARD` (defer = failure). Расширять `PARTIAL` потом — ломаем семантику.

4. **`ReportApplyResultIn.last_apply_result = string`** — stringly-typed enum, который должен был быть `enum ApplyOutcome { UNSPECIFIED, SUCCESS, PARTIAL, FAILED, DEFERRED, INTERRUPTED }`. Через год клиенты будут слать произвольные строки, сервер парсить.

5. **`Target.canary_percent = string`** «чтобы поддержать 10.5 в будущем» — premature optimization не в ту сторону. На 60k нод 1% точности достаточно (1% = 600 нод). Сделать `uint32`, при необходимости sub-percent добавить `permille` field.

6. **Нет `client_request_id`** на `IssueCommand` / `IssueRollout` / `PublishBundle`. Network retry создаёт duplicates. Добавить сейчас, не после первого incident'а.

**Policy на будущее**:
- Все enum'ы с `UNSPECIFIED = 0` + комментарием «client must treat unknown as UNSPECIFIED».
- Все поля additive only до `vN+1`.
- `v2` рядом с `v1`, не вместо. P0-RPC стабильны 18+ месяцев.
- Реестр `BOSUN_API_DEPRECATION.md`.

### C. Coupling between bosun-server and chiit-server

Coupling абсолютное на четырёх уровнях: process, DB schema (общая `chiit_validators`), code base (`cmd/server/main.go`), auth model (общая ECDSA-инфраструктура).

В моменте — правильно (бесшовный переход клиентов). **Не масштабируется на 2-3 года** по трём причинам:

**Load shape принципиально разный.** chiit-client — 2k RPS unary, p50 ~5ms. bosun-client — 60k long-lived streams + 2k RPS write + bursts of NOTIFY-driven dispatch. grpc-go держит ~96 KB per connection — 60k × 96 KB = 5.7 GB только на streams. GC pause на таком heap'е был **главным аргументом за Rust** в research'е 2026-05-20, который отменили 2026-05-22. Проблема никуда не делась, она теперь живёт в chiit-server'е. 200ms GC pause в chiit-server'е одновременно задержит heartbeat ack'и и chiit pull requests — **bimodal failure**.

**Cadence релизов разный.** chiit — стабильный legacy, риск-aversion. bosun — новая поверхность, активная разработка. Один deploy artifact = bosun-fix требует rebuild всего chiit-server'а.

**Общая ECDSA-таблица фиксирует контракт навечно.** Сейчас удобно. Через 12 месяцев, когда chiit декомиссирован, таблица всё ещё «общая», хотя владелец один. Если bosun потребует отдельной auth-модели (90-day cert, OCSP, per-cluster CA) — fork с миграцией или feature-flag в одной таблице. Оба дороже превентивного namespacing'а.

**Предложение**: зафиксировать в ADR **trigger condition для split**: «при decommission последнего chiit-client» либо «при p99 chiit-latency > 100ms по причине bosun load». Уже сейчас:
- Новые поля для bosun — в `bosun_node_attributes` через FK на `chiit_validators.host`, не в общую таблицу.
- Rate-limit interceptor на bosun path'ах **с самого начала** — SLA chiit (1ms p50, 50ms p99) задокументировать как жёсткое требование, bosun не имеет права съесть >40% latency budget'а.
- Запланировать flavor Helm с отдельным bosun-only pod (та же кодовая база, отдельный process).

### D. Data lifecycle

Раздел с наибольшим числом упущений в плане.

**`bosun_bundles` blob — immutable forever.** ~50 MB per bundle × 5 публикаций/день × 5 лет = **~450 GB blob'ов**. PG TOAST технически выдержит (до 1 GB на field), но:
- Backup chiit-DB: 50 GB → 500 GB. RPO/RTO деградируют.
- `pg_dump` / restore становится disruptive.
- WAL volume на каждый INSERT 50 MB → replication lag.
- `GetBundleBlob` cold-cache на 60k агентов параллельно — IOPS spike.

Стратегия archival отсутствует. **Предложение**: сразу в P0 заложить `archived_at TIMESTAMPTZ`, `blob_ref TEXT` (NULL для inline blob, иначе `s3://`). Policy: archived if `retracted=true AND published_at < NOW() - INTERVAL '90 days'`. Через 12-18 месяцев — все новые в S3, PG-blob legacy.

**`bosun_commands_queue` без retention.** 5k команд/день × 365 × 3 года = 5.5M записей × ~500 байт JSONB = ~3.2 GB. Index на `(host) WHERE delivered_at IS NULL` не покрывает audit queries. Появятся ad-hoc индексы → bloat → degradation. **Предложение**: partitioning by month с самого начала (`pg_partman`), дроп партиций через 12 месяцев, terminal state в отдельный `bosun_command_audit` (без payload, только hash + outcome).

**`bosun_heartbeats` hot row update каждые 30s.** Без cleanup через 90 дней без heartbeat'а row остаётся forever. TTL job или partition by `heartbeat_at`.

**`bosun_pods` для SIGKILL'нутых.** Graceful delete — есть, force kill — нет cleanup. Нужен job: `DELETE WHERE last_ping_at < NOW() - INTERVAL '1 minute'`.

**`bosun_rollouts.target_spec` JSONB.** Развёрнутый target (5000 hosts) = ~250 KB. Не критично, но нормализовать: хранить filter expression, expansion лежит в `bosun_commands_queue`.

### E. Failure mode boundaries

**`IssueRollout` транзакционность не описана.** Steps: expand target → INSERT bosun_rollouts → INSERT batch commands_queue → NOTIFY per-host. Если в одной транзакции — на 5000 INSERT lock contention + WAL spike. Если в разных — что при crash между 2 и 4? Background dispatcher подберёт через 5s (благодаря idempotency «expected - actual»), но это **не описано**.

**Dispatcher leader crash midway.** Через `pg_advisory_lock`, освобождается на disconnect. Новый leader считает `dispatched_targets` из queue, продолжает diff. **Работает корректно**, но: между INSERT и NOTIFY если crash — записи висят с `delivered_at IS NULL`, никто не получает push. Нужен **fallback poller на стороне Subscribe-handler'а**: каждые 30s `SELECT WHERE host=$self AND delivered_at IS NULL`. В proto не упомянуто.

**Storage-inventory cache staleness при rollout expansion.** Cache TTL 60s типичен. Сценарий:
- 12:00:00 — N1 reclassified medium → high в inventory.
- 12:00:10 — `IssueRollout target=medium`, cache ещё видит N1 как medium.
- 12:00:30 — cache refresh, поздно — bundle применился на N1.

**Предложение**: rollout = **first-class entity со snapshot'ом target'а** на момент create. `bosun_rollouts.target_spec_snapshot` хранит **полный list of host_ids**, никаких re-resolution'ов. Это и audit, и предсказуемость.

**`ReportApplyResult` partial semantics неоднозначна.** `last_apply_result="partial"` с `exit_code=0` (gracefully degraded) — это success в `FailureMode.STRICT`? Очевидно да, но не очевидно из proto. Нужен ADR.

### F. API surface size

P0 = 14 RPC, +P1 = 23, +follow-ups (ListBundles, RetractBundle, RotateCert, RevokeCert, ReportLogs, GetNodeStatus, GetAuditLog, GetBundleBlob, PublishBundle) ≈ **28 RPC** в одном service'е через год.

Не критично для gRPC (service может содержать 100 RPC), **критично для cognitive load и testability**. Service granularity лучшая практика:

```
BosunSessionAPI:   Bootstrap, Subscribe, Heartbeat, ReportApplyResult
BosunBundleAPI:    PublishBundle, GetBundleManifest, GetBundleBlob, ListBundles, RetractBundle
BosunRolloutAPI:   IssueCommand, IssueRollout, Get/Pause/Resume/AbortRollout
BosunOpsAPI:       GetNodeStatus, GetAuditLog, WatchRolloutStatus
BosunInventoryAPI: VaultGet, GetCert, GetRSAPairs, StorageHostInventoryGet (proxy thin)
```

Плюсы: каждый сервис — свой auth, свой rate-limit, свои метрики, независимая evolution, естественные boundaries для будущего split deployment'а.

**Streaming.** Сейчас единственный stream — `Subscribe`. Кандидаты на streaming: `GetBundleBlob` (P1), `ReportLogs`, `WatchRolloutStatus` (вместо polling `GetRolloutStatus`). Последний — **существенное** UX-улучшение для оператора при rollout'е 30+ минут. Закрепить policy: streaming для >1 MB payload или >5 events/sec.

### G. Testability

В планах **test strategy не упомянута** — критичное упущение.

**Unit-тесты handler'ов**: для `IssueRollout` потребуются моки PG, chiit cache (Vault, storage-inventory), validator, sessions registry, pods registry, dispatcher — 5-6 моков на один handler. Тесты будут хрупкими.

**Предложение**: rollout-state-machine pure-function-style (вход — current state + ack event, выход — new state + side-effect descriptor). State machine тестируется без моков. Dispatcher вызывает state machine с реальной PG.

**Integration**: spawn fake bosun-client (Rust) или Go-stub'ы, симуляция Subscribe streams, IssueRollout → проверить commands_queue + push. Не описано. Минимум — golden test full rollout с N=10 fake клиентов и varied failure_rate.

**Contract tests Rust ↔ Go**: prost и protoc-gen-go-vtproto могут разойтись на `bytes` vs `Vec<u8>`, defaults, `oneof` empty case, Timestamp precision. **Golden suite** — Rust→Go и обратно, byte-identity + semantic-equivalence. Хранить в `tests/proto-golden/v1/*.bin`. Без этого первый production-разлад появится в самом непредсказуемом месте.

**Chaos testing**: что при потере PG NOTIFY (replica failover)? Что при истечении drain_timeout с 100 живыми streams? Что при `Redirect` на pod который не отвечает (DNS lag)? Failure-mode test plan отсутствует.

### H. Documentation / onboarding

Сильные стороны: proto-sketch с детальными комментариями, lifecycle diagrams, ADR-style решения с цитатами пользователя.

**Чего не хватает**:
- **Operator runbook**: «как делать rollout», «как разруливать halted state», «что при failure_rate=80%».
- **Developer onboarding doc**: где BosunAPI handler'ы, как добавить RPC, testing checklist. Новый разработчик должен понять boundaries за пару дней.
- **ADR registry**: «bundle = PG bytea», «no self-upgrade», «общая ECDSA-таблица», «mature_threshold=20s» — сейчас разбросаны по research'ам и memory-файлам.

### I. Evolution scenarios

**Bundle size growth 5 MB → 200 MB через 2-3 года.** PG bytea технически выдержит, но `GetBundleBlob` cold-cache на 60k агентов = 4s × 60k = disruption + WAL 200 MB per INSERT.

Path: P0 — PG до 50 MB OK. +6 мес — `blob_ref` поддерживает S3. +12 мес — auto-offload в S3 при размере > 100 MB. +24 мес — все новые в S3, PG legacy.

**Rollout complexity.** Через год: cluster-aware (master/replica), dependency graph (bundle X требует Y), per-DC rate limit. Текущая state machine плоская, не масштабируется. Не делать сейчас, **зафиксировать в ADR**: «state machine v1 — простая; для cluster-aware будет v2 с подэтапами».

**Multi-DC.** Если PG разные в DC1/DC2 — два изолированных pod-кластера. Federation проще всего (per-DC bosun-server, оператор зовёт оба через CLI обёртку). Зафиксировать в ADR.

**Self-upgrade.** Архитектура поддерживает (не блокирует), но если требование разворачивается — нужен `UpgradeBinary(version, sha256, url)` RPC. Не critical сейчас.

### J. Что упустил архитектор

15 missing aspects:

1. **Backpressure на Subscribe stream.** Медленный клиент → `stream.Send()` блокируется → handler goroutine залип. Нужно ограниченная очередь per-session + timeout.

2. **Order semantics команд.** Два rollout'а на тот же host — порядок? `issued_at` с clock skew на parallel pods может перепутать. Нужен monotonic sequence.

3. **Idempotency на operator RPC.** `client_request_id` отсутствует. Network retry → duplicates.

4. **Cancellation семантика.** `AbortRollout` — что с already delivered, but not acked? Нужен `CancelCommand(command_id)` или флаг «server-aborted, ignore on pickup». В proto нет.

5. **Observability на rollout.** Глобальный view через PG aggregations. Где dashboard? bosun-specific метрики (`bosun_rollout_in_flight`, `bosun_failure_rate`) не описаны.

6. **Audit policy.** Какие события явно логируются в bosun-flow? Cert rotation в proto есть, rollout halted — где? Audit-policy не описана.

7. **Bootstrap rate limit / DDoS.** Если `registration_token` утёк — attacker regenerate cert'ы массово. Без TTL и без rate-limit. Нужен либо TTL, либо rate-limit, либо human approval.

8. **gRPC keepalive parameters.** Open question #2 в proto. 60k long-lived streams без keepalive — TCP RST через 5-10 минут от firewall'ов / NAT.

9. **Connection limits per host.** Bug в bosun-client → 50 streams вместо одного. Сейчас «новый отменяет старый», но через 24h может уронить pod FD exhaustion'ом. Hard cap в session registry.

10. **Cert renewal flow.** 60k нод × 1-year cert = ~165 expirations/день. `RotateCert` — nice-to-have. **Должен быть P0**, иначе через год coordinated cert-renewal — disaster.

11. **Webhook / external integrations.** Rollout fails — никаких alerts (Slack, PagerDuty). Типичный gap «P0 без alerting», проявится в первый incident.

12. **Capabilities semantics.** `SubscribeIn.capabilities = ["runr.service"]` — что с этим делает сервер? Filter команд? Audit? Runtime contract не описан.

13. **gRPC server-side error budget.** `max_concurrent_streams`, `max_message_size` не указаны. На 60k streams default'ы grpc-go будут резать клиентов.

14. **Dev/CI test environment.** Где разработчик тестирует BosunAPI без production chiit'а? Нужен `docker-compose.dev.yaml` с minimal stack + fake bosun-client.

15. **Migration success criteria.** Когда декомиссируется первый chiit-client → bosun-client — что green light? Не описано.

## Топ-10 архитектурных рисков с mitigation

**P0 (срочно, до начала implementation)**:

1. **Stringly-typed enum в `ReportApplyResult.last_apply_result`** (B). Mitigation: заменить на `enum ApplyOutcome` сейчас. Через год дороже.

2. **Bundle blob без archival** (D). Mitigation: добавить `archived_at`, `blob_ref`, ADR об S3-offload после 90 дней `retracted`.

3. **Отсутствие `client_request_id` на operator RPC** (J). Mitigation: добавить в `IssueCommandIn`, `IssueRolloutIn`, `PublishBundleIn`.

4. **`RotateCert` не P0** (J). Mitigation: переместить из nice-to-have. Через 365 дней — coordinated cert-renewal на 60k агентов будет disaster без него.

5. **`internal/bosun/` как монолитный пакет** (A). Mitigation: с самого start разделить на `sessions/`, `rollouts/`, `bundles/`, `proxy/`.

**P1 (важно, до окончания P0)**:

6. **Storage-inventory cache staleness при rollout expansion** (E). Mitigation: snapshot target'а в `bosun_rollouts.target_spec_snapshot` (полный list of host_ids), zero re-resolution.

7. **`crc32 % len(active_pods)` reshuffle** (C). Mitigation: jump-consistent-hash (jchash). При 60k клиентах reshuffle 50% = 30k одновременных reconnect'ов — нужен stagger или jchash.

8. **Contract tests Rust↔Go отсутствуют** (G). Mitigation: golden proto serialization suite до первого release'а.

**P2 (важно, до 12 месяцев)**:

9. **Coupling chiit-server / bosun-server** (C). Mitigation: SLO monitoring разделения load'ов; trigger для split deployment'а в ADR; bosun rate-limit с самого start.

10. **`bosun_commands_queue` без retention** (D). Mitigation: partitioning by month через 6 месяцев. Archive partition через 12 месяцев.

## Что особенно хорошо

1. **«bosun-server = chiit-server» в моменте**: минимизирует deployment complexity, переиспользует scratch, audit, Vault, cert manager. Для P0 — идеальный trade-off speed vs purity.

2. **`Subscribe` единственный stream + остальное unary**: идиоматично для control plane (etcd watch + lease, k8s informer pattern), не reinventing.

3. **Bundle immutable с BIGSERIAL version**: INSERT-only, audit-friendly, bug-in-bundle = new version, не editing старой.

4. **Pod redirect через Subscribe-команду, не L4 трюки**: protocol-level redirect, debuggable, observable, не зависит от k8s feature'ов.

5. **Mature_threshold 20s** для нового pod'а перед routing: защита от crashloop flapping. Конкретная инженерная защита, такие детали часто упускают.

6. **Rollout state machine с `PAUSE / RESUME / ABORT`**: operator-controllable, явные terminal states, audit-friendly.

7. **`failure_rate` halt с `FailureMode`**: главная защита от cascading apply-storm'ов в production. Decision-fail-stop с настраиваемой строгостью.

## Open questions для design owner'а

1. **Когда допустим split bosun-server в отдельный deployment?** Trigger condition в ADR. Я предлагаю: «при decommission последнего chiit-client» или «при p99 chiit-latency > 100ms по причине bosun».

2. **Retention для bundle blob в PG.** Сколько лет хранится, что мигрирует в S3?

3. **`registration_token` lifecycle.** Shared per-park forever или per-env rotation? TTL?

4. **`crc32 % len(active_pods)` или jchash?** Какой % reshuffle приемлем для 60k клиентов?

5. **Идемпотентность operator RPC.** `client_request_id` — будет ли?

6. **Snapshot target'а или re-resolution?** При `IssueRollout target=severity:medium`, expand ровно at-create или может re-resolve через час?

7. **Multi-DC pattern.** Federation или global coordinator?

8. **Audit policy.** Какие события явно audit'ятся? Regulatory требования есть?

9. **Capabilities semantics.** `SubscribeIn.capabilities` — фильтр команд или audit?

10. **`WatchRolloutStatus` server-streaming.** P0 (вместо polling `GetRolloutStatus`) — даёт большой UX-выигрыш при длинных rollout'ах.

11. **Bosun-client cert lifetime.** 1 год для 60k нод — много или мало? Какой % renewal через cron vs RotateCert?

12. **Granularity proto-services.** Один fat `BosunAPI` или split на Session / Rollout / Bundle / Ops / Inventory?
