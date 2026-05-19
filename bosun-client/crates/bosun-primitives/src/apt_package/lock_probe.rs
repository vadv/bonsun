//! Non-blocking probe `/var/lib/dpkg/lock-frontend` через `fcntl(F_GETLK)`.
//!
//! `apt`/`dpkg`/`unattended-upgrades` берут write-lock через
//! `fcntl(F_SETLK, F_WRLCK)` — POSIX advisory lock, не BSD `flock(2)`.
//! `flock(2)` и `fcntl(F_SETLK)` — **независимые** lock-механизмы:
//! файл, заблокированный одним, не виден через другой. Поэтому
//! раньше bosun запускал apt-get «вслепую» и упирался в DPkg::Lock::Timeout=30s.
//!
//! Текущая реализация делает `F_GETLK` — probe без acquire: ядро отдаёт
//! `l_type=F_UNLCK` если лок свободен, или `l_type=F_WRLCK` + `l_pid`
//! текущего держателя.

// SAFETY-обоснование на уровне модуля: unsafe-блоки — это FFI-вызовы
// `libc::fcntl`. Все указатели валидны (file descriptor из открытого
// File, &mut flock — стэковая структура). Return code проверяется
// явно, errno читается через io::Error::last_os_error.
#![allow(unsafe_code)]

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::Path;

use bosun_core::PrimitiveError;

/// Probe `/var/lib/dpkg/lock-frontend` через `fcntl(F_GETLK, F_WRLCK)`.
///
/// Семантика возврата:
/// - `Ok(())` — lock свободен, можно запускать apt-get.
/// - `PrimitiveError::DpkgLocked { holder_pid }` — кто-то держит lock.
///   `holder_pid` берётся из `l_pid` (положительный → Some, иначе None).
/// - `PrimitiveError::Io { ... }` — не удалось открыть файл или сам
///   `fcntl` упал (например, EBADF — но это уже наш баг).
pub fn probe_dpkg_lock(path: &Path) -> Result<(), PrimitiveError> {
    // O_RDWR обязателен для F_WRLCK probe: ядро проверяет, что fd открыт
    // на запись (иначе EBADF/EINVAL).
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(path)
        .map_err(|e| PrimitiveError::Io {
            context: format!("open {} for lock probe", path.display()),
            source: e,
        })?;

    let mut fl = libc::flock {
        l_type: libc::F_WRLCK as i16,
        l_whence: libc::SEEK_SET as i16,
        l_start: 0,
        // l_len = 0 — «до конца файла», стандартный paradigm у apt/dpkg.
        l_len: 0,
        l_pid: 0,
    };

    // SAFETY: `libc::fcntl(fd, F_GETLK, *mut flock)` — POSIX API.
    // fd валиден (из File выше, живёт до конца функции).
    // F_GETLK — стандартная команда без побочных эффектов: ядро только
    // заполняет fl, не берёт лок. Возврат -1 при ошибке, в этой ветке
    // читаем errno через io::Error::last_os_error.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut fl) };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        return Err(PrimitiveError::Io {
            context: format!("fcntl(F_GETLK) {}", path.display()),
            source: err,
        });
    }

    if fl.l_type == libc::F_UNLCK as i16 {
        Ok(())
    } else {
        let holder_pid = if fl.l_pid > 0 { Some(fl.l_pid) } else { None };
        Err(PrimitiveError::DpkgLocked { holder_pid })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn probe_free_lock_returns_ok() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
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

    /// Helper: запустить дочерний `python3` процесс, который берёт fcntl
    /// write-lock через `fcntl.lockf(LOCK_EX | LOCK_NB)` и держит до stdin EOF.
    /// Возвращает (Child, флаг успешного старта). Если python3 нет в системе —
    /// возвращает Err. Тесты вызывающие helper, грейсфулно проpускаются.
    fn spawn_fcntl_holder(path: &Path) -> Result<std::process::Child, String> {
        let path_str = path
            .to_str()
            .ok_or_else(|| "non-utf8 path".to_string())?
            .to_string();
        let script = format!(
            "import fcntl, sys, time\n\
             f = open(r'{path_str}', 'r+')\n\
             fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB)\n\
             sys.stdout.write('locked\\n'); sys.stdout.flush()\n\
             # Hold until parent kills us.\n\
             sys.stdin.read()\n",
        );
        let mut child = std::process::Command::new("python3")
            .args(["-u", "-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn python3: {e}"))?;

        // Ждём «locked» с stdout — максимум 5 сек.
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel::<()>();
        let reader = thread::spawn(move || {
            let mut buf = [0_u8; 32];
            let mut acc = String::new();
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    return;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                if acc.contains("locked") {
                    let _ = tx.send(());
                    return;
                }
            }
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(()) => {
                // reader thread выйдет сам.
                let _ = reader.join();
                Ok(child)
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                Err("python3 child did not signal 'locked' in 5s".into())
            }
        }
    }

    #[test]
    fn probe_detects_fcntl_lock_from_another_process() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

        let mut child = match spawn_fcntl_holder(&path) {
            Ok(c) => c,
            Err(reason) => {
                eprintln!("skipping fcntl-cross-process test: {reason}");
                return;
            }
        };

        let err = probe_dpkg_lock(&path).unwrap_err();
        match err {
            PrimitiveError::DpkgLocked { .. } => {}
            other => panic!("expected DpkgLocked, got {other:?}"),
        }

        // Завершаем child: closing stdin даёт EOF, скрипт выходит.
        drop(child.stdin.take());
        let _ = child.wait();
    }

    #[test]
    fn probe_does_not_detect_flock_locks() {
        // Защитный тест: BSD-flock(2) и POSIX-fcntl — независимые механизмы.
        // Наш probe смотрит ТОЛЬКО fcntl, поэтому flock-захват от другого
        // процесса остаётся незаметным. Это и есть причина, почему фикс
        // F03 поменял реализацию: apt/dpkg используют fcntl, а старая
        // bosun-проверка через flock(2) их не видела.
        use fs4::fs_std::FileExt;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();

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

        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // probe должен пройти — flock не виден через fcntl.
        probe_dpkg_lock(&path).expect("flock не должен быть виден через fcntl probe");

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    /// Smoke-тест: ручной fcntl-lock-cycle внутри одного процесса.
    /// fcntl-локи per-process: после fcntl(F_SETLK) тот же процесс
    /// видит свой lock как F_UNLCK через F_GETLK, поэтому полноценно
    /// проверить probe в одном процессе нельзя. Здесь только убеждаемся,
    /// что probe возвращает Ok когда lock реально свободен.
    #[test]
    fn fcntl_lock_self_visible_helper_smoke() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let f = File::options()
            .read(true)
            .write(true)
            .open(tmp.path())
            .unwrap();
        let mut fl = libc::flock {
            l_type: libc::F_WRLCK as i16,
            l_whence: libc::SEEK_SET as i16,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        // SAFETY: см. probe_dpkg_lock; same pattern. F_SETLK без блокировки.
        let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETLK, &mut fl) };
        assert_ne!(
            rc,
            -1,
            "fcntl(F_SETLK) failed: {}",
            std::io::Error::last_os_error()
        );
        // Самостоятельный probe того же процесса не увидит lock — это
        // фича fcntl, не баг bosun.
        probe_dpkg_lock(tmp.path()).expect("same-process fcntl-lock invisible to probe");
    }
}
