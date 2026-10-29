//! `/api/sync?since_rev=N` — delta sync (RR-0024, Invariant 35).
//!
//! Returns every StateEvent after revision N, bounded. When the journal no
//! longer holds that window, `full_sync_required: true` — a delta that
//! cannot be complete must say so, never silently omit (Invariant 40).

use super::AppState;
use amux_core::revision::{StateEvent, StateRevision};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

/// Response cap: bounded payloads by design. The Python board's 6MB
/// unbounded responses are the L1 lesson; every list endpoint here is
/// bounded and announces truncation.
const MAX_EVENTS: usize = 2000;

#[derive(Deserialize)]
pub struct SyncParams {
    #[serde(default)]
    pub since_rev: u64,
}

#[derive(Serialize)]
pub struct SyncResponse {
    pub rev: u64,
    pub events: Vec<StateEvent>,
    pub full_sync_required: bool,
    /// True when MAX_EVENTS truncated the answer — the client calls again
    /// with the last event's rev.
    pub more: bool,
}

pub async fn delta_sync(
    State(state): State<AppState>,
    Query(params): Query<SyncParams>,
) -> Response {
    let store = state.store.clone();
    let since = StateRevision(params.since_rev);
    let result = match tokio::task::spawn_blocking(move || {
        let current = store.current_rev()?;
        let (events, full_sync_required) = store.events_since(since, MAX_EVENTS + 1)?;
        anyhow::Ok((current, events, full_sync_required))
    })
    .await
    {
        Ok(Ok(r)) => r,
        // A store error here is the read pool or the database refusing in the
        // moment (AMUX-6585: 24 empty-bodied 500s in two minutes during a
        // runtime stall, with nothing logged). Say what it is, 503 + Retry-After,
        // and leave a line a sweep can count. The client re-syncs on its next poll.
        Ok(Err(e)) => return store_busy(&e.to_string()),
        Err(e) => {
            tracing::warn!(verdict = "delta_sync_task_failed", error = %e, "delta sync task did not complete");
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "delta sync task failed"})))
                .into_response();
        }
    };

    let (current, mut events, full_sync_required) = result;
    let more = events.len() > MAX_EVENTS;
    events.truncate(MAX_EVENTS);
    Json(SyncResponse {
        rev: current.0,
        events,
        full_sync_required,
        more,
    })
    .into_response()
}

fn store_busy(error: &str) -> Response {
    tracing::warn!(verdict = "delta_sync_store_busy", error, measured = true, n_considered = 1,
        "delta sync could not read the store; answered 503 so the client retries");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(axum::http::header::RETRY_AFTER, "2")],
        Json(serde_json::json!({"error": "store busy; retry", "detail": error, "retryable": true})),
    )
        .into_response()
}

#[cfg(test)]
mod store_busy_tests {
    /// AMUX-6585: a store error is an honest, retryable 503 with a body.
    #[tokio::test]
    async fn a_store_error_answers_503_with_a_reason() {
        let r = super::store_busy("timed out waiting for connection");
        assert_eq!(r.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.headers().get("retry-after").and_then(|v| v.to_str().ok()), Some("2"));
        let body = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["retryable"], true);
        assert!(v["detail"].as_str().unwrap().contains("timed out"));
    }
}
