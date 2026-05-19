//! Phase J: интеграционные тесты на `bosun status` через реальный бинарь.
//!
//! Тесты подкладывают defer-файлы в TempDir, дёргают subprocess'ом и
//! проверяют exit-коды и stdout. Это даёт нам ту самую гарантию, что
//! CLI-поверхность работает end-to-end, а не только в unit-тестах.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn bosun() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bosun"))
}

/// Создаёт `defers_dir` пустую и возвращает её PATH. Используется при
/// проверках no-pending пути.
fn make_empty_journal() -> TempDir {
    let tmp = TempDir::new().expect("tempdir created");
    // Journal::open сам создаёт root с mode 0o700, если его нет, но
    // tempdir уже даёт нам директорию. Здесь нам важно лишь, что путь
    // действительно существует.
    let _ = &tmp;
    tmp
}

/// Создаёт пустую директорию и подкладывает в неё указанные defer-файлы
/// напрямую. Содержимое — minimally валидный DeferEntry JSON.
fn make_journal_with(entries: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().expect("tempdir created");
    // mode 0o700, чтобы Journal::open сразу принимал директорию.
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = fs::metadata(tmp.path()).unwrap().permissions();
    perms.set_mode(0o700);
    fs::set_permissions(tmp.path(), perms).unwrap();
    for (filename, body) in entries {
        write_defer_file(tmp.path(), filename, body);
    }
    tmp
}

fn write_defer_file(root: &Path, filename: &str, body: &str) {
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = root.join(filename);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    let f = opts.open(&path).unwrap();
    use std::io::Write as _;
    let mut w = std::io::BufWriter::new(f);
    w.write_all(body.as_bytes()).unwrap();
    w.flush().unwrap();
}

/// Минимальный валидный JSON для DeferEntry с заданным id/target.
fn defer_entry_json(id: &str, action: &str, target: &str) -> String {
    format!(
        "{{\
\"spec_version\":1,\
\"id\":\"{id}\",\
\"action\":\"{action}\",\
\"init_system\":\"systemd\",\
\"target\":\"{target}\",\
\"priority\":\"restart\",\
\"enqueued_at\":\"2026-05-19T14:32:11Z\",\
\"enqueued_by\":[],\
\"attempt_count\":0,\
\"max_attempts\":3\
}}"
    )
}

#[test]
fn status_empty_journal_exits_zero_with_no_pending_message() {
    let tmp = make_empty_journal();
    let output = bosun()
        .args(["status", "--defers-dir"])
        .arg(tmp.path())
        .output()
        .expect("binary runs");
    assert!(
        output.status.success(),
        "expected exit 0, got: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no pending defers"),
        "expected 'no pending defers', got:\n{stdout}"
    );
}

#[test]
fn status_with_two_pending_renders_table_in_text_mode() {
    let tmp = make_journal_with(&[
        (
            "0r-systemd.restart:nginx.deferred",
            &defer_entry_json("systemd.restart:nginx", "restart", "nginx"),
        ),
        (
            "0r-systemd.restart:postgres.deferred",
            &defer_entry_json("systemd.restart:postgres", "restart", "postgres"),
        ),
    ]);
    let output = bosun()
        .args(["status", "--defers-dir"])
        .arg(tmp.path())
        .output()
        .expect("binary runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("STATE"),
        "expected table headers, got:\n{stdout}"
    );
    assert!(stdout.contains("nginx"), "expected nginx, got:\n{stdout}");
    assert!(
        stdout.contains("postgres"),
        "expected postgres, got:\n{stdout}"
    );
}

#[test]
fn status_with_clear_id_removes_entry_and_exits_zero() {
    let tmp = make_journal_with(&[(
        "0r-systemd.restart:nginx.deferred",
        &defer_entry_json("systemd.restart:nginx", "restart", "nginx"),
    )]);
    // Подтверждаем, что файл существует.
    assert!(tmp
        .path()
        .join("0r-systemd.restart:nginx.deferred")
        .exists());

    let output = bosun()
        .args(["status", "--defers-dir"])
        .arg(tmp.path())
        .args(["--clear", "systemd.restart:nginx"])
        .output()
        .expect("binary runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    assert!(!tmp
        .path()
        .join("0r-systemd.restart:nginx.deferred")
        .exists());
}

#[test]
fn status_json_format_emits_valid_array() {
    let tmp = make_journal_with(&[(
        "0r-systemd.restart:nginx.deferred",
        &defer_entry_json("systemd.restart:nginx", "restart", "nginx"),
    )]);
    let output = bosun()
        .args(["status", "--defers-dir"])
        .arg(tmp.path())
        .args(["--format", "json"])
        .output()
        .expect("binary runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("status JSON valid");
    let arr = value.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["target"], "nginx");
    assert_eq!(arr[0]["state"], "pending");
}

#[test]
fn status_with_manual_clear_present_exits_with_code_one() {
    let tmp = make_journal_with(&[(
        "0r-systemd.restart:nginx.manual_clear",
        &defer_entry_json("systemd.restart:nginx", "restart", "nginx"),
    )]);
    let output = bosun()
        .args(["status", "--defers-dir"])
        .arg(tmp.path())
        .output()
        .expect("binary runs");
    let code = output.status.code().expect("exit code present");
    assert_eq!(
        code, 1,
        "expected exit 1 (manual_clear present), got {code}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("manual_clear"));
}

#[test]
fn status_clear_all_manual_removes_only_manual_clear_files() {
    let tmp = make_journal_with(&[
        (
            "0r-systemd.restart:nginx.deferred",
            &defer_entry_json("systemd.restart:nginx", "restart", "nginx"),
        ),
        (
            "0r-systemd.restart:postgres.manual_clear",
            &defer_entry_json("systemd.restart:postgres", "restart", "postgres"),
        ),
    ]);
    let output = bosun()
        .args(["status", "--defers-dir"])
        .arg(tmp.path())
        .arg("--clear-all-manual")
        .output()
        .expect("binary runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    // Pending defer остался.
    assert!(tmp
        .path()
        .join("0r-systemd.restart:nginx.deferred")
        .exists());
    // Manual clear исчез.
    assert!(!tmp
        .path()
        .join("0r-systemd.restart:postgres.manual_clear")
        .exists());
}
