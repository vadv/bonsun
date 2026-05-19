//! Apply-фаза `cert.tls`: re-plan, генерация, атомарная запись cert+key.
//!
//! Шаги:
//! 1. Re-plan через `decide_action_cert` — между plan и apply файл мог
//!    обновиться или, наоборот, протухнуть. NoChange → выходим без записи.
//! 2. `generator::generate(spec, now)` → пара PEM-строк.
//! 3. Атомарная запись каждого файла: `NamedTempFile::new_in(parent)` →
//!    `write_all` → `sync_all` → `set_permissions` → `chown` → `persist`.
//! 4. `chown` только если spec явно задал owner/group и процесс root.
//!    Под non-root chown «в чужого» → `ChownNotPermitted`, как в file.content.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};
use chrono::Utc;
use tempfile::NamedTempFile;
use time::OffsetDateTime;

use super::generator::{generate, GeneratedCert};
use super::plan::{decide_action_cert, Action};
use super::spec::CertTlsSpec;
use crate::file_content::{
    chown::{chown_if_needed, current_euid, resolve_group, resolve_owner},
    sha256_hex,
};

/// Главная функция apply. Возвращает `ChangeReport::no_change()` если
/// между plan и apply файлы стали корректными, иначе генерирует и пишет
/// пару cert+key.
pub fn apply(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    let spec: CertTlsSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls payload: {e}")))?;
    spec.validate()?;

    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }
    // diff используется только для решения «можно ли пропустить»; всё
    // остальное приходит из re-plan'а ниже. Оставляем переменную «как
    // принято» в других примитивах.
    let _ = diff;

    // Re-plan: тот же decide_action_cert, что и в plan. Если между plan
    // и apply файлы поправили (например, оператор положил вручную правильный
    // пара), отказываемся от мутации.
    let now = Utc::now();
    let action = decide_action_cert(&spec, now)?;
    let reason = match action {
        Action::NoChange => {
            // record_changed не вызываем — нет факта изменения.
            return Ok(ChangeReport::no_change());
        }
        Action::Create => "create".to_string(),
        Action::Renew { reason } => reason,
    };

    tracing::info!(
        cert_path = %spec.cert_path.display(),
        key_path = %spec.key_path.display(),
        reason = %reason,
        "generating self-signed certificate",
    );

    // Резолвим owner/group ДО генерации: ошибки на этом шаге дешевле,
    // чем выкинуть уже сгенерированный материал.
    let resolved_owner = resolve_optional_user(&spec.owner)?;
    let resolved_group = resolve_optional_group(&spec.group)?;

    let now_ot = OffsetDateTime::now_utc();
    let GeneratedCert { cert_pem, key_pem } = generate(&spec, now_ot)?;

    let is_root = current_euid() == 0;

    // Сначала записываем приватный ключ — он критичнее. Если запись
    // упадёт после cert, оператор увидит cert без соответствующего ключа.
    // Обратный порядок (key → cert) оставляет старый cert валидным до
    // момента, когда новый ключ уже на диске.
    write_atomic(
        &spec.key_path,
        key_pem.as_bytes(),
        spec.mode_key,
        resolved_owner,
        resolved_group,
        is_root,
    )?;
    write_atomic(
        &spec.cert_path,
        cert_pem.as_bytes(),
        spec.mode_cert,
        resolved_owner,
        resolved_group,
        is_root,
    )?;

    ctx.record_changed(&resource.id);

    let cert_sha = sha256_hex(cert_pem.as_bytes());
    Ok(ChangeReport::changed(format!(
        "wrote {} and {} (cn={}, sha256_cert={}, reason={})",
        spec.cert_path.display(),
        spec.key_path.display(),
        spec.common_name,
        cert_sha,
        reason,
    )))
}

/// Атомарная запись через `NamedTempFile`: tempfile в parent → chmod →
/// chown → persist (rename). Внутри одной FS rename атомарный.
fn write_atomic(
    target: &Path,
    body: &[u8],
    mode: u32,
    owner_uid: Option<u32>,
    group_gid: Option<u32>,
    is_root: bool,
) -> Result<(), PrimitiveError> {
    let parent = target.parent().ok_or_else(|| {
        PrimitiveError::InvalidPayload(format!(
            "cert.tls: target {} has no parent directory",
            target.display(),
        ))
    })?;
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| PrimitiveError::Io {
            context: format!("create_dir_all {}", parent.display()),
            source: e,
        })?;
    }

    let mut tmp = NamedTempFile::new_in(parent).map_err(|e| PrimitiveError::Io {
        context: format!("tempfile in {}", parent.display()),
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

    let perms = std::fs::Permissions::from_mode(mode & 0o7777);
    std::fs::set_permissions(tmp.path(), perms).map_err(|e| PrimitiveError::Io {
        context: format!("chmod tempfile {}", tmp.path().display()),
        source: e,
    })?;

    if owner_uid.is_some() || group_gid.is_some() {
        let final_uid = match owner_uid {
            Some(u) => u,
            None => unix_meta_uid(tmp.path())?,
        };
        let final_gid = match group_gid {
            Some(g) => g,
            None => unix_meta_gid(tmp.path())?,
        };
        chown_if_needed(tmp.path(), final_uid, final_gid, is_root)?;
    }

    let target_buf = target.to_path_buf();
    tmp.persist(&target_buf).map_err(|e| PrimitiveError::Io {
        context: format!("persist tempfile to {}", target_buf.display()),
        source: e.error,
    })?;

    Ok(())
}

fn resolve_optional_user(name: &Option<String>) -> Result<Option<u32>, PrimitiveError> {
    let Some(n) = name.as_ref() else {
        return Ok(None);
    };
    Ok(Some(resolve_owner(n)?))
}

fn resolve_optional_group(name: &Option<String>) -> Result<Option<u32>, PrimitiveError> {
    let Some(n) = name.as_ref() else {
        return Ok(None);
    };
    Ok(Some(resolve_group(n)?))
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

    use bosun_core::{ResourceId, ResourceKind, SensitiveStore};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::cert_tls::spec::CertAlgorithm;

    fn make_ctx(tmp: &tempfile::TempDir) -> ApplyCtx {
        let store = Arc::new(SensitiveStore::new());
        let defers_root = tmp.path().join("defers");
        let defers = Arc::new(bosun_core::defers::Journal::open(&defers_root).unwrap());
        ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            store,
            tmp.path().join("backup"),
            tmp.path().join("log"),
            defers,
            None,
            None,
        )
    }

    /// Сериализуем spec в json вручную: у CertTlsSpec нет derive(Serialize),
    /// потому что payload приходит в plan через Deserialize и обратной
    /// конвертации в production-коде не требуется.
    fn spec_to_payload(spec: &CertTlsSpec) -> serde_json::Value {
        let algorithm = match spec.algorithm {
            CertAlgorithm::Rsa2048 => "rsa2048",
            CertAlgorithm::Ed25519 => "ed25519",
            CertAlgorithm::EcdsaP256 => "ecdsa_p256",
        };
        serde_json::json!({
            "cert_path": spec.cert_path,
            "key_path": spec.key_path,
            "common_name": spec.common_name,
            "algorithm": algorithm,
            "days_valid": spec.days_valid,
            "renew_before_days": spec.renew_before_days,
            "owner": spec.owner,
            "group": spec.group,
            "mode_cert": spec.mode_cert,
            "mode_key": spec.mode_key,
            "subject_alt_names": spec.subject_alt_names,
        })
    }

    fn make_resource_via_json(spec: &CertTlsSpec) -> Resource {
        let kind = ResourceKind::from_static("cert.tls");
        let id = ResourceId::new(&kind, &spec.cert_path.to_string_lossy());
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: spec_to_payload(spec),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn spec_in(tmp: &tempfile::TempDir, cn: &str) -> CertTlsSpec {
        CertTlsSpec {
            cert_path: tmp.path().join("server.crt"),
            key_path: tmp.path().join("server.key"),
            common_name: cn.to_string(),
            // Ed25519 быстрее в тестах: RSA 2048 на дебаг-сборке стоит
            // ~секунды, а тестов несколько.
            algorithm: CertAlgorithm::Ed25519,
            days_valid: 365,
            renew_before_days: 30,
            owner: None,
            group: None,
            mode_cert: 0o644,
            mode_key: 0o600,
            subject_alt_names: Vec::new(),
        }
    }

    #[test]
    fn apply_creates_files_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host.example.com");
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);

        let diff = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);

        assert!(spec.cert_path.exists());
        assert!(spec.key_path.exists());

        // mode_key=0o600 — критично, тест на регрессию permissions.
        let key_perms = std::fs::metadata(&spec.key_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(key_perms, 0o600, "private key должен быть 0o600");

        let cert_perms = std::fs::metadata(&spec.cert_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            cert_perms, 0o644,
            "public cert должен быть 0o644 по умолчанию"
        );

        let cert_pem = std::fs::read_to_string(&spec.cert_path).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn apply_is_idempotent_no_change_on_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host.example.com");
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);

        // 1-й apply — генерация.
        let diff_first = Diff::Add {
            description: "create".into(),
            payload: resource.payload.clone(),
        };
        let report_first = apply(&resource, &diff_first, &ctx).unwrap();
        assert!(report_first.changed);

        let cert_before = std::fs::read(&spec.cert_path).unwrap();

        // 2-й apply — план должен сказать NoChange, файлы трогаться не должны.
        // Эмулируем повторный цикл: передаём Diff::NoChange.
        let report_second = apply(&resource, &Diff::NoChange, &ctx).unwrap();
        assert!(!report_second.changed);

        let cert_after = std::fs::read(&spec.cert_path).unwrap();
        assert_eq!(
            cert_before, cert_after,
            "cert не должен переписываться при NoChange"
        );
    }

    #[test]
    fn apply_skips_generation_when_replan_says_no_change() {
        // Trickier: даже если diff Add, файл уже корректный (race между plan и apply) →
        // re-plan вернёт NoChange и apply не должен генерировать новый cert.
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host.example.com");
        let ctx = make_ctx(&tmp);

        // Подкладываем уже-корректные файлы до apply.
        let now_ot = OffsetDateTime::now_utc();
        let pre = generate(&spec, now_ot).unwrap();
        std::fs::write(&spec.cert_path, &pre.cert_pem).unwrap();
        std::fs::write(&spec.key_path, &pre.key_pem).unwrap();
        let cert_before = std::fs::read(&spec.cert_path).unwrap();

        let resource = make_resource_via_json(&spec);
        let diff = Diff::Add {
            description: "stale plan".into(),
            payload: resource.payload.clone(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(
            !report.changed,
            "re-plan должен дать NoChange и пропустить генерацию"
        );

        let cert_after = std::fs::read(&spec.cert_path).unwrap();
        assert_eq!(cert_before, cert_after, "файл не должен переписываться");
    }

    #[test]
    fn apply_renews_when_existing_cert_near_expiry() {
        // Apply берёт `Utc::now()` через decide_action_cert, мокать часы
        // нельзя. Поэтому pre-генерируем cert «в прошлом»: not_after
        // оказывается раньше now, разница отрицательная, любой неотрицательный
        // renew_before_days гарантирует Renew.
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = spec_in(&tmp, "host.example.com");
        spec.days_valid = 1;
        spec.renew_before_days = 0;

        let ctx = make_ctx(&tmp);

        // past = now - 2 дня; days_valid=1 → not_after = past + 1d = now - 1d.
        let past = OffsetDateTime::now_utc() - time::Duration::days(2);
        let pre = generate(&spec, past).unwrap();
        std::fs::write(&spec.cert_path, &pre.cert_pem).unwrap();
        std::fs::write(&spec.key_path, &pre.key_pem).unwrap();
        let cert_before = std::fs::read(&spec.cert_path).unwrap();

        let resource = make_resource_via_json(&spec);
        let diff = Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "renew".into(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed, "expired cert должен привести к Renew");
        assert!(report.message.contains("expiry near"));

        let cert_after = std::fs::read(&spec.cert_path).unwrap();
        assert_ne!(cert_before, cert_after, "cert должен быть перевыпущен");
    }

    #[test]
    fn apply_renews_when_common_name_drifted() {
        let tmp = tempfile::tempdir().unwrap();
        // Подкладываем cert с CN=old. spec ожидает CN=new.
        let mut spec = spec_in(&tmp, "old.example.com");
        let pre = generate(&spec, OffsetDateTime::now_utc()).unwrap();
        std::fs::write(&spec.cert_path, &pre.cert_pem).unwrap();
        std::fs::write(&spec.key_path, &pre.key_pem).unwrap();
        let cert_before = std::fs::read(&spec.cert_path).unwrap();

        spec.common_name = "new.example.com".to_string();
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);

        let diff = Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "CN drift".into(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert!(report.message.contains("common_name drift"));

        let cert_after = std::fs::read(&spec.cert_path).unwrap();
        assert_ne!(cert_before, cert_after);
    }

    #[test]
    fn apply_renews_when_cert_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host.example.com");
        // Кладём мусор на оба файла.
        std::fs::write(&spec.cert_path, b"corrupt").unwrap();
        std::fs::write(&spec.key_path, b"corrupt").unwrap();

        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);
        let diff = Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "corrupt".into(),
        };
        let report = apply(&resource, &diff, &ctx).unwrap();
        assert!(report.changed);

        let cert_after = std::fs::read_to_string(&spec.cert_path).unwrap();
        assert!(cert_after.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn apply_no_change_diff_short_circuits_without_disk_read() {
        // Diff::NoChange — оркестратор уже сообщил, что план был стабилен.
        // Apply должен сразу выйти без чтения файлов и без генерации.
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host.example.com");
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);

        let report = apply(&resource, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
        // Файлы не должны появиться.
        assert!(!spec.cert_path.exists());
        assert!(!spec.key_path.exists());
    }

    #[test]
    fn apply_rejects_invalid_spec_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = spec_in(&tmp, "host");
        spec.cert_path = std::path::PathBuf::from("relative.crt");
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);
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
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("absolute")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn apply_records_changed_in_ctx_on_create() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);
        let diff = Diff::Add {
            description: "x".into(),
            payload: resource.payload.clone(),
        };
        apply(&resource, &diff, &ctx).unwrap();
        assert!(
            ctx.is_changed(&resource.id),
            "ctx.is_changed должен быть true после create",
        );
    }

    #[test]
    fn apply_does_not_record_changed_when_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);
        apply(&resource, &Diff::NoChange, &ctx).unwrap();
        assert!(
            !ctx.is_changed(&resource.id),
            "ctx.is_changed должен быть false без записи",
        );
    }

    #[test]
    fn apply_writes_pair_atomically_via_tempfile() {
        // Smoke-тест: проверяем, что после apply ни одного `.tmp`-файла
        // не осталось в parent'е. tempfile-based persist гарантирует, что
        // tempfile либо стал target'ом, либо был удалён в Drop'е.
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        let resource = make_resource_via_json(&spec);
        let ctx = make_ctx(&tmp);
        let diff = Diff::Add {
            description: "x".into(),
            payload: resource.payload.clone(),
        };
        apply(&resource, &diff, &ctx).unwrap();
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with('.') || name.ends_with(".tmp")
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "не должно остаться tempfile'ов: {leftover:?}"
        );
    }
}
