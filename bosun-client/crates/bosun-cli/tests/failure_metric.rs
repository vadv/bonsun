//! Integration-тесты: на каждом early-failure пути `bosun apply` обязан
//! записать снимок метрики с ненулевым `exit_code`. Иначе textfile-collector
//! сообщит alerting'у устаревшее «успех» от прошлого прогона.
//!
//! Тесты запускают реальный собранный бинарь (`CARGO_BIN_EXE_bosun`) с
//! заведомо невалидным input'ом и проверяют:
//! 1. exit-code не SUCCESS,
//! 2. `metric-file` существует,
//! 3. в нём есть `bosun_last_run_exit_code <ненулевое>`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

fn bosun() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bosun"))
}

/// Подготовить временное рабочее окружение и вернуть путь metric-файла,
/// который тест будет проверять после apply.
struct CliEnv {
    _tmp: TempDir,
    metric_file: PathBuf,
    state_dir: PathBuf,
    log_dir: PathBuf,
    backup_dir: PathBuf,
    lock_path: PathBuf,
    defers_dir: PathBuf,
}

fn cli_env() -> CliEnv {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().to_path_buf();
    CliEnv {
        metric_file: root.join("bosun.prom"),
        state_dir: root.join("state"),
        log_dir: root.join("log"),
        backup_dir: root.join("backup"),
        lock_path: root.join("bosun.lock"),
        defers_dir: root.join("defers"),
        _tmp: tmp,
    }
}

fn read_metric(env: &CliEnv) -> String {
    std::fs::read_to_string(&env.metric_file).unwrap_or_else(|e| {
        panic!(
            "metric file {} not written or unreadable: {e}",
            env.metric_file.display()
        )
    })
}

fn run_apply(env: &CliEnv, bundle: &std::path::Path) -> std::process::Output {
    bosun()
        .args([
            "apply",
            "--bundle",
            &bundle.to_string_lossy(),
            "--lock-path",
            &env.lock_path.to_string_lossy(),
            "--state-dir",
            &env.state_dir.to_string_lossy(),
            "--log-dir",
            &env.log_dir.to_string_lossy(),
            "--backup-dir",
            &env.backup_dir.to_string_lossy(),
            "--metric-file",
            &env.metric_file.to_string_lossy(),
            "--defers-dir",
            &env.defers_dir.to_string_lossy(),
            "--no-color",
        ])
        .output()
        .expect("bosun binary runs")
}

/// Bundle.toml есть, но требует несовместимую версию bosun. Это срабатывает
/// после bundle-load и проваливает check_compatibility — early-return №2.
#[test]
fn failure_metric_written_when_bundle_version_incompatible() {
    let env = cli_env();
    let bundle_dir = env._tmp.path().join("bundle-version-mismatch");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    std::fs::write(
        bundle_dir.join("bundle.toml"),
        r#"
[bundle]
name = "incompatible"
version = "0.1.0"
requires_bosun = "999.999.999"
entry = "main.star"
"#,
    )
    .unwrap();
    std::fs::write(bundle_dir.join("main.star"), "# empty\n").unwrap();

    let out = run_apply(&env, &bundle_dir);
    assert!(
        !out.status.success(),
        "expected non-success exit; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let metric = read_metric(&env);
    assert!(
        metric.contains("bosun_last_run_exit_code"),
        "metric file missing exit_code series:\n{metric}",
    );
    // Конкретно — `bosun_last_run_exit_code` отлично от 0.
    let line = metric
        .lines()
        .find(|l| l.starts_with("bosun_last_run_exit_code "))
        .unwrap_or_else(|| panic!("no exit_code line in metric:\n{metric}"));
    let value: i32 = line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("can't parse exit_code from line: {line}"));
    assert_ne!(value, 0, "expected failure exit_code, got 0 in:\n{metric}");
}

/// bundle.toml отсутствует → Bundle::load_dir вернёт Err — early-return №1.
#[test]
fn failure_metric_written_when_bundle_load_fails() {
    let env = cli_env();
    let bundle_dir = env._tmp.path().join("bundle-no-toml");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    // Никаких файлов внутри — load_dir должен упасть на отсутствии bundle.toml.

    let out = run_apply(&env, &bundle_dir);
    assert!(
        !out.status.success(),
        "expected non-success exit; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let metric = read_metric(&env);
    let line = metric
        .lines()
        .find(|l| l.starts_with("bosun_last_run_exit_code "))
        .unwrap_or_else(|| panic!("no exit_code line in metric:\n{metric}"));
    let value: i32 = line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("can't parse exit_code from line: {line}"));
    assert_ne!(value, 0, "expected failure exit_code, got 0 in:\n{metric}");
}

/// Bundle валидный, но манифест бросает eval-ошибку (явный `fail()`) —
/// early-return №4 (manifest evaluation failed).
#[test]
fn failure_metric_written_when_manifest_evaluation_fails() {
    let env = cli_env();
    let bundle_dir = env._tmp.path().join("bundle-eval-error");
    std::fs::create_dir_all(&bundle_dir).unwrap();
    std::fs::write(
        bundle_dir.join("bundle.toml"),
        format!(
            r#"
[bundle]
name = "eval-error"
version = "0.1.0"
requires_bosun = "{ver}"
entry = "main.star"
"#,
            ver = env!("CARGO_PKG_VERSION"),
        ),
    )
    .unwrap();
    // `fail("...")` — starlark builtin для умышленного провала eval.
    std::fs::write(
        bundle_dir.join("main.star"),
        "fail(\"intentional eval failure for test\")\n",
    )
    .unwrap();

    let out = run_apply(&env, &bundle_dir);
    assert!(
        !out.status.success(),
        "expected non-success exit; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let metric = read_metric(&env);
    let line = metric
        .lines()
        .find(|l| l.starts_with("bosun_last_run_exit_code "))
        .unwrap_or_else(|| panic!("no exit_code line in metric:\n{metric}"));
    let value: i32 = line
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("can't parse exit_code from line: {line}"));
    assert_ne!(value, 0, "expected failure exit_code, got 0 in:\n{metric}");
}
