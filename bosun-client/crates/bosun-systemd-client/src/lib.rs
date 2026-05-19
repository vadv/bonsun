//! Async (and blocking-facade) client for `org.freedesktop.systemd1` over
//! dbus.
//!
//! Phase A of the runr+systemd+defers plan. See
//! `docs/superpowers/specs/2026-05-19-bosun-runr-systemd-defers-design.md`
//! section "systemd integration via dbus" for the design rationale.
//!
//! Public surface:
//! - [`SystemdManager`] — async client, all methods return
//!   `Result<_, SystemdError>`.
//! - [`BlockingSystemdManager`] — synchronous facade for primitives that
//!   live outside tokio.
//! - [`JobHandle`], [`JobResult`], [`UnitInfo`] — typed return values.
//! - [`SystemdError`] — error enum, `#[non_exhaustive]`.

pub mod error;
pub mod facade;
pub mod job_watch;
pub mod manager;
pub mod types;

pub use error::SystemdError;
pub use facade::BlockingSystemdManager;
pub use manager::SystemdManager;
pub use types::{JobHandle, JobResult, UnitInfo};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// When the system bus socket does not exist, both the async and
    /// blocking constructors must return `SystemdError::BusUnavailable`.
    /// We point DBUS_SYSTEM_BUS_ADDRESS at a guaranteed-missing socket and
    /// verify the error category.
    #[test]
    fn connect_system_bus_unavailable_returns_typed_error() {
        // Process-global env var. No other test in this crate reads or
        // writes DBUS_SYSTEM_BUS_ADDRESS, so the parallel-test race that
        // motivates the 2024-edition unsafe marker does not apply here.
        // Original value is restored before assertions to keep cargo's
        // test harness reusable across runs that share a shell environment.
        let orig = std::env::var("DBUS_SYSTEM_BUS_ADDRESS").ok();
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/nonexistent");

        let res = BlockingSystemdManager::connect_system();

        match orig {
            Some(v) => std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", v),
            None => std::env::remove_var("DBUS_SYSTEM_BUS_ADDRESS"),
        }

        match res {
            Err(SystemdError::BusUnavailable { reason, .. }) => {
                assert!(
                    !reason.is_empty(),
                    "BusUnavailable.reason should be populated, got empty"
                );
            }
            Err(other) => panic!("expected BusUnavailable, got {other:?}"),
            Ok(_) => panic!("expected BusUnavailable, got Ok"),
        }
    }
}
