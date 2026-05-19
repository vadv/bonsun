//! Десериализуемая часть payload'а `cert.tls`.
//!
//! По умолчанию воспроизводим chiit-аналог (`init_ssl.go`):
//! RSA 2048, 10 лет (3650 дней) validity, renew когда до expiry осталось
//! меньше 30 дней. Permissions: 0o644 для cert, 0o600 для key — приватный
//! ключ читается только владельцем.

use std::path::{Component, Path, PathBuf};

use bosun_core::PrimitiveError;
use serde::Deserialize;

/// Алгоритм ключа сертификата. RSA 2048 — chiit-совместимый дефолт.
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CertAlgorithm {
    /// RSA с длиной ключа 2048 бит и подписью SHA-256.
    #[default]
    Rsa2048,
    /// Ed25519 — современный быстрый алгоритм, поддерживается postgres ≥ 15.
    Ed25519,
    /// ECDSA на P-256. На будущее — на случай интеграций, где RSA нежелателен.
    EcdsaP256,
}

/// Спека `cert.tls`, как она лежит в `Resource.payload`.
#[derive(Deserialize, Debug, Clone)]
pub struct CertTlsSpec {
    /// Куда положить `.crt` (PEM). Должен быть абсолютным.
    pub cert_path: PathBuf,
    /// Куда положить `.key` (PEM, PKCS#8). Должен быть абсолютным.
    pub key_path: PathBuf,
    /// CommonName в Subject DN. Обычно равен hostname — postgres сверяет
    /// его при `verify-full`, поэтому опечатка тут видна сразу клиенту.
    pub common_name: String,
    /// Алгоритм ключа. `Default = Rsa2048` — chiit-аналог.
    #[serde(default)]
    pub algorithm: CertAlgorithm,
    /// Срок действия в днях. Default 3650 (10 лет), как в chiit.
    #[serde(default = "default_days_valid")]
    pub days_valid: u32,
    /// За сколько дней до expiry начинать renew. Default 30.
    #[serde(default = "default_renew_before_days")]
    pub renew_before_days: u32,
    /// Owner для chown. None → процесс (обычно root). chiit использует
    /// `postgres:postgres` — это и кладёт сюда оператор bundle'а.
    #[serde(default)]
    pub owner: Option<String>,
    /// Группа для chown. None → primary group процесса.
    #[serde(default)]
    pub group: Option<String>,
    /// Permissions для cert-файла. Default 0o644 — публичный сертификат.
    #[serde(default = "default_mode_cert")]
    pub mode_cert: u32,
    /// Permissions для key-файла. Default 0o600 — приватный ключ читает
    /// только owner. Менять можно, но это плохая идея.
    #[serde(default = "default_mode_key")]
    pub mode_key: u32,
    /// Дополнительные SubjectAlternativeName-записи. CommonName всегда
    /// идёт первым, эти добавляются после.
    #[serde(default)]
    pub subject_alt_names: Vec<String>,
}

const fn default_days_valid() -> u32 {
    3650
}

const fn default_renew_before_days() -> u32 {
    30
}

const fn default_mode_cert() -> u32 {
    0o644
}

const fn default_mode_key() -> u32 {
    0o600
}

impl CertTlsSpec {
    /// Проверить, что пути абсолютные, без `..`-сегментов и NUL-байт,
    /// `common_name` непустой, `days_valid` > 0 и больше `renew_before_days`
    /// (иначе сертификат сразу после генерации просился бы на renew).
    pub fn validate(&self) -> Result<(), PrimitiveError> {
        validate_path(&self.cert_path, "cert_path")?;
        validate_path(&self.key_path, "key_path")?;
        if self.cert_path == self.key_path {
            return Err(PrimitiveError::InvalidPayload(
                "cert.tls: cert_path and key_path must differ".to_string(),
            ));
        }
        if self.common_name.is_empty() {
            return Err(PrimitiveError::InvalidPayload(
                "cert.tls: common_name must not be empty".to_string(),
            ));
        }
        if self.common_name.contains('\0') {
            return Err(PrimitiveError::InvalidPayload(
                "cert.tls: common_name contains NUL byte".to_string(),
            ));
        }
        if self.days_valid == 0 {
            return Err(PrimitiveError::InvalidPayload(
                "cert.tls: days_valid must be > 0".to_string(),
            ));
        }
        if u64::from(self.renew_before_days) >= u64::from(self.days_valid) {
            return Err(PrimitiveError::InvalidPayload(format!(
                "cert.tls: renew_before_days ({}) must be < days_valid ({})",
                self.renew_before_days, self.days_valid,
            )));
        }
        for san in &self.subject_alt_names {
            if san.is_empty() {
                return Err(PrimitiveError::InvalidPayload(
                    "cert.tls: subject_alt_names contains empty string".to_string(),
                ));
            }
            if san.contains('\0') {
                return Err(PrimitiveError::InvalidPayload(
                    "cert.tls: subject_alt_names entry contains NUL byte".to_string(),
                ));
            }
        }
        Ok(())
    }
}

fn validate_path(p: &Path, field: &str) -> Result<(), PrimitiveError> {
    let s = p.to_string_lossy();
    if s.contains('\0') {
        return Err(PrimitiveError::InvalidPayload(format!(
            "cert.tls.{field} contains NUL byte",
        )));
    }
    if !p.is_absolute() {
        return Err(PrimitiveError::InvalidPayload(format!(
            "cert.tls.{field} must be absolute, got: {s}",
        )));
    }
    for component in p.components() {
        if matches!(component, Component::ParentDir) {
            return Err(PrimitiveError::InvalidPayload(format!(
                "cert.tls.{field} contains '..' segment: {s}",
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn min_spec() -> CertTlsSpec {
        CertTlsSpec {
            cert_path: PathBuf::from("/etc/ssl/server.crt"),
            key_path: PathBuf::from("/etc/ssl/server.key"),
            common_name: "host.example.com".to_string(),
            algorithm: CertAlgorithm::Rsa2048,
            days_valid: 3650,
            renew_before_days: 30,
            owner: None,
            group: None,
            mode_cert: 0o644,
            mode_key: 0o600,
            subject_alt_names: Vec::new(),
        }
    }

    #[test]
    fn deserialize_minimal_payload_applies_defaults() {
        let json = serde_json::json!({
            "cert_path": "/etc/ssl/server.crt",
            "key_path": "/etc/ssl/server.key",
            "common_name": "host.example.com",
        });
        let spec: CertTlsSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.algorithm, CertAlgorithm::Rsa2048);
        assert_eq!(spec.days_valid, 3650);
        assert_eq!(spec.renew_before_days, 30);
        assert_eq!(spec.mode_cert, 0o644);
        assert_eq!(spec.mode_key, 0o600);
        assert!(spec.owner.is_none());
        assert!(spec.subject_alt_names.is_empty());
    }

    #[test]
    fn deserialize_full_payload() {
        let json = serde_json::json!({
            "cert_path": "/var/lib/pg/server.crt",
            "key_path": "/var/lib/pg/server.key",
            "common_name": "pg.test",
            "algorithm": "ed25519",
            "days_valid": 365_u32,
            "renew_before_days": 7_u32,
            "owner": "postgres",
            "group": "postgres",
            "mode_cert": 0o640_u32,
            "mode_key": 0o600_u32,
            "subject_alt_names": ["pg.alias", "10.0.0.1"],
        });
        let spec: CertTlsSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.algorithm, CertAlgorithm::Ed25519);
        assert_eq!(spec.days_valid, 365);
        assert_eq!(spec.renew_before_days, 7);
        assert_eq!(spec.owner.as_deref(), Some("postgres"));
        assert_eq!(spec.mode_cert, 0o640);
        assert_eq!(spec.subject_alt_names.len(), 2);
    }

    #[test]
    fn deserialize_ecdsa_p256_variant() {
        let json = serde_json::json!({
            "cert_path": "/x.crt",
            "key_path": "/x.key",
            "common_name": "h",
            "algorithm": "ecdsa_p256",
        });
        let spec: CertTlsSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.algorithm, CertAlgorithm::EcdsaP256);
    }

    #[test]
    fn validate_accepts_min_spec() {
        min_spec().validate().unwrap();
    }

    #[test]
    fn validate_rejects_relative_cert_path() {
        let mut spec = min_spec();
        spec.cert_path = PathBuf::from("server.crt");
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(msg.contains("absolute"));
                assert!(msg.contains("cert_path"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_parent_dir_in_key_path() {
        let mut spec = min_spec();
        spec.key_path = PathBuf::from("/etc/ssl/../etc/shadow");
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("'..'")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_same_path_for_cert_and_key() {
        let mut spec = min_spec();
        spec.key_path = spec.cert_path.clone();
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("differ")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_empty_common_name() {
        let mut spec = min_spec();
        spec.common_name = String::new();
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("common_name")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_zero_days_valid() {
        let mut spec = min_spec();
        spec.days_valid = 0;
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("days_valid")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_renew_before_days_greater_than_validity() {
        let mut spec = min_spec();
        spec.days_valid = 10;
        spec.renew_before_days = 30;
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("renew_before_days")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_empty_san_entry() {
        let mut spec = min_spec();
        spec.subject_alt_names = vec!["valid".into(), String::new()];
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("subject_alt_names")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_nul_byte_in_common_name() {
        let mut spec = min_spec();
        spec.common_name = "ho\0st".into();
        let err = spec.validate().unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("NUL")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
