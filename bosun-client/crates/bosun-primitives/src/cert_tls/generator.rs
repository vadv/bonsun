//! Pure-Rust генерация self-signed x509-сертификата.
//!
//! Pipeline:
//! 1. `KeyPair` создаётся в rcgen для Ed25519/ECDSA P-256 — ring их умеет.
//! 2. Для RSA 2048 используется `rsa`-крейт (ring сам RSA-ключи не
//!    создаёт), PKCS#8 DER заносится обратно в rcgen.
//! 3. `CertificateParams` заполняется `not_before`/`not_after`, CN, SAN'ами.
//! 4. `params.self_signed(&key_pair)` → PEM-сертификат.
//!
//! I/O в этом модуле нет — функция чистая, всё state'ом тащится через
//! аргументы. Apply вызывает `generate`, потом сам пишет файлы на диск.

use bosun_core::PrimitiveError;
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, SanType, PKCS_ECDSA_P256_SHA256,
    PKCS_ED25519, PKCS_RSA_SHA256,
};
use rsa::pkcs8::EncodePrivateKey;
use rsa::RsaPrivateKey;
use rustls_pki_types::PrivatePkcs8KeyDer;
use time::OffsetDateTime;

use super::spec::{CertAlgorithm, CertTlsSpec};

/// Размер RSA-ключа в битах. chiit-аналог использует 2048.
const RSA_KEY_BITS: usize = 2048;

/// Результат генерации: PEM-сертификат и PEM-приватный ключ.
#[derive(Debug, Clone)]
pub struct GeneratedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Сгенерировать self-signed сертификат под `spec`. `now` приходит снаружи,
/// чтобы тесты могли проверять детерминированную validity без зависимости
/// от системных часов.
pub fn generate(spec: &CertTlsSpec, now: OffsetDateTime) -> Result<GeneratedCert, PrimitiveError> {
    let key_pair = build_key_pair(spec.algorithm)?;

    // CommonName идёт первым в SAN-список, чтобы rcgen всегда добавил его
    // в `dNSName`-расширение. Postgres и nginx с verify-full смотрят
    // именно SAN, а не legacy-CN-fallback.
    let mut sans: Vec<String> = Vec::with_capacity(spec.subject_alt_names.len() + 1);
    sans.push(spec.common_name.clone());
    for san in &spec.subject_alt_names {
        if !sans.iter().any(|s| s == san) {
            sans.push(san.clone());
        }
    }

    let mut params = CertificateParams::new(sans.clone()).map_err(|e| PrimitiveError::Apply {
        reason: format!("rcgen params: {e}"),
    })?;

    params.not_before = now;
    params.not_after = now
        .checked_add(time::Duration::days(i64::from(spec.days_valid)))
        .ok_or_else(|| PrimitiveError::Apply {
            reason: format!(
                "cert.tls: days_valid={} overflows OffsetDateTime",
                spec.days_valid
            ),
        })?;

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, &spec.common_name);
    params.distinguished_name = dn;

    // rcgen::CertificateParams::new пытается распарсить элементы как IP/DNS;
    // при добавлении одинаковых SAN'ов получаем дубликаты. Поэтому
    // переопределяем subject_alt_names руками, явно классифицируя как
    // DnsName — для нашего сценария (CN=hostname) этого достаточно.
    let mut explicit_sans: Vec<SanType> = Vec::with_capacity(sans.len());
    for s in &sans {
        let dns = s.as_str().try_into().map_err(|e| {
            PrimitiveError::InvalidPayload(format!("cert.tls: invalid DNS name '{s}': {e}",))
        })?;
        explicit_sans.push(SanType::DnsName(dns));
    }
    params.subject_alt_names = explicit_sans;

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| PrimitiveError::Apply {
            reason: format!("rcgen self_signed: {e}"),
        })?;
    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    Ok(GeneratedCert { cert_pem, key_pem })
}

/// Собрать KeyPair под выбранный алгоритм. RSA идёт через `rsa`-крейт,
/// остальные — нативно через rcgen+ring.
fn build_key_pair(algorithm: CertAlgorithm) -> Result<KeyPair, PrimitiveError> {
    match algorithm {
        CertAlgorithm::Rsa2048 => generate_rsa_key_pair(),
        CertAlgorithm::Ed25519 => KeyPair::generate_for(&PKCS_ED25519).map_err(map_rcgen_error),
        CertAlgorithm::EcdsaP256 => {
            KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(map_rcgen_error)
        }
    }
}

/// Сгенерировать RSA 2048 через `rsa`-крейт, сериализовать в PKCS#8 DER и
/// импортировать в rcgen. ring сам не генерирует RSA-ключи (только подписи
/// существующими), поэтому без `rsa`-крейта или aws_lc_rs не обойтись;
/// здесь выбран pure-Rust путь.
fn generate_rsa_key_pair() -> Result<KeyPair, PrimitiveError> {
    let mut rng = rand::thread_rng();
    let key = RsaPrivateKey::new(&mut rng, RSA_KEY_BITS).map_err(|e| PrimitiveError::Apply {
        reason: format!("rsa key generation failed: {e}"),
    })?;
    let pkcs8 = key.to_pkcs8_der().map_err(|e| PrimitiveError::Apply {
        reason: format!("rsa pkcs8 encode failed: {e}"),
    })?;
    let der: PrivatePkcs8KeyDer<'static> = pkcs8.as_bytes().to_vec().into();
    KeyPair::from_pkcs8_der_and_sign_algo(&der, &PKCS_RSA_SHA256).map_err(map_rcgen_error)
}

fn map_rcgen_error(e: rcgen::Error) -> PrimitiveError {
    PrimitiveError::Apply {
        reason: format!("rcgen key pair: {e}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use x509_parser::pem::parse_x509_pem;
    use x509_parser::prelude::{FromDer, X509Certificate};

    use super::*;

    fn spec_with(algorithm: CertAlgorithm, sans: Vec<String>) -> CertTlsSpec {
        CertTlsSpec {
            cert_path: PathBuf::from("/tmp/c.crt"),
            key_path: PathBuf::from("/tmp/c.key"),
            common_name: "host.example.com".to_string(),
            algorithm,
            days_valid: 30,
            renew_before_days: 7,
            owner: None,
            group: None,
            mode_cert: 0o644,
            mode_key: 0o600,
            subject_alt_names: sans,
        }
    }

    fn parse_cert(pem: &str) -> (String, i64, i64) {
        let (_, parsed_pem) = parse_x509_pem(pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&parsed_pem.contents).unwrap();
        let cn = cert
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let nb = cert.validity().not_before.timestamp();
        let na = cert.validity().not_after.timestamp();
        (cn, nb, na)
    }

    #[test]
    fn generate_rsa_round_trip() {
        let spec = spec_with(CertAlgorithm::Rsa2048, Vec::new());
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let out = generate(&spec, now).unwrap();

        assert!(out.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(out.cert_pem.contains("END CERTIFICATE"));
        assert!(out.key_pem.contains("BEGIN PRIVATE KEY"));

        let (cn, nb, na) = parse_cert(&out.cert_pem);
        assert_eq!(cn, "host.example.com");
        assert_eq!(nb, 1_700_000_000);
        // days_valid=30 → 30*86400 seconds.
        assert_eq!(na - nb, 30 * 86_400);
    }

    #[test]
    fn generate_ed25519_round_trip() {
        let spec = spec_with(CertAlgorithm::Ed25519, Vec::new());
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let out = generate(&spec, now).unwrap();
        let (cn, _, _) = parse_cert(&out.cert_pem);
        assert_eq!(cn, "host.example.com");
        // Ed25519-ключи короткие — порядка 100 байт base64; границы выбраны
        // широко, чтобы не сломаться при изменениях формата PKCS#8.
        assert!(out.key_pem.len() < 400);
    }

    #[test]
    fn generate_ecdsa_p256_round_trip() {
        let spec = spec_with(CertAlgorithm::EcdsaP256, Vec::new());
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let out = generate(&spec, now).unwrap();
        let (cn, _, _) = parse_cert(&out.cert_pem);
        assert_eq!(cn, "host.example.com");
    }

    #[test]
    fn generate_includes_common_name_as_san() {
        // Postgres ≥ 14 при verify-full смотрит SAN.dNSName, а не CN-fallback.
        // Поэтому CommonName всегда дублируется в SAN — тест ловит регрессию.
        let spec = spec_with(CertAlgorithm::Ed25519, Vec::new());
        let now = OffsetDateTime::now_utc();
        let out = generate(&spec, now).unwrap();
        let (_, parsed_pem) = parse_x509_pem(out.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&parsed_pem.contents).unwrap();
        let extensions = cert.extensions();
        let san = extensions
            .iter()
            .find(|e| e.oid.to_id_string() == "2.5.29.17")
            .expect("SAN extension must be present");
        // value содержит ASN.1; ищем строку CN целиком — простая проверка
        // достаточна, потому что у нас всего один DNS-name.
        let bytes = san.value;
        let mut found = false;
        for window in bytes.windows(b"host.example.com".len()) {
            if window == b"host.example.com" {
                found = true;
                break;
            }
        }
        assert!(found, "SAN must contain common_name");
    }

    #[test]
    fn generate_with_additional_sans() {
        let spec = spec_with(
            CertAlgorithm::Ed25519,
            vec![
                "alias1.example.com".to_string(),
                "alias2.example.com".to_string(),
            ],
        );
        let now = OffsetDateTime::now_utc();
        let out = generate(&spec, now).unwrap();
        let (_, parsed_pem) = parse_x509_pem(out.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&parsed_pem.contents).unwrap();
        let san = cert
            .extensions()
            .iter()
            .find(|e| e.oid.to_id_string() == "2.5.29.17")
            .unwrap();
        let bytes = san.value;
        for name in [
            "host.example.com",
            "alias1.example.com",
            "alias2.example.com",
        ] {
            let mut found = false;
            for window in bytes.windows(name.len()) {
                if window == name.as_bytes() {
                    found = true;
                    break;
                }
            }
            assert!(found, "SAN must contain '{name}'");
        }
    }

    #[test]
    fn generate_deduplicates_common_name_in_sans() {
        // Если bundle уже включает common_name в subject_alt_names, мы не
        // должны добавлять его второй раз — иначе rcgen напишет одинаковый
        // dNSName дважды.
        let spec = spec_with(
            CertAlgorithm::Ed25519,
            vec![
                "host.example.com".to_string(),
                "alias.example.com".to_string(),
            ],
        );
        let now = OffsetDateTime::now_utc();
        let out = generate(&spec, now).unwrap();
        let (_, parsed_pem) = parse_x509_pem(out.cert_pem.as_bytes()).unwrap();
        let (_, cert) = X509Certificate::from_der(&parsed_pem.contents).unwrap();
        let san = cert
            .extensions()
            .iter()
            .find(|e| e.oid.to_id_string() == "2.5.29.17")
            .unwrap();
        // Считаем вхождения CN в SAN-extension.
        let needle = b"host.example.com";
        let mut count = 0;
        let mut i = 0;
        while i + needle.len() <= san.value.len() {
            if &san.value[i..i + needle.len()] == needle {
                count += 1;
                i += needle.len();
            } else {
                i += 1;
            }
        }
        assert_eq!(count, 1, "common_name должен быть в SAN ровно один раз");
    }
}
