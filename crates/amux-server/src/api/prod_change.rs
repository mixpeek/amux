//! Contract A1, phase 1 (AH-388): a planned production change runs as a
//! procedure the server gates, not one a lane remembers.
//!
//! A card tagged `prod-change` on a contract lane (`contract::enabled_for`)
//! cannot enter doing until:
//! - a notice went to the owner at least 30 minutes earlier. The first doing
//!   request raises it: a needsyou card addressed to the owner, ask_type
//!   decision. Silence proceeds once the 30 minutes pass; approving the notice
//!   (moving it out of needsyou to todo, doing, done or verified) proceeds at
//!   once.
//! - the owner has not held it: the notice moved to blocked or discarded, or a
//!   reply line starting "no" or "hold", holds the change until the owner
//!   moves the notice again.
//!
//! The positive control is the card's frozen verify command (contract rule 1),
//! which the server runs at done (rule 2) against production. For a
//! prod-change card a failing control does not leave the card quietly in
//! doing: it goes to the owner as a needsyou hold and the lane is told to roll
//! back. Blue/green itself is the lane's procedure; the harness owns the gates.
//!
//! Verdicts (contract rule 14, under "A1"): a1_notice_raised,
//! a1_doing_refused_notice, a1_held_by_owner, a1_positive_control_failed.
use crate::api::AppState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{Connection, OptionalExtension};
use serde_json::json;

pub const TAG: &str = "prod-change";
pub const NOTICE_LEAD_S: f64 = 1800.0;

/// The notice as stored, plus the notice card's current state.
#[derive(Clone, Debug)]
pub struct Notice {
    pub notice: String,
    pub raised_at: f64,
    pub status: String,
    /// Text appended to the notice after it was raised (the owner's reply).
    pub reply: String,
}

#[derive(Debug, PartialEq)]
pub enum Gate {
    /// The change may start.
    Ready,
    /// No notice yet: raise one and refuse.
    Raise,
    /// The notice is out; the change may start at `ready_at`.
    Wait { ready_at: f64 },
    /// The owner held it.
    Held,
}

fn owner_held(n: &Notice) -> bool {
    matches!(n.status.as_str(), "blocked" | "discarded")
        || n.reply.lines().any(|l| {
            let w: String = l
                .trim_start()
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            w == "no" || w == "hold"
        })
}

/// The A1 gate as a pure decision.
pub fn gate(notice: Option<&Notice>, now: f64) -> Gate {
    let Some(n) = notice else { return Gate::Raise };
    if owner_held(n) {
        return Gate::Held;
    }
    if matches!(n.status.as_str(), "todo" | "doing" | "done" | "verified") {
        return Gate::Ready; // the owner approved early
    }
    let ready_at = n.raised_at + NOTICE_LEAD_S;
    if now >= ready_at {
        Gate::Ready
    } else {
        Gate::Wait { ready_at }
    }
}

pub fn is_prod_change(tags: &[String]) -> bool {
    tags.iter().any(|t| t == TAG)
}

fn load(conn: &Connection, card: &str) -> rusqlite::Result<Option<Notice>> {
    let row: Option<(String, f64, i64)> = conn
        .query_row(
            "SELECT notice, raised_at, base_len FROM prod_change_notices WHERE card = ?1",
            [card],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((notice, raised_at, base_len)) = row else { return Ok(None) };
    let (status, desc) = match crate::db::board_store::get_issue(conn, &notice)? {
        Some(r) => (r.status, r.desc),
        // A notice deleted out from under the gate counts as no notice.
        None => return Ok(None),
    };
    let reply = desc.get(base_len.max(0) as usize..).unwrap_or("").to_string();
    Ok(Some(Notice { notice, raised_at, status, reply }))
}

fn utc(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_default()
}

fn refuse(code: &str, error: String, extra: serde_json::Value) -> Response {
    let mut body = json!({"ok": false, "blocked": true, "code": code, "error": error,
        "contract": "docs/orchestration-contract.md (A1)"});
    if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    (StatusCode::CONFLICT, Json(body)).into_response()
}

fn new_notice(card: &str, lane: &str, title: &str, ready_at: f64) -> crate::db::board_store::NewIssue {
    let desc = format!(
        "{lane} plans a production change on {card}: {title}\n\nIt starts no earlier than {} unless you hold it. \
         Silence proceeds then; approving this card (move it to todo or done) lets it start now. \
         To hold it, move this card to blocked or reply with a line starting \"no\" or \"hold\".",
        utc(ready_at)
    );
    crate::db::board_store::NewIssue {
        title: format!("Notice: planned production change {card} starts no earlier than {} ({title})", utc(ready_at)),
        desc,
        status: "needsyou".into(),
        session: Some(lane.to_string()),
        item_type: "decision".into(),
        creator: "harness:a1".into(),
        owner_type: "human".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags: vec!["prod-change-notice".into()],
        ask_type: Some("decision".into()),
        next_action: None,
        acceptance_criteria: None,
        ask_question: Some(format!(
            "Hold {card}, a planned production change by {lane}? Silence lets it start at {}.",
            utc(ready_at)
        )),
        ask_unblocks: Some(format!("{card} starts at {} unless you hold it.", utc(ready_at))),
        ask_actor: Some(crate::api::turn_end::owner_name()),
        source: Some("a1_notice".into()),
        requested_by: None,
        callback_session: None,
        callback_prompt: None,
    }
}

/// Gate a worker's request to move a prod-change card into doing. None means
/// let the request through.
pub async fn guard(state: &AppState, card: &str, lane: &str, title: &str) -> Option<Response> {
    let now = crate::config::now_f64();
    let c = card.to_string();
    let notice = state.store.read_async(move |conn| Ok(load(conn, &c)?)).await.ok().flatten();
    match gate(notice.as_ref(), now) {
        Gate::Ready => None,
        Gate::Held => {
            let n = notice.map(|n| n.notice).unwrap_or_default();
            tracing::info!(card, lane, notice = %n, measured = true, n_considered = 1, verdict = "a1_held_by_owner",
                "a planned production change is held by the owner");
            Some(refuse(
                "a1_held_by_owner",
                format!("{card} is a planned production change the owner held on {n}"),
                json!({"notice": n, "next": "wait for the owner to move the notice card again; do not start the change"}),
            ))
        }
        Gate::Wait { ready_at } => {
            let n = notice.map(|n| n.notice).unwrap_or_default();
            tracing::info!(card, lane, notice = %n, ready_at, measured = true, n_considered = 1,
                verdict = "a1_doing_refused_notice", "a planned production change asked to start inside its notice window");
            Some(refuse(
                "a1_doing_refused_notice",
                format!("{card} may start at {} (30 minutes after notice {n})", utc(ready_at)),
                json!({"notice": n, "ready_at": ready_at, "ready_at_utc": utc(ready_at)}),
            ))
        }
        Gate::Raise => {
            let ready_at = now + NOTICE_LEAD_S;
            let new = new_notice(card, lane, title, ready_at);
            let c = card.to_string();
            let r = state
                .store
                .write_async(move |conn| {
                    let row = crate::db::board_store::create_issue(conn, &new, now as i64)?;
                    conn.execute(
                        "INSERT OR REPLACE INTO prod_change_notices (card, notice, raised_at, base_len) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![c, row.id, now, row.desc.len() as i64],
                    )?;
                    Ok(crate::db::WriteOutcome {
                        applied: true,
                        events: vec![crate::db::PendingEvent {
                            entity_type: amux_core::revision::EntityType::Task,
                            entity_id: row.id.clone(),
                            mutation: amux_core::revision::MutationKind::Created,
                            payload: Some(json!({"id": row.id, "source": "a1_notice"})),
                        }],
                    })
                })
                .await;
            let c = card.to_string();
            let id = state
                .store
                .read_async(move |conn| Ok(load(conn, &c)?.map(|n| n.notice)))
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            tracing::info!(card, lane, notice = %id, ready_at, ok = r.is_ok(), measured = true, n_considered = 1,
                verdict = "a1_notice_raised", "raised the owner notice for a planned production change");
            Some(refuse(
                "a1_doing_refused_notice",
                format!(
                    "{card} is a planned production change: notice {id} went to the owner; it may start at {}",
                    utc(ready_at)
                ),
                json!({"notice": id, "ready_at": ready_at, "ready_at_utc": utc(ready_at)}),
            ))
        }
    }
}

/// After a prod-change card's positive control failed: put it on hold with
/// the owner. Returns true when the card was a prod-change card (the caller
/// then tells the lane to roll back).
pub fn hold_on_failed_control(conn: &Connection, card: &str, why: &str) -> rusqlite::Result<bool> {
    let Some(mut row) = crate::db::board_store::get_issue(conn, card)? else { return Ok(false) };
    if !is_prod_change(&row.tags) {
        return Ok(false);
    }
    row.ask_actor = Some(crate::api::turn_end::owner_name());
    row.ask_type = Some("decision".into());
    row.ask_question = Some(format!(
        "{card}'s positive control failed after the production change (contract A1): confirm the rollback, or decide what happens next?"
    ));
    row.ask_unblocks = Some("Your decision; the lane was told to roll back and the control output is in the card.".into());
    row.desc.push_str(&format!(
        "\nA1 hold: the positive control failed after the change. {}",
        why.chars().take(800).collect::<String>()
    ));
    let from = row.status.clone();
    crate::db::board_store::save_patched(conn, &mut row)?;
    let opts = crate::db::advance::AdvanceOpts {
        expected_from: Some(from),
        gate_ack: true,
        skip_continuation: true,
        reason: Some("contract A1: positive control failed; held for the owner".into()),
        ..Default::default()
    };
    let _ = crate::db::advance::advance(conn, card, "needsyou", crate::api::contract::ACTOR, &opts)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(status: &str, reply: &str) -> Notice {
        Notice { notice: "N-1".into(), raised_at: 1000.0, status: status.into(), reply: reply.into() }
    }

    #[test]
    fn a_change_waits_thirty_minutes_after_its_notice_unless_held_or_approved() {
        assert_eq!(gate(None, 0.0), Gate::Raise);
        assert_eq!(gate(Some(&n("needsyou", "")), 1000.0 + 1799.0), Gate::Wait { ready_at: 2800.0 });
        assert_eq!(gate(Some(&n("needsyou", "")), 2800.0), Gate::Ready, "silence proceeds at 30 minutes");
        assert_eq!(gate(Some(&n("todo", "")), 1001.0), Gate::Ready, "approval proceeds at once");
        assert_eq!(gate(Some(&n("blocked", "")), 9999.0), Gate::Held);
        assert_eq!(gate(Some(&n("needsyou", "\nNo, not during the soak")), 9999.0), Gate::Held);
        assert_eq!(gate(Some(&n("needsyou", "\n- hold until Monday")), 9999.0), Gate::Held);
        assert_eq!(gate(Some(&n("needsyou", "\nnotes: nothing to add")), 9999.0), Gate::Ready, "'notes' is not 'no'");
    }
}
