//! Примитив `apt.package` — установка/обновление Debian/Ubuntu пакетов через
//! `apt-get install`.
//!
//! Внутри:
//! - `spec` — десериализация payload'а в `AptPackageSpec`.
//! - `plan` — сравнение spec'а с фактом `installed_packages`.
//! - `exec` — `CommandRunner` trait и `RealCommandRunner`, плюс анализ
//!   результата install'а.
//! - `lock_probe` — non-blocking probe `/var/lib/dpkg/lock-frontend`.
//! - `recovery` — `dpkg --configure -a` и `apt-get update` с retry.
//! - `apply` — главная орchestration-логика.

mod apply;
pub mod exec;
mod lock_probe;
mod plan;
pub mod recovery;
mod spec;

use std::path::PathBuf;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use exec::{
    analyze_install_result, CommandResult, CommandRunner, InstallOutcome, RealCommandRunner,
};
pub use spec::AptPackageSpec;

/// Реализация Primitive для `apt.package`.
///
/// Параметризована `CommandRunner` — production использует
/// `RealCommandRunner`, тесты выше уровня unit могут подставить mock.
pub struct AptPrimitive<R: CommandRunner = RealCommandRunner> {
    runner: R,
    /// Путь к dpkg lock-frontend. В Debian/Ubuntu всегда
    /// `/var/lib/dpkg/lock-frontend`; параметр поднят в поле, чтобы тесты
    /// могли подставить tempfile.
    dpkg_lock_path: PathBuf,
}

impl AptPrimitive<RealCommandRunner> {
    /// Default-конструктор для production: реальный runner, стандартный
    /// путь до lock-frontend.
    pub fn new() -> Self {
        Self {
            runner: RealCommandRunner,
            dpkg_lock_path: PathBuf::from("/var/lib/dpkg/lock-frontend"),
        }
    }
}

impl Default for AptPrimitive<RealCommandRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: CommandRunner> AptPrimitive<R> {
    /// Сконструировать с явным runner'ом и lock-path'ом. Используется
    /// BDD-тестами и unit-тестами через mock.
    pub fn with_runner(runner: R, dpkg_lock_path: PathBuf) -> Self {
        Self {
            runner,
            dpkg_lock_path,
        }
    }
}

impl<R: CommandRunner> Primitive for AptPrimitive<R> {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("apt.package")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package: {e}")))?;
        let version = args
            .optional_str("version")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package: {e}")))?;
        let timeout_sec = args
            .optional_u32("timeout_sec")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package: {e}")))?
            .unwrap_or(600);
        // F08: opt-in флаги; по умолчанию false — apt отказывает в
        // downgrade и не трогает hold'ы.
        let allow_downgrade = args
            .optional_bool("allow_downgrade")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package: {e}")))?
            .unwrap_or(false);
        let allow_change_held = args
            .optional_bool("allow_change_held")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package: {e}")))?
            .unwrap_or(false);

        Ok(serde_json::json!({
            "name": name,
            "version": version,
            "timeout_sec": timeout_sec,
            "allow_downgrade": allow_downgrade,
            "allow_change_held": allow_change_held,
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
        apply::run(&self.runner, &self.dpkg_lock_path, resource, diff, ctx)
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
    fn type_name_is_apt_package() {
        assert_eq!(
            AptPrimitive::new().type_name(),
            ResourceKind::from_static("apt.package"),
        );
    }

    #[test]
    fn identity_keys_is_name() {
        assert_eq!(AptPrimitive::new().identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_with_all_args() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx".into()));
        args.insert("version".into(), ArgValue::Str("1.18.0".into()));
        args.insert("timeout_sec".into(), ArgValue::Int(1800));
        let call_args = CallArgs::new(args);
        let payload = AptPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "nginx");
        assert_eq!(payload["version"], "1.18.0");
        assert_eq!(payload["timeout_sec"], 1800);
        // F08: дефолтные значения allow_*-флагов.
        assert_eq!(payload["allow_downgrade"], false);
        assert_eq!(payload["allow_change_held"], false);
    }

    #[test]
    fn build_payload_defaults_timeout_to_600() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("curl".into()));
        let call_args = CallArgs::new(args);
        let payload = AptPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["timeout_sec"], 600);
        assert_eq!(payload["version"], serde_json::Value::Null);
        assert_eq!(payload["allow_downgrade"], false);
        assert_eq!(payload["allow_change_held"], false);
    }

    #[test]
    fn build_payload_accepts_allow_flags() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("nginx".into()));
        args.insert("allow_downgrade".into(), ArgValue::Bool(true));
        args.insert("allow_change_held".into(), ArgValue::Bool(true));
        let call_args = CallArgs::new(args);
        let payload = AptPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["allow_downgrade"], true);
        assert_eq!(payload["allow_change_held"], true);
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let call_args = CallArgs::new(HashMap::new());
        let err = AptPrimitive::new()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
