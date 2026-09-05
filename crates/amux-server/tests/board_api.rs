//! Board API integration tests (RR-0049, RR-0055; Invariants 3, 18, 37, 40
//! and the L1 payload lesson), run with `tower::ServiceExt::oneshot` against
//! the real router + a temp-file store — never against ~/.amux/amux.db.
//!
//! The Python-interop test hand-INSERTs a row shaped exactly like a live
//! Python row (int timestamps, the `needsyou` spelling, `` `HH:MM` `` log
//! lines, JSON-array depends_on) and asserts the Rust API round-trips it
//! without corrupting a single column the Python server reads — the
//! strangler-fig requirement (Phase 11: both servers, same rows).

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Evidence that satisfies the global AF-321 `done` constraint.
///
/// The tests below assert about gate ACKS, retyping, reviewers and the
/// lifecycle — not about evidence. They carry this so a constraint they are
/// not testing does not decide their result, which is the same reason they
/// already carry an asset link. The evidence gate has its own coverage in
/// `done_requires_evidence_a_plan_cannot_supply`.
const EV: &str = "ran `cargo test -p amux-server`";

fn app_with_store() -> (axum::Router, std::sync::Arc<Store>, tempfile::TempDir) {
    std::env::set_var("AMUX_DONE_LINK_REQUIRED", "1");
    std::env::set_var("AMUX_DONE_EVIDENCE_REQUIRED", "1");
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), store, dir)
}

fn app() -> (axum::Router, tempfile::TempDir) {
    // Pin the global done-link gate ON so this suite is hermetic (not dependent
    // on whether the real ~/.amux disables it): the gate tests here provide a
    // link where they reach done, and done_requires_an_asset_link asserts it.
    std::env::set_var("AMUX_DONE_LINK_REQUIRED", "1");
    std::env::set_var("AMUX_DONE_EVIDENCE_REQUIRED", "1");
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("amux-test.db")).unwrap();
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn send_with(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, headers, v)
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    send_with(app, method, path, body, &[]).await
}

async fn create(app: &axum::Router, body: Value) -> Value {
    let (st, _, v) = send(app, "POST", "/api/board", Some(body)).await;
    assert_eq!(st, StatusCode::CREATED, "create failed: {v}");
    v
}

fn hdr<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .map(|v| v.to_str().unwrap())
        .unwrap_or_else(|| panic!("missing header {name}"))
}

// ---- create -> list -> detail lifecycle ----------------------------------

#[tokio::test]
async fn create_list_detail_lifecycle() {
    let (app, _dir) = app();

    // Session-derived prefix + shared-counter minting: my-project -> MP-1,
    // MP-2; no session -> AMUX-1.
    let a = create(
        &app,
        json!({ "title": "First card", "session": "my-project", "desc": "line one\nline two" }),
    )
    .await;
    assert_eq!(a["id"], json!("MP-1"));
    assert_eq!(a["status"], json!("todo"));
    assert_eq!(a["type"], json!("code"));
    assert_eq!(a["session"], json!("my-project"));
    assert_eq!(a["owner_type"], json!("agent"));
    assert!(a["created"].is_i64(), "created must be unix INTEGER seconds");
    assert!(a["updated"].is_i64());

    let b = create(&app, json!({ "title": "Second", "session": "my-project" })).await;
    assert_eq!(b["id"], json!("MP-2"));
    let c = create(&app, json!({ "title": "No lane" })).await;
    assert_eq!(c["id"], json!("AMUX-1"));

    // Missing title is a 400.
    let (st, _, v) = send(&app, "POST", "/api/board", Some(json!({ "desc": "x" }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], json!("missing title"));

    // List: a BARE JSON ARRAY (the Python dashboard parses exactly that).
    let (st, _, list) = send(&app, "GET", "/api/board", None).await;
    assert_eq!(st, StatusCode::OK);
    let arr = list.as_array().expect("list must be a bare array");
    assert_eq!(arr.len(), 3);
    assert!(arr.iter().any(|i| i["id"] == json!("MP-1")));

    // Detail: full desc, log field present, ETag carries the rev.
    let (st, headers, detail) = send(&app, "GET", "/api/board/MP-1", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["desc"], json!("line one\nline two"));
    assert!(detail.get("log").is_some());
    assert_eq!(hdr(&headers, "etag"), "W/\"MP-1-0\"");

    // Unknown id -> 404.
    let (st, _, _) = send(&app, "GET", "/api/board/NOPE-1", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // MO-3038: session OMITTED + X-Amux-Session header -> the sender's lane.
    let (st, _, v) = send_with(
        &app,
        "POST",
        "/api/board",
        Some(json!({ "title": "header lane" })),
        &[("X-Amux-Session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(v["session"], json!("orch"));
    assert_eq!(v["id"], json!("ORCH-1"));
    assert_eq!(v["creator"], json!("orch"));
}

#[tokio::test]
async fn capture_decomposition_is_atomic_ordered_and_keeps_the_message_on_the_epic() {
    let (app, store, _dir) = app_with_store();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO issues \
                 (id,title,desc,status,session,type,creator,owner_type,source,created,updated) \
                 VALUES ('ATE-1','Captured request','**Prompt:** build and verify it', \
                         'doing','amux-testing-e2e','code','amux','agent','capture',1,1)",
                [],
            )?;
            conn.execute(
                "INSERT INTO cmd_history (id,text,type,session,ts,origin,card_id) \
                 VALUES (39225,'build and verify it','user','amux-testing-e2e',1000,'browser','ATE-1')",
                [],
            )?;
            conn.execute(
                "INSERT INTO issue_counters (prefix,next_n) VALUES ('ATE',2)",
                [],
            )?;
            Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();

    let plan = json!({"tasks":[
        {"title":"Implement the change","description":"Build the requested behavior.",
         "type":"code","priority":0,"depends_on":[],"next_action":"Implement the requested behavior"},
        {"title":"Verify the change","description":"Exercise the complete flow.",
         "type":"investigation","priority":1,"depends_on":[1],"next_action":"Run the end to end verification"},
        {"title":"Document the result","description":"Record the verified outcome.",
         "type":"doc","priority":2,"depends_on":[2],"next_action":"Write the final result summary"}
    ]});
    let (st, _, made) = send_with(
        &app,
        "POST",
        "/api/board/ATE-1/decompose",
        Some(plan.clone()),
        &[("X-Amux-Worker", "amux-testing-e2e")],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{made}");
    assert_eq!(made["epic"]["type"], json!("epic"));
    let tasks = made["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0]["status"], json!("todo"));
    assert_eq!(tasks[1]["status"], json!("backlog"));
    assert_eq!(tasks[1]["depends_on"], json!([tasks[0]["id"].clone()]));
    assert_eq!(tasks[2]["depends_on"], json!([tasks[1]["id"].clone()]));
    assert_eq!(tasks[0]["tags"], json!(["p0"]));
    assert_eq!(tasks[1]["epic"], json!("ATE-1"));
    assert_eq!(tasks[0]["session"], json!("amux-testing-e2e"));

    let (st, _, detail) = send(&app, "GET", "/api/board/ATE-1", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["children"].as_array().unwrap().len(), 3);
    assert_eq!(detail["messages"][0]["id"], json!(39225));
    assert_eq!(detail["messages"][0]["card_id"], json!("ATE-1"));

    let first_id = tasks[0]["id"].as_str().unwrap();
    let (st, _, artifact) = send(
        &app,
        "POST",
        &format!("/api/board/{first_id}/artifacts"),
        Some(json!({"kind":"implementation","ref":"/tmp/result.md","state":"created"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{artifact}");
    let (_, _, child_detail) = send(&app, "GET", &format!("/api/board/{first_id}"), None).await;
    assert_eq!(child_detail["epic"], json!("ATE-1"));
    assert_eq!(child_detail["messages"][0]["id"], json!(39225));
    assert_eq!(child_detail["artifacts"][0]["ref"], json!("/tmp/result.md"));
    assert!(
        child_detail["log"].as_str().unwrap().contains("artifact (api-anonymous): implementation/created /tmp/result.md"),
        "artifact registration must be part of the task action history: {child_detail}"
    );

    // A retry is idempotent: it reports the existing plan instead of minting
    // a second set of leaves after a client loses the first response.
    let (st, _, retried) = send_with(
        &app,
        "POST",
        "/api/board/ATE-1/decompose",
        Some(plan),
        &[("X-Amux-Worker", "amux-testing-e2e")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(retried["idempotent"], json!(true));
    assert_eq!(retried["tasks"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn invalid_decomposition_creates_no_partial_children() {
    let (app, store, _dir) = app_with_store();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO issues \
                 (id,title,desc,status,session,type,creator,owner_type,source,created,updated) \
                 VALUES ('ATE-9','Captured request','**Prompt:** bad plan','doing','lane', \
                         'code','amux','agent','capture',1,1)",
                [],
            )?;
            Ok(amux_server::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    let (st, _, body) = send_with(
        &app,
        "POST",
        "/api/board/ATE-9/decompose",
        Some(json!({"tasks":[
            {"title":"First","priority":0,"depends_on":[],"next_action":"Implement the first step"},
            {"title":"Second","priority":1,"depends_on":[2],"next_action":"Implement the second step"}
        ]})),
        &[("X-Amux-Worker", "lane")],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let children: i64 = store
        .read()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM issues WHERE epic='ATE-9'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(children, 0);
}

// ---- List payload shapes: slim by default, prose on request --------------
//
// History, because this contract moved TWICE and each move was deliberate.
// The earlier L1 slimming (first-line desc + log_n) broke the Python-era SPA,
// which read prose off the LIST payload (AMUX-2586 fix #4) — so the plain
// list went full-prose. AMUX-2840 then gave slim the derived facts the SPA
// actually renders, the poll moved to slim=1, and AMUX-3496 flipped the
// DEFAULT: 1,657 live cards were carrying 6MB of prose to consumers that
// render three fields. A reader that needs desc/log asks with ?full=1 (or
// legacy slim=0); `.desc` on a default row is a loud KeyError, not a
// silently empty string. Detail is unchanged.

#[tokio::test]
async fn list_is_slim_by_default_and_serves_prose_only_on_request() {
    let (app, _dir) = app();
    let long_desc = format!("first line {}\nsecond line body", "x".repeat(300));
    let v = create(&app, json!({ "title": "Big desc", "desc": long_desc })).await;
    let id = v["id"].as_str().unwrap().to_string();
    // Give the card a log line via a desc-edit PATCH (log is system-appended).
    let (_, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc": format!("{long_desc} edited") })),
    )
    .await;

    // DEFAULT: slim-shaped — no prose, all four derived facts.
    let (_, _, list) = send(&app, "GET", "/api/board", None).await;
    let item = &list.as_array().unwrap()[0];
    assert!(item.get("desc").is_none(), "default list must not carry desc");
    assert!(item.get("log").is_none(), "default list must not carry log");
    // `> 0`, not `is_some()` (AF-346). A blanked loader returns 0 for both, and
    // `is_some()` is true of 0 — so the pair read as passing while every card on
    // the board carried desc_len 0. The desc_head assertion below is what actually
    // caught the 2026-08-30 regression; these two were present and could not have.
    assert!(
        item["desc_len"].as_u64().unwrap_or(0) > 0,
        "a card created with a 300-char desc must report a non-zero desc_len"
    );
    assert!(
        item["log_n"].as_u64().unwrap_or(0) > 0,
        "the PATCH above appends a log line, so log_n must be non-zero"
    );
    assert!(item["desc_head"].as_str().unwrap().starts_with("first line"));
    assert!(item.get("folded_n").is_some());

    // ?full=1: the WHOLE desc, the WHOLE log (string or null), none of the
    // slim-only keys.
    for fat in ["/api/board?full=1", "/api/board?slim=0"] {
        let (_, _, full) = send(&app, "GET", fat, None).await;
        let item = &full.as_array().unwrap()[0];
        assert!(
            item["desc"].as_str().unwrap().contains("second line"),
            "{fat} must serve full desc"
        );
        assert!(item.get("desc_truncated").is_none());
        assert!(item.get("log_n").is_none());
        assert!(item.get("desc_len").is_none());
        assert!(
            item.get("log").is_some(),
            "{fat}: log must be present (prose consumers hydrate from it)"
        );
    }

    // ?slim=1 explicitly: same diet as the default (the dashboard poll).
    let (_, _, slim) = send(&app, "GET", "/api/board?slim=1", None).await;
    let item = &slim.as_array().unwrap()[0];
    assert!(item.get("desc").is_none());
    assert!(item.get("log").is_none());
    assert!(item["desc_len"].as_u64().is_some());
    assert!(item["log_n"].as_u64().is_some());

    // full=1 beats a stray slim=1 (explicit prose request wins).
    let (_, _, both) = send(&app, "GET", "/api/board?slim=1&full=1", None).await;
    assert!(both.as_array().unwrap()[0].get("desc").is_some());

    // Detail: the whole desc, as ever.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert!(detail["desc"].as_str().unwrap().contains("second line"));
    assert!(detail.get("desc_truncated").is_none());
}

// ---- GET /api/board/statuses (AMUX-2596) ---------------------------------
//
// The SPA builds its kanban columns from this list and silently falls back
// to a hardcoded default set on any failure — a 404 here meant custom
// Python-configured columns never rendered on the Rust origin.

#[tokio::test]
async fn board_statuses_serves_columns_or_python_defaults() {
    let (app, _dir) = app();
    let (st, _, v) = send(&app, "GET", "/api/board/statuses", None).await;
    assert_eq!(st, StatusCode::OK);
    let cols = v.as_array().unwrap();
    assert_eq!(cols.len(), 7, "python's builtin column set");
    assert_eq!(cols[0]["id"], json!("backlog"));
    assert_eq!(cols[2]["label"], json!("In Progress"));
    assert_eq!(cols[6]["id"], json!("discarded"));
}

// ---- PATCH {archived} — the SPA/CLI archive path (AMUX-2492 parity) ------

#[tokio::test]
async fn patch_archived_round_trip_with_cross_lane_guard() {
    let (app, _dir) = app();
    let v = create(&app, json!({ "title": "mine", "session": "lane-a" })).await;
    let id = v["id"].as_str().unwrap().to_string();

    // Cross-lane archive without authorized_by -> Python's 400 guard.
    let (st, _, e) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "archived": 1 })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(e["error"].as_str().unwrap().contains("authorized_by"), "{e}");
    assert_eq!(e["card_owner"], json!("lane-a"));

    // Same-lane archive: applied; the card leaves the active view.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "archived": 1 })),
        &[("X-Amux-Session", "lane-a")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["archived"], json!(1));
    let (_, _, active) = send(&app, "GET", "/api/board?archived=0", None).await;
    assert!(active.as_array().unwrap().is_empty());

    // authorized_by is control, not "ignored"; and it unlocks cross-lane.
    let v2 = create(&app, json!({ "title": "theirs", "session": "lane-a" })).await;
    let id2 = v2["id"].as_str().unwrap().to_string();
    let (st, _, v3) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id2}"),
        Some(json!({ "archived": "true", "authorized_by": "ethan" })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v3}");
    assert_eq!(v3["archived"], json!(1));
    assert!(v3
        .get("ignored_fields")
        .and_then(|f| f.as_array())
        .map(|a| !a.iter().any(|x| x == "authorized_by"))
        .unwrap_or(true));

    // UN-archive is never gated — the un-do must stay reachable, even
    // cross-lane (restoring visibility is not destruction).
    let (st, _, r) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "archived": 0 })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["archived"], json!(0));
}

// ---- Python `archived` grammar + the tab-counter fetches (AMUX-2586 #5) --
//
// The SPA's board tab counters are fed by two fetches: the main list
// (`?archived=0`) and the archived merge (`?archived=1&done_limit=0`), and
// the full-text corpus by a BARE `?done_limit=0`. Python's grammar: absent
// or "" = NO filter; "1"/"true"/"yes" (lowercased) = archived-only; any
// other value = non-archived only. This pins all three against a fixture
// mixing archived x owned states, counting exactly what the SPA counts.

#[tokio::test]
async fn archived_grammar_matches_python_and_tab_counts_pin() {
    let (app, _dir) = app();
    // Fixture: 2 live owned, 3 live unowned, 4 archived unowned, 1 archived
    // owned. "Unowned" (the SPA chip) = open cards with no session.
    for i in 0..2 {
        create(&app, json!({ "title": format!("live owned {i}"), "session": "lane-a" })).await;
    }
    for i in 0..3 {
        create(&app, json!({ "title": format!("live unowned {i}"), "session": "" })).await;
    }
    let mut archived_ids = Vec::new();
    for i in 0..4 {
        let v = create(&app, json!({ "title": format!("arch unowned {i}"), "session": "" })).await;
        archived_ids.push(v["id"].as_str().unwrap().to_string());
    }
    let v = create(&app, json!({ "title": "arch owned", "session": "lane-a" })).await;
    archived_ids.push(v["id"].as_str().unwrap().to_string());
    for id in &archived_ids {
        let (st, _, _) =
            send(&app, "POST", &format!("/api/board/{id}/archive"), Some(json!({}))).await;
        assert_eq!(st, StatusCode::OK);
    }

    let count = |v: &Value| v.as_array().unwrap().len();
    let unowned_open = |v: &Value| {
        v.as_array()
            .unwrap()
            .iter()
            .filter(|i| {
                i["session"].as_str().unwrap_or("").is_empty()
                    && i["archived"].as_i64().unwrap_or(0) == 0
            })
            .count()
    };

    // Main SPA fetch: non-archived only.
    let (_, _, active) = send(&app, "GET", "/api/board?archived=0", None).await;
    assert_eq!(count(&active), 5);
    assert_eq!(unowned_open(&active), 3, "the Unowned chip's number");

    // Archived-merge fetch: archived rows ONLY (returning everything here
    // is what inflated the merged set the counters scan).
    let (_, _, arch) = send(&app, "GET", "/api/board?archived=1&done_limit=0", None).await;
    assert_eq!(count(&arch), 5);
    assert!(arch.as_array().unwrap().iter().all(|i| i["archived"] == json!(1)));

    // Case-insensitive truthy, Python's `.lower()`.
    let (_, _, arch2) = send(&app, "GET", "/api/board?archived=TRUE", None).await;
    assert_eq!(count(&arch2), 5);

    // Bare list (param absent): NO filter — the text-search corpus.
    let (_, _, all) = send(&app, "GET", "/api/board?done_limit=0", None).await;
    assert_eq!(count(&all), 10);

    // Python has no "all" spelling: any other value means non-archived.
    let (_, _, not_truthy) = send(&app, "GET", "/api/board?archived=all", None).await;
    assert_eq!(count(&not_truthy), 5);
    let (_, _, zero) = send(&app, "GET", "/api/board?archived=false", None).await;
    assert_eq!(count(&zero), 5);

    // The two counter feeds are disjoint and together cover the board.
    assert_eq!(count(&active) + count(&arch), count(&all));
}

// ---- done_limit + the truncation header quartet (Invariant 40) -----------

#[tokio::test]
async fn done_limit_caps_terminal_and_headers_announce_it() {
    let (app, _dir) = app();
    for i in 0..3 {
        create(&app, json!({ "title": format!("done {i}"), "status": "done" })).await;
    }
    create(&app, json!({ "title": "live", "status": "todo" })).await;

    // Cap bites: 2 of 3 terminal kept, active card never capped.
    let (st, headers, list) = send(&app, "GET", "/api/board?done_limit=2", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 3); // 1 active + 2 terminal
    assert_eq!(hdr(&headers, "x-amux-done-limit"), "2");
    assert_eq!(hdr(&headers, "x-amux-truncated"), "1");
    assert_eq!(hdr(&headers, "x-amux-terminal-total"), "3");
    assert_eq!(hdr(&headers, "x-amux-terminal-returned"), "2");

    // Default limit 100: nothing withheld, and the headers SAY so.
    let (_, headers, list) = send(&app, "GET", "/api/board", None).await;
    assert_eq!(list.as_array().unwrap().len(), 4);
    assert_eq!(hdr(&headers, "x-amux-done-limit"), "100");
    assert_eq!(hdr(&headers, "x-amux-truncated"), "0");
    assert_eq!(hdr(&headers, "x-amux-terminal-total"), "3");
    assert_eq!(hdr(&headers, "x-amux-terminal-returned"), "3");

    // done_limit=0 = unlimited (Python contract: totals report 0/0).
    let (_, headers, list) = send(&app, "GET", "/api/board?done_limit=0", None).await;
    assert_eq!(list.as_array().unwrap().len(), 4);
    assert_eq!(hdr(&headers, "x-amux-truncated"), "0");

    // Status/session filters run BEFORE the cap (AC-291's lesson).
    let (_, headers, list) = send(&app, "GET", "/api/board?status=done&done_limit=2", None).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(hdr(&headers, "x-amux-terminal-total"), "3");
}

// ---- gate 409: Python-compatible body, then honest satisfaction ----------

#[tokio::test]
async fn gate_blocks_with_python_body_then_gate_checked_satisfies() {
    let (app, _dir) = app();
    // desc carries an artifact link: the global done-link gate (AMUX-3275)
    // requires one, and this test is about the ACK gate, not the link gate.
    let card = create(
        &app,
        json!({ "title": "gated", "status": "doing", "desc": "artifact: crates/amux-server/src/api/board.rs" }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    // Unacked doing->done on a code card: the exact 409 the CLI parses.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["error"], json!("gate not acknowledged"));
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["blocked"], json!(true));
    assert_eq!(
        v["gate"],
        json!(["Implemented and merged", "Tests / lint pass"])
    );
    assert_eq!(v["attempted_status"], json!("done"));
    assert_eq!(v["item"], json!(id));
    assert_eq!(v["item_type"], json!("code"));
    assert!(v["valid_types"].as_array().unwrap().contains(&json!("escalation")));
    // NB: no "status" key — a client reading the body instead of the HTTP
    // code must not misread the rejection as success (orch MO-2952).
    assert!(v.get("status").is_none());
    // Core's why-blocked answer rides along (Invariant 18): criterion,
    // missing evidence kind, serialized refusal kind.
    assert_eq!(v["kind"], json!("gate_blocked"));
    let wb = v["why_blocked"].as_array().unwrap();
    assert_eq!(wb.len(), 2);
    assert_eq!(wb[0]["criterion"], json!("Implemented and merged"));
    assert_eq!(wb[0]["missing"], json!("model_transcript"));

    // gate_checked that does NOT match every criterion is refused (AMUX-1719).
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_checked": ["Implemented and merged"] })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["error"], json!("gate_checked does not match the gate"));
    assert_eq!(v["missing"], json!(["Tests / lint pass"]));

    // The full ack passes, and the evidence lands in the card's log.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({
            "status": "done", "evidence": EV,
            "gate_checked": ["Implemented and merged", "Tests / lint pass"]
        })),
        &[("X-Amux-Session", "worker-1")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("done"));
    assert_eq!(v["applied"], json!(true));
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    let log = detail["log"].as_str().unwrap();
    assert!(log.contains("worker-1: gate satisfied via gate_checked (2/2)"), "log: {log}");
    assert!(log.contains("worker-1: doing -> done"), "log: {log}");
}

#[tokio::test]
async fn gates_derive_from_type_and_retyping_is_the_honest_exit() {
    let (app, _dir) = app();
    let card = create(
        &app,
        json!({ "title": "self-resolved page", "status": "doing", "type": "escalation", "desc": "artifact: crates/amux-server/src/api/board.rs" }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    // An escalation is NOT gated on a merge — its gate is the honest one.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(
        v["gate"],
        json!(["Outcome recorded in the item (what happened, and why it is closed)"])
    );

    // gate_ack: true satisfies it wholesale.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("done"));

    // Unknown types are rejected at the door with the valid set — never
    // silently mis-gated.
    //
    // THE EXAMPLE USED TO BE `decision`, and swapping it is the point of AF-323
    // rather than a detail. Refusing an UNLISTED type is correct and still
    // asserted here; what was wrong was `decision` being unlisted while five
    // live cards already stored it, so this test was pinning the defect. Its own
    // comment tracked the count growing — "the seven 'decision' cards incident",
    // filed at three — and a growing number in a test's prose is the tell that
    // the test is describing a leak, not a guarantee.
    //
    // `task` carries the same provenance and is still genuinely unknown: the CLI
    // advertised `task` and `decision` in its usage line until AMUX-2479, and
    // neither was accepted. One of those two is now a real type; this is the
    // other.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "type": "task" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("unknown type"));
    assert!(v["valid_types"].as_array().unwrap().contains(&json!("watch")));
    // And the type this card family is named after is now IN that set, which is
    // the half a "reject unknown types" test can never show on its own.
    assert!(
        v["valid_types"].as_array().unwrap().contains(&json!("decision")),
        "decision must be offered as a valid type: {v}"
    );
}

/// AMUX-3058: a gate OVERRIDE (`gate` field) pins the gate over the type, so
/// ethos rule 3's "fix the type" escape was a DEAD END while an override stood
/// (TUBES-1622: an override carrying code criteria on a non-code card). Retyping
/// now clears a stale override so the gate re-derives from the new type. Both
/// legs: the new type's gate becomes satisfiable, AND the gate is RE-DERIVED, not
/// bypassed — a fresh card of the new type still refuses the old criteria.
#[tokio::test]
async fn retyping_clears_a_gate_override_so_the_gate_re_derives_from_the_new_type() {
    let (app, _dir) = app();
    // An INVESTIGATION card whose gate is OVERRIDDEN to the code done-gate — the
    // override matches no type default, exactly the reported shape, so `done`
    // demanded code criteria regardless of the type.
    let card = create(
        &app,
        json!({
            "title": "override card", "session": "worker-1", "status": "doing",
            "type": "investigation", "desc": "outcome recorded (artifact: crates/amux-server/src/api/board.rs)",
            "gate": ["Implemented and merged", "Tests / lint pass"],
        }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    // Before the fix: done demanded the code override even for an investigation.
    let (st, _, v) = send(&app, "PATCH", &format!("/api/board/{id}"), Some(json!({ "status": "done", "evidence": EV }))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["gate"], json!(["Implemented and merged", "Tests / lint pass"]), "the override pins code criteria pre-retype");

    // Retype to chore: AMUX-3058 clears the stale override.
    let (st, _, _) = send(&app, "PATCH", &format!("/api/board/{id}"), Some(json!({ "type": "chore" }))).await;
    assert_eq!(st, StatusCode::OK);

    // POSITIVE leg: the chore gate is now satisfiable (override cleared, re-derived).
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_checked": ["Outcome recorded in the item (what happened, and why it is closed)"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the type-derived chore gate must be satisfiable after retype: {v}");
    assert_eq!(v["status"], json!("done"));

    // NEGATIVE leg: the gate is RE-DERIVED, not bypassed. A fresh chore card must
    // still REFUSE the old code criteria — the fix did not make every ack pass.
    let control = create(&app, json!({ "title": "control", "session": "worker-1", "status": "doing", "type": "chore", "desc": "artifact: crates/amux-server/src/api/board.rs" })).await;
    let cid = control["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{cid}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_checked": ["Implemented and merged", "Tests / lint pass"] })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "code criteria must NOT satisfy a chore gate: {v}");
    assert_eq!(v["error"], json!("gate_checked does not match the gate"));
}

// ---- force: bypass WITH audit (ethos rule 6) -----------------------------

#[tokio::test]
async fn force_bypasses_the_gate_and_leaves_the_audit_line() {
    let (app, _dir) = app();
    let card = create(&app, json!({ "title": "hotfix", "status": "doing" })).await;
    let id = card["id"].as_str().unwrap().to_string();

    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "force": true, "reason": "hotfix, evidence in PR" })),
        &[("X-Amux-Session", "tester")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("done"));

    // Read the log BACK — the force must be traceable from the card itself.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    let log = detail["log"].as_str().unwrap();
    assert!(
        log.contains("force by tester: doing->done reason=hotfix, evidence in PR"),
        "force audit line missing from log: {log}"
    );

    // A headerless force is REFUSED (Python parity: "force requires
    // attribution", amux-server.py ~70111). The refusal fires on `force`
    // itself, not on `eff_gate && force` — the ts-gke incident specimen was
    // an UNGATED transition, which a gate-conditioned check waves through.
    let card2 = create(&app, json!({ "title": "h2", "status": "doing" })).await;
    let id2 = card2["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id2}"),
        Some(json!({ "status": "done", "force": true })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], json!("force requires attribution"));
    // And the card did not move.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id2}"), None).await;
    assert_eq!(detail["status"], json!("doing"));
}

/// AMUX-3464: an ATTRIBUTED force with a BLANK reason is refused too.
///
/// The measurement that motivated this: 41 force audit lines existed on the
/// live board and 41 read `reason=` with nothing after it. Attribution was
/// enforced (41/41 named an actor) and the judgment half was never once
/// populated, so the ledger recorded that something happened and nothing about
/// why — ethos rule 6's "audited bypass" that audits nothing.
///
/// The case above cannot catch this: it is UNATTRIBUTED, so it fails on the
/// older check and would stay green if the reason requirement were deleted.
/// This one supplies a valid session precisely so the reason is the only thing
/// under test.
#[tokio::test]
async fn force_with_a_blank_reason_is_refused_even_when_properly_attributed() {
    let (app, _dir) = app();

    // Three spellings of "no judgment supplied", because the production
    // specimens arrived as the first two: `amux board --force` omitted the
    // field entirely, and raw PATCHes sent it empty.
    for body in [
        json!({ "status": "done", "force": true }),
        json!({ "status": "done", "force": true, "reason": "" }),
        json!({ "status": "done", "force": true, "reason": "   " }),
    ] {
        let card = create(&app, json!({ "title": "blank", "status": "doing" })).await;
        let id = card["id"].as_str().unwrap().to_string();
        let (st, _, v) = send_with(
            &app,
            "PATCH",
            &format!("/api/board/{id}"),
            Some(body.clone()),
            &[("X-Amux-Session", "tester")],
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "body {body} should be refused: {v}");
        assert_eq!(v["error"], json!("force requires a reason"), "{v}");
        // The 409/400 body must name the SANCTIONED command, not just the HTTP
        // shape. AMUX-2325: an error that publishes the escape purely in
        // protocol terms sends a correct, literal-minded agent off-trail into a
        // hand-rolled curl, which is where 25 of those 41 came from.
        let how = v["how"].as_str().unwrap_or_default();
        assert!(how.contains("amux board"), "refusal must name the CLI escape: {how}");

        // The card did not move — a refused force must not half-apply.
        let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
        assert_eq!(detail["status"], json!("doing"), "{detail}");
    }

    // POSITIVE CONTROL, in the same test so a build that refuses EVERY force
    // cannot pass. Without this the assertions above are satisfied by breaking
    // force outright.
    let ok = create(&app, json!({ "title": "with reason", "status": "doing" })).await;
    let ok_id = ok["id"].as_str().unwrap().to_string();
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{ok_id}"),
        Some(json!({ "status": "done", "force": true, "reason": "gate does not fit; judged by hand" })),
        &[("X-Amux-Session", "tester")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a force WITH a reason must still work: {v}");
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{ok_id}"), None).await;
    let log = detail["log"].as_str().unwrap();
    assert!(
        log.contains("reason=gate does not fit; judged by hand"),
        "the reason must reach the permanent audit line, not just the request: {log}"
    );
}

// ---- archive / restore (RR-0055) -----------------------------------------

#[tokio::test]
async fn a_scoped_list_excludes_archived_by_default_but_unscoped_still_includes_it() {
    // AMUX-3086 / AMUX-3107. A scoped query (session= or status=) with `archived`
    // absent now defaults to ActiveOnly, so an agent building a discard candidate
    // set from `?session=X&status=done` never sees an immutable archived card
    // (which would 409 on the PATCH). The UNSCOPED bare list still includes it (the
    // SPA text-search corpus relies on that). This test fails in EITHER direction:
    // if the scoped default regresses to All, or if the unscoped path is filtered.
    let (app, _dir) = app();
    let live = create(
        &app,
        json!({ "title": "live done", "status": "done", "session": "alpha", "type": "chore" }),
    )
    .await;
    let live_id = live["id"].as_str().unwrap().to_string();
    let arch = create(
        &app,
        json!({ "title": "archived done", "status": "done", "session": "alpha", "type": "chore" }),
    )
    .await;
    let arch_id = arch["id"].as_str().unwrap().to_string();
    let (st, _, _) = send_with(
        &app,
        "POST",
        &format!("/api/board/{arch_id}/archive"),
        Some(json!({ "reason": "done and parked" })),
        &[("X-Amux-Session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let ids = |list: &serde_json::Value| -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["id"].as_str().map(str::to_string))
            .collect()
    };

    // Scoped by session+status: the fix EXCLUDES the archived card.
    let (_, _, list) = send(&app, "GET", "/api/board?session=alpha&status=done", None).await;
    let got = ids(&list);
    assert!(got.contains(&live_id), "live card must be present: {list}");
    assert!(!got.contains(&arch_id), "archived card must be excluded from a scoped list: {list}");

    // Scoped by session alone: same exclusion.
    let (_, _, list) = send(&app, "GET", "/api/board?session=alpha", None).await;
    assert!(!ids(&list).contains(&arch_id), "session-scoped list must exclude archived: {list}");

    // UNSCOPED bare list still includes it (the guard the unscoped default protects).
    let (_, _, list) = send(&app, "GET", "/api/board", None).await;
    assert!(ids(&list).contains(&arch_id), "unscoped bare list must still include archived: {list}");

    // Explicit ?archived=1 on a scoped query still finds it (the override wins).
    let (_, _, list) = send(&app, "GET", "/api/board?session=alpha&archived=1", None).await;
    assert!(ids(&list).contains(&arch_id), "explicit archived=1 must still return it: {list}");
}

#[tokio::test]
async fn archive_restore_round_trip_preserves_every_field() {
    let (app, _dir) = app();
    let card = create(
        &app,
        json!({
            "title": "parked work", "status": "doing", "session": "my-project",
            "desc": "half-done", "type": "research", "tags": ["q3"]
        }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    let (st, _, v) = send_with(
        &app,
        "POST",
        &format!("/api/board/{id}/archive"),
        Some(json!({ "reason": "parking for Q4" })),
        &[("X-Amux-Session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["applied"], json!(true));
    assert_eq!(v["archived"], json!(1));
    assert_eq!(v["status"], json!("doing"), "archive is a FLAG, not a status");

    // Python's grammar: the BARE list has NO archived filter (the card is
    // still in it, flagged); `?archived=0` is the SPA's active view, which
    // excludes it; `?archived=1` finds it alone.
    let (_, _, list) = send(&app, "GET", "/api/board", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list.as_array().unwrap()[0]["archived"], json!(1));
    let (_, _, list) = send(&app, "GET", "/api/board?archived=0", None).await;
    assert!(list.as_array().unwrap().is_empty());
    let (_, _, list) = send(&app, "GET", "/api/board?archived=1", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Double-archive: honest no-op, rev unmoved (Invariant 37).
    let rev_after_archive = v["rev"].as_i64().unwrap();
    let (st, _, v2) = send(&app, "POST", &format!("/api/board/{id}/archive"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v2["applied"], json!(false));
    assert_eq!(v2["rev"].as_i64().unwrap(), rev_after_archive);

    // A status PATCH on an archived card is refused (restore it first).
    // Attributed force WITH a reason: this cell tests archived-immutability,
    // and BOTH force preconditions 400 before the 409 can be reached, so
    // either one missing masks the property under test. Attribution was the
    // first (ts-gke 2026-08-03); `reason` joined it in AMUX-3464.
    let (st, _, v3) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "force": true, "reason": "testing immutability" })),
        &[("X-Amux-Session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(v3["error"].as_str().unwrap().contains("archived"));

    // Restore: back exactly where it was, every field intact.
    let (st, _, r) = send(&app, "POST", &format!("/api/board/{id}/restore"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(r["applied"], json!(true));
    assert_eq!(r["archived"], json!(0));
    assert_eq!(r["status"], json!("doing"));
    assert_eq!(r["title"], json!("parked work"));
    assert_eq!(r["desc"], json!("half-done"));
    assert_eq!(r["session"], json!("my-project"));
    assert_eq!(r["type"], json!("research"));
    assert_eq!(r["tags"], json!(["q3"]));
    let log = r["log"].as_str().unwrap();
    assert!(log.contains("orch: archived — parking for Q4"), "log: {log}");
    assert!(log.contains("restored"), "log: {log}");
}

// ---- circular depends_on -------------------------------------------------

#[tokio::test]
async fn circular_depends_on_is_rejected_with_the_cycle_path() {
    let (app, _dir) = app();
    let a = create(&app, json!({ "title": "A", "session": "g" })).await;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b = create(&app, json!({ "title": "B", "session": "g", "depends_on": [a_id.clone()] })).await;
    let b_id = b["id"].as_str().unwrap().to_string();
    assert_eq!(b["depends_on"], json!([a_id.clone()]));

    // Closing the loop is a 400 naming the cycle.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{a_id}"),
        Some(json!({ "depends_on": [b_id.clone()] })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("circular depends_on"));
    let cycle = v["cycle"].as_array().unwrap();
    assert!(cycle.contains(&json!(a_id.clone())) && cycle.contains(&json!(b_id)));

    // A self-dependency at create is the same refusal.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{a_id}"),
        Some(json!({ "depends_on": [a_id.clone()] })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

    // And nothing was written by the refusals.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{a_id}"), None).await;
    assert_eq!(detail["depends_on"], json!([]));
}

// ---- no-op PATCH: applied:false, rev unmoved (Invariant 37) --------------

#[tokio::test]
async fn a_card_is_assigned_to_an_epic_and_can_be_cleared() {
    // AMUX-2992: epic = a type=epic card, children link up via the epic field.
    let (app, _dir) = app();
    let epic = create(&app, json!({ "title": "TubeScience search reliability", "type": "epic" })).await;
    let epic_id = epic["id"].as_str().unwrap().to_string();
    assert_eq!(epic["type"], json!("epic"), "epic is a real card type now");

    let child = create(&app, json!({ "title": "fix unsearchable docs" })).await;
    let child_id = child["id"].as_str().unwrap().to_string();

    // Assign the child to the epic. `epic` must be a WRITABLE field, not ignored.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{child_id}"),
        Some(json!({ "epic": epic_id })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["applied"], json!(true));
    let ignored = v["ignored_fields"].as_array().cloned().unwrap_or_default();
    assert!(!ignored.contains(&json!("epic")), "epic must be writable, not ignored: {v}");

    // It rolls up: the child's detail carries the epic id.
    let (st, _, detail) = send(&app, "GET", &format!("/api/board/{child_id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["epic"], json!(epic_id), "child must report its epic: {detail}");

    // Clearing it (empty string) removes the link.
    let (st, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{child_id}"),
        Some(json!({ "epic": "" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{child_id}"), None).await;
    assert!(detail["epic"].is_null(), "cleared epic must read null: {detail}");
}

#[tokio::test]
async fn noop_patch_reports_applied_false_and_moves_nothing() {
    let (app, _dir) = app();
    let card = create(&app, json!({ "title": "steady", "desc": "d" })).await;
    let id = card["id"].as_str().unwrap().to_string();
    let rev0 = card["rev"].as_i64().unwrap();

    // Same values -> nothing changed.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "title": "steady", "desc": "d" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["applied"], json!(false));
    assert_eq!(v["rev"].as_i64().unwrap(), rev0);

    // Unknown keys are NAMED, never silently dropped (AC-263). `archived`
    // is no longer among them — it is a writable field since the AMUX-2492
    // parity port; `archived: 0` on an active card is an honest no-op.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "archived": 0, "bogus_key": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["applied"], json!(false));
    let ignored = v["ignored_fields"].as_array().unwrap();
    assert!(ignored.contains(&json!("bogus_key")) && !ignored.contains(&json!("archived")));

    // Same-status PATCH is also a no-op, not a phantom transition.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "todo" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["applied"], json!(false));

    // Read back: rev truly unmoved, no log lines invented.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(detail["rev"].as_i64().unwrap(), rev0);
    assert_eq!(detail["log"], Value::Null);
}

// ---- optimistic concurrency ----------------------------------------------

#[tokio::test]
async fn stale_expect_rev_is_409_with_current_rev() {
    let (app, _dir) = app();
    let card = create(&app, json!({ "title": "contested" })).await;
    let id = card["id"].as_str().unwrap().to_string();

    // Move rev forward once.
    let (st, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc": "writer A", "expect_rev": 0 })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // A stale writer against rev 0 gets the conflict WITH the current state.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc": "writer B", "expect_rev": 0 })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["error"], json!("rev conflict"));
    assert_eq!(v["current_rev"], json!(1));
    assert_eq!(v["item"]["desc"], json!("writer A"));

    // Nothing was clobbered.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(detail["desc"], json!("writer A"));
}

// ---- global done-evidence gate (AF-321): done needs proof, not a plan -----

/// THE PROPERTY THIS GATE EXISTS FOR.
///
/// `done_requires_asset_link` is a shape check over the whole desc, so a card
/// that merely NAMES the file it intends to edit satisfies it before anyone
/// touches that file. Measured on the live board 2026-08-29: 843 of 1372 open
/// cards (61%) passed it on their filed text with no work done.
///
/// So the test is not "does done refuse an empty card" — the older gate already
/// did that. It is that a realistic, fully-linked PROBLEM STATEMENT, the exact
/// text a card is filed with, still cannot close it.
#[tokio::test]
async fn done_requires_evidence_a_plan_cannot_supply() {
    let (app, _dir) = app();
    // A real filing: names the source file and the source doc. Both gates that
    // read `desc` see artifact links here.
    let card = create(
        &app,
        json!({
            "title": "add the check",
            "status": "doing",
            "type": "chore",
            "desc": "Location: crates/amux-server/src/api/board.rs — add the check there. \
                     Source: docs/fleet-friction-review-2026-08-29.md Theme 4",
        }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    // The OLD gate is satisfied by this text. Assert that directly, so the test
    // below cannot pass for the wrong reason (a control that can distinguish
    // "the new gate fired" from "the old gate fired").
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(
        v["code"],
        json!("done_requires_evidence"),
        "the plan must get PAST the link gate and be stopped by the evidence gate: {v}"
    );

    // Prose is not evidence either.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "gate_ack": true, "evidence": "implemented" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("done_evidence_has_no_artifact"), "{v}");
    // The refused text is handed back, so a caller can see what it sent.
    assert_eq!(v["recorded_evidence"], json!("implemented"));

    // Something a reader can re-run closes it.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({
            "status": "done",
            "gate_ack": true,
            "evidence": "ran `cargo test -p amux-server --test board_api`, 51 passed",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("done"));
    // And it is READABLE afterwards: a gate whose input is not stored is a
    // gate nobody can audit (ethos rule 6).
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert!(
        detail["evidence"].as_str().unwrap_or("").contains("cargo test"),
        "evidence must survive onto the card: {detail}"
    );
}

// ---- todo WIP limit + blocked needs a watch (AF-317) ----------------------

/// A lane's `todo` is a dispatch queue with a ceiling, and the refusal names
/// the cards to close FIRST rather than saying "close something".
#[tokio::test]
async fn the_todo_wip_limit_refuses_the_next_card_and_names_what_to_close() {
    let (app, _tmp) = app();
    std::env::set_var("AMUX_TODO_WIP_LIMIT", "3");

    let mk = |app: &axum::Router, n: usize| {
        let app = app.clone();
        async move {
            create(
                &app,
                json!({"title": format!("queued work {n}"), "session": "wippy",
                       "owner_type": "agent", "status": "todo"}),
            )
            .await
        }
    };
    for n in 0..3 {
        let c = mk(&app, n).await;
        assert!(c["id"].as_str().is_some(), "card {n} must be creatable: {c}");
    }

    // The FOURTH is refused, on the CREATE path — which is the path `amux board
    // add` uses and therefore the one that actually grew the queues.
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board",
        Some(json!({"title": "one too many", "session": "wippy",
                    "owner_type": "agent", "status": "todo"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("todo_wip_limit_reached"), "{v}");
    assert_eq!(v["holding"], json!(3), "{v}");
    assert_eq!(v["limit"], json!(3), "{v}");
    // Not a generic "close something": the refusal carries specific cards.
    let close = v["close_these_first"].as_array().expect("must name cards to close");
    assert!(!close.is_empty(), "a limit with no list is an obstacle, not a fix: {v}");
    assert!(close[0]["id"].as_str().is_some() && close[0]["days_since_touched"].is_number(), "{v}");

    // `backlog` is the sanctioned exit and is NOT capped — the limit is a
    // ceiling on QUEUEING, not on filing (ethos rule 3 wants a truthful path).
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board",
        Some(json!({"title": "real but not next", "session": "wippy",
                    "owner_type": "agent", "status": "backlog"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "backlog must stay unbounded: {v}");

    // A DETECTOR's card is never refused for queue depth: it carries no
    // session, so it is outside the count by construction rather than by an
    // exemption someone has to remember.
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board",
        Some(json!({"title": "disk is at 97%", "owner_type": "human", "status": "todo"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "a fault report must never be dropped for WIP: {v}");

    std::env::remove_var("AMUX_TODO_WIP_LIMIT");
}

/// `blocked` has to name what would unblock it, or nobody is watching.
#[tokio::test]
async fn blocked_refuses_a_card_that_names_no_watch() {
    let (app, _tmp) = app();
    let c = create(&app, json!({"title": "waiting on something", "session": "w2"})).await;
    let id = c["id"].as_str().unwrap().to_string();

    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"status": "blocked", "gate_ack": true})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("blocked_needs_a_watch"), "{v}");
    // All three honest exits are offered, including the AF-318 one.
    let fix = &v["how_to_fix"];
    assert!(fix["on_another_card"].as_str().unwrap().contains("depends_on"), "{v}");
    assert!(fix["on_a_condition"].as_str().unwrap().contains("--trigger"), "{v}");
    assert!(fix["on_a_person"].as_str().unwrap().contains("needsyou"), "{v}");

    // A dependency satisfies it.
    let other = create(&app, json!({"title": "the thing that must land first"})).await;
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"status": "blocked", "gate_ack": true,
                    "depends_on": [other["id"].as_str().unwrap()]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a named dependency is a watch: {v}");
}

// ---- typed needsyou ask (AF-318): a park has to name the human act ---------

/// THE PROPERTY. A card can be parked on a human only if it says which KIND of
/// human act, what is being asked, and what ends the block.
///
/// The specimen is not an empty card — the old gates already caught those. It
/// is a card whose title and desc are perfectly ordinary engineering work, the
/// shape roughly half the live `needsyou` cards actually have ("Compute
/// Utilization Audit", "Fix Namespace Pollution"). Those pass every other gate
/// on the board and are exactly what makes the ~20 real asks unfindable.
///
/// The population is 389 live, not the 445 the filing card quoted: that query
/// counted archived rows. The 51% share is unaffected, and the shape is what
/// this test pins.
#[tokio::test]
async fn needsyou_refuses_a_park_that_names_no_human_act() {
    let (app, _tmp) = app();
    let c = create(
        &app,
        json!({
            "title": "Compute Utilization Audit",
            "desc": "Audit idle GPU hours across the fleet. See docs/compute.md.",
        }),
    )
    .await;
    let id = c["id"].as_str().unwrap().to_string();

    // Untyped: refused, and the five types are IN the refusal so the reader
    // does not have to go and find them (AF-112).
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "needsyou", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("needsyou_requires_ask_type"), "{v}");
    let types: Vec<&str> =
        v["ask_types"].as_array().unwrap().iter().map(|t| t["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["decision", "access", "credential", "external", "judgment"], "{v}");

    // A type outside the closed vocabulary is refused too, or the vocabulary
    // is decorative.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "needsyou", "gate_ack": true, "ask_actor": "Ethan", "ask_type": "blocked",
                     "ask_question": "can someone look at this",
                     "ask_unblocks": "someone looks at it" })),
    )
    .await;
    assert_eq!(v["code"], json!("needsyou_ask_type_unknown"), "{v}");
    assert_eq!(st, StatusCode::CONFLICT);

    // "Blocked on Ethan" with no question: the exact phrasing the rules call
    // out as not-an-ask.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "needsyou", "gate_ack": true, "ask_actor": "Ethan", "ask_type": "decision",
                     "ask_question": "blocked", "ask_unblocks": "Ethan answers this question" })),
    )
    .await;
    assert_eq!(v["code"], json!("needsyou_ask_has_no_question"), "{v}");
    assert_eq!(st, StatusCode::CONFLICT);

    // A question with no exit condition. This is the half that makes an ask
    // falsifiable, so it is refused separately rather than folded in.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "needsyou", "gate_ack": true, "ask_actor": "Ethan", "ask_type": "decision",
                     "ask_question": "Should idle GPU hours be reclaimed automatically?",
                     "ask_unblocks": "yes" })),
    )
    .await;
    assert_eq!(v["code"], json!("needsyou_ask_has_no_exit"), "{v}");
    assert_eq!(st, StatusCode::CONFLICT);

    // All four, and it parks.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "needsyou", "gate_ack": true, "ask_actor": "Ethan", "ask_type": "decision",
                     "ask_question": "Should idle GPU hours be reclaimed automatically?",
                     "ask_unblocks": "a yes or no from the owner on auto-reclaim" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // And the ask is READABLE afterwards — a gate whose input is not stored is
    // a gate nobody can audit (ethos rule 6), and the owner view ranks on it.
    let (_, _, got) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(got["ask_type"], json!("decision"));
    assert!(got["ask_unblocks"].as_str().unwrap().contains("auto-reclaim"), "{got}");
}

/// The ask survives a transition that a DIFFERENT gate refuses.
///
/// Same two-write property as `evidence`: a PATCH is atomic, so sending the ask
/// together with the status means a 409 rolls the ask back and the retry has
/// nothing to satisfy the gate that just refused it.
#[tokio::test]
async fn a_refused_park_does_not_discard_the_ask_it_asked_for() {
    let (app, _tmp) = app();
    let c = create(&app, json!({ "title": "park me", "type": "code" })).await;
    let id = c["id"].as_str().unwrap().to_string();

    // Written on its own, with no status change at all.
    let (st, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "ask_actor": "platform-admin", "ask_type": "access", "ask_question": "Who owns the staging console?",
                     "ask_unblocks": "a name, or an invite to the console" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (_, _, got) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(got["ask_type"], json!("access"), "the ask must persist without a transition");
    assert_eq!(got["status"], json!("todo"), "writing an ask must not move the card");
}

/// THE OWNER VIEW: capped, ranked by what clearing it releases, and honest
/// about the remainder.
#[tokio::test]
async fn the_needsyou_view_is_capped_and_ranks_by_blast_radius_not_age_alone() {
    let (app, _tmp) = app();
    let park = |app: &axum::Router, id: String| {
        let app = app.clone();
        async move {
            send(
                &app,
                "PATCH",
                &format!("/api/board/{id}"),
                Some(json!({ "status": "needsyou", "gate_ack": true, "ask_actor": "Ethan", "ask_type": "decision",
                             "ask_question": "Which way should this go?",
                             "ask_unblocks": "a direction from the owner" })),
            )
            .await
        }
    };

    // Two parked cards. `blocker` has an open card depending on it; `lonely`
    // has none. Both are the same age, so age alone cannot separate them and
    // any ordering difference is the blast radius doing the work.
    let blocker = create(&app, json!({"title": "three lanes wait on this"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let lonely = create(&app, json!({"title": "nobody waits on this"})).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    create(&app, json!({"title": "waiting behind the blocker", "depends_on": [blocker.clone()]}))
        .await;
    assert_eq!(park(&app, blocker.clone()).await.0, StatusCode::OK);
    assert_eq!(park(&app, lonely.clone()).await.0, StatusCode::OK);

    let (st, _, v) = send(&app, "GET", "/api/board/needsyou", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let ids: Vec<&str> =
        v["queue"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids.first(),
        Some(&blocker.as_str()),
        "the card others are blocked behind must outrank the one nobody waits on, at equal \
         age — otherwise this is the same list sorted by age: {v}"
    );
    assert_eq!(v["queue"][0]["blast_radius"], json!(1), "{v}");
    assert_eq!(v["queue"][1]["blast_radius"], json!(0), "{v}");

    // AF-320 contract, since this is a diagnostic-shaped read.
    assert_eq!(v["measured"], json!(true), "{v}");
    assert_eq!(v["n_considered"], json!(2), "{v}");

    // THE CAP, and the honesty about what it hides. A view that hid rows with
    // no way to ask for them would be an instrument lying about its own
    // population.
    let (_, _, capped) = send(&app, "GET", "/api/board/needsyou?limit=1", None).await;
    assert_eq!(capped["shown"], json!(1), "{capped}");
    assert_eq!(capped["hidden"], json!(1), "{capped}");
    assert_eq!(capped["total"], json!(2), "{capped}");
    let (_, _, all) = send(&app, "GET", "/api/board/needsyou?limit=1&all=1", None).await;
    assert_eq!(all["shown"], json!(2), "?all=1 must reach the remainder: {all}");
    assert_eq!(all["hidden"], json!(0), "{all}");
}

/// The no-artifact escape is real, and it is not a shrug. Both halves matter:
/// without the first, an escalation that closed because the owner decided has
/// no truthful path to done (ethos rule 3); without the second, `none:` is a
/// blanket bypass and the gate means nothing.
#[tokio::test]
async fn none_needs_a_reason_and_then_it_is_accepted_and_stored() {
    let (app, _dir) = app();
    let card = create(
        &app,
        json!({ "title": "stand down", "status": "doing", "type": "escalation",
                "desc": "raised in docs/x.md" }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "gate_ack": true, "evidence": "none: n/a" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("done_evidence_none_unexplained"), "{v}");

    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({
            "status": "done", "gate_ack": true,
            "evidence": "none: owner decided to stand this down, nothing was built",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // Stored verbatim, so `evidence LIKE 'none:%'` counts the escapes. An escape
    // nobody can count is the blind spot the escape was meant not to be.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert!(detail["evidence"].as_str().unwrap_or("").starts_with("none: owner decided"));
}

// ---- global done-link gate (AMUX-3275): done needs a real asset link ------

/// The done-link gate is validated against the card text, so a full gate_ack
/// cannot fake it (ethos rule 7); adding a real link then lets the same ack
/// through. This is the integration coverage that was missing when the gate
/// shipped: the unit tests exercised the detector, but no board test moved a
/// card to done, so the fleet CI is what first proved the gate fires end to end.
#[tokio::test]
async fn done_requires_an_asset_link_and_gate_ack_cannot_fake_it() {
    let (app, _dir) = app();
    // No link anywhere: prose desc, and evidence that is prose too. Both of the
    // places this gate reads are empty of anything checkable, so it refuses.
    // `PROSE_EV` keeps the first refusal about completely unstructured prose.
    const PROSE_EV: &str = "implemented it and it works";
    let card = create(
        &app,
        json!({ "title": "linkless", "status": "doing", "type": "chore", "desc": "just prose, nothing produced" }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": PROSE_EV, "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("done_requires_asset_link"));

    // A reproducible command satisfies the evidence gate, but it is not a
    // produced asset. Keeping these predicates separate prevents `cargo test`
    // from being presented as the file/URL/commit the card created.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "a command is evidence, not an asset: {v}");
    assert_eq!(v["code"], json!("done_requires_asset_link"));

    // The structured artifact registry is canonical and sufficient by itself;
    // workers must not have to duplicate a successful artifact write into card
    // prose. The single-card response also publishes the ref for a clickable UI.
    let (st, _, artifact) = send(
        &app,
        "POST",
        &format!("/api/board/{id}/artifacts"),
        Some(json!({
            "kind": "doc",
            "ref": "summary.md",
            "state": "created",
            "description": "run summary"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "artifact registration failed: {artifact}");
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a registered artifact must satisfy the asset gate: {v}");
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(detail["artifacts"][0]["ref"], json!("summary.md"), "{detail}");

    // Evidence that actually names an artifact remains a direct path, with the
    // description still containing only prose.
    let linked = create(
        &app,
        json!({ "title": "linked evidence", "status": "doing", "type": "chore", "desc": "prose only" }),
    )
    .await;
    let linked_id = linked["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{linked_id}"),
        Some(json!({
            "status": "done",
            "evidence": "created video-moderation-launch.mp4",
            "gate_ack": true
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a bare produced filename is a real asset ref: {v}");

    // THE DOCUMENTED ESCAPE ACTUALLY WORKS NOW. CLAUDE.md says `none: <reason>`
    // "is stored and counted, not a bypass"; this gate used to refuse it, so an
    // honest close of a card that genuinely produced nothing had to go through
    // `--force` and was logged as an override. Two such closes are on record.
    let noart = create(
        &app,
        json!({ "title": "self-cleared", "status": "doing", "type": "chore", "desc": "a report that resolved itself" }),
    )
    .await;
    let nid = noart["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{nid}"),
        Some(json!({
            "status": "done",
            "evidence": "none: the rate limit reset on its own and no artifact was produced",
            "gate_ack": true
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "`none: <reason>` must close without --force: {v}");

    // CONTROL: a bare `none:` is still refused. The escape is a reason, not a
    // shrug, and an unexplained one is exactly what it exists to prevent — so
    // the arm above must not have opened a hole.
    let shrug = create(
        &app,
        json!({ "title": "shrug", "status": "doing", "type": "chore", "desc": "prose only" }),
    )
    .await;
    let sid = shrug["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{sid}"),
        Some(json!({ "status": "done", "evidence": "none:", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "an unexplained `none:` must not pass: {v}");

    // And the original path still works: an artifact link in the DESC, which is
    // what this gate has always read.
    let (st, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{sid}"),
        Some(json!({ "desc": "shipped in crates/amux-server/src/api/board.rs" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{sid}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("done"));
}

/// AMUX-3929, reported and measured by mixpeek-general. The typed-ask gate held
/// on the TRANSITION door and creation walked past it.
///
/// `POST /api/board {"status":"needsyou"}` returned 201 with ask_type NULL while
/// `PATCH {"status":"needsyou"}` on the same card was refused. Creation is the
/// door most of this traffic uses, because "I found something the human must
/// decide" is naturally expressed by filing the card already parked.
///
/// Live measurement at the time: 491 in needsyou, 68 typed, 423 untyped (86%),
/// untyped median age 16 days, oldest 59 — and still leaking, 13 untyped created
/// in 24h against 15 typed, across every lane.
///
/// The typed control is the half that matters. A gate that refused every
/// needsyou creation would push lanes to file in `todo` and transition, which is
/// the same hole one step later; the point is that a REAL ask must sail through.
#[tokio::test]
async fn creating_a_needsyou_card_needs_a_typed_ask_just_like_the_transition_does() {
    let (app, _dir) = app();

    // The hole: parked at birth, no ask.
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board",
        Some(json!({ "title": "parked at birth", "status": "needsyou", "type": "chore" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "creation must be gated like the transition: {v}");
    assert_eq!(v["code"], json!("needsyou_requires_ask_type"));

    // CONTROL: a REAL ask is created, not refused. Without this the fix would be
    // indistinguishable from banning needsyou creation outright, which just moves
    // the untyped cards to a two-step path.
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board",
        Some(json!({
            "title": "a real ask",
            "status": "needsyou",
            "type": "chore",
            "ask_actor": "Ethan",
            "ask_type": "decision",
            "ask_question": "Should we raise the browser profile TTL above 30 days?",
            "ask_unblocks": "A yes or no from Ethan; either answer closes this.",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "a typed ask must be creatable: {v}");
    assert_eq!(v["status"], json!("needsyou"));
    assert_eq!(v["ask_type"], json!("decision"));

    // CONTROL: the vocabulary stays closed at this door too, with the same code
    // the transition door uses — one contract, whichever door you arrive at.
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board",
        Some(json!({
            "title": "invented type",
            "status": "needsyou",
            "type": "chore",
            "ask_actor": "Ethan",
            "ask_type": "vibes",
            "ask_question": "Is this ok?",
            "ask_unblocks": "Someone says yes.",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], json!("needsyou_ask_type_unknown"));

    // CONTROL: an ordinary card is untouched. The gate must not tax todo.
    let (st, _, _) = send(
        &app,
        "POST",
        "/api/board",
        Some(json!({ "title": "ordinary", "type": "chore" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "a normal create must not be gated");
}

// ---- full lifecycle through the named transitions ------------------------

#[tokio::test]
async fn lifecycle_todo_doing_review_done_verified_via_state_machine() {
    let (app, _dir) = app();
    // desc carries an artifact link for the global done-link gate (AMUX-3275),
    // and the body below carries evidence for the AF-321 one; this test is about
    // the lifecycle state machine, not about either global constraint.
    let card = create(
        &app,
        json!({ "title": "full run", "type": "chore", "desc": "artifact: crates/amux-server/src/api/board.rs" }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    // Chore gates are the honest non-code bar; ack each hop.
    for (target, expect) in [
        ("doing", "doing"),
        ("review", "review"),
        ("done", "done"),
        ("verified", "verified"),
    ] {
        let (st, _, v) = send_with(
            &app,
            "PATCH",
            &format!("/api/board/{id}"),
            Some(json!({ "status": target, "gate_ack": true, "evidence": EV })),
            &[("X-Amux-Session", "runner")],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "-> {target}: {v}");
        assert_eq!(v["status"], json!(expect));
    }
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    let log = detail["log"].as_str().unwrap();
    for line in [
        "runner: todo -> doing",
        "runner: doing -> review",
        "runner: review -> done",
        "runner: done -> verified",
    ] {
        assert!(log.contains(line), "missing {line:?} in log: {log}");
    }
    // Verified work cannot be discarded — the state machine speaks.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "discarded" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(v["error"].as_str().unwrap().contains("archive it instead"), "{v}");
}

// ---- PYTHON INTEROP: a live-shaped row survives the Rust API -------------

#[tokio::test]
async fn python_shaped_row_round_trips_without_corruption() {
    let (app, dir) = app();
    let db_path = dir.path().join("amux-test.db");

    // Hand-INSERT a row exactly as the live Python server writes them:
    // int unix timestamps, the `needsyou` spelling, `` `HH:MM` `` log lines,
    // JSON-array depends_on TEXT, no `version` named (0002's default).
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO issues (id, title, \"desc\", status, session, creator, created, \
                 updated, owner_type, pos, notified, type, archived, depends_on, log, rev, pinned) \
             VALUES ('ORCH-42', 'Live python card', 'body from python\nsecond line', 'needsyou', \
                 'orch', 'orch', 1754000000, 1754000600, 'agent', -2048.0, 1, 'escalation', 0, \
                 '[\"ORCH-1\"]', '`09:14` created by orch\n`09:20` STATUS (orch): waiting on Ethan', \
                 7, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO issue_tags (issue_id, tag, added_at) \
             VALUES ('ORCH-42', 'mixpeek', 1754000000)",
            [],
        )
        .unwrap();
    }

    // GET: raw Python vocabulary preserved on the wire.
    let (st, _, detail) = send(&app, "GET", "/api/board/ORCH-42", None).await;
    assert_eq!(st, StatusCode::OK, "{detail}");
    assert_eq!(detail["status"], json!("needsyou"), "spelling preserved, not rewritten");
    assert_eq!(detail["depends_on"], json!(["ORCH-1"]), "JSON TEXT decoded to a list");
    assert_eq!(detail["created"], json!(1754000000));
    assert_eq!(detail["rev"], json!(7));
    assert_eq!(detail["tags"], json!(["mixpeek"]));
    assert!(detail["log"].as_str().unwrap().contains("`09:20` STATUS (orch)"));

    // It appears in the list, and a needs_you filter (core spelling) finds
    // the needsyou row — both vocabularies resolve.
    let (_, _, list) = send(&app, "GET", "/api/board?status=needs_you", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // A field-only PATCH must not rewrite the status spelling.
    let (st, _, v) = send(
        &app,
        "PATCH",
        "/api/board/ORCH-42",
        Some(json!({ "title": "Live python card (triaged)" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("needsyou"));

    // Status transition needsyou -> doing (core: Resume) with the type-
    // derived escalation gate acked, attributed via header.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        "/api/board/ORCH-42",
        Some(json!({ "status": "doing", "gate_ack": true })),
        &[("X-Amux-Session", "orch")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("doing"));

    // Now read the raw columns back the way the PYTHON server will: every
    // column it depends on must still be exactly the shape it writes.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let (status, created, updated, desc, session, creator, dep, log, rev, owner_type): (
        String,
        i64,
        i64,
        String,
        String,
        String,
        String,
        String,
        i64,
        String,
    ) = conn
        .query_row(
            "SELECT status, created, updated, \"desc\", session, creator, depends_on, log, \
                 COALESCE(rev,0), owner_type FROM issues WHERE id = 'ORCH-42'",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(status, "doing");
    assert_eq!(created, 1754000000, "created is Python's, untouched");
    assert!(updated > 1754000600, "updated bumped, still unix seconds");
    assert_eq!(desc, "body from python\nsecond line", "desc untouched");
    assert_eq!(session, "orch");
    assert_eq!(creator, "orch", "creator column never written by PATCH");
    assert_eq!(dep, "[\"ORCH-1\"]", "depends_on TEXT still exact JSON");
    assert_eq!(owner_type, "agent");
    assert_eq!(rev, 9, "two applied PATCHes bumped Python's counter twice");
    // The Python log lines are intact and ours were APPENDED after them in
    // the same `HH:MM` format.
    assert!(log.starts_with("`09:14` created by orch\n`09:20` STATUS (orch): waiting on Ethan\n"));
    assert!(log.contains("orch: needsyou -> doing"), "log: {log}");

    // Timestamp column types stayed INTEGER (a Python `int(time.time())`
    // consumer would silently break on an RFC3339 string).
    let t: String = conn
        .query_row(
            "SELECT typeof(updated) FROM issues WHERE id = 'ORCH-42'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(t, "integer");
}

// ---- auth: the board sits inside the protected router --------------------

#[tokio::test]
async fn board_routes_sit_behind_auth_when_token_configured() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("amux-test.db")).unwrap();
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: Some("sekrit".into()),
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = router(state);
    let (st, _, _) = send(&app, "GET", "/api/board", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _, _) = send_with(
        &app,
        "GET",
        "/api/board",
        None,
        &[("authorization", "Bearer sekrit")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

// ---- one-doing-per-session (AMUX-1707 parity) ----------------------------

#[tokio::test]
async fn second_doing_for_same_session_is_refused_with_named_escape() {
    let (app, _dir) = app();
    let first = create(
        &app,
        json!({ "title": "in flight", "status": "doing", "session": "lane-a" }),
    )
    .await;
    let second = create(
        &app,
        json!({ "title": "queued", "status": "todo", "session": "lane-a" }),
    )
    .await;
    let id2 = second["id"].as_str().unwrap().to_string();

    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id2}"),
        Some(json!({ "status": "doing" })),
        &[("X-Amux-Session", "lane-a")],
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], json!("already holding doing"));
    assert_eq!(v["holding"][0], first["id"]);
    // The escape must name the attributed CLI command (AMUX-2325).
    assert!(v["cli"].as_str().unwrap().contains("--override-doing"));

    // The named escape works, and a different session is never capped.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id2}"),
        Some(json!({ "status": "doing", "override_doing": true, "gate_ack": true })),
        &[("X-Amux-Session", "lane-a")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // Dormant types hold no WIP: a watch card in doing must not block.
    let third = create(
        &app,
        json!({ "title": "third", "status": "todo", "session": "lane-b" }),
    )
    .await;
    let id3 = third["id"].as_str().unwrap().to_string();
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id3}"),
        Some(json!({ "status": "doing", "gate_ack": true })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

// ---- board status (column) mutations (the live 405, 2026-08-09) ----------

#[tokio::test]
async fn status_column_crud_matches_python() {
    let (app, _dir) = app();

    // PATCH on a builtin (the exact 405 repro: rename the review column).
    let (st, _, v) = send(
        &app,
        "PATCH",
        "/api/board/statuses/review",
        Some(json!({ "label": "In Review!" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], json!(true));

    // POST create -> slugified id, 201.
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board/statuses",
        Some(json!({ "label": "Waiting On Vendor" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["id"], json!("waiting-on-vendor"));

    // Reorder accepts the id.
    let (st, _, v) = send(
        &app,
        "PUT",
        "/api/board/statuses/reorder",
        Some(json!({ "order": ["waiting-on-vendor", "review"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    // Builtin delete refused; custom delete moves cards to todo WITH an
    // audit line on each card (AMUX-2491).
    let (st, _, _) = send(&app, "DELETE", "/api/board/statuses/done", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // Hand-INSERT a row in the custom status (the python-interop idiom).
    // NOTE: the API can create a card here directly now (AMUX-2609 lifted the
    // typed-vocabulary refusal — see `cards_move_into_and_out_of_user_created
    // _columns`). The hand-INSERT is KEPT deliberately: it is the python-shaped
    // row this test exists to prove we do not corrupt, and it still covers the
    // case the API cannot reach — a row already sitting in a column that was
    // deleted out from under it.
    let cid = "PY-777".to_string();
    {
        let conn = rusqlite::Connection::open(_dir.path().join("amux-test.db")).unwrap();
        conn.execute(
            "INSERT INTO issues (id, title, status, created, updated) \
             VALUES ('PY-777', 'stranded', 'waiting-on-vendor', 1786300000, 1786300000)",
            [],
        )
        .unwrap();
    }
    let (st, _, v) = send(&app, "DELETE", "/api/board/statuses/waiting-on-vendor", None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["moved"], json!(1));
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{cid}"), None).await;
    assert_eq!(detail["status"], json!("todo"));
    assert!(detail["log"]
        .as_str()
        .unwrap()
        .contains("column 'waiting-on-vendor' deleted by"));
}

/// AMUX-2609 — cards must be able to enter AND leave user-created columns.
///
/// Python's columns are fully dynamic; the rust origin refused any status
/// outside the closed `TaskStatus` enum, so a drag into a custom column 400'd.
/// The SPA had already moved the card optimistically and cached it, so the user
/// saw it sit in the new column behind a bare "Error: 400" toast until the next
/// poll snapped it back — a refusal nobody could read.
///
/// The exit direction is asserted deliberately: allowing entry without exit
/// would build a roach motel, which is the shape ethos rule 3 forbids (every
/// legitimate state needs a truthful exit). Before this landed, moving OUT
/// answered 409 telling the caller to "fix it via the Python board first" — a
/// server that had already been retired.
#[tokio::test]
async fn cards_move_into_and_out_of_user_created_columns() {
    let (app, _dir) = app();

    let (st, _, v) = send(
        &app,
        "POST",
        "/api/board/statuses",
        Some(json!({ "label": "QA Review" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["id"], json!("qa-review"));

    // 1. CREATE directly into a user column.
    let born = create(&app, json!({ "title": "born in qa", "status": "qa-review" })).await;
    assert_eq!(born["status"], json!("qa-review"), "{born}");

    // 2. MOVE IN — the drag that used to 400.
    let card = create(&app, json!({ "title": "drag me", "type": "chore" })).await;
    let id = card["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "qa-review" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "move into custom column refused: {v}");
    assert_eq!(v["status"], json!("qa-review"), "{v}");
    // Not laundered through the force path: a routine move must not write a
    // bypass line into the one audit trail that is supposed to mean something.
    let log = v["log"].as_str().unwrap_or("");
    assert!(log.contains("todo -> qa-review"), "no honest audit line: {log}");
    assert!(!log.contains("force by"), "routine move logged as a force: {log}");

    // 3. MOVE OUT — the roach-motel guard.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "todo" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "no exit from a custom column: {v}");
    assert_eq!(v["status"], json!("todo"), "{v}");

    // 4. A REAL typo still refuses — and names BOTH vocabularies, so the
    //    answer is actionable rather than a list the caller already read.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "qa-reviewww" })),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["configured_columns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "qa-review"),
        "the refusal must list the columns that DO exist: {v}"
    );
    assert!(v["valid_statuses"].as_array().unwrap().iter().any(|c| c == "todo"), "{v}");
}

/// A user column carrying a gate must enforce it. `statuses.gate` is written by
/// the column editor and was read by nothing, so a custom column would
/// otherwise have been a gate-shaped hole in the board — a move that looks
/// governed and is not.
#[tokio::test]
async fn a_user_column_with_a_gate_enforces_it() {
    let (app, _dir) = app();
    send(
        &app,
        "POST",
        "/api/board/statuses",
        Some(json!({ "label": "Security Review" })),
    )
    .await;
    let (st, _, _) = send(
        &app,
        "PATCH",
        "/api/board/statuses/security-review",
        Some(json!({ "gate": ["Threat model written"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let card = create(&app, json!({ "title": "gate me", "type": "chore" })).await;
    let id = card["id"].as_str().unwrap().to_string();

    // Unacknowledged -> 409 carrying the column's own gate.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "security-review" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "column gate not enforced: {v}");
    assert_eq!(v["gate"], json!(["Threat model written"]), "{v}");

    // Acknowledged -> through, with the ack recorded on the card.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "security-review", "gate_checked": ["Threat model written"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], json!("security-review"), "{v}");
    assert!(
        v["log"].as_str().unwrap_or("").contains("gate satisfied via gate_checked"),
        "the ack must be recorded on the card: {v}"
    );
}

// AC-323: `desc_append` must APPEND, and must never be reported ignored.
//
// The cutover dropped the field. The server correctly listed it in
// `ignored_fields`, but `amux board progress` only checked that the reply had
// an `id`, so it printed "progress noted" and wrote nothing — for weeks, on the
// verb CLAUDE.md tells sessions to use to record an outcome BEFORE a gate
// transition. Two outcome records were lost to it in the session that found it.
//
// Python's own comment (amux-server.py:69887) records the harsher earlier form:
// accepted, ignored, and the destructive replace ran anyway — ~20 silent wipes
// in one day. Hence the wipe assertions below, not just the append ones.
#[tokio::test]
async fn desc_append_appends_and_is_never_reported_ignored() {
    let (app, _dir) = app();
    let v = create(&app, json!({ "title": "Outcome", "desc": "original body" })).await;
    let id = v["id"].as_str().unwrap().to_string();

    // {desc_append: "text"} -> old + "\n" + text
    let (_, _, r) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc_append": "first note" })),
    )
    .await;
    let ignored = r["ignored_fields"].as_array().cloned().unwrap_or_default();
    assert!(
        !ignored.iter().any(|f| f == "desc_append"),
        "desc_append must be honoured, not reported ignored; got {ignored:?}"
    );

    let (_, _, d) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(
        d["desc"].as_str().unwrap(),
        "original body\nfirst note",
        "append must PRESERVE the prior body"
    );

    // {desc: "text", desc_append: true} -> the same append
    send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc": "second note", "desc_append": true })),
    )
    .await;
    let (_, _, d) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(
        d["desc"].as_str().unwrap(),
        "original body\nfirst note\nsecond note"
    );

    // An empty append is a NO-OP, never a wipe. This is the assertion that
    // would have caught the ~20 silent wipes: the dangerous failure is not
    // "append did nothing", it is "append replaced".
    send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc_append": "" })),
    )
    .await;
    let (_, _, d) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(
        d["desc"].as_str().unwrap(),
        "original body\nfirst note\nsecond note",
        "an empty append must not erase the body"
    );

    // {desc_append: false} is an explicit opt-OUT: plain replace semantics.
    // Without this the escape from append-mode would not exist.
    send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc": "replaced", "desc_append": false })),
    )
    .await;
    let (_, _, d) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(d["desc"].as_str().unwrap(), "replaced");
}

// ---- POST /api/board/clear-done (AMUX-2630) ------------------------------
//
// The button was dead: the SPA POSTed, the GET-only catch-all answered 405,
// the optimistically-hidden cards came back on refresh, and nothing was ever
// said. `route_table.rs` now walks the table entry so a MISSING route fails
// the build — but a mounted route can still be wrong in the one way that
// cannot be undone, so this pins BEHAVIOUR:
//
//   * it ARCHIVES, it does not delete. On the live board this runs against a
//     957-card done column; a delete would be irreversible loss of the user's
//     own record (ethos rule 8). The assertion that the rows are still
//     readable afterwards is the whole point of this test.
//   * it reports HOW MANY (ethos rule 4). A bare {"ok":true} cannot tell
//     "archived 957" from "matched nothing", which is exactly the ambiguity
//     that let the dead button pass for a working one.
#[tokio::test]
async fn clear_done_archives_and_reports_the_count() {
    let (app, _d) = app();

    let mut done_ids = Vec::new();
    for i in 0..3 {
        let v = create(&app, json!({ "title": format!("finished {i}"), "status": "done" })).await;
        done_ids.push(v["id"].as_str().unwrap().to_string());
    }
    let keep = create(&app, json!({ "title": "still open", "status": "todo" })).await;
    let keep_id = keep["id"].as_str().unwrap().to_string();

    let (st, _, v) = send(&app, "POST", "/api/board/clear-done", None).await;
    // Not 405: the failure this card is about is the route not existing.
    assert_eq!(st, StatusCode::OK, "clear-done did not answer 200: {v}");
    assert_eq!(v["archived"], json!(3), "must report the count, not a bare flag: {v}");
    assert_eq!(v["action"], json!("archived"), "the payload must say it archived: {v}");

    // NOT DELETED. Each card is still individually readable, with archived=1.
    for id in &done_ids {
        let (st, _, row) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
        assert_eq!(st, StatusCode::OK, "clear-done DELETED {id} — data loss");
        assert_eq!(row["archived"], json!(1), "{id} should be archived, got {row}");
        assert_eq!(row["status"], json!("done"), "status must be untouched: {row}");
    }
    // ...and gone from the default (non-archived) view, which is what the
    // button promises.
    let (_, _, active) = send(&app, "GET", "/api/board?archived=0", None).await;
    let live: Vec<&str> = active
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    for id in &done_ids {
        assert!(!live.contains(&id.as_str()), "{id} still in the default view");
    }
    assert!(live.contains(&keep_id.as_str()), "a todo card was swept: {active}");

    // Idempotent, and honest about it: nothing left to archive reports 0
    // rather than repeating the first run's number.
    let (st, _, v) = send(&app, "POST", "/api/board/clear-done", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["archived"], json!(0), "second run must report 0: {v}");
}

// ---- AC-322: X-Amux-Worker is attribution here too -----------------------
//
// board.rs was the ONE module of eight that read `x-amux-session` alone, while
// the installed `amux` CLI is the bash script whose 14 board-path PATCH sites
// all send `X-Amux-Worker`. Both tests below fail against the pre-fix
// `actor_from_headers` (they were written against it), and they cover the TWO
// independent things that one header resolution broke. The controls matter as
// much as the cases: without them a blanket "always attributed" bug would pass.

/// EFFECT 1 — the sanctioned escape becomes walkable again.
///
/// `amux board <status> --force` sends X-Amux-Worker, so the force check saw
/// `api-anonymous` and refused with an error telling the caller to use the CLI
/// they had just used. Ethos rule 6: a constraint whose sanctioned escape is
/// unwalkable from the audited path gets walked from an unaudited one.
#[tokio::test]
async fn force_accepts_x_amux_worker_attribution_like_every_other_module() {
    let (app, _d) = app();
    let item = create(&app, json!({ "title": "gated card", "type": "code" })).await;
    let id = item["id"].as_str().unwrap().to_string();

    // CONTROL: no header at all must still be refused, or this test would pass
    // against a build that simply stopped checking.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "force": true })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "unattributed force must refuse: {v}");
    // ORDERING MATTERS, and this assertion pins it. This control omits BOTH a
    // header and a reason, so it trips two checks; attribution must be the one
    // that answers, or the test silently stops covering attribution at all and
    // starts covering AMUX-3464's reason check while still passing.
    assert_eq!(v["error"], json!("force requires attribution"), "{v}");

    // THE CASE: the spelling the bash CLI actually sends. The `reason` is
    // required since AMUX-3464 (an audit line reading `reason=` recorded no
    // judgment) and the CLI sends it; it is incidental to what THIS test is
    // about, which is that x-amux-worker counts as attribution.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "force": true, "reason": "gate does not fit" })),
        &[("x-amux-worker", "amux")],
    )
    .await;
    assert_ne!(
        v["error"], json!("force requires attribution"),
        "X-Amux-Worker IS attribution — every other module accepts it: {v}"
    );
    assert_eq!(st, StatusCode::OK, "forced transition should apply: {v}");

    // And the ledger must NAME the forcer — an unattributed-in-effect force
    // that merely stopped erroring would be the worse bug (ethos rule 6).
    let (_, _, row) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    let log = row["log"].as_str().unwrap_or_default().to_string();
    assert!(
        log.contains("amux"),
        "the force must be attributed to the caller in the log: {log}"
    );
}

/// EFFECT 2 — the cross-lane ARCHIVE guard stops being blind.
///
/// `caller_lane` derives from the same resolver, and an EMPTY caller_lane
/// disables the guard entirely (board.rs: `!caller_lane.is_empty() && ...`).
/// So AMUX-2492's protection — one lane may not archive another lane's card —
/// was open for every bash-CLI caller for as long as the CLI has sent
/// X-Amux-Worker. Fixing the header closes this without touching the guard.
#[tokio::test]
async fn cross_lane_archive_guard_sees_x_amux_worker_callers() {
    let (app, _d) = app();
    let item = create(&app, json!({ "title": "lane-a's card", "session": "lane-a" })).await;
    let id = item["id"].as_str().unwrap().to_string();

    // THE CASE: a DIFFERENT lane archives it, identifying itself the way the
    // bash CLI does. Pre-fix caller_lane was "" and this silently succeeded.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "archived": 1 })),
        &[("x-amux-worker", "lane-b")],
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "lane-b archiving lane-a's card must be refused: {v}"
    );
    assert_eq!(v["error"], json!("cross-lane destruction requires authorized_by"), "{v}");

    // CONTROL A: the card must be untouched — a guard that refuses AND writes
    // is not a guard.
    let (_, _, row) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(row["archived"], json!(0), "refused archive must not write: {row}");

    // CONTROL B: the OWNER archiving its own card is still allowed, so the test
    // discriminates rather than proving archiving is broken for everyone.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "archived": 1 })),
        &[("x-amux-worker", "lane-a")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the owner may archive its own card: {v}");
}

// AVE-36: `amux board progress` reported success while notifying nobody, and
// `ask` notified — nothing at the call site distinguished delivery from
// intent, so three cross-group confirms went unread on a card the owner was
// actively working. A non-owner's desc_append now earns the owner a notice,
// and the RESPONSE says honestly whether one was sent. In tests no lane runs,
// so the honest answer is the not-running reason — which is exactly the cell
// the incident lived in from the sender's side.
#[tokio::test]
async fn a_peers_progress_note_reports_owner_notification_honestly() {
    let (app, _dir) = app();
    let v = create(&app, json!({ "title": "Handoff", "session": "ave36-nonexistent-owner" })).await;
    let id = v["id"].as_str().unwrap().to_string();

    // A DIFFERENT lane appends: the response must carry the notify verdict.
    let (_, _, r) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc_append": "confirm: BOTH PASS" })),
        &[("X-Amux-Session", "ai-video-editor")],
    )
    .await;
    assert_eq!(r["applied"], json!(true), "{r}");
    assert_eq!(r["owner_notified"], json!(false), "{r}");
    assert!(
        r["owner_notify_reason"].as_str().unwrap().contains("not running"),
        "the reason must name WHY nobody was told: {r}"
    );

    // The owner's own append is a self-note: no notify verdict at all.
    let (_, _, r) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc_append": "my own journal line" })),
        &[("X-Amux-Session", "ave36-nonexistent-owner")],
    )
    .await;
    assert_eq!(r["applied"], json!(true), "{r}");
    assert!(r.get("owner_notified").is_none(), "a self-note must not claim a notice: {r}");

    // An unattributed append (server-side automation) notifies nobody either.
    let (_, _, r) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc_append": "automation outcome line" })),
    )
    .await;
    assert!(r.get("owner_notified").is_none(), "{r}");
}

// ---- AF-137 / AMUX-3464: discarding an auto-filed report RE-ARMS its detector

/// The filing dedupe is a PERMANENT session_events idem row, so a discarded
/// report whose idem survives suppresses every future recurrence of that
/// signature — the signal dies with the card. The discard transition must
/// clear it (and ONLY the autofix idem: a non-autofix source_ref card must
/// touch nothing).
/// AMUX-3472: OCCURRENCE-class reports are judged forever. The outlier
/// signature names specific requests, so its idem must SURVIVE the discard —
/// re-arming it refiled the identical specimen while it sat in the scan
/// window. New occurrences mint new signatures and file regardless.
#[tokio::test]
async fn discarding_an_outlier_report_does_not_rearm() {
    let (app, store, _dir) = app_with_store();
    let made = create(
        &app,
        serde_json::json!({"title": "outlier report", "session": "amux",
                            "type": "investigation",
                            "desc": "Filed automatically by amux (runtime_jobs/autofix)"}),
    )
    .await;
    let id = made["id"].as_str().unwrap().to_string();
    store
        .write({
            let id = id.clone();
            move |conn| {
                conn.execute(
                    "UPDATE issues SET source_ref='autofix:latency|outlier|PATCH|/api/x|123' WHERE id=?1",
                    rusqlite::params![id],
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                     VALUES (1, 'amux', 'autofix.filed', '{}', 'autofix:latency|outlier|PATCH|/api/x|123', 'autofix')",
                    [],
                )?;
                Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
            }
        })
        .unwrap();
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(serde_json::json!({"status": "discarded", "gate_ack": true,
                            "desc_append": "judged: churn-window specimen"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success(), "discard must apply: {st}");
    let n: i64 = store
        .read()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM session_events WHERE idem='autofix:latency|outlier|PATCH|/api/x|123'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "an occurrence-class idem must SURVIVE the discard — deleting it refiles the same specimen");
}

#[tokio::test]
async fn discarding_an_autofix_report_rearms_the_detector() {
    let (app, store, _dir) = app_with_store();
    let made = create(
        &app,
        serde_json::json!({"title": "auto report", "session": "amux",
                            "type": "investigation",
                            "desc": "Filed automatically by amux (runtime_jobs/autofix)"}),
    )
    .await;
    let id = made["id"].as_str().unwrap().to_string();
    // Arm: source_ref + the idem row, exactly as the filer writes them.
    store
        .write({
            let id = id.clone();
            move |conn| {
                conn.execute(
                    "UPDATE issues SET source_ref='autofix:sig-x' WHERE id=?1",
                    rusqlite::params![id],
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                     VALUES (1, 'amux', 'autofix.filed', '{}', 'autofix:sig-x', 'autofix')",
                    [],
                )?;
                Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
            }
        })
        .unwrap();
    let idem_count = |store: &std::sync::Arc<Store>| -> i64 {
        store
            .read()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM session_events WHERE idem='autofix:sig-x'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(idem_count(&store), 1, "fixture must actually be armed");
    // Discard through the REAL handler (gate_ack: a report retirement is the
    // worker's judgment, which is exactly what the ack asserts).
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(serde_json::json!({"status": "discarded", "gate_ack": true,
                            "desc_append": "retired: condition re-checked, recovered"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success(), "discard must apply: {st}");
    assert_eq!(
        idem_count(&store),
        0,
        "the discard transition must clear the autofix idem — a surviving row \
         suppresses every future recurrence of this signature"
    );
}


// ---- AF-160: a gate that says "name them" must collect the name -----------

/// The gate's second criterion is `Peer-reviewed by a DIFFERENT worker in group
/// `amux` (name them)`. Acking it asserted a peer reviewed the work and recorded
/// WHO nowhere, so the assertion was unfalsifiable — measured 2026-08-23, 148 of
/// 1632 live verified cards named a peer and 45 of 1381 archived ones. 91% of the
/// board passed this gate with nothing machine-readable behind it.
///
/// The predicate is reviewer != the card's OWNER, never reviewer != whoever is
/// typing. The first draft compared against the ACTING session and would have
/// refused both real verifications made on this board within the hour (AF-161:
/// owner amux, reviewer and actor amux-frustrations; AF-16: the mirror). The peer
/// signing off IS the one acting — criterion 3 says they verify it themselves.
#[tokio::test]
async fn a_gate_that_asks_you_to_name_the_peer_refuses_until_a_peer_is_named() {
    let (app, _dir) = app();
    let gate = json!([
        "Functionality change is live and exercised, not just merged",
        "Peer-reviewed by a DIFFERENT worker in group `amux` (name them)",
    ]);
    let card = create(
        &app,
        json!({
            "title": "named-peer gate",
            "status": "review",
            "session": "amux",
            "desc": "artifact: crates/amux-server/src/api/board.rs",
            "gate": gate,
        }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();
    let ack = json!([
        "Functionality change is live and exercised, not just merged",
        "Peer-reviewed by a different worker in group amux (amux-frustrations)",
    ]);

    // 1. The ack NOW MATCHES — the name is filled into the parenthetical, the
    //    case differs and the backticks are gone, all three of which used to
    //    refuse. Getting past it to the named-peer check is itself the proof.
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "verified", "gate_checked": ack })),
        &[("X-Amux-Session", "amux-frustrations")],
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_ne!(
        v["error"], json!("gate_checked does not match the gate"),
        "the filled-in ack must be ACCEPTED as matching — refusing it here is the original bug"
    );
    assert_eq!(v["ok"], json!(false));
    assert_eq!(v["blocked"], json!(true));
    assert!(
        v["error"].as_str().unwrap().contains("no reviewer is recorded"),
        "the refusal must name the real reason: {v}"
    );
    // The remedy must be reachable with sanctioned tooling, or this gate
    // manufactures the hand-rolled curls the audit trail depends on not existing.
    assert!(v["how_to_fix"]["cli"].as_str().unwrap().starts_with("amux board reviewer "));

    // 2. A SELF-REVIEW is refused: reviewer == the card's owner.
    let (st, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "reviewer": "amux" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "verified", "gate_checked": ack })),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("self-review"),
        "owner reviewing their own card must be refused: {v}"
    );

    // 3. THE MIRROR CASE, which the first draft of this rule would have refused:
    //    reviewer is a different session from the OWNER, and is also the one
    //    acting. That is correct and must pass.
    let (st, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "reviewer": "amux-frustrations" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "verified", "gate_checked": ack })),
        &[("X-Amux-Session", "amux-frustrations")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the peer signing off themselves must pass: {v}");
    assert_eq!(v["status"], json!("verified"));
}

/// A gate with NO named-peer criterion is untouched — the check must not leak
/// onto every other card on the board.
#[tokio::test]
async fn a_gate_without_a_named_peer_criterion_needs_no_reviewer() {
    let (app, _dir) = app();
    let card = create(
        &app,
        json!({
            "title": "ordinary code card",
            "status": "doing",
            "session": "amux",
            "desc": "artifact: crates/amux-server/src/api/board.rs",
        }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();
    let (st, _, v) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({
            "status": "done", "evidence": EV,
            "gate_checked": ["Implemented and merged", "Tests / lint pass"]
        })),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "no reviewer required when nothing asks for one: {v}");
    assert_eq!(v["status"], json!("done"));
}

// ---- AF-169: a hint that cannot apply must not print ----------------------

/// The 409 said "set its type — the gate is DERIVED from the type" on EVERY
/// refusal. That is true only when the type default is what refused. For a card
/// in a worker- or group-scoped gate, retyping changes nothing: AF-168's
/// reporter retyped TUBES-2053 code -> research, watched the done gate not move,
/// and concluded the override was pinned per-card. The hint sent them there.
///
/// Both branches are asserted. A fix that simply deleted `wrong_type?` would
/// pass the scoped case and fail the type-default one, where the advice is
/// correct and useful.
#[tokio::test]
async fn the_retype_hint_prints_only_when_retyping_would_actually_help() {
    let (app, _dir) = app();

    // (a) TYPE DEFAULT refused -> the hint is right, and must still appear.
    // `artifact:` in the desc satisfies done_requires_asset_link, which fires
    // BEFORE the ack gate — without it this test never reaches the hint at all.
    let card = create(&app, json!({
        "title": "plain code card", "status": "doing",
        "desc": "artifact: crates/amux-server/src/api/board.rs",
    })).await;
    let id = card["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(&app, "PATCH", &format!("/api/board/{id}"), Some(json!({ "status": "done", "evidence": EV }))).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(
        v["how_to_ack"]["wrong_type?"].is_string(),
        "a type-derived gate is exactly when retyping helps: {v}"
    );
    assert!(v["how_to_ack"]["gate_source"].is_null());

    // (b) A CARD-SCOPED gate refused -> retyping cannot clear it, so the hint
    //     must be REPLACED by one that names where the bar came from.
    let scoped = create(
        &app,
        json!({
            "title": "card with its own gate",
            "status": "doing",
            "desc": "artifact: crates/amux-server/src/api/board.rs",
            "gate": ["Something only this card demands"],
        }),
    )
    .await;
    let sid = scoped["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(&app, "PATCH", &format!("/api/board/{sid}"), Some(json!({ "status": "done", "evidence": EV }))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(
        v["how_to_ack"]["wrong_type?"].is_null(),
        "retyping cannot clear a card-scoped gate, so the hint must NOT print: {v}"
    );
    let src = v["how_to_ack"]["gate_source"].as_str().unwrap_or("");
    assert!(
        src.contains("gate` override") || src.contains("not what refused"),
        "the refusal must say WHERE the bar came from instead: {v}"
    );
}

// ---- AMUX-3567: the contract says WHERE each gate came from ---------------

/// Nothing surfaced that a worker or group carries a custom gate until you
/// tripped it. The contract already served the RESOLVED gate; it did not say
/// which tier produced it, so a reader could not tell a type default (which
/// retyping changes) from a scoped override (which it cannot).
///
/// Per TRANSITION, not per card: `group:amux` pins only `verified`,
/// `tubescience` only `done`. A card-level summary would be wrong for every
/// transition it did not describe.
#[tokio::test]
async fn the_contract_says_which_tier_each_gate_came_from() {
    let (app, _dir) = app();
    let card = create(
        &app,
        json!({ "title": "plain", "status": "todo", "desc": "artifact: x/y.rs" }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    let (st, _, v) = send(&app, "GET", &format!("/api/board/contract?card={id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    let src = &v["card_effective_gates"]["gate_sources"];
    assert!(src.is_object(), "the contract must name the source per transition: {v}");

    // A card with no override anywhere resolves from the TYPE, and there the
    // retype advice is correct — this is the cell that fails if the field is
    // hardcoded to "scoped".
    let done = &src["done"];
    assert_eq!(
        done["retype_would_change_it"], json!(true),
        "a type-derived gate IS changed by retyping: {v}"
    );
    assert!(done["explain"].as_str().unwrap().contains("TYPE"), "{v}");

    // Now pin the card's own gate and the same transition must flip — the
    // control that stops this being a constant. A fix that always reported
    // "type default" passes the assertions above and fails here.
    let (st, _, _) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "gate": ["Something only this card demands"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, _, v2) = send(&app, "GET", &format!("/api/board/contract?card={id}"), None).await;
    let done2 = &v2["card_effective_gates"]["gate_sources"]["done"];
    assert_eq!(
        done2["retype_would_change_it"], json!(false),
        "a card-scoped gate is NOT changed by retyping, and the contract must say so: {v2}"
    );
}

/// AF-187: the refusal names `desc_append` and must SHOW it, not just say it.
///
/// A lane POSTed /api/board/BACKE-3531/desc-append twice, 33s apart, and got 404
/// both times. That path is not routed and should not be — `desc_append` is a
/// PATCH field. But the guess is not careless: this API's board resource has six
/// real POST sub-paths (archive, restore, status-request, status-update, claim),
/// so a bare field name reads as a path here.
///
/// ASSERTING ON THE TEXT IS CORRECT IN THIS ONE CASE. The general rule is that a
/// text mutation measures coupling rather than correctness — but that holds when
/// the string is incidental to the thing under test. Here the refusal body IS
/// the product: its whole job is to be readable and copyable. So the artifact
/// under test is the text, and mutating it is the right instrument.
///
/// The structured half is the part a machine can use, and it is asserted
/// separately so the test does not rest on prose alone.
#[tokio::test]
async fn the_desc_shrink_refusal_shows_how_to_append_not_just_the_field_name() {
    let (app, _dir) = app();
    let long = "x".repeat(900);
    let card = create(&app, json!({ "title": "long desc", "status": "todo", "desc": long })).await;
    let id = card["id"].as_str().unwrap().to_string();

    // A replace dropping a strict majority of a 500+ char desc trips the SIZE rule.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "desc": "tiny" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "a 900 -> 4 char replace must be refused: {v}");
    assert_eq!(v["kind"], json!("desc_shrink_blocked"), "{v}");

    // THE MACHINE-READABLE HALF: a caller can build the request without prose.
    let ex = &v["append_example"];
    assert_eq!(ex["method"], json!("PATCH"), "{v}");
    assert_eq!(ex["path"], json!(format!("/api/board/{id}")), "{v}");
    assert!(
        ex["body"]["desc_append"].is_string(),
        "the example body must carry the FIELD, or it does not show the shape: {v}"
    );

    // THE PROSE HALF: the sentence a human reads must carry the shape too, since
    // that is the half that got guessed as a path.
    let err = v["error"].as_str().unwrap_or("");
    assert!(err.contains("desc_append"), "still name the field — it is what a reader greps: {err:?}");
    assert!(
        err.contains(&format!("/api/board/{id}")) || err.contains("in place of"),
        "the sentence must show the invocation, not just the name: {err:?}"
    );

    // CONTROL: the bare field name stays where a machine already looked for it.
    // Replacing it with the example would break every existing consumer.
    assert_eq!(v["append_instead"], json!("desc_append"), "{v}");
    assert_eq!(v["ack_field"], json!("desc_shrink_ack"), "{v}");

    // THE OTHER BRANCH, and it is here because a mutation caught its absence.
    // The block above trips the SIZE rule (a lane shrinking its own desc). The
    // AUTHORSHIP rule is a different string on a different path, and dropping
    // the shape from it left this test GREEN — a cell that could not fail on
    // half the feature it claimed to cover.
    let owned = create(
        &app,
        json!({ "title": "owned", "status": "todo", "session": "lane-a",
                "desc": "y".repeat(900) }),
    )
    .await;
    let oid = owned["id"].as_str().unwrap().to_string();
    let (st2, _, v2) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{oid}"),
        Some(json!({ "desc": "my review note" })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st2, StatusCode::CONFLICT, "a peer replacing the owner's prose must refuse: {v2}");
    assert_eq!(v2["rule"], json!("authorship"), "this must be the OTHER branch: {v2}");
    let err2 = v2["error"].as_str().unwrap_or("");
    assert!(err2.contains("desc_append"), "{err2:?}");
    assert!(
        err2.contains("not a sub-path"),
        "the authorship branch must show the shape too — it is the one a REVIEWER reads,          and reviewers are who guessed the path: {err2:?}"
    );
    assert_eq!(v2["append_example"]["method"], json!("PATCH"), "{v2}");

    // AF-191: THE TWO SPECIMENS THE NUMERIC FLOORS LET THROUGH, end to end, so
    // this pins the WRITE SITE and not only the predicate. Both were reproduced
    // against the running server on scratch cards and both returned
    // `applied: true` with the owner's text gone.
    //
    // (a) a LONGER replacement. The old rule required a 200-char net LOSS, and
    // this one grows, so nothing fired while every character was destroyed.
    let grew = create(
        &app,
        json!({ "title": "owned", "status": "todo", "session": "lane-a",
                "desc": "their line one\ntheir line two\ntheir line three" }),
    )
    .await;
    let gid = grew["id"].as_str().unwrap().to_string();
    let (st3, _, v3) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{gid}"),
        Some(json!({ "desc": "TOTALLY DIFFERENT CONTENT, and noticeably longer than what it \
                             replaced, which is the point of this case." })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st3, StatusCode::CONFLICT, "a LONGER replacement destroys just as much: {v3}");
    assert_eq!(v3["rule"], json!("authorship"), "{v3}");

    // (b) a SHORT card. The old rule required `before >= 200`; amux-cloud's
    // reported incident was 54 chars replaced by 17.
    let small = create(
        &app,
        json!({ "title": "owned", "status": "todo", "session": "lane-a",
                "desc": "their whole one-line description here" }),
    )
    .await;
    let sid = small["id"].as_str().unwrap().to_string();
    let (st4, _, v4) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{sid}"),
        Some(json!({ "desc": "mine now" })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st4, StatusCode::CONFLICT, "a SHORT desc is not exempt: {v4}");

    // CONTROL, and it carries the weight: a peer editing ONE line of a
    // multi-line write-up keeps the rest and must still pass, or the guard
    // becomes a refusal met during ordinary work, which is how a safety
    // property turns into a reflexive ack.
    let (st5, _, v5) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{gid}"),
        Some(json!({ "desc": "their line one\ntheir line TWO (typo fixed)\ntheir line three" })),
        &[("X-Amux-Session", "lane-b")],
    )
    .await;
    assert_eq!(st5, StatusCode::OK, "a typo fix that keeps the other lines must pass: {v5}");
}

/// AMUX-3567 REVIEW (amux-frustrations): the same answer for the WORKER tier,
/// which is the tier the card is named after and the one the original cell
/// skipped. It went type-default -> card-override, so `GateSource::Worker` was
/// never produced by any HTTP test; a mutation making `retype_would_help`
/// return true for Worker and Group left the whole suite green.
///
/// TUBES-2053's shape exactly: a card owned by a worker that carries its own
/// `done` gate. Retyping it does nothing, and reading the contract must say so
/// BEFORE the transition is refused, which is the entire point of the card.
#[tokio::test]
async fn the_contract_names_a_worker_scoped_gate_before_you_trip_it() {
    let (app, _dir) = app();
    let card = create(
        &app,
        json!({
            "title": "owned by a worker with its own bar",
            "status": "todo",
            "session": "tubes-like",
            "desc": "artifact: x/y.rs",
        }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    // BEFORE: nothing is scoped, so the gate is the type's and retyping helps.
    let (_, _, v0) = send(&app, "GET", &format!("/api/board/contract?card={id}"), None).await;
    assert_eq!(
        v0["card_effective_gates"]["gate_sources"]["done"]["retype_would_change_it"],
        json!(true),
        "with nothing scoped the gate is the type's: {v0}"
    );

    // Give that worker a `done` gate through the endpoint the SPA uses.
    let (st, _, _) = send(
        &app,
        "PATCH",
        "/api/board/session-gates",
        Some(json!({ "worker": "tubes-like", "status": "done", "gate": ["Worker-scoped bar"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // AFTER: same card, same transition, and the advice must invert.
    let (_, _, v1) = send(&app, "GET", &format!("/api/board/contract?card={id}"), None).await;
    let done = &v1["card_effective_gates"]["gate_sources"]["done"];
    assert_eq!(
        done["retype_would_change_it"], json!(false),
        "a worker-scoped gate ignores the item type — this is what AF-168's reporter \
         was not told, and they concluded the override was pinned per-card: {v1}"
    );
    let explain = done["explain"].as_str().unwrap_or("");
    assert!(
        explain.contains("tubes-like") && explain.contains("WORKER"),
        "name the worker and the tier, or the reader cannot go look: {explain:?}"
    );
    assert_eq!(
        v1["card_effective_gates"]["gates"]["done"],
        json!(["Worker-scoped bar"]),
        "and the resolved gate itself must be the worker's: {v1}"
    );

    // A DIFFERENT transition on the SAME card stays type-derived. This is the
    // per-transition claim the commit message makes, and without this cell a
    // per-card summary would pass every assertion above.
    let review = &v1["card_effective_gates"]["gate_sources"]["review"];
    assert_eq!(
        review["retype_would_change_it"], json!(true),
        "only `done` was pinned — the other transitions are still the type's: {v1}"
    );
}

// ---- AMUX-3686: a --trigger must not eat an autofix dedupe signature -------
//
// AF-459. A trigger replacing a trigger is ALLOWED (the test below this one
// pins that, and it is correct). What was missing is that it left no record of
// what it destroyed: the PATCH log builds one line per patch out of a Vec of
// FIELD NAMES, so an overwrite rendered as the bare word "source_ref".
//
// gtm-engine lost a five-item inventory from 2026-08-09 that way, probing
// whether --trigger works on an archived card. It does. They recovered four
// items from a prefix they happened to have printed earlier in their own
// transcript; the fifth is gone. /api/history carries no row with the value
// either, so the column that got written was the only copy that existed.
//
// Driven through the real router for the same reason as the test below: the
// decision lives in the handler.
#[tokio::test]
async fn overwriting_a_trigger_records_the_value_it_destroyed() {
    let (app, _dir) = app();

    let (_s, _h, created) = send_with(
        &app,
        "POST",
        "/api/board",
        Some(json!({"title": "parked card", "status": "todo", "session": "amux"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    // The value that must survive its own replacement. Long on purpose: it is
    // an inventory, which is what the real loss was.
    let original = "inventory 2026-08-09: (1) shard roll evidence (2) warm-search p50 \
                    (3) tenant caps (4) scheduler breach counts (5) eviction gap notes";
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"source_ref": original})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success());

    // THE SPECIMEN: a second trigger replaces the first. Allowed, and it must
    // not be silent.
    let replacement = "the next ts-engine roll";
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"source_ref": replacement})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success(), "replacing a trigger with a trigger stays allowed");

    let (_s, _h, detail) = send_with(&app, "GET", &format!("/api/board/{id}"), None, &[]).await;
    let log = detail["log"].as_str().unwrap_or("");

    // 1. THE DESTROYED VALUE IS RECOVERABLE FROM THE CARD. This is the whole
    //    card: not that the field moved, but what it moved FROM.
    assert!(
        log.contains("eviction gap notes"),
        "the overwritten trigger must be recoverable from the card log, \
         otherwise the write is the only copy and it is gone: {log}"
    );
    // 2. Kept LONG. A truncated sole copy reproduces the exact partial recovery
    //    gtm-engine got by accident — they retrieved a prefix and lost the tail.
    //    The fifth item is the tail, which is why it is the one asserted above.
    assert!(
        log.contains("shard roll evidence") && log.contains("scheduler breach"),
        "head AND tail of the destroyed value must survive, not just a prefix: {log}"
    );
    // 2b. THIS FIXTURE IS 132 CHARACTERS AND THE CAP WAS 200, so the assertion
    //     above is satisfied by ANY cap at or above 132 and cannot see the
    //     boundary at all (gtm-engine, 2026-09-04, refusing to validate the
    //     entry this test was written for). Mutating the cap to 60 reddens it;
    //     mutating it to 201, the shipped value, does not. Their real loss was
    //     366 characters, which the fix head-truncated to 201 exactly as before.
    //
    //     A fixture that cannot cross the boundary cannot test it, so the cell
    //     below builds one that must. It is asserted through the ELISION MARKER
    //     rather than through a constant this integration target cannot see: if
    //     the fixture ever stops crossing, the marker is absent and this fails,
    //     so the cell cannot go quietly vacuous the way the one above did.
    let long_head = "HEADSENTINEL-inventory-2026-09-04";
    let long_tail = "TAILSENTINEL-the-fifth-item-that-was-lost";
    let long_original =
        format!("{long_head}{}{long_tail}", "x".repeat(2600));
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"source_ref": long_original})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success());
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"source_ref": "a third trigger, replacing the long one"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success());
    let (_s, _h, detail) = send_with(&app, "GET", &format!("/api/board/{id}"), None, &[]).await;
    let log = detail["log"].as_str().unwrap_or("");
    // SELECT THE LINE WHERE IT IS THE *DESTROYED* VALUE, not the one where it
    // ARRIVED. Both mention the fixture, and the arriving side is truncated at
    // 60 by design — a whole-log `contains` would read the wrong record and
    // this cell would be asserting the wrong half. The discriminator is the
    // arriving value on the NEXT write, which is unique.
    let was_line = log
        .lines()
        .find(|l| l.contains("a third trigger, replacing the long one"))
        .unwrap_or_else(|| panic!("the over-long destroyed value must be logged: {log}"));
    assert!(
        was_line.contains("chars elided"),
        "premise: the fixture must EXCEED the bound, or this cell measures nothing: {was_line}"
    );
    assert!(
        was_line.contains(long_tail),
        "the TAIL of an over-long destroyed value must survive: a prefix is the \
         failure this card exists for, and the tail is the item gtm-engine lost: {was_line}"
    );
    assert!(
        was_line.contains(long_head),
        "and the head, so the elision is a middle rather than a suffix: {was_line}"
    );

    // 2c. gtm-engine's ACTUAL value size, which the previous fix silently cut.
    //     366 characters must come back WHOLE, with no elision marker at all.
    let real_case = format!("REALHEAD{}REALTAIL", "y".repeat(350));
    assert_eq!(real_case.chars().count(), 366, "premise: their measured size");
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"source_ref": real_case})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success());
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"source_ref": "a fourth trigger"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success());
    let (_s, _h, detail) = send_with(&app, "GET", &format!("/api/board/{id}"), None, &[]).await;
    let log = detail["log"].as_str().unwrap_or("");
    let line = log
        .lines()
        .find(|l| l.contains("-> a fourth trigger"))
        .unwrap_or_else(|| panic!("the 366-char destroyed value must be logged: {log}"));
    assert!(
        line.contains(&real_case),
        "a 366-character trigger is gtm-engine's real loss and must survive WHOLE: {line}"
    );
    assert!(
        !line.contains("chars elided"),
        "and must not be elided at all, or the fix is a bigger version of the bug: {line}"
    );
    // 3. It says WAS, so a reader can tell the old value from the new one.
    assert!(
        log.contains("source_ref: WAS"),
        "the log must mark which side is the destroyed value: {log}"
    );
    assert!(
        log.contains(replacement),
        "the arriving value is still named: {log}"
    );

    // 4. NEGATIVE ARM: a FIRST write destroys nothing, so it must not claim to.
    //    Without this the check would pass on an implementation that printed
    //    "WAS" unconditionally, which would read as data loss on every create.
    let (_s, _h, c2) = send_with(
        &app,
        "POST",
        "/api/board",
        Some(json!({"title": "fresh card", "status": "todo", "session": "amux"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    let id2 = c2["id"].as_str().unwrap().to_string();
    let (_s, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id2}"),
        Some(json!({"source_ref": "first trigger ever"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    let (_s, _h, d2) = send_with(&app, "GET", &format!("/api/board/{id2}"), None, &[]).await;
    let log2 = d2["log"].as_str().unwrap_or("");
    assert!(
        !log2.contains("WAS"),
        "a first write overwrote nothing and must not imply it did: {log2}"
    );
    assert!(
        log2.contains("first trigger ever"),
        "a first write still names the value it set: {log2}"
    );
}

// AF-469. A trigger with no verification time re-drains FOREVER, and nothing said
// so. board_drive's idle gate is "source_ref is empty OR last_verified_at is older
// than 24h", so a card parked with source_ref alone reads as a trigger nobody has
// re-checked on every single tick.
//
// `amux board <status> --trigger` stamps BOTH fields. A raw PATCH — the shape the
// CLAUDE.md board recipes teach — sets only source_ref. The two calls do the same
// visible thing and the operator sees a 200 either way.
//
// Measured 2026-09-04 by ts-gke on themselves: TG-3239 parked by raw PATCH was
// served four times in one session while eleven cards parked with the CLI stayed
// quiet. They filed a dispatch-ORDERING report on the strength of it — wrong
// population, wrong conclusion, wrong recommendation, sent to a peer — and the
// whole split was which call recorded the same act.
#[tokio::test]
async fn parking_with_a_trigger_and_no_verification_time_warns_the_caller() {
    let (app, _dir) = app();
    let mk = |title: &str| {
        let app = &app;
        let t = title.to_string();
        async move {
            let (_s, _h, c) = send_with(app, "POST", "/api/board",
                Some(json!({"title": t, "status": "todo", "session": "amux"})),
                &[("X-Amux-Session", "amux")]).await;
            c["id"].as_str().unwrap().to_string()
        }
    };

    // THE FOOT-GUN: source_ref alone, exactly as a raw curl PATCH sends it.
    let id = mk("parked by raw patch").await;
    let (st, _h, body) = send_with(&app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({"source_ref": "the next ts-engine roll"})),
        &[("X-Amux-Session", "amux")]).await;
    assert!(st.is_success(), "the write must still succeed: {body}");
    // `advisories`, NOT `diverted_fields`. The first version of this fix put the
    // note in `diverted`, and the control in
    // a_trigger_cannot_overwrite_an_autofix_signature_but_can_replace_a_trigger
    // caught it — that control asserts an ordinary trigger write reports NO
    // diversion, and exists so an advisory cannot fire on every source_ref write
    // and train readers to ignore it. A diversion means "the key you named is not
    // the key that changed"; nothing was diverted here.
    let warned = body["advisories"].as_array().map(|a| a.iter().any(|d| {
        d["field"] == "last_verified_at" && d["why"].as_str().unwrap_or("").contains("re-offer it")
    })).unwrap_or(false);
    assert!(warned, "a trigger with no last_verified_at must warn the caller: {body}");

    // AND IT NAMES THE FIX, because a warning that does not is a warning that gets
    // read once and ignored.
    let why = body["advisories"][0]["why"].as_str().unwrap_or("");
    assert!(why.contains("--trigger"), "the warning must name the verb that stamps both: {why}");

    // THE CLI's SHAPE: both fields together. Must stay SILENT, or the warning fires
    // on the correct path and becomes noise on every park.
    let id2 = mk("parked by the CLI").await;
    let (_s, _h, body2) = send_with(&app, "PATCH", &format!("/api/board/{id2}"),
        Some(json!({"source_ref": "the next ts-engine roll", "last_verified_at": 1788510477i64})),
        &[("X-Amux-Session", "amux")]).await;
    let quiet = body2["advisories"].as_array().map(|a| a.iter().all(|d| d["field"] != "last_verified_at")).unwrap_or(true);
    assert!(quiet, "the CLI shape stamps both and must not warn: {body2}");

    // CLEARING a trigger is not parking, so it must stay silent too.
    let (_s, _h, body3) = send_with(&app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({"source_ref": ""})), &[("X-Amux-Session", "amux")]).await;
    let quiet3 = body3["advisories"].as_array().map(|a| a.iter().all(|d| d["field"] != "last_verified_at")).unwrap_or(true);
    assert!(quiet3, "clearing a trigger must not warn about verification time: {body3}");
}

// AF-475. Posting an artifact to a card that does not exist answered
// `500 Query returned no rows` — the raw rusqlite error as the entire body.
//
// Found by the 2026-09-04 log sweep: one row, mixpeek-cicd, 0.25ms latency. The
// cost is not the wrong code, it is that the sweep's own contract says a 500 is
// ALWAYS a finding, so a client error wearing a 500 buys a real investigation
// every time it shows up in /api/logs/analyze.
#[tokio::test]
async fn an_artifact_on_a_missing_card_is_404_not_a_raw_db_error() {
    let (app, _dir) = app();
    // `implementation`, not `commit`: kind validation runs BEFORE the card
    // lookup, so an invalid kind 400s and never reaches the arm under test.
    let body = json!({"kind": "implementation", "ref": "deadbeef"});

    let (st, _h, b) = send_with(&app, "POST", "/api/board/NOPE-999/artifacts",
        Some(body.clone()), &[("X-Amux-Session", "amux")]).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a missing card is a client error: {b}");
    assert_eq!(b["error"], "no such task", "and it must say which fault: {b}");
    assert!(!b.to_string().contains("Query returned no rows"),
        "the raw storage error must not reach the caller: {b}");

    // THE OTHER ARM: a real card still succeeds. Without this the test passes on
    // a handler that 404s everything.
    let (_s, _h, c) = send_with(&app, "POST", "/api/board",
        Some(json!({"title": "has artifacts", "status": "todo", "session": "amux"})),
        &[("X-Amux-Session", "amux")]).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (st2, _h, b2) = send_with(&app, "POST", &format!("/api/board/{id}/artifacts"),
        Some(body), &[("X-Amux-Session", "amux")]).await;
    assert_eq!(st2, StatusCode::CREATED, "an artifact on a real card still lands: {b2}");
}

/// A worker-authored output is already attributed to one exact card. Capture
/// its produced refs there once, through the common board API every provider
/// uses, rather than scraping four provider-specific terminal formats.
#[tokio::test]
async fn worker_outputs_auto_register_every_provider_artifact_and_survive_done() {
    let (app, _dir) = app();
    let cases = [
        ("claude", "claude-output.md"),
        ("codex", "codex-output.png"),
        ("gemini", "https://example.test/gemini-output"),
        ("opencode", "53a868f"),
    ];

    for (provider, artifact_ref) in cases {
        let lane = format!("provider-{provider}");
        let (_st, _, made) = send_with(
            &app,
            "POST",
            "/api/board",
            Some(json!({
                "title": format!("{provider} output capture"),
                "status": "doing",
                "type": "chore",
                "session": lane,
            })),
            &[("X-Amux-Worker", lane.as_str())],
        )
        .await;
        let id = made["id"].as_str().unwrap().to_string();
        let update = json!({"text": format!("Produced {artifact_ref}; ready for review.")});

        for _ in 0..2 {
            let (st, _, body) = send_with(
                &app,
                "POST",
                &format!("/api/board/{id}/status-update"),
                Some(update.clone()),
                &[("X-Amux-Worker", lane.as_str())],
            )
            .await;
            assert_eq!(st, StatusCode::OK, "{provider} status update failed: {body}");
        }

        let (_, _, before) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
        let artifacts = before["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 1, "capture must be idempotent for {provider}: {before}");
        assert_eq!(artifacts[0]["task_id"], json!(id), "artifact leaked off {provider}'s card");
        assert_eq!(artifacts[0]["ref"], json!(artifact_ref));
        assert!(
            artifacts[0]["description"].as_str().unwrap_or("").contains(&lane),
            "the automatic registration must retain its provider lane attribution: {before}"
        );

        let (st, _, done) = send_with(
            &app,
            "PATCH",
            &format!("/api/board/{id}"),
            Some(json!({
                "status": "done",
                "evidence": "ran `cargo test -p amux-server`",
                "gate_ack": true,
            })),
            &[("X-Amux-Worker", lane.as_str())],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "terminal transition failed for {provider}: {done}");
        let (_, _, after) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
        assert_eq!(after["status"], json!("done"));
        assert_eq!(after["artifacts"][0]["ref"], json!(artifact_ref), "done lost {provider}'s artifact");
    }
}

/// GCA-153: the provider-independent status endpoint must turn a worker's
/// exact actionable card into current work. A stale source ref is provenance,
/// not an eternal trigger block.
#[tokio::test]
async fn own_status_update_claims_exact_todo_or_backlog_for_every_provider() {
    let (app, store, _dir) = app_with_store();
    for (provider, initial) in [
        ("claude", "todo"), ("codex", "backlog"),
        ("gemini", "todo"), ("opencode", "backlog"),
    ] {
        let lane = format!("claim-{provider}");
        let made = create(&app, json!({
            "title": format!("{provider} exact-card claim"),
            "status": initial, "session": lane,
            "desc": "Implement the exact-card status claim."
        })).await;
        let id = made["id"].as_str().unwrap().to_string();
        let id_for_db = id.clone();
        store.write(move |conn| {
            conn.execute(
                "UPDATE issues SET source_ref='message:153', last_verified_at=NULL WHERE id=?1",
                [&id_for_db],
            )?;
            Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let (st, _, body) = send_with(
            &app, "POST", &format!("/api/board/{id}/status-update"),
            Some(json!({"text": format!("{provider} is implementing {id}")})),
            &[("X-Amux-Worker", lane.as_str())],
        ).await;
        assert_eq!(st, StatusCode::OK, "{provider}: {body}");
        assert_eq!(body["claimed"], json!(true), "{provider}: {body}");
        assert_eq!(body["claim_verdict"], json!("claimed"), "{provider}: {body}");
        assert_eq!(body["status"], json!("doing"), "{provider}: {body}");

        let conn = store.read().unwrap();
        let (status, owner, log): (String, String, String) = conn.query_row(
            "SELECT status, COALESCE(session,''), COALESCE(log,'') FROM issues WHERE id=?1",
            [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(status, "doing");
        assert_eq!(owner, lane);
        assert!(log.contains("Claimed by status update"), "{log}");
        assert!(log.contains(&format!("STATUS ({lane})")), "{log}");
    }
}

/// Refused claims preserve their progress note without stealing ownership,
/// bypassing blockers/dependencies/WIP, or moving later states backward.
#[tokio::test]
async fn status_update_refuses_cross_worker_blocked_dependency_wip_and_later_states() {
    let (app, store, _dir) = app_with_store();

    let cross = create(&app, json!({
        "title":"owned elsewhere", "status":"todo", "session":"owner-lane"
    })).await;
    let cross_id = cross["id"].as_str().unwrap().to_string();
    let (_, _, body) = send_with(
        &app, "POST", &format!("/api/board/{cross_id}/status-update"),
        Some(json!({"text":"cross-worker note must survive"})),
        &[("X-Amux-Worker", "other-lane")],
    ).await;
    assert_eq!(body["claim_verdict"], json!("owner_mismatch"));

    let blocked = create(&app, json!({
        "title":"blocked card", "status":"todo", "session":"blocked-lane"
    })).await;
    let blocked_id = blocked["id"].as_str().unwrap().to_string();
    let blocked_for_db = blocked_id.clone();
    store.write(move |conn| {
        conn.execute("UPDATE issues SET blocked_on='network' WHERE id=?1", [&blocked_for_db])?;
        Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
    }).unwrap();
    let (_, _, body) = send_with(
        &app, "POST", &format!("/api/board/{blocked_id}/status-update"),
        Some(json!({"text":"still investigating the blocker"})),
        &[("X-Amux-Worker", "blocked-lane")],
    ).await;
    assert_eq!(body["claim_verdict"], json!("blocked"));

    let dep = create(&app, json!({
        "title":"open dependency", "status":"backlog", "session":"dep-lane"
    })).await;
    let dependent = create(&app, json!({
        "title":"dependent card", "status":"todo", "session":"dependent-lane",
        "depends_on":[dep["id"].as_str().unwrap()]
    })).await;
    let dependent_id = dependent["id"].as_str().unwrap().to_string();
    let (_, _, body) = send_with(
        &app, "POST", &format!("/api/board/{dependent_id}/status-update"),
        Some(json!({"text":"dependency has not cleared"})),
        &[("X-Amux-Worker", "dependent-lane")],
    ).await;
    assert_eq!(body["claim_verdict"], json!("dependency_blocked"));

    let triggered = create(&app, json!({
        "title":"fresh external trigger", "status":"backlog", "session":"trigger-lane"
    })).await;
    let triggered_id = triggered["id"].as_str().unwrap().to_string();
    let trigger_for_db = triggered_id.clone();
    store.write(move |conn| {
        conn.execute(
            "UPDATE issues SET source_ref='wait for upstream', last_verified_at=?1 WHERE id=?2",
            rusqlite::params![amux_server::config::now_f64() as i64, &trigger_for_db],
        )?;
        Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
    }).unwrap();
    let (_, _, body) = send_with(
        &app, "POST", &format!("/api/board/{triggered_id}/status-update"),
        Some(json!({"text":"upstream is still unavailable"})),
        &[("X-Amux-Worker", "trigger-lane")],
    ).await;
    assert_eq!(body["claim_verdict"], json!("external_trigger"));

    let holding = create(&app, json!({
        "title":"current work", "status":"doing", "session":"wip-lane"
    })).await;
    let target = create(&app, json!({
        "title":"queued work", "status":"todo", "session":"wip-lane"
    })).await;
    let target_id = target["id"].as_str().unwrap().to_string();
    let (_, _, body) = send_with(
        &app, "POST", &format!("/api/board/{target_id}/status-update"),
        Some(json!({"text":format!("still on {}", holding["id"])})),
        &[("X-Amux-Worker", "wip-lane")],
    ).await;
    assert_eq!(body["claim_verdict"], json!("wip_conflict"));

    for later in ["doing", "review", "done", "verified", "discarded"] {
        let lane = format!("past-{later}");
        let made = create(&app, json!({
            "title":format!("already {later}"), "status":later, "session":lane
        })).await;
        let id = made["id"].as_str().unwrap().to_string();
        let (_, _, body) = send_with(
            &app, "POST", &format!("/api/board/{id}/status-update"),
            Some(json!({"text":"informational follow-up"})),
            &[("X-Amux-Worker", lane.as_str())],
        ).await;
        assert_eq!(body["claim_verdict"], json!("status_not_actionable"));
        let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
        assert_eq!(detail["status"], json!(later), "{later} moved backward");
    }

    for id in [&cross_id, &blocked_id, &dependent_id, &target_id] {
        let conn = store.read().unwrap();
        let (status, log): (String, String) = conn.query_row(
            "SELECT status, COALESCE(log,'') FROM issues WHERE id=?1", [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(status, "todo", "{id} bypassed its claim guard");
        assert!(log.contains("STATUS ("), "refused update was lost: {log}");
    }
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{triggered_id}"), None).await;
    assert_eq!(detail["status"], json!("backlog"), "fresh trigger was bypassed");
}

/// Claim and progress are one transaction: a failed progress write rolls back
/// the claim rather than leaving a partially-mutated card.
#[tokio::test]
async fn status_update_claim_rolls_back_when_its_log_write_fails() {
    let (app, store, _dir) = app_with_store();
    let made = create(&app, json!({
        "title":"atomic update", "status":"todo", "session":"atomic-lane"
    })).await;
    let id = made["id"].as_str().unwrap().to_string();
    let trigger_id = id.clone();
    store.write(move |conn| {
        conn.execute_batch(&format!(
            "CREATE TRIGGER reject_status_progress BEFORE UPDATE OF log ON issues \
             WHEN NEW.id='{trigger_id}' AND NEW.log LIKE '%ROLLBACK-SPECIMEN%' \
             BEGIN SELECT RAISE(ABORT, 'forced status progress failure'); END;"
        ))?;
        Ok(amux_server::db::WriteOutcome { applied: true, events: vec![] })
    }).unwrap();
    let (st, _, _) = send_with(
        &app, "POST", &format!("/api/board/{id}/status-update"),
        Some(json!({"text":"ROLLBACK-SPECIMEN"})),
        &[("X-Amux-Worker", "atomic-lane")],
    ).await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    let conn = store.read().unwrap();
    let (status, log): (String, Option<String>) = conn.query_row(
        "SELECT status, log FROM issues WHERE id=?1", [&id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!(status, "todo");
    assert!(log.as_deref().unwrap_or("").is_empty(), "partial log: {log:?}");
}

/// Artifact identity is the `(task, artifact)` pair in the URL. A mismatched
/// task must never be able to mutate or delete another task's output, and a
/// deleted/missing artifact is a 404 rather than a 500 or a false 204 success.
#[tokio::test]
async fn artifact_crud_is_exact_and_missing_targets_fail_honestly() {
    let (app, _dir) = app();
    let first = create(&app, json!({"title": "artifact owner", "session": "amux"})).await;
    let second = create(&app, json!({"title": "other task", "session": "amux"})).await;
    let a = first["id"].as_str().unwrap();
    let b = second["id"].as_str().unwrap();

    let (st, _, body) = send(
        &app,
        "POST",
        &format!("/api/board/{a}/artifacts"),
        Some(json!({"kind": "implementation", "ref": "   "})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "a blank artifact cannot be clickable: {body}");

    let (st, _, made) = send(
        &app,
        "POST",
        &format!("/api/board/{a}/artifacts"),
        Some(json!({"kind": "doc", "ref": "result.md", "state": "created"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{made}");
    let aid = made["id"].as_str().unwrap();

    let (st, _, wrong_patch) = send(
        &app,
        "PATCH",
        &format!("/api/board/{b}/artifacts/{aid}"),
        Some(json!({"state": "submitted"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "cross-task PATCH must not become a storage 500: {wrong_patch}");

    let (st, _, invalid) = send(
        &app,
        "PATCH",
        &format!("/api/board/{a}/artifacts/{aid}"),
        Some(json!({"state": "teleported"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "an invalid state is a client error: {invalid}");

    let (st, _, wrong_delete) = send(&app, "DELETE", &format!("/api/board/{b}/artifacts/{aid}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a different task must not delete this artifact: {wrong_delete}");
    let (_, _, still_there) = send(&app, "GET", &format!("/api/board/{a}/artifacts"), None).await;
    assert_eq!(still_there.as_array().unwrap().len(), 1, "cross-task delete removed the artifact");
    assert_eq!(still_there[0]["state"], json!("created"), "cross-task patch changed the artifact");

    let (st, _, _) = send(&app, "DELETE", &format!("/api/board/{a}/artifacts/{aid}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _, missing_delete) = send(&app, "DELETE", &format!("/api/board/{a}/artifacts/{aid}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "repeat delete must say the target is gone: {missing_delete}");
    let (st, _, missing_patch) = send(
        &app,
        "PATCH",
        &format!("/api/board/{a}/artifacts/{aid}"),
        Some(json!({"description": "too late"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "patching a deleted artifact must be 404: {missing_patch}");
}

/// Live controls from TUBES-2426/TUBES-2428: `.env` is a file even though its
/// basename begins with a dot, while `1.93M` is a quantity even though it has
/// a dot and an alphabetic suffix.
#[tokio::test]
async fn evidence_assets_keep_hidden_files_and_reject_decimal_measurements() {
    let (app, _dir) = app();
    let made = create(
        &app,
        json!({
            "title": "hidden evidence asset",
            "status": "doing",
            "type": "chore",
        }),
    )
    .await;
    let id = made["id"].as_str().unwrap();
    let (status, _, patched) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({
            "evidence": "validated customers/tubescience/.env across 1.93M rows",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "evidence patch failed: {patched}");

    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    let refs = detail["asset_links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|asset| asset["ref"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(refs, vec!["customers/tubescience/.env"], "parser regressed: {detail}");
}

// AF-476. `trigger` is a CLI FLAG, not an API field. A raw PATCH of
// {"trigger": ...} writes nothing and answers 422 all_ignored — correctly, since
// no key sent was writable. What it did not say was what to send instead.
//
// Measured by the 2026-09-04 log sweep: 226 such PATCHes from `backend` in 80
// seconds across ~220 distinct cards, every one incapable of doing anything.
#[tokio::test]
async fn an_ignored_trigger_key_names_the_fields_it_meant() {
    let (app, _dir) = app();
    let (_s, _h, c) = send_with(&app, "POST", "/api/board",
        Some(json!({"title": "park me", "status": "todo", "session": "amux"})),
        &[("X-Amux-Session", "amux")]).await;
    let id = c["id"].as_str().unwrap().to_string();

    let (st, _h, b) = send_with(&app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({"trigger": "the next ts-engine roll"})),
        &[("X-Amux-Session", "amux")]).await;

    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "nothing sent was writable: {b}");
    assert_eq!(b["ignored_fields"][0], "trigger");
    let h = &b["ignored_hints"][0];
    assert_eq!(h["sent"], "trigger", "the hint must name the key sent: {b}");
    assert_eq!(h["meant"][0], "source_ref");
    assert_eq!(h["meant"][1], "last_verified_at",
        "both fields, or the caller lands in AF-469's re-draining state: {b}");
    assert!(h["how"].as_str().unwrap_or("").contains("--trigger"),
        "and it must name the CLI verb that writes both: {b}");

    // THE OTHER ARM: an ordinary unwritable key gets no hint. Without this the
    // test passes on an implementation that attaches the trigger hint to
    // everything, which would be worse than silence.
    let (st2, _h, b2) = send_with(&app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({"nonsense_key": 1})), &[("X-Amux-Session", "amux")]).await;
    assert_eq!(st2, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(b2.get("ignored_hints").is_none(),
        "a key with no known equivalent must not invent one: {b2}");
}

// `source_ref` has two owners. autofix stores its fault signature there and
// `open_card_for_fault` reads it to suppress a duplicate filing; `amux board
// backlog --trigger` writes the external condition a parked card waits on, as
// a plain overwrite.
//
// So parking an autofix card exactly as the board's own idle nudge prescribes
// destroyed the dedupe key. Measured 2026-08-24: AMUX-3651 parked with a
// trigger, AMUX-3685 filed minutes later for the same fault. The sanctioned
// instruction is what caused it.
//
// Driven through the REAL router and a real PATCH, not a pure helper, because
// the decision lives in the handler and a helper test would pin the property
// one layer above where the defect enters.
#[tokio::test]
async fn a_trigger_cannot_overwrite_an_autofix_signature_but_can_replace_a_trigger() {
    let (app, _dir) = app();

    let (_s, _h, created) = send_with(
        &app,
        "POST",
        "/api/board",
        Some(json!({"title": "autofix card", "status": "todo", "session": "amux"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();

    // autofix stamps its signature, exactly as file_finding does.
    let sig = "autofix:latency|outlier|ROLLUP|/api/board,/api/sessions";
    let (st, _h, _b) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"source_ref": sig})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success(), "autofix must be able to stamp its signature");

    // THE SPECIMEN: park it with a trigger, as the idle nudge prescribes.
    let trigger = "560d40ba adopted by the running server";
    let (st, _h, body) = send_with(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({"status": "backlog", "source_ref": trigger, "gate_ack": true})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    assert!(st.is_success(), "parking must still succeed: {body}");

    // 0. THE CALLER IS TOLD (AMUX-3791). Everything below asserts the value was
    //    preserved somewhere; none of it reaches the operator who ran the
    //    command. This is the one write where the field you NAMED is
    //    deliberately not the field that changed, so a caller verifying the
    //    obvious way reads back source_ref, sees the old value, and concludes
    //    the trigger was silently dropped. That false negative cost a probe
    //    against a live card and nearly a bug report against this very code.
    let dv = &body["diverted_fields"][0];
    assert_eq!(
        dv["field"].as_str().unwrap_or(""),
        "source_ref",
        "the response must name the field the caller asked for: {body}"
    );
    assert_eq!(
        dv["landed_in"].as_str().unwrap_or(""),
        "desc",
        "and where it actually went: {body}"
    );
    assert_eq!(
        dv["value"].as_str().unwrap_or(""),
        trigger,
        "and the value, so the caller can confirm it without re-reading the card: {body}"
    );
    assert!(
        dv["why"].as_str().unwrap_or("").contains("AMUX-3686"),
        "and WHY, or the next reader re-derives the dedupe-signature reasoning: {body}"
    );

    let (_s, _h, card) = send_with(&app, "GET", &format!("/api/board/{id}"), None, &[]).await;
    // 1. The dedupe key SURVIVES.
    assert_eq!(
        card["source_ref"].as_str().unwrap_or(""),
        sig,
        "a trigger must not overwrite an autofix signature"
    );
    // 2. And the card is still PARKED — the property --trigger exists for. A
    //    fix that only preserved the dedupe would break this, which is why both
    //    are asserted.
    assert_eq!(card["status"].as_str().unwrap_or(""), "backlog");
    // 3. The condition is not LOST: it goes where a human reads it.
    assert!(
        card["desc"].as_str().unwrap_or("").contains(trigger),
        "the parked-on condition must be recorded, not dropped: {}",
        card["desc"]
    );

    // CONTROL: a trigger replacing another TRIGGER is normal and must still
    // work — only `autofix:` is protected. Narrowing this wrongly would freeze
    // every parked card's condition at its first value.
    let (_s, _h, plain) = send_with(
        &app,
        "POST",
        "/api/board",
        Some(json!({"title": "hand-parked", "status": "todo", "session": "amux"})),
        &[("X-Amux-Session", "amux")],
    )
    .await;
    let pid = plain["id"].as_str().unwrap().to_string();
    for cond in ["waiting on vendor", "waiting on the deploy"] {
        let (st, _h, pb) = send_with(
            &app,
            "PATCH",
            &format!("/api/board/{pid}"),
            Some(json!({"source_ref": cond})),
            &[("X-Amux-Session", "amux")],
        )
        .await;
        assert!(st.is_success());
        // CONTROL for the diverted_fields assertions above: an ORDINARY trigger
        // write went exactly where the caller asked, so there is nothing to
        // announce. Without this, a version that emitted the advisory on every
        // source_ref write would pass every cell above and train readers to
        // ignore a line that cries wolf.
        assert!(
            pb.get("diverted_fields").is_none(),
            "a trigger that landed in source_ref must NOT report a diversion: {pb}"
        );
    }
    let (_s, _h, pcard) = send_with(&app, "GET", &format!("/api/board/{pid}"), None, &[]).await;
    assert_eq!(
        pcard["source_ref"].as_str().unwrap_or(""),
        "waiting on the deploy",
        "a trigger replacing a trigger must still work"
    );
}

/// tsukimiya, reviewing #134: "a PATCH of item_type returns 200 with a bumped rev
/// and silently does nothing — which cost you six mistyped cards in one night."
///
/// The rev-bump and the silence were already fixed. The STATUS CODE was not: a
/// caller checking `r.ok` still read success, which is AC-227's trap (`d.get('ok',
/// True)` defaulting True is how a refused write got reported as done).
///
/// Two of these three cells must STAY 200 — a 422 on every no-op is the opposite
/// bug and would break every caller that PATCHes idempotently.
#[tokio::test]
async fn a_patch_whose_every_key_is_unwritable_is_422_and_a_real_noop_is_not() {
    let (app, _dir) = app();
    let card = create(&app, json!({ "title": "typo probe", "type": "chore" })).await;
    let id = card["id"].as_str().unwrap().to_string();

    // 1) EVERY key unwritable -> 422. `item_type` is the classic typo for `type`.
    let (st, _, body) = send(
        &app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({"item_type": "code"})),
    ).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "nothing sent was writable: {body}");
    assert_eq!(body["applied"], json!(false));
    assert_eq!(body["ignored_fields"], json!(["item_type"]));
    assert_eq!(body["type"], json!("chore"), "and the card is untouched");

    // 2) CONTROL — an ordinary no-op is a SUCCESSFUL request that changed nothing.
    let (st, _, body) = send(
        &app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({"type": "chore"})),
    ).await;
    assert_eq!(st, StatusCode::OK, "a writable field at its current value is not an error: {body}");
    assert_eq!(body["applied"], json!(false));

    // 3) CONTROL — a MIXED body must not 422. `type` is legitimate and merely
    // unchanged; the typo is reported, not fatal. Without this cell the narrowing
    // is untested and "all keys ignored" could quietly become "any key ignored".
    let (st, _, body) = send(
        &app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({"type": "chore", "item_type": "code"})),
    ).await;
    assert_eq!(st, StatusCode::OK, "one legitimate key means the request was usable: {body}");
    assert_eq!(body["ignored_fields"], json!(["item_type"]), "still reported");
}

/// AMUX-3715: `?count=1` agrees with the list it describes, and refuses to look
/// like one.
///
/// Requested by tubescience: they archive in bulk (81 cards in one session) and
/// lost the ability to see that group, because `amux board archive` sets
/// archived=1 and leaves `status` alone, so archived work scatters under its old
/// status with nowhere to review it.
///
/// The count exists because the dashboard's "Archived (N)" header is collapsed
/// by default and the archived set was MEASURED at 445KB raw / 87KB gzipped —
/// +38% on the board poll — which is too much to ship on every load of a
/// mobile-first dashboard to render a number.
///
/// THE PROPERTY THAT MATTERS is not the number, it is that the count and the
/// list share a predicate. A count from its own SELECT would drift the moment
/// either side changed, and the header would then confidently disagree with
/// what expanding it shows (ethos rule 1). So the cells assert the two against
/// EACH OTHER rather than against literals — a literal still passes if both
/// drift the same way — and a final pair pins the values so a constant cannot
/// satisfy the agreement cells.
#[tokio::test]
async fn count_agrees_with_the_list_and_is_not_shaped_like_one() {
    let (app, _dir) = app();
    // ARCHIVE VIA PATCH, not via create. The first draft passed `archived: 1`
    // in the create body; POST /api/board ignores it, so nothing was archived
    // and BOTH sides of every agreement cell were 0 — they passed on an empty
    // set. The discriminating cells at the bottom are what caught it, which is
    // the entire reason they are there.
    for (i, arch) in [(1, 0), (2, 0), (3, 1), (4, 1), (5, 1)] {
        let c = create(&app, json!({"title": format!("c{i}"), "session": "lane"})).await;
        if arch == 1 {
            let id = c["id"].as_str().expect("created id");
            let (st, _, v) =
                send(&app, "PATCH", &format!("/api/board/{id}"), Some(json!({"archived": 1}))).await;
            assert_eq!(st, StatusCode::OK, "archive c{i} failed: {v}");
        }
    }
    // PREMISE, asserted: the seeding actually archived something. Without this
    // an empty board satisfies every "count equals list length" cell below.
    let (_, _, seeded) = send(&app, "GET", "/api/board?archived=1", None).await;
    assert_eq!(
        seeded.as_array().map(Vec::len),
        Some(3),
        "premise: 3 cards must actually be archived, or the agreement cells compare 0 to 0"
    );

    for q in ["archived=1", "archived=0", "session=lane"] {
        let (_, _, list) = send(&app, "GET", &format!("/api/board?{q}"), None).await;
        let n_list = list.as_array().expect("the list is a bare array").len();
        let (_, _, cnt) = send(&app, "GET", &format!("/api/board?{q}&count=1"), None).await;
        assert_eq!(
            cnt["count"].as_u64().expect("count is a number") as usize,
            n_list,
            "{q}: the count must equal the length of the list it describes: {cnt}"
        );
    }

    // SHAPE. An object, not an array: a caller that drops `count=1` from its URL
    // should get a shape error rather than a plausible one-element array, and
    // `.length` on the count body must be undefined rather than 1.
    let (_, _, cnt) = send(&app, "GET", "/api/board?archived=1&count=1", None).await;
    assert!(cnt.is_object(), "count must not be array-shaped: {cnt}");
    assert!(cnt.get("filter").is_some(), "it must echo the filter it counted: {cnt}");

    // AND IT MUST DISCRIMINATE, or a constant would pass every agreement cell
    // above.
    let (_, _, a1) = send(&app, "GET", "/api/board?archived=1&count=1", None).await;
    let (_, _, a0) = send(&app, "GET", "/api/board?archived=0&count=1", None).await;
    assert_eq!(a1["count"], json!(3), "3 archived were seeded: {a1}");
    assert_eq!(a0["count"], json!(2), "2 active were seeded: {a0}");
}

/// AMUX-3715: archiving a card that carries a live trigger says so, and one
/// without a trigger does not.
///
/// tubescience's finding, and it is about the primitive rather than the view
/// they asked for: `archived` hides a card from every board view AND every
/// autonomy loop, so a `--trigger` in `source_ref` will never fire again. The
/// card reads as parked-on-a-condition and is parked forever.
///
/// Measured when they reported it: 202 archived cards fleet-wide carry a
/// non-empty source_ref across at least six lanes. Not one will fire.
///
/// RECORDED, NOT REFUSED — they archive trigger-bearing cards deliberately and
/// keep the conditions in committed docs, so refusing would break a working
/// flow to protect them from a deliberate choice. The log line outlives the
/// response, which is what a future lane actually reads.
///
/// The negative cell is the one that keeps this useful: warning on EVERY
/// archive would pass the first cell and train everyone to ignore the line.
#[tokio::test]
async fn archiving_a_trigger_bearing_card_records_that_it_de_arms_it() {
    let (app, _dir) = app();

    let armed = create(&app, json!({"title": "armed", "session": "lane"})).await;
    let armed_id = armed["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{armed_id}"),
        Some(json!({"source_ref": "when the content_hash deploy lands"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "set trigger: {v}");
    // PREMISE: the trigger actually stuck. Without this the cell below passes
    // for a card that never had one.
    assert_eq!(v["source_ref"], json!("when the content_hash deploy lands"), "{v}");

    let (st, _, v) =
        send(&app, "PATCH", &format!("/api/board/{armed_id}"), Some(json!({"archived": 1}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let log = v["log"].as_str().unwrap_or_default();
    assert!(log.contains("DE-ARMS"), "the log must say archiving de-arms it: {log}");
    assert!(
        log.contains("when the content_hash deploy lands"),
        "and must quote the trigger, so a future reader knows WHAT stopped firing: {log}"
    );

    // THE NEGATIVE: no trigger, no warning. A line on every archive is a line
    // nobody reads.
    let plain = create(&app, json!({"title": "plain", "session": "lane"})).await;
    let plain_id = plain["id"].as_str().unwrap();
    let (_, _, pv) =
        send(&app, "PATCH", &format!("/api/board/{plain_id}"), Some(json!({"archived": 1}))).await;
    let plog = pv["log"].as_str().unwrap_or_default();
    assert!(plog.contains("ARCHIVED"), "it still records the archive: {plog}");
    assert!(!plog.contains("DE-ARMS"), "but must not warn about a trigger it does not have: {plog}");
}

/// `backlog` and `needsyou` were the only two statuses in the vocabulary with
/// NO gate and no exit any automated loop could produce, and fleet-wide on
/// 2026-08-29 they held 963 of 1029 open cards against 64 in the one status
/// the drive loop dispatches. Zero of 589 backlog cards carried a due date.
///
/// A card entering either now leaves with a revisit date, so the drive loop's
/// due arm can hand it back rather than a human triaging 219 cards at once.
#[tokio::test]
async fn entering_an_undrained_status_stamps_a_revisit_date() {
    let (app, _tmp) = app();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    for (status, days) in [("backlog", 14i64), ("needsyou", 3)] {
        let c = create(&app, json!({ "title": format!("park me to {status}") })).await;
        let id = c["id"].as_str().unwrap().to_string();
        // A typed ask, because this test is about the REVISIT DATE and not the
        // AF-318 ask gate — the same reason these tests already carry an asset
        // link and evidence. The gate has its own coverage below.
        let mut body = json!({ "status": status, "gate_ack": true });
        if status == "needsyou" {
            body["ask_actor"] = json!("Ethan");
            body["ask_type"] = json!("decision");
            body["ask_question"] = json!("Which retention window should this use?");
            body["ask_unblocks"] = json!("a number of days from the owner");
        }
        let (st, _, v) = send(&app, "PATCH", &format!("/api/board/{id}"), Some(body)).await;
        assert_eq!(st, StatusCode::OK, "{status} transition refused: {v}");

        let (_, _, got) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
        let due = got["due"].as_str().unwrap_or("");
        let want = (chrono::Local::now() + chrono::Duration::days(days))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(due, want, "{status} must be stamped {days}d out, got {due:?}");
        // NOT already due, or the drain promotes it straight back on the next
        // tick and the park never happened.
        assert!(due > today.as_str(), "{status} date {due} must be in the future");
        // The stamp is on the card's own history, not only in a response the
        // caller may never read (Ethan: the card is the source of truth for
        // its own history).
        let log = got["log"].as_str().unwrap_or("");
        assert!(log.contains(&format!("revisit {due}")), "log must name the date: {log}");
    }
}

/// The stamp fills a blank; it never overwrites a date a human chose. This is
/// the clause that keeps the default from being a decision taken away from the
/// owner (ethos rule 8) — and it is the one that silently reverses if the
/// stamp is ever moved above `set_opt`.
#[tokio::test]
async fn a_caller_supplied_revisit_date_survives_the_default() {
    let (app, _tmp) = app();
    let c = create(&app, json!({ "title": "i know when to look at this" })).await;
    let id = c["id"].as_str().unwrap().to_string();

    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "backlog", "due": "2027-01-15", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let (_, _, got) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(got["due"].as_str(), Some("2027-01-15"), "the caller's date must win");
}

/// Every status with a next actor must be left alone. A `doing` card carrying
/// an auto-stamped due date would read as a deadline nobody set, and `done`
/// would acquire one after the work finished.
#[tokio::test]
async fn statuses_that_have_a_next_actor_are_not_stamped() {
    let (app, _tmp) = app();
    for status in ["doing", "review", "blocked"] {
        let c = create(&app, json!({ "title": format!("move to {status}") })).await;
        let id = c["id"].as_str().unwrap().to_string();
        // `blocked` carries a named watch, because this test is about the DUE
        // STAMP and not the AF-317 watch gate — same reason these tests already
        // carry an asset link, evidence and a typed ask. The gate has its own
        // coverage in `blocked_refuses_a_card_that_names_no_watch`.
        let mut body = json!({ "status": status, "gate_ack": true });
        if status == "blocked" {
            let dep = create(&app, json!({ "title": "the thing that must land first" })).await;
            body["depends_on"] = json!([dep["id"].as_str().unwrap()]);
        }
        let (st, _, v) = send(&app, "PATCH", &format!("/api/board/{id}"), Some(body)).await;
        assert_eq!(st, StatusCode::OK, "{status}: {v}");
        let (_, _, got) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
        let due = got["due"].as_str().unwrap_or("");
        assert!(due.is_empty(), "{status} must not be stamped, got due={due:?}");
    }
}

/// `verified` is the board's highest claim and its default code gate is four
/// INDEPENDENT assertions. `gate_ack: true` asserted all four with one bit and
/// recorded which of them the acker looked at nowhere. Fleet-wide that was 302
/// of 1605 verifications (18.8%); the other 81% already enumerate.
#[tokio::test]
async fn verified_refuses_a_blanket_ack_and_takes_the_enumeration() {
    let (app, _tmp) = app();
    let c = create(&app, json!({ "title": "shipped it", "type": "code" })).await;
    let id = c["id"].as_str().unwrap().to_string();

    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "verified", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "blanket ack must be refused: {v}");
    assert_eq!(v["code"], "verified_requires_gate_checked");

    // The refusal must carry the criteria back. The `amux` CLI's refusal
    // printer keys on "gate" appearing in the error text and echoes
    // `d["gate"]` as a ready `--checked ...` line — without BOTH, the operator
    // is pushed to a hand-rolled PATCH, which is the unattributed write this
    // whole gate system depends on not happening (AMUX-2325).
    assert!(
        v["error"].as_str().unwrap_or("").contains("gate"),
        "CLI remedy printer keys on this: {}", v["error"]
    );
    let gate: Vec<String> = v["gate"]
        .as_array()
        .expect("refusal must return the gate")
        .iter()
        .map(|g| g.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(gate.len() > 1, "the multi-criterion case is the one refused: {gate:?}");

    // Enumerating exactly what came back is accepted — the honest path is the
    // one the refusal just printed.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "verified", "gate_checked": gate })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "enumerated ack must be accepted: {v}");
}

/// Both narrowings, which exist so the check cannot fire where it would only
/// be ceremony. If either regresses the gate starts refusing transitions that
/// carry no extra information, and the pressure to `--force` goes up.
#[tokio::test]
async fn the_blanket_ack_refusal_is_narrow_by_design() {
    let (app, _tmp) = app();

    // 1. A ONE-criterion verified gate (the non-code default, "Outcome
    //    confirmed to still hold"): blanket-acking it is byte-identical to
    //    checking it, so it is allowed.
    let c = create(&app, json!({ "title": "looked again", "type": "investigation" })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "verified", "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "single-criterion gate must still take an ack: {v}");

    // 2. `done` is deliberately excluded: it already carries a machine-checked
    //    asset link that no ack can fake, so it needs no second mechanism.
    let c = create(&app, json!({ "title": "landed it", "type": "code", "desc": "landed in crates/amux-server/src/api/board.rs" })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV, "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "done must still take a blanket ack: {v}");
}

/// A gate you can only learn by tripping it is the AF-112 shape. Both global
/// machine-checked constraints must be readable from the contract, and the
/// contract's claim about each must match what the handler actually does.
#[tokio::test]
async fn the_contract_publishes_every_machine_checked_constraint() {
    let (app, _tmp) = app();
    let (st, _, v) = send(&app, "GET", "/api/board/contract", None).await;
    assert_eq!(st, StatusCode::OK);
    // AF-321 added the third. A totalizing name ("both", "every") has to be
    // tested at the widest scope the mechanism touches — ethos rule 7 — so this
    // list is the list, and adding a constraint without adding it here is the
    // failure the rename is guarding against.
    for key in
        ["done_requires_asset_link", "done_requires_evidence", "verified_requires_gate_checked"]
    {
        assert!(v[key].is_object(), "contract must publish {key}: {v}");
        assert!(
            v[key]["enforced"].as_str().unwrap_or("").contains("force bypasses"),
            "{key} must say force bypasses it — the audited exit is part of the contract"
        );
    }
    // The contract's `code` must be the one the refusal actually returns, or a
    // caller keying on it reads the docs and matches nothing.
    let c = create(&app, json!({ "title": "x", "type": "code" })).await;
    let id = c["id"].as_str().unwrap();
    let (_, _, refused) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "verified", "gate_ack": true })),
    )
    .await;
    assert_eq!(refused["code"], "verified_requires_gate_checked");
}

// ---- full export (AMUX-3868) ---------------------------------------------

/// The export carries COMPLETE descriptions, which is the only reason this
/// endpoint exists.
///
/// The contrast is the test: `GET /api/board` deliberately omits `desc` and
/// sends `desc_head`/`desc_len` instead (AMUX-3861), so the dashboard's own
/// client-side export physically cannot include full text. If a future change
/// made the list carry `desc` again this test would still pass on the export
/// half — so it asserts the LIST omission too, and the day that stops being
/// true someone should be told, because this endpoint's justification changed.
#[tokio::test]
async fn export_carries_full_descriptions_where_the_list_deliberately_does_not() {
    let (app, _dir) = app();
    // Long enough that any head-truncation shows up as a real difference.
    let long = "D".repeat(4000);
    create(
        &app,
        json!({ "title": "big card", "session": "alpha", "desc": long.clone() }),
    )
    .await;

    let (_, _, list) = send(&app, "GET", "/api/board", None).await;
    let row = &list.as_array().expect("array")[0];
    assert!(
        row.get("desc").is_none(),
        "the LIST must keep omitting desc — if it does not, this endpoint's reason for \
         existing has changed and the docs on both sides are now wrong: {row}"
    );
    assert_eq!(row["desc_len"], json!(4000), "the list reports the true length");

    let (st, h, ex) = send(&app, "GET", "/api/board/export?format=json", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ex["issues"][0]["desc"], json!(long), "export must carry the WHOLE desc");
    assert_eq!(ex["count"], json!(1));
    assert_eq!(ex["scoped"], json!(false), "no filters were given");
    assert!(
        hdr(&h, "content-disposition").contains("attachment; filename=\"amux-board-"),
        "a download needs a filename: {}",
        hdr(&h, "content-disposition")
    );
}

/// A SCOPED export says so, and an unscoped one says that instead of staying
/// quiet. Silence would make the two indistinguishable, which is the whole
/// failure mode (ethos rule 4).
#[tokio::test]
async fn a_scoped_export_declares_its_scope_and_an_unscoped_one_declares_that() {
    let (app, _dir) = app();
    create(&app, json!({ "title": "a1", "session": "alpha", "desc": "AAA" })).await;
    create(&app, json!({ "title": "b1", "session": "beta", "desc": "BBB" })).await;

    let (_, _, all) = send(&app, "GET", "/api/board/export?format=json", None).await;
    assert_eq!(all["count"], json!(2));
    assert_eq!(all["scoped"], json!(false));

    let (_, _, one) = send(&app, "GET", "/api/board/export?format=json&worker=alpha", None).await;
    assert_eq!(one["count"], json!(1), "worker filter must actually filter");
    assert_eq!(one["scoped"], json!(true));
    assert!(
        one["scope"].as_array().expect("scope array").iter().any(|s| s
            .as_str()
            .unwrap_or_default()
            .contains("alpha")),
        "the scope must NAME the filter, not merely flag that one ran: {}",
        one["scope"]
    );

    // Markdown: the two headers must be different TEXT, not present-vs-absent.
    let (_, _, md_all) = send(&app, "GET", "/api/board/export?format=md", None).await;
    let (_, _, md_one) =
        send(&app, "GET", "/api/board/export?format=md&worker=alpha", None).await;
    let a = md_all.as_str().expect("md is text");
    let b = md_one.as_str().expect("md is text");
    assert!(a.contains("Whole board"), "unscoped md must say so: {}", &a[..a.len().min(200)]);
    assert!(b.contains("Scoped export"), "scoped md must say so: {}", &b[..b.len().min(200)]);
    assert!(b.contains("alpha"), "scoped md must name the worker");
    assert!(b.contains("AAA"), "scoped md must carry the full desc");
    assert!(!b.contains("BBB"), "scoped md must not leak the other worker's card");
}

/// AF-323: a `decision` card can be FILED and CLOSED, through the real HTTP
/// path, with a gate that does not demand a merge.
///
/// The friction this pins is that two components disagreed about one fact: the
/// board stored `type: decision` on five live cards while the create path
/// refused the word. The disagreement was load-bearing rather than cosmetic —
/// an unlisted type falls through `core_item_type` to `Code`, the STRICTEST
/// gate, so those five cards demanded "Implemented and merged" for work whose
/// entire output is a sentence from the person who owns the call. No owner
/// could close one honestly, which is ethos rule 3: a constraint with no
/// truthful path in a legitimate state.
///
/// This is deliberately an END-TO-END test and not a `default_gates_for` unit
/// test. AF-342 shipped four passing cells over a correct pure function while
/// the production caller read the wrong input, so the fix was inert on every
/// path in the fleet and every instrument still said pass. The gate arm being
/// right is not the claim; the claim is that a lane can do this.
#[tokio::test]
async fn a_decision_card_can_be_filed_and_closed_without_claiming_a_merge() {
    let (app, _dir) = app();

    // 1. THE CREATE PATH ACCEPTS IT. This is the arm that returned
    //    `unknown type "decision"` while the board held cards typed exactly this.
    let card = create(
        &app,
        json!({
            "title": "Which region for the new shard?",
            "session": "worker-1",
            "status": "doing",
            "type": "decision",
        }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();
    assert_eq!(card["type"], json!("decision"), "type must round-trip: {card}");

    // 2. THE GATE IS NOT THE CODE GATE. A refused transition reports the gate it
    //    enforced, so this reads the criteria the server actually applied rather
    //    than re-deriving them here — the two can differ, and when they do it is
    //    this one that governs.
    //
    //    The evidence is supplied on THIS call because the global asset-link
    //    constraint is checked BEFORE the type gate and short-circuits it: a bare
    //    `{"status":"done"}` comes back `done_requires_asset_link` with no `gate`
    //    key at all. Worth stating rather than just working around, because it
    //    means the first refusal a decision card hits says nothing about its type
    //    — the honest `none:` path is offered in `how_to_fix`, so there IS a way
    //    through, but a reader debugging one of the five live cards would see the
    //    artifact demand first and the mis-gating never.
    const DECIDED: &str =
        "none: Ethan chose us-east-1 on 2026-08-31; a decision ships no artifact";
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": DECIDED })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "expected the gate to ask first: {v}");
    let gate = v["gate"].as_array().unwrap_or_else(|| panic!("no gate key in refusal: {v}"));
    assert!(
        !gate.contains(&json!("Implemented and merged")),
        "a decision card must not be asked to claim a merge: {gate:?}"
    );
    assert!(
        gate.iter().any(|c| c.as_str().unwrap_or("").contains("by whom")),
        "the decision gate must ask WHO decided, which is the field that stops a \
         settled question being re-asked: {gate:?}"
    );

    // 3. AND IT CLOSES. The whole point is a truthful path to `done`, so a test
    //    that stopped at the refusal above would pin half the fix — the half a
    //    card stuck at the strictest gate already satisfied.
    //
    //    `none: <reason>` is the evidence: a decision produces no artifact, and
    //    that escape is documented, stored and counted rather than a bypass.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": DECIDED, "gate_ack": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a decision card must be closable: {v}");
    assert_eq!(v["status"], json!("done"));
}

/// The two hand-synced type vocabularies must not drift.
///
/// `KNOWN_TYPES` (amux-server, what the API validates against) and
/// `ItemType::ALL` (amux-core, what gates and the state machine derive from) are
/// separate lists kept in step BY HAND — `KNOWN_TYPES`' own doc says so and asks
/// for a future cleanup that derives one from the other. Until that exists, this
/// is the thing that fails when someone adds a type to one list only.
///
/// That failure is not cosmetic, which is why it is worth a test rather than a
/// comment: a type in `KNOWN_TYPES` but absent from `core_item_type` is ACCEPTED
/// by the API and then silently gated as `code`. That is precisely AF-323's
/// defect — a card whose stored type the gate layer does not believe in — and
/// adding `decision` by hand to two lists is exactly the move that would
/// reintroduce it one list at a time.
#[test]
fn every_known_type_survives_the_round_trip_into_the_gate_vocabulary() {
    use amux_server::db::board_store as bs;
    for t in bs::KNOWN_TYPES {
        let core = bs::core_item_type(t);
        assert_eq!(
            core.as_str(),
            t,
            "`{t}` is offered by the API but `core_item_type` does not map it, so \
             a card filed with it would be silently gated as `code` — AF-323's \
             exact shape, one list at a time"
        );
    }
    // And the reverse direction: a variant that exists in core but is not
    // offered by the API is a type nobody can file.
    for ty in amux_core::board::ItemType::ALL {
        assert!(
            bs::KNOWN_TYPES.contains(&ty.as_str()),
            "`{}` exists as an ItemType but the API will not accept it",
            ty.as_str()
        );
    }
}

/// AF-367: a card carries WHERE it came from, and the field survives to the API.
///
/// `creator` cannot answer that question. `mint_capture_card` stamps
/// `creator: "amux"` for every auto-captured human prompt and the amux LANE
/// stamps the same string for cards it authors, so 49 of 90 cards carrying that
/// value in one 24-hour window belonged to other lanes entirely. `owner_type`
/// does not split them either: it reads `agent` for both populations.
///
/// The fix is ADDITIVE on purpose. Stamping captures `amux-capture` is the
/// obvious alternative and it has a trap: `mint_capture_card`'s own dedup keys on
/// `creator = 'amux'`, so changing the value would silently stop deduping and
/// mint a card per prompt. That is this defect's own shape reappearing inside its
/// fix, which is why nothing about `creator` moved.
#[tokio::test]
async fn a_card_records_where_it_came_from_and_the_api_publishes_it() {
    let (app, _dir) = app();

    // The HTTP create path is `agent`: a real POST, from a lane or a human.
    let card = create(
        &app,
        json!({ "title": "an ordinary create", "session": "worker-1", "status": "todo" }),
    )
    .await;
    assert_eq!(
        card["source"],
        json!("agent"),
        "an API create must record itself as an agent create: {card}"
    );

    // AND IT SURVIVES THE READ. The insert and the SELECT are separate column
    // lists kept in step by hand, so a field written but not selected is a real
    // and silent outcome — the value would be in the database and absent from
    // every payload, which is worse than not storing it at all.
    let id = card["id"].as_str().unwrap();
    let (st, _, got) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        got["source"],
        json!("agent"),
        "source must survive the round-trip to the single-card GET: {got}"
    );

    // THE CONTROL, and it is the load-bearing one: the field must DISCRIMINATE.
    // A column hardcoded to "agent" everywhere would pass both assertions above
    // while answering nothing, which is exactly the state `creator` is already
    // in and the reason this card exists. So drive a second population through
    // the store directly and require a different value.
    let conn = rusqlite::Connection::open(_dir.path().join("amux-test.db")).unwrap();
    let row = amux_server::db::board_store::create_issue(
        &conn,
        &amux_server::db::board_store::NewIssue {
            title: "a captured human prompt".into(),
            desc: String::new(),
            status: "doing".into(),
            session: Some("worker-2".into()),
            item_type: "code".into(),
            creator: "amux".into(),
            owner_type: "agent".into(),
            due: None,
            due_time: None,
            reviewer: None,
            shepherd: None,
            gate: vec![],
            depends_on: vec![],
            tags: vec![],
            ask_type: None,
            ask_question: None,
            ask_unblocks: None,
            ask_actor: None,
            source: Some("capture".into()),
            requested_by: None,
            callback_session: None,
            callback_prompt: None,
        },
        1_788_000_000,
    )
    .unwrap();
    assert_eq!(row.source.as_deref(), Some("capture"));
    assert_eq!(
        row.creator, "amux",
        "the capture population still carries creator=amux — that is the collision, \
         and this fix deliberately does not move it"
    );

    // NULL IS AN HONEST ANSWER, not a default. Rows predating the column read
    // NULL, which means "unknown", and nothing is backfilled: guessing which
    // population an old row belonged to would manufacture the confident wrong
    // attribution the field exists to end.
    conn.execute("UPDATE issues SET source = NULL WHERE id = ?1", [&row.id])
        .unwrap();
    let (st, _, aged) = send(&app, "GET", &format!("/api/board/{}", row.id), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        aged["source"].is_null(),
        "a row with no source must publish null, not invent one: {aged}"
    );
}

/// AF-413: a refusal must SAY what it threw away, and the caller must be able to
/// tell "nothing else was lost" from "this server does not report losses".
///
/// The PATCH is atomic and stays that way — this asserts the reporting, not a
/// partial write. Measured three times in one session on 2026-09-02 across three
/// rejection reasons, one of which silently dropped a 4.2 KB card body.
#[tokio::test]
async fn a_refused_transition_names_the_fields_it_discarded() {
    let (app, _dir) = app();
    let card = create(
        &app,
        json!({ "title": "gated", "status": "doing", "desc": "artifact: crates/amux-server/src/api/board.rs" }),
    )
    .await;
    let id = card["id"].as_str().unwrap().to_string();

    // A desc riding along with a gated status: the specimen.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({
            "status": "done",
            "evidence": EV,
            "desc": "CANARY-BODY artifact: crates/amux-server/src/api/board.rs",
            "title": "renamed"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["error"], json!("gate not acknowledged"));
    assert_eq!(
        v["discarded"],
        json!(["desc", "evidence", "title"]),
        "the refusal must name the content fields it dropped: {v}"
    );
    assert!(
        v["discarded_note"].as_str().unwrap_or("").contains("NOT applied"),
        "a bare array is cryptic; say what happened: {v}"
    );

    // AND THE WRITE REALLY DID NOT HAPPEN — otherwise this test would pass just
    // as well against a server that applied the desc and reported it discarded.
    let (_, _, detail) = send(&app, "GET", &format!("/api/board/{id}"), None).await;
    assert_eq!(detail["title"], json!("gated"), "title must be unchanged: {detail}");
    assert!(
        !detail["desc"].as_str().unwrap_or("").contains("CANARY-BODY"),
        "desc must be unchanged: {detail}"
    );

    // THE FLEET'S COMMONEST LOSS, and the reason this matters beyond a card
    // body. `amux board done <ID> --evidence-stdin` sends {status, evidence}.
    // `evidence` is a writable field, so a gate refusal discards the evidence
    // the caller just composed — which is what happened twice on 2026-09-02,
    // on AF-410 and AF-416, each time re-sent by hand with --checked.
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["discarded"], json!(["evidence"]), "{v}");

    // CONTROL: a body with NOTHING but the status discards nothing surprising,
    // and the key is STILL present. Its absence would mean "this server does not
    // compute losses", which a caller cannot distinguish from "nothing was lost"
    // if it were omitted when empty (ethos rule 4).
    let (st, _, v) = send(
        &app,
        "PATCH",
        &format!("/api/board/{id}"),
        Some(json!({ "status": "done" })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(v["discarded"], json!([]), "empty, but PRESENT: {v}");
    assert!(v.get("discarded_note").is_none(), "no note when nothing was dropped: {v}");
}

/// AF-424: a card whose stated deadline has ARRIVED outranks an older card
/// nobody dated.
///
/// Reported by gtm-engine (GE-769). The queue ranked by `age_days *
/// (blast_radius + 1)`, and the bias was structural rather than incidental:
/// stating a deadline correlates with being NEW, new means low age_days, and
/// the formula rewards age — so dating a card pushed it DOWN. Measured live
/// 2026-09-02: 119 of 404 rows carried a due date at MEDIAN rank 329 of 404,
/// and 47 were due-today-or-overdue. "Scrub 646 leaked API-key occurrences",
/// due that day, ranked 349.
#[tokio::test]
async fn the_needsyou_queue_ranks_a_passed_deadline_above_an_older_undated_card() {
    let (app, _dir) = app();
    let ask = json!({
        "status": "needsyou", "ask_actor": "Ethan", "ask_type": "decision",
        "ask_question": "Should this move to the new cluster, or stay where it is?",
        "ask_unblocks": "the migration can be scheduled either way",
    });

    // An OLD card nobody dated. Created first, so it is strictly older.
    // NB: parking a card auto-stamps a near-future `due`, so this one is
    // dated-but-not-arrived rather than truly undated — which is the same
    // comparison and the commoner live shape (119 of 404 carry a date, mostly future).
    let old = create(&app, json!({ "title": "older, date not yet arrived" })).await;
    let old_id = old["id"].as_str().unwrap().to_string();
    let (st, _, why) = send(&app, "PATCH", &format!("/api/board/{old_id}"), Some(ask.clone())).await;
    assert_eq!(st, StatusCode::OK, "park refused: {why}");

    // A NEWER card with a deadline that has already arrived.
    let due = create(&app, json!({ "title": "new, due today" })).await;
    let due_id = due["id"].as_str().unwrap().to_string();
    let mut with_due = ask.as_object().unwrap().clone();
    with_due.insert("due".into(), json!("2000-01-01")); // long past
    let (st, _, _) =
        send(&app, "PATCH", &format!("/api/board/{due_id}"), Some(Value::Object(with_due))).await;
    assert_eq!(st, StatusCode::OK);

    let (_, _, v) = send(&app, "GET", "/api/board/needsyou?all=1", None).await;
    let q = v["queue"].as_array().unwrap();
    let pos = |id: &str| q.iter().position(|r| r["id"] == json!(id)).expect("card in queue");
    assert!(
        pos(&due_id) < pos(&old_id),
        "an ARRIVED deadline must outrank an older card whose date has not: {v}"
    );
    assert_eq!(v["queue"][pos(&due_id)]["overdue"], json!(true), "and say why: {v}");
    assert_eq!(v["queue"][pos(&old_id)]["overdue"], json!(false));
    assert_eq!(v["overdue"], json!(1), "the count is published: {v}");
    assert!(
        v["ranked_by"].as_str().unwrap().contains("overdue first"),
        "the rule is stated, not silent: {v}"
    );

    // CONTROL 1: a FUTURE deadline is not urgency. Without this, "any due date
    // wins" would pass — and 119 of 404 live cards carry one, mostly future.
    let far = create(&app, json!({ "title": "dated, far future" })).await;
    let far_id = far["id"].as_str().unwrap().to_string();
    let mut future = ask.as_object().unwrap().clone();
    future.insert("due".into(), json!("2099-12-31"));
    let (st, _, _) =
        send(&app, "PATCH", &format!("/api/board/{far_id}"), Some(Value::Object(future))).await;
    assert_eq!(st, StatusCode::OK);
    let (_, _, v) = send(&app, "GET", "/api/board/needsyou?all=1", None).await;
    let q = v["queue"].as_array().unwrap();
    let pos = |id: &str| q.iter().position(|r| r["id"] == json!(id)).expect("card in queue");
    assert!(
        pos(&old_id) < pos(&far_id),
        "a future deadline must NOT jump the queue; only an arrived one does: {v}"
    );
    assert_eq!(v["overdue"], json!(1), "still one overdue, not two: {v}");

    // CONTROL 2: the view names the population it does NOT consider, so a
    // reader comparing against a raw board dump cannot mistake archived cards
    // for live work the view dropped. That misreading is what this card came in as.
    assert!(v["archived_excluded"].is_number(), "excluded population is stated: {v}");
}

/// AF-506 — a gate refusal names the reassignment exit, not only the gate.
///
/// THE WIRING, not the wording. `reassign_exit`'s own cells read the STRINGS and
/// all of them stay green if `gate_409` never calls it — the same shape that let
/// two suites earlier today pass over a deleted call site. This drives a real
/// refusal through the real handler and reads the response.
///
/// Reported by `backend` on MI-4155: a lane holding a card that is not its work
/// had no honest state to move it to, and the refusal taught exactly one exit.
#[tokio::test]
async fn a_gate_refusal_offers_the_reassignment_exit_and_says_it_is_not_a_bypass() {
    let (app, _dir) = app();
    let card = create(&app, json!({
        "title": "a card whose work belongs elsewhere", "status": "doing",
        "desc": "artifact: crates/amux-server/src/api/board.rs",
        "session": "mvs-infra",
    })).await;
    let id = card["id"].as_str().unwrap().to_string();

    // A DIFFERENT lane trips the gate: the exit can name the owner.
    let (st, _, v) = send_with(
        &app, "PATCH", &format!("/api/board/{id}"),
        Some(json!({ "status": "done", "evidence": EV })),
        &[("X-Amux-Session", "backend")],
    ).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let ex = &v["or_reassign"];
    assert!(!ex.is_null(), "the refusal offers no reassignment exit at all: {v}");
    assert!(
        ex["how"].as_str().unwrap_or("").contains("mvs-infra"),
        "the exit does not name the owning lane it could see: {v}"
    );
    assert!(
        ex["not_a_bypass"].as_str().unwrap_or("").contains("does not skip the gate"),
        "the exit reads as a way around the gate: {v}"
    );
    // The gate itself is untouched: this is offered BESIDE the refusal.
    assert_eq!(v["kind"], json!("gate_blocked"), "{v}");
    assert!(v["how_to_ack"]["gate_ack"] == json!(true), "{v}");
}

/// AF-506, second pass — EVERY refusal that blocks closing or reviewing a card
/// offers the reassignment exit, not just the one it was written for.
///
/// Found by probing the LIVE server on the commit that added the exit to
/// `gate_409`: `done_requires_asset_link` fires first, then
/// `done_requires_evidence`, and only then the gate ack. A lane routing a card
/// away hits whichever comes first and never reaches the one that had been
/// taught. A fix verified only through the path it was written for would have
/// shipped looking complete — the live probe is what caught it.
///
/// SCOPE, deliberately: refusals that block CLOSING or REVIEWING. The needs-you,
/// blocked-must-name and todo-capacity refusals are excluded because supplying
/// the ask or the reason IS their exit; reassignment there would be noise.
#[tokio::test]
async fn every_close_refusal_offers_the_reassignment_exit() {
    let (app, _dir) = app();

    // Each tuple walks one card further along the ladder, so a DIFFERENT
    // refusal answers each time. The order is the order the server applies.
    let cases: Vec<(&str, Value, &str)> = vec![
        // No desc artifact at all -> the asset-link gate answers first.
        ("done_requires_asset_link", json!({ "status": "done" }), ""),
        // Artifact present, evidence missing -> the evidence gate answers.
        (
            "done_requires_evidence",
            json!({ "status": "done" }),
            "artifact: crates/amux-server/src/api/board.rs",
        ),
    ];

    for (want_code, patch, desc) in cases {
        let card = create(&app, json!({
            "title": format!("card for {want_code}"), "status": "doing",
            "desc": desc, "session": "mvs-infra",
        })).await;
        let id = card["id"].as_str().unwrap().to_string();
        let (st, _, v) = send_with(
            &app, "PATCH", &format!("/api/board/{id}"), Some(patch.clone()),
            &[("X-Amux-Session", "backend")],
        ).await;
        assert_eq!(st, StatusCode::CONFLICT, "{want_code}: {v}");
        assert_eq!(v["code"], json!(want_code), "wrong refusal answered: {v}");
        assert!(
            v["or_reassign"]["how"].as_str().unwrap_or("").contains("mvs-infra"),
            "{want_code} teaches only its own exit: {v}"
        );
        assert!(
            v["or_reassign"]["not_a_bypass"].as_str().unwrap_or("").contains("does not skip"),
            "{want_code}: the exit reads as a way around the refusal: {v}"
        );
    }
}
