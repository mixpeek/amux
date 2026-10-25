//! The move bundle end to end (docs/design/cloud-self-service.md 7.1): export
//! from one server's state, import into another, through the real router and
//! two temp-file stores. Never ~/.amux/amux.db.
//!
//! What must hold:
//! - dry run is the default and writes nothing;
//! - apply adds exactly the rows the dry run counted;
//! - a second apply adds nothing (merge by key, never duplicate);
//! - a table off the allowlist, or a newer schema, refuses the whole bundle;
//! - an org member is refused; only the host owner moves a server.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// One empty amux home for the whole binary, so config export/apply never
/// reads or writes the real ~/.amux/sessions.
fn pin_home() {
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| tempfile::tempdir().unwrap().keep());
    std::env::set_var("AMUX_HOME", home);
}

fn server() -> (axum::Router, tempfile::TempDir) {
    pin_home();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("move-test.db")).unwrap();
    let state = AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<String>,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(s) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(s))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let h = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, h, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn export(app: &axum::Router) -> Value {
    let (st, _, v) = send(app, "GET", "/api/move/export", None, &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

fn rows(bundle: &Value, table: &str) -> usize {
    bundle["tables"][table].as_array().map(Vec::len).unwrap_or(0)
}

fn total(report: &Value, field: &str) -> u64 {
    report["tables"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(|t| t[field].as_u64())
        .sum()
}

async fn seed(app: &axum::Router) {
    for title in ["move me one", "move me two"] {
        let body = json!({"title": title, "type": "chore", "status": "backlog"}).to_string();
        let (st, _, v) = send(app, "POST", "/api/board", Some(body), &[]).await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
    }
    let (st, _, v) = send(
        app,
        "POST",
        "/api/crm/contacts",
        Some(json!({"name": "Ada Lovelace", "company": "Analytical"}).to_string()),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
}

#[tokio::test]
async fn export_then_import_dry_run_apply_and_reapply() {
    let (a, _da) = server();
    let (b, _db) = server();
    seed(&a).await;

    let bundle = export(&a).await;
    assert_eq!(bundle["format"], "amuxmove");
    assert_eq!(bundle["version"], 1);
    assert!(bundle["schema_version"].as_i64().unwrap() > 0);
    assert!(bundle["config"].is_object(), "{}", bundle["config"]);
    let issues = rows(&bundle, "issues");
    assert!(issues >= 2, "both seeded cards exported: {}", bundle["tables"]["issues"]);
    assert_eq!(rows(&bundle, "crm_contacts"), 1);
    for t in bundle["tables"].as_object().unwrap().keys() {
        assert!(!t.contains("token") && !t.contains("oauth"), "secret table {t} exported");
    }
    let body = bundle.to_string();
    let before_b = rows(&export(&b).await, "issues");

    // Dry run is the default: counts, and nothing written.
    let (st, _, dry) = send(&b, "POST", "/api/move/import", Some(body.clone()), &[]).await;
    assert_eq!(st, StatusCode::OK, "{dry}");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["tables"]["crm_contacts"]["new"], 1, "{dry}");
    assert_eq!(dry["tables"]["crm_contacts"]["existing"], 0, "{dry}");
    let dry_new = total(&dry, "new");
    assert!(dry_new >= 3, "{dry}");
    assert!(dry["config"]["summary"].is_string(), "{dry}");
    assert_eq!(rows(&export(&b).await, "issues"), before_b, "a dry run wrote rows");

    // Apply adds exactly what the dry run counted.
    let (st, _, applied) =
        send(&b, "POST", "/api/move/import?dry_run=0", Some(body.clone()), &[]).await;
    assert_eq!(st, StatusCode::OK, "{applied}");
    assert_eq!(applied["dry_run"], false);
    assert_eq!(total(&applied, "new"), dry_new, "{applied}");
    let after = export(&b).await;
    assert_eq!(rows(&after, "crm_contacts"), 1);
    let titles: Vec<&str> = after["tables"]["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["title"].as_str())
        .collect();
    assert!(titles.contains(&"move me one") && titles.contains(&"move me two"), "{titles:?}");

    // A second apply imports nothing new and overwrites nothing.
    let (st, _, again) = send(&b, "POST", "/api/move/import?dry_run=0", Some(body), &[]).await;
    assert_eq!(st, StatusCode::OK, "{again}");
    assert_eq!(total(&again, "new"), 0, "{again}");
    assert_eq!(total(&again, "existing"), dry_new, "{again}");
    assert_eq!(rows(&export(&b).await, "issues"), rows(&after, "issues"));

    // A card minted on B after the import must not reuse an imported id.
    let (st, _, fresh) = send(
        &b,
        "POST",
        "/api/board",
        Some(json!({"title": "made after import", "type": "chore", "status": "backlog"}).to_string()),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{fresh}");
}

#[tokio::test]
async fn a_table_off_the_allowlist_refuses_the_whole_bundle() {
    let (a, _da) = server();
    let (b, _db) = server();
    seed(&a).await;
    let mut bundle = export(&a).await;
    bundle["tables"]["share_tokens"] =
        json!([{"token": "t", "session": "x", "perms": "rw", "created_at": 1}]);
    for q in ["", "?dry_run=0"] {
        let (st, _, v) = send(
            &b,
            "POST",
            &format!("/api/move/import{q}"),
            Some(bundle.to_string()),
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"].as_str().unwrap().contains("share_tokens"), "{v}");
    }
    // Nothing from the refused bundle landed, not even the allowlisted part.
    assert_eq!(rows(&export(&b).await, "crm_contacts"), 0);
}

#[tokio::test]
async fn a_newer_schema_or_foreign_format_is_refused() {
    let (a, _da) = server();
    let (b, _db) = server();
    let mut bundle = export(&a).await;
    bundle["schema_version"] = json!(bundle["schema_version"].as_i64().unwrap() + 1);
    let (st, _, v) = send(&b, "POST", "/api/move/import", Some(bundle.to_string()), &[]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("newer"), "{v}");

    let raw = json!({"format": "sqlite-dump", "version": 1, "schema_version": 1, "tables": {}});
    let (st, _, v) = send(&b, "POST", "/api/move/import", Some(raw.to_string()), &[]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test]
async fn an_org_member_cannot_export_or_import() {
    let (a, _da) = server();
    let (b, _db) = server();
    seed(&a).await;
    let body = export(&a).await.to_string();

    // A GLOBAL member: the widest member scope is still not the host owner.
    let (st, _, invite) = send(
        &b,
        "POST",
        "/api/org/invites",
        Some(json!({"email": "mate@example.com", "scope_level": "global", "grant_host_access": true}).to_string()),
        &[("host", "b-host:8824"), ("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{invite}");
    let token = invite["token"].as_str().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/invite/{token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("email=mate%40example.com&name=Mate"))
        .unwrap();
    let res = b.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let cookie = res.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // The cookie really is a member identity (so the refusals below are about
    // the move routes, not a broken sign-in).
    let (st, _, id) = send(&b, "GET", "/api/identity", None, &[("cookie", &cookie)]).await;
    assert_eq!(st, StatusCode::OK, "{id}");
    assert_eq!(id["is_local_member"], true, "{id}");

    for (method, path, body) in [
        ("POST", "/api/move/import?dry_run=0", Some(body.clone())),
        ("POST", "/api/move/import", Some(body.clone())),
        ("GET", "/api/move/export", None),
    ] {
        let (st, _, v) = send(&b, method, path, body, &[("cookie", &cookie)]).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{method} {path}: {v}");
    }
    assert_eq!(rows(&export(&b).await, "crm_contacts"), 0, "a member's import landed");
}

#[tokio::test]
async fn an_agent_session_may_dry_run_but_not_apply() {
    let (a, _da) = server();
    let (b, _db) = server();
    seed(&a).await;
    let body = export(&a).await.to_string();
    let hdr = [("x-amux-session", "some-worker")];
    let (st, _, v) = send(&b, "POST", "/api/move/import", Some(body.clone()), &hdr).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, _, v) = send(&b, "POST", "/api/move/import?dry_run=0", Some(body), &hdr).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(rows(&export(&b).await, "crm_contacts"), 0);
}

/// `amux import` sends the bundle gzipped, and a bundle past the old 100 MB
/// ceiling still imports (amux Cloud e2e 2026-10-18: a real 239 MB board got
/// 413). Imported schedules arrive paused, so a move never fires one twice.
#[tokio::test]
async fn a_gzipped_bundle_past_100_mb_imports_with_schedules_paused() {
    use std::io::Write;
    let (a, _da) = server();
    let (b, _db) = server();
    seed(&a).await;
    let (st, _, v) = send(
        &a,
        "POST",
        "/api/schedules",
        Some(json!({"title": "nightly", "schedule_expr": "every 1h", "command": "true", "kind": "shell"}).to_string()),
        &[],
    )
    .await;
    assert!(st.is_success(), "{v}");
    let mut bundle = export(&a).await;
    assert_eq!(bundle["tables"]["schedules"][0]["enabled"], 1, "source schedule is on");
    // Card history is what makes a real bundle big; 101 MB of it here.
    bundle["tables"]["issues"][0]["desc"] = json!("x".repeat(101 * 1024 * 1024));
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(bundle.to_string().as_bytes()).unwrap();
    let gz = gz.finish().unwrap();

    let req = Request::builder()
        .method("POST")
        .uri("/api/move/import?dry_run=0")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_ENCODING, "gzip")
        .body(Body::from(gz))
        .unwrap();
    let res = b.clone().oneshot(req).await.unwrap();
    let st = res.status();
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap_or(Value::Null);
    assert_eq!(st, StatusCode::OK, "{}", body.to_string().chars().take(400).collect::<String>());
    assert_eq!(body["tables"]["schedules"]["new"], 1, "{}", body["tables"]);
    assert!(body["note"].as_str().unwrap_or("").contains("paused"), "{}", body["note"]);

    let after = export(&b).await;
    let sched = &after["tables"]["schedules"][0];
    assert_eq!(sched["title"], "nightly");
    assert_eq!(sched["enabled"], 0, "imported schedule must arrive paused: {sched}");
}
