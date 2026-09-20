//! The single transition engine (Gap 1 of the workflow-engine convergence).
//!
//! `advance(conn, task_id, destination, actor, opts) -> AdvanceOutcome`
//!
//! ALL status-changing paths call this. No direct SQL UPDATE on
//! `issues.status` is permitted outside this module. The function:
//!
//! 1. Loads the card.
//! 2. Loads the workflow from the `statuses` table (Gap 2: workflow as
//!    runtime authority).
//! 3. Evaluates column gates from the workflow, falling back to the
//!    five-tier gate trail for typed statuses.
//! 4. Checks continuation requirements (Gap 3: universal enforcement).
//! 5. Checks WIP limits.
//! 6. Applies the status change via `save_patched`.
//! 7. Populates `waiting_on` when entering NeedsYou (Gap 4).
//! 8. Returns structured events for the caller to include in WriteOutcome.

use crate::db::board_store::{self as bs, IssueRow};
use crate::db::workflow_store;
use crate::db::PendingEvent;
use amux_core::board::TaskStatus;
use amux_core::revision::{EntityType, MutationKind};
use amux_core::workflow::BoardWorkflow;
use rusqlite::Connection;

#[derive(Default)]
pub struct AdvanceOpts {
    pub force: bool,
    /// CAS guard: only proceed if the card's current status matches this.
    pub expected_from: Option<String>,
    pub reason: Option<String>,
    /// Set the card's session/owner on transition (e.g. claim).
    pub assign_to: Option<String>,
    /// Log line to append to the card's history.
    pub log_line: Option<String>,
    /// Skip continuation check (for system transitions like promote).
    pub skip_continuation: bool,
    /// Allow a dependency successor into `todo` even when the lane's ordinary
    /// dispatch queue is full. This is narrower than `force`: every gate,
    /// transition and archive check still applies. Board-drive uses it only
    /// when a committed predecessor just completed, so the successor replaces
    /// that plan step instead of adding unrelated queue inventory.
    pub skip_todo_wip: bool,
    /// Acknowledge gate satisfaction (bypass gate without force, with
    /// attribution). The PATCH handler uses this for `gate_ack`.
    pub gate_ack: bool,
}

#[derive(Debug, Clone)]
pub enum AdvanceRefusal {
    NotFound,
    NoOp,
    Stale { actual: String, expected: String },
    GateBlocked { criteria: Vec<String>, source: String },
    ContinuationMissing { verdict: bs::ContinuationVerdict },
    WipLimitReached { limit: i64, current: i64 },
    InvalidTransition { from: String, to: String, reason: String },
    ArchivedImmutable,
}

pub struct AdvanceOutcome {
    pub from: String,
    pub to: String,
    pub row: IssueRow,
    pub events: Vec<PendingEvent>,
}

/// The single transition primitive. Runs inside a write transaction.
pub fn advance(
    conn: &Connection,
    task_id: &str,
    destination: &str,
    actor: &str,
    opts: &AdvanceOpts,
) -> Result<Result<AdvanceOutcome, AdvanceRefusal>, rusqlite::Error> {
    let Some(mut row) = bs::get_issue(conn, task_id)? else {
        return Ok(Err(AdvanceRefusal::NotFound));
    };

    if let Some(ref expected) = opts.expected_from {
        if row.status != *expected {
            return Ok(Err(AdvanceRefusal::Stale {
                actual: row.status.clone(),
                expected: expected.clone(),
            }));
        }
    }

    if row.status == destination {
        return Ok(Err(AdvanceRefusal::NoOp));
    }

    if row.archived != 0 {
        return Ok(Err(AdvanceRefusal::ArchivedImmutable));
    }

    if row.project_group.is_some() && actor != "command-lifecycle" {
        return Ok(Err(AdvanceRefusal::InvalidTransition {from:row.status.clone(),to:destination.to_string(),reason:"project planner owns this task lifecycle".into()}));
    }
    let from_raw = row.status.clone();
    let prev_holder = row.lease_owner.clone();
    let workflow = workflow_store::load_workflow(conn);
    let target_typed = bs::parse_status(destination);

    let inner = if let Some(target) = target_typed {
        advance_typed(conn, &mut row, target, destination, actor, opts, &workflow)?
    } else {
        advance_custom(conn, &mut row, destination, actor, opts, &workflow)?
    };
    if let Err(refusal) = inner {
        return Ok(Err(refusal));
    }
    // RR-0052 Invariant 1: the card is saved, so its lease change is real.
    // Recorded in the same transaction, so an attempt and the lease it
    // describes commit or roll back together.
    crate::db::attempts::record_lease_change(
        conn,
        &row.id,
        prev_holder.as_deref(),
        row.lease_owner.as_deref(),
        row.lease_generation,
        &row.status,
        actor,
        opts.reason.as_deref().or(opts.log_line.as_deref()),
        chrono::Utc::now().timestamp(),
    )?;

    let to_raw = row.status.clone();
    let event = PendingEvent {
        entity_type: EntityType::Task,
        entity_id: row.id.clone(),
        mutation: MutationKind::StatusChanged {
            from: from_raw.clone(),
            to: to_raw.clone(),
        },
        payload: Some(row.snapshot()),
    };
    Ok(Ok(AdvanceOutcome {
        from: from_raw,
        to: to_raw,
        row,
        events: vec![event],
    }))
}

fn advance_typed(
    conn: &Connection,
    row: &mut IssueRow,
    target: TaskStatus,
    _destination: &str,
    actor: &str,
    opts: &AdvanceOpts,
    workflow: &Option<BoardWorkflow>,
) -> Result<Result<(), AdvanceRefusal>, rusqlite::Error> {
    if target == TaskStatus::NeedsYou && !bs::approval_type_allowed(row.session.as_deref(),row.ask_type.as_deref().unwrap_or("")) {
        tracing::warn!(card=%row.id,verdict="approval_category_refused","transition refused by standing approval policy");
        return Ok(Err(AdvanceRefusal::InvalidTransition{from:row.status.clone(),to:"needsyou".into(),reason:format!("approval category outside policy; allowed: {}",bs::approval_types(row.session.as_deref()).join(","))}));
    }
    // Gate check: workflow gates are the authority when present, otherwise
    // the five-tier precedence trail.
    if !opts.force && !opts.gate_ack {
        let criteria = resolve_gate_criteria(conn, row, target, actor, workflow);
        if !criteria.is_empty() {
            return Ok(Err(AdvanceRefusal::GateBlocked {
                criteria,
                source: "typed".into(),
            }));
        }
    }

    // Continuation check (Gap 3).
    if !opts.force
        && !opts.skip_continuation
        && bs::continuation_applies(target)
        && bs::continuation_required(Some(actor))
    {
        let v = bs::continuation_verdict(row.next_action.as_deref().unwrap_or(""));
        if v != bs::ContinuationVerdict::Ok {
            return Ok(Err(AdvanceRefusal::ContinuationMissing { verdict: v }));
        }
    }

    // WIP limit check for todo.
    if target == TaskStatus::Todo && !opts.force && !opts.skip_todo_wip {
        let session = opts
            .assign_to
            .as_deref()
            .or(row.session.as_deref())
            .unwrap_or(actor);
        let limit = bs::todo_wip_limit(Some(session));
        if limit > 0 {
            let current = bs::todo_wip_count(conn, session, &row.id);
            if current >= limit {
                return Ok(Err(AdvanceRefusal::WipLimitReached { limit, current }));
            }
        }
    }

    // Validate from-status is typed (unless force).
    let from_parsed = bs::parse_status(&row.status);
    if from_parsed.is_none() && !opts.force {
        return Ok(Err(AdvanceRefusal::InvalidTransition {
            from: row.status.clone(),
            to: bs::status_to_db(target, &row.status),
            reason: "current status is outside the typed vocabulary".into(),
        }));
    }

    apply_common(conn, row, &bs::status_to_db(target, &row.status), actor, opts)?;

    // Gap 4: populate waiting_on when entering NeedsYou.
    if target == TaskStatus::NeedsYou && row.waiting_on.is_none() {
        let waiting = serde_json::json!({
            "actor": row.ask_actor.as_deref().unwrap_or("human"),
            "type": row.ask_type.as_deref().unwrap_or("judgment"),
            "question": row.ask_question.as_deref().unwrap_or(""),
            "unblocks": row.ask_unblocks.as_deref().unwrap_or(""),
        });
        row.waiting_on = Some(waiting.to_string());
    }
    if from_parsed == Some(TaskStatus::NeedsYou) && target != TaskStatus::NeedsYou {
        row.waiting_on = None;
    }

    bs::save_patched(conn, row)?;
    Ok(Ok(()))
}

fn advance_custom(
    conn: &Connection,
    row: &mut IssueRow,
    destination: &str,
    actor: &str,
    opts: &AdvanceOpts,
    workflow: &Option<BoardWorkflow>,
) -> Result<Result<(), AdvanceRefusal>, rusqlite::Error> {
    // Gate check from workflow column criteria.
    if !opts.force && !opts.gate_ack {
        if let Some(ref wf) = workflow {
            let col_id = amux_core::workflow::ColumnId::new(destination);
            if let Some(col) = wf.get(&col_id) {
                let criteria: Vec<String> = col
                    .gate_criteria
                    .iter()
                    .filter(|c| c.required)
                    .map(|c| c.description.clone())
                    .collect();
                if !criteria.is_empty() {
                    return Ok(Err(AdvanceRefusal::GateBlocked {
                        criteria,
                        source: format!("column:{destination}"),
                    }));
                }
            }
        }
    }

    apply_common(conn, row, destination, actor, opts)?;
    bs::save_patched(conn, row)?;
    Ok(Ok(()))
}

/// Status change + metadata updates shared by typed and custom paths.
/// Set or clear a card's hard lease (RR-0052) to match a status change. Shared
/// by the board_drive path (`apply_common`) and the PATCH door so the two can
/// never disagree about who holds a card.
///
/// - Entering `doing` grants `holder` the lease (heartbeat now, expiry now+TTL);
///   a brand-new holder bumps the generation (a fresh claim = a fresh attempt),
///   a self-reclaim only advances the heartbeat.
/// - Any other status (todo/backlog/review/blocked/needsyou/terminal) releases
///   the lease and bumps the generation, so a stale-claimant write is
///   recognizable and no non-doing card carries a lease.
pub fn apply_lease_transition(
    row: &mut IssueRow,
    new_status: &str,
    holder: Option<&str>,
    now: i64,
) {
    if new_status == "doing" {
        if let Some(holder) = holder.filter(|s| !s.trim().is_empty()) {
            let fresh = row.lease_owner.as_deref() != Some(holder);
            row.lease_owner = Some(holder.to_string());
            row.lease_acquired_at = Some(now);
            row.lease_heartbeat_at = Some(now);
            row.lease_expires_at = Some(now + bs::lease_ttl_s());
            if fresh {
                row.lease_generation += 1;
            }
        }
    } else if row.lease_owner.is_some() {
        row.lease_owner = None;
        row.lease_acquired_at = None;
        row.lease_heartbeat_at = None;
        row.lease_expires_at = None;
        row.lease_generation += 1;
    }
}

fn apply_common(
    _conn: &Connection,
    row: &mut IssueRow,
    new_status: &str,
    _actor: &str,
    opts: &AdvanceOpts,
) -> Result<(), rusqlite::Error> {
    let now = chrono::Utc::now().timestamp();
    row.status = new_status.to_string();
    row.version += 1;
    row.updated = now;

    if let Some(ref session) = opts.assign_to {
        row.session = Some(session.clone());
    }

    // RR-0052 hard lease. This is the single board_drive claim/reclaim/promote
    // choke point, so setting/clearing the lease here keeps the driver and the
    // PATCH door from ever disagreeing about who holds a card.
    let holder = opts.assign_to.clone().or_else(|| row.session.clone());
    apply_lease_transition(row, new_status, holder.as_deref(), now);

    if let Some(ref line) = opts.log_line {
        let stamp = chrono::Local::now().format("%H:%M").to_string();
        row.log = Some(bs::append_log(row.log.as_deref(), &stamp, line));
    }

    Ok(())
}

/// Resolve gate criteria for a typed status transition.
///
/// Gap 2: workflow column gates are the authority when present. Falls back
/// to the five-tier precedence trail (card > worker > group > column >
/// type default) for backward compatibility.
fn resolve_gate_criteria(
    conn: &Connection,
    row: &IssueRow,
    target: TaskStatus,
    actor: &str,
    workflow: &Option<BoardWorkflow>,
) -> Vec<String> {
    // Check workflow column gates first (Gap 2).
    if let Some(ref wf) = workflow {
        let target_str = bs::status_to_db(target, "");
        let col_id = amux_core::workflow::ColumnId::new(&target_str);
        if let Some(col) = wf.get(&col_id) {
            if !col.gate_criteria.is_empty() {
                return col
                    .gate_criteria
                    .iter()
                    .filter(|c| c.required)
                    .map(|c| c.description.clone())
                    .collect();
            }
        }
    }

    // Fall back to the existing five-tier gate trail.
    let session = row
        .session
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(actor);
    let groups = crate::api::session_verbs::lane_groups(session);
    let trail = bs::effective_gate_trail(conn, row, target, &groups);
    trail.criteria
}

/// Validate a transition without applying it. Returns Ok(()) if the
/// transition would succeed, or the refusal reason. Used by the PATCH
/// handler, which batches field changes and calls save_patched once.
///
/// This ensures ALL status-change validation goes through one code path
/// even when the caller needs to own the save.
pub fn validate_transition(
    conn: &Connection,
    row: &IssueRow,
    destination: &str,
    actor: &str,
    opts: &AdvanceOpts,
) -> Result<Result<(), AdvanceRefusal>, rusqlite::Error> {
    if row.archived != 0 {
        return Ok(Err(AdvanceRefusal::ArchivedImmutable));
    }
    if row.status == destination {
        return Ok(Err(AdvanceRefusal::NoOp));
    }

    let workflow = workflow_store::load_workflow(conn);
    let target_typed = bs::parse_status(destination);

    if let Some(target) = target_typed {
        // Gate check.
        if !opts.force && !opts.gate_ack {
            let criteria = resolve_gate_criteria(conn, row, target, actor, &workflow);
            if !criteria.is_empty() {
                return Ok(Err(AdvanceRefusal::GateBlocked {
                    criteria,
                    source: "typed".into(),
                }));
            }
        }
        // Continuation check.
        if !opts.force
            && !opts.skip_continuation
            && bs::continuation_applies(target)
            && bs::continuation_required(Some(actor))
        {
            let v = bs::continuation_verdict(row.next_action.as_deref().unwrap_or(""));
            if v != bs::ContinuationVerdict::Ok {
                return Ok(Err(AdvanceRefusal::ContinuationMissing { verdict: v }));
            }
        }
        // WIP limit.
        if target == TaskStatus::Todo && !opts.force && !opts.skip_todo_wip {
            let session = opts
                .assign_to
                .as_deref()
                .or(row.session.as_deref())
                .unwrap_or(actor);
            let limit = bs::todo_wip_limit(Some(session));
            if limit > 0 {
                let current = bs::todo_wip_count(conn, session, &row.id);
                if current >= limit {
                    return Ok(Err(AdvanceRefusal::WipLimitReached { limit, current }));
                }
            }
        }
        // From-status validity.
        if bs::parse_status(&row.status).is_none() && !opts.force {
            return Ok(Err(AdvanceRefusal::InvalidTransition {
                from: row.status.clone(),
                to: destination.into(),
                reason: "current status is outside the typed vocabulary".into(),
            }));
        }
    } else {
        // Custom column: check workflow gate criteria.
        if !opts.force && !opts.gate_ack {
            if let Some(ref wf) = workflow {
                let col_id = amux_core::workflow::ColumnId::new(destination);
                if let Some(col) = wf.get(&col_id) {
                    let criteria: Vec<String> = col
                        .gate_criteria
                        .iter()
                        .filter(|c| c.required)
                        .map(|c| c.description.clone())
                        .collect();
                    if !criteria.is_empty() {
                        return Ok(Err(AdvanceRefusal::GateBlocked {
                            criteria,
                            source: format!("column:{destination}"),
                        }));
                    }
                }
            }
        }
    }

    Ok(Ok(()))
}

/// Apply status-change side effects on a pre-loaded row (no save). Called
/// by the PATCH handler after validate_transition passes, so it can batch
/// the status change with other field mutations in one save_patched call.
pub fn apply_status_side_effects(row: &mut IssueRow, destination: &str) {
    let from_parsed = bs::parse_status(&row.status);
    let target_typed = bs::parse_status(destination);

    // Gap 4: populate waiting_on when entering NeedsYou.
    if target_typed == Some(TaskStatus::NeedsYou) && row.waiting_on.is_none() {
        let waiting = serde_json::json!({
            "actor": row.ask_actor.as_deref().unwrap_or("human"),
            "type": row.ask_type.as_deref().unwrap_or("judgment"),
            "question": row.ask_question.as_deref().unwrap_or(""),
            "unblocks": row.ask_unblocks.as_deref().unwrap_or(""),
        });
        row.waiting_on = Some(waiting.to_string());
    }
    // Clear waiting_on when leaving NeedsYou.
    if from_parsed == Some(TaskStatus::NeedsYou)
        && target_typed != Some(TaskStatus::NeedsYou)
    {
        row.waiting_on = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_is_granted_on_doing_and_released_off_it() {
        let mut row = IssueRow::default();
        // Entering doing grants the lease to the holder and bumps generation.
        apply_lease_transition(&mut row, "doing", Some("lane-a"), 1000);
        assert_eq!(row.lease_owner.as_deref(), Some("lane-a"));
        assert_eq!(row.lease_acquired_at, Some(1000));
        assert_eq!(row.lease_heartbeat_at, Some(1000));
        assert_eq!(row.lease_expires_at, Some(1000 + bs::lease_ttl_s()));
        assert_eq!(row.lease_generation, 1);

        // A self-reclaim (same holder, still doing) only advances the heartbeat.
        apply_lease_transition(&mut row, "doing", Some("lane-a"), 1500);
        assert_eq!(row.lease_generation, 1, "same holder keeps the generation");
        assert_eq!(row.lease_heartbeat_at, Some(1500));
        assert_eq!(row.lease_expires_at, Some(1500 + bs::lease_ttl_s()));

        // A different holder claiming (still doing) is a fresh attempt.
        apply_lease_transition(&mut row, "doing", Some("lane-b"), 1600);
        assert_eq!(row.lease_owner.as_deref(), Some("lane-b"));
        assert_eq!(row.lease_generation, 2);

        // Leaving doing releases the lease and bumps the generation.
        apply_lease_transition(&mut row, "todo", None, 1700);
        assert_eq!(row.lease_owner, None);
        assert_eq!(row.lease_expires_at, None);
        assert_eq!(row.lease_generation, 3);

        // A no-lease card leaving a non-doing status is a no-op.
        let g = row.lease_generation;
        apply_lease_transition(&mut row, "backlog", None, 1800);
        assert_eq!(row.lease_generation, g, "no lease to release, no bump");

        // An empty holder never grants a lease.
        apply_lease_transition(&mut row, "doing", Some("  "), 1900);
        assert_eq!(row.lease_owner, None);
    }

    #[test]
    fn advance_opts_default_is_sane() {
        let opts = AdvanceOpts::default();
        assert!(!opts.force);
        assert!(opts.expected_from.is_none());
        assert!(opts.assign_to.is_none());
        assert!(!opts.skip_continuation);
        assert!(!opts.skip_todo_wip);
        assert!(!opts.gate_ack);
    }
}
