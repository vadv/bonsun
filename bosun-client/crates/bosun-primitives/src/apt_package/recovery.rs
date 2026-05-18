//! Recovery-команды поверх apt-get install:
//! - `dpkg --configure -a` — починить half-configured state.
//! - `apt-get update` с экспоненциальным backoff'ом — обновить apt-lists,
//!   когда install встретил candidate-miss.

use std::thread;
use std::time::{Duration, Instant};

use bosun_core::PrimitiveError;
use rand::Rng;

use super::exec::CommandRunner;

/// Per-attempt deadline на `dpkg --configure -a`. Спека требует 60s.
const DPKG_CONFIGURE_TIMEOUT: Duration = Duration::from_secs(60);

/// Параметры retry-loop'а для `apt-get update`. Production-конфигурация —
/// `default()` (см. impl). Тесты переопределяют backoff'ы на ноль, чтобы
/// не ждать 15 секунд за unit-проверкой.
#[derive(Clone, Debug)]
pub struct AptUpdatePolicy {
    pub per_attempt_timeout: Duration,
    pub max_attempts: u32,
    /// `backoffs[i]` — пауза между attempt `i` и attempt `i+1`. Если `i`
    /// выходит за длину массива, используется последнее значение.
    pub backoffs: &'static [Duration],
    /// Размер jitter'а в процентах от базового backoff'а. Фактическая
    /// пауза — `base * (1 + uniform(-jitter, +jitter))` с верхним
    /// зажимом на `base * 2`. На 60k нод фиксированный backoff пробивает
    /// mirror одной волной retry'ев; ±25% размазывает их по 25%-секундному
    /// окну. `jitter_pct=0` отключает рандом (используется в тестах).
    pub jitter_pct: u32,
}

/// Production-конфигурация backoff'ов: 5s → 10s → 20s по spec'у.
const DEFAULT_APT_UPDATE_BACKOFFS: &[Duration] = &[
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

/// Дефолтный jitter — ±25% от базового backoff'а.
const DEFAULT_JITTER_PCT: u32 = 25;

impl Default for AptUpdatePolicy {
    fn default() -> Self {
        Self {
            per_attempt_timeout: Duration::from_secs(30),
            max_attempts: 3,
            backoffs: DEFAULT_APT_UPDATE_BACKOFFS,
            jitter_pct: DEFAULT_JITTER_PCT,
        }
    }
}

/// Применить jitter к backoff'у: `base * (1 + jitter * uniform(-1, +1))`,
/// зажимая результат сверху на `base * 2`. Если `jitter_pct == 0` или
/// `base == 0`, возвращаем `base` как есть — это важно для тестов и для
/// последней попытки.
fn jittered_backoff(base: Duration, jitter_pct: u32) -> Duration {
    if jitter_pct == 0 || base.is_zero() {
        return base;
    }
    let mut rng = rand::thread_rng();
    let r: f64 = rng.gen_range(-1.0..=1.0);
    let factor = 1.0 + r * (f64::from(jitter_pct) / 100.0);
    let base_secs = base.as_secs_f64();
    let scaled = base_secs * factor;
    // Зажимаем сверху на base*2: убегающее значение от случайной
    // выборки не должно перевешивать сам backoff.
    let max_secs = base_secs * 2.0;
    let bounded = scaled.clamp(0.0, max_secs);
    Duration::from_secs_f64(bounded)
}

/// Выполнить `dpkg --configure -a`. Per-attempt deadline = 60s или
/// глобальный `deadline` (что раньше).
pub fn run_dpkg_configure_a(
    runner: &dyn CommandRunner,
    global_deadline: Instant,
) -> Result<(), PrimitiveError> {
    let attempt_deadline = min_deadline(global_deadline, Instant::now() + DPKG_CONFIGURE_TIMEOUT);
    let result = runner.run("dpkg", &["--configure", "-a"], attempt_deadline)?;
    if result.exit_code == Some(0) {
        return Ok(());
    }
    Err(PrimitiveError::Exec {
        reason: "dpkg --configure -a failed".into(),
        exit: result.exit_code,
        stderr_excerpt: stderr_excerpt(&result.stderr),
    })
}

/// Выполнить `apt-get update` с retry-loop'ом по default-policy.
///
/// Поверх встроенного `APT::Acquire::Retries=3` мы добавляем внешний цикл
/// на случай, когда внутренние попытки apt все упали по разным сетевым
/// причинам. Логика категоризации retriable/non-retriable — по spec.
pub fn run_apt_update_with_retries(
    runner: &dyn CommandRunner,
    global_deadline: Instant,
) -> Result<(), PrimitiveError> {
    run_apt_update_with_policy(runner, global_deadline, &AptUpdatePolicy::default())
}

/// Тот же `apt-get update`, но с явным `AptUpdatePolicy`. Нужен для тестов:
/// в production-конфигурации backoff'ы 5s/10s/20s превратили бы unit-тесты
/// в минутные. Тест передаёт policy с нулевыми backoff'ами.
pub fn run_apt_update_with_policy(
    runner: &dyn CommandRunner,
    global_deadline: Instant,
    policy: &AptUpdatePolicy,
) -> Result<(), PrimitiveError> {
    let args = &[
        "update",
        "-q",
        "-oAPT::Acquire::Retries=3",
        "-oDPkg::Lock::Timeout=30",
    ];

    let mut last_result = None;
    for attempt in 0..policy.max_attempts {
        // Каждая попытка ограничена per-attempt deadline'ом, но не дольше
        // глобального deadline'а.
        let attempt_deadline =
            min_deadline(global_deadline, Instant::now() + policy.per_attempt_timeout);
        let result = runner.run("apt-get", args, attempt_deadline)?;

        if result.exit_code == Some(0) {
            return Ok(());
        }

        let category = classify_update_failure(&result.stderr);
        last_result = Some(result);

        match category {
            UpdateFailure::NonRetriable => break,
            UpdateFailure::Retriable => {
                // Дальше не пытаемся — это была последняя попытка.
                if attempt + 1 >= policy.max_attempts {
                    break;
                }
                let base_backoff = policy
                    .backoffs
                    .get(attempt as usize)
                    .copied()
                    .or_else(|| policy.backoffs.last().copied())
                    .unwrap_or(Duration::ZERO);
                let backoff = jittered_backoff(base_backoff, policy.jitter_pct);
                tracing::warn!(
                    attempt = attempt + 1,
                    max = policy.max_attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    "apt-get update transient failure, retrying",
                );
                // Если global_deadline уже истёк — выходим заранее, без
                // следующей попытки. Тонкость: при `backoff == ZERO`
                // и валидном (будущем) deadline'е разница между двумя
                // Instant::now() = микросекунды, и saturating_duration_since
                // даёт ~0 — корректно, без ложного Cancelled.
                if Instant::now() >= global_deadline {
                    return Err(PrimitiveError::Cancelled);
                }
                let wake_at = Instant::now() + backoff;
                let bounded_wake = min_deadline(global_deadline, wake_at);
                let dt = bounded_wake.saturating_duration_since(Instant::now());
                if !dt.is_zero() {
                    thread::sleep(dt);
                }
            }
        }
    }

    // Достали последний результат: либо вышли по non-retriable, либо
    // исчерпали попытки.
    let result = last_result.unwrap_or_else(|| super::exec::CommandResult {
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
    });
    Err(PrimitiveError::Exec {
        reason: "apt-get update failed after retries".into(),
        exit: result.exit_code,
        stderr_excerpt: stderr_excerpt(&result.stderr),
    })
}

/// Категория провала `apt-get update` для retry-решения.
#[derive(Debug, PartialEq, Eq)]
enum UpdateFailure {
    Retriable,
    NonRetriable,
}

/// Классифицировать stderr `apt-get update`.
///
/// Retriable: транзиентные сетевые проблемы — `connection refused`,
/// `timed out`, `Temporary failure in name resolution`, `503`, `504`,
/// `Hash Sum mismatch`; также пустой stderr (например, TCP reset без
/// сообщения).
///
/// Non-retriable: `GPG error`, `permission denied` — это не лечится
/// повтором, нужен ручной фикс.
fn classify_update_failure(stderr: &str) -> UpdateFailure {
    if stderr.contains("GPG error") || stderr.contains("permission denied") {
        return UpdateFailure::NonRetriable;
    }
    if stderr.trim().is_empty() {
        return UpdateFailure::Retriable;
    }
    let retriable_patterns = [
        "connection refused",
        "timed out",
        "Temporary failure in name resolution",
        "503",
        "504",
        "Hash Sum mismatch",
    ];
    for p in retriable_patterns {
        if stderr.contains(p) {
            return UpdateFailure::Retriable;
        }
    }
    UpdateFailure::NonRetriable
}

/// Выбрать ближайший дедлайн (наименьший Instant).
fn min_deadline(a: Instant, b: Instant) -> Instant {
    if a < b {
        a
    } else {
        b
    }
}

/// Первые и последние 10 строк stderr — для краткого изложения в
/// `PrimitiveError::Exec.stderr_excerpt`. Если строк ≤ 20, отдаём как есть.
pub(crate) fn stderr_excerpt(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    if lines.len() <= 20 {
        return stderr.to_string();
    }
    let first_10 = lines[..10].join("\n");
    let elided = lines.len() - 20;
    let last_10 = lines[lines.len() - 10..].join("\n");
    format!("{first_10}\n... [{elided} lines elided] ...\n{last_10}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use std::sync::Mutex;

    use super::super::exec::CommandResult;
    use super::*;

    /// MockRunner — записывает все вызовы и отдаёт заранее заданные ответы.
    struct MockRunner {
        responses: Mutex<Vec<CommandResult>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandResult>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(
            &self,
            cmd: &str,
            args: &[&str],
            _deadline: Instant,
        ) -> Result<CommandResult, PrimitiveError> {
            self.calls.lock().unwrap().push((
                cmd.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                panic!("MockRunner: no more responses for {cmd} {args:?}");
            }
            Ok(responses.remove(0))
        }
    }

    fn cmdres(exit: Option<i32>, stderr: &str) -> CommandResult {
        CommandResult {
            exit_code: exit,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    const ZERO_BACKOFFS: &[Duration] = &[Duration::ZERO, Duration::ZERO, Duration::ZERO];

    /// Policy без задержек — для тестов retry-логики без реального ожидания.
    /// `jitter_pct=0` отключает рандом: тесты должны быть детерминированы.
    fn zero_backoff_policy(attempts: u32) -> AptUpdatePolicy {
        AptUpdatePolicy {
            per_attempt_timeout: Duration::from_secs(30),
            max_attempts: attempts,
            backoffs: ZERO_BACKOFFS,
            jitter_pct: 0,
        }
    }

    #[test]
    fn dpkg_configure_a_success() {
        let runner = MockRunner::new(vec![cmdres(Some(0), "")]);
        run_dpkg_configure_a(&runner, deadline()).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "dpkg");
        assert_eq!(calls[0].1, vec!["--configure", "-a"]);
    }

    #[test]
    fn dpkg_configure_a_failure_returns_exec() {
        let runner = MockRunner::new(vec![cmdres(Some(1), "dpkg: error processing")]);
        let err = run_dpkg_configure_a(&runner, deadline()).unwrap_err();
        match err {
            PrimitiveError::Exec {
                reason,
                exit,
                stderr_excerpt,
            } => {
                assert!(reason.contains("dpkg --configure -a"));
                assert_eq!(exit, Some(1));
                assert!(stderr_excerpt.contains("dpkg: error"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_first_attempt_success_makes_one_call() {
        let runner = MockRunner::new(vec![cmdres(Some(0), "")]);
        run_apt_update_with_policy(&runner, deadline(), &zero_backoff_policy(3)).unwrap();
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "apt-get");
        assert_eq!(calls[0].1[0], "update");
        assert!(calls[0]
            .1
            .iter()
            .any(|a| a.contains("APT::Acquire::Retries")));
        assert!(calls[0].1.iter().any(|a| a.contains("DPkg::Lock::Timeout")));
    }

    #[test]
    fn update_two_transient_then_success_makes_three_calls() {
        let runner = MockRunner::new(vec![
            cmdres(Some(1), "timed out"),
            cmdres(Some(1), "503"),
            cmdres(Some(0), ""),
        ]);
        run_apt_update_with_policy(&runner, deadline(), &zero_backoff_policy(3)).unwrap();
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn update_non_retriable_gpg_aborts_immediately() {
        let runner = MockRunner::new(vec![cmdres(Some(1), "W: GPG error: http://repo unsigned")]);
        let err =
            run_apt_update_with_policy(&runner, deadline(), &zero_backoff_policy(3)).unwrap_err();
        match err {
            PrimitiveError::Exec {
                reason,
                stderr_excerpt,
                ..
            } => {
                assert!(reason.contains("apt-get update"));
                assert!(stderr_excerpt.contains("GPG error"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Один call — без retry.
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn update_non_retriable_permission_denied_aborts_immediately() {
        let runner = MockRunner::new(vec![cmdres(Some(1), "E: permission denied on /var/lib")]);
        let err =
            run_apt_update_with_policy(&runner, deadline(), &zero_backoff_policy(3)).unwrap_err();
        assert!(matches!(err, PrimitiveError::Exec { .. }));
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn update_empty_stderr_is_retriable_and_exhausts_attempts() {
        let runner = MockRunner::new(vec![
            cmdres(Some(1), ""),
            cmdres(Some(1), ""),
            cmdres(Some(1), ""),
        ]);
        let err =
            run_apt_update_with_policy(&runner, deadline(), &zero_backoff_policy(3)).unwrap_err();
        match err {
            PrimitiveError::Exec { reason, .. } => assert!(reason.contains("after retries")),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn update_503_is_retriable() {
        assert_eq!(
            classify_update_failure("503 Service Unavailable"),
            UpdateFailure::Retriable
        );
        assert_eq!(
            classify_update_failure("504 Gateway Timeout"),
            UpdateFailure::Retriable
        );
    }

    #[test]
    fn update_hash_sum_mismatch_is_retriable() {
        assert_eq!(
            classify_update_failure("Hash Sum mismatch"),
            UpdateFailure::Retriable
        );
    }

    #[test]
    fn update_connection_refused_is_retriable() {
        assert_eq!(
            classify_update_failure("E: Failed to fetch http://... connection refused"),
            UpdateFailure::Retriable
        );
    }

    #[test]
    fn update_dns_failure_is_retriable() {
        assert_eq!(
            classify_update_failure("Temporary failure in name resolution"),
            UpdateFailure::Retriable
        );
    }

    #[test]
    fn update_unknown_is_non_retriable() {
        assert_eq!(
            classify_update_failure("E: Something else broken"),
            UpdateFailure::NonRetriable
        );
    }

    #[test]
    fn stderr_excerpt_short_is_unchanged() {
        let s = "line1\nline2\nline3";
        assert_eq!(stderr_excerpt(s), s);
    }

    #[test]
    fn stderr_excerpt_long_elides_middle() {
        let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let stderr = lines.join("\n");
        let excerpt = stderr_excerpt(&stderr);
        assert!(excerpt.contains("line 0"));
        assert!(excerpt.contains("line 9"));
        assert!(excerpt.contains("line 20"));
        assert!(excerpt.contains("line 29"));
        assert!(excerpt.contains("10 lines elided"));
        // Среднее не должно попасть.
        assert!(!excerpt.contains("line 15"));
    }

    #[test]
    fn min_deadline_picks_smaller() {
        let now = Instant::now();
        let a = now + Duration::from_secs(10);
        let b = now + Duration::from_secs(5);
        assert_eq!(min_deadline(a, b), b);
        assert_eq!(min_deadline(b, a), b);
    }

    #[test]
    fn jittered_backoff_zero_pct_returns_base() {
        let base = Duration::from_secs(10);
        assert_eq!(jittered_backoff(base, 0), base);
    }

    #[test]
    fn jittered_backoff_zero_base_returns_zero() {
        // jitter * 0 = 0, не должно стать negative или Inf.
        assert_eq!(jittered_backoff(Duration::ZERO, 25), Duration::ZERO);
    }

    #[test]
    fn jittered_backoff_within_25pct_window_over_many_samples() {
        let base = Duration::from_secs(10);
        let base_ms = base.as_millis() as i64;
        let low = base_ms * 75 / 100;
        let high = base_ms * 125 / 100;
        for _ in 0..200 {
            let actual = jittered_backoff(base, 25).as_millis() as i64;
            assert!(
                actual >= low && actual <= high,
                "jitter out of ±25% window: actual={actual}, low={low}, high={high}",
            );
        }
    }

    #[test]
    fn jittered_backoff_never_exceeds_double_base() {
        let base = Duration::from_millis(100);
        // Даже с пикантным jitter_pct сверху зажат на base*2.
        for _ in 0..200 {
            let actual = jittered_backoff(base, 500).as_millis();
            assert!(actual <= base.as_millis() * 2);
        }
    }

    #[test]
    fn apt_update_emits_retry_event_on_transient_failure() {
        use bosun_core::tracing_test_util::{install_global_router, record_events};

        install_global_router();
        // Первая попытка падает с retriable ошибкой → ждём backoff →
        // вторая успешна. Между ними должен пройти retry-event.
        let runner = MockRunner::new(vec![cmdres(Some(1), "timed out"), cmdres(Some(0), "")]);

        let events = record_events(|| {
            run_apt_update_with_policy(&runner, deadline(), &zero_backoff_policy(3)).unwrap();
        });

        assert!(
            events.iter().any(|e| e.contains("transient failure")),
            "expected retry event; got: {events:?}",
        );
    }
}
