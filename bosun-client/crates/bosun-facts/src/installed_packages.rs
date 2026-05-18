//! Коллектор `installed_packages` — парсер dpkg-status и apt/lists.
//!
//! Алгоритм:
//! - Читаем `<root_fs>/var/lib/dpkg/status` — секции, разделённые пустыми
//!   строками. Берём `Package`, `Version`, `Status`. Пакеты без `Status`,
//!   содержащего `installed`, пропускаем.
//! - Читаем все обычные файлы в `<root_fs>/var/lib/apt/lists/` — без фильтра
//!   по расширению (фикс бага self-upgrade: в стандартной Debian/Ubuntu имена
//!   идут без `.Packages`). Каждый файл парсим в том же формате; собираем
//!   `Package`, `Version`, `Priority`. Для каждого имени держим максимум
//!   одной кандидатной версии — выбор по правилу:
//!     1. Если `Priority` отличается — побеждает строка с большим Priority
//!        (выше apt-pinning).
//!     2. Иначе побеждает более новая `Version` по debversion::cmp.
//!
//! Возврат: `Known(json-объект)` с `{ "<pkg>": { "current_version": "...",
//! "candidate_version": "..." | null } }`. Если `dpkg/status` нечитаем →
//! `Unknown`. Если только `lists/` пуст/недоступен — candidate_version
//! везде null, current_version из dpkg остаётся.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use bosun_core::{FactCategory, FactValue, RefreshPolicy, ResourceKind};
use debversion::Version as DebVersion;

use crate::collector::{Fact, FactCollectCtx};

pub struct InstalledPackagesFact;

impl Fact for InstalledPackagesFact {
    fn name(&self) -> &str {
        "installed_packages"
    }
    fn category(&self) -> FactCategory {
        FactCategory::Slow
    }
    fn refresh_policy(&self) -> RefreshPolicy {
        RefreshPolicy::AfterApply {
            triggers: vec![ResourceKind::from_static("apt.package")],
        }
    }
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue {
        let status_path = ctx.root_fs.join("var/lib/dpkg/status");
        let dpkg_text = match fs::read_to_string(&status_path) {
            Ok(s) => s,
            Err(e) => {
                return FactValue::Unknown {
                    reason: format!("read {}: {e}", status_path.display()),
                };
            }
        };

        let installed = parse_installed_from_status(&dpkg_text);
        let candidates = collect_candidates(&ctx.root_fs.join("var/lib/apt/lists"));

        let mut result: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for (name, current) in &installed {
            let candidate = candidates.get(name).map(|c| c.version.clone());
            let entry = serde_json::json!({
                "current_version": current,
                "candidate_version": candidate,
            });
            result.insert(name.clone(), entry);
        }
        // Кандидатные версии для пакетов, которых нет в dpkg — игнорируем:
        // факт описывает «что установлено», информация о неустановленных
        // пакетах добавила бы шум и не используется apt.package примитивом.

        FactValue::Known(serde_json::to_value(result).unwrap_or(serde_json::Value::Null))
    }
}

/// Минимальная запись о пакете: имя + опционально версия.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPackage {
    pub name: String,
    pub version: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i32>,
}

/// Парсит один Debian control file (`dpkg/status` или apt-`Packages`).
///
/// Возвращает Vec, потому что в одном файле могут быть multiarch-секции
/// с одинаковым именем; decision о dedup делается в caller'е.
pub(crate) fn parse_control_file(text: &str) -> Vec<ParsedPackage> {
    let mut result = Vec::new();
    for section in split_sections(text) {
        let fields = parse_fields(section);
        let Some(name) = fields.get("Package").cloned() else {
            continue;
        };
        let pkg = ParsedPackage {
            name,
            version: fields.get("Version").cloned(),
            status: fields.get("Status").cloned(),
            priority: fields
                .get("Priority")
                .and_then(|s| priority_value(s.as_str())),
        };
        result.push(pkg);
    }
    result
}

/// Разрезает control-text на секции по пустой строке.
fn split_sections(text: &str) -> Vec<&str> {
    text.split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// Парсит одну секцию `Key: Value` с поддержкой continuation-строк
/// (начинаются с whitespace). Возвращает только нужные нам ключи —
/// безымянная BTreeMap гарантирует детерминированный порядок при отладке.
fn parse_fields(section: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut current_key: Option<String> = None;
    let mut current_value = String::new();

    for line in section.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            // Continuation — присоединяем к последнему значению. В Debian
            // control-формате continuation начинается с одного пробела,
            // следующего за reapeated-line. Для наших целей нам continuation
            // важен только для Description (которое мы не парсим), но
            // оставим обработку — иначе можем потерять секцию из-за того,
            // что continuation бьёт по парсингу Status.
            if current_key.is_some() {
                current_value.push('\n');
                current_value.push_str(line.trim_start());
            }
            continue;
        }
        // Закрыть предыдущее поле.
        if let Some(key) = current_key.take() {
            fields.insert(key, std::mem::take(&mut current_value));
        }
        // Найти `Key:`.
        if let Some((key, rest)) = line.split_once(':') {
            current_key = Some(key.trim().to_string());
            current_value = rest.trim().to_string();
        }
        // Строки без `:` — мусор, игнорируем.
    }
    // Закрыть последний.
    if let Some(key) = current_key {
        fields.insert(key, current_value);
    }
    fields
}

/// Конвертирует Priority-токен в числовое значение. APT принимает
/// нечисловые имена (`important`, `required`, `standard`, `optional`,
/// `extra`), но в `Packages` обычно лежит число. Для нечисловых имён
/// возвращаем None — это означает «использовать debversion-сравнение».
fn priority_value(s: &str) -> Option<i32> {
    s.trim().parse().ok()
}

/// Выбирает установленные пакеты из текста status-файла.
fn parse_installed_from_status(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for pkg in parse_control_file(text) {
        // Status должен содержать `installed`. Иначе пропускаем
        // (`deinstall ok config-files`, `purge ok not-installed`).
        let status_ok = pkg
            .status
            .as_deref()
            .map(|s| s.split_whitespace().any(|t| t == "installed"))
            .unwrap_or(false);
        if !status_ok {
            continue;
        }
        let Some(version) = pkg.version else {
            // Странный пакет без версии — пропускаем.
            continue;
        };
        // Multiarch-пакеты могут появиться несколько раз; для current_version
        // выбираем максимальную версию (по debversion).
        match out.get(&pkg.name) {
            Some(existing) => {
                if is_newer(&version, existing) {
                    out.insert(pkg.name, version);
                }
            }
            None => {
                out.insert(pkg.name, version);
            }
        }
    }
    out
}

/// Кандидатная версия из apt/lists — версия + приоритет (для tie-break).
#[derive(Debug, Clone)]
struct Candidate {
    version: String,
    priority: Option<i32>,
}

/// Перебирает все обычные файлы в каталоге lists/ и собирает кандидатные
/// версии. Невалидные файлы (мусор, бинари) silently пропускаются.
fn collect_candidates(lists_dir: &Path) -> BTreeMap<String, Candidate> {
    let mut out: BTreeMap<String, Candidate> = BTreeMap::new();
    let Ok(entries) = fs::read_dir(lists_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        // Эвристика «похоже на Packages-файл»: первая непустая строка
        // должна начинаться с `Package:`. Иначе пропускаем.
        if !looks_like_packages_file(&text) {
            continue;
        }
        for pkg in parse_control_file(&text) {
            let Some(version) = pkg.version else { continue };
            let candidate = Candidate {
                version,
                priority: pkg.priority,
            };
            merge_candidate(&mut out, pkg.name, candidate);
        }
    }
    out
}

fn looks_like_packages_file(text: &str) -> bool {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.starts_with("Package:"))
        .unwrap_or(false)
}

/// Сливает кандидат в map: выигрывает большая `Priority`, при равенстве —
/// более новая `Version` по debversion.
fn merge_candidate(map: &mut BTreeMap<String, Candidate>, name: String, candidate: Candidate) {
    match map.get(&name) {
        None => {
            map.insert(name, candidate);
        }
        Some(existing) => {
            let prefer_new = match (candidate.priority, existing.priority) {
                (Some(new_p), Some(old_p)) if new_p != old_p => new_p > old_p,
                _ => is_newer(&candidate.version, &existing.version),
            };
            if prefer_new {
                map.insert(name, candidate);
            }
        }
    }
}

/// Сравнивает две версии по debversion. Если хоть одна не парсится —
/// fall back на строковое сравнение, чтобы один битый кандидат не сломал
/// весь факт.
fn is_newer(new: &str, old: &str) -> bool {
    match (DebVersion::from_str(new), DebVersion::from_str(old)) {
        (Ok(n), Ok(o)) => n > o,
        _ => {
            tracing::warn!(
                new = new,
                old = old,
                "version compare failed, using string fallback"
            );
            new > old
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::fs;
    use std::str::FromStr;

    use tempfile::TempDir;

    use super::*;

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn collect_value(root: &Path) -> serde_json::Value {
        let ctx = FactCollectCtx::new(root.to_path_buf());
        let v = InstalledPackagesFact.collect(&ctx);
        match v {
            FactValue::Known(j) => j,
            other => panic!("expected Known, got {other:?}"),
        }
    }

    // ------------- Парсер control-файлов -------------

    #[test]
    fn parse_fields_extracts_package_and_version() {
        let text = "Package: nginx\nVersion: 1.20.1-6\nStatus: install ok installed\n";
        let pkgs = parse_control_file(text);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "nginx");
        assert_eq!(pkgs[0].version.as_deref(), Some("1.20.1-6"));
        assert_eq!(pkgs[0].status.as_deref(), Some("install ok installed"));
    }

    #[test]
    fn parse_fields_handles_continuation_lines() {
        // Description часто многострочный; парсер не должен сбиваться.
        let text = "Package: nginx\nVersion: 1.0\nDescription: web server\n .\n Extra line\nStatus: install ok installed\n";
        let pkgs = parse_control_file(text);
        assert_eq!(pkgs[0].status.as_deref(), Some("install ok installed"));
    }

    #[test]
    fn parse_multiple_sections() {
        let text = "Package: a\nVersion: 1.0\nStatus: install ok installed\n\nPackage: b\nVersion: 2.0\nStatus: install ok installed\n";
        let pkgs = parse_control_file(text);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "a");
        assert_eq!(pkgs[1].name, "b");
    }

    #[test]
    fn empty_text_yields_empty_vec() {
        assert!(parse_control_file("").is_empty());
        assert!(parse_control_file("\n\n").is_empty());
    }

    #[test]
    fn section_without_package_field_skipped() {
        let text = "Version: 1.0\nStatus: install ok installed\n";
        let pkgs = parse_control_file(text);
        assert!(pkgs.is_empty());
    }

    // ------------- Status фильтр -------------

    #[test]
    fn status_deinstall_skipped() {
        let text = "Package: orphan\nVersion: 1.0\nStatus: deinstall ok config-files\n";
        let map = parse_installed_from_status(text);
        assert!(map.is_empty());
    }

    #[test]
    fn status_not_installed_skipped() {
        let text = "Package: nope\nVersion: 1.0\nStatus: purge ok not-installed\n";
        let map = parse_installed_from_status(text);
        assert!(map.is_empty());
    }

    #[test]
    fn status_install_ok_installed_kept() {
        let text = "Package: nginx\nVersion: 1.0\nStatus: install ok installed\n";
        let map = parse_installed_from_status(text);
        assert_eq!(map.get("nginx").map(String::as_str), Some("1.0"));
    }

    #[test]
    fn package_without_version_skipped() {
        // Без Version пакет нечитаемый — пропускаем без шума.
        let text = "Package: brokenpkg\nStatus: install ok installed\n";
        let map = parse_installed_from_status(text);
        assert!(map.is_empty());
    }

    // ------------- Multiarch -------------

    #[test]
    fn multiarch_keeps_latest_version() {
        // Один и тот же пакет дважды (multiarch); выбираем более новый.
        let text = "Package: libc6\nVersion: 2.31-13\nStatus: install ok installed\n\nPackage: libc6\nVersion: 2.31-13+deb11u1\nStatus: install ok installed\n";
        let map = parse_installed_from_status(text);
        assert_eq!(
            map.get("libc6").map(String::as_str),
            Some("2.31-13+deb11u1")
        );
    }

    // ------------- debversion comparison cases (по spec) -------------

    #[test]
    fn debversion_10_gt_9() {
        let a = DebVersion::from_str("1.10.0").unwrap();
        let b = DebVersion::from_str("1.9.0").unwrap();
        assert!(a > b);
    }

    #[test]
    fn debversion_epoch_beats_higher_upstream() {
        let a = DebVersion::from_str("1:1.0").unwrap();
        let b = DebVersion::from_str("2.0").unwrap();
        assert!(a > b);
    }

    #[test]
    fn debversion_tilde_sorts_before_nothing() {
        let a = DebVersion::from_str("1.0~rc1").unwrap();
        let b = DebVersion::from_str("1.0").unwrap();
        assert!(a < b);
    }

    #[test]
    fn debversion_plus_nmu_sorts_after_base() {
        let a = DebVersion::from_str("1.0+nmu1").unwrap();
        let b = DebVersion::from_str("1.0").unwrap();
        assert!(a > b);
    }

    #[test]
    fn debversion_ubuntu_dot_dot_dot_revisions() {
        let a = DebVersion::from_str("1.0-1ubuntu1.18.04.1").unwrap();
        let b = DebVersion::from_str("1.0-1ubuntu1").unwrap();
        assert!(a > b);
    }

    #[test]
    fn debversion_equal_versions_compare_equal() {
        let a = DebVersion::from_str("1.0-1").unwrap();
        let b = DebVersion::from_str("1.0-1").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn debversion_helper_is_newer_works() {
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(!is_newer("1.9.0", "1.10.0"));
        assert!(!is_newer("1.0-1", "1.0-1"));
    }

    // ------------- Полный путь fact collection -------------

    #[test]
    fn collect_returns_unknown_when_status_missing() {
        let tmp = TempDir::new().unwrap();
        let ctx = FactCollectCtx::new(tmp.path().to_path_buf());
        let v = InstalledPackagesFact.collect(&ctx);
        match v {
            FactValue::Unknown { reason } => assert!(reason.contains("status")),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn collect_empty_status_returns_empty_object() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "var/lib/dpkg/status", "");
        let v = collect_value(tmp.path());
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn collect_with_status_and_no_lists() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "var/lib/dpkg/status",
            "Package: nginx\nVersion: 1.0\nStatus: install ok installed\n",
        );
        let v = collect_value(tmp.path());
        assert_eq!(v["nginx"]["current_version"], serde_json::json!("1.0"));
        assert_eq!(v["nginx"]["candidate_version"], serde_json::json!(null));
    }

    #[test]
    fn collect_with_apt_lists_provides_candidate() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "var/lib/dpkg/status",
            "Package: nginx\nVersion: 1.0\nStatus: install ok installed\n",
        );
        // Файл без `.Packages` суффикса — намеренно (фикс self-upgrade бага).
        write_file(
            tmp.path(),
            "var/lib/apt/lists/repo.example.com_dists_bookworm_main_binary-amd64_Packages",
            "Package: nginx\nVersion: 1.2\n",
        );
        let v = collect_value(tmp.path());
        assert_eq!(v["nginx"]["current_version"], serde_json::json!("1.0"));
        assert_eq!(v["nginx"]["candidate_version"], serde_json::json!("1.2"));
    }

    #[test]
    fn collect_lists_priority_beats_higher_version() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "var/lib/dpkg/status",
            "Package: foo\nVersion: 1.0\nStatus: install ok installed\n",
        );
        // Низкий priority + высокая версия.
        write_file(
            tmp.path(),
            "var/lib/apt/lists/repoA_Packages",
            "Package: foo\nVersion: 2.0\nPriority: 100\n",
        );
        // Высокий priority + низкая версия — должен выиграть.
        write_file(
            tmp.path(),
            "var/lib/apt/lists/repoB_Packages",
            "Package: foo\nVersion: 1.5\nPriority: 500\n",
        );
        let v = collect_value(tmp.path());
        assert_eq!(v["foo"]["candidate_version"], serde_json::json!("1.5"));
    }

    #[test]
    fn collect_lists_higher_version_wins_on_equal_priority() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "var/lib/dpkg/status",
            "Package: foo\nVersion: 1.0\nStatus: install ok installed\n",
        );
        write_file(
            tmp.path(),
            "var/lib/apt/lists/repoA_Packages",
            "Package: foo\nVersion: 1.2\nPriority: 500\n",
        );
        write_file(
            tmp.path(),
            "var/lib/apt/lists/repoB_Packages",
            "Package: foo\nVersion: 1.5\nPriority: 500\n",
        );
        let v = collect_value(tmp.path());
        assert_eq!(v["foo"]["candidate_version"], serde_json::json!("1.5"));
    }

    #[test]
    fn collect_garbage_file_in_lists_silently_skipped() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "var/lib/dpkg/status",
            "Package: foo\nVersion: 1.0\nStatus: install ok installed\n",
        );
        write_file(
            tmp.path(),
            "var/lib/apt/lists/garbage.bin",
            "binary noise: not a packages file",
        );
        write_file(tmp.path(), "var/lib/apt/lists/Release", "Origin: Debian\n");
        let v = collect_value(tmp.path());
        // foo есть, но candidate_version=null, так как нет валидного Packages.
        assert_eq!(v["foo"]["current_version"], serde_json::json!("1.0"));
        assert_eq!(v["foo"]["candidate_version"], serde_json::json!(null));
    }

    #[test]
    fn collect_candidate_only_in_lists_not_in_dpkg_is_ignored() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "var/lib/dpkg/status",
            "Package: foo\nVersion: 1.0\nStatus: install ok installed\n",
        );
        write_file(
            tmp.path(),
            "var/lib/apt/lists/r_Packages",
            "Package: bar\nVersion: 2.0\n",
        );
        let v = collect_value(tmp.path());
        // bar не должен попадать в результат — он не установлен.
        assert!(v.get("bar").is_none());
        assert_eq!(v["foo"]["candidate_version"], serde_json::json!(null));
    }

    #[test]
    fn name_and_policy() {
        assert_eq!(InstalledPackagesFact.name(), "installed_packages");
        match InstalledPackagesFact.refresh_policy() {
            RefreshPolicy::AfterApply { triggers } => {
                assert_eq!(triggers.len(), 1);
                assert_eq!(triggers[0].as_str(), "apt.package");
            }
            other => panic!("expected AfterApply, got {other:?}"),
        }
        assert_eq!(InstalledPackagesFact.category(), FactCategory::Slow);
    }
}
