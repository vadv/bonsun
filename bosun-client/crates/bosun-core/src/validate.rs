//! Контракт исполнителя validate-команд (`validate_with`).
//!
//! Phase H вводит общий механизм валидации перед инвазивным действием:
//! `file.content` рендерит `<path>.new`, запускает validator на этом
//! файле, и только при exit=0 swap'ает в `<path>`. `service.unit`
//! запускает validator (`nginx -t`, `pgbouncer -t` и т.п.) ДО enqueue
//! defer'а restart/reload — провал validator'а означает, что defer не
//! ставится и оператор видит ошибку синхронно.
//!
//! Trait выделен отдельно, чтобы тесты могли подменять spawn без
//! зависимости от системного `nginx`/`pg_doorman`. Production-реализация
//! `RealValidateRunner` использует `std::process::Command` с собственным
//! polling-loop для taймаута (Tokio в этом пути не уместен — apply
//! однопоточен и блокирующий).

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Ошибка запуска validate-команды. Различает три категории, чтобы
/// caller мог сформировать корректный `PrimitiveError::Validation`
/// с конкретным reason.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ValidateError {
    /// Validator завершился с ненулевым exit-code. Включает обрезанный
    /// stderr — оператор увидит конкретную причину в логах и
    /// `bosun status`.
    #[error("validator exited with code {exit_code}: {stderr_excerpt}")]
    ExitNonZero {
        exit_code: i32,
        stderr_excerpt: String,
    },
    /// Validator не успел отработать за отведённый timeout. Дочерний
    /// процесс убит, validator считается провалившимся.
    #[error("validator timed out after {0:?}")]
    Timeout(Duration),
    /// Validator не смог запуститься: бинарь не найден, нет прав на exec,
    /// и т.п. Возвращается до запуска первого byte'а.
    #[error("validator failed to spawn: {0}")]
    Spawn(#[source] std::io::Error),
}

/// Сколько байт stderr оставляем в excerpt. Достаточно для типичного
/// nginx-syntax-error и предотвращает разрастание журнала defers /
/// логов на гигантский traceback.
pub const STDERR_EXCERPT_LIMIT: usize = 4096;

/// Контракт DI-исполнителя. `argv` — полный массив, начиная с пути
/// исполняемого файла. Никакого `sh -c`, никаких глоббингов.
///
/// Реализация обязана:
/// - читать stderr дочернего процесса до конца (чтобы захватить excerpt),
/// - убивать процесс при истечении `timeout`,
/// - возвращать `ExitNonZero` на любой `status.code() != 0`,
///   независимо от того, был ли это normal exit или signal.
pub trait ValidateRunner: Send + Sync {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<(), ValidateError>;
}

/// Production-реализация. Запускает `argv` через `std::process::Command`,
/// поллит `try_wait` с шагом `POLL_INTERVAL` до истечения `timeout`. На
/// таймауте — `Child::kill` и возврат `Timeout`.
#[derive(Default, Debug)]
pub struct RealValidateRunner;

/// Шаг polling-цикла для `try_wait`. 50 мс — компромисс: меньше — лишняя
/// нагрузка на ядро, больше — заметная задержка реакции на завершение
/// быстрого validator'а (типичный nginx -t отвечает за миллисекунды).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

impl ValidateRunner for RealValidateRunner {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<(), ValidateError> {
        let Some((cmd, rest)) = argv.split_first() else {
            // Пустой argv приходит только из теста или сломанного
            // build_payload — на проде уже до spawn'а упадёт InvalidPayload
            // (`validate_with` Vec проверяется на непустоту в primitive).
            return Err(ValidateError::Spawn(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "validate runner: empty argv",
            )));
        };
        let mut command = Command::new(cmd);
        command
            .args(rest)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(ValidateError::Spawn)?;

        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Перед чтением stderr дождёмся wait, чтобы pipe был
                    // полностью drain'нут на стороне child'а. `try_wait`
                    // уже reaped, но stderr-readable end ещё держится.
                    let stderr_excerpt = read_stderr_excerpt(&mut child);
                    let code = status.code().unwrap_or(-1);
                    if status.success() {
                        return Ok(());
                    }
                    return Err(ValidateError::ExitNonZero {
                        exit_code: code,
                        stderr_excerpt,
                    });
                }
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        // Грохаем child; если убийство падает —
                        // прокидываем оригинальную причину (timeout).
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(ValidateError::Timeout(timeout));
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(e) => {
                    // try_wait сам по себе вернул ошибку: это редкий
                    // случай (несуществующий PID, прерванное ядро),
                    // трактуем как Spawn-эквивалент — Validator
                    // не отработал.
                    return Err(ValidateError::Spawn(e));
                }
            }
        }
    }
}

/// Прочитать stderr дочернего процесса и обрезать до
/// `STDERR_EXCERPT_LIMIT` байт. Если чтение упало (например, child
/// закрыл pipe раньше) — возвращаем пустую строку: validator всё равно
/// уже отчитался exit-code, и отсутствие stderr — это не повод
/// падать всем процессом apply'я.
fn read_stderr_excerpt(child: &mut std::process::Child) -> String {
    use std::io::Read as _;
    let Some(stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buf = Vec::with_capacity(STDERR_EXCERPT_LIMIT);
    let _ = stderr
        .take(STDERR_EXCERPT_LIMIT as u64)
        .read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Подставить `{new_path}` в каждом элементе `argv` на реальный path.
/// Любая другая `{...}`-подстрока остаётся as-is: мы НЕ парсим формат
/// шаблонов и не расширяем regex'ы, это узкая точечная функция.
pub fn substitute_new_path(argv: &[String], new_path: &str) -> Vec<String> {
    argv.iter()
        .map(|s| s.replace("{new_path}", new_path))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn real_runner_executes_true_and_returns_ok() {
        let runner = RealValidateRunner;
        let res = runner.run(&["true".to_string()], Duration::from_secs(5));
        assert!(res.is_ok(), "expected Ok from /bin/true, got {res:?}");
    }

    #[test]
    fn real_runner_returns_exit_non_zero_for_false() {
        let runner = RealValidateRunner;
        let err = runner
            .run(&["false".to_string()], Duration::from_secs(5))
            .unwrap_err();
        match err {
            ValidateError::ExitNonZero { exit_code, .. } => {
                assert_eq!(exit_code, 1, "false exit code должен быть 1");
            }
            other => panic!("expected ExitNonZero, got {other:?}"),
        }
    }

    #[test]
    fn real_runner_captures_stderr_excerpt() {
        let runner = RealValidateRunner;
        let err = runner
            .run(
                &[
                    "sh".to_string(),
                    "-c".into(),
                    "echo invalid syntax >&2; exit 1".into(),
                ],
                Duration::from_secs(5),
            )
            .unwrap_err();
        match err {
            ValidateError::ExitNonZero { stderr_excerpt, .. } => {
                assert!(
                    stderr_excerpt.contains("invalid syntax"),
                    "stderr должен содержать 'invalid syntax', got: {stderr_excerpt:?}"
                );
            }
            other => panic!("expected ExitNonZero, got {other:?}"),
        }
    }

    #[test]
    fn real_runner_kills_on_timeout() {
        // sleep 30 — а timeout 200 мс, должны убить.
        let runner = RealValidateRunner;
        let started = Instant::now();
        let err = runner
            .run(
                &["sleep".to_string(), "30".into()],
                Duration::from_millis(200),
            )
            .unwrap_err();
        let elapsed = started.elapsed();
        match err {
            ValidateError::Timeout(d) => {
                assert_eq!(d, Duration::from_millis(200));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
        // Убедимся, что мы не висели полные 30 секунд — должны были
        // убить child'а быстро после timeout (200 мс + poll-step).
        assert!(
            elapsed < Duration::from_secs(5),
            "должны были убить child quickly, прошло {elapsed:?}"
        );
    }

    #[test]
    fn real_runner_empty_argv_returns_spawn_err() {
        let runner = RealValidateRunner;
        let err = runner.run(&[], Duration::from_secs(1)).unwrap_err();
        match err {
            ValidateError::Spawn(_) => {}
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn real_runner_nonexistent_binary_returns_spawn_err() {
        let runner = RealValidateRunner;
        let err = runner
            .run(
                &["__bosun_nonexistent_binary_12345__".to_string()],
                Duration::from_secs(1),
            )
            .unwrap_err();
        match err {
            ValidateError::Spawn(_) => {}
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn substitute_new_path_replaces_placeholder() {
        let argv = vec![
            "nginx".to_string(),
            "-t".into(),
            "-c".into(),
            "{new_path}".into(),
        ];
        let out = substitute_new_path(&argv, "/etc/nginx.conf.new");
        assert_eq!(out, vec!["nginx", "-t", "-c", "/etc/nginx.conf.new"]);
    }

    #[test]
    fn substitute_new_path_inside_string() {
        // Plug в середину аргумента — типа --config={new_path}.
        let argv = vec!["validator".to_string(), "--config={new_path}".into()];
        let out = substitute_new_path(&argv, "/etc/conf.new");
        assert_eq!(out, vec!["validator", "--config=/etc/conf.new"]);
    }

    #[test]
    fn substitute_new_path_leaves_other_placeholders_intact() {
        // Не парсим как шаблонизатор — {path}, {owner}, {anything} остаётся.
        let argv = vec![
            "tool".to_string(),
            "{path}".into(),
            "{owner}".into(),
            "{new_path}".into(),
        ];
        let out = substitute_new_path(&argv, "/x");
        assert_eq!(out, vec!["tool", "{path}", "{owner}", "/x"]);
    }

    #[test]
    fn substitute_new_path_handles_multiple_occurrences() {
        // Защита от регрессии: если оператор задал плейсхолдер дважды,
        // обе подстановки должны произойти.
        let argv = vec![
            "diff".to_string(),
            "{new_path}".into(),
            "{new_path}.bak".into(),
        ];
        let out = substitute_new_path(&argv, "/etc/conf.new");
        assert_eq!(out, vec!["diff", "/etc/conf.new", "/etc/conf.new.bak"]);
    }

    #[test]
    fn substitute_new_path_no_placeholder_passes_through() {
        let argv = vec!["true".to_string()];
        let out = substitute_new_path(&argv, "/x");
        assert_eq!(out, vec!["true"]);
    }
}
