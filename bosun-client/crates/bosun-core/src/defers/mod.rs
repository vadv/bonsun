//! Журнал отложенных действий с at-least-once семантикой.
//!
//! Журнал — это директория на tmpfs (`/tmp/bosun-defers/` в проде), в
//! которой каждая отложенная операция — отдельный JSON-файл. Семантика
//! повторяет chiit-defers с двумя улучшениями: structured JSON вместо
//! shell-скриптов и обязательный `fsync(dir)` после `rename`/`unlink`.
//!
//! `/tmp` выбран намеренно: после ребута журнал обнуляется, и это
//! ожидаемо — boot уже сам перезапустил все сервисы.
//!
//! Публичная поверхность:
//! - [`DeferAction`], [`DeferEntry`], [`HealthCheck`] — формат записи.
//! - [`DeferPriority`] — приоритет, дублирующийся в префиксе имени файла.
//! - [`Journal`] — хранилище: open/enqueue/list_sorted/remove/bump_attempt.
//! - [`EnqueueResult`] — результат вставки с учётом dedup правил.
//! - [`replay`] + [`ReplayReport`] — at-least-once цикл выполнения.
//! - [`DispatchClient`] + [`DispatchError`] — trait для подключения
//!   конкретных клиентов (systemd dbus, runr HTTP, shell). Реальные
//!   реализации появляются в Phase D.

pub mod action;
pub mod format;
pub mod journal;
pub mod priority;
pub mod replay;

pub use action::{dispatch, DispatchClient, DispatchError};
pub use format::{make_id, DeferAction, DeferEntry, HealthCheck, CURRENT_SPEC_VERSION};
pub use journal::{DeferError, EnqueueResult, Journal};
pub use priority::{sortkey, DeferPriority};
pub use replay::{replay, ReplayReport};
