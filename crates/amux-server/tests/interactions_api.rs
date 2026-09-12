use amux_core::revision::{EntityType, MutationKind};
use amux_server::{
    api::{self, AppState},
    db::{PendingEvent, Store, WriteOutcome},
};
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn mutate(State(state): State<AppState>) -> Json<Value> {
    let result = state
        .store
        .write_async(|_| {
            Ok(WriteOutcome {
                applied: true,
                events: vec![
                    PendingEvent {
                        entity_type: EntityType::Task,
                        entity_id: "AR-142".into(),
                        mutation: MutationKind::Updated,
                        payload: None,
                    },
                    PendingEvent {
                        entity_type: EntityType::Worker,
                        entity_id: "amux".into(),
                        mutation: MutationKind::Updated,
                        payload: None,
                    },
                ],
            })
        })
        .await
        .unwrap();
    Json(json!({"applied":true,"rev":result.rev.0}))
}

fn app() -> (Router, Arc<Store>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("test.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = Router::new()
        .route("/api/test", post(mutate))
        .route(
            "/api/refused",
            post(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error":"Gate refused","fix":"Provide evidence"})),
                )
            }),
        )
        .route(
            "/api/failed",
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error":"Injected failure"})),
                )
            }),
        )
        .route(
            "/api/noop",
            post(|| async { Json(json!({"applied":false})) }),
        )
        .route(
            "/api/running",
            post(|| async { (StatusCode::ACCEPTED, Json(json!({"ok":true}))) }),
        )
        .route("/api/read", get(|| async { Json(json!({"ok":true})) }))
        .merge(api::interactions::routes())
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            store.clone(),
            api::interactions::middleware,
        ));
    (api::request_log::layer(app, store.clone()), store, dir)
}

async fn send(app: &Router, method: &str, path: &str, id: Option<&str>) -> (u16, Value) {
    let mut req = Request::builder().uri(path).method(method);
    if let Some(id) = id {
        req = req.header("x-amux-interaction-id", id);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1000000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn journal_effects_are_atomic_and_do_not_link_concurrent_requests() {
    let (app, store, _dir) = app();
    let (a, b) = tokio::join!(
        send(&app, "POST", "/api/test", Some("int_a")),
        send(&app, "POST", "/api/test", Some("int_b"))
    );
    assert_eq!(a.0, 200);
    assert_eq!(b.0, 200);
    for id in ["int_a", "int_b"] {
        let (_, receipt) = send(&app, "GET", &format!("/api/interactions/{id}"), None).await;
        assert_eq!(receipt["phase"], "applied");
        assert_eq!(receipt["applied_writes"], 1);
        let (_, effects) = send(
            &app,
            "GET",
            &format!("/api/interactions/{id}/effects"),
            None,
        )
        .await;
        assert_eq!(effects["n_considered"], 2);
        assert_eq!(effects["effects"][0]["kind"], "task.updated");
        assert!(effects["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["interaction_id"] == id));
    }
    let count: i64 = store
        .read()
        .unwrap()
        .query_row("SELECT count(*) FROM _amux_interaction_effects", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 4);
}

#[tokio::test]
async fn errors_noop_and_accepted_have_truthful_receipts_including_405() {
    let (app, _, _dir) = app();
    for (index, path, status, phase) in [
        (0, "/api/refused", 409, "refused"),
        (1, "/api/failed", 500, "failed"),
        (2, "/api/noop", 200, "noop"),
        (3, "/api/running", 202, "running"),
        (4, "/api/read", 405, "refused"),
    ] {
        let id = format!("int_{index}");
        assert_eq!(send(&app, "POST", path, Some(&id)).await.0, status);
        let (_, receipt) = send(&app, "GET", &format!("/api/interactions/{id}"), None).await;
        assert_eq!(receipt["phase"], phase);
        assert_eq!(receipt["acknowledgement"]["status"], status);
    }
    let (_, why) = send(&app, "GET", "/api/interactions/int_0/why", None).await;
    assert_eq!(why["receipt"]["acknowledgement"]["fix"], "Provide evidence");
    let (_, debug) = send(&app, "GET", "/api/debug/interactions", None).await;
    assert_eq!(debug["measured"], true);
    assert_eq!(debug["n_considered"], 5);
    assert!(debug["groups"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["phase"] == "failed" && g["count"] == 1));
}

#[tokio::test]
async fn correlation_identity_cannot_be_reused_for_a_different_command() {
    let (app, _, _dir) = app();
    assert_eq!(
        send(&app, "POST", "/api/noop", Some("int_shared")).await.0,
        200
    );
    assert_eq!(
        send(&app, "POST", "/api/test", Some("int_shared")).await.0,
        409
    );
    let (_, effects) = send(&app, "GET", "/api/interactions/int_shared/effects", None).await;
    assert_eq!(effects["n_considered"], 0);
    assert_eq!(send(&app, "POST", "/api/test", Some("bad id")).await.0, 400);
}

#[tokio::test]
async fn reads_do_not_create_receipts_and_summary_reports_unmeasured_device_state() {
    let (app, _, _dir) = app();
    send(&app, "GET", "/api/read", None).await;
    let (_, summary) = send(&app, "GET", "/api/state/summary", None).await;
    assert_eq!(summary["measured"], true);
    assert_eq!(summary["n_considered"], 0);
    assert!(summary["health"]["outbox_pending"].is_null());
    assert!(summary["health"]["why_unmeasured"].is_string());
}

#[tokio::test]
async fn detached_and_blocking_work_preserve_cause_and_report_progress() {
    use amux_server::db::interactions::{self, CURRENT};
    let (app, store, _dir) = app();
    send(&app, "POST", "/api/noop", Some("int_detached")).await;
    let work_store = store.clone();
    CURRENT
        .scope("int_detached".into(), async move {
            interactions::spawn(async move {
                interactions::progress(&work_store, "waiting")
                    .await
                    .unwrap();
                interactions::spawn_blocking(move || {
                    work_store.write(|_| {
                        Ok(WriteOutcome {
                            applied: true,
                            events: vec![],
                        })
                    })
                })
                .await
                .unwrap()
                .unwrap();
            })
            .await
            .unwrap();
        })
        .await;
    let (_, receipt) = send(&app, "GET", "/api/interactions/int_detached", None).await;
    assert_eq!(receipt["phase"], "waiting");
    assert_eq!(receipt["applied_writes"], 1);
    assert_eq!(receipt["unjournaled_writes"], 1);
}

#[test]
fn bare_success_is_unknown_and_explicit_acknowledgement_is_required() {
    assert_eq!(api::interactions::classify(200, &json!({})), "unknown");
    assert_eq!(api::interactions::classify(204, &Value::Null), "unknown");
    assert_eq!(
        api::interactions::classify(200, &json!({"applied":true})),
        "applied"
    );
    assert_eq!(
        api::interactions::classify(202, &json!({"ok":true})),
        "running"
    );
}

#[test]
fn explicit_receipt_phases_match_the_browser_contract() {
    for phase in [
        "accepted",
        "queued",
        "sending",
        "running",
        "waiting",
        "blocked",
        "applied",
        "noop",
        "refused",
        "failed",
        "reconciled",
        "unknown",
    ] {
        assert_eq!(
            api::interactions::classify(200, &json!({"phase":phase})),
            phase
        );
    }
    assert_eq!(
        api::interactions::classify(409, &json!({"phase":"applied"})),
        "refused"
    );
}

#[tokio::test]
async fn mutation_latency_measurement_reports_both_populations() {
    let (with_receipts, store, _dir) = app();
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "benchmark".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let baseline = api::request_log::layer(
        Router::new()
            .route("/api/test", post(mutate))
            .with_state(state),
        store,
    );
    let mut populations = [vec![], vec![]];
    for i in 0..60 {
        for (index, app) in [&baseline, &with_receipts].into_iter().enumerate() {
            let start = std::time::Instant::now();
            assert_eq!(send(app, "POST", "/api/test", None).await.0, 200);
            if i >= 10 {
                populations[index].push(start.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
    for (index, values) in populations.iter_mut().enumerate() {
        values.sort_by(f64::total_cmp);
        println!("interaction_latency measured=true n_considered={} mode={} p50_ms={:.3} p95_ms={:.3} scope=in_process_sqlite_and_middleware",values.len(),if index==0 {"baseline_request_log"} else {"receipts_and_request_log"},values[values.len()/2],values[values.len()*95/100]);
    }
}

#[tokio::test]
async fn rolled_back_writes_create_no_effect_links() {
    let (app, store, _dir) = app();
    send(&app, "POST", "/api/noop", Some("int_rollback")).await;
    let result = amux_server::db::interactions::CURRENT
        .scope(
            "int_rollback".into(),
            store.write_async(|conn| {
                conn.execute("INSERT INTO _amux_rev (id,rev) VALUES (1,999)", [])?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            }),
        )
        .await;
    assert!(result.is_err());
    let (_, receipt) = send(&app, "GET", "/api/interactions/int_rollback", None).await;
    assert_eq!(receipt["applied_writes"], 0);
}

#[tokio::test]
async fn unavailable_receipt_storage_refuses_before_executing_a_mutation() {
    let (app, store, _dir) = app();
    let before: i64 = store
        .read()
        .unwrap()
        .query_row("SELECT rev FROM _amux_rev WHERE id=1", [], |r| r.get(0))
        .unwrap();
    store.write_async(|conn| {
        conn.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON _amux_interactions BEGIN SELECT RAISE(FAIL, 'injected receipt failure'); END;")?;
        Ok(WriteOutcome {applied:false,events:vec![]})
    }).await.unwrap();
    assert_eq!(
        send(&app, "POST", "/api/test", Some("int_not_run")).await.0,
        503
    );
    let after: i64 = store
        .read()
        .unwrap()
        .query_row("SELECT rev FROM _amux_rev WHERE id=1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn invalid_scope_is_not_misreported_as_a_database_outage() {
    let (app, _, _dir) = app();
    let (status, body) = send(&app, "GET", "/api/state/summary?scope=group:nope", None).await;
    assert_eq!(status, 400);
    assert_eq!(body["measured"], false);
    assert_eq!(body["n_considered"], 0);
    assert!(body["why_unmeasured"]
        .as_str()
        .unwrap()
        .contains("validation"));
}
