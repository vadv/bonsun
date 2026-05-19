//! Apply-фаза `sysctl.reload`.
//!
//! Поток:
//! 1. NoChange → ранний return (на самом деле plan не возвращает NoChange,
//!    но мы оставляем guard для совместимости с диспатчером).
//! 2. Re-check: `path` должен существовать на момент apply. Если нет —
//!    `Apply { reason }` (это конфигурационная ошибка bundle'а).
//! 3. `sysctl -p <path>` через `SysctlBackend::reload`. Exit 0 — успех;
//!    иначе `Apply { reason }` с stderr-excerpt.
//!
//! DI: trait `SysctlBackend` — production использует `RealSysctlBackend`
//! поверх `std::process::Command`, тесты — recorder без spawn'а.

use std::path::Path;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::spec::SysctlReloadSpec;

/// Контракт исполнителя `sysctl`-операций. DI-точка для тестов.
pub trait SysctlBackend: Send + Sync {
    /// Вызвать `sysctl -p <path>`. Возвращает `Ok(())` на exit 0.
    /// Иначе — `Err(reason)` со stderr-excerpt.
    fn reload(&self, path: &Path) -> Result<(), String>;
}

/// Production-реализация: spawn `sysctl -p <path>`.
///
/// Семантика exit-code'ов `sysctl`:
/// - 0 — все строки применились.
/// - !=0 — хотя бы одна строка не применилась (unknown key, permission,
///   read-only). Возвращаем `Apply { reason }` со stderr-excerpt, чтобы
///   оператор увидел причину.
pub struct RealSysctlBackend;

impl SysctlBackend for RealSysctlBackend {
    fn reload(&self, path: &Path) -> Result<(), String> {
        use std::process::{Command, Stdio};

        let output = Command::new("sysctl")
            .arg("-p")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("spawn sysctl -p {}: {e}", path.display()))?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let excerpt = if stderr.len() > 512 {
            format!("{}…", &stderr[..512])
        } else {
            stderr.to_string()
        };
        Err(format!(
            "sysctl -p {} exit {:?}: {}",
            path.display(),
            output.status.code(),
            excerpt
        ))
    }
}

/// Главная функция apply.
pub fn run(
    backend: &dyn SysctlBackend,
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: SysctlReloadSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("sysctl.reload payload: {e}")))?;

    if ctx.cancelled_or_past_deadline() {
        return Err(PrimitiveError::Cancelled);
    }

    if !spec.path.exists() {
        return Err(PrimitiveError::Apply {
            reason: format!(
                "sysctl.reload '{}': path {} does not exist (bundle order issue?)",
                spec.name,
                spec.path.display()
            ),
        });
    }

    tracing::info!(
        resource = %spec.name,
        path = %spec.path.display(),
        "sysctl.reload: running sysctl -p",
    );

    backend
        .reload(&spec.path)
        .map_err(|reason| PrimitiveError::Apply { reason })?;

    Ok(ChangeReport::changed(format!(
        "applied sysctl from {}",
        spec.path.display()
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtx, ResourceId, ResourceKind, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct MockBackend {
        result: Result<(), String>,
        calls: Mutex<Vec<PathBuf>>,
    }

    impl MockBackend {
        fn ok() -> Self {
            Self {
                result: Ok(()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn fail(reason: &str) -> Self {
            Self {
                result: Err(reason.to_string()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl SysctlBackend for MockBackend {
        fn reload(&self, path: &Path) -> Result<(), String> {
            self.calls.lock().unwrap().push(path.to_path_buf());
            self.result.clone()
        }
    }

    fn make_ctx() -> (TempDir, ApplyCtx) {
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            defers,
            None,
            None,
        );
        (tmp, ctx)
    }

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("sysctl.reload");
        let id = ResourceId::new(&kind, "test");
        Resource {
            id,
            kind,
            spec_version: 1,
            payload,
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn update_diff() -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "apply".into(),
        }
    }

    #[test]
    fn run_no_change_returns_early() {
        let backend = MockBackend::ok();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "path": "/etc/sysctl.d/x.conf",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&backend, &r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn run_calls_sysctl_p_with_path() {
        let backend = MockBackend::ok();
        let tmp = tempfile::tempdir().unwrap();
        let conf_path = tmp.path().join("60-bosun.conf");
        std::fs::write(&conf_path, "kernel.shmmax = 1024\n").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "bosun-kernel",
            "path": conf_path,
        }));
        let (_t, ctx) = make_ctx();
        let report = run(&backend, &r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], conf_path);
    }

    #[test]
    fn run_missing_path_returns_apply_error_without_call() {
        let backend = MockBackend::ok();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "path": "/etc/sysctl.d/no-such-file-12345.conf",
        }));
        let (_t, ctx) = make_ctx();
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("does not exist"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
        // Backend не должен вызываться — bundle-ошибка ловится до spawn'а.
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn run_backend_failure_returns_apply_error() {
        let backend = MockBackend::fail("kernel.unknown-key = read-only");
        let tmp = tempfile::tempdir().unwrap();
        let conf_path = tmp.path().join("bad.conf");
        std::fs::write(&conf_path, "kernel.unknown-key = 1\n").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "path": conf_path,
        }));
        let (_t, ctx) = make_ctx();
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("kernel.unknown-key"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn run_cancelled_returns_cancelled_no_backend_call() {
        let backend = MockBackend::ok();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            cancel,
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            defers,
            None,
            None,
        );
        let r = make_resource(serde_json::json!({
            "name": "x",
            "path": "/tmp/whatever.conf",
        }));
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn run_invalid_payload_is_invalid_payload() {
        let backend = MockBackend::ok();
        let r = make_resource(serde_json::json!({ "no_path": true }));
        let (_t, ctx) = make_ctx();
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }
}
