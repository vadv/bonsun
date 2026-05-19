//! Валидированный newtype для имени unit'а (systemd service/timer, runr
//! service/timer/cgroup, target для defer-журнала).
//!
//! Имя unit'а попадает сразу в три критичных места:
//! - В путь файла журнала defers (`<priority>-<init_system>.<action>:<name>.deferred`).
//!   Имя с `/` или `..` ломает путь и может попасть в чужую директорию.
//! - В HTTP-путь runr API (`/api/v1/services/<name>/start`). Имя с пробелом,
//!   `?`, `#` ломает URL.
//! - В serde payload примитива из Starlark. Имя приходит от пользователя
//!   bundle'а и до сих пор не валидировалось.
//!
//! Регексп валидации: `^[a-zA-Z0-9][a-zA-Z0-9._@-]*$`. Это объединение
//! systemd unit naming (буквы/цифры, `.`, `-`, `_`, `@` для template
//! instances) и допустимых для HTTP path segments. Максимум 255 байт —
//! POSIX-лимит на filename и заведомо больше любого реалистичного unit
//! name.

use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

/// Ошибка валидации `UnitName`. Возвращается из конструкторов и serde
/// Deserialize. Хранит входное значение для диагностики, но усекается до
/// разумного размера: при `name="<10kb of garbage>"` логи должны быть
/// читаемыми.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum UnitNameError {
    #[error("unit name is empty")]
    Empty,
    #[error("unit name too long: {len} bytes (max 255)")]
    TooLong { len: usize },
    #[error("unit name {input:?} contains invalid character at byte {offset}: {ch:?}")]
    InvalidChar {
        input: String,
        offset: usize,
        ch: char,
    },
    #[error("unit name {input:?} must start with letter or digit, got {ch:?}")]
    BadFirstChar { input: String, ch: char },
}

/// Максимальная длина имени unit'а в байтах. POSIX-лимит на filename —
/// 255, мы держим то же значение для совместимости.
pub const UNIT_NAME_MAX_BYTES: usize = 255;

/// Имя unit'а после валидации.
///
/// Сконструировать можно либо `UnitName::new("foo")?`, либо через serde
/// Deserialize — последний кейс срабатывает на десериализации payload'а
/// примитива из Starlark. Валидация гарантирует:
/// - не пусто;
/// - не длиннее `UNIT_NAME_MAX_BYTES`;
/// - первый символ — буква (ASCII) или цифра;
/// - каждый следующий символ — буква, цифра, `.`, `_`, `@`, `-`.
///
/// Это исключает path traversal (`/`, `..`), управляющие символы, любой
/// non-ASCII текст. Имя безопасно вкладывается в filename, в HTTP path
/// segment и в JSON.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnitName(String);

impl UnitName {
    /// Создать `UnitName` с валидацией. Используется в production-коде,
    /// где имя приходит из надёжного источника (например, после явного
    /// преобразования из проверенной строки).
    pub fn new(s: impl Into<String>) -> Result<Self, UnitNameError> {
        let raw = s.into();
        validate(&raw)?;
        Ok(Self(raw))
    }

    /// Доступ к внутреннему `&str`. Используется для filename и HTTP path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for UnitName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Deref for UnitName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UnitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for UnitName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UnitName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        UnitName::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Чистая функция валидации. Принимает `&str` и возвращает либо
/// `Ok(())`, либо первую найденную ошибку. Вызывается из `UnitName::new`
/// и serde — единая точка истины.
fn validate(s: &str) -> Result<(), UnitNameError> {
    if s.is_empty() {
        return Err(UnitNameError::Empty);
    }
    if s.len() > UNIT_NAME_MAX_BYTES {
        return Err(UnitNameError::TooLong { len: s.len() });
    }
    // Первый символ — буква или цифра. Это исключает leading `-`, `.`,
    // `_`, `@`: такие имена ломают системные утилиты (apt/ls/find).
    let first = match s.chars().next() {
        Some(c) => c,
        None => return Err(UnitNameError::Empty),
    };
    if !is_alphanumeric_ascii(first) {
        return Err(UnitNameError::BadFirstChar {
            input: s.to_string(),
            ch: first,
        });
    }
    // Все байты должны быть допустимыми. Идём по байтам, не char-индексам:
    // нам важна точная позиция ошибки в JSON-диагностике.
    for (offset, byte) in s.bytes().enumerate() {
        if !is_allowed_byte(byte) {
            // Восстанавливаем char для удобства чтения сообщения.
            let ch = s[offset..].chars().next().unwrap_or('?');
            return Err(UnitNameError::InvalidChar {
                input: s.to_string(),
                offset,
                ch,
            });
        }
    }
    Ok(())
}

fn is_alphanumeric_ascii(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

fn is_allowed_byte(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'@' | b'-')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn accepts_simple_name() {
        let n = UnitName::new("nginx").unwrap();
        assert_eq!(n.as_str(), "nginx");
    }

    #[test]
    fn accepts_systemd_template_instance() {
        // `getty@tty1` — стандартный systemd template instance.
        let n = UnitName::new("getty@tty1").unwrap();
        assert_eq!(n.as_str(), "getty@tty1");
    }

    #[test]
    fn accepts_dots_dashes_underscores() {
        UnitName::new("foo.service").unwrap();
        UnitName::new("foo-bar").unwrap();
        UnitName::new("foo_bar").unwrap();
        UnitName::new("a1.b2-c3_d4").unwrap();
    }

    #[test]
    fn rejects_empty() {
        let err = UnitName::new("").unwrap_err();
        assert!(matches!(err, UnitNameError::Empty));
    }

    #[test]
    fn rejects_path_traversal() {
        // `../etc/passwd` — типичная атака на defer-журнал.
        let err = UnitName::new("../etc/passwd").unwrap_err();
        match err {
            UnitNameError::BadFirstChar { ch, .. } => assert_eq!(ch, '.'),
            other => panic!("expected BadFirstChar, got {other:?}"),
        }
    }

    #[test]
    fn rejects_slash() {
        let err = UnitName::new("foo/bar").unwrap_err();
        assert!(matches!(err, UnitNameError::InvalidChar { ch: '/', .. }));
    }

    #[test]
    fn rejects_whitespace() {
        let err = UnitName::new("foo bar").unwrap_err();
        assert!(matches!(err, UnitNameError::InvalidChar { ch: ' ', .. }));
    }

    #[test]
    fn rejects_newline() {
        let err = UnitName::new("foo\nbar").unwrap_err();
        assert!(matches!(err, UnitNameError::InvalidChar { .. }));
    }

    #[test]
    fn rejects_url_special_chars() {
        for bad in ["foo?bar", "foo#bar", "foo%2fbar", "foo&bar"] {
            let err = UnitName::new(bad).unwrap_err();
            assert!(
                matches!(err, UnitNameError::InvalidChar { .. }),
                "for input {bad:?} got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_non_ascii() {
        let err = UnitName::new("инвалид").unwrap_err();
        // Cyrillic chars не ascii_alphanumeric и не в allowed_byte —
        // первый байт это half-cyrillic, поэтому BadFirstChar.
        assert!(matches!(
            err,
            UnitNameError::BadFirstChar { .. } | UnitNameError::InvalidChar { .. }
        ));
    }

    #[test]
    fn rejects_leading_special() {
        // Имя не может начинаться с `.`, `-`, `_`, `@`: leading-dot файлы
        // прячутся в ls, leading-dash ломает getopts'ы инструментов.
        for bad in [".foo", "-foo", "_foo", "@foo"] {
            let err = UnitName::new(bad).unwrap_err();
            assert!(
                matches!(err, UnitNameError::BadFirstChar { .. }),
                "for input {bad:?} got {err:?}"
            );
        }
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(UNIT_NAME_MAX_BYTES + 1);
        let err = UnitName::new(long).unwrap_err();
        match err {
            UnitNameError::TooLong { len } => assert_eq!(len, UNIT_NAME_MAX_BYTES + 1),
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    #[test]
    fn accepts_exactly_255_bytes() {
        let max = "a".repeat(UNIT_NAME_MAX_BYTES);
        UnitName::new(max).unwrap();
    }

    #[test]
    fn serde_round_trip() {
        let original = UnitName::new("foo@1.service").unwrap();
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, "\"foo@1.service\"");
        let parsed: UnitName = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn serde_deserialize_rejects_invalid_name() {
        let json = "\"../etc/passwd\"";
        let err = serde_json::from_str::<UnitName>(json).unwrap_err();
        let msg = format!("{err}");
        // Сообщение прокинуто из UnitNameError::BadFirstChar.
        assert!(msg.contains("must start with"), "got: {msg}");
    }

    #[test]
    fn deref_to_str_works() {
        let n = UnitName::new("foo").unwrap();
        // Deref<Target=str> — можно использовать &n как &str.
        let s: &str = &n;
        assert_eq!(s, "foo");
        assert_eq!(n.len(), 3);
    }
}
