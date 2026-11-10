//! Works the open directives in [`crate::api::directive`]: retries a delivery that
//! did not land, repeats an unanswered directive once, then tells the owner.
//! Everything it reads is in the table, so a restart picks up where it stopped.

use crate::api::directive::{deliver_and_record, due, open_rows, written, Due};
use crate::api::AppState;
use crate::config::now_f64;
use serde_json::Value;

const JOB: &str = super::registry::ids::DIRECTIVE;

async fn stamp(state: &AppState, id: &str, column: &'static str) {
    let i = id.to_string();
    let stored = state.store.write_async(move |c| {
        c.execute(&format!("UPDATE directives SET {column} = ?2 WHERE id = ?1"), rusqlite::params![i, now_f64()])?;
        Ok(written())
    }).await;
    if let Err(error) = stored {
        tracing::warn!(directive = id, %error, verdict = "directive_write_failed", measured = true, n_considered = 1,
            "a directive's state could not be recorded");
    }
}

async fn tell(state: &AppState, row: &Value, what: &str) {
    let (id, session, sender) = (
        row["id"].as_str().unwrap_or(""), row["session"].as_str().unwrap_or(""), row["sender"].as_str().unwrap_or(""));
    tracing::warn!(directive = id, session, sender, verdict = "directive_unanswered", what, measured = true,
        n_considered = 1, "a directive needs the owner");
    crate::push::send_all(state, &format!("{session}: {what}"), &format!("{id} from {sender}"), session,
        &format!("directive:{id}"), &format!("/#peek={session}")).await;
    stamp(state, id, "told_at").await;
}

async fn tick(state: &AppState) {
    let rows = match state.store.read_async(|c| Ok(open_rows(c)?)).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, verdict = "directive_watch_unmeasured", measured = false, n_considered = 0,
                "the open directives could not be read");
            return;
        }
    };
    let ack_s = crate::config::env_i64("AMUX_DIRECTIVE_ACK_S", 600).max(1) as f64;
    let now = now_f64();
    for row in rows {
        let s = |k: &str| row[k].as_str().unwrap_or("").to_string();
        let (id, session, sender, text) = (s("id"), s("session"), s("sender"), s("text"));
        let tries = row["tries"].as_i64().unwrap_or(0);
        match due(&row, now, ack_s) {
            Due::Nothing => {}
            Due::Deliver => {
                if deliver_and_record(state, &id, &session, &sender, &text, tries).await.is_err() {
                    tracing::debug!(directive = %id, tries = tries + 1, "directive retry did not land; next pass tries again");
                }
            }
            // A failed repeat still counts, so the owner is told after one more
            // AMUX_DIRECTIVE_ACK_S instead of the repeat retrying forever.
            Due::Resend => {
                if deliver_and_record(state, &id, &session, &sender, &text, tries).await.is_err() {
                    tracing::debug!(directive = %id, "directive repeat did not land; the owner is told next");
                }
                stamp(state, &id, "resent_at").await;
            }
            Due::Tell => tell(state, &row, "a directive has gone unanswered").await,
            Due::GiveUp => tell(state, &row, "a directive could not be delivered").await,
        }
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    let secs = crate::config::env_i64("AMUX_DIRECTIVE_TICK_S", 60).max(1) as u64;
    super::spawn_periodic(JOB, secs, move || {
        let st = state.clone();
        async move { tick(&st).await }
    })
}
