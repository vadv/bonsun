//! Примитив `apt.key` — управление GPG-ключом репозитория в modern
//! signed-by стиле.
//!
//! Семантика:
//! - `state=Present` со ссылкой `url` (HTTP GET) или с inline `key_data`.
//!   Скачанный/inline-данные опционально проходят через `gpg --dearmor`,
//!   если это ASCII-armored блок, и сохраняются в keyring_path
//!   (`/etc/apt/keyrings/<name>.gpg` по умолчанию) с режимом 0o644.
//! - Опциональный `fingerprint` верифицируется через
//!   `gpg --show-keys --with-fingerprint` после установки; mismatch →
//!   Apply error.
//! - `state=Absent` снимает keyring.
//!
//! chiit-аналог: `lib/apt/apt.go::AddKeyURL` использовал legacy `apt-key
//! add` (deprecated в Debian 11+/Ubuntu 22.04+). Мы намеренно переходим на
//! signed-by стиль: keyring живёт в `/etc/apt/keyrings/`, а `.list`
//! ссылается на него через `signed-by=<path>`.
//!
//! DI: trait `AptKeyBackend` — production использует `RealAptKeyBackend`
//! (ureq + `gpg --dearmor` / `--show-keys`), тесты подменяют mock без
//! HTTP-вызовов и без gpg-бинаря.

mod apply;
mod plan;
mod spec;

use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use apply::{AptKeyBackend, RealAptKeyBackend};
pub use plan::Action;
pub use spec::{AptKeySpec, AptKeyState};

/// Реализация Primitive для `apt.key`.
pub struct AptKeyPrimitive {
    backend: Arc<dyn AptKeyBackend>,
}

impl AptKeyPrimitive {
    /// Конструктор с явным backend'ом.
    pub fn new(backend: Arc<dyn AptKeyBackend>) -> Self {
        Self { backend }
    }

    /// Удобный конструктор для production: `RealAptKeyBackend` внутри Arc.
    pub fn with_real_backend() -> Self {
        Self::new(Arc::new(RealAptKeyBackend))
    }
}

impl Default for AptKeyPrimitive {
    fn default() -> Self {
        Self::with_real_backend()
    }
}

impl Primitive for AptKeyPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("apt.key")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key: {e}")))?;
        let url = args
            .optional_str("url")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key: {e}")))?;
        let key_data = args
            .optional_str("key_data")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key: {e}")))?;
        let fingerprint = args
            .optional_str("fingerprint")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key: {e}")))?;
        let keyring_path = args
            .optional_str("keyring_path")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key: {e}")))?;

        // `state` валидируется при десериализации в plan/apply — здесь
        // только пробрасываем строку, чтобы serde поймал опечатки.

        let mut out = serde_json::Map::new();
        out.insert("name".into(), serde_json::Value::String(name));
        out.insert("state".into(), serde_json::Value::String(state));
        if let Some(u) = url {
            out.insert("url".into(), serde_json::Value::String(u));
        }
        if let Some(d) = key_data {
            out.insert("key_data".into(), serde_json::Value::String(d));
        }
        if let Some(f) = fingerprint {
            out.insert("fingerprint".into(), serde_json::Value::String(f));
        }
        if let Some(p) = keyring_path {
            out.insert("keyring_path".into(), serde_json::Value::String(p));
        }

        Ok(serde_json::Value::Object(out))
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
    fn type_name_is_apt_key() {
        let p = AptKeyPrimitive::with_real_backend();
        assert_eq!(p.type_name(), ResourceKind::from_static("apt.key"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = AptKeyPrimitive::with_real_backend();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_present_with_url() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        args.insert("state".into(), ArgValue::Str("present".into()));
        args.insert(
            "url".into(),
            ArgValue::Str("https://example.com/key.asc".into()),
        );
        let call_args = CallArgs::new(args);
        let p = AptKeyPrimitive::with_real_backend();
        let payload = p.build_payload(&call_args, &plan_ctx()).unwrap();
        assert_eq!(payload["name"], "postgres");
        assert_eq!(payload["state"], "present");
        assert_eq!(payload["url"], "https://example.com/key.asc");
    }

    #[test]
    fn build_payload_absent_minimum() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("oldrepo".into()));
        args.insert("state".into(), ArgValue::Str("absent".into()));
        let call_args = CallArgs::new(args);
        let p = AptKeyPrimitive::with_real_backend();
        let payload = p.build_payload(&call_args, &plan_ctx()).unwrap();
        assert_eq!(payload["state"], "absent");
        assert!(payload.get("url").is_none());
    }

    #[test]
    fn build_payload_missing_state_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("x".into()));
        let call_args = CallArgs::new(args);
        let p = AptKeyPrimitive::with_real_backend();
        let err = p.build_payload(&call_args, &plan_ctx()).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("state")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let call_args = CallArgs::new(HashMap::new());
        let p = AptKeyPrimitive::with_real_backend();
        let err = p.build_payload(&call_args, &plan_ctx()).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
