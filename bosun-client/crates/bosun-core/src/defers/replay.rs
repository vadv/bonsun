//! Replay-цикл по журналу defers.
//!
//! Алгоритм описан в design-секции «Replay протокол»:
//! 1. `list_sorted()` — entries по lex-порядку имён файлов (даёт
//!    приоритет за счёт префикса `r0`/`r1`/`r2`/`c0`/`d0`).
//! 2. Для каждого entry — `dispatch` через переданного клиента.
//! 3. Если у entry задан `health_check` — после успешного dispatch'а
//!    запускается probe. Failure → bump_attempt (как у обычного action-fail).
//! 4. Ok → remove + counter++; ClientUnavailable → skip; Action(err) →
//!    bump_attempt; при достижении max — move_to_manual_clear.
//! 5. Любая ошибка одной записи не прерывает loop.
//!
//! Health-check в replay-пути запускается ПОСЛЕ успешного dispatch'а
//! (sync-путь восстановлен через retry на следующем цикле). Если
//! `Cancelled` — defer не bump'аем (это сигнал прервать процесс, не
//! признак провала health-check'а как такового).

use tokio_util::sync::CancellationToken;
use tracing::{info_span, warn};

use super::action::{dispatch, DispatchClient, DispatchError};
use super::journal::{DeferError, Journal};
use crate::health_check::{HealthCheckError, HealthCheckRunner, NoopHealthCheckRunner};

/// Сводка по одному вызову replay.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplayReport {
    /// Сколько записей успешно выполнено и удалено из журнала.
    pub executed: u32,
    /// Сколько раз клиент был недоступен (transient).
    pub skipped_unavailable: u32,
    /// Сколько записей провалилось с bump_attempt без promotion.
    pub failed: u32,
    /// Сколько записей переведено в `.manual_clear`.
    pub promoted_to_manual_clear: u32,
    /// Сколько раз health-check после успешного dispatch'а провалился
    /// (bump_attempt). Считается отдельно от `failed`, чтобы оператор
    /// видел разницу между «restart упал» и «restart прошёл, но
    /// /healthz ответил 500».
    pub health_check_failed: u32,
}

/// Прогон одного цикла replay по журналу без health-check'а (Phase D-G
/// поведение). Это тонкая обёртка над [`replay_with_health_check`],
/// подставляющая [`NoopHealthCheckRunner`]: тесты Phase D-G и любой
/// caller, которому health-check не нужен, остаются без изменений.
///
/// Errors:
/// - `Err(DeferError)` возвращается только при невозможности прочитать
///   директорию журнала или при системных I/O-проблемах на самом
///   journal-уровне. Отдельные сбои dispatch не прерывают цикл и
///   отражаются в `ReplayReport`.
pub fn replay<C: DispatchClient + ?Sized>(
    journal: &Journal,
    client: &C,
) -> Result<ReplayReport, DeferError> {
    let cancel = CancellationToken::new();
    replay_with_health_check(journal, client, &NoopHealthCheckRunner, &cancel)
}

/// Phase I: replay с health-check'ом после успешного dispatch'а.
///
/// Если `entry.health_check` задан — после `Ok(()) от dispatch'а запускается
/// probe через `health_check_runner`:
/// - `Ok(())` → defer удаляется из журнала (как обычно).
/// - `Err(HealthCheckError::Cancelled)` → запись остаётся, attempt НЕ
///   bump'ается, цикл прерывает оставшиеся entries (точно так же, как
///   при cancel'е во время action'а — это глобальный сигнал).
/// - `Err(_)` иное → `bump_attempt` с описанием health-check ошибки.
///   При превышении `max_attempts` → `move_to_manual_clear`.
///
/// Семантика идентична action-fail'у: dispatch уже «случился», но
/// system'у не довёл до здорового состояния. Оператор увидит запись в
/// `bosun status` с reason'ом hc-failure.
pub fn replay_with_health_check<C, H>(
    journal: &Journal,
    client: &C,
    health_check_runner: &H,
    cancel: &CancellationToken,
) -> Result<ReplayReport, DeferError>
where
    C: DispatchClient + ?Sized,
    H: HealthCheckRunner + ?Sized,
{
    let entries = journal.list_sorted()?;
    let mut report = ReplayReport::default();

    for entry in entries {
        let _span = info_span!(
            "defer",
            id = %entry.id,
            action = entry.action.filename_slug(),
            target = %entry.target,
        )
        .entered();

        // Глобальный cancel: прервать оставшиеся entries. Уже обработанные
        // записи остались в их финальном состоянии (выполненные удалены,
        // упавшие bump'нуты).
        if cancel.is_cancelled() {
            tracing::info!(result = "cancelled", "replay cancelled mid-loop");
            break;
        }

        match dispatch(&entry, client) {
            Ok(()) => {
                // Опциональный health-check после успешного action'а.
                if let Some(check) = entry.health_check.as_ref() {
                    match health_check_runner.run(check, cancel) {
                        Ok(()) => {
                            // health-check прошёл — удаляем запись.
                        }
                        Err(HealthCheckError::Cancelled) => {
                            // Cancelled — запись остаётся для следующего цикла.
                            // attempt НЕ bump'ается: это не настоящий fail,
                            // а сигнал «прервите процесс».
                            tracing::info!(
                                result = "cancelled",
                                "defer dispatch ok, health-check cancelled, kept for next cycle",
                            );
                            continue;
                        }
                        Err(err) => {
                            let reason = format!("health-check failed: {err}");
                            let updated = journal.bump_attempt(&entry, &reason)?;
                            if updated.attempt_count >= updated.max_attempts {
                                journal.move_to_manual_clear(&updated)?;
                                report.promoted_to_manual_clear =
                                    report.promoted_to_manual_clear.saturating_add(1);
                                tracing::warn!(
                                    attempt = updated.attempt_count,
                                    max = updated.max_attempts,
                                    error = %err,
                                    result = "manual_clear",
                                    "defer dispatch ok but health-check exhausted retries",
                                );
                            } else {
                                report.health_check_failed =
                                    report.health_check_failed.saturating_add(1);
                                tracing::warn!(
                                    attempt = updated.attempt_count,
                                    max = updated.max_attempts,
                                    error = %err,
                                    result = "health_check_failed",
                                    "defer dispatch ok but health-check failed, will retry",
                                );
                            }
                            continue;
                        }
                    }
                }
                match journal.remove(&entry) {
                    Ok(()) => {
                        report.executed = report.executed.saturating_add(1);
                        tracing::info!(result = "ok", "defer executed");
                    }
                    Err(e) => {
                        warn!(error = %e, "defer succeeded but remove failed");
                        return Err(e);
                    }
                }
            }
            Err(DispatchError::ClientUnavailable) => {
                report.skipped_unavailable = report.skipped_unavailable.saturating_add(1);
                tracing::info!(result = "client_unavailable", "defer skipped");
            }
            Err(DispatchError::Action(err)) => {
                let updated = journal.bump_attempt(&entry, &err)?;
                if updated.attempt_count >= updated.max_attempts {
                    journal.move_to_manual_clear(&updated)?;
                    report.promoted_to_manual_clear =
                        report.promoted_to_manual_clear.saturating_add(1);
                    tracing::warn!(
                        attempt = updated.attempt_count,
                        max = updated.max_attempts,
                        error = %err,
                        result = "manual_clear",
                        "defer promoted to manual_clear",
                    );
                } else {
                    report.failed = report.failed.saturating_add(1);
                    tracing::warn!(
                        attempt = updated.attempt_count,
                        max = updated.max_attempts,
                        error = %err,
                        result = "failed",
                        "defer failed, will retry",
                    );
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Mutex;

    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::defers::action::{DispatchClient, DispatchError};
    use crate::defers::format::{
        make_id, DeferAction, DeferEntry, HealthCheck, CURRENT_SPEC_VERSION,
    };
    use crate::defers::journal::{count_files_with_extension, Journal};
    use crate::defers::priority::DeferPriority;

    fn make_entry(
        init_system: &str,
        action: DeferAction,
        target: &str,
        priority: DeferPriority,
        max_attempts: u32,
    ) -> DeferEntry {
        DeferEntry {
            spec_version: CURRENT_SPEC_VERSION,
            id: make_id(init_system, &action, target),
            action,
            init_system: init_system.to_string(),
            target: target.to_string(),
            validate_cmd: None,
            health_check: None,
            priority,
            enqueued_at: Utc::now(),
            enqueued_by: vec![],
            attempt_count: 0,
            max_attempts,
        }
    }

    fn make_entry_with_hc(
        init_system: &str,
        action: DeferAction,
        target: &str,
        priority: DeferPriority,
        max_attempts: u32,
        hc: HealthCheck,
    ) -> DeferEntry {
        let mut e = make_entry(init_system, action, target, priority, max_attempts);
        e.health_check = Some(hc);
        e
    }

    struct FakeClient<F: Fn(&DeferEntry) -> Result<(), DispatchError>> {
        inner: F,
        calls: RefCell<Vec<String>>,
    }

    impl<F> FakeClient<F>
    where
        F: Fn(&DeferEntry) -> Result<(), DispatchError>,
    {
        fn new(inner: F) -> Self {
            Self {
                inner,
                calls: RefCell::new(vec![]),
            }
        }
    }

    impl<F> DispatchClient for FakeClient<F>
    where
        F: Fn(&DeferEntry) -> Result<(), DispatchError>,
    {
        fn dispatch(&self, entry: &DeferEntry) -> Result<(), DispatchError> {
            self.calls.borrow_mut().push(entry.id.clone());
            (self.inner)(entry)
        }
    }

    /// Тестовый health-check runner: возвращает заданный результат на
    /// каждый вызов и считает обращения.
    struct FakeHealthCheck {
        result: Mutex<Result<(), HealthCheckError>>,
        calls: Mutex<u32>,
    }

    impl FakeHealthCheck {
        fn ok() -> Self {
            Self {
                result: Mutex::new(Ok(())),
                calls: Mutex::new(0),
            }
        }
        fn failing(err: HealthCheckError) -> Self {
            Self {
                result: Mutex::new(Err(err)),
                calls: Mutex::new(0),
            }
        }
        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl HealthCheckRunner for FakeHealthCheck {
        fn run(
            &self,
            _check: &HealthCheck,
            _cancel: &CancellationToken,
        ) -> Result<(), HealthCheckError> {
            *self.calls.lock().unwrap() += 1;
            // Возвращаем копию из result; для одноразовых err'ов это
            // невозможно (HealthCheckError не Clone), поэтому используем
            // swap'-семантику: после первого вызова результат становится
            // Ok. Для текущих тестов этого хватает.
            let current = std::mem::replace(&mut *self.result.lock().unwrap(), Ok(()));
            current
        }
    }

    /// Multi-shot mock health-check: на каждый вызов возвращает заданную
    /// ошибку и продолжает делать это бесконечно. Нужен для прогона
    /// нескольких replay-циклов подряд (e2e bump'а).
    struct AlwaysFailingHealthCheck;
    impl HealthCheckRunner for AlwaysFailingHealthCheck {
        fn run(
            &self,
            _check: &HealthCheck,
            _cancel: &CancellationToken,
        ) -> Result<(), HealthCheckError> {
            Err(HealthCheckError::UrlBadStatus {
                url: "http://127.0.0.1/h".to_string(),
                actual: 500,
                expected: 200,
                attempts: 1,
            })
        }
    }

    fn open() -> (TempDir, Journal) {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        (tmp, journal)
    }

    #[test]
    fn replay_success_removes_file_and_counts_executed() {
        let (tmp, journal) = open();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
        );
        journal.enqueue(entry.clone()).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let report = replay(&journal, &client).unwrap();
        assert_eq!(report.executed, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.skipped_unavailable, 0);
        assert_eq!(report.promoted_to_manual_clear, 0);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
    }

    #[test]
    fn replay_client_unavailable_keeps_file_and_does_not_bump() {
        let (tmp, journal) = open();
        let entry = make_entry(
            "runr",
            DeferAction::Restart,
            "postgres",
            DeferPriority::Restart,
            3,
        );
        journal.enqueue(entry.clone()).unwrap();

        let client = FakeClient::new(|_| Err(DispatchError::ClientUnavailable));
        let report = replay(&journal, &client).unwrap();
        assert_eq!(report.skipped_unavailable, 1);
        assert_eq!(report.executed, 0);
        assert_eq!(report.failed, 0);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list[0].attempt_count, 0);
    }

    #[test]
    fn replay_action_failure_bumps_attempt_and_increments_failed() {
        let (tmp, journal) = open();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
        );
        journal.enqueue(entry.clone()).unwrap();

        let client = FakeClient::new(|_| Err(DispatchError::Action("boom".into())));
        let report = replay(&journal, &client).unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(report.promoted_to_manual_clear, 0);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list[0].attempt_count, 1);
    }

    #[test]
    fn replay_promotes_to_manual_clear_when_attempts_exhausted() {
        let (tmp, journal) = open();
        let mut entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
        );
        entry.attempt_count = 2; // следующий bump => 3 == max_attempts → promotion.
        journal.enqueue(entry.clone()).unwrap();

        let client = FakeClient::new(|_| Err(DispatchError::Action("boom".into())));
        let report = replay(&journal, &client).unwrap();
        assert_eq!(report.promoted_to_manual_clear, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
        assert_eq!(count_files_with_extension(tmp.path(), "manual_clear"), 1);
    }

    #[test]
    fn replay_continues_after_per_entry_failure() {
        let (tmp, journal) = open();
        let good = make_entry(
            "systemd",
            DeferAction::Reload,
            "good",
            DeferPriority::Reload,
            3,
        );
        let bad = make_entry(
            "systemd",
            DeferAction::Restart,
            "bad",
            DeferPriority::Restart,
            3,
        );
        journal.enqueue(good.clone()).unwrap();
        journal.enqueue(bad.clone()).unwrap();

        // Restart (`r0`) идёт первым и фейлится, reload (`r2`) — успех.
        let client = FakeClient::new(|entry| {
            if entry.target == "bad" {
                Err(DispatchError::Action("boom".into()))
            } else {
                Ok(())
            }
        });
        let report = replay(&journal, &client).unwrap();
        assert_eq!(report.executed, 1);
        assert_eq!(report.failed, 1);
        // `bad` остался с bumped attempt, `good` удалён.
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list[0].target, "bad");
        assert_eq!(list[0].attempt_count, 1);
    }

    #[test]
    fn replay_respects_priority_order() {
        let (_tmp, journal) = open();
        let reload = make_entry(
            "systemd",
            DeferAction::Reload,
            "z",
            DeferPriority::Reload,
            3,
        );
        let restart = make_entry(
            "systemd",
            DeferAction::Restart,
            "a",
            DeferPriority::Restart,
            3,
        );
        let command = make_entry(
            "",
            DeferAction::Command {
                argv: vec!["echo".into()],
            },
            "cmd-x",
            DeferPriority::Command,
            3,
        );

        journal.enqueue(reload).unwrap();
        journal.enqueue(restart).unwrap();
        journal.enqueue(command).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let _ = replay(&journal, &client).unwrap();
        let calls = client.calls.borrow();
        // r0 → r2 → c0 (по lex-порядку префиксов).
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], "systemd.restart:a");
        assert_eq!(calls[1], "systemd.reload:z");
        assert_eq!(calls[2], "command.run:cmd-x");
    }

    #[test]
    fn replay_skips_corrupt_json() {
        let (tmp, journal) = open();
        let good = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
        );
        journal.enqueue(good.clone()).unwrap();
        std::fs::write(
            tmp.path().join("0r-systemd.restart:bad.deferred"),
            b"corrupt",
        )
        .unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let report = replay(&journal, &client).unwrap();
        assert_eq!(report.executed, 1);
        // Повреждённый файл не считается «выполненным», но и не падает.
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
    }

    // -- Phase I: health-check после dispatch --------------------------------

    fn hc_url() -> HealthCheck {
        HealthCheck::Url {
            url: "http://127.0.0.1/h".to_string(),
            expected_status: Some(200),
            timeout_sec: Some(1),
            retry_count: Some(1),
            retry_interval_sec: Some(0),
        }
    }

    #[test]
    fn replay_with_hc_dispatch_ok_and_hc_ok_executes_and_removes() {
        let (tmp, journal) = open();
        let entry = make_entry_with_hc(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
            hc_url(),
        );
        journal.enqueue(entry.clone()).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let hc = FakeHealthCheck::ok();
        let cancel = CancellationToken::new();
        let report = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();

        assert_eq!(report.executed, 1);
        assert_eq!(report.health_check_failed, 0);
        assert_eq!(hc.calls(), 1, "health-check должен быть вызван ровно раз");
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
    }

    #[test]
    fn replay_with_hc_dispatch_ok_but_hc_fails_bumps_attempt() {
        let (tmp, journal) = open();
        let entry = make_entry_with_hc(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
            hc_url(),
        );
        journal.enqueue(entry.clone()).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let hc = FakeHealthCheck::failing(HealthCheckError::UrlBadStatus {
            url: "http://127.0.0.1/h".to_string(),
            actual: 500,
            expected: 200,
            attempts: 3,
        });
        let cancel = CancellationToken::new();
        let report = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();

        assert_eq!(report.executed, 0);
        assert_eq!(report.health_check_failed, 1);
        assert_eq!(report.promoted_to_manual_clear, 0);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list[0].attempt_count, 1);
    }

    #[test]
    fn replay_with_hc_failure_promotes_to_manual_clear_when_exhausted() {
        let (tmp, journal) = open();
        let mut entry = make_entry_with_hc(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
            hc_url(),
        );
        entry.attempt_count = 2; // следующий bump доведёт до 3 == max.
        journal.enqueue(entry).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let hc = FakeHealthCheck::failing(HealthCheckError::UrlBadStatus {
            url: "http://127.0.0.1/h".to_string(),
            actual: 500,
            expected: 200,
            attempts: 3,
        });
        let cancel = CancellationToken::new();
        let report = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();

        assert_eq!(report.promoted_to_manual_clear, 1);
        assert_eq!(report.health_check_failed, 0);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
        assert_eq!(count_files_with_extension(tmp.path(), "manual_clear"), 1);
    }

    #[test]
    fn replay_with_hc_cancelled_keeps_entry_without_bump() {
        // health-check вернул Cancelled → defer остаётся, attempt НЕ
        // bump'ается, replay завершает текущую запись и идёт дальше
        // (continue), но global cancel в начале loop'а его не остановил.
        let (tmp, journal) = open();
        let entry = make_entry_with_hc(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
            hc_url(),
        );
        journal.enqueue(entry).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let hc = FakeHealthCheck::failing(HealthCheckError::Cancelled);
        let cancel = CancellationToken::new();
        let report = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();

        assert_eq!(report.executed, 0);
        assert_eq!(report.health_check_failed, 0);
        assert_eq!(report.promoted_to_manual_clear, 0);
        // Файл остался, attempt не bump'ался.
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list[0].attempt_count, 0);
    }

    #[test]
    fn replay_with_hc_no_health_check_skips_runner() {
        // Если entry.health_check = None, runner не дёргается вовсе —
        // ведём себя как до Phase I.
        let (tmp, journal) = open();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
        );
        journal.enqueue(entry).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let hc = FakeHealthCheck::ok();
        let cancel = CancellationToken::new();
        let report = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();

        assert_eq!(report.executed, 1);
        assert_eq!(
            hc.calls(),
            0,
            "runner не должен быть вызван без health_check"
        );
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
    }

    #[test]
    fn replay_with_global_cancel_aborts_loop() {
        // Несколько entries в журнале; cancel выставлен до replay'я →
        // loop завершается сразу.
        let (tmp, journal) = open();
        for name in ["a", "b", "c"] {
            let e = make_entry(
                "systemd",
                DeferAction::Restart,
                name,
                DeferPriority::Restart,
                3,
            );
            journal.enqueue(e).unwrap();
        }
        let client = FakeClient::new(|_| Ok(()));
        let hc = FakeHealthCheck::ok();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let report = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();
        assert_eq!(report.executed, 0);
        // Все файлы остались на месте.
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 3);
    }

    #[test]
    fn replay_with_hc_progresses_bump_attempt_across_multiple_cycles() {
        // E2E: три прогона replay подряд, каждый раз dispatch=Ok, hc=fail.
        // Первый цикл → attempt_count 0→1 (failed=1).
        // Второй цикл → attempt 1→2 (failed=2, всё ещё < max=3).
        // Третий цикл → attempt 2→3 → manual_clear promotion.
        let (tmp, journal) = open();
        let entry = make_entry_with_hc(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
            3,
            hc_url(),
        );
        journal.enqueue(entry).unwrap();

        let client = FakeClient::new(|_| Ok(()));
        let hc = AlwaysFailingHealthCheck;
        let cancel = CancellationToken::new();

        // Цикл 1.
        let r1 = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();
        assert_eq!(r1.health_check_failed, 1);
        assert_eq!(r1.promoted_to_manual_clear, 0);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list[0].attempt_count, 1);

        // Цикл 2.
        let r2 = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();
        assert_eq!(r2.health_check_failed, 1);
        assert_eq!(r2.promoted_to_manual_clear, 0);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list[0].attempt_count, 2);

        // Цикл 3 — должен промоутнуть.
        let r3 = replay_with_health_check(&journal, &client, &hc, &cancel).unwrap();
        assert_eq!(r3.health_check_failed, 0);
        assert_eq!(r3.promoted_to_manual_clear, 1);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
        assert_eq!(count_files_with_extension(tmp.path(), "manual_clear"), 1);
    }
}
