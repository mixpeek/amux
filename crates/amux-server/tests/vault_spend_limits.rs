//! Vault payment-method spending limits, end to end through the real router.
//!
//! One test function on purpose: it sets AMUX_HOME for this process. The
//! ledger is seeded with spend just inside and just outside each calendar
//! window, then real order requests go through `/api/browser/complete-order`
//! and `/api/vault/{id}/fill`. A charge under every limit passes the gate and
//! then fails with 502 (no browser in a test home), which releases its
//! reservation; a charge over a limit is refused with 403 before any browser
//! is touched. The card number is a canary swept from every output.

use amux_server::api::{router, AppState};
use axum::body::Body;
use axum::http::Request;
use chrono::{Datelike, Local, TimeZone};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const TOKEN: &str = "test-owner-token-spend";
const CANARY_NUMBER: &str = "4111111111111111";

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

/// Local midnight at the start of today, this month and this year, computed
/// here independently of the server's own window code.
fn starts() -> (f64, f64, f64) {
    let now = Local::now();
    let at = |y: i32, m: u32, d: u32| Local.with_ymd_and_hms(y, m, d, 0, 0, 0).earliest().unwrap().timestamp() as f64;
    (at(now.year(), now.month(), now.day()), at(now.year(), now.month(), 1), at(now.year(), 1, 1))
}

fn seed(home: &std::path::Path, lines: &[Value]) {
    let p = home.join("vault/spend-ledger.jsonl");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn spend(id: &str, item: &str, ts: f64, amount: f64) -> Value {
    json!({"ev": "spend", "id": id, "ts": ts, "item": item, "amount_usd": amount, "currency": "USD",
        "merchant": "Seed", "purpose": "seeded", "session": "seed", "path": "seed"})
}

fn order(item_name: &str, amount: f64) -> Value {
    json!({"amount_usd": amount, "currency": "USD", "merchant": "Test Merchant", "purpose": "limit test",
        "session": "w1", "item_name": item_name})
}

#[tokio::test]
async fn spending_limits_are_enforced_at_every_window_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("amux-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::write(home.join("sessions/w1.env"), "CC_TAGS=\"ops\"\n").unwrap();
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

    // Add cards the way the dashboard does: owner, no worker origin.
    let mut ids = std::collections::BTreeMap::new();
    for (name, limits) in [
        ("daily card", json!({"daily": 50})),
        ("monthly card", json!({"monthly": 100})),
        ("yearly card", json!({"yearly": 1000})),
        ("open card", json!({})),
        ("approval card", json!({"daily": 50})),
    ] {
        let approval = if name == "approval card" { 4.0 } else { 100000.0 };
        let (s, b, raw) = call(&app, "POST", "/api/vault", Some(json!({"name": name, "kind": "card",
            "fields": {"number": CANARY_NUMBER, "exp": "12/39", "cvc": "321", "name": "Test Holder", "zip": "10001"},
            "rules": {"max_usd_without_approval": approval, "spend_limits_usd": limits}})), None).await;
        assert_eq!(s, 201, "{raw}");
        assert!(!raw.contains(CANARY_NUMBER), "create response carried a card value");
        assert_eq!(b["item"]["status"], "active");
        ids.insert(name, b["item"]["id"].as_str().unwrap().to_string());
    }
    let (s, _, _) = call(&app, "POST", "/api/vault", Some(json!({"name": "bad", "fields": {"number": CANARY_NUMBER},
        "rules": {"spend_limits_usd": {"weekly": 5}}})), None).await;
    assert_eq!(s, 400, "an unknown window is refused");
    let (s, _, _) = call(&app, "POST", "/api/vault", Some(json!({"name": "bad", "fields": {"number": CANARY_NUMBER},
        "rules": {"spend_limits_usd": {"daily": -1}}})), None).await;
    assert_eq!(s, 400, "a negative limit is refused");

    // Seed: just inside each window, and a large amount just outside it.
    let (day, month, year) = starts();
    seed(&home, &[
        spend("spd_d_in", &ids["daily card"], day + 1.0, 40.0),
        spend("spd_d_out", &ids["daily card"], day - 1.0, 1000.0),
        spend("spd_m_in", &ids["monthly card"], month + 1.0, 90.0),
        spend("spd_m_out", &ids["monthly card"], month - 1.0, 5000.0),
        spend("spd_y_in", &ids["yearly card"], year + 1.0, 900.0),
        spend("spd_y_out", &ids["yearly card"], year - 1.0, 50000.0),
        spend("spd_open", &ids["open card"], day + 1.0, 1_000_000.0),
        spend("spd_a_in", &ids["approval card"], day + 1.0, 45.0),
    ]);

    // Over the limit by one cent: refused before any browser, naming the window.
    for (name, amount, window) in [("daily card", 10.01, "daily"), ("monthly card", 10.01, "monthly"), ("yearly card", 100.01, "yearly")] {
        let (s, b, raw) = call(&app, "POST", "/api/browser/complete-order", Some(order(name, amount)), Some("w1")).await;
        assert_eq!(s, 403, "{name} ${amount}: {raw}");
        assert_eq!(b["limit_exceeded"], true, "{raw}");
        assert_eq!(b["window"], window, "{raw}");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        assert!(b["resets_at"].as_f64().unwrap() > now, "the window resets in the future: {raw}");
    }
    // Exactly at the limit: let through the gate (then 502, no browser here).
    for (name, amount) in [("daily card", 10.0), ("monthly card", 10.0), ("yearly card", 100.0)] {
        let (s, b, raw) = call(&app, "POST", "/api/browser/complete-order", Some(order(name, amount)), Some("w1")).await;
        assert_eq!(s, 502, "{name} ${amount} is at the limit and must pass the gate: {raw}");
        assert!(b.get("limit_exceeded").is_none(), "{raw}");
    }
    // The 502s released their reservations (the card never reached a page),
    // so the same charge passes again.
    let (s, _, raw) = call(&app, "POST", "/api/browser/complete-order", Some(order("daily card", 10.0)), Some("w1")).await;
    assert_eq!(s, 502, "a released reservation must not count: {raw}");
    let ledger = std::fs::read_to_string(home.join("vault/spend-ledger.jsonl")).unwrap();
    assert_eq!(ledger.matches("\"ev\":\"release\"").count(), 4, "every no-browser charge is released: {ledger}");

    // No limit configured: the old behaviour, whatever has been spent.
    let (s, _, raw) = call(&app, "POST", "/api/browser/complete-order", Some(order("open card", 5000.0)), Some("w1")).await;
    assert_eq!(s, 502, "a card with no limits is only gated by its approval rule: {raw}");

    // The per-card fill path takes the same gate.
    let (s, b, raw) = call(&app, "POST", &format!("/api/vault/{}/fill", ids["daily card"]),
        Some(json!({"amount_usd": 10.01, "merchant": "Test Merchant", "purpose": "fill path", "session": "w1"})), Some("w1")).await;
    assert_eq!(s, 403, "{raw}");
    assert_eq!(b["window"], "daily");
    let (s, _, raw) = call(&app, "POST", &format!("/api/vault/{}/fill", ids["daily card"]),
        Some(json!({"amount_usd": 10.0, "merchant": "Test Merchant", "purpose": "fill path", "session": "w1"})), Some("w1")).await;
    assert_eq!(s, 502, "{raw}");

    // An approval cannot lift a limit: over the $4 approval threshold AND over
    // the daily limit is a limit refusal, and no grant is minted for it.
    let (s, b, raw) = call(&app, "POST", "/api/browser/complete-order", Some(order("approval card", 10.0)), Some("w1")).await;
    assert_eq!(s, 403, "{raw}");
    assert_eq!(b["limit_exceeded"], true, "{raw}");
    assert!(b.get("requires_approval").is_none(), "a limit breach must not ask for approval: {raw}");
    // Under the limit but over the threshold: the approval flow, as before.
    let (s, b, raw) = call(&app, "POST", "/api/browser/complete-order", Some(order("approval card", 5.0)), Some("w1")).await;
    assert_eq!(s, 403, "{raw}");
    assert_eq!(b["requires_approval"], true, "{raw}");

    // The owner raises a limit; a worker cannot.
    let (s, _, _) = call(&app, "PATCH", &format!("/api/vault/{}", ids["daily card"]),
        Some(json!({"rules": {"spend_limits_usd": {"daily": 1000}}})), Some("w1")).await;
    assert_eq!(s, 403, "a worker may not change a limit");
    let (s, b, raw) = call(&app, "PATCH", &format!("/api/vault/{}", ids["daily card"]),
        Some(json!({"rules": {"spend_limits_usd": {"daily": 60, "monthly": 5000}}})), None).await;
    assert_eq!(s, 200, "{raw}");
    assert_eq!(b["item"]["rules"]["spend_limits_usd"], json!({"daily": 60.0, "monthly": 5000.0}));
    let (s, _, raw) = call(&app, "POST", "/api/browser/complete-order", Some(order("daily card", 20.0)), Some("w1")).await;
    assert_eq!(s, 502, "after the raise, $40 + $20 fits $60: {raw}");
    let (s, b, _) = call(&app, "PATCH", &format!("/api/vault/{}", ids["daily card"]),
        Some(json!({"rules": {"spend_limits_usd": {"monthly": null}}})), None).await;
    assert_eq!(s, 200);
    assert_eq!(b["item"]["rules"]["spend_limits_usd"], json!({"daily": 60.0}), "null clears one window");

    // The list shows what each window has spent, beside the limits.
    let (s, b, raw) = call(&app, "GET", "/api/vault", None, None).await;
    assert_eq!(s, 200);
    let item = b["items"].as_array().unwrap().iter().find(|i| i["id"] == ids["monthly card"].as_str()).unwrap().clone();
    assert_eq!(item["spent_usd"]["monthly"], json!(90.0), "{item}");
    assert!(!raw.contains(CANARY_NUMBER), "the list carried a card value");

    // Canary sweep: no card value in the log, the audit file or the ledger.
    let log = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    assert!(log.contains("vault_spend_limit_exceeded"), "the refusal verdict must be in the log, or the sweep proves nothing");
    let audit = std::fs::read_to_string(home.join("logs/vault-audit.jsonl")).unwrap();
    assert!(audit.contains("refused_spend_limit"), "{audit}");
    let ledger = std::fs::read_to_string(home.join("vault/spend-ledger.jsonl")).unwrap();
    assert!(!log.contains(CANARY_NUMBER), "a card number reached the server log");
    assert!(!audit.contains(CANARY_NUMBER), "a card number reached the audit file");
    assert!(!ledger.contains(CANARY_NUMBER), "a card number reached the spend ledger");
}
