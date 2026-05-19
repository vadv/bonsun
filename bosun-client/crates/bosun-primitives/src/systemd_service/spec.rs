//! Десериализуемая часть payload'а `systemd.service`.
//!
//! Структура зеркалит `RunrServiceSpec` — общие поля, общая семантика
//! `state`. Различие в дефолтах: для systemd unit'ы стандартно
//! enable'ятся при manage'е, поэтому `enable` по умолчанию `true`. Для
//! runr autostart — это отдельный флаг в INI, и default `false`
//! сохраняет explicit-режим.

use bosun_core::defers::HealthCheck;
use bosun_core::UnitName;
use serde::Deserialize;

/// Желаемое состояние systemd unit'а. Совпадает по форме с
/// `runr_service::ServiceState`, но интерпретируется через `ActiveState`
/// (а не runr-овский `state`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServiceState {
    /// Unit должен быть `active` и оставаться им.
    Running,
    /// Unit должен быть остановлен (`ActiveState != active`). Unit-файл
    /// остаётся.
    Stopped,
    /// Семантически совпадает со `Stopped` на стороне примитива —
    /// удаление unit-файла делает отдельный `file.content`.
    Absent,
}

/// Возвращает default для поля `enable`. systemd-units стандартно
/// enable'ятся при manage'е: cluster'ы chiit и puppet всегда зовут
/// `EnableUnitFiles`, поэтому default `true` соответствует ожиданиям
/// оператора. Для отключения — `enable = false` явно.
const fn enable_default() -> bool {
    true
}

/// Спека `systemd.service`, как она лежит в `Resource.payload`.
#[derive(Clone, Debug, Deserialize)]
pub struct SystemdServiceSpec {
    /// Имя unit'а (с расширением `.service` либо без — systemd
    /// нормализует сам).
    /// Валидация имени через `UnitName` отвергает path-traversal, пробелы
    /// и не-ASCII символы прямо на десериализации payload'а.
    pub name: UnitName,
    /// Целевое состояние.
    pub state: ServiceState,
    /// Включить unit (`EnableUnitFiles`). По умолчанию `true`.
    #[serde(default = "enable_default")]
    pub enable: bool,
    /// Health-check после restart/reload. Сам unit-rendering не зависит
    /// от health-check'а; spec несёт его сквозь, чтобы defer мог
    /// запустить probe после replay (Phase I).
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    /// Validate-cmd: запускается ДО enqueue defer'а restart/reload.
    /// Полная семантика — Phase H.
    #[serde(default)]
    pub validate_with: Option<Vec<String>>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum_defaults_enable_true() {
        let json = serde_json::json!({"name": "nginx.service", "state": "running"});
        let spec: SystemdServiceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name.as_str(), "nginx.service");
        assert_eq!(spec.state, ServiceState::Running);
        // Это отличие от runr.service.
        assert!(spec.enable);
        assert!(spec.health_check.is_none());
        assert!(spec.validate_with.is_none());
    }

    #[test]
    fn deserialize_rejects_invalid_unit_name() {
        let json = serde_json::json!({"name": "/etc/passwd", "state": "running"});
        let err = serde_json::from_value::<SystemdServiceSpec>(json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must start with") || msg.contains("invalid character"),
            "expected UnitName error, got: {msg}"
        );
    }

    #[test]
    fn deserialize_enable_false_explicit() {
        let json =
            serde_json::json!({"name": "nginx.service", "state": "running", "enable": false});
        let spec: SystemdServiceSpec = serde_json::from_value(json).unwrap();
        assert!(!spec.enable);
    }

    #[test]
    fn deserialize_stopped_and_absent() {
        let s1: SystemdServiceSpec =
            serde_json::from_value(serde_json::json!({"name": "x.service", "state": "stopped"}))
                .unwrap();
        assert_eq!(s1.state, ServiceState::Stopped);
        let s2: SystemdServiceSpec =
            serde_json::from_value(serde_json::json!({"name": "x.service", "state": "absent"}))
                .unwrap();
        assert_eq!(s2.state, ServiceState::Absent);
    }

    #[test]
    fn deserialize_with_health_check_and_validate_with() {
        let json = serde_json::json!({
            "name": "nginx.service",
            "state": "running",
            "health_check": {
                "kind": "url",
                "url": "http://127.0.0.1/healthz",
                "expected_status": 200,
                "timeout_sec": 5,
                "retry_count": 3,
                "retry_interval_sec": 2,
            },
            "validate_with": ["nginx", "-t", "-c", "{new_path}"],
        });
        let spec: SystemdServiceSpec = serde_json::from_value(json).unwrap();
        assert!(matches!(
            spec.health_check,
            Some(HealthCheck::Url { ref url, .. }) if url == "http://127.0.0.1/healthz"
        ));
        assert_eq!(
            spec.validate_with.as_deref(),
            Some(
                [
                    "nginx".to_string(),
                    "-t".into(),
                    "-c".into(),
                    "{new_path}".into()
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn deserialize_unknown_state_is_error() {
        let json = serde_json::json!({"name": "x.service", "state": "reloading"});
        let err = serde_json::from_value::<SystemdServiceSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "got: {err}");
    }
}
