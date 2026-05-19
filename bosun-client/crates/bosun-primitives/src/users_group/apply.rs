//! Apply-фаза `users.group`. Симметрично `users_user::apply`.

use std::sync::Arc;

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use crate::users_user::apply::users_error_to_primitive;
use crate::users_user::UsersBackend;

use super::backend::{GroupAddOpts, GroupModOpts};
use super::plan::{decide_action_group, Action};
use super::spec::GroupSpec;

/// Максимальная длина group name. Совпадает с username.
const MAX_NAME_LEN: usize = 32;

/// Валидация group name. Те же правила, что у user'а: `^[a-z_][a-z0-9_-]*$`,
/// до 32 символов.
pub fn validate_group_name(name: &str) -> Result<(), PrimitiveError> {
    if name.is_empty() {
        return Err(PrimitiveError::InvalidPayload(
            "users.group: name is empty".to_string(),
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(PrimitiveError::InvalidPayload(format!(
            "users.group: name {name:?} length {} > {MAX_NAME_LEN}",
            name.len(),
        )));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(PrimitiveError::InvalidPayload(
            "users.group: name is empty".to_string(),
        ));
    };
    let first_ok = first.is_ascii_lowercase() || first == '_';
    if !first_ok {
        return Err(PrimitiveError::InvalidPayload(format!(
            "users.group: name {name:?} must start with [a-z_]",
        )));
    }
    for ch in chars {
        let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-';
        if !ok {
            return Err(PrimitiveError::InvalidPayload(format!(
                "users.group: name {name:?} contains invalid character {ch:?}",
            )));
        }
    }
    Ok(())
}

pub fn run(
    resource: &Resource,
    diff: &Diff,
    _ctx: &ApplyCtx,
    backend: &Arc<dyn UsersBackend>,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: GroupSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("users.group payload: {e}")))?;

    validate_group_name(&spec.name)?;

    let current = backend
        .lookup_group(&spec.name)
        .map_err(users_error_to_primitive)?;

    match decide_action_group(&spec, current.as_ref()) {
        Action::NoChange => Ok(ChangeReport::no_change()),
        Action::Create => {
            let opts = GroupAddOpts {
                name: spec.name.clone(),
                gid: spec.gid,
                system: spec.system,
            };
            tracing::info!(name = %spec.name, gid = ?spec.gid, "users.group: groupadd");
            backend.groupadd(&opts).map_err(users_error_to_primitive)?;
            Ok(ChangeReport::changed(format!(
                "created group {}",
                spec.name
            )))
        }
        Action::Update => {
            let opts = GroupModOpts {
                name: spec.name.clone(),
                gid: spec.gid,
            };
            tracing::info!(name = %spec.name, gid = ?spec.gid, "users.group: groupmod");
            backend.groupmod(&opts).map_err(users_error_to_primitive)?;
            Ok(ChangeReport::changed(format!(
                "updated group {}: gid -> {}",
                spec.name,
                spec.gid
                    .map(|g| g.to_string())
                    .unwrap_or_else(|| "<unset>".into()),
            )))
        }
        Action::Delete => {
            tracing::info!(name = %spec.name, "users.group: groupdel");
            backend
                .groupdel(&spec.name)
                .map_err(users_error_to_primitive)?;
            Ok(ChangeReport::changed(format!(
                "deleted group {}",
                spec.name
            )))
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

    use crate::users_user::{UserAddOpts, UserInfo, UserModOpts, UsersError};

    use super::super::backend::GroupInfo;
    use super::*;

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
        group_snapshot: Mutex<Option<GroupInfo>>,
        calls: Mutex<CallLog>,
    }

    impl MockBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                group_snapshot: Mutex::new(None),
                calls: Mutex::new(CallLog::default()),
            })
        }
        fn set_group(self: &Arc<Self>, info: GroupInfo) {
            *self.group_snapshot.lock().unwrap() = Some(info);
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
            Ok(None)
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
        let kind = ResourceKind::from_static("users.group");
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

    // -- validate_group_name -------------------------------------------------

    #[test]
    fn validate_group_name_accepts_typical() {
        for n in ["postgres", "users", "_apt", "g-1"] {
            validate_group_name(n).unwrap_or_else(|e| panic!("should accept {n}: {e}"));
        }
    }

    #[test]
    fn validate_group_name_rejects_leading_dash() {
        assert!(matches!(
            validate_group_name("-rf").unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn validate_group_name_rejects_empty() {
        assert!(matches!(
            validate_group_name("").unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn validate_group_name_rejects_too_long() {
        let s = "a".repeat(33);
        assert!(matches!(
            validate_group_name(&s).unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    // -- apply: Create -------------------------------------------------------

    #[test]
    fn apply_present_missing_group_calls_groupadd() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
            "gid": 5432,
            "system": false,
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(report.changed);
        let calls = backend.calls();
        assert_eq!(calls.groupadd.len(), 1);
        let opts = &calls.groupadd[0];
        assert_eq!(opts.name, "postgres");
        assert_eq!(opts.gid, Some(5432));
        assert!(!opts.system);
        assert!(calls.groupmod.is_empty());
    }

    #[test]
    fn apply_present_system_flag_propagates_to_groupadd() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "bosun-sys",
            "state": "present",
            "system": true,
        }));
        let (_tmp, ctx) = make_ctx();
        let _ = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        let calls = backend.calls();
        assert!(calls.groupadd[0].system);
    }

    // -- apply: Update -------------------------------------------------------

    #[test]
    fn apply_present_with_gid_drift_calls_groupmod() {
        let backend = MockBackend::new();
        backend.set_group(GroupInfo {
            name: "postgres".into(),
            gid: 1000,
        });
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
            "gid": 5432,
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(report.changed);
        let calls = backend.calls();
        assert_eq!(calls.groupmod.len(), 1);
        assert_eq!(calls.groupmod[0].name, "postgres");
        assert_eq!(calls.groupmod[0].gid, Some(5432));
        assert!(calls.groupadd.is_empty());
    }

    // -- apply: Delete -------------------------------------------------------

    #[test]
    fn apply_absent_existing_group_calls_groupdel() {
        let backend = MockBackend::new();
        backend.set_group(GroupInfo {
            name: "postgres".into(),
            gid: 5432,
        });
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "absent",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(report.changed);
        let calls = backend.calls();
        assert_eq!(calls.groupdel, vec!["postgres".to_string()]);
    }

    // -- apply: idempotency --------------------------------------------------

    #[test]
    fn apply_present_with_matching_gid_is_no_change() {
        let backend = MockBackend::new();
        backend.set_group(GroupInfo {
            name: "postgres".into(),
            gid: 5432,
        });
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
            "gid": 5432,
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap();
        assert!(!report.changed);
        let calls = backend.calls();
        assert!(calls.groupadd.is_empty());
        assert!(calls.groupmod.is_empty());
    }

    #[test]
    fn apply_absent_missing_group_is_no_change() {
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
    fn apply_invalid_name_returns_invalid_payload() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "-evil",
            "state": "present",
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &backend.clone().as_arc()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
        assert!(backend.calls().groupadd.is_empty());
    }

    #[test]
    fn apply_no_change_diff_short_circuits() {
        let backend = MockBackend::new();
        let r = make_resource(serde_json::json!({
            "name": "postgres",
            "state": "present",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &Diff::NoChange, &ctx, &backend.clone().as_arc()).unwrap();
        assert!(!report.changed);
    }
}
