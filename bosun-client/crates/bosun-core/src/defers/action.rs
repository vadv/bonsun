//! Маршрутизация defer-записей в конкретного клиента (systemd/runr/shell).
//!
//! `DispatchClient` — единая точка интеграции для replay-цикла. В этой
//! фазе клиент мокируется в тестах; реальные реализации поверх
//! `bosun-systemd-client` и `bosun-runr-client` появятся в Phase D.
//!
//! Решение оставить trait `DispatchClient` в `bosun-core` обусловлено
//! желанием избежать циркулярных зависимостей: defers внутри `core`,
//! клиенты — отдельные крейты, которые тянут `core`. Если бы trait жил
//! в одном из клиентских крейтов, defers пришлось бы тянуть туда же.

use thiserror::Error;

use super::format::DeferEntry;

/// Ошибки выполнения отдельной defer-записи. Разделение на
/// `ClientUnavailable` и `Action` критично для replay: первое — это
/// transient (runr ещё не поднялся, systemd dbus отключён), при котором
/// файл остаётся в журнале без bump'а `attempt_count`; второе — реальный
/// провал, бампающий счётчик и при превышении лимита промоутящий в
/// `.manual_clear`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DispatchError {
    /// Клиент недоступен (transport-уровень). Replay пропустит запись и
    /// попробует на следующем вызове.
    #[error("dispatch client unavailable")]
    ClientUnavailable,
    /// Любая другая ошибка action'а. Текст пробрасывается в `bump_attempt`
    /// для логирования.
    #[error("action failed: {0}")]
    Action(String),
}

/// Контракт для replay-цикла. Реализации мостят `DeferEntry` к реальным
/// клиентам (systemd dbus, runr HTTP, локальный shell для `Command`).
///
/// Метод синхронный — replay не требует параллелизма и крутится
/// последовательно по lex-sorted списку файлов.
pub trait DispatchClient {
    fn dispatch(&self, entry: &DeferEntry) -> Result<(), DispatchError>;
}

/// Удобная обёртка для замыканий. Позволяет в тестах писать
/// `dispatch_fn(|e| Ok(()))` без отдельной структуры.
impl<F> DispatchClient for F
where
    F: Fn(&DeferEntry) -> Result<(), DispatchError>,
{
    fn dispatch(&self, entry: &DeferEntry) -> Result<(), DispatchError> {
        (self)(entry)
    }
}

/// Свободная функция-обёртка: делегирует в `DispatchClient::dispatch`.
/// Вынесена ради удобства плана (`action.rs::dispatch(entry, ctx)`),
/// который оперирует функцией, а не методом.
pub fn dispatch<C: DispatchClient + ?Sized>(
    entry: &DeferEntry,
    client: &C,
) -> Result<(), DispatchError> {
    client.dispatch(entry)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::defers::format::{make_id, DeferAction, CURRENT_SPEC_VERSION};
    use crate::defers::priority::DeferPriority;
    use chrono::Utc;

    fn entry_for_test() -> DeferEntry {
        DeferEntry {
            spec_version: CURRENT_SPEC_VERSION,
            id: make_id("systemd", &DeferAction::Restart, "nginx.service"),
            action: DeferAction::Restart,
            init_system: "systemd".to_string(),
            target: "nginx.service".to_string(),
            validate_cmd: None,
            health_check: None,
            priority: DeferPriority::Restart,
            enqueued_at: Utc::now(),
            enqueued_by: vec![],
            attempt_count: 0,
            max_attempts: 3,
        }
    }

    #[test]
    fn dispatch_closure_propagates_ok() {
        let client = |_entry: &DeferEntry| -> Result<(), DispatchError> { Ok(()) };
        let entry = entry_for_test();
        assert!(dispatch(&entry, &client).is_ok());
    }

    #[test]
    fn dispatch_closure_propagates_client_unavailable() {
        let client = |_entry: &DeferEntry| -> Result<(), DispatchError> {
            Err(DispatchError::ClientUnavailable)
        };
        let entry = entry_for_test();
        match dispatch(&entry, &client) {
            Err(DispatchError::ClientUnavailable) => {}
            other => panic!("expected ClientUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_closure_propagates_action_error() {
        let client = |_entry: &DeferEntry| -> Result<(), DispatchError> {
            Err(DispatchError::Action("unit not found".into()))
        };
        let entry = entry_for_test();
        match dispatch(&entry, &client) {
            Err(DispatchError::Action(msg)) => assert_eq!(msg, "unit not found"),
            other => panic!("expected Action(_), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_client_struct_observes_entries() {
        struct Recorder {
            seen: RefCell<Vec<String>>,
        }
        impl DispatchClient for Recorder {
            fn dispatch(&self, entry: &DeferEntry) -> Result<(), DispatchError> {
                self.seen.borrow_mut().push(entry.id.clone());
                Ok(())
            }
        }

        let recorder = Recorder {
            seen: RefCell::new(vec![]),
        };
        let entry = entry_for_test();
        dispatch(&entry, &recorder).unwrap();
        assert_eq!(recorder.seen.borrow().len(), 1);
        assert_eq!(recorder.seen.borrow()[0], entry.id);
    }
}
