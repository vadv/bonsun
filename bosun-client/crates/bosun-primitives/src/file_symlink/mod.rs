//! Примитив `file.symlink` — управление симлинком: создать/обновить/удалить.
//!
//! Покрывает chiit-кейсы:
//! - `roles/postgres/install_nix.go:32-36` — ~50 симлинков на pg-бинари из
//!   `/usr/nix/postgres<N>/bin/...` в `/usr/local/bin/`.
//! - `roles/patroni/install.go:24-25` — patroni симлинк.
//! - `roles/runr/chiit.go:64-67` — `systemctl`/`journalctl` → runr-обёртки.
//!
//! Эквивалентен `files.Link(ctx, src, dst)` из
//! `chiit/lib/providers/file/filemanager/link.go`.
//!
//! Внутри:
//! - `spec` — десериализация payload'а в `FileSymlinkSpec` + `SymlinkState`.
//! - `plan` — `decide_action_symlink` по `symlink_metadata`/`read_link`.
//! - `apply` — `std::os::unix::fs::symlink` после unlink (для Update),
//!   идемпотентно.

mod apply;
mod plan;
mod spec;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use plan::{decide_action_symlink, Action};
pub use spec::{FileSymlinkSpec, SymlinkState};

/// Реализация Primitive для `file.symlink`. Stateless.
pub struct FileSymlinkPrimitive;

impl Primitive for FileSymlinkPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("file.symlink")
    }

    fn identity_keys(&self) -> &'static [&'static str] {
        &["path"]
    }

    fn build_payload(
        &self,
        args: &CallArgs,
        _ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError> {
        let path = args
            .required_str("path")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.symlink: {e}")))?;
        let target = args
            .required_str("target")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.symlink: {e}")))?;
        // state — строковый, без него — present по дефолту. Принимаем
        // "present"/"absent", остальные значения серверная десериализация
        // отвергнет при разборе payload'а.
        let state = args
            .optional_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.symlink: {e}")))?
            .unwrap_or_else(|| "present".to_string());
        let force = args
            .optional_bool("force")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.symlink: {e}")))?
            .unwrap_or(false);

        Ok(serde_json::json!({
            "path": path,
            "target": target,
            "state": state,
            "force": force,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        _facts: &dyn FactsSource,
        _ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        let spec: FileSymlinkSpec = serde_json::from_value(resource.payload.clone())
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.symlink payload: {e}")))?;
        spec.validate()?;

        let action = decide_action_symlink(&spec, &spec.path)?;
        Ok(match action {
            Action::NoChange => Diff::NoChange,
            Action::Create => Diff::Add {
                description: format!("create symlink {} -> {}", spec.path.display(), spec.target),
                payload: resource.payload.clone(),
            },
            Action::Update => Diff::Update {
                from: serde_json::json!({"path": spec.path.to_string_lossy()}),
                to: serde_json::json!({"path": spec.path.to_string_lossy(), "target": spec.target}),
                description: format!("update symlink {} -> {}", spec.path.display(), spec.target),
            },
            Action::Delete => Diff::Update {
                from: serde_json::json!({"path": spec.path.to_string_lossy(), "exists": true}),
                to: serde_json::json!({"path": spec.path.to_string_lossy(), "exists": false}),
                description: format!("delete symlink {}", spec.path.display()),
            },
        })
    }

    fn apply(
        &self,
        resource: &Resource,
        diff: &Diff,
        ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError> {
        apply::apply(resource, diff, ctx)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use bosun_core::{ArgValue, FactValue, PlanCtx, Resource, ResourceId};
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct NoFacts;
    impl FactsSource for NoFacts {
        fn get(&self, _: &str) -> FactValue {
            FactValue::Unknown {
                reason: "test".into(),
            }
        }
    }

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    fn make_resource(path: &str, target: &str, state: &str, force: bool) -> Resource {
        let kind = ResourceKind::from_static("file.symlink");
        let id = ResourceId::new(&kind, path);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "path": path,
                "target": target,
                "state": state,
                "force": force,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn type_name_is_file_symlink() {
        assert_eq!(
            FileSymlinkPrimitive.type_name(),
            ResourceKind::from_static("file.symlink"),
        );
    }

    #[test]
    fn identity_keys_is_path() {
        assert_eq!(FileSymlinkPrimitive.identity_keys(), &["path"]);
    }

    #[test]
    fn build_payload_with_all_args() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("path".into(), ArgValue::Str("/usr/local/bin/pg".into()));
        args.insert("target".into(), ArgValue::Str("/usr/nix/pg".into()));
        args.insert("state".into(), ArgValue::Str("present".into()));
        args.insert("force".into(), ArgValue::Bool(true));
        let call_args = CallArgs::new(args);
        let payload = FileSymlinkPrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["path"], "/usr/local/bin/pg");
        assert_eq!(payload["target"], "/usr/nix/pg");
        assert_eq!(payload["state"], "present");
        assert_eq!(payload["force"], true);
    }

    #[test]
    fn build_payload_defaults_state_to_present() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("path".into(), ArgValue::Str("/x".into()));
        args.insert("target".into(), ArgValue::Str("/y".into()));
        let call_args = CallArgs::new(args);
        let payload = FileSymlinkPrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["state"], "present");
        assert_eq!(payload["force"], false);
    }

    #[test]
    fn build_payload_missing_path_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("target".into(), ArgValue::Str("/y".into()));
        let call_args = CallArgs::new(args);
        let err = FileSymlinkPrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("path")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_payload_missing_target_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("path".into(), ArgValue::Str("/x".into()));
        let call_args = CallArgs::new(args);
        let err = FileSymlinkPrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("target")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn plan_create_when_missing_and_present() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        let r = make_resource(&link.to_string_lossy(), "/target", "present", false);
        let diff = FileSymlinkPrimitive
            .plan(&r, &NoFacts, &plan_ctx())
            .unwrap();
        assert!(matches!(diff, Diff::Add { .. }));
    }

    #[test]
    fn plan_no_change_when_symlink_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/target", &link).unwrap();
        let r = make_resource(&link.to_string_lossy(), "/target", "present", false);
        let diff = FileSymlinkPrimitive
            .plan(&r, &NoFacts, &plan_ctx())
            .unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn plan_update_when_target_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/old", &link).unwrap();
        let r = make_resource(&link.to_string_lossy(), "/new", "present", false);
        let diff = FileSymlinkPrimitive
            .plan(&r, &NoFacts, &plan_ctx())
            .unwrap();
        assert!(matches!(diff, Diff::Update { .. }));
    }

    #[test]
    fn plan_returns_apply_error_on_non_symlink_without_force() {
        // Поведение plan'а: для path с не-симлинком и force=false возвращаем
        // Apply-ошибку, чтобы оператор увидел отказ ещё на dry-run.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file");
        std::fs::write(&path, b"x").unwrap();
        let r = make_resource(&path.to_string_lossy(), "/target", "present", false);
        let err = FileSymlinkPrimitive
            .plan(&r, &NoFacts, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => assert!(reason.contains("not a symlink")),
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn plan_delete_when_state_absent_and_symlink_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/target", &link).unwrap();
        let r = make_resource(&link.to_string_lossy(), "/target", "absent", false);
        let diff = FileSymlinkPrimitive
            .plan(&r, &NoFacts, &plan_ctx())
            .unwrap();
        match diff {
            Diff::Update { description, .. } => assert!(description.contains("delete")),
            other => panic!("expected Update, got {other:?}"),
        }
    }
}
