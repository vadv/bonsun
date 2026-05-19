//! Apply-фаза `systemd.service`.
//!
//! Логика симметрична `runr_service::apply`:
//! 1. `ctx.systemd is None` → `SystemdUnavailable` (deferrable).
//! 2. Throttle `daemon_reload`: первый ресурс в apply'е, для которого
//!    `needs_daemon_reload(name) == true`, вызывает `daemon_reload()`;
//!    остальные пропускают (флаг `ctx.systemd_daemon_reload_done`).
//! 3. Snapshot `before: UnitInfo` через `unit_info(name)`.
//! 4. Если `spec.enable` — read-before-write: сначала `is_unit_enabled(name)`,
//!    и `enable_unit` зовётся только при `false`. systemd обрабатывает
//!    повторный enable как no-op, но это лишний dbus round-trip на каждый
//!    apply — `GetUnitFileState` дешевле.
//! 5. `decide_action_systemd(spec, before, restart_triggered, reload_triggered)`.
//! 6. Defer-eligible (Restart/Reload) → `ctx.defers.enqueue(...)` ДО
//!    реального вызова — at-least-once гарантия.
//! 7. Synchronous (Start/Stop) → `systemd.start_unit` / `stop_unit` +
//!    `InvocationID` diff verification против `after = unit_info(name)`.
//! 8. Маппинг `SystemdError`:
//!    - `BusUnavailable` / `Dbus` / `Timeout` / `Io` → `SystemdUnavailable`
//!      (deferrable).
//!    - `NoSuchUnit` / `AuthorizationDenied` / `JobFailed` /
//!      `RestartNotObserved` → `Apply` (non-deferrable).

use std::sync::atomic::Ordering;
use std::time::Duration;

use bosun_core::defers::{make_id, DeferAction, DeferEntry, DeferPriority, CURRENT_SPEC_VERSION};
use bosun_core::{
    ApplyCtx, ChangeReport, Diff, HealthCheck, HealthCheckError, PrimitiveError, Resource,
    ValidateError,
};
use bosun_handles::{SystemdError, SystemdHandle, UnitInfo};

use super::plan::{decide_action_systemd, Action};
use super::spec::SystemdServiceSpec;

/// Таймаут на validate-команду перед enqueue restart/reload defer'а.
/// Совпадает с runr.service / file.content.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Максимум попыток в защёлкивающем defer'е до промоушена в `.manual_clear`.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Тег init-системы для defer-id и логов.
const INIT_SYSTEM_SYSTEMD: &str = "systemd";

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

    let spec: SystemdServiceSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service payload: {e}")))?;

    let Some(systemd) = ctx.systemd.as_ref() else {
        return Err(PrimitiveError::SystemdUnavailable {
            reason: "systemd client not initialized in ApplyCtx".to_string(),
        });
    };

    // 1. Throttle daemon_reload. Семантически: спрашиваем у systemd, нужен
    // ли daemon-reload для текущего unit'а; если да и флаг ещё не
    // выставлен — делаем reload и поднимаем флаг. Если `needs_daemon_reload`
    // вернул false — флаг не трогаем, чтобы другой ресурс мог проверить
    // свой unit. Если флаг уже стоит — пропускаем проверку.
    if !ctx.systemd_daemon_reload_done.load(Ordering::Acquire) {
        match systemd.needs_daemon_reload(&spec.name) {
            Ok(true) => {
                // Атомарно ставим флаг — если кто-то параллельно тоже
                // поднял, swap вернёт true и мы пропустим вызов.
                if !ctx.systemd_daemon_reload_done.swap(true, Ordering::AcqRel) {
                    tracing::debug!(
                        unit = %spec.name,
                        "calling systemd.daemon_reload (first NeedDaemonReload=true in apply)",
                    );
                    if let Err(e) = systemd.daemon_reload() {
                        // Откатываем флаг: следующий ресурс попробует
                        // снова, иначе мы навсегда «съели» reload.
                        ctx.systemd_daemon_reload_done
                            .store(false, Ordering::Release);
                        return Err(map_systemd_error(e, "daemon_reload"));
                    }
                }
            }
            Ok(false) => {
                // Не нужен — флаг не трогаем.
            }
            Err(e) => {
                return Err(map_systemd_error(e, "needs_daemon_reload"));
            }
        }
    }

    // 2. EnableUnitFiles если требуется. Read-before-write: сначала
    // `is_unit_enabled` (GetUnitFileState), потом enable_unit только если
    // юнит ещё не включён. Экономит dbus round-trip на повторных apply'ях.
    if spec.enable {
        let already_enabled = systemd
            .is_unit_enabled(&spec.name)
            .map_err(|e| map_systemd_error(e, "is_unit_enabled"))?;
        if !already_enabled {
            if let Err(e) = systemd.enable_unit(&spec.name) {
                return Err(map_systemd_error(e, "enable_unit"));
            }
        }
    }

    // 3. Pre-snapshot. `unit_info` возвращает name/active_state/sub_state/
    // invocation_id/exec_main_start_timestamp; нам важен active_state
    // (для decide_action) и invocation_id (для verify).
    let before = match systemd.unit_info(&spec.name) {
        Ok(info) => Some(info),
        // unit ещё не загружен (нет file'а либо load not attempted) —
        // трактуем как not active.
        Err(SystemdError::NoSuchUnit(_)) => None,
        Err(e) => return Err(map_systemd_error(e, "unit_info")),
    };

    // 4. Тригеры notify.
    let restart_triggered = resource.restart_on.iter().any(|id| ctx.is_changed(id));
    let reload_triggered = resource.reload_on.iter().any(|id| ctx.is_changed(id));
    let action = decide_action_systemd(&spec, before.as_ref(), restart_triggered, reload_triggered);

    let sources = collect_notify_sources(resource, ctx, restart_triggered, reload_triggered);

    match action {
        Action::NoChange => Ok(ChangeReport::no_change()),
        Action::Start => {
            let report = execute_start(systemd.as_ref(), &spec, before.as_ref())?;
            run_health_check_if_configured(&spec, ctx)?;
            Ok(report)
        }
        Action::Stop => execute_stop(systemd.as_ref(), &spec),
        Action::Restart => {
            // Phase H: validate_with запускается ДО enqueue. Failure →
            // defer не появляется, оператор видит синхронную ошибку.
            run_validate_if_configured(&spec, ctx)?;
            enqueue_defer(
                ctx,
                &spec,
                DeferAction::Restart,
                DeferPriority::Restart,
                sources,
            )
        }
        Action::Reload => {
            run_validate_if_configured(&spec, ctx)?;
            enqueue_defer(
                ctx,
                &spec,
                DeferAction::Reload,
                DeferPriority::Reload,
                sources,
            )
        }
    }
}

/// Запустить `validate_with` (если задан) перед enqueue defer'а
/// restart/reload. У service.unit нет `<path>.new`: validator проверяет
/// текущий target config (он уже на месте — file.content валидировал
/// свой `.new` до swap'а ранее в apply'е).
///
/// Failure → `PrimitiveError::Validation`, defer не enqueue'ится.
fn run_validate_if_configured(
    spec: &SystemdServiceSpec,
    ctx: &ApplyCtx,
) -> Result<(), PrimitiveError> {
    let Some(argv) = spec.validate_with.as_deref() else {
        return Ok(());
    };
    if argv.is_empty() {
        return Ok(());
    }
    let validator_name = argv[0].clone();
    tracing::info!(
        unit = %spec.name,
        validator = %validator_name,
        "systemd.service: running validate_with before defer enqueue",
    );
    match ctx.validator.run(argv, VALIDATE_TIMEOUT) {
        Ok(()) => {
            tracing::info!(
                unit = %spec.name,
                validator = %validator_name,
                "systemd.service: validate_with passed",
            );
            Ok(())
        }
        Err(err) => {
            tracing::warn!(
                unit = %spec.name,
                validator = %validator_name,
                error = %err,
                "systemd.service: validate_with failed; defer not enqueued",
            );
            Err(map_validate_error(err, &validator_name))
        }
    }
}

/// Phase I: запустить health-check после успешного синхронного Start.
/// Симметрично `runr_service::run_health_check_if_configured`. Restart/
/// Reload идут через defer и проверяются в `replay_with_health_check`.
fn run_health_check_if_configured(
    spec: &SystemdServiceSpec,
    ctx: &ApplyCtx,
) -> Result<(), PrimitiveError> {
    let Some(check) = spec.health_check.as_ref() else {
        return Ok(());
    };
    let kind = health_check_kind(check);
    tracing::info!(
        unit = %spec.name,
        kind = %kind,
        "systemd.service: running health-check after sync start",
    );
    match ctx.health_check_runner.run(check, &ctx.cancel) {
        Ok(()) => {
            tracing::info!(
                unit = %spec.name,
                kind = %kind,
                "systemd.service: health-check passed",
            );
            Ok(())
        }
        Err(HealthCheckError::Cancelled) => {
            tracing::warn!(
                unit = %spec.name,
                "systemd.service: health-check cancelled (deadline/SIGTERM)",
            );
            Err(PrimitiveError::Cancelled)
        }
        Err(err) => {
            tracing::warn!(
                unit = %spec.name,
                error = %err,
                "systemd.service: health-check failed",
            );
            Err(PrimitiveError::HealthCheckFailed {
                target: spec.name.as_str().to_string(),
                reason: err.to_string(),
            })
        }
    }
}

/// Описать вариант health-check'а строкой для логов (`cmd`/`url`).
fn health_check_kind(check: &HealthCheck) -> &'static str {
    match check {
        HealthCheck::Cmd { .. } => "cmd",
        HealthCheck::Url { .. } => "url",
        _ => "unknown",
    }
}

/// Маппинг `ValidateError` → `PrimitiveError::Validation`. Зеркалит
/// `runr_service::map_validate_error`.
fn map_validate_error(err: ValidateError, validator: &str) -> PrimitiveError {
    let stderr_excerpt = match err {
        ValidateError::ExitNonZero { stderr_excerpt, .. } => stderr_excerpt,
        ValidateError::Timeout(d) => format!("timeout after {d:?}"),
        ValidateError::Spawn(e) => format!("failed to spawn: {e}"),
        other => format!("validator error: {other}"),
    };
    PrimitiveError::Validation {
        validator: validator.to_string(),
        stderr_excerpt,
    }
}

/// Синхронно запустить unit и сверить, что он реально запустился.
///
/// Проверка через InvocationID: snapshot `before` уже сделан выше;
/// после `start_unit` берём `after` и сравниваем. Для unit'ов, которые
/// ещё не были загружены (`before is None`), достаточно факта, что
/// `after.invocation_id` непустой.
fn execute_start(
    systemd: &dyn SystemdHandle,
    spec: &SystemdServiceSpec,
    before: Option<&UnitInfo>,
) -> Result<ChangeReport, PrimitiveError> {
    tracing::info!(unit = %spec.name, "systemd.service: start");
    systemd
        .start_unit(&spec.name)
        .map_err(|e| map_systemd_error(e, "start_unit"))?;
    let after = systemd
        .unit_info(&spec.name)
        .map_err(|e| map_systemd_error(e, "unit_info"))?;
    // InvocationID должен либо появиться (если before None), либо
    // отличаться от before (если был). Пустой invocation_id у `after`
    // означает, что unit_info вернул unit, но systemd ещё не присвоил
    // ему ID — это не сбой, а просто timing: считаем успехом.
    verify_invocation_change(before, &after, spec)?;
    Ok(ChangeReport::changed(format!(
        "started systemd.service:{}",
        spec.name
    )))
}

/// Синхронно остановить unit. Для Stop verify InvocationID не требуется:
/// `wait_for_job` (внутри `stop_unit`) уже убеждается через
/// `ActiveState != failed`, что job завершился штатно.
fn execute_stop(
    systemd: &dyn SystemdHandle,
    spec: &SystemdServiceSpec,
) -> Result<ChangeReport, PrimitiveError> {
    tracing::info!(unit = %spec.name, "systemd.service: stop");
    systemd
        .stop_unit(&spec.name)
        .map_err(|e| map_systemd_error(e, "stop_unit"))?;
    Ok(ChangeReport::changed(format!(
        "stopped systemd.service:{}",
        spec.name
    )))
}

/// Проверка изменения `InvocationID`. Срабатывает на Debian bug 996911:
/// systemd JobRemoved=done, но unit на самом деле не рестартанулся (тот
/// же main PID). Признак — `invocation_id` не изменился между before и
/// after.
fn verify_invocation_change(
    before: Option<&UnitInfo>,
    after: &UnitInfo,
    spec: &SystemdServiceSpec,
) -> Result<(), PrimitiveError> {
    let Some(before) = before else {
        // Не было ничего — `after` создал unit. Главное, чтобы он стал
        // active либо хотя бы получил invocation_id; обработка `Failed`
        // уже сделана wait_for_job внутри start_unit.
        return Ok(());
    };
    if before.invocation_id.is_empty() {
        // До старта InvocationID не было — это нормально (unit не
        // активирован). Любой непустой after.invocation_id — успех;
        // пустой — диагностика systemd не отдала, но не наша проблема.
        return Ok(());
    }
    if !after.invocation_id.is_empty() && after.invocation_id == before.invocation_id {
        return Err(PrimitiveError::Apply {
            reason: format!(
                "systemd.service:{}: restart not observed (InvocationID unchanged: {})",
                spec.name, before.invocation_id
            ),
        });
    }
    Ok(())
}

/// Положить запись в журнал defers ДО реального вызова systemd. Phase E
/// инвариант: если bosun упадёт между enqueue и реальным
/// restart_unit/reload_unit, replay-цикл подхватит defer.
fn enqueue_defer(
    ctx: &ApplyCtx,
    spec: &SystemdServiceSpec,
    defer_action: DeferAction,
    priority: DeferPriority,
    sources: Vec<String>,
) -> Result<ChangeReport, PrimitiveError> {
    let id = make_id(INIT_SYSTEM_SYSTEMD, &defer_action, spec.name.as_str());
    let entry = DeferEntry {
        spec_version: CURRENT_SPEC_VERSION,
        id: id.clone(),
        action: defer_action.clone(),
        init_system: INIT_SYSTEM_SYSTEMD.to_string(),
        target: spec.name.as_str().to_string(),
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
        "systemd.service: enqueueing defer",
    );
    ctx.defers
        .enqueue(entry)
        .map_err(|e| PrimitiveError::DeferIo {
            path: ctx.defers.root().to_path_buf(),
            reason: format!("{e}"),
        })?;
    Ok(ChangeReport::deferred(format!(
        "deferred {} of systemd.service:{}",
        action_slug, spec.name
    )))
}

/// Источники notify для `enqueued_by`. Те же ресурсы, что и
/// `runr_service::collect_notify_sources`.
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

/// Маппинг `SystemdError` → `PrimitiveError`. Таблица решает, попадёт ли
/// ресурс в `Outcome::Deferred` (retry на следующем цикле) или сразу в
/// `Outcome::Failed`:
///
/// | SystemdError          | PrimitiveError                 | is_deferrable |
/// |-----------------------|--------------------------------|---------------|
/// | BusUnavailable        | SystemdUnavailable             | true          |
/// | Dbus                  | SystemdUnavailable             | true          |
/// | Timeout               | SystemdUnavailable             | true          |
/// | NoSuchUnit            | Apply                          | false         |
/// | AuthorizationDenied   | Apply                          | false         |
/// | JobFailed             | Apply                          | false         |
/// | RestartNotObserved    | Apply                          | false         |
/// | Io                    | Io (отдельный вариант)         | false         |
///
/// Io не deferrable: повторный цикл не починит сломанный сокет dbus или
/// упавший файл бэкенда — это сигнал «нода в нештатном состоянии».
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
        // non_exhaustive: новые варианты — Apply с текстом.
        other => PrimitiveError::Apply {
            reason: format!("systemd error during {op}: {other}"),
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
    use bosun_core::{
        ApplyCtx, Diff, PrimitiveError, Resource, ResourceId, ResourceKind, SensitiveStore,
    };
    use bosun_handles::{SystemdError, SystemdHandle, UnitInfo};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::systemd_service::spec::ServiceState;

    /// Mock-handle с логом вызовов и подменяемыми ответами `unit_info`.
    /// `before_after`: первый вызов возвращает `before_state`, второй —
    /// `after_state`. Используется в тестах verify InvocationID.
    struct MockSystemd {
        calls: Mutex<Vec<String>>,
        daemon_reload_count: AtomicU32,
        needs_daemon_reload_response: Mutex<bool>,
        // Очередь ответов на unit_info(name) — каждый вызов pop'ит с конца.
        unit_info_queue: Mutex<Vec<Result<UnitInfo, SystemdError>>>,
        // Что вернуть на start_unit / stop_unit (None = Ok).
        start_error: Mutex<Option<SystemdError>>,
        stop_error: Mutex<Option<SystemdError>>,
        // Что вернуть на enable_unit / disable_unit.
        enable_error: Mutex<Option<SystemdError>>,
        disable_error: Mutex<Option<SystemdError>>,
        // Ответ `is_unit_enabled`. По умолчанию `false` — апплай идёт в
        // `enable_unit`, как требуется большинству тестов. Чтобы тестировать
        // путь «уже включён», см. `with_is_unit_enabled(true)`. Для error-path
        // используем одноразовый инжектор `is_unit_enabled_error`.
        is_unit_enabled_response: Mutex<bool>,
        is_unit_enabled_error: Mutex<Option<SystemdError>>,
    }

    impl MockSystemd {
        fn new() -> Self {
            Self {
                calls: Mutex::new(vec![]),
                daemon_reload_count: AtomicU32::new(0),
                needs_daemon_reload_response: Mutex::new(false),
                unit_info_queue: Mutex::new(vec![]),
                start_error: Mutex::new(None),
                stop_error: Mutex::new(None),
                enable_error: Mutex::new(None),
                disable_error: Mutex::new(None),
                is_unit_enabled_response: Mutex::new(false),
                is_unit_enabled_error: Mutex::new(None),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, label: &str) {
            self.calls.lock().unwrap().push(label.to_string());
        }

        fn with_needs_reload(self, v: bool) -> Self {
            *self.needs_daemon_reload_response.lock().unwrap() = v;
            self
        }

        /// Подменить ответ `is_unit_enabled`. По умолчанию `false`.
        fn with_is_unit_enabled(self, v: bool) -> Self {
            *self.is_unit_enabled_response.lock().unwrap() = v;
            self
        }

        /// Положить последовательность ответов `unit_info`. Очередь
        /// потребляется в порядке push'а.
        fn enqueue_unit_info(&self, info: Result<UnitInfo, SystemdError>) {
            self.unit_info_queue.lock().unwrap().push(info);
        }
    }

    impl SystemdHandle for MockSystemd {
        fn daemon_reload(&self) -> Result<(), SystemdError> {
            self.daemon_reload_count.fetch_add(1, Ordering::AcqRel);
            self.record("daemon_reload");
            Ok(())
        }
        fn needs_daemon_reload(&self, _unit_name: &str) -> Result<bool, SystemdError> {
            self.record("needs_daemon_reload");
            Ok(*self.needs_daemon_reload_response.lock().unwrap())
        }
        fn start_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("start_unit:{name}"));
            if let Some(e) = self.start_error.lock().unwrap().take() {
                return Err(e);
            }
            Ok(())
        }
        fn stop_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("stop_unit:{name}"));
            if let Some(e) = self.stop_error.lock().unwrap().take() {
                return Err(e);
            }
            Ok(())
        }
        fn restart_unit(&self, name: &str) -> Result<(), SystemdError> {
            // НЕ должен вызываться напрямую в Phase E apply — restart идёт
            // в defer.
            self.record(&format!("restart_unit:{name}"));
            Ok(())
        }
        fn reload_unit(&self, name: &str) -> Result<(), SystemdError> {
            // НЕ должен вызываться напрямую в Phase E apply.
            self.record(&format!("reload_unit:{name}"));
            Ok(())
        }
        fn enable_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("enable_unit:{name}"));
            if let Some(e) = self.enable_error.lock().unwrap().take() {
                return Err(e);
            }
            Ok(())
        }
        fn is_unit_enabled(&self, name: &str) -> Result<bool, SystemdError> {
            self.record(&format!("is_unit_enabled:{name}"));
            if let Some(e) = self.is_unit_enabled_error.lock().unwrap().take() {
                return Err(e);
            }
            Ok(*self.is_unit_enabled_response.lock().unwrap())
        }
        fn disable_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.record(&format!("disable_unit:{name}"));
            if let Some(e) = self.disable_error.lock().unwrap().take() {
                return Err(e);
            }
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

    fn make_unit(name: &str, active: &str, invocation: &str) -> UnitInfo {
        UnitInfo {
            name: name.to_string(),
            active_state: active.to_string(),
            sub_state: "running".to_string(),
            invocation_id: invocation.to_string(),
            exec_main_start_timestamp: Some(100),
        }
    }

    fn make_resource(name: &str, state: ServiceState, enable: bool) -> Resource {
        let kind = ResourceKind::from_static("systemd.service");
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
    fn apply_returns_no_change_for_diff_no_change() {
        let mock = Arc::new(MockSystemd::new());
        let r = make_resource("nginx.service", ServiceState::Running, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
        assert!(!report.deferred);
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn apply_returns_systemd_unavailable_when_ctx_systemd_none() {
        let r = make_resource("nginx.service", ServiceState::Running, true);
        let (_tmp, ctx) = make_ctx(None);
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        match err {
            PrimitiveError::SystemdUnavailable { reason } => {
                assert!(reason.contains("not initialized"), "got: {reason}");
            }
            other => panic!("expected SystemdUnavailable, got {other:?}"),
        }
        // SystemdUnavailable должен быть deferrable.
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        assert!(err.is_deferrable());
    }

    #[test]
    fn apply_daemon_reload_throttled_to_one_call_per_apply() {
        // Прогоняем три ресурса через один ctx. needs_daemon_reload=true,
        // и daemon_reload должен вызваться ровно один раз.
        let mock = Arc::new(MockSystemd::new().with_needs_reload(true));
        // На каждый ресурс заполняем unit_info: первый запрос — before,
        // а если будет Start, потом ещё after. Для трёх ресурсов с
        // state=Running×active → NoChange, поэтому достаточно одного
        // unit_info на ресурс.
        for _ in 0..3 {
            mock.enqueue_unit_info(Ok(make_unit("x", "active", "abc")));
        }
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        for name in ["a.service", "b.service", "c.service"] {
            // enable=false, чтобы не загромождать вызовами enable_unit
            // (это не цель текущего теста).
            let r = make_resource(name, ServiceState::Running, false);
            let _ = run(&r, &force_update(&r), &ctx);
        }
        assert_eq!(
            mock.daemon_reload_count.load(Ordering::Acquire),
            1,
            "daemon_reload должен быть вызван ровно один раз, вызовы: {:?}",
            mock.calls()
        );
    }

    #[test]
    fn apply_daemon_reload_not_called_when_needs_returns_false() {
        let mock = Arc::new(MockSystemd::new().with_needs_reload(false));
        mock.enqueue_unit_info(Ok(make_unit("x", "active", "abc")));
        let r = make_resource("x.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let _ = run(&r, &force_update(&r), &ctx).unwrap();
        assert_eq!(
            mock.daemon_reload_count.load(Ordering::Acquire),
            0,
            "daemon_reload не должен быть вызван, если needs_daemon_reload=false"
        );
    }

    #[test]
    fn apply_running_with_inactive_calls_start_and_verifies_invocation() {
        let mock = Arc::new(MockSystemd::new());
        // before: inactive с пустым invocation, after: active с непустым → ok.
        mock.enqueue_unit_info(Ok(make_unit("nginx", "inactive", "")));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "newid")));
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        assert!(mock.calls().iter().any(|c| c == "start_unit:nginx.service"));
    }

    #[test]
    fn apply_running_with_no_such_unit_calls_start_and_succeeds() {
        let mock = Arc::new(MockSystemd::new());
        // Pre-snapshot: NoSuchUnit (unit ещё не загружен) → trace as not active.
        mock.enqueue_unit_info(Err(SystemdError::NoSuchUnit("nginx.service".into())));
        // After-snapshot: active.
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "fresh")));
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
    }

    #[test]
    fn apply_stopped_active_calls_stop() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = make_resource("nginx.service", ServiceState::Stopped, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        assert!(mock.calls().iter().any(|c| c == "stop_unit:nginx.service"));
    }

    #[test]
    fn apply_running_active_no_triggers_is_no_change() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(!report.changed);
        // НИ start, НИ stop не должны быть вызваны.
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| c.starts_with("start_unit:")));
        assert!(!calls.iter().any(|c| c.starts_with("stop_unit:")));
        assert!(!calls.iter().any(|c| c.starts_with("restart_unit:")));
        assert!(!calls.iter().any(|c| c.starts_with("reload_unit:")));
    }

    #[test]
    fn apply_restart_trigger_enqueues_defer_does_not_call_systemd_restart() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = {
            let mut r = make_resource("nginx.service", ServiceState::Running, false);
            let src_kind = ResourceKind::from_static("file.content");
            r.restart_on
                .push(ResourceId::new(&src_kind, "/etc/nginx.conf"));
            r
        };
        let (tmp, ctx) = make_ctx(Some(mock.clone()));
        ctx.record_changed(&r.restart_on[0]);

        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.deferred);
        // restart_unit НЕ должен быть вызван — это инвариант.
        assert!(
            !mock
                .calls()
                .iter()
                .any(|c| c == "restart_unit:nginx.service"),
            "restart_unit должен быть отложен, не выполнен синхронно"
        );

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".deferred"))
            .collect();
        assert_eq!(entries.len(), 1, "should be one deferred file: {entries:?}");
        assert!(
            entries[0].contains("systemd.restart:nginx.service"),
            "expected systemd.restart:nginx.service in filename, got {entries:?}"
        );
        // Префикс 0r- — приоритет Restart.
        assert!(entries[0].starts_with("0r-"));
    }

    #[test]
    fn apply_reload_trigger_enqueues_reload_defer() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = {
            let mut r = make_resource("nginx.service", ServiceState::Running, false);
            let src_kind = ResourceKind::from_static("file.content");
            r.reload_on
                .push(ResourceId::new(&src_kind, "/etc/nginx.conf"));
            r
        };
        let (tmp, ctx) = make_ctx(Some(mock.clone()));
        ctx.record_changed(&r.reload_on[0]);

        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.deferred);
        assert!(!mock
            .calls()
            .iter()
            .any(|c| c == "reload_unit:nginx.service"));
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".deferred"))
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].contains("systemd.reload:nginx.service"));
        // Префикс 2r- — приоритет Reload.
        assert!(entries[0].starts_with("2r-"));
    }

    #[test]
    fn apply_invocation_unchanged_returns_apply_error() {
        // Симулируем Debian bug 996911: чтобы попасть в путь Start, делаем
        // before inactive, а после `start_unit` возвращаем тот же invocation_id —
        // verify должен зафиксировать «restart not observed».
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "inactive", "stable")));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "stable")));
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("InvocationID unchanged"), "got: {reason}");
            }
            other => panic!("expected Apply with InvocationID, got {other:?}"),
        }
    }

    #[test]
    fn apply_invocation_change_passes_verify() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "inactive", "old")));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "new")));
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
    }

    #[test]
    fn apply_enable_true_calls_enable_unit() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = make_resource("nginx.service", ServiceState::Running, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let _ = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(mock
            .calls()
            .iter()
            .any(|c| c == "enable_unit:nginx.service"));
    }

    #[test]
    fn apply_enable_false_does_not_call_enable_unit() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let _ = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(!mock
            .calls()
            .iter()
            .any(|c| c == "enable_unit:nginx.service"));
    }

    #[test]
    fn apply_enable_true_already_enabled_skips_enable_unit() {
        // is_unit_enabled=true → enable_unit пропускается (read-before-write).
        let mock = Arc::new(MockSystemd::new().with_is_unit_enabled(true));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = make_resource("nginx.service", ServiceState::Running, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let _ = run(&r, &force_update(&r), &ctx).unwrap();
        let calls = mock.calls();
        assert!(
            calls.iter().any(|c| c == "is_unit_enabled:nginx.service"),
            "expected is_unit_enabled to be called, got {calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c == "enable_unit:nginx.service"),
            "enable_unit must be skipped when already enabled, got {calls:?}"
        );
    }

    #[test]
    fn apply_enable_true_when_not_enabled_calls_enable_unit() {
        // is_unit_enabled=false → enable_unit вызывается.
        let mock = Arc::new(MockSystemd::new().with_is_unit_enabled(false));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let r = make_resource("nginx.service", ServiceState::Running, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let _ = run(&r, &force_update(&r), &ctx).unwrap();
        let calls = mock.calls();
        assert!(
            calls.iter().any(|c| c == "is_unit_enabled:nginx.service"),
            "expected is_unit_enabled to be called, got {calls:?}"
        );
        assert!(
            calls.iter().any(|c| c == "enable_unit:nginx.service"),
            "enable_unit must be called when not enabled, got {calls:?}"
        );
    }

    #[test]
    fn apply_enable_true_is_unit_enabled_error_propagates() {
        // Ошибка от is_unit_enabled должна пробрасываться, enable не дёргается.
        let mock = Arc::new(MockSystemd::new());
        *mock.is_unit_enabled_error.lock().unwrap() = Some(SystemdError::BusUnavailable {
            reason: "socket missing".into(),
            source: zbus::Error::Address("unix:path=/nonexistent".into()),
        });
        let r = make_resource("nginx.service", ServiceState::Running, true);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        assert!(
            err.is_deferrable(),
            "BusUnavailable должен маппиться в deferrable SystemdUnavailable, got {err:?}"
        );
        match err {
            PrimitiveError::SystemdUnavailable { reason } => {
                assert!(
                    reason.contains("is_unit_enabled"),
                    "reason должен ссылаться на is_unit_enabled, got {reason}"
                );
            }
            other => panic!("expected SystemdUnavailable, got {other:?}"),
        }
        assert!(
            !mock
                .calls()
                .iter()
                .any(|c| c == "enable_unit:nginx.service"),
            "enable_unit не должен вызываться при ошибке is_unit_enabled"
        );
    }

    #[test]
    fn apply_bus_unavailable_during_needs_reload_is_deferrable() {
        struct UnavailableSystemd;
        impl SystemdHandle for UnavailableSystemd {
            fn daemon_reload(&self) -> Result<(), SystemdError> {
                unimplemented!()
            }
            fn needs_daemon_reload(&self, _: &str) -> Result<bool, SystemdError> {
                Err(SystemdError::BusUnavailable {
                    reason: "socket missing".into(),
                    source: zbus::Error::Address("unix:path=/nonexistent".into()),
                })
            }
            fn start_unit(&self, _: &str) -> Result<(), SystemdError> {
                unimplemented!()
            }
            fn stop_unit(&self, _: &str) -> Result<(), SystemdError> {
                unimplemented!()
            }
            fn restart_unit(&self, _: &str) -> Result<(), SystemdError> {
                unimplemented!()
            }
            fn reload_unit(&self, _: &str) -> Result<(), SystemdError> {
                unimplemented!()
            }
            fn enable_unit(&self, _: &str) -> Result<(), SystemdError> {
                unimplemented!()
            }
            fn is_unit_enabled(&self, _: &str) -> Result<bool, SystemdError> {
                unimplemented!()
            }
            fn disable_unit(&self, _: &str) -> Result<(), SystemdError> {
                unimplemented!()
            }
            fn unit_info(&self, _: &str) -> Result<UnitInfo, SystemdError> {
                unimplemented!()
            }
        }
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(Arc::new(UnavailableSystemd) as Arc<dyn SystemdHandle>));
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        assert!(
            err.is_deferrable(),
            "ожидался deferrable error, got {err:?}"
        );
        match err {
            PrimitiveError::SystemdUnavailable { .. } => {}
            other => panic!("expected SystemdUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn apply_no_such_unit_during_start_returns_non_deferrable_apply() {
        let mock = Arc::new(MockSystemd::new());
        // pre-snapshot: NoSuchUnit → fall through. start_unit вернёт error.
        mock.enqueue_unit_info(Err(SystemdError::NoSuchUnit("missing.service".into())));
        *mock.start_error.lock().unwrap() =
            Some(SystemdError::NoSuchUnit("missing.service".into()));
        let r = make_resource("missing.service", ServiceState::Running, false);
        let (_tmp, ctx) = make_ctx(Some(mock.clone()));
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        assert!(!err.is_deferrable());
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("not found"), "got: {reason}")
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn apply_idempotent_reenqueue_deferred_does_not_create_duplicate() {
        // Триггер сработал дважды (два разных ресурса → один сервис) — журнал
        // должен содержать ровно один файл.
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let (tmp, ctx) = make_ctx(Some(mock.clone()));
        let r = {
            let mut r = make_resource("nginx.service", ServiceState::Running, false);
            let src_kind = ResourceKind::from_static("file.content");
            r.restart_on.push(ResourceId::new(&src_kind, "/cfg1"));
            r.restart_on.push(ResourceId::new(&src_kind, "/cfg2"));
            r
        };
        ctx.record_changed(&r.restart_on[0]);
        ctx.record_changed(&r.restart_on[1]);
        let _ = run(&r, &force_update(&r), &ctx).unwrap();
        let _ = run(&r, &force_update(&r), &ctx).unwrap();
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
    fn map_systemd_error_bus_unavailable_is_deferrable() {
        let err = SystemdError::BusUnavailable {
            reason: "socket missing".into(),
            source: zbus::Error::Address("unix:path=/nonexistent".into()),
        };
        let mapped = map_systemd_error(err, "op");
        assert!(mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::SystemdUnavailable { .. }));
    }

    #[test]
    fn map_systemd_error_no_such_unit_is_non_deferrable() {
        let err = SystemdError::NoSuchUnit("x".into());
        let mapped = map_systemd_error(err, "op");
        assert!(!mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::Apply { .. }));
    }

    #[test]
    fn map_systemd_error_authorization_denied_is_apply() {
        let err = SystemdError::AuthorizationDenied {
            action: "start_unit".into(),
            unit: "nginx".into(),
        };
        let mapped = map_systemd_error(err, "op");
        assert!(!mapped.is_deferrable());
        match mapped {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("authorization denied"), "got: {reason}");
                assert!(reason.contains("polkit"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn map_systemd_error_job_failed_is_apply() {
        let err = SystemdError::JobFailed {
            job: "/job/1".into(),
            result: "failed".into(),
            active_state: "failed".into(),
        };
        let mapped = map_systemd_error(err, "start_unit");
        assert!(!mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::Apply { .. }));
    }

    #[test]
    fn map_systemd_error_restart_not_observed_is_apply() {
        let err = SystemdError::RestartNotObserved {
            unit: "nginx".into(),
        };
        let mapped = map_systemd_error(err, "restart_unit");
        assert!(!mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::Apply { .. }));
    }

    #[test]
    fn map_systemd_error_timeout_is_deferrable() {
        let err = SystemdError::Timeout(Duration::from_secs(60));
        let mapped = map_systemd_error(err, "wait");
        assert!(mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::SystemdUnavailable { .. }));
    }

    #[test]
    fn map_systemd_error_io_is_io_primitive_error() {
        let err = SystemdError::Io(std::io::Error::other("bad fd"));
        let mapped = map_systemd_error(err, "x");
        assert!(!mapped.is_deferrable());
        assert!(matches!(mapped, PrimitiveError::Io { .. }));
    }

    // ===== Phase H: validate_with =====

    use bosun_core::{ValidateError, ValidateRunner};

    /// Mock-validator: записывает argv, возвращает Ok или Fail.
    struct MockValidator {
        calls: Mutex<Vec<Vec<String>>>,
        response: Mutex<MockValidatorResponse>,
    }

    #[derive(Clone)]
    enum MockValidatorResponse {
        Ok,
        Fail { stderr: String },
    }

    impl MockValidator {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(MockValidatorResponse::Ok),
            })
        }
        fn failing(stderr: &str) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(MockValidatorResponse::Fail {
                    stderr: stderr.to_string(),
                }),
            })
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ValidateRunner for MockValidator {
        fn run(&self, argv: &[String], _timeout: Duration) -> Result<(), ValidateError> {
            self.calls.lock().unwrap().push(argv.to_vec());
            match self.response.lock().unwrap().clone() {
                MockValidatorResponse::Ok => Ok(()),
                MockValidatorResponse::Fail { stderr } => Err(ValidateError::ExitNonZero {
                    exit_code: 1,
                    stderr_excerpt: stderr,
                }),
            }
        }
    }

    fn make_ctx_with_systemd_and_validator(
        systemd: Option<Arc<dyn SystemdHandle>>,
        validator: Arc<dyn ValidateRunner>,
    ) -> (TempDir, ApplyCtx) {
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::with_validator(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            defers,
            None,
            systemd,
            validator,
        );
        (tmp, ctx)
    }

    fn make_resource_with_validate(
        name: &str,
        state: ServiceState,
        enable: bool,
        validate_with: Vec<String>,
    ) -> Resource {
        let kind = ResourceKind::from_static("systemd.service");
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
                "enable": enable,
                "validate_with": validate_with,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn validate_with_success_allows_restart_defer_enqueue() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let validator = MockValidator::ok();
        let r = {
            let mut r = make_resource_with_validate(
                "nginx.service",
                ServiceState::Running,
                false,
                vec!["nginx".into(), "-t".into()],
            );
            let src_kind = ResourceKind::from_static("file.content");
            r.restart_on
                .push(ResourceId::new(&src_kind, "/etc/nginx.conf"));
            r
        };
        let (tmp, ctx) = make_ctx_with_systemd_and_validator(
            Some(mock.clone()),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        ctx.record_changed(&r.restart_on[0]);

        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.deferred);
        // validator вызван один раз с argv из spec.
        assert_eq!(validator.calls().len(), 1);
        assert_eq!(validator.calls()[0], vec!["nginx", "-t"]);
        // Defer-файл создан.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".deferred"))
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn validate_with_failure_blocks_restart_defer_enqueue() {
        // Главный инвариант: failed validator → defer НЕ enqueue'ится,
        // оператор видит синхронную ошибку. restart_unit на mock'е
        // тоже не должен дёргаться.
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let validator = MockValidator::failing("nginx: configuration test failed");
        let r = {
            let mut r = make_resource_with_validate(
                "nginx.service",
                ServiceState::Running,
                false,
                vec!["nginx".into(), "-t".into()],
            );
            let src_kind = ResourceKind::from_static("file.content");
            r.restart_on
                .push(ResourceId::new(&src_kind, "/etc/nginx.conf"));
            r
        };
        let (tmp, ctx) = make_ctx_with_systemd_and_validator(
            Some(mock.clone()),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        ctx.record_changed(&r.restart_on[0]);

        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        match err {
            PrimitiveError::Validation {
                validator: v,
                stderr_excerpt,
            } => {
                assert_eq!(v, "nginx");
                assert!(
                    stderr_excerpt.contains("configuration test failed"),
                    "stderr должен быть в reason, got: {stderr_excerpt}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        // restart_unit НЕ вызывался — defer не enqueue'ился и replay
        // тоже не запускался.
        assert!(!mock
            .calls()
            .iter()
            .any(|c| c == "restart_unit:nginx.service"));
        // Defer-файлы отсутствуют.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".deferred"))
            .collect();
        assert!(
            entries.is_empty(),
            "defer не должен enqueue'иться, got {entries:?}"
        );
    }

    #[test]
    fn validate_with_failure_blocks_reload_defer_enqueue() {
        // То же самое для reload-action.
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let validator = MockValidator::failing("bad reload");
        let r = {
            let mut r = make_resource_with_validate(
                "nginx.service",
                ServiceState::Running,
                false,
                vec!["nginx".into(), "-t".into()],
            );
            let src_kind = ResourceKind::from_static("file.content");
            r.reload_on
                .push(ResourceId::new(&src_kind, "/etc/nginx.conf"));
            r
        };
        let (tmp, ctx) = make_ctx_with_systemd_and_validator(
            Some(mock.clone()),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        ctx.record_changed(&r.reload_on[0]);

        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::Validation { .. }));
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".deferred"))
            .collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn validate_with_not_called_for_start_action() {
        // Start не запускает validate_with: семантически validate — про
        // restart/reload running-сервиса. Запуск из inactive с битым
        // конфигом обнаружится при start_unit либо в file.content's
        // validate_with до swap'а.
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "inactive", "")));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "new")));
        let validator = MockValidator::failing("would block start");
        let r = make_resource_with_validate(
            "nginx.service",
            ServiceState::Running,
            false,
            vec!["nginx".into(), "-t".into()],
        );
        let (_tmp, ctx) = make_ctx_with_systemd_and_validator(
            Some(mock.clone()),
            validator.clone() as Arc<dyn ValidateRunner>,
        );

        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        // validator НЕ должен быть вызван.
        assert!(validator.calls().is_empty(),);
    }

    #[test]
    fn no_validate_with_path_unchanged() {
        // Без validate_with restart-defer enqueue'ится напрямую.
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let validator = MockValidator::ok();
        let r = {
            let mut r = make_resource("nginx.service", ServiceState::Running, false);
            let src_kind = ResourceKind::from_static("file.content");
            r.restart_on
                .push(ResourceId::new(&src_kind, "/etc/nginx.conf"));
            r
        };
        let (tmp, ctx) = make_ctx_with_systemd_and_validator(
            Some(mock.clone()),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        ctx.record_changed(&r.restart_on[0]);

        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.deferred);
        assert!(validator.calls().is_empty());
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".deferred"))
            .collect();
        assert_eq!(entries.len(), 1);
    }

    // ===== Phase I: health_check =====

    use bosun_core::{HealthCheck, HealthCheckError, HealthCheckRunner};

    /// Mock health-check runner: записывает количество вызовов и
    /// возвращает заданный результат.
    struct MockHealthCheck {
        calls: Mutex<u32>,
        response: Mutex<Result<(), HealthCheckError>>,
    }

    impl MockHealthCheck {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(0),
                response: Mutex::new(Ok(())),
            })
        }
        fn failing(err: HealthCheckError) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(0),
                response: Mutex::new(Err(err)),
            })
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl HealthCheckRunner for MockHealthCheck {
        fn run(
            &self,
            _check: &HealthCheck,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<(), HealthCheckError> {
            *self.calls.lock().unwrap() += 1;
            std::mem::replace(&mut *self.response.lock().unwrap(), Ok(()))
        }
    }

    fn make_ctx_with_hc(
        systemd: Option<Arc<dyn SystemdHandle>>,
        health_check: Arc<dyn HealthCheckRunner>,
    ) -> (TempDir, ApplyCtx) {
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::with_runners(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            defers,
            None,
            systemd,
            Arc::new(bosun_core::RealValidateRunner),
            health_check,
        );
        (tmp, ctx)
    }

    fn make_resource_with_hc(
        name: &str,
        state: ServiceState,
        enable: bool,
        hc: HealthCheck,
    ) -> Resource {
        let kind = ResourceKind::from_static("systemd.service");
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
                "enable": enable,
                "health_check": hc,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn health_check_runs_after_sync_start_and_passes() {
        let mock = Arc::new(MockSystemd::new());
        // Pre: NoSuchUnit → trace as not-active, выбираем Start.
        mock.enqueue_unit_info(Err(SystemdError::NoSuchUnit("nginx.service".into())));
        // After: active, новый invocation_id.
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "fresh")));
        let hc = MockHealthCheck::ok();
        let r = make_resource_with_hc(
            "nginx.service",
            ServiceState::Running,
            false,
            HealthCheck::Url {
                url: "http://localhost/h".to_string(),
                expected_status: Some(200),
                timeout_sec: Some(1),
                retry_count: Some(1),
                retry_interval_sec: Some(0),
            },
        );
        let (_tmp, ctx) =
            make_ctx_with_hc(Some(mock.clone()), hc.clone() as Arc<dyn HealthCheckRunner>);
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(hc.calls(), 1);
    }

    #[test]
    fn health_check_failure_returns_health_check_failed_after_sync_start() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Err(SystemdError::NoSuchUnit("nginx.service".into())));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "fresh")));
        let hc = MockHealthCheck::failing(HealthCheckError::UrlBadStatus {
            url: "http://localhost/h".to_string(),
            actual: 500,
            expected: 200,
            attempts: 3,
        });
        let r = make_resource_with_hc(
            "nginx.service",
            ServiceState::Running,
            false,
            HealthCheck::Url {
                url: "http://localhost/h".to_string(),
                expected_status: Some(200),
                timeout_sec: Some(1),
                retry_count: Some(3),
                retry_interval_sec: Some(0),
            },
        );
        let (_tmp, ctx) =
            make_ctx_with_hc(Some(mock.clone()), hc.clone() as Arc<dyn HealthCheckRunner>);
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        match err {
            PrimitiveError::HealthCheckFailed { target, .. } => {
                assert_eq!(target, "nginx.service");
            }
            other => panic!("expected HealthCheckFailed, got {other:?}"),
        }
    }

    #[test]
    fn health_check_cancelled_maps_to_primitive_cancelled() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Err(SystemdError::NoSuchUnit("nginx.service".into())));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "fresh")));
        let hc = MockHealthCheck::failing(HealthCheckError::Cancelled);
        let r = make_resource_with_hc(
            "nginx.service",
            ServiceState::Running,
            false,
            HealthCheck::Cmd {
                cmd: vec!["true".to_string()],
                timeout_sec: None,
                retry_count: None,
                retry_interval_sec: None,
            },
        );
        let (_tmp, ctx) =
            make_ctx_with_hc(Some(mock.clone()), hc.clone() as Arc<dyn HealthCheckRunner>);
        let err = run(&r, &force_update(&r), &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
    }

    #[test]
    fn health_check_not_called_when_no_spec() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Err(SystemdError::NoSuchUnit("nginx.service".into())));
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "fresh")));
        let hc = MockHealthCheck::ok();
        let r = make_resource("nginx.service", ServiceState::Running, false);
        let (_tmp, ctx) =
            make_ctx_with_hc(Some(mock.clone()), hc.clone() as Arc<dyn HealthCheckRunner>);
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(hc.calls(), 0);
    }

    #[test]
    fn health_check_not_called_for_stop_action() {
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let hc = MockHealthCheck::ok();
        let r = make_resource_with_hc(
            "nginx.service",
            ServiceState::Stopped,
            false,
            HealthCheck::Cmd {
                cmd: vec!["true".to_string()],
                timeout_sec: None,
                retry_count: None,
                retry_interval_sec: None,
            },
        );
        let (_tmp, ctx) =
            make_ctx_with_hc(Some(mock.clone()), hc.clone() as Arc<dyn HealthCheckRunner>);
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(hc.calls(), 0);
    }

    #[test]
    fn health_check_not_called_for_deferred_restart() {
        // Restart-triggered → defer enqueue, health-check НЕ запускается
        // в apply'е (это делает replay-цикл с replay_with_health_check).
        let mock = Arc::new(MockSystemd::new());
        mock.enqueue_unit_info(Ok(make_unit("nginx", "active", "abc")));
        let hc = MockHealthCheck::ok();
        let r = {
            let mut r = make_resource_with_hc(
                "nginx.service",
                ServiceState::Running,
                false,
                HealthCheck::Cmd {
                    cmd: vec!["true".to_string()],
                    timeout_sec: None,
                    retry_count: None,
                    retry_interval_sec: None,
                },
            );
            let src_kind = ResourceKind::from_static("file.content");
            r.restart_on
                .push(ResourceId::new(&src_kind, "/etc/nginx.conf"));
            r
        };
        let (_tmp, ctx) =
            make_ctx_with_hc(Some(mock.clone()), hc.clone() as Arc<dyn HealthCheckRunner>);
        ctx.record_changed(&r.restart_on[0]);
        let report = run(&r, &force_update(&r), &ctx).unwrap();
        assert!(report.deferred);
        assert_eq!(hc.calls(), 0);
    }
}
