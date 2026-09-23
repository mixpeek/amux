//! Explicit operator grants on the existing receipt, never counter resets.
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub idempotency_key: String,
    pub expect_attempts: i64,
    pub expect_revision: usize,
}

pub fn grant(
    conn: &Connection,
    project: &str,
    id: i64,
    body: &Request,
    now: i64,
) -> anyhow::Result<WriteOutcome> {
    anyhow::ensure!(
        !body.idempotency_key.trim().is_empty() && body.idempotency_key.len() <= 160,
        "retry idempotency key required (1..160 bytes)"
    );
    let (pending, attempts, retry_at, raw, meta): (bool, i64, i64, Option<String>, Option<String>) = conn.query_row(
        "SELECT capture_pending,intake_attempts,intake_retry_at,intake_result,client_meta FROM cmd_history WHERE session='project:'||project_group AND type='user' AND id=?1 AND project_group=?2",
        params![id,project], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    let mut meta: Value = meta
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or(json!({}));
    let mut grants = meta
        .get("intake_retries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(prior) = grants.iter().find(|g| g["key"] == body.idempotency_key) {
        anyhow::ensure!(
            prior["expect_attempts"] == body.expect_attempts
                && prior["expect_revision"] == body.expect_revision,
            "retry key already belongs to another request"
        );
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    anyhow::ensure!(
        pending && attempts == body.expect_attempts && grants.len() == body.expect_revision,
        "receipt revision changed; reload before retrying"
    );
    anyhow::ensure!(
        attempts >= 2 + grants.len() as i64,
        "intake still has an available attempt"
    );
    anyhow::ensure!(
        retry_at <= now,
        "intake is in flight or its retry lease has not expired"
    );
    let previous: Value = raw
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or(Value::Null);
    anyhow::ensure!(
        previous.get("waiting_on").is_none(),
        "retry the original receipt, not its duplicate"
    );
    anyhow::ensure!(
        previous["state"] != "prepared" && previous.get("error").is_some(),
        "receipt has no failed interpretation to retry"
    );
    grants.push(json!({"key":body.idempotency_key,"expect_attempts":attempts,"expect_revision":body.expect_revision,"granted_at":now,"previous_result":previous}));
    meta["intake_retries"] = json!(grants);
    conn.execute(
        "UPDATE cmd_history SET client_meta=?2,intake_retry_at=0 WHERE id=?1",
        params![id, meta.to_string()],
    )?;
    tracing::info!(
        project,
        message_id = id,
        attempts,
        revision = grants.len(),
        measured = true,
        n_considered = 1,
        verdict = "project_intake_retry_granted",
        "operator authorized one additional interpretation; attempt counters and history retained"
    );
    Ok(WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: EntityType::Message,
            entity_id: format!("MSG-{id}"),
            mutation: MutationKind::Updated,
            payload: Some(
                json!({"project_group":project,"retry_revision":grants.len(),"attempt_limit":2+grants.len()}),
            ),
        }],
    })
}
