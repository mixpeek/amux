//! Vault secrets API, end to end through the real router (AMUX-5375).
//!
//! One test function on purpose: it sets AMUX_HOME for this process, and the
//! steps depend on each other (create, rotate, reveal, delete). Every value is
//! a canary, and the last step greps everything the server wrote, the tracing
//! output included, for each of them.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const TOKEN: &str = "test-owner-token-0001";
const CANARY_GLOBAL: &str = "canary-global-5a1f0c9e";
const CANARY_GROUP: &str = "canary-group-77d2b3aa";
const CANARY_WORKER: &str = "canary-worker-0be4c1d7";
const CANARY_ROTATED: &str = "canary-rotated-91fe2a40";

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);
impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn app(home: &std::path::Path) -> axum::Router {
    let store = amux_server::db::Store::open(&home.join("test.db")).unwrap();
    router(AppState {
        store: Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: Some(TOKEN.into()),
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

/// (status, body, raw text, cache-control)
async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>, owner: bool, worker: Option<&str>) -> (u16, Value, String, String) {
    let mut b = Request::builder().method(method).uri(path).header("content-type", "application/json");
    if owner {
        b = b.header("authorization", format!("Bearer {TOKEN}"));
    }
    if let Some(w) = worker {
        b = b.header("x-amux-session", w);
    }
    let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let cc = res.headers().get("cache-control").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024).await.unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    (status, serde_json::from_str(&raw).unwrap_or(Value::Null), raw, cc)
}

#[tokio::test]
async fn vault_secrets_lifecycle_authz_precedence_and_no_leaks() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("amux-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(home.join("sessions/w1.env"), "CC_TAGS=\"ops\"\n").unwrap();
    std::fs::write(home.join("sessions/w2.env"), "CC_TAGS=\"web\"\n").unwrap();
    std::fs::write(home.join("sessions/w3.env"), "CC_ISOLATED=1\nCC_TAGS=\"ops\"\n").unwrap();
    // A private, empty tmux socket dir: "which workers are running" must ask
    // a tmux server that does not exist, never the real one.
    let tmux_dir = dir.path().join("tmux");
    std::fs::create_dir_all(&tmux_dir).unwrap();
    std::env::set_var("TMUX_TMPDIR", &tmux_dir);
    std::env::remove_var("TMUX");
    std::env::set_var("AMUX_HOME", &home);
    std::env::set_var("AMUX_VAULT_KEYSTORE", "file");
    let logs = Buf::default();
    let sink = logs.clone();
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .try_init();
    let app = app(&home);

    // Authorization: a worker origin, and a request without the owner bearer,
    // are refused on every write.
    let add = |key: &str, scope: &str, target: &str, value: &str| json!({"key": key, "scope": scope, "target": target, "value": value, "description": "test item"});
    let (s, _, raw, _) = call(&app, "POST", "/api/vault/secrets", Some(add("API_TOKEN", "global", "", CANARY_GLOBAL)), true, Some("w1")).await;
    assert_eq!(s, 403, "worker origin must be refused: {raw}");
    let (s, _, _, _) = call(&app, "POST", "/api/vault/secrets", Some(add("API_TOKEN", "global", "", CANARY_GLOBAL)), false, None).await;
    assert!(s == 401 || s == 403, "no owner bearer must be refused, got {s}");

    let mut ids = std::collections::BTreeMap::new();
    for (scope, target, value) in [("global", "", CANARY_GLOBAL), ("group", "ops", CANARY_GROUP), ("worker", "w1", CANARY_WORKER)] {
        let (s, b, raw, _) = call(&app, "POST", "/api/vault/secrets", Some(add("API_TOKEN", scope, target, value)), true, None).await;
        assert_eq!(s, 201, "{raw}");
        assert!(!raw.contains(value), "a create response carried the value");
        assert_eq!(b["item"]["version"], 1);
        ids.insert(scope.to_string(), b["item"]["id"].as_str().unwrap().to_string());
    }
    let (s, _, _, _) = call(&app, "POST", "/api/vault/secrets", Some(add("CC_DIR", "global", "", "x1234")), true, None).await;
    assert_eq!(s, 400, "harness variables are refused");
    let (s, _, _, _) = call(&app, "POST", "/api/vault/secrets", Some(add("API_TOKEN", "global", "", "dup-value")), true, None).await;
    assert_eq!(s, 409, "one value per key and scope");
    let (s, _, _, _) = call(&app, "POST", "/api/vault/secrets", Some(add("OTHER", "worker", "nobody", "x1234")), true, None).await;
    assert_eq!(s, 404, "a worker scope must name a real worker");

    // Owner list: metadata and reach, no values.
    let (s, b, raw, _) = call(&app, "GET", "/api/vault/secrets", None, true, None).await;
    assert_eq!(s, 200);
    assert_eq!(b["measured"], true);
    for v in [CANARY_GLOBAL, CANARY_GROUP, CANARY_WORKER] {
        assert!(!raw.contains(v), "list leaked a value");
    }
    let item = |id: &str| b["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap().clone();
    // w1: worker wins. w3: group ops wins. w2: global.
    assert_eq!(item(&ids["global"])["effective_for"], 1, "global wins only for w2");
    assert_eq!(item(&ids["global"])["overridden_for"], 2);
    assert_eq!(item(&ids["group"])["effective_for"], 1, "group wins for w3 only");
    assert_eq!(item(&ids["worker"])["effective_for"], 1);

    // Precedence per worker, isolated included.
    for (w, want) in [("w1", "worker"), ("w2", "global"), ("w3", "group")] {
        let (_, b, _, _) = call(&app, "GET", &format!("/api/vault/secrets?worker={w}"), None, true, None).await;
        let r = &b["resolved"][0];
        assert_eq!(r["scope"], want, "{w}: {b}");
        assert_eq!(r["state"], "next_launch", "{w} is not running");
    }
    let (_, b, _, _) = call(&app, "GET", "/api/vault/secrets?worker=w3", None, true, None).await;
    assert_eq!(b["isolated"], true, "the provisioned view says the worker is isolated");

    // A worker sees only what applies to it, and cannot ask about another.
    let (_, b, _, _) = call(&app, "GET", "/api/vault/secrets?worker=w1", None, true, Some("w2")).await;
    let seen: Vec<&str> = b["items"].as_array().unwrap().iter().map(|i| i["scope"].as_str().unwrap()).collect();
    assert_eq!(seen, vec!["global"], "w2 sees only the global item: {b}");
    assert_eq!(b["resolved_for"], "w2", "the origin wins over ?worker=");

    // Edit: description and a move from group ops to group web.
    let (s, b, raw, _) = call(&app, "PATCH", &format!("/api/vault/secrets/{}", ids["group"]),
        Some(json!({"description": "moved", "scope": "group", "target": "web"})), true, None).await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(b["item"]["target"], "web");
    assert_eq!(b["item"]["version"], 1, "a move is not a rotation");
    let (_, b, _, _) = call(&app, "GET", "/api/vault/secrets?worker=w2", None, true, None).await;
    assert_eq!(b["resolved"][0]["scope"], "group", "w2 now gets the moved group item");
    let (s, _, _, _) = call(&app, "PATCH", &format!("/api/vault/secrets/{}", ids["group"]), Some(json!({"description": "x".repeat(201)})), true, None).await;
    assert_eq!(s, 400, "description is capped at 200");
    let (s, _, _, _) = call(&app, "PATCH", &format!("/api/vault/secrets/{}", ids["group"]), Some(json!({"value": "x"})), true, Some("w2")).await;
    assert_eq!(s, 403);

    // Rotate: version 2.
    let (s, b, raw, _) = call(&app, "PUT", &format!("/api/vault/secrets/{}", ids["worker"]), Some(json!({"value": CANARY_ROTATED})), true, None).await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(b["item"]["version"], 2);
    assert!(!raw.contains(CANARY_ROTATED));

    // Reveal: owner only, no-store, the rotated value.
    let path = format!("/api/vault/secrets/{}/reveal", ids["worker"]);
    let (s, _, raw, _) = call(&app, "POST", &path, None, true, Some("w1")).await;
    assert_eq!(s, 403, "even the worker the secret belongs to cannot reveal it: {raw}");
    let (s, _, raw, _) = call(&app, "POST", &path, None, false, None).await;
    assert!((s == 401 || s == 403) && !raw.contains(CANARY_ROTATED), "no bearer, no value");
    let (s, b, _, cc) = call(&app, "POST", &path, None, true, None).await;
    assert_eq!(s, 200);
    assert_eq!(b["value"], CANARY_ROTATED);
    assert!(cc.contains("no-store"), "reveal must not be cached: {cc}");
    assert!(b["remask_after_s"].as_u64().unwrap() > 0);

    // Effective access for the global item.
    let (s, b, raw, _) = call(&app, "GET", &format!("/api/vault/secrets/{}/access", ids["global"]), None, true, None).await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(b["n_considered"], 3);
    let row = |w: &str| b["workers"].as_array().unwrap().iter().find(|r| r["worker"] == w).cloned().unwrap();
    assert_eq!(row("w1")["state"], "overridden");
    assert_eq!(row("w1")["overridden_by"]["scope"], "worker");
    assert_eq!(row("w3")["effective"], true, "after the move, w3 falls back to global");
    assert_eq!(row("w3")["isolated"], true);
    let (s, _, _, _) = call(&app, "GET", &format!("/api/vault/secrets/{}/access", ids["global"]), None, true, Some("w1")).await;
    assert_eq!(s, 403);

    // Delete.
    let (s, _, _, _) = call(&app, "DELETE", &format!("/api/vault/secrets/{}", ids["global"]), None, true, Some("w2")).await;
    assert_eq!(s, 403);
    let (s, b, _, _) = call(&app, "DELETE", &format!("/api/vault/secrets/{}", ids["global"]), None, true, None).await;
    assert_eq!(s, 200);
    assert_eq!(b["still_held_by"], json!([]), "nobody is running");
    let (_, b, _, _) = call(&app, "GET", "/api/vault/secrets?worker=w3", None, true, None).await;
    assert_eq!(b["resolved"], json!([]), "w3 has nothing once global is gone");

    // Audit: every action, no value.
    let (s, b, raw, _) = call(&app, "GET", "/api/vault/secrets/audit", None, true, None).await;
    assert_eq!(s, 200);
    assert_eq!(b["measured"], true);
    for ev in ["secret_added", "secret_updated", "secret_replaced", "secret_revealed", "secret_deleted", "secret_write_refused"] {
        assert!(raw.contains(&format!("\"event\":\"{ev}\"")), "audit lacks {ev}: {raw}");
    }
    let (s, _, _, _) = call(&app, "GET", "/api/vault/secrets/audit", None, true, Some("w1")).await;
    assert_eq!(s, 403, "a worker cannot read the audit log");
    let (_, b, _, _) = call(&app, "GET", "/api/vault/secrets/audit?key=NOPE", None, true, None).await;
    assert_eq!(b["entries"], json!([]));

    // The canary sweep: tracing output, audit file, store, ledger.
    let log = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(log.contains("vault_secret_revealed") && log.contains("vault_secret_rotated"),
        "the verdicts must be in the log, or this sweep proves nothing");
    let audit = std::fs::read_to_string(home.join("logs/vault-audit.jsonl")).unwrap();
    let store = std::fs::read_to_string(home.join("vault/secrets.json")).unwrap();
    for v in [CANARY_GLOBAL, CANARY_GROUP, CANARY_WORKER, CANARY_ROTATED] {
        assert!(!log.contains(v), "a value reached the server log");
        assert!(!audit.contains(v), "a value reached the audit file");
        assert!(!store.contains(v), "a value is in the store as plaintext");
    }
}
