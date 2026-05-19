//! Синхронный HTTP-клиент для runr-демона (`127.0.0.1:8010`).
//!
//! Phase B плана `2026-05-19-bosun-runr-systemd-defers-plan.md`. См. также
//! секцию «Новый крейт: bosun-runr-client» в
//! `2026-05-19-bosun-runr-systemd-defers-design.md`.
//!
//! Публичная поверхность:
//! - [`Client`] — sync-клиент над `ureq::Agent`. Все методы возвращают
//!   `Result<_, RunrError>`.
//! - [`RunrError`] — `#[non_exhaustive]` enum с категоризацией ошибок:
//!   `Unavailable`, `ApiError`, `BadResponse`, `NotFound`,
//!   `RestartNotObserved`, `Io`.
//! - Типы ответов: [`ServiceStatus`], [`TimerStatus`], [`UnitListItem`],
//!   [`ActionAck`], [`DaemonInfo`], [`CgroupMetrics`], [`UnitKind`].
//! - [`verify::verify_restart`] — polling-верификация рестарта по диффу
//!   `restarts` и состояния `state == "Running"`.
//! - [`verify::verify_start`] — polling-верификация старта (только
//!   `state == "Running"`, без опоры на инкремент `restarts`).
//!
//! Никаких `reqwest`/`hyper`/`tokio` в production-зависимостях: runr доступен
//! только через `localhost`, fork+exec шелла не нужен, асинхронность не
//! оправдана.

pub mod client;
pub mod error;
pub mod types;
pub mod verify;

pub use client::Client;
pub use error::RunrError;
pub use types::{
    ActionAck, CgroupMetrics, DaemonInfo, ServiceStatus, TimerStatus, UnitKind, UnitListItem,
};
pub use verify::{verify_restart, verify_start};
