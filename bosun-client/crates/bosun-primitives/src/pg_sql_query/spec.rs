//! Spec примитива `pg_sql.query`.
//!
//! Семантика — «выполнить SELECT и вернуть строки как Vec<BTreeMap>».
//! Используется для discovery-фактов (список пользователей, статус
//! pg_is_in_recovery, версия pg_extension). При наличии `store_as_fact`
//! результат публикуется в [`ApplyCtx::publish_fact`].
//!
//! В отличие от exec, query всегда без побочных эффектов в БД — но он
//! всегда нужен, потому что результаты могут понадобиться следующим
//! ресурсам (через published_facts). Поэтому plan возвращает Diff::Update
//! безусловно.

use bosun_core::UnitName;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct PgSqlQuerySpec {
    pub name: UnitName,
    pub dsn: String,
    /// SELECT-statement. Любой SQL, который возвращает rows. Если query
    /// вернул 0 строк — это валидный результат, факт публикуется как
    /// пустой массив.
    pub sql: String,
    #[serde(default)]
    pub timeout_sec: Option<u32>,
    /// Если задано — результат публикуется в `ApplyCtx::publish_fact`
    /// под этим именем. Иначе просто логируем и считаем changed (no_change
    /// невозможен, потому что query всегда наблюдает текущее состояние).
    #[serde(default)]
    pub store_as_fact: Option<String>,
}

impl PgSqlQuerySpec {
    pub const DEFAULT_TIMEOUT_SEC: u32 = 30;

    pub fn effective_timeout_sec(&self) -> u32 {
        self.timeout_sec.unwrap_or(Self::DEFAULT_TIMEOUT_SEC)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum_required() {
        let json = serde_json::json!({
            "name": "list-roles",
            "dsn": "postgres://u@h/d",
            "sql": "SELECT rolname FROM pg_roles",
        });
        let spec: PgSqlQuerySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name.as_str(), "list-roles");
        assert!(spec.store_as_fact.is_none());
        assert_eq!(spec.effective_timeout_sec(), 30);
    }

    #[test]
    fn deserialize_with_store_as_fact() {
        let json = serde_json::json!({
            "name": "list-roles",
            "dsn": "x",
            "sql": "SELECT 1",
            "store_as_fact": "pg.roles",
            "timeout_sec": 15,
        });
        let spec: PgSqlQuerySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.store_as_fact.as_deref(), Some("pg.roles"));
        assert_eq!(spec.effective_timeout_sec(), 15);
    }
}
