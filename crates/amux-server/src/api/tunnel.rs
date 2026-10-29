//! `/api/tunnel` — the controls for the ChatGPT app's relay (AMUX-2888,
//! AMUX-5396).
//!
//! The relay itself is `runtime_jobs::tunnel`. The dashboard's ChatGPT app
//! panel (`app.js` `_chatgptPublish`) starts it with `{"mcp":true}`.
//!
//! Publishing an arbitrary local port through this route is retired with the
//! Proxies tab and `/proxy` (MSG-95893). A start without `mcp` answers 410
//! and names the Apps tab, so an old caller learns where the feature went.
//!
//! HISTORY, because the shape of this file is the argument. The routes were
//! never mounted while the callers stayed, so `GET /api/tunnel/status` 404'd
//! and the POSTs 405'd (the SPA's GET-only catch-all answering a non-GET). A
//! caller could not tell any of that from "amux is broken". `c703c34b` mounted
//! them with an honest 501 on the actions and a real 200 on status; this
//! commit replaces the 501s with the ported relay.
//!
//! A missing token is a REFUSAL rather than a failure, so it answers 400:
//! nothing is broken, amux is declining with a remedy.

use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::runtime_jobs::tunnel as tun;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/start", post(start))
        .route("/stop", post(stop))
}

async fn status(State(_state): State<AppState>) -> Response {
    (StatusCode::OK, Json(status_body())).into_response()
}

/// The status FACTS, split out so the test can assert them (ethos rule 7). The
/// handler needs an `AppState` a unit test has no business constructing, and a
/// test that only exercised the failure arm while being NAMED for this one is
/// the overclaiming test this repo keeps finding.
///
/// `running: false` with `configured: false` and `running: false` with a token
/// set are different situations, so both facts ship together — the 404 this
/// module replaced could express neither.
fn status_body() -> Value {
    let s = tun::snapshot();
    json!({
        "ok": true,
        "running": s.running,
        "url": s.url,
        "tid": s.tid,
        "target": s.target,
        "requests": s.requests,
        "dropped": tun::dropped(),
        "error": s.error,
        "mcp_only": true,
        "mcp_url": s.url.as_deref().map(|u| format!("{}/mcp", u.trim_end_matches('/'))),
        "mcp_paths_refused": tun::mcp_refused(),
        "configured": tun::configured(),
        "gateway": tun::gateway(),
        "rate_per_min": tun::rate_per_min(),
        "max_concurrent": tun::max_concurrent(),
        // Kept from the interim body: `ported` was `false` for two weeks and a
        // client may still be reading it. It is now true, which is the honest
        // value and also the migration signal for anything that branched on it.
        "ported": true,
    })
}

/// What a start without `mcp` answers: the general relay is gone.
const RETIRED: &str = "publishing a local port is retired: add the app on the Apps tab instead";

async fn start(State(_state): State<AppState>, body: Option<Json<Value>>) -> Response {
    let mcp = body.as_ref().and_then(|Json(b)| b.get("mcp")).and_then(Value::as_bool).unwrap_or(false);
    if !mcp {
        tracing::info!(verdict = "tunnel_port_start_retired", "tunnel: refused a general start (retired, see the Apps tab)");
        return (StatusCode::GONE, Json(json!({ "ok": false, "error": RETIRED, "use": "/#view=apps" }))).into_response();
    }
    let started = tun::start_mcp().await;
    if started.is_ok() {
        // Survive restarts: maybe_boot_start reads this (see the marker's doc).
        if let Err(e) = std::fs::write(tun::mcp_published_marker(), b"1\n") {
            tracing::warn!(error = %e, verdict = "tunnel_mcp_marker_unwritten", "tunnel: connector published but will not survive a restart");
        }
    }
    match started {
        Ok(s) => (
            StatusCode::OK,
            Json(json!({
                "ok": true, "running": s.running, "url": s.url, "error": s.error,
            })),
        )
            .into_response(),
        // 400, not 500: the self-port case and the missing-token case are both
        // amux DECLINING with a stated remedy, and a 5xx would file them as
        // faults with the automated detector.
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

async fn stop(State(_state): State<AppState>) -> Response {
    tun::stop();
    let _ = std::fs::remove_file(tun::mcp_published_marker());
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "running": false })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status must STATE a fact rather than refuse, and the facts must include
    /// the two that are different situations wearing the same `running: false`:
    /// no token at all, versus configured and simply not started.
    #[test]
    fn status_states_the_live_facts_including_whether_a_tunnel_is_possible() {
        let b = status_body();
        assert_eq!(b["ok"], json!(true));
        assert_eq!(b["running"], json!(false), "nothing started in a unit test");
        assert!(
            b["configured"].is_boolean(),
            "configured must be a measurement: {b}"
        );
        assert_eq!(
            b["ported"],
            json!(true),
            "the relay is ported now (AMUX-2888)"
        );
        // The policy reported must come from the same place the limiter
        // reads, or the report shows a cap the relay does not enforce.
        assert_eq!(b["rate_per_min"], json!(tun::rate_per_min()));
        assert_eq!(b["max_concurrent"], json!(tun::max_concurrent()));
        assert_eq!(b["gateway"], json!(tun::gateway()));
        // CONTROL: counters are present and numeric even at rest. A status that
        // omitted them until the first request would make "up and serving
        // nothing" indistinguishable from "up and shedding everything".
        assert_eq!(b["requests"], json!(0));
        assert!(b["dropped"].is_number(), "{b}");
    }

    /// Starting without a token is a REFUSAL with a remedy, not a fault. The
    /// test asserts the sentence names the env var, because a refusal a caller
    /// cannot act on is the same dead end as the 404 this replaced.
    #[tokio::test]
    async fn starting_with_no_token_refuses_and_says_what_to_set() {
        if tun::configured() {
            // A machine with a real token cannot exercise this arm honestly,
            // and pretending otherwise is the overclaiming test this repo keeps
            // finding. Say so rather than asserting nothing.
            eprintln!("AMUX_TUNNEL_TOKEN is set here; the no-token arm is not exercised");
            return;
        }
        let e = tun::start_mcp()
            .await
            .expect_err("no token must refuse");
        assert!(e.contains("AMUX_TUNNEL_TOKEN"), "name the var to set: {e}");
    }
}
