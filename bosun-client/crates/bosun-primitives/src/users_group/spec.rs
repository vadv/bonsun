//! Spec примитива `users.group`.

use serde::Deserialize;

/// Желаемое состояние системной группы. Симметрично `UserState`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GroupState {
    Present,
    Absent,
}

/// Spec примитива `users.group`. Семантика — узкая: «группа должна
/// существовать с таким-то GID или вообще не существовать».
#[derive(Clone, Debug, Deserialize)]
pub struct GroupSpec {
    /// UNIX group name. Регулярка как у username:
    /// `^[a-z_][a-z0-9_-]*$`, длина 1..=32. Валидация — на этапе apply
    /// через `validate_group_name`.
    pub name: String,
    /// Целевое состояние.
    pub state: GroupState,
    /// Фиксированный GID. Если `None` — groupadd выделит автоматически.
    /// При расхождении с реальным GID — `groupmod --gid`.
    #[serde(default)]
    pub gid: Option<u32>,
    /// `groupadd --system` — выделить GID из системного диапазона. Влияет
    /// только в момент создания.
    #[serde(default)]
    pub system: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum() {
        let json = serde_json::json!({"name": "postgres", "state": "present"});
        let spec: GroupSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "postgres");
        assert_eq!(spec.state, GroupState::Present);
        assert!(spec.gid.is_none());
        assert!(!spec.system);
    }

    #[test]
    fn deserialize_all_fields() {
        let json = serde_json::json!({
            "name": "postgres",
            "state": "present",
            "gid": 5432,
            "system": true,
        });
        let spec: GroupSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.gid, Some(5432));
        assert!(spec.system);
    }

    #[test]
    fn deserialize_absent() {
        let json = serde_json::json!({"name": "postgres", "state": "absent"});
        let spec: GroupSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.state, GroupState::Absent);
    }

    #[test]
    fn deserialize_unknown_state_is_error() {
        let json = serde_json::json!({"name": "postgres", "state": "vanished"});
        let err = serde_json::from_value::<GroupSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }
}
