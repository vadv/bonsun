//! Запись Prometheus textfile-метрики `bosun.prom`.
//!
//! Формат — стандартный textfile collector node_exporter'а. Запись атомарна:
//! сначала в `*.tmp` рядом с целевым файлом, потом `rename` поверх. Это
//! гарантирует, что node_exporter не прочитает частичный файл.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

/// Один снимок состояния факта для метрики.
#[derive(Debug, Clone)]
pub struct FactStateEntry {
    pub name: String,
    pub state_code: u8,
}

/// Phase J: счётчики replay-цикла defer'ов.
///
/// Все поля — суммарные значения по двум replay (`pre` + `post`), которые
/// CLI делает в одном прогоне `bosun apply`. Метрики счётчиков
/// (`*_total{...}`) пишутся как gauge, потому что node_exporter textfile
/// collector переписывает значение при каждом sync'е; ретенция
/// counter-семантики строится на стороне Prometheus через `increase()`.
#[derive(Debug, Clone, Default)]
pub struct DeferReplayStats {
    /// Сколько успешных dispatch'ей (entry удалён).
    pub executed_ok: u32,
    /// Сколько dispatch'ей упало с `Action(_)` (bump_attempt).
    pub executed_failed: u32,
    /// Сколько раз handle был недоступен (skip без bump'а).
    pub client_unavailable: u32,
    /// Сколько `.deferred` промоутнуты в `.manual_clear`.
    pub promoted_manual_clear: u32,
}

/// Все данные одного прогона, которые публикуются в Prometheus.
///
/// Разделение `attempted_at_utc` и `started_at_utc` нужно, чтобы оператор
/// мог отличить «бинарь даже не запустился» от «запустился, но flock не
/// взял»: `attempted_at_utc` пишется на каждую попытку, включая skipped
/// по flock=Held; `started_at_utc` — только при попадании в полный flow.
/// Алерт на staleness ставится на `attempted_at_utc`, иначе зависший
/// держатель lock'а маскируется свежей нагрузкой.
#[derive(Debug, Clone)]
pub struct MetricSnapshot {
    pub version: String,
    pub exit_code: i32,
    pub duration_sec: f64,
    pub started_at_utc: i64,
    pub attempted_at_utc: i64,
    pub resources_changed: usize,
    pub resources_unchanged: usize,
    pub resources_failed: usize,
    pub resources_deferred: usize,
    /// Прерванные ресурсы — отдельный outcome для SIGTERM/deadline.
    /// Алерт на «postgres-bosun застрял» проще ставить на эту метрику
    /// напрямую, чем угадывать «exit_code=130 и >0 ресурсов».
    pub resources_interrupted: usize,
    pub fact_states: Vec<FactStateEntry>,
    /// Phase J: количество `*.deferred` файлов в journal'е на момент конца
    /// прогона. Если apply прошёл и replay чистил всё подряд — 0; ненулевое
    /// значение значит «есть отложенные действия для следующего цикла».
    pub defers_pending: u32,
    /// Phase J: счётчики replay-цикла (агрегированные по pre+post).
    pub defers_replay_stats: DeferReplayStats,
    /// Phase J: сколько replay-вызовов было в этом прогоне. Production CLI
    /// делает ровно два: до и после evaluate. В тестах может быть 0 или 1.
    pub defers_replay_runs: u32,
    /// Phase J: handle'ы runr/systemd, которые удалось построить. `None`
    /// — клиент не требовался (init_system факт не предписывал его). 0/1
    /// — пытались поднять. Это даёт оператору ясный сигнал «runr нужен,
    /// но недоступен» через одиночную метрику без дополнительных
    /// label-фильтров.
    pub runr_reachable: Option<bool>,
    pub systemd_reachable: Option<bool>,
}

/// Сформировать содержимое .prom-файла. Чистая функция — её удобно тестировать.
pub fn format(snapshot: &MetricSnapshot) -> String {
    let mut out = String::new();

    out.push_str("# HELP bosun_last_attempt_timestamp_seconds UTC timestamp of last bosun invocation; alert on staleness here, not on bosun_last_run_timestamp_seconds\n");
    out.push_str("# TYPE bosun_last_attempt_timestamp_seconds gauge\n");
    out.push_str(&format!(
        "bosun_last_attempt_timestamp_seconds{{version=\"{version}\"}} {ts}\n",
        version = escape_label(&snapshot.version),
        ts = snapshot.attempted_at_utc,
    ));
    out.push('\n');

    out.push_str("# HELP bosun_last_run_timestamp_seconds UTC timestamp of last completed run\n");
    out.push_str("# TYPE bosun_last_run_timestamp_seconds gauge\n");
    out.push_str(&format!(
        "bosun_last_run_timestamp_seconds{{version=\"{version}\"}} {ts}\n",
        version = escape_label(&snapshot.version),
        ts = snapshot.started_at_utc,
    ));
    out.push('\n');

    out.push_str("# HELP bosun_last_run_exit_code Exit code of last run\n");
    out.push_str("# TYPE bosun_last_run_exit_code gauge\n");
    out.push_str(&format!(
        "bosun_last_run_exit_code {code}\n",
        code = snapshot.exit_code,
    ));
    out.push('\n');

    out.push_str("# HELP bosun_last_run_duration_seconds Duration of last run\n");
    out.push_str("# TYPE bosun_last_run_duration_seconds gauge\n");
    out.push_str(&format!(
        "bosun_last_run_duration_seconds {dur:.3}\n",
        dur = snapshot.duration_sec,
    ));
    out.push('\n');

    out.push_str("# HELP bosun_resources_total Total resources in last run by outcome\n");
    out.push_str("# TYPE bosun_resources_total gauge\n");
    out.push_str(&format!(
        "bosun_resources_total{{outcome=\"changed\"}} {}\n",
        snapshot.resources_changed,
    ));
    out.push_str(&format!(
        "bosun_resources_total{{outcome=\"unchanged\"}} {}\n",
        snapshot.resources_unchanged,
    ));
    out.push_str(&format!(
        "bosun_resources_total{{outcome=\"failed\"}} {}\n",
        snapshot.resources_failed,
    ));
    out.push_str(&format!(
        "bosun_resources_total{{outcome=\"deferred\"}} {}\n",
        snapshot.resources_deferred,
    ));
    out.push_str(&format!(
        "bosun_resources_total{{outcome=\"interrupted\"}} {}\n",
        snapshot.resources_interrupted,
    ));
    out.push('\n');

    out.push_str(
        "# HELP bosun_fact_state Last collected state of each fact (0=Known, 1=Unknown, 2=Stale)\n",
    );
    out.push_str("# TYPE bosun_fact_state gauge\n");
    for entry in &snapshot.fact_states {
        out.push_str(&format!(
            "bosun_fact_state{{fact=\"{fact}\"}} {state}\n",
            fact = escape_label(&entry.name),
            state = entry.state_code,
        ));
    }
    out.push('\n');

    // --- Phase J: defer / runr / systemd reachability ----------------------

    out.push_str("# HELP bosun_defers_pending Number of deferred-action files in the journal\n");
    out.push_str("# TYPE bosun_defers_pending gauge\n");
    out.push_str(&format!(
        "bosun_defers_pending {n}\n\n",
        n = snapshot.defers_pending,
    ));

    out.push_str(
        "# HELP bosun_defers_executed_total Total deferred actions handled during replay\n",
    );
    out.push_str("# TYPE bosun_defers_executed_total counter\n");
    out.push_str(&format!(
        "bosun_defers_executed_total{{result=\"ok\"}} {}\n",
        snapshot.defers_replay_stats.executed_ok,
    ));
    out.push_str(&format!(
        "bosun_defers_executed_total{{result=\"failed\"}} {}\n",
        snapshot.defers_replay_stats.executed_failed,
    ));
    out.push_str(&format!(
        "bosun_defers_executed_total{{result=\"client_unavailable\"}} {}\n",
        snapshot.defers_replay_stats.client_unavailable,
    ));
    out.push_str(&format!(
        "bosun_defers_executed_total{{result=\"manual_clear\"}} {}\n\n",
        snapshot.defers_replay_stats.promoted_manual_clear,
    ));

    out.push_str("# HELP bosun_defers_replay_total Number of replay invocations this run\n");
    out.push_str("# TYPE bosun_defers_replay_total counter\n");
    out.push_str(&format!(
        "bosun_defers_replay_total {n}\n\n",
        n = snapshot.defers_replay_runs,
    ));

    if let Some(reachable) = snapshot.runr_reachable {
        out.push_str("# HELP bosun_runr_reachable 1 if a runr handle was constructed this run, 0 otherwise\n");
        out.push_str("# TYPE bosun_runr_reachable gauge\n");
        let value = u8::from(reachable);
        out.push_str(&format!("bosun_runr_reachable {value}\n\n"));
    }
    if let Some(reachable) = snapshot.systemd_reachable {
        out.push_str("# HELP bosun_systemd_reachable 1 if a systemd handle was constructed this run, 0 otherwise\n");
        out.push_str("# TYPE bosun_systemd_reachable gauge\n");
        let value = u8::from(reachable);
        out.push_str(&format!("bosun_systemd_reachable {value}\n"));
    }

    out
}

/// Записать снимок в файл атомарно: temp в том же каталоге, потом rename.
/// Каталог должен существовать (CLI создаёт его через `ensure_dirs`).
pub fn write_atomic(path: &Path, snapshot: &MetricSnapshot) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "metric file path {} has no parent directory",
                path.display()
            ),
        )
    })?;
    std::fs::create_dir_all(parent)?;

    let body = format(snapshot);
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(body.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| std::io::Error::other(e.error))?;

    // На некоторых ФС rename не fsync'ит родителя — делаем это явно,
    // чтобы метрика гарантированно пережила kill -9.
    if let Ok(dir) = OpenOptions::new().read(true).open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Подсчитать `*.deferred` файлы в директории journal'а. Используется при
/// финальном format'е метрики. Возвращает 0 при I/O ошибке — метрика не
/// должна валить весь прогон, если journal неожиданно ушёл.
pub fn count_pending_defers(journal_dir: &std::path::Path) -> u32 {
    let Ok(dir) = std::fs::read_dir(journal_dir) else {
        return 0;
    };
    let mut n: u32 = 0;
    for ent in dir.flatten() {
        let path = ent.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) == Some("deferred") {
            n = n.saturating_add(1);
        }
    }
    n
}

/// Экранировать значение label по правилам Prometheus exposition format:
/// `\\`, `\n`, `\"`. Поскольку имена факта/версии — наша строгая зона
/// (alphanum + dot/underscore), это защита-on-belt-and-braces.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn sample() -> MetricSnapshot {
        MetricSnapshot {
            version: "0.1.0".to_string(),
            exit_code: 0,
            duration_sec: 12.34,
            started_at_utc: 1_747_567_200,
            attempted_at_utc: 1_747_567_210,
            resources_changed: 2,
            resources_unchanged: 47,
            resources_failed: 0,
            resources_deferred: 0,
            resources_interrupted: 0,
            fact_states: vec![
                FactStateEntry {
                    name: "hostname".to_string(),
                    state_code: 0,
                },
                FactStateEntry {
                    name: "cpu_count".to_string(),
                    state_code: 1,
                },
                FactStateEntry {
                    name: "installed_packages".to_string(),
                    state_code: 2,
                },
            ],
            defers_pending: 0,
            defers_replay_stats: DeferReplayStats::default(),
            defers_replay_runs: 0,
            runr_reachable: None,
            systemd_reachable: None,
        }
    }

    #[test]
    fn format_contains_all_metric_families() {
        let s = format(&sample());
        for needle in [
            "bosun_last_attempt_timestamp_seconds",
            "bosun_last_run_timestamp_seconds",
            "bosun_last_run_exit_code",
            "bosun_last_run_duration_seconds",
            "bosun_resources_total",
            "bosun_fact_state",
        ] {
            assert!(s.contains(needle), "missing metric family {needle}");
        }
    }

    #[test]
    fn format_emits_each_help_and_type_pair() {
        // Phase J: +3 family (`defers_pending`, `defers_executed_total`,
        // `defers_replay_total`); `runr_reachable`/`systemd_reachable`
        // условные (None в sample), их в дефолте нет.
        let s = format(&sample());
        assert_eq!(s.matches("# HELP ").count(), 9);
        assert_eq!(s.matches("# TYPE ").count(), 9);
    }

    #[test]
    fn format_includes_version_label_and_timestamp() {
        let s = format(&sample());
        assert!(s.contains("bosun_last_run_timestamp_seconds{version=\"0.1.0\"} 1747567200"));
    }

    #[test]
    fn format_emits_attempt_timestamp() {
        let s = format(&sample());
        assert!(
            s.contains("bosun_last_attempt_timestamp_seconds{version=\"0.1.0\"} 1747567210"),
            "expected attempt timestamp series; got:\n{s}"
        );
    }

    #[test]
    fn format_emits_exit_code_value() {
        let mut sn = sample();
        sn.exit_code = 1;
        let s = format(&sn);
        assert!(s.contains("\nbosun_last_run_exit_code 1\n"));
    }

    #[test]
    fn format_emits_duration_with_three_decimals() {
        let s = format(&sample());
        assert!(s.contains("bosun_last_run_duration_seconds 12.340"));
    }

    #[test]
    fn format_emits_all_resource_outcomes() {
        let mut sn = sample();
        sn.resources_deferred = 3;
        sn.resources_interrupted = 1;
        let s = format(&sn);
        assert!(s.contains("bosun_resources_total{outcome=\"changed\"} 2"));
        assert!(s.contains("bosun_resources_total{outcome=\"unchanged\"} 47"));
        assert!(s.contains("bosun_resources_total{outcome=\"failed\"} 0"));
        assert!(s.contains("bosun_resources_total{outcome=\"deferred\"} 3"));
        assert!(s.contains("bosun_resources_total{outcome=\"interrupted\"} 1"));
    }

    #[test]
    fn format_emits_each_fact_state() {
        let s = format(&sample());
        assert!(s.contains("bosun_fact_state{fact=\"hostname\"} 0"));
        assert!(s.contains("bosun_fact_state{fact=\"cpu_count\"} 1"));
        assert!(s.contains("bosun_fact_state{fact=\"installed_packages\"} 2"));
    }

    #[test]
    fn escape_label_quotes_and_backslashes() {
        assert_eq!(escape_label("simple"), "simple");
        assert_eq!(escape_label("a\"b"), "a\\\"b");
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("a\nb"), "a\\nb");
    }

    #[test]
    fn write_atomic_creates_file_with_expected_content() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bosun.prom");
        let sn = sample();
        write_atomic(&target, &sn).unwrap();
        let written = std::fs::read_to_string(&target).unwrap();
        assert_eq!(written, format(&sn));
    }

    #[test]
    fn write_atomic_creates_parent_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nested/path/bosun.prom");
        write_atomic(&target, &sample()).unwrap();
        assert!(target.is_file());
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bosun.prom");
        std::fs::write(&target, "stale data").unwrap();
        write_atomic(&target, &sample()).unwrap();
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(written.contains("bosun_last_run_exit_code"));
        assert!(!written.contains("stale data"));
    }

    #[test]
    fn format_emits_defers_pending_metric() {
        let mut sn = sample();
        sn.defers_pending = 5;
        let s = format(&sn);
        assert!(s.contains("bosun_defers_pending 5"));
    }

    #[test]
    fn format_emits_all_defers_executed_total_results() {
        let mut sn = sample();
        sn.defers_replay_stats = DeferReplayStats {
            executed_ok: 3,
            executed_failed: 1,
            client_unavailable: 2,
            promoted_manual_clear: 1,
        };
        let s = format(&sn);
        assert!(s.contains("bosun_defers_executed_total{result=\"ok\"} 3"));
        assert!(s.contains("bosun_defers_executed_total{result=\"failed\"} 1"));
        assert!(s.contains("bosun_defers_executed_total{result=\"client_unavailable\"} 2"));
        assert!(s.contains("bosun_defers_executed_total{result=\"manual_clear\"} 1"));
    }

    #[test]
    fn format_emits_defers_replay_total() {
        let mut sn = sample();
        sn.defers_replay_runs = 2;
        let s = format(&sn);
        assert!(s.contains("bosun_defers_replay_total 2"));
    }

    #[test]
    fn format_skips_reachability_metrics_when_none() {
        let s = format(&sample());
        assert!(!s.contains("bosun_runr_reachable"));
        assert!(!s.contains("bosun_systemd_reachable"));
    }

    #[test]
    fn format_emits_reachability_metrics_when_set() {
        let mut sn = sample();
        sn.runr_reachable = Some(true);
        sn.systemd_reachable = Some(false);
        let s = format(&sn);
        assert!(s.contains("bosun_runr_reachable 1"));
        assert!(s.contains("bosun_systemd_reachable 0"));
    }
}
