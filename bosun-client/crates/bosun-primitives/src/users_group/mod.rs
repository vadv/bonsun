//! Примитив `users.group` — декларативное управление системными группами
//! через `groupadd`/`groupmod`/`groupdel`.
//!
//! Идемпотентность: lookup через `getgrnam_r` перед exec'ом. Совпадение
//! spec'а с фактом → `ChangeReport::no_change()`.
//!
//! Безопасность: те же требования, что у `users.user` — root-only,
//! валидация имени, типизированный argv.

pub mod apply;
pub mod backend;
pub mod plan;
mod spec;

use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

use crate::users_user::UsersBackend;

pub use backend::{GroupAddOpts, GroupInfo, GroupModOpts};
pub use plan::{decide_action_group, Action};
pub use spec::{GroupSpec, GroupState};

/// Реализация `Primitive` для `users.group`. Stateless, DI-backend через
/// `Arc<dyn UsersBackend>` (общий trait с `users.user`).
pub struct GroupPrimitive {
    backend: Arc<dyn UsersBackend>,
}

impl GroupPrimitive {
    pub fn new(backend: Arc<dyn UsersBackend>) -> Self {
        Self { backend }
    }

    pub fn with_real_backend() -> Self {
        Self::new(Arc::new(crate::users_user::RealUsersBackend))
    }
}

impl Default for GroupPrimitive {
    fn default() -> Self {
        Self::with_real_backend()
    }
}

impl Primitive for GroupPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("users.group")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.group: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.group: {e}")))?;
        if !matches!(state.as_str(), "present" | "absent") {
            return Err(PrimitiveError::InvalidPayload(format!(
                "users.group: state {state:?} invalid; expected present|absent",
            )));
        }
        let gid = args
            .optional_u32("gid")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.group: {e}")))?;
        let system = args
            .optional_bool("system")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.group: {e}")))?
            .unwrap_or(false);

        Ok(serde_json::json!({
            "name": name,
            "state": state,
            "gid": gid,
            "system": system,
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
        apply::run(resource, diff, ctx, &self.backend)
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
    fn type_name_is_users_group() {
        let p = GroupPrimitive::with_real_backend();
        assert_eq!(p.type_name(), ResourceKind::from_static("users.group"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = GroupPrimitive::with_real_backend();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        args.insert("state".into(), ArgValue::Str("present".into()));
        let call_args = CallArgs::new(args);
        let payload = GroupPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "postgres");
        assert_eq!(payload["state"], "present");
        assert!(payload["gid"].is_null());
        assert_eq!(payload["system"], false);
    }

    #[test]
    fn build_payload_full() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        args.insert("state".into(), ArgValue::Str("present".into()));
        args.insert("gid".into(), ArgValue::Int(5432));
        args.insert("system".into(), ArgValue::Bool(true));
        let call_args = CallArgs::new(args);
        let payload = GroupPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["gid"], 5432);
        assert_eq!(payload["system"], true);
    }

    #[test]
    fn build_payload_unknown_state_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        args.insert("state".into(), ArgValue::Str("nope".into()));
        let call_args = CallArgs::new(args);
        let err = GroupPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }
}
