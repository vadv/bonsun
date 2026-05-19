//! Коллектор `pg_is_master` — определяет роль кластера по `pg_is_in_recovery()`.
//!
//! Зачем discovery-факт, а не явный `pg_sql.query` в Starlark:
//! - postgres_manage применяет одни ресурсы на master, другие на replica
//!   (CREATE EXTENSION только на master, ALTER ROLE на обоих, и т.д.).
//! - Без factа эти ветвления требуют живого SELECT'а в Starlark, что плохо
//!   композируется с Strict-режимом — fact-snapshot фиксирован к моменту eval.

use std::sync::Arc;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use super::query::PgFactQuery;
use crate::collector::{Fact, FactCollectCtx};

pub struct PgIsMasterFact {
    /// Поставщик результата `pg_is_in_recovery()`. Опционален: если
    /// PG на ноде не обнаружен (catalog не построил `RealPgFactQuery`),
    /// факт сразу возвращает `Unknown` с понятным reason.
    query: Option<Arc<dyn PgFactQuery>>,
}

impl PgIsMasterFact {
    pub fn new(query: Option<Arc<dyn PgFactQuery>>) -> Self {
        Self { query }
    }
}

impl Fact for PgIsMasterFact {
    fn name(&self) -> &str {
        "pg_is_master"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Discovery
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AtStart
    }
    fn collect(&self, _ctx: &FactCollectCtx) -> FactValue {
        let Some(query) = &self.query else {
            return FactValue::Unknown {
                reason: "no PostgreSQL detected on this node".to_string(),
            };
        };
        match query.is_master() {
            Ok(is_master) => FactValue::Known(serde_json::json!({ "is_master": is_master })),
            Err(e) => FactValue::Unknown {
                reason: format!("pg_is_in_recovery query failed: {e}"),
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use super::super::query::mock::MockPgFactQuery;
    use super::*;

    fn ctx() -> FactCollectCtx {
        FactCollectCtx::new(PathBuf::from("/"))
    }

    #[test]
    fn name_category_and_policy_are_stable() {
        let f = PgIsMasterFact::new(None);
        assert_eq!(f.name(), "pg_is_master");
        assert_eq!(f.category(), FactCategory::Discovery);
        assert!(matches!(f.refresh_policy(), RefreshPolicy::AtStart));
    }

    #[test]
    fn returns_unknown_when_no_query() {
        let f = PgIsMasterFact::new(None);
        match f.collect(&ctx()) {
            FactValue::Unknown { reason } => {
                assert!(
                    reason.contains("no PostgreSQL detected"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn known_true_when_not_in_recovery() {
        let mock = Arc::new(MockPgFactQuery::new().with_is_master(true));
        let f = PgIsMasterFact::new(Some(mock));
        let v = f.collect(&ctx());
        assert_eq!(v.value().unwrap(), &serde_json::json!({"is_master": true}));
    }

    #[test]
    fn known_false_when_in_recovery() {
        let mock = Arc::new(MockPgFactQuery::new().with_is_master(false));
        let f = PgIsMasterFact::new(Some(mock));
        let v = f.collect(&ctx());
        assert_eq!(v.value().unwrap(), &serde_json::json!({"is_master": false}));
    }

    #[test]
    fn returns_unknown_on_query_error() {
        let mock = Arc::new(MockPgFactQuery::new().with_is_master_err("connection refused"));
        let f = PgIsMasterFact::new(Some(mock));
        match f.collect(&ctx()) {
            FactValue::Unknown { reason } => {
                assert!(reason.contains("pg_is_in_recovery"), "reason: {reason}");
                assert!(reason.contains("connection refused"), "reason: {reason}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
