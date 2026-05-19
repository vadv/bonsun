//! Интеграционный слой между Starlark и bosun.
//!
//! Архитектура (см. spec «bundle architecture rev 2»):
//! - Native globals `apt`, `file`, `runr`, `template`, `inventory`, `tags`,
//!   `inv` — собираются в `Globals` и в `FrozenModule` `@bosun/builtins`.
//! - `BundleLoader` отдаёт `@bosun/builtins`, `@roles/<name>`, `@lib/<name>`.
//! - На время `evaluate_manifest` thread-local `CURRENT_STATE` хранит
//!   `Rc<EvalState>`. Native-функции и attribute access читают state через
//!   `with_state(...)`. После возврата thread-local очищается.
//! - `current_module` stack обновляется через RAII `ModuleStackGuard` —
//!   push при входе в load_role_or_lib, pop при выходе/Drop'е.
//!
//! `allow(unsafe_code)`: derive-макросы `Trace`/`Freeze` из `starlark_derive`
//! генерируют `unsafe impl`-блоки. Мы не пишем `unsafe` сами — это требование
//! API starlark для опубликованных Value-типов.

#![allow(unsafe_code)]

mod globals;
mod inv_object;
mod load_resolver;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::syntax::AstModule;

use crate::bundle::{Bundle, BundleError};
use crate::primitive::{FactsSource, PlanCtx, Primitive};
use crate::registry::Registry;
use crate::resource::ResourceKind;
use crate::sensitive::SensitiveStore;

pub use globals::{build_builtins_module, build_globals};
pub(crate) use load_resolver::BundleLoader;

/// Closure для рендера шаблона. Аргументы:
/// - `resolved_path`: канонический absolute путь к файлу .j2 под bundle root
///   (уже валидированный через path_safety; glue передаёт готовый путь).
/// - `original`: исходная строка из `template("...")` — полезна для логов.
/// - `extra_context`: kwargs, переданные в `template(path, **kwargs)`.
///   CLI закладывает в свою closure базовые facts/inv; этот аргумент
///   добавляется поверх (kwargs побеждают). Часто будет пустой JSON-объект.
///
/// CLI собирает closure через `bosun-primitives::render_template` с
/// захваченным facts. Glue-слой вычисляет defining модуль через walk
/// call-stack и резолвит шаблон до вызова closure.
pub type TemplateFn =
    Rc<dyn Fn(&std::path::Path, &str, &serde_json::Value) -> Result<String, anyhow::Error>>;

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
    #[error("load cycle detected: {0}")]
    LoadCycle(String),
}

/// State, разделяемое между всеми Starlark-объектами одного запуска
/// evaluate_manifest. Хранится в thread-local на время вызова.
pub(crate) struct EvalState {
    pub(crate) bundle: Rc<Bundle>,
    pub(crate) primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
    /// FactsSource — read-only доступ к фактам. Сейчас используется
    /// диспатчером `service.unit` (Phase F): читает `init_system`, чтобы
    /// выбрать между `runr.service` и `systemd.service`. Примитивы напрямую
    /// факты в build_payload пока не читают — им хватает materialized JSON
    /// из template-closure.
    pub(crate) facts: Rc<dyn FactsSource>,
    /// Хранилище секретов для file.content.contents.
    pub(crate) sensitive: Arc<SensitiveStore>,
    pub(crate) registry: Rc<RefCell<Registry>>,
    pub(crate) plan_ctx: PlanCtx,
    pub(crate) errors: RefCell<Vec<StarlarkGlueError>>,
    pub(crate) template_fn: TemplateFn,
    pub(crate) tags: HashSet<String>,
    /// Стек canonical путей загружаемых модулей. Пушится при входе в
    /// `BundleLoader::load_role_or_lib` через `ModuleStackGuard`. Используется
    /// как backup для call-stack-walk, если starlark frame не отдаёт codemap.
    pub(crate) current_module: RefCell<Vec<PathBuf>>,
    /// Кэш `inventory.load`: канонический путь yaml → распарсенный JSON.
    /// Заполняется лениво при первой загрузке; в следующих вызовах того же
    /// path парсинг не повторяется.
    pub(crate) inventory_cache: RefCell<HashMap<PathBuf, serde_json::Value>>,
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

/// RAII-guard для стека current_module: push на конструировании, pop на drop.
/// При панике в eval'е drop гарантирует, что стек консистентен и следующий
/// template() не резолвится из неправильной директории.
pub(crate) struct ModuleStackGuard {
    state: Rc<EvalState>,
}

impl ModuleStackGuard {
    pub(crate) fn push(state: Rc<EvalState>, module: PathBuf) -> Self {
        state.current_module.borrow_mut().push(module);
        Self { state }
    }
}

impl Drop for ModuleStackGuard {
    fn drop(&mut self) {
        self.state.current_module.borrow_mut().pop();
    }
}

/// Хук-функция для starlark-evaluator'а. Срабатывает перед каждым
/// statement'ом и прерывает выполнение, если истёк deadline или
/// сработал cancellation token. Применяется ко всем evaluator'ам —
/// и для entry-манифеста, и для loaded role/lib модулей; см.
/// `install_deadline_checker`.
pub(crate) struct DeadlineChecker {
    pub(crate) deadline: std::time::Instant,
    pub(crate) cancel: tokio_util::sync::CancellationToken,
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

/// Устанавливает `DeadlineChecker` в evaluator'е. Один общий helper для
/// entry-манифеста и для loaded модулей через `BundleLoader::parse_and_eval`:
/// раньше loaded-модули создавались без чекера, и infinite loop в
/// `_lib/foo.star` мог повесить run, игнорируя --deadline-sec.
pub(crate) fn install_deadline_checker<'a, 'e>(
    eval: &mut Evaluator<'_, 'a, 'e>,
    deadline: std::time::Instant,
    cancel: tokio_util::sync::CancellationToken,
) {
    let checker: Box<dyn starlark::eval::BeforeStmtFuncDyn> =
        Box::new(DeadlineChecker { deadline, cancel });
    eval.before_stmt_for_dap(checker.into());
}

/// Прочитать текущий state.
pub(crate) fn with_state<R>(f: impl FnOnce(&EvalState) -> R) -> Option<R> {
    CURRENT_STATE.with(|cell| {
        let slot = cell.borrow();
        slot.as_ref().map(|state| f(state.as_ref()))
    })
}

/// Получить Rc клон текущего state.
pub(crate) fn current_state() -> Option<Rc<EvalState>> {
    CURRENT_STATE.with(|cell| cell.borrow().as_ref().map(Rc::clone))
}

/// Default closure для тестов и контекстов, где template-рендеринг
/// не сконфигурирован.
pub fn default_template_fn() -> TemplateFn {
    Rc::new(|_resolved, path, _ctx| {
        Err(anyhow::anyhow!(
            "template('{path}') called but no template renderer configured for this evaluator",
        ))
    })
}

/// Конфигурация запуска `evaluate_manifest`. Собрана в один объект, чтобы не
/// тянуть 8+ позиционных параметров через все callsite'ы.
pub struct EvaluatorConfig {
    pub bundle: Rc<Bundle>,
    pub primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
    pub facts: Rc<dyn FactsSource>,
    pub sensitive: Arc<SensitiveStore>,
    pub registry: Rc<RefCell<Registry>>,
    pub plan_ctx: PlanCtx,
    pub template_fn: TemplateFn,
    pub tags: HashSet<String>,
}

/// Запустить Starlark-evaluation для entry-манифеста bundle.
pub fn evaluate_manifest(config: EvaluatorConfig) -> Result<(), StarlarkGlueError> {
    let EvaluatorConfig {
        bundle,
        primitives,
        facts,
        sensitive,
        registry,
        plan_ctx,
        template_fn,
        tags,
    } = config;

    let entry_path = bundle.entry.clone();
    let entry_source = std::fs::read_to_string(&entry_path).map_err(|e| {
        StarlarkGlueError::Bundle(BundleError::Io {
            path: entry_path.to_string_lossy().into_owned(),
            source: e,
        })
    })?;

    let entry_name = entry_path.to_string_lossy().into_owned();
    let ast = AstModule::parse(&entry_name, entry_source, &bundle_dialect()).map_err(|e| {
        StarlarkGlueError::Syntax {
            file: entry_name.clone(),
            message: format!("{e}"),
        }
    })?;

    let globals = build_globals();
    let builtins_module = build_builtins_module(&globals).map_err(|e| {
        StarlarkGlueError::Eval(format!("failed to build @bosun/builtins module: {e}"))
    })?;

    let deadline = plan_ctx.deadline;
    let cancel = plan_ctx.cancel.clone();

    let state = Rc::new(EvalState {
        bundle: Rc::clone(&bundle),
        primitives,
        facts,
        sensitive,
        registry,
        plan_ctx,
        errors: RefCell::new(Vec::new()),
        template_fn,
        tags,
        current_module: RefCell::new(Vec::new()),
        inventory_cache: RefCell::new(HashMap::new()),
    });

    let _guard = StateGuard::install(Rc::clone(&state))?;

    // Стек current_module начинаем с entry — это нужно, если template()
    // вызывается из top-level кода main.star (он попадёт в reject через
    // TemplateFromEntry). Иначе fallback на codemap пойдёт мимо.
    let _entry_guard = ModuleStackGuard::push(Rc::clone(&state), entry_path.clone());

    let module = Module::new();
    let loader = BundleLoader::new(&builtins_module, Rc::clone(&state));

    let eval_result = {
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&loader);
        install_deadline_checker(&mut eval, deadline, cancel.clone());
        eval.eval_module(ast, &globals)
    };

    if let Err(e) = eval_result {
        // Если в state накопилась структурная ошибка, она информативнее.
        if let Some(first) = state.errors.borrow_mut().drain(..).next() {
            return Err(first);
        }
        let raw = format!("{e}");
        // Маппим Starlark-сообщения об ошибках в наши доменные варианты
        // там, где это даёт более полезный диагноз.
        return Err(classify_eval_error(&raw));
    }

    if let Some(first) = state.errors.borrow_mut().drain(..).next() {
        return Err(first);
    }

    Ok(())
}

/// Starlark dialect, разрешающий top-level if/for. Это нужно для
/// идиомы «после load() сразу `tags.require_one_of(...)`, потом
/// `if tags.has(): inv = inventory.merge(...)`» из spec.
pub(crate) fn bundle_dialect() -> starlark::syntax::Dialect {
    starlark::syntax::Dialect::Extended
}

/// Попытаться классифицировать ошибку eval по тексту сообщения. Это hack,
/// потому что starlark::Error не выставляет структурный тип для ошибок из
/// FileLoader / get_option. Тексты совпадают с тем, что starlark формирует
/// для `EnvironmentError::ModuleSymbolIsNotExported`.
fn classify_eval_error(msg: &str) -> StarlarkGlueError {
    // Privacy enforcement: starlark возвращает "Module symbol is not
    // exported: NAME". Маппим в наш PrivateSymbol.
    if let Some(rest) = msg.find("is not exported: ") {
        let name = msg[rest + "is not exported: ".len()..]
            .split(|c: char| c.is_whitespace() || c == '`' || c == '\'')
            .next()
            .unwrap_or("")
            .trim_matches(|c: char| c == '`' || c == '\'')
            .to_string();
        if !name.is_empty() {
            return StarlarkGlueError::Bundle(BundleError::PrivateSymbol {
                symbol: name,
                module: PathBuf::new(),
            });
        }
    }
    if msg.contains("Cyclic load") || msg.contains("cyclic load") {
        return StarlarkGlueError::LoadCycle(msg.to_string());
    }
    StarlarkGlueError::Eval(msg.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
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

    /// Подготовить bundle с произвольной структурой файлов. `files` —
    /// пары (relative-path, content). bundle.toml пишется автоматически, если
    /// не задан в `files`.
    fn make_bundle_with_files(files: &[(&str, &str)]) -> Bundle {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.keep();
        let has_bundle_toml = files.iter().any(|(p, _)| *p == "bundle.toml");
        if !has_bundle_toml {
            std::fs::write(
                root.join("bundle.toml"),
                r#"
[bundle]
name = "test"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "main.star"

[bundle.inventory]
default_merge_strategy = "deep_map_replace_list"
"#,
            )
            .unwrap();
        }
        for (rel, body) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        Bundle::load_dir(&root).unwrap()
    }

    fn bundle_with_manifest(manifest_source: &str) -> Bundle {
        make_bundle_with_files(&[("main.star", manifest_source)])
    }

    fn primitives_with_mock_apt() -> Rc<HashMap<ResourceKind, Box<dyn Primitive>>> {
        let mut m: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        m.insert(ResourceKind::from_static("apt.package"), Box::new(MockApt));
        Rc::new(m)
    }

    fn primitives_with_mock_file() -> Rc<HashMap<ResourceKind, Box<dyn Primitive>>> {
        let mut m: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        m.insert(
            ResourceKind::from_static("file.content"),
            Box::new(MockFile),
        );
        Rc::new(m)
    }

    fn run(bundle: Bundle, primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>) -> EvalRun {
        run_with(bundle, primitives, default_template_fn(), HashSet::new())
    }

    fn run_with(
        bundle: Bundle,
        primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
        template_fn: TemplateFn,
        tags: HashSet<String>,
    ) -> EvalRun {
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let sensitive = Arc::new(SensitiveStore::new());
        let plan_ctx = plan_ctx();
        let config = EvaluatorConfig {
            bundle: Rc::new(bundle),
            primitives,
            facts,
            sensitive: Arc::clone(&sensitive),
            registry: Rc::clone(&registry),
            plan_ctx,
            template_fn,
            tags,
        };
        let result = evaluate_manifest(config);
        EvalRun {
            result,
            registry,
            sensitive,
        }
    }

    struct EvalRun {
        result: Result<(), StarlarkGlueError>,
        registry: Rc<RefCell<Registry>>,
        sensitive: Arc<SensitiveStore>,
    }

    #[test]
    fn apt_package_call_registers_resource() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "nginx")
"#,
        );
        let run = run(bundle, primitives_with_mock_apt());
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert_eq!(reg.all().len(), 1);
        let r = &reg.all()[0];
        assert_eq!(
            r.id,
            ResourceId::new(&ResourceKind::from_static("apt.package"), "nginx")
        );
    }

    #[test]
    fn tags_have_returns_membership() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt", "tags")
if tags.has("production"):
    apt.package(name = "prod-pkg")
"#,
        );
        let mut active = HashSet::new();
        active.insert("production".to_string());
        let run = run_with(
            bundle,
            primitives_with_mock_apt(),
            default_template_fn(),
            active,
        );
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert_eq!(reg.all().len(), 1);
    }

    #[test]
    fn tags_require_one_of_fails_when_no_intersection() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "tags")
tags.require_one_of("production", "staging")
"#,
        );
        let run = run_with(
            bundle,
            Rc::new(HashMap::new()),
            default_template_fn(),
            HashSet::new(),
        );
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("expected one of"));
    }

    #[test]
    fn tags_active_returns_sorted_list() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt", "tags")
active = tags.active()
apt.package(name = active[0])
"#,
        );
        let mut tags = HashSet::new();
        tags.insert("zebra".to_string());
        tags.insert("alpha".to_string());
        let run = run_with(
            bundle,
            primitives_with_mock_apt(),
            default_template_fn(),
            tags,
        );
        run.result.unwrap();
        let reg = run.registry.borrow();
        // sorted("alpha","zebra")[0] = "alpha"
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("apt.package"),
                "alpha"
            ))
            .is_some());
    }

    #[test]
    fn inventory_load_reads_yaml() {
        let bundle = make_bundle_with_files(&[
            (
                "main.star",
                r#"
load("@bosun/builtins", "apt", "inventory")
inv_data = inventory.read("inventory/base.yaml")
apt.package(name = inv_data["pkg"])
"#,
            ),
            ("inventory/base.yaml", "pkg: redis\n"),
        ]);
        let run = run(bundle, primitives_with_mock_apt());
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("apt.package"),
                "redis"
            ))
            .is_some());
    }

    #[test]
    fn inventory_merge_default_strategy_from_bundle_toml() {
        let bundle = make_bundle_with_files(&[
            (
                "main.star",
                r#"
load("@bosun/builtins", "apt", "inventory")
a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge(a, b)
apt.package(name = m["pkg"])
"#,
            ),
            ("inventory/a.yaml", "pkg: from-a\nother: x\n"),
            ("inventory/b.yaml", "pkg: from-b\n"),
        ]);
        let run = run(bundle, primitives_with_mock_apt());
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("apt.package"),
                "from-b"
            ))
            .is_some());
    }

    #[test]
    fn inventory_merge_explicit_strategy_replace() {
        let bundle = make_bundle_with_files(&[
            (
                "main.star",
                r#"
load("@bosun/builtins", "apt", "inventory")
a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge(a, b, strategy = "replace")
apt.package(name = m["pkg"])
"#,
            ),
            ("inventory/a.yaml", "pkg: from-a\n"),
            ("inventory/b.yaml", "pkg: from-b\n"),
        ]);
        let run = run(bundle, primitives_with_mock_apt());
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("apt.package"),
                "from-b"
            ))
            .is_some());
    }

    #[test]
    fn inventory_merge_without_default_and_without_argument_fails() {
        let bundle = make_bundle_with_files(&[
            (
                "bundle.toml",
                r#"
[bundle]
name = "test"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "main.star"
"#,
            ),
            (
                "main.star",
                r#"
load("@bosun/builtins", "inventory")
a = inventory.read("inventory/a.yaml")
b = inventory.read("inventory/b.yaml")
m = inventory.merge(a, b)
"#,
            ),
            ("inventory/a.yaml", "pkg: a\n"),
            ("inventory/b.yaml", "pkg: b\n"),
        ]);
        let run = run(bundle, primitives_with_mock_apt());
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("missing default merge strategy"));
    }

    #[test]
    fn role_load_evaluates_role_module() {
        let bundle = make_bundle_with_files(&[
            (
                "main.star",
                r#"
load("@bosun/builtins", "apt")
load("@roles/myrole", "configure")
configure()
"#,
            ),
            (
                "roles/myrole/main.star",
                r#"
load("@bosun/builtins", "apt")

def configure():
    apt.package(name = "from-role")
"#,
            ),
        ]);
        let run = run(bundle, primitives_with_mock_apt());
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("apt.package"),
                "from-role"
            ))
            .is_some());
    }

    #[test]
    fn lib_load_evaluates_lib_module() {
        let bundle = make_bundle_with_files(&[
            (
                "main.star",
                r#"
load("@bosun/builtins", "apt")
load("@lib/helpers", "do_something")
do_something()
"#,
            ),
            (
                "_lib/helpers/main.star",
                r#"
load("@bosun/builtins", "apt")

def do_something():
    apt.package(name = "from-lib")
"#,
            ),
        ]);
        let run = run(bundle, primitives_with_mock_apt());
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("apt.package"),
                "from-lib"
            ))
            .is_some());
    }

    #[test]
    fn private_symbol_in_role_cannot_be_imported() {
        let bundle = make_bundle_with_files(&[
            (
                "main.star",
                r#"
load("@roles/myrole", "_private")
"#,
            ),
            (
                "roles/myrole/main.star",
                r#"
def _private():
    pass
"#,
            ),
        ]);
        let run = run(bundle, primitives_with_mock_apt());
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("_private") || msg.contains("private"),
            "expected private-symbol error, got: {msg}"
        );
    }

    #[test]
    fn template_called_from_root_entry_is_rejected() {
        let bundle = make_bundle_with_files(&[(
            "main.star",
            r#"
load("@bosun/builtins", "template")
template("anything.j2")
"#,
        )]);
        let template_fn: TemplateFn = Rc::new(|_resolved, _path, _ctx| Ok(String::new()));
        let run = run_with(
            bundle,
            primitives_with_mock_apt(),
            template_fn,
            HashSet::new(),
        );
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("main.star") || msg.contains("bundle entry"),
            "expected entry-template rejection, got: {msg}"
        );
    }

    #[test]
    fn template_inside_role_resolves_to_role_templates() {
        let bundle = make_bundle_with_files(&[
            (
                "main.star",
                r#"
load("@roles/myrole", "configure")
configure()
"#,
            ),
            (
                "roles/myrole/main.star",
                r#"
load("@bosun/builtins", "file", "template")

def configure():
    file.content(path = "/etc/x", contents = template("body.j2"))
"#,
            ),
            ("roles/myrole/templates/body.j2", "hello"),
        ]);
        let captured: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
        let captured_clone = Rc::clone(&captured);
        let template_fn: TemplateFn = Rc::new(
            move |resolved: &std::path::Path, _path: &str, _ctx: &serde_json::Value| {
                *captured_clone.borrow_mut() = Some(resolved.to_path_buf());
                Ok("rendered".to_string())
            },
        );
        let run = run_with(
            bundle,
            primitives_with_mock_file(),
            template_fn,
            HashSet::new(),
        );
        run.result.unwrap();
        let captured = captured.borrow();
        let resolved = captured.as_ref().unwrap();
        // template("body.j2") из роли myrole резолвится в roles/myrole/templates/body.j2.
        assert!(
            resolved.ends_with("roles/myrole/templates/body.j2"),
            "got resolved: {resolved:?}"
        );
    }

    #[test]
    fn file_content_extracts_contents_into_sensitive_store() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "file")
file.content(path = "/etc/conf", contents = "hello")
"#,
        );
        let run = run(bundle, primitives_with_mock_file());
        run.result.unwrap();
        let id = ResourceId::new(&ResourceKind::from_static("file.content"), "/etc/conf");
        let sensitive = run.sensitive.take(&id).unwrap();
        assert_eq!(sensitive.into_inner(), "hello");
    }

    #[test]
    fn load_resolver_rejects_non_builtin_path() {
        let bundle = bundle_with_manifest(
            r#"
load("//some/module", "apt")
"#,
        );
        let run = run(bundle, primitives_with_mock_apt());
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unsupported") || msg.contains("//some/module"));
    }

    #[test]
    fn duplicate_resource_id_is_error() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "nginx")
apt.package(name = "nginx")
"#,
        );
        let run = run(bundle, primitives_with_mock_apt());
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("duplicate") || msg.contains("apt.package:nginx"));
    }

    /// Источник фактов с фиксированным значением `init_system`. Используется
    /// тестами `service.unit`, где исход dispatch зависит только от этого факта.
    struct FixedInitSystem(Option<&'static str>);

    impl FactsSource for FixedInitSystem {
        fn get(&self, name: &str) -> FactValue {
            if name == "init_system" {
                match self.0 {
                    Some(value) => FactValue::Known(serde_json::Value::String(value.to_string())),
                    None => FactValue::Unknown {
                        reason: "init_system intentionally unset for test".into(),
                    },
                }
            } else {
                FactValue::Unknown {
                    reason: format!("test facts have no '{name}'"),
                }
            }
        }
    }

    /// Stub-примитив для `runr.service`: ничего не делает, нужен только
    /// для регистрации в реестре через `register_primitive_call`.
    struct MockRunrService;

    impl Primitive for MockRunrService {
        fn type_name(&self) -> ResourceKind {
            ResourceKind::from_static("runr.service")
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
                .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
            let state = args
                .required_str("state")
                .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
            Ok(serde_json::json!({"name": name, "state": state}))
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

    /// Stub-примитив для `systemd.service`: симметричен `MockRunrService`.
    struct MockSystemdService;

    impl Primitive for MockSystemdService {
        fn type_name(&self) -> ResourceKind {
            ResourceKind::from_static("systemd.service")
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
                .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
            let state = args
                .required_str("state")
                .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
            Ok(serde_json::json!({"name": name, "state": state}))
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

    /// Регистрируем оба service-примитива в одном HashMap, чтобы тесты могли
    /// проверять, какой из них реально вызвался при разных значениях факта.
    fn primitives_with_mock_services() -> Rc<HashMap<ResourceKind, Box<dyn Primitive>>> {
        let mut m: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        m.insert(
            ResourceKind::from_static("runr.service"),
            Box::new(MockRunrService),
        );
        m.insert(
            ResourceKind::from_static("systemd.service"),
            Box::new(MockSystemdService),
        );
        Rc::new(m)
    }

    /// Запустить eval с пользовательским FactsSource. Нужен тестам
    /// `service.unit`, у которых поведение зависит от факта `init_system`.
    fn run_with_facts(
        bundle: Bundle,
        primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
        facts: Rc<dyn FactsSource>,
    ) -> EvalRun {
        let registry = Rc::new(RefCell::new(Registry::new()));
        let sensitive = Arc::new(SensitiveStore::new());
        let plan_ctx = plan_ctx();
        let config = EvaluatorConfig {
            bundle: Rc::new(bundle),
            primitives,
            facts,
            sensitive: Arc::clone(&sensitive),
            registry: Rc::clone(&registry),
            plan_ctx,
            template_fn: default_template_fn(),
            tags: HashSet::new(),
        };
        let result = evaluate_manifest(config);
        EvalRun {
            result,
            registry,
            sensitive,
        }
    }

    #[test]
    fn service_unit_dispatches_to_systemd_for_systemd_init() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "service")
service.unit(name = "nginx", state = "running")
"#,
        );
        let facts: Rc<dyn FactsSource> = Rc::new(FixedInitSystem(Some("systemd")));
        let run = run_with_facts(bundle, primitives_with_mock_services(), facts);
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert_eq!(reg.all().len(), 1);
        let r = &reg.all()[0];
        assert_eq!(
            r.id,
            ResourceId::new(&ResourceKind::from_static("systemd.service"), "nginx")
        );
    }

    #[test]
    fn service_unit_dispatches_to_systemd_for_mixed_init() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "service")
service.unit(name = "nginx", state = "running")
"#,
        );
        let facts: Rc<dyn FactsSource> = Rc::new(FixedInitSystem(Some("mixed-systemd-runr")));
        let run = run_with_facts(bundle, primitives_with_mock_services(), facts);
        run.result.unwrap();
        let reg = run.registry.borrow();
        // В смешанной конфигурации primary = systemd, согласно дизайну Phase F.
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("systemd.service"),
                "nginx"
            ))
            .is_some());
        assert!(reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("runr.service"),
                "nginx"
            ))
            .is_none());
    }

    #[test]
    fn service_unit_dispatches_to_runr_for_runr_init() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "service")
service.unit(name = "postgres", state = "running")
"#,
        );
        let facts: Rc<dyn FactsSource> = Rc::new(FixedInitSystem(Some("runr")));
        let run = run_with_facts(bundle, primitives_with_mock_services(), facts);
        run.result.unwrap();
        let reg = run.registry.borrow();
        assert_eq!(reg.all().len(), 1);
        let r = &reg.all()[0];
        assert_eq!(
            r.id,
            ResourceId::new(&ResourceKind::from_static("runr.service"), "postgres")
        );
    }

    #[test]
    fn service_unit_fails_on_unsupported_init() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "service")
service.unit(name = "x", state = "running")
"#,
        );
        let facts: Rc<dyn FactsSource> = Rc::new(FixedInitSystem(Some("openrc")));
        let run = run_with_facts(bundle, primitives_with_mock_services(), facts);
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unsupported init_system") && msg.contains("openrc"),
            "expected unsupported init_system error, got: {msg}"
        );
    }

    #[test]
    fn service_unit_fails_when_init_system_unknown() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "service")
service.unit(name = "x", state = "running")
"#,
        );
        let facts: Rc<dyn FactsSource> = Rc::new(FixedInitSystem(None));
        let run = run_with_facts(bundle, primitives_with_mock_services(), facts);
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("init_system fact unknown"),
            "expected init_system unknown error, got: {msg}"
        );
    }

    #[test]
    fn service_unit_rejects_init_specific_kwarg() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "service")
service.unit(name = "x", state = "running", cgroup_procs_path = "/sys/fs/cgroup")
"#,
        );
        let facts: Rc<dyn FactsSource> = Rc::new(FixedInitSystem(Some("runr")));
        let run = run_with_facts(bundle, primitives_with_mock_services(), facts);
        let err = run.result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unexpected keyword argument") && msg.contains("cgroup_procs_path"),
            "expected unexpected-kwarg error, got: {msg}"
        );
    }

    #[test]
    fn service_unit_propagates_restart_on_handle_list() {
        // restart_on есть в allow-листе service.unit и должен сохраниться
        // в Resource. Берём handle от apt.package как источник notify.
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt", "service")
trigger = apt.package(name = "nginx")
service.unit(name = "nginx", state = "running", restart_on = [trigger])
"#,
        );
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(ResourceKind::from_static("apt.package"), Box::new(MockApt));
        primitives.insert(
            ResourceKind::from_static("systemd.service"),
            Box::new(MockSystemdService),
        );
        let primitives = Rc::new(primitives);
        let facts: Rc<dyn FactsSource> = Rc::new(FixedInitSystem(Some("systemd")));
        let run = run_with_facts(bundle, primitives, facts);
        run.result.unwrap();
        let reg = run.registry.borrow();
        let svc = reg
            .get(&ResourceId::new(
                &ResourceKind::from_static("systemd.service"),
                "nginx",
            ))
            .unwrap();
        assert_eq!(svc.restart_on.len(), 1);
        assert_eq!(
            svc.restart_on[0],
            ResourceId::new(&ResourceKind::from_static("apt.package"), "nginx"),
        );
    }

    #[test]
    fn evaluation_aborts_on_deadline_for_heavy_manifest() {
        let bundle = bundle_with_manifest(
            r#"
load("@bosun/builtins", "apt")
def heavy():
    acc = 0
    for i in range(10000000):
        acc = acc + i
    return acc
_ = heavy()
"#,
        );
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts: Rc<dyn FactsSource> = Rc::new(NoFacts);
        let sensitive = Arc::new(SensitiveStore::new());
        let plan_ctx = PlanCtx {
            deadline: Instant::now() + Duration::from_millis(50),
            cancel: CancellationToken::new(),
        };
        let started = Instant::now();
        let config = EvaluatorConfig {
            bundle: Rc::new(bundle),
            primitives: primitives_with_mock_apt(),
            facts,
            sensitive,
            registry,
            plan_ctx,
            template_fn: default_template_fn(),
            tags: HashSet::new(),
        };
        let err = evaluate_manifest(config).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        let msg = format!("{err}");
        assert!(msg.contains("deadline") || msg.contains("cancel"));
    }
}
