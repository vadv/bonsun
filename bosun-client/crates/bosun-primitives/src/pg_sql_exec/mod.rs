//! Примитив `pg_sql.exec` — выполнение SQL-команды (DDL/DML/GRANT/etc) через
//! sync PostgreSQL-клиент.
//!
//! chiit-аналог: `lib/utils/pg/{users.go,grant.go,extension.go,alter_role.go}`
//! делает CREATE ROLE / GRANT / CREATE EXTENSION с предварительным
//! «уже сделано»-чеком в pg_catalog. Этот примитив воспроизводит ту же
//! семантику декларативно: автор bundle пишет SQL и опциональный
//! `if_not_exists_check` (SELECT, который должен вернуть `> 0` строк, чтобы
//! exec пропустили).
//!
//! Read-before-write выдержан строго: и plan, и apply делают check перед
//! exec'ом. Apply делает re-check (rece-чек), чтобы между plan и apply
//! другая сессия не успела создать объект.

mod apply;
mod plan;
mod spec;

use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

use crate::pg_sql_common::{PgSqlBackend, RealPgSqlBackend};

pub use spec::PgSqlExecSpec;

/// Реализация Primitive для `pg_sql.exec`. Stateless, держит DI-backend
/// в `Arc<dyn PgSqlBackend>` — тестам это позволяет не запускать
/// реальный PostgreSQL.
pub struct PgSqlExecPrimitive {
    backend: Arc<dyn PgSqlBackend>,
}

impl PgSqlExecPrimitive {
    /// Конструктор с явным backend'ом. Для production-CLI см.
    /// [`PgSqlExecPrimitive::with_real_backend`].
    pub fn new(backend: Arc<dyn PgSqlBackend>) -> Self {
        Self { backend }
    }

    /// Удобный конструктор для production: внутри `Arc<RealPgSqlBackend>`.
    pub fn with_real_backend() -> Self {
        Self::new(Arc::new(RealPgSqlBackend))
    }
}

impl Default for PgSqlExecPrimitive {
    fn default() -> Self {
        Self::with_real_backend()
    }
}

impl Primitive for PgSqlExecPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("pg_sql.exec")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.exec: {e}")))?;
        let dsn = args
            .required_str("dsn")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.exec: {e}")))?;
        let sql = args
            .required_str("sql")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.exec: {e}")))?;
        let if_not_exists_check = args
            .optional_str("if_not_exists_check")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.exec: {e}")))?;
        let timeout_sec = args
            .optional_u32("timeout_sec")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("pg_sql.exec: {e}")))?;

        Ok(serde_json::json!({
            "name": name,
            "dsn": dsn,
            "sql": sql,
            "if_not_exists_check": if_not_exists_check,
            "timeout_sec": timeout_sec,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        facts: &dyn FactsSource,
        ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        plan::compute_diff(resource, facts, ctx, &self.backend)
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
    fn type_name_is_pg_sql_exec() {
        let p = PgSqlExecPrimitive::with_real_backend();
        assert_eq!(p.type_name(), ResourceKind::from_static("pg_sql.exec"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = PgSqlExecPrimitive::with_real_backend();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("create-monitor".into()));
        args.insert("dsn".into(), ArgValue::Str("postgres://u@h/d".into()));
        args.insert("sql".into(), ArgValue::Str("CREATE ROLE monitor".into()));
        let call_args = CallArgs::new(args);
        let payload = PgSqlExecPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "create-monitor");
        assert_eq!(payload["dsn"], "postgres://u@h/d");
        assert_eq!(payload["sql"], "CREATE ROLE monitor");
        assert!(payload["if_not_exists_check"].is_null());
        assert!(payload["timeout_sec"].is_null());
    }

    #[test]
    fn build_payload_with_check_and_timeout() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        args.insert("dsn".into(), ArgValue::Str("d".into()));
        args.insert("sql".into(), ArgValue::Str("s".into()));
        args.insert(
            "if_not_exists_check".into(),
            ArgValue::Str("SELECT 1".into()),
        );
        args.insert("timeout_sec".into(), ArgValue::Int(60));
        let call_args = CallArgs::new(args);
        let payload = PgSqlExecPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["if_not_exists_check"], "SELECT 1");
        assert_eq!(payload["timeout_sec"], 60);
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("dsn".into(), ArgValue::Str("d".into()));
        args.insert("sql".into(), ArgValue::Str("s".into()));
        let err = PgSqlExecPrimitive::with_real_backend()
            .build_payload(&CallArgs::new(args), &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name"), "got: {msg}"),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_dsn_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        args.insert("sql".into(), ArgValue::Str("s".into()));
        let err = PgSqlExecPrimitive::with_real_backend()
            .build_payload(&CallArgs::new(args), &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("dsn"), "got: {msg}"),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_sql_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        args.insert("dsn".into(), ArgValue::Str("d".into()));
        let err = PgSqlExecPrimitive::with_real_backend()
            .build_payload(&CallArgs::new(args), &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("sql"), "got: {msg}"),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
