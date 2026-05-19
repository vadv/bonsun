//! `wait_for_job` implementation: subscribe to `Manager.JobRemoved`, match
//! the matching object path, time out via `tokio::time::timeout`.
//!
//! Isolated in its own module so the matching loop can be unit-tested with a
//! mock stream that yields synthetic `JobRemoved` events. The real bus
//! signal stream comes from `ManagerProxy::receive_job_removed`; the mock
//! stream comes from a `tokio::sync::mpsc::Receiver` channel — both
//! conform to `futures_util::Stream<Item = JobEvent>`.

use std::time::Duration;

use futures_util::stream::Stream;
use futures_util::StreamExt;

use crate::error::SystemdError;

/// Decoded contents of a single `JobRemoved` signal that we care about.
/// The `id` parameter from the wire is ignored: matching is by `job` object
/// path, which is what the corresponding `restart_unit` etc. call returns.
///
/// `unit` is kept for diagnostics / future filtering even though the match
/// loop only uses `job_path`.
#[derive(Debug, Clone)]
pub(crate) struct JobEvent {
    /// `o` parameter of `JobRemoved` — the job object path.
    pub job_path: String,
    /// `s` parameter — `"done" | "canceled" | "timeout" | "failed" | ...`.
    pub result: String,
    /// `s` parameter — name of the unit the job belonged to.
    #[allow(dead_code)]
    pub unit: String,
}

/// Drain the stream until either a matching event or the timeout fires.
///
/// The caller wires the actual stream — production code passes a stream
/// adapted from `ManagerProxy::receive_job_removed`, tests pass an mpsc
/// stream. Either way the loop logic is the same: skip events whose
/// `job_path` does not match `target`; once a match arrives, return it.
///
/// Timeout: implemented via `tokio::time::timeout`, so the function is
/// usable both with real and `tokio::time::pause()`-driven test time.
pub(crate) async fn wait_for_job_match<S>(
    stream: S,
    target_job_path: &str,
    timeout: Duration,
) -> Result<JobEvent, SystemdError>
where
    S: Stream<Item = JobEvent> + Unpin,
{
    let fut = async {
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            if ev.job_path == target_job_path {
                return Ok(ev);
            }
            // Foreign events keep arriving; that's normal on a busy bus.
            // Continue polling.
        }
        // The stream ended before we saw our event — the connection was
        // dropped. Surface as a generic "no observation" timeout-equivalent.
        Err(SystemdError::Timeout(timeout))
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(SystemdError::Timeout(timeout)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use futures_util::stream;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;

    // A small helper: build a stream of pre-canned events.
    fn fixed_stream(events: Vec<JobEvent>) -> impl Stream<Item = JobEvent> + Unpin {
        Box::pin(stream::iter(events))
    }

    #[tokio::test]
    async fn matches_by_object_path_skipping_others() {
        let events = vec![
            JobEvent {
                job_path: "/org/freedesktop/systemd1/job/100".to_string(),
                result: "done".to_string(),
                unit: "other.service".to_string(),
            },
            JobEvent {
                job_path: "/org/freedesktop/systemd1/job/42".to_string(),
                result: "done".to_string(),
                unit: "nginx.service".to_string(),
            },
        ];
        let stream = fixed_stream(events);
        let got = wait_for_job_match(
            stream,
            "/org/freedesktop/systemd1/job/42",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(got.unit, "nginx.service");
        assert_eq!(got.result, "done");
    }

    #[tokio::test]
    async fn returns_first_matching_skipping_later_match() {
        // Two events match by path — we must return the first.
        let events = vec![
            JobEvent {
                job_path: "/org/freedesktop/systemd1/job/42".to_string(),
                result: "done".to_string(),
                unit: "nginx.service".to_string(),
            },
            JobEvent {
                job_path: "/org/freedesktop/systemd1/job/42".to_string(),
                result: "failed".to_string(),
                unit: "nginx.service".to_string(),
            },
        ];
        let stream = fixed_stream(events);
        let got = wait_for_job_match(
            stream,
            "/org/freedesktop/systemd1/job/42",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        // The first matching event is `done`, so that wins.
        assert_eq!(got.result, "done");
    }

    #[tokio::test]
    async fn returns_timeout_when_no_match_arrives() {
        // Drive time forward so the timeout fires deterministically.
        tokio::time::pause();

        // Stream yields nothing, but the channel sender is held → no end-of-stream.
        let (_tx, rx) = mpsc::channel::<JobEvent>(8);
        let stream = ReceiverStream::new(rx);

        let join = tokio::spawn(async move {
            wait_for_job_match(
                stream,
                "/org/freedesktop/systemd1/job/42",
                Duration::from_millis(500),
            )
            .await
        });

        tokio::time::advance(Duration::from_millis(600)).await;
        let res = join.await.unwrap();
        match res {
            Err(SystemdError::Timeout(d)) => {
                assert_eq!(d, Duration::from_millis(500));
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_timeout_when_stream_drops_before_match() {
        // Build a stream that ends without producing the wanted event.
        let events = vec![JobEvent {
            job_path: "/org/freedesktop/systemd1/job/100".to_string(),
            result: "done".to_string(),
            unit: "other.service".to_string(),
        }];
        let stream = fixed_stream(events);
        let res = wait_for_job_match(
            stream,
            "/org/freedesktop/systemd1/job/42",
            Duration::from_secs(1),
        )
        .await;
        match res {
            Err(SystemdError::Timeout(_)) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }
}
