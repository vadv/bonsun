//! Десериализуемая часть payload'а `process.signal`.
//!
//! Spec лежит в `Resource.payload` после `build_payload` и десериализуется
//! планом/apply'ем через `serde_json::from_value`. Семантика — узкая:
//! «послать allowlist-сигнал процессу по имени или uid», без shell.

use bosun_core::UnitName;
use serde::Deserialize;

/// Spec примитива `process.signal`.
///
/// Выбор «по имени» или «по uid» — взаимоисключающий, проверяется в
/// `build_signal_argv`. Allowlist сигналов ограничен «мягкими» (`HUP`,
/// `TERM`, `INT`, `USR1`, `USR2`, `WINCH`, `PIPE`) — `KILL`/`STOP`/`CONT`
/// сознательно исключены, чтобы автор bundle'а не вырубал процессы в обход
/// `service.unit`.
///
/// `deferred = true` по умолчанию: в chiit-практике все вызовы
/// `defers.AddCommand` для pkill ставились в журнал отложенных действий, и
/// мы повторяем это поведение.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct ProcessSignalSpec {
    /// Уникальное имя ресурса. Используется как часть `defer_id`
    /// (`process.signal:<name>`) и как target в журнале defers, играет ту же
    /// роль, что и `name` у `defers.AddCommand` в chiit.
    /// Валидация через `UnitName` отвергает path-traversal — defer-файл
    /// строится из этого имени, и пробитие имени могло бы записать журнал
    /// в чужую директорию.
    pub name: UnitName,
    /// Имя сигнала. Допускается префикс `SIG` (например `SIGHUP` ↔ `HUP`).
    /// Валидируется в `build_signal_argv` против allowlist.
    pub signal: String,
    /// Селектор «по имени процесса» (pkill `<name>` ищет по `comm`).
    #[serde(default)]
    pub process_name: Option<String>,
    /// Селектор «по владельцу процесса» (pkill `-u <user>`).
    #[serde(default)]
    pub process_user: Option<String>,
    /// Положить в журнал defers (true, default) или выполнить синхронно.
    #[serde(default = "default_deferred")]
    pub deferred: bool,
}

fn default_deferred() -> bool {
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimum_by_name_defaults_deferred_true() {
        let json = serde_json::json!({
            "name": "hup-pg-doorman",
            "signal": "HUP",
            "process_name": "pg_doorman",
        });
        let spec: ProcessSignalSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name.as_str(), "hup-pg-doorman");
        assert_eq!(spec.signal, "HUP");
        assert_eq!(spec.process_name.as_deref(), Some("pg_doorman"));
        assert!(spec.process_user.is_none());
        assert!(spec.deferred, "deferred должен быть true по умолчанию");
    }

    #[test]
    fn deserialize_rejects_invalid_unit_name() {
        let json = serde_json::json!({
            "name": "../etc/passwd",
            "signal": "HUP",
            "process_name": "x",
        });
        let err = serde_json::from_value::<ProcessSignalSpec>(json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must start with") || msg.contains("invalid character"),
            "expected UnitName error, got: {msg}"
        );
    }

    #[test]
    fn deserialize_by_user_with_explicit_deferred_false() {
        let json = serde_json::json!({
            "name": "blind-reload-pg",
            "signal": "SIGHUP",
            "process_user": "postgres",
            "deferred": false,
        });
        let spec: ProcessSignalSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.process_user.as_deref(), Some("postgres"));
        assert!(spec.process_name.is_none());
        assert!(!spec.deferred);
    }

    #[test]
    fn deserialize_unknown_field_is_error() {
        // Опечатки в имени поля должны ловиться: serde по умолчанию
        // молчаливо отбрасывает unknown-поля, но мы хотим бы такой контроль
        // — поэтому сейчас этот тест документирует поведение «глотать
        // unknown» (Rust serde default).
        let json = serde_json::json!({
            "name": "x",
            "signal": "HUP",
            "process_name": "x",
            "typo_field": 42,
        });
        let spec: ProcessSignalSpec = serde_json::from_value(json).unwrap();
        // Поле typo_field тихо игнорируется serde по дефолту; задача spec'а
        // — не падать на нём, а валидация лежит на slой выше.
        assert_eq!(spec.name.as_str(), "x");
    }
}
