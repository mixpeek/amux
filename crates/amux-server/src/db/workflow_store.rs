//! Load the canonical [`BoardWorkflow`] from the `statuses` table (AMUX-2631).
//!
//! This is the seam that was missing: `statuses` is where columns and their
//! gates are CONFIGURED (the API writes it at `POST/PATCH/DELETE
//! /api/board/statuses` and `PUT /api/board/statuses/reorder`), and nothing in
//! the Rust orchestrator ever read it. Progression came from a hardcoded
//! sequence and gates came from a hardcoded `match`, so the configuration was
//! write-only.

use amux_core::board::GateCriterion;
use amux_core::verification::VerifierKind;
use amux_core::workflow::{BoardColumn, BoardWorkflow, ColumnId, ColumnRole, TerminalBehavior};
use rusqlite::Connection;

/// Semantic defaults for the seven built-in column ids.
///
/// Only these get typed roles automatically. Anything else is `Custom` +
/// `NonTerminal`, which is the safe default: a user-created column
/// participates in ordering and gating but can never accidentally be treated
/// as a terminal success state (spec §16).
fn builtin_semantics(id: &str) -> Option<(ColumnRole, TerminalBehavior)> {
    Some(match id {
        "backlog" => (ColumnRole::Backlog, TerminalBehavior::NonTerminal),
        "todo" => (ColumnRole::Dispatchable, TerminalBehavior::NonTerminal),
        "doing" => (ColumnRole::Active, TerminalBehavior::NonTerminal),
        "review" => (ColumnRole::Review, TerminalBehavior::NonTerminal),
        "done" => (ColumnRole::CompletionClaim, TerminalBehavior::NonTerminal),
        "verified" => (ColumnRole::VerifiedTerminal, TerminalBehavior::SuccessfulTerminal),
        "discarded" => (ColumnRole::DiscardedTerminal, TerminalBehavior::AbandonedTerminal),
        // Not built-in: caller falls back to Custom/NonTerminal.
        _ => return None,
    })
}

/// Read the configured workflow.
///
/// Returns `None` — never a silently-empty workflow — when the table cannot be
/// read. An empty `BoardWorkflow` would make every card's column
/// "unconfigured" and every next_column `None`, i.e. it would look exactly like
/// a board with nothing to do. A failed read must be distinguishable from a
/// board in order (the `unknown is not zero` rule).
pub fn load_workflow(conn: &Connection) -> Option<BoardWorkflow> {
    let mut stmt = conn
        .prepare(
            "SELECT id, label, position, COALESCE(gate,''), COALESCE(mode,'implicit'), \
             purpose, entry_conditions, responsible_role, allowed_actions, \
             required_outputs, failure_transition, target_sla_seconds \
             FROM statuses ORDER BY position"
        )
        .ok()?;
    let cols: Vec<BoardColumn> = stmt
        .query_map([], |r| {
            let id: String = r.get(0)?;
            let label: String = r.get(1)?;
            let position: i64 = r.get(2)?;
            let gate_json: String = r.get(3)?;
            let _mode: String = r.get(4)?;
            let purpose: Option<String> = r.get(5)?;
            let entry_conditions: Option<String> = r.get(6)?;
            let responsible_role: Option<String> = r.get(7)?;
            let allowed_actions_json: Option<String> = r.get(8)?;
            let required_outputs_json: Option<String> = r.get(9)?;
            let failure_transition: Option<String> = r.get(10)?;
            let target_sla_seconds: Option<i64> = r.get(11)?;
            Ok((id, label, position, gate_json, purpose, entry_conditions,
                responsible_role, allowed_actions_json, required_outputs_json,
                failure_transition, target_sla_seconds))
        })
        .ok()?
        .flatten()
        .map(|(id, label, position, gate_json, purpose, entry_conditions,
               responsible_role, allowed_actions_json, required_outputs_json,
               failure_transition, target_sla_seconds)| {
            let (role, terminal) = builtin_semantics(&id)
                .unwrap_or((ColumnRole::Custom, TerminalBehavior::NonTerminal));
            BoardColumn {
                id: ColumnId::new(&id),
                label,
                position,
                role,
                terminal,
                gate_criteria: parse_gate_json(&gate_json),
                applies_to_types: None,
                purpose,
                entry_conditions,
                responsible_role,
                allowed_actions: parse_string_array_json(allowed_actions_json.as_deref()),
                required_outputs: parse_string_array_json(required_outputs_json.as_deref()),
                failure_transition: failure_transition.map(ColumnId::new),
                target_sla_seconds,
            }
        })
        .collect();
    if cols.is_empty() {
        return None;
    }
    Some(BoardWorkflow::new(cols))
}

fn parse_string_array_json(raw: Option<&str>) -> Vec<String> {
    match raw {
        None => Vec::new(),
        Some(s) if s.trim().is_empty() => Vec::new(),
        Some(s) => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
    }
}

/// `statuses.gate` is a JSON array of criterion strings (Python's shape).
///
/// Criteria verify as `ModelJudgment`, matching what `board_store::core_gates`
/// already does, so both gate sources produce the same criterion type and one
/// evaluation path stays possible.
fn parse_gate_json(raw: &str) -> Vec<GateCriterion> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(|c| GateCriterion {
            description: c.clone(),
            verifier: VerifierKind::ModelJudgment { prompt: c },
            required: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE statuses (id TEXT PRIMARY KEY, label TEXT NOT NULL,
                position INTEGER NOT NULL DEFAULT 0, is_builtin INTEGER NOT NULL DEFAULT 0,
                gate TEXT, mode TEXT NOT NULL DEFAULT 'implicit',
                purpose TEXT, entry_conditions TEXT, responsible_role TEXT,
                allowed_actions TEXT, required_outputs TEXT,
                failure_transition TEXT, target_sla_seconds INTEGER);",
        )
        .unwrap();
        // Exactly the live production seed.
        for (id, label, pos, gate) in [
            ("backlog", "Backlog", 0, ""),
            ("todo", "To Do", 1, ""),
            ("doing", "In Progress", 2, r#"["Scope & acceptance criteria are clear"]"#),
            ("review", "In Review", 3, r#"["Implemented and self-tested"]"#),
            ("done", "Done", 4, r#"["Implemented and merged","Tests / lint pass"]"#),
            ("verified", "Verified", 5, r#"["CI/CD green (incl. e2e)"]"#),
            ("discarded", "Discarded", 6, ""),
        ] {
            c.execute(
                "INSERT INTO statuses (id,label,position,is_builtin,gate) VALUES (?,?,?,1,?)",
                rusqlite::params![id, label, pos, gate],
            )
            .unwrap();
        }
        c
    }

    #[test]
    fn loads_the_live_production_workflow() {
        let w = load_workflow(&seeded()).expect("workflow loads");
        assert_eq!(w.columns().len(), 7);
        assert_eq!(w.next_column(&ColumnId::new("todo")).unwrap().id.0, "doing");
        assert_eq!(w.furthest_terminal().unwrap().id.0, "verified");
    }

    /// THE GATE THAT WAS BEING IGNORED. `statuses.gate` must reach the
    /// workflow, or editing a column's gate in the UI stays write-only —
    /// which is the live defect this module exists to close.
    #[test]
    fn configured_column_gates_are_loaded() {
        let w = load_workflow(&seeded()).unwrap();
        let done = w.get(&ColumnId::new("done")).unwrap();
        assert_eq!(done.gate_criteria.len(), 2);
        assert_eq!(done.gate_criteria[0].description, "Implemented and merged");
    }

    /// A gate EDITED through the API must change what the workflow enforces.
    /// Before this loader existed the UPDATE landed in the DB and enforcement
    /// kept using the hardcoded defaults — two sources agreeing only by
    /// coincidence.
    #[test]
    fn an_edited_column_gate_changes_the_loaded_workflow() {
        let c = seeded();
        c.execute(
            "UPDATE statuses SET gate = ? WHERE id = 'done'",
            [r#"["Security scan green","Customer sign-off"]"#],
        )
        .unwrap();
        let w = load_workflow(&c).unwrap();
        let done = w.get(&ColumnId::new("done")).unwrap();
        let descs: Vec<_> = done.gate_criteria.iter().map(|g| g.description.as_str()).collect();
        assert_eq!(
            descs,
            vec!["Security scan green", "Customer sign-off"],
            "an edited column gate must reach enforcement"
        );
    }

    /// A user-created column loads as a first-class stage: ordered, gateable,
    /// dispatchable, and NOT terminal.
    #[test]
    fn custom_columns_become_first_class_stages() {
        let c = seeded();
        c.execute(
            "UPDATE statuses SET position = 9 WHERE id = 'verified'",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO statuses (id,label,position,is_builtin,gate) VALUES
             ('security-review','Security Review',7,0,'[\"Security scan green\"]')",
            [],
        )
        .unwrap();
        let w = load_workflow(&c).unwrap();
        let sr = w.get(&ColumnId::new("security-review")).expect("custom column present");
        assert_eq!(sr.role, ColumnRole::Custom);
        assert_eq!(sr.terminal, TerminalBehavior::NonTerminal, "custom must never be terminal by accident");
        assert_eq!(sr.gate_criteria.len(), 1);
        assert!(sr.role.is_dispatchable(), "a custom stage must not be a dead end");
        // ...and it participates in ordering: done(4) -> security-review(7) -> verified(9).
        assert_eq!(w.next_column(&ColumnId::new("done")).unwrap().id.0, "security-review");
        assert_eq!(w.next_column(&ColumnId::new("security-review")).unwrap().id.0, "verified");
    }

    /// A failed read must be None, not an empty workflow — an empty workflow
    /// makes every card unconfigured and every next_column None, which is
    /// indistinguishable from a board with nothing left to do.
    #[test]
    fn a_missing_table_is_none_not_an_empty_workflow() {
        let c = Connection::open_in_memory().unwrap();
        assert!(load_workflow(&c).is_none(), "unreadable must not look like an ordered empty board");
    }
}

// ---------------------------------------------------------------------------
// The invisibility bug this module exists to end (AMUX-2632).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod invisibility_tests {
    use crate::db::board_store::parse_status;

    /// THE ROOT DEFECT, pinned so it cannot be reintroduced quietly.
    ///
    /// `IssueRow::to_task()` is `Option<Task>` and starts with
    /// `parse_status(&self.status)?`; `runtime::load_board_tasks` consumes it as
    /// `let Some(task) = row.to_task() else { continue }`. So a card whose
    /// column is not in the CLOSED `TaskStatus` enum is not merely
    /// mis-classified — it is DROPPED from the planning set entirely. No
    /// disposition, no next action, no assignment, no stall detection, and
    /// nothing anywhere reports that a card went missing.
    ///
    /// The API already lets a user create exactly such a column
    /// (`POST /api/board/statuses {label:"Security Review"}`), so this is not
    /// hypothetical — it is armed, and the only reason it has not fired is that
    /// every status currently in the production DB happens to parse.
    ///
    /// A row that differs from the default in exactly one way: the column it
    /// sits in. That is the variable under test; spelling out the other
    /// twenty-five fields would bury it.
    fn row_in_status(status: &str) -> crate::db::board_store::IssueRow {
        crate::db::board_store::IssueRow {
            id: format!("T-{status}"),
            title: "a card".into(),
            status: status.into(),
            item_type: "code".into(),
            creator: "tester".into(),
            ..Default::default()
        }
    }

    /// THE ASSERTION FLIPPED (AMUX-2632). This test was the pre-fix specimen and
    /// said so: "when unconfigured columns become visible structured waits, this
    /// assertion flips, and that flip is the fix landing." It has landed.
    ///
    /// It now tests the level that actually carries the invariant. The old
    /// version asserted on `parse_status`, which is a DETAIL — and the detail is
    /// unchanged, deliberately: the enum stays closed, because "security-review"
    /// is genuinely not a member of the shared vocabulary and a parser that
    /// guessed would make every consumer's match arm a lie. What must never
    /// happen is a card VANISHING, and that is a property of `to_task`.
    #[test]
    fn a_custom_column_is_visible_to_the_orchestrator_as_blocked() {
        // The vocabulary is still closed — this half is intentionally unchanged.
        assert_eq!(parse_status("security-review"), None);
        assert_eq!(parse_status("qa"), None);
        assert_eq!(parse_status("customer-validation"), None);

        // ...but the card no longer disappears. Before this fix `to_task()`
        // returned None here and the orchestrator's `else { continue }` dropped
        // the card with no trace anywhere.
        for column in ["security-review", "qa", "customer-validation"] {
            let row = row_in_status(column);
            let task = row
                .to_task()
                .unwrap_or_else(|| panic!("a card in '{column}' must not vanish from the board"));
            assert_eq!(
                task.status,
                amux_core::board::TaskStatus::Blocked,
                "'{column}' is blocked on configuration the orchestrator cannot model"
            );
        }

        // A modelled column is untouched — the mapping must not swallow real
        // statuses on its way past.
        let row = row_in_status("doing");
        assert_eq!(row.to_task().unwrap().status, amux_core::board::TaskStatus::Doing);
    }

    /// Every status value present in the LIVE production board parses today.
    /// This is why the bug above is latent rather than active, and it is worth
    /// pinning: if a future migration introduces a value that stops parsing,
    /// this test fails loudly instead of cards silently disappearing.
    ///
    /// Values taken from `SELECT DISTINCT status FROM issues` on the real DB.
    #[test]
    fn every_live_production_status_parses_today() {
        for raw in [
            "verified", "done", "discarded", "backlog", "todo", "review", "doing", "needsyou",
            "blocked", "armed", "resolved",
        ] {
            assert!(
                parse_status(raw).is_some(),
                "production status {raw:?} stopped parsing — cards in it are now INVISIBLE \
                 to the orchestrator, not merely mislabelled"
            );
        }
    }
}
