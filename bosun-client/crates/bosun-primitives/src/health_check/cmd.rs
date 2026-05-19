//! Cmd-вариант health-check'а: spawn `argv`, exit 0 → success.
//!
//! Реализует одну попытку: spawn, polling до `timeout`, чтение stderr-
//! excerpt'а. Retry-loop живёт в `mod.rs::run_cmd` и переиспользует
//! `cancellable_sleep` из `bosun-core`.

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use bosun_core::health_check::EXCERPT_LIMIT;

/// Результат одной попытки cmd-probe'а.
#[derive(Debug)]
pub(super) enum Attempt {
    /// Exit code 0 — health-check считается успешным.
    Success,
    /// Ненулевой exit (либо kill через signal). `code` — Option, потому
    /// что `ExitStatus::code()` возвращает None при kill'е сигналом —
    /// тогда подставляется -1.
    ExitNonZero { code: i32, stderr_excerpt: String },
    /// Процесс не уложился в `timeout`. Child убит.
    Timeout,
    /// Не удалось запустить процесс (ENOENT и т.п.). Retry не поможет.
    SpawnError(std::io::Error),
}

/// Шаг polling-цикла `try_wait`. 50 мс — компромисс: реагируем быстро на
/// завершение быстрого probe'а (типичный curl/healthcheck отвечает за
/// миллисекунды), не нагружаем ядро лишними сис-вызовами.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Выполнить одну попытку: spawn → polling до `timeout`. Возвращает
/// классификацию исхода (см. [`Attempt`]).
pub(super) fn run_once(argv: &[String], timeout: Duration) -> Attempt {
    let Some((cmd, rest)) = argv.split_first() else {
        return Attempt::SpawnError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "health_check cmd: empty argv",
        ));
    };

    let mut command = Command::new(cmd);
    command
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Attempt::SpawnError(e),
    };

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stderr_excerpt = read_stderr_excerpt(&mut child);
                if status.success() {
                    return Attempt::Success;
                }
                let code = status.code().unwrap_or(-1);
                return Attempt::ExitNonZero {
                    code,
                    stderr_excerpt,
                };
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    // Грохаем child; ошибки kill/wait игнорируем — главное
                    // не висеть в цикле дальше timeout'а.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Attempt::Timeout;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                // try_wait отдал ошибку — редкий случай (несуществующий
                // PID, прерванное ядро). Трактуем как Spawn-эквивалент.
                return Attempt::SpawnError(e);
            }
        }
    }
}

/// Прочитать stderr дочернего процесса и обрезать до `EXCERPT_LIMIT`
/// байт. Симметрично `bosun-core::validate::read_stderr_excerpt` —
/// специально не выносим в core, чтобы health-check оставался автономным.
fn read_stderr_excerpt(child: &mut std::process::Child) -> String {
    use std::io::Read as _;
    let Some(stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::with_capacity(EXCERPT_LIMIT);
    let _ = stderr.take(EXCERPT_LIMIT as u64).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn run_once_true_returns_success() {
        let res = run_once(&["true".to_string()], Duration::from_secs(2));
        match res {
            Attempt::Success => {}
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn run_once_false_returns_exit_non_zero() {
        let res = run_once(&["false".to_string()], Duration::from_secs(2));
        match res {
            Attempt::ExitNonZero { code, .. } => assert_eq!(code, 1),
            other => panic!("expected ExitNonZero, got {other:?}"),
        }
    }

    #[test]
    fn run_once_captures_stderr_excerpt() {
        let res = run_once(
            &[
                "sh".to_string(),
                "-c".into(),
                "echo healthcheck-bad-state >&2; exit 7".into(),
            ],
            Duration::from_secs(2),
        );
        match res {
            Attempt::ExitNonZero {
                code,
                stderr_excerpt,
            } => {
                assert_eq!(code, 7);
                assert!(
                    stderr_excerpt.contains("healthcheck-bad-state"),
                    "stderr должен содержать маркер, got: {stderr_excerpt:?}",
                );
            }
            other => panic!("expected ExitNonZero, got {other:?}"),
        }
    }

    #[test]
    fn run_once_timeout_kills_long_running() {
        let started = Instant::now();
        let res = run_once(
            &["sleep".to_string(), "30".into()],
            Duration::from_millis(200),
        );
        let elapsed = started.elapsed();
        assert!(matches!(res, Attempt::Timeout));
        // Убедимся, что мы убили child'а быстро и не висели до конца sleep'а.
        assert!(
            elapsed < Duration::from_secs(3),
            "child должен быть убит сразу после timeout'а, заняло {elapsed:?}",
        );
    }

    #[test]
    fn run_once_empty_argv_returns_spawn_error() {
        let res = run_once(&[], Duration::from_secs(1));
        assert!(matches!(res, Attempt::SpawnError(_)));
    }

    #[test]
    fn run_once_nonexistent_binary_returns_spawn_error() {
        let res = run_once(
            &["__bosun_no_such_bin_health_check__".to_string()],
            Duration::from_secs(1),
        );
        assert!(matches!(res, Attempt::SpawnError(_)));
    }
}
