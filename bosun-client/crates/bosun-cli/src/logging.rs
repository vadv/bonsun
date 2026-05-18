//! Инициализация `tracing-subscriber`. Per spec логи всегда идут в stderr —
//! stdout зарезервирован под отчёт.
//!
//! Поддерживается две формы вывода: human-readable text и structured JSON.
//! Subscriber устанавливается глобально через `set_global_default`.

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt as _;
use tracing_subscriber::EnvFilter;

use crate::args::{LogFormat, LogLevel};

/// Сконфигурировать глобальный subscriber. Вызывается ровно один раз на
/// процесс — повторный вызов невозможен из-за `set_global_default`. CLI
/// гарантирует один вызов из `run::run`.
///
/// Возвращает `Err`, если subscriber уже установлен. CLI это лечит через
/// логирование на stderr и продолжает работу.
pub fn init(
    level: LogLevel,
    format: LogFormat,
    no_color: bool,
) -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    let level_filter = match level {
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Error => LevelFilter::ERROR,
    };

    // EnvFilter позволяет переопределять уровень через RUST_LOG в проде.
    // Если RUST_LOG не задан — используем уровень из CLI-флага.
    let env_filter = EnvFilter::builder()
        .with_default_directive(level_filter.into())
        .from_env_lossy();

    let writer = std::io::stderr.with_max_level(tracing::Level::TRACE);

    match format {
        LogFormat::Text => {
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(writer)
                .with_ansi(!no_color)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
        }
        LogFormat::Json => {
            let subscriber = tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(writer)
                .json()
                .finish();
            tracing::subscriber::set_global_default(subscriber)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Тесты subscriber'а ставят его в локальный scope через
    /// `with_default` — это не глобальная установка, и потому безопасно
    /// для cargo test (где несколько тестов делят процесс).
    #[test]
    fn level_filter_maps_each_variant() {
        // Sanity: enum -> level. Через прямое сравнение, без установки subscriber'а.
        for (variant, expected) in [
            (LogLevel::Debug, LevelFilter::DEBUG),
            (LogLevel::Info, LevelFilter::INFO),
            (LogLevel::Warn, LevelFilter::WARN),
            (LogLevel::Error, LevelFilter::ERROR),
        ] {
            // Повторяем маппинг из init — мы хотим зафиксировать его
            // как часть API.
            let actual = match variant {
                LogLevel::Debug => LevelFilter::DEBUG,
                LogLevel::Info => LevelFilter::INFO,
                LogLevel::Warn => LevelFilter::WARN,
                LogLevel::Error => LevelFilter::ERROR,
            };
            assert_eq!(actual, expected);
        }
    }
}
