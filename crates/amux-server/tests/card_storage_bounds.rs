//! CW-860 incident regressions through real artifact and status-update handlers.
use amux_server::api::{router, AppState};
use amux_server::db::{board_store, replay, Store};
use axum::{body::Body, http::Request};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

fn rig() -> (axum::Router, Arc<Store>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("bounds.db")).unwrap());
    store.write(|conn| {
        conn.execute("INSERT INTO issues (id,title,status,session,created,updated) VALUES ('CW-1','storage test','doing','bounds-test',1,1)", [])?;
        Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
    }).unwrap();
    let app = router(AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });
    (app, store, dir)
}

async fn post(app: &axum::Router, path: &str, body: serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .header("x-amux-session", "bounds-test")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        status.is_success(),
        "{status}: {}",
        String::from_utf8_lossy(&bytes)
    );
}

#[tokio::test]
async fn thousand_artifacts_keep_full_registry_and_at_most_five_log_lines() {
    let (app, store, _dir) = rig();
    for i in 0..1000 {
        post(&app, "/api/board/CW-1/artifacts", json!({"kind":"implementation", "ref": format!("SchaffVault:/srv/datasets/finmtm/image{i}.png")})).await;
    }
    let conn = store.read().unwrap();
    let row = board_store::get_issue(&conn, "CW-1").unwrap().unwrap();
    assert!(row.log.unwrap().lines().count() <= 5);
    let count: u64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _amux_task_artifacts WHERE task_id='CW-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1000);
    let audit = replay::verify_replay(&conn).unwrap();
    assert_eq!(audit.divergences_total, 0, "{audit:?}");
}

#[tokio::test]
async fn real_log_append_does_not_journal_megabyte_log() {
    let (app, store, _dir) = rig();
    store
        .write(|conn| {
            conn.execute(
                "UPDATE issues SET log=?1 WHERE id='CW-1'",
                ["x".repeat(1024 * 1024)],
            )?;
            Ok(amux_server::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
    post(
        &app,
        "/api/board/CW-1/status-update",
        json!({"text":"CW-860 measured append"}),
    )
    .await;
    let conn = store.read().unwrap();
    let (bytes, payload): (u64, String) = conn.query_row("SELECT length(CAST(payload AS BLOB)),payload FROM _amux_state_events WHERE entity_type='task' ORDER BY id DESC LIMIT 1", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert!(bytes < 64 * 1024, "journal row was {bytes} bytes");
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert!(payload.get("log").is_none());
    let row = board_store::get_issue(&conn, "CW-1").unwrap().unwrap();
    assert!(row.log.as_ref().unwrap().len() <= 256 * 1024);
    assert!(row.log.unwrap().contains("CW-860 measured append"));
    assert_eq!(replay::verify_replay(&conn).unwrap().divergences_total, 0);
}

#[test]
fn pruned_empty_journal_requires_full_sync_for_behind_client() {
    let (_app, store, _dir) = rig();
    store
        .write(|conn| {
            conn.execute("UPDATE _amux_rev SET rev=123 WHERE id=1", [])?;
            Ok(amux_server::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
    assert!(
        store
            .events_since(amux_core::revision::StateRevision(122), 100)
            .unwrap()
            .1
    );
    assert!(
        !store
            .events_since(amux_core::revision::StateRevision(123), 100)
            .unwrap()
            .1
    );
}

#[test]
fn ten_thousand_appends_and_direct_save_keep_stored_log_bounded() {
    let (_app, store, _dir) = rig();
    let mut log = String::new();
    for i in 0..10_000 {
        log = board_store::append_log(Some(&log), "05:10", &format!("entry {i}"));
    }
    assert!(log.lines().count() <= 500);
    assert!(log.len() <= 256 * 1024);
    assert!(log.contains("older lines trimmed"));
    assert!(log.ends_with("entry 9999"));
    store
        .write(move |conn| {
            let mut row = board_store::get_issue(conn, "CW-1")?.unwrap();
            row.log = Some(format!("{}\n{log}", "old line\n".repeat(10_000)));
            board_store::save_patched(conn, &mut row)?;
            Ok(amux_server::db::WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .unwrap();
    let conn = store.read().unwrap();
    let row = board_store::get_issue(&conn, "CW-1").unwrap().unwrap();
    let log = row.log.unwrap();
    assert!(log.lines().count() <= 500);
    assert!(log.len() <= 256 * 1024);
    assert!(log.ends_with("entry 9999"));
}

#[test]
fn vacuum_binary_helper_uses_child_temp_setting_and_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let db = dir.path().join("vacuum.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE keep(value); INSERT INTO keep VALUES ('proof'); CREATE TABLE waste(value); INSERT INTO waste VALUES (zeroblob(1048576)); DROP TABLE waste;").unwrap();
    }
    let before = std::fs::metadata(&db).unwrap().len();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_amux-server"))
        .arg("--storage-vacuum-helper")
        .arg(&db)
        .env("SQLITE_TMPDIR", tmp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(std::fs::metadata(&db).unwrap().len() < before);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let value: String = conn
        .query_row("SELECT value FROM keep", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "proof");
}
