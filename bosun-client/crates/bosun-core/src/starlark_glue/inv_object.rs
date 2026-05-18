//! Динамические объекты для `inv` и `inv.facts` в Starlark.
//!
//! Поведение (см. spec «inv в Starlark»):
//! - `inv.foo` — значение из inventory.
//! - `inv.nested.bar` — вложенный объект, рекурсивный.
//! - `inv.facts.bar` — `FactsSource::get("bar")` через thread-local state.
//! - Отсутствие ключа в inventory → starlark вернёт ошибку «attribute not found».
//!
//! Архитектурное решение: каждый `inv.X` — это nested `InvObject` с
//! собственным фрагментом JSON. `inv.facts` — отдельный тип `FactsObject`,
//! который при `get_attr` берёт текущий state из thread-local
//! (`with_state(...)`) и делегирует в `FactsSource`.

use std::fmt;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{FreezeResult, Heap, NoSerialize, StarlarkValue, Value};
use starlark_derive::{starlark_value, Freeze, Trace};

use crate::facts::FactValue;
use crate::starlark_glue::with_state;

/// `inv`-объект: вложенный attribute-доступ к JSON-инвентарю.
///
/// Поля помечены `#[trace(static)]`/`#[freeze(identity)]` потому что
/// они не содержат Starlark `Value<'v>` — это owned данные, неподвижные
/// при freeze/trace heap. `#[allocative(skip)]` снимает Allocative-bound
/// для serde_json::Value, который не реализует Allocative из коробки.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, Freeze)]
pub(crate) struct InvObject {
    #[allocative(skip)]
    #[trace(static)]
    #[freeze(identity)]
    value: serde_json::Value,
    /// Полный путь от корня для информативных fail-сообщений и
    /// чтобы отличать «корень» от nested-уровня (для `inv.facts`).
    path: String,
}

starlark_simple_value!(InvObject);

impl fmt::Display for InvObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str("inv")
        } else {
            write!(f, "inv.{}", self.path)
        }
    }
}

impl InvObject {
    pub(crate) fn root(value: serde_json::Value) -> Self {
        Self {
            value,
            path: String::new(),
        }
    }

    fn child(&self, key: &str, child_value: serde_json::Value) -> Self {
        let path = if self.path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{key}", self.path)
        };
        Self {
            value: child_value,
            path,
        }
    }
}

#[starlark_value(type = "bosun.inv")]
impl<'v> StarlarkValue<'v> for InvObject {
    type Canonical = Self;

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        // `inv.facts` доступен только на корне.
        if attribute == "facts" && self.path.is_empty() {
            return Some(heap.alloc(FactsObject));
        }
        match &self.value {
            serde_json::Value::Object(map) => match map.get(attribute) {
                Some(child) => {
                    let child_inv = self.child(attribute, child.clone());
                    Some(alloc_inv_or_scalar(heap, child_inv))
                }
                None => None,
            },
            _ => None,
        }
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        if attribute == "facts" && self.path.is_empty() {
            return true;
        }
        match &self.value {
            serde_json::Value::Object(map) => map.contains_key(attribute),
            _ => false,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut names = match &self.value {
            serde_json::Value::Object(map) => map.keys().cloned().collect(),
            _ => Vec::new(),
        };
        if self.path.is_empty() {
            names.push("facts".to_string());
        }
        names
    }
}

/// `inv.facts`-объект. Не хранит данные: каждый attr читает FactsSource
/// из thread-local state.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, Freeze)]
pub(crate) struct FactsObject;

starlark_simple_value!(FactsObject);

impl fmt::Display for FactsObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("inv.facts")
    }
}

#[starlark_value(type = "bosun.inv.facts")]
impl<'v> StarlarkValue<'v> for FactsObject {
    type Canonical = Self;

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        let fact = with_state(|state| state.facts.get(attribute))?;
        Some(fact_value_to_starlark(heap, fact))
    }

    fn has_attr(&self, _attribute: &str, _heap: &'v Heap) -> bool {
        // FactsSource через trait не отдаёт списка имён; считаем, что
        // любой атрибут «существует» — реальный get отдаст Unknown → None,
        // что эквивалентно «нет атрибута» с точки зрения Starlark.
        true
    }

    fn dir_attr(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Маршалинг `FactValue` в Starlark по правилам spec:
/// - Known(v) → конвертируется в Starlark-значение.
/// - Stale { value, .. } → конвертируется как Known, логируется info.
/// - Unknown → None.
fn fact_value_to_starlark(heap: &Heap, fact: FactValue) -> Value<'_> {
    match fact {
        FactValue::Known(v) => json_scalar_to_value(heap, v),
        FactValue::Stale { value, age_ms } => {
            tracing::info!(age_ms, "fact returned as Stale to Starlark");
            json_scalar_to_value(heap, value)
        }
        FactValue::Unknown { reason } => {
            tracing::debug!(reason = %reason, "fact returned as Unknown");
            Value::new_none()
        }
    }
}

/// Если значение — Object, оборачиваем в InvObject для дальнейшего .a.b.c.
/// Иначе — материализуем скаляр/массив сразу.
fn alloc_inv_or_scalar(heap: &Heap, inv: InvObject) -> Value<'_> {
    match &inv.value {
        serde_json::Value::Object(_) => heap.alloc(inv),
        _ => json_scalar_to_value(heap, inv.value),
    }
}

/// Конвертация JSON в Starlark Value (без обёртки в InvObject).
pub(crate) fn json_scalar_to_value(heap: &Heap, value: serde_json::Value) -> Value<'_> {
    use serde_json::Value as V;
    match value {
        V::Null => Value::new_none(),
        V::Bool(b) => Value::new_bool(b),
        V::Number(n) => {
            if let Some(i) = n.as_i64() {
                if let Ok(i32_val) = i32::try_from(i) {
                    heap.alloc(i32_val)
                } else {
                    // Большие числа представляем как строку — обычная Starlark
                    // арифметика ограничена i32.
                    heap.alloc(i.to_string())
                }
            } else if let Some(f) = n.as_f64() {
                heap.alloc(f)
            } else {
                heap.alloc(n.to_string())
            }
        }
        V::String(s) => heap.alloc(s),
        V::Array(arr) => {
            let items: Vec<Value> = arr
                .into_iter()
                .map(|v| json_scalar_to_value(heap, v))
                .collect();
            heap.alloc(AllocList(items))
        }
        V::Object(map) => {
            let mut sm = SmallMap::with_capacity(map.len());
            for (k, v) in map {
                let key_v: Value = heap.alloc(k);
                let Ok(hashed_key) = key_v.get_hashed() else {
                    continue;
                };
                sm.insert_hashed(hashed_key, json_scalar_to_value(heap, v));
            }
            heap.alloc(AllocDict(sm))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use starlark::values::ValueLike;

    use super::*;

    #[test]
    fn known_fact_translates_to_value() {
        let heap = Heap::new();
        let v = fact_value_to_starlark(&heap, FactValue::Known(serde_json::json!("host-1")));
        assert_eq!(v.unpack_str(), Some("host-1"));
    }

    #[test]
    fn stale_fact_translates_like_known() {
        let heap = Heap::new();
        let v = fact_value_to_starlark(
            &heap,
            FactValue::Stale {
                value: serde_json::json!(42),
                age_ms: 999,
            },
        );
        assert_eq!(v.unpack_i32(), Some(42));
    }

    #[test]
    fn unknown_fact_returns_none() {
        let heap = Heap::new();
        let v = fact_value_to_starlark(&heap, FactValue::Unknown { reason: "x".into() });
        assert!(v.is_none());
    }

    #[test]
    fn json_scalar_string() {
        let heap = Heap::new();
        let v = json_scalar_to_value(&heap, serde_json::json!("hello"));
        assert_eq!(v.unpack_str(), Some("hello"));
    }

    #[test]
    fn json_scalar_int() {
        let heap = Heap::new();
        let v = json_scalar_to_value(&heap, serde_json::json!(42));
        assert_eq!(v.unpack_i32(), Some(42));
    }

    #[test]
    fn json_scalar_bool() {
        let heap = Heap::new();
        let v_true = json_scalar_to_value(&heap, serde_json::json!(true));
        let v_false = json_scalar_to_value(&heap, serde_json::json!(false));
        assert_eq!(v_true.unpack_bool(), Some(true));
        assert_eq!(v_false.unpack_bool(), Some(false));
    }

    #[test]
    fn json_scalar_null_is_none() {
        let heap = Heap::new();
        let v = json_scalar_to_value(&heap, serde_json::Value::Null);
        assert!(v.is_none());
    }

    #[test]
    fn inv_object_get_attr_returns_nested_object() {
        let heap = Heap::new();
        let inv = InvObject::root(serde_json::json!({"a": {"b": 1}}));
        let v_a = inv.get_attr("a", &heap).unwrap();
        let inv_a = v_a.downcast_ref::<InvObject>().unwrap();
        assert_eq!(inv_a.path, "a");
        let v_b = inv_a.get_attr("b", &heap).unwrap();
        assert_eq!(v_b.unpack_i32(), Some(1));
    }

    #[test]
    fn inv_object_missing_key_returns_none() {
        let heap = Heap::new();
        let inv = InvObject::root(serde_json::json!({"a": 1}));
        assert!(inv.get_attr("missing", &heap).is_none());
    }

    #[test]
    fn inv_facts_returns_facts_object_only_at_root() {
        let heap = Heap::new();
        let inv = InvObject::root(serde_json::json!({"x": {"facts": "nope"}}));
        // На корне есть .facts.
        assert!(inv.get_attr("facts", &heap).is_some());
        // На вложенном — обычный ключ.
        let v_x = inv.get_attr("x", &heap).unwrap();
        let inv_x = v_x.downcast_ref::<InvObject>().unwrap();
        let nested_facts = inv_x.get_attr("facts", &heap).unwrap();
        assert_eq!(nested_facts.unpack_str(), Some("nope"));
    }

    #[test]
    fn inv_dir_attr_lists_keys_plus_facts_at_root() {
        let inv = InvObject::root(serde_json::json!({"a": 1, "b": 2}));
        let mut names = inv.dir_attr();
        names.sort();
        assert_eq!(names, vec!["a", "b", "facts"]);
    }

    #[test]
    fn inv_dir_attr_skips_facts_on_nested() {
        let inv = InvObject::root(serde_json::json!({"x": {"y": 1}}));
        let heap = Heap::new();
        let v_x = inv.get_attr("x", &heap).unwrap();
        let inv_x = v_x.downcast_ref::<InvObject>().unwrap();
        assert_eq!(inv_x.dir_attr(), vec!["y"]);
    }

    #[test]
    fn facts_object_dir_attr_is_empty() {
        let fo = FactsObject;
        assert!(fo.dir_attr().is_empty());
    }
}
