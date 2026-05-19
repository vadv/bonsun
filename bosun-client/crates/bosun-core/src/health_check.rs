//! Контракт исполнителя health-check'ов после restart/reload.
//!
//! Phase I вводит post-action probe: после успешного restart/reload (sync
//! путь или внутри replay defer'а) запускается health-check — либо
//! exec-cmd, либо HTTP GET — с retry-loop'ом. Failure → `PrimitiveError::
//! HealthCheckFailed` в sync-пути либо bump_attempt в replay-пути.
//!
//! Trait живёт в `bosun-core`, чтобы `ApplyCtx` мог хранить
//! `Arc<dyn HealthCheckRunner>` без зависимости на `bosun-primitives`.
//! Production-реализация (`RealHealthCheckRunner` с `std::process::Command`
//! для cmd и `ureq::Agent` для url) живёт в `bosun-primitives::health_check`
//! — там, где `ureq` уже подключён через `runr-client`.
//!
//! ## Cancellation
//!
//! Каждая итерация retry проверяет `CancellationToken` через переданную
//! ссылку (`ctx.cancel`). Если token cancelled во время sleep между
//! попытками → `HealthCheckError::Cancelled`. Это коротит как SIGTERM,
//! так и истечение `ctx.deadline` (CLI cancel'ит токен по дедлайну).

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::defers::HealthCheck;

/// Дефолт retry_count, когда оператор не указал явно. Совпадает с
/// chiit (его «hardcoded 3 attempts»).
pub const DEFAULT_RETRY_COUNT: u32 = 3;

/// Дефолт retry_interval_sec.
pub const DEFAULT_RETRY_INTERVAL_SEC: u32 = 2;

/// Дефолт timeout_sec на одну попытку.
pub const DEFAULT_TIMEOUT_SEC: u32 = 10;

/// Сколько байт stderr/тела ответа оставляем в excerpt'е. Достаточно для
/// типичного nginx/health-endpoint и предотвращает разрастание логов.
pub const EXCERPT_LIMIT: usize = 4096;

/// Ошибка health-check'а после всех retry'ев. Каждая категория несёт
/// достаточно информации для post-mortem'а: оператор увидит, *что* именно
/// провалилось и *сколько раз* пытались.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HealthCheckError {
    /// Cmd-вариант: процесс завершился с ненулевым exit-code на всех
    /// `attempts` попытках. `stderr_excerpt` — обрезанный stderr последней
    /// попытки.
    #[error(
        "cmd health-check exited with code {exit_code} after {attempts} attempts: {stderr_excerpt}"
    )]
    CmdExitNonZero {
        exit_code: i32,
        attempts: u32,
        stderr_excerpt: String,
    },
    /// Cmd-вариант: каждая попытка повисла дольше `timeout_sec`. Процесс
    /// был убит, retry'и исчерпаны.
    #[error("cmd health-check timed out on every attempt ({attempts} total): {argv:?}")]
    CmdTimeout { argv: Vec<String>, attempts: u32 },
    /// Cmd-вариант: бинарь не запустился (ENOENT, permission denied и т.п.).
    /// До первого byte'а — retry смысла не имеют.
    #[error("cmd health-check failed to spawn: {0}")]
    CmdSpawnError(#[source] std::io::Error),
    /// Url-вариант: status code не совпал с ожидаемым на всех `attempts`
    /// попытках. `actual` — последний полученный код.
    #[error("url health-check expected status {expected} but got {actual} after {attempts} attempts: {url}")]
    UrlBadStatus {
        url: String,
        actual: u16,
        expected: u16,
        attempts: u32,
    },
    /// Url-вариант: transport-ошибка (connection refused, DNS, timeout
    /// на уровне сокета) на всех `attempts` попытках.
    #[error("url health-check transport error after {attempts} attempts on {url}: {reason}")]
    UrlTransport {
        url: String,
        attempts: u32,
        reason: String,
    },
    /// Прервано через `CancellationToken` (SIGTERM/SIGINT/deadline). Не
    /// deferrable, не failure — отдельный класс для CLI exit-code 130
    /// и для replay-цикла, чтобы он не bump'ал attempt.
    #[error("health-check cancelled by deadline or signal")]
    Cancelled,
}

/// Контракт исполнителя health-check'ов. DI-точка для тестов:
/// production-реализация (`RealHealthCheckRunner`) поверх std + ureq
/// живёт в `bosun-primitives`; тесты собирают мок-runner и проверяют
/// retry-логику, cancellation, маппинг ошибок без зависимости от сети
/// и системных бинарей.
///
/// Метод `run` возвращает `Ok(())` при первом успешном probe'е или
/// `Err(HealthCheckError)` если все retry'и исчерпаны.
pub trait HealthCheckRunner: Send + Sync {
    /// Запустить health-check. `check` — спецификация (cmd либо url с
    /// timeouts/retries). `cancel` — токен для прерывания во время sleep
    /// между попытками.
    fn run(&self, check: &HealthCheck, cancel: &CancellationToken) -> Result<(), HealthCheckError>;
}

/// No-op runner для случаев, когда CLI не сконфигурировал production-
/// реализацию: ApplyCtx::new по умолчанию подставляет именно его. Любой
/// health-check возвращает Ok — это безопасный default: тесты, которые
/// не упоминают health_check, не должны падать, а production-CLI
/// обязан явно подключить `RealHealthCheckRunner`.
///
/// TODO: builder-pattern для ApplyCtx (см. модульный комментарий
/// `primitive.rs`). Текущая Noop-семантика — временный костыль, пока
/// builder не введён и не сделает выбор runner'а обязательным на стороне
/// CLI.
#[derive(Default, Debug)]
pub struct NoopHealthCheckRunner;

impl HealthCheckRunner for NoopHealthCheckRunner {
    fn run(
        &self,
        _check: &HealthCheck,
        _cancel: &CancellationToken,
    ) -> Result<(), HealthCheckError> {
        Ok(())
    }
}

/// Применить дефолты к необязательным timeout/retry полям. Используется
/// в `RealHealthCheckRunner` и в mock'ах: тестам полезно вычислить те же
/// значения через единую функцию.
pub fn resolve_defaults(
    timeout_sec: Option<u32>,
    retry_count: Option<u32>,
    retry_interval_sec: Option<u32>,
) -> (Duration, u32, Duration) {
    let timeout = Duration::from_secs(timeout_sec.unwrap_or(DEFAULT_TIMEOUT_SEC) as u64);
    let retries = retry_count.unwrap_or(DEFAULT_RETRY_COUNT).max(1);
    let interval =
        Duration::from_secs(retry_interval_sec.unwrap_or(DEFAULT_RETRY_INTERVAL_SEC) as u64);
    (timeout, retries, interval)
}

/// Sleep с проверкой cancel: спит шагами по 50 мс, ранний выход при
/// cancel. Возвращает `true` если sleep дошёл до конца, `false` если
/// был прерван.
///
/// Вынесено в отдельную функцию, чтобы Real-реализация health_check
/// и любой DI-runner мог переиспользовать ту же логику.
pub fn cancellable_sleep(total: Duration, cancel: &CancellationToken) -> bool {
    const POLL: Duration = Duration::from_millis(50);
    let start = std::time::Instant::now();
    while start.elapsed() < total {
        if cancel.is_cancelled() {
            return false;
        }
        let remaining = total.saturating_sub(start.elapsed());
        std::thread::sleep(remaining.min(POLL));
    }
    !cancel.is_cancelled()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_uses_constants_when_none() {
        let (timeout, retries, interval) = resolve_defaults(None, None, None);
        assert_eq!(timeout, Duration::from_secs(DEFAULT_TIMEOUT_SEC as u64));
        assert_eq!(retries, DEFAULT_RETRY_COUNT);
        assert_eq!(
            interval,
            Duration::from_secs(DEFAULT_RETRY_INTERVAL_SEC as u64)
        );
    }

    #[test]
    fn resolve_defaults_respects_overrides() {
        let (timeout, retries, interval) = resolve_defaults(Some(20), Some(5), Some(7));
        assert_eq!(timeout, Duration::from_secs(20));
        assert_eq!(retries, 5);
        assert_eq!(interval, Duration::from_secs(7));
    }

    #[test]
    fn resolve_defaults_retry_count_zero_promoted_to_one() {
        // Защита от петли с нулевым retry: при retry_count=0 не должно
        // быть «никаких попыток» — это бессмыслица. Минимум 1 попытка.
        let (_, retries, _) = resolve_defaults(None, Some(0), None);
        assert_eq!(retries, 1);
    }

    #[test]
    fn noop_runner_returns_ok_for_cmd() {
        let runner = NoopHealthCheckRunner;
        let check = HealthCheck::Cmd {
            cmd: vec!["true".to_string()],
            timeout_sec: None,
            retry_count: None,
            retry_interval_sec: None,
        };
        let cancel = CancellationToken::new();
        assert!(runner.run(&check, &cancel).is_ok());
    }

    #[test]
    fn noop_runner_returns_ok_for_url() {
        let runner = NoopHealthCheckRunner;
        let check = HealthCheck::Url {
            url: "http://localhost".to_string(),
            expected_status: Some(200),
            timeout_sec: None,
            retry_count: None,
            retry_interval_sec: None,
        };
        let cancel = CancellationToken::new();
        assert!(runner.run(&check, &cancel).is_ok());
    }

    #[test]
    fn cancellable_sleep_returns_false_when_cancelled_mid_sleep() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        // Параллельный thread cancel'ит токен через 50 мс.
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel_clone.cancel();
        });
        let ok = cancellable_sleep(Duration::from_secs(5), &cancel);
        handle.join().unwrap();
        assert!(!ok, "sleep должен вернуть false при cancel'е");
    }

    #[test]
    fn cancellable_sleep_returns_true_when_completes() {
        let cancel = CancellationToken::new();
        let ok = cancellable_sleep(Duration::from_millis(80), &cancel);
        assert!(ok, "короткий sleep должен дойти до конца");
    }

    #[test]
    fn cancellable_sleep_returns_false_immediately_if_already_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let started = std::time::Instant::now();
        let ok = cancellable_sleep(Duration::from_secs(5), &cancel);
        assert!(!ok);
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "должны выйти быстро, заняло {:?}",
            started.elapsed(),
        );
    }
}
