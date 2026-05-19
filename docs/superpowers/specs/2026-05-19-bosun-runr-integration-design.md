---
title: "bosun ↔ runr integration — Design"
date: 2026-05-19
status: draft (pending review)
author: dmitrivasilyev
related:
  - docs/superpowers/specs/2026-05-18-bosun-client-mvp-design.md
  - .claude-memory-compiler/knowledge/concepts/runr-supervisor.md
  - .claude-memory-compiler/knowledge/concepts/chiit-go-dsl.md
---

# bosun ↔ runr integration — Design

## Контекст и цель

bosun-client MVP покрывает базу: `apt.package`, `file.content`, `template()`, факты, plan/apply, BDD в Docker. Очередь — управление долгоживущими процессами на ноде. На текущем парке Ozon это **runr** ([[concepts/runr-supervisor]]): HTTP API на `127.0.0.1:8010` + INI-конфиги в `/etc/runr/`.

Эта итерация добавляет три native-функции:
- `runr.service` — описывает unit + желаемое состояние (`running`/`stopped`/`absent`).
- `runr.timer` — описывает таймер unit'а.
- `runr.cgroup` — описывает cgroup-юнит для лимитов.

И механику handle-связей:
- `reload_on=[...]` — graceful reload через `POST /reload`.
- `restart_on=[...]` — restart через `POST /restart`.
- `depends_on=[...]` — только порядок применения, без действий.

После этой итерации демо-сценарий: bundle ставит nginx через `apt.package`, кладёт конфиг через `file.content` + `template`, описывает `runr.service` для nginx с `reload_on=[nginx_conf]`. При изменении конфига bosun делает `POST /api/v1/services/nginx/reload`.

## Принципы

1. **Минимум HTTP-запросов.** GET статусов один раз на старте apply-фазы (через FactsCollector), кешируется. DaemonReload — один раз в конце phase (если кто-то писал unit-файл), не per-resource. POST start/stop/restart — только при реальном diff.
2. **Split vs Inline:** runr.service НЕ пишет unit-файл сам. Это всегда отдельный `file.content` ресурс, передаваемый через handle. Inline-kwargs — sugar для следующей итерации.
3. **Никаких силент-фолбэков.** Если runr daemon недоступен (`Connection refused` на `127.0.0.1:8010`) — `PrimitiveError::RunrUnavailable`, классифицирующийся как `Outcome::Deferred` (transient, retry next cycle).
4. **INI сериализация типизирована.** Каждое поле `ServiceSection`/`TimerSection`/`CgroupSection` — Rust struct с serde. INI-writer выделен отдельно с round-trip тестами.
5. **Handle-связи разделены семантически.** `reload_on`/`restart_on`/`depends_on` — три разных списка ResourceId в `Resource`. Orchestrator при apply ресурса проверяет: если кто-то из `reload_on` был Changed → запросить reload; если кто-то из `restart_on` был Changed → restart.

## Что в scope MVP интеграции

- Новый крейт `bosun-runr-client` — HTTP-клиент с минимальным набором: `daemon_info`, `units_reload`, `services_start/stop/restart/reload`, `services_statuses`, `timers_*`, `units_list`.
- В `bosun-primitives`: три новых модуля `runr_service`, `runr_timer`, `runr_cgroup` с impl `Primitive`.
- В `bosun-primitives`: модуль `runr_ini` для генерации INI-конфигов.
- В `bosun-core`:
  - Расширить `Resource` полем `restart_on: Vec<ResourceId>` (сейчас есть только `reload_on` + `depends_on`, в MVP трактовались одинаково).
  - В `Orchestrator::apply`: трекать «какие resource id стали Changed на этой итерации» и при apply следующих смотреть пересечение с их `reload_on`/`restart_on`.
  - Новое поле `ApplyCtx.changed_resources: Arc<Mutex<HashSet<ResourceId>>>` — для передачи трекинга в примитивы.
  - End-of-phase callback в Orchestrator: после apply всех — если кто-то из runr-примитивов был Changed (по типу — `runr.service|timer|cgroup`), сделать один `POST /units/reload`.
- В `bosun-core::PrimitiveError`: новый вариант `RunrUnavailable { reason: String }`, классифицированный как deferrable в `is_deferrable()`.
- В `bosun-cli`: новые флаги `--runr-url` (default `http://127.0.0.1:8010`), `--runr-config-dir` (default `/etc/runr`). Регистрация runr-примитивов в orchestrator.
- BDD: docker test-base с предустановленным runr daemon (клон `github.com/ozontech/runr` + сборка в multi-stage). Сценарии: service install/start/stop/restart/reload, timer enable/disable, cgroup apply, reload_on triggers reload, restart_on triggers restart, daemon unavailable → Deferred.

## Что НЕ в scope этой итерации

- Inline-декларация: `runr.service(name=..., exec_start=..., user=...)` сам генерирует unit-файл. Только split через `file.content` + handle.
- runr `syslog` config (отдельный концепт, не критичный для MVP).
- bosun-side rollback при failed reload/restart (PG-критичный сценарий — отдельный спек).
- `service.validate` (например `nginx -t` перед reload). Это будет следующий шаг.
- Удалённое управление runr на другой ноде (только локальный `127.0.0.1`).

## Архитектура

### Новый крейт `bosun-runr-client`

Изолированный HTTP-клиент. Зависит только от `reqwest` (blocking-фича для синхронного API под current_thread tokio runtime) или `ureq` (лёгкий синхронный, без tokio). Выбор: **`ureq`** — нет цепочки тяжёлых зависимостей, не требует runtime, проще тестировать.

```
bosun-client/
└── crates/
    └── bosun-runr-client/
        ├── Cargo.toml
        └── src/
            ├── lib.rs              # pub API: Client, RunrError, типы запросов/ответов
            ├── client.rs           # struct Client с методами
            ├── types.rs            # ServiceStatus, TimerStatus, UnitListItem, ActionAck, DaemonInfo
            └── errors.rs           # RunrError enum
```

#### `Client` API

```rust
pub struct Client {
    base_url: String,
    http: ureq::Agent,
    timeout: std::time::Duration,
}

impl Client {
    pub fn new(base_url: impl Into<String>, timeout: std::time::Duration) -> Self;

    pub fn daemon_info(&self) -> Result<DaemonInfo, RunrError>;
    pub fn daemon_reload(&self) -> Result<ActionAck, RunrError>;

    pub fn service_start(&self, name: &str, idempotent: bool) -> Result<ActionAck, RunrError>;
    pub fn service_stop(&self, name: &str, force: bool, timeout: Option<&str>) -> Result<ActionAck, RunrError>;
    pub fn service_restart(&self, name: &str) -> Result<ActionAck, RunrError>;
    pub fn service_reload(&self, name: &str) -> Result<ActionAck, RunrError>;

    pub fn timer_start(&self, name: &str) -> Result<ActionAck, RunrError>;
    pub fn timer_stop(&self, name: &str) -> Result<ActionAck, RunrError>;
    pub fn timer_enable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError>;
    pub fn timer_disable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError>;

    pub fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError>;
    pub fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError>;
    pub fn units_list(&self) -> Result<Vec<UnitListItem>, RunrError>;
}
```

#### `RunrError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunrError {
    #[error("runr daemon unavailable at {base_url}: {source}")]
    Unavailable { base_url: String, #[source] source: Box<dyn std::error::Error + Send + Sync> },

    #[error("runr api error {status}: {body}")]
    ApiError { status: u16, body: String },

    #[error("invalid response body: {0}")]
    BadResponse(String),

    #[error("not found: {kind} '{name}'")]
    NotFound { kind: String, name: String },
}
```

`Unavailable` — для DNS, TCP-connect, refused. `ApiError` — для не-2xx ответов. `NotFound` — для 404 (несуществующий unit).

#### Зависимости bosun-runr-client

- `ureq = "2"` (синхронный HTTP-клиент, ~minimal deps)
- `serde`, `serde_json`, `thiserror`
- В dev: `mockito` или `wiremock` для unit-тестов HTTP-стороны

### Новые примитивы в `bosun-primitives`

```
bosun-primitives/src/
├── runr_service/
│   ├── mod.rs              # RunrServicePrimitive impl Primitive
│   ├── spec.rs             # RunrServiceSpec (name, state, reload_strategy)
│   ├── plan.rs             # compute_diff via service_statuses()
│   └── apply.rs            # POST start/stop/restart
├── runr_timer/
│   ├── mod.rs
│   ├── spec.rs
│   ├── plan.rs
│   └── apply.rs
├── runr_cgroup/
│   ├── mod.rs
│   ├── spec.rs             # тонкая обёртка над INI
│   ├── plan.rs             # GET /units, find Cgroup with name; сравнить fields
│   └── apply.rs            # write_file is done elsewhere; здесь только trigger reload
└── runr_ini/
    ├── mod.rs              # writer API: pub fn render_service_section(...) -> String
    ├── service.rs          # render [Service] section
    ├── log.rs              # render [Log] section
    ├── timer.rs            # render [Timer] section
    └── cgroup.rs           # render [Cgroup] section
```

### `runr.service` Primitive

#### `RunrServiceSpec`

```rust
#[derive(serde::Deserialize, Debug, Clone)]
pub struct RunrServiceSpec {
    pub name: String,
    pub state: ServiceState,  // running | stopped | absent
}

#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServiceState {
    Running,
    Stopped,
    Absent,  // удалить unit-файл + остановить если запущен
}
```

В MVP интеграции — **только эти три поля**. Inline-конфиг (ExecStart/User/etc.) добавляется в следующей итерации; пока bundle декларирует unit-файл через отдельный `file.content`.

#### Starlark API

```python
nginx_unit = file.content(
    path     = "/etc/runr/nginx.service",
    contents = template("services/nginx.service.j2"),
    mode     = 0o644,
)

nginx_conf = file.content(
    path     = "/etc/nginx/nginx.conf",
    contents = template("nginx.conf.j2"),
)

runr.service(
    name        = "nginx",
    state       = "running",
    depends_on  = [nginx_unit],   # порядок: unit-файл прежде чем service.start
    reload_on   = [nginx_conf],   # graceful reload при изменении nginx.conf
)
```

#### `plan`

1. Получить `service_statuses()` через runr-client. Кешируется в `ApplyCtx.runr_service_statuses: OnceCell<Vec<ServiceStatus>>` — один HTTP-запрос на весь apply.
2. Найти запись с `name == spec.name`.
3. Сравнить:
   - `state == Running` & запись отсутствует → `Add` (unit-файл уже должен быть положен предыдущим ресурсом, иначе start не сработает; это runtime warning).
   - `state == Running` & state ≠ "Running" → `Update`.
   - `state == Running` & state == "Running" → `NoChange` (но может потребоваться reload/restart — это решается на apply через `changed_resources`).
   - `state == Stopped` & state == "Stopped" → `NoChange`.
   - `state == Stopped` & state ≠ "Stopped" → `Update`.
   - `state == Absent` & запись отсутствует → `NoChange`.
   - `state == Absent` & запись присутствует → `Update`.

#### `apply`

1. Проверить cancel/deadline.
2. Если `diff == Update` и `state == Running`:
   - `POST /services/{name}/start { idempotent: true }`.
   - `ChangeReport::changed(format!("started {name}"))`.
3. Если `diff == Update` и `state == Stopped`:
   - `POST /services/{name}/stop { force: false }`.
   - `ChangeReport::changed(format!("stopped {name}"))`.
4. Если `diff == Update` и `state == Absent`:
   - `POST /services/{name}/stop { force: false }` (best-effort; если уже stopped — ok).
   - **НЕ удаляем unit-файл** — это работа другого ресурса (`file.content` с `ensure="absent"` в будущем; в MVP unit-файл удаляется bundle-владельцем явно).
5. Если `diff == NoChange` И state == Running И на этой phase кто-то из `reload_on` стал Changed (см. `ctx.changed_resources`):
   - `POST /services/{name}/reload`.
   - `ChangeReport::changed(format!("reloaded {name}"))`.
6. Если `diff == NoChange` И state == Running И на этой phase кто-то из `restart_on` стал Changed:
   - `POST /services/{name}/restart`.
   - `ChangeReport::changed(format!("restarted {name}"))`.
7. На каждый HTTP-вызов: при `RunrError::Unavailable` → `PrimitiveError::RunrUnavailable`. Это `is_deferrable() == true`, попадает в `Outcome::Deferred`.

### `runr.timer` Primitive

`RunrTimerSpec`:

```rust
pub struct RunrTimerSpec {
    pub name: String,
    pub state: TimerState,  // enabled | disabled | absent
    #[serde(default)]
    pub start_now: bool,    // при enable дополнительно запустить
}

#[derive(serde::Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerState { Enabled, Disabled, Absent }
```

`plan` через `timer_statuses()`. `apply`:
- Enabled & not enabled → `POST /timers/{name}/enable { now: spec.start_now }`.
- Disabled & enabled → `POST /timers/{name}/disable { now: false }`.
- Absent & ... → stop + disable; unit-файл удаляется отдельно.

### `runr.cgroup` Primitive

Cgroup не имеет команд `start/stop` — это конфигурационный объект. В runr cgroup активируется через unit-файл `/etc/runr/X.cgroup` + DaemonReload. Поэтому `runr.cgroup` в MVP **только декларация присутствия**:

```rust
pub struct RunrCgroupSpec {
    pub name: String,
    pub state: CgroupState,  // present | absent
}

pub enum CgroupState { Present, Absent }
```

`plan` через `units_list()` (фильтр `kind == Cgroup`). `apply`: если был Changed в `depends_on` (unit-файл) — trigger `POST /units/reload` (но это центральный механизм, см. End-of-phase callback).

### End-of-phase DaemonReload

Изменение unit-файла (`file.content` для `/etc/runr/X.service|timer|cgroup`) требует DaemonReload. Делать его per-resource — расточительно (3 unit-файла = 3 reload'а). Решение:

В `Orchestrator::apply` после прохода всех ресурсов — секция «end-of-phase callbacks». Если **хотя бы один** ресурс с `kind ∈ {runr.service, runr.timer, runr.cgroup}` стал Changed, ИЛИ хотя бы один `file.content` writing into `/etc/runr/` стал Changed — единичный `POST /units/reload`.

Реализация:
- В `ApplyCtx` добавить `runr_client: Option<Arc<bosun_runr_client::Client>>` (None для CLI без runr).
- В `Orchestrator` собирать `runr_dirty: Mutex<bool>` — выставляется в `true` при Changed-applied ресурсов с runr-kind.
- После apply-loop: `if runr_dirty.load() && runr_client.is_some() { client.daemon_reload()?; }`.

Альтернатива: вынести reload-фазу как новый `Outcome::PostReload` или отдельный шаг в Orchestrator API. Не стоит — это будет переусложнение MVP. Простой mutex-флаг достаточно.

Для `file.content` пишущих в `/etc/runr/` — детект через path prefix `/etc/runr/`. Это эвристика; альтернативно — пользовательский явный handle через `runr.daemon_reload()` функцию. Решение: эвристика по path prefix в MVP, явная функция — TODO.

### Разделение reload_on / restart_on / depends_on в Resource

```rust
pub struct Resource {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub spec_version: u16,
    pub payload: serde_json::Value,
    pub reload_on: Vec<ResourceId>,   // тригерит reload primitive'а
    pub restart_on: Vec<ResourceId>,  // НОВОЕ: тригерит restart primitive'а
    pub depends_on: Vec<ResourceId>,  // только порядок
}
```

В `Registry::topological_order` — все три trait'утся как рёбра. Различие — в `Orchestrator::apply`: примитив при `diff == NoChange` ещё спрашивает у `ctx.changed_resources` — есть ли пересечение с `reload_on`/`restart_on`. Если да — это разрешение на дополнительное действие, реализуется в самом primitive.

API дополнения:
- `ApplyCtx.changed_resources: Arc<Mutex<HashSet<ResourceId>>>` — Orchestrator пополняет после успешного apply-Changed.
- `ApplyCtx.is_changed(&id) -> bool` — helper для primitive.
- `Resource.reload_triggers(&self, ctx) -> bool` — вычисляет «хотя бы один из `reload_on` есть в `ctx.changed_resources`».
- Аналогично `Resource.restart_triggers(&self, ctx) -> bool`.

Это всё на стороне `bosun-core`. Примитивы apt/file не используют это (в их `apply` нет понятия reload/restart, они меняют state файла/пакета).

### CallArgs расширение

В `bosun-core::CallArgs`:
- `optional_handle_list(name)` — уже есть.
- Нужно добавить специализированный getter для `reload_on`/`restart_on`/`depends_on`, или Starlark-glue вытаскивает их единожды и передаёт через `Resource` builder.

Простое решение: в `register_primitive_call` (`starlark_glue/globals.rs`) Starlark-glue парсит все три ключа и кладёт в `Resource.reload_on`/`restart_on`/`depends_on`. Примитив не видит их в `CallArgs` — это reserved fields, обрабатываемые ядром.

### CLI расширение

В `bosun-cli/src/args.rs`:

```rust
#[arg(long, default_value = "http://127.0.0.1:8010")]
pub runr_url: String,

#[arg(long, default_value_t = 10)]
pub runr_timeout_sec: u32,

#[arg(long, default_value = "/etc/runr")]
pub runr_config_dir: PathBuf,
```

В `bosun-cli/src/run.rs`:
- Создать `RunrClient` если `runr_url` доступен по daemon_info. Если первый запрос упал с `Unavailable` — продолжить с `runr_client: None` (runr primitives будут уходить в Deferred).
- Зарегистрировать примитивы:
  ```rust
  primitives.insert(
      ResourceKind::from_static("runr.service"),
      Box::new(RunrServicePrimitive::new(client.clone())),
  );
  primitives.insert(
      ResourceKind::from_static("runr.timer"),
      Box::new(RunrTimerPrimitive::new(client.clone())),
  );
  primitives.insert(
      ResourceKind::from_static("runr.cgroup"),
      Box::new(RunrCgroupPrimitive::new(client.clone())),
  );
  ```

### Метрика

В `bosun.prom` добавить:
- `bosun_runr_reachable` — gauge 0/1. Обновляется при инициализации (попытка daemon_info).
- `bosun_resources_total{outcome="deferred"}` — уже есть; runr-failures сюда.
- `bosun_runr_units_total{kind="Service|Timer|Cgroup"}` — кол-во unit'ов в реестре runr (для контекста).

### Bundle пример

`examples/nginx-runr/`:

```
bundle/
├── bundle.toml
├── manifests/main.star
├── defaults/main.yaml
└── templates/
    ├── nginx.service.j2
    └── nginx.conf.j2
```

`manifests/main.star`:

```python
load("@bosun/builtins", "apt", "file", "runr", "template")

apt.package(name = "nginx")

nginx_service_file = file.content(
    path     = "/etc/runr/nginx.service",
    contents = template("nginx.service.j2"),
    mode     = 0o644,
)

nginx_conf = file.content(
    path     = "/etc/nginx/nginx.conf",
    contents = template("nginx.conf.j2"),
    mode     = 0o644,
)

runr.service(
    name        = "nginx",
    state       = "running",
    depends_on  = [nginx_service_file],
    reload_on   = [nginx_conf],
)
```

## Тестирование

### Уровень 1 — Unit (cargo test)

**bosun-runr-client:**
- Все методы клиента — через `wiremock` или `mockito` (HTTP mock-сервер на эфемерном порту).
- Сценарии: 200 OK, 404 (NotFound), 5xx (ApiError), connection refused (Unavailable), таймаут (Unavailable).

**bosun-primitives/runr_*:**
- Mock `RunrClient` trait (если не выделим — через mock-HTTP в тестах).
- Для `RunrServicePrimitive`:
  - `plan_running_when_already_running_is_no_change`
  - `plan_running_when_stopped_is_update`
  - `plan_running_when_missing_is_add`
  - `plan_stopped_when_running_is_update`
  - `plan_absent_when_missing_is_no_change`
  - `apply_update_running_calls_service_start`
  - `apply_no_change_with_reload_trigger_calls_service_reload`
  - `apply_no_change_with_restart_trigger_calls_service_restart`
  - `apply_unavailable_returns_runr_unavailable_error`
- Аналогично timer/cgroup.

**bosun-primitives/runr_ini:**
- Round-trip тесты: построить ServiceSection программно → render → парсить обратно → сравнить. (Или просто проверить, что output содержит ожидаемые ключи в нужном порядке.)
- Особо: continuation-строки, экранирование значений с пробелами/спецсимволами.

**bosun-core:**
- `ApplyCtx.is_changed/reload_triggers/restart_triggers`.
- Orchestrator: при `runr.service` Changed устанавливает `runr_dirty=true`, в end-of-phase делает `daemon_reload`. Один reload даже при 3 changed runr-resources.
- Orchestrator: при `file.content` пишущем в `/etc/runr/` Changed — то же.

### Уровень 2 — Golden (для template)

Не требуется новых golden — runr.service не имеет template-функции. INI-сериализация unit'а покрывается unit-тестами.

### Уровень 3 — BDD в Docker

**Docker test-base:**

Расширить `docker/test-base.Dockerfile`:

```dockerfile
FROM debian:bookworm-slim AS runr-build

RUN apt-get update && apt-get install -y --no-install-recommends \
    git ca-certificates curl build-essential pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

# Install rustup minimally
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# Clone and build runr from upstream
RUN git clone --depth 1 https://github.com/ozontech/runr.git /src/runr
WORKDIR /src/runr
RUN cargo build --release --bin runr

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=runr-build /src/runr/target/release/runr /usr/bin/runr
RUN mkdir -p /etc/runr

# Запуск runr supervisor в фоне на $RUNR_URL=http://127.0.0.1:8010
# Контейнер не делает это автоматически — BDD тест запускает явно через
# docker exec, чтобы можно было протестировать «daemon unavailable».

WORKDIR /work
```

Известная сложность: репо `github.com/ozontech/runr` полузакрытое. Если оно требует токен — fallback на pre-built бинарь, скачиваемый из локального registry. Решение в BDD-Dockerfile: переменная `RUNR_SOURCE`, defaults на git-clone, может быть переопределена на `COPY` локального файла. Документировать в `docs/runr-source.md`.

**Feature файлы:**

`features/runr_service.feature`:

```gherkin
@docker @runr
Feature: runr.service primitive
  bosun manages services through the runr supervisor.

  Background:
    Given a fresh container with runr daemon started

  @runr-service @slow
  Scenario: Service unit file write triggers daemon-reload and start
    Given a bundle with manifest:
      """
      load("@bosun/builtins", "file", "runr", "template")

      unit = file.content(
          path     = "/etc/runr/demo.service",
          contents = "[Service]\nExecStart=/usr/bin/sleep 3600\nRestart=on-failure\n",
      )

      runr.service(name = "demo", state = "running", depends_on = [unit])
      """
    When I apply the bundle
    Then exit code is 0
    And running "curl -fsS http://127.0.0.1:8010/api/v1/services/statuses" prints contains "demo"
    And running "curl -fsS http://127.0.0.1:8010/api/v1/services/statuses" prints contains "Running"

  @runr-service @slow
  Scenario: Idempotent re-apply makes no change
    [setup with demo service running]
    When I apply the bundle again
    Then exit code is 0
    And stdout contains "no-change"

  @runr-service
  Scenario: Service state=stopped stops the service
    [...]

  @runr-service
  Scenario: reload_on triggers service reload
    Given service "demo" is running
    And a bundle that has file.content "/etc/demo.conf" with new content
    When I apply the bundle
    Then exit code is 0
    And bosun stdout contains "reloaded demo"

  @runr-service @daemon-unavailable
  Scenario: Daemon unavailable returns Deferred
    Given runr daemon is stopped in the container
    And a bundle with runr.service(name="demo", state="running")
    When I apply the bundle
    Then exit code is 0
    And bosun stdout contains "deferred"
    And metric file shows bosun_resources_total{outcome="deferred"} >= 1
```

Аналогичные для `runr.timer` и `runr.cgroup`.

## Error handling

### `PrimitiveError::RunrUnavailable`

Новый вариант:

```rust
#[error("runr daemon unavailable at {base_url}: {reason}")]
RunrUnavailable { base_url: String, reason: String },
```

В `is_deferrable()`:

```rust
pub fn is_deferrable(&self) -> bool {
    matches!(
        self,
        PrimitiveError::DpkgLocked { .. }
            | PrimitiveError::Cancelled
            | PrimitiveError::RunrUnavailable { .. }
    )
}
```

Это автоматически переводит RunrUnavailable в `Outcome::Deferred` в Orchestrator (см. DevOps Critical fix 5 от 2026-05-18).

### Логирование

- На `daemon_info` при старте: `tracing::info!(reachable = %ok, "runr daemon")`.
- На каждый primitive.apply: span `runr_service{name=...}` + `tracing::info!(action = "start|stop|restart|reload", "runr action")`.
- На Unavailable: `tracing::warn!(base_url = %url, reason = %r, "runr daemon unavailable, deferring resource")`.

## Внешние зависимости

В workspace.dependencies:
- `ureq = "2"` — синхронный HTTP-клиент.

В `bosun-runr-client/Cargo.toml`:
- ureq, serde, serde_json, thiserror.

В `bosun-primitives/Cargo.toml`:
- `bosun-runr-client = { path = "../bosun-runr-client" }`.

Никаких новых workspace deps.

## Open questions

- **Inline-декларация unit'а внутри `runr.service(...)`**. Например `runr.service(name="nginx", exec_start="/usr/sbin/nginx -g 'daemon off;'", user="nginx", ...)` сам пишет `/etc/runr/nginx.service`. Удобнее для пользователя, дороже на реализацию (smelling abstraction). MVP — split. В следующей итерации — добавить inline как syntactic sugar поверх split. Зафиксировано в open questions.
- **Управление `state="absent"` с удалением unit-файла**. Сейчас bundle-владелец должен сделать `file.content(path="/etc/runr/X.service", state="absent")` отдельно. Но `file.content` пока не поддерживает `state="absent"` — это open question с MVP. Принимаем компромисс: `runr.service(state="absent")` останавливает сервис, но НЕ удаляет файл; удалить нужно ручкой. Документировать.
- **Auto-discovery runr URL**. В будущем bosun читает `runr.host`/`runr.port` из inventory или env. Сейчас — флаг командной строки. Не блокер.
- **Concurrency`. ureq синхронный — start/stop происходит последовательно. Для bundle с 30 runr.service это будет 30 HTTP-запросов последовательно. На медленной ноде это значимо. Open question: батчинг.
- **`POST /units/reload` body**. Сейчас в chiit-клиенте передаётся пустой `{}`. Если runr API ожидает иное — выяснить при первом BDD-прогоне.
- **`stop` timeout**. Сейчас в spec не поддерживаем `stop_timeout` в `RunrServiceSpec`. Если нужно дольше — добавить opt-in поле в следующей итерации.
- **Path-detection для unit-файлов `/etc/runr/...`**. Эвристика «file.content writing into /etc/runr/ → mark runr_dirty». Подойдёт для MVP. Альтернатива — явная функция `runr.daemon_reload()` в Starlark, которую пользователь явно зовёт. TODO.

## Связь с существующими концептами

| Концепт wiki | Как используется |
|---|---|
| `concepts/runr-supervisor.md` | источник правды для HTTP API и формата INI-конфигов |
| `concepts/bosun-overview.md` | bosun-client получает runr-primitives как ещё один слой native API |
| `concepts/manifest-vs-task.md` | runr.service — manifest-global, не task-global (декларативное желаемое состояние) |
| `concepts/chiit-go-dsl.md` | паттерн idempotent (file.Create + DaemonReload + ListStatuses + Start) переносится в bosun |

## Следующие шаги после этой итерации

1. Inline-декларация: `runr.service(name=..., exec_start=..., user=...)` пишет unit-файл автоматически.
2. `file.content(state="absent")` для удаления unit'ов.
3. `service.validate` — проверка конфига перед reload (`nginx -t`, `pg_ctl -t`).
4. `service.health_check` — post-apply readiness probe.
5. Rollback при failed reload (особенно для PG).
6. Объединение с systemd-primitive (одна Starlark функция, разная реализация в зависимости от `inv.facts.init_system`).
