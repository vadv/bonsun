//! Примитив `file.content` — атомарная запись файла с backup'ом и chown'ом.
//!
//! Тело контента не хранится в `Resource.payload` (оно может быть секретом),
//! а лежит в `SensitiveStore` под `Resource.id`. В payload — sha256 и size,
//! которых хватает для plan-сравнения.

mod apply;
mod backup;
mod chown;
mod plan;
mod spec;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use plan::sha256_hex;
pub use spec::FileContentSpec;

/// Реализация Primitive для `file.content`. Stateless.
pub struct FilePrimitive;

impl Primitive for FilePrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("file.content")
    }

    fn identity_keys(&self) -> &'static [&'static str] {
        &["path"]
    }

    /// Args, дошедшие сюда из Starlark-glue, уже без `contents` — оно
    /// перехвачено glue'ем и положено в SensitiveStore. Здесь ожидаются
    /// `path`, `content_sha256`, `content_size`, опционально `mode/owner/group`.
    fn build_payload(
        &self,
        args: &CallArgs,
        _ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError> {
        let path = args
            .required_str("path")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?;
        let content_sha256 = args
            .required_str("content_sha256")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?;
        let content_size: u64 = args
            .optional_u64("content_size")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?
            .ok_or_else(|| {
                PrimitiveError::InvalidPayload("file.content: missing content_size".to_string())
            })?;
        let mode = args
            .optional_u32("mode")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?
            .unwrap_or(0o644);
        let owner = args
            .optional_str("owner")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?;
        let group = args
            .optional_str("group")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content: {e}")))?;

        Ok(serde_json::json!({
            "path": path,
            "mode": mode,
            "owner": owner,
            "group": group,
            "content_sha256": content_sha256,
            "content_size": content_size,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        _facts: &dyn FactsSource,
        _ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        let spec: FileContentSpec = serde_json::from_value(resource.payload.clone())
            .map_err(|e| PrimitiveError::InvalidPayload(format!("file.content payload: {e}")))?;
        spec.validate()?;
        let target = Path::new(&spec.path);

        match std::fs::symlink_metadata(target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Diff::Add {
                description: format!("create {} ({} bytes)", target.display(), spec.content_size),
                payload: resource.payload.clone(),
            }),
            Err(e) => Err(PrimitiveError::Io {
                context: format!("symlink_metadata {}", target.display()),
                source: e,
            }),
            Ok(meta) => {
                let ft = meta.file_type();
                if ft.is_symlink() {
                    return Err(PrimitiveError::InvalidTarget);
                }
                if !ft.is_file() {
                    return Err(PrimitiveError::InvalidPayload(format!(
                        "target {} exists but is not a regular file",
                        target.display(),
                    )));
                }
                let obs = plan::observe_existing(target, &meta)?;
                if plan::matches_spec(&spec, &obs)? {
                    return Ok(Diff::NoChange);
                }
                Ok(Diff::Update {
                    from: serde_json::json!({
                        "sha256": obs.sha256_hex,
                        "size": obs.size,
                        "mode": format!("{:o}", meta.permissions().mode() & 0o7777),
                        "uid": meta.uid(),
                        "gid": meta.gid(),
                    }),
                    to: serde_json::json!({
                        "sha256": spec.content_sha256,
                        "size": spec.content_size,
                        "mode": format!("{:o}", spec.mode),
                        "owner": spec.owner,
                        "group": spec.group,
                    }),
                    description: format!(
                        "update {} (sha {} -> {})",
                        target.display(),
                        obs.sha256_hex,
                        spec.content_sha256,
                    ),
                })
            }
        }
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
    use std::time::{Duration, Instant};

    use bosun_core::{FactValue, Resource, ResourceId};
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

    fn make_resource(path: &str, contents: &str, mode: u32) -> Resource {
        let sha = sha256_hex(contents.as_bytes());
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, path);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "path": path,
                "mode": mode,
                "content_sha256": sha,
                "content_size": contents.len() as u64,
            }),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn type_name_is_file_content() {
        assert_eq!(
            FilePrimitive.type_name(),
            ResourceKind::from_static("file.content")
        );
    }

    #[test]
    fn identity_keys_is_path() {
        assert_eq!(FilePrimitive.identity_keys(), &["path"]);
    }

    #[test]
    fn plan_returns_add_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing");
        let r = make_resource(path.to_str().unwrap(), "x", 0o644);
        let diff = FilePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::Add { .. }));
    }

    #[test]
    fn plan_returns_no_change_when_file_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conf");
        std::fs::write(&path, b"hello").unwrap();
        let perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();
        let r = make_resource(path.to_str().unwrap(), "hello", 0o644);
        let diff = FilePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::NoChange));
    }

    #[test]
    fn plan_returns_update_when_content_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conf");
        std::fs::write(&path, b"different content").unwrap();
        let r = make_resource(path.to_str().unwrap(), "expected", 0o644);
        let diff = FilePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::Update { .. }));
    }

    #[test]
    fn plan_returns_update_when_mode_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conf");
        std::fs::write(&path, b"identical").unwrap();
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms).unwrap();
        let r = make_resource(path.to_str().unwrap(), "identical", 0o644);
        let diff = FilePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap();
        assert!(matches!(diff, Diff::Update { .. }));
    }

    #[test]
    fn plan_rejects_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::write(&real, b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let r = make_resource(link.to_str().unwrap(), "y", 0o644);
        let err = FilePrimitive.plan(&r, &NoFacts, &plan_ctx()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidTarget));
    }

    #[test]
    fn build_payload_round_trip() {
        use std::collections::HashMap;

        use bosun_core::ArgValue;

        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("path".into(), ArgValue::Str("/etc/x".into()));
        args.insert("content_sha256".into(), ArgValue::Str("abc".into()));
        args.insert("content_size".into(), ArgValue::Int(3));
        args.insert("mode".into(), ArgValue::Int(0o600));
        args.insert("owner".into(), ArgValue::Str("root".into()));
        let call_args = CallArgs::new(args);
        let payload = FilePrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["path"], serde_json::json!("/etc/x"));
        assert_eq!(payload["content_sha256"], serde_json::json!("abc"));
        assert_eq!(payload["content_size"], serde_json::json!(3));
        assert_eq!(payload["mode"], serde_json::json!(0o600));
        assert_eq!(payload["owner"], serde_json::json!("root"));
        assert_eq!(payload["group"], serde_json::json!(null));
    }

    #[test]
    fn build_payload_missing_required_is_error() {
        use std::collections::HashMap;

        let call_args = CallArgs::new(HashMap::new());
        let err = FilePrimitive
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("path")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
