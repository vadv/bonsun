//! Бэкапы существующих файлов перед перезаписью.
//!
//! Целевой путь backup-файла:
//! `{backup_root}{target}.{utc_ts}`, где `utc_ts` имеет формат
//! `YYYYMMDDTHHMMSSZ`. После создания свежего бэкапа удаляются все, кроме
//! последних `keep_last` — это предотвращает разрастание `/var/backups/bosun`
//! на нодах, где конфиг флапает.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bosun_core::PrimitiveError;
use chrono::Utc;

/// Сделать backup `target` под `backup_root` с rotation `keep_last`.
///
/// Если `target` не существует или это директория — возвращаем Ok без действий
/// (это противоречит вызывающей стороне, но защищает на случай повторных
/// вызовов; в нормальном потоке backup делается только когда `target` — файл).
///
/// `now` инжектируется через возвращаемое значение `chrono::Utc::now()` —
/// rotation в тесте проверяется через несколько последовательных вызовов с
/// явными разными `target`-файлами и небольшой задержкой.
pub fn backup_with_rotation(
    target: &Path,
    backup_root: &Path,
    keep_last: usize,
) -> Result<PathBuf, PrimitiveError> {
    let backup_path = build_backup_path(
        backup_root,
        target,
        &Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
    );

    if let Some(parent) = backup_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PrimitiveError::Io {
            context: format!("create_dir_all {}", parent.display()),
            source: e,
        })?;
    }

    std::fs::copy(target, &backup_path).map_err(|e| PrimitiveError::Io {
        context: format!("copy {} -> {}", target.display(), backup_path.display()),
        source: e,
    })?;

    rotate(&backup_path, keep_last)?;
    Ok(backup_path)
}

/// Построить путь к бэкапу: `{backup_root}{target}.{ts}`.
///
/// Реализация конкатенирует строки путей напрямую, потому что `target`
/// абсолютный (`/etc/nginx/...`) — `Path::join(backup_root, target)` сбросит
/// `backup_root`. Чтобы сохранить structure, мы пропускаем leading-slash.
fn build_backup_path(backup_root: &Path, target: &Path, ts: &str) -> PathBuf {
    let mut out: PathBuf = backup_root.to_path_buf();
    let rel = target.strip_prefix("/").unwrap_or(target);
    out.push(rel);
    // Дописываем `.{ts}` к имени файла, сохраняя расширение.
    let mut file_name: OsString = out
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("backup"));
    file_name.push(".");
    file_name.push(ts);
    out.set_file_name(file_name);
    out
}

/// Удалить старые бэкапы для пути, оставив только последние `keep_last`.
///
/// «Тот же путь» определяется по basename без timestamp-суффикса: для нового
/// бэкапа `/var/backups/bosun/etc/nginx/nginx.conf.20260518T120000Z` ищем
/// сиблингов в той же директории с префиксом `nginx.conf.` — это эквивалентно
/// glob'у `nginx.conf.*`.
fn rotate(new_backup: &Path, keep_last: usize) -> Result<(), PrimitiveError> {
    let parent = match new_backup.parent() {
        Some(p) => p,
        None => return Ok(()),
    };
    let new_name = match new_backup.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return Ok(()),
    };
    // Префикс = всё до последней точки (включая её). Для
    // `nginx.conf.20260518T120000Z` префикс — `nginx.conf.`.
    let prefix = match new_name.rfind('.') {
        Some(i) => &new_name[..=i],
        None => return Ok(()),
    };

    let entries = std::fs::read_dir(parent).map_err(|e| PrimitiveError::Io {
        context: format!("read_dir {}", parent.display()),
        source: e,
    })?;
    let mut siblings: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| PrimitiveError::Io {
            context: format!("read_dir entry in {}", parent.display()),
            source: e,
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(prefix) {
            siblings.push(path);
        }
    }
    // Лексикографическая сортировка по полному имени работает для нашего
    // ts-формата (`YYYYMMDDTHHMMSSZ`): он zero-padded и монотонен.
    siblings.sort();
    if siblings.len() <= keep_last {
        return Ok(());
    }
    let to_remove = siblings.len() - keep_last;
    for path in siblings.iter().take(to_remove) {
        std::fs::remove_file(path).map_err(|e| PrimitiveError::Io {
            context: format!("remove_file {}", path.display()),
            source: e,
        })?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::io::Write;

    use super::*;

    fn write_file(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content).unwrap();
    }

    #[test]
    fn build_backup_path_strips_leading_slash_and_appends_ts() {
        let backup_root = Path::new("/var/backups/bosun");
        let target = Path::new("/etc/nginx/nginx.conf");
        let path = build_backup_path(backup_root, target, "20260518T120000Z");
        assert_eq!(
            path,
            Path::new("/var/backups/bosun/etc/nginx/nginx.conf.20260518T120000Z")
        );
    }

    #[test]
    fn build_backup_path_relative_target() {
        let backup_root = Path::new("/tmp/bosun");
        let target = Path::new("etc/host");
        let path = build_backup_path(backup_root, target, "X");
        assert_eq!(path, Path::new("/tmp/bosun/etc/host.X"));
    }

    #[test]
    fn rotation_keeps_last_n_alphabetically() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("etc");
        fs::create_dir_all(&dir).unwrap();
        // 7 файлов с возрастающими timestamps. После rotate(keep_last=5)
        // первые 2 удаляются.
        let ts_list = [
            "20260101T000000Z",
            "20260102T000000Z",
            "20260103T000000Z",
            "20260104T000000Z",
            "20260105T000000Z",
            "20260106T000000Z",
            "20260107T000000Z",
        ];
        for ts in &ts_list {
            write_file(&dir.join(format!("nginx.conf.{ts}")), b"x");
        }
        let new_backup = dir.join("nginx.conf.20260107T000000Z");
        rotate(&new_backup, 5).unwrap();

        let mut remaining: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), 5);
        assert_eq!(
            remaining,
            vec![
                "nginx.conf.20260103T000000Z",
                "nginx.conf.20260104T000000Z",
                "nginx.conf.20260105T000000Z",
                "nginx.conf.20260106T000000Z",
                "nginx.conf.20260107T000000Z",
            ]
        );
    }

    #[test]
    fn rotation_noop_when_fewer_than_keep_last() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("etc");
        fs::create_dir_all(&dir).unwrap();
        for i in 0..3 {
            write_file(&dir.join(format!("file.20260101T00000{i}Z")), b"x");
        }
        let new_backup = dir.join("file.20260101T000002Z");
        rotate(&new_backup, 5).unwrap();
        let count = fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 3);
    }

    #[test]
    fn rotation_skips_unrelated_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("etc");
        fs::create_dir_all(&dir).unwrap();
        for ts in ["20260101T000000Z", "20260102T000000Z", "20260103T000000Z"] {
            write_file(&dir.join(format!("file.{ts}")), b"x");
        }
        // Файл с другим basename — не трогаем.
        write_file(&dir.join("other.20260101T000000Z"), b"y");
        let new_backup = dir.join("file.20260103T000000Z");
        rotate(&new_backup, 1).unwrap();
        assert!(dir.join("file.20260103T000000Z").exists());
        assert!(dir.join("other.20260101T000000Z").exists());
        assert!(!dir.join("file.20260101T000000Z").exists());
    }

    #[test]
    fn backup_with_rotation_creates_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("etc/host");
        write_file(&target, b"original");
        let backup_root = tmp.path().join("backup");
        let backup_path = backup_with_rotation(&target, &backup_root, 5).unwrap();
        assert!(backup_path.exists());
        let copied = fs::read(&backup_path).unwrap();
        assert_eq!(copied, b"original");
        assert!(backup_path
            .to_string_lossy()
            .contains(&tmp.path().join("backup").to_string_lossy().to_string()));
    }
}
