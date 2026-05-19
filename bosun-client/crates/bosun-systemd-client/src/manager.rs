//! Async client for `org.freedesktop.systemd1.Manager`.
//!
//! Wraps `zbus_systemd::systemd1::ManagerProxy` and exposes a stable, typed
//! surface that returns `Result<_, SystemdError>` rather than raw zbus errors.

use std::time::Duration;

use futures_util::StreamExt;
use zbus::Connection;
use zbus_systemd::systemd1::{ManagerProxy, ServiceProxy, UnitProxy};

use crate::error::SystemdError;
use crate::job_watch::{self, JobEvent};
use crate::types::{render_invocation_id, JobHandle, JobResult, UnitInfo};

/// Job-start mode passed to every state-changing Manager method. `replace`
/// supersedes any pending conflicting job; this matches chiit behaviour
/// (`manager_dbus.go:168`) and is what every production caller wants.
const JOB_MODE_REPLACE: &str = "replace";

/// Async systemd Manager client. Holds a `zbus::Connection` and a long-lived
/// `ManagerProxy<'static>`; cheap to clone via `Rc`/`Arc` if needed.
pub struct SystemdManager {
    conn: Connection,
    proxy: ManagerProxy<'static>,
}

impl SystemdManager {
    /// Open the system bus and bind the Manager proxy.
    ///
    /// Failure modes:
    /// - Bus socket missing or address invalid → `SystemdError::BusUnavailable`.
    /// - Any other dbus error during proxy setup → `SystemdError::Dbus`.
    pub async fn connect_system() -> Result<Self, SystemdError> {
        let conn = Connection::system()
            .await
            .map_err(Self::classify_connect_error)?;
        let proxy = ManagerProxy::new(&conn).await.map_err(|err| {
            if SystemdError::is_bus_unavailable(&err) {
                SystemdError::BusUnavailable {
                    reason: format!("{err}"),
                    source: err,
                }
            } else {
                SystemdError::Dbus(err)
            }
        })?;
        Ok(Self { conn, proxy })
    }

    fn classify_connect_error(err: zbus::Error) -> SystemdError {
        // Every Connection::system() failure is in practice "bus
        // unreachable". We still keep the original error attached as source
        // for postmortem.
        SystemdError::BusUnavailable {
            reason: format!("{err}"),
            source: err,
        }
    }

    /// Borrow the underlying connection. Useful for callers that want to
    /// spawn additional proxies (Unit/Service) without re-connecting.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Equivalent of `systemctl daemon-reload` — reload all unit files.
    pub async fn daemon_reload(&self) -> Result<(), SystemdError> {
        self.proxy
            .reload()
            .await
            .map_err(|err| SystemdError::from_zbus(err, "daemon_reload", ""))
    }

    /// Read `Unit.NeedDaemonReload` property for the given unit.
    ///
    /// systemd exposes `NeedDaemonReload` per-unit (the daemon-reload flag
    /// becomes true for a unit whose on-disk file has changed since the
    /// daemon last loaded it). Callers typically resolve the answer for a
    /// single hot unit and call `daemon_reload()` once if any return true.
    pub async fn needs_daemon_reload(&self, unit_name: &str) -> Result<bool, SystemdError> {
        let unit_path = self
            .proxy
            .get_unit(unit_name.to_string())
            .await
            .map_err(|err| SystemdError::from_zbus(err, "needs_daemon_reload", unit_name))?;
        let unit_proxy = UnitProxy::new(&self.conn, unit_path)
            .await
            .map_err(|err| SystemdError::from_zbus(err, "needs_daemon_reload", unit_name))?;
        unit_proxy
            .need_daemon_reload()
            .await
            .map_err(|err| SystemdError::from_zbus(err, "needs_daemon_reload", unit_name))
    }

    /// `StartUnit(name, "replace")`. Returns the job object path the caller
    /// can wait on with `wait_for_job`.
    pub async fn start_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.proxy
            .start_unit(name.to_string(), JOB_MODE_REPLACE.to_string())
            .await
            .map(JobHandle)
            .map_err(|err| SystemdError::from_zbus(err, "start_unit", name))
    }

    /// `StopUnit(name, "replace")`.
    pub async fn stop_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.proxy
            .stop_unit(name.to_string(), JOB_MODE_REPLACE.to_string())
            .await
            .map(JobHandle)
            .map_err(|err| SystemdError::from_zbus(err, "stop_unit", name))
    }

    /// `RestartUnit(name, "replace")`.
    pub async fn restart_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.proxy
            .restart_unit(name.to_string(), JOB_MODE_REPLACE.to_string())
            .await
            .map(JobHandle)
            .map_err(|err| SystemdError::from_zbus(err, "restart_unit", name))
    }

    /// `ReloadUnit(name, "replace")`.
    pub async fn reload_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.proxy
            .reload_unit(name.to_string(), JOB_MODE_REPLACE.to_string())
            .await
            .map(JobHandle)
            .map_err(|err| SystemdError::from_zbus(err, "reload_unit", name))
    }

    /// `ReloadOrRestartUnit(name, "replace")`.
    pub async fn reload_or_restart_unit(&self, name: &str) -> Result<JobHandle, SystemdError> {
        self.proxy
            .reload_or_restart_unit(name.to_string(), JOB_MODE_REPLACE.to_string())
            .await
            .map(JobHandle)
            .map_err(|err| SystemdError::from_zbus(err, "reload_or_restart_unit", name))
    }

    /// `EnableUnitFiles([name], runtime=false, force=true)`. Force-enables
    /// the unit; matches the chiit-equivalent behaviour for idempotent
    /// re-enable.
    ///
    /// Returns nothing — the `(carries_install_info, changes)` tuple is
    /// dropped because callers care about success/failure, not the
    /// detailed change list. Add a typed return only when a caller needs
    /// it.
    pub async fn enable_unit(&self, name: &str) -> Result<(), SystemdError> {
        self.proxy
            .enable_unit_files(vec![name.to_string()], false, true)
            .await
            .map(|_| ())
            .map_err(|err| SystemdError::from_zbus(err, "enable_unit", name))
    }

    /// `DisableUnitFiles([name], runtime=false)`.
    pub async fn disable_unit(&self, name: &str) -> Result<(), SystemdError> {
        self.proxy
            .disable_unit_files(vec![name.to_string()], false)
            .await
            .map(|_| ())
            .map_err(|err| SystemdError::from_zbus(err, "disable_unit", name))
    }

    /// Look up a unit's runtime info: `ActiveState`, `SubState`,
    /// `InvocationID` (rendered as hex) and, when present,
    /// `ExecMainStartTimestamp`.
    ///
    /// Issues four properties calls on the resolved Unit object path. For
    /// units whose type does not expose `ExecMainStartTimestamp` (timers,
    /// sockets, mounts, ...), the property fetch fails and the value is
    /// returned as `None`.
    pub async fn unit_info(&self, name: &str) -> Result<UnitInfo, SystemdError> {
        let unit_path = self
            .proxy
            .get_unit(name.to_string())
            .await
            .map_err(|err| SystemdError::from_zbus(err, "unit_info", name))?;

        let unit_proxy = UnitProxy::new(&self.conn, unit_path.clone())
            .await
            .map_err(|err| SystemdError::from_zbus(err, "unit_info", name))?;

        let active_state = unit_proxy
            .active_state()
            .await
            .map_err(|err| SystemdError::from_zbus(err, "unit_info", name))?;
        let sub_state = unit_proxy
            .sub_state()
            .await
            .map_err(|err| SystemdError::from_zbus(err, "unit_info", name))?;
        let invocation_bytes = unit_proxy
            .invocation_id()
            .await
            .map_err(|err| SystemdError::from_zbus(err, "unit_info", name))?;
        let invocation_id = render_invocation_id(&invocation_bytes);

        // ExecMainStartTimestamp lives on the Service interface. For
        // non-service units the call returns UnknownInterface/UnknownProperty
        // → fall through to `None`. Genuine connection errors still surface.
        let exec_main_start_timestamp = match ServiceProxy::new(&self.conn, unit_path).await {
            Ok(svc) => match svc.exec_main_start_timestamp().await {
                Ok(v) => Some(v),
                Err(zbus::Error::MethodError(_, _, _)) => None,
                Err(other) => return Err(SystemdError::from_zbus(other, "unit_info", name)),
            },
            Err(zbus::Error::MethodError(_, _, _)) => None,
            Err(other) => return Err(SystemdError::from_zbus(other, "unit_info", name)),
        };

        Ok(UnitInfo {
            name: name.to_string(),
            active_state,
            sub_state,
            invocation_id,
            exec_main_start_timestamp,
        })
    }

    /// Wait for the systemd job to finish.
    ///
    /// Subscribes to `Manager.JobRemoved`, drains events until one matches
    /// `handle`'s object path or `timeout` fires. After observing
    /// `result == "done"`, re-queries the unit's `ActiveState` and fails
    /// with `JobFailed` if the state is `failed`. This defends against
    /// Debian bug 996911 where `JobRemoved` reports `done` for a unit that
    /// the unit manager subsequently reports as failed.
    pub async fn wait_for_job(
        &self,
        handle: &JobHandle,
        unit_name: &str,
        timeout: Duration,
    ) -> Result<JobResult, SystemdError> {
        // Subscribing before issuing the job is the right order for new
        // callers, but here the caller has already issued the job and the
        // signal may have raced ahead. zbus delivers buffered signals from
        // the moment the stream is created, not from subscription — but in
        // practice the call rate of JobRemoved is low and the race window is
        // tiny. Worst case: we miss the signal and time out.
        let raw_stream = self
            .proxy
            .receive_job_removed()
            .await
            .map_err(|err| SystemdError::from_zbus(err, "wait_for_job", unit_name))?;

        let stream = raw_stream.filter_map(|sig| async move {
            let args = sig.args().ok()?;
            Some(JobEvent {
                job_path: args.job.as_str().to_string(),
                result: args.result.clone(),
                unit: args.unit.clone(),
            })
        });

        let target_path = handle.0.as_str().to_string();
        // `Box::pin` to get a `Unpin` future that satisfies the helper's
        // bound on the stream type.
        let pinned = Box::pin(stream);
        let event = job_watch::wait_for_job_match(pinned, &target_path, timeout).await?;

        let active_state = match self.unit_info(unit_name).await {
            Ok(info) => info.active_state,
            Err(SystemdError::NoSuchUnit(_)) => {
                // Unit was unloaded between job completion and our query
                // (think `stop_unit` followed by transient unit eviction).
                // Treat as "no active_state observable" — caller's `result`
                // is still authoritative for stop-style jobs.
                String::new()
            }
            Err(err) => return Err(err),
        };

        if event.result != "done" {
            return Err(SystemdError::JobFailed {
                job: target_path,
                result: event.result,
                active_state,
            });
        }

        // Debian bug 996911 guard. We only trip on definite failure; the
        // transitional states (`activating`/`reloading`) at this point are
        // ambiguous and we let them through. A unit that ends up in
        // `inactive` after a successful stop is still success.
        if active_state == "failed" {
            return Err(SystemdError::JobFailed {
                job: target_path,
                result: event.result,
                active_state,
            });
        }

        Ok(JobResult {
            result: event.result,
            active_state,
        })
    }
}
