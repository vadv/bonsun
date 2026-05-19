//! Коллектор `pg_initialized` — проверяет, проинициализирован ли data-dir
//! PostgreSQL через наличие `PG_VERSION`-файла под `/etc/postgresql/<ver>/main`.
//!
//! Зачем file-based, а не SQL: до initdb сервер не запустится, и client-side
//! `connect()` упадёт с Connection refused. Result был бы Unknown с reason,
//! который вёл бы автора bundle к ложной диагнозу (сетевая проблема), вместо
//! верной (data-dir ещё не существует). Отдельный file-check даёт чистый
//! сигнал.
//!
//! Пути — стандартные debian/ubuntu расположения: `/etc/postgresql/<ver>/main`.
//! Версии обходим перебором (14..17 + 13 как нижняя граница, актуальная на 2024).
//! `<root_fs>` подменяется в тестах.

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use crate::collector::{Fact, FactCollectCtx};

/// Перечень версий PG, которые мы ищем. Порядок отсортирован по убыванию
/// «свежести» — на узле обычно одна версия, но если случайно стоят несколько
/// (миграция в процессе), отдадим самую новую.
const KNOWN_VERSIONS: &[&str] = &["17", "16", "15", "14", "13"];

pub struct PgInitializedFact;

impl Fact for PgInitializedFact {
    fn name(&self) -> &str {
        "pg_initialized"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Discovery
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AtStart
    }
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue {
        for &version in KNOWN_VERSIONS {
            let p = ctx
                .root_fs
                .join("etc/postgresql")
                .join(version)
                .join("main/PG_VERSION");
            if p.exists() {
                return FactValue::Known(serde_json::json!({
                    "initialized": true,
                    "version": version,
                }));
            }
        }
        FactValue::Known(serde_json::json!({ "initialized": false }))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_pg_version(root: &std::path::Path, version: &str) {
        let dir = root.join("etc/postgresql").join(version).join("main");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("PG_VERSION"), format!("{version}\n")).unwrap();
    }

    #[test]
    fn name_and_policy_are_stable() {
        assert_eq!(PgInitializedFact.name(), "pg_initialized");
        assert_eq!(PgInitializedFact.category(), FactCategory::Discovery);
        assert!(matches!(
            PgInitializedFact.refresh_policy(),
            RefreshPolicy::AtStart
        ));
    }

    #[test]
    fn detects_pg_initialized_for_version_14() {
        let tmp = TempDir::new().unwrap();
        write_pg_version(tmp.path(), "14");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = PgInitializedFact.collect(&ctx);
        let value = v.value().unwrap();
        assert_eq!(value["initialized"], true);
        assert_eq!(value["version"], "14");
    }

    #[test]
    fn detects_pg_initialized_for_version_16() {
        let tmp = TempDir::new().unwrap();
        write_pg_version(tmp.path(), "16");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = PgInitializedFact.collect(&ctx);
        let value = v.value().unwrap();
        assert_eq!(value["initialized"], true);
        assert_eq!(value["version"], "16");
    }

    #[test]
    fn no_pg_version_file_returns_initialized_false() {
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = PgInitializedFact.collect(&ctx);
        let value = v.value().unwrap();
        assert_eq!(value["initialized"], false);
        assert!(value.get("version").is_none());
    }

    #[test]
    fn picks_newest_version_when_multiple_present() {
        let tmp = TempDir::new().unwrap();
        write_pg_version(tmp.path(), "14");
        write_pg_version(tmp.path(), "16");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = PgInitializedFact.collect(&ctx);
        // KNOWN_VERSIONS отсортирован 17 → 13; первая попадающая
        // версия — 16, она и побеждает.
        assert_eq!(v.value().unwrap()["version"], "16");
    }

    #[test]
    fn unknown_version_directory_does_not_count() {
        // Каталог /etc/postgresql/9.5/main/PG_VERSION не попадает в
        // KNOWN_VERSIONS, поэтому не считается за initialized.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("etc/postgresql/9.5/main");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("PG_VERSION"), "9.5\n").unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = PgInitializedFact.collect(&ctx);
        assert_eq!(v.value().unwrap()["initialized"], false);
    }
}
