# bosun — runr + systemd + deferred-restart journal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) или superpowers:executing-plans для пофазного выполнения. Каждая фаза — отдельная единица работы; внутри фазы implementer-агент сам разбивает на TDD-итерации.

**Goal:** Реализовать вторую большую итерацию bosun-client поверх MVP и bundle-rev-2: управление долгоживущими процессами через runr (HTTP API) и systemd (dbus), журнал отложенных действий (defers) с at-least-once и crash-resilience, валидация конфигов перед swap, health-check после рестарта, абстрактный `service.unit`, `command.run` с `deferred=True`.

**Architecture:** Расширение Rust-workspace новыми крейтами `bosun-systemd-client` (через `zbus`/`zbus_systemd`) и `bosun-runr-client` (через `ureq`). Новый модуль `defers` в `bosun-core` пишет journal в `/tmp/bosun-defers/` (tmpfs by design — reboot обнуляет; см. research-файл секция 2.6.1 и зафиксированное пользовательское решение). Примитивы `runr.service/timer/cgroup`, `systemd.service/timer`, абстрактный `service.unit` и `command.run` в `bosun-primitives`. INI-рендер для runr живёт в bundle `_lib/runr/` как Starlark функции, вызывающие `template()`. bosun-cli делает двойной replay (до и после) в `apply`, добавляет `bosun status` subcommand.

**Tech Stack:** Поверх MVP-tech-stack добавляется: `zbus = "5"`, `zbus_systemd = "0.25"`, `ureq = "2"`, `tokio = { version = "1", features = ["rt","macros","sync","time"] }`. Dev: `wiremock`, `python3-dbusmock` в Docker test-base, `tokio-test`.

**Source spec:** `docs/superpowers/specs/2026-05-19-bosun-runr-systemd-defers-design.md`

**Constraints (применяются к каждой фазе):**
- Workspace lints (`deny(unwrap_used, expect_used, panic, unsafe_code)`) — без исключений.
- `#[non_exhaustive]` на все новые public enum.
- `cargo fmt --all` и `cargo clippy --workspace --all-targets -- --deny warnings` зелёные.
- Никаких новых workspace deps без явной необходимости (если deps мог бы заменить уже-installed crate — выбираем installed).
- Никакого `mod tests` без `#[allow(clippy::unwrap_used, clippy::panic)]` на самом модуле — это стандартное послабление для тестов в workspace.
- Все Russian-language комментарии и doc'и (project-wide convention из MVP).
- На каждую non-trivial pure function — unit-тесты (project-wide convention, см. global review rule).

---

## File Structure (новое + изменения)

Новые крейты:

- `crates/bosun-systemd-client/` — `manager.rs` (async `SystemdManager`),
  `facade.rs` (sync `BlockingSystemdManager` через current-thread tokio),
  `types.rs` (`UnitInfo`/`JobResult`/`ServiceState`), `job_watch.rs`
  (JobRemoved subscription + match по object path), `error.rs`
  (`SystemdError`).
- `crates/bosun-runr-client/` — `client.rs` (sync `Client` через ureq),
  `types.rs` (`ServiceStatus`/`TimerStatus`/`UnitListItem`/`ActionAck`/
  `DaemonInfo`), `verify.rs` (poll-based Restarts/StartedAt diff),
  `error.rs` (`RunrError`).

Модификации в `crates/bosun-core/src/`:

- Новый модуль `defers/` — `format.rs` (`DeferEntry` + serde),
  `journal.rs` (Journal API + atomic ops), `replay.rs` (replay loop +
  dispatch), `action.rs` (`DeferAction` enum), `priority.rs` (sortkey +
  dedup rules).
- `resource.rs` — добавляется `Resource.restart_on: Vec<ResourceId>`.
- `primitive.rs` — добавляются `PrimitiveError::RunrUnavailable /
  SystemdUnavailable / Validation / HealthCheckFailed / DeferIo`,
  `is_deferrable()` расширяется.
- `orchestrator.rs` — `ApplyCtx.changed_resources / defers / runr /
  systemd`, `ctx.is_changed/record_changed`.
- `registry.rs` — `topological_order` учитывает `restart_on`.

Модификации в `crates/bosun-facts/src/init_system.rs` — распознавание
`runr` PID 1 и комбинированного `mixed-systemd-runr`.

Новые подмодули в `crates/bosun-primitives/src/` — `runr_service/`,
`runr_timer/`, `runr_cgroup/`, `systemd_service/`, `systemd_timer/`,
`service_unit/` (dispatcher), `command_run/`, `health_check/`. Каждый
service-primitive подмодуль содержит `mod.rs` + `spec.rs` + `plan.rs` +
`apply.rs`. Существующий `file_content/` расширяется (`validate_with` в
spec.rs, render-to-.new + validator + rename в apply.rs).

Модификации в `crates/bosun-cli/src/` — `args.rs` (новые флаги),
`run.rs` (инициализация клиентов, двойной replay), `status.rs` (NEW —
`bosun status` subcommand), `metric.rs` (новые метрики).

Новые BDD feature-файлы в `crates/bosun-cli/tests/features/` —
`runr_service.feature`, `systemd_service.feature`, `defers_journal.feature`,
`service_unit_abstract.feature`, `command_run_deferred.feature`,
`validate_with.feature`. Новые step-модули — `steps/runr.rs`,
`steps/systemd_mock.rs`, `steps/defers.rs`.

Модификация `docker/test-base.Dockerfile` — добавление dbus, systemd,
python3-dbusmock, runr build. Новый `docker/runr-source.md` —
документация про gated upstream и fallback.

Новый `examples/pgbouncer-cluster/bundle/` — service.unit +
validate_with + cross-resource notify (см. Phase L).

---

## Phase A: bosun-systemd-client крейт

**Цель:** Создать изолированный async-клиент к systemd1 dbus через
zbus_systemd с sync-фасадом для синхронных примитивов.

**Acceptance criteria:**

- [ ] Новый крейт `crates/bosun-systemd-client/` собирается и проходит
  `cargo test -p bosun-systemd-client`.
- [ ] `SystemdManager::connect_system()` открывает system bus и
  оборачивает `zbus_systemd::systemd1::ManagerProxy`. Ошибка отсутствия
  bus-сокета → `SystemdError::BusUnavailable` с понятным сообщением.
- [ ] Реализованы методы `daemon_reload`, `needs_daemon_reload`,
  `start_unit`, `stop_unit`, `restart_unit`, `reload_unit`,
  `reload_or_restart_unit`, `enable_unit`, `disable_unit`, `unit_info`,
  `wait_for_job` — все async, возвращают `Result<_, SystemdError>`.
- [ ] `JobHandle` — newtype над `zbus::zvariant::OwnedObjectPath`,
  derived `Debug`, `Clone`.
- [ ] `wait_for_job` подписывается на `Manager.JobRemoved` и матчит по
  job object path; таймаут через `tokio::time::timeout` возвращает
  `SystemdError::Timeout`. Учтена баг Debian 996911: после
  `result=done` дополнительно фетчится `ActiveState`; если `failed` —
  `SystemdError::JobFailed`.
- [ ] `unit_info` возвращает `UnitInfo { name, active_state, sub_state,
  invocation_id: String /* hex of [u8;16] */, exec_main_start_timestamp:
  Option<u64> }`.
- [ ] `BlockingSystemdManager` (модуль `facade.rs`) поднимает
  `tokio::runtime::Builder::new_current_thread().enable_all().build()` и
  блокирующе вызывает async-методы. Конструктор `connect_system()`
  возвращает готовый `BlockingSystemdManager`, не панкует на ошибку.
- [ ] `SystemdError` enum — `#[non_exhaustive]`, варианты как в design:
  `BusUnavailable`, `Dbus`, `NoSuchUnit`, `AuthorizationDenied`,
  `JobFailed`, `RestartNotObserved`, `Timeout`, `Io`.
- [ ] Unit-тесты (`cargo test -p bosun-systemd-client`):
  - mock-test через `tokio-test`: симулировать `JobRemoved` через mock
    stream, проверить корректный match.
  - `BusUnavailable` при отсутствии сокета: переопределяем env
    `DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent`, ожидаем правильную
    ошибку.
  - `InvocationID` парсинг: 16 байтов из `Vec<u8>` → `String` hex.
- [ ] Integration-тесты с реальным systemd — в Phase K (BDD).

**Что НЕ в scope этой фазы:**
- Polkit-friendly non-root deployment.
- Shell-fallback (`systemctl restart x` через Command::new) — мы не
  поддерживаем этот путь.
- `EnableUnitFiles` с custom targets/wants.

**Hard constraints:**
- Все async-методы возвращают `Result<_, SystemdError>`, **не `anyhow::Error`**.
- `SystemdError` имплементит `std::error::Error` + `Debug` + `Display` через
  `thiserror`.
- `tokio` берётся с минимальными features: `rt`, `macros`, `sync`, `time`.
  Никаких `rt-multi-thread`, никаких `net` features.
- `zbus` — `default-features = false` с `["tokio"]`. Никаких
  `async-std`-зависимостей.
- Тесты не должны требовать живого systemd; всё мокается.

**Зависимости (workspace.dependencies):**
- `zbus = "5"`, default-features = false, features = `["tokio"]`
- `zbus_systemd = "0.25"`
- `tokio = "1"`, features = `["rt","macros","sync","time"]`

---

## Phase B: bosun-runr-client крейт

**Цель:** Sync HTTP-клиент через ureq, замещает черновую секцию из
предыдущего `runr-integration-design.md`.

**Acceptance criteria:**

- [ ] Новый крейт `crates/bosun-runr-client/` собирается и проходит
  `cargo test -p bosun-runr-client`.
- [ ] `Client::new(base_url, timeout)` создаёт ureq Agent с указанным
  таймаутом read+write.
- [ ] Реализованы методы из chiit-API (см. research секция 4):
  - `daemon_info`, `daemon_reload`
  - `service_start(name, idempotent)`, `service_stop(name, force,
    timeout_humantime)`, `service_restart(name)`, `service_reload(name)`
  - `timer_start`, `timer_stop`, `timer_enable(now)`, `timer_disable(now)`
  - `service_statuses`, `timer_statuses`, `units_list`
- [ ] Все request bodies сериализуются через serde_json; response bodies
  десериализуются с `#[serde(deny_unknown_fields)]` (для catch регрессий
  схемы).
- [ ] Типы: `ServiceStatus { name, state, pid: Option<u32>, restarts:
  u64, in_state_for_ms: u64, uptime_ms: Option<u64>, downtime_ms:
  Option<u64>, started_at: Option<String>, autostart: bool,
  memory_rss_anon_bytes: u64, memory_rss_file_bytes: u64,
  cpu_usage_percent: f64 }` — поля как в client.go,
  research секция 4.
- [ ] `verify.rs::verify_restart(client, name, before, poll_interval,
  poll_total)` — polling до тех пор пока `restarts > before.restarts`
  AND `state == "Running"`; таймаут → `RunrError::RestartNotObserved`.
- [ ] `RunrError` enum — `#[non_exhaustive]`: `Unavailable`,
  `ApiError`, `BadResponse`, `NotFound`, `RestartNotObserved`.
- [ ] Unit-тесты через `wiremock`:
  - 200 OK для каждого endpoint.
  - 404 для несуществующего unit → `NotFound`.
  - 500 для surprise error → `ApiError`.
  - Connection refused (mock не запущен) → `Unavailable`.
  - `verify_restart`: pre `restarts=3` → post `restarts=4` ✓; pre==post
    → `RestartNotObserved`.
- [ ] Round-trip JSON-сериализация: построить `ServiceStatus`
  программно, serialize → deserialize → equal.

**Что НЕ в scope этой фазы:**
- Auto-retry на 5xx (это решение orchestrator-уровня).
- Auth headers (runr не требует).
- Multi-host клиент (только `127.0.0.1`).

**Hard constraints:**
- `ureq = "2"` (sync, без runtime).
- Никакого `reqwest`, `hyper`, `tokio` зависимостей в этом крейте.
- Все public API возвращают `Result<_, RunrError>`.

---

## Phase C: bosun-core defers модуль

**Цель:** Реализовать journal в `/tmp/bosun-defers/` с атомарной записью,
дедупом, replay и priority rules.

**Acceptance criteria:**

- [ ] Новый модуль `crates/bosun-core/src/defers/` с подмодулями
  `format.rs`, `journal.rs`, `replay.rs`, `action.rs`, `priority.rs`.
- [ ] `DeferEntry` struct с полями из design-секции «Формат файла»:
  `spec_version: u16`, `id: String`, `action: DeferAction`,
  `init_system: String`, `target: String`,
  `validate_cmd: Option<Vec<String>>`, `health_check: Option<HealthCheck>`,
  `priority: DeferPriority`, `enqueued_at: chrono::DateTime<Utc>`,
  `enqueued_by: Vec<String>`, `attempt_count: u32`, `max_attempts: u32`.
- [ ] `DeferAction` enum, `#[non_exhaustive]`: `Start`, `Stop`,
  `Restart`, `Reload`, `ReloadOrRestart`, `Command { argv: Vec<String> }`,
  `DaemonReload`.
- [ ] `DeferPriority` enum, `#[non_exhaustive]`: `Restart` (sortkey `r0`),
  `ReloadOrRestart` (`r1`), `Reload` (`r2`), `Command` (`c0`),
  `DaemonReload` (`d0`).
- [ ] `Journal::open(path)` создаёт директорию если её нет, ставит
  permissions `0o700`, owner root (если запущено root). Возвращает
  `Journal { root: PathBuf }`.
- [ ] `Journal::enqueue(entry)`:
  - Применяет dedup rules (см. design «Дедуп правила»):
    - вставка `Reload` если есть `Restart` для того же target → no-op
      (возвращает `EnqueueResult::Subsumed`).
    - вставка `Restart` если есть `Reload` → удаляем reload-файл (с
      `fsync(dir)`), пишем restart.
    - идемпотентный insert того же `(action, target)` → no-op (но JSON
      content-stable: те же `enqueued_by` сортируются, `enqueued_at`
      сохраняется — это означает, что повторная вставка возвращает
      `EnqueueResult::AlreadyExists` без перезаписи).
  - Атомарная запись: `tmp.<nanos>` → `write_all` → `sync_all` → close →
    `rename` → `fsync(dir)`.
- [ ] `Journal::list_sorted()` — `read_dir`, фильтрует `*.deferred`
  (исключает `*.tmp.*`, `*.manual_clear`), sort by filename (lex), parse
  JSON, возвращает `Vec<DeferEntry>`. Поврежденные JSON — `tracing::warn`
  + skip + не abort.
- [ ] `Journal::remove(entry)` — `unlink` + `fsync(dir)`.
- [ ] `Journal::bump_attempt(entry, err)` — rewrite файла с
  `attempt_count += 1`. Same atomicity.
- [ ] `Journal::move_to_manual_clear(entry)` — `rename` из `*.deferred`
  в `*.manual_clear` + `fsync(dir)`.
- [ ] `priority.rs::sortkey(action) -> &'static str` — функция
  возвращающая `r0`/`r1`/`r2`/`c0`/`d0`.
- [ ] `replay.rs::replay(journal, ctx) -> ReplayReport`:
  - `ReplayReport { executed, skipped_unavailable, failed,
    promoted_to_manual_clear }`.
  - Цикл по `list_sorted`, dispatch через
    `action.rs::dispatch(entry, ctx)`. ClientUnavailable → skip,
    Action::Err → bump_attempt, success → remove.
  - На каждый entry — `tracing::info_span!("defer", id =
    %entry.id).entered()`.
- [ ] `action.rs::dispatch(entry, ctx)` — match по
  `entry.action × entry.init_system`, вызывает соответствующий клиент
  (runr или systemd) из `ctx.runr` / `ctx.systemd`.
- [ ] Unit-тесты (`cargo test -p bosun-core defers::`):
  - Atomicity: запись прерывается между `rename` и `fsync(dir)` → файла
    нет в `list_sorted` (имитируем через сторонний `unlink` сразу после
    rename — fsync пропускается).
  - Atomicity success path: `Journal::enqueue` создаёт ровно один файл,
    не оставляет `*.tmp.*`.
  - Dedup: enqueue reload → enqueue restart → один файл с `r0-`-префиксом.
  - Dedup инверс: enqueue restart → enqueue reload → один файл с
    `r0-`-префиксом (reload no-op).
  - Lex order: создать `r0-x.deferred`, `r1-y.deferred`, `r2-z.deferred`
    → `list_sorted` возвращает `[r0, r1, r2]`.
  - Replay success: dispatch возвращает `Ok` → файл удалён.
  - Replay failure → `bump_attempt` инкрементирует, при `attempt_count
    >= max_attempts` промоутит в `.manual_clear`.
  - Replay client_unavailable → файл остаётся, attempt не bump'ится.
  - Поврежденный JSON в директории → warn + skip + остальные обработаны.
  - Idempotent re-enqueue: enqueue → enqueue same `(action, target)` →
    один файл; контент не меняется (`AlreadyExists`).

**Что НЕ в scope этой фазы:**
- Реальный вызов systemd/runr клиента — мокируется в тестах через trait
  `DispatchClient`.
- 7-дневный timeout pruning — добавляется в Phase J.
- CLI команды для просмотра — в Phase J.

**Hard constraints:**
- `fsync(dir)` обязательно после `rename` / `unlink`. Использовать
  `nix::fcntl::fsync` или `std::fs::File::open(&dir).and_then(|f|
  f.sync_all())`.
- Никакого `panic!`/`unwrap`/`expect` в production-пути.
- JSON через `serde_json::to_writer` + `BufWriter`; никаких `format!`
  с ручным экранированием.

---

## Phase D: bosun-primitives `runr.service`/`runr.timer`/`runr.cgroup`

**Цель:** Native примитивы runr на основе bosun-runr-client (Phase B) и
defers модуля (Phase C).

**Acceptance criteria:**

- [ ] Модули `runr_service/`, `runr_timer/`, `runr_cgroup/` в
  `bosun-primitives/src/`. Каждый содержит `mod.rs` с
  `impl Primitive`, `spec.rs`, `plan.rs`, `apply.rs`.
- [ ] `RunrServiceSpec` (`#[derive(serde::Deserialize)]`): `name: String`,
  `state: ServiceState` (`Running`/`Stopped`/`Absent`), `enable: bool` (default false),
  `health_check: Option<HealthCheck>`, `validate_with: Option<Vec<String>>`.
- [ ] `RunrServicePrimitive::type_name()` → `runr.service`.
  `identity_keys()` → `&["name"]`.
- [ ] `build_payload` через `CallArgs`:
  - извлекает `name`, `state`, `enable`.
  - извлекает `health_check_cmd: Option<Vec<String>>` /
    `health_check_url: Option<String>` и объединяет в `HealthCheck`
    enum.
  - извлекает `validate_with: Option<Vec<String>>`.
  - `reload_on`, `restart_on`, `depends_on` обрабатываются Starlark-glue
    (см. Phase J).
- [ ] `plan` через `runr.service_statuses`:
  - Кешируется в `ApplyCtx.runr_service_statuses: OnceCell<Vec<ServiceStatus>>`
    (один HTTP-запрос на весь apply).
  - `decide_action_runr(spec, before, ctx)` возвращает `Action::Start /
    Stop / Restart / Reload / NoChange` по matrix из design-секции
    «Decide action».
- [ ] `apply`:
  - Если `ApplyCtx.runr` is None → `PrimitiveError::RunrUnavailable
    { base_url: "n/a", reason: "runr client not initialized" }`.
  - daemon_reload throttle: `ctx.runr_daemon_reload_done.get()` → если
    false, проверка нужен ли (через `units_list` + сравнение mtime?
    проще — всегда вызвать однажды), вызвать `runr.daemon_reload()`,
    `ctx.runr_daemon_reload_done.set(true)`.
  - `decide_action_runr` → если action == NoChange → возвращаем
    `ChangeReport::no_change`.
  - Если action — `Start` / `Stop` (state-change, не notify-driven) →
    выполнить синхронно через runr-client, верифицировать.
  - Если action — `Restart` / `Reload` (notify-driven) → enqueue defer
    через `ctx.defers.enqueue(DeferEntry::from_runr(...))`,
    `ChangeReport::deferred(...)`.
  - На `RunrError::Unavailable` → `PrimitiveError::RunrUnavailable`
    (is_deferrable=true → `Outcome::Deferred`).
  - На `RunrError::ApiError` / `BadResponse` / `NotFound` →
    `PrimitiveError::Apply { ... }`.
- [ ] `RunrTimerSpec`: `name`, `state: TimerState (Enabled/Disabled/Absent)`,
  `start_now: bool` (default false).
- [ ] `runr_timer::apply`:
  - Enabled & not enabled → `timer_enable(start_now)`.
  - Disabled & enabled → `timer_disable(false)`.
  - Absent & ... → `timer_stop` + `timer_disable(true)`.
- [ ] `RunrCgroupSpec`: `name`, `state: CgroupState (Present/Absent)`.
  Apply ничего не делает кроме декларации присутствия (реальная работа —
  в `file.content` пишущем `/etc/runr/X.cgroup` + global daemon_reload).
- [ ] Unit-тесты:
  - `decide_action_runr` matrix (15+ комбинаций).
  - `apply` mock-client (через mock `RunrHandle` trait): при
    `Action::Restart` enqueue defer, real `service_restart` НЕ вызван.
  - `apply` Start/Stop synchronous → `verify_restart` вызван для
    Restart, для Start — `service_start(idempotent=true)`.
  - `apply` ctx.runr=None → `RunrUnavailable`.
  - `apply` notify-trigger из `restart_on` → enqueue
    `r0-runr.restart:<name>.deferred`.

**Что НЕ в scope этой фазы:**
- INI-генерация для unit-файлов (она живёт в `_lib/runr/` Starlark, см.
  Phase L).
- Systemd-сторона (Phase E).
- Health-check (Phase I — он подключается тут позже).
- validate_with пока не на service.unit; только на `file.content` (Phase H).

**Hard constraints:**
- Никаких `unwrap` на `runr_service_statuses.get_or_try_init(...)`.
- defer enqueue идёт ДО реального вызова runr, чтобы при crash между
  enqueue и dispatch мы могли догнаться. Однако для синхронных
  Start/Stop — порядок наоборот (defer не нужен; синхронно дёргаем).

---

## Phase E: bosun-primitives `systemd.service` / `systemd.timer`

**Цель:** Native примитивы systemd через `bosun-systemd-client` (Phase A).

**Acceptance criteria:**

- [ ] Модули `systemd_service/`, `systemd_timer/` в `bosun-primitives/src/`.
- [ ] `SystemdServiceSpec`: те же поля, что `RunrServiceSpec`. Поле `enable`
  по умолчанию `true` (systemd-units стандартно ENABLE'ятся при
  manage'е); `false` отключает `EnableUnitFiles` шаг.
- [ ] `apply` flow по design-секции «Flow для `systemd.service` apply»:
  1. `daemon_reload` throttle через
     `ctx.systemd_daemon_reload_done: Cell<bool>` + проверка
     `needs_daemon_reload`.
  2. Snapshot `unit_info` → `before`.
  3. Если `spec.enable && !before.is_enabled` → `enable_unit_blocking`.
  4. `decide_action` (та же matrix, что для runr, но against
     `ActiveState`).
  5. defer-eligible actions → enqueue. State-change actions →
     synchronous + verify.
  6. `InvocationID` diff verification: если `before.invocation_id ==
     after.invocation_id` → `PrimitiveError::SystemdUnavailable {
     reason: "restart not observed (InvocationID unchanged)" }`.
- [ ] Health-check после успешного restart/reload, если задан.
- [ ] `SystemdTimerSpec`: `name`, `state: TimerState`, `enable: bool`.
  Apply через `start_unit`/`stop_unit` для timer-units (systemd timer
  = unit type, обычные методы работают).
- [ ] Маппинг `SystemdError` → `PrimitiveError`:
  - `BusUnavailable` → `SystemdUnavailable { reason: ... }` (deferrable).
  - `NoSuchUnit` → `Apply { ... }` (non-deferrable).
  - `AuthorizationDenied` → `Apply { ... }` (документация: запускать от
    root).
  - `JobFailed`, `RestartNotObserved` → `Apply { ... }`.
- [ ] Unit-тесты (mock через `SystemdHandle` trait):
  - decide_action matrix.
  - daemon_reload throttle: 3 systemd-services в одном apply → ровно 1
    вызов `daemon_reload`.
  - notify-trigger → enqueue defer, реальный restart НЕ вызван.
  - `InvocationID` same before/after → `RestartNotObserved` ошибка.
  - `enable=true && !is_enabled` → `enable_unit` вызвано.

**Что НЕ в scope:**
- Polkit configuration.
- Custom WantedBy/Aliases в EnableUnitFiles.
- Live unit-status streaming.

**Hard constraints:**
- `BlockingSystemdManager` создаётся **один раз** в `bosun-cli/src/run.rs`,
  передаётся как `Rc<...>` в `ApplyCtx.systemd`. Каждый primitive
  использует один и тот же handle.
- Никаких `tokio::main` или вложенных runtime — primitive не имеет права
  создавать runtime внутри.

---

## Phase F: bosun-primitives abstract `service.unit`

**Цель:** Starlark-функция `service.unit(...)` диспатчится в
`runr.service(...)` или `systemd.service(...)` по `inv.facts.init_system`.

**Acceptance criteria:**

- [ ] Модуль `bosun-primitives/src/service_unit/` с `mod.rs` и `dispatch.rs`.
- [ ] Не отдельный primitive (не impl Primitive trait), а
  Starlark-functino, регистрируемая в `@bosun/builtins` globals.
- [ ] Подмножество параметров (только то, что общее для runr и systemd):
  - `name: String` (required)
  - `state: String` (required, `"running"`/`"stopped"`/`"absent"`)
  - `enable: bool` (default false)
  - `health_check_cmd: Option<Vec<String>>`
  - `health_check_url: Option<String>`
  - `health_check_expected_status: Option<u16>`
  - `health_check_retry: Option<u32>`
  - `health_check_interval: Option<String>` (humantime)
  - `validate_with: Option<Vec<String>>`
  - `reload_on: Option<Vec<Handle>>`
  - `restart_on: Option<Vec<Handle>>`
  - `depends_on: Option<Vec<Handle>>`
- [ ] Dispatch:
  ```rust
  let init = state.facts.get("init_system")?;
  match init.as_deref() {
      Some("systemd") | Some("mixed-systemd-runr") => systemd_service(args, eval),
      Some("runr") => runr_service(args, eval),
      Some(other) => fail("service.unit: unsupported init_system {other:?}"),
      None => fail("service.unit: init_system fact unknown"),
  }
  ```
- [ ] Если в Starlark передан init-specific параметр (например
  `cgroup_procs_path` — runr-only, или `condition_path_exists` —
  systemd-only) — `service.unit` НЕ принимает его, явная ошибка
  `unexpected keyword argument`. Power-user должен идти на `runr.service`
  или `systemd.service`.
- [ ] Unit-тесты (Starlark eval из `bosun-core/tests/`):
  - `service.unit(name="x", state="running")` с `init_system=systemd` →
    Registry содержит `Resource { kind: "systemd.service", ... }`.
  - С `init_system=runr` → Registry содержит `runr.service`.
  - С `init_system=mixed-systemd-runr` → primary systemd.
  - С `init_system=unknown` → fail с понятным сообщением.

**Что НЕ в scope:**
- `service.timer` абстрактный — на следующую итерацию (по запросу).
- Auto-emit notify в `bundle.toml.tags` метаданные.

**Hard constraints:**
- `service.unit` — Rust функция в Starlark globals; никакого Starlark
  wrapper-кода для неё (это упрощает stack walking в `template()`).
- Должна работать в обоих контекстах (role и lib).

---

## Phase G: bosun-primitives `command.run`

**Цель:** Аналог chiit's `command.New().Execute()` плюс
`defers.AddCommand` — синхронный run с `only_if/not_if` и опциональный
`deferred=True`.

**Acceptance criteria:**

- [ ] Модуль `bosun-primitives/src/command_run/` с `mod.rs`, `spec.rs`,
  `plan.rs`, `apply.rs`.
- [ ] `CommandRunSpec`:
  - `name: String` (required, unique для дедупа в defers)
  - `cmd: Vec<String>` (argv, не shell)
  - `deferred: bool` (default false)
  - `only_if: Option<Vec<String>>` (argv)
  - `not_if: Option<Vec<String>>` (argv)
  - `timeout_sec: Option<u32>` (default 60)
  - `working_directory: Option<String>`
  - `environment: Option<BTreeMap<String, String>>`
- [ ] `plan`:
  - Если `only_if` задан и `run_predicate(only_if) == false` → `NoChange`
    (skipped).
  - Если `not_if` задан и `run_predicate(not_if) == true` → `NoChange`.
  - Иначе → `Update`.
- [ ] `run_predicate(argv)`:
  - spawn argv через `std::process::Command`.
  - wait с таймаутом (default 10s).
  - exit 0 → `true`; иначе → `false`.
  - On exec error → `false` (предикат считается отрицательным; alternative
    `Err` сделает primitive не-идемпотентным).
- [ ] `apply`:
  - Re-eval предикаты (на случай если состояние изменилось между plan и
    apply).
  - Если `spec.deferred == false` → выполнить argv через
    `std::process::Command`, capture stdout/stderr (max 4KiB excerpt),
    проверить exit code. Success → `ChangeReport::changed(format!("ran
    {name}"))`. Failure → `PrimitiveError::Apply { stderr_excerpt }`.
  - Если `spec.deferred == true` → enqueue
    `DeferEntry { action: Command { argv: spec.cmd }, id: "command.run:<name>", ... }`
    через `ctx.defers.enqueue`. ChangeReport::deferred.
- [ ] Unit-тесты:
  - `only_if=[true]` → run; `only_if=[false]` → skip.
  - `not_if=[true]` → skip; `not_if=[false]` → run.
  - exec timeout → `PrimitiveError::Apply { reason: "timeout" }`.
  - `deferred=true` → enqueue (через mock journal), реальный exec НЕ
    вызван.
  - stdout/stderr capturing: spawn `sh -c 'echo a >&2; exit 0'` → success
    + stderr capture в логе.

**Что НЕ в scope:**
- Shell-через-стрку (`cmd: "sysctl -p"` без массива) — намеренно
  заставляем массив для безопасности от инъекций.
- Restart-on-change для command.run (notify-цели для command.run сейчас
  только через `reload_on`/`restart_on` в Resource).

**Hard constraints:**
- `Command::new` принимает только argv-массив. Никаких `sh -c`.
- Все exec'и через child + timeout-wait (`std::process::Child::try_wait`
  + sleep loop, или `wait-timeout` крейт — но мы избегаем deps, поэтому
  custom loop).

---

## Phase H: validate_with на `file.content` / `file.template` / `service.unit`

**Цель:** Расширить существующий `file.content` (и
`file.template` если есть отдельно) параметром `validate_with` по design
секции «Validation pattern». Реализовать render-to-.new → validator →
rename pattern.

**Acceptance criteria:**

- [ ] `FileContentSpec` расширяется полем
  `validate_with: Option<Vec<String>>`.
- [ ] `apply` flow:
  1. Сравнить sha256 nового contents с текущим target. Если совпадает →
     `NoChange`.
  2. Иначе:
     - Создать `<path>.new` с правильными permissions/owner/group.
     - Записать новый contents в `<path>.new` через tempfile + rename
       (атомарно).
     - Если `validate_with` задан → substitute `{new_path}` →
       `run_command(substituted, timeout=30s)`.
       - Exit 0 → продолжить.
       - Exit !=0 → `PrimitiveError::Validation { validator,
         stderr_excerpt }`. **`<path>.new` остаётся на диске.**
     - `rename(<path>.new, <path>)` — атомарно.
     - `ctx.record_changed(&resource.id)`.
     - `ChangeReport::changed(...)`.
- [ ] Если `validate_with` не задан → backward-compatible путь (`<path>.tmp`
  → rename), как в MVP.
- [ ] Substitution: только `{new_path}` плейсхолдер. Любая другая `{...}`
  оставляется as-is.
- [ ] Тесты:
  - Validator success: создать `validate_with = ["sh", "-c", "test -f
    {new_path}"]` → success.
  - Validator failure: `validate_with = ["false"]` → `Validation` ошибка,
    `<path>.new` существует, `<path>` не изменён.
  - No validate_with → старое поведение (regression-test).
  - Substitution: `validate_with = ["echo", "{new_path}"]` → exec получает
    реальный path.
- [ ] `service.unit` тоже принимает `validate_with` — но запускается
  **перед** dispatching restart/reload defer. Если validation fail —
  defer не enqueue, `PrimitiveError::Validation` propagate. Логика
  отличается от file.content тем, что нет файла .new — валидатор
  работает с текущим target.
- [ ] Тесты для service.unit validate_with:
  - `nginx -t` после изменения config → success path: defer enqueue.
  - `nginx -t` failing → no defer, ошибка.

**Что НЕ в scope:**
- Multiple validators (chain).
- Conditional validators per-state.
- `{path}`, `{owner}`, `{group}` плейсхолдеры (Open question).

**Hard constraints:**
- `<path>.new` **никогда** не удаляется автоматически при failure —
  оператор смотрит в forensics.
- Permission на `<path>.new` ставится сразу через `O_CREAT` + chown — не
  `0644 → chmod`.

---

## Phase I: `health_check` на `service.unit` / `runr.service` / `systemd.service`

**Цель:** Post-restart probe (cmd или url), retry policy с promotion в
`manual_clear` при превышении.

**Acceptance criteria:**

- [ ] Модуль `bosun-primitives/src/health_check/` с `mod.rs`, `cmd.rs`,
  `url.rs`.
- [ ] `HealthCheck` enum, `#[non_exhaustive]`:
  - `Cmd { cmd: Vec<String>, timeout_sec: Option<u32>, retry_count:
    Option<u32>, retry_interval_sec: Option<u32> }`.
  - `Url { url: String, expected_status: Option<u16>, timeout_sec:
    Option<u32>, retry_count: Option<u32>, retry_interval_sec:
    Option<u32> }`.
- [ ] `run_health_check(check, ctx) -> Result<(), HealthCheckError>`:
  - Defaults: `retry_count = 3`, `retry_interval_sec = 2`, `timeout_sec = 10`.
  - Cmd: spawn, exit 0 → success.
  - Url: GET через `ureq::Agent`, проверка status code (default 200) →
    success. На любой transport error / status mismatch — retry.
  - Между попытками — sleep `retry_interval_sec` с проверкой
    `ctx.cancel`.
- [ ] Интеграция в `runr_service::apply` / `systemd_service::apply`:
  - После успешного синхронного start/stop/restart/reload — запустить
    health_check.
  - При defer-flow health_check **не запускается синхронно** — он
    запускается в defer-replay (см. ниже).
- [ ] Интеграция в `defers::replay::dispatch`:
  - После выполнения action (restart/reload/start/...) — если
    entry.health_check задан → run_health_check.
  - Если health_check failed → defer считается failed → `bump_attempt`.
  - При превышении max_attempts → `move_to_manual_clear`.
- [ ] Логирование: `tracing::info_span!("health_check", target = %target,
  kind = %"cmd"|"url")` + per-retry `tracing::warn!(attempt = N, retry =
  Y, "health_check failed, retrying")`.
- [ ] Тесты:
  - Url: 200 → success.
  - Url: 500 → fail после retry_count попыток.
  - Url: 200 после 2 неудачных retries → success.
  - Cmd: exit 0 → success.
  - Cmd: exit !=0 → fail.
  - Timeout: hang command → fail after timeout_sec.

**Что НЕ в scope:**
- Exponential backoff.
- TLS/auth для url-check.
- WebSocket / gRPC checks.
- Кастомная проверка тела response.

**Hard constraints:**
- Использовать тот же `ureq::Agent`, что и `bosun-runr-client`, для
  consistency (импортировать из bosun-runr-client? или создать
  отдельно — решение: отдельный Agent, чтобы не зависеть от runr-client
  если init=systemd).

---

## Phase J: bosun-cli — defers replay before/after, `bosun status`, metrics

**Цель:** Подключить всё новое в CLI. Двойной replay в `bosun apply`,
новая subcommand `bosun status`, расширение метрик.

**Acceptance criteria:**

- [ ] В `bosun-cli/src/args.rs`:
  - `ApplyArgs::runr_url: String` (default `http://127.0.0.1:8010`).
  - `ApplyArgs::runr_timeout_sec: u32` (default 10).
  - `ApplyArgs::defers_dir: PathBuf` (default `/tmp/bosun-defers`).
  - `ApplyArgs::defer_max_attempts: u32` (default 3).
  - Новая subcommand `StatusArgs { defers_dir, format: "text"|"json",
    clear: Option<String>, clear_all_manual: bool }`.
- [ ] В `bosun-cli/src/run.rs::apply`:
  - Инициализация systemd/runr клиентов по `inv.facts.init_system`:
    - `systemd` или `mixed-systemd-runr` → `BlockingSystemdManager::connect_system()?`.
    - `runr` или `mixed-systemd-runr` → `runr_client::Client::new(...)`.
    - `unknown` → оба None.
  - Открыть `Journal::open(args.defers_dir)?`.
  - **Pre-replay**: `defers::replay(&journal, &ctx)` до evaluate
    manifest. Метрика `bosun_defers_replay_total{phase="pre"} += 1`.
  - Запуск `evaluate_manifest` → Registry.
  - `Orchestrator::apply(&registry, &ctx)`.
  - **Post-replay**: `defers::replay(&journal, &ctx)` после apply.
    Метрика `bosun_defers_replay_total{phase="post"} += 1`.
  - Метрика `bosun_defers_pending` после обоих replay — кол-во `.deferred`
    файлов.
  - `bosun_defers_executed_total{result="ok"|"failed"|"client_unavailable"|"manual_clear"}`
    инкрементируется внутри replay-цикла через `ReplayReport`.
- [ ] Новый файл `bosun-cli/src/status.rs`:
  - `cmd_status(args) -> Result<i32, Error>`.
  - Открывает journal, `list_sorted()`, дополнительно сканит
    `.manual_clear` файлы.
  - `--format=text` → tabular print (заголовки: ID, STATE, ACTION,
    TARGET, ATTEMPTS, ENQUEUED_AT).
  - `--format=json` → JSON-array всех entries (с дополнительным полем
    `state: "pending"|"manual_clear"`).
  - `--clear=<id>` → `unlink` файла + `fsync(dir)` + log warning.
  - `--clear-all-manual` → удалить все `*.manual_clear` файлы.
  - Exit code: 0 если pending defers ≥0 и нет `.manual_clear`; 1 если
    есть `.manual_clear`; 4 на IO error.
- [ ] Расширение `metric.rs`:
  - `record_defers_state(journal: &Journal, prom: &mut PromWriter)` —
    записывает `bosun_defers_pending`, `bosun_defers_executed_total`,
    `bosun_defers_replay_total`.
  - Также `bosun_runr_reachable {0,1}` и `bosun_systemd_reachable {0,1}`
    проставляются при инициализации клиентов.
- [ ] Расширение `args.rs::Subcommand`:
  ```rust
  enum Subcommand {
      Apply(ApplyArgs),
      BundleValidate(BundleValidateArgs),
      Status(StatusArgs),                  // NEW
      Version,
  }
  ```
- [ ] Тесты (integration через `bosun-cli/tests/integration.rs`):
  - `bosun status` с пустым defer-dir → exit 0, печатает "no pending defers".
  - `bosun status` с 2 pending → table с 2 строками.
  - `bosun status --clear=<id>` → файл исчезает, exit 0.
  - `bosun apply` runs pre-replay → выполнен defer от предыдущего run.

**Что НЕ в scope:**
- `bosun apply --skip-defers` флаг (можно добавить позже для отладки).
- `bosun status --watch` (live streaming).

**Hard constraints:**
- Журнал открывается ровно один раз на жизнь процесса; передаётся через
  Rc.
- Если `defers_dir` недоступен (permission denied) → `Error::DeferIo`,
  exit 4.

---

## Phase K: BDD-сценарии

**Цель:** Покрыть docker-сценариями: defer durability, dedup,
validation, health-check, abstract dispatch, cross-resource notify.

**Acceptance criteria:**

- [ ] Расширен `docker/test-base.Dockerfile`:
  - Установлены `dbus`, `systemd`, `python3-dbusmock`, `curl`.
  - Скачан / собран `/usr/bin/runr` (см. design-секция «Тест-инфраструктура:
    runr daemon» и `docker/runr-source.md`).
  - `dbus-daemon --system` стартует при первом тесте (через step-glue).
- [ ] Новые feature-файлы в `bosun-cli/tests/features/`:
  - `runr_service.feature` — durability, cross-resource, validation,
    health-check, daemon-unavailable.
  - `systemd_service.feature` — InvocationID verify, daemon-reload
    throttle.
  - `service_unit_abstract.feature` — dispatch по init_system.
  - `command_run_deferred.feature` — deferred=True + only_if.
  - `defers_journal.feature` — dedup, priority order.
  - `validate_with.feature` — render-to-.new + validator + rename, error
    cases.
- [ ] Новые step-definition модули в `bosun-cli/tests/steps/`:
  - `runr.rs`: helpers для управления runr-daemon в контейнере (`start_runr_daemon`,
    `stop_runr_daemon`, `runr_service_state(name)`, `runr_service_restarts(name)`).
  - `systemd_mock.rs`: спавн python3-dbusmock с manager-template,
    helpers для mock-expectations.
  - `defers.rs`: assertions для `/tmp/bosun-defers/` — есть ли файл,
    содержимое JSON, attempt_count, и т.д.
- [ ] Сценарии из design (раздел «Тестирование → Уровень 3 — BDD в
  Docker») реализованы и зелёные в `cargo test --test bdd --
  --features=docker --include-ignored`.
- [ ] Симуляция краша: новый `--simulate-crash-after-file-write` флаг
  bosun-cli (только для тестов, gated через `#[cfg(any(test,
  feature = "bdd-test-hooks"))]`). После записи первого `file.content`
  процесс exit(2). Используется в `Defer survives bosun crash mid-apply`
  сценарии.

**Что НЕ в scope:**
- Run-tests-in-real-systemd (kvm-based).
- Performance benchmarks.

**Hard constraints:**
- `cargo test --test bdd --no-default-features` — должно собраться (BDD
  сценарии gated по feature, остальные unit-тесты работают всегда).
- Не запускать BDD-сценарии в обычном `cargo test` — только при
  `--features docker` или CI с включённой feature.

---

## Phase L: examples/pgbouncer-cluster

**Цель:** Реалистичный пример с `service.unit` + `file.content` с
`validate_with` + cross-resource notify через `reload_on`/`restart_on`.

**Acceptance criteria:**

- [ ] Директория `examples/pgbouncer-cluster/bundle/` создана и содержит:
  - `bundle.toml` с `entry = "main.star"`, `requires_bosun = "^0.4"`,
    тэгами `production`/`staging`.
  - `main.star` в bundle root: load `@bosun/builtins` + `@roles/pgbouncer`,
    `tags.require_one_of("production", "staging")`,
    `inventory.merge(base, prod-or-staging, strategy = "deep_map_replace_list")`,
    вызов `configure_pgbouncer(inv)`.
  - `roles/pgbouncer/main.star` с `configure(inv)` функцией: устанавливает
    pgbouncer через `apt.package`, пишет `/etc/pgbouncer/pgbouncer.ini`
    через `file.content` с `validate_with = ["pgbouncer", "-V",
    "{new_path}"]`, пишет `/etc/pgbouncer/userlist.txt`, пишет
    `/etc/runr/pgbouncer.service` через `render_service(...)` из
    `@lib/runr`, объявляет `service.unit(name="pgbouncer", state="running",
    enable=True, depends_on=[unit], reload_on=[ini, users],
    health_check_cmd=["pg_isready", "-h", "127.0.0.1", "-p", "6432"],
    health_check_retry=5, health_check_interval="3s")`, и `command.run`
    с `deferred=True` + `only_if=["pgrep", "pgbouncer"]` для SIGHUP при
    connection-limit incident.
  - `_lib/runr/main.star` — `render_service`/`render_timer`/`render_cgroup`
    функции (см. design «Native primitives → runr.service»).
  - `_lib/runr/templates/{service,timer,cgroup}.j2`.
  - `roles/pgbouncer/templates/{pgbouncer.ini,userlist.txt}.j2`.
  - `inventory/{base,production,staging}.yaml`.
  - `README.md` с командой `bosun apply --bundle ./bundle --tags production`
    и `bosun status`.
- [ ] BDD-сценарий
  `bosun-cli/tests/features/example_pgbouncer.feature` применяет example
  end-to-end в Docker:
  - Установка pgbouncer.
  - `pgbouncer.ini` validate pass + запуск через `service.unit`.
  - Изменение конфига (через `inventory/production.yaml`) → единый defer
    `reload:pgbouncer` → health-check pass → defer удалён.
  - Намеренная порча `pgbouncer.ini.j2` → validate fail → exit 1, файл
    не обновился.

**Что НЕ в scope:**
- Реальный PostgreSQL backend (только pgbouncer-фронт).
- HA / multi-host setup.

**Hard constraints:**
- Bundle проходит `bosun bundle validate --bundle ./bundle
  --tags=production --facts=fixtures/facts.json`.
- Все `.star` файлы соответствуют bundle-rev-2 layout (privacy,
  module-relative template).

---

## Финальная проверка (общая для всех фаз)

После завершения всех фаз:

1. `cargo fmt --all` — без diff'а.
2. `cargo clippy --workspace --all-targets -- --deny warnings` — без
   warnings.
3. `cargo test --workspace` — все unit-тесты зелёные.
4. `cargo test --test bdd --features docker -- --include-ignored` — все
   BDD-сценарии зелёные в Docker.
5. `cargo doc --workspace --no-deps --document-private-items` — без
   warnings.
6. Документация спека и плана совпадает с реализованным кодом
   (handover-проверка).
7. Все новые public enum имеют `#[non_exhaustive]`.
8. Никаких `unwrap()` / `expect()` / `panic!()` в production-путях (тесты
   и `tests::` модули — разрешены).

---

## Связь с предыдущими планами

- `2026-05-18-bosun-client-mvp.md` — базовая инфраструктура (Resource,
  Primitive, Orchestrator, FactsCollector, Bundle, Evaluator) — должна
  быть завершена и зелёная.
- Bundle-rev-2 imp план (если выпущен) — bundle layout с `_lib/`,
  `inventory/`, `tags`, `service.unit` — должны быть в продакшене.
- Этот план **сам не зависит** от Bundle-rev-2 implementation, но
  использует его примитивы (`_lib/runr/`, `inventory.load`/`merge`,
  `tags.has/require_one_of`). Если bundle-rev-2 ещё не имплементирован
  — Phase L (пример) откладывается, остальные фазы можно делать в
  старом MVP-bundle формате с временными костылями. Уточнить с
  controller'ом перед стартом.

---

## История ревизий

### rev 1 (2026-05-19)

Первая версия. Source spec —
`docs/superpowers/specs/2026-05-19-bosun-runr-systemd-defers-design.md` (rev 1).
Зафиксированные пользователем решения:

- Defers журнал в `/tmp/bosun-defers/` (tmpfs by design).
- Native клиенты в отдельных крейтах: `bosun-systemd-client`,
  `bosun-runr-client`.
- INI-рендер runr — в bundle `_lib/runr/` (Starlark функции), не в Rust.
- `service.unit` — абстрактная Starlark функция (option 3 из research).
- defers покрывают service-actions И `command.run`.
- main.star в bundle root, bundle.toml.entry default `"main.star"`.

Phase A–L соответствуют 12 укрупнённым задачам для subagent-driven
execution; каждая фаза — отдельный handoff для implementer-агента.
