//! DI-trait `PgFactQuery` для PG-discovery-фактов и его production-реализация
//! поверх `postgres`-крейта.
//!
//! Разделение DI:
//! - Trait описывает три discovery-запроса: `is_master`, `users`, `extensions`.
//! - `RealPgFactQuery` для production: открывает соединение per call, DSN
//!   парсится `postgres::Config`, `connect_timeout` инжектится принудительно.
//! - В тестах подменяется на `MockPgFactQuery` (см. `#[cfg(test)]`).
//!
//! Stateless backend: каждый вызов даёт новое подключение. Discovery-факты
//! собираются один раз на старте; пуллинг был бы избыточным.

use std::time::Duration;

use postgres::{Client, NoTls};

/// Ошибка одного запроса. Имя домена нарочно широкое — discovery
/// не разделяет «не подключились» от «упал SQL»: в обоих случаях
/// `FactValue::Unknown` с конкретным reason.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PgFactQueryError {
    /// Не удалось установить соединение по DSN. Сюда же относим
    /// неверный DSN — для facts-уровня дополнительное разделение не нужно.
    #[error("connect failed: {0}")]
    Connect(String),
    /// Запрос отправлен, но БД ответила ошибкой (permission denied,
    /// missing pg_authid доступ, прочее).
    #[error("query failed: {0}")]
    Sql(String),
}

/// Контракт запросов для discovery-фактов.
///
/// Каждый метод возвращает либо парсированный результат, либо ошибку.
/// Уровнем выше (`Fact::collect`) ошибки конвертируются в
/// `FactValue::Unknown` с понятным reason — чтобы Strict-режим в
/// Starlark корректно сигналил автору bundle.
pub trait PgFactQuery: Send + Sync {
    /// Возвращает `true`, если узел сейчас master (не в recovery).
    /// SQL: `SELECT pg_is_in_recovery()`. Master = NOT in_recovery.
    fn is_master(&self) -> Result<bool, PgFactQueryError>;

    /// Возвращает список ролей с паролями: `(rolname, rolpassword)`.
    /// `rolpassword` приходит в виде хэша (SCRAM/MD5) либо `None`, если
    /// пароль не задан. SQL смотрит в `pg_authid` — таблица требует
    /// superuser-доступа, на replica может вернуть permission denied.
    fn users(&self) -> Result<Vec<(String, Option<String>)>, PgFactQueryError>;

    /// Возвращает список установленных расширений: `(extname, extversion)`.
    /// SQL: `SELECT extname, extversion FROM pg_extension`.
    fn extensions(&self) -> Result<Vec<(String, String)>, PgFactQueryError>;
}

/// Production-реализация. Хранит DSN и connect-timeout; никакого
/// внутреннего состояния. Поверх sync-обёртки `postgres::Client`.
pub struct RealPgFactQuery {
    dsn: String,
    timeout: Duration,
}

impl RealPgFactQuery {
    pub fn new(dsn: String, timeout: Duration) -> Self {
        Self { dsn, timeout }
    }

    /// Открыть соединение с учётом connect_timeout. Если в DSN
    /// `connect_timeout` не задан — подставляем свой, чтобы зависший
    /// TCP не держал facts collector до общего deadline'а.
    fn open(&self) -> Result<Client, PgFactQueryError> {
        let mut cfg: postgres::Config = self
            .dsn
            .parse()
            .map_err(|e: postgres::Error| PgFactQueryError::Connect(format!("invalid DSN: {e}")))?;
        if cfg.get_connect_timeout().is_none() {
            cfg.connect_timeout(self.timeout);
        }
        cfg.connect(NoTls)
            .map_err(|e| PgFactQueryError::Connect(format!("{e}")))
    }
}

impl PgFactQuery for RealPgFactQuery {
    fn is_master(&self) -> Result<bool, PgFactQueryError> {
        let mut client = self.open()?;
        let row = client
            .query_one("SELECT pg_is_in_recovery()", &[])
            .map_err(|e| PgFactQueryError::Sql(format!("pg_is_in_recovery: {e}")))?;
        let in_recovery: bool = row.try_get(0).map_err(|e| {
            PgFactQueryError::Sql(format!("pg_is_in_recovery returned non-bool: {e}"))
        })?;
        Ok(!in_recovery)
    }

    fn users(&self) -> Result<Vec<(String, Option<String>)>, PgFactQueryError> {
        let mut client = self.open()?;
        // pg_authid: rolname + rolpassword. Берём только активные роли
        // с правом login — иначе вернём служебные внутренние роли,
        // которые apply-стороне не нужны.
        let sql = "SELECT rolname, rolpassword FROM pg_authid \
                   WHERE rolcanlogin = true \
                   AND (rolvaliduntil IS NULL OR rolvaliduntil > current_timestamp)";
        let rows = client
            .query(sql, &[])
            .map_err(|e| PgFactQueryError::Sql(format!("pg_authid query: {e}")))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let name: String = r
                .try_get(0)
                .map_err(|e| PgFactQueryError::Sql(format!("rolname decode: {e}")))?;
            let pass: Option<String> = r
                .try_get(1)
                .map_err(|e| PgFactQueryError::Sql(format!("rolpassword decode: {e}")))?;
            out.push((name, pass));
        }
        Ok(out)
    }

    fn extensions(&self) -> Result<Vec<(String, String)>, PgFactQueryError> {
        let mut client = self.open()?;
        let sql = "SELECT extname, extversion FROM pg_extension";
        let rows = client
            .query(sql, &[])
            .map_err(|e| PgFactQueryError::Sql(format!("pg_extension query: {e}")))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let name: String = r
                .try_get(0)
                .map_err(|e| PgFactQueryError::Sql(format!("extname decode: {e}")))?;
            let ver: String = r
                .try_get(1)
                .map_err(|e| PgFactQueryError::Sql(format!("extversion decode: {e}")))?;
            out.push((name, ver));
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
pub(crate) mod mock {
    //! Mock-реализация PgFactQuery для тестов модулей-факторов.
    use std::sync::Mutex;

    use super::{PgFactQuery, PgFactQueryError};

    /// Тип-алиас для результата `users()`: пара (имя роли, опциональный
    /// хэш пароля). Clippy ругается на эту вложенность, когда она встроена
    /// прямо в поле `Mutex<...>`, и отдельный тип читается понятнее.
    type UsersResult = Result<Vec<(String, Option<String>)>, PgFactQueryError>;
    /// Тип-алиас для результата `extensions()`: пара (имя расширения, версия).
    type ExtensionsResult = Result<Vec<(String, String)>, PgFactQueryError>;

    pub(crate) struct MockPgFactQuery {
        pub(crate) is_master_result: Mutex<Result<bool, PgFactQueryError>>,
        pub(crate) users_result: Mutex<UsersResult>,
        pub(crate) extensions_result: Mutex<ExtensionsResult>,
    }

    impl MockPgFactQuery {
        pub(crate) fn new() -> Self {
            Self {
                is_master_result: Mutex::new(Ok(true)),
                users_result: Mutex::new(Ok(Vec::new())),
                extensions_result: Mutex::new(Ok(Vec::new())),
            }
        }

        pub(crate) fn with_is_master(self, v: bool) -> Self {
            *self.is_master_result.lock().unwrap() = Ok(v);
            self
        }

        pub(crate) fn with_is_master_err(self, reason: &str) -> Self {
            *self.is_master_result.lock().unwrap() =
                Err(PgFactQueryError::Connect(reason.to_string()));
            self
        }

        pub(crate) fn with_users(self, users: Vec<(String, Option<String>)>) -> Self {
            *self.users_result.lock().unwrap() = Ok(users);
            self
        }

        pub(crate) fn with_users_err(self, reason: &str) -> Self {
            *self.users_result.lock().unwrap() = Err(PgFactQueryError::Sql(reason.to_string()));
            self
        }

        pub(crate) fn with_extensions(self, exts: Vec<(String, String)>) -> Self {
            *self.extensions_result.lock().unwrap() = Ok(exts);
            self
        }

        pub(crate) fn with_extensions_err(self, reason: &str) -> Self {
            *self.extensions_result.lock().unwrap() =
                Err(PgFactQueryError::Sql(reason.to_string()));
            self
        }
    }

    fn clone_err(e: &PgFactQueryError) -> PgFactQueryError {
        match e {
            PgFactQueryError::Connect(s) => PgFactQueryError::Connect(s.clone()),
            PgFactQueryError::Sql(s) => PgFactQueryError::Sql(s.clone()),
        }
    }

    impl PgFactQuery for MockPgFactQuery {
        fn is_master(&self) -> Result<bool, PgFactQueryError> {
            match &*self.is_master_result.lock().unwrap() {
                Ok(v) => Ok(*v),
                Err(e) => Err(clone_err(e)),
            }
        }

        fn users(&self) -> Result<Vec<(String, Option<String>)>, PgFactQueryError> {
            match &*self.users_result.lock().unwrap() {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(clone_err(e)),
            }
        }

        fn extensions(&self) -> Result<Vec<(String, String)>, PgFactQueryError> {
            match &*self.extensions_result.lock().unwrap() {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(clone_err(e)),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn real_query_rejects_invalid_dsn() {
        let q = RealPgFactQuery::new("not-a-valid-dsn".to_string(), Duration::from_secs(1));
        match q.is_master() {
            Err(PgFactQueryError::Connect(msg)) => assert!(msg.contains("invalid DSN"), "{msg}"),
            other => panic!("expected Connect(invalid DSN), got {other:?}"),
        }
    }

    /// Smoke-тест против реального unix socket. На CI обычно нет PG —
    /// помечен `#[ignore]`, запускается вручную:
    /// `cargo test -p bosun-facts -- --ignored real_pg_query`
    #[test]
    #[ignore = "требует локального PostgreSQL на /var/run/postgresql"]
    fn real_pg_query_is_master_returns_bool() {
        let q = RealPgFactQuery::new(
            "host=/var/run/postgresql user=postgres dbname=postgres".to_string(),
            Duration::from_secs(5),
        );
        // Один из двух вариантов — true (master) или false (replica) —
        // valid; главное, что Ok.
        let _v = q.is_master().unwrap();
    }
}
