//! Apply-фаза `pg_sql.exec`.
//!
//! Поток:
//! 1. Re-десериализовать spec.
//! 2. Если `if_not_exists_check` задан — re-выполнить check (re-read перед
//!    write): за время между plan и apply состояние БД могло измениться
//!    (другая сессия успела создать роль). Если check возвращает > 0 строк
//!    → `ChangeReport::no_change()`.
//! 3. Иначе — выполнить `sql` через backend.execute. На успех — `Changed`,
//!    на ошибку — маппинг `PgSqlError` → `PrimitiveError`.
//!
//! Этот re-check — ключевая гарантия read-before-write. Без него:
//!
//! ```text
//! Session A: plan → check OK (отсутствует) → execute CREATE ROLE …
//! Session B: одновременно сделал CREATE ROLE → SQLSTATE 42710 duplicate_object
//! Session A: получит ошибку, потеряет идемпотентность.
//! ```

use std::sync::Arc;
use std::time::Duration;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use crate::pg_sql_common::{PgSqlBackend, PgSqlError};

use super::spec::PgSqlExecSpec;

pub fn run(
    resource: &Resource,
    diff: &Diff,
    _ctx: &ApplyCtx,
    backend: &Arc<dyn PgSqlBackend>,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: PgSqlExecSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.exec payload: {e}")))?;

    let timeout = Duration::from_secs(u64::from(spec.effective_timeout_sec()));

    // Re-check: если check задан, повторяем его в apply, чтобы не выполнять
    // exec на уже существующем объекте. Это критически важно при concurrent
    // bosun-run'ах (две ноды одновременно настраивают одного пользователя
    // на patroni-кластере — встречаются на standby и проигрывают разные
    // мини-сценарии).
    if let Some(check_sql) = &spec.if_not_exists_check {
        let rows = backend
            .query(&spec.dsn, check_sql, timeout)
            .map_err(|e| map_backend_error(&spec, "re-check", e))?;
        if !rows.is_empty() {
            tracing::info!(
                resource = %spec.name.as_str(),
                "pg_sql.exec: re-check positive, skipping exec",
            );
            return Ok(ChangeReport::no_change());
        }
    }

    tracing::info!(
        resource = %spec.name.as_str(),
        dsn = %crate::pg_sql_common::redact_dsn(&spec.dsn),
        "pg_sql.exec: running statement",
    );

    backend
        .execute(&spec.dsn, &spec.sql, timeout)
        .map(|_affected| {
            ChangeReport::changed(format!("pg_sql.exec[{}]: executed", spec.name.as_str()))
        })
        .map_err(|e| map_backend_error(&spec, "execute", e))
}

/// Маппинг ошибок backend → PrimitiveError. Конкретные ошибки postgres-сервера
/// (`Sql`) — это семантический отказ (битый SQL, нет привилегий), они не
/// deferrable. Connect/Timeout — Apply, но не deferrable: bosun-cli не умеет
/// откладывать `pg_sql.*` через журнал defers (там только команды и
/// runr/systemd-операции). Оператор должен либо поднять БД, либо чинить
/// конфигурацию.
fn map_backend_error(spec: &PgSqlExecSpec, stage: &str, err: PgSqlError) -> PrimitiveError {
    let dsn_redacted = crate::pg_sql_common::redact_dsn(&spec.dsn);
    match err {
        PgSqlError::InvalidDsn(msg) => PrimitiveError::InvalidPayload(format!(
            "pg_sql.exec[{}] ({stage}): invalid dsn: {msg}",
            spec.name.as_str(),
        )),
        PgSqlError::Connect { dsn: _, reason } => PrimitiveError::Apply {
            reason: format!(
                "pg_sql.exec[{}] ({stage}): connect failed for {dsn_redacted}: {reason}",
                spec.name.as_str(),
            ),
        },
        PgSqlError::Timeout(d) => PrimitiveError::Apply {
            reason: format!(
                "pg_sql.exec[{}] ({stage}): operation timed out after {:?}",
                spec.name.as_str(),
                d,
            ),
        },
        PgSqlError::Sql { sqlstate, message } => PrimitiveError::Apply {
            reason: format!(
                "pg_sql.exec[{}] ({stage}): sqlstate={} message={message}",
                spec.name.as_str(),
                sqlstate.as_deref().unwrap_or("?"),
            ),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtxBuilder, Diff, ResourceId, ResourceKind, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::pg_sql_common::testutil::MockBackend;
    use crate::pg_sql_common::Row;

    use super::*;

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("pg_sql.exec");
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

    /// При diff=NoChange backend не должен быть вызван вовсе.
    #[test]
    fn apply_no_change_diff_short_circuits() {
        let backend_inner = Arc::new(MockBackend::new());
        let backend: Arc<dyn PgSqlBackend> = backend_inner.clone();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "dsn": "postgres://u@h/d",
            "sql": "SELECT 1",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &Diff::NoChange, &ctx, &backend).unwrap();
        assert!(!report.changed);
        assert!(backend_inner.execute_calls.lock().unwrap().is_empty());
        assert!(backend_inner.query_calls.lock().unwrap().is_empty());
    }

    /// Apply при отрицательном re-check (rows empty) вызывает execute.
    #[test]
    fn apply_runs_exec_when_check_negative() {
        let backend_inner = Arc::new(MockBackend::new().with_query_rows(vec![]));
        let backend: Arc<dyn PgSqlBackend> = backend_inner.clone();
        let r = make_resource(serde_json::json!({
            "name": "create-monitor",
            "dsn": "postgres://u@h/d",
            "sql": "CREATE ROLE monitor",
            "if_not_exists_check": "SELECT 1 FROM pg_roles WHERE rolname='monitor'",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap();
        assert!(report.changed, "expected changed, got {report:?}");
        // Re-check вызван 1 раз, execute — 1 раз.
        assert_eq!(backend_inner.query_calls.lock().unwrap().len(), 1);
        assert_eq!(backend_inner.execute_calls.lock().unwrap().len(), 1);
        let (_, sql) = &backend_inner.execute_calls.lock().unwrap()[0];
        assert_eq!(sql, "CREATE ROLE monitor");
    }

    /// Apply при положительном re-check (rows non-empty) НЕ вызывает execute.
    /// Это критический read-before-write тест.
    #[test]
    fn apply_skips_exec_when_check_positive() {
        let backend_inner = Arc::new(MockBackend::new().with_query_rows(vec![Row::from_iter([(
            "?column?".to_string(),
            "1".to_string(),
        )])]));
        let backend: Arc<dyn PgSqlBackend> = backend_inner.clone();
        let r = make_resource(serde_json::json!({
            "name": "create-monitor",
            "dsn": "postgres://u@h/d",
            "sql": "CREATE ROLE monitor",
            "if_not_exists_check": "SELECT 1 FROM pg_roles WHERE rolname='monitor'",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap();
        assert!(!report.changed, "expected no_change, got {report:?}");
        assert!(!report.deferred);
        // Re-check вызван 1 раз, execute — НИ РАЗУ.
        assert_eq!(backend_inner.query_calls.lock().unwrap().len(), 1);
        assert!(
            backend_inner.execute_calls.lock().unwrap().is_empty(),
            "execute не должен был быть вызван, calls={:?}",
            backend_inner.execute_calls.lock().unwrap(),
        );
    }

    /// Apply без check выполняет execute напрямую (без query'я).
    #[test]
    fn apply_without_check_runs_exec_directly() {
        let backend_inner = Arc::new(MockBackend::new());
        let backend: Arc<dyn PgSqlBackend> = backend_inner.clone();
        let r = make_resource(serde_json::json!({
            "name": "set-tz",
            "dsn": "postgres://u@h/d",
            "sql": "SET timezone TO 'UTC'",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap();
        assert!(report.changed);
        assert!(backend_inner.query_calls.lock().unwrap().is_empty());
        assert_eq!(backend_inner.execute_calls.lock().unwrap().len(), 1);
    }

    /// Connect-ошибка в execute → PrimitiveError::Apply.
    #[test]
    fn apply_connect_error_returns_apply_with_redacted_dsn() {
        let backend_inner = Arc::new(MockBackend::new());
        *backend_inner.execute_result.lock().unwrap() = Err(PgSqlError::Connect {
            dsn: "*****".into(),
            reason: "connection refused".into(),
        });
        let backend: Arc<dyn PgSqlBackend> = backend_inner.clone();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "dsn": "postgres://postgres:topsecret@h/d",
            "sql": "CREATE ROLE x",
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("connect failed"), "got: {reason}");
                assert!(reason.contains("connection refused"), "got: {reason}");
                assert!(
                    !reason.contains("topsecret"),
                    "пароль утёк в reason: {reason}"
                );
                assert!(reason.contains("*****"), "ожидался маркер: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    /// SQL-ошибка → Apply с sqlstate.
    #[test]
    fn apply_sql_error_returns_apply_with_sqlstate() {
        let backend_inner = Arc::new(MockBackend::new());
        *backend_inner.execute_result.lock().unwrap() = Err(PgSqlError::Sql {
            sqlstate: Some("42501".into()),
            message: "permission denied for table foo".into(),
        });
        let backend: Arc<dyn PgSqlBackend> = backend_inner.clone();
        let r = make_resource(serde_json::json!({
            "name": "g",
            "dsn": "postgres://u@h/d",
            "sql": "GRANT SELECT ON foo TO bar",
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("42501"), "got: {reason}");
                assert!(reason.contains("permission denied"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    /// Невалидный DSN → InvalidPayload (а не Apply).
    #[test]
    fn apply_invalid_dsn_in_execute_returns_invalid_payload() {
        let backend_inner = Arc::new(MockBackend::new());
        *backend_inner.execute_result.lock().unwrap() =
            Err(PgSqlError::InvalidDsn("bad scheme".into()));
        let backend: Arc<dyn PgSqlBackend> = backend_inner.clone();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "dsn": "bad",
            "sql": "SELECT 1",
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &backend).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }
}
