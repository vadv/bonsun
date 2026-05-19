//! Plan-фаза `process.signal`.
//!
//! Семантика «всегда Update»: примитив не хранит state — каждый apply
//! пытается поставить запись в журнал defers (или выполнить синхронно),
//! идемпотентность гарантирует journal dedup (Phase C). Если запись с тем
//! же `id` уже лежит — это `EnqueueResult::AlreadyExists`, apply вернёт
//! `ChangeReport::no_change()`.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use super::spec::ProcessSignalSpec;

/// Главная функция plan: десериализует spec и валидирует селектор/сигнал,
/// чтобы поймать невалидную конфигурацию ДО apply (раньше всего, и до
/// reload defer-цикла).
pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: ProcessSignalSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("process.signal payload: {e}")))?;

    // Раннее обнаружение проблем: невалидный сигнал или некорректный
    // селектор. Это даёт fail-fast в plan-фазе вместо отложенного провала
    // в apply (и тем более — в defer replay).
    let _argv = super::apply::build_signal_argv(&spec)?;

    let selector = describe_selector(&spec);
    Ok(Diff::Update {
        from: serde_json::json!({"process.signal": "stateless"}),
        to: resource.payload.clone(),
        description: format!(
            "send {} to {}",
            normalize_signal_for_display(&spec.signal),
            selector
        ),
    })
}

/// Описание селектора для тестов и логов: `by-name=pg_doorman` или
/// `by-user=postgres`. Используется и в `Diff::description`, и в
/// `ChangeReport::deferred`.
pub(crate) fn describe_selector(spec: &ProcessSignalSpec) -> String {
    match (&spec.process_name, &spec.process_user) {
        (Some(n), None) => format!("by-name={n}"),
        (None, Some(u)) => format!("by-user={u}"),
        // Эти ветки недостижимы при корректном build_signal_argv (он бы уже
        // дал InvalidPayload). Возвращаем сырое описание для диагностики.
        (Some(n), Some(u)) => format!("by-name={n} by-user={u} (invalid)"),
        (None, None) => "no-selector (invalid)".to_string(),
    }
}

/// Нормализация имени сигнала для отображения: убираем префикс `SIG`,
/// чтобы лог `send SIGHUP to ...` не дублировал префикс.
fn normalize_signal_for_display(signal: &str) -> String {
    signal.strip_prefix("SIG").unwrap_or(signal).to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::time::{Duration, Instant};

    use bosun_core::{FactValue, ResourceId, ResourceKind};
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct EmptyFacts;
    impl FactsSource for EmptyFacts {
        fn get(&self, _name: &str) -> FactValue {
            FactValue::Unknown {
                reason: "test".to_string(),
            }
        }
    }

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    fn resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("process.signal");
        let id = ResourceId::new(&kind, "test");
        Resource {
            id,
            kind,
            spec_version: 1,
            payload,
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn compute_diff_by_name_returns_update_with_description() {
        let r = resource(serde_json::json!({
            "name": "hup-doorman",
            "signal": "HUP",
            "process_name": "pg_doorman",
            "deferred": true,
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("HUP"), "got: {description}");
                assert!(description.contains("pg_doorman"), "got: {description}");
                assert!(description.contains("by-name="), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_normalizes_sig_prefix_in_description() {
        let r = resource(serde_json::json!({
            "name": "x",
            "signal": "SIGHUP",
            "process_user": "postgres",
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                // В описании ожидаем «HUP», а не «SIGHUP», чтобы не дублировать префикс.
                assert!(
                    description.contains("send HUP to"),
                    "expected normalized 'send HUP', got: {description}",
                );
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_invalid_signal_returns_invalid_payload() {
        let r = resource(serde_json::json!({
            "name": "x",
            "signal": "KILL",
            "process_name": "evil",
        }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(msg.contains("KILL"), "got: {msg}");
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_both_selectors_returns_invalid_payload() {
        let r = resource(serde_json::json!({
            "name": "x",
            "signal": "HUP",
            "process_name": "a",
            "process_user": "b",
        }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    #[test]
    fn compute_diff_no_selector_returns_invalid_payload() {
        let r = resource(serde_json::json!({
            "name": "x",
            "signal": "HUP",
        }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }
}
