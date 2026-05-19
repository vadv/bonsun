---
title: "bosun ↔ runr + systemd + deferred-restart journal — Design"
date: 2026-05-19
status: draft (pending review)
author: dmitrivasilyev
supersedes_sections:
  - "вся секция «Архитектура»" (из docs/superpowers/specs/2026-05-19-bosun-runr-integration-design.md — это итерация заменяет ту черновую полностью)
related:
  - docs/superpowers/research/2026-05-19-runr-systemd-defers-research.md
  - docs/superpowers/specs/2026-05-19-bosun-bundle-architecture-design.md
  - docs/superpowers/specs/2026-05-18-bosun-client-mvp-design.md
  - .claude-memory-compiler/knowledge/concepts/runr-supervisor.md
  - .claude-memory-compiler/knowledge/concepts/chiit-go-dsl.md
---

# bosun ↔ runr + systemd + deferred-restart journal — Design

## Контекст и цель

После MVP bosun умеет ставить пакеты, писать файлы и рендерить шаблоны.
После bundle-rev-2 — раскладывает bundle на роли с `_lib/`, `inventory/`, тэгами
и module-relative `template()`. Чего не хватает для реальных нод парка:

1. **Управление долгоживущими процессами на ноде.** На KVM-нодах это runr
   (HTTP API + INI-конфиги в `/etc/runr/`); на гипервизорах и редких ноутбуках —
   systemd. В postgres-chiit оба источника обслуживаются параллельно, через
   собственный пакет `lib/runr/` + `lib/systemd/manager_dbus.go` (см. секцию 4
   и секцию 3 research-файла).
2. **Notify-driven restarts.** Изменение `file.content("/etc/nginx/nginx.conf")`
   должно тригерить `systemctl reload nginx` или `POST /api/v1/services/nginx/reload`
   на runr — но не сразу, а после успешного применения всех остальных ресурсов.
   chiit делает это через `defers` — журнал shell-скриптов на диске
   (см. `defers/process.go:14` для расположения и `defers/custom.go:35` для
   атомарной записи).
3. **Crash-resilience.** Если bosun упал между записью конфига и рестартом
   сервиса — следующий запуск должен довести начатое. chiit реплеит defers в
   начале и в конце каждого `chiit run` (`cmd/run.go:61` и `cmd/run.go:90`).
4. **Validation gate.** Сломанный `nginx.conf` не должен попасть к live-nginx.
   chiit делает это в `files.TemplateWithValidation` (`lib/files/file.go:84–156`):
   рендер в `<path>.new` → запуск валидатора → atomic rename → notify.
5. **Health check.** После рестарта сервис должен реально подняться. У chiit
   этого нет; для bosun это первый-класс концепт (`health_check_cmd=` /
   `health_check_url=`).
6. **Init-system абстракция.** `service.unit(name="nginx", ...)` пишется один
   раз и работает и на KVM (runr), и на гипервизоре (systemd). Дифференциация
   делается по факту `inv.facts.init_system`.

Эта итерация добавляет:

- Новые крейты `bosun-systemd-client` (через `zbus_systemd`) и
  `bosun-runr-client` (через `ureq`) — нативные клиенты, изолированные
  от Starlark.
- Модуль `defers` в `bosun-core` — журнал отложенных действий на tmpfs.
- Примитивы `runr.service` / `runr.timer` / `runr.cgroup`,
  `systemd.service` / `systemd.timer`, абстрактный `service.unit` и
  `command.run` (включая `deferred=True`).
- Параметр `validate_with=` на `file.content` / `file.template` / `service.unit`.
- Параметр `health_check_cmd=` / `health_check_url=` на `service.unit` /
  `runr.service` / `systemd.service`.
- `bosun status` — печатает pending defers.
- Метрики Prometheus: `bosun_defers_pending`, `bosun_defers_executed_total`,
  `bosun_defers_replay_total`.

После этой итерации демо-сценарий собирается end-to-end: pgbouncer-config →
`validate_with=["pgbouncer", "-V", "{new_path}"]` → at-least-once reload
с health-check на `127.0.0.1:6432`, причём при крэше bosun посреди apply'я
следующий запуск доводит reload до конца.

## Принципы

1. **At-least-once для notify-driven restarts.** Defers пишутся на диск до
   того, как могут быть выполнены. Bosun может упасть между записью и
   рестартом, и запись остаётся жить до успешного rerun. Это chiit-семантика
   (`defers/process.go:19` — replay file-by-file, при ошибке continue, не abort).
2. **Defers на tmpfs by design.** `/tmp/bosun-defers/` — это намеренный выбор.
   Reboot перезапускает все сервисы, поэтому накопленные за прошлый run
   defers устаревают. tmpfs гарантирует, что мы не унаследуем чужой запрос
   рестарта из past-life ноды. Если ОС стартует без tmpfs `/tmp` — место для
   defers всё равно `/tmp/`, в этом случае reboot обнулит каталог через
   `systemd-tmpfiles` (или эквивалент).
3. **Abstract `service.unit` + concrete `runr.*` / `systemd.*`.** Идея option 3
   из research-секции 6.7. `service.unit(...)` — sugar, диспатчится в
   `runr.service(...)` или `systemd.service(...)` по `inv.facts.init_system`.
   Power-user, которому нужен `[Log]` runr или systemd-specific
   `ConditionPathExists=`, использует concrete-функции напрямую.
4. **Validation before swap.** Конфиг рендерится в `<path>.new`, валидатор
   запускается на `<path>.new`. Если валидатор failed — `<path>.new` остаётся
   для forensics, `<path>` не трогается, defer **НЕ добавляется в журнал**.
   Это то же, что делает `TemplateWithValidation` в chiit
   (`files/file.go:90` — `validator(path + ".new")` перед `os.Rename`).
5. **`InvocationID` / `Restarts` diff verification.** После запроса
   `RestartUnit` мы не доверяем `JobRemoved{result=done}` (см. research 3, бага
   Debian 996911). Сравниваем `InvocationID` до и после; для runr — `Restarts`
   счётчик в `/api/v1/services/statuses`. Если до и после совпадает, рестарт
   **не произошёл** — это failed-defer.
6. **`Manager.NeedDaemonReload` once-per-converge.** В chiit
   `manager_dbus.go:85` для каждого `RestartUnit` сначала проверяется
   `NeedDaemonReload` свойство. bosun делает то же, плюс глобальный
   throttle — реальный `daemon-reload` через dbus за один apply вызывается
   максимум один раз.
7. **Дедуп по `(action, target)`, restart субсумирует reload.** То же правило,
   что в chiit: `defers/systemd.go:25` (`AddRestart*` вызывает `RemoveReload*`),
   `defers/systemd.go:52` (`AddReload*` no-op если есть `restart-*`). Приоритет:
   `restart > reload_or_restart > reload`.
8. **Никаких silent fallbacks.** Если systemd dbus недоступен — exit с
   диагностическим сообщением; если runr недоступен — `PrimitiveError::RunrUnavailable`,
   классифицируемый как `Outcome::Deferred` (retry next cycle).
9. **#[non_exhaustive] для всех public enum.** Workspace-конвенция; читается
   во всех `Spec`/`Error`/`Outcome`-типах.

## Что в scope MVP интеграции

- `bosun-systemd-client` крейт — async через `zbus`/`zbus_systemd`,
  sync-фасад для примитивов (через tokio current-thread runtime).
- `bosun-runr-client` крейт — sync через `ureq`.
- `defers` модуль в `bosun-core` — JSON-per-file журнал в
  `/tmp/bosun-defers/`.
- В `bosun-core::Resource` — новое поле `restart_on: Vec<ResourceId>`.
- В `bosun-core::Orchestrator::apply` — двойной replay (до и после), tracking
  `ApplyCtx.changed_resources: Rc<RefCell<HashSet<ResourceId>>>`.
- Примитивы: `runr.service`, `runr.timer`, `runr.cgroup`,
  `systemd.service`, `systemd.timer`, `service.unit` (абстрактный),
  `command.run` (с `deferred=True`/`only_if=`/`not_if=`).
- `validate_with=` на `file.content`, `file.template`, `service.unit`.
- `health_check_cmd=` / `health_check_url=` на `service.unit` /
  `runr.service` / `systemd.service`.
- INI-генератор для runr-unit-файлов — Starlark функция
  `render_service(...)` в `_lib/runr/main.star`, вызывает `template()`.
  То же для `render_timer`, `render_cgroup`.
- `bosun status` subcommand — печатает pending defers.
- Метрики: `bosun_defers_pending`, `bosun_defers_executed_total{result}`,
  `bosun_defers_replay_total`.
- Расширение `bosun-facts::init_system` — добавить распознавание `runr`
  (поверх `systemd`/`runit`/`init`/`unknown`).
- BDD-сценарии: defer durability, dedup, validation failure, health-check
  failure, cross-resource notify, abstract dispatch.

## Что НЕ в scope этой итерации

- **Cross-bundle notify identity.** Все notify-таргеты — Handle, видимые в
  evaluation области текущего bundle. Эскейп-хэтч `service.handle_by_name(...)`
  (research 6.5) — следующая итерация.
- **Polkit non-root deployment.** В research 3 описано: для не-root
  bosun нужно `org.freedesktop.systemd1.manage-units` правило в
  `/etc/polkit-1/rules.d/`. В MVP интеграции **bosun запускается как root**;
  при попытке выполнить systemd action не как root возвращается
  диагностическая ошибка, не silent fallback.
- **Validator argument templating sophistication.** Текущий MVP принимает
  плейсхолдер `{new_path}` (research 9 пункт 8). Поддержка произвольных
  плейсхолдеров (`{path}`, `{owner}`, `{group}`) и условных выражений — позже.
- **Health-check retry policy с unbounded retries.** chiit-style "retry forever"
  опасен (research 6.3). MVP делает фиксированные N попыток (default 3),
  после чего файл defer переименовывается в `<id>.manual_clear`. Оператор
  чистит руками или вручную `bosun status --clear <id>`.
- **`file.content(state="absent")` с удалением unit-файла.** `runr.service(state="absent")`
  / `systemd.service(state="absent")` останавливают сервис, но НЕ удаляют
  файл. Удаление — отдельная итерация вместе с `file.content(state="absent")`.
- **Системд unit `EnableUnitFiles` с пользовательскими targets/wants/aliases.**
  MVP делает `EnableUnitFiles([name], runtime=false, force=true)` —
  стандартный preset.
- **runr `syslog.conf` декларация.** Отдельный концепт (research 4, конец
  секции про runr), сейчас не критичен.
- **Замена `systemctl daemon-reload` через shell.** В runr-init mode
  `systemctl` — это симлинк на `runr` (research 4), поэтому для runr-init
  pod'ов мы НЕ зовём dbus. Используется runr HTTP API напрямую (см. ниже
  «runr-init detection»).
- **Удалённое управление runr/systemd на других нодах.** Только локальный
  `127.0.0.1:8010` для runr, только system bus для systemd.
- **Bosun-side rollback при failed reload/restart.** Если новый конфиг
  валиден, но сервис не поднялся (health-check failed) — defer остаётся в
  failed-state, конфиг **не откатывается**. PG-критичный auto-rollback —
  отдельный спек.

## Архитектура

### Новый крейт: `bosun-systemd-client`

Async HTTP-equivalent для dbus. Изоляция от Starlark позволяет тестировать
через `dbus-mock` (см. секцию «Тестирование»).

```
bosun-client/crates/bosun-systemd-client/
├── Cargo.toml
└── src/
    ├── lib.rs                 # pub API
    ├── manager.rs             # SystemdManager (async)
    ├── facade.rs              # sync-фасад через tokio current-thread runtime
    ├── types.rs               # UnitInfo, UnitProperties, JobResult, ServiceState
    ├── error.rs               # SystemdError enum
    └── job_watch.rs           # JobRemoved subscription + матчинг по object path
```

#### Зависимости

- `zbus = { version = "5", features = ["tokio"], default-features = false }`
- `zbus_systemd = "0.25"` — preset proxy для `org.freedesktop.systemd1`
- `tokio = { version = "1", features = ["rt", "macros", "sync", "time"] }`
- `serde`, `serde_json`, `thiserror`
- В dev: `dbus-mock` (Python harness через `pytest-dbusmock`) — запускается
  в BDD; `tokio-test` для unit-тестов сценариев совместимости с zbus API.

#### Async API

`SystemdManager` оборачивает `zbus::Connection` и
`zbus_systemd::systemd1::ManagerProxy<'static>`. Все методы — `async fn ...
-> Result<_, SystemdError>`. Набор:

- `connect_system()` → открывает system bus и binding'ит ManagerProxy.
- `daemon_reload()` — `Manager.Reload()`. Throttled через
  `needs_daemon_reload()` (свойство `NeedDaemonReload` на ManagerProxy).
- `start_unit/stop_unit/restart_unit/reload_unit/reload_or_restart_unit(name)`
  — соответствующие `Manager.<X>Unit(name, "replace")` вызовы, возвращают
  `JobHandle` (newtype над `OwnedObjectPath`).
- `enable_unit/disable_unit(name)` — `EnableUnitFiles([name],
  runtime=false, force=true)` / симметрично.
- `unit_info(name)` — `GetUnit(name)` + чтение свойств `ActiveState`,
  `SubState`, `InvocationID`, `ExecMainStartTimestamp`.
- `wait_for_job(handle, timeout)` — подписка на `JobRemoved`, матчинг по
  object path, возврат `JobResult { result: String, active_state: String }`
  (после `result=done` дополнительно проверяется
  `ActiveState=="active"`, см. research-секция 3 «`JobRemoved` может
  отчитаться done при `ActiveState=failed`»).

#### Sync-фасад

Примитивы синхронные. `BlockingSystemdManager` поднимает
`tokio::runtime::Builder::new_current_thread().enable_all().build()` один
раз при `connect_system()`, и в каждом блокирующем методе делает
`rt.block_on(async { ... })`. Конкретные методы — `restart_unit_blocking`,
`reload_unit_blocking`, `start_unit_blocking`, `stop_unit_blocking`,
`enable_unit_blocking`, `disable_unit_blocking`, `unit_info_blocking`,
`daemon_reload_blocking`, `needs_daemon_reload_blocking` — каждый
делегирует в соответствующий async-метод + `wait_for_job` где нужно.

`BlockingSystemdManager` создаётся один раз в `bosun-cli/src/run.rs`,
оборачивается в `Rc<...>` и инжектится в каждый systemd-примитив через
`ApplyCtx.systemd`.

#### `InvocationID` verification

После `restart_unit_blocking` примитив фетчит свойство `InvocationID`
через `unit_info_blocking()`. Snapshot до и после: если совпадают →
`SystemdError::RestartNotObserved { unit }`. Это сигнал «job отчитался
done, но рестарт не произошёл» (см. research-секция 3, baга Debian 996911).

#### `SystemdError` enum

`#[non_exhaustive]`, реализован через `thiserror::Error`. Варианты:
`BusUnavailable { source: zbus::Error }`,
`Dbus(#[from] zbus::Error)`,
`NoSuchUnit(String)`,
`AuthorizationDenied { action: String, unit: String }` — polkit отказ,
`JobFailed { job: String, result: String, active_state: String }`,
`RestartNotObserved { unit: String }`,
`Timeout(Duration)`,
`Io(#[from] std::io::Error)`.

### Новый крейт: `bosun-runr-client`

Sync HTTP-клиент. Заменяет одноимённую черновую секцию из предыдущего
runr-integration спека (вся та черновая — superseded).

```
bosun-client/crates/bosun-runr-client/
├── Cargo.toml
└── src/
    ├── lib.rs                 # pub API
    ├── client.rs              # struct Client + методы
    ├── types.rs               # ServiceStatus, TimerStatus, UnitListItem, ActionAck, DaemonInfo
    ├── error.rs               # RunrError enum
    └── verify.rs              # polling-based verification: Restarts/StartedAt diff
```

#### Зависимости

- `ureq = "2"` — синхронный HTTP без runtime.
- `serde`, `serde_json`, `thiserror`.
- В dev: `wiremock` (а не `mockito` — wiremock даёт более чистый sync API
  через `wiremock::matchers`, поддерживается активно в 2025–2026).

#### `Client` API

`Client { base_url, http: ureq::Agent, timeout: Duration }` с конструктором
`new(base_url, timeout)`. Sync методы возвращают `Result<_, RunrError>`:
`daemon_info()`, `daemon_reload()`, `service_start(name, idempotent: bool)`,
`service_stop(name, force: bool, timeout_humantime: Option<&str>)`,
`service_restart(name)`, `service_reload(name)`, `timer_start(name)`,
`timer_stop(name)`, `timer_enable(name, now: bool)`,
`timer_disable(name, now: bool)`, `service_statuses() ->
Vec<ServiceStatus>`, `timer_statuses() -> Vec<TimerStatus>`,
`units_list() -> Vec<UnitListItem>`. См. полный chiit-client в research
секция 4.

#### `Restarts` diff verification

Runr не отдаёт `JobRemoved`-эквивалент (research 4). Чтобы убедиться, что
рестарт произошёл, делается snapshot `ServiceStatus` до, выполняется
restart, polling-loop с `poll_interval` до `poll_total` deadline: каждую
итерацию `service_statuses()` → find `name` → если `restarts >
before.restarts && state == "Running"` → `Ok(())`. По исходу deadline →
`RunrError::RestartNotObserved { unit }`. Аналогично для reload — но там
вместо `restarts` смотрим, что `state == "Running"` не упал в `Failed`
после операции.

#### `RunrError` enum

`#[non_exhaustive]` через `thiserror`. Варианты:
`Unavailable { base_url, source: Box<dyn Error + Send + Sync> }` —
connection refused / DNS / TCP error;
`ApiError { status: u16, body: String }` — non-2xx с body excerpt;
`BadResponse(String)` — invalid JSON в response;
`NotFound { kind: String, name: String }` — 404 для конкретного unit;
`RestartNotObserved { unit: String }` — verify-loop hit deadline.

#### runr-init detection

runr в pod-режиме симлинкается под `/usr/bin/systemctl` (research 4,
`runr/chiit.go:64`). Если bosun запускается рядом с runr-init pod, dbus
недоступен, но `systemctl restart foo` фактически вызывает runr-CLI и
выходит OK.

Bosun **не** использует shell `systemctl` в runr-init mode. Вместо этого
определяем runr-init через `bosun-facts::init_system` (которому в этой
итерации добавляется распознавание `runr` PID 1) и используем runr HTTP API
напрямую. Это:

- безопаснее (не шелл-инъекция),
- быстрее (нет fork+exec),
- даёт нам `Restarts` diff verification, недоступный через CLI.

В `bosun-cli/src/run.rs` логика:

```rust
let init = facts.get("init_system")?;
let runr_client = if init == "runr" || std::path::Path::new("/etc/runr").exists() {
    Some(Rc::new(runr_client::Client::new(args.runr_url, args.runr_timeout)))
} else {
    None
};
let systemd_client = if init == "systemd" {
    Some(Rc::new(BlockingSystemdManager::connect_system()?))
} else {
    None
};
```

Если `init == "runr"`, systemd_client = None. Если `init == "systemd"` и
`/etc/runr` тоже существует — оба клиента активны (это валидный сценарий
для гипервизора, где runr и systemd сосуществуют).

### Изменения в `bosun-core`

#### Новый модуль `defers`

```
bosun-client/crates/bosun-core/src/defers/
├── mod.rs
├── format.rs                 # DeferEntry struct + JSON serde
├── journal.rs                # Journal: list, write, remove, atomic ops
├── replay.rs                 # replay loop
├── action.rs                 # enum DeferAction + dispatcher to client
└── priority.rs               # priority rules + dedup
```

#### Расширение `Resource`

```rust
pub struct Resource {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub spec_version: u16,
    pub payload: serde_json::Value,
    pub reload_on: Vec<ResourceId>,
    pub restart_on: Vec<ResourceId>,        // NEW
    pub depends_on: Vec<ResourceId>,
}
```

В `Registry::topological_order` все три вектора участвуют как рёбра. Различие
в семантике обрабатывается в Orchestrator.

#### Расширение `PrimitiveError`

```rust
#[non_exhaustive]
pub enum PrimitiveError {
    // existing...
    RunrUnavailable { base_url: String, reason: String },
    SystemdUnavailable { reason: String },
    Validation { validator: String, stderr_excerpt: String },
    HealthCheckFailed { target: String, reason: String },
    DeferIo { path: PathBuf, reason: String },
}

impl PrimitiveError {
    pub fn is_deferrable(&self) -> bool {
        matches!(
            self,
            PrimitiveError::DpkgLocked { .. }
                | PrimitiveError::Cancelled
                | PrimitiveError::RunrUnavailable { .. }
                | PrimitiveError::SystemdUnavailable { .. }
        )
    }
}
```

`Validation`, `HealthCheckFailed`, `DeferIo` — **не deferrable**. Validation
failure — это структурная проблема bundle, retry не поможет.

#### `ApplyCtx` расширение

```rust
pub struct ApplyCtx {
    // existing
    pub deadline: Option<Instant>,
    pub cancel: tokio_util::sync::CancellationToken,
    pub sensitive: Rc<SensitiveStore>,
    pub facts: Rc<dyn FactsSource>,

    // NEW
    pub changed_resources: Rc<RefCell<HashSet<ResourceId>>>,
    pub defers: Rc<DeferJournal>,
    pub runr: Option<Rc<RunrHandle>>,        // RunrHandle = bosun_runr_client::Client wrapper
    pub systemd: Option<Rc<SystemdHandle>>,  // SystemdHandle = BlockingSystemdManager wrapper
}

impl ApplyCtx {
    pub fn is_changed(&self, id: &ResourceId) -> bool {
        self.changed_resources.borrow().contains(id)
    }
    pub fn record_changed(&self, id: &ResourceId) {
        self.changed_resources.borrow_mut().insert(id.clone());
    }
}
```

`Rc` вместо `Arc` — orchestrator однопоточный, держим консистентно с
`EvalState` (см. bundle-architecture-design.md → секция «EvalState»).

## defers журнал

### Layout

- Корень: `/tmp/bosun-defers/`. Создаётся при первом write,
  permissions `0700`, owner root.
- На каждое pending действие — один файл с именем
  `<sortkey>-<dedup-id>.deferred`.
- Файлы валидные JSON, расширение `.deferred` для filtering при `ls`.
- Дополнительные расширения для нестандартных состояний:
  - `.manual_clear` — defer, который N раз не смог выполниться. Требует
    оператор интервенции.
  - `.tmp.<nanos>` — staging файл, ещё не закоммичен через rename.

```
/tmp/bosun-defers/
├── r0-systemd.restart:nginx.deferred
├── r1-runr.reload_or_restart:postgres.deferred
├── r2-command.run:hup-pg-doorman.deferred
└── r0-systemd.restart:bad-config.manual_clear
```

### Sortkey + дедуп

`<sortkey>` — двухсимвольный префикс, отражающий приоритет:

| Префикс | Action | Приоритет |
|---|---|---|
| `r0` | restart | highest — выполняется первым |
| `r1` | reload_or_restart | medium |
| `r2` | reload | lowest |
| `c0` | command.run (deferred) | после всех service-actions |

`<dedup-id>` — `<init_system>.<action>:<target>`. Например
`systemd.restart:nginx` или `runr.reload:postgres`. Для `command.run` —
`command.run:<user-defined-name>` (имя берётся из Starlark `command.run(name=...)`).

**Дедуп правила:**
1. При попытке вставить `r2-systemd.reload:nginx` если уже есть
   `r0-systemd.restart:nginx` — no-op (restart субсумирует reload).
2. При вставке `r0-systemd.restart:nginx` если есть
   `r2-systemd.reload:nginx` — удаляем reload-файл, пишем restart-файл.
3. При вставке `r1-systemd.reload_or_restart:nginx`:
   - если есть `r0-...:nginx` — no-op (restart выше);
   - если есть `r2-...:nginx` — удаляем reload, пишем reload_or_restart;
   - если нет ни одного — пишем reload_or_restart.
4. Идемпотентный insert того же файла — no-op (content-stable, см. chiit
   `defers_test.go:36`).

Это соответствует chiit's `defers/systemd.go:25` (`AddRestart*` → `RemoveReload*`)
и `defers/systemd.go:52` (`AddReload*` no-op при наличии restart).

### Формат файла

```json
{
  "spec_version": 1,
  "id": "systemd.restart:nginx",
  "action": "restart",
  "init_system": "systemd",
  "target": "nginx.service",
  "validate_cmd": null,
  "health_check": {
    "kind": "url",
    "url": "http://127.0.0.1/healthz",
    "expected_status": 200,
    "timeout_sec": 10
  },
  "priority": "restart",
  "enqueued_at": "2026-05-19T14:32:11Z",
  "enqueued_by": [
    "file.content:/etc/nginx/nginx.conf",
    "file.content:/etc/nginx/sites-enabled/default"
  ],
  "attempt_count": 0,
  "max_attempts": 3
}
```

Поля:

- `spec_version` — версия формата, для совместимости при росте.
- `id` — dedup key, повторяется в имени файла.
- `action` — `start | stop | restart | reload | reload_or_restart | command.run`.
- `init_system` — `systemd | runr`. Определяет, через какой клиент диспатчить.
- `target` — для service-actions имя unit'а; для `command.run` — shell-команда.
- `validate_cmd` — `Option<Vec<String>>`. Если есть — выполняется перед
  service-action. См. секцию «Validation».
- `health_check` — `Option<HealthCheck>`. Структурное `{kind: cmd|url, ...}`.
  См. секцию «Health check».
- `priority` — `restart | reload_or_restart | reload | command`. Дублирует
  prefix sortkey, для удобства читалок (без парсинга имени файла).
- `enqueued_at` — UTC timestamp в RFC3339.
- `enqueued_by` — список ResourceId-источников, которые тригернули defer.
  Чисто для дебага; в логике replay не используется.
- `attempt_count` — счётчик попыток. Инкрементируется в начале каждой
  попытки (через rewrite файла).
- `max_attempts` — default 3, конфигурируется через CLI `--defer-max-attempts`.

### Атомарность

Запись:
1. Создать `<dir>/<id>.deferred.tmp.<nanos>` с `O_RDWR|O_CREAT|O_TRUNC`,
   permissions `0600`.
2. `write_all(json_bytes)`.
3. `file.sync_all()` — fsync файла.
4. `file.close()`.
5. `rename(tmp, final)` — POSIX atomic.
6. **`fsync(dir)`** — добавляем относительно chiit (research отметил, что
   chiit пропускает это, что критично на `data=writeback`).

Удаление:
1. `unlink(final)`.
2. `fsync(dir)`.

`fsync` на каталог обеспечивает persistent visibility rename/unlink даже
при kernel crash сразу после операции.

### Replay протокол

```rust
pub fn replay(journal: &Journal, ctx: &ReplayCtx) -> ReplayReport {
    let mut report = ReplayReport::default();
    let entries = journal.list_sorted()?;          // lex-sorted by filename
    for entry in entries {
        let _span = info_span!("defer", id = %entry.id).entered();
        match dispatch(&entry, ctx) {
            Ok(_) => {
                journal.remove(&entry)?;
                report.executed += 1;
            }
            Err(DeferReplayError::ClientUnavailable) => {
                report.skipped_unavailable += 1;
            }
            Err(DeferReplayError::Action(e)) => {
                journal.bump_attempt(&entry, &e)?;
                if entry.attempt_count + 1 >= entry.max_attempts {
                    journal.move_to_manual_clear(&entry)?;
                    report.promoted_to_manual_clear += 1;
                } else {
                    report.failed += 1;
                }
            }
        }
    }
    report
}
```

Ключевые поведения:
- Lex-order по filename: `r0-...` идёт раньше `r1-...` идёт раньше `r2-...`,
  это нужный приоритет (research 6.4: chiit использует `sort.Strings`).
- `ClientUnavailable` (runr daemon down или dbus отключён) — defer остаётся,
  будем пробовать ещё раз. Это transient.
- `Action` failure — bump attempt, при превышении max — promotion в
  `.manual_clear`. Этим оператор информируется, что нужно интервенция.
- При успешном выполнении файл удаляется + fsync(dir).
- Логирование per-defer через tracing span с полями `id`, `action`, `target`,
  `result`.

### Когда вызывается replay

В `bosun-cli/src/run.rs` orchestrator вызывает defer-replay дважды:

```rust
let journal = Journal::open("/tmp/bosun-defers")?;

// 1. До apply: догоняем предыдущий run, если был interrupted.
let pre_report = defers::replay(&journal, &ReplayCtx { systemd, runr, .. })?;
metrics::record_replay(&pre_report);

// 2. Сам apply. Примитивы пишут новые defers в journal.
let apply_report = orchestrator.apply(&registry, &ctx)?;

// 3. После apply: исполняем новые defers, которые накопились этой фазой.
let post_report = defers::replay(&journal, &ReplayCtx { systemd, runr, .. })?;
metrics::record_replay(&post_report);
```

Это симметрично chiit-схеме (`cmd/run.go:61` и `cmd/run.go:90`).

### Pruning

- Если target unit deleted (роль убрана из bundle), defer всё равно
  выполняется. systemd вернёт `NoSuchUnit` → bosun логирует warning,
  remove файл (как у обычного успеха — defer выполнен, unit-нет — мы
  не виноваты).
- Если defer старше 7 суток (от `enqueued_at` до `now`) — replay делает
  одну попытку, при неудаче сразу `.manual_clear`. Не ждём бесконечно.
- `bosun status --clear <id>` (subcommand, см. секцию CLI) — оператор
  убирает defer вручную.

### Observability

В `metric.rs`:

- `bosun_defers_pending` — gauge, кол-во файлов в `/tmp/bosun-defers/`
  (исключая `.tmp.*` и `.manual_clear`).
- `bosun_defers_executed_total{result="ok"|"client_unavailable"|"failed"|"manual_clear"}` —
  counter.
- `bosun_defers_replay_total{phase="pre"|"post"}` — counter, инкрементируется
  на каждый replay.

Tracing:

- Span `defer` per replay entry с полями `id`, `action`, `target`, `result`.
- Span `defer_replay` per replay call с фазой (`pre`/`post`).

## systemd integration via dbus

### Flow для `systemd.service` apply

1. **Daemon-reload throttle.** В `ApplyCtx.systemd_daemon_reload_done: Cell<bool>`
   флаг «уже вызвали». Если ещё нет — `needs_daemon_reload()`; если true —
   `daemon_reload_blocking()`, флаг устанавливается. Это гарантирует один
   reload на весь apply (research-секция 6: chiit зовёт `daemon-reload`
   до каждого RestartUnit, что расточительно).
2. **Pre-snapshot** через `unit_info_blocking(name)` — `before`.
3. **EnableUnitFiles** если `spec.enable && !before.is_enabled`.
4. **`decide_action(spec, before, ctx)`** — см. ниже.
5. **Defer-eligible** (`Restart`/`Reload`/`ReloadOrRestart` от notify) →
   `ctx.defers.enqueue(...)` + `ChangeReport::deferred`.
6. **Synchronous** (`Start`/`Stop` от desired-state-diff) → выполнить
   через client + `InvocationID` diff verification.
7. После успешного действия — если `spec.health_check` задан → запустить.
   Failure → `PrimitiveError::HealthCheckFailed`.

### Decide action

Matrix `(spec.state, before.active_state)`:

| desired \ current | "active" | not "active" |
|---|---|---|
| `Running` | `Restart` если restart_on triggered, `Reload` если reload_on triggered, иначе `NoChange` | `Start` |
| `Stopped` | `Stop` | `NoChange` |
| `Absent` | `Stop` | `NoChange` |

Reload/Restart — оба deferred (at-least-once через journal). Start/Stop —
синхронные (это desired-state-change, не reaction-on-config).

### Polkit / root assumption

`BlockingSystemdManager::connect_system` пытается открыть system bus.
Если bosun не root и polkit отказывает — `SystemdError::AuthorizationDenied`.
Bosun **не** обходит polkit. Сообщение об ошибке инструктирует: либо
root, либо добавить правило в `/etc/polkit-1/rules.d/` (см. research-секция 3,
конец «Failure modes»). Non-root deployment — out-of-scope MVP, open
question.

## runr integration via HTTP

### Flow для `runr.service` apply

Симметрично systemd-flow:

- `daemon_reload` через `POST /api/v1/units/reload`, кешируется через
  `ctx.runr_daemon_reload_done: Cell<bool>`. В отличие от systemd, нет
  свойства «нужен ли reload» — мы либо делаем один раз unconditionally,
  либо если был хоть один `file.content` пишущий в `/etc/runr/`.
- Нет `EnableUnitFiles` — `Autostart` runr-units определяется в INI
  (`Autostart=true`), парсится при `daemon-reload`.
- Pre-snapshot через `service_statuses()` (один HTTP-call на весь apply
  через `OnceCell<Vec<ServiceStatus>>`).
- `decide_action_runr` — та же matrix.
- Synchronous Restart → `service_restart(name)` + `verify_restart(client,
  name, before, 500ms, 30s)` (`Restarts` diff).
- На любую `RunrError::Unavailable` → `PrimitiveError::RunrUnavailable`
  (deferrable: defer остаётся, retry next cycle).
- Health-check после успеха — как у systemd.

### Cancel/deadline propagation

В loop verify внутри `verify_restart` проверяются `ctx.cancel.is_cancelled()`
и overrun `ctx.deadline`. Срабатывание → `PrimitiveError::Cancelled`
или `PrimitiveError::Deadline`.

## Native primitives

### `runr.service`

```rust
#[derive(serde::Deserialize, Debug, Clone)]
pub struct RunrServiceSpec {
    pub name: String,
    pub state: ServiceState,
    #[serde(default)]
    pub enable: bool,
    pub health_check: Option<HealthCheck>,
    pub validate_with: Option<Vec<String>>,
}

#[non_exhaustive]
#[derive(serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState { Running, Stopped, Absent }
```

Starlark sketch:

```python
load("@bosun/builtins", "runr", "file", "template")
load("@lib/runr", render_service = "render_service")

# Шаблон INI пишется явно через _lib/runr/.
nginx_unit = file.content(
    path     = "/etc/runr/nginx.service",
    contents = render_service(
        exec_start = "/usr/sbin/nginx -g 'daemon off;'",
        user = "www-data",
        autostart = True,
    ),
    mode     = 0o644,
)

nginx_conf = file.content(
    path     = "/etc/nginx/nginx.conf",
    contents = template("nginx.conf.j2"),
    mode     = 0o644,
    validate_with = ["nginx", "-t", "-c", "{new_path}"],
)

runr.service(
    name       = "nginx",
    state      = "running",
    depends_on = [nginx_unit],
    reload_on  = [nginx_conf],
    health_check_url = "http://127.0.0.1/healthz",
)
```

`_lib/runr/main.star` экспортирует три функции: `render_service`,
`render_timer`, `render_cgroup`. Каждая принимает `**kwargs` по всем полям
из research-секции 4 («Формат unit-файла на диске»: `[Service]`/`[Timer]`/
`[Cgroup]`/`[Log]`) и вызывает `template("service.j2", vars=kwargs)` /
`timer.j2` / `cgroup.j2`. Шаблоны живут в `_lib/runr/templates/`.

Шаблон — обычный jinja через minijinja с `Strict` undefined behavior
(тот же `template()`-механизм MVP). Соглашение: `Optional`-поля
сворачиваются через `{% if x %}` блоки; `[Log]` секция целиком включается
только если задано хотя бы одно `log_*` поле; `Environment=` через цикл
по списку. Полная грамматика INI описана в research секция 4 и
не переписывается тут.

### `runr.timer` и `runr.cgroup`

`RunrTimerSpec`:

```rust
pub struct RunrTimerSpec {
    pub name: String,
    pub state: TimerState,
    #[serde(default)]
    pub start_now: bool,
}

#[non_exhaustive]
pub enum TimerState { Enabled, Disabled, Absent }
```

`RunrCgroupSpec`:

```rust
pub struct RunrCgroupSpec {
    pub name: String,
    pub state: CgroupState,
}

#[non_exhaustive]
pub enum CgroupState { Present, Absent }
```

Cgroup-апplyer ничего не делает кроме чёткой регистрации в `units_list`
(decision: `runr.cgroup` существует только потому что cgroup-юниты висят
в общем потоке reload-цикла; реальная сила — в `file.content` пишущем
INI на диск).

### `systemd.service` и `systemd.timer`

`SystemdServiceSpec` повторяет `RunrServiceSpec` (`name`, `state`, `enable`,
`health_check`, `validate_with`). `SystemdTimerSpec` — `name`,
`state: TimerState`, `enable: bool`. Starlark API совпадает по форме —
заменяется только namespace (`systemd.service` вместо `runr.service`,
unit-файл пишется в `/etc/systemd/system/` вместо `/etc/runr/`).

### Абстрактный `service.unit`

`service.unit` — это **Rust-функция, зарегистрированная в Starlark
globals**, не primitive. Сигнатура поведенчески эквивалентна:
`def service.unit(name, state, **kwargs)` — внутри читает
`inv.facts.init_system` и делегирует в `runr.service(args, eval)` или
`systemd.service(args, eval)`. Для значений `mixed-systemd-runr` primary —
systemd. На `unknown` или None — fail с сообщением «use runr.service or
systemd.service explicitly».

`service.unit` принимает только параметры, **общие** для runr и systemd —
`name`, `state`, `enable`, `health_check_*`, `validate_with`, `reload_on`,
`restart_on`, `depends_on`. Init-specific параметры (runr `[Log]`,
systemd `ConditionPathExists=`) ведут к unexpected-keyword fail —
вынуждая power-user идти на concrete-функции. Это сужение — фича, не баг:
bundle, рассчитанный быть переносимым между init-системами, не должен
случайно использовать init-specific knob.

### `command.run`

```rust
#[derive(serde::Deserialize, Debug, Clone)]
pub struct CommandRunSpec {
    pub name: String,
    pub cmd: Vec<String>,
    #[serde(default)]
    pub deferred: bool,
    pub only_if: Option<Vec<String>>,
    pub not_if: Option<Vec<String>>,
    pub timeout_sec: Option<u32>,
    pub working_directory: Option<String>,
    pub environment: Option<BTreeMap<String, String>>,
}
```

Starlark sketch:

```python
load("@bosun/builtins", "command")

# Сразу выполнить.
command.run(
    name = "sysctl-apply",
    cmd  = ["sysctl", "-p", "/etc/sysctl.d/60-bosun.conf"],
    only_if = ["test", "-f", "/etc/sysctl.d/60-bosun.conf"],
)

# Отложить до конца apply (deferred journal).
command.run(
    name = "hup-pg-doorman",
    cmd  = ["pkill", "-HUP", "pg_doorman"],
    deferred = True,
    only_if = ["pgrep", "pg_doorman"],
)
```

`only_if` / `not_if` — точно как у chiit (`chiit/lib/providers/command/types.go`):
команда, exit-code 0 = true, !=0 = false.

`deferred=True` → апplyer кладёт в journal с sortkey `c0-command.run:<name>`
и не выполняет сразу. Дедуп — по `name`.

## Validation pattern

`validate_with` принимает `Vec<String>` с одним поддерживаемым
плейсхолдером — `{new_path}`. Алгоритм для `file.content`:

1. Подготовить `<path>.new`: если `<path>` существует — скопировать как
   стартовую точку (даёт diff между `.new` и target); иначе — пустой файл.
2. Записать contents в `<path>.new` с целевыми permissions/owner/group
   через tempfile + rename внутри той же FS.
3. Если содержимое `<path>.new` идентично `<path>` → `<path>.new`
   удаляется, `NoChange`.
4. Если `validate_with` задан — substitute `{new_path}`, выполнить через
   `std::process::Command` (timeout 30s по умолчанию). Exit !=0 →
   `PrimitiveError::Validation { validator, stderr_excerpt }`. Файл
   `<path>.new` **остаётся на диске для forensics**, target не трогается,
   `ctx.record_changed` НЕ вызывается → defer не enqueue'ится.
5. Validation passed → `rename(<path>.new, <path>)` (атомарно), notify
   через `ctx.record_changed(&resource.id)`.

Если `validate_with` не задан — backward-compat путь `<path>.tmp` →
`rename` без промежуточного `.new` (как в MVP).

Для `service.unit` `validate_with` — валидатор запускается перед
dispatching restart/reload defer. Файла `.new` нет; это «query validator
на текущий target»: nginx -t смотрит на `/etc/nginx/nginx.conf`,
pg_doorman -t на `/etc/pg_doorman/pg_doorman.toml`. Failure → defer не
enqueue.

## Health check

`HealthCheck` enum (`#[non_exhaustive]`, `#[serde(tag="kind",
rename_all="snake_case")]`) с двумя вариантами:

- `Cmd { cmd: Vec<String>, timeout_sec, retry_count, retry_interval_sec }`.
- `Url { url: String, expected_status: Option<u16>, timeout_sec, retry_count,
  retry_interval_sec }`.

Все полей `timeout_sec`/`retry_count`/`retry_interval_sec` — `Option<u32>`.

Starlark маппит keyword-аргументы `health_check_cmd=`, `health_check_url=`,
`health_check_expected_status=`, `health_check_retry=`,
`health_check_interval=` (humantime) в эту структуру. `cmd` и `url` —
взаимоисключающие; задание обоих → eval-error.

Поведение:

- После успешного restart/reload или прохождения `decide_action=NoChange` —
  выполняется health-check.
- Cmd: spawn, ждать exit, retry до `retry_count` (default 3) с интервалом
  `retry_interval_sec` (default 2).
- Url: GET через `ureq`, проверка status code (default 200), retry с теми
  же defaults.
- Failure → `PrimitiveError::HealthCheckFailed { target, reason }`.
- В defer-context: failure инкрементирует `attempt_count` defer-файла, при
  превышении `max_attempts` файл переезжает в `.manual_clear`.

## Notify wire-up

В Resource — три вектора: `depends_on`, `reload_on`, `restart_on`. Все три
участвуют в топологическом порядке (рёбра графа). Семантика различается
в Orchestrator: после успешного apply Orchestrator зовёт
`ctx.record_changed(&resource.id)` если `report.is_changed()`.

Каждый service-primitive при apply проверяет `need_restart =
resource.restart_on.iter().any(|id| ctx.is_changed(id))` и `need_reload =
resource.reload_on.iter().any(|id| ctx.is_changed(id))`. Если
`need_restart` → enqueue `restart` defer. Если `need_reload &&
!need_restart` → enqueue `reload` defer. Дедуп в journal даёт правило
«restart субсумирует reload» — см. секцию «defers журнал → Дедуп правила».

**Cross-target dedup** работает через journal: два разных file.content,
notify-ящие один service, дают **один** defer в journal (имя файла =
`r0-systemd.restart:nginx`, повторная запись — no-op).

## Init-system fact

В bosun-facts уже есть `init_system.rs` (распознаёт `systemd | runit | init | unknown`).
**Эта итерация добавляет**:

- Распознавание `runr` PID 1. Логика: если `/proc/1/comm == "runr"` или
  `/usr/bin/runr` -> realpath матчит PID 1 cmdline.
- Дополнительно — если `/etc/runr/` существует и `init_system` определён
  как `systemd` или `unknown` — fact становится `mixed-systemd-runr`. Это
  валидный сценарий для гипервизора, где systemd PID 1 запускает runr как
  один из своих сервисов, и runr управляет под-сервисами.

Значения после расширения:

| Значение | Раскрытие |
|---|---|
| `systemd` | PID 1 = systemd, `/etc/runr/` отсутствует |
| `runr` | PID 1 = runr (контейнерный runr-init режим) |
| `mixed-systemd-runr` | PID 1 = systemd, но `/etc/runr/` существует — оба клиента активны |
| `runit` | PID 1 = runit |
| `init` | PID 1 = SysV init |
| `unknown` | не распознано |

В `bosun-cli/src/run.rs` инициализация клиентов:

```rust
let init = facts.get("init_system")?.unwrap_or_default();
let runr = if matches!(init, "runr" | "mixed-systemd-runr") {
    Some(Rc::new(runr_client::Client::new(args.runr_url, args.runr_timeout)))
} else { None };
let systemd = if matches!(init, "systemd" | "mixed-systemd-runr") {
    Some(Rc::new(BlockingSystemdManager::connect_system()?))
} else { None };
```

`service.unit` диспатчится по primary init (`runr` для `runr` и
`mixed-systemd-runr`, `systemd` для `systemd`); если у роли явно прописано
`runr.service` или `systemd.service`, оно работает напрямую с
соответствующим клиентом независимо от primary.

## CLI changes

### `bosun apply`

Добавляются флаги (в дополнение к существующим из bundle-rev-2):

```rust
#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    // existing...
    #[arg(long, default_value = "http://127.0.0.1:8010")]
    pub runr_url: String,

    #[arg(long, default_value_t = 10)]
    pub runr_timeout_sec: u32,

    #[arg(long, default_value = "/etc/runr")]
    pub runr_config_dir: PathBuf,

    #[arg(long, default_value = "/tmp/bosun-defers")]
    pub defers_dir: PathBuf,

    #[arg(long, default_value_t = 3)]
    pub defer_max_attempts: u32,
}
```

Exit-коды (unchanged from MVP/bundle-rev-2):
- 0 — apply ok / dry-run no drift
- 1 — apply partial fail (включая health-check failed defer'ы)
- 2 — dry-run drift detected
- 3 — manifest/eval error
- 4 — CLI/environment error

### `bosun status`

Новая subcommand:

```
bosun status [--defers-dir=/tmp/bosun-defers] [--format=text|json] [--clear=<id>]
```

Поведение:

- Default — печатает таблицу pending defers (включая `.manual_clear`):
  ```
  ID                                  STATE        ACTION  TARGET          ATTEMPTS  ENQUEUED_AT
  systemd.restart:nginx               pending      restart nginx.service   0/3       2026-05-19T14:32:11Z
  runr.reload:postgres                pending      reload  postgres        1/3       2026-05-19T14:30:05Z
  systemd.restart:bad-config          manual_clear restart bad-config      3/3       2026-05-18T09:11:42Z
  ```
- `--format=json` — JSON-array всех defer-entries.
- `--clear=<id>` — удаляет файл defer'а (любого state) и выходит. Логирует
  warning, что defer удалён вручную.
- `--clear-all-manual` — удаляет все `.manual_clear` файлы.

Exit code:
- 0 — defers есть или нет, всё OK.
- 1 — есть хотя бы один `.manual_clear` (signal для оператора).
- 4 — IO error (директория недоступна).

## Observability

### Метрики (`bosun.prom` в Prometheus textfile)

```
bosun_defers_pending 4
bosun_defers_executed_total{result="ok"} 142
bosun_defers_executed_total{result="failed"} 3
bosun_defers_executed_total{result="client_unavailable"} 1
bosun_defers_executed_total{result="manual_clear"} 0
bosun_defers_replay_total{phase="pre"} 17
bosun_defers_replay_total{phase="post"} 17
bosun_runr_reachable 1
bosun_systemd_reachable 1
```

### Tracing spans

- `defer_replay { phase: "pre"|"post" }` — wrap'ает цикл реплея.
- `defer { id, action, target, init_system, result }` — per-entry.
- `service_action { primitive: "runr"|"systemd", action, target }` — per
  primitive apply.
- `validate { primitive, path, validator }` — per validation run.
- `health_check { primitive, target, kind: "cmd"|"url" }` — per health-check
  run.

Все span'ы — `tracing::info_span!`, не debug-span — это операционная
трасса, всегда видна.

## Тестирование

### Уровень 1 — Unit

**bosun-systemd-client:**
- `BlockingSystemdManager::restart_unit_blocking` — happy path через
  zbus_systemd's `ManagerProxy::receive_job_removed` mock. Используем
  `tokio-test` для синхронной фиксации.
- `InvocationID` diff: snapshot before, mock service возвращает changed
  id → `Ok`. Mock возвращает same id → `RestartNotObserved`.
- `BusUnavailable` сценарий: `Connection::system()` падает (нет dbus
  socket) → правильная ошибка.

**bosun-runr-client:**
- Все методы через `wiremock`: 200 OK, 404, 5xx, connection refused,
  timeout.
- `verify_restart`: 0 changes → `RestartNotObserved`; counter changed →
  `Ok`; timeout → error.

**bosun-core::defers:**
- Atomicity: `Journal::enqueue` пишет `.tmp.<nanos>` → rename → `fsync(dir)`.
  Тест через mock filesystem (`tempdir` + проверка наличия `.tmp.*` после
  partial-fail).
- Dedup правил: вставить reload → вставить restart → файл `r0-...` есть,
  `r2-...` удалён.
- Replay в порядке lex: defer-журнал с тремя файлами `r0-`, `r1-`, `r2-`,
  fake dispatcher складывает order; assert matches priority order.
- Manual-clear promotion: dispatch failing 3 раза → файл переименован.

**bosun-primitives::runr_service / systemd_service:**
- `decide_action` matrix: state desired × current ActiveState × notify
  flags → expected Action. Покрытие всех 16+ комбинаций.
- `apply` Mock-client запросов (мок-метод `RunrHandle`, `SystemdHandle`):
  при `Action::Restart` зовётся `service_restart`, при `Action::Reload` —
  `service_reload`.
- При defer-eligible action — `ctx.defers.enqueue` зовётся, real action
  НЕ зовётся.
- При validation-failure не пишется defer; возвращается
  `PrimitiveError::Validation`.

**bosun-primitives::command_run:**
- `only_if` exit-0 → run; exit !=0 → skip.
- `not_if` exit-0 → skip; exit !=0 → run.
- `deferred=True` enqueue'ит defer, не выполняет.

**health_check:**
- Url: 200 → ok; 404 → fail; retry до retry_count.
- Cmd: exit 0 → ok; exit !=0 → fail.

### Уровень 2 — Golden

`bosun-primitives/tests/golden/runr_ini/` — рендер `service.j2`/
`timer.j2`/`cgroup.j2` с разными комбинациями полей. Текущий MVP-стиль
golden (`UPDATE_GOLDEN=1` regenerates).

```
golden/runr_ini/
├── basic_service/
│   ├── input.json         # render_service args
│   └── expected.ini       # ожидаемый INI
├── service_with_log/
├── service_with_cgroup_path/
├── timer_on_calendar/
├── timer_on_unit_inactive/
└── cgroup_with_io_max/
```

### Уровень 3 — BDD в Docker

Новые feature-файлы в `tests/bdd/features/`. Каждый — набор Gherkin-сценариев,
конкретная реализация шагов — в Phase K (plan). Минимальное покрытие:

- **`runr_service.feature`** — defer durability (crash mid-apply,
  replay restores reload), cross-resource notify (две file.content на
  один nginx → один defer), validation failure (broken config → no defer,
  `.new` остаётся), health-check failure → bump attempt, после max →
  `.manual_clear`, daemon unavailable → defer ждёт.
- **`systemd_service.feature`** — InvocationID verify (рестарт сменил →
  defer удалён), daemon-reload throttle (3 systemd-services → 1 Reload()
  call).
- **`defers_journal.feature`** — dedup (restart субсумирует reload,
  reverse: вставка restart удаляет reload), priority order (r0 → r1 →
  r2 → c0 в dispatch).
- **`service_unit_abstract.feature`** — диспатч по `init_system`:
  systemd, runr, unknown (последний → exit 3 с понятной диагностикой).
- **`command_run_deferred.feature`** — `deferred=True` enqueue без
  exec, executed в post-replay; `only_if`/`not_if` skip-логика.

### Тест-инфраструктура

Расширение `docker/test-base.Dockerfile`:

- Установка `dbus`, `systemd`, `python3-dbusmock`, `curl`,
  `ca-certificates`.
- Multi-stage сборка `runr` из upstream (`github.com/ozontech/runr`); при
  недоступности репозитория — fallback на pre-built binary, путь к
  которому фиксируется через build-arg. См. `docker/runr-source.md`.

`tests/bdd/steps/`:
- `systemd_mock.rs` — спавн `python3-dbusmock` sidecar с
  manager-template, harness для assertions «вызвано N раз», «вернуть
  фиксированный ответ».
- `runr.rs` — `start_runr_daemon`, `stop_runr_daemon`, `runr_service_state`,
  `runr_service_restarts(name)` helpers через `docker exec`.
- `defers.rs` — assertions для `/tmp/bosun-defers/` (наличие файла,
  содержимое JSON, attempt_count).

## Migration notes

Предыдущая черновая `docs/superpowers/specs/2026-05-19-bosun-runr-integration-design.md`
**полностью superseded** этой версией. Из неё переносится:

- HTTP-клиент структура и API (минимальные изменения).
- INI-генератор переходит в `_lib/runr/main.star` (Starlark функция,
  использует `template()`), вместо отдельного Rust-модуля `runr_ini`.
  Это соответствует bundle-rev-2 правилу: рендер шаблонов — в
  `_lib/<name>/templates/`.
- `runr_dirty` end-of-phase callback заменяется на explicit
  `runr.daemon_reload()` через journal: первый раз когда нужен reload,
  enqueue `r0-runr.daemon_reload:.deferred` (или сразу выполнение, потому
  что daemon-reload недорого); в любом случае throttled через
  `ctx.runr_daemon_reload_done: Cell<bool>`.

Что **не переносится** из черновой:
- `runr_ini` Rust-module — заменено на Starlark `_lib/runr/render_*`.
- Inline-декларация `runr.service(name=..., exec_start=...)` — продолжаем
  держать split (`file.content` + `runr.service` через handle), inline —
  open question.

Bundle-формат — bundle-rev-2 (main.star в корне bundle, `bundle.toml.entry`
default `"main.star"`). Это правка относительно bundle-architecture-design.md
rev 2: финализированное решение — main.star в bundle root, `manifests/`
директории нет.

## Open questions

1. **Inline-декларация `runr.service(name=..., exec_start=..., ...)`.**
   Удобнее для пользователя, ломает разделение «один resource = одна
   ответственность» (юнит-файл + state). MVP остаётся split.
2. **`file.content(state="absent")`.** Нужно для удаления unit-файлов при
   `service.absent`. Open question с MVP, отложено.
3. **Validator argument templating sophistication.** Сейчас `{new_path}`.
   Нужны ли `{path}`, `{owner}`, `{group}`? Решаем по мере получения
   реальных bundle с разными валидаторами.
4. **Health-check retry policy.** Сейчас фиксированная (default 3 попытки,
   2-секундный интервал). Возможно, для critical-services нужен backoff
   (1s, 4s, 16s).
5. **`bosun status --clear-all-manual` для оператора.** Сейчас manual_clear
   убирается per-id; bulk clear полезен для recovery после массового
   incident, но рискован.
6. **Cross-bundle notify.** `service.handle_by_name("nginx")` для случаев,
   когда file.content в одном bundle, а service.unit — в другом. Сейчас
   только in-bundle Handle.
7. **runr-init detection precision.** Сейчас по `/proc/1/comm == "runr"`
   и `/etc/runr/` существует. На systemd-хосте, где runr запущен как
   сервис, оба клиента активны (`mixed-systemd-runr`). Достаточно ли
   гибко?

## Связь с концептами wiki

| Концепт wiki | Как используется |
|---|---|
| `concepts/runr-supervisor.md` | источник правды для runr HTTP API и INI-формата |
| `concepts/bosun-overview.md` | `runr.*`, `systemd.*`, `service.unit` — Phase 11+ примитивы |
| `concepts/chiit-go-dsl.md` | defers семантика (на диск, replay в начале и конце), notify-chain паттерн |
| `concepts/starlark-dsl.md` | новые builtins `runr`, `systemd`, `service`, `command` в @bosun/builtins |
| `concepts/bundle-architecture.md` | `_lib/runr/`, `_lib/systemd/` живут под bundle-rev-2 layout |

## История ревизий

### rev 1 (2026-05-19)

Первая версия. Источник — research-файл
`docs/superpowers/research/2026-05-19-runr-systemd-defers-research.md` и
зафиксированные пользователем решения:

- defers в `/tmp/bosun-defers/`, не `/var/lib/`. Reboot обнуляет.
- main.star в bundle root, без `manifests/` директории.
- Native клиенты в отдельных крейтах: `bosun-systemd-client`,
  `bosun-runr-client`. INI-генератор — в bundle `_lib/runr/`.
- `service.unit` — абстрактная Starlark функция, диспатчится по
  `inv.facts.init_system`. Concrete `runr.*`/`systemd.*` доступны напрямую.
- defers покрывают `service.restart/reload/reload_or_restart` и
  `command.run(deferred=True)`.
- Предыдущая `2026-05-19-bosun-runr-integration-design.md` —
  superseded полностью.
