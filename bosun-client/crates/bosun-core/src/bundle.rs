//! Загрузчик bundle: layout `bundle.toml + manifests/ + defaults/ + templates/`.
//!
//! Bundle — самодостаточная единица деплоя. Структура читается один раз
//! на старте, дальше evaluator получает её по ссылке.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use semver::VersionReq;
use serde::Deserialize;

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
}

/// Метаданные bundle, читаемые из `bundle.toml` секции `[bundle]`.
#[derive(Debug, Clone, Deserialize)]
pub struct BundleMetadata {
    pub name: String,
    pub version: String,
    pub requires_bosun: String,
    /// Относительный путь под bundle/ к стартовому манифесту (например `manifests/main.star`).
    pub entry: String,
}

/// Корневая структура `bundle.toml`. Только секция `[bundle]` обязательна.
#[derive(Debug, Deserialize)]
struct BundleManifest {
    bundle: BundleMetadata,
}

/// Загруженный bundle: метаданные, контент манифестов, путь к templates,
/// объединённые defaults.
#[derive(Debug)]
pub struct Bundle {
    pub metadata: BundleMetadata,
    pub root: PathBuf,
    /// Контент всех `*.star` манифестов: ключ — относительный путь под bundle/,
    /// значение — содержимое файла.
    pub manifests: HashMap<PathBuf, String>,
    pub templates_root: PathBuf,
    /// JSON-представление слитых defaults/*.yaml. Если defaults/ пуст или
    /// отсутствует, корень — пустой объект.
    pub defaults: serde_json::Value,
}

impl Bundle {
    /// Прочитать bundle из директории. Читает `bundle.toml`, все `manifests/*.star`,
    /// сливает `defaults/*.yaml` в один JSON-объект, фиксирует путь к `templates/`.
    pub fn load_dir(path: &Path) -> Result<Self, BundleError> {
        let manifest = load_manifest(path)?;
        let entry_path = path.join(&manifest.bundle.entry);
        if !entry_path.is_file() {
            return Err(BundleError::EntryNotFound(
                entry_path.to_string_lossy().into_owned(),
            ));
        }

        let manifests = load_manifests(path)?;
        let defaults = load_defaults(path)?;
        let templates_root = path.join("templates");

        Ok(Self {
            metadata: manifest.bundle,
            root: path.to_path_buf(),
            manifests,
            templates_root,
            defaults,
        })
    }

    /// Проверить совместимость текущей версии bosun с требованием bundle.
    /// Используется парсер `semver::VersionReq` с cargo-семантикой.
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

    /// Получить содержимое entry-манифеста (например `manifests/main.star`).
    pub fn entry_manifest(&self) -> Option<&str> {
        let entry_rel = PathBuf::from(&self.metadata.entry);
        self.manifests.get(&entry_rel).map(|s| s.as_str())
    }

    /// Deep-merge defaults с override. Правила:
    /// - Object + Object: ключи объединяются, override побеждает при коллизии.
    /// - Array/Scalar в override: полностью заменяет defaults.
    /// - Null в override: удаляет ключ из defaults.
    pub fn merge_inventory(&self, override_value: serde_json::Value) -> serde_json::Value {
        merge_json(self.defaults.clone(), override_value)
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

/// Загрузить все `*.star` файлы из `manifests/` рекурсивно. Если директории
/// нет, возвращаем пустой набор — entry-проверка отдельно отловит проблему.
fn load_manifests(root: &Path) -> Result<HashMap<PathBuf, String>, BundleError> {
    let manifests_dir = root.join("manifests");
    if !manifests_dir.exists() {
        return Ok(HashMap::new());
    }
    let mut out = HashMap::new();
    walk_star_files(root, &manifests_dir, &mut out)?;
    Ok(out)
}

fn walk_star_files(
    root: &Path,
    dir: &Path,
    out: &mut HashMap<PathBuf, String>,
) -> Result<(), BundleError> {
    let entries = std::fs::read_dir(dir).map_err(|e| BundleError::Io {
        path: dir.to_string_lossy().into_owned(),
        source: e,
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| BundleError::Io {
            path: dir.to_string_lossy().into_owned(),
            source: e,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| BundleError::Io {
            path: path.to_string_lossy().into_owned(),
            source: e,
        })?;
        if file_type.is_dir() {
            walk_star_files(root, &path, out)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("star") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| BundleError::Io {
            path: path.to_string_lossy().into_owned(),
            source: e,
        })?;
        let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        out.insert(rel, text);
    }
    Ok(())
}

/// Прочитать все `*.yaml` файлы из `defaults/` и слить deep-merge-ом в один JSON-объект.
/// Порядок имеет значение: файлы сортируются по имени, каждый следующий
/// перезаписывает предыдущий при коллизии ключей (без null-семантики удаления —
/// это поведение из override, не между defaults-файлами).
fn load_defaults(root: &Path) -> Result<serde_json::Value, BundleError> {
    let defaults_dir = root.join("defaults");
    if !defaults_dir.exists() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let entries = std::fs::read_dir(&defaults_dir).map_err(|e| BundleError::Io {
        path: defaults_dir.to_string_lossy().into_owned(),
        source: e,
    })?;
    let mut yaml_files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| BundleError::Io {
            path: defaults_dir.to_string_lossy().into_owned(),
            source: e,
        })?;
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if matches!(ext, Some("yaml") | Some("yml")) {
            yaml_files.push(path);
        }
    }
    yaml_files.sort();

    let mut merged = serde_json::Value::Object(serde_json::Map::new());
    for yaml_path in yaml_files {
        let text = std::fs::read_to_string(&yaml_path).map_err(|e| BundleError::Io {
            path: yaml_path.to_string_lossy().into_owned(),
            source: e,
        })?;
        let yaml_value: serde_norway::Value =
            serde_norway::from_str(&text).map_err(|e| BundleError::InvalidYaml {
                path: yaml_path.to_string_lossy().into_owned(),
                source: e,
            })?;
        let json_value = yaml_to_json(yaml_value).map_err(BundleError::InvalidManifest)?;
        merged = merge_json_no_null_delete(merged, json_value);
    }
    Ok(merged)
}

/// Конвертация serde_norway::Value → serde_json::Value. Не поддерживаем
/// сложные ключи маппинга (только строковые ключи в YAML), а также tagged-значения.
fn yaml_to_json(v: serde_norway::Value) -> Result<serde_json::Value, String> {
    use serde_norway::Value as Y;
    Ok(match v {
        Y::Null => serde_json::Value::Null,
        Y::Bool(b) => serde_json::Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                serde_json::Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            } else {
                return Err(format!("unsupported number in YAML: {n:?}"));
            }
        }
        Y::String(s) => serde_json::Value::String(s),
        Y::Sequence(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for item in seq {
                out.push(yaml_to_json(item)?);
            }
            serde_json::Value::Array(out)
        }
        Y::Mapping(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                let key = match k {
                    Y::String(s) => s,
                    other => {
                        return Err(format!("YAML mapping key must be a string; got {other:?}"));
                    }
                };
                out.insert(key, yaml_to_json(v)?);
            }
            serde_json::Value::Object(out)
        }
        Y::Tagged(_) => {
            return Err("YAML tagged values are not supported in bundle defaults".to_string());
        }
    })
}

/// Deep-merge defaults и override с null-семантикой удаления.
/// - Object + Object: ключи сливаются; null в override удаляет ключ defaults.
/// - null на корне override: «override не задан», возвращаем defaults без изменений.
///   Это отличается от null внутри map: null-как-значение-ключа удаляет ключ,
///   но null-как-корень означает отсутствие самого override.
/// - Иначе override полностью заменяет defaults.
fn merge_json(base: serde_json::Value, over: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (base, over) {
        (base, Value::Null) => base,
        (Value::Object(mut base_map), Value::Object(over_map)) => {
            for (k, v) in over_map {
                if v.is_null() {
                    base_map.remove(&k);
                    continue;
                }
                match base_map.remove(&k) {
                    Some(base_v) => {
                        base_map.insert(k, merge_json(base_v, v));
                    }
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
            Value::Object(base_map)
        }
        (_, over) => over,
    }
}

/// То же, что merge_json, но без удаления по null — используется для слияния
/// defaults-файлов между собой.
fn merge_json_no_null_delete(
    base: serde_json::Value,
    over: serde_json::Value,
) -> serde_json::Value {
    use serde_json::Value;
    match (base, over) {
        (Value::Object(mut base_map), Value::Object(over_map)) => {
            for (k, v) in over_map {
                match base_map.remove(&k) {
                    Some(base_v) => {
                        base_map.insert(k, merge_json_no_null_delete(base_v, v));
                    }
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
            Value::Object(base_map)
        }
        (_, over) => over,
    }
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
    fn load_dir_reads_metadata_and_manifests() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name           = "demo"
version        = "0.1.0"
requires_bosun = "^0.1"
entry          = "manifests/main.star"
"#,
        );
        write(
            &root.join("manifests/main.star"),
            "load(\"@bosun/builtins\", \"apt\")\n",
        );
        write(&root.join("defaults/main.yaml"), "foo: bar\n");
        fs::create_dir_all(root.join("templates")).unwrap();

        let bundle = Bundle::load_dir(&root).unwrap();
        assert_eq!(bundle.metadata.name, "demo");
        assert_eq!(bundle.metadata.entry, "manifests/main.star");
        assert!(bundle.entry_manifest().unwrap().contains("@bosun/builtins"));
        assert_eq!(bundle.defaults["foo"], serde_json::json!("bar"));
        assert_eq!(bundle.templates_root, root.join("templates"));
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
    fn check_compatibility_supports_explicit_range_req() {
        let (_keep, root) = make_bundle_dir();
        write(
            &root.join("bundle.toml"),
            r#"
[bundle]
name = "demo"
version = "0.1.0"
requires_bosun = ">=0.1, <0.3"
entry = "manifests/main.star"
"#,
        );
        write(&root.join("manifests/main.star"), "");
        let bundle = Bundle::load_dir(&root).unwrap();
        bundle.check_compatibility("0.1.0").unwrap();
        bundle.check_compatibility("0.2.5").unwrap();
        let err = bundle.check_compatibility("0.3.0").unwrap_err();
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
    fn merge_inventory_combines_nested_objects() {
        let bundle = with_defaults(serde_json::json!({"a": {"b": 1}}));
        let result = bundle.merge_inventory(serde_json::json!({"a": {"c": 2}}));
        assert_eq!(result, serde_json::json!({"a": {"b": 1, "c": 2}}));
    }

    #[test]
    fn merge_inventory_override_wins_on_scalar_collision() {
        let bundle = with_defaults(serde_json::json!({"name": "default"}));
        let result = bundle.merge_inventory(serde_json::json!({"name": "override"}));
        assert_eq!(result, serde_json::json!({"name": "override"}));
    }

    #[test]
    fn merge_inventory_array_is_replaced_not_concatenated() {
        let bundle = with_defaults(serde_json::json!({"a": [1, 2, 3]}));
        let result = bundle.merge_inventory(serde_json::json!({"a": [4, 5]}));
        assert_eq!(result, serde_json::json!({"a": [4, 5]}));
    }

    #[test]
    fn merge_inventory_null_removes_key() {
        let bundle = with_defaults(serde_json::json!({"a": {"b": 1, "c": 2}}));
        let result = bundle.merge_inventory(serde_json::json!({"a": {"b": null}}));
        assert_eq!(result, serde_json::json!({"a": {"c": 2}}));
    }

    #[test]
    fn merge_inventory_empty_override_keeps_defaults() {
        let bundle = with_defaults(serde_json::json!({"a": 1, "b": 2}));
        let result = bundle.merge_inventory(serde_json::json!({}));
        assert_eq!(result, serde_json::json!({"a": 1, "b": 2}));
    }

    #[test]
    fn merge_inventory_null_at_root_keeps_defaults() {
        // null на корне override'а трактуем как «override не передан» —
        // возвращаем defaults без изменений. Этот путь срабатывает, когда
        // CLI вызывается без флага `--inventory`.
        let bundle = with_defaults(serde_json::json!({"a": 1}));
        let result = bundle.merge_inventory(serde_json::Value::Null);
        assert_eq!(result, serde_json::json!({"a": 1}));
    }

    #[test]
    fn merge_inventory_null_at_root_with_empty_defaults_keeps_empty_object() {
        let bundle = with_defaults(serde_json::json!({}));
        let result = bundle.merge_inventory(serde_json::Value::Null);
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn merge_inventory_scalar_into_object_replaces() {
        let bundle = with_defaults(serde_json::json!({"a": {"b": 1}}));
        let result = bundle.merge_inventory(serde_json::json!({"a": "simple"}));
        assert_eq!(result, serde_json::json!({"a": "simple"}));
    }

    #[test]
    fn merge_inventory_object_into_scalar_replaces() {
        let bundle = with_defaults(serde_json::json!({"a": 1}));
        let result = bundle.merge_inventory(serde_json::json!({"a": {"b": 2}}));
        assert_eq!(result, serde_json::json!({"a": {"b": 2}}));
    }

    #[test]
    fn load_defaults_merges_multiple_yaml_files_in_alphabetical_order() {
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
        // a-file задаёт x=1, b-file перебивает на x=2 и добавляет y=3.
        write(&root.join("defaults/a-base.yaml"), "x: 1\nshared: from_a\n");
        write(&root.join("defaults/b-extra.yaml"), "x: 2\ny: 3\n");
        let bundle = Bundle::load_dir(&root).unwrap();
        assert_eq!(bundle.defaults["x"], serde_json::json!(2));
        assert_eq!(bundle.defaults["y"], serde_json::json!(3));
        assert_eq!(bundle.defaults["shared"], serde_json::json!("from_a"));
    }

    #[test]
    fn load_defaults_returns_empty_when_missing() {
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
        assert_eq!(bundle.defaults, serde_json::json!({}));
    }

    #[test]
    fn load_manifests_finds_nested_star_files() {
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
        write(&root.join("manifests/main.star"), "# main\n");
        write(&root.join("manifests/lib/util.star"), "# util\n");
        let bundle = Bundle::load_dir(&root).unwrap();
        let main_rel = PathBuf::from("manifests/main.star");
        let util_rel = PathBuf::from("manifests/lib/util.star");
        assert!(bundle.manifests.contains_key(&main_rel));
        assert!(bundle.manifests.contains_key(&util_rel));
    }

    #[test]
    fn invalid_yaml_in_defaults_returns_error() {
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
        write(&root.join("defaults/bad.yaml"), "x: : :");
        let err = Bundle::load_dir(&root).unwrap_err();
        assert!(matches!(err, BundleError::InvalidYaml { .. }));
    }

    fn with_defaults(defaults: serde_json::Value) -> Bundle {
        Bundle {
            metadata: BundleMetadata {
                name: "test".into(),
                version: "0.1.0".into(),
                requires_bosun: "^0.1".into(),
                entry: "manifests/main.star".into(),
            },
            root: PathBuf::from("/tmp/nonexistent"),
            manifests: HashMap::new(),
            templates_root: PathBuf::from("/tmp/nonexistent/templates"),
            defaults,
        }
    }
}
