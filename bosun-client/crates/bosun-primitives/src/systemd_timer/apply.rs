//! Apply-фаза `systemd.timer`.
//!
//! Логика проще, чем у service: notify-семантики нет (taimer обычно не
//! «рестартят», а пересоздают через `file.content` + `daemon-reload`).
//! Все действия enable/disable/start/stop синхронные.

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};
use bosun_handles::SystemdError;

use super::plan::{decide_timer_action, TimerAction};
use super::spec::SystemdTimerSpec;

pub fn run(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }
    let spec: SystemdTimerSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.timer payload: {e}")))?;
    let Some(systemd) = ctx.systemd.as_ref() else {
        return Err(PrimitiveError::SystemdUnavailable {
            reason: "systemd client not initialized in ApplyCtx".to_string(),
        });
    };

    // Snapshot. NoSuchUnit → not active.
    let before = match systemd.unit_info(&spec.name) {
        Ok(info) => Some(info),
        Err(SystemdError::NoSuchUnit(_)) => None,
        Err(e) => return Err(map_systemd_error(e, "unit_info")),
    };
    let action = decide_timer_action(&spec, before.as_ref());

    match action {
        TimerAction::NoChange => Ok(ChangeReport::no_change()),
        TimerAction::Enable => {
            if spec.enable {
                systemd
                    .enable_unit(&spec.name)
                    .map_err(|e| map_systemd_error(e, "enable_unit"))?;
            }
            systemd
                .start_unit(&spec.name)
                .map_err(|e| map_systemd_error(e, "start_unit"))?;
            Ok(ChangeReport::changed(format!(
                "enabled systemd.timer:{}",
                spec.name
            )))
        }
        TimerAction::Disable => {
            systemd
                .stop_unit(&spec.name)
                .map_err(|e| map_systemd_error(e, "stop_unit"))?;
            systemd
                .disable_unit(&spec.name)
                .map_err(|e| map_systemd_error(e, "disable_unit"))?;
            Ok(ChangeReport::changed(format!(
                "disabled systemd.timer:{}",
                spec.name
            )))
        }
        TimerAction::StopAndDisable => {
            // Stop первым, чтобы между ним и disable не отлетел один тик.
            systemd
                .stop_unit(&spec.name)
                .map_err(|e| map_systemd_error(e, "stop_unit"))?;
            systemd
                .disable_unit(&spec.name)
                .map_err(|e| map_systemd_error(e, "disable_unit"))?;
            Ok(ChangeReport::changed(format!(
                "stopped and disabled systemd.timer:{}",
                spec.name
            )))
        }
    }
}

/// Маппинг идентичен `systemd_service::apply::map_systemd_error`,
/// дублирован чтобы не плодить pub-exposure.
fn map_systemd_error(err: SystemdError, op: &str) -> PrimitiveError {
    match err {
        SystemdError::BusUnavailable { reason, .. } => PrimitiveError::SystemdUnavailable {
            reason: format!("{op}: bus unavailable: {reason}"),
        },
        SystemdError::Dbus(e) => PrimitiveError::SystemdUnavailable {
            reason: format!("{op}: dbus error: {e}"),
        },
        SystemdError::NoSuchUnit(name) => PrimitiveError::Apply {
            reason: format!("systemd unit not found: {name} (during {op})"),
        },
        SystemdError::AuthorizationDenied { action, unit } => PrimitiveError::Apply {
            reason: format!(
                "authorization denied for {action} on {unit} (during {op}); run as root or add a polkit rule",
            ),
        },
        SystemdError::JobFailed {
            job,
            result,
            active_state,
        } => PrimitiveError::Apply {
            reason: format!(
                "systemd job {job} failed during {op}: result={result}, active_state={active_state}",
            ),
        },
        SystemdError::RestartNotObserved { unit } => PrimitiveError::Apply {
            reason: format!("restart of {unit} not observed (op={op})"),
        },
        SystemdError::Timeout(d) => PrimitiveError::SystemdUnavailable {
            reason: format!("{op}: timeout after {d:?} waiting for job"),
        },
        SystemdError::Io(e) => PrimitiveError::Io {
            context: format!("systemd {op}"),
            source: e,
        },
        other => PrimitiveError::Apply {
            reason: format!("systemd error during {op}: {other}"),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::{atomic::AtomicU32, Arc};
    use std::time::{Duration, Instant};

    use bosun_core::defers::Journal;
    use bosun_core::{
        ApplyCtx, Diff, PrimitiveError, Resource, ResourceId, ResourceKind, SensitiveStore,
    };
    use bosun_handles::{SystemdError, SystemdHandle, UnitInfo};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::systemd_timer::spec::TimerState;

    struct MockSystemd {
        calls: Mutex<Vec<String>>,
        unit_info_queue: Mutex<Vec<Result<UnitInfo, SystemdError>>>,
        _daemon_reload_count: AtomicU32,
    }

    impl MockSystemd {
        fn new() -> Self {
            Self {
                calls: Mutex::new(vec![]),
                unit_info_queue: Mutex::new(vec![]),
                _daemon_reload_count: AtomicU32::new(0),
            }
        }
        fn enqueue_unit_info(&self, info: Result<UnitInfo, SystemdError>) {
            self.unit_info_queue.lock().unwrap().push(info);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, label: &str) {
            self.calls.lock().unwrap().push(label.to_string());
        }
    }

    impl SystemdHandle for MockSystemd {
        fn daemon_reload(&self) -> Result<(), SystemdError> {
            self.record("daemon_reload");
            Ok(())
        }
        fn needs_daemon_reload(&self, _: &str) -> Result<bool, SystemdError> {
            Ok(false)
        }
        fn start_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("start_unit:{name}"));
            Ok(())
        }
        fn stop_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("stop_unit:{name}"));
            Ok(())
        }
        fn restart_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("restart_unit:{name}"));
            Ok(())
        }
        fn reload_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("reload_unit:{name}"));
            Ok(())
        }
        fn enable_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("enable_unit:{name}"));
            Ok(())
        }
        fn disable_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("disable_unit:{name}"));
            Ok(())
        }
        fn unit_info(&self, name: &str) -> Result<UnitInfo, SystemdError> {
            self.record(&format!("unit_info:{name}"));
            let mut queue = self.unit_info_queue.lock().unwrap();
            if queue.is_empty() {
                return Err(SystemdError::NoSuchUnit(name.to_string()));
            }
            queue.remove(0)
        }
    }

    fn make_unit(active: &str) -> UnitInfo {
        UnitInfo {
            name: "logrotate.timer".to_string(),
            active_state: active.to_string(),
            sub_state: "running".to_string(),
            invocation_id: String::new(),
            exec_main_start_timestamp: None,
        }
    }

    fn make_resource(name: &str, state: TimerState, enable: bool) -> Resource {
        let kind = ResourceKind::from_static("systemd.timer");
        let id = ResourceId::new(&kind, name);
        let state_str = match state {
            TimerState::Enabled => "enabled",
            TimerState::Disabled => "disabled",
            TimerState::Absent => "absent",
        };
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "name": name,
                "state": state_str,
                "enable": enable,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn make_ctx(systemd: Option<Arc<dyn SystemdHandle>>) -> (TempDir, ApplyCtx) {
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            defers,
            None,
            systemd,
        );
        (tmp, ctx)
    }

    fn force_update(r: &Resource) -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: r.payload.clone(),
            description: "converge".into(),
        }
    }

    #[test]
    fn apply_no_change_for_diff_no_change() {
        let mock = Arc::new(MockSystemd::new());
        let r = make_resource("logrotate.timer", TimerState::Enabled, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn apply_systemd_none_returns_deferrable_unavailable() {
        let r = make_resource("logrotate.timer", TimerState::Enabled, true);
        let (_tmp, ctx) = make_ctx(None);
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        assert!(err.is_deferrable());
        match err {
            PrimitiveError::SystemdUnavailable { .. } => {}
            other => panic!("expected SystemdUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn apply_enable_inactive_calls_enable_and_start() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("inactive")));
        let r = make_resource("logrotate.timer", TimerState::Enabled, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        assert!(calls.iter().any(|c| c == "enable_unit:logrotate.timer"));
        assert!(calls.iter().any(|c| c == "start_unit:logrotate.timer"));
    }

    #[test]
    fn apply_enable_false_only_starts_unit_no_enable() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("inactive")));
        let r = make_resource("logrotate.timer", TimerState::Enabled, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| c == "enable_unit:logrotate.timer"));
        assert!(calls.iter().any(|c| c == "start_unit:logrotate.timer"));
    }

    #[test]
    fn apply_already_active_is_no_change() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("active")));
        let r = make_resource("logrotate.timer", TimerState::Enabled, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(!report.changed);
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| c.starts_with("enable_unit:")));
        assert!(!calls.iter().any(|c| c.starts_with("start_unit:")));
    }

    #[test]
    fn apply_disable_active_calls_stop_and_disable() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("active")));
        let r = make_resource("logrotate.timer", TimerState::Disabled, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        assert!(calls.iter().any(|c| c == "stop_unit:logrotate.timer"));
        assert!(calls.iter().any(|c| c == "disable_unit:logrotate.timer"));
    }

    #[test]
    fn apply_absent_active_calls_stop_then_disable() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("active")));
        let r = make_resource("logrotate.timer", TimerState::Absent, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        let stop_idx = calls
            .iter()
            .position(|c| c == "stop_unit:logrotate.timer")
            .unwrap();
        let disable_idx = calls
            .iter()
            .position(|c| c == "disable_unit:logrotate.timer")
            .unwrap();
        assert!(stop_idx < disable_idx, "stop должен идти ДО disable");
    }

    #[test]
    fn apply_no_such_unit_pre_snapshot_then_enable_starts_unit() {
        let mock = Arc::new(MockSystemd::new());
        // unit_info → NoSuchUnit → not active, enable path.
        mock.enqueue_unit_info(Err(SystemdError::NoSuchUnit("x.timer".into())));
        let r = make_resource("x.timer", TimerState::Enabled, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        assert!(calls.iter().any(|c| c == "enable_unit:x.timer"));
        assert!(calls.iter().any(|c| c == "start_unit:x.timer"));
    }
}
