//! Шаг 2-3 flow: создание state/log/backup-директорий и advisory flock.
//!
//! Делать это до tracing init, потому что lock-файл нельзя создать на пустой
//! файловой системе, и потому что diagnostic при отсутствии прав должен идти
//! в stderr напрямую — subscriber'а ещё нет.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BootstrapError {
    #[error("cannot create directory {path}: {source}")]
    DirCreate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot open lock file {path}: {source}")]
    LockOpen {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot acquire lock on {path}: {source}")]
    LockIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Создать набор директорий через `create_dir_all`. Идемпотентно: если
/// директория уже есть и она директория — ok. Любой Io (PermissionDenied,
/// NotADirectory, ...) превращается в `BootstrapError::DirCreate` с путём.
pub fn ensure_dirs(paths: &[&Path]) -> Result<(), BootstrapError> {
    for path in paths {
        std::fs::create_dir_all(path).map_err(|source| BootstrapError::DirCreate {
            path: path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

/// Результат попытки взять advisory-lock.
///
/// `Acquired` несёт RAII-guard, который снимает lock на drop. `Held` означает,
/// что другая инстанция bosun уже держит lock — для CLI это семантически
/// «нет работы», exit 0.
#[derive(Debug)]
#[non_exhaustive]
pub enum LockOutcome {
    Acquired(LockGuard),
    Held,
}

/// RAII-guard для advisory-lock. Drop вызывает `unlock`. File открыт O_RDWR
/// и нужен только чтобы держать lock — мы туда не пишем.
#[derive(Debug)]
pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // unlock — best-effort: на момент drop'а процесс уже выходит, и
        // даже если ядро не успеет вернуть, lock освободится при close.
        let _ = FileExt::unlock(&self.file);
    }
}

/// Попытаться взять exclusive flock на `lock_path` неблокирующе.
///
/// Семантика fs4 v0.13: `try_lock_exclusive` возвращает `Ok(true)` при
/// успехе, `Ok(false)` если lock уже занят (нет WouldBlock-ветви как
/// io::Error). Это сделано в новых версиях fs4, см. CHANGELOG. Поэтому
/// мы маппим `Ok(false)` в `LockOutcome::Held`, а любую `Err` — в
/// `BootstrapError::LockIo`.
pub fn try_flock(lock_path: &Path) -> Result<LockOutcome, BootstrapError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|source| BootstrapError::LockOpen {
            path: lock_path.to_path_buf(),
            source,
        })?;

    match FileExt::try_lock_exclusive(&file) {
        Ok(true) => Ok(LockOutcome::Acquired(LockGuard { file })),
        Ok(false) => Ok(LockOutcome::Held),
        Err(source) => Err(BootstrapError::LockIo {
            path: lock_path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn ensure_dirs_creates_missing_paths() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a/b/c");
        let b = tmp.path().join("d");
        ensure_dirs(&[&a, &b]).unwrap();
        assert!(a.is_dir());
        assert!(b.is_dir());
    }

    #[test]
    fn ensure_dirs_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("existing");
        std::fs::create_dir_all(&p).unwrap();
        // Второй вызов на ту же директорию должен пройти без ошибок.
        ensure_dirs(&[&p]).unwrap();
        assert!(p.is_dir());
    }

    #[test]
    fn ensure_dirs_returns_error_on_unwritable_parent() {
        // Создаём read-only родителя и пытаемся создать в нём поддиректорию —
        // должен прийти PermissionDenied. Тест валиден только для не-root:
        // root пробивает любые права. На CI обычно не-root, и это нормально.
        if nix_is_root() {
            // На root-окружении мы не можем спровоцировать PermissionDenied
            // через chmod. Считаем тест успешно пропущенным.
            return;
        }
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("locked");
        std::fs::create_dir_all(&parent).unwrap();
        let mut perms = std::fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&parent, perms).unwrap();
        let child = parent.join("child");
        let err = ensure_dirs(&[&child]).unwrap_err();
        // Возвращаем mode обратно, чтобы tempdir мог корректно убраться.
        let mut perms = std::fs::metadata(&parent).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&parent, perms).unwrap();
        match err {
            BootstrapError::DirCreate { source, .. } => {
                assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            }
            other => panic!("expected DirCreate, got {other:?}"),
        }
    }

    /// Эвристика «работаем ли мы под root» без unsafe и без libc-dep: проверяем
    /// окружение. CI и dev-машины запускают тесты не под root, prod-rust
    /// юнит-тесты не запускает совсем. Если попали под root — тест ниже
    /// просто пропускается, чтобы не падать на отсутствии PermissionDenied.
    fn nix_is_root() -> bool {
        std::env::var("USER").map(|u| u == "root").unwrap_or(false)
            || std::env::var("LOGNAME")
                .map(|u| u == "root")
                .unwrap_or(false)
    }

    #[test]
    fn try_flock_acquires_when_free() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("bosun.lock");
        let outcome = try_flock(&lock_path).unwrap();
        match outcome {
            LockOutcome::Acquired(_guard) => {
                // Guard alive до конца блока — lock держится.
            }
            LockOutcome::Held => panic!("expected Acquired on free lock"),
        }
    }

    #[test]
    fn try_flock_second_attempt_reports_held() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("bosun.lock");
        let first = try_flock(&lock_path).unwrap();
        assert!(matches!(first, LockOutcome::Acquired(_)));
        let second = try_flock(&lock_path).unwrap();
        assert!(
            matches!(second, LockOutcome::Held),
            "second flock attempt must see lock as held"
        );
        // Drop первого guard'а — теперь третья попытка должна снова получить lock.
        drop(first);
        let third = try_flock(&lock_path).unwrap();
        assert!(matches!(third, LockOutcome::Acquired(_)));
    }

    #[test]
    fn try_flock_creates_missing_lock_file() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("never-existed.lock");
        assert!(!lock_path.exists());
        let outcome = try_flock(&lock_path).unwrap();
        assert!(matches!(outcome, LockOutcome::Acquired(_)));
        assert!(lock_path.exists());
    }

    #[test]
    fn try_flock_returns_lock_open_when_parent_missing() {
        let tmp = TempDir::new().unwrap();
        // Несуществующая родительская директория.
        let lock_path = tmp.path().join("no/such/parent/x.lock");
        let err = try_flock(&lock_path).unwrap_err();
        assert!(matches!(err, BootstrapError::LockOpen { .. }));
    }
}
