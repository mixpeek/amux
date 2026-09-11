//! Structured verification persistence (migration 0044).
//!
//! Verification outcomes were previously recorded only in the card's append-only
//! `log` text and `last_verified_at` timestamp. This store makes them queryable:
//! gates can evaluate structured records rather than parsing log text, and the
//! full history of attempts (including failures and retries) is preserved with
//! actor attribution and evidence.

use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone, serde::Serialize)]
pub struct VerificationRow {
    pub id: String,
    pub task_id: String,
    /// JSON-serialized `Actor` (who reached the conclusion).
    pub verifier: String,
    /// JSON array of `CriterionId`s checked.
    pub criteria: String,
    /// JSON array of `Evidence` records.
    pub evidence: String,
    /// `"passed"` or `"failed"`.
    pub verdict: String,
    /// Failure reason (None when passed).
    pub reason: Option<String>,
    /// JSON-serialized `CriteriaRun` for full replay.
    pub run_detail: Option<String>,
    /// Session name or `"api-anonymous"`.
    pub actor: String,
    /// Unix seconds.
    pub created_at: i64,
    pub criteria_version: u32,
    pub harness_version: Option<String>,
    pub duration_ms: u64,
}

fn row_to_verification(r: &Row<'_>) -> rusqlite::Result<VerificationRow> {
    Ok(VerificationRow {
        id: r.get(0)?,
        task_id: r.get(1)?,
        verifier: r.get(2)?,
        criteria: r.get(3)?,
        evidence: r.get(4)?,
        verdict: r.get(5)?,
        reason: r.get(6)?,
        run_detail: r.get(7)?,
        actor: r.get(8)?,
        created_at: r.get(9)?,
        criteria_version: r.get::<_, i64>(10)? as u32,
        harness_version: r.get(11)?,
        duration_ms: r.get::<_, i64>(12)? as u64,
    })
}

const COLS: &str = "id, task_id, verifier, criteria, evidence, verdict, reason, \
                    run_detail, actor, created_at, criteria_version, harness_version, duration_ms";

pub fn insert(conn: &Connection, row: &VerificationRow) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO _amux_verifications \
             (id, task_id, verifier, criteria, evidence, verdict, reason, run_detail, actor, created_at,
              criteria_version, harness_version, duration_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            row.id,
            row.task_id,
            row.verifier,
            row.criteria,
            row.evidence,
            row.verdict,
            row.reason,
            row.run_detail,
            row.actor,
            row.created_at,
            row.criteria_version,
            row.harness_version,
            row.duration_ms,
        ],
    )
}

pub fn list_for_task(conn: &Connection, task_id: &str) -> rusqlite::Result<Vec<VerificationRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM _amux_verifications WHERE task_id = ?1 ORDER BY created_at DESC, rowid DESC"
    ))?;
    let rows = stmt.query_map(params![task_id], row_to_verification)?;
    rows.collect()
}

#[allow(dead_code)]
pub(crate) fn latest_for_task(
    conn: &Connection,
    task_id: &str,
) -> rusqlite::Result<Option<VerificationRow>> {
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM _amux_verifications \
             WHERE task_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1"
        ),
        params![task_id],
        row_to_verification,
    )
    .optional()
}

/// Returns `(passed_count, failed_count)` for a task.
#[allow(dead_code)]
pub(crate) fn count_by_verdict(conn: &Connection, task_id: &str) -> rusqlite::Result<(i64, i64)> {
    let passed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _amux_verifications WHERE task_id = ?1 AND verdict = 'passed'",
        params![task_id],
        |r| r.get(0),
    )?;
    let failed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _amux_verifications WHERE task_id = ?1 AND verdict = 'failed'",
        params![task_id],
        |r| r.get(0),
    )?;
    Ok((passed, failed))
}

/// Project current coverage without pretending a gate acknowledgement is an
/// independently executed test. Historical attempts remain in the same ledger.
pub fn coverage(conn: &Connection, row: &super::board_store::IssueRow, gate: &[String]) -> rusqlite::Result<serde_json::Value> {
    let criteria = crate::api::criteria::load(conn, &row.id)?;
    let history = list_for_task(conn, &row.id)?;
    let latest = history.first();
    let typed = history.iter().find(|v| v.verdict == "passed" || v.verdict == "failed");
    let snapshot = history.iter().find_map(|v| {
        if v.verdict == "acknowledged" { serde_json::from_str::<Vec<String>>(&v.criteria).ok() }
        else if v.verdict == "passed" { v.run_detail.as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|run| serde_json::from_value::<Vec<String>>(run["gate_snapshot"].clone()).ok()) }
        else { None }
    });
    let gate_matches = snapshot.as_ref().map(|recorded| {
        let old: std::collections::BTreeSet<_> = recorded.iter().collect();
        let current: std::collections::BTreeSet<_> = gate.iter().collect();
        old == current
    });
    let typed_current = criteria.as_ref().map(|c| typed.is_some_and(|v| v.verdict == "passed" && v.criteria_version == c.version));
    let stale = row.status == "verified" && (gate_matches == Some(false) || typed_current == Some(false));
    Ok(serde_json::json!({
        "state": if stale { "needs_reverification" } else if row.status != "verified" { "not_verified" }
            else if latest.is_none() { "history_unavailable" } else { "current" },
        "criteria_version": criteria.as_ref().map(|c| c.version),
        "criteria": criteria.map(|c| c.criteria),
        "verified_criteria_version": typed.map(|v| v.criteria_version),
        "gate_matches": gate_matches,
        "gate_snapshot": snapshot,
        "method": latest.map(|v| if v.verdict == "acknowledged" { "gate_acknowledgement" } else { "independent_harness" }),
        "actor": latest.map(|v| &v.actor),
        "checked_at": latest.map(|v| v.created_at),
        "attempts": history.len(),
    }))
}
