//! Приоритет defer-действий и lex-sortkey для имён файлов.
//!
//! Дизайн фиксирует двухсимвольный префикс имени файла, чтобы
//! `read_dir` + лексикографическая сортировка давала корректный
//! порядок выполнения без дополнительной in-memory сортировки по
//! отдельному полю.
//!
//! Порядок выполнения (от первого к последнему): restart → reload_or_restart
//! → reload → command → daemon_reload. План плана содержит таблицу с
//! префиксами `r0/r1/r2/c0/d0`, но это противоречит лексической сортировке:
//! `c0` < `r0`. Чтобы сохранить поведение из design-спека («`command.run`
//! после всех service-actions»), здесь использованы числовые префиксы
//! `0r/1r/2r/3c/4d` — они монотонны и не зависят от ASCII-капризов.
//!
//! Семантика порядка совпадает с chiit (`sort.Strings` по именам файлов
//! в `/tmp/chiit-defers/`), но префиксы у chiit отсутствовали — там
//! `restart-*.sh` шёл раньше `reload-*.sh` по лексической случайности.
//! Здесь это закреплено явно.
//!
//! См. `2026-05-19-bosun-runr-systemd-defers-design.md`, секция «Sortkey + дедуп».

use serde::{Deserialize, Serialize};

/// Приоритет defer-записи. Сериализуется в JSON в snake_case и хранится
/// в поле `priority` файла журнала — это дублирует prefix sortkey, чтобы
/// читалки журнала не вынуждены парсить имя файла.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeferPriority {
    /// Restart — субсумирует reload, выполняется первым.
    Restart,
    /// ReloadOrRestart — позволяет даунстриму выбрать reload, если unit поддерживает.
    ReloadOrRestart,
    /// Reload — самый мягкий вариант, выполняется последним среди service-actions.
    Reload,
    /// Command — отложенный shell/argv после всех service-actions.
    Command,
    /// DaemonReload — после всего остального, чтобы накатить unit-файлы.
    DaemonReload,
}

impl DeferPriority {
    /// Двухсимвольный префикс имени файла. Порядок lex-сортировки совпадает
    /// с приоритетом выполнения.
    pub const fn sortkey(self) -> &'static str {
        match self {
            DeferPriority::Restart => "0r",
            DeferPriority::ReloadOrRestart => "1r",
            DeferPriority::Reload => "2r",
            DeferPriority::Command => "3c",
            DeferPriority::DaemonReload => "4d",
        }
    }
}

/// Краткая функция-обёртка для использования из format/journal без
/// явной материализации enum'а.
pub const fn sortkey(priority: DeferPriority) -> &'static str {
    priority.sortkey()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn sortkey_prefixes_are_stable() {
        assert_eq!(DeferPriority::Restart.sortkey(), "0r");
        assert_eq!(DeferPriority::ReloadOrRestart.sortkey(), "1r");
        assert_eq!(DeferPriority::Reload.sortkey(), "2r");
        assert_eq!(DeferPriority::Command.sortkey(), "3c");
        assert_eq!(DeferPriority::DaemonReload.sortkey(), "4d");
    }

    #[test]
    fn lex_order_matches_priority() {
        // Главный инвариант модуля: лексикографический порядок
        // sortkey-префиксов совпадает с порядком выполнения. Перетасуем
        // и убедимся, что sort даёт правильную последовательность:
        // restart → reload_or_restart → reload → command → daemon_reload.
        let mut keys = [
            DeferPriority::DaemonReload.sortkey(),
            DeferPriority::Command.sortkey(),
            DeferPriority::Reload.sortkey(),
            DeferPriority::Restart.sortkey(),
            DeferPriority::ReloadOrRestart.sortkey(),
        ];
        keys.sort();
        assert_eq!(keys, ["0r", "1r", "2r", "3c", "4d"]);
    }

    #[test]
    fn priority_serializes_to_snake_case() {
        let p = DeferPriority::ReloadOrRestart;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"reload_or_restart\"");
    }

    #[test]
    fn priority_roundtrip_json() {
        for variant in [
            DeferPriority::Restart,
            DeferPriority::ReloadOrRestart,
            DeferPriority::Reload,
            DeferPriority::Command,
            DeferPriority::DaemonReload,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: DeferPriority = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }
}
