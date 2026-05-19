//! Управление настоящим runr-supervisor'ом внутри BDD-контейнера.
//!
//! В отличие от Phase K (где runr был мокнут python-сервером и
//! `runr.service`-сценарии стояли под `@todo-skip`), здесь поднимается
//! настоящий бинарь из `target/runr-bookworm/runr` (см. Makefile цель
//! `runr-bookworm`). Это даёт:
//! - реальный lifecycle: unit-файл рендерится в `/etc/runr/<n>.service`
//!   (test-only хелпер, в проде это делает `service.unit` примитив),
//!   `daemon_reload` подхватывает его, `service_start` запускает
//!   реальный child-процесс.
//! - реальную ошибку «daemon недоступен»: kill через pid-файл,
//!   orchestrator переводит ресурс в `Outcome::Deferred`.
//! - наблюдаемый эффект restart'а: PID процесса меняется. На счётчик
//!   `restarts` опираться нельзя — runr инкрементит его только при
//!   автоматических restart'ах (Restart=always после exit/crash), а
//!   не на внешние API-вызовы (`POST /api/v1/services/<n>/restart`).
//!   См. `runr/src/orchestration/actors/simple.rs`: `self.core.restarts`
//!   увеличивается строго в обработчике exit child'а.
//!
//! Фактически модуль реализует:
//! 1. `Given a fresh container with runr daemon` — bootstrap supervisor'а
//!    в фоне, /etc/runr подготовлен, daemon отвечает на 127.0.0.1:8010.
//! 2. `Given the runr service "<name>" with unit file:` — создание
//!    минимального `.service` файла + `daemon_reload`.
//! 3. Assertions на state, PID-снимок и сравнение, pending defer.

use cucumber::{given, then, when};

use crate::docker_helper::{docker_exec_args, docker_exec_shell};
use crate::world::BosunWorld;

const RUNR_BINARY: &str = "/usr/local/bin/runr";
const RUNR_ROOT: &str = "/etc/runr";
const RUNR_LOG_DIR: &str = "/var/log/runr";
const RUNR_PID_FILE: &str = "/tmp/runr.pid";
const RUNR_LOG_FILE: &str = "/var/log/runr/supervisor.out";
/// Listen address для HTTP API, совпадает с дефолтом `bosun-runr-client::Client`.
const RUNR_HTTP_ADDR: &str = "127.0.0.1:8010";
/// Бюджет ожидания готовности daemon'а. На холодном bookworm-контейнере
/// первый запуск занимает ~0.5s; беру с запасом на медленный CI.
const RUNR_READY_TIMEOUT_SEC: u64 = 15;

fn container_id_or_panic(world: &BosunWorld) -> String {
    world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"))
}

/// Старт контейнера + поднятие runr-daemon'а. По дизайну сценария мы
/// делаем это в одном шаге: PID 1 контейнера остаётся `tail -f /dev/null`,
/// чтобы все существующие шаги (Bash, file-проверки) работали без
/// специальной обвязки. Runr живёт в background-процессе, который мы
/// сами кладём в `/tmp/runr.pid` для последующего kill'а.
#[given(regex = r#"^a fresh container with runr daemon$"#)]
pub async fn given_fresh_container_with_runr(world: &mut BosunWorld) {
    // Делегируем стандартному `Given a fresh container` через прямой
    // вызов docker_helper: иначе пришлось бы заводить два разных
    // фабричных метода и расходовать тест.
    crate::docker_helper::given_fresh_container(world).await;
    start_runr_daemon(world);
    // BDD-сценарий «работает под runr» — fact init_system должен
    // дать `runr`, а не tail-classified `unknown`. CLI флаг
    // `--init-system runr` ставится через World, чтобы apply подобрал
    // соответствующую ветку (см. bundle_helper::apply_cmd).
    world.init_system_override = Some("runr".to_string());
}

/// Поднять runr supervisor в фоне. Если уже поднят — no-op. Стартуем
/// через `nohup` + `&`, перенаправляем stdout/stderr в файл, чтобы
/// можно было прочитать ошибки post-mortem. PID сохраняется
/// в `/tmp/runr.pid` для `pkill`-by-pid.
pub fn start_runr_daemon(world: &mut BosunWorld) {
    let id = container_id_or_panic(world);

    // /etc/runr должен существовать ДО запуска — иначе FsUnitRepository
    // не сможет инициализироваться. mkdir -p идемпотентен.
    let setup = format!("mkdir -p {RUNR_ROOT} {RUNR_LOG_DIR}");
    let res = docker_exec_shell(&id, &setup).unwrap_or_else(|e| panic!("setup /etc/runr: {e}"));
    if res.exit_code != 0 {
        panic!("failed to prepare runr root: {}", res.stderr);
    }

    // Daemon уже жив? Проверяем pid-файл и /proc.
    if is_runr_alive(&id) {
        return;
    }

    // nohup ставит SIGHUP в IGN, & отвязывает; перенаправление в файл
    // нужно, чтобы tail-f-PID 1 не получал поток. Запоминаем PID через
    // bash `$!`.
    let cmd = format!(
        "nohup {bin} supervisor {root} \
            --http-listen-api {addr} \
            --log-dir {log_dir} \
            --log-level error \
            > {log_file} 2>&1 & \
         echo $! > {pid_file}",
        bin = RUNR_BINARY,
        root = RUNR_ROOT,
        addr = RUNR_HTTP_ADDR,
        log_dir = RUNR_LOG_DIR,
        log_file = RUNR_LOG_FILE,
        pid_file = RUNR_PID_FILE,
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("start runr daemon: {e}"));
    if res.exit_code != 0 {
        panic!("runr daemon launch failed: {}", res.stderr);
    }

    wait_for_runr_ready(&id);
}

/// Polling-цикл: дёргаем HTTP API `daemon/info` до получения 200.
fn wait_for_runr_ready(container_id: &str) {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(RUNR_READY_TIMEOUT_SEC);
    let probe = format!(
        "curl -s -o /dev/null -w '%{{http_code}}' http://{RUNR_HTTP_ADDR}/api/v1/daemon/info"
    );
    loop {
        let res = docker_exec_shell(container_id, &probe)
            .unwrap_or_else(|e| panic!("probe runr daemon: {e}"));
        if res.stdout.trim() == "200" {
            return;
        }
        if std::time::Instant::now() >= deadline {
            // Дамп лога — оператор увидит причину сразу.
            let log = docker_exec_args(container_id, &["cat", RUNR_LOG_FILE])
                .map(|r| r.stdout)
                .unwrap_or_default();
            panic!(
                "runr daemon did not become ready in {RUNR_READY_TIMEOUT_SEC}s; \
                 last probe code: {code:?}\nlog:\n{log}",
                code = res.stdout.trim(),
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// True, если pid-файл указывает на живой процесс.
fn is_runr_alive(container_id: &str) -> bool {
    let cmd = format!(
        "if [ -f {pid_file} ]; then \
            kill -0 \"$(cat {pid_file})\" 2>/dev/null && echo alive || echo dead; \
         else echo missing; fi",
        pid_file = RUNR_PID_FILE
    );
    let res = match docker_exec_shell(container_id, &cmd) {
        Ok(r) => r,
        Err(_) => return false,
    };
    res.stdout.trim() == "alive"
}

/// Подложить минимальный `.service` файл в `/etc/runr/<name>.service`.
/// До появления `service.unit`-примитива это шаг сценария: тест сам
/// рендерит unit, иначе runr.service-вызов попадает в `ServiceNotFound`
/// и оператор видит ошибку до апплая.
#[given(regex = r#"^the runr service "([^"]+)" with unit file:$"#)]
pub async fn given_runr_unit_file(
    world: &mut BosunWorld,
    name: String,
    step: &cucumber::gherkin::Step,
) {
    let id = container_id_or_panic(world);
    let body = step
        .docstring
        .clone()
        .unwrap_or_else(|| panic!("unit file body missing in docstring"));
    let escaped = body.replace('\'', "'\\''");
    let path = format!("{RUNR_ROOT}/{name}.service");
    let cmd = format!(
        "mkdir -p {RUNR_ROOT} && printf '%s' '{escaped}' > {path}",
        path = path,
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("write unit file: {e}"));
    if res.exit_code != 0 {
        panic!("failed to write {path}: {}", res.stderr);
    }

    // Если runr уже поднят — попросим его перечитать ФС, иначе свежий
    // unit-файл будет невидим до daemon-restart'а.
    if is_runr_alive(&id) {
        let reload = format!(
            "curl -s -X POST -H 'Content-Type: application/json' -d '{{}}' \
             http://{RUNR_HTTP_ADDR}/api/v1/units/reload"
        );
        let _ = docker_exec_shell(&id, &reload);
    }
}

/// Считать `restarts` для сервиса. None означает «runr не знает такого
/// сервиса» (то есть тест ожидает start-с-нуля). Ответ runr —
/// плоский массив `[{name, state, restarts, ...}, ...]`, поэтому jq
/// обращается через `.[]`, а не `.services[]`: схема Go-клиента
/// (с обёрткой `{services: [...]}`) больше не актуальна.
fn runr_service_restarts(container_id: &str, name: &str) -> Option<u64> {
    let cmd = format!(
        "curl -s http://{RUNR_HTTP_ADDR}/api/v1/services/statuses | jq -r '.[] | select(.name == \"{name}\") | .restarts // 0'"
    );
    let res = docker_exec_shell(container_id, &cmd).ok()?;
    if res.exit_code != 0 {
        return None;
    }
    let trimmed = res.stdout.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

/// Считать state сервиса (`Running` / `Stopped` / ...). None означает,
/// что сервис runr'у неизвестен.
fn runr_service_state(container_id: &str, name: &str) -> Option<String> {
    let cmd = format!(
        "curl -s http://{RUNR_HTTP_ADDR}/api/v1/services/statuses | jq -r '.[] | select(.name == \"{name}\") | .state // \"\"'"
    );
    let res = docker_exec_shell(container_id, &cmd).ok()?;
    if res.exit_code != 0 {
        return None;
    }
    let trimmed = res.stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

#[then(regex = r#"^runr service "([^"]+)" is in state "([^"]+)"$"#)]
pub async fn then_runr_state(world: &mut BosunWorld, name: String, expected: String) {
    let id = container_id_or_panic(world);
    let actual = runr_service_state(&id, &name)
        .unwrap_or_else(|| panic!("runr does not know service {name:?}"));
    if actual != expected {
        panic!("runr service {name} state mismatch: expected {expected:?}, got {actual:?}");
    }
}

#[then(regex = r#"^runr service "([^"]+)" has restarts (\d+)$"#)]
pub async fn then_runr_restarts(world: &mut BosunWorld, name: String, expected: u64) {
    let id = container_id_or_panic(world);
    let actual = runr_service_restarts(&id, &name)
        .unwrap_or_else(|| panic!("runr does not know service {name:?}"));
    if actual != expected {
        panic!("runr service {name} restarts mismatch: expected {expected}, got {actual}");
    }
}

/// Считать PID сервиса. None — runr не знает имя или сервис в Stopped
/// (PID отсутствует). Используется для assertion'ов «PID до и после
/// рестарта отличается». Опираться на `restarts` нельзя: runr не
/// инкрементит счётчик при ручных API-restart'ах (см. runr источник:
/// `simple.rs::restart` инкрементит только при auto-restart на крэше).
fn runr_service_pid(container_id: &str, name: &str) -> Option<u32> {
    let cmd = format!(
        "curl -s http://{RUNR_HTTP_ADDR}/api/v1/services/statuses | jq -r '.[] | select(.name == \"{name}\") | .pid // \"null\"'"
    );
    let res = docker_exec_shell(container_id, &cmd).ok()?;
    if res.exit_code != 0 {
        return None;
    }
    let trimmed = res.stdout.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return None;
    }
    trimmed.parse::<u32>().ok()
}

#[given(regex = r#"^I remember pid of runr service "([^"]+)" as "([^"]+)"$"#)]
pub async fn given_remember_pid(world: &mut BosunWorld, name: String, label: String) {
    let id = container_id_or_panic(world);
    let pid = runr_service_pid(&id, &name)
        .unwrap_or_else(|| panic!("runr service {name:?} has no pid (Stopped or unknown)"));
    world.runr_pid_snapshots.insert(label, pid);
}

#[then(regex = r#"^pid of runr service "([^"]+)" differs from "([^"]+)"$"#)]
pub async fn then_pid_differs(world: &mut BosunWorld, name: String, label: String) {
    let id = container_id_or_panic(world);
    let actual = runr_service_pid(&id, &name)
        .unwrap_or_else(|| panic!("runr service {name:?} has no pid now"));
    let before = world
        .runr_pid_snapshots
        .get(&label)
        .copied()
        .unwrap_or_else(|| {
            panic!("no pid snapshot under label {label:?}; run `I remember pid ...` first")
        });
    if actual == before {
        panic!(
            "runr service {name} pid did not change: still {actual} (snapshot {label}={before})"
        );
    }
}

/// Убить runr daemon (имитация недоступности). Используется в сценарии
/// «Deferred при недоступности» и подобных.
#[when(regex = r#"^I stop the runr daemon$"#)]
pub async fn when_stop_runr(world: &mut BosunWorld) {
    let id = container_id_or_panic(world);
    let cmd = format!(
        "if [ -f {pid_file} ]; then \
            kill \"$(cat {pid_file})\" 2>/dev/null || true; \
            rm -f {pid_file}; \
         fi; \
         # ждём пока порт реально освободится \
         for _ in 1 2 3 4 5 6 7 8 9 10; do \
            curl -s -o /dev/null -w '%{{http_code}}' http://{addr}/api/v1/daemon/info \
                | grep -qx 200 || break; \
            sleep 0.1; \
         done",
        pid_file = RUNR_PID_FILE,
        addr = RUNR_HTTP_ADDR,
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("stop runr daemon: {e}"));
    if res.exit_code != 0 {
        panic!("failed to stop runr daemon: {}", res.stderr);
    }
}

/// Снова поднять runr daemon (после `I stop the runr daemon`).
#[when(regex = r#"^I start the runr daemon$"#)]
pub async fn when_start_runr(world: &mut BosunWorld) {
    start_runr_daemon(world);
}
