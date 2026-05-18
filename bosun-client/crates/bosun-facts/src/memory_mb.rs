//! Коллектор `memory_mb` — cgroup-aware с fallback на MemTotal из meminfo.
//!
//! Алгоритм:
//! - v2: `<root_fs>/sys/fs/cgroup/memory.max`. "max" → fallback на MemTotal.
//! - v1: `<root_fs>/sys/fs/cgroup/memory/memory.limit_in_bytes`. Значение
//!   >= `CGROUP_V1_UNLIMITED` (магическое 9223372036854771712) → fallback.
//! - Unknown cgroup-version → fallback на MemTotal.
//!
//! MemTotal парсится из `<root_fs>/proc/meminfo`, поле `MemTotal:`,
//! значение в kB, конвертируется в MB через integer-деление на 1024.
//!
//! Возвращает `Unknown` только если ни cgroup, ни meminfo не дали значение —
//! что на нормальной Linux-ноде не должно происходить.

use std::fs;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use crate::cgroup::{detect_version, CgroupVersion};
use crate::collector::{Fact, FactCollectCtx};

/// Magic-value "без лимита" в v1 cgroup. Берётся PAGE_COUNTER_MAX в ядре.
const CGROUP_V1_UNLIMITED: u64 = 9_223_372_036_854_771_712;

pub struct MemoryMbFact;

impl Fact for MemoryMbFact {
    fn name(&self) -> &str {
        "memory_mb"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Static
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AtStart
    }
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue {
        let cgroup_mb = match detect_version(&ctx.root_fs) {
            CgroupVersion::V2 => v2_bytes(&ctx.root_fs).map(bytes_to_mb),
            CgroupVersion::V1 => v1_bytes(&ctx.root_fs).map(bytes_to_mb),
            CgroupVersion::Unknown => None,
        };
        let mb = cgroup_mb.or_else(|| meminfo_total_mb(&ctx.root_fs));
        match mb {
            Some(m) => FactValue::Known(serde_json::json!(m)),
            None => FactValue::Unknown {
                reason: "no cgroup memory limit and /proc/meminfo unreadable".to_string(),
            },
        }
    }
}

fn v2_bytes(root_fs: &std::path::Path) -> Option<u64> {
    let path = root_fs.join("sys/fs/cgroup/memory.max");
    let content = fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    if trimmed == "max" {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

fn v1_bytes(root_fs: &std::path::Path) -> Option<u64> {
    let path = root_fs.join("sys/fs/cgroup/memory/memory.limit_in_bytes");
    let content = fs::read_to_string(&path).ok()?;
    let bytes: u64 = content.trim().parse().ok()?;
    if bytes >= CGROUP_V1_UNLIMITED {
        return None;
    }
    Some(bytes)
}

/// Парсит `MemTotal: <N> kB` из meminfo и переводит в MB.
fn meminfo_total_mb(root_fs: &std::path::Path) -> Option<u64> {
    let path = root_fs.join("proc/meminfo");
    let content = fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.len() < 2 {
            return None;
        }
        let kb: u64 = tokens[0].parse().ok()?;
        // meminfo всегда в kB, проверка для будущей защиты.
        if tokens[1] != "kB" {
            return None;
        }
        return Some(kb / 1024);
    }
    None
}

fn bytes_to_mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_file(root: &std::path::Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn known_u64(v: &FactValue) -> u64 {
        match v {
            FactValue::Known(json) => json.as_u64().unwrap(),
            other => panic!("expected Known, got {other:?}"),
        }
    }

    #[test]
    fn v2_max_falls_back_to_meminfo() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "memory\n");
        write_file(tmp.path(), "sys/fs/cgroup/memory.max", "max\n");
        write_file(
            tmp.path(),
            "proc/meminfo",
            "MemTotal:        4096000 kB\nSwapTotal:    0 kB\n",
        );
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert_eq!(known_u64(&v), 4000); // 4096000 kB / 1024 = 4000 MB
    }

    #[test]
    fn v2_explicit_bytes_translated_to_mb() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "memory\n");
        // 512 MB в байтах
        write_file(
            tmp.path(),
            "sys/fs/cgroup/memory.max",
            &format!("{}\n", 512u64 * 1024 * 1024),
        );
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert_eq!(known_u64(&v), 512);
    }

    #[test]
    fn v1_unlimited_magic_falls_back() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/self/cgroup", "11:memory:/user.slice\n");
        write_file(
            tmp.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            &format!("{}\n", CGROUP_V1_UNLIMITED),
        );
        write_file(tmp.path(), "proc/meminfo", "MemTotal:        2097152 kB\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert_eq!(known_u64(&v), 2048); // 2097152 / 1024
    }

    #[test]
    fn v1_above_unlimited_threshold_also_falls_back() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/self/cgroup", "11:memory:/user.slice\n");
        // Чуть выше threshold — тоже считается unlimited.
        write_file(
            tmp.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "9223372036854775000\n",
        );
        write_file(tmp.path(), "proc/meminfo", "MemTotal:        2097152 kB\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert_eq!(known_u64(&v), 2048);
    }

    #[test]
    fn v1_explicit_bytes_translated_to_mb() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/self/cgroup", "11:memory:/user.slice\n");
        write_file(
            tmp.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            &format!("{}\n", 1024u64 * 1024 * 1024), // 1 GB
        );
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert_eq!(known_u64(&v), 1024);
    }

    #[test]
    fn unknown_cgroup_falls_back_to_meminfo() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/meminfo", "MemTotal:        524288 kB\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert_eq!(known_u64(&v), 512);
    }

    #[test]
    fn meminfo_without_memtotal_returns_unknown() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/meminfo", "SwapTotal:        0 kB\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert!(matches!(v, FactValue::Unknown { .. }));
    }

    #[test]
    fn meminfo_malformed_returns_unknown() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/meminfo", "MemTotal: garbage\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert!(matches!(v, FactValue::Unknown { .. }));
    }

    #[test]
    fn no_files_at_all_returns_unknown() {
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = MemoryMbFact.collect(&ctx);
        assert!(matches!(v, FactValue::Unknown { .. }));
    }

    #[test]
    fn name_is_memory_mb() {
        assert_eq!(MemoryMbFact.name(), "memory_mb");
    }
}
