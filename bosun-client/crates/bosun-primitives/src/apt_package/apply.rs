//! Apply-фаза `apt.package`. Орchestrate steps:
//!
//! 1. NoChange → ранний return.
//! 2. Десериализация payload в `AptPackageSpec`.
//! 3. Check cancel/deadline.
//! 4. Probe `/var/lib/dpkg/lock-frontend` — `DpkgLocked` если занят.
//! 5. Per-resource deadline = min(ctx.deadline, now + spec.timeout_sec).
//! 6. `apt-get install …` → analyze:
//!    - Success → `ChangeReport::changed`. Если stderr непустой — пишем в log_dir.
//!    - DpkgInterrupted → `dpkg --configure -a` + один retry install.
//!    - CandidateMissing → `apt-get update` с retries + один retry install.
//!    - OtherFailure → `Exec` с stderr_excerpt; полный stderr — в файл.

use std::path::Path;
use std::time::{Duration, Instant};

use bosun_core::{ApplyCtx, ChangeReport, Diff, PrimitiveError, Resource};

use super::exec::{analyze_install_result, CommandResult, CommandRunner, InstallOutcome};
use super::lock_probe::probe_dpkg_lock;
use super::recovery::{run_apt_update_with_retries, run_dpkg_configure_a, stderr_excerpt};
use super::spec::AptPackageSpec;

/// Главный entry-point apply'я. Принимает trait-объект `CommandRunner`,
/// чтобы пользоваться mock'ом в unit-тестах. В production `AptPrimitive`
/// держит `RealCommandRunner`.
pub fn run(
    runner: &dyn CommandRunner,
    dpkg_lock_path: &Path,
    resource: &Resource,
    diff: &Diff,
    ctx: &ApplyCtx,
) -> Result<ChangeReport, PrimitiveError> {
    if diff.is_no_change() {
        return Ok(ChangeReport::no_change());
    }

    let spec: AptPackageSpec = serde_json::from_value(resource.payload.clone())
        .map_err(|e| PrimitiveError::InvalidPayload(format!("apt.package payload: {e}")))?;

    check_cancel_or_deadline(ctx)?;

    // dpkg-lock probe — quick-fail при чьей-то блокировке.
    probe_dpkg_lock(dpkg_lock_path)?;

    let resource_deadline = compute_resource_deadline(ctx.deadline, spec.timeout_sec);

    tracing::info!(
        package = %spec.name,
        version = ?spec.version,
        "starting install",
    );
    let install_result = install_attempt(runner, &spec, resource_deadline, &ctx.cancel)?;
    let outcome = analyze_install_result(&install_result);
    tracing::debug!(outcome = ?outcome, exit = ?install_result.exit_code, "install attempt result");

    match outcome {
        InstallOutcome::Success => {
            // stderr может содержать предупреждения даже при exit 0 — пишем
            // в файл для трассировки. Это best-effort: ошибка записи в
            // log_dir не должна валить apply.
            if !install_result.stderr.is_empty() {
                let _ = write_log(&ctx.log_dir, "install", &install_result.stderr);
            }
            Ok(ChangeReport::changed(install_success_message(&spec)))
        }
        InstallOutcome::DpkgInterrupted => {
            tracing::warn!(
                package = %spec.name,
                "dpkg interrupted, running dpkg --configure -a",
            );
            recover_dpkg_then_retry(runner, &spec, resource_deadline, ctx, &install_result)
        }
        InstallOutcome::CandidateMissing => {
            tracing::warn!(
                package = %spec.name,
                "candidate missing, running apt-get update",
            );
            recover_apt_update_then_retry(runner, &spec, resource_deadline, ctx, &install_result)
        }
        InstallOutcome::OtherFailure => {
            let excerpt = stderr_excerpt(&install_result.stderr);
            tracing::error!(
                package = %spec.name,
                exit = ?install_result.exit_code,
                stderr_excerpt = %excerpt,
                "install failed",
            );
            let _ = write_log(&ctx.log_dir, "install", &install_result.stderr);
            Err(PrimitiveError::Exec {
                reason: format!("apt-get install {} failed", &spec.name),
                exit: install_result.exit_code,
                stderr_excerpt: excerpt,
            })
        }
    }
}

fn check_cancel_or_deadline(ctx: &ApplyCtx) -> Result<(), PrimitiveError> {
    if ctx.cancelled_or_past_deadline() {
        return Err(PrimitiveError::Cancelled);
    }
    Ok(())
}

fn compute_resource_deadline(ctx_deadline: Instant, spec_timeout_sec: u32) -> Instant {
    let from_spec = Instant::now() + Duration::from_secs(u64::from(spec_timeout_sec));
    if ctx_deadline < from_spec {
        ctx_deadline
    } else {
        from_spec
    }
}

/// Сборка `apt-get install` с canonical-флагами по spec.
/// F08: `--allow-downgrades` и `--allow-change-held-packages` добавляются
/// только если spec явно их разрешает. По умолчанию apt отказывается от
/// downgrade и не трогает `apt-mark hold` пакеты.
fn install_attempt(
    runner: &dyn CommandRunner,
    spec: &AptPackageSpec,
    deadline: Instant,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<CommandResult, PrimitiveError> {
    let pkg_spec = match &spec.version {
        Some(v) => format!("{}={}", spec.name, v),
        None => spec.name.clone(),
    };
    let mut args: Vec<&str> = vec![
        "install",
        "-qy",
        "-oDpkg::Options::=--force-confdef",
        "-oDpkg::Options::=--force-confold",
        "-oAPT::Acquire::Retries=3",
        "-oDPkg::Lock::Timeout=30",
    ];
    if spec.allow_downgrade {
        args.push("--allow-downgrades");
    }
    if spec.allow_change_held {
        args.push("--allow-change-held-packages");
    }
    args.push(&pkg_spec);
    runner.run("apt-get", &args, deadline, cancel)
}

fn install_success_message(spec: &AptPackageSpec) -> String {
    match &spec.version {
        Some(v) => format!("installed {}={v}", spec.name),
        None => format!("installed {}", spec.name),
    }
}

/// Recovery после «dpkg was interrupted»: configure -a + ровно один retry.
fn recover_dpkg_then_retry(
    runner: &dyn CommandRunner,
    spec: &AptPackageSpec,
    deadline: Instant,
    ctx: &ApplyCtx,
    first_result: &CommandResult,
) -> Result<ChangeReport, PrimitiveError> {
    let _ = write_log(&ctx.log_dir, "install", &first_result.stderr);
    check_cancel_or_deadline(ctx)?;

    if let Err(e) = run_dpkg_configure_a(runner, deadline, &ctx.cancel) {
        // Сохраняем stderr `dpkg --configure -a` — даже если это `Exec`,
        // у нас уже есть excerpt в самой ошибке, но полный лог поможет
        // post-mortem'у.
        if let PrimitiveError::Exec { stderr_excerpt, .. } = &e {
            let _ = write_log(&ctx.log_dir, "configure", stderr_excerpt);
        }
        return Err(e);
    }

    check_cancel_or_deadline(ctx)?;
    let retry = install_attempt(runner, spec, deadline, &ctx.cancel)?;
    if retry.exit_code == Some(0) {
        if !retry.stderr.is_empty() {
            let _ = write_log(&ctx.log_dir, "install-retry", &retry.stderr);
        }
        return Ok(ChangeReport::changed(install_success_message(spec)));
    }
    let _ = write_log(&ctx.log_dir, "install-retry", &retry.stderr);
    Err(PrimitiveError::Exec {
        reason: format!(
            "apt-get install {} failed after dpkg --configure -a",
            &spec.name
        ),
        exit: retry.exit_code,
        stderr_excerpt: stderr_excerpt(&retry.stderr),
    })
}

/// Recovery после «candidate missing»: apt-get update + ровно один retry.
fn recover_apt_update_then_retry(
    runner: &dyn CommandRunner,
    spec: &AptPackageSpec,
    deadline: Instant,
    ctx: &ApplyCtx,
    first_result: &CommandResult,
) -> Result<ChangeReport, PrimitiveError> {
    let _ = write_log(&ctx.log_dir, "install", &first_result.stderr);
    check_cancel_or_deadline(ctx)?;

    if let Err(e) = run_apt_update_with_retries(runner, deadline, &ctx.cancel) {
        if let PrimitiveError::Exec { stderr_excerpt, .. } = &e {
            let _ = write_log(&ctx.log_dir, "update", stderr_excerpt);
        }
        return Err(e);
    }

    check_cancel_or_deadline(ctx)?;
    let retry = install_attempt(runner, spec, deadline, &ctx.cancel)?;
    if retry.exit_code == Some(0) {
        if !retry.stderr.is_empty() {
            let _ = write_log(&ctx.log_dir, "install-retry", &retry.stderr);
        }
        return Ok(ChangeReport::changed(install_success_message(spec)));
    }
    let _ = write_log(&ctx.log_dir, "install-retry", &retry.stderr);
    Err(PrimitiveError::Exec {
        reason: format!("apt-get install {} failed after apt-get update", &spec.name),
        exit: retry.exit_code,
        stderr_excerpt: stderr_excerpt(&retry.stderr),
    })
}

/// Перезаписать `{log_dir}/apt-{step}-last-error.log` полным stderr.
/// Best-effort: возвращаем `Err` только для compile-time API, в caller'е
/// игнорируется — мы не хотим, чтобы ошибка записи логов валила apply.
fn write_log(log_dir: &Path, step: &str, stderr: &str) -> std::io::Result<()> {
    if !log_dir.exists() {
        std::fs::create_dir_all(log_dir)?;
    }
    let path = log_dir.join(format!("apt-{step}-last-error.log"));
    std::fs::write(path, stderr)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bosun_core::{ResourceId, ResourceKind, SensitiveStore};
    use tokio_util::sync::CancellationToken;

    use super::*;

    /// MockRunner с очередью ответов. Если ответов меньше, чем call'ов,
    /// тест паникует — это явная неполнота сценария.
    struct MockRunner {
        responses: Mutex<Vec<CommandResult>>,
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandResult>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
        fn count_calls_to(&self, cmd: &str, first_arg: &str) -> usize {
            self.calls()
                .iter()
                .filter(|(c, args)| c == cmd && args.first().map(|s| s.as_str()) == Some(first_arg))
                .count()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(
            &self,
            cmd: &str,
            args: &[&str],
            _deadline: Instant,
            _cancel: &CancellationToken,
        ) -> Result<CommandResult, PrimitiveError> {
            self.calls
                .lock()
                .unwrap()
                .push((cmd.into(), args.iter().map(|s| s.to_string()).collect()));
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                panic!("MockRunner: no more responses for {cmd} {args:?}");
            }
            Ok(responses.remove(0))
        }
    }

    fn cmdres(exit: Option<i32>, stderr: &str) -> CommandResult {
        CommandResult {
            exit_code: exit,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    fn resource(name: &str, version: Option<&str>) -> Resource {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, name);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "name": name,
                "version": version,
                "timeout_sec": 600_u32,
                "allow_downgrade": false,
                "allow_change_held": false,
            }),
            reload_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    /// Resource с явными opt-in флагами для F08-тестов.
    fn resource_with_flags(
        name: &str,
        version: Option<&str>,
        allow_downgrade: bool,
        allow_change_held: bool,
    ) -> Resource {
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, name);
        Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({
                "name": name,
                "version": version,
                "timeout_sec": 600_u32,
                "allow_downgrade": allow_downgrade,
                "allow_change_held": allow_change_held,
            }),
            reload_on: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn make_ctx(log_dir: std::path::PathBuf) -> ApplyCtx {
        ApplyCtx::new(
            Instant::now() + Duration::from_secs(600),
            CancellationToken::new(),
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            std::path::PathBuf::from("/tmp"),
            log_dir,
        )
    }

    /// Возвращает временный lock-frontend файл, который никто не держит.
    fn free_lock_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dpkg-lock-frontend");
        std::fs::write(&path, "").unwrap();
        (dir, path)
    }

    #[test]
    fn apply_no_change_returns_early_and_makes_no_calls() {
        let runner = MockRunner::new(vec![]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());

        let report = run(&runner, &lock_path, &r, &Diff::NoChange, &ctx).unwrap();
        assert!(!report.changed);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn apply_success_on_first_attempt() {
        let runner = MockRunner::new(vec![cmdres(Some(0), "")]);
        let r = resource("nginx", Some("1.18.0"));
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());

        let diff = Diff::Add {
            description: "install nginx".into(),
            payload: r.payload.clone(),
        };
        let report = run(&runner, &lock_path, &r, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert!(report.message.contains("nginx=1.18.0"));
        assert_eq!(runner.count_calls_to("apt-get", "install"), 1);
        let install_args = &runner.calls()[0].1;
        // Версия добавляется как name=version.
        assert!(install_args.iter().any(|a| a == "nginx=1.18.0"));
        // Canonical-флаги.
        assert!(install_args.iter().any(|a| a == "-qy"));
        assert!(install_args
            .iter()
            .any(|a| a.contains("APT::Acquire::Retries=3")));
        assert!(install_args
            .iter()
            .any(|a| a.contains("DPkg::Lock::Timeout=30")));
        // F08: opt-in флаги по умолчанию отсутствуют.
        assert!(!install_args.iter().any(|a| a == "--allow-downgrades"));
        assert!(!install_args
            .iter()
            .any(|a| a == "--allow-change-held-packages"));
    }

    #[test]
    fn apply_install_with_allow_downgrade_adds_flag() {
        let runner = MockRunner::new(vec![cmdres(Some(0), "")]);
        let r = resource_with_flags("nginx", Some("1.18.0"), true, false);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());

        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        run(&runner, &lock_path, &r, &diff, &ctx).unwrap();
        let install_args = &runner.calls()[0].1;
        assert!(install_args.iter().any(|a| a == "--allow-downgrades"));
        assert!(!install_args
            .iter()
            .any(|a| a == "--allow-change-held-packages"));
    }

    #[test]
    fn apply_install_with_allow_change_held_adds_flag() {
        let runner = MockRunner::new(vec![cmdres(Some(0), "")]);
        let r = resource_with_flags("nginx", None, false, true);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());

        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        run(&runner, &lock_path, &r, &diff, &ctx).unwrap();
        let install_args = &runner.calls()[0].1;
        assert!(install_args
            .iter()
            .any(|a| a == "--allow-change-held-packages"));
        assert!(!install_args.iter().any(|a| a == "--allow-downgrades"));
    }

    #[test]
    fn apply_without_version_omits_equal_sign() {
        let runner = MockRunner::new(vec![cmdres(Some(0), "")]);
        let r = resource("curl", None);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let diff = Diff::Add {
            description: "install curl".into(),
            payload: r.payload.clone(),
        };
        run(&runner, &lock_path, &r, &diff, &ctx).unwrap();
        let args = &runner.calls()[0].1;
        assert!(args.iter().any(|a| a == "curl"));
        assert!(args.iter().all(|a| !a.contains("curl=")));
    }

    #[test]
    fn apply_dpkg_interrupted_runs_configure_a_and_retries_install() {
        let runner = MockRunner::new(vec![
            cmdres(
                Some(100),
                "E: dpkg was interrupted, you must manually run 'dpkg --configure -a'",
            ),
            cmdres(Some(0), ""), // dpkg --configure -a
            cmdres(Some(0), ""), // install retry
        ]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        let report = run(&runner, &lock_path, &r, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(runner.count_calls_to("apt-get", "install"), 2);
        assert_eq!(runner.count_calls_to("dpkg", "--configure"), 1);
    }

    #[test]
    fn apply_dpkg_interrupted_then_configure_a_fails_returns_exec_no_install_retry() {
        let runner = MockRunner::new(vec![
            cmdres(Some(100), "E: dpkg was interrupted"),
            cmdres(Some(1), "dpkg: error processing package"),
        ]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        let err = run(&runner, &lock_path, &r, &diff, &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::Exec { .. }));
        // Один install + один configure, без retry install'а.
        assert_eq!(runner.count_calls_to("apt-get", "install"), 1);
        assert_eq!(runner.count_calls_to("dpkg", "--configure"), 1);
    }

    #[test]
    fn apply_candidate_missing_runs_apt_update_and_retries_install() {
        let runner = MockRunner::new(vec![
            cmdres(Some(100), "E: Unable to locate package nginx"),
            cmdres(Some(0), ""), // apt-get update
            cmdres(Some(0), ""), // install retry
        ]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        let report = run(&runner, &lock_path, &r, &diff, &ctx).unwrap();
        assert!(report.changed);
        assert_eq!(runner.count_calls_to("apt-get", "install"), 2);
        assert_eq!(runner.count_calls_to("apt-get", "update"), 1);
    }

    #[test]
    fn apply_other_failure_is_exec_with_no_retry() {
        let runner = MockRunner::new(vec![cmdres(
            Some(100),
            "E: Sub-process /usr/bin/dpkg returned error",
        )]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        let err = run(&runner, &lock_path, &r, &diff, &ctx).unwrap_err();
        match err {
            PrimitiveError::Exec {
                reason,
                exit,
                stderr_excerpt,
            } => {
                assert!(reason.contains("nginx"));
                assert_eq!(exit, Some(100));
                assert!(stderr_excerpt.contains("Sub-process"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(runner.count_calls_to("apt-get", "install"), 1);
    }

    #[test]
    fn apply_writes_stderr_log_on_failure() {
        let runner = MockRunner::new(vec![cmdres(Some(100), "E: generic failure")]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let log_dir = tmp.path().to_path_buf();
        let ctx = make_ctx(log_dir.clone());
        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        let _ = run(&runner, &lock_path, &r, &diff, &ctx);
        let log_path = log_dir.join("apt-install-last-error.log");
        assert!(log_path.exists(), "expected log file at {log_path:?}");
        let body = std::fs::read_to_string(&log_path).unwrap();
        assert!(body.contains("generic failure"));
    }

    #[test]
    fn apply_dpkg_locked_returns_dpkg_locked_with_no_runner_calls() {
        // F03-фикс: probe использует fcntl(F_GETLK), который видит только
        // fcntl-локи. Захватываем lock через python3 (как делает apt/dpkg).
        // Если python3 нет — тест пропускается, потому что без него
        // невозможно эмулировать fcntl-lock из другого процесса.
        use std::io::Read;
        use std::process::Stdio;
        use std::sync::mpsc;
        use std::thread;

        let runner = MockRunner::new(vec![]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());

        let lock_dir = tempfile::tempdir().unwrap();
        let lock_path = lock_dir.path().join("dpkg-lock-frontend");
        std::fs::write(&lock_path, "").unwrap();

        let path_str = lock_path.to_str().unwrap().to_string();
        let script = format!(
            "import fcntl, sys, time\n\
             f = open(r'{path_str}', 'r+')\n\
             fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB)\n\
             sys.stdout.write('locked\\n'); sys.stdout.flush()\n\
             sys.stdin.read()\n"
        );
        let mut child = match std::process::Command::new("python3")
            .args(["-u", "-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                eprintln!("skipping: python3 not available for fcntl-lock holder");
                return;
            }
        };

        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<()>();
        let reader = thread::spawn(move || {
            let mut buf = [0_u8; 32];
            let mut acc = String::new();
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    return;
                }
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                if acc.contains("locked") {
                    let _ = tx.send(());
                    return;
                }
            }
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        let err = run(&runner, &lock_path, &r, &diff, &ctx).unwrap_err();
        match err {
            PrimitiveError::DpkgLocked { .. } => {}
            other => panic!("expected DpkgLocked, got {other:?}"),
        }
        assert!(runner.calls().is_empty());

        drop(child.stdin.take());
        let _ = child.wait();
        let _ = reader.join();
    }

    #[test]
    fn apply_cancel_token_aborts_before_install() {
        let runner = MockRunner::new(vec![]);
        let r = resource("nginx", None);
        let tmp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ApplyCtx::new(
            Instant::now() + Duration::from_secs(600),
            cancel,
            tracing::Span::none(),
            Arc::new(SensitiveStore::new()),
            std::path::PathBuf::from("/tmp"),
            tmp.path().to_path_buf(),
        );
        let (_d, lock_path) = free_lock_path();
        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };
        let err = run(&runner, &lock_path, &r, &diff, &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::Cancelled));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn apply_payload_invalid_returns_invalid_payload_error() {
        let runner = MockRunner::new(vec![]);
        let kind = ResourceKind::from_static("apt.package");
        let id = ResourceId::new(&kind, "broken");
        let r = Resource {
            id,
            kind,
            spec_version: 1,
            payload: serde_json::json!({ "no_name": true }),
            reload_on: Vec::new(),
            depends_on: Vec::new(),
        };
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let diff = Diff::Add {
            description: "x".into(),
            payload: r.payload.clone(),
        };
        let err = run(&runner, &lock_path, &r, &diff, &ctx).unwrap_err();
        assert!(matches!(err, PrimitiveError::InvalidPayload(_)));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn install_success_message_with_version() {
        let s = AptPackageSpec {
            name: "nginx".into(),
            version: Some("1.0".into()),
            timeout_sec: 600,
            allow_downgrade: false,
            allow_change_held: false,
        };
        assert_eq!(install_success_message(&s), "installed nginx=1.0");
    }

    #[test]
    fn install_success_message_without_version() {
        let s = AptPackageSpec {
            name: "curl".into(),
            version: None,
            timeout_sec: 600,
            allow_downgrade: false,
            allow_change_held: false,
        };
        assert_eq!(install_success_message(&s), "installed curl");
    }

    #[test]
    fn compute_resource_deadline_picks_smaller_of_ctx_and_spec() {
        let now = Instant::now();
        let ctx_dl = now + Duration::from_secs(100);
        // spec=10s → меньше ctx; вернёт ~10s.
        let d = compute_resource_deadline(ctx_dl, 10);
        assert!(d <= now + Duration::from_secs(11));
        assert!(d >= now + Duration::from_secs(9));
    }

    #[test]
    fn compute_resource_deadline_caps_at_ctx_deadline() {
        let now = Instant::now();
        let ctx_dl = now + Duration::from_secs(5);
        // spec=600s но ctx=5s → вернёт ctx.
        let d = compute_resource_deadline(ctx_dl, 600);
        assert_eq!(d, ctx_dl);
    }

    #[test]
    fn apply_emits_starting_install_event() {
        use bosun_core::tracing_test_util::{install_global_router, record_events};

        install_global_router();
        let runner = MockRunner::new(vec![cmdres(Some(0), "")]);
        let r = resource("nginx", Some("1.18.0"));
        let tmp = tempfile::tempdir().unwrap();
        let (_d, lock_path) = free_lock_path();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let diff = Diff::Add {
            description: "install".into(),
            payload: r.payload.clone(),
        };

        let events = record_events(|| {
            run(&runner, &lock_path, &r, &diff, &ctx).unwrap();
        });

        assert!(
            events.iter().any(|e| e.contains("starting install")),
            "expected 'starting install' event; got: {events:?}",
        );
    }
}
