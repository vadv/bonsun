use std::collections::HashMap;

use crate::resource::ResourceId;

/// Помощник для парсинга именованных аргументов Starlark-вызова
/// в типизированные значения. Создаётся glue-слоем и передаётся
/// в Primitive::build_payload.
pub struct CallArgs {
    inner: HashMap<String, ArgValue>,
}

#[derive(Clone, Debug)]
pub enum ArgValue {
    Str(String),
    Int(i64),
    Bool(bool),
    HandleList(Vec<ResourceId>),
    Other(serde_json::Value),
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CallArgsError {
    #[error("missing required argument '{0}'")]
    Missing(String),
    #[error("argument '{name}' has wrong type: expected {expected}, got {actual}")]
    WrongType {
        name: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("argument '{name}' value {value} out of range for {target}")]
    OutOfRange {
        name: String,
        value: i64,
        target: &'static str,
    },
}

impl CallArgs {
    pub fn new(args: HashMap<String, ArgValue>) -> Self {
        Self { inner: args }
    }

    /// Извлечь значение по имени, удалив его из CallArgs. Используется
    /// Starlark-glue для side-channel'ов: например, `file.content.contents`
    /// перехватывается из args до того, как они попадут в `build_payload`.
    pub fn take_raw(&mut self, name: &str) -> Option<ArgValue> {
        self.inner.remove(name)
    }

    /// Положить значение по имени. Используется Starlark-glue для замены
    /// `contents` на `content_sha256`+`content_size` перед `build_payload`.
    pub fn put_raw(&mut self, name: &str, value: ArgValue) {
        self.inner.insert(name.to_string(), value);
    }

    pub fn required_str(&self, name: &str) -> Result<String, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Str(s)) => Ok(s.clone()),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "str",
                actual: type_name(other),
            }),
            None => Err(CallArgsError::Missing(name.into())),
        }
    }

    pub fn optional_str(&self, name: &str) -> Result<Option<String>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Str(s)) => Ok(Some(s.clone())),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "str",
                actual: type_name(other),
            }),
            None => Ok(None),
        }
    }

    pub fn optional_u32(&self, name: &str) -> Result<Option<u32>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Int(i)) => {
                // u32::try_from(i64) сам ловит и отрицательные значения, и
                // выход за u32::MAX — заменяет ручной диапазон-чек и
                // потенциально lossy `as u32`.
                let value = u32::try_from(*i).map_err(|_| CallArgsError::OutOfRange {
                    name: name.into(),
                    value: *i,
                    target: "u32",
                })?;
                Ok(Some(value))
            }
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "int",
                actual: type_name(other),
            }),
            None => Ok(None),
        }
    }

    /// Аналог `optional_u32`, но допускает значения до `i64::MAX` — нужен,
    /// например, для `file.content.content_size`, где спека требует u64.
    /// Отрицательные значения возвращают `OutOfRange`.
    pub fn optional_u64(&self, name: &str) -> Result<Option<u64>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Int(i)) => {
                let value = u64::try_from(*i).map_err(|_| CallArgsError::OutOfRange {
                    name: name.into(),
                    value: *i,
                    target: "u64",
                })?;
                Ok(Some(value))
            }
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "int",
                actual: type_name(other),
            }),
            None => Ok(None),
        }
    }

    /// Опциональный bool-аргумент. None если не передан.
    pub fn optional_bool(&self, name: &str) -> Result<Option<bool>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Bool(b)) => Ok(Some(*b)),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "bool",
                actual: type_name(other),
            }),
            None => Ok(None),
        }
    }

    pub fn optional_handle_list(&self, name: &str) -> Result<Vec<ResourceId>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::HandleList(v)) => Ok(v.clone()),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "list[Handle]",
                actual: type_name(other),
            }),
            None => Ok(Vec::new()),
        }
    }

    /// Список строк из starlark-литерала. Starlark-glue превращает list[str]
    /// в `ArgValue::Other(json_array)`, поэтому здесь распаковываем JSON.
    /// `Some(_)` гарантирует, что аргумент был передан (даже как `[]`);
    /// `None` — поле опустили.
    pub fn optional_str_list(&self, name: &str) -> Result<Option<Vec<String>>, CallArgsError> {
        match self.inner.get(name) {
            Some(ArgValue::Other(serde_json::Value::Array(items))) => {
                let mut out = Vec::with_capacity(items.len());
                for (idx, item) in items.iter().enumerate() {
                    match item {
                        serde_json::Value::String(s) => out.push(s.clone()),
                        _ => {
                            return Err(CallArgsError::WrongType {
                                name: format!("{name}[{idx}]"),
                                expected: "str",
                                actual: "non-string json",
                            });
                        }
                    }
                }
                Ok(Some(out))
            }
            Some(ArgValue::Other(serde_json::Value::Null)) | None => Ok(None),
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "list[str]",
                actual: type_name(other),
            }),
        }
    }
}

fn type_name(v: &ArgValue) -> &'static str {
    match v {
        ArgValue::Str(_) => "str",
        ArgValue::Int(_) => "int",
        ArgValue::Bool(_) => "bool",
        ArgValue::HandleList(_) => "list[Handle]",
        ArgValue::Other(_) => "other",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make(pairs: &[(&str, ArgValue)]) -> CallArgs {
        let map = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        CallArgs::new(map)
    }

    #[test]
    fn required_str_present() {
        let args = make(&[("name", ArgValue::Str("nginx".into()))]);
        assert_eq!(args.required_str("name").unwrap(), "nginx");
    }

    #[test]
    fn required_str_missing() {
        let args = make(&[]);
        let err = args.required_str("name").unwrap_err();
        assert!(matches!(err, CallArgsError::Missing(_)));
    }

    #[test]
    fn required_str_wrong_type() {
        let args = make(&[("name", ArgValue::Int(1))]);
        let err = args.required_str("name").unwrap_err();
        assert!(matches!(err, CallArgsError::WrongType { .. }));
    }

    #[test]
    fn optional_str_absent_returns_none() {
        let args = make(&[]);
        assert!(args.optional_str("version").unwrap().is_none());
    }

    #[test]
    fn optional_u32_out_of_range() {
        let args = make(&[("mode", ArgValue::Int(-1))]);
        let err = args.optional_u32("mode").unwrap_err();
        assert!(matches!(err, CallArgsError::OutOfRange { .. }));
    }

    #[test]
    fn optional_u32_in_range() {
        let args = make(&[("mode", ArgValue::Int(0o644))]);
        assert_eq!(args.optional_u32("mode").unwrap(), Some(0o644));
    }

    #[test]
    fn optional_handle_list_default_empty() {
        let args = make(&[]);
        assert!(args.optional_handle_list("reload_on").unwrap().is_empty());
    }

    #[test]
    fn optional_bool_present_true() {
        let args = make(&[("k", ArgValue::Bool(true))]);
        assert_eq!(args.optional_bool("k").unwrap(), Some(true));
    }

    #[test]
    fn optional_bool_absent_returns_none() {
        let args = make(&[]);
        assert_eq!(args.optional_bool("k").unwrap(), None);
    }

    #[test]
    fn optional_bool_wrong_type_is_error() {
        let args = make(&[("k", ArgValue::Int(1))]);
        let err = args.optional_bool("k").unwrap_err();
        assert!(matches!(err, CallArgsError::WrongType { .. }));
    }

    #[test]
    fn optional_u64_accepts_value_above_u32_max() {
        // 5 GB не помещается в u32, должно работать в u64.
        let value: i64 = (u32::MAX as i64) + 1;
        let args = make(&[("size", ArgValue::Int(value))]);
        assert_eq!(args.optional_u64("size").unwrap(), Some(value as u64));
    }

    #[test]
    fn optional_u64_rejects_negative() {
        let args = make(&[("size", ArgValue::Int(-1))]);
        let err = args.optional_u64("size").unwrap_err();
        assert!(matches!(err, CallArgsError::OutOfRange { .. }));
    }

    #[test]
    fn optional_u64_absent_returns_none() {
        let args = make(&[]);
        assert!(args.optional_u64("size").unwrap().is_none());
    }

    #[test]
    fn take_raw_removes_value() {
        let mut args = make(&[("k", ArgValue::Str("v".into()))]);
        let taken = args.take_raw("k");
        assert!(matches!(taken, Some(ArgValue::Str(s)) if s == "v"));
        // Второй take даёт None.
        assert!(args.take_raw("k").is_none());
    }

    #[test]
    fn put_raw_inserts_value() {
        let mut args = make(&[]);
        args.put_raw("k", ArgValue::Int(42));
        let taken = args.take_raw("k");
        assert!(matches!(taken, Some(ArgValue::Int(42))));
    }

    #[test]
    fn put_raw_overwrites_existing() {
        let mut args = make(&[("k", ArgValue::Str("old".into()))]);
        args.put_raw("k", ArgValue::Str("new".into()));
        let taken = args.take_raw("k");
        assert!(matches!(taken, Some(ArgValue::Str(s)) if s == "new"));
    }

    #[test]
    fn optional_str_list_absent_returns_none() {
        let args = make(&[]);
        assert!(args.optional_str_list("sans").unwrap().is_none());
    }

    #[test]
    fn optional_str_list_explicit_null_returns_none() {
        // starlark `None` приходит как `Other(Null)`: трактуем как absent.
        let args = make(&[("sans", ArgValue::Other(serde_json::Value::Null))]);
        assert!(args.optional_str_list("sans").unwrap().is_none());
    }

    #[test]
    fn optional_str_list_parses_string_array() {
        let json = serde_json::json!(["a", "b", "c"]);
        let args = make(&[("sans", ArgValue::Other(json))]);
        let got = args.optional_str_list("sans").unwrap().unwrap();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn optional_str_list_empty_array_returns_some_empty() {
        let json = serde_json::json!([]);
        let args = make(&[("sans", ArgValue::Other(json))]);
        let got = args.optional_str_list("sans").unwrap().unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn optional_str_list_rejects_non_string_element() {
        let json = serde_json::json!(["ok", 42]);
        let args = make(&[("sans", ArgValue::Other(json))]);
        let err = args.optional_str_list("sans").unwrap_err();
        assert!(matches!(err, CallArgsError::WrongType { .. }));
    }

    #[test]
    fn optional_str_list_rejects_wrong_type() {
        let args = make(&[("sans", ArgValue::Str("not a list".into()))]);
        let err = args.optional_str_list("sans").unwrap_err();
        // Сверяем строкой через Display: вариант WrongType содержит
        // expected="list[str]", и эта подстрока попадает в Display.
        let msg = format!("{err}");
        assert!(
            msg.contains("expected list[str]"),
            "expected WrongType list[str], got: {msg}",
        );
    }
}
