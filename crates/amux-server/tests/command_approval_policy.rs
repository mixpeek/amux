//! Isolated process: this policy fixture cannot change another integration
//! target's environment or create questions on the live owner's board.
use amux_server::{
    api::{router, AppState},
    db::Store,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicBool, Arc};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, path: &str, data: Value) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("X-Amux-Session", "approval-fixture")
                .body(Body::from(data.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn needs_you_requires_budget_or_customer_outbound_at_every_write_boundary() {
    std::env::set_var("AMUX_APPROVAL_TYPES", "budget,customer_outbound");
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&temp.path().join("policy.db")).unwrap());
    let app = router(AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(AtomicBool::new(true)),
    });
    for kind in [
        "decision",
        "access",
        "credential",
        "external",
        "judgment",
        "budget",
        "customer_outbound",
    ] {
        let body = json!({"title":format!("Policy fixture {kind}"),"session":"approval-fixture","type":"chore",
            "status":"needsyou","tags":["test"],"gate_ack":true,"ask_type":kind,"ask_actor":"Ethan",
            "ask_question":"Approve the explicitly described action for this fixture?",
            "ask_unblocks":"Approval permits this fixture's described action"});
        let (status, result) = call(&app, "POST", "/api/board", body).await;
        if ["budget", "customer_outbound"].contains(&kind) {
            assert!(status.is_success(), "{kind}: {result}");
        } else {
            assert_eq!(status, StatusCode::CONFLICT, "{kind}: {result}");
        }
    }
    let (status, created) = call(
        &app,
        "POST",
        "/api/board",
        json!({"title":"Choose report formatting",
        "session":"approval-fixture","type":"chore","status":"todo","tags":["test"]}),
    )
    .await;
    assert!(status.is_success(), "{created}");
    let id = created["id"].as_str().unwrap();
    let (status,result)=call(&app,"PATCH",&format!("/api/board/{id}"),json!({"status":"needsyou",
        "force":true,"reason":"test force cannot override standing authority","gate_ack":true,
        "ask_type":"decision","ask_actor":"Ethan","ask_question":"Which report formatting should be used?",
        "ask_unblocks":"Choosing a report format"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{result}");
    assert_eq!(
        result["code"], "needsyou_outside_approval_policy",
        "{result}"
    );
    let c = store.read().unwrap();
    assert_eq!(
        amux_server::db::board_store::get_issue(&c, id)
            .unwrap()
            .unwrap()
            .status,
        "todo"
    );
    let opts = amux_server::db::advance::AdvanceOpts {
        force: true,
        gate_ack: true,
        ..Default::default()
    };
    assert!(
        amux_server::db::advance::advance(&c, id, "needsyou", "test", &opts)
            .unwrap()
            .is_err(),
        "internal transitions cannot bypass the same approval policy"
    );
}
