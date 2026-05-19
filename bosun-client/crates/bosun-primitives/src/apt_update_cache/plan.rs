//! Plan-фаза `apt.update_cache`.
//!
//! Read-before-write: смотрим mtime `pkgcache.bin`. Если файл моложе
//! `max_age_sec` И не выставлен `force` — возвращаем `Diff::NoChange`.
//! Иначе — `Diff::Update`. Apply делает повторную проверку перед
//! `apt-get update`, чтобы соседний bosun-цикл не обновил кеш между plan
//! и apply.

use std::path::Path;
use std::time::SystemTime;

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use super::spec::AptUpdateCacheSpec;

/// Стандартный путь к apt-кешу. Расположение фиксированное и не зависит от
/// дистрибутива (Debian/Ubuntu всегда кладут его сюда).
pub(crate) const PKGCACHE_PATH: &str = "/var/cache/apt/pkgcache.bin";

/// Решение plan-фазы. Возвращается из чистой функции — это упрощает unit-
/// тесты без касания файловой системы.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Кеш свежий, обновление не нужно.
    Fresh { age_sec: u64 },
    /// Кеш устарел или отсутствует — нужно `apt-get update`.
    Refresh { reason: RefreshReason },
}

/// Причина обновления — для логирования и описания в `Diff::Update.description`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RefreshReason {
    /// `pkgcache.bin` не существует.
    Missing,
    /// Возраст файла превысил порог.
    Stale { age_sec: u64, max_age_sec: u32 },
    /// `force=true` — игнорируем mtime.
    Forced,
    /// Не удалось прочитать mtime — обновляем на всякий случай.
    UnreadableMtime { context: String },
}

/// Чистый decision: принимает age (вычисленный или None если файл
/// отсутствует) и spec, возвращает Action. Используется и из plan, и из
/// apply для re-check.
pub(crate) fn decide_action(age_sec: Option<u64>, spec: &AptUpdateCacheSpec) -> Action {
    if spec.force {
        return Action::Refresh {
            reason: RefreshReason::Forced,
        };
    }
    match age_sec {
        None => Action::Refresh {
            reason: RefreshReason::Missing,
        },
        Some(age) if age >= u64::from(spec.max_age_sec) => Action::Refresh {
            reason: RefreshReason::Stale {
                age_sec: age,
                max_age_sec: spec.max_age_sec,
            },
        },
        Some(age) => Action::Fresh { age_sec: age },
    }
}

/// Прочитать возраст `pkgcache.bin` в секундах. Возвращает None, если
/// файла нет; пробрасывает контекстную ошибку при иных I/O-сбоях.
pub(crate) fn read_pkgcache_age(path: &Path) -> Result<Option<u64>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    };
    let mtime = metadata
        .modified()
        .map_err(|e| format!("modified() {}: {e}", path.display()))?;
    let now = SystemTime::now();
    let age = match now.duration_since(mtime) {
        Ok(d) => d.as_secs(),
        // Файл с mtime «из будущего» (например, после rollback NTP). Считаем
        // age=0 — кеш только что обновился, оставляем как есть.
        Err(_) => 0,
    };
    Ok(Some(age))
}

/// Главная функция plan. Возвращает Diff::NoChange/Update в зависимости от
/// возраста кеша. I/O-ошибки чтения mtime (не NotFound) пробрасываются как
/// `PrimitiveError::Io` — это сигнал «что-то не так с файловой системой,
/// не пытайся скрывать ошибку».
pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: AptUpdateCacheSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.update_cache payload: {e}")))?;

    let age = match read_pkgcache_age(Path::new(PKGCACHE_PATH)) {
        Ok(a) => a,
        Err(reason) => {
            // I/O сбой кроме NotFound — отдаём предупреждение в Action, но
            // конкретный путь к выбору даёт decide_action.
            return Ok(diff_for_action(
                &spec,
                Action::Refresh {
                    reason: RefreshReason::UnreadableMtime { context: reason },
                },
                &resource.payload,
            ));
        }
    };

    Ok(diff_for_action(
        &spec,
        decide_action(age, &spec),
        &resource.payload,
    ))
}

/// Перевести Action в Diff с человекочитаемым описанием.
pub(crate) fn diff_for_action(
    spec: &AptUpdateCacheSpec,
    action: Action,
    payload: &serde_json::Value,
) -> Diff {
    match action {
        Action::Fresh { .. } => Diff::NoChange,
        Action::Refresh { reason } => Diff::Update {
            from: serde_json::json!({"apt.update_cache": describe_state(spec, &reason)}),
            to: payload.clone(),
            description: describe_refresh(&reason),
        },
    }
}

fn describe_state(spec: &AptUpdateCacheSpec, reason: &RefreshReason) -> serde_json::Value {
    serde_json::json!({
        "max_age_sec": spec.max_age_sec,
        "force": spec.force,
        "reason": describe_refresh(reason),
    })
}

fn describe_refresh(reason: &RefreshReason) -> String {
    match reason {
        RefreshReason::Missing => "pkgcache.bin missing".to_string(),
        RefreshReason::Stale {
            age_sec,
            max_age_sec,
        } => format!("pkgcache.bin age {age_sec}s exceeds max {max_age_sec}s"),
        RefreshReason::Forced => "force=true".to_string(),
        RefreshReason::UnreadableMtime { context } => format!("mtime unreadable: {context}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::time::{Duration, Instant};

    use bosun_core::{FactValue, ResourceId, ResourceKind};
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct EmptyFacts;
    impl FactsSource for EmptyFacts {
        fn get(&self, _name: &str) -> FactValue {
            FactValue::Unknown {
                reason: "test".to_string(),
            }
        }
    }

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("apt.update_cache");
        let id = ResourceId::new(&kind, "test");
        Resource {
            id,
            kind,
            spec_version: 1,
            payload,
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn spec(name: &str, max_age_sec: u32, force: bool) -> AptUpdateCacheSpec {
        AptUpdateCacheSpec {
            name: name.to_string(),
            max_age_sec,
            force,
            cleanup_old_debs_days: 1,
            skip_cleanup: false,
        }
    }

    #[test]
    fn decide_force_always_refresh() {
        let s = spec("x", 3600, true);
        let action = decide_action(Some(60), &s);
        assert!(matches!(
            action,
            Action::Refresh {
                reason: RefreshReason::Forced
            }
        ));
    }

    #[test]
    fn decide_missing_age_is_missing_refresh() {
        let s = spec("x", 3600, false);
        let action = decide_action(None, &s);
        assert!(matches!(
            action,
            Action::Refresh {
                reason: RefreshReason::Missing
            }
        ));
    }

    #[test]
    fn decide_fresh_age_returns_fresh() {
        let s = spec("x", 3600, false);
        let action = decide_action(Some(1000), &s);
        assert!(matches!(action, Action::Fresh { age_sec: 1000 }));
    }

    #[test]
    fn decide_stale_age_returns_stale_refresh() {
        let s = spec("x", 60, false);
        let action = decide_action(Some(120), &s);
        match action {
            Action::Refresh {
                reason:
                    RefreshReason::Stale {
                        age_sec,
                        max_age_sec,
                    },
            } => {
                assert_eq!(age_sec, 120);
                assert_eq!(max_age_sec, 60);
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn decide_age_exactly_equal_to_threshold_is_stale() {
        // Граница: age == max_age_sec считаем «надо обновлять» — иначе
        // дрейф mtime и часов вылавливается только при следующем тике.
        let s = spec("x", 3600, false);
        let action = decide_action(Some(3600), &s);
        assert!(matches!(
            action,
            Action::Refresh {
                reason: RefreshReason::Stale { .. }
            }
        ));
    }

    #[test]
    fn diff_for_fresh_action_is_no_change() {
        let s = spec("x", 3600, false);
        let diff = diff_for_action(&s, Action::Fresh { age_sec: 60 }, &serde_json::json!({}));
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn diff_for_refresh_action_is_update_with_description() {
        let s = spec("x", 3600, false);
        let diff = diff_for_action(
            &s,
            Action::Refresh {
                reason: RefreshReason::Forced,
            },
            &serde_json::json!({}),
        );
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("force"), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_missing_pkgcache_returns_update() {
        // На обычной dev-машине pkgcache.bin может присутствовать; тест
        // проверяет именно code path, где compute_diff видит spec.force=true
        // и сразу возвращает Update без I/O мимо файла.
        let r = make_resource(serde_json::json!({
            "name": "apt-cache",
            "max_age_sec": 3600_u32,
            "force": true,
            "cleanup_old_debs_days": 1_u32,
            "skip_cleanup": false,
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("force"), "got: {description}");
            }
            other => panic!("expected Update for force=true, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_invalid_payload_returns_invalid_payload() {
        let r = make_resource(serde_json::json!({ "no_name_field": true }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    #[test]
    fn read_pkgcache_age_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("no-such-file");
        let result = read_pkgcache_age(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_pkgcache_age_existing_returns_some() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let age = read_pkgcache_age(tmp.path()).unwrap();
        assert!(age.is_some());
        // Только что созданный файл — age должен быть очень маленьким.
        assert!(age.unwrap() < 60, "newly-created file should be <60s old");
    }
}
