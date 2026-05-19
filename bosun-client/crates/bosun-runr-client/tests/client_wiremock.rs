//! Интеграционные тесты `Client` через `wiremock`.
//!
//! wiremock тянет tokio для своего рантайма, но `Client` остаётся синхронным.
//! Архитектура теста:
//! 1. Поднимаем `MockServer` через `wiremock` (async, multi_thread рантайм).
//! 2. Регистрируем ожидаемые `Mock`'и.
//! 3. Sync-вызовы `Client` выполняем через `tokio::task::spawn_blocking` —
//!    это не блокирует основные Tokio-задачи, на которых работает мок-сервер.
//!
//! Используем `multi_thread` flavor, поэтому `spawn_blocking` действительно
//! отдаёт работу в отдельный thread-pool, а не блокирует event loop.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use bosun_runr_client::{Client, RunrError, ServiceStatus, TimerStatus, UnitKind};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Хелпер: создаёт `Client` для адреса мок-сервера с разумным таймаутом.
fn client_for(server: &MockServer) -> Client {
    Client::new(server.uri(), Duration::from_secs(5))
}

/// Выполняет sync-вызов клиента в blocking-пуле, чтобы не блокировать
/// tokio event loop, на котором живёт wiremock.
async fn blocking<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_info_returns_parsed_payload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/daemon/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "runr",
            "version": "0.42.0",
            "started_at": "2026-05-19T00:00:00Z",
            "pid": 1,
            "self_vm_rss_bytes": 50_000_000u64,
            "self_vm_hwm_bytes": 60_000_000u64,
            "memory_vm_rss_bytes": 500_000_000u64,
            "memory_vm_hwm_bytes": 600_000_000u64,
            "cpu_usage_percent": 0.1,
            "features": ["cgroups", "syslog"],
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let info = blocking(move || client.daemon_info()).await.unwrap();

    assert_eq!(info.name, "runr");
    assert_eq!(info.version, "0.42.0");
    assert_eq!(info.features, vec!["cgroups", "syslog"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_reload_posts_empty_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/units/reload"))
        .and(body_json(serde_json::json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "reload-1",
            "accepted_at": "2026-05-19T10:00:00Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.daemon_reload()).await.unwrap();

    assert_eq!(ack.action_id, "reload-1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_start_sends_idempotent_flag() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/services/pg/start"))
        .and(body_json(serde_json::json!({ "idempotent": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-start",
            "accepted_at": "2026-05-19T10:00:01Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.service_start("pg", true))
        .await
        .unwrap();
    assert_eq!(ack.action_id, "act-start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_stop_with_timeout_serializes_humantime() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/services/pg/stop"))
        .and(body_json(serde_json::json!({
            "timeout": "90s",
            "force": false,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-stop",
            "accepted_at": "2026-05-19T10:00:02Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.service_stop("pg", false, Some("90s")))
        .await
        .unwrap();
    assert_eq!(ack.action_id, "act-stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_stop_without_timeout_omits_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/services/pg/stop"))
        .and(body_json(serde_json::json!({ "force": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-stop-force",
            "accepted_at": "2026-05-19T10:00:03Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.service_stop("pg", true, None))
        .await
        .unwrap();
    assert_eq!(ack.action_id, "act-stop-force");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_restart_sends_default_options() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/services/pg/restart"))
        .and(body_json(serde_json::json!({
            "stop": { "force": false },
            "start": { "idempotent": true },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-restart",
            "accepted_at": "2026-05-19T10:00:04Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.service_restart("pg"))
        .await
        .unwrap();
    assert_eq!(ack.action_id, "act-restart");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_reload_posts_empty_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/services/pg/reload"))
        .and(body_json(serde_json::json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-reload",
            "accepted_at": "2026-05-19T10:00:05Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.service_reload("pg")).await.unwrap();
    assert_eq!(ack.action_id, "act-reload");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_start_posts_empty_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/timers/pg-vac/start"))
        .and(body_json(serde_json::json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-tstart",
            "accepted_at": "2026-05-19T10:00:06Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.timer_start("pg-vac"))
        .await
        .unwrap();
    assert_eq!(ack.action_id, "act-tstart");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_stop_posts_empty_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/timers/pg-vac/stop"))
        .and(body_json(serde_json::json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-tstop",
            "accepted_at": "2026-05-19T10:00:07Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.timer_stop("pg-vac")).await.unwrap();
    assert_eq!(ack.action_id, "act-tstop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_enable_with_now_true() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/timers/pg-vac/enable"))
        .and(body_json(serde_json::json!({ "now": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-enable-now",
            "accepted_at": "2026-05-19T10:00:08Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.timer_enable("pg-vac", true))
        .await
        .unwrap();
    assert_eq!(ack.action_id, "act-enable-now");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_disable_with_now_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/timers/pg-vac/disable"))
        .and(body_json(serde_json::json!({ "now": false })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "action_id": "act-disable",
            "accepted_at": "2026-05-19T10:00:09Z",
        })))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let ack = blocking(move || client.timer_disable("pg-vac", false))
        .await
        .unwrap();
    assert_eq!(ack.action_id, "act-disable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_statuses_returns_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Running",
                "pid": 42,
                "restarts": 3,
                "in_state_for_ms": 1000,
                "uptime_ms": 60000,
                "started_at": "2026-05-19T09:00:00Z",
                "autostart": true,
                "memory_rss_anon_bytes": 1024,
                "memory_rss_file_bytes": 512,
                "cpu_usage_percent": 0.5,
            },
            {
                "name": "redis",
                "state": "Stopped",
                "restarts": 0,
                "in_state_for_ms": 5000,
                "downtime_ms": 5000,
                "autostart": false,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            },
        ])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let statuses = blocking(move || client.service_statuses()).await.unwrap();
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0].name, "pg");
    assert_eq!(statuses[0].restarts, 3);
    assert_eq!(statuses[1].state, "Stopped");
    assert_eq!(statuses[1].pid, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_statuses_returns_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/timers/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg-vacuum",
                "state": "Active",
                "next_run": "2026-05-20T03:00:00Z",
                "target_service": "pg-vacuum-runner",
                "enabled": true,
            },
        ])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let statuses = blocking(move || client.timer_statuses()).await.unwrap();
    assert_eq!(statuses.len(), 1);
    let TimerStatus {
        name,
        state,
        next_run,
        target_service,
        enabled,
    } = statuses[0].clone();
    assert_eq!(name, "pg-vacuum");
    assert_eq!(state, "Active");
    assert_eq!(next_run.as_deref(), Some("2026-05-20T03:00:00Z"));
    assert_eq!(target_service, "pg-vacuum-runner");
    assert_eq!(enabled, Some(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn units_list_returns_mixed_kinds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/units"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "name": "pg", "kind": "Service", "state": "Running" },
            { "name": "pg-vac", "kind": "Timer", "state": "Active" },
            {
                "name": "pg-cg",
                "kind": "Cgroup",
                "state": "Active",
                "metrics": {
                    "pressure_some_avg10": 0.1,
                    "pressure_full_avg10": 0.0,
                    "mem_anon": 1024,
                    "mem_file": 2048,
                    "mem_other": 0,
                }
            },
        ])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let units = blocking(move || client.units_list()).await.unwrap();
    assert_eq!(units.len(), 3);
    assert_eq!(units[0].kind, UnitKind::Service);
    assert_eq!(units[1].kind, UnitKind::Timer);
    assert_eq!(units[2].kind, UnitKind::Cgroup);
    assert!(units[2].metrics.is_some());
}

// ---------------------------------------------------------------------------
// Error paths.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_found_404_on_service_endpoint_returns_typed_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/services/ghost/start"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = blocking(move || client.service_start("ghost", true))
        .await
        .unwrap_err();
    match err {
        RunrError::NotFound { kind, name } => {
            assert_eq!(kind, "service");
            assert_eq!(name, "ghost");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_found_404_on_timer_endpoint_returns_typed_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/timers/ghost-timer/enable"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = blocking(move || client.timer_enable("ghost-timer", false))
        .await
        .unwrap_err();
    match err {
        RunrError::NotFound { kind, name } => {
            assert_eq!(kind, "timer");
            assert_eq!(name, "ghost-timer");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_500_returns_api_error_with_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/services/pg/restart"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal boom: cgroup eperm"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = blocking(move || client.service_restart("pg"))
        .await
        .unwrap_err();
    match err {
        RunrError::ApiError { status, body } => {
            assert_eq!(status, 500);
            assert!(body.contains("internal boom"), "got body: {body}");
        }
        other => panic!("expected ApiError, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_404_without_unit_lookup_returns_api_error() {
    // `units_list()` обращается к /api/v1/units и не передаёт unit_lookup
    // в from_ureq. Если runr вернёт 404 на этот endpoint, классификация
    // должна стать ApiError{404}, а не NotFound — иначе мы потеряем status.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/units"))
        .respond_with(ResponseTemplate::new(404).set_body_string("endpoint not registered"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = blocking(move || client.units_list()).await.unwrap_err();
    match err {
        RunrError::ApiError { status, body } => {
            assert_eq!(status, 404);
            assert!(body.contains("endpoint not registered"), "got body: {body}");
        }
        other => panic!("expected ApiError, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_json_returns_bad_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/daemon/info"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = blocking(move || client.daemon_info()).await.unwrap_err();
    match err {
        RunrError::BadResponse(msg) => {
            assert!(msg.contains("not json"), "expected body in msg: {msg}");
        }
        other => panic!("expected BadResponse, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_refused_returns_unavailable() {
    // Свободный, гарантированно никем не слушаемый порт. Используем порт
    // вне обычного диапазона listen, чтобы не зависеть от состояния хоста.
    // `0` подходит как маркер «no listener», но ureq отказывается от него на
    // стороне URL — поэтому используем 1 (port reserved/unbindable) или
    // конкретный высокий не-listen-порт.
    let base = "http://127.0.0.1:1".to_string();
    let client = Client::new(base.clone(), Duration::from_millis(500));
    let err = blocking(move || client.daemon_info()).await.unwrap_err();
    match err {
        RunrError::Unavailable {
            base_url,
            source: _,
        } => {
            assert_eq!(base_url, base);
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_url_trailing_slash_is_normalized() {
    // Если caller передал `http://host:8010/` с trailing slash, конкатенация
    // не должна давать `//api/...`.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/units"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let trailing = format!("{}/", server.uri());
    let client = Client::new(trailing, Duration::from_secs(5));
    let units = blocking(move || client.units_list()).await.unwrap();
    assert!(units.is_empty());
}

// ---------------------------------------------------------------------------
// verify_restart.
// ---------------------------------------------------------------------------

/// Готовый `ServiceStatus` с заданными `pid`, `restarts` и `state`. Хелпер,
/// чтобы тестовые case-ы не дублировали все поля.
fn status_with(name: &str, pid: Option<u32>, restarts: u64, state: &str) -> ServiceStatus {
    ServiceStatus {
        name: name.to_string(),
        state: state.to_string(),
        pid,
        restarts,
        in_state_for_ms: 1000,
        uptime_ms: Some(1000),
        downtime_ms: None,
        next_restart_in_ms: None,
        started_at: Some("2026-05-19T10:00:00Z".to_string()),
        autostart: true,
        memory_rss_anon_bytes: 1024,
        memory_rss_file_bytes: 512,
        cpu_usage_percent: 0.5,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_restart_returns_status_when_pid_changed() {
    let server = MockServer::start().await;
    // PID сменился (42 → 43), state=Running — primary критерий restart'а.
    // Счётчик `restarts` сохраняем тем же, что подчёркивает: real runr на
    // external restart его не двигает.
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Running",
                "pid": 43,
                "restarts": 3u64,
                "in_state_for_ms": 500,
                "uptime_ms": 500,
                "autostart": true,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            }
        ])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let before = status_with("pg", Some(42), 3, "Running");
    let after = blocking(move || {
        bosun_runr_client::verify_restart(
            &client,
            "pg",
            &before,
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
    })
    .await
    .unwrap();
    assert_eq!(after.pid, Some(43));
    assert_eq!(after.state, "Running");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_restart_returns_restart_not_observed_when_pid_unchanged() {
    let server = MockServer::start().await;
    // PID тот же (42 = 42) и счётчик не вырос — runr ничего не пересоздал.
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Running",
                "pid": 42,
                "restarts": 3u64,
                "in_state_for_ms": 1000,
                "autostart": true,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            }
        ])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let before = status_with("pg", Some(42), 3, "Running");
    let err = blocking(move || {
        bosun_runr_client::verify_restart(
            &client,
            "pg",
            &before,
            Duration::from_millis(20),
            Duration::from_millis(200),
        )
    })
    .await
    .unwrap_err();
    match err {
        RunrError::RestartNotObserved { unit } => assert_eq!(unit, "pg"),
        other => panic!("expected RestartNotObserved, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_restart_returns_restart_not_observed_when_state_not_running() {
    let server = MockServer::start().await;
    // PID сменился (42 → 99), но state=Failed — verify не должен считать
    // это успехом: сервис не в Running. Дойдёт до deadline → RestartNotObserved.
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Failed",
                "pid": 99,
                "restarts": 3u64,
                "in_state_for_ms": 100,
                "autostart": true,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            }
        ])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let before = status_with("pg", Some(42), 3, "Running");
    let err = blocking(move || {
        bosun_runr_client::verify_restart(
            &client,
            "pg",
            &before,
            Duration::from_millis(20),
            Duration::from_millis(200),
        )
    })
    .await
    .unwrap_err();
    assert!(matches!(err, RunrError::RestartNotObserved { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_restart_falls_back_to_restarts_increment_when_pid_missing() {
    let server = MockServer::start().await;
    // Mock эмулирует старый runr без `pid` в snapshot'е: PID нет,
    // но счётчик растёт. Fallback на restarts-инкремент должен
    // распознать restart.
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Running",
                "restarts": 4u64,
                "in_state_for_ms": 500,
                "uptime_ms": 500,
                "autostart": true,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            }
        ])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let before = status_with("pg", None, 3, "Running");
    let after = blocking(move || {
        bosun_runr_client::verify_restart(
            &client,
            "pg",
            &before,
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
    })
    .await
    .unwrap();
    assert_eq!(after.restarts, 4);
    assert_eq!(after.state, "Running");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_start_returns_status_when_state_is_running() {
    let server = MockServer::start().await;
    // Свежестартующий сервис: restarts=0, state=Running.
    // verify_start не должен опираться на инкремент restarts.
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Running",
                "pid": 100,
                "restarts": 0u64,
                "in_state_for_ms": 200,
                "uptime_ms": 200,
                "autostart": true,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            }
        ])))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let status = blocking(move || {
        bosun_runr_client::verify_start(
            &client,
            "pg",
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
    })
    .await
    .unwrap();
    assert_eq!(status.state, "Running");
    assert_eq!(status.restarts, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_start_returns_service_start_failed_on_failed_state() {
    let server = MockServer::start().await;
    // Сервис попал в Failed сразу после start — verify_start должен
    // вернуть ошибку немедленно, не дожидаясь deadline'а.
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Failed",
                "restarts": 0u64,
                "in_state_for_ms": 50,
                "autostart": true,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            }
        ])))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let started_at = std::time::Instant::now();
    let err = blocking(move || {
        bosun_runr_client::verify_start(
            &client,
            "pg",
            Duration::from_millis(20),
            // Большой deadline — должны вернуться раньше его, увидев Failed.
            Duration::from_secs(10),
        )
    })
    .await
    .unwrap_err();
    let elapsed = started_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "verify_start не вернулся быстро на Failed: {elapsed:?}",
    );
    match err {
        RunrError::ServiceStartFailed { unit } => assert_eq!(unit, "pg"),
        other => panic!("expected ServiceStartFailed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_start_returns_start_not_observed_on_timeout() {
    let server = MockServer::start().await;
    // Сервис висит в Starting — не Running и не Failed, верифицируем
    // что после deadline возвращается StartNotObserved с last_state.
    Mock::given(method("GET"))
        .and(path("/api/v1/services/statuses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "name": "pg",
                "state": "Starting",
                "restarts": 0u64,
                "in_state_for_ms": 100,
                "autostart": true,
                "memory_rss_anon_bytes": 0,
                "memory_rss_file_bytes": 0,
                "cpu_usage_percent": 0.0,
            }
        ])))
        .mount(&server)
        .await;
    let client = client_for(&server);
    let err = blocking(move || {
        bosun_runr_client::verify_start(
            &client,
            "pg",
            Duration::from_millis(20),
            Duration::from_millis(200),
        )
    })
    .await
    .unwrap_err();
    match err {
        RunrError::StartNotObserved { unit, last_state } => {
            assert_eq!(unit, "pg");
            assert_eq!(last_state, "Starting");
        }
        other => panic!("expected StartNotObserved, got {other:?}"),
    }
}
