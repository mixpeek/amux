//! A chat worker survives a server restart (exec on every deploy).
//!
//! Measured 2026-09-24 in a click-only UI run: the server re-exec'd onto a new
//! build mid-turn. The turn's user message stayed recorded with no reply and no
//! error, and the message queued behind it vanished. This pins the recovery:
//! the interrupted turn is REPORTED (not re-run: its tools may already have
//! acted) and the queued message RUNS.
//!
//! The "previous process" is simulated by writing exactly what it leaves on
//! disk: the in-flight marker in meta and the persisted queue. Own process,
//! because AMUX_HOME is global.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_ARGS_LOG"
sid=""
while [ $# -gt 0 ]; do
  case "$1" in --session-id|--resume) sid="$2"; shift;; esac
  shift
done
input=$(cat)
python3 -c 'import json,sys; print(json.dumps({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"echo: "+sys.argv[1]}}}))' "$input"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"$sid\"}"
"#;

async fn history(app: &axum::Router) -> Value {
    let req = Request::builder().uri("/api/sessions/chatty/chat").body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn a_restart_reports_the_cut_off_turn_and_runs_the_queued_message() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    std::fs::create_dir_all(home.join("chat-state")).unwrap();
    let fake = tmp.path().join("fake-claude");
    std::fs::write(&fake, FAKE_CLAUDE).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let args_log = tmp.path().join("args.log");
    unsafe {
        std::env::set_var("AMUX_HOME", &home);
        std::env::set_var("AMUX_CHAT_CLAUDE_BIN", &fake);
        std::env::set_var("FAKE_ARGS_LOG", &args_log);
    }
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        home.join("sessions/chatty.env"),
        format!("CC_DIR=\"{}\"\nCC_WORKER_TYPE=\"chat\"\n", dir.display()),
    )
    .unwrap();
    // What the dead process left behind.
    std::fs::write(
        home.join("sessions/chatty.meta.json"),
        json!({"chat_running": true, "chat_inflight_turn": "TURN-CUT-OFF"}).to_string(),
    )
    .unwrap();
    std::fs::write(
        home.join("chat-state/chatty.queue.json"),
        json!([{"text": "queued before the restart", "origin": "owner"}]).to_string(),
    )
    .unwrap();

    let store = Arc::new(Store::open(&tmp.path().join("t.db")).unwrap());
    let state = AppState {
        store,
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = router(state.clone());

    let (workers, interrupted, resumed) = amux_server::api::chat_worker::recover_all(&state).await;
    assert_eq!((workers, interrupted, resumed), (1, 1, 1));

    let mut h = history(&app).await;
    for _ in 0..200 {
        let replies = h["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").count();
        if replies >= 2 && h["busy"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        h = history(&app).await;
    }
    let msgs = h["messages"].as_array().unwrap();
    let cut = msgs.iter().find(|m| m["turn_id"] == "TURN-CUT-OFF").expect("the cut-off turn is reported");
    assert_eq!(cut["role"], "assistant");
    assert!(cut["error"].as_str().unwrap().contains("restarted"), "{cut}");
    let answered = msgs.iter().any(|m| m["role"] == "assistant" && m["text"] == "echo: queued before the restart");
    assert!(answered, "the queued message runs after recovery: {msgs:?}");
    let log = std::fs::read_to_string(&args_log).unwrap();
    assert_eq!(log.lines().count(), 1, "exactly one provider turn: the cut-off one is NOT re-run");
    assert!(!home.join("chat-state/chatty.queue.json").exists(), "the drained queue is removed");

    // Idempotent: a second pass (another restart with nothing pending) does nothing.
    assert_eq!(amux_server::api::chat_worker::recover_all(&state).await, (1, 0, 0));
}
