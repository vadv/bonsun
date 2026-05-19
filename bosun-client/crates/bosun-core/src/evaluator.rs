//! Evaluator — обёртка над `starlark_glue::evaluate_manifest`.
//!
//! Назначение: спрятать механику `Rc<RefCell<Registry>>` / `Rc<dyn FactsSource>`,
//! необходимую thread-local'у Starlark-glue, от вызывающего кода. Снаружи
//! Evaluator принимает `FactsSnapshot` по значению и возвращает свежий
//! `Registry`.
//!
//! Согласно спеке («Evaluator и Orchestrator»): Evaluator отвечает только
//! за «Starlark → Registry». Никаких apply, никаких побочных эффектов вне
//! Starlark-evaluation.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::bundle::Bundle;
use crate::primitive::{FactsSource, PlanCtx, Primitive};
use crate::registry::Registry;
use crate::resource::ResourceKind;
use crate::sensitive::SensitiveStore;
use crate::starlark_glue::{evaluate_manifest, EvaluatorConfig, StarlarkGlueError, TemplateFn};

pub struct Evaluator {
    bundle: Bundle,
    primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
}

impl Evaluator {
    pub fn new(bundle: Bundle, primitives: HashMap<ResourceKind, Box<dyn Primitive>>) -> Self {
        Self {
            bundle,
            primitives: Rc::new(primitives),
        }
    }

    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    /// Запустить Starlark-eval entry-манифеста с указанным набором тэгов.
    pub fn evaluate<F>(
        &self,
        facts: F,
        sensitive: Arc<SensitiveStore>,
        template_fn: TemplateFn,
        plan_ctx: PlanCtx,
        tags: HashSet<String>,
    ) -> Result<Registry, StarlarkGlueError>
    where
        F: FactsSource + 'static,
    {
        let registry = Rc::new(RefCell::new(Registry::new()));
        let facts_rc: Rc<dyn FactsSource> = Rc::new(facts);

        let config = EvaluatorConfig {
            bundle: Rc::new(self.bundle.clone()),
            primitives: Rc::clone(&self.primitives),
            facts: facts_rc,
            sensitive,
            registry: Rc::clone(&registry),
            plan_ctx,
            template_fn,
            tags,
        };
        evaluate_manifest(config)?;

        match Rc::try_unwrap(registry) {
            Ok(cell) => Ok(cell.into_inner()),
            Err(rc) => {
                let cell = rc.borrow();
                let mut out = Registry::new();
                for r in cell.all() {
                    let _ = out.add(r.clone());
                }
                Ok(out)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;

    use crate::bundle::Bundle;
    use crate::call_args::CallArgs;
    use crate::diff::{ChangeReport, Diff};
    use crate::facts::FactValue;
    use crate::primitive::{ApplyCtx, FactsSource, PlanCtx, Primitive, PrimitiveError};
    use crate::resource::{Resource, ResourceId, ResourceKind};
    use crate::sensitive::SensitiveStore;
    use crate::starlark_glue::default_template_fn;

    use super::*;

    struct NoFacts;

    impl FactsSource for NoFacts {
        fn get(&self, name: &str) -> FactValue {
            FactValue::Unknown {
                reason: format!("no fact '{name}'"),
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
            Ok(serde_json::json!({ "name": name }))
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

    fn make_bundle(manifest_source: &str) -> Bundle {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.keep();
        std::fs::write(
            root.join("bundle.toml"),
            r#"
[bundle]
name = "test"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "main.star"
"#,
        )
        .unwrap();
        std::fs::write(root.join("main.star"), manifest_source).unwrap();
        Bundle::load_dir(&root).unwrap()
    }

    #[test]
    fn evaluate_returns_populated_registry() {
        let bundle = make_bundle(
            r#"
load("@bosun/builtins", "apt")
apt.package(name = "nginx")
apt.package(name = "curl")
"#,
        );
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(ResourceKind::from_static("apt.package"), Box::new(MockApt));

        let evaluator = Evaluator::new(bundle, primitives);
        let store = Arc::new(SensitiveStore::new());
        let registry = evaluator
            .evaluate(
                NoFacts,
                store,
                default_template_fn(),
                plan_ctx(),
                HashSet::new(),
            )
            .unwrap();

        assert_eq!(registry.all().len(), 2);
        let nginx = ResourceId::new(&ResourceKind::from_static("apt.package"), "nginx");
        assert!(registry.get(&nginx).is_some());
    }

    #[test]
    fn evaluate_propagates_syntax_error() {
        let bundle = make_bundle("this is = = = not starlark");
        let primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        let evaluator = Evaluator::new(bundle, primitives);
        let store = Arc::new(SensitiveStore::new());
        let err = evaluator
            .evaluate(
                NoFacts,
                store,
                default_template_fn(),
                plan_ctx(),
                HashSet::new(),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            StarlarkGlueError::Syntax { .. } | StarlarkGlueError::Eval(_)
        ));
    }

    /// Сборка bundle с lib-модулем под `_lib/<name>/main.star`. Используется
    /// для проверки, что deadline-чекер применяется к loaded модулям так же,
    /// как и к entry-манифесту.
    fn make_bundle_with_lib(manifest_source: &str, lib_name: &str, lib_source: &str) -> Bundle {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.keep();
        std::fs::write(
            root.join("bundle.toml"),
            r#"
[bundle]
name = "test"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "main.star"
"#,
        )
        .unwrap();
        std::fs::write(root.join("main.star"), manifest_source).unwrap();
        let lib_dir = root.join("_lib").join(lib_name);
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(lib_dir.join("main.star"), lib_source).unwrap();
        Bundle::load_dir(&root).unwrap()
    }

    #[test]
    fn deadline_aborts_infinite_loop_in_loaded_lib_module() {
        // Регрессия H2: loaded модули раньше создавались без
        // DeadlineChecker — бесконечный цикл в _lib/X/main.star висел
        // вечно и игнорировал --deadline-sec.
        //
        // Сценарий: main.star делает `load("@lib/loopy", ...)`, lib
        // содержит while True: pass. Ставим deadline=200ms. Ожидаем
        // ошибку eval в пределах разумного времени.
        let bundle = make_bundle_with_lib(
            r#"
load("@lib/loopy", "x")
"#,
            "loopy",
            // Top-level for-loop ходит больше шагов чем before_stmt
            // успевает увидеть, и проверяется до выхода. Используем
            // явный «бесконечный» цикл через counter, который заведомо
            // не завершится до deadline.
            r#"
x = 1
for i in range(1000000000):
    x = x + i
"#,
        );
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(ResourceKind::from_static("apt.package"), Box::new(MockApt));
        let evaluator = Evaluator::new(bundle, primitives);
        let store = Arc::new(SensitiveStore::new());

        // Очень близкий deadline: 200мс с запасом на сборку.
        let plan = PlanCtx {
            deadline: Instant::now() + Duration::from_millis(200),
            cancel: CancellationToken::new(),
        };
        let start = Instant::now();
        let err = evaluator
            .evaluate(NoFacts, store, default_template_fn(), plan, HashSet::new())
            .unwrap_err();
        let elapsed = start.elapsed();

        // Должны прерваться задолго до условного «навсегда».
        assert!(
            elapsed < Duration::from_secs(5),
            "loaded module took {elapsed:?} — deadline checker не применился",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("deadline") || msg.contains("cancelled"),
            "expected deadline error, got: {msg}",
        );
    }

    #[test]
    fn cancel_token_aborts_loaded_lib_module() {
        // Симметричная проверка: cancellation token, как и deadline,
        // должен прерывать loaded модуль. Заранее cancel'им и запускаем.
        let bundle = make_bundle_with_lib(
            r#"
load("@lib/loopy", "x")
"#,
            "loopy",
            r#"
x = 1
for i in range(1000000000):
    x = x + i
"#,
        );
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(ResourceKind::from_static("apt.package"), Box::new(MockApt));
        let evaluator = Evaluator::new(bundle, primitives);
        let store = Arc::new(SensitiveStore::new());

        let cancel = CancellationToken::new();
        cancel.cancel();
        let plan = PlanCtx {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel,
        };
        let start = Instant::now();
        let err = evaluator
            .evaluate(NoFacts, store, default_template_fn(), plan, HashSet::new())
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(5), "cancel не применился");
        let msg = format!("{err}");
        assert!(
            msg.contains("cancelled") || msg.contains("deadline"),
            "expected cancel error, got: {msg}",
        );
    }
}
