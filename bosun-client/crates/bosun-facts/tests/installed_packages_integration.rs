//! Integration-тест installed_packages: полный путь сбора через
//! FactsCollector, синтетические dpkg/status и lists/.
//!
//! Тестирует:
//! - mark_dirty_after_apply(apt.package) запускает collect лениво в view.get
//! - Файлы в lists/ без `.Packages` суффикса парсятся
//! - Кандидаты с разными priority корректно мержатся
//! - Status фильтр пропускает purge ok not-installed и deinstall ok config-files
//! - Snapshot не зависит от последующего mark_dirty.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::fs;
use std::path::Path;

use bosun_core::{FactValue, FactsSource, ResourceKind};
use bosun_facts::with_default_collectors;
use tempfile::TempDir;

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Минимально реалистичный набор файлов на синтетическом root.
fn populate_root(root: &Path) {
    // Минимальный hostname.
    write_file(root, "proc/sys/kernel/hostname", "synthetic-host\n");
    // /proc/1/comm для init_system.
    write_file(root, "proc/1/comm", "systemd\n");
    // meminfo для memory_mb fallback.
    write_file(root, "proc/meminfo", "MemTotal:        2097152 kB\n");
    // dpkg/status: один пакет установлен, второй — config-files (skip).
    write_file(
        root,
        "var/lib/dpkg/status",
        concat!(
            "Package: nginx\n",
            "Version: 1.20.1-6\n",
            "Status: install ok installed\n",
            "Architecture: amd64\n",
            "Description: small web server\n",
            "\n",
            "Package: orphan\n",
            "Version: 0.1\n",
            "Status: deinstall ok config-files\n",
            "Architecture: amd64\n",
            "\n",
            "Package: curl\n",
            "Version: 7.74.0-1\n",
            "Status: install ok installed\n",
            "Architecture: amd64\n",
        ),
    );
    // apt/lists: два «репозитория», каждый без .Packages суффикса (фикс бага).
    write_file(
        root,
        "var/lib/apt/lists/deb.example.com_dists_bookworm_main_binary-amd64_Packages",
        concat!(
            "Package: nginx\n",
            "Version: 1.20.1-7\n",
            "Priority: 500\n",
            "\n",
            "Package: curl\n",
            "Version: 7.74.0-2\n",
            "Priority: 500\n",
            "\n",
            "Package: vim\n",
            "Version: 8.2\n",
        ),
    );
    // Второй файл без расширения вообще.
    write_file(
        root,
        "var/lib/apt/lists/security.example.com_dists_bookworm-security_main_binary-amd64_Packages",
        concat!("Package: nginx\n", "Version: 1.20.1-9\n", "Priority: 990\n",),
    );
    // Release-файл с другим содержимым — должен быть пропущен эвристикой.
    write_file(
        root,
        "var/lib/apt/lists/deb.example.com_dists_bookworm_Release",
        "Origin: Debian\nLabel: Debian\nSuite: bookworm\n",
    );
}

#[test]
fn collects_installed_packages_through_collector() {
    let tmp = TempDir::new().unwrap();
    populate_root(tmp.path());

    let c = with_default_collectors(tmp.path().to_path_buf());
    c.collect_at_start();

    // installed_packages — AfterApply, snapshot его не содержит.
    let snap = c.snapshot();
    assert!(matches!(
        snap.get("installed_packages"),
        FactValue::Unknown { .. }
    ));

    // Перед mark_dirty view.get для AfterApply-факта — Unknown ("unknown fact"),
    // потому что записи в кэше ещё нет.
    let view = c.view();
    assert!(matches!(
        view.get("installed_packages"),
        FactValue::Unknown { .. }
    ));

    // После mark_dirty(apt.package) — пересборка.
    c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
    let v = view.get("installed_packages");
    let map = match v {
        FactValue::Known(j) => j,
        other => panic!("expected Known, got {other:?}"),
    };

    // nginx: current=1.20.1-6, candidate из security должен победить
    // (priority 990 > 500).
    assert_eq!(
        map["nginx"]["current_version"],
        serde_json::json!("1.20.1-6")
    );
    assert_eq!(
        map["nginx"]["candidate_version"],
        serde_json::json!("1.20.1-9")
    );

    // curl: current=7.74.0-1, candidate=7.74.0-2 (single source).
    assert_eq!(
        map["curl"]["current_version"],
        serde_json::json!("7.74.0-1")
    );
    assert_eq!(
        map["curl"]["candidate_version"],
        serde_json::json!("7.74.0-2")
    );

    // orphan не должен попасть (purge ok config-files эквивалент:
    // не содержит "installed" — Status: deinstall ok config-files).
    assert!(map.get("orphan").is_none());

    // vim есть в lists, но не установлен — не должен попасть в результат.
    assert!(map.get("vim").is_none());
}

#[test]
fn snapshot_isolated_from_subsequent_marks() {
    let tmp = TempDir::new().unwrap();
    populate_root(tmp.path());
    let c = with_default_collectors(tmp.path().to_path_buf());
    c.collect_at_start();
    let snap = c.snapshot();
    // hostname виден в snapshot.
    assert_eq!(
        snap.get("hostname").value().unwrap(),
        &serde_json::json!("synthetic-host")
    );
    // Пометка dirty AfterApply-фактов не задевает snapshot.
    c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
    // Snapshot не содержит installed_packages — он AfterApply, не успел собраться.
    assert!(matches!(
        snap.get("installed_packages"),
        FactValue::Unknown { .. }
    ));
}

#[test]
fn second_get_uses_cache_without_recollect() {
    let tmp = TempDir::new().unwrap();
    populate_root(tmp.path());
    let c = with_default_collectors(tmp.path().to_path_buf());
    c.collect_at_start();
    c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
    let view = c.view();
    let v1 = view.get("installed_packages");
    // Удаляем status-файл — если бы был второй collect, мы бы получили Unknown.
    fs::remove_file(tmp.path().join("var/lib/dpkg/status")).unwrap();
    let v2 = view.get("installed_packages");
    // Кэш — оба значения совпадают.
    match (v1, v2) {
        (FactValue::Known(a), FactValue::Known(b)) => assert_eq!(a, b),
        other => panic!("expected both Known, got {other:?}"),
    }
}
