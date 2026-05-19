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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bosun_handles::{RunrHandle, SystemdHandle};
use tokio_util::sync::CancellationToken;

use crate::defers::Journal;
use crate::diff::{ChangeReport, Diff};
use crate::health_check::{HealthCheckRunner, NoopHealthCheckRunner};
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
/// Снимки состояния от runr/systemd (`service_statuses`, `timer_statuses`,
/// `unit_info`) **не кэшируются** в ApplyCtx. Каждый примитив делает свой
/// read-only вызов перед apply: HTTP к runr и dbus к systemd дёшевы
/// (loopback и локальный сокет), а кэш per-apply ломает кросс-ресурсные
/// notify-цепочки — primitive, ловящий `restart_on` после синхронного
/// `service_start` соседа, должен видеть post-start snapshot, не stale
/// pre-start версию. См. memory `feedback_bosun_no_cache_for_runr_systemd`.
///
/// `validator` — исполнитель `validate_with`-команд. Передаётся в Arc, чтобы
/// тесты подменяли spawn без зависимости от системных бинарей.
///
/// `health_check_runner` — исполнитель post-action health-probe'ов (Phase I).
/// Production CLI подключает `RealHealthCheckRunner` из `bosun-primitives`;
/// тесты подменяют mock; default — `NoopHealthCheckRunner` (см. TODO ниже).
///
/// Phase J: вместо разрастающихся `new`/`with_validator`/`with_runners`
/// конструкторов есть [`ApplyCtxBuilder`]. Legacy-конструкторы оставлены
/// для совместимости с существующими тестами примитивов.
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
    /// Исполнитель `validate_with`-команд (`nginx -t`, etc). В production
    /// CLI собирает `RealValidateRunner`; в тестах примитивы подменяют
    /// mock, который записывает argv и возвращает заранее заданный
    /// результат. Используется и `file.content` (validate перед swap), и
    /// `runr/systemd.service` (validate перед enqueue defer'а).
    pub validator: Arc<dyn ValidateRunner>,
    /// Исполнитель health-check'ов после restart/reload. В sync-пути
    /// (`Start`/`Stop` от desired-state-diff) запускается прямо в apply'е;
    /// в defer-replay вызывается после успешного dispatch'а defer'а. См.
    /// `bosun-core::health_check` (контракт) и
    /// `bosun-primitives::health_check::RealHealthCheckRunner` (production).
    pub health_check_runner: Arc<dyn HealthCheckRunner>,
    /// Runtime-store published-фактов: примитив (например, `pg_sql.query`
    /// с `store_as_fact=<name>`) пишет сюда результат, последующие
    /// примитивы или вышестоящий evaluator могут прочитать. Доступ —
    /// двумя путями: прямой `read_published_fact` для apply'я соседнего
    /// примитива и через [`OverlayFactsSource`], которым CLI оборачивает
    /// обычный `FactsView` для plan/apply (overlay приоритетно отдаёт
    /// published-факты через стандартный `FactsSource::get`). Это НЕ
    /// FactsCollector: добавление в `published_facts` не триггерит
    /// dirty-refresh статических фактов из `bosun-facts`.
    pub published_facts: Arc<Mutex<HashMap<String, serde_json::Value>>>,
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
        Self::with_runners(
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
            Arc::new(NoopHealthCheckRunner),
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
        Self::with_runners(
            deadline,
            cancel,
            log_span,
            sensitive,
            backup_root,
            log_dir,
            defers,
            runr,
            systemd,
            validator,
            Arc::new(NoopHealthCheckRunner),
        )
    }

    /// Phase I: ApplyCtx с явными validator'ом и health-check runner'ом.
    /// Production CLI собирает `RealHealthCheckRunner` (поверх ureq +
    /// `std::process::Command`); тесты Phase I подменяют mock-runner,
    /// чтобы проверять retry-логику и cancellation без зависимости от
    /// сети и системных бинарей.
    ///
    /// Phase J: legacy-конструктор оставлен для существующих тестов
    /// примитивов. В новом коде используй [`ApplyCtxBuilder`].
    #[allow(clippy::too_many_arguments)]
    pub fn with_runners(
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
        health_check_runner: Arc<dyn HealthCheckRunner>,
    ) -> Self {
        let mut builder =
            ApplyCtxBuilder::new(deadline, cancel, sensitive, backup_root, log_dir, defers)
                .log_span(log_span)
                .validator(validator)
                .health_check_runner(health_check_runner);
        if let Some(h) = runr {
            builder = builder.runr(h);
        }
        if let Some(h) = systemd {
            builder = builder.systemd(h);
        }
        builder.build()
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

    /// Опубликовать факт под именем `name` со значением `value`. Если факт
    /// с таким именем уже есть — перезапишется. PoisonError игнорируем
    /// аналогично [`Self::record_changed`].
    pub fn publish_fact(&self, name: impl Into<String>, value: serde_json::Value) {
        if let Ok(mut guard) = self.published_facts.lock() {
            guard.insert(name.into(), value);
        }
    }

    /// Прочитать опубликованный факт. None, если факт не публиковался или
    /// mutex отравлён.
    pub fn read_published_fact(&self, name: &str) -> Option<serde_json::Value> {
        self.published_facts.lock().ok()?.get(name).cloned()
    }
}

/// Builder для [`ApplyCtx`]. Введён в Phase J: ApplyCtx разросся до 15 полей,
/// и сигнатуры `new`/`with_validator`/`with_runners` стали неподъёмными.
///
/// Обязательные поля (`deadline`, `cancel`, `sensitive`, `backup_root`,
/// `log_dir`, `defers`) задаются в [`ApplyCtxBuilder::new`]. Остальные —
/// через setter'ы; при отсутствии используются sensible defaults
/// ([`tracing::Span::none`], `None` для runr/systemd, [`RealValidateRunner`],
/// [`NoopHealthCheckRunner`]).
///
/// Пример использования из CLI:
///
/// ```no_run
/// # use std::path::PathBuf;
/// # use std::sync::Arc;
/// # use std::time::{Duration, Instant};
/// # use bosun_core::{ApplyCtxBuilder, SensitiveStore};
/// # use bosun_core::defers::Journal;
/// # use tokio_util::sync::CancellationToken;
/// # let tmp = tempfile::tempdir().unwrap();
/// # let journal = Arc::new(Journal::open(tmp.path()).unwrap());
/// let ctx = ApplyCtxBuilder::new(
///     Instant::now() + Duration::from_secs(60),
///     CancellationToken::new(),
///     Arc::new(SensitiveStore::new()),
///     PathBuf::from("/var/backups/bosun"),
///     PathBuf::from("/var/log/bosun"),
///     journal,
/// )
/// .build();
/// ```
#[non_exhaustive]
pub struct ApplyCtxBuilder {
    deadline: Instant,
    cancel: CancellationToken,
    sensitive: Arc<SensitiveStore>,
    backup_root: PathBuf,
    log_dir: PathBuf,
    defers: Arc<Journal>,
    log_span: Option<tracing::Span>,
    runr: Option<Arc<dyn RunrHandle>>,
    systemd: Option<Arc<dyn SystemdHandle>>,
    validator: Option<Arc<dyn ValidateRunner>>,
    health_check_runner: Option<Arc<dyn HealthCheckRunner>>,
}

impl ApplyCtxBuilder {
    /// Создаёт builder с обязательными полями. Остальные настраиваются через
    /// setter'ы; если не вызвать — применятся sensible defaults.
    pub fn new(
        deadline: Instant,
        cancel: CancellationToken,
        sensitive: Arc<SensitiveStore>,
        backup_root: PathBuf,
        log_dir: PathBuf,
        defers: Arc<Journal>,
    ) -> Self {
        Self {
            deadline,
            cancel,
            sensitive,
            backup_root,
            log_dir,
            defers,
            log_span: None,
            runr: None,
            systemd: None,
            validator: None,
            health_check_runner: None,
        }
    }

    /// Привязать tracing-span к ApplyCtx. По умолчанию используется
    /// [`tracing::Span::none`].
    pub fn log_span(mut self, span: tracing::Span) -> Self {
        self.log_span = Some(span);
        self
    }

    /// Подключить runr-клиент. На ноде с `init_system = systemd` оставить
    /// без вызова — поле будет `None`.
    pub fn runr(mut self, handle: Arc<dyn RunrHandle>) -> Self {
        self.runr = Some(handle);
        self
    }

    /// Подключить systemd-клиент. На ноде с `init_system = runr` оставить
    /// без вызова — поле будет `None`.
    pub fn systemd(mut self, handle: Arc<dyn SystemdHandle>) -> Self {
        self.systemd = Some(handle);
        self
    }

    /// Подменить runner валидаторов (`validate_with`). По умолчанию —
    /// [`RealValidateRunner`] (исполняет argv через std::process::Command).
    pub fn validator(mut self, validator: Arc<dyn ValidateRunner>) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Подменить runner health-check'ов. По умолчанию —
    /// [`NoopHealthCheckRunner`]; production CLI обязан явно передать
    /// `RealHealthCheckRunner`.
    pub fn health_check_runner(mut self, runner: Arc<dyn HealthCheckRunner>) -> Self {
        self.health_check_runner = Some(runner);
        self
    }

    /// Собирает [`ApplyCtx`]. Никаких отказов на этом шаге нет — все
    /// обязательные поля проверяются типом при вызове `new`.
    pub fn build(self) -> ApplyCtx {
        ApplyCtx {
            deadline: self.deadline,
            cancel: self.cancel,
            log_span: self.log_span.unwrap_or_else(tracing::Span::none),
            sensitive: self.sensitive,
            backup_root: self.backup_root,
            log_dir: self.log_dir,
            changed_resources: Arc::new(Mutex::new(HashSet::new())),
            defers: self.defers,
            runr: self.runr,
            systemd: self.systemd,
            runr_daemon_reload_done: Arc::new(AtomicBool::new(false)),
            systemd_daemon_reload_done: Arc::new(AtomicBool::new(false)),
            validator: self
                .validator
                .unwrap_or_else(|| Arc::new(RealValidateRunner)),
            health_check_runner: self
                .health_check_runner
                .unwrap_or_else(|| Arc::new(NoopHealthCheckRunner)),
            published_facts: Arc::new(Mutex::new(HashMap::new())),
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

/// Overlay-FactsSource: сначала проверяет runtime-published факты
/// (например, `pg_sql.query store_as_fact`), и только потом — основной
/// snapshot. Позволяет «вылитому» примитиву факту быть виденным через
/// штатный `FactsSource::get` без отдельного API.
///
/// Lifetime связывает overlay с inner: оба живут только во время одного
/// apply-цикла. `published_facts` совпадает по Arc с тем, что лежит в
/// `ApplyCtx::published_facts` — оба используют одну и ту же
/// внутреннюю Mutex-карту, без копирования.
pub struct OverlayFactsSource<'a> {
    inner: &'a dyn FactsSource,
    published_facts: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

impl<'a> OverlayFactsSource<'a> {
    /// Создать overlay поверх `inner`. `published_facts` — тот же Arc,
    /// что и в `ApplyCtx::published_facts`: оба указывают на одну
    /// карту, читать/писать можно из обеих сторон.
    pub fn new(
        inner: &'a dyn FactsSource,
        published_facts: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    ) -> Self {
        Self {
            inner,
            published_facts,
        }
    }
}

impl<'a> FactsSource for OverlayFactsSource<'a> {
    fn get(&self, name: &str) -> crate::facts::FactValue {
        // PoisonError: внутренняя HashMap после паники остаётся
        // консистентной — данные не теряются. Возвращаем как Known,
        // чтобы предоставлять published-факты приоритетно.
        if let Ok(guard) = self.published_facts.lock() {
            if let Some(value) = guard.get(name) {
                return crate::facts::FactValue::Known(value.clone());
            }
        }
        self.inner.get(name)
    }
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
#[allow(clippy::unwrap_used, clippy::panic)]
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

    #[test]
    fn apply_ctx_builder_with_defaults_matches_legacy_new() {
        // Без вызова setter'ов builder должен производить ApplyCtx с теми же
        // default'ами, что и legacy `ApplyCtx::new`: пустой span, None handle'ы,
        // RealValidateRunner, NoopHealthCheckRunner.
        use crate::sensitive::SensitiveStore;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let journal = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtxBuilder::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            journal.clone(),
        )
        .build();
        assert!(ctx.runr.is_none());
        assert!(ctx.systemd.is_none());
        assert_eq!(ctx.backup_root, PathBuf::from("/tmp/backup"));
        assert_eq!(ctx.log_dir, PathBuf::from("/tmp/log"));
    }

    #[test]
    fn apply_ctx_builder_sets_runr_and_systemd_when_supplied() {
        use crate::sensitive::SensitiveStore;
        use tempfile::TempDir;

        // Стаб-реализации: возвращают ошибки, нам важно только что Some
        // в ApplyCtx.
        struct FakeRunr;
        impl RunrHandle for FakeRunr {
            fn base_url(&self) -> &str {
                "http://stub"
            }
            fn daemon_info(&self) -> Result<bosun_handles::DaemonInfo, bosun_handles::RunrError> {
                unreachable!()
            }
            fn daemon_reload(&self) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn service_start(
                &self,
                _: &str,
                _: bool,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn service_stop(
                &self,
                _: &str,
                _: bool,
                _: Option<&str>,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn service_restart(
                &self,
                _: &str,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn service_reload(
                &self,
                _: &str,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn timer_start(
                &self,
                _: &str,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn timer_stop(
                &self,
                _: &str,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn timer_enable(
                &self,
                _: &str,
                _: bool,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn timer_disable(
                &self,
                _: &str,
                _: bool,
            ) -> Result<bosun_handles::ActionAck, bosun_handles::RunrError> {
                unreachable!()
            }
            fn service_statuses(
                &self,
            ) -> Result<Vec<bosun_handles::ServiceStatus>, bosun_handles::RunrError> {
                unreachable!()
            }
            fn timer_statuses(
                &self,
            ) -> Result<Vec<bosun_handles::TimerStatus>, bosun_handles::RunrError> {
                unreachable!()
            }
            fn units_list(
                &self,
            ) -> Result<Vec<bosun_handles::UnitListItem>, bosun_handles::RunrError> {
                unreachable!()
            }
            fn verify_restart(
                &self,
                _: &str,
                _: &bosun_handles::ServiceStatus,
                _: std::time::Duration,
                _: std::time::Duration,
            ) -> Result<bosun_handles::ServiceStatus, bosun_handles::RunrError> {
                unreachable!()
            }
            fn verify_start(
                &self,
                _: &str,
                _: std::time::Duration,
                _: std::time::Duration,
            ) -> Result<bosun_handles::ServiceStatus, bosun_handles::RunrError> {
                unreachable!()
            }
        }

        let tmp = TempDir::new().unwrap();
        let journal = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtxBuilder::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            journal,
        )
        .runr(Arc::new(FakeRunr))
        .build();
        assert!(ctx.runr.is_some());
        assert_eq!(ctx.runr.as_ref().unwrap().base_url(), "http://stub");
    }

    /// Внутренний stub FactsSource: возвращает заранее заданное значение для
    /// одного имени, для всего остального — Unknown. Используется тестами
    /// OverlayFactsSource как «нижний» слой.
    struct StubFacts {
        name: String,
        value: serde_json::Value,
    }

    impl FactsSource for StubFacts {
        fn get(&self, name: &str) -> crate::facts::FactValue {
            if name == self.name {
                crate::facts::FactValue::Known(self.value.clone())
            } else {
                crate::facts::FactValue::Unknown {
                    reason: format!("stub: unknown '{name}'"),
                }
            }
        }
    }

    #[test]
    fn overlay_returns_published_fact_first() {
        // Если факт есть и в overlay, и в inner — overlay побеждает.
        let inner = StubFacts {
            name: "host.os".into(),
            value: serde_json::json!("from-inner"),
        };
        let published = Arc::new(Mutex::new(HashMap::from([(
            "host.os".to_string(),
            serde_json::json!("from-overlay"),
        )])));
        let overlay = OverlayFactsSource::new(&inner, published);
        match overlay.get("host.os") {
            crate::facts::FactValue::Known(v) => assert_eq!(v, serde_json::json!("from-overlay")),
            other => panic!("expected Known(from-overlay), got {other:?}"),
        }
    }

    #[test]
    fn overlay_falls_through_to_inner_when_not_published() {
        // Факт есть только в inner — overlay должен прозрачно его отдать.
        let inner = StubFacts {
            name: "init.system".into(),
            value: serde_json::json!("systemd"),
        };
        let published = Arc::new(Mutex::new(HashMap::new()));
        let overlay = OverlayFactsSource::new(&inner, published);
        match overlay.get("init.system") {
            crate::facts::FactValue::Known(v) => assert_eq!(v, serde_json::json!("systemd")),
            other => panic!("expected Known, got {other:?}"),
        }
    }

    #[test]
    fn overlay_returns_unknown_when_neither_layer_has_fact() {
        let inner = StubFacts {
            name: "x".into(),
            value: serde_json::json!(1),
        };
        let published = Arc::new(Mutex::new(HashMap::new()));
        let overlay = OverlayFactsSource::new(&inner, published);
        match overlay.get("nonexistent") {
            crate::facts::FactValue::Unknown { .. } => {}
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn overlay_picks_up_facts_published_after_construction() {
        // Семантика опубликованных фактов: ресурс A пишет, ресурс B читает в
        // ТОМ ЖЕ apply. Overlay должен видеть факт сразу после `publish_fact`,
        // даже если был сконструирован раньше.
        let inner = StubFacts {
            name: "irrelevant".into(),
            value: serde_json::json!(0),
        };
        let published: Arc<Mutex<HashMap<String, serde_json::Value>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let overlay = OverlayFactsSource::new(&inner, published.clone());

        // До publish — Unknown.
        assert!(matches!(
            overlay.get("pg.roles"),
            crate::facts::FactValue::Unknown { .. }
        ));

        // Эмулируем `ctx.publish_fact("pg.roles", ...)`.
        published
            .lock()
            .unwrap()
            .insert("pg.roles".to_string(), serde_json::json!(["admin", "ro"]));

        // Теперь Known.
        match overlay.get("pg.roles") {
            crate::facts::FactValue::Known(v) => assert_eq!(v, serde_json::json!(["admin", "ro"])),
            other => panic!("expected Known after publish, got {other:?}"),
        }
    }
}
