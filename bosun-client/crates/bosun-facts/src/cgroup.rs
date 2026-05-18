//! Детектирование версии cgroup для cpu_count и memory_mb коллекторов.
//!
//! Алгоритм по spec:
//! - v2, если существует `<root_fs>/sys/fs/cgroup/cgroup.controllers`.
//! - иначе v1, если `<root_fs>/proc/self/cgroup` содержит строку формата
//!   `N:<controller>:<path>` (вне zero-id строки v2).
//! - иначе Unknown.

use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CgroupVersion {
    V1,
    V2,
    Unknown,
}

/// Определяет версию cgroup по содержимому root-FS.
///
/// Не делает I/O ошибок «громко» — отсутствие любых файлов означает Unknown,
/// что для коллекторов cpu/memory переключает на num_cpus / MemTotal fallback.
pub fn detect_version(root_fs: &Path) -> CgroupVersion {
    let v2_marker = root_fs.join("sys/fs/cgroup/cgroup.controllers");
    if v2_marker.is_file() {
        return CgroupVersion::V2;
    }

    let v1_marker = root_fs.join("proc/self/cgroup");
    let Ok(content) = fs::read_to_string(&v1_marker) else {
        return CgroupVersion::Unknown;
    };
    if has_v1_subsys_line(&content) {
        CgroupVersion::V1
    } else {
        CgroupVersion::Unknown
    }
}

/// Распознать v1-формат `/proc/self/cgroup`:
/// строка `N:<controllers>:<path>`, где `N` — целое число > 0, `controllers`
/// непустой. Под v2 контроллеры в этом файле пишутся одной строкой
/// `0::/<path>` — её намеренно отбрасываем.
fn has_v1_subsys_line(content: &str) -> bool {
    content.lines().any(|line| {
        let mut parts = line.splitn(3, ':');
        let id = parts.next().unwrap_or("");
        let subsys = parts.next().unwrap_or("");
        let _path = parts.next();
        if id.is_empty() || subsys.is_empty() {
            return false;
        }
        // v2-only строка — `0::...`, ID == "0" и subsys пустой; не наш случай.
        if id == "0" && subsys.is_empty() {
            return false;
        }
        id.chars().all(|c| c.is_ascii_digit()) && !subsys.is_empty()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn v2_detected_via_marker_file() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "sys/fs/cgroup/cgroup.controllers",
            "cpu cpuset memory\n",
        );
        assert_eq!(detect_version(tmp.path()), CgroupVersion::V2);
    }

    #[test]
    fn v1_detected_via_proc_self_cgroup() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "proc/self/cgroup",
            "12:freezer:/\n11:cpu,cpuacct:/user.slice\n10:memory:/user.slice\n",
        );
        assert_eq!(detect_version(tmp.path()), CgroupVersion::V1);
    }

    #[test]
    fn v2_marker_takes_priority_over_v1_format() {
        // В hybrid-режимах оба файла присутствуют; v2-маркер выигрывает.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "sys/fs/cgroup/cgroup.controllers", "cpu\n");
        write_file(tmp.path(), "proc/self/cgroup", "11:cpu:/\n");
        assert_eq!(detect_version(tmp.path()), CgroupVersion::V2);
    }

    #[test]
    fn unknown_when_no_files() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(detect_version(tmp.path()), CgroupVersion::Unknown);
    }

    #[test]
    fn v2_only_zero_line_yields_unknown_without_marker() {
        // Без cgroup.controllers строка `0::/path` одна — это
        // hybrid v2-only режим без классических v1 контроллеров.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/self/cgroup", "0::/user.slice\n");
        assert_eq!(detect_version(tmp.path()), CgroupVersion::Unknown);
    }

    #[test]
    fn garbage_lines_yield_unknown() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "proc/self/cgroup", "not a valid line\n");
        assert_eq!(detect_version(tmp.path()), CgroupVersion::Unknown);
    }
}
