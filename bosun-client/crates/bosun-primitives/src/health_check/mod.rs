//! Production-реализация `HealthCheckRunner`: cmd + url probe'ы с retry.
//!
//! Phase I — после успешного restart/reload (sync или через replay defer'а)
//! здесь крутится проверка «сервис реально живой?». Контракт trait'а и
//! ошибки живут в `bosun-core::health_check`; этот модуль предоставляет
//! `RealHealthCheckRunner` поверх `std::process::Command` и `ureq::Agent`.
//!
//! ## Структура
//!
//! - `cmd.rs` — одна попытка spawn'а argv с timeout'ом.
//! - `url.rs` — одна попытка HTTP GET через `ureq::Agent`.
//! - `mod.rs` — retry-loop, cancellation, маппинг ошибок в
//!   `HealthCheckError`.
//!
//! Реальные тесты Url'а живут в `tests/health_check_wiremock.rs` (integration
//! test, поднимающий wiremock-сервер) — здесь только smoke без сети.

mod cmd;
mod url;

use bosun_core::health_check::{
    cancellable_sleep, resolve_defaults, HealthCheckError, HealthCheckRunner,
};
use bosun_core::HealthCheck;
use tokio_util::sync::CancellationToken;

/// Production-реализация: exec через `std::process::Command`, HTTP через
/// собственный `ureq::Agent` (не привязанный к `bosun-runr-client`).
///
/// Stateless — все runtime-ресурсы (Command spawn, Agent) создаются
/// внутри `run`. Это упрощает тестирование и не требует Drop.
#[derive(Default, Debug)]
pub struct RealHealthCheckRunner;

impl RealHealthCheckRunner {
    pub fn new() -> Self {
        Self
    }
}

impl HealthCheckRunner for RealHealthCheckRunner {
    fn run(&self, check: &HealthCheck, cancel: &CancellationToken) -> Result<(), HealthCheckError> {
        match check {
            HealthCheck::Cmd {
                cmd,
                timeout_sec,
                retry_count,
                retry_interval_sec,
            } => {
                let target = cmd.first().cloned().unwrap_or_default();
                let span = tracing::info_span!("health_check", target = %target, kind = "cmd");
                let _g = span.enter();
                run_cmd_with_retries(cmd, *timeout_sec, *retry_count, *retry_interval_sec, cancel)
            }
            HealthCheck::Url {
                url,
                expected_status,
                timeout_sec,
                retry_count,
                retry_interval_sec,
            } => {
                let span = tracing::info_span!("health_check", target = %url, kind = "url");
                let _g = span.enter();
                run_url_with_retries(
                    url,
                    *expected_status,
                    *timeout_sec,
                    *retry_count,
                    *retry_interval_sec,
                    cancel,
                )
            }
            // HealthCheck is `#[non_exhaustive]`; новые варианты (gRPC,
            // tcp-probe и т.п.) попадут сюда. Пока возвращаем Ok, чтобы
            // не блокировать апгрейд формата — лучше попустить health-check,
            // чем уронить весь apply на старом binary'е.
            _ => {
                tracing::warn!("health-check: unknown variant, skipping");
                Ok(())
            }
        }
    }
}

/// Cmd retry-loop. На каждый retry между попытками — `cancellable_sleep`.
/// Возвращает первый успех; если все retry'и исчерпаны — последнюю
/// ошибку, упакованную в `HealthCheckError`.
fn run_cmd_with_retries(
    argv: &[String],
    timeout_sec: Option<u32>,
    retry_count: Option<u32>,
    retry_interval_sec: Option<u32>,
    cancel: &CancellationToken,
) -> Result<(), HealthCheckError> {
    let (timeout, retries, interval) =
        resolve_defaults(timeout_sec, retry_count, retry_interval_sec);

    let mut last_exit_code: Option<i32> = None;
    let mut last_stderr: String = String::new();
    let mut had_timeout = false;

    for attempt in 1..=retries {
        if cancel.is_cancelled() {
            return Err(HealthCheckError::Cancelled);
        }
        match cmd::run_once(argv, timeout) {
            cmd::Attempt::Success => {
                tracing::info!(attempt, "health-check cmd ok");
                return Ok(());
            }
            cmd::Attempt::ExitNonZero {
                code,
                stderr_excerpt,
            } => {
                last_exit_code = Some(code);
                last_stderr = stderr_excerpt;
                had_timeout = false;
                if attempt < retries {
                    tracing::warn!(
                        attempt,
                        retry = attempt + 1,
                        exit_code = code,
                        "health-check cmd failed, retrying",
                    );
                }
            }
            cmd::Attempt::Timeout => {
                last_exit_code = None;
                had_timeout = true;
                if attempt < retries {
                    tracing::warn!(
                        attempt,
                        retry = attempt + 1,
                        "health-check cmd timeout, retrying",
                    );
                }
            }
            cmd::Attempt::SpawnError(e) => {
                // Spawn-fail означает, что бинарь либо не существует,
                // либо нет прав. Retry не поможет — провал сразу.
                tracing::error!(
                    attempt,
                    error = %e,
                    "health-check cmd spawn failed",
                );
                return Err(HealthCheckError::CmdSpawnError(e));
            }
        }
        if attempt < retries && !cancellable_sleep(interval, cancel) {
            return Err(HealthCheckError::Cancelled);
        }
    }

    if had_timeout {
        Err(HealthCheckError::CmdTimeout {
            argv: argv.to_vec(),
            attempts: retries,
        })
    } else {
        Err(HealthCheckError::CmdExitNonZero {
            exit_code: last_exit_code.unwrap_or(-1),
            attempts: retries,
            stderr_excerpt: last_stderr,
        })
    }
}

/// Url retry-loop. Симметричен cmd-варианту: одна попытка в
/// `url::run_once`, retry между попытками, маппинг в `HealthCheckError`.
fn run_url_with_retries(
    target: &str,
    expected_status: Option<u16>,
    timeout_sec: Option<u32>,
    retry_count: Option<u32>,
    retry_interval_sec: Option<u32>,
    cancel: &CancellationToken,
) -> Result<(), HealthCheckError> {
    let (timeout, retries, interval) =
        resolve_defaults(timeout_sec, retry_count, retry_interval_sec);
    let expected = expected_status.unwrap_or(url::DEFAULT_EXPECTED_STATUS);
    let agent = url::build_agent(timeout);

    let mut last_actual: Option<u16> = None;
    let mut last_transport_reason: Option<String> = None;

    for attempt in 1..=retries {
        if cancel.is_cancelled() {
            return Err(HealthCheckError::Cancelled);
        }
        match url::run_once(&agent, target, expected) {
            url::Attempt::Success => {
                tracing::info!(attempt, "health-check url ok");
                return Ok(());
            }
            url::Attempt::BadStatus { actual } => {
                last_actual = Some(actual);
                last_transport_reason = None;
                if attempt < retries {
                    tracing::warn!(
                        attempt,
                        retry = attempt + 1,
                        actual,
                        expected,
                        "health-check url bad status, retrying",
                    );
                }
            }
            url::Attempt::Transport { reason } => {
                last_actual = None;
                last_transport_reason = Some(reason.clone());
                if attempt < retries {
                    tracing::warn!(
                        attempt,
                        retry = attempt + 1,
                        reason = %reason,
                        "health-check url transport error, retrying",
                    );
                }
            }
        }
        if attempt < retries && !cancellable_sleep(interval, cancel) {
            return Err(HealthCheckError::Cancelled);
        }
    }

    if let Some(actual) = last_actual {
        Err(HealthCheckError::UrlBadStatus {
            url: target.to_string(),
            actual,
            expected,
            attempts: retries,
        })
    } else {
        Err(HealthCheckError::UrlTransport {
            url: target.to_string(),
            attempts: retries,
            reason: last_transport_reason.unwrap_or_else(|| "unknown".to_string()),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn cmd_true_returns_ok_on_first_attempt() {
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Cmd {
            cmd: vec!["true".to_string()],
            timeout_sec: Some(2),
            retry_count: Some(1),
            retry_interval_sec: Some(0),
        };
        let cancel = CancellationToken::new();
        assert!(runner.run(&check, &cancel).is_ok());
    }

    #[test]
    fn cmd_false_fails_after_retries_and_returns_exit_non_zero() {
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Cmd {
            cmd: vec!["false".to_string()],
            timeout_sec: Some(2),
            retry_count: Some(2),
            retry_interval_sec: Some(0),
        };
        let cancel = CancellationToken::new();
        let err = runner.run(&check, &cancel).unwrap_err();
        match err {
            HealthCheckError::CmdExitNonZero {
                exit_code,
                attempts,
                ..
            } => {
                assert_eq!(exit_code, 1);
                assert_eq!(attempts, 2);
            }
            other => panic!("expected CmdExitNonZero, got {other:?}"),
        }
    }

    #[test]
    fn cmd_long_running_returns_timeout_after_retries() {
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Cmd {
            cmd: vec!["sleep".to_string(), "30".into()],
            timeout_sec: Some(1),
            retry_count: Some(2),
            retry_interval_sec: Some(0),
        };
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let err = runner.run(&check, &cancel).unwrap_err();
        let elapsed = started.elapsed();
        match err {
            HealthCheckError::CmdTimeout { attempts, .. } => {
                assert_eq!(attempts, 2);
            }
            other => panic!("expected CmdTimeout, got {other:?}"),
        }
        // Должны вписаться в ~2 секунды (2 timeout по 1 секунде) плюс
        // небольшой overhead. Если зависли на полные 30 секунд — это
        // регрессия kill-логики.
        assert!(
            elapsed < Duration::from_secs(8),
            "timeout retry должен укладываться в ~2s, заняло {elapsed:?}",
        );
    }

    #[test]
    fn cmd_spawn_error_does_not_retry() {
        // Несуществующий бинарь — SpawnError, retry не должен вызываться:
        // если бы вызывался, мы бы зря тратили время на 5 попыток.
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Cmd {
            cmd: vec!["__bosun_no_such_bin_health_check__".to_string()],
            timeout_sec: Some(2),
            retry_count: Some(5),
            retry_interval_sec: Some(1),
        };
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let err = runner.run(&check, &cancel).unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(err, HealthCheckError::CmdSpawnError(_)));
        // 5 попыток × 1 секунда интервала = 5+ секунд. Должны выйти быстро.
        assert!(
            elapsed < Duration::from_secs(2),
            "SpawnError не должен retry'иться, заняло {elapsed:?}",
        );
    }

    #[test]
    fn cmd_cancelled_during_sleep_returns_cancelled() {
        // retry_interval=10s, cancel через 100ms во время sleep'а.
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Cmd {
            cmd: vec!["false".to_string()],
            timeout_sec: Some(2),
            retry_count: Some(3),
            retry_interval_sec: Some(10),
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel_clone.cancel();
        });
        let started = std::time::Instant::now();
        let err = runner.run(&check, &cancel).unwrap_err();
        let elapsed = started.elapsed();
        handle.join().unwrap();
        assert!(matches!(err, HealthCheckError::Cancelled));
        assert!(
            elapsed < Duration::from_secs(3),
            "должны выйти быстро через cancel, заняло {elapsed:?}",
        );
    }

    #[test]
    fn cmd_already_cancelled_does_not_run_first_attempt() {
        // Если cancel выставлен до вызова — даже первая попытка не должна
        // быть запущена.
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Cmd {
            cmd: vec!["true".to_string()],
            timeout_sec: Some(2),
            retry_count: Some(1),
            retry_interval_sec: Some(0),
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = runner.run(&check, &cancel).unwrap_err();
        assert!(matches!(err, HealthCheckError::Cancelled));
    }

    #[test]
    fn url_unreachable_returns_transport_after_retries() {
        // 127.0.0.1:1 — почти гарантированно никто не слушает.
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Url {
            url: "http://127.0.0.1:1/health".to_string(),
            expected_status: Some(200),
            timeout_sec: Some(1),
            retry_count: Some(2),
            retry_interval_sec: Some(0),
        };
        let cancel = CancellationToken::new();
        let err = runner.run(&check, &cancel).unwrap_err();
        match err {
            HealthCheckError::UrlTransport { attempts, .. } => {
                assert_eq!(attempts, 2);
            }
            other => panic!("expected UrlTransport, got {other:?}"),
        }
    }

    #[test]
    fn url_cancelled_during_sleep_returns_cancelled() {
        // retry_interval=10s, cancel через 100ms.
        let runner = RealHealthCheckRunner::new();
        let check = HealthCheck::Url {
            url: "http://127.0.0.1:1/health".to_string(),
            expected_status: Some(200),
            timeout_sec: Some(1),
            retry_count: Some(3),
            retry_interval_sec: Some(10),
        };
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            cancel_clone.cancel();
        });
        let started = std::time::Instant::now();
        let err = runner.run(&check, &cancel).unwrap_err();
        let elapsed = started.elapsed();
        handle.join().unwrap();
        assert!(matches!(err, HealthCheckError::Cancelled));
        assert!(
            elapsed < Duration::from_secs(5),
            "должны выйти через cancel быстрее retry_interval'а, заняло {elapsed:?}",
        );
    }
}
