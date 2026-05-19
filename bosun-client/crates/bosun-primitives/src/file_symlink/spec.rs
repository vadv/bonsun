//! Десериализуемая часть payload'а `file.symlink`.
//!
//! Spec лежит в `Resource.payload` после `build_payload` и читается планом/
//! apply'ем через `serde_json::from_value`. Семантика — создать/обновить/
//! удалить именно симлинк (file kind `S_IFLNK`), не следуя за ним при
//! проверке состояния.

use std::path::{Component, Path, PathBuf};

use bosun_core::PrimitiveError;
use serde::Deserialize;

/// Целевое состояние симлинка.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SymlinkState {
    /// Симлинк должен существовать и указывать на `target`. Если по `path`
    /// уже лежит регулярный файл/директория — нужно `force=true` для
    /// замены.
    #[default]
    Present,
    /// Симлинк должен отсутствовать. Если по пути регулярный файл —
    /// apply вернёт ошибку (мы не удаляем то, что и не создавали).
    Absent,
}

/// Спека `file.symlink`.
#[derive(Deserialize, Debug, Clone)]
pub struct FileSymlinkSpec {
    /// Где будет лежать симлинк (абсолютный путь).
    pub path: PathBuf,
    /// На что указывает симлинк. Строка, а не PathBuf: симлинк
    /// может быть на несуществующий путь (типичный паттерн в
    /// `chiit/roles/postgres/install_nix.go` — pg-bin симлинки
    /// создаются до раскатки реального дистрибутива) и на относительный
    /// путь. canonicalize здесь не уместен.
    pub target: String,
    /// Желаемое состояние. По умолчанию — `Present`.
    #[serde(default)]
    pub state: SymlinkState,
    /// Разрешить замену существующего файла/директории симлинком.
    /// По умолчанию false — apply откажется, если по `path` уже лежит
    /// не-симлинк. Это защищает от случайного `rm -rf` через bundle.
    #[serde(default)]
    pub force: bool,
}

impl FileSymlinkSpec {
    /// Проверить, что `path` абсолютный, без `..`-сегментов и NUL-байт.
    /// `target` проверяется только на NUL-байт: путь цели может быть
    /// относительным и содержать `..` (`/etc/alternatives/awk` →
    /// `/usr/bin/mawk` — частый случай в Debian).
    pub fn validate(&self) -> Result<(), PrimitiveError> {
        if self.target.contains('\0') {
            return Err(PrimitiveError::InvalidPayload(
                "file.symlink.target contains NUL byte".to_string(),
            ));
        }
        let path_str = self.path.to_string_lossy();
        if path_str.as_bytes().contains(&0) {
            return Err(PrimitiveError::InvalidPayload(
                "file.symlink.path contains NUL byte".to_string(),
            ));
        }
        let p = self.path.as_path();
        if !p.is_absolute() {
            return Err(PrimitiveError::InvalidPayload(format!(
                "file.symlink.path must be absolute, got: {}",
                p.display(),
            )));
        }
        if has_parent_dir_segment(p) {
            return Err(PrimitiveError::InvalidPayload(format!(
                "file.symlink.path contains '..' segment: {}",
                p.display(),
            )));
        }
        Ok(())
    }
}

fn has_parent_dir_segment(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::ParentDir))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_with_required_fields() {
        let json =
            serde_json::json!({"path": "/usr/local/bin/pg", "target": "/usr/nix/pg17/bin/pg"});
        let spec: FileSymlinkSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.path, PathBuf::from("/usr/local/bin/pg"));
        assert_eq!(spec.target, "/usr/nix/pg17/bin/pg");
        assert_eq!(spec.state, SymlinkState::Present);
        assert!(!spec.force);
    }

    #[test]
    fn deserialize_with_absent_state() {
        let json = serde_json::json!({
            "path": "/usr/local/bin/old",
            "target": "/somewhere",
            "state": "absent",
        });
        let spec: FileSymlinkSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.state, SymlinkState::Absent);
    }

    #[test]
    fn deserialize_with_force() {
        let json = serde_json::json!({
            "path": "/usr/local/bin/pg",
            "target": "/usr/nix/pg17/bin/pg",
            "force": true,
        });
        let spec: FileSymlinkSpec = serde_json::from_value(json).unwrap();
        assert!(spec.force);
    }

    #[test]
    fn deserialize_unknown_state_is_error() {
        let json = serde_json::json!({
            "path": "/x",
            "target": "/y",
            "state": "vanished",
        });
        let err = serde_json::from_value::<FileSymlinkSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "got: {err}");
    }

    #[test]
    fn validate_accepts_absolute_path_and_relative_target() {
        let s = FileSymlinkSpec {
            path: PathBuf::from("/etc/alternatives/awk"),
            target: "/usr/bin/mawk".to_string(),
            state: SymlinkState::Present,
            force: false,
        };
        s.validate().unwrap();
    }

    #[test]
    fn validate_rejects_relative_path() {
        let s = FileSymlinkSpec {
            path: PathBuf::from("etc/foo"),
            target: "/y".into(),
            state: SymlinkState::Present,
            force: false,
        };
        let err = s.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("absolute")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_parent_dir() {
        let s = FileSymlinkSpec {
            path: PathBuf::from("/etc/../etc/x"),
            target: "/y".into(),
            state: SymlinkState::Present,
            force: false,
        };
        let err = s.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("'..'")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_nul_byte_in_target() {
        let s = FileSymlinkSpec {
            path: PathBuf::from("/etc/x"),
            target: "/y\0bad".into(),
            state: SymlinkState::Present,
            force: false,
        };
        let err = s.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("NUL")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
