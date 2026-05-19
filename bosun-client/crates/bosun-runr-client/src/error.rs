//! Типы ошибок для runr HTTP-клиента.

use std::error::Error as StdError;

use thiserror::Error;

/// Верхнеуровневая ошибка runr-клиента.
///
/// Все публичные методы `Client` возвращают `Result<_, RunrError>`. Enum
/// помечен `#[non_exhaustive]` — orchestrator-уровневые слои (defers, primitives)
/// различают категории через `match`, и появление новой категории не должно
/// ломать сборку клиентов.
///
/// Категоризация спроектирована для решения вопроса «можно ли отложить операцию
/// в defers?»:
/// - `Unavailable` — runr недоступен (connection refused, DNS, TCP error). Это
///   `is_deferrable=true` для orchestrator: имеет смысл повторить позже.
/// - `ApiError` — runr ответил 5xx или иной не-2xx, кроме 404. Тоже подлежит
///   повтору, но это уже проблема самого демона, а не сети.
/// - `NotFound` — 404 на запрос конкретного юнита. Не подлежит повтору без
///   изменения spec'а.
/// - `BadResponse` — runr ответил 2xx, но JSON не валиден или схема расходится.
///   Сигнал о расхождении версий клиента и демона.
/// - `RestartNotObserved` — verify-цикл истёк, не увидев инкремента `restarts`.
/// - `Io` — локальная I/O-ошибка (например, при работе с потоком запроса).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunrError {
    /// runr недоступен на указанном `base_url`: connection refused, DNS-сбой,
    /// reset соединения. Сохраняем `base_url` и оригинальную причину, чтобы
    /// orchestrator мог отличить «runr ещё не поднялся» от «runr вернул 5xx».
    #[error("runr unavailable at {base_url}: {source}")]
    Unavailable {
        base_url: String,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    /// runr ответил не-2xx, не относящимся к 404. Сохраняем числовой статус и
    /// тело ответа целиком для логов.
    #[error("runr API error: status={status}, body={body}")]
    ApiError { status: u16, body: String },

    /// 2xx-ответ с телом, которое не разобралось как ожидаемый JSON. Сообщение
    /// содержит подробности от `serde_json` для отладки расхождения схем.
    #[error("runr returned invalid JSON: {0}")]
    BadResponse(String),

    /// 404 на запрос конкретного юнита (сервиса или таймера). `kind` хранит
    /// категорию: `"service"` / `"timer"` / `"unit"`.
    #[error("runr {kind} not found: {name}")]
    NotFound { kind: String, name: String },

    /// Polling-цикл `verify_restart` истёк до того, как `restarts` инкрементился
    /// и состояние стало `Running`. Сигнал, что runr принял команду, но фактический
    /// рестарт либо не произошёл, либо не завершился до дедлайна.
    #[error("restart of {unit} did not produce observed restart increment")]
    RestartNotObserved { unit: String },

    /// Локальная I/O-ошибка (чтение/запись HTTP-потока, чтение env и т.п.).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl RunrError {
    /// Классифицирует `ureq::Error` в подходящий вариант `RunrError`.
    ///
    /// Принимает `base_url` для контекста и `unit_lookup` — опциональную пару
    /// `(kind, name)`, активную для запросов вида `/services/<name>/...`,
    /// чтобы 404 превращался в `NotFound { kind, name }` вместо общего
    /// `ApiError`.
    ///
    /// Для transport-ошибок ureq (`ureq::Error::Transport`) возвращает
    /// `Unavailable` — runr-демон либо не отвечает, либо соединение сорвалось
    /// до получения статуса. Для orchestrator'а оба случая одинаково
    /// deferrable, поэтому различать дальше смысла нет.
    pub(crate) fn from_ureq(
        err: ureq::Error,
        base_url: &str,
        unit_lookup: Option<(&str, &str)>,
    ) -> Self {
        match err {
            ureq::Error::Status(status, response) => {
                let body = response
                    .into_string()
                    .unwrap_or_else(|e| format!("<failed to read body: {e}>"));
                if status == 404 {
                    if let Some((kind, name)) = unit_lookup {
                        return RunrError::NotFound {
                            kind: kind.to_string(),
                            name: name.to_string(),
                        };
                    }
                }
                RunrError::ApiError { status, body }
            }
            ureq::Error::Transport(transport) => RunrError::Unavailable {
                base_url: base_url.to_string(),
                source: Box::new(transport),
            },
        }
    }
}
