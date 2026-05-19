//! Apply-фаза `apt.update_cache`.
//!
//! Поток:
//! 1. NoChange → early return.
//! 2. Re-check mtime через `read_pkgcache_age` + `decide_action`: если за
//!    время между plan и apply кеш стал свежим (соседний bosun-цикл уже
//!    отработал), отдадим NoChange. Это и есть read-before-write.
//! 3. Probe `/var/lib/dpkg/lock-frontend` — если apt уже занят, отдаём
//!    `DpkgLocked` (deferrable, следующий цикл попробует снова).
//! 4. `apt-get update` через `AptCacheBackend::update`.
//! 5. Если `skip_cleanup=false` и cleanup не упал — лучшее усилие
//!    `cleanup_old_debs`. Сбой cleanup не валит apply: cache уже обновлён,
//!    оператор увидит warning, но bundle convergence не пострадает.

use std::path::{Path, PathBuf};

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::plan::{decide_action, read_pkgcache_age, Action, PKGCACHE_PATH};
use super::spec::AptUpdateCacheSpec;
use crate::apt_package::lock_probe::probe_dpkg_lock;

/// Стандартный путь к dpkg lock-frontend. В spec'е не настраиваем — apt
/// hardcoded'ит этот путь, переопределение возможно только в тестах.
const DPKG_LOCK_PATH: &str = "/var/lib/dpkg/lock-frontend";

/// Стандартный путь к директории с скачанными `.deb`.
const APT_ARCHIVES_DIR: &str = "/var/cache/apt/archives";

/// Контракт исполнителя apt-кеш-операций. DI-точка для тестов: production
/// использует `RealAptCacheBackend` (вызов `apt-get update` и `find -delete`),
/// в тестах — recorder без побочных эффектов.
pub trait AptCacheBackend: Send + Sync {
    /// Запустить `apt-get update` с дедлайном из ctx и проверкой cancel.
    /// Возвращает `Ok(())` на exit=0, иначе `PrimitiveError::Exec`.
    fn update(&self, ctx: &ApplyCtx) -> Result<(), PrimitiveError>;

    /// Удалить из `archives_dir` все `.deb` старше `older_than_days` дней.
    /// Best-effort: ошибки маппятся в `Err(reason)`, caller сам решает,
    /// валить ли apply (по умолчанию мы только логируем).
    fn cleanup_old_debs(&self, archives_dir: &Path, older_than_days: u32) -> Result<usize, String>;
}

/// Production-реализация поверх `apt-package::exec::RealCommandRunner`.
///
/// Сама команда `apt-get update` запускается с `-q -y` и таймаутом из
/// `ctx.deadline`. Cleanup `.deb`-файлов — через прямой обход
/// `std::fs::read_dir` + `modified()`, без shell'а: вызов `find ... -delete`
/// требовал бы spawn'ить `find`, а это лишняя зависимость на coreutils-find,
/// плюс защитный аргумент: ходим только по `*.deb`-файлам в одной
/// директории, без рекурсии в symlink-фермы.
pub struct RealAptCacheBackend;

impl AptCacheBackend for RealAptCacheBackend {
    fn update(&self, ctx: &ApplyCtx) -> Result<(), PrimitiveError> {
        use crate::apt_package::exec::{CommandRunner, RealCommandRunner};

        let runner = RealCommandRunner;
        let result = runner.run(
            "apt-get",
            &[
                "update",
                "-qy",
                "-oDPkg::Lock::Timeout=30",
                "-oAPT::Acquire::Retries=3",
            ],
            ctx.deadline,
            &ctx.cancel,
        )?;
        if result.exit_code == Some(0) {
            return Ok(());
        }
        let excerpt = if result.stderr.len() > 512 {
            format!("{}…", &result.stderr[..512])
        } else {
            result.stderr.clone()
        };
        Err(PrimitiveError::Exec {
            reason: "apt-get update failed".to_string(),
            exit: result.exit_code,
            stderr_excerpt: excerpt,
        })
    }

    fn cleanup_old_debs(&self, archives_dir: &Path, older_than_days: u32) -> Result<usize, String> {
        cleanup_old_debs_impl(archives_dir, older_than_days)
    }
}

/// Реализация cleanup'а, выделена в свободную функцию для unit-тестов.
///
/// Семантика: бежим по entries в `archives_dir` и в его подкаталоге
/// `partial/`, оставляем только обычные файлы с `.deb`-расширением,
/// проверяем `modified()` и удаляем, если возраст ≥ `older_than_days *
/// 86400`. Возвращает количество удалённых файлов. Если ни одной из
/// директорий нет — возвращает 0 без ошибки. Глубже, чем `partial/`,
/// не идём: apt больше нигде в archives/ не хранит deb-файлы.
pub(crate) fn cleanup_old_debs_impl(
    archives_dir: &Path,
    older_than_days: u32,
) -> Result<usize, String> {
    let threshold_secs = u64::from(older_than_days) * 86_400;
    let now = std::time::SystemTime::now();
    let mut removed = 0_usize;

    // archives/ и archives/partial/ — оба места, где apt держит .deb-файлы.
    // partial/ заполняется при прерванной apt-get install (network drop,
    // диск кончился), и без cleanup'а копится бесконечно.
    for dir in [archives_dir.to_path_buf(), archives_dir.join("partial")] {
        cleanup_old_debs_in_dir(&dir, threshold_secs, now, &mut removed)?;
    }

    Ok(removed)
}

/// Однотировой проход по одной директории. Выделено, чтобы вызывать дважды
/// (archives и archives/partial) без копипасты и без рекурсии.
fn cleanup_old_debs_in_dir(
    dir: &Path,
    threshold_secs: u64,
    now: std::time::SystemTime,
    removed: &mut usize,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read_dir {}: {e}", dir.display())),
    };

    for entry in entries {
        let entry = entry.map_err(|e| format!("readdir iter: {e}"))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("deb") {
            continue;
        }
        let metadata = match path.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let mtime = match metadata.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age_secs = match now.duration_since(mtime) {
            Ok(d) => d.as_secs(),
            // mtime в будущем — пропускаем, не наше дело подменять часы.
            Err(_) => continue,
        };
        if age_secs >= threshold_secs && std::fs::remove_file(&path).is_ok() {
            *removed += 1;
        }
    }

    Ok(())
}

/// Главная функция apply.
pub fn run(
    backend: &dyn AptCacheBackend,
    pkgcache_path: &Path,
    archives_dir: &Path,
    dpkg_lock_path: &Path,
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: AptUpdateCacheSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.update_cache payload: {e}")))?;

    // Cancel/deadline check — до probe и до spawn.
    if ctx.cancelled_or_past_deadline() {
        return Err(PrimitiveError::Cancelled);
    }

    // Re-check: соседний bosun-цикл (или ручной `apt-get update` оператора)
    // мог обновить кеш между plan и apply. Сейчас mtime может оказаться
    // свежим — отдадим NoChange без spawn'а apt-get.
    let age = match read_pkgcache_age(pkgcache_path) {
        Ok(a) => a,
        Err(reason) => {
            // I/O сбой кроме NotFound: лучше попробовать update, чем
            // упасть; reason пишем в trace для диагностики.
            tracing::warn!(
                resource = %spec.name,
                reason = %reason,
                "apt.update_cache: pkgcache mtime read failed, proceeding to refresh",
            );
            None
        }
    };

    if let Action::Fresh { age_sec } = decide_action(age, &spec) {
        tracing::info!(
            resource = %spec.name,
            age_sec,
            "apt.update_cache: cache fresh on re-check, skipping",
        );
        return Ok(ChangeReport::no_change());
    }

    // dpkg-lock probe — если apt-get install/unattended-upgrades держат
    // lock, отдаём `DpkgLocked` (deferrable). Следующий тик попробует снова.
    probe_dpkg_lock(dpkg_lock_path)?;

    tracing::info!(
        resource = %spec.name,
        force = spec.force,
        max_age_sec = spec.max_age_sec,
        "apt.update_cache: running apt-get update",
    );

    backend.update(ctx)?;

    if !spec.skip_cleanup {
        match backend.cleanup_old_debs(archives_dir, spec.cleanup_old_debs_days) {
            Ok(n) if n > 0 => {
                tracing::info!(
                    resource = %spec.name,
                    removed = n,
                    days = spec.cleanup_old_debs_days,
                    "apt.update_cache: removed old .deb files",
                );
            }
            Ok(_) => {}
            Err(reason) => {
                tracing::warn!(
                    resource = %spec.name,
                    reason = %reason,
                    "apt.update_cache: cleanup_old_debs failed, ignoring",
                );
            }
        }
    }

    Ok(ChangeReport::changed(format!(
        "apt-get update succeeded (resource {})",
        spec.name
    )))
}

/// Конкретный путь к pkgcache.bin как PathBuf — production константа,
/// поднимаем в функцию для использования в new()-конструкторе примитива.
pub(crate) fn default_pkgcache_path() -> PathBuf {
    PathBuf::from(PKGCACHE_PATH)
}

/// Конкретный путь к archives/ — аналогично.
pub(crate) fn default_archives_dir() -> PathBuf {
    PathBuf::from(APT_ARCHIVES_DIR)
}

/// Конкретный путь к dpkg lock — аналогично.
pub(crate) fn default_dpkg_lock_path() -> PathBuf {
    PathBuf::from(DPKG_LOCK_PATH)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtx, ResourceId, ResourceKind, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// Mock backend: записывает вызовы update/cleanup, возвращает заранее
    /// заданные результаты.
    struct MockBackend {
        update_result: Result<(), String>,
        cleanup_result: Result<usize, String>,
        calls: Mutex<Calls>,
    }

    #[derive(Default)]
    struct Calls {
        update_called: usize,
        cleanup_calls: Vec<(PathBuf, u32)>,
    }

    impl MockBackend {
        fn ok() -> Self {
            Self {
                update_result: Ok(()),
                cleanup_result: Ok(0),
                calls: Mutex::new(Calls::default()),
            }
        }
        fn update_fail(reason: &str) -> Self {
            Self {
                update_result: Err(reason.to_string()),
                cleanup_result: Ok(0),
                calls: Mutex::new(Calls::default()),
            }
        }
        fn with_cleanup_count(n: usize) -> Self {
            Self {
                update_result: Ok(()),
                cleanup_result: Ok(n),
                calls: Mutex::new(Calls::default()),
            }
        }
    }

    impl AptCacheBackend for MockBackend {
        fn update(&self, _ctx: &ApplyCtx) -> Result<(), PrimitiveError> {
            self.calls.lock().unwrap().update_called += 1;
            match &self.update_result {
                Ok(()) => Ok(()),
                Err(reason) => Err(PrimitiveError::Exec {
                    reason: reason.clone(),
                    exit: Some(100),
                    stderr_excerpt: reason.clone(),
                }),
            }
        }
        fn cleanup_old_debs(
            &self,
            archives_dir: &Path,
            older_than_days: u32,
        ) -> Result<usize, String> {
            self.calls
                .lock()
                .unwrap()
                .cleanup_calls
                .push((archives_dir.to_path_buf(), older_than_days));
            self.cleanup_result.clone()
        }
    }

    fn make_ctx() -> (TempDir, ApplyCtx) {
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp/backup"),
            PathBuf::from("/tmp/log"),
            defers,
            None,
            None,
        );
        (tmp, ctx)
    }

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("apt.update_cache");
        let id = ResourceId::new(&kind, "test");
        Resource {
            id,
            kind,
            spec_version: 1,
            payload,
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn free_lock_path() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dpkg-lock-frontend");
        std::fs::write(&path, "").unwrap();
        (dir, path)
    }

    fn update_diff() -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "force".into(),
        }
    }

    #[test]
    fn run_no_change_diff_returns_early_no_calls() {
        let backend = MockBackend::ok();
        let r = make_resource(serde_json::json!({ "name": "x" }));
        let (_tmp, ctx) = make_ctx();
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        let archives_dir = tempfile::tempdir().unwrap();

        let report = run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &Diff::NoChange,
            &ctx,
        )
        .unwrap();
        assert!(!report.changed);
        assert_eq!(backend.calls.lock().unwrap().update_called, 0);
    }

    #[test]
    fn run_force_invokes_update() {
        let backend = MockBackend::ok();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "force": true,
            "skip_cleanup": true,
        }));
        let (_tmp, ctx) = make_ctx();
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        let archives_dir = tempfile::tempdir().unwrap();

        let report = run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &update_diff(),
            &ctx,
        )
        .unwrap();
        assert!(report.changed);
        assert_eq!(backend.calls.lock().unwrap().update_called, 1);
        // skip_cleanup → cleanup_calls пустой.
        assert!(backend.calls.lock().unwrap().cleanup_calls.is_empty());
    }

    #[test]
    fn run_recheck_skips_update_when_cache_became_fresh() {
        // Имитируем «соседний bosun-цикл обновил кеш между plan и apply»:
        // pkgcache.bin создан только что (age < max_age_sec), force=false.
        let backend = MockBackend::ok();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "max_age_sec": 3600_u32,
            "force": false,
            "skip_cleanup": true,
        }));
        let (_tmp, ctx) = make_ctx();
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        std::fs::write(&pkgcache_path, "").unwrap();
        let archives_dir = tempfile::tempdir().unwrap();

        let report = run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &update_diff(),
            &ctx,
        )
        .unwrap();
        assert!(!report.changed, "re-check should yield NoChange");
        assert_eq!(backend.calls.lock().unwrap().update_called, 0);
    }

    #[test]
    fn run_force_with_skip_cleanup_does_not_call_cleanup() {
        let backend = MockBackend::with_cleanup_count(5);
        let r = make_resource(serde_json::json!({
            "name": "x",
            "force": true,
            "skip_cleanup": true,
        }));
        let (_tmp, ctx) = make_ctx();
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        let archives_dir = tempfile::tempdir().unwrap();

        run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &update_diff(),
            &ctx,
        )
        .unwrap();
        assert!(backend.calls.lock().unwrap().cleanup_calls.is_empty());
    }

    #[test]
    fn run_force_default_cleanup_invoked() {
        let backend = MockBackend::with_cleanup_count(2);
        let r = make_resource(serde_json::json!({
            "name": "x",
            "force": true,
            "cleanup_old_debs_days": 7_u32,
        }));
        let (_tmp, ctx) = make_ctx();
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        let archives_dir = tempfile::tempdir().unwrap();

        run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &update_diff(),
            &ctx,
        )
        .unwrap();
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.cleanup_calls.len(), 1);
        assert_eq!(calls.cleanup_calls[0].0, archives_dir.path());
        assert_eq!(calls.cleanup_calls[0].1, 7);
    }

    #[test]
    fn run_update_failure_returns_exec_error_and_skips_cleanup() {
        let backend = MockBackend::update_fail("network error");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "force": true,
        }));
        let (_tmp, ctx) = make_ctx();
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        let archives_dir = tempfile::tempdir().unwrap();

        let err = run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &update_diff(),
            &ctx,
        )
        .unwrap_err();
        assert!(matches!(err, PrimitiveError::Exec { .. }));
        // Cleanup не вызывается при провале update'а.
        assert!(backend.calls.lock().unwrap().cleanup_calls.is_empty());
    }

    #[test]
    fn run_cleanup_failure_is_swallowed() {
        // Cleanup упал, update прошёл — apply должен вернуть Changed,
        // оператор увидит warning в логах, но bundle convergence не сломан.
        struct CleanupFailBackend;
        impl AptCacheBackend for CleanupFailBackend {
            fn update(&self, _ctx: &ApplyCtx) -> Result<(), PrimitiveError> {
                Ok(())
            }
            fn cleanup_old_debs(
                &self,
                _archives_dir: &Path,
                _older_than_days: u32,
            ) -> Result<usize, String> {
                Err("permission denied".to_string())
            }
        }
        let backend = CleanupFailBackend;
        let r = make_resource(serde_json::json!({
            "name": "x",
            "force": true,
        }));
        let (_tmp, ctx) = make_ctx();
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        let archives_dir = tempfile::tempdir().unwrap();

        let report = run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &update_diff(),
            &ctx,
        )
        .unwrap();
        assert!(report.changed);
    }

    #[test]
    fn run_cancelled_returns_cancelled_no_update() {
        let backend = MockBackend::ok();
        let r = make_resource(serde_json::json!({ "name": "x", "force": true }));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let tmp = TempDir::new().unwrap();
        let defers = Arc::new(Journal::open(tmp.path()).unwrap());
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(60),
            cancel,
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
            defers,
            None,
            None,
        );
        let (_d, lock_path) = free_lock_path();
        let pkgcache_dir = tempfile::tempdir().unwrap();
        let pkgcache_path = pkgcache_dir.path().join("pkgcache.bin");
        let archives_dir = tempfile::tempdir().unwrap();

        let err = run(
            &backend,
            &pkgcache_path,
            archives_dir.path(),
            &lock_path,
            &r,
            &update_diff(),
            &ctx,
        )
        .unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
        assert_eq!(backend.calls.lock().unwrap().update_called, 0);
    }

    #[test]
    fn cleanup_old_debs_removes_old_files_keeps_fresh() {
        let dir = tempfile::tempdir().unwrap();
        // Свежий .deb — оставить.
        let fresh = dir.path().join("fresh.deb");
        std::fs::write(&fresh, "fresh").unwrap();

        // Старый .deb — выставим mtime в прошлое (10 дней назад).
        let old = dir.path().join("old.deb");
        std::fs::write(&old, "old").unwrap();
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 86_400);
        set_mtime(&old, ten_days_ago);

        // Старый, но не .deb — не трогаем.
        let other = dir.path().join("readme.txt");
        std::fs::write(&other, "x").unwrap();
        set_mtime(&other, ten_days_ago);

        let removed = cleanup_old_debs_impl(dir.path(), 1).unwrap();
        assert_eq!(removed, 1);
        assert!(!old.exists(), "old.deb должен быть удалён");
        assert!(fresh.exists(), "fresh.deb должен остаться");
        assert!(other.exists(), "не-deb файл должен остаться");
    }

    #[test]
    fn cleanup_old_debs_missing_dir_returns_zero() {
        let removed = cleanup_old_debs_impl(Path::new("/no/such/dir/12345"), 1).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn cleanup_old_debs_zero_days_removes_all_debs() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.deb");
        std::fs::write(&f, "x").unwrap();
        // age 0 ≥ threshold 0 → удалить.
        let removed = cleanup_old_debs_impl(dir.path(), 0).unwrap();
        assert_eq!(removed, 1);
    }

    /// `archives/partial/` копится при прерванной установке (network drop,
    /// диск переполнен). cleanup должен заглядывать и сюда, иначе старые
    /// `.deb` остаются навсегда.
    #[test]
    fn cleanup_old_debs_also_cleans_partial_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("partial");
        std::fs::create_dir(&partial).unwrap();

        // Старый deb в archives/ — должен быть удалён.
        let archives_old = dir.path().join("old-top.deb");
        std::fs::write(&archives_old, "x").unwrap();
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 86_400);
        set_mtime(&archives_old, ten_days_ago);

        // Старый deb в archives/partial/ — тоже должен быть удалён.
        let partial_old = partial.join("old-partial.deb");
        std::fs::write(&partial_old, "x").unwrap();
        set_mtime(&partial_old, ten_days_ago);

        // Свежий deb в archives/partial/ — должен остаться.
        let partial_fresh = partial.join("fresh-partial.deb");
        std::fs::write(&partial_fresh, "x").unwrap();

        let removed = cleanup_old_debs_impl(dir.path(), 1).unwrap();
        assert_eq!(removed, 2, "должны быть удалены два старых файла");
        assert!(!archives_old.exists());
        assert!(!partial_old.exists());
        assert!(partial_fresh.exists());
    }

    /// Если `archives/` существует, но `partial/` отсутствует — cleanup
    /// не должен ошибаться. Сценарий типичный для свежеустановленной ноды
    /// без прерванных apt-операций.
    #[test]
    fn cleanup_old_debs_works_without_partial_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.deb");
        std::fs::write(&old, "x").unwrap();
        let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 86_400);
        set_mtime(&old, ten_days_ago);

        let removed = cleanup_old_debs_impl(dir.path(), 1).unwrap();
        assert_eq!(removed, 1);
    }

    /// Helper: установить mtime через `File::set_modified` (stable с Rust
    /// 1.75). Используется только в тестах cleanup.
    fn set_mtime(path: &Path, when: SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }
}
