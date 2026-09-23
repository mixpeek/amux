//! Gemini turns in the token ledger (AMUX-4679).
//!
//! Three lanes on this fleet run gemini and every token they spent was invisible
//! to `/api/usage`, the Cost tab and every fleet total. The same confident-zero
//! shape AMUX-4583 closed for codex.
//!
//! THE CARD FOR THIS SAID THERE WAS NOTHING TO READ. It was wrong, and the
//! mistake is worth recording because it is easy to repeat:
//! `provider/static_providers.rs` asserts `!GeminiAdapter.capabilities()
//! .reports_usage`, which is TRUE and is a statement about the PROVIDER
//! INTERFACE, not about the world. The CLI persists per-turn usage on disk
//! regardless, exactly as codex does.
//!
//! Gemini writes `~/.gemini/tmp/<project>/chats/**/*.jsonl`, one JSON object per
//! line. Messages carry `id`, `timestamp`, `model` and a `tokens` object. A
//! subagent gets its own nested file with `kind: "subagent"` and the SAME
//! `projectHash`, so its spend attributes to the same lane.
//!
//! THE TOKEN ARITHMETIC IS DERIVED, NOT ASSUMED, and the first reading of it was
//! wrong. From one sample it looked like
//! `total = input + output + cached + thoughts + tool`, because that row's
//! `cached` happened to be 0. Over all 82 rows of that session, 70 do not fit.
//! Tested:
//!
//! ```text
//!   input+output+cached+thoughts+tool     70/82 mismatch
//!   input+output+thoughts+tool             0/82 mismatch
//!   input+output                          72/82 mismatch
//! ```
//!
//! So `cached` is a SUBSET of `input`, not a sibling bucket, and 70 of those
//! rows carry `cached > 0` (up to 133,260) so the fit is not vacuous. This is
//! the same convention codex uses, recorded at `codex_ledger.rs`'s
//! `parse_codex_from`: "input_tokens INCLUDES the cached part".
//!
//! Shipping the one-sample relation would have over-counted input by the cached
//! amount on every cache hit, silently, with a cost that looked measured. That
//! is the defect this ledger exists to prevent, reintroduced by its own fix.

use crate::db::SharedStore;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeminiTurn {
    pub ts: i64,
    /// The message's own `id`, a UUID. Stable across re-reads, so the ledger's
    /// `ON CONFLICT(conversation, message_id)` cannot bill one turn twice.
    /// Codex has no per-event id and keys on an ordinal instead; Gemini's is
    /// better, because it survives a file being rewritten with lines prepended.
    pub message_id: String,
    pub model: String,
    /// `[input, cache_read, cache_write, output]`, matching `token_ledger`.
    pub tokens: [i64; 4],
}

/// `~/.gemini/tmp`, from the OS home. Gemini writes here, NOT into amux's home,
/// so this does not take `home` the way the amux-owned jobs do.
fn gemini_chats_root() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".gemini/tmp")
}

/// Every chat transcript under `<root>/<project>/chats/`, including the nested
/// per-subagent directories. Bounded depth, because this walks a user home.
pub(crate) fn chat_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 4 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, depth + 1, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let chats = e.path().join("chats");
        if chats.is_dir() {
            walk(&chats, 0, &mut out);
        }
    }
    out.sort();
    out
}

/// The conversation key for a chat file.
///
/// Prefixed `gemini:` for the reason `codex_ledger::conversation_key` is
/// prefixed `codex:`: both providers name conversations with a UUID and
/// `ledger_cursor` is keyed by that string alone, so an unprefixed collision
/// would let one provider's cursor skip the other's file.
///
/// Reads `sessionId` from the header rather than parsing the filename, because
/// the two file shapes disagree: a main session is
/// `session-2026-09-15T10-39-6e64f2da.jsonl`, whose trailing token is an
/// 8-character TRUNCATION of the uuid, while a subagent file is named with the
/// full uuid. Truncating to 8 characters would be a real collision risk across
/// 55 files; the header carries the whole thing.
pub(crate) fn conversation_key(path: &Path) -> String {
    let id = header_field(path, "sessionId").unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    format!("gemini:{id}")
}

/// One field from the transcript's first line.
fn header_field(path: &Path, field: &str) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(f).read_line(&mut first).ok()?;
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    v.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// sha256 of an absolute working directory, which is what Gemini stores as
/// `projectHash`.
///
/// Verified rather than inferred: `sha256("/Users/ethan/Dev/amux")` is
/// `fbaf807375e0235e1a292e39f2c4904198caa5c6902cb5baa705b6caa680eb0b`, the
/// `projectHash` in that project's own transcripts, and a reverse lookup over
/// all 24 entries of `~/.gemini/projects.json` matches exactly one path.
///
/// Hashing the LANE'S OWN workdir means `projects.json` is never read: that file
/// is Gemini's private index, it holds friendly names rather than lanes, and a
/// mapping that depends on it would break the moment a project is renamed.
pub(crate) fn project_hash(workdir: &str) -> String {
    let mut h = Sha256::new();
    h.update(workdir.trim_end_matches('/').as_bytes());
    format!("{:x}", h.finalize())
}

/// `projectHash` -> lane, for the lanes whose workdir is unambiguous.
///
/// Reuses `codex_ledger::unambiguous_owners` for the cwd -> lane step, so two
/// lanes sharing a checkout stay unattributed here exactly as they do for codex
/// rather than having their spend guessed onto one of them.
pub(crate) fn owners_by_hash(workdirs: &BTreeMap<String, String>) -> HashMap<String, String> {
    super::codex_ledger::unambiguous_owners(workdirs)
        .into_iter()
        .map(|(cwd, lane)| (project_hash(&cwd), lane))
        .collect()
}

/// Parse messages after `offset`.
///
/// Returns the new byte offset and one turn per message that reported tokens. A
/// zero-token message is skipped rather than stored, matching codex: a ledger
/// row of four zeros is noise a reader has to explain.
pub(crate) fn parse_gemini_from(path: &Path, offset: u64) -> (u64, Vec<GeminiTurn>) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (offset, vec![]);
    };
    if offset > 0 && f.seek(SeekFrom::Start(offset)).is_err() {
        return (offset, vec![]);
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return (offset, vec![]);
    }
    let mut new_off = offset;
    let mut out = Vec::new();
    // `split` drops the delimiter, so the cursor counts it back: the same
    // arithmetic codex_ledger does, and getting it wrong re-reads or skips a
    // line on every tick.
    let mut parts = buf.split('\n').peekable();
    while let Some(line) = parts.next() {
        let is_last = parts.peek().is_none();
        if is_last && !line.is_empty() {
            // A partial final line: leave the cursor before it so the next tick
            // reads it whole. A half-written JSON object is not an empty turn.
            break;
        }
        new_off += line.len() as u64 + if is_last { 0 } else { 1 };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(tokens) = v.get("tokens").and_then(serde_json::Value::as_object) else {
            continue;
        };
        let n = |k: &str| {
            tokens
                .get(k)
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        };
        let cached = n("cached");
        // `input` INCLUDES `cached` (derived over 82 rows; see the module doc),
        // so the fresh part is the difference. `thoughts` and `tool` are billed
        // as output and are NOT inside `output`, which is why they are added
        // rather than assumed present.
        let fresh_input = (n("input") - cached).max(0);
        let output = n("output") + n("thoughts") + n("tool");
        let slots = [fresh_input, cached, 0, output];
        if slots.iter().all(|x| *x == 0) {
            continue;
        }
        let Some(message_id) = v.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        out.push(GeminiTurn {
            ts: v
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.timestamp())
                .unwrap_or(0),
            message_id: message_id.to_string(),
            model: v
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            tokens: slots,
        });
    }
    (new_off, out)
}

pub async fn index_once(store: &SharedStore, home: &Path) -> anyhow::Result<usize> {
    index_once_at(
        store,
        home,
        &gemini_chats_root(),
        &crate::api::session_verbs::all_session_workdirs(),
    )
    .await
}

pub(crate) async fn index_once_at(
    store: &SharedStore,
    home: &Path,
    root: &Path,
    workdirs: &BTreeMap<String, String>,
) -> anyhow::Result<usize> {
    use super::token_ledger::{self, LedgerFileBatch, LedgerRow};
    if !root.is_dir() {
        return Ok(0);
    }
    let table = token_ledger::prices(home);
    let owners = owners_by_hash(workdirs);
    let cursors = token_ledger::read_cursors(store)?;

    let mut batches: Vec<LedgerFileBatch> = Vec::new();
    for path in chat_files(root) {
        let conversation = conversation_key(&path);
        let Ok(meta) = path.metadata() else { continue };
        let size = meta.len();
        let offset = cursors.get(&conversation).copied().unwrap_or(0);
        if offset >= size {
            continue;
        }
        let offset = if offset > size { 0 } else { offset };
        let lane = header_field(&path, "projectHash")
            .and_then(|h| owners.get(&h).cloned())
            .unwrap_or_default();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (new_off, turns) = parse_gemini_from(&path, offset);
        if turns.is_empty() && new_off == offset {
            continue;
        }
        let rows = turns
            .into_iter()
            .map(|t| {
                let cost = token_ledger::turn_cost_usd(&table, &t.model, t.tokens);
                LedgerRow {
                    ts: t.ts,
                    session: lane.clone(),
                    conversation: conversation.clone(),
                    model: t.model,
                    tokens: t.tokens,
                    cost,
                    message_id: Some(t.message_id),
                }
            })
            .collect();
        batches.push(LedgerFileBatch {
            conversation,
            offset: new_off,
            mtime,
            rows,
        });
    }

    token_ledger::warn_unpriced("gemini", &table, &batches);
    token_ledger::commit_ledger_batch(store, batches).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let st = crate::db::Store::open(&dir.path().join("gemini-ledger-test.db")).unwrap();
        std::mem::forget(dir);
        std::sync::Arc::new(st)
    }

    /// One message in the shape gemini actually writes, copied from a live
    /// transcript on this box and trimmed.
    fn msg(id: &str, ts: &str, input: i64, cached: i64, output: i64, thoughts: i64) -> String {
        format!(
            r#"{{"id":"{id}","timestamp":"{ts}","type":"gemini","model":"gemini-2.5-pro","content":"x","tokens":{{"input":{input},"output":{output},"cached":{cached},"thoughts":{thoughts},"tool":0,"total":{total}}}}}"#,
            total = input + output + thoughts,
        )
    }

    fn header(project_hash: &str) -> String {
        format!(
            r#"{{"sessionId":"6e64f2da-206c-4f4d-83bc-bd8e43673d40","projectHash":"{project_hash}","startTime":"2026-09-15T10:39:03.756Z","lastUpdated":"2026-09-15T10:39:03.756Z","kind":"main"}}"#
        )
    }

    fn chat(root: &Path, project: &str, project_hash: &str, body: &str) -> PathBuf {
        let chats = root.join(project).join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        let p = chats.join("session-2026-09-15T10-39-6e64f2da.jsonl");
        // TRAILING NEWLINE, because that is what gemini writes: all 59
        // transcripts on this box end with one. The first cut of this fixture
        // omitted it, and three cells failed on a parser that was CORRECT:
        // it defers an unterminated final line rather than billing a
        // half-written one. A fixture that does not match the writer tests the
        // reader against a world that does not exist.
        std::fs::write(&p, format!("{}\n{}\n", header(project_hash), body)).unwrap();
        p
    }

    /// `cached` IS INSIDE `input`, and `thoughts`/`tool` are NOT inside `output`.
    ///
    /// THE CELL FOR THE MISTAKE. The first reading of this format came from one
    /// sample whose `cached` was 0, where `input+output+cached+thoughts+tool`
    /// equalled `total` exactly. Over the 82 rows of that session, 70 do not fit
    /// it. Shipping it would have over-counted input by the cached amount on
    /// every cache hit, silently, with a cost that looked measured.
    ///
    /// The numbers below are a real specimen:
    ///   {"input":41944,"output":55,"cached":36716,"thoughts":98,"tool":0,"total":42097}
    /// and 41944+55+98 = 42097, which is the arithmetic that holds.
    #[test]
    fn cached_input_is_not_billed_twice_and_thinking_is_output() {
        let dir = tempfile::tempdir().unwrap();
        let p = chat(
            dir.path(),
            "amux",
            "deadbeef",
            &msg("m1", "2026-09-15T10:39:10Z", 41944, 36716, 55, 98),
        );
        let (_, turns) = parse_gemini_from(&p, 0);
        assert_eq!(turns.len(), 1, "{turns:?}");
        let t = &turns[0];
        assert_eq!(
            t.tokens[0],
            41944 - 36716,
            "fresh input excludes the cached part"
        );
        assert_eq!(t.tokens[1], 36716, "the cached part is cache_read");
        assert_eq!(t.tokens[2], 0, "gemini reports no cache-write bucket");
        assert_eq!(
            t.tokens[3],
            55 + 98,
            "thoughts are billed as output and are not inside it"
        );
        // NOTHING IS LOST AND NOTHING IS DOUBLED. The four slots must sum to the
        // transcript's own total, which is the invariant the wrong relation broke.
        assert_eq!(
            t.tokens.iter().sum::<i64>(),
            42097,
            "the slots must reconstruct total: {t:?}"
        );
        assert_eq!(t.message_id, "m1");
        assert_eq!(t.model, "gemini-2.5-pro");
        assert!(
            t.ts > 1_700_000_000,
            "the timestamp is parsed, not zero: {t:?}"
        );
    }

    /// A message with no spend is skipped, and one with only cached input is
    /// not: a turn served entirely from cache still costs cache-read money.
    #[test]
    fn a_zero_turn_is_skipped_but_a_cache_only_turn_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "{}\n{}\n{}",
            msg("z", "2026-09-15T10:39:10Z", 0, 0, 0, 0),
            msg("c", "2026-09-15T10:39:11Z", 500, 500, 0, 0),
            r#"{"id":"no-tokens","timestamp":"2026-09-15T10:39:12Z","type":"user","content":"hi"}"#,
        );
        let p = chat(dir.path(), "amux", "deadbeef", &body);
        let (_, turns) = parse_gemini_from(&p, 0);
        let ids: Vec<&str> = turns.iter().map(|t| t.message_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["c"],
            "only the cache-only turn survives: {turns:?}"
        );
        assert_eq!(
            turns[0].tokens,
            [0, 500, 0, 0],
            "all of it was cache_read: {turns:?}"
        );
    }

    /// A HALF-WRITTEN FINAL LINE IS DEFERRED, not billed and not skipped.
    ///
    /// The indexer runs on a tick while a lane is mid-turn, so it will read a
    /// file gemini is still appending to. An unterminated last line is a line
    /// still being written: the cursor stops before it and the next tick reads
    /// it whole. All 59 transcripts on this box end with a newline, so this
    /// costs nothing in practice and it is what makes reading a live file safe.
    ///
    /// codex_ledger does the opposite, counting `len + 1` for every line
    /// including an unterminated one, which advances its cursor one byte past
    /// EOF. Not changed here: that is its file format's problem and a separate
    /// card, and the two parsers are not shared code.
    #[test]
    fn a_partial_final_line_waits_for_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let chats = dir.path().join("amux/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let p = chats.join("session-2026-09-15T10-39-6e64f2da.jsonl");
        let whole = msg("m1", "2026-09-15T10:39:10Z", 1000, 0, 50, 0);
        let partial = &msg("m2", "2026-09-15T10:39:20Z", 2000, 0, 60, 0)[..40];
        std::fs::write(
            &p,
            format!("{}\n{}\n{}", header("deadbeef"), whole, partial),
        )
        .unwrap();

        let (off, turns) = parse_gemini_from(&p, 0);
        assert_eq!(turns.len(), 1, "only the terminated message: {turns:?}");
        assert_eq!(turns[0].message_id, "m1");
        let total = std::fs::metadata(&p).unwrap().len();
        assert!(
            off < total,
            "the cursor stops before the partial line: {off} of {total}"
        );

        // FINISH THE LINE. The next pass must pick it up, or a deferral is just
        // a slower version of dropping it.
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n{}\n",
                header("deadbeef"),
                whole,
                msg("m2", "2026-09-15T10:39:20Z", 2000, 0, 60, 0)
            ),
        )
        .unwrap();
        let (_, turns2) = parse_gemini_from(&p, off);
        assert_eq!(turns2.len(), 1, "{turns2:?}");
        assert_eq!(
            turns2[0].message_id, "m2",
            "the once-partial message, now whole"
        );
    }

    /// The lane comes from sha256 of its own workdir, matching the transcript's
    /// `projectHash`. A lane whose checkout is shared stays unattributed rather
    /// than having another lane's spend guessed onto it.
    #[test]
    fn a_lane_is_found_by_hashing_its_own_workdir() {
        // The live value, so this pins the real hash rather than a round trip
        // through the same function.
        assert_eq!(
            project_hash("/Users/ethan/Dev/amux"),
            "fbaf807375e0235e1a292e39f2c4904198caa5c6902cb5baa705b6caa680eb0b",
            "this is the projectHash gemini wrote for that directory"
        );
        assert_eq!(
            project_hash("/Users/ethan/Dev/amux/"),
            project_hash("/Users/ethan/Dev/amux"),
            "a trailing slash is the same project"
        );

        let mut workdirs = BTreeMap::new();
        workdirs.insert("solo".to_string(), "/Users/ethan/Dev/amux".to_string());
        workdirs.insert(
            "shared-a".to_string(),
            "/Users/ethan/Dev/mixpeek".to_string(),
        );
        workdirs.insert(
            "shared-b".to_string(),
            "/Users/ethan/Dev/mixpeek".to_string(),
        );
        let owners = owners_by_hash(&workdirs);
        assert_eq!(
            owners
                .get(&project_hash("/Users/ethan/Dev/amux"))
                .map(String::as_str),
            Some("solo")
        );
        assert!(
            !owners.contains_key(&project_hash("/Users/ethan/Dev/mixpeek")),
            "two lanes share that checkout, so neither may be billed for it: {owners:?}"
        );
    }

    /// The conversation key comes from the header's full `sessionId`, not the
    /// filename. A main session's filename carries only an 8-character
    /// truncation of the uuid, which across 55 files is a real collision risk,
    /// and a cursor collision silently skips another file's turns.
    #[test]
    fn the_conversation_key_uses_the_full_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let p = chat(
            dir.path(),
            "amux",
            "deadbeef",
            &msg("m1", "2026-09-15T10:39:10Z", 10, 0, 5, 0),
        );
        assert_eq!(
            conversation_key(&p),
            "gemini:6e64f2da-206c-4f4d-83bc-bd8e43673d40"
        );
        assert!(
            conversation_key(&p).starts_with("gemini:"),
            "prefixed, or a uuid collision with codex would cross the providers' cursors"
        );
    }

    /// The whole pass: rows land keyed to the lane, and a second pass over an
    /// unchanged file bills nothing again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_second_pass_over_the_same_file_bills_nothing_twice() {
        let st = store();
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let body = format!(
            "{}\n{}",
            msg("m1", "2026-09-15T10:39:10Z", 1000, 400, 50, 10),
            msg("m2", "2026-09-15T10:39:20Z", 2000, 0, 80, 20),
        );
        chat(root.path(), "amux", &project_hash("/w/amux"), &body);
        let mut workdirs = BTreeMap::new();
        workdirs.insert("lane-amux".to_string(), "/w/amux".to_string());

        let n = index_once_at(&st, home.path(), root.path(), &workdirs)
            .await
            .unwrap();
        assert_eq!(n, 2, "two messages carried tokens");

        let rows: Vec<(String, String, i64, i64, i64)> = {
            let conn = st.read().unwrap();
            let mut stmt = conn
                .prepare("SELECT session, message_id, input, cache_read, output FROM token_ledger ORDER BY message_id")
                .unwrap();
            let r = stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap();
            r.flatten().collect()
        };
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(
            rows[0].0, "lane-amux",
            "billed to the lane whose workdir hashes to it"
        );
        assert_eq!(
            (rows[0].2, rows[0].3, rows[0].4),
            (600, 400, 60),
            "{rows:?}"
        );

        // THE RE-READ. The cursor is at EOF, so this must add nothing. Without
        // it a restart would re-bill every historical turn on this box.
        let again = index_once_at(&st, home.path(), root.path(), &workdirs)
            .await
            .unwrap();
        assert_eq!(again, 0, "an unchanged file has nothing new");
        let total: i64 = {
            let conn = st.read().unwrap();
            conn.query_row("SELECT COUNT(*) FROM token_ledger", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(total, 2, "and no duplicate rows");
    }

    /// An APPEND is picked up from the cursor, which is the live case: gemini
    /// writes to an open session while the lane works.
    #[tokio::test(flavor = "current_thread")]
    async fn an_append_is_indexed_from_where_the_last_pass_stopped() {
        let st = store();
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let p = chat(
            root.path(),
            "amux",
            &project_hash("/w/amux"),
            &msg("m1", "2026-09-15T10:39:10Z", 1000, 0, 50, 0),
        );
        let mut workdirs = BTreeMap::new();
        workdirs.insert("lane-amux".to_string(), "/w/amux".to_string());
        assert_eq!(
            index_once_at(&st, home.path(), root.path(), &workdirs)
                .await
                .unwrap(),
            1
        );

        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str(&msg("m2", "2026-09-15T10:39:30Z", 3000, 0, 90, 0));
        text.push('\n');
        std::fs::write(&p, text).unwrap();

        assert_eq!(
            index_once_at(&st, home.path(), root.path(), &workdirs)
                .await
                .unwrap(),
            1,
            "the appended message, and only it"
        );
        let ids: Vec<String> = {
            let conn = st.read().unwrap();
            let mut stmt = conn
                .prepare("SELECT message_id FROM token_ledger ORDER BY message_id")
                .unwrap();
            let r = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            r.flatten().collect()
        };
        assert_eq!(ids, vec!["m1".to_string(), "m2".to_string()]);
    }
}
