//! Примитив `sysctl.reload` — применить параметры ядра из `.conf`-файла
//! через `sysctl -p <path>`.
//!
//! Семантика:
//! - plan всегда отдаёт `Diff::Update`: ядро не экспортирует «когда
//!   последний раз грузили этот файл», поэтому идемпотентность на уровне
//!   SCM не достигается без external state. Повторный set того же значения
//!   через `sysctl -p` — no-op на уровне ядра, что и обеспечивает реальную
//!   идемпотентность.
//! - apply проверяет, что `path` существует (типично создан file.content'ом
//!   в том же bundle'е), затем зовёт `sysctl -p <path>`. Несуществующий
//!   путь — `Apply error` без молчаливого фикса.
//!
//! chiit-аналог: роль `sysctl/main.go` писала file.content +
//! Notify-only `command.run "sysctl -p <file>"`. Здесь это разделено на
//! два примитива (file.content + sysctl.reload), что даёт явный
//! observable «после write идёт reload».
//!
//! DI: trait `SysctlBackend` — production использует `RealSysctlBackend`
//! (spawn `sysctl -p`), тесты подменяют mock без касания ядра.

mod apply;
mod plan;
mod spec;

use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use apply::{RealSysctlBackend, SysctlBackend};
pub use spec::SysctlReloadSpec;

/// Реализация Primitive для `sysctl.reload`.
pub struct SysctlReloadPrimitive {
    backend: Arc<dyn SysctlBackend>,
}

impl SysctlReloadPrimitive {
    /// Конструктор с явным backend'ом.
    pub fn new(backend: Arc<dyn SysctlBackend>) -> Self {
        Self { backend }
    }

    /// Удобный конструктор для production: `RealSysctlBackend` внутри Arc.
    pub fn with_real_backend() -> Self {
        Self::new(Arc::new(RealSysctlBackend))
    }
}

impl Default for SysctlReloadPrimitive {
    fn default() -> Self {
        Self::with_real_backend()
    }
}

impl Primitive for SysctlReloadPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("sysctl.reload")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("sysctl.reload: {e}")))?;
        let path = args
            .required_str("path")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("sysctl.reload: {e}")))?;

        Ok(serde_json::json!({
            "name": name,
            "path": path,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        facts: &dyn FactsSource,
        ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        plan::compute_diff(resource, facts, ctx)
    }

    fn apply(
        &self,
        resource: &Resource,
        diff: &Diff,
        ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError> {
        apply::run(self.backend.as_ref(), resource, diff, ctx)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use bosun_core::{ArgValue, PlanCtx};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    #[test]
    fn type_name_is_sysctl_reload() {
        let p = SysctlReloadPrimitive::with_real_backend();
        assert_eq!(p.type_name(), ResourceKind::from_static("sysctl.reload"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = SysctlReloadPrimitive::with_real_backend();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum_required_only() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("bosun-kernel".into()));
        args.insert(
            "path".into(),
            ArgValue::Str("/etc/sysctl.d/60-bosun.conf".into()),
        );
        let call_args = CallArgs::new(args);
        let p = SysctlReloadPrimitive::with_real_backend();
        let payload = p.build_payload(&call_args, &plan_ctx()).unwrap();
        assert_eq!(payload["name"], "bosun-kernel");
        assert_eq!(payload["path"], "/etc/sysctl.d/60-bosun.conf");
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("path".into(), ArgValue::Str("/x".into()));
        let call_args = CallArgs::new(args);
        let p = SysctlReloadPrimitive::with_real_backend();
        let err = p.build_payload(&call_args, &plan_ctx()).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_path_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        let call_args = CallArgs::new(args);
        let p = SysctlReloadPrimitive::with_real_backend();
        let err = p.build_payload(&call_args, &plan_ctx()).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("path")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
