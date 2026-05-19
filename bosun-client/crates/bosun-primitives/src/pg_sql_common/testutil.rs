//! Shared test helpers для `pg_sql.exec` / `pg_sql.query`.
//!
//! `MockBackend` — recorder без реальной БД. Хранит canned результаты
//! отдельно для `execute` и `query`, плюс ведёт счётчики вызовов, чтобы
//! тесты могли проверять read-before-write контракт.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Mutex;
use std::time::Duration;

use super::backend::{PgSqlBackend, PgSqlError, Row};

pub(crate) struct MockBackend {
    pub(crate) execute_calls: Mutex<Vec<(String, String)>>,
    pub(crate) query_calls: Mutex<Vec<(String, String)>>,
    pub(crate) query_result: Mutex<Result<Vec<Row>, PgSqlError>>,
    pub(crate) execute_result: Mutex<Result<u64, PgSqlError>>,
}

impl MockBackend {
    pub(crate) fn new() -> Self {
        Self {
            execute_calls: Mutex::new(Vec::new()),
            query_calls: Mutex::new(Vec::new()),
            query_result: Mutex::new(Ok(Vec::new())),
            execute_result: Mutex::new(Ok(0)),
        }
    }

    pub(crate) fn with_query_rows(self, rows: Vec<Row>) -> Self {
        *self.query_result.lock().unwrap() = Ok(rows);
        self
    }

    pub(crate) fn with_query_err(self, err: PgSqlError) -> Self {
        *self.query_result.lock().unwrap() = Err(err);
        self
    }
}

/// PgSqlError не реализует Clone из-за `std::io::Error`-подобных полей;
/// собираем «копию» вручную через перечисление вариантов.
fn clone_err(err: &PgSqlError) -> PgSqlError {
    match err {
        PgSqlError::Timeout(d) => PgSqlError::Timeout(*d),
        PgSqlError::Connect { dsn, reason } => PgSqlError::Connect {
            dsn: dsn.clone(),
            reason: reason.clone(),
        },
        PgSqlError::Sql { sqlstate, message } => PgSqlError::Sql {
            sqlstate: sqlstate.clone(),
            message: message.clone(),
        },
        PgSqlError::InvalidDsn(m) => PgSqlError::InvalidDsn(m.clone()),
    }
}

impl PgSqlBackend for MockBackend {
    fn execute(&self, dsn: &str, sql: &str, _timeout: Duration) -> Result<u64, PgSqlError> {
        self.execute_calls
            .lock()
            .unwrap()
            .push((dsn.to_string(), sql.to_string()));
        match &*self.execute_result.lock().unwrap() {
            Ok(n) => Ok(*n),
            Err(e) => Err(clone_err(e)),
        }
    }

    fn query(&self, dsn: &str, sql: &str, _timeout: Duration) -> Result<Vec<Row>, PgSqlError> {
        self.query_calls
            .lock()
            .unwrap()
            .push((dsn.to_string(), sql.to_string()));
        match &*self.query_result.lock().unwrap() {
            Ok(rows) => Ok(rows.clone()),
            Err(e) => Err(clone_err(e)),
        }
    }
}
