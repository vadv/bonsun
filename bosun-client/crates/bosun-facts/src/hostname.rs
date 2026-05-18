//! Коллектор `hostname` — читает `/proc/sys/kernel/hostname`.
//!
//! Зачем через procfs, а не gethostname(3):
//! - Не вводим зависимость `nix` ради одной системной обёртки.
//! - procfs читается одинаково в любом контейнере и стандартно проксируется
//!   через bind-mount, что упрощает тестирование на tempdir.

use std::fs;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use crate::collector::{Fact, FactCollectCtx};

pub struct HostnameFact;

impl Fact for HostnameFact {
    fn name(&self) -> &str {
        "hostname"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Static
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AtStart
    }
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue {
        let path = ctx.root_fs.join("proc/sys/kernel/hostname");
        match fs::read_to_string(&path) {
            Ok(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return FactValue::Unknown {
                        reason: format!("{} contained only whitespace", path.display()),
                    };
                }
                FactValue::Known(serde_json::Value::String(trimmed.to_string()))
            }
            Err(e) => FactValue::Unknown {
                reason: format!("read {}: {e}", path.display()),
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_hostname(root: &std::path::Path, content: &str) {
        let dir = root.join("proc/sys/kernel");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hostname"), content).unwrap();
    }

    #[test]
    fn reads_hostname_and_trims_newline() {
        let tmp = TempDir::new().unwrap();
        write_hostname(tmp.path(), "node-01\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = HostnameFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!("node-01"));
    }

    #[test]
    fn reads_hostname_without_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        write_hostname(tmp.path(), "host");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = HostnameFact.collect(&ctx);
        assert_eq!(v.value().unwrap(), &serde_json::json!("host"));
    }

    #[test]
    fn missing_file_returns_unknown() {
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = HostnameFact.collect(&ctx);
        match v {
            FactValue::Unknown { reason } => {
                assert!(reason.contains("hostname"), "reason: {reason}")
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_only_file_returns_unknown() {
        let tmp = TempDir::new().unwrap();
        write_hostname(tmp.path(), "  \n\t  \n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = HostnameFact.collect(&ctx);
        assert!(matches!(v, FactValue::Unknown { .. }));
    }

    #[test]
    fn name_and_policy_are_stable() {
        assert_eq!(HostnameFact.name(), "hostname");
        assert!(matches!(
            HostnameFact.refresh_policy(),
            RefreshPolicy::AtStart
        ));
        assert_eq!(HostnameFact.category(), FactCategory::Static);
    }
}
