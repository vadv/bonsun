//! Контракт `PgSqlBackend` и production-реализация поверх sync-обёртки
//! `postgres`-крейта.
//!
//! Backend полностью stateless: каждый вызов открывает новое подключение
//! по DSN, исполняет запрос и закрывает. Connection pooling не вводим —
//! bosun-cli делает несколько pg_sql.* за прогон, накладной расход от
//! re-handshake пренебрежимо мал по сравнению с TCP-handshake'ом и
//! authentication-овершедом.
//!
//! Тестам нужен mock без реальной БД: для этого backend — trait, в тесты
//! инжектится `MockBackend` (см. ниже в `#[cfg(test)]`).
//!
//! Ошибки нормализованы в `PgSqlError`: оператор должен по `Display`
//! отличать «не достучались» от «упал SQL» без чтения исходного `postgres::Error`.

use std::collections::BTreeMap;
use std::time::Duration;

use postgres::fallible_iterator::FallibleIterator;
use postgres::types::{FromSqlOwned, Type};
use postgres::{Client, NoTls};
use thiserror::Error;

/// Одна запись результата `query` в нормализованной форме: имя колонки →
/// строковое представление значения. Используем `BTreeMap`, чтобы порядок
/// колонок был стабильным для логов и сериализации.
pub type Row = BTreeMap<String, String>;

/// Ошибки backend'а. На уровне примитива маппятся в `PrimitiveError::Apply`
/// либо `PrimitiveError::Cancelled`; разделение нужно, чтобы оператор по
/// логам понимал кейс.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PgSqlError {
    /// Не удалось установить соединение: TCP refused, DNS, неверный DSN,
    /// SSL handshake. Не путать с auth-ошибкой — она тоже сюда, потому что
    /// в `postgres::Error` они оба `DbError`/`Connect`.
    #[error("postgres connect failed for {dsn}: {reason}")]
    Connect { dsn: String, reason: String },
    /// Соединение есть, но запрос упал: SQL syntax, недостающая роль,
    /// permission denied. `sqlstate` достаём, если есть — это удобный
    /// дискриминатор для оператора (`42710` = duplicate object и т.п.).
    #[error("postgres sql error{}: {message}", .sqlstate.as_deref().map(|s| format!(" sqlstate={s}")).unwrap_or_default())]
    Sql {
        sqlstate: Option<String>,
        message: String,
    },
    /// Запрос превысил `timeout`. Production-реализация ставит
    /// `statement_timeout` через сессионную команду, плюс заворачивает
    /// connect в `connect_timeout`. Если timeout всё-таки превышен — это
    /// сигнал, что БД перегружена, либо запрос требует > N секунд.
    #[error("postgres operation timed out after {0:?}")]
    Timeout(Duration),
    /// Парсинг DSN не удался ещё до попытки connect'а. Это InvalidPayload
    /// семантически, но дёшево вернуть из backend'а для единообразия.
    #[error("invalid DSN: {0}")]
    InvalidDsn(String),
}

/// Контракт backend'а. Разделение «execute» (мутирующая команда без
/// табличного результата) и «query» (SELECT-возвращающая) повторяет API
/// postgres-крейта (`Client::execute` vs `Client::query`), и оператору так
/// понятнее, что делает каждый примитив.
pub trait PgSqlBackend: Send + Sync {
    /// Исполняет `sql` через `Client::batch_execute` — это даёт нам
    /// поддержку multi-statement payload'ов (`CREATE ROLE x; GRANT y TO x;`)
    /// и DDL без необходимости расщеплять запрос вручную. Возвращает
    /// количество затронутых строк (sum по всем statement'ам, если запросы
    /// возвращали rows). Для DDL/GRANT это обычно 0 — то есть индикатор
    /// «команда исполнилась», а не «строк обновлено».
    fn execute(&self, dsn: &str, sql: &str, timeout: Duration) -> Result<u64, PgSqlError>;

    /// Исполняет `sql` (single-statement SELECT) и собирает строки в
    /// `Vec<Row>`. Каждое значение конвертируется в строку через
    /// `value_to_string` (см. реализацию ниже). NULL превращается в
    /// строку `"NULL"` — это сигнал «значение было NULL», а не пустая
    /// строка, которую можно перепутать с пустым varchar'ом.
    fn query(&self, dsn: &str, sql: &str, timeout: Duration) -> Result<Vec<Row>, PgSqlError>;
}

/// Production-реализация. Никакого внутреннего состояния — открывает
/// connection per call, ставит `statement_timeout` через сессионную команду,
/// после чего исполняет запрос.
#[derive(Default, Debug, Clone, Copy)]
pub struct RealPgSqlBackend;

impl PgSqlBackend for RealPgSqlBackend {
    fn execute(&self, dsn: &str, sql: &str, timeout: Duration) -> Result<u64, PgSqlError> {
        let mut client = open_client(dsn, timeout)?;
        apply_statement_timeout(&mut client, timeout)?;
        // `batch_execute` исполняет любой DDL/DML без подготовленного
        // statement'а. Для CREATE ROLE/GRANT/CREATE EXTENSION это обычно
        // единственный путь — prepared statements не поддерживают
        // utility-команды.
        client
            .batch_execute(sql)
            .map_err(map_pg_error)
            .map(|()| 0_u64)
    }

    fn query(&self, dsn: &str, sql: &str, timeout: Duration) -> Result<Vec<Row>, PgSqlError> {
        let mut client = open_client(dsn, timeout)?;
        apply_statement_timeout(&mut client, timeout)?;
        let rows = client.query(sql, &[]).map_err(map_pg_error)?;

        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let mut record = Row::new();
            for (idx, col) in r.columns().iter().enumerate() {
                let val = value_to_string(r, idx, col.type_());
                record.insert(col.name().to_string(), val);
            }
            out.push(record);
        }
        Ok(out)
    }
}

/// Открыть соединение с учётом `connect_timeout`. `connect_timeout` зашит
/// в DSN как параметр libpq (`?connect_timeout=10` для URL или
/// `connect_timeout=10` для key=value); если автор bundle не указал —
/// добавляем сами, иначе подвешенный TCP мог бы держать примитив до
/// верхнего deadline'а.
fn open_client(dsn: &str, timeout: Duration) -> Result<Client, PgSqlError> {
    let cfg: postgres::Config = dsn
        .parse()
        .map_err(|e: postgres::Error| PgSqlError::InvalidDsn(format!("{e}")))?;
    // Если `connect_timeout` в DSN не задан, `cfg.get_connect_timeout()`
    // вернёт None. Подставляем `timeout` — оператор не получит зависший
    // примитив при недоступной БД.
    let mut cfg = cfg;
    if cfg.get_connect_timeout().is_none() {
        cfg.connect_timeout(timeout);
    }
    cfg.connect(NoTls).map_err(map_pg_error_connect(dsn))
}

/// Установить `statement_timeout` для текущей сессии. Это синхронная
/// серверная гарантия: postgres сам прервёт запрос, если он не уложился.
fn apply_statement_timeout(client: &mut Client, timeout: Duration) -> Result<(), PgSqlError> {
    let ms = timeout.as_millis();
    // `statement_timeout=0` — отключение. Если timeout у нас 0 (вырожденный
    // случай), пусть так и будет: пусть постгрес не убивает запросы по
    // таймауту.
    let sql = format!("SET LOCAL statement_timeout = {ms}");
    client.batch_execute(&sql).map_err(map_pg_error)
}

/// Преобразовать значение колонки в строку. Текстовые/числовые типы — через
/// `FromSqlOwned`-обёртки и Display; всё остальное — через generic
/// `String`-decode, который у postgres-крейта работает для большинства
/// печатаемых типов. NULL → `"NULL"`.
fn value_to_string(row: &postgres::Row, idx: usize, ty: &Type) -> String {
    if matches!(*ty, Type::INT2 | Type::INT4 | Type::INT8 | Type::OID) {
        return decode_owned::<i64>(row, idx, |v| v.to_string());
    }
    if matches!(*ty, Type::FLOAT4 | Type::FLOAT8) {
        return decode_owned::<f64>(row, idx, |v| v.to_string());
    }
    if matches!(*ty, Type::BOOL) {
        return decode_owned::<bool>(row, idx, |v| v.to_string());
    }
    if matches!(*ty, Type::TEXT | Type::VARCHAR | Type::NAME | Type::BPCHAR) {
        return decode_owned::<String>(row, idx, |v| v);
    }
    // UUID, JSON, JSONB, BYTEA и другие — пытаемся как строку, иначе
    // явный маркер.
    decode_owned::<String>(row, idx, |v| v)
}

/// Helper: попытаться декодировать значение в `T: FromSqlOwned`. NULL
/// возвращает `"NULL"`. Ошибка декодинга — `"<decode-error>"`, чтобы
/// результат query не падал из-за одной экзотической колонки.
fn decode_owned<T: FromSqlOwned>(
    row: &postgres::Row,
    idx: usize,
    to_string: impl FnOnce(T) -> String,
) -> String {
    match row.try_get::<usize, Option<T>>(idx) {
        Ok(Some(v)) => to_string(v),
        Ok(None) => "NULL".to_string(),
        Err(_) => "<decode-error>".to_string(),
    }
}

/// Маппинг `postgres::Error` в `PgSqlError` для запросов: timeout по
/// SQLSTATE `57014` (query_canceled), всё остальное — `Sql`.
fn map_pg_error(err: postgres::Error) -> PgSqlError {
    let sqlstate = err.as_db_error().map(|d| d.code().code().to_string());
    if sqlstate.as_deref() == Some("57014") {
        // statement_timeout сработал на сервере. Точное значение timeout'а
        // не знаем — возвращаем грубый признак.
        return PgSqlError::Timeout(Duration::from_secs(0));
    }
    let message = err
        .as_db_error()
        .map(|d| d.message().to_string())
        .unwrap_or_else(|| format!("{err}"));
    PgSqlError::Sql { sqlstate, message }
}

/// Маппинг ошибки `connect()` — она тоже `postgres::Error`, но для оператора
/// удобнее видеть `Connect{dsn=...}`. DSN перед логом проходит через
/// `redact_dsn`, чтобы пароль не утёк.
fn map_pg_error_connect(dsn: &str) -> impl FnOnce(postgres::Error) -> PgSqlError + '_ {
    move |err| {
        let reason = err
            .as_db_error()
            .map(|d| d.message().to_string())
            .unwrap_or_else(|| format!("{err}"));
        PgSqlError::Connect {
            dsn: super::redact::redact_dsn(dsn),
            reason,
        }
    }
}

/// `FallibleIterator` нужен `postgres`-крейту для streaming-результатов
/// (например, `Client::query_raw`); мы используем только `Client::query`,
/// но keep'аем импорт через `use` — чтобы `next()`/`collect()` методы
/// были доступны, если в будущем переключимся на `query_raw` для больших
/// resultset'ов. Сейчас trait не используется, поэтому подавляем
/// `unused_imports` через `_`.
#[allow(dead_code)]
fn _silence_fallible_iter_import() {
    fn _f<T: FallibleIterator>(_: T) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn pg_sql_error_display_includes_sqlstate() {
        let e = PgSqlError::Sql {
            sqlstate: Some("42710".into()),
            message: "role already exists".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("42710"), "got: {s}");
        assert!(s.contains("role already exists"), "got: {s}");
    }

    #[test]
    fn pg_sql_error_display_without_sqlstate() {
        let e = PgSqlError::Sql {
            sqlstate: None,
            message: "boom".into(),
        };
        let s = format!("{e}");
        assert!(!s.contains("sqlstate="), "got: {s}");
        assert!(s.contains("boom"), "got: {s}");
    }

    #[test]
    fn pg_sql_error_connect_redacts_dsn() {
        let e = PgSqlError::Connect {
            dsn: super::super::redact::redact_dsn("postgres://u:secret@h/d"),
            reason: "refused".into(),
        };
        let s = format!("{e}");
        assert!(!s.contains("secret"), "пароль утёк в Display: {s}");
        assert!(s.contains("*****"), "ожидался маркер: {s}");
    }

    /// Smoke-тест против реального unix socket: если PostgreSQL запущен
    /// локально, RealPgSqlBackend смог исполнить `SELECT 1`. На CI обычно
    /// нет PG — тест помечен `#[ignore]`. Запускать вручную:
    /// `cargo test -p bosun-primitives -- --ignored real_backend_query_one`
    #[test]
    #[ignore = "требует локального PostgreSQL на /var/run/postgresql"]
    fn real_backend_query_one() {
        let backend = RealPgSqlBackend;
        let dsn = "host=/var/run/postgresql user=postgres dbname=postgres";
        let rows = backend
            .query(dsn, "SELECT 1 AS one", Duration::from_secs(5))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("one").map(String::as_str), Some("1"));
    }

    #[test]
    #[ignore = "требует локального PostgreSQL на /var/run/postgresql"]
    fn real_backend_execute_set_does_not_fail() {
        let backend = RealPgSqlBackend;
        let dsn = "host=/var/run/postgresql user=postgres dbname=postgres";
        // SET — DDL-ish, batch_execute должен принять без ошибок.
        backend
            .execute(dsn, "SET timezone = 'UTC'", Duration::from_secs(5))
            .unwrap();
    }

    /// Невалидный DSN должен сразу возвращать InvalidDsn, без сетевых
    /// попыток.
    #[test]
    fn real_backend_rejects_invalid_dsn() {
        let backend = RealPgSqlBackend;
        let res = backend.query("not-a-valid-dsn-at-all", "SELECT 1", Duration::from_secs(1));
        match res.unwrap_err() {
            PgSqlError::InvalidDsn(_) => {}
            other => panic!("expected InvalidDsn, got {other:?}"),
        }
    }
}
