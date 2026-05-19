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
        let argv = build_useradd_argv(opts);
        run_command(&argv)
    }

    fn usermod(&self, opts: &UserModOpts) -> Result<(), UsersError> {
        self.require_root()?;
        if opts.is_empty() {
            // Defensive: apply не должен звать пустой usermod, но если
            // вызвали — это no-op, без exec'а.
            return Ok(());
        }
        let argv = build_usermod_argv(opts);
        run_command(&argv)
    }

    fn userdel(&self, name: &str) -> Result<(), UsersError> {
        self.require_root()?;
        let argv = build_userdel_argv(name);
        run_command(&argv)
    }

    fn groupadd(&self, opts: &GroupAddOpts) -> Result<(), UsersError> {
        self.require_root()?;
        let argv = build_groupadd_argv(opts);
        run_command(&argv)
    }

    fn groupmod(&self, opts: &GroupModOpts) -> Result<(), UsersError> {
        self.require_root()?;
        if opts.gid.is_none() {
            // Если gid не задан — менять нечего, no-op без exec'а.
            return Ok(());
        }
        let argv = build_groupmod_argv(opts);
        run_command(&argv)
    }

    fn groupdel(&self, name: &str) -> Result<(), UsersError> {
        self.require_root()?;
        let argv = build_groupdel_argv(name);
        run_command(&argv)
    }
}

/// Сборка argv для `useradd`. Каждый Option-флаг добавляется только если
/// задан; имя пользователя идёт последним аргументом. Выделена отдельной
/// функцией, чтобы покрыть direct-тестами условные ветки без spawn'а.
pub(crate) fn build_useradd_argv(opts: &UserAddOpts) -> Vec<String> {
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
    argv
}

/// Сборка argv для `usermod`. Caller обязан проверить `opts.is_empty()`
/// перед вызовом — пустой `usermod` тут не отсекается, чтобы builder
/// оставался чистой функцией без скрытого no-op результата.
pub(crate) fn build_usermod_argv(opts: &UserModOpts) -> Vec<String> {
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
    argv
}

/// Сборка argv для `userdel`. По умолчанию домашняя директория
/// сохраняется (см. ADR в `users_user::mod`); trait `UsersBackend`
/// сейчас не экспонирует флаг `--remove`, удаление файлов — отдельный
/// шаг bundle'а через `file.delete`.
pub(crate) fn build_userdel_argv(name: &str) -> Vec<String> {
    vec!["userdel".to_string(), name.to_string()]
}

/// Сборка argv для `groupadd`. `--system` и `--gid` — единственные
/// флаги, экспонируемые на trait-уровне; остальные опции `groupadd(8)`
/// (например, `--password`) сознательно не поддерживаются.
pub(crate) fn build_groupadd_argv(opts: &GroupAddOpts) -> Vec<String> {
    let mut argv: Vec<String> = vec!["groupadd".into()];
    if opts.system {
        argv.push("--system".into());
    }
    if let Some(gid) = opts.gid {
        argv.push("--gid".into());
        argv.push(gid.to_string());
    }
    argv.push(opts.name.clone());
    argv
}

/// Сборка argv для `groupmod`. Caller обязан проверить, что `opts.gid`
/// задан — пустой `groupmod` без `--gid` здесь не отсекается, чтобы
/// builder оставался чистым.
pub(crate) fn build_groupmod_argv(opts: &GroupModOpts) -> Vec<String> {
    let mut argv: Vec<String> = vec!["groupmod".into()];
    if let Some(gid) = opts.gid {
        argv.push("--gid".into());
        argv.push(gid.to_string());
    }
    argv.push(opts.name.clone());
    argv
}

/// Сборка argv для `groupdel`. Имя группы — единственный аргумент.
pub(crate) fn build_groupdel_argv(name: &str) -> Vec<String> {
    vec!["groupdel".to_string(), name.to_string()]
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

    // ---- builder tests: useradd ----

    #[test]
    fn build_useradd_argv_minimum() {
        // Минимальный набор: только имя пользователя без флагов.
        let opts = UserAddOpts {
            name: "alice".into(),
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        assert_eq!(argv, vec!["useradd".to_string(), "alice".into()]);
    }

    #[test]
    fn build_useradd_argv_with_system() {
        // system=true → флаг --system добавлен сразу после имени бинаря.
        let opts = UserAddOpts {
            name: "alice".into(),
            system: true,
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        assert_eq!(argv[0], "useradd");
        assert!(
            argv.iter().any(|a| a == "--system"),
            "--system must be present, got argv={argv:?}",
        );
        // Имя пользователя — последний аргумент.
        assert_eq!(argv.last().unwrap(), "alice");
    }

    #[test]
    fn build_useradd_argv_with_uid() {
        // uid=5432 → "--uid 5432".
        let opts = UserAddOpts {
            name: "alice".into(),
            uid: Some(5432),
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        let uid_pos = argv.iter().position(|a| a == "--uid").unwrap();
        assert_eq!(argv[uid_pos + 1], "5432");
    }

    #[test]
    fn build_useradd_argv_with_group() {
        // group=postgres → "--gid postgres".
        let opts = UserAddOpts {
            name: "alice".into(),
            group: Some("postgres".into()),
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        let gid_pos = argv.iter().position(|a| a == "--gid").unwrap();
        assert_eq!(argv[gid_pos + 1], "postgres");
    }

    #[test]
    fn build_useradd_argv_with_shell() {
        // shell=/bin/false → "--shell /bin/false".
        let opts = UserAddOpts {
            name: "alice".into(),
            shell: Some("/bin/false".into()),
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        let pos = argv.iter().position(|a| a == "--shell").unwrap();
        assert_eq!(argv[pos + 1], "/bin/false");
    }

    #[test]
    fn build_useradd_argv_with_home() {
        // home=/var/lib/postgres → "--home-dir /var/lib/postgres".
        let opts = UserAddOpts {
            name: "alice".into(),
            home: Some(PathBuf::from("/var/lib/postgres")),
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        let pos = argv.iter().position(|a| a == "--home-dir").unwrap();
        assert_eq!(argv[pos + 1], "/var/lib/postgres");
    }

    #[test]
    fn build_useradd_argv_with_no_create_home() {
        // no_create_home=true → "--no-create-home".
        let opts = UserAddOpts {
            name: "alice".into(),
            no_create_home: true,
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        assert!(
            argv.iter().any(|a| a == "--no-create-home"),
            "--no-create-home must be present, got argv={argv:?}",
        );
    }

    #[test]
    fn build_useradd_argv_with_comment() {
        // comment задан → "--comment 'X'". Содержимое уходит одним
        // элементом argv (без shell-escaping'а).
        let opts = UserAddOpts {
            name: "alice".into(),
            comment: Some("postgres role".into()),
            ..Default::default()
        };
        let argv = build_useradd_argv(&opts);
        let pos = argv.iter().position(|a| a == "--comment").unwrap();
        assert_eq!(argv[pos + 1], "postgres role");
    }

    #[test]
    fn build_useradd_argv_full_combo_keeps_name_last() {
        // Все опции вместе — имя всё ещё в самом конце.
        let opts = UserAddOpts {
            name: "postgres".into(),
            uid: Some(5432),
            group: Some("postgres".into()),
            shell: Some("/bin/bash".into()),
            home: Some(PathBuf::from("/var/lib/postgres")),
            no_create_home: false,
            system: true,
            comment: Some("PostgreSQL admin".into()),
        };
        let argv = build_useradd_argv(&opts);
        assert_eq!(argv.last().unwrap(), "postgres");
        assert_eq!(argv[0], "useradd");
    }

    // ---- builder tests: usermod ----

    #[test]
    fn build_usermod_argv_changes_shell() {
        // shell=new → "--shell new" в argv.
        let opts = UserModOpts {
            name: "alice".into(),
            shell: Some("/bin/zsh".into()),
            ..Default::default()
        };
        let argv = build_usermod_argv(&opts);
        let pos = argv.iter().position(|a| a == "--shell").unwrap();
        assert_eq!(argv[pos + 1], "/bin/zsh");
        // Имя пользователя в конце.
        assert_eq!(argv.last().unwrap(), "alice");
        assert_eq!(argv[0], "usermod");
    }

    #[test]
    fn build_usermod_argv_changes_uid_and_group() {
        // uid и group вместе → оба флага присутствуют в argv.
        let opts = UserModOpts {
            name: "alice".into(),
            uid: Some(1001),
            group: Some("staff".into()),
            ..Default::default()
        };
        let argv = build_usermod_argv(&opts);
        assert!(argv.contains(&"--uid".to_string()));
        assert!(argv.contains(&"--gid".to_string()));
        assert!(argv.contains(&"1001".to_string()));
        assert!(argv.contains(&"staff".to_string()));
    }

    // ---- builder tests: userdel ----

    #[test]
    fn build_userdel_argv_minimum() {
        // userdel без флагов: ["userdel", "name"].
        let argv = build_userdel_argv("alice");
        assert_eq!(argv, vec!["userdel".to_string(), "alice".into()]);
    }

    // ---- builder tests: groupadd ----

    #[test]
    fn build_groupadd_argv_minimum() {
        // Только имя группы.
        let opts = GroupAddOpts {
            name: "postgres".into(),
            ..Default::default()
        };
        let argv = build_groupadd_argv(&opts);
        assert_eq!(argv, vec!["groupadd".to_string(), "postgres".into()]);
    }

    #[test]
    fn build_groupadd_argv_with_gid() {
        // gid=5432 → "--gid 5432".
        let opts = GroupAddOpts {
            name: "postgres".into(),
            gid: Some(5432),
            ..Default::default()
        };
        let argv = build_groupadd_argv(&opts);
        let pos = argv.iter().position(|a| a == "--gid").unwrap();
        assert_eq!(argv[pos + 1], "5432");
        assert_eq!(argv.last().unwrap(), "postgres");
    }

    #[test]
    fn build_groupadd_argv_with_system() {
        // system=true → "--system".
        let opts = GroupAddOpts {
            name: "postgres".into(),
            system: true,
            ..Default::default()
        };
        let argv = build_groupadd_argv(&opts);
        assert!(argv.contains(&"--system".to_string()));
        assert_eq!(argv.last().unwrap(), "postgres");
    }

    // ---- builder tests: groupmod ----

    #[test]
    fn build_groupmod_argv_gid() {
        // gid задан → "--gid N" в argv.
        let opts = GroupModOpts {
            name: "postgres".into(),
            gid: Some(5433),
        };
        let argv = build_groupmod_argv(&opts);
        let pos = argv.iter().position(|a| a == "--gid").unwrap();
        assert_eq!(argv[pos + 1], "5433");
        assert_eq!(argv.last().unwrap(), "postgres");
        assert_eq!(argv[0], "groupmod");
    }

    // ---- builder tests: groupdel ----

    #[test]
    fn build_groupdel_argv_minimum() {
        // groupdel без флагов.
        let argv = build_groupdel_argv("postgres");
        assert_eq!(argv, vec!["groupdel".to_string(), "postgres".into()]);
    }
}
