//! Apply-фаза `file.content`: atomic write через tempfile в той же FS.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};
use tempfile::NamedTempFile;

use super::backup::backup_with_rotation;
use super::chown::{chown_if_needed, current_euid, resolve_group, resolve_owner};
use super::plan::{matches_spec, observe_existing, sha256_hex};
use super::spec::FileContentSpec;

/// Сколько последних бэкапов хранить. Спека требует ровно 5.
const KEEP_BACKUPS: usize = 5;

/// Главная функция apply. Шаги:
/// 1. Достать сенситивные contents из `ctx.sensitive`.
/// 2. Re-stat: убедиться, что target не стал symlink между plan и apply.
/// 3. Re-plan: возможно, файл уже совпадает — отдадим NoChange.
/// 4. Backup при Update.
/// 5. Atomic write через `NamedTempFile + persist`.
/// 6. chmod + chown.
pub fn apply(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    let spec: FileContentSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content payload: {e}")))?;
    spec.validate()?;
    let target = Path::new(&spec.path);

    let sensitive = ctx.sensitive.take(&resource.id).ok_or_else(|| {
        PrimitiveError::InvalidPayload(
            "sensitive contents not in store for file.content".to_string(),
        )
    })?;
    let contents = sensitive.into_inner();

    // Sanity: sha256 sensitive должен совпадать со spec.content_sha256.
    // Иначе кто-то засунул не тот payload — это блокер.
    let actual_sha = sha256_hex(contents.as_bytes());
    if actual_sha != spec.content_sha256 {
        return Err(PrimitiveError::InvalidPayload(format!(
            "sensitive contents sha256 mismatch: store has {actual_sha}, payload has {}",
            spec.content_sha256,
        )));
    }

    // Re-stat: что лежит на пути сейчас?
    let observed = match std::fs::symlink_metadata(target) {
        Ok(meta) => {
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(PrimitiveError::InvalidTarget);
            }
            if !ft.is_file() {
                return Err(PrimitiveError::InvalidPayload(format!(
                    "target {} exists but is not a regular file",
                    target.display(),
                )));
            }
            Some(meta)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(PrimitiveError::Io {
                context: format!("symlink_metadata {} in apply", target.display()),
                source: e,
            });
        }
    };

    // Re-plan: если файл уже совпадает с желаемым — выходим NoChange, даже
    // если plan-фаза думала что нужен Update (между plan и apply файл мог
    // поправить кто-то ещё, либо плановый расчёт устарел).
    let is_update = match &observed {
        Some(meta) => {
            let obs = observe_existing(target, meta)?;
            if matches_spec(&spec, &obs)? {
                return Ok(ChangeReport::no_change());
            }
            true
        }
        None => false,
    };

    // Backup при Update — до записи. При Add бэкапить нечего.
    if is_update {
        let backup_path = backup_with_rotation(target, &ctx.backup_root, KEEP_BACKUPS)?;
        tracing::debug!(
            path = %target.display(),
            backup = %backup_path.display(),
            "backup created",
        );
    }

    // Diff используется только для лог-сообщения — на этом шаге мы уже
    // приняли решение по re-plan. Игнорируем как информацию, но оставляем
    // в сигнатуре, потому что Primitive::apply требует это.
    let _ = diff;

    // F07: для Update сохраняем существующий owner/group, если spec
    // не задал явно. Иначе bosun под root'ом превращал бы файлы в
    // root:root — гриб для postgres/nginx/etc, чьи conf-файлы owned
    // непривилегированным юзером.
    let existing_owner = observed.as_ref().map(|m| {
        use std::os::unix::fs::MetadataExt as _;
        (m.uid(), m.gid())
    });

    tracing::info!(path = %target.display(), "writing file");
    write_atomic(target, contents.as_bytes(), &spec, existing_owner)?;

    Ok(ChangeReport::changed(format!(
        "wrote {} (sha256={})",
        target.display(),
        spec.content_sha256,
    )))
}

/// Запись через `tempfile` в той же родительской директории, `fsync`, `chmod`,
/// `chown`, потом `rename`. Атомарность даёт rename внутри одной FS.
///
/// `existing_owner` (uid, gid) существующего target'а — используется
/// для случая, когда spec.owner / spec.group не заданы при Update:
/// чтобы не «обнулять» текущего владельца до процесса (root:root
/// при запуске под root).
fn write_atomic(
    target: &Path,
    body: &[u8],
    spec: &FileContentSpec,
    existing_owner: Option<(u32, u32)>,
) -> Result<(), PrimitiveError> {
    let parent = target.parent().ok_or_else(|| {
        PrimitiveError::InvalidPayload(format!(
            "target {} has no parent directory",
            target.display(),
        ))
    })?;

    // Если parent не существует — создаём (например, при первом apply на
    // свежей ноде /etc может уже быть, а /etc/myapp — ещё нет). Это совпадает
    // с поведением shell-инструментов.
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| PrimitiveError::Io {
            context: format!("create_dir_all {}", parent.display()),
            source: e,
        })?;
    }

    // tempfile_in() гарантирует ту же FS, что и `parent` — критично для
    // того, чтобы `persist` (= rename) был атомарным внутри одной точки
    // монтирования.
    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| PrimitiveError::Io {
        context: format!("tempfile in {}", parent.display()),
        source: e,
    })?;

    tmp.write_all(body).map_err(|e| PrimitiveError::Io {
        context: format!("write to tempfile in {}", parent.display()),
        source: e,
    })?;

    // Доводим до диска: write_all → fsync.
    tmp.as_file().sync_all().map_err(|e| PrimitiveError::Io {
        context: format!("sync_all on tempfile in {}", parent.display()),
        source: e,
    })?;

    // chmod до rename: пермиссии видны атомарно вместе с содержимым.
    let perms = std::fs::Permissions::from_mode(spec.mode & 0o7777);
    std::fs::set_permissions(tmp.path(), perms).map_err(|e| PrimitiveError::Io {
        context: format!("chmod tempfile {}", tmp.path().display()),
        source: e,
    })?;

    // F07: chown всегда вызывается с целевыми (uid, gid), независимо от
    // того, задал ли spec явно owner/group. Правила выбора:
    //   spec.owner=Some → resolve;
    //   spec.owner=None + existing_owner=Some → берём текущего;
    //   spec.owner=None + existing_owner=None (новый файл) → tempfile (= current euid).
    // То же для gid. Так Update без явного owner/group не сбрасывает
    // владельца в root:root.
    let want_uid = match &spec.owner {
        Some(name) => Some(resolve_owner(name)?),
        None => existing_owner.map(|(u, _)| u),
    };
    let want_gid = match &spec.group {
        Some(name) => Some(resolve_group(name)?),
        None => existing_owner.map(|(_, g)| g),
    };
    if want_uid.is_some() || want_gid.is_some() {
        // Если одна из сторон осталась None (новый файл, spec не указал
        // ту же сторону), берём текущий uid/gid tempfile'а — это euid процесса.
        let final_uid = match want_uid {
            Some(u) => u,
            None => unix_meta_uid(tmp.path())?,
        };
        let final_gid = match want_gid {
            Some(g) => g,
            None => unix_meta_gid(tmp.path())?,
        };
        let is_root = current_euid() == 0;
        chown_if_needed(tmp.path(), final_uid, final_gid, is_root)?;
    }

    let target_buf = target.to_path_buf();
    tmp.persist(&target_buf).map_err(|e| PrimitiveError::Io {
        context: format!("persist tempfile to {}", target_buf.display()),
        source: e.error,
    })?;

    Ok(())
}

fn unix_meta_uid(path: &Path) -> Result<u32, PrimitiveError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).map_err(|e| PrimitiveError::Io {
        context: format!("stat {} for uid", path.display()),
        source: e,
    })?;
    Ok(meta.uid())
}

fn unix_meta_gid(path: &Path) -> Result<u32, PrimitiveError> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(path).map_err(|e| PrimitiveError::Io {
        context: format!("stat {} for gid", path.display()),
        source: e,
    })?;
    Ok(meta.gid())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bosun_core::{ResourceId, ResourceKind, SensitivePayload, SensitiveStore};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn ctx_with_store_and_backup(
        store: Arc<SensitiveStore>,
        backup_root: std::path::PathBuf,
    ) -> ApplyCtx {
        ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            store,
            backup_root,
            std::path::PathBuf::from("/tmp"),
        )
    }

    fn make_resource(path: &str, contents: &str, mode: u32) -> (Resource, String) {
        let sha = sha256_hex(contents.as_bytes());
        let payload = serde_json::json!({
            "path": path,
            "mode": mode,
            "content_sha256": sha,
            "content_size": contents.len() as u64,
        });
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, path);
        (
            Resource {
                id,
                kind,
                spec_version: 1,
                payload,
                reload_on: Vec::new(),
                depends_on: Vec::new(),
            },
            sha,
        )
    }

    #[test]
    fn apply_creates_new_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let (resource, sha) = make_resource(target.to_str().unwrap(), "hello world", 0o644);
        let store = Arc::new(SensitiveStore::new());
        store.put(
            resource.id.clone(),
            SensitivePayload::new("hello world".into()),
        );
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));

        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert!(report.message.contains(&sha));
        assert_eq!(std::fs::read(&target).unwrap(), b"hello world");
    }

    #[test]
    fn apply_updates_existing_file_and_creates_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("etc/conf");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"old").unwrap();
        let (resource, _sha) = make_resource(target.to_str().unwrap(), "new", 0o644);
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("new".into()));
        let backup_root = tmp.path().join("backup");
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), backup_root.clone());

        let diff = Diff::Update {
            from: serde_json::json!({"sha":"old"}),
            to: serde_json::json!({"sha":"new"}),
            description: "update".into(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        // Backup создан где-то под backup_root.
        let backup_dir = backup_root.join(
            target
                .strip_prefix("/")
                .unwrap_or(target.as_path())
                .parent()
                .unwrap(),
        );
        let entries: Vec<_> = std::fs::read_dir(&backup_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn apply_no_change_when_file_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        std::fs::write(&target, b"identical").unwrap();
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&target, perms).unwrap();
        let (resource, _sha) = make_resource(target.to_str().unwrap(), "identical", 0o644);
        let store = Arc::new(SensitiveStore::new());
        store.put(
            resource.id.clone(),
            SensitivePayload::new("identical".into()),
        );
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));
        let diff = Diff::NoChange;
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn apply_rejects_symlink_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::write(&real, b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (resource, _sha) = make_resource(link.to_str().unwrap(), "y", 0o644);
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("y".into()));
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));
        let err = apply(
            &resource,
            &Diff::Update {
                from: serde_json::json!({}),
                to: serde_json::json!({}),
                description: "x".into(),
            },
            &ctx,
        )
        .unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidTarget));
    }

    #[test]
    fn apply_missing_sensitive_is_invalid_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let (resource, _sha) = make_resource(target.to_str().unwrap(), "x", 0o644);
        let store = Arc::new(SensitiveStore::new());
        // Не кладём!
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));
        let err = apply(
            &resource,
            &Diff::Add {
                description: "x".into(),
                payload: resource.payload.clone(),
            },
            &ctx,
        )
        .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("sensitive")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn apply_sha_mismatch_between_store_and_payload_is_invalid_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let (mut resource, _sha) = make_resource(target.to_str().unwrap(), "x", 0o644);
        // Подделываем sha в payload — теперь sensitive не совпадёт.
        resource.payload["content_sha256"] = serde_json::json!("0000");
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("x".into()));
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));
        let err = apply(
            &resource,
            &Diff::Add {
                description: "x".into(),
                payload: resource.payload.clone(),
            },
            &ctx,
        )
        .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("mismatch")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn apply_chown_without_root_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let sha = sha256_hex(b"body");
        let payload = serde_json::json!({
            "path": target.to_str().unwrap(),
            "mode": 0o644_u32,
            "owner": "root",
            "content_sha256": sha,
            "content_size": 4_u64,
        });
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, target.to_str().unwrap());
        let resource = Resource {
            id: id.clone(),
            kind,
            spec_version: 1,
            payload,
            reload_on: Vec::new(),
            depends_on: Vec::new(),
        };
        let store = Arc::new(SensitiveStore::new());
        store.put(id, SensitivePayload::new("body".into()));
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));

        if current_euid() == 0 {
            // Под root тест не информативен (chown пройдёт).
            return;
        }
        let err = apply(
            &resource,
            &Diff::Add {
                description: "x".into(),
                payload: serde_json::json!({}),
            },
            &ctx,
        )
        .unwrap_err();
        assert!(matches!(err, PrimitiveError::ChownNotPermitted { .. }));
    }

    #[test]
    fn apply_emits_writing_file_event() {
        use bosun_core::tracing_test_util::{install_global_router, record_events};

        install_global_router();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let (resource, _sha) = make_resource(target.to_str().unwrap(), "x", 0o644);
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("x".into()));
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));
        let diff = Diff::Add {
            description: "x".into(),
            payload: resource.payload.clone(),
        };

        let events = record_events(|| {
            apply(&resource, &diff, &ctx).unwrap();
        });

        assert!(
            events.iter().any(|e| e.contains("writing file")),
            "expected 'writing file' event; got: {events:?}",
        );
    }

    // F07 regression: ownership preservation. На non-root тестах мы не
    // можем chown'ить файл в другого пользователя; используем chown в
    // того же uid/gid процесса, чтобы протестировать «no-op preserve».

    #[test]
    fn apply_preserves_existing_owner_when_spec_omits() {
        // Симулируем существующий target с owner=current_euid/gid
        // (это то что мы можем сделать без root). Spec не указывает
        // owner/group — bosun должен оставить файл с тем же uid/gid,
        // а не пытаться сбросить в default-tempfile значения (которые
        // тоже текущие — это no-op chown в любом случае; критический
        // тест — что bosun не вернул ChownNotPermitted, пытаясь
        // chown'нуть в чужого).
        use std::os::unix::fs::MetadataExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        std::fs::write(&target, b"old content").unwrap();
        let orig_meta = std::fs::metadata(&target).unwrap();
        let orig_uid = orig_meta.uid();
        let orig_gid = orig_meta.gid();

        let (resource, _sha) = make_resource(target.to_str().unwrap(), "new content", 0o644);
        let store = Arc::new(SensitiveStore::new());
        store.put(
            resource.id.clone(),
            SensitivePayload::new("new content".into()),
        );
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));

        let diff = Diff::Update {
            from: serde_json::json!({"sha": "old"}),
            to: serde_json::json!({"sha": "new"}),
            description: "update".into(),
        };
        apply(&resource, &diff, &ctx).unwrap();

        let new_meta = std::fs::metadata(&target).unwrap();
        assert_eq!(new_meta.uid(), orig_uid, "uid должен сохраниться");
        assert_eq!(new_meta.gid(), orig_gid, "gid должен сохраниться");
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
    }

    #[test]
    fn apply_preserves_existing_uid_when_only_group_specified() {
        // spec задаёт только group (тут — текущая группа процесса по
        // имени, чтобы не упереться в ChownNotPermitted). owner должен
        // взяться из existing target, а не из process.
        use std::os::unix::fs::MetadataExt as _;

        if current_euid() == 0 {
            // Под root ChownNotPermitted не сработает, тест неинформативен
            // относительно poveden'ия non-root; пропускаем.
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        std::fs::write(&target, b"old").unwrap();
        let orig_meta = std::fs::metadata(&target).unwrap();
        let orig_uid = orig_meta.uid();
        let orig_gid = orig_meta.gid();

        // Резолвим имя текущей группы — оно должно быть в /etc/group и
        // совпадать с gid процесса. Если не получается — пропускаем
        // (контейнерные окружения).
        let group_name = match resolve_group_by_gid(orig_gid) {
            Some(n) => n,
            None => {
                eprintln!("skipping: cannot resolve group name for gid={orig_gid}");
                return;
            }
        };

        let sha = sha256_hex(b"new");
        let payload = serde_json::json!({
            "path": target.to_str().unwrap(),
            "mode": 0o644_u32,
            "group": group_name,
            "content_sha256": sha,
            "content_size": 3_u64,
        });
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, target.to_str().unwrap());
        let resource = Resource {
            id: id.clone(),
            kind,
            spec_version: 1,
            payload,
            reload_on: Vec::new(),
            depends_on: Vec::new(),
        };
        let store = Arc::new(SensitiveStore::new());
        store.put(id, SensitivePayload::new("new".into()));
        let ctx = ctx_with_store_and_backup(Arc::clone(&store), tmp.path().join("backup"));

        let diff = Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "x".into(),
        };
        apply(&resource, &diff, &ctx).unwrap();

        let new_meta = std::fs::metadata(&target).unwrap();
        assert_eq!(new_meta.uid(), orig_uid);
        assert_eq!(new_meta.gid(), orig_gid);
    }

    /// Резолвить gid → name через /etc/group. None если запись не найдена.
    fn resolve_group_by_gid(gid: u32) -> Option<String> {
        let text = std::fs::read_to_string("/etc/group").ok()?;
        for line in text.lines() {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 3 {
                if let Ok(g) = parts[2].parse::<u32>() {
                    if g == gid {
                        return Some(parts[0].to_string());
                    }
                }
            }
        }
        None
    }

    #[test]
    fn apply_backup_rotation_keeps_last_five() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("etc/conf");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let backup_root = tmp.path().join("backup");

        // 7 раз перезаписываем разным content'ом. Каждый раз — Update.
        // Бэкап создаётся с метками времени с разрешением в секунду, поэтому
        // отдельные итерации могут разделить одну секунду — добавляем явную
        // задержку, чтобы lexicographically-уникальные suffix'ы.
        for i in 0..7 {
            let body = format!("content-{i}");
            std::fs::write(&target, format!("prev-{i}")).unwrap();
            let (resource, _sha) = make_resource(target.to_str().unwrap(), &body, 0o644);
            let store = Arc::new(SensitiveStore::new());
            store.put(resource.id.clone(), SensitivePayload::new(body.clone()));
            let ctx = ctx_with_store_and_backup(Arc::clone(&store), backup_root.clone());
            apply(
                &resource,
                &Diff::Update {
                    from: serde_json::json!({}),
                    to: serde_json::json!({}),
                    description: "x".into(),
                },
                &ctx,
            )
            .unwrap();
            // 1 секунда между итерациями достаточна для разных ts-suffix.
            std::thread::sleep(Duration::from_millis(1100));
        }

        let backup_dir = backup_root.join(
            target
                .strip_prefix("/")
                .unwrap_or(target.as_path())
                .parent()
                .unwrap(),
        );
        let entries: Vec<_> = std::fs::read_dir(&backup_dir).unwrap().collect();
        // ровно 5 после rotation
        assert_eq!(
            entries.len(),
            5,
            "expected exactly 5 backups after rotation"
        );
    }
}
