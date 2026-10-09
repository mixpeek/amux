//! Durable memory re-supply across the agent lifecycle (#73), proven at the
//! public boundary: memories go in through `db::memories`, a re-supply is
//! queued through `orchestrator::memory_resupply`, and what the provider
//! would receive is asserted on `MockProtocol`'s recorded `SendPrompt`.
//!
//! This file covers the provider-facing claims only: one bounded turn per
//! lifecycle path, scope isolation, and the empty-set no-op. Enqueue
//! mechanics (idempotency, terminal replacement) and trigger wiring live
//! beside the code in `orchestrator::memory_resupply`'s unit tests.

use amux_core::ids::{GroupId, MemoryId, WorkerId};
use amux_core::memory::{MemoryEntry, MemoryProvenance, MemoryType};
use amux_core::protocol::MemoryResupplyReason;
use amux_core::scope::Scope;
use amux_server::db::{commands, memories, migrate};
use amux_server::opencode::mock::{MockProtocol, RecordedCall};
use amux_server::opencode::{AgentProtocol, AgentState};
use amux_server::orchestrator::memory_resupply;
use chrono::Utc;
use rusqlite::{params, Connection};
use std::sync::Arc;

fn conn() -> Connection {
    let mut c = Connection::open_in_memory().unwrap();
    migrate::apply_all(&mut c).unwrap();
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

fn seed(c: &Connection, n: u64, scope: Scope, name: &str, content: &str) {
    let e = MemoryEntry::new(
        mid(n),
        scope,
        name,
        content,
        MemoryType::Project,
        MemoryProvenance::HumanWritten,
        "2026-08-01T00:00:00Z".parse().unwrap(),
    );
    memories::insert(c, &e).unwrap();
}

fn seed_worker_in_group(c: &Connection, w: &WorkerId, g: &GroupId) {
    c.execute(
        "INSERT INTO _amux_workers (id, display_name, group_id, created_at, updated_at)
         VALUES (?1, 'w', ?2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        params![w.as_str(), g.as_str()],
    )
    .unwrap();
}

fn save_conversation_ref(c: &Connection, w: &WorkerId) {
    c.execute(
        "INSERT INTO _amux_conversations (worker_id, provider, conversation_ref, updated_at)
         VALUES (?1, 'claude', 'conv-live-1', '2026-01-01T00:00:00Z')",
        params![w.as_str()],
    )
    .unwrap();
}

/// Prepare exactly the way the pump arm does, then send. Returns the
/// provider-facing texts in queue order.
async fn deliver_all(
    c: &Connection,
    protocol: &Arc<MockProtocol>,
    w: &WorkerId,
    max_chars: usize,
) -> Vec<String> {
    use amux_core::ids::CommandId;
    use amux_core::protocol::{DeliveryTiming, QueuedCommand, WorkerCommand};
    let rows: Vec<(String, String, String)> = {
        let mut stmt = c
            .prepare("SELECT id, command, idempotency_key FROM _amux_commands WHERE worker_id = ?1 ORDER BY rowid")
            .unwrap();
        stmt.query_map(params![w.as_str()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    let mut out = Vec::new();
    for (id, raw, key) in rows {
        assert!(key.starts_with("memory-resupply:"), "{key}");
        let cmd = QueuedCommand::new(
            CommandId::parse(&id).unwrap(),
            w.clone(),
            serde_json::from_str::<WorkerCommand>(&raw).unwrap(),
            key,
            Utc::now(),
            DeliveryTiming::AtTurnBoundary,
            None,
        );
        if let Some(prompt) = memory_resupply::prepare_resupply(c, &cmd, max_chars).unwrap() {
            protocol.send_prompt(w, prompt.clone()).await.unwrap();
            out.push(prompt.text);
        }
    }
    out
}

fn send_prompts(protocol: &MockProtocol) -> Vec<String> {
    protocol
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            RecordedCall::SendPrompt { prompt, .. } => Some(prompt.text),
            _ => None,
        })
        .collect()
}

fn queue(c: &Connection, w: &WorkerId, reason: MemoryResupplyReason) {
    assert!(
        memory_resupply::enqueue_resupply(c, w, reason, Utc::now()).unwrap(),
        "{reason:?} must queue fresh"
    );
}

#[tokio::test]
async fn start_supplies_worker_group_and_global_memories() {
    let c = conn();
    let (w, g) = (wid(1), gid(2));
    seed_worker_in_group(&c, &w, &g);
    seed(&c, 10, Scope::Global, "house-rules", "no deploys friday");
    seed(&c, 11, Scope::Group { id: g.clone() }, "team-notes", "rotate creds");
    seed(&c, 12, Scope::Worker { id: w.clone() }, "my-notes", "auth in src/auth.rs");
    let protocol = Arc::new(MockProtocol::new());
    protocol.register(w.clone(), AgentState::Idle);

    queue(&c, &w, MemoryResupplyReason::SessionStart);
    let texts = deliver_all(&c, &protocol, &w, 4_000).await;
    assert_eq!(texts.len(), 1, "exactly one provider-facing turn");
    assert!(texts[0].contains("no deploys friday"), "{}", texts[0]);
    assert!(texts[0].contains("rotate creds"), "{}", texts[0]);
    assert!(texts[0].contains("auth in src/auth.rs"), "{}", texts[0]);
}

#[tokio::test]
async fn resume_supplies_after_ref_loss_and_skips_on_live_conversation() {
    let c = conn();
    let w = wid(1);
    seed(&c, 10, Scope::Global, "decision", "we chose sqlite");
    let protocol = Arc::new(MockProtocol::new());
    protocol.register(w.clone(), AgentState::Idle);

    queue(&c, &w, MemoryResupplyReason::Resume);
    let texts = deliver_all(&c, &protocol, &w, 4_000).await;
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("we chose sqlite"));

    // Complete the first supply, then resume into a live conversation:
    // the re-queue replaces the terminal row, and delivery sends nothing.
    use amux_core::protocol::CommandTransition;
    let key = memory_resupply::resupply_key(&w, MemoryResupplyReason::Resume);
    let first = commands::by_idempotency_key(&c, &w, &key).unwrap().unwrap();
    for tr in [CommandTransition::Dispatch, CommandTransition::Deliver, CommandTransition::Confirm] {
        commands::transition(&c, &first.id, tr, 3).unwrap();
    }
    save_conversation_ref(&c, &w);
    queue(&c, &w, MemoryResupplyReason::Resume);
    let before = send_prompts(&protocol).len();
    let texts = deliver_all(&c, &protocol, &w, 4_000).await;
    assert!(texts.is_empty(), "live conversation needs no re-supply");
    assert_eq!(send_prompts(&protocol).len(), before);
}

#[tokio::test]
async fn post_compaction_reasserts_despite_live_conversation() {
    let c = conn();
    let w = wid(1);
    seed(&c, 10, Scope::Global, "decision", "we chose sqlite");
    save_conversation_ref(&c, &w);
    let protocol = Arc::new(MockProtocol::new());
    protocol.register(w.clone(), AgentState::Idle);

    queue(&c, &w, MemoryResupplyReason::PostCompaction);
    let texts = deliver_all(&c, &protocol, &w, 4_000).await;
    assert_eq!(texts.len(), 1, "re-assertion is the point of post-compaction");
    assert!(texts[0].contains("we chose sqlite"));
}

#[tokio::test]
async fn empty_memory_set_sends_no_turn() {
    let c = conn();
    let w = wid(1);
    let protocol = Arc::new(MockProtocol::new());
    protocol.register(w.clone(), AgentState::Idle);

    queue(&c, &w, MemoryResupplyReason::SessionStart);
    let texts = deliver_all(&c, &protocol, &w, 4_000).await;
    assert!(texts.is_empty());
    assert!(send_prompts(&protocol).is_empty());
}

#[tokio::test]
async fn scope_isolation_holds_provider_facing() {
    let c = conn();
    let (a, b, g) = (wid(1), wid(2), gid(7));
    seed_worker_in_group(&c, &a, &g);
    seed(&c, 1, Scope::Worker { id: a.clone() }, "a-private", "a secret");
    seed(&c, 2, Scope::Worker { id: b.clone() }, "b-private", "b secret");
    seed(&c, 3, Scope::Group { id: g.clone() }, "team", "shared plan");
    let protocol = Arc::new(MockProtocol::new());
    protocol.register(a.clone(), AgentState::Idle);
    protocol.register(b.clone(), AgentState::Idle);

    queue(&c, &a, MemoryResupplyReason::SessionStart);
    queue(&c, &b, MemoryResupplyReason::SessionStart);
    let text_a = &deliver_all(&c, &protocol, &a, 8_000).await.pop().unwrap();
    assert!(text_a.contains("a secret") && text_a.contains("shared plan"));
    assert!(!text_a.contains("b secret"), "{text_a}");
    let text_b = &deliver_all(&c, &protocol, &b, 8_000).await.pop().unwrap();
    assert!(text_b.contains("b secret"));
    assert!(!text_b.contains("a secret"), "{text_b}");
    assert!(!text_b.contains("shared plan"), "B is in no group: {text_b}");
}
