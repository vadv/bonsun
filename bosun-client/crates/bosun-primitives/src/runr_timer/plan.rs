//! Plan-фаза `runr.timer`.
//!
//! runr таймер — это lightweight unit; решение «нужно ли что-то делать»
//! принимается на apply через сравнение `runr.timer_statuses()` с spec'ом.
//! План возвращает `Update` — apply на месте может вернуть `no_change`.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};
use bosun_runr_client::TimerStatus;

use super::spec::{RunrTimerSpec, TimerState};

/// Целевое apply-действие над таймером.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimerAction {
    /// Без изменений.
    NoChange,
    /// Enable + (опционально) start now.
    Enable { start_now: bool },
    /// Disable (без force-stop, runr сам остановит таймер).
    Disable,
    /// Полное выключение: stop + disable.
    StopAndDisable,
}

pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: RunrTimerSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.timer payload: {e}")))?;
    Ok(Diff::Update {
        from: serde_json::json!({"runr_timer": "current pending lookup"}),
        to: resource.payload.clone(),
        description: format!("converge runr.timer:{}", spec.name),
    })
}

/// Решение действия по spec и snapshot'у. `current` берётся из ответа
/// `runr.timer_statuses()`, по полю `name`. Если таймера ещё нет в snapshot'е,
/// считаем `enabled=false`.
pub fn decide_timer_action(spec: &RunrTimerSpec, current: Option<&TimerStatus>) -> TimerAction {
    let enabled_now = current.and_then(|t| t.enabled).unwrap_or(false);
    match spec.state {
        TimerState::Enabled => {
            if enabled_now {
                TimerAction::NoChange
            } else {
                TimerAction::Enable {
                    start_now: spec.start_now,
                }
            }
        }
        TimerState::Disabled => {
            if enabled_now {
                TimerAction::Disable
            } else {
                TimerAction::NoChange
            }
        }
        TimerState::Absent => {
            if enabled_now {
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

    fn status(name: &str, enabled: Option<bool>) -> TimerStatus {
        TimerStatus {
            name: name.to_string(),
            state: "Stopped".to_string(),
            next_run: None,
            target_service: "x".to_string(),
            enabled,
        }
    }

    fn spec(state: TimerState, start_now: bool) -> RunrTimerSpec {
        RunrTimerSpec {
            name: "vacuum".into(),
            state,
            start_now,
        }
    }

    #[test]
    fn enabled_when_disabled_returns_enable() {
        let cur = status("vacuum", Some(false));
        let a = decide_timer_action(&spec(TimerState::Enabled, false), Some(&cur));
        assert_eq!(a, TimerAction::Enable { start_now: false });
    }

    #[test]
    fn enabled_when_already_enabled_is_no_change() {
        let cur = status("vacuum", Some(true));
        let a = decide_timer_action(&spec(TimerState::Enabled, true), Some(&cur));
        assert_eq!(a, TimerAction::NoChange);
    }

    #[test]
    fn enabled_with_start_now_propagates_flag() {
        let cur = status("vacuum", Some(false));
        let a = decide_timer_action(&spec(TimerState::Enabled, true), Some(&cur));
        assert_eq!(a, TimerAction::Enable { start_now: true });
    }

    #[test]
    fn disabled_when_enabled_is_disable() {
        let cur = status("vacuum", Some(true));
        let a = decide_timer_action(&spec(TimerState::Disabled, false), Some(&cur));
        assert_eq!(a, TimerAction::Disable);
    }

    #[test]
    fn disabled_when_already_disabled_is_no_change() {
        let cur = status("vacuum", Some(false));
        let a = decide_timer_action(&spec(TimerState::Disabled, false), Some(&cur));
        assert_eq!(a, TimerAction::NoChange);
    }

    #[test]
    fn absent_when_enabled_is_stop_and_disable() {
        let cur = status("vacuum", Some(true));
        let a = decide_timer_action(&spec(TimerState::Absent, false), Some(&cur));
        assert_eq!(a, TimerAction::StopAndDisable);
    }

    #[test]
    fn enabled_when_unknown_is_enable() {
        let a = decide_timer_action(&spec(TimerState::Enabled, false), None);
        assert_eq!(a, TimerAction::Enable { start_now: false });
    }

    #[test]
    fn disabled_when_unknown_is_no_change() {
        // Таймер отсутствует в snapshot (или ещё не зарегистрирован) →
        // enabled_now=false. desired=Disabled → NoChange.
        let a = decide_timer_action(&spec(TimerState::Disabled, false), None);
        assert_eq!(a, TimerAction::NoChange);
    }

    #[test]
    fn absent_when_unknown_is_no_change() {
        // Таймер отсутствует → enabled_now=false. desired=Absent → NoChange.
        // Иначе попытка stop+disable вернула бы NotFound от runr.
        let a = decide_timer_action(&spec(TimerState::Absent, false), None);
        assert_eq!(a, TimerAction::NoChange);
    }

    #[test]
    fn enabled_when_unknown_with_start_now_propagates_flag() {
        // Покрывает start_now=true для unknown → enable_now сценария.
        let a = decide_timer_action(&spec(TimerState::Enabled, true), None);
        assert_eq!(a, TimerAction::Enable { start_now: true });
    }
}
