//! Примитив `runr.timer` — управление recurring-таймером через runr.

mod apply;
mod plan;
mod spec;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use plan::{decide_timer_action, TimerAction};
pub use spec::{RunrTimerSpec, TimerState};

#[derive(Default)]
pub struct RunrTimerPrimitive;

impl RunrTimerPrimitive {
    pub fn new() -> Self {
        Self
    }
}

impl Primitive for RunrTimerPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("runr.timer")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.timer: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.timer: {e}")))?;
        if !matches!(state.as_str(), "enabled" | "disabled" | "absent") {
            return Err(PrimitiveError::InvalidPayload(format!(
                "runr.timer: state '{state}' invalid; expected enabled|disabled|absent"
            )));
        }
        let start_now = args
            .optional_bool("start_now")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.timer: {e}")))?
            .unwrap_or(false);

        Ok(serde_json::json!({
            "name": name,
            "state": state,
            "start_now": start_now,
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
    fn type_name_is_runr_timer() {
        assert_eq!(
            RunrTimerPrimitive::new().type_name(),
            ResourceKind::from_static("runr.timer")
        );
    }

    #[test]
    fn build_payload_default_start_now_false() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("v".into()));
        args.insert("state".into(), ArgValue::Str("enabled".into()));
        let call_args = CallArgs::new(args);
        let payload = RunrTimerPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["start_now"], false);
    }

    #[test]
    fn build_payload_invalid_state() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("v".into()));
        args.insert("state".into(), ArgValue::Str("paused".into()));
        let call_args = CallArgs::new(args);
        let err = RunrTimerPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("state")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
