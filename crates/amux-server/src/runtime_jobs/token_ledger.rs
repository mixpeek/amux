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
use serde_json::Value;
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
    // LOCAL INFERENCE IS A KNOWN RATE THAT HAPPENS TO BE ZERO, not a missing
    // one (AMUX-4806). An ollama model runs on this Mac and produces no vendor
    // bill, so $0 is a measured fact and belongs IN the table. Being in the
    // table is also what stops it reading as "rate unknown": the two states
    // are different claims and the ledger now keeps them apart.
    //
    // Before this, `qwen3.8:27b` carried $223.32 of Anthropic Sonnet pricing
    // for 551 turns that cost nothing at all.
    //
    // Substring match, so a tag like `qwen3-coder:30b-65k` is covered without
    // an entry per tag. A local family absent from this list is UNMETERED,
    // which is the safe direction: it withholds a number rather than inventing
    // a zero, and ~/.amux/prices.json can add it with no redeploy.
    ("qwen", [0.0, 0.0, 0.0, 0.0]),
    ("llama", [0.0, 0.0, 0.0, 0.0]),
    ("mistral", [0.0, 0.0, 0.0, 0.0]),
    ("gemma", [0.0, 0.0, 0.0, 0.0]),
    ("deepseek", [0.0, 0.0, 0.0, 0.0]),
    ("phi", [0.0, 0.0, 0.0, 0.0]),
];
const PRICE_DEFAULT: [f64; 4] = [3.0, 0.30, 3.75, 15.0];

/// py:18114 — `<synthetic>` turns carry usage that was never billed.
fn model_is_skipped(model: &str) -> bool {
    model.is_empty() || model == "<synthetic>"
}

pub(crate) fn prices(home: &Path) -> Vec<(String, [f64; 4])> {
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
    let Some(obj) = user.as_object() else {
        return table;
    };
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

/// The rate for a model, or `None` when nobody has told us one.
///
/// AMUX-4806. This used to fall back to `PRICE_DEFAULT` for EVERY unknown
/// model, and `PRICE_DEFAULT` is Anthropic Sonnet's rate. The result landed in
/// the same column as measured Claude spend: 116,966 rows across gpt-*,
/// gemini-* and a local qwen carried $6,942.14 that nobody spent.
///
/// THE FALLBACK IS KEPT, BUT SCOPED TO THE VENDOR IT BELONGS TO. AMUX-4583
/// defended it for a good reason -- "that is right for a new Claude model" --
/// and a brand-new `claude-*` really is better approximated by Sonnet's rate
/// than withheld. That reasoning does not reach a different vendor's model, so
/// the fallback now applies only where it was ever true. Everything else is
/// UNMETERED: the tokens are kept, the number is withheld.
///
/// An explicit `_default` in ~/.amux/prices.json still wins for everything,
/// because that is the owner saying "price the unknowns like this", which is a
/// decision they are entitled to make.
fn price_for_model(table: &[(String, [f64; 4])], model: &str) -> Option<[f64; 4]> {
    let m = model.to_lowercase();
    for (key, rates) in table {
        if key != "_default" && m.contains(key.as_str()) {
            return Some(*rates);
        }
    }
    if let Some((_, r)) = table.iter().find(|(k, _)| k == "_default") {
        return Some(*r);
    }
    // Same vendor, unlisted model: approximate rather than withhold.
    m.contains("claude").then_some(PRICE_DEFAULT)
}

/// Does the price table actually name this model?
///
/// AMUX-4583: `price_for_model` falls back to PRICE_DEFAULT on purpose, because
/// a zero reads as a free turn. That is right for a new Claude model and wrong
/// for a different vendor: applying Claude rates to codex turns put $5,654 of
/// cost in the ledger that nobody spent. The fallback stays; the COUNT of rows
/// that took it is now reportable, so a guessed dollar figure cannot pass as a
/// measured one.
pub(crate) fn model_is_priced(table: &[(String, [f64; 4])], model: &str) -> bool {
    price_for_model(table, model).is_some()
}

/// The rate itself, for a caller that must tell a zero RATE from a missing one
/// (AMUX-4806). `model_is_priced` collapses that distinction, and the cost
/// surface needs it: a local model priced at a real zero still carries legacy
/// dollars in the table from before this existed.
pub(crate) fn model_rate(table: &[(String, [f64; 4])], model: &str) -> Option<[f64; 4]> {
    price_for_model(table, model)
}

/// Cost for a turn. ZERO WHEN NO RATE IS KNOWN, and `model_is_priced` is what
/// tells a reader which kind of zero it is: a genuinely free local turn, or a
/// number we declined to invent (AMUX-4806).
pub(crate) fn turn_cost_usd(table: &[(String, [f64; 4])], model: &str, t: [i64; 4]) -> f64 {
    let Some(p) = price_for_model(table, model) else {
        return 0.0;
    };
    (t[0] as f64 * p[0] + t[1] as f64 * p[1] + t[2] as f64 * p[2] + t[3] as f64 * p[3])
        / 1_000_000.0
}

/// A single row destined for the `token_ledger` table. Shared by all three
/// provider parsers so the INSERT SQL lives in one place.
pub(crate) struct LedgerRow {
    pub ts: i64,
    pub session: String,
    pub conversation: String,
    pub model: String,
    pub tokens: [i64; 4],
    pub cost: f64,
    pub message_id: Option<String>,
}

/// One conversation's parsed turns plus the cursor state to commit.
pub(crate) struct LedgerFileBatch {
    pub conversation: String,
    pub offset: u64,
    pub mtime: i64,
    pub rows: Vec<LedgerRow>,
}

/// Read all ledger cursors. Both codex and gemini duplicated this query.
pub(crate) fn read_cursors(store: &SharedStore) -> anyhow::Result<HashMap<String, u64>> {
    let conn = store.read()?;
    let mut stmt = conn.prepare("SELECT conversation, offset FROM ledger_cursor")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
    })?;
    Ok(rows.flatten().collect())
}

/// Warn about rows that took the default price because no table entry names
/// their model. Duplicated in codex and gemini before this was extracted.
pub(crate) fn warn_unpriced(
    provider: &str,
    table: &[(String, [f64; 4])],
    batches: &[LedgerFileBatch],
) {
    let models: std::collections::BTreeSet<String> = batches
        .iter()
        .flat_map(|b| b.rows.iter())
        .filter(|r| !model_is_priced(table, &r.model))
        .map(|r| r.model.clone())
        .collect();
    let guessed: usize = batches
        .iter()
        .flat_map(|b| b.rows.iter())
        .filter(|r| !model_is_priced(table, &r.model))
        .count();
    let expected: usize = batches.iter().map(|b| b.rows.len()).sum();
    if guessed > 0 {
        tracing::warn!(
            verdict = %format!("{provider}_rows_priced_by_default"),
            rows = guessed,
            models = %models.into_iter().collect::<Vec<_>>().join(","),
            measured = true,
            n_considered = expected,
            "{provider} turns carry the DEFAULT price: their token counts are measured, their cost is not. \
             Set real rates for these models in ~/.amux/prices.json (config, no redeploy)."
        );
    }
}

/// Commit parsed turns to the ledger and update cursors. Returns rows inserted.
///
/// This is the shared write path for codex and gemini. Claude's own
/// `index_once_at` keeps its richer variant (request_id column, per-pass cap,
/// task attribution) rather than forcing those into a generic interface.
pub(crate) async fn commit_ledger_batch(
    store: &SharedStore,
    batches: Vec<LedgerFileBatch>,
) -> anyhow::Result<usize> {
    if batches.is_empty() {
        return Ok(0);
    }
    let expected: usize = batches.iter().map(|b| b.rows.len()).sum();
    store
        .write_async(move |conn| {
            let mut n = 0usize;
            {
                let mut ins = conn.prepare(
                    "INSERT INTO token_ledger
                       (ts, session, conversation, model, input, cache_read, cache_write, output, cost_usd, message_id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                     ON CONFLICT(conversation, message_id) WHERE message_id IS NOT NULL
                     DO UPDATE SET output = MAX(output, excluded.output),
                                   cost_usd = MAX(cost_usd, excluded.cost_usd)",
                )?;
                let mut cur = conn.prepare(
                    "INSERT INTO ledger_cursor (conversation, offset, mtime) VALUES (?1,?2,?3)
                     ON CONFLICT(conversation) DO UPDATE SET offset=?2, mtime=?3",
                )?;
                for b in &batches {
                    for r in &b.rows {
                        ins.execute(rusqlite::params![
                            r.ts, r.session, r.conversation, r.model,
                            r.tokens[0], r.tokens[1], r.tokens[2], r.tokens[3],
                            r.cost, r.message_id
                        ])?;
                        n += 1;
                    }
                    cur.execute(rusqlite::params![b.conversation, b.offset as i64, b.mtime])?;
                }
            }
            Ok(WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await
        .map(|_| expected)
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
    /// `message.id` of the API response this usage belongs to (AMUX-4580).
    /// The key that makes billing exact; None only for lines that carry no id.
    message_id: Option<String>,
    request_id: Option<String>,
}

/// Usage lines dropped because their `message.id` was already billed in the
/// same pass, process-lifetime (AMUX-4580). Published beside the insert count so
/// a subagent-heavy day shows how much double billing the key prevented.
pub static LEDGER_DUPLICATE_MESSAGES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Parse one JSONL from `offset` onward. Returns the new offset and the turns.
///
/// The dedup is load-bearing and easy to get subtly wrong: Claude Code emits
/// the SAME usage block again for a turn's thinking and tool_use parts, so
/// counting every `usage` line double- or triple-bills a turn. Python keyed on
/// `(input, cache_read, output)` and compared only against the IMMEDIATELY
/// preceding line (py:18133) — deliberately not a set, because two genuinely
/// distinct turns can carry identical counts and a set would silently drop the
/// second.
fn parse_from(
    path: &Path,
    offset: u64,
    fallback_ts: i64,
    owner: &str,
    table: &[(String, [f64; 4])],
) -> (u64, Vec<Turn>) {
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
    // AMUX-4580: responses already billed in this pass, by message id.
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
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
        // KEYED BY THE RESPONSE WHEN THE TRANSCRIPT NAMES IT (AMUX-4580). One
        // API response's usage is repeated for its thinking, text and tool_use
        // parts, and in subagent transcripts not always on adjacent lines, so
        // the adjacent signature below missed the repeats and billed one
        // response several times. The id cannot merge two real turns: they have
        // different ids even when their counts match. Lines without an id keep
        // the adjacent-signature rule, which is still right for them.
        let message_id = msg["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(id) = message_id.as_deref() {
            if !seen_ids.insert(id.to_string()) {
                LEDGER_DUPLICATE_MESSAGES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
        } else {
            let sig = (tokens[0], tokens[1], tokens[3]);
            if prev_sig == Some(sig) {
                continue;
            }
            prev_sig = Some(sig);
        }
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
            message_id,
            request_id: e["requestId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
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

/// A helper hook once borrowed tmux's selected lane and charged Claude usage
/// to a Codex project. Recover only rows inside a proven Codex launch whose
/// transcript belongs outside that project's repo. Preserve every token and
/// older provider history; uncertain ownership stays in fleet totals.
async fn repair_foreign_project_owners(
    store: &SharedStore,
    home: &Path,
    projects: &Path,
) -> anyhow::Result<()> {
    let Ok(entries) = std::fs::read_dir(home.join("status-events")) else {
        return Ok(());
    };
    let mut repairs = Vec::new();
    for entry in entries.flatten() {
        let worker = entry.file_name().to_string_lossy().to_string();
        let launch: Value = std::fs::read(entry.path().join("current.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        if launch["provider"] != "codex" {
            continue;
        }
        let Some(started) = launch["started"].as_f64() else {
            continue;
        };
        let settings =
            crate::config::parse_env_file(&home.join("sessions").join(format!("{worker}.env")));
        if settings.get("CC_PROJECT").is_none_or(String::is_empty) {
            continue;
        }
        let Some(repo) = settings
            .get("CC_DIR")
            .filter(|s| Path::new(s).is_absolute())
        else {
            continue;
        };
        let c = store.read()?;
        let proven: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM session_events WHERE session=?1 AND type='session.native_status' AND json_extract(data,'$.provider')='codex' AND json_extract(data,'$.run_id')=?2)", rusqlite::params![worker,launch["run_id"].as_str().unwrap_or("")], |r|r.get(0))?;
        if !proven {
            continue;
        }
        // A newly allocated worker is claimed before its first CLI is launched.
        // False helper reports can arrive in that startup gap. Prior launches
        // retain their history; only the first launch can include this gap.
        let previous_launch: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM session_events WHERE session=?1 AND type='session.started' AND ts<?2)", rusqlite::params![worker,started], |r|r.get(0))?;
        let started = if previous_launch {
            started
        } else {
            c.query_row("SELECT coalesce(min(ts),?2) FROM session_events WHERE session=?1 AND type='project.claimed' AND ts<=?2 AND json_extract(data,'$.project_group')=?3", rusqlite::params![worker,started,settings["CC_PROJECT"]], |r|r.get::<_,f64>(0))?
        };
        let mut q = c.prepare("SELECT id,conversation FROM token_ledger INDEXED BY idx_ledger_session WHERE session=?1 AND ts>=?2 AND model LIKE 'claude-%' ORDER BY id LIMIT 1000")?;
        let rows = q
            .query_map(rusqlite::params![worker, started], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut foreign = HashMap::new();
        for (id, conversation) in rows {
            let outside = *foreign.entry(conversation.clone()).or_insert_with(|| {
                if conversation.contains('/') || conversation.contains('\\') {
                    return false;
                }
                let Ok(dirs) = std::fs::read_dir(projects) else {
                    return false;
                };
                let mut cwds = Vec::new();
                for dir in dirs.flatten() {
                    let mut paths = vec![dir.path().join(format!("{conversation}.jsonl"))];
                    // The indexer charges delegated transcripts to their parent.
                    // Repair must use that same parent identity, not miss children.
                    if conversation.starts_with("agent-") {
                        if let Ok(parents) = std::fs::read_dir(dir.path()) {
                            for parent in parents.flatten() {
                                if parent
                                    .path()
                                    .join("subagents")
                                    .join(format!("{conversation}.jsonl"))
                                    .is_file()
                                {
                                    paths.push(parent.path().with_extension("jsonl"));
                                }
                            }
                        }
                    }
                    for path in paths {
                        let Ok(f) = std::fs::File::open(path) else {
                            continue;
                        };
                        for line in BufReader::new(f).lines().take(32).flatten() {
                            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            if let Some(cwd) =
                                v["cwd"].as_str().filter(|s| Path::new(s).is_absolute())
                            {
                                cwds.push(PathBuf::from(cwd));
                                break;
                            }
                        }
                    }
                }
                cwds.len() == 1 && !cwds[0].starts_with(repo)
            });
            if outside {
                repairs.push((id, worker.clone()));
            }
        }
    }
    if repairs.is_empty() {
        return Ok(());
    }
    store.write_async(move |c| {
        let mut n = 0;
        for (id, worker) in repairs {
            let prior: (String, String) = c.query_row("SELECT task,conversation FROM token_ledger WHERE id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?)))?;
            let changed = c.execute("UPDATE token_ledger SET session='',task='' WHERE id=?1 AND session=?2", rusqlite::params![id,worker])?;
            if changed > 0 {
                c.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'usage.ownership_repaired',?3,'token-ledger')", rusqlite::params![crate::config::now_f64(),worker,serde_json::json!({"ledger_row":id,"previous_task":prior.0,"conversation":prior.1,"reason":"foreign_provider_transcript"}).to_string()])?;
            }
            n += changed;
        }
        tracing::warn!(verdict="foreign_provider_usage_unbound", rows=n, "retained foreign Claude usage in fleet totals without charging a Codex project");
        Ok(crate::db::WriteOutcome { applied: n>0, events: vec![] })
    }).await?;
    Ok(())
}

/// One indexing pass. Returns rows inserted. Cheap on a fully-indexed tree —
/// a stat per file and nothing else.
pub async fn index_once(store: &SharedStore, home: &Path) -> anyhow::Result<usize> {
    repair_foreign_project_owners(store, home, &claude_projects_dir()).await?;
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
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)?),
            ))
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
        let Ok(files) = std::fs::read_dir(&p) else {
            continue;
        };
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
                let conv = jf
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
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
            indexed = pending.len(),
            deferred = skipped_for_cap,
            cap,
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
                    // A response already billed by an EARLIER pass (the cursor
                    // resumed mid-message, or a repeat landed after a flush)
                    // updates its row to the larger output instead of adding one.
                    "INSERT INTO token_ledger
                       (ts, session, conversation, model, input, cache_read, cache_write, output, cost_usd,
                        message_id, request_id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                     ON CONFLICT(conversation, message_id) WHERE message_id IS NOT NULL
                     DO UPDATE SET output = MAX(output, excluded.output),
                                   cost_usd = MAX(cost_usd, excluded.cost_usd)",
                )?;
                let mut cur = conn.prepare(
                    "INSERT INTO ledger_cursor (conversation, offset, mtime) VALUES (?1,?2,?3)
                     ON CONFLICT(conversation) DO UPDATE SET offset=?2, mtime=?3",
                )?;
                for (conv, off, mtime, turns) in &pending {
                    for t in turns {
                        ins.execute(rusqlite::params![
                            t.ts, t.session, t.conversation, t.model,
                            t.tokens[0], t.tokens[1], t.tokens[2], t.tokens[3], t.cost,
                            t.message_id, t.request_id
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

/// A claim window may not run longer than this, however it ended.
///
/// The open-ended window is what made the old attribution wrong, so the
/// replacement refuses to express one. `task_attempts` closes an abandoned
/// attempt through the lease reaper and `reconcile_orphans`, but "the writer
/// will close it" is the same assumption `task_windows` was built on, and it
/// held until the writer went away. A genuinely long claim loses attribution
/// for its tail, which is a bounded error; an unclosed one silently absorbs
/// every later turn on that lane, which is not.
const MAX_CLAIM_WINDOW_S: i64 = 24 * 3600;

/// How long one writer acquisition may spend applying claim windows (AMUX-4750).
///
/// A TIME BUDGET, after counting failed twice. The chunk was sized by dividing
/// a total by an item count: 500 windows was "~200ms" at an implied 0.4ms per
/// UPDATE, and measured 738ms. Resized to 150 on the corrected 1.5ms, it
/// measured 696ms, implying ~4.6ms. The per-window cost is not a constant to
/// divide by — most windows match nothing and return instantly while a few scan
/// a real range — so no fixed count bounds the worst case. Elapsed time does,
/// directly, and it is what the card actually asks about.
///
/// The hold is this budget plus at most one UPDATE, because the check happens
/// after each one rather than before. That overshoot is real and measured: at a
/// 200ms budget the worst hold was 292ms, so a single UPDATE can cost ~90ms.
/// 120 + ~90 is ~210ms, inside the 250ms this card asks for, at roughly 33
/// acquisitions for a full pass.
const HOLD_BUDGET_MS: u128 = 120;

/// Ledger rows one UPDATE may attribute (AMUX-4750).
///
/// THE TIME BUDGET CANNOT BOUND A HOLD BELOW ITS SLOWEST SINGLE STATEMENT, and
/// at a 120ms budget the worst hold measured 563ms, so one UPDATE took ~440ms.
/// That one is not waste: a 24-hour window over a busy lane genuinely matches
/// thousands of rows, and each is a write plus an index update.
///
/// So the statement gets a cap too. A window with more rows than this attributes
/// the rest on a later cycle, which is safe here for a reason specific to this
/// job: it re-derives every window from scratch every 120 seconds and only ever
/// fills rows still unattributed, so an interrupted window is resumed rather
/// than lost. A job without that property would need a cursor instead.
const ROWS_PER_UPDATE: i64 = 2_000;

/// Fill `token_ledger.task` for turns that fall inside a card's claim window on
/// the SAME lane. Only touches unattributed rows.
///
/// WHY NOT `task_windows` (AMUX-4581). It has had no writer since the Python
/// cutover: 484 rows, newest `entered_doing` 2026-08-09, measured 37 days
/// stale. Four of those windows never closed, and the old query read
/// `COALESCE(left_doing, now)`, so each one stretched from last July to the
/// present and swallowed everything after it. Measured over 7 days before this
/// change: AMUX-1808 took $2,680 of ownerless conversations and AMUX-2598 took
/// $1,618 of every `amux` turn. The Cost tab's by-task bars were reporting
/// which stale row won a race, not where the money went.
///
/// TWO causes, both fixed here, because swapping the table alone would have
/// rebuilt the same trap on fresher data:
///
/// 1. THE UNBOUNDED WINDOW. `task_attempts` is the live record (written at both
///    lease choke points), but an attempt that never closes would extend to now
///    exactly as those four windows did. Capped at `MAX_CLAIM_WINDOW_S`.
///
/// 2. THE EMPTY SESSION MATCHING ITSELF. Three of the four absorbing windows
///    had `session = ''`, and the old `WHERE session=?2` matched them against
///    ledger rows whose session is also `''` — every ownerless conversation on
///    the box. An empty lane is not an identity, so a blank on EITHER side now
///    attributes nothing. This is the clause that actually drained AMUX-1808,
///    and it is independent of which table the windows come from.
///
/// `task.claimed` covers the era before attempts existed: it is the only record
/// of a claim for ledger rows older than the first attempt row. Its interval
/// runs to the lane's next claim, capped the same way, so it cannot become an
/// open window either.
/// Claim windows, derived from a read-only snapshot (AMUX-4750).
///
/// PURE, AND OUTSIDE THE WRITER. This whole derivation used to run inside
/// `write_async`, so the single writer was held for two full-table scans, a
/// JSON parse per claim and an O(n^2) scan, every 120 seconds. Measured on the
/// live DB: 9,294 claims, which is ~43M comparisons of pure CPU while every
/// non-GET request in the fleet waits behind `record_receipt`.
fn claim_windows(
    attempts: Vec<(String, String, i64, i64)>,
    claims: Vec<(String, String, i64)>,
    now: i64,
) -> Vec<(String, String, i64, i64)> {
    let mut wins = attempts;
    for (i, (session, data, ts)) in claims.iter().enumerate() {
        let Some(card) = serde_json::from_str::<serde_json::Value>(data)
            .ok()
            .and_then(|v| v.get("issue").and_then(|c| c.as_str()).map(str::to_string))
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        // The next claim BY THE SAME LANE ends this one. A later claim by
        // another lane says nothing about when this one stopped.
        //
        // O(1), NOT A SCAN. The query orders by (session, ts), so a lane's
        // claims are already contiguous and ascending: the next claim by this
        // lane is the very next row, or there is none. The `.find()` this
        // replaces walked the whole remaining tail per claim and looked
        // perfectly correct doing it, which is why it survived.
        let next = claims
            .get(i + 1)
            .filter(|(s, _, _)| s == session)
            .map(|(_, _, t)| *t)
            .unwrap_or(now);
        wins.push((
            card,
            session.clone(),
            *ts,
            next.min(ts + MAX_CLAIM_WINDOW_S),
        ));
    }
    // Oldest first, so the earliest claim covering a turn wins it; the UPDATE
    // only touches rows still unattributed.
    wins.sort_by_key(|(_, _, from, _)| *from);
    wins
}

pub async fn attribute_tasks(store: &SharedStore) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    // READ PHASE, off the writer. The snapshot can be a moment stale relative
    // to the write below and it does not matter: a claim recorded in that gap
    // is picked up on the next cycle, and the UPDATE only ever fills rows that
    // are still unattributed, so nothing is overwritten by arriving late.
    let (wins, bounds) = store
        .read_async(move |conn| {
            let mut attempts: Vec<(String, String, i64, i64)> = Vec::new();
            // The live source. `min(a, b)` is SQLite's two-argument scalar min.
            if let Ok(mut stmt) = conn.prepare(
                "SELECT card, worker, started_at, \
                        min(COALESCE(ended_at, ?1), started_at + ?2) \
                 FROM task_attempts \
                 WHERE COALESCE(worker,'') <> '' AND COALESCE(card,'') <> '' \
                 ORDER BY started_at",
            ) {
                if let Ok(rows) = stmt.query_map([now, MAX_CLAIM_WINDOW_S], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                }) {
                    attempts.extend(rows.flatten());
                }
            }

            attempts.extend(crate::project_execution::outputs::usage_windows(conn,now,MAX_CLAIM_WINDOW_S)?);

            // The historical source: one interval per claim, ending at that
            // lane's next claim. `data` carries {"issue": "<id>", ...}.
            let claims: Vec<(String, String, i64)> = {
                let mut stmt = conn.prepare(
                    "SELECT session, data, CAST(ts AS INTEGER) FROM session_events \
                     WHERE type='task.claimed' AND COALESCE(session,'') <> '' \
                     AND NOT EXISTS(SELECT 1 FROM issues i WHERE i.project_group IS NOT NULL \
                       AND i.id=CASE WHEN json_valid(session_events.data) THEN json_extract(session_events.data,'$.issue') END) \
                     ORDER BY session, ts",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, i64>(2)?,
                    ))
                })?;
                rows.flatten().collect()
            };
            // The ts range that still has anything to attribute. A window
            // outside it cannot match a row, so issuing its UPDATE is a
            // guaranteed no-op — and in the steady state that is nearly all of
            // them, because this job re-derives the whole of history every
            // cycle and history was attributed on the first one.
            let bounds: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT MIN(ts), MAX(ts) FROM token_ledger WHERE task=''",
                    [],
                    |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
                )
                .ok()
                .and_then(|(lo, hi)| Some((lo?, hi?)));
            Ok((claim_windows(attempts, claims, now), bounds))
        })
        .await?;

    let Some((lo, hi)) = bounds else {
        // Nothing is unattributed. Taking the writer to prove that is the cost
        // this whole job used to pay unconditionally.
        return Ok(());
    };
    let pending: Vec<(String, String, i64, i64)> = wins
        .into_iter()
        .filter(|(task, session, from, to)| {
            !session.trim().is_empty() && !task.trim().is_empty() && to >= from
                // Overlaps the unattributed range, so the UPDATE can match.
                && *from <= hi && *to >= lo
        })
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    // BOUND THE HOLD, not the total work (AMUX-4750). Moving the derivation out
    // left ~10,000 UPDATEs, and at ~0.4ms each that is still a ~4s hold on the
    // single writer, which is a floor under every non-GET request in the fleet.
    // Chunking does not make the job cheaper; it stops any one interactive write
    // being stuck behind all of it. 500 x ~0.4ms is ~200ms, inside the 250ms
    // this card asks for, and the loop yields the writer between chunks.
    let pending = std::sync::Arc::new(pending);
    let mut done = 0usize;
    while done < pending.len() {
        let consumed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (windows, counter, start_at) = (pending.clone(), consumed.clone(), done);
        store
            .write_async(move |conn| {
                let started = std::time::Instant::now();
                // rowid-IN, because a plain UPDATE..LIMIT needs a nonstandard
                // SQLite build flag — the same shape the retention trims use.
                let mut stmt = conn.prepare_cached(
                    "UPDATE token_ledger SET task=?1 WHERE rowid IN ( \
                       SELECT rowid FROM token_ledger \
                        WHERE task='' AND session=?2 AND COALESCE(session,'') <> '' \
                          AND ts>=?3 AND ts<=?4 LIMIT ?5)",
                )?;
                let mut n = 0usize;
                for (task, session, from, to) in &windows[start_at..] {
                    stmt.execute(rusqlite::params![task, session, from, to, ROWS_PER_UPDATE])?;
                    n += 1;
                    if started.elapsed().as_millis() >= HOLD_BUDGET_MS {
                        break;
                    }
                }
                counter.store(n, std::sync::atomic::Ordering::Relaxed);
                Ok(WriteOutcome {
                    applied: n > 0,
                    events: vec![],
                })
            })
            .await?;
        let n = consumed.load(std::sync::atomic::Ordering::Relaxed);
        // A zero would mean the closure applied nothing, which cannot happen
        // while windows remain — but looping on it forever is the failure mode
        // worth refusing outright rather than reasoning about.
        if n == 0 {
            tracing::warn!(
                target: "amux::token_ledger",
                verdict = "attribute_no_progress",
                remaining = pending.len() - done,
                "a writer acquisition applied no claim window; stopping rather than spinning"
            );
            break;
        }
        done += n;
    }
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
    Some(super::spawn_periodic(
        super::registry::ids::TOKEN_LEDGER,
        secs,
        move || {
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
                // AMUX-4583: codex lanes spend through a different file tree, and
                // their turns were in no ledger at all. Same tick, same table,
                // counted separately so "0 codex rows" is readable as a state.
                match super::codex_ledger::index_once(&store, &home).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(rows = n, provider = "codex", "token-ledger indexed"),
                    Err(e) => {
                        tracing::warn!(error = %e, provider = "codex", "token-ledger index failed")
                    }
                }
                // AMUX-4679, the same shape a third time. The gemini ADAPTER
                // declares it reports no usage, which is true of the provider
                // interface and says nothing about the CLI, which writes per-turn
                // counts to ~/.gemini/tmp/<project>/chats. Counted separately for
                // the reason codex is: "0 gemini rows" has to be readable as a
                // state rather than as an absent provider.
                match super::gemini_ledger::index_once(&store, &home).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(rows = n, provider = "gemini", "token-ledger indexed"),
                    Err(e) => {
                        tracing::warn!(error = %e, provider = "gemini", "token-ledger index failed")
                    }
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<(String, [f64; 4])> {
        MODEL_PRICES_DEFAULT
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
    }

    // ---- claim windows (AMUX-4750) --------------------------------------

    fn claim(session: &str, card: &str, ts: i64) -> (String, String, i64) {
        (session.into(), format!(r#"{{"issue":"{card}"}}"#), ts)
    }

    /// The O(n^2) original, kept here as the ORACLE.
    ///
    /// The optimisation is a claim that two implementations agree, so the thing
    /// to test is that claim directly. Asserting only the new function's output
    /// against hand-written expectations would pass just as happily if the
    /// rewrite had changed the semantics and I had written the expectations to
    /// match it.
    fn windows_by_scan(
        attempts: Vec<(String, String, i64, i64)>,
        claims: Vec<(String, String, i64)>,
        now: i64,
    ) -> Vec<(String, String, i64, i64)> {
        let mut wins = attempts;
        for (i, (session, data, ts)) in claims.iter().enumerate() {
            let Some(card) = serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .and_then(|v| v.get("issue").and_then(|c| c.as_str()).map(str::to_string))
                .filter(|c| !c.is_empty())
            else {
                continue;
            };
            let next = claims[i + 1..]
                .iter()
                .find(|(s, _, _)| s == session)
                .map(|(_, _, t)| *t)
                .unwrap_or(now);
            wins.push((
                card,
                session.clone(),
                *ts,
                next.min(ts + MAX_CLAIM_WINDOW_S),
            ));
        }
        wins.sort_by_key(|(_, _, from, _)| *from);
        wins
    }

    #[test]
    fn a_lanes_window_ends_at_that_lanes_next_claim_and_never_a_peers() {
        let now = 10_000;
        // ORDERED BY (session, ts), which is what the query produces and what
        // the O(1) lookup depends on. Two lanes, interleaved by time but
        // contiguous by lane.
        let claims = vec![
            claim("alpha", "A-1", 100),
            claim("alpha", "A-2", 300),
            claim("beta", "B-1", 200),
            claim("beta", "B-2", 400),
        ];
        let got = claim_windows(Vec::new(), claims.clone(), now);
        let by_card = |c: &str| -> (i64, i64) {
            let w = got.iter().find(|(card, _, _, _)| card == c).expect(c);
            (w.2, w.3)
        };
        // alpha's first claim ends at ALPHA's next claim (300), not beta's 200.
        assert_eq!(by_card("A-1"), (100, 300));
        assert_eq!(by_card("B-1"), (200, 400));
        // The last claim of each lane runs to `now`, capped by the window.
        assert_eq!(by_card("A-2"), (300, now));
        assert_eq!(by_card("B-2"), (400, now));
    }

    #[test]
    fn the_window_is_capped_even_when_the_lane_never_claims_again() {
        let now = 10 * MAX_CLAIM_WINDOW_S;
        let got = claim_windows(Vec::new(), vec![claim("alpha", "A-1", 0)], now);
        assert_eq!(
            got,
            vec![("A-1".into(), "alpha".into(), 0, MAX_CLAIM_WINDOW_S)]
        );
    }

    #[test]
    fn a_claim_with_no_issue_in_its_payload_is_skipped() {
        let claims = vec![
            ("alpha".into(), "{}".into(), 100),
            ("alpha".into(), r#"{"issue":""}"#.into(), 200),
            ("alpha".into(), "not json at all".into(), 300),
            claim("alpha", "A-1", 400),
        ];
        let got = claim_windows(Vec::new(), claims, 10_000);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "A-1");
    }

    /// THE ONE THAT MATTERS. O(1) next-claim must agree with the O(n^2) scan on
    /// every input the query can actually produce.
    #[test]
    fn the_indexed_lookup_agrees_with_the_scan_it_replaced() {
        let now = 1_000_000;
        // A corpus with the shapes that distinguish the two: lanes of length
        // one, runs of several, adjacent lanes whose names sort next to each
        // other, and a gap wider than the cap.
        let mut claims: Vec<(String, String, i64)> = Vec::new();
        for (lane, count) in [
            ("a", 1),
            ("aa", 5),
            ("ab", 2),
            ("b", 7),
            ("c", 1),
            ("cc", 3),
        ] {
            for k in 0..count {
                claims.push(claim(lane, &format!("{lane}-{k}"), 100 + (k as i64) * 250));
            }
        }
        // One lane whose last two claims straddle the cap.
        claims.push(claim("z", "z-0", 10));
        claims.push(claim("z", "z-1", 10 + 3 * MAX_CLAIM_WINDOW_S));
        let attempts = vec![("T-9".to_string(), "runner".to_string(), 5i64, 50i64)];

        let fast = claim_windows(attempts.clone(), claims.clone(), now);
        let slow = windows_by_scan(attempts, claims, now);
        assert_eq!(fast, slow, "the rewrite changed a window");
        assert!(!fast.is_empty());
    }

    /// The CONTROL for the cell above: the oracle and the subject must be able
    /// to disagree, or their agreement proves nothing. Unordered input is
    /// exactly where they part, and it is why the ordering is load-bearing
    /// rather than incidental.
    #[test]
    fn the_two_implementations_part_company_on_input_the_query_never_emits() {
        let now = 10_000;
        // Same lane, NOT contiguous — a shape `ORDER BY session, ts` cannot
        // produce. The scan finds alpha's later claim; the indexed lookup sees
        // a different lane in the next row and runs to `now`.
        let claims = vec![
            claim("alpha", "A-1", 100),
            claim("beta", "B-1", 200),
            claim("alpha", "A-2", 300),
        ];
        assert_ne!(
            claim_windows(Vec::new(), claims.clone(), now),
            windows_by_scan(Vec::new(), claims, now),
            "if these agree on unordered input the differential test above is vacuous"
        );
    }

    /// AMUX-4806: a rate is applied only where one is actually known, and the
    /// three outcomes are kept apart.
    ///
    /// The fallback used to catch EVERY unknown model and it is Anthropic
    /// Sonnet's rate, so 116,966 rows of gpt-*, gemini-* and a local qwen
    /// carried $6,942.14 nobody spent, in the same column as measured Claude
    /// cost. The fallback is kept for the case AMUX-4583 defended, a new model
    /// from the SAME vendor, and reaches no further.
    #[test]
    fn a_rate_is_applied_only_where_one_is_known() {
        let t = table();
        assert_eq!(
            price_for_model(&t, "claude-opus-5"),
            Some([15.0, 1.50, 18.75, 75.0])
        );
        assert_eq!(
            price_for_model(&t, "claude-haiku-4-5-20251001"),
            Some([0.80, 0.08, 1.00, 4.0])
        );
        // SAME VENDOR, unlisted: still approximated, which is AMUX-4583's case
        // and the reason the fallback survives at all. The name must not
        // contain an existing family keyword or the cell tests the family
        // match instead of the fallback -- `claude-opus-6` would match `opus`
        // and pass while proving nothing, which is how I first wrote it.
        assert_eq!(price_for_model(&t, "claude-nova-1"), Some(PRICE_DEFAULT));
        assert!(model_is_priced(&t, "claude-nova-1"));
        for fam in ["opus", "sonnet", "haiku", "fable"] {
            assert!(
                !"claude-nova-1".contains(fam),
                "fixture must not match {fam}"
            );
        }

        // DIFFERENT VENDOR, no rate: UNMETERED. Not Sonnet's price, which is
        // the live defect, and the cost is 0 because the column is NOT NULL —
        // `model_is_priced` is what says this zero is "withheld", not "free".
        for m in ["gpt-5.5", "gpt-6-astra", "gemini-3.5-flash", "gpt-5-codex"] {
            assert_eq!(
                price_for_model(&t, m),
                None,
                "{m} must not take a Claude rate"
            );
            assert!(!model_is_priced(&t, m), "{m} must read as unmetered");
            assert_eq!(
                turn_cost_usd(&t, m, [1_000_000, 0, 0, 1_000_000]),
                0.0,
                "{m}"
            );
        }

        // LOCAL: a known rate that happens to be zero. $0 is a FACT here, so it
        // must read as PRICED, which is what separates it from the rows above.
        for m in ["qwen3.8:27b", "qwen3-coder:30b-65k", "llama3.3:70b"] {
            assert_eq!(price_for_model(&t, m), Some([0.0; 4]), "{m}");
            assert!(model_is_priced(&t, m), "{m} is free, not unknown");
            assert_eq!(
                turn_cost_usd(&t, m, [9_000_000, 0, 0, 9_000_000]),
                0.0,
                "{m}"
            );
        }

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
        assert_eq!(
            price_for_model(&t, "claude-opus-5"),
            Some([1.0, 2.0, 3.0, 4.0])
        );
        assert_eq!(
            price_for_model(&t, "newmodel-x"),
            Some([9.0, 9.0, 9.0, 9.0])
        );
        // A malformed row is ignored, not fatal, and must not shadow anything.
        assert_eq!(price_for_model(&t, "sonnet"), Some([3.0, 0.30, 3.75, 15.0]));
    }

    /// AMUX-4806, criterion 5: the owner's answer to AMUX-4680 arrives as
    /// config, and an unmetered family starts metering the moment it lands.
    #[test]
    fn a_rate_added_to_prices_json_meters_a_previously_unmetered_family() {
        let dir = tempfile::tempdir().unwrap();
        // Before: no entry, so no number is invented.
        let t = prices(dir.path());
        assert_eq!(price_for_model(&t, "gpt-5.5"), None);
        assert!(!model_is_priced(&t, "gpt-5.5"));

        std::fs::write(
            dir.path().join("prices.json"),
            r#"{"gpt-5.5": [1.25, 0.125, 1.5, 10.0]}"#,
        )
        .unwrap();
        let t = prices(dir.path());
        assert_eq!(
            price_for_model(&t, "gpt-5.5"),
            Some([1.25, 0.125, 1.5, 10.0])
        );
        assert!(model_is_priced(&t, "gpt-5.5"));
        // 1M in + 1M out at those rates.
        assert!((turn_cost_usd(&t, "gpt-5.5", [1_000_000, 0, 0, 1_000_000]) - 11.25).abs() < 1e-9);
        // And its sibling families are untouched: one line prices one family.
        assert_eq!(price_for_model(&t, "gpt-6-astra"), None);

        // An explicit `_default` is the owner saying "price the unknowns like
        // this", which is theirs to decide and still honoured.
        std::fs::write(
            dir.path().join("prices.json"),
            r#"{"_default": [2.0, 2.0, 2.0, 2.0]}"#,
        )
        .unwrap();
        let t = prices(dir.path());
        assert_eq!(
            price_for_model(&t, "gpt-6-astra"),
            Some([2.0, 2.0, 2.0, 2.0])
        );
    }

    fn line(model: &str, ts: &str, inp: i64, cr: i64, cw: i64, out: i64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","message":{{"model":"{model}","usage":{{"input_tokens":{inp},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cw},"output_tokens":{out}}}}}}}"#
        )
    }

    fn line_with_id(
        model: &str,
        ts: &str,
        id: &str,
        inp: i64,
        cr: i64,
        cw: i64,
        out: i64,
    ) -> String {
        format!(
            r#"{{"timestamp":"{ts}","requestId":"req_{id}","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{inp},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cw},"output_tokens":{out}}}}}}}"#
        )
    }

    /// AMUX-4580, the live specimen's shape: one response's usage repeated on
    /// NON-adjacent lines (a different response in between) is billed once, and
    /// two different responses with identical counts are both billed.
    #[test]
    fn one_response_is_billed_once_by_message_id_even_when_its_repeats_are_not_adjacent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-sub.jsonl");
        let body = [
            line_with_id("opus", "2026-09-14T18:00:00Z", "msg_A", 32, 10865, 4512, 3),
            line_with_id("opus", "2026-09-14T18:00:01Z", "msg_B", 32, 10865, 4512, 3),
            line_with_id("opus", "2026-09-14T18:00:02Z", "msg_A", 32, 10865, 4512, 3),
            line_with_id("opus", "2026-09-14T18:00:03Z", "msg_A", 32, 10865, 4512, 3),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&path, &body).unwrap();
        let (_, turns) = parse_from(&path, 0, 1_786_000_000, "lane", &table());
        let ids: Vec<_> = turns
            .iter()
            .map(|t| t.message_id.clone().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["msg_A".to_string(), "msg_B".to_string()],
            "msg_A billed once despite non-adjacent repeats; msg_B kept though its counts match"
        );
        assert_eq!(turns[0].request_id.as_deref(), Some("req_msg_A"));
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
        assert_eq!(
            turns.iter().map(|t| t.tokens[0]).collect::<Vec<_>>(),
            vec![100, 5, 100]
        );
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
        assert_eq!(
            turns2.len(),
            1,
            "the already-indexed line must not be re-billed"
        );
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

    #[tokio::test]
    async fn foreign_hook_usage_recovery_preserves_tokens_history_and_audit() {
        let home = tempfile::tempdir().unwrap();
        let projects = tempfile::tempdir().unwrap();
        let store = store();
        for dir in ["sessions", "status-events/worker"] {
            std::fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        std::fs::create_dir_all(projects.path().join("foreign")).unwrap();
        std::fs::write(
            home.path().join("sessions/worker.env"),
            "CC_DIR=/project/repo\nCC_PROJECT=demo\nCC_PROVIDER=codex\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join("status-events/worker/current.json"),
            r#"{"provider":"codex","started":100,"run_id":"run"}"#,
        )
        .unwrap();
        std::fs::write(
            projects.path().join("foreign/foreign.jsonl"),
            "{\"cwd\":\"/other/repo\"}\n",
        )
        .unwrap();
        std::fs::write(
            projects.path().join("foreign/local.jsonl"),
            "{\"cwd\":\"/project/repo/subdir\"}\n",
        )
        .unwrap();
        std::fs::create_dir_all(projects.path().join("foreign/foreign/subagents")).unwrap();
        std::fs::write(
            projects
                .path()
                .join("foreign/foreign/subagents/agent-child.jsonl"),
            "{}\n",
        )
        .unwrap();
        store.write(|c| {
            c.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(101,'worker','session.native_status',?1,'native-hook')", [r#"{"provider":"codex","run_id":"run"}"#])?;
            for (ts, conversation) in [(99,"foreign"),(101,"foreign"),(101,"local"),(101,"unknown"),(101,"agent-child")] {
                c.execute("INSERT INTO token_ledger(ts,session,conversation,model,input,task) VALUES(?1,'worker',?2,'claude-opus',10,'task')", rusqlite::params![ts,conversation])?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        for _ in 0..2 {
            repair_foreign_project_owners(&store, home.path(), projects.path())
                .await
                .unwrap();
        }
        let c = store.read().unwrap();
        let totals: (i64,i64,i64) = c.query_row("SELECT count(*),sum(input),sum(CASE WHEN session='' AND task='' THEN 1 ELSE 0 END) FROM token_ledger", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(totals, (5, 50, 2));
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM session_events WHERE type='usage.ownership_repaired'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        drop(c);
        store.write(|c| {
            c.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(95,'worker','project.claimed',?1,'project')", [r#"{"project_group":"demo"}"#])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        repair_foreign_project_owners(&store, home.path(), projects.path())
            .await
            .unwrap();
        let c = store.read().unwrap();
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM token_ledger WHERE session=''",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            3,
            "the first launch also recovers misattribution between allocation and launch"
        );
    }

    /// AMUX-4581: the two misattributions the token audit measured, reproduced
    /// as fixtures and then required NOT to happen.
    ///
    /// Both were caused by a window that never closed being read as
    /// `COALESCE(left_doing, now)`: AMUX-1808 (entered_doing 2026-07-21, no
    /// session) absorbed $2,680 of ownerless conversations in 7 days, and
    /// AMUX-2598 (session `amux`) absorbed $1,618 of every amux turn. Both
    /// stale rows are seeded here in the shape the live DB actually holds.
    #[tokio::test]
    async fn a_stale_open_window_stops_absorbing_every_later_turn() {
        let st = store();
        let now = chrono::Utc::now().timestamp();
        let july = now - 56 * 86400;
        st.write(move |conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS task_windows (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 task TEXT NOT NULL, title TEXT NOT NULL DEFAULT '', session TEXT NOT NULL DEFAULT '', \
                 entered_doing INTEGER NOT NULL, left_doing INTEGER);",
            )?;
            crate::db::attempts::ensure_table(conn)?;
            // The two absorbing windows, never closed, exactly as measured.
            conn.execute(
                "INSERT INTO task_windows (task, session, entered_doing, left_doing) \
                 VALUES ('AMUX-1808','',?1,NULL), ('AMUX-2598','amux',?1,NULL)",
                [july],
            )?;
            // THE SAME SHAPE ON THE NEW SOURCE. Without these two rows the
            // empty-lane rule is untestable: dropping it stays green purely
            // because the dead table is no longer read, so the fixture would be
            // proving the table swap and nothing else. An attempt and a claim
            // that carry no lane are the way AMUX-1808 could come back.
            conn.execute(
                "INSERT INTO task_attempts (card, attempt, worker, generation, started_at, ended_at) \
                 VALUES ('AMUX-1808', 1, '', 1, ?1, NULL)",
                [now - 3600],
            )?;
            conn.execute(
                "INSERT INTO session_events (ts, session, type, data, source) \
                 VALUES (?1,'','task.claimed','{\"issue\":\"AMUX-1808\"}','test')",
                [now - 3600],
            )?;
            // Turns from today: one ownerless, one on the amux lane.
            conn.execute(
                "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, \
                 cache_write, output, cost_usd, task) VALUES \
                 (?1,'','conv-ownerless','m',10,0,0,1,0.5,''), \
                 (?1,'amux','conv-amux','m',10,0,0,1,0.5,'')",
                [now - 60],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();

        attribute_tasks(&st).await.unwrap();

        let task_of = |conv: &str| -> String {
            st.read()
                .unwrap()
                .query_row(
                    "SELECT task FROM token_ledger WHERE conversation=?1",
                    [conv],
                    |r| r.get::<_, String>(0),
                )
                .unwrap()
        };
        assert_eq!(
            task_of("conv-ownerless"),
            "",
            "an ownerless conversation must not land on AMUX-1808: an empty lane is not an identity"
        );
        assert_eq!(
            task_of("conv-amux"),
            "",
            "a 56-day-old unclosed window must not claim today's amux turn (AMUX-2598)"
        );
    }

    /// The other half: with the dead table gone, a REAL claim still attributes.
    /// Without this the fix above would be indistinguishable from attributing
    /// nothing at all, which would also make both assertions pass.
    #[tokio::test]
    async fn a_live_attempt_and_a_historical_claim_still_attribute() {
        let st = store();
        let now = chrono::Utc::now().timestamp();
        st.write(move |conn| {
            crate::db::attempts::ensure_table(conn)?;
            conn.execute(
                "INSERT INTO task_attempts (card, attempt, worker, generation, started_at, ended_at) \
                 VALUES ('AMUX-9001', 1, 'lane-a', 1, ?1, NULL)",
                [now - 600],
            )?;
            conn.execute(
                "INSERT INTO session_events (ts, session, type, data, source) \
                 VALUES (?1,'lane-b','task.claimed','{\"issue\":\"AMUX-9002\"}','test')",
                [now - 500],
            )?;
            conn.execute(
                "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, \
                 cache_write, output, cost_usd, task) VALUES \
                 (?1,'lane-a','conv-a','m',10,0,0,1,0.5,''), \
                 (?1,'lane-b','conv-b','m',10,0,0,1,0.5,''), \
                 (?2,'lane-a','conv-old','m',10,0,0,1,0.5,'')",
                [now - 300, now - 40 * 3600],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();

        attribute_tasks(&st).await.unwrap();

        let task_of = |conv: &str| -> String {
            st.read()
                .unwrap()
                .query_row(
                    "SELECT task FROM token_ledger WHERE conversation=?1",
                    [conv],
                    |r| r.get::<_, String>(0),
                )
                .unwrap()
        };
        assert_eq!(
            task_of("conv-a"),
            "AMUX-9001",
            "a running attempt attributes its lane's turn"
        );
        assert_eq!(
            task_of("conv-b"),
            "AMUX-9002",
            "a task.claimed event still attributes historical turns"
        );
        assert_eq!(
            task_of("conv-old"),
            "",
            "a turn 40h BEFORE the claim is outside the window and must stay unattributed"
        );
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
            format!(
                "{}\n{}\n",
                r#"{"customTitle":"gtm-videos"}"#,
                line("claude-opus-5", "2026-08-11T10:00:00Z", 11, 0, 0, 1)
            ),
        )
        .unwrap();
        // Delegated transcript: NO owner of its own.
        std::fs::write(
            proj.join(conv).join("subagents").join("agent-abc123.jsonl"),
            format!(
                "{}\n{}\n",
                r#"{"type":"user"}"#,
                line("claude-opus-5", "2026-08-11T10:01:00Z", 22, 0, 0, 2)
            ),
        )
        .unwrap();

        let st = store();
        // The claims map is now an ARGUMENT, so this cell states which one it
        // is testing under instead of inheriting whatever is on the machine.
        // Empty = no meta claim, so the owner resolves from the title record.
        let claims = BTreeMap::new();
        let n = index_once_at(&st, home.path(), projects.path(), &claims)
            .await
            .unwrap();
        assert_eq!(n, 2, "one parent turn + one delegated turn");

        let rows = ledger(&st).await;
        let by_conv: std::collections::HashMap<_, _> = rows
            .iter()
            .map(|(s, c, i)| (c.clone(), (s.clone(), *i)))
            .collect();
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
            proj.join("dead-uuid")
                .join("subagents")
                .join("agent-zzz.jsonl"),
            format!(
                "{}\n{}\n",
                r#"{"type":"user"}"#,
                line("sonnet", "2026-08-11T10:00:00Z", 5, 0, 0, 1)
            ),
        )
        .unwrap();

        let st = store();
        // No claim for this conversation: the owner must come from the
        // transcript, which is what this cell is about.
        let claims = BTreeMap::new();
        assert_eq!(
            index_once_at(&st, home.path(), projects.path(), &claims)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            ledger(&st).await,
            vec![(String::new(), "agent-zzz".into(), 5)]
        );
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
            format!(
                "{}\n{}\n",
                r#"{"customTitle":"gtm-videos"}"#,
                line("sonnet", "2026-08-11T10:00:00Z", 7, 0, 0, 1)
            ),
        )
        .unwrap();

        let st = store();
        let mut claims = BTreeMap::new();
        claims.insert(conv.to_string(), "claiming-lane".to_string());
        assert_eq!(
            index_once_at(&st, home.path(), projects.path(), &claims)
                .await
                .unwrap(),
            1
        );
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
            format!(
                "{}\n{}\n",
                r#"{"customTitle":"amux"}"#,
                line("sonnet", "2026-08-11T10:00:00Z", 9, 0, 0, 1)
            ),
        )
        .unwrap();
        std::fs::write(
            proj.join("u1").join("subagents").join("agent-q.jsonl"),
            format!(
                "{}\n{}\n",
                r#"{"type":"user"}"#,
                line("sonnet", "2026-08-11T10:01:00Z", 3, 0, 0, 1)
            ),
        )
        .unwrap();

        let st = store();
        // No claim for this conversation: the owner must come from the
        // transcript, which is what this cell is about.
        let claims = BTreeMap::new();
        assert_eq!(
            index_once_at(&st, home.path(), projects.path(), &claims)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            index_once_at(&st, home.path(), projects.path(), &claims)
                .await
                .unwrap(),
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
        std::fs::write(
            &named,
            "{\"customTitle\":\"amux-rust\"}\n{\"customTitle\":\"amux\"}\n",
        )
        .unwrap();
        assert_eq!(jsonl_owner_title(&named, &BTreeMap::new()), "amux");

        let anon = dir.path().join("b.jsonl");
        std::fs::write(&anon, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(jsonl_owner_title(&anon, &BTreeMap::new()), "");

        let missing = dir.path().join("nope.jsonl");
        assert_eq!(jsonl_owner_title(&missing, &BTreeMap::new()), "");
    }
}
