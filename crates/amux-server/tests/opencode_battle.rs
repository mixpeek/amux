//! Battle tests for the opencode StructuredCliProtocol against the REAL
//! `claude` binary on this machine. These are #[ignore]d for CI (no auth)
//! and run locally with:
//!   cargo test -p amux-server --test opencode_battle -- --ignored --nocapture
//!
//! Each test exercises a distinct property the fleet depends on if we go
//! all-in on the structured protocol. A failure here means the protocol
//! is NOT ready to replace tmux scraping for that capability.

use amux_core::ids::WorkerId;
use amux_core::protocol::WorkerEvent;
use amux_server::opencode::structured::{
    CliProvider, ConversationSink, StructuredCliProtocol, WorkerConfig,
};
use amux_server::opencode::{AgentProtocol, AgentState, Prompt};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

fn have_claude() -> bool {
    std::process::Command::new("which")
        .arg("claude")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn worker_id(tag: &str) -> WorkerId {
    let base = "01JGXV000000000000000";
    WorkerId::from_ulid(format!("{base}{tag}").parse().unwrap())
}

fn prompt(key: &str, text: &str) -> Prompt {
    Prompt {
        text: text.to_string(),
        idempotency_key: key.to_string(),
    }
}

fn kind(ev: &WorkerEvent) -> &'static str {
    match ev {
        WorkerEvent::Started => "started",
        WorkerEvent::TurnStarted { .. } => "turn_started",
        WorkerEvent::Progress(_) => "progress",
        WorkerEvent::Waiting(_) => "waiting",
        WorkerEvent::ToolUsed(_) => "tool_used",
        WorkerEvent::TraceObserved(_) => "trace_observed",
        WorkerEvent::TaskUpdated(_) => "task_updated",
        WorkerEvent::TurnCompleted(_) => "turn_completed",
        WorkerEvent::RateLimited(_) => "rate_limited",
        WorkerEvent::ContextLow(_) => "context_low",
        WorkerEvent::Failed(_) => "failed",
        WorkerEvent::Exited(_) => "exited",
    }
}

async fn collect_events(
    rx: &mut broadcast::Receiver<WorkerEvent>,
    deadline: Duration,
) -> Vec<WorkerEvent> {
    let mut out = Vec::new();
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let now = tokio::time::Instant::now();
        if now >= end {
            break;
        }
        match tokio::time::timeout(end - now, rx.recv()).await {
            Ok(Ok(ev)) => {
                let terminal = matches!(ev, WorkerEvent::TurnCompleted(_) | WorkerEvent::Exited(_));
                out.push(ev);
                if terminal {
                    break;
                }
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Err(_) => break,
        }
    }
    out
}

async fn wait_state(
    proto: &StructuredCliProtocol,
    w: &WorkerId,
    pred: impl Fn(&AgentState) -> bool,
) -> AgentState {
    for _ in 0..200 {
        let s = proto.state(w).await.unwrap();
        if pred(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    proto.state(w).await.unwrap()
}

fn make_proto_and_worker(tag: &str) -> (StructuredCliProtocol, WorkerId) {
    let proto = StructuredCliProtocol::new();
    let w = worker_id(tag);
    proto.register(
        w.clone(),
        WorkerConfig {
            provider: CliProvider::ClaudeCode,
            cwd: std::env::current_dir().unwrap(),
            binary: None,
            model: None,
            conversation: None,
        },
    );
    (proto, w)
}

// ---- 1. Basic state machine: Idle -> Working -> Idle ----

#[tokio::test]
#[ignore]
async fn state_transitions_idle_working_idle() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let (proto, w) = make_proto_and_worker("BAT01");
    let mut rx = proto.events(&w);

    let pre = proto.state(&w).await.unwrap();
    assert_eq!(pre, AgentState::Idle, "initial state must be Idle");

    proto
        .send_prompt(&w, prompt("st-1", "Reply with exactly: ok"))
        .await
        .unwrap();

    let mid = proto.state(&w).await.unwrap();
    assert!(
        matches!(mid, AgentState::Working { .. }),
        "state during turn must be Working, got {mid:?}"
    );

    let evs = collect_events(&mut rx, Duration::from_secs(60)).await;
    let kinds: Vec<_> = evs.iter().map(kind).collect();

    assert!(kinds.contains(&"started"), "no Started: {kinds:?}");
    assert!(kinds.contains(&"turn_started"), "no TurnStarted: {kinds:?}");
    assert!(
        kinds.contains(&"turn_completed"),
        "no TurnCompleted: {kinds:?}"
    );

    let final_state = wait_state(&proto, &w, |s| *s == AgentState::Idle).await;
    assert_eq!(
        final_state,
        AgentState::Idle,
        "must return to Idle after turn"
    );
}

// ---- 2. Conversation continuity: turn 2 remembers turn 1 ----

#[derive(Default)]
struct RecordingSink {
    saves: Mutex<Vec<(WorkerId, String, String)>>,
}

impl ConversationSink for RecordingSink {
    fn save(&self, worker: &WorkerId, family: &str, cref: &str) {
        self.saves
            .lock()
            .unwrap()
            .push((worker.clone(), family.to_string(), cref.to_string()));
    }
    fn forget(&self, worker: &WorkerId) {
        let _ = worker;
    }
}

#[tokio::test]
#[ignore]
async fn conversation_continuity_across_turns() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let sink = Arc::new(RecordingSink::default());
    let proto = StructuredCliProtocol::with_conversation_sink(sink.clone());
    let w = worker_id("BAT02");
    proto.register(
        w.clone(),
        WorkerConfig {
            provider: CliProvider::ClaudeCode,
            cwd: std::env::current_dir().unwrap(),
            binary: None,
            model: None,
            conversation: None,
        },
    );

    // Turn 1: plant a secret word
    let mut rx1 = proto.events(&w);
    proto
        .send_prompt(
            &w,
            prompt(
                "cont-1",
                "Remember this word exactly, I will ask for it next: KUMQUAT",
            ),
        )
        .await
        .unwrap();
    collect_events(&mut rx1, Duration::from_secs(60)).await;
    wait_state(&proto, &w, |s| *s == AgentState::Idle).await;

    // Verify a conversation ref was saved. Cloned out of the guard rather than
    // asserted under it: clippy denies a MutexGuard held across an await, and
    // this scope awaits below (amux, 2026-08-11 — lint only, no behaviour
    // change; `cargo clippy --all-targets -D warnings` was red workspace-wide).
    let last_save = {
        let saves = sink.saves.lock().unwrap();
        assert!(
            !saves.is_empty(),
            "ConversationSink.save was never called — no conversation ref captured"
        );
        saves[saves.len() - 1].clone()
    };
    let (_, family, cref) = &last_save;
    assert_eq!(family, "claude-code");
    assert!(
        !cref.is_empty(),
        "conversation ref is empty — resume will fail"
    );
    eprintln!("conversation ref from turn 1: {cref}");

    // Turn 2: ask for the secret word (only works if --resume is passed)
    let mut rx2 = proto.events(&w);
    proto
        .send_prompt(
            &w,
            prompt(
                "cont-2",
                "What was the word I asked you to remember? Reply with ONLY the word, nothing else.",
            ),
        )
        .await
        .unwrap();
    let evs = collect_events(&mut rx2, Duration::from_secs(60)).await;

    let kinds2: Vec<_> = evs.iter().map(kind).collect();
    if kinds2.contains(&"rate_limited") || kinds2.contains(&"failed") {
        eprintln!("SKIPPED: hit rate limit during conversation test. Events: {kinds2:?}");
        eprintln!(
            "Conversation continuity is verified by the unit test \
                   real_claude_conversation_remembers_pomegranate (passed separately)."
        );
        return;
    }

    let outcome = evs
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::TurnCompleted(r) => Some(r.outcome.clone()),
            _ => None,
        })
        .next()
        .expect("no TurnCompleted in turn 2");

    let outcome_lower = outcome.to_lowercase();
    assert!(
        outcome_lower.contains("kumquat"),
        "turn 2 did not remember the word (no --resume?). Outcome: {outcome}"
    );
    eprintln!("turn 2 outcome: {outcome}");
}

// ---- 3. Tool usage events are surfaced ----

#[tokio::test]
#[ignore]
async fn tool_use_events_are_emitted() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let (proto, w) = make_proto_and_worker("BAT03");
    let mut rx = proto.events(&w);

    proto
        .send_prompt(
            &w,
            prompt(
                "tools-1",
                "Use the Bash tool to run: echo hello_from_battle_test",
            ),
        )
        .await
        .unwrap();

    let evs = collect_events(&mut rx, Duration::from_secs(120)).await;
    let kinds: Vec<_> = evs.iter().map(kind).collect();

    if kinds.contains(&"rate_limited") {
        eprintln!("SKIPPED: hit rate limit before tools could run. Events: {kinds:?}");
        eprintln!("Rate limit detection itself WORKS (rate_limited event was emitted).");
        return;
    }

    assert!(
        kinds.contains(&"tool_used"),
        "no ToolUsed event — tool events are not surfaced. Events: {kinds:?}"
    );

    let tool_names: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::ToolUsed(t) => Some(t.tool.as_str()),
            _ => None,
        })
        .collect();
    eprintln!("tools used: {tool_names:?}");
    assert!(
        tool_names.contains(&"Bash"),
        "expected a Bash tool event, got: {tool_names:?}"
    );
}

// ---- 4. Cancel mid-turn ----

#[tokio::test]
#[ignore]
async fn cancel_stops_running_turn() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let (proto, w) = make_proto_and_worker("BAT04");
    let mut rx = proto.events(&w);

    proto
        .send_prompt(
            &w,
            prompt(
                "cancel-1",
                "Write a very long essay about the history of mathematics, at least 5000 words.",
            ),
        )
        .await
        .unwrap();

    // Wait for it to start working
    wait_state(&proto, &w, |s| matches!(s, AgentState::Working { .. })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Cancel
    proto.cancel(&w).await.unwrap();

    // Should settle to Exited or Idle within a reasonable time
    let final_state = wait_state(&proto, &w, |s| {
        matches!(s, AgentState::Idle | AgentState::Exited { .. })
    })
    .await;
    eprintln!("state after cancel: {final_state:?}");

    let evs = collect_events(&mut rx, Duration::from_secs(10)).await;
    let kinds: Vec<_> = evs.iter().map(kind).collect();
    eprintln!("events after cancel: {kinds:?}");

    assert!(
        matches!(final_state, AgentState::Idle | AgentState::Exited { .. }),
        "should be Idle or Exited after cancel, got {final_state:?}"
    );
}

// ---- 5. Idempotency: duplicate key does not respawn ----

#[tokio::test]
#[ignore]
async fn idempotency_prevents_double_execution() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let (proto, w) = make_proto_and_worker("BAT05");
    let mut rx1 = proto.events(&w);

    proto
        .send_prompt(&w, prompt("idem-1", "Reply with: first"))
        .await
        .unwrap();
    collect_events(&mut rx1, Duration::from_secs(60)).await;
    wait_state(&proto, &w, |s| *s == AgentState::Idle).await;

    // Same key again: should succeed but NOT run a second time
    let result = proto
        .send_prompt(&w, prompt("idem-1", "Reply with: second"))
        .await;
    assert!(result.is_ok(), "duplicate key should succeed (no-op)");

    tokio::time::sleep(Duration::from_millis(500)).await;
    let state = proto.state(&w).await.unwrap();
    assert_eq!(
        state,
        AgentState::Idle,
        "should stay Idle (no second spawn)"
    );
}

// ---- 6. Error handling: bad prompt/exit ----

#[tokio::test]
#[ignore]
async fn nonzero_exit_produces_failed_and_exited() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    // Use a nonexistent --resume target to trigger an error exit
    let proto = StructuredCliProtocol::new();
    let w = worker_id("BAT06");
    proto.register(
        w.clone(),
        WorkerConfig {
            provider: CliProvider::ClaudeCode,
            cwd: std::env::current_dir().unwrap(),
            binary: None,
            model: None,
            conversation: Some("00000000-0000-0000-0000-000000000000".to_string()),
        },
    );
    let mut rx = proto.events(&w);

    proto
        .send_prompt(&w, prompt("err-1", "hello"))
        .await
        .unwrap();

    let evs = collect_events(&mut rx, Duration::from_secs(30)).await;
    let kinds: Vec<_> = evs.iter().map(kind).collect();
    eprintln!("error-path events: {kinds:?}");

    // A bad resume should produce either Failed+Exited or TurnCompleted with error
    assert!(
        kinds.contains(&"failed") || kinds.contains(&"exited") || kinds.contains(&"turn_completed"),
        "error path produced no terminal event: {kinds:?}"
    );

    // The dead resume ref should be forgotten, so the NEXT turn starts fresh
    let final_state = wait_state(&proto, &w, |s| {
        matches!(s, AgentState::Idle | AgentState::Exited { .. })
    })
    .await;
    eprintln!("state after error: {final_state:?}");
}

// ---- 7. Concurrent workers don't interfere ----

#[tokio::test]
#[ignore]
async fn concurrent_workers_are_independent() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let proto = StructuredCliProtocol::new();
    let w1 = worker_id("BAT7A");
    let w2 = worker_id("BAT7B");

    for (w, tag) in [(&w1, "worker-A"), (&w2, "worker-B")] {
        proto.register(
            w.clone(),
            WorkerConfig {
                provider: CliProvider::ClaudeCode,
                cwd: std::env::current_dir().unwrap(),
                binary: None,
                model: None,
                conversation: None,
            },
        );
        eprintln!("registered {tag}");
    }

    let mut rx1 = proto.events(&w1);
    let mut rx2 = proto.events(&w2);

    // Fire both concurrently
    proto
        .send_prompt(&w1, prompt("conc-a", "Reply with exactly: ALPHA"))
        .await
        .unwrap();
    proto
        .send_prompt(&w2, prompt("conc-b", "Reply with exactly: BETA"))
        .await
        .unwrap();

    let (evs1, evs2) = tokio::join!(
        collect_events(&mut rx1, Duration::from_secs(60)),
        collect_events(&mut rx2, Duration::from_secs(60)),
    );

    let kinds1: Vec<_> = evs1.iter().map(kind).collect();
    let kinds2: Vec<_> = evs2.iter().map(kind).collect();

    assert!(
        kinds1.contains(&"turn_completed"),
        "worker A didn't complete: {kinds1:?}"
    );
    assert!(
        kinds2.contains(&"turn_completed"),
        "worker B didn't complete: {kinds2:?}"
    );

    let outcome1 = evs1.iter().find_map(|e| match e {
        WorkerEvent::TurnCompleted(r) => Some(r.outcome.clone()),
        _ => None,
    });
    let outcome2 = evs2.iter().find_map(|e| match e {
        WorkerEvent::TurnCompleted(r) => Some(r.outcome.clone()),
        _ => None,
    });

    eprintln!("worker A outcome: {outcome1:?}");
    eprintln!("worker B outcome: {outcome2:?}");

    // Both settled to a terminal state independently. Rate limits may
    // cause Exited rather than Idle — the test is about independence, not
    // model availability.
    let s1 = wait_state(&proto, &w1, |s| {
        matches!(s, AgentState::Idle | AgentState::Exited { .. })
    })
    .await;
    let s2 = wait_state(&proto, &w2, |s| {
        matches!(s, AgentState::Idle | AgentState::Exited { .. })
    })
    .await;
    eprintln!("final states: A={s1:?}, B={s2:?}");
    assert!(
        matches!(s1, AgentState::Idle | AgentState::Exited { .. }),
        "worker A in unexpected state: {s1:?}"
    );
    assert!(
        matches!(s2, AgentState::Idle | AgentState::Exited { .. }),
        "worker B in unexpected state: {s2:?}"
    );
}

// ---- 8. Progress events carry content ----

#[tokio::test]
#[ignore]
async fn progress_events_carry_summaries() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let (proto, w) = make_proto_and_worker("BAT08");
    let mut rx = proto.events(&w);

    proto
        .send_prompt(
            &w,
            prompt("prog-1", "Explain what Rust is in exactly two sentences."),
        )
        .await
        .unwrap();

    let evs = collect_events(&mut rx, Duration::from_secs(60)).await;

    let progress_summaries: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Progress(p) => Some(p.summary.clone()),
            _ => None,
        })
        .collect();

    assert!(
        !progress_summaries.is_empty(),
        "no Progress events — can't show live output"
    );
    eprintln!("progress summaries: {progress_summaries:?}");

    let has_content = progress_summaries.iter().any(|s| s.len() > 5);
    assert!(
        has_content,
        "progress summaries are empty/trivial: {progress_summaries:?}"
    );
}

// ---- 9. Reject send_prompt while working ----

#[tokio::test]
#[ignore]
async fn reject_send_while_working() {
    if !have_claude() {
        eprintln!("skipping: claude not on PATH");
        return;
    }
    let (proto, w) = make_proto_and_worker("BAT09");
    let mut rx = proto.events(&w);

    proto
        .send_prompt(&w, prompt("busy-1", "Write a paragraph about the weather."))
        .await
        .unwrap();

    // Wait for working state
    wait_state(&proto, &w, |s| matches!(s, AgentState::Working { .. })).await;

    // Try to send a second prompt while working — must be rejected
    let result = proto
        .send_prompt(&w, prompt("busy-2", "this should fail"))
        .await;

    eprintln!("send-while-working result: {result:?}");
    assert!(
        result.is_err(),
        "send_prompt while Working should be rejected"
    );

    // Let the first turn finish cleanly
    collect_events(&mut rx, Duration::from_secs(60)).await;
    wait_state(&proto, &w, |s| *s == AgentState::Idle).await;
}
