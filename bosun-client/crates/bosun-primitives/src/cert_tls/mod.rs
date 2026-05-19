//! Примитив `cert.tls` — pure-Rust генерация self-signed x509-сертификата.
//!
//! chiit-аналог (`postgres-chiit/roles/postgres/init_ssl.go`) делает
//! `openssl req -new -x509 ...` через shell. Этот примитив воспроизводит
//! ту же семантику без openssl-binary и без libssl/libcrypto: rcgen
//! поверх ring-бэкенда строит сертификат, RSA-ключи генерирует `rsa`-крейт.
//!
//! Read-before-write: plan только парсит существующий cert через
//! x509-parser и решает Create/Renew/NoChange. Apply вызывает re-plan и
//! пишет файлы только если действие не NoChange — атомарно через
//! tempfile + rename, с chmod (mode_key=0o600 по дефолту) и chown.

mod apply;
mod generator;
mod plan;
mod spec;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};
use chrono::Utc;

pub use plan::{decide_action_cert, Action};
pub use spec::{CertAlgorithm, CertTlsSpec};

/// Реализация Primitive для `cert.tls`. Stateless: вся логика проходит
/// через payload и состояние диска.
pub struct CertTlsPrimitive;

impl Primitive for CertTlsPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("cert.tls")
    }

    fn identity_keys(&self) -> &'static [&'static str] {
        // Идентичность по `cert_path`: пара (cert, key) однозначно
        // привязана к одному cert-пути. Менять key_path без смены
        // cert_path — нонсенс с точки зрения PKI-роли.
        &["cert_path"]
    }

    fn build_payload(
        &self,
        args: &CallArgs,
        _ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError> {
        let cert_path = args
            .required_str("cert_path")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?;
        let key_path = args
            .required_str("key_path")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?;
        let common_name = args
            .required_str("common_name")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?;
        let algorithm = args
            .optional_str("algorithm")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?
            .unwrap_or_else(|| "rsa2048".to_string());
        let days_valid = args
            .optional_u32("days_valid")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?
            .unwrap_or(3650);
        let renew_before_days = args
            .optional_u32("renew_before_days")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?
            .unwrap_or(30);
        let owner = args
            .optional_str("owner")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?;
        let group = args
            .optional_str("group")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?;
        let mode_cert = args
            .optional_u32("mode_cert")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?
            .unwrap_or(0o644);
        let mode_key = args
            .optional_u32("mode_key")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?
            .unwrap_or(0o600);
        let subject_alt_names = args
            .optional_str_list("subject_alt_names")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls: {e}")))?
            .unwrap_or_default();

        Ok(serde_json::json!({
            "cert_path": cert_path,
            "key_path": key_path,
            "common_name": common_name,
            "algorithm": algorithm,
            "days_valid": days_valid,
            "renew_before_days": renew_before_days,
            "owner": owner,
            "group": group,
            "mode_cert": mode_cert,
            "mode_key": mode_key,
            "subject_alt_names": subject_alt_names,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        _facts: &dyn FactsSource,
        _ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        let spec: CertTlsSpec = serde_json::from_value(resource.payload.clone())
            .map_err(|e| PrimitiveError::InvalidPayload(format!("cert.tls payload: {e}")))?;
        spec.validate()?;

        let action = decide_action_cert(&spec, Utc::now())?;
        Ok(match action {
            Action::NoChange => Diff::NoChange,
            Action::Create => Diff::Add {
                description: format!(
                    "create {} and {} (cn={})",
                    spec.cert_path.display(),
                    spec.key_path.display(),
                    spec.common_name,
                ),
                payload: resource.payload.clone(),
            },
            Action::Renew { reason } => Diff::Update {
                from: serde_json::json!({
                    "cert_path": spec.cert_path.to_string_lossy(),
                    "key_path": spec.key_path.to_string_lossy(),
                }),
                to: serde_json::json!({
                    "cert_path": spec.cert_path.to_string_lossy(),
                    "key_path": spec.key_path.to_string_lossy(),
                    "common_name": spec.common_name,
                }),
                description: format!(
                    "renew {} (cn={}): {}",
                    spec.cert_path.display(),
                    spec.common_name,
                    reason,
                ),
            },
        })
    }

    fn apply(
        &self,
        resource: &Resource,
        diff: &Diff,
        ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError> {
        apply::apply(resource, diff, ctx)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use bosun_core::{ArgValue, FactValue, ResourceId};
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct NoFacts;
    impl FactsSource for NoFacts {
        fn get(&self, _: &str) -> FactValue {
            FactValue::Unknown {
                reason: "test".into(),
            }
        }
    }

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    #[test]
    fn type_name_is_cert_tls() {
        assert_eq!(
            CertTlsPrimitive.type_name(),
            ResourceKind::from_static("cert.tls"),
        );
    }

    #[test]
    fn identity_keys_is_cert_path() {
        assert_eq!(CertTlsPrimitive.identity_keys(), &["cert_path"]);
    }

    #[test]
    fn build_payload_with_min_args_applies_defaults() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("cert_path".into(), ArgValue::Str("/etc/ssl/x.crt".into()));
        args.insert("key_path".into(), ArgValue::Str("/etc/ssl/x.key".into()));
        args.insert("common_name".into(), ArgValue::Str("h".into()));
        let call_args = CallArgs::new(args);
        let payload = CertTlsPrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["cert_path"], "/etc/ssl/x.crt");
        assert_eq!(payload["common_name"], "h");
        assert_eq!(payload["algorithm"], "rsa2048");
        assert_eq!(payload["days_valid"], 3650);
        assert_eq!(payload["renew_before_days"], 30);
        assert_eq!(payload["mode_cert"], 0o644);
        assert_eq!(payload["mode_key"], 0o600);
    }

    #[test]
    fn build_payload_missing_cert_path_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("key_path".into(), ArgValue::Str("/x.key".into()));
        args.insert("common_name".into(), ArgValue::Str("h".into()));
        let call_args = CallArgs::new(args);
        let err = CertTlsPrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("cert_path")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_common_name_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("cert_path".into(), ArgValue::Str("/x.crt".into()));
        args.insert("key_path".into(), ArgValue::Str("/x.key".into()));
        let call_args = CallArgs::new(args);
        let err = CertTlsPrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("common_name")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    fn make_resource(cert: &str, key: &str, cn: &str) -> Resource {
        let kind = ResourceKind::from_static("cert.tls");
        let id = ResourceId::new(&kind, cert);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "cert_path": cert,
                "key_path": key,
                "common_name": cn,
                "algorithm": "ed25519",
                "days_valid": 365_u32,
                "renew_before_days": 30_u32,
                "mode_cert": 0o644_u32,
                "mode_key": 0o600_u32,
                "subject_alt_names": Vec::<String>::new(),
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn plan_returns_add_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cert = tmp.path().join("s.crt");
        let key = tmp.path().join("s.key");
        let r = make_resource(cert.to_str().unwrap(), key.to_str().unwrap(), "h");
        let diff = CertTlsPrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::Add { .. }));
    }

    #[test]
    fn plan_returns_no_change_when_cert_is_fresh() {
        // Pre-генерируем cert через тот же spec, что и planner ожидает,
        // потом plan должен сказать NoChange.
        use time::OffsetDateTime;

        let tmp = tempfile::tempdir().unwrap();
        let cert = tmp.path().join("s.crt");
        let key = tmp.path().join("s.key");

        let spec = CertTlsSpec {
            cert_path: cert.clone(),
            key_path: key.clone(),
            common_name: "h".to_string(),
            algorithm: CertAlgorithm::Ed25519,
            days_valid: 365,
            renew_before_days: 30,
            owner: None,
            group: None,
            mode_cert: 0o644,
            mode_key: 0o600,
            subject_alt_names: Vec::new(),
        };
        let now_ot = OffsetDateTime::now_utc();
        let g = generator::generate(&spec, now_ot).unwrap();
        std::fs::write(&cert, g.cert_pem).unwrap();
        std::fs::write(&key, g.key_pem).unwrap();

        let r = make_resource(cert.to_str().unwrap(), key.to_str().unwrap(), "h");
        let diff = CertTlsPrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }
}
