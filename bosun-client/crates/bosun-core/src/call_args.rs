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
                if *i < 0 || *i > i64::from(u32::MAX) {
                    Err(CallArgsError::OutOfRange {
                        name: name.into(),
                        value: *i,
                        target: "u32",
                    })
                } else {
                    Ok(Some(*i as u32))
                }
            }
            Some(other) => Err(CallArgsError::WrongType {
                name: name.into(),
                expected: "int",
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
}
