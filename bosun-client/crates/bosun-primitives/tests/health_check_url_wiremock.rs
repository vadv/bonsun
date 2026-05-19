//! Integration-тест `RealHealthCheckRunner` (Url-вариант) через wiremock.
//!
//! Поднимает мок-сервер, отдаёт заданные статусы по очереди и проверяет:
//! - 200 первой же попытки → Ok.
//! - 500 N раз → UrlBadStatus с правильным `attempts`.
//! - 500, 500, 200 → Ok за 3 попытки.
//! - Несуществующий URL → UrlTransport.
//!
//! wiremock требует tokio runtime, поэтому используем `#[tokio::test]` с
//! multi_thread. Sync вызовы runner'а — через `spawn_blocking`.

#![allow(clippy::unwrap_used, clippy::panic)]

use bosun_core::{HealthCheck, HealthCheckError, HealthCheckRunner};
use bosun_primitives::RealHealthCheckRunner;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn blocking<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap()
}

fn url_check(url: String, expected: u16, retries: u32) -> HealthCheck {
    HealthCheck::Url {
        url,
        expected_status: Some(expected),
        timeout_sec: Some(2),
        retry_count: Some(retries),
        retry_interval_sec: Some(0),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_200_first_attempt_returns_ok() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let url = format!("{}/healthz", server.uri());

    let res = blocking(move || {
        let runner = RealHealthCheckRunner::new();
        let cancel = CancellationToken::new();
        runner.run(&url_check(url, 200, 3), &cancel)
    })
    .await;
    assert!(res.is_ok(), "200 → Ok, got {res:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_500_exhausts_retries_returns_bad_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let url = format!("{}/healthz", server.uri());

    let res = blocking(move || {
        let runner = RealHealthCheckRunner::new();
        let cancel = CancellationToken::new();
        runner.run(&url_check(url, 200, 3), &cancel)
    })
    .await;
    match res {
        Err(HealthCheckError::UrlBadStatus {
            actual,
            expected,
            attempts,
            ..
        }) => {
            assert_eq!(actual, 500);
            assert_eq!(expected, 200);
            assert_eq!(attempts, 3);
        }
        other => panic!("expected UrlBadStatus, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_500_500_200_succeeds_on_third_attempt() {
    let server = MockServer::start().await;
    // wiremock не имеет очереди ответов из коробки, но `.expect()` плюс
    // несколько Mock'ов с `.up_to_n_times(n)` дают нужное поведение:
    // первые 2 запроса → 500, остальные → 200.
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let url = format!("{}/healthz", server.uri());

    let res = blocking(move || {
        let runner = RealHealthCheckRunner::new();
        let cancel = CancellationToken::new();
        runner.run(&url_check(url, 200, 3), &cancel)
    })
    .await;
    assert!(
        res.is_ok(),
        "500, 500, 200 → Ok после 3-х попыток, got {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_204_with_expected_204_returns_ok() {
    // Не все сервисы отвечают 200. Если оператор указал 204, мы должны
    // принимать именно его.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let url = format!("{}/healthz", server.uri());

    let res = blocking(move || {
        let runner = RealHealthCheckRunner::new();
        let cancel = CancellationToken::new();
        runner.run(&url_check(url, 204, 1), &cancel)
    })
    .await;
    assert!(res.is_ok(), "204 с expected=204 → Ok, got {res:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_404_with_expected_200_returns_bad_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let url = format!("{}/healthz", server.uri());

    let res = blocking(move || {
        let runner = RealHealthCheckRunner::new();
        let cancel = CancellationToken::new();
        runner.run(&url_check(url, 200, 1), &cancel)
    })
    .await;
    match res {
        Err(HealthCheckError::UrlBadStatus {
            actual, attempts, ..
        }) => {
            assert_eq!(actual, 404);
            assert_eq!(attempts, 1);
        }
        other => panic!("expected UrlBadStatus, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_no_listener_returns_transport() {
    // 127.0.0.1:1 — почти гарантированно никто не слушает. Это другой
    // путь, чем wiremock: проверяем transport-ошибку (connection refused).
    let res = blocking(|| {
        let runner = RealHealthCheckRunner::new();
        let cancel = CancellationToken::new();
        runner.run(
            &HealthCheck::Url {
                url: "http://127.0.0.1:1/h".to_string(),
                expected_status: Some(200),
                timeout_sec: Some(1),
                retry_count: Some(2),
                retry_interval_sec: Some(0),
            },
            &cancel,
        )
    })
    .await;
    match res {
        Err(HealthCheckError::UrlTransport { attempts, .. }) => {
            assert_eq!(attempts, 2);
        }
        other => panic!("expected UrlTransport, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn url_default_expected_status_is_200() {
    // Когда expected_status = None, по дефолту ожидаем 200.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let url = format!("{}/healthz", server.uri());

    let res = blocking(move || {
        let runner = RealHealthCheckRunner::new();
        let cancel = CancellationToken::new();
        runner.run(
            &HealthCheck::Url {
                url,
                expected_status: None,
                timeout_sec: Some(1),
                retry_count: Some(1),
                retry_interval_sec: Some(0),
            },
            &cancel,
        )
    })
    .await;
    assert!(
        res.is_ok(),
        "default expected_status=200 → Ok при 200, got {res:?}"
    );
}
