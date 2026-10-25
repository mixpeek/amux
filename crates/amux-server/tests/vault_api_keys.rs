//! Vault API keys: a key/value the owner creates from the API Keys tab is a
//! vault secret of kind `api_key`, and obeys exactly the same scope,
//! visibility and value rules as every other secret. One test function: it
//! sets AMUX_HOME for this process.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "test-owner-token-keys";
const CANARY_KEY: &str = "canary-apikey-3c9e71d2";
const CANARY_OLD: &str = "canary-oldsecret-a04b6f11";

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

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>, owner: bool, worker: Option<&str>) -> (u16, Value, String) {
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
    let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024).await.unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    (status, serde_json::from_str(&raw).unwrap_or(Value::Null), raw)
}

#[tokio::test]
async fn an_api_key_obeys_secret_scope_and_value_rules() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("amux-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(home.join("sessions/w_ops.env"), "CC_TAGS=\"ops\"\n").unwrap();
    std::fs::write(home.join("sessions/w_web.env"), "CC_TAGS=\"web\"\n").unwrap();
    std::fs::write(home.join("sessions/w_iso.env"), "CC_ISOLATED=1\nCC_TAGS=\"ops\"\n").unwrap();
    let tmux_dir = dir.path().join("tmux");
    std::fs::create_dir_all(&tmux_dir).unwrap();
    std::env::set_var("TMUX_TMPDIR", &tmux_dir);
    std::env::remove_var("TMUX");
    std::env::set_var("AMUX_HOME", &home);
    std::env::set_var("AMUX_VAULT_KEYSTORE", "file");
    let app = app(&home);

    // An existing plain secret, written without a kind, stays a secret.
    let (s, b, _) = call(&app, "POST", "/api/vault/secrets", Some(json!({"key": "OLD_TOKEN", "scope": "global", "value": CANARY_OLD})), true, None).await;
    assert_eq!(s, 201);
    assert_eq!(b["item"]["kind"], "secret");
    let old_id = b["item"]["id"].as_str().unwrap().to_string();

    // A worker cannot create an API key, and neither can a request without the
    // owner bearer.
    let key = json!({"key": "STRIPE_API_KEY", "kind": "api_key", "scope": "group", "target": "ops",
        "value": CANARY_KEY, "description": "ops billing key"});
    let (s, _, _) = call(&app, "POST", "/api/vault/secrets", Some(key.clone()), true, Some("w_ops")).await;
    assert_eq!(s, 403, "a worker origin must be refused");
    let (s, _, _) = call(&app, "POST", "/api/vault/secrets", Some(key.clone()), false, None).await;
    assert!(s == 401 || s == 403, "no owner bearer must be refused, got {s}");
    let (s, _, _) = call(&app, "POST", "/api/vault/secrets", Some(json!({"key": "X_KEY", "kind": "password", "value": "abcd"})), true, None).await;
    assert_eq!(s, 400, "an unknown kind is refused");

    let (s, b, raw) = call(&app, "POST", "/api/vault/secrets", Some(key), true, None).await;
    assert_eq!(s, 201, "{raw}");
    assert!(!raw.contains(CANARY_KEY), "the create response carried the value");
    assert_eq!(b["item"]["kind"], "api_key");
    assert_eq!(b["item"]["scope"], "group");
    let id = b["item"]["id"].as_str().unwrap().to_string();

    // Owner list: metadata, the kind, how far it reaches, no value.
    let (_, b, raw) = call(&app, "GET", "/api/vault/secrets", None, true, None).await;
    assert!(!raw.contains(CANARY_KEY) && !raw.contains(CANARY_OLD), "the list leaked a value");
    let it = b["items"].as_array().unwrap().iter().find(|i| i["id"] == id.as_str()).unwrap().clone();
    assert_eq!(it["kind"], "api_key");
    assert_eq!(it["effective_for"], 2, "reaches w_ops and the isolated w_iso, both in ops: {it}");
    let old = b["items"].as_array().unwrap().iter().find(|i| i["id"] == old_id.as_str()).unwrap().clone();
    assert_eq!(old["kind"], "secret", "the existing secret is untouched");

    // Scope: the worker outside the group does not get it, by any route.
    let (_, b, _) = call(&app, "GET", "/api/vault/secrets", None, true, Some("w_web")).await;
    let seen: Vec<&str> = b["items"].as_array().unwrap().iter().filter_map(|i| i["key"].as_str()).collect();
    assert_eq!(seen, vec!["OLD_TOKEN"], "w_web sees only the global secret: {b}");
    let (_, b, _) = call(&app, "GET", "/api/vault/secrets?worker=w_web", None, true, None).await;
    let resolved: Vec<&str> = b["resolved"].as_array().unwrap().iter().filter_map(|i| i["key"].as_str()).collect();
    assert!(!resolved.contains(&"STRIPE_API_KEY"), "w_web is provisioned with the ops key: {b}");
    for w in ["w_ops", "w_iso"] {
        let (_, b, _) = call(&app, "GET", &format!("/api/vault/secrets?worker={w}"), None, true, None).await;
        let resolved: Vec<&str> = b["resolved"].as_array().unwrap().iter().filter_map(|i| i["key"].as_str()).collect();
        assert!(resolved.contains(&"STRIPE_API_KEY"), "{w} is in ops and must get the key: {b}");
    }
    let (_, b, _) = call(&app, "GET", "/api/vault/secrets", None, true, Some("w_ops")).await;
    let mine = b["items"].as_array().unwrap().iter().find(|i| i["key"] == "STRIPE_API_KEY").cloned();
    assert!(mine.is_some_and(|m| m["kind"] == "api_key"), "w_ops sees the key's metadata: {b}");
    let (s, b, raw) = call(&app, "GET", &format!("/api/vault/secrets/{id}/access"), None, true, None).await;
    assert_eq!(s, 200, "{raw}");
    let row = |w: &str| b["workers"].as_array().unwrap().iter().find(|r| r["worker"] == w).cloned();
    assert!(row("w_web").is_none(), "w_web is outside the key's scope and has no access row: {b}");
    assert_eq!(row("w_ops").unwrap()["effective"], true, "{b}");

    // Value: only the owner can reveal it; a worker, even one in scope, cannot.
    let path = format!("/api/vault/secrets/{id}/reveal");
    let (s, _, raw) = call(&app, "POST", &path, None, true, Some("w_ops")).await;
    assert!(s == 403 && !raw.contains(CANARY_KEY), "a worker revealed an API key: {raw}");
    let (s, b, _) = call(&app, "POST", &path, None, true, None).await;
    assert_eq!(s, 200);
    assert_eq!(b["value"], CANARY_KEY);

    // Editing keeps the kind; moving it to another group moves who gets it.
    let (s, b, raw) = call(&app, "PATCH", &format!("/api/vault/secrets/{id}"), Some(json!({"scope": "group", "target": "web"})), true, None).await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(b["item"]["kind"], "api_key");
    let (_, b, _) = call(&app, "GET", "/api/vault/secrets?worker=w_web", None, true, None).await;
    assert!(b["resolved"].as_array().unwrap().iter().any(|i| i["key"] == "STRIPE_API_KEY"), "after the move w_web gets it: {b}");
    let (_, b, _) = call(&app, "GET", "/api/vault/secrets?worker=w_ops", None, true, None).await;
    assert!(!b["resolved"].as_array().unwrap().iter().any(|i| i["key"] == "STRIPE_API_KEY"), "after the move w_ops does not: {b}");

    // The store holds no plaintext; the audit line names the kind, not the value.
    let store = std::fs::read_to_string(home.join("vault/secrets.json")).unwrap();
    let audit = std::fs::read_to_string(home.join("logs/vault-audit.jsonl")).unwrap();
    assert!(audit.contains("\"kind\":\"api_key\""), "{audit}");
    for v in [CANARY_KEY, CANARY_OLD] {
        assert!(!store.contains(v), "a value is in the store as plaintext");
        assert!(!audit.contains(v), "a value reached the audit file");
    }
}
