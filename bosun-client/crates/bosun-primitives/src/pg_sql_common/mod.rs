//! Общая инфраструктура для примитивов `pg_sql.exec` и `pg_sql.query`:
//! DI-trait `PgSqlBackend`, production-реализация `RealPgSqlBackend` поверх
//! sync-обёртки `postgres`-крейта, единая ошибка `PgSqlError` и утилита
//! `redact_dsn` для безопасного логирования DSN с паролем.
//!
//! Backend разделяется между обоими примитивами через `Arc<dyn PgSqlBackend>`
//! и инжектится в primitive struct (по образцу `users.user` + `RealUsersBackend`).

mod backend;
mod redact;

#[cfg(test)]
pub(crate) mod testutil;

pub use backend::{PgSqlBackend, PgSqlError, RealPgSqlBackend, Row};
pub use redact::redact_dsn;
