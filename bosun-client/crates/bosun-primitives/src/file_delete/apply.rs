//! Apply-фаза `file.delete`: снять файл/симлинк или директорию с диска.
//!
//! Идемпотентность: повторный apply на отсутствующем пути возвращает
//! `ChangeReport::no_change()`. Race ENOENT (между plan и apply) обрабатывается
//! так же — без ошибки.

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::plan::{decide_action_delete, Action};
use super::spec::FileDeleteSpec;

/// Главный entry-point apply'я.
pub fn apply(
    resource: &Resource,
    diff: &Diff,
    _ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: FileDeleteSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("file.delete payload: {e}")))?;
    spec.validate()?;

    let action = decide_action_delete(&spec.path)?;

    match action {
        Action::NoChange => Ok(ChangeReport::no_change()),
        Action::DeleteFile => remove_file(&spec),
        Action::DeleteDir => remove_directory(&spec),
    }
}

/// Снять файл/симлинк через `remove_file`. ENOENT между plan и apply
/// (race) — это успешный no-op: цель уже отсутствует, decisive — наш
/// invariant «после apply пути нет».
fn remove_file(spec: &FileDeleteSpec) -> Result<ChangeReport, PrimitiveError> {
    tracing::info!(path = %spec.path.display(), "deleting file");
    match std::fs::remove_file(&spec.path) {
        Ok(()) => Ok(ChangeReport::changed(format!(
            "deleted {}",
            spec.path.display(),
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ChangeReport::no_change()),
        Err(e) => Err(PrimitiveError::Io {
            context: format!("remove_file {}", spec.path.display()),
            source: e,
        }),
    }
}

/// Снять директорию. Если `recursive=false`, путь содержит хоть один элемент
/// — отказываемся с `Apply`-ошибкой: оператор должен явно разрешить рекурсию.
/// Пустую директорию `remove_dir_all` тоже снимает.
fn remove_directory(spec: &FileDeleteSpec) -> Result<ChangeReport, PrimitiveError> {
    if !spec.recursive && !is_empty_dir(&spec.path)? {
        return Err(PrimitiveError::Apply {
            reason: format!(
                "refusing to delete non-empty directory {} without recursive=true",
                spec.path.display(),
            ),
        });
    }

    tracing::info!(path = %spec.path.display(), recursive = spec.recursive, "deleting directory");
    match std::fs::remove_dir_all(&spec.path) {
        Ok(()) => Ok(ChangeReport::changed(format!(
            "deleted directory {}",
            spec.path.display(),
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ChangeReport::no_change()),
        Err(e) => Err(PrimitiveError::Io {
            context: format!("remove_dir_all {}", spec.path.display()),
            source: e,
        }),
    }
}

/// Пуста ли директория. `read_dir().next().is_none()` дешевле full-listing'а
/// и даёт правильный ответ. ENOENT (race) — считаем пустой, чтобы apply
/// дальше нормально отработал idempotent NoChange.
fn is_empty_dir(path: &std::path::Path) -> Result<bool, PrimitiveError> {
    match std::fs::read_dir(path) {
        Ok(mut it) => Ok(it.next().is_none()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(PrimitiveError::Io {
            context: format!("read_dir {}", path.display()),
            source: e,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bosun_core::{ResourceId, ResourceKind, SensitiveStore};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn make_ctx() -> ApplyCtx {
        let defers_root = std::env::temp_dir().join("bosun-file-delete-test-defers");
        let defers = Arc::new(bosun_core::defers::Journal::open(&defers_root).unwrap());
        ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            defers,
            None,
            None,
        )
    }

    fn make_resource(path: &std::path::Path, recursive: bool) -> Resource {
        let kind = ResourceKind::from_static("file.delete");
        let id = ResourceId::new(&kind, &path.to_string_lossy());
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "path": path.to_string_lossy(),
                "recursive": recursive,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn update_diff() -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "delete".into(),
        }
    }

    #[test]
    fn apply_no_change_on_diff_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("missing");
        let r = make_resource(&target, false);
        let ctx = make_ctx();
        let report = apply(&r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn apply_removes_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("doomed");
        std::fs::write(&target, b"bye").unwrap();
        let r = make_resource(&target, false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        assert!(!target.exists());
    }

    #[test]
    fn apply_removes_dangling_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("no-such-target"), &link).unwrap();
        let r = make_resource(&link, false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        assert!(std::fs::symlink_metadata(&link).is_err());
    }

    #[test]
    fn apply_removes_symlink_to_real_file_keeps_target() {
        // Защитный тест безопасности: симлинк удаляется, цель остаётся.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::write(&real, b"keep me").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let r = make_resource(&link, false);
        let ctx = make_ctx();
        apply(&r, &update_diff(), &ctx).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(real.exists(), "target должна остаться");
        assert_eq!(std::fs::read(&real).unwrap(), b"keep me");
    }

    #[test]
    fn apply_empty_directory_without_recursive_succeeds() {
        // Пустая директория удаляется и без recursive — это совпадает с
        // поведением `rmdir(2)`.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("empty");
        std::fs::create_dir(&dir).unwrap();
        let r = make_resource(&dir, false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        assert!(!dir.exists());
    }

    #[test]
    fn apply_non_empty_dir_without_recursive_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("child"), b"x").unwrap();
        let r = make_resource(&dir, false);
        let ctx = make_ctx();
        let err = apply(&r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => assert!(reason.contains("recursive")),
            other => panic!("expected Apply, got {other:?}"),
        }
        assert!(dir.exists(), "директория должна остаться, apply отказался");
    }

    #[test]
    fn apply_non_empty_dir_with_recursive_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::write(dir.join("sub/deeper/leaf"), b"x").unwrap();
        let r = make_resource(&dir, true);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        assert!(!dir.exists());
    }

    #[test]
    fn apply_path_already_missing_is_no_change() {
        // Race ENOENT: к моменту apply пути уже нет (внешний actor успел).
        // Это no-op, не ошибка.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("never-existed");
        let r = make_resource(&path, false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn apply_idempotent_second_run_is_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("once");
        std::fs::write(&target, b"x").unwrap();
        let r = make_resource(&target, false);
        let ctx = make_ctx();
        let r1 = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(r1.changed);
        let r2 = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(!r2.changed);
    }

    #[test]
    fn apply_rejects_relative_path() {
        let kind = ResourceKind::from_static("file.delete");
        let id = ResourceId::new(&kind, "relative");
        let r = Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({"path": "etc/foo"}),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        };
        let ctx = make_ctx();
        let err = apply(&r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("absolute")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
