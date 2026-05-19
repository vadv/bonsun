//! Интеграционный слой между Starlark и bosun.
//!
//! Архитектура (см. spec «Starlark evaluator glue»):
//! - Native globals `apt`, `file`, `template` собираются в `FrozenModule`
//!   `@bosun/builtins`. `BundleLoader` выдаёт его на `load("@bosun/builtins")`.
//! - `inv` — это специальный объект, который устанавливается как
//!   module-level переменная через `module.set("inv", ...)` перед
//!   eval_module. Внутри `inv` хранит ссылку на состояние через
//!   thread-local handle.
//! - На время `evaluate_manifest` thread-local `CURRENT_STATE` хранит
//!   `Rc<EvalState>`. Native-функции и attribute access читают state
//!   через `with_state(...)`. После возврата thread-local очищается.
//!
//! `allow(unsafe_code)`: derive-макросы `Trace`/`Freeze` из `starlark_derive`
//! генерируют `unsafe impl`-блоки. Мы не пишем `unsafe` сами — это требование
//! API starlark для опубликованных Value-типов.

#![allow(unsafe_code)]

mod globals;
mod inv_object;
mod load_resolver;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};

use crate::bundle::{Bundle, BundleError};
use crate::primitive::{FactsSource, PlanCtx, Primitive};
use crate::registry::Registry;
use crate::resource::ResourceKind;
use crate::sensitive::SensitiveStore;

pub use globals::{build_builtins_module, build_globals};
pub use load_resolver::BundleLoader;

/// Closure для рендера шаблона из Starlark-глобала `template(path)`.
///
/// Вынесена в инжектируемый closure, потому что реальный рендер
/// (`render_template`) живёт в `bosun-primitives` — он зависит от
/// `bosun-core`, и обратная зависимость замкнула бы цикл крейтов. CLI
/// в Phase 8 строит этот closure через `bosun-primitives::render_template`
/// с забэканным templates_root, inv-копией и materialized-фактами.
///
/// Тип `Rc<dyn Fn>`, потому что closure хранится в `Rc<EvalState>` и
/// разделяется между вызовами native-функций — копия дешёвая.
pub type TemplateFn = Rc<dyn Fn(&str) -> Result<String, anyhow::Error>>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StarlarkGlueError {
    #[error("starlark syntax error in {file}: {message}")]
    Syntax { file: String, message: String },
    #[error("starlark evaluation error: {0}")]
    Eval(String),
    #[error("invalid call to {kind}: {reason}")]
    InvalidCall { kind: String, reason: String },
    #[error("primitive not registered: {0}")]
    UnknownPrimitive(String),
    #[error("bundle error: {0}")]
    Bundle(#[from] BundleError),
}

/// State, разделяемое между всеми Starlark-объектами одного запуска
/// evaluate_manifest. Хранится в thread-local на время вызова.
///
/// Все поля — owned либо Arc/Rc/RefCell. Никаких borrowed-ссылок, чтобы
/// state мог жить в `Rc` (необходимое для thread-local).
pub(crate) struct EvalState {
    pub(crate) primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
    pub(crate) facts: Rc<dyn FactsSource>,
    /// Хранилище секретов для file.content.contents. Native-функция
    /// `file.content(...)` извлекает `contents`-аргумент и кладёт его сюда
    /// под ключом ResourceId до того, как зовёт `Primitive::build_payload`.
    pub(crate) sensitive: Arc<SensitiveStore>,
    pub(crate) registry: Rc<RefCell<Registry>>,
    pub(crate) plan_ctx: PlanCtx,
    pub(crate) errors: RefCell<Vec<StarlarkGlueError>>,
    /// Closure для рендера шаблонов. Инжектируется CLI в Phase 8; в
    /// unit-тестах bosun-core может быть `default_template_fn`, который
    /// сразу отдаёт ошибку «template rendering not configured».
    pub(crate) template_fn: TemplateFn,
}

thread_local! {
    /// Текущий state evaluate_manifest. Set/clear выполняются guard'ом
    /// `StateGuard` ниже — это гарантирует, что cleanup произойдёт даже
    /// при панике.
    static CURRENT_STATE: RefCell<Option<Rc<EvalState>>> = const { RefCell::new(None) };
}

/// RAII-guard: ставит state в thread-local на time existence, очищает на drop.
struct StateGuard;

impl StateGuard {
    fn install(state: Rc<EvalState>) -> Result<Self, StarlarkGlueError> {
        let already_installed = CURRENT_STATE.with(|cell| cell.borrow().is_some());
        if already_installed {
            // Re-entrant evaluate_manifest на том же thread запрещён: вложенный
            // bosun-evaluation потерял бы parent state. В release-build раньше
            // это был silent debug_assert; теперь — явная ошибка.
            return Err(StarlarkGlueError::Eval(
                "nested evaluate_manifest is not supported on the same thread".to_string(),
            ));
        }
        CURRENT_STATE.with(|cell| {
            *cell.borrow_mut() = Some(state);
        });
        Ok(Self)
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        CURRENT_STATE.with(|cell| {
            cell.borrow_mut().take();
        });
    }
}

/// Хук-функция для starlark-evaluator'а: выставляется через
/// `before_stmt_for_dap` и вызывается перед каждым bytecode-statement'ом.
/// Проверяет `plan_ctx.deadline` и `cancel`; при превышении/отмене
/// возвращает `Err`, и evaluator прерывает eval_module с этой ошибкой.
struct DeadlineChecker {
    deadline: std::time::Instant,
    cancel: tokio_util::sync::CancellationToken,
}

impl<'a, 'e: 'a> starlark::eval::BeforeStmtFuncDyn<'a, 'e> for DeadlineChecker {
    fn call<'v>(
        &mut self,
        _span: starlark::codemap::FileSpanRef,
        _eval: &mut starlark::eval::Evaluator<'v, 'a, 'e>,
    ) -> starlark::Result<()> {
        if self.cancel.is_cancelled() {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "starlark evaluation cancelled"
            )));
        }
        if std::time::Instant::now() >= self.deadline {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "starlark evaluation exceeded deadline"
            )));
        }
        Ok(())
    }
}

/// Прочитать текущий state. Возвращает None, если вне `evaluate_manifest`.
/// Использовать ТОЛЬКО внутри native-функций и attribute-access'ов
/// Starlark-объектов, созданных нашим glue.
pub(crate) fn with_state<R>(f: impl FnOnce(&EvalState) -> R) -> Option<R> {
    CURRENT_STATE.with(|cell| {
        let slot = cell.borrow();
        slot.as_ref().map(|state| f(state.as_ref()))
    })
}

/// Default closure для тестов и контекстов, где template-рендеринг
/// не сконфигурирован: всегда возвращает ошибку с понятным сообщением.
pub fn default_template_fn() -> TemplateFn {
    Rc::new(|path: &str| {
        Err(anyhow::anyhow!(
            "template('{path}') called but no template renderer configured for this evaluator",
        ))
    })
}

/// Запустить Starlark-evaluation для entry-манифеста bundle.
///
/// Каждый вызов `apt.package(...)` / `file.content(...)` через native-globals
/// конвертируется в `Resource` и добавляется в `registry`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_manifest(
    bundle: &Bundle,
    primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
    inventory: serde_json::Value,
    facts: Rc<dyn FactsSource>,
    sensitive: Arc<SensitiveStore>,
    registry: Rc<RefCell<Registry>>,
    plan_ctx: PlanCtx,
    template_fn: TemplateFn,
) -> Result<(), StarlarkGlueError> {
    let entry_source = bundle.entry_manifest().ok_or_else(|| {
        BundleError::EntryNotFound(format!(
            "entry manifest '{}' not loaded from bundle",
            bundle.metadata.entry
        ))
    })?;

    let ast = AstModule::parse(
        &bundle.metadata.entry,
        entry_source.to_string(),
        &Dialect::Standard,
    )
    .map_err(|e| StarlarkGlueError::Syntax {
        file: bundle.metadata.entry.clone(),
        message: format!("{e}"),
    })?;

    let globals = build_globals();
    let builtins_module = build_builtins_module(&globals).map_err(|e| {
        StarlarkGlueError::Eval(format!("failed to build @bosun/builtins module: {e}"))
    })?;

    // Снимок plan_ctx до перемещения в EvalState — нужен для DeadlineChecker.
    let deadline = plan_ctx.deadline;
    let cancel = plan_ctx.cancel.clone();

    let state = Rc::new(EvalState {
        primitives,
        facts,
        sensitive,
        registry,
        plan_ctx,
        errors: RefCell::new(Vec::new()),
        template_fn,
    });

    let _guard = StateGuard::install(Rc::clone(&state))?;

    let module = Module::new();

    // Установить `inv` как module-level переменную.
    globals::install_inv(&module, inventory);

    let loader = BundleLoader::new(&builtins_module);

    let eval_result = {
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&loader);
        // F06: перед каждым bytecode-statement'ом DeadlineChecker проверяет
        // deadline/cancel; при превышении prevents бесконечный/тяжёлый
        // манифест от блокировки CLI. Хук вызывается из starlark-runtime,
        // не из нашего кода — отсюда `BeforeStmtFuncDyn` через `from`.
        let checker: Box<dyn starlark::eval::BeforeStmtFuncDyn> = Box::new(DeadlineChecker {
            deadline,
            cancel: cancel.clone(),
        });
        eval.before_stmt_for_dap(checker.into());
        eval.eval_module(ast, &globals)
    };

    if let Err(e) = eval_result {
        // Структурированная ошибка из state, если есть, информативнее.
        if let Some(first) = state.errors.borrow_mut().drain(..).next() {
            return Err(first);
        }
        return Err(StarlarkGlueError::Eval(format!("{e}")));
    }

    if let Some(first) = state.errors.borrow_mut().drain(..).next() {
        return Err(first);
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;

    use crate::bundle::Bundle;
    use crate::call_args::CallArgs;
    use crate::diff::{ChangeReport, Diff};
    use crate::facts::FactValue;
    use crate::primitive::{ApplyCtx, FactsSource, PlanCtx, Primitive, PrimitiveError};
    use crate::registry::Registry;
    use crate::resource::{Resource, ResourceId, ResourceKind};
    use crate::sensitive::SensitiveStore;

    use super::*;

    struct NoFacts;

    impl FactsSource for NoFacts {
        fn get(&self, name: &str) -> FactValue {
            FactValue::Unknown {
                reason: format!("test facts have no '{name}'"),
            }
        }
    }

    /// Mock-примитив: build_payload пишет args как-есть, чтобы тест мог
    /// проверить регистрацию ресурса.
    struct MockApt;

    impl Primitive for MockApt {
        fn type_name(&self) -> ResourceKind {
            ResourceKind::from_static("apt.package")
        }
        fn identity_keys(&self) -> &'static [&'static str] {
            &["name"]
        }
        fn build_payload(
            &self,
            args: &CallArgs,
            _ctx: &PlanCtx,
        ) -> Result<serde_json::Value, PrimitiveError> {
            let name = args
                .required_str("name")
                .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package: {e}")))?;
            let version = args
                .optional_str("version")
                .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package: {e}")))?;
            Ok(serde_json::json!({
                "name": name,
                "version": version,
            }))
        }
        fn plan(
            &self,
            _resource: &Resource,
            _facts: &dyn FactsSource,
            _ctx: &PlanCtx,
        ) -> Result<Diff, PrimitiveError> {
            Ok(Diff::NoChange)
        }
        fn apply(
            &self,
            _resource: &Resource,
            _diff: &Diff,
            _ctx: &ApplyCtx,
        ) -> Result<ChangeReport, PrimitiveError> {
            Ok(ChangeReport::no_change())
        }
    }

    /// Mock file.content: build_payload отражает имеющиеся аргументы как payload,
    /// в том числе уже подменённые glue'ем `content_sha256` и `content_size`.
    struct MockFile;

    impl Primitive for MockFile {
        fn type_name(&self) -> ResourceKind {
            ResourceKind::from_static("file.content")
        }
        fn identity_keys(&self) -> &'static [&'static str] {
            &["path"]
        }
        fn build_payload(
            &self,
            args: &CallArgs,
            _ctx: &PlanCtx,
        ) -> Result<serde_json::Value, PrimitiveError> {
            let path = args
                .required_str("path")
                .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?;
            let sha = args
                .required_str("content_sha256")
                .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?;
            let size = args
                .optional_u64("content_size")
                .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?
                .ok_or_else(|| {
                    PrimitiveError::InvalidPayload("file.content: missing content_size".to_string())
                })?;
            Ok(serde_json::json!({
                "path": path,
                "content_sha256": sha,
                "content_size": size,
            }))
        }
        fn plan(
            &self,
            _resource: &Resource,
            _facts: &dyn FactsSource,
            _ctx: &PlanCtx,
        ) -> Result<Diff, PrimitiveError> {
            Ok(Diff::NoChange)
        }
        fn apply(
            &self,
            _resource: &Resource,
            _diff: &Diff,
            _ctx: &ApplyCtx,
        ) -> Result<ChangeReport, PrimitiveError> {
            Ok(ChangeReport::no_change())
        }
    }

    fn plan_ctx() -> PlanCtx {
        PlanCtx {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: CancellationToken::new(),
        }
    }

    fn bundle_with_manifest(manifest_source: &str, defaults: serde_json::Value) -> Bundle {
        let tmp = tempfile::tempdir().unwrap();
        // keep TempDir alive by leaking it — bundle file content stays in tempdir
        // for the duration of the test; OS reclaims on process exit.
        let root = tmp.keep();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        std::fs::write(
            root.join("bundle.toml"),
            r#"
[bundle]
name = "test"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "manifests/main.star"
"#,
        )
        .unwrap();
        std::fs::write(root.join("manifests/main.star"), manifest_source).unwrap();
        std::fs::create_dir_all(root.join("templates")).unwrap();
        let mut bundle = Bundle::load_dir(&root).unwrap();
        bundle.defaults = defaults;
        bundle
    }

    fn primitives_with_mock_apt() -> Rc<HashMap<ResourceKind, Box<dyn Primitive>>> {
        let mut m: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        m.insert(ResourceKind::from_static("apt.package"), Box::new(MockApt));
        Rc::new(m)
    }

    #[test]
    fn apt_package_call_registers_resource() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "nginx")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            Rc::clone(&registry),
            ctx,
            default_template_fn(),
        )
        .unwrap();
        let reg = registry.borrow();
        assert_eq!(reg.all().len(), 1);
        let r = &reg.all()[0];
        assert_eq!(
            r.id,
            ResourceId::new(&ResourceKind::from_static("apt.package"), "nginx")
        );
        assert_eq!(r.payload["name"], serde_json::json!("nginx"));
    }

    #[test]
    fn apt_package_with_version_from_inventory() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "nginx", version = inv.nginx_version)
"#,
            serde_json::json!({"nginx_version": "1.18.0"}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let inventory = bundle.merge_inventory(serde_json::json!({}));
        evaluate_manifest(
            &bundle,
            primitives,
            inventory,
            facts,
            store,
            Rc::clone(&registry),
            ctx,
            default_template_fn(),
        )
        .unwrap();
        let reg = registry.borrow();
        let r = &reg.all()[0];
        assert_eq!(r.payload["version"], serde_json::json!("1.18.0"));
    }

    #[test]
    fn load_resolver_rejects_non_builtin_path() {
        let bundle = bundle_with_manifest(
            r#"
load("//some/module", "apt")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            default_template_fn(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("only @bosun/builtins is supported") || msg.contains("//some/module"),
            "expected loader error, got: {msg}"
        );
    }

    #[test]
    fn unknown_primitive_kind_returns_error() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "file")
file.content(path = "/tmp/x", contents = "x")
"#,
            serde_json::json!({}),
        );
        // primitives без file.content
        let primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>> = Rc::new(HashMap::new());
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            default_template_fn(),
        )
        .unwrap_err();
        match err {
            StarlarkGlueError::UnknownPrimitive(_) | StarlarkGlueError::Eval(_) => {}
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn missing_inventory_key_fails_evaluation() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = inv.nonexistent_key)
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            default_template_fn(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("nonexistent_key") || msg.contains("Object has no attribute"),
            "expected inventory error mentioning key, got: {msg}"
        );
    }

    #[test]
    fn duplicate_resource_id_is_error() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "nginx")
apt.package(name = "nginx")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            default_template_fn(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate") || msg.contains("apt.package:nginx"),
            "expected duplicate-id error, got: {msg}"
        );
    }

    #[test]
    fn handle_passed_via_depends_on() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
h = apt.package(name = "a")
apt.package(name = "b", depends_on = [h])
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            Rc::clone(&registry),
            ctx,
            default_template_fn(),
        )
        .unwrap();
        let reg = registry.borrow();
        let b = reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("apt.package"),
                "b",
            ))
            .unwrap();
        assert_eq!(b.depends_on.len(), 1);
        assert_eq!(
            b.depends_on[0],
            ResourceId::new(&ResourceKind::from_static("apt.package"), "a")
        );
    }

    fn primitives_with_mock_file() -> Rc<HashMap<ResourceKind, Box<dyn Primitive>>> {
        let mut m: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        m.insert(
            ResourceKind::from_static("file.content"),
            Box::new(MockFile),
        );
        Rc::new(m)
    }

    #[test]
    fn file_content_extracts_contents_into_sensitive_store() {
        // file.content(contents="hello") должен:
        // 1. убрать "contents" из аргументов до build_payload,
        // 2. положить sha256+size в payload,
        // 3. положить тело в SensitiveStore под ResourceId.
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "file")
file.content(path = "/etc/conf", contents = "hello")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_file();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            Arc::clone(&store),
            Rc::clone(&registry),
            ctx,
            default_template_fn(),
        )
        .unwrap();
        let reg = registry.borrow();
        assert_eq!(reg.all().len(), 1);
        let r = &reg.all()[0];
        assert_eq!(r.payload["path"], serde_json::json!("/etc/conf"));
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert_eq!(
            r.payload["content_sha256"],
            serde_json::json!("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        assert_eq!(r.payload["content_size"], serde_json::json!(5));
        // Контент НЕ в payload — только в store.
        assert!(r.payload.get("contents").is_none());

        let id = ResourceId::new(&ResourceKind::from_static("file.content"), "/etc/conf");
        let sensitive = store.take(&id).unwrap();
        assert_eq!(sensitive.into_inner(), "hello");
    }

    #[test]
    fn file_content_missing_contents_is_error() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "file")
file.content(path = "/etc/conf")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_file();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            default_template_fn(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("contents"),
            "expected contents-error, got: {msg}"
        );
    }

    #[test]
    fn template_closure_is_invoked_with_relative_path() {
        // Подменяем template_fn: возвращает echo строку, проверяем результат.
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "file", "template")
file.content(path = "/etc/conf", contents = template("hello.j2"))
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_file();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let template_fn: TemplateFn = Rc::new(|path: &str| Ok(format!("rendered:{path}")));
        evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            Arc::clone(&store),
            Rc::clone(&registry),
            ctx,
            template_fn,
        )
        .unwrap();

        let id = ResourceId::new(&ResourceKind::from_static("file.content"), "/etc/conf");
        let sensitive = store.take(&id).unwrap();
        assert_eq!(sensitive.into_inner(), "rendered:hello.j2");
    }

    #[test]
    fn template_closure_error_propagates_as_starlark_eval_error() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "template")
x = template("broken.j2")
"#,
            serde_json::json!({}),
        );
        let primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>> = Rc::new(HashMap::new());
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());
        let ctx = plan_ctx();
        let template_fn: TemplateFn = Rc::new(|_| Err(anyhow::anyhow!("synthetic render failure")));
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            template_fn,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("synthetic render failure") || msg.contains("broken.j2"),
            "expected template error to surface, got: {msg}"
        );
    }

    #[test]
    fn evaluation_aborts_on_deadline_for_heavy_manifest() {
        // F06 regression: for-цикл с большим range — легальная starlark
        // конструкция (без before_stmt-hook'а eval работает до полного
        // завершения). С deadline через DeadlineChecker eval прерывается
        // между statement'ами тела цикла.
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
def heavy():
    acc = 0
    for i in range(10000000):
        acc = acc + i
        acc = acc - 1
    return acc
_ = heavy()
apt.package(name = "nginx")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());

        // Очень короткий дедлайн: 50ms.
        let ctx = PlanCtx {
            deadline: Instant::now() + Duration::from_millis(50),
            cancel: CancellationToken::new(),
        };

        let started = Instant::now();
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            default_template_fn(),
        )
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "elapsed: {:?}",
            started.elapsed()
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("deadline") || msg.contains("cancel"),
            "expected deadline-related error, got: {msg}"
        );
    }

    #[test]
    fn evaluation_aborts_on_cancel_token() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
def heavy():
    acc = 0
    for i in range(10000000):
        acc = acc + i
        acc = acc - 1
    return acc
_ = heavy()
apt.package(name = "nginx")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());

        let cancel = CancellationToken::new();
        let cancel_for_thread = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel_for_thread.cancel();
        });
        let ctx = PlanCtx {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel,
        };

        let started = Instant::now();
        let err = evaluate_manifest(
            &bundle,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            default_template_fn(),
        )
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "elapsed: {:?}",
            started.elapsed()
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("cancel") || msg.contains("deadline"),
            "expected cancel-related error, got: {msg}"
        );
    }

    #[test]
    fn nested_evaluate_manifest_on_same_thread_returns_error() {
        // F06 secondary: re-entrant evaluate_manifest должен явно отказать,
        // не молча тратить state predecessor'а (раньше — debug_assert,
        // в release-build silent UB).
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "outer")
"#,
            serde_json::json!({}),
        );
        let primitives = primitives_with_mock_apt();
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let store = Arc::new(SensitiveStore::new());

        // Эмулируем «вложенность» через прямую установку state в
        // thread-local поверх ещё одного вызова. Самый простой способ —
        // запустить evaluate_manifest изнутри template_fn.
        let inner_bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "inner")
"#,
            serde_json::json!({}),
        );
        let primitives_for_inner = primitives_with_mock_apt();
        let inner_state = std::cell::RefCell::new(Some(inner_bundle));
        let inner_primitives = std::cell::RefCell::new(Some(primitives_for_inner));
        let inner_called = std::rc::Rc::new(std::cell::RefCell::new(false));
        let inner_err = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));

        let inner_called_clone = std::rc::Rc::clone(&inner_called);
        let inner_err_clone = std::rc::Rc::clone(&inner_err);
        let template_fn: TemplateFn = std::rc::Rc::new(move |_path: &str| {
            *inner_called_clone.borrow_mut() = true;
            // Пытаемся запустить evaluate_manifest внутри template-функции.
            // Должны получить StarlarkGlueError::Eval("nested...").
            let b = inner_state.borrow_mut().take().unwrap();
            let p = inner_primitives.borrow_mut().take().unwrap();
            let inner_reg = std::rc::Rc::new(std::cell::RefCell::new(Registry::new()));
            let inner_facts: std::rc::Rc<dyn FactsSource> = std::rc::Rc::new(NoFacts);
            let inner_store = std::sync::Arc::new(SensitiveStore::new());
            let result = evaluate_manifest(
                &b,
                p,
                serde_json::json!({}),
                inner_facts,
                inner_store,
                inner_reg,
                PlanCtx {
                    deadline: Instant::now() + Duration::from_secs(60),
                    cancel: CancellationToken::new(),
                },
                default_template_fn(),
            );
            match result {
                Err(StarlarkGlueError::Eval(msg)) if msg.contains("nested") => {
                    *inner_err_clone.borrow_mut() = Some(msg);
                    Ok(String::new())
                }
                Err(other) => Err(anyhow::anyhow!("expected nested error, got: {other}")),
                Ok(()) => Err(anyhow::anyhow!("expected nested error, got Ok")),
            }
        });

        let bundle_with_template = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt", "template")
x = template("dummy.j2")
apt.package(name = "outer")
"#,
            serde_json::json!({}),
        );
        let _ = bundle;
        let ctx = plan_ctx();
        evaluate_manifest(
            &bundle_with_template,
            primitives,
            serde_json::json!({}),
            facts,
            store,
            registry,
            ctx,
            template_fn,
        )
        .unwrap();
        assert!(
            *inner_called.borrow(),
            "template-fn должна была быть вызвана"
        );
        assert!(
            inner_err.borrow().as_ref().is_some(),
            "ожидаем nested-эрор из inner evaluate_manifest"
        );
    }
}
