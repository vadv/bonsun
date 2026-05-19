//! Фабрика default-набора коллекторов для MVP.
//!
//! Используется из bosun-cli при старте: создаёт `FactsCollector` со всеми
//! MVP-фактами плюс опциональным набором PG-discovery-фактов. На проде
//! root_fs = "/", в тестах подменяется на tempdir.
//!
//! PG-факты регистрируются всегда, даже на не-PG-нодах — это нужно для
//! Strict-режима в Starlark: набор имён фактов должен быть стабильным
//! независимо от того, какая роль применяется. На нодах без PG факты
//! отдают `Unknown { reason: "no PostgreSQL detected ..." }`.

use std::path::PathBuf;

use crate::collector::{Fact, FactsCollector};
use crate::cpu_count::CpuCountFact;
use crate::hostname::HostnameFact;
use crate::init_system::InitSystemFact;
use crate::installed_packages::InstalledPackagesFact;
use crate::is_pod::IsPodFact;
use crate::memory_mb::MemoryMbFact;
use crate::pg::build_pg_facts;

/// Возвращает коллектор со всеми MVP-фактами плюс PG-discovery-фактами.
pub fn with_default_collectors(root_fs: PathBuf) -> FactsCollector {
    let mut facts: Vec<Box<dyn Fact>> = vec![
        Box::new(HostnameFact),
        Box::new(CpuCountFact),
        Box::new(MemoryMbFact),
        Box::new(InitSystemFact),
        Box::new(IsPodFact),
        Box::new(InstalledPackagesFact),
    ];
    facts.extend(build_pg_facts(&root_fs, None));
    FactsCollector::new(root_fs, facts)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use bosun_core::{FactsSource, ResourceKind};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn default_collectors_includes_mvp_and_pg_facts() {
        let tmp = TempDir::new().unwrap();
        let c = with_default_collectors(tmp.path().to_path_buf());
        c.collect_at_start();
        let snap = c.snapshot();
        let names: Vec<&str> = snap.names().collect();
        // MVP-факты — все шесть попадают в snapshot после collect_at_start:
        // installed_packages с политикой AtStartAndAfterApply тоже собирается
        // на старте (F02-фикс: иначе apt.package видит Unknown и фолбэчит в Add).
        assert!(names.contains(&"hostname"));
        assert!(names.contains(&"cpu_count"));
        assert!(names.contains(&"memory_mb"));
        assert!(names.contains(&"init_system"));
        assert!(names.contains(&"is_pod"));
        assert!(names.contains(&"installed_packages"));
        // PG-discovery-факты регистрируются всегда; на не-PG-нодах три из
        // четырёх отдают Unknown с reason, но имена в snapshot должны быть.
        assert!(names.contains(&"pg_initialized"));
        assert!(names.contains(&"pg_is_master"));
        assert!(names.contains(&"pg_users_with_passwords"));
        assert!(names.contains(&"pg_extensions"));
        assert_eq!(names.len(), 10);
    }

    #[test]
    fn installed_packages_known_at_start_when_dpkg_present() {
        // Подготовка fake-FS root с dpkg/status: installed_packages должен
        // быть Known сразу после collect_at_start, без необходимости
        // звать mark_dirty_after_apply.
        let tmp = TempDir::new().unwrap();
        let dpkg = tmp.path().join("var/lib/dpkg");
        std::fs::create_dir_all(&dpkg).unwrap();
        std::fs::write(
            dpkg.join("status"),
            "Package: nginx\nVersion: 1.18.0\nStatus: install ok installed\n",
        )
        .unwrap();

        let c = with_default_collectors(tmp.path().to_path_buf());
        c.collect_at_start();
        let view = c.view();
        let v = view.get("installed_packages");
        assert!(
            v.is_known(),
            "installed_packages должно быть Known после collect_at_start: {v:?}"
        );
        let pkgs = v.value().unwrap();
        assert!(pkgs.get("nginx").is_some());
    }

    #[test]
    fn installed_packages_refreshes_after_apt_apply() {
        // После каждого apply apt.package факт installed_packages
        // помечается dirty и пересобирается лениво.
        let tmp = TempDir::new().unwrap();
        let dpkg = tmp.path().join("var/lib/dpkg");
        std::fs::create_dir_all(&dpkg).unwrap();
        std::fs::write(
            dpkg.join("status"),
            "Package: a\nVersion: 1.0\nStatus: install ok installed\n",
        )
        .unwrap();

        let c = with_default_collectors(tmp.path().to_path_buf());
        c.collect_at_start();

        // «Установили» новый пакет — обновляем dpkg/status.
        std::fs::write(
            dpkg.join("status"),
            "Package: a\nVersion: 1.0\nStatus: install ok installed\n\nPackage: b\nVersion: 2.0\nStatus: install ok installed\n",
        )
        .unwrap();
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));

        let view = c.view();
        let v = view.get("installed_packages");
        assert!(v.is_known());
        assert!(v.value().unwrap().get("b").is_some());
    }
}
