//! Native-globals для Starlark: `apt.package`, `file.content`, `template`.
//!
//! Архитектура:
//! - `apt`, `file` — Starlark `namespace`-ы с методами через
//!   `GlobalsBuilder::namespace`. То есть `apt.package(...)` — это вызов
//!   функции `package` внутри namespace `apt`.
//! - `template` — top-level функция.
//! - `inv` — устанавливается в Module через `install_inv` перед eval_module,
//!   потому что зависит от inventory конкретного запуска.
//!
//! Native-функции читают разделяемое состояние из thread-local через
//! `with_state(...)` (см. `mod.rs::CURRENT_STATE`).

use std::collections::HashMap;

use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::values::{FreezeResult, Value, ValueLike};
use starlark_derive::starlark_module;

use crate::call_args::{ArgValue, CallArgs};
use crate::digest::sha256_hex;
use crate::resource::{Resource, ResourceId, ResourceKind};
use crate::sensitive::SensitivePayload;
use crate::starlark_glue::inv_object::InvObject;
use crate::starlark_glue::{with_state, StarlarkGlueError};

/// Globals для bosun-манифеста. Включает namespaces `apt`, `file` и функцию
/// `template`, плюс стандартную библиотеку starlark.
pub fn build_globals() -> Globals {
    GlobalsBuilder::standard()
        .with_namespace("apt", apt_namespace)
        .with_namespace("file", file_namespace)
        .with(template_fn)
        .build()
}

#[starlark_module]
fn apt_namespace(builder: &mut GlobalsBuilder) {
    /// Зарегистрировать `apt.package`. Возвращает Handle, который можно
    /// передать в `reload_on=[...]`/`depends_on=[...]` других ресурсов.
    fn package<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("apt.package", kwargs, eval)
    }
}

#[starlark_module]
fn file_namespace(builder: &mut GlobalsBuilder) {
    /// Зарегистрировать `file.content`.
    fn content<'v>(
        #[starlark(kwargs)] kwargs: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        register_primitive_call("file.content", kwargs, eval)
    }
}

#[starlark_module]
fn template_fn(builder: &mut GlobalsBuilder) {
    /// `template(path)` рендерит шаблон `<bundle>/templates/<path>` через
    /// инжектируемый closure из `EvalState`. CLI собирает closure с
    /// забэканным templates_root, inv-копией и materialized-фактами.
    fn template<'v>(
        #[starlark(require = pos)] path: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let rendered: Result<String, anyhow::Error> = with_state(|state| (state.template_fn)(path))
            .ok_or_else(|| {
                anyhow::anyhow!("internal: no eval state in thread-local during template()")
            })?;
        let rendered = rendered
            .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("template('{path}'): {e}")))?;
        Ok(eval.heap().alloc(rendered))
    }
}

/// Собрать `@bosun/builtins` FrozenModule. Экспортирует `apt`, `file`,
/// `template` через `import_public_symbols` из globals.
///
/// Под капотом: создаём пустой module, копируем в него apt/file/template
/// из готовых Globals (через `FrozenModule::from_globals` → потом
/// import_public_symbols в свежий module → freeze).
pub fn build_builtins_module(globals: &Globals) -> starlark::Result<FrozenModule> {
    // Globals можно превратить в FrozenModule, тогда все top-level имена
    // (apt, file, template) станут публичными символами модуля. Имена
    // standard-library (str, list, dict, fail, ...) тоже попадут — это OK,
    // потому что они не конфликтуют с user-кодом и load() выбирает
    // только нужные.
    FrozenModule::from_globals(globals).map_err(starlark::Error::from)
}

/// Установить `inv` как module-level переменную перед запуском evaluate.
pub(crate) fn install_inv(module: &Module, inventory: serde_json::Value) {
    let inv = InvObject::root(inventory);
    module.set("inv", module.heap().alloc(inv));
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

/// Конвертация одного Starlark Value в `ArgValue`. Распознаёт строку, int,
/// bool, list-of-handles. Остальное идёт через JSON-fallback.
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
    // Все обращения к state сгруппированы внутри `with_state` —
    // вне него thread-local не trapped.
    let mut call_args = kwargs_to_call_args(kwargs, eval)?;

    let kind = match kind_str {
        "apt.package" => ResourceKind::from_static("apt.package"),
        "file.content" => ResourceKind::from_static("file.content"),
        other => ResourceKind::try_new(other).map_err(|e| {
            starlark::Error::new_other(anyhow::anyhow!("invalid resource kind '{other}': {e}"))
        })?,
    };

    // Side-channel для file.content.contents — секреты не лежат в Resource.payload.
    // Извлекаем contents из args, считаем sha256+size, заменяем contents на
    // content_sha256+content_size, сам тело кладём в SensitiveStore.
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

    let (identity, payload, reload_on, depends_on) =
        with_state(|state| -> Result<_, starlark::Error> {
            let primitive = state.primitives.get(&kind).ok_or_else(|| {
                let err = StarlarkGlueError::UnknownPrimitive(kind_str.to_string());
                let msg = format!("{err}");
                state.errors.borrow_mut().push(err);
                starlark::Error::new_other(anyhow::anyhow!("{msg}"))
            })?;

            let identity = build_identity(primitive.identity_keys(), &call_args)
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;

            let payload = primitive
                .build_payload(&call_args, &state.plan_ctx)
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;

            let reload_on = call_args
                .optional_handle_list("reload_on")
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;
            let depends_on = call_args
                .optional_handle_list("depends_on")
                .map_err(|e| record_invalid_call_in(state, kind_str, &format!("{e}")))?;

            Ok((identity, payload, reload_on, depends_on))
        })
        .ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!("internal: no eval state in thread-local"))
        })??;

    let id = ResourceId::new(&kind, &identity);

    // Положить sensitive contents в store ровно один раз, когда у нас уже
    // вычислен ResourceId. Если registry.add ниже упадёт (дубликат), запись
    // в store останется — это не утечка, потому что store очищается вместе
    // с EvalState. Чисто в плане «не положили лишнего» — alternative было
    // бы откатывать put на ошибке, что усложняет инвариант.
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

/// Извлекает `contents: str` из `CallArgs`, заменяя на `content_sha256` и
/// `content_size`. Возвращает оригинальное тело, чтобы caller положил его в
/// SensitiveStore.
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
    // CallArgs::optional_u64 принимает Int и проверяет range — поэтому кладём как Int.
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

fn record_invalid_call_in(
    state: &crate::starlark_glue::EvalState,
    kind: &str,
    reason: &str,
) -> starlark::Error {
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

/// Handle, возвращаемый `apt.package` / `file.content`. Передаётся в
/// `reload_on=[...]` / `depends_on=[...]` других ресурсов.
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
    fn build_identity_missing_key_is_error() {
        let args = CallArgs::new(HashMap::new());
        let err = build_identity(&["name"], &args).unwrap_err();
        assert!(matches!(err, crate::call_args::CallArgsError::Missing(_)));
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
        // contents удалён, sha и size добавлены.
        assert!(args.take_raw("contents").is_none());
        assert_eq!(
            args.required_str("content_sha256").unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        );
        assert_eq!(args.optional_u64("content_size").unwrap(), Some(5));
    }

    #[test]
    fn extract_sensitive_contents_missing_is_error() {
        let mut args = CallArgs::new(HashMap::new());
        let err = extract_sensitive_contents(&mut args).unwrap_err();
        assert!(err.contains("missing"));
        assert!(err.contains("contents"));
    }

    #[test]
    fn extract_sensitive_contents_wrong_type_is_error() {
        let mut map = HashMap::new();
        map.insert("contents".to_string(), ArgValue::Int(42));
        let mut args = CallArgs::new(map);
        let err = extract_sensitive_contents(&mut args).unwrap_err();
        assert!(err.contains("must be str"));
    }

    #[test]
    fn extract_sensitive_contents_empty_string_ok() {
        let mut map = HashMap::new();
        map.insert("contents".to_string(), ArgValue::Str(String::new()));
        let mut args = CallArgs::new(map);
        let body = extract_sensitive_contents(&mut args).unwrap();
        assert_eq!(body, "");
        // Пустая строка → sha256 пустой строки, size = 0.
        assert_eq!(
            args.required_str("content_sha256").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(args.optional_u64("content_size").unwrap(), Some(0));
    }
}
