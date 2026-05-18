use std::time::Duration;

use serde::Serialize;

use crate::resource::ResourceKind;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
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
    fn fact_value_serializes_with_state_tag() {
        // Внутренне-тегированный enum в serde требует, чтобы newtype-вариант
        // содержал map-подобное значение — иначе тег некуда внедрить. Поэтому
        // оборачиваем в объект {hostname: "x"}; для serialization-тестов это
        // достаточно, а scalar-факты при необходимости диагностики дампятся
        // через value() и не через сам enum.
        let v = FactValue::Known(serde_json::json!({"hostname": "x"}));
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j["state"], "known");
        assert_eq!(j["hostname"], "x");
    }
}
