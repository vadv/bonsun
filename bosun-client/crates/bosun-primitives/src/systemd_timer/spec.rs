//! Десериализуемая часть payload'а `systemd.timer`.

use bosun_core::UnitName;
use serde::Deserialize;

/// Целевое состояние systemd timer'а.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerState {
    /// Таймер должен быть включён и активен.
    Enabled,
    /// Таймер должен быть остановлен, unit-файл остаётся.
    Disabled,
    /// То же, что Disabled, на стороне примитива: удаление unit-файла —
    /// задача отдельного file.content'а.
    Absent,
}

/// Возвращает default для `enable`: systemd-таймеры по умолчанию
/// enable'ятся при manage'е (как и `systemd.service`).
const fn enable_default() -> bool {
    true
}

/// Спека `systemd.timer`. По форме совпадает с `runr.timer`, но без
/// `start_now` (для systemd-таймера старт делается тем же `enable_unit`
/// с дальнейшим `start_unit`).
#[derive(Clone, Debug, Deserialize)]
pub struct SystemdTimerSpec {
    /// Имя timer'а (с расширением `.timer` либо без — systemd
    /// нормализует сам).
    /// Валидация через `UnitName` отвергает path-traversal и не-ASCII.
    pub name: UnitName,
    /// Целевое состояние.
    pub state: TimerState,
    /// `EnableUnitFiles` на стороне systemd. По умолчанию true.
    #[serde(default = "enable_default")]
    pub enable: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_min_enabled_default_enable_true() {
        let json = serde_json::json!({"name": "logrotate.timer", "state": "enabled"});
        let spec: SystemdTimerSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name.as_str(), "logrotate.timer");
        assert_eq!(spec.state, TimerState::Enabled);
        assert!(spec.enable);
    }

    #[test]
    fn deserialize_rejects_invalid_unit_name() {
        let json = serde_json::json!({"name": "foo bar", "state": "enabled"});
        let err = serde_json::from_value::<SystemdTimerSpec>(json).unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn deserialize_enable_false_explicit() {
        let json = serde_json::json!({
            "name": "logrotate.timer",
            "state": "enabled",
            "enable": false,
        });
        let spec: SystemdTimerSpec = serde_json::from_value(json).unwrap();
        assert!(!spec.enable);
    }

    #[test]
    fn deserialize_disabled_and_absent() {
        for s in ["disabled", "absent"] {
            let json = serde_json::json!({"name": "x.timer", "state": s});
            let spec: SystemdTimerSpec = serde_json::from_value(json).unwrap();
            assert!(matches!(
                spec.state,
                TimerState::Disabled | TimerState::Absent
            ));
        }
    }

    #[test]
    fn deserialize_unknown_state_is_error() {
        let json = serde_json::json!({"name": "x.timer", "state": "paused"});
        let err = serde_json::from_value::<SystemdTimerSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "got: {err}");
    }
}
