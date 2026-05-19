//! Plan-фаза `file.symlink`. Чистая decide-функция от пары
//! (`state`, текущий объект на path) → действие.
//!
//! Сравнение target'а делается через `read_link` без `canonicalize` —
//! симлинк может указывать на несуществующий или относительный путь,
//! и canonicalize либо упал бы, либо подменил семантику.

use std::path::Path;

use bosun_core::PrimitiveError;

use super::spec::{FileSymlinkSpec, SymlinkState};

/// Что собирается сделать apply.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Состояние совпадает с желаемым.
    NoChange,
    /// Симлинка нет — создать.
    Create,
    /// Симлинк есть, но указывает не туда; либо по пути не-симлинк (нужно
    /// `force=true`). Apply сам unlink'нет старое и создаст новое.
    Update,
    /// Симлинк есть, должен быть удалён (state=Absent).
    Delete,
}

/// Решение плана. Может вернуть `PrimitiveError::Apply` для случая
/// «по пути не-симлинк, force=false» — это решение не апдейтится в apply
/// (мы не хотим неявно удалять файл, который автор bundle'а не просил
/// заменять).
pub fn decide_action_symlink(
    spec: &FileSymlinkSpec,
    path: &Path,
) -> Result<Action, PrimitiveError> {
    let meta = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Пути нет — для Present это Create, для Absent это NoChange.
            return Ok(match spec.state {
                SymlinkState::Present => Action::Create,
                SymlinkState::Absent => Action::NoChange,
            });
        }
        Err(e) => {
            return Err(PrimitiveError::Io {
                context: format!("symlink_metadata {}", path.display()),
                source: e,
            });
        }
        Ok(m) => m,
    };

    let is_symlink = meta.file_type().is_symlink();

    match spec.state {
        SymlinkState::Absent => {
            if is_symlink {
                Ok(Action::Delete)
            } else {
                // По пути регулярный файл/директория — это НЕ наш объект.
                // Отказ безопаснее, чем неявное удаление.
                Err(PrimitiveError::Apply {
                    reason: format!(
                        "file.symlink: path {} exists but is not a symlink; refusing to delete",
                        path.display(),
                    ),
                })
            }
        }
        SymlinkState::Present => {
            if is_symlink {
                let current = std::fs::read_link(path).map_err(|e| PrimitiveError::Io {
                    context: format!("read_link {}", path.display()),
                    source: e,
                })?;
                // Сравнение «буква в букву»: target — Path, мы хранили его
                // как String. Конвертация через PathBuf делает сравнение
                // независимым от способа представления (`/a/b` == `/a/b`).
                let want = std::path::Path::new(&spec.target);
                if current.as_path() == want {
                    Ok(Action::NoChange)
                } else {
                    Ok(Action::Update)
                }
            } else if spec.force {
                Ok(Action::Update)
            } else {
                Err(PrimitiveError::Apply {
                    reason: format!(
                        "file.symlink: path {} exists but is not a symlink; use force=true to replace",
                        path.display(),
                    ),
                })
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn make_spec(path: &Path, target: &str, state: SymlinkState, force: bool) -> FileSymlinkSpec {
        FileSymlinkSpec {
            path: path.to_path_buf(),
            target: target.to_string(),
            state,
            force,
        }
    }

    #[test]
    fn present_and_missing_yields_create() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("link");
        let s = make_spec(&path, "/some/target", SymlinkState::Present, false);
        assert_eq!(decide_action_symlink(&s, &path).unwrap(), Action::Create);
    }

    #[test]
    fn absent_and_missing_yields_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("link");
        let s = make_spec(&path, "/some/target", SymlinkState::Absent, false);
        assert_eq!(decide_action_symlink(&s, &path).unwrap(), Action::NoChange);
    }

    #[test]
    fn present_with_correct_symlink_yields_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let target = PathBuf::from("/some/target");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let s = make_spec(
            &link,
            target.to_str().unwrap(),
            SymlinkState::Present,
            false,
        );
        assert_eq!(decide_action_symlink(&s, &link).unwrap(), Action::NoChange);
    }

    #[test]
    fn present_with_wrong_target_yields_update() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/old/target", &link).unwrap();
        let s = make_spec(&link, "/new/target", SymlinkState::Present, false);
        assert_eq!(decide_action_symlink(&s, &link).unwrap(), Action::Update);
    }

    #[test]
    fn present_with_regular_file_and_no_force_yields_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file");
        std::fs::write(&path, b"x").unwrap();
        let s = make_spec(&path, "/target", SymlinkState::Present, false);
        let err = decide_action_symlink(&s, &path).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => assert!(reason.contains("force")),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn present_with_regular_file_and_force_yields_update() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file");
        std::fs::write(&path, b"x").unwrap();
        let s = make_spec(&path, "/target", SymlinkState::Present, true);
        assert_eq!(decide_action_symlink(&s, &path).unwrap(), Action::Update);
    }

    #[test]
    fn absent_and_symlink_yields_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/some/target", &link).unwrap();
        let s = make_spec(&link, "/some/target", SymlinkState::Absent, false);
        assert_eq!(decide_action_symlink(&s, &link).unwrap(), Action::Delete);
    }

    #[test]
    fn absent_and_regular_file_yields_error() {
        // Защитный отказ: state=Absent, по пути обычный файл — это НЕ наш
        // объект, и удалять его без явной воли оператора нельзя.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file");
        std::fs::write(&path, b"x").unwrap();
        let s = make_spec(&path, "/some/target", SymlinkState::Absent, false);
        let err = decide_action_symlink(&s, &path).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => assert!(reason.contains("not a symlink")),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn present_with_dangling_symlink_pointing_correctly_yields_no_change() {
        // Цель отсутствует, но симлинк указывает буквально туда, куда
        // надо — это NoChange. Сценарий из chiit/postgres/install_nix.go:
        // bundle пишет симлинки до раскатки реального дистрибутива.
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/nonexistent/target", &link).unwrap();
        let s = make_spec(&link, "/nonexistent/target", SymlinkState::Present, false);
        assert_eq!(decide_action_symlink(&s, &link).unwrap(), Action::NoChange);
    }
}
