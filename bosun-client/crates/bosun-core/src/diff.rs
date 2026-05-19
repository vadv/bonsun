use serde::Serialize;

/// Результат plan-фазы примитива.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Diff {
    NoChange,
    Add {
        description: String,
        payload: serde_json::Value,
    },
    Update {
        from: serde_json::Value,
        to: serde_json::Value,
        description: String,
    },
}

impl Diff {
    pub fn is_no_change(&self) -> bool {
        matches!(self, Diff::NoChange)
    }
}

/// Результат apply-фазы примитива (только успех).
/// Ошибка возвращается через Err(PrimitiveError) — двойного канала нет.
///
/// `deferred=true` маркирует случай, когда apply положил действие в журнал
/// defers и вернулся, не дожидаясь выполнения. Оркестратор трактует это
/// как `Outcome::Deferred` (см. `Orchestrator::apply`), summary
/// инкрементируется в `deferred`, а не `changed`. Использование: примитив
/// поставил `ctx.defers.enqueue(...)` и сразу же возвращает
/// `ChangeReport::deferred(reason)`.
#[derive(Clone, Debug, Serialize)]
pub struct ChangeReport {
    pub changed: bool,
    pub message: String,
    /// Действие отложено в журнал defers. `changed=false`, `deferred=true`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred: bool,
}

impl ChangeReport {
    pub fn no_change() -> Self {
        Self {
            changed: false,
            message: String::new(),
            deferred: false,
        }
    }

    pub fn changed(message: impl Into<String>) -> Self {
        Self {
            changed: true,
            message: message.into(),
            deferred: false,
        }
    }

    /// Marker «действие положено в defer-журнал и будет выполнено в
    /// replay». Оркестратор инкрементирует `summary.deferred`, не `changed`.
    pub fn deferred(message: impl Into<String>) -> Self {
        Self {
            changed: false,
            message: message.into(),
            deferred: true,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn no_change_detected() {
        assert!(Diff::NoChange.is_no_change());
        assert!(!Diff::Add {
            description: "x".into(),
            payload: serde_json::json!({})
        }
        .is_no_change());
    }

    #[test]
    fn change_report_factories() {
        let nc = ChangeReport::no_change();
        assert!(!nc.changed);
        assert!(!nc.deferred);
        assert!(nc.message.is_empty());

        let ch = ChangeReport::changed("installed");
        assert!(ch.changed);
        assert!(!ch.deferred);
        assert_eq!(ch.message, "installed");

        let def = ChangeReport::deferred("enqueued restart");
        assert!(!def.changed);
        assert!(def.deferred);
        assert_eq!(def.message, "enqueued restart");
    }

    #[test]
    fn change_report_deferred_omits_field_in_default_json() {
        // skip_serializing_if гарантирует, что обратно-совместимые потребители
        // (читалки старого формата) не споткнутся об лишнее поле для
        // changed/no_change-отчётов.
        let ch = ChangeReport::changed("ok");
        let json = serde_json::to_value(&ch).unwrap();
        assert_eq!(
            json.get("deferred"),
            None,
            "default false должен быть пропущен"
        );

        let def = ChangeReport::deferred("ok");
        let json = serde_json::to_value(&def).unwrap();
        assert_eq!(json["deferred"], serde_json::json!(true));
    }

    #[test]
    fn diff_serializes_to_tagged_json() {
        let diff = Diff::Add {
            description: "install nginx".into(),
            payload: serde_json::json!({"name": "nginx"}),
        };
        let json = serde_json::to_value(&diff).unwrap();
        assert_eq!(json["kind"], "add");
        assert_eq!(json["description"], "install nginx");
    }
}
