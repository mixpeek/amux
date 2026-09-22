//! Durable memory re-supply across the agent lifecycle (#73).
//!
//! Assignment snapshots (`orchestrator::context::assemble_context`) already
//! carry memories into the first turn of a task. Three paths start (or
//! continue) a provider conversation WITHOUT a snapshot:
//!
//! - **Session start**: a fresh agent process with no persisted conversation
//!   ref and no assignment yet.
//! - **Resume**: `protocol.resume()` only unpauses; when the conversation ref
//!   was lost (server restart, dropped ref) the next turn starts fresh and
//!   memoryless.
//! - **Post-compaction**: native compaction is provider-managed and
//!   unobservable from here (AMUX-4366 deliberately sends no compact
//!   reminders from amux). A `ContextLow` reading means the next turn
//!   boundary is the earliest point durable facts can be re-asserted after
//!   the provider summarized them away.
//!
//! The vehicle is a reference-only `WorkerCommand::MemoryResupply`
//! (Invariant 29: the command carries the trigger reason, never rendered
//! text). Visible memories are resolved LIVE at delivery from the canonical
//! store through the ONE visibility predicate, so a queued resupply can
//! never deliver a stale copy, and idempotency keys make re-enqueue safe
//! (Invariant 9). Delivery rides `AtTurnBoundary`, which the docs on
//! `DeliveryTiming` already name as the timing for context refresh, so a
//! resupply never interrupts an active turn, and an empty memory set
//! delivers NO turn at all.
//!
//! What this module does NOT do, on purpose: no second store (the
//! `memory_entries` table stays canonical), no mission object (#69 was
//! declined), no transcript replay, and no new context composer.
//! Rendering is `context::memory_snapshot`, the same builder assignment uses.

use crate::db::commands;
use crate::opencode::Prompt;
use amux_core::ids::{CommandId, WorkerId};
use amux_core::protocol::{DeliveryTiming, MemoryResupplyReason, QueuedCommand, WorkerCommand};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

/// Default cap for one re-supply turn. Small on purpose: a post-compaction
/// re-supply fires exactly when context is scarcest, so it must stay a
/// reminder of durable facts, never a second transcript. Override with
/// `AMUX_MEMORY_RESUPPLY_MAX_CHARS`.
pub const DEFAULT_RESUPPLY_MAX_CHARS: usize = 4_000;

/// Idempotency scope: one live resupply per (worker, reason). A second
/// trigger with the same reason reuses the queued row instead of stacking
/// another turn.
pub fn resupply_key(worker: &WorkerId, reason: MemoryResupplyReason) -> String {
    let reason = match reason {
        MemoryResupplyReason::SessionStart => "start",
        MemoryResupplyReason::Resume => "resume",
        MemoryResupplyReason::PostCompaction => "post-compaction",
    };
    format!("memory-resupply:{}:{reason}", worker.as_str())
}

/// Whether the worker has a persisted provider conversation to continue.
/// Best-effort read: a lookup failure answers "no" (a fresh conversation
/// is always a safe start), it never blocks the caller.
pub fn has_conversation_ref(conn: &Connection, worker: &WorkerId) -> bool {
    conn.query_row(
        "SELECT 1 FROM _amux_conversations WHERE worker_id = ?1",
        params![worker.as_str()],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .unwrap_or(false)
}

/// Queue a re-supply for `worker`. Returns whether a row was NEWLY queued
/// (`false` = an active resupply for this reason already exists, nothing
/// stacked). A TERMINAL row for the same key is replaced: a fresh trigger
/// after a completed delivery means the agent is starting over again, so
/// re-supplying is correct, while an in-flight row (queued, dispatched,
/// delivered-unconfirmed, or failing with budget left) is left alone so a
/// trigger burst cannot stack turns.
pub fn enqueue_resupply(
    conn: &Connection,
    worker: &WorkerId,
    reason: MemoryResupplyReason,
    now: DateTime<Utc>,
) -> rusqlite::Result<bool> {
    let key = resupply_key(worker, reason);
    if let Some(existing) = commands::by_idempotency_key(conn, worker, &key)? {
        if !existing.state.is_terminal() {
            return Ok(false);
        }
        conn.execute(
            "DELETE FROM _amux_commands WHERE id = ?1",
            params![existing.id.as_str()],
        )?;
    }
    let id = CommandId::from_ulid(ulid::Ulid::new());
    let (_, created) = commands::enqueue(
        conn,
        id,
        worker,
        &WorkerCommand::MemoryResupply { reason },
        &key,
        &DeliveryTiming::AtTurnBoundary,
        None,
        now,
    )?;
    Ok(created)
}

/// Prepare one queued re-supply for provider delivery. Fully synchronous:
/// resolves the worker's CURRENT visible memories (never enqueue-time
/// state), renders them with the shared bounded builder, and returns the
/// prompt the provider would receive, or `None` for an honest no-op (no
/// memories visible, or a start/resume whose conversation already
/// exists). The caller sends the prompt itself, AFTER the store borrow
/// ends (`Connection` is not `Sync`, so no borrow may cross the send;
/// the same scoping the ExecuteTask arm uses).
pub fn prepare_resupply(
    conn: &Connection,
    cmd: &QueuedCommand,
    max_chars: usize,
) -> Result<Option<Prompt>, crate::opencode::ProtocolError> {
    let WorkerCommand::MemoryResupply { reason } = &cmd.command else {
        return Err(crate::opencode::ProtocolError::Transport(format!(
            "prepare_resupply called for non-resupply command {}",
            cmd.id.as_str()
        )));
    };
    // Start/resume re-supply exists for a conversation that does not
    // continue one. If a ref appeared between enqueue and delivery (an
    // assignment snapshot turn ran first), the agent already has its
    // memories in-conversation, so restating them adds a turn for nothing.
    // Post-compaction always re-asserts: summarization is exactly what it
    // repairs.
    if !matches!(reason, MemoryResupplyReason::PostCompaction)
        && has_conversation_ref(conn, &cmd.worker)
    {
        return Ok(None);
    }
    let snapshot =
        crate::orchestrator::context::memory_snapshot(conn, &cmd.worker, max_chars).map_err(
            |e| crate::opencode::ProtocolError::Transport(format!("memory snapshot failed: {e}")),
        )?;
    Ok(snapshot.map(|s| Prompt {
        text: crate::orchestrator::context::render_snapshot(&s),
        idempotency_key: cmd.idempotency_key.clone(),
    }))
}

/// Read the configured re-supply cap. Runtime calls this; tests pass
/// explicit budgets so they never depend on process env.
pub fn resupply_max_chars() -> usize {
    std::env::var("AMUX_MEMORY_RESUPPLY_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_RESUPPLY_MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::mock::MockProtocol;
    use crate::opencode::{AgentProtocol, AgentState};
    use amux_core::ids::{GroupId, MemoryId};
    use amux_core::memory::{MemoryEntry, MemoryProvenance, MemoryType};
    use amux_core::scope::Scope;
    use std::sync::Arc;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    fn wid(n: u64) -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
    }

    fn gid(n: u64) -> GroupId {
        GroupId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
    }

    fn mid(n: u64) -> MemoryId {
        MemoryId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
    }

    fn t0() -> DateTime<Utc> {
        "2026-08-01T00:00:00Z".parse().unwrap()
    }

    fn seed(
        c: &Connection,
        n: u64,
        scope: Scope,
        name: &str,
        content: &str,
        provenance: MemoryProvenance,
    ) {
        let e = MemoryEntry::new(mid(n), scope, name, content, MemoryType::Project, provenance, t0());
        crate::db::memories::insert(c, &e).unwrap();
    }

    fn seed_worker_in_group(c: &Connection, w: &WorkerId, g: &GroupId) {
        c.execute(
            "INSERT INTO _amux_workers (id, display_name, group_id, created_at, updated_at)
             VALUES (?1, 'w', ?2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            params![w.as_str(), g.as_str()],
        )
        .unwrap();
    }

    fn save_conversation_ref(c: &Connection, w: &WorkerId, provider: &str, cref: &str) {
        c.execute(
            "INSERT INTO _amux_conversations (worker_id, provider, conversation_ref, updated_at)
             VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z')",
            params![w.as_str(), provider, cref],
        )
        .unwrap();
    }

    fn queued_resupply(c: &Connection, w: &WorkerId, reason: MemoryResupplyReason) -> QueuedCommand {
        assert!(enqueue_resupply(c, w, reason, Utc::now()).unwrap());
        commands::by_idempotency_key(c, w, &resupply_key(w, reason))
            .unwrap()
            .expect("row just queued")
    }

    fn send_prompts(protocol: &MockProtocol) -> Vec<(WorkerId, Prompt)> {
        protocol
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                crate::opencode::mock::RecordedCall::SendPrompt { worker, prompt } => {
                    Some((worker, prompt))
                }
                _ => None,
            })
            .collect()
    }

    /// Drive delivery exactly the way the pump arm does (prepare under the
    /// store borrow, send after it ends) and report whether a
    /// provider-facing turn went out.
    async fn pump_one(
        c: &Connection,
        protocol: &Arc<MockProtocol>,
        cmd: &QueuedCommand,
        max_chars: usize,
    ) -> bool {
        let prepared =
            prepare_resupply(c, cmd, max_chars).expect("prepare must not error in these tests");
        match prepared {
            Some(prompt) => {
                protocol
                    .send_prompt(&cmd.worker, prompt)
                    .await
                    .expect("mock send never fails for a registered worker");
                true
            }
            None => false,
        }
    }

    #[tokio::test]
    async fn session_start_delivers_visible_memories_provider_facing() {
        let c = conn();
        let (w, g) = (wid(1), gid(2));
        seed_worker_in_group(&c, &w, &g);
        seed(&c, 10, Scope::Global, "house-rules", "no deploys friday", MemoryProvenance::HumanWritten);
        seed(&c, 11, Scope::Group { id: g.clone() }, "team-notes", "rotate creds", MemoryProvenance::HumanWritten);
        seed(&c, 12, Scope::Worker { id: w.clone() }, "my-notes", "auth in src/auth.rs",
            MemoryProvenance::WorkerWritten { worker: w.clone() });
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        let cmd = queued_resupply(&c, &w, MemoryResupplyReason::SessionStart);
        let sent = pump_one(&c, &protocol, &cmd, 4_000).await;
        assert!(sent);

        let prompts = send_prompts(&protocol);
        assert_eq!(prompts.len(), 1, "exactly one provider-facing turn");
        assert_eq!(prompts[0].0, w);
        let text = &prompts[0].1.text;
        assert!(text.contains("house-rules") && text.contains("no deploys friday"), "{text}");
        assert!(text.contains("team-notes") && text.contains("rotate creds"), "{text}");
        assert!(text.contains("my-notes") && text.contains("auth in src/auth.rs"), "{text}");
        // Provenance rides along; idempotency key is the command key.
        assert!(text.contains("worker_written"), "{text}");
        assert_eq!(prompts[0].1.idempotency_key, cmd.idempotency_key);
    }

    #[tokio::test]
    async fn resume_after_ref_loss_delivers_current_memories() {
        let c = conn();
        let w = wid(1);
        seed(&c, 10, Scope::Global, "house-rules", "no deploys friday", MemoryProvenance::HumanWritten);
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        // No conversation ref: the resume continues nothing, so memories go out.
        let cmd = queued_resupply(&c, &w, MemoryResupplyReason::Resume);
        let sent = pump_one(&c, &protocol, &cmd, 4_000).await;
        assert!(sent);
        assert_eq!(send_prompts(&protocol).len(), 1);
    }

    #[tokio::test]
    async fn post_compaction_reasserts_even_with_live_conversation() {
        let c = conn();
        let w = wid(1);
        seed(&c, 10, Scope::Global, "decision", "we chose sqlite", MemoryProvenance::HumanWritten);
        save_conversation_ref(&c, &w, "claude", "conv-live-1");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        let cmd = queued_resupply(&c, &w, MemoryResupplyReason::PostCompaction);
        let sent = pump_one(&c, &protocol, &cmd, 4_000).await;
        assert!(sent, "re-assertion is the point of post-compaction");
        let prompts = send_prompts(&protocol);
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].1.text.contains("we chose sqlite"));
    }

    #[tokio::test]
    async fn start_skips_when_conversation_already_exists() {
        let c = conn();
        let w = wid(1);
        seed(&c, 10, Scope::Global, "decision", "we chose sqlite", MemoryProvenance::HumanWritten);
        save_conversation_ref(&c, &w, "claude", "conv-live-1");
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        for reason in [MemoryResupplyReason::SessionStart, MemoryResupplyReason::Resume] {
            let cmd = queued_resupply(&c, &w, reason);
            let sent = pump_one(&c, &protocol, &cmd, 4_000).await;
            assert!(!sent, "{reason:?} with a live conversation sends nothing");
        }
        assert!(send_prompts(&protocol).is_empty(), "no provider turn at all");
    }

    #[tokio::test]
    async fn empty_memory_set_delivers_no_turn_for_any_reason() {
        let c = conn();
        let w = wid(1);
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        for reason in [
            MemoryResupplyReason::SessionStart,
            MemoryResupplyReason::Resume,
            MemoryResupplyReason::PostCompaction,
        ] {
            let cmd = queued_resupply(&c, &w, reason);
            let sent = pump_one(&c, &protocol, &cmd, 4_000).await;
            assert!(!sent, "{reason:?} with no memories sends nothing");
        }
        assert!(send_prompts(&protocol).is_empty());
    }

    #[tokio::test]
    async fn scope_isolation_group_and_worker_boundaries_hold() {
        let c = conn();
        let (a, b, g) = (wid(1), wid(2), gid(7));
        seed_worker_in_group(&c, &a, &g);
        seed(&c, 1, Scope::Worker { id: a.clone() }, "a-private", "a secret", MemoryProvenance::HumanWritten);
        seed(&c, 2, Scope::Worker { id: b.clone() }, "b-private", "b secret", MemoryProvenance::HumanWritten);
        seed(&c, 3, Scope::Group { id: g.clone() }, "team", "shared plan", MemoryProvenance::HumanWritten);
        seed(&c, 4, Scope::Global, "all", "everyone reads", MemoryProvenance::HumanWritten);
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(a.clone(), AgentState::Idle);
        protocol.register(b.clone(), AgentState::Idle);

        let cmd_a = queued_resupply(&c, &a, MemoryResupplyReason::SessionStart);
        pump_one(&c, &protocol, &cmd_a, 8_000).await;
        let cmd_b = queued_resupply(&c, &b, MemoryResupplyReason::SessionStart);
        pump_one(&c, &protocol, &cmd_b, 8_000).await;

        let prompts = send_prompts(&protocol);
        assert_eq!(prompts.len(), 2);
        let text_a = &prompts.iter().find(|(w, _)| w == &a).unwrap().1.text;
        assert!(text_a.contains("a secret") && text_a.contains("shared plan") && text_a.contains("everyone reads"));
        assert!(!text_a.contains("b secret"), "A must never see B's worker memory: {text_a}");
        let text_b = &prompts.iter().find(|(w, _)| w == &b).unwrap().1.text;
        assert!(text_b.contains("b secret") && text_b.contains("everyone reads"));
        assert!(!text_b.contains("a secret"), "{text_b}");
        assert!(!text_b.contains("shared plan"), "B is in no group: {text_b}");
    }

    #[tokio::test]
    async fn bounds_are_deterministic_with_omission_receipt() {
        let c = conn();
        let w = wid(1);
        for n in 0..10u64 {
            seed(&c, 100 + n, Scope::Global, &format!("fact-{n:02}"), &"x".repeat(500), MemoryProvenance::HumanWritten);
        }
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        let cmd = queued_resupply(&c, &w, MemoryResupplyReason::SessionStart);
        pump_one(&c, &protocol, &cmd, 1_000).await;
        let first = send_prompts(&protocol).pop().unwrap().1.text;
        assert!(first.chars().count() <= 1_000, "hard bound holds");
        assert!(first.contains("omitted by context budget"), "omission receipt: {first}");

        // Same store state renders byte-identical text: deterministic selection.
        let again = crate::orchestrator::context::memory_snapshot(&c, &w, 1_000)
            .unwrap()
            .expect("memories exist");
        let again_text = crate::orchestrator::context::render_snapshot(&again);
        assert_eq!(first, again_text);
    }

    #[tokio::test]
    async fn obsolete_memories_never_reach_the_provider() {
        let c = conn();
        let w = wid(1);
        seed(&c, 1, Scope::Global, "live", "keep me", MemoryProvenance::HumanWritten);
        let mut expired = MemoryEntry::new(mid(2), Scope::Global, "old", "drop me",
            MemoryType::Project, MemoryProvenance::HumanWritten, t0());
        expired.expires_at = Some("2026-01-01T00:00:00Z".parse().unwrap());
        crate::db::memories::insert(&c, &expired).unwrap();
        let mut superseded = MemoryEntry::new(mid(3), Scope::Global, "replaced", "drop me",
            MemoryType::Project, MemoryProvenance::HumanWritten, t0());
        superseded.superseded_by = Some("mem_newer".into());
        crate::db::memories::insert(&c, &superseded).unwrap();
        let mut deleted = MemoryEntry::new(mid(4), Scope::Global, "gone", "drop me",
            MemoryType::Project, MemoryProvenance::HumanWritten, t0());
        deleted.soft_delete(t0()).unwrap();
        crate::db::memories::insert(&c, &deleted).unwrap();
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        let cmd = queued_resupply(&c, &w, MemoryResupplyReason::SessionStart);
        pump_one(&c, &protocol, &cmd, 8_000).await;
        let text = send_prompts(&protocol).pop().unwrap().1.text;
        assert!(text.contains("keep me"), "{text}");
        assert!(!text.contains("drop me"), "expired, superseded, and deleted stay out: {text}");
    }

    #[tokio::test]
    async fn delivery_resolves_live_state_not_enqueue_time_state() {
        let c = conn();
        let w = wid(1);
        seed(&c, 1, Scope::Global, "plan", "version one", MemoryProvenance::HumanWritten);
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        let cmd = queued_resupply(&c, &w, MemoryResupplyReason::SessionStart);
        // A writer lands between enqueue and delivery: the turn must carry
        // the new content, never the stale copy.
        let mut e = crate::db::memories::get(&c, mid(1).as_str()).unwrap().unwrap();
        let before = e.version;
        e.update("version two", Utc::now()).unwrap();
        crate::db::memories::persist_mutation(&c, &e, before).unwrap();

        pump_one(&c, &protocol, &cmd, 8_000).await;
        let text = send_prompts(&protocol).pop().unwrap().1.text;
        assert!(text.contains("version two"), "{text}");
        assert!(!text.contains("version one"), "{text}");
    }

    #[tokio::test]
    async fn trust_boundary_imported_memories_stay_untrusted() {
        let c = conn();
        let w = wid(1);
        seed(&c, 1, Scope::Global, "runbook", "do the thing",
            MemoryProvenance::Imported { source: "legacy-file".into() });
        let protocol = Arc::new(MockProtocol::new());
        protocol.register(w.clone(), AgentState::Idle);

        let cmd = queued_resupply(&c, &w, MemoryResupplyReason::SessionStart);
        pump_one(&c, &protocol, &cmd, 8_000).await;
        let text = send_prompts(&protocol).pop().unwrap().1.text;
        assert!(text.contains("untrusted"), "imported provenance must read untrusted: {text}");
    }

    #[test]
    fn enqueue_is_idempotent_per_worker_and_reason() {
        let c = conn();
        let w = wid(1);
        assert!(enqueue_resupply(&c, &w, MemoryResupplyReason::SessionStart, Utc::now()).unwrap());
        assert!(!enqueue_resupply(&c, &w, MemoryResupplyReason::SessionStart, Utc::now()).unwrap());
        // A different reason is a different key: post-compaction still queues.
        assert!(enqueue_resupply(&c, &w, MemoryResupplyReason::PostCompaction, Utc::now()).unwrap());
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM _amux_commands WHERE worker_id = ?1", params![w.as_str()], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn terminal_row_is_replaced_so_restart_triggers_supply_again() {
        use amux_core::protocol::{CommandState, CommandTransition};
        let c = conn();
        let w = wid(1);
        assert!(enqueue_resupply(&c, &w, MemoryResupplyReason::SessionStart, Utc::now()).unwrap());
        let key = resupply_key(&w, MemoryResupplyReason::SessionStart);
        let first = commands::by_idempotency_key(&c, &w, &key).unwrap().unwrap();
        for tr in [CommandTransition::Dispatch, CommandTransition::Deliver, CommandTransition::Confirm] {
            commands::transition(&c, &first.id, tr, 3).unwrap();
        }
        assert!(matches!(
            commands::by_idempotency_key(&c, &w, &key).unwrap().unwrap().state,
            CommandState::Confirmed
        ));
        // A fresh start after a completed supply queues again (same key).
        assert!(enqueue_resupply(&c, &w, MemoryResupplyReason::SessionStart, Utc::now()).unwrap());
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM _amux_commands WHERE worker_id = ?1 AND idempotency_key = ?2",
                params![w.as_str(), key], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "replaced, not stacked");
    }

    #[test]
    fn persistence_across_restarts_queued_row_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let mut c = Connection::open(&path).unwrap();
            crate::db::migrate::apply_all(&mut c).unwrap();
            seed(&c, 1, Scope::Global, "plan", "persisted fact", MemoryProvenance::HumanWritten);
            assert!(enqueue_resupply(&c, &wid(1), MemoryResupplyReason::SessionStart, Utc::now()).unwrap());
        }
        // "Restart": a brand-new connection delivers from durable rows only.
        let c2 = Connection::open(&path).unwrap();
        let key = resupply_key(&wid(1), MemoryResupplyReason::SessionStart);
        let cmd = commands::by_idempotency_key(&c2, &wid(1), &key).unwrap().expect("row survived");
        assert!(matches!(cmd.command, WorkerCommand::MemoryResupply { .. }));
        let snap = crate::orchestrator::context::memory_snapshot(&c2, &wid(1), 4_000).unwrap().expect("memory survived");
        assert!(crate::orchestrator::context::render_snapshot(&snap).contains("persisted fact"));
    }
}
