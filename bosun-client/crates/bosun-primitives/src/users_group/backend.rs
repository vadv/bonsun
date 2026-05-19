//! Group-side types для users-backend'а. Сам trait `UsersBackend` живёт в
//! `users_user::backend` и объявляет операции и для пользователей, и для
//! групп — это даёт один общий Arc для обоих примитивов.

/// Снимок существующей системной группы. GID + имя достаточно для
/// idempotency-проверки в `decide_action_group`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GroupInfo {
    pub name: String,
    pub gid: u32,
}

/// Опции `groupadd`. `--system` и `--gid` — единственные флаги, которые
/// мы экспонируем; всё остальное (например, `--password`) сознательно не
/// поддерживаем.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct GroupAddOpts {
    pub name: String,
    pub gid: Option<u32>,
    pub system: bool,
}

/// Опции `groupmod`. На текущем этапе из изменяемых полей у группы
/// поддерживается только GID. Rename (`--new-name`) не нужен: в bosun
/// идентификатор ресурса — имя, переименование = удалить+создать.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct GroupModOpts {
    pub name: String,
    pub gid: Option<u32>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn group_info_eq_by_value() {
        let a = GroupInfo {
            name: "postgres".into(),
            gid: 5432,
        };
        let b = GroupInfo {
            name: "postgres".into(),
            gid: 5432,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn group_add_opts_default_is_minimal() {
        let opts = GroupAddOpts {
            name: "postgres".into(),
            ..Default::default()
        };
        assert!(opts.gid.is_none());
        assert!(!opts.system);
    }
}
