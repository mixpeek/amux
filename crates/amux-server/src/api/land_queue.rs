//! Orchestration contract rule 5 (AH-380): no worker pushes to main; the
//! server lands. docs/orchestration-contract.md.
//!
//! A lane enqueues a commit (`POST /api/land`, which `amux land` calls on a
//! lane where the rule is on). The land-queue job takes the queued commits of
//! one repository in order (priority first), composes them onto origin/main in
//! a server-owned detached worktree under ~/.amux/tmp/land by cherry-picking
//! each lane's commits (author, message and trailers kept), runs the lane's
//! `AMUX_LAND_GATE` on the composed tree when one is set, and pushes. The
//! repository's own pre-push hook runs on that push, so it gates the composed
//! tree exactly as it gated a worker's push. A red batch is bisected so one
//! lane's change never refuses another's; a red single commit is returned to
//! its lane with the output, unmerged. A commit that conflicts with main is
//! returned to its lane alone.
//!
//! On the same lanes a direct `git push` to main is refused by the shared git
//! guard, which asks `GET /api/land/policy`.
//!
//! Verdicts (rule 5 at /api/contract/counters): land_queued, land_merged,
//! land_refused, land_batch_bisected, worker_push_refused.
use crate::api::AppState;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RULE: &str = "5";
const DEFAULT_BATCH_MAX: usize = 6;
const GATE_TIMEOUT_S: u64 = 3600;
/// A land still `running` this long after it started died with the server.
const RUNNING_STALE_S: f64 = 2.0 * 3600.0;
const PUSH_TRIES: usize = 3;

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: i64,
    pub repo: String,
    pub lane: String,
    pub sha: String,
}

/// What became of one entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Merged(String),
    Refused(String),
}

fn home() -> PathBuf {
    crate::config::amux_home()
}

pub fn queue_on(lane: &str) -> bool {
    crate::api::contract::rule_on(&home(), lane, RULE)
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    crate::fanout_workspace::git(&dir.to_string_lossy(), args).await
}

async fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", a, b]).await.is_ok()
}

fn tail(s: &str, n: usize) -> String {
    let t: Vec<char> = s.chars().collect();
    t[t.len().saturating_sub(n)..].iter().collect()
}

/// The repository a lane's tree belongs to, as its common git dir: worktrees
/// of one clone share it, so they share one queue.
async fn repo_key(tree: &Path) -> Result<String, String> {
    let d = git(tree, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
    Ok(std::fs::canonicalize(&d).map(|p| p.to_string_lossy().into_owned()).unwrap_or(d))
}

// ---------------------------------------------------------------------------
// Composing and pushing
// ---------------------------------------------------------------------------

enum Attempt {
    /// Pushed: (merged head, entries it carries, per-entry outcomes for the rest).
    Done(Vec<(i64, Outcome)>),
    /// The gate or the push hook refused the composed tree.
    Red(String, Vec<Entry>, Vec<(i64, Outcome)>),
}

/// Compose `entries` onto origin/main in a fresh worktree of `tree`'s repo,
/// gate it, and push. Retries when main moves under the push.
async fn compose(tree: &Path, entries: &[Entry], gate: Option<&str>, prefix: &str) -> Result<Attempt, String> {
    for _ in 0..PUSH_TRIES {
        git(tree, &["fetch", "-q", "origin", "main"]).await?;
        let main = git(tree, &["rev-parse", "origin/main"]).await?;
        let mut out: Vec<(i64, Outcome)> = Vec::new();
        let dir = home().join("tmp").join("land").join(format!("c-{}-{}", std::process::id(), crate::config::now_f64() as u64));
        let _ = std::fs::create_dir_all(dir.parent().unwrap_or(&dir));
        let cand = dir.to_string_lossy().into_owned();
        git(tree, &["worktree", "add", "-q", "--detach", &cand, &main]).await?;
        let mut applied: Vec<Entry> = Vec::new();
        for e in entries {
            if is_ancestor(tree, &e.sha, &main).await {
                out.push((e.id, Outcome::Merged(main.clone())));
                continue;
            }
            let base = match git(tree, &["merge-base", &e.sha, &main]).await {
                Ok(b) => b,
                Err(err) => {
                    out.push((e.id, Outcome::Refused(format!("{} shares no history with origin/main: {err}", e.sha))));
                    continue;
                }
            };
            let merges = git(tree, &["rev-list", "--merges", &format!("{base}..{}", e.sha)]).await.unwrap_or_default();
            if !merges.trim().is_empty() {
                out.push((e.id, Outcome::Refused("the range contains a merge commit; rebase it onto origin/main first".into())));
                continue;
            }
            let before = git(&dir, &["rev-parse", "HEAD"]).await?;
            // Hooks off for the replay only: the push below runs the repo's gate.
            match git(&dir, &["-c", "core.hooksPath=/dev/null", "cherry-pick", "--allow-empty", "--keep-redundant-commits", &format!("{base}..{}", e.sha)]).await {
                Ok(_) => applied.push(e.clone()),
                Err(err) => {
                    let _ = git(&dir, &["cherry-pick", "--abort"]).await;
                    let _ = git(&dir, &["reset", "-q", "--hard", &before]).await;
                    out.push((e.id, Outcome::Refused(format!("conflicts with origin/main {}: rebase onto origin/main and land again. {}", &main[..12.min(main.len())], tail(&err, 600)))));
                }
            }
        }
        let finish = |dir: PathBuf| async move {
            let _ = git(tree, &["worktree", "remove", "--force", &dir.to_string_lossy()]).await;
        };
        if applied.is_empty() {
            finish(dir).await;
            return Ok(Attempt::Done(out));
        }
        if let Some(g) = gate.filter(|g| !g.trim().is_empty()) {
            let (ok, text) = crate::api::contract::sh(&dir, g, prefix, Duration::from_secs(GATE_TIMEOUT_S)).await;
            if !ok {
                finish(dir).await;
                return Ok(Attempt::Red(format!("land gate `{g}` failed on the composed tree:\n{text}"), applied, out));
            }
        }
        let merged = git(&dir, &["rev-parse", "HEAD"]).await?;
        match git(&dir, &["push", "-q", "origin", &format!("{merged}:refs/heads/main")]).await {
            Ok(_) => {
                finish(dir).await;
                out.extend(applied.iter().map(|e| (e.id, Outcome::Merged(merged.clone()))));
                return Ok(Attempt::Done(out));
            }
            Err(err) => {
                finish(dir).await;
                git(tree, &["fetch", "-q", "origin", "main"]).await?;
                if git(tree, &["rev-parse", "origin/main"]).await? != main {
                    continue; // main moved under us: recompose on the new tip
                }
                return Ok(Attempt::Red(format!("push refused (the repository's pre-push gate ran on the composed tree):\n{}", tail(&err, 1800)), applied, out));
            }
        }
    }
    Err(format!("origin/main moved under {PUSH_TRIES} consecutive pushes; the batch stays queued"))
}

/// Land a batch. A red batch of more than one is split in halves until each
/// red commit is alone, so nobody inherits another lane's refusal.
pub async fn land(tree: &Path, entries: Vec<Entry>, gate: Option<&str>, prefix: &str) -> Result<Vec<(i64, Outcome)>, String> {
    let mut stack = vec![entries];
    let mut out = Vec::new();
    while let Some(batch) = stack.pop() {
        match compose(tree, &batch, gate, prefix).await? {
            Attempt::Done(o) => out.extend(o),
            Attempt::Red(why, applied, o) => {
                out.extend(o);
                if applied.len() == 1 {
                    out.push((applied[0].id, Outcome::Refused(why)));
                } else {
                    tracing::info!(batch = applied.len(), measured = true, n_considered = applied.len(),
                        verdict = "land_batch_bisected", "a red land batch was split so each commit is judged on its own");
                    let mid = applied.len() / 2;
                    // Second half pushed first onto the stack, so the earlier
                    // (older) half lands first.
                    stack.push(applied[mid..].to_vec());
                    stack.push(applied[..mid].to_vec());
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The queue
// ---------------------------------------------------------------------------

async fn record(state: &AppState, results: &[(i64, Outcome)], lanes: &HashMap<i64, (String, String)>) {
    let now = crate::config::now_f64();
    for (id, o) in results {
        let (st, merged, output) = match o {
            Outcome::Merged(m) => ("merged", Some(m.clone()), None),
            Outcome::Refused(why) => ("refused", None, Some(why.clone())),
        };
        let (id2, m2, o2) = (*id, merged.clone(), output.clone());
        let _ = state.store.write_async(move |conn| {
            conn.execute("UPDATE land_queue SET state = ?2, merged_sha = ?3, output = ?4, done_at = ?5 WHERE id = ?1",
                rusqlite::params![id2, st, m2, o2, now])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).await;
        let (lane, sha) = lanes.get(id).cloned().unwrap_or_default();
        let text = match o {
            Outcome::Merged(m) => {
                tracing::info!(id, lane, sha, merged = %m, measured = true, n_considered = 1, verdict = "land_merged",
                    "the server landed a queued commit on main");
                format!("[amux land] {} landed on main (as part of {}).", &sha[..12.min(sha.len())], &m[..12.min(m.len())])
            }
            Outcome::Refused(why) => {
                tracing::warn!(id, lane, sha, reason = %tail(why, 300), measured = true, n_considered = 1, verdict = "land_refused",
                    "the server refused a queued commit; nothing was merged");
                format!("[amux land] {} was NOT landed; nothing was merged.\n\n{}\n\nFix it and run `amux land` again.", &sha[..12.min(sha.len())], tail(why, 1500))
            }
        };
        let _ = crate::api::session_verbs::steer_enqueue(state, &lane, &text, "land-queue", "harness:land").await;
    }
}

/// One pass: requeue lands orphaned by a restart, then start one batch per
/// repository that has nothing running. Returns batches started.
pub async fn tick(state: &AppState) -> usize {
    let now = crate::config::now_f64();
    let _ = state.store.write_async(move |conn| {
        let n = conn.execute("UPDATE land_queue SET state = 'queued', started_at = NULL WHERE state = 'running' AND started_at < ?1",
            [now - RUNNING_STALE_S])?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    }).await;
    type Row = (i64, String, String, String);
    let rows: Vec<Row> = state.store.read_async(|c| {
        let mut st = c.prepare("SELECT id, repo, lane, sha FROM land_queue WHERE state = 'queued'
                                AND repo NOT IN (SELECT repo FROM land_queue WHERE state = 'running')
                                ORDER BY priority DESC, id")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let mut by_repo: Vec<(String, Vec<Entry>)> = Vec::new();
    for (id, repo, lane, sha) in rows {
        let e = Entry { id, repo: repo.clone(), lane, sha };
        match by_repo.iter_mut().find(|(r, _)| *r == repo) {
            Some((_, v)) => v.push(e),
            None => by_repo.push((repo, vec![e])),
        }
    }
    let mut started = 0;
    for (_repo, mut entries) in by_repo {
        let first = entries[0].lane.clone();
        let max = crate::api::contract::lane_setting(&home(), &first, "AMUX_LAND_BATCH_MAX")
            .and_then(|v| v.trim().trim_matches('"').parse::<usize>().ok()).filter(|n| *n > 0).unwrap_or(DEFAULT_BATCH_MAX);
        entries.truncate(max);
        let ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
        let ids2 = ids.clone();
        let claimed = state.store.write_async(move |conn| {
            let mut n = 0;
            for id in &ids2 {
                n += conn.execute("UPDATE land_queue SET state = 'running', started_at = ?2 WHERE id = ?1 AND state = 'queued'",
                    rusqlite::params![id, now])?;
            }
            Ok(crate::db::WriteOutcome { applied: n == ids2.len(), events: vec![] })
        }).await;
        if !matches!(claimed, Ok(ref o) if o.applied) {
            continue;
        }
        started += 1;
        let st = state.clone();
        tokio::spawn(async move { run_batch(&st, entries).await });
    }
    started
}

async fn run_batch(state: &AppState, entries: Vec<Entry>) {
    let lanes: HashMap<i64, (String, String)> = entries.iter().map(|e| (e.id, (e.lane.clone(), e.sha.clone()))).collect();
    let first = entries[0].lane.clone();
    let Some(tree) = crate::api::contract::lane_tree(&first) else {
        let r: Vec<(i64, Outcome)> = entries.iter().map(|e| (e.id, Outcome::Refused(format!("lane {} has no checkout the server can land from", e.lane)))).collect();
        record(state, &r, &lanes).await;
        return;
    };
    let gate = crate::api::contract::lane_setting(&home(), &first, "AMUX_LAND_GATE").map(|v| v.trim().trim_matches('"').to_string());
    let prefix = crate::api::contract::verify_path_prefix(&tree, &first);
    match land(&tree, entries.clone(), gate.as_deref(), &prefix).await {
        Ok(results) => record(state, &results, &lanes).await,
        Err(why) => {
            // Not a verdict on any commit: put the batch back.
            tracing::warn!(lane = first, reason = %tail(&why, 300), measured = false, n_considered = entries.len(),
                why_unmeasured = "the land could not complete", "land batch requeued");
            let ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
            let _ = state.store.write_async(move |conn| {
                for id in &ids {
                    conn.execute("UPDATE land_queue SET state = 'queued', started_at = NULL, output = ?2 WHERE id = ?1", rusqlite::params![id, why])?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/land", axum::routing::post(enqueue_route).get(status_route))
        .route("/api/land/policy", axum::routing::get(policy_route))
}

fn caller(headers: &HeaderMap) -> String {
    headers.get("x-amux-session").and_then(|v| v.to_str().ok()).unwrap_or("").trim().to_string()
}

async fn enqueue_route(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let lane = caller(&headers);
    if lane.is_empty() || !queue_on(&lane) {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "land_queue_off",
            "error": "the server land queue (contract rule 5) is not on for this lane; land the ordinary way"}))).into_response();
    }
    let Some(tree) = crate::api::contract::lane_tree(&lane) else {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "land_no_checkout", "error": format!("lane {lane} has no git checkout")}))).into_response();
    };
    let want = body.get("sha").and_then(Value::as_str).unwrap_or("HEAD").to_string();
    let sha = match git(&tree, &["rev-parse", "--verify", "-q", &format!("{want}^{{commit}}")]).await {
        Ok(s) if s.len() == 40 => s,
        _ => return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "code": "land_unknown_commit",
            "error": format!("{want} is not a commit in {lane}'s repository")}))).into_response(),
    };
    let repo = match repo_key(&tree).await {
        Ok(r) => r,
        Err(e) => return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "land_no_repo", "error": e}))).into_response(),
    };
    let priority = body.get("priority").and_then(Value::as_bool).unwrap_or(false);
    let reason = body.get("reason").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty()).map(String::from);
    if priority && reason.is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "code": "land_priority_needs_reason",
            "error": "a priority land needs a reason (it moves ahead of every ordinary waiter)"}))).into_response();
    }
    let now = crate::config::now_f64();
    let (l2, s2, r2, why2) = (lane.clone(), sha.clone(), repo.clone(), reason.clone());
    let id = state.store.write_async(move |conn| {
        conn.execute("INSERT INTO land_queue (repo, lane, sha, priority, reason, queued_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![r2, l2, s2, priority as i64, why2, now])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
    if id.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": "could not queue the land"}))).into_response();
    }
    let (r3, s3) = (repo.clone(), sha.clone());
    let (qid, ahead): (i64, i64) = state.store.read_async(move |c| {
        let id: i64 = c.query_row("SELECT max(id) FROM land_queue WHERE repo = ?1 AND sha = ?2", rusqlite::params![r3, s3], |r| r.get(0))?;
        let ahead: i64 = c.query_row("SELECT count(*) FROM land_queue WHERE repo = ?1 AND state IN ('queued','running') AND id < ?2", rusqlite::params![r3, id], |r| r.get(0))?;
        Ok((id, ahead))
    }).await.unwrap_or((0, 0));
    tracing::info!(id = qid, lane, sha, priority, ahead, measured = true, n_considered = 1, verdict = "land_queued",
        "a lane queued a commit for the server to land");
    (StatusCode::ACCEPTED, Json(json!({"ok": true, "id": qid, "sha": sha, "ahead": ahead,
        "note": "The server composes queued commits onto origin/main, runs the land gate and the repository's pre-push hook, and pushes. You get a message when it lands or is refused."}))).into_response()
}

/// p95 of (done - queued) for lands finished since `since`, in minutes.
pub fn wait_p95(waits_s: &mut [f64]) -> Option<f64> {
    if waits_s.is_empty() {
        return None;
    }
    waits_s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = ((waits_s.len() as f64) * 0.95).ceil() as usize;
    Some(waits_s[i.saturating_sub(1).min(waits_s.len() - 1)] / 60.0)
}

async fn status_route(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let since_h = q.get("since_h").and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0 && *v <= 168.0).unwrap_or(24.0);
    let since = crate::config::now_f64() - since_h * 3600.0;
    type Row = (i64, String, String, String, i64, String, Option<String>, Option<String>, f64, Option<f64>);
    let rows: Vec<Row> = state.store.read_async(move |c| {
        let mut st = c.prepare("SELECT id, repo, lane, sha, priority, state, merged_sha, output, queued_at, done_at FROM land_queue
                                WHERE state IN ('queued','running') OR queued_at >= ?1 ORDER BY id")?;
        let v = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let mut waits: Vec<f64> = rows.iter().filter_map(|r| r.9.filter(|_| r.8 >= since).map(|d| d - r.8)).collect();
    let n = waits.len();
    let p95 = wait_p95(&mut waits);
    let items: Vec<Value> = rows.iter().map(|r| json!({"id": r.0, "repo": r.1, "lane": r.2, "sha": r.3, "priority": r.4 != 0,
        "state": r.5, "merged_sha": r.6, "output": r.7.as_deref().map(|o| tail(o, 600)), "queued_at": r.8, "done_at": r.9})).collect();
    Json(json!({"since_h": since_h, "measured": true, "n_considered": n, "wait_p95_min": p95,
        "why_unmeasured": if n == 0 { Some("no land finished in the window") } else { None },
        "items": items, "contract": "docs/orchestration-contract.md (rule 5)"})).into_response()
}

/// Asked by the shared git guard before a worker push. `push=1` means a push
/// is about to run, so a refusal is logged here (the guard cannot log to the
/// server's verdict stream itself).
async fn policy_route(Query(q): Query<HashMap<String, String>>) -> Response {
    let lane = q.get("lane").map(|s| s.trim().to_string()).unwrap_or_default();
    let queue = !lane.is_empty() && queue_on(&lane);
    if queue && q.get("push").is_some_and(|v| v == "1") {
        tracing::info!(lane, measured = true, n_considered = 1, verdict = "worker_push_refused",
            "a worker push to main was refused; the lane lands through the server queue");
    }
    Json(json!({"lane": lane, "queue": queue, "measured": true, "n_considered": 1,
        "how": "amux land (queues the commit on the server; you get a message when it lands)"})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git").arg("-C").arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"]).args(args).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// origin (bare) plus one clone, with identity set so cherry-pick works.
    fn fixture(d: &Path) -> PathBuf {
        let origin = d.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        sh(&origin, &["init", "-q", "--bare", "-b", "main"]);
        let seed = d.join("seed");
        std::process::Command::new("git").args(["clone", "-q"]).arg(&origin).arg(&seed).status().unwrap();
        std::fs::write(seed.join("a.txt"), "1\n").unwrap();
        sh(&seed, &["add", "a.txt"]);
        sh(&seed, &["commit", "-qm", "base"]);
        sh(&seed, &["push", "-q", "origin", "HEAD:main"]);
        let lane = d.join("lane");
        std::process::Command::new("git").args(["clone", "-q"]).arg(&origin).arg(&lane).status().unwrap();
        sh(&lane, &["config", "user.email", "t@t"]);
        sh(&lane, &["config", "user.name", "t"]);
        lane
    }

    fn commit(tree: &Path, file: &str, body: &str) -> String {
        sh(tree, &["checkout", "-q", "--detach", "origin/main"]);
        std::fs::write(tree.join(file), body).unwrap();
        sh(tree, &["add", file]);
        sh(tree, &["commit", "-qm", &format!("change {file}")]);
        sh(tree, &["rev-parse", "HEAD"])
    }

    /// The core of rule 5: a green batch lands together; a red one is bisected
    /// so the good commits still land and only the red one is refused, with
    /// nothing of it merged; a conflicting commit is refused alone.
    #[tokio::test]
    async fn a_red_batch_is_bisected_so_only_the_red_commit_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        let a = commit(&lane, "b.txt", "ok\n");
        let bad = commit(&lane, "bad.txt", "x\n");
        let c = commit(&lane, "c.txt", "ok\n");
        let clash = commit(&lane, "a.txt", "conflict\n");
        // A main-side change the `clash` commit conflicts with.
        sh(&lane, &["checkout", "-q", "--detach", "origin/main"]);
        std::fs::write(lane.join("a.txt"), "main moved\n").unwrap();
        sh(&lane, &["commit", "-qam", "main moves a.txt"]);
        sh(&lane, &["push", "-q", "origin", "HEAD:main"]);
        let entries: Vec<Entry> = [(1, &a), (2, &bad), (3, &c), (4, &clash)].iter()
            .map(|(i, s)| Entry { id: *i, repo: "r".into(), lane: "l".into(), sha: (*s).clone() }).collect();
        let gate = "test ! -e bad.txt";
        let out: HashMap<i64, Outcome> = land(&lane, entries, Some(gate), "").await.unwrap().into_iter().collect();
        assert!(matches!(out[&1], Outcome::Merged(_)), "{:?}", out[&1]);
        assert!(matches!(out[&3], Outcome::Merged(_)), "{:?}", out[&3]);
        match &out[&2] {
            Outcome::Refused(why) => assert!(why.contains("land gate"), "{why}"),
            o => panic!("the red commit must be refused: {o:?}"),
        }
        match &out[&4] {
            Outcome::Refused(why) => assert!(why.contains("conflicts with origin/main"), "{why}"),
            o => panic!("the conflicting commit must be refused: {o:?}"),
        }
        sh(&lane, &["fetch", "-q", "origin"]);
        let files = sh(&lane, &["ls-tree", "--name-only", "origin/main"]);
        assert!(files.contains("b.txt") && files.contains("c.txt"), "{files}");
        assert!(!files.contains("bad.txt"), "nothing of a refused commit is merged: {files}");
        assert_eq!(std::fs::read_to_string(lane.join("a.txt")).unwrap(), "main moved\n");
        // An already-landed commit is reported merged without a second push.
        let again = land(&lane, vec![Entry { id: 9, repo: "r".into(), lane: "l".into(), sha: a.clone() }], Some(gate), "").await.unwrap();
        assert!(matches!(again[0].1, Outcome::Merged(_)));
    }

    fn app() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("land-test.db")).unwrap();
        std::mem::forget(dir);
        AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    async fn call(state: &AppState, method: &str, uri: &str, lane: &str, body: Value) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder().method(method).uri(uri)
            .header("content-type", "application/json").header("x-amux-session", lane)
            .body(axum::body::Body::from(body.to_string())).unwrap();
        let r = routes().with_state(state.clone()).oneshot(req).await.unwrap();
        let st = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    /// Through the real routes and job: off unless rule 5 is on for the lane;
    /// on, a queued commit is landed by the server and the wait is measured;
    /// the push policy names the queue for the lane.
    #[tokio::test]
    async fn a_queued_commit_is_landed_by_the_server_only_where_rule_5_is_on() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let lane = fixture(h);
        let sha = commit(&lane, "feature.txt", "x\n");
        let state = app();
        std::fs::write(h.join("sessions/lane-q.env"), format!("CC_DIR=\"{}\"\n", lane.display())).unwrap();
        let (st, b) = call(&state, "POST", "/api/land", "lane-q", json!({"sha": sha})).await;
        assert_eq!((st, b["code"].as_str()), (StatusCode::CONFLICT, Some("land_queue_off")), "off unless the rule is on");
        assert_eq!(call(&state, "GET", "/api/land/policy?lane=lane-q&push=1", "", json!(null)).await.1["queue"], false);

        std::fs::write(h.join("sessions/lane-q.env"), format!("CC_DIR=\"{}\"\nAMUX_CONTRACT_DONE=1\n", lane.display())).unwrap();
        assert_eq!(call(&state, "GET", "/api/land/policy?lane=lane-q&push=1", "", json!(null)).await.1["queue"], true);
        let (st, b) = call(&state, "POST", "/api/land", "lane-q", json!({"sha": sha})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
        assert_eq!(tick(&state).await, 1);
        for _ in 0..200 {
            let (_, s) = call(&state, "GET", "/api/land", "", json!(null)).await;
            if s["items"][0]["state"] == "merged" {
                assert_eq!(s["n_considered"], 1);
                assert!(s["wait_p95_min"].is_number(), "{s}");
                sh(&lane, &["fetch", "-q", "origin"]);
                assert!(sh(&lane, &["ls-tree", "--name-only", "origin/main"]).contains("feature.txt"));
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the queued commit never landed");
    }

    #[test]
    fn the_wait_p95_is_the_95th_percentile_in_minutes() {
        let mut w: Vec<f64> = (1..=20).map(|m| m as f64 * 60.0).collect();
        assert_eq!(wait_p95(&mut w), Some(19.0));
        assert_eq!(wait_p95(&mut []), None, "no finished land is unmeasured, never zero");
    }
}
