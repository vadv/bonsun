//! Sync HTTP-клиент над `ureq::Agent`.

use std::time::Duration;

use ureq::Agent;

use crate::error::RunrError;
use crate::types::{
    ActionAck, DaemonInfo, RestartOptions, ServiceStatus, StartOptions, StopOptions, TimerStatus,
    TimerToggleNow, UnitListItem,
};

/// Sync-клиент runr API.
///
/// Содержит `ureq::Agent` с настроенными read/write timeout'ами и нормализованный
/// `base_url` (без trailing slash). Клонирование агента дёшево — он внутри
/// `Arc`, поэтому при необходимости разделить клиента между несколькими
/// потоками можно просто склонировать сам `Client`.
#[derive(Clone)]
pub struct Client {
    base_url: String,
    agent: Agent,
}

impl Client {
    /// Создаёт нового клиента для указанного `base_url` (например,
    /// `"http://127.0.0.1:8010"`) с общим таймаутом `timeout` на чтение и
    /// запись. Trailing slash в `base_url` отсекается, чтобы конкатенация с
    /// path-частями вида `"/api/v1/..."` не давала двойного слэша.
    pub fn new(base_url: impl Into<String>, timeout: Duration) -> Self {
        let base = base_url.into();
        let base_url = base.trim_end_matches('/').to_string();
        let agent = ureq::AgentBuilder::new()
            .timeout_read(timeout)
            .timeout_write(timeout)
            .build();
        Self { base_url, agent }
    }

    /// Возвращает нормализованный base URL — полезно для error-сообщений и
    /// логирования из orchestrator.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /api/v1/daemon/info`.
    pub fn daemon_info(&self) -> Result<DaemonInfo, RunrError> {
        self.get_json("/api/v1/daemon/info", None)
    }

    /// `POST /api/v1/units/reload` — перечитывает unit-файлы из репозитория.
    pub fn daemon_reload(&self) -> Result<ActionAck, RunrError> {
        self.post_json::<_, ActionAck>("/api/v1/units/reload", &EmptyBody {}, None)
    }

    /// `POST /api/v1/services/<name>/start`.
    pub fn service_start(&self, name: &str, idempotent: bool) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/services/{name}/start");
        self.post_json(&path, &StartOptions { idempotent }, Some(("service", name)))
    }

    /// `POST /api/v1/services/<name>/stop`. `timeout_humantime` — опциональная
    /// humantime-строка (`"90s"`, `"2min"` и т.п.), которая прокидывается как
    /// `StopOptions.timeout`.
    pub fn service_stop(
        &self,
        name: &str,
        force: bool,
        timeout_humantime: Option<&str>,
    ) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/services/{name}/stop");
        let body = StopOptions {
            timeout: timeout_humantime.map(str::to_string),
            force,
        };
        self.post_json(&path, &body, Some(("service", name)))
    }

    /// `POST /api/v1/services/<name>/restart`. Внутри отправляется
    /// `RestartOptions { stop: { force: false }, start: { idempotent: true } }`
    /// — те же дефолты, что использует chiit-клиент.
    pub fn service_restart(&self, name: &str) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/services/{name}/restart");
        let body = RestartOptions {
            stop: StopOptions {
                timeout: None,
                force: false,
            },
            start: StartOptions { idempotent: true },
        };
        self.post_json(&path, &body, Some(("service", name)))
    }

    /// `POST /api/v1/services/<name>/reload`.
    pub fn service_reload(&self, name: &str) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/services/{name}/reload");
        self.post_json(&path, &EmptyBody {}, Some(("service", name)))
    }

    /// `POST /api/v1/timers/<name>/start`.
    pub fn timer_start(&self, name: &str) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/timers/{name}/start");
        self.post_json(&path, &EmptyBody {}, Some(("timer", name)))
    }

    /// `POST /api/v1/timers/<name>/stop`.
    pub fn timer_stop(&self, name: &str) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/timers/{name}/stop");
        self.post_json(&path, &EmptyBody {}, Some(("timer", name)))
    }

    /// `POST /api/v1/timers/<name>/enable`. Флаг `now` соответствует
    /// `EnableTimerOptions.now` — запустить таймер сразу после enable.
    pub fn timer_enable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/timers/{name}/enable");
        self.post_json(&path, &TimerToggleNow { now }, Some(("timer", name)))
    }

    /// `POST /api/v1/timers/<name>/disable`.
    pub fn timer_disable(&self, name: &str, now: bool) -> Result<ActionAck, RunrError> {
        let path = format!("/api/v1/timers/{name}/disable");
        self.post_json(&path, &TimerToggleNow { now }, Some(("timer", name)))
    }

    /// `GET /api/v1/services/statuses`. Возвращает снимок состояния всех
    /// сервисов одним запросом — orchestrator кеширует результат на весь apply.
    pub fn service_statuses(&self) -> Result<Vec<ServiceStatus>, RunrError> {
        self.get_json("/api/v1/services/statuses", None)
    }

    /// `GET /api/v1/timers/statuses`.
    pub fn timer_statuses(&self) -> Result<Vec<TimerStatus>, RunrError> {
        self.get_json("/api/v1/timers/statuses", None)
    }

    /// `GET /api/v1/units` — унифицированный список всех юнитов: сервисы,
    /// таймеры, cgroups.
    pub fn units_list(&self) -> Result<Vec<UnitListItem>, RunrError> {
        self.get_json("/api/v1/units", None)
    }

    // -----------------------------------------------------------------
    // Внутренние хелперы.
    // -----------------------------------------------------------------

    /// Выполняет GET-запрос и парсит JSON-ответ. `unit_lookup` — пара
    /// `(kind, name)` для случая, когда 404 должна стать `NotFound{kind, name}`.
    fn get_json<T>(&self, path: &str, unit_lookup: Option<(&str, &str)>) -> Result<T, RunrError>
    where
        T: serde::de::DeserializeOwned,
    {
        let url = format!("{}{}", self.base_url, path);
        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|err| RunrError::from_ureq(err, &self.base_url, unit_lookup))?;
        Self::parse_json(response)
    }

    /// Выполняет POST с JSON-body и парсит JSON-ответ. Сериализация body
    /// идёт через `serde_json::to_value` + `ureq::Request::send_json`,
    /// никаких ручных `format!`. Ошибка сериализации запроса в практике
    /// недостижима (все наши body-структуры — простые Serialize-derive без
    /// custom impl), но защитный путь обязан вернуть типизированную ошибку,
    /// а не паниковать.
    fn post_json<B, T>(
        &self,
        path: &str,
        body: &B,
        unit_lookup: Option<(&str, &str)>,
    ) -> Result<T, RunrError>
    where
        B: serde::Serialize,
        T: serde::de::DeserializeOwned,
    {
        let url = format!("{}{}", self.base_url, path);
        let value = serde_json::to_value(body).map_err(|err| {
            RunrError::BadResponse(format!("failed to serialize request body: {err}"))
        })?;
        let response = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(value)
            .map_err(|err| RunrError::from_ureq(err, &self.base_url, unit_lookup))?;
        Self::parse_json(response)
    }

    fn parse_json<T>(response: ureq::Response) -> Result<T, RunrError>
    where
        T: serde::de::DeserializeOwned,
    {
        let body = response.into_string()?;
        serde_json::from_str(&body)
            .map_err(|err| RunrError::BadResponse(format!("{err} (body: {body})")))
    }
}

/// Маркерный тип для эндпоинтов без тела запроса. runr ожидает валидный JSON
/// (`{}`), а не пустую строку.
#[derive(serde::Serialize)]
struct EmptyBody {}
