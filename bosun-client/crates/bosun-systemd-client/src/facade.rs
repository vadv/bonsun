//! Synchronous facade over `SystemdManager`.
//!
//! Bosun primitives are synchronous (they live inside Starlark `apply`).
//! We don't want to push tokio runtime ownership into every call site, so
//! this module hands them a `BlockingSystemdManager` that owns a private
//! single-thread runtime and exposes blocking equivalents of every async
//! method.

use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

use crate::error::SystemdError;
use crate::manager::SystemdManager;
use crate::types::{JobHandle, JobResult, UnitInfo};

/// Blocking version of `SystemdManager`. The dbus connection is owned by a
/// private `current_thread` tokio runtime; every public method calls
/// `Runtime::block_on` to drive the async client to completion.
///
/// Thread-safety: this type is `Send + Sync` at the type level — both
/// `tokio::runtime::Runtime` and `zbus::Connection` are `Send + Sync`,
/// and `Runtime::block_on` takes `&self`. Multiple threads MAY call
/// `block_on` concurrently; tokio guarantees only one task runs at a
/// time on the single worker, so dbus calls are serialized internally.
/// `Arc<BlockingSystemdManager>` is the supported shared-ownership form.
pub struct BlockingSystemdManager {
    // `inner` хранит zbus::Connection; её background-task требует активный
    // tokio reactor в момент drop'а. Просто менять порядок полей мало —
    // Drop поля бежит вне runtime-контекста. Оборачиваем `inner` в Option,
    // чтобы пользовательский Drop мог `take()` его и `block_on(drop(...))`
    // под reactor'ом до того, как Runtime остановится. В нормальной
    // жизни `inner` всегда `Some`; `None` бывает только внутри
    // `Drop::drop` и сразу после.
    inner: Option<SystemdManager>,
    rt: Runtime,
}

impl BlockingSystemdManager {
    /// Create a single-thread tokio runtime and connect to the system bus.
    /// Returns the manager on success.
    ///
    /// The runtime is created with `enable_all` so timers used by
    /// `wait_for_job` work without further wiring.
    pub fn connect_system() -> Result<Self, SystemdError> {
        let rt = Builder::new_current_thread().enable_all().build()?;
        let inner = rt.block_on(SystemdManager::connect_system())?;
        Ok(Self {
            inner: Some(inner),
            rt,
        })
    }

    /// Внутренний accessor. Возвращает `&SystemdManager` либо
    /// `BusUnavailable`, если объект уже в стадии Drop (это невозможно
    /// штатно: ни один публичный метод не получает `&self` после Drop;
    /// проверка нужна только чтобы не использовать `panic!`).
    fn manager(&self) -> Result<&SystemdManager, SystemdError> {
        self.inner
            .as_ref()
            .ok_or_else(|| SystemdError::BusUnavailable {
                reason: "BlockingSystemdManager used after Drop".to_string(),
                source: zbus::Error::Failure("manager taken by Drop".to_string()),
            })
    }

    /// Blocking `daemon_reload`.
    pub fn daemon_reload(&self) -> Result<(), SystemdError> {
        self.rt.block_on(self.manager()?.daemon_reload())
    }

    /// Blocking `needs_daemon_reload`.
    pub fn needs_daemon_reload(&self, unit_name: &str) -> Result<bool, SystemdError> {
        self.rt
            .block_on(self.manager()?.needs_daemon_reload(unit_name))
    }

    /// Blocking `start_unit`.
    pub fn start_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.manager()?.start_unit(name))
    }

    /// Blocking `stop_unit`.
    pub fn stop_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.manager()?.stop_unit(name))
    }

    /// Blocking `restart_unit`.
    pub fn restart_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.manager()?.restart_unit(name))
    }

    /// Blocking `reload_unit`.
    pub fn reload_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.manager()?.reload_unit(name))
    }

    /// Blocking `reload_or_restart_unit`.
    pub fn reload_or_restart_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt
            .block_on(self.manager()?.reload_or_restart_unit(name))
    }

    /// Blocking `enable_unit`.
    pub fn enable_unit(&self, name: &str) -> Result<(), SystemdError> {
        self.rt.block_on(self.manager()?.enable_unit(name))
    }

    /// Blocking `is_unit_enabled`.
    pub fn is_unit_enabled(&self, name: &str) -> Result<bool, SystemdError> {
        self.rt.block_on(self.manager()?.is_unit_enabled(name))
    }

    /// Blocking `disable_unit`.
    pub fn disable_unit(&self, name: &str) -> Result<(), SystemdError> {
        self.rt.block_on(self.manager()?.disable_unit(name))
    }

    /// Blocking `unit_info`.
    pub fn unit_info(&self, name: &str) -> Result<UnitInfo, SystemdError> {
        self.rt.block_on(self.manager()?.unit_info(name))
    }

    /// Blocking `wait_for_job`.
    pub fn wait_for_job(
        &self,
        handle: &JobHandle,
        unit_name: &str,
        timeout: Duration,
    ) -> Result<JobResult, SystemdError> {
        self.rt
            .block_on(self.manager()?.wait_for_job(handle, unit_name, timeout))
    }
}

impl Drop for BlockingSystemdManager {
    fn drop(&mut self) {
        // Сбрасываем zbus::Connection (внутри SystemdManager) под
        // активным reactor'ом. Без block_on под runtime drop'у
        // background task'у zbus негде завершиться — panic «no reactor
        // running». `inner.take()` гарантированно None'ит поле, дальше
        // обычный Drop сам уничтожит Runtime.
        if let Some(inner) = self.inner.take() {
            self.rt.block_on(async move {
                drop(inner);
            });
        }
    }
}
