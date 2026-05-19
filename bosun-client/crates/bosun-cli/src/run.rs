//! Главный flow `bosun apply` per spec.
//!
//! Шаги повторяют MVP-flow, но:
//! - `--inventory` убран; inventory полностью внутри bundle и грузится
//!   через `inventory.load` в Starlark.
//! - `--tags=production,canary` добавлен; CLI дедуплицирует и сортирует.
//! - Активные тэги пишутся в Prometheus textfile (отдельный файл
//!   `bosun_tags.prom`) и логируются на старте через `tracing::info!`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bosun_core::{
    defers::{replay_with_health_check, Journal, ReplayReport},
    ApplyCtxBuilder, ApplyOpts, ApplyReport, Bundle, Evaluator, FactValue, HealthCheckRunner,
    Orchestrator, Outcome, PlanCtx, PlanReport, Primitive, ResourceKind, SensitiveStore,
    TemplateFn,
};
use bosun_facts::FactsCollector;
use bosun_handles::{RunrHandle, SystemdHandle};
use bosun_primitives::{
    dispatch::RealDispatchClient, template::render_template, AptPrimitive, FileDeletePrimitive,
    FilePrimitive, FileSymlinkPrimitive, GroupPrimitive, ProcessSignalPrimitive,
    RealHealthCheckRunner, RunrCgroupPrimitive, RunrServicePrimitive, RunrTimerPrimitive,
    SystemdServicePrimitive, SystemdTimerPrimitive, UserPrimitive,
};
use bosun_runr_client::Client as RunrClient;
use bosun_systemd_client::BlockingSystemdManager;
use tokio_util::sync::CancellationToken;

use crate::args::{ApplyArgs, ReportFormat};
use crate::bootstrap::{self, LockOutcome};
use crate::exit_code;
use crate::logging;
use crate::metric::{self, DeferReplayStats, FactStateEntry, MetricSnapshot};
use crate::tags_metric;

const TRACKED_FACTS: &[&str] = &[
    "hostname",
    "cpu_count",
    "memory_mb",
    "init_system",
    "is_pod",
    "installed_packages",
];

pub fn run(args: &ApplyArgs) -> i32 {
    let started = Instant::now();
    let started_utc = chrono::Utc::now().timestamp();
    let version = env!("CARGO_PKG_VERSION").to_string();

    let lock_parent = args
        .lock_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"));
    let dirs: Vec<&Path> = vec![
        args.state_dir.as_path(),
        args.log_dir.as_path(),
        args.backup_dir.as_path(),
        lock_parent.as_path(),
    ];
    if let Err(e) = bootstrap::ensure_dirs(&dirs) {
        eprintln!("bosun: bootstrap failed: {e}");
        eprintln!("bosun: hint: run as root or pre-create the directory");
        return exit_code::CLI_ENV_ERROR;
    }

    let _lock_guard = match bootstrap::try_flock(&args.lock_path) {
        Ok(LockOutcome::Acquired(g)) => g,
        Ok(LockOutcome::Held) => {
            eprintln!(
                "bosun: another bosun instance holds {}, skipping",
                args.lock_path.display(),
            );
            let snapshot = MetricSnapshot {
                version,
                exit_code: exit_code::SUCCESS,
                duration_sec: 0.0,
                started_at_utc: 0,
                attempted_at_utc: started_utc,
                resources_changed: 0,
                resources_unchanged: 0,
                resources_failed: 0,
                resources_deferred: 0,
                resources_interrupted: 0,
                fact_states: Vec::new(),
                defers_pending: 0,
                defers_replay_stats: DeferReplayStats::default(),
                defers_replay_runs: 0,
                runr_reachable: None,
                systemd_reachable: None,
            };
            if let Err(e) = metric::write_atomic(&args.metric_file, &snapshot) {
                eprintln!("bosun: failed to write skip metric file: {e}");
            }
            return exit_code::SUCCESS;
        }
        Err(e) => {
            eprintln!("bosun: flock failed: {e}");
            return exit_code::CLI_ENV_ERROR;
        }
    };

    if let Err(e) = logging::init(args.log_level, args.log_format, args.no_color) {
        eprintln!("bosun: logging init failed: {e}");
        return exit_code::CLI_ENV_ERROR;
    }

    // Активные тэги: dedup + sort до передачи в evaluator.
    let active_tags: BTreeSet<String> = args.tags.iter().cloned().collect();
    let tags_for_eval: HashSet<String> = active_tags.iter().cloned().collect();
    let tags_log: Vec<&str> = active_tags.iter().map(|s| s.as_str()).collect();
    tracing::info!(active_tags = ?tags_log, "bosun: active tags");
    if let Err(e) = tags_metric::write_atomic(&tags_metric_path(&args.metric_file), &active_tags) {
        tracing::warn!(error = %e, "failed to write tags metric file");
    }

    let cancel = CancellationToken::new();
    install_signal_handlers(cancel.clone());

    let deadline = Instant::now() + Duration::from_secs(args.deadline_sec.into());

    let bundle = match Bundle::load_dir(&args.bundle) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "bundle load failed");
            return exit_code::EVAL_ERROR;
        }
    };

    if let Err(e) = bundle.check_compatibility(&version) {
        tracing::error!(error = %e, "bundle requires different bosun version");
        return exit_code::EVAL_ERROR;
    }

    let facts = FactsCollector::with_default_collectors_path();
    facts.collect_at_start();

    let snapshot = facts.snapshot();
    let facts_json_for_templates = materialize_facts_json(&snapshot);
    let bundle_root_for_templates = bundle.root.clone();
    let template_fn: TemplateFn = Rc::new(
        move |resolved_path: &Path, _rel: &str, ctx: &serde_json::Value| {
            // resolved_path — канонический абсолют под bundle root. render_template
            // ожидает (templates_root, relative). Подкатегория «relative»
            // считается строго в пределах role/lib templates/-директории.
            let parent = resolved_path.parent().ok_or_else(|| {
                anyhow::anyhow!("template: resolved path has no parent: {resolved_path:?}")
            })?;
            let file_name = resolved_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("template: resolved path has no file name"))?
                .to_string_lossy()
                .into_owned();
            // `inv` для шаблона: берётся из kwargs `template(path, inv = ...)`,
            // либо целиком ctx (если ctx — объект). Жирная семантика: ctx
            // целиком кладётся как `inv`, если в нём нет ключа `inv`; иначе
            // используется ctx.inv. Это даёт совместимость с legacy template'ами
            // (`{{ inv.foo }}`) и новый стиль (`template(..., inv = ...)`).
            let inv_value = match ctx {
                serde_json::Value::Object(m) if m.contains_key("inv") => m["inv"].clone(),
                other => other.clone(),
            };
            let _ = &bundle_root_for_templates;
            render_template(parent, &file_name, &inv_value, &facts_json_for_templates)
                .map_err(|e| anyhow::anyhow!(e))
        },
    );

    // Phase J: defers журнал и handle'ы runr/systemd инициализируются ДО
    // evaluate manifest'а. Это нужно, чтобы pre-replay прошёл по journal'у
    // оставшемуся с прошлого прогона, до того как новый apply начнёт
    // что-то enqueue'ить (иначе можно стереть/субсумировать ещё не
    // выполненный restart полезной нагрузкой текущего цикла).
    let defers = match Journal::open(&args.defers_dir) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            tracing::error!(error = %e, "failed to open defer journal");
            return exit_code::CLI_ENV_ERROR;
        }
    };

    // Phase J: handle'ы runr/systemd инициализируются по init_system факту.
    // `systemd` → systemd dbus client; `runr` / `mixed-*` → runr HTTP client;
    // `unknown` или другие — оба None, replay будет пропускать соответствующие
    // entries как `client_unavailable`.
    let init_system = init_system_value(&snapshot);
    let needs_systemd = init_system_requires_systemd(init_system.as_deref());
    let needs_runr = init_system_requires_runr(init_system.as_deref());

    let mut runr_reachable: Option<bool> = None;
    let mut systemd_reachable: Option<bool> = None;

    let runr_handle: Option<Arc<dyn RunrHandle>> = if needs_runr {
        let client = RunrClient::new(
            args.runr_url.clone(),
            Duration::from_secs(args.runr_timeout_sec.into()),
        );
        runr_reachable = Some(true);
        Some(Arc::new(client) as Arc<dyn RunrHandle>)
    } else {
        None
    };

    let systemd_handle: Option<Arc<dyn SystemdHandle>> = if needs_systemd {
        match BlockingSystemdManager::connect_system() {
            Ok(m) => {
                systemd_reachable = Some(true);
                Some(Arc::new(m) as Arc<dyn SystemdHandle>)
            }
            Err(e) => {
                tracing::warn!(error = %e, "systemd dbus unavailable, falling back to defer-only");
                systemd_reachable = Some(false);
                None
            }
        }
    } else {
        None
    };

    // Phase I: production health_check runner (cmd + url).
    let health_check_runner: Arc<dyn HealthCheckRunner> = Arc::new(RealHealthCheckRunner::new());

    // Phase J: pre-replay по journal'у до evaluate. Так оператор
    // гарантированно получает успех на defer'ы, оставшиеся с прошлого
    // прогона, до того как evaluate/apply начнёт enqueue'ить новые.
    let dispatcher = RealDispatchClient::new(runr_handle.clone(), systemd_handle.clone());
    let mut replay_runs: u32 = 0;
    let mut replay_stats = DeferReplayStats::default();
    if let Some(report) = run_replay_phase(
        &defers,
        &dispatcher,
        health_check_runner.as_ref(),
        &cancel,
        "pre",
    ) {
        replay_runs += 1;
        accumulate_replay_stats(&mut replay_stats, &report);
    }

    let evaluator_primitives = build_primitives();

    let sensitive = Arc::new(SensitiveStore::new());
    let plan_ctx = PlanCtx::new(deadline, cancel.clone());
    let evaluator = Evaluator::new(bundle, evaluator_primitives);
    let registry = match evaluator.evaluate(
        snapshot.clone(),
        sensitive.clone(),
        template_fn,
        plan_ctx.clone(),
        tags_for_eval,
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "manifest evaluation failed");
            return exit_code::EVAL_ERROR;
        }
    };

    let orchestrator = Orchestrator::new(build_primitives());

    let mut builder = ApplyCtxBuilder::new(
        deadline,
        cancel.clone(),
        sensitive.clone(),
        args.backup_dir.clone(),
        args.log_dir.clone(),
        defers.clone(),
    )
    .log_span(tracing::Span::current())
    .health_check_runner(health_check_runner.clone());
    if let Some(h) = runr_handle.clone() {
        builder = builder.runr(h);
    }
    if let Some(h) = systemd_handle.clone() {
        builder = builder.systemd(h);
    }
    let apply_ctx = builder.build();

    let view = facts.view();
    let (exit, changed, unchanged, failed, deferred, interrupted) = if args.dry_run {
        match orchestrator.plan_only(&registry, &view, &plan_ctx) {
            Ok(report) => {
                print_plan(&report, args.format);
                let drift = report.has_drift();
                let code = if drift {
                    exit_code::DRY_RUN_DRIFT
                } else {
                    exit_code::SUCCESS
                };
                let changed_count = report.summary.add + report.summary.update;
                (
                    code,
                    changed_count,
                    report.summary.no_change,
                    report.errors.len(),
                    0_usize,
                    0_usize,
                )
            }
            Err(e) => {
                tracing::error!(error = %e, "plan failed");
                return exit_code::EVAL_ERROR;
            }
        }
    } else {
        let mark_dirty = |kind: &ResourceKind| {
            facts.mark_dirty_after_apply(kind);
        };
        let mut opts = ApplyOpts::default();
        opts.continue_on_error = args.continue_on_error;
        match orchestrator.apply(&registry, &view, &mark_dirty, &plan_ctx, &apply_ctx, opts) {
            Ok(report) => {
                print_apply(&report, args.format);
                // Приоритет кодов: Interrupted > PartialFailure > Success.
                // Прерванный прогон важнее частичного провала: оператор
                // должен понимать, что мы вообще не довели run до конца,
                // а не «что-то упало, остальное доделали».
                let code = if report.has_interruptions() {
                    exit_code::APPLY_INTERRUPTED
                } else if report.has_failures() {
                    exit_code::APPLY_PARTIAL_FAILURE
                } else {
                    exit_code::SUCCESS
                };
                (
                    code,
                    report.summary.changed,
                    report.summary.no_change,
                    report.summary.failed,
                    report.summary.deferred,
                    report.summary.interrupted,
                )
            }
            Err(e) => {
                tracing::error!(error = %e, "apply failed");
                return exit_code::EVAL_ERROR;
            }
        }
    };

    // Phase J: post-replay после evaluate+apply. Захватывает defer'ы,
    // которые сам apply enqueue'нул, если они теперь могут быть выполнены
    // (например, validate прошёл с предыдущего цикла, но сервис был
    // unavailable; теперь handle поднялся).
    if let Some(report) = run_replay_phase(
        &defers,
        &dispatcher,
        health_check_runner.as_ref(),
        &cancel,
        "post",
    ) {
        replay_runs += 1;
        accumulate_replay_stats(&mut replay_stats, &report);
    }

    let defers_pending = metric::count_pending_defers(defers.root());

    let duration = started.elapsed().as_secs_f64();
    let fact_states = build_fact_states(&facts);
    let snapshot = MetricSnapshot {
        version,
        exit_code: exit,
        duration_sec: duration,
        started_at_utc: started_utc,
        attempted_at_utc: started_utc,
        resources_changed: changed,
        resources_unchanged: unchanged,
        resources_failed: failed,
        resources_deferred: deferred,
        resources_interrupted: interrupted,
        fact_states,
        defers_pending,
        defers_replay_stats: replay_stats,
        defers_replay_runs: replay_runs,
        runr_reachable,
        systemd_reachable,
    };
    if let Err(e) = metric::write_atomic(&args.metric_file, &snapshot) {
        tracing::warn!(error = %e, "failed to write metric file");
    }

    exit
}

/// Положить `bosun_tags.prom` в ту же директорию, что и `bosun.prom`.
fn tags_metric_path(metric_file: &Path) -> PathBuf {
    let parent = metric_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    parent.join("bosun_tags.prom")
}

fn materialize_facts_json(snapshot: &bosun_facts::FactsSnapshot) -> serde_json::Value {
    use bosun_core::FactsSource;
    let mut map = serde_json::Map::new();
    for name in snapshot.names() {
        let value = snapshot.get(name);
        let json_value = match value {
            FactValue::Known(v) => v,
            FactValue::Stale { value, .. } => value,
            FactValue::Unknown { .. } => serde_json::Value::Null,
            _ => serde_json::Value::Null,
        };
        map.insert(name.to_string(), json_value);
    }
    serde_json::Value::Object(map)
}

fn build_primitives() -> HashMap<ResourceKind, Box<dyn Primitive>> {
    let mut m: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
    m.insert(
        ResourceKind::from_static("apt.package"),
        Box::new(AptPrimitive::new()),
    );
    m.insert(
        ResourceKind::from_static("file.content"),
        Box::new(FilePrimitive),
    );
    m.insert(
        ResourceKind::from_static("file.delete"),
        Box::new(FileDeletePrimitive),
    );
    m.insert(
        ResourceKind::from_static("file.symlink"),
        Box::new(FileSymlinkPrimitive),
    );
    m.insert(
        ResourceKind::from_static("process.signal"),
        Box::new(ProcessSignalPrimitive::with_real_runner()),
    );
    m.insert(
        ResourceKind::from_static("runr.service"),
        Box::new(RunrServicePrimitive),
    );
    m.insert(
        ResourceKind::from_static("runr.timer"),
        Box::new(RunrTimerPrimitive),
    );
    m.insert(
        ResourceKind::from_static("runr.cgroup"),
        Box::new(RunrCgroupPrimitive),
    );
    m.insert(
        ResourceKind::from_static("systemd.service"),
        Box::new(SystemdServicePrimitive),
    );
    m.insert(
        ResourceKind::from_static("systemd.timer"),
        Box::new(SystemdTimerPrimitive),
    );
    m.insert(
        ResourceKind::from_static("users.user"),
        Box::new(UserPrimitive::with_real_backend()),
    );
    m.insert(
        ResourceKind::from_static("users.group"),
        Box::new(GroupPrimitive::with_real_backend()),
    );
    m
}

fn build_fact_states(collector: &FactsCollector) -> Vec<FactStateEntry> {
    let snapshot = collector.snapshot();
    use bosun_core::FactsSource;
    TRACKED_FACTS
        .iter()
        .map(|name| {
            let state_code = match snapshot.get(name) {
                FactValue::Known(_) => 0,
                FactValue::Unknown { .. } => 1,
                FactValue::Stale { .. } => 2,
                _ => 1,
            };
            FactStateEntry {
                name: (*name).to_string(),
                state_code,
            }
        })
        .collect()
}

fn install_signal_handlers(cancel: CancellationToken) {
    let signals_result = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ]);
    let mut signals = match signals_result {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to install signal handlers");
            return;
        }
    };
    std::thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            tracing::info!(signal = sig, "received termination signal, cancelling");
            cancel.cancel();
        }
    });
}

fn print_plan(report: &PlanReport, format: ReportFormat) {
    let mut out = std::io::stdout().lock();
    match format {
        ReportFormat::Json => {
            if let Err(e) = serde_json::to_writer_pretty(&mut out, report) {
                tracing::warn!(error = %e, "writing JSON plan report");
            }
            let _ = writeln!(out);
        }
        ReportFormat::Text => {
            let _ = writeln!(out, "Plan:");
            for plan in &report.resources {
                let marker = match plan.diff {
                    bosun_core::Diff::NoChange => "  ",
                    bosun_core::Diff::Add { .. } => "+ ",
                    bosun_core::Diff::Update { .. } => "~ ",
                    _ => "? ",
                };
                let description = match &plan.diff {
                    bosun_core::Diff::NoChange => "(no change)".to_string(),
                    bosun_core::Diff::Add { description, .. } => description.clone(),
                    bosun_core::Diff::Update { description, .. } => description.clone(),
                    _ => "(unknown diff variant)".to_string(),
                };
                let _ = writeln!(out, "  {marker}{kind} {id}", kind = plan.kind, id = plan.id);
                let _ = writeln!(out, "      {description}");
            }
            for err in &report.errors {
                let _ = writeln!(
                    out,
                    "  ! {kind} {id} — {message}",
                    kind = err.kind,
                    id = err.id,
                    message = err.message,
                );
            }
            let _ = writeln!(
                out,
                "\nSummary: {add} add, {upd} update, {nc} no-change, {err} errors.",
                add = report.summary.add,
                upd = report.summary.update,
                nc = report.summary.no_change,
                err = report.errors.len(),
            );
        }
    }
}

fn print_apply(report: &ApplyReport, format: ReportFormat) {
    let mut out = std::io::stdout().lock();
    match format {
        ReportFormat::Json => {
            if let Err(e) = serde_json::to_writer_pretty(&mut out, report) {
                tracing::warn!(error = %e, "writing JSON apply report");
            }
            let _ = writeln!(out);
        }
        ReportFormat::Text => {
            let _ = writeln!(out, "Apply:");
            for r in &report.resources {
                let marker = match &r.outcome {
                    Outcome::NoChange => "  ",
                    Outcome::Changed => "+ ",
                    Outcome::Failed { .. } => "x ",
                    Outcome::Skipped => "- ",
                    Outcome::Deferred { .. } => "~ ",
                    Outcome::Interrupted { .. } => "! ",
                    _ => "? ",
                };
                let label = match &r.outcome {
                    Outcome::NoChange => "no change".to_string(),
                    Outcome::Changed => r.message.clone(),
                    Outcome::Failed { error } => format!("failed: {error}"),
                    Outcome::Skipped => r.message.clone(),
                    Outcome::Deferred { reason } => format!("deferred: {reason}"),
                    Outcome::Interrupted { reason } => format!("interrupted: {reason}"),
                    _ => r.message.clone(),
                };
                let _ = writeln!(out, "  {marker}{kind} {id}", kind = r.kind, id = r.id);
                if !label.is_empty() {
                    let _ = writeln!(out, "      {label}");
                }
            }
            let _ = writeln!(
                out,
                "\nSummary: {changed} changed, {nc} no-change, {failed} failed, {skipped} skipped, {deferred} deferred, {interrupted} interrupted.",
                changed = report.summary.changed,
                nc = report.summary.no_change,
                failed = report.summary.failed,
                skipped = report.summary.skipped,
                deferred = report.summary.deferred,
                interrupted = report.summary.interrupted,
            );
        }
    }
}

trait WithDefaultCollectorsPath {
    fn with_default_collectors_path() -> FactsCollector;
}

impl WithDefaultCollectorsPath for FactsCollector {
    fn with_default_collectors_path() -> FactsCollector {
        bosun_facts::with_default_collectors(PathBuf::from("/"))
    }
}

/// Достать строковое значение факта `init_system` из snapshot'а.
/// `None` означает `Unknown`/`Stale` — относим к unsupported.
fn init_system_value(snapshot: &bosun_facts::FactsSnapshot) -> Option<String> {
    use bosun_core::FactsSource;
    match snapshot.get("init_system") {
        FactValue::Known(serde_json::Value::String(s)) => Some(s),
        FactValue::Stale {
            value: serde_json::Value::String(s),
            ..
        } => Some(s),
        _ => None,
    }
}

/// Нужен ли в этом прогоне systemd dbus handle. Включает явный `systemd`
/// и смешанную конфигурацию `mixed-systemd-runr` (по spec'у Phase J).
fn init_system_requires_systemd(value: Option<&str>) -> bool {
    matches!(value, Some("systemd") | Some("mixed-systemd-runr"))
}

/// Нужен ли в этом прогоне runr HTTP handle. Включает явный `runr` и
/// `mixed-systemd-runr`.
fn init_system_requires_runr(value: Option<&str>) -> bool {
    matches!(value, Some("runr") | Some("mixed-systemd-runr"))
}

/// Прогон одной фазы replay. Возвращает `Some(report)` если list_sorted
/// прошёл, `None` если journal недоступен (тогда метрики не двигаем —
/// post-mortem из tracing-логов виднее).
fn run_replay_phase(
    journal: &Journal,
    dispatcher: &RealDispatchClient,
    health: &dyn HealthCheckRunner,
    cancel: &CancellationToken,
    phase: &'static str,
) -> Option<ReplayReport> {
    match replay_with_health_check(journal, dispatcher, health, cancel) {
        Ok(report) => {
            tracing::info!(
                phase = phase,
                executed = report.executed,
                failed = report.failed,
                skipped_unavailable = report.skipped_unavailable,
                manual_clear = report.promoted_to_manual_clear,
                health_check_failed = report.health_check_failed,
                "defers replay phase complete",
            );
            Some(report)
        }
        Err(e) => {
            tracing::warn!(phase = phase, error = %e, "defers replay phase failed");
            None
        }
    }
}

/// Аккумулирует ReplayReport в running totals для метрики.
fn accumulate_replay_stats(stats: &mut DeferReplayStats, report: &ReplayReport) {
    stats.executed_ok = stats.executed_ok.saturating_add(report.executed);
    stats.executed_failed = stats
        .executed_failed
        .saturating_add(report.failed)
        .saturating_add(report.health_check_failed);
    stats.client_unavailable = stats
        .client_unavailable
        .saturating_add(report.skipped_unavailable);
    stats.promoted_manual_clear = stats
        .promoted_manual_clear
        .saturating_add(report.promoted_to_manual_clear);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use bosun_facts::FactsCollector;

    use super::*;

    #[test]
    fn build_primitives_registers_all_primitives() {
        let m = build_primitives();
        for kind in [
            "apt.package",
            "file.content",
            "file.delete",
            "file.symlink",
            "process.signal",
            "runr.service",
            "runr.timer",
            "runr.cgroup",
            "systemd.service",
            "systemd.timer",
            "users.user",
            "users.group",
        ] {
            assert!(
                m.contains_key(&ResourceKind::from_static(kind)),
                "primitive {kind} not registered",
            );
        }
        assert_eq!(m.len(), 12);
    }

    #[test]
    fn build_fact_states_returns_one_entry_per_tracked_fact() {
        let collector = bosun_facts::with_default_collectors(tempfile::tempdir().unwrap().keep());
        let states = build_fact_states(&collector);
        assert_eq!(states.len(), TRACKED_FACTS.len());
        for (state, name) in states.iter().zip(TRACKED_FACTS.iter()) {
            assert_eq!(&state.name, name);
        }
    }

    #[test]
    fn tags_metric_path_uses_parent_of_metric_file() {
        let p = tags_metric_path(Path::new("/var/lib/foo/bosun.prom"));
        assert_eq!(p, PathBuf::from("/var/lib/foo/bosun_tags.prom"));
    }

    #[test]
    fn materialize_facts_json_handles_unknown_as_null() {
        let tmp = tempfile::tempdir().unwrap();
        let collector: FactsCollector =
            bosun_facts::with_default_collectors(tmp.path().to_path_buf());
        collector.collect_at_start();
        let snapshot = collector.snapshot();
        let json = materialize_facts_json(&snapshot);
        let obj = json.as_object().unwrap();
        assert!(!obj.is_empty());
        for (_name, value) in obj {
            assert!(
                value.is_null()
                    || value.is_string()
                    || value.is_number()
                    || value.is_object()
                    || value.is_boolean()
                    || value.is_array(),
                "fact value must be null/scalar/object after materialization, got {value:?}",
            );
        }
    }

    #[test]
    fn init_system_requires_systemd_matches_expected_values() {
        // Чистый systemd → нужен systemd handle.
        assert!(init_system_requires_systemd(Some("systemd")));
        // Смешанный — оба handle'а.
        assert!(init_system_requires_systemd(Some("mixed-systemd-runr")));
        // runr-only — нет.
        assert!(!init_system_requires_systemd(Some("runr")));
        // Unknown — нет.
        assert!(!init_system_requires_systemd(Some("init")));
        assert!(!init_system_requires_systemd(None));
    }

    #[test]
    fn init_system_requires_runr_matches_expected_values() {
        assert!(init_system_requires_runr(Some("runr")));
        assert!(init_system_requires_runr(Some("mixed-systemd-runr")));
        assert!(!init_system_requires_runr(Some("systemd")));
        assert!(!init_system_requires_runr(Some("init")));
        assert!(!init_system_requires_runr(None));
    }

    #[test]
    fn accumulate_replay_stats_sums_across_invocations() {
        let mut stats = DeferReplayStats::default();
        let report1 = ReplayReport {
            executed: 2,
            failed: 1,
            skipped_unavailable: 0,
            promoted_to_manual_clear: 0,
            health_check_failed: 0,
        };
        let report2 = ReplayReport {
            executed: 1,
            failed: 0,
            skipped_unavailable: 3,
            promoted_to_manual_clear: 1,
            health_check_failed: 2,
        };
        accumulate_replay_stats(&mut stats, &report1);
        accumulate_replay_stats(&mut stats, &report2);
        assert_eq!(stats.executed_ok, 3);
        // failed + health_check_failed считаются вместе.
        assert_eq!(stats.executed_failed, 3);
        assert_eq!(stats.client_unavailable, 3);
        assert_eq!(stats.promoted_manual_clear, 1);
    }

    #[test]
    fn init_system_value_returns_none_for_unknown_or_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let collector: FactsCollector =
            bosun_facts::with_default_collectors(tmp.path().to_path_buf());
        collector.collect_at_start();
        let snapshot = collector.snapshot();
        // На тестовом root'е без /proc/1/comm init_system должен быть
        // Unknown → init_system_value возвращает None.
        let v = init_system_value(&snapshot);
        assert_eq!(v, None);
    }
}
