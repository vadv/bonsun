//! Утилиты для проверки журнала defer'ов внутри контейнера.
//!
//! Сценарии Phase G (process.signal deferred=True) и Phase J
//! (bosun status, replay) опираются на содержимое `/tmp/bosun-defers/`.
//! Этот модуль реализует ровно те Then-шаги, которые читают директорию
//! через `docker exec` и сверяют состояние с ожиданиями.
//!
//! Файлы в журнале — это JSON, описывающий defer-entry, с расширением
//! `.deferred` (pending) или `.manual_clear` (промоутированный после
//! исчерпания max_attempts). Шаги читают их через `cat` и парсят на
//! хосте — без отдельного `bosun status`, чтобы сценарий не зависел от
//! формата вывода CLI.

use cucumber::then;

use crate::docker_helper::{docker_exec_args, docker_exec_shell};
use crate::world::BosunWorld;

const DEFAULT_DEFERS_DIR: &str = "/tmp/bosun-defers";

fn container_id_or_panic(world: &BosunWorld) -> String {
    world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"))
}

/// Список всех `.deferred` файлов в journal'е (filenames, без полного пути).
fn list_deferred(world: &BosunWorld) -> Vec<String> {
    let id = container_id_or_panic(world);
    let cmd = format!("ls -1 {DEFAULT_DEFERS_DIR} 2>/dev/null | grep '\\.deferred$' || true");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("ls defers: {e}"));
    res.stdout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Список всех `.manual_clear` файлов.
fn list_manual_clear(world: &BosunWorld) -> Vec<String> {
    let id = container_id_or_panic(world);
    let cmd = format!("ls -1 {DEFAULT_DEFERS_DIR} 2>/dev/null | grep '\\.manual_clear$' || true");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("ls manual_clear: {e}"));
    res.stdout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[then(regex = r#"^the defer journal contains (\d+) pending entr(?:y|ies)$"#)]
pub async fn then_defer_pending_count(world: &mut BosunWorld, expected: usize) {
    let files = list_deferred(world);
    if files.len() != expected {
        panic!(
            "pending defer count mismatch: expected {expected}, got {actual}\nfiles:\n{listing}",
            actual = files.len(),
            listing = files.join("\n"),
        );
    }
}

#[then(regex = r#"^the defer journal contains (\d+) manual_clear entr(?:y|ies)$"#)]
pub async fn then_defer_manual_clear_count(world: &mut BosunWorld, expected: usize) {
    let files = list_manual_clear(world);
    if files.len() != expected {
        panic!(
            "manual_clear count mismatch: expected {expected}, got {actual}\nfiles:\n{listing}",
            actual = files.len(),
            listing = files.join("\n"),
        );
    }
}

#[then(regex = r#"^the defer journal is empty$"#)]
pub async fn then_defer_empty(world: &mut BosunWorld) {
    let pending = list_deferred(world);
    let manual = list_manual_clear(world);
    if !pending.is_empty() || !manual.is_empty() {
        panic!(
            "defer journal is not empty\npending:\n{p}\nmanual_clear:\n{m}",
            p = pending.join("\n"),
            m = manual.join("\n"),
        );
    }
}

#[then(regex = r#"^the defer journal has a pending entry for "([^"]+)"$"#)]
pub async fn then_defer_pending_target(world: &mut BosunWorld, target: String) {
    let id = container_id_or_panic(world);
    let cmd = format!(
        "for f in {DEFAULT_DEFERS_DIR}/*.deferred; do [ -e \"$f\" ] || continue; cat \"$f\"; echo; done"
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("read defers: {e}"));
    let mut hit = false;
    for line in res.stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            if json.get("target").and_then(|v| v.as_str()) == Some(target.as_str()) {
                hit = true;
                break;
            }
        }
    }
    if !hit {
        panic!(
            "no pending defer entry with target {target:?}\njournal dump:\n{dump}",
            dump = res.stdout,
        );
    }
}

#[then(regex = r#"^the pending defer for "([^"]+)" has action "([^"]+)"$"#)]
pub async fn then_defer_action(world: &mut BosunWorld, target: String, action: String) {
    let id = container_id_or_panic(world);
    let cmd = format!(
        "for f in {DEFAULT_DEFERS_DIR}/*.deferred; do [ -e \"$f\" ] || continue; cat \"$f\"; echo; done"
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("read defers: {e}"));
    for line in res.stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if json.get("target").and_then(|v| v.as_str()) != Some(target.as_str()) {
            continue;
        }
        let actual = json.get("action").and_then(|v| v.as_str()).unwrap_or("");
        // action в JSON может сериализоваться и в snake_case ("restart") и
        // как объект (для DaemonReload и пр.); сверяем как substring,
        // что устойчиво к обоим форматам.
        if actual.eq_ignore_ascii_case(&action) || actual.contains(&action) {
            return;
        }
        let raw = json
            .get("action")
            .map(|v| v.to_string())
            .unwrap_or_default();
        panic!("defer {target:?} action {raw:?}, expected {action:?}");
    }
    panic!(
        "no pending defer for {target:?}\njournal dump:\n{dump}",
        dump = res.stdout,
    );
}

#[then(regex = r#"^the pending defer for "([^"]+)" has attempt_count (\d+)$"#)]
pub async fn then_defer_attempt_count(world: &mut BosunWorld, target: String, expected: u64) {
    let id = container_id_or_panic(world);
    let cmd = format!(
        "for f in {DEFAULT_DEFERS_DIR}/*.deferred; do [ -e \"$f\" ] || continue; cat \"$f\"; echo; done"
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("read defers: {e}"));
    for line in res.stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if json.get("target").and_then(|v| v.as_str()) != Some(target.as_str()) {
            continue;
        }
        let actual = json
            .get("attempt_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if actual == expected {
            return;
        }
        panic!("defer {target:?} attempt_count={actual}, expected {expected}");
    }
    panic!(
        "no pending defer for {target:?}\njournal dump:\n{dump}",
        dump = res.stdout,
    );
}

#[then(regex = r#"^the defer journal has a manual_clear entry for "([^"]+)"$"#)]
pub async fn then_defer_manual_clear_target(world: &mut BosunWorld, target: String) {
    let id = container_id_or_panic(world);
    let cmd = format!(
        "for f in {DEFAULT_DEFERS_DIR}/*.manual_clear; do [ -e \"$f\" ] || continue; cat \"$f\"; echo; done"
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("read manual_clear: {e}"));
    for line in res.stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if json.get("target").and_then(|v| v.as_str()) == Some(target.as_str()) {
            return;
        }
    }
    panic!(
        "no manual_clear defer for {target:?}\nlisting:\n{dump}",
        dump = res.stdout,
    );
}

/// Stage shorthand: подготовить journal-директорию пустой.
#[cucumber::given(regex = r#"^the defer journal is empty$"#)]
pub async fn given_empty_defer_journal(world: &mut BosunWorld) {
    let id = container_id_or_panic(world);
    let cmd = format!("rm -f {DEFAULT_DEFERS_DIR}/*.deferred {DEFAULT_DEFERS_DIR}/*.manual_clear 2>/dev/null; mkdir -p {DEFAULT_DEFERS_DIR}");
    docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("reset defer journal: {e}"));
}

/// Phase J: команда `bosun status` с явным `--defers-dir`.
#[cucumber::when(regex = r#"^I run "bosun status"$"#)]
pub async fn when_run_bosun_status(world: &mut BosunWorld) {
    let id = container_id_or_panic(world);
    let cmd = format!("bosun status --defers-dir {DEFAULT_DEFERS_DIR}");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec status: {e}"));
    world.last_exec = Some(res);
}

#[cucumber::when(regex = r#"^I run "bosun status --format json"$"#)]
pub async fn when_run_bosun_status_json(world: &mut BosunWorld) {
    let id = container_id_or_panic(world);
    let cmd = format!("bosun status --defers-dir {DEFAULT_DEFERS_DIR} --format json");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec status: {e}"));
    world.last_exec = Some(res);
}

#[cucumber::when(regex = r#"^I run "bosun status --clear ([^"]+)"$"#)]
pub async fn when_run_bosun_status_clear(world: &mut BosunWorld, id_or_filename: String) {
    let cid = container_id_or_panic(world);
    let cmd = format!("bosun status --defers-dir {DEFAULT_DEFERS_DIR} --clear '{id_or_filename}'");
    let res = docker_exec_shell(&cid, &cmd).unwrap_or_else(|e| panic!("docker exec status: {e}"));
    world.last_exec = Some(res);
}

/// Прямой ввод defer-entry в журнал — короткий путь для bosun_status.feature
/// (не зависит от Phase G/J pipeline'а). Пишем готовый JSON в файл с нужным
/// расширением; пути соответствуют формату `<priority>-<init>.<action>:<target>.<ext>`.
#[cucumber::given(
    regex = r#"^the defer journal has a pending entry for "([^"]+)" with action "([^"]+)"$"#
)]
pub async fn given_pending_defer_entry(world: &mut BosunWorld, target: String, action: String) {
    seed_defer_file(world, &target, &action, "deferred", 0);
}

#[cucumber::given(
    regex = r#"^the defer journal has a manual_clear entry for "([^"]+)" with action "([^"]+)"$"#
)]
pub async fn given_manual_clear_entry(world: &mut BosunWorld, target: String, action: String) {
    // attempt_count=3 → исчерпан, manual_clear именно так и появляется.
    seed_defer_file(world, &target, &action, "manual_clear", 3);
}

fn seed_defer_file(
    world: &mut BosunWorld,
    target: &str,
    action: &str,
    ext: &str,
    attempt_count: u32,
) {
    let id = container_id_or_panic(world);
    let action_slug = action.to_lowercase();
    // DeferAction сериализуется через `#[serde(tag = "action", rename_all =
    // "snake_case")]`, поэтому action в JSON — это lowercase-строка
    // ("restart", "reload"). Priority — тоже snake_case-строка через
    // DeferPriority Display (sortkey-первая буква типа "5r", полное имя
    // varianta для serde — отдельно).
    let priority_slug = match action_slug.as_str() {
        "restart" | "start" | "stop" => "restart",
        "reload_or_restart" => "reload_or_restart",
        "reload" => "reload",
        "daemon_reload" => "daemon_reload",
        _ => "restart",
    };
    let priority_prefix = match priority_slug {
        "restart" => "5r",
        "reload_or_restart" => "6r",
        "reload" => "7r",
        "daemon_reload" => "0r",
        _ => "5r",
    };
    let filename = format!("{priority_prefix}-systemd.{action_slug}:{target}.{ext}");
    let json = serde_json::json!({
        "spec_version": 1,
        "id": format!("systemd.{action_slug}:{target}"),
        "action": action_slug,
        "init_system": "systemd",
        "target": target,
        "validate_cmd": null,
        "health_check": null,
        "priority": priority_slug,
        "enqueued_at": "2026-01-01T00:00:00Z",
        "enqueued_by": [],
        "attempt_count": attempt_count,
        "max_attempts": 3,
    });
    let escaped = json.to_string().replace('\'', "'\\''");
    let cmd = format!(
        "mkdir -p {DEFAULT_DEFERS_DIR} && printf '%s' '{escaped}' > {DEFAULT_DEFERS_DIR}/{filename}"
    );
    let res =
        docker_exec_args(&id, &["sh", "-c", &cmd]).unwrap_or_else(|e| panic!("seed defer: {e}"));
    if res.exit_code != 0 {
        panic!("seed defer file failed: {}", res.stderr);
    }
}
