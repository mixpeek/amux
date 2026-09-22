//! Codex turns in the token ledger (AMUX-4583).
//!
//! Nine lanes on this fleet run codex, and every token they spent was invisible
//! to `/api/usage`, `/api/observability` and the Cost tab: `translate_codex`
//! discards the usage on `turn.completed`, and `~/.codex/sessions` was read only
//! to decide whether a lane looked busy. A dashboard that shows the fleet's
//! spend while silently omitting a provider is the confident-zero shape the
//! token ledger itself was built to end.
//!
//! Codex writes the same thing Claude does, in a different place and shape:
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, one `event_msg` per
//! `token_count` carrying `info.last_token_usage` (the delta for that turn) and
//! `info.total_token_usage` (cumulative). This reads the DELTA, keyed by the
//! event's own `ordinal`, so a re-read cannot double-bill.
//!
//! Attribution is by working directory and only when it is unambiguous. Codex
//! records `cwd` in its `session_meta`; a lane records the same path. When two
//! lanes share one directory the row is left unattributed (`session = ''`),
//! which the ledger already means as "counts toward fleet totals, owned by
//! nobody". Charging a guess to a named lane is worse than charging nobody:
//! the ledger is what the cost view bills, and AMUX-2612 already records what
//! a wrong owner costs to unpick.
use crate::db::SharedStore;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One codex turn's usage, as the ledger stores it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexTurn {
    pub ts: i64,
    /// The event's position in its own file: stable, so re-reading a file
    /// cannot bill the same turn twice.
    pub ordinal: i64,
    pub model: String,
    /// `[input, cache_read, cache_write, output]`, matching `token_ledger`.
    pub tokens: [i64; 4],
}

/// `~/.codex/sessions`, from the OS home. Codex writes here, NOT into amux's
/// own home; `codex_rollout_files` records what pointing at the wrong one cost.
fn codex_sessions_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/sessions")
}

/// Every `rollout-*.jsonl` under `root`, depth-bounded like the transcript walk.
fn rollout_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 3 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, depth + 1, out);
            } else if p
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("rollout-") && s.ends_with(".jsonl"))
            {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out
}

/// The conversation key for a rollout file.
///
/// Prefixed `codex:` on purpose. Both providers name conversations with a
/// UUID, and the ledger's cursor table is keyed by that string alone, so an
/// unprefixed collision would let one provider's cursor skip the other's file.
/// It also makes a codex row identifiable in the ledger without a join.
pub(crate) fn conversation_key(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // rollout-2026-09-09T14-12-21-<uuid>
    let id = stem
        .rsplit_once("-")
        .map(|(_, tail)| tail.to_string())
        .unwrap_or_default();
    let id = if id.len() >= 12 { id } else { stem.clone() };
    format!("codex:{id}")
}

/// The `cwd` a rollout file was opened in, from its `session_meta` first line.
pub(crate) fn rollout_cwd(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(f).read_line(&mut first).ok()?;
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    v.pointer("/payload/cwd")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
}

/// Parse `token_count` deltas after `offset`.
///
/// Returns the new byte offset and one turn per event that reported any tokens.
/// A zero-token event is skipped rather than stored: codex emits one on turns
/// that spent nothing, and a ledger row of four zeros is noise a reader has to
/// explain.
pub(crate) fn parse_codex_from(path: &Path, offset: u64) -> (u64, Vec<CodexTurn>) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (offset, vec![]);
    };
    if offset > 0 && f.seek(SeekFrom::Start(offset)).is_err() {
        return (offset, vec![]);
    }
    let mut new_off = offset;
    let mut out = Vec::new();
    let mut model = String::new();
    for raw in BufReader::new(f).split(b'\n') {
        let Ok(mut bytes) = raw else { break };
        // `split` drops the delimiter; the cursor must still count it, or every
        // line shifts the offset back by one byte and the next pass re-reads a
        // partial line as garbage (the same trap the Claude parser records).
        new_off += bytes.len() as u64 + 1;
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let Ok(e) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        // The model is named by turn context events, not by `session_meta`.
        // Remember the most recent one: it is what the turns below were spent on.
        for key in ["/payload/model", "/model", "/payload/turn_context/model"] {
            if let Some(m) = e.pointer(key).and_then(serde_json::Value::as_str) {
                if !m.trim().is_empty() {
                    model = m.trim().to_string();
                }
            }
        }
        if e.pointer("/payload/type")
            .and_then(serde_json::Value::as_str)
            != Some("token_count")
        {
            continue;
        }
        let Some(last) = e.pointer("/payload/info/last_token_usage") else {
            continue;
        };
        let n = |k: &str| last.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        // `input_tokens` INCLUDES the cached part (measured on a live rollout:
        // 58880 input of which 57344 cached), and `total_tokens` is input +
        // output, so `reasoning_output_tokens` is already inside output_tokens
        // and must not be added again.
        let cache_read = n("cached_input_tokens");
        let fresh_input = (n("input_tokens") - cache_read).max(0);
        let tokens = [
            fresh_input,
            cache_read,
            n("cache_write_input_tokens"),
            n("output_tokens"),
        ];
        if tokens.iter().all(|t| *t == 0) {
            continue;
        }
        let Some(ts) = e
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp())
        else {
            continue;
        };
        let ordinal = e
            .get("ordinal")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        out.push(CodexTurn {
            ts,
            ordinal,
            model: model.clone(),
            tokens,
        });
    }
    (new_off, out)
}

/// cwd -> the single lane that works there. A directory two lanes share is
/// dropped rather than guessed at.
pub(crate) fn unambiguous_owners(workdirs: &BTreeMap<String, String>) -> HashMap<String, String> {
    let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
    for (lane, dir) in workdirs {
        let dir = dir.trim().trim_end_matches('/').to_string();
        if dir.is_empty() {
            continue;
        }
        by_dir.entry(dir).or_default().push(lane.clone());
    }
    by_dir
        .into_iter()
        .filter(|(_, lanes)| lanes.len() == 1)
        .map(|(dir, lanes)| (dir, lanes[0].clone()))
        .collect()
}

/// Prefer durable validated workspace identity over the configured parent clone.
/// Retirement keeps both the workspace record and .env.reaped as provenance.
pub(crate) fn workspace_workdirs(
    home: &Path,
    mut dirs: BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    if let Ok(entries) = std::fs::read_dir(home.join("workspaces")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !crate::api::session_verbs::valid_session_name(name) {
                continue;
            }
            let Some(w) = crate::fanout_workspace::load(home, name) else {
                continue;
            };
            let env = home.join("sessions").join(format!("{name}.env"));
            let env = if env.exists() {
                env
            } else {
                env.with_extension("env.reaped")
            };
            let settings = crate::config::parse_env_file(&env);
            if w.path != crate::fanout_workspace::expected_path(&w.repo, name).to_string_lossy()
                || w.branch != format!("amux/fanout/{name}")
                || !Path::new(&w.repo).is_absolute()
                || w.base.len() != 40
                || !w.base.bytes().all(|b| b.is_ascii_hexdigit())
                || settings.get("CC_DIR") != Some(&w.repo)
            {
                continue;
            }
            dirs.insert(name.into(), w.path);
        }
    }
    dirs
}

/// Read through the existing session index once; write only bounded exact row IDs.
fn ownership_repairs(
    c: &rusqlite::Connection,
    owners: &[(String, String)],
) -> rusqlite::Result<Vec<(i64, String)>> {
    if owners.is_empty() {
        return Ok(vec![]);
    }
    let mut unique = HashMap::<String, Option<String>>::new();
    for (conversation, owner) in owners {
        unique
            .entry(conversation.clone())
            .and_modify(|prior| {
                if prior.as_deref() != Some(owner.as_str()) {
                    *prior = None;
                }
            })
            .or_insert_with(|| Some(owner.clone()));
    }
    let owners: HashMap<_, _> = unique
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
        .collect();
    if owners.is_empty() {
        return Ok(vec![]);
    }

    let conversations = serde_json::to_string(&owners.keys().collect::<Vec<_>>()).unwrap();
    let mut q=c.prepare("SELECT id,conversation FROM token_ledger INDEXED BY idx_ledger_session WHERE session='' AND task='' AND conversation IN (SELECT value FROM json_each(?1)) ORDER BY id LIMIT 1000")?;
    let rows = q.query_map([conversations], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|r| r.map(|(id, conversation)| (id, owners[&conversation].clone())))
        .collect()
}

/// One pass over the codex rollouts. Returns how many ledger rows were written.
pub async fn index_once(store: &SharedStore, home: &Path) -> anyhow::Result<usize> {
    index_once_at(
        store,
        home,
        &codex_sessions_dir(),
        &workspace_workdirs(home, crate::api::session_verbs::all_session_workdirs()),
    )
    .await
}

/// The pass with its roots injected, so a test drives a temp tree and a fixed
/// lane map instead of this machine's real ones.
pub async fn index_once_at(
    store: &SharedStore,
    home: &Path,
    sessions: &Path,
    workdirs: &BTreeMap<String, String>,
) -> anyhow::Result<usize> {
    use super::token_ledger::{self, LedgerFileBatch, LedgerRow};
    if !sessions.is_dir() {
        return Ok(0);
    }
    let table = token_ledger::prices(home);
    let owners = unambiguous_owners(workdirs);
    let cursors = token_ledger::read_cursors(store)?;

    let mut batches: Vec<LedgerFileBatch> = Vec::new();
    let mut repairs = Vec::new();
    for path in rollout_files(sessions) {
        let conversation = conversation_key(&path);
        let Ok(meta) = path.metadata() else { continue };
        let size = meta.len();
        let offset = cursors.get(&conversation).copied().unwrap_or(0);
        let lane = rollout_cwd(&path)
            .and_then(|cwd| owners.get(&cwd).cloned())
            .unwrap_or_default();
        if !lane.is_empty() {
            repairs.push((conversation.clone(), lane.clone()));
        }
        if offset == size {
            continue;
        }
        let offset = if offset > size { 0 } else { offset };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let (new_off, turns) = parse_codex_from(&path, offset);
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
                    message_id: Some(format!("ord:{}", t.ordinal)),
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

    token_ledger::warn_unpriced("codex", &table, &batches);
    let inserted = token_ledger::commit_ledger_batch(store, batches).await?;
    let repairs = {
        let c = store.read()?;
        ownership_repairs(&c, &repairs)?
    };
    let repaired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = repaired.clone();
    if !repairs.is_empty() {
        store
            .write_async(move |c| {
                let mut n = 0;
                for (id, lane) in repairs {
                    n += c.execute(
                        "UPDATE token_ledger SET session=?2 WHERE id=?1 AND session='' AND task=''",
                        rusqlite::params![id, lane],
                    )?;
                }
                count.store(n, std::sync::atomic::Ordering::Relaxed);
                if n > 0 {
                    tracing::info!(
                        measured = true,
                        n_considered = n,
                        verdict = "codex_workspace_usage_recovered",
                        "unowned exact-match rollout rows assigned from durable workspace identity"
                    );
                }
                Ok(crate::db::WriteOutcome {
                    applied: n > 0,
                    events: vec![],
                })
            })
            .await?;
    }
    if inserted > 0 || repaired.load(std::sync::atomic::Ordering::Relaxed) > 0 {
        token_ledger::attribute_tasks(store).await?;
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let st = crate::db::Store::open(&dir.path().join("codex-ledger-test.db")).unwrap();
        std::mem::forget(dir);
        std::sync::Arc::new(st)
    }

    /// One `token_count` event in the shape codex actually writes (copied from a
    /// live rollout on this box, trimmed).
    fn token_count(
        ordinal: i64,
        ts: &str,
        input: i64,
        cached: i64,
        cache_write: i64,
        output: i64,
    ) -> String {
        format!(
            r#"{{"timestamp":"{ts}","ordinal":{ordinal},"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":999999,"cached_input_tokens":0,"output_tokens":999}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":{cache_write},"output_tokens":{output},"reasoning_output_tokens":{reasoning},"total_tokens":{total}}},"model_context_window":258400}}}}}}"#,
            reasoning = output / 3,
            total = input + output,
        )
    }

    fn session_meta(cwd: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-09-15T10:00:00.000Z","ordinal":0,"type":"session_meta","payload":{{"session_id":"019a4580-abd6-7153-993c-217e6941ea1d","cwd":"{cwd}","model_provider":"openai"}}}}"#
        )
    }

    fn rollout(dir: &Path, cwd: &str, body: &str) -> PathBuf {
        let day = dir.join("2026/09/15");
        std::fs::create_dir_all(&day).unwrap();
        let p = day.join("rollout-2026-09-15T10-00-00-019a4580-abd6-7153-993c-217e6941ea1d.jsonl");
        std::fs::write(&p, format!("{}\n{}", session_meta(cwd), body)).unwrap();
        p
    }

    /// The mapping this whole file rests on: codex reports the input total WITH
    /// the cached part inside it, and the reasoning tokens inside the output.
    /// Getting either wrong inflates a lane's bill with tokens nobody spent.
    #[test]
    fn a_turn_splits_cached_input_out_and_never_counts_reasoning_twice() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "{}\n{}\n{}\n",
            r#"{"timestamp":"2026-09-15T10:00:01.000Z","ordinal":1,"type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
            token_count(2, "2026-09-15T10:00:02.000Z", 58880, 57344, 0, 390),
            token_count(3, "2026-09-15T10:00:03.000Z", 0, 0, 0, 0),
        );
        let p = rollout(dir.path(), "/Users/ethan/Dev/amux", &body);

        let (off, turns) = parse_codex_from(&p, 0);
        assert_eq!(
            turns.len(),
            1,
            "the zero-token event is not a ledger row: {turns:?}"
        );
        let t = &turns[0];
        assert_eq!(t.tokens, [1536, 57344, 0, 390],
            "fresh input is input_tokens minus the cached part, and output is not summed with reasoning");
        assert_eq!(
            t.model, "gpt-5-codex",
            "the model comes from the turn context, not session_meta"
        );
        assert_eq!(t.ordinal, 2);
        assert_eq!(t.ts, 1789466402);
        assert_eq!(
            off,
            std::fs::metadata(&p).unwrap().len(),
            "the cursor lands on the end of the file"
        );

        // Resuming from that cursor finds nothing: the same turn cannot be
        // billed twice by a second pass.
        let (off2, again) = parse_codex_from(&p, off);
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(off2, off);
    }

    /// A directory two lanes share attributes to NEITHER: the ledger is what
    /// the cost view bills, and a wrong owner is worse than an unowned row.
    #[test]
    fn a_shared_working_directory_is_left_unattributed() {
        let mut dirs = BTreeMap::new();
        dirs.insert("alpha".to_string(), "/Users/ethan/Dev/amux".to_string());
        dirs.insert("beta".to_string(), "/Users/ethan/Dev/amux/".to_string()); // same dir, trailing slash
        dirs.insert("solo".to_string(), "/Users/ethan/Dev/solo".to_string());
        dirs.insert("nowhere".to_string(), String::new());
        let owners = unambiguous_owners(&dirs);
        assert_eq!(
            owners.get("/Users/ethan/Dev/solo"),
            Some(&"solo".to_string())
        );
        assert!(
            !owners.contains_key("/Users/ethan/Dev/amux"),
            "two lanes, no owner: {owners:?}"
        );
        assert!(!owners.contains_key(""), "an empty workdir owns nothing");
    }

    /// End to end: a rollout becomes ledger rows charged to the lane that works
    /// in that directory, and a second pass adds nothing.
    #[tokio::test]
    async fn a_rollout_becomes_ledger_rows_for_the_lane_that_works_there() {
        let home = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let body = format!(
            "{}\n{}\n{}\n",
            r#"{"timestamp":"2026-09-15T10:00:01.000Z","ordinal":1,"type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
            token_count(2, "2026-09-15T10:00:02.000Z", 1000, 400, 0, 50),
            token_count(4, "2026-09-15T10:00:04.000Z", 2000, 0, 0, 70),
        );
        rollout(sessions.path(), "/Users/ethan/Dev/amux", &body);
        let mut workdirs = BTreeMap::new();
        workdirs.insert(
            "amux-research".to_string(),
            "/Users/ethan/Dev/amux".to_string(),
        );

        let st = store();
        let n = index_once_at(&st, home.path(), sessions.path(), &workdirs)
            .await
            .unwrap();
        assert_eq!(n, 2);
        {
            let conn = st.read().unwrap();
            let rows: Vec<(String, String, String, i64, i64, i64, String)> = conn
                .prepare(
                    "SELECT session, conversation, model, input, cache_read, output, message_id \
                          FROM token_ledger ORDER BY ts",
                )
                .unwrap()
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .unwrap()
                .flatten()
                .collect();
            assert_eq!(rows.len(), 2, "{rows:?}");
            assert_eq!(
                rows[0].0, "amux-research",
                "charged to the lane working in that cwd"
            );
            assert!(
                rows[0].1.starts_with("codex:"),
                "the conversation says which provider it came from: {}",
                rows[0].1
            );
            assert_eq!(rows[0].2, "gpt-5-codex");
            assert_eq!((rows[0].3, rows[0].4, rows[0].5), (600, 400, 50));
            assert_eq!(rows[0].6, "ord:2");
            assert_eq!(rows[1].6, "ord:4");
        }

        // Idempotent: the cursor holds, and re-running bills nothing again.
        let again = index_once_at(&st, home.path(), sessions.path(), &workdirs)
            .await
            .unwrap();
        assert_eq!(again, 0, "a second pass must not re-bill the same turns");
        let conn = st.read().unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM token_ledger", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    /// A rollout in a directory no lane owns still counts toward fleet totals,
    /// unowned. Dropping it would recreate the invisible-spend hole this closes.
    #[tokio::test]
    async fn an_unknown_working_directory_still_reaches_the_ledger_unowned() {
        let home = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        rollout(
            sessions.path(),
            "/somewhere/nobody/owns",
            &format!(
                "{}\n",
                token_count(2, "2026-09-15T10:00:02.000Z", 500, 0, 0, 10)
            ),
        );
        let st = store();
        let n = index_once_at(&st, home.path(), sessions.path(), &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(n, 1);
        let conn = st.read().unwrap();
        let (session, input): (String, i64) = conn
            .query_row("SELECT session, input FROM token_ledger", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(session, "", "unowned, not dropped and not guessed");
        assert_eq!(input, 500);
    }
    #[tokio::test]
    async fn project_workspace_owners_survive_retirement_and_recover_exact_unowned_rows() {
        let home = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let mut parent_dirs = BTreeMap::new();
        for (name, suffix) in [("active", "env"), ("retired", "env.reaped")] {
            let w = crate::fanout_workspace::Workspace {
                repo: "/parent/clone".into(),
                path: home
                    .path()
                    .join("worktrees")
                    .join(name)
                    .to_string_lossy()
                    .into(),
                branch: format!("amux/fanout/{name}"),
                base: "a".repeat(40),
            };
            crate::fanout_workspace::save(home.path(), name, &w).unwrap();
            std::fs::write(
                home.path()
                    .join("sessions")
                    .join(format!("{name}.{suffix}")),
                "CC_DIR=/parent/clone\n",
            )
            .unwrap();
            parent_dirs.insert(name.into(), w.repo);
        }
        let dirs = workspace_workdirs(home.path(), parent_dirs);
        assert_eq!(dirs.len(), 2);
        assert_ne!(dirs["active"], dirs["retired"]);
        assert!(!unambiguous_owners(&dirs).contains_key("/parent/clone"));
        let st = store();
        rollout(
            sessions.path(),
            &dirs["retired"],
            &format!(
                "{}\n",
                token_count(2, "2026-09-15T10:00:02.000Z", 500, 0, 0, 10)
            ),
        );
        assert_eq!(
            index_once_at(&st, home.path(), sessions.path(), &BTreeMap::new())
                .await
                .unwrap(),
            1
        );
        let mut ambiguous = dirs.clone();
        ambiguous.insert("other".into(), dirs["retired"].clone());
        index_once_at(&st, home.path(), sessions.path(), &ambiguous)
            .await
            .unwrap();
        assert_eq!(
            st.read()
                .unwrap()
                .query_row("SELECT session FROM token_ledger", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            ""
        );
        index_once_at(&st, home.path(), sessions.path(), &dirs)
            .await
            .unwrap();
        index_once_at(&st, home.path(), sessions.path(), &dirs)
            .await
            .unwrap();
        let c = st.read().unwrap();
        let row: (String, String, i64) = c
            .query_row("SELECT session,task,count(*) FROM token_ledger", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(
            row,
            ("retired".into(), "".into(), 1),
            "ownership repair neither double bills nor invents a claim window"
        );
        std::fs::write(home.path().join("sessions/active.env"), "CC_DIR=/foreign\n").unwrap();
        assert!(!workspace_workdirs(home.path(), BTreeMap::new()).contains_key("active"));
    }

    #[tokio::test]
    async fn codex_ownership_repair_skips_unchanged_rows_and_admits_new_owners() {
        let st = store();
        st.write(|c| {
            for (conversation, session) in [("one", ""), ("two", ""), ("foreign", "fixed")] {
                c.execute(
                    "INSERT INTO token_ledger(ts,session,conversation,input) VALUES(1,?1,?2,10)",
                    rusqlite::params![session, conversation],
                )?;
            }
            let first = vec![
                ("one".into(), "owner".into()),
                ("foreign".into(), "wrong".into()),
            ];
            let rows = ownership_repairs(c, &first)?;
            assert_eq!(rows.len(), 1);
            for (id, owner) in rows {
                c.execute(
                    "UPDATE token_ledger SET session=?2 WHERE id=?1",
                    rusqlite::params![id, owner],
                )?;
            }
            assert!(
                ownership_repairs(c, &first)?.is_empty(),
                "unchanged pass does not enter writer"
            );
            let later = vec![
                ("one".into(), "owner".into()),
                ("two".into(), "retired-owner".into()),
            ];
            let rows = ownership_repairs(c, &later)?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1, "retired-owner");
            assert!(
                ownership_repairs(
                    c,
                    &[
                        ("two".into(), "first".into()),
                        ("two".into(), "second".into())
                    ]
                )?
                .is_empty(),
                "ambiguous conversation ownership refused"
            );
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
    }
}
