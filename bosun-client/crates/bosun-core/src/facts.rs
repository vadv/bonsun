use std::time::Duration;

use serde::Serialize;

use crate::resource::ResourceKind;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
#[non_exhaustive]
pub enum FactValue {
    Known(serde_json::Value),
    Unknown {
        reason: String,
    },
    Stale {
        value: serde_json::Value,
        age_ms: u64,
    },
}

impl FactValue {
    pub fn known(v: impl Into<serde_json::Value>) -> Self {
        Self::Known(v.into())
    }

    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }

    pub fn stale(value: impl Into<serde_json::Value>, age: Duration) -> Self {
        Self::Stale {
            value: value.into(),
            age_ms: age.as_millis() as u64,
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, FactValue::Known(_))
    }

    /// Достаёт значение если Known или Stale. Unknown → None.
    pub fn value(&self) -> Option<&serde_json::Value> {
        match self {
            FactValue::Known(v) | FactValue::Stale { value: v, .. } => Some(v),
            FactValue::Unknown { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshPolicy {
    AtStart,
    AfterApply { triggers: Vec<ResourceKind> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FactCategory {
    Static,
    Slow,
    Live,
    Discovery,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn known_factories() {
        let v = FactValue::known(serde_json::json!({"hostname": "abc"}));
        assert!(v.is_known());
        assert_eq!(v.value().unwrap()["hostname"], "abc");
    }

    #[test]
    fn unknown_has_no_value() {
        let v = FactValue::unknown("io error");
        assert!(!v.is_known());
        assert!(v.value().is_none());
    }

    #[test]
    fn stale_has_value_but_not_known() {
        let v = FactValue::stale(serde_json::json!(42), Duration::from_secs(5));
        assert!(!v.is_known());
        assert_eq!(v.value().unwrap(), &serde_json::json!(42));
    }

    #[test]
    fn fact_value_serializes_known_variants() {
        // Adjacently-tagged enum: {"state": "...", "value": ...}.
        // Тег вынесен в отдельное поле, поэтому value может быть любым
        // JSON-типом — null, bool, число, строка, массив, объект.
        let cases: Vec<serde_json::Value> = vec![
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(42),
            serde_json::json!("hostname-abc"),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({"key": "val"}),
        ];
        for case in cases {
            let fact = FactValue::Known(case.clone());
            let serialized = serde_json::to_string(&fact).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
            assert_eq!(parsed["state"], "known", "input={case}");
            assert_eq!(parsed["value"], case, "input={case}");
        }
    }

    #[test]
    fn fact_value_serializes_stale_variants() {
        let cases: Vec<serde_json::Value> = vec![serde_json::json!(42), serde_json::json!("x")];
        for case in cases {
            let fact = FactValue::Stale {
                value: case.clone(),
                age_ms: 1000,
            };
            let serialized = serde_json::to_string(&fact).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
            assert_eq!(parsed["state"], "stale", "input={case}");
            // Поля struct-варианта при adjacent tagging вложены в value.
            assert_eq!(parsed["value"]["value"], case, "input={case}");
            assert_eq!(parsed["value"]["age_ms"], 1000, "input={case}");
        }
    }

    #[test]
    fn fact_value_serializes_unknown_variant() {
        let fact = FactValue::Unknown {
            reason: "io error".into(),
        };
        let serialized = serde_json::to_string(&fact).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["state"], "unknown");
        assert_eq!(parsed["value"]["reason"], "io error");
    }
}
