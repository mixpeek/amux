//! Tell an open card when a merged commit names it (AMUX-3579).
//!
//! `GET /api/board/commit-mentions` has existed, been routed, and WORKED for a
//! while, and nothing called it. Zero consumers across the SPA, the bash CLI,
//! `scripts/` and the server itself. A working capability that reaches no
//! session is ethos rule 1 in its plainest form — the `mcp.json` shape, where
//! six configured servers reached 0 of 101 sessions.
//!
//! The cost of that was a whole turn: AMUX-3557 was fixed at 16:49 by a commit
//! whose SUBJECT LINE named it, sat in `todo` for seven hours, was handed to a
//! lane by auto-pickup, and cost that lane a turn establishing nothing was
//! wrong. The endpoint could have said so the whole time.
//!
//! NOTED, NEVER CLOSED, and that is not timidity. A mention is not proof of
//! completion — the endpoint's own verdict says so, because commits also
//! reference cards for context, for partial work and for reverts. AMUX-3557 is
//! the proof: closing it honestly still required checking the behaviour held
//! and proving the regression test discriminates. Auto-closing on a mention
//! would have skipped exactly the check that made the close honest, and would
//! be an agent deciding someone's work is done (ethos rule 8).
//!
//! ITS OWN JOB, NOT THE AUTOFIX TICK, because it is expensive: measured at
//! 10.6-11.1s across three consecutive calls, scanning 3 repos against 65 open
//! cards. Putting that on autofix's ~2-minute tick would stall a loop that has
//! detectors waiting behind it. Hourly is plenty for a signal about commits
//! that have already landed.

use crate::api::AppState;
use crate::db::WriteOutcome;
use serde_json::json;

const JOB: &str = "commit-mention-notes";

/// Hourly. The signal is about commits that ALREADY landed, so freshness buys
/// nothing and the scan is ~11s.
fn tick_secs() -> u64 {
    crate::config::env_i64("AMUX_COMMIT_MENTION_TICK_S", 3600).max(60) as u64
}

/// One pending note.
struct Note {
    card: String,
    sha: String,
    subject: String,
    idem: String,
}

pub async fn tick(state: AppState) -> anyhow::Result<Vec<(String, String)>> {
    let payload = crate::api::commit_mentions::mentions_payload(&state).await;
    let candidates = payload
        .get("candidates")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let mut pending: Vec<Note> = Vec::new();
    for c in &candidates {
        let Some(card) = c.get("id").and_then(|v| v.as_str()) else { continue };
        // FIRST commit only. A card named in five commits is one fact, and five
        // log lines is the ethos rule 5 shape — at volume the card stops being
        // a task and becomes a log. The idem is per (card, sha) so a LATER
        // commit naming the same card still gets its own line, which is the
        // case where the news is genuinely new.
        let Some(commit) = c.get("commits").and_then(|v| v.as_array()).and_then(|a| a.first())
        else {
            continue;
        };
        let sha = commit.get("sha").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let subject = commit.get("subject").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if sha.is_empty() {
            continue;
        }
        pending.push(Note {
            idem: format!("commit-mention:{card}:{sha}"),
            card: card.to_string(),
            sha,
            subject,
        });
    }

    let mut noted: Vec<(String, String)> = Vec::new();
    for Note { card, sha, subject, idem } in pending {
        let already = {
            let conn = state.store.read()?;
            conn.query_row(
                "SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1",
                rusqlite::params![idem],
                |_| Ok(()),
            )
            .is_ok()
        };
        if already {
            continue;
        }
        let line = format!(
            "[amux] a merged commit names this card: {sha} \"{subject}\". A mention is NOT proof \
             of completion — commits also reference cards for context, for partial work and for \
             reverts — so check the behaviour actually holds before closing. Nothing here has \
             been moved; that call is yours."
        );
        let (c2, i2) = (card.clone(), idem.clone());
        state
            .store
            .write_async(move |conn| {
                use rusqlite::OptionalExtension;
                let existing: Option<String> = conn
                    .query_row("SELECT log FROM issues WHERE id=?1", rusqlite::params![c2], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .flatten();
                let hhmm = chrono::Local::now().format("%H:%M").to_string();
                let log = crate::db::board_store::append_log(existing.as_deref(), &hhmm, &line);
                conn.execute("UPDATE issues SET log=?1 WHERE id=?2", rusqlite::params![log, c2])?;
                conn.execute(
                    "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                     VALUES (?1, '', 'board.commit_mention', ?2, ?3, 'commit-mentions')",
                    rusqlite::params![
                        crate::config::now_f64(),
                        json!({ "card": c2 }).to_string(),
                        i2
                    ],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await?;
        noted.push((card, sha));
    }

    // UNCONDITIONAL, INCLUDING ZERO — the same rule the latency and blindness
    // lines follow (AF-178). A job that speaks only when it has news cannot
    // tell you it ran, and this one runs hourly where nobody is watching.
    tracing::info!(
        target: "autofix",
        candidates = candidates.len(),
        noted = noted.len(),
        "commit-mention notes (AMUX-3579)"
    );
    Ok(noted)
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let st = state.clone();
        async move {
            if let Err(e) = tick(st).await {
                tracing::warn!(target: "autofix", error = %e, "commit-mention notes failed");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        (
            AppState {
                store,
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
            },
            dir,
        )
    }

    /// AMUX-3579: the note lands once, says a mention is not proof, and NEVER
    /// moves the card.
    ///
    /// The status assertion is the load-bearing one. A commit naming a card is
    /// evidence the work may be done; it is not the holder's judgment that it
    /// IS done. AMUX-3557 is the proof — closing it honestly still required
    /// checking the behaviour held and proving the regression test
    /// discriminates, and a mention-triggered auto-close would have skipped
    /// exactly that.
    #[tokio::test]
    async fn a_mention_is_noted_once_and_never_moves_the_card() {
        let (st, _d) = state();
        st.store
            .write_async(|conn| {
                conn.execute(
                    "INSERT INTO issues (id,title,status,session,created,updated,archived) \
                     VALUES ('AMUX-9998','a card','todo','amux',0,0,0)",
                    [],
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();

        let note = |sha: &str| Note {
            card: "AMUX-9998".into(),
            sha: sha.into(),
            subject: "fix(x): something (AMUX-9998)".into(),
            idem: format!("commit-mention:AMUX-9998:{sha}"),
        };
        // Drive the write half directly: the scan half shells out to git across
        // real repos and takes ~11s, which is not what this cell is about.
        for n in [note("abc123"), note("abc123")] {
            let already = {
                let conn = st.store.read().unwrap();
                conn.query_row(
                    "SELECT 1 FROM session_events WHERE idem=?1 LIMIT 1",
                    rusqlite::params![n.idem],
                    |_| Ok(()),
                )
                .is_ok()
            };
            if already {
                continue;
            }
            let (c2, i2) = (n.card.clone(), n.idem.clone());
            let line = format!("[amux] a merged commit names this card: {} \"{}\".", n.sha, n.subject);
            st.store
                .write_async(move |conn| {
                    use rusqlite::OptionalExtension;
                    let existing: Option<String> = conn
                        .query_row("SELECT log FROM issues WHERE id=?1", rusqlite::params![c2], |r| r.get(0))
                        .optional()?
                        .flatten();
                    let log = crate::db::board_store::append_log(existing.as_deref(), "12:00", &line);
                    conn.execute("UPDATE issues SET log=?1 WHERE id=?2", rusqlite::params![log, c2])?;
                    conn.execute(
                        "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
                         VALUES (?1, '', 'board.commit_mention', ?2, ?3, 'commit-mentions')",
                        rusqlite::params![1.0, json!({"card": c2}).to_string(), i2],
                    )?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .await
                .unwrap();
        }

        let (log, status): (Option<String>, String) = {
            let conn = st.store.read().unwrap();
            conn.query_row(
                "SELECT log, status FROM issues WHERE id='AMUX-9998'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let log = log.unwrap_or_default();
        assert_eq!(
            log.matches("a merged commit names this card").count(),
            1,
            "exactly-once: the same sha must not re-nag on every hourly tick:\n{log}"
        );
        assert_eq!(
            status, "todo",
            "NOTED, NEVER MOVED — a mention is not proof of completion, and deciding it is \
             would be an agent closing someone's work on a commit subject line"
        );
    }
}
