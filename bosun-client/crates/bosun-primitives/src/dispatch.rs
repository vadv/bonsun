//! Phase J: production-реализация [`DispatchClient`] для replay-цикла defers.
//!
//! Мостит [`DeferEntry`] на конкретные методы [`RunrHandle`] /
//! [`SystemdHandle`] / `std::process::Command`. Используется в CLI после
//! сборки handle'ов из фактов: `init_system = systemd` → есть systemd-handle,
//! `init_system = runr` → есть runr-handle, `mixed-systemd-runr` → оба.
//!
//! Контракт классификации ошибок повторяет правила примитивов
//! (`bosun-primitives::runr_service::map_runr_error` и
//! `systemd_service::map_systemd_error`):
//! - Transport-уровневые отказы (runr `Unavailable`, systemd `BusUnavailable`)
//!   маппятся в [`DispatchError::ClientUnavailable`] → replay пропускает
//!   запись без bump'а attempt'а.
//! - Любые другие ошибки (404, 5xx, JobFailed, exec failed) маппятся в
//!   [`DispatchError::Action`] → bump_attempt и при превышении лимита
//!   promotion в `.manual_clear`.
//! - Если для нужного init-system'а не подключен handle (например,
//!   `entry.init_system = "runr"`, а CLI собирал только systemd-handle) —
//!   возвращается `ClientUnavailable`. Это позволяет на смешанной ноде
//!   быть терпимым к временной недоступности одного из стеков.
//!
//! Action `Command` исполняется через `std::process::Command` без
//! привлечения shell'а: `argv[0]` — путь к бинарю, `argv[1..]` —
//! аргументы. Это совпадает с поведением `process.signal` и убирает
//! инъекционные риски.

use std::io::Read as _;
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bosun_core::defers::{DeferAction, DeferEntry, DispatchClient, DispatchError};
use bosun_handles::{RunrError, RunrHandle, SystemdError, SystemdHandle};

/// Максимальное время на одну deferred command в replay-цикле. Фиксированная
/// константа на 60 секунд — длиннее обычно тонкие grep'ы и pkill, которые
/// мы деферим, не делают. Если оператор знает, что defer-команда долгая,
/// её нужно переоформить как синхронный примитив в bundle'е (не defer-канал):
/// иначе один зависший exec заблокирует весь replay и через него — весь
/// `bosun apply`. `--deadline-sec` не покрывает replay-фазу, поэтому
/// hard-timeout живёт прямо в dispatch'е.
///
/// На превышении — child убивается через `kill()`, возвращается
/// `DispatchError::Action("timed out ...")`. Defer-запись остаётся в
/// журнале (replay инкрементирует attempt), и при превышении `max_attempts`
/// промоутится в `.manual_clear` ровно так же, как и любой другой Action
/// failure. То есть бесконечно зависшая команда не выйдет за пределы
/// retry-окна сама собой.
const COMMAND_DISPATCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Шаг polling-цикла `try_wait`. Тот же baseline, что в health_check::cmd
/// и process_signal::apply — 50 мс достаточно, чтобы быстро ловить
/// завершение обычной команды и не нагружать ядро лишними сис-вызовами.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Максимум stderr, который мы возвращаем в сообщении ошибки — 256 байт.
/// Длиннее всё равно не помещается в `tracing::warn`-строку без обрезки.
const STDERR_EXCERPT_LIMIT: usize = 256;

/// Маркер init-system для маршрутизации. Совпадает с `DeferEntry::init_system`.
const INIT_SYSTEMD: &str = "systemd";
const INIT_RUNR: &str = "runr";

/// Production-реализация [`DispatchClient`]: маршрутизирует записи
/// журнала defers к соответствующему backend'у (systemd dbus, runr HTTP,
/// локальный shell).
///
/// Поля `runr`/`systemd` опциональны: CLI выставляет тот handle, для
/// которого определена init-система. Если запись требует backend'а,
/// которого нет — [`DispatchClient::dispatch`] возвращает
/// `Err(DispatchError::ClientUnavailable)`. Семантика идентична случаю
/// «бэкенд временно недоступен»: запись остаётся в журнале без bump'а
/// attempt'а до следующего цикла.
pub struct RealDispatchClient {
    pub runr: Option<Arc<dyn RunrHandle>>,
    pub systemd: Option<Arc<dyn SystemdHandle>>,
}

impl RealDispatchClient {
    /// Создаёт диспатчер с опциональными handle'ами.
    pub fn new(runr: Option<Arc<dyn RunrHandle>>, systemd: Option<Arc<dyn SystemdHandle>>) -> Self {
        Self { runr, systemd }
    }

    fn dispatch_systemd(&self, entry: &DeferEntry) -> Result<(), DispatchError> {
        let Some(handle) = self.systemd.as_ref() else {
            return Err(DispatchError::ClientUnavailable);
        };
        let target = entry.target.as_str();
        let result = match &entry.action {
            DeferAction::Start => handle.start_unit(target),
            DeferAction::Stop => handle.stop_unit(target),
            DeferAction::Restart => handle.restart_unit(target),
            DeferAction::Reload => handle.reload_unit(target),
            DeferAction::ReloadOrRestart => {
                // systemd-handle не имеет отдельного reload_or_restart;
                // практический эквивалент — restart, который покрывает
                // обе семантики (как и mapping в primitives).
                handle.restart_unit(target)
            }
            DeferAction::DaemonReload => handle.daemon_reload(),
            DeferAction::Command { .. } => {
                return Err(DispatchError::Action(format!(
                    "command action with init_system=systemd is invalid for entry {}",
                    entry.id
                )));
            }
            // DeferAction помечен #[non_exhaustive]; новый вариант без явной
            // поддержки лучше явно отвергнуть, чем неявно выполнить wrong action.
            _ => {
                return Err(DispatchError::Action(format!(
                    "unsupported systemd action {} for entry {}",
                    entry.action.filename_slug(),
                    entry.id
                )));
            }
        };
        map_systemd_result(result)
    }

    fn dispatch_runr(&self, entry: &DeferEntry) -> Result<(), DispatchError> {
        let Some(handle) = self.runr.as_ref() else {
            return Err(DispatchError::ClientUnavailable);
        };
        let target = entry.target.as_str();
        let result = match &entry.action {
            DeferAction::Start => handle.service_start(target, true).map(|_| ()),
            DeferAction::Stop => handle.service_stop(target, false, None).map(|_| ()),
            DeferAction::Restart => handle.service_restart(target).map(|_| ()),
            DeferAction::Reload => handle.service_reload(target).map(|_| ()),
            DeferAction::ReloadOrRestart => {
                // runr-handle также не различает reload_or_restart; restart
                // — strict superset (см. семейство dedup'ов).
                handle.service_restart(target).map(|_| ())
            }
            DeferAction::DaemonReload => handle.daemon_reload().map(|_| ()),
            DeferAction::Command { .. } => {
                return Err(DispatchError::Action(format!(
                    "command action with init_system=runr is invalid for entry {}",
                    entry.id
                )));
            }
            _ => {
                return Err(DispatchError::Action(format!(
                    "unsupported runr action {} for entry {}",
                    entry.action.filename_slug(),
                    entry.id
                )));
            }
        };
        map_runr_result(result)
    }

    fn dispatch_command(entry: &DeferEntry) -> Result<(), DispatchError> {
        let DeferAction::Command { argv } = &entry.action else {
            return Err(DispatchError::Action(format!(
                "expected Command action for entry {}, got {:?}",
                entry.id,
                entry.action.filename_slug(),
            )));
        };
        let Some((program, args)) = argv.split_first() else {
            return Err(DispatchError::Action(format!(
                "command argv is empty for entry {}",
                entry.id
            )));
        };
        run_with_timeout(program, args, COMMAND_DISPATCH_TIMEOUT)
    }
}

/// Запустить `program` с `args` в дочернем процессе и дождаться завершения
/// или превышения `timeout`. На timeout — `kill()`+`wait()` (errors на
/// kill/wait игнорируем: главное не висеть в loop'е дальше timeout'а), и
/// возвращается `DispatchError::Action("timed out ...")`. Семантика повторяет
/// `health_check::cmd::run_once` и `process_signal::apply`, чтобы поведение
/// зависших exec'ов в bosun было предсказуемым повсюду.
fn run_with_timeout(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<(), DispatchError> {
    let mut command = StdCommand::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(DispatchError::Action(format!(
                "failed to spawn command {program}: {e}"
            )));
        }
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let stderr_excerpt = read_stderr_excerpt(&mut child);
                let code = status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                return Err(DispatchError::Action(format!(
                    "command {program} exited with {code}: {stderr_excerpt}",
                )));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DispatchError::Action(format!(
                        "command {program} timed out after {}s",
                        timeout.as_secs(),
                    )));
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                return Err(DispatchError::Action(format!(
                    "try_wait error for command {program}: {e}"
                )));
            }
        }
    }
}

/// Прочитать stderr дочернего процесса с обрезкой до [`STDERR_EXCERPT_LIMIT`].
/// `child.stderr.take()` отдаёт PipeReader; на ошибке чтения возвращаем то,
/// что успели прочитать.
fn read_stderr_excerpt(child: &mut std::process::Child) -> String {
    let Some(stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::with_capacity(STDERR_EXCERPT_LIMIT);
    let _ = stderr
        .take(STDERR_EXCERPT_LIMIT as u64)
        .read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

impl DispatchClient for RealDispatchClient {
    fn dispatch(&self, entry: &DeferEntry) -> Result<(), DispatchError> {
        // DaemonReload и Command имеют свои правила:
        // - Command всегда исполняется локально (init_system игнорируется).
        // - DaemonReload идёт в соответствующий handle (systemd или runr).
        if matches!(entry.action, DeferAction::Command { .. }) {
            return Self::dispatch_command(entry);
        }
        match entry.init_system.as_str() {
            INIT_SYSTEMD => self.dispatch_systemd(entry),
            INIT_RUNR => self.dispatch_runr(entry),
            other => Err(DispatchError::Action(format!(
                "unsupported init_system {other:?} for entry {}",
                entry.id
            ))),
        }
    }
}

/// Маппинг результата runr-handle'а на [`DispatchError`].
fn map_runr_result(result: Result<(), RunrError>) -> Result<(), DispatchError> {
    match result {
        Ok(()) => Ok(()),
        Err(RunrError::Unavailable { .. }) => Err(DispatchError::ClientUnavailable),
        Err(other) => Err(DispatchError::Action(format!("{other}"))),
    }
}

/// Маппинг результата systemd-handle'а на [`DispatchError`].
fn map_systemd_result(result: Result<(), SystemdError>) -> Result<(), DispatchError> {
    match result {
        Ok(()) => Ok(()),
        Err(SystemdError::BusUnavailable { .. }) => Err(DispatchError::ClientUnavailable),
        Err(other) => Err(DispatchError::Action(format!("{other}"))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use bosun_core::defers::{
        make_id, DeferAction, DeferEntry, DeferPriority, CURRENT_SPEC_VERSION,
    };
    use bosun_handles::{
        ActionAck, DaemonInfo, RunrError, RunrHandle, ServiceStatus, SystemdError, SystemdHandle,
        TimerStatus, UnitInfo, UnitListItem,
    };
    use chrono::Utc;

    use super::*;

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

    /// Mock runr-handle: записывает вызовы и возвращает заданный результат
    /// на restart/reload.
    #[derive(Default)]
    struct MockRunr {
        calls: Mutex<Vec<String>>,
        // По умолчанию Ok; тесты с ошибкой подменяют через .with_error.
        error: Mutex<Option<RunrError>>,
    }

    impl MockRunr {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn fail_with(&self, err: RunrError) {
            *self.error.lock().unwrap() = Some(err);
        }
        fn pop_error(&self) -> Option<RunrError> {
            self.error.lock().unwrap().take()
        }
        fn ack() -> ActionAck {
            ActionAck {
                action_id: "mock-1".into(),
                accepted_at: "2026-05-19T00:00:00Z".into(),
                message: None,
            }
        }
    }

    impl RunrHandle for MockRunr {
        fn base_url(&self) -> &str {
            "http://mock"
        }
        fn daemon_info(&self) -> Result<DaemonInfo, RunrError> {
            unimplemented!()
        }
        fn daemon_reload(&self) -> Result<ActionAck, RunrError> {
            self.calls.lock().unwrap().push("daemon_reload".into());
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(MockRunr::ack())
        }
        fn service_start(&self, name: &str, _: bool) -> Result<ActionAck, RunrError> {
            self.calls.lock().unwrap().push(format!("start:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(MockRunr::ack())
        }
        fn service_stop(
            &self,
            name: &str,
            _: bool,
            _: Option<&str>,
        ) -> Result<ActionAck, RunrError> {
            self.calls.lock().unwrap().push(format!("stop:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(MockRunr::ack())
        }
        fn service_restart(&self, name: &str) -> Result<ActionAck, RunrError> {
            self.calls.lock().unwrap().push(format!("restart:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(MockRunr::ack())
        }
        fn service_reload(&self, name: &str) -> Result<ActionAck, RunrError> {
            self.calls.lock().unwrap().push(format!("reload:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(MockRunr::ack())
        }
        fn timer_start(&self, _: &str) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn timer_stop(&self, _: &str) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn timer_enable(&self, _: &str, _: bool) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn timer_disable(&self, _: &str, _: bool) -> Result<ActionAck, RunrError> {
            unimplemented!()
        }
        fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError> {
            unimplemented!()
        }
        fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError> {
            unimplemented!()
        }
        fn units_list(&self) -> Result<Vec<UnitListItem>, RunrError> {
            unimplemented!()
        }
        fn verify_restart(
            &self,
            _: &str,
            _: &ServiceStatus,
            _: Duration,
            _: Duration,
        ) -> Result<ServiceStatus, RunrError> {
            unimplemented!()
        }
        fn verify_start(
            &self,
            _: &str,
            _: Duration,
            _: Duration,
        ) -> Result<ServiceStatus, RunrError> {
            unimplemented!()
        }
    }

    /// Mock systemd-handle.
    #[derive(Default)]
    struct MockSystemd {
        calls: Mutex<Vec<String>>,
        error: Mutex<Option<SystemdError>>,
    }

    impl MockSystemd {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn fail_with(&self, err: SystemdError) {
            *self.error.lock().unwrap() = Some(err);
        }
        fn pop_error(&self) -> Option<SystemdError> {
            self.error.lock().unwrap().take()
        }
    }

    impl SystemdHandle for MockSystemd {
        fn daemon_reload(&self) -> Result<(), SystemdError> {
            self.calls.lock().unwrap().push("daemon_reload".into());
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(())
        }
        fn needs_daemon_reload(&self, _: &str) -> Result<bool, SystemdError> {
            Ok(false)
        }
        fn start_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.calls.lock().unwrap().push(format!("start:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(())
        }
        fn stop_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.calls.lock().unwrap().push(format!("stop:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(())
        }
        fn restart_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.calls.lock().unwrap().push(format!("restart:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(())
        }
        fn reload_unit(&self, name: &str) -> Result<(), SystemdError> {
            self.calls.lock().unwrap().push(format!("reload:{name}"));
            if let Some(e) = self.pop_error() {
                return Err(e);
            }
            Ok(())
        }
        fn enable_unit(&self, _: &str) -> Result<(), SystemdError> {
            unimplemented!()
        }
        fn is_unit_enabled(&self, _: &str) -> Result<bool, SystemdError> {
            unimplemented!()
        }
        fn disable_unit(&self, _: &str) -> Result<(), SystemdError> {
            unimplemented!()
        }
        fn unit_info(&self, _: &str) -> Result<UnitInfo, SystemdError> {
            unimplemented!()
        }
    }

    #[test]
    fn dispatch_systemd_restart_calls_handle() {
        let systemd = Arc::new(MockSystemd::default());
        let client = RealDispatchClient::new(None, Some(systemd.clone()));
        let entry = make_entry("systemd", DeferAction::Restart, "nginx.service");
        client.dispatch(&entry).unwrap();
        assert_eq!(systemd.calls(), vec!["restart:nginx.service"]);
    }

    #[test]
    fn dispatch_systemd_reload_calls_handle() {
        let systemd = Arc::new(MockSystemd::default());
        let client = RealDispatchClient::new(None, Some(systemd.clone()));
        let entry = make_entry("systemd", DeferAction::Reload, "nginx.service");
        client.dispatch(&entry).unwrap();
        assert_eq!(systemd.calls(), vec!["reload:nginx.service"]);
    }

    #[test]
    fn dispatch_systemd_reload_or_restart_maps_to_restart() {
        let systemd = Arc::new(MockSystemd::default());
        let client = RealDispatchClient::new(None, Some(systemd.clone()));
        let entry = make_entry("systemd", DeferAction::ReloadOrRestart, "nginx.service");
        client.dispatch(&entry).unwrap();
        assert_eq!(systemd.calls(), vec!["restart:nginx.service"]);
    }

    #[test]
    fn dispatch_runr_restart_calls_handle() {
        let runr = Arc::new(MockRunr::default());
        let client = RealDispatchClient::new(Some(runr.clone()), None);
        let entry = make_entry("runr", DeferAction::Restart, "postgres");
        client.dispatch(&entry).unwrap();
        assert_eq!(runr.calls(), vec!["restart:postgres"]);
    }

    #[test]
    fn dispatch_runr_reload_calls_handle() {
        let runr = Arc::new(MockRunr::default());
        let client = RealDispatchClient::new(Some(runr.clone()), None);
        let entry = make_entry("runr", DeferAction::Reload, "postgres");
        client.dispatch(&entry).unwrap();
        assert_eq!(runr.calls(), vec!["reload:postgres"]);
    }

    #[test]
    fn dispatch_systemd_daemon_reload_calls_handle() {
        let systemd = Arc::new(MockSystemd::default());
        let client = RealDispatchClient::new(None, Some(systemd.clone()));
        let mut entry = make_entry("systemd", DeferAction::DaemonReload, "");
        entry.priority = DeferPriority::DaemonReload;
        client.dispatch(&entry).unwrap();
        assert_eq!(systemd.calls(), vec!["daemon_reload"]);
    }

    #[test]
    fn dispatch_runr_daemon_reload_calls_handle() {
        let runr = Arc::new(MockRunr::default());
        let client = RealDispatchClient::new(Some(runr.clone()), None);
        let mut entry = make_entry("runr", DeferAction::DaemonReload, "");
        entry.priority = DeferPriority::DaemonReload;
        client.dispatch(&entry).unwrap();
        assert_eq!(runr.calls(), vec!["daemon_reload"]);
    }

    #[test]
    fn dispatch_command_executes_argv_when_exit_zero() {
        let client = RealDispatchClient::new(None, None);
        let action = DeferAction::Command {
            argv: vec!["/usr/bin/true".into()],
        };
        let entry = make_entry("", action, "smoke");
        client.dispatch(&entry).unwrap();
    }

    #[test]
    fn dispatch_command_returns_action_error_on_nonzero_exit() {
        let client = RealDispatchClient::new(None, None);
        let action = DeferAction::Command {
            argv: vec!["/usr/bin/false".into()],
        };
        let entry = make_entry("", action, "fail-smoke");
        match client.dispatch(&entry) {
            Err(DispatchError::Action(msg)) => assert!(msg.contains("/usr/bin/false")),
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_command_returns_action_error_on_missing_binary() {
        let client = RealDispatchClient::new(None, None);
        let action = DeferAction::Command {
            argv: vec!["/nonexistent/binary".into()],
        };
        let entry = make_entry("", action, "missing");
        match client.dispatch(&entry) {
            Err(DispatchError::Action(msg)) => assert!(msg.contains("/nonexistent/binary")),
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_command_returns_action_error_on_empty_argv() {
        let client = RealDispatchClient::new(None, None);
        let action = DeferAction::Command { argv: vec![] };
        let entry = make_entry("", action, "empty");
        match client.dispatch(&entry) {
            Err(DispatchError::Action(msg)) => assert!(msg.contains("argv is empty")),
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_returns_client_unavailable_when_systemd_handle_missing() {
        // entry требует systemd, но handle не подключён.
        let client = RealDispatchClient::new(None, None);
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        match client.dispatch(&entry) {
            Err(DispatchError::ClientUnavailable) => {}
            other => panic!("expected ClientUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_returns_client_unavailable_when_runr_handle_missing() {
        let client = RealDispatchClient::new(None, None);
        let entry = make_entry("runr", DeferAction::Restart, "postgres");
        match client.dispatch(&entry) {
            Err(DispatchError::ClientUnavailable) => {}
            other => panic!("expected ClientUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_maps_runr_unavailable_to_client_unavailable() {
        let runr = Arc::new(MockRunr::default());
        runr.fail_with(RunrError::Unavailable {
            base_url: "http://stub".into(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            )),
        });
        let client = RealDispatchClient::new(Some(runr.clone()), None);
        let entry = make_entry("runr", DeferAction::Restart, "postgres");
        match client.dispatch(&entry) {
            Err(DispatchError::ClientUnavailable) => {}
            other => panic!("expected ClientUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_maps_runr_apierror_to_action_error() {
        let runr = Arc::new(MockRunr::default());
        runr.fail_with(RunrError::ApiError {
            status: 500,
            body: "boom".into(),
        });
        let client = RealDispatchClient::new(Some(runr.clone()), None);
        let entry = make_entry("runr", DeferAction::Restart, "postgres");
        match client.dispatch(&entry) {
            Err(DispatchError::Action(msg)) => assert!(msg.contains("500")),
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_maps_systemd_bus_unavailable_to_client_unavailable() {
        let systemd = Arc::new(MockSystemd::default());
        // Конструируем BusUnavailable через transport-уровневую zbus-ошибку.
        // В тестах это допустимо.
        systemd.fail_with(SystemdError::BusUnavailable {
            reason: "socket missing".into(),
            source: zbus::Error::Address("bad-address".into()),
        });
        let client = RealDispatchClient::new(None, Some(systemd.clone()));
        let entry = make_entry("systemd", DeferAction::Restart, "nginx");
        match client.dispatch(&entry) {
            Err(DispatchError::ClientUnavailable) => {}
            other => panic!("expected ClientUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_maps_systemd_nosuchunit_to_action_error() {
        let systemd = Arc::new(MockSystemd::default());
        systemd.fail_with(SystemdError::NoSuchUnit("nginx.service".into()));
        let client = RealDispatchClient::new(None, Some(systemd.clone()));
        let entry = make_entry("systemd", DeferAction::Restart, "nginx.service");
        match client.dispatch(&entry) {
            Err(DispatchError::Action(msg)) => assert!(msg.contains("nginx.service")),
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_unknown_init_system_returns_action_error() {
        let client = RealDispatchClient::new(None, None);
        let entry = make_entry("openrc", DeferAction::Restart, "nginx");
        match client.dispatch(&entry) {
            Err(DispatchError::Action(msg)) => assert!(msg.contains("openrc")),
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    /// `run_with_timeout` с долгой командой и коротким timeout'ом обязан
    /// убить child и вернуть Action("timed out ..."). Замеряем wall-clock,
    /// чтобы убедиться: мы не висим до конца sleep'а. Это регрессия H4:
    /// раньше `StdCommand::output()` блокировался до завершения команды,
    /// и одна зависшая deferred-команда могла заблокировать весь
    /// `bosun apply` навсегда.
    #[test]
    fn run_with_timeout_kills_long_running_command() {
        let started = Instant::now();
        let res = run_with_timeout("sleep", &["30".to_string()], Duration::from_millis(200));
        let elapsed = started.elapsed();
        match res {
            Err(DispatchError::Action(msg)) => {
                assert!(msg.contains("timed out"), "got: {msg}");
            }
            other => panic!("expected Action(timed out), got {other:?}"),
        }
        // sleep 30 убит через ~200мс; пусть будет 5с с запасом — тест не
        // должен зависать дольше этого даже на медленной машине.
        assert!(
            elapsed < Duration::from_secs(5),
            "child должен быть убит сразу после timeout'а, заняло {elapsed:?}",
        );
    }

    #[test]
    fn run_with_timeout_returns_ok_for_fast_success() {
        let res = run_with_timeout("/usr/bin/true", &[], Duration::from_secs(2));
        assert!(matches!(res, Ok(())), "expected Ok, got {res:?}");
    }

    #[test]
    fn run_with_timeout_returns_action_error_for_nonzero_exit() {
        let res = run_with_timeout("/usr/bin/false", &[], Duration::from_secs(2));
        match res {
            Err(DispatchError::Action(msg)) => {
                assert!(msg.contains("/usr/bin/false"), "got: {msg}");
                assert!(msg.contains("exited with 1"), "got: {msg}");
            }
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    #[test]
    fn run_with_timeout_includes_stderr_excerpt_on_failure() {
        let res = run_with_timeout(
            "sh",
            &[
                "-c".to_string(),
                "echo defer-bad-marker >&2; exit 9".to_string(),
            ],
            Duration::from_secs(2),
        );
        match res {
            Err(DispatchError::Action(msg)) => {
                assert!(
                    msg.contains("defer-bad-marker"),
                    "stderr excerpt должен попасть в сообщение, got: {msg}",
                );
            }
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    /// Документируем: production-таймаут защищает replay от зависших
    /// deferred-команд. Здесь не дёргаем `client.dispatch` с `sleep 30` —
    /// тест бы реально ждал 60 секунд (константа). Конкретное поведение
    /// timeout'а проверяется юнит-тестом `run_with_timeout_kills_long_running_command`.
    #[test]
    fn command_dispatch_timeout_constant_is_sane() {
        // sanity: константа должна быть достаточно длинной для тонких
        // grep/pkill, но не такой большой, чтобы зависший exec блокировал
        // ноду на десятки минут.
        assert!(
            COMMAND_DISPATCH_TIMEOUT >= Duration::from_secs(10)
                && COMMAND_DISPATCH_TIMEOUT <= Duration::from_secs(300),
            "COMMAND_DISPATCH_TIMEOUT = {:?} вне разумного диапазона 10-300s",
            COMMAND_DISPATCH_TIMEOUT,
        );
    }
}
