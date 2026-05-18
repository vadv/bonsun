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
}

/// Read-only доступ к inventory из манифеста (`inv.foo`, `inv.nested.bar`).
/// Send/Sync не требуется: apply однопоточный, см. комментарий в `primitive.rs`.
pub trait InventorySource {
    /// Получить значение по dotted-path (например "nginx.workers"). None → KeyNotFound.
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
}
