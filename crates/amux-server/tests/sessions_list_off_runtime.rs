//! `GET /api/sessions` must not build its payload on the async runtime (AF-300).
//!
//! `legacy_sessions_array` is synchronous and shells out — `tmux list-sessions`,
//! two `tmux list-panes`, and a `pgrep` per session. Awaiting it inline blocks a
//! tokio WORKER thread, and there are only as many of those as CPUs.
//!
//! THE INCIDENT: 2026-08-28, eight concurrent requests from one iPhone each ran
//! ~696s (18:17:19 -> 18:28:58). While they held worker threads every other read
//! failed `timed out waiting for connection` at 30s — 22 x 500 across
//! /api/sessions, /api/board, /api/board/statuses and /api/board/session-gates,
//! every one a dashboard UA. Same burst on 08-24 (29 rows). This is the
//! most-polled endpoint in the system (81,935 requests in 24h).
//!
//! Asserted on the SOURCE rather than by timing, deliberately: a timing test for
//! "the runtime stayed responsive" needs a controllable hang in a subprocess and
//! would be the flakiest test in the suite. The property that actually regressed
//! is textual — someone calls the sync builder inline again — and that is
//! exactly what this catches.

const SRC: &str = include_str!("../src/api/sessions_legacy.rs");

#[test]
fn the_sessions_handler_builds_off_the_async_runtime() {
    // CONTROL FIRST: if these move or get renamed the assertions below would
    // pass vacuously against a file that no longer contains the thing at all.
    assert!(
        SRC.contains("pub async fn list_sessions_legacy"),
        "premise gone: the handler is not in this file any more, so nothing below is testing it"
    );
    assert!(
        SRC.contains("pub fn legacy_sessions_array"),
        "premise gone: the sync builder is not in this file any more"
    );

    assert!(
        SRC.contains("spawn_blocking(move || legacy_sessions_array(&store))"),
        "the handler must build via spawn_blocking. api/graph.rs already calls this same \
         function this same way; the fix existed on the low-traffic caller and was missing \
         on the hot one"
    );
    assert!(
        !SRC.contains("match legacy_sessions_array(&state.store)"),
        "the handler is calling the SYNC builder inline again — that blocks a tokio worker \
         thread on `tmux`, which is what took the dashboard down for minutes on 2026-08-24 \
         and 2026-08-28 (AF-300)"
    );
}
