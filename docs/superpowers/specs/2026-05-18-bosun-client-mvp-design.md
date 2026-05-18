---
title: bosun-client MVP — Design
date: 2026-05-18
status: draft (revision 3, pending user review)
author: dmitrivasilyev
reviewers:
  - devops (general-purpose subagent, 2026-05-18)
  - rust-architect (general-purpose subagent, 2026-05-18)
related:
  - .claude-memory-compiler/knowledge/concepts/bosun-overview.md
  - .claude-memory-compiler/knowledge/concepts/bundle-architecture.md
  - .claude-memory-compiler/knowledge/concepts/starlark-dsl.md
  - .claude-memory-compiler/knowledge/concepts/facts-module.md
  - .claude-memory-compiler/knowledge/concepts/manifest-vs-task.md
  - .claude-memory-compiler/knowledge/concepts/monotonicity-rule.md
  - .claude-memory-compiler/knowledge/concepts/self-upgrade.md
  - .claude-memory-compiler/knowledge/concepts/chiit-go-dsl.md
---

# bosun-client MVP — Design

## Контекст и цель

Первая итерация **bosun-client** — Rust-агента, который применяет на ноде декларативный bundle на Starlark. Демонстрационный сценарий: в свежем Docker-контейнере по bundle поставить пакет nginx, развернуть `/etc/nginx/nginx.conf` из шаблона.

Это не Go-итерация и не порт `postgres-chiit` на Rust. Это сразу Rust + Starlark, фокус — кости проекта: DSL evaluator, native API, реестр ресурсов, plan/apply, базовая подсистема фактов.

В первой итерации **нет** интеграции с chiit-server (warden discovery, ECDSA-подпись, Vault, storage_inventory): локальный bundle и локальный inventory.yaml — этого достаточно для проверки скелета. Интеграция с control plane — следующий design.

## Принципы (ценности)

Эти принципы — фон для всех решений ниже. При конфликте — этот раздел источник истины.

1. **Минимум exec, минимум флюктуаций, минимум нагрузки на систему.** В стационарном случае «всё уже стоит как надо» bosun не должен запускать внешних процессов.
2. **Хорошая изоляция модулей через явные контракты.** Каждый крейт workspace имеет один концерн и публичный API через trait/публичные типы. Изменение внутренностей одного крейта не ломает соседей.
3. **Полнота тестирования.** Чистая логика покрывается unit-тестами на tempfile/моках источников. Поведение end-to-end проверяется BDD-сценариями в реальном Docker — без подмен apt-get, dpkg или ФС.
4. **Никаких паник в production-path.** Ошибки возвращаются через `Result`, паника ловится `catch_unwind` на границах подсистем.
5. **Failure-mode фактов явная.** Факт может быть `Known`, `Unknown` или `Stale`. Манифест и примитивы обрабатывают это явно.
6. **Все exec — под deadline и cancel.** apt-get update и install эпизодически зависают; per-resource deadline + глобальный run deadline предотвращают застревание.

## Что в scope MVP

- Rust workspace `bosun-client/` с четырьмя крейтами.
- Native API в Starlark: `apt.package`, `file.content`, `template(...)`.
- Реестр ресурсов с handle-связями (`reload_on`, `depends_on`). В MVP связи регистрируются, но без сервисов их применение не наблюдаемо — основа для будущего `runr.service`.
- Подсистема фактов с failure-mode `Known / Unknown / Stale` и refresh-политиками `AtStart`, `AfterApply`.
- Dirty-tracking при AfterApply: после успешного apply ресурса bosun помечает зависимые факты как dirty; пересборка происходит лениво при следующем чтении факта.
- MVP-коллекторы фактов: `hostname`, `cpu_count`, `memory_mb`, `init_system`, `is_pod`, `installed_packages`.
- Cgroup-aware `cpu_count` и `memory_mb` (v1 и v2).
- Bundle layout: `bundle.toml` + `manifests/` + `defaults/` + `templates/`.
- Локальный inventory как override базовых значений.
- CLI: `bosun apply` (с `--dry-run`), `bosun version`. Команда `bosun plan` намеренно не делается — её роль покрывает `apply --dry-run`.
- Process-level mutex через `flock(/var/run/bosun.lock)`: при наличии активного прогона новый возвращает exit 0 с info-логом «another bosun instance is running».
- Метрика прогона в текстовом collector-файле для node_exporter.
- Три уровня тестов: unit, golden для шаблонов, BDD в Docker.

## Что НЕ в scope MVP

- Интеграция с chiit-server / bosun-server, warden discovery, ECDSA-подпись, Vault, storage_inventory, canary.
- `runr.service`, `runr.timer`, `service`, `systemd.*` — поэтому handle-связи не имеют наблюдаемого эффекта.
- `command.run`, `pg_sql.exec`, `http.get`, `file.read`, `file.write_temp` (task-globals, не manifest-globals).
- Tasks (императивные одноразовые действия) — отдельный режим, отдельный design.
- Live-факты (`pg_is_master`, ...) и discovery-факты (`postgres.version`, ...).
- Monotonicity-helpers (`apt.package_unless`) — нужны для live-фактов, которых нет.
- Загрузка bundle из удалённого источника, подпись bundle.
- Daemon-режим. Каждый прогон — отдельный короткоживущий процесс, перезапускается внешним таймером (runr).
- `Diff::Remove` — bundle не описывает удаление пакетов и файлов; такое поведение будет добавлено позже.
- Persistent fact cache между прогонами (на диске). В MVP факты собираются каждый прогон с нуля.

## Архитектура

### Workspace layout

```
bosun-client/
├── Cargo.toml                  # [workspace]
├── crates/
│   ├── bosun-core/             # типы, trait'ы, evaluator, registry, plan, apply, bundle loader
│   ├── bosun-facts/            # FactValue, trait Fact, FactsCollector, MVP-коллекторы
│   ├── bosun-primitives/       # impls trait Primitive: apt, file, template
│   └── bosun-cli/              # бинарь bosun, integration-тесты в docker
├── examples/
│   └── nginx-demo/
│       ├── bundle/
│       │   ├── bundle.toml
│       │   ├── manifests/main.star
│       │   ├── defaults/main.yaml
│       │   └── templates/nginx.conf.j2
│       └── README.md
├── docker/
│   └── test-base.Dockerfile    # FROM debian:bookworm-slim
└── README.md
```

### Crate boundaries

| Крейт | Концерн | Зависит от |
|---|---|---|
| `bosun-core` | контракты (trait Primitive, FactsSource, InventorySource), типы (Resource, Diff, ChangeReport, FactValue, ApplyCtx), Starlark-evaluator, Registry, Evaluator, Orchestrator, Bundle-loader | `starlark`, `serde`, `serde_norway` (см. зависимости), `serde_json`, `thiserror`, `semver`, `tokio-util` (CancellationToken) |
| `bosun-facts` | сбор фактов с failure-mode | `bosun-core` (только для типов FactValue/FactCategory/RefreshPolicy и трейта Fact), `procfs` (зафиксирован на минор) |
| `bosun-primitives` | impls `Primitive`: `apt.package`, `file.content`; функция `template(...)` | `bosun-core`, `minijinja`, `sha2`, `tempfile` |
| `bosun-cli` | CLI, конфигурация tracing, integration-тесты | все три выше, `clap`, `tracing`, `tracing-subscriber`, `anyhow`, `fs4` (advisory flock, активный fork устаревшего `fs2`), `cucumber` (dev), `testcontainers` (dev) |

Принципы декомпозиции:
- `bosun-core` не знает ничего об apt, файлах, шаблонах, конкретных фактах. Только контракты.
- `bosun-facts` не знает о Primitive'ах. Только как собрать наблюдения.
- `bosun-primitives` не знает о CLI и трассировке. Только как реализовать `Primitive`.
- `bosun-cli` — единственный потребитель всех остальных. Здесь настраивается tracing-subscriber, маппинг exit-кодов, BDD-тесты, flock.

## bosun-core

### Trait `Primitive`

```rust
pub trait Primitive: Send + Sync {
    fn type_name(&self) -> ResourceKind;

    /// Какие поля payload участвуют в построении ResourceId.
    /// Например, для apt.package — ["name"]; для file.content — ["path"].
    fn identity_keys(&self) -> &'static [&'static str];

    /// Парсит Starlark-аргументы вызова в типизированный payload.
    /// Возвращает payload как serde_json::Value (унифицированный обмен
    /// между ядром и примитивами), но конкретный примитив определяет
    /// собственный Spec-тип через serde и десериализует payload в него
    /// в начале plan/apply.
    fn build_payload(
        &self,
        args: &CallArgs,
        ctx: &EvalCtx,
    ) -> Result<serde_json::Value, PrimitiveError>;

    fn plan(
        &self,
        resource: &Resource,
        facts: &dyn FactsSource,
        ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError>;

    fn apply(
        &self,
        resource: &Resource,
        diff: &Diff,
        ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError>;
}
```

Принципы:

- `build_payload` отдаёт `serde_json::Value`, ядро на основе `identity_keys` собирает `ResourceId`. Это снимает с примитива ответственность за уникальность id и фиксирует контракт: id зависит от значений конкретных полей payload.
- Каждый примитив определяет приватный `Spec`-тип через `serde::Deserialize`. В начале `plan` и `apply` примитив делает `serde_json::from_value::<Spec>(resource.payload.clone())?`. Это даёт типобезопасность внутри примитива при сохранении type-erased обмена через `Resource.payload`.
- Тестирование `plan`/`apply` в unit'ах: тест строит `Resource` напрямую с `payload: serde_json::json!({"name": "nginx", "version": "1.18.0"})` — без Starlark.
- `PlanCtx` и `ApplyCtx` несут `deadline` и `cancel`, см. ниже.

```rust
#[non_exhaustive]
pub struct PlanCtx {
    pub deadline: std::time::Instant,
    pub cancel: tokio_util::sync::CancellationToken,  // Clone, дёшев (Arc внутри)
}

#[non_exhaustive]
pub struct ApplyCtx {
    pub deadline: std::time::Instant,
    pub cancel: tokio_util::sync::CancellationToken,
    pub log_span: tracing::Span,                       // Clone, дёшев
    pub sensitive: std::sync::Arc<SensitiveStore>,     // side-channel для file.content.contents
}
```

`PlanCtx`/`ApplyCtx` — `#[non_exhaustive]`, передаются по value (поля внутри `Clone`-дешёвые), без lifetime-параметра. Это убирает `'a` из сигнатур trait-методов и упрощает trait-object эргономику. `SensitiveStore` (см. ниже) хранит реальные тексты `file.content.contents` по `ResourceId` и инкапсулирует доступ через типобезопасный newtype.

### `CallArgs` (Starlark-glue только)

```rust
impl CallArgs {
    pub fn required_str(&self, name: &str) -> Result<String, CallArgsError>;
    pub fn optional_str(&self, name: &str) -> Result<Option<String>, CallArgsError>;
    pub fn optional_u32(&self, name: &str) -> Result<Option<u32>, CallArgsError>;
    pub fn optional_handle_list(&self, name: &str) -> Result<Vec<ResourceId>, CallArgsError>;
}
```

Это вспомогательный API между Starlark-evaluator'ом и примитивом. Изоляция: тесты примитива не зависят от `CallArgs`, потому что `plan` и `apply` принимают `Resource` напрямую.

### `Resource`, `Diff`, `ChangeReport`, `ResourceKind`

```rust
/// Newtype над строкой kind. Создаётся через два конструктора —
/// `from_static` для built-ins (один alloc в Arc, не const fn) и
/// `try_new` для динамической регистрации (в будущем, для PackageProvider
/// trait). Хранит Arc<str> для дешёвого clone.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ResourceKind(std::sync::Arc<str>);

impl ResourceKind {
    /// Для built-ins. Статика гарантирует формат на этапе компиляции.
    /// Аллокация в Arc — не const, но дешёвая (один alloc на built-in).
    pub fn from_static(s: &'static str) -> Self;
    /// Для runtime-регистрации. Валидация формата (kebab-case + точки).
    pub fn try_new(s: impl Into<String>) -> Result<Self, ResourceKindError>;
    pub fn as_str(&self) -> &str;
}

pub struct ResourceId(std::sync::Arc<str>);  // дешёвый clone

pub struct Resource {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub spec_version: u16,           // версия схемы payload, для будущей миграции
    pub payload: serde_json::Value,  // type-erased для ядра; примитив десериализует в свой Spec
    pub reload_on: Vec<ResourceId>,
    pub depends_on: Vec<ResourceId>,
}

#[non_exhaustive]
pub enum Diff {
    NoChange,
    Add {
        description: String,
        payload: serde_json::Value,
    },
    Update {
        from: serde_json::Value,
        to: serde_json::Value,
        description: String,
    },
}

pub struct ChangeReport {
    pub changed: bool,
    pub message: String,
}
```

Изменения относительно первой версии:
- `ChangeReport` больше не хранит `error`. Ошибка — это `Err(PrimitiveError)` из `apply`; `ChangeReport` описывает только успех (с изменением или без). Двойного канала ошибки больше нет.
- `Resource.kind: ResourceKind` (newtype) вместо `&'static str`.
- `Resource.id` строится ядром по `identity_keys`. Примитив не имеет права формировать id.
- `Resource.spec_version` зарезервирован для будущей миграции схемы payload.
- `Diff` помечен `#[non_exhaustive]`.

### Registry

```rust
pub struct Registry {
    resources: Vec<Resource>,
    by_id: HashMap<ResourceId, usize>,
}

impl Registry {
    pub fn add(&mut self, r: Resource) -> ResourceId;
    pub fn topological_order(&self) -> Result<Vec<ResourceId>, RegistryError>;
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("dependency cycle detected: {path}")]
    Cycle { path: String },
    #[error("duplicate resource id: {0}")]
    DuplicateId(ResourceId),
    #[error("unknown handle referenced: {0}")]
    UnknownHandle(ResourceId),
}
```

`topological_order` — Kahn-алгоритм. `reload_on` и `depends_on` в MVP обрабатываются одинаково (одинаковый ребро). Различение семантики (цикл по `reload_on` валиден в Puppet) — open question, см. ниже.

### Starlark evaluator glue

Над крейтом `starlark` строится модуль `bosun_core::starlark_glue`. Регистрируются глобальные объекты:

- `apt` — объект с методом `package(name=..., version=..., reload_on=..., depends_on=...)`.
- `file` — объект с методом `content(path=..., contents=..., mode=..., owner=..., group=..., reload_on=..., depends_on=...)`.
- `template` — функция `template(path)`.
- `inv` — read-only объект с динамическим атрибутом-доступом: `inv.foo`, `inv.facts.bar`.

`load`-резолвер регистрирует виртуальный путь `"@bosun/builtins"`:

```python
load("@bosun/builtins", "apt", "file", "template")
```

Префикс `@bosun/` явно маркирует, что это built-ins, и оставляет `//path` свободным для будущих пользовательских модулей из bundle. Реальные built-ins хранятся в Rust, не в файле bundle.

### Evaluator и Orchestrator

```rust
pub struct Evaluator {
    primitives: HashMap<ResourceKind, Box<dyn Primitive>>,
    inventory: Inventory,
    bundle: Bundle,
}

impl Evaluator {
    pub fn new(primitives: ..., inventory: Inventory, bundle: Bundle) -> Self;

    /// Запускает Starlark-evaluation для entry manifest'а,
    /// возвращает заполненный Registry.
    /// Снимок фактов передаётся как FactsSnapshot — иммутабельная вью.
    pub fn evaluate(
        &mut self,
        facts: &FactsSnapshot,
        ctx: &PlanCtx,
    ) -> Result<Registry, EvalError>;
}

pub struct Orchestrator {
    primitives: HashMap<ResourceKind, Box<dyn Primitive>>,
}

impl Orchestrator {
    pub fn new(primitives: ...) -> Self;

    /// Только plan, никаких apply. Используется для --dry-run.
    pub fn plan_only(
        &self,
        registry: &Registry,
        facts: &FactsView,
        ctx: &PlanCtx,
    ) -> PlanReport;

    /// plan + apply per-resource c dirty-tracking фактов.
    pub fn apply(
        &self,
        registry: &Registry,
        facts: &FactsView,
        opts: ApplyOpts,
        ctx: &ApplyCtx,
    ) -> ApplyReport;
}

pub struct ApplyOpts {
    pub continue_on_error: bool,
}
```

`Evaluator` отвечает за: Starlark → Registry. `Orchestrator` отвечает за: Registry + Facts → план/применение. Это вынесение разделяет «evaluation» (детерминированная функция от inventory+facts) и «execution» (взаимодействие с системой).

`FactsSnapshot` — immutable вью для evaluation (Starlark видит фиксированный набор фактов). `FactsView<'_>` — mutating ссылка на `FactsCollector` для plan/apply, поддерживает lazy refresh по dirty-флагам.

`PlanReport` / `ApplyReport` — `#[non_exhaustive]`, сериализуются в JSON через `serde`.

## bosun-facts

### FactValue, FactCategory, RefreshPolicy

```rust
#[non_exhaustive]
pub enum FactValue {
    Known(serde_json::Value),
    Unknown { reason: String },
    Stale { value: serde_json::Value, age: std::time::Duration },
}

#[non_exhaustive]
pub enum RefreshPolicy {
    AtStart,
    AfterApply { triggers: Vec<ResourceKind> },
}

#[non_exhaustive]
pub enum FactCategory {
    Static,
    Slow,
    Live,
    Discovery,
}
```

`FactCategory` остаётся как документационная классификация. В runtime используется `RefreshPolicy`.

`FactValue::Stale` появляется в MVP в одном случае: факт помечен dirty через `mark_dirty_after_apply`, следующий `FactsView::get` запросил пересборку, а новый `collect` вернул `Unknown` — предыдущее `Known` сохраняется как `Stale { value, age }`. Это даёт устойчивость к транзиентным сбоям сборки факта.

### Trait Fact и FactsCollector

```rust
pub trait Fact: Send + Sync {
    fn name(&self) -> &str;
    fn category(&self) -> FactCategory;
    fn refresh_policy(&self) -> RefreshPolicy;
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue;
}

pub struct FactsCollector {
    facts: Vec<Box<dyn Fact>>,
    /// Interior mutability через RefCell — позволяет trait FactsSource
    /// иметь `fn get(&self, ...)` и при этом lazy-refresh-ить факты.
    /// Однопоточная модель агента (sequential apply) делает RefCell корректным
    /// выбором: при многопоточности заменить на Mutex.
    cache: std::cell::RefCell<HashMap<String, CachedFact>>,
}

struct CachedFact {
    value: FactValue,
    collected_at: std::time::Instant,
    dirty: bool,
}

impl FactsCollector {
    pub fn with_default_collectors() -> Self;
    pub fn collect_at_start(&self);

    /// Помечает все факты с AfterApply.triggers содержащими applied_kind как dirty.
    /// НЕ пересобирает сразу — пересборка происходит лениво при следующем
    /// обращении к факту через FactsView::get. Вызывается при diff != NoChange
    /// независимо от того, был apply успешен или нет: если apply пытался
    /// модифицировать систему — мы не знаем точно, что осталось на диске,
    /// поэтому пересобираем факты заново.
    pub fn mark_dirty_after_apply(&self, applied_kind: &ResourceKind);

    /// Иммутабельный снапшот для Starlark-evaluation. После snapshot
    /// дальнейшие mark_dirty не видны через эту вью.
    pub fn snapshot(&self) -> FactsSnapshot;

    /// Mutable вью для plan/apply. Существование вью совместимо
    /// с RefCell — методы FactsView только вызывают сам FactsCollector,
    /// а interior mutability спрятана внутри.
    pub fn view(&self) -> FactsView<'_>;
}

pub struct FactsSnapshot { /* immutable view, владеет копией Known/Stale */ }
impl FactsSource for FactsSnapshot { /* get(&self, name) -> FactValue */ }

pub struct FactsView<'a> { collector: &'a FactsCollector }
impl FactsSource for FactsView<'_> {
    /// На каждый get: если факт помечен dirty — повторный collect()
    /// в catch_unwind. Если новый результат Unknown — сохраняем предыдущее
    /// Known как Stale { value, age }, чтобы downstream-логика не флапала
    /// на транзиентном сбое сборки.
    /// Мутации кэша спрятаны за RefCell внутри FactsCollector.
    fn get(&self, name: &str) -> FactValue;
}
```

Каждый коллектор обёрнут в `std::panic::catch_unwind` + `AssertUnwindSafe` (Box<dyn Fact> не UnwindSafe по умолчанию; assert корректен потому что после паники сборщика мы больше не используем `self` коллектора). Паника → `FactValue::Unknown { reason: "panic: {message}" }`.

### Lazy refresh — dirty-tracking механика

Поток `apply`:

1. `facts.collect_at_start()` — собрать все `AtStart` факты.
2. `snapshot = facts.snapshot()` — immutable вью для evaluation.
3. `registry = evaluator.evaluate(&snapshot, ...)`.
4. `view = facts.view()` — mutable ссылка для plan/apply.
5. Для каждого ресурса `R` в топ-порядке:
   - `diff = primitive.plan(R, &view, ...)`. Если в plan'е примитив запросил факт, помеченный dirty — `view.get` пересобирает его перед отдачей.
   - Если diff != NoChange: `report = primitive.apply(R, &diff, ...)`.
   - Если `report.changed`: `facts.mark_dirty_after_apply(R.kind)`.

Эффект: если bundle содержит `apt.package nginx`, `apt.package curl`, `apt.package vim`, `file.content /etc/...`, и только `file.content` читает `inv.facts.installed_packages` — пересборка `installed_packages` произойдёт **один раз** перед `file.content`, а не три раза после каждого apt-apply.

### MVP-коллекторы

| Имя | Категория | RefreshPolicy | Источник |
|---|---|---|---|
| `hostname` | Static | AtStart | `procfs` (`/proc/sys/kernel/hostname`) — без зависимости на `nix` |
| `cpu_count` | Static | AtStart | cgroup-aware (см. ниже) |
| `memory_mb` | Static | AtStart | cgroup-aware (см. ниже) |
| `init_system` | Static | AtStart | `procfs` (`/proc/1/comm`); значения `systemd`, `runit`, `init`, `unknown`. Чтение `/proc/1/exe` пропускается под не-root (EACCES) |
| `is_pod` | Static | AtStart | Иерархия проверок (см. ниже) |
| `installed_packages` | Slow | AfterApply { triggers: [`apt.package`] } | парсинг `/var/lib/dpkg/status` + `/var/lib/apt/lists/` |

#### `is_pod` — иерархия детектов

В порядке убывания приоритета:

1. Env-override `BOSUN_FORCE_POD=true|false` — тестовое или ручное переопределение.
2. Файл `/var/run/secrets/kubernetes.io/serviceaccount/token` существует.
3. Env-переменная `KUBERNETES_SERVICE_HOST` непустая.
4. `/proc/1/cgroup` содержит `kubepods` или `containerd` — pod.
5. Иначе — `false`.

Каждый сработавший шаг логируется через `tracing::debug!` с указанием reason — это нужно для отладки на mixed-инфраструктуре (lxc, systemd-nspawn, k3s).

#### Cgroup-aware `cpu_count`

- Если `is_pod = false` → `num_cpus::get`.
- Если `is_pod = true` → читаем cgroup-лимит:
  - v2: `/sys/fs/cgroup/cpu.max`. Формат `"quota period"`. `ceil(quota / period)`. Если `quota == "max"` → fallback на `num_cpus`.
  - v1: `/sys/fs/cgroup/cpu/cpu.cfs_quota_us` и `cpu.cfs_period_us`. Если `quota == -1` → fallback.
- Версия cgroup определяется по существованию `/sys/fs/cgroup/cgroup.controllers` (v2).

#### Cgroup-aware `memory_mb`

- Если `is_pod = false` → `MemTotal` из `/proc/meminfo`.
- Если `is_pod = true`:
  - v2: `/sys/fs/cgroup/memory.max`. Если `"max"` → fallback на `MemTotal`.
  - v1: `/sys/fs/cgroup/memory/memory.limit_in_bytes`. Если значение `>= CGROUP_V1_UNLIMITED` (константа `9223372036854771712`, классический «без лимита» v1) → fallback.

#### `installed_packages`

Переработанный парсер по мотивам `self-upgrade/src/dpkg/`, с исправлениями:

- Файлы в `/var/lib/apt/lists/` перебираются БЕЗ фильтра по расширению (фикс бага self-upgrade: имена в стандартной Ubuntu/Debian идут без суффикса `.Packages`).
- Сравнение версий — через крейт `debversion` (MIT/Apache-2.0). Своя реализация не делается: правила Debian comparison (`~`, `+nmu`, `1:1.0-1ubuntu1.18.04.1`) — нетривиальны, ошибки приведут к ложным «no-change» и отказу применять апгрейды.
- Перед чтением `/var/lib/dpkg/status` проверяется наличие `/var/lib/dpkg/lock-frontend`. Если файл существует и его mtime в пределах последних 30 секунд — велик шанс, что dpkg сейчас пишет; читаем как обычно, но если получили consistent map — `Known`, иначе при детекте transient-состояния (например, отсутствует разделитель пустой строкой в конце) — возвращаем последнее `Known` как `Stale`, а не `Unknown`. Это предотвращает массовый «`apt-get update` fallback» при работе unattended-upgrades.

Возвращает `HashMap<String, PackageInfo { current_version: Option<String>, candidate_version: Option<String> }>`.

### Доступ из Starlark

Манифест читает факт через `inv.facts.<name>`. Трансляция `FactValue` → Starlark:

- `Known(v)` → значение `v` (объект, число, строка, словарь).
- `Unknown { reason }` → `None`. bosun-core логирует `reason` через `tracing::warn!`.
- `Stale { value, age }` → значение `value`. Логируется через `tracing::info!` (не warn — Stale нормален для AfterApply случаев).

Манифест может проверить `if inv.facts.foo == None:`. Для `apt.package` фолбэк при `Unknown` — внутренний.

## bosun-primitives

### `apt.package`

Spec (приватный тип внутри `bosun-primitives::apt`):

```rust
#[derive(serde::Deserialize)]
struct AptPackageSpec {
    name: String,
    version: Option<String>,
    timeout_sec: Option<u32>,  // открытое поле для следующей итерации; default 300
}
```

- `build_payload`:
  - `name: str` — обязательный.
  - `version: str | None` — необязательный.
  - `reload_on: list | None`, `depends_on: list | None` — обрабатываются ядром.
  - Возвращает payload как JSON.
- `identity_keys() = &["name"]`. Ядро строит `id = "apt.package:<name>"`.
- `plan`:
  - Читает `facts.get("installed_packages")`.
  - `Known(map)`:
    - Если пакета в map нет → `Add { install <name> [<version>] }`.
    - Если стоит и `version is None` → `NoChange`.
    - Если стоит и `version == current_version` → `NoChange`.
    - Если стоит и `version != current_version` → `Update`.
  - `Unknown` или `Stale` → fallback `Add { description: "install (facts unknown, fallback)" }`. Объяснение в plan-отчёте.
- `apply` — пошагово. Модель основана на «лучших практиках» из исследования (см. список источников в spec истории): максимально опираемся на встроенные механизмы apt (`APT::Acquire::Retries`, `DPkg::Lock::Timeout`), плюс агентная защита от half-configured-state в стиле chiit:

  1. **Dpkg-lock probe (quick-fail).** Перед exec пробуем взять non-blocking advisory-lock на `/var/lib/dpkg/lock-frontend` через `fcntl(F_SETLK, F_WRLCK)`. Если занят кем-то другим — возвращаем `PrimitiveError::DpkgLocked { holder_pid: Option<i32> }`. Никакого exec, пытаемся на следующем прогоне через 30 секунд. Это даёт quick-fail при работающем `unattended-upgrades` (который может работать минутами); ждать его блокированно — потерять прогон.
  2. **Exec install со встроенными ретраями apt.** Команда:
     ```
     apt-get install -qy
       -oDpkg::Options::=--force-confdef
       -oDpkg::Options::=--force-confold
       -oAPT::Acquire::Retries=3
       -oDPkg::Lock::Timeout=30
       --allow-downgrades
       --allow-change-held-packages
       <name>[=<version>]
     ```
     `APT::Acquire::Retries=3` — встроенный в apt механизм retry скачивания .deb-файлов на сетевые ошибки. `DPkg::Lock::Timeout=30` — safety net на TOCTOU между нашим probe и реальным запуском apt. Per-resource deadline = `min(ctx.deadline, now + spec.timeout_sec)`. По умолчанию `timeout_sec = 600` — этого достаточно для тяжёлых пакетов вроде `postgresql`, `mariadb`, `linux-headers-*` на медленных зеркалах.
  3. **Анализ результата install:**
     - exit 0 → `ChangeReport { changed: true, message: "installed <name>=<v>" }`.
     - exit 100, stderr содержит `"dpkg was interrupted"` → один retry с предварительной очисткой state. Стратегия — chiit-style:
       - exec `dpkg --configure -a` (отдельный per-attempt deadline 60s).
       - Если `dpkg --configure -a` вернул не 0 → `PrimitiveError::Exec { reason: "dpkg --configure -a failed", ... }`. Без retry install.
       - Если 0 → ровно один retry команды install из шага 2. Если retry упал — `PrimitiveError::Exec`, без дальнейших попыток.
     - exit 100, stderr содержит `"Unable to locate package"` или `"Unable to fetch some archives"` → fallback к `apt-get update` (см. шаг 4), затем ровно один retry install. Это явный трейдоф, согласованный с пользователем: на 60k нод массовый `apt-get update` — большой трафик, но без него bundle с устаревшим apt-lists вообще не сможет поставить новый пакет.
     - exit 100, иное stderr (404 на конкретный архив, GPG error, dependency error) → `PrimitiveError::Exec`. Не retry — эти причины не лечатся повтором.
     - иной exit-code → `PrimitiveError::Exec`. Не retry.
  4. **`apt-get update` (фаза fallback).** Команда:
     ```
     apt-get update -q -oAPT::Acquire::Retries=3 -oDPkg::Lock::Timeout=30
     ```
     Поверх встроенного `APT::Acquire::Retries=3` мы добавляем внешний retry-loop на случай когда внутренние попытки apt все упали:
     - До 3 попыток на стороне bosun.
     - exponential backoff между ними: 5s → 10s → 20s.
     - Per-attempt deadline 30s.
     - Total budget update-фазы ~150 секунд.
     - Retriable условия (после провала всех `APT::Acquire::Retries`): exit != 0 и stderr содержит транзиентные сигналы (`connection refused`, `timed out`, `Temporary failure in name resolution`, `503`, `504`, `Hash Sum mismatch`). Если stderr пуст и exit != 0 — считаем retriable (TCP reset без сообщения).
     - Non-retriable: `GPG error`, `permission denied`. Однократная попытка, без retry.
  5. **Захват stderr.** При любом не-нулевом exit'е первые и последние N строк stderr (~20) сохраняются в `PrimitiveError::Exec { reason, exit, stderr_excerpt }`. Полный stderr — в `/var/log/bosun/apt-<step>-last-error.log` (перезаписывается). Без этого post-mortem на 60k нод невозможен.
  6. **Cancel/deadline checks.** Между шагами и попытками — проверка `ctx.cancel.is_cancelled()` и `Instant::now() >= ctx.deadline`. При срабатывании — `PrimitiveError::Cancelled`, не дёргаем дальнейшие шаги.

Сравнение с другими SCM:
- Chef `apt_package` и Ansible `apt` модули — retry один-в-один без учёта half-configured. Bosun сильнее за счёт `dpkg --configure -a` перед retry.
- chiit — реагирует на «dpkg was interrupted» через `dpkg --configure -a` и retry. Bosun наследует, плюс использует встроенные `APT::Acquire::Retries`/`DPkg::Lock::Timeout` вместо собственных циклов на скачивание.

Источники: см. секцию «История ревизий» rev 3.

### `file.content`

Spec:

```rust
#[derive(serde::Deserialize)]
struct FileContentSpec {
    path: String,
    contents: String,
    #[serde(default = "default_mode")] mode: u32,
    owner: Option<String>,
    group: Option<String>,
}
```

- `build_payload`: парсит `path`, `contents` (обязательные), `mode` (default `0o644`), `owner`, `group`, `reload_on`, `depends_on`.
- `identity_keys() = &["path"]`. id = `file.content:<path>`.
- `plan`:
  - `symlink_metadata(path)` (не `metadata` — детектим symlink):
    - Не существует → `Add`.
    - Symlink → `PrimitiveError::InvalidTarget` (отказываемся писать через symlink — это политика безопасности; обходить symlink молча — путь к багам).
    - Регулярный файл, sha256 контента совпадает, mode совпадает, владелец/группа совпадают → `NoChange`.
    - Иначе → `Update { from: {sha256, size, mode, owner, group}, to: {sha256, size, mode, owner, group} }`.
- `apply` — пошагово:
  1. **Re-plan непосредственно перед изменением.** Заново вычисляем `symlink_metadata` + sha256 контента и пересчитываем diff. Если результат отличается от plan-времени (а apt-applies между ними могли быть долгими) — продолжаем уже по свежему diff. Это **не закрывает TOCTOU полностью** (между re-check и `rename` всё ещё есть окно), но устраняет основной класс расхождений «plan был давно, файл уже другой». Атомарность собственно записи обеспечивается atomic-rename внутри одной FS (шаг 3).
  2. **Backup только при Update.** При `Add` файла нет, бэкап не нужен. При `Update`:
     - Целевой путь: `/var/backups/bosun{path}.{ts}`, например `/etc/nginx/nginx.conf` → `/var/backups/bosun/etc/nginx/nginx.conf.20260518T135700Z`. `{ts}` — UTC в формате `YYYYMMDDTHHMMSSZ`.
     - **Rotation.** После создания бэкапа удаляем все, кроме последних 5 для этого пути (сортировка по `ts`). Это предотвращает разрастание `/var/backups/bosun` на нодах, где конфиг флапает.
     - **Атрибуты.** Бэкап делается через `std::fs::copy` (сохраняет mode); владелец/группа копируются явно через `chown`. xattr/SELinux в MVP **не сохраняются** (open question — нужно ли).
  3. **Atomic write.** `tempfile` в той же FS, `write_all`, `fsync`, `chmod`, `chown` (если запрошен), потом `rename`. После rename — нет окна частичной записи.
  4. **Chown семантика.**
     - Если `owner`/`group` не указаны в манифесте — не трогаем.
     - Если указаны и совпадают с текущим — no-op (это уже учтено в plan, но при re-check может всплыть).
     - Если указаны и не совпадают:
       - Под root → `chown(uid, gid)`.
       - Под не-root → `PrimitiveError::ChownNotPermitted { requested: "{owner}:{group}", actual: "{current_owner}:{current_group}" }` с подсказкой «requires root».
  5. `ChangeReport { changed: true, message: "wrote /etc/nginx/nginx.conf (sha256=ab12cd...)" }`.

### `template` — pure function

`template(path)` — функция в globals Starlark, не Primitive. Возвращает строку.

- `path` — относительный путь от `<bundle>/templates/`.
- Файл читается, рендерится через `minijinja`. `Environment` создаётся с `set_undefined_behavior(UndefinedBehavior::Strict)` — `{{ inv.missing }}` без значения → ошибка, **не пустая строка**. Это критично для конфигов: тихий undefined превращается в синтаксически валидный пустой конфиг и сервис ломается.
- Контекст рендера: `{ inv, inv.facts }` (через type-erased serde-объекты).
- При любой ошибке (отсутствует файл, синтаксис jinja, undefined variable, missing inv key) — `fail()` в Starlark с ясным сообщением + указанием template-файла и строки.

Чисто value-producer. Никаких сайд-эффектов и регистрации в Registry.

### Sensitive payload

`file.content.contents` может содержать секреты (пароли через шаблон). Чтобы не утечь в логи и в `PlanReport`:

```rust
/// Хранилище секретов, передаётся в ApplyCtx.sensitive.
pub struct SensitiveStore {
    inner: std::sync::Mutex<std::collections::HashMap<ResourceId, SensitivePayload<String>>>,
}

impl SensitiveStore {
    /// Положить тело контента под id ресурса. Вызывается evaluator'ом
    /// при build_payload для file.content.
    pub fn put(&self, id: &ResourceId, value: SensitivePayload<String>);
    /// Достать тело контента для apply. Vec вытаскивается ровно один раз
    /// и потом удаляется из store — снижает риск двойного использования.
    pub fn take(&self, id: &ResourceId) -> Option<SensitivePayload<String>>;
}

/// Маскирующий newtype: Debug/Display печатают "<sensitive: N bytes>",
/// не настоящее содержимое.
pub struct SensitivePayload<T>(T);

impl<T> std::fmt::Debug for SensitivePayload<T>
where T: AsRef<str> { /* maskirovat */ }
```

Поток:
1. В `Evaluator::evaluate` при вызове `file.content(...)` Starlark-glue:
   a. Кладёт реальный `contents` в `SensitiveStore::put` через `ApplyCtx.sensitive`.
   b. В `Resource.payload` пишет только `{ "path": ..., "content_sha256": "ab12...", "content_size": N, "mode": ..., "owner": ..., "group": ... }`.
2. В `Primitive::plan` сравнение идёт по sha256+size — достаточно для diff.
3. В `Primitive::apply` `FilePrimitive` запрашивает `ctx.sensitive.take(&resource.id)` и пишет тело на диск через `tempfile + rename`.
4. После `apply` тело удалено из store (Drop). В `ApplyReport` тела нет — есть только sha256.

Гарантии:
- На любом `--log-level` тело `contents` НЕ логируется (sha256 в `Resource.payload` — да; настоящее тело — только в момент записи на диск).
- `PlanReport`/`ApplyReport` через serde сериализуют `Resource.payload`, секретов там нет.
- `SensitivePayload<T>` через `Debug`/`Display` маскирует значение — случайный `tracing::debug!("{:?}", payload)` не утечёт.
- Линтер CI проверяет, что `tracing::*!` macroses не принимают сам `contents`-аргумент напрямую (паттерн на AST).

Это даёт типобезопасность сейчас (до того, как туда придут реальные пароли через template), и не ломает design Resource как сериализуемой структуры.

### Handle-связи

`file.content(...)` и `apt.package(...)` возвращают `Handle` — opaque newtype над `ResourceId`. В Starlark — обычное значение, передаваемое в `reload_on=[...]` или `depends_on=[...]` другого ресурса.

В MVP связи регистрируются в `Registry` и влияют на топологический порядок. Ресурсов, реагирующих на `reload_on`, в MVP нет (нет сервисов). Подготовка к будущему `runr.service` без переработки ядра.

## bosun-cli

### Команды и флаги

```
bosun apply  --bundle <dir> [--inventory <yaml>] [--dry-run] [--continue-on-error]
bosun version
```

Команды `bosun plan` нет. `bosun apply --dry-run` — единственный способ получить план. Сделано сознательно, чтобы избежать расхождения двух subcommand'ов с одинаковой семантикой.

Общие флаги:

| Флаг | Тип | Default | Назначение |
|---|---|---|---|
| `--bundle <dir>` | path | required | путь к bundle directory |
| `--inventory <yaml>` | path | none | override `bundle/defaults/main.yaml` |
| `--log-level <l>` | enum | `info` | `debug \| info \| warn \| error` |
| `--log-format <f>` | enum | `text` | `text \| json` (логи в stderr) |
| `--format <f>` | enum | `text` | `text \| json` (отчёт в stdout) |
| `--no-color` | bool | false | отключить ANSI-цвета в text-режиме |
| `--lock-path <p>` | path | `/var/run/bosun.lock` | путь к файлу advisory-lock |
| `--deadline-sec <n>` | u32 | 600 | глобальный deadline на весь прогон |
| `--state-dir <p>` | path | `/var/lib/bosun` | директория для state-файлов |
| `--log-dir <p>` | path | `/var/log/bosun` | директория для последних stderr-логов exec'ов |
| `--backup-dir <p>` | path | `/var/backups/bosun` | директория бэкапов для file.content |

apply-специфика:

| Флаг | Назначение |
|---|---|
| `--dry-run` | только plan, никаких apply |
| `--continue-on-error` | не останавливаться на первой ошибке ресурса |

Разделение каналов: **логи всегда в stderr, отчёт всегда в stdout.** Это даёт чистый pipeline `bosun apply --format json | jq ...` без смешивания.

### Flow

```
1. parse CLI args (clap derive)
2. bootstrap_dirs(): mkdir_p ensures для state-dir, log-dir, backup-dir.
   Для каждой:
     - попытка create_dir_all
     - on Ok                          → продолжаем
     - on PermissionDenied            → exit 4 с сообщением «cannot create <dir>, run as root or pre-create directory»
     - on NotADirectory / иное Io     → exit 4 с тем же диагностическим сообщением
   Эта секция выполняется ДО flock — иначе lock-файл нельзя создать на пустой ФС.
3. flock(lock_path) — F_SETLK F_WRLCK через fs4. Три исхода:
     - Lock получен → продолжаем.
     - WouldBlock (другой bosun держит) → tracing::info!("another bosun instance is running"), НЕ обновляем метрику, exit 0.
     - Io error (Permission denied на создание/открытие, NotADirectory) → tracing::error!, exit 4. Это другая ситуация, не «lock taken».
4. tracing-subscriber init по --log-level/--log-format
5. global_deadline = Instant::now() + Duration::from_secs(args.deadline_sec)
6. cancel = CancellationToken::new()
   signal::ctrl_c() / SIGTERM → cancel.cancel()
7. Bundle::load_dir(args.bundle)
8. semver-проверка `requires_bosun` vs CARGO_PKG_VERSION → exit 3 при несовпадении
9. inventory = bundle.merge_inventory(args.inventory.map(parse_yaml).unwrap_or(Null))
10. facts = FactsCollector::with_default_collectors()
    facts.collect_at_start()  (под catch_unwind+AssertUnwindSafe на каждый коллектор)
11. evaluator = Evaluator::new(primitives, inventory, bundle)
12. snapshot = facts.snapshot()
13. registry = evaluator.evaluate(&snapshot, &plan_ctx)?  // exit 3 при manifest error
14. orchestrator = Orchestrator::new(primitives)
15. ветка:
    --dry-run: report = orchestrator.plan_only(&registry, &facts.view(), &plan_ctx)
                exit_code = if report.has_drift { 2 } else { 0 }
    apply:     report = orchestrator.apply(&registry, &facts.view(), opts, &apply_ctx)
                exit_code = report.to_exit_code()
16. write_metrics(metric_file_path)  // atomic via tempfile+rename
17. print_report(stdout)
18. flock released (Drop)
19. exit(exit_code)
```

Основной кейс эксплуатации — bosun запускается под root внешним runr-таймером. В этом сценарии шаг 2 (bootstrap_dirs) однократно создаёт директории при первом прогоне; на последующих прогонах `create_dir_all` идемпотентен. Запуск под не-root в pod без предварительно созданных директорий — known failure mode с явным exit 4 и диагностикой.

### Exit codes

| Код | Значение |
|---|---|
| 0 | apply без ошибок (включая «всё уже стоит»); либо `--dry-run` показал нет drift; либо flock не получен — другая инстанция активна (WouldBlock) |
| 1 | apply начался, часть ресурсов применилась, дальше критическая ошибка ресурса |
| 2 | `--dry-run` обнаружил drift (есть pending changes) — для CI-gating |
| 3 | ошибка до apply: invalid manifest, `fail()` в Starlark, отсутствует ключ inv, не загрузился bundle, version mismatch |
| 4 | CLI/окружение: некорректные CLI-аргументы, отсутствует `--bundle`, неправильный путь, не удалось создать state/log/backup-dir, не удалось создать или открыть lock-file |

Семантика exit-code 0 на «lock taken» (WouldBlock) — намеренная: если runr-таймер запустил предыдущий прогон и он висит, новый прогон не должен генерировать алерт. Метрика прогона при этом не обновляется — старая метрика остаётся, что детектируется как «прогон не происходит» внешними средствами.

**Различение «lock taken» и «cannot access lock»** — критично. Первое (exit 0) — нормальная situational ситуация. Второе (exit 4) — конфигурационная проблема (нет прав, нет директории, /var/run read-only в pod), требует вмешательства SRE. На pod-нодах без root часто встречается именно второй случай.

### Метрика прогона

`bosun-cli` пишет файл `/var/lib/node_exporter/textfile_collector/bosun.prom` (путь конфигурируется через `--metric-file <path>`, default — этот). Формат — стандартный Prometheus textfile collector:

```
# HELP bosun_last_run_timestamp_seconds UTC timestamp of last completed run
# TYPE bosun_last_run_timestamp_seconds gauge
bosun_last_run_timestamp_seconds{version="0.1.0"} 1747567200

# HELP bosun_last_run_exit_code Exit code of last run
# TYPE bosun_last_run_exit_code gauge
bosun_last_run_exit_code 0

# HELP bosun_last_run_duration_seconds Duration of last run
# TYPE bosun_last_run_duration_seconds gauge
bosun_last_run_duration_seconds 12.34

# HELP bosun_resources_total Total resources in last run by outcome
# TYPE bosun_resources_total gauge
bosun_resources_total{outcome="changed"} 2
bosun_resources_total{outcome="unchanged"} 47
bosun_resources_total{outcome="failed"} 0

# HELP bosun_fact_state Состояние каждого факта в последнем прогоне
# TYPE bosun_fact_state gauge
# value: 0 = Known, 1 = Unknown, 2 = Stale
bosun_fact_state{fact="hostname"} 0
bosun_fact_state{fact="cpu_count"} 0
bosun_fact_state{fact="installed_packages"} 0
bosun_fact_state{fact="is_pod"} 0
```

Per-fact-name label — критично для observability на 60k нод: одно «installed_packages = Unknown» влияет на каждый `apt.package`, а «cpu_count = Unknown» — косметика. Агрегированный gauge без меток не позволит вычленить «у 10k нод dpkg сломан».

Файл пишется атомарно (tempfile + rename), чтобы collector не прочитал частичный. node_exporter подхватывает автоматически — никакой дополнительной интеграции с control plane в MVP не требуется.

## Bundle формат

### Layout

```
bundle/
├── bundle.toml
├── manifests/
│   └── main.star
├── defaults/
│   └── main.yaml
└── templates/
    └── <name>.j2
```

### bundle.toml

```toml
[bundle]
name           = "nginx-demo"
version        = "0.1.0"
requires_bosun = "^0.1"     # эквивалентно ">=0.1.0, <0.2.0" — pre-1.0 семантика cargo
entry          = "manifests/main.star"
```

Bosun читает свою версию через `env!("CARGO_PKG_VERSION")` и сравнивает с `requires_bosun` через `semver::VersionReq::matches`. Несовпадение — exit 3 до evaluate. Cargo-семантика `^0.1` корректна для pre-1.0 (любая минорная версия 0.1.x подходит, 0.2.x уже не подходит); рекомендуем использовать caret-формат, а не голый `">=0.1"`, который при выходе bosun 1.0 случайно сматчится с breaking релизом.

### `inv` в Starlark

- `inv.foo` — значение из merged `defaults/main.yaml + --inventory <override>.yaml`.
- `inv.nested.bar` — вложенный доступ.
- Отсутствует ключ → `fail("inv: key 'foo' not found in inventory")`. Никаких silent-None для опечаток.
- `inv.facts.<name>` — значение факта (см. правила трансляции `FactValue` выше).

Deep-merge inventory: для `Mapping` ключи сливаются, при коллизии override побеждает. Для `Sequence` и скаляров override полностью заменяет defaults.

**Семантика `null` в override:** значение `null` (или YAML `~`) в override — это «удалить ключ»; отсутствие ключа в override — «оставить defaults». Это явно фиксируется, потому что иначе путаница порождает баги на стыке override-слоёв.

### Пример `manifests/main.star`

```python
load("@bosun/builtins", "apt", "file", "template")

apt.package(
    name    = "nginx",
    version = inv.nginx_version,
)

file.content(
    path     = "/etc/nginx/nginx.conf",
    contents = template("nginx.conf.j2"),
    mode     = 0o644,
    owner    = "root",
    group    = "root",
)
```

`defaults/main.yaml`:

```yaml
nginx_version: 1.18.0-6ubuntu14.4
worker_processes: auto
```

`templates/nginx.conf.j2`:

```jinja
worker_processes {{ inv.worker_processes }};
events { worker_connections 768; }
http {
    server {
        listen 80 default_server;
        server_name {{ inv.facts.hostname }};
    }
}
```

## Plan / Apply / Dry-run

### `--dry-run` (один путь, та же реализация)

```
1. facts.collect_at_start()
2. snapshot = facts.snapshot()
3. registry = evaluator.evaluate(&snapshot)
4. view = facts.view()
5. order = registry.topological_order()
6. for id in order: primitive.plan(resource, &view, ...) → Diff (append to PlanReport)
7. print PlanReport. Никаких apply.
8. exit_code = 2 if PlanReport.has_drift else 0
```

`bosun apply --dry-run` и `bosun apply` ходят через одну функцию `plan_resources`; `apply` дополнительно делает execute.

### `apply`

```
1. facts.collect_at_start()
2. snapshot = facts.snapshot()
3. registry = evaluator.evaluate(&snapshot)
4. view = facts.view()
5. order = registry.topological_order()
6. for id in order:
   a. resource = registry.get(id)
   b. diff = primitive.plan(resource, &view, &plan_ctx)
   c. if diff == NoChange: log noop; continue
   d. ВАЖНО: `facts.mark_dirty_after_apply(&resource.kind)` вызывается СРАЗУ ДО `primitive.apply`. Логика: попытка apply могла модифицировать систему — apt-get install мог пройти, но post-install script упасть; dpkg-state транзитный. Мы не знаем точно, дошло ли изменение до диска, поэтому помечаем зависимые факты dirty независимо от результата. Это устраняет баг «changed=false при Err оставлял installed_packages устаревшим».
   e. report = primitive.apply(resource, &diff, &apply_ctx)
   f. on Err(PrimitiveError):
      - if continue_on_error: log error, push to ApplyReport.errors; continue
      - else: break (exit 1)
7. write_metrics(...)
8. print ApplyReport.
```

### Формат отчёта (text)

```
[facts]
  hostname=ubuntu-22 cpu_count=4 memory_mb=8192 is_pod=false
  installed_packages: 1247 entries (collected at start)

Plan:
  + apt.package nginx
      install 1.18.0-6ubuntu14.4 (currently not installed)
  + file.content /etc/nginx/nginx.conf
      mode 0o644 owner root group root
      sha256 ab12cd34... (file did not exist)

Summary: 2 changes pending (2 add, 0 update, 0 no-change).
```

Аннотации к ресурсам, чей plan может стать неточным из-за AfterApply-фактов:

```
  + file.content /etc/nginx/index.html
      [note] reads inv.facts.installed_packages; value may change after apt.package nginx applies
```

JSON-формат идентичен по содержимому, сериализуется через `serde`.

## Тестирование

### Уровень 1 — Unit (`cargo test`, без docker)

- `bosun-core`:
  - Registry topological sort на разных графах.
  - Цикл-детектор: `A → B → A` детектируется, сообщение содержит цепочку.
  - Bundle::load_dir на tempdir.
  - merge_inventory: defaults + override + null-семантика → ожидаемый merged Value.
  - Парсинг `bundle.toml`, semver-сравнение.
  - `ResourceKind::new` валидация формата (kebab + точки), отказ на «странном» вводе.
- `bosun-facts`:
  - dpkg parser на tempfile со специально подготовленными status-файлами; кейсы: чистый формат, многоархитектурные пакеты, описание с переносами строк, пакет с пустым `Version`, обрезанный файл (transient).
  - Парсер apt/lists на tempdir с файлами без `.Packages` суффикса.
  - Cgroup-reader v1/v2 на симулированных файлах в tempdir.
  - `/proc/meminfo` parser.
  - `is_pod` детект иерархия — каждый шаг отдельно.
  - Compare debian versions через `debversion` — проверочный набор из 10 классических кейсов.
  - Lazy refresh / dirty: после `mark_dirty_after_apply` следующий `view.get` пересобирает, при Unknown сохраняет предыдущее Known как Stale.
- `bosun-primitives`:
  - apt: десериализация `AptPackageSpec` из payload, `plan` против разных `FactValue` (mock `FactsSource`), проверка структуры command-line при apply (без exec). dpkg-lock probe мокается через trait `FileLockProbe`.
  - file: десериализация `FileContentSpec`, `plan` на tempfile с разными сценариями, `apply` с tempfile + проверка наличия бэкапа и его контента + проверка rotation после 6-го apply (остаётся 5). Symlink-rejection.
  - chown семантика: запрос совпадает / не совпадает / не root.
  - template: рендер с разными inv+facts, `Strict` undefined → ошибка с понятным сообщением, missing template файл → ошибка.
  - SensitivePayload: проверка что `Debug`/`Display` маскируют значение.
- `bosun-cli`:
  - clap-парсинг разных флагов.
  - Mapping `Result<..., bosun_core::Error>` в exit-код.
  - flock: два процесса бьются за lock, второй получает «занят» (тест через `tempfile::NamedTempFile` для lock-path).

### Уровень 2 — Golden (для template, без docker)

```
crates/bosun-primitives/tests/golden/
├── basic_render/
│   ├── template.j2
│   ├── inventory.yaml
│   ├── facts.json
│   └── expected.txt
├── with_facts/
│   └── ...
└── missing_inv_key/
    └── error_message.txt
```

Тест читает входные файлы, рендерит, сравнивает с `expected.txt`. При `UPDATE_GOLDEN=1` — регенерирует `expected.txt`.

### Уровень 3 — BDD/feature в Docker (`cucumber-rs`)

```
crates/bosun-cli/tests/
├── features/
│   ├── apt_package.feature
│   ├── file_content.feature
│   ├── template.feature
│   ├── facts.feature
│   ├── lock_handling.feature
│   ├── dry_run.feature
│   └── idempotency.feature
├── steps/
│   ├── mod.rs
│   ├── docker.rs
│   ├── bundle.rs
│   └── assertions.rs
└── data/
    └── bundles/
```

Базовый образ `bosun-test-base` собирается из `docker/test-base.Dockerfile`:

```dockerfile
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
```

Базовый образ собирается один раз в CI, кэшируется. В каждом сценарии — свежий контейнер.

Сценарий BDD-теста:
1. Поднять контейнер из `bosun-test-base:latest` через `testcontainers-rs`.
2. `docker cp` `target/release/bosun` и нужный bundle/inventory внутрь.
3. `docker exec` запускает `bosun apply --bundle /work/bundle --inventory /work/inv.yaml`.
4. Assertions через дополнительные `docker exec`: `dpkg-query -W`, `cat /etc/...`, `test -e ...`, проверка exit-кода bosun.
5. Контейнер уничтожается.

Параллелизация: один контейнер на feature-файл (не на scenario). Между scenario'ами state не очищается — это **намеренно**, чтобы тестировать idempotency: второй scenario «install nginx», после первого «install nginx» — ожидаемый exit 0, no-change. Каждый scenario выбирает: `Given a fresh container` (uniqe contenair) или `Given the existing container` (continued state).

Обязательные scenario'и MVP:

- `apt_package.feature`:
  - Fresh install nginx (Add).
  - Already installed (idempotency — exit 0, no-change).
  - Different version requested (Update).
  - Package not in apt-lists, fallback triggers `apt-get update` (retry-loop), затем install.
  - dpkg-lock present → `PrimitiveError::DpkgLocked` → exit 1, без exec apt-get.
  - dpkg в half-configured state (предварительно прерванный `apt-get install postgresql` через SIGKILL) → bosun детектирует «dpkg was interrupted», выполняет `dpkg --configure -a`, делает retry install, exit 0.
  - apt-get update имитирует транзиентный 503 (через mock-mirror) первые 2 попытки, третья успешна → bosun повторяет, фиксируется в логах, exit 0.

- `file_content.feature`:
  - New file write.
  - Same content, same mode → no-change.
  - Different content → update + backup создан.
  - Different mode only → update + chmod.
  - Owner not matching, running as root → chown.
  - Owner not matching, running as non-root → exit 1, `ChownNotPermitted`.
  - Symlink at path → exit 1, `InvalidTarget`.
  - 6 sequential updates → последний backup в /var/backups/bosun/..., предыдущие 5 сохранены, более старые удалены.

- `template.feature`:
  - Basic render with inv vars.
  - Render with inv.facts.hostname.
  - `{{ inv.missing }}` → Strict-fail with clear message.

- `facts.feature`:
  - cpu_count outside pod = num_cpus.
  - cpu_count inside pod with cgroup v2 limit = cgroup quota.
  - is_pod detection per стадия.
  - dpkg unreadable → installed_packages = Unknown, apt.package fallback срабатывает.

- `lock_handling.feature`:
  - Second bosun while first holds flock → exit 0, info message, no metric write.

- `dry_run.feature`:
  - --dry-run shows plan, doesn't change system, exit code 2 if drift.
  - --dry-run on clean system, exit 0.

- `idempotency.feature`:
  - Run apply twice in a row, second run reports 0 changes, exit 0 (для всех трёх примитивов).

Запуск:
- `cargo test` — unit + golden (быстро).
- `cargo test -- --ignored` или `cargo test --features bdd` — BDD (требует docker).
- В CI два независимых job: unit + BDD. BDD после unit.

Никаких моков системы. Никаких подмен apt-get, dpkg или ФС. Тесты честно прогоняются в реальном Docker.

## Error handling

### Структура ошибок

Все pub API возвращают `Result<T, E>`. Per-crate Error через `thiserror`. `bosun-cli` агрегирует через `anyhow::Result` в main и маппит в exit-код.

Все public enum'ы помечены `#[non_exhaustive]`:

```rust
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum PrimitiveError {
    #[error("io error in {context}")]
    Io { context: String, #[source] source: std::io::Error },

    #[error("invalid resource payload: {0}")]
    InvalidPayload(String),

    #[error("external command failed: {reason}")]
    Exec { reason: String, exit: Option<i32>, stderr_excerpt: String },

    #[error("dpkg locked, holder pid={holder_pid:?}")]
    DpkgLocked { holder_pid: Option<i32> },

    #[error("chown not permitted: requested {requested}, current {actual}")]
    ChownNotPermitted { requested: String, actual: String },

    #[error("target is symlink, refusing to write through it")]
    InvalidTarget,

    #[error("operation cancelled by deadline or signal")]
    Cancelled,

    #[error("panicked in {context}: {message}")]
    Panic { context: String, message: String },
}
```

Аналогичная структура для `EvalError`, `RegistryError`, `BundleError`, `FactCollectError`, `CallArgsError`. Все `#[non_exhaustive]`.

### Запрет паник

- `panic!`, `.unwrap()`, `.expect("...")` запрещены в production-path. Линт `#![deny(clippy::unwrap_used, clippy::expect_used)]` на каждый библиотечный крейт. В `mod tests` — `#[allow(...)]`.
- `catch_unwind` ставится на границах подсистем:
  - Каждый `Fact::collect` — паника → `FactValue::Unknown { reason: "panic: {msg}" }`.
  - Каждый `Primitive::apply` — паника → `PrimitiveError::Panic { context, message }`.
  - Starlark `fail("msg")` → `EvalError::ManifestFail { msg, location }` с указанием места.

## Logging и observability

- Крейт `tracing` + `tracing-subscriber`.
- `text`-формат логов в stderr по умолчанию.
- `json`-формат через `--log-format json` для CI и мониторинга.
- Spans для фаз:
  - `facts/collect`
  - `evaluate`
  - `plan`
  - `apply{kind=apt.package, id=apt.package:nginx}` — per-resource span.
- Уровни:
  - `info`: start/end фаз, change reports, итоговые counts, lock-conflict skip.
  - `debug`: lookups фактов, command-lines exec (без секретов), sha256 файлов.
  - `warn`: fallback-пути (`Unknown` факт, retriable apt-get update попытка, transient dpkg-status).
  - `info` (не warn): `Stale` факты — это нормальное состояние после AfterApply.
  - `error`: ошибки primitive.apply, manifest `fail()`.
- `tracing-subscriber` инициализируется только в `bosun-cli/main.rs`. Библиотечные крейты не настраивают subscriber.
- Метрика прогона — см. секцию «bosun-cli / Метрика прогона».
- Длинные stderr внешних exec'ов пишутся в `/var/log/bosun/<step>-last-error.log` (один файл на step, перезаписывается). Это покрывает post-mortem на нодах без долгой работы с tracing.

## Внешние зависимости

Production:

- `starlark` — последний стабильный 0.x; закрепить минорно (`starlark = "~0.13"` — эквивалент `>=0.13.0, <0.14.0`). Exact pin (`=0.13.0`) блокирует security-фиксы в патчах.
- `minijinja` — для template.
- `clap` (derive) — CLI.
- `serde`, `serde_json` — структуры.
- **`serde_norway`** — drop-in fork `serde_yaml` (который deprecated с конца 2024). Используем эту библиотеку для всех YAML-парсингов.
- `thiserror` — error definitions в библиотеках.
- `anyhow` — в `bosun-cli/main.rs` для агрегации.
- `tracing`, `tracing-subscriber` — логирование.
- `procfs` — `/proc/*` чтение. Закрепить минорно (`procfs = "=0.16"`).
- `sha2` — content diff.
- `semver` — version pin.
- `tempfile` — atomic write file.content.
- `tokio-util` — `CancellationToken` для `ApplyCtx`.
- `fs4` — advisory flock (cross-platform). Активный fork устаревшего `fs2` (последний релиз 0.4.3 в 2019); `fs4` поддерживается, имеет async-варианты.
- `debversion` — debian version comparison. Закрепить минорно (как `procfs`).
- `num_cpus` — `cpu_count` фолбэк не в pod.

Dev:

- `cucumber` — BDD.
- `testcontainers` — управление Docker из тестов.
- `tempfile` (уже выше).
- `pretty_assertions` — для понятных diff'ов в тестах.

Удалены из изначального списка:
- `nix` — `gethostname` берётся через `/proc/sys/kernel/hostname` (через `procfs`); одна строка кода против ~1 MB бинаря.
- `wait-timeout` — таймауты делаем через `tokio::process::Command` под однопоточным runtime (через `tokio` minimal-features) или через std + threading. Решается при имплементации, в open questions.
- `serde_yaml` — deprecated, заменён на `serde_norway`.

## Open questions

Вопросы для следующих итераций; в MVP фиксируем простое поведение.

- **Async vs sync exec в примитивах.** Сейчас sync. Если потребуется отзывчивая отмена долгого `apt-get install` — нужен async. Решение: tokio current-thread runtime в `bosun-cli/main.rs`, sync façade в примитивах. Уточнить при имплементации.
- **`reload_on` vs `depends_on` различение в топсорте.** В MVP оба ребра треатятся одинаково. В будущем (когда придут сервисы) `reload_on` может быть допустим в цикле, `depends_on` — никогда.
- **Persistent fact cache между прогонами.** На диск в `/var/cache/bosun/facts.json` с инвалидацией по mtime. Не в MVP, открыт.
- **xattr / SELinux context в file.content и его бэкапе.** Сейчас не сохраняются. Не критично для nginx в Docker, но для RHEL-нод с SELinux в проде — обязательно.
- **Per-resource `timeout_sec` в `apt.package(...)`** — в MVP default 600, аргумент частично уже в `AptPackageSpec`; зафиксировать парсинг и валидацию при имплементации.
- **`bundle.toml` `inventory_schema`** — пока не делаем, но это путь к ранней валидации опечаток.
- **Signing bundle, distribution через bosun-server** — следующая итерация.
- **PackageProvider trait** — отделять `apt.package` от будущих `dnf.package`, `pacman.package`. Сейчас YAGNI, но архитектурно мы это допускаем (Primitive trait уже общий, `ResourceKind` через `Arc<str>` поддерживает runtime-регистрацию).
- **Notification grouping (Chef-style)** — когда придёт `runr.service`, нужно дедуплицировать рестарты, если несколько `file.content` notify-ят один сервис.
- **Splay/jitter для apt-update на 60k нод** — координируется не в bosun, а в runr-timer'е снаружи. Зафиксировано как ожидание от infrastructure-стека.
- **Optimization: typed Spec cache в `Resource`.** Сейчас `Primitive::plan` и `Primitive::apply` каждый раз делают `serde_json::from_value::<Spec>(payload.clone())`. На bundle с 100+ ресурсами в каждом прогоне это лишняя CPU. Подход: после `build_payload` ядро кэширует `Arc<dyn Any>` с десериализованным `Spec` рядом с `payload`. Не критично для MVP (1-3 ресурса), фиксируется как путь оптимизации.
- **`topological_order` сигнатура.** Сейчас возвращает `Vec<ResourceId>`, apply делает per-step `registry.get(&id)`. Дешевле было бы `Vec<usize>` (индекс в `resources`) или `Vec<&Resource>`. Не блокер MVP, открытый рефакторинг.
- **Send + Sync на Primitive.** Сейчас trait требует `Send + Sync`. MVP делает sequential apply, реальная необходимость в Sync — заранее. Оставлено для будущего concurrency; если concurrency окажется не нужна — можно ослабить.
- **`Resource: Clone`?** Сейчас не помечен. `ApplyReport` / `PlanReport` сериализуют ресурсы через `serde` (borrow). Если потребуется keep копии ресурсов в отчётах после Registry drop — добавим `Clone`. Пока не критично.
- **`Diff::Add.payload` семантика.** В MVP — копия `Resource.payload` (по аналогии с Chef'овским `:install` action). Альтернатива: payload в `Diff` — это desired-state, а в `Resource` — declared-state; они могут различаться для composite-примитивов (template-merge). Не возникает в MVP с тремя простыми примитивами.
- **`serde_norway` как замена `serde_yaml`.** Fork сравнительно молодой (2024+). Если возникнут проблемы — fallback на `yaml-rust2` через свой Deserializer. Держать в плане.
- **`tokio-util::CancellationToken` тянет tokio.** Для CLI это норма (мы и так используем tokio для процессов). Если потребуется `bosun-core` без tokio — заменим на собственный `Cancel { flag: AtomicBool }`.
- **Plan-аннотации «value may change after X applies»** — в MVP это эвристика: для каждого ресурса R, если в его `plan` обращение к factу F было прочитано (зарегистрировано через wrapper `FactsView` с tracking), и F помечен `AfterApply.triggers` каким-то предыдущим resource'ом в order — добавляем аннотацию. Если tracking сделать сложно — выводим аннотацию для всех ресурсов после первого, читающих AfterApply-факт.

## Связь с существующими концептами

| Концепт wiki | Как используется |
|---|---|
| `concepts/bosun-overview.md` | bosun-client — реализация описанного агента, без интеграции с control plane в MVP |
| `concepts/bundle-architecture.md` | bundle-layout соответствует описанному; в MVP только manifests/defaults/templates, без tasks |
| `concepts/starlark-dsl.md` | стиль манифестов через side-effect-вызовы; `inv.*` read-only; реактивность через handles; load c префиксом `@bosun/builtins` вместо `//lib` |
| `concepts/facts-module.md` | failure-mode `Known/Unknown/Stale`; категории Static/Slow/Live/Discovery; `inv.facts.*`; dirty-tracking как реализация AfterApply |
| `concepts/manifest-vs-task.md` | MVP реализует только manifest-режим |
| `concepts/monotonicity-rule.md` | в MVP нет live-фактов, правило не применяется; helpers (`apt.package_unless`) отложены |
| `concepts/self-upgrade.md` | служит источником подхода к парсингу dpkg-статуса (с фиксами багов); архитектурно self-upgrade не наследуется |
| `concepts/chiit-go-dsl.md` | Notify-pattern (changed/error) частично переходит в `ChangeReport` (без `error`); whyrun через context НЕ переходит — заменён `apply --dry-run` через ту же функцию |

## Следующие шаги после MVP

(вне scope этого design, но полезно зафиксировать направление)

1. `runr.service` и `runr.timer` примитивы — handle-связи начнут работать практически.
2. Интеграция с control plane (warden discovery, ECDSA-подпись, bundle distribution, secrets via Vault).
3. Task-режим: императивные действия с `pg_sql.exec`, `command.run_now`, `http.get`.
4. Live-факты и monotonicity-helpers.
5. Discovery-факты (`postgres.version` после установки nix-package).
6. Persistent fact cache между прогонами.
7. PackageProvider trait для расширения на dnf/pacman/rpm.
8. xattr/SELinux в file.content для RHEL-нод.

## История ревизий

- **rev 1 (2026-05-18, первый draft)** — собран по итогам brainstorming-сессии.
- **rev 3 (2026-05-18, текущий)** — применены замечания повторного DevOps-ревью и Rust-архитектора:
  - **apt.package apply** переписан на модель с использованием встроенных `APT::Acquire::Retries=3` и `DPkg::Lock::Timeout=30` (стандартная практика chef-cookbooks/apt, k8s, autopkgtest); явный анализ exit-code+stderr; `dpkg --configure -a` + один retry при «dpkg was interrupted» (chiit-style); apt-get update fallback с 3 попытками exp backoff при candidate-miss. Источники: Chef apt_package docs, sinjakli blog DPkg::Lock::Timeout, GitHub chef-cookbooks/apt#164, Ansible apt idempotency forum.
  - **mark_dirty_after_apply вызывается перед apply** (когда diff != NoChange), не после report.changed. Закрывает баг «при failed apply installed_packages остаётся устаревшим».
  - **ResourceKind**: `Arc<str>` вместо `&'static str`, два конструктора (`from_static` для built-ins, `try_new` для runtime PackageProvider). Не блокирует будущую динамическую регистрацию.
  - **FactsCollector::cache**: явный `RefCell` для interior mutability — `FactsSource::get(&self, ...)` корректен и trait-object-friendly. `FactsView::new(&FactsCollector)` (immutable borrow) вместо `&mut`.
  - **ApplyCtx.sensitive: Arc<SensitiveStore>** — side-channel для `file.content.contents` теперь явно в API, не «где-то в ядре». PlanCtx/ApplyCtx без lifetime-параметра (поля Clone-дешёвые).
  - **bootstrap_dirs шаг** в начале CLI flow: mkdir_p для state/log/backup directories перед flock. Различение «lock taken» (WouldBlock → exit 0) и «cannot access lock» (Io error → exit 4).
  - **Глобальный deadline default 600 секунд** (от 240) — для тяжёлых пакетов (kernel-headers, postgresql) на медленных зеркалах.
  - **Метрика прогона**: `bosun_fact_state{fact="..."}` per-fact-name gauge вместо одного aggregated counter. Позволяет агрегировать «installed_packages = Unknown на 10k нод» отдельно от «cpu_count = Unknown».
  - **bundle.toml requires_bosun = "^0.1"** в примере, чтобы случайно не сматчиться с 1.0.0.
  - **fs2 → fs4** (fs2 deprecated с 2019). starlark с минорным pin `~0.13`, не exact. debversion закреплён минорно как procfs.
  - **catch_unwind + AssertUnwindSafe** явно зафиксирован в spec — Box<dyn Fact> и Box<dyn Primitive> не UnwindSafe по умолчанию.
  - **file.content re-check переформулирован** как re-plan: устраняет «plan был давно, файл уже другой», но не закрывает TOCTOU полностью (атомарность даёт rename внутри одной FS).
  - **Plan-аннотации AfterApply-фактов** — описаны как эвристика с tracking через wrapper FactsView, либо упрощённый вариант «для всех ресурсов после первого AfterApply-trigger».
  - **Open questions расширены**: typed Spec cache, topological_order сигнатура, Send+Sync обоснование, Resource: Clone, Diff::Add.payload семантика, serde_norway fallback, tokio dependency на CancellationToken.

- **rev 2 (2026-05-18)** — применены замечания DevOps-ревью и Rust-архитектора:
  - apt.package: dpkg-lock probe, retry для `apt-get update` (3 попытки, exp backoff), захват stderr в файл и `PrimitiveError::Exec.stderr_excerpt`, `PrimitiveError::DpkgLocked`.
  - file.content: backup только при Update + rotation 5 копий, TOCTOU re-check перед rename, symlink rejection, chown семантика (skip-if-equal, error-if-non-root).
  - CLI: `bosun plan` команда удалена (только `apply --dry-run`); exit code 2 для drift; flock на `/var/run/bosun.lock`; глобальный deadline 240 сек; метрика прогона в node_exporter textfile.
  - Архитектура trait Primitive: `build_payload` вместо `build_resource` (ядро строит ResourceId по `identity_keys`); `PlanCtx`/`ApplyCtx` с `deadline` и `CancellationToken`; примитивы используют приватный Spec-тип через serde.
  - Resource: `ResourceKind` newtype, `spec_version` для будущей миграции.
  - ChangeReport: убрано поле `error`, только Result.
  - Все public enum'ы `#[non_exhaustive]`.
  - Magic `//lib` заменён на `@bosun/builtins`.
  - FactsCollector: `FactsSnapshot` (immutable для evaluation) + `FactsView<'_>` (mutable для plan/apply); lazy dirty-refresh вместо batch.
  - Orchestrator разделён на `Evaluator` и `Orchestrator`.
  - minijinja `UndefinedBehavior::Strict`.
  - inventory deep-merge: `null` в override = «удалить ключ».
  - `Sensitive<String>` для `file.content.contents` — в payload хранится sha256+size, тело отдельным side-channel.
  - serde_yaml (deprecated с 2024) заменён на `serde_norway`.
  - procfs закреплён на `=0.16`.
  - nix удалён (gethostname через procfs).
  - debversion фиксирован как зависимость (не своя реализация).
  - BDD-сценарии расширены: dpkg-lock, idempotency, dry-run drift, lock-conflict skip, backup rotation, chown not-permitted, symlink rejection.
