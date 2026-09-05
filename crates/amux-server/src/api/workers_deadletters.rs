//! Dead-letter visibility (RR-0068, Invariant 34).
//!
//! `GET /api/workers/{id}/dead-letters` — the queryable surface for "the
//! orchestrator wanted something to happen and it did not". The response
//! carries the dead-lettered commands themselves plus a queue-health
//! summary (per-state counts + depth), because a dead-letter count with no
//! queue context sends the reader to the wrong cause: one dead letter atop
//! a healthy queue is history, one atop twelve failed commands is a fire.
//!
//! MOUNTING (documented choice): merged into the `/api/workers` nest in
//! api/mod.rs (`workers::routes().merge(workers_deadletters::routes())`)
//! rather than a second `.nest` at the same prefix — axum treats two nests
//! at one path as a conflict, and a separate prefix would fork the worker
//! URL namespace RR-0068 explicitly places this under.
//!
//! The EMISSION half of RR-0068 lives in orchestrator/runtime.rs's pump
//! failure path: when a Fail lands on a command whose retry budget is
//! spent, the pump applies `Retry` (which dead-letters it, per the core
//! state machine) and emits a `command_dead_letter` StateEvent in the same
//! transaction — the DurableEvent a dead letter must produce.

use super::AppState;
use crate::db::commands;
use crate::db::queries;
use amux_core::ids::WorkerId;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new().route("/{id}/dead-letters", get(dead_letters))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    200
}

/// GET /api/workers/{id}/dead-letters — worker resolved by id, display
/// name, or alias (Invariant 17), same as every other worker route.
pub async fn dead_letters(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(p): Query<ListParams>,
) -> Response {
    let limit = p.limit.clamp(1, 1000);
    let store = state.store.clone();
    let k = key.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let Some(row) = queries::get_worker(&conn, &k)? else {
            return Ok(None);
        };
        let worker = WorkerId::parse(&row.id)
            .map_err(|e| anyhow::anyhow!("corrupt worker id {}: {e}", row.id))?;
        let dead = commands::dead_letters_for(&conn, &worker, limit)?;
        let counts = commands::state_counts(&conn, &worker)?;
        // Depth = commands still owed an outcome (non-terminal states).
        // Confirmed/dead-lettered rows are history, not load.
        let depth: u32 = ["queued", "dispatched", "delivered", "failed"]
            .iter()
            .filter_map(|s| counts.get(*s))
            .sum();
        let dead_letter_count = counts.get("dead_lettered").copied().unwrap_or(0);
        Ok(Some(json!({
            "worker_id": row.id,
            "dead_letters": dead
                .iter()
                .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
                .collect::<Vec<_>>(),
            "queue_health": {
                "counts": counts,
                "depth": depth,
                "dead_letter_count": dead_letter_count,
            },
        })))
    })
    .await;
    match joined {
        Ok(Ok(Some(body))) => Json(body).into_response(),
        Ok(Ok(None)) => err(
            StatusCode::NOT_FOUND,
            json!({ "error": "worker not found", "key": key }),
        ),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::router;
    use crate::db::{SharedStore, Store, WriteOutcome};
    use crate::opencode::mock::MockProtocol;
    use amux_core::ids::CommandId;
    use amux_core::protocol::{
        CommandState, CommandTransition, DeliveryTiming, WorkerCommand,
    };
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use chrono::Utc;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn app() -> (axum::Router, SharedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (router(state), store, dir)
    }

    async fn send(app: &axum::Router, method: &str, path: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let b = Request::builder().method(method).uri(path);
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, v)
    }

    async fn create_worker(app: &axum::Router, name: &str) -> String {
        let (st, v) = send(
            app,
            "POST",
            "/api/workers",
            Some(serde_json::json!({ "display_name": name, "cwd": "/tmp/w" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().to_string()
    }

    /// Drive a command through fail/retry cycles to a given attempts count.
    fn spend_attempts(store: &SharedStore, id: &CommandId, cycles: u32) {
        let id = id.clone();
        store
            .write(move |conn| {
                for _ in 0..cycles {
                    commands::transition(conn, &id, CommandTransition::Dispatch, 3)?;
                    commands::transition(
                        conn,
                        &id,
                        CommandTransition::Fail { reason: "transport down".into() },
                        3,
                    )?;
                    commands::transition(conn, &id, CommandTransition::Retry, 3)?;
                }
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    #[tokio::test]
    async fn listing_scopes_to_worker_and_summarizes_queue_health() {
        let (app, store, _dir) = app();
        let w1 = create_worker(&app, "victim").await;
        let w2 = create_worker(&app, "bystander").await;
        let wid1 = WorkerId::parse(&w1).unwrap();
        let wid2 = WorkerId::parse(&w2).unwrap();

        // w1: one dead letter + one queued. w2: its own dead letter.
        let dead1 = CommandId::from_ulid(ulid::Ulid::new());
        let queued1 = CommandId::from_ulid(ulid::Ulid::new());
        let dead2 = CommandId::from_ulid(ulid::Ulid::new());
        {
            let (dead1, queued1, dead2) = (dead1.clone(), queued1.clone(), dead2.clone());
            let (wid1, wid2) = (wid1.clone(), wid2.clone());
            store
                .write(move |conn| {
                    commands::enqueue(conn, dead1, &wid1, &WorkerCommand::Continue, "k1",
                        &DeliveryTiming::Immediate, None, Utc::now())?;
                    commands::enqueue(conn, queued1, &wid1, &WorkerCommand::Continue, "k2",
                        &DeliveryTiming::Immediate, None, Utc::now())?;
                    commands::enqueue(conn, dead2, &wid2, &WorkerCommand::Continue, "k3",
                        &DeliveryTiming::Immediate, None, Utc::now())?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        }
        spend_attempts(&store, &dead1, 3); // third retry dead-letters
        spend_attempts(&store, &dead2, 3);

        let (st, v) = send(&app, "GET", &format!("/api/workers/{w1}/dead-letters"), None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["worker_id"], serde_json::json!(w1));
        let dead = v["dead_letters"].as_array().unwrap();
        assert_eq!(dead.len(), 1, "only w1's dead letter: {v}");
        assert_eq!(dead[0]["id"], serde_json::json!(dead1.as_str()));
        assert_eq!(v["queue_health"]["counts"]["dead_lettered"], serde_json::json!(1));
        assert_eq!(v["queue_health"]["counts"]["queued"], serde_json::json!(1));
        assert_eq!(v["queue_health"]["depth"], serde_json::json!(1), "dead letters are not load");
        assert_eq!(v["queue_health"]["dead_letter_count"], serde_json::json!(1));

        // Unknown worker: 404, not an empty happy list.
        let (st, _) = send(&app, "GET", "/api/workers/ghost/dead-letters", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pump_dead_letters_exhausted_command_and_emits_the_event() {
        // The emission half of RR-0068 (wired in runtime.rs's pump): a Fail
        // that spends the budget is retried into DeadLettered and produces
        // a `command_dead_letter` StateEvent in the same transaction.
        let (app, store, _dir) = app();
        let w = create_worker(&app, "doomed").await;
        let wid = WorkerId::parse(&w).unwrap();
        let cmd_id = CommandId::from_ulid(ulid::Ulid::new());
        {
            let (cmd_id, wid) = (cmd_id.clone(), wid.clone());
            store
                .write(move |conn| {
                    commands::enqueue(conn, cmd_id, &wid, &WorkerCommand::Continue, "k",
                        &DeliveryTiming::Immediate, None, Utc::now())?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .unwrap();
        }
        // Two spent attempts; the pump's own failure is the third.
        spend_attempts(&store, &cmd_id, 2);

        // Protocol does NOT know this worker -> delivery fails (Immediate
        // timing skips the boundary gate, so the pump reaches dispatch).
        let protocol = Arc::new(MockProtocol::new());
        let rt = crate::orchestrator::runtime::Runtime {
            store: store.clone(),
            backends: vec![],
            tick_secs: 3,
            heartbeat_every: 1000,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: u64::MAX,
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: Some(protocol),
            pickup_unowned: false,
            resume_stagger_secs: 5,
        };
        let mut rx = store.subscribe();
        rt.pump_commands(Utc::now(), &std::collections::BTreeMap::new()).await.unwrap();

        // Terminal state reached...
        {
            let conn = store.read().unwrap();
            let cmd = commands::by_id(&conn, &cmd_id).unwrap().unwrap();
            assert!(
                matches!(&cmd.state, CommandState::DeadLettered { .. }),
                "{:?}",
                cmd.state
            );
            assert_eq!(cmd.attempts, 3);
        }
        // ...and announced as a DurableEvent (an unannounced dead letter is
        // the silent-vanish failure mode this exists to kill).
        let mut saw_dead_letter_event = false;
        while let Ok(ev) = rx.try_recv() {
            if format!("{:?}", ev.entity_type).contains("command_dead_letter")
                && ev.entity_id == cmd_id.as_str()
            {
                saw_dead_letter_event = true;
            }
        }
        assert!(saw_dead_letter_event, "dead letter must emit its StateEvent");

        // And it is visible through the API surface.
        let (st, v) = send(&app, "GET", &format!("/api/workers/{w}/dead-letters"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["dead_letters"].as_array().unwrap().len(), 1);
        assert_eq!(v["queue_health"]["dead_letter_count"], serde_json::json!(1));
    }
}
