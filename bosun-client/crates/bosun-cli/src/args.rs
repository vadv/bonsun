//! CLI-аргументы и их парсинг через clap derive.
//!
//! Структура повторяет таблицу флагов из spec, секция «bosun-cli / Команды
//! и флаги». Дефолтные пути соответствуют production-ноде под root.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "bosun", version, about = "bosun-client agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print version and exit.
    Version,
    /// Apply a bundle to the local system.
    Apply(ApplyArgs),
    /// Bundle utilities (validate, ...).
    Bundle(BundleCli),
    /// Inspect or clear the deferred-actions journal.
    Status(StatusArgs),
}

#[derive(Debug, clap::Args)]
pub struct BundleCli {
    #[command(subcommand)]
    pub command: BundleSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum BundleSubcommand {
    /// Statically evaluate a bundle without touching the system.
    Validate(BundleValidateArgs),
}

#[derive(Debug, clap::Args)]
pub struct BundleValidateArgs {
    /// Path to the bundle directory.
    #[arg(long)]
    pub bundle: PathBuf,

    /// Comma-separated active tags.
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// Optional facts fixture (JSON).
    #[arg(long)]
    pub facts: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    /// Path to the bundle directory.
    #[arg(long)]
    pub bundle: PathBuf,

    /// Comma-separated active tags (e.g. `--tags=production,canary`). CLI
    /// dedups and sorts before passing to the evaluator.
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// Run plan only, do not modify the system.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Do not stop at the first resource error.
    #[arg(long, default_value_t = false)]
    pub continue_on_error: bool,

    #[arg(long, default_value_t = LogLevel::Info, value_enum)]
    pub log_level: LogLevel,

    #[arg(long, default_value_t = LogFormat::Text, value_enum)]
    pub log_format: LogFormat,

    /// Format of the apply/plan report printed to stdout.
    #[arg(long, default_value_t = ReportFormat::Text, value_enum)]
    pub format: ReportFormat,

    /// Disable ANSI colors in text reports.
    #[arg(long, default_value_t = false)]
    pub no_color: bool,

    /// Path to the advisory-lock file (`flock`).
    #[arg(long, default_value = "/var/run/bosun.lock")]
    pub lock_path: PathBuf,

    /// Global deadline for the whole run, in seconds.
    #[arg(long, default_value_t = 600)]
    pub deadline_sec: u32,

    #[arg(long, default_value = "/var/lib/bosun")]
    pub state_dir: PathBuf,

    #[arg(long, default_value = "/var/log/bosun")]
    pub log_dir: PathBuf,

    #[arg(long, default_value = "/var/backups/bosun")]
    pub backup_dir: PathBuf,

    /// Where to write the Prometheus textfile metric.
    #[arg(
        long,
        default_value = "/var/lib/node_exporter/textfile_collector/bosun.prom"
    )]
    pub metric_file: PathBuf,

    /// Базовый URL runr-демона. Подключение строится, только если
    /// `init_system` факт показывает `runr` или `mixed-systemd-runr`.
    #[arg(long, default_value = "http://127.0.0.1:8010")]
    pub runr_url: String,

    /// Таймаут одного HTTP-вызова runr.
    #[arg(long, default_value_t = 10)]
    pub runr_timeout_sec: u32,

    /// Корень journal'а defers (tmpfs by design).
    #[arg(long, default_value = "/tmp/bosun-defers")]
    pub defers_dir: PathBuf,

    /// Максимум попыток на одну defer-запись до промоушена в `.manual_clear`.
    /// Это глобальный CLI-дефолт, который используется при отсутствии
    /// явного значения в самом записи (Phase D-G делают snapshot
    /// max_attempts из записи; CLI-флаг — fallback).
    #[arg(long, default_value_t = 3)]
    pub defer_max_attempts: u32,

    /// Pacer: целевая длительность размазывания apply'я (секунды). `0`
    /// (дефолт) — pacer выключен, поведение идентично прежним фазам.
    /// При значении `> 0` orchestrator вставляет cancel-aware sleep между
    /// ресурсами; интервал — `target / N`, clamp'нутый к
    /// `[pacer-min-interval-ms, pacer-max-interval-ms]`.
    #[arg(long, default_value_t = 0)]
    pub pacer_target_sec: u32,

    /// Pacer: нижняя граница интервала между ресурсами (миллисекунды).
    /// Защищает от вырожденного случая «много ресурсов, sleep по
    /// микросекунде».
    #[arg(long, default_value_t = 60)]
    pub pacer_min_interval_ms: u32,

    /// Pacer: верхняя граница интервала между ресурсами (миллисекунды).
    /// Защищает от вырожденного случая «target большой, ресурсов мало».
    #[arg(long, default_value_t = 100)]
    pub pacer_max_interval_ms: u32,

    /// Override факта `init_system`. Принимает те же значения, что коллектор
    /// `InitSystemFact` (`systemd` / `runit` / `init` / `runr` /
    /// `mixed-systemd-runr` / `unknown`). Используется тестовой
    /// инфраструктурой, где `/proc/1/comm` контейнера не соответствует
    /// реальному набору демонов: BDD-сценарии под `runr` поднимают
    /// supervisor сами и не могут менять PID 1, поэтому фact приходится
    /// форсировать снаружи. На production-ноде флаг не нужен.
    #[arg(long = "init-system", value_name = "INIT_SYSTEM")]
    pub init_system_override: Option<String>,
}

/// Аргументы `bosun status` (Phase J). Команда без апплая показывает, что
/// лежит в journal'е defer'ов и умеет очищать ручные `.manual_clear`-файлы.
#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Корень journal'а defers (по умолчанию — тот же, что у `bosun apply`).
    #[arg(long, default_value = "/tmp/bosun-defers")]
    pub defers_dir: PathBuf,

    /// Формат вывода: `text` (таблица) или `json` (массив объектов).
    #[arg(long, default_value_t = StatusFormat::Text, value_enum)]
    pub format: StatusFormat,

    /// Удалить конкретный defer/manual_clear по id. Принимает либо
    /// канонический id (`systemd.restart:nginx.service`), либо полное имя
    /// файла. Сначала ищется среди `*.deferred`, потом — `*.manual_clear`.
    #[arg(long)]
    pub clear: Option<String>,

    /// Удалить все `*.manual_clear` файлы. Используется оператором после
    /// разбирательства с зависшими defer'ами.
    #[arg(long, default_value_t = false)]
    pub clear_all_manual: bool,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum LogFormat {
    Text,
    Json,
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            LogFormat::Text => "text",
            LogFormat::Json => "json",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ReportFormat {
    Text,
    Json,
}

impl std::fmt::Display for ReportFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ReportFormat::Text => "text",
            ReportFormat::Json => "json",
        };
        f.write_str(s)
    }
}

/// Формат вывода `bosun status`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum StatusFormat {
    Text,
    Json,
}

impl std::fmt::Display for StatusFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StatusFormat::Text => "text",
            StatusFormat::Json => "json",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn version_command_parses() {
        let cli = Cli::try_parse_from(["bosun", "version"]).unwrap();
        assert!(matches!(cli.command, Command::Version));
    }

    #[test]
    fn apply_requires_bundle() {
        let err = Cli::try_parse_from(["bosun", "apply"]).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("bundle"), "expected --bundle in error, got: {s}");
    }

    #[test]
    fn apply_with_bundle_only_uses_defaults() {
        let cli = Cli::try_parse_from(["bosun", "apply", "--bundle", "/srv/b"]).unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply subcommand")
        };
        assert_eq!(args.bundle, PathBuf::from("/srv/b"));
        assert!(!args.dry_run);
        assert!(!args.continue_on_error);
        assert_eq!(args.deadline_sec, 600);
        assert_eq!(args.state_dir, PathBuf::from("/var/lib/bosun"));
        assert_eq!(args.lock_path, PathBuf::from("/var/run/bosun.lock"));
        assert_eq!(
            args.metric_file,
            PathBuf::from("/var/lib/node_exporter/textfile_collector/bosun.prom"),
        );
    }

    #[test]
    fn apply_with_all_overrides() {
        let cli = Cli::try_parse_from([
            "bosun",
            "apply",
            "--bundle",
            "/b",
            "--tags",
            "production,canary",
            "--dry-run",
            "--continue-on-error",
            "--log-level",
            "debug",
            "--log-format",
            "json",
            "--format",
            "json",
            "--no-color",
            "--lock-path",
            "/tmp/x.lock",
            "--deadline-sec",
            "30",
            "--state-dir",
            "/tmp/state",
            "--log-dir",
            "/tmp/log",
            "--backup-dir",
            "/tmp/bk",
            "--metric-file",
            "/tmp/m.prom",
        ])
        .unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert!(args.dry_run);
        assert!(args.continue_on_error);
        assert!(matches!(args.log_level, LogLevel::Debug));
        assert!(matches!(args.log_format, LogFormat::Json));
        assert!(matches!(args.format, ReportFormat::Json));
        assert!(args.no_color);
        assert_eq!(args.deadline_sec, 30);
        assert_eq!(
            args.tags,
            vec!["production".to_string(), "canary".to_string()],
        );
        assert_eq!(args.metric_file, PathBuf::from("/tmp/m.prom"));
    }

    #[test]
    fn bundle_validate_parses_tags_csv() {
        let cli = Cli::try_parse_from([
            "bosun",
            "bundle",
            "validate",
            "--bundle",
            "/b",
            "--tags",
            "production,staging",
        ])
        .unwrap();
        let Command::Bundle(bundle_cli) = cli.command else {
            panic!("expected bundle subcommand")
        };
        let BundleSubcommand::Validate(args) = bundle_cli.command;
        assert_eq!(args.bundle, PathBuf::from("/b"));
        assert_eq!(
            args.tags,
            vec!["production".to_string(), "staging".to_string()]
        );
    }

    #[test]
    fn invalid_log_level_rejected() {
        let err = Cli::try_parse_from(["bosun", "apply", "--bundle", "/b", "--log-level", "trace"])
            .unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("log-level") || s.contains("trace"));
    }

    #[test]
    fn apply_defaults_for_runr_and_defers_match_spec() {
        // Phase J: новые флаги имеют production-defaults, описанные в плане.
        let cli = Cli::try_parse_from(["bosun", "apply", "--bundle", "/b"]).unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert_eq!(args.runr_url, "http://127.0.0.1:8010");
        assert_eq!(args.runr_timeout_sec, 10);
        assert_eq!(args.defers_dir, PathBuf::from("/tmp/bosun-defers"));
        assert_eq!(args.defer_max_attempts, 3);
    }

    #[test]
    fn apply_runr_and_defers_overrides_applied() {
        let cli = Cli::try_parse_from([
            "bosun",
            "apply",
            "--bundle",
            "/b",
            "--runr-url",
            "http://127.0.0.1:9999",
            "--runr-timeout-sec",
            "30",
            "--defers-dir",
            "/var/tmp/defers",
            "--defer-max-attempts",
            "5",
        ])
        .unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert_eq!(args.runr_url, "http://127.0.0.1:9999");
        assert_eq!(args.runr_timeout_sec, 30);
        assert_eq!(args.defers_dir, PathBuf::from("/var/tmp/defers"));
        assert_eq!(args.defer_max_attempts, 5);
    }

    #[test]
    fn apply_pacer_defaults_disabled() {
        // Phase S: дефолты соответствуют выключенному pacer'у (target=0).
        // Это backward-compat: `bosun apply` без новых флагов работает
        // идентично прежним фазам.
        let cli = Cli::try_parse_from(["bosun", "apply", "--bundle", "/b"]).unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert_eq!(args.pacer_target_sec, 0);
        assert_eq!(args.pacer_min_interval_ms, 60);
        assert_eq!(args.pacer_max_interval_ms, 100);
    }

    #[test]
    fn apply_pacer_overrides_applied() {
        let cli = Cli::try_parse_from([
            "bosun",
            "apply",
            "--bundle",
            "/b",
            "--pacer-target-sec",
            "30",
            "--pacer-min-interval-ms",
            "50",
            "--pacer-max-interval-ms",
            "120",
        ])
        .unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert_eq!(args.pacer_target_sec, 30);
        assert_eq!(args.pacer_min_interval_ms, 50);
        assert_eq!(args.pacer_max_interval_ms, 120);
    }

    #[test]
    fn apply_init_system_override_defaults_to_none() {
        // По умолчанию флаг не задан — bosun читает factual snapshot.
        let cli = Cli::try_parse_from(["bosun", "apply", "--bundle", "/b"]).unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert!(args.init_system_override.is_none());
    }

    #[test]
    fn apply_init_system_override_accepts_runr() {
        let cli =
            Cli::try_parse_from(["bosun", "apply", "--bundle", "/b", "--init-system", "runr"])
                .unwrap();
        let Command::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert_eq!(args.init_system_override.as_deref(), Some("runr"));
    }

    #[test]
    fn status_subcommand_parses_with_defaults() {
        let cli = Cli::try_parse_from(["bosun", "status"]).unwrap();
        let Command::Status(args) = cli.command else {
            panic!("expected status")
        };
        assert_eq!(args.defers_dir, PathBuf::from("/tmp/bosun-defers"));
        assert!(matches!(args.format, StatusFormat::Text));
        assert!(args.clear.is_none());
        assert!(!args.clear_all_manual);
    }

    #[test]
    fn status_subcommand_with_overrides() {
        let cli = Cli::try_parse_from([
            "bosun",
            "status",
            "--defers-dir",
            "/var/tmp/defers",
            "--format",
            "json",
            "--clear",
            "systemd.restart:nginx",
            "--clear-all-manual",
        ])
        .unwrap();
        let Command::Status(args) = cli.command else {
            panic!("expected status")
        };
        assert_eq!(args.defers_dir, PathBuf::from("/var/tmp/defers"));
        assert!(matches!(args.format, StatusFormat::Json));
        assert_eq!(args.clear.as_deref(), Some("systemd.restart:nginx"));
        assert!(args.clear_all_manual);
    }
}
