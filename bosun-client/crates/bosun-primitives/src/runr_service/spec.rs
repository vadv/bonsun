//! Десериализуемая часть payload'а `runr.service`.
//!
//! Хранится в `Resource.payload` после `build_payload`. План и apply
//! читают её через `serde_json::from_value`.

use bosun_core::defers::HealthCheck;
use serde::Deserialize;

/// Желаемое состояние сервиса. Соответствует тройке из chiit и design-секции
/// «Decide action».
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServiceState {
    /// Сервис должен быть запущен и оставаться запущен.
    Running,
    /// Сервис должен быть остановлен. Unit-файл при этом остаётся на диске.
    Stopped,
    /// Сервис должен быть остановлен. Желаемое значение пересекается со
    /// `Stopped` на стороне примитива runr.service — реальный demolish
    /// unit'а (удаление файла) делает отдельный примитив `file.content`
    /// над `/etc/runr/<name>.service`. Различие нужно бoлее высокому
    /// уровню (`service.unit` Phase F), который собирает обе операции.
    Absent,
}

/// Спека `runr.service`, как она лежит в `Resource.payload`.
///
/// Поле `restart_on` не дублируется в spec'е: notify-связи живут в
/// `Resource.restart_on`/`reload_on` (см. design «Notify-связи»).
#[derive(Clone, Debug, Deserialize)]
pub struct RunrServiceSpec {
    /// Имя unit'а (без `.service` суффикса — runr принимает голое имя).
    pub name: String,
    /// Целевое состояние.
    pub state: ServiceState,
    /// Включить autostart unit'а. По умолчанию false: runr не требует
    /// явного enable для запуска (`start_now`), но autostart управляется
    /// отдельным флагом. См. design «Native primitives → runr.service».
    #[serde(default)]
    pub enable: bool,
    /// Опциональный health-check после restart/reload. Сам unit-rendering
    /// не зависит от health-check'а; spec несёт его сквозь, чтобы defer
    /// мог запустить probe после replay.
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    /// Опциональный validate-cmd: запускается ДО enqueue defer'а
    /// restart/reload. Полная семантика — в Phase H.
    #[serde(default)]
    pub validate_with: Option<Vec<String>>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum_running() {
        let json = serde_json::json!({"name": "postgres", "state": "running"});
        let spec: RunrServiceSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "postgres");
        assert_eq!(spec.state, ServiceState::Running);
        assert!(!spec.enable);
        assert!(spec.health_check.is_none());
        assert!(spec.validate_with.is_none());
    }

    #[test]
    fn deserialize_stopped_and_absent() {
        let s1: RunrServiceSpec =
            serde_json::from_value(serde_json::json!({"name": "x", "state": "stopped"})).unwrap();
        assert_eq!(s1.state, ServiceState::Stopped);
        let s2: RunrServiceSpec =
            serde_json::from_value(serde_json::json!({"name": "x", "state": "absent"})).unwrap();
        assert_eq!(s2.state, ServiceState::Absent);
    }

    #[test]
    fn deserialize_with_enable_and_health_check() {
        let json = serde_json::json!({
            "name": "pg",
            "state": "running",
            "enable": true,
            "health_check": {
                "kind": "url",
                "url": "http://127.0.0.1/healthz",
                "expected_status": 200,
                "timeout_sec": 5,
                "retry_count": 3,
                "retry_interval_sec": 2,
            },
            "validate_with": ["pgbouncer", "-V", "{new_path}"],
        });
        let spec: RunrServiceSpec = serde_json::from_value(json).unwrap();
        assert!(spec.enable);
        assert!(matches!(
            spec.health_check,
            Some(HealthCheck::Url { ref url, .. }) if url == "http://127.0.0.1/healthz"
        ));
        assert_eq!(
            spec.validate_with.as_deref(),
            Some(["pgbouncer".to_string(), "-V".into(), "{new_path}".into()].as_slice())
        );
    }

    #[test]
    fn deserialize_unknown_state_is_error() {
        let json = serde_json::json!({"name": "x", "state": "unknown"});
        let err = serde_json::from_value::<RunrServiceSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "got: {err}");
    }
}
