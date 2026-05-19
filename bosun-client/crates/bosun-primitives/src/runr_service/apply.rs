//! Apply-фаза `runr.service`.
//!
//! Логика:
//! 1. `ctx.runr is None` → `RunrUnavailable` (deferrable). Без клиента
//!    делать нечего.
//! 2. Throttle `daemon_reload`: первый ресурс на apply вызывает
//!    `runr.daemon_reload()`, остальные пропускают (флаг в
//!    `ApplyCtx.runr_daemon_reload_done`).
//! 3. Сделать snapshot всех сервисов один раз: вызов `service_statuses`
//!    кэшируется в `ApplyCtx.runr_service_statuses` (OnceLock).
//! 4. Прогнать `decide_action_runr(spec, snapshot, notify-флаги)`.
//! 5. На `Action::Restart` / `Action::Reload` — enqueue defer ДО реального
//!    вызова runr (at-least-once: при крэше после enqueue replay подберёт).
//! 6. На `Action::Start` / `Action::Stop` — синхронно через runr-клиент.
//!    Для Start после ответа runr ждём `verify_start` (state=Running),
//!    отдельный helper от `verify_restart`: у нового процесса счётчик
//!    `restarts` остаётся 0 и не подходит как критерий успеха.
//!    Для Stop verify не нужен — runr возвращает 200 после успешной
//!    остановки.
//! 7. Маппинг ошибок: `RunrError::Unavailable` → `RunrUnavailable`
//!    (deferrable), остальные → `Apply { reason }` (non-deferrable).

use std::sync::atomic::Ordering;
use std::time::Duration;

use bosun_core::defers::{make_id, DeferAction, DeferEntry, DeferPriority, CURRENT_SPEC_VERSION};
use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};
use bosun_runr_client::{RunrError, ServiceStatus};

use super::plan::{decide_action_runr, Action};
use super::spec::RunrServiceSpec;

/// Поллинг-интервал для `verify_start` после синхронного Start.
const VERIFY_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Бюджет общего ожидания, после которого verify считается провалившимся.
const VERIFY_POLL_TOTAL: Duration = Duration::from_secs(15);
/// Максимум попыток в защёлкивающем defer'е до промоушена в `.manual_clear`.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Тег init-системы для defer-id и логов.
const INIT_SYSTEM_RUNR: &str = "runr";

/// Главная entry-point apply'я. Десериализует payload, решает action,
/// выбирает sync- или defer-путь.
pub fn run(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: RunrServiceSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service payload: {e}")))?;

    let Some(runr) = ctx.runr.as_ref() else {
        // Клиент не инициализирован — трактуем как unavailable, чтобы
        // оркестратор положил resource в Deferred и попытался снова на
        // следующем цикле, когда CLI поднимет клиент.
        return Err(PrimitiveError::RunrUnavailable {
            base_url: "n/a".to_string(),
            reason: "runr client not initialized in ApplyCtx".to_string(),
        });
    };

    // 1. Throttle daemon_reload.
    if !ctx.runr_daemon_reload_done.swap(true, Ordering::AcqRel) {
        tracing::debug!(unit = %spec.name, "calling runr.daemon_reload (first resource in apply)");
        match runr.daemon_reload() {
            Ok(_) => {}
            Err(RunrError::Unavailable { base_url, source }) => {
                // Откатываем флаг: следующий ресурс попробует снова.
                ctx.runr_daemon_reload_done.store(false, Ordering::Release);
                return Err(PrimitiveError::RunrUnavailable {
                    base_url,
                    reason: format!("daemon_reload: {source}"),
                });
            }
            Err(other) => {
                return Err(PrimitiveError::Apply {
                    reason: format!("runr.daemon_reload failed: {other}"),
                });
            }
        }
    }

    // 2. Snapshot service statuses (один HTTP-call на весь apply).
    let statuses = get_or_fetch_statuses(runr.as_ref(), &ctx.runr_service_statuses)?;
    let current = statuses.iter().find(|s| s.name == spec.name);

    // 3. Тригеры notify из `restart_on` / `reload_on`.
    let restart_triggered = resource.restart_on.iter().any(|id| ctx.is_changed(id));
    let reload_triggered = resource.reload_on.iter().any(|id| ctx.is_changed(id));
    let action = decide_action_runr(&spec, current, restart_triggered, reload_triggered);

    let sources = collect_notify_sources(resource, ctx, restart_triggered, reload_triggered);

    match action {
        Action::NoChange => Ok(ChangeReport::no_change()),
        Action::Start => execute_start(runr.as_ref(), &spec, current),
        Action::Stop => execute_stop(runr.as_ref(), &spec),
        Action::Restart => enqueue_defer(
            ctx,
            &spec,
            DeferAction::Restart,
            DeferPriority::Restart,
            sources,
        ),
        Action::Reload => enqueue_defer(
            ctx,
            &spec,
            DeferAction::Reload,
            DeferPriority::Reload,
            sources,
        ),
    }
}

/// Получить snapshot service_statuses один раз на apply. Кэшируется в
/// `OnceLock`. Ошибка transport → RunrUnavailable, остальные → Apply.
fn get_or_fetch_statuses<R: bosun_handles::RunrHandle + ?Sized>(
    runr: &R,
    cache: &std::sync::OnceLock<Vec<ServiceStatus>>,
) -> Result<Vec<ServiceStatus>, PrimitiveError> {
    if let Some(cached) = cache.get() {
        return Ok(cached.clone());
    }
    let fresh = runr
        .service_statuses()
        .map_err(|e| map_runr_error(e, runr.base_url(), "service_statuses"))?;
    // get_or_init может проиграть гонку (если кто-то параллельно тоже
    // дёргал runr), но это безопасно: значения от двух одинаковых snapshot'ов
    // эквивалентны для нашей цели.
    let stored = cache.get_or_init(|| fresh.clone());
    Ok(stored.clone())
}

/// Синхронно запустить unit и убедиться, что он добрался до `Running`.
///
/// Раньше здесь стоял `verify_restart`, опирающийся на инкремент счётчика
/// `restarts`. Для start-с-нуля счётчик у нового процесса равен 0 и не
/// двигается, поэтому таймаут «restart не наблюдался» был ложным сигналом —
/// и, что хуже, мы трактовали его как «success (idempotent)», маскируя
/// сценарий «сервис упал в Failed сразу после start». Теперь критерий
/// прямой: `state == "Running"` ⇒ Ok, `state == "Failed"` ⇒ Err
/// (`ServiceStartFailed`), таймаут ⇒ Err (`StartNotObserved`).
///
/// Параметр `before` не используется здесь, остаётся для симметрии
/// с restart-веткой (если когда-то понадобится сравнивать pid'ы).
fn execute_start<R: bosun_handles::RunrHandle + ?Sized>(
    runr: &R,
    spec: &RunrServiceSpec,
    _before: Option<&ServiceStatus>,
) -> Result<ChangeReport, PrimitiveError> {
    tracing::info!(unit = %spec.name, "runr.service: start");
    runr.service_start(&spec.name, true)
        .map_err(|e| map_runr_error(e, runr.base_url(), "service_start"))?;
    match runr.verify_start(&spec.name, VERIFY_POLL_INTERVAL, VERIFY_POLL_TOTAL) {
        Ok(_status) => Ok(ChangeReport::changed(format!(
            "started runr.service:{}",
            spec.name
        ))),
        Err(e) => Err(map_runr_error(e, runr.base_url(), "verify_start")),
    }
}

/// Синхронно остановить unit. Для Stop verify не нужен — runr возвращает
/// 200 после прихода SIGTERM/завершения процесса. Если демон обещал
/// graceful, мы доверяем ему.
fn execute_stop<R: bosun_handles::RunrHandle + ?Sized>(
    runr: &R,
    spec: &RunrServiceSpec,
) -> Result<ChangeReport, PrimitiveError> {
    tracing::info!(unit = %spec.name, "runr.service: stop");
    runr.service_stop(&spec.name, false, None)
        .map_err(|e| map_runr_error(e, runr.base_url(), "service_stop"))?;
    Ok(ChangeReport::changed(format!(
        "stopped runr.service:{}",
        spec.name
    )))
}

/// Положить запись в журнал defers. Это критический инвариант Phase D:
/// enqueue идёт ДО реального вызова runr.{restart,reload} — replay подхватит
/// её, даже если bosun упадёт между enqueue и реальным вызовом. Поэтому
/// функция не делает HTTP-вызов: реальный restart/reload произведёт
/// replay-цикл (immediate в этом же apply'е в `cli::apply` post-replay, либо
/// следующий цикл).
fn enqueue_defer(
    ctx: &ApplyCtx,
    spec: &RunrServiceSpec,
    defer_action: DeferAction,
    priority: DeferPriority,
    sources: Vec<String>,
) -> Result<ChangeReport, PrimitiveError> {
    let id = make_id(INIT_SYSTEM_RUNR, &defer_action, &spec.name);
    let entry = DeferEntry {
        spec_version: CURRENT_SPEC_VERSION,
        id: id.clone(),
        action: defer_action.clone(),
        init_system: INIT_SYSTEM_RUNR.to_string(),
        target: spec.name.clone(),
        validate_cmd: spec.validate_with.clone(),
        health_check: spec.health_check.clone(),
        priority,
        enqueued_at: chrono::Utc::now(),
        enqueued_by: sources,
        attempt_count: 0,
        max_attempts: DEFAULT_MAX_ATTEMPTS,
    };
    let action_slug = defer_action.filename_slug();
    tracing::info!(
        unit = %spec.name,
        defer_id = %id,
        action = action_slug,
        "runr.service: enqueueing defer",
    );
    ctx.defers
        .enqueue(entry)
        .map_err(|e| PrimitiveError::DeferIo {
            path: ctx.defers.root().to_path_buf(),
            reason: format!("{e}"),
        })?;
    Ok(ChangeReport::deferred(format!(
        "deferred {} of runr.service:{}",
        action_slug, spec.name
    )))
}

/// Собрать список source-id для поля `enqueued_by`. Это просто id'шники
/// тех ресурсов, которые изменились в текущем apply и связаны
/// restart_on/reload_on с целевым unit'ом.
fn collect_notify_sources(
    resource: &Resource,
    ctx: &ApplyCtx,
    restart_triggered: bool,
    reload_triggered: bool,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if restart_triggered {
        for src in &resource.restart_on {
            if ctx.is_changed(src) {
                out.push(src.to_string());
            }
        }
    }
    if reload_triggered {
        for src in &resource.reload_on {
            if ctx.is_changed(src) {
                out.push(src.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Маппинг `RunrError` → `PrimitiveError`. Transport-ошибки идут в
/// deferrable RunrUnavailable; остальные — в Apply.
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
        RunrError::StartNotObserved { unit, last_state } => PrimitiveError::Apply {
            reason: format!(
                "runr start of {unit} did not reach Running (last={last_state}, op={op})"
            ),
        },
        RunrError::ServiceStartFailed { unit } => PrimitiveError::Apply {
            reason: format!("runr {unit} entered Failed after start (op={op})"),
        },
        RunrError::Io(e) => PrimitiveError::RunrUnavailable {
            base_url: base_url.to_string(),
            reason: format!("{op}: i/o error: {e}"),
        },
        // non_exhaustive: новые варианты пробрасываем как Apply с текстом.
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
    use std::time::Instant;

    use bosun_core::defers::Journal;
    use bosun_core::{
        ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource, ResourceId, ResourceKind,
        SensitiveStore,
    };
    use bosun_handles::RunrHandle;
    use bosun_runr_client::{ActionAck, DaemonInfo, ServiceStatus, TimerStatus, UnitListItem};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::runr_service::spec::ServiceState;

    /// Mock-handle, который ведёт лог вызовов и возвращает заданные
    /// snapshot'ы. Используется только в тестах — production-handle
    /// (`bosun_runr_client::Client`) подключается через blanket impl.
    struct MockRunr {
        statuses: Vec<ServiceStatus>,
        calls: Mutex<Vec<String>>,
        daemon_reload_count: AtomicU32,
        // Что вернуть на service_start — для error-инжекшна.
        start_error: Mutex<Option<RunrError>>,
        // Snapshot, который вернёт verify_restart (поднимет restarts на 1).
        verify_after_restarts: AtomicU32,
        // Что вернуть на verify_start: None → Ok(state=Running), Some(err) →
        // тестируем сценарии Failed/Timeout.
        verify_start_error: Mutex<Option<RunrError>>,
    }

    impl MockRunr {
        fn new(statuses: Vec<ServiceStatus>) -> Self {
            Self {
                statuses,
                calls: Mutex::new(vec![]),
                daemon_reload_count: AtomicU32::new(0),
                start_error: Mutex::new(None),
                verify_after_restarts: AtomicU32::new(1),
                verify_start_error: Mutex::new(None),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, label: &str) {
            self.calls.lock().unwrap().push(label.to_string());
        }
    }

    impl RunrHandle for MockRunr {
        fn base_url(&self) -> &str {
            "http://mock"
        }
        fn daemon_info(&self) -> Result<DaemonInfo, RunrError> {
            unimplemented!("not used in tests")
        }
        fn daemon_reload(&self) -> Result<ActionAck, RunrError> {
            self.daemon_reload_count.fetch_add(1, Ordering::AcqRel);
            self.record("daemon_reload");
            Ok(ActionAck {
                action_id: "1".into(),
                accepted_at: "2026-05-19T00:00:00Z".into(),
                message: None,
            })
        }
        fn service_start(&self, name: &str, _idem: bool) -> Result<ActionAck, RunrError> {
            self.record(&format!("service_start:{name}"));
            if let Some(err) = self.start_error.lock().unwrap().take() {
                return Err(err);
            }
            Ok(ActionAck {
                action_id: "2".into(),
                accepted_at: "2026-05-19T00:00:00Z".into(),
                message: None,
            })
        }
        fn service_stop(
            &self,
            name: &str,
            _force: bool,
            _timeout: Option<&str>,
        ) -> Result<ActionAck, RunrError> {
            self.record(&format!("service_stop:{name}"));
            Ok(ActionAck {
                action_id: "3".into(),
                accepted_at: "2026-05-19T00:00:00Z".into(),
                message: None,
            })
        }
        fn service_restart(&self, name: &str) -> Result<ActionAck, RunrError> {
            // Этот метод не должен вызываться в Phase D apply'е!
            self.record(&format!("service_restart:{name}"));
            Ok(ActionAck {
                action_id: "4".into(),
                accepted_at: "2026-05-19T00:00:00Z".into(),
                message: None,
            })
        }
        fn service_reload(&self, name: &str) -> Result<ActionAck, RunrError> {
            // Этот метод тоже не должен вызываться напрямую в Phase D.
            self.record(&format!("service_reload:{name}"));
            Ok(ActionAck {
                action_id: "5".into(),
                accepted_at: "2026-05-19T00:00:00Z".into(),
                message: None,
            })
        }
        fn timer_start(&self, name: &str) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_start:{name}"));
            Ok(ActionAck {
                action_id: "6".into(),
                accepted_at: "x".into(),
                message: None,
            })
        }
        fn timer_stop(&self, name: &str) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_stop:{name}"));
            Ok(ActionAck {
                action_id: "7".into(),
                accepted_at: "x".into(),
                message: None,
            })
        }
        fn timer_enable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_enable:{name}:{now}"));
            Ok(ActionAck {
                action_id: "8".into(),
                accepted_at: "x".into(),
                message: None,
            })
        }
        fn timer_disable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
            self.record(&format!("timer_disable:{name}:{now}"));
            Ok(ActionAck {
                action_id: "9".into(),
                accepted_at: "x".into(),
                message: None,
            })
        }
        fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError> {
            self.record("service_statuses");
            Ok(self.statuses.clone())
        }
        fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError> {
            self.record("timer_statuses");
            Ok(vec![])
        }
        fn units_list(&self) -> Result<Vec<UnitListItem>, RunrError> {
            self.record("units_list");
            Ok(vec![])
        }
        fn verify_restart(
            &self,
            name: &str,
            before: &ServiceStatus,
            _poll_interval: Duration,
            _poll_total: Duration,
        ) -> Result<ServiceStatus, RunrError> {
            self.record(&format!("verify_restart:{name}"));
            let new_restarts =
                before.restarts + self.verify_after_restarts.load(Ordering::Acquire) as u64;
            Ok(ServiceStatus {
                name: name.to_string(),
                state: "Running".to_string(),
                pid: Some(42),
                restarts: new_restarts,
                in_state_for_ms: 100,
                uptime_ms: Some(100),
                downtime_ms: None,
                next_restart_in_ms: None,
                started_at: Some("2026-05-19T00:00:00Z".to_string()),
                autostart: false,
                memory_rss_anon_bytes: 0,
                memory_rss_file_bytes: 0,
                cpu_usage_percent: 0.0,
            })
        }
        fn verify_start(
            &self,
            name: &str,
            _poll_interval: Duration,
            _poll_total: Duration,
        ) -> Result<ServiceStatus, RunrError> {
            self.record(&format!("verify_start:{name}"));
            if let Some(err) = self.verify_start_error.lock().unwrap().take() {
                return Err(err);
            }
            Ok(ServiceStatus {
                name: name.to_string(),
                state: "Running".to_string(),
                pid: Some(42),
                restarts: 0,
                in_state_for_ms: 100,
                uptime_ms: Some(100),
                downtime_ms: None,
                next_restart_in_ms: None,
                started_at: Some("2026-05-19T00:00:00Z".to_string()),
                autostart: false,
                memory_rss_anon_bytes: 0,
                memory_rss_file_bytes: 0,
                cpu_usage_percent: 0.0,
            })
        }
    }

    fn status(name: &str, state: &str, restarts: u64) -> ServiceStatus {
        ServiceStatus {
            name: name.to_string(),
            state: state.to_string(),
            pid: None,
            restarts,
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

    fn make_resource(name: &str, state: ServiceState) -> Resource {
        let kind = ResourceKind::from_static("runr.service");
        let id = ResourceId::new(&kind, name);
        let state_str = match state {
            ServiceState::Running => "running",
            ServiceState::Stopped => "stopped",
            ServiceState::Absent => "absent",
        };
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "name": name,
                "state": state_str,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn make_ctx_with_runr(runr: Option<Arc<dyn RunrHandle>>) -> (TempDir, ApplyCtx) {
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

    fn force_update_diff(r: &Resource) -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: r.payload.clone(),
            description: "converge".into(),
        }
    }

    #[test]
    fn apply_returns_no_change_for_diff_no_change() {
        let mock = Arc::new(MockRunr::new(vec![]));
        let r = make_resource("svc", ServiceState::Running);
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let report = run(&r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn apply_returns_runr_unavailable_when_ctx_runr_none() {
        let r = make_resource("svc", ServiceState::Running);
        let (_tmp, ctx) = make_ctx_with_runr(None);
        let err = run(&r, &force_update_diff(&r), &ctx).unwrap_err();
        match err {
            PrimitiveError::RunrUnavailable { base_url, reason } => {
                assert_eq!(base_url, "n/a");
                assert!(reason.contains("not initialized"), "got: {reason}");
            }
            other => panic!("expected RunrUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn apply_calls_daemon_reload_exactly_once_per_apply() {
        // Прогоняем три ресурса через тот же ctx — daemon_reload должен
        // быть вызван ровно один раз.
        let mock = Arc::new(MockRunr::new(vec![]));
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        for name in ["a", "b", "c"] {
            let r = make_resource(name, ServiceState::Running);
            let _ = run(&r, &force_update_diff(&r), &ctx);
        }
        assert_eq!(
            mock.daemon_reload_count.load(Ordering::Acquire),
            1,
            "daemon_reload должен быть вызван один раз, вызовы: {:?}",
            mock.calls()
        );
    }

    #[test]
    fn apply_running_with_unknown_status_calls_start() {
        // Snapshot пустой → spec=Running → Start.
        let mock = Arc::new(MockRunr::new(vec![]));
        let r = make_resource("svc", ServiceState::Running);
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let report = run(&r, &force_update_diff(&r), &ctx).unwrap();
        assert!(report.changed);
        let calls = mock.calls();
        // daemon_reload и service_statuses и service_start идут в этом порядке.
        assert!(calls.iter().any(|c| c == "daemon_reload"));
        assert!(calls.iter().any(|c| c == "service_start:svc"));
        // verify_start должен быть вызван — для старта смотрим state=Running,
        // не инкремент restarts (он у новой инстанции равен 0).
        assert!(calls.iter().any(|c| c == "verify_start:svc"));
        // verify_restart НЕ должен быть вызван для start-сценария.
        assert!(!calls.iter().any(|c| c == "verify_restart:svc"));
        // service_restart НЕ должен быть вызван напрямую.
        assert!(!calls.iter().any(|c| c == "service_restart:svc"));
    }

    #[test]
    fn apply_start_fails_when_service_enters_failed_state() {
        // Регрессия H4: раньше execute_start использовал verify_restart,
        // и failed-сервис мог отчитаться success (idempotent). Теперь
        // verify_start возвращает ServiceStartFailed, что мапится в
        // PrimitiveError::Apply.
        let mock = Arc::new(MockRunr::new(vec![]));
        *mock.verify_start_error.lock().unwrap() =
            Some(RunrError::ServiceStartFailed { unit: "svc".into() });
        let r = make_resource("svc", ServiceState::Running);
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let err = run(&r, &force_update_diff(&r), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(
                    reason.contains("Failed"),
                    "expected 'Failed' in reason, got: {reason}"
                );
            }
            other => panic!("expected Apply, got {other:?}"),
        }
        assert!(mock.calls().iter().any(|c| c == "service_start:svc"));
        assert!(mock.calls().iter().any(|c| c == "verify_start:svc"));
    }

    #[test]
    fn apply_start_fails_on_verify_start_timeout() {
        // StartNotObserved → Apply error, не молчаливый success.
        let mock = Arc::new(MockRunr::new(vec![]));
        *mock.verify_start_error.lock().unwrap() = Some(RunrError::StartNotObserved {
            unit: "svc".into(),
            last_state: "Starting".into(),
        });
        let r = make_resource("svc", ServiceState::Running);
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let err = run(&r, &force_update_diff(&r), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(
                    reason.contains("not reach Running") || reason.contains("Starting"),
                    "got reason: {reason}"
                );
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn apply_stopped_running_calls_stop() {
        let mock = Arc::new(MockRunr::new(vec![status("svc", "Running", 0)]));
        let r = make_resource("svc", ServiceState::Stopped);
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let report = run(&r, &force_update_diff(&r), &ctx).unwrap();
        assert!(report.changed);
        assert!(mock.calls().iter().any(|c| c == "service_stop:svc"));
    }

    #[test]
    fn apply_running_running_no_triggers_is_no_change() {
        let mock = Arc::new(MockRunr::new(vec![status("svc", "Running", 0)]));
        let r = make_resource("svc", ServiceState::Running);
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let report = run(&r, &force_update_diff(&r), &ctx).unwrap();
        assert!(!report.changed);
        // НИ start, НИ stop, НИ restart не должны быть вызваны.
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| c.starts_with("service_start")));
        assert!(!calls.iter().any(|c| c.starts_with("service_stop")));
        assert!(!calls.iter().any(|c| c.starts_with("service_restart")));
    }

    #[test]
    fn apply_restart_trigger_enqueues_defer_not_calls_runr_restart() {
        let mock = Arc::new(MockRunr::new(vec![status("svc", "Running", 5)]));
        let r = {
            let mut r = make_resource("svc", ServiceState::Running);
            // Триггер: source-resource в restart_on был изменён.
            let src_kind = ResourceKind::from_static("file.content");
            let src_id = ResourceId::new(&src_kind, "/etc/cfg");
            r.restart_on.push(src_id);
            r
        };
        let (tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        // Помечаем источник изменённым.
        ctx.record_changed(&r.restart_on[0]);

        let report = run(&r, &force_update_diff(&r), &ctx).unwrap();
        assert!(matches!(report, ChangeReport { deferred: true, .. }));
        // service_restart НЕ должен быть вызван — это самый важный инвариант.
        assert!(
            !mock.calls().iter().any(|c| c == "service_restart:svc"),
            "service_restart должен быть отложен, не выполнен синхронно"
        );

        // Файл defer должен лежать в журнале с правильным префиксом.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".deferred"))
            .collect();
        assert_eq!(entries.len(), 1, "should be one deferred file: {entries:?}");
        assert!(
            entries[0].contains("runr.restart:svc"),
            "expected runr.restart:svc in filename, got {entries:?}"
        );
        // Префикс 0r- — приоритет Restart.
        assert!(entries[0].starts_with("0r-"));
    }

    #[test]
    fn apply_reload_trigger_enqueues_reload_defer() {
        let mock = Arc::new(MockRunr::new(vec![status("svc", "Running", 0)]));
        let r = {
            let mut r = make_resource("svc", ServiceState::Running);
            let src_kind = ResourceKind::from_static("file.content");
            let src_id = ResourceId::new(&src_kind, "/etc/cfg");
            r.reload_on.push(src_id);
            r
        };
        let (tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        ctx.record_changed(&r.reload_on[0]);

        let report = run(&r, &force_update_diff(&r), &ctx).unwrap();
        assert!(report.deferred);
        // service_reload не должен быть вызван напрямую.
        assert!(!mock.calls().iter().any(|c| c == "service_reload:svc"));
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".deferred"))
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].contains("runr.reload:svc"),
            "expected runr.reload:svc, got {entries:?}"
        );
        // Префикс 2r- — приоритет Reload.
        assert!(entries[0].starts_with("2r-"));
    }

    #[test]
    fn apply_unavailable_runr_returns_deferrable_error() {
        // Smoke-test через service_statuses: мокаем Unavailable.
        struct UnavailableRunr;
        impl RunrHandle for UnavailableRunr {
            fn base_url(&self) -> &str {
                "http://127.0.0.1:8010"
            }
            fn daemon_info(&self) -> Result<DaemonInfo, RunrError> {
                unimplemented!()
            }
            fn daemon_reload(&self) -> Result<ActionAck, RunrError> {
                Err(RunrError::Unavailable {
                    base_url: "http://127.0.0.1:8010".to_string(),
                    source: Box::new(std::io::Error::other("refused")),
                })
            }
            fn service_start(&self, _: &str, _: bool) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn service_stop(
                &self,
                _: &str,
                _: bool,
                _: Option<&str>,
            ) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn service_restart(&self, _: &str) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn service_reload(&self, _: &str) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn timer_start(&self, _: &str) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn timer_stop(&self, _: &str) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn timer_enable(&self, _: &str, _: bool) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn timer_disable(&self, _: &str, _: bool) -> Result<ActionAck, RunrError> {
                unimplemented!()
            }
            fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError> {
                unimplemented!()
            }
            fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError> {
                unimplemented!()
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
        let r = make_resource("svc", ServiceState::Running);
        let (_tmp, ctx) =
            make_ctx_with_runr(Some(Arc::new(UnavailableRunr) as Arc<dyn RunrHandle>));
        let err = run(&r, &force_update_diff(&r), &ctx).unwrap_err();
        assert!(
            err.is_deferrable(),
            "ожидался deferrable error, got {err:?}"
        );
        match err {
            PrimitiveError::RunrUnavailable { .. } => {}
            other => panic!("expected RunrUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn apply_service_statuses_cached_once_per_apply() {
        // Несколько runr.service ресурсов в одном apply должны брать snapshot
        // ровно один раз через ctx.runr_service_statuses (OnceLock).
        let mock = Arc::new(MockRunr::new(vec![
            status("a", "Running", 0),
            status("b", "Running", 0),
        ]));
        let (_tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let r1 = make_resource("a", ServiceState::Running);
        let r2 = make_resource("b", ServiceState::Running);
        let _ = run(&r1, &force_update_diff(&r1), &ctx).unwrap();
        let _ = run(&r2, &force_update_diff(&r2), &ctx).unwrap();
        let snapshot_calls = mock
            .calls()
            .iter()
            .filter(|c| c.as_str() == "service_statuses")
            .count();
        assert_eq!(
            snapshot_calls,
            1,
            "service_statuses должен быть вызван один раз, calls: {:?}",
            mock.calls()
        );
    }

    #[test]
    fn apply_idempotent_reenqueue_deferred_does_not_create_duplicate() {
        // Тригер сработал дважды (например, два разных ресурса дёрнули
        // restart_on на тот же сервис) — журнал должен содержать ровно один
        // файл (idempotent dedup из Phase C).
        let mock = Arc::new(MockRunr::new(vec![status("svc", "Running", 0)]));
        let (tmp, ctx) = make_ctx_with_runr(Some(mock.clone()));
        let r = {
            let mut r = make_resource("svc", ServiceState::Running);
            let src_kind = ResourceKind::from_static("file.content");
            r.restart_on.push(ResourceId::new(&src_kind, "/cfg1"));
            r.restart_on.push(ResourceId::new(&src_kind, "/cfg2"));
            r
        };
        ctx.record_changed(&r.restart_on[0]);
        ctx.record_changed(&r.restart_on[1]);
        let _ = run(&r, &force_update_diff(&r), &ctx).unwrap();
        // Повторный apply того же ресурса (например, plan переоценился) —
        // одна и та же запись.
        let _ = run(&r, &force_update_diff(&r), &ctx).unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".deferred"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "ожидался один defer-файл, got {entries:?}"
        );
    }

    #[test]
    fn map_runr_error_unavailable_is_runr_unavailable() {
        let err = RunrError::Unavailable {
            base_url: "x".into(),
            source: Box::new(std::io::Error::other("eof")),
        };
        let mapped = map_runr_error(err, "x", "op");
        assert!(matches!(mapped, PrimitiveError::RunrUnavailable { .. }));
        assert!(mapped.is_deferrable());
    }

    #[test]
    fn map_runr_error_not_found_is_apply() {
        let err = RunrError::NotFound {
            kind: "service".into(),
            name: "nope".into(),
        };
        let mapped = map_runr_error(err, "x", "op");
        assert!(matches!(mapped, PrimitiveError::Apply { .. }));
        assert!(!mapped.is_deferrable());
    }
}
