//! /api/palette: the Cmd-K name index for cards and schedules.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<Store>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), store, dir)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn card(store: &Store, id: &str, title: &str, status: &str, updated: i64) {
    let (id, title, status) = (id.to_string(), title.to_string(), status.to_string());
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, session, creator, created, updated)
                 VALUES (?1, ?2, '', ?3, 'lane-a', 'tester', ?4, ?4)",
                rusqlite::params![id, title, status, updated],
            )?;
            Ok(amux_server::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
}

fn ids(v: &Value) -> Vec<String> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn names_match_and_a_new_card_appears_after_the_rebuild() {
    let (app, store, _d) = app();
    card(
        &store,
        "PAL-1",
        "Rotate the parsnip deploy key",
        "todo",
        1_790_000_000,
    );
    card(
        &store,
        "PAL-2",
        "Notes on the parsnip changelog",
        "done",
        1_780_000_000,
    );

    let (st, v) = get(&app, "/api/palette?q=parsnip").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["measured"], true);
    assert!(v["n_considered"].as_u64().unwrap() >= 2, "{v}");
    assert_eq!(
        ids(&v),
        vec!["PAL-1", "PAL-2"],
        "the open card ranks first: {v}"
    );

    let (_, v) = get(&app, "/api/palette?q=parsnip%20changelog").await;
    assert_eq!(ids(&v), vec!["PAL-2"], "every word must match: {v}");

    let (_, v) = get(&app, "/api/palette?q=").await;
    assert_eq!(
        v["total"], 0,
        "a blank query matches nothing, and says it looked: {v}"
    );
    assert_eq!(v["measured"], true);

    // A card written after the index was built: the next answer may be stale
    // (and says so), and a later one carries it.
    card(
        &store,
        "PAL-3",
        "Parsnip rollback plan",
        "todo",
        1_790_000_100,
    );
    let mut found = false;
    for _ in 0..100 {
        let (_, v) = get(&app, "/api/palette?q=rollback").await;
        if ids(&v) == vec!["PAL-3"] {
            found = true;
            break;
        }
        assert!(
            v["stale"] == true || v["fingerprint_age_ms"].as_u64().unwrap_or(0) > 0,
            "an answer without the new card must say it is stale or that its change check is old: {v}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(found, "the rebuilt index never carried PAL-3");
}

fn card_with(store: &Store, id: &str, title: &str, desc: &str, status: &str, archived: i64) {
    let (id, title, desc, status) = (
        id.to_string(),
        title.to_string(),
        desc.to_string(),
        status.to_string(),
    );
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, session, creator, created, updated, archived)
                 VALUES (?1, ?2, ?3, ?4, 'lane-a', 'tester', 1790000000, 1790000000, ?5)",
                rusqlite::params![id, title, desc, status, archived],
            )?;
            Ok(amux_server::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
}

// Archived and discarded cards stay out unless asked for, and the answer says
// how many it left out; a word found only in a description comes back as a
// body match with a snippet, below the title matches.
#[tokio::test(flavor = "multi_thread")]
async fn hidden_cards_are_counted_and_body_matches_carry_a_snippet() {
    let (app, store, _d) = app();
    card_with(&store, "HID-1", "Turnip rollout", "", "todo", 0);
    card_with(&store, "HID-2", "Turnip rollout old", "", "todo", 1);
    card_with(
        &store,
        "HID-3",
        "Turnip rollout dropped",
        "",
        "discarded",
        0,
    );
    card_with(
        &store,
        "HID-4",
        "Weekly cleanup",
        "first drain the turnip queue, then rotate keys",
        "todo",
        0,
    );

    let (st, v) = get(&app, "/api/palette?q=turnip").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(ids(&v), vec!["HID-1", "HID-4"], "{v}");
    assert_eq!(v["hidden"], 2, "{v}");
    assert_eq!(v["hits"][0]["match"], "prefix");
    assert_eq!(v["hits"][1]["match"], "body");
    assert!(
        v["hits"][1]["snippet"]
            .as_str()
            .unwrap()
            .contains("drain the turnip queue"),
        "{v}"
    );

    let (_, v) = get(&app, "/api/palette?q=turnip&hidden=1").await;
    assert_eq!(v["hidden"], 0);
    assert_eq!(v["with_hidden"], true);
    let got = ids(&v);
    assert_eq!(got.len(), 4, "{v}");
    assert_eq!(got[0], "HID-1", "the live card stays first: {v}");
}
