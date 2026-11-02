//! Z5: a red main after a land is acted on inside one queue cycle, or left alone.
//!
//! A repository that declares `amux.queueGate` and `amux.queueChecks` (the
//! required workflow names) in `.amux/land.conf` gets this watch over every push
//! the queue made. `amux.queueOnRed` is `fix` (the default), `revert` or `off`.
//! Each push is judged oldest first, once:
//!
//! - green, or its runs never ran: settled, nothing happens.
//! - red with a base that is not proven green, or a newer main already green:
//!   not attributable to this land. Nothing is reverted (land_red_unattributed,
//!   land_red_healed). main-ci-watch keeps those reds.
//! - red on a first attempt: its failed jobs are re-run once. A re-run that
//!   passes was flaky (land_red_flaky) and nothing happens.
//! - red again: attributed. The push's base was green and the push is the only
//!   change between that and this red. One entry and `revert`: a revert commit
//!   joins the queue at priority and passes the same gate as any land, so the
//!   revert lands in the next queue cycle. Anything else, a batch of several
//!   entries included, opens a fix: a card on each lane, or a message when the
//!   lane has no board. A revert that cannot be built or is refused opens one too.
//!
//! It cannot loop. A repository, sha pair has one episode (a UNIQUE key), a
//! revert is never itself reverted, a push is settled once, one revert is in
//! flight per repository, and at most REVERT_CAP land in a day; past those the
//! answer is a fix. Crash boundary: the revert row and the episode are written in
//! one transaction, so a SIGKILL either left nothing, and the next tick redoes
//! it, or left a queued row the queue finishes; the revert commit is built from
//! the same inputs either way. A notice is resent until `told_at` says it went,
//! and the steering queue replaces an identical undelivered text.
//!
//! AMUX_LAND_RED=0 in server.env stops the watch (A11). Verdicts: land_red_*.
use crate::api::land_queue::{repo_policy, OnRed};
use crate::api::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const DEFAULT_TICK_S: u64 = 120;
/// A push still unjudged this long after it landed is given up on, unmeasured.
const WINDOW_S: f64 = 6.0 * 3600.0;
/// A required workflow with no run this long after the land did not run for it
/// (path filters, a cancelled queue).
const NO_RUN_S: f64 = 1800.0;
const RERUN_TRIES: i64 = 3;
pub const REVERT_CAP: i64 = 2;
const CAP_WINDOW_S: f64 = 24.0 * 3600.0;
const GH_TIMEOUT_S: u64 = 60;

fn home() -> PathBuf {
    crate::config::amux_home()
}

fn env_value(key: &str) -> Option<String> {
    crate::config::parse_env_file(&home().join("server.env")).get(key).map(|v| v.trim().trim_matches('"').to_string())
}

fn switched_off() -> bool {
    env_value("AMUX_LAND_RED").is_some_and(|v| matches!(v.as_str(), "0" | "false" | "off" | "no"))
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    crate::fanout_workspace::git(&dir.to_string_lossy(), args).await
}

/// A store write this watch needs. Its failure is retried by the next tick, and
/// it says so, because a watch that silently stops recording is no watch.
fn written<T>(what: &str, r: Result<T, impl std::fmt::Display>) {
    if let Err(e) = r {
        tracing::warn!(what, error = %e, measured = false, n_considered = 1, why_unmeasured = "a store write failed",
            verdict = "land_red_write_failed", "the next tick retries");
    }
}

fn short(s: &str) -> &str {
    &s[..12.min(s.len())]
}

// ---------------------------------------------------------------------------
// CI, read through `gh`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub attempt: i64,
    pub url: String,
}

#[derive(Debug, PartialEq)]
pub enum Ci {
    Pending,
    Green,
    Red(Vec<Run>),
    Unmeasured(String),
}

/// `owner/name` of a GitHub remote; None for anything else, which is unmeasured.
pub fn slug_of(url: &str) -> Option<String> {
    if let Some(s) = env_value("AMUX_LAND_RED_SLUG").filter(|s| !s.is_empty()) {
        return Some(s);
    }
    let rest = url.split_once("github.com").map(|(_, r)| r.trim_start_matches([':', '/']))?;
    let slug = rest.trim_end_matches('/').trim_end_matches(".git");
    (slug.split('/').count() == 2 && !slug.contains(' ')).then(|| slug.to_string())
}

async fn gh(args: &[String]) -> Result<String, String> {
    let bin = env_value("AMUX_LAND_RED_GH").filter(|s| !s.is_empty()).unwrap_or_else(|| "gh".into());
    let run = tokio::process::Command::new(bin).args(args).kill_on_drop(true).stdin(std::process::Stdio::null()).output();
    match tokio::time::timeout(Duration::from_secs(GH_TIMEOUT_S), run).await {
        Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(Ok(o)) => Err(String::from_utf8_lossy(&o.stderr).trim().chars().take(300).collect()),
        Ok(Err(e)) => Err(format!("cannot run gh: {e}")),
        Err(_) => Err(format!("gh timed out after {GH_TIMEOUT_S}s")),
    }
}

/// The push runs at `sha`: the newest run of each workflow.
pub fn parse_runs(text: &str) -> Result<Vec<Run>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("unreadable runs: {e}"))?;
    let rows = v.get("workflow_runs").and_then(Value::as_array).ok_or("no workflow_runs in the answer")?;
    let mut newest: HashMap<String, Run> = HashMap::new();
    for r in rows {
        if r.get("event").and_then(Value::as_str).is_some_and(|e| e != "push") {
            continue;
        }
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let run = Run { id: r.get("id").and_then(Value::as_i64).unwrap_or(0), name: s("name"), status: s("status"),
            conclusion: s("conclusion"), attempt: r.get("run_attempt").and_then(Value::as_i64).unwrap_or(1), url: s("html_url") };
        if newest.get(&run.name).is_none_or(|old| run.id > old.id) {
            newest.insert(run.name.clone(), run);
        }
    }
    Ok(newest.into_values().collect())
}

async fn runs_at(slug: &str, sha: &str) -> Result<Vec<Run>, String> {
    parse_runs(&gh(&["api".to_string(), format!("repos/{slug}/actions/runs?head_sha={sha}&per_page=100")]).await?)
}

async fn rerun_failed(slug: &str, run: i64) -> Result<String, String> {
    gh(&["api".to_string(), "-X".to_string(), "POST".to_string(), format!("repos/{slug}/actions/runs/{run}/rerun-failed-jobs")]).await
}

/// What the required workflows say about one commit. A failure wins over
/// anything still running. A cancelled, skipped or timed-out run says nothing
/// about the code, so it is unmeasured, never red.
pub fn classify(required: &[String], runs: &[Run], age_s: f64) -> Ci {
    let mut red = Vec::new();
    let mut pending = false;
    let mut unmeasured = None;
    for name in required {
        match runs.iter().find(|r| &r.name == name) {
            None if age_s > NO_RUN_S => unmeasured = Some(format!("no {name} run after {age_s:.0}s")),
            None => pending = true,
            Some(r) if r.status != "completed" => pending = true,
            Some(r) if r.conclusion == "success" => {}
            Some(r) if r.conclusion == "failure" => red.push(r.clone()),
            Some(r) => unmeasured = Some(format!("{name} ended {}", r.conclusion)),
        }
    }
    if !red.is_empty() {
        Ci::Red(red)
    } else if pending {
        Ci::Pending
    } else if let Some(why) = unmeasured {
        Ci::Unmeasured(why)
    } else {
        Ci::Green
    }
}

// ---------------------------------------------------------------------------
// The pushes the queue made
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Landed {
    id: i64,
    lane: String,
    kind: String,
    landed_base: Option<String>,
    landed_tip: Option<String>,
}

#[derive(Clone, Debug)]
struct Push {
    repo: String,
    tip: String,
    base: String,
    tree: Option<String>,
    done_at: f64,
    rows: Vec<Landed>,
}

async fn unsettled(state: &AppState) -> Vec<Push> {
    type Row = (i64, String, String, String, String, String, Option<String>, Option<String>, Option<String>, f64);
    let rows: Vec<Row> = state.store.read_async(|c| {
        let mut st = c.prepare("SELECT id, repo, lane, kind, merged_sha, base_sha, landed_base, landed_tip, tree, done_at FROM land_queue
            WHERE state = 'merged' AND ci_state IS NULL AND base_sha IS NOT NULL AND merged_sha IS NOT NULL AND done_at IS NOT NULL
            ORDER BY id LIMIT 500")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let mut out: Vec<Push> = Vec::new();
    for (id, repo, lane, kind, tip, base, landed_base, landed_tip, tree, done_at) in rows {
        let row = Landed { id, lane, kind, landed_base, landed_tip };
        match out.iter_mut().find(|p| p.repo == repo && p.tip == tip) {
            Some(p) => p.rows.push(row),
            None => out.push(Push { repo, tip, base, tree, done_at, rows: vec![row] }),
        }
    }
    out
}

async fn settle(state: &AppState, push: &Push, ci: &str) {
    let (r, t, c) = (push.repo.clone(), push.tip.clone(), ci.to_string());
    let done = state.store.write_async(move |conn| {
        let n = conn.execute("UPDATE land_queue SET ci_state = ?3 WHERE repo = ?1 AND merged_sha = ?2 AND ci_state IS NULL", rusqlite::params![r, t, c])?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    }).await;
    written("settle", done);
}

fn tree_of(push_tree: Option<&str>, repo: &str) -> Option<PathBuf> {
    push_tree.map(PathBuf::from).filter(|p| p.is_dir())
        .or_else(|| Path::new(repo).parent().map(Path::to_path_buf).filter(|p| p.join(".git").exists()))
}

// ---------------------------------------------------------------------------
// Episodes: one per repository and red sha
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Episode {
    state: String,
    tries: i64,
}

async fn episode(state: &AppState, repo: &str, tip: &str) -> Option<Episode> {
    let (r, t) = (repo.to_string(), tip.to_string());
    state.store.read_async(move |c| {
        use rusqlite::OptionalExtension;
        Ok(c.query_row("SELECT state, tries FROM land_red WHERE repo = ?1 AND red_sha = ?2", [r, t],
            |x| Ok(Episode { state: x.get(0)?, tries: x.get(1)? })).optional()?)
    }).await.ok().flatten()
}

fn upsert(conn: &rusqlite::Connection, push: &Push, st: &str, tries: i64, detail: &str, now: f64) -> rusqlite::Result<()> {
    let lanes = lanes_of(push).join(",");
    let land_id = push.rows.first().map(|r| r.id).unwrap_or(0);
    conn.execute("INSERT INTO land_red (repo, red_sha, base_sha, land_id, lanes, tree, state, tries, detail, opened_at, updated_at)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                  ON CONFLICT (repo, red_sha) DO UPDATE SET state = ?7, tries = ?8, detail = ?9, updated_at = ?10",
        rusqlite::params![push.repo, push.tip, push.base, land_id, lanes, push.tree, st, tries, detail, now])?;
    Ok(())
}

async fn set_episode(state: &AppState, push: &Push, st: &str, tries: i64, detail: &str) {
    let (p, s, d, now) = (push.clone(), st.to_string(), detail.to_string(), crate::config::now_f64());
    let done = state.store.write_async(move |conn| {
        upsert(conn, &p, &s, tries, &d, now)?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    written("episode", done);
}

fn lanes_of(push: &Push) -> Vec<String> {
    let mut v: Vec<String> = push.rows.iter().map(|r| r.lane.clone()).collect();
    v.dedup();
    v.sort();
    v.dedup();
    v
}

fn evidence(push: &Push, failed: &[Run]) -> String {
    let runs: Vec<String> = failed.iter().map(|r| format!("{} attempt {}: {}", r.name, r.attempt, r.url)).collect();
    format!("main went red at {} after land {}. Its base {} was green.\n{}",
        short(&push.tip), push.rows.iter().map(|r| r.id.to_string()).collect::<Vec<_>>().join(", "), short(&push.base), runs.join("\n"))
}

/// Open a fix: a card on each lane with a board, in one write with the episode,
/// so a retry cannot file a second. The notice follows from `told_at`.
async fn open_fix(state: &AppState, push: &Push, why: &str, detail: &str) -> bool {
    let (p, whyt, det, now) = (push.clone(), why.to_string(), detail.to_string(), crate::config::now_f64());
    let boarded: Vec<String> = lanes_of(push).into_iter().filter(|l| !crate::api::session_verbs::session_is_isolated(l)).collect();
    let n_cards = boarded.len();
    let out = state.store.write_async(move |conn| {
        let opened: Option<String> = {
            use rusqlite::OptionalExtension;
            conn.query_row("SELECT state FROM land_red WHERE repo = ?1 AND red_sha = ?2", [&p.repo, &p.tip], |r| r.get(0)).optional()?
        };
        if opened.as_deref() == Some("fix_opened") {
            return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
        }
        let full = format!("{det}\n\n{whyt}\n\nFix forward or revert it, then land through `amux land`.");
        upsert(conn, &p, "fix_opened", 0, &full, now)?;
        let mut cards = Vec::new();
        for lane in &boarded {
            let new = crate::db::board_store::NewIssue {
                title: format!("main is red after your land: {}", short(&p.tip)),
                desc: full.clone(),
                status: "todo".into(),
                session: Some(lane.clone()),
                item_type: "bug".into(),
                creator: "land-queue".into(),
                owner_type: "agent".into(),
                due: None, due_time: None, reviewer: None, shepherd: None,
                gate: vec![], depends_on: vec![], tags: vec![],
                ask_type: None, next_action: None, acceptance_criteria: None,
                ask_question: None, ask_unblocks: None, ask_actor: None,
                source: Some("agent".into()), requested_by: None,
                callback_session: None, callback_prompt: None,
            };
            cards.push(crate::db::board_store::create_issue(conn, &new, now as i64)?.id);
        }
        conn.execute("UPDATE land_red SET cards = ?3 WHERE repo = ?1 AND red_sha = ?2", rusqlite::params![p.repo, p.tip, cards.join(",")])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    match out {
        Ok(o) => {
            if o.applied {
                tracing::warn!(repo = %push.repo, sha = %push.tip, why, measured = true, n_considered = 1, n_cards,
                    verdict = "land_red_fix_opened", "main is red after a land; the lanes hold a fix");
            }
            true
        }
        Err(e) => {
            tracing::warn!(repo = %push.repo, sha = %push.tip, error = %e, measured = false, n_considered = 1,
                why_unmeasured = "the fix could not be written", verdict = "land_red_fix_failed", "the next tick retries");
            false
        }
    }
}

/// The revert of one entry as a commit on main. Err is a conflict with what
/// landed since, or anything else that stops a clean build.
async fn build_revert(tree: &Path, row: &Landed, evidence: &str) -> Result<String, String> {
    let (Some(from), Some(to)) = (row.landed_base.as_deref(), row.landed_tip.as_deref()) else {
        return Err("the row has no landed range".into());
    };
    git(tree, &["fetch", "-q", "origin", "main"]).await?;
    let main = git(tree, &["rev-parse", "origin/main"]).await?;
    let tree_id = revert_tree(tree, &main, from, to).await?;
    let subject = git(tree, &["log", "-1", "--format=%s", to]).await.unwrap_or_default();
    let msg = format!("Revert \"{subject}\" (land {id})\n\n{evidence}\n\nThis reverts commit {to}.\n\nRevert: land the change again once its red is fixed\nAmux-Land-Revert: {id}\nAmux-Session: amux-land\n", id = row.id);
    let who = ["-c", "user.name=amux land queue", "-c", "user.email=land-queue@amux.invalid"];
    let mut args: Vec<&str> = who.to_vec();
    args.extend(["commit-tree", &tree_id, "-p", &main, "-m", &msg]);
    let sha = git(tree, &args).await?;
    // A ref, so the commit outlives a gc while it waits for its turn.
    git(tree, &["update-ref", &format!("refs/amux/land-revert/{}", row.id), &sha]).await?;
    Ok(sha)
}

/// git with a private index, optionally fed `stdin`.
async fn git_idx(tree: &Path, idx: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> Result<String, String> {
    let mut c = tokio::process::Command::new("git");
    c.arg("-C").arg(tree).args(args).env("GIT_INDEX_FILE", idx).kill_on_drop(true)
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("git {} did not start: {e}", args.join(" ")))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use tokio::io::AsyncWriteExt;
        pipe.write_all(&bytes).await.map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    }
    let o = child.wait_with_output().await.map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().chars().take(400).collect::<String>())
    }
}

/// main's tree with `from..to` taken back out, built in a private index with
/// `git apply --cached -R`, which every git has. `merge-tree --merge-base` needs
/// git 2.40, and on the 2.39 in rust:bookworm every revert failed to build and
/// became a fix (harness-recovery job 125100056947, 2026-11-02).
async fn revert_tree(tree: &Path, main: &str, from: &str, to: &str) -> Result<String, String> {
    let diff = tokio::process::Command::new("git").arg("-C").arg(tree)
        .args(["diff", "--binary", "--full-index", from, to]).kill_on_drop(true).output().await
        .map_err(|e| format!("git diff did not start: {e}"))?;
    if !diff.status.success() {
        return Err(format!("git diff {from}..{to}: {}", String::from_utf8_lossy(&diff.stderr).trim()));
    }
    let idx = home().join("tmp").join("land").join(format!("revert-index-{}-{}", std::process::id(), short(to)));
    if let Some(d) = idx.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("cannot create {}: {e}", d.display()))?;
    }
    let out = async {
        git_idx(tree, &idx, &["read-tree", main], None).await?;
        if !diff.stdout.is_empty() {
            git_idx(tree, &idx, &["apply", "--cached", "-R"], Some(diff.stdout.clone())).await
                .map_err(|e| format!("the revert conflicts with origin/main {}: {e}", short(main)))?;
        }
        git_idx(tree, &idx, &["write-tree"], None).await
    }.await;
    let _ = std::fs::remove_file(&idx);
    out
}

/// The revert row and the episode, in one transaction.
async fn queue_revert(state: &AppState, push: &Push, row: &Landed, sha: &str, detail: &str) -> Result<i64, String> {
    let (p, lane, s, d, now, origin) = (push.clone(), row.lane.clone(), sha.to_string(), detail.to_string(), crate::config::now_f64(), row.id);
    let id = std::sync::Arc::new(std::sync::Mutex::new(0i64));
    let out = id.clone();
    state.store.write_async(move |conn| {
        upsert(conn, &p, "reverting", 0, &d, now)?;
        conn.execute("INSERT INTO land_queue (repo, lane, sha, priority, reason, queued_at, kind, tree)
                      VALUES (?1, ?2, ?3, 1, ?4, ?5, 'revert', ?6)",
            rusqlite::params![p.repo, lane, s, format!("revert of land {origin}: main went red"), now, p.tree])?;
        let rid = conn.last_insert_rowid();
        conn.execute("UPDATE land_red SET revert_land_id = ?3 WHERE repo = ?1 AND red_sha = ?2", rusqlite::params![p.repo, p.tip, rid])?;
        if let Ok(mut g) = out.lock() {
            *g = rid;
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await.map_err(|e| e.to_string())?;
    let v = id.lock().map(|g| *g).unwrap_or(0);
    Ok(v)
}

/// Reverts in flight, and reverts this repository started inside the cap window.
async fn revert_load(state: &AppState, repo: &str) -> (i64, i64) {
    let (r, since) = (repo.to_string(), crate::config::now_f64() - CAP_WINDOW_S);
    state.store.read_async(move |c| {
        let live: i64 = c.query_row("SELECT count(*) FROM land_red WHERE repo = ?1 AND state = 'reverting'", [&r], |x| x.get(0))?;
        let day: i64 = c.query_row("SELECT count(*) FROM land_red WHERE repo = ?1 AND revert_land_id IS NOT NULL AND opened_at >= ?2",
            rusqlite::params![r, since], |x| x.get(0))?;
        Ok((live, day))
    }).await.unwrap_or((1, REVERT_CAP))
}

/// A confirmed red: revert the one attributable entry, or open a fix. true when
/// the decision is durable, so the push may be settled; false retries next tick.
async fn act(state: &AppState, push: &Push, tree: &Path, on_red: OnRed, failed: &[Run]) -> bool {
    let ev = evidence(push, failed);
    if push.rows.iter().any(|r| r.kind == "revert") {
        return open_fix(state, push, "A revert the queue landed went red. A revert is never reverted, so this one is for a person or a lane to read.", &ev).await;
    }
    if push.rows.len() != 1 {
        return open_fix(state, push, "This push carried several entries and CI cannot say which one broke it, so nothing was reverted.", &ev).await;
    }
    if on_red != OnRed::Revert {
        return open_fix(state, push, "This repository declares no auto-revert (amux.queueOnRed).", &ev).await;
    }
    let (live, day) = revert_load(state, &push.repo).await;
    if live > 0 || day >= REVERT_CAP {
        tracing::warn!(repo = %push.repo, sha = %push.tip, live, day, cap = REVERT_CAP, measured = true, n_considered = 1,
            verdict = "land_red_revert_capped", "the revert budget is spent; this red gets a fix");
        return open_fix(state, push, &format!("The queue reverts at most {REVERT_CAP} changes a day and one at a time, and that budget is spent."), &ev).await;
    }
    let row = &push.rows[0];
    match build_revert(tree, row, &ev).await {
        Ok(sha) => match queue_revert(state, push, row, &sha, &ev).await {
            Ok(id) => {
                tracing::warn!(repo = %push.repo, sha = %push.tip, revert = %sha, revert_land_id = id, lane = %row.lane, measured = true, n_considered = 1,
                    verdict = "land_red_revert_queued", "main is red after one land; its revert joined the queue at priority");
                true
            }
            Err(e) => {
                tracing::warn!(repo = %push.repo, sha = %push.tip, error = %e, measured = false, n_considered = 1,
                    why_unmeasured = "the revert row could not be written", verdict = "land_red_revert_failed", "the next tick retries");
                false
            }
        },
        Err(why) => {
            tracing::warn!(repo = %push.repo, sha = %push.tip, reason = %why, measured = true, n_considered = 1,
                verdict = "land_red_revert_unbuildable", "the revert commit could not be built; the lane gets a fix instead");
            open_fix(state, push, &format!("The revert could not be built: {why}."), &ev).await
        }
    }
}

/// One push, oldest first. true settles this repository's turn for the tick;
/// the caller moves on when false says this push is finished.
async fn judge(state: &AppState, push: &Push) -> bool {
    let age = crate::config::now_f64() - push.done_at;
    let Some(tree) = tree_of(push.tree.as_deref(), &push.repo) else {
        tracing::warn!(repo = %push.repo, sha = %push.tip, measured = false, n_considered = 1, why_unmeasured = "no checkout of the repository remains",
            verdict = "land_red_unmeasured", "a landed push cannot be watched");
        settle(state, push, "unmeasured").await;
        return false;
    };
    let policy = repo_policy(&tree, "origin/main").await;
    if policy.gate.is_none() || policy.on_red == OnRed::Off || policy.checks.is_empty() {
        settle(state, push, "off").await;
        return false;
    }
    let url = git(&tree, &["remote", "get-url", "origin"]).await.unwrap_or_default();
    let Some(slug) = slug_of(&url) else {
        tracing::warn!(repo = %push.repo, sha = %push.tip, measured = false, n_considered = 1, why_unmeasured = "origin is not a GitHub remote",
            verdict = "land_red_unmeasured", "a landed push cannot be watched");
        settle(state, push, "unmeasured").await;
        return false;
    };
    if age > WINDOW_S {
        tracing::warn!(repo = %push.repo, sha = %push.tip, age_s = age, measured = false, n_considered = 1, why_unmeasured = "CI gave no verdict inside the window",
            verdict = "land_red_unmeasured", "a landed push is no longer watched");
        settle(state, push, "unmeasured").await;
        return false;
    }
    let runs = match runs_at(&slug, &push.tip).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(repo = %push.repo, sha = %push.tip, measured = false, n_considered = 0, why_unmeasured = %e,
                verdict = "land_red_unmeasured", "CI could not be read; the next tick retries");
            return true;
        }
    };
    let prior = episode(state, &push.repo, &push.tip).await;
    match classify(&policy.checks, &runs, age) {
        Ci::Pending => true,
        Ci::Unmeasured(why) => {
            tracing::warn!(repo = %push.repo, sha = %push.tip, measured = false, n_considered = 1, why_unmeasured = %why,
                verdict = "land_red_unmeasured", "CI said nothing about this push");
            settle(state, push, "unmeasured").await;
            false
        }
        Ci::Green => {
            if prior.is_some_and(|e| e.state == "rerun") {
                tracing::warn!(repo = %push.repo, sha = %push.tip, measured = true, n_considered = 1, verdict = "land_red_flaky",
                    "the failed jobs passed when re-run; nothing is reverted");
                set_episode(state, push, "flaky", 0, "a re-run of the failed jobs passed").await;
            }
            settle(state, push, "green").await;
            false
        }
        Ci::Red(failed) => {
            // Already decided (a settle that failed after it): settle, never act twice.
            if prior.as_ref().is_some_and(|e| matches!(e.state.as_str(), "reverting" | "fix_opened" | "reverted")) {
                settle(state, push, "red").await;
                return false;
            }
            let first_look = prior.is_none();
            let tries = prior.map(|e| e.tries).unwrap_or(0);
            if first_look {
                // Attribution: nothing else may explain this red.
                if let Some((why, st)) = not_this_land(&tree, &slug, push, &policy.checks, age).await {
                    let Some(st) = st else { return true };
                    tracing::warn!(repo = %push.repo, sha = %push.tip, why, measured = true, n_considered = 1, verdict = format!("land_red_{st}"),
                        "main is red and this land is not the proven cause; nothing is reverted");
                    set_episode(state, push, st, 0, &why).await;
                    settle(state, push, "red").await;
                    return false;
                }
            }
            let first_attempt: Vec<&Run> = failed.iter().filter(|r| r.attempt == 1).collect();
            if !first_attempt.is_empty() {
                if tries >= RERUN_TRIES {
                    if !open_fix(state, push, "Its failed jobs could not be re-run to rule out a flake, so nothing was reverted.", &evidence(push, &failed)).await {
                        return true;
                    }
                    settle(state, push, "red").await;
                    return false;
                }
                for r in first_attempt {
                    if let Err(e) = rerun_failed(&slug, r.id).await {
                        // main-ci-watch may have re-run it first; the next tick reads which.
                        tracing::info!(run = r.id, error = %e, measured = true, n_considered = 1, verdict = "land_red_rerun_refused", "the re-run was refused");
                    }
                }
                tracing::info!(repo = %push.repo, sha = %push.tip, tries = tries + 1, measured = true, n_considered = failed.len(), verdict = "land_red_rerun",
                    "main is red on a first attempt; its failed jobs run again before anything is attributed");
                set_episode(state, push, "rerun", tries + 1, &evidence(push, &failed)).await;
                return true;
            }
            // Again at the moment of acting: main may have gone green during the re-run.
            if let Some((why, st)) = not_this_land(&tree, &slug, push, &policy.checks, age).await {
                let Some(st) = st else { return true };
                tracing::warn!(repo = %push.repo, sha = %push.tip, why, measured = true, n_considered = 1, verdict = format!("land_red_{st}"),
                    "main is red and this land is not the proven cause; nothing is reverted");
                set_episode(state, push, st, 0, &why).await;
                settle(state, push, "red").await;
                return false;
            }
            if !act(state, push, &tree, policy.on_red, &failed).await {
                return true;
            }
            settle(state, push, "red").await;
            false
        }
    }
}

/// Why this red cannot be pinned on this push, as (why, Some(state)); (_, None)
/// waits for the base's CI. None means nothing else explains it.
async fn not_this_land(tree: &Path, slug: &str, push: &Push, checks: &[String], age: f64) -> Option<(String, Option<&'static str>)> {
    let main = git(tree, &["rev-parse", "origin/main"]).await.unwrap_or_default();
    if !main.is_empty() && main != push.tip {
        if let Ok(runs) = runs_at(slug, &main).await {
            if classify(checks, &runs, 0.0) == Ci::Green {
                return Some((format!("origin/main {} is already green", short(&main)), Some("healed")));
            }
        }
    }
    match runs_at(slug, &push.base).await {
        Err(e) => Some((format!("the base's CI could not be read: {e}"), None)),
        Ok(runs) => match classify(checks, &runs, age.max(NO_RUN_S + 1.0)) {
            Ci::Green => None,
            Ci::Pending => Some(("the base is still running".into(), None)),
            Ci::Red(_) => Some((format!("the base {} was already red", short(&push.base)), Some("unattributed"))),
            Ci::Unmeasured(why) => Some((format!("the base {} is not proven green: {why}", short(&push.base)), Some("unattributed"))),
        },
    }
}

/// Reverts already queued, and notices not yet sent. Runs every tick.
async fn advance(state: &AppState) {
    type Row = (String, String, String, String, Option<i64>, Option<String>, Option<String>, i64, String);
    let rows: Vec<Row> = state.store.read_async(|c| {
        let mut st = c.prepare("SELECT l.repo, l.red_sha, l.base_sha, l.state, l.revert_land_id, q.state, q.output, l.land_id, l.lanes
            FROM land_red l LEFT JOIN land_queue q ON q.id = l.revert_land_id WHERE l.state = 'reverting'")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    for (repo, tip, base, _, revert, row_state, output, land_id, lanes) in rows {
        let push = Push { repo: repo.clone(), tip: tip.clone(), base, tree: None, done_at: 0.0,
            rows: lanes.split(',').map(|l| Landed { id: land_id, lane: l.to_string(), kind: "land".into(), landed_base: None, landed_tip: None }).collect() };
        match row_state.as_deref() {
            Some("merged") => {
                tracing::warn!(repo, sha = %tip, revert_land_id = ?revert, lanes, measured = true, n_considered = 1, verdict = "land_red_reverted",
                    "the revert landed; main no longer carries the change");
                let (r, t, now) = (repo.clone(), tip.clone(), crate::config::now_f64());
                let done = state.store.write_async(move |conn| {
                    conn.execute("UPDATE land_red SET state = 'reverted', updated_at = ?3 WHERE repo = ?1 AND red_sha = ?2", rusqlite::params![r, t, now])?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                }).await;
                written("reverted", done);
            }
            Some("refused") | Some("withdrawn") | None => {
                tracing::warn!(repo, sha = %tip, revert_land_id = ?revert, measured = true, n_considered = 1, verdict = "land_red_revert_refused",
                    "the queue refused the revert; the lane holds a fix");
                let why = format!("The queue refused the revert:\n{}", output.unwrap_or_default().chars().take(800).collect::<String>());
                // A failed write leaves the episode `reverting`, so the next tick retries.
                let _opened = open_fix(state, &push, &why, &format!("main went red at {} after land {land_id}.", short(&tip))).await;
            }
            _ => {}
        }
    }
    type Note = (i64, String, String, String, String, Option<String>);
    let notes: Vec<Note> = state.store.read_async(|c| {
        let mut st = c.prepare("SELECT id, state, red_sha, lanes, detail, cards FROM land_red WHERE told_at IS NULL AND state IN ('reverted', 'fix_opened')")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<Vec<Note>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    for (id, st, sha, lanes, detail, cards) in notes {
        let text = if st == "reverted" {
            format!("[amux land] your change was reverted from main: it went red at {}.\n\n{detail}\n\nFix it and run `amux land` again.", short(&sha))
        } else {
            format!("[amux land] main is red at {} after your land{}.\n\n{detail}", short(&sha), cards.filter(|c| !c.is_empty()).map(|c| format!(" (card {c})")).unwrap_or_default())
        };
        for lane in lanes.split(',').filter(|l| !l.is_empty()) {
            // An isolated lane takes no injected prompt (A9): its row and the log carry it.
            if !crate::api::session_verbs::session_is_isolated(lane) {
                if let Err(why) = crate::api::session_verbs::steer_enqueue(state, lane, &text, &format!("land-red-{id}"), "harness:land").await {
                    tracing::warn!(lane, id, why, measured = true, n_considered = 1, verdict = "land_red_notice_refused",
                        "a lane could not be told; its card and this row carry the red");
                }
            }
        }
        let now = crate::config::now_f64();
        let done = state.store.write_async(move |conn| {
            conn.execute("UPDATE land_red SET told_at = ?2 WHERE id = ?1", rusqlite::params![id, now])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).await;
        written("told", done);
    }
}

/// One pass. Each repository judges pushes oldest first and stops at the first
/// that is still waiting, so a push is never read out of order.
pub async fn tick(state: &AppState) {
    if switched_off() {
        return;
    }
    advance(state).await;
    let pushes = unsettled(state).await;
    let mut waiting: Vec<String> = Vec::new();
    for push in &pushes {
        if waiting.contains(&push.repo) {
            continue;
        }
        if judge(state, push).await {
            waiting.push(push.repo.clone());
        }
    }
    advance(state).await;
}

static LAST: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Called from the land queue's clock. At most one pass at a time, never in
/// the clock's own turn, because a pass waits on GitHub.
pub fn maybe_tick(state: &AppState) {
    let every = env_value("AMUX_LAND_RED_TICK_S").and_then(|v| v.parse().ok()).filter(|n| *n > 0).unwrap_or(DEFAULT_TICK_S);
    let now = crate::config::now_f64() as u64;
    if now.saturating_sub(LAST.load(Ordering::Relaxed)) < every || RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    let st = state.clone();
    tokio::spawn(async move {
        crate::runtime_jobs::registry::guard_job("land-red", async { tick(&st).await; }).await;
        RUNNING.store(false, Ordering::Release);
    });
}

/// Recent episodes, for GET /api/land.
pub async fn episodes(state: &AppState, since: f64) -> Vec<Value> {
    state.store.read_async(move |c| {
        let mut st = c.prepare("SELECT repo, red_sha, base_sha, land_id, lanes, state, tries, detail, revert_land_id, cards, opened_at, updated_at
            FROM land_red WHERE updated_at >= ?1 ORDER BY id DESC LIMIT 50")?;
        let v = st.query_map([since], |r| Ok(json!({"repo": r.get::<_, String>(0)?, "red_sha": r.get::<_, String>(1)?, "base_sha": r.get::<_, String>(2)?,
            "land_id": r.get::<_, i64>(3)?, "lanes": r.get::<_, String>(4)?, "state": r.get::<_, String>(5)?, "tries": r.get::<_, i64>(6)?,
            "detail": r.get::<_, String>(7)?, "revert_land_id": r.get::<_, Option<i64>>(8)?, "cards": r.get::<_, Option<String>>(9)?,
            "opened_at": r.get::<_, f64>(10)?, "updated_at": r.get::<_, f64>(11)?})))?
            .collect::<rusqlite::Result<Vec<Value>>>()?;
        Ok(v)
    }).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::land_queue::tests::{app, call, commit, fixture, on_main, seed_commit, sh};
    use axum::http::StatusCode;

    const CONF: &str = "[amux]\n\tqueueGate = true\n\tqueueChecks = rust\n\tqueueOnRed = revert\n";

    fn run(name: &str, status: &str, conclusion: &str, attempt: i64) -> Run {
        Run { id: 1, name: name.into(), status: status.into(), conclusion: conclusion.into(), attempt, url: "u".into() }
    }

    #[test]
    fn classify_never_reads_silence_as_green_or_a_cancel_as_red() {
        let need = vec!["rust".to_string(), "checks".to_string()];
        let (ok, bad) = (run("rust", "completed", "success", 1), run("checks", "completed", "failure", 2));
        assert_eq!(classify(&need, &[ok.clone(), run("checks", "completed", "success", 1)], 0.0), Ci::Green);
        assert_eq!(classify(&need, std::slice::from_ref(&ok), 0.0), Ci::Pending, "a required run not started yet");
        assert!(matches!(classify(&need, std::slice::from_ref(&ok), NO_RUN_S + 1.0), Ci::Unmeasured(_)), "it never ran");
        assert_eq!(classify(&need, &[bad.clone(), run("rust", "in_progress", "", 1)], 0.0), Ci::Red(vec![bad.clone()]), "a failure wins");
        assert!(matches!(classify(&need, &[ok.clone(), run("checks", "completed", "cancelled", 1)], 0.0), Ci::Unmeasured(_)));
        assert!(matches!(classify(&need, &[ok, run("checks", "completed", "timed_out", 1)], 0.0), Ci::Unmeasured(_)));
    }

    #[test]
    fn the_newest_push_run_of_a_workflow_is_the_one_read() {
        let t = r#"{"workflow_runs":[
            {"id":1,"name":"rust","status":"completed","conclusion":"failure","run_attempt":1,"event":"push","html_url":"a"},
            {"id":3,"name":"rust","status":"completed","conclusion":"success","run_attempt":1,"event":"push","html_url":"b"},
            {"id":9,"name":"rust","status":"completed","conclusion":"failure","run_attempt":1,"event":"pull_request","html_url":"c"}]}"#;
        let r = parse_runs(t).unwrap();
        assert_eq!((r.len(), r[0].id, r[0].conclusion.as_str()), (1, 3, "success"));
        assert!(parse_runs(r#"{"message":"Bad credentials"}"#).is_err(), "a 401 body is an error, never an empty green");
    }

    #[test]
    fn a_github_remote_gives_its_slug_and_nothing_else_does() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        assert_eq!(slug_of("https://github.com/mixpeek/amux.git").as_deref(), Some("mixpeek/amux"));
        assert_eq!(slug_of("git@github.com:mixpeek/mixpeek.git").as_deref(), Some("mixpeek/mixpeek"));
        assert_eq!(slug_of("/tmp/origin.git"), None);
    }

    /// A temp amux home with a stub `gh` that reads CI answers from files.
    struct Rig {
        d: tempfile::TempDir,
        lane: PathBuf,
        state: AppState,
        _g: crate::api::settings::test_env::HomeGuard,
    }

    impl Rig {
        fn new(conf: &str) -> Self {
            let d = tempfile::tempdir().unwrap();
            let g = crate::api::settings::test_env::set_home(d.path());
            let h = d.path().to_path_buf();
            std::fs::create_dir_all(h.join("sessions")).unwrap();
            std::fs::create_dir_all(h.join("runs")).unwrap();
            let lane = fixture(&h);
            let stub = h.join("gh-stub");
            std::fs::write(&stub, format!("#!/bin/sh\ncase \"$*\" in\n*rerun-failed-jobs*) echo \"$*\" >> {0}/reruns ;;\n*head_sha=*) q=\"$*\"; s=\"${{q#*head_sha=}}\"; s=\"${{s%%&*}}\"; cat {0}/runs/$s.json 2>/dev/null || echo '{{\"workflow_runs\":[]}}' ;;\nesac\n", h.display())).unwrap();
            std::process::Command::new("chmod").arg("+x").arg(&stub).status().unwrap();
            std::fs::write(h.join("server.env"), format!("AMUX_LAND_RED_GH={}\nAMUX_LAND_RED_SLUG=o/r\n", stub.display())).unwrap();
            seed_commit(&h, &lane, &[(".amux/land.conf", conf)]);
            Rig { d, lane, state: app(), _g: g }
        }
        fn h(&self) -> &Path {
            self.d.path()
        }
        fn ci(&self, sha: &str, conclusion: &str, attempt: i64) {
            let (status, c) = if conclusion.is_empty() { ("in_progress", "null".to_string()) } else { ("completed", format!("\"{conclusion}\"")) };
            std::fs::write(self.h().join("runs").join(format!("{sha}.json")), format!(
                "{{\"workflow_runs\":[{{\"id\":7,\"name\":\"rust\",\"status\":\"{status}\",\"conclusion\":{c},\"run_attempt\":{attempt},\"event\":\"push\",\"html_url\":\"http://run\"}}]}}")).unwrap();
        }
        async fn drain(&self) {
            // Generous and re-ticking: this box runs at load 100, and a batch that
            // lost the land lock goes back to queued for the next tick.
            for i in 0..4000 {
                if i % 100 == 0 {
                    crate::api::land_queue::tick(&self.state).await;
                }
                let open: i64 = self.state.store.read().unwrap().query_row("SELECT count(*) FROM land_queue WHERE state IN ('queued','running')", [], |r| r.get(0)).unwrap();
                if open == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            panic!("the queue never drained");
        }
        /// Land commits as one batch; returns (base, tip) of the push.
        async fn land(&self, shas: &[(&str, &str)]) -> (String, String) {
            sh(&self.lane, &["fetch", "-q", "origin", "main"]);
            let base = sh(&self.lane, &["rev-parse", "origin/main"]);
            let tree = self.lane.to_string_lossy().to_string();
            for (who, sha) in shas {
                let (st, b) = call(&self.state, "POST", "/api/land", who, json!({"sha": sha, "tree": tree})).await;
                assert_eq!(st, StatusCode::ACCEPTED, "{b}");
            }
            self.drain().await;
            sh(&self.lane, &["fetch", "-q", "origin", "main"]);
            (base, sh(&self.lane, &["rev-parse", "origin/main"]))
        }
        fn count(&self, sql: &str) -> i64 {
            self.state.store.read().unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
        }
        fn ep(&self) -> String {
            self.state.store.read().unwrap().query_row("SELECT state FROM land_red", [], |r| r.get(0)).unwrap()
        }
        /// Tick until an episode exists. One tick can legitimately wait: a CI
        /// read that does not answer is retried on the next one.
        async fn tick_until_episode(&self) {
            for _ in 0..20 {
                tick(&self.state).await;
                if self.count("SELECT count(*) FROM land_red") > 0 {
                    return;
                }
            }
            panic!("no episode after 20 ticks");
        }
        fn reverts(&self) -> i64 {
            self.count("SELECT count(*) FROM land_queue WHERE kind = 'revert'")
        }
    }

    /// The whole loop: a confirmed red on one land's push builds a revert,
    /// which goes through the same queue and gate and lands; the lane is told
    /// once; every later tick is a no-op.
    #[tokio::test]
    async fn a_confirmed_red_reverts_the_one_land_through_the_queue_exactly_once() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        assert!(on_main(&r.lane).contains("feature.txt"));
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        r.tick_until_episode().await;
        assert_eq!((r.reverts(), r.ep().as_str()), (1, "reverting"));
        let (lane, prio): (String, i64) = r.state.store.read().unwrap().query_row("SELECT lane, priority FROM land_queue WHERE kind='revert'", [], |x| Ok((x.get(0)?, x.get(1)?))).unwrap();
        assert_eq!((lane.as_str(), prio), ("lane-a", 1), "the change's lane, ahead of ordinary waiters");
        r.drain().await;
        assert!(!on_main(&r.lane).contains("feature.txt"), "the change is gone from main");
        let body = sh(&r.lane, &["log", "-1", "--format=%B", "origin/main"]);
        assert!(body.contains("This reverts commit") && body.contains("Revert: ") && body.contains("Amux-Session: amux-land"), "{body}");
        let main = sh(&r.lane, &["rev-parse", "origin/main"]);
        r.ci(&main, "success", 1);
        tick(&r.state).await;
        assert_eq!(r.ep(), "reverted");
        assert_eq!(r.count("SELECT count(*) FROM land_red WHERE told_at IS NOT NULL"), 1);
        for _ in 0..3 {
            tick(&r.state).await;
        }
        assert_eq!(r.reverts(), 1, "never a second revert");
        assert_eq!(sh(&r.lane, &["rev-parse", "origin/main"]), main, "nothing moved");
    }

    /// F1: main goes green while the failed jobs re-run. The re-run still fails,
    /// and nothing is reverted, because the land is checked again before acting.
    #[tokio::test]
    async fn a_main_that_heals_during_the_rerun_is_not_reverted() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 1);
        r.tick_until_episode().await;
        assert_eq!(r.ep(), "rerun");
        seed_commit(r.h(), &r.lane, &[("fix.txt", "y\n")]);
        let main = sh(&r.lane, &["rev-parse", "origin/main"]);
        r.ci(&main, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "healed"));
    }

    /// A first-attempt red only re-runs, once per tick that needs it. A re-run
    /// that passes is flaky and nothing is reverted or filed.
    #[tokio::test]
    async fn a_red_that_passes_its_rerun_is_flaky_and_reverts_nothing() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 1);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "rerun"));
        assert!(std::fs::read_to_string(r.h().join("reruns")).unwrap().contains("runs/7/rerun-failed-jobs"));
        r.ci(&tip, "", 2);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "rerun"), "still running: no verdict");
        r.ci(&tip, "success", 2);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "flaky"));
        assert_eq!(r.count("SELECT count(*) FROM issues WHERE title LIKE 'main is red%'"), 0);
        assert!(on_main(&r.lane).contains("feature.txt"));
    }

    /// A first attempt that fails again on its re-run is confirmed.
    #[tokio::test]
    async fn a_red_that_fails_its_rerun_is_confirmed() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 1);
        tick(&r.state).await;
        assert_eq!(r.reverts(), 0);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!(r.reverts(), 1);
    }

    /// Unattributable: the base is red, or was never proven green. No revert,
    /// no card; main-ci-watch keeps it. One rig per test: each holds the home lock.
    async fn unattributed_when_the_base_says(base_says: &str) {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, base_says, 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "unattributed"), "{base_says}");
        assert_eq!(r.count("SELECT count(*) FROM issues WHERE title LIKE 'main is red%'"), 0);
    }

    #[tokio::test]
    async fn a_red_on_a_red_base_reverts_nothing() {
        unattributed_when_the_base_says("failure").await;
    }

    #[tokio::test]
    async fn a_red_on_a_base_never_proven_green_reverts_nothing() {
        unattributed_when_the_base_says("cancelled").await;
    }

    /// A base still running waits, and a push that CI cannot be read for waits:
    /// neither is a verdict.
    #[tokio::test]
    async fn no_verdict_is_taken_while_ci_cannot_answer() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!(r.count("SELECT count(*) FROM land_red"), 0, "the base is running");
        std::fs::write(r.h().join("runs").join(format!("{tip}.json")), "{\"message\":\"Bad credentials\"}").unwrap();
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.count("SELECT count(*) FROM land_queue WHERE ci_state IS NOT NULL")), (0, 0), "still unjudged");
    }

    /// A newer green main means this red is already over.
    #[tokio::test]
    async fn a_red_main_has_already_moved_past_is_left_alone() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        seed_commit(r.h(), &r.lane, &[("later.txt", "y\n")]);
        let main = sh(&r.lane, &["rev-parse", "origin/main"]);
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        r.ci(&main, "success", 1);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "healed"));
    }

    /// Two entries on one push: CI cannot name one, so each lane gets a fix.
    #[tokio::test]
    async fn a_red_batch_opens_a_fix_for_each_lane_and_reverts_nothing() {
        let r = Rig::new(CONF);
        let a = commit(&r.lane, "a2.txt", "x\n");
        let b = commit(&r.lane, "b2.txt", "y\n");
        let (base, tip) = r.land(&[("lane-a", &a), ("lane-b", &b)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "fix_opened"));
        assert_eq!(r.count("SELECT count(*) FROM issues WHERE title LIKE 'main is red%'"), 2, "one card per lane, once");
        assert_eq!(r.count("SELECT count(*) FROM land_red WHERE told_at IS NOT NULL"), 1);
    }

    #[tokio::test]
    async fn a_repository_that_declares_fix_gets_a_card() {
        let r = Rig::new("[amux]\n\tqueueGate = true\n\tqueueChecks = rust\n");
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "fix_opened"));
    }

    #[tokio::test]
    async fn a_repository_that_declares_off_gets_nothing() {
        let r = Rig::new("[amux]\n\tqueueGate = true\n\tqueueChecks = rust\n\tqueueOnRed = off\n");
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!(r.count("SELECT count(*) FROM land_red"), 0);
        assert_eq!(r.count("SELECT count(*) FROM land_queue WHERE ci_state = 'off'"), 1);
    }

    /// A later change on the same lines: the revert cannot be built cleanly,
    /// so the lane gets a fix and main is untouched.
    #[tokio::test]
    async fn a_revert_that_conflicts_opens_a_fix() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "a.txt", "lane\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        seed_commit(r.h(), &r.lane, &[("a.txt", "later\n")]);
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!((r.reverts(), r.ep().as_str()), (0, "fix_opened"));
    }

    /// A revert goes through the gate like anything else. The gate refuses it
    /// and the lane gets a fix; main keeps the change.
    #[tokio::test]
    async fn a_revert_the_gate_refuses_opens_a_fix() {
        let r = Rig::new("[amux]\n\tqueueGate = test -e feature.txt -o ! -e must-exist.txt\n\tqueueChecks = rust\n\tqueueOnRed = revert\n");
        seed_commit(r.h(), &r.lane, &[("must-exist.txt", "1\n")]);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!(r.reverts(), 1);
        r.drain().await;
        tick(&r.state).await;
        assert_eq!(r.ep(), "fix_opened");
        assert!(on_main(&r.lane).contains("feature.txt"));
        assert_eq!(r.count("SELECT count(*) FROM issues WHERE title LIKE 'main is red%'"), 1);
    }

    /// A revert that lands and goes red is never reverted: no loop.
    #[tokio::test]
    async fn a_revert_is_never_reverted() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        r.drain().await;
        let main = sh(&r.lane, &["rev-parse", "origin/main"]);
        r.ci(&tip, "success", 2); // its base reads green now
        r.ci(&main, "failure", 2);
        tick(&r.state).await;
        tick(&r.state).await;
        assert_eq!(r.reverts(), 1, "the revert's red got a fix, no second revert");
        assert_eq!(r.count("SELECT count(*) FROM land_red WHERE state = 'fix_opened'"), 1);
    }

    /// Past the daily budget a red gets a fix, whatever the repository declares.
    #[tokio::test]
    async fn the_revert_budget_turns_the_next_red_into_a_fix() {
        let r = Rig::new(CONF);
        let repo = std::fs::canonicalize(r.lane.join(".git")).unwrap().to_string_lossy().to_string();
        let now = crate::config::now_f64();
        for i in 0..REVERT_CAP {
            let (rp, n) = (repo.clone(), i);
            r.state.store.write(move |conn| {
                conn.execute("INSERT INTO land_red (repo, red_sha, base_sha, land_id, lanes, state, revert_land_id, opened_at, updated_at, told_at)
                              VALUES (?1, ?2, 'b', 1, 'x', 'reverted', 1, ?3, ?3, ?3)", rusqlite::params![rp, format!("old{n}"), now])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).unwrap();
        }
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        assert_eq!(r.reverts(), 0);
        assert_eq!(r.count("SELECT count(*) FROM land_red WHERE state = 'fix_opened'"), 1);
    }

    #[tokio::test]
    async fn the_switch_stops_the_watch() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        let env = std::fs::read_to_string(r.h().join("server.env")).unwrap();
        std::fs::write(r.h().join("server.env"), format!("{env}AMUX_LAND_RED=0\n")).unwrap();
        tick(&r.state).await;
        assert_eq!(r.count("SELECT count(*) FROM land_red"), 0);
    }

    /// A revert queued by an image that then died finishes: the row and the
    /// episode were one write, so a second pass neither loses nor doubles it.
    #[tokio::test]
    async fn a_queued_revert_survives_a_restart_without_a_second_row() {
        let r = Rig::new(CONF);
        let sha = commit(&r.lane, "feature.txt", "x\n");
        let (base, tip) = r.land(&[("lane-a", &sha)]).await;
        r.ci(&base, "success", 1);
        r.ci(&tip, "failure", 2);
        tick(&r.state).await;
        // The new image sees exactly what the store kept.
        let reopened = r.state.clone();
        tick(&reopened).await;
        assert_eq!(r.reverts(), 1);
        crate::api::land_queue::tick(&reopened).await;
        r.drain().await;
        tick(&reopened).await;
        assert_eq!(r.ep(), "reverted");
        assert!(!on_main(&r.lane).contains("feature.txt"));
    }
}
