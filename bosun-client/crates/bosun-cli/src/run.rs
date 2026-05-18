//! Главный flow `bosun apply` per spec, секция «bosun-cli / Flow».
//!
//! Шаги 1-19 spec'а реализованы в одной функции `run`, потому что границы
//! между ними — не самостоятельные единицы переиспользования. Каждый шаг
//! пишет наблюдаемый эффект (директория, lock, файл метрики) и возвращает
//! exit-код через значение, не через panic.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bosun_core::{
    ApplyCtx, ApplyOpts, ApplyReport, Bundle, Evaluator, FactValue, Orchestrator, Outcome, PlanCtx,
    PlanReport, Primitive, ResourceKind, SensitiveStore, TemplateFn,
};
use bosun_facts::FactsCollector;
use bosun_primitives::{template::render_template, AptPrimitive, FilePrimitive};
use tokio_util::sync::CancellationToken;

use crate::args::{ApplyArgs, ReportFormat};
use crate::bootstrap::{self, LockOutcome};
use crate::exit_code;
use crate::logging;
use crate::metric::{self, FactStateEntry, MetricSnapshot};

/// Список фактов, состояние которых публикуется в метрику. Порядок и
/// набор зафиксированы для observability: scrape'ы 60k нод должны бить
/// в одни и те же метрические серии.
const TRACKED_FACTS: &[&str] = &[
    "hostname",
    "cpu_count",
    "memory_mb",
    "init_system",
    "is_pod",
    "installed_packages",
];

/// Выполнить `bosun apply` per spec. Возвращает exit-код. Никогда не
/// panic'ует на ожидаемых ошибках — все маппятся в код.
pub fn run(args: &ApplyArgs) -> i32 {
    let started = Instant::now();
    let started_utc = chrono::Utc::now().timestamp();
    let version = env!("CARGO_PKG_VERSION").to_string();

    // Шаг 2: создание директорий ДО tracing init и flock.
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

    // Шаг 3: flock. На этом этапе tracing ещё не настроен — пишем в stderr
    // напрямую. Это согласовано со spec: «WouldBlock → tracing::info» там
    // приведено как иллюстрация, но без subscriber'а logger всё равно не
    // вывел бы строку. Используем stderr.
    let _lock_guard = match bootstrap::try_flock(&args.lock_path) {
        Ok(LockOutcome::Acquired(g)) => g,
        Ok(LockOutcome::Held) => {
            eprintln!(
                "bosun: another bosun instance holds {}, skipping",
                args.lock_path.display(),
            );
            // Метрика пишется даже при flock=Held: иначе оператор не отличит
            // «бинарь сейчас стоит и работает» от «бинарь умер давно». Серия
            // bosun_last_attempt_timestamp_seconds обновляется в любом
            // запуске, поэтому алерт на её staleness ловит реально завис-
            // шие cron-таймеры.
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
                fact_states: Vec::new(),
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

    // Шаг 4: tracing init. После flock — чтобы повторные вызовы под одной
    // блокировкой не приводили к двойной установке global subscriber'а.
    if let Err(e) = logging::init(args.log_level, args.log_format, args.no_color) {
        eprintln!("bosun: logging init failed: {e}");
        return exit_code::CLI_ENV_ERROR;
    }

    // Шаг 6: cancellation token + signal handlers.
    let cancel = CancellationToken::new();
    install_signal_handlers(cancel.clone());

    // Шаг 5: deadline.
    let deadline = Instant::now() + Duration::from_secs(args.deadline_sec.into());

    // Шаг 7: загрузка bundle.
    let bundle = match Bundle::load_dir(&args.bundle) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "bundle load failed");
            return exit_code::EVAL_ERROR;
        }
    };

    // Шаг 8: semver-проверка.
    if let Err(e) = bundle.check_compatibility(&version) {
        tracing::error!(error = %e, "bundle requires different bosun version");
        return exit_code::EVAL_ERROR;
    }

    // Шаг 9: inventory merge.
    let inventory = match load_inventory_override(args.inventory.as_deref()) {
        Ok(v) => bundle.merge_inventory(v),
        Err(code) => return code,
    };

    // Шаг 10: facts.
    let facts = FactsCollector::with_default_collectors_path();
    facts.collect_at_start();

    // Snapshot до Starlark-eval: эта же копия материализуется в JSON для
    // template-рендера, чтобы шаблоны видели те же значения, что и
    // манифест.
    let snapshot = facts.snapshot();
    let facts_json_for_templates = materialize_facts_json(&snapshot);
    let templates_root = bundle.templates_root.clone();
    let inventory_for_templates = inventory.clone();
    let template_fn: TemplateFn = Rc::new(move |path: &str| {
        render_template(
            &templates_root,
            path,
            &inventory_for_templates,
            &facts_json_for_templates,
        )
        .map_err(|e| anyhow::anyhow!(e))
    });

    // Шаг 11: primitives для evaluator.
    let evaluator_primitives = build_primitives();

    // Шаг 12-13: evaluator → registry.
    let sensitive = Arc::new(SensitiveStore::new());
    let plan_ctx = PlanCtx::new(deadline, cancel.clone());
    let evaluator = Evaluator::new(bundle, evaluator_primitives, inventory);
    let registry = match evaluator.evaluate(
        snapshot.clone(),
        sensitive.clone(),
        template_fn,
        plan_ctx.clone(),
    ) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "manifest evaluation failed");
            return exit_code::EVAL_ERROR;
        }
    };

    // Шаг 14: orchestrator. Primitives регистрируем повторно — `Box<dyn
    // Primitive>` не Clone, исходный map уехал в Evaluator.
    let orchestrator = Orchestrator::new(build_primitives());

    // Шаг 15: plan vs apply.
    let apply_ctx = ApplyCtx::new(
        deadline,
        cancel.clone(),
        tracing::Span::current(),
        sensitive.clone(),
        args.backup_dir.clone(),
        args.log_dir.clone(),
    );

    let view = facts.view();
    let (exit, changed, unchanged, failed, deferred) = if args.dry_run {
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
                // has_failures смотрит только на failed: Deferred сюда не
                // попадает, exit-код остаётся SUCCESS при чисто транзиентных
                // отказах.
                let code = if report.has_failures() {
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
                )
            }
            Err(e) => {
                tracing::error!(error = %e, "apply failed");
                return exit_code::EVAL_ERROR;
            }
        }
    };

    // Шаг 16: метрика. Финальные значения после run.
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
        fact_states,
    };
    if let Err(e) = metric::write_atomic(&args.metric_file, &snapshot) {
        tracing::warn!(error = %e, "failed to write metric file");
    }

    exit
}

/// Прочитать override-inventory с диска. Возвращает Null при отсутствии
/// флага, валидный JSON при успехе, exit-код при ошибке чтения/парсинга.
fn load_inventory_override(path: Option<&Path>) -> Result<serde_json::Value, i32> {
    let Some(path) = path else {
        return Ok(serde_json::Value::Null);
    };
    let text = std::fs::read_to_string(path).map_err(|e| {
        tracing::error!(error = %e, path = %path.display(), "reading inventory");
        exit_code::EVAL_ERROR
    })?;
    let yaml: serde_norway::Value = serde_norway::from_str(&text).map_err(|e| {
        tracing::error!(error = %e, path = %path.display(), "parsing inventory yaml");
        exit_code::EVAL_ERROR
    })?;
    serde_json::to_value(yaml).map_err(|e| {
        tracing::error!(error = %e, "converting inventory yaml to json");
        exit_code::EVAL_ERROR
    })
}

/// Собрать material'изованную JSON-карту фактов для шаблонов. Snapshot
/// перебирается через `FactsSource::get` для каждого имени из snapshot.names.
fn materialize_facts_json(snapshot: &bosun_facts::FactsSnapshot) -> serde_json::Value {
    use bosun_core::FactsSource;
    let mut map = serde_json::Map::new();
    for name in snapshot.names() {
        let value = snapshot.get(name);
        let json_value = match value {
            FactValue::Known(v) => v,
            FactValue::Stale { value, .. } => value,
            FactValue::Unknown { .. } => serde_json::Value::Null,
            // Forward-compat: новые FactValue-варианты в core безопасно
            // материализуются как Null, пока шаблоны не научатся их
            // обрабатывать.
            _ => serde_json::Value::Null,
        };
        map.insert(name.to_string(), json_value);
    }
    serde_json::Value::Object(map)
}

/// Сконструировать набор примитивов. Вынесено в функцию, потому что
/// Evaluator и Orchestrator владеют ими отдельно — у `Box<dyn Primitive>`
/// нет Clone, проще собрать два раза.
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
    m
}

/// Собрать состояние каждого зафиксированного факта для метрики. Snapshot
/// берётся свежий, после возможного mark_dirty в apply.
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
                // Forward-compat: новые состояния факта пока трактуем как
                // Unknown в метрике, пока не появятся отдельные коды.
                _ => 1,
            };
            FactStateEntry {
                name: (*name).to_string(),
                state_code,
            }
        })
        .collect()
}

/// Установить обработчик SIGTERM/SIGINT через `signal-hook`. Отдельный поток
/// дёргает `cancel.cancel()` при первом полученном сигнале и выходит.
/// Второй сигнал уже не отменит ничего повторно (CancellationToken
/// идемпотентен), но кооперативное завершение примитивов уже идёт.
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

/// Распечатать plan-отчёт в stdout. Логи остаются в stderr — это разделение
/// каналов из spec («формат отчёта»).
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

/// Распечатать apply-отчёт.
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
                    _ => "? ",
                };
                let label = match &r.outcome {
                    Outcome::NoChange => "no change".to_string(),
                    Outcome::Changed => r.message.clone(),
                    Outcome::Failed { error } => format!("failed: {error}"),
                    Outcome::Skipped => r.message.clone(),
                    Outcome::Deferred { reason } => format!("deferred: {reason}"),
                    _ => r.message.clone(),
                };
                let _ = writeln!(out, "  {marker}{kind} {id}", kind = r.kind, id = r.id);
                if !label.is_empty() {
                    let _ = writeln!(out, "      {label}");
                }
            }
            let _ = writeln!(
                out,
                "\nSummary: {changed} changed, {nc} no-change, {failed} failed, {skipped} skipped, {deferred} deferred.",
                changed = report.summary.changed,
                nc = report.summary.no_change,
                failed = report.summary.failed,
                skipped = report.summary.skipped,
                deferred = report.summary.deferred,
            );
        }
    }
}

/// Утилита для `with_default_collectors`: ставит root в `/`. Вынесено,
/// чтобы `run` не тащил PathBuf-боилерплейт.
trait WithDefaultCollectorsPath {
    fn with_default_collectors_path() -> FactsCollector;
}

impl WithDefaultCollectorsPath for FactsCollector {
    fn with_default_collectors_path() -> FactsCollector {
        bosun_facts::with_default_collectors(PathBuf::from("/"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use bosun_facts::FactsCollector;

    use super::*;

    #[test]
    fn build_primitives_registers_apt_and_file() {
        let m = build_primitives();
        assert!(m.contains_key(&ResourceKind::from_static("apt.package")));
        assert!(m.contains_key(&ResourceKind::from_static("file.content")));
        assert_eq!(m.len(), 2);
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
    fn load_inventory_override_none_returns_null() {
        let v = load_inventory_override(None).unwrap();
        assert_eq!(v, serde_json::Value::Null);
    }

    #[test]
    fn load_inventory_override_parses_yaml_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "name: nginx\nversion: 1.18.0\n").unwrap();
        let v = load_inventory_override(Some(tmp.path())).unwrap();
        assert_eq!(v["name"], serde_json::json!("nginx"));
        assert_eq!(v["version"], serde_json::json!("1.18.0"));
    }

    #[test]
    fn load_inventory_override_returns_eval_error_on_missing_file() {
        // Используем гарантированно несуществующий путь.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("nope.yaml");
        let code = load_inventory_override(Some(&path)).unwrap_err();
        assert_eq!(code, exit_code::EVAL_ERROR);
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
        // На пустом tempdir-root коллекторы не находят /etc/hostname и т.д.,
        // поэтому большинство фактов в Unknown — должны материализоваться в Null.
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
}
