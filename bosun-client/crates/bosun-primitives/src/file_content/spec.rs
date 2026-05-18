//! Десериализуемая часть payload'а `file.content`.
//!
//! Само тело `contents` сюда не входит — оно идёт через `SensitiveStore`
//! из ApplyCtx. В payload остаются sha256 + size, по которым plan сравнивает
//! состояние без чтения секретов.

use std::path::{Component, Path};

use bosun_core::PrimitiveError;
use serde::Deserialize;

/// Спека `file.content`, как она лежит в `Resource.payload`.
#[derive(Deserialize, Debug, Clone)]
pub struct FileContentSpec {
    pub path: String,
    #[serde(default = "default_mode")]
    pub mode: u32,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    /// Hex-кодированный sha256 от настоящего тела `contents`.
    pub content_sha256: String,
    /// Длина тела в байтах.
    pub content_size: u64,
}

const fn default_mode() -> u32 {
    0o644
}

impl FileContentSpec {
    /// Проверить, что `path` — абсолютный и не содержит `..`-сегментов
    /// или NUL-байт. Без этого манифест может попросить запись по
    /// `../../etc/shadow.poisoned` или встроить нулевой байт, что
    /// разламывает построение backup-пути и открывает arbitrary write.
    ///
    /// Симлинки и тип файла на target проверяются в plan/apply через
    /// `symlink_metadata` — на уровне spec'а мы валидируем только саму
    /// строку пути.
    pub fn validate(&self) -> Result<(), PrimitiveError> {
        if self.path.contains('\0') {
            return Err(PrimitiveError::InvalidPayload(
                "file.content.path contains NUL byte".to_string(),
            ));
        }
        let p = Path::new(&self.path);
        if !p.is_absolute() {
            return Err(PrimitiveError::InvalidPayload(format!(
                "file.content.path must be absolute, got: {}",
                self.path,
            )));
        }
        for component in p.components() {
            if matches!(component, Component::ParentDir) {
                return Err(PrimitiveError::InvalidPayload(format!(
                    "file.content.path contains '..' segment: {}",
                    self.path,
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_with_required_fields_only() {
        let json = serde_json::json!({
            "path": "/etc/nginx/nginx.conf",
            "content_sha256": "deadbeef",
            "content_size": 4_u64,
        });
        let spec: FileContentSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.path, "/etc/nginx/nginx.conf");
        assert_eq!(spec.mode, 0o644);
        assert!(spec.owner.is_none());
        assert!(spec.group.is_none());
        assert_eq!(spec.content_sha256, "deadbeef");
        assert_eq!(spec.content_size, 4);
    }

    #[test]
    fn deserialize_with_all_fields() {
        let json = serde_json::json!({
            "path": "/etc/nginx/nginx.conf",
            "mode": 0o600,
            "owner": "root",
            "group": "www-data",
            "content_sha256": "ab12",
            "content_size": 100_u64,
        });
        let spec: FileContentSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.mode, 0o600);
        assert_eq!(spec.owner.as_deref(), Some("root"));
        assert_eq!(spec.group.as_deref(), Some("www-data"));
    }

    #[test]
    fn deserialize_missing_path_is_error() {
        let json = serde_json::json!({
            "content_sha256": "x",
            "content_size": 0_u64,
        });
        let err = serde_json::from_value::<FileContentSpec>(json).unwrap_err();
        assert!(err.to_string().contains("path"));
    }

    #[test]
    fn deserialize_missing_sha_is_error() {
        let json = serde_json::json!({
            "path": "/x",
            "content_size": 0_u64,
        });
        let err = serde_json::from_value::<FileContentSpec>(json).unwrap_err();
        assert!(err.to_string().contains("content_sha256"));
    }

    #[test]
    fn deserialize_explicit_null_owner_keeps_none() {
        // serde-default: `Option<String>` с пропущенным полем — None. Явный
        // null тоже допустим — это важно для override-флоу из inventory.
        let json = serde_json::json!({
            "path": "/x",
            "content_sha256": "x",
            "content_size": 0_u64,
            "owner": serde_json::Value::Null,
        });
        let spec: FileContentSpec = serde_json::from_value(json).unwrap();
        assert!(spec.owner.is_none());
    }

    fn spec_with_path(path: &str) -> FileContentSpec {
        FileContentSpec {
            path: path.to_string(),
            mode: 0o644,
            owner: None,
            group: None,
            content_sha256: "x".into(),
            content_size: 0,
        }
    }

    #[test]
    fn spec_accepts_absolute_path() {
        spec_with_path("/etc/nginx/nginx.conf").validate().unwrap();
    }

    #[test]
    fn spec_rejects_relative_path() {
        let err = spec_with_path("etc/foo").validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("absolute")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn spec_rejects_parent_dir() {
        // Security-critical: path-traversal через `..` запрещён.
        let err = spec_with_path("/etc/../etc/foo").validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("'..'")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn spec_rejects_nul_byte() {
        let err = spec_with_path("/etc/foo\0bar").validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("NUL")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
