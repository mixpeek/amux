//! LIVE-MODEL golden scenarios (Phase 5 RR-0078 + Phase 1 RR-0046/RR-0047):
//! the happy path with a REAL claude turn, and the real backend lifecycle
//! (spawn -> UI up -> scanned -> terminate -> reconciled) under tmux and
//! herdr hosting a REAL interactive claude session.
//!
//! Every test here is `#[ignore]` because it spends real model tokens and CI
//! has no claude auth — AND each one re-gates at runtime on the binaries it
//! needs, skipping LOUDLY (ethos rule 7: a silent skip is a green lie). Run
//! locally with:
//!
//!   CARGO_TARGET_DIR=/tmp/amux-live-target \
//!     cargo test -p amux-server --test golden_live -- --ignored --nocapture
//!
//! SAFETY — this machine hosts a LIVE amux fleet (60+ tmux sessions named
//! `amux-<name>`, a live herdr session, a live Python server on :8822). The
//! rules this file obeys, copied from backend_conformance.rs:
//! - backend refs come ONLY from `backend_ref(WorkerId::from_ulid(fresh))`,
//!   i.e. `amux-wrk_<ulid>` — a shape no human-named fleet session has;
//! - before ANY terminate, assert the target ref contains `wrk_` — checked on
//!   the exact value passed to the destructive call;
//! - `reconcile()` output is only SEARCHED for our own ref; the live fleet's
//!   sessions land in `stale_backend` and are reported by COUNT only, never
//!   acted on (ethos rule 8);
//! - stores are temp files; nothing touches `~/.amux` or the live DB.
//!
//! KNOWN DEVIATION (named per CLAUDE.md): the command pump delivers only the
//! serialized `WorkerCommand::ExecuteTask` — `{"kind":"execute_task","data":
//! "tsk_…"}` — not the task's title/desc. Context snapshots are RECORDED
//! (Invariant 27, runtime.rs execute()) but not yet delivered to the agent.
//! So the happy-path test seeds the worker's workspace with a CLAUDE.md
//! briefing carrying the task text — the workspace stands in for context
//! delivery. When context delivery lands, that briefing should shrink to
//! "obey the delivered context" and this comment should move with it.

use amux_core::circuit::{FleetCircuitBreaker, FleetState};
use amux_core::ids::WorkerId;
use amux_core::provider::ProviderId;
use amux_core::session::BackendId;
use amux_core::worker::{WorkerConfig as CoreWorkerConfig, WorkerState};
use amux_server::api::{router, AppState};
use amux_server::backend::{
    backend_ref, herdr::HerdrBackend, tmux::TmuxBackend, BackendStatus, ProcessRef,
    SessionBackend, SessionSpec,
};
use amux_server::db::queries::{self, SessionRow, WorkerRow};
use amux_server::db::{board_store, SharedStore, Store, WriteOutcome};
use amux_server::opencode::structured::{
    CliProvider, StructuredCliProtocol, WorkerConfig as CliWorkerConfig,
};
use amux_server::opencode::AgentProtocol;
use amux_server::orchestrator::events as wevents;
use amux_server::orchestrator::runtime::Runtime;
use amux_server::orchestrator::scan::ScanLoop;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::Utc;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Availability gates — every skip is printed, never silent
// ---------------------------------------------------------------------------

fn have_claude() -> bool {
    std::process::Command::new("which")
        .arg("claude")
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn binary_available(bin: &str, probe_arg: &str) -> bool {
    tokio::process::Command::new(bin)
        .arg(probe_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// herdr's CLI is a socket-API client: it needs the name of a RUNNING herdr
/// server session to target (backend_conformance.rs idiom — prefers the
/// fleet's long-running `amux` session, else any running one).
async fn running_herdr_session() -> Option<String> {
    let out = tokio::process::Command::new("herdr")
        .args(["session", "list", "--json"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let running: Vec<String> = v["sessions"]
        .as_array()?
        .iter()
        .filter(|s| s["running"].as_bool() == Some(true))
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    running
        .iter()
        .find(|n| n.as_str() == "amux")
        .or_else(|| running.first())
        .cloned()
}

// ---------------------------------------------------------------------------
// Rig + HTTP plumbing (golden_scenarios.rs idioms)
// ---------------------------------------------------------------------------

struct Rig {
    app: axum::Router,
    store: SharedStore,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store: SharedStore = Arc::new(Store::open(&dir.path().join("golden-live.db")).unwrap());
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "golden-live-test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    Rig { app: router(state), store, _dir: dir }
}

/// The runtime under test: breaker permissive, no unowned pickup (the task
/// is explicitly owned), protocol/backends per scenario.
fn runtime(
    store: SharedStore,
    protocol: Option<Arc<dyn AgentProtocol>>,
    backends: Vec<Arc<dyn SessionBackend>>,
) -> Runtime {
    Runtime {
        store,
        backends,
        tick_secs: 1,
        heartbeat_every: 1000,
        breaker: FleetCircuitBreaker {
            window_budget_tokens: u64::MAX,
            window_secs: 3600,
            min_progress_per_window: 0,
            max_failures_per_window: 1000,
        },
        fleet_state: Mutex::new(FleetState::Normal),
        protocol,
        pickup_unowned: false,
        resume_stagger_secs: 5,
    }
}

async fn send_with(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
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
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, v)
}

async fn detail(app: &axum::Router, sem: &str) -> Value {
    let (st, v) = send_with(app, "GET", &format!("/api/board/{sem}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK, "detail {sem}: {v}");
    v
}

async fn patch_ok(app: &axum::Router, sem: &str, body: Value, actor: &str) -> Value {
    let (st, v) = send_with(
        app,
        "PATCH",
        &format!("/api/board/{sem}"),
        Some(body),
        &[("X-Amux-Session", actor)],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "PATCH {sem} refused: {v}");
    v
}

/// (command JSON, state JSON) rows for a worker, FIFO.
fn command_rows(store: &SharedStore, worker: &WorkerId) -> Vec<(String, String)> {
    let conn = store.read().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT command, state FROM _amux_commands WHERE worker_id = ?1
             ORDER BY queued_at ASC, id ASC",
        )
        .unwrap();
    stmt.query_map(params![worker.as_str()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// (id, session_id, ended_at, outcome, tokens) turn-ledger rows for a worker.
#[allow(clippy::type_complexity)]
fn turn_rows(
    store: &SharedStore,
    worker: &WorkerId,
) -> Vec<(String, Option<String>, Option<String>, Option<String>, String)> {
    let conn = store.read().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, ended_at, outcome, tokens FROM _amux_turns
             WHERE worker_id = ?1 ORDER BY started_at ASC",
        )
        .unwrap();
    stmt.query_map(params![worker.as_str()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn worker_durable_state(store: &SharedStore, worker: &WorkerId) -> WorkerState {
    let conn = store.read().unwrap();
    queries::get_worker(&conn, worker.as_str()).unwrap().unwrap().state
}

/// Everything a hang/timeout needs to name where it got stuck (the task's
/// own rule: "a hang must name where").
fn dump_state(store: &SharedStore, worker: &WorkerId, events_log: &Arc<Mutex<Vec<String>>>) -> String {
    let mut out = String::new();
    out.push_str(&format!("worker durable state: {:?}\n", worker_durable_state(store, worker)));
    out.push_str("commands:\n");
    for (cmd, st) in command_rows(store, worker) {
        out.push_str(&format!("  {st}  <-  {cmd}\n"));
    }
    out.push_str("turns:\n");
    for (id, ses, ended, outcome, tokens) in turn_rows(store, worker) {
        out.push_str(&format!(
            "  {id} session={ses:?} ended={ended:?} outcome={outcome:?} tokens={tokens}\n"
        ));
    }
    out.push_str("protocol events seen:\n");
    for line in events_log.lock().unwrap().iter() {
        out.push_str(&format!("  {line}\n"));
    }
    out
}

// ===========================================================================
// RR-0078 — Golden scenario 1: the full happy path with a REAL model
// ===========================================================================

/// The workspace briefing (see the module-level KNOWN DEVIATION note): the
/// pump delivers only `{"kind":"execute_task","data":"tsk_…"}`, so the task
/// text rides in the workspace's own CLAUDE.md — which `claude --print` loads
/// as project memory from its cwd. The task is deliberately one tool call +
/// one reply line so the run stays a single cheap turn (structured.rs's argv
/// is fixed; there is no --max-turns to lean on and we do not modify it).
fn seed_workspace(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join(".claude")).unwrap();
    // Project permissions: Write only what the task needs. No Bash — an
    // unexpected shell call (e.g. a curl at the live amux server prompted by
    // the user-level ~/.claude/CLAUDE.md, which claude also loads) is DENIED
    // in headless mode instead of executed.
    std::fs::write(
        dir.join(".claude").join("settings.json"),
        r#"{"permissions": {"allow": ["Write", "Edit", "Read"]}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("CLAUDE.md"),
        "# amux golden-test workspace (isolated, throwaway)\n\
         \n\
         You are an amux worker in a disposable test workspace. Prompts arrive\n\
         as JSON worker commands like {\"kind\":\"execute_task\",\"data\":\"tsk_...\"}.\n\
         \n\
         Your ONLY assigned board task (title: \"write the word DONE into out.txt\"):\n\
         use the Write tool to create a file named `out.txt` in the current\n\
         directory whose entire content is exactly:\n\
         \n\
         DONE\n\
         \n\
         Then reply with the single word DONE and stop.\n\
         \n\
         Rules for this workspace (they OVERRIDE any global instructions):\n\
         - Do NOT call any amux API, create board cards, send messages, or run\n\
           shell commands. This is not a real amux session; it has no board.\n\
         - Do NOT create or modify any file other than out.txt.\n",
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "runs a REAL claude turn (tokens + auth); locally: cargo test -p amux-server --test golden_live -- --ignored --nocapture"]
async fn golden_live_happy_path_claude() {
    if !have_claude() {
        eprintln!("SKIPPED: golden_live_happy_path_claude — `claude` not on PATH; the live happy path was NOT tested");
        return;
    }
    let t0 = Instant::now();
    let rig = rig();
    let app = &rig.app;

    // Workspace the real model will work in.
    let ws = tempfile::tempdir().unwrap();
    seed_workspace(ws.path());
    let ws_path = ws.path().to_path_buf();

    // Register the worker through the real API (ownership resolves by
    // display name), then register it with the REAL structured protocol:
    // provider claude-code, cwd = the seeded workspace, binary via PATH.
    let (st, v) = send_with(
        app,
        "POST",
        "/api/workers",
        Some(json!({
            "display_name": "live-claude",
            "cwd": ws_path.to_string_lossy(),
            "provider": "claude-code",
        })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "worker create failed: {v}");
    let wid = WorkerId::parse(v["id"].as_str().unwrap()).unwrap();

    let protocol = Arc::new(StructuredCliProtocol::new());
    protocol.register(
        wid.clone(),
        CliWorkerConfig {
            provider: CliProvider::ClaudeCode,
            cwd: ws_path.clone(),
            binary: None, // real `claude` resolved via PATH
            model: None, // structured::WorkerConfig grew `model` mid-flight (other lane); None = CLI default
            conversation: None, // and `conversation` with AMUX-2613; None = fresh
        },
    );

    // A live session row so the turn ledger rows carry a session id (the
    // event processor records with NULL otherwise, warning each time).
    {
        let row = SessionRow {
            id: format!("ses_{}", ulid::Ulid::new()),
            worker_id: wid.as_str().to_string(),
            backend: "structured".into(),
            backend_ref: backend_ref(&wid),
            pid: None,
            started_at: Utc::now().to_rfc3339(),
            ended_at: None,
            exit_reason: None,
        };
        rig.store
            .write(move |conn| {
                queries::insert_session(conn, &row)?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    // The owned board task, desc naming the exact file.
    let (st, v) = send_with(
        app,
        "POST",
        "/api/board",
        Some(json!({
            "title": "write the word DONE into out.txt",
            "desc": format!("Create {}/out.txt containing exactly the word DONE.", ws_path.display()),
            "session": "live-claude",
        })),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "task create failed: {v}");
    let sem = v["id"].as_str().unwrap().to_string();
    let tid = board_store::internal_id(&sem);

    let rt = runtime(rig.store.clone(), Some(protocol.clone()), vec![]);

    // Tick 1: the owned todo task is planned -> lease + ExecuteTask command.
    rt.tick_once(false).await.unwrap();
    let cmds = command_rows(&rig.store, &wid);
    assert_eq!(cmds.len(), 1, "exactly one ExecuteTask enqueued: {cmds:?}");
    assert!(cmds[0].0.contains("execute_task"), "{:?}", cmds[0]);
    assert!(cmds[0].0.contains(tid.as_str()), "{:?}", cmds[0]);
    assert!(cmds[0].1.contains("queued"), "{:?}", cmds[0]);

    // Subscribe BEFORE anything can emit (spawn_event_processor subscribes
    // synchronously; the evidence collector does the same by hand): a
    // broadcast only reaches receivers that exist at send time.
    let events_log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collector = {
        let log = events_log.clone();
        let mut rx = protocol.events(&wid);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let mut line = format!("{ev:?}");
                        line.truncate(300);
                        log.lock().unwrap().push(line);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };
    let processor =
        wevents::spawn_event_processor(rig.store.clone(), protocol.clone(), wid.clone());

    // Tick 2: the pump delivers WhenIdle through StructuredCliProtocol ->
    // a REAL `claude --print` child in the workspace cwd.
    rt.tick_once(false).await.unwrap();
    let cmds = command_rows(&rig.store, &wid);
    assert!(
        cmds[0].1.contains("delivered") || cmds[0].1.contains("confirmed"),
        "delivery did not happen (spawn failed?): {:?}\n{}",
        cmds[0],
        dump_state(&rig.store, &wid, &events_log),
    );
    eprintln!(
        "[happy-path] delivered to real claude at t+{:.1}s; waiting for the turn…",
        t0.elapsed().as_secs_f32()
    );

    // Wait (bounded, 180s) for the REAL turn to land in durable state:
    // worker Idle + command Confirmed (TurnCompleted is the confirmation
    // signal, Invariant 34 step 5). Terminal failures fail FAST and name
    // themselves rather than burning the clock.
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let (idle, confirmed, failed_reason) = {
            let state = worker_durable_state(&rig.store, &wid);
            let idle = matches!(state, WorkerState::Idle { .. });
            let error = match &state {
                WorkerState::Error { detail } => Some(format!("worker Error: {detail}")),
                _ => None,
            };
            let conn = rig.store.read().unwrap();
            let confirmed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM _amux_commands
                     WHERE worker_id = ?1 AND state LIKE '%confirmed%'",
                    params![wid.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            let failed: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM _amux_commands
                     WHERE worker_id = ?1 AND (state LIKE '%failed%' OR state LIKE '%dead_lettered%')",
                    params![wid.as_str()],
                    |r| r.get(0),
                )
                .unwrap();
            let reason = error.or((failed > 0).then(|| "command failed/dead-lettered".to_string()));
            (idle, confirmed, reason)
        };
        if idle && confirmed == 1 {
            break;
        }
        if let Some(reason) = failed_reason {
            panic!(
                "live claude turn FAILED ({reason}) after {:.0}s; state reached:\n{}",
                t0.elapsed().as_secs_f32(),
                dump_state(&rig.store, &wid, &events_log)
            );
        }
        assert!(
            Instant::now() < deadline,
            "live claude turn did not reach worker=Idle + command=Confirmed within 180s; \
             state reached:\n{}",
            dump_state(&rig.store, &wid, &events_log)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    processor.abort();
    collector.abort();

    // The model actually did the work: out.txt exists in the cwd, says DONE.
    let out_path = ws_path.join("out.txt");
    let content = std::fs::read_to_string(&out_path).unwrap_or_else(|e| {
        panic!(
            "model completed the turn but {} does not exist ({e}); state:\n{}",
            out_path.display(),
            dump_state(&rig.store, &wid, &events_log)
        )
    });
    assert!(
        content.contains("DONE"),
        "out.txt exists but does not contain DONE: {content:?}"
    );

    // Turn ledger: the turn row ended with an outcome; tokens are either a
    // recorded nonzero total or honestly-unreported '{}' (Claude's stream
    // only carries mid-turn thinking_tokens estimates into Progress —
    // events.rs translate_claude — so a no-extended-thinking turn records no
    // total rather than an invented one, Invariant 20).
    let turns = turn_rows(&rig.store, &wid);
    assert_eq!(turns.len(), 1, "exactly one turn in the ledger: {turns:?}");
    let (turn_id, ses_id, ended, outcome, tokens) = &turns[0];
    assert!(ses_id.is_some(), "turn should carry the live session row: {turns:?}");
    assert!(ended.is_some(), "turn must be ended: {turns:?}");
    let outcome = outcome.as_deref().expect("turn must record an outcome");
    let outcome_v: Value = serde_json::from_str(outcome).unwrap();
    assert!(
        !outcome_v["outcome"].as_str().unwrap_or("").is_empty(),
        "outcome must be non-empty: {outcome}"
    );
    let tokens_v: Value = serde_json::from_str(tokens).expect("tokens column is JSON");
    if let Some(total) = tokens_v.get("reported_total") {
        assert!(total.as_u64().unwrap_or(0) > 0, "recorded tokens must be nonzero: {tokens}");
    }

    // The worker claims completion on the board (todo -> doing -> done with
    // the exact type-derived gate criteria), then /api/verify proves it with
    // a typed FileExists criterion -> verified.
    let v = patch_ok(app, &sem, json!({ "status": "doing", "gate_ack": true }), "live-claude").await;
    assert_eq!(v["status"], json!("doing"), "{v}");
    let v = patch_ok(
        app,
        &sem,
        json!({
            "status": "done",
            "gate_checked": ["Implemented and merged", "Tests / lint pass"]
        }),
        "live-claude",
    )
    .await;
    assert_eq!(v["status"], json!("done"), "{v}");

    let (st, v) = send_with(
        app,
        "POST",
        &format!("/api/verify/{sem}"),
        Some(json!({
            "criteria": [{
                "description": "the model's artifact exists on disk",
                "verifier": { "kind": "file_exists", "path": out_path.to_string_lossy() },
                "required": true
            }]
        })),
        &[("X-Amux-Session", "live-claude")],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "verify: {v}");
    assert_eq!(v["verdict"]["kind"], json!("passed"), "{v}");
    assert_eq!(v["new_status"], json!("verified"), "{v}");
    let d = detail(app, &sem).await;
    assert_eq!(d["status"], json!("verified"), "{d}");
    assert!(d["last_verified_at"].is_i64(), "{d}");

    eprintln!(
        "[happy-path] PASS in {:.0}s — turn {turn_id} outcome={} tokens={} out.txt={:?} events={}",
        t0.elapsed().as_secs_f32(),
        outcome_v["outcome"],
        tokens,
        content.trim(),
        events_log.lock().unwrap().len(),
    );
}

// ===========================================================================
// RR-0046 / RR-0047 — real backend lifecycle around a REAL interactive claude
// ===========================================================================

/// Markers that only the RUNNING claude TUI paints — none of them appear in
/// the echoed spawn command line (`cd … && exec claude
/// --dangerously-skip-permissions`), so matching one proves the UI is up,
/// not that the shell echoed our keystrokes. Matched case-insensitively.
const CLAUDE_UI_MARKERS: &[&str] = &[
    "? for shortcuts",
    "welcome to claude",
    "bypass permissions",
    "esc to interrupt",
    "no, exit",
];

/// Poll capture until the claude TUI is visibly up (bounded). String errors
/// so the caller can guarantee terminate() runs first (conformance idiom).
async fn wait_for_claude_ui(
    backend: &Arc<dyn SessionBackend>,
    proc: &ProcessRef,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    let mut last_capture = String::new();
    loop {
        match backend.status(proc).await {
            Ok(BackendStatus::Running) => {}
            Ok(other) => {
                return Err(format!(
                    "claude session ended before the UI appeared (status {other:?}); \
                     last capture:\n{last_capture}"
                ))
            }
            Err(e) => return Err(format!("status probe errored while waiting for UI: {e}")),
        }
        match backend.capture(proc, 100).await {
            Ok(frame) => {
                let lower = frame.to_lowercase();
                if CLAUDE_UI_MARKERS.iter().any(|m| lower.contains(m)) {
                    return Ok(frame);
                }
                last_capture = frame;
            }
            Err(e) => last_capture = format!("<capture error: {e}>"),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "claude UI did not appear within {timeout:?}; last capture:\n{last_capture}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The generic live lifecycle (RR-0046/0047), run unchanged against both
/// real backends: spawn a REAL interactive claude -> pane shows the claude
/// UI -> ScanLoop reaches it (no protocol session, so it is scanned, not
/// demoted) -> terminate -> backend reports it gone -> reconcile marks the
/// session interrupted. Cleanup ALWAYS runs: everything between spawn and
/// terminate is a Result so a mid-lifecycle failure cannot leak a live
/// claude session into the fleet's namespace.
async fn run_live_backend_lifecycle(backend: Arc<dyn SessionBackend>, label: &str) {
    let t0 = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let store: SharedStore = Arc::new(Store::open(&dir.path().join("live.db")).unwrap());
    let ws = tempfile::tempdir().unwrap();

    let wid = WorkerId::from_ulid(ulid::Ulid::new());
    let ref_ = backend_ref(&wid);
    // Safety precondition: throwaway refs are worker-shaped and can never
    // collide with a human-named fleet session.
    assert!(
        ref_.contains("wrk_"),
        "[{label}] test ref {ref_:?} is not worker-shaped; refusing to run against a live fleet"
    );

    // Durable rows the scan + reconciliation read: a worker and its live
    // session (the scan targets live tmux/herdr sessions; reconciliation
    // compares them against backend truth).
    {
        let (wid2, ref2, backend_name) = (wid.clone(), ref_.clone(), label.to_string());
        let cwd = ws.path().to_string_lossy().into_owned();
        store
            .write(move |conn| {
                let cfg = CoreWorkerConfig {
                    display_name: format!("live-{backend_name}"),
                    name_aliases: vec![],
                    cwd: cwd.clone(),
                    provider: ProviderId::new("claude"),
                    model: None,
                    backend: if backend_name == "tmux" {
                        BackendId::tmux()
                    } else {
                        BackendId::herdr()
                    },
                    environment: Default::default(),
                    permissions: vec![],
                    group: None,
                };
                let row = WorkerRow::new(&wid2, &cfg, &Utc::now().to_rfc3339());
                queries::insert_worker(conn, &row)?;
                queries::insert_session(
                    conn,
                    &SessionRow {
                        id: format!("ses_{}", ulid::Ulid::new()),
                        worker_id: wid2.as_str().to_string(),
                        backend: backend_name.clone(),
                        backend_ref: ref2.clone(),
                        pid: None,
                        started_at: Utc::now().to_rfc3339(),
                        ended_at: None,
                        exit_reason: None,
                    },
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    // Spawn the REAL interactive claude session.
    let spec = SessionSpec {
        worker: wid.clone(),
        command: vec!["claude".into(), "--dangerously-skip-permissions".into()],
        cwd: ws.path().to_string_lossy().into_owned(),
        env: BTreeMap::from([("AMUX_GOLDEN_LIVE".to_string(), "1".to_string())]),
        human_label: None,
    };
    let proc = backend
        .spawn(&spec)
        .await
        .unwrap_or_else(|e| panic!("[{label}] spawn failed: {e}"));
    assert_eq!(proc.backend_ref, ref_, "[{label}] spawn must return the canonical ref");

    // Everything until terminate is collected, never panicked (guard idiom).
    let mid: Result<(String, Vec<String>, usize), String> = async {
        let frame = wait_for_claude_ui(&backend, &proc, Duration::from_secs(60)).await?;
        let ui_at = t0.elapsed().as_secs_f32();
        eprintln!("[{label}] claude UI up at t+{ui_at:.1}s");

        // The scan loop: this worker has NO structured protocol session, so
        // it must be SCANNED (the scraper is its only voice), not demoted.
        let scan = ScanLoop::new(store.clone(), vec![backend.clone()], None);
        let report = scan
            .scan_once()
            .await
            .map_err(|e| format!("scan_once errored: {e}"))?;
        if !report.scanned.iter().any(|s| s == wid.as_str()) {
            return Err(format!(
                "scan did not reach the worker: scanned={:?} demoted={:?} failures={:?}",
                report.scanned, report.demoted_structured, report.capture_failures
            ));
        }
        if !report.demoted_structured.is_empty() {
            return Err(format!(
                "nothing should be demoted (no protocol wired): {:?}",
                report.demoted_structured
            ));
        }
        Ok((frame, report.scanned.clone(), report.events_applied))
    }
    .await;

    // SAFETY GUARD: never point a kill at anything that is not one of our
    // throwaway workers — checked on the exact value passed to the call.
    assert!(
        proc.backend_ref.contains("wrk_"),
        "[{label}] REFUSING terminate: {:?} is not a throwaway ref",
        proc.backend_ref
    );
    let term = backend.terminate(&proc).await;

    let (frame, scanned, scan_events) = match mid {
        Ok(ok) => ok,
        Err(msg) => panic!("[{label}] {msg}"),
    };
    term.unwrap_or_else(|e| panic!("[{label}] terminate failed: {e}"));

    // The host must report the session gone (NotFound or an honest corpse).
    match backend.status(&proc).await {
        Ok(BackendStatus::NotFound) | Ok(BackendStatus::Completed { .. }) => {}
        Ok(other) => panic!("[{label}] status after terminate: expected NotFound/Completed, got {other:?}"),
        Err(e) => panic!("[{label}] status after terminate errored: {e}"),
    }

    // Startup reconciliation: DB says the session is live, the backend says
    // it is gone -> marked interrupted. The live fleet's own amux-* sessions
    // surface as stale_backend — READ ONLY, reported by count, never touched.
    let rt = runtime(store.clone(), None, vec![backend.clone()]);
    let report = rt
        .reconcile_on_startup()
        .await
        .unwrap_or_else(|e| panic!("[{label}] reconcile_on_startup failed: {e}"));
    assert!(
        report.backend_probe_failures.is_empty(),
        "[{label}] backend probe failed — interruption not judged: {:?}",
        report.backend_probe_failures
    );
    assert!(
        report.interrupted.iter().any(|w| w == wid.as_str()),
        "[{label}] reconciliation did not mark our vanished session interrupted: {report:?}"
    );
    let (ended, exit_reason): (Option<String>, Option<String>) = {
        let conn = store.read().unwrap();
        conn.query_row(
            "SELECT ended_at, exit_reason FROM _amux_sessions WHERE worker_id = ?1",
            params![wid.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert!(ended.is_some(), "[{label}] session row must be ended after reconcile");

    eprintln!(
        "[{label}] PASS in {:.0}s — ref {ref_}; scanned={scanned:?} (scan events applied: \
         {scan_events}); reconcile: interrupted={:?}, stale_backend fleet sessions seen (untouched): {}; \
         exit_reason={exit_reason:?}; final UI frame tail:\n{}",
        t0.elapsed().as_secs_f32(),
        report.interrupted,
        report.stale_backend.len(),
        last_lines(&frame, 10),
    );
}

#[tokio::test]
#[ignore = "spawns a REAL interactive claude under tmux; locally: cargo test -p amux-server --test golden_live -- --ignored --nocapture"]
async fn golden_live_backend_lifecycle_tmux() {
    if !have_claude() {
        eprintln!("SKIPPED: golden_live_backend_lifecycle_tmux — `claude` not on PATH; the tmux live lifecycle was NOT tested");
        return;
    }
    if !binary_available("tmux", "-V").await {
        eprintln!("SKIPPED: golden_live_backend_lifecycle_tmux — `tmux` not found on PATH; the tmux live lifecycle was NOT tested");
        return;
    }
    run_live_backend_lifecycle(Arc::new(TmuxBackend::new()), "tmux").await;
}

#[tokio::test]
#[ignore = "spawns a REAL interactive claude under herdr; locally: cargo test -p amux-server --test golden_live -- --ignored --nocapture"]
async fn golden_live_backend_lifecycle_herdr() {
    if !have_claude() {
        eprintln!("SKIPPED: golden_live_backend_lifecycle_herdr — `claude` not on PATH; the herdr live lifecycle was NOT tested");
        return;
    }
    if !binary_available("herdr", "--version").await {
        eprintln!("SKIPPED: golden_live_backend_lifecycle_herdr — `herdr` not found on PATH; the herdr live lifecycle was NOT tested");
        return;
    }
    let Some(session) = running_herdr_session().await else {
        eprintln!(
            "SKIPPED: golden_live_backend_lifecycle_herdr — herdr is installed but no herdr \
             server session is running (start `herdr --session amux`); the herdr live \
             lifecycle was NOT tested"
        );
        return;
    };
    run_live_backend_lifecycle(Arc::new(HerdrBackend::new(session)), "herdr").await;
}
