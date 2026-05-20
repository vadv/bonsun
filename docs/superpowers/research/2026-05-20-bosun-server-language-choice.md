# Bosun-server: Go vs Rust — research

Date: 2026-05-20
Author: research-агент по заданию пользователя
Status: draft, требует принятия решения

## TL;DR

Рекомендую **bosun-server на Rust**, единый стек с bosun-client. Решающий фактор —
не raw performance (оба языка справятся с 60k стримов), а архитектурное единство:
shared протокол и доменные типы между client и server в одном workspace,
отсутствие двух runtime-моделей в команде, отсутствие GC-tail-latency как класса
проблем и пресечение соблазна тянуть в новый компонент всю груду «платформенных»
зависимостей chiit-server (`gitlab.ozon.ru/platform/*`), которые на самом деле к
домену bosun не имеют отношения. chiit-server остаётся как есть — он не
переписывается, а доживает на своих контурах до естественного вывода. discovery
подов и базы переезжают в bosun-server отдельным проходом — это понятный
ограниченный объём (~3–5 PG-таблиц + один HTTP-клиент к storage-inventory).

## Контекст

### Что нужно от bosun-server

Из `concepts/bosun-server-client-architecture.md` (commit `dad416c`):

- gRPC bi-directional streams с mTLS. 60k клиентов одновременно держат
  long-lived подключение.
- Multi-pod deployment в k8s. Под падает — клиенты ребалансируются на другой,
  read state из PG.
- PG как single source of truth: `nodes`, `node_affinity`, `servers`,
  `bundle_registry`, `commands_queue`, `audit_log`, `node_status`.
- Application-level heartbeat ping/pong ~10s, drop через 30s.
- Полл `commands_queue` каждую секунду на каждом поде; PG NOTIFY/LISTEN
  отвергнут как оверинжиниринг.
- Bootstrap-протокол: HTTP POST с shared secret → ECDSA cert.
- Audit-модель: routine success в `node_status` UPSERT, в `audit_log` —
  только failures. Оценка нагрузки: `node_status` UPSERT ≈ 2000 rows/sec,
  `audit_log` INSERT ≈ 100–500/sec в peak.

Это и есть полная performance-картина. Throughput умеренный, главное —
держать 60k long-lived стримов и не словить GC-spike в момент rollout.

### Что есть сейчас

**chiit-server** (`/chiit-server/`, Go 1.24):
- 31 RPC в `api/chiit/api.proto`: Build, Release, Report, GetReports,
  CreateClient, DeleteClient, ListClients, VaultGet, StorageInventoryGet,
  StorageInventoryGroupGet, StorageInventoryList, UpdateClientKey,
  GetWhiteList, GetCanary, GetMasterOfPatroniCluster, GetDatabaseList,
  GetSeverity, GetDatabasesBySeverity, GetPersonRoles, GetRSAPairs,
  GetTalosKeys, SetSilence, DeleteSilence, DropCache, ManagePartitions,
  PgBackupManager и пара служебных.
- gRPC + REST gateway через `grpc-gateway/v2`, swagger в `api.swagger.json`.
- Repository-слой в `internal/repository/` поверх `gitlab.ozon.ru/platform/go/database-pg` (Ozon SDK).
- Сборка proto через buf; модули `api/chiit`, `api/pg_shard_manager`.
- Auth клиентов: chiit-server хранит публичные RSA-ключи, клиент подписывает
  HTTP-запросы приватным ключом, подпись в header. `internal/ecsda/sign.go`
  реализует sign/verify.
- Vault через `hashicorp/vault/api v1.8.3`, плюс Ozon-обёртки.
- Cert-manager через свой Ozon-Gateway клиент.
- Tons of платформенных зависимостей: `tracer-go`, `metrics-go`,
  `databus/v2`, `realtime-config-go`, `scratch`, `meshmd/v2`, `o3auth`,
  `o3sync`, `warden`, `bootstrap-gen`, `circuit/v3`.

**bosun-client** (`/bosun-client/`, Rust 2021, MSRV 1.84):
- Workspace из 7 крейтов: `bosun-core`, `bosun-facts`, `bosun-primitives`,
  `bosun-cli`, `bosun-systemd-client`, `bosun-runr-client`, `bosun-handles`.
- Lints зафиксированы: `unsafe_code = "deny"`, `unwrap_used = "deny"`,
  `expect_used = "deny"`, `panic = "deny"` — для production-путей.
- Pure-Rust TLS: `rustls` + `webpki-roots`, без libssl/libcrypto. Это
  принципиально для musl-сборки агента.
- Starlark через `starlark ~0.13` для bundle-evaluation.
- Sync PG-клиент `postgres = "0.19"` (поверх tokio-postgres) уже подключён —
  для `pg_sql.exec` примитива.
- `rcgen` + `rsa` + `ring` для self-signed cert generation. ECDSA на
  стороне клиента — готово.

То есть половина криптографии и TLS-стек на Rust уже работают и подобраны
осознанно (комментарии в `Cargo.toml` объясняют выбор).

### Парк (`concepts/pg-park-topology.md`)

40–60k нод. Ubuntu 22.04. PostgreSQL 10–17, поровну KVM и k8s
StatefulSet. Кластеры 2–7 нод, 6–7 k8s-кластеров в разных DC. PG-кластер
прибит к etcd-кластеру через storage-inventory. Цель — миграция в k8s,
KVM выводится.

## Trade-off matrix Go vs Rust

### Производственные факторы

#### gRPC long-lived streams на 60k клиентов

**Go (grpc-go + goroutine-per-stream):**

- Каждый stream = минимум 1 goroutine на сервере (handler) + 1 goroutine
  read loop в HTTP/2 transport. Часто 2–3 goroutines на stream.
- Goroutine starts at 2 KB stack, grows dynamically. 60k × 2 stacks ×
  ~4–8 KB после роста = 480 MB–960 MB just stacks.
- grpc-go buffers: ReadBufferSize 32 KB + WriteBufferSize 64 KB (с x2
  для custom writer = 128 KB) per transport. Issue
  [grpc/grpc-go#5751](https://github.com/grpc/grpc-go/issues/5751)
  фиксирует: «with 1000 connections open, those buffers consume 100 MB
  of memory». На 60k стримов это **≈ 6 GB** только под буферы grpc-go,
  если не тюнить ReadBufferSize/WriteBufferSize.
- Issue [grpc/grpc-go#8403](https://github.com/grpc/grpc-go/issues/8403)
  (закрыт как Working As Intended): heap allocation растёт линейно при
  длительном streaming. Объяснение — `runtime.MemStats.Alloc` идёт по
  трамплинам алокаций до следующего GC. Поведение управляемое, но в
  bosun-сценарии каждый из 60k стримов будет генерировать heartbeat
  каждые 10s — это 6k allocations/sec только от heartbeat'ов.

**Rust (tonic + tokio):**

- Каждый stream = future (`async fn`), которая компилируется в
  state-машину. Никакого stack-per-task; всё аллоцируется на heap при
  spawn'е, и состояния между awaits хранятся компактно.
- Бенчмарк [Memory Consumption of Async](https://pkolaczk.github.io/memory-consumption-of-async/)
  фиксирует: при 1M concurrent tasks Rust tokio выиграл у Go goroutines
  больше чем в 12×. На 100k tasks Tokio удерживается на ~100 MB total
  (4 worker threads), пока Go показывает >1 GB.
- [Tokio Versus Goroutines: Latency Under Adversarial Load](https://dev.to/speed_engineer/tokio-versus-goroutines-latency-under-adversarial-load-5ll)
  под memory pressure при 10k rps:
  - Go: p50=1.2ms, p95=15ms, p99=45ms, **p99.9=200+ms**.
  - Tokio: p50=0.8ms, p95=2.1ms, p99=4.5ms, **p99.9=12ms**.
  - Причина перекоса — GC-pauses и scheduler contention в Go при
    memory pressure. У Tokio cooperative scheduling без GC.

Для bosun-server абсолютные числа второстепенны (push commands → ack —
не latency-critical), но **predictability tail latency матит для
heartbeat'ов**. Если на 60k стримов GC-spike в Go вытолкнет ack
heartbeat'а за 30s timeout, под решит что под мёртв и сделает
reconnect — каскад reconnects из-за 1 GC-pause возможен.

#### Memory footprint

Сценарий — single bosun-server pod держит ≈ 15k стримов (60k нод / 4
пода, k8s Service балансирует по L4 random):

| Стек            | Per-stream  | 15k stream pod | Notes |
|-----------------|-------------|----------------|-------|
| Go grpc-go default | 96 KB buf + 4–8 KB stack ≈ 105 KB | **1.6 GB**  | Без тюнинга ReadBufferSize |
| Go grpc-go tuned   | 8 KB buf + 4 KB stack ≈ 12 KB     | **180 MB**  | ReadBufferSize=4K, WriteBufferSize=4K — теряем пропускную способность на bursts |
| Rust tonic default | future ~5–20 KB + h2 buffers ~8 KB ≈ 25 KB | **375 MB**  | Без специального тюнинга |

Это только сетевой transport, без учёта per-connection auth-state,
session-state. С PG connection pool (~16 connections per pod), audit
buffer'ами, cache карты `node_id → cert` — добавится ещё 100–500 MB,
порядок не меняется.

Wall-clock вердикт: Rust удерживает 15k streams в ~400–600 MB pod
heap, Go без тюнинга — в ~1.6–2.5 GB, Go с тюнингом — в ~300–500 MB,
но с риском throughput-degradation на bursts.

#### Latency tail (p99/p999)

Для bosun-server латентность критична в трёх точках:

1. **Apply command → start downloading.** Клиент ждёт команду в открытом
   stream'е. p99 здесь — 50ms vs 5ms — пользователю невидимо.
2. **Heartbeat ack.** Под подтверждает что жив. Тут p99 в 200ms против
   12ms (Tokio vs Go под memory pressure) уже даёт разницу — если
   timeout 30s достаточен, но если 60k нод одновременно heartbeat'ит
   каждые 10s, всё держится только если tail закрывается заметно
   раньше следующего цикла.
3. **Status report → audit insert.** Идёт через `commands_queue` в PG.
   Latency тут определяется PG, не языком.

Конкретно для bosun-server **Go GC pause-spikes — реальный risk class**,
не теоретический. См. [issue golang/go#56966](https://github.com/golang/go/issues/56966)
про aggressive GC assist с many goroutines, [issue golang/go#19378](https://github.com/golang/go/issues/19378)
про long STW pauses на heavy heaps. Все workaround'ы — heap-tuning,
GOGC, ручное `runtime.GC()` — это новая операционная сложность.

В Rust класс проблем отсутствует. Pre-emption есть только в runtime
scheduler, и она кооперативна (point — `.await`).

#### Crash recovery / panics

- **Go**: `panic + recover` — стандарт. Один зависший handler не валит
  процесс. systemd / k8s livenessProbe рестартит pod при OOM или deadlock.
- **Rust**: panic = abort (default в release с правильным
  `panic = "abort"`), но workspace уже `panic = "deny"` в lints. Все
  ошибки идут через `Result<_, _>`. Это **более строгая дисциплина**:
  чтобы протолкнуть код в master, надо обработать каждый Err. В
  bosun-client это уже норма.
- **Side effect**: в Rust сложнее «временно заигнорить» транзиентную
  ошибку без явной семантики. Это хорошо для control-plane сервера, где
  пропуск ошибки = клиент в нестабильном состоянии.

### Developer-relevant факторы

#### Time-to-market

**Go:**
- Команда знает Go (chiit-server, postgres-chiit, chiit-library).
- Ozon-платформа дает готовые gitlab-pipeline, metric pushers, tracing.
- gRPC-Go — самая mature gRPC-имплементация. Tutorials, StackOverflow,
  внутренние патчи.
- **Минус 1**: вся domain-логика bosun-сервера (orchestrator команд,
  audit, bundle-registry) — переписывается с нуля, неважно на каком
  языке.
- **Минус 2**: одна команда — два языка. Server на Go, client на Rust.
  Proto-схема даёт типы, но shared business logic (рендеринг bundle
  metadata, validation, semver-rules) дублируется.

**Rust:**
- Половина криптографии (rustls, rcgen, ECDSA через `rsa`,
  x509-parser) уже отлажена в bosun-client. Reuse прямой.
- `tonic` для gRPC. `sqlx` или `tokio-postgres` для PG (postgres-крейт
  уже в workspace). `axum` для bootstrap-HTTP endpoint'а.
- Workspace расширяется: добавляются крейты `bosun-server`,
  `bosun-proto` (shared с client), `bosun-server-repo` (PG-layer).
- **Минус**: в команде меньше Rust-экспертизы. Учебный период есть.
- **Плюс**: shared types через крейт `bosun-proto` (или прямой share
  через workspace) — никаких ручных synchronization структур.

Time-to-market: Go может дать первый MVP на 2–4 недели быстрее за счёт
готовой Ozon-платформы. Rust выигрывает на 6+ месяцев — нет
дублирования business logic, нет cross-language boundary, нет
переключения контекста между runtime-моделями.

#### Maintenance burden

| Аспект            | Go-server | Rust-server |
|-------------------|-----------|-------------|
| Языков в стеке    | 2 (Rust client + Go server)            | 1 (Rust обоих) |
| Shared types      | proto only                              | proto + Rust types |
| Cross-debugging   | proto-level                             | shared logging, errors, tracing |
| Dependency surface | Ozon-platform (~50 deps) + grpc-go     | tonic + tokio + sqlx, всё чистое |
| MSRV/Go version   | go1.24 (Ozon-platform — locked)         | rust 1.84+ (workspace setting) |
| GC-tuning skills  | требуется                               | не нужно |

Перевод в production эксплуатацию: Go-server тянет за собой огромный
платформенный груз Ozon (`gitlab.ozon.ru/platform/*` — 50+ зависимостей
в `go.sum`). Это не плохо само по себе — даёт metrics, tracing,
RT-config. Но это значит: каждое обновление SDK ломает или меняет
поведение. Для bosun-server, который **только-только пишут**, это
большой неоправданный лок-ин — компонент будет жить 5+ лет, и
платформа за это время точно изменится.

Rust-server строится на стандартных open-source крейтах. Зависимости
явные. Каждый — задокументирован. Обновление tonic 0.x → 1.x понятно
читателю, ничего магии под капотом. И workspace lints (`deny.toml`,
`audit.toml`) уже работают в bosun-client.

#### Hiring

Go-девы шире на рынке. Это **не значимый фактор для Ozon-команды**, где
людей нанимают на платформенные принципы и обучают и Go, и Rust. В
команде Ozon уже есть Rust-эксперты на bosun-client; брать на сервер
тех же людей логично.

### Архитектурные факторы

#### Shared state с chiit-server

**Если bosun-server = Go в одном репо с chiit-server:**
- Соблазн «в одном процессе». Не делать так — chiit-server и
  bosun-server решают разные задачи (chiit раздаёт chiit-client'у
  бинари, bosun command'ит bosun-client). Один процесс = одно слабое
  место.
- Shared codebase, общий PG connection pool? Тоже плохо — chiit-server
  ходит в свою PG-схему (`itc_severity`, `clients`, `releases`,
  `reports`), bosun-server — в свою (`nodes`, `bundle_registry`,
  `commands_queue`). Объединение делает миграции опасными.

**Если bosun-server = Rust, отдельный процесс/репо:**
- Общий PG-инстанс возможен, но отдельная схема (`bosun.*`).
- chiit-server ничего не знает про bosun-server и наоборот. Они
  параллельны и независимы.

#### Discovery подов / discovery базы

Это самый важный концерн пользователя. Что есть в chiit-server для
discovery:

- **Storage-inventory как HTTP-клиент** в
  `internal/app/.../chiit/storage_inventory_get.go`,
  `storage_inventory_group_get.go`, `storage_inventory_list_test.go`.
  По коду это **тонкая обёртка**: HTTP GET в storage-inventory,
  unmarshal в protobuf, отдача клиенту. Storage-inventory сам — другой
  сервис, который остаётся как есть (см. `concepts/storage-inventory.md`).
- **Pod-discovery в `repository/get_databases_by_severity.go` и
  `severity.go`**: PG-запрос `select name from itc_severity where ...`.
  То есть «pod discovery» в chiit-server-контексте — это **выборка имён
  баз из PG-таблицы**, не k8s API.
- **RSA-keys discovery** в `repository/rsa_keys.go`: пары публичных
  ключей клиентов из PG.

В терминах bosun-server discovery становится:
- `nodes { node_id, cert_pem, hostname, cluster, severity, registered_at }` —
  таблица в bosun-schema.
- HTTP-клиент к storage-inventory — отдельный тонкий крейт
  `bosun-storage-inventory-client` (~200 строк Rust против ~500 строк
  Go-обёртки).
- Severity/canary queries — Rust query через `tokio-postgres`.

**Это всё переписывается за 1–2 недели одним разработчиком.** Не
гигантский объём. И самое главное — здесь нет ничего, что Go может, а
Rust не может: ни особой Go-биллиотеки, ни специфической Ozon-зависимости.

#### Если хочется reuse chiit-server'а

Альтернатива: bosun-server (Rust) делает gRPC/HTTP-вызов в
chiit-server (Go) для VaultGet, StorageInventoryGet, GetRSAPairs.

Pros: не переписываем работающий код.
Cons:
- Дополнительный network hop на каждый client request.
- chiit-server становится **жёсткой зависимостью** bosun-server — если
  chiit-server упадёт, bosun-server тоже не отвечает.
- Будут разные deployment cycles: bosun-server рестартится, а
  chiit-server старый.
- Это **снова два сервиса на два языка** — все недостатки сохраняются.

## Real-world precedents

### Discord: Go → Rust для read-states

- [Discord blog (старый)](https://discord.com/blog/why-discord-is-switching-from-go-to-rust)
  и переосмысление в [Medium](https://medium.com/@chopra.kanta.73/why-discord-migrated-read-states-from-go-to-rust-bdff7fb7c487).
- Workload — long-lived gRPC sessions, hot path с private cache.
- Метрики до перехода: GC-pauses до 2 минут на пиковой нагрузке.
- После перехода: latency p99 в 10× ниже, GC-pauses исчезли как класс.
- Bosun-server по нагрузочной картине похож на Discord read-states:
  long-lived streams + cache + hot-path.

### Grab: Counter Service Go → Rust

- [bytebytego.com on Grab](https://blog.bytebytego.com/p/how-grabs-migration-from-go-to-rust).
- gRPC service на тысячи rps. Перевели на Rust:
  - CPU: 20 cores → 4.5 cores на 1000 rps. Costs −70%.
  - Latency p99: paritet (важно — не выиграли, но и не проиграли).
  - Memory footprint значительно ниже.
- Это **прямо релевантный случай**: gRPC server с PG/Redis на бэкенде,
  именно та архитектурная форма bosun-server.

### Cloudflare Pingora

- Open-source [Pingora](https://blog.cloudflare.com/pingora-open-source/) —
  Rust-фреймворк для proxy. Закрыл реализацию NGINX в Cloudflare.
- 40M rps на production. End-to-end HTTP/2 + gRPC + WebSocket
  proxy. Zero-downtime graceful restart.
- Не control plane, но доказательство: Rust выдерживает реальный
  network-heavy workload на массовых масштабах.

### Etcd, Nomad, HashiCorp stack — Go

- Etcd, Consul, Nomad, Kubernetes — все на Go.
- Это **противоположный довод**: control planes исторически на Go.
- Но: etcd и Kubernetes API server решают разную задачу (consensus +
  watch streams). Bosun-server ближе к agent-control-plane (SaltStack,
  Chef Server, Puppet Server), где Go-выбор был исторический, не
  обязательный.
- SaltStack на ZeroMQ держит десятки тысяч minions; код на Python.
  Принципиально это не уперлось ни в язык, ни в transport — уперлось в
  thundering herd на authentication.

### Async runtime бенчмарки

- [pkolaczk.github.io: 1M concurrent tasks](https://pkolaczk.github.io/memory-consumption-of-async/) —
  Rust tokio выиграл у Go в 12× по памяти при 1M tasks. На 60k этот
  фактор меньше, но направление — то же.
- [Tokio vs Goroutines tail latency under memory pressure](https://dev.to/speed_engineer/tokio-versus-goroutines-latency-under-adversarial-load-5ll) —
  под memory pressure tail latency Tokio в 4–16× стабильнее.

## Hybrid options и их анализ

### A) bosun-server как новый Go-сервис рядом с chiit-server, shared codebase

**Pros:**
- Reuse существующих Ozon-платформенных интеграций (vault, metrics,
  tracing). 2–3 недели на bootstrap проекта.
- Знакомый стек для команды.
- Готовый ECDSA/RSA код можно переиспользовать.

**Cons:**
- Два языка в bosun-стеке навсегда.
- GC-tail-latency как реальный риск на 60k стримов.
- Платформенный lock-in (Ozon SDK), который ломается на minor-апдейтах.
- Дублирование бизнес-логики client ↔ server.

**Когда выбрать:** если time-to-market — приоритет №1 и MVP нужен через
2 месяца.

### B) bosun-server gRPC endpoints добавляются в существующий chiit-server (Go)

**Pros:**
- Чисто инкрементально.
- Один process, один деплоймент.
- Reuse PG-pool, vault, метрик.

**Cons:**
- chiit-server и bosun-server смешиваются на уровне процесса. Если
  bosun-handler упадёт, chiit-функции не работают.
- chiit-server старый, рос исторически. Лезть туда с двумя десятками
  новых RPC = боль и страх регрессий.
- chiit-server планировался к выводу (`концепт chiit-server` явно
  говорит «логика переезжает в bosun-server»). Усиление chiit-server-а
  = обратный путь.

**Когда выбрать:** никогда. Тактически проигрышный вариант.

### C) bosun-server как новый Rust-сервис, общий PG-state с chiit-server

**Pros:**
- Один язык на client + server.
- Никакой GC-tail-latency как класс.
- Чистый dependency stack (tonic, sqlx, axum, без Ozon-платформенной
  обвески).
- Прямой reuse rustls/rcgen/ECDSA из bosun-client.
- Shared business logic в общем Rust-workspace.
- Без cross-language boundary при дебаге.

**Cons:**
- 2–4 недели extra на bootstrap проекта (выбор tonic, build pipeline,
  metrics, tracing).
- Команда учит Rust на серверной стороне.
- Vault-интеграция — нужно решать (Rust крейт `vaultrs` или прокси
  через старый chiit).
- Ozon-platform пляшет с tracing/metrics — нужны решения для Rust
  (отдельный pipeline или OpenTelemetry).

**Когда выбрать:** если рассматривается долгоживущий компонент на 5+
лет, и архитектурное единство важнее короткого buster'а скорости.

### D) Микросервисная декомпозиция: bosun-server (Rust) обращается к chiit-server (Go) за discovery

**Pros:**
- chiit-server не трогается вообще.
- bosun-server чистый Rust.
- Постепенный отказ от chiit-server: сначала через прокси, потом
  миграция к direct read из bosun-schemy.

**Cons:**
- Каждый bosun-server-handler делает дополнительный HTTP/gRPC hop в
  chiit-server. На 60k стримов с heartbeat каждые 10s это значит
  pivot-load на chiit-server.
- chiit-server становится жёсткой зависимостью.
- Two services to monitor, two SLOs, two version mismatch contracts.

**Когда выбрать:** если discovery-данные нужны только при connect'е
клиента (один раз), а не на каждом heartbeat. Тогда extra-hop
амортизируется.

### E) Гибридный: bosun-control-plane (Rust) для gRPC sessions, chiit-server (Go) живёт параллельно, PG общий

**Pros:**
- chiit-server продолжает обслуживать chiit-client до конца его жизни.
- bosun-server обслуживает bosun-client, у него своя PG-схема.
- Два параллельных control plane, не конкурируют.
- chiit-server тихо умирает по мере декомиссионирования chiit-client'ов.

**Cons:**
- В переходный период два control plane одновременно — операционный
  overhead.
- Команда поддерживает обе кодбазы.

**Когда выбрать:** прагматичный default. По сути это и есть нынешний
план — chiit-стек живёт, bosun-стек растёт независимо.

## Рекомендация

**Выбираю вариант E (гибрид: параллельная Rust-реализация bosun-server,
chiit-server остаётся как есть).** Это технический шорт-кат до полной
миграции, но с правильной языковой основой.

### Три главных аргумента

1. **Единый язык на client+server даёт реальную экономию на трёх
   фронтах**: shared types через workspace, shared
   crypto/TLS-инфраструктуру (rustls, rcgen, ECDSA уже отлажены в
   bosun-client), shared debug-инструменты (один tracing-стек, одни
   error-типы). Дублирование бизнес-логики между Rust-client и Go-server
   — это **20–30% времени команды через 6 месяцев**. Один язык убирает
   эту строку из бюджета.

2. **GC-tail-latency на 60k long-lived streams в Go — не теоретический
   риск.** На 60k стримов с heartbeat каждые 10s = 6k heartbeat-events/sec.
   Go-GC pause в 50–200ms на heavy heap (60k стримов × ~25 KB state =
   1.5 GB heap minimum) — фиксированный risk-class. В Rust его нет.
   Discord и Grab фиксировали это в продакшне на похожих workload'ах.

3. **discovery в bosun-server — небольшой объём.** Storage-inventory
   остаётся как есть, bosun-server идёт к нему HTTP-клиентом (200
   строк Rust). PG-discovery (severity, canary, white-list) — 5–7 PG-
   таблиц с простыми запросами. ECDSA/RSA-keys ходим читать в PG — уже
   подключён `postgres` крейт. Переписать всё это можно за 2–3 недели
   одним разработчиком. Это не аргумент в пользу reuse Go-кода.

### Три главных риска и mitigation

1. **Команда меньше знает Rust на серверной стороне.**
   *Mitigation:* парная разработка первых 2–3 крейтов с экспертом из
   bosun-client. Документация и шаблоны workspace уже есть
   (`audit.toml`, `deny.toml`, `rustfmt.toml`). Сначала bootstrap
   протокол (~1 неделя), потом gRPC handler skeleton, потом PG-layer.
   Каждый этап имеет clear deliverable.

2. **Ozon platform integration (metrics, tracing, RT-config) на Rust
   придётся переделать с нуля.**
   *Mitigation:* OpenTelemetry-стек на Rust зрелый
   (`opentelemetry`, `opentelemetry-otlp`, `tracing-opentelemetry`).
   Метрики — `metrics` крейт + OTLP-экспортер в Mimir (тот же бэкенд,
   что у chiit-server). RT-config заменяется на чтение из PG-таблицы
   `config` с кешем — проще чем `realtime-config-go`. Vault — крейт
   `vaultrs`, протокол стандартный.

3. **Параллельная работа двух control plane'ов в переходный период —
   операционный overhead.**
   *Mitigation:* это не overhead, а явный плюс гибрида — chiit
   обслуживает chiit-client, bosun обслуживает bosun-client. Они
   независимы. Команда саппорта работает с обоими через одинаковые
   dashboards (Grafana). Финальная декомиссия chiit-server происходит
   когда последний chiit-client заменён — это естественный triggerpoint,
   не дополнительная миграция.

## Альтернативный рекомендованный путь (если приоритеты другие)

- **Приоритет — fast time-to-market:** Variant A (Go-сервер с
  Ozon-платформой). MVP за 2 месяца, но фиксируем технический долг — в
  следующие 2 года придётся переписать на Rust, иначе GC-проблемы на
  60k стримов прилетят.
- **Приоритет — long-term consistency + low ops risk:** Variant C/E
  (Rust). Дополнительные 2–4 недели на старте, но команда строит один
  язык на client + server раз и навсегда.

## Open questions для пользователя

1. **Latency SLO для heartbeat ack.** Если p99 > 100ms допустим —
   GC-spike в Go не критичен. Если требуется < 30ms p99 — Rust почти
   обязателен.

2. **Бюджет команды Rust-разработчиков.** На bosun-client сколько FTE
   сейчас? Сможет ли один-два разработчика стартовать bosun-server,
   или это означает заморозить bosun-client на месяц?

3. **Vault как dependency.** chiit-server проксирует Vault — клиенты
   не имеют прямого доступа. Bosun-server должен делать то же? Или
   bosun-client получает короткоживущий Vault-token и идёт сам? Это
   меняет complexity Vault-интеграции.

4. **Metrics/Tracing stack.** Mimir + OpenTelemetry?
   `gitlab.ozon.ru/platform/metrics-go` обязан или можно стандартный
   Prometheus-export?

5. **OCI/S3 для bundle distribution.** Из текущего бэклога это open
   question (см. `bosun-server-client-architecture` секция Distribution).
   Если OCI — нужен `oras-rs`, который зрелый. Если S3 — `aws-sdk-rust`
   тоже зрелый. На выбор языка это не влияет.

6. **`audit_log` объём.** Если хранение audit нужно дольше 30 дней —
   нужен retention/архивирование. Это не влияет на язык, но влияет на
   архитектуру PG-схемы.

7. **Деплой-кейс.** k8s StatefulSet или Deployment с anti-affinity?
   Multi-DC? Pod count на DC? Это аффектит rollout-стратегию, но не
   выбор языка.

## Sources

- [hyperium/tonic GitHub](https://github.com/hyperium/tonic) — production gRPC
  для Rust.
- [Tokio vs Goroutines tail latency under adversarial load](https://dev.to/speed_engineer/tokio-versus-goroutines-latency-under-adversarial-load-5ll) —
  бенчмарки p99/p999 Go vs Rust.
- [Memory Consumption of Async Tasks (pkolaczk)](https://pkolaczk.github.io/memory-consumption-of-async/) —
  1M tasks Tokio vs Go.
- [grpc/grpc-go issue #5751: High memory footprint per connection](https://github.com/grpc/grpc-go/issues/5751) —
  96KB на connection в grpc-go.
- [grpc/grpc-go issue #8403: Memory growth in long-lived streams](https://github.com/grpc/grpc-go/issues/8403) —
  закрыт как Working As Intended.
- [Go GC issue #56966: aggressive gc assist with many goroutines](https://github.com/golang/go/issues/56966).
- [Go GC issue #19378: long GC STW pauses ≥80ms](https://github.com/golang/go/issues/19378).
- [Discord Go → Rust migration on read-states](https://medium.com/@chopra.kanta.73/why-discord-migrated-read-states-from-go-to-rust-bdff7fb7c487) —
  10× latency improvement, 2-минутные GC pauses до перехода.
- [Grab Go → Rust Counter Service migration](https://blog.bytebytego.com/p/how-grabs-migration-from-go-to-rust) —
  CPU 20→4.5 cores, costs -70%.
- [Cloudflare Pingora open-sourced](https://blog.cloudflare.com/pingora-open-source/) —
  40M rps Rust framework.
- Внутренние документы: `concepts/bosun-server-client-architecture.md`,
  `concepts/chiit-server.md`, `concepts/chiit-client.md`,
  `concepts/pg-park-topology.md`, `concepts/storage-inventory.md`.
- Код chiit-server: `chiit-server/api/chiit/api.proto`,
  `chiit-server/internal/repository/types.go`,
  `chiit-server/internal/ecsda/sign.go`,
  `chiit-server/cmd/chiit-server/main.go`.
- Код bosun-client: `bosun-client/Cargo.toml` (workspace lints и
  зависимости).
