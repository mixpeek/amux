//! Keep synchronous maintenance polls off the HTTP/TLS runtime (AMUX-4225).
//!
//! A separate task is not a separate thread: several jobs blocked a Tokio
//! worker for seconds while real worker API calls timed out. The binary owns
//! both runtimes; the registry keeps its existing jobs and cancellation handles.
use std::{future::Future, sync::OnceLock};
use tokio::{runtime::Handle, task::JoinHandle};

static MAINTENANCE: OnceLock<Handle> = OnceLock::new();

pub(crate) fn install(handle: Handle) {
    MAINTENANCE
        .set(handle)
        .expect("maintenance runtime installed once");
}

pub(super) fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    // Library callers and tests retain their caller-owned runtime and teardown.
    // Only run() installs the process-lifetime maintenance runtime.
    spawn_on(MAINTENANCE.get(), future)
}

fn spawn_on<F>(maintenance: Option<&Handle>, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    maintenance
        .cloned()
        .unwrap_or_else(Handle::current)
        .spawn(future)
}

pub(crate) fn pool_name() -> &'static str {
    if MAINTENANCE.get().is_some() {
        "maintenance"
    } else {
        "caller"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{atomic::AtomicBool, mpsc, Arc},
        time::{Duration, Instant},
    };

    #[test]
    fn blocked_maintenance_poll_does_not_delay_health_or_worker_api() {
        let runtime = || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap()
        };
        let maintenance = runtime();
        let http = runtime();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::api::AppState {
            store: Arc::new(crate::db::Store::open(&dir.path().join("runtime.db")).unwrap()),
            started: Instant::now(),
            build_hash: "isolated-runtime".into(),
            auth_token: None,
            reconciled: Arc::new(AtomicBool::new(true)),
        };
        let (began_tx, began_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        // Spawn from the HTTP runtime, just as async_main calls the registry.
        // A finite block also makes the old, shared-runtime control terminate.
        let job = {
            let _entered = http.enter();
            spawn_on(Some(maintenance.handle()), async move {
                began_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(2));
            })
        };
        began_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        // Spawn onto its sole worker. block_on itself can poll on the test
        // thread, which would hide starvation if we awaited the handler there.
        let (status, measured, worker_api) = http
            .block_on(http.spawn(async move {
                use axum::{body::Body, http::Request};
                use tower::ServiceExt;
                let (status, axum::Json(body)) =
                    crate::api::health::health(axum::extract::State(state.clone())).await;
                let response = crate::api::router(state)
                    .oneshot(
                        Request::builder()
                            .uri("/api/board")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                (status, body.board.measured, response.status())
            }))
            .unwrap();
        let elapsed = started.elapsed();
        let _ = release_tx.send(());
        http.block_on(job).unwrap();
        assert!(
            elapsed < Duration::from_millis(500),
            "maintenance blocked HTTP for {elapsed:?}"
        );
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(measured);
        assert_eq!(worker_api, axum::http::StatusCode::OK);
    }
}
