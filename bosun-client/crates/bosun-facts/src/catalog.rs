//! Фабрика default-набора коллекторов для MVP.
//!
//! Используется из bosun-cli при старте: создаёт `FactsCollector` со всеми
//! шестью MVP-фактами и заданным root-путём. На проде root_fs = "/",
//! в тестах подменяется на tempdir.

use std::path::PathBuf;

use crate::collector::{Fact, FactsCollector};
use crate::cpu_count::CpuCountFact;
use crate::hostname::HostnameFact;
use crate::init_system::InitSystemFact;
use crate::installed_packages::InstalledPackagesFact;
use crate::is_pod::IsPodFact;
use crate::memory_mb::MemoryMbFact;

/// Возвращает коллектор со всеми MVP-фактами.
pub fn with_default_collectors(root_fs: PathBuf) -> FactsCollector {
    let facts: Vec<Box<dyn Fact>> = vec![
        Box::new(HostnameFact),
        Box::new(CpuCountFact),
        Box::new(MemoryMbFact),
        Box::new(InitSystemFact),
        Box::new(IsPodFact),
        Box::new(InstalledPackagesFact),
    ];
    FactsCollector::new(root_fs, facts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use bosun_core::{FactsSource, ResourceKind};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn default_collectors_includes_all_six_facts() {
        let tmp = TempDir::new().unwrap();
        let c = with_default_collectors(tmp.path().to_path_buf());
        c.collect_at_start();
        let snap = c.snapshot();
        let names: Vec<&str> = snap.names().collect();
        // Snapshot содержит только AtStart-факты (installed_packages — AfterApply).
        assert!(names.contains(&"hostname"));
        assert!(names.contains(&"cpu_count"));
        assert!(names.contains(&"memory_mb"));
        assert!(names.contains(&"init_system"));
        assert!(names.contains(&"is_pod"));
        assert!(!names.contains(&"installed_packages"));
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn installed_packages_collected_lazily_after_apply() {
        let tmp = TempDir::new().unwrap();
        let c = with_default_collectors(tmp.path().to_path_buf());
        c.collect_at_start();
        // До mark_dirty факт installed_packages не виден через view.
        let view = c.view();
        let v = view.get("installed_packages");
        match v {
            bosun_core::FactValue::Unknown { reason } => {
                assert!(reason.contains("unknown fact"), "got: {reason}");
            }
            other => panic!("expected Unknown before mark_dirty, got {other:?}"),
        }
        // После mark_dirty by apt.package — пересборка.
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let v2 = view.get("installed_packages");
        // Без dpkg/status получится Unknown — но это уже из коллектора.
        match v2 {
            bosun_core::FactValue::Unknown { reason } => {
                assert!(reason.contains("status"), "got: {reason}");
            }
            other => panic!("expected Unknown (dpkg missing), got {other:?}"),
        }
    }
}
