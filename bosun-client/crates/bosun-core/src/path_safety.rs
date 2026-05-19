//! Единый helper для path-safety проверок внутри bundle.
//!
//! Используется четырьмя резолверами: `bundle.toml.entry`, `inventory.load`,
//! `Bundle::resolve_module` (для `@roles/`/`@lib/`), `Bundle::resolve_template`.
//! Централизованная точка — security audit проверяет один helper, а не
//! четыре копии.
//!
//! Правила:
//! 1. Reject если относительный путь начинается с `/` (Component::RootDir)
//!    или содержит сегмент `..` (Component::ParentDir).
//! 2. Reject если в строке есть NUL-байт.
//! 3. Reject если `root` сам не существует или не канонизируется.
//! 4. Join + canonicalize кандидата. NotFound отображается отдельным
//!    вариантом, чтобы caller мог дать понятное сообщение.
//! 5. Reject если canonical-результат не начинается с canonical root.
//! 6. Reject если leaf — symlink (защита от подмены файла внутри bundle).
//!
//! Возвращает канонический PathBuf в случае успеха.

use std::path::{Component, Path, PathBuf};

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PathSafetyError {
    #[error("path must be relative, got absolute: {0}")]
    Absolute(String),
    #[error("path contains parent-dir segment ('..'): {0}")]
    ParentDir(String),
    #[error("path contains NUL byte")]
    NulByte,
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    #[error("path resolves outside root: attempted={attempted:?}, root={root:?}")]
    NotInRoot { root: PathBuf, attempted: PathBuf },
    #[error("path is a symlink (refusing to follow): {0}")]
    IsSymlink(PathBuf),
    #[error("io error while resolving path {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Резолв относительного пути под `root`.
///
/// `root` обязан существовать. `relative` — строка пути относительно `root`.
/// Возвращает канонический absolute PathBuf, готовый к чтению.
pub fn resolve_within_root(root: &Path, relative: &str) -> Result<PathBuf, PathSafetyError> {
    if relative.contains('\0') {
        return Err(PathSafetyError::NulByte);
    }

    let rel = Path::new(relative);
    for component in rel.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(PathSafetyError::Absolute(relative.to_string()));
            }
            Component::ParentDir => {
                return Err(PathSafetyError::ParentDir(relative.to_string()));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    let canonical_root = std::fs::canonicalize(root).map_err(|e| PathSafetyError::Io {
        path: root.to_path_buf(),
        source: e,
    })?;
    let candidate = canonical_root.join(rel);

    let canonical_candidate = match std::fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(PathSafetyError::NotFound(candidate));
        }
        Err(e) => {
            return Err(PathSafetyError::Io {
                path: candidate,
                source: e,
            });
        }
    };

    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(PathSafetyError::NotInRoot {
            root: canonical_root,
            attempted: canonical_candidate,
        });
    }

    let lmeta = std::fs::symlink_metadata(&candidate).map_err(|e| PathSafetyError::Io {
        path: candidate.clone(),
        source: e,
    })?;
    if lmeta.file_type().is_symlink() {
        return Err(PathSafetyError::IsSymlink(candidate));
    }

    Ok(canonical_candidate)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn resolves_simple_relative_path() {
        let dir = make_root();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let p = resolve_within_root(dir.path(), "a.txt").unwrap();
        assert!(p.ends_with("a.txt"));
    }

    #[test]
    fn resolves_nested_relative_path() {
        let dir = make_root();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "x").unwrap();
        let p = resolve_within_root(dir.path(), "sub/a.txt").unwrap();
        assert!(p.ends_with("sub/a.txt"));
    }

    #[test]
    fn rejects_absolute_path() {
        let dir = make_root();
        let err = resolve_within_root(dir.path(), "/etc/passwd").unwrap_err();
        assert!(matches!(err, PathSafetyError::Absolute(_)));
    }

    #[test]
    fn rejects_parent_dir_segment() {
        let dir = make_root();
        let err = resolve_within_root(dir.path(), "../etc/passwd").unwrap_err();
        assert!(matches!(err, PathSafetyError::ParentDir(_)));
    }

    #[test]
    fn rejects_parent_dir_in_middle() {
        let dir = make_root();
        let err = resolve_within_root(dir.path(), "a/../b").unwrap_err();
        assert!(matches!(err, PathSafetyError::ParentDir(_)));
    }

    #[test]
    fn rejects_nul_byte() {
        let dir = make_root();
        let err = resolve_within_root(dir.path(), "a\0b").unwrap_err();
        assert!(matches!(err, PathSafetyError::NulByte));
    }

    #[test]
    fn reports_not_found() {
        let dir = make_root();
        let err = resolve_within_root(dir.path(), "missing.txt").unwrap_err();
        assert!(matches!(err, PathSafetyError::NotFound(_)));
    }

    #[test]
    fn rejects_symlink_pointing_outside() {
        let dir = make_root();
        let outside = dir.path().parent().unwrap().join("outside.txt");
        std::fs::write(&outside, "data").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let err = resolve_within_root(dir.path(), "link.txt").unwrap_err();
        // Symlink-outside ловится либо как NotInRoot (resolved path не под root),
        // либо как IsSymlink — оба варианта приемлемы для security perspective.
        assert!(matches!(
            err,
            PathSafetyError::NotInRoot { .. } | PathSafetyError::IsSymlink(_)
        ));
    }

    #[test]
    fn rejects_symlink_pointing_inside_root() {
        let dir = make_root();
        let real = dir.path().join("real.txt");
        std::fs::write(&real, "data").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = resolve_within_root(dir.path(), "link.txt").unwrap_err();
        assert!(matches!(err, PathSafetyError::IsSymlink(_)));
    }

    #[test]
    fn accepts_curdir_segments() {
        // `./a.txt` — допустимо: CurDir-сегмент не нарушает изоляцию.
        let dir = make_root();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let p = resolve_within_root(dir.path(), "./a.txt").unwrap();
        assert!(p.ends_with("a.txt"));
    }

    #[test]
    fn rejects_root_that_does_not_exist() {
        let dir = make_root();
        let missing_root = dir.path().join("nope");
        let err = resolve_within_root(&missing_root, "x").unwrap_err();
        assert!(matches!(err, PathSafetyError::Io { .. }));
    }
}
