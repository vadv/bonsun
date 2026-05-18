//! Контракты примитива.
//!
//! ## Почему `FactsSource` без `Send + Sync`
//!
//! Apply-фаза однопоточная: один Worker последовательно прогоняет ресурсы
//! из топологического порядка. FactsCollector держит локальный кэш в
//! `RefCell<HashMap<...>>` для interior mutability — это даёт дешёвый
//! lazy refresh без блокировок. `RefCell: !Sync`, поэтому требовать `Sync`
//! от `FactsSource` означало бы запретить такую реализацию ради воображаемой
//! многопоточности, которой в MVP нет.
//!
//! Симметрично `InventorySource`: тоже read-only single-threaded, тех же
//! ограничений нет смысла навязывать.
//!
//! `Primitive` остаётся `Send + Sync` — примитивы stateless, в будущем
//! параллельная плоскость apply (per-namespace pool) потребует, чтобы их
//! можно было держать в Arc и звать из любого worker'а.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::diff::{ChangeReport, Diff};
use crate::resource::Resource;
use crate::sensitive::SensitiveStore;

/// Контекст plan-фазы: дедлайн + cancel token. Передаётся by value
/// (поля Clone-дешёвые, CancellationToken — Arc внутри).
#[derive(Clone)]
#[non_exhaustive]
pub struct PlanCtx {
    pub deadline: Instant,
    pub cancel: CancellationToken,
}

/// Контекст apply-фазы. Дополнительно хранит side-channel для секретов
/// (SensitiveStore) и tracing-span для пер-ресурсного логирования.
#[derive(Clone)]
#[non_exhaustive]
pub struct ApplyCtx {
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub log_span: tracing::Span,
    pub sensitive: Arc<SensitiveStore>,
    /// Корень дерева для бэкапов file.content. Бэкап-путь строится как
    /// `{backup_root}{target}.{utc_ts}` — например `/etc/nginx/nginx.conf`
    /// под `backup_root = /var/backups/bosun` даёт
    /// `/var/backups/bosun/etc/nginx/nginx.conf.YYYYMMDDTHHMMSSZ`.
    pub backup_root: PathBuf,
    /// Каталог для лог-файлов примитивов: например, `apt.package` пишет
    /// полный stderr в `{log_dir}/apt-install-last-error.log` при провале,
    /// чтобы оператор мог поднять post-mortem без перезапуска bosun.
    /// В production CLI задаёт `/var/log/bosun`.
    pub log_dir: PathBuf,
}

impl PlanCtx {
    /// Конструктор для случаев, когда нужно создать PlanCtx из внешнего крейта.
    /// Структура `#[non_exhaustive]`, поэтому struct-литерал снаружи запрещён.
    pub fn new(deadline: Instant, cancel: CancellationToken) -> Self {
        Self { deadline, cancel }
    }

    pub fn cancelled_or_past_deadline(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

impl ApplyCtx {
    /// Конструктор для внешних крейтов; см. `PlanCtx::new`.
    pub fn new(
        deadline: Instant,
        cancel: CancellationToken,
        log_span: tracing::Span,
        sensitive: Arc<SensitiveStore>,
        backup_root: PathBuf,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            deadline,
            cancel,
            log_span,
            sensitive,
            backup_root,
            log_dir,
        }
    }

    pub fn cancelled_or_past_deadline(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }
}

/// Ошибка любой стадии примитива.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PrimitiveError {
    #[error("io error in {context}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid resource payload: {0}")]
    InvalidPayload(String),
    #[error("external command failed: {reason}")]
    Exec {
        reason: String,
        exit: Option<i32>,
        stderr_excerpt: String,
    },
    #[error("dpkg locked, holder pid={holder_pid:?}")]
    DpkgLocked { holder_pid: Option<i32> },
    #[error("chown not permitted: requested {requested}, current {actual}")]
    ChownNotPermitted { requested: String, actual: String },
    #[error("target is a symlink, refusing to write through it")]
    InvalidTarget,
    #[error("operation cancelled by deadline or signal")]
    Cancelled,
    #[error("panicked in {context}: {message}")]
    Panic { context: String, message: String },
}

impl PrimitiveError {
    /// Returns true для ошибок, которые означают «попробуй на следующем
    /// цикле, это не настоящий провал». Пример — `DpkgLocked`: пока
    /// `unattended-upgrades` или другой apt-инструмент держит lock-frontend,
    /// ресурс apt.package не может ничего сделать. Это транзиентное
    /// состояние, метрика `bosun_resources_total{outcome="failed"}` не должна
    /// флапать каждые 30 секунд при таком сценарии.
    pub fn is_deferrable(&self) -> bool {
        matches!(
            self,
            PrimitiveError::DpkgLocked { .. } | PrimitiveError::Cancelled
        )
    }
}

/// Trait для FactsSource — read-only доступ к фактам.
/// Объявляется здесь, реализуется в bosun-facts.
/// Send/Sync не требуется: apply однопоточный, см. модульный комментарий.
pub trait FactsSource {
    fn get(&self, name: &str) -> crate::facts::FactValue;
}

/// Trait одного примитива.
pub trait Primitive: Send + Sync {
    fn type_name(&self) -> crate::resource::ResourceKind;
    fn identity_keys(&self) -> &'static [&'static str];

    fn build_payload(
        &self,
        args: &crate::call_args::CallArgs,
        ctx: &PlanCtx,
    ) -> Result<serde_json::Value, PrimitiveError>;

    fn plan(
        &self,
        resource: &Resource,
        facts: &dyn FactsSource,
        ctx: &PlanCtx,
    ) -> Result<Diff, PrimitiveError>;

    fn apply(
        &self,
        resource: &Resource,
        diff: &Diff,
        ctx: &ApplyCtx,
    ) -> Result<ChangeReport, PrimitiveError>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn plan_ctx_cancelled_via_token() {
        let cancel = CancellationToken::new();
        let ctx = PlanCtx {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: cancel.clone(),
        };
        assert!(!ctx.cancelled_or_past_deadline());
        cancel.cancel();
        assert!(ctx.cancelled_or_past_deadline());
    }

    #[test]
    fn plan_ctx_cancelled_via_deadline() {
        let ctx = PlanCtx {
            deadline: Instant::now() - Duration::from_millis(1),
            cancel: CancellationToken::new(),
        };
        assert!(ctx.cancelled_or_past_deadline());
    }

    #[test]
    fn is_deferrable_for_dpkg_locked_and_cancelled() {
        assert!(PrimitiveError::DpkgLocked { holder_pid: None }.is_deferrable());
        assert!(PrimitiveError::DpkgLocked {
            holder_pid: Some(42)
        }
        .is_deferrable());
        assert!(PrimitiveError::Cancelled.is_deferrable());
    }

    #[test]
    fn is_deferrable_false_for_real_failures() {
        assert!(!PrimitiveError::InvalidPayload("boom".into()).is_deferrable());
        assert!(!PrimitiveError::InvalidTarget.is_deferrable());
        assert!(!PrimitiveError::Exec {
            reason: "x".into(),
            exit: Some(1),
            stderr_excerpt: String::new(),
        }
        .is_deferrable());
        assert!(!PrimitiveError::ChownNotPermitted {
            requested: "uid=0".into(),
            actual: "uid=1000".into(),
        }
        .is_deferrable());
    }
}
