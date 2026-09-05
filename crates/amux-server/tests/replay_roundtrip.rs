//! RR-0111a integration: write through the REAL API handlers -> journal ->
//! replay -> verify. Ethos rule 7: test the shipped code path, not a
//! paraphrase — these tests drive the same axum router production serves, so
//! a payload-population site that silently stopped journaling would fail
//! here, not in a hand-built simulation of it.

use amux_server::api::{router, AppState};
use amux_server::db::replay::{self, Divergence};
use amux_server::db::{board_store, queries, PendingEvent, Store, WriteOutcome};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

struct Rig {
    app: axum::Router,
    store: Arc<Store>,
    db_path: PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    // Replay round-trip exercises the event log, not the global done-link gate
    // (which has its own coverage), so pin it OFF.
    std::env::set_var("AMUX_DONE_LINK_REQUIRED", "0");
    std::env::set_var("AMUX_DONE_EVIDENCE_REQUIRED", "0");
    // Same reason, for the AF-317/AF-318 status gates: these rigs exercise the
    // lifecycle state machine, and each gate has its own coverage in board_api.
    std::env::set_var("AMUX_BLOCKED_NEEDS_WATCH", "0");
    std::env::set_var("AMUX_NEEDSYOU_ASK_REQUIRED", "0");
    std::env::set_var("AMUX_TODO_WIP_LIMIT", "0");
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("replay-test.db");
    let store = Arc::new(Store::open(&db_path).unwrap());
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    Rig {
        app: router(state),
        store,
        db_path,
        _dir: dir,
    }
}

async fn send(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder()
        .method(method)
        .uri(path)
        .header("x-amux-session", "replay-test");
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
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, v)
}

/// Worker created + patched + started, board card created + edited + moved —
/// all through the API — then the journal alone must reproduce both rows.
#[tokio::test]
async fn write_then_replay_round_trip() {
    let r = rig();

    // Worker: create -> config patch -> start (Created, Updated,
    // StatusChanged events, each with a snapshot).
    let (st, w) = send(
        &r.app,
        "POST",
        "/api/workers",
        Some(json!({ "display_name": "w1", "cwd": "/tmp/w" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{w}");
    let wid = w["id"].as_str().unwrap().to_string();
    let (st, body) = send(
        &r.app,
        "PATCH",
        &format!("/api/workers/{wid}"),
        Some(json!({ "cwd": "/elsewhere", "expect_version": 0 })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = send(&r.app, "POST", &format!("/api/workers/{wid}/start"), None).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");

    // Board card: create -> desc edit -> status changes (gate acked).
    let (st, card) = send(
        &r.app,
        "POST",
        "/api/board",
        Some(json!({ "title": "Replay card", "type": "chore", "desc": "v1" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{card}");
    let cid = card["id"].as_str().unwrap().to_string();
    let (st, patched) = send(
        &r.app,
        "PATCH",
        &format!("/api/board/{cid}"),
        Some(json!({ "desc": "updated desc" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{patched}");
    let desc_rev = patched["global_rev"].as_u64().expect("applied PATCH carries global_rev");
    let (st, body) = send(
        &r.app,
        "PATCH",
        &format!("/api/board/{cid}"),
        Some(json!({ "status": "doing", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = send(
        &r.app,
        "PATCH",
        &format!("/api/board/{cid}"),
        Some(json!({ "status": "done", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    // Fold the journal and compare with the live rows, on a fresh direct
    // connection (nothing cached from the request path).
    let conn = rusqlite::Connection::open(&r.db_path).unwrap();
    let head: u64 = conn
        .query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |x| x.get(0))
        .unwrap();
    let rs = replay::replay_state(&conn, head).unwrap();
    assert!(rs.pre_payload_horizon.is_none(), "every event carried its snapshot");

    let live_card = board_store::get_issue(&conn, &cid).unwrap().unwrap();
    let replayed_card = &rs.entities["task"][&cid];
    assert_eq!(
        replayed_card.state.as_ref(),
        Some(&live_card.snapshot()),
        "journal alone reproduces the board row"
    );
    assert_eq!(replayed_card.events_folded, 4); // create + desc + 2 status moves

    let live_worker = queries::all_workers_for_replay(&conn)
        .unwrap()
        .into_iter()
        .find(|x| x.id == wid)
        .unwrap();
    let replayed_worker = &rs.entities["worker"][&wid];
    assert_eq!(
        replayed_worker.state.as_ref(),
        Some(&live_worker.snapshot()),
        "journal alone reproduces the worker row"
    );

    // Sliced replay reproduces the PAST: at the desc-edit rev the card said
    // "updated desc" but had not moved yet.
    let at_desc = replay::replay_state(&conn, desc_rev).unwrap();
    let past = at_desc.entities["task"][&cid].state.as_ref().unwrap();
    assert_eq!(past["desc"], json!("updated desc"));
    assert_eq!(past["status"], json!("todo"));

    // And the audit verdict: everything checked, everything matched.
    let report = replay::verify_replay(&conn).unwrap();
    assert_eq!(report.divergences_total, 0, "{:?}", report.divergences);
    assert_eq!(report.horizon_entities_total, 0);
    assert_eq!(report.live_not_in_journal_total, 0);
    assert_eq!(report.entities_checked, 2);
    assert_eq!(report.entities_matched, 2);
}

/// Hand-corrupt live rows behind the journal's back: verify_replay must NAME
/// each corrupted entity and the exact fields, not just count something.
#[tokio::test]
async fn divergence_detection_names_the_corrupted_rows() {
    let r = rig();
    let (st, w) = send(
        &r.app,
        "POST",
        "/api/workers",
        Some(json!({ "display_name": "w1", "cwd": "/tmp/w" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let wid = w["id"].as_str().unwrap().to_string();
    let (st, card) = send(
        &r.app,
        "POST",
        "/api/board",
        Some(json!({ "title": "honest title", "type": "chore" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let cid = card["id"].as_str().unwrap().to_string();

    // Corruption: direct writes that bypass the store, the journal, and the
    // revision counter entirely.
    let conn = rusqlite::Connection::open(&r.db_path).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    conn.execute(
        "UPDATE issues SET title = 'corrupted-by-hand' WHERE id = ?1",
        rusqlite::params![cid],
    )
    .unwrap();
    conn.execute(
        "UPDATE _amux_workers SET cwd = '/corrupted' WHERE id = ?1",
        rusqlite::params![wid],
    )
    .unwrap();

    let report = replay::verify_replay(&conn).unwrap();
    assert_eq!(report.divergences_total, 2, "{:?}", report.divergences);

    let field_mismatch = |etype: &str, eid: &str| -> Vec<(String, Value, Value)> {
        report
            .divergences
            .iter()
            .find_map(|d| match d {
                Divergence::FieldMismatch { entity_type, entity_id, fields, .. }
                    if entity_type == etype && entity_id == eid =>
                {
                    Some(
                        fields
                            .iter()
                            .map(|f| (f.field.clone(), f.replayed.clone(), f.live.clone()))
                            .collect(),
                    )
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no FieldMismatch for {etype}/{eid}: {:?}", report.divergences))
    };

    // The card divergence names the field AND both values.
    let fields = field_mismatch("task", &cid);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].0, "title");
    assert_eq!(fields[0].1, json!("honest title"));
    assert_eq!(fields[0].2, json!("corrupted-by-hand"));

    let fields = field_mismatch("worker", &wid);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].0, "cwd");
    assert_eq!(fields[0].1, json!("/tmp/w"));
    assert_eq!(fields[0].2, json!("/corrupted"));
}

/// A payload-less event for a payload-bearing type (the pre-0008 shape) must
/// surface as horizon — reported with the first full-replay rev — while
/// payload-carrying entities in the same journal still verify. Never
/// fabricate what was not recorded.
#[tokio::test]
async fn horizon_honesty_reports_instead_of_fabricating() {
    let r = rig();

    // Simulate a pre-payload event exactly as the old writer produced it.
    let reply = r
        .store
        .write(|_conn| {
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: "GHOST-9".into(),
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: None,
                }],
            })
        })
        .unwrap();
    let ghost_rev = reply.rev.0;

    // A modern, snapshot-carrying card lands after it.
    let (st, card) = send(
        &r.app,
        "POST",
        "/api/board",
        Some(json!({ "title": "modern card", "type": "chore" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let cid = card["id"].as_str().unwrap().to_string();

    let conn = rusqlite::Connection::open(&r.db_path).unwrap();
    let report = replay::verify_replay(&conn).unwrap();

    // The ghost is horizon, not divergence, not silence.
    assert_eq!(report.horizon_entities_total, 1);
    assert_eq!(report.horizon_entities[0].entity_id, "GHOST-9");
    assert_eq!(report.horizon_entities[0].last_rev, ghost_rev);
    let h = report.pre_payload_horizon.as_ref().expect("horizon block present");
    assert_eq!(h.payloadless_events, 1);
    assert_eq!(h.first_full_replay_rev, ghost_rev + 1);
    assert_eq!(report.divergences_total, 0, "{:?}", report.divergences);

    // The modern card is still fully verified alongside it.
    assert_eq!(report.entities_checked, 1);
    assert_eq!(report.entities_matched, 1);
    let rs = replay::replay_state(&conn, report.head_rev).unwrap();
    assert_eq!(
        rs.entities["task"][&cid].state.as_ref(),
        Some(&board_store::get_issue(&conn, &cid).unwrap().unwrap().snapshot())
    );
    // The ghost's replayed state is honestly unknown.
    assert_eq!(rs.entities["task"]["GHOST-9"].state, None);
}

/// The report is readable where people already look (ethos rule 4):
/// GET /api/metrics/replay serves the full ReplayReport.
#[tokio::test]
async fn metrics_replay_endpoint_serves_the_report() {
    let r = rig();
    let (st, card) = send(
        &r.app,
        "POST",
        "/api/board",
        Some(json!({ "title": "endpoint card", "type": "chore" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{card}");

    let (st, body) = send(&r.app, "GET", "/api/metrics/replay", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body["head_rev"].as_u64().unwrap() >= 1);
    assert_eq!(body["payload_bearing_types"], json!(["worker", "task"]));
    assert_eq!(body["entities_checked"], json!(1));
    assert_eq!(body["entities_matched"], json!(1));
    assert_eq!(body["divergences"], json!([]));
    assert_eq!(body["pre_payload_horizon"], Value::Null);
    // The cap announces itself even when nothing was cut (Invariant 40).
    assert!(body["lists_capped_at"].as_u64().unwrap() > 0);
}
