//! Plan-фаза `runr.service`.
//!
//! Сравнивает spec со снимком `runr.service_statuses()`, возвращает Diff.
//! Решение действия (`Action`) принимается на apply-фазе через
//! `decide_action_runr`, потому что зависит от состояния notify-флагов
//! `ctx.is_changed(...)`, недоступных в plan_ctx.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};
use bosun_runr_client::ServiceStatus;

use super::spec::{RunrServiceSpec, ServiceState};

/// Действие, которое apply должен выполнить над runr-сервисом. Решение
/// принимается матрицей `desired × current × notify`, см.
/// `decide_action_runr`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Без изменений: целевое состояние совпадает с текущим, notify не
    /// тригерил.
    NoChange,
    /// Сервис должен быть запущен и сейчас не запущен.
    Start,
    /// Сервис должен быть остановлен (или absent) и сейчас запущен.
    Stop,
    /// Тригер от `restart_on`: жёстко перезапустить, превалирует над reload.
    Restart,
    /// Тригер от `reload_on`: послать SIGHUP, если unit поддерживает.
    Reload,
}

const RUNNING_STATE: &str = "Running";

/// Главная функция plan: десериализует spec, выбирает Diff. Фактический
/// статус берётся не из FactsSource (его нет для runr), а из cache в
/// ApplyCtx — поэтому plan может только сказать «возможно change», а
/// apply уточнит через `decide_action_runr`.
pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let _spec: RunrServiceSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service payload: {e}")))?;

    // На plan-стадии у нас нет дешёвого способа узнать, нужно ли что-то
    // делать: HTTP-вызов в plan был бы слишком тяжёлым для dry-run.
    // Возвращаем Update — apply на месте проверит реальное состояние и
    // если совпадёт со spec, вернёт `ChangeReport::no_change()`.
    Ok(Diff::Update {
        from: serde_json::json!({"runr": "current state pending lookup"}),
        to: resource.payload.clone(),
        description: format!("converge runr.service:{}", _spec.name),
    })
}

/// Главная decide-таблица. Возвращает `Action` по тройке (spec.state,
/// current state, notify-флаги).
///
/// Матрица (из design «Decide action»):
///
/// | desired \ current | "Running"                                                | not "Running"  |
/// |-------------------|----------------------------------------------------------|----------------|
/// | Running           | Restart если restart_on triggered, Reload если reload_on triggered, иначе NoChange | Start          |
/// | Stopped           | Stop                                                     | NoChange       |
/// | Absent            | Stop                                                     | NoChange       |
///
/// Если `current` отсутствует в snapshot'е (сервис ещё не известен runr),
/// считаем «not Running»: на свежей ноде первое apply запустит сервис.
pub fn decide_action_runr(
    spec: &RunrServiceSpec,
    current: Option<&ServiceStatus>,
    restart_triggered: bool,
    reload_triggered: bool,
) -> Action {
    let is_running = current.map(|s| s.state == RUNNING_STATE).unwrap_or(false);
    match spec.state {
        ServiceState::Running => {
            if is_running {
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
            if is_running {
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

    fn status(name: &str, state: &str) -> ServiceStatus {
        ServiceStatus {
            name: name.to_string(),
            state: state.to_string(),
            pid: None,
            restarts: 0,
            in_state_for_ms: 0,
            uptime_ms: None,
            downtime_ms: None,
            next_restart_in_ms: None,
            started_at: None,
            autostart: false,
            memory_rss_anon_bytes: 0,
            memory_rss_file_bytes: 0,
            cpu_usage_percent: 0.0,
        }
    }

    fn spec(state: ServiceState) -> RunrServiceSpec {
        RunrServiceSpec {
            name: "pg".into(),
            state,
            enable: false,
            health_check: None,
            validate_with: None,
        }
    }

    // -- Running × Running, без тригеров → NoChange.
    #[test]
    fn running_running_no_triggers_is_no_change() {
        let running = status("pg", "Running");
        let action = decide_action_runr(&spec(ServiceState::Running), Some(&running), false, false);
        assert_eq!(action, Action::NoChange);
    }

    // -- Running × Running × restart_triggered → Restart.
    #[test]
    fn running_running_restart_trigger_is_restart() {
        let running = status("pg", "Running");
        let action = decide_action_runr(&spec(ServiceState::Running), Some(&running), true, false);
        assert_eq!(action, Action::Restart);
    }

    // -- Running × Running × reload_triggered → Reload.
    #[test]
    fn running_running_reload_trigger_is_reload() {
        let running = status("pg", "Running");
        let action = decide_action_runr(&spec(ServiceState::Running), Some(&running), false, true);
        assert_eq!(action, Action::Reload);
    }

    // -- Running × Running × оба тригера → Restart (restart субсумирует reload).
    #[test]
    fn running_running_both_triggers_prefers_restart() {
        let running = status("pg", "Running");
        let action = decide_action_runr(&spec(ServiceState::Running), Some(&running), true, true);
        assert_eq!(action, Action::Restart);
    }

    // -- Running × Stopped → Start.
    #[test]
    fn running_stopped_is_start() {
        let stopped = status("pg", "Stopped");
        let action = decide_action_runr(&spec(ServiceState::Running), Some(&stopped), false, false);
        assert_eq!(action, Action::Start);
    }

    // -- Running × unknown (отсутствует в snapshot) → Start.
    #[test]
    fn running_unknown_is_start() {
        let action = decide_action_runr(&spec(ServiceState::Running), None, false, false);
        assert_eq!(action, Action::Start);
    }

    // -- Running × Stopped × restart_triggered → Start (notify не имеет смысла, если не запущен).
    #[test]
    fn running_stopped_with_notify_is_still_start() {
        let stopped = status("pg", "Stopped");
        let action = decide_action_runr(&spec(ServiceState::Running), Some(&stopped), true, true);
        assert_eq!(action, Action::Start);
    }

    // -- Stopped × Running → Stop.
    #[test]
    fn stopped_running_is_stop() {
        let running = status("pg", "Running");
        let action = decide_action_runr(&spec(ServiceState::Stopped), Some(&running), false, false);
        assert_eq!(action, Action::Stop);
    }

    // -- Stopped × Stopped → NoChange.
    #[test]
    fn stopped_stopped_is_no_change() {
        let stopped = status("pg", "Stopped");
        let action = decide_action_runr(&spec(ServiceState::Stopped), Some(&stopped), false, false);
        assert_eq!(action, Action::NoChange);
    }

    // -- Stopped × Running × restart_triggered → Stop (notify ничего не меняет
    // если desired-state — стоп).
    #[test]
    fn stopped_running_with_restart_trigger_is_still_stop() {
        let running = status("pg", "Running");
        let action = decide_action_runr(&spec(ServiceState::Stopped), Some(&running), true, false);
        assert_eq!(action, Action::Stop);
    }

    // -- Absent × Running → Stop.
    #[test]
    fn absent_running_is_stop() {
        let running = status("pg", "Running");
        let action = decide_action_runr(&spec(ServiceState::Absent), Some(&running), false, false);
        assert_eq!(action, Action::Stop);
    }

    // -- Absent × Stopped → NoChange.
    #[test]
    fn absent_stopped_is_no_change() {
        let stopped = status("pg", "Stopped");
        let action = decide_action_runr(&spec(ServiceState::Absent), Some(&stopped), false, false);
        assert_eq!(action, Action::NoChange);
    }

    // -- Absent × unknown → NoChange.
    #[test]
    fn absent_unknown_is_no_change() {
        let action = decide_action_runr(&spec(ServiceState::Absent), None, false, false);
        assert_eq!(action, Action::NoChange);
    }

    // -- Stopped × unknown → NoChange.
    #[test]
    fn stopped_unknown_is_no_change() {
        let action = decide_action_runr(&spec(ServiceState::Stopped), None, false, false);
        assert_eq!(action, Action::NoChange);
    }

    // -- State "Failed" (не Running) трактуется как not running → Start.
    #[test]
    fn running_failed_is_start() {
        let failed = status("pg", "Failed");
        let action = decide_action_runr(&spec(ServiceState::Running), Some(&failed), false, false);
        assert_eq!(action, Action::Start);
    }

    // -- State "Starting" не равно "Running" → Start.
    #[test]
    fn running_starting_is_start() {
        let starting = status("pg", "Starting");
        let action =
            decide_action_runr(&spec(ServiceState::Running), Some(&starting), false, false);
        assert_eq!(action, Action::Start);
    }
}
