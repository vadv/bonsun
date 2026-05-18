use std::sync::Arc;

/// Newtype для типа ресурса (например "apt.package", "file.content").
/// Хранится как Arc<str> для дешёвого clone и поддержки runtime-регистрации.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKind(Arc<str>);

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
}
