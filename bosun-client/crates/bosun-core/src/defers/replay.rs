//! Replay-цикл по журналу defers.
//!
//! Алгоритм описан в design-секции «Replay протокол»:
//! 1. `list_sorted()` — entries по lex-порядку имён файлов (даёт
//!    приоритет за счёт префикса `r0`/`r1`/`r2`/`c0`/`d0`).
//! 2. Для каждого entry — `dispatch` через переданного клиента.
//! 3. Ok → remove + counter++; ClientUnavailable → skip; Action(err) →
//!    bump_attempt; при достижении max — move_to_manual_clear.
//! 4. Любая ошибка одной записи не прерывает loop.

use tracing::{info_span, warn};

use super::action::{dispatch, DispatchClient, DispatchError};
use super::journal::{DeferError, Journal};

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
}

/// Прогон одного цикла replay по журналу.
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

        match dispatch(&entry, client) {
            Ok(()) => match journal.remove(&entry) {
                Ok(()) => {
                    report.executed = report.executed.saturating_add(1);
                    tracing::info!(result = "ok", "defer executed");
                }
                Err(e) => {
                    warn!(error = %e, "defer succeeded but remove failed");
                    return Err(e);
                }
            },
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

    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::defers::action::{DispatchClient, DispatchError};
    use crate::defers::format::{make_id, DeferAction, DeferEntry, CURRENT_SPEC_VERSION};
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
}
