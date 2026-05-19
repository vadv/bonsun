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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use bosun_handles::{RunrHandle, ServiceStatus, SystemdHandle};
use tokio_util::sync::CancellationToken;

use crate::defers::Journal;
use crate::diff::{ChangeReport, Diff};
use crate::resource::{Resource, ResourceId};
use crate::sensitive::SensitiveStore;
use crate::validate::{RealValidateRunner, ValidateRunner};

/// Контекст plan-фазы: дедлайн + cancel token. Передаётся by value
/// (поля Clone-дешёвые, CancellationToken — Arc внутри).
#[derive(Clone)]
#[non_exhaustive]
pub struct PlanCtx {
    pub deadline: Instant,
    pub cancel: CancellationToken,
}

/// Контекст apply-фазы. Несёт всё, что нужно примитивам сквозь apply:
/// дедлайн, cancel-токен, side-channel для секретов, пути для backup'ов и
/// логов, общий журнал defers, журналы изменений в текущем прогоне и
/// handle'ы внешних демонов (runr, systemd).
///
/// Поля, связанные с runr/systemd, опциональны: на ноде с `init_system =
/// runr` будет только `runr`, на чистом systemd — только `systemd`, в
/// смешанной конфигурации `mixed-systemd-runr` — оба. Примитив, которому
/// нужен handle, проверяет `Option::is_some()` и возвращает
/// `PrimitiveError::RunrUnavailable` / `SystemdUnavailable` иначе.
///
/// `defers` — общий журнал на весь процесс apply. Передаётся в primitive
/// через ctx, чтобы один и тот же файл-журнал был виден и при enqueue
/// (внутри apply), и при последующем replay (в bosun-cli).
///
/// `runr_daemon_reload_done` / `systemd_daemon_reload_done` — throttle на
/// один apply: первый примитив, которому нужен daemon-reload, выставит
/// флаг и реально позовёт daemon_reload; остальные пропустят. Тип
/// `Arc<AtomicBool>` сохраняет требование `Sync` для ApplyCtx и
/// не требует ручной синхронизации (даже с однопоточным apply это
/// корректно — атомарный compare-and-set дешёв).
///
/// `runr_service_statuses` — кэш ответа `runr.service_statuses()` на весь
/// apply: одного HTTP-call'а хватает на сравнение plan/apply для всех
/// `runr.service` ресурсов в манифесте. `OnceLock` обеспечивает lazy-init и
/// безопасную инициализацию single-shot.
///
/// `validator` — исполнитель `validate_with`-команд. Передаётся в Arc, чтобы
/// тесты подменяли spawn без зависимости от системных бинарей.
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
    /// Множество id ресурсов, у которых apply в текущем прогоне завершился
    /// `Changed`. Используется notify-логикой: примитив проверяет, изменился
    /// ли источник из `restart_on` / `reload_on`, через `is_changed`.
    /// `Arc<Mutex<...>>` сохраняет согласованность с остальными Arc-полями
    /// и работает даже если apply станет параллельным.
    pub changed_resources: Arc<Mutex<HashSet<ResourceId>>>,
    /// Журнал defer-записей. См. [`crate::defers`].
    pub defers: Arc<Journal>,
    /// Опциональный runr-клиент. Отсутствует на чистом systemd-стеке.
    pub runr: Option<Arc<dyn RunrHandle>>,
    /// Опциональный systemd-клиент. Отсутствует на runr-only нодах.
    pub systemd: Option<Arc<dyn SystemdHandle>>,
    /// Throttle для `runr.daemon_reload()` — один вызов на apply.
    pub runr_daemon_reload_done: Arc<AtomicBool>,
    /// Throttle для `systemd.daemon_reload()` — один вызов на apply.
    pub systemd_daemon_reload_done: Arc<AtomicBool>,
    /// Кэш ответа `runr.service_statuses()` на весь apply. См. описание
    /// поля.
    pub runr_service_statuses: Arc<OnceLock<Vec<ServiceStatus>>>,
    /// Исполнитель `validate_with`-команд (`nginx -t`, etc). В production
    /// CLI собирает `RealValidateRunner`; в тестах примитивы подменяют
    /// mock, который записывает argv и возвращает заранее заданный
    /// результат. Используется и `file.content` (validate перед swap), и
    /// `runr/systemd.service` (validate перед enqueue defer'а).
    pub validator: Arc<dyn ValidateRunner>,
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
    ///
    /// `defers` — общий журнал на процесс. Для тестов, которым defers не
    /// нужен, удобно держать журнал во временной директории через
    /// [`Journal::open`] на `TempDir`.
    ///
    /// Поля `runr` / `systemd` — handle'ы клиентов, передаются CLI после
    /// определения init-системы. Чистый systemd → `systemd=Some, runr=None`,
    /// и наоборот; `mixed-systemd-runr` — оба `Some`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        deadline: Instant,
        cancel: CancellationToken,
        log_span: tracing::Span,
        sensitive: Arc<SensitiveStore>,
        backup_root: PathBuf,
        log_dir: PathBuf,
        defers: Arc<Journal>,
        runr: Option<Arc<dyn RunrHandle>>,
        systemd: Option<Arc<dyn SystemdHandle>>,
    ) -> Self {
        Self::with_validator(
            deadline,
            cancel,
            log_span,
            sensitive,
            backup_root,
            log_dir,
            defers,
            runr,
            systemd,
            Arc::new(RealValidateRunner),
        )
    }

    /// То же, что `new`, но с явным `ValidateRunner`. Production CLI
    /// использует `new` (выбирает `RealValidateRunner` по умолчанию);
    /// тестам Phase H нужен mock-runner для проверки validate_with без
    /// зависимости от системных бинарей вроде `nginx`/`pgbouncer`.
    #[allow(clippy::too_many_arguments)]
    pub fn with_validator(
        deadline: Instant,
        cancel: CancellationToken,
        log_span: tracing::Span,
        sensitive: Arc<SensitiveStore>,
        backup_root: PathBuf,
        log_dir: PathBuf,
        defers: Arc<Journal>,
        runr: Option<Arc<dyn RunrHandle>>,
        systemd: Option<Arc<dyn SystemdHandle>>,
        validator: Arc<dyn ValidateRunner>,
    ) -> Self {
        Self {
            deadline,
            cancel,
            log_span,
            sensitive,
            backup_root,
            log_dir,
            changed_resources: Arc::new(Mutex::new(HashSet::new())),
            defers,
            runr,
            systemd,
            runr_daemon_reload_done: Arc::new(AtomicBool::new(false)),
            systemd_daemon_reload_done: Arc::new(AtomicBool::new(false)),
            runr_service_statuses: Arc::new(OnceLock::new()),
            validator,
        }
    }

    pub fn cancelled_or_past_deadline(&self) -> bool {
        self.cancel.is_cancelled() || Instant::now() >= self.deadline
    }

    /// Отметить ресурс как изменённый в текущем apply. Источник notify
    /// проверяет через [`Self::is_changed`].
    pub fn record_changed(&self, id: &ResourceId) {
        // PoisonError игнорируем: внутренняя структура — HashSet, после
        // паники чтения остаются валидными. Альтернатива (`expect`)
        // запрещена workspace-линтами.
        if let Ok(mut guard) = self.changed_resources.lock() {
            guard.insert(id.clone());
        }
    }

    /// Проверить, был ли ресурс отмечен изменённым в этом apply. Возвращает
    /// `false`, если mutex отравлён — это безопасный default для
    /// notify-логики (мы скорее не пошлём ложный restart, чем пошлём).
    pub fn is_changed(&self, id: &ResourceId) -> bool {
        match self.changed_resources.lock() {
            Ok(guard) => guard.contains(id),
            Err(_) => false,
        }
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
    /// Apply-уровневая ошибка с конкретным reason. Используется, когда
    /// внешний клиент вернул семантический non-deferrable отказ:
    /// например, runr ответил 404 NotFound или 500 ApiError; запись о
    /// причине достаётся в логах из `reason`.
    #[error("apply failed: {reason}")]
    Apply { reason: String },
    /// runr-демон недоступен (connection refused, transport error).
    /// `is_deferrable=true`: следующий цикл попробует снова, файл в
    /// журнале defers (если успели enqueue'нуть) подхватится при replay.
    #[error("runr unavailable at {base_url}: {reason}")]
    RunrUnavailable { base_url: String, reason: String },
    /// systemd dbus недоступен: BusUnavailable или транзиентный
    /// dbus-сбой. См. `RunrUnavailable` про deferrable-семантику.
    #[error("systemd unavailable: {reason}")]
    SystemdUnavailable { reason: String },
    /// Валидатор (`validate_with`) не пропустил конфигурацию.
    /// Возвращает имя валидатора и stderr-excerpt для post-mortem.
    #[error("validation by {validator} failed: {stderr_excerpt}")]
    Validation {
        validator: String,
        stderr_excerpt: String,
    },
    /// Health-check после действия не прошёл.
    #[error("health check on {target} failed: {reason}")]
    HealthCheckFailed { target: String, reason: String },
    /// I/O ошибка при работе с журналом defers (создание директории,
    /// запись файла, fsync). `path` — целевой путь, `reason` — описание.
    #[error("defer journal i/o error at {path}: {reason}")]
    DeferIo { path: PathBuf, reason: String },
}

impl PrimitiveError {
    /// Returns true для ошибок, которые означают «попробуй на следующем
    /// цикле, это не настоящий провал». Пример — `DpkgLocked`: пока
    /// `unattended-upgrades` или другой apt-инструмент держит lock-frontend,
    /// ресурс apt.package не может ничего сделать. Это транзиентное
    /// состояние, метрика `bosun_resources_total{outcome="failed"}` не должна
    /// флапать каждые 30 секунд при таком сценарии.
    ///
    /// `RunrUnavailable` / `SystemdUnavailable` ведут себя так же: демон
    /// уйдёт в reboot или перезапустится и через минуту будет доступен.
    ///
    /// `Cancelled` сюда **не** входит: SIGTERM или истечение deadline —
    /// это явный сигнал «прервите процесс», и трактовать его как «отложим,
    /// замаскируем под success» враждебно оператору. Orchestrator маппит
    /// такие ошибки в отдельный `Outcome::Interrupted`, CLI возвращает
    /// exit-code 130 (POSIX-стандарт для SIGINT/SIGTERM).
    pub fn is_deferrable(&self) -> bool {
        matches!(
            self,
            PrimitiveError::DpkgLocked { .. }
                | PrimitiveError::RunrUnavailable { .. }
                | PrimitiveError::SystemdUnavailable { .. }
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
    fn is_deferrable_for_dpkg_locked() {
        assert!(PrimitiveError::DpkgLocked { holder_pid: None }.is_deferrable());
        assert!(PrimitiveError::DpkgLocked {
            holder_pid: Some(42)
        }
        .is_deferrable());
    }

    #[test]
    fn is_deferrable_for_cancelled_is_false() {
        // SIGTERM / deadline expiry — это «прервите процесс», не «отложим».
        // Маппится в Outcome::Interrupted на уровне orchestrator'а, не в
        // Deferred. См. exit-code 130 в CLI.
        assert!(!PrimitiveError::Cancelled.is_deferrable());
    }

    #[test]
    fn is_deferrable_for_runr_and_systemd_unavailable() {
        assert!(PrimitiveError::RunrUnavailable {
            base_url: "http://127.0.0.1:8010".into(),
            reason: "connection refused".into(),
        }
        .is_deferrable());
        assert!(PrimitiveError::SystemdUnavailable {
            reason: "bus socket missing".into(),
        }
        .is_deferrable());
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
        assert!(!PrimitiveError::Apply {
            reason: "runr returned 404".into(),
        }
        .is_deferrable());
        assert!(!PrimitiveError::Validation {
            validator: "nginx -t".into(),
            stderr_excerpt: "syntax error".into(),
        }
        .is_deferrable());
        assert!(!PrimitiveError::HealthCheckFailed {
            target: "nginx".into(),
            reason: "500".into(),
        }
        .is_deferrable());
        assert!(!PrimitiveError::DeferIo {
            path: PathBuf::from("/tmp/x"),
            reason: "ENOSPC".into(),
        }
        .is_deferrable());
    }

    #[test]
    fn apply_ctx_record_changed_then_is_changed() {
        use crate::resource::{ResourceId, ResourceKind};
        use crate::sensitive::SensitiveStore;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let journal = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            journal,
            None,
            None,
        );
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, "/etc/cfg");
        assert!(!ctx.is_changed(&id));
        ctx.record_changed(&id);
        assert!(ctx.is_changed(&id));
    }
}
