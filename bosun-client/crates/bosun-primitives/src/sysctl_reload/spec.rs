//! Десериализуемая часть payload'а `sysctl.reload`.
//!
//! Узкая семантика — «применить параметры ядра из одного `.conf`-файла».
//! `sysctl --system` (загрузка всего `/etc/sysctl.d/`) сознательно не
//! поддерживается: bundle декларирует свой файл и отвечает только за него,
//! а трогать чужие drop-ins без явного указания — антипаттерн.

use std::path::PathBuf;

use serde::Deserialize;

/// Spec примитива `sysctl.reload`.
///
/// `name` — идентификатор ресурса для дедупа и логов; не обязан совпадать
/// с filename'ом (например, можно держать `name="kernel.tuning"` и
/// `path="/etc/sysctl.d/60-bosun.conf"`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SysctlReloadSpec {
    /// Имя ресурса для реестра и логирования.
    pub name: String,
    /// Путь к sysctl `.conf`-файлу, который нужно загрузить через
    /// `sysctl -p <path>`. Файл должен существовать на момент apply —
    /// типично он создаётся file.content'ом в том же bundle'е.
    pub path: PathBuf,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_with_path() {
        let json = serde_json::json!({
            "name": "bosun-kernel",
            "path": "/etc/sysctl.d/60-bosun.conf",
        });
        let spec: SysctlReloadSpec = serde_json::from_value(json).unwrap();
        assert_eq!(spec.name, "bosun-kernel");
        assert_eq!(spec.path, PathBuf::from("/etc/sysctl.d/60-bosun.conf"));
    }

    #[test]
    fn deserialize_missing_name_is_error() {
        let json = serde_json::json!({ "path": "/x" });
        let err = serde_json::from_value::<SysctlReloadSpec>(json).unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn deserialize_missing_path_is_error() {
        let json = serde_json::json!({ "name": "x" });
        let err = serde_json::from_value::<SysctlReloadSpec>(json).unwrap_err();
        assert!(err.to_string().contains("path"));
    }
}
