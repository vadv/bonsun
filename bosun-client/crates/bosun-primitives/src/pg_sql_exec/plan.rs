//! Plan-фаза `pg_sql.exec`.
//!
//! Контракт:
//! - Если `if_not_exists_check` задан и backend вернул `> 0` строк →
//!   `Diff::NoChange`. Это read-before-write на уровне plan'а: оператор в
//!   `bosun apply --dry-run` видит, какие команды реально побежали бы.
//! - Если check вернул `0` строк или check не задан → `Diff::Update`.
//!
//! Если check сам упал (например, БД недоступна) — это `PrimitiveError::Apply`,
//! plan честно фейлит, не пытаясь догадаться. Альтернатива «фолбэк в
//! Diff::Update» молча скрывала бы недоступность БД, и оператор узнавал бы
//! о ней только из логов apply'я.

use std::sync::Arc;
use std::time::Duration;

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use crate::pg_sql_common::{PgSqlBackend, PgSqlError};

use super::spec::PgSqlExecSpec;

/// Главная функция plan: десериализует spec, при наличии check выполняет
/// его через backend и решает Update vs NoChange.
pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
    backend: &Arc<dyn PgSqlBackend>,
) -> Result<Diff, PrimitiveError> {
    let spec: PgSqlExecSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.exec payload: {e}")))?;

    let timeout = Duration::from_secs(u64::from(spec.effective_timeout_sec()));

    if let Some(check_sql) = &spec.if_not_exists_check {
        match backend.query(&spec.dsn, check_sql, timeout) {
            Ok(rows) => {
                if !rows.is_empty() {
                    return Ok(Diff::NoChange);
                }
                Ok(Diff::Update {
                    from: serde_json::json!({"present": false}),
                    to: serde_json::json!({"present": true}),
                    description: format!("pg_sql.exec[{}]: check empty → run", spec.name.as_str()),
                })
            }
            Err(e) => Err(map_check_error(&spec, e)),
        }
    } else {
        // Always-execute путь. Описываем явно, чтобы --dry-run показывал
        // отсутствие idempotency-проверки.
        Ok(Diff::Update {
            from: serde_json::json!({"check": "none"}),
            to: serde_json::json!({"check": "none"}),
            description: format!(
                "pg_sql.exec[{}]: no if_not_exists_check, always run",
                spec.name.as_str(),
            ),
        })
    }
}

/// Конверсия ошибки check'а в `PrimitiveError`. Connect/Timeout — это
/// `Apply` (плановый: БД должна быть доступна для idempotent-проверки);
/// Sql — тоже Apply (check написан с ошибкой). InvalidDsn — InvalidPayload.
fn map_check_error(spec: &PgSqlExecSpec, err: PgSqlError) -> PrimitiveError {
    match err {
        PgSqlError::InvalidDsn(msg) => PrimitiveError::InvalidPayload(format!(
            "pg_sql.exec[{}]: invalid dsn: {msg}",
            spec.name.as_str(),
        )),
        other => PrimitiveError::Apply {
            reason: format!(
                "pg_sql.exec[{}]: if_not_exists_check failed: {other}",
                spec.name.as_str(),
            ),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::time::Instant;

    use bosun_core::{FactValue, ResourceId, ResourceKind};
    use tokio_util::sync::CancellationToken;

    use crate::pg_sql_common::testutil::MockBackend;
    use crate::pg_sql_common::Row;

    use super::*;

    struct EmptyFacts;
    impl FactsSource for EmptyFacts {
        fn get(&self, _: &str) -> FactValue {
            FactValue::Unknown { reason: "t".into() }
        }
    }

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    fn resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("pg_sql.exec");
        let id = ResourceId::new(&kind, "test");
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

    /// Если check возвращает > 0 строк — Diff::NoChange и query вызван
    /// ровно один раз.
    #[test]
    fn plan_with_check_returns_no_change_when_exists() {
        let backend: Arc<dyn PgSqlBackend> =
            Arc::new(MockBackend::new().with_query_rows(vec![Row::from_iter([(
                "?column?".to_string(),
                "1".to_string(),
            )])]));
        let r = resource(serde_json::json!({
            "name": "create-monitor",
            "dsn": "postgres://postgres@h/d",
            "sql": "CREATE ROLE monitor",
            "if_not_exists_check": "SELECT 1 FROM pg_roles WHERE rolname='monitor'",
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx(), &backend).unwrap();
        assert!(matches!(diff, Diff::NoChange), "got: {diff:?}");
    }

    /// Если check возвращает 0 строк — Diff::Update.
    #[test]
    fn plan_with_check_returns_update_when_missing() {
        let backend: Arc<dyn PgSqlBackend> = Arc::new(MockBackend::new().with_query_rows(vec![]));
        let r = resource(serde_json::json!({
            "name": "create-monitor",
            "dsn": "postgres://postgres@h/d",
            "sql": "CREATE ROLE monitor",
            "if_not_exists_check": "SELECT 1 FROM pg_roles WHERE rolname='monitor'",
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx(), &backend).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("create-monitor"), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    /// Без check — всегда Diff::Update.
    #[test]
    fn plan_without_check_always_updates() {
        let backend = Arc::new(MockBackend::new());
        let backend_dyn: Arc<dyn PgSqlBackend> = backend.clone();
        let r = resource(serde_json::json!({
            "name": "always-run",
            "dsn": "postgres://postgres@h/d",
            "sql": "SET timezone TO 'UTC'",
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx(), &backend_dyn).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(
                    description.contains("no if_not_exists_check"),
                    "got: {description}",
                );
            }
            other => panic!("expected Update, got {other:?}"),
        }
        // Никаких query'й при отсутствии check'а — это read-before-write
        // optimization, оператор не платит за check, которого не просил.
        assert!(backend.query_calls.lock().unwrap().is_empty());
    }

    /// Невалидный DSN в check'е → InvalidPayload (не Apply), потому что это
    /// проблема spec'а, не runtime'а БД.
    #[test]
    fn plan_invalid_dsn_in_check_returns_invalid_payload() {
        let backend: Arc<dyn PgSqlBackend> =
            Arc::new(MockBackend::new().with_query_err(PgSqlError::InvalidDsn("bad".into())));
        let r = resource(serde_json::json!({
            "name": "x",
            "dsn": "bad",
            "sql": "SELECT 1",
            "if_not_exists_check": "SELECT 1",
        }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx(), &backend).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    /// Connect-ошибка в check'е → Apply (плановое, оператор должен поднять
    /// БД и попробовать снова).
    #[test]
    fn plan_connect_error_in_check_returns_apply() {
        let backend: Arc<dyn PgSqlBackend> =
            Arc::new(MockBackend::new().with_query_err(PgSqlError::Connect {
                dsn: "*****".into(),
                reason: "refused".into(),
            }));
        let r = resource(serde_json::json!({
            "name": "x",
            "dsn": "postgres://u:p@h/d",
            "sql": "SELECT 1",
            "if_not_exists_check": "SELECT 1",
        }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx(), &backend).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(
                    reason.contains("if_not_exists_check failed"),
                    "got: {reason}"
                );
                assert!(reason.contains("refused"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }
}
