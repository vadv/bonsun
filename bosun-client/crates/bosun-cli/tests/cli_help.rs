//! Integration-тесты на парсинг CLI через реальный бинарь.

#![allow(clippy::expect_used)]

use std::process::Command;

fn bosun() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bosun"))
}

#[test]
fn apply_help_lists_all_documented_flags() {
    let output = bosun()
        .args(["apply", "--help"])
        .output()
        .expect("binary runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--bundle",
        "--tags",
        "--dry-run",
        "--continue-on-error",
        "--log-level",
        "--log-format",
        "--format",
        "--no-color",
        "--lock-path",
        "--deadline-sec",
        "--state-dir",
        "--log-dir",
        "--backup-dir",
        "--metric-file",
    ] {
        assert!(
            stdout.contains(flag),
            "apply --help missing flag {flag}; got:\n{stdout}",
        );
    }
}

#[test]
fn apply_without_bundle_exits_with_clap_error() {
    let output = bosun().arg("apply").output().expect("binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bundle"),
        "expected --bundle in clap error, got:\n{stderr}",
    );
}

#[test]
fn version_subcommand_prints_pkg_version() {
    let output = bosun().arg("version").output().expect("binary runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
}
