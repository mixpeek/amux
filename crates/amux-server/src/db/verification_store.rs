//! Structured verification persistence (migration 0044).
//!
//! Verification outcomes were previously recorded only in the card's append-only
//! `log` text and `last_verified_at` timestamp. This store makes them queryable:
//! gates can evaluate structured records rather than parsing log text, and the
//! full history of attempts (including failures and retries) is preserved with
//! actor attribution and evidence.

use rusqlite::{params, Connection, OptionalExtension, Row};

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
    })
}

const COLS: &str = "id, task_id, verifier, criteria, evidence, verdict, reason, \
                    run_detail, actor, created_at";

pub fn insert(conn: &Connection, row: &VerificationRow) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO _amux_verifications \
             (id, task_id, verifier, criteria, evidence, verdict, reason, run_detail, actor, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
        ],
    )
}

pub fn list_for_task(conn: &Connection, task_id: &str) -> rusqlite::Result<Vec<VerificationRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM _amux_verifications WHERE task_id = ?1 ORDER BY created_at DESC"
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
             WHERE task_id = ?1 ORDER BY created_at DESC LIMIT 1"
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
