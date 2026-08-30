//! The `token_ledger` WRITER (AMUX-2892).
//!
//! Every reader of this table was ported at the cutover and the writer was not,
//! so on 2026-08-11 the table's newest row was 36.3 hours old, it had zero rows
//! in the last 24h, and `GET /api/stats/daily` — live, ported, and looking
//! entirely healthy — served `total_tokens: 0, sessions: 0`. The source data
//! was never the problem: 2637 conversation JSONLs under `~/.claude/projects`,
//! the newest written that same minute. Nothing read them.
//!
//! That is ethos rule 1's shape (capability that exists but never reaches
//! anyone) crossed with rule 4 (a wrong answer nobody could detect): a zero on
//! a usage panel is indistinguishable from a quiet day, so the failure mode is
//! a number people believe.
//!
//! Python contract (amux-server.py at 792ce1f^):
//!   `_index_token_ledger`   py:18117 — incremental parse, per-conversation
//!                                      byte cursor, dedup, price, stamp owner
//!   `_turn_cost_usd`        py:18108 · `_price_for_model` py:18099
//!   `_jsonl_owner_title`    py:17969 · `_attribute_ledger_tasks` py:18219
//!
//! ONE DEPARTURE, deliberate. In Python the only caller was
//! `observability_rollup` (py:18245) — the READER was the writer's trigger, so
//! the ledger only advanced while someone had the Cost tab open. Here it is a
//! periodic runtime job, and `/api/observability` may still poke it. A trigger
//! that fires only when observed is how a ledger goes stale without anyone
//! being able to notice, which is the bug this file exists to end.

use crate::db::{SharedStore, WriteOutcome};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// `<model substring, (input, cache_read, cache_write, output)>` in USD per
/// million tokens. Substring match, longest concern first is not needed — the
/// families are disjoint. `~/.amux/prices.json` overrides any row (py:18077),
/// so a price change is config, never a redeploy.
const MODEL_PRICES_DEFAULT: &[(&str, [f64; 4])] = &[
    ("opus", [15.0, 1.50, 18.75, 75.0]),
    ("sonnet", [3.0, 0.30, 3.75, 15.0]),
    ("haiku", [0.80, 0.08, 1.00, 4.0]),
    ("fable", [3.0, 0.30, 3.75, 15.0]),
];
const PRICE_DEFAULT: [f64; 4] = [3.0, 0.30, 3.75, 15.0];

/// py:18114 — `<synthetic>` turns carry usage that was never billed.
fn model_is_skipped(model: &str) -> bool {
    model.is_empty() || model == "<synthetic>"
}

fn prices(home: &Path) -> Vec<(String, [f64; 4])> {
    let mut table: Vec<(String, [f64; 4])> = MODEL_PRICES_DEFAULT
        .iter()
        .map(|(k, v)| ((*k).to_string(), *v))
        .collect();
    let Ok(text) = std::fs::read_to_string(home.join("prices.json")) else {
        return table;
    };
    let Ok(user) = serde_json::from_str::<serde_json::Value>(&text) else {
        return table;
    };
    let Some(obj) = user.as_object() else { return table };
    for (k, v) in obj {
        let Some(arr) = v.as_array() else { continue };
        if arr.len() != 4 {
            continue;
        }
        let mut rates = [0.0f64; 4];
        let mut ok = true;
        for (i, x) in arr.iter().enumerate() {
            match x.as_f64() {
                Some(f) => rates[i] = f,
                None => ok = false,
            }
        }
        if !ok {
            continue;
        }
        let key = k.to_lowercase();
        match table.iter_mut().find(|(t, _)| *t == key) {
            Some(row) => row.1 = rates,
            None => table.push((key, rates)),
        }
    }
    table
}

fn price_for_model(table: &[(String, [f64; 4])], model: &str) -> [f64; 4] {
    let m = model.to_lowercase();
    for (key, rates) in table {
        if key != "_default" && m.contains(key.as_str()) {
            return *rates;
        }
    }
    table
        .iter()
        .find(|(k, _)| k == "_default")
        .map(|(_, r)| *r)
        .unwrap_or(PRICE_DEFAULT)
}

fn turn_cost_usd(table: &[(String, [f64; 4])], model: &str, t: [i64; 4]) -> f64 {
    let p = price_for_model(table, model);
    (t[0] as f64 * p[0] + t[1] as f64 * p[1] + t[2] as f64 * p[2] + t[3] as f64 * p[3])
        / 1_000_000.0
}

/// py:17969 — the owning amux session. "" means an ad-hoc conversation amux
/// does not own; those still count toward fleet totals under an empty session.
///
/// Resolves through `conversation_owner` (meta claim, then LAST title record —
/// AMUX-2612), not the first line: a renamed lane's transcript keeps its birth
/// name on line 0 forever, and reading it here charged every token the `amux`
/// lane spent to `amux-rust`, a session that no longer exists. Rows indexed
/// before 2026-08-11 may still carry the dead name; the fix is forward.
/// `claims` IS A PARAMETER, and that is the whole point (AF-209).
///
/// It used to call `conversation_claims()` itself, which takes no arguments and
/// reads `~/.amux` on the live machine. `conversation_owner` already accepted
/// `claims`; this function was the one place that closed the seam back off, so
/// no caller below it could isolate the map — including a test.
///
/// WHAT THAT COST: `subagent_turns_are_charged_to_the_parent_conversation_owner`
/// hardcoded a real conversation id and therefore asserted WHO CURRENTLY OWNS A
/// REAL CONVERSATION ON THIS BOX. It passed as `gtm-videos` when written and
/// failed as `mixpeek-cicd` when a lane re-claimed that conversation, with no
/// commit to this file in between. Everything the fixture explicitly controlled
/// was already isolated — home tempdir, projects tempdir, its own store. The
/// one input it could not reach was the one the assertion rested on.
///
/// And it was invisible to CI in the worst possible direction: a runner has no
/// `~/.amux`, so `conversation_claims()` returns empty, the code falls through
/// to the title record, and the test passes. Green where nobody works, red on
/// every machine the fleet runs on. CI can never report this class, so the
/// parameter is the fix and the synthetic id alone would not have been —
/// that avoids one collision and leaves the hole open for the next fixture.
fn jsonl_owner_title(path: &Path, claims: &BTreeMap<String, String>) -> String {
    crate::api::session_verbs::conversation_owner(path, claims)
}

struct Turn {
    ts: i64,
    session: String,
    conversation: String,
    model: String,
    tokens: [i64; 4],
    cost: f64,
}

/// Parse one JSONL from `offset` onward. Returns the new offset and the turns.
///
/// The dedup is load-bearing and easy to get subtly wrong: Claude Code emits
/// the SAME usage block again for a turn's thinking and tool_use parts, so
/// counting every `usage` line double- or triple-bills a turn. Python keyed on
/// `(input, cache_read, output)` and compared only against the IMMEDIATELY
/// preceding line (py:18133) — deliberately not a set, because two genuinely
/// distinct turns can carry identical counts and a set would silently drop the
/// second.
fn parse_from(path: &Path, offset: u64, fallback_ts: i64, owner: &str, table: &[(String, [f64; 4])]) -> (u64, Vec<Turn>) {
    let conversation = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let Ok(mut f) = std::fs::File::open(path) else {
        return (offset, vec![]);
    };
    if offset > 0 && f.seek(SeekFrom::Start(offset)).is_err() {
        return (offset, vec![]);
    }
    let mut new_off = offset;
    let mut out = Vec::new();
    let mut prev_sig: Option<(i64, i64, i64)> = None;
    for raw in BufReader::new(f).split(b'\n') {
        let Ok(mut bytes) = raw else { break };
        // `split` strips the delimiter; the cursor must still count it or every
        // line permanently shifts the offset backwards by one byte and the
        // NEXT pass re-reads a partial line as garbage.
        let consumed = bytes.len() as u64 + 1;
        new_off += consumed;
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let Ok(e) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        let msg = &e["message"];
        let u = &msg["usage"];
        if !u.is_object() {
            prev_sig = None;
            continue;
        }
        let model = msg["model"].as_str().unwrap_or("").to_string();
        if model_is_skipped(&model) {
            continue;
        }
        let g = |k: &str| u[k].as_i64().unwrap_or(0);
        let tokens = [
            g("input_tokens"),
            g("cache_read_input_tokens"),
            g("cache_creation_input_tokens"),
            g("output_tokens"),
        ];
        let sig = (tokens[0], tokens[1], tokens[3]);
        if prev_sig == Some(sig) {
            continue;
        }
        prev_sig = Some(sig);
        if tokens.iter().sum::<i64>() == 0 {
            continue;
        }
        let ts = e["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or(fallback_ts);
        out.push(Turn {
            ts,
            session: owner.to_string(),
            conversation: conversation.clone(),
            model: model.clone(),
            tokens,
            cost: turn_cost_usd(table, &model, tokens),
        });
    }
    (new_off, out)
}

fn claude_projects_dir() -> PathBuf {
    std::env::var("CLAUDE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude")
        })
        .join("projects")
}

/// One indexing pass. Returns rows inserted. Cheap on a fully-indexed tree —
/// a stat per file and nothing else.
pub async fn index_once(store: &SharedStore, home: &Path) -> anyhow::Result<usize> {
    index_once_at(
        store,
        home,
        &claude_projects_dir(),
        &crate::api::session_verbs::conversation_claims(),
    )
    .await
}

/// The pass, with the projects root injected. Split out so tests drive a temp
/// tree instead of setting a process-global env var — env in tests races every
/// other test in the binary, and a racy test is one that gets deleted later.
pub async fn index_once_at(
    store: &SharedStore,
    home: &Path,
    projects: &Path,
    claims: &BTreeMap<String, String>,
) -> anyhow::Result<usize> {
    if !projects.is_dir() {
        return Ok(0);
    }
    let projects = projects.to_path_buf();
    let table = prices(home);

    let cursors: HashMap<String, (u64, i64)> = {
        let conn = store.read()?;
        let mut stmt = conn.prepare("SELECT conversation, offset, mtime FROM ledger_cursor")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)?)))
        })?;
        rows.flatten().collect()
    };

    // Collect the work OUTSIDE the write lock: parsing 2637 JSONLs while
    // holding it would stall every other writer on the shared store.
    //
    // CAPPED PER PASS, and the cap is LOGGED (ethos: no silent caps). The first
    // run after this ships has ~2086 un-indexed conversations to backfill, and
    // committing all of them in one transaction would hold the write lock long
    // enough to be felt by a 50-session fleet on a machine that is supposed to
    // stay up. At 120s a tick the backlog drains in well under an hour, and a
    // steady state never reaches the cap.
    let cap = std::env::var("AMUX_LEDGER_INDEX_CAP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(200);
    let mut skipped_for_cap = 0usize;
    let mut pending: Vec<(String, u64, i64, Vec<Turn>)> = Vec::new();
    // Owner titles are read from a conversation's FIRST line; subagent files
    // all share one parent, so cache per parent rather than re-opening it once
    // per delegated transcript.
    let mut owner_cache: HashMap<PathBuf, String> = HashMap::new();
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Ok(0);
    };
    for proj in entries.flatten() {
        let p = proj.path();
        if !p.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&p) else { continue };
        for f in files.flatten() {
            let path = f.path();
            // Two shapes under a project dir:
            //   <proj>/<conversation-uuid>.jsonl              a real conversation
            //   <proj>/<conversation-uuid>/subagents/*.jsonl  its delegated turns
            let mut targets: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
            if path.is_dir() {
                let subs = path.join("subagents");
                if let Ok(rd) = std::fs::read_dir(&subs) {
                    // The parent conversation is the sibling <uuid>.jsonl, and
                    // it is where the owning session's name lives — a subagent
                    // transcript's first record is {"type":"user"} with no
                    // customTitle at all (AMUX-2894).
                    let parent = path.with_extension("jsonl");
                    for sf in rd.flatten() {
                        let sp = sf.path();
                        if sp.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                            targets.push((sp, Some(parent.clone())));
                        }
                    }
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                targets.push((path, None));
            }

            for (jf, parent) in targets {
                let Ok(st) = jf.metadata() else { continue };
                let size = st.len();
                let mtime = st
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let conv = jf.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                let (off, cmt) = cursors.get(&conv).copied().unwrap_or((0, 0));
                // Both halves matter. Size alone misses an in-place rewrite;
                // mtime alone re-reads an untouched file after any `touch`.
                if off >= size && cmt == mtime {
                    continue;
                }
                if cap > 0 && pending.len() >= cap {
                    skipped_for_cap += 1;
                    continue;
                }
                // A file that SHRANK was rotated or rewritten — re-read from 0
                // rather than seeking past its new end, which would park the
                // cursor permanently beyond the data and index nothing forever.
                let start = if off > size { 0 } else { off };
                let owner = match &parent {
                    None => jsonl_owner_title(&jf, claims),
                    Some(pp) => owner_cache
                        .entry(pp.clone())
                        .or_insert_with(|| jsonl_owner_title(pp, claims))
                        .clone(),
                };
                let (new_off, turns) = parse_from(&jf, start, mtime, &owner, &table);
                pending.push((conv, new_off, mtime, turns));
            }
        }
    }
    if skipped_for_cap > 0 {
        tracing::info!(
            indexed = pending.len(), deferred = skipped_for_cap, cap,
            "token-ledger: capped this pass; the rest are picked up next tick"
        );
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let expected: usize = pending.iter().map(|(_, _, _, t)| t.len()).sum();

    let inserted = store
        .write_async(move |conn| {
            let mut n = 0usize;
            {
                let mut ins = conn.prepare(
                    "INSERT INTO token_ledger
                       (ts, session, conversation, model, input, cache_read, cache_write, output, cost_usd)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                )?;
                let mut cur = conn.prepare(
                    "INSERT INTO ledger_cursor (conversation, offset, mtime) VALUES (?1,?2,?3)
                     ON CONFLICT(conversation) DO UPDATE SET offset=?2, mtime=?3",
                )?;
                for (conv, off, mtime, turns) in &pending {
                    for t in turns {
                        ins.execute(rusqlite::params![
                            t.ts, t.session, t.conversation, t.model,
                            t.tokens[0], t.tokens[1], t.tokens[2], t.tokens[3], t.cost
                        ])?;
                        n += 1;
                    }
                    cur.execute(rusqlite::params![conv, *off as i64, mtime])?;
                }
            }
            debug_assert_eq!(n, expected, "insert count must match what was parsed");
            Ok(WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await
        .map(|_| expected)?;

    if inserted > 0 {
        attribute_tasks(store).await?;
    }
    Ok(inserted)
}

/// py:18219 — fill `token_ledger.task` for turns that fall inside a card's
/// doing-window for the SAME session. Only touches unattributed rows.
pub async fn attribute_tasks(store: &SharedStore) -> anyhow::Result<()> {
    store
        .write_async(move |conn| {
            let now = chrono::Utc::now().timestamp();
            let wins: Vec<(String, String, i64, i64)> = {
                let mut stmt = conn.prepare(
                    "SELECT task, session, entered_doing, COALESCE(left_doing, ?1) lo
                     FROM task_windows ORDER BY entered_doing",
                )?;
                let rows = stmt.query_map([now], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?;
                rows.flatten().collect()
            };
            for (task, session, from, to) in wins {
                conn.execute(
                    "UPDATE token_ledger SET task=?1
                     WHERE task='' AND session=?2 AND ts>=?3 AND ts<=?4",
                    rusqlite::params![task, session, from, to],
                )?;
            }
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await?;
    Ok(())
}

/// Every 120s by default. A cheap no-op on a fully-indexed tree (one stat per
/// conversation), and the JSONLs it reads are appended continuously by the
/// fleet, so a longer period only widens the window in which a usage panel is
/// wrong. `AMUX_LEDGER_INDEX_SECS=0` disables it.
pub fn spawn(state: crate::api::AppState) -> Option<super::PeriodicTask> {
    let secs = std::env::var("AMUX_LEDGER_INDEX_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(120);
    if secs == 0 {
        tracing::info!("token-ledger indexer: disabled (AMUX_LEDGER_INDEX_SECS=0)");
        return None;
    }
    let home = crate::api::settings::amux_home();
    Some(super::spawn_periodic("token-ledger", secs, move || {
        let store = state.store.clone();
        let home = home.clone();
        async move {
            match index_once(&store, &home).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(rows = n, "token-ledger indexed"),
                // LOUD. The whole reason this job exists is that a silent gap
                // between writer and readers served a confident zero for
                // 36 hours; a failing indexer must not reproduce that quietly.
                Err(e) => tracing::warn!(error = %e, "token-ledger index failed"),
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<(String, [f64; 4])> {
        MODEL_PRICES_DEFAULT.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn pricing_matches_the_family_and_falls_back() {
        let t = table();
        assert_eq!(price_for_model(&t, "claude-opus-5"), [15.0, 1.50, 18.75, 75.0]);
        assert_eq!(price_for_model(&t, "claude-haiku-4-5-20251001"), [0.80, 0.08, 1.00, 4.0]);
        // An unknown model must PRICE, not zero — a 0 here reads as a free turn.
        assert_eq!(price_for_model(&t, "some-future-model"), PRICE_DEFAULT);
        // 1M output tokens on opus = $75 exactly.
        assert!((turn_cost_usd(&t, "opus", [0, 0, 0, 1_000_000]) - 75.0).abs() < 1e-9);
    }

    #[test]
    fn prices_json_overrides_a_default_and_can_add_a_family() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("prices.json"),
            r#"{"opus": [1,2,3,4], "newmodel": [9,9,9,9], "bad": [1,2]}"#,
        )
        .unwrap();
        let t = prices(dir.path());
        assert_eq!(price_for_model(&t, "claude-opus-5"), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(price_for_model(&t, "newmodel-x"), [9.0, 9.0, 9.0, 9.0]);
        // A malformed row is ignored, not fatal, and must not shadow anything.
        assert_eq!(price_for_model(&t, "sonnet"), [3.0, 0.30, 3.75, 15.0]);
    }

    fn line(model: &str, ts: &str, inp: i64, cr: i64, cw: i64, out: i64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","message":{{"model":"{model}","usage":{{"input_tokens":{inp},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cw},"output_tokens":{out}}}}}}}"#
        )
    }

    #[test]
    fn repeated_usage_blocks_are_billed_once_but_identical_separate_turns_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let jf = dir.path().join("conv1.jsonl");
        let body = [
            r#"{"customTitle":"alpha"}"#.to_string(),
            line("claude-opus-5", "2026-08-11T10:00:00Z", 100, 0, 0, 10),
            // Same turn re-emitted for thinking + tool_use — must collapse.
            line("claude-opus-5", "2026-08-11T10:00:00Z", 100, 0, 0, 10),
            // A DIFFERENT turn in between, then the same counts again. A set
            // would swallow the third; adjacent-only dedup keeps it.
            line("claude-opus-5", "2026-08-11T10:01:00Z", 5, 0, 0, 1),
            line("claude-opus-5", "2026-08-11T10:02:00Z", 100, 0, 0, 10),
            // Zero-token and synthetic turns are not rows.
            line("claude-opus-5", "2026-08-11T10:03:00Z", 0, 0, 0, 0),
            line("<synthetic>", "2026-08-11T10:04:00Z", 500, 0, 0, 500),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&jf, &body).unwrap();

        let (off, turns) = parse_from(&jf, 0, 0, "alpha", &table());
        assert_eq!(turns.len(), 3, "2 distinct + 1 repeat-after-gap");
        assert_eq!(turns.iter().map(|t| t.tokens[0]).collect::<Vec<_>>(), vec![100, 5, 100]);
        assert_eq!(off, body.len() as u64, "cursor must land exactly at EOF");
        assert_eq!(turns[0].conversation, "conv1");
        assert!(turns[0].cost > 0.0);
    }

    #[test]
    fn resuming_from_the_cursor_reads_only_the_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let jf = dir.path().join("conv2.jsonl");
        let first = line("sonnet", "2026-08-11T10:00:00Z", 10, 0, 0, 2) + "\n";
        std::fs::write(&jf, &first).unwrap();
        let (off, turns) = parse_from(&jf, 0, 0, "", &table());
        assert_eq!(turns.len(), 1);
        assert_eq!(off, first.len() as u64);

        // Append; a resumed pass must yield ONLY the appended turn.
        let second = line("sonnet", "2026-08-11T10:05:00Z", 20, 0, 0, 4) + "\n";
        std::fs::write(&jf, first.clone() + &second).unwrap();
        let (off2, turns2) = parse_from(&jf, off, 0, "", &table());
        assert_eq!(turns2.len(), 1, "the already-indexed line must not be re-billed");
        assert_eq!(turns2[0].tokens[0], 20);
        assert_eq!(off2, (first.len() + second.len()) as u64);
    }

    #[test]
    fn a_turn_with_no_parseable_timestamp_falls_back_instead_of_landing_at_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let jf = dir.path().join("conv3.jsonl");
        std::fs::write(
            &jf,
            r#"{"timestamp":"not-a-date","message":{"model":"sonnet","usage":{"input_tokens":7,"output_tokens":1}}}"#,
        )
        .unwrap();
        let (_, turns) = parse_from(&jf, 0, 1_786_000_000, "", &table());
        assert_eq!(turns.len(), 1);
        // 0 would silently park the turn in 1970 and drop it out of every
        // since-cutoff query — a row that exists and can never be counted.
        assert_eq!(turns[0].ts, 1_786_000_000);
    }

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let st = crate::db::Store::open(&dir.path().join("ledger-test.db")).unwrap();
        std::mem::forget(dir);
        std::sync::Arc::new(st)
    }

    async fn ledger(store: &SharedStore) -> Vec<(String, String, i64)> {
        let conn = store.read().unwrap();
        let mut stmt = conn
            .prepare("SELECT session, conversation, input FROM token_ledger ORDER BY conversation, input")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.flatten().collect()
    }

    /// AMUX-2894. A subagent transcript's first record is `{"type":"user"}` —
    /// no customTitle — so reading its owner the ordinary way yields "" and
    /// every delegated turn lands in the unowned bucket. That is WORSE than not
    /// indexing them: ~112k records would move from invisible to misattributed,
    /// and the lane that actually spent the tokens still would not be charged.
    #[tokio::test]
    async fn subagent_turns_are_charged_to_the_parent_conversation_owner() {
        let home = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let proj = projects.path().join("-Users-ethan-Dev-amux");
        // SYNTHETIC, and it must stay synthetic (AF-209). This was a REAL
        // conversation id, so with the claims map unreachable the assertion
        // below asserted who currently owns that conversation on the developer's
        // machine. It read `gtm-videos` when written and `mixpeek-cicd` once a
        // lane re-claimed it, months later, with no commit to this file. A uuid
        // that cannot exist collides with nothing.
        let conv = "00000000-af20-9000-0000-000000000001";
        std::fs::create_dir_all(proj.join(conv).join("subagents")).unwrap();

        // Parent conversation: carries the owning session.
        std::fs::write(
            proj.join(format!("{conv}.jsonl")),
            format!("{}\n{}\n", r#"{"customTitle":"gtm-videos"}"#,
                    line("claude-opus-5", "2026-08-11T10:00:00Z", 11, 0, 0, 1)),
        )
        .unwrap();
        // Delegated transcript: NO owner of its own.
        std::fs::write(
            proj.join(conv).join("subagents").join("agent-abc123.jsonl"),
            format!("{}\n{}\n", r#"{"type":"user"}"#,
                    line("claude-opus-5", "2026-08-11T10:01:00Z", 22, 0, 0, 2)),
        )
        .unwrap();

        let st = store();
        // The claims map is now an ARGUMENT, so this cell states which one it
        // is testing under instead of inheriting whatever is on the machine.
        // Empty = no meta claim, so the owner resolves from the title record.
        let claims = BTreeMap::new();
        let n = index_once_at(&st, home.path(), projects.path(), &claims).await.unwrap();
        assert_eq!(n, 2, "one parent turn + one delegated turn");

        let rows = ledger(&st).await;
        let by_conv: std::collections::HashMap<_, _> =
            rows.iter().map(|(s, c, i)| (c.clone(), (s.clone(), *i))).collect();
        assert_eq!(by_conv[conv], ("gtm-videos".into(), 11));
        assert_eq!(
            by_conv["agent-abc123"],
            ("gtm-videos".into(), 22),
            "delegated spend must be charged to the lane that delegated it"
        );
        // And it stays DISTINGUISHABLE without a schema change: the subagent
        // keeps its own `agent-*` conversation id, so "of which N was
        // delegated" is a query, not a migration.
        assert!(by_conv.keys().any(|k| k.starts_with("agent-")));
    }

    /// A subagent whose parent conversation was deleted has no owner to
    /// inherit. It must land unowned — honestly uncharged — rather than being
    /// attributed to whichever lane happens to be nearby.
    #[tokio::test]
    async fn an_orphaned_subagent_is_unowned_rather_than_misattributed() {
        let home = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let proj = projects.path().join("-proj");
        std::fs::create_dir_all(proj.join("dead-uuid").join("subagents")).unwrap();
        std::fs::write(
            proj.join("dead-uuid").join("subagents").join("agent-zzz.jsonl"),
            format!("{}\n{}\n", r#"{"type":"user"}"#,
                    line("sonnet", "2026-08-11T10:00:00Z", 5, 0, 0, 1)),
        )
        .unwrap();

        let st = store();
        // No claim for this conversation: the owner must come from the
        // transcript, which is what this cell is about.
        let claims = BTreeMap::new();
        assert_eq!(index_once_at(&st, home.path(), projects.path(), &claims).await.unwrap(), 1);
        assert_eq!(ledger(&st).await, vec![(String::new(), "agent-zzz".into(), 5)]);
    }

    /// THE CLAIMS MAP MUST REACH THE INDEXER (AF-209).
    ///
    /// Without this cell, threading `claims` through is decoration: every other
    /// test passes an EMPTY map, so a future edit could drop the parameter and
    /// go back to reading the live machine with the whole module still green.
    /// That is exactly how the seam closed the first time.
    ///
    /// It is also the positive control for the synthetic-uuid fix. Making the
    /// fixture synthetic stops one collision; proving the map is an argument is
    /// what stops the next fixture repeating it silently.
    ///
    /// The property: a meta CLAIM outranks the transcript's own title record
    /// (`conversation_owner` resolves claim first, then the last title). So a
    /// transcript that says `gtm-videos` must be charged to the claimant.
    #[tokio::test]
    async fn a_meta_claim_outranks_the_transcript_title_and_reaches_the_ledger() {
        let home = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let proj = projects.path().join("-proj");
        let conv = "00000000-af20-9000-0000-000000000002";
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join(format!("{conv}.jsonl")),
            format!("{}\n{}\n", r#"{"customTitle":"gtm-videos"}"#,
                    line("sonnet", "2026-08-11T10:00:00Z", 7, 0, 0, 1)),
        )
        .unwrap();

        let st = store();
        let mut claims = BTreeMap::new();
        claims.insert(conv.to_string(), "claiming-lane".to_string());
        assert_eq!(index_once_at(&st, home.path(), projects.path(), &claims).await.unwrap(), 1);
        assert_eq!(
            ledger(&st).await,
            vec![("claiming-lane".to_string(), conv.to_string(), 7)],
            "the claim must win over the title record, and it must arrive via the ARGUMENT"
        );
    }

    /// A second pass over an unchanged tree must insert NOTHING. Without this
    /// the indexer double-bills every conversation every 120s, which is the one
    /// failure mode worse than the zero it was written to fix.
    #[tokio::test]
    async fn a_second_pass_over_an_unchanged_tree_bills_nothing() {
        let home = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let proj = projects.path().join("-proj");
        std::fs::create_dir_all(proj.join("u1").join("subagents")).unwrap();
        std::fs::write(
            proj.join("u1.jsonl"),
            format!("{}\n{}\n", r#"{"customTitle":"amux"}"#,
                    line("sonnet", "2026-08-11T10:00:00Z", 9, 0, 0, 1)),
        )
        .unwrap();
        std::fs::write(
            proj.join("u1").join("subagents").join("agent-q.jsonl"),
            format!("{}\n{}\n", r#"{"type":"user"}"#,
                    line("sonnet", "2026-08-11T10:01:00Z", 3, 0, 0, 1)),
        )
        .unwrap();

        let st = store();
        // No claim for this conversation: the owner must come from the
        // transcript, which is what this cell is about.
        let claims = BTreeMap::new();
        assert_eq!(index_once_at(&st, home.path(), projects.path(), &claims).await.unwrap(), 2);
        assert_eq!(
            index_once_at(&st, home.path(), projects.path(), &claims).await.unwrap(),
            0,
            "an unchanged tree must be a no-op"
        );
        assert_eq!(ledger(&st).await.len(), 2);
    }

    /// The LAST title record wins, not the first (AMUX-2612): `--name` on a
    /// resume APPENDS a fresh record, so the first line holds the name the
    /// conversation was born with forever. This test used to pin the opposite
    /// — which is exactly the semantic that charged the renamed `amux` lane's
    /// tokens to `amux-rust` for a month.
    #[test]
    fn owner_comes_from_the_last_title_record_and_is_empty_for_ad_hoc_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("a.jsonl");
        std::fs::write(&named, "{\"customTitle\":\"amux-rust\"}\n{\"customTitle\":\"amux\"}\n")
            .unwrap();
        assert_eq!(jsonl_owner_title(&named, &BTreeMap::new()), "amux");

        let anon = dir.path().join("b.jsonl");
        std::fs::write(&anon, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(jsonl_owner_title(&anon, &BTreeMap::new()), "");

        let missing = dir.path().join("nope.jsonl");
        assert_eq!(jsonl_owner_title(&missing, &BTreeMap::new()), "");
    }
}
