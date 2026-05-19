//! Polling-верификация для синхронных runr-операций.
//!
//! runr не отдаёт `JobRemoved`-эквивалент (см. research-секцию 4 в спеке).
//! Поэтому после `service_restart` или `service_start` мы опрашиваем
//! `service_statuses()` в цикле. Критерии успеха разные:
//!
//! - `verify_restart` — ждёт инкремента `restarts` И `state="Running"`.
//!   Для restart-сценария: счётчик инкрементится при каждом restart, и
//!   успех означает «runr действительно перезапустил процесс».
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
/// Возвращает свежий `ServiceStatus` сервиса, у которого `restarts` строго
/// больше, чем `before.restarts`, и `state == "Running"`. По истечении
/// `poll_total` без такого наблюдения — `RunrError::RestartNotObserved`.
///
/// Контракт:
/// - `before` — снимок состояния сервиса, сделанный непосредственно ДО запроса
///   `service_restart`. Поле `before.restarts` фиксирует базовую линию.
/// - `poll_interval` — пауза между опросами. Должна быть достаточно мала,
///   чтобы вписаться в `poll_total` хотя бы 3-5 раз.
/// - `poll_total` — общий дедлайн. Если до момента, когда `now() - start >=
///   poll_total`, инкремент не наблюдается, возвращается ошибка.
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
            if current.restarts > before.restarts && current.state == RUNNING_STATE {
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
