use std::sync::Arc;

use serde::{Serialize, Serializer};

/// Newtype для типа ресурса (например "apt.package", "file.content").
/// Хранится как Arc<str> для дешёвого clone и поддержки runtime-регистрации.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKind(Arc<str>);

impl Serialize for ResourceKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ResourceKindError {
    #[error("resource kind must be non-empty")]
    Empty,
    #[error("resource kind '{0}' contains invalid character; expected kebab-case dotted (e.g. apt.package)")]
    InvalidChar(String),
}

impl ResourceKind {
    /// Для built-in примитивов — статика, формат гарантируется автором.
    pub fn from_static(s: &'static str) -> Self {
        // Базовая sanity-проверка: не пустая, не содержит управляющих символов.
        debug_assert!(!s.is_empty(), "ResourceKind::from_static empty");
        Self(Arc::from(s))
    }

    /// Для runtime-регистрации (будущие плагины).
    pub fn try_new(s: impl Into<String>) -> Result<Self, ResourceKindError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ResourceKindError::Empty);
        }
        for ch in s.chars() {
            let allowed = ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || ch == '.'
                || ch == '_'
                || ch == '-';
            if !allowed {
                return Err(ResourceKindError::InvalidChar(s));
            }
        }
        Ok(Self(Arc::from(s)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Глобально уникальный идентификатор ресурса. Хранится как Arc<str>.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceId(Arc<str>);

impl Serialize for ResourceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl ResourceId {
    /// Сконструировать ResourceId из kind и identity-segment.
    /// Формат: "<kind>:<identity>". Например, "apt.package:nginx".
    pub fn new(kind: &ResourceKind, identity: &str) -> Self {
        let s = format!("{}:{}", kind.as_str(), identity);
        Self(Arc::from(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Handle — opaque newtype над ResourceId, используется в Starlark для
/// связей `reload_on=[...]`, `depends_on=[...]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Handle(pub ResourceId);

/// Зарегистрированный ресурс в Registry. Payload type-erased через JSON,
/// каждый примитив десериализует payload в собственный Spec через serde.
#[derive(Clone, Debug)]
pub struct Resource {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub spec_version: u16,
    pub payload: serde_json::Value,
    pub reload_on: Vec<ResourceId>,
    pub depends_on: Vec<ResourceId>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn from_static_apt_package_ok() {
        let k = ResourceKind::from_static("apt.package");
        assert_eq!(k.as_str(), "apt.package");
    }

    #[test]
    fn try_new_empty_returns_empty_error() {
        let err = ResourceKind::try_new("").unwrap_err();
        assert!(matches!(err, ResourceKindError::Empty));
    }

    #[test]
    fn try_new_uppercase_returns_invalid_char() {
        let err = ResourceKind::try_new("Apt.Package").unwrap_err();
        assert!(matches!(err, ResourceKindError::InvalidChar(_)));
    }

    #[test]
    fn try_new_space_returns_invalid_char() {
        let err = ResourceKind::try_new("apt package").unwrap_err();
        assert!(matches!(err, ResourceKindError::InvalidChar(_)));
    }

    #[test]
    fn try_new_dotted_and_dash_ok() {
        ResourceKind::try_new("apt.package").unwrap();
        ResourceKind::try_new("runr.service").unwrap();
        ResourceKind::try_new("kafka-cluster.topic").unwrap();
    }

    #[test]
    fn equal_kinds_have_equal_hash() {
        use std::collections::HashSet;
        let a = ResourceKind::from_static("apt.package");
        let b = ResourceKind::try_new("apt.package").unwrap();
        let mut set: HashSet<ResourceKind> = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn resource_id_format_matches_kind_colon_identity() {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, "nginx");
        assert_eq!(id.as_str(), "apt.package:nginx");
    }

    #[test]
    fn resource_id_equal_when_same_kind_and_identity() {
        let kind = ResourceKind::from_static("file.content");
        let a = ResourceId::new(&kind, "/etc/nginx/nginx.conf");
        let b = ResourceId::new(&kind, "/etc/nginx/nginx.conf");
        assert_eq!(a, b);
    }

    #[test]
    fn handle_wraps_resource_id() {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, "nginx");
        let h = Handle(id.clone());
        assert_eq!(h.0, id);
    }
}
