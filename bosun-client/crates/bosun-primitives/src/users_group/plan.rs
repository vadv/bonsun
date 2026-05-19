//! Plan-фаза `users.group`. Симметрично `users_user::plan`.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use super::backend::GroupInfo;
use super::spec::{GroupSpec, GroupState};

/// Действие для apply. Стандартная CRUD-матрица.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    NoChange,
    Create,
    /// Существует, GID отличается от spec'а. Поле gid в decide-функции
    /// не хранится — apply возьмёт его из spec'а.
    Update,
    Delete,
}

/// Чистая decide-функция. Сравнивает GID, если spec задаёт.
pub fn decide_action_group(spec: &GroupSpec, current: Option<&GroupInfo>) -> Action {
    match (spec.state, current) {
        (GroupState::Present, None) => Action::Create,
        (GroupState::Present, Some(info)) => match spec.gid {
            Some(gid) if gid != info.gid => Action::Update,
            _ => Action::NoChange,
        },
        (GroupState::Absent, None) => Action::NoChange,
        (GroupState::Absent, Some(_)) => Action::Delete,
    }
}

pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: GroupSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("users.group payload: {e}")))?;

    super::apply::validate_group_name(&spec.name)?;

    Ok(Diff::Update {
        from: serde_json::json!({"users.group": "current state pending lookup"}),
        to: resource.payload.clone(),
        description: format!("converge users.group:{}", spec.name),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn base_spec(state: GroupState) -> GroupSpec {
        GroupSpec {
            name: "postgres".into(),
            state,
            gid: None,
            system: false,
        }
    }

    fn info(gid: u32) -> GroupInfo {
        GroupInfo {
            name: "postgres".into(),
            gid,
        }
    }

    #[test]
    fn present_and_missing_yields_create() {
        assert_eq!(
            decide_action_group(&base_spec(GroupState::Present), None),
            Action::Create,
        );
    }

    #[test]
    fn present_and_matching_gid_yields_no_change() {
        let mut s = base_spec(GroupState::Present);
        s.gid = Some(5432);
        assert_eq!(decide_action_group(&s, Some(&info(5432))), Action::NoChange);
    }

    #[test]
    fn present_with_gid_unspecified_yields_no_change_regardless() {
        // gid в spec'е = None: «не управляем GID-ом», любой GID в info
        // должен давать NoChange.
        let s = base_spec(GroupState::Present);
        assert_eq!(decide_action_group(&s, Some(&info(9999))), Action::NoChange);
    }

    #[test]
    fn present_with_gid_drift_yields_update() {
        let mut s = base_spec(GroupState::Present);
        s.gid = Some(5432);
        assert_eq!(decide_action_group(&s, Some(&info(1234))), Action::Update);
    }

    #[test]
    fn absent_and_existing_yields_delete() {
        let s = base_spec(GroupState::Absent);
        assert_eq!(decide_action_group(&s, Some(&info(5432))), Action::Delete);
    }

    #[test]
    fn absent_and_missing_yields_no_change() {
        let s = base_spec(GroupState::Absent);
        assert_eq!(decide_action_group(&s, None), Action::NoChange);
    }
}
