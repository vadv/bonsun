//! Примитив `apt.update_cache` — ленивый `apt-get update` с cleanup'ом
//! старых `.deb`-файлов.
//!
//! Семантика — read-before-write через mtime `pkgcache.bin`:
//! - plan: если кеш моложе `max_age_sec` (default 3600) и `force=false` —
//!   `Diff::NoChange`. Иначе — `Diff::Update`.
//! - apply: повторно проверяет mtime (соседний цикл мог обновить кеш),
//!   probe'ит dpkg-lock, запускает `apt-get update`, чистит `.deb` старше
//!   `cleanup_old_debs_days` (default 1) дней.
//!
//! chiit-аналог: `lib/apt/apt.go::Update` — пускал `apt-get update` под
//! lock'ом, проверял свежесть `pkgcache.bin` (1 час), потом `find
//! /var/cache/apt -mtime +1 -name "*.deb" -delete`.
//!
//! DI: trait `AptCacheBackend` — production использует
//! `RealAptCacheBackend` (поверх `apt_package::exec::RealCommandRunner`),
//! тесты подменяют mock без побочных эффектов на систему.

mod apply;
mod plan;
mod spec;

use std::path::PathBuf;
use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use apply::{AptCacheBackend, RealAptCacheBackend};
pub use plan::{Action, RefreshReason};
pub use spec::AptUpdateCacheSpec;

/// Реализация Primitive для `apt.update_cache`.
pub struct AptUpdateCachePrimitive {
    backend: Arc<dyn AptCacheBackend>,
    pkgcache_path: PathBuf,
    archives_dir: PathBuf,
    dpkg_lock_path: PathBuf,
}

impl AptUpdateCachePrimitive {
    /// Конструктор с явным backend'ом. Для production CLI — обёртка над
    /// `RealAptCacheBackend`, для тестов — mock.
    pub fn new(backend: Arc<dyn AptCacheBackend>) -> Self {
        Self {
            backend,
            pkgcache_path: apply::default_pkgcache_path(),
            archives_dir: apply::default_archives_dir(),
            dpkg_lock_path: apply::default_dpkg_lock_path(),
        }
    }

    /// Удобный конструктор для production: `RealAptCacheBackend` внутри Arc.
    pub fn with_real_backend() -> Self {
        Self::new(Arc::new(RealAptCacheBackend))
    }
}

impl Default for AptUpdateCachePrimitive {
    fn default() -> Self {
        Self::with_real_backend()
    }
}

impl Primitive for AptUpdateCachePrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("apt.update_cache")
    }

    fn identity_keys(&self) -> &'static [&'static str] {
        &["name"]
    }

    fn build_payload(
        &self,
        args: &CallArgs,
        _ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError> {
        let name = args
            .required_str("name")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.update_cache: {e}")))?;
        let max_age_sec = args
            .optional_u32("max_age_sec")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.update_cache: {e}")))?
            .unwrap_or(3600);
        let force = args
            .optional_bool("force")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.update_cache: {e}")))?
            .unwrap_or(false);
        let cleanup_old_debs_days = args
            .optional_u32("cleanup_old_debs_days")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.update_cache: {e}")))?
            .unwrap_or(1);
        let skip_cleanup = args
            .optional_bool("skip_cleanup")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.update_cache: {e}")))?
            .unwrap_or(false);

        Ok(serde_json::json!({
            "name": name,
            "max_age_sec": max_age_sec,
            "force": force,
            "cleanup_old_debs_days": cleanup_old_debs_days,
            "skip_cleanup": skip_cleanup,
        }))
    }

    fn plan(
        &self,
        resource: &Resource,
        facts: &dyn FactsSource,
        ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError> {
        plan::compute_diff(resource, facts, ctx)
    }

    fn apply(
        &self,
        resource: &Resource,
        diff: &Diff,
        ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError> {
        apply::run(
            self.backend.as_ref(),
            &self.pkgcache_path,
            &self.archives_dir,
            &self.dpkg_lock_path,
            resource,
            diff,
            ctx,
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use bosun_core::{ArgValue, PlanCtx};
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn plan_ctx() -> PlanCtx {
        PlanCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
        )
    }

    #[test]
    fn type_name_is_apt_update_cache() {
        let p = AptUpdateCachePrimitive::with_real_backend();
        assert_eq!(p.type_name(), ResourceKind::from_static("apt.update_cache"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = AptUpdateCachePrimitive::with_real_backend();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_defaults() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("apt-cache".into()));
        let call_args = CallArgs::new(args);
        let p = AptUpdateCachePrimitive::with_real_backend();
        let payload = p.build_payload(&call_args, &plan_ctx()).unwrap();
        assert_eq!(payload["name"], "apt-cache");
        assert_eq!(payload["max_age_sec"], 3600);
        assert_eq!(payload["force"], false);
        assert_eq!(payload["cleanup_old_debs_days"], 1);
        assert_eq!(payload["skip_cleanup"], false);
    }

    #[test]
    fn build_payload_all_fields() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("weekly".into()));
        args.insert("max_age_sec".into(), ArgValue::Int(604_800));
        args.insert("force".into(), ArgValue::Bool(true));
        args.insert("cleanup_old_debs_days".into(), ArgValue::Int(7));
        args.insert("skip_cleanup".into(), ArgValue::Bool(true));
        let call_args = CallArgs::new(args);
        let p = AptUpdateCachePrimitive::with_real_backend();
        let payload = p.build_payload(&call_args, &plan_ctx()).unwrap();
        assert_eq!(payload["name"], "weekly");
        assert_eq!(payload["max_age_sec"], 604_800);
        assert_eq!(payload["force"], true);
        assert_eq!(payload["cleanup_old_debs_days"], 7);
        assert_eq!(payload["skip_cleanup"], true);
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let call_args = CallArgs::new(HashMap::new());
        let p = AptUpdateCachePrimitive::with_real_backend();
        let err = p.build_payload(&call_args, &plan_ctx()).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => assert!(msg.contains("name")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }
}
