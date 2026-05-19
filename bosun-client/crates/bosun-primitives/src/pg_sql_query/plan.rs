//! Plan-фаза `pg_sql.query`.
//!
//! Семантика «всегда Update»: query сам по себе не имеет idempotency-
//! проверки — каждый запуск читает текущее состояние БД. Если плагин
//! хочет skip — пусть пишет соответствующий exec через `pg_sql.exec` с
//! `if_not_exists_check`.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use super::spec::PgSqlQuerySpec;

pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: PgSqlQuerySpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.query payload: {e}")))?;

    let description = match &spec.store_as_fact {
        Some(name) => format!(
            "pg_sql.query[{}]: execute SELECT, publish fact '{name}'",
            spec.name.as_str(),
        ),
        None => format!("pg_sql.query[{}]: execute SELECT", spec.name.as_str()),
    };
    Ok(Diff::Update {
        from: serde_json::json!({"query": "stateless"}),
        to: resource.payload.clone(),
        description,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::time::{Duration, Instant};

    use bosun_core::{FactValue, ResourceId, ResourceKind};
    use tokio_util::sync::CancellationToken;

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
        let kind = ResourceKind::from_static("pg_sql.query");
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

    #[test]
    fn plan_returns_update_with_select_description() {
        let r = resource(serde_json::json!({
            "name": "list",
            "dsn": "x",
            "sql": "SELECT 1",
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("execute SELECT"), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn plan_includes_fact_name_when_store_as_fact_set() {
        let r = resource(serde_json::json!({
            "name": "list",
            "dsn": "x",
            "sql": "SELECT 1",
            "store_as_fact": "pg.roles",
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("pg.roles"), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }
}
