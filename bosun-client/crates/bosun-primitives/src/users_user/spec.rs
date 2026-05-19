//! Десериализуемая часть payload'а `users.user`.
//!
//! Spec лежит в `Resource.payload` после `build_payload` и десериализуется
//! планом/apply'ем через `serde_json::from_value`. Семантика — узкая:
//! «системный пользователь должен существовать с такими-то полями» или
//! «должен отсутствовать». Никаких пользователей с явным интерактивным
//! shell-доступом мы не поощряем — default shell `/bin/false` именно
//! поэтому: типичный postgres/pgbouncer-пользователь не логинится.

use std::path::PathBuf;

use serde::Deserialize;

/// Желаемое состояние системного пользователя.
///
/// Расширение через `Absent` сознательно консервативно: `userdel` без флагов
/// удаляет учётную запись, но НЕ домашнюю директорию. Удаление /home/...
/// — destructive операция, для неё в будущем добавится явный флаг
/// `purge: true`; в MVP мы его не вводим.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UserState {
    /// Пользователь должен существовать. Поля spec'а нормализуются до
    /// текущего состояния через usermod, если отличаются.
    Present,
    /// Пользователь должен отсутствовать. Home не удаляется.
    Absent,
}

/// Spec примитива `users.user`. Все поля кроме `name`/`state` опциональны:
/// `useradd` (Debian) подставит sensible defaults (например, выделит
/// UID/GID из системного диапазона). Если спека задаёт фиксированный
/// `uid`/`group` — apply сравнит их с текущим состоянием и при расхождении
/// выполнит `usermod`.
#[derive(Clone, Debug, Deserialize)]
pub struct UserSpec {
    /// UNIX username. Регулярка POSIX: `^[a-z_][a-z0-9_-]*$`, длина
    /// 1..=32. Валидация — на этапе apply через
    /// [`super::apply::validate_user_name`] (а не на десериализации
    /// payload'а), чтобы тесты могли конструировать «плохие» spec'и для
    /// проверки error-path'а без раздувания UnitName-инфраструктуры.
    pub name: String,
    /// Целевое состояние.
    pub state: UserState,
    /// Фиксированный UID. Если `None` — useradd выделит из системного
    /// диапазона. При расхождении с реальным UID существующего user'а
    /// apply выполнит `usermod --uid`.
    #[serde(default)]
    pub uid: Option<u32>,
    /// Имя primary group. Если `None` — `useradd -U` создаст одноимённую
    /// группу автоматически (Debian-конвенция). При расхождении —
    /// `usermod --gid`.
    #[serde(default)]
    pub group: Option<String>,
    /// Логин-shell. По умолчанию `/bin/false` (см. документацию модуля).
    /// При расхождении — `usermod --shell`.
    #[serde(default)]
    pub shell: Option<String>,
    /// Home-директория. Если `None` — useradd использует
    /// `/home/<name>` (либо `/nonexistent` при `--no-create-home`).
    #[serde(default)]
    pub home: Option<PathBuf>,
    /// Не создавать home-директорию при useradd. Для system-user'ов
    /// типичная практика — chiit постгрес-роли в проде ставят
    /// `NoCreateHome=false`, потому что данные кластера лежат именно
    /// в home.
    #[serde(default)]
    pub no_create_home: bool,
    /// Создать как системного пользователя (`useradd --system`). UID будет
    /// выделен из системного диапазона (обычно <1000). Для существующего
    /// пользователя флаг игнорируется — он влияет только на выделение UID
    /// в момент создания.
    #[serde(default)]
    pub system: bool,
    /// GECOS / комментарий. При расхождении — `usermod --comment`.
    #[serde(default)]
    pub comment: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum_required_fields() {
        let json = serde_json::json!({"name": "postgres", "state": "present"});
        let spec: UserSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "postgres");
        assert_eq!(spec.state, UserState::Present);
        assert!(spec.uid.is_none());
        assert!(spec.group.is_none());
        assert!(spec.shell.is_none());
        assert!(spec.home.is_none());
        assert!(!spec.no_create_home);
        assert!(!spec.system);
        assert!(spec.comment.is_none());
    }

    #[test]
    fn deserialize_all_fields() {
        let json = serde_json::json!({
            "name": "postgres",
            "state": "present",
            "uid": 5432,
            "group": "postgres",
            "shell": "/bin/bash",
            "home": "/var/lib/postgresql",
            "no_create_home": false,
            "system": false,
            "comment": "PostgreSQL administrator",
        });
        let spec: UserSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.uid, Some(5432));
        assert_eq!(spec.group.as_deref(), Some("postgres"));
        assert_eq!(spec.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(spec.home, Some(PathBuf::from("/var/lib/postgresql")));
        assert_eq!(spec.comment.as_deref(), Some("PostgreSQL administrator"));
    }

    #[test]
    fn deserialize_absent_state() {
        let json = serde_json::json!({"name": "postgres", "state": "absent"});
        let spec: UserSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.state, UserState::Absent);
    }

    #[test]
    fn deserialize_unknown_state_is_error() {
        let json = serde_json::json!({"name": "postgres", "state": "vanished"});
        let err = serde_json::from_value::<UserSpec>(json).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "got: {err}");
    }

    #[test]
    fn deserialize_unknown_field_is_silently_ignored() {
        // Serde по умолчанию игнорирует unknown — задача spec'а не падать
        // на лишних полях, а валидация лежит выше (build_payload).
        let json = serde_json::json!({
            "name": "postgres",
            "state": "present",
            "typo_field": 42,
        });
        let spec: UserSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "postgres");
    }
}
