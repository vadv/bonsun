//! Примитив `systemd.service` — управление long-running unit'ом через
//! native systemd dbus-клиент.
//!
//! Поверх:
//! - `bosun-systemd-client` (Phase A) — async + blocking facade над
//!   `org.freedesktop.systemd1`.
//! - `bosun-handles::SystemdHandle` (Phase D + Phase E расширение) —
//!   sync-trait для подмены в тестах; blanket impl делает sync-обёртку
//!   над `BlockingSystemdManager` с `wait_for_job` внутри.
//! - `bosun-core::defers` (Phase C) — журнал отложенных restart/reload.
//!
//! Логика плана и apply подробно описаны в `plan.rs` и `apply.rs`.

mod apply;
mod plan;
mod spec;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use plan::{decide_action_systemd, Action};
pub use spec::{ServiceState, SystemdServiceSpec};

/// Реализация Primitive для `systemd.service`. Stateless: всё runtime-
/// состояние (handle к systemd-клиенту, throttle daemon_reload) живёт в
/// `ApplyCtx`.
#[derive(Default)]
pub struct SystemdServicePrimitive;

impl SystemdServicePrimitive {
    pub fn new() -> Self {
        Self
    }
}

impl Primitive for SystemdServicePrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("systemd.service")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
        if !matches!(state.as_str(), "running" | "stopped" | "absent") {
            return Err(PrimitiveError::InvalidPayload(format!(
                "systemd.service: state '{state}' is invalid; expected running|stopped|absent"
            )));
        }
        // По умолчанию enable=true. Это отличие от runr.service (там default
        // false) — соответствует ожиданию systemd-операторов.
        let enable = args
            .optional_bool("enable")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?
            .unwrap_or(true);

        let health_check = build_health_check(args)?;
        // validate_with — pre-swap валидатор, list[str] из Starlark. Glue
        // упаковывает список в `ArgValue::Other(Array)`. Пустой массив
        // равноценен «не задан».
        let validate_with = args
            .optional_str_list("validate_with")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?
            .filter(|v| !v.is_empty());

        Ok(serde_json::json!({
            "name": name,
            "state": state,
            "enable": enable,
            "health_check": health_check,
            "validate_with": validate_with,
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

/// HealthCheck builder. Симметричен runr_service::build_health_check.
/// `health_check_cmd` — list[str], glue упаковывает в `ArgValue::Other(Array)`,
/// `optional_str_list` распаковывает обратно. Пустой массив игнорируем.
fn build_health_check(args: &CallArgs) -> Result<Option<serde_json::Value>, PrimitiveError> {
    let cmd = args
        .optional_str_list("health_check_cmd")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?
        .filter(|v| !v.is_empty());
    let url = args
        .optional_str("health_check_url")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
    let expected = args
        .optional_u32("health_check_expected_status")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
    let retry_count = args
        .optional_u32("health_check_retry")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
    let retry_interval = args
        .optional_u32("health_check_retry_interval_sec")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;
    let timeout = args
        .optional_u32("health_check_timeout_sec")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("systemd.service: {e}")))?;

    match (cmd, url) {
        (Some(_), Some(_)) => Err(PrimitiveError::InvalidPayload(
            "systemd.service: health_check_cmd и health_check_url одновременно не допускаются"
                .to_string(),
        )),
        (Some(cmd_vec), None) => Ok(Some(serde_json::json!({
            "kind": "cmd",
            "cmd": cmd_vec,
            "timeout_sec": timeout,
            "retry_count": retry_count,
            "retry_interval_sec": retry_interval,
        }))),
        (None, Some(url_value)) => {
            let expected_u16 = match expected {
                Some(v) if v > u32::from(u16::MAX) => {
                    return Err(PrimitiveError::InvalidPayload(format!(
                        "systemd.service: health_check_expected_status {v} > u16::MAX"
                    )))
                }
                Some(v) => Some(v as u16),
                None => None,
            };
            Ok(Some(serde_json::json!({
                "kind": "url",
                "url": url_value,
                "expected_status": expected_u16,
                "timeout_sec": timeout,
                "retry_count": retry_count,
                "retry_interval_sec": retry_interval,
            })))
        }
        (None, None) => Ok(None),
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
    fn type_name_is_systemd_service() {
        assert_eq!(
            SystemdServicePrimitive::new().type_name(),
            ResourceKind::from_static("systemd.service"),
        );
    }

    #[test]
    fn identity_keys_is_name() {
        assert_eq!(SystemdServicePrimitive::new().identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum_required_default_enable_true() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        let call_args = CallArgs::new(args);
        let payload = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "nginx.service");
        assert_eq!(payload["state"], "running");
        // Это отличие от runr.service.
        assert_eq!(payload["enable"], true);
    }

    #[test]
    fn build_payload_enable_false_explicit() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        args.insert("enable".into(), ArgValue::Bool(false));
        let call_args = CallArgs::new(args);
        let payload = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["enable"], false);
    }

    #[test]
    fn build_payload_invalid_state_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("reloading".into()));
        let call_args = CallArgs::new(args);
        let err = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("state")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("state".into(), ArgValue::Str("running".into()));
        let call_args = CallArgs::new(args);
        let err = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_url_health_check() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        args.insert(
            "health_check_url".into(),
            ArgValue::Str("http://127.0.0.1/healthz".into()),
        );
        args.insert("health_check_expected_status".into(), ArgValue::Int(204));
        let call_args = CallArgs::new(args);
        let payload = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["health_check"]["kind"], "url");
        assert_eq!(payload["health_check"]["url"], "http://127.0.0.1/healthz");
        assert_eq!(payload["health_check"]["expected_status"], 204);
    }

    /// Раньше health_check_cmd как list[str] тихо игнорировался — теперь
    /// должен распаковываться в `HealthCheck::Cmd { cmd: [..] }`.
    #[test]
    fn build_payload_parses_health_check_cmd_list() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        args.insert(
            "health_check_cmd".into(),
            ArgValue::Other(serde_json::json!(["curl", "-fsS", "http://127.0.0.1/"])),
        );
        let call_args = CallArgs::new(args);
        let payload = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["health_check"]["kind"], "cmd");
        assert_eq!(
            payload["health_check"]["cmd"],
            serde_json::json!(["curl", "-fsS", "http://127.0.0.1/"]),
        );
    }

    /// validate_with — pre-swap валидатор для restart/reload defer.
    /// Регрессия: service.unit(validate_with=["nginx","-t"]).
    #[test]
    fn build_payload_parses_validate_with_list() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        args.insert(
            "validate_with".into(),
            ArgValue::Other(serde_json::json!(["nginx", "-t", "-c", "{new_path}"])),
        );
        let call_args = CallArgs::new(args);
        let payload = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(
            payload["validate_with"],
            serde_json::json!(["nginx", "-t", "-c", "{new_path}"]),
        );
    }

    #[test]
    fn build_payload_empty_health_check_cmd_is_absent() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        args.insert(
            "health_check_cmd".into(),
            ArgValue::Other(serde_json::json!([])),
        );
        args.insert(
            "validate_with".into(),
            ArgValue::Other(serde_json::json!([])),
        );
        let call_args = CallArgs::new(args);
        let payload = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert!(payload["health_check"].is_null());
        assert!(payload["validate_with"].is_null());
    }

    /// health_check_cmd и health_check_url вместе — InvalidPayload, ловим,
    /// что новая реализация cmd-парсера не сломала эту проверку.
    #[test]
    fn build_payload_both_cmd_and_url_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx.service".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        args.insert(
            "health_check_cmd".into(),
            ArgValue::Other(serde_json::json!(["curl"])),
        );
        args.insert("health_check_url".into(), ArgValue::Str("http://x/".into()));
        let call_args = CallArgs::new(args);
        let err = SystemdServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(msg.contains("одновременно не допускаются"), "got: {msg}");
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
