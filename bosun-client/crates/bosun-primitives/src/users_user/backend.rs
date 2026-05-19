//! DI-trait для users-примитивов: lookup + useradd/usermod/userdel и
//! параллельный набор для групп. Trait разделён на user-часть (этот файл)
//! и group-часть (`users_group::backend`), оба объединены в общий
//! `UsersBackend` — чтобы передавать один Arc через Primitive struct.
//!
//! Production-реализация — `RealUsersBackend`:
//! - lookup читает /etc/passwd и /etc/group через `nix::unistd::User` /
//!   `nix::unistd::Group` (под капотом `getpwnam_r`/`getgrnam_r`),
//! - mutating-операции вызывают `useradd`/`usermod`/`userdel`/`groupadd`/
//!   `groupmod`/`groupdel` через `std::process::Command`.
//!
//! Тестовая реализация (`tests::MockBackend`) — recorder без побочных
//! эффектов: фиксирует argv и возвращает заранее заданный lookup-снимок.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::users_group::backend::{GroupAddOpts, GroupInfo, GroupModOpts};

/// Снимок существующего системного пользователя. Минимальный набор полей,
/// необходимых для idempotency-проверки в `decide_action_user`.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub primary_gid: u32,
    /// Имя primary group через getgrgid. Если группа есть, но имя
    /// получить не удалось (битый /etc/group), сюда кладётся строка
    /// `gid=<число>` — это всё равно даст стабильное сравнение
    /// при `spec.group = Some(...)`.
    pub primary_group_name: String,
    /// Login shell из /etc/passwd (поле 7).
    pub shell: String,
    /// Home-директория из /etc/passwd (поле 6).
    pub home: PathBuf,
    /// GECOS / comment из /etc/passwd (поле 5).
    pub comment: String,
}

/// Опции `useradd`. Все Option-поля — флаги, которые передаются только
/// если заданы. Имя пользователя — отдельный аргумент, потому что оно
/// обязательное и идёт в конце argv.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct UserAddOpts {
    pub name: String,
    pub uid: Option<u32>,
    pub group: Option<String>,
    pub shell: Option<String>,
    pub home: Option<PathBuf>,
    pub no_create_home: bool,
    pub system: bool,
    pub comment: Option<String>,
}

/// Опции `usermod`. Все Option-поля — флаги, которые передаются только
/// если они в diff'е. Пустой `UserModOpts` (все None) — это вырожденный
/// случай, backend в этом случае не запускает usermod вовсе.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct UserModOpts {
    pub name: String,
    pub uid: Option<u32>,
    pub group: Option<String>,
    pub shell: Option<String>,
    pub home: Option<PathBuf>,
    pub comment: Option<String>,
}

impl UserModOpts {
    /// Возвращает true, если все mutating-поля пусты — usermod звать
    /// нечего. Используется в apply, чтобы не делать пустой exec.
    pub fn is_empty(&self) -> bool {
        self.uid.is_none()
            && self.group.is_none()
            && self.shell.is_none()
            && self.home.is_none()
            && self.comment.is_none()
    }
}

/// Ошибки backend'а. Все они в apply-фазе превращаются в
/// `PrimitiveError::Apply { reason }` через `From`-конверсию или ручной
/// маппинг.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UsersError {
    #[error("lookup failed for {target}: {reason}")]
    Lookup { target: String, reason: String },
    #[error("{tool} not found in PATH")]
    ToolNotFound { tool: String },
    #[error("{tool} exited with status {status}: {stderr_excerpt}")]
    Exec {
        tool: String,
        status: String,
        stderr_excerpt: String,
    },
    #[error("operation requires root (euid != 0)")]
    NotRoot,
    /// Невалидное имя пользователя/группы. Возвращается из
    /// `validate_*_name` ещё до exec'а, чтобы поймать инъекцию через имя.
    #[error("invalid name {name:?}: {reason}")]
    InvalidName { name: String, reason: String },
}

/// Контракт users-backend'а. Разделение на «чтение» (`lookup_*`) и
/// «мутацию» (всё остальное) удобно для тестов: lookup-snapshot можно
/// подменить даже без полного mock'а CRUD-операций.
pub trait UsersBackend: Send + Sync {
    /// Возвращает снимок существующего пользователя или None.
    fn lookup_user(&self, name: &str) -> Result<Option<UserInfo>, UsersError>;
    /// Возвращает снимок существующей группы или None.
    fn lookup_group(&self, name: &str) -> Result<Option<GroupInfo>, UsersError>;

    /// `useradd ...`. Возвращает Ok(()) на успех (exit=0), Err иначе.
    /// Перед вызовом обязан проверить, что euid=0.
    fn useradd(&self, opts: &UserAddOpts) -> Result<(), UsersError>;
    /// `usermod ...`.
    fn usermod(&self, opts: &UserModOpts) -> Result<(), UsersError>;
    /// `userdel <name>`. Без `--remove`: домашняя директория
    /// сохраняется, см. ADR в `users_user::mod`.
    fn userdel(&self, name: &str) -> Result<(), UsersError>;

    /// `groupadd ...`.
    fn groupadd(&self, opts: &GroupAddOpts) -> Result<(), UsersError>;
    /// `groupmod ...`.
    fn groupmod(&self, opts: &GroupModOpts) -> Result<(), UsersError>;
    /// `groupdel <name>`.
    fn groupdel(&self, name: &str) -> Result<(), UsersError>;
}

/// Production-реализация. Никакого внутреннего состояния — просто
/// инкапсулирует пути к утилитам и вызывает их через
/// `std::process::Command`.
#[derive(Default, Debug, Clone, Copy)]
pub struct RealUsersBackend;

impl RealUsersBackend {
    /// Проверка эффективного UID. Полная цель — отказать в apply на
    /// non-root окружении ещё до exec'а useradd: тот всё равно упадёт с
    /// EPERM, но симметричная и явная ошибка лучше для UX оператора.
    fn require_root(&self) -> Result<(), UsersError> {
        // nix::unistd::geteuid()->Uid; Uid::is_root() возвращает true
        // только когда euid == 0.
        if nix::unistd::geteuid().is_root() {
            Ok(())
        } else {
            Err(UsersError::NotRoot)
        }
    }
}

impl UsersBackend for RealUsersBackend {
    fn lookup_user(&self, name: &str) -> Result<Option<UserInfo>, UsersError> {
        // getpwnam_r через nix. Возвращает Ok(None) если пользователь не
        // найден; Err(...) — только если NSS-backend сломан (EIO).
        let user = nix::unistd::User::from_name(name).map_err(|e| UsersError::Lookup {
            target: name.to_string(),
            reason: format!("getpwnam: {e}"),
        })?;
        let Some(user) = user else {
            return Ok(None);
        };

        // Имя primary-группы. Если /etc/group битый или группа исчезла —
        // подставляем `gid=<n>` для стабильной diff-проверки.
        let group_name = match nix::unistd::Group::from_gid(user.gid) {
            Ok(Some(g)) => g.name,
            _ => format!("gid={}", user.gid.as_raw()),
        };

        Ok(Some(UserInfo {
            name: user.name,
            uid: user.uid.as_raw(),
            primary_gid: user.gid.as_raw(),
            primary_group_name: group_name,
            shell: user.shell.to_string_lossy().into_owned(),
            home: user.dir,
            comment: user.gecos.to_string_lossy().into_owned(),
        }))
    }

    fn lookup_group(&self, name: &str) -> Result<Option<GroupInfo>, UsersError> {
        let group = nix::unistd::Group::from_name(name).map_err(|e| UsersError::Lookup {
            target: name.to_string(),
            reason: format!("getgrnam: {e}"),
        })?;
        Ok(group.map(|g| GroupInfo {
            name: g.name,
            gid: g.gid.as_raw(),
        }))
    }

    fn useradd(&self, opts: &UserAddOpts) -> Result<(), UsersError> {
        self.require_root()?;
        let mut argv: Vec<String> = vec!["useradd".to_string()];
        if opts.system {
            argv.push("--system".into());
        }
        if let Some(uid) = opts.uid {
            argv.push("--uid".into());
            argv.push(uid.to_string());
        }
        if let Some(ref group) = opts.group {
            argv.push("--gid".into());
            argv.push(group.clone());
        }
        if let Some(ref shell) = opts.shell {
            argv.push("--shell".into());
            argv.push(shell.clone());
        }
        if let Some(ref home) = opts.home {
            argv.push("--home-dir".into());
            argv.push(home.to_string_lossy().into_owned());
        }
        if opts.no_create_home {
            argv.push("--no-create-home".into());
        }
        if let Some(ref c) = opts.comment {
            argv.push("--comment".into());
            argv.push(c.clone());
        }
        argv.push(opts.name.clone());
        run_command(&argv)
    }

    fn usermod(&self, opts: &UserModOpts) -> Result<(), UsersError> {
        self.require_root()?;
        if opts.is_empty() {
            // Defensive: apply не должен звать пустой usermod, но если
            // вызвали — это no-op, без exec'а.
            return Ok(());
        }
        let mut argv: Vec<String> = vec!["usermod".into()];
        if let Some(uid) = opts.uid {
            argv.push("--uid".into());
            argv.push(uid.to_string());
        }
        if let Some(ref group) = opts.group {
            argv.push("--gid".into());
            argv.push(group.clone());
        }
        if let Some(ref shell) = opts.shell {
            argv.push("--shell".into());
            argv.push(shell.clone());
        }
        if let Some(ref home) = opts.home {
            argv.push("--home".into());
            argv.push(home.to_string_lossy().into_owned());
        }
        if let Some(ref c) = opts.comment {
            argv.push("--comment".into());
            argv.push(c.clone());
        }
        argv.push(opts.name.clone());
        run_command(&argv)
    }

    fn userdel(&self, name: &str) -> Result<(), UsersError> {
        self.require_root()?;
        let argv = vec!["userdel".to_string(), name.to_string()];
        run_command(&argv)
    }

    fn groupadd(&self, opts: &GroupAddOpts) -> Result<(), UsersError> {
        self.require_root()?;
        let mut argv: Vec<String> = vec!["groupadd".into()];
        if opts.system {
            argv.push("--system".into());
        }
        if let Some(gid) = opts.gid {
            argv.push("--gid".into());
            argv.push(gid.to_string());
        }
        argv.push(opts.name.clone());
        run_command(&argv)
    }

    fn groupmod(&self, opts: &GroupModOpts) -> Result<(), UsersError> {
        self.require_root()?;
        let mut argv: Vec<String> = vec!["groupmod".into()];
        let Some(gid) = opts.gid else {
            // Если gid не задан — менять нечего, no-op без exec'а.
            return Ok(());
        };
        argv.push("--gid".into());
        argv.push(gid.to_string());
        argv.push(opts.name.clone());
        run_command(&argv)
    }

    fn groupdel(&self, name: &str) -> Result<(), UsersError> {
        self.require_root()?;
        let argv = vec!["groupdel".to_string(), name.to_string()];
        run_command(&argv)
    }
}

/// Запустить argv через std::process::Command, синхронно дождаться exit'а.
/// Возвращает Ok(()) только при exit=0; иначе Err с фрагментом stderr для
/// post-mortem. Никаких shell, никаких string-templating'ов — все элементы
/// argv передаются как отдельные параметры execve.
fn run_command(argv: &[String]) -> Result<(), UsersError> {
    let Some((cmd, rest)) = argv.split_first() else {
        return Err(UsersError::Exec {
            tool: "<empty>".to_string(),
            status: "argv is empty".to_string(),
            stderr_excerpt: String::new(),
        });
    };
    let output = Command::new(cmd)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            // io::ErrorKind::NotFound — характерный признак того, что
            // useradd/groupadd не установлен (минималистичный chroot).
            if e.kind() == std::io::ErrorKind::NotFound {
                UsersError::ToolNotFound {
                    tool: cmd.to_string(),
                }
            } else {
                UsersError::Exec {
                    tool: cmd.to_string(),
                    status: format!("spawn: {e}"),
                    stderr_excerpt: String::new(),
                }
            }
        })?;

    if output.status.success() {
        return Ok(());
    }
    // Возвращаем первые ~512 байт stderr — этого хватает для diagnosis,
    // полный лог попадёт в log_dir отдельным шагом примитива.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let excerpt = stderr.chars().take(512).collect::<String>();
    Err(UsersError::Exec {
        tool: cmd.to_string(),
        status: format!("{}", output.status),
        stderr_excerpt: excerpt,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn usermod_opts_empty_detected() {
        let opts = UserModOpts {
            name: "x".into(),
            ..Default::default()
        };
        assert!(opts.is_empty());
    }

    #[test]
    fn usermod_opts_non_empty_when_uid_set() {
        let opts = UserModOpts {
            name: "x".into(),
            uid: Some(42),
            ..Default::default()
        };
        assert!(!opts.is_empty());
    }

    #[test]
    fn usermod_opts_non_empty_when_shell_set() {
        let opts = UserModOpts {
            name: "x".into(),
            shell: Some("/bin/false".into()),
            ..Default::default()
        };
        assert!(!opts.is_empty());
    }

    /// Smoke: RealUsersBackend должен находить root'а на любой нормальной
    /// системе и возвращать uid=0. Если /etc/passwd сломан — это
    /// валидный сигнал «среда сломана», тест честно упадёт.
    #[test]
    fn real_backend_finds_root() {
        let backend = RealUsersBackend;
        let info = backend.lookup_user("root").unwrap();
        let info = info.expect_unwrap_or_panic("root should exist");
        assert_eq!(info.uid, 0, "root must have uid=0, got {}", info.uid);
        assert_eq!(info.name, "root");
    }

    /// Аналогично для группы root.
    #[test]
    fn real_backend_finds_root_group() {
        let backend = RealUsersBackend;
        let info = backend.lookup_group("root").unwrap();
        let info = info.expect_unwrap_or_panic("root group should exist");
        assert_eq!(info.gid, 0);
        assert_eq!(info.name, "root");
    }

    #[test]
    fn real_backend_lookup_missing_user_returns_none() {
        let backend = RealUsersBackend;
        let res = backend.lookup_user("definitely-no-such-user-bosun-2026");
        assert!(matches!(res, Ok(None)));
    }

    #[test]
    fn real_backend_lookup_missing_group_returns_none() {
        let backend = RealUsersBackend;
        let res = backend.lookup_group("definitely-no-such-group-bosun-2026");
        assert!(matches!(res, Ok(None)));
    }

    /// Без root'а useradd обязан вернуть NotRoot до exec'а. Запуск под
    /// root в CI заблокирует этот ассерт, но в нормальной dev-среде
    /// (uid != 0) тест защищает от случайного снятия root-check'а.
    #[test]
    fn real_backend_useradd_requires_root() {
        if nix::unistd::geteuid().is_root() {
            // Под root — пропускаем, ассерт не имеет смысла.
            return;
        }
        let backend = RealUsersBackend;
        let opts = UserAddOpts {
            name: "bosun-test-deny".into(),
            ..Default::default()
        };
        let err = backend.useradd(&opts).unwrap_err();
        assert!(matches!(err, UsersError::NotRoot), "got: {err:?}");
    }

    #[test]
    fn real_backend_groupadd_requires_root() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let backend = RealUsersBackend;
        let opts = GroupAddOpts {
            name: "bosun-test-deny".into(),
            ..Default::default()
        };
        let err = backend.groupadd(&opts).unwrap_err();
        assert!(matches!(err, UsersError::NotRoot));
    }

    /// Локальный helper для тестов: явный panic-message без expect/unwrap
    /// в production-коде (Option::expect разрешён в #[cfg(test)] через
    /// allow выше, но удобнее иметь свой именованный путь, чтобы тест
    /// failure был понятен).
    trait OptionTestExt<T> {
        fn expect_unwrap_or_panic(self, msg: &str) -> T;
    }

    impl<T> OptionTestExt<T> for Option<T> {
        fn expect_unwrap_or_panic(self, msg: &str) -> T {
            match self {
                Some(v) => v,
                None => panic!("{msg}"),
            }
        }
    }
}
