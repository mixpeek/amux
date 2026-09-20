//! Context assembly pipeline + snapshot recording (RR-0070, Invariant 27).
//!
//! `assemble_context` builds the layered context a worker receives on
//! assignment, as `ContextFragment`s whose `priority` IS the pipeline
//! position (lower = earlier in the assembled context):
//!
//! | priority | source          | layer                                       |
//! |----------|-----------------|---------------------------------------------|
//! | 0        | memory:org      | org-scope memories                          |
//! | 10       | memory:global   | global instructions (Global-scope memories) |
//! | 20       | memory:group    | the worker's group memories                 |
//! | 30       | memory:worker   | the worker's private memories               |
//! | 35       | guide:*         | active, validated failure-derived rules     |
//! | 38       | criteria        | stored acceptance criteria                  |
//! | 40       | task            | task context: title / desc / deps           |
//! | 42       | handoff         | latest structured handoff                   |
//! | 45       | checkpoint      | latest durable continuation checkpoint      |
//! | 50       | attempts        | prior attempt evidence and outcomes         |
//! | 60       | turns           | prior-turn results                          |
//! | 99       | omission:*      | deterministic receipts for omitted context  |
//!
//! Ordering note: this is the plan's ASSEMBLY order (standing instructions,
//! general -> specific, then the task at hand, then recent results). RR-0070
//! also states a priority list "task > deps > memory > turns > history" —
//! that is IMPORTANCE under a token budget (what survives trimming), not
//! position in the prompt. `assemble_context_with_budget` now enforces that
//! budget deterministically and records every omission in the snapshot.
//!
//! Which memories appear is decided by core's ONE visibility predicate
//! (`amux_core::memory::visible` via `db::memories::list_visible`,
//! Invariant 2): a worker-scoped memory reaches only that worker's context.
//!
//! `record_snapshot` persists the snapshot per assignment, INSERT OR IGNORE
//! on the planner's idempotency key — recording is idempotent (Invariant 27:
//! the snapshot is immutable; a re-planned assignment re-records nothing).

use crate::db::memories;
use amux_core::board::Task;
use amux_core::ids::{GroupId, TaskId, WorkerId};
use amux_core::policy::TrustLevel;
use amux_core::scope::{ResolutionTarget, ScopeLevel};
use amux_core::turn::{ContextFragment, ContextSnapshot};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

/// Pipeline positions (see module table). Public so a future budget-trimming
/// pass and the dashboard speak the same numbers instead of re-inventing
/// them.
pub const PRIO_MEMORY_ORG: u32 = 0;
pub const PRIO_MEMORY_GLOBAL: u32 = 10;
pub const PRIO_MEMORY_GROUP: u32 = 20;
pub const PRIO_MEMORY_WORKER: u32 = 30;
pub const PRIO_GUIDES: u32 = 35;
pub const PRIO_CRITERIA: u32 = 38;
pub const PRIO_TASK: u32 = 40;
pub const PRIO_HANDOFF: u32 = 42;
pub const PRIO_CHECKPOINT: u32 = 45;
pub const PRIO_ATTEMPTS: u32 = 50;
pub const PRIO_TURNS: u32 = 60;
pub const PRIO_OMISSIONS: u32 = 99;

fn pretty_json(value: &impl serde::Serialize) -> rusqlite::Result<String> {
    serde_json::to_string_pretty(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn memory_layer(level: ScopeLevel) -> (u32, &'static str) {
    match level {
        ScopeLevel::Org => (PRIO_MEMORY_ORG, "memory:org"),
        ScopeLevel::Global => (PRIO_MEMORY_GLOBAL, "memory:global"),
        ScopeLevel::Group => (PRIO_MEMORY_GROUP, "memory:group"),
        ScopeLevel::Worker => (PRIO_MEMORY_WORKER, "memory:worker"),
    }
}

/// Resolution target for a worker: itself plus its group (looked up from
/// the worker row; a worker with no row still resolves as itself plus
/// globals). One spelling shared by assembly and re-supply.
fn resolution_target(conn: &Connection, worker: &WorkerId) -> rusqlite::Result<ResolutionTarget> {
    let group: Option<GroupId> = crate::db::queries::get_worker(conn, worker.as_str())?
        .and_then(|row| row.group_id)
        .and_then(|g| GroupId::parse(&g).ok());
    Ok(ResolutionTarget {
        worker: Some(worker.clone()),
        group,
    })
}

/// Deterministic memory-only fragments for `worker`, most general scope
/// first. Reads the canonical store through the ONE visibility predicate
/// (`db::memories::list_visible`, Invariant 2), which already excludes
/// soft-deleted, expired, and superseded entries, so provenance, expiry,
/// supersession, and trust boundaries arrive preserved, not re-derived.
/// Untrimmed: callers apply their own budget (`assemble_context_*` trims
/// the whole assembly; re-supply trims the memory set to its own cap).
pub fn memory_fragments(
    conn: &Connection,
    worker: &WorkerId,
) -> rusqlite::Result<Vec<ContextFragment>> {
    let target = resolution_target(conn, worker)?;
    let mut fragments = Vec::new();
    // Memory layers: org -> global -> group -> worker (general to specific,
    // so the more specific layer lands closer to the task and can override
    // in the model's reading, the same precedence direction as Invariant 2).
    for e in memories::list_visible(conn, &target)? {
        let (priority, source) = memory_layer(e.scope.level());
        fragments.push(ContextFragment {
            priority,
            source: source.into(),
            content: format!("{}: {}", e.name, e.content),
            trust: match &e.provenance {
                amux_core::memory::MemoryProvenance::Imported { .. } => TrustLevel::Untrusted,
                _ => TrustLevel::Trusted,
            },
            provenance: serde_json::to_string(&e.provenance).unwrap_or_else(|_| "unknown".into()),
        });
    }
    Ok(fragments)
}

/// Bounded memory-only snapshot for lifecycle re-supply (#73). Returns
/// `None` when the worker sees no live memories, in which case the caller
/// must deliver no turn at all rather than an empty one. `Some` is trimmed
/// to `max_chars` with the same deterministic omission receipts as assembly.
pub fn memory_snapshot(
    conn: &Connection,
    worker: &WorkerId,
    max_chars: usize,
) -> rusqlite::Result<Option<ContextSnapshot>> {
    let fragments = memory_fragments(conn, worker)?;
    if fragments.is_empty() {
        return Ok(None);
    }
    Ok(Some(ContextSnapshot::build(trim_to_budget(fragments, max_chars))))
}

/// Assemble the context snapshot for assigning `task` to `worker`.
///
/// Deterministic by construction: every input is read from the DB in this
/// one call, fragment content is a pure function of those rows, and
/// `ContextSnapshot::build` canonicalizes order before hashing — same DB
/// state, same (worker, task), same hash, every time (Invariant 27's cache
/// rule depends on exactly this).
///
/// Returns `rusqlite::Result` rather than the plan sketch's bare snapshot:
/// a read that fails must surface, not vanish into an empty context that
/// hashes as if the worker legitimately received nothing (ethos rule 7 — a
/// wrong answer that looks plausible is worse than an error).
pub fn assemble_context(
    conn: &Connection,
    worker: &WorkerId,
    task: &Task,
) -> rusqlite::Result<ContextSnapshot> {
    assemble_context_with_budget(conn, worker, task, usize::MAX)
}

/// Assemble and deterministically trim to a character budget. Runtime uses
/// this entry point; the unlimited wrapper remains useful to readers/tests.
pub fn assemble_context_with_budget(
    conn: &Connection,
    worker: &WorkerId,
    task: &Task,
    max_chars: usize,
) -> rusqlite::Result<ContextSnapshot> {
    let now = chrono::Utc::now();
    // Resolution target through the shared spelling (worker plus its
    // group); the group half is also needed for guide-rule scoping below.
    let group = resolution_target(conn, worker)?.group;

    // Memory layers through the shared builder: assignment assembly and
    // lifecycle re-supply render the same store state identically. No
    // budget is applied here; the whole assembly is trimmed once below.
    let mut fragments: Vec<ContextFragment> = memory_fragments(conn, worker)?;

    // Active failure-derived rules are real context, with a stable harness
    // hash recorded beside them. Scope spellings are intentionally simple:
    // global, worker:<id>, group:<id>.
    let active_rules: Vec<_> = crate::db::harness_store::list_guide_rules(conn)?
        .into_iter()
        .filter(|rule| {
            matches!(rule.status, amux_core::harness::GuideRuleStatus::Active)
                && rule.expires_at.is_none_or(|expiry| expiry > now)
                && rule.superseded_by.is_none()
                && (rule.scope == "global"
                    || rule.scope == format!("worker:{worker}")
                    || group
                        .as_ref()
                        .is_some_and(|g| rule.scope == format!("group:{g}")))
        })
        .collect();
    for rule in active_rules {
        fragments.push(ContextFragment {
            priority: PRIO_GUIDES,
            source: format!("guide:{}", rule.id),
            content: rule.content,
            trust: TrustLevel::Trusted,
            provenance: format!(
                "failure:{};owner:{};v{}",
                rule.source_failure, rule.owner, rule.version
            ),
        });
    }

    let issue = issue_by_internal_id(conn, &task.id)?;
    let semantic_id = issue.as_ref().map(|r| r.id.as_str());

    if let Some(task_id) = semantic_id {
        if let Some(criteria) = crate::api::criteria::load(conn, task_id)? {
            fragments.push(ContextFragment {
                priority: PRIO_CRITERIA,
                source: "acceptance_criteria".into(),
                content: pretty_json(&criteria)?,
                trust: TrustLevel::Trusted,
                provenance: format!("criteria:v{}", criteria.version),
            });
        }
    }

    // Task context: title / desc / deps in ONE fragment — the unit the
    // worker is being handed.
    let mut task_content = format!("Task {}: {}", task.id, task.title);
    if !task.desc.trim().is_empty() {
        task_content.push('\n');
        task_content.push_str(&task.desc);
    }
    if !task.depends_on.is_empty() {
        let deps: Vec<&str> = task.depends_on.iter().map(|d| d.as_str()).collect();
        task_content.push_str("\nDepends on: ");
        task_content.push_str(&deps.join(", "));
    }
    if let Some(row) = &issue {
        if let Some(last) = row.last_result.as_deref().filter(|v| !v.trim().is_empty()) {
            task_content.push_str("\nLast result: ");
            task_content.push_str(last);
        }
        if let Some(next) = row.next_action.as_deref().filter(|v| !v.trim().is_empty()) {
            task_content.push_str("\nNext action: ");
            task_content.push_str(next);
        }
        if let Some(open) = row.unresolved.as_deref().filter(|v| !v.trim().is_empty()) {
            task_content.push_str("\nUnresolved: ");
            task_content.push_str(open);
        }
    }
    fragments.push(ContextFragment {
        priority: PRIO_TASK,
        source: "task".into(),
        content: task_content,
        trust: TrustLevel::Trusted,
        provenance: "board".into(),
    });

    if let Some(task_id) = semantic_id {
        if let Some(handoff) = crate::db::harness_store::latest_handoff(conn, task_id)? {
            fragments.push(ContextFragment {
                priority: PRIO_HANDOFF,
                source: "handoff".into(),
                content: pretty_json(&handoff)?,
                trust: TrustLevel::Trusted,
                provenance: handoff.id,
            });
        }
        if let Some(checkpoint) = crate::db::harness_store::get_checkpoint(conn, task_id)? {
            fragments.push(ContextFragment {
                priority: PRIO_CHECKPOINT,
                source: "checkpoint".into(),
                content: pretty_json(&checkpoint)?,
                trust: TrustLevel::Trusted,
                provenance: format!("checkpoint:v{}", checkpoint.version),
            });
        }
    }

    let mut attempt_stmt = conn.prepare(
        "SELECT record FROM _amux_attempts WHERE task_id=?1 ORDER BY attempt ASC",
    )?;
    let attempts: Vec<String> = attempt_stmt
        .query_map([task.id.as_str()], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !attempts.is_empty() {
        let records: Vec<amux_core::limits::AttemptRecord> = attempts
            .iter()
            .map(|raw| {
                serde_json::from_str(raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        fragments.push(ContextFragment {
            priority: PRIO_ATTEMPTS,
            source: "prior_attempts".into(),
            content: pretty_json(&records)?,
            trust: TrustLevel::Trusted,
            provenance: "attempt-ledger".into(),
        });
    }

    let mut turn_stmt = conn.prepare(
        "SELECT id,outcome,tokens,ended_at FROM _amux_turns
         WHERE task_id=?1 AND ended_at IS NOT NULL ORDER BY ended_at DESC LIMIT 5",
    )?;
    let turns: Vec<serde_json::Value> = turn_stmt
        .query_map([task.id.as_str()], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "outcome": r.get::<_, Option<String>>(1)?,
                "tokens": r.get::<_, String>(2)?,
                "ended_at": r.get::<_, String>(3)?,
            }))
        })?
        .collect::<Result<_, _>>()?;
    if !turns.is_empty() {
        fragments.push(ContextFragment {
            priority: PRIO_TURNS,
            source: "prior_turns".into(),
            content: pretty_json(&turns)?,
            trust: TrustLevel::Trusted,
            provenance: "turn-ledger".into(),
        });
    }

    fragments.push(ContextFragment {
        priority: PRIO_GUIDES,
        source: "harness_version".into(),
        content: crate::db::harness_store::harness_version(conn)?,
        trust: TrustLevel::Trusted,
        provenance: "harness-registry".into(),
    });

    Ok(ContextSnapshot::build(trim_to_budget(fragments, max_chars)))
}

fn retention_rank(source: &str) -> u8 {
    if source.starts_with("memory:org") || source.starts_with("memory:global") {
        0
    } else if source.starts_with("memory:") || source == "prior_turns" {
        1
    } else if source.starts_with("guide:") {
        2
    } else {
        10
    }
}

/// Trim whole low-priority fragments first and add an omission receipt. Core
/// task, criteria, checkpoint, attempts and safety/version fragments survive.
fn trim_to_budget(mut fragments: Vec<ContextFragment>, max_chars: usize) -> Vec<ContextFragment> {
    let mut total: usize = fragments.iter().map(|f| f.content.chars().count()).sum();
    if total <= max_chars {
        return fragments;
    }
    let mut omitted = Vec::new();
    while total > max_chars {
        let Some((idx, _)) = fragments
            .iter()
            .enumerate()
            .filter(|(_, f)| retention_rank(&f.source) < 10)
            .min_by_key(|(_, f)| (retention_rank(&f.source), f.priority))
        else {
            break;
        };
        let removed = fragments.remove(idx);
        total = total.saturating_sub(removed.content.chars().count());
        let mut h = Sha256::new();
        h.update(removed.content.as_bytes());
        omitted.push(format!("{}:{}", removed.source, hex::encode(&h.finalize()[..6])));
    }
    if !omitted.is_empty() {
        fragments.push(ContextFragment {
            priority: PRIO_OMISSIONS,
            source: "omissions".into(),
            content: format!(
                "{} fragment(s) omitted by context budget: {}",
                omitted.len(),
                omitted.join(", ")
            ),
            trust: TrustLevel::Trusted,
            provenance: "context-budget".into(),
        });
    }
    // A single task, criterion, or checkpoint can itself exceed the budget.
    // Whole-fragment eviction cannot solve that case, so cap the largest
    // remaining payloads deterministically and leave the original hash in the
    // replacement marker. This is a hard bound, not a best-effort hint.
    loop {
        let current: usize = fragments.iter().map(|f| f.content.chars().count()).sum();
        if current <= max_chars {
            break;
        }
        let Some((index, length)) = fragments
            .iter()
            .enumerate()
            .map(|(index, fragment)| (index, fragment.content.chars().count()))
            .max_by_key(|(index, length)| (*length, std::cmp::Reverse(*index)))
        else {
            break;
        };
        let target = length.saturating_sub(current - max_chars);
        let mut hash = Sha256::new();
        hash.update(fragments[index].content.as_bytes());
        let marker = format!("[truncated:{}]", hex::encode(&hash.finalize()[..6]));
        let marker_len = marker.chars().count();
        fragments[index].content = if target <= marker_len {
            marker.chars().take(target).collect()
        } else {
            let mut value: String = fragments[index]
                .content
                .chars()
                .take(target - marker_len)
                .collect();
            value.push_str(&marker);
            value
        };
    }
    fragments
}

pub fn load_snapshot(
    conn: &Connection,
    assignment_key: &str,
) -> rusqlite::Result<Option<ContextSnapshot>> {
    conn.query_row(
        "SELECT fragments,content_hash FROM _amux_context_snapshots WHERE assignment_key=?1",
        [assignment_key],
        |r| {
            let raw: String = r.get(0)?;
            let fragments = serde_json::from_str(&raw).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            Ok(ContextSnapshot {
                fragments,
                content_hash: r.get(1)?,
            })
        },
    )
    .optional()
}

pub fn render_snapshot(snapshot: &ContextSnapshot) -> String {
    let mut out = format!("AMUX CONTEXT SNAPSHOT {}\n", snapshot.content_hash);
    for fragment in &snapshot.fragments {
        let trust = match fragment.trust {
            TrustLevel::Trusted => "trusted",
            TrustLevel::Untrusted => "untrusted",
        };
        out.push_str(&format!(
            "\n--- {} [{}; {}] ---\n{}\n",
            fragment.source, trust, fragment.provenance, fragment.content
        ));
    }
    out
}

/// Mint the required typed handoff for an assignment from the board's
/// existing structured fields. The packet is persisted before the context
/// snapshot, so the snapshot can include the exact handoff being delivered.
pub fn record_assignment_handoff(
    conn: &Connection,
    worker: &WorkerId,
    task: &Task,
    assignment_key: &str,
) -> rusqlite::Result<bool> {
    let Some(row) = issue_by_internal_id(conn, &task.id)? else {
        return Ok(false);
    };
    let criteria_version = crate::api::criteria::load(conn, &row.id)?
        .map(|c| c.version)
        .unwrap_or(0);
    let checkpoint = crate::db::harness_store::get_checkpoint(conn, &row.id)?;
    let artifacts = crate::db::artifact_store::list_for_task(conn, &row.id)?
        .into_iter()
        .filter(|a| !crate::db::artifact_store::is_retired_state(&a.state))
        .map(|a| a.ref_value)
        .collect();
    let packet = amux_core::harness::HandoffPacket {
        id: format!("handoff_{}", ulid::Ulid::new().to_string().to_lowercase()),
        task_id: row.id.clone(),
        assignment_key: Some(assignment_key.to_string()),
        objective: if row.desc.trim().is_empty() {
            row.title.clone()
        } else {
            format!("{}\n{}", row.title, row.desc)
        },
        criteria_version,
        checkpoint_version: checkpoint.as_ref().map(|c| c.version),
        checkpoint_hash: checkpoint.as_ref().map(|c| c.input_hash.clone()),
        artifacts,
        evidence: row.evidence.clone().into_iter().collect(),
        assumptions: Vec::new(),
        unresolved: row.unresolved.clone().into_iter().collect(),
        concerns: Vec::new(),
        deviations: Vec::new(),
        findings: Vec::new(),
        requires_replan: false,
        planning_scope_id: None,
        next_action: row
            .next_action
            .clone()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "Execute {} against acceptance criteria v{}",
                    row.id, criteria_version
                )
            }),
        deadline: row.due.clone(),
        sender: row.session.clone().unwrap_or_else(|| "orchestrator".into()),
        receiver: worker.to_string(),
        created_at: chrono::Utc::now(),
    };
    crate::db::harness_store::insert_handoff(conn, &packet)
}

/// Record a snapshot for one assignment (Invariant 27). INSERT OR IGNORE on
/// the UNIQUE `assignment_key` — the planner's idempotency key — so
/// re-recording the same assignment is a no-op. Returns whether a row was
/// NEWLY recorded (false = this assignment already has its immutable
/// snapshot; the caller must not bump revisions or emit events for it).
pub fn record_snapshot(
    conn: &Connection,
    assignment_key: &str,
    task_id: &TaskId,
    worker_id: &WorkerId,
    snap: &ContextSnapshot,
) -> rusqlite::Result<bool> {
    let fragments = serde_json::to_string(&snap.fragments).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(e))
    })?;
    let semantic = issue_by_internal_id(conn, task_id)?.map(|r| r.id);
    let checkpoint_version = semantic
        .as_deref()
        .map(|id| crate::db::harness_store::get_checkpoint(conn, id))
        .transpose()?
        .flatten()
        .map(|c| c.version);
    let harness_version = crate::db::harness_store::harness_version(conn)?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO _amux_context_snapshots
         (assignment_key, task_id, worker_id, content_hash, fragments, at, checkpoint_version, harness_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            assignment_key,
            task_id.as_str(),
            worker_id.as_str(),
            snap.content_hash,
            fragments,
            chrono::Utc::now().to_rfc3339(),
            checkpoint_version,
            harness_version,
        ],
    )?;
    Ok(n > 0)
}

/// Find the board task behind a planner TaskId. Board tasks carry synthetic
/// ids minted by `board_store::internal_id` (a one-way hash of the semantic
/// id), so the lookup scans and re-mints — the table is board-sized and the
/// scan is exact, where a reverse map would be a second spelling of the id
/// scheme that could drift from the first.
pub fn task_by_internal_id(conn: &Connection, id: &TaskId) -> rusqlite::Result<Option<Task>> {
    Ok(issue_by_internal_id(conn, id)?.and_then(|row| row.to_task()))
}

/// The RAW issue row behind a planner TaskId — for callers that must write
/// board columns (log, rev) rather than reason over the core Task. Same
/// scan-and-re-mint as [`task_by_internal_id`], one implementation.
pub fn issue_by_internal_id(
    conn: &Connection,
    id: &TaskId,
) -> rusqlite::Result<Option<crate::db::board_store::IssueRow>> {
    let rows = crate::db::board_store::list_issues(
        conn,
        &[],
        &[],
        crate::db::board_store::ArchivedFilter::All,
    )?;
    Ok(rows
        .into_iter()
        .find(|row| crate::db::board_store::internal_id(&row.id) == *id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::events::Actor;
    use amux_core::harness::{EnforcementLayer, GuideRule, GuideRuleStatus};
    use amux_core::ids::MemoryId;
    use amux_core::memory::{MemoryEntry, MemoryProvenance, MemoryType};
    use amux_core::scope::Scope;

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

    fn tid(n: u64) -> TaskId {
        TaskId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
    }

    fn seed_memory(c: &Connection, n: u64, scope: Scope, name: &str, content: &str) {
        let e = MemoryEntry::new(
            MemoryId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128)),
            scope,
            name,
            content,
            MemoryType::Project,
            MemoryProvenance::HumanWritten,
            "2026-08-01T00:00:00Z".parse().unwrap(),
        );
        crate::db::memories::insert(c, &e).unwrap();
    }

    /// A worker row with a group, so assemble_context resolves group scope.
    fn seed_worker_in_group(c: &Connection, w: &WorkerId, g: &GroupId) {
        c.execute(
            "INSERT INTO _amux_workers (id, display_name, group_id, created_at, updated_at)
             VALUES (?1, 'w', ?2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            params![w.as_str(), g.as_str()],
        )
        .unwrap();
    }

    fn task(n: u64) -> Task {
        let mut t = Task::create(
            tid(n),
            "Fix the login redirect loop",
            amux_core::board::ItemType::Code,
            Actor::Human { name: "ethan".into() },
            "2026-08-01T00:00:00Z".parse().unwrap(),
        );
        t.desc = "Users bounce between /login and /".into();
        t.depends_on = vec![tid(900)];
        t
    }

    #[test]
    fn layers_ordered_global_before_group_before_worker_then_task() {
        let c = conn();
        let (w, g) = (wid(1), gid(2));
        seed_worker_in_group(&c, &w, &g);
        seed_memory(&c, 10, Scope::Global, "house-rules", "no em-dashes");
        seed_memory(&c, 11, Scope::Group { id: g.clone() }, "team-notes", "deploy fridays");
        seed_memory(&c, 12, Scope::Worker { id: w.clone() }, "my-notes", "auth in src/auth.rs");
        // Another worker's memory must NOT appear in this context.
        seed_memory(&c, 13, Scope::Worker { id: wid(99) }, "not-mine", "secret");

        let snap = assemble_context(&c, &w, &task(1)).unwrap();
        let sources: Vec<&str> = snap.fragments.iter().map(|f| f.source.as_str()).collect();
        assert_eq!(
            sources,
            vec![
                "memory:global",
                "memory:group",
                "memory:worker",
                "harness_version",
                "task"
            ],
            "assembly order: global -> group -> worker -> harness -> task"
        );
        // Priorities are strictly the documented pipeline positions.
        let prios: Vec<u32> = snap.fragments.iter().map(|f| f.priority).collect();
        assert_eq!(
            prios,
            vec![
                PRIO_MEMORY_GLOBAL,
                PRIO_MEMORY_GROUP,
                PRIO_MEMORY_WORKER,
                PRIO_GUIDES,
                PRIO_TASK
            ]
        );
        // Isolation: the other worker's memory is absent.
        assert!(!snap.fragments.iter().any(|f| f.content.contains("secret")));
        // Task fragment carries title, desc and deps.
        let task_frag = &snap.fragments[4];
        assert!(task_frag.content.contains("Fix the login redirect loop"));
        assert!(task_frag.content.contains("Users bounce"));
        assert!(task_frag.content.contains(tid(900).as_str()));
    }

    #[test]
    fn same_inputs_same_hash_different_inputs_different_hash() {
        let c = conn();
        let w = wid(1);
        seed_memory(&c, 10, Scope::Global, "house-rules", "no em-dashes");

        let a = assemble_context(&c, &w, &task(1)).unwrap();
        let b = assemble_context(&c, &w, &task(1)).unwrap();
        assert_eq!(a.content_hash, b.content_hash, "deterministic");
        assert_eq!(a, b);

        // A new visible memory moves the hash.
        seed_memory(&c, 11, Scope::Worker { id: w.clone() }, "note", "x");
        let d = assemble_context(&c, &w, &task(1)).unwrap();
        assert_ne!(d.content_hash, a.content_hash);
    }

    #[test]
    fn record_snapshot_is_idempotent_per_assignment_key() {
        let c = conn();
        let w = wid(1);
        let t = task(1);
        let snap = assemble_context(&c, &w, &t).unwrap();
        let key = format!("{}:{}:1", t.id, w);

        assert!(record_snapshot(&c, &key, &t.id, &w, &snap).unwrap(), "first: recorded");
        assert!(
            !record_snapshot(&c, &key, &t.id, &w, &snap).unwrap(),
            "second: same key, NOT re-recorded"
        );
        let (n, hash): (i64, String) = c
            .query_row(
                "SELECT COUNT(*), MAX(content_hash) FROM _amux_context_snapshots
                 WHERE assignment_key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(hash, snap.content_hash);

        // A NEW attempt is a new assignment key -> its own row.
        let key2 = format!("{}:{}:2", t.id, w);
        assert!(record_snapshot(&c, &key2, &t.id, &w, &snap).unwrap());
    }

    #[test]
    fn oversized_required_fragment_is_hard_capped_with_a_receipt() {
        let c = conn();
        let w = wid(1);
        let mut oversized = task(1);
        oversized.desc = "x".repeat(10_000);
        let snap = assemble_context_with_budget(&c, &w, &oversized, 512).unwrap();
        let chars: usize = snap
            .fragments
            .iter()
            .map(|fragment| fragment.content.chars().count())
            .sum();
        assert!(chars <= 512, "hard context cap: {chars}");
        assert!(snap
            .fragments
            .iter()
            .any(|fragment| fragment.content.contains("[truncated:")));
    }

    #[test]
    fn only_live_unsuperseded_active_guides_enter_context() {
        let c = conn();
        let w = wid(1);
        let now = chrono::Utc::now();
        for (id, content, expires_at, superseded_by) in [
            ("live", "live guidance", None, None),
            (
                "expired",
                "expired guidance",
                Some(now - chrono::Duration::hours(1)),
                None,
            ),
            (
                "superseded",
                "superseded guidance",
                None,
                Some("replacement".to_string()),
            ),
        ] {
            crate::db::harness_store::put_guide_rule(
                &c,
                &GuideRule {
                    id: id.into(),
                    scope: "global".into(),
                    name: id.into(),
                    content: content.into(),
                    owner: "platform".into(),
                    source_failure: "INC-1".into(),
                    rationale: "regression prevention".into(),
                    status: GuideRuleStatus::Active,
                    enforcement_layer: EnforcementLayer::Guide,
                    sensor_ref: None,
                    version: 0,
                    added_at: now,
                    last_validated_at: Some(now),
                    expires_at,
                    superseded_by,
                },
            )
            .unwrap();
        }

        let snap = assemble_context(&c, &w, &task(1)).unwrap();
        let guides: Vec<&str> = snap
            .fragments
            .iter()
            .filter(|fragment| fragment.source.starts_with("guide:"))
            .map(|fragment| fragment.content.as_str())
            .collect();
        assert_eq!(guides, vec!["live guidance"]);
    }
}
