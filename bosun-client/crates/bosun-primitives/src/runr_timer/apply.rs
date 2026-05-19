//! Apply-фаза `runr.timer`.
//!
//! Логика проще, чем для service: таймеры не требуют notify-семантики,
//! enable/disable — desired-state операции. Все вызовы синхронные.
//!
//! Snapshot `timer_statuses` запрашивается заново на каждый ресурс. Кэш
//! per-apply снят сознательно (см. `runr_service::apply` и memory
//! `feedback_bosun_no_cache_for_runr_systemd`): HTTP к runr на loopback
//! дёшев, а stale snapshot мешает корректно реагировать на изменения,
//! которые соседние примитивы сделали в этом же apply.

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};
use bosun_runr_client::RunrError;

use super::plan::{decide_timer_action, TimerAction};
use super::spec::RunrTimerSpec;

pub fn run(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }
    let spec: RunrTimerSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.timer payload: {e}")))?;
    let Some(runr) = ctx.runr.as_ref() else {
        return Err(PrimitiveError::RunrUnavailable {
            base_url: "n/a".to_string(),
            reason: "runr client not initialized in ApplyCtx".to_string(),
        });
    };

    let timers = runr
        .timer_statuses()
        .map_err(|e| map_runr_error(e, runr.base_url(), "timer_statuses"))?;
    let current = timers.iter().find(|t| t.name == spec.name);
    let action = decide_timer_action(&spec, current);

    match action {
        TimerAction::NoChange => Ok(ChangeReport::no_change()),
        TimerAction::Enable { start_now } => {
            runr.timer_enable(&spec.name, start_now)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_enable"))?;
            Ok(ChangeReport::changed(format!(
                "enabled runr.timer:{} (start_now={})",
                spec.name, start_now
            )))
        }
        TimerAction::Disable => {
            runr.timer_disable(&spec.name, false)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_disable"))?;
            Ok(ChangeReport::changed(format!(
                "disabled runr.timer:{}",
                spec.name
            )))
        }
        TimerAction::StopAndDisable => {
            // Stop первым, чтобы между ним и disable не отлетел один тик.
            runr.timer_stop(&spec.name)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_stop"))?;
            runr.timer_disable(&spec.name, true)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_disable"))?;
            Ok(ChangeReport::changed(format!(
                "stopped and disabled runr.timer:{}",
                spec.name
            )))
        }
    }
}

/// Маппинг идентичен `runr_service::apply::map_runr_error`. Вынесено в
/// локальную функцию, чтобы избежать pub-exposure.
fn map_runr_error(err: RunrError, base_url: &str, op: &str) -> PrimitiveError {
    match err {
        RunrError::Unavailable { base_url, source } => PrimitiveError::RunrUnavailable {
            base_url,
            reason: format!("{op}: {source}"),
        },
        RunrError::NotFound { kind, name } => PrimitiveError::Apply {
            reason: format!("runr {kind} not found: {name} (during {op})"),
        },
        RunrError::ApiError { status, body } => PrimitiveError::Apply {
            reason: format!("runr API error during {op}: status={status}, body={body}"),
        },
        RunrError::BadResponse(msg) => PrimitiveError::Apply {
            reason: format!("runr returned invalid JSON during {op}: {msg}"),
        },
        RunrError::RestartNotObserved { unit } => PrimitiveError::Apply {
            reason: format!("runr restart of {unit} not observed (op={op})"),
        },
        RunrError::Io(e) => PrimitiveError::RunrUnavailable {
            base_url: base_url.to_string(),
            reason: format!("{op}: i/o error: {e}"),
        },
        other => PrimitiveError::Apply {
            reason: format!("runr error during {op}: {other}"),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtx, Diff, Resource, ResourceId, ResourceKind, SensitiveStore};
    use bosun_handles::{ActionAck, DaemonInfo, RunrHandle, ServiceStatus, UnitListItem};
    use bosun_runr_client::TimerStatus;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::runr_timer::spec::TimerState;

    /// Минимальный mock с подсчётом вызовов `timer_statuses` и записью
    /// timer_enable/disable/stop, нужный для cache-теста.
    struct MockRunr {
        calls: Mutex<Vec<String>>,
        timer_statuses_count: AtomicU32,
        statuses: Vec<TimerStatus>,
    }

    impl MockRunr {
        fn new(statuses: Vec<TimerStatus>) -> Self {
            Self {
                calls: Mutex::new(vec![]),
                timer_statuses_count: AtomicU32::new(0),
                statuses,
            }
        }
        fn record(&self, label: &str) {
            self.calls.lock().unwrap().push(label.to_string());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RunrHandle for MockRunr {
        fn base_url(&self) -> &str {
            "http://mock"
        }
        fn daemon_info(&self) -> Result<DaemonInfo, RunrError> {
            unimplemented!()
        }
        fn daemon_reload(&self) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn service_start(&self, _: &str, _: bool) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn service_stop(&self, _: &str, _: bool, _: Option<&str>) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn service_restart(&self, _: &str) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn service_reload(&self, _: &str) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn timer_start(&self, name: &str) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_start:{name}"));
            Ok(ack())
        }
        fn timer_stop(&self, name: &str) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_stop:{name}"));
            Ok(ack())
        }
        fn timer_enable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_enable:{name}:{now}"));
            Ok(ack())
        }
        fn timer_disable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_disable:{name}:{now}"));
            Ok(ack())
        }
        fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError> {
            unimplemented!()
        }
        fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError> {
            self.timer_statuses_count.fetch_add(1, Ordering::AcqRel);
            self.record("timer_statuses");
            Ok(self.statuses.clone())
        }
        fn units_list(&self) -> Result<Vec<UnitListItem>, RunrError> {
            unimplemented!()
        }
        fn verify_restart(
            &self,
            _: &str,
            _: &ServiceStatus,
            _: Duration,
            _: Duration,
        ) -> Result<ServiceStatus, RunrError> {
            unimplemented!()
        }
        fn verify_start(
            &self,
            _: &str,
            _: Duration,
            _: Duration,
        ) -> Result<ServiceStatus, RunrError> {
            unimplemented!()
        }
    }

    fn ack() -> ActionAck {
        ActionAck {
            action_id: "1".into(),
            accepted_at: "2026-05-19T00:00:00Z".into(),
            message: None,
        }
    }

    fn status(name: &str, enabled: Option<bool>) -> TimerStatus {
        TimerStatus {
            name: name.to_string(),
            state: "Stopped".to_string(),
            next_run: None,
            target_service: format!("{name}_target"),
            enabled,
        }
    }

    fn make_resource(name: &str, state: TimerState) -> Resource {
        let kind = ResourceKind::from_static("runr.timer");
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
                "start_now": false,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn make_ctx(runr: Option<Arc<dyn RunrHandle>>) -> (TempDir, ApplyCtx) {
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
            runr,
            None,
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
    fn apply_calls_timer_statuses_once_per_resource() {
        // Кэш per-apply снят: каждый ресурс делает свой свежий
        // timer_statuses, чтобы видеть изменения соседних примитивов в
        // том же прогоне. Фиксируем именно N запросов на N ресурсов.
        let mock = Arc::new(MockRunr::new(vec![
            status("a.timer", Some(false)),
            status("b.timer", Some(false)),
        ]));
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        for name in ["a.timer", "b.timer"] {
            let r = make_resource(name, TimerState::Enabled);
            let _ = run(&r, &force_update(&r), &ctx).unwrap();
        }
        assert_eq!(
            mock.timer_statuses_count.load(Ordering::Acquire),
            2,
            "timer_statuses ожидается ровно по одному на ресурс, calls: {:?}",
            mock.calls()
        );
    }

    #[test]
    fn apply_no_change_does_not_invoke_mutation() {
        // Таймер уже enabled → action=NoChange → ни enable, ни start.
        let mock = Arc::new(MockRunr::new(vec![status("x.timer", Some(true))]));
        let r = make_resource("x.timer", TimerState::Enabled);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(!report.changed, "NoChange должен возвращать changed=false");
        let calls = mock.calls();
        // Только timer_statuses должен быть в логе.
        assert!(
            calls.iter().any(|c| c == "timer_statuses"),
            "ожидался хотя бы один timer_statuses, got {calls:?}"
        );
        for forbidden in &["timer_enable", "timer_start", "timer_stop", "timer_disable"] {
            assert!(
                !calls.iter().any(|c| c.starts_with(forbidden)),
                "{forbidden} не должен вызываться на NoChange path, got {calls:?}"
            );
        }
    }

    // -- маппинг ошибок ------------------------------------------------------
    //
    // `map_runr_error` дублирует логику runr_service::apply::map_runr_error,
    // но имеет свою копию (private). Поэтому тесты повторяем — регрессия в
    // одной копии не отлавливается тестом другой.

    #[test]
    fn map_runr_error_unavailable_is_runr_unavailable_deferrable() {
        let err = RunrError::Unavailable {
            base_url: "http://mock".into(),
            source: Box::new(std::io::Error::other("refused")),
        };
        let mapped = map_runr_error(err, "http://mock", "timer_enable");
        assert!(mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::RunrUnavailable { .. }));
    }

    #[test]
    fn map_runr_error_not_found_is_apply() {
        let err = RunrError::NotFound {
            kind: "timer".into(),
            name: "nope.timer".into(),
        };
        let mapped = map_runr_error(err, "http://mock", "timer_disable");
        assert!(!mapped.is_deferrable());
        match mapped {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("nope.timer"), "reason: {reason}");
                assert!(reason.contains("timer_disable"), "op: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn map_runr_error_api_error_is_apply_with_status_and_body() {
        let err = RunrError::ApiError {
            status: 502,
            body: "gateway error".into(),
        };
        let mapped = map_runr_error(err, "http://mock", "timer_enable");
        assert!(!mapped.is_deferrable());
        match mapped {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("502"), "status: {reason}");
                assert!(reason.contains("gateway error"), "body: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn map_runr_error_bad_response_is_apply() {
        let err = RunrError::BadResponse("schema mismatch".into());
        let mapped = map_runr_error(err, "http://mock", "timer_statuses");
        assert!(!mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::Apply { .. }));
    }

    #[test]
    fn map_runr_error_restart_not_observed_is_apply() {
        // У таймера эта ошибка маловероятна (verify_restart не вызывается),
        // но через map проходит и должна попасть в Apply, не deferrable.
        let err = RunrError::RestartNotObserved {
            unit: "x.timer".into(),
        };
        let mapped = map_runr_error(err, "http://mock", "op");
        assert!(!mapped.is_deferrable());
        match mapped {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("x.timer"), "reason: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn map_runr_error_io_is_runr_unavailable_deferrable() {
        let err = RunrError::Io(std::io::Error::other("EOF"));
        let mapped = map_runr_error(err, "http://mock", "timer_stop");
        assert!(mapped.is_deferrable());
        match mapped {
            PrimitiveError::RunrUnavailable { base_url, .. } => {
                assert_eq!(base_url, "http://mock");
            }
            other => panic!("expected RunrUnavailable, got {other:?}"),
        }
    }

    // -- apply paths: Enable / Disable / StopAndDisable -----------------------

    /// Спец для теста с настраиваемым start_now (фикстура make_resource
    /// зафиксирован на false).
    fn resource_with_start_now(name: &str, state: TimerState, start_now: bool) -> Resource {
        let kind = ResourceKind::from_static("runr.timer");
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
                "start_now": start_now,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn apply_enable_calls_timer_enable_with_start_now_true_flag() {
        // Таймер выключен, spec.state=Enabled, start_now=true → ожидаем один
        // вызов timer_enable:x.timer:true и ни одного start/stop/disable.
        let mock = Arc::new(MockRunr::new(vec![status("x.timer", Some(false))]));
        let r = resource_with_start_now("x.timer", TimerState::Enabled, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        assert!(
            calls.iter().any(|c| c == "timer_enable:x.timer:true"),
            "ожидался timer_enable:x.timer:true, got {calls:?}"
        );
        for forbidden in &["timer_start", "timer_stop", "timer_disable"] {
            assert!(
                !calls.iter().any(|c| c.starts_with(forbidden)),
                "{forbidden} не должен вызываться на Enable path, got {calls:?}"
            );
        }
    }

    #[test]
    fn apply_enable_calls_timer_enable_with_start_now_false_flag() {
        let mock = Arc::new(MockRunr::new(vec![status("x.timer", Some(false))]));
        let r = resource_with_start_now("x.timer", TimerState::Enabled, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        assert!(
            calls.iter().any(|c| c == "timer_enable:x.timer:false"),
            "ожидался timer_enable:x.timer:false, got {calls:?}"
        );
    }

    #[test]
    fn apply_disable_calls_timer_disable_no_now_flag() {
        // Таймер включён, spec=Disabled → timer_disable, без stop.
        let mock = Arc::new(MockRunr::new(vec![status("x.timer", Some(true))]));
        let r = make_resource("x.timer", TimerState::Disabled);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        // timer_disable:name:now — для Disable передаём now=false.
        assert!(
            calls.iter().any(|c| c == "timer_disable:x.timer:false"),
            "ожидался timer_disable:x.timer:false, got {calls:?}"
        );
        // timer_stop не должен вызываться (для Disable только disable).
        assert!(
            !calls.iter().any(|c| c.starts_with("timer_stop")),
            "timer_stop не должен вызываться на Disable path, got {calls:?}"
        );
        assert!(!calls.iter().any(|c| c.starts_with("timer_enable")));
    }

    #[test]
    fn apply_stop_and_disable_calls_stop_then_disable_in_that_order() {
        // Таймер включён, spec=Absent → stop первым, disable вторым.
        // Порядок принципиален: между stop и disable не должен отлететь тик.
        let mock = Arc::new(MockRunr::new(vec![status("x.timer", Some(true))]));
        let r = make_resource("x.timer", TimerState::Absent);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        let stop_idx = calls
            .iter()
            .position(|c| c == "timer_stop:x.timer")
            .unwrap_or_else(|| panic!("ожидался timer_stop:x.timer, got {calls:?}"));
        let disable_idx = calls
            .iter()
            .position(|c| c == "timer_disable:x.timer:true")
            .unwrap_or_else(|| panic!("ожидался timer_disable:x.timer:true, got {calls:?}"));
        assert!(
            stop_idx < disable_idx,
            "stop должен идти ДО disable, calls={calls:?}"
        );
    }
}
