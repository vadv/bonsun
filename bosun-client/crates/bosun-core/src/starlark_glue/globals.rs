//! Native-globals для Starlark.
//!
//! Глобалы (доступны без `load`, но также экспортируются через
//! `@bosun/builtins`):
//! - `apt`, `file` — namespaces, регистрирующие ресурсы (как в MVP).
//! - `template(path)` — module-relative рендер шаблона.
//! - `inventory.load/merge/merge_keyed` — загрузка и слияние yaml-инвентарей.
//! - `tags.has/require_one_of/active` — runtime gate по тэгам CLI.
//! - `inv` — устанавливается per-evaluate как module-level переменная (legacy
//!   MVP-доступ; в новых bundle'ах автор использует `inventory.load`).
//!
//! Native-функции читают разделяемое состояние из thread-local через
//! `with_state(...)` (см. `mod.rs::CURRENT_STATE`).

use std::collections::HashMap;
use std::path::PathBuf;

use starlark::environment::{FrozenModule, Globals, GlobalsBuilder};
use starlark::eval::Evaluator;
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::tuple::UnpackTuple;
use starlark::values::{FreezeResult, Value, ValueLike};
use starlark_derive::starlark_module;

use crate::call_args::{ArgValue, CallArgs};
use crate::digest::sha256_hex;
use crate::inventory::{merge_inventory, merge_inventory_keyed, MergeStrategy};
use crate::path_safety::{resolve_within_root, PathSafetyError};
use crate::resource::{Resource, ResourceId, ResourceKind};
use crate::sensitive::SensitivePayload;
use crate::starlark_glue::inv_object::json_scalar_to_value;
use crate::starlark_glue::{current_state, with_state, EvalState, StarlarkGlueError};

/// Globals для bosun-манифеста. Включает namespaces `apt`, `file`, `inventory`,
/// `tags`, `service`, `process`, `users`, `runr`, `systemd`, `cert` и функцию
/// `template`, плюс стандартную библиотеку starlark.
///
/// `service.unit` — диспетчер по факту `init_system`. Конкретные `runr.*` и
/// `systemd.*` — для случаев, когда роль действительно зависит от init-системы
/// (например, runr-only `cgroup_procs_path` или systemd-only
/// `condition_path_exists`). Power-user может использовать их напрямую без
/// обёртки `service.unit`.
pub fn build_globals() -> Globals {
    GlobalsBuilder::standard()
        .with_namespace("apt", apt_namespace)
        .with_namespace("cert", cert_namespace)
        .with_namespace("file", file_namespace)
        .with_namespace("inventory", inventory_namespace)
        .with_namespace("tags", tags_namespace)
        .with_namespace("service", service_namespace)
        .with_namespace("process", process_namespace)
        .with_namespace("users", users_namespace)
        .with_namespace("runr", runr_namespace)
        .with_namespace("systemd", systemd_namespace)
        .with(template_fn)
        .build()
}

#[starlark_module]
fn apt_namespace(builder: &mut GlobalsBuilder) {
    fn package<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("apt.package", kwargs, eval)
    }
}

#[starlark_module]
fn file_namespace(builder: &mut GlobalsBuilder) {
    fn content<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("file.content", kwargs, eval)
    }

    /// `file.delete(path=, recursive=False, follow_symlinks=False)` —
    /// снять файл, симлинк или директорию. По умолчанию отказывается
    /// удалять непустую директорию: для этого нужно `recursive=True`.
    /// Симлинки удаляются как символические ссылки, без следования за
    /// ними (определение типа через `symlink_metadata`).
    fn delete<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("file.delete", kwargs, eval)
    }

    /// `file.symlink(path=, target=, state="present", force=False)` —
    /// управление симлинком. По умолчанию `state="present"`. `force=True`
    /// разрешает заменить существующий файл/директорию по `path`. Сама
    /// цель `target` может указывать на несуществующий путь — chiit
    /// сценарий pg-симлинков создаёт их до раскатки реального
    /// дистрибутива.
    fn symlink<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("file.symlink", kwargs, eval)
    }
}

#[starlark_module]
fn inventory_namespace(builder: &mut GlobalsBuilder) {
    /// Загрузить yaml-файл по пути относительно корня bundle. Результат
    /// кешируется per-evaluate; повторный вызов с тем же путём не парсит
    /// файл повторно.
    ///
    /// Метод называется `read` (не `load`), потому что `load` в Starlark —
    /// зарезервированное keyword'о для load()-statement и не может
    /// использоваться как attribute-name в выражении `inventory.load(...)`.
    /// См. starlark grammar.lalrpop: `"load" => Token::Load`. Спека rev 2
    /// в Starlark-примерах пишет `inventory.load(...)`, но Rust-сигнатура
    /// там использует `r#load` (raw identifier — Rust-конструкция, не
    /// относящаяся к Starlark). В Starlark такой возможности нет, поэтому
    /// мы экспортируем функционал под именем `read`.
    fn read<'v>(
        #[starlark(require = pos)] path: String,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let json = with_state(|state| load_inventory_yaml(state, &path))
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "internal: no eval state during inventory.read"
                ))
            })?
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("{e}")))?;
        Ok(json_scalar_to_value(eval.heap(), json))
    }

    /// Слить два и более inventory'я. Стратегия по умолчанию берётся из
    /// bundle.toml `[bundle.inventory].default_merge_strategy`. Передача
    /// `strategy=""` (пустая строка) эквивалентна отсутствию аргумента.
    fn merge<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(default = String::new())] strategy: String,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let inputs = unpack_inventory_sources(&args)
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("inventory.merge: {e}")))?;
        let strategy = resolve_merge_strategy(&strategy)
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("{e}")))?;
        let merged = inputs
            .into_iter()
            .reduce(|acc, next| merge_inventory(acc, next, strategy))
            .unwrap_or(serde_json::Value::Null);
        Ok(json_scalar_to_value(eval.heap(), merged))
    }

    /// Слить inventory'и по ключу-полю в каждом list-of-records. Top-level —
    /// обычный deep merge; внутри любого list ожидается map с указанным
    /// `<key>`.
    fn merge_keyed<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        key: String,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let inputs = unpack_inventory_sources(&args).map_err(|e| {
            starlark::Error::new_other(anyhow::anyhow!("inventory.merge_keyed: {e}"))
        })?;
        let mut iter = inputs.into_iter();
        let merged = match iter.next() {
            Some(first) => iter
                .try_fold(first, |acc, next| merge_inventory_keyed(acc, next, &key))
                .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("{e}")))?,
            None => serde_json::Value::Null,
        };
        Ok(json_scalar_to_value(eval.heap(), merged))
    }
}

#[starlark_module]
fn tags_namespace(builder: &mut GlobalsBuilder) {
    /// Возвращает True если тэг активен.
    fn has(#[starlark(require = pos)] tag: String) -> starlark::Result<bool> {
        let active = with_state(|state| state.tags.contains(&tag)).ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!("internal: no eval state during tags.has"))
        })?;
        Ok(active)
    }

    /// Fail-fast, если ни один из перечисленных тэгов не активен.
    fn require_one_of<'v>(
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
    ) -> starlark::Result<NoneType> {
        let mut expected: Vec<String> = Vec::with_capacity(args.items.len());
        for v in &args.items {
            let s = v.unpack_str().ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "tags.require_one_of: expected string argument, got {}",
                    v.get_type()
                ))
            })?;
            expected.push(s.to_string());
        }
        let result: Result<(), String> = with_state(|state| {
            if expected.iter().any(|t| state.tags.contains(t)) {
                Ok(())
            } else {
                let mut active: Vec<&str> = state.tags.iter().map(|s| s.as_str()).collect();
                active.sort_unstable();
                Err(format!(
                    "tags: expected one of [{expected_list}] in active set, got [{active_list}]",
                    expected_list = expected.join(", "),
                    active_list = active.join(", "),
                ))
            }
        })
        .ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "internal: no eval state during tags.require_one_of"
            ))
        })?;
        result.map_err(|msg| starlark::Error::new_other(anyhow::anyhow!("{msg}")))?;
        Ok(NoneType)
    }

    /// Отсортированная копия активного набора тэгов (для логирования).
    fn active<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        let mut active: Vec<String> = with_state(|state| state.tags.iter().cloned().collect())
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "internal: no eval state during tags.active"
                ))
            })?;
        active.sort_unstable();
        let items: Vec<Value> = active.into_iter().map(|s| eval.heap().alloc(s)).collect();
        Ok(eval.heap().alloc(AllocList(items)))
    }
}

#[starlark_module]
fn service_namespace(builder: &mut GlobalsBuilder) {
    /// `service.unit(name=, state=, ...)` — абстрактный диспатчер unit-сервиса.
    ///
    /// На основе факта `init_system` выбирает между `systemd.service`
    /// (для `systemd` и `mixed-systemd-runr`) и `runr.service` (для `runr`).
    /// Принимает только общий поднабор параметров: всё init-специфичное
    /// (например, `cgroup_procs_path` у runr или `condition_path_exists`
    /// у systemd) отклоняется с явной ошибкой. Power-user в таких случаях
    /// должен идти на конкретный примитив напрямую.
    fn unit<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        dispatch_service_unit(kwargs, eval)
    }
}

#[starlark_module]
fn process_namespace(builder: &mut GlobalsBuilder) {
    /// `process.signal(name=, signal=, process_name=|process_user=, deferred=?)`
    /// — узкий примитив отправки allowlist-сигнала процессу.
    ///
    /// Позволяет реализовать chiit-кейс `defers.AddCommand(ctx,
    /// "hup-pg-doorman", "pkill -HUP pg_doorman")` без exposing-а полного
    /// shell-escape hatch. Сигналы ограничены `HUP`/`TERM`/`INT`/`USR1`/
    /// `USR2`/`WINCH`/`PIPE` (без `KILL`/`STOP`/`CONT` — для остановки
    /// процессов используйте `service.unit(state="stopped")`).
    ///
    /// Селектор — ровно один из `process_name`/`process_user`. По умолчанию
    /// `deferred=True` (как в chiit-практике): запись попадает в журнал
    /// defers и выполняется в replay-фазе.
    fn signal<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("process.signal", kwargs, eval)
    }
}

#[starlark_module]
fn users_namespace(builder: &mut GlobalsBuilder) {
    /// `users.user(name=, state=, ...)` — декларативный системный пользователь.
    ///
    /// state — обязательный, "present" или "absent". Опциональные поля:
    /// `uid`, `group`, `shell`, `home`, `no_create_home`, `system`,
    /// `comment`. Если pользователь существует и spec совпадает с фактом
    /// — ничего не делает. Иначе вызывает `useradd`/`usermod`/`userdel`
    /// под капотом (требует root).
    fn user<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("users.user", kwargs, eval)
    }

    /// `users.group(name=, state=, ...)` — декларативная системная группа.
    /// state — "present" или "absent"; опциональные `gid`, `system`. При
    /// расхождении GID вызывает `groupmod --gid`.
    fn group<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("users.group", kwargs, eval)
    }
}

#[starlark_module]
fn cert_namespace(builder: &mut GlobalsBuilder) {
    /// `cert.tls(cert_path=, key_path=, common_name=, ...)` — self-signed
    /// x509-сертификат, сгенерированный pure-Rust пайплайном (rcgen + ring,
    /// RSA-ключи — через rsa-крейт). Без openssl-binary и без libssl.
    ///
    /// Обязательные kwargs: `cert_path`, `key_path`, `common_name`.
    /// Опциональные: `algorithm` ("rsa2048" | "ed25519" | "ecdsa_p256",
    /// default "rsa2048"), `days_valid` (3650), `renew_before_days` (30),
    /// `owner`, `group`, `mode_cert` (0o644), `mode_key` (0o600),
    /// `subject_alt_names` (list[str]).
    fn tls<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("cert.tls", kwargs, eval)
    }
}

/// Namespace `runr` — прямой доступ к runr-специфичным примитивам без обёртки
/// `service.unit`. Использовать, когда роль завязана на runr (например, нужен
/// `cgroup_procs_path` или роль гарантированно запускается на хосте с runr).
/// Иначе предпочтительнее `service.unit`, которая работает и на чистом systemd.
#[starlark_module]
fn runr_namespace(builder: &mut GlobalsBuilder) {
    /// `runr.service(name=, state=, ...)` — управление runr-сервисом через
    /// HTTP API `runr` daemon. Помимо общего поднабора `service.unit` принимает
    /// runr-специфичные kwargs: `cgroup_procs_path`, `restart_policy` и т.п.
    fn service<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("runr.service", kwargs, eval)
    }

    /// `runr.timer(name=, ...)` — runr-таймер. На хостах с systemd использовать
    /// `systemd.timer`.
    fn timer<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("runr.timer", kwargs, eval)
    }

    /// `runr.cgroup(name=, ...)` — runr-cgroup. Конфигурирует resource limits
    /// (cpu, memory, io) на уровне cgroup'а в runr.
    fn cgroup<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("runr.cgroup", kwargs, eval)
    }
}

/// Namespace `systemd` — прямой доступ к systemd-специфичным примитивам через
/// dbus. Использовать, когда роль завязана на systemd (например, нужен
/// `condition_path_exists`, drop-in override и т.п.). Иначе — `service.unit`.
#[starlark_module]
fn systemd_namespace(builder: &mut GlobalsBuilder) {
    /// `systemd.service(name=, state=, ...)` — управление systemd unit'ом через
    /// org.freedesktop.systemd1 dbus API. Принимает systemd-специфичные kwargs:
    /// `condition_path_exists`, `drop_in` и т.п.
    fn service<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("systemd.service", kwargs, eval)
    }

    /// `systemd.timer(name=, ...)` — systemd-таймер. На хостах с runr —
    /// `runr.timer`.
    fn timer<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("systemd.timer", kwargs, eval)
    }
}

/// Перечень параметров, которые `service.unit` пропускает в конкретный
/// примитив. Совпадает с тем подмножеством, которое и `runr.service`, и
/// `systemd.service` читают в `build_payload` (плюс общая notify-инфраструктура
/// `reload_on`/`restart_on`/`depends_on`). Любой ключ за пределами этого
/// списка — ошибка: автор bundle'а либо обращается к init-специфичной фиче
/// (тогда вызов должен идти на `runr.service` / `systemd.service` напрямую),
/// либо опечатался.
const SERVICE_UNIT_ALLOWED_KWARGS: &[&str] = &[
    "name",
    "state",
    "enable",
    "health_check_cmd",
    "health_check_url",
    "health_check_expected_status",
    "health_check_retry",
    "health_check_retry_interval_sec",
    "health_check_timeout_sec",
    "validate_with",
    "reload_on",
    "restart_on",
    "depends_on",
];

/// Реализация `service.unit`. Читает факт `init_system`, валидирует kwargs
/// против allow-листа и делегирует в `register_primitive_call` с целевым
/// `kind`.
///
/// Замечание про `enable`: и `runr.service`, и `systemd.service` сами назначают
/// дефолт (false и true соответственно). Диспатчер передаёт `enable` только
/// если ключ присутствует в исходных kwargs — это сохраняет per-init дефолт
/// в случае «параметр не передан» и пробрасывает явное значение пользователя
/// в случае `enable=True/False`.
fn dispatch_service_unit<'v>(
    kwargs: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    reject_unexpected_service_unit_kwargs(kwargs)?;

    let kind_str = with_state(resolve_service_unit_kind)
        .ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "internal: no eval state during service.unit"
            ))
        })?
        .map_err(starlark::Error::new_other)?;

    register_primitive_call(kind_str, kwargs, eval)
}

/// Прочитать `init_system` из EvalState и решить, в какой примитив диспатчить.
/// Возвращает статический литерал, чтобы дальше использовать его в
/// `register_primitive_call` как `kind_str`.
fn resolve_service_unit_kind(state: &EvalState) -> Result<&'static str, anyhow::Error> {
    let fact = state.facts.get("init_system");
    let value = fact.value().ok_or_else(|| {
        anyhow::anyhow!(
            "service.unit: init_system fact unknown; ensure the facts collector populated it",
        )
    })?;
    let init = value.as_str().ok_or_else(|| {
        anyhow::anyhow!(
            "service.unit: init_system fact is not a string (got {value}); facts collector misconfiguration",
        )
    })?;
    match init {
        "systemd" | "mixed-systemd-runr" => Ok("systemd.service"),
        "runr" => Ok("runr.service"),
        other => Err(anyhow::anyhow!(
            "service.unit: unsupported init_system {other:?}; expected one of \
             systemd, mixed-systemd-runr, runr",
        )),
    }
}

/// Проверить, что в kwargs нет неожиданных ключей. Все доступные параметры
/// перечислены в [`SERVICE_UNIT_ALLOWED_KWARGS`]. Это намеренно строго:
/// init-специфичные опции (runr-only `cgroup_procs_path`, systemd-only
/// `condition_path_exists`) должны вызывать `runr.service` / `systemd.service`
/// напрямую, а не маскироваться под общий dispatcher.
fn reject_unexpected_service_unit_kwargs<'v>(kwargs: Value<'v>) -> Result<(), starlark::Error> {
    use starlark::values::dict::DictRef;

    let dict = DictRef::from_value(kwargs).ok_or_else(|| {
        starlark::Error::new_other(anyhow::anyhow!(
            "internal: service.unit kwargs is not a dict"
        ))
    })?;
    for (key, _) in dict.iter() {
        let key_str = key.unpack_str().ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "service.unit: kwargs key must be a string, got {}",
                key.get_type()
            ))
        })?;
        if !SERVICE_UNIT_ALLOWED_KWARGS.contains(&key_str) {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "service.unit: unexpected keyword argument {key_str:?}; \
                 use runr.service(...) or systemd.service(...) directly for init-specific options",
            )));
        }
    }
    Ok(())
}

#[starlark_module]
fn template_fn(builder: &mut GlobalsBuilder) {
    /// `template(path, **kwargs)` рендерит шаблон, лежащий **в той же роли/lib,
    /// где определена вызывающая функция** (см. spec секция «module-relative»).
    /// Дополнительные kwargs передаются в template-контекст в виде
    /// одноимённых переменных Jinja. Например:
    /// `template("nginx.conf.j2", inv = my_inv)` → внутри шаблона
    /// `{{ inv.worker_processes }}`.
    fn template<'v>(
        #[starlark(require = pos)] path: &str,
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let defining_module = pick_defining_module(eval)
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("{e}")))?;

        let extra_context = kwargs_to_json(kwargs)?;

        let rendered = with_state(|state| {
            let bundle = state.bundle.as_ref();
            let resolved = bundle.resolve_template(&defining_module, path)?;
            (state.template_fn)(&resolved, path, &extra_context)
                .map_err(|e| StarlarkGlueError::Eval(format!("template('{path}'): {e}")))
        })
        .ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "internal: no eval state in thread-local during template()"
            ))
        })?;

        let rendered = match rendered {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("{e}");
                with_state(|state| state.errors.borrow_mut().push(e));
                return Err(starlark::Error::new_other(anyhow::anyhow!("{msg}")));
            }
        };
        Ok(eval.heap().alloc(rendered))
    }
}

/// Конвертация kwargs Starlark dict → JSON Object.
fn kwargs_to_json<'v>(kwargs: Value<'v>) -> Result<serde_json::Value, starlark::Error> {
    use starlark::values::dict::DictRef;

    let dict = DictRef::from_value(kwargs).ok_or_else(|| {
        starlark::Error::new_other(anyhow::anyhow!("internal: template kwargs not a dict"))
    })?;
    let mut out = serde_json::Map::new();
    for (k, v) in dict.iter() {
        let key = k
            .unpack_str()
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "template: kwargs key must be a string, got {}",
                    k.get_type()
                ))
            })?
            .to_string();
        let json = v.to_json_value().map_err(starlark::Error::new_other)?;
        out.insert(key, json);
    }
    Ok(serde_json::Value::Object(out))
}

/// Определить, из какого .star файла вызвана текущая функция. Алгоритм:
/// 1. Идём по call stack starlark с верхнего фрейма.
/// 2. Берём первый Frame с непустым `location` (то есть user-defined,
///    не native) — `location.filename()` это путь модуля.
/// 3. Возвращаем canonical PathBuf этого файла.
///
/// Если call stack пустой или у всех фреймов location = None — fallback на
/// `current_module` стек из EvalState. Это происходит когда template()
/// вызван из top-level кода модуля (нет user frame для функции на стеке).
fn pick_defining_module<'v>(
    eval: &mut Evaluator<'v, '_, '_>,
) -> Result<PathBuf, StarlarkGlueError> {
    let call_stack = eval.call_stack();
    for frame in call_stack.frames.iter().rev() {
        if let Some(loc) = &frame.location {
            let name = loc.file.filename();
            if !name.is_empty() {
                let p = PathBuf::from(name);
                if p.is_absolute() {
                    return Ok(p);
                }
                // Если starlark отдал относительный путь — возможно, имя
                // символическое (например "test.star" без полного пути).
                // Это нормально для unit-тестов; пробрасываем как есть.
                return Ok(p);
            }
        }
    }
    // Fallback: вершина current_module стека из EvalState (последний пушнутый
    // модуль через ModuleStackGuard). Это срабатывает на top-level
    // template() в роли (когда нет вложенных user-функций).
    let from_stack = current_state()
        .and_then(|s| s.current_module.borrow().last().cloned())
        .ok_or_else(|| {
            StarlarkGlueError::Eval(
                "template(): cannot determine defining module from call stack".to_string(),
            )
        })?;
    Ok(from_stack)
}

fn unpack_inventory_sources<'v>(
    args: &UnpackTuple<Value<'v>>,
) -> Result<Vec<serde_json::Value>, String> {
    let mut out = Vec::with_capacity(args.items.len());
    for v in &args.items {
        let json = v.to_json_value().map_err(|e| format!("{e}"))?;
        out.push(json);
    }
    Ok(out)
}

fn resolve_merge_strategy(arg: &str) -> Result<MergeStrategy, StarlarkGlueError> {
    let chosen = if arg.is_empty() {
        let default = with_state(|state| {
            state
                .bundle
                .metadata
                .inventory
                .default_merge_strategy
                .clone()
        })
        .flatten();
        default.ok_or(StarlarkGlueError::Bundle(
            crate::bundle::BundleError::DefaultMergeStrategyMissing,
        ))?
    } else {
        arg.to_string()
    };
    MergeStrategy::parse(&chosen).map_err(|e| StarlarkGlueError::Eval(format!("{e}")))
}

/// Прочитать yaml по относительному bundle-пути; вернуть JSON.
fn load_inventory_yaml(
    state: &EvalState,
    path: &str,
) -> Result<serde_json::Value, StarlarkGlueError> {
    let bundle_root = state.bundle.root.clone();
    let resolved = match resolve_within_root(&bundle_root, path) {
        Ok(p) => p,
        Err(PathSafetyError::NotFound(_)) => {
            return Err(StarlarkGlueError::Eval(format!(
                "inventory: read '{path}': file not found"
            )));
        }
        Err(other) => {
            return Err(StarlarkGlueError::Bundle(
                crate::bundle::BundleError::PathSafety(other),
            ));
        }
    };

    if let Some(cached) = state.inventory_cache.borrow().get(&resolved) {
        return Ok(cached.clone());
    }
    let text = std::fs::read_to_string(&resolved).map_err(|e| {
        StarlarkGlueError::Bundle(crate::bundle::BundleError::Io {
            path: resolved.to_string_lossy().into_owned(),
            source: e,
        })
    })?;
    let yaml: serde_norway::Value = serde_norway::from_str(&text).map_err(|e| {
        StarlarkGlueError::Bundle(crate::bundle::BundleError::InvalidYaml {
            path: resolved.to_string_lossy().into_owned(),
            source: e,
        })
    })?;
    let json = yaml_to_json(yaml).map_err(StarlarkGlueError::Eval)?;
    state
        .inventory_cache
        .borrow_mut()
        .insert(resolved.clone(), json.clone());
    Ok(json)
}

fn yaml_to_json(v: serde_norway::Value) -> Result<serde_json::Value, String> {
    use serde_norway::Value as Y;
    Ok(match v {
        Y::Null => serde_json::Value::Null,
        Y::Bool(b) => serde_json::Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                return Err(format!("unsupported number in YAML: {n:?}"));
            }
        }
        Y::String(s) => serde_json::Value::String(s),
        Y::Sequence(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for item in seq {
                out.push(yaml_to_json(item)?);
            }
            serde_json::Value::Array(out)
        }
        Y::Mapping(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let key = match k {
                    Y::String(s) => s,
                    other => {
                        return Err(format!("YAML mapping key must be a string; got {other:?}"));
                    }
                };
                out.insert(key, yaml_to_json(v)?);
            }
            serde_json::Value::Object(out)
        }
        Y::Tagged(_) => {
            return Err("YAML tagged values are not supported in bundle inventory".to_string());
        }
    })
}

/// Собрать `@bosun/builtins` FrozenModule из globals.
pub fn build_builtins_module(globals: &Globals) -> starlark::Result<FrozenModule> {
    FrozenModule::from_globals(globals).map_err(starlark::Error::from)
}

/// Конвертация kwargs (dict Value) в `CallArgs`.
fn kwargs_to_call_args<'v>(
    kwargs: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Result<CallArgs, starlark::Error> {
    use starlark::values::dict::DictRef;

    let dict = DictRef::from_value(kwargs).ok_or_else(|| {
        starlark::Error::new_other(anyhow::anyhow!("internal: kwargs is not a dict"))
    })?;
    let mut out: HashMap<String, ArgValue> = HashMap::new();
    for (k, v) in dict.iter() {
        let key = k
            .unpack_str()
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "kwargs key must be a string, got {}",
                    k.get_type()
                ))
            })?
            .to_string();
        out.insert(key, value_to_arg(v, eval)?);
    }
    Ok(CallArgs::new(out))
}

fn value_to_arg<'v>(
    v: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Result<ArgValue, starlark::Error> {
    use starlark::values::list::ListRef;

    if v.is_none() {
        return Ok(ArgValue::Other(serde_json::Value::Null));
    }
    if let Some(s) = v.unpack_str() {
        return Ok(ArgValue::Str(s.to_string()));
    }
    if let Some(b) = v.unpack_bool() {
        return Ok(ArgValue::Bool(b));
    }
    if let Some(i) = v.unpack_i32() {
        return Ok(ArgValue::Int(i64::from(i)));
    }
    if let Some(list) = ListRef::from_value(v) {
        let mut handles: Vec<ResourceId> = Vec::new();
        let mut all_handles = true;
        for item in list.iter() {
            if let Some(h) = item.downcast_ref::<HandleObject>() {
                handles.push(h.id.clone());
            } else {
                all_handles = false;
                break;
            }
        }
        if all_handles && !list.is_empty() {
            return Ok(ArgValue::HandleList(handles));
        }
    }
    let json = value_to_json(v, eval)?;
    Ok(ArgValue::Other(json))
}

fn value_to_json<'v>(
    v: Value<'v>,
    _eval: &mut Evaluator<'v, '_, '_>,
) -> Result<serde_json::Value, starlark::Error> {
    v.to_json_value().map_err(starlark::Error::new_other)
}

/// Универсальная регистрация вызова примитива из Starlark.
fn register_primitive_call<'v>(
    kind_str: &str,
    kwargs: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let mut call_args = kwargs_to_call_args(kwargs, eval)?;

    let kind = match kind_str {
        "apt.package" => ResourceKind::from_static("apt.package"),
        "cert.tls" => ResourceKind::from_static("cert.tls"),
        "file.content" => ResourceKind::from_static("file.content"),
        "file.delete" => ResourceKind::from_static("file.delete"),
        "file.symlink" => ResourceKind::from_static("file.symlink"),
        "runr.service" => ResourceKind::from_static("runr.service"),
        "runr.timer" => ResourceKind::from_static("runr.timer"),
        "runr.cgroup" => ResourceKind::from_static("runr.cgroup"),
        "systemd.service" => ResourceKind::from_static("systemd.service"),
        "systemd.timer" => ResourceKind::from_static("systemd.timer"),
        "process.signal" => ResourceKind::from_static("process.signal"),
        "users.user" => ResourceKind::from_static("users.user"),
        "users.group" => ResourceKind::from_static("users.group"),
        other => ResourceKind::try_new(other).map_err(|e| {
            starlark::Error::new_other(anyhow::anyhow!("invalid resource kind '{other}': {e}"))
        })?,
    };

    let pending_sensitive = if kind_str == "file.content" {
        Some(
            extract_sensitive_contents(&mut call_args).map_err(|reason| {
                with_state(|state| record_invalid_call_in(state, kind_str, &reason))
                    .unwrap_or_else(|| starlark::Error::new_other(anyhow::anyhow!("{reason}")))
            })?,
        )
    } else {
        None
    };

    let (identity, payload, reload_on, restart_on, depends_on) =
        with_state(|state| -> Result<_, starlark::Error> {
            let primitive = state.primitives.get(&kind).ok_or_else(|| {
                let err = StarlarkGlueError::UnknownPrimitive(kind_str.to_string());
                let msg = format!("{err}");
                state.errors.borrow_mut().push(err);
                starlark::Error::new_other(anyhow::anyhow!("{msg}"))
            })?;

            let identity = build_identity(primitive.identity_keys(), &call_args)
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;

            let payload =
                crate::orchestrator::call_primitive(&format!("build_payload {kind_str}"), || {
                    primitive.build_payload(&call_args, &state.plan_ctx)
                })
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;

            let reload_on = call_args
                .optional_handle_list("reload_on")
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;
            let restart_on = call_args
                .optional_handle_list("restart_on")
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;
            let depends_on = call_args
                .optional_handle_list("depends_on")
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;

            Ok((identity, payload, reload_on, restart_on, depends_on))
        })
        .ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!("internal: no eval state in thread-local"))
        })??;

    let id = ResourceId::new(&kind, &identity);

    if let Some(contents) = pending_sensitive {
        with_state(|state| {
            state
                .sensitive
                .put(id.clone(), SensitivePayload::new(contents));
        })
        .ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "internal: no eval state during sensitive.put"
            ))
        })?;
    }

    let resource = Resource {
        id: id.clone(),
        kind: kind.clone(),
        spec_version: 1,
        payload,
        reload_on,
        restart_on,
        depends_on,
    };

    with_state(|state| -> Result<(), starlark::Error> {
        let mut registry = state.registry.borrow_mut();
        registry
            .add(resource)
            .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;
        Ok(())
    })
    .ok_or_else(|| {
        starlark::Error::new_other(anyhow::anyhow!(
            "internal: no eval state during registry.add"
        ))
    })??;

    Ok(eval.heap().alloc(HandleObject { id }))
}

fn extract_sensitive_contents(args: &mut CallArgs) -> Result<String, String> {
    let contents = match args.take_raw("contents") {
        Some(ArgValue::Str(s)) => s,
        Some(other) => {
            return Err(format!(
                "file.content: 'contents' must be str, got {}",
                describe_arg(&other),
            ));
        }
        None => {
            return Err("file.content: missing required argument 'contents'".to_string());
        }
    };
    let sha = sha256_hex(contents.as_bytes());
    let size = i64::try_from(contents.len())
        .map_err(|_| "file.content: contents too large for i64 size".to_string())?;
    args.put_raw("content_sha256", ArgValue::Str(sha));
    args.put_raw("content_size", ArgValue::Int(size));
    Ok(contents)
}

fn describe_arg(v: &ArgValue) -> &'static str {
    match v {
        ArgValue::Str(_) => "str",
        ArgValue::Int(_) => "int",
        ArgValue::Bool(_) => "bool",
        ArgValue::HandleList(_) => "list[Handle]",
        ArgValue::Other(_) => "other",
    }
}

fn record_invalid_call_in(state: &EvalState, kind: &str, reason: &str) -> starlark::Error {
    let err = StarlarkGlueError::InvalidCall {
        kind: kind.to_string(),
        reason: reason.to_string(),
    };
    let msg = format!("{err}");
    state.errors.borrow_mut().push(err);
    starlark::Error::new_other(anyhow::anyhow!("{msg}"))
}

fn build_identity(
    keys: &[&'static str],
    args: &CallArgs,
) -> Result<String, crate::call_args::CallArgsError> {
    let mut parts: Vec<String> = Vec::with_capacity(keys.len());
    for key in keys {
        parts.push(args.required_str(key)?);
    }
    Ok(parts.join(":"))
}

/// Handle, возвращаемый `apt.package` / `file.content`.
#[derive(
    Debug,
    allocative::Allocative,
    starlark::values::NoSerialize,
    starlark::values::ProvidesStaticType,
    starlark::values::Trace,
    starlark::values::Freeze,
)]
pub(crate) struct HandleObject {
    #[allocative(skip)]
    #[trace(static)]
    #[freeze(identity)]
    pub(crate) id: ResourceId,
}

starlark::starlark_simple_value!(HandleObject);

impl std::fmt::Display for HandleObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handle({})", self.id)
    }
}

#[starlark::values::starlark_value(type = "bosun.handle")]
impl<'v> starlark::values::StarlarkValue<'v> for HandleObject {
    type Canonical = Self;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn build_identity_uses_single_key() {
        let mut map = HashMap::new();
        map.insert("name".to_string(), ArgValue::Str("nginx".into()));
        let args = CallArgs::new(map);
        let id = build_identity(&["name"], &args).unwrap();
        assert_eq!(id, "nginx");
    }

    #[test]
    fn build_identity_joins_multiple_keys() {
        let mut map = HashMap::new();
        map.insert("name".to_string(), ArgValue::Str("nginx".into()));
        map.insert("arch".to_string(), ArgValue::Str("amd64".into()));
        let args = CallArgs::new(map);
        let id = build_identity(&["name", "arch"], &args).unwrap();
        assert_eq!(id, "nginx:amd64");
    }

    #[test]
    fn build_globals_compiles() {
        let _g = build_globals();
    }

    #[test]
    fn extract_sensitive_contents_pulls_str_and_injects_sha_size() {
        let mut map = HashMap::new();
        map.insert("path".to_string(), ArgValue::Str("/etc/x".into()));
        map.insert("contents".to_string(), ArgValue::Str("hello".into()));
        let mut args = CallArgs::new(map);
        let body = extract_sensitive_contents(&mut args).unwrap();
        assert_eq!(body, "hello");
        assert!(args.take_raw("contents").is_none());
        assert_eq!(
            args.required_str("content_sha256").unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        );
        assert_eq!(args.optional_u64("content_size").unwrap(), Some(5));
    }
}
