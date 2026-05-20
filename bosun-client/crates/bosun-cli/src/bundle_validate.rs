//! `bosun bundle validate` — статическая проверка bundle без обращения к системе.
//!
//! Шаги:
//! 1. Загрузить bundle (читает bundle.toml, валидирует entry через path_safety).
//! 2. Прочитать --facts JSON в FactsSnapshot (если задан); иначе пустой.
//! 3. Создать Evaluator с **продакшен-набором** примитивов — те же
//!    `build_primitives()` из `run`. На фазе evaluate_manifest вызывается
//!    только `Primitive::build_payload` (см. `register_primitive_call` в
//!    starlark_glue), который не делает сетевых вызовов и не лезет в систему,
//!    но строго валидирует kwargs: required/optional типы, диапазоны
//!    `optional_u32`/`optional_u64`, allow-list `service.unit`. Опечатки
//!    вроде `apt.package(name="x", versionn="1.0")` (silent drop поля) не
//!    ловятся `build_payload`'ом, поскольку он читает только известные ему
//!    ключи; зато ловятся все ошибки типов и обязательных параметров.
//! 4. `template_fn` — реальный `render_template` с `Strict` undefined-behavior.
//!    С `--facts` фикстурой контекст подставляется; без — пустой объект,
//!    и шаблон, ссылающийся на `{{ inv.X }}`, корректно валится на
//!    UndefinedVariable. Это ловит Jinja2 syntax errors и неизвестные
//!    переменные ДО того, как bundle поедет на ноду.
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
    evaluate_manifest, Bundle, EvaluatorConfig, FactValue, FactsSource, PlanCtx, Registry,
    SensitiveStore, TemplateFn,
};
use bosun_primitives::template::render_template;
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
    run_core(args)
}

/// Ядро `bundle validate` без logging-init. Выделено отдельно, чтобы юнит-тесты
/// могли вызывать его в любом порядке: в test-binary tracing-subscriber
/// устанавливается один раз, поэтому повторный `logging::init` всегда падает
/// и возвращал бы `CLI_ENV_ERROR`, маскируя реальный исход validate'а.
pub(crate) fn run_core(args: &BundleValidateArgs) -> i32 {
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

    // Продакшен-набор примитивов: те же, что run::apply. evaluate_manifest
    // вызывает только build_payload, который не лезет в систему, поэтому
    // pg_sql и apt без реальной БД и apt-get тут безопасны.
    let primitives = crate::run::build_primitives();
    let registry = Rc::new(RefCell::new(Registry::new()));
    let plan_ctx = PlanCtx::new(
        Instant::now() + Duration::from_secs(60),
        CancellationToken::new(),
    );

    // Материализуем facts из fixture в плоский JSON-объект для шаблонов
    // (`{{ inv.facts.X }}` через render_template принимает facts отдельным
    // аргументом). Без --facts всё равно пустой объект — Strict undefined
    // тогда поймает любой шаблон, обращающийся к `{{ inv.foo }}`, что и есть
    // желаемое поведение: оператор сразу видит, что bundle ожидает inventory.
    let facts_json_for_templates =
        serde_json::Value::Object(facts_snapshot.clone().into_iter().collect());
    let template_fn: TemplateFn = Rc::new(
        move |resolved_path: &Path, _rel: &str, ctx: &serde_json::Value| {
            // resolved_path — абсолютный канонический путь шаблона в роли.
            // render_template ожидает (templates_root, relative): разбиваем
            // на parent + file_name.
            let parent = resolved_path.parent().ok_or_else(|| {
                anyhow::anyhow!("template: resolved path has no parent: {resolved_path:?}")
            })?;
            let file_name = resolved_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("template: resolved path has no file name"))?
                .to_string_lossy()
                .into_owned();
            // `inv` для шаблона: совпадает с продакшен-семантикой из run.rs.
            // Если ctx — объект с ключом `inv`, берём `ctx.inv`; иначе ctx
            // целиком кладётся как `inv`. Это даёт совместимость с
            // legacy-шаблонами (`{{ inv.foo }}`) и новым стилем
            // (`template(..., inv = inv)`).
            let inv_value = match ctx {
                serde_json::Value::Object(m) if m.contains_key("inv") => m["inv"].clone(),
                other => other.clone(),
            };
            render_template(parent, &file_name, &inv_value, &facts_json_for_templates)
                .map_err(|e| anyhow::anyhow!(e))
        },
    );

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use crate::args::BundleValidateArgs;

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

    /// Соберёт минимальный bundle с заданным main.star. Без ролей и шаблонов.
    fn make_bundle(dir: &Path, main_star: &str) {
        fs::write(
            dir.join("bundle.toml"),
            "[bundle]\nname = \"test\"\nversion = \"0.0.1\"\nentry = \"main.star\"\nrequires_bosun = \"^0.1\"\n",
        )
        .unwrap();
        fs::write(dir.join("main.star"), main_star).unwrap();
    }

    /// Bundle с ролью r1: main.star загружает её, роль вызывает указанный код.
    /// Если задан `template_body`, в roles/r1/templates/x.j2 кладётся этот контент.
    fn make_bundle_with_role(dir: &Path, role_star: &str, template_body: Option<&str>) {
        fs::write(
            dir.join("bundle.toml"),
            "[bundle]\nname = \"test\"\nversion = \"0.0.1\"\nentry = \"main.star\"\nrequires_bosun = \"^0.1\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("main.star"),
            "load(\"@roles/r1\", configure_r1 = \"configure\")\nconfigure_r1()\n",
        )
        .unwrap();
        let role_dir = dir.join("roles").join("r1");
        fs::create_dir_all(&role_dir).unwrap();
        fs::write(role_dir.join("main.star"), role_star).unwrap();
        if let Some(body) = template_body {
            let templates_dir = role_dir.join("templates");
            fs::create_dir_all(&templates_dir).unwrap();
            fs::write(templates_dir.join("x.j2"), body).unwrap();
        }
    }

    fn args_for(bundle: PathBuf) -> BundleValidateArgs {
        BundleValidateArgs {
            bundle,
            facts: None,
            tags: Vec::new(),
        }
    }

    /// build_payload реальной AptPrimitive требует `name` строкой. Если
    /// оператор пишет `apt.package(name = 42)` — это WrongType, validate
    /// обязан вернуть EVAL_ERROR. До перехода на реальные примитивы
    /// NoopPrimitive принимал любой тип.
    #[test]
    fn validate_rejects_wrong_type_for_required_kwarg() {
        let tmp = TempDir::new().unwrap();
        make_bundle(
            tmp.path(),
            "load(\"@bosun/builtins\", \"apt\")\napt.package(name = 42)\n",
        );
        let code = run_core(&args_for(tmp.path().to_path_buf()));
        assert_eq!(
            code,
            exit_code::EVAL_ERROR,
            "validate должен поймать wrong type для apt.package(name=int)",
        );
    }

    /// Без `name` apt.package должен падать как InvalidPayload (required arg).
    #[test]
    fn validate_rejects_missing_required_kwarg() {
        let tmp = TempDir::new().unwrap();
        make_bundle(
            tmp.path(),
            "load(\"@bosun/builtins\", \"apt\")\napt.package(version = \"1.0.0\")\n",
        );
        let code = run_core(&args_for(tmp.path().to_path_buf()));
        assert_eq!(
            code,
            exit_code::EVAL_ERROR,
            "validate должен поймать отсутствие required name",
        );
    }

    /// `service.unit` строго проверяет allow-list kwargs. Опечатка типа
    /// `cgroup_procs_path` (runr-only) на service.unit должна валиться
    /// reject_unexpected_service_unit_kwargs до build_payload'а.
    #[test]
    fn validate_rejects_unexpected_kwarg_on_service_unit() {
        let tmp = TempDir::new().unwrap();
        make_bundle(
            tmp.path(),
            "load(\"@bosun/builtins\", \"service\")\nservice.unit(name = \"nginx\", cgroup_procs_path = \"/sys/fs/cgroup/nginx\")\n",
        );
        // Без --tags активных тэгов нет, но service.unit вызывается на top-level,
        // так что reject_unexpected_service_unit_kwargs срабатывает до диспатча
        // в init-specific примитив.
        let code = run_core(&args_for(tmp.path().to_path_buf()));
        assert_eq!(
            code,
            exit_code::EVAL_ERROR,
            "service.unit должен отвергать unexpected kwarg",
        );
    }

    /// Jinja2 syntax error в шаблоне должен ловиться render_template'ом
    /// при evaluate_manifest. До фикса template_fn возвращал пустую строку,
    /// и любой syntax error проходил «evaluate OK».
    /// template() вызывается из роли (из entry-манифеста запрещён by spec).
    #[test]
    fn validate_rejects_jinja_syntax_error_in_template() {
        let tmp = TempDir::new().unwrap();
        make_bundle_with_role(
            tmp.path(),
            "load(\"@bosun/builtins\", \"file\", \"template\")\n\ndef configure():\n    file.content(path = \"/etc/x.conf\", contents = template(\"x.j2\"), mode = 0o644)\n",
            Some("broken {{ unclosed"),
        );
        let code = run_core(&args_for(tmp.path().to_path_buf()));
        assert_eq!(
            code,
            exit_code::EVAL_ERROR,
            "validate должен поймать jinja syntax error",
        );
    }

    /// Strict undefined: шаблон ссылается на `{{ inv.foo }}` без fixture —
    /// должен валиться UndefinedVariable. Это даёт оператору раннюю обратную
    /// связь: «bundle ожидает inventory, прогони validate с --facts».
    #[test]
    fn validate_rejects_undefined_variable_in_template_without_fixture() {
        let tmp = TempDir::new().unwrap();
        make_bundle_with_role(
            tmp.path(),
            "load(\"@bosun/builtins\", \"file\", \"template\")\n\ndef configure():\n    file.content(path = \"/etc/x.conf\", contents = template(\"x.j2\"), mode = 0o644)\n",
            Some("{{ inv.foo }}"),
        );
        let code = run_core(&args_for(tmp.path().to_path_buf()));
        assert_eq!(
            code,
            exit_code::EVAL_ERROR,
            "validate без fixture должен ловить undefined inv.foo",
        );
    }

    /// Минимальный sanity-check: корректный bundle без шаблонов — evaluate OK.
    /// Защита от «всегда возвращаем EVAL_ERROR».
    #[test]
    fn validate_accepts_well_formed_bundle() {
        let tmp = TempDir::new().unwrap();
        make_bundle(
            tmp.path(),
            "load(\"@bosun/builtins\", \"apt\")\napt.package(name = \"curl\")\n",
        );
        let code = run_core(&args_for(tmp.path().to_path_buf()));
        assert_eq!(
            code,
            exit_code::SUCCESS,
            "корректный bundle должен возвращать SUCCESS",
        );
    }
}
