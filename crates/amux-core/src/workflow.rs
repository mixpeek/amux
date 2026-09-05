//! The canonical board workflow: ordered, configurable columns (AMUX-2631).
//!
//! # The mismatch this resolves
//!
//! Before this module there were two disagreeing models of "what column is a
//! card in", and the disagreement was structural rather than a bug anyone could
//! point at:
//!
//! * the `statuses` TABLE — ordered, user-configurable, creatable, renamable,
//!   reorderable, with a per-column `gate` JSON. The API already ships
//!   `POST/PATCH/DELETE /api/board/statuses`, so users can build
//!   `todo -> implementation -> qa -> security-review -> verified` today.
//! * [`crate::board::TaskStatus`] — a CLOSED Rust enum, with the progression
//!   `Todo -> Doing -> Review -> Done -> Verified` written into the disposition
//!   logic, and `Gate.guards` pointing at it.
//!
//! Three consequences, all confirmed in the live tree before this landed:
//!
//! 1. **A card in a custom column is invisible to the orchestrator.**
//!    `board_store::to_task()` returns `Option<Task>` via `parse_status`, and
//!    `runtime::load_board_tasks` does `let Some(task) = row.to_task() else {
//!    continue }`. An unparseable column silently drops the card from planning
//!    entirely — no disposition, no next action, no assignment, no stall
//!    detection. Latent only because every status currently in production
//!    happens to parse; armed the moment anyone uses the custom-column feature
//!    the API already exposes.
//!
//! 2. **Configured column gates are ignored.** `effective_gate(row, target)`
//!    consults the row's own override then `default_gates_for(item_type,
//!    TaskStatus)` — a hardcoded match. `statuses.gate` is never read. Editing
//!    a column's gate through the UI writes the DB and changes nothing about
//!    enforcement. It looks correct today only because the seeded values are
//!    byte-identical to the hardcoded defaults — two subsystems agreeing by
//!    coincidence, which is the pattern that hides until someone edits one.
//!
//! 3. **Progression is hardcoded**, so `position` — the thing the reorder
//!    endpoint writes — does not actually determine what comes next.
//!
//! # The model
//!
//! Column IDENTITY is a durable string ([`ColumnId`]); column BEHAVIOUR is
//! typed ([`ColumnRole`], [`TerminalBehavior`]). That split is the whole point:
//! a user adds `security-review` without a Rust enum variant, while the
//! semantics that must stay typed — what is dispatchable, what is terminal and
//! whether terminal means success — remain in the type system where the
//! compiler can check them.
//!
//! Ordering comes from `position`. "Furthest right" is derived from the
//! configured order, never from a hardcoded sequence.

use crate::board::{GateCriterion, ItemType};
use serde::{Deserialize, Serialize};

/// Durable column identifier — the `statuses.id` value (`todo`, `qa`,
/// `security-review`). Never an enum: users create these at runtime.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ColumnId(pub String);

impl ColumnId {
    pub fn new(s: impl Into<String>) -> Self {
        ColumnId(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ColumnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a column MEANS to the orchestrator, independent of its name.
///
/// Deliberately not one variant per user column — that would put us straight
/// back into a closed enum. A workflow may contain many `Custom` columns; role
/// answers only the questions the orchestrator must ask: may I dispatch from
/// here, is someone actively on it, does entry need a reviewer, is this the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnRole {
    /// Parked; deliberately NOT auto-dispatched (triage is a human's call).
    Backlog,
    /// Queued and dispatchable — the orchestrator may lease from here.
    Dispatchable,
    /// Actively being executed.
    Active,
    /// Awaiting another actor's judgment.
    Review,
    /// The worker's unverified CLAIM of completion.
    CompletionClaim,
    /// Successful end state.
    VerifiedTerminal,
    /// Abandoned end state.
    DiscardedTerminal,
    /// Structurally waiting (needs-you / blocked / armed).
    Waiting,
    /// A user-defined stage with no special semantics: it participates in
    /// ordering and gating like any other, and is dispatchable by default so a
    /// custom stage never becomes a silent dead end.
    Custom,
}

impl ColumnRole {
    /// May the orchestrator lease work from a column with this role?
    pub fn is_dispatchable(self) -> bool {
        matches!(self, ColumnRole::Dispatchable | ColumnRole::Custom)
    }
}

/// Whether a column ends the card's journey, and what that ending means.
///
/// EXPLICIT, never inferred from being rightmost (spec §16). If "rightmost is
/// terminal" were the rule, a user appending an `archive` or `notes` column at
/// the far right would silently redefine the success state for the whole board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalBehavior {
    NonTerminal,
    /// Reached the goal.
    SuccessfulTerminal,
    /// Ended without achieving it (discarded/quarantined).
    AbandonedTerminal,
    /// Parked indefinitely awaiting an external firing event (tripwire/watch).
    Dormant,
}

impl TerminalBehavior {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TerminalBehavior::SuccessfulTerminal | TerminalBehavior::AbandonedTerminal
        )
    }
}

/// One configured stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardColumn {
    pub id: ColumnId,
    pub label: String,
    pub position: i64,
    pub role: ColumnRole,
    pub terminal: TerminalBehavior,
    /// Criteria guarding ENTRY INTO this column, from `statuses.gate`.
    ///
    /// On the column, not on the transition, because that is what the storage
    /// and the UI already express: a gate belongs to its destination stage.
    pub gate_criteria: Vec<GateCriterion>,
    /// Types this column's gate applies to; `None` = all.
    pub applies_to_types: Option<Vec<ItemType>>,
    /// What this stage is for (human-readable, not enforced).
    pub purpose: Option<String>,
    /// Prose description of conditions for entering this stage, complementing
    /// `gate_criteria` which are the machine-enforced subset.
    pub entry_conditions: Option<String>,
    /// Who is responsible for work in this stage (e.g. "engineer", "qa", "pm").
    pub responsible_role: Option<String>,
    /// What actions are allowed while a card is in this stage.
    pub allowed_actions: Vec<String>,
    /// What outputs the stage must produce before a card may leave.
    pub required_outputs: Vec<String>,
    /// Where a card goes if it fails this stage (e.g. back to "doing").
    pub failure_transition: Option<ColumnId>,
    /// Target time-in-stage in seconds; advisory, surfaced in dashboards.
    pub target_sla_seconds: Option<i64>,
}

impl BoardColumn {
    pub fn is_terminal(&self) -> bool {
        self.terminal.is_terminal()
    }
}

/// The ordered set of configured columns — the authoritative workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardWorkflow {
    columns: Vec<BoardColumn>,
}

impl BoardWorkflow {
    /// Build from configured columns, sorting by `position`.
    ///
    /// Sorting here rather than trusting the caller means `position` is the
    /// single source of progression order — which is the point of the reorder
    /// endpoint, and was not true while progression was hardcoded.
    pub fn new(mut columns: Vec<BoardColumn>) -> Self {
        columns.sort_by_key(|c| (c.position, c.id.0.clone()));
        Self { columns }
    }

    pub fn columns(&self) -> &[BoardColumn] {
        &self.columns
    }

    pub fn get(&self, id: &ColumnId) -> Option<&BoardColumn> {
        self.columns.iter().find(|c| &c.id == id)
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// The next column to the right, skipping terminal and non-progression
    /// stages.
    ///
    /// "Furthest right" is NOT blind position incrementing (spec §3). A
    /// `discarded` column sitting between `done` and `verified` by position
    /// must never be the automatic next stop — abandoning work is a decision,
    /// never a default. Same for `Waiting` roles: they are entered by an event,
    /// not by progression.
    pub fn next_column(&self, from: &ColumnId) -> Option<&BoardColumn> {
        let cur = self.get(from)?;
        if cur.is_terminal() {
            return None;
        }
        self.columns
            .iter()
            .filter(|c| c.position > cur.position)
            .find(|c| {
                !matches!(
                    c.terminal,
                    TerminalBehavior::AbandonedTerminal | TerminalBehavior::Dormant
                ) && !matches!(c.role, ColumnRole::Waiting | ColumnRole::Backlog)
            })
    }

    /// The furthest-right column a card could legitimately reach: the last
    /// successful terminal, else the last progression column.
    pub fn furthest_terminal(&self) -> Option<&BoardColumn> {
        self.columns
            .iter()
            .rev()
            .find(|c| c.terminal == TerminalBehavior::SuccessfulTerminal)
            .or_else(|| self.columns.last())
    }

    /// Is `id` a configured column of this workflow?
    ///
    /// The negative answer is load-bearing: an UNCONFIGURED column must still
    /// produce a visible card with a structured wait, never a dropped one.
    pub fn is_configured(&self, id: &ColumnId) -> bool {
        self.get(id).is_some()
    }
}

/// How a raw stored status value relates to the configured workflow (spec §23).
///
/// Every value found in the DB must land in exactly one of these, and none of
/// them is "silently gone". A historical column from a workflow that has since
/// been reconfigured keeps its card visible and recoverable rather than being
/// remapped to `todo`, which would move a user's work without telling them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ColumnClassification {
    /// Present in the configured workflow.
    Configured { id: ColumnId },
    /// A legacy spelling that resolves to a configured column
    /// (`in_progress` -> `doing`).
    LegacyAlias { raw: String, resolves_to: ColumnId },
    /// Not configured, but a real value on real cards. Visible, never dropped,
    /// never silently remapped.
    Historical { raw: String },
    /// Empty/whitespace — genuinely no column.
    Invalid { raw: String },
}

/// Classify a stored status against the configured workflow.
///
/// `aliases` maps legacy spellings to configured ids, supplied by the caller so
/// core stays free of the DB's vocabulary.
pub fn classify_status(
    raw: &str,
    workflow: &BoardWorkflow,
    aliases: &dyn Fn(&str) -> Option<ColumnId>,
) -> ColumnClassification {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ColumnClassification::Invalid { raw: raw.to_string() };
    }
    let lower = trimmed.to_lowercase();
    let id = ColumnId::new(&lower);
    if workflow.is_configured(&id) {
        return ColumnClassification::Configured { id };
    }
    if let Some(target) = aliases(&lower) {
        if workflow.is_configured(&target) {
            return ColumnClassification::LegacyAlias {
                raw: trimmed.to_string(),
                resolves_to: target,
            };
        }
    }
    ColumnClassification::Historical { raw: trimmed.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(id: &str, pos: i64, role: ColumnRole, term: TerminalBehavior) -> BoardColumn {
        BoardColumn {
            id: ColumnId::new(id),
            label: id.to_string(),
            position: pos,
            role,
            terminal: term,
            gate_criteria: vec![],
            applies_to_types: None,
            purpose: None,
            entry_conditions: None,
            responsible_role: None,
            allowed_actions: vec![],
            required_outputs: vec![],
            failure_transition: None,
            target_sla_seconds: None,
        }
    }

    /// The default board, as configured in the live DB today.
    fn builtin() -> BoardWorkflow {
        BoardWorkflow::new(vec![
            col("backlog", 0, ColumnRole::Backlog, TerminalBehavior::NonTerminal),
            col("todo", 1, ColumnRole::Dispatchable, TerminalBehavior::NonTerminal),
            col("doing", 2, ColumnRole::Active, TerminalBehavior::NonTerminal),
            col("review", 3, ColumnRole::Review, TerminalBehavior::NonTerminal),
            col("done", 4, ColumnRole::CompletionClaim, TerminalBehavior::NonTerminal),
            col("verified", 5, ColumnRole::VerifiedTerminal, TerminalBehavior::SuccessfulTerminal),
            col("discarded", 6, ColumnRole::DiscardedTerminal, TerminalBehavior::AbandonedTerminal),
        ])
    }

    /// A user-built workflow with stages that have no Rust enum variant —
    /// the case the closed enum could not express at all.
    fn custom() -> BoardWorkflow {
        BoardWorkflow::new(vec![
            col("todo", 0, ColumnRole::Dispatchable, TerminalBehavior::NonTerminal),
            col("implementation", 1, ColumnRole::Custom, TerminalBehavior::NonTerminal),
            col("qa", 2, ColumnRole::Custom, TerminalBehavior::NonTerminal),
            col("security-review", 3, ColumnRole::Review, TerminalBehavior::NonTerminal),
            col("verified", 4, ColumnRole::VerifiedTerminal, TerminalBehavior::SuccessfulTerminal),
        ])
    }

    #[test]
    fn progression_follows_configured_order_not_a_hardcoded_sequence() {
        let w = custom();
        let mut seen = vec![];
        let mut cur = ColumnId::new("todo");
        while let Some(next) = w.next_column(&cur) {
            seen.push(next.id.0.clone());
            cur = next.id.clone();
        }
        assert_eq!(
            seen,
            vec!["implementation", "qa", "security-review", "verified"],
            "a custom workflow must progress through ITS stages, not todo/doing/review/done"
        );
    }

    /// Reordering columns changes future progression — the reorder endpoint
    /// must actually mean something.
    #[test]
    fn reordering_columns_changes_progression() {
        let w = BoardWorkflow::new(vec![
            col("todo", 0, ColumnRole::Dispatchable, TerminalBehavior::NonTerminal),
            col("qa", 1, ColumnRole::Custom, TerminalBehavior::NonTerminal),
            col("implementation", 2, ColumnRole::Custom, TerminalBehavior::NonTerminal),
            col("verified", 3, ColumnRole::VerifiedTerminal, TerminalBehavior::SuccessfulTerminal),
        ]);
        assert_eq!(w.next_column(&ColumnId::new("todo")).unwrap().id.0, "qa");
        assert_eq!(w.next_column(&ColumnId::new("qa")).unwrap().id.0, "implementation");
    }

    /// A terminal column has no next stage, so a verified card is never
    /// redispatched.
    #[test]
    fn terminal_columns_have_no_next() {
        assert!(builtin().next_column(&ColumnId::new("verified")).is_none());
        assert!(builtin().next_column(&ColumnId::new("discarded")).is_none());
    }

    /// THE §16 TRAP: discarded sits at position 6, to the RIGHT of verified.
    /// Blind position-incrementing would make abandoning work the automatic
    /// next step after the success state.
    #[test]
    fn abandoned_terminal_is_never_the_automatic_next_stage() {
        let w = builtin();
        assert_eq!(w.next_column(&ColumnId::new("done")).unwrap().id.0, "verified");
        assert_eq!(
            w.furthest_terminal().unwrap().id.0,
            "verified",
            "the furthest legitimate terminal is the SUCCESSFUL one, not the rightmost column"
        );
    }

    /// A user appending an archive column on the far right must not silently
    /// become the success state.
    #[test]
    fn appending_a_rightmost_column_does_not_redefine_success() {
        let mut cols = builtin().columns().to_vec();
        cols.push(col("archive", 99, ColumnRole::Custom, TerminalBehavior::NonTerminal));
        let w = BoardWorkflow::new(cols);
        assert_eq!(
            w.furthest_terminal().unwrap().id.0,
            "verified",
            "terminal semantics are explicit, never 'rightmost wins'"
        );
    }

    /// An unconfigured column must classify as Historical — visible and
    /// recoverable — never remapped to todo and never dropped.
    #[test]
    fn unconfigured_status_is_historical_not_remapped() {
        let w = custom();
        let c = classify_status("needsyou", &w, &|_| None);
        assert_eq!(c, ColumnClassification::Historical { raw: "needsyou".into() });
    }

    /// Legacy spellings resolve without rewriting the stored value.
    #[test]
    fn legacy_aliases_resolve_to_configured_columns() {
        let w = builtin();
        let alias = |s: &str| match s {
            "in_progress" | "wip" => Some(ColumnId::new("doing")),
            _ => None,
        };
        assert_eq!(
            classify_status("in_progress", &w, &alias),
            ColumnClassification::LegacyAlias {
                raw: "in_progress".into(),
                resolves_to: ColumnId::new("doing")
            }
        );
    }

    /// EVERY status value present in the live production board must classify
    /// as something visible (spec §23). Nothing may fall into a hole.
    #[test]
    fn every_production_status_classifies_visibly() {
        let w = builtin();
        let alias = |s: &str| match s {
            "resolved" | "complete" | "completed" | "closed" => Some(ColumnId::new("done")),
            "in_progress" | "wip" | "inprogress" => Some(ColumnId::new("doing")),
            "in_review" | "inreview" => Some(ColumnId::new("review")),
            "needs_you" => Some(ColumnId::new("needsyou")),
            _ => None,
        };
        // Taken from the live DB: SELECT DISTINCT status FROM issues.
        for raw in [
            "verified", "done", "discarded", "backlog", "todo", "review", "doing", "needsyou",
            "blocked", "armed", "resolved",
        ] {
            let c = classify_status(raw, &w, &alias);
            assert!(
                !matches!(c, ColumnClassification::Invalid { .. }),
                "production status {raw:?} must remain visible, got {c:?}"
            );
        }
    }

    /// Custom columns are dispatchable by default, or a user-defined stage
    /// becomes a silent dead end where cards accumulate with no next action.
    #[test]
    fn custom_columns_are_dispatchable() {
        assert!(ColumnRole::Custom.is_dispatchable());
        assert!(!ColumnRole::Backlog.is_dispatchable(), "backlog is human triage");
    }
}
