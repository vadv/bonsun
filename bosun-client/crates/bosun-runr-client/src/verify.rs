//! Polling-верификация для синхронных runr-операций.
//!
//! runr не отдаёт `JobRemoved`-эквивалент (см. research-секцию 4 в спеке).
//! Поэтому после `service_restart` или `service_start` мы опрашиваем
//! `service_statuses()` в цикле. Критерии успеха разные:
//!
//! - `verify_restart` — ждёт замены PID процесса И `state="Running"`. Это
//!   единственный наблюдаемый сигнал «runr действительно поднял новый
//!   процесс» при ручном `POST /api/v1/services/<n>/restart`. На счётчик
//!   `restarts` опираться нельзя: runr инкрементирует его только при
//!   автоматическом restart'е (Restart=always после exit/crash), не при
//!   внешних API-вызовах restart (см. handler `ServiceCommand::Restart`
//!   в `runr/src/orchestration/actors/simple.rs`). Сохранён fallback на
//!   инкремент `restarts` для совместимости со старыми runr-демонами,
//!   которые не отдают `pid` в `ServiceStatus`, и для моков, эмулирующих
//!   только restarts-counter.
//!
//! - `verify_start` — ждёт только `state="Running"`. Для start-с-нуля
//!   счётчик `restarts` остаётся 0, и проверять его бессмысленно. Если
//!   процесс упал в `Failed` сразу после старта — возвращаем
//!   `ServiceStartFailed` (ждать дольше бесполезно, runr сам не оживит).
//!
//! По таймауту: `RestartNotObserved` или `StartNotObserved` соответственно.

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::RunrError;
use crate::types::ServiceStatus;

/// Значение `ServiceStatus.state`, при котором verify считает сервис
/// успешно поднявшимся. Совпадает с тем, что runr возвращает в JSON.
const RUNNING_STATE: &str = "Running";

/// Значение `ServiceStatus.state`, означающее, что сервис упал.
/// `verify_start` отвергает Failed мгновенно — ждать дальше бессмысленно.
const FAILED_STATE: &str = "Failed";

/// Polling-цикл верификации рестарта.
///
/// Возвращает свежий `ServiceStatus` сервиса, который удовлетворяет:
/// `state == "Running"` И (PID изменился, либо PID недоступен и счётчик
/// `restarts` строго вырос относительно `before.restarts`).
/// По истечении `poll_total` без такого наблюдения — `RunrError::RestartNotObserved`.
///
/// Почему PID, а не `restarts`:
/// real runr инкрементирует счётчик только при автоматическом restart'е
/// (Restart=always после exit/crash), не при внешнем `POST /restart`.
/// Опора на `restarts` для ручных restart'ов даёт false negative:
/// процесс реально пересоздаётся, но bosun дожидается дедлайна и считает
/// операцию провальной. Подробнее в комментарии в начале модуля.
///
/// Контракт:
/// - `before` — снимок состояния сервиса, сделанный непосредственно ДО запроса
///   `service_restart`. Поля `before.pid` и `before.restarts` фиксируют
///   базовую линию.
/// - `poll_interval` — пауза между опросами. Должна быть достаточно мала,
///   чтобы вписаться в `poll_total` хотя бы 3-5 раз.
/// - `poll_total` — общий дедлайн. Если до момента, когда `now() - start >=
///   poll_total`, изменение не наблюдается, возвращается ошибка.
///
/// Любая `RunrError` от `service_statuses()` (Unavailable, BadResponse и
/// т.п.) пробрасывается наверх без ретраев — это решение orchestrator-уровня.
pub fn verify_restart(
    client: &Client,
    name: &str,
    before: &ServiceStatus,
    poll_interval: Duration,
    poll_total: Duration,
) -> Result<ServiceStatus, RunrError> {
    let deadline = Instant::now() + poll_total;
    loop {
        let statuses = client.service_statuses()?;
        if let Some(current) = statuses.into_iter().find(|s| s.name == name) {
            if is_restart_observed(before, &current) {
                return Ok(current);
            }
        }
        if Instant::now() >= deadline {
            return Err(RunrError::RestartNotObserved {
                unit: name.to_string(),
            });
        }
        sleep(poll_interval);
    }
}

/// Чистая функция: считает ли `current` наблюдаемым результатом restart'а
/// относительно `before`. Выделена ради тестируемости — два условия (PID-diff
/// и restarts-increment) описывают одно и то же успешное состояние, но
/// взаимно исключают друг друга на разных версиях runr.
///
/// Логика:
/// - Сервис обязан быть в `Running`. Иначе любые изменения других полей не
///   считаются — может быть Backoff между падением и следующим стартом.
/// - PID-diff: если оба снимка отдали `pid` и они различаются — restart
///   виден напрямую. Это primary-критерий для production-runr.
/// - Fallback на `restarts`: если PID не помог (например, кто-то из снимков
///   без `pid` — старый runr API, mock или transient race на ребуте),
///   считаем растущий счётчик `restarts`. Это back-compat путь.
fn is_restart_observed(before: &ServiceStatus, current: &ServiceStatus) -> bool {
    if current.state != RUNNING_STATE {
        return false;
    }
    if let (Some(before_pid), Some(current_pid)) = (before.pid, current.pid) {
        if before_pid != current_pid {
            return true;
        }
    }
    // Fallback: pid не доступен в одном из снимков. Старый runr без `pid`
    // или мок, эмулирующий только counter, — считаем инкремент `restarts`.
    current.restarts > before.restarts
}

/// Polling-цикл верификации старта.
///
/// Семантика отличается от `verify_restart`: для start-с-нуля счётчик
/// `restarts` остаётся 0, поэтому опираться на инкремент нельзя. Ждём
/// прямого перехода `state == "Running"`.
///
/// Возврат:
/// - `Ok(status)` — сервис в `Running` за окно `poll_total`.
/// - `Err(ServiceStartFailed)` — сервис попал в `Failed`. Не ждём, runr
///   сам не оживит процесс.
/// - `Err(StartNotObserved)` — `poll_total` истёк, а `state` так и не
///   стал `Running` (но и не `Failed`: например, остался в `Starting`
///   или `Stopped`).
pub fn verify_start(
    client: &Client,
    name: &str,
    poll_interval: Duration,
    poll_total: Duration,
) -> Result<ServiceStatus, RunrError> {
    let deadline = Instant::now() + poll_total;
    let mut last_state = "Unknown".to_string();
    loop {
        let statuses = client.service_statuses()?;
        if let Some(current) = statuses.into_iter().find(|s| s.name == name) {
            last_state = current.state.clone();
            if current.state == RUNNING_STATE {
                return Ok(current);
            }
            if current.state == FAILED_STATE {
                return Err(RunrError::ServiceStartFailed {
                    unit: name.to_string(),
                });
            }
        }
        if Instant::now() >= deadline {
            return Err(RunrError::StartNotObserved {
                unit: name.to_string(),
                last_state,
            });
        }
        sleep(poll_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Минимальный конструктор `ServiceStatus` для unit-тестов чистой
    /// функции `is_restart_observed`. Заполняет `name` пустым, потому что
    /// функция его не смотрит.
    fn snap(pid: Option<u32>, restarts: u64, state: &str) -> ServiceStatus {
        ServiceStatus {
            name: String::new(),
            state: state.to_string(),
            restarts,
            pid,
            in_state_for_ms: 0,
            uptime_ms: None,
            downtime_ms: None,
            next_restart_in_ms: None,
            started_at: None,
            autostart: false,
            memory_rss_anon_bytes: 0,
            memory_rss_file_bytes: 0,
            cpu_usage_percent: 0.0,
        }
    }

    #[test]
    fn restart_observed_when_pid_changed_and_running() {
        let before = snap(Some(100), 3, "Running");
        let after = snap(Some(200), 3, "Running");
        assert!(is_restart_observed(&before, &after));
    }

    #[test]
    fn restart_not_observed_when_pid_same_and_restarts_same() {
        // Ни один из критериев не сработал — restart не виден.
        let before = snap(Some(100), 3, "Running");
        let after = snap(Some(100), 3, "Running");
        assert!(!is_restart_observed(&before, &after));
    }

    #[test]
    fn restart_not_observed_when_pid_changed_but_state_not_running() {
        // PID сменился, но сервис в Backoff/Failed — нельзя считать успехом.
        let before = snap(Some(100), 3, "Running");
        let after = snap(Some(200), 3, "Failed");
        assert!(!is_restart_observed(&before, &after));
    }

    #[test]
    fn restart_observed_via_restarts_fallback_when_before_pid_none() {
        // Старый runr или snapshot до старта — pid=None. Fallback на счётчик.
        let before = snap(None, 3, "Running");
        let after = snap(Some(200), 4, "Running");
        assert!(is_restart_observed(&before, &after));
    }

    #[test]
    fn restart_observed_via_restarts_fallback_when_current_pid_none() {
        // Snapshot пришёл в момент Backoff между exit и start — pid=None,
        // но restarts уже инкрементился. Если state Running — успех (теоретически
        // редкое сочетание, но допустимо: runr может опубликовать состояние
        // раньше пида в конкурентной обстановке).
        let before = snap(Some(100), 3, "Running");
        let after = snap(None, 4, "Running");
        assert!(is_restart_observed(&before, &after));
    }

    #[test]
    fn restart_not_observed_when_both_pids_none_and_counter_unchanged() {
        let before = snap(None, 3, "Running");
        let after = snap(None, 3, "Running");
        assert!(!is_restart_observed(&before, &after));
    }
}
