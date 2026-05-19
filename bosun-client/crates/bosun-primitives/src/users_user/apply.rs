//! Apply-фаза `users.user`.
//!
//! Шаги:
//! 1. Re-десериализовать spec; ранняя валидация имени (защита от
//!    инъекции аргументов через имя).
//! 2. Lookup текущего состояния через `UsersBackend::lookup_user`.
//! 3. `decide_action_user` → Create/Update/Delete/NoChange.
//! 4. На каждый случай — exec соответствующего инструмента через backend.
//!
//! Все exec'и проходят через `UsersBackend`, который в production —
//! `RealUsersBackend` (geteuid-check + std::process::Command), в тестах —
//! mock-recorder. Это даёт unit-тесты, которые не модифицируют систему.

use std::sync::Arc;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::backend::{UserAddOpts, UserModOpts, UsersBackend, UsersError};
use super::plan::{decide_action_user, Action, FieldDiff};
use super::spec::UserSpec;

/// Максимальная длина UNIX-имени. POSIX-2017 + Debian useradd: 32 байта,
/// проверяется на нашей стороне для fail-fast.
const MAX_NAME_LEN: usize = 32;

/// Валидация имени пользователя. Допустимый алфавит совпадает с дефолтным
/// `useradd` regex (`^[a-z_][a-z0-9_-]*$`): первый символ — буква нижнего
/// регистра или `_`, далее — буква, цифра, `_` или `-`. Это блокирует:
/// - инъекцию `--bogus-flag` через имя (первый символ `-`);
/// - попытку имени `root; rm -rf /` (пробелы и `;`);
/// - имена с слэшами/точками, которые часть NSS-backend'ов интерпретирует
///   как поиск в LDAP/AD.
pub fn validate_user_name(name: &str) -> Result<(), PrimitiveError> {
    if name.is_empty() {
        return Err(PrimitiveError::InvalidPayload(
            "users.user: name is empty".to_string(),
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(PrimitiveError::InvalidPayload(format!(
            "users.user: name {name:?} length {} > {MAX_NAME_LEN}",
            name.len(),
        )));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(PrimitiveError::InvalidPayload(
            "users.user: name is empty".to_string(),
        ));
    };
    let first_ok = first.is_ascii_lowercase() || first == '_';
    if !first_ok {
        return Err(PrimitiveError::InvalidPayload(format!(
            "users.user: name {name:?} must start with [a-z_], got {first:?}",
        )));
    }
    for ch in chars {
        let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-';
        if !ok {
            return Err(PrimitiveError::InvalidPayload(format!(
                "users.user: name {name:?} contains invalid character {ch:?}",
            )));
        }
    }
    Ok(())
}

/// Главная entry-point apply. Соответствует контракту `Primitive::apply`,
/// дополнительно принимает `&Arc<dyn UsersBackend>` — DI-точка для тестов.
pub fn run(
    resource: &Resource,
    diff: &Diff,
    _ctx: &ApplyCtx,
    backend: &Arc<dyn UsersBackend>,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: UserSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user payload: {e}")))?;

    validate_user_name(&spec.name)?;

    let current = backend
        .lookup_user(&spec.name)
        .map_err(users_error_to_primitive)?;

    match decide_action_user(&spec, current.as_ref()) {
        Action::NoChange => Ok(ChangeReport::no_change()),
        Action::Create => {
            let opts = UserAddOpts {
                name: spec.name.clone(),
                uid: spec.uid,
                group: spec.group.clone(),
                shell: spec.shell.clone(),
                home: spec.home.clone(),
                no_create_home: spec.no_create_home,
                system: spec.system,
                comment: spec.comment.clone(),
            };
            tracing::info!(
                name = %spec.name,
                uid = ?spec.uid,
                group = ?spec.group,
                "users.user: useradd",
            );
            backend.useradd(&opts).map_err(users_error_to_primitive)?;
            Ok(ChangeReport::changed(format!("created user {}", spec.name)))
        }
        Action::Update { diffs } => {
            let opts = build_usermod_opts(&spec, &diffs);
            tracing::info!(
                name = %spec.name,
                diffs = ?diffs,
                "users.user: usermod",
            );
            backend.usermod(&opts).map_err(users_error_to_primitive)?;
            Ok(ChangeReport::changed(format!(
                "updated user {}: {}",
                spec.name,
                describe_diffs(&diffs),
            )))
        }
        Action::Delete => {
            tracing::info!(name = %spec.name, "users.user: userdel");
            backend
                .userdel(&spec.name)
                .map_err(users_error_to_primitive)?;
            Ok(ChangeReport::changed(format!("deleted user {}", spec.name)))
        }
    }
}

/// Собирает `UserModOpts`, выставляя только те поля, которые реально в
/// diff'е. Поля spec'а без расхождения — None (usermod их не передаст
/// и не вызовет лишнюю операцию).
fn build_usermod_opts(spec: &UserSpec, diffs: &[FieldDiff]) -> UserModOpts {
    let mut opts = UserModOpts {
        name: spec.name.clone(),
        ..Default::default()
    };
    for d in diffs {
        match d {
            FieldDiff::Uid => opts.uid = spec.uid,
            FieldDiff::Group => opts.group = spec.group.clone(),
            FieldDiff::Shell => opts.shell = spec.shell.clone(),
            FieldDiff::Home => opts.home = spec.home.clone(),
            FieldDiff::Comment => opts.comment = spec.comment.clone(),
        }
    }
    opts
}

/// Человеко-читабельное описание набора расхождений для лога.
fn describe_diffs(diffs: &[FieldDiff]) -> String {
    let parts: Vec<&str> = diffs
        .iter()
        .map(|d| match d {
            FieldDiff::Uid => "uid",
            FieldDiff::Group => "group",
            FieldDiff::Shell => "shell",
            FieldDiff::Home => "home",
            FieldDiff::Comment => "comment",
        })
        .collect();
    parts.join(",")
}

/// Маппинг `UsersError` в `PrimitiveError`. Особо различаем `NotRoot`
/// и `ToolNotFound` — это разные сценарии: первый чинится sudo, второй
/// — установкой `passwd`/`shadow` пакета.
pub(crate) fn users_error_to_primitive(err: UsersError) -> PrimitiveError {
    match err {
        UsersError::NotRoot => PrimitiveError::Apply {
            reason: "users primitives require root (euid != 0)".to_string(),
        },
        UsersError::ToolNotFound { tool } => PrimitiveError::Apply {
            reason: format!("{tool} not found in PATH"),
        },
        UsersError::Exec {
            tool,
            status,
            stderr_excerpt,
        } => PrimitiveError::Exec {
            reason: format!("{tool}: {status}"),
            exit: None,
            stderr_excerpt,
        },
        UsersError::Lookup { target, reason } => PrimitiveError::Apply {
            reason: format!("lookup {target}: {reason}"),
        },
        UsersError::InvalidName { name, reason } => {
            PrimitiveError::InvalidPayload(format!("invalid name {name:?}: {reason}"))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtx, ResourceId, ResourceKind, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::users_group::backend::{GroupAddOpts, GroupInfo, GroupModOpts};

    use super::super::backend::UserInfo;
    use super::*;

    /// Записанные вызовы backend'а — для проверки, что apply дёргает
    /// правильные операции с правильными опциями.
    #[derive(Default)]
    struct CallLog {
        useradd: Vec<UserAddOpts>,
        usermod: Vec<UserModOpts>,
        userdel: Vec<String>,
        groupadd: Vec<GroupAddOpts>,
        groupmod: Vec<GroupModOpts>,
        groupdel: Vec<String>,
    }

    struct MockBackend {
        user_snapshot: Mutex<Option<UserInfo>>,
        group_snapshot: Mutex<Option<GroupInfo>>,
        calls: Mutex<CallLog>,
    }

    impl MockBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                user_snapshot: Mutex::new(None),
                group_snapshot: Mutex::new(None),
                calls: Mutex::new(CallLog::default()),
            })
        }
        fn set_user(self: &Arc<Self>, info: UserInfo) {
            *self.user_snapshot.lock().unwrap() = Some(info);
        }
        fn calls(self: &Arc<Self>) -> std::sync::MutexGuard<'_, CallLog> {
            self.calls.lock().unwrap()
        }
        fn as_arc(self: Arc<Self>) -> Arc<dyn UsersBackend> {
            self
        }
    }

    impl UsersBackend for MockBackend {
        fn lookup_user(&self, _: &str) -> Result<Option<UserInfo>, UsersError> {
            Ok(self.user_snapshot.lock().unwrap().clone())
        }
        fn lookup_group(&self, _: &str) -> Result<Option<GroupInfo>, UsersError> {
            Ok(self.group_snapshot.lock().unwrap().clone())
        }
        fn useradd(&self, opts: &UserAddOpts) -> Result<(), UsersError> {
            self.calls.lock().unwrap().useradd.push(opts.clone());
            Ok(())
        }
        fn usermod(&self, opts: &UserModOpts) -> Result<(), UsersError> {
            self.calls.lock().unwrap().usermod.push(opts.clone());
            Ok(())
        }
        fn userdel(&self, name: &str) -> Result<(), UsersError> {
            self.calls.lock().unwrap().userdel.push(name.to_string());
            Ok(())
        }
        fn groupadd(&self, opts: &GroupAddOpts) -> Result<(), UsersError> {
            self.calls.lock().unwrap().groupadd.push(opts.clone());
            Ok(())
        }
        fn groupmod(&self, opts: &GroupModOpts) -> Result<(), UsersError> {
            self.calls.lock().unwrap().groupmod.push(opts.clone());
            Ok(())
        }
        fn groupdel(&self, name: &str) -> Result<(), UsersError> {
            self.calls.lock().unwrap().groupdel.push(name.to_string());
            Ok(())
        }
    }

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("users.user");
        let name = payload["name"].as_str().unwrap_or("test").to_string();
        let id = ResourceId::new(&kind, &name);
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

    fn make_ctx() -> (TempDir, ApplyCtx) {
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            defers,
            None,
            None,
        );
        (tmp, ctx)
    }

    fn force_update_diff(r: &Resource) -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: r.payload.clone(),
            description: "converge".into(),
        }
    }

    // -- validate_user_name --------------------------------------------------

    #[test]
    fn validate_name_accepts_typical_system_users() {
        for n in [
            "postgres",
            "pgbouncer",
            "_apt",
            "user-1",
            "a",
            "abc_def-ghi",
        ] {
            validate_user_name(n).unwrap_or_else(|e| panic!("should accept {n}: {e}"));
        }
    }

    #[test]
    fn validate_name_rejects_leading_dash() {
        let err = validate_user_name("-rf").unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(msg.contains("[a-z_]") || msg.contains("must start with"));
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn validate_name_rejects_uppercase() {
        assert!(matches!(
            validate_user_name("Root").unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn validate_name_rejects_spaces() {
        assert!(matches!(
            validate_user_name("post gres").unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn validate_name_rejects_semicolon() {
        assert!(matches!(
            validate_user_name("root;rm").unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn validate_name_rejects_slash() {
        assert!(matches!(
            validate_user_name("postgres/admin").unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(matches!(
            validate_user_name("").unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn validate_name_rejects_too_long() {
        let too_long = "a".repeat(33);
        assert!(matches!(
            validate_user_name(&too_long).unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    // -- apply: Create -------------------------------------------------------

    #[test]
    fn apply_present_missing_user_calls_useradd() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
            "uid": 5432,
            "group": "postgres",
            "shell": "/bin/bash",
            "home": "/var/lib/postgresql",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(report.changed);
        let calls = backend.calls();
        assert_eq!(calls.useradd.len(), 1);
        let opts = &calls.useradd[0];
        assert_eq!(opts.name, "postgres");
        assert_eq!(opts.uid, Some(5432));
        assert_eq!(opts.group.as_deref(), Some("postgres"));
        assert_eq!(opts.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(opts.home, Some(PathBuf::from("/var/lib/postgresql")));
        assert!(calls.usermod.is_empty());
        assert!(calls.userdel.is_empty());
    }

    // -- apply: Update -------------------------------------------------------

    #[test]
    fn apply_present_with_shell_drift_calls_usermod() {
        let backend = MockBackend::new();
        backend.set_user(UserInfo {
            name: "postgres".into(),
            uid: 5432,
            primary_gid: 5432,
            primary_group_name: "postgres".into(),
            shell: "/bin/false".into(),
            home: PathBuf::from("/var/lib/postgresql"),
            comment: String::new(),
        });
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
            "shell": "/bin/bash",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(report.changed);
        let calls = backend.calls();
        assert_eq!(calls.usermod.len(), 1);
        let opts = &calls.usermod[0];
        assert_eq!(opts.name, "postgres");
        assert_eq!(opts.shell.as_deref(), Some("/bin/bash"));
        // Поля без drift не должны попасть в usermod.
        assert!(opts.uid.is_none());
        assert!(opts.home.is_none());
        assert!(calls.useradd.is_empty());
    }

    // -- apply: Delete -------------------------------------------------------

    #[test]
    fn apply_absent_existing_user_calls_userdel() {
        let backend = MockBackend::new();
        backend.set_user(UserInfo {
            name: "postgres".into(),
            uid: 5432,
            primary_gid: 5432,
            primary_group_name: "postgres".into(),
            shell: "/bin/false".into(),
            home: PathBuf::from("/var/lib/postgresql"),
            comment: String::new(),
        });
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "absent",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(report.changed);
        let calls = backend.calls();
        assert_eq!(calls.userdel, vec!["postgres".to_string()]);
        assert!(calls.useradd.is_empty());
        assert!(calls.usermod.is_empty());
    }

    // -- apply: idempotency --------------------------------------------------

    #[test]
    fn apply_present_with_matching_state_is_no_change() {
        let backend = MockBackend::new();
        backend.set_user(UserInfo {
            name: "postgres".into(),
            uid: 5432,
            primary_gid: 5432,
            primary_group_name: "postgres".into(),
            shell: "/bin/bash".into(),
            home: PathBuf::from("/var/lib/postgresql"),
            comment: String::new(),
        });
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
            "uid": 5432,
            "group": "postgres",
            "shell": "/bin/bash",
            "home": "/var/lib/postgresql",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(!report.changed, "expected no change, got {report:?}");
        let calls = backend.calls();
        assert!(calls.useradd.is_empty());
        assert!(calls.usermod.is_empty());
        assert!(calls.userdel.is_empty());
    }

    #[test]
    fn apply_absent_missing_user_is_no_change() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "ghost",
            "state": "absent",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn apply_no_change_diff_short_circuits_without_lookup() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &Diff::NoChange, &ctx, &backend.clone().as_arc()).unwrap();
        assert!(!report.changed);
        // Lookup даже не дёргался — все списки call_log пусты.
        let calls = backend.calls();
        assert!(calls.useradd.is_empty());
        assert!(calls.usermod.is_empty());
        assert!(calls.userdel.is_empty());
    }

    #[test]
    fn apply_invalid_name_returns_invalid_payload_without_lookup() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "-evil",
            "state": "present",
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
        // Backend не должен быть вызван.
        let calls = backend.calls();
        assert!(calls.useradd.is_empty());
    }

    // -- маппинг ошибок ------------------------------------------------------

    #[test]
    fn map_not_root_to_apply() {
        let err = users_error_to_primitive(UsersError::NotRoot);
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("root"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_not_found_to_apply() {
        let err = users_error_to_primitive(UsersError::ToolNotFound {
            tool: "useradd".into(),
        });
        match err {
            PrimitiveError::Apply { reason } => assert!(reason.contains("useradd")),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn map_exec_failure_to_exec() {
        let err = users_error_to_primitive(UsersError::Exec {
            tool: "useradd".into(),
            status: "exit status: 4".into(),
            stderr_excerpt: "name not unique".into(),
        });
        match err {
            PrimitiveError::Exec {
                reason,
                stderr_excerpt,
                ..
            } => {
                assert!(reason.contains("useradd"));
                assert!(stderr_excerpt.contains("name not unique"));
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }
}
