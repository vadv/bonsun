//! Типы ответов и запросов runr HTTP API.
//!
//! Все ответные структуры помечены `#[serde(deny_unknown_fields)]` — это
//! ловит расхождения схемы между клиентом и демоном на раннем этапе, вместо
//! тихого игнорирования новых полей. JSON-имена сохранены строго как в
//! `postgres-chiit/lib/runr/client.go`.

use serde::{Deserialize, Serialize};

/// Тип юнита в `units_list()`. Реальные значения runr — `"Service"`, `"Timer"`,
/// `"Cgroup"` (см. `UnitKind` в Go-клиенте).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum UnitKind {
    Service,
    Timer,
    Cgroup,
}

/// Метрики ресурсов cgroup. Присутствует только у юнитов типа `Cgroup` в
/// `UnitListItem`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupMetrics {
    pub pressure_some_avg10: f64,
    pub pressure_full_avg10: f64,
    pub mem_anon: u64,
    pub mem_file: u64,
    pub mem_other: u64,
}

/// Элемент унифицированного списка юнитов из `GET /api/v1/units`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitListItem {
    pub name: String,
    pub kind: UnitKind,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<CgroupMetrics>,
}

/// Снимок состояния сервиса из `GET /api/v1/services/statuses`. Поля строго
/// соответствуют `ServiceStatus` из Go-клиента; `started_at` хранится как
/// raw-строка RFC3339, поскольку runr-клиент не имеет своей привязки к
/// chrono-типам (это решает каждый потребитель отдельно).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceStatus {
    pub name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub restarts: u64,
    pub in_state_for_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downtime_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_in_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub autostart: bool,
    pub memory_rss_anon_bytes: u64,
    pub memory_rss_file_bytes: u64,
    pub cpu_usage_percent: f64,
}

/// Снимок состояния таймера из `GET /api/v1/timers/statuses`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerStatus {
    pub name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run: Option<String>,
    pub target_service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Информация о runr-демоне из `GET /api/v1/daemon/info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonInfo {
    pub name: String,
    pub version: String,
    pub started_at: String,
    pub pid: i32,
    pub self_vm_rss_bytes: u64,
    pub self_vm_hwm_bytes: u64,
    pub memory_vm_rss_bytes: u64,
    pub memory_vm_hwm_bytes: u64,
    pub cpu_usage_percent: f64,
    pub features: Vec<String>,
}

/// Унифицированный ack для всех action-эндпоинтов
/// (`/start`, `/stop`, `/restart`, `/reload`, `/enable`, `/disable`,
/// `/units/reload`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionAck {
    pub action_id: String,
    pub accepted_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ---------------------------------------------------------------------------
// Тела запросов. Не часть публичного API, но pub(crate) для использования в
// client.rs. Сериализуются через serde_json, никаких ручных format!.
// ---------------------------------------------------------------------------

/// Тело `POST /api/v1/services/<name>/start`.
#[derive(Debug, Serialize)]
pub(crate) struct StartOptions {
    pub idempotent: bool,
}

/// Тело `POST /api/v1/services/<name>/stop`. `timeout` — humantime-строка
/// (например, `"90s"`).
#[derive(Debug, Serialize)]
pub(crate) struct StopOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    pub force: bool,
}

/// Тело `POST /api/v1/services/<name>/restart`.
#[derive(Debug, Serialize)]
pub(crate) struct RestartOptions {
    pub stop: StopOptions,
    pub start: StartOptions,
}

/// Тело `POST /api/v1/timers/<name>/enable` и `POST /api/v1/timers/<name>/disable`.
#[derive(Debug, Serialize)]
pub(crate) struct TimerToggleNow {
    pub now: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn service_status_roundtrip_full() {
        // Покрывает все поля включая опциональные. Проверяем что после
        // serialize→deserialize получаем эквивалентную структуру.
        let original = ServiceStatus {
            name: "postgresql-15".to_string(),
            state: "Running".to_string(),
            pid: Some(12345),
            restarts: 7,
            in_state_for_ms: 3_600_000,
            uptime_ms: Some(86_400_000),
            downtime_ms: None,
            next_restart_in_ms: None,
            started_at: Some("2026-05-19T08:00:00Z".to_string()),
            autostart: true,
            memory_rss_anon_bytes: 1024 * 1024 * 512,
            memory_rss_file_bytes: 1024 * 1024 * 64,
            cpu_usage_percent: 1.25,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ServiceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn service_status_roundtrip_minimal() {
        // Сервис в Stopped: нет pid, uptime, started_at. Проверяем что
        // skip_serializing_if=Option::is_none не ломает round-trip.
        let original = ServiceStatus {
            name: "stopped-svc".to_string(),
            state: "Stopped".to_string(),
            pid: None,
            restarts: 0,
            in_state_for_ms: 1_000,
            uptime_ms: None,
            downtime_ms: Some(60_000),
            next_restart_in_ms: None,
            started_at: None,
            autostart: false,
            memory_rss_anon_bytes: 0,
            memory_rss_file_bytes: 0,
            cpu_usage_percent: 0.0,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ServiceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn service_status_deny_unknown_fields() {
        // Регрессия схемы должна детектиться: добавление произвольного поля
        // обязано ломать парсинг, а не молча игнорироваться.
        let json = r#"{
            "name": "x",
            "state": "Running",
            "restarts": 0,
            "in_state_for_ms": 0,
            "autostart": false,
            "memory_rss_anon_bytes": 0,
            "memory_rss_file_bytes": 0,
            "cpu_usage_percent": 0.0,
            "unexpected_field": "boom"
        }"#;
        let result: Result<ServiceStatus, _> = serde_json::from_str(json);
        assert!(result.is_err(), "deny_unknown_fields must reject extras");
    }

    #[test]
    fn timer_status_roundtrip() {
        let original = TimerStatus {
            name: "pg-vacuum".to_string(),
            state: "Active".to_string(),
            next_run: Some("2026-05-20T03:00:00Z".to_string()),
            target_service: "pg-vacuum-runner".to_string(),
            enabled: Some(true),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: TimerStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn unit_list_item_roundtrip_service() {
        let original = UnitListItem {
            name: "pg".to_string(),
            kind: UnitKind::Service,
            state: "Running".to_string(),
            summary: Some("pid=42".to_string()),
            metrics: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: UnitListItem = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn unit_list_item_roundtrip_cgroup_with_metrics() {
        // Cgroup с метриками — единственный случай, когда metrics не None.
        let original = UnitListItem {
            name: "postgres-cgroup".to_string(),
            kind: UnitKind::Cgroup,
            state: "Active".to_string(),
            summary: None,
            metrics: Some(CgroupMetrics {
                pressure_some_avg10: 0.5,
                pressure_full_avg10: 0.1,
                mem_anon: 1024,
                mem_file: 2048,
                mem_other: 0,
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: UnitListItem = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn action_ack_roundtrip() {
        let original = ActionAck {
            action_id: "act-7".to_string(),
            accepted_at: "2026-05-19T10:00:00Z".to_string(),
            message: Some("ok".to_string()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ActionAck = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn daemon_info_roundtrip() {
        let original = DaemonInfo {
            name: "runr".to_string(),
            version: "0.42.0".to_string(),
            started_at: "2026-05-19T00:00:00Z".to_string(),
            pid: 1,
            self_vm_rss_bytes: 50_000_000,
            self_vm_hwm_bytes: 60_000_000,
            memory_vm_rss_bytes: 500_000_000,
            memory_vm_hwm_bytes: 600_000_000,
            cpu_usage_percent: 0.1,
            features: vec!["cgroups".to_string(), "syslog".to_string()],
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: DaemonInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}
