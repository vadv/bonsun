//! Десериализуемая часть payload'а `runr.timer`.

use serde::Deserialize;

/// Целевое состояние timer'а runr.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TimerState {
    /// Таймер должен быть включён и активен.
    Enabled,
    /// Таймер должен быть выключен, но unit-файл остаётся.
    Disabled,
    /// Таймер должен быть остановлен и убран из autostart.
    Absent,
}

/// Спека `runr.timer`.
#[derive(Clone, Debug, Deserialize)]
pub struct RunrTimerSpec {
    /// Имя timer'а (без `.timer` суффикса).
    pub name: String,
    /// Целевое состояние.
    pub state: TimerState,
    /// Запустить таймер прямо сейчас при enable. По умолчанию false:
    /// штатный режим — стартануть по cron-расписанию.
    #[serde(default)]
    pub start_now: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_min_enabled() {
        let json = serde_json::json!({"name": "vacuum", "state": "enabled"});
        let spec: RunrTimerSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "vacuum");
        assert_eq!(spec.state, TimerState::Enabled);
        assert!(!spec.start_now);
    }

    #[test]
    fn deserialize_with_start_now() {
        let json = serde_json::json!({"name": "vacuum", "state": "enabled", "start_now": true});
        let spec: RunrTimerSpec = serde_json::from_value(json).unwrap();
        assert!(spec.start_now);
    }

    #[test]
    fn deserialize_disabled_and_absent() {
        for s in ["disabled", "absent"] {
            let json = serde_json::json!({"name": "x", "state": s});
            let spec: RunrTimerSpec = serde_json::from_value(json).unwrap();
            assert!(matches!(
                spec.state,
                TimerState::Disabled | TimerState::Absent
            ));
        }
    }
}
