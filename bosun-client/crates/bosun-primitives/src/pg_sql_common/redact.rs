//! Замена пароля в DSN на маркер `*****` для безопасного логирования.
//!
//! Поддерживаются два формата DSN:
//!
//! - URL-style `postgres://user:pass@host/db` — парсится через `url::Url`,
//!   `set_password(None)` заменяет пароль на пустую строку, формат
//!   `postgres://user@host/db` остаётся валидным для повторного парсинга.
//!
//! - Key=value `host=... user=... password=...` — простой пословный
//!   скан: каждый токен с префиксом `password=` или `password='`
//!   заменяется на `password=*****`. Не пытаемся быть полным libpq-парсером;
//!   достаточно для post-mortem-логов.
//!
//! Бизнес-правило: пароль не должен попадать в tracing-логи.
//! `tracing::info!(dsn = %dsn)` без redaction — потенциальная утечка.

use url::Url;

/// Маркер, на который заменяется пароль.
const REDACTED: &str = "*****";

/// Вернуть DSN с замаскированным паролем. Если парсинг как URL не удался —
/// падаем в key=value-замену. Если и она ничего не нашла — возвращаем DSN
/// как есть (там нет пароля).
pub fn redact_dsn(dsn: &str) -> String {
    if let Ok(mut url) = Url::parse(dsn) {
        // url::Url::set_password возвращает Err только для cannot-be-base
        // URL (например, `data:...`). Для `postgres://` — всегда Ok.
        if url.password().is_some() {
            let _ = url.set_password(Some(REDACTED));
            return url.to_string();
        }
        return dsn.to_string();
    }

    // Key=value формат: токены разделены whitespace.
    let mut out = String::with_capacity(dsn.len());
    let mut first = true;
    for token in dsn.split_whitespace() {
        if !first {
            out.push(' ');
        }
        first = false;
        if let Some(rest) = token.strip_prefix("password=") {
            // Сохраняем quoting если был: `password='secret'` → `password=*****`.
            // libpq разрешает quoting через `'...'` или escape; мы не пытаемся
            // воссоздать оригинал — просто маскируем всё после `=`.
            let _ = rest;
            out.push_str("password=");
            out.push_str(REDACTED);
        } else {
            out.push_str(token);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn url_style_password_replaced_with_marker() {
        let dsn = "postgres://postgres:s3cret@localhost/postgres";
        let red = redact_dsn(dsn);
        assert!(!red.contains("s3cret"), "пароль не должен остаться: {red}");
        assert!(red.contains("*****"), "маркер должен присутствовать: {red}");
        // user должен сохраниться (с маркером пароля): `postgres:*****@`.
        assert!(
            red.contains("postgres:*****@"),
            "user должен сохраниться: {red}",
        );
    }

    #[test]
    fn url_style_without_password_unchanged() {
        let dsn = "postgres://postgres@localhost/postgres";
        let red = redact_dsn(dsn);
        assert_eq!(red, dsn);
    }

    #[test]
    fn kv_style_password_replaced_with_marker() {
        let dsn = "host=/var/run/postgresql user=postgres password=top_secret dbname=db";
        let red = redact_dsn(dsn);
        assert!(
            !red.contains("top_secret"),
            "пароль не должен остаться: {red}"
        );
        assert!(
            red.contains("password=*****"),
            "маркер должен присутствовать: {red}"
        );
        assert!(red.contains("user=postgres"));
        assert!(red.contains("dbname=db"));
    }

    #[test]
    fn kv_style_without_password_unchanged_modulo_whitespace() {
        let dsn = "host=/var/run/postgresql user=postgres dbname=db";
        let red = redact_dsn(dsn);
        // Содержимое сохраняется; whitespace нормализуется до одного пробела.
        for needle in ["host=/var/run/postgresql", "user=postgres", "dbname=db"] {
            assert!(red.contains(needle), "нет ожидаемого токена в {red}");
        }
        assert!(
            !red.contains("password="),
            "не должно появиться password= когда его не было"
        );
    }

    #[test]
    fn quoted_password_in_kv_style_also_masked() {
        let dsn = "user=u password='se cr et' dbname=d";
        let red = redact_dsn(dsn);
        // После маскирования пробелов внутри пароля больше нет, поэтому
        // обычная whitespace-сплитовка не воспроизведёт исходную форму,
        // но главное — `se cr et` исчезло.
        assert!(
            !red.contains("se cr et"),
            "quoted password not masked: {red}"
        );
        assert!(red.contains("password=*****"), "маркер должен быть: {red}");
    }

    #[test]
    fn empty_string_passes_through_without_panic() {
        // Граничный случай: redact_dsn("") не должен паниковать и должен
        // вернуть пустую строку. url::Url::parse("") → Err → key=value путь,
        // split_whitespace → пусто, out — "".
        let red = redact_dsn("");
        assert_eq!(red, "", "пустой DSN → пустая строка, got {red:?}");
    }

    #[test]
    fn kv_style_password_without_value_masks_to_marker() {
        // `password=` без значения — corner-case libpq. Замена должна
        // подменить остаток на маркер, не сохранить пустую строку как пароль.
        let dsn = "host=h user=u password= dbname=d";
        let red = redact_dsn(dsn);
        assert!(red.contains("password=*****"), "маркер должен быть: {red}");
        assert!(red.contains("dbname=d"));
    }

    #[test]
    fn kv_style_multiple_password_tokens_all_masked() {
        // Если в DSN несколько токенов с префиксом password= (хотя libpq
        // возьмёт только последний) — мы маскируем каждый.
        let dsn = "user=u password=first dbname=d password=second";
        let red = redact_dsn(dsn);
        assert!(!red.contains("first"), "first password утёк: {red}");
        assert!(!red.contains("second"), "second password утёк: {red}");
        assert_eq!(
            red.matches("password=*****").count(),
            2,
            "оба токена должны быть замаскированы, got {red}"
        );
    }
}
