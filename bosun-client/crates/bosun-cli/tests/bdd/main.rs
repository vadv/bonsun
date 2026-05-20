//! BDD-раннер для bosun-cli.
//!
//! Запуск:
//!   cargo test -p bosun-cli --release --test bdd
//!   cargo test -p bosun-cli --release --test bdd -- --tags @apt-package
//!
//! Через Makefile:
//!   make test-bdd
//!   make test-bdd TAGS=@apt-package
//!
//! Требуется docker. Бэйс-образ собирается `make docker-base`.
//!
//! Stoppers: тесты падают, если scenario был skipped (включая шаги через
//! `World::skip()`). `@todo-skip` отфильтровывается до запуска.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod assertions_helper;
mod bundle_helper;
mod cert_helper;
mod defers_helper;
mod docker_helper;
mod runr_helper;
mod systemd_helper;
mod users_helper;
mod validate_helper;
mod world;

use cucumber::gherkin::tagexpr::TagOperation;
use cucumber::World;

use crate::docker_helper::{docker_kill, locate_bosun_binary};
use crate::world::BosunWorld;

fn main() {
    if std::env::var("DEBUG").is_ok() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_target(true)
            .init();
    }

    let bin = locate_bosun_binary().unwrap_or_else(|e| {
        eprintln!("BDD setup error: {e}");
        std::process::exit(1);
    });
    eprintln!("BDD: using bosun binary {}", bin.display());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut cli = cucumber::cli::Opts::<
            cucumber::parser::basic::Cli,
            cucumber::runner::basic::Cli,
            cucumber::writer::basic::Cli,
            cucumber::cli::Empty,
        >::parsed();

        // Filter `not @todo-skip` всегда добавляется автоматически.
        let not_todo = TagOperation::Not(Box::new(TagOperation::Tag("todo-skip".to_string())));
        cli.tags_filter = match cli.tags_filter.take() {
            Some(existing) => Some(TagOperation::And(Box::new(existing), Box::new(not_todo))),
            None => Some(not_todo),
        };

        // `@systemd-privileged` сценарии требуют privileged docker
        // (systemd как PID 1). По умолчанию они отфильтрованы. Включить
        // ровно их — через `make test-bdd-systemd`, который выставляет
        // BDD_SYSTEMD_PRIVILEGED=1 и `--tags @systemd-privileged`.
        if std::env::var("BDD_SYSTEMD_PRIVILEGED").is_err() {
            let not_priv = TagOperation::Not(Box::new(TagOperation::Tag(
                "systemd-privileged".to_string(),
            )));
            cli.tags_filter = match cli.tags_filter.take() {
                Some(existing) => Some(TagOperation::And(Box::new(existing), Box::new(not_priv))),
                None => Some(not_priv),
            };
        }

        let bin_for_init = bin.clone();
        let writer = BosunWorld::cucumber()
            .max_concurrent_scenarios(1)
            .fail_fast()
            .with_cli(cli)
            .before(move |feature, _rule, scenario, world| {
                let scenario_name = scenario.name.clone();
                let feature_name = feature.name.clone();
                let bin = bin_for_init.clone();
                Box::pin(async move {
                    world.bosun_binary_path = bin;

                    // Slow-warning: пишет каждую минуту, что сценарий ещё
                    // выполняется. Отменяется в After-хуке.
                    let slow_warn = tokio::spawn(async move {
                        let mut elapsed = 0u64;
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                            elapsed += 1;
                            eprintln!(
                                "[SLOW] scenario '{scenario_name}' (feature '{feature_name}') still running after {elapsed}m",
                            );
                        }
                    });
                    world.slow_warning_abort = Some(slow_warn.abort_handle());
                })
            })
            .after(|_feature, _rule, _scenario, _finished, world| {
                Box::pin(async move {
                    if let Some(w) = world {
                        if let Some(handle) = w.slow_warning_abort.take() {
                            handle.abort();
                        }
                        if let Some(id) = w.container_id.take() {
                            docker_kill(&id);
                        }
                        // bundle_tmp очищается через Drop при сбросе World.
                    }
                })
            })
            .run(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/bdd/features"))
            .await;

        use cucumber::writer::Stats;
        let failed = writer.execution_has_failed();
        let failed_steps = writer.failed_steps();
        let skipped = writer.skipped_steps();
        let parsing_errors = writer.parsing_errors();
        let hook_errors = writer.hook_errors();

        // User explicit: «падение при скипах».
        if failed || skipped > 0 || parsing_errors > 0 || hook_errors > 0 {
            let mut msgs: Vec<String> = Vec::new();
            if failed_steps > 0 {
                msgs.push(format!("{failed_steps} failed steps"));
            }
            if skipped > 0 {
                msgs.push(format!("{skipped} skipped steps"));
            }
            if parsing_errors > 0 {
                msgs.push(format!("{parsing_errors} parsing errors"));
            }
            if hook_errors > 0 {
                msgs.push(format!("{hook_errors} hook errors"));
            }
            eprintln!("BDD failed: {}", msgs.join(", "));
            std::process::exit(1);
        }
    });
}
