//! Apply-фаза `runr.timer`.
//!
//! Логика проще, чем для service: таймеры не требуют notify-семантики,
//! enable/disable — desired-state операции. Все вызовы синхронные.
//!
//! Snapshot всех таймеров берётся один раз на apply и кэшируется в
//! `ApplyCtx.runr_timer_statuses` (OnceLock), симметрично
//! `runr_service_statuses` — на 10 таймерах в одном bundle экономит 9
//! HTTP round-trip'ов.

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};
use bosun_runr_client::{RunrError, TimerStatus};

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

    let timers = get_or_fetch_timer_statuses(runr.as_ref(), &ctx.runr_timer_statuses)?;
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

/// Получить snapshot timer_statuses один раз на apply. Кэшируется в
/// `OnceLock` на ApplyCtx. Симметрично `runr_service::get_or_fetch_statuses`.
/// Ошибки transport → RunrUnavailable, остальные → Apply.
fn get_or_fetch_timer_statuses<R: bosun_handles::RunrHandle + ?Sized>(
    runr: &R,
    cache: &std::sync::OnceLock<Vec<TimerStatus>>,
) -> Result<Vec<TimerStatus>, PrimitiveError> {
    if let Some(cached) = cache.get() {
        return Ok(cached.clone());
    }
    let fresh = runr
        .timer_statuses()
        .map_err(|e| map_runr_error(e, runr.base_url(), "timer_statuses"))?;
    // get_or_init может проиграть гонку (два параллельных apply), но
    // snapshot'ы эквивалентны для текущей цели — берём что есть в кэше.
    let stored = cache.get_or_init(|| fresh.clone());
    Ok(stored.clone())
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
    fn apply_caches_timer_statuses_across_resources() {
        // Два таймера в одном ctx → timer_statuses должен быть вызван
        // ровно один раз (OnceLock-кэш).
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
            1,
            "timer_statuses должен быть вызван 1 раз, calls: {:?}",
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
}
