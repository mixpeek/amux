//! Keep synchronous maintenance polls off the HTTP/TLS runtime (AMUX-4225).
//!
//! A separate task is not a separate thread: several jobs blocked a Tokio
//! worker for seconds while real worker API calls timed out. The binary owns
//! both runtimes; the registry keeps its existing jobs and cancellation handles.
use std::{future::Future, sync::OnceLock};
use tokio::{runtime::Handle, task::JoinHandle};

static MAINTENANCE: OnceLock<Handle> = OnceLock::new();
/// Its own threads for the jobs measured to hold a runtime thread for seconds
/// (AF-1006). On 2026-10-29 sixteen jobs stopped ticking together, and on
/// 2026-10-31 board-drive held a maintenance thread 2,265 times (avg 2.4 s, max
/// 108 s): steering delivery, heartbeat and the rest share that pool, so its
/// long ticks starved them. These jobs still block their own threads; they no
/// longer block everyone else's.
static HEAVY: OnceLock<Handle> = OnceLock::new();
pub(crate) const HEAVY_JOBS: [&str; 2] = ["board-drive", "orchestrator-runtime"];

pub(crate) fn install(handle: Handle) {
    MAINTENANCE
        .set(handle)
        .expect("maintenance runtime installed once");
}

pub(crate) fn install_heavy(handle: Handle) {
    HEAVY.set(handle).expect("heavy-job runtime installed once");
}

/// The pool a job runs on, for spawning and for the log.
pub(crate) fn pool_for(job: &str) -> &'static str {
    if HEAVY.get().is_some() && HEAVY_JOBS.contains(&job) {
        "heavy"
    } else {
        pool_name()
    }
}

/// [`spawn`] for a named job: a job in [`HEAVY_JOBS`] gets its own threads.
pub(super) fn spawn_for<F>(job: &str, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    spawn_for_on(HEAVY.get(), MAINTENANCE.get(), job, future)
}

fn spawn_for_on<F>(heavy: Option<&Handle>, maintenance: Option<&Handle>, job: &str, future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match heavy {
        Some(h) if HEAVY_JOBS.contains(&job) => h.spawn(future),
        _ => spawn_on(maintenance, future),
    }
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

    /// AF-1006: board-drive holding the shared maintenance pool stalled every
    /// other job. A job in HEAVY_JOBS runs on its own runtime, so it ticks while
    /// the maintenance pool's only thread is held, and any other job does not.
    #[test]
    fn a_heavy_job_runs_on_its_own_threads_while_the_maintenance_pool_is_held() {
        let one = || tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (maintenance, heavy) = (one(), one());
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let (held_tx, held_rx) = mpsc::channel();
        maintenance.spawn(async move {
            held_tx.send(()).unwrap();
            let _ = hold_rx.recv(); // a synchronous hold, as board-drive's lane loop did
        });
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let tx = done_tx.clone();
        let _h = spawn_for_on(Some(heavy.handle()), Some(maintenance.handle()), "board-drive", async move { tx.send("heavy").unwrap(); });
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).ok(), Some("heavy"), "the heavy job ran while maintenance was held");
        let _o = spawn_for_on(Some(heavy.handle()), Some(maintenance.handle()), "steer-deliver", async move { done_tx.send("other").unwrap(); });
        assert!(done_rx.recv_timeout(Duration::from_millis(300)).is_err(), "any other job stays on the maintenance pool");
        hold_tx.send(()).unwrap();
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).ok(), Some("other"));
    }

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
