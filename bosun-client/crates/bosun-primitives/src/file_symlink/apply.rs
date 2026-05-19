//! Apply-фаза `file.symlink`: создать/обновить/удалить симлинк.
//!
//! При Update/Create со старым объектом по пути сначала идёт unlink
//! (или recursive remove при `force=true` для директории), потом
//! `std::os::unix::fs::symlink`.

use std::path::Path;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::plan::{decide_action_symlink, Action};
use super::spec::FileSymlinkSpec;

/// Главный entry-point apply'я.
pub fn apply(
    resource: &Resource,
    diff: &Diff,
    _ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: FileSymlinkSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("file.symlink payload: {e}")))?;
    spec.validate()?;

    let action = decide_action_symlink(&spec, &spec.path)?;

    match action {
        Action::NoChange => Ok(ChangeReport::no_change()),
        Action::Create => create_symlink(&spec),
        Action::Update => replace_symlink(&spec),
        Action::Delete => delete_symlink(&spec),
    }
}

/// Создать симлинк. parent-директорию создаём при отсутствии, чтобы
/// совпадать с поведением `file.content` и shell-инструментов (`mkdir -p`
/// перед `ln -s`).
fn create_symlink(spec: &FileSymlinkSpec) -> Result<ChangeReport, PrimitiveError> {
    ensure_parent_dir(&spec.path)?;
    tracing::info!(
        path = %spec.path.display(),
        target = %spec.target,
        "creating symlink",
    );
    std::os::unix::fs::symlink(&spec.target, &spec.path).map_err(|e| PrimitiveError::Io {
        context: format!("symlink {} -> {}", spec.path.display(), spec.target,),
        source: e,
    })?;
    Ok(ChangeReport::changed(format!(
        "created symlink {} -> {}",
        spec.path.display(),
        spec.target,
    )))
}

/// Заменить старый объект по пути на новый симлинк. Решение «что лежит» уже
/// принято в plan'е — повторяем lstat для idempotent выбора между
/// remove_file и remove_dir_all.
fn replace_symlink(spec: &FileSymlinkSpec) -> Result<ChangeReport, PrimitiveError> {
    let meta = std::fs::symlink_metadata(&spec.path).map_err(|e| PrimitiveError::Io {
        context: format!("symlink_metadata {} for update", spec.path.display()),
        source: e,
    })?;
    let ft = meta.file_type();
    if ft.is_dir() {
        // sanity-чек: directory заменяем только при force=true. Plan уже
        // отказал бы без force, здесь double-check на случай гонки.
        if !spec.force {
            return Err(PrimitiveError::Apply {
                reason: format!(
                    "file.symlink: path {} is a directory; use force=true to replace",
                    spec.path.display(),
                ),
            });
        }
        std::fs::remove_dir_all(&spec.path).map_err(|e| PrimitiveError::Io {
            context: format!("remove_dir_all {} for symlink replace", spec.path.display()),
            source: e,
        })?;
    } else {
        std::fs::remove_file(&spec.path).map_err(|e| PrimitiveError::Io {
            context: format!("remove_file {} for symlink replace", spec.path.display()),
            source: e,
        })?;
    }
    tracing::info!(
        path = %spec.path.display(),
        target = %spec.target,
        "updating symlink",
    );
    std::os::unix::fs::symlink(&spec.target, &spec.path).map_err(|e| PrimitiveError::Io {
        context: format!(
            "symlink {} -> {} after remove",
            spec.path.display(),
            spec.target,
        ),
        source: e,
    })?;
    Ok(ChangeReport::changed(format!(
        "updated symlink {} -> {}",
        spec.path.display(),
        spec.target,
    )))
}

/// Удалить симлинк (`remove_file` — атомарный unlink). ENOENT (race)
/// трактуем как успех.
fn delete_symlink(spec: &FileSymlinkSpec) -> Result<ChangeReport, PrimitiveError> {
    tracing::info!(path = %spec.path.display(), "deleting symlink");
    match std::fs::remove_file(&spec.path) {
        Ok(()) => Ok(ChangeReport::changed(format!(
            "deleted symlink {}",
            spec.path.display(),
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ChangeReport::no_change()),
        Err(e) => Err(PrimitiveError::Io {
            context: format!("remove_file {} (symlink)", spec.path.display()),
            source: e,
        }),
    }
}

/// Создать родительскую директорию при отсутствии. Идемпотентно: если
/// уже есть — no-op.
fn ensure_parent_dir(path: &Path) -> Result<(), PrimitiveError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(|e| PrimitiveError::Io {
        context: format!("create_dir_all {}", parent.display()),
        source: e,
    })?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bosun_core::{ResourceId, ResourceKind, SensitiveStore};
    use tokio_util::sync::CancellationToken;

    use super::super::spec::SymlinkState;
    use super::*;

    fn make_ctx() -> ApplyCtx {
        let defers_root = std::env::temp_dir().join("bosun-file-symlink-test-defers");
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

    fn make_resource(path: &Path, target: &str, state: &str, force: bool) -> Resource {
        let kind = ResourceKind::from_static("file.symlink");
        let id = ResourceId::new(&kind, &path.to_string_lossy());
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "path": path.to_string_lossy(),
                "target": target,
                "state": state,
                "force": force,
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
            description: "x".into(),
        }
    }

    #[test]
    fn create_makes_symlink_with_correct_target() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        let r = make_resource(&link, "/somewhere/else", "present", false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        let read = std::fs::read_link(&link).unwrap();
        assert_eq!(read, PathBuf::from("/somewhere/else"));
    }

    #[test]
    fn create_creates_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("nested/deep/link");
        let r = make_resource(&link, "/target", "present", false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        let read = std::fs::read_link(&link).unwrap();
        assert_eq!(read, PathBuf::from("/target"));
    }

    #[test]
    fn idempotent_apply_when_symlink_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/target", &link).unwrap();
        let r = make_resource(&link, "/target", "present", false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn update_replaces_wrong_symlink_target() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/old", &link).unwrap();
        let r = make_resource(&link, "/new", "present", false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        let read = std::fs::read_link(&link).unwrap();
        assert_eq!(read, PathBuf::from("/new"));
    }

    #[test]
    fn force_replaces_regular_file_with_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("was-file");
        std::fs::write(&path, b"content").unwrap();
        let r = make_resource(&path, "/target", "present", true);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        // path теперь симлинк.
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_symlink());
        let read = std::fs::read_link(&path).unwrap();
        assert_eq!(read, PathBuf::from("/target"));
    }

    #[test]
    fn force_replaces_directory_with_symlink_removing_contents() {
        // Между plan и apply по пути появилась директория (раса, или bundle
        // переставили шаги). С force=true apply должен снять директорию
        // рекурсивно и создать симлинк. Это покрывает ветку replace_symlink,
        // где meta.file_type().is_dir() == true → remove_dir_all.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("was-dir");
        std::fs::create_dir_all(path.join("inner")).unwrap();
        std::fs::write(path.join("inner/leaf"), b"keep until removed").unwrap();
        let r = make_resource(&path, "/target", "present", true);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_symlink(), "path должен стать симлинком");
        let read = std::fs::read_link(&path).unwrap();
        assert_eq!(read, PathBuf::from("/target"));
        // Содержимого директории больше нет.
        assert!(!path.join("inner").exists());
    }

    #[test]
    fn no_force_with_regular_file_returns_apply_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("was-file");
        std::fs::write(&path, b"keep").unwrap();
        let r = make_resource(&path, "/target", "present", false);
        let ctx = make_ctx();
        let err = apply(&r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => assert!(reason.contains("force")),
            other => panic!("expected Apply, got {other:?}"),
        }
        // Файл цел.
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }

    #[test]
    fn absent_deletes_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/target", &link).unwrap();
        let r = make_resource(&link, "/target", "absent", false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        assert!(std::fs::symlink_metadata(&link).is_err());
    }

    #[test]
    fn absent_when_missing_is_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("never");
        let r = make_resource(&link, "/target", "absent", false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn second_apply_is_no_change() {
        // Идемпотентность: после успешного create повторный apply того же
        // spec'а ничего не делает.
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        let r = make_resource(&link, "/target", "present", false);
        let ctx = make_ctx();
        apply(&r, &update_diff(), &ctx).unwrap();
        let report2 = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(!report2.changed);
    }

    #[test]
    fn diff_no_change_is_handled_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        let r = make_resource(&link, "/target", "present", false);
        let ctx = make_ctx();
        let report = apply(&r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn create_with_chiit_pattern_symlink_to_missing_target() {
        // Из chiit/roles/postgres/install_nix.go: bundle пишет ~50 симлинков
        // до раскатки реального дистрибутива. Цель отсутствует — апи должно
        // отработать без падения.
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("pg_ctl");
        let target = "/usr/nix/postgres17/bin/pg_ctl";
        let r = make_resource(&link, target, "present", false);
        let ctx = make_ctx();
        let report = apply(&r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        let read = std::fs::read_link(&link).unwrap();
        assert_eq!(read, PathBuf::from(target));
    }

    #[test]
    fn rejects_relative_path() {
        let kind = ResourceKind::from_static("file.symlink");
        let id = ResourceId::new(&kind, "relative");
        let r = Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({"path": "etc/foo", "target": "/y"}),
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

    #[test]
    fn deserialize_spec_default_present_when_state_omitted() {
        // Соответствует Default-импл SymlinkState — без явного state получаем
        // Present.
        let json = serde_json::json!({"path": "/x", "target": "/y"});
        let spec: FileSymlinkSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.state, SymlinkState::Present);
    }
}
