//! Plan-фаза `sysctl.reload`.
//!
//! sysctl-параметры на уровне SCM не имеют read-side кэша (ядро не
//! экспортирует «когда последний раз грузили этот файл»). Поэтому plan
//! всегда возвращает `Diff::Update`: повторный set того же значения через
//! `sysctl -p` — no-op на уровне ядра (одно и то же `kernel.shmmax=X`
//! не меняет состояние), но declarative-уровень bosun должен каждый цикл
//! «подтвердить» применение.
//!
//! Apply делает read-before-write на уровне существования файла: если
//! `path` исчез — это конфигурационная ошибка bundle'а (file.content
//! упал или порядок ресурсов нарушен), apply вернёт `Apply { reason }`,
//! не пытаясь молча восстановить.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use super::spec::SysctlReloadSpec;

/// План: всегда `Update`. Идемпотентность на уровне ядра.
pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: SysctlReloadSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("sysctl.reload payload: {e}")))?;

    Ok(Diff::Update {
        from: serde_json::json!({"sysctl.reload": "stateless"}),
        to: resource.payload.clone(),
        description: format!("sysctl -p {}", spec.path.display()),
    })
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
        let kind = ResourceKind::from_static("sysctl.reload");
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

    #[test]
    fn compute_diff_returns_update_with_path_in_description() {
        let r = make_resource(serde_json::json!({
            "name": "kernel",
            "path": "/etc/sysctl.d/60-bosun.conf",
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("60-bosun.conf"), "got: {description}");
                assert!(description.contains("sysctl -p"), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_invalid_payload_returns_invalid_payload() {
        let r = make_resource(serde_json::json!({ "name": "x" }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }
}
