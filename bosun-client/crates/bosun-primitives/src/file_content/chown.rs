//! Резолвинг owner/group и установка через libc::chown.
//!
//! `unsafe`-блоки локализованы в этом модуле и оправданы тем, что FFI
//! `libc::getpwnam_r`/`libc::getgrnam_r`/`libc::chown` не имеют безопасной
//! обёртки в std. Везде проверяются errno, нулевые указатели и длина строк.

#![allow(unsafe_code)]

use std::ffi::{CString, NulError};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use bosun_core::PrimitiveError;

/// Резолвинг имени пользователя в uid через `getpwnam_r`.
///
/// Использует thread-safe reentrant вариант с буфером на стеке. Размер
/// буфера 4096 — хватает на большинство passwd-записей в Linux/Debian.
pub fn resolve_owner(name: &str) -> Result<u32, PrimitiveError> {
    let cname = CString::new(name).map_err(|e: NulError| {
        PrimitiveError::InvalidPayload(format!("owner '{name}' contains nul byte: {e}"))
    })?;

    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = [0_i8; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();

    // SAFETY: getpwnam_r — POSIX reentrant API. Принимает валидный CString
    // (нулём терминированный), указатель на out-параметр pwd, буфер с
    // известной длиной, и out-указатель result. На ошибку errno != 0 и
    // result == NULL — оба пути мы обрабатываем.
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            &mut result,
        )
    };

    if rc != 0 {
        return Err(PrimitiveError::InvalidPayload(format!(
            "getpwnam_r('{name}') failed: errno {rc}",
        )));
    }
    if result.is_null() {
        return Err(PrimitiveError::InvalidPayload(format!(
            "unknown user '{name}'",
        )));
    }
    Ok(pwd.pw_uid)
}

/// Резолвинг имени группы в gid через `getgrnam_r`.
pub fn resolve_group(name: &str) -> Result<u32, PrimitiveError> {
    let cname = CString::new(name).map_err(|e: NulError| {
        PrimitiveError::InvalidPayload(format!("group '{name}' contains nul byte: {e}"))
    })?;

    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = [0_i8; 4096];
    let mut result: *mut libc::group = std::ptr::null_mut();

    // SAFETY: см. resolve_owner, аналогичная POSIX-обёртка с теми же гарантиями.
    let rc = unsafe {
        libc::getgrnam_r(
            cname.as_ptr(),
            &mut grp,
            buf.as_mut_ptr().cast::<libc::c_char>(),
            buf.len(),
            &mut result,
        )
    };

    if rc != 0 {
        return Err(PrimitiveError::InvalidPayload(format!(
            "getgrnam_r('{name}') failed: errno {rc}",
        )));
    }
    if result.is_null() {
        return Err(PrimitiveError::InvalidPayload(format!(
            "unknown group '{name}'",
        )));
    }
    Ok(grp.gr_gid)
}

/// Сменить владельца/группу файла, если они отличаются от текущих.
///
/// - Если `want_uid`/`want_gid` совпадают с текущими — no-op (skip).
/// - Если отличаются и процесс не root (`!is_root`) — `ChownNotPermitted`.
/// - Иначе вызывает `libc::chown` и проверяет errno.
pub fn chown_if_needed(
    path: &Path,
    want_uid: u32,
    want_gid: u32,
    is_root: bool,
) -> Result<(), PrimitiveError> {
    let meta = std::fs::metadata(path).map_err(|e| PrimitiveError::Io {
        context: format!("stat {} for chown", path.display()),
        source: e,
    })?;
    let actual_uid = meta.uid();
    let actual_gid = meta.gid();

    if actual_uid == want_uid && actual_gid == want_gid {
        tracing::debug!(path = %path.display(), "chown unchanged, skipping");
        return Ok(());
    }

    if !is_root {
        let requested = format!("uid={want_uid} gid={want_gid}");
        let actual = format!("uid={actual_uid} gid={actual_gid}");
        tracing::warn!(
            path = %path.display(),
            requested = %requested,
            actual = %actual,
            "chown not permitted",
        );
        return Err(PrimitiveError::ChownNotPermitted { requested, actual });
    }

    let cpath = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e: NulError| {
        PrimitiveError::InvalidPayload(format!("path '{}' contains nul byte: {e}", path.display()))
    })?;

    // SAFETY: libc::chown принимает нулём терминированный путь (CString)
    // и числовые uid/gid. Возврат -1 — выставляется errno; в этой ветке
    // мы читаем io::Error::last_os_error и оборачиваем в PrimitiveError::Io.
    let rc = unsafe { libc::chown(cpath.as_ptr(), want_uid, want_gid) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(PrimitiveError::Io {
            context: format!("chown {} to uid={want_uid} gid={want_gid}", path.display(),),
            source: err,
        });
    }
    Ok(())
}

/// Текущий effective uid процесса. Используется для решения «root vs non-root»
/// без передачи флага явно — это упрощает тестирование (тест-окружение почти
/// всегда non-root, и `chown_if_needed` отдаст `ChownNotPermitted` сам).
pub fn current_euid() -> u32 {
    // SAFETY: libc::geteuid не имеет побочных эффектов и всегда возвращает
    // валидный uid процесса. Это POSIX, не failing call.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_owner_root_returns_zero() {
        // root есть на любом Linux/Debian — стабильный фикстурный uid 0.
        let uid = resolve_owner("root").unwrap();
        assert_eq!(uid, 0);
    }

    #[test]
    fn resolve_owner_unknown_user_is_error() {
        let err = resolve_owner("__bosun_nonexistent_user__").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown user"));
    }

    #[test]
    fn resolve_owner_nul_byte_in_name_is_error() {
        let err = resolve_owner("a\0b").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("nul byte"));
    }

    #[test]
    fn resolve_group_root_returns_zero() {
        let gid = resolve_group("root").unwrap();
        assert_eq!(gid, 0);
    }

    #[test]
    fn resolve_group_unknown_is_error() {
        let err = resolve_group("__bosun_nonexistent_group__").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown group"));
    }

    #[test]
    fn chown_if_needed_noop_when_match() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let meta = std::fs::metadata(tmp.path()).unwrap();
        // Запрашиваем текущие — должен быть no-op независимо от root.
        chown_if_needed(tmp.path(), meta.uid(), meta.gid(), false).unwrap();
    }

    #[test]
    fn chown_if_needed_non_root_error_when_diff() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Тест запускается под не-root и хочет сменить владельца на 0:0.
        let euid = current_euid();
        if euid == 0 {
            // Под root тест неинформативен — пропускаем.
            return;
        }
        let err = chown_if_needed(tmp.path(), 0, 0, false).unwrap_err();
        assert!(matches!(err, PrimitiveError::ChownNotPermitted { .. }));
    }
}
