//! Apply-фаза `process.signal`.
//!
//! Логика:
//! 1. Re-десериализовать spec и собрать argv через `build_signal_argv`.
//!    Allowlist сигналов проверяется именно здесь — повторно после plan,
//!    чтобы apply был робастен к ручным правкам payload'а в registry.
//! 2. `deferred = true` (дефолт) → enqueue `DeferEntry { action: Command { argv },
//!    id: "process.signal:<name>", priority: Command }`. Реального
//!    `Command::spawn` НЕТ — выполнит replay.
//! 3. `deferred = false` → синхронно через DI-trait `ProcessSignalRunner`.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bosun_core::defers::{DeferAction, DeferEntry, DeferPriority, CURRENT_SPEC_VERSION};
use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::plan::describe_selector;
use super::spec::ProcessSignalSpec;

/// Init-system тег для defer-журнала. У `process.signal` нет привязки к
/// init-системе (это просто pkill), поэтому строка пустая — `make_id` для
/// `DeferAction::Command` всё равно игнорирует init_system.
const INIT_SYSTEM_NONE: &str = "";

/// Дефолт max_attempts для defer-замка повторов. Совпадает с
/// `runr.service` / `systemd.service` — 3 попытки до промоушена в
/// `.manual_clear`.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Таймаут синхронного `Command::spawn` (deferred=false). 5 секунд — pkill
/// — это лёгкая операция; если она висит дольше, что-то очень не так с
/// ядром или сигналами.
const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

/// Шаг polling-цикла для `try_wait` в синхронном пути.
const SYNC_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Allowlist сигналов. `KILL`/`STOP`/`CONT` сознательно исключены — для
/// остановки процессов есть `service.unit` со stop-семантикой.
const ALLOWED_SIGNALS: &[&str] = &["HUP", "TERM", "INT", "USR1", "USR2", "WINCH", "PIPE"];

/// Контракт исполнителя pkill-команд. DI-точка для тестов: в production
/// используется `RealProcessSignalRunner` поверх `std::process::Command`,
/// в тестах — recorder без побочных эффектов.
///
/// `argv` — полный массив, начиная с пути исполняемого файла (`pkill`).
/// Shell не вмешивается, никаких `sh -c`.
pub trait ProcessSignalRunner: Send + Sync {
    /// Запустить `argv` синхронно с таймаутом `SYNC_TIMEOUT`. Exit 0 → `Ok(())`.
    /// Прочее (ненулевой exit, signal, timeout, IO) → `Err(reason)`.
    fn run(&self, argv: &[String]) -> Result<(), String>;
}

/// Production-реализация. Запускает `argv` через `std::process::Command`,
/// поллит `try_wait` с шагом `SYNC_POLL_INTERVAL` до `SYNC_TIMEOUT`. По
/// таймауту — `Child::kill()` и возврат ошибки.
#[derive(Default, Debug)]
pub struct RealProcessSignalRunner;

impl ProcessSignalRunner for RealProcessSignalRunner {
    fn run(&self, argv: &[String]) -> Result<(), String> {
        let Some((cmd, rest)) = argv.split_first() else {
            return Err("process.signal: empty argv".to_string());
        };
        let mut command = Command::new(cmd);
        command
            .args(rest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| format!("process.signal: failed to spawn {cmd}: {e}"))?;

        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        return Ok(());
                    }
                    let code = status.code();
                    // pkill exit codes:
                    //   0 — один или более процессов найдены и сигналены
                    //   1 — процессов не найдено
                    //   2 — синтаксическая ошибка в командной строке
                    //   3 — внутренняя ошибка
                    //
                    // Exit=1 («не найдено») для chiit-кейса не блокер: если
                    // pg_doorman/postgres не запущен, hup-сигнал просто
                    // некому слать. Мы возвращаем Ok, чтобы defer не уходил
                    // в attempt-bump.
                    if code == Some(1) {
                        return Ok(());
                    }
                    return Err(format!(
                        "process.signal: {cmd} exited with status {status:?}"
                    ));
                }
                Ok(None) => {
                    if started.elapsed() >= SYNC_TIMEOUT {
                        // Грохаем child; если убийство тоже падает —
                        // прокидываем оригинальную причину (timeout).
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "process.signal: {cmd} timed out after {:?}",
                            SYNC_TIMEOUT
                        ));
                    }
                    thread::sleep(SYNC_POLL_INTERVAL);
                }
                Err(e) => {
                    return Err(format!("process.signal: try_wait error for {cmd}: {e}"));
                }
            }
        }
    }
}

/// Построить argv для pkill из spec'а. Валидирует:
/// - сигнал — допустимый по allowlist (с нормализацией `SIGHUP` → `HUP`),
/// - селектор — ровно один из `process_name` / `process_user`.
///
/// Возвращает массив, начинающийся с пути исполняемого файла (`pkill`).
/// Никаких shell-spec символов: всё параметры передаются как отдельные
/// элементы argv.
pub fn build_signal_argv(spec: &ProcessSignalSpec) -> Result<Vec<String>, PrimitiveError> {
    let normalized = normalize_signal(&spec.signal)?;

    match (&spec.process_name, &spec.process_user) {
        (Some(name), None) => Ok(vec![
            "pkill".to_string(),
            "--signal".to_string(),
            normalized,
            name.clone(),
        ]),
        (None, Some(user)) => Ok(vec![
            "pkill".to_string(),
            "--signal".to_string(),
            normalized,
            "-u".to_string(),
            user.clone(),
        ]),
        (Some(_), Some(_)) => Err(PrimitiveError::InvalidPayload(format!(
            "process.signal '{}': exactly one of process_name/process_user required, got both",
            spec.name,
        ))),
        (None, None) => Err(PrimitiveError::InvalidPayload(format!(
            "process.signal '{}': exactly one of process_name/process_user required, got neither",
            spec.name,
        ))),
    }
}

/// Нормализация имени сигнала: удаляет необязательный префикс `SIG` и
/// сверяет с allowlist. Невалидный сигнал → `InvalidPayload`.
fn normalize_signal(raw: &str) -> Result<String, PrimitiveError> {
    let bare = raw.strip_prefix("SIG").unwrap_or(raw);
    if ALLOWED_SIGNALS.contains(&bare) {
        Ok(bare.to_string())
    } else {
        Err(PrimitiveError::InvalidPayload(format!(
            "process.signal: signal {raw:?} not in allowlist {ALLOWED_SIGNALS:?}; \
             KILL/STOP/CONT excluded by design — use service.unit for stopping",
        )))
    }
}

/// Главная entry-point apply'я. Десериализует payload, проверяет diff,
/// выбирает sync/defer путь.
pub fn run(
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
    runner: &Arc<dyn ProcessSignalRunner>,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: ProcessSignalSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("process.signal payload: {e}")))?;

    // Re-валидация argv: если в registry попал payload без селектора или с
    // запрещённым сигналом, словим это здесь, а не в момент spawn'а.
    let argv = build_signal_argv(&spec)?;

    if spec.deferred {
        enqueue_defer(ctx, &spec, argv)
    } else {
        execute_sync(runner, &spec, &argv)
    }
}

/// Положить запись в журнал defers. Возвращает `ChangeReport::deferred`,
/// независимо от dedup-исхода (`Created`/`AlreadyExists`/`Subsumed`) —
/// семантически apply отложил действие, дальше за идемпотентность отвечает
/// journal.
fn enqueue_defer(
    ctx: &ApplyCtx,
    spec: &ProcessSignalSpec,
    argv: Vec<String>,
) -> Result<ChangeReport, PrimitiveError> {
    let action = DeferAction::Command { argv };
    // `make_id` для Command возвращает `command.run:<target>`, но нам нужен
    // собственный namespace `process.signal:<name>`, чтобы defer-журнал не
    // конфликтовал с потенциальными будущими `command.run` примитивами и
    // чтобы `target` нёс пользовательское имя ресурса. Поэтому собираем id
    // вручную; формат `process.signal:<name>` совпадает с тем, что
    // оркестратор использует в логах и метриках.
    let id = format!("process.signal:{}", spec.name);
    let entry = DeferEntry {
        spec_version: CURRENT_SPEC_VERSION,
        id: id.clone(),
        action,
        init_system: INIT_SYSTEM_NONE.to_string(),
        target: spec.name.as_str().to_string(),
        validate_cmd: None,
        health_check: None,
        priority: DeferPriority::Command,
        enqueued_at: chrono::Utc::now(),
        enqueued_by: Vec::new(),
        attempt_count: 0,
        max_attempts: DEFAULT_MAX_ATTEMPTS,
    };
    tracing::info!(
        signal = %spec.signal,
        target = %spec.name,
        defer_id = %id,
        "process.signal: enqueueing defer",
    );
    ctx.defers
        .enqueue(entry)
        .map_err(|e| PrimitiveError::DeferIo {
            path: ctx.defers.root().to_path_buf(),
            reason: format!("{e}"),
        })?;
    Ok(ChangeReport::deferred(format!(
        "queued {} signal for {}",
        spec.signal,
        describe_selector(spec),
    )))
}

/// Синхронный путь: вызываем runner с argv. На успех — `Changed`, иначе
/// — `Apply { reason }`. Cancellation-checks не нужны: pkill отвечает
/// меньше секунды, а полный таймаут зашит в `SYNC_TIMEOUT`.
fn execute_sync(
    runner: &Arc<dyn ProcessSignalRunner>,
    spec: &ProcessSignalSpec,
    argv: &[String],
) -> Result<ChangeReport, PrimitiveError> {
    tracing::info!(
        signal = %spec.signal,
        target = %spec.name,
        argv = ?argv,
        "process.signal: running synchronously",
    );
    runner
        .run(argv)
        .map(|()| {
            ChangeReport::changed(format!(
                "sent {} signal to {}",
                spec.signal,
                describe_selector(spec),
            ))
        })
        .map_err(|reason| PrimitiveError::Apply { reason })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    use bosun_core::defers::Journal;
    use bosun_core::{ApplyCtx, Diff, ResourceId, ResourceKind, SensitiveStore};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// Mock-runner: записывает все вызовы и возвращает заранее заданный
    /// результат. `MockRunnerHandle` хранит `Arc<MockRunner>` отдельно от
    /// trait-object'а, чтобы тесты имели прямой доступ к recorder'у без
    /// trait downcast'а (Any-маркер на trait'е отсутствует, и добавлять
    /// его ради тестов в production-trait — overhead).
    struct MockRunner {
        calls: Mutex<Vec<Vec<String>>>,
        result: Result<(), String>,
    }

    impl ProcessSignalRunner for MockRunner {
        fn run(&self, argv: &[String]) -> Result<(), String> {
            self.calls.lock().unwrap().push(argv.to_vec());
            self.result.clone()
        }
    }

    struct MockRunnerHandle {
        inner: Arc<MockRunner>,
    }

    impl MockRunnerHandle {
        fn ok() -> Self {
            Self {
                inner: Arc::new(MockRunner {
                    calls: Mutex::new(Vec::new()),
                    result: Ok(()),
                }),
            }
        }
        fn failing(reason: &str) -> Self {
            Self {
                inner: Arc::new(MockRunner {
                    calls: Mutex::new(Vec::new()),
                    result: Err(reason.to_string()),
                }),
            }
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.inner.calls.lock().unwrap().clone()
        }
        fn as_runner(&self) -> Arc<dyn ProcessSignalRunner> {
            self.inner.clone()
        }
    }

    fn make_resource(payload: serde_json::Value) -> Resource {
        let kind = ResourceKind::from_static("process.signal");
        let name = payload["name"].as_str().unwrap_or("test").to_string();
        let id = ResourceId::new(&kind, &name);
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

    fn force_update_diff(r: &Resource) -> Diff {
        Diff::Update {
            from: serde_json::json!({}),
            to: r.payload.clone(),
            description: "converge".into(),
        }
    }

    // -- build_signal_argv: матрица allowlist + селекторы ------------------

    #[test]
    fn build_argv_by_name_hup() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("hup-doorman").unwrap(),
            signal: "HUP".into(),
            process_name: Some("pg_doorman".into()),
            process_user: None,
            deferred: true,
        };
        let argv = build_signal_argv(&spec).unwrap();
        assert_eq!(argv, vec!["pkill", "--signal", "HUP", "pg_doorman"]);
    }

    #[test]
    fn build_argv_by_user_with_sig_prefix_normalized() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("reload-pg").unwrap(),
            signal: "SIGHUP".into(),
            process_name: None,
            process_user: Some("postgres".into()),
            deferred: true,
        };
        let argv = build_signal_argv(&spec).unwrap();
        // Префикс SIG должен быть нормализован.
        assert_eq!(argv, vec!["pkill", "--signal", "HUP", "-u", "postgres"]);
    }

    #[test]
    fn build_argv_signal_kill_is_invalid_payload() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("x").unwrap(),
            signal: "KILL".into(),
            process_name: Some("evil".into()),
            process_user: None,
            deferred: false,
        };
        let err = build_signal_argv(&spec).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(msg.contains("KILL"), "got: {msg}");
                assert!(msg.contains("allowlist") || msg.contains("not in"));
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_argv_signal_stop_is_invalid_payload() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("x").unwrap(),
            signal: "STOP".into(),
            process_name: Some("p".into()),
            process_user: None,
            deferred: false,
        };
        assert!(matches!(
            build_signal_argv(&spec).unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn build_argv_signal_cont_is_invalid_payload() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("x").unwrap(),
            signal: "CONT".into(),
            process_name: Some("p".into()),
            process_user: None,
            deferred: false,
        };
        assert!(matches!(
            build_signal_argv(&spec).unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn build_argv_signal_garbage_is_invalid_payload() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("x").unwrap(),
            signal: "MEOW".into(),
            process_name: Some("p".into()),
            process_user: None,
            deferred: false,
        };
        assert!(matches!(
            build_signal_argv(&spec).unwrap_err(),
            PrimitiveError::InvalidPayload(_),
        ));
    }

    #[test]
    fn build_argv_both_selectors_is_invalid_payload() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("x").unwrap(),
            signal: "HUP".into(),
            process_name: Some("a".into()),
            process_user: Some("b".into()),
            deferred: true,
        };
        let err = build_signal_argv(&spec).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(
                    msg.contains("exactly one of") && msg.contains("both"),
                    "got: {msg}",
                );
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_argv_no_selector_is_invalid_payload() {
        let spec = ProcessSignalSpec {
            name: bosun_core::UnitName::new("x").unwrap(),
            signal: "HUP".into(),
            process_name: None,
            process_user: None,
            deferred: true,
        };
        let err = build_signal_argv(&spec).unwrap_err();
        match err {
            PrimitiveError::InvalidPayload(msg) => {
                assert!(
                    msg.contains("exactly one of") && msg.contains("neither"),
                    "got: {msg}",
                );
            }
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[test]
    fn build_argv_supports_all_allowlist_signals() {
        for sig in ["HUP", "TERM", "INT", "USR1", "USR2", "WINCH", "PIPE"] {
            let spec = ProcessSignalSpec {
                name: bosun_core::UnitName::new(format!("test-{sig}")).unwrap(),
                signal: sig.to_string(),
                process_name: Some("p".into()),
                process_user: None,
                deferred: true,
            };
            let argv = build_signal_argv(&spec).unwrap();
            assert_eq!(argv[2], sig, "allowlist должен пропускать {sig}");
        }
    }

    // -- apply: deferred=true ------------------------------------

    #[test]
    fn apply_deferred_creates_journal_file_with_3c_prefix() {
        let runner = MockRunnerHandle::ok();
        let r = make_resource(serde_json::json!({
            "name": "hup-doorman",
            "signal": "HUP",
            "process_name": "pg_doorman",
            "deferred": true,
        }));
        let (tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap();
        assert!(report.deferred, "expected deferred report, got {report:?}");
        // Runner НЕ должен быть вызван.
        assert!(
            runner.calls().is_empty(),
            "deferred=true должен пропустить runner, calls={:?}",
            runner.calls(),
        );
        // В журнале должен быть ровно один файл с префиксом 3c-.
        let entries: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".deferred"))
            .collect();
        assert_eq!(entries.len(), 1, "expected 1 defer file, got {entries:?}");
        assert!(
            entries[0].starts_with("3c-"),
            "defer file должен иметь sortkey 3c-, got {entries:?}",
        );
        assert!(
            entries[0].contains("process.signal:hup-doorman"),
            "id должен быть process.signal:<name>, got {entries:?}",
        );
    }

    #[test]
    fn apply_deferred_payload_contains_argv() {
        let runner = MockRunnerHandle::ok();
        let r = make_resource(serde_json::json!({
            "name": "reload-pg",
            "signal": "SIGHUP",
            "process_user": "postgres",
        }));
        let (tmp, ctx) = make_ctx();
        let _ = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap();
        // Содержимое: считываем файл, парсим JSON.
        let entry = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".deferred"))
            .unwrap();
        let body = std::fs::read_to_string(entry.path()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["action"], "command.run");
        let argv: Vec<String> = json["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            argv,
            vec!["pkill", "--signal", "HUP", "-u", "postgres"],
            "argv должен быть нормализован и не содержать shell-специальных символов",
        );
        // Проверяем отсутствие shell-сep символов: ни `|`, ни `;`, ни `&`.
        for a in &argv {
            for ch in ['|', ';', '&', '`', '$', '>', '<'] {
                assert!(
                    !a.contains(ch),
                    "argv element {a:?} contains shell-special {ch:?}",
                );
            }
        }
    }

    #[test]
    fn apply_deferred_idempotent_second_enqueue_keeps_one_file() {
        let runner = MockRunnerHandle::ok();
        let r = make_resource(serde_json::json!({
            "name": "hup-doorman",
            "signal": "HUP",
            "process_name": "pg_doorman",
        }));
        let (tmp, ctx) = make_ctx();
        let _ = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap();
        let _ = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap();
        let entries: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".deferred"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "повторный enqueue должен быть idempotent, got {entries:?}",
        );
    }

    // -- apply: deferred=false (synchronous via runner) ---------------------

    #[test]
    fn apply_sync_ok_returns_changed_and_calls_runner() {
        let runner = MockRunnerHandle::ok();
        let r = make_resource(serde_json::json!({
            "name": "hup-doorman",
            "signal": "HUP",
            "process_name": "pg_doorman",
            "deferred": false,
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap();
        assert!(report.changed, "expected changed report, got {report:?}");
        assert!(!report.deferred);
        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "runner должен быть вызван ровно один раз");
        assert_eq!(
            calls[0],
            vec!["pkill", "--signal", "HUP", "pg_doorman"],
            "argv должен передаваться как есть",
        );
    }

    #[test]
    fn apply_sync_runner_err_returns_apply_error() {
        let runner = MockRunnerHandle::failing("pkill: permission denied");
        let r = make_resource(serde_json::json!({
            "name": "hup-doorman",
            "signal": "HUP",
            "process_name": "pg_doorman",
            "deferred": false,
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap_err();
        match err {
            PrimitiveError::Apply { reason } => {
                assert!(reason.contains("permission denied"), "got: {reason}");
            }
            other => panic!("expected Apply, got {other:?}"),
        }
        // Runner всё ещё вызывался ровно один раз.
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn apply_sync_kill_signal_is_invalid_payload_before_spawn() {
        // KILL должен быть отвергнут до spawn'а: даже с deferred=false и
        // mock runner'ом, build_signal_argv ловит KILL и возвращает
        // InvalidPayload без вызова runner'а.
        let runner = MockRunnerHandle::ok();
        let r = make_resource(serde_json::json!({
            "name": "evil",
            "signal": "KILL",
            "process_name": "victim",
            "deferred": false,
        }));
        let (_tmp, ctx) = make_ctx();
        let err = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
        // Runner НЕ должен был быть вызван.
        assert!(
            runner.calls().is_empty(),
            "KILL должен быть отвергнут до runner'а"
        );
    }

    #[test]
    fn apply_no_change_diff_returns_no_change_report() {
        let runner = MockRunnerHandle::ok();
        let r = make_resource(serde_json::json!({
            "name": "x",
            "signal": "HUP",
            "process_name": "p",
        }));
        let (_tmp, ctx) = make_ctx();
        let report = run(&r, &Diff::NoChange, &ctx, &runner.as_runner()).unwrap();
        assert!(!report.changed);
        assert!(!report.deferred);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn apply_deferred_default_when_field_missing() {
        // Если deferred не указан явно — должен быть true (через
        // serde-default), значит runner не дёргается.
        let runner = MockRunnerHandle::ok();
        let r = make_resource(serde_json::json!({
            "name": "default-deferred",
            "signal": "HUP",
            "process_name": "p",
        }));
        let (tmp, ctx) = make_ctx();
        let report = run(&r, &force_update_diff(&r), &ctx, &runner.as_runner()).unwrap();
        assert!(report.deferred, "default deferred=true ожидался");
        assert!(runner.calls().is_empty());
        let count = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".deferred"))
            .count();
        assert_eq!(count, 1);
    }

    // -- RealProcessSignalRunner (smoke-тест на /bin/true) -----------------

    #[test]
    fn real_runner_executes_true_and_returns_ok() {
        // /bin/true возвращает exit=0 — гарантированно успешный сценарий
        // для проверки basic spawn + wait. Если /bin/true недоступен в
        // runtime — это сигнал ненормальной среды, и тест честно упадёт.
        let runner = RealProcessSignalRunner;
        let res = runner.run(&["true".to_string()]);
        assert!(res.is_ok(), "expected Ok from /bin/true, got {res:?}");
    }

    #[test]
    fn real_runner_treats_pkill_no_match_as_ok() {
        // Симулируем «процесс не найден»: запускаем pkill с заведомо
        // несуществующим именем. pkill вернёт exit=1, runner должен это
        // трактовать как Ok (chiit-кейс — hup сигнал нечему слать).
        let runner = RealProcessSignalRunner;
        let res = runner.run(&[
            "pkill".to_string(),
            "--signal".to_string(),
            "HUP".to_string(),
            "definitely-no-such-process-bosun-test-2026".to_string(),
        ]);
        // Если pkill недоступен в среде CI — тест станет error, это
        // допустимо: pkill стандартный, и его отсутствие — повод чинить
        // окружение, а не маскировать.
        assert!(
            res.is_ok(),
            "pkill exit=1 (no match) должен трактоваться как Ok, got {res:?}",
        );
    }

    #[test]
    fn real_runner_empty_argv_returns_err() {
        let runner = RealProcessSignalRunner;
        let res = runner.run(&[]);
        match res {
            Err(msg) => assert!(msg.contains("empty argv"), "got: {msg}"),
            Ok(_) => panic!("expected Err on empty argv"),
        }
    }
}
