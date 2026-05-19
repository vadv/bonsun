//! Примитив `systemd.timer` — управление recurring-таймером через native
//! systemd dbus-клиент.
//!
//! Поверх:
//! - `bosun-systemd-client` (Phase A) — sync facade `BlockingSystemdManager`.
//! - `bosun-handles::SystemdHandle` (Phase D + Phase E расширение).
//!
//! Логика проще, чем у `systemd.service`: нет notify-семантики, все
//! действия (enable/disable/start/stop) синхронные.

mod apply;
mod plan;
mod spec;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use plan::{decide_timer_action, TimerAction};
pub use spec::{SystemdTimerSpec, TimerState};

#[derive(Default)]
pub struct SystemdTimerPrimitive;

impl SystemdTimerPrimitive {
    pub fn new() -> Self {
        Self
    }
}

impl Primitive for SystemdTimerPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("systemd.timer")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.timer: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.timer: {e}")))?;
        if !matches!(state.as_str(), "enabled" | "disabled" | "absent") {
            return Err(PrimitiveError::InvalidPayload(format!(
                "systemd.timer: state '{state}' invalid; expected enabled|disabled|absent"
            )));
        }
        // По умолчанию enable=true (см. spec.rs).
        let enable = args
            .optional_bool("enable")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.timer: {e}")))?
            .unwrap_or(true);

        Ok(serde_json::json!({
            "name": name,
            "state": state,
            "enable": enable,
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
        apply::run(resource, diff, ctx)
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
    fn type_name_is_systemd_timer() {
        assert_eq!(
            SystemdTimerPrimitive::new().type_name(),
            ResourceKind::from_static("systemd.timer")
        );
    }

    #[test]
    fn build_payload_minimum_default_enable_true() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("logrotate.timer".into()));
        args.insert("state".into(), ArgValue::Str("enabled".into()));
        let call_args = CallArgs::new(args);
        let payload = SystemdTimerPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "logrotate.timer");
        assert_eq!(payload["state"], "enabled");
        assert_eq!(payload["enable"], true);
    }

    #[test]
    fn build_payload_enable_false_explicit() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x.timer".into()));
        args.insert("state".into(), ArgValue::Str("enabled".into()));
        args.insert("enable".into(), ArgValue::Bool(false));
        let call_args = CallArgs::new(args);
        let payload = SystemdTimerPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["enable"], false);
    }

    #[test]
    fn build_payload_invalid_state() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("v.timer".into()));
        args.insert("state".into(), ArgValue::Str("paused".into()));
        let call_args = CallArgs::new(args);
        let err = SystemdTimerPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("state")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
