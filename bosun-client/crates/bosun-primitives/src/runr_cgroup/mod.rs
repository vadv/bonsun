//! Примитив `runr.cgroup` — декларация cgroup-юнита.
//!
//! Реальная работа (создание `/etc/runr/<name>.cgroup`, `daemon_reload`)
//! делается соседними примитивами:
//! - `file.content` пишет содержимое cgroup-юнита.
//! - `runr.service` / общий apply делает `daemon_reload` через ApplyCtx
//!   throttle.
//!
//! Сам `runr.cgroup` существует как separate registry entry, чтобы:
//! 1. Иметь стабильный `ResourceId` для notify-связей (`reload_on=cgroup`).
//! 2. Позволить будущему расширению (verify cgroup actually exists в
//!    `/sys/fs/cgroup` — Phase L и далее).
//!
//! В Phase D apply возвращает `no_change` всегда. Это сознательное упрощение.

use serde::Deserialize;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

/// Желаемое состояние cgroup-юнита.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CgroupState {
    /// Юнит должен быть зарегистрирован в runr.
    Present,
    /// Юнит должен быть удалён.
    Absent,
}

/// Спека `runr.cgroup`.
#[derive(Clone, Debug, Deserialize)]
pub struct RunrCgroupSpec {
    pub name: String,
    pub state: CgroupState,
}

#[derive(Default)]
pub struct RunrCgroupPrimitive;

impl RunrCgroupPrimitive {
    pub fn new() -> Self {
        Self
    }
}

impl Primitive for RunrCgroupPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("runr.cgroup")
    }

    fn identity_keys(&self) -> &'static [&'static str] {
        &["name"]
    }

    fn build_payload(
        &self,
        args: &CallArgs,
        _ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError> {
        let name = args
            .required_str("name")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.cgroup: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.cgroup: {e}")))?;
        if !matches!(state.as_str(), "present" | "absent") {
            return Err(PrimitiveError::InvalidPayload(format!(
                "runr.cgroup: state '{state}' invalid; expected present|absent"
            )));
        }
        Ok(serde_json::json!({ "name": name, "state": state }))
    }

    fn plan(
        &self,
        resource: &Resource,
        _facts: &dyn FactsSource,
        _ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        // Десериализация для валидации; результат игнорируется.
        let _spec: RunrCgroupSpec = serde_json::from_value(resource.payload.clone())
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.cgroup payload: {e}")))?;
        // Declarative-only: реальные работы делаются соседями. Возвращаем
        // NoChange — это безопасный default, plan не описывает фактического
        // changeset'а.
        Ok(Diff::NoChange)
    }

    fn apply(
        &self,
        _resource: &Resource,
        _diff: &Diff,
        _ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError> {
        Ok(ChangeReport::no_change())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtx, ArgValue, PlanCtx, ResourceId, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    fn apply_ctx(defers: Arc<Journal>) -> ApplyCtx {
        ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            defers,
            None,
            None,
        )
    }

    #[test]
    fn type_name_is_runr_cgroup() {
        assert_eq!(
            RunrCgroupPrimitive::new().type_name(),
            ResourceKind::from_static("runr.cgroup")
        );
    }

    #[test]
    fn build_payload_minimum() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("pg".into()));
        args.insert("state".into(), ArgValue::Str("present".into()));
        let call_args = CallArgs::new(args);
        let payload = RunrCgroupPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "pg");
        assert_eq!(payload["state"], "present");
    }

    #[test]
    fn invalid_state_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        args.insert("state".into(), ArgValue::Str("unknown".into()));
        let call_args = CallArgs::new(args);
        let err = RunrCgroupPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    #[test]
    fn plan_always_returns_no_change() {
        let kind = ResourceKind::from_static("runr.cgroup");
        let id = ResourceId::new(&kind, "pg");
        let resource = Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({"name": "pg", "state": "present"}),
            reload_on: vec![],
            restart_on: vec![],
            depends_on: vec![],
        };
        struct NoFacts;
        impl FactsSource for NoFacts {
            fn get(&self, _: &str) -> bosun_core::FactValue {
                bosun_core::FactValue::Unknown {
                    reason: "n/a".into(),
                }
            }
        }
        let diff = RunrCgroupPrimitive::new()
            .plan(&resource, &NoFacts, &plan_ctx())
            .unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn apply_always_returns_no_change() {
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = apply_ctx(defers);
        let kind = ResourceKind::from_static("runr.cgroup");
        let id = ResourceId::new(&kind, "pg");
        let resource = Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({"name": "pg", "state": "present"}),
            reload_on: vec![],
            restart_on: vec![],
            depends_on: vec![],
        };
        let report = RunrCgroupPrimitive::new()
            .apply(&resource, &Diff::NoChange, &ctx)
            .unwrap();
        assert!(!report.changed);
        assert!(!report.deferred);
    }
}
