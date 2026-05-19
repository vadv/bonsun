//! Plan-фаза `apt.package` — сравнение spec'а с фактом `installed_packages`.
//!
//! Логика по spec:
//! - `Known(map)`:
//!   - пакета нет в map → `Add { install <name> [<version>] }`;
//!   - есть и `version is None` → `NoChange`;
//!   - есть и `current_version == spec.version` → `NoChange`;
//!   - есть и `current_version != spec.version` → `Update`.
//! - `Unknown` или `Stale` → fallback `Add` с пометкой «facts unknown,
//!   fallback» — apply сам разберётся (через apt-get install идемпотентен
//!   для уже установленного).

use bosun_core::{Diff, FactValue, FactsSource, PrimitiveError, Resource};

use super::spec::AptPackageSpec;

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
        _ => Ok(Diff::Add {
            description: format!(
                "install {} [{}] (facts unknown, fallback)",
                spec.name,
                spec.version.as_deref().unwrap_or("latest"),
            ),
            payload: resource.payload.clone(),
        }),
    }
}

/// `Known`-ветка: смотрим в map, сравниваем версии.
fn diff_against_known(
    spec: &AptPackageSpec,
    value: &serde_json::Value,
    payload: &serde_json::Value,
) -> Result<Diff, PrimitiveError> {
    let map = match value.as_object() {
        Some(m) => m,
        None => {
            // Факт сломан — пишем fallback Add вместо ошибки. Спека требует
            // fallback на любую «непригодность» фактов, и формальный
            // тип-mismatch — частный случай.
            return Ok(Diff::Add {
                description: format!(
                    "install {} [{}] (installed_packages fact malformed, fallback)",
                    spec.name,
                    spec.version.as_deref().unwrap_or("latest"),
                ),
                payload: payload.clone(),
            });
        }
    };

    let Some(entry) = map.get(&spec.name) else {
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
            Diff::Add { description, .. } => assert!(description.contains("malformed")),
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
}
