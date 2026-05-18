use std::collections::HashMap;
use std::sync::Mutex;

use crate::resource::ResourceId;

/// Маскирующий newtype для секретного содержимого.
/// Debug/Display печатают `<sensitive: N bytes>`, не настоящее значение.
pub struct SensitivePayload<T>(T);

impl<T> SensitivePayload<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }

    pub fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T: AsRef<str>> std::fmt::Debug for SensitivePayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.as_ref().len();
        write!(f, "<sensitive: {bytes} bytes>")
    }
}

impl<T: AsRef<str>> std::fmt::Display for SensitivePayload<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0.as_ref().len();
        write!(f, "<sensitive: {bytes} bytes>")
    }
}

/// Side-channel хранилище для секретных payload'ов (например, file.content.contents).
/// Передаётся в ApplyCtx; примитив выгружает значение через take(&id).
#[derive(Default)]
pub struct SensitiveStore {
    inner: Mutex<HashMap<ResourceId, SensitivePayload<String>>>,
}

impl SensitiveStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, id: ResourceId, value: SensitivePayload<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(id, value);
        }
    }

    pub fn take(&self, id: &ResourceId) -> Option<SensitivePayload<String>> {
        self.inner.lock().ok().and_then(|mut g| g.remove(id))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::resource::ResourceKind;

    #[test]
    fn debug_masks_value() {
        let s: SensitivePayload<String> = SensitivePayload::new("super-secret-password".into());
        let dbg = format!("{:?}", s);
        assert!(!dbg.contains("super-secret-password"));
        assert!(dbg.contains("sensitive"));
        assert!(dbg.contains("21 bytes"));
    }

    #[test]
    fn store_put_take_round_trip() {
        let kind = ResourceKind::from_static("file.content");
        let id = ResourceId::new(&kind, "/etc/secret");
        let store = SensitiveStore::new();
        store.put(id.clone(), SensitivePayload::new("body".into()));
        let taken = store.take(&id).unwrap();
        assert_eq!(taken.into_inner(), "body");
        assert!(store.take(&id).is_none(), "second take returns None");
    }
}
