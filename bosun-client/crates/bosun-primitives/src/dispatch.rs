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

use std::process::Command as StdCommand;
use std::sync::Arc;

use bosun_core::defers::{DeferAction, DeferEntry, DispatchClient, DispatchError};
use bosun_handles::{RunrError, RunrHandle, SystemdError, SystemdHandle};

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
        let output = StdCommand::new(program).args(args).output();
        match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => {
                let code = out
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                let stderr_excerpt = excerpt_stderr(&out.stderr);
                Err(DispatchError::Action(format!(
                    "command {} exited with {}: {}",
                    program, code, stderr_excerpt
                )))
            }
            Err(e) => Err(DispatchError::Action(format!(
                "failed to spawn command {}: {}",
                program, e
            ))),
        }
    }
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

/// Сократить stderr для логов defer'а: первые 256 байт + ellipsis.
fn excerpt_stderr(stderr: &[u8]) -> String {
    const LIMIT: usize = 256;
    let text = String::from_utf8_lossy(stderr);
    if text.len() <= LIMIT {
        text.into_owned()
    } else {
        let mut truncated = text[..LIMIT].to_string();
        truncated.push_str("...(truncated)");
        truncated
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
}
