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
/// Not `Sync`. Not `Send` across threads in any meaningful way (the
/// connection is bound to the runtime worker). Wrap in `Rc` in
/// orchestrator code and pass by reference.
pub struct BlockingSystemdManager {
    rt: Runtime,
    inner: SystemdManager,
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
        Ok(Self { rt, inner })
    }

    /// Blocking `daemon_reload`.
    pub fn daemon_reload(&self) -> Result<(), SystemdError> {
        self.rt.block_on(self.inner.daemon_reload())
    }

    /// Blocking `needs_daemon_reload`.
    pub fn needs_daemon_reload(&self, unit_name: &str) -> Result<bool, SystemdError> {
        self.rt.block_on(self.inner.needs_daemon_reload(unit_name))
    }

    /// Blocking `start_unit`.
    pub fn start_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.inner.start_unit(name))
    }

    /// Blocking `stop_unit`.
    pub fn stop_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.inner.stop_unit(name))
    }

    /// Blocking `restart_unit`.
    pub fn restart_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.inner.restart_unit(name))
    }

    /// Blocking `reload_unit`.
    pub fn reload_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.inner.reload_unit(name))
    }

    /// Blocking `reload_or_restart_unit`.
    pub fn reload_or_restart_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.rt.block_on(self.inner.reload_or_restart_unit(name))
    }

    /// Blocking `enable_unit`.
    pub fn enable_unit(&self, name: &str) -> Result<(), SystemdError> {
        self.rt.block_on(self.inner.enable_unit(name))
    }

    /// Blocking `is_unit_enabled`.
    pub fn is_unit_enabled(&self, name: &str) -> Result<bool, SystemdError> {
        self.rt.block_on(self.inner.is_unit_enabled(name))
    }

    /// Blocking `disable_unit`.
    pub fn disable_unit(&self, name: &str) -> Result<(), SystemdError> {
        self.rt.block_on(self.inner.disable_unit(name))
    }

    /// Blocking `unit_info`.
    pub fn unit_info(&self, name: &str) -> Result<UnitInfo, SystemdError> {
        self.rt.block_on(self.inner.unit_info(name))
    }

    /// Blocking `wait_for_job`.
    pub fn wait_for_job(
        &self,
        handle: &JobHandle,
        unit_name: &str,
        timeout: Duration,
    ) -> Result<JobResult, SystemdError> {
        self.rt
            .block_on(self.inner.wait_for_job(handle, unit_name, timeout))
    }
}
