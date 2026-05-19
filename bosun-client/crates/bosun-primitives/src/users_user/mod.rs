//! Примитив `users.user` — декларативное создание, обновление и удаление
//! системных пользователей через `useradd`/`usermod`/`userdel`.
//!
//! Идемпотентность: перед exec'ом всегда вызывается lookup через
//! `getpwnam_r` (внутри `RealUsersBackend`). Если пользователь
//! существует и поля spec'а совпадают с реальными — возвращается
//! `ChangeReport::no_change()`, никаких внешних вызовов.
//!
//! Безопасность:
//! - Все mutating-операции требуют `euid == 0`. Backend отказывает
//!   через `UsersError::NotRoot` ещё до exec'а.
//! - Имя пользователя валидируется по дефолтному NAME_REGEX
//!   useradd'а: блокирует имена-флаги (`-rf`) и shell-injection.
//! - argv собирается в типизированный `Vec<String>`, никакого shell'а.
//!
//! Destructive-defaults (ADR):
//! - `userdel` зовётся БЕЗ `--remove`: home-директория сохраняется. В
//!   реальных deployment'ах postgres/pgbouncer home может содержать
//!   данные кластера, и автоматическое удаление неприемлемо. Если в
//!   будущем понадобится `purge`, добавим явный флаг `purge: true` в
//!   spec — отдельным изменением.

pub mod apply;
mod backend;
pub mod plan;
mod spec;

use std::sync::Arc;

use bosun_core::{
    ApplyCtx, CallArgs, ChangeReport, Diff, FactsSource, PlanCtx, Primitive, PrimitiveError,
    Resource, ResourceKind,
};

pub use backend::{RealUsersBackend, UserAddOpts, UserInfo, UserModOpts, UsersBackend, UsersError};
pub use plan::{decide_action_user, Action, FieldDiff};
pub use spec::{UserSpec, UserState};

/// Реализация `Primitive` для `users.user`. Stateless, держит DI-backend
/// в `Arc<dyn UsersBackend>` — тестам это позволяет подменять exec'и без
/// модификации системы.
pub struct UserPrimitive {
    backend: Arc<dyn UsersBackend>,
}

impl UserPrimitive {
    /// Конструктор с явным backend'ом. Production-CLI использует
    /// [`UserPrimitive::with_real_backend`].
    pub fn new(backend: Arc<dyn UsersBackend>) -> Self {
        Self { backend }
    }

    /// Удобный конструктор: внутри Arc::new(RealUsersBackend).
    pub fn with_real_backend() -> Self {
        Self::new(Arc::new(RealUsersBackend))
    }
}

impl Default for UserPrimitive {
    fn default() -> Self {
        Self::with_real_backend()
    }
}

impl Primitive for UserPrimitive {
    fn type_name(&self) -> ResourceKind {
        ResourceKind::from_static("users.user")
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
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?;
        let state = args
            .required_str("state")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?;
        if !matches!(state.as_str(), "present" | "absent") {
            return Err(PrimitiveError::InvalidPayload(format!(
                "users.user: state {state:?} invalid; expected present|absent",
            )));
        }
        let uid = args
            .optional_u32("uid")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?;
        let group = args
            .optional_str("group")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?;
        let shell = args
            .optional_str("shell")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?;
        let home = args
            .optional_str("home")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?;
        let no_create_home = args
            .optional_bool("no_create_home")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?
            .unwrap_or(false);
        let system = args
            .optional_bool("system")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?
            .unwrap_or(false);
        let comment = args
            .optional_str("comment")
            .map_err(|e| PrimitiveError::InvalidPayload(format!("users.user: {e}")))?;

        Ok(serde_json::json!({
            "name": name,
            "state": state,
            "uid": uid,
            "group": group,
            "shell": shell,
            "home": home,
            "no_create_home": no_create_home,
            "system": system,
            "comment": comment,
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
        apply::run(resource, diff, ctx, &self.backend)
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
    fn type_name_is_users_user() {
        let p = UserPrimitive::with_real_backend();
        assert_eq!(p.type_name(), ResourceKind::from_static("users.user"));
    }

    #[test]
    fn identity_keys_is_name() {
        let p = UserPrimitive::with_real_backend();
        assert_eq!(p.identity_keys(), &["name"]);
    }

    #[test]
    fn build_payload_minimum_present() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        args.insert("state".into(), ArgValue::Str("present".into()));
        let call_args = CallArgs::new(args);
        let payload = UserPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["name"], "postgres");
        assert_eq!(payload["state"], "present");
        assert!(payload["uid"].is_null());
        assert_eq!(payload["no_create_home"], false);
        assert_eq!(payload["system"], false);
    }

    #[test]
    fn build_payload_full_fields() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        args.insert("state".into(), ArgValue::Str("present".into()));
        args.insert("uid".into(), ArgValue::Int(5432));
        args.insert("group".into(), ArgValue::Str("postgres".into()));
        args.insert("shell".into(), ArgValue::Str("/bin/false".into()));
        args.insert("home".into(), ArgValue::Str("/var/lib/postgresql".into()));
        args.insert("no_create_home".into(), ArgValue::Bool(true));
        args.insert("system".into(), ArgValue::Bool(true));
        args.insert("comment".into(), ArgValue::Str("PG admin".into()));
        let call_args = CallArgs::new(args);
        let payload = UserPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap();
        assert_eq!(payload["uid"], 5432);
        assert_eq!(payload["group"], "postgres");
        assert_eq!(payload["shell"], "/bin/false");
        assert_eq!(payload["home"], "/var/lib/postgresql");
        assert_eq!(payload["no_create_home"], true);
        assert_eq!(payload["system"], true);
        assert_eq!(payload["comment"], "PG admin");
    }

    #[test]
    fn build_payload_rejects_unknown_state() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        args.insert("state".into(), ArgValue::Str("vanished".into()));
        let call_args = CallArgs::new(args);
        let err = UserPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    #[test]
    fn build_payload_missing_state_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("name".into(), ArgValue::Str("postgres".into()));
        let call_args = CallArgs::new(args);
        let err = UserPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    #[test]
    fn build_payload_missing_name_is_error() {
        let mut args: HashMap<String, ArgValue> = HashMap::new();
        args.insert("state".into(), ArgValue::Str("present".into()));
        let call_args = CallArgs::new(args);
        let err = UserPrimitive::with_real_backend()
            .build_payload(&call_args, &plan_ctx())
            .unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }
}
