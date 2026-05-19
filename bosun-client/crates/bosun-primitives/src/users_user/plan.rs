//! Plan-фаза `users.user`.
//!
//! Plan возвращает `Update` или `NoChange`: реальное решение Create/Update/
//! Delete принимается в apply через `decide_action_user`, который получает
//! текущий снимок от `UsersBackend::lookup_user`.
//!
//! Причина: чтобы plan не зависел от системного состояния (dry-run на CI
//! без root'а должен работать), мы не зовём lookup до apply. Сам apply
//! сравнит spec с фактом и при совпадении вернёт `ChangeReport::no_change()`.

use bosun_core::{Diff, FactsSource, PlanCtx, PrimitiveError, Resource};

use super::backend::UserInfo;
use super::spec::{UserSpec, UserState};

/// Действие, которое apply должен выполнить с системным пользователем.
/// Стандартная CRUD-матрица:
///
/// | desired \ exists | да                                  | нет        |
/// |------------------|-------------------------------------|------------|
/// | Present          | NoChange (поля совпадают) / Update  | Create     |
/// | Absent           | Delete                              | NoChange   |
///
/// `Update` несёт diff-описание для логов, чтобы оператор видел, какие
/// поля будут перенастроены через usermod.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Action {
    /// Состояние совпадает с желаемым — apply ничего не делает.
    NoChange,
    /// Пользователь должен быть создан через useradd.
    Create,
    /// Пользователь существует, но один или несколько полей отличаются.
    /// Список расхождений нужен и для логов, и для построения argv usermod
    /// (не зовём usermod если флаги нечего менять).
    Update { diffs: Vec<FieldDiff> },
    /// Пользователь существует, должен быть удалён через userdel.
    Delete,
}

/// Описание расхождения одного поля. Только метаданные для логов и для
/// сборки argv usermod; конкретные значения берутся из spec'а в apply.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FieldDiff {
    Uid,
    Group,
    Shell,
    Home,
    Comment,
}

/// Главная decide-таблица. Чистая функция: вход — spec + (опционально)
/// текущий снимок, выход — Action. Никаких side-эффектов, удобно
/// тестировать.
pub fn decide_action_user(spec: &UserSpec, current: Option<&UserInfo>) -> Action {
    match (spec.state, current) {
        (UserState::Present, None) => Action::Create,
        (UserState::Present, Some(info)) => {
            let diffs = collect_diffs(spec, info);
            if diffs.is_empty() {
                Action::NoChange
            } else {
                Action::Update { diffs }
            }
        }
        (UserState::Absent, None) => Action::NoChange,
        (UserState::Absent, Some(_)) => Action::Delete,
    }
}

/// Сравнивает spec с фактом и возвращает список расхождений.
/// Поля, для которых spec задаёт `None` — пропускаются: «не указано»
/// трактуется как «не управляем», а не «должно быть пустым». Это совпадает
/// с поведением chiit (там нулевые поля у структуры User означали «не
/// передавать флаг adduser'у»).
///
/// `group` сравнивается по имени, не по GID: spec несёт имя, и lookup
/// тоже даёт имя через getgrgid.
fn collect_diffs(spec: &UserSpec, info: &UserInfo) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();
    if let Some(uid) = spec.uid {
        if uid != info.uid {
            diffs.push(FieldDiff::Uid);
        }
    }
    if let Some(ref group) = spec.group {
        if group != &info.primary_group_name {
            diffs.push(FieldDiff::Group);
        }
    }
    if let Some(ref shell) = spec.shell {
        if shell != &info.shell {
            diffs.push(FieldDiff::Shell);
        }
    }
    if let Some(ref home) = spec.home {
        if home.as_path() != info.home.as_path() {
            diffs.push(FieldDiff::Home);
        }
    }
    if let Some(ref comment) = spec.comment {
        if comment != &info.comment {
            diffs.push(FieldDiff::Comment);
        }
    }
    diffs
}

pub fn compute_diff(
    resource: &Resource,
    _facts: &dyn FactsSource,
    _ctx: &PlanCtx,
) -> Result<Diff, PrimitiveError> {
    let spec: UserSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user payload: {e}")))?;

    // Ранняя валидация имени, чтобы поймать инъекцию через имя
    // (например, `--bogus-flag`) ещё до apply'я.
    super::apply::validate_user_name(&spec.name)?;

    Ok(Diff::Update {
        from: serde_json::json!({"users.user": "current state pending lookup"}),
        to: resource.payload.clone(),
        description: format!("converge users.user:{}", spec.name),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn base_spec(state: UserState) -> UserSpec {
        UserSpec {
            name: "postgres".to_string(),
            state,
            uid: None,
            group: None,
            shell: None,
            home: None,
            no_create_home: false,
            system: false,
            comment: None,
        }
    }

    fn info() -> UserInfo {
        UserInfo {
            name: "postgres".to_string(),
            uid: 5432,
            primary_gid: 5432,
            primary_group_name: "postgres".to_string(),
            shell: "/bin/bash".to_string(),
            home: PathBuf::from("/var/lib/postgresql"),
            comment: "PostgreSQL administrator".to_string(),
        }
    }

    #[test]
    fn present_and_missing_yields_create() {
        let s = base_spec(UserState::Present);
        assert_eq!(decide_action_user(&s, None), Action::Create);
    }

    #[test]
    fn present_and_matching_all_fields_yields_no_change() {
        let mut s = base_spec(UserState::Present);
        s.uid = Some(5432);
        s.group = Some("postgres".into());
        s.shell = Some("/bin/bash".into());
        s.home = Some(PathBuf::from("/var/lib/postgresql"));
        s.comment = Some("PostgreSQL administrator".into());
        assert_eq!(decide_action_user(&s, Some(&info())), Action::NoChange);
    }

    #[test]
    fn present_with_uid_mismatch_yields_update() {
        let mut s = base_spec(UserState::Present);
        s.uid = Some(9999);
        let action = decide_action_user(&s, Some(&info()));
        match action {
            Action::Update { diffs } => assert_eq!(diffs, vec![FieldDiff::Uid]),
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn present_with_multiple_mismatches_collects_all() {
        let mut s = base_spec(UserState::Present);
        s.uid = Some(9999);
        s.shell = Some("/bin/zsh".into());
        s.home = Some(PathBuf::from("/elsewhere"));
        let action = decide_action_user(&s, Some(&info()));
        match action {
            Action::Update { diffs } => {
                assert!(diffs.contains(&FieldDiff::Uid));
                assert!(diffs.contains(&FieldDiff::Shell));
                assert!(diffs.contains(&FieldDiff::Home));
                assert_eq!(diffs.len(), 3);
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn present_with_none_fields_skips_comparison() {
        // spec задаёт только name+state, всё остальное None.
        // Любые значения в info не должны давать Update.
        let s = base_spec(UserState::Present);
        assert_eq!(decide_action_user(&s, Some(&info())), Action::NoChange);
    }

    #[test]
    fn absent_and_existing_yields_delete() {
        let s = base_spec(UserState::Absent);
        assert_eq!(decide_action_user(&s, Some(&info())), Action::Delete);
    }

    #[test]
    fn absent_and_missing_yields_no_change() {
        let s = base_spec(UserState::Absent);
        assert_eq!(decide_action_user(&s, None), Action::NoChange);
    }

    #[test]
    fn group_compared_by_name_not_gid() {
        let mut s = base_spec(UserState::Present);
        s.group = Some("nogroup".into());
        let action = decide_action_user(&s, Some(&info()));
        match action {
            Action::Update { diffs } => assert_eq!(diffs, vec![FieldDiff::Group]),
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn collect_diffs_shell_only_mismatch_yields_single_diff() {
        // Spec задаёт только shell, отличный от факта: остальные поля spec'а
        // None, поэтому не должны порождать diff. Проверяем, что список ровно
        // [Shell] и порядок зафиксирован.
        let mut s = base_spec(UserState::Present);
        s.shell = Some("/bin/zsh".into());
        let action = decide_action_user(&s, Some(&info()));
        match action {
            Action::Update { diffs } => assert_eq!(diffs, vec![FieldDiff::Shell]),
            other => panic!("expected Update with shell-only diff, got {other:?}"),
        }
    }

    #[test]
    fn collect_diffs_home_only_mismatch_yields_single_diff() {
        let mut s = base_spec(UserState::Present);
        s.home = Some(PathBuf::from("/elsewhere"));
        let action = decide_action_user(&s, Some(&info()));
        match action {
            Action::Update { diffs } => assert_eq!(diffs, vec![FieldDiff::Home]),
            other => panic!("expected Update with home-only diff, got {other:?}"),
        }
    }

    #[test]
    fn collect_diffs_comment_only_mismatch_yields_single_diff() {
        let mut s = base_spec(UserState::Present);
        s.comment = Some("Different comment".into());
        let action = decide_action_user(&s, Some(&info()));
        match action {
            Action::Update { diffs } => assert_eq!(diffs, vec![FieldDiff::Comment]),
            other => panic!("expected Update with comment-only diff, got {other:?}"),
        }
    }

    #[test]
    fn group_with_gid_fallback_name_yields_mismatch_when_spec_uses_name() {
        // Когда /etc/group битый, getgrgid возвращает None → backend подставит
        // "gid=N" вместо имени. spec.group = "postgres" даст Group-mismatch,
        // потому что сравнение идёт по строке. Это намеренно — оператор
        // увидит «несоответствие, нужно чинить /etc/group или GID».
        let mut s = base_spec(UserState::Present);
        s.group = Some("postgres".into());
        let mut bad_info = info();
        bad_info.primary_group_name = "gid=5432".into();
        let action = decide_action_user(&s, Some(&bad_info));
        match action {
            Action::Update { diffs } => assert_eq!(diffs, vec![FieldDiff::Group]),
            other => panic!("expected Update with group-only diff, got {other:?}"),
        }
    }
}
