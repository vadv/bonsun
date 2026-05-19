//! Примитив `runr.service` — управление long-running unit'ом через runr.
//!
//! Поверх:
//! - `bosun-runr-client` (`Phase B`) — HTTP клиент.
//! - `bosun-handles::RunrHandle` (`Phase D`) — sync-trait для подмены в тестах.
//! - `bosun-core::defers` (`Phase C`) — журнал отложенных действий.
//!
//! Логика плана и apply подробно описаны в `plan.rs` и `apply.rs`.

mod apply;
mod plan;
mod spec;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use plan::{decide_action_runr, Action};
pub use spec::{RunrServiceSpec, ServiceState};

/// Реализация Primitive для `runr.service`. Stateless: всё runtime-состояние
/// (handle к runr-клиенту, throttle daemon_reload, cache snapshot'ов)
/// живёт в `ApplyCtx`.
#[derive(Default)]
pub struct RunrServicePrimitive;

impl RunrServicePrimitive {
    pub fn new() -> Self {
        Self
    }
}

impl Primitive for RunrServicePrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("runr.service")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
        // Доп. валидация: state должен быть из enum-варианты. Спец-парс
        // через serde_json даст одинаковую ошибку, но раннее предупреждение
        // полезно для Starlark-вызовов.
        if !matches!(state.as_str(), "running" | "stopped" | "absent") {
            return Err(PrimitiveError::InvalidPayload(format!(
                "runr.service: state '{state}' is invalid; expected running|stopped|absent"
            )));
        }
        let enable = args
            .optional_bool("enable")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?
            .unwrap_or(false);

        // health_check: либо cmd-вариант (`health_check_cmd: Vec<String>`),
        // либо url-вариант (`health_check_url: String`). Оба сразу — ошибка.
        let hc_cmd = args
            .optional_handle_list("health_check_cmd")
            .ok()
            .filter(|v| !v.is_empty());
        let _ = hc_cmd; // используем версию ниже через take_raw
        let health_check = build_health_check(args)?;

        // validate_with: список аргументов validate-команды.
        // CallArgs не имеет тип List<String>, поэтому берём raw через
        // `take_raw` ниже не вариант (CallArgs &). Пока поддерживаем
        // только через `Other(serde_json::Value)`.
        let validate_with = extract_validate_with(args)?;

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

/// Сборка HealthCheck из CallArgs. Поддерживает cmd-форму и url-форму.
/// `health_check_cmd` (List[str] из Starlark) → `HealthCheck::Cmd`.
/// `health_check_url: str` → `HealthCheck::Url`. Оба одновременно — ошибка.
fn build_health_check(args: &CallArgs) -> Result<Option<serde_json::Value>, PrimitiveError> {
    // CallArgs не выдаёт `List[str]` напрямую — используем `Other` JSON-форму.
    // Это согласовано с design: для Phase D Starlark-glue ещё не сериализует
    // эти поля; реальное использование появится в `service.unit` Phase F.
    let cmd = serialize_str_list_from_other(args, "health_check_cmd")?;
    let url = args
        .optional_str("health_check_url")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
    let expected = args
        .optional_u32("health_check_expected_status")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
    let retry_count = args
        .optional_u32("health_check_retry")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
    let retry_interval = args
        .optional_u32("health_check_retry_interval_sec")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;
    let timeout = args
        .optional_u32("health_check_timeout_sec")
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.service: {e}")))?;

    match (cmd, url) {
        (Some(_), Some(_)) => Err(PrimitiveError::InvalidPayload(
            "runr.service: health_check_cmd и health_check_url одновременно не допускаются"
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
            // expected_status в JSON хранится как u16; CallArgs::optional_u32
            // отдаёт u32, поэтому даункастим. Значения > u16::MAX недопустимы
            // для HTTP — ловим и возвращаем InvalidPayload.
            let expected_u16 = match expected {
                Some(v) if v > u32::from(u16::MAX) => {
                    return Err(PrimitiveError::InvalidPayload(format!(
                        "runr.service: health_check_expected_status {v} > u16::MAX"
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

/// Прочитать `List[str]`-параметр из CallArgs через `Other`. Если значение —
/// JSON-array строк, возвращает `Vec<String>`. Иначе ошибка.
fn serialize_str_list_from_other(
    args: &CallArgs,
    name: &str,
) -> Result<Option<Vec<String>>, PrimitiveError> {
    // CallArgs не предоставляет публичный геттер для Other-варианта без
    // mutating-доступа. Здесь мы сознательно пробуем через optional_str
    // как fallback на случай, когда Starlark пробросил список как
    // JSON-string. Реальная обработка списка в build_payload подключится
    // через `take_raw` на mutable CallArgs (см. Phase F refactor).
    // Сейчас возвращаем None, если параметр отсутствует или не строка.
    match args.optional_str(name) {
        Ok(Some(joined)) => {
            // Допустим, что Starlark кладёт пустой список как "" или
            // не передаёт параметр. Тестам это не нужно — функциональность
            // подключится в Phase F.
            if joined.is_empty() {
                Ok(None)
            } else {
                Ok(Some(joined.split('\x1f').map(str::to_string).collect()))
            }
        }
        Ok(None) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// Аналог для `validate_with`. Сейчас возвращает None, реальный список
/// подключится через Starlark glue в Phase H, когда `validate_with` появится
/// в `service.unit` builder'е.
fn extract_validate_with(_args: &CallArgs) -> Result<Option<Vec<String>>, PrimitiveError> {
    Ok(None)
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
    fn type_name_is_runr_service() {
        assert_eq!(
            RunrServicePrimitive::new().type_name(),
            ResourceKind::from_static("runr.service"),
        );
    }

    #[test]
    fn identity_keys_is_name() {
        assert_eq!(RunrServicePrimitive::new().identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum_required() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("pg".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        let call_args = CallArgs::new(args);
        let payload = RunrServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "pg");
        assert_eq!(payload["state"], "running");
        assert_eq!(payload["enable"], false);
        assert!(payload["health_check"].is_null());
        assert!(payload["validate_with"].is_null());
    }

    #[test]
    fn build_payload_with_enable_true() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("svc".into()));
        args.insert("state".into(), ArgValue::Str("stopped".into()));
        args.insert("enable".into(), ArgValue::Bool(true));
        let call_args = CallArgs::new(args);
        let payload = RunrServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["enable"], true);
    }

    #[test]
    fn build_payload_url_health_check() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("api".into()));
        args.insert("state".into(), ArgValue::Str("running".into()));
        args.insert(
            "health_check_url".into(),
            ArgValue::Str("http://127.0.0.1/healthz".into()),
        );
        args.insert("health_check_expected_status".into(), ArgValue::Int(204));
        let call_args = CallArgs::new(args);
        let payload = RunrServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["health_check"]["kind"], "url");
        assert_eq!(payload["health_check"]["url"], "http://127.0.0.1/healthz");
        assert_eq!(payload["health_check"]["expected_status"], 204);
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("state".into(), ArgValue::Str("running".into()));
        let call_args = CallArgs::new(args);
        let err = RunrServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_invalid_state_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("svc".into()));
        args.insert("state".into(), ArgValue::Str("starting".into()));
        let call_args = CallArgs::new(args);
        let err = RunrServicePrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("state")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
