//! Десериализуемая часть payload'а `apt.key`.
//!
//! Modern apt-стиль: ключ репозитория лежит в
//! `/etc/apt/keyrings/<name>.gpg` (а не в глобальном
//! `/etc/apt/trusted.gpg`) и привязывается к `.list`-файлу через
//! `signed-by=<keyring_path>`. Legacy `apt-key add` сознательно не
//! поддерживается — он deprecated в Debian 11+/Ubuntu 22.04+.

use std::path::PathBuf;

use serde::Deserialize;

/// Состояние ключа: present (есть и совпадает с ожиданием) или absent
/// (удалён из системы).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[non_exhaustive]
#[serde(rename_all = "lowercase")]
pub enum AptKeyState {
    Present,
    Absent,
}

/// Spec примитива `apt.key`.
///
/// Источник ключа для `state=Present` — ровно один из `url` (HTTPS GET) или
/// `key_data` (inline ASCII-armored / binary). Валидация комбинаций в
/// `plan` через `decide_action`.
///
/// `fingerprint` — опциональная сверка после установки. Если указан,
/// `apply` запускает `gpg --show-keys --with-fingerprint <keyring_path>` и
/// сравнивает нормализованный fingerprint. Несовпадение → ошибка Apply.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct AptKeySpec {
    /// Имя ключа: идентификатор в реестре ресурсов и часть пути к keyring'у
    /// по умолчанию (`/etc/apt/keyrings/<name>.gpg`).
    pub name: String,
    /// Желаемое состояние.
    pub state: AptKeyState,
    /// URL для скачивания ключа. Только для `state=Present`.
    #[serde(default)]
    pub url: Option<String>,
    /// Inline данные ключа (ASCII-armored или binary). Только для
    /// `state=Present`. Взаимоисключает `url`.
    #[serde(default)]
    pub key_data: Option<String>,
    /// Ожидаемый fingerprint (hex, 40 символов SHA-1 или 64 SHA-256, с
    /// пробелами или без). Если указан, после установки/обновления
    /// `apply` верифицирует через `gpg --show-keys`.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Путь к keyring-файлу. По умолчанию `/etc/apt/keyrings/<name>.gpg`.
    /// Тесты подменяют на tempdir, чтобы не трогать реальную систему.
    #[serde(default)]
    pub keyring_path: Option<PathBuf>,
}

impl AptKeySpec {
    /// Полный путь к keyring'у с дефолтом по имени.
    pub fn effective_keyring_path(&self) -> PathBuf {
        self.keyring_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("/etc/apt/keyrings/{}.gpg", self.name)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_present_with_url() {
        let json = serde_json::json!({
            "name": "postgres",
            "state": "present",
            "url": "https://example.com/key.asc",
        });
        let spec: AptKeySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "postgres");
        assert_eq!(spec.state, AptKeyState::Present);
        assert_eq!(spec.url.as_deref(), Some("https://example.com/key.asc"));
        assert!(spec.key_data.is_none());
    }

    #[test]
    fn deserialize_absent() {
        let json = serde_json::json!({
            "name": "old-repo",
            "state": "absent",
        });
        let spec: AptKeySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.state, AptKeyState::Absent);
    }

    #[test]
    fn deserialize_with_fingerprint_and_inline_data() {
        let json = serde_json::json!({
            "name": "myrepo",
            "state": "present",
            "key_data": "-----BEGIN PGP PUBLIC KEY BLOCK-----\ndata\n-----END PGP PUBLIC KEY BLOCK-----",
            "fingerprint": "ABCD 1234 5678 9012 3456  7890 1234 5678 9012 3456",
        });
        let spec: AptKeySpec = serde_json::from_value(json).unwrap();
        assert!(spec.key_data.is_some());
        assert!(spec.fingerprint.is_some());
        assert!(spec.url.is_none());
    }

    #[test]
    fn deserialize_keyring_path_override() {
        let json = serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://x/y",
            "keyring_path": "/tmp/x.gpg",
        });
        let spec: AptKeySpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.effective_keyring_path(), PathBuf::from("/tmp/x.gpg"));
    }

    #[test]
    fn effective_keyring_path_defaults_to_keyrings_dir() {
        let spec = AptKeySpec {
            name: "postgres".into(),
            state: AptKeyState::Present,
            url: None,
            key_data: None,
            fingerprint: None,
            keyring_path: None,
        };
        assert_eq!(
            spec.effective_keyring_path(),
            PathBuf::from("/etc/apt/keyrings/postgres.gpg"),
        );
    }

    #[test]
    fn deserialize_missing_state_is_error() {
        let json = serde_json::json!({ "name": "x" });
        let err = serde_json::from_value::<AptKeySpec>(json).unwrap_err();
        assert!(err.to_string().contains("state"));
    }
}
