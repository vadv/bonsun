//! Plan-фаза `systemd.timer`.
//!
//! systemd timer = unit type, методы `start_unit`/`stop_unit` работают
//! на нём один-к-одному. Plan возвращает `Update` — apply на месте
//! проверит `ActiveState` через `unit_info` и решит действие.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};
use bosun_handles::UnitInfo;

use super::spec::{SystemdTimerSpec, TimerState};

/// Действие, которое apply должен выполнить над таймером.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimerAction {
    /// Без изменений.
    NoChange,
    /// Enable (+ optionally start если spec.enable=true).
    Enable,
    /// Disable + остановка таймера.
    Disable,
    /// Полное выключение: stop + disable.
    StopAndDisable,
}

const ACTIVE_STATE: &str = "active";

pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: SystemdTimerSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.timer payload: {e}")))?;
    Ok(Diff::Update {
        from: serde_json::json!({"systemd_timer": "current pending lookup"}),
        to: resource.payload.clone(),
        description: format!("converge systemd.timer:{}", spec.name),
    })
}

/// Решение действия. Для systemd `active_state == "active"` означает,
/// что timer запущен; иначе — нет. Если `current is None` (NoSuchUnit),
/// трактуем как «не активен».
pub fn decide_timer_action(spec: &SystemdTimerSpec, current: Option<&UnitInfo>) -> TimerAction {
    let is_active = current
        .map(|u| u.active_state == ACTIVE_STATE)
        .unwrap_or(false);
    match spec.state {
        TimerState::Enabled => {
            if is_active {
                TimerAction::NoChange
            } else {
                TimerAction::Enable
            }
        }
        TimerState::Disabled => {
            if is_active {
                TimerAction::Disable
            } else {
                TimerAction::NoChange
            }
        }
        TimerState::Absent => {
            if is_active {
                TimerAction::StopAndDisable
            } else {
                TimerAction::NoChange
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn unit(active: &str) -> UnitInfo {
        UnitInfo {
            name: "logrotate.timer".to_string(),
            active_state: active.to_string(),
            sub_state: "running".to_string(),
            invocation_id: String::new(),
            exec_main_start_timestamp: None,
        }
    }

    fn spec(state: TimerState) -> SystemdTimerSpec {
        SystemdTimerSpec {
            name: "logrotate.timer".into(),
            state,
            enable: true,
        }
    }

    #[test]
    fn enabled_when_inactive_returns_enable() {
        let a = decide_timer_action(&spec(TimerState::Enabled), Some(&unit("inactive")));
        assert_eq!(a, TimerAction::Enable);
    }

    #[test]
    fn enabled_when_already_active_is_no_change() {
        let a = decide_timer_action(&spec(TimerState::Enabled), Some(&unit("active")));
        assert_eq!(a, TimerAction::NoChange);
    }

    #[test]
    fn disabled_when_active_is_disable() {
        let a = decide_timer_action(&spec(TimerState::Disabled), Some(&unit("active")));
        assert_eq!(a, TimerAction::Disable);
    }

    #[test]
    fn disabled_when_already_inactive_is_no_change() {
        let a = decide_timer_action(&spec(TimerState::Disabled), Some(&unit("inactive")));
        assert_eq!(a, TimerAction::NoChange);
    }

    #[test]
    fn absent_when_active_is_stop_and_disable() {
        let a = decide_timer_action(&spec(TimerState::Absent), Some(&unit("active")));
        assert_eq!(a, TimerAction::StopAndDisable);
    }

    #[test]
    fn absent_when_inactive_is_no_change() {
        let a = decide_timer_action(&spec(TimerState::Absent), Some(&unit("inactive")));
        assert_eq!(a, TimerAction::NoChange);
    }

    #[test]
    fn enabled_when_unknown_is_enable() {
        let a = decide_timer_action(&spec(TimerState::Enabled), None);
        assert_eq!(a, TimerAction::Enable);
    }

    #[test]
    fn absent_when_unknown_is_no_change() {
        let a = decide_timer_action(&spec(TimerState::Absent), None);
        assert_eq!(a, TimerAction::NoChange);
    }
}
