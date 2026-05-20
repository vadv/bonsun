//! Управление настоящим systemd-PID1 внутри BDD-контейнера.
//!
//! Phase K оставил `systemd_service.feature` с `@todo-skip` потому, что
//! systemd как PID 1 в обычном контейнере не запускается без `--privileged`.
//! Здесь поднимается реальный systemd (252.39 из debian:bookworm), bosun
//! ходит к нему по штатному system-bus сокету `/run/dbus/system_bus_socket`
//! и `/run/systemd/private`.
//!
//! Запуск ограничен сценариями с тегом `@systemd-privileged`. Они НЕ
//! исполняются обычным `make test-bdd` — фильтр в `main.rs` через
//! `BDD_SYSTEMD_PRIVILEGED=1` env (это ставит `make test-bdd-systemd`).
//!
//! Модуль предоставляет:
//! 1. `Given a fresh container with systemd` — поднимает privileged-контейнер,
//!    дожидается `is-system-running`, копирует bosun-бинарь, выставляет
//!    `init_system_override = "systemd"`.
//! 2. `Given the systemd unit "<name>" with content:` — пишет unit-файл
//!    в `/etc/systemd/system/<name>.service` и зовёт `daemon-reload`.
//! 3. `Given the systemd unit "<name>" is started` — `systemctl start`
//!    + дождаться `is-active`.
//! 4. `Given I remember InvocationID of systemd unit "<name>" as "<label>"`
//!    — снимок `systemctl show -p InvocationID`.
//! 5. Assertions: `is in state`, `InvocationID differs from`, `is enabled`.

use cucumber::{given, then};

use crate::docker_helper::{
    docker_cp_into, docker_exec_args, docker_exec_shell, docker_kill, docker_run_systemd,
    install_bosun_binary, locate_bosun_binary, test_image,
};
use crate::world::BosunWorld;

/// Бюджет ожидания готовности systemd. На холодном bookworm-контейнере
/// `is-system-running` отдаёт `running` через ~3 секунды; беру 30 секунд
/// с запасом на медленный CI (`actions-runner`).
const SYSTEMD_READY_TIMEOUT_SEC: u64 = 30;

fn container_id_or_panic(world: &BosunWorld) -> String {
    world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"))
}

/// Запустить privileged-контейнер с systemd как PID 1. Шаг
/// идемпотентен — если до него был обычный `Given a fresh container`,
/// контейнер убивается и поднимается заново уже в privileged-режиме.
#[given(regex = r#"^a fresh container with systemd$"#)]
pub async fn given_fresh_container_with_systemd(world: &mut BosunWorld) {
    if let Some(id) = world.container_id.take() {
        docker_kill(&id);
    }
    let image = test_image();
    let id = docker_run_systemd(&image)
        .unwrap_or_else(|e| panic!("failed to start systemd container from {image}: {e}"));

    // Бутстрап bosun-бинаря. Делаем это сразу — systemd параллельно
    // догружает unit'ы, а нам ничто не мешает скопировать файл и chmod
    // через privileged-exec.
    let bin = world.bosun_binary_path.as_path().to_path_buf();
    let bin = if bin.as_os_str().is_empty() {
        locate_bosun_binary().unwrap_or_else(|e| panic!("locate bosun: {e}"))
    } else {
        bin
    };
    if let Err(e) = install_bosun_binary(&id, &bin) {
        docker_kill(&id);
        panic!("install bosun into systemd container: {e}");
    }
    world.bosun_binary_path = bin;
    world.container_id = Some(id.clone());
    world.container_workdir = "/work".to_string();
    // Сценарий — про реальный systemd; bosun apply должен подобрать
    // ветку `needs_systemd` (Phase J), а не идти в runr или fallback.
    world.init_system_override = Some("systemd".to_string());

    wait_for_systemd_ready(&id);
}

/// Polling `systemctl is-system-running --wait`. Возвращает на первом
/// статусе из `{running, degraded}`: `degraded` валиден в минимальном
/// контейнере, где часть `multi-user.target` зависимостей не нужна
/// (`systemd-tmpfiles-setup` падает на read-only `/usr/lib/tmpfiles.d`,
/// и это не наш дефект).
fn wait_for_systemd_ready(container_id: &str) {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(SYSTEMD_READY_TIMEOUT_SEC);
    let probe = "systemctl is-system-running 2>&1 || true";
    loop {
        let res = docker_exec_shell(container_id, probe)
            .unwrap_or_else(|e| panic!("probe systemd readiness: {e}"));
        let state = res.stdout.trim();
        // `running` — норма, `degraded` — часть unit'ов упала (в
        // минимальном контейнере ожидаемо), оба означают что dbus и
        // PID1 уже отвечают.
        if matches!(state, "running" | "degraded") {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let dump = docker_exec_args(container_id, &["systemctl", "list-jobs"])
                .map(|r| r.combined())
                .unwrap_or_default();
            panic!(
                "systemd did not become ready in {SYSTEMD_READY_TIMEOUT_SEC}s; \
                 last is-system-running: {state:?}\njobs:\n{dump}",
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Записать unit-файл в `/etc/systemd/system/<name>.service`, дёрнуть
/// `daemon-reload`. Имя в шаге — без расширения (`.service` дописывается
/// здесь, как и принято в systemd).
#[given(regex = r#"^the systemd unit "([^"]+)" with content:$"#)]
pub async fn given_systemd_unit(
    world: &mut BosunWorld,
    name: String,
    step: &cucumber::gherkin::Step,
) {
    let id = container_id_or_panic(world);
    let body = step
        .docstring
        .clone()
        .unwrap_or_else(|| panic!("unit-файл missing в docstring шага"));

    // Через `docker cp` из tempfile'а, не через shell-here-doc: shell-
    // escape для unit-файлов с `=`, `$`, кавычками — источник
    // нестабильности.
    let tmp = tempfile::NamedTempFile::new()
        .unwrap_or_else(|e| panic!("create tempfile for unit body: {e}"));
    std::fs::write(tmp.path(), &body).unwrap_or_else(|e| panic!("write unit body: {e}"));
    let dst = format!("/etc/systemd/system/{name}.service");
    docker_cp_into(&id, tmp.path(), &dst).unwrap_or_else(|e| panic!("docker cp unit: {e}"));

    let res = docker_exec_args(&id, &["systemctl", "daemon-reload"])
        .unwrap_or_else(|e| panic!("systemctl daemon-reload: {e}"));
    if res.exit_code != 0 {
        panic!("daemon-reload failed: {}", res.stderr);
    }
}

/// `systemctl start <name>` и подождать `is-active`. Используется, когда
/// сценарий хочет привести unit в `Running` ДО первого apply (например,
/// для NoChange-пути).
#[given(regex = r#"^the systemd unit "([^"]+)" is started$"#)]
pub async fn given_systemd_unit_started(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let unit = format!("{name}.service");
    let res = docker_exec_args(&id, &["systemctl", "start", &unit])
        .unwrap_or_else(|e| panic!("systemctl start: {e}"));
    if res.exit_code != 0 {
        panic!("systemctl start {unit} failed: {}", res.stderr);
    }
    // Дожидаемся active. Без этого следующий шаг сценария может
    // словить `activating` в первое apply.
    wait_for_unit_state(&id, &unit, "active");
}

/// `systemctl enable <name>` — однократный enable до apply'я. Используется
/// в сценарии «уже enabled → bosun не вызывает enable_unit повторно».
#[given(regex = r#"^the systemd unit "([^"]+)" is enabled$"#)]
pub async fn given_systemd_unit_enabled(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let unit = format!("{name}.service");
    let res = docker_exec_args(&id, &["systemctl", "enable", &unit])
        .unwrap_or_else(|e| panic!("systemctl enable: {e}"));
    if res.exit_code != 0 {
        panic!("systemctl enable {unit} failed: {}", res.stderr);
    }
}

/// Опрос `systemctl is-active`. Поллит до deadline'а (90с) либо до
/// получения ожидаемого состояния. systemd может ответить `activating`
/// сразу после `start`, и rant'еться 0–1 секунду.
fn wait_for_unit_state(container_id: &str, unit: &str, expected: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let res = docker_exec_args(container_id, &["systemctl", "is-active", unit])
            .unwrap_or_else(|e| panic!("systemctl is-active {unit}: {e}"));
        let state = res.stdout.trim();
        if state == expected {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("systemd unit {unit} did not reach state {expected:?} in 90s; last: {state:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `systemctl show -p InvocationID --value <unit>`. Пустая строка
/// означает что systemd ещё не присвоил ID (unit не стартовал).
fn systemd_invocation_id(container_id: &str, unit: &str) -> Option<String> {
    let res = docker_exec_args(
        container_id,
        &["systemctl", "show", "-p", "InvocationID", "--value", unit],
    )
    .ok()?;
    if res.exit_code != 0 {
        return None;
    }
    let trimmed = res.stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[given(regex = r#"^I remember InvocationID of systemd unit "([^"]+)" as "([^"]+)"$"#)]
pub async fn given_remember_invocation_id(world: &mut BosunWorld, name: String, label: String) {
    let id = container_id_or_panic(world);
    let unit = format!("{name}.service");
    let inv = systemd_invocation_id(&id, &unit)
        .unwrap_or_else(|| panic!("unit {unit} has no InvocationID (not started?)"));
    world.systemd_invocation_snapshots.insert(label, inv);
}

#[then(regex = r#"^InvocationID of systemd unit "([^"]+)" differs from "([^"]+)"$"#)]
pub async fn then_invocation_differs(world: &mut BosunWorld, name: String, label: String) {
    let id = container_id_or_panic(world);
    let unit = format!("{name}.service");
    let actual = systemd_invocation_id(&id, &unit)
        .unwrap_or_else(|| panic!("unit {unit} has no InvocationID now"));
    let before =
        world
            .systemd_invocation_snapshots
            .get(&label)
            .cloned()
            .unwrap_or_else(|| {
                panic!("no InvocationID snapshot under label {label:?}; run `I remember InvocationID ...` first")
            });
    if actual == before {
        panic!(
            "systemd unit {unit} InvocationID did not change: still {actual} (snapshot {label}={before})"
        );
    }
}

#[then(regex = r#"^systemd unit "([^"]+)" is in state "([^"]+)"$"#)]
pub async fn then_systemd_state(world: &mut BosunWorld, name: String, expected: String) {
    let id = container_id_or_panic(world);
    let unit = format!("{name}.service");
    let res = docker_exec_args(&id, &["systemctl", "is-active", &unit])
        .unwrap_or_else(|e| panic!("systemctl is-active {unit}: {e}"));
    let actual = res.stdout.trim();
    if actual != expected {
        panic!("systemd unit {unit} state mismatch: expected {expected:?}, got {actual:?}");
    }
}

#[then(regex = r#"^systemd unit "([^"]+)" is enabled$"#)]
pub async fn then_systemd_enabled(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let unit = format!("{name}.service");
    let res = docker_exec_args(&id, &["systemctl", "is-enabled", &unit])
        .unwrap_or_else(|e| panic!("systemctl is-enabled {unit}: {e}"));
    // systemctl is-enabled возвращает много вариантов: `enabled`,
    // `enabled-runtime`, `static`, `alias`, `linked`. Для assertion'а
    // «оператор может рассчитывать, что unit стартует на boot» нам
    // нужны только `enabled` / `enabled-runtime`.
    let actual = res.stdout.trim();
    if !matches!(actual, "enabled" | "enabled-runtime") {
        panic!("systemd unit {unit} not enabled: is-enabled={actual:?}");
    }
}
