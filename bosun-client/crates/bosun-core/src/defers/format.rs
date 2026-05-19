//! Формат файла журнала defers: `DeferEntry`, `DeferAction`, `HealthCheck`.
//!
//! Каждая запись — отдельный JSON-файл с расширением `.deferred`. Поля
//! фиксированы в design-секции «Формат файла»; новые поля добавляются с
//! увеличением `spec_version`.
//!
//! Маппинг action ↔ wire-формат: enum `DeferAction` сериализуется в
//! строковое поле `action` через `#[serde(tag = "action", rename_all =
//! "snake_case")]`. Так wire-формат остаётся читаемым (`"action":
//! "restart"`), а варианты с полезной нагрузкой (`Command { argv }`)
//! получают свои собственные поля рядом.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::priority::DeferPriority;

/// Текущая версия формата файла. При несовместимом изменении схемы
/// инкрементировать и при чтении ловить mismatch.
pub const CURRENT_SPEC_VERSION: u16 = 1;

/// Действие, которое нужно выполнить отложенно.
///
/// Сериализуется внутрь `DeferEntry` через внешний тэг `action`. Варианты
/// без полезной нагрузки идентифицируются строкой; `Command` несёт
/// `argv` рядом в том же объекте.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeferAction {
    /// Запуск unit'а.
    Start,
    /// Остановка unit'а.
    Stop,
    /// Restart unit'а — самый сильный из reload-/restart-семьи.
    Restart,
    /// Reload unit'а.
    Reload,
    /// Reload-or-restart — daemon-сам решает.
    ReloadOrRestart,
    /// Отложенный запуск произвольной команды. `argv[0]` — путь к
    /// исполняемому файлу, остальное — аргументы. Shell не вмешивается.
    #[serde(rename = "command.run")]
    Command {
        /// Массив argv, передаваемый в `Command::new(argv[0]).args(&argv[1..])`.
        argv: Vec<String>,
    },
    /// `systemctl daemon-reload` / `runr daemon-reload`.
    DaemonReload,
}

impl DeferAction {
    /// Короткий слаг для имени файла (`restart`, `reload`, `reload_or_restart`,
    /// `command.run`, `daemon_reload`, `start`, `stop`). Не зависит от serde
    /// и стабилен даже если в будущем поменяется внешний тэг.
    pub const fn filename_slug(&self) -> &'static str {
        match self {
            DeferAction::Start => "start",
            DeferAction::Stop => "stop",
            DeferAction::Restart => "restart",
            DeferAction::Reload => "reload",
            DeferAction::ReloadOrRestart => "reload_or_restart",
            DeferAction::Command { .. } => "command.run",
            DeferAction::DaemonReload => "daemon_reload",
        }
    }

    /// Приоритет, который соответствует action'у. Используется как
    /// дефолт при конструировании `DeferEntry` через builder/конструкторы.
    pub const fn default_priority(&self) -> DeferPriority {
        match self {
            DeferAction::Restart | DeferAction::Start | DeferAction::Stop => DeferPriority::Restart,
            DeferAction::ReloadOrRestart => DeferPriority::ReloadOrRestart,
            DeferAction::Reload => DeferPriority::Reload,
            DeferAction::Command { .. } => DeferPriority::Command,
            DeferAction::DaemonReload => DeferPriority::DaemonReload,
        }
    }
}

/// Health-check, выполняемый после успешного действия.
///
/// Структурно tagged с `kind: cmd|url` — Starlark маппит keyword-аргументы
/// `health_check_cmd=`/`health_check_url=` в эти варианты, см. design-секцию
/// «Health check».
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum HealthCheck {
    /// Выполнить argv и считать health-check успешным при exit code 0.
    Cmd {
        cmd: Vec<String>,
        timeout_sec: Option<u32>,
        retry_count: Option<u32>,
        retry_interval_sec: Option<u32>,
    },
    /// GET URL, сравнить status code с `expected_status` (default 200).
    Url {
        url: String,
        expected_status: Option<u16>,
        timeout_sec: Option<u32>,
        retry_count: Option<u32>,
        retry_interval_sec: Option<u32>,
    },
}

/// Запись в журнале defers. Один файл = одна запись.
///
/// `id` дублирует `<init_system>.<action>:<target>` из имени файла и
/// служит идемпотентным dedup-ключом. `priority` дублирует префикс
/// sortkey, чтобы читалки не парсили имя файла.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeferEntry {
    /// Версия формата файла; для совместимости при росте схемы.
    pub spec_version: u16,
    /// Dedup-ключ. Формат: `<init_system>.<action_slug>:<target>` либо
    /// `command.run:<name>` для `Command`, либо `<init_system>.daemon_reload`
    /// для `DaemonReload`.
    pub id: String,
    /// Действие, которое будет выполнено.
    #[serde(flatten)]
    pub action: DeferAction,
    /// `systemd` | `runr` | пустая строка для command/daemon_reload без привязки.
    pub init_system: String,
    /// Имя unit'а либо user-defined имя команды.
    pub target: String,
    /// Опциональная команда валидации перед выполнением action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate_cmd: Option<Vec<String>>,
    /// Опциональный health-check, который выполняется после успешного action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check: Option<HealthCheck>,
    /// Приоритет (дублирует префикс имени файла).
    pub priority: DeferPriority,
    /// UTC timestamp первой постановки в журнал.
    pub enqueued_at: DateTime<Utc>,
    /// Источники notify, тригернувшие defer. Сортируется и дедуплицируется
    /// при идемпотентной повторной вставке — чтобы content был стабильным.
    pub enqueued_by: Vec<String>,
    /// Сколько раз replay уже пытался выполнить эту запись.
    pub attempt_count: u32,
    /// Лимит попыток. По достижении файл переезжает в `.manual_clear`.
    pub max_attempts: u32,
}

impl DeferEntry {
    /// Имя файла журнала: `<sortkey>-<id>.deferred`. Используется и при
    /// записи, и при поиске для dedup/remove.
    pub fn filename(&self) -> String {
        format!("{}-{}.deferred", self.priority.sortkey(), self.id)
    }

    /// Имя файла для расширения `.manual_clear` — defer-промоушен.
    pub fn manual_clear_filename(&self) -> String {
        format!("{}-{}.manual_clear", self.priority.sortkey(), self.id)
    }

    /// Канонический ключ dedup. Совпадает с `id` и подаётся как часть
    /// имени файла; вынесен в отдельный метод для читаемости.
    pub fn dedup_key(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for DeferEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeferEntry({})", self.id)
    }
}

/// Канонический id для пары `(init_system, action, target)`. Используется
/// и при конструировании записи, и при дедупе. Для `Command` формат
/// `command.run:<target>` без префикса init_system, потому что команды не
/// привязаны к init-системе. Для `DaemonReload` — `<init_system>.daemon_reload`.
pub fn make_id(init_system: &str, action: &DeferAction, target: &str) -> String {
    match action {
        DeferAction::Command { .. } => format!("command.run:{target}"),
        DeferAction::DaemonReload => {
            if init_system.is_empty() {
                "daemon_reload".to_string()
            } else {
                format!("{init_system}.daemon_reload")
            }
        }
        other => format!("{}.{}:{}", init_system, other.filename_slug(), target),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_entry() -> DeferEntry {
        DeferEntry {
            spec_version: CURRENT_SPEC_VERSION,
            id: "systemd.restart:nginx".to_string(),
            action: DeferAction::Restart,
            init_system: "systemd".to_string(),
            target: "nginx.service".to_string(),
            validate_cmd: None,
            health_check: Some(HealthCheck::Url {
                url: "http://127.0.0.1/healthz".to_string(),
                expected_status: Some(200),
                timeout_sec: Some(10),
                retry_count: None,
                retry_interval_sec: None,
            }),
            priority: DeferPriority::Restart,
            enqueued_at: Utc.with_ymd_and_hms(2026, 5, 19, 14, 32, 11).unwrap(),
            enqueued_by: vec![
                "file.content:/etc/nginx/nginx.conf".to_string(),
                "file.content:/etc/nginx/sites-enabled/default".to_string(),
            ],
            attempt_count: 0,
            max_attempts: 3,
        }
    }

    #[test]
    fn make_id_for_service_action() {
        let id = make_id("systemd", &DeferAction::Restart, "nginx.service");
        assert_eq!(id, "systemd.restart:nginx.service");
    }

    #[test]
    fn make_id_for_runr_reload() {
        let id = make_id("runr", &DeferAction::Reload, "postgres");
        assert_eq!(id, "runr.reload:postgres");
    }

    #[test]
    fn make_id_for_command_ignores_init_system() {
        let action = DeferAction::Command {
            argv: vec!["/usr/bin/true".into()],
        };
        let id = make_id("", &action, "smoke-test");
        assert_eq!(id, "command.run:smoke-test");
    }

    #[test]
    fn make_id_for_daemon_reload_with_init_system() {
        let id = make_id("systemd", &DeferAction::DaemonReload, "");
        assert_eq!(id, "systemd.daemon_reload");
    }

    #[test]
    fn make_id_for_daemon_reload_without_init_system() {
        let id = make_id("", &DeferAction::DaemonReload, "");
        assert_eq!(id, "daemon_reload");
    }

    #[test]
    fn filename_uses_sortkey_prefix() {
        let entry = sample_entry();
        assert_eq!(entry.filename(), "0r-systemd.restart:nginx.deferred");
        assert_eq!(
            entry.manual_clear_filename(),
            "0r-systemd.restart:nginx.manual_clear"
        );
    }

    #[test]
    fn defer_action_filename_slug_covers_all_variants() {
        assert_eq!(DeferAction::Start.filename_slug(), "start");
        assert_eq!(DeferAction::Stop.filename_slug(), "stop");
        assert_eq!(DeferAction::Restart.filename_slug(), "restart");
        assert_eq!(DeferAction::Reload.filename_slug(), "reload");
        assert_eq!(
            DeferAction::ReloadOrRestart.filename_slug(),
            "reload_or_restart"
        );
        assert_eq!(
            DeferAction::Command {
                argv: vec!["x".into()]
            }
            .filename_slug(),
            "command.run"
        );
        assert_eq!(DeferAction::DaemonReload.filename_slug(), "daemon_reload");
    }

    #[test]
    fn defer_action_default_priority() {
        assert_eq!(
            DeferAction::Restart.default_priority(),
            DeferPriority::Restart
        );
        assert_eq!(
            DeferAction::ReloadOrRestart.default_priority(),
            DeferPriority::ReloadOrRestart
        );
        assert_eq!(
            DeferAction::Reload.default_priority(),
            DeferPriority::Reload
        );
        assert_eq!(
            DeferAction::Command {
                argv: vec!["x".into()]
            }
            .default_priority(),
            DeferPriority::Command
        );
        assert_eq!(
            DeferAction::DaemonReload.default_priority(),
            DeferPriority::DaemonReload
        );
    }

    #[test]
    fn entry_round_trip_json() {
        let entry = sample_entry();
        let json = serde_json::to_string(&entry).unwrap();
        let back: DeferEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn entry_serializes_action_as_flat_tag() {
        // action: "restart" должен быть на верхнем уровне рядом с остальными полями,
        // а не вложен в { "action": { "kind": "restart" } }. Это ключ для design-секции
        // «Формат файла».
        let entry = sample_entry();
        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["action"], "restart");
        assert_eq!(value["target"], "nginx.service");
        assert_eq!(value["priority"], "restart");
        assert_eq!(value["spec_version"], 1);
    }

    #[test]
    fn entry_with_command_action_serializes_argv() {
        let mut entry = sample_entry();
        entry.action = DeferAction::Command {
            argv: vec!["/usr/bin/echo".into(), "hi".into()],
        };
        entry.priority = DeferPriority::Command;
        entry.init_system = String::new();
        entry.target = "echo-hi".into();
        entry.id = "command.run:echo-hi".into();

        let value: serde_json::Value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["action"], "command.run");
        assert_eq!(value["argv"][0], "/usr/bin/echo");
        assert_eq!(value["argv"][1], "hi");
        let back: DeferEntry = serde_json::from_value(value).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn health_check_url_round_trip() {
        let hc = HealthCheck::Url {
            url: "http://localhost/healthz".to_string(),
            expected_status: Some(204),
            timeout_sec: Some(5),
            retry_count: Some(3),
            retry_interval_sec: Some(2),
        };
        let json = serde_json::to_string(&hc).unwrap();
        let back: HealthCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(hc, back);
    }

    #[test]
    fn health_check_cmd_round_trip() {
        let hc = HealthCheck::Cmd {
            cmd: vec!["/usr/bin/true".into()],
            timeout_sec: Some(2),
            retry_count: Some(1),
            retry_interval_sec: None,
        };
        let json = serde_json::to_string(&hc).unwrap();
        let back: HealthCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(hc, back);
    }

    #[test]
    fn missing_optional_fields_deserialize_to_none() {
        // Старые файлы могут не содержать validate_cmd/health_check —
        // должны парситься как None.
        let raw = r#"{
            "spec_version": 1,
            "id": "systemd.restart:nginx",
            "action": "restart",
            "init_system": "systemd",
            "target": "nginx.service",
            "priority": "restart",
            "enqueued_at": "2026-05-19T14:32:11Z",
            "enqueued_by": [],
            "attempt_count": 0,
            "max_attempts": 3
        }"#;
        let entry: DeferEntry = serde_json::from_str(raw).unwrap();
        assert!(entry.validate_cmd.is_none());
        assert!(entry.health_check.is_none());
    }
}
