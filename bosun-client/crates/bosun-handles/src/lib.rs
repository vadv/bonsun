//! Sync-traits для абстракции над клиентами runr (HTTP) и systemd (dbus).
//!
//! Зачем отдельный крейт. `bosun-core` определяет `ApplyCtx`, который должен
//! уметь хранить handle на runr/systemd. Если бы trait'ы жили в самом
//! `bosun-core`, ему пришлось бы тянуть `bosun-runr-client` и
//! `bosun-systemd-client` — а они уже зависят от `bosun-core` через прочие
//! модули. Получился бы цикл. `bosun-handles` решает это, занимая место
//! «выше» клиентов в графе зависимостей: он импортирует клиентские типы и
//! предоставляет trait + blanket impl, а `bosun-core` импортирует только
//! трейты.
//!
//! Trait'ы повторяют публичную поверхность клиентов один-в-один: для
//! тестов достаточно подменить trait-object'ом, а в production достаются
//! клиенты по `Arc::clone`.

use std::time::Duration;

pub use bosun_runr_client::{
    ActionAck, DaemonInfo, RunrError, ServiceStatus, TimerStatus, UnitListItem,
};
pub use bosun_systemd_client::{SystemdError, UnitInfo};

/// Поведение runr-клиента, нужное примитивам и replay-циклу. Сигнатуры
/// повторяют `bosun_runr_client::Client` 1:1 — это сознательный выбор,
/// чтобы в production обходиться `impl RunrHandle for Client`, а в тестах
/// подставить mock без переоформления возвращаемых типов.
///
/// `Send + Sync` обязательны: ApplyCtx клонируется по `Arc`, оркестратор
/// держит handle сквозь весь apply и может вызывать его из произвольного
/// worker'а (даже если сейчас всё последовательно).
pub trait RunrHandle: Send + Sync {
    fn base_url(&self) -> &str;
    fn daemon_info(&self) -> Result<DaemonInfo, RunrError>;
    fn daemon_reload(&self) -> Result<ActionAck, RunrError>;
    fn service_start(&self, name: &str, idempotent: bool) -> Result<ActionAck, RunrError>;
    fn service_stop(
        &self,
        name: &str,
        force: bool,
        timeout_humantime: Option<&str>,
    ) -> Result<ActionAck, RunrError>;
    fn service_restart(&self, name: &str) -> Result<ActionAck, RunrError>;
    fn service_reload(&self, name: &str) -> Result<ActionAck, RunrError>;
    fn timer_start(&self, name: &str) -> Result<ActionAck, RunrError>;
    fn timer_stop(&self, name: &str) -> Result<ActionAck, RunrError>;
    fn timer_enable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError>;
    fn timer_disable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError>;
    fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError>;
    fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError>;
    fn units_list(&self) -> Result<Vec<UnitListItem>, RunrError>;
    /// Polling-проверка фактического рестарта по диффу `restarts`. Удобный
    /// blanket-метод поверх `service_statuses`, реализованный для production
    /// клиента в `bosun_runr_client::verify_restart`.
    fn verify_restart(
        &self,
        name: &str,
        before: &ServiceStatus,
        poll_interval: Duration,
        poll_total: Duration,
    ) -> Result<ServiceStatus, RunrError>;
}

impl RunrHandle for bosun_runr_client::Client {
    fn base_url(&self) -> &str {
        bosun_runr_client::Client::base_url(self)
    }
    fn daemon_info(&self) -> Result<DaemonInfo, RunrError> {
        bosun_runr_client::Client::daemon_info(self)
    }
    fn daemon_reload(&self) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::daemon_reload(self)
    }
    fn service_start(&self, name: &str, idempotent: bool) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::service_start(self, name, idempotent)
    }
    fn service_stop(
        &self,
        name: &str,
        force: bool,
        timeout_humantime: Option<&str>,
    ) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::service_stop(self, name, force, timeout_humantime)
    }
    fn service_restart(&self, name: &str) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::service_restart(self, name)
    }
    fn service_reload(&self, name: &str) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::service_reload(self, name)
    }
    fn timer_start(&self, name: &str) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::timer_start(self, name)
    }
    fn timer_stop(&self, name: &str) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::timer_stop(self, name)
    }
    fn timer_enable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::timer_enable(self, name, now)
    }
    fn timer_disable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
        bosun_runr_client::Client::timer_disable(self, name, now)
    }
    fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError> {
        bosun_runr_client::Client::service_statuses(self)
    }
    fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError> {
        bosun_runr_client::Client::timer_statuses(self)
    }
    fn units_list(&self) -> Result<Vec<UnitListItem>, RunrError> {
        bosun_runr_client::Client::units_list(self)
    }
    fn verify_restart(
        &self,
        name: &str,
        before: &ServiceStatus,
        poll_interval: Duration,
        poll_total: Duration,
    ) -> Result<ServiceStatus, RunrError> {
        bosun_runr_client::verify_restart(self, name, before, poll_interval, poll_total)
    }
}

/// Sync-фасад над systemd dbus-клиентом. Только подмножество, нужное
/// будущим `systemd_service`/`systemd_timer` примитивам (Phase E): start,
/// stop, restart, reload, enable, disable, daemon_reload и `unit_info`
/// для InvocationID-сравнения.
///
/// В Phase D реализация нужна лишь чтобы зарезервировать поле
/// `ApplyCtx.systemd` и убедиться, что trait-object компилируется в
/// `Option<Arc<dyn SystemdHandle>>`. Конкретный adapter для
/// `bosun_systemd_client::BlockingSystemdManager` добавится в Phase E
/// вместе с примитивами `systemd.service` / `systemd.timer` — там же
/// доопределится семантика `JobHandle + wait_for_job` поверх sync API.
pub trait SystemdHandle: Send + Sync {
    fn daemon_reload(&self) -> Result<(), SystemdError>;
    fn needs_daemon_reload(&self, unit_name: &str) -> Result<bool, SystemdError>;
    fn start_unit(&self, name: &str) -> Result<(), SystemdError>;
    fn stop_unit(&self, name: &str) -> Result<(), SystemdError>;
    fn restart_unit(&self, name: &str) -> Result<(), SystemdError>;
    fn reload_unit(&self, name: &str) -> Result<(), SystemdError>;
    fn enable_unit(&self, name: &str) -> Result<(), SystemdError>;
    fn disable_unit(&self, name: &str) -> Result<(), SystemdError>;
    fn unit_info(&self, name: &str) -> Result<UnitInfo, SystemdError>;
}
