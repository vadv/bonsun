//! Десериализуемая часть payload'а `file.content`.
//!
//! Само тело `contents` сюда не входит — оно идёт через `SensitiveStore`
//! из ApplyCtx. В payload остаются sha256 + size, по которым plan сравнивает
//! состояние без чтения секретов.

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
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
}
