//! A worker's direct message with one person, and a person's notes to
//! themselves (Ethan, 2026-11-06: "can you make it so workers can DM people
//! directly it should show up in a 1:1 conversation. i shoudl also be able to
//! open my own").
//!
//! Through the real router, real invite cookies, a temp store and a temp amux
//! home holding one worker (`ed`, group `wexus`). Never ~/.amux.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn pin_home() {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| {
        let h = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        std::fs::write(h.join("sessions/ed.env"), "CC_TAGS=\"wexus\"\n").unwrap();
        h
    });
    std::env::set_var("AMUX_HOME", home);
}

fn server() -> (axum::Router, tempfile::TempDir) {
    pin_home();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("worker-dm-test.db")).unwrap();
    let state = AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn send(app: &axum::Router, method: &str, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Invite a global member, accept the invite, and return their cookie.
async fn member(app: &axum::Router, email: &str) -> String {
    let (st, invite) = send(app, "POST", "/api/org/invites",
        Some(json!({"email": email, "scope_level": "global", "grant_host_access": true})),
        &[("host", "test-host:8824"), ("x-forwarded-proto", "https")]).await;
    assert_eq!(st, StatusCode::CREATED, "{invite}");
    let token = invite["token"].as_str().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/invite/{token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("email={}&name=", email.replace('@', "%40"))))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    res.headers()[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap().to_string()
}

fn direct(list: &Value) -> Vec<String> {
    list["direct"].as_array().unwrap().iter().map(|d| d["conv"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn a_worker_writes_to_one_person_in_their_own_direct_message() {
    let (app, _d) = server();
    let doron = member(&app, "doron@z.com").await;
    let mate = member(&app, "mate@z.com").await;
    let as_doron = [("cookie", doron.as_str())];
    let as_mate = [("cookie", mate.as_str())];
    let as_ed = [("x-amux-session", "ed")];

    // `amux post p:doron` from the worker: a 1:1 with Doron, not ed's shared thread.
    let (st, post) = send(&app, "POST", "/api/conversations/p:doron/posts", Some(json!({"body": "the card is ready"})), &as_ed).await;
    assert_eq!(st, StatusCode::CREATED, "{post}");
    assert_eq!(post["conv"], "d:ed|doron@z.com", "{post}");
    assert_eq!(post["author"], "worker:ed");
    assert!(post["mentions"].as_array().unwrap().contains(&json!("person:doron@z.com")), "Doron is notified: {post}");

    // Doron's Messages lists it as a direct conversation with ed, unread.
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_doron).await;
    assert_eq!(direct(&list), vec!["d:ed|doron@z.com".to_string()], "{list}");
    assert_eq!(list["direct"][0]["worker"], "ed");
    assert_eq!(list["direct"][0]["unread"], 1);
    let (st, page) = send(&app, "GET", "/api/conversations/d:ed%7Cdoron@z.com/posts", None, &as_doron).await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(page["posts"].as_array().unwrap().len(), 1, "{page}");

    // Private to the two: another member and the owner neither list nor read it.
    for who in [&as_mate[..], &[][..]] {
        let (_, list) = send(&app, "GET", "/api/conversations", None, who).await;
        assert!(direct(&list).is_empty(), "{list}");
        let (st, _) = send(&app, "GET", "/api/conversations/d:ed%7Cdoron@z.com/posts", None, who).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }
    // Nor did it land in ed's shared thread, which the owner reads.
    let (_, shared) = send(&app, "GET", "/api/conversations/w:ed/posts", None, &[]).await;
    assert!(!shared["posts"].as_array().unwrap().iter().any(|p| p["body"] == "the card is ready"), "{shared}");

    // Doron's answer there goes to ed, the way a post in w:ed does.
    let (st, reply) = send(&app, "POST", "/api/conversations/d:ed%7Cdoron@z.com/posts", Some(json!({"body": "thanks, ship it"})), &as_doron).await;
    assert_eq!(st, StatusCode::CREATED, "{reply}");
    assert_eq!(reply["delivering_to"], 1, "{reply}");
    assert!(reply["mentions"].as_array().unwrap().contains(&json!("worker:ed")), "{reply}");

    // Another member cannot post into it, and a worker cannot write into another worker's.
    let (st, _) = send(&app, "POST", "/api/conversations/d:ed%7Cdoron@z.com/posts", Some(json!({"body": "hi"})), &as_mate).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = send(&app, "POST", "/api/conversations/p:nobody@z.com/posts", Some(json!({"body": "hi"})), &as_ed).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "an unknown person");
}

#[tokio::test]
async fn a_person_opens_a_conversation_with_themselves() {
    let (app, _d) = server();
    let doron = member(&app, "doron@z.com").await;
    let as_doron = [("cookie", doron.as_str())];

    let (_, list) = send(&app, "GET", "/api/conversations", None, &[]).await;
    let me = list["people"].as_array().unwrap().iter().find(|p| p["me"] == true).cloned().unwrap_or(Value::Null);
    assert_eq!(me["conv"], "p:owner|owner", "the owner's own entry opens: {list}");

    let (st, post) = send(&app, "POST", "/api/conversations/p:owner%7Cowner/posts", Some(json!({"body": "note to self"})), &[]).await;
    assert_eq!(st, StatusCode::CREATED, "{post}");
    let (st, page) = send(&app, "GET", "/api/conversations/p:owner%7Cowner/posts", None, &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(page["posts"].as_array().unwrap().len(), 1);
    // Nobody else reads it.
    let (st, _) = send(&app, "GET", "/api/conversations/p:owner%7Cowner/posts", None, &as_doron).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (_, list) = send(&app, "GET", "/api/conversations", None, &as_doron).await;
    let me = list["people"].as_array().unwrap().iter().find(|p| p["me"] == true).cloned().unwrap_or(Value::Null);
    assert_eq!(me["conv"], "p:doron@z.com|doron@z.com", "a member has their own too: {list}");
}
