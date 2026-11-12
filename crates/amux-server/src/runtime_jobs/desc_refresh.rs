//! Keep worker descriptions current with how each worker is actually used
//! (Ethan, 2026-11-11: "worker descriptions should keep up with how a worker is
//! actually used"). Peers route work by `CC_DESC` through the fleet roster, and
//! it only changed on a hand edit, so a lane whose job had moved kept
//! advertising the old one and work went to the wrong worker.
//!
//! Each tick picks a few non-isolated, non-archived, non-paused workers whose
//! description is not locked and that have new owner messages or closed cards
//! since they were last checked (and at least a day ago), oldest check first.
//! The helper model reads the description beside that work and answers KEEP or
//! a replacement in the same short style. A replacement keeps the old text in
//! `CC_DESC_PREV` (Revert in the worker's Configuration tab puts it back), is
//! marked `CC_DESC_BY=auto`, refreshes the peer rosters and is logged as
//! `desc_auto_updated` plus a `session.desc_updated` event.
//!
//! What it never touches: a description with `CC_DESC_LOCK=1`. The owner's own
//! edit sets that lock (session_verbs `config_patch`), and the Configuration tab
//! can set or clear it.

use serde_json::{json, Value};

use super::registry::ids;
use crate::api::session_verbs as sv;

const JOB: &str = ids::DESC_REFRESH;
const DAY: i64 = 86_400;
/// How far back the work that describes a worker is read.
const WINDOW: i64 = 14 * DAY;
const MAX_LEN: usize = 200;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// One worker the pass may look at.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Candidate {
    pub name: String,
    pub desc: String,
}

/// Whether amux may consider this worker's description at all.
pub(crate) fn eligible(env: &sv::EnvFile) -> bool {
    let on = |k: &str| matches!(env.get(k).unwrap_or("").trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes");
    !on("CC_ARCHIVED") && !on("CC_ISOLATED") && !on("CC_PAUSED") && !on("CC_DESC_LOCK")
}

/// The work a worker did in the window: the owner's messages to it and the
/// cards it closed, newest last, each one line. Returns (lines, newest ts).
fn recent_work(conn: &rusqlite::Connection, name: &str, since: i64) -> rusqlite::Result<(Vec<String>, i64)> {
    let mut lines = Vec::new();
    let mut newest = 0i64;
    let mut stmt = conn.prepare(
        "SELECT text, ts FROM cmd_history WHERE lower(session) = lower(?1) AND ts >= ?2
           AND ((type = 'user' AND COALESCE(origin, '') = '') OR (type = 'chat' AND origin = 'chat-companion'))
         ORDER BY ts DESC LIMIT 15",
    )?;
    let mut msgs: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params![name, since], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    msgs.reverse();
    for (text, ts) in msgs {
        newest = newest.max(ts);
        let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect();
        if !one.is_empty() {
            lines.push(format!("owner asked: {one}"));
        }
    }
    let mut stmt = conn.prepare(
        "SELECT title, COALESCE(updated, 0) FROM issues WHERE lower(session) = lower(?1) AND deleted IS NULL
           AND status IN ('done', 'verified') AND COALESCE(updated, 0) >= ?2
         ORDER BY updated DESC LIMIT 15",
    )?;
    let mut cards: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params![name, since], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    cards.reverse();
    for (title, ts) in cards {
        newest = newest.max(ts);
        let one: String = title.chars().take(160).collect();
        lines.push(format!("card closed: {one}"));
    }
    Ok((lines, newest))
}

pub(crate) fn prompt(name: &str, desc: &str, work: &[String]) -> String {
    let current = if desc.trim().is_empty() { "(none)" } else { desc.trim() };
    format!(
        "You keep the one-line description of the amux worker \"{name}\" current. Other workers and the owner \
         decide what to send this worker by reading it, so it must say what the worker is used for now.\n\n\
         Current description: {current}\n\n\
         Its work over the last two weeks, oldest first:\n{}\n\n\
         If the current description still fits this work, answer exactly KEEP. Otherwise answer with the new \
         description inside <desc></desc> and nothing else: one line in the same style and about the same \
         length as the current one (at most 160 characters), naming the kinds of work, with no dates, ids, \
         quotes or em-dashes.",
        work.iter().map(|l| format!("- {l}")).collect::<Vec<_>>().join("\n")
    )
}

/// The model's answer as a new description, or None to keep the current one.
/// Only text inside `<desc></desc>` counts: on 2026-11-11 the first live run
/// took a preamble line ("Here's the updated description:") as the
/// description of amux-security, because the answer was read as its first line.
pub(crate) fn parse_answer(out: &str, current: &str) -> Option<String> {
    let start = out.find("<desc>")? + "<desc>".len();
    let end = start + out[start..].find("</desc>")?;
    let text = out[start..end].split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_matches(|c| c == '"' || c == '\'' || c == '`').trim();
    if text.is_empty() || text.eq_ignore_ascii_case("KEEP") || text.chars().count() > MAX_LEN {
        return None;
    }
    let text = text.replace(" \u{2014} ", ", ").replace('\u{2014}', ",");
    (text != current.trim()).then_some(text)
}

/// Whether this worker is due: work newer than its last check, and the last
/// check at least `interval` ago.
pub(crate) fn due(now: i64, checked: i64, newest_work: i64, interval: i64) -> bool {
    newest_work > checked && now - checked >= interval
}

/// Write the new description unless the env changed under us (an owner edit
/// or a lock during the model call). Returns whether it was written.
pub(crate) fn apply(path: &std::path::Path, expected: &str, new: &str, now: i64) -> std::io::Result<bool> {
    let mut env = sv::EnvFile::load(path);
    if !eligible(&env) || env.get_or("CC_DESC", "") != expected {
        return Ok(false);
    }
    env.set("CC_DESC_PREV", expected);
    env.set("CC_DESC", new);
    env.set("CC_DESC_BY", "auto");
    env.set("CC_DESC_AT", &now.to_string());
    env.write(path)?;
    Ok(true)
}

pub fn spawn(app: crate::api::AppState) -> super::PeriodicTask {
    let secs = env_u64("AMUX_DESC_REFRESH_SECS", 3600).max(1);
    super::spawn_periodic(JOB, secs, move || {
        let app = app.clone();
        async move { tick(app).await }
    })
}

async fn tick(app: crate::api::AppState) {
    if env_u64("AMUX_DESC_REFRESH_SECS", 3600) == 0 {
        return;
    }
    let now = crate::config::now_f64() as i64;
    let interval = env_u64("AMUX_DESC_REFRESH_INTERVAL_S", DAY as u64) as i64;
    let per_tick = env_u64("AMUX_DESC_REFRESH_PER_TICK", 3) as usize;
    let home = crate::api::groups::amux_home();
    let mut candidates: Vec<(i64, Candidate)> = Vec::new();
    for e in std::fs::read_dir(home.join("sessions")).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue };
        let env = sv::EnvFile::load(&p);
        if !eligible(&env) {
            continue;
        }
        let checked = sv::load_meta(&name).get("desc_checked_ts").and_then(Value::as_i64).unwrap_or(0);
        candidates.push((checked, Candidate { name, desc: env.get_or("CC_DESC", "").to_string() }));
    }
    let considered = candidates.len();
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let Some(client) = crate::api::board_intake::model_client() else {
        tracing::warn!(measured = false, n_considered = considered, verdict = "desc_refresh_no_model",
            "worker descriptions cannot refresh: no helper model is configured");
        return;
    };
    let (mut checked_n, mut updated, mut kept, mut failed, mut raced) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for (checked, c) in candidates {
        if checked_n >= per_tick {
            break;
        }
        let since = now - WINDOW;
        let name = c.name.clone();
        let work = app.store.read().map_err(|e| e.to_string())
            .and_then(|conn| recent_work(&conn, &name, since).map_err(|e| e.to_string()));
        let (lines, newest) = match work {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(session = %c.name, error = %e, verdict = "desc_refresh_read_failed", "could not read a worker's recent work");
                continue;
            }
        };
        // Three items is the least that says what a worker is for.
        if lines.len() < 3 || !due(now, checked, newest, interval) {
            continue;
        }
        checked_n += 1;
        sv::update_meta(&c.name, &[("desc_checked_ts", json!(now))]);
        let p = prompt(&c.name, &c.desc, &lines);
        let p = sv::redact_prompt_secrets(&p);
        let model = crate::api::mdai::resolve_model(None);
        let cl = client.clone();
        let out = tokio::task::spawn_blocking(move || cl.complete(&model, &p)).await;
        let out = match out.map_err(|e| e.to_string()).and_then(|r| r) {
            Ok(o) => o,
            Err(e) => {
                failed += 1;
                tracing::warn!(session = %c.name, error = %e, verdict = "desc_refresh_model_failed", "the helper model did not answer");
                continue;
            }
        };
        let Some(new) = parse_answer(&out, &c.desc) else {
            kept += 1;
            continue;
        };
        match apply(&sv::env_path(&c.name), &c.desc, &new, now) {
            Ok(true) => {
                updated += 1;
                tracing::info!(session = %c.name, from = %c.desc, to = %new, items = lines.len(), measured = true,
                    n_considered = 1, verdict = "desc_auto_updated",
                    "worker description updated to match its recent work (Revert in its Configuration tab)");
                sv::emit_event(&app, &c.name, "session.desc_updated",
                    Some(json!({"from": c.desc, "to": new, "by": "auto", "items": lines.len()})), None, "desc-refresh").await;
            }
            Ok(false) => raced += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(session = %c.name, error = %e, verdict = "desc_refresh_write_failed", "could not write the description");
            }
        }
    }
    if updated > 0 {
        sv::refresh_fleet_rosters();
        crate::api::sessions_legacy::invalidate_sessions_cache();
    }
    if checked_n > 0 {
        tracing::info!(measured = true, n_considered = considered, checked = checked_n, updated, kept, failed,
            changed_meanwhile = raced, verdict = "desc_refresh_tick", "worker descriptions checked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_isolated_paused_and_archived_workers_are_left_alone() {
        let env = |t: &str| {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("w.env");
            std::fs::write(&p, t).unwrap();
            sv::EnvFile::load(&p)
        };
        assert!(eligible(&env("CC_DESC=\"x\"\n")));
        for k in ["CC_DESC_LOCK", "CC_ISOLATED", "CC_PAUSED", "CC_ARCHIVED"] {
            assert!(!eligible(&env(&format!("CC_DESC=\"x\"\n{k}=\"1\"\n"))), "{k}");
        }
    }

    #[test]
    fn the_answer_is_keep_or_one_new_line() {
        assert_eq!(parse_answer("KEEP", "a"), None);
        assert_eq!(parse_answer("<desc>a</desc>", "a"), None, "the same text is a keep");
        assert_eq!(parse_answer("<desc>\"Billing and\n invoices\"</desc>", "a").as_deref(), Some("Billing and invoices"));
        assert_eq!(parse_answer(&format!("<desc>{}</desc>", "x".repeat(MAX_LEN + 1)), "a"), None, "an essay is not a description");
        assert_eq!(parse_answer("<desc>Ops \u{2014} disk and CPU RCAs</desc>", "a").as_deref(), Some("Ops, disk and CPU RCAs"));
        // The live answer that became amux-security's description: a preamble
        // with the real text below it and no tags is not taken at all.
        let live = "Based on the recent work items, the current description of \"(none)\" does not fit. Here's the updated description:\n\nSecurity review of amux";
        assert_eq!(parse_answer(live, ""), None);
        assert_eq!(parse_answer("Here it is:\n<desc>Security review of amux</desc>", "").as_deref(), Some("Security review of amux"));
    }

    #[test]
    fn a_worker_is_due_once_a_day_and_only_with_new_work() {
        assert!(due(100_000, 0, 50, DAY));
        assert!(!due(100_000, 90_000, 95_000, DAY), "checked under a day ago");
        assert!(!due(200_000, 90_000, 80_000, DAY), "nothing new since the check");
    }

    /// The write re-reads the file: an owner edit or a lock made while the
    /// model was thinking wins, and the replaced text is kept for Revert.
    #[test]
    fn the_write_keeps_the_old_text_and_yields_to_an_owner_edit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("w.env");
        std::fs::write(&p, "CC_DESC=\"old job\"\nCC_DIR=\"/tmp\"\n").unwrap();
        assert!(apply(&p, "old job", "new job", 7).unwrap());
        let e = sv::EnvFile::load(&p);
        assert_eq!(e.get("CC_DESC"), Some("new job"));
        assert_eq!(e.get("CC_DESC_PREV"), Some("old job"));
        assert_eq!(e.get("CC_DESC_BY"), Some("auto"));
        assert_eq!(e.get("CC_DIR"), Some("/tmp"));
        assert!(!apply(&p, "old job", "third", 8).unwrap(), "the description changed since it was read");
        std::fs::write(&p, "CC_DESC=\"new job\"\nCC_DESC_LOCK=\"1\"\n").unwrap();
        assert!(!apply(&p, "new job", "third", 9).unwrap(), "locked");
        assert_eq!(sv::EnvFile::load(&p).get("CC_DESC"), Some("new job"));
    }

    #[test]
    fn recent_work_reads_the_owners_messages_and_closed_cards() {
        let conn = crate::db::migrate::test_memdb();
        conn.execute_batch(
            "INSERT INTO cmd_history (text,type,session,ts,origin) VALUES
               ('fix the   invoice\nexport','user','w',100,''),
               ('peer chatter','user','w',110,'other-lane'),
               ('old','user','w',5,''),
               ('relayed ask','chat','W',120,'chat-companion');
             INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type,deleted) VALUES
               ('A-1','Ship billing page','','done','w',1,130,'agent','task',NULL),
               ('A-2','open card','','todo','w',1,140,'agent','task',NULL),
               ('A-3','gone','','done','w',1,150,'agent','task',1);",
        ).unwrap();
        let (lines, newest) = recent_work(&conn, "w", 50).unwrap();
        assert_eq!(lines, vec![
            "owner asked: fix the invoice export",
            "owner asked: relayed ask",
            "card closed: Ship billing page",
        ]);
        assert_eq!(newest, 130);
    }
}
