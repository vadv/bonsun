//! Коллектор `init_system` — читает `/proc/1/comm` и классифицирует PID 1.
//!
//! Значения: `systemd`, `runit`, `init`, `unknown`.
//! `/proc/1/exe` намеренно не читаем: требует root, без него EACCES — а procfs
//! `/proc/1/comm` доступен любому пользователю.

use std::fs;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use crate::collector::{Fact, FactCollectCtx};

pub struct InitSystemFact;

impl InitSystemFact {
    fn classify(raw: &str) -> &'static str {
        // `comm` всегда одно слово, но защитимся от пробелов/мусора.
        let token = raw.split_whitespace().next().unwrap_or("");
        match token {
            "systemd" => "systemd",
            "runit" => "runit",
            "init" => "init",
            _ => "unknown",
        }
    }
}

impl Fact for InitSystemFact {
    fn name(&self) -> &str {
        "init_system"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Static
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AtStart
    }
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue {
        let path = ctx.root_fs.join("proc/1/comm");
        match fs::read_to_string(&path) {
            Ok(s) => {
                let classified = Self::classify(s.trim());
                FactValue::Known(serde_json::Value::String(classified.to_string()))
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

    fn write_comm(root: &std::path::Path, content: &str) {
        let dir = root.join("proc/1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("comm"), content).unwrap();
    }

    fn collect_for(content: &str) -> FactValue {
        let tmp = TempDir::new().unwrap();
        write_comm(tmp.path(), content);
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        InitSystemFact.collect(&ctx)
    }

    #[test]
    fn classifies_systemd() {
        let v = collect_for("systemd\n");
        assert_eq!(v.value().unwrap(), &serde_json::json!("systemd"));
    }

    #[test]
    fn classifies_runit() {
        let v = collect_for("runit\n");
        assert_eq!(v.value().unwrap(), &serde_json::json!("runit"));
    }

    #[test]
    fn classifies_init() {
        let v = collect_for("init\n");
        assert_eq!(v.value().unwrap(), &serde_json::json!("init"));
    }

    #[test]
    fn classifies_unknown_for_arbitrary_text() {
        let v = collect_for("docker-init\n");
        assert_eq!(v.value().unwrap(), &serde_json::json!("unknown"));
    }

    #[test]
    fn classifies_unknown_for_empty_token() {
        let v = collect_for("");
        assert_eq!(v.value().unwrap(), &serde_json::json!("unknown"));
    }

    #[test]
    fn missing_proc_1_comm_returns_unknown() {
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = InitSystemFact.collect(&ctx);
        match v {
            FactValue::Unknown { reason } => assert!(reason.contains("comm"), "reason: {reason}"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }
}
