//! Inventory: модель данных манифеста плюс стратегии слияния YAML/JSON.
//!
//! Bundle hint: inventory загружается из Starlark через `inventory.load`,
//! сливается через `inventory.merge` / `inventory.merge_keyed`. Эти helpers
//! живут здесь, чтобы их можно было unit-тестировать в чистом виде, без
//! Starlark-обвязки.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InventoryError {
    #[error("inv: key '{0}' not found in inventory")]
    KeyNotFound(String),
    #[error("inv: type mismatch at '{path}': expected {expected}, got {actual}")]
    TypeMismatch {
        path: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("inventory.merge_keyed: list at '{path}' contains element without key '{key}'")]
    KeyedListMissingKey { path: String, key: String },
    #[error("inventory.merge_keyed: list at '{path}' contains non-map element")]
    KeyedListNonMapElement { path: String },
    #[error("inventory.merge: unknown strategy '{0}'")]
    UnknownMergeStrategy(String),
}

/// Read-only доступ к inventory из манифеста (`inv.foo`, `inv.nested.bar`).
/// Send/Sync не требуется: apply однопоточный.
pub trait InventorySource {
    fn get(&self, dotted_path: &str) -> Result<&serde_json::Value, InventoryError>;
}

/// Стандартная реализация над serde_json::Value (root — Object).
pub struct JsonInventory {
    root: serde_json::Value,
}

impl JsonInventory {
    pub fn new(root: serde_json::Value) -> Self {
        Self { root }
    }
}

impl InventorySource for JsonInventory {
    fn get(&self, dotted_path: &str) -> Result<&serde_json::Value, InventoryError> {
        let mut node = &self.root;
        for segment in dotted_path.split('.') {
            match node {
                serde_json::Value::Object(map) => {
                    node = map
                        .get(segment)
                        .ok_or_else(|| InventoryError::KeyNotFound(dotted_path.into()))?;
                }
                _ => {
                    return Err(InventoryError::TypeMismatch {
                        path: dotted_path.into(),
                        expected: "object",
                        actual: variant_name(node),
                    });
                }
            }
        }
        Ok(node)
    }
}

/// Стратегии слияния inventory'ев.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Deep merge maps; правый источник заменяет list-поля целиком; null удаляет ключ.
    DeepMapReplaceList,
    /// Deep merge maps; list-поля конкатенируются с дедупликацией по equality.
    DeepMapAppendList,
    /// Правый источник полностью заменяет левый на любом уровне.
    Replace,
}

impl MergeStrategy {
    pub fn parse(s: &str) -> Result<Self, InventoryError> {
        match s {
            "deep_map_replace_list" => Ok(Self::DeepMapReplaceList),
            "deep_map_append_list" => Ok(Self::DeepMapAppendList),
            "replace" => Ok(Self::Replace),
            other => Err(InventoryError::UnknownMergeStrategy(other.to_string())),
        }
    }
}

/// Слить два inventory'я. `over` имеет приоритет; `null` в правом удаляет ключ
/// независимо от стратегии.
pub fn merge_inventory(base: Value, over: Value, strategy: MergeStrategy) -> Value {
    match strategy {
        MergeStrategy::Replace => merge_replace(base, over),
        MergeStrategy::DeepMapReplaceList => merge_deep(base, over, /* append_lists */ false),
        MergeStrategy::DeepMapAppendList => merge_deep(base, over, /* append_lists */ true),
    }
}

fn merge_replace(_base: Value, over: Value) -> Value {
    // Стратегия Replace: правый источник побеждает целиком, включая Null
    // (это удаляет вершину). На любом уровне ниже merge_inventory не
    // рекурсирует, поэтому ничего больше делать не нужно.
    over
}

fn merge_deep(base: Value, over: Value, append_lists: bool) -> Value {
    match (base, over) {
        (base, Value::Null) => {
            // null на корне ничего не делает (нет ключа, который удалять).
            base
        }
        (Value::Object(mut base_map), Value::Object(over_map)) => {
            for (k, v) in over_map {
                if v.is_null() {
                    base_map.remove(&k);
                    continue;
                }
                match base_map.remove(&k) {
                    Some(base_v) => {
                        base_map.insert(k, merge_deep(base_v, v, append_lists));
                    }
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
            Value::Object(base_map)
        }
        (Value::Array(mut base_arr), Value::Array(over_arr)) if append_lists => {
            for item in over_arr {
                if !base_arr.iter().any(|existing| existing == &item) {
                    base_arr.push(item);
                }
            }
            Value::Array(base_arr)
        }
        (_, over) => over,
    }
}

/// Слить inventory'и по ключу-полю в каждом list-of-records. Top-level — обычный
/// deep merge. Любой list внутри обязан состоять из maps; элементы объединяются
/// по совпадению `<key>`.
pub fn merge_inventory_keyed(base: Value, over: Value, key: &str) -> Result<Value, InventoryError> {
    merge_keyed_at(base, over, key, "$")
}

fn merge_keyed_at(
    base: Value,
    over: Value,
    key: &str,
    path: &str,
) -> Result<Value, InventoryError> {
    match (base, over) {
        (base, Value::Null) => Ok(base),
        (Value::Object(mut base_map), Value::Object(over_map)) => {
            for (k, v) in over_map {
                let child_path = format!("{path}.{k}");
                if v.is_null() {
                    base_map.remove(&k);
                    continue;
                }
                match base_map.remove(&k) {
                    Some(base_v) => {
                        let merged = merge_keyed_at(base_v, v, key, &child_path)?;
                        base_map.insert(k, merged);
                    }
                    None => {
                        base_map.insert(k, v);
                    }
                }
            }
            Ok(Value::Object(base_map))
        }
        (Value::Array(base_arr), Value::Array(over_arr)) => {
            merge_keyed_lists(base_arr, over_arr, key, path)
        }
        (_, over) => Ok(over),
    }
}

fn merge_keyed_lists(
    base_arr: Vec<Value>,
    over_arr: Vec<Value>,
    key: &str,
    path: &str,
) -> Result<Value, InventoryError> {
    let mut merged: Vec<Value> = Vec::with_capacity(base_arr.len() + over_arr.len());
    for item in base_arr {
        ensure_map_with_key(&item, key, path)?;
        merged.push(item);
    }
    for item in over_arr {
        ensure_map_with_key(&item, key, path)?;
        let item_key = key_value_of(&item, key);
        match merged
            .iter()
            .position(|existing| key_value_of(existing, key).as_ref() == item_key.as_ref())
        {
            Some(idx) => {
                let prev = std::mem::take(&mut merged[idx]);
                let child_path = format!("{path}[{key}={item_key:?}]");
                merged[idx] = merge_keyed_at(prev, item, key, &child_path)?;
            }
            None => merged.push(item),
        }
    }
    Ok(Value::Array(merged))
}

fn ensure_map_with_key(item: &Value, key: &str, path: &str) -> Result<(), InventoryError> {
    match item {
        Value::Object(map) => {
            if !map.contains_key(key) {
                return Err(InventoryError::KeyedListMissingKey {
                    path: path.to_string(),
                    key: key.to_string(),
                });
            }
            Ok(())
        }
        _ => Err(InventoryError::KeyedListNonMapElement {
            path: path.to_string(),
        }),
    }
}

fn key_value_of(item: &Value, key: &str) -> Option<Value> {
    item.as_object().and_then(|m| m.get(key)).cloned()
}

fn variant_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn flat_key_found() {
        let inv = JsonInventory::new(serde_json::json!({"name": "nginx"}));
        assert_eq!(inv.get("name").unwrap(), &serde_json::json!("nginx"));
    }

    #[test]
    fn nested_key_found() {
        let inv = JsonInventory::new(serde_json::json!({"nginx": {"workers": 4}}));
        assert_eq!(inv.get("nginx.workers").unwrap(), &serde_json::json!(4));
    }

    #[test]
    fn missing_key_returns_keyerror() {
        let inv = JsonInventory::new(serde_json::json!({"name": "x"}));
        let err = inv.get("missing").unwrap_err();
        assert!(matches!(err, InventoryError::KeyNotFound(_)));
    }

    #[test]
    fn type_mismatch_when_traversing_scalar() {
        let inv = JsonInventory::new(serde_json::json!({"name": "x"}));
        let err = inv.get("name.extra").unwrap_err();
        assert!(matches!(err, InventoryError::TypeMismatch { .. }));
    }

    // ---- merge_inventory ----

    #[test]
    fn deep_map_replace_list_deep_merges_maps() {
        let base = serde_json::json!({"a": {"b": 1, "c": 2}});
        let over = serde_json::json!({"a": {"c": 99, "d": 3}});
        let merged = merge_inventory(base, over, MergeStrategy::DeepMapReplaceList);
        assert_eq!(merged, serde_json::json!({"a": {"b": 1, "c": 99, "d": 3}}));
    }

    #[test]
    fn deep_map_replace_list_replaces_arrays() {
        let base = serde_json::json!({"items": [1, 2, 3]});
        let over = serde_json::json!({"items": [4, 5]});
        let merged = merge_inventory(base, over, MergeStrategy::DeepMapReplaceList);
        assert_eq!(merged, serde_json::json!({"items": [4, 5]}));
    }

    #[test]
    fn deep_map_replace_list_null_deletes_key() {
        let base = serde_json::json!({"a": 1, "b": 2});
        let over = serde_json::json!({"a": null});
        let merged = merge_inventory(base, over, MergeStrategy::DeepMapReplaceList);
        assert_eq!(merged, serde_json::json!({"b": 2}));
    }

    #[test]
    fn deep_map_append_list_concats_arrays_with_dedup() {
        let base = serde_json::json!({"items": ["a", "b"]});
        let over = serde_json::json!({"items": ["b", "c"]});
        let merged = merge_inventory(base, over, MergeStrategy::DeepMapAppendList);
        assert_eq!(merged, serde_json::json!({"items": ["a", "b", "c"]}));
    }

    #[test]
    fn replace_strategy_replaces_whole_value() {
        let base = serde_json::json!({"a": {"b": 1, "c": 2}});
        let over = serde_json::json!({"a": {"d": 3}});
        let merged = merge_inventory(base, over, MergeStrategy::Replace);
        // Replace заменяет верхний уровень целиком.
        assert_eq!(merged, serde_json::json!({"a": {"d": 3}}));
    }

    #[test]
    fn merge_strategy_parses_known_names() {
        assert_eq!(
            MergeStrategy::parse("deep_map_replace_list").unwrap(),
            MergeStrategy::DeepMapReplaceList
        );
        assert_eq!(
            MergeStrategy::parse("deep_map_append_list").unwrap(),
            MergeStrategy::DeepMapAppendList
        );
        assert_eq!(
            MergeStrategy::parse("replace").unwrap(),
            MergeStrategy::Replace
        );
    }

    #[test]
    fn merge_strategy_unknown_name_is_error() {
        let err = MergeStrategy::parse("magic").unwrap_err();
        assert!(matches!(err, InventoryError::UnknownMergeStrategy(_)));
    }

    // ---- merge_inventory_keyed ----

    #[test]
    fn merge_keyed_combines_records_by_key() {
        let base = serde_json::json!({
            "servers": [
                {"id": "a", "role": "primary"},
                {"id": "b", "role": "replica"},
            ]
        });
        let over = serde_json::json!({
            "servers": [
                {"id": "b", "weight": 10},
                {"id": "c", "role": "standby"},
            ]
        });
        let merged = merge_inventory_keyed(base, over, "id").unwrap();
        assert_eq!(
            merged,
            serde_json::json!({
                "servers": [
                    {"id": "a", "role": "primary"},
                    {"id": "b", "role": "replica", "weight": 10},
                    {"id": "c", "role": "standby"},
                ]
            })
        );
    }

    #[test]
    fn merge_keyed_missing_key_is_error() {
        let base = serde_json::json!({"servers": [{"id": "a"}]});
        let over = serde_json::json!({"servers": [{"name": "x"}]});
        let err = merge_inventory_keyed(base, over, "id").unwrap_err();
        assert!(matches!(err, InventoryError::KeyedListMissingKey { .. }));
    }

    #[test]
    fn merge_keyed_non_map_element_is_error() {
        let base = serde_json::json!({"servers": ["raw"]});
        let over = serde_json::json!({"servers": []});
        let err = merge_inventory_keyed(base, over, "id").unwrap_err();
        assert!(matches!(err, InventoryError::KeyedListNonMapElement { .. }));
    }
}
