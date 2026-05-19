//! Error type for systemd1 dbus client.

use std::time::Duration;

use thiserror::Error;

/// Top-level error type for the systemd dbus client.
///
/// All async methods of `SystemdManager` and blocking methods of
/// `BlockingSystemdManager` return `Result<_, SystemdError>`. The variant set
/// is `#[non_exhaustive]` so additional categories can be added without a
/// breaking release.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SystemdError {
    /// System bus is unreachable: socket missing, address invalid, or
    /// connection refused. Distinguished from generic dbus errors so callers
    /// can fall back to a different init system (runr) without inspecting
    /// nested causes.
    #[error("system bus unavailable: {reason}")]
    BusUnavailable {
        reason: String,
        #[source]
        source: zbus::Error,
    },

    /// Any dbus error other than bus-unavailable that isn't already mapped to
    /// a more specific variant below. Wraps `zbus::Error` losslessly via
    /// `#[from]`.
    #[error("dbus error: {0}")]
    Dbus(#[from] zbus::Error),

    /// systemd reported that the unit does not exist
    /// (`org.freedesktop.systemd1.NoSuchUnit`).
    #[error("no such unit: {0}")]
    NoSuchUnit(String),

    /// polkit denied the action (`org.freedesktop.DBus.Error.AccessDenied`).
    /// Carries the action and unit name so the message can point the operator
    /// at a polkit rule.
    #[error("authorization denied for {action} on {unit}")]
    AuthorizationDenied { action: String, unit: String },

    /// systemd job completed with a non-`done` result or with `done` but the
    /// unit subsequently reported `ActiveState=failed` (see Debian bug
    /// 996911).
    #[error("job {job} failed: result={result}, active_state={active_state}")]
    JobFailed {
        job: String,
        result: String,
        active_state: String,
    },

    /// `RestartUnit` returned `JobRemoved{result=done}` but the unit's
    /// `InvocationID` did not change. The job-system claimed success but the
    /// service did not actually restart.
    #[error("restart of {unit} did not change InvocationID")]
    RestartNotObserved { unit: String },

    /// `wait_for_job` timed out before `JobRemoved` was observed.
    #[error("timed out after {0:?} waiting for systemd job")]
    Timeout(Duration),

    /// Local I/O error (env var read, etc.).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl SystemdError {
    /// Inspect a raw `zbus::Error` and classify it into the most specific
    /// `SystemdError` variant. Used by every async method that converts
    /// `zbus::Error` from a method-call result.
    ///
    /// Caller passes `action` (the method name being attempted, e.g.
    /// `"restart_unit"`) and `unit` so that `AuthorizationDenied` has
    /// context.
    pub(crate) fn from_zbus(err: zbus::Error, action: &str, unit: &str) -> Self {
        if let zbus::Error::MethodError(ref name, _, _) = err {
            let n = name.as_str();
            if n == "org.freedesktop.systemd1.NoSuchUnit" {
                return SystemdError::NoSuchUnit(unit.to_string());
            }
            if n == "org.freedesktop.DBus.Error.AccessDenied" {
                return SystemdError::AuthorizationDenied {
                    action: action.to_string(),
                    unit: unit.to_string(),
                };
            }
        }
        // Address/InputOutput at proxy-call time is "bus disappeared
        // mid-session" — still classified as bus-unavailable.
        if Self::is_bus_unavailable(&err) {
            return SystemdError::BusUnavailable {
                reason: format!("{err}"),
                source: err,
            };
        }
        SystemdError::Dbus(err)
    }

    /// True for the small subset of `zbus::Error` that indicates the bus
    /// itself is unreachable, as opposed to a method call that travelled the
    /// bus and got rejected by the peer.
    pub(crate) fn is_bus_unavailable(err: &zbus::Error) -> bool {
        matches!(
            err,
            zbus::Error::Address(_) | zbus::Error::InputOutput(_) | zbus::Error::Handshake(_)
        )
    }
}
