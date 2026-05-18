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
}

#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    /// Path to the bundle directory.
    #[arg(long)]
    pub bundle: PathBuf,

    /// Path to a YAML inventory that overrides bundle defaults.
    #[arg(long)]
    pub inventory: Option<PathBuf>,

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
            "--inventory",
            "/i.yaml",
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
        assert_eq!(args.inventory, Some(PathBuf::from("/i.yaml")));
        assert_eq!(args.metric_file, PathBuf::from("/tmp/m.prom"));
    }

    #[test]
    fn invalid_log_level_rejected() {
        let err = Cli::try_parse_from(["bosun", "apply", "--bundle", "/b", "--log-level", "trace"])
            .unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("log-level") || s.contains("trace"));
    }
}
