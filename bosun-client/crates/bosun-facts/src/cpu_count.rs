//! Коллектор `cpu_count` — cgroup-aware с fallback на num_cpus.
//!
//! Алгоритм:
//! - v2: читаем `<root_fs>/sys/fs/cgroup/cpu.max`, формат `"<quota> <period>"`.
//!   quota == "max" → num_cpus::get().
//!   иначе → ceil(quota / period).
//! - v1: `<root_fs>/sys/fs/cgroup/cpu/cpu.cfs_quota_us` и `cpu.cfs_period_us`.
//!   quota == -1 → num_cpus::get().
//!   иначе → ceil(quota / period).
//! - Unknown cgroup-version → num_cpus::get().
//!
//! Намеренно НЕ зависим от факта `is_pod`. На bare-metal cgroup-файлы
//! обычно содержат `max` (v2) или `-1` (v1), что само переключает
//! fallback. Это убирает ordering-зависимость между фактами.

use std::fs;

use bosun_core::{FactCategory, FactValue, RefreshPolicy};

use crate::cgroup::{detect_version, CgroupVersion};
use crate::collector::{Fact, FactCollectCtx};

pub struct CpuCountFact;

impl Fact for CpuCountFact {
    fn name(&self) -> &str {
        "cpu_count"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Static
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AtStart
    }
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue {
        let count = match detect_version(&ctx.root_fs) {
            CgroupVersion::V2 => v2_count(&ctx.root_fs).unwrap_or_else(num_cpus::get),
            CgroupVersion::V1 => v1_count(&ctx.root_fs).unwrap_or_else(num_cpus::get),
            CgroupVersion::Unknown => num_cpus::get(),
        };
        // CPU всегда >= 1 — даже limited cgroup даёт минимум 1 ядро для
        // полезной работы. Защитимся от деления, где quota < period:
        // ceil_div(123, 1000) = 1, всё ок.
        FactValue::Known(serde_json::json!(count.max(1)))
    }
}

fn v2_count(root_fs: &std::path::Path) -> Option<usize> {
    let path = root_fs.join("sys/fs/cgroup/cpu.max");
    let content = fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    let mut parts = trimmed.split_whitespace();
    let quota_token = parts.next()?;
    let period_token = parts.next()?;
    if quota_token == "max" {
        return None;
    }
    let quota: i64 = quota_token.parse().ok()?;
    let period: i64 = period_token.parse().ok()?;
    if period <= 0 || quota <= 0 {
        return None;
    }
    Some(ceil_div(quota as u64, period as u64) as usize)
}

fn v1_count(root_fs: &std::path::Path) -> Option<usize> {
    let quota_path = root_fs.join("sys/fs/cgroup/cpu/cpu.cfs_quota_us");
    let period_path = root_fs.join("sys/fs/cgroup/cpu/cpu.cfs_period_us");
    let quota_raw = fs::read_to_string(&quota_path).ok()?;
    let period_raw = fs::read_to_string(&period_path).ok()?;
    let quota: i64 = quota_raw.trim().parse().ok()?;
    let period: i64 = period_raw.trim().parse().ok()?;
    if quota <= 0 || period <= 0 {
        return None;
    }
    Some(ceil_div(quota as u64, period as u64) as usize)
}

/// Целочисленное `ceil(a / b)` для положительных значений.
/// Для b == 0 возвращает 0 — защита от divide-by-zero.
fn ceil_div(a: u64, b: u64) -> u64 {
    if b == 0 {
        return 0;
    }
    a.div_ceil(b)
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

    fn known_usize(v: &FactValue) -> usize {
        match v {
            FactValue::Known(json) => json.as_u64().unwrap() as usize,
            other => panic!("expected Known, got {other:?}"),
        }
    }

    #[test]
    fn ceil_div_examples() {
        assert_eq!(ceil_div(100_000, 100_000), 1);
        assert_eq!(ceil_div(150_000, 100_000), 2);
        assert_eq!(ceil_div(200_000, 100_000), 2);
        assert_eq!(ceil_div(250_000, 100_000), 3);
        assert_eq!(ceil_div(0, 100), 0);
        assert_eq!(ceil_div(50, 0), 0);
    }

    #[test]
    fn v2_quota_max_falls_back_to_num_cpus() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "cpu\n");
        write_file(tmp.path(), "sys/fs/cgroup/cpu.max", "max 100000\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert!(known_usize(&v) >= 1, "fallback должен дать >=1 ядро");
    }

    #[test]
    fn v2_explicit_quota_yields_ceil_division() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "cpu\n");
        // 250_000us / 100_000us = 2.5 → ceil = 3.
        write_file(tmp.path(), "sys/fs/cgroup/cpu.max", "250000 100000\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert_eq!(known_usize(&v), 3);
    }

    #[test]
    fn v2_exact_quota_yields_floor() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "cpu\n");
        // 200_000us / 100_000us = 2.0 → ceil = 2.
        write_file(tmp.path(), "sys/fs/cgroup/cpu.max", "200000 100000\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert_eq!(known_usize(&v), 2);
    }

    #[test]
    fn v2_sub_period_quota_yields_one() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "cpu\n");
        // 50_000us / 100_000us = 0.5 → ceil = 1.
        write_file(tmp.path(), "sys/fs/cgroup/cpu.max", "50000 100000\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert_eq!(known_usize(&v), 1);
    }

    #[test]
    fn v2_garbage_falls_back() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "cpu\n");
        write_file(tmp.path(), "sys/fs/cgroup/cpu.max", "garbage\n");
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert!(known_usize(&v) >= 1);
    }

    #[test]
    fn v1_negative_quota_falls_back() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "proc/self/cgroup",
            "11:cpu,cpuacct:/user.slice\n",
        );
        write_file(tmp.path(), "sys/fs/cgroup/cpu/cpu.cfs_quota_us", "-1\n");
        write_file(
            tmp.path(),
            "sys/fs/cgroup/cpu/cpu.cfs_period_us",
            "100000\n",
        );
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert!(known_usize(&v) >= 1);
    }

    #[test]
    fn v1_explicit_quota_yields_ceil() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "proc/self/cgroup",
            "11:cpu,cpuacct:/user.slice\n",
        );
        // 350_000us / 100_000us = 3.5 → 4.
        write_file(tmp.path(), "sys/fs/cgroup/cpu/cpu.cfs_quota_us", "350000\n");
        write_file(
            tmp.path(),
            "sys/fs/cgroup/cpu/cpu.cfs_period_us",
            "100000\n",
        );
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert_eq!(known_usize(&v), 4);
    }

    #[test]
    fn unknown_cgroup_falls_back_to_num_cpus() {
        let tmp = TempDir::new().unwrap();
        // Ничего не пишем — версия Unknown.
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = CpuCountFact.collect(&ctx);
        assert!(known_usize(&v) >= 1);
    }

    #[test]
    fn name_is_cpu_count() {
        assert_eq!(CpuCountFact.name(), "cpu_count");
        assert!(matches!(
            CpuCountFact.refresh_policy(),
            RefreshPolicy::AtStart
        ));
    }
}
