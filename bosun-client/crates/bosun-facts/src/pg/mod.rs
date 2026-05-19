//! PG discovery facts — четыре дискавер-факта о локальном PostgreSQL.
//!
//! Архитектура:
//! - `query` — DI-trait `PgFactQuery` и production-реализация `RealPgFactQuery`.
//! - `is_master` / `users` / `extensions` — три факта поверх трёх SQL-запросов.
//!   Все три используют общий `Arc<dyn PgFactQuery>`, что в тестах позволяет
//!   подменить один мок и проверить все три факта согласованно.
//! - `initialized` — четвёртый факт; file-based, без PG-клиента.
//!
//! Опциональность: если на ноде нет PG (нет unix-socket'а
//! `/var/run/postgresql`), `build_pg_facts` отдаёт коллекторам `query: None`,
//! и при сборе они возвращают `Unknown { reason: "no PostgreSQL detected ..." }`.
//! Это нужно для unified catalog'а: PG-факты регистрируются всегда, чтобы
//! Strict-режим в Starlark получал стабильный набор имён независимо от того,
//! какая роль раскатывается на ноде.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::collector::Fact;

pub mod extensions;
pub mod initialized;
pub mod is_master;
pub mod query;
pub mod users;

pub use extensions::PgExtensionsFact;
pub use initialized::PgInitializedFact;
pub use is_master::PgIsMasterFact;
pub use query::{PgFactQuery, PgFactQueryError, RealPgFactQuery};
pub use users::PgUsersFact;

/// Default connect-timeout для `RealPgFactQuery`. Discovery-факты собираются
/// раз на старте, и если PG зависает дольше — мы предпочтём `Unknown` с
/// reason, чем длительный stall.
const DEFAULT_PG_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Стандартный путь до unix-сокета postgres'а. Если каталог существует под
/// `root_fs`, считаем, что PG установлен и доступен через socket'ы — даже если
/// сам процесс ещё не запущен (auto-detect отдаст DSN, connect упадёт, и
/// факт уйдёт в Unknown с конкретным reason'ом).
const POSTGRES_SOCKET_DIR: &str = "var/run/postgresql";

/// Резолвинг DSN: если задан вручную — берём как есть, иначе пробуем
/// автодетект через `<root_fs>/var/run/postgresql`. Возврат `None`
/// означает «PG не обнаружен», и фабрика создаст факты без клиента.
pub fn resolve_dsn(root_fs: &Path, override_dsn: Option<&str>) -> Option<String> {
    if let Some(dsn) = override_dsn {
        return Some(dsn.to_string());
    }
    let socket_dir = root_fs.join(POSTGRES_SOCKET_DIR);
    if socket_dir.exists() {
        // libpq формат: `host=/var/run/postgresql` указывает на unix-socket.
        // dbname=postgres — стандартная maintenance-БД, существует на любом
        // initialised-кластере. application_name=bosun-facts помогает в
        // pg_stat_activity отделить наши connect'ы от пользовательских.
        return Some(format!(
            "host=/{POSTGRES_SOCKET_DIR} user=postgres dbname=postgres application_name=bosun-facts"
        ));
    }
    None
}

/// Построить набор PG-discovery-фактов. Если DSN не задан и сокет не
/// найден — отдаст `query: None`, факты ответят `Unknown`.
///
/// Production вызов: `build_pg_facts(root_fs, None)` для автодетекта.
/// Тесты могут передать кастомный DSN или подменить `root_fs`.
pub fn build_pg_facts(root_fs: &Path, override_dsn: Option<&str>) -> Vec<Box<dyn Fact>> {
    let dsn = resolve_dsn(root_fs, override_dsn);
    let query: Option<Arc<dyn PgFactQuery>> = dsn.map(|d| {
        let real = RealPgFactQuery::new(d, DEFAULT_PG_CONNECT_TIMEOUT);
        let arc: Arc<dyn PgFactQuery> = Arc::new(real);
        arc
    });
    vec![
        Box::new(PgInitializedFact),
        Box::new(PgIsMasterFact::new(query.clone())),
        Box::new(PgUsersFact::new(query.clone())),
        Box::new(PgExtensionsFact::new(query)),
    ]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn resolve_dsn_uses_override_when_present() {
        let tmp = TempDir::new().unwrap();
        let dsn = resolve_dsn(tmp.path(), Some("host=custom user=test"));
        assert_eq!(dsn.as_deref(), Some("host=custom user=test"));
    }

    #[test]
    fn resolve_dsn_returns_some_when_socket_dir_exists() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("var/run/postgresql")).unwrap();
        let dsn = resolve_dsn(tmp.path(), None);
        let dsn = dsn.unwrap();
        assert!(dsn.contains("host=/var/run/postgresql"), "got: {dsn}");
        assert!(dsn.contains("user=postgres"), "got: {dsn}");
        assert!(dsn.contains("application_name=bosun-facts"), "got: {dsn}");
    }

    #[test]
    fn resolve_dsn_returns_none_when_no_pg_signs() {
        let tmp = TempDir::new().unwrap();
        let dsn = resolve_dsn(tmp.path(), None);
        assert!(dsn.is_none());
    }

    #[test]
    fn build_pg_facts_returns_four_facts() {
        let tmp = TempDir::new().unwrap();
        let facts = build_pg_facts(tmp.path(), None);
        let names: Vec<&str> = facts.iter().map(|f| f.name()).collect();
        assert_eq!(names.len(), 4);
        assert!(names.contains(&"pg_initialized"));
        assert!(names.contains(&"pg_is_master"));
        assert!(names.contains(&"pg_users_with_passwords"));
        assert!(names.contains(&"pg_extensions"));
    }

    #[test]
    fn build_pg_facts_without_pg_produces_unknown_for_sql_facts() {
        // Когда PG не обнаружен — query=None, и три SQL-факта возвращают
        // Unknown с понятным reason. pg_initialized остаётся file-based и
        // отдаёт Known { initialized: false }.
        use bosun_core::FactValue;

        use crate::collector::FactCollectCtx;

        let tmp = TempDir::new().unwrap();
        let facts = build_pg_facts(tmp.path(), None);
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        for fact in &facts {
            let v = fact.collect(&ctx);
            match fact.name() {
                "pg_initialized" => {
                    assert!(matches!(v, FactValue::Known(_)), "{}: {v:?}", fact.name());
                    assert_eq!(v.value().unwrap()["initialized"], false);
                }
                _ => match v {
                    FactValue::Unknown { reason } => {
                        assert!(
                            reason.contains("no PostgreSQL detected"),
                            "reason: {reason}"
                        );
                    }
                    other => panic!("expected Unknown for {}, got {other:?}", fact.name()),
                },
            }
        }
    }
}
