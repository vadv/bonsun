//! Коллектор `pg_extensions` — список установленных расширений из `pg_extension`.
//!
//! Зачем factом: postgres_manage решает, нужно ли `CREATE EXTENSION`. Без
//! discovery приходилось бы делать `pg_sql.query` в Starlark на каждое
//! расширение — quadratic в количестве extensions и не композируется со
//! Strict-режимом, где fact-snapshot должен быть готов к моменту eval'а.

use std::sync::Arc;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use super::query::PgFactQuery;
use crate::collector::{Fact, FactCollectCtx};

pub struct PgExtensionsFact {
    query: Option<Arc<dyn PgFactQuery>>,
}

impl PgExtensionsFact {
    pub fn new(query: Option<Arc<dyn PgFactQuery>>) -> Self {
        Self { query }
    }
}

impl Fact for PgExtensionsFact {
    fn name(&self) -> &str {
        "pg_extensions"
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
        match query.extensions() {
            Ok(rows) => {
                let extensions: Vec<serde_json::Value> = rows
                    .into_iter()
                    .map(|(name, version)| {
                        serde_json::json!({
                            "name": name,
                            "version": version,
                        })
                    })
                    .collect();
                FactValue::Known(serde_json::json!({ "extensions": extensions }))
            }
            Err(e) => FactValue::Unknown {
                reason: format!("pg_extension query failed: {e}"),
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
    fn name_and_policy_are_stable() {
        let f = PgExtensionsFact::new(None);
        assert_eq!(f.name(), "pg_extensions");
        assert_eq!(f.category(), FactCategory::Discovery);
        assert!(matches!(f.refresh_policy(), RefreshPolicy::AtStart));
    }

    #[test]
    fn returns_unknown_when_no_query() {
        let f = PgExtensionsFact::new(None);
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
    fn known_returns_list_of_extensions() {
        let mock = Arc::new(MockPgFactQuery::new().with_extensions(vec![
            ("plpgsql".into(), "1.0".into()),
            ("pg_stat_statements".into(), "1.10".into()),
        ]));
        let f = PgExtensionsFact::new(Some(mock));
        let v = f.collect(&ctx());
        let exts = v
            .value()
            .unwrap()
            .get("extensions")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(exts.len(), 2);
        assert_eq!(exts[0]["name"], "plpgsql");
        assert_eq!(exts[0]["version"], "1.0");
        assert_eq!(exts[1]["name"], "pg_stat_statements");
        assert_eq!(exts[1]["version"], "1.10");
    }

    #[test]
    fn known_returns_empty_list_for_pristine_cluster() {
        let mock = Arc::new(MockPgFactQuery::new().with_extensions(Vec::new()));
        let f = PgExtensionsFact::new(Some(mock));
        let v = f.collect(&ctx());
        let exts = v
            .value()
            .unwrap()
            .get("extensions")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(exts.len(), 0);
    }

    #[test]
    fn returns_unknown_on_query_error() {
        let mock =
            Arc::new(MockPgFactQuery::new().with_extensions_err("relation pg_extension missing"));
        let f = PgExtensionsFact::new(Some(mock));
        match f.collect(&ctx()) {
            FactValue::Unknown { reason } => {
                assert!(reason.contains("pg_extension"), "reason: {reason}");
                assert!(reason.contains("missing"), "reason: {reason}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
