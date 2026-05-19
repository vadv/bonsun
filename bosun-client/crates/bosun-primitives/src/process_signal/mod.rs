//! Примитив `process.signal` — узкая обёртка над `pkill --signal <SIG>` для
//! отправки allowlist-сигналов процессу по имени или по uid владельца.
//!
//! Сознательное ограничение scope'а: универсальный `command.run` ломает
//! sandbox-гарантии Starlark — автор bundle'а получает escape-hatch к
//! произвольному shell'у. Вместо этого вводится узкий типизированный
//! примитив, который покрывает единственный реально нужный chiit-сценарий
//! `defers.AddCommand(ctx, "hup-pg-doorman", "pkill -HUP pg_doorman")`.
//!
//! Внутри:
//! - `spec` — `ProcessSignalSpec` (имя, сигнал, селектор, deferred).
//! - `plan` — fail-fast валидация селектора/сигнала + Diff::Update.
//! - `apply` — enqueue в defers (default) либо синхронный pkill через
//!   trait `ProcessSignalRunner` (DI для тестов).

mod apply;
mod plan;
mod spec;

use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use apply::{build_signal_argv, ProcessSignalRunner, RealProcessSignalRunner};
pub use spec::ProcessSignalSpec;

/// Реализация Primitive для `process.signal`. Stateless, держит DI-runner
/// в Arc, чтобы тесты подменяли spawn без хака в ApplyCtx.
pub struct ProcessSignalPrimitive {
    runner: Arc<dyn ProcessSignalRunner>,
}

impl ProcessSignalPrimitive {
    /// Конструктор с явным runner'ом. Для production CLI используется
    /// `RealProcessSignalRunner`, для тестов — mock.
    pub fn new(runner: Arc<dyn ProcessSignalRunner>) -> Self {
        Self { runner }
    }

    /// Удобный конструктор для production-CLI: внутри Arc-обёрнут
    /// `RealProcessSignalRunner`.
    pub fn with_real_runner() -> Self {
        Self::new(Arc::new(RealProcessSignalRunner))
    }
}

impl Default for ProcessSignalPrimitive {
    fn default() -> Self {
        Self::with_real_runner()
    }
}

impl Primitive for ProcessSignalPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("process.signal")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("process.signal: {e}")))?;
        let signal = args
            .required_str("signal")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("process.signal: {e}")))?;
        let process_name = args
            .optional_str("process_name")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("process.signal: {e}")))?;
        let process_user = args
            .optional_str("process_user")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("process.signal: {e}")))?;
        // deferred по умолчанию true: в chiit-практике 100% этих вызовов
        // идут через defers.AddCommand.
        let deferred = args
            .optional_bool("deferred")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("process.signal: {e}")))?
            .unwrap_or(true);

        Ok(serde_json::json!({
            "name": name,
            "signal": signal,
            "process_name": process_name,
            "process_user": process_user,
            "deferred": deferred,
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
        apply::run(resource, diff, ctx, &self.runner)
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
    fn type_name_is_process_signal() {
        let p = ProcessSignalPrimitive::with_real_runner();
        assert_eq!(p.type_name(), ResourceKind::from_static("process.signal"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = ProcessSignalPrimitive::with_real_runner();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum_by_name() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("hup-doorman".into()));
        args.insert("signal".into(), ArgValue::Str("HUP".into()));
        args.insert("process_name".into(), ArgValue::Str("pg_doorman".into()));
        let call_args = CallArgs::new(args);
        let payload = ProcessSignalPrimitive::with_real_runner()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "hup-doorman");
        assert_eq!(payload["signal"], "HUP");
        assert_eq!(payload["process_name"], "pg_doorman");
        assert!(payload["process_user"].is_null());
        // По умолчанию — true.
        assert_eq!(payload["deferred"], true);
    }

    #[test]
    fn build_payload_explicit_deferred_false() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("now".into()));
        args.insert("signal".into(), ArgValue::Str("HUP".into()));
        args.insert("process_user".into(), ArgValue::Str("postgres".into()));
        args.insert("deferred".into(), ArgValue::Bool(false));
        let call_args = CallArgs::new(args);
        let payload = ProcessSignalPrimitive::with_real_runner()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["deferred"], false);
        assert_eq!(payload["process_user"], "postgres");
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("signal".into(), ArgValue::Str("HUP".into()));
        let call_args = CallArgs::new(args);
        let err = ProcessSignalPrimitive::with_real_runner()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_signal_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        let call_args = CallArgs::new(args);
        let err = ProcessSignalPrimitive::with_real_runner()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("signal")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
