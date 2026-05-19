//! Plan-фаза `apt.package` — сравнение spec'а с фактом `installed_packages`.
//!
//! Логика по spec:
//! - `state=Present` (default):
//!   - пакета нет в map → `Add { install <name> [<version>] }`;
//!   - есть и `version is None` → `NoChange`;
//!   - есть и `current_version == spec.version` → `NoChange`;
//!   - есть и `current_version != spec.version` → `Update`.
//! - `state=Absent`:
//!   - есть в map → `Update { remove <name> }`;
//!   - нет в map → `NoChange`.
//! - `state=Purged`:
//!   - есть в map → `Update { purge <name> }`;
//!   - нет в map → `Update { purge <name> (best-effort) }`. Возможно
//!     `config-files`-state, apply узнает через `dpkg-query` и сделает
//!     NoChange при полностью очищенном пакете.
//!
//! Когда `Unknown` или `Stale` — fallback: для `Present` это `Add`, для
//! `Absent`/`Purged` — `Update` (apply дойдёт до dpkg-query или сразу до
//! apt-get remove/purge — обе команды идемпотентны для отсутствующего пакета).

use bosun_core::{Diff, FactValue, FactsSource, PrimitiveError, Resource};

use super::spec::{AptPackageSpec, AptPackageState};

/// Главная функция plan'а. Десериализует payload в `AptPackageSpec`,
/// читает факт `installed_packages` и формирует `Diff`.
pub fn compute_diff(
    resource: &Resource,
    facts: &dyn FactsSource,
    _ctx: &bosun_core::PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: AptPackageSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package payload: {e}")))?;

    match facts.get("installed_packages") {
        FactValue::Known(value) => diff_against_known(&spec, &value, &resource.payload),
        // Stale — те же данные, но устаревшие; spec говорит fallback.
        // Unknown — данных нет. Wildcard ловит будущие варианты (FactValue
        // помечен #[non_exhaustive]) — для них тот же безопасный fallback.
        _ => Ok(unknown_facts_fallback(&spec, &resource.payload)),
    }
}

fn unknown_facts_fallback(spec: &AptPackageSpec, payload: &serde_json::Value) -> Diff {
    match spec.state {
        AptPackageState::Present => Diff::Add {
            description: format!(
                "install {} [{}] (facts unknown, fallback)",
                spec.name,
                spec.version.as_deref().unwrap_or("latest"),
            ),
            payload: payload.clone(),
        },
        AptPackageState::Absent => Diff::Update {
            from: serde_json::json!({"name": spec.name, "state": "unknown"}),
            to: serde_json::json!({"name": spec.name, "state": "absent"}),
            description: format!("remove {} (facts unknown, fallback)", spec.name),
        },
        AptPackageState::Purged => Diff::Update {
            from: serde_json::json!({"name": spec.name, "state": "unknown"}),
            to: serde_json::json!({"name": spec.name, "state": "purged"}),
            description: format!("purge {} (facts unknown, fallback)", spec.name),
        },
    }
}

/// `Known`-ветка: смотрим в map, сравниваем версии и желаемое состояние.
fn diff_against_known(
    spec: &AptPackageSpec,
    value: &serde_json::Value,
    payload: &serde_json::Value,
) -> Result<Diff, PrimitiveError> {
    let map = match value.as_object() {
        Some(m) => m,
        None => {
            // Факт сломан — fallback вместо ошибки. Тот же fallback, что и
            // для Unknown/Stale: ровно одна семантика «факт не пригоден».
            return Ok(unknown_facts_fallback(spec, payload));
        }
    };

    let installed_entry = map.get(&spec.name);

    match spec.state {
        AptPackageState::Present => diff_present(spec, installed_entry, payload),
        AptPackageState::Absent => Ok(diff_absent(spec, installed_entry)),
        AptPackageState::Purged => Ok(diff_purged(spec, installed_entry)),
    }
}

fn diff_present(
    spec: &AptPackageSpec,
    installed_entry: Option<&serde_json::Value>,
    payload: &serde_json::Value,
) -> Result<Diff, PrimitiveError> {
    let Some(entry) = installed_entry else {
        return Ok(Diff::Add {
            description: format!(
                "install {} [{}] (not installed)",
                spec.name,
                spec.version.as_deref().unwrap_or("latest"),
            ),
            payload: payload.clone(),
        });
    };

    let current = entry
        .get("current_version")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match spec.version.as_deref() {
        None => Ok(Diff::NoChange),
        Some(want) if want == current => Ok(Diff::NoChange),
        Some(want) => Ok(Diff::Update {
            from: serde_json::json!({ "name": spec.name, "version": current }),
            to: serde_json::json!({ "name": spec.name, "version": want }),
            description: format!("update {} {current} -> {want}", spec.name),
        }),
    }
}

fn diff_absent(spec: &AptPackageSpec, installed_entry: Option<&serde_json::Value>) -> Diff {
    if installed_entry.is_some() {
        Diff::Update {
            from: serde_json::json!({"name": spec.name, "state": "installed"}),
            to: serde_json::json!({"name": spec.name, "state": "absent"}),
            description: format!("remove {}", spec.name),
        }
    } else {
        Diff::NoChange
    }
}

fn diff_purged(spec: &AptPackageSpec, installed_entry: Option<&serde_json::Value>) -> Diff {
    if installed_entry.is_some() {
        Diff::Update {
            from: serde_json::json!({"name": spec.name, "state": "installed"}),
            to: serde_json::json!({"name": spec.name, "state": "purged"}),
            description: format!("purge {}", spec.name),
        }
    } else {
        // Факт `installed_packages` не показывает `config-files`-state,
        // поэтому plan не может различить «пакет полностью отсутствует» и
        // «остались конфиги». Возвращаем Update (best-effort); apply
        // через `dpkg-query` уточнит и выдаст NoChange если очищать нечего.
        Diff::Update {
            from: serde_json::json!({"name": spec.name, "state": "unknown"}),
            to: serde_json::json!({"name": spec.name, "state": "purged"}),
            description: format!("purge {} (best-effort, may have config-files)", spec.name),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::time::{Duration, Instant};

    use bosun_core::{FactValue, PlanCtx, Resource, ResourceId, ResourceKind};
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct StubFacts {
        value: FactValue,
    }
    impl FactsSource for StubFacts {
        fn get(&self, name: &str) -> FactValue {
            assert_eq!(name, "installed_packages");
            self.value.clone()
        }
    }

    fn ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    fn resource(name: &str, version: Option<&str>) -> Resource {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, name);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "name": name,
                "version": version,
                "timeout_sec": 600_u32,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn known(json: serde_json::Value) -> FactValue {
        FactValue::Known(json)
    }

    #[test]
    fn plan_no_change_when_installed_and_version_not_requested() {
        let r = resource("nginx", None);
        let facts = StubFacts {
            value: known(serde_json::json!({
                "nginx": { "current_version": "1.18.0", "candidate_version": "1.20.1" }
            })),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn plan_no_change_when_installed_and_version_matches() {
        let r = resource("nginx", Some("1.18.0"));
        let facts = StubFacts {
            value: known(serde_json::json!({
                "nginx": { "current_version": "1.18.0", "candidate_version": "1.20.1" }
            })),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn plan_update_when_installed_but_different_version() {
        let r = resource("nginx", Some("1.20.1"));
        let facts = StubFacts {
            value: known(serde_json::json!({
                "nginx": { "current_version": "1.18.0", "candidate_version": "1.20.1" }
            })),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("nginx"));
                assert!(description.contains("1.18.0"));
                assert!(description.contains("1.20.1"));
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn plan_add_when_not_installed() {
        let r = resource("nginx", None);
        let facts = StubFacts {
            value: known(serde_json::json!({})),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Add { description, .. } => assert!(description.contains("nginx")),
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn plan_add_when_not_installed_with_explicit_version() {
        let r = resource("nginx", Some("1.20.1"));
        let facts = StubFacts {
            value: known(serde_json::json!({})),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Add { description, .. } => {
                assert!(description.contains("nginx"));
                assert!(description.contains("1.20.1"));
            }
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn plan_fallback_add_when_facts_unknown() {
        let r = resource("nginx", Some("1.20.1"));
        let facts = StubFacts {
            value: FactValue::Unknown {
                reason: "io error".into(),
            },
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Add { description, .. } => {
                assert!(description.contains("nginx"));
                assert!(description.contains("fallback"));
            }
            other => panic!("expected Add fallback, got {other:?}"),
        }
    }

    #[test]
    fn plan_fallback_add_when_facts_stale() {
        let r = resource("nginx", None);
        let facts = StubFacts {
            value: FactValue::Stale {
                value: serde_json::json!({"nginx": {"current_version": "1.0"}}),
                age_ms: 10_000,
            },
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Add { description, .. } => assert!(description.contains("fallback")),
            other => panic!("expected Add fallback, got {other:?}"),
        }
    }

    #[test]
    fn plan_fallback_add_when_known_is_not_object() {
        let r = resource("nginx", None);
        let facts = StubFacts {
            value: known(serde_json::json!([1, 2, 3])),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Add { description, .. } => assert!(description.contains("fallback")),
            other => panic!("expected Add fallback, got {other:?}"),
        }
    }

    #[test]
    fn plan_invalid_payload_returns_error() {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, "broken");
        let r = Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({ "no_name_here": true }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        };
        let facts = StubFacts {
            value: known(serde_json::json!({})),
        };
        let err = compute_diff(&r, &facts, &ctx()).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("apt.package")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    /// Helper для тестов state=Absent/Purged.
    fn resource_with_state(name: &str, state: &str) -> Resource {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, name);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "name": name,
                "state": state,
                "timeout_sec": 600_u32,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn plan_absent_when_installed_yields_update_remove() {
        let r = resource_with_state("snapd", "absent");
        let facts = StubFacts {
            value: known(serde_json::json!({
                "snapd": {"current_version": "2.55", "candidate_version": "2.55"}
            })),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => assert!(description.contains("remove snapd")),
            other => panic!("expected Update remove, got {other:?}"),
        }
    }

    #[test]
    fn plan_absent_when_not_installed_yields_no_change() {
        let r = resource_with_state("snapd", "absent");
        let facts = StubFacts {
            value: known(serde_json::json!({})),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn plan_purged_when_installed_yields_update_purge() {
        let r = resource_with_state("needrestart", "purged");
        let facts = StubFacts {
            value: known(serde_json::json!({
                "needrestart": {"current_version": "3.5", "candidate_version": "3.5"}
            })),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => assert!(description.contains("purge needrestart")),
            other => panic!("expected Update purge, got {other:?}"),
        }
    }

    #[test]
    fn plan_purged_when_not_installed_yields_best_effort_purge() {
        // Pkg может быть в `config-files`-state — факт это не отражает.
        // Plan возвращает Update с пометкой best-effort; apply через
        // dpkg-query выдаст NoChange если очищать нечего.
        let r = resource_with_state("needrestart", "purged");
        let facts = StubFacts {
            value: known(serde_json::json!({})),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("purge"));
                assert!(description.contains("best-effort"));
            }
            other => panic!("expected Update purge best-effort, got {other:?}"),
        }
    }

    #[test]
    fn plan_default_state_is_present() {
        // Backward-compat: payload без явного state ведёт себя как старый
        // Phase A install.
        let r = resource("nginx", Some("1.18.0"));
        let facts = StubFacts {
            value: known(serde_json::json!({
                "nginx": {"current_version": "1.18.0", "candidate_version": "1.20.1"}
            })),
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn plan_absent_unknown_facts_falls_back_to_update_remove() {
        let r = resource_with_state("snapd", "absent");
        let facts = StubFacts {
            value: FactValue::Unknown {
                reason: "io".into(),
            },
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("remove"));
                assert!(description.contains("fallback"));
            }
            other => panic!("expected Update fallback, got {other:?}"),
        }
    }

    #[test]
    fn plan_purged_unknown_facts_falls_back_to_update_purge() {
        let r = resource_with_state("needrestart", "purged");
        let facts = StubFacts {
            value: FactValue::Unknown {
                reason: "io".into(),
            },
        };
        let diff = compute_diff(&r, &facts, &ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("purge"));
                assert!(description.contains("fallback"));
            }
            other => panic!("expected Update fallback, got {other:?}"),
        }
    }
}
