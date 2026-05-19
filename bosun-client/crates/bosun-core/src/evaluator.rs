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
}
