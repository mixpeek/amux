//! `/api/events` — SSE (RR-0023/RR-0042, Invariants 35/26).
//!
//! One stream, three event kinds:
//! - Revisioned StateEvents (`{"type":"state",...}`) + gap detection
//!   (`lagged`) + `hello` carrying the current rev.
//! - `{"type":"invalidate","keys":["board"|"sessions",...]}` — the signal
//!   that replaced the legacy full-list pushes (AMUX-3503). The old dialect
//!   shipped the ENTIRE board (883KB) + sessions (177KB) on every connect
//!   and every coalesced change, RAW — CompressionLayer exempts
//!   text/event-stream — while the client's fetch path was already gzipped
//!   AND ETag'd, so intermittent mobile paid ~1MB per reconnect for data a
//!   conditional fetch serves as a 304. Worse, half the push was DEAD: the
//!   server said `"type":"sessions"`, the client only handled `workers`
//!   (the AF-10 vocabulary-rename class), so 177KB per push was parsed and
//!   dropped for weeks. The client now fetches on invalidate through the
//!   cheap path; a burst of N writes still coalesces to one signal.
//! - The 10s `{"type":"ping"}` keep-alive (the SPA's 18s staleness detector
//!   feeds on it, and its `v` drives stale-shell self-reload — which is
//!   also what migrates an old full-push client off this contract within
//!   seconds of connecting to a new server).

use super::AppState;
use axum::extract::State;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use futures::StreamExt;
use std::convert::Infallible;
use std::time::Duration;

/// LIVE SSE CONNECTIONS, and the cumulative count of opens (AF-262).
///
/// The SSE backbone had NO health signal of any kind, and `/api/events` is
/// deliberately excluded from the request log (it is one long-lived request, so
/// logging it would say nothing about the traffic it carries). So the one
/// component whose documented failure mode is "every client falls back to
/// POLLING" was the one component with nothing to look at.
///
/// That cost a real wrong conclusion on 2026-08-27. A day's `/api/sessions`
/// volume rose 63% with 90% of it unattributed — exactly the shape a fleet-wide
/// SSE degradation produces per `.claude/rules/sse-realtime.md` — and there was
/// no way to tell that from "more browser tabs are open". The latency half of
/// that finding turned out to be wrong for an unrelated reason; the blindness
/// was real either way and survives the correction.
///
/// VOLATILE BY CONSTRUCTION and that is honest here: the builder swaps this
/// binary whenever anyone commits, and every SSE connection dies with it. A
/// gauge that resets on deploy is exactly right for "how many clients are
/// attached to THIS process" — which is the question. `opened_total` is
/// per-process for the same reason, so a high open count against a low live
/// count is reconnect churn within one process's life, which is the signal.
static SSE_LIVE: AtomicI64 = AtomicI64::new(0);
static SSE_OPENED: AtomicU64 = AtomicU64::new(0);

/// Decrements when the RESPONSE STREAM is dropped, which is what axum does on
/// client disconnect.
///
/// Deliberately NOT held by the producer task. The producer parks on
/// `rx.recv()` for the broadcast channel and only discovers a dead client when
/// it next tries to SEND — so on a quiet fleet, which is precisely when you are
/// asking whether clients are still attached, it would keep counting clients
/// that had already gone. The stream is dropped promptly; the producer is not.
struct ConnGuard;

impl ConnGuard {
    fn open() -> Self {
        SSE_LIVE.fetch_add(1, Ordering::Relaxed);
        SSE_OPENED.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        SSE_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// `(live, opened_total)` for the debug surface.
pub fn conn_stats() -> (i64, u64) {
    (SSE_LIVE.load(Ordering::Relaxed), SSE_OPENED.load(Ordering::Relaxed))
}

pub async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.store.subscribe();
    let current = state.store.current_rev().map(|r| r.0).unwrap_or(0);

    let stream = async_stream(current, move |yielder: tokio::sync::mpsc::Sender<Event>| async move {
        // No initial snapshot (AMUX-3503): `hello` above carries the rev, and
        // the client renders from its cache then conditional-fetches — a 304
        // when nothing changed, 176KB gzipped when something did, versus the
        // 1,060KB raw this used to push on every (re)connect.
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let payload = serde_json::json!({
                        "type": "state",
                        "payload": ev,
                    });
                    if yielder
                        .send(Event::default().data(payload.to_string()))
                        .await
                        .is_err()
                    {
                        break; // client went away
                    }
                    // Coalesce this event plus everything already queued:
                    // a burst of N writes = one invalidate signal.
                    let mut board_dirty = matches!(
                        ev.entity_type,
                        amux_core::revision::EntityType::Task
                    );
                    let mut sessions_dirty = matches!(
                        ev.entity_type,
                        amux_core::revision::EntityType::Worker
                            | amux_core::revision::EntityType::Session
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    while let Ok(more) = rx.try_recv() {
                        board_dirty |= matches!(
                            more.entity_type,
                            amux_core::revision::EntityType::Task
                        );
                        sessions_dirty |= matches!(
                            more.entity_type,
                            amux_core::revision::EntityType::Worker
                                | amux_core::revision::EntityType::Session
                        );
                    }
                    if board_dirty || sessions_dirty {
                        let ev = invalidate_payload(board_dirty, sessions_dirty);
                        if yielder.send(Event::default().data(ev)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    // Backpressure (Invariant 26): a slow client that missed
                    // events is TOLD so, with the count — it must delta-sync,
                    // not assume continuity. The invalidate makes the SPA
                    // refetch both lists through the conditional path, which
                    // IS its recovery.
                    let payload = serde_json::json!({
                        "type": "lagged",
                        "missed": missed,
                    });
                    if yielder
                        .send(Event::default().data(payload.to_string()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if yielder
                        .send(Event::default().data(invalidate_payload(true, true)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Sse::new(stream).keep_alive(
        // The keep-alive IS the ping contract: 10s cadence, real data event.
        // `v` = the APP_VER of the app.js THIS binary embeds (Python parity,
        // amux-server.py:65292): the SPA's ping handler self-reloads on
        // mismatch (rate-limited, SW-nudged), which is how clients adopt a
        // new build without a human clicking anything — the server restarts
        // on deploy, the next ping carries the new version, every open
        // window follows. Ethan 2026-08-09: clients restart like the Python
        // server's, not behind a banner.
        // .event(data), NOT .text(): KeepAlive::text() is literally
        // Event::default().comment(t), and an SSE COMMENT never reaches
        // EventSource.onmessage — so the ping parsed right on the wire and
        // was invisible to the client: self-reload unreachable, _lastDataTime
        // starved, and the SPA's 18s zombie detector reconnect-looped on any
        // quiet fleet (browser re-verify, 2026-08-09: 2 comment-pings, 0
        // data-pings in 32s of raw wire). Axum keep-alives fire only after
        // `interval` of SILENCE, unlike Python's unconditional 10s ping —
        // acceptable, because flowing data events feed _lastDataTime
        // themselves; the ping covers exactly the quiet gaps.
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .event(Event::default().data(ping_payload())),
    )
}

/// `{"type":"ping","v":"<APP_VER>"}` with the version parsed ONCE from the
/// embedded app.js — the same file this server serves, so client and ping can
/// never disagree about what "current" means. Falls back to a bare ping if
/// the constant ever moves (a missing `v` disables self-reload, it does not
/// break liveness).
fn ping_payload() -> &'static str {
    static PAYLOAD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PAYLOAD.get_or_init(|| {
        let ver = amux_dashboard::DashboardAssets::get("app.js")
            .and_then(|f| {
                let s = String::from_utf8_lossy(&f.data).into_owned();
                s.split("const APP_VER = '")
                    .nth(1)
                    .and_then(|rest| rest.split('\'').next().map(String::from))
            })
            .unwrap_or_default();
        if ver.is_empty() {
            "{\"type\":\"ping\"}".to_string()
        } else {
            format!("{{\"type\":\"ping\",\"v\":\"{ver}\"}}")
        }
    })
}

/// The invalidate signal (AMUX-3503) — the entire replacement for the legacy
/// full-list pushes. `keys` uses the SPA's existing invalidate vocabulary
/// (`msg.keys`, an array), so the event shape predates this change on the
/// client side. Pure so the contract is pinned by test: this exact JSON is
/// what app.js parses, and a shape drift here silently stops every refetch.
fn invalidate_payload(board: bool, sessions: bool) -> String {
    let mut keys: Vec<&str> = Vec::new();
    if board {
        keys.push("board");
    }
    if sessions {
        keys.push("sessions");
    }
    format!(
        "{{\"type\":\"invalidate\",\"keys\":{}}}",
        serde_json::to_string(&keys).unwrap_or_else(|_| "[]".into())
    )
}

/// Bridge an async producer into an SSE stream, prefixed with a `hello`
/// event carrying the current revision so the client knows exactly where
/// the live stream begins (and what to pass to /api/sync).
fn async_stream<F, Fut>(
    current_rev: u64,
    producer: F,
) -> impl Stream<Item = Result<Event, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Event>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(256);
    let hello = Event::default().data(
        serde_json::json!({"type": "hello", "rev": current_rev}).to_string(),
    );
    tokio::spawn(producer(tx));
    // The guard rides in the STREAM STATE so it drops exactly when axum drops
    // the response body — i.e. on client disconnect (AF-262).
    let guard = ConnGuard::open();
    futures::stream::once(async move { Ok(hello) }).chain(futures::stream::unfold(
        (rx, guard),
        |(mut rx, guard)| async move {
            rx.recv().await.map(|ev| (Ok(ev), (rx, guard)))
        },
    ))
}

#[cfg(test)]
mod ping_tests {
    use super::*;

    /// AF-262 — the gauge must go DOWN, and it must go down when the STREAM is
    /// dropped rather than when the producer notices.
    ///
    /// A counter that only increments is worse than none: it reports a growing
    /// fleet forever and reads as health. The decrement is the whole instrument,
    /// so it is what this pins.
    ///
    /// The placement matters as much as the arithmetic. The obvious home for the
    /// guard is the producer task, and it is WRONG: the producer parks on
    /// `rx.recv()` for the broadcast channel and only discovers a dead client
    /// when it next tries to SEND. On a quiet fleet — precisely when you are
    /// asking whether anyone is still attached — it would keep counting clients
    /// that had already gone, and the gauge would be highest exactly when the
    /// truth was lowest.
    #[test]
    fn the_connection_gauge_decrements_when_the_stream_drops_not_when_the_producer_notices() {
        let (live0, opened0) = conn_stats();

        let g1 = ConnGuard::open();
        let (live1, opened1) = conn_stats();
        assert_eq!(live1, live0 + 1, "open must increment live");
        assert_eq!(opened1, opened0 + 1, "open must increment the cumulative total");

        let g2 = ConnGuard::open();
        assert_eq!(conn_stats().0, live0 + 2, "two streams, two live");

        drop(g2);
        assert_eq!(
            conn_stats().0,
            live0 + 1,
            "dropping one stream must decrement live — a gauge that only counts up \
             reports a growing fleet forever and reads as health"
        );

        drop(g1);
        assert_eq!(conn_stats().0, live0, "live returns to baseline when all streams drop");
        assert_eq!(
            conn_stats().1,
            opened0 + 2,
            "opened_total is CUMULATIVE and must NOT decrement — opened >> live within \
             one process is the reconnect-churn signal, and it disappears if this \
             counter tracks live"
        );
    }

    /// AMUX-3503 — this exact JSON is the ENTIRE replacement for the 1MB
    /// legacy pushes, and app.js parses it as `msg.keys` (an array). A shape
    /// drift here silently stops every SSE-driven refetch fleet-wide, so the
    /// bytes are pinned, not just the semantics. Both-false is the
    /// cannot-happen guard (callers gate on dirty) — pinned anyway so a
    /// future caller cannot ship a keyless invalidate the client ignores.
    #[test]
    fn invalidate_payload_is_the_exact_shape_the_client_parses() {
        assert_eq!(
            super::invalidate_payload(true, true),
            r#"{"type":"invalidate","keys":["board","sessions"]}"#
        );
        assert_eq!(
            super::invalidate_payload(true, false),
            r#"{"type":"invalidate","keys":["board"]}"#
        );
        assert_eq!(
            super::invalidate_payload(false, true),
            r#"{"type":"invalidate","keys":["sessions"]}"#
        );
        assert_eq!(
            super::invalidate_payload(false, false),
            r#"{"type":"invalidate","keys":[]}"#
        );
    }

    #[test]
    fn ping_carries_the_embedded_app_ver() {
        let p = super::ping_payload();
        // The version must be the one inside the EMBEDDED app.js — not a
        // hardcoded copy that drifts on the next client bump.
        let f = amux_dashboard::DashboardAssets::get("app.js").unwrap();
        let s = String::from_utf8_lossy(&f.data).into_owned();
        let ver = s
            .split("const APP_VER = '")
            .nth(1)
            .and_then(|r| r.split('\'').next())
            .expect("APP_VER present in embedded app.js");
        assert_eq!(p, &format!("{{\"type\":\"ping\",\"v\":\"{ver}\"}}"));
    }
}
