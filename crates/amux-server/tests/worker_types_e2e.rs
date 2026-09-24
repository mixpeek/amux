//! ACW-10/11: one coding worker and one chat worker through the SAME generic
//! worker operations, through the real router. What may differ is only the
//! execution adapter and the output renderer.
//!
//! The chat worker runs real headless turns against a fake provider CLI
//! (`AMUX_CHAT_CLAUDE_BIN`) that speaks Claude's stream-json. The coding
//! worker is never started: starting it spawns tmux, which a test must not
//! do, and its execution path is the pre-existing one this change leaves
//! untouched. For it the test pins the dispatch seam instead: its adapter
//! hands every operation to the terminal pipeline.
//!
//! Own process, because AMUX_HOME is global.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_as(app, "", method, path, body).await
}

/// `as_worker` non-empty = the request comes from that worker's own session.
async fn call_as(
    app: &axum::Router,
    as_worker: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    if !as_worker.is_empty() {
        b = b.header("X-Amux-Session", as_worker);
    }
    let req = match body {
        Some(v) => b
            .header("Content-Type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn row<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from /api/sessions"))
}

const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_ARGS_LOG"
sid=""
while [ $# -gt 0 ]; do
  case "$1" in --session-id|--resume) sid="$2"; shift;; esac
  shift
done
input=$(cat)
echo "profile noise that is not json"
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$sid\",\"model\":\"fake-model\"}"
printf '%s\n' '{"type":"stream_event","event":{"type":"message_start"}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"echo: "}}}'
python3 -c 'import json,sys; print(json.dumps({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":sys.argv[1]}}}))' "$input"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"$sid\",\"total_cost_usd\":0}"
"#;

async fn wait_for_reply(app: &axum::Router, name: &str, n_assistant: usize) -> Value {
    for _ in 0..200 {
        let (_, h) = call(app, "GET", &format!("/api/sessions/{name}/chat"), None).await;
        let replies = h["messages"]
            .as_array()
            .map(|m| m.iter().filter(|x| x["role"] == "assistant").count())
            .unwrap_or(0);
        if replies >= n_assistant && h["busy"] == false {
            return h;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("chat worker {name} never produced {n_assistant} replies");
}

#[tokio::test]
async fn coding_and_chat_workers_share_every_generic_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("amux-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let fake = tmp.path().join("fake-claude");
    std::fs::write(&fake, FAKE_CLAUDE).unwrap();
    std::process::Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let args_log = tmp.path().join("args.log");
    unsafe {
        std::env::set_var("AMUX_HOME", &home);
        std::env::set_var("AMUX_CHAT_CLAUDE_BIN", &fake);
        std::env::set_var("FAKE_ARGS_LOG", &args_log);
    }
    let store = Arc::new(Store::open(&tmp.path().join("t.db")).unwrap());
    let app = router(AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    // --- the registry the create/edit UI renders from ---------------------
    let (st, types) = call(&app, "GET", "/api/worker-types", None).await;
    assert_eq!(st, StatusCode::OK);
    let ids: Vec<&str> = types["types"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["coding", "chat"]);
    assert_eq!(types["default"], "coding");

    // --- create: same endpoint, same fields, type is the only difference --
    let common = |name: &str| json!({"name": name, "tags": ["team-x"], "desc": "shared", "start": false});
    let mut coding = common("code-lane");
    coding["dir"] = json!(repo);
    let (st, created) = call(&app, "POST", "/api/sessions", Some(coding)).await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["worker_type"], "coding", "no worker_type = coding (old clients)");
    let mut chat = common("chat-lane");
    chat["worker_type"] = json!("chat");
    let (st, created) = call(&app, "POST", "/api/sessions", Some(chat)).await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["renderer"], "chat");

    // Old env files stay byte-compatible: coding writes no type key at all.
    let coding_env = std::fs::read_to_string(home.join("sessions/code-lane.env")).unwrap();
    assert!(!coding_env.contains("CC_WORKER_TYPE"), "{coding_env}");
    let chat_env = std::fs::read_to_string(home.join("sessions/chat-lane.env")).unwrap();
    assert!(chat_env.contains("CC_WORKER_TYPE=\"chat\""), "{chat_env}");
    assert!(chat_env.contains("/chat/chat-lane"), "chat needs no project dir: {chat_env}");

    // Type requirements are explicit and refused up front.
    let (st, e) = call(&app, "POST", "/api/sessions",
        Some(json!({"name":"bad","worker_type":"chat","worktree":true,"start":false}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(e["error"].as_str().unwrap().contains("worktree"), "{e}");
    let (st, e) = call(&app, "POST", "/api/sessions",
        Some(json!({"name":"bad","worker_type":"robot","start":false}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");

    // --- list/detail: both identify their type; shared fields identical ---
    let (_, list) = call(&app, "GET", "/api/sessions", None).await;
    let (c, h) = (row(&list, "code-lane"), row(&list, "chat-lane"));
    assert_eq!((c["worker_type"].as_str(), c["renderer"].as_str()), (Some("coding"), Some("terminal")));
    assert_eq!((h["worker_type"].as_str(), h["renderer"].as_str()), (Some("chat"), Some("chat")));
    for key in ["tags", "desc", "provider", "lifecycle", "archived"] {
        assert_eq!(c[key], h[key], "shared field {key} must not depend on type");
    }
    assert_eq!(c["running"], false);
    assert_eq!(h["running"], false);

    // --- board: the same card ops attribute to either worker ---------------
    for name in ["code-lane", "chat-lane"] {
        // Each worker files on its own board, as the board requires of any worker.
        let (st, card) = call_as(&app, name, "POST", "/api/board",
            Some(json!({"title": format!("shared task for {name}"), "status": "todo", "session": name}))).await;
        assert!(st.is_success(), "{name}: {st} {card}");
        let (_, cards) = call(&app, "GET", &format!("/api/board?session={name}"), None).await;
        assert!(cards.as_array().unwrap().iter().any(|x| x["session"] == name),
            "{name}'s card must be on its board");
    }

    // --- lifecycle: start -------------------------------------------------
    let (st, r) = call(&app, "POST", "/api/sessions/chat-lane/start", Some(json!({}))).await;
    assert!(st.is_success(), "{st} {r}");
    for _ in 0..100 {
        let (_, list) = call(&app, "GET", "/api/sessions", None).await;
        if row(&list, "chat-lane")["running"] == true { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, list) = call(&app, "GET", "/api/sessions", None).await;
    assert_eq!(row(&list, "chat-lane")["running"], true);

    // --- messaging: the SAME send endpoint, with the body the dashboard's
    // composer sends for a human message (`record_history`) --------------
    let (st, r) = call(&app, "POST", "/api/sessions/chat-lane/send", Some(json!({"text": "hello chat", "record_history": true}))).await;
    assert!(st.is_success(), "{st} {r}");
    let h1 = wait_for_reply(&app, "chat-lane", 1).await;
    let msgs = h1["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["text"], "hello chat");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["text"], "echo: hello chat");
    assert!(msgs[1]["error"].is_null(), "{}", msgs[1]);

    // History + resume: a second turn resumes the SAME conversation.
    call(&app, "POST", "/api/sessions/chat-lane/send", Some(json!({"text": "again", "record_history": true}))).await;
    let h2 = wait_for_reply(&app, "chat-lane", 2).await;
    assert_eq!(h2["messages"].as_array().unwrap().len(), 4);
    let log = std::fs::read_to_string(&args_log).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    let conv = h2["conversation_id"].as_str().unwrap().to_string();
    assert!(lines[0].contains(&format!("--session-id {conv}")), "{log}");
    assert!(lines[1].contains(&format!("--resume {conv}")), "{log}");
    assert!(!log.contains("hello chat"), "the prompt goes on stdin, never argv: {log}");

    // Terminal-lane conversation management writes `cc_conversation_id`
    // (adoption, takeover, reset). An outside write there must not start a new
    // chat conversation: the adapter resumes from its own key and restores
    // the mirror.
    let meta_path = home.join("sessions/chat-lane.meta.json");
    let mut meta: Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta["cc_conversation_id"] = json!("");
    std::fs::write(&meta_path, meta.to_string()).unwrap();
    call(&app, "POST", "/api/sessions/chat-lane/send", Some(json!({"text": "third", "record_history": true}))).await;
    wait_for_reply(&app, "chat-lane", 3).await;
    let log = std::fs::read_to_string(&args_log).unwrap();
    assert!(log.lines().nth(2).unwrap().contains(&format!("--resume {conv}")), "{log}");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["cc_conversation_id"], json!(conv), "mirror restored for transcript readers");

    // --- canonical event stream: ONE source of truth ------------------------
    {
        let conn = store.read().unwrap();
        let chat_events: i64 = conn.query_row(
            "SELECT COUNT(*) FROM session_events WHERE session='chat-lane' AND type='chat.message'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(chat_events, 6, "every chat message is a canonical session event");
        let ledger: i64 = conn.query_row(
            "SELECT COUNT(*) FROM cmd_history WHERE session='chat-lane'", [], |r| r.get(0)).unwrap();
        assert!(ledger >= 2, "inbound sends land in the same message ledger as coding workers");
        let status: i64 = conn.query_row(
            "SELECT COUNT(*) FROM session_events WHERE session='chat-lane' AND type='session.native_status'",
            [], |r| r.get(0)).unwrap();
        assert!(status >= 3, "turn state flows through the provider status channel: {status}");
    }

    // --- peek: the same verb, rendered from the transcript --------------------
    let (st, peek) = call(&app, "GET", "/api/sessions/chat-lane/peek", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(peek["renderer"], "chat");
    assert!(peek["output"].as_str().unwrap().contains("echo: third"), "{peek}");

    // --- edit: type is editable; identity (name, board, env) survives -------
    let (st, r) = call(&app, "PATCH", "/api/sessions/code-lane/config", Some(json!({"worker_type":"chat"}))).await;
    assert!(st.is_success(), "{st} {r}");
    let (_, list) = call(&app, "GET", "/api/sessions", None).await;
    assert_eq!(row(&list, "code-lane")["worker_type"], "chat");
    assert_eq!(row(&list, "code-lane")["dir"], json!(repo), "keeps its project dir");
    let (st, _) = call(&app, "PATCH", "/api/sessions/code-lane/config", Some(json!({"worker_type":"coding"}))).await;
    assert!(st.is_success());
    let env = std::fs::read_to_string(home.join("sessions/code-lane.env")).unwrap();
    assert!(!env.contains("CC_WORKER_TYPE"), "back to the default spelling: {env}");

    // --- lifecycle: stop ------------------------------------------------------
    let (st, _) = call(&app, "POST", "/api/sessions/chat-lane/stop", Some(json!({}))).await;
    assert!(st.is_success());
    for _ in 0..100 {
        let (_, list) = call(&app, "GET", "/api/sessions", None).await;
        if row(&list, "chat-lane")["running"] == false { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (_, list) = call(&app, "GET", "/api/sessions", None).await;
    assert_eq!(row(&list, "chat-lane")["running"], false);
    // History survives a stop (persistence), served from the event stream.
    let (_, h3) = call(&app, "GET", "/api/sessions/chat-lane/chat", None).await;
    assert_eq!(h3["messages"].as_array().unwrap().len(), 6);

    // --- store-backed workers carry the same field --------------------------
    let (st, w) = call(&app, "POST", "/api/workers",
        Some(json!({"display_name":"store-chat","worker_type":"chat"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{w}");
    assert_eq!(w["worker_type"], "chat");
    let (st, w) = call(&app, "POST", "/api/workers", Some(json!({"display_name":"store-code"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{w}");
    assert_eq!(w["worker_type"], "coding");
    let (st, e) = call(&app, "POST", "/api/workers",
        Some(json!({"display_name":"store-bad","worker_type":"chat","provider":"ollama"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
}
