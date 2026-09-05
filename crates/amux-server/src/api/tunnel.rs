//! `/api/tunnel` — the client controls for the cloud tunnel relay (AMUX-2888).
//!
//! The relay itself is `runtime_jobs::tunnel`. This module is the HTTP surface
//! the dashboard's tunnel panel (`app.js` `_tunnelStatus` /
//! `_tunnelSettingsStart` / `_tunnelSettingsStop`) and the `amux tunnel` CLI
//! (`cmd_tunnel`) already call.
//!
//! HISTORY, because the shape of this file is the argument. The routes were
//! never mounted while the callers stayed, so `GET /api/tunnel/status` 404'd
//! and the POSTs 405'd (the SPA's GET-only catch-all answering a non-GET). A
//! caller could not tell any of that from "amux is broken". `c703c34b` mounted
//! them with an honest 501 on the actions and a real 200 on status; this
//! commit replaces the 501s with the ported relay.
//!
//! `start` is the one route here with a REFUSAL rather than a failure:
//! tunnelling amux's own port publishes an unauthenticated control plane, so it
//! answers 400 with the override named. That is a 400 and not a 500 because
//! nothing is broken — amux is declining.

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
        "proxy_id": s.proxy_id,
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

async fn start(State(_state): State<AppState>, body: Option<Json<Value>>) -> Response {
    let port = body
        .as_ref()
        .and_then(|Json(b)| b.get("port").cloned())
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .and_then(|p| u16::try_from(p).ok());
    match tun::start(port).await {
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
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "error": e }))).into_response(),
    }
}

async fn stop(State(_state): State<AppState>) -> Response {
    tun::stop();
    tun::set_proxy_id(None);
    (StatusCode::OK, Json(json!({ "ok": true, "running": false }))).into_response()
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
        assert!(b["configured"].is_boolean(), "configured must be a measurement: {b}");
        assert_eq!(b["ported"], json!(true), "the relay is ported now (AMUX-2888)");
        // The policy the Proxies tab renders must come from the same place the
        // limiter reads, or the UI shows a cap the relay does not enforce.
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
        let e = tun::start(Some(3000)).await.expect_err("no token must refuse");
        assert!(e.contains("AMUX_TUNNEL_TOKEN"), "name the var to set: {e}");
    }
}
