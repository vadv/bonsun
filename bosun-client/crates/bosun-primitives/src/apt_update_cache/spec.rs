//! Десериализуемая часть payload'а `apt.update_cache`.
//!
//! Семантика — узкая: «убедись, что apt-кеш свежее, чем `max_age_sec`».
//! План смотрит mtime `/var/cache/apt/pkgcache.bin`. Если файл моложе
//! `max_age_sec` — Diff::NoChange. Иначе apply делает `apt-get update`
//! под dpkg-lock и опционально подчищает старые `.deb` из
//! `/var/cache/apt/archives`.

use serde::Deserialize;

/// Spec примитива `apt.update_cache`.
///
/// `name` нужен для дедупа в реестре ресурсов и для понятных логов
/// (несколько ресурсов `apt.update_cache` с разными `name` в одном bundle
/// — допустимо: например, отдельный «прогрев» и отдельная «ленивая
/// перепроверка», хотя на практике используется один экземпляр).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct AptUpdateCacheSpec {
    /// Имя ресурса для дедупа и логов. На сам apt-cache не влияет —
    /// pkgcache.bin один на ноде.
    pub name: String,
    /// Максимальный возраст `pkgcache.bin` в секундах. Если файл моложе —
    /// `apt-get update` пропускается (lazy). Соответствует chiit-практике
    /// «не дёргать update чаще раза в час».
    #[serde(default = "default_max_age_sec")]
    pub max_age_sec: u32,
    /// Игнорировать `max_age_sec` и всегда выполнять `apt-get update`.
    /// Используется в ситуациях, когда автор bundle хочет гарантированно
    /// подтянуть новый репозиторий сразу после `apt.key` + создания
    /// `.list` файла, не дожидаясь часа.
    #[serde(default)]
    pub force: bool,
    /// Удалять `.deb` файлы из `/var/cache/apt/archives` старше N дней.
    /// Соответствует `find /var/cache/apt -mtime +N -name "*.deb" -delete`
    /// из chiit. По умолчанию 1 день.
    #[serde(default = "default_cleanup_old_debs_days")]
    pub cleanup_old_debs_days: u32,
    /// Пропустить cleanup полностью (для тестов и для нод, где cleanup
    /// делается централизованно вне bosun).
    #[serde(default)]
    pub skip_cleanup: bool,
}

const fn default_max_age_sec() -> u32 {
    3600
}

const fn default_cleanup_old_debs_days() -> u32 {
    1
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum_fills_defaults() {
        let json = serde_json::json!({ "name": "apt-cache" });
        let spec: AptUpdateCacheSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "apt-cache");
        assert_eq!(spec.max_age_sec, 3600);
        assert!(!spec.force);
        assert_eq!(spec.cleanup_old_debs_days, 1);
        assert!(!spec.skip_cleanup);
    }

    #[test]
    fn deserialize_all_fields() {
        let json = serde_json::json!({
            "name": "weekly",
            "max_age_sec": 604_800_u32,
            "force": true,
            "cleanup_old_debs_days": 7_u32,
            "skip_cleanup": true,
        });
        let spec: AptUpdateCacheSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "weekly");
        assert_eq!(spec.max_age_sec, 604_800);
        assert!(spec.force);
        assert_eq!(spec.cleanup_old_debs_days, 7);
        assert!(spec.skip_cleanup);
    }

    #[test]
    fn deserialize_missing_name_is_error() {
        let json = serde_json::json!({ "force": true });
        let err = serde_json::from_value::<AptUpdateCacheSpec>(json).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("name"), "expected 'name' in error: {msg}");
    }
}
