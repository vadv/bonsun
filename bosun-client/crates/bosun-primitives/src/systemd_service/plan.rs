//! Plan-фаза `systemd.service`.
//!
//! Структурно повторяет `runr_service::plan`: фактический status берётся
//! только на apply-стадии (dbus-вызов в plan был бы тяжёл для dry-run).
//! Plan возвращает `Update` — apply на месте проверит реальный
//! `ActiveState` через `unit_info`.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};
use bosun_handles::UnitInfo;

use super::spec::{ServiceState, SystemdServiceSpec};

/// Действие, которое apply должен выполнить над systemd unit'ом. Та же
/// форма, что у `runr_service::Action`, но решается против
/// `UnitInfo.active_state`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Без изменений.
    NoChange,
    /// Unit должен быть запущен и сейчас не запущен.
    Start,
    /// Unit должен быть остановлен и сейчас запущен.
    Stop,
    /// Тригер от `restart_on`: жёстко перезапустить.
    Restart,
    /// Тригер от `reload_on`: послать SIGHUP / `ReloadUnit`.
    Reload,
}

/// systemd-овский маркер «unit активен». Совпадает с поведением
/// `systemctl is-active` (которое возвращает 0 на `active`).
const ACTIVE_STATE: &str = "active";

pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: SystemdServiceSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service payload: {e}")))?;

    // На plan-стадии у нас нет дешёвого способа узнать текущий
    // `ActiveState`: dbus-вызов в plan был бы слишком тяжёлым для
    // dry-run. Возвращаем Update — apply на месте сравнит spec с
    // unit_info и вернёт `ChangeReport::no_change()`, если совпадёт.
    Ok(Diff::Update {
        from: serde_json::json!({"systemd": "current state pending lookup"}),
        to: resource.payload.clone(),
        description: format!("converge systemd.service:{}", spec.name),
    })
}

/// Главная decide-таблица. Симметрична `decide_action_runr`, но
/// проверяет `current.active_state == "active"`.
///
/// Матрица:
///
/// | desired \ current | "active"                                                 | not "active" |
/// |-------------------|----------------------------------------------------------|--------------|
/// | Running           | Restart если restart_on, Reload если reload_on, иначе NoChange | Start  |
/// | Stopped           | Stop                                                     | NoChange     |
/// | Absent            | Stop                                                     | NoChange     |
///
/// Если `current is None` (т.е. `unit_info` ещё не вызван) — считаем
/// `not active`: на свежей ноде первое apply запустит unit. Decide тестируется
/// без вызова unit_info.
pub fn decide_action_systemd(
    spec: &SystemdServiceSpec,
    current: Option<&UnitInfo>,
    restart_triggered: bool,
    reload_triggered: bool,
) -> Action {
    let is_active = current
        .map(|u| u.active_state == ACTIVE_STATE)
        .unwrap_or(false);
    match spec.state {
        ServiceState::Running => {
            if is_active {
                if restart_triggered {
                    Action::Restart
                } else if reload_triggered {
                    Action::Reload
                } else {
                    Action::NoChange
                }
            } else {
                Action::Start
            }
        }
        ServiceState::Stopped | ServiceState::Absent => {
            if is_active {
                Action::Stop
            } else {
                Action::NoChange
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
            name: "nginx.service".to_string(),
            active_state: active.to_string(),
            sub_state: "running".to_string(),
            invocation_id: "abc".to_string(),
            exec_main_start_timestamp: Some(100),
        }
    }

    fn spec(state: ServiceState) -> SystemdServiceSpec {
        SystemdServiceSpec {
            name: "nginx.service".into(),
            state,
            enable: true,
            health_check: None,
            validate_with: None,
        }
    }

    // -- Running × active, без тригеров → NoChange.
    #[test]
    fn running_active_no_triggers_is_no_change() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("active")),
            false,
            false,
        );
        assert_eq!(action, Action::NoChange);
    }

    // -- Running × active × restart_triggered → Restart.
    #[test]
    fn running_active_restart_trigger_is_restart() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("active")),
            true,
            false,
        );
        assert_eq!(action, Action::Restart);
    }

    // -- Running × active × reload_triggered → Reload.
    #[test]
    fn running_active_reload_trigger_is_reload() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("active")),
            false,
            true,
        );
        assert_eq!(action, Action::Reload);
    }

    // -- Running × active × оба тригера → Restart (restart > reload).
    #[test]
    fn running_active_both_triggers_prefers_restart() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("active")),
            true,
            true,
        );
        assert_eq!(action, Action::Restart);
    }

    // -- Running × inactive → Start.
    #[test]
    fn running_inactive_is_start() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("inactive")),
            false,
            false,
        );
        assert_eq!(action, Action::Start);
    }

    // -- Running × failed → Start.
    #[test]
    fn running_failed_is_start() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("failed")),
            false,
            false,
        );
        assert_eq!(action, Action::Start);
    }

    // -- Running × activating (transient) → Start. Это сознательное решение:
    // на момент plan'а unit «не active», поэтому повторно дёрнем start.
    // systemd сам отбросит no-op job через JOB_MODE_REPLACE.
    #[test]
    fn running_activating_is_start() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("activating")),
            false,
            false,
        );
        assert_eq!(action, Action::Start);
    }

    // -- Running × None (unit_info ещё не вызван) → Start.
    #[test]
    fn running_unknown_is_start() {
        let action = decide_action_systemd(&spec(ServiceState::Running), None, false, false);
        assert_eq!(action, Action::Start);
    }

    // -- Running × inactive × restart_triggered → Start (notify ничего не меняет
    // если unit не запущен).
    #[test]
    fn running_inactive_with_notify_is_still_start() {
        let action = decide_action_systemd(
            &spec(ServiceState::Running),
            Some(&unit("inactive")),
            true,
            true,
        );
        assert_eq!(action, Action::Start);
    }

    // -- Stopped × active → Stop.
    #[test]
    fn stopped_active_is_stop() {
        let action = decide_action_systemd(
            &spec(ServiceState::Stopped),
            Some(&unit("active")),
            false,
            false,
        );
        assert_eq!(action, Action::Stop);
    }

    // -- Stopped × inactive → NoChange.
    #[test]
    fn stopped_inactive_is_no_change() {
        let action = decide_action_systemd(
            &spec(ServiceState::Stopped),
            Some(&unit("inactive")),
            false,
            false,
        );
        assert_eq!(action, Action::NoChange);
    }

    // -- Stopped × active × restart_triggered → Stop (notify не меняет desired-stop).
    #[test]
    fn stopped_active_with_restart_trigger_is_still_stop() {
        let action = decide_action_systemd(
            &spec(ServiceState::Stopped),
            Some(&unit("active")),
            true,
            false,
        );
        assert_eq!(action, Action::Stop);
    }

    // -- Absent × active → Stop.
    #[test]
    fn absent_active_is_stop() {
        let action = decide_action_systemd(
            &spec(ServiceState::Absent),
            Some(&unit("active")),
            false,
            false,
        );
        assert_eq!(action, Action::Stop);
    }

    // -- Absent × inactive → NoChange.
    #[test]
    fn absent_inactive_is_no_change() {
        let action = decide_action_systemd(
            &spec(ServiceState::Absent),
            Some(&unit("inactive")),
            false,
            false,
        );
        assert_eq!(action, Action::NoChange);
    }

    // -- Absent × None → NoChange.
    #[test]
    fn absent_unknown_is_no_change() {
        let action = decide_action_systemd(&spec(ServiceState::Absent), None, false, false);
        assert_eq!(action, Action::NoChange);
    }

    // -- Stopped × None → NoChange.
    #[test]
    fn stopped_unknown_is_no_change() {
        let action = decide_action_systemd(&spec(ServiceState::Stopped), None, false, false);
        assert_eq!(action, Action::NoChange);
    }
}
