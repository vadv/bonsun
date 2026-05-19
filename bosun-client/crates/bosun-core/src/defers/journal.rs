//! Persistent журнал defer-записей на tmpfs.
//!
//! Журнал — это директория с одиночными JSON-файлами; каждое отложенное
//! действие — отдельный файл. Атомарность операций гарантирована парой
//! `rename` + `fsync(dir)`: первое атомарно публикует файл с точки зрения
//! POSIX, второе фиксирует это переименование на диске (актуально при
//! `data=writeback` и kernel crash сразу после rename'а).
//!
//! Dedup правила реализуют chiit-семантику: restart субсумирует reload,
//! идемпотентная повторная вставка возвращает `AlreadyExists` без
//! перезаписи (content stable, проверено в `defers_test.go:36`).

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use super::format::{DeferAction, DeferEntry};

/// Ошибки журнала defers. Разделяются на I/O и логику парсинга, чтобы
/// caller мог различать «directory недоступна» и «файл повреждён».
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DeferError {
    /// Ошибка ввода-вывода с контекстом, где она произошла.
    #[error("defer journal i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Сериализация/десериализация JSON.
    #[error("defer journal serde error at {path}: {source}")]
    Serde {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// Время в системе ушло за epoch (это означает или сильную поломку
    /// часов, или баг конструирования `tmp.<nanos>` имени).
    #[error("system clock is before unix epoch")]
    Clock,
}

impl DeferError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        DeferError::Io {
            path: path.into(),
            source,
        }
    }
    fn serde(path: impl Into<PathBuf>, source: serde_json::Error) -> Self {
        DeferError::Serde {
            path: path.into(),
            source,
        }
    }
}

/// Результат `Journal::enqueue`. Различает случаи, когда insert
/// фактически создал файл, был no-op'ом (idempotent) и когда вытеснил
/// существующий файл с более низким приоритетом.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EnqueueResult {
    /// Файл создан с нуля — раньше такой записи не было.
    Created,
    /// Точно такая же запись уже есть — никаких изменений. Content stable.
    AlreadyExists,
    /// Новое действие субсумировано существующим (например, попытка
    /// вставить reload при наличии restart). No-op.
    Subsumed,
    /// Старая запись удалена и заменена новой с более высоким приоритетом.
    ReplacedLowerPriority,
}

/// Журнал defers — обёртка над директорией.
#[derive(Clone, Debug)]
pub struct Journal {
    root: PathBuf,
}

const DEFERRED_EXT: &str = "deferred";

impl Journal {
    /// Корневая директория журнала.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Открывает (или создаёт) журнал. На создании ставится `0o700`;
    /// если запуск под root, ядро автоматически проставит owner=root.
    /// Мы не делаем `chown` — он требует CAP_CHOWN и не нужен в неpriv
    /// тестах.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DeferError> {
        let root = path.as_ref().to_path_buf();
        if !root.exists() {
            fs::create_dir_all(&root).map_err(|e| DeferError::io(&root, e))?;
        }
        let metadata = fs::metadata(&root).map_err(|e| DeferError::io(&root, e))?;
        if !metadata.is_dir() {
            return Err(DeferError::io(
                &root,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "journal root is not a directory",
                ),
            ));
        }
        let mut perms = metadata.permissions();
        if perms.mode() & 0o777 != 0o700 {
            perms.set_mode(0o700);
            fs::set_permissions(&root, perms).map_err(|e| DeferError::io(&root, e))?;
        }
        Ok(Journal { root })
    }

    /// Полный путь к финальному файлу записи.
    fn final_path(&self, entry: &DeferEntry) -> PathBuf {
        self.root.join(entry.filename())
    }

    /// Полный путь к `.manual_clear` варианту записи.
    fn manual_clear_path(&self, entry: &DeferEntry) -> PathBuf {
        self.root.join(entry.manual_clear_filename())
    }

    /// Atomic write: tmp → fsync(file) → close → rename → fsync(dir).
    fn atomic_write(&self, entry: &DeferEntry) -> Result<(), DeferError> {
        let final_path = self.final_path(entry);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| DeferError::Clock)?
            .as_nanos();
        let tmp_name = format!("{}.tmp.{}", entry.filename(), nanos);
        let tmp_path = self.root.join(&tmp_name);

        // 1. open + 2. write + 3. fsync + 4. close.
        {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)
                .map_err(|e| DeferError::io(&tmp_path, e))?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer(&mut writer, entry)
                .map_err(|e| DeferError::serde(&tmp_path, e))?;
            // Финальный перевод строки полезен при `cat` журнала оператором.
            writer
                .write_all(b"\n")
                .map_err(|e| DeferError::io(&tmp_path, e))?;
            let file = writer
                .into_inner()
                .map_err(|err| DeferError::io(&tmp_path, err.into_error()))?;
            file.sync_all().map_err(|e| DeferError::io(&tmp_path, e))?;
            // file дропается здесь, fd закрывается до rename.
        }

        // 5. rename — POSIX atomic.
        fs::rename(&tmp_path, &final_path).map_err(|e| {
            // Если rename провалился — попытаемся подчистить tmp,
            // чтобы не оставлять мусор. Если и это упадёт — пробрасываем
            // оригинальную ошибку.
            let _ = fs::remove_file(&tmp_path);
            DeferError::io(&final_path, e)
        })?;

        // 6. fsync(dir) — фиксируем переименование в каталоге.
        fsync_dir(&self.root)?;
        Ok(())
    }

    /// Удаление файла записи с fsync(dir) после `unlink`.
    pub fn remove(&self, entry: &DeferEntry) -> Result<(), DeferError> {
        let path = self.final_path(entry);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(DeferError::io(&path, e)),
        }
        fsync_dir(&self.root)
    }

    /// Инкремент счётчика попыток с записью ошибки. Перезаписывает файл
    /// атомарно по тем же правилам, что и `enqueue`.
    pub fn bump_attempt(&self, entry: &DeferEntry, _err: &str) -> Result<DeferEntry, DeferError> {
        let mut next = entry.clone();
        next.attempt_count = next.attempt_count.saturating_add(1);
        self.atomic_write(&next)?;
        Ok(next)
    }

    /// Перевод defer'а в `.manual_clear` — renames без перезаписи содержимого.
    pub fn move_to_manual_clear(&self, entry: &DeferEntry) -> Result<(), DeferError> {
        let from = self.final_path(entry);
        let to = self.manual_clear_path(entry);
        fs::rename(&from, &to).map_err(|e| DeferError::io(&from, e))?;
        fsync_dir(&self.root)
    }

    /// Список всех валидных `.deferred` файлов, лексикографически отсортированных.
    /// Повреждённые JSON — `tracing::warn` + skip, не abort.
    pub fn list_sorted(&self) -> Result<Vec<DeferEntry>, DeferError> {
        let mut names: Vec<(String, PathBuf)> = Vec::new();
        let dir = fs::read_dir(&self.root).map_err(|e| DeferError::io(&self.root, e))?;
        for ent in dir {
            let ent = ent.map_err(|e| DeferError::io(&self.root, e))?;
            let path = ent.path();
            if path.extension().and_then(OsStr::to_str) != Some(DEFERRED_EXT) {
                continue;
            }
            let Some(name) = path.file_name().and_then(OsStr::to_str).map(str::to_owned) else {
                continue;
            };
            names.push((name, path));
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = Vec::with_capacity(names.len());
        for (_name, path) in names {
            match read_entry(&path) {
                Ok(entry) => out.push(entry),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping corrupt defer file");
                }
            }
        }
        Ok(out)
    }

    /// Постановка записи в журнал с применением dedup правил.
    pub fn enqueue(&self, entry: DeferEntry) -> Result<EnqueueResult, DeferError> {
        // Снимаем срез существующих записей для того же target+init_system.
        // Журнал маленький (десятки записей в худшем случае), полный
        // прогон допустим.
        let existing = self.list_sorted()?;
        let same_target: Vec<DeferEntry> = existing
            .into_iter()
            .filter(|e| e.init_system == entry.init_system && e.target == entry.target)
            .collect();

        // Шаг 1: точно такой же id уже есть → idempotent no-op.
        // Контент остаётся стабильным — повторная вставка не меняет
        // enqueued_at и не дублирует enqueued_by (chiit
        // `at_double_add_text_does_not_change`).
        if same_target.iter().any(|e| e.id == entry.id) {
            return Ok(EnqueueResult::AlreadyExists);
        }

        // Шаг 2: восходящая субсумация. Если уже есть запись с более
        // высоким приоритетом для того же target — новая no-op.
        // Семантика: restart субсумирует reload_or_restart и reload;
        // reload_or_restart субсумирует reload.
        if let Some(_winner) = same_target
            .iter()
            .find(|e| subsumes(&e.action, &entry.action))
        {
            return Ok(EnqueueResult::Subsumed);
        }

        // Шаг 3: нисходящая замена. Если новая запись доминирует над
        // существующими — удаляем их и записываем новую.
        let mut replaced_any = false;
        for old in &same_target {
            if subsumes(&entry.action, &old.action) {
                let old_path = self.final_path(old);
                match fs::remove_file(&old_path) {
                    Ok(()) => {
                        replaced_any = true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        // Race с параллельным процессом — допустимо.
                    }
                    Err(e) => return Err(DeferError::io(&old_path, e)),
                }
            }
        }
        if replaced_any {
            fsync_dir(&self.root)?;
        }

        self.atomic_write(&entry)?;
        if replaced_any {
            Ok(EnqueueResult::ReplacedLowerPriority)
        } else {
            Ok(EnqueueResult::Created)
        }
    }
}

/// Возвращает `true`, если действие `a` субсумирует `b` для одного и того
/// же target+init_system. Семантика:
/// - `Restart` субсумирует `Reload` и `ReloadOrRestart`.
/// - `ReloadOrRestart` субсумирует `Reload`.
///
/// Все остальные пары не субсумируют друг друга (включая разные
/// действия, не относящиеся к семейству reload/restart).
fn subsumes(a: &DeferAction, b: &DeferAction) -> bool {
    use DeferAction::*;
    matches!(
        (a, b),
        (Restart, Reload) | (Restart, ReloadOrRestart) | (ReloadOrRestart, Reload)
    )
}

/// Гарантирует, что каталог записан на диск. Открываем директорию по
/// чтению и зовём `sync_all` — это portable способ дать ядру команду
/// fsync на inode каталога.
fn fsync_dir(dir: &Path) -> Result<(), DeferError> {
    let f = File::open(dir).map_err(|e| DeferError::io(dir, e))?;
    f.sync_all().map_err(|e| DeferError::io(dir, e))
}

/// Чтение одной записи из JSON-файла.
fn read_entry(path: &Path) -> Result<DeferEntry, DeferError> {
    let bytes = fs::read(path).map_err(|e| DeferError::io(path, e))?;
    serde_json::from_slice(&bytes).map_err(|e| DeferError::serde(path, e))
}

/// Утилита для тестов: проверка, что в журнале нет залежавшихся `.tmp.*` файлов.
#[cfg(test)]
pub(crate) fn has_tmp_leftovers(root: &Path) -> bool {
    let Ok(dir) = fs::read_dir(root) else {
        return false;
    };
    for ent in dir.flatten() {
        if let Some(name) = ent.file_name().to_str() {
            if name.contains(".tmp.") {
                return true;
            }
        }
    }
    false
}

/// Утилита для тестов: подсчёт файлов с указанным расширением.
#[cfg(test)]
pub(crate) fn count_files_with_extension(root: &Path, ext: &str) -> usize {
    let Ok(dir) = fs::read_dir(root) else {
        return 0;
    };
    dir.flatten()
        .filter(|e| e.path().extension().and_then(OsStr::to_str) == Some(ext))
        .count()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::defers::format::{make_id, DeferAction, CURRENT_SPEC_VERSION};
    use crate::defers::priority::DeferPriority;
    use chrono::Utc;
    use tempfile::TempDir;

    fn make_entry(
        init_system: &str,
        action: DeferAction,
        target: &str,
        priority: DeferPriority,
    ) -> DeferEntry {
        DeferEntry {
            spec_version: CURRENT_SPEC_VERSION,
            id: make_id(init_system, &action, target),
            action,
            init_system: init_system.to_string(),
            target: target.to_string(),
            validate_cmd: None,
            health_check: None,
            priority,
            enqueued_at: Utc::now(),
            enqueued_by: vec![],
            attempt_count: 0,
            max_attempts: 3,
        }
    }

    fn open_in_tempdir() -> (TempDir, Journal) {
        let tmp = TempDir::new().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        (tmp, journal)
    }

    #[test]
    fn open_creates_directory_with_mode_0700() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("journal");
        let journal = Journal::open(&path).unwrap();
        assert!(path.is_dir());
        let mode = fs::metadata(journal.root()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn open_tightens_existing_loose_permissions() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("journal");
        fs::create_dir_all(&path).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        let _journal = Journal::open(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn enqueue_creates_exactly_one_deferred_file_no_tmp_leftovers() {
        let (tmp, journal) = open_in_tempdir();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );
        let result = journal.enqueue(entry.clone()).unwrap();
        assert_eq!(result, EnqueueResult::Created);
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        assert!(!has_tmp_leftovers(tmp.path()));
        let final_path = tmp.path().join("0r-systemd.restart:nginx.deferred");
        assert!(final_path.exists());
        // Permissions для tmp-файла были 0600; rename сохранит mode.
        let mode = fs::metadata(&final_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn enqueue_dedup_reload_then_restart_keeps_only_restart() {
        let (tmp, journal) = open_in_tempdir();
        let reload = make_entry(
            "systemd",
            DeferAction::Reload,
            "nginx",
            DeferPriority::Reload,
        );
        let restart = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );

        assert_eq!(journal.enqueue(reload).unwrap(), EnqueueResult::Created);
        let r = journal.enqueue(restart).unwrap();
        assert_eq!(r, EnqueueResult::ReplacedLowerPriority);

        let files: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "0r-systemd.restart:nginx.deferred");
    }

    #[test]
    fn enqueue_dedup_restart_then_reload_is_noop() {
        let (tmp, journal) = open_in_tempdir();
        let restart = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );
        let reload = make_entry(
            "systemd",
            DeferAction::Reload,
            "nginx",
            DeferPriority::Reload,
        );

        assert_eq!(journal.enqueue(restart).unwrap(), EnqueueResult::Created);
        let r = journal.enqueue(reload).unwrap();
        assert_eq!(r, EnqueueResult::Subsumed);
        let files: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "0r-systemd.restart:nginx.deferred");
    }

    #[test]
    fn enqueue_dedup_reload_or_restart_subsumed_by_restart() {
        let (_tmp, journal) = open_in_tempdir();
        let restart = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );
        let ror = make_entry(
            "systemd",
            DeferAction::ReloadOrRestart,
            "nginx",
            DeferPriority::ReloadOrRestart,
        );

        journal.enqueue(restart).unwrap();
        let r = journal.enqueue(ror).unwrap();
        assert_eq!(r, EnqueueResult::Subsumed);
    }

    #[test]
    fn enqueue_dedup_reload_or_restart_replaces_reload() {
        let (tmp, journal) = open_in_tempdir();
        let reload = make_entry(
            "systemd",
            DeferAction::Reload,
            "nginx",
            DeferPriority::Reload,
        );
        let ror = make_entry(
            "systemd",
            DeferAction::ReloadOrRestart,
            "nginx",
            DeferPriority::ReloadOrRestart,
        );

        journal.enqueue(reload).unwrap();
        let r = journal.enqueue(ror).unwrap();
        assert_eq!(r, EnqueueResult::ReplacedLowerPriority);
        let files: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "1r-systemd.reload_or_restart:nginx.deferred");
    }

    #[test]
    fn enqueue_dedup_restart_replaces_reload_or_restart() {
        let (tmp, journal) = open_in_tempdir();
        let ror = make_entry(
            "systemd",
            DeferAction::ReloadOrRestart,
            "nginx",
            DeferPriority::ReloadOrRestart,
        );
        let restart = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );

        journal.enqueue(ror).unwrap();
        let r = journal.enqueue(restart).unwrap();
        assert_eq!(r, EnqueueResult::ReplacedLowerPriority);
        let files: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "0r-systemd.restart:nginx.deferred");
    }

    #[test]
    fn enqueue_same_target_different_init_system_not_deduped() {
        let (_tmp, journal) = open_in_tempdir();
        let systemd_restart = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );
        let runr_restart = make_entry(
            "runr",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );
        journal.enqueue(systemd_restart).unwrap();
        journal.enqueue(runr_restart).unwrap();
        assert_eq!(journal.list_sorted().unwrap().len(), 2);
    }

    #[test]
    fn idempotent_reenqueue_keeps_content_stable() {
        let (_tmp, journal) = open_in_tempdir();
        let mut entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "nginx",
            DeferPriority::Restart,
        );
        entry.enqueued_by = vec!["file.content:/etc/nginx/nginx.conf".to_string()];
        entry.enqueued_at = Utc::now();
        journal.enqueue(entry.clone()).unwrap();
        let first_bytes = fs::read(journal.final_path(&entry)).unwrap();

        // Вторая попытка вставить с тем же id — но с другим enqueued_at
        // и другим enqueued_by. Должна вернуть AlreadyExists и оставить
        // содержимое нетронутым.
        let mut second = entry.clone();
        second.enqueued_at = Utc::now() + chrono::Duration::seconds(60);
        second.enqueued_by = vec!["file.content:/etc/other.conf".to_string()];
        let r = journal.enqueue(second).unwrap();
        assert_eq!(r, EnqueueResult::AlreadyExists);
        let second_bytes = fs::read(journal.final_path(&entry)).unwrap();
        assert_eq!(first_bytes, second_bytes);
    }

    #[test]
    fn list_sorted_returns_files_in_priority_order() {
        let (_tmp, journal) = open_in_tempdir();
        let reload = make_entry(
            "systemd",
            DeferAction::Reload,
            "z-svc",
            DeferPriority::Reload,
        );
        let ror = make_entry(
            "systemd",
            DeferAction::ReloadOrRestart,
            "y-svc",
            DeferPriority::ReloadOrRestart,
        );
        let restart = make_entry(
            "systemd",
            DeferAction::Restart,
            "x-svc",
            DeferPriority::Restart,
        );
        let command = make_entry(
            "",
            DeferAction::Command {
                argv: vec!["echo".into()],
            },
            "echo",
            DeferPriority::Command,
        );

        journal.enqueue(reload).unwrap();
        journal.enqueue(ror).unwrap();
        journal.enqueue(restart).unwrap();
        journal.enqueue(command).unwrap();

        let list = journal.list_sorted().unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].priority, DeferPriority::Restart);
        assert_eq!(list[1].priority, DeferPriority::ReloadOrRestart);
        assert_eq!(list[2].priority, DeferPriority::Reload);
        assert_eq!(list[3].priority, DeferPriority::Command);
    }

    #[test]
    fn list_sorted_skips_corrupt_files() {
        let (tmp, journal) = open_in_tempdir();
        let good = make_entry(
            "systemd",
            DeferAction::Restart,
            "good",
            DeferPriority::Restart,
        );
        journal.enqueue(good.clone()).unwrap();
        // Подкладываем повреждённый файл с правильным расширением.
        let bad_path = tmp.path().join("0r-systemd.restart:bad.deferred");
        fs::write(&bad_path, b"not valid json {{{").unwrap();

        let list = journal.list_sorted().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, good.id);
    }

    #[test]
    fn list_sorted_ignores_tmp_and_manual_clear() {
        let (tmp, journal) = open_in_tempdir();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "svc",
            DeferPriority::Restart,
        );
        journal.enqueue(entry.clone()).unwrap();
        // Подкладываем мусор.
        fs::write(
            tmp.path().join("0r-systemd.restart:other.deferred.tmp.123"),
            b"{}",
        )
        .unwrap();
        fs::write(
            tmp.path().join("0r-systemd.restart:stale.manual_clear"),
            b"{}",
        )
        .unwrap();
        let list = journal.list_sorted().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, entry.id);
    }

    #[test]
    fn remove_deletes_file() {
        let (tmp, journal) = open_in_tempdir();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "svc",
            DeferPriority::Restart,
        );
        journal.enqueue(entry.clone()).unwrap();
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 1);
        journal.remove(&entry).unwrap();
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
    }

    #[test]
    fn remove_missing_file_is_ok() {
        let (_tmp, journal) = open_in_tempdir();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "ghost",
            DeferPriority::Restart,
        );
        journal.remove(&entry).unwrap();
    }

    #[test]
    fn bump_attempt_increments_counter_and_rewrites_file() {
        let (_tmp, journal) = open_in_tempdir();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "svc",
            DeferPriority::Restart,
        );
        journal.enqueue(entry.clone()).unwrap();
        let updated = journal.bump_attempt(&entry, "transient").unwrap();
        assert_eq!(updated.attempt_count, 1);
        let list = journal.list_sorted().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].attempt_count, 1);
    }

    #[test]
    fn move_to_manual_clear_renames_extension() {
        let (tmp, journal) = open_in_tempdir();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "svc",
            DeferPriority::Restart,
        );
        journal.enqueue(entry.clone()).unwrap();
        journal.move_to_manual_clear(&entry).unwrap();
        assert_eq!(count_files_with_extension(tmp.path(), "deferred"), 0);
        assert_eq!(count_files_with_extension(tmp.path(), "manual_clear"), 1);
    }

    #[test]
    fn fsync_dir_called_via_manual_unlink_after_rename() {
        // Симуляция «прерывания между rename и fsync(dir)»: после rename
        // сторонний процесс удаляет файл. list_sorted не должен его видеть.
        // Это покрывает требование acceptance-criteria: атомарность с
        // точки зрения видимости. Реальный fsync(dir) внутри enqueue
        // нечем имитировать в user-space, но мы проверяем, что
        // последующий unlink+fsync_dir не оставляет залежей.
        let (tmp, journal) = open_in_tempdir();
        let entry = make_entry(
            "systemd",
            DeferAction::Restart,
            "svc",
            DeferPriority::Restart,
        );
        journal.enqueue(entry.clone()).unwrap();
        // Внешний unlink — симулирует, что crash kernel'я мог не сохранить
        // rename. После него list_sorted пуст.
        fs::remove_file(journal.final_path(&entry)).unwrap();
        fsync_dir(tmp.path()).unwrap();
        let list = journal.list_sorted().unwrap();
        assert!(list.is_empty());
        assert!(!has_tmp_leftovers(tmp.path()));
    }
}
