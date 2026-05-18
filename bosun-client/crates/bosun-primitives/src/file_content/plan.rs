//! Хелперы plan-фазы: sha256 от bytes, сравнение с реальным состоянием файла.

use std::fs::Metadata;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

pub use bosun_core::sha256_hex;
use bosun_core::PrimitiveError;

use super::chown::{resolve_group, resolve_owner};
use super::spec::FileContentSpec;

/// Состояние существующего файла, нужное plan'у для сравнения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileObservation {
    pub sha256_hex: String,
    pub size: u64,
    /// Только разрешения (биты 0o7777). Type-бит не интересен — plan заранее
    /// отказывается работать с symlink, регулярные файлы — единственный случай.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// Прочитать `target` целиком и собрать `FileObservation`. Вызывается из
/// plan и apply (re-plan перед записью). `target` уже должен быть проверен
/// на «не symlink» — иначе `read` пойдёт через symlink и наблюдение будет
/// о другом файле.
pub fn observe_existing(target: &Path, meta: &Metadata) -> Result<FileObservation, PrimitiveError> {
    let bytes = std::fs::read(target).map_err(|e| PrimitiveError::Io {
        context: format!("read {} for plan", target.display()),
        source: e,
    })?;
    Ok(FileObservation {
        sha256_hex: sha256_hex(&bytes),
        size: bytes.len() as u64,
        mode: meta.permissions().mode() & 0o7777,
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

/// Сравнить наблюдение с желаемой спекой. Возвращает true, если состояние
/// файла совпадает с manifest'ом — план должен пометить ресурс как NoChange.
pub fn matches_spec(spec: &FileContentSpec, obs: &FileObservation) -> Result<bool, PrimitiveError> {
    if obs.sha256_hex != spec.content_sha256 || obs.size != spec.content_size {
        return Ok(false);
    }
    if (obs.mode & 0o7777) != (spec.mode & 0o7777) {
        return Ok(false);
    }
    if let Some(owner_name) = &spec.owner {
        let want_uid = resolve_owner(owner_name)?;
        if obs.uid != want_uid {
            return Ok(false);
        }
    }
    if let Some(group_name) = &spec.group {
        let want_gid = resolve_group(group_name)?;
        if obs.gid != want_gid {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn observe_existing_reads_content_and_metadata() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"abc").unwrap();
        let meta = std::fs::metadata(tmp.path()).unwrap();
        let obs = observe_existing(tmp.path(), &meta).unwrap();
        assert_eq!(obs.size, 3);
        assert_eq!(
            obs.sha256_hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(obs.uid, meta.uid());
        assert_eq!(obs.gid, meta.gid());
        assert_eq!(obs.mode, meta.permissions().mode() & 0o7777);
    }

    #[test]
    fn matches_spec_true_when_all_fields_match() {
        let spec = FileContentSpec {
            path: "/x".into(),
            mode: 0o644,
            owner: None,
            group: None,
            content_sha256: "abc".into(),
            content_size: 3,
        };
        let obs = FileObservation {
            sha256_hex: "abc".into(),
            size: 3,
            mode: 0o644,
            uid: 1000,
            gid: 1000,
        };
        assert!(matches_spec(&spec, &obs).unwrap());
    }

    #[test]
    fn matches_spec_false_on_sha_mismatch() {
        let spec = FileContentSpec {
            path: "/x".into(),
            mode: 0o644,
            owner: None,
            group: None,
            content_sha256: "abc".into(),
            content_size: 3,
        };
        let obs = FileObservation {
            sha256_hex: "different".into(),
            size: 3,
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        assert!(!matches_spec(&spec, &obs).unwrap());
    }

    #[test]
    fn matches_spec_false_on_mode_mismatch() {
        let spec = FileContentSpec {
            path: "/x".into(),
            mode: 0o600,
            owner: None,
            group: None,
            content_sha256: "abc".into(),
            content_size: 3,
        };
        let obs = FileObservation {
            sha256_hex: "abc".into(),
            size: 3,
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        assert!(!matches_spec(&spec, &obs).unwrap());
    }

    #[test]
    fn matches_spec_false_on_size_mismatch() {
        let spec = FileContentSpec {
            path: "/x".into(),
            mode: 0o644,
            owner: None,
            group: None,
            content_sha256: "abc".into(),
            content_size: 4,
        };
        let obs = FileObservation {
            sha256_hex: "abc".into(),
            size: 3,
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        assert!(!matches_spec(&spec, &obs).unwrap());
    }

    #[test]
    fn matches_spec_owner_root_when_uid_zero() {
        let spec = FileContentSpec {
            path: "/x".into(),
            mode: 0o644,
            owner: Some("root".into()),
            group: None,
            content_sha256: "abc".into(),
            content_size: 3,
        };
        let obs = FileObservation {
            sha256_hex: "abc".into(),
            size: 3,
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        assert!(matches_spec(&spec, &obs).unwrap());
    }

    #[test]
    fn matches_spec_owner_mismatch_when_uid_nonzero() {
        let spec = FileContentSpec {
            path: "/x".into(),
            mode: 0o644,
            owner: Some("root".into()),
            group: None,
            content_sha256: "abc".into(),
            content_size: 3,
        };
        let obs = FileObservation {
            sha256_hex: "abc".into(),
            size: 3,
            mode: 0o644,
            uid: 1000,
            gid: 0,
        };
        assert!(!matches_spec(&spec, &obs).unwrap());
    }
}
