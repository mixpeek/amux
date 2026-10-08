//! Chat delegate (AMUX-5432): a worker's Chat asks a read-only background job on
//! the worker's own provider, and the job never interferes with the worker.
//!
//! One test function, run in sequence, because AMUX_HOME, HOME and PATH are
//! process-global. A fake provider stands in for claude: it records what it was
//! given, tries to write into the worker's checkout, and answers with what it
//! read from an UNCOMMITTED file. A fake `tmux` on PATH records every call so
//! the test can prove nothing was ever sent to the worker's pane.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const FAKE_CLAUDE: &str = r#"#!/bin/sh
{ echo "ARGS $*"; echo "SESSION $AMUX_SESSION"; echo "PWD $PWD"; echo "LOCKS $GIT_OPTIONAL_LOCKS"; } >> "$FAKE_LOG"
input=$(cat)
if [ -n "$FAKE_SLEEP" ]; then sleep "$FAKE_SLEEP"; fi
if echo pwned > "$PWD/pwned.txt" 2>/dev/null; then echo "WRITE ok" >> "$FAKE_LOG"; else echo "WRITE denied" >> "$FAKE_LOG"; fi
git status --porcelain >/dev/null 2>&1
uncommitted=$(cat "$PWD/notes.txt")
python3 -c 'import json,sys; print(json.dumps({"type":"result","subtype":"success","is_error":False,"result":"answer from "+sys.argv[1],"total_cost_usd":0.0123}))' "$uncommitted"
"#;

const FAKE_TMUX: &str = r#"#!/bin/sh
echo "TMUX $*" >> "$FAKE_LOG"
exit 1
"#;

async fn call(app: &axum::Router, method: &str, uri: &str, session: &str, body: Option<Value>) -> (u16, Value) {
    let mut b = Request::builder().method(method).uri(uri).header("x-amux-session", session);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let req = b.body(body.map(|v| Body::from(v.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let code = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn wait_done(app: &axum::Router, id: &str, secs: u64) -> Value {
    for _ in 0..(secs * 10) {
        let (_, j) = call(app, "GET", &format!("/api/chat-delegate/{id}"), "", None).await;
        if j["status"] == "done" {
            return j;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("job {id} did not finish in {secs}s");
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success();
    assert!(ok, "git {args:?}");
}

fn index_fingerprint(dir: &Path) -> (std::time::SystemTime, Vec<u8>) {
    let p = dir.join(".git/index");
    (std::fs::metadata(&p).unwrap().modified().unwrap(), std::fs::read(&p).unwrap())
}

fn delegate_messages(h: &Value) -> usize {
    h["messages"].as_array().map(|m| m.iter().filter(|m| m["origin"] == "delegate").count()).unwrap_or(0)
}

fn delegate_job_messages(h: &Value, id: &str) -> usize {
    let turn = format!("delegate-{id}");
    h["messages"].as_array().map(|m| m.iter().filter(|m| m["origin"] == "delegate" && m["turn_id"] == turn).count()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_delegate_reads_the_live_worker_without_ever_touching_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("fake-claude");
    std::fs::write(&fake, FAKE_CLAUDE).unwrap();
    std::fs::write(bin.join("tmux"), FAKE_TMUX).unwrap();
    for f in [&fake, &bin.join("tmux")] {
        std::process::Command::new("chmod").arg("+x").arg(f).status().unwrap();
    }
    let log = tmp.path().join("fake.log");
    let user_home = tmp.path().join("user");
    std::fs::create_dir_all(user_home.join(".claude/projects/p")).unwrap();
    unsafe {
        std::env::set_var("AMUX_HOME", &home);
        std::env::set_var("HOME", &user_home);
        std::env::set_var("AMUX_DELEGATE_CLAUDE_BIN", &fake);
        std::env::set_var("FAKE_LOG", &log);
        std::env::set_var("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()));
    }

    // The worker's LIVE checkout: one commit, plus an uncommitted edit.
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "base"]);
    std::fs::write(repo.join("notes.txt"), "the dress code is cocktail attire").unwrap();
    git(&repo, &["add", "notes.txt"]);
    std::fs::write(repo.join("notes.txt"), "UNCOMMITTED: the dress code is cocktail attire").unwrap();
    std::fs::write(
        home.join("sessions/social.env"),
        format!("CC_DIR=\"{}\"\nCC_PROVIDER=\"claude\"\nCC_FLAGS=\"--dangerously-skip-permissions\"\n", repo.display()),
    )
    .unwrap();
    let index_before = index_fingerprint(&repo);

    let store = Arc::new(Store::open(&tmp.path().join("t.db")).unwrap());
    let state = AppState {
        store,
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let app = router(state.clone());

    // A delegate is a background job on the EXISTING worker, never a worker of
    // its own (Ethan, 2026-10-01: "it should be a background task on the
    // existing chats worker"). The worker list must not change around a job.
    let worker_names = |v: &Value| -> Vec<String> {
        let mut n: Vec<String> = v.as_array().map(|a| a.iter().filter_map(|s| s["name"].as_str().map(str::to_string)).collect()).unwrap_or_default();
        n.sort();
        n
    };
    let (_, before_list) = call(&app, "GET", "/api/sessions", "", None).await;
    let workers_before = worker_names(&before_list);

    // 1. A fresh delegate: read-only, sees uncommitted work, answers into the Chat.
    let (code, r) = call(&app, "POST", "/api/chat-delegate", "social@chat",
        Some(json!({"prompt": "what is the dress code?", "wait_s": 0}))).await;
    assert_eq!(code, 200, "{r}");
    let id = r["job"]["id"].as_str().unwrap().to_string();
    assert_eq!(r["job"]["worker"], "social", "a Chat's caller name resolves to its worker");
    assert_eq!(r["job"]["mode"], "fresh", "no conversation to fork, so fresh");
    let job = wait_done(&app, &id, 30).await;
    assert_eq!(job["outcome"], "ok", "{job}");
    assert!(job["result"].as_str().unwrap().contains("UNCOMMITTED: the dress code"), "the delegate reads uncommitted work: {job}");
    assert_eq!(job["cost_usd"], json!(0.0123));
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("--permission-mode dontAsk"), "{logged}");
    assert!(!logged.contains("dangerously"), "the worker's YOLO flag never reaches a delegate: {logged}");
    assert!(logged.contains("SESSION social@delegate"), "{logged}");
    assert!(logged.contains("LOCKS 0"), "git may not refresh the shared index: {logged}");
    // Layer 1, the provider CLI's read-only mode, is asserted on its argv above
    // (a fake provider cannot enforce flags it ignores). Layer 3, the API
    // guard, is asserted in step 2 below. Layer 2, the OS sandbox, exists only
    // where macOS sandbox-exec does: on Linux (CI) there is no unprivileged
    // equivalent amux can rely on (Ubuntu 24.04 blocks the user namespaces
    // bwrap needs), so the job must say it ran WITHOUT it rather than claim it.
    // AMUX_DELEGATE_NO_SANDBOX=1 runs the Linux path on a Mac, so both arms are
    // exercisable locally; a Mac WITHOUT it must have the sandbox.
    let sandbox_expected = Path::new("/usr/bin/sandbox-exec").is_file()
        && std::env::var("AMUX_DELEGATE_NO_SANDBOX").ok().as_deref() != Some("1");
    if sandbox_expected {
        assert!(logged.contains("WRITE denied"), "the OS sandbox denies writes in the checkout: {logged}");
        assert_eq!(job["enforcement"]["os_sandbox"], true, "{job}");
        assert!(!repo.join("pwned.txt").exists(), "nothing may be written into the worker's checkout");
    } else {
        assert_eq!(job["enforcement"]["os_sandbox"], false, "no OS sandbox here, and the record must say so: {job}");
        assert_eq!(job["enforcement"]["cli"], true, "{job}");
        assert_eq!(job["enforcement"]["api_guard"], true, "{job}");
        // The fake ignored the CLI's read-only flags, so its probe file may exist.
        let _ = std::fs::remove_file(repo.join("pwned.txt"));
    }
    assert_eq!(index_fingerprint(&repo), index_before, "the shared git index is untouched");
    // Never anything into the worker's pane: no tmux verb that sends input.
    assert!(logged.lines().any(|l| l.starts_with("TMUX capture-pane ")), "the seed read the pane: {logged}");
    for line in logged.lines().filter(|l| l.starts_with("TMUX ")) {
        for input in ["send-keys", "paste-buffer", "load-buffer", "set-buffer", "respawn-pane", "kill-"] {
            assert!(!line.contains(input), "nothing may be sent to the worker's pane: {line}");
        }
    }
    // The answer reaches the Chat exactly once (the Chat is not running, so it is recorded).
    let mut delivered = 0;
    for _ in 0..80 {
        let (_, h) = call(&app, "GET", "/api/sessions/social@chat/chat", "", None).await;
        delivered = delegate_messages(&h);
        if delivered > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(delivered, 1, "the answer is posted into the Chat once");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, h) = call(&app, "GET", "/api/sessions/social@chat/chat", "", None).await;
    assert_eq!(delegate_messages(&h), 1, "and never twice");
    let (_, after_list) = call(&app, "GET", "/api/sessions", "", None).await;
    let workers_after = worker_names(&after_list);
    assert_eq!(workers_after, workers_before, "a delegate job never creates or lists a worker");
    assert!(!workers_after.iter().any(|n| n.contains("@delegate") || n.starts_with("dg-")), "{workers_after:?}");
    assert!(!home.join("sessions").read_dir().unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("delegate")),
        "no session file is written for a delegate");

    // 1b. Attribution: a LANE posting to a Chat tab is that lane, not the owner.
    // An owner send would start the stopped Chat; a lane's is refused instead.
    let (code, r) = call(&app, "POST", "/api/sessions/social@chat/send", "some-helper",
        Some(json!({"text": "probe from a helper"}))).await;
    assert_eq!(code, 409, "a lane's send is not the owner's and cannot start a stopped Chat: {r}");

    // 2. The API guard: a delegate identity may read, never change anything.
    let (code, _) = call(&app, "POST", "/api/board", "social@delegate", Some(json!({"title": "x"}))).await;
    assert_eq!(code, 403, "a delegate cannot write the board");
    let (code, _) = call(&app, "POST", "/api/chat-delegate", "social@delegate", Some(json!({"prompt": "nested"}))).await;
    assert_eq!(code, 403, "a delegate cannot start another delegate");
    let (code, _) = call(&app, "GET", "/api/chat-delegate?worker=social", "social@delegate", None).await;
    assert_eq!(code, 200, "reads stay open");

    // 3. A waiting caller consumes the answer: it is not posted a second time.
    let (_, r) = call(&app, "POST", "/api/chat-delegate", "social@chat",
        Some(json!({"prompt": "again?", "wait_s": 30}))).await;
    let id2 = r["job"]["id"].as_str().unwrap().to_string();
    let _ = wait_done(&app, &id2, 30).await;
    let (_, j2) = call(&app, "GET", &format!("/api/chat-delegate/{id2}?consume=1"), "", None).await;
    assert_eq!(j2["consumed"], true);

    // 4. Timeout: the whole process group is stopped.
    std::fs::write(
        home.join("sessions/social.env"),
        format!("CC_DIR=\"{}\"\nCC_PROVIDER=\"claude\"\nAMUX_CHAT_DELEGATE_TIMEOUT_S=\"2\"\n", repo.display()),
    )
    .unwrap();
    unsafe { std::env::set_var("FAKE_SLEEP", "30") };
    let (_, r) = call(&app, "POST", "/api/chat-delegate", "social@chat", Some(json!({"prompt": "slow", "wait_s": 0}))).await;
    let id3 = r["job"]["id"].as_str().unwrap().to_string();
    let pgid = r["job"]["pgid"].as_i64().unwrap() as i32;
    let j3 = wait_done(&app, &id3, 20).await;
    assert_eq!(j3["outcome"], "timeout", "{j3}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_ne!(unsafe { libc::kill(-pgid, 0) }, 0, "the timed-out job's process group is gone");
    unsafe { std::env::remove_var("FAKE_SLEEP") };

    // 5. Fork of the worker's conversation: the original session file is untouched.
    std::fs::write(home.join("sessions/social.env"), format!("CC_DIR=\"{}\"\nCC_PROVIDER=\"claude\"\n", repo.display())).unwrap();
    let session_file = user_home.join(".claude/projects/p/conv-live.jsonl");
    std::fs::write(&session_file, "{\"type\":\"user\",\"message\":\"the secret number is 42\"}\n").unwrap();
    std::fs::write(home.join("sessions/social.meta.json"), json!({"cc_conversation_id": "conv-live"}).to_string()).unwrap();
    let before = std::fs::read(&session_file).unwrap();
    let (_, r) = call(&app, "POST", "/api/chat-delegate", "social@chat", Some(json!({"prompt": "what number?", "wait_s": 0}))).await;
    assert_eq!(r["job"]["mode"], "fork", "{r}");
    let id4 = r["job"]["id"].as_str().unwrap().to_string();
    let j4 = wait_done(&app, &id4, 30).await;
    assert_eq!(j4["outcome"], "ok");
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("--resume conv-live --fork-session") && logged.contains("--no-session-persistence"), "{logged}");
    assert_eq!(std::fs::read(&session_file).unwrap(), before, "the worker's conversation file is byte-identical");
    assert!(j4["record"].as_str().unwrap().contains("not the live agent"), "a forked answer is labelled: {j4}");
    assert!(j4["cost_usd"].is_null() && j4["cost_note"].as_str().is_some(), "a fork's cost is not reported as the forked total: {j4}");

    // 5b. A conversation over the fork cap is answered fresh, and says why.
    std::fs::write(
        home.join("sessions/social.env"),
        format!("CC_DIR=\"{}\"\nCC_PROVIDER=\"claude\"\nAMUX_CHAT_DELEGATE_FORK_MAX_MB=\"0.00001\"\n", repo.display()),
    )
    .unwrap();
    let (_, r) = call(&app, "POST", "/api/chat-delegate", "social@chat", Some(json!({"prompt": "big", "wait_s": 0}))).await;
    assert_eq!(r["job"]["mode"], "fresh", "{r}");
    assert!(r["job"]["fork_skipped"].as_str().unwrap_or("").contains("fork cap"), "{r}");
    let capped_id = r["job"]["id"].as_str().unwrap();
    let capped_job = wait_done(&app, capped_id, 30).await;
    assert_eq!(capped_job["outcome"], "ok", "{capped_job}");
    let (code, r) = call(&app, "POST", "/api/chat-delegate", "social@chat", Some(json!({"prompt": "big", "mode": "fork"}))).await;
    assert_eq!(code, 400, "an explicit fork of an over-cap conversation is refused with the reason: {r}");
    assert!(r["error"].as_str().unwrap_or("").contains("fork cap"), "{r}");

    // 6. Restart: a job the previous server started is followed and delivered once.
    let id5 = "dg-RESTARTED";
    let dir = home.join("chat-state/delegates").join(id5);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("in"), "q").unwrap();
    let inner = format!(
        "FAKE_SLEEP=1 {} -p; ec=$?; printf '%s' \"$ec\" > '{d}/exit.tmp' && mv '{d}/exit.tmp' '{d}/exit'",
        fake.display(),
        d = dir.display()
    );
    let launcher = format!("bash -c \"$0\" < '{d}/in' > '{d}/out' 2> '{d}/err' & echo $!", d = dir.display());
    let child = std::process::Command::new("bash").arg("-c").arg(&launcher).arg(&inner).current_dir(&repo).process_group(0).output().unwrap();
    let pid: i32 = String::from_utf8_lossy(&child.stdout).trim().parse().unwrap();
    let pg = unsafe { libc::getpgid(pid) };
    std::fs::write(
        dir.join("job.json"),
        json!({"id": id5, "worker": "social", "chat": "social@chat", "provider": "claude", "mode": "fresh",
               "prompt": "restart?", "pgid": pg, "status": "running", "started": 1.0e12, "timeout_s": 60, "wait_until": 0})
            .to_string(),
    )
    .unwrap();
    // Earlier jobs can finish during this recovery. Count the durable identity,
    // not all delegate messages after an arbitrary sleep (CI exposed that race).
    let (_, h) = call(&app, "GET", "/api/sessions/social@chat/chat", "", None).await;
    assert_eq!(delegate_job_messages(&h, id5), 0);
    let n = amux_server::api::chat_delegate::recover_all(&state).await;
    assert_eq!(n, 1, "the running job is re-attached");
    let j5 = wait_done(&app, id5, 30).await;
    assert_eq!(j5["outcome"], "ok", "{j5}");
    let mut after = 0;
    for _ in 0..80 {
        let (_, h) = call(&app, "GET", "/api/sessions/social@chat/chat", "", None).await;
        after = delegate_job_messages(&h, id5);
        if after > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(after, 1, "the re-attached job's answer is delivered once");
    amux_server::api::chat_delegate::recover_all(&state).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, h) = call(&app, "GET", "/api/sessions/social@chat/chat", "", None).await;
    assert_eq!(delegate_job_messages(&h, id5), 1, "another recovery does not redeliver this identity");
}

/// No code path in the delegate module can put input into a worker: no tmux
/// key or paste, no steering queue, no message send.
#[test]
fn the_delegate_module_has_no_path_into_a_worker_pane() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/api/chat_delegate.rs")).unwrap();
    let code: String = src.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
    for banned in ["send-keys", "paste-buffer", "load-buffer", "steer_enqueue", "send_to_session", "deliver_to_pane", "/send\""] {
        assert!(!code.contains(banned), "chat_delegate.rs must not contain `{banned}`");
    }
}
