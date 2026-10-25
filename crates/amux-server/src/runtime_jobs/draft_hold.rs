//! Tell the owner when queued messages wait behind a composer draft
//! (gs12 review, 2026-10-18, finding 7).
//!
//! # What happened
//!
//! The steering loop never types over text already in a worker's composer
//! (`steering_draft_preserved`): that text may be the owner's own half-written
//! prompt, and only its author can decide whether to send it or throw it away.
//! But the hold had no visible state and no exit. Four mxp-gs12 windows held
//! owner messages 55 to 375 minutes, about 16 hours in all; one owner approval
//! for a production Mongo restart arrived 4.5 h after the time it asked about.
//! Each window ended only when the owner happened to type "continue" into the
//! terminal. The stall line meanwhile said the lane "should have been forced
//! through mid-turn", which is not what a draft hold is.
//!
//! # What this does
//!
//! Draft-aware delivery keeps the draft: amux still never clears, overwrites or
//! re-types it (a draft can hold `[Pasted text]` placeholders or a pasted
//! secret, neither of which can be restored faithfully). What changes is that
//! the hold is legible and has an owner:
//!
//! - the stall reporter names it (`steering_held_by_draft`, condition
//!   `composer-draft`) and the queue listing says what each row waits for;
//! - after [`alert_after_s`] (`AMUX_STEER_DRAFT_ALERT_MIN`, default 30) one
//!   `needsyou` card per worker asks the owner to submit or clear the draft;
//! - when the hold ends the harness closes that card with the time
//!   (`steering_draft_hold_resolved`).
//!
//! Nothing is sent to the worker, so this holds for isolated workers too.

use serde_json::json;

use crate::db::{PendingEvent, SharedStore, WriteOutcome};

/// The creator stamped on the owner card.
pub const CREATOR: &str = "harness:steering";

/// A drain refusal that means "the composer holds text amux will not touch".
pub fn is_draft_hold(skip_reason: &str) -> bool {
    let r = skip_reason.trim_start();
    r.starts_with("composer contains a draft") || r.starts_with("composer draft appeared before paste")
}

/// Seconds a draft hold lasts before the owner is asked. 0 disables the card.
pub fn alert_after_s() -> f64 {
    std::env::var("AMUX_STEER_DRAFT_ALERT_MIN")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| *v >= 0.0)
        .unwrap_or(30.0)
        * 60.0
}

pub fn card_ref(worker: &str) -> String {
    format!("steer-draft:{worker}")
}

/// One lane currently held behind a draft.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub worker: String,
    pub queued: i64,
    pub oldest_age_s: f64,
}

/// What [`reconcile`] did, for the log line and the tests.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub opened: Vec<String>,
    pub refreshed: Vec<String>,
    pub resolved: Vec<String>,
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

/// Open, refresh or close the per-worker owner cards so they match `held`,
/// the complete list of lanes held behind a draft right now.
pub async fn reconcile(store: &SharedStore, held: &[Held], threshold_s: f64, now: i64) -> anyhow::Result<Report> {
    let due: Vec<Held> = if threshold_s > 0.0 {
        held.iter().filter(|h| h.oldest_age_s >= threshold_s).cloned().collect()
    } else {
        Vec::new()
    };
    let still_held: std::collections::BTreeSet<String> = held.iter().map(|h| card_ref(&h.worker)).collect();
    let owner = crate::api::turn_end::owner_name();
    let report = std::sync::Arc::new(std::sync::Mutex::new(Report::default()));
    let report_w = report.clone();
    store
        .write_async(move |conn| {
            use rusqlite::OptionalExtension;
            let mut events = Vec::new();
            let mut rep = Report::default();
            let open_for = |conn: &rusqlite::Connection, r: &str| -> rusqlite::Result<Option<String>> {
                conn.query_row(
                    "SELECT id FROM issues WHERE source_ref=?1 AND COALESCE(archived,0)=0 \
                     AND status NOT IN ('done','verified','discarded') ORDER BY rowid DESC LIMIT 1",
                    [r],
                    |row| row.get(0),
                )
                .optional()
            };
            for h in &due {
                let r = card_ref(&h.worker);
                let mins = (h.oldest_age_s / 60.0) as i64;
                let line = format!(
                    "{}: {} message(s) queued for {} have waited {mins} min behind text in its composer.",
                    fmt_ts(now),
                    h.queued,
                    h.worker
                );
                let (card, mutation) = match open_for(conn, &r)? {
                    Some(card) => {
                        conn.execute(
                            "UPDATE issues SET \"desc\" = COALESCE(\"desc\",'') || ?1, updated = ?2, rev = COALESCE(rev,0) + 1 WHERE id = ?3",
                            rusqlite::params![format!("\n{line}"), now, card],
                        )?;
                        rep.refreshed.push(h.worker.clone());
                        (card, amux_core::revision::MutationKind::Updated)
                    }
                    None => {
                        let title = format!("{} has queued messages waiting behind a composer draft", h.worker);
                        let desc = format!(
                            "{line}\n\namux does not clear or overwrite text in a worker's composer: it may be your own \
                             unsent prompt. Open {w}'s terminal and press Enter to send the draft, or clear it. The queued \
                             messages then deliver on the next steering pass and this card closes itself.\n\n\
                             Queue: GET /api/sessions/{w}/steer",
                            w = h.worker
                        );
                        let new = crate::db::board_store::NewIssue {
                            title,
                            desc,
                            status: "needsyou".into(),
                            session: None,
                            item_type: "decision".into(),
                            creator: CREATOR.into(),
                            owner_type: "human".into(),
                            due: None,
                            due_time: None,
                            reviewer: None,
                            shepherd: None,
                            gate: vec![],
                            depends_on: vec![],
                            tags: vec!["steering-draft-hold".into()],
                            ask_type: Some("decision".into()),
                            next_action: None,
                            acceptance_criteria: None,
                            ask_question: Some(format!(
                                "{} message(s) for {} are held behind a draft in its composer: send or clear the draft in its terminal?",
                                h.queued, h.worker
                            )),
                            ask_unblocks: Some(format!("The queued messages reach {}.", h.worker)),
                            ask_actor: Some(owner.clone()),
                            source: Some("steering_draft_hold".into()),
                            requested_by: None,
                            callback_session: None,
                            callback_prompt: None,
                        };
                        let row = crate::db::board_store::create_issue(conn, &new, now)?;
                        conn.execute("UPDATE issues SET source_ref=?1 WHERE id=?2", rusqlite::params![r, row.id])?;
                        rep.opened.push(h.worker.clone());
                        (row.id, amux_core::revision::MutationKind::Created)
                    }
                };
                events.push(PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: card.clone(),
                    mutation,
                    payload: Some(json!({"id": card, "source": "steering_draft_hold"})),
                });
            }
            // Close every open card whose worker is no longer held.
            let open: Vec<(String, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT id, source_ref FROM issues WHERE source_ref LIKE 'steer-draft:%' \
                     AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded')",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.flatten().collect()
            };
            for (card, r) in open {
                if still_held.contains(&r) {
                    continue;
                }
                conn.execute(
                    "UPDATE issues SET \"desc\" = COALESCE(\"desc\",'') || ?1, status='done', updated=?2, \
                     rev = COALESCE(rev,0) + 1 WHERE id=?3",
                    rusqlite::params![
                        format!("\n{}: resolved by the harness. The draft is gone and the queue is moving again.", fmt_ts(now)),
                        now,
                        card
                    ],
                )?;
                rep.resolved.push(r.trim_start_matches("steer-draft:").to_string());
                events.push(PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: card.clone(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: Some(json!({"id": card, "source": "steering_draft_hold"})),
                });
            }
            let applied = !events.is_empty();
            *report_w.lock().expect("report slot") = rep;
            Ok(WriteOutcome { applied, events })
        })
        .await?;
    let rep = report.lock().expect("report slot").clone();
    for w in &rep.opened {
        tracing::warn!(session = %w, measured = true, n_considered = held.len(), verdict = "steering_draft_hold_owner_asked",
            "queued messages held behind a composer draft past the alert threshold; the owner has a needsyou card");
    }
    for w in &rep.resolved {
        tracing::info!(session = %w, measured = true, n_considered = held.len(), verdict = "steering_draft_hold_resolved",
            "the composer draft hold ended; its owner card was closed");
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (SharedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store: SharedStore = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("dh.db")).unwrap());
        (store, dir)
    }

    #[test]
    fn only_a_draft_refusal_is_a_draft_hold() {
        assert!(is_draft_hold("composer contains a draft; durable message remains queued"));
        assert!(is_draft_hold("composer draft appeared before paste"));
        assert!(!is_draft_hold("session is in resume picker"));
        assert!(!is_draft_hold("unknown (no skip recorded since this process started)"));
    }

    async fn cards(store: &SharedStore) -> Vec<(String, String, String)> {
        store
            .read_async(|c| {
                let mut s = c.prepare("SELECT source_ref, status, \"desc\" FROM issues WHERE source_ref LIKE 'steer-draft:%' ORDER BY rowid")?;
                let rows = s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                Ok(rows.flatten().collect())
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_owner_is_asked_once_past_the_threshold_and_the_card_closes_when_the_hold_ends() {
        let (store, _dir) = store();
        let held = |age: f64| vec![Held { worker: "lane-d".into(), queued: 2, oldest_age_s: age }];
        let r = reconcile(&store, &held(600.0), 1800.0, 1_000).await.unwrap();
        assert_eq!(r, Report::default(), "under the threshold: no card");
        assert!(cards(&store).await.is_empty());
        let r = reconcile(&store, &held(1900.0), 1800.0, 2_000).await.unwrap();
        assert_eq!(r.opened, vec!["lane-d".to_string()]);
        let r = reconcile(&store, &held(2500.0), 1800.0, 3_000).await.unwrap();
        assert_eq!(r.refreshed, vec!["lane-d".to_string()], "one card per worker, refreshed");
        let c = cards(&store).await;
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1, "needsyou");
        assert!(c[0].2.contains("2 message(s) queued for lane-d have waited 41 min"), "{}", c[0].2);
        let r = reconcile(&store, &[], 1800.0, 4_000).await.unwrap();
        assert_eq!(r.resolved, vec!["lane-d".to_string()]);
        let c = cards(&store).await;
        assert_eq!(c[0].1, "done");
        assert!(c[0].2.contains("resolved by the harness"), "{}", c[0].2);
        assert_eq!(reconcile(&store, &[], 1800.0, 5_000).await.unwrap(), Report::default(), "nothing left to close");
    }

    #[tokio::test]
    async fn a_zero_threshold_never_files() {
        let (store, _dir) = store();
        let held = vec![Held { worker: "lane-z".into(), queued: 1, oldest_age_s: 99_999.0 }];
        assert_eq!(reconcile(&store, &held, 0.0, 1).await.unwrap(), Report::default());
    }
}
