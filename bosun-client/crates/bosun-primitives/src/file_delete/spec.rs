//! Десериализуемая часть payload'а `file.delete`.
//!
//! Spec лежит в `Resource.payload` после `build_payload` и читается планом/
//! apply'ем через `serde_json::from_value`. Семантика — снять с диска файл,
//! симлинк или директорию по указанному пути. Идемпотентно: повторный запуск
//! на отсутствующем пути возвращает `NoChange`.

use std::path::{Component, Path, PathBuf};

use bosun_core::PrimitiveError;
use serde::Deserialize;

/// Спека `file.delete`.
#[derive(Deserialize, Debug, Clone)]
pub struct FileDeleteSpec {
    /// Абсолютный путь к удаляемому объекту. Симлинк остаётся симлинком —
    /// удаляется именно он, а не цель (за исключением `follow_symlinks=true`,
    /// зарезервированного для будущих сценариев).
    pub path: PathBuf,
    /// Разрешить рекурсивное удаление непустой директории. Без флага apply
    /// откажется удалять директорию с содержимым и вернёт `Apply`-ошибку.
    /// Это защита от случайного `rm -rf` в bundle'е: оператор должен
    /// явно разрешить рекурсию.
    #[serde(default)]
    pub recursive: bool,
    /// Снимать симлинк как файл (без следования за ним). По умолчанию
    /// false — мы определяем тип через `symlink_metadata`, поэтому
    /// симлинк удаляется как симлинк, а не его цель. Флаг зарезервирован
    /// под будущий сценарий «удалить и саму цель», в MVP игнорируется.
    #[serde(default)]
    pub follow_symlinks: bool,
}

impl FileDeleteSpec {
    /// Проверить, что `path` — абсолютный, без `..`-сегментов и NUL-байт.
    /// Без этого манифест мог бы попросить удалить `../../etc/shadow`,
    /// что выходит за дисциплину bundle-чтения.
    pub fn validate(&self) -> Result<(), PrimitiveError> {
        let s = self.path.to_string_lossy();
        if s.as_bytes().contains(&0) {
            return Err(PrimitiveError::InvalidPayload(
                "file.delete.path contains NUL byte".to_string(),
            ));
        }
        let p = self.path.as_path();
        if !p.is_absolute() {
            return Err(PrimitiveError::InvalidPayload(format!(
                "file.delete.path must be absolute, got: {}",
                p.display(),
            )));
        }
        if has_parent_dir_segment(p) {
            return Err(PrimitiveError::InvalidPayload(format!(
                "file.delete.path contains '..' segment: {}",
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
    fn deserialize_with_required_only() {
        let json = serde_json::json!({"path": "/etc/foo"});
        let spec: FileDeleteSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.path, PathBuf::from("/etc/foo"));
        assert!(!spec.recursive);
        assert!(!spec.follow_symlinks);
    }

    #[test]
    fn deserialize_with_recursive() {
        let json = serde_json::json!({"path": "/var/cache/bosun", "recursive": true});
        let spec: FileDeleteSpec = serde_json::from_value(json).unwrap();
        assert!(spec.recursive);
    }

    #[test]
    fn validate_accepts_absolute_path() {
        let s = FileDeleteSpec {
            path: PathBuf::from("/etc/foo"),
            recursive: false,
            follow_symlinks: false,
        };
        s.validate().unwrap();
    }

    #[test]
    fn validate_rejects_relative_path() {
        let s = FileDeleteSpec {
            path: PathBuf::from("etc/foo"),
            recursive: false,
            follow_symlinks: false,
        };
        let err = s.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("absolute")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_parent_dir() {
        let s = FileDeleteSpec {
            path: PathBuf::from("/etc/../etc/foo"),
            recursive: false,
            follow_symlinks: false,
        };
        let err = s.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("'..'")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_nul_byte() {
        let s = FileDeleteSpec {
            path: PathBuf::from("/etc/foo\0bar"),
            recursive: false,
            follow_symlinks: false,
        };
        let err = s.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("NUL")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
