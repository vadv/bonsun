//! Spec примитива `pg_sql.exec`.
//!
//! Семантика — «выполнить SQL-команду через postgres-клиент». DDL/GRANT/INSERT
//! и прочие не-SELECT-команды, для которых prepared statements
//! не подходят (PG не поддерживает PREPARE для utility-команд). Один
//! ресурс — одна логическая операция: «создать роль X», «грантнуть Y на Z».
//!
//! Идемпотентность достигается через опциональный `if_not_exists_check` —
//! SELECT-запрос, который должен вернуть `> 0` строк, чтобы apply пропустил
//! exec. Это явный read-before-write контракт: оператор сам решает, по какому
//! признаку измерять «уже сделано» (роль есть в `pg_roles`, grant есть в
//! `information_schema.role_table_grants` и т.п.).
//!
//! Если check не задан — exec вызывается всегда. Это допустимо для
//! сценариев типа `SET search_path TO ...` или повторно безопасных DDL
//! (`CREATE TABLE IF NOT EXISTS`), но автор bundle'а явно принимает риск.

use bosun_core::UnitName;
use serde::Deserialize;

/// Spec ресурса `pg_sql.exec`.
///
/// `name` валидируется как `UnitName` — это часть идентичности ресурса
/// (`pg_sql.exec[<name>]`), и попадает в логи/метрики.
#[derive(Clone, Debug, Deserialize)]
pub struct PgSqlExecSpec {
    /// Уникальное имя ресурса. Используется в `ResourceId` и в логах.
    pub name: UnitName,
    /// DSN postgres. Допускаются оба формата: URL `postgres://...` и
    /// libpq key=value `host=... user=... dbname=...`. Пароль (если есть)
    /// логируется только через `redact_dsn`.
    pub dsn: String,
    /// SQL-команда (одно или несколько statement'ов через `;`). Передаётся
    /// в `batch_execute` без подготовки. Это означает: интерполяция
    /// параметров через `$1` не работает; все значения должны быть зашиты
    /// в текст SQL автором bundle'а.
    pub sql: String,
    /// Опциональная проверка «уже сделано». Должен возвращать `> 0` строк,
    /// чтобы apply пропустил exec. Например:
    /// `SELECT 1 FROM pg_roles WHERE rolname='monitor'`.
    /// Если None — exec вызывается всегда (non-idempotent).
    #[serde(default)]
    pub if_not_exists_check: Option<String>,
    /// Таймаут операции. Применяется и к connect, и к выполнению через
    /// `SET LOCAL statement_timeout`. Default 30 секунд — это стандартная
    /// верхняя граница для DDL/GRANT-операций в постгресе chiit-парка.
    #[serde(default)]
    pub timeout_sec: Option<u32>,
}

impl PgSqlExecSpec {
    /// Default-таймаут для exec-операций. Используется, когда `timeout_sec`
    /// не задан явно в spec'е.
    pub const DEFAULT_TIMEOUT_SEC: u32 = 30;

    /// Эффективный таймаут (`timeout_sec` или DEFAULT_TIMEOUT_SEC).
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
            "name": "create-role-monitor",
            "dsn": "host=/var/run/postgresql user=postgres",
            "sql": "CREATE ROLE monitor",
        });
        let spec: PgSqlExecSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name.as_str(), "create-role-monitor");
        assert!(spec.if_not_exists_check.is_none());
        assert_eq!(spec.effective_timeout_sec(), 30);
    }

    #[test]
    fn deserialize_with_check_and_timeout() {
        let json = serde_json::json!({
            "name": "x",
            "dsn": "postgres://u@h/d",
            "sql": "GRANT pg_monitor TO monitor",
            "if_not_exists_check": "SELECT 1 FROM pg_roles WHERE rolname='monitor'",
            "timeout_sec": 60,
        });
        let spec: PgSqlExecSpec = serde_json::from_value(json).unwrap();
        assert!(spec.if_not_exists_check.is_some());
        assert_eq!(spec.effective_timeout_sec(), 60);
    }

    #[test]
    fn deserialize_rejects_invalid_unit_name() {
        let json = serde_json::json!({
            "name": "../etc/passwd",
            "dsn": "x",
            "sql": "y",
        });
        let err = serde_json::from_value::<PgSqlExecSpec>(json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must start with") || msg.contains("invalid character"),
            "expected UnitName error, got: {msg}",
        );
    }
}
