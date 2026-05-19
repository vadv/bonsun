//! Примитив `file.delete` — снятие файла, симлинка или директории.
//!
//! Покрывает chiit-кейсы из `roles/repos/repos.go` (cleanup устаревших
//! repo-list файлов), `roles/journald/config.go` (удалить journald
//! override) и `roles/postgres_manage/init_config.go` (patroni guard
//! трейггер-файл). Эквивалентен `files.Delete(ctx, path)` из
//! `chiit/lib/providers/file/filemanager/filemanager.go`.
//!
//! Внутри:
//! - `spec` — десериализация payload'а в `FileDeleteSpec`.
//! - `plan` — `decide_action_delete` по `symlink_metadata`.
//! - `apply` — `remove_file`/`remove_dir_all`, идемпотентно для отсутствующих
//!   путей.

mod apply;
mod plan;
mod spec;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use plan::{decide_action_delete, Action};
pub use spec::FileDeleteSpec;

/// Реализация Primitive для `file.delete`. Stateless.
pub struct FileDeletePrimitive;

impl Primitive for FileDeletePrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("file.delete")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.delete: {e}")))?;
        let recursive = args
            .optional_bool("recursive")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.delete: {e}")))?
            .unwrap_or(false);
        let follow_symlinks = args
            .optional_bool("follow_symlinks")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.delete: {e}")))?
            .unwrap_or(false);

        Ok(serde_json::json!({
            "path": path,
            "recursive": recursive,
            "follow_symlinks": follow_symlinks,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        _facts: &dyn FactsSource,
        _ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        let spec: FileDeleteSpec = serde_json::from_value(resource.payload.clone())
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.delete payload: {e}")))?;
        spec.validate()?;

        let action = decide_action_delete(&spec.path)?;
        Ok(match action {
            Action::NoChange => Diff::NoChange,
            Action::DeleteFile | Action::DeleteDir => Diff::Update {
                from: serde_json::json!({"path": spec.path.to_string_lossy(), "exists": true}),
                to: serde_json::json!({"path": spec.path.to_string_lossy(), "exists": false}),
                description: format!("delete {}", spec.path.display()),
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

    fn make_resource(path: &str, recursive: bool) -> Resource {
        let kind = ResourceKind::from_static("file.delete");
        let id = ResourceId::new(&kind, path);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({"path": path, "recursive": recursive}),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn type_name_is_file_delete() {
        assert_eq!(
            FileDeletePrimitive.type_name(),
            ResourceKind::from_static("file.delete"),
        );
    }

    #[test]
    fn identity_keys_is_path() {
        assert_eq!(FileDeletePrimitive.identity_keys(), &["path"]);
    }

    #[test]
    fn build_payload_round_trip() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("path".into(), ArgValue::Str("/etc/foo".into()));
        args.insert("recursive".into(), ArgValue::Bool(true));
        let call_args = CallArgs::new(args);
        let payload = FileDeletePrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["path"], "/etc/foo");
        assert_eq!(payload["recursive"], true);
        assert_eq!(payload["follow_symlinks"], false);
    }

    #[test]
    fn build_payload_missing_path_is_error() {
        let call_args = CallArgs::new(HashMap::new());
        let err = FileDeletePrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("path")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn plan_no_change_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing");
        let r = make_resource(&path.to_string_lossy(), false);
        let diff = FileDeletePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn plan_update_when_file_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("file");
        std::fs::write(&path, b"x").unwrap();
        let r = make_resource(&path.to_string_lossy(), false);
        let diff = FileDeletePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        match diff {
            Diff::Update { description, .. } => assert!(description.contains("delete")),
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn plan_update_when_dir_exists_even_without_recursive() {
        // Plan не блокирует non-empty directory без recursive — это решение
        // переехало в apply (так оператор увидит явный Apply-отказ в логах).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("child"), b"x").unwrap();
        let r = make_resource(&dir.to_string_lossy(), false);
        let diff = FileDeletePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::Update { .. }));
    }
}
