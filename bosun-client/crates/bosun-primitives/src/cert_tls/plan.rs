//! Plan-фаза `cert.tls` — чистая decide-функция от пары (`spec`, состояние
//! файлов на диске) к [`Action`].
//!
//! Read-before-write принцип: plan ничего не пишет. Парсинг существующего
//! сертификата происходит здесь, а не в apply, чтобы dry-run (`bosun plan`)
//! не открывал и не модифицировал ни один файл.

use bosun_core::PrimitiveError;
use chrono::{DateTime, Utc};
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::{FromDer, X509Certificate};

use super::spec::CertTlsSpec;

/// Решение plan'а. `Renew` несёт причину, чтобы оператор в логе видел,
/// почему файлы переписываются.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Cert/key совпадают с желаемым: CN тот же, expiry достаточно далеко.
    NoChange,
    /// Хотя бы один файл отсутствует — нужно сгенерировать заново.
    Create,
    /// Файлы есть, но содержимое не подходит. `reason` — короткая
    /// человекочитаемая причина для лога.
    Renew { reason: String },
}

/// Принять решение по парe (spec, состояние на диске, текущее время).
///
/// `now` приходит снаружи: тесты подставляют детерминированное время,
/// production — `Utc::now()` через PlanCtx.
///
/// Read-only: вызывает `std::fs::read` и `std::fs::metadata`, но не пишет.
pub fn decide_action_cert(
    spec: &CertTlsSpec,
    now: DateTime<Utc>,
) -> Result<Action, PrimitiveError> {
    let cert_path = spec.cert_path.as_path();
    let key_path = spec.key_path.as_path();

    let cert_meta = match std::fs::symlink_metadata(cert_path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(PrimitiveError::Io {
                context: format!("symlink_metadata {}", cert_path.display()),
                source: e,
            });
        }
    };
    let key_meta = match std::fs::symlink_metadata(key_path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(PrimitiveError::Io {
                context: format!("symlink_metadata {}", key_path.display()),
                source: e,
            });
        }
    };

    let (Some(cert_meta), Some(key_meta)) = (cert_meta, key_meta) else {
        // Хотя бы одного файла нет — пара заведомо рассинхронизирована,
        // даже если второй файл валиден. Генерируем оба, иначе оператор
        // получит cert от одного KeyPair и key от другого.
        return Ok(Action::Create);
    };

    // Симлинки и не-регулярные файлы недопустимы: подмена target'а
    // приватного ключа симлинком на `/etc/shadow` приведёт к записи
    // в чужой файл при apply. Отказываем сразу.
    if cert_meta.file_type().is_symlink() || !cert_meta.file_type().is_file() {
        return Err(PrimitiveError::InvalidTarget);
    }
    if key_meta.file_type().is_symlink() || !key_meta.file_type().is_file() {
        return Err(PrimitiveError::InvalidTarget);
    }

    let cert_bytes = std::fs::read(cert_path).map_err(|e| PrimitiveError::Io {
        context: format!("read {} for plan", cert_path.display()),
        source: e,
    })?;

    let parsed = match parse_cert(&cert_bytes) {
        Ok(p) => p,
        Err(reason) => {
            // Не можем разобрать → не можем доверять. Renew, чтобы выйти
            // из неконсистентного состояния за один apply.
            return Ok(Action::Renew { reason });
        }
    };

    if parsed.common_name != spec.common_name {
        return Ok(Action::Renew {
            reason: format!(
                "common_name drift: cert has '{}', spec wants '{}'",
                parsed.common_name, spec.common_name,
            ),
        });
    }

    let renew_threshold_secs = i64::from(spec.renew_before_days) * 86_400;
    let seconds_remaining = parsed.not_after_unix - now.timestamp();
    if seconds_remaining < renew_threshold_secs {
        let days_remaining = seconds_remaining / 86_400;
        return Ok(Action::Renew {
            reason: format!(
                "expiry near: {} days remaining (< renew_before_days={})",
                days_remaining, spec.renew_before_days,
            ),
        });
    }

    Ok(Action::NoChange)
}

/// Распарсенное состояние существующего .crt — то, что plan'у нужно для
/// сравнения.
struct ParsedCert {
    common_name: String,
    not_after_unix: i64,
}

/// Достать CN и `not_after` из PEM-байтов. На любую ошибку парсинга
/// возвращаем `Err(reason)` — вызывающая сторона решит, как реагировать
/// (обычно `Action::Renew`).
fn parse_cert(pem_bytes: &[u8]) -> Result<ParsedCert, String> {
    let (_, pem) = parse_x509_pem(pem_bytes).map_err(|e| format!("PEM decode: {e}"))?;
    if pem.label != "CERTIFICATE" {
        return Err(format!("unexpected PEM label '{}'", pem.label));
    }
    let (_, cert) =
        X509Certificate::from_der(&pem.contents).map_err(|e| format!("DER parse: {e}"))?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .ok_or_else(|| "subject has no CommonName".to_string())?
        .as_str()
        .map_err(|e| format!("CN encoding: {e}"))?
        .to_string();
    let not_after_unix = cert.validity().not_after.timestamp();
    Ok(ParsedCert {
        common_name: cn,
        not_after_unix,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use time::OffsetDateTime;

    use super::super::generator::generate;
    use super::super::spec::{CertAlgorithm, CertTlsSpec};
    use super::*;

    fn spec_in(tmp: &tempfile::TempDir, common_name: &str) -> CertTlsSpec {
        CertTlsSpec {
            cert_path: tmp.path().join("server.crt"),
            key_path: tmp.path().join("server.key"),
            common_name: common_name.to_string(),
            algorithm: CertAlgorithm::Ed25519,
            days_valid: 365,
            renew_before_days: 30,
            owner: None,
            group: None,
            mode_cert: 0o644,
            mode_key: 0o600,
            subject_alt_names: Vec::new(),
        }
    }

    /// Записать на диск cert/key, выданные `generate`, чтобы plan мог их
    /// распарсить. Имитирует «уже существующее состояние, оставленное
    /// предыдущим apply».
    fn write_pair(spec: &CertTlsSpec, now: OffsetDateTime) {
        let out = generate(spec, now).unwrap();
        std::fs::write(&spec.cert_path, out.cert_pem).unwrap();
        std::fs::write(&spec.key_path, out.key_pem).unwrap();
    }

    #[test]
    fn create_when_cert_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        std::fs::write(&spec.key_path, b"key").unwrap();
        let now = Utc::now();
        let action = decide_action_cert(&spec, now).unwrap();
        assert_eq!(action, Action::Create);
    }

    #[test]
    fn create_when_key_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        std::fs::write(&spec.cert_path, b"---BEGIN CERTIFICATE---\nx").unwrap();
        let now = Utc::now();
        let action = decide_action_cert(&spec, now).unwrap();
        assert_eq!(action, Action::Create);
    }

    #[test]
    fn create_when_both_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        let now = Utc::now();
        let action = decide_action_cert(&spec, now).unwrap();
        assert_eq!(action, Action::Create);
    }

    #[test]
    fn no_change_when_expiry_far() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host.example.com");
        let now_ts: i64 = 1_700_000_000;
        let now_ot = OffsetDateTime::from_unix_timestamp(now_ts).unwrap();
        write_pair(&spec, now_ot);
        let now = DateTime::<Utc>::from_timestamp(now_ts, 0).unwrap();
        let action = decide_action_cert(&spec, now).unwrap();
        assert_eq!(action, Action::NoChange);
    }

    #[test]
    fn renew_when_expiry_near() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host.example.com");
        // Сертификат сгенерирован «давно» (days_valid=365, наблюдатель
        // живёт за 350 дней до now-2024 → остаётся 15 дней, < threshold 30).
        let issued_ts: i64 = 1_700_000_000;
        let issued = OffsetDateTime::from_unix_timestamp(issued_ts).unwrap();
        write_pair(&spec, issued);
        let now_ts = issued_ts + 350 * 86_400;
        let now = DateTime::<Utc>::from_timestamp(now_ts, 0).unwrap();
        let action = decide_action_cert(&spec, now).unwrap();
        match action {
            Action::Renew { reason } => assert!(reason.contains("expiry near")),
            other => panic!("expected Renew, got {other:?}"),
        }
    }

    #[test]
    fn renew_when_common_name_drift() {
        let tmp = tempfile::tempdir().unwrap();
        // Записываем cert с CN=old; spec ожидает CN=new.
        let mut old_spec = spec_in(&tmp, "old.example.com");
        let now_ot = OffsetDateTime::now_utc();
        write_pair(&old_spec, now_ot);
        // Меняем CN в spec — файлы остались с CN=old.
        old_spec.common_name = "new.example.com".to_string();
        let now = Utc::now();
        let action = decide_action_cert(&old_spec, now).unwrap();
        match action {
            Action::Renew { reason } => assert!(reason.contains("common_name drift")),
            other => panic!("expected Renew, got {other:?}"),
        }
    }

    #[test]
    fn renew_when_cert_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        std::fs::write(&spec.cert_path, b"not a real cert").unwrap();
        std::fs::write(&spec.key_path, b"not a real key").unwrap();
        let now = Utc::now();
        let action = decide_action_cert(&spec, now).unwrap();
        match action {
            Action::Renew { reason } => {
                // PEM decode failure либо unexpected label — оба варианта валидны.
                assert!(reason.contains("PEM decode") || reason.contains("PEM label"));
            }
            other => panic!("expected Renew, got {other:?}"),
        }
    }

    #[test]
    fn rejects_symlink_cert() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        let real = tmp.path().join("real.crt");
        std::fs::write(&real, b"x").unwrap();
        std::os::unix::fs::symlink(&real, &spec.cert_path).unwrap();
        std::fs::write(&spec.key_path, b"y").unwrap();
        let now = Utc::now();
        let err = decide_action_cert(&spec, now).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidTarget));
    }

    #[test]
    fn rejects_symlink_key() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec_in(&tmp, "host");
        let real = tmp.path().join("real.key");
        std::fs::write(&real, b"x").unwrap();
        std::os::unix::fs::symlink(&real, &spec.key_path).unwrap();
        std::fs::write(&spec.cert_path, b"y").unwrap();
        let now = Utc::now();
        let err = decide_action_cert(&spec, now).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidTarget));
    }
}
