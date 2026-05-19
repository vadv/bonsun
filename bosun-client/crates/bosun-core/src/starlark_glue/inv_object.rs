//! Конвертация JSON → Starlark Value.
//!
//! Bundle rev 2 убрал mono-`inv` объект (на верх клался JSON inventory целиком
//! и `inv.X.Y` ходил по нему). Вместо этого manifest получает inventory как
//! обычный Starlark dict через `inventory.load`/`inventory.merge`, и работает
//! с ним идиомами Starlark (`m["key"]`, `for k, v in m.items()`). Здесь
//! осталась только утилита для маршалинга JSON → Value, используемая
//! `inventory.load` и тестами.

use starlark::collections::SmallMap;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, Value};

/// Конвертация JSON в Starlark Value. Числа за пределами i32 кодируются как
/// строка — обычная арифметика Starlark ограничена i32, и для inventory это
/// корректное поведение (большие числа реально приходят как идентификаторы).
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
    use super::*;

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
    fn json_scalar_object_round_trips_via_dict() {
        let heap = Heap::new();
        let v = json_scalar_to_value(&heap, serde_json::json!({"a": 1, "b": "x"}));
        // Просто проверяем тип: dict с двумя ключами.
        assert_eq!(v.get_type(), "dict");
    }
}
