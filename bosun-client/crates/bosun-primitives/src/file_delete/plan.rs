//! Plan-фаза `file.delete`. Чистая функция от состояния файла на диске.
//!
//! Главное: проверка типа идёт через `symlink_metadata`, чтобы случайно
//! не follow'нуть симлинк. Без этого `metadata` следует за линком, и план
//! считает, что мы удаляем «директорию» через symlink, тогда как на самом
//! деле удалится `<symlink>` (что корректно), но проверка `is_dir()` даст
//! ложный «нужно recursive=true» отказ.

use std::path::Path;

use bosun_core::PrimitiveError;

/// Что собирается сделать apply: ничего, удалить файл/симлинк или удалить
/// директорию (с проверкой `recursive`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Пути нет — apply не делает ничего.
    NoChange,
    /// Регулярный файл или симлинк — `std::fs::remove_file`.
    DeleteFile,
    /// Директория — `std::fs::remove_dir_all` (при `recursive=true`) или
    /// `Apply`-ошибка (при `recursive=false`).
    DeleteDir,
}

/// Чистая decide-таблица. Возвращает `Action` либо `PrimitiveError::Io`
/// для нечитаемого пути. Race ENOENT (между планом и apply) трактуется как
/// `NoChange`: путь уже снят.
pub fn decide_action_delete(path: &Path) -> Result<Action, PrimitiveError> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Action::NoChange),
        Err(e) => Err(PrimitiveError::Io {
            context: format!("symlink_metadata {}", path.display()),
            source: e,
        }),
        Ok(meta) => {
            let ft = meta.file_type();
            // Симлинк, регулярный файл, fifo, socket, block-/char-устройство
            // — всё это `unlink(2)`. Директория идёт отдельной веткой.
            if ft.is_dir() {
                Ok(Action::DeleteDir)
            } else {
                Ok(Action::DeleteFile)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn not_exists_yields_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing");
        let action = decide_action_delete(&path).unwrap();
        assert_eq!(action, Action::NoChange);
    }

    #[test]
    fn regular_file_yields_delete_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file");
        std::fs::write(&path, b"x").unwrap();
        let action = decide_action_delete(&path).unwrap();
        assert_eq!(action, Action::DeleteFile);
    }

    #[test]
    fn directory_yields_delete_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("d");
        std::fs::create_dir(&path).unwrap();
        let action = decide_action_delete(&path).unwrap();
        assert_eq!(action, Action::DeleteDir);
    }

    #[test]
    fn symlink_to_dir_yields_delete_file_not_dir() {
        // Key invariant: symlink_metadata не следует за линком. Симлинк
        // на директорию должен трактоваться как «удалить сам линк», а не
        // как «удалить директорию».
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        let action = decide_action_delete(&link).unwrap();
        assert_eq!(action, Action::DeleteFile);
    }

    #[test]
    fn symlink_to_missing_target_yields_delete_file() {
        // Висящий симлинк: цель отсутствует, но сам линк существует и
        // должен быть удалён.
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("no-such"), &link).unwrap();
        let action = decide_action_delete(&link).unwrap();
        assert_eq!(action, Action::DeleteFile);
    }
}
