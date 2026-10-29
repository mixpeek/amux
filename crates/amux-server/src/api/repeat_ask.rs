//! Flag an owner message that repeats an earlier ask to the same lane (F8(e),
//! AMUX-5241).
//!
//! INCIDENT. Ethan sent mixpeek-ops-server "can you organize the op tasks by
//! type like gtm is a category ..." as MSG-68912 on 2026-09-25 00:16Z and
//! again, byte for byte, as MSG-69179 on 2026-09-26 14:02Z, because the first
//! result was not findable. The lane received the second copy as a fresh
//! request, with nothing telling it the work had been asked for before, so the
//! likely outcome was the same work done twice and the same unfindable result.
//!
//! When an owner message is a near-duplicate of one sent to the same lane in
//! the last 14 days, the DELIVERED text gets a one-line note naming the earlier
//! message and asking the lane to say where that result lives and what is
//! still missing. The history row records the original text plus `repeat_of`
//! (migration 0088), so the Messages tab can show it.
//!
//! Similarity is Jaccard over normalized word sets. It is deliberately simple:
//! the incident was an exact repeat, and a paraphrase that scores below the
//! threshold costs nothing (the message is delivered exactly as before).
//!
//! Kill switch: `AMUX_REPEAT_ASK_NOTE=0` (process env, then worker > group >
//! global).
//!
//! SENT TO ANOTHER LANE TOO (AF-985). On 2026-10-28 Ethan sent "make the
//! ability in the amux Mac OS app to apply a zoom in/zoom out to all windows"
//! to amux-frustrations as MSG-99572 and, 88 seconds later, to amux-admin as
//! MSG-99574. Both lanes held it and one built it (6109d02e1); neither was
//! told about the other. An owner message that matches one sent to a
//! DIFFERENT lane in the last 6 hours is named in the send result
//! (`same_request_elsewhere`) and, for a lane that receives it directly, in a
//! note on the delivered text, so one lane takes it and the other coordinates.
//! Same similarity, no minimum age: the incident was a minute apart.
use std::collections::BTreeSet;

pub(crate) const GATE_KEY: &str = "AMUX_REPEAT_ASK_NOTE";
/// How far back an earlier ask counts.
pub(crate) const WINDOW_MS: i64 = 14 * 24 * 3_600_000;
/// Younger than this is a retry or a double send, which the duplicate-delivery
/// detector already announces; it is not a request the lane failed to answer.
pub(crate) const MIN_AGE_MS: i64 = 30 * 60_000;
/// Jaccard at or above this is a repeat.
pub(crate) const THRESHOLD: f64 = 0.7;
/// "continue", "done" and "yes" are repeated constantly and are never repeat
/// asks. Six distinct content words is the smallest request worth flagging.
pub(crate) const MIN_TOKENS: usize = 6;
/// How many recent owner rows to compare against. A lane receives far fewer
/// owner messages than this in 14 days; the cap bounds the read.
pub(crate) const MAX_CANDIDATES: i64 = 400;

const STOPWORDS: [&str; 24] = [
    "the", "and", "for", "you", "that", "this", "with", "can", "are", "its", "it's", "all", "but",
    "not", "was", "have", "has", "our", "any", "what", "from", "into", "then", "now",
];

/// The dashboard prefixes a local clock stamp (`[08:16 PM] ...`). It differs
/// on every send of the same text and must not count against a repeat.
fn strip_clock_stamp(text: &str) -> &str {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let stamp = &rest[..end];
            let looks_like_clock = stamp.len() <= 12
                && stamp.contains(':')
                && stamp
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b':' | b' ' | b'A' | b'P' | b'M'));
            if looks_like_clock {
                return &rest[end + 1..];
            }
        }
    }
    t
}

pub(crate) fn tokens(text: &str) -> BTreeSet<String> {
    strip_clock_stamp(text)
        .to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(w))
        .map(str::to_owned)
        .collect()
}

pub(crate) fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// How far back a send to another lane counts. Two lanes building the same
/// thing is a same-day problem; older work is findable on the board.
pub(crate) const ELSEWHERE_WINDOW_MS: i64 = 6 * 3_600_000;

/// An earlier owner message to the same lane: `(id, ts_ms, text)`.
pub(crate) type Candidate = (i64, i64, String);

/// The earlier ask this message repeats, if any: `(id, ts_ms, score)`. The most
/// recent qualifying message wins, because that is the result the owner was
/// most recently waiting on.
pub(crate) fn find_repeat(
    text: &str,
    candidates: &[Candidate],
    now_ms: i64,
) -> Option<(i64, i64, f64)> {
    let mine = tokens(text);
    if mine.len() < MIN_TOKENS {
        return None;
    }
    candidates
        .iter()
        .filter(|(_, ts, _)| {
            let age = now_ms - ts;
            (MIN_AGE_MS..=WINDOW_MS).contains(&age)
        })
        .filter_map(|(id, ts, t)| {
            let score = similarity(&mine, &tokens(t));
            (score >= THRESHOLD).then_some((*id, *ts, score))
        })
        .max_by_key(|(_, ts, _)| *ts)
}

/// An owner message to another lane: `(id, session, ts_ms, text)`.
pub(crate) type ElsewhereCandidate = (i64, String, i64, String);

/// The same request sent to another lane, if any: `(id, session, ts_ms)`. The
/// most recent match wins.
pub(crate) fn find_elsewhere(
    text: &str,
    candidates: &[ElsewhereCandidate],
    now_ms: i64,
) -> Option<(i64, String, i64)> {
    let mine = tokens(text);
    if mine.len() < MIN_TOKENS {
        return None;
    }
    candidates
        .iter()
        .filter(|(_, _, ts, _)| (0..=ELSEWHERE_WINDOW_MS).contains(&(now_ms - ts)))
        .filter(|(_, _, _, t)| similarity(&mine, &tokens(t)) >= THRESHOLD)
        .max_by_key(|(_, _, ts, _)| *ts)
        .map(|(id, lane, ts, _)| (*id, lane.clone(), *ts))
}

/// The line prepended to the delivered text when the request also went to
/// another lane.
pub(crate) fn note_elsewhere(id: i64, lane: &str, ts_ms: i64, now_ms: i64) -> String {
    let mins = ((now_ms - ts_ms).max(0) + 59_999) / 60_000;
    format!(
        "[amux: the owner sent this same request to {lane} as MSG-{id} {mins} min ago. \
         Check that lane's board before building it, so only one lane does the work.]\n\n"
    )
}

/// The line prepended to the delivered text.
pub(crate) fn note(id: i64, ts_ms: i64) -> String {
    let date = chrono::DateTime::from_timestamp_millis(ts_ms)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "an earlier date".into());
    format!(
        "[amux: this repeats MSG-{id} from {date}. Before redoing the work, say where the \
         result of MSG-{id} lives and what is still missing.]\n\n"
    )
}

/// Is this owner message a repeat ask? Gate, lookup and log in one place, so
/// the send path only has to prepend the note and mark the row.
pub(crate) async fn lookup(
    state: &super::AppState,
    session: &str,
    text: &str,
) -> Option<(i64, i64)> {
    if !super::session_verbs::scoped_gate_on(session, GATE_KEY) {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let rows = state
        .store
        .read()
        .ok()
        .and_then(|c| candidates(&c, session, now_ms).ok());
    let Some(rows) = rows else {
        tracing::warn!(
            session,
            measured = false,
            verdict = "repeat_ask_unmeasured",
            "could not read earlier owner messages; delivering without a repeat check"
        );
        return None;
    };
    let found = find_repeat(text, &rows, now_ms);
    if let Some((id, ts, score)) = found {
        tracing::info!(
            session,
            repeat_of = id,
            age_h = (now_ms - ts) / 3_600_000,
            score,
            measured = true,
            n_considered = rows.len(),
            verdict = "repeat_ask_noted",
            "owner message repeats an earlier ask to this lane; delivered with a note naming it"
        );
    }
    found.map(|(id, ts, _)| (id, ts))
}

/// Did the owner send this message to another lane recently? Gate, lookup and
/// log in one place. Returns `(id, session, ts_ms)`.
pub(crate) async fn lookup_elsewhere(
    state: &super::AppState,
    session: &str,
    text: &str,
) -> Option<(i64, String, i64)> {
    if !super::session_verbs::scoped_gate_on(session, GATE_KEY) {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let rows = state
        .store
        .read()
        .ok()
        .and_then(|c| candidates_elsewhere(&c, session, now_ms).ok());
    let Some(rows) = rows else {
        tracing::warn!(
            session,
            measured = false,
            verdict = "owner_request_elsewhere_unmeasured",
            "could not read owner messages to other lanes; delivering without the check"
        );
        return None;
    };
    let found = find_elsewhere(text, &rows, now_ms);
    if let Some((id, lane, ts)) = &found {
        tracing::info!(
            session,
            other_lane = %lane,
            other_msg = id,
            age_s = (now_ms - ts) / 1000,
            measured = true,
            n_considered = rows.len(),
            verdict = "owner_request_sent_elsewhere",
            "owner message matches one sent to another lane; the send result and the delivered text name it"
        );
    }
    found
}

/// Owner messages (no worker origin) to OTHER lanes inside the window, newest
/// first.
pub(crate) fn candidates_elsewhere(
    conn: &rusqlite::Connection,
    session: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<ElsewhereCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT id, session, ts, text FROM cmd_history WHERE session<>?1 AND ts>?2 \
         AND type IN ('user','direct','steering') AND COALESCE(origin,'')='' \
         ORDER BY ts DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![session, now_ms - ELSEWHERE_WINDOW_MS, MAX_CANDIDATES],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    rows.collect()
}

/// Owner messages to `session` inside the window, newest first.
pub(crate) fn candidates(
    conn: &rusqlite::Connection,
    session: &str,
    now_ms: i64,
) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, text FROM cmd_history WHERE session=?1 AND ts>?2 \
         AND type IN ('user','direct','steering') ORDER BY ts DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![session, now_ms - WINDOW_MS, MAX_CANDIDATES],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    const ASK: &str = "can you organize the op tasks by type like gtm is a category and make it \
                       easy to disable enable each there shoukd be a standard class so we can add \
                       more extend etc";

    #[test]
    fn the_incident_repeat_is_found_through_the_clock_stamps() {
        // MSG-68912 and MSG-69179 as stored: same text, different clock stamps,
        // about 38 hours apart.
        let first = (68912, 1_000 * HOUR, format!("[08:16 PM] {ASK}"));
        let now = 1_000 * HOUR + 38 * HOUR;
        let got = find_repeat(&format!("[10:02 AM] {ASK}"), &[first], now).unwrap();
        assert_eq!((got.0, got.1), (68912, 1_000 * HOUR));
        assert!((got.2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_light_rewording_still_counts_and_a_different_ask_does_not() {
        let prior = vec![
            (1, 0, ASK.to_string()),
            (
                2,
                HOUR,
                "whats the audit of all ops server responsibilities and the list of scheduled runs"
                    .to_string(),
            ),
        ];
        let now = 10 * HOUR;
        let reworded = "can you organize the op tasks by type, like gtm is a category, and make it \
                        easy to enable and disable each one; there should be a standard class so we \
                        can add more and extend";
        assert_eq!(find_repeat(reworded, &prior, now).map(|r| r.0), Some(1));
        assert_eq!(
            find_repeat(
                "the founder email should explain the starting namespace in one sentence",
                &prior,
                now
            ),
            None
        );
    }

    #[test]
    fn short_messages_retries_and_old_asks_are_not_repeats() {
        let now = 20 * 24 * HOUR;
        let prior = vec![(1, now - 2 * HOUR, "continue".to_string())];
        assert_eq!(find_repeat("continue", &prior, now), None, "too short");
        let retry = vec![(2, now - 5 * 60_000, ASK.to_string())];
        assert_eq!(find_repeat(ASK, &retry, now), None, "a retry minutes later");
        let stale = vec![(3, now - 15 * 24 * HOUR, ASK.to_string())];
        assert_eq!(find_repeat(ASK, &stale, now), None, "outside 14 days");
    }

    #[test]
    fn the_most_recent_repeat_wins_and_the_note_names_it() {
        let now = 100 * HOUR;
        let prior = vec![
            (10, 10 * HOUR, ASK.to_string()),
            (20, 50 * HOUR, ASK.to_string()),
        ];
        let (id, ts, _) = find_repeat(ASK, &prior, now).unwrap();
        assert_eq!(id, 20);
        let n = note(id, 1_790_000_000_000);
        assert!(
            n.starts_with("[amux: this repeats MSG-20 from 2026-"),
            "{n}"
        );
        assert!(n.contains("what is still missing"));
        assert!(n.ends_with("\n\n"));
        let _ = ts;
    }

    #[tokio::test]
    async fn a_repeat_is_found_in_the_live_ledger_and_marked_on_the_new_row() {
        let tmp = tempfile::tempdir().unwrap();
        let state = crate::api::AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&tmp.path().join("t.db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let lane = "repeat-ask-fixture";
        let then = chrono::Utc::now().timestamp_millis() - 38 * HOUR;
        state
            .store
            .write(move |c| {
                c.execute(
                    "INSERT INTO cmd_history(id,text,type,session,ts,origin,delivery) VALUES(68912,?1,'user',?2,?3,'','direct')",
                    rusqlite::params![format!("[08:16 PM] {ASK}"), "repeat-ask-fixture", then],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let found = lookup(&state, lane, &format!("[10:02 AM] {ASK}")).await;
        assert_eq!(found, Some((68912, then)));
        let row = crate::api::session_verbs::cmd_hist_record_with_id(
            &state,
            lane,
            &format!("[10:02 AM] {ASK}"),
            "user",
            "",
            true,
            crate::api::session_verbs::DeliveryMeta {
                repeat_of: found.map(|f| f.0),
                ..crate::api::session_verbs::DeliveryMeta::direct()
            },
        )
        .await;
        let conn = state.store.read().unwrap();
        let (text, repeat_of): (String, Option<i64>) = conn
            .query_row(
                "SELECT text, repeat_of FROM cmd_history WHERE id=?1",
                [row],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(repeat_of, Some(68912));
        assert!(
            !text.contains("[amux: this repeats"),
            "the row keeps the owner's words"
        );
    }

    #[test]
    fn the_zoom_request_sent_to_two_lanes_is_named_on_the_second() {
        // MSG-99572 to amux-frustrations, then MSG-99574 to amux-admin 88 s later.
        let zoom = "make the ability in the amux Mac OS app to apply a zoom in/zoom out to all windows";
        let now = 1_793_198_252_000;
        let prior = vec![
            (99572, "amux-frustrations".to_string(), now - 88_000, zoom.to_string()),
            (99560, "amux".to_string(), now - 60_000, "fix the speaker notes in the pptx viewer".to_string()),
        ];
        let got = find_elsewhere(&format!("[02:37 PM] {zoom}"), &prior, now);
        assert_eq!(got, Some((99572, "amux-frustrations".to_string(), now - 88_000)));
        let n = note_elsewhere(99572, "amux-frustrations", now - 88_000, now);
        assert!(n.starts_with("[amux: the owner sent this same request to amux-frustrations as MSG-99572 2 min ago."), "{n}");
        assert!(n.ends_with("\n\n"));
        // Outside six hours, a short ack, or a different ask: nothing.
        assert_eq!(find_elsewhere(zoom, &[(1, "a".into(), now - 7 * HOUR, zoom.into())], now), None);
        assert_eq!(find_elsewhere("continue", &[(1, "a".into(), now, "continue".into())], now), None);
        assert_eq!(find_elsewhere("fix the editor edit button that goes away when zoomed in", &prior, now), None);
    }

    #[test]
    fn candidates_elsewhere_reads_only_owner_rows_to_other_lanes() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE cmd_history(id INTEGER PRIMARY KEY, text TEXT, type TEXT, session TEXT, ts INTEGER, origin TEXT);
             INSERT INTO cmd_history VALUES (1,'a','user','ops',100,''),(2,'b','user','web',100,''),
               (3,'c','user','web',100,'amux-helper'),(4,'d','pickup','web',100,''),(5,'e','direct','api',90,NULL);",
        )
        .unwrap();
        let got = candidates_elsewhere(&conn, "ops", 200).unwrap();
        assert_eq!(got.iter().map(|c| c.0).collect::<Vec<_>>(), vec![2, 5]);
    }

    #[test]
    fn candidates_reads_only_owner_rows_for_the_lane() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE cmd_history(id INTEGER PRIMARY KEY, text TEXT, type TEXT, session TEXT, ts INTEGER);
             INSERT INTO cmd_history VALUES (1,'a','user','ops',100),(2,'b','session','ops',100),
               (3,'c','pickup','ops',100),(4,'d','user','other',100),(5,'e','direct','ops',50);",
        )
        .unwrap();
        let got = candidates(&conn, "ops", 200).unwrap();
        assert_eq!(got.iter().map(|c| c.0).collect::<Vec<_>>(), vec![1, 5]);
    }
}
