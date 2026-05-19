//! Реализация subcommand `bosun status` (Phase J).
//!
//! Печатает содержимое journal'а defer'ов (`*.deferred` и `*.manual_clear`)
//! и позволяет очистить конкретный entry или пакетно удалить все
//! `.manual_clear`. Exit-коды:
//! - 0 — journal пуст или содержит только `.deferred` (in-flight defer'ы);
//! - 1 — есть хотя бы один `.manual_clear` (оператору есть что разбирать);
//! - 4 — I/O ошибка (директория недоступна, нет прав, etc).

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use bosun_core::defers::{DeferEntry, Journal};
use serde_json::json;

use crate::args::{StatusArgs, StatusFormat};
use crate::exit_code;

/// Один entry в выдаче `bosun status`.
struct StatusEntry {
    entry: DeferEntry,
    /// `pending` для `.deferred`, `manual_clear` для промоутнутых.
    state: &'static str,
}

pub fn run(args: &StatusArgs) -> i32 {
    let journal = match Journal::open(&args.defers_dir) {
        Ok(j) => j,
        Err(e) => {
            eprintln!(
                "bosun: failed to open defer journal at {}: {}",
                args.defers_dir.display(),
                e,
            );
            return exit_code::CLI_ENV_ERROR;
        }
    };

    // Clear-варианты выполняются до листинга: операция атомарна, ошибка
    // обрабатывается отдельно. Если оператор указал и --clear, и
    // --clear-all-manual — выполняются оба (clear сначала, manual-clear
    // после), затем вывод обновлённого состояния.
    if let Some(id) = args.clear.as_deref() {
        match clear_one(&journal, id) {
            Ok(true) => {
                tracing::warn!(id = %id, "defer entry cleared via bosun status --clear");
            }
            Ok(false) => {
                eprintln!("bosun status --clear: no defer entry matches {id}");
                return exit_code::CLI_ENV_ERROR;
            }
            Err(e) => {
                eprintln!("bosun status --clear failed: {e}");
                return exit_code::CLI_ENV_ERROR;
            }
        }
    }
    if args.clear_all_manual {
        match clear_all_manual(journal.root()) {
            Ok(n) => {
                tracing::warn!(
                    removed = n,
                    "bosun status --clear-all-manual removed manual_clear files",
                );
            }
            Err(e) => {
                eprintln!("bosun status --clear-all-manual failed: {e}");
                return exit_code::CLI_ENV_ERROR;
            }
        }
    }

    let pending = match journal.list_sorted() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("bosun status: failed to list defer journal: {e}");
            return exit_code::CLI_ENV_ERROR;
        }
    };
    let manual = match list_manual_clear(journal.root()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("bosun status: failed to list manual_clear files: {e}");
            return exit_code::CLI_ENV_ERROR;
        }
    };

    let mut entries: Vec<StatusEntry> = pending
        .into_iter()
        .map(|e| StatusEntry {
            entry: e,
            state: "pending",
        })
        .collect();
    for m in manual {
        entries.push(StatusEntry {
            entry: m,
            state: "manual_clear",
        });
    }

    let has_manual = entries.iter().any(|e| e.state == "manual_clear");
    let mut out = std::io::stdout().lock();
    match args.format {
        StatusFormat::Text => print_text(&mut out, &entries),
        StatusFormat::Json => print_json(&mut out, &entries),
    }

    if has_manual {
        exit_code::STATUS_MANUAL_CLEAR_PRESENT
    } else {
        exit_code::SUCCESS
    }
}

/// Найти файл по id и удалить его. Возвращает `true`, если что-то нашли.
fn clear_one(journal: &Journal, id: &str) -> std::io::Result<bool> {
    let root = journal.root();
    let dir = fs::read_dir(root)?;
    for ent in dir {
        let ent = ent?;
        let path = ent.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let ext = path.extension().and_then(OsStr::to_str);
        if ext != Some("deferred") && ext != Some("manual_clear") {
            continue;
        }
        // id-match: либо файл полностью совпадает с переданной строкой
        // (если оператор скопировал имя из ls'а), либо записанный entry
        // имеет соответствующий id. Точное совпадение через имя дешевле,
        // поэтому сначала проверяем его.
        if name == id {
            fs::remove_file(&path)?;
            fsync_dir(root)?;
            return Ok(true);
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            // Если файл уже исчез — другая инстанция параллельно очистила,
            // продолжаем поиск.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if let Ok(entry) = serde_json::from_slice::<DeferEntry>(&bytes) {
            if entry.id == id {
                fs::remove_file(&path)?;
                fsync_dir(root)?;
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Удалить все `*.manual_clear` файлы в journal'е. Возвращает количество.
fn clear_all_manual(root: &Path) -> std::io::Result<usize> {
    let dir = fs::read_dir(root)?;
    let mut removed = 0;
    for ent in dir {
        let ent = ent?;
        let path = ent.path();
        if path.extension().and_then(OsStr::to_str) != Some("manual_clear") {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed += 1,
            // Файл уже исчез между read_dir и remove_file — не ошибка.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    if removed > 0 {
        fsync_dir(root)?;
    }
    Ok(removed)
}

/// Перечислить `.manual_clear` файлы с разбором содержимого. Повреждённые
/// файлы игнорируем (как делает Journal::list_sorted для `.deferred`).
fn list_manual_clear(root: &Path) -> std::io::Result<Vec<DeferEntry>> {
    let dir = fs::read_dir(root)?;
    let mut out: Vec<(String, DeferEntry)> = Vec::new();
    for ent in dir {
        let ent = ent?;
        let path = ent.path();
        if path.extension().and_then(OsStr::to_str) != Some("manual_clear") {
            continue;
        }
        let Some(name) = path.file_name().and_then(OsStr::to_str).map(str::to_owned) else {
            continue;
        };
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let Ok(entry) = serde_json::from_slice::<DeferEntry>(&bytes) else {
            tracing::warn!(
                path = %path.display(),
                "skipping corrupt manual_clear file"
            );
            continue;
        };
        out.push((name, entry));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out.into_iter().map(|(_, e)| e).collect())
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let f = OpenOptions::new().read(true).open(dir)?;
    f.sync_all()
}

/// Текстовый табличный вывод: фиксированные колонки. Пустой journal — две
/// строки «no pending defers», чтобы оператор сразу видел, что искать
/// нечего.
fn print_text<W: Write>(out: &mut W, entries: &[StatusEntry]) {
    if entries.is_empty() {
        let _ = writeln!(out, "no pending defers");
        return;
    }
    let _ = writeln!(
        out,
        "{:<12} {:<32} {:<20} {:<24} {:<8} {:<20}",
        "STATE", "ID", "ACTION", "TARGET", "ATTEMPTS", "ENQUEUED_AT",
    );
    for e in entries {
        let action = e.entry.action.filename_slug();
        let attempts = format!("{}/{}", e.entry.attempt_count, e.entry.max_attempts);
        let enq = e.entry.enqueued_at.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let _ = writeln!(
            out,
            "{:<12} {:<32} {:<20} {:<24} {:<8} {:<20}",
            e.state,
            truncate(&e.entry.id, 32),
            truncate(action, 20),
            truncate(&e.entry.target, 24),
            attempts,
            enq,
        );
    }
}

fn print_json<W: Write>(out: &mut W, entries: &[StatusEntry]) {
    // Сериализуем каждый entry как JSON, добавляя поле "state".
    let array: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let mut v = serde_json::to_value(&e.entry).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("state".to_string(), json!(e.state));
            }
            v
        })
        .collect();
    let json_value = serde_json::Value::Array(array);
    if let Ok(text) = serde_json::to_string_pretty(&json_value) {
        let _ = writeln!(out, "{text}");
    } else {
        // Fallback: пустой массив. Гарантирует валидный JSON даже при
        // загадочной ошибке serde.
        let _ = writeln!(out, "[]");
    }
}

/// Подрезать строку до `n` символов с многоточием. Нужна для табличного
/// вывода: длинные id (`systemd.restart:nginx.service`) не должны ломать
/// колонки.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    if n < 4 {
        return s.chars().take(n).collect();
    }
    let mut out: String = s.chars().take(n - 3).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use bosun_core::defers::{
        make_id, DeferAction, DeferEntry, EnqueueResult, CURRENT_SPEC_VERSION,
    };
    use chrono::Utc;
    use tempfile::TempDir;

    fn make_entry(init: &str, action: DeferAction, target: &str) -> DeferEntry {
        let priority = action.default_priority();
        DeferEntry {
            spec_version: CURRENT_SPEC_VERSION,
            id: make_id(init, &action, target),
            action,
            init_system: init.to_string(),
            target: target.to_string(),
            validate_cmd: None,
            health_check: None,
            priority,
            enqueued_at: Utc::now(),
            enqueued_by: vec![],
            attempt_count: 0,
            max_attempts: 3,
        }
    }

    fn open() -> (TempDir, Journal) {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        (tmp, journal)
    }

    #[test]
    fn truncate_short_string_returns_as_is() {
        assert_eq!(truncate("nginx", 10), "nginx");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        // n=16 → take 13 chars ("systemd.resta") + "..." → 16 chars total.
        assert_eq!(
            truncate("systemd.restart:nginx.service", 16),
            "systemd.resta..."
        );
    }

    #[test]
    fn print_text_empty_journal_shows_message() {
        let mut out = Vec::new();
        print_text(&mut out, &[]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("no pending defers"));
    }

    #[test]
    fn print_text_with_two_entries_renders_table() {
        let e1 = make_entry("systemd", DeferAction::Restart, "nginx");
        let e2 = make_entry("runr", DeferAction::Reload, "postgres");
        let entries = vec![
            StatusEntry {
                entry: e1,
                state: "pending",
            },
            StatusEntry {
                entry: e2,
                state: "pending",
            },
        ];
        let mut out = Vec::new();
        print_text(&mut out, &entries);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("STATE"));
        assert!(s.contains("ID"));
        assert!(s.contains("ACTION"));
        assert!(s.contains("nginx"));
        assert!(s.contains("postgres"));
    }

    #[test]
    fn print_json_emits_array_with_state_field() {
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        let entries = vec![StatusEntry {
            entry,
            state: "pending",
        }];
        let mut out = Vec::new();
        print_json(&mut out, &entries);
        let s = String::from_utf8(out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["state"], "pending");
        assert_eq!(arr[0]["target"], "nginx");
    }

    #[test]
    fn clear_one_removes_existing_entry_by_id() {
        let (tmp, journal) = open();
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        journal.enqueue(entry.clone()).unwrap();
        let removed = clear_one(&journal, &entry.id).unwrap();
        assert!(removed);
        let count: usize = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(OsStr::to_str) == Some("deferred"))
            .count();
        assert_eq!(count, 0);
    }

    #[test]
    fn clear_one_returns_false_when_id_not_found() {
        let (_tmp, journal) = open();
        let removed = clear_one(&journal, "systemd.restart:ghost").unwrap();
        assert!(!removed);
    }

    #[test]
    fn clear_one_accepts_filename_directly() {
        let (_tmp, journal) = open();
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        journal.enqueue(entry.clone()).unwrap();
        let filename = entry.filename();
        let removed = clear_one(&journal, &filename).unwrap();
        assert!(removed);
    }

    #[test]
    fn clear_all_manual_removes_only_manual_clear_files() {
        let (tmp, journal) = open();
        // Один pending defer.
        let pending = make_entry("systemd", DeferAction::Restart, "nginx");
        journal.enqueue(pending.clone()).unwrap();
        // Один manual_clear — создаём через move_to_manual_clear.
        let manual = make_entry("systemd", DeferAction::Reload, "postgres");
        journal.enqueue(manual.clone()).unwrap();
        journal.move_to_manual_clear(&manual).unwrap();

        let removed = clear_all_manual(tmp.path()).unwrap();
        assert_eq!(removed, 1);
        // pending остался.
        let deferred_count: usize = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(OsStr::to_str) == Some("deferred"))
            .count();
        assert_eq!(deferred_count, 1);
        let manual_count: usize = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(OsStr::to_str) == Some("manual_clear"))
            .count();
        assert_eq!(manual_count, 0);
    }

    #[test]
    fn list_manual_clear_returns_promoted_entries() {
        let (tmp, journal) = open();
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        assert_eq!(
            journal.enqueue(entry.clone()).unwrap(),
            EnqueueResult::Created
        );
        journal.move_to_manual_clear(&entry).unwrap();
        let list = list_manual_clear(tmp.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, entry.id);
    }

    #[test]
    fn run_empty_journal_exits_zero() {
        let tmp = TempDir::new().unwrap();
        let args = StatusArgs {
            defers_dir: tmp.path().to_path_buf(),
            format: StatusFormat::Text,
            clear: None,
            clear_all_manual: false,
        };
        let code = run(&args);
        assert_eq!(code, exit_code::SUCCESS);
    }

    #[test]
    fn run_with_manual_clear_returns_exit_one() {
        let (tmp, journal) = open();
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        journal.enqueue(entry.clone()).unwrap();
        journal.move_to_manual_clear(&entry).unwrap();
        let args = StatusArgs {
            defers_dir: tmp.path().to_path_buf(),
            format: StatusFormat::Text,
            clear: None,
            clear_all_manual: false,
        };
        let code = run(&args);
        assert_eq!(code, exit_code::STATUS_MANUAL_CLEAR_PRESENT);
    }

    #[test]
    fn run_clear_id_removes_entry() {
        let (tmp, journal) = open();
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        journal.enqueue(entry.clone()).unwrap();
        let args = StatusArgs {
            defers_dir: tmp.path().to_path_buf(),
            format: StatusFormat::Text,
            clear: Some(entry.id.clone()),
            clear_all_manual: false,
        };
        let code = run(&args);
        assert_eq!(code, exit_code::SUCCESS);
        let count: usize = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(OsStr::to_str) == Some("deferred"))
            .count();
        assert_eq!(count, 0);
    }
}
