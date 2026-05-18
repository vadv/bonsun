//! Non-blocking probe `/var/lib/dpkg/lock-frontend` через fs4::FileExt.
//!
//! `apt`/`dpkg` берут exclusive flock(2) на этот файл. Если бэк-граундовый
//! `unattended-upgrades` сейчас работает, мы не должны ждать минутами — это
//! quick-fail, верхний уровень повторит на следующем прогоне.

use std::fs::OpenOptions;
use std::path::Path;

use bosun_core::PrimitiveError;
use fs4::fs_std::FileExt;

/// Попытаться взять exclusive-lock неблокирующе. Сразу освобождаем —
/// это только probe «можно ли запускать apt-get сейчас».
///
/// Семантика возврата:
/// - `Ok(())` — lock-frontend свободен, можно запускать apt-get.
/// - `PrimitiveError::DpkgLocked { holder_pid }` — кто-то держит lock.
///   `holder_pid` — best-effort: пытаемся прочитать содержимое файла как
///   pid; на любую ошибку → `None`.
/// - `PrimitiveError::Io { ... }` — открыть файл не удалось (например,
///   `/var/lib/dpkg/` не существует — нода не Debian/Ubuntu).
pub fn probe_dpkg_lock(path: &Path) -> Result<(), PrimitiveError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(path)
        .map_err(|e| PrimitiveError::Io {
            context: format!("open {} for lock probe", path.display()),
            source: e,
        })?;

    // UFCS-вызовы FileExt: на Rust ≥ 1.89 у `std::fs::File` появились
    // inherent методы `try_lock_exclusive` и `unlock`, конфликтующие по
    // именам с trait'ом fs4. При MSRV 1.84 inherent методов ещё нет, но
    // clippy::incompatible_msrv ругается на любое такое имя — проще
    // явно адресовать fs4-trait через UFCS.
    match FileExt::try_lock_exclusive(&file) {
        Ok(true) => {
            FileExt::unlock(&file).map_err(|e| PrimitiveError::Io {
                context: format!("unlock {} after probe", path.display()),
                source: e,
            })?;
            Ok(())
        }
        Ok(false) => Err(PrimitiveError::DpkgLocked {
            holder_pid: try_read_holder_pid(path),
        }),
        Err(e) => Err(PrimitiveError::Io {
            context: format!("try_lock_exclusive {}", path.display()),
            source: e,
        }),
    }
}

/// Best-effort попытка вытащить pid держателя lock'а. Debian/Ubuntu в
/// `lock-frontend` пишут pid удерживающего процесса в виде ASCII, но это
/// не зафиксировано в man-странице. Любой сбой парсинга → None, чтобы
/// probe не падал из-за best-effort деталей.
fn try_read_holder_pid(path: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse::<i32>().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use std::fs::File;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn probe_free_lock_returns_ok() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Файл существует, но никто не держит lock — probe должен пройти.
        probe_dpkg_lock(tmp.path()).expect("free lock should be acquirable");
    }

    #[test]
    fn probe_missing_file_is_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("no-such-file");
        let err = probe_dpkg_lock(&path).unwrap_err();
        match err {
            PrimitiveError::Io { context, .. } => assert!(context.contains("lock probe")),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn probe_locked_by_other_thread_returns_dpkg_locked() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        // Другой тред берёт lock и держит его, пока мы пробуем.
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let path_for_thread = path.clone();
        let holder = thread::spawn(move || {
            let f = File::options()
                .read(true)
                .write(true)
                .open(&path_for_thread)
                .unwrap();
            FileExt::lock_exclusive(&f).unwrap();
            started_tx.send(()).unwrap();
            // Висим, пока тест не разрешит выход.
            let _ = release_rx.recv();
            FileExt::unlock(&f).unwrap();
        });

        // Дожидаемся, что lock реально взят.
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("holder thread should signal");

        let err = probe_dpkg_lock(&path).unwrap_err();
        match err {
            PrimitiveError::DpkgLocked { .. } => {}
            other => panic!("expected DpkgLocked, got {other:?}"),
        }

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn probe_locked_reads_pid_when_file_contains_number() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::fs::write(&path, "12345\n").unwrap();

        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let path_for_thread = path.clone();
        let holder = thread::spawn(move || {
            let f = File::options()
                .read(true)
                .write(true)
                .open(&path_for_thread)
                .unwrap();
            FileExt::lock_exclusive(&f).unwrap();
            started_tx.send(()).unwrap();
            let _ = release_rx.recv();
            FileExt::unlock(&f).unwrap();
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("holder should signal");

        let err = probe_dpkg_lock(&path).unwrap_err();
        match err {
            PrimitiveError::DpkgLocked { holder_pid } => assert_eq!(holder_pid, Some(12345)),
            other => panic!("expected DpkgLocked, got {other:?}"),
        }

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }
}
