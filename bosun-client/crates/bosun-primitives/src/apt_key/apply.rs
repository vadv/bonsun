//! Apply-фаза `apt.key`.
//!
//! Поток:
//! 1. NoChange → ранний return.
//! 2. Re-check существования файла + (опционально) fingerprint verify.
//!    Если уже совпадает — возвращаем NoChange-report без переустановки.
//! 3. Present: скачать (url) или взять inline (key_data) → atomic write
//!    через NamedTempFile + persist → chmod 0o644. Если данные
//!    ASCII-armored — сначала прогон через `gpg --dearmor` (бинарь
//!    `/usr/bin/gpg` обязателен, на Debian/Ubuntu стоит из коробки).
//! 4. Absent: rm файла. ENOENT на момент apply трактуется как успех.
//! 5. После Present: если fingerprint указан, верификация через
//!    `AptKeyBackend::fingerprint_of` и сверка нормализованных hex-строк.
//!
//! DI: `AptKeyBackend` — HTTP download + gpg-операции. Production —
//! `RealAptKeyBackend` (ureq + gpg --dearmor / --show-keys), тесты —
//! recorder.

use std::path::{Path, PathBuf};

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::plan::{decide_action, validate_source_combination, Action};
use super::spec::AptKeySpec;

/// Mode for keyring file. 0o644 — readable всем, modifiable только root'ом.
/// apt сам читает keyring под root'ом, но другим утилитам (например,
/// `apt-key list` в дeprecated режиме) удобно иметь read-доступ для
/// диагностики.
const KEYRING_MODE: u32 = 0o644;

/// Контракт для побочных операций apt.key: HTTP-скачивание и
/// gpg-вызовы (`--dearmor`, `--show-keys --with-fingerprint`).
/// DI-точка для тестов.
pub trait AptKeyBackend: Send + Sync {
    /// Скачать тело по URL. Production — HTTP GET через ureq с
    /// разумным таймаутом. Возвращает raw байты (armored или binary).
    fn download(&self, url: &str) -> Result<Vec<u8>, String>;

    /// Прогнать байты через `gpg --dearmor` если они ASCII-armored;
    /// иначе вернуть как есть. Production — spawn `gpg --dearmor`.
    fn dearmor_if_needed(&self, data: &[u8]) -> Result<Vec<u8>, String>;

    /// Прочитать fingerprint существующего keyring'а через
    /// `gpg --show-keys --with-fingerprint --with-colons <path>`.
    /// Возвращает нормализованный hex без пробелов.
    fn fingerprint_of(&self, keyring_path: &Path) -> Result<String, String>;
}

/// Production-реализация.
pub struct RealAptKeyBackend;

/// Таймаут на HTTP-скачивание. Меньше chiit'овской пятиминутной retry-петли:
/// если зеркало не отвечает за 30s, лучше упасть и дать defer/retry-логике
/// orchestrator'a решить, чем тихо висеть.
const HTTP_TIMEOUT_SEC: u64 = 30;

impl AptKeyBackend for RealAptKeyBackend {
    fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        use std::io::Read;

        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SEC))
            .build();
        let response = agent
            .get(url)
            .call()
            .map_err(|e| format!("http GET {url}: {e}"))?;
        if response.status() != 200 {
            return Err(format!(
                "http GET {url}: unexpected status {}",
                response.status()
            ));
        }
        // Ограничиваем размер тела на всякий случай: GPG-ключ редко больше
        // 1 MiB, защита от mirror, отдающего огромный мусор.
        let mut reader = response.into_reader().take(4 * 1024 * 1024);
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| format!("http body read {url}: {e}"))?;
        Ok(buf)
    }

    fn dearmor_if_needed(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        if !is_ascii_armored(data) {
            return Ok(data.to_vec());
        }
        run_gpg_dearmor(data)
    }

    fn fingerprint_of(&self, keyring_path: &Path) -> Result<String, String> {
        run_gpg_fingerprint(keyring_path)
    }
}

/// Сигнатура ASCII-armored GPG-блока: первая 50-байтная строка содержит
/// `-----BEGIN PGP PUBLIC KEY BLOCK-----`. Простой prefix-check этого
/// достаточно — binary-формат начинается с тэга 0x99 или 0xC6.
pub(crate) fn is_ascii_armored(data: &[u8]) -> bool {
    const MARKER: &[u8] = b"-----BEGIN PGP";
    // Игнорируем ведущий whitespace/BOM
    let trimmed = data
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0xEF | 0xBB | 0xBF))
        .map(|i| &data[i..])
        .unwrap_or(data);
    trimmed.starts_with(MARKER)
}

/// Запустить `gpg --dearmor` и подать `data` на stdin. Stdout — binary
/// keyring.
fn run_gpg_dearmor(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("gpg")
        .args(["--dearmor", "--batch", "--no-tty"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn gpg --dearmor: {e}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "gpg --dearmor: no stdin".to_string())?;
        stdin
            .write_all(data)
            .map_err(|e| format!("gpg --dearmor stdin: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("gpg --dearmor wait: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gpg --dearmor failed (status {:?}): {stderr}",
            output.status.code()
        ));
    }
    Ok(output.stdout)
}

/// Запустить `gpg --show-keys --with-fingerprint --with-colons <path>`,
/// извлечь fingerprint из строк `fpr:::::::::<HEX>::`.
fn run_gpg_fingerprint(keyring_path: &Path) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let output = Command::new("gpg")
        .args([
            "--show-keys",
            "--with-fingerprint",
            "--with-colons",
            "--batch",
            "--no-tty",
        ])
        .arg(keyring_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn gpg --show-keys: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gpg --show-keys failed (status {:?}): {stderr}",
            output.status.code()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_first_fingerprint(&stdout).ok_or_else(|| {
        format!(
            "gpg --show-keys: no fingerprint in output of {}",
            keyring_path.display()
        )
    })
}

/// Парсер вывода `gpg --with-colons`. Ищем первую строку, начинающуюся с
/// `fpr:`, fingerprint — в 10-й колонке (индекс 9, разделитель `:`).
pub(crate) fn parse_first_fingerprint(out: &str) -> Option<String> {
    for line in out.lines() {
        if !line.starts_with("fpr:") {
            continue;
        }
        let parts: Vec<&str> = line.split(':').collect();
        if let Some(fpr) = parts.get(9) {
            if !fpr.is_empty() {
                return Some(fpr.to_string());
            }
        }
    }
    None
}

/// Нормализовать fingerprint: убрать пробелы, перевести в верхний регистр.
/// Сравнение fingerprint'ов делается на нормализованных строках.
pub(crate) fn normalize_fingerprint(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Главная функция apply.
pub fn run(
    backend: &dyn AptKeyBackend,
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: AptKeySpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.key payload: {e}")))?;

    validate_source_combination(&spec)?;

    if ctx.cancelled_or_past_deadline() {
        return Err(PrimitiveError::Cancelled);
    }

    let keyring_path = spec.effective_keyring_path();
    let exists = keyring_path.exists();
    let action = decide_action(exists, spec.fingerprint.is_some(), spec.state);

    match action {
        Action::NoChange => Ok(ChangeReport::no_change()),
        Action::Install => install_key(backend, &spec, &keyring_path),
        Action::Remove => remove_key(&spec, &keyring_path),
    }
}

/// Установить ключ: скачать/взять inline → dearmor → atomic write →
/// (опц.) verify fingerprint. Если файл уже совпадает по fingerprint —
/// NoChange-report.
fn install_key(
    backend: &dyn AptKeyBackend,
    spec: &AptKeySpec,
    keyring_path: &Path,
) -> Result<ChangeReport, PrimitiveError> {
    // Если файл уже на месте и fingerprint совпадает — это идемпотентный
    // повторный apply: ничего не переустанавливаем, чтобы не задирать
    // mtime и не плодить лишние «изменилось».
    if keyring_path.exists() {
        if let Some(expected) = &spec.fingerprint {
            let actual = backend
                .fingerprint_of(keyring_path)
                .map_err(|reason| PrimitiveError::Apply { reason })?;
            if normalize_fingerprint(&actual) == normalize_fingerprint(expected) {
                return Ok(ChangeReport::no_change());
            }
            // Несовпадение — переустанавливаем; новый fingerprint проверим
            // ещё раз ниже после write.
            tracing::warn!(
                resource = %spec.name,
                expected = %expected,
                actual = %actual,
                "apt.key: fingerprint mismatch, reinstalling",
            );
        }
    }

    let raw = obtain_key_data(backend, spec)?;
    let binary = backend
        .dearmor_if_needed(&raw)
        .map_err(|reason| PrimitiveError::Apply { reason })?;

    write_keyring_atomic(keyring_path, &binary)
        .map_err(|reason| PrimitiveError::Apply { reason })?;

    if let Some(expected) = &spec.fingerprint {
        let actual = backend
            .fingerprint_of(keyring_path)
            .map_err(|reason| PrimitiveError::Apply { reason })?;
        if normalize_fingerprint(&actual) != normalize_fingerprint(expected) {
            return Err(PrimitiveError::Apply {
                reason: format!(
                    "apt.key '{}': fingerprint mismatch after install (expected {expected}, got {actual})",
                    spec.name,
                ),
            });
        }
    }

    Ok(ChangeReport::changed(format!(
        "installed apt key {} at {}",
        spec.name,
        keyring_path.display()
    )))
}

/// Получить байты ключа из spec'а — либо HTTP-скачать, либо взять inline.
fn obtain_key_data(
    backend: &dyn AptKeyBackend,
    spec: &AptKeySpec,
) -> Result<Vec<u8>, PrimitiveError> {
    match (&spec.url, &spec.key_data) {
        (Some(url), None) => backend
            .download(url)
            .map_err(|reason| PrimitiveError::Apply { reason }),
        (None, Some(data)) => Ok(data.as_bytes().to_vec()),
        // validate_source_combination гарантирует, что это не случится.
        _ => Err(PrimitiveError::InvalidPayload(format!(
            "apt.key '{}': internal — source combination not validated",
            spec.name
        ))),
    }
}

/// Atomic write: temp в той же директории → fsync → persist → set mode.
/// Если parent-директория не существует — создаём.
fn write_keyring_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| format!("keyring path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| format!("tempfile in {}: {e}", parent.display()))?;
    tmp.write_all(data)
        .map_err(|e| format!("write keyring tmp: {e}"))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| format!("fsync keyring tmp: {e}"))?;
    let _persisted = tmp
        .persist(path)
        .map_err(|e| format!("persist {}: {e}", path.display()))?;
    let perms = std::fs::Permissions::from_mode(KEYRING_MODE);
    std::fs::set_permissions(path, perms)
        .map_err(|e| format!("set_permissions {}: {e}", path.display()))?;
    Ok(())
}

/// Удалить keyring. ENOENT на момент удаления — успех (race с другим
/// процессом или ручным rm допустим).
fn remove_key(spec: &AptKeySpec, keyring_path: &Path) -> Result<ChangeReport, PrimitiveError> {
    match std::fs::remove_file(keyring_path) {
        Ok(()) => Ok(ChangeReport::changed(format!(
            "removed apt key {} at {}",
            spec.name,
            keyring_path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ChangeReport::no_change()),
        Err(e) => Err(PrimitiveError::Io {
            context: format!("remove apt key {}", keyring_path.display()),
            source: e,
        }),
    }
}

/// Тестовый helper: путь к директории под `/etc/apt/keyrings/<name>.gpg`.
/// Не используется в production-коде; оставлен для будущих CLI-конфигов.
#[allow(dead_code)]
pub(crate) fn default_keyrings_dir() -> PathBuf {
    PathBuf::from("/etc/apt/keyrings")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtx, ResourceId, ResourceKind, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// Mock backend: записывает все вызовы, возвращает заранее заданные
    /// результаты.
    struct MockBackend {
        download_result: Result<Vec<u8>, String>,
        dearmor_result: Result<Vec<u8>, String>,
        fingerprint_result: Result<String, String>,
        calls: Mutex<MockCalls>,
    }

    #[derive(Default)]
    struct MockCalls {
        download: Vec<String>,
        dearmor: usize,
        fingerprint: Vec<PathBuf>,
    }

    impl MockBackend {
        fn ok(content: &[u8]) -> Self {
            Self {
                download_result: Ok(content.to_vec()),
                dearmor_result: Ok(content.to_vec()),
                fingerprint_result: Ok("ABCD".into()),
                calls: Mutex::new(MockCalls::default()),
            }
        }
        fn with_fingerprint(content: &[u8], fpr: &str) -> Self {
            Self {
                download_result: Ok(content.to_vec()),
                dearmor_result: Ok(content.to_vec()),
                fingerprint_result: Ok(fpr.to_string()),
                calls: Mutex::new(MockCalls::default()),
            }
        }
        fn download_fail(reason: &str) -> Self {
            Self {
                download_result: Err(reason.to_string()),
                dearmor_result: Ok(Vec::new()),
                fingerprint_result: Ok("X".into()),
                calls: Mutex::new(MockCalls::default()),
            }
        }
    }

    impl AptKeyBackend for MockBackend {
        fn download(&self, url: &str) -> Result<Vec<u8>, String> {
            self.calls.lock().unwrap().download.push(url.to_string());
            self.download_result.clone()
        }
        fn dearmor_if_needed(&self, _data: &[u8]) -> Result<Vec<u8>, String> {
            self.calls.lock().unwrap().dearmor += 1;
            self.dearmor_result.clone()
        }
        fn fingerprint_of(&self, keyring_path: &Path) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .fingerprint
                .push(keyring_path.to_path_buf());
            self.fingerprint_result.clone()
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
        let kind = ResourceKind::from_static("apt.key");
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

    fn update_diff() -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: serde_json::json!({}),
            description: "install".into(),
        }
    }

    #[test]
    fn run_no_change_returns_early() {
        let backend = MockBackend::ok(b"data");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example/k",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&backend, &r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
        assert!(backend.calls.lock().unwrap().download.is_empty());
    }

    #[test]
    fn run_present_url_downloads_and_writes_keyring() {
        let backend = MockBackend::ok(b"binary-key");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("test.gpg");
        let r = make_resource(serde_json::json!({
            "name": "test",
            "state": "present",
            "url": "https://example.com/key.gpg",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        let report = run(&backend, &r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        assert!(keyring.exists());
        let written = std::fs::read(&keyring).unwrap();
        assert_eq!(written, b"binary-key");
        let calls = backend.calls.lock().unwrap();
        assert_eq!(
            calls.download,
            vec!["https://example.com/key.gpg".to_string()]
        );
        assert_eq!(calls.dearmor, 1);
    }

    #[test]
    fn run_present_key_data_inline_writes_keyring() {
        let backend = MockBackend::ok(b"inline-data");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("inline.gpg");
        let r = make_resource(serde_json::json!({
            "name": "inline",
            "state": "present",
            "key_data": "raw-binary-or-armored",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        let report = run(&backend, &r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        // download не вызывается, dearmor вызывается.
        let calls = backend.calls.lock().unwrap();
        assert!(calls.download.is_empty());
        assert_eq!(calls.dearmor, 1);
    }

    #[test]
    fn run_present_existing_matching_fingerprint_returns_no_change() {
        // Файл уже на месте, fingerprint совпадает — apply не должен
        // переписывать.
        let backend = MockBackend::with_fingerprint(b"existing", "ABCD1234");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("existing.gpg");
        std::fs::write(&keyring, b"existing").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example/k",
            "fingerprint": "ABCD 1234",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        let report = run(&backend, &r, &update_diff(), &ctx).unwrap();
        assert!(!report.changed, "matching fingerprint должен дать NoChange");
        let calls = backend.calls.lock().unwrap();
        assert_eq!(
            calls.download.len(),
            0,
            "не должно быть нового download при совпадении"
        );
    }

    #[test]
    fn run_present_existing_mismatched_fingerprint_reinstalls() {
        // Контрольная схема: backend читает fingerprint существующего файла
        // и возвращает «OLD-FPR» при первом вызове, а после переустановки
        // — «NEW-FPR». В spec задан expected=«NEW-FPR»: первый verify даёт
        // mismatch (OLD vs NEW), apply переустанавливает, второй verify
        // совпадает. Для упрощения mock всегда возвращает «NEW-FPR» —
        // тест проверяет два механизма: (1) factual mismatch при чтении
        // old файла обнаруживается через сравнение с expected; (2) после
        // переустановки post-verify совпадает.
        // Здесь mock returns "NEW-FPR" всегда, expected="NEW-FPR".
        // Чтобы проверить именно ветку «переустановки», подменим content:
        // mock содержимое = b"new", старый файл = b"old". После переустановки
        // содержимое keyring'а должно стать b"new" — это и есть сигнал
        // переустановки. Fingerprint совпадает с expected на post-verify.
        let backend = MockBackend::with_fingerprint(b"new", "NEW-FPR");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("mismatch.gpg");
        std::fs::write(&keyring, b"old").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example/k",
            "fingerprint": "OLD-FPR",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        // Mock возвращает actual="NEW-FPR", expected="OLD-FPR" — mismatch.
        // После переустановки post-verify тоже даст "NEW-FPR" против
        // expected "OLD-FPR" → Apply error. Это даёт точно ту защиту,
        // которую обещает примитив: если ключ не тот, что задан в spec,
        // — мы НЕ молча принимаем чужой ключ.
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("mismatch"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
        // Файл всё же переписан — на этой стадии мы переустановили его
        // (получили новый mismatch уже после write). Это «не идеально»:
        // на проде такого случая быть не должно (или mirror сменил ключ,
        // или сменился spec). Поведение видно через содержимое файла.
        let written = std::fs::read(&keyring).unwrap();
        assert_eq!(written, b"new");
    }

    #[test]
    fn run_present_post_install_fingerprint_mismatch_is_apply_error() {
        // download'ом получили чужой ключ, fingerprint не совпадает —
        // должен вернуть Apply error и оставить файл на диске (rollback
        // не делаем, оператор сам разберётся).
        let backend = MockBackend::with_fingerprint(b"wrong-key", "ACTUAL-FPR");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("verify.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example/k",
            "fingerprint": "EXPECTED-FPR",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("mismatch"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[test]
    fn run_present_download_fail_returns_apply_error() {
        let backend = MockBackend::download_fail("network error");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("fail.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example/k",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("network error"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
        assert!(!keyring.exists());
    }

    #[test]
    fn run_absent_existing_removes_keyring() {
        let backend = MockBackend::ok(b"unused");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("toremove.gpg");
        std::fs::write(&keyring, b"x").unwrap();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "absent",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        let report = run(&backend, &r, &update_diff(), &ctx).unwrap();
        assert!(report.changed);
        assert!(!keyring.exists());
    }

    #[test]
    fn run_absent_missing_returns_no_change() {
        let backend = MockBackend::ok(b"unused");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("never.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "absent",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        let report = run(&backend, &r, &update_diff(), &ctx).unwrap();
        assert!(!report.changed);
    }

    #[test]
    fn run_cancelled_returns_cancelled_no_io() {
        let backend = MockBackend::ok(b"data");
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
        let key_tmp = tempfile::tempdir().unwrap();
        let keyring = key_tmp.path().join("x.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://example/k",
            "keyring_path": keyring,
        }));
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
        assert!(backend.calls.lock().unwrap().download.is_empty());
    }

    #[test]
    fn run_invalid_payload_returns_invalid_payload() {
        let backend = MockBackend::ok(b"");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "url": "https://x",
            "key_data": "data",
        }));
        let (_t, ctx) = make_ctx();
        let err = run(&backend, &r, &update_diff(), &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
    }

    #[test]
    fn keyring_file_permissions_are_0o644() {
        use std::os::unix::fs::PermissionsExt;
        let backend = MockBackend::ok(b"data");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("perms.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "key_data": "data",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        run(&backend, &r, &update_diff(), &ctx).unwrap();
        let mode = std::fs::metadata(&keyring).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "expected 0o644 mode");
    }

    #[test]
    fn keyring_dir_is_created_if_missing() {
        let backend = MockBackend::ok(b"data");
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("deep").join("nested").join("k.gpg");
        let r = make_resource(serde_json::json!({
            "name": "x",
            "state": "present",
            "key_data": "data",
            "keyring_path": keyring,
        }));
        let (_t, ctx) = make_ctx();
        run(&backend, &r, &update_diff(), &ctx).unwrap();
        assert!(keyring.exists());
    }

    #[test]
    fn is_ascii_armored_detects_pgp_marker() {
        assert!(is_ascii_armored(
            b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nXXX"
        ));
        assert!(!is_ascii_armored(b"\x99\x01\x0d\x00binary-blob"));
        // Leading whitespace OK.
        assert!(is_ascii_armored(
            b"  \n-----BEGIN PGP PUBLIC KEY BLOCK-----"
        ));
    }

    #[test]
    fn normalize_fingerprint_strips_whitespace_and_uppercases() {
        assert_eq!(normalize_fingerprint("abcd 1234"), "ABCD1234");
        assert_eq!(normalize_fingerprint("AB CD\t12\n34"), "ABCD1234");
        assert_eq!(normalize_fingerprint(""), "");
    }

    #[test]
    fn parse_first_fingerprint_extracts_hex_from_colon_output() {
        let out = "tru::1:1700000000:0:3:1:5\n\
                   pub:-:255:22:1234567890ABCDEF:1700000000:::-:::scESC:::::ed25519::\n\
                   fpr:::::::::ABCD1234567890ABCDEFABCDEF0123456789ABCD::\n\
                   uid:-::::1700000000::ABCDEF::Some Name <e@x>:::::::::\n";
        let fpr = parse_first_fingerprint(out).unwrap();
        assert_eq!(fpr, "ABCD1234567890ABCDEFABCDEF0123456789ABCD");
    }

    #[test]
    fn parse_first_fingerprint_returns_none_if_absent() {
        let out = "pub:-:255:22:KEYID:1700000000::::scESC:::::ed25519::\n";
        assert!(parse_first_fingerprint(out).is_none());
    }
}
