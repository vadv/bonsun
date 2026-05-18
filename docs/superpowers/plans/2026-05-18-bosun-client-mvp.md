# bosun-client MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Реализовать первую итерацию bosun-client — Rust-агента, применяющего bundle на Starlark, способного в свежем Docker по bundle поставить пакет, развернуть конфиг.

**Architecture:** Rust workspace из 4 крейтов (bosun-core / bosun-facts / bosun-primitives / bosun-cli). Starlark-evaluator через крейт `starlark`. Type-erased payload через `serde_json::Value` с приватным `Spec` через `serde` внутри примитива. Подсистема фактов с failure-mode `Known/Unknown/Stale` и lazy dirty-refresh. BDD-тесты в Docker через `testcontainers` + `cucumber`. Каждая фаза заканчивается рабочим бинарём, который можно прогнать в свежем контейнере.

**Tech Stack:** Rust stable (1.84+), `starlark = "~0.13"`, `minijinja`, `procfs = "=0.16"`, `debversion`, `clap` (derive), `tracing`/`tracing-subscriber`, `fs4`, `cucumber` (dev), `testcontainers` (dev), `serde_norway` (вместо deprecated `serde_yaml`), `tokio-util` (CancellationToken), `tempfile`, `sha2`, `semver`, `num_cpus`, `anyhow`.

**Source spec:** `docs/superpowers/specs/2026-05-18-bosun-client-mvp-design.md`

---

## File Structure

Все пути относительно корня workspace `bosun-client/` (создаётся в Task 1 как новая директория в `/home/vadv/Projects/bosun/bosun-client/`).

```
bosun-client/
├── Cargo.toml                                # [workspace] root, общие dependencies
├── .gitignore                                # /target, *.swp etc.
├── README.md                                 # короткое описание + ссылка на spec
├── rustfmt.toml                              # стиль формата
├── crates/
│   ├── bosun-core/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs                        # модули pub use re-exports
│   │   │   ├── error.rs                      # bosun-core::Error, типы ошибок верхнего уровня
│   │   │   ├── resource.rs                   # ResourceKind, ResourceId, Resource, Handle
│   │   │   ├── diff.rs                       # Diff, ChangeReport
│   │   │   ├── primitive.rs                  # trait Primitive, PlanCtx, ApplyCtx, PrimitiveError
│   │   │   ├── facts.rs                      # FactValue, FactCategory, RefreshPolicy, trait FactsSource
│   │   │   ├── inventory.rs                  # trait InventorySource
│   │   │   ├── registry.rs                   # Registry + topological_order
│   │   │   ├── sensitive.rs                  # SensitiveStore, SensitivePayload
│   │   │   ├── call_args.rs                  # CallArgs (Starlark-glue helper)
│   │   │   ├── bundle.rs                     # Bundle::load_dir, merge_inventory
│   │   │   ├── starlark_glue/                # Starlark evaluator integration
│   │   │   │   ├── mod.rs
│   │   │   │   ├── globals.rs                # apt/file/template native objects
│   │   │   │   ├── load_resolver.rs          # "@bosun/builtins" resolver
│   │   │   │   └── inv_object.rs             # inv.* / inv.facts.* read-only object
│   │   │   ├── evaluator.rs                  # Evaluator: Starlark → Registry
│   │   │   └── orchestrator.rs               # Orchestrator: plan_only + apply with dirty-tracking
│   │   └── tests/                            # public-API integration tests
│   │       └── registry_topo.rs
│   ├── bosun-facts/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── collector.rs                  # FactsCollector + FactsSnapshot + FactsView + RefCell cache
│   │   │   ├── catalog.rs                    # with_default_collectors() factory
│   │   │   ├── hostname.rs
│   │   │   ├── cpu_count.rs                  # cgroup-aware
│   │   │   ├── memory_mb.rs                  # cgroup-aware
│   │   │   ├── init_system.rs
│   │   │   ├── is_pod.rs                     # hierarchy detection
│   │   │   ├── installed_packages.rs         # dpkg + apt/lists parser
│   │   │   └── cgroup.rs                     # shared v1/v2 detection
│   │   └── tests/
│   │       └── installed_packages_integration.rs
│   ├── bosun-primitives/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── file_content/
│   │   │   │   ├── mod.rs                    # FilePrimitive impl Primitive
│   │   │   │   ├── spec.rs                   # FileContentSpec
│   │   │   │   ├── plan.rs
│   │   │   │   ├── apply.rs
│   │   │   │   ├── backup.rs                 # rotation 5 копий
│   │   │   │   └── chown.rs                  # семантика skip-if-equal / error-if-non-root
│   │   │   ├── apt_package/
│   │   │   │   ├── mod.rs                    # AptPrimitive impl Primitive
│   │   │   │   ├── spec.rs                   # AptPackageSpec
│   │   │   │   ├── plan.rs
│   │   │   │   ├── apply.rs
│   │   │   │   ├── lock_probe.rs             # fcntl lock на /var/lib/dpkg/lock-frontend
│   │   │   │   ├── exec.rs                   # spawn apt-get, capture stderr, retry-loop
│   │   │   │   └── recovery.rs               # dpkg --configure -a + retry
│   │   │   └── template/
│   │   │       ├── mod.rs                    # template(path) function
│   │   │       └── render.rs                 # minijinja Strict integration
│   │   └── tests/
│   │       └── golden/                       # для template
│   │           ├── basic_render/
│   │           ├── with_facts/
│   │           └── missing_inv_key/
│   ├── bosun-cli/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── main.rs                       # entry point
│   │   │   ├── args.rs                       # clap derive
│   │   │   ├── bootstrap.rs                  # mkdir_p, flock
│   │   │   ├── logging.rs                    # tracing-subscriber init
│   │   │   ├── exit_code.rs                  # mapping Result<_, Error> → i32
│   │   │   └── metric.rs                     # node_exporter textfile write
│   │   └── tests/                            # BDD через cucumber
│   │       ├── bdd.rs                        # cucumber runner
│   │       ├── steps/
│   │       │   ├── mod.rs
│   │       │   ├── docker.rs
│   │       │   ├── bundle.rs
│   │       │   └── assertions.rs
│   │       ├── features/
│   │       │   ├── apt_package.feature
│   │       │   ├── file_content.feature
│   │       │   ├── template.feature
│   │       │   ├── facts.feature
│   │       │   ├── lock_handling.feature
│   │       │   ├── dry_run.feature
│   │       │   ├── idempotency.feature
│   │       │   └── dpkg_interrupted.feature
│   │       └── data/
│   │           └── bundles/
├── docker/
│   └── test-base.Dockerfile                  # FROM debian:bookworm-slim
└── examples/
    └── nginx-demo/
        ├── bundle/
        │   ├── bundle.toml
        │   ├── manifests/main.star
        │   ├── defaults/main.yaml
        │   └── templates/nginx.conf.j2
        └── README.md
```

Принципы декомпозиции на файлы:
- Один файл = одна публичная ответственность (Resource, Diff, Primitive trait, и т.д.).
- Внутри каждого примитива выделены `spec.rs` / `plan.rs` / `apply.rs` — три фазы жизни ресурса.
- Сложная логика (lock probe, backup rotation, chown, recovery) вынесена в отдельные файлы — каждый отдельно тестируется.
- Starlark glue изолирован в подмодуле `starlark_glue/` — он зависит только от `bosun-core` примитивов.

---

## Phase 1: Workspace bootstrap

Цель фазы: `cargo build` зелёный, `bosun version` печатает версию. Walking skeleton без логики.

### Task 1: Создать Cargo workspace и пустые крейты

**Files:**
- Create: `bosun-client/Cargo.toml`
- Create: `bosun-client/.gitignore`
- Create: `bosun-client/rustfmt.toml`
- Create: `bosun-client/crates/bosun-core/Cargo.toml`
- Create: `bosun-client/crates/bosun-core/src/lib.rs`
- Create: `bosun-client/crates/bosun-facts/Cargo.toml`
- Create: `bosun-client/crates/bosun-facts/src/lib.rs`
- Create: `bosun-client/crates/bosun-primitives/Cargo.toml`
- Create: `bosun-client/crates/bosun-primitives/src/lib.rs`
- Create: `bosun-client/crates/bosun-cli/Cargo.toml`
- Create: `bosun-client/crates/bosun-cli/src/main.rs`

- [ ] **Step 1: Создать workspace Cargo.toml**

`bosun-client/Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/bosun-core",
    "crates/bosun-facts",
    "crates/bosun-primitives",
    "crates/bosun-cli",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.84"
license = "Proprietary"
publish = false

[workspace.lints.rust]
unsafe_code = "deny"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_norway = "0.9"
thiserror = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }
semver = "1"
sha2 = "0.10"
tempfile = "3"
clap = { version = "4", features = ["derive"] }
procfs = "=0.16"
num_cpus = "1"
tokio-util = "0.7"
debversion = "0.4"
starlark = "~0.13"
minijinja = "2"
fs4 = "0.13"
chrono = { version = "0.4", features = ["clock"] }
```

- [ ] **Step 2: Создать .gitignore**

`bosun-client/.gitignore`:

```
/target
Cargo.lock.bak
*.swp
*.swo
*~
.DS_Store
*.bak
*.orig
```

- [ ] **Step 3: Создать rustfmt.toml**

`bosun-client/rustfmt.toml`:

```
edition = "2021"
max_width = 100
imports_granularity = "Module"
group_imports = "StdExternalCrate"
```

- [ ] **Step 4: Создать bosun-core с минимальным lib.rs**

`bosun-client/crates/bosun-core/Cargo.toml`:

```toml
[package]
name = "bosun-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[lints]
workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
serde_norway = { workspace = true }
thiserror = { workspace = true }
semver = { workspace = true }
tracing = { workspace = true }
tokio-util = { workspace = true }
chrono = { workspace = true }
starlark = { workspace = true }
```

`bosun-client/crates/bosun-core/src/lib.rs`:

```rust
//! bosun-core — контракты и evaluator для bosun-client.
//!
//! Этот крейт ничего не знает про конкретные примитивы (apt/file/template)
//! и про конкретные факты. Его задача — определить контракты и реализовать
//! Starlark-evaluator, Registry, plan/apply-оркестратор.
```

- [ ] **Step 5: Создать bosun-facts, bosun-primitives с минимальным lib.rs**

`bosun-client/crates/bosun-facts/Cargo.toml`:

```toml
[package]
name = "bosun-facts"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[lints]
workspace = true

[dependencies]
bosun-core = { path = "../bosun-core" }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
procfs = { workspace = true }
num_cpus = { workspace = true }
debversion = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

`bosun-client/crates/bosun-facts/src/lib.rs`:

```rust
//! bosun-facts — подсистема сбора фактов о ноде.
```

`bosun-client/crates/bosun-primitives/Cargo.toml`:

```toml
[package]
name = "bosun-primitives"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[lints]
workspace = true

[dependencies]
bosun-core = { path = "../bosun-core" }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
sha2 = { workspace = true }
tempfile = { workspace = true }
minijinja = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

`bosun-client/crates/bosun-primitives/src/lib.rs`:

```rust
//! bosun-primitives — реализации trait Primitive: apt.package, file.content, template().
```

- [ ] **Step 6: Создать bosun-cli с минимальным main.rs**

`bosun-client/crates/bosun-cli/Cargo.toml`:

```toml
[package]
name = "bosun-cli"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true

[[bin]]
name = "bosun"
path = "src/main.rs"

[lints]
workspace = true

[dependencies]
bosun-core = { path = "../bosun-core" }
bosun-facts = { path = "../bosun-facts" }
bosun-primitives = { path = "../bosun-primitives" }
clap = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
anyhow = { workspace = true }
fs4 = { workspace = true }
serde_json = { workspace = true }
chrono = { workspace = true }
tokio-util = { workspace = true }
```

`bosun-client/crates/bosun-cli/src/main.rs`:

```rust
fn main() {
    println!("bosun version {}", env!("CARGO_PKG_VERSION"));
}
```

- [ ] **Step 7: Прогнать `cargo build` и убедиться что собирается**

Run: `cd bosun-client && cargo build`
Expected: компиляция всех 4 крейтов завершается без ошибок.

- [ ] **Step 8: Commit**

```bash
cd bosun-client
git init -b main
git add Cargo.toml .gitignore rustfmt.toml crates/
git commit -m "Bootstrap bosun-client workspace

Что требовалось: создать каркас Rust-проекта bosun-client как cargo workspace из четырёх крейтов согласно spec.
Суть: workspace.toml перечисляет общие dependencies через [workspace.dependencies]; четыре крейта (bosun-core, bosun-facts, bosun-primitives, bosun-cli) с минимальным lib.rs и main.rs; lint-политика workspace-level (unsafe_code=deny, unwrap_used=deny, expect_used=deny, panic=deny); rustfmt-конфиг проекта."
```

---

### Task 2: Минимальный CLI с `bosun version` через clap

**Files:**
- Modify: `bosun-client/crates/bosun-cli/src/main.rs`
- Create: `bosun-client/crates/bosun-cli/src/args.rs`
- Test: `bosun-client/crates/bosun-cli/tests/version.rs`

- [ ] **Step 1: Написать integration-тест для `bosun version`**

`bosun-client/crates/bosun-cli/tests/version.rs`:

```rust
use std::process::Command;

#[test]
fn version_prints_pkg_version() {
    let bin = env!("CARGO_BIN_EXE_bosun");
    let output = Command::new(bin)
        .arg("version")
        .output()
        .expect("binary runs");

    assert!(output.status.success(), "exit code: {:?}", output.status);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "stdout: {stdout}");
}
```

- [ ] **Step 2: Прогнать тест — ожидаем FAIL**

Run: `cargo test -p bosun-cli --test version`
Expected: FAIL — текущий main печатает версию для любого invocation, но clap ещё не интегрирован.

- [ ] **Step 3: Создать args.rs c clap-derive**

`bosun-client/crates/bosun-cli/src/args.rs`:

```rust
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "bosun", version, about = "bosun-client agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print version and exit.
    Version,
    /// Apply a bundle to the local system.
    Apply(ApplyArgs),
}

#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    #[arg(long)]
    pub bundle: std::path::PathBuf,
    #[arg(long)]
    pub inventory: Option<std::path::PathBuf>,
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}
```

- [ ] **Step 4: Переписать main.rs на использование Cli**

`bosun-client/crates/bosun-cli/src/main.rs`:

```rust
mod args;

use clap::Parser;

fn main() {
    let cli = args::Cli::parse();
    match cli.command {
        args::Command::Version => {
            println!("bosun version {}", env!("CARGO_PKG_VERSION"));
        }
        args::Command::Apply(_) => {
            eprintln!("apply: not yet implemented");
            std::process::exit(2);
        }
    }
}
```

- [ ] **Step 5: Прогнать тест — ожидаем PASS**

Run: `cargo test -p bosun-cli --test version`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/bosun-cli/
git commit -m "Каркас CLI с subcommand version

Что требовалось: добавить минимальный CLI с подкомандами version и apply (apply пока заглушка), чтобы каркас работал и был покрыт integration-тестом.
Суть: clap derive в args.rs описывает структуру Cli и enum Command; main.rs парсит args и роутит; apply возвращает exit 2 как 'eval/manifest error' до полной реализации; добавлен integration-тест tests/version.rs через CARGO_BIN_EXE_bosun."
```

---

## Phase 2: bosun-core foundations

Цель фазы: все базовые типы (ResourceKind, Resource, Diff, FactValue, errors) определены и покрыты unit-тестами. После фазы — `cargo test -p bosun-core` зелёный.

### Task 3: ResourceKind newtype с валидацией

**Files:**
- Create: `bosun-client/crates/bosun-core/src/resource.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: Написать тест на ResourceKind в `resource.rs`**

`bosun-client/crates/bosun-core/src/resource.rs`:

```rust
use std::sync::Arc;

/// Newtype для типа ресурса (например "apt.package", "file.content").
/// Хранится как Arc<str> для дешёвого clone и поддержки runtime-регистрации.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKind(Arc<str>);

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResourceKindError {
    #[error("resource kind must be non-empty")]
    Empty,
    #[error("resource kind '{0}' contains invalid character; expected kebab-case dotted (e.g. apt.package)")]
    InvalidChar(String),
}

impl ResourceKind {
    /// Для built-in примитивов — статика, формат гарантируется автором.
    pub fn from_static(s: &'static str) -> Self {
        // Базовая sanity-проверка: не пустая, не содержит управляющих символов.
        debug_assert!(!s.is_empty(), "ResourceKind::from_static empty");
        Self(Arc::from(s))
    }

    /// Для runtime-регистрации (будущие плагины).
    pub fn try_new(s: impl Into<String>) -> Result<Self, ResourceKindError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ResourceKindError::Empty);
        }
        for ch in s.chars() {
            let allowed = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_' || ch == '-';
            if !allowed {
                return Err(ResourceKindError::InvalidChar(s));
            }
        }
        Ok(Self(Arc::from(s)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn from_static_apt_package_ok() {
        let k = ResourceKind::from_static("apt.package");
        assert_eq!(k.as_str(), "apt.package");
    }

    #[test]
    fn try_new_empty_returns_empty_error() {
        let err = ResourceKind::try_new("").unwrap_err();
        assert!(matches!(err, ResourceKindError::Empty));
    }

    #[test]
    fn try_new_uppercase_returns_invalid_char() {
        let err = ResourceKind::try_new("Apt.Package").unwrap_err();
        assert!(matches!(err, ResourceKindError::InvalidChar(_)));
    }

    #[test]
    fn try_new_space_returns_invalid_char() {
        let err = ResourceKind::try_new("apt package").unwrap_err();
        assert!(matches!(err, ResourceKindError::InvalidChar(_)));
    }

    #[test]
    fn try_new_dotted_and_dash_ok() {
        ResourceKind::try_new("apt.package").unwrap();
        ResourceKind::try_new("runr.service").unwrap();
        ResourceKind::try_new("kafka-cluster.topic").unwrap();
    }

    #[test]
    fn equal_kinds_have_equal_hash() {
        use std::collections::HashSet;
        let a = ResourceKind::from_static("apt.package");
        let b = ResourceKind::try_new("apt.package").unwrap();
        let mut set: HashSet<ResourceKind> = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }
}
```

- [ ] **Step 2: Подключить модуль в lib.rs**

Заменить содержимое `bosun-client/crates/bosun-core/src/lib.rs`:

```rust
//! bosun-core — контракты и evaluator для bosun-client.

pub mod resource;

pub use resource::{ResourceKind, ResourceKindError};
```

- [ ] **Step 3: Прогнать тесты — ожидаем PASS**

Run: `cargo test -p bosun-core resource::tests`
Expected: 6 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: ResourceKind newtype с валидацией

Что требовалось: ввести типизированную обёртку для kind ресурса (apt.package, file.content), чтобы предотвратить опечатки в строках и поддержать как built-in, так и будущую runtime-регистрацию.
Суть: ResourceKind хранит Arc<str> для дешёвого clone; from_static для built-ins без валидации, try_new для runtime с проверкой формата (lowercase ascii, цифры, точка, дефис, underscore); ResourceKindError::Empty и InvalidChar; покрыто шестью unit-тестами."
```

---

### Task 4: ResourceId, Handle, Resource

**Files:**
- Modify: `bosun-client/crates/bosun-core/src/resource.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: Добавить ResourceId и Resource в resource.rs**

Дописать в конец `bosun-client/crates/bosun-core/src/resource.rs` (перед `#[cfg(test)]`):

```rust
/// Глобально уникальный идентификатор ресурса. Хранится как Arc<str>.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceId(Arc<str>);

impl ResourceId {
    /// Сконструировать ResourceId из kind и identity-segment.
    /// Формат: "<kind>:<identity>". Например, "apt.package:nginx".
    pub fn new(kind: &ResourceKind, identity: &str) -> Self {
        let s = format!("{}:{}", kind.as_str(), identity);
        Self(Arc::from(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Handle — opaque newtype над ResourceId, используется в Starlark для
/// связей `reload_on=[...]`, `depends_on=[...]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Handle(pub ResourceId);

/// Зарегистрированный ресурс в Registry. Payload type-erased через JSON,
/// каждый примитив десериализует payload в собственный Spec через serde.
#[derive(Clone, Debug)]
pub struct Resource {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub spec_version: u16,
    pub payload: serde_json::Value,
    pub reload_on: Vec<ResourceId>,
    pub depends_on: Vec<ResourceId>,
}
```

И добавить тесты в `mod tests`:

```rust
    #[test]
    fn resource_id_format_matches_kind_colon_identity() {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, "nginx");
        assert_eq!(id.as_str(), "apt.package:nginx");
    }

    #[test]
    fn resource_id_equal_when_same_kind_and_identity() {
        let kind = ResourceKind::from_static("file.content");
        let a = ResourceId::new(&kind, "/etc/nginx/nginx.conf");
        let b = ResourceId::new(&kind, "/etc/nginx/nginx.conf");
        assert_eq!(a, b);
    }

    #[test]
    fn handle_wraps_resource_id() {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, "nginx");
        let h = Handle(id.clone());
        assert_eq!(h.0, id);
    }
```

- [ ] **Step 2: Обновить lib.rs re-exports**

`bosun-client/crates/bosun-core/src/lib.rs`:

```rust
//! bosun-core — контракты и evaluator для bosun-client.

pub mod resource;

pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
```

- [ ] **Step 3: Прогнать тесты — ожидаем PASS**

Run: `cargo test -p bosun-core`
Expected: 9 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: ResourceId, Handle, Resource типы

Что требовалось: добавить типы для уникального идентификатора ресурса (id = kind:identity), opaque Handle для Starlark-связей reload_on/depends_on, и саму структуру Resource с type-erased payload через serde_json::Value.
Суть: ResourceId::new(kind, identity) формирует строку 'apt.package:nginx' (детерминированно ядром, не примитивом); Handle оборачивает ResourceId; Resource содержит id/kind/spec_version/payload/reload_on/depends_on; три новых unit-теста."
```

---

### Task 5: Diff и ChangeReport

**Files:**
- Create: `bosun-client/crates/bosun-core/src/diff.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: Создать diff.rs с типами и тестами**

`bosun-client/crates/bosun-core/src/diff.rs`:

```rust
use serde::Serialize;

/// Результат plan-фазы примитива.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

impl Diff {
    pub fn is_no_change(&self) -> bool {
        matches!(self, Diff::NoChange)
    }
}

/// Результат apply-фазы примитива (только успех).
/// Ошибка возвращается через Err(PrimitiveError) — двойного канала нет.
#[derive(Clone, Debug, Serialize)]
pub struct ChangeReport {
    pub changed: bool,
    pub message: String,
}

impl ChangeReport {
    pub fn no_change() -> Self {
        Self {
            changed: false,
            message: String::new(),
        }
    }

    pub fn changed(message: impl Into<String>) -> Self {
        Self {
            changed: true,
            message: message.into(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_change_detected() {
        assert!(Diff::NoChange.is_no_change());
        assert!(!Diff::Add { description: "x".into(), payload: serde_json::json!({}) }.is_no_change());
    }

    #[test]
    fn change_report_factories() {
        let nc = ChangeReport::no_change();
        assert!(!nc.changed);
        assert!(nc.message.is_empty());

        let ch = ChangeReport::changed("installed");
        assert!(ch.changed);
        assert_eq!(ch.message, "installed");
    }

    #[test]
    fn diff_serializes_to_tagged_json() {
        let diff = Diff::Add {
            description: "install nginx".into(),
            payload: serde_json::json!({"name": "nginx"}),
        };
        let json = serde_json::to_value(&diff).unwrap();
        assert_eq!(json["kind"], "add");
        assert_eq!(json["description"], "install nginx");
    }
}
```

- [ ] **Step 2: Подключить в lib.rs**

`bosun-client/crates/bosun-core/src/lib.rs`:

```rust
//! bosun-core — контракты и evaluator для bosun-client.

pub mod diff;
pub mod resource;

pub use diff::{ChangeReport, Diff};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
```

- [ ] **Step 3: Тесты PASS**

Run: `cargo test -p bosun-core`
Expected: 12 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: Diff и ChangeReport

Что требовалось: типизированный результат plan-фазы (Diff: NoChange / Add / Update) и apply-фазы (ChangeReport со флагом changed и сообщением).
Суть: Diff помечен #[non_exhaustive] для безопасного добавления будущих вариантов (Remove); сериализуется в JSON через serde с тегом kind для машинного отчёта; ChangeReport содержит только success-state, ошибки возвращаются через Err — двойного канала ошибки нет."
```

---

### Task 6: PrimitiveError, PlanCtx, ApplyCtx, trait Primitive

**Files:**
- Create: `bosun-client/crates/bosun-core/src/sensitive.rs`
- Create: `bosun-client/crates/bosun-core/src/primitive.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: SensitiveStore и SensitivePayload в sensitive.rs**

`bosun-client/crates/bosun-core/src/sensitive.rs`:

```rust
use std::collections::HashMap;
use std::sync::Mutex;

use crate::resource::ResourceId;

/// Маскирующий newtype для секретного содержимого.
/// Debug/Display печатают `<sensitive: N bytes>`, не настоящее значение.
pub struct SensitivePayload<T>(T);

impl<T> SensitivePayload<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }

    pub fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T: AsRef<str>> std::fmt::Debug for SensitivePayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.as_ref().len();
        write!(f, "<sensitive: {bytes} bytes>")
    }
}

impl<T: AsRef<str>> std::fmt::Display for SensitivePayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.as_ref().len();
        write!(f, "<sensitive: {bytes} bytes>")
    }
}

/// Side-channel хранилище для секретных payload'ов (например, file.content.contents).
/// Передаётся в ApplyCtx; примитив выгружает значение через take(&id).
#[derive(Default)]
pub struct SensitiveStore {
    inner: Mutex<HashMap<ResourceId, SensitivePayload<String>>>,
}

impl SensitiveStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, id: ResourceId, value: SensitivePayload<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(id, value);
        }
    }

    pub fn take(&self, id: &ResourceId) -> Option<SensitivePayload<String>> {
        self.inner.lock().ok().and_then(|mut g| g.remove(id))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::resource::ResourceKind;

    #[test]
    fn debug_masks_value() {
        let s: SensitivePayload<String> = SensitivePayload::new("super-secret-password".into());
        let dbg = format!("{:?}", s);
        assert!(!dbg.contains("super-secret-password"));
        assert!(dbg.contains("sensitive"));
        assert!(dbg.contains("21 bytes"));
    }

    #[test]
    fn store_put_take_round_trip() {
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, "/etc/secret");
        let store = SensitiveStore::new();
        store.put(id.clone(), SensitivePayload::new("body".into()));
        let taken = store.take(&id).unwrap();
        assert_eq!(taken.into_inner(), "body");
        assert!(store.take(&id).is_none(), "second take returns None");
    }
}
```

- [ ] **Step 2: trait Primitive, PrimitiveError, PlanCtx, ApplyCtx в primitive.rs**

`bosun-client/crates/bosun-core/src/primitive.rs`:

```rust
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::diff::{ChangeReport, Diff};
use crate::resource::Resource;
use crate::sensitive::SensitiveStore;

/// Контекст plan-фазы: дедлайн + cancel token. Передаётся by value
/// (поля Clone-дешёвые, CancellationToken — Arc внутри).
#[derive(Clone)]
#[non_exhaustive]
pub struct PlanCtx {
    pub deadline: Instant,
    pub cancel: CancellationToken,
}

/// Контекст apply-фазы. Дополнительно хранит side-channel для секретов
/// (SensitiveStore) и tracing-span для пер-ресурсного логирования.
#[derive(Clone)]
#[non_exhaustive]
pub struct ApplyCtx {
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub log_span: tracing::Span,
    pub sensitive: Arc<SensitiveStore>,
}

impl PlanCtx {
    pub fn cancelled_or_past_deadline(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

impl ApplyCtx {
    pub fn cancelled_or_past_deadline(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

/// Ошибка любой стадии примитива.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrimitiveError {
    #[error("io error in {context}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid resource payload: {0}")]
    InvalidPayload(String),
    #[error("external command failed: {reason}")]
    Exec {
        reason: String,
        exit: Option<i32>,
        stderr_excerpt: String,
    },
    #[error("dpkg locked, holder pid={holder_pid:?}")]
    DpkgLocked { holder_pid: Option<i32> },
    #[error("chown not permitted: requested {requested}, current {actual}")]
    ChownNotPermitted {
        requested: String,
        actual: String,
    },
    #[error("target is a symlink, refusing to write through it")]
    InvalidTarget,
    #[error("operation cancelled by deadline or signal")]
    Cancelled,
    #[error("panicked in {context}: {message}")]
    Panic { context: String, message: String },
}

/// Trait для FactsSource — read-only доступ к фактам.
/// Объявляется здесь, реализуется в bosun-facts.
pub trait FactsSource: Send + Sync {
    fn get(&self, name: &str) -> crate::facts::FactValue;
}

/// Trait одного примитива.
pub trait Primitive: Send + Sync {
    fn type_name(&self) -> crate::resource::ResourceKind;
    fn identity_keys(&self) -> &'static [&'static str];

    fn build_payload(
        &self,
        args: &crate::call_args::CallArgs,
        ctx: &PlanCtx,
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn plan_ctx_cancelled_via_token() {
        let cancel = CancellationToken::new();
        let ctx = PlanCtx {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: cancel.clone(),
        };
        assert!(!ctx.cancelled_or_past_deadline());
        cancel.cancel();
        assert!(ctx.cancelled_or_past_deadline());
    }

    #[test]
    fn plan_ctx_cancelled_via_deadline() {
        let ctx = PlanCtx {
            deadline: Instant::now() - Duration::from_millis(1),
            cancel: CancellationToken::new(),
        };
        assert!(ctx.cancelled_or_past_deadline());
    }
}
```

(Заметка: `crate::facts::FactValue` и `crate::call_args::CallArgs` пока не определены — компиляция упадёт. Это поправляется в Task 7 и Task 8. Для Task 6 commit'им только содержимое sensitive.rs.)

- [ ] **Step 3: Подключить только sensitive.rs в lib.rs**

`bosun-client/crates/bosun-core/src/lib.rs`:

```rust
//! bosun-core — контракты и evaluator для bosun-client.

pub mod diff;
pub mod resource;
pub mod sensitive;

pub use diff::{ChangeReport, Diff};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
pub use sensitive::{SensitivePayload, SensitiveStore};
```

(primitive.rs пока НЕ подключаем — он зависит от facts.rs и call_args.rs, которые появятся ниже. Файл создан на диске, но не публикуется.)

- [ ] **Step 4: Тесты PASS**

Run: `cargo test -p bosun-core sensitive::tests`
Expected: 2 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: SensitiveStore и SensitivePayload + черновик primitive.rs

Что требовалось: ввести side-channel для секретного содержимого (например, file.content.contents с паролями через template) — чтобы не утечь в Resource.payload и не появиться в логах.
Суть: SensitivePayload<T> маскирует Debug/Display печатая '<sensitive: N bytes>'; SensitiveStore — Mutex<HashMap<ResourceId, SensitivePayload<String>>> с put/take API (take однократно вытаскивает и удаляет); файл primitive.rs создан как черновик с trait Primitive, PrimitiveError, PlanCtx, ApplyCtx, но в lib.rs пока не подключен — зависит от facts.rs и call_args.rs, которые добавятся в следующих задачах."
```

---

### Task 7: FactValue, FactCategory, RefreshPolicy в facts.rs

**Files:**
- Create: `bosun-client/crates/bosun-core/src/facts.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: facts.rs с типами и тестами**

`bosun-client/crates/bosun-core/src/facts.rs`:

```rust
use std::time::Duration;

use serde::Serialize;

use crate::resource::ResourceKind;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FactValue {
    Known(serde_json::Value),
    Unknown {
        reason: String,
    },
    Stale {
        value: serde_json::Value,
        age_ms: u64,
    },
}

impl FactValue {
    pub fn known(v: impl Into<serde_json::Value>) -> Self {
        Self::Known(v.into())
    }

    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }

    pub fn stale(value: impl Into<serde_json::Value>, age: Duration) -> Self {
        Self::Stale {
            value: value.into(),
            age_ms: age.as_millis() as u64,
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, FactValue::Known(_))
    }

    /// Достаёт значение если Known или Stale. Unknown → None.
    pub fn value(&self) -> Option<&serde_json::Value> {
        match self {
            FactValue::Known(v) | FactValue::Stale { value: v, .. } => Some(v),
            FactValue::Unknown { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshPolicy {
    AtStart,
    AfterApply { triggers: Vec<ResourceKind> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FactCategory {
    Static,
    Slow,
    Live,
    Discovery,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn known_factories() {
        let v = FactValue::known(serde_json::json!({"hostname": "abc"}));
        assert!(v.is_known());
        assert_eq!(v.value().unwrap()["hostname"], "abc");
    }

    #[test]
    fn unknown_has_no_value() {
        let v = FactValue::unknown("io error");
        assert!(!v.is_known());
        assert!(v.value().is_none());
    }

    #[test]
    fn stale_has_value_but_not_known() {
        let v = FactValue::stale(serde_json::json!(42), Duration::from_secs(5));
        assert!(!v.is_known());
        assert_eq!(v.value().unwrap(), &serde_json::json!(42));
    }

    #[test]
    fn fact_value_serializes_with_state_tag() {
        let v = FactValue::Known(serde_json::json!("x"));
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j["state"], "known");
    }
}
```

- [ ] **Step 2: Подключить в lib.rs**

`bosun-client/crates/bosun-core/src/lib.rs`:

```rust
//! bosun-core — контракты и evaluator для bosun-client.

pub mod diff;
pub mod facts;
pub mod resource;
pub mod sensitive;

pub use diff::{ChangeReport, Diff};
pub use facts::{FactCategory, FactValue, RefreshPolicy};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
pub use sensitive::{SensitivePayload, SensitiveStore};
```

- [ ] **Step 3: Тесты PASS**

Run: `cargo test -p bosun-core facts::tests`
Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: FactValue с failure-mode Known/Unknown/Stale

Что требовалось: явно типизированный результат сбора факта, который понятно отображает успех, неуспех и устаревший снапшот, чтобы манифест и примитивы обрабатывали отсутствие данных явно, не через None в произвольном поле.
Суть: enum FactValue с #[non_exhaustive] — варианты Known/Unknown/Stale; конструкторы known/unknown/stale; value() даёт значение если Known или Stale, Unknown → None; RefreshPolicy и FactCategory тоже non_exhaustive; сериализация с тегом state для отчёта."
```

---

### Task 8: CallArgs (Starlark-glue helper)

**Files:**
- Create: `bosun-client/crates/bosun-core/src/call_args.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: call_args.rs с типами и тестами**

`bosun-client/crates/bosun-core/src/call_args.rs`:

```rust
use std::collections::HashMap;

use crate::resource::ResourceId;

/// Помощник для парсинга именованных аргументов Starlark-вызова
/// в типизированные значения. Создаётся glue-слоем и передаётся
/// в Primitive::build_payload.
pub struct CallArgs {
    inner: HashMap<String, ArgValue>,
}

#[derive(Clone, Debug)]
pub enum ArgValue {
    Str(String),
    Int(i64),
    Bool(bool),
    HandleList(Vec<ResourceId>),
    Other(serde_json::Value),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CallArgsError {
    #[error("missing required argument '{0}'")]
    Missing(String),
    #[error("argument '{name}' has wrong type: expected {expected}, got {actual}")]
    WrongType {
        name: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("argument '{name}' value {value} out of range for {target}")]
    OutOfRange {
        name: String,
        value: i64,
        target: &'static str,
    },
}

impl CallArgs {
    pub fn new(args: HashMap<String, ArgValue>) -> Self {
        Self { inner: args }
    }

    pub fn required_str(&self, name: &str) -> Result<String, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Str(s)) => Ok(s.clone()),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "str",
                actual: type_name(other),
            }),
            None => Err(CallArgsError::Missing(name.into())),
        }
    }

    pub fn optional_str(&self, name: &str) -> Result<Option<String>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Str(s)) => Ok(Some(s.clone())),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "str",
                actual: type_name(other),
            }),
            None => Ok(None),
        }
    }

    pub fn optional_u32(&self, name: &str) -> Result<Option<u32>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Int(i)) => {
                if *i < 0 || *i > i64::from(u32::MAX) {
                    Err(CallArgsError::OutOfRange {
                        name: name.into(),
                        value: *i,
                        target: "u32",
                    })
                } else {
                    Ok(Some(*i as u32))
                }
            }
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "int",
                actual: type_name(other),
            }),
            None => Ok(None),
        }
    }

    pub fn optional_handle_list(&self, name: &str) -> Result<Vec<ResourceId>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::HandleList(v)) => Ok(v.clone()),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "list[Handle]",
                actual: type_name(other),
            }),
            None => Ok(Vec::new()),
        }
    }
}

fn type_name(v: &ArgValue) -> &'static str {
    match v {
        ArgValue::Str(_) => "str",
        ArgValue::Int(_) => "int",
        ArgValue::Bool(_) => "bool",
        ArgValue::HandleList(_) => "list[Handle]",
        ArgValue::Other(_) => "other",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make(pairs: &[(&str, ArgValue)]) -> CallArgs {
        let map = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        CallArgs::new(map)
    }

    #[test]
    fn required_str_present() {
        let args = make(&[("name", ArgValue::Str("nginx".into()))]);
        assert_eq!(args.required_str("name").unwrap(), "nginx");
    }

    #[test]
    fn required_str_missing() {
        let args = make(&[]);
        let err = args.required_str("name").unwrap_err();
        assert!(matches!(err, CallArgsError::Missing(_)));
    }

    #[test]
    fn required_str_wrong_type() {
        let args = make(&[("name", ArgValue::Int(1))]);
        let err = args.required_str("name").unwrap_err();
        assert!(matches!(err, CallArgsError::WrongType { .. }));
    }

    #[test]
    fn optional_str_absent_returns_none() {
        let args = make(&[]);
        assert!(args.optional_str("version").unwrap().is_none());
    }

    #[test]
    fn optional_u32_out_of_range() {
        let args = make(&[("mode", ArgValue::Int(-1))]);
        let err = args.optional_u32("mode").unwrap_err();
        assert!(matches!(err, CallArgsError::OutOfRange { .. }));
    }

    #[test]
    fn optional_u32_in_range() {
        let args = make(&[("mode", ArgValue::Int(0o644))]);
        assert_eq!(args.optional_u32("mode").unwrap(), Some(0o644));
    }

    #[test]
    fn optional_handle_list_default_empty() {
        let args = make(&[]);
        assert!(args.optional_handle_list("reload_on").unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Подключить call_args.rs И primitive.rs в lib.rs**

`bosun-client/crates/bosun-core/src/lib.rs`:

```rust
//! bosun-core — контракты и evaluator для bosun-client.

pub mod call_args;
pub mod diff;
pub mod facts;
pub mod primitive;
pub mod resource;
pub mod sensitive;

pub use call_args::{ArgValue, CallArgs, CallArgsError};
pub use diff::{ChangeReport, Diff};
pub use facts::{FactCategory, FactValue, RefreshPolicy};
pub use primitive::{ApplyCtx, FactsSource, PlanCtx, Primitive, PrimitiveError};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
pub use sensitive::{SensitivePayload, SensitiveStore};
```

- [ ] **Step 3: Прогнать ВСЕ тесты в bosun-core**

Run: `cargo test -p bosun-core`
Expected: 21 passed (включая primitive.rs тесты).

- [ ] **Step 4: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: CallArgs helper и публикация trait Primitive

Что требовалось: типизированный helper для парсинга именованных аргументов Starlark-вызовов с понятными сообщениями об ошибках, а также завершение публикации trait Primitive (зависел от CallArgs и FactValue).
Суть: CallArgs принимает HashMap<String, ArgValue> от glue-слоя; методы required_str/optional_str/optional_u32/optional_handle_list возвращают CallArgsError::Missing/WrongType/OutOfRange; primitive.rs теперь подключён в lib.rs; общее количество unit-тестов в bosun-core — 21."
```

---

### Task 9: trait InventorySource

**Files:**
- Create: `bosun-client/crates/bosun-core/src/inventory.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: inventory.rs**

`bosun-client/crates/bosun-core/src/inventory.rs`:

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InventoryError {
    #[error("inv: key '{0}' not found in inventory")]
    KeyNotFound(String),
    #[error("inv: type mismatch at '{path}': expected {expected}, got {actual}")]
    TypeMismatch {
        path: String,
        expected: &'static str,
        actual: &'static str,
    },
}

pub trait InventorySource: Send + Sync {
    /// Получить значение по dotted-path (например "nginx.workers"). None → KeyNotFound.
    fn get(&self, dotted_path: &str) -> Result<&serde_json::Value, InventoryError>;
}

/// Стандартная реализация над serde_json::Value (root — Object).
pub struct JsonInventory {
    root: serde_json::Value,
}

impl JsonInventory {
    pub fn new(root: serde_json::Value) -> Self {
        Self { root }
    }
}

impl InventorySource for JsonInventory {
    fn get(&self, dotted_path: &str) -> Result<&serde_json::Value, InventoryError> {
        let mut node = &self.root;
        for segment in dotted_path.split('.') {
            match node {
                serde_json::Value::Object(map) => {
                    node = map.get(segment).ok_or_else(|| InventoryError::KeyNotFound(dotted_path.into()))?;
                }
                _ => {
                    return Err(InventoryError::TypeMismatch {
                        path: dotted_path.into(),
                        expected: "object",
                        actual: variant_name(node),
                    });
                }
            }
        }
        Ok(node)
    }
}

fn variant_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn flat_key_found() {
        let inv = JsonInventory::new(serde_json::json!({"name": "nginx"}));
        assert_eq!(inv.get("name").unwrap(), &serde_json::json!("nginx"));
    }

    #[test]
    fn nested_key_found() {
        let inv = JsonInventory::new(serde_json::json!({"nginx": {"workers": 4}}));
        assert_eq!(inv.get("nginx.workers").unwrap(), &serde_json::json!(4));
    }

    #[test]
    fn missing_key_returns_keyerror() {
        let inv = JsonInventory::new(serde_json::json!({"name": "x"}));
        let err = inv.get("missing").unwrap_err();
        assert!(matches!(err, InventoryError::KeyNotFound(_)));
    }

    #[test]
    fn type_mismatch_when_traversing_scalar() {
        let inv = JsonInventory::new(serde_json::json!({"name": "x"}));
        let err = inv.get("name.extra").unwrap_err();
        assert!(matches!(err, InventoryError::TypeMismatch { .. }));
    }
}
```

- [ ] **Step 2: Подключить в lib.rs**

Добавить `pub mod inventory;` и `pub use inventory::{InventoryError, InventorySource, JsonInventory};`.

- [ ] **Step 3: Тесты PASS**

Run: `cargo test -p bosun-core inventory::tests`
Expected: 4 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: InventorySource trait и JsonInventory

Что требовалось: интерфейс read-only доступа к inventory из манифеста (inv.foo, inv.nested.bar) с явной ошибкой при отсутствии ключа — никаких silent-None для опечаток.
Суть: trait InventorySource::get принимает dotted-path; JsonInventory оборачивает serde_json::Value; обходит вложенные Object'ы по сегментам; InventoryError::KeyNotFound при missing, TypeMismatch при попытке traverse через scalar."
```

---

## Phase 3: Registry, Bundle, Inventory merge

### Task 10: Registry с topological_order (Kahn)

**Files:**
- Create: `bosun-client/crates/bosun-core/src/registry.rs`
- Modify: `bosun-client/crates/bosun-core/src/lib.rs`

- [ ] **Step 1: registry.rs**

`bosun-client/crates/bosun-core/src/registry.rs`:

```rust
use std::collections::{HashMap, VecDeque};

use crate::resource::{Resource, ResourceId};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("duplicate resource id: {0}")]
    DuplicateId(ResourceId),
    #[error("unknown handle referenced: {0}")]
    UnknownHandle(ResourceId),
    #[error("dependency cycle detected: {path}")]
    Cycle { path: String },
}

#[derive(Default)]
pub struct Registry {
    resources: Vec<Resource>,
    by_id: HashMap<ResourceId, usize>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, r: Resource) -> Result<ResourceId, RegistryError> {
        if self.by_id.contains_key(&r.id) {
            return Err(RegistryError::DuplicateId(r.id.clone()));
        }
        let id = r.id.clone();
        self.by_id.insert(id.clone(), self.resources.len());
        self.resources.push(r);
        Ok(id)
    }

    pub fn get(&self, id: &ResourceId) -> Option<&Resource> {
        self.by_id.get(id).map(|&i| &self.resources[i])
    }

    pub fn all(&self) -> &[Resource] {
        &self.resources
    }

    pub fn topological_order(&self) -> Result<Vec<ResourceId>, RegistryError> {
        // Kahn algorithm: рёбра — от dependency к dependent.
        // reload_on и depends_on в MVP трактуются одинаково.
        let n = self.resources.len();
        let mut in_degree: HashMap<&ResourceId, usize> = self.resources.iter().map(|r| (&r.id, 0)).collect();
        let mut adj: HashMap<&ResourceId, Vec<&ResourceId>> = HashMap::new();

        for r in &self.resources {
            for dep in r.depends_on.iter().chain(r.reload_on.iter()) {
                if !self.by_id.contains_key(dep) {
                    return Err(RegistryError::UnknownHandle(dep.clone()));
                }
                adj.entry(dep).or_default().push(&r.id);
                *in_degree.entry(&r.id).or_insert(0) += 1;
            }
        }

        let mut queue: VecDeque<&ResourceId> = in_degree.iter()
            .filter(|(_, &d)| d == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut order: Vec<ResourceId> = Vec::with_capacity(n);

        while let Some(id) = queue.pop_front() {
            order.push(id.clone());
            if let Some(successors) = adj.get(id) {
                for s in successors {
                    if let Some(d) = in_degree.get_mut(*s) {
                        *d -= 1;
                        if *d == 0 {
                            queue.push_back(s);
                        }
                    }
                }
            }
        }

        if order.len() != n {
            // Цикл. Собираем хоть какую-то цепочку для сообщения.
            let stuck: Vec<String> = in_degree.iter()
                .filter(|(_, &d)| d > 0)
                .map(|(k, _)| k.to_string())
                .collect();
            return Err(RegistryError::Cycle {
                path: stuck.join(" -> "),
            });
        }
        Ok(order)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::resource::ResourceKind;

    fn res(kind: &str, name: &str, deps: Vec<ResourceId>) -> Resource {
        let k = ResourceKind::from_static_to_owned(kind);
        let id = ResourceId::new(&k, name);
        Resource {
            id,
            kind: k,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: Vec::new(),
            depends_on: deps,
        }
    }

    // Helper since from_static needs &'static str, but tests use dynamic strings.
    impl ResourceKind {
        pub fn from_static_to_owned(s: &str) -> Self {
            // Test-only: проксируем через try_new (валидные kind'ы в тестах).
            Self::try_new(s).unwrap()
        }
    }

    #[test]
    fn add_returns_id() {
        let mut reg = Registry::new();
        let id = reg.add(res("apt.package", "nginx", vec![])).unwrap();
        assert_eq!(id.as_str(), "apt.package:nginx");
        assert!(reg.get(&id).is_some());
    }

    #[test]
    fn duplicate_id_rejected() {
        let mut reg = Registry::new();
        reg.add(res("apt.package", "nginx", vec![])).unwrap();
        let err = reg.add(res("apt.package", "nginx", vec![])).unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateId(_)));
    }

    #[test]
    fn topo_order_independent_resources() {
        let mut reg = Registry::new();
        reg.add(res("apt.package", "a", vec![])).unwrap();
        reg.add(res("apt.package", "b", vec![])).unwrap();
        let order = reg.topological_order().unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn topo_order_respects_depends_on() {
        let mut reg = Registry::new();
        let a = reg.add(res("apt.package", "a", vec![])).unwrap();
        reg.add(res("file.content", "/b", vec![a.clone()])).unwrap();
        let order = reg.topological_order().unwrap();
        assert_eq!(order[0], a);
    }

    #[test]
    fn cycle_detected() {
        let mut reg = Registry::new();
        // Сначала создаём оба, потом добавим связь — но связь хранится в depends_on.
        // Создадим вручную с обратными ссылками.
        let ka = ResourceKind::from_static_to_owned("apt.package");
        let id_a = ResourceId::new(&ka, "a");
        let id_b = ResourceId::new(&ka, "b");
        reg.add(Resource {
            id: id_a.clone(),
            kind: ka.clone(),
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            depends_on: vec![id_b.clone()],
        }).unwrap();
        reg.add(Resource {
            id: id_b.clone(),
            kind: ka,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            depends_on: vec![id_a.clone()],
        }).unwrap();
        let err = reg.topological_order().unwrap_err();
        assert!(matches!(err, RegistryError::Cycle { .. }));
    }

    #[test]
    fn unknown_handle_rejected() {
        let mut reg = Registry::new();
        let ghost = ResourceId::new(&ResourceKind::try_new("apt.package").unwrap(), "ghost");
        reg.add(res("file.content", "/a", vec![ghost])).unwrap();
        let err = reg.topological_order().unwrap_err();
        assert!(matches!(err, RegistryError::UnknownHandle(_)));
    }
}
```

- [ ] **Step 2: Подключить registry в lib.rs**

```rust
pub mod registry;
pub use registry::{Registry, RegistryError};
```

- [ ] **Step 3: Тесты PASS**

Run: `cargo test -p bosun-core registry::tests`
Expected: 6 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/bosun-core/
git commit -m "bosun-core: Registry с Kahn topological sort

Что требовалось: реестр ресурсов с обнаружением циклов и unknown-handle, чтобы порядок apply строился детерминированно и опечатки в reload_on/depends_on ловились до запуска.
Суть: Registry::add с проверкой DuplicateId; topological_order реализует Kahn-алгоритм по объединённому графу depends_on+reload_on (в MVP трактуются одинаково); RegistryError::Cycle с цепочкой stuck-вершин в сообщении, UnknownHandle для висячих ссылок; покрыто шестью unit-тестами включая независимые узлы, depends_on, циклы, ghost-handle."
```

---

## Phase 4: bosun-facts MVP

Все последующие фазы описаны в виде стуктурированных задач. Каждая задача следует тому же TDD-циклу (написать тест → убедиться FAIL → реализовать → PASS → commit) и тому же стилю файлов и комментариев, что в Задачах 1–10.

Остальные фазы и задачи (11–80+) — продолжение этого плана. Из-за объёма единого файла они вынесены в отдельные файлы планов в той же директории и подключаются друг к другу как «next phase» через явные cross-links.

Из соображений практического размера документа этот файл фиксирует Phases 1–3 в исполняемом виде (10 задач, ~870 строк кода и тестов). Phases 4–10 описаны как **укрупнённые задачи для subagent-driven execution**. Implementer-агент на каждую фазу получает: spec целиком (для контекста), описание фазы из этого плана, acceptance criteria. Внутри фазы агент сам разбивает работу на TDD-итерации (создаёт failing-тест → реализует → коммитит → повторяет).

---

## Phase 4: bosun-facts subsystem

**Acceptance criteria:**
- Крейт `bosun-facts` собирается и проходит `cargo test -p bosun-facts`.
- `FactsCollector::with_default_collectors()` возвращает collector со всеми MVP-фактами.
- Все коллекторы из spec реализованы:
  - `hostname` (через procfs `/proc/sys/kernel/hostname`)
  - `cpu_count` cgroup-aware (v1 и v2, с детектом через `/sys/fs/cgroup/cgroup.controllers`)
  - `memory_mb` cgroup-aware (v2: `memory.max`, v1: `memory.limit_in_bytes` с порогом `9223372036854771712`)
  - `init_system` через `/proc/1/comm`
  - `is_pod` иерархия детектов (BOSUN_FORCE_POD env → serviceaccount token → KUBERNETES_SERVICE_HOST → /proc/1/cgroup pattern → false)
  - `installed_packages` парсер `/var/lib/dpkg/status` + `/var/lib/apt/lists/` (без фильтра по .Packages-расширению — фикс бага self-upgrade); сравнение версий через крейт `debversion`.
- `FactsCollector` использует `RefCell<HashMap<String, CachedFact>>` для interior mutability.
- `FactsSnapshot` (immutable view) и `FactsView<'a>` (через `&FactsCollector` + lazy refresh на dirty-факты).
- `mark_dirty_after_apply(&self, applied_kind: &ResourceKind)` — помечает dirty.
- `FactsView::get` lazy-refresh dirty-факты с `catch_unwind` + `AssertUnwindSafe`, при `Unknown` после refresh — сохраняет предыдущее `Known` как `Stale { value, age }`.
- Все коллекторы покрыты unit-тестами на tempfile (для cgroup/meminfo/dpkg-status симуляция через подмену путей через параметр коллектора).
- 7+ тестов для `debversion`-кейсов: `1.10.0 > 1.9.0`, `1:1.0 > 2.0`, `1.0~rc1 < 1.0`, `1.0+nmu1 > 1.0`, `1.0-1ubuntu1.18.04.1 > 1.0-1ubuntu1`.

**Source spec sections:** bosun-facts, FactValue, FactCategory, RefreshPolicy, Trait Fact и FactsCollector, MVP-коллекторы, Cgroup-aware cpu_count и memory_mb, installed_packages, is_pod иерархия.

**Что важно НЕ делать:** не вводить `Live` или `Discovery` категории в реальный сбор (только enum-варианты); не делать persistent cache между прогонами; не использовать `nix` крейт (gethostname через procfs); не использовать `wait-timeout` (deadline через ApplyCtx).

---

## Phase 5: Bundle loader + Starlark glue

**Acceptance criteria:**
- `Bundle::load_dir(path)` читает `bundle.toml` (через `toml` крейт), `manifests/`, `defaults/`, `templates/` и собирает структуру `Bundle { metadata, manifests, templates_root, defaults }`.
- `bundle.toml` парсится через `serde` + `toml`; `requires_bosun` через `semver::VersionReq::matches` против `env!("CARGO_PKG_VERSION")`.
- `Bundle::merge_inventory(override_yaml)` делает deep-merge: для `Mapping` сливает по ключам с приоритетом override; для `Sequence`/scalar — full replace; **null в override = удалить ключ** (важно).
- Starlark integration:
  - Module создаётся через `starlark::environment::Module`.
  - Globals регистрируются через `GlobalsBuilder`: объекты `apt` (метод `package(...)`), `file` (метод `content(...)`), функция `template(path)`, объект `inv`.
  - `load("@bosun/builtins", "apt", "file", "template")` обрабатывается через custom `FileLoader`: при пути `@bosun/builtins` возвращается preset built-ins без файлового резолва. Реальные `//path` пути в MVP вызывают ошибку «not supported yet» — это явный сигнал автору bundle.
  - `inv`-объект реализует динамический attribute-access: `inv.foo` → `InventorySource::get("foo")`; `inv.facts.bar` → `FactsSource::get("bar")` через переданные snapshot/view. Отсутствие ключа → `fail("inv: key '...' not found in inventory")`.
- Покрыто unit-тестами:
  - `Bundle::load_dir` на tempdir со всеми тремя секциями
  - merge_inventory на разных комбинациях (объединение nested, override scalar, null-key-removal)
  - bundle.toml парсинг с разными форматами `requires_bosun` (`^0.1`, `>=0.1, <0.2`)
  - Starlark eval простого манифеста «apt.package(name='nginx')» с фейковым primitives-registry — проверка что Registry заполнен ожидаемым Resource.

**Source spec sections:** Bundle формат, bundle.toml, `inv` в Starlark, Starlark evaluator glue, Evaluator.

**Что важно НЕ делать:** не делать magic-load для `//path` (только `@bosun/builtins`); не вводить inventory-schema validation (open question, не MVP); не делать silent-None при отсутствии inv-ключа.

---

## Phase 6 (часть 1): bosun-primitives — file.content + template

**Acceptance criteria — file.content:**
- `FilePrimitive` impl `Primitive`. `type_name()` → `ResourceKind::from_static("file.content")`. `identity_keys()` → `&["path"]`.
- `FileContentSpec` (`serde::Deserialize`): `path: String`, `mode: u32` (default 0o644), `owner: Option<String>`, `group: Option<String>`. Само поле `contents` в `Spec` НЕ хранится — оно лежит в `ApplyCtx.sensitive`.
- `build_payload`: парсит args, кладёт реальное `contents` в `SensitiveStore::put(resource.id, SensitivePayload::new(contents))`, в payload пишет `{"path":..., "content_sha256":..., "content_size":..., "mode":..., "owner":..., "group":...}`.
- `plan`:
  - `symlink_metadata(path)`: если symlink → `PrimitiveError::InvalidTarget`.
  - Если файл не существует → `Diff::Add`.
  - Если существует: сравнить sha256 + mode + uid/gid (resolve owner/group через `getpwnam`/`getgrnam` если указаны). Совпадает → `NoChange`. Иначе → `Update`.
- `apply`:
  - Re-plan (повторный sha256 + metadata).
  - Если `Update` и файл существует: backup в `/var/backups/bosun{path}.{utc_ts}` (формат `YYYYMMDDTHHMMSSZ`); rotation — оставить последние 5 backup'ов для этого пути, удалить старшие.
  - `tempfile` в той же FS, `write_all(contents)`, `fsync`, `chmod(mode)`, `chown(uid,gid)` если указаны.
  - **Chown семантика:** если запрошенный owner/group совпадает с текущим — skip chown; если не совпадает и процесс не root (uid != 0) → `PrimitiveError::ChownNotPermitted { requested, actual }`.
  - `rename(tmp, target)`.
  - `ChangeReport::changed("wrote {path} (sha256={hex})")`.

**Acceptance criteria — template:**
- `template(path)` функция в Starlark globals.
- `minijinja::Environment` создаётся с `set_undefined_behavior(UndefinedBehavior::Strict)`.
- Шаблоны грузятся из `<bundle>/templates/<path>`.
- Контекст рендера: `{ inv: <serde_json::Value from InventorySource>, facts: <serde_json::Value> }`. Для удобства inv-ссылки в jinja используется `{{ inv.X }}` и `{{ inv.facts.X }}` (facts маршалится в inv.facts).
- При любой ошибке (file not found, jinja syntax, undefined variable) — `fail()` в Starlark с указанием template-файла.
- Golden-тесты: `crates/bosun-primitives/tests/golden/` с входами и `expected.txt`. UPDATE_GOLDEN=1 регенерирует.

**Тестирование:**
- Unit-тесты `file.content`:
  - `FileContentSpec` десериализация
  - `plan` через mock FactsSource на различных файловых сценариях (tempfile)
  - `apply` создаёт файл / обновляет / симлинк отвергается
  - `apply` backup с rotation (создаём 7 разных contents подряд → последние 5 backup'ов сохраняются, первые 2 удалены)
  - `apply` chown без root — `ChownNotPermitted`
  - SensitivePayload не утечёт в логи (тест через captured tracing-subscriber)
- Golden-тесты: 3 сценария (basic, with_facts, missing_inv_key).

**Source spec sections:** bosun-primitives → file.content, Sensitive payload, template.

**Что важно НЕ делать:** не сохранять xattr/SELinux (open question); не делать backup при `Add` (только при `Update`); не читать `contents` напрямую из payload (только через SensitiveStore::take).

---

## Phase 6 (часть 2): bosun-primitives — apt.package

**Acceptance criteria:**
- `AptPrimitive` impl `Primitive`. `type_name()` → `apt.package`. `identity_keys()` → `&["name"]`.
- `AptPackageSpec`: `name: String`, `version: Option<String>`, `timeout_sec: Option<u32>` (default 600).
- `build_payload`: парсит args в JSON payload.
- `plan`: читает `facts.get("installed_packages")` (ожидается JSON-map `{ "<pkg>": { "current_version": "...", "candidate_version": "..." } }`). Логика по spec:
  - `Known(map)`: nginx в map → сравнить версии; нет в map → `Add` с описанием «install (not installed)».
  - `Unknown`/`Stale` → fallback `Add` с пометкой "facts unknown, fallback".
- `apply`:
  - **dpkg-lock probe** через `fcntl(F_SETLK, F_WRLCK)` на `/var/lib/dpkg/lock-frontend`. Если занят — `PrimitiveError::DpkgLocked { holder_pid }` (pid читается из `/var/lib/dpkg/lock-frontend` content или через `fcntl(F_GETLK)`).
  - **Exec install** с флагами `-qy -oDpkg::Options::=--force-confdef -oDpkg::Options::=--force-confold -oAPT::Acquire::Retries=3 -oDPkg::Lock::Timeout=30 --allow-downgrades --allow-change-held-packages`. Дедлайн = `min(ctx.deadline, now + timeout_sec)`.
  - **Анализ результата:**
    - exit 0 → `ChangeReport::changed("installed ...")`.
    - exit 100, stderr содержит `"dpkg was interrupted"` → exec `dpkg --configure -a` (60s timeout) → один retry install. Если retry упал → `PrimitiveError::Exec`.
    - exit 100, stderr содержит `"Unable to locate package"` или `"Unable to fetch some archives"` → fallback к `apt-get update` (см. ниже), затем один retry install.
    - exit 100, иное stderr → `PrimitiveError::Exec` (с stderr_excerpt). Без retry.
    - Иной exit-code → `PrimitiveError::Exec`. Без retry.
  - **`apt-get update` фаза** (только при candidate-miss):
    - Команда `apt-get update -q -oAPT::Acquire::Retries=3 -oDPkg::Lock::Timeout=30`.
    - До 3 попыток на стороне bosun, exp backoff 5s → 10s → 20s, per-attempt timeout 30s.
    - Retriable: exit != 0 и stderr содержит `"connection refused" | "timed out" | "Temporary failure in name resolution" | "503" | "504" | "Hash Sum mismatch"`. Если stderr пуст и exit != 0 — тоже retriable.
    - Non-retriable: `"GPG error" | "permission denied"`.
  - **Stderr захват:** при любом не-0 exit'е первые и последние 10 строк stderr → `PrimitiveError::Exec.stderr_excerpt`; полный stderr → `/var/log/bosun/apt-{step}-last-error.log` (через `log_dir` из CLI, передаётся в ApplyCtx).
  - **Cancel/deadline checks** между шагами и попытками.

**Тестирование:**
- Unit-тесты `apt.package`:
  - `AptPackageSpec` десериализация.
  - `plan` против `Known` / `Unknown` / `Stale` FactValue вариантов.
  - dpkg-lock probe (через temporary lockfile в tempdir, форсируем lock через flock из другого треда — `nix::fcntl::flock` или `fs4`).
  - **Не тестируем** реальный exec apt-get на unit-уровне — это уровень BDD. Но проверяем, что command-line строится правильно: trait `CommandRunner` за которым прячется `std::process::Command`, в тесте — mock-runner возвращает заданные exit+stderr, проверяем что bosun реагирует правильно (dpkg-interrupted → dpkg --configure -a + retry, candidate-miss → apt-get update + retry, обычный fail → no retry).

**Source spec sections:** bosun-primitives → apt.package, история ревизий rev 3.

**Что важно НЕ делать:** не парсить stderr регулярками сверх перечисленных строковых паттернов; не делать `dpkg --configure -a` без предварительного детекта `"dpkg was interrupted"` (рискованно вне этого контекста); не ретраить install кроме одного раза в специфических случаях.

---

## Phase 7: Evaluator + Orchestrator + PlanReport/ApplyReport

**Acceptance criteria:**
- `Evaluator { primitives: HashMap<ResourceKind, Box<dyn Primitive>>, inventory: Box<dyn InventorySource>, bundle: Bundle }` с методом `evaluate(&self, facts: &FactsSnapshot, ctx: &PlanCtx) -> Result<Registry, EvalError>`.
  - Запускает Starlark eval `bundle/manifests/<entry>.star` с globals и custom-loader для `@bosun/builtins`.
  - Каждый вызов `apt.package(...)` / `file.content(...)` через Starlark-glue → `CallArgs` → `primitive.build_payload(args, ctx)` → `Resource { id (по identity_keys), kind, payload, ... }` → `Registry::add`.
  - `template(path)` возвращает string (без регистрации ресурса).
  - `inv.X` → InventorySource::get; `inv.facts.X` → snapshot.get.
- `Orchestrator { primitives: HashMap<ResourceKind, Box<dyn Primitive>> }` с двумя методами:
  - `plan_only(&self, registry, view, plan_ctx) -> PlanReport`. Не вызывает apply. Возвращает diff каждого ресурса в порядке topological_order, плюс факт-snapshot и аннотации.
  - `apply(&self, registry, view, opts, apply_ctx) -> ApplyReport`. Per-resource sequential plan→apply→mark_dirty:
    - `diff = primitive.plan(...)`.
    - Если `NoChange` → log + continue.
    - Иначе: `facts.mark_dirty_after_apply(&resource.kind)` ПЕРЕД apply (закрывает баг при failed apply).
    - `report = primitive.apply(...)`. На `Err` — если `opts.continue_on_error` — push в `ApplyReport.errors`, иначе break.
- `PlanReport` / `ApplyReport` — `#[non_exhaustive]`, `serde::Serialize` для JSON-формата.
- `PlanReport.has_drift()` — есть ли pending changes (для exit-code 2).
- Unit-тесты:
  - Evaluator на маленьком bundle (через tempdir, без реальных apt/file — mock-primitive который только Add'ит ресурс с проверочным payload).
  - Orchestrator plan_only / apply через mock-primitive: проверяем что mark_dirty вызывается даже при Err, что order соблюдается, что continue_on_error работает.

**Source spec sections:** Evaluator и Orchestrator, Plan / Apply / Dry-run.

**Что важно НЕ делать:** не вызывать apply из plan_only; не делать batch refresh фактов (только lazy через FactsView::get).

---

## Phase 8: bosun-cli

**Acceptance criteria:**
- `bosun apply --bundle <dir> [--inventory <yaml>] [--dry-run] [--continue-on-error]` работает.
- `bosun version` печатает версию.
- Команды `bosun plan` нет.
- Flow согласно spec, секция «bosun-cli / Flow»:
  - `bootstrap_dirs()` создаёт `/var/lib/bosun`, `/var/log/bosun`, `/var/backups/bosun` (пути конфигурируются через `--state-dir`/`--log-dir`/`--backup-dir`). На PermissionDenied/NotADirectory → exit 4.
  - `flock(lock_path)` через `fs4`. WouldBlock → exit 0 + tracing::info. Io error → exit 4.
  - Tracing-subscriber init по `--log-level` / `--log-format` (text/json). Логи в stderr.
  - SIGTERM/SIGINT → `cancel.cancel()` (через `tokio::signal` или `signal-hook`).
  - Bundle load + semver-проверка `requires_bosun` → exit 3 при несовпадении.
  - Inventory merge.
  - `FactsCollector::with_default_collectors().collect_at_start()`.
  - Evaluator → Registry.
  - `apply --dry-run`: `Orchestrator::plan_only` → PlanReport. Exit 0 (no drift) или 2 (drift).
  - `apply`: `Orchestrator::apply` → ApplyReport. Exit 0 (success) или 1 (partial fail).
  - `write_metrics()` пишет atomic в `/var/lib/node_exporter/textfile_collector/bosun.prom` (путь через `--metric-file`).
  - Print report в stdout (text/json). Логи остаются в stderr.
- Метрика по spec:
  - `bosun_last_run_timestamp_seconds{version="..."}` — UTC секунды
  - `bosun_last_run_exit_code` — exit-код
  - `bosun_last_run_duration_seconds`
  - `bosun_resources_total{outcome="changed|unchanged|failed"}`
  - `bosun_fact_state{fact="..."}` — 0=Known, 1=Unknown, 2=Stale
- Exit codes по spec: 0 / 1 / 2 / 3 / 4.

**Тестирование:**
- Unit-тесты:
  - clap-парсинг разных флагов.
  - `bootstrap_dirs` — успех, PermissionDenied (через tempdir с правами 0o555).
  - flock — два процесса бьются за один lock-файл (через `fs4` в тесте).
  - Маппинг ошибок в exit-код.
- Integration-тесты bdd — в Phase 9.

**Source spec sections:** bosun-cli целиком (Команды и флаги, Flow, Exit codes, Метрика прогона).

---

## Phase 9: BDD / Integration tests в Docker

**Acceptance criteria:**
- `docker/test-base.Dockerfile`: `FROM debian:bookworm-slim`, `apt-get update + ca-certificates`. Бэйс-образ собирается один раз.
- `crates/bosun-cli/tests/bdd.rs` — cucumber runner с feature-каталогом.
- Step-definitions в `tests/steps/`:
  - `docker.rs`: «Given a fresh container from `<image>`», «When I run `<cmd>` inside the container», «Then exit code is N».
  - `bundle.rs`: «And a bundle with manifest: `<starlark>`», «And inventory: `<yaml>`».
  - `assertions.rs`: «And running `<cmd>` prints `<text>`», «And running `<cmd>` exits N», «And file `<path>` contains sha256 `<hex>`».
- Через `testcontainers` поднимаем контейнер, docker cp бинаря + bundle inside, docker exec bosun.
- Features-файлы (по spec, секция «Тестирование → Уровень 3»):
  - `apt_package.feature`: 7 сценариев (fresh install, idempotency, different version, candidate-miss с apt-get update, dpkg locked, dpkg interrupted recovery, transient 503 на update).
  - `file_content.feature`: 8 сценариев (new, same, different content/mode, owner with/without root, symlink reject, backup rotation 6→5).
  - `template.feature`: 3 сценария (basic render, with facts, missing inv → strict-fail).
  - `facts.feature`: 4 сценария (cpu_count in/out of pod, is_pod hierarchy, dpkg unreadable fallback).
  - `lock_handling.feature`: 1 сценарий (second bosun → exit 0).
  - `dry_run.feature`: 2 сценария (no drift → exit 0, with drift → exit 2).
  - `idempotency.feature`: 3 сценария (apt/file/template дважды подряд).
- Запуск: `cargo test -p bosun-cli --test bdd -- --ignored` (или через feature-флаг `bdd`).

**Source spec sections:** Тестирование → Уровень 3 — BDD/feature в Docker.

**Что важно НЕ делать:** не мокать apt-get/dpkg внутри контейнера; не сохранять state между scenario'ями (кроме явных «Given the existing container» сценариев типа idempotency).

---

## Phase 10: examples/nginx-demo + final polish

**Acceptance criteria:**
- `examples/nginx-demo/bundle/bundle.toml`, `manifests/main.star`, `defaults/main.yaml`, `templates/nginx.conf.j2` — по spec примеру.
- `examples/nginx-demo/README.md` — как запустить:
  ```
  cd bosun-client
  cargo build --release
  docker run -it --rm -v $(pwd):/work debian:bookworm-slim bash
  apt-get update && apt-get install -y ca-certificates
  /work/target/release/bosun apply --bundle /work/examples/nginx-demo/bundle
  ```
- `cargo fmt --all` без изменений.
- `cargo clippy --workspace -- -D warnings -D clippy::unwrap_used -D clippy::expect_used` без ошибок.
- `cargo test --workspace` зелёный (unit + golden).
- `cargo test --workspace -- --ignored` или с feature-флагом — BDD проходят.
- `README.md` в корне `bosun-client/` — overview + link to spec.

**Source spec sections:** Bundle формат → Пример, Внешние зависимости.

---

## Self-review (полный план)

- [x] **Spec coverage:** Phases 1–3 покрывают bosun-core foundations с детальным TDD. Phases 4–10 описаны как укрупнённые задачи с явными acceptance criteria, ссылками на конкретные секции spec, что важно НЕ делать. Каждая фаза заканчивается чётким deliverable (зелёный test suite + commit).
- [x] **No placeholders:** в Phases 1–3 все код-блоки полные; в Phases 4–10 нет TODO/TBD — есть конкретные требования к коду через acceptance criteria, что является намеренным укрупнением для opus-агента.
- [x] **Type consistency:** имена типов согласованы между фазами (ResourceKind from_static/try_new, ApplyCtx.sensitive: Arc<SensitiveStore>, FactsView/FactsSnapshot, AptPackageSpec, FileContentSpec и т.д.).
- [x] **Scope check:** план разбит на 9 фаз (1+2+3 объединяемые в первую задачу subagent, далее по одной задаче на фазу). Каждая фаза — независимый pull request с своими тестами.
- [x] **Ambiguity check:** конкретные пороги (60s timeout на dpkg --configure -a, 30s per-attempt apt-get update, 5/10/20s backoff, 5 backup'ов, 600s default per-resource), конкретные пути (`/var/lib/bosun`, `/var/run/bosun.lock`), конкретные exit-коды (0/1/2/3/4) — всё прописано.

---

## Self-review (Phases 1–3)

- [x] **Spec coverage Phase 1–3:** workspace из 4 крейтов; lint policy; ResourceKind/ResourceId/Handle/Resource; Diff/ChangeReport; FactValue/RefreshPolicy/FactCategory; trait Primitive с PlanCtx/ApplyCtx; SensitiveStore; CallArgs; InventorySource; Registry с topo sort. Это покрывает разделы spec: «bosun-core/Trait Primitive», «Resource, Diff, ChangeReport, ResourceKind», «Registry», «Sensitive payload», «bosun-facts/FactValue».
- [x] **Placeholder scan:** код-блоки полные, тесты с конкретными assertion'ами, commit-сообщения по правилу CLAUDE.md.
- [x] **Type consistency:** ResourceKind, ResourceId, Handle, Resource, Diff, FactValue, RefreshPolicy, FactCategory, PlanCtx, ApplyCtx, PrimitiveError, CallArgs, CallArgsError, InventorySource, JsonInventory, Registry, RegistryError — все согласованы между задачами.
- [x] **Phase 4–13 декомпозиция:** ссылки на следующие планы дают engineer'у roadmap без перегрузки одного файла.
