//! Plan-фаза `apt.key`.
//!
//! Read-before-write:
//! - `Present`: если `keyring_path` есть и совпадает fingerprint (когда
//!   задан) — `NoChange`. Иначе — `Update`.
//! - `Absent`: если `keyring_path` отсутствует — `NoChange`. Иначе — `Update`.
//!
//! Backend для верификации fingerprint'а через `gpg --show-keys` — DI
//! (см. `apply::AptKeyBackend`). Plan не делает spawn'ов: если для Present
//! файл есть, но fingerprint не задан, считаем «существует — значит ОК»;
//! если fingerprint задан, отдадим Update (verify сделает apply). Это даёт
//! безопасный fallback: следующий цикл подтвердит идемпотентность, а
//! plan-фаза не блокируется на gpg-вызовах.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use super::spec::{AptKeySpec, AptKeyState};

/// Чистое решение plan-фазы. Принимает только наличие keyring'а — этого
/// достаточно, чтобы выбрать NoChange/Update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Установить или перезаписать keyring (Present + отсутствует ИЛИ
    /// fingerprint задан для verify в apply).
    Install,
    /// Удалить keyring (Absent + существует).
    Remove,
    /// Состояние уже соответствует ожиданию.
    NoChange,
}

/// Чистое decision: на вход — есть ли файл, есть ли fingerprint и
/// какое состояние требуется. Используется и в plan, и в apply.
pub(crate) fn decide_action(
    keyring_exists: bool,
    has_fingerprint: bool,
    state: AptKeyState,
) -> Action {
    match (state, keyring_exists) {
        (AptKeyState::Present, false) => Action::Install,
        (AptKeyState::Present, true) if has_fingerprint => {
            // fingerprint verify — в apply'е (требует spawn gpg).
            // Plan возвращает Install, apply делает verify; если совпадает —
            // вернёт NoChange-report без переустановки.
            Action::Install
        }
        (AptKeyState::Present, true) => Action::NoChange,
        (AptKeyState::Absent, true) => Action::Remove,
        (AptKeyState::Absent, false) => Action::NoChange,
    }
}

/// Главная функция plan. Валидирует комбинации Present/url/key_data до
/// выбора Action, чтобы поймать конфигурационные ошибки в plan-фазе.
pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: AptKeySpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key payload: {e}")))?;

    validate_source_combination(&spec)?;

    let keyring_path = spec.effective_keyring_path();
    let exists = keyring_path.exists();
    let has_fingerprint = spec.fingerprint.is_some();

    let action = decide_action(exists, has_fingerprint, spec.state);
    Ok(action_to_diff(&spec, action, &resource.payload))
}

/// Валидировать spec: ровно один из `url`/`key_data` для Present; для
/// Absent оба должны быть отсутствующими.
pub(crate) fn validate_source_combination(spec: &AptKeySpec) -> Result<(), PrimitiveError> {
    match (spec.state, spec.url.is_some(), spec.key_data.is_some()) {
        (AptKeyState::Present, true, false) | (AptKeyState::Present, false, true) => Ok(()),
        (AptKeyState::Present, true, true) => Err(PrimitiveError::InvalidPayload(format!(
            "apt.key '{}': exactly one of url/key_data required for state=present, got both",
            spec.name,
        ))),
        (AptKeyState::Present, false, false) => Err(PrimitiveError::InvalidPayload(format!(
            "apt.key '{}': state=present requires one of url/key_data",
            spec.name,
        ))),
        (AptKeyState::Absent, false, false) => Ok(()),
        (AptKeyState::Absent, _, _) => Err(PrimitiveError::InvalidPayload(format!(
            "apt.key '{}': state=absent must not specify url/key_data",
            spec.name,
        ))),
    }
}

/// Перевести Action в Diff.
fn action_to_diff(spec: &AptKeySpec, action: Action, payload: &serde_json::Value) -> Diff {
    let keyring_path = spec.effective_keyring_path();
    match action {
        Action::NoChange => Diff::NoChange,
        Action::Install => Diff::Update {
            from: serde_json::json!({
                "apt.key": "missing or pending verify",
                "keyring_path": keyring_path.display().to_string(),
            }),
            to: payload.clone(),
            description: format!("install apt key to {}", keyring_path.display()),
        },
        Action::Remove => Diff::Update {
            from: serde_json::json!({
                "apt.key": "present",
                "keyring_path": keyring_path.display().to_string(),
            }),
            to: payload.clone(),
            description: format!("remove apt key {}", keyring_path.display()),
        },
    }
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

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("apt.key");
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
    fn decide_present_missing_is_install() {
        assert_eq!(
            decide_action(false, false, AptKeyState::Present),
            Action::Install
        );
    }

    #[test]
    fn decide_present_exists_no_fingerprint_is_no_change() {
        assert_eq!(
            decide_action(true, false, AptKeyState::Present),
            Action::NoChange
        );
    }

    #[test]
    fn decide_present_exists_with_fingerprint_is_install_for_verify() {
        // fingerprint указан — plan делегирует verify в apply, возвращая
        // Install. Apply реально верифицирует и при совпадении даёт
        // NoChange-report.
        assert_eq!(
            decide_action(true, true, AptKeyState::Present),
            Action::Install
        );
    }

    #[test]
    fn decide_absent_missing_is_no_change() {
        assert_eq!(
            decide_action(false, false, AptKeyState::Absent),
            Action::NoChange
        );
    }

    #[test]
    fn decide_absent_exists_is_remove() {
        assert_eq!(
            decide_action(true, false, AptKeyState::Absent),
            Action::Remove
        );
    }

    #[test]
    fn validate_present_with_url_ok() {
        let spec = AptKeySpec {
            name: "x".into(),
            state: AptKeyState::Present,
            url: Some("https://x/key".into()),
            key_data: None,
            fingerprint: None,
            keyring_path: None,
        };
        validate_source_combination(&spec).unwrap();
    }

    #[test]
    fn validate_present_with_key_data_ok() {
        let spec = AptKeySpec {
            name: "x".into(),
            state: AptKeyState::Present,
            url: None,
            key_data: Some("KEY".into()),
            fingerprint: None,
            keyring_path: None,
        };
        validate_source_combination(&spec).unwrap();
    }

    #[test]
    fn validate_present_both_sources_is_invalid_payload() {
        let spec = AptKeySpec {
            name: "x".into(),
            state: AptKeyState::Present,
            url: Some("https://x".into()),
            key_data: Some("data".into()),
            fingerprint: None,
            keyring_path: None,
        };
        let err = validate_source_combination(&spec).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("both"), "got: {msg}");
    }

    #[test]
    fn validate_present_no_source_is_invalid_payload() {
        let spec = AptKeySpec {
            name: "x".into(),
            state: AptKeyState::Present,
            url: None,
            key_data: None,
            fingerprint: None,
            keyring_path: None,
        };
        let err = validate_source_combination(&spec).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("requires"), "got: {msg}");
    }

    #[test]
    fn validate_absent_with_url_is_invalid_payload() {
        let spec = AptKeySpec {
            name: "x".into(),
            state: AptKeyState::Absent,
            url: Some("https://x".into()),
            key_data: None,
            fingerprint: None,
            keyring_path: None,
        };
        let err = validate_source_combination(&spec).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    #[test]
    fn compute_diff_present_missing_keyring_returns_install() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("missing.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example.com/k",
            "keyring_path": keyring,
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("install"), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_present_existing_no_fingerprint_returns_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("present.gpg");
        std::fs::write(&keyring, b"fake").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example.com/k",
            "keyring_path": keyring,
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn compute_diff_present_existing_with_fingerprint_returns_update() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("present.gpg");
        std::fs::write(&keyring, b"fake").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example.com/k",
            "fingerprint": "ABCD1234",
            "keyring_path": keyring,
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        // Fingerprint указан — plan делегирует verify в apply, поэтому
        // Update, не NoChange.
        match diff {
            Diff::Update { .. } => {}
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_absent_existing_returns_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("toberemoved.gpg");
        std::fs::write(&keyring, b"fake").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "absent",
            "keyring_path": keyring,
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => {
                assert!(description.contains("remove"), "got: {description}");
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn compute_diff_absent_missing_returns_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("absent.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "absent",
            "keyring_path": keyring,
        }));
        let diff = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn compute_diff_invalid_combination_returns_invalid_payload() {
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://x",
            "key_data": "data",
        }));
        let err = compute_diff(&r, &EmptyFacts, &plan_ctx()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }
}
