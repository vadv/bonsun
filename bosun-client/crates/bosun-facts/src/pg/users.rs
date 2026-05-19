//! Коллектор `pg_users_with_passwords` — список логинабельных ролей с хэшами
//! паролей из `pg_authid`.
//!
//! Зачем factом, а не диффом в самой роли postgres_manage:
//! - Plan-фаза должна быть детерминированной от snapshot'а — иначе при
//!   повторном prog'е plan меняется, нарушая инвариант idempotency.
//! - Хранение паролей в JSON-валуе факта — это хэши (SCRAM/MD5), не plaintext.
//!   Утечка хэша приемлема по нашему threat model'у; для plaintext'а
//!   роль использует `users_with_passwords` overlay (см. inventory).

use std::sync::Arc;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use super::query::PgFactQuery;
use crate::collector::{Fact, FactCollectCtx};

pub struct PgUsersFact {
    query: Option<Arc<dyn PgFactQuery>>,
}

impl PgUsersFact {
    pub fn new(query: Option<Arc<dyn PgFactQuery>>) -> Self {
        Self { query }
    }
}

impl Fact for PgUsersFact {
    fn name(&self) -> &str {
        "pg_users_with_passwords"
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
        match query.users() {
            Ok(rows) => {
                let users: Vec<serde_json::Value> = rows
                    .into_iter()
                    .map(|(name, pass)| {
                        serde_json::json!({
                            "name": name,
                            "password_hash": pass.unwrap_or_default(),
                        })
                    })
                    .collect();
                FactValue::Known(serde_json::json!({ "users": users }))
            }
            Err(e) => FactValue::Unknown {
                reason: format!("pg_authid query failed: {e}"),
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
        let f = PgUsersFact::new(None);
        assert_eq!(f.name(), "pg_users_with_passwords");
        assert_eq!(f.category(), FactCategory::Discovery);
        assert!(matches!(f.refresh_policy(), RefreshPolicy::AtStart));
    }

    #[test]
    fn returns_unknown_when_no_query() {
        let f = PgUsersFact::new(None);
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
    fn known_returns_list_with_password_hashes() {
        let mock = Arc::new(MockPgFactQuery::new().with_users(vec![
            ("alice".into(), Some("SCRAM-SHA-256$...".into())),
            ("bob".into(), Some("md5abcdef".into())),
        ]));
        let f = PgUsersFact::new(Some(mock));
        let v = f.collect(&ctx());
        let users = v.value().unwrap().get("users").unwrap().as_array().unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0]["name"], "alice");
        assert_eq!(users[0]["password_hash"], "SCRAM-SHA-256$...");
        assert_eq!(users[1]["name"], "bob");
        assert_eq!(users[1]["password_hash"], "md5abcdef");
    }

    #[test]
    fn known_returns_empty_password_for_role_without_hash() {
        // pg_authid даёт rolpassword = NULL для ролей без задания пароля
        // (пере-делегация на pg_ident.conf или внешняя auth). Мы маппим
        // в пустую строку — апликабельная роль увидит «пароль не задан».
        let mock = Arc::new(MockPgFactQuery::new().with_users(vec![("carol".into(), None)]));
        let f = PgUsersFact::new(Some(mock));
        let v = f.collect(&ctx());
        let users = v.value().unwrap().get("users").unwrap().as_array().unwrap();
        assert_eq!(users[0]["password_hash"], "");
    }

    #[test]
    fn known_returns_empty_list_for_no_users() {
        let mock = Arc::new(MockPgFactQuery::new().with_users(Vec::new()));
        let f = PgUsersFact::new(Some(mock));
        let v = f.collect(&ctx());
        let users = v.value().unwrap().get("users").unwrap().as_array().unwrap();
        assert_eq!(users.len(), 0);
    }

    #[test]
    fn returns_unknown_on_query_error() {
        let mock = Arc::new(
            MockPgFactQuery::new().with_users_err("permission denied for table pg_authid"),
        );
        let f = PgUsersFact::new(Some(mock));
        match f.collect(&ctx()) {
            FactValue::Unknown { reason } => {
                assert!(reason.contains("pg_authid"), "reason: {reason}");
                assert!(reason.contains("permission denied"), "reason: {reason}");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
