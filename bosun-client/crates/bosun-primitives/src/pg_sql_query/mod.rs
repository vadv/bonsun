//! Примитив `pg_sql.query` — выполнение SELECT и опциональная публикация
//! результата как факта.
//!
//! chiit-аналог: `postgres-chiit/lib/pg/list_users.go::ListUsers` —
//! читает существующих postgres-пользователей. Здесь то же декларативно:
//! автор bundle пишет SELECT и опционально просит сохранить результат в
//! published_facts для следующих ресурсов.
//!
//! В отличие от Phase D-фактов из bosun-facts (`hostname`, `cpu_count` и т.п.),
//! published_facts — runtime-only registry: они доступны только в рамках
//! одного apply-цикла. Читаются двумя способами: через
//! `ApplyCtx::read_published_fact` (прямой доступ из apply другого примитива)
//! и через штатный `FactsSource::get` — CLI оборачивает обычный
//! `FactsView` в `OverlayFactsSource`, который сначала смотрит
//! published_facts, потом основной snapshot. Это сознательная асимметрия:
//! фактовая инфраструктура — слишком тяжёлая для динамического per-bundle
//! factset'а, а ad-hoc запросы — частые.

mod apply;
mod plan;
mod spec;

use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

use crate::pg_sql_common::{PgSqlBackend, RealPgSqlBackend};

pub use spec::PgSqlQuerySpec;

pub struct PgSqlQueryPrimitive {
    backend: Arc<dyn PgSqlBackend>,
}

impl PgSqlQueryPrimitive {
    pub fn new(backend: Arc<dyn PgSqlBackend>) -> Self {
        Self { backend }
    }

    pub fn with_real_backend() -> Self {
        Self::new(Arc::new(RealPgSqlBackend))
    }
}

impl Default for PgSqlQueryPrimitive {
    fn default() -> Self {
        Self::with_real_backend()
    }
}

impl Primitive for PgSqlQueryPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("pg_sql.query")
    }

    fn identity_keys(&self) -> &'static [&'static str] {
        &["name"]
    }

    fn build_payload(
        &self,
        args: &CallArgs,
        _ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError> {
        let name = args
            .required_str("name")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.query: {e}")))?;
        let dsn = args
            .required_str("dsn")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.query: {e}")))?;
        let sql = args
            .required_str("sql")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.query: {e}")))?;
        let timeout_sec = args
            .optional_u32("timeout_sec")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.query: {e}")))?;
        let store_as_fact = args
            .optional_str("store_as_fact")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.query: {e}")))?;

        Ok(serde_json::json!({
            "name": name,
            "dsn": dsn,
            "sql": sql,
            "timeout_sec": timeout_sec,
            "store_as_fact": store_as_fact,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        facts: &dyn FactsSource,
        ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        plan::compute_diff(resource, facts, ctx)
    }

    fn apply(
        &self,
        resource: &Resource,
        diff: &Diff,
        ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError> {
        apply::run(resource, diff, ctx, &self.backend)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use bosun_core::{ArgValue, PlanCtx};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    #[test]
    fn type_name_is_pg_sql_query() {
        let p = PgSqlQueryPrimitive::with_real_backend();
        assert_eq!(p.type_name(), ResourceKind::from_static("pg_sql.query"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = PgSqlQueryPrimitive::with_real_backend();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("list".into()));
        args.insert("dsn".into(), ArgValue::Str("postgres://u@h/d".into()));
        args.insert("sql".into(), ArgValue::Str("SELECT 1".into()));
        let payload = PgSqlQueryPrimitive::with_real_backend()
            .build_payload(&CallArgs::new(args), &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "list");
        assert!(payload["store_as_fact"].is_null());
        assert!(payload["timeout_sec"].is_null());
    }

    #[test]
    fn build_payload_with_store_as_fact_and_timeout() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        args.insert("dsn".into(), ArgValue::Str("d".into()));
        args.insert("sql".into(), ArgValue::Str("s".into()));
        args.insert("timeout_sec".into(), ArgValue::Int(15));
        args.insert("store_as_fact".into(), ArgValue::Str("pg.users".into()));
        let payload = PgSqlQueryPrimitive::with_real_backend()
            .build_payload(&CallArgs::new(args), &plan_ctx())
            .unwrap();
        assert_eq!(payload["timeout_sec"], 15);
        assert_eq!(payload["store_as_fact"], "pg.users");
    }

    #[test]
    fn build_payload_missing_sql_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        args.insert("dsn".into(), ArgValue::Str("d".into()));
        let err = PgSqlQueryPrimitive::with_real_backend()
            .build_payload(&CallArgs::new(args), &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("sql"), "got: {msg}"),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
