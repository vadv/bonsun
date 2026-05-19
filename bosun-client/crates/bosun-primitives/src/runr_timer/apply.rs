//! Apply-фаза `runr.timer`.
//!
//! Логика проще, чем для service: таймеры не требуют notify-семантики,
//! enable/disable — desired-state операции. Все вызовы синхронные.

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};
use bosun_runr_client::RunrError;

use super::plan::{decide_timer_action, TimerAction};
use super::spec::RunrTimerSpec;

pub fn run(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }
    let spec: RunrTimerSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("runr.timer payload: {e}")))?;
    let Some(runr) = ctx.runr.as_ref() else {
        return Err(PrimitiveError::RunrUnavailable {
            base_url: "n/a".to_string(),
            reason: "runr client not initialized in ApplyCtx".to_string(),
        });
    };

    let timers = runr
        .timer_statuses()
        .map_err(|e| map_runr_error(e, runr.base_url(), "timer_statuses"))?;
    let current = timers.iter().find(|t| t.name == spec.name);
    let action = decide_timer_action(&spec, current);

    match action {
        TimerAction::NoChange => Ok(ChangeReport::no_change()),
        TimerAction::Enable { start_now } => {
            runr.timer_enable(&spec.name, start_now)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_enable"))?;
            Ok(ChangeReport::changed(format!(
                "enabled runr.timer:{} (start_now={})",
                spec.name, start_now
            )))
        }
        TimerAction::Disable => {
            runr.timer_disable(&spec.name, false)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_disable"))?;
            Ok(ChangeReport::changed(format!(
                "disabled runr.timer:{}",
                spec.name
            )))
        }
        TimerAction::StopAndDisable => {
            // Stop первым, чтобы между ним и disable не отлетел один тик.
            runr.timer_stop(&spec.name)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_stop"))?;
            runr.timer_disable(&spec.name, true)
                .map_err(|e| map_runr_error(e, runr.base_url(), "timer_disable"))?;
            Ok(ChangeReport::changed(format!(
                "stopped and disabled runr.timer:{}",
                spec.name
            )))
        }
    }
}

/// Маппинг идентичен `runr_service::apply::map_runr_error`. Вынесено в
/// локальную функцию, чтобы избежать pub-exposure.
fn map_runr_error(err: RunrError, base_url: &str, op: &str) -> PrimitiveError {
    match err {
        RunrError::Unavailable { base_url, source } => PrimitiveError::RunrUnavailable {
            base_url,
            reason: format!("{op}: {source}"),
        },
        RunrError::NotFound { kind, name } => PrimitiveError::Apply {
            reason: format!("runr {kind} not found: {name} (during {op})"),
        },
        RunrError::ApiError { status, body } => PrimitiveError::Apply {
            reason: format!("runr API error during {op}: status={status}, body={body}"),
        },
        RunrError::BadResponse(msg) => PrimitiveError::Apply {
            reason: format!("runr returned invalid JSON during {op}: {msg}"),
        },
        RunrError::RestartNotObserved { unit } => PrimitiveError::Apply {
            reason: format!("runr restart of {unit} not observed (op={op})"),
        },
        RunrError::Io(e) => PrimitiveError::RunrUnavailable {
            base_url: base_url.to_string(),
            reason: format!("{op}: i/o error: {e}"),
        },
        other => PrimitiveError::Apply {
            reason: format!("runr error during {op}: {other}"),
        },
    }
}
