//! Десериализуемая часть payload'а `apt.package`.
//!
//! Хранится в `Resource.payload` после `build_payload`. Plan и apply
//! читают её через `serde_json::from_value`.

use serde::Deserialize;

/// Спека `apt.package`, как она лежит в `Resource.payload`.
#[derive(Deserialize, Debug, Clone)]
pub struct AptPackageSpec {
    /// Имя пакета (например, "nginx").
    pub name: String,
    /// Опциональная конкретная версия. Если задана — apt будет ставить
    /// её через `name=version`. Если None — последнюю кандидатную.
    #[serde(default)]
    pub version: Option<String>,
    /// Per-resource дедлайн на весь install (вместе с recovery). 600 секунд
    /// хватает на тяжёлые пакеты (`postgresql`, `mariadb`, `linux-headers-*`)
    /// при медленных зеркалах.
    #[serde(default = "default_timeout_sec")]
    pub timeout_sec: u32,
}

const fn default_timeout_sec() -> u32 {
    600
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum_required_only_name() {
        let json = serde_json::json!({ "name": "nginx" });
        let spec: AptPackageSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "nginx");
        assert!(spec.version.is_none());
        assert_eq!(spec.timeout_sec, 600);
    }

    #[test]
    fn deserialize_with_version() {
        let json = serde_json::json!({ "name": "nginx", "version": "1.18.0-6.1" });
        let spec: AptPackageSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.version.as_deref(), Some("1.18.0-6.1"));
    }

    #[test]
    fn deserialize_with_explicit_timeout() {
        let json = serde_json::json!({ "name": "postgresql", "timeout_sec": 1800 });
        let spec: AptPackageSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.timeout_sec, 1800);
    }

    #[test]
    fn deserialize_missing_name_is_error() {
        let json = serde_json::json!({ "version": "1.0" });
        let err = serde_json::from_value::<AptPackageSpec>(json).unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn deserialize_explicit_null_version_keeps_none() {
        let json = serde_json::json!({ "name": "nginx", "version": serde_json::Value::Null });
        let spec: AptPackageSpec = serde_json::from_value(json).unwrap();
        assert!(spec.version.is_none());
    }
}
