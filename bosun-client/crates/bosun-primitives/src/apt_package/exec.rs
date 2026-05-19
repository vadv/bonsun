//! Запуск внешних команд с дедлайном, cancellation и process-group reaping.
//!
//! `CommandRunner` — trait, чтобы apt.package можно было unit-тестить через
//! mock без реального `apt-get`. `RealCommandRunner` использует
//! `std::process::Command` + drain-threads для stdout/stderr + polling
//! `try_wait` для timeout/cancel.
//!
//! ## Design notes
//!
//! - **Pipe drain** (F04): без асинхронного чтения stdout/stderr пайпы
//!   могут переполниться (~64 KB на linux) и заблокировать child. Раньше
//!   bosun только опрашивал try_wait, поэтому verbose apt-get install
//!   завесает до дедлайна и возвращал ложный Cancelled. Сейчас два
//!   отдельных треда копируют pipes в Vec<u8> параллельно с polling'ом.
//!
//! - **Process group** (F05): через `pre_exec` ребёнок становится
//!   process-group leader (`setpgid(0, 0)`). На cancel/deadline отправляем
//!   SIGTERM на всю группу, потом SIGKILL — это убивает maintainer-script
//!   потомков (postinst и т.д.), которые иначе пережили бы родителя и
//!   оставили dpkg в half-configured.
//!
//! - **Cancellation token** (F05): CommandRunner получает `&CancellationToken`,
//!   polling-цикл проверяет и cancel, и deadline. При cancel отдаёт тот же
//!   `PrimitiveError::Cancelled`, что и при дедлайне — caller считает
//!   причину через ApplyCtx.

// SAFETY-обоснование на уровне модуля: unsafe-блоки только для FFI
// libc::setpgid (в pre_exec, между fork и exec) и libc::kill (для
// signalling процесс-группы). Оба вызова стандартные POSIX, аргументы
// числовые/корректные, return code проверяется.
#![allow(unsafe_code)]

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use bosun_core::PrimitiveError;
use tokio_util::sync::CancellationToken;

/// Результат запуска команды. Exit-code может быть None, если процесс
/// прибит сигналом (на Unix — kill + WIFSIGNALED).
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Контракт исполнителя команд: запустить cmd с args, дождаться завершения
/// или дедлайна/cancel. На дедлайн или cancel — kill всей process-group +
/// `Cancelled`.
pub trait CommandRunner: Send + Sync {
    fn run(
        &self,
        cmd: &str,
        args: &[&str],
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<CommandResult, PrimitiveError>;
}

/// Production-ready реализация через `std::process::Command`.
///
/// Polling-loop с шагом 50 ms — на apt-get install это нерелевантный
/// оверхед (сама операция секунды-минуты), зато reaction-time на cancel
/// в районе 100ms, что приятнее для оператора.
pub struct RealCommandRunner;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Grace-period между SIGTERM и SIGKILL для процесс-группы.
const SIGTERM_GRACE: Duration = Duration::from_millis(500);

impl CommandRunner for RealCommandRunner {
    fn run(
        &self,
        cmd: &str,
        args: &[&str],
        deadline: Instant,
        cancel: &CancellationToken,
    ) -> Result<CommandResult, PrimitiveError> {
        // Если дедлайн уже истёк или cancel — даже не пытаемся spawnить.
        if Instant::now() >= deadline || cancel.is_cancelled() {
            return Err(PrimitiveError::Cancelled);
        }

        let mut command = std::process::Command::new(cmd);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // SAFETY: pre_exec выполняется между fork и exec в child-процессе.
        // libc::setpgid(0, 0) — POSIX, без побочных эффектов для родителя.
        // Возврат -1 ловим и пропагируем как io::Error из pre_exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(|e| PrimitiveError::Io {
            context: format!("spawn {cmd}"),
            source: e,
        })?;

        let pid = child.id() as i32;

        // Запускаем дренаж stdout/stderr в отдельных тредах. Это критично:
        // без чтения пайпы апт-get'a с длинным выводом упрутся в OS pipe
        // buffer (~64 KB), child заблокируется на write, а bosun будет
        // полагать «процесс висит» и убьёт его по дедлайну.
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_thread = stdout.map(|mut s| {
            std::thread::spawn(move || {
                let mut buf = Vec::with_capacity(4096);
                let _ = s.read_to_end(&mut buf);
                buf
            })
        });
        let stderr_thread = stderr.map(|mut s| {
            std::thread::spawn(move || {
                let mut buf = Vec::with_capacity(4096);
                let _ = s.read_to_end(&mut buf);
                buf
            })
        });

        let outcome = wait_with_timeout_and_cancel(&mut child, pid, deadline, cancel);

        let stdout_buf = stdout_thread
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        let stderr_buf = stderr_thread
            .and_then(|t| t.join().ok())
            .unwrap_or_default();

        match outcome {
            WaitOutcome::Exited(status) => Ok(CommandResult {
                exit_code: status.code(),
                stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
            }),
            WaitOutcome::Cancelled => Err(PrimitiveError::Cancelled),
            WaitOutcome::TryWaitFailed(e) => Err(PrimitiveError::Io {
                context: format!("try_wait {cmd}"),
                source: e,
            }),
        }
    }
}

enum WaitOutcome {
    Exited(std::process::ExitStatus),
    Cancelled,
    TryWaitFailed(std::io::Error),
}

/// Polling-цикл: ждём child, реагируем на deadline и cancel. При обоих
/// — посылаем SIGTERM на process-group (pgid = pid_лидера), даём
/// grace-period, потом SIGKILL и reap.
fn wait_with_timeout_and_cancel(
    child: &mut Child,
    pid: i32,
    deadline: Instant,
    cancel: &CancellationToken,
) -> WaitOutcome {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return WaitOutcome::Exited(status),
            Ok(None) => {
                let past_deadline = Instant::now() >= deadline;
                let cancelled = cancel.is_cancelled();
                if past_deadline || cancelled {
                    kill_process_group(pid);
                    // Reap child — иначе zombie.
                    let _ = child.wait();
                    return WaitOutcome::Cancelled;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return WaitOutcome::TryWaitFailed(e),
        }
    }
}

/// Терминирует процесс-группу с лидером `pid`: SIGTERM, grace-period, SIGKILL.
/// `kill(-pid, signal)` — POSIX-семантика «послать всем в группе с pgid=pid».
fn kill_process_group(pid: i32) {
    // SAFETY: libc::kill(-pid, sig) — POSIX. pid в данном контексте
    // получен из spawned Child, валиден до wait(). Послать SIGTERM/SIGKILL
    // несуществующей группе вернёт -1 с errno=ESRCH, что нас устраивает
    // (нет процессов — нечего гасить).
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    std::thread::sleep(SIGTERM_GRACE);
    // SAFETY: см. выше.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
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
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = runner.run("true", &[], deadline, &cancel).unwrap();
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn real_command_runner_captures_stdout() {
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = runner.run("echo", &["hello"], deadline, &cancel).unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("hello"));
    }

    #[test]
    fn real_command_runner_captures_stderr() {
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = runner
            .run("sh", &["-c", "echo err 1>&2; exit 7"], deadline, &cancel)
            .unwrap();
        assert_eq!(result.exit_code, Some(7));
        assert!(result.stderr.contains("err"));
    }

    #[test]
    fn real_command_runner_deadline_cancels_long_sleep() {
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_millis(300);
        let started = Instant::now();
        let err = runner
            .run("sleep", &["30"], deadline, &cancel)
            .expect_err("sleep 30s should not finish within 300ms");
        assert!(matches!(err, PrimitiveError::Cancelled));
        // Запас на polling-interval + grace + spawn-overhead.
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn real_command_runner_cancel_token_aborts() {
        // F05: внешний cancel через token должен убить group и вернуть
        // Cancelled даже до дедлайна.
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        let cancel_for_thread = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel_for_thread.cancel();
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let started = Instant::now();
        let err = runner
            .run("sleep", &["30"], deadline, &cancel)
            .expect_err("cancel должен прервать sleep");
        assert!(matches!(err, PrimitiveError::Cancelled));
        // Reaction window: cancel срабатывает через ~200ms, polling 50ms,
        // SIGTERM/grace 500ms, SIGKILL — итого должно уложиться в 3s.
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "elapsed: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn real_command_runner_past_deadline_returns_cancelled_without_spawn() {
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        let deadline = Instant::now() - Duration::from_secs(1);
        let err = runner.run("true", &[], deadline, &cancel).unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
    }

    #[test]
    fn real_command_runner_pre_cancelled_token_returns_cancelled_without_spawn() {
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = runner.run("true", &[], deadline, &cancel).unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
    }

    #[test]
    fn real_command_runner_unknown_binary_is_io_error() {
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = runner
            .run("/no/such/binary/12345", &[], deadline, &cancel)
            .unwrap_err();
        match err {
            PrimitiveError::Io { context, .. } => assert!(context.starts_with("spawn ")),
            other => panic!("expected Io, got {other:?}"),
        }
    }

    #[test]
    fn real_command_runner_drains_large_stdout_without_deadlock() {
        // F04 regression: без drain-тредов пайп stdout переполнялся (~64 KB)
        // и child блокировался. Эмулируем большой вывод (~600 KB) и
        // убеждаемся, что bosun не залипает на try_wait.
        let runner = RealCommandRunner;
        let cancel = CancellationToken::new();
        // 100k строк по ~6 байт ≈ 600 KB — гарантированно больше pipe buffer.
        let script = "for i in $(seq 1 100000); do echo \"x$i\"; done; exit 0";
        let deadline = Instant::now() + Duration::from_secs(30);
        let started = Instant::now();
        let result = runner
            .run("sh", &["-c", script], deadline, &cancel)
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        // Должно занять считанные секунды, а не «дойти до дедлайна».
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "elapsed: {:?}",
            started.elapsed()
        );
        // И stdout должен быть реально захвачен.
        assert!(result.stdout.contains("x1\n"));
        assert!(result.stdout.contains("x100000\n"));
    }
}
