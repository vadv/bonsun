//! Apply-фаза `file.content`: atomic write через tempfile в той же FS.
//!
//! Phase H ввела расщепление flow по наличию `validate_with`:
//! - validate_with=None — старый MVP-путь через `tempfile.persist()`
//!   (atomic rename из `.tmp` файла в той же FS).
//! - validate_with=Some — render-to-`<path>.new` → validator → rename.
//!   На провал validator'а `.new` ОСТАЁТСЯ на диске для forensics,
//!   target не трогается, `record_changed` не вызывается.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bosun_core::validate::substitute_new_path;
use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource, ValidateError};
use tempfile::NamedTempFile;

use super::backup::backup_with_rotation;
use super::chown::{chown_if_needed, current_euid, resolve_group, resolve_owner};
use super::plan::{matches_spec, observe_existing, sha256_hex};
use super::spec::FileContentSpec;

/// Сколько последних бэкапов хранить. Спека требует ровно 5.
const KEEP_BACKUPS: usize = 5;

/// Таймаут на validate-команду. 30 секунд — хватает даже самым тяжёлым
/// валидаторам (`nginx -t` обычно отвечает за миллисекунды, но
/// `pg_doorman -t` на больших pool-конфигах может тянуться). Бóльше —
/// смысла нет: validator всё равно ждёт ответа sync, и admit time
/// проседает.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Главная функция apply. Шаги:
/// 1. Достать сенситивные contents из `ctx.sensitive`.
/// 2. Re-stat: убедиться, что target не стал symlink между plan и apply.
/// 3. Re-plan: возможно, файл уже совпадает — отдадим NoChange.
/// 4. Backup при Update + atomic write — выбор пути:
///    - без `validate_with`: tempfile.persist() напрямую в target;
///    - с `validate_with`: render-to-`<path>.new` → validator → rename.
/// 5. chmod + chown — внутри write-helper'ов.
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

    // Phase H: validate_with расщепляет flow.
    // - None — старый MVP-путь: backup (Update) → tempfile.persist()
    //   (атомарный rename `.tmp` → target).
    // - Some — render-to-`<path>.new` → validator → backup → rename.
    //   На provoque validator'а `<path>.new` ОСТАЁТСЯ на диске, target
    //   не трогается, `record_changed` НЕ вызывается (early return Err).
    match spec.validate_with.as_deref() {
        None => {
            if is_update {
                let backup_path = backup_with_rotation(target, &ctx.backup_root, KEEP_BACKUPS)?;
                tracing::debug!(
                    path = %target.display(),
                    backup = %backup_path.display(),
                    "backup created",
                );
            }
            write_atomic(target, contents.as_bytes(), &spec, existing_owner)?;
        }
        Some([]) => {
            // Пустой массив (`validate_with=[]`) — bundle-bug, отлавливаем
            // здесь до spawn'а. Альтернатива «считать пустой как None»
            // была бы враждебной: оператор явно вписал поле, ожидает
            // выполнения, тихий пропуск — это сюрприз.
            return Err(PrimitiveError::InvalidPayload(
                "file.content.validate_with is empty; remove the field or list a command"
                    .to_string(),
            ));
        }
        Some(argv) => {
            write_with_validation(
                target,
                contents.as_bytes(),
                &spec,
                existing_owner,
                argv,
                is_update,
                ctx,
            )?;
        }
    }

    // record_changed вызывает оркестратор на основании
    // ChangeReport::changed — здесь не дёргаем сами, чтобы не дублировать.
    Ok(ChangeReport::changed(format!(
        "wrote {} (sha256={})",
        target.display(),
        spec.content_sha256,
    )))
}

/// Phase H путь: рендерим `<path>.new`, запускаем validator, при успехе
/// делаем backup и атомарно rename'им в `<path>`.
///
/// На failure validator'а `<path>.new` остаётся на диске для forensics;
/// мы возвращаем `PrimitiveError::Validation`. Главный `apply` не
/// успеет вызвать `record_changed`, поэтому notify-источники не дёрнут
/// restart/reload пустыми руками.
fn write_with_validation(
    target: &Path,
    body: &[u8],
    spec: &FileContentSpec,
    existing_owner: Option<(u32, u32)>,
    argv: &[String],
    is_update: bool,
    ctx: &ApplyCtx,
) -> Result<(), PrimitiveError> {
    let new_path = new_path_for(target);

    let parent = target.parent().ok_or_else(|| {
        PrimitiveError::InvalidPayload(format!(
            "target {} has no parent directory",
            target.display(),
        ))
    })?;
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| PrimitiveError::Io {
            context: format!("create_dir_all {}", parent.display()),
            source: e,
        })?;
    }

    // Этап 1. Пишем в `<path>.new`: tempfile в parent → chmod → chown →
    // persist по точному имени `<path>.new`. Атомарность rename'а
    // гарантируется тем, что `.new` и target живут в одной FS.
    let written = write_to_new_path(target, &new_path, body, spec, existing_owner)?;
    debug_assert_eq!(written, new_path);

    // Этап 2. Подставляем `{new_path}` в argv и запускаем validator.
    let real_argv = substitute_new_path(argv, &new_path.to_string_lossy());
    let validator_name = real_argv
        .first()
        .cloned()
        .unwrap_or_else(|| "<empty>".to_string());

    tracing::info!(
        target = %target.display(),
        new_path = %new_path.display(),
        validator = %validator_name,
        "running validate_with",
    );

    match ctx.validator.run(&real_argv, VALIDATE_TIMEOUT) {
        Ok(()) => {
            tracing::info!(
                target = %target.display(),
                validator = %validator_name,
                "validate_with passed",
            );
        }
        Err(err) => {
            // `<path>.new` ОСТАЁТСЯ. Это hard-constraint Phase H: оператор
            // открывает его, видит rendered config и причину провала в
            // логе. Стирать здесь означало бы потерять forensics.
            tracing::warn!(
                target = %target.display(),
                new_path = %new_path.display(),
                validator = %validator_name,
                error = %err,
                "validate_with failed; .new kept for forensics",
            );
            return Err(map_validate_error(err, &validator_name));
        }
    }

    // Этап 3. Backup существующего target'а — после validation, чтобы не
    // плодить пустые backup'ы на failed apply.
    if is_update {
        let backup_path = backup_with_rotation(target, &ctx.backup_root, KEEP_BACKUPS)?;
        tracing::debug!(
            path = %target.display(),
            backup = %backup_path.display(),
            "backup created",
        );
    }

    // Этап 4. Атомарный rename `.new` → target. system call rename
    // в той же FS даёт atomicity.
    std::fs::rename(&new_path, target).map_err(|e| PrimitiveError::Io {
        context: format!("rename {} -> {}", new_path.display(), target.display()),
        source: e,
    })?;

    Ok(())
}

/// Собрать путь `<path>.new`. Если target — `/etc/nginx.conf`, .new будет
/// `/etc/nginx.conf.new` в той же родительской директории.
fn new_path_for(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".new");
    target
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(&name))
}

/// Записать body в `<path>.new`: tempfile → chmod → chown → persist
/// под точное имя `.new`. После этой функции `<path>.new` существует на
/// диске с финальными permissions/owner.
fn write_to_new_path(
    target: &Path,
    new_path: &Path,
    body: &[u8],
    spec: &FileContentSpec,
    existing_owner: Option<(u32, u32)>,
) -> Result<PathBuf, PrimitiveError> {
    let parent = target.parent().ok_or_else(|| {
        PrimitiveError::InvalidPayload(format!(
            "target {} has no parent directory",
            target.display(),
        ))
    })?;

    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| PrimitiveError::Io {
        context: format!("tempfile in {} (validate flow)", parent.display()),
        source: e,
    })?;
    tmp.write_all(body).map_err(|e| PrimitiveError::Io {
        context: format!("write to tempfile in {}", parent.display()),
        source: e,
    })?;
    tmp.as_file().sync_all().map_err(|e| PrimitiveError::Io {
        context: format!("sync_all on tempfile in {}", parent.display()),
        source: e,
    })?;

    let perms = std::fs::Permissions::from_mode(spec.mode & 0o7777);
    std::fs::set_permissions(tmp.path(), perms).map_err(|e| PrimitiveError::Io {
        context: format!("chmod tempfile {}", tmp.path().display()),
        source: e,
    })?;

    let want_uid = match &spec.owner {
        Some(name) => Some(resolve_owner(name)?),
        None => existing_owner.map(|(u, _)| u),
    };
    let want_gid = match &spec.group {
        Some(name) => Some(resolve_group(name)?),
        None => existing_owner.map(|(_, g)| g),
    };
    if want_uid.is_some() || want_gid.is_some() {
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

    let new_path_buf = new_path.to_path_buf();
    tmp.persist(&new_path_buf).map_err(|e| PrimitiveError::Io {
        context: format!("persist tempfile to {}", new_path_buf.display()),
        source: e.error,
    })?;
    Ok(new_path_buf)
}

/// Преобразование `ValidateError` в `PrimitiveError::Validation`. Excerpt
/// stderr попадает в reason; для timeout — fixed-string с длительностью,
/// для spawn-ошибки — описание io::Error.
fn map_validate_error(err: ValidateError, validator: &str) -> PrimitiveError {
    let stderr_excerpt = match err {
        ValidateError::ExitNonZero { stderr_excerpt, .. } => stderr_excerpt,
        ValidateError::Timeout(d) => format!("timeout after {d:?}"),
        ValidateError::Spawn(e) => format!("failed to spawn: {e}"),
        // ValidateError помечен `#[non_exhaustive]`: новые варианты
        // мапим в общий бакет, чтобы не падать compile-time, но и не
        // молчать в логах.
        other => format!("validator error: {other}"),
    };
    PrimitiveError::Validation {
        validator: validator.to_string(),
        stderr_excerpt,
    }
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
        // Журнал defers — фиксированная директория на tmpfs; file.content
        // не enqueue'ит defers напрямую, поле нужно только для удовлетворения
        // сигнатуры конструктора.
        let defers_root = std::env::temp_dir().join("bosun-file-test-defers");
        let defers = Arc::new(bosun_core::defers::Journal::open(&defers_root).unwrap());
        ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            store,
            backup_root,
            std::path::PathBuf::from("/tmp"),
            defers,
            None,
            None,
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
                restart_on: Vec::new(),
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
            restart_on: Vec::new(),
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
            restart_on: Vec::new(),
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

    // ===== Phase H: validate_with =====

    use std::sync::Mutex;

    use bosun_core::validate::{
        substitute_new_path, RealValidateRunner, ValidateError, ValidateRunner,
    };

    /// Mock-validator: записывает вызовы и возвращает заданный результат.
    /// Использует `MockResponse`-перечисление, чтобы тесты могли симулировать
    /// разные сценарии (success, fail с stderr, timeout, spawn-fail).
    struct MockValidator {
        calls: Mutex<Vec<Vec<String>>>,
        response: Mutex<MockResponse>,
    }

    #[derive(Clone)]
    enum MockResponse {
        Ok,
        ExitNonZero { code: i32, stderr: String },
        Timeout,
    }

    impl MockValidator {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(MockResponse::Ok),
            })
        }
        fn failing(stderr: &str) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(MockResponse::ExitNonZero {
                    code: 1,
                    stderr: stderr.to_string(),
                }),
            })
        }
        fn timing_out() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(MockResponse::Timeout),
            })
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ValidateRunner for MockValidator {
        fn run(&self, argv: &[String], _timeout: Duration) -> Result<(), ValidateError> {
            self.calls.lock().unwrap().push(argv.to_vec());
            match self.response.lock().unwrap().clone() {
                MockResponse::Ok => Ok(()),
                MockResponse::ExitNonZero { code, stderr } => Err(ValidateError::ExitNonZero {
                    exit_code: code,
                    stderr_excerpt: stderr,
                }),
                MockResponse::Timeout => Err(ValidateError::Timeout(Duration::from_secs(30))),
            }
        }
    }

    /// ApplyCtx с подменяемым validator'ом — нужен для проверки validate_with
    /// без зависимости от системного nginx/pg_doorman.
    fn ctx_with_validator(
        store: Arc<SensitiveStore>,
        backup_root: std::path::PathBuf,
        validator: Arc<dyn ValidateRunner>,
    ) -> ApplyCtx {
        let defers_root = std::env::temp_dir().join("bosun-file-test-defers");
        let defers = Arc::new(bosun_core::defers::Journal::open(&defers_root).unwrap());
        ApplyCtx::with_validator(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            store,
            backup_root,
            std::path::PathBuf::from("/tmp"),
            defers,
            None,
            None,
            validator,
        )
    }

    /// Сделать resource с заданным validate_with.
    fn make_resource_with_validate(
        path: &str,
        contents: &str,
        mode: u32,
        validate_with: Option<Vec<String>>,
    ) -> Resource {
        let sha = sha256_hex(contents.as_bytes());
        let payload = serde_json::json!({
            "path": path,
            "mode": mode,
            "content_sha256": sha,
            "content_size": contents.len() as u64,
            "validate_with": validate_with,
        });
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, path);
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

    #[test]
    fn validate_with_success_swaps_file_and_calls_validator() {
        // Validator (mock = Ok) → swap проходит, target обновлён, .new
        // исчезает (потому что rename переместил его в target).
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let validator = MockValidator::ok();
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "new content",
            0o644,
            Some(vec!["true".to_string()]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(
            resource.id.clone(),
            SensitivePayload::new("new content".into()),
        );
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
        let new_path = target.with_extension("new");
        assert!(
            !new_path.exists(),
            "<path>.new должен быть переименован в target, остался: {}",
            new_path.display(),
        );
        // Validator должен быть вызван один раз с переданным argv.
        assert_eq!(validator.calls().len(), 1);
        assert_eq!(validator.calls()[0], vec!["true"]);
    }

    #[test]
    fn validate_with_substitution_passes_real_path() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let validator = MockValidator::ok();
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "body",
            0o644,
            Some(vec![
                "sh".to_string(),
                "-c".into(),
                "echo {new_path}".into(),
                "{new_path}".into(),
            ]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("body".into()));
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        apply(&resource, &diff, &ctx).unwrap();
        let calls = validator.calls();
        assert_eq!(calls.len(), 1);
        let new_path_str = target.with_extension("new").to_string_lossy().to_string();
        assert_eq!(calls[0][0], "sh");
        assert_eq!(calls[0][1], "-c");
        // Оба плейсхолдера подставились.
        assert_eq!(calls[0][2], format!("echo {new_path_str}"));
        assert_eq!(calls[0][3], new_path_str);
    }

    #[test]
    fn validate_with_failure_keeps_new_file_and_does_not_swap() {
        // Это самый важный инвариант: validator failed → <path>.new ОСТАЁТСЯ,
        // target не изменён, ошибка PrimitiveError::Validation.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        std::fs::write(&target, b"original").unwrap();

        let validator = MockValidator::failing("syntax error at line 1");
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "broken config",
            0o644,
            Some(vec!["nginx".to_string(), "-t".into(), "{new_path}".into()]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(
            resource.id.clone(),
            SensitivePayload::new("broken config".into()),
        );
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "x".into(),
        };
        let err = apply(&resource, &diff, &ctx).unwrap_err();
        match err {
            PrimitiveError::Validation {
                validator: v,
                stderr_excerpt,
            } => {
                assert_eq!(v, "nginx");
                assert!(
                    stderr_excerpt.contains("syntax error"),
                    "stderr должен быть в reason, got: {stderr_excerpt}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        // Target НЕ изменён.
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"original",
            "target не должен быть изменён при validation failure",
        );
        // <path>.new ОСТАЁТСЯ для forensics.
        let new_path = target.with_extension("new");
        assert!(
            new_path.exists(),
            "<path>.new должен остаться для forensics: {}",
            new_path.display(),
        );
        let new_contents = std::fs::read(&new_path).unwrap();
        assert_eq!(
            new_contents, b"broken config",
            ".new должен содержать rendered config",
        );
    }

    #[test]
    fn validate_with_failure_does_not_create_backup() {
        // Failed validation НЕ должен создавать backup target'а: backup
        // делается ТОЛЬКО после успешного validation, чтобы failed apply
        // не плодил мусор. Конкретный backup_dir для этого target —
        // `<backup_root>/<target_parent>/`. Если он отсутствует или пустой,
        // значит backup_with_rotation не дёргали.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("etc/conf");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();

        let validator = MockValidator::failing("bad");
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "new",
            0o644,
            Some(vec!["false".to_string()]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("new".into()));
        let backup_root = tmp.path().join("backup");
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            backup_root.clone(),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "x".into(),
        };
        let _ = apply(&resource, &diff, &ctx).unwrap_err();
        // backup_dir = backup_root + target_parent (strip leading `/`).
        let target_parent = target.parent().unwrap();
        let backup_dir = backup_root.join(target_parent.strip_prefix("/").unwrap_or(target_parent));
        if backup_dir.exists() {
            let count = std::fs::read_dir(&backup_dir).unwrap().count();
            assert_eq!(
                count, 0,
                "backup_dir не должен содержать файлов при failed validation"
            );
        }
        // Backup-root всё ещё может быть создан (например, ensure_dirs в
        // CLI), но сам конкретный backup_dir под target — нет.
    }

    #[test]
    fn validate_with_timeout_maps_to_validation_error() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");

        let validator = MockValidator::timing_out();
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "body",
            0o644,
            Some(vec!["slow-validator".to_string()]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("body".into()));
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let err = apply(&resource, &diff, &ctx).unwrap_err();
        match err {
            PrimitiveError::Validation {
                validator: v,
                stderr_excerpt,
            } => {
                assert_eq!(v, "slow-validator");
                assert!(
                    stderr_excerpt.contains("timeout"),
                    "timeout reason должен попадать в stderr_excerpt, got: {stderr_excerpt}",
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        let new_path = target.with_extension("new");
        assert!(new_path.exists(), ".new должен остаться при timeout");
    }

    #[test]
    fn no_validate_with_uses_mvp_path() {
        // Regression: validate_with=None → старый flow через tempfile.persist().
        // Никаких <path>.new файлов не создаётся ни при успехе, ни в случае
        // failure (которого тут нет — без validator'а ничего не валится).
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let validator = MockValidator::ok(); // не должен быть вызван
        let resource =
            make_resource_with_validate(target.to_str().unwrap(), "mvp body", 0o644, None);
        let store = Arc::new(SensitiveStore::new());
        store.put(
            resource.id.clone(),
            SensitivePayload::new("mvp body".into()),
        );
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(std::fs::read(&target).unwrap(), b"mvp body");
        let new_path = target.with_extension("new");
        assert!(!new_path.exists(), "MVP-путь не должен создавать .new файл");
        // Validator вообще не вызывался.
        assert!(
            validator.calls().is_empty(),
            "validator не должен быть вызван при validate_with=None"
        );
    }

    #[test]
    fn empty_validate_with_array_is_invalid_payload() {
        // validate_with=[] — bundle-bug; должен быть отвергнут с
        // InvalidPayload до spawn'а validator'а.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let validator = MockValidator::ok();
        let resource =
            make_resource_with_validate(target.to_str().unwrap(), "x", 0o644, Some(vec![]));
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("x".into()));
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let err = apply(&resource, &diff, &ctx).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(msg.contains("validate_with"), "got: {msg}");
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
        assert!(
            validator.calls().is_empty(),
            "validator не должен быть вызван при пустом argv"
        );
    }

    #[test]
    fn validate_with_creates_new_file_with_correct_mode() {
        // Permissions на .new ставятся через `O_CREAT + chmod` до persist —
        // должны совпадать с spec.mode (0o600 в этом тесте).
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let validator = MockValidator::ok();
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "secret",
            0o600,
            Some(vec!["true".to_string()]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("secret".into()));
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            validator.clone() as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        apply(&resource, &diff, &ctx).unwrap();
        let perms = std::fs::metadata(&target).unwrap().permissions();
        assert_eq!(
            perms.mode() & 0o7777,
            0o600,
            "perms должны соответствовать spec.mode после swap'а"
        );
    }

    #[test]
    fn real_validator_with_sh_test_exists_succeeds() {
        // Smoke-тест с реальным валидатором: `test -f` на свежесозданном .new.
        // RealValidateRunner запускает sh -c, файл существует, exit=0,
        // swap проходит.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "hello",
            0o644,
            Some(vec![
                "sh".to_string(),
                "-c".into(),
                "test -f {new_path}".into(),
            ]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("hello".into()));
        // Производственный runner; mock не используем.
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            Arc::new(RealValidateRunner) as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
    }

    #[test]
    fn real_validator_with_sh_exit_1_fails() {
        // Smoke-тест: реальный validator завершается с exit=1 → Validation,
        // target не изменён, .new остаётся.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("conf");
        std::fs::write(&target, b"original").unwrap();
        let resource = make_resource_with_validate(
            target.to_str().unwrap(),
            "new",
            0o644,
            Some(vec![
                "sh".to_string(),
                "-c".into(),
                "echo bad >&2; exit 1".into(),
            ]),
        );
        let store = Arc::new(SensitiveStore::new());
        store.put(resource.id.clone(), SensitivePayload::new("new".into()));
        let ctx = ctx_with_validator(
            Arc::clone(&store),
            tmp.path().join("backup"),
            Arc::new(RealValidateRunner) as Arc<dyn ValidateRunner>,
        );
        let diff = Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "x".into(),
        };
        let err = apply(&resource, &diff, &ctx).unwrap_err();
        match err {
            PrimitiveError::Validation {
                validator,
                stderr_excerpt,
            } => {
                assert_eq!(validator, "sh");
                assert!(
                    stderr_excerpt.contains("bad"),
                    "stderr должен содержать 'bad', got: {stderr_excerpt}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        let new_path = target.with_extension("new");
        assert!(new_path.exists());
    }

    #[test]
    fn new_path_for_appends_dot_new() {
        // Регрессия: имя должно быть `<original>.new`, не `<dirname>.new`.
        let target = Path::new("/etc/nginx/nginx.conf");
        let got = new_path_for(target);
        assert_eq!(got, Path::new("/etc/nginx/nginx.conf.new"));
    }

    #[test]
    fn new_path_for_handles_extension_path() {
        // Path без расширения.
        let target = Path::new("/etc/hosts");
        let got = new_path_for(target);
        assert_eq!(got, Path::new("/etc/hosts.new"));
    }

    #[test]
    fn substitute_new_path_module_re_export_works() {
        // Защита от регрессии: substitute_new_path импортируется из
        // bosun_core::validate; если переименуют — тест укажет на нужный путь.
        let argv = vec!["x".to_string(), "{new_path}".into()];
        let out = substitute_new_path(&argv, "/y");
        assert_eq!(out, vec!["x", "/y"]);
    }
}
