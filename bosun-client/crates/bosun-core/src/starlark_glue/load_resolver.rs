//! `load()` резолвер для bundle.
//!
//! Поддерживаемые формы:
//! - `@bosun/builtins` → frozen module со всеми native globals.
//! - `@roles/<name>` → `roles/<name>/main.star`.
//! - `@lib/<name>` → `_lib/<name>/main.star`.
//!
//! Кеш модулей живёт в `BundleLoader` (RefCell) и сбрасывается между
//! вызовами `evaluate_manifest`. Ключ — канонический PathBuf, что
//! делает symlink-эквивалентность видимой одному и тому же кешу.
//!
//! Privacy enforcement: `_`-префиксные символы помечены Visibility::Private
//! на уровне starlark и автоматически отказываются в `load("@roles/X", "_p")`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use dupe::Dupe as _;
use starlark::environment::{FrozenModule, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::AstModule;

use crate::bundle::BundleError;
use crate::starlark_glue::{
    build_globals, bundle_dialect, install_deadline_checker, EvalState, ModuleStackGuard,
    StarlarkGlueError,
};

/// FileLoader для bundle: знает `@bosun/builtins` + `@roles/<name>` + `@lib/<name>`.
pub(crate) struct BundleLoader<'a> {
    builtins: &'a FrozenModule,
    state: Rc<EvalState>,
    /// Кеш загруженных модулей. RefCell — потому что FileLoader::load берёт &self.
    cache: RefCell<HashMap<PathBuf, FrozenModule>>,
}

impl<'a> BundleLoader<'a> {
    pub(crate) fn new(builtins: &'a FrozenModule, state: Rc<EvalState>) -> Self {
        Self {
            builtins,
            state,
            cache: RefCell::new(HashMap::new()),
        }
    }
}

impl FileLoader for BundleLoader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        if path == "@bosun/builtins" {
            return Ok(self.builtins.dupe());
        }
        if path.starts_with("@roles/") || path.starts_with("@lib/") {
            return self.load_role_or_lib(path).map_err(|e| {
                self.state.errors.borrow_mut().push(match e {
                    StarlarkGlueError::Bundle(inner) => StarlarkGlueError::Bundle(inner),
                    other => other,
                });
                starlark::Error::new_other(anyhow::anyhow!("{}", self.last_error_message()))
            });
        }
        let err = StarlarkGlueError::Bundle(BundleError::UnsupportedLoadPath {
            load_path: path.to_string(),
        });
        let msg = format!("{err}");
        self.state.errors.borrow_mut().push(err);
        Err(starlark::Error::new_other(anyhow::anyhow!("{msg}")))
    }
}

impl BundleLoader<'_> {
    fn last_error_message(&self) -> String {
        self.state
            .errors
            .borrow()
            .last()
            .map(|e| format!("{e}"))
            .unwrap_or_else(|| "load error".to_string())
    }

    /// Разрешить путь к .star файлу, проверить кеш, загрузить и заполнить кеш.
    fn load_role_or_lib(&self, load_path: &str) -> Result<FrozenModule, StarlarkGlueError> {
        let resolved = self
            .state
            .bundle
            .resolve_module(load_path)
            .map_err(StarlarkGlueError::Bundle)?;

        if let Some(cached) = self.cache.borrow().get(&resolved) {
            return Ok(cached.dupe());
        }

        let _guard = ModuleStackGuard::push(Rc::clone(&self.state), resolved.clone());
        let frozen = self.parse_and_eval(&resolved)?;
        self.cache.borrow_mut().insert(resolved, frozen.dupe());
        Ok(frozen)
    }

    fn parse_and_eval(&self, module_path: &PathBuf) -> Result<FrozenModule, StarlarkGlueError> {
        let source = std::fs::read_to_string(module_path).map_err(|e| {
            StarlarkGlueError::Bundle(BundleError::Io {
                path: module_path.to_string_lossy().into_owned(),
                source: e,
            })
        })?;
        let module_name = module_path.to_string_lossy().into_owned();
        let ast = AstModule::parse(&module_name, source, &bundle_dialect()).map_err(|e| {
            StarlarkGlueError::Syntax {
                file: module_name.clone(),
                message: format!("{e}"),
            }
        })?;

        let module = Module::new();
        let globals = build_globals();
        let inner_loader = InnerLoader { outer: self };
        {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&inner_loader);
            // DeadlineChecker применяется и здесь — иначе бесконечный
            // цикл в loaded `_lib/foo.star` или `roles/<r>/main.star`
            // игнорировал бы --deadline-sec и SIGTERM, повесив весь run.
            install_deadline_checker(
                &mut eval,
                self.state.plan_ctx.deadline,
                self.state.plan_ctx.cancel.clone(),
            );
            eval.eval_module(ast, &globals)
                .map_err(|e| StarlarkGlueError::Eval(format!("{e}")))?;
        }
        module
            .freeze()
            .map_err(|e| StarlarkGlueError::Eval(format!("module freeze: {e:?}")))
    }
}

/// Внутренний loader, передающийся при загрузке role/lib модулей. Делегирует
/// в outer для всех решений; нужен потому что `Evaluator::set_loader` требует
/// `&dyn FileLoader` с подходящим лайфтаймом.
struct InnerLoader<'a, 'b> {
    outer: &'a BundleLoader<'b>,
}

impl FileLoader for InnerLoader<'_, '_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        self.outer.load(path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use starlark::environment::GlobalsBuilder;

    use super::*;

    fn empty_builtins() -> FrozenModule {
        let g = GlobalsBuilder::new().build();
        FrozenModule::from_globals(&g).unwrap()
    }

    fn empty_state() -> Rc<EvalState> {
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        use crate::bundle::{Bundle, BundleMetadata};
        use crate::primitive::{FactsSource, PlanCtx};
        use crate::registry::Registry;
        use crate::sensitive::SensitiveStore;
        use crate::starlark_glue::default_template_fn;

        struct NoFacts;
        impl FactsSource for NoFacts {
            fn get(&self, _name: &str) -> crate::facts::FactValue {
                crate::facts::FactValue::Unknown {
                    reason: "test".to_string(),
                }
            }
        }

        let bundle = Bundle {
            metadata: BundleMetadata {
                name: "x".into(),
                version: "0.1.0".into(),
                description: None,
                requires_bosun: "^0.1".into(),
                entry: "main.star".into(),
                inventory: Default::default(),
                tags: Default::default(),
            },
            root: PathBuf::from("/tmp/nonexistent"),
            entry: PathBuf::from("/tmp/nonexistent/main.star"),
        };
        Rc::new(EvalState {
            bundle: Rc::new(bundle),
            primitives: Rc::new(HashMap::new()),
            facts: Rc::new(NoFacts),
            sensitive: Arc::new(SensitiveStore::new()),
            registry: Rc::new(RefCell::new(Registry::new())),
            plan_ctx: PlanCtx {
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                cancel: CancellationToken::new(),
            },
            errors: RefCell::new(Vec::new()),
            template_fn: default_template_fn(),
            tags: Default::default(),
            current_module: RefCell::new(Vec::new()),
            inventory_cache: RefCell::new(HashMap::new()),
        })
    }

    #[test]
    fn accepts_bosun_builtins() {
        let builtins = empty_builtins();
        let state = empty_state();
        let loader = BundleLoader::new(&builtins, state);
        let res = loader.load("@bosun/builtins");
        assert!(res.is_ok());
    }

    #[test]
    fn rejects_unknown_path_with_clear_message() {
        let builtins = empty_builtins();
        let state = empty_state();
        let loader = BundleLoader::new(&builtins, state);
        let err = loader.load("//foo/bar").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unsupported"), "got: {msg}");
    }
}
