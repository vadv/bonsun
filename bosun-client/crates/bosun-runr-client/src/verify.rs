//! Polling-верификация: убедиться, что restart фактически произошёл.
//!
//! runr не отдаёт `JobRemoved`-эквивалент (см. research-секцию 4 в спеке).
//! Чтобы отличить «runr принял команду, но рестарт не случился» от успеха,
//! берём snapshot `ServiceStatus` до запроса, выполняем restart, затем в
//! цикле опрашиваем `service_statuses()` и ждём, пока счётчик `restarts`
//! инкрементится, а `state` снова станет `"Running"`. По таймауту →
//! `RunrError::RestartNotObserved`.

use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::RunrError;
use crate::types::ServiceStatus;

/// Значение `ServiceStatus.state`, при котором verify считает сервис
/// успешно поднявшимся. Совпадает с тем, что runr возвращает в JSON.
const RUNNING_STATE: &str = "Running";

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
