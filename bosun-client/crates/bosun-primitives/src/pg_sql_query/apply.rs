//! Apply-фаза `pg_sql.query`.
//!
//! 1. Десериализовать spec.
//! 2. Выполнить SELECT через backend.
//! 3. Если `store_as_fact` задан — сериализовать rows в `serde_json::Value`
//!    (Array of Object), записать в `ctx.publish_fact(name, value)`.
//! 4. Вернуть `ChangeReport::changed` с числом строк.

use std::sync::Arc;
use std::time::Duration;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use crate::pg_sql_common::{PgSqlBackend, PgSqlError, Row};

use super::spec::PgSqlQuerySpec;

pub fn run(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
    backend: &Arc<dyn PgSqlBackend>,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: PgSqlQuerySpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.query payload: {e}")))?;

    let timeout = Duration::from_secs(u64::from(spec.effective_timeout_sec()));

    tracing::info!(
        resource = %spec.name.as_str(),
        dsn = %crate::pg_sql_common::redact_dsn(&spec.dsn),
        "pg_sql.query: running SELECT",
    );

    let rows = backend
        .query(&spec.dsn, &spec.sql, timeout)
        .map_err(|e| map_backend_error(&spec, e))?;

    if let Some(fact_name) = &spec.store_as_fact {
        let value = rows_to_json(&rows);
        tracing::info!(
            resource = %spec.name.as_str(),
            fact = %fact_name,
            row_count = rows.len(),
            "pg_sql.query: publishing fact",
        );
        ctx.publish_fact(fact_name.clone(), value);
    }

    Ok(ChangeReport::changed(format!(
        "pg_sql.query[{}]: {} row(s)",
        spec.name.as_str(),
        rows.len(),
    )))
}

/// Конверсия `Vec<Row>` в `serde_json::Value::Array<Object>` для публикации
/// в `published_facts`. Каждое значение — строка, как и в `Row`.
fn rows_to_json(rows: &[Row]) -> serde_json::Value {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::with_capacity(row.len());
        for (k, v) in row {
            obj.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        out.push(serde_json::Value::Object(obj));
    }
    serde_json::Value::Array(out)
}

fn map_backend_error(spec: &PgSqlQuerySpec, err: PgSqlError) -> PrimitiveError {
    let dsn_redacted = crate::pg_sql_common::redact_dsn(&spec.dsn);
    match err {
        PgSqlError::InvalidDsn(msg) => PrimitiveError::InvalidPayload(format!(
            "pg_sql.query[{}]: invalid dsn: {msg}",
            spec.name.as_str(),
        )),
        PgSqlError::Connect { dsn: _, reason } => PrimitiveError::Apply {
            reason: format!(
                "pg_sql.query[{}]: connect failed for {dsn_redacted}: {reason}",
                spec.name.as_str(),
            ),
        },
        PgSqlError::Timeout(d) => PrimitiveError::Apply {
            reason: format!(
                "pg_sql.query[{}]: operation timed out after {:?}",
                spec.name.as_str(),
                d,
            ),
        },
        PgSqlError::Sql { sqlstate, message } => PrimitiveError::Apply {
            reason: format!(
                "pg_sql.query[{}]: sqlstate={} message={message}",
                spec.name.as_str(),
                sqlstate.as_deref().unwrap_or("?"),
            ),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtxBuilder, Diff, ResourceId, ResourceKind, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::pg_sql_common::Row;

    use super::*;

    /// Локальный mock-backend, дублирующий MockBackend из pg_sql_exec::plan
    /// для read-only сценариев. Без дублирования query_calls/result доступ
    /// тестам не получить (private-видимость).
    struct MockBackend {
        query_calls: Mutex<Vec<(String, String)>>,
        query_result: Mutex<Result<Vec<Row>, PgSqlError>>,
    }

    impl MockBackend {
        fn ok(rows: Vec<Row>) -> Self {
            Self {
                query_calls: Mutex::new(Vec::new()),
                query_result: Mutex::new(Ok(rows)),
            }
        }
        fn err(err: PgSqlError) -> Self {
            Self {
                query_calls: Mutex::new(Vec::new()),
                query_result: Mutex::new(Err(err)),
            }
        }
    }

    impl PgSqlBackend for MockBackend {
        fn execute(&self, _: &str, _: &str, _: Duration) -> Result<u64, PgSqlError> {
            unreachable!("query primitive не вызывает execute")
        }
        fn query(&self, dsn: &str, sql: &str, _: Duration) -> Result<Vec<Row>, PgSqlError> {
            self.query_calls
                .lock()
                .unwrap()
                .push((dsn.to_string(), sql.to_string()));
            match &*self.query_result.lock().unwrap() {
                Ok(rows) => Ok(rows.clone()),
                Err(PgSqlError::Timeout(d)) => Err(PgSqlError::Timeout(*d)),
                Err(PgSqlError::Connect { dsn, reason }) => Err(PgSqlError::Connect {
                    dsn: dsn.clone(),
                    reason: reason.clone(),
                }),
                Err(PgSqlError::Sql { sqlstate, message }) => Err(PgSqlError::Sql {
                    sqlstate: sqlstate.clone(),
                    message: message.clone(),
                }),
                Err(PgSqlError::InvalidDsn(m)) => Err(PgSqlError::InvalidDsn(m.clone())),
            }
        }
    }

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("pg_sql.query");
        let name = payload["name"].as_str().unwrap_or("test").to_string();
        let id = ResourceId::new(&kind, &name);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload,
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn make_ctx() -> (TempDir, ApplyCtx) {
        let tmp = TempDir::new().unwrap();
        let journal = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtxBuilder::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            journal,
        )
        .build();
        (tmp, ctx)
    }

    fn force_update_diff(r: &Resource) -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: r.payload.clone(),
            description: "run".into(),
        }
    }

    /// query возвращает canned rows, apply конвертирует в Vec<JSON-object>.
    #[test]
    fn apply_query_returns_rows_as_map() {
        let backend: Arc<dyn PgSqlBackend> = Arc::new(MockBackend::ok(vec![
            Row::from_iter([
                ("name".to_string(), "alice".to_string()),
                ("uid".to_string(), "1000".to_string()),
            ]),
            Row::from_iter([
                ("name".to_string(), "bob".to_string()),
                ("uid".to_string(), "1001".to_string()),
            ]),
        ]));
        let r = make_resource(serde_json::json!({
            "name": "list-users",
            "dsn": "postgres://u@h/d",
            "sql": "SELECT name, uid FROM users",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap();
        assert!(report.changed);
        assert!(
            report.message.contains("2 row(s)"),
            "got: {}",
            report.message
        );
    }

    /// store_as_fact: после apply значение лежит в ctx.read_published_fact.
    #[test]
    fn apply_query_stores_as_fact_when_requested() {
        let backend: Arc<dyn PgSqlBackend> = Arc::new(MockBackend::ok(vec![Row::from_iter([(
            "v".to_string(),
            "1".to_string(),
        )])]));
        let r = make_resource(serde_json::json!({
            "name": "list",
            "dsn": "postgres://u@h/d",
            "sql": "SELECT 1 v",
            "store_as_fact": "my.fact",
        }));
        let (_tmp, ctx) = make_ctx();
        let _ = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap();
        let fact = ctx.read_published_fact("my.fact").unwrap();
        let arr = match fact.as_array() {
            Some(a) => a,
            None => panic!("expected array, got {fact:?}"),
        };
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["v"], "1");
    }

    /// Без store_as_fact: фактов в ctx нет.
    #[test]
    fn apply_query_does_not_publish_when_store_as_fact_absent() {
        let backend: Arc<dyn PgSqlBackend> = Arc::new(MockBackend::ok(vec![]));
        let r = make_resource(serde_json::json!({
            "name": "list",
            "dsn": "postgres://u@h/d",
            "sql": "SELECT 1",
        }));
        let (_tmp, ctx) = make_ctx();
        let _ = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap();
        assert!(ctx.read_published_fact("my.fact").is_none());
    }

    /// Connect timeout → Apply с redacted DSN.
    #[test]
    fn apply_connect_timeout_redacts_dsn_in_error() {
        let backend: Arc<dyn PgSqlBackend> = Arc::new(MockBackend::err(PgSqlError::Timeout(
            Duration::from_secs(5),
        )));
        let r = make_resource(serde_json::json!({
            "name": "x",
            "dsn": "postgres://u:topsecret@h/d",
            "sql": "SELECT 1",
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("timed out"), "got: {reason}");
                assert!(!reason.contains("topsecret"), "пароль утёк: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    /// SQL error → Apply.
    #[test]
    fn apply_sql_error_returns_apply() {
        let backend: Arc<dyn PgSqlBackend> = Arc::new(MockBackend::err(PgSqlError::Sql {
            sqlstate: Some("42P01".into()),
            message: "table foo does not exist".into(),
        }));
        let r = make_resource(serde_json::json!({
            "name": "x",
            "dsn": "postgres://u@h/d",
            "sql": "SELECT * FROM foo",
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("42P01"), "got: {reason}");
                assert!(reason.contains("does not exist"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    /// rows_to_json преобразует Row → JSON-object.
    #[test]
    fn rows_to_json_produces_array_of_objects() {
        let rows = vec![
            Row::from_iter([
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
            ]),
            BTreeMap::from_iter([("a".to_string(), "x".to_string())]),
        ];
        let v = rows_to_json(&rows);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["a"], "1");
        assert_eq!(arr[0]["b"], "2");
        assert_eq!(arr[1]["a"], "x");
    }
}
