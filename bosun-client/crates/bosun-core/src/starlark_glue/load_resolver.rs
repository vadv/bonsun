//! `load()` резолвер. В MVP поддерживается единственный путь — `@bosun/builtins`.
//!
//! Любой другой путь (`//module`, file paths) — ошибка с явным сообщением.
//! Это сознательное ограничение: пользовательские модули из bundle не
//! поддерживаются в MVP, спека вынесла это в open-questions.

use starlark::environment::FrozenModule;
use starlark::eval::FileLoader;
// `FrozenModule::dupe` приходит из crate `dupe`. Re-export-ом starlark не
// предоставляет — берём напрямую из workspace зависимостей starlark.
use dupe::Dupe as _;

/// FileLoader для bundle: знает только `@bosun/builtins`.
pub struct BundleLoader<'a> {
    builtins: &'a FrozenModule,
}

impl<'a> BundleLoader<'a> {
    pub fn new(builtins: &'a FrozenModule) -> Self {
        Self { builtins }
    }
}

impl FileLoader for BundleLoader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        if path == "@bosun/builtins" {
            return Ok(self.builtins.dupe());
        }
        Err(starlark::Error::new_other(anyhow::anyhow!(
            "only @bosun/builtins is supported in MVP; got '{path}'"
        )))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use starlark::environment::{GlobalsBuilder, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    use super::*;

    fn empty_builtins() -> FrozenModule {
        let g = GlobalsBuilder::new().build();
        FrozenModule::from_globals(&g).unwrap()
    }

    #[test]
    fn accepts_bosun_builtins() {
        let builtins = empty_builtins();
        let loader = BundleLoader::new(&builtins);
        let res = loader.load("@bosun/builtins");
        assert!(res.is_ok());
    }

    #[test]
    fn rejects_unknown_path_with_clear_message() {
        let builtins = empty_builtins();
        let loader = BundleLoader::new(&builtins);
        let err = loader.load("//foo/bar").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("only @bosun/builtins is supported"));
        assert!(msg.contains("//foo/bar"));
    }

    #[test]
    fn rejects_relative_path() {
        let builtins = empty_builtins();
        let loader = BundleLoader::new(&builtins);
        let err = loader.load("./local.star").unwrap_err();
        assert!(format!("{err}").contains("only @bosun/builtins"));
    }

    #[test]
    fn evaluator_using_unknown_path_via_resolver_fails() {
        // Smoke-тест: парс модуля, который делает load по непознанному пути.
        // Ожидаем ошибку с сообщением «only @bosun/builtins is supported».
        let builtins = empty_builtins();
        let loader = BundleLoader::new(&builtins);
        let globals = GlobalsBuilder::new().build();
        let module = Module::new();
        let ast = AstModule::parse(
            "test.star",
            "load(\"//some/foo\", \"x\")\n".to_string(),
            &Dialect::Standard,
        )
        .unwrap();
        let mut eval = Evaluator::new(&module);
        eval.set_loader(&loader);
        let err = eval.eval_module(ast, &globals).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("only @bosun/builtins is supported") || msg.contains("//some/foo"),
            "expected loader error in eval, got: {msg}"
        );
    }
}
