//! Запуск внешних команд с дедлайном.
//!
//! `CommandRunner` — trait, чтобы apt.package можно было unit-тестить через
//! mock без реального `apt-get`. `RealCommandRunner` использует
//! `std::process::Command` + thread-based polling try_wait для timeout.

use std::process::Stdio;
use std::time::{Duration, Instant};

use bosun_core::PrimitiveError;

/// Результат запуска команды. Exit-code может быть None, если процесс
/// прибит сигналом (на Unix — kill + WIFSIGNALED).
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Контракт исполнителя команд: запустить cmd с args, дождаться завершения
/// или дедлайна. На дедлайн — kill + `Cancelled`.
pub trait CommandRunner: Send + Sync {
    fn run(
        &self,
        cmd: &str,
        args: &[&str],
        deadline: Instant,
    ) -> Result<CommandResult, PrimitiveError>;
}

/// Production-ready реализация через `std::process::Command`.
///
/// Polling-loop с шагом 100ms — на apt-get install это нерелевантный
/// оверхед (сама операция секунды-минуты), зато дёшево и не тащит tokio.
pub struct RealCommandRunner;

const POLL_INTERVAL: Duration = Duration::from_millis(100);

impl CommandRunner for RealCommandRunner {
    fn run(
        &self,
        cmd: &str,
        args: &[&str],
        deadline: Instant,
    ) -> Result<CommandResult, PrimitiveError> {
        // Если дедлайн уже истёк до запуска — даже не пытаемся spawnить.
        if Instant::now() >= deadline {
            return Err(PrimitiveError::Cancelled);
        }

        let mut child = std::process::Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| PrimitiveError::Io {
                context: format!("spawn {cmd}"),
                source: e,
            })?;

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = child.wait_with_output().map_err(|e| PrimitiveError::Io {
                        context: format!("wait_with_output {cmd}"),
                        source: e,
                    })?;
                    return Ok(CommandResult {
                        exit_code: status.code(),
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    });
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        // Дедлайн истёк — убиваем процесс. Игнорируем kill-error:
                        // если процесс уже завершился сам, kill вернёт ошибку,
                        // но это нерелевантно — нам важно вернуть Cancelled.
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(PrimitiveError::Cancelled);
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(e) => {
                    return Err(PrimitiveError::Io {
                        context: format!("try_wait {cmd}"),
                        source: e,
                    });
                }
            }
        }
    }
}

/// Категория исхода `apt-get install` — определяется по exit-code и stderr.
/// Используется apply.rs для выбора recovery-стратегии.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallOutcome {
    /// exit 0 — установка прошла успешно.
    Success,
    /// exit 100 + stderr содержит «dpkg was interrupted» — нужен
    /// `dpkg --configure -a` + retry.
    DpkgInterrupted,
    /// exit 100 + stderr про «Unable to locate package» или
    /// «Unable to fetch some archives» — нужен `apt-get update` + retry.
    CandidateMissing,
    /// Любая другая ошибка — без retry, PrimitiveError::Exec.
    OtherFailure,
}

/// Разобрать результат `apt-get install` в категорию для recovery.
///
/// Pattern-matching по подстрокам stderr — это договорённость spec'а:
/// regex был бы хрупкий, а localized stderr apt-get у нас всегда английский
/// (мы запускаем под `LC_ALL=C` через переменные среды CLI; даже без неё
/// английский — дефолт для не-интерактивного режима).
pub fn analyze_install_result(result: &CommandResult) -> InstallOutcome {
    match result.exit_code {
        Some(0) => InstallOutcome::Success,
        Some(100) => {
            if result.stderr.contains("dpkg was interrupted") {
                InstallOutcome::DpkgInterrupted
            } else if result.stderr.contains("Unable to locate package")
                || result.stderr.contains("Unable to fetch some archives")
            {
                InstallOutcome::CandidateMissing
            } else {
                InstallOutcome::OtherFailure
            }
        }
        _ => InstallOutcome::OtherFailure,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    fn cmdres(exit: Option<i32>, stderr: &str) -> CommandResult {
        CommandResult {
            exit_code: exit,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn analyze_exit_zero_is_success() {
        assert_eq!(
            analyze_install_result(&cmdres(Some(0), "")),
            InstallOutcome::Success
        );
    }

    #[test]
    fn analyze_dpkg_interrupted_marker() {
        let r = cmdres(
            Some(100),
            "E: dpkg was interrupted, you must manually run 'dpkg --configure -a' to correct the problem.",
        );
        assert_eq!(analyze_install_result(&r), InstallOutcome::DpkgInterrupted);
    }

    #[test]
    fn analyze_unable_to_locate() {
        let r = cmdres(Some(100), "E: Unable to locate package nginx-noexist");
        assert_eq!(analyze_install_result(&r), InstallOutcome::CandidateMissing);
    }

    #[test]
    fn analyze_unable_to_fetch_archives() {
        let r = cmdres(
            Some(100),
            "E: Unable to fetch some archives, maybe run apt-get update or try with --fix-missing?",
        );
        assert_eq!(analyze_install_result(&r), InstallOutcome::CandidateMissing);
    }

    #[test]
    fn analyze_exit_100_with_unknown_stderr_is_other_failure() {
        let r = cmdres(Some(100), "E: Sub-process /usr/bin/dpkg returned an error");
        assert_eq!(analyze_install_result(&r), InstallOutcome::OtherFailure);
    }

    #[test]
    fn analyze_unknown_exit_code_is_other_failure() {
        assert_eq!(
            analyze_install_result(&cmdres(Some(1), "")),
            InstallOutcome::OtherFailure,
        );
    }

    #[test]
    fn analyze_signal_killed_is_other_failure() {
        // exit_code == None — процесс прибит сигналом. Это не Success и не
        // recovery-кейс; должны вернуть OtherFailure.
        assert_eq!(
            analyze_install_result(&cmdres(None, "")),
            InstallOutcome::OtherFailure,
        );
    }

    #[test]
    fn real_command_runner_runs_true() {
        let runner = RealCommandRunner;
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = runner.run("true", &[], deadline).unwrap();
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn real_command_runner_captures_stdout() {
        let runner = RealCommandRunner;
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = runner.run("echo", &["hello"], deadline).unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("hello"));
    }

    #[test]
    fn real_command_runner_captures_stderr() {
        let runner = RealCommandRunner;
        let deadline = Instant::now() + Duration::from_secs(5);
        // Команда `sh -c` гарантированно есть на Linux/Debian базовых образах.
        let result = runner
            .run("sh", &["-c", "echo err 1>&2; exit 7"], deadline)
            .unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert!(result.stderr.contains("err"));
    }

    #[test]
    fn real_command_runner_deadline_cancels_long_sleep() {
        let runner = RealCommandRunner;
        let deadline = Instant::now() + Duration::from_millis(300);
        let started = Instant::now();
        let err = runner
            .run("sleep", &["30"], deadline)
            .expect_err("sleep 30s should not finish within 300ms");
        assert!(matches!(err, PrimitiveError::Cancelled));
        // Запас на polling-interval (100ms) + spawn-overhead.
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn real_command_runner_past_deadline_returns_cancelled_without_spawn() {
        let runner = RealCommandRunner;
        let deadline = Instant::now() - Duration::from_secs(1);
        let err = runner.run("true", &[], deadline).unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
    }

    #[test]
    fn real_command_runner_unknown_binary_is_io_error() {
        let runner = RealCommandRunner;
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = runner
            .run("/no/such/binary/12345", &[], deadline)
            .unwrap_err();
        match err {
            PrimitiveError::Io { context, .. } => assert!(context.starts_with("spawn ")),
            other => panic!("expected Io, got {other:?}"),
        }
    }
}
