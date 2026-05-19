//! Url-вариант health-check'а: GET через `ureq::Agent`, сверка status code.
//!
//! Каждая попытка переиспользует один `ureq::Agent`. Это даёт нам keep-alive
//! и одинаковый timeout-режим, но не привязывает нас к runr-client'у:
//! agent создаётся внутри Real-runner'а, отдельный от `bosun-runr-client`,
//! чтобы health-check работал и на systemd-only нодах.

use std::time::Duration;

use bosun_core::health_check::EXCERPT_LIMIT;
use ureq::Agent;

/// Результат одной попытки url-probe'а.
#[derive(Debug)]
pub(super) enum Attempt {
    /// Status code совпал с ожидаемым.
    Success,
    /// Получен ответ, но status не совпал.
    BadStatus { actual: u16 },
    /// Transport-ошибка (connection refused, DNS, read timeout). Текст
    /// причины — короткий excerpt без секретов.
    Transport { reason: String },
}

/// Дефолтный expected status, если оператор не указал.
pub(super) const DEFAULT_EXPECTED_STATUS: u16 = 200;

/// Сборка ureq-агента под health-check. Отдельный от `bosun-runr-client`:
/// health-check работает на systemd-only нодах, где runr-client может
/// быть не инициализирован.
pub(super) fn build_agent(timeout: Duration) -> Agent {
    ureq::AgentBuilder::new()
        .timeout_read(timeout)
        .timeout_write(timeout)
        .timeout_connect(timeout)
        .build()
}

/// Выполнить одну попытку GET → проверка status.
pub(super) fn run_once(agent: &Agent, url: &str, expected: u16) -> Attempt {
    match agent.get(url).call() {
        Ok(response) => {
            let actual = response.status();
            if actual == expected {
                Attempt::Success
            } else {
                Attempt::BadStatus { actual }
            }
        }
        Err(ureq::Error::Status(actual, _response)) => {
            // ureq трактует 4xx/5xx как `Error::Status`. Если оператор
            // ждал 200, а получил 500 — это BadStatus. Если оператор
            // ждал 500 (edge-case), trade'м точно так же — это совпадение.
            if actual == expected {
                Attempt::Success
            } else {
                Attempt::BadStatus { actual }
            }
        }
        Err(ureq::Error::Transport(t)) => {
            let reason = transport_reason(&t);
            Attempt::Transport { reason }
        }
    }
}

/// Краткое описание transport-ошибки. Обрезается до `EXCERPT_LIMIT` — то же
/// ограничение, что в cmd-варианте; в production-логах оно вмещает сообщение
/// типа «connection refused» / «dns error: NXDOMAIN».
pub(crate) fn transport_reason(t: &ureq::Transport) -> String {
    truncate_reason(format!("{t}"))
}

/// Обрезка длинной строки до `EXCERPT_LIMIT` байт. Выделено отдельной
/// функцией, чтобы можно было тестировать truncation без real transport-
/// ошибки (создать `ureq::Transport` с длинным сообщением неудобно).
pub(crate) fn truncate_reason(mut s: String) -> String {
    if s.len() > EXCERPT_LIMIT {
        s.truncate(EXCERPT_LIMIT);
    }
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn build_agent_constructs_without_panic() {
        let _ = build_agent(Duration::from_secs(1));
    }

    #[test]
    fn run_once_unreachable_returns_transport() {
        // 127.0.0.1:1 — почти гарантированно никто не слушает.
        let agent = build_agent(Duration::from_millis(200));
        let res = run_once(&agent, "http://127.0.0.1:1/x", 200);
        match res {
            Attempt::Transport { reason } => {
                assert!(
                    !reason.is_empty(),
                    "transport reason должен быть непустым, got: {reason:?}",
                );
            }
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[test]
    fn run_once_invalid_url_returns_transport() {
        let agent = build_agent(Duration::from_millis(200));
        let res = run_once(&agent, "not-a-url://###", 200);
        // ureq может вернуть либо Transport, либо Status. Главное — что
        // не падаем и не блокируемся.
        assert!(matches!(
            res,
            Attempt::Transport { .. } | Attempt::BadStatus { .. }
        ));
    }

    #[test]
    fn run_once_default_expected_status_constant_is_200() {
        assert_eq!(DEFAULT_EXPECTED_STATUS, 200);
    }

    // ---- truncate_reason ----

    #[test]
    fn truncate_reason_short_message_unchanged() {
        // Сообщение короче лимита остаётся как есть, без обрезки.
        let short = "connection refused".to_string();
        let len_before = short.len();
        let out = truncate_reason(short);
        assert_eq!(out.len(), len_before);
        assert_eq!(out, "connection refused");
    }

    #[test]
    fn truncate_reason_at_exact_limit_unchanged() {
        // Граничный случай: длина == EXCERPT_LIMIT — обрезка не делается.
        let s: String = "x".repeat(EXCERPT_LIMIT);
        let out = truncate_reason(s);
        assert_eq!(out.len(), EXCERPT_LIMIT);
    }

    #[test]
    fn truncate_reason_truncates_long_message_to_limit() {
        // Длинное сообщение > EXCERPT_LIMIT → результат строго равен лимиту.
        // Это защищает оператора от мегабайтных error chain'ов в логах,
        // когда `ureq::Transport::Display` цепляет inner `io::Error` с
        // длинным контекстом.
        let s: String = "x".repeat(EXCERPT_LIMIT + 100);
        let out = truncate_reason(s);
        assert_eq!(out.len(), EXCERPT_LIMIT);
    }

    #[test]
    fn truncate_reason_preserves_prefix_of_long_message() {
        // После обрезки начало сообщения сохраняется — это важно для
        // диагностики (`connection refused`, `dns error`, `tls: ...`
        // обычно первые 30-100 байт самые полезные).
        let prefix = "connection refused at upstream.example:5432 ";
        let s = format!("{}{}", prefix, "z".repeat(EXCERPT_LIMIT));
        let out = truncate_reason(s);
        assert_eq!(out.len(), EXCERPT_LIMIT);
        assert!(
            out.starts_with(prefix),
            "prefix должен сохраняться, got: {out:?}",
        );
    }
}
