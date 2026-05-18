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
#[derive(Clone, Debug, Serialize)]
pub struct ChangeReport {
    pub changed: bool,
    pub message: String,
}

impl ChangeReport {
    pub fn no_change() -> Self {
        Self {
            changed: false,
            message: String::new(),
        }
    }

    pub fn changed(message: impl Into<String>) -> Self {
        Self {
            changed: true,
            message: message.into(),
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
        assert!(nc.message.is_empty());

        let ch = ChangeReport::changed("installed");
        assert!(ch.changed);
        assert_eq!(ch.message, "installed");
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
