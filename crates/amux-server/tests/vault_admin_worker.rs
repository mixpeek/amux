//! Admin-group workers act with the owner's reach on the vault: they add items
//! as active, change rules and remove items. Any other worker still gets the
//! pending/refused path. Through the real router, with worker groups read from
//! real session env files.
//!
//! One test function on purpose: it sets AMUX_HOME for this process.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const TOKEN: &str = "test-owner-token-vault-admin";
const CANARY_NUMBER: &str = "4111111111111111";
const CANARY_PASSWORD: &str = "canary-pw-9f3k2";

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

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>, worker: Option<&str>) -> (u16, Value, String) {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {TOKEN}"));
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

fn card(name: &str) -> Value {
    json!({"name": name, "kind": "card",
        "fields": {"number": CANARY_NUMBER, "exp": "12/39", "cvc": "321", "name": "Test Holder", "zip": "10001"},
        "rules": {"max_usd_without_approval": 25.0}})
}

#[tokio::test]
async fn admin_group_workers_add_edit_and_remove_vault_items_and_others_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("amux-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(home.join("sessions/w1.env"), "CC_TAGS=\"ops\"\n").unwrap();
    std::fs::write(home.join("sessions/boss.env"), "CC_TAGS=\"ops,admin\"\n").unwrap();
    let tmux_dir = dir.path().join("tmux");
    std::fs::create_dir_all(&tmux_dir).unwrap();
    std::env::set_var("TMUX_TMPDIR", &tmux_dir);
    std::env::remove_var("TMUX");
    std::env::set_var("AMUX_HOME", &home);
    let logs = Buf::default();
    let sink = logs.clone();
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .try_init();
    let app = app(&home);

    // An ordinary worker: its card is held pending, and it cannot edit or remove.
    let (s, b, raw) = call(&app, "POST", "/api/vault", Some(card("worker card")), Some("w1")).await;
    assert_eq!(s, 201, "{raw}");
    assert_eq!(b["item"]["status"], "pending_owner_activation", "{raw}");
    let pending = b["item"]["id"].as_str().unwrap().to_string();
    let (s, _, raw) = call(&app, "PATCH", &format!("/api/vault/{pending}"), Some(json!({"activate": true})), Some("w1")).await;
    assert_eq!(s, 403, "a non-admin worker may not activate: {raw}");
    let (s, _, raw) = call(&app, "DELETE", &format!("/api/vault/{pending}"), None, Some("w1")).await;
    assert_eq!(s, 403, "a non-admin worker may not delete: {raw}");

    // An admin-group worker: active at once, for a card and a login.
    let (s, b, raw) = call(&app, "POST", "/api/vault", Some(card("admin card")), Some("boss")).await;
    assert_eq!(s, 201, "{raw}");
    assert_eq!(b["item"]["status"], "active", "{raw}");
    assert!(!raw.contains(CANARY_NUMBER), "create response carried a card value");
    let admin_card = b["item"]["id"].as_str().unwrap().to_string();
    let (s, b, raw) = call(&app, "POST", "/api/vault", Some(json!({"name": "admin login", "kind": "login",
        "fields": {"username": "ops@example.com", "password": CANARY_PASSWORD},
        "rules": {"origins": ["example.com"]}})), Some("boss")).await;
    assert_eq!(s, 201, "{raw}");
    assert_eq!(b["item"]["status"], "active", "{raw}");
    assert!(!raw.contains(CANARY_PASSWORD), "create response carried the password");

    // It edits: activates the ordinary worker's pending card and changes rules.
    let (s, b, raw) = call(&app, "PATCH", &format!("/api/vault/{pending}"), Some(json!({"activate": true})), Some("boss")).await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(b["item"]["status"], "active");
    let (s, b, raw) = call(&app, "PATCH", &format!("/api/vault/{admin_card}"),
        Some(json!({"rules": {"max_usd_without_approval": 40.0, "spend_limits_usd": {"daily": 100}}})), Some("boss")).await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(b["item"]["rules"]["max_usd_without_approval"], json!(40.0));
    assert_eq!(b["item"]["rules"]["spend_limits_usd"], json!({"daily": 100.0}));

    // And removes.
    let (s, _, raw) = call(&app, "DELETE", &format!("/api/vault/{pending}"), None, Some("boss")).await;
    assert_eq!(s, 200, "{raw}");
    let (_, list, raw) = call(&app, "GET", "/api/vault", None, None).await;
    let names: Vec<&str> = list["items"].as_array().map(|a| a.iter().filter_map(|i| i["name"].as_str()).collect()).unwrap_or_default();
    assert!(!names.contains(&"worker card") && names.contains(&"admin card"), "{raw}");

    // Each admin pass is logged and attributed; no value reached the log or the audit.
    let log = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    for gate in ["vault_create", "vault_update", "vault_delete"] {
        assert!(log.contains("admin_group_owner_gate") && log.contains(gate), "no admin pass logged for {gate}");
    }
    let audit = std::fs::read_to_string(home.join("logs/vault-audit.jsonl")).unwrap_or_default();
    assert!(audit.contains("\"by\":\"boss\""), "admin edits are attributed in the vault audit: {audit}");
    for text in [&log, &audit] {
        assert!(!text.contains(CANARY_NUMBER) && !text.contains(CANARY_PASSWORD), "a vault value leaked");
    }
}
