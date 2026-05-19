//! Загрузчик bundle: layout `bundle.toml + manifests/ + inventory/ + roles/ + _lib/`.
//!
//! Bundle — самодостаточная директория. `Bundle::load_dir` валидирует
//! `bundle.toml`, проверяет `entry` через path-safety helper и сохраняет
//! канонические пути. Manifests/roles/lib/inventory грузятся on-demand
//! Starlark-loader'ом, не предзагружаются.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::VersionReq;
use serde::Deserialize;

use crate::path_safety::{resolve_within_root, PathSafetyError};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BundleError {
    #[error("io error reading bundle at {path}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid bundle.toml: {0}")]
    InvalidManifest(String),
    #[error("bundle entry file not found: {0}")]
    EntryNotFound(String),
    #[error("invalid yaml in {path}: {source}")]
    InvalidYaml {
        path: String,
        #[source]
        source: serde_norway::Error,
    },
    #[error("bundle requires bosun {required} but current is {current}")]
    VersionIncompatible { required: String, current: String },
    #[error("module not found: load path '{load_path}' resolves to {fs_path:?}")]
    ModuleNotFound { load_path: String, fs_path: PathBuf },
    #[error("unsupported load path: '{load_path}' (expected @bosun/builtins, @roles/<name>, or @lib/<name>)")]
    UnsupportedLoadPath { load_path: String },
    #[error("template() called from unsupported module: {module:?} (only roles/<name>/main.star or _lib/<name>/main.star)")]
    UnsupportedModuleForTemplate { module: PathBuf },
    #[error("template() cannot be called from manifests/main.star: {hint}")]
    TemplateFromManifests { hint: String },
    #[error("private symbol '{symbol}' cannot be imported from {module:?}")]
    PrivateSymbol { symbol: String, module: PathBuf },
    #[error("path-safety violation: {0}")]
    PathSafety(#[from] PathSafetyError),
    #[error("inventory.merge: missing default merge strategy; set [bundle.inventory].default_merge_strategy in bundle.toml or pass strategy= argument")]
    DefaultMergeStrategyMissing,
}

/// Метаданные bundle из `bundle.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct BundleMetadata {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub requires_bosun: String,
    /// Относительный путь под bundle/ к entry-манифесту.
    pub entry: String,
    /// Опциональная конфигурация inventory (default_merge_strategy).
    #[serde(default)]
    pub inventory: BundleInventoryConfig,
    /// Документация активных тэгов для CLI `--help`. Не валидируется
    /// при evaluate — bundle author может использовать любые тэги, набор
    /// здесь служит документацией.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

/// `[bundle.inventory]`-секция.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BundleInventoryConfig {
    #[serde(default)]
    pub default_merge_strategy: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BundleManifest {
    bundle: BundleMetadata,
}

/// Загруженный bundle.
///
/// В отличие от MVP-версии, bundle не предзагружает manifests/defaults в
/// память. Все .star/.yaml файлы читаются on-demand через FileLoader и
/// `inventory.load`. Это даёт чёткий контракт «структура bundle — на
/// усмотрение автора» и убирает необходимость auto-scan.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub metadata: BundleMetadata,
    /// Канонический путь к корню bundle. Никаких symlink'ов в этом значении.
    pub root: PathBuf,
    /// Канонический absolute путь к entry-манифесту. Проверяется через
    /// `path_safety::resolve_within_root` в `load_dir`.
    pub entry: PathBuf,
}

impl Bundle {
    /// Прочитать bundle из директории. Минимум: `bundle.toml` с обязательными
    /// полями плюс файл, на который указывает `entry`.
    pub fn load_dir(path: &Path) -> Result<Self, BundleError> {
        let canonical_root = std::fs::canonicalize(path).map_err(|e| BundleError::Io {
            path: path.to_string_lossy().into_owned(),
            source: e,
        })?;

        let manifest = load_manifest(&canonical_root)?;

        let entry = match resolve_within_root(&canonical_root, &manifest.bundle.entry) {
            Ok(p) => p,
            Err(PathSafetyError::NotFound(missing)) => {
                return Err(BundleError::EntryNotFound(
                    missing.to_string_lossy().into_owned(),
                ));
            }
            Err(other) => return Err(BundleError::PathSafety(other)),
        };

        Ok(Self {
            metadata: manifest.bundle,
            root: canonical_root,
            entry,
        })
    }

    /// Проверка совместимости текущей версии bosun с требованием bundle.
    /// Cargo-семантика caret-синтаксиса: `^0.4` → `>=0.4.0, <0.5.0`.
    pub fn check_compatibility(&self, current_version: &str) -> Result<(), BundleError> {
        let req = VersionReq::parse(&self.metadata.requires_bosun).map_err(|e| {
            BundleError::InvalidManifest(format!(
                "requires_bosun '{}' is not a valid semver requirement: {}",
                self.metadata.requires_bosun, e
            ))
        })?;
        let current = semver::Version::parse(current_version).map_err(|e| {
            BundleError::InvalidManifest(format!(
                "current version '{current_version}' is not valid semver: {e}"
            ))
        })?;
        if !req.matches(&current) {
            return Err(BundleError::VersionIncompatible {
                required: self.metadata.requires_bosun.clone(),
                current: current_version.to_string(),
            });
        }
        Ok(())
    }

    /// Резолв `@roles/<name>` или `@lib/<name>` в канонический путь к
    /// `roles/<name>/main.star` или `_lib/<name>/main.star`. Любые другие
    /// префиксы — `UnsupportedLoadPath`.
    pub fn resolve_module(&self, load_path: &str) -> Result<PathBuf, BundleError> {
        let relative = if let Some(name) = load_path.strip_prefix("@roles/") {
            format!("roles/{name}/main.star")
        } else if let Some(name) = load_path.strip_prefix("@lib/") {
            format!("_lib/{name}/main.star")
        } else {
            return Err(BundleError::UnsupportedLoadPath {
                load_path: load_path.to_string(),
            });
        };

        match resolve_within_root(&self.root, &relative) {
            Ok(p) => Ok(p),
            Err(PathSafetyError::NotFound(missing)) => Err(BundleError::ModuleNotFound {
                load_path: load_path.to_string(),
                fs_path: missing,
            }),
            Err(other) => Err(BundleError::PathSafety(other)),
        }
    }

    /// Резолв template-пути относительно defining-модуля. `module` —
    /// канонический путь к .star файлу, определившему текущую функцию;
    /// `template_rel` — что было передано в `template("...")`.
    pub fn resolve_template(
        &self,
        module: &Path,
        template_rel: &str,
    ) -> Result<PathBuf, BundleError> {
        // template() из manifests/main.star — запрещён по spec.
        let manifests_main = self.root.join("manifests").join("main.star");
        if module == manifests_main {
            return Err(BundleError::TemplateFromManifests {
                hint: "move rendering into a role or @lib module".to_string(),
            });
        }

        // Cross-module access ловится здесь же: если template_rel содержит
        // `@`-префикс или `:`-разделитель, отказываем до резолва.
        if template_rel.starts_with('@') || template_rel.contains(':') {
            return Err(BundleError::PathSafety(PathSafetyError::ParentDir(
                template_rel.to_string(),
            )));
        }

        let templates_dir = templates_dir_for_module(&self.root, module).ok_or_else(|| {
            BundleError::UnsupportedModuleForTemplate {
                module: module.to_path_buf(),
            }
        })?;

        resolve_within_root(&templates_dir, template_rel).map_err(BundleError::PathSafety)
    }
}

/// Определить templates/ директорию для модуля. Возвращает None, если
/// модуль не лежит ни под `roles/<name>/main.star`, ни под `_lib/<name>/main.star`.
fn templates_dir_for_module(root: &Path, module: &Path) -> Option<PathBuf> {
    let rel = module.strip_prefix(root).ok()?;
    let components: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    // Ожидаем «roles/<name>/main.star» или «_lib/<name>/main.star».
    if components.len() == 3 && components[2] == "main.star" {
        match components[0] {
            "roles" => Some(root.join("roles").join(components[1]).join("templates")),
            "_lib" => Some(root.join("_lib").join(components[1]).join("templates")),
            _ => None,
        }
    } else {
        None
    }
}

/// Прочитать и распарсить `bundle.toml`.
fn load_manifest(root: &Path) -> Result<BundleManifest, BundleError> {
    let path = root.join("bundle.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| BundleError::Io {
        path: path.to_string_lossy().into_owned(),
        source: e,
    })?;
    toml::from_str::<BundleManifest>(&text)
        .map_err(|e| BundleError::InvalidManifest(format!("{path}: {e}", path = path.display())))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use super::*;

    fn make_bundle_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn load_dir_reads_metadata_and_validates_entry() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name           = "demo"
version        = "0.1.0"
description    = "demo bundle"
requires_bosun = "^0.1"
entry          = "manifests/main.star"

[bundle.inventory]
default_merge_strategy = "deep_map_replace_list"

[bundle.tags]
production = "Production"
staging    = "Staging"
"#,
        );
        write(
            &root.join("manifests/main.star"),
            "load(\"@bosun/builtins\", \"apt\")\n",
        );

        let bundle = Bundle::load_dir(&root).unwrap();
        assert_eq!(bundle.metadata.name, "demo");
        assert_eq!(bundle.metadata.entry, "manifests/main.star");
        assert_eq!(bundle.metadata.description.as_deref(), Some("demo bundle"));
        assert_eq!(
            bundle.metadata.inventory.default_merge_strategy.as_deref(),
            Some("deep_map_replace_list")
        );
        assert_eq!(bundle.metadata.tags.len(), 2);
        assert!(bundle.entry.ends_with("manifests/main.star"));
    }

    #[test]
    fn load_dir_supports_caret_version_req() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name = "demo"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "manifests/main.star"
"#,
        );
        write(&root.join("manifests/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        bundle.check_compatibility("0.1.0").unwrap();
        bundle.check_compatibility("0.1.5").unwrap();
    }

    #[test]
    fn check_compatibility_rejects_major_bump() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name = "demo"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "manifests/main.star"
"#,
        );
        write(&root.join("manifests/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let err = bundle.check_compatibility("0.2.0").unwrap_err();
        assert!(matches!(err, BundleError::VersionIncompatible { .. }));
    }

    #[test]
    fn missing_entry_file_is_error() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name = "demo"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "manifests/missing.star"
"#,
        );
        write(&root.join("manifests/other.star"), "");
        let err = Bundle::load_dir(&root).unwrap_err();
        assert!(matches!(err, BundleError::EntryNotFound(_)));
    }

    #[test]
    fn load_dir_rejects_absolute_entry() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name = "demo"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "/etc/passwd"
"#,
        );
        let err = Bundle::load_dir(&root).unwrap_err();
        assert!(matches!(err, BundleError::PathSafety(_)));
    }

    #[test]
    fn load_dir_rejects_parent_dir_in_entry() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name = "demo"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "../etc/passwd"
"#,
        );
        let err = Bundle::load_dir(&root).unwrap_err();
        assert!(matches!(err, BundleError::PathSafety(_)));
    }

    #[test]
    fn invalid_toml_is_error() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), "this is not toml = = =");
        let err = Bundle::load_dir(&root).unwrap_err();
        assert!(matches!(err, BundleError::InvalidManifest(_)));
    }

    #[test]
    fn missing_bundle_toml_is_io_error() {
        let (_keep, root) = make_bundle_dir();
        let err = Bundle::load_dir(&root).unwrap_err();
        assert!(matches!(err, BundleError::Io { .. }));
    }

    #[test]
    fn resolve_module_for_roles_returns_canonical_path() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        write(&root.join("roles/nginx/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let p = bundle.resolve_module("@roles/nginx").unwrap();
        assert!(p.ends_with("roles/nginx/main.star"));
    }

    #[test]
    fn resolve_module_for_lib_returns_canonical_path() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        write(&root.join("_lib/runr/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let p = bundle.resolve_module("@lib/runr").unwrap();
        assert!(p.ends_with("_lib/runr/main.star"));
    }

    #[test]
    fn resolve_module_unsupported_prefix_is_error() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let err = bundle.resolve_module("//foo/bar").unwrap_err();
        assert!(matches!(err, BundleError::UnsupportedLoadPath { .. }));
    }

    #[test]
    fn resolve_module_missing_role_is_module_not_found() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let err = bundle.resolve_module("@roles/missing").unwrap_err();
        assert!(matches!(err, BundleError::ModuleNotFound { .. }));
    }

    #[test]
    fn resolve_template_from_role_module_resolves_to_role_templates() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        write(&root.join("roles/nginx/main.star"), "");
        write(&root.join("roles/nginx/templates/nginx.conf.j2"), "ok");
        let bundle = Bundle::load_dir(&root).unwrap();
        let role_module = bundle.resolve_module("@roles/nginx").unwrap();
        let p = bundle
            .resolve_template(&role_module, "nginx.conf.j2")
            .unwrap();
        assert!(p.ends_with("roles/nginx/templates/nginx.conf.j2"));
    }

    #[test]
    fn resolve_template_from_lib_module_resolves_to_lib_templates() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        write(&root.join("_lib/runr/main.star"), "");
        write(&root.join("_lib/runr/templates/service.j2"), "ok");
        let bundle = Bundle::load_dir(&root).unwrap();
        let lib_module = bundle.resolve_module("@lib/runr").unwrap();
        let p = bundle.resolve_template(&lib_module, "service.j2").unwrap();
        assert!(p.ends_with("_lib/runr/templates/service.j2"));
    }

    #[test]
    fn resolve_template_from_manifests_main_is_rejected() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let err = bundle
            .resolve_template(&bundle.entry, "anything.j2")
            .unwrap_err();
        assert!(matches!(err, BundleError::TemplateFromManifests { .. }));
    }

    #[test]
    fn resolve_template_cross_module_path_is_rejected() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        write(&root.join("roles/nginx/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let role_module = bundle.resolve_module("@roles/nginx").unwrap();
        let err = bundle
            .resolve_template(&role_module, "@roles/other:foo.j2")
            .unwrap_err();
        // Cross-module access блокируется на уровне строки до резолва.
        assert!(matches!(err, BundleError::PathSafety(_)));
    }

    #[test]
    fn resolve_template_unsupported_module_is_error() {
        let (_keep, root) = make_bundle_dir();
        write(&root.join("bundle.toml"), default_bundle_toml());
        write(&root.join("manifests/main.star"), "");
        write(&root.join("scratch/foo.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        let weird = root.join("scratch/foo.star").canonicalize().unwrap();
        let err = bundle.resolve_template(&weird, "x.j2").unwrap_err();
        assert!(matches!(
            err,
            BundleError::UnsupportedModuleForTemplate { .. }
        ));
    }

    fn default_bundle_toml() -> &'static str {
        r#"
[bundle]
name = "demo"
version = "0.1.0"
requires_bosun = "^0.1"
entry = "manifests/main.star"
"#
    }
}
