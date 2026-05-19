//! `bosun bundle validate` — статическая проверка bundle без обращения к системе.
//!
//! Шаги:
//! 1. Загрузить bundle (читает bundle.toml, валидирует entry через path_safety).
//! 2. Прочитать --facts JSON в FactsSnapshot (если задан); иначе пустой.
//! 3. Создать Evaluator с моками primitive'ов (они только регистрируют
//!    Resource, не делают plan/apply).
//! 4. Выполнить evaluate_manifest.
//! 5. Распечатать «evaluate OK, N resources registered» при успехе или
//!    диагностику при ошибке.
//!
//! Применение: CI bundle-репозиториев, pre-commit hook.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bosun_core::{
    evaluate_manifest, Bundle, CallArgs, ChangeReport, Diff, EvaluatorConfig, FactValue,
    FactsSource, PlanCtx, Primitive, PrimitiveError, Registry, Resource, ResourceKind,
    SensitiveStore, TemplateFn,
};
use tokio_util::sync::CancellationToken;

use crate::args::BundleValidateArgs;
use crate::exit_code;

/// Запустить `bundle validate`. Возвращает exit-код.
pub fn run(args: &BundleValidateArgs) -> i32 {
    if let Err(e) = crate::logging::init(
        crate::args::LogLevel::Info,
        crate::args::LogFormat::Text,
        true,
    ) {
        eprintln!("bosun: logging init failed: {e}");
        return exit_code::CLI_ENV_ERROR;
    }

    let bundle = match Bundle::load_dir(&args.bundle) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("bosun: bundle load failed: {e}");
            return exit_code::EVAL_ERROR;
        }
    };

    let version = env!("CARGO_PKG_VERSION");
    if let Err(e) = bundle.check_compatibility(version) {
        eprintln!("bosun: bundle requires different bosun version: {e}");
        return exit_code::EVAL_ERROR;
    }

    let facts_snapshot = match load_facts_fixture(args.facts.as_deref()) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let mut tags: Vec<String> = args.tags.clone();
    tags.sort_unstable();
    tags.dedup();
    let tags: HashSet<String> = tags.into_iter().collect();

    let primitives = build_validate_primitives();
    let registry = Rc::new(RefCell::new(Registry::new()));
    let plan_ctx = PlanCtx::new(
        Instant::now() + Duration::from_secs(60),
        CancellationToken::new(),
    );
    let template_fn: TemplateFn =
        Rc::new(|_resolved: &Path, _path: &str, _ctx: &serde_json::Value| {
            // Validate не рендерит шаблоны (нет inv/facts полного), отдаёт
            // заглушку. Если bundle обязательно вызывает template() — пройдёт,
            // но содержимое будет пустым; это OK для статической проверки.
            Ok(String::new())
        });

    let facts_rc: Rc<dyn FactsSource> = Rc::new(FixtureFacts {
        values: facts_snapshot,
    });

    let config = EvaluatorConfig {
        bundle: Rc::new(bundle),
        primitives: Rc::new(primitives),
        facts: facts_rc,
        sensitive: Arc::new(SensitiveStore::new()),
        registry: Rc::clone(&registry),
        plan_ctx,
        template_fn,
        tags,
    };

    match evaluate_manifest(config) {
        Ok(()) => {
            let count = registry.borrow().all().len();
            println!("evaluate OK, {count} resources registered");
            exit_code::SUCCESS
        }
        Err(e) => {
            eprintln!("bosun: bundle validate failed: {e}");
            exit_code::EVAL_ERROR
        }
    }
}

/// Прочитать facts.json в map; ключ — имя факта.
fn load_facts_fixture(path: Option<&Path>) -> Result<HashMap<String, serde_json::Value>, i32> {
    let Some(path) = path else {
        return Ok(HashMap::new());
    };
    let text = std::fs::read_to_string(path).map_err(|e| {
        eprintln!("bosun: reading facts fixture {}: {e}", path.display());
        exit_code::EVAL_ERROR
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        eprintln!("bosun: parsing facts fixture {}: {e}", path.display());
        exit_code::EVAL_ERROR
    })?;
    match value {
        serde_json::Value::Object(map) => Ok(map.into_iter().collect()),
        _ => {
            eprintln!(
                "bosun: facts fixture {} must be a JSON object",
                path.display()
            );
            Err(exit_code::EVAL_ERROR)
        }
    }
}

/// FactsSource поверх HashMap из fixture'а.
struct FixtureFacts {
    values: HashMap<String, serde_json::Value>,
}

impl FactsSource for FixtureFacts {
    fn get(&self, name: &str) -> FactValue {
        match self.values.get(name) {
            Some(v) => FactValue::Known(v.clone()),
            None => FactValue::Unknown {
                reason: format!("fixture has no '{name}'"),
            },
        }
    }
}

/// Mock-набор примитивов для validate. Регистрируют payload, не делают
/// plan/apply (валидация не запускает оркестратор). Состав совпадает с
/// продакшен-набором из `run::build_primitives` плюс runr.service /
/// systemd.service, к которым диспатчит абстрактный `service.unit`.
fn build_validate_primitives() -> HashMap<ResourceKind, Box<dyn Primitive>> {
    let mut m: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
    m.insert(
        ResourceKind::from_static("apt.key"),
        Box::new(NoopPrimitive {
            kind: "apt.key",
            identity_keys: &["name"],
        }),
    );
    m.insert(
        ResourceKind::from_static("apt.package"),
        Box::new(NoopPrimitive {
            kind: "apt.package",
            identity_keys: &["name"],
        }),
    );
    m.insert(
        ResourceKind::from_static("apt.update_cache"),
        Box::new(NoopPrimitive {
            kind: "apt.update_cache",
            identity_keys: &["name"],
        }),
    );
    m.insert(
        ResourceKind::from_static("file.content"),
        Box::new(NoopPrimitive {
            kind: "file.content",
            identity_keys: &["path"],
        }),
    );
    m.insert(
        ResourceKind::from_static("runr.service"),
        Box::new(NoopPrimitive {
            kind: "runr.service",
            identity_keys: &["name"],
        }),
    );
    m.insert(
        ResourceKind::from_static("systemd.service"),
        Box::new(NoopPrimitive {
            kind: "systemd.service",
            identity_keys: &["name"],
        }),
    );
    m.insert(
        ResourceKind::from_static("process.signal"),
        Box::new(NoopPrimitive {
            kind: "process.signal",
            identity_keys: &["name"],
        }),
    );
    m.insert(
        ResourceKind::from_static("cert.tls"),
        Box::new(NoopPrimitive {
            kind: "cert.tls",
            identity_keys: &["cert_path"],
        }),
    );
    m
}

struct NoopPrimitive {
    kind: &'static str,
    identity_keys: &'static [&'static str],
}

impl Primitive for NoopPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static(self.kind)
    }
    fn identity_keys(&self) -> &'static [&'static str] {
        self.identity_keys
    }
    fn build_payload(
        &self,
        args: &CallArgs,
        _ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError> {
        // Возвращаем простой JSON-снимок identity-ключей, чтобы Resource
        // прошёл валидацию Registry.
        let mut out = serde_json::Map::new();
        for key in self.identity_keys {
            let v = args.required_str(key).map_err(|e| {
                PrimitiveError::InvalidPayload(format!("{kind}: {e}", kind = self.kind))
            })?;
            out.insert((*key).to_string(), serde_json::Value::String(v));
        }
        Ok(serde_json::Value::Object(out))
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
        _ctx: &bosun_core::ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError> {
        Ok(ChangeReport::no_change())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn fixture_facts_returns_known_for_present_key() {
        let mut m = HashMap::new();
        m.insert("hostname".to_string(), serde_json::json!("test"));
        let f = FixtureFacts { values: m };
        match f.get("hostname") {
            FactValue::Known(v) => assert_eq!(v, serde_json::json!("test")),
            _ => panic!("expected Known"),
        }
    }

    #[test]
    fn fixture_facts_returns_unknown_for_missing_key() {
        let f = FixtureFacts {
            values: HashMap::new(),
        };
        match f.get("nope") {
            FactValue::Unknown { .. } => {}
            _ => panic!("expected Unknown"),
        }
    }
}
