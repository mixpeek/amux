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
//! The server takes the same land lock `amux land` and the repository's
//! pre-push land-lock leg use (~/.amux/locks/land-<16 hex of sha1("<url>
//! main")>, holder pid in `pid`), and pushes with AMUX_LAND_HOLDER_PID set to
//! its own pid, so a client land and a server land never push past each
//! other (MO-4396, 2026-10-06: land 5 was refused while gs12-data held it).
//!
//! Verdicts (rule 5 at /api/contract/counters): land_queued, land_merged,
//! land_refused, land_batch_bisected, worker_push_refused, land_lock_acquired,
//! land_lock_waited, land_lock_timeout_requeued.
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
// The client land lock (MO-4396)
// ---------------------------------------------------------------------------

pub const LOCK_WHO: &str = "amux-server (land queue)";
const LOCK_WAIT_S: u64 = 600;
const LOCK_POLL: Duration = Duration::from_secs(5);
/// A lock directory with no pid yet is a holder between its mkdir and its
/// first write; only this old is it abandoned.
const LOCK_NO_PID_STALE_S: u64 = 60;

/// The lock `amux land` names: first 16 hex of sha1("<remote url> <branch>").
pub fn lock_path(url: &str, branch: &str) -> PathBuf {
    use sha1::{Digest, Sha1};
    let h = hex::encode(Sha1::digest(format!("{url} {branch}").as_bytes()));
    home().join("locks").join(format!("land-{}", &h[..16]))
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn read_pid(lock: &Path) -> Option<i32> {
    let t = std::fs::read_to_string(lock.join("pid")).ok()?;
    t.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}

/// Land locks and queue rows THIS process image is working. A self-adoption
/// swap execs a new image under the SAME pid, so a lock naming our pid looked
/// held forever while the task holding it had died with the old image
/// (2026-10-07: taken 07:37:57Z, swap 07:39:13Z, 11 lands waited 30 minutes
/// behind it). A lock or running row naming us that is not in these sets was
/// left by an earlier image and is reclaimed at once.
static LIVE_LOCKS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(Default::default);
static LIVE_ROWS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<i64>>> =
    std::sync::LazyLock::new(Default::default);

fn lock_is_live_here(path: &Path) -> bool {
    LIVE_LOCKS.lock().map(|s| s.contains(path)).unwrap_or(true)
}

/// A held land lock. Dropping it removes the lock only while this process
/// still owns it, on every path out of a land, an error or a panic included.
pub struct LandLock {
    pub path: PathBuf,
    pub pid: u32,
}

impl Drop for LandLock {
    fn drop(&mut self) {
        if read_pid(&self.path) == Some(self.pid as i32) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
        if let Ok(mut s) = LIVE_LOCKS.lock() {
            s.remove(&self.path);
        }
    }
}

pub enum LockWait {
    Acquired(LandLock),
    TimedOut(String),
}

/// Take the land lock the way `amux land` does (mkdir, then pid, since, who),
/// taking over a lock whose holder is gone, and waiting up to `wait` for a
/// live holder.
pub async fn acquire_lock(path: &Path, wait: Duration, poll: Duration) -> LockWait {
    let me = std::process::id();
    let deadline = std::time::Instant::now() + wait;
    let mut announced = false;
    loop {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::create_dir(path).is_ok() {
            let now = crate::config::now_f64() as u64;
            let _ = std::fs::write(path.join("pid"), format!("{me}\n"));
            let _ = std::fs::write(path.join("since"), format!("{now}\n"));
            let _ = std::fs::write(path.join("who"), format!("{LOCK_WHO}\n"));
            tracing::info!(lock = %path.display(), pid = me, measured = true, n_considered = 1,
                verdict = "land_lock_acquired", "the server took the land lock");
            if let Ok(mut s) = LIVE_LOCKS.lock() {
                s.insert(path.to_path_buf());
            }
            return LockWait::Acquired(LandLock { path: path.to_path_buf(), pid: me });
        }
        let hpid = read_pid(path);
        let who = std::fs::read_to_string(path.join("who")).unwrap_or_default().trim().to_string();
        let age = std::fs::metadata(path).and_then(|m| m.modified()).ok()
            .and_then(|t| t.elapsed().ok()).map(|d| d.as_secs()).unwrap_or(0);
        // Our own pid on a server lock this image does not hold: an earlier
        // image of this process took it and died in a self-adoption swap.
        let orphaned_by_swap = hpid == Some(me as i32) && who == LOCK_WHO && !lock_is_live_here(path);
        let stale = orphaned_by_swap || match hpid {
            Some(p) => !pid_alive(p),
            None => age > LOCK_NO_PID_STALE_S,
        };
        if orphaned_by_swap {
            tracing::warn!(lock = %path.display(), pid = me, measured = true, n_considered = 1,
                verdict = "land_lock_orphaned_by_swap", "a land lock an earlier image of this server held is reclaimed");
        }
        if stale {
            tracing::warn!(lock = %path.display(), holder = %who, holder_pid = ?hpid, measured = true, n_considered = 1,
                "taking over a stale land lock (its holder is gone)");
            let _ = std::fs::remove_dir_all(path);
            continue;
        }
        let holder = format!("{} (pid {})", if who.is_empty() { "?" } else { &who }, hpid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()));
        if !announced {
            tracing::info!(lock = %path.display(), holder = %holder, measured = true, n_considered = 1,
                verdict = "land_lock_waited", "the land lock is held; the server waits for it");
            announced = true;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(lock = %path.display(), holder = %holder, waited_s = wait.as_secs(), measured = true, n_considered = 1,
                verdict = "land_lock_timeout_requeued", "the land lock stayed held; the batch goes back to the queue");
            return LockWait::TimedOut(holder);
        }
        tokio::time::sleep(poll).await;
    }
}

/// Push as the lock holder, so the repository's pre-push land-lock leg
/// recognises this push as the holder's own.
async fn push(dir: &Path, refspec: &str, lock: Option<&LandLock>) -> Result<String, String> {
    let Some(l) = lock else {
        return git(dir, &["push", "-q", "origin", refspec]).await;
    };
    let run = tokio::process::Command::new("git").arg("-C").arg(dir).args(["push", "-q", "origin", refspec])
        .env("AMUX_LAND_HOLDER_PID", l.pid.to_string()).env("AMUX_LAND_LOCK", &l.path)
        .kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(1800), run).await {
        Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(Ok(o)) => Err(format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string()),
        Ok(Err(e)) => Err(format!("could not run git push: {e}")),
        Err(_) => Err("git push timed out after 1800s".into()),
    }
}

// ---------------------------------------------------------------------------
// Composing and pushing
// ---------------------------------------------------------------------------

enum Attempt {
    /// Main moved during push: cleanup, then compose the same batch again.
    Retry,
    /// Pushed: (merged head, entries it carries, per-entry outcomes for the rest).
    Done(Vec<(i64, Outcome)>),
    /// The gate or the push hook refused the composed tree.
    Red(String, Vec<Entry>, Vec<(i64, Outcome)>),
}

async fn cleanup_candidate(tree: &Path, dir: &Path) {
    // Only the server-created candidate is removed. Double force is required
    // for git's "locked initializing" residue after a checkout timeout.
    match git(tree, &["worktree", "remove", "--force", "--force", &dir.to_string_lossy()]).await {
        Ok(_) => tracing::info!(candidate = %dir.display(), measured = true, n_considered = 1,
            verdict = "land_candidate_removed", "land candidate removed"),
        Err(why) => tracing::warn!(candidate = %dir.display(), reason = %why, measured = false, n_considered = 1,
            verdict = "land_candidate_cleanup_failed", "land candidate cleanup could not be confirmed"),
    }
}

/// A positive remote receipt also identifies the abandoned composed checkout.
/// Only this repository's registered server candidates at that exact HEAD are
/// removed, while its land lock is held. Unknown/incomplete candidates stay put.
async fn cleanup_accepted_candidate(tree: &Path, accepted: &str, lock: Option<&LandLock>) {
    if lock.is_none() { return; }
    let Ok(list) = git(tree, &["worktree", "list", "--porcelain"]).await else { return; };
    let Ok(parent) = std::fs::canonicalize(home().join("tmp").join("land")) else { return; };
    for block in list.split("\n\n") {
        let path = block.lines().find_map(|l| l.strip_prefix("worktree "));
        let head = block.lines().find_map(|l| l.strip_prefix("HEAD "));
        let Some(path) = path.filter(|_| head == Some(accepted)) else { continue; };
        let dir = Path::new(path);
        if dir.parent().and_then(|p| std::fs::canonicalize(p).ok()).as_deref() == Some(parent.as_path())
            && dir.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("c-"))
            && !dir.is_symlink()
        {
            cleanup_candidate(tree, dir).await;
        }
    }
}

async fn adopt_push_receipts(tree: &Path, entries: &[Entry], main: &str, lock: Option<&LandLock>, state: Option<&AppState>) -> Result<(Vec<Entry>, Vec<(i64, Outcome)>), String> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    if let Some(state) = state {
        use rusqlite::OptionalExtension;
        for entry in entries.iter().cloned() {
            let id = entry.id;
            let candidate: Option<String> = state.store.read_async(move |conn| {
                Ok(conn.query_row("SELECT merged_sha FROM land_queue WHERE id=?1", [id], |r| r.get(0)).optional()?.flatten())
            }).await.map_err(|e| e.to_string())?;
            if let Some(candidate) = candidate {
                if is_ancestor(tree, &candidate, main).await {
                    tracing::info!(id, candidate, measured = true, n_considered = 1, verdict = "land_push_receipt_adopted",
                        "remote ancestry proves the accepted push; do not replay its rebased commits");
                    cleanup_accepted_candidate(tree, &candidate, lock).await;
                    out.push((id, Outcome::Merged(candidate)));
                    continue;
                }
            }
            pending.push(entry);
        }
    } else { pending = entries.to_vec(); }
    Ok((pending, out))
}

async fn compose(tree: &Path, entries: &[Entry], gate: Option<&str>, prefix: &str, lock: Option<&LandLock>, state: Option<&AppState>) -> Result<Attempt, String> {
    for _ in 0..PUSH_TRIES {
        git(tree, &["fetch", "-q", "origin", "main"]).await?;
        let main = git(tree, &["rev-parse", "origin/main"]).await?;
        // Reconcile before every retry as well as after controller recovery.
        let (pending, mut adopted) = adopt_push_receipts(tree, entries, &main, lock, state).await?;
        if pending.is_empty() { return Ok(Attempt::Done(adopted)); }
        let dir = home().join("tmp").join("land").join(format!("c-{}-{}", std::process::id(), ulid::Ulid::new()));
        std::fs::create_dir_all(dir.parent().unwrap_or(&dir)).map_err(|e| e.to_string())?;
        let result = compose_candidate(tree, &pending, gate, prefix, lock, (&dir, &main), state).await;
        cleanup_candidate(tree, &dir).await;
        match result {
            Ok(Attempt::Retry) => continue,
            Ok(Attempt::Done(done)) => { adopted.extend(done); return Ok(Attempt::Done(adopted)); }
            Ok(Attempt::Red(why, pending, done)) => { adopted.extend(done); return Ok(Attempt::Red(why, pending, adopted)); }
            Err(why) => return Err(why),
        }
    }
    Err(format!("origin/main moved under {PUSH_TRIES} consecutive pushes; the batch stays queued"))
}

async fn compose_candidate(tree: &Path, entries: &[Entry], gate: Option<&str>, prefix: &str, lock: Option<&LandLock>, candidate: (&Path, &str), state: Option<&AppState>) -> Result<Attempt, String> {
    let (dir, main) = candidate;
    let cand = dir.to_string_lossy().into_owned();
    if let Err(why) = git(tree, &["worktree", "add", "-q", "--detach", &cand, main]).await {
        tracing::warn!(candidate = %dir.display(), reason = %why, measured = true, n_considered = 1,
            verdict = "land_candidate_start_failed", "land candidate could not start; cleanup runs before requeue");
        return Err(why);
    }
    let mut out: Vec<(i64, Outcome)> = Vec::new();
    let mut applied: Vec<Entry> = Vec::new();
    for e in entries {
        if is_ancestor(tree, &e.sha, main).await {
            out.push((e.id, Outcome::Merged(main.to_string())));
            continue;
        }
        let base = match git(tree, &["merge-base", &e.sha, main]).await {
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
        let before = git(dir, &["rev-parse", "HEAD"]).await?;
        // Hooks off for the replay only: the push below runs the repo's gate.
        match git(dir, &["-c", "core.hooksPath=/dev/null", "cherry-pick", "--allow-empty", "--keep-redundant-commits", &format!("{base}..{}", e.sha)]).await {
            Ok(_) => applied.push(e.clone()),
            Err(err) => {
                let _ = git(dir, &["cherry-pick", "--abort"]).await;
                let _ = git(dir, &["reset", "-q", "--hard", &before]).await;
                out.push((e.id, Outcome::Refused(format!("conflicts with origin/main {}: rebase onto origin/main and land again. {}", &main[..12.min(main.len())], tail(&err, 600)))));
            }
        }
    }
    if applied.is_empty() {
        return Ok(Attempt::Done(out));
    }
    if let Some(g) = gate.filter(|g| !g.trim().is_empty()) {
        let (ok, text) = crate::api::contract::sh(dir, g, prefix, Duration::from_secs(GATE_TIMEOUT_S)).await;
        if !ok {
            return Ok(Attempt::Red(format!("land gate `{g}` failed on the composed tree:\n{text}"), applied, out));
        }
    }
    let merged = git(dir, &["rev-parse", "HEAD"]).await?;
    if let Some(state) = state {
        let ids: Vec<i64> = applied.iter().map(|e| e.id).collect();
        let intended = merged.clone();
        let saved = state.store.write_async(move |conn| {
            for id in &ids {
                if conn.execute("UPDATE land_queue SET merged_sha=?2 WHERE id=?1 AND state='running'", rusqlite::params![id, intended])? != 1 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).await.map_err(|e| format!("land push intent was not persisted; no push attempted: {e}"))?;
        if !saved.applied { return Err("land push intent was not applied; no push attempted".into()); }
        tracing::info!(candidate = %merged, measured = true, n_considered = applied.len(),
            verdict = "land_push_intent_saved", "exact composed SHA persisted before remote push");
    }
    match push(dir, &format!("{merged}:refs/heads/main"), lock).await {
        Ok(_) => {
            out.extend(applied.iter().map(|e| (e.id, Outcome::Merged(merged.clone()))));
            Ok(Attempt::Done(out))
        }
        Err(err) => {
            git(tree, &["fetch", "-q", "origin", "main"]).await?;
            if git(tree, &["rev-parse", "origin/main"]).await? != main {
                return Ok(Attempt::Retry); // recompose on the new tip
            }
            Ok(Attempt::Red(format!("push refused (the repository's pre-push gate ran on the composed tree):\n{}", tail(&err, 1800)), applied, out))
        }
    }
}

/// Land a batch. A red batch of more than one is split in halves until each
/// red commit is alone, so nobody inherits another lane's refusal.
pub async fn land(tree: &Path, entries: Vec<Entry>, gate: Option<&str>, prefix: &str, lock: Option<&LandLock>) -> Result<Vec<(i64, Outcome)>, String> {
    land_with_receipts(tree, entries, gate, prefix, lock, None).await
}

async fn land_with_receipts(tree: &Path, entries: Vec<Entry>, gate: Option<&str>, prefix: &str, lock: Option<&LandLock>, state: Option<&AppState>) -> Result<Vec<(i64, Outcome)>, String> {
    let mut stack = vec![entries];
    let mut out = Vec::new();
    while let Some(batch) = stack.pop() {
        match compose(tree, &batch, gate, prefix, lock, state).await? {
            Attempt::Retry => unreachable!("compose consumes retries"),
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
    // Running rows this image is not working were started by an earlier image
    // (a self-adoption swap ends the task): requeue them now, not after
    // RUNNING_STALE_S.
    let live: Vec<i64> = LIVE_ROWS.lock().map(|s| s.iter().copied().collect()).unwrap_or_default();
    let _ = state.store.write_async(move |conn| {
        let mut n = conn.execute("UPDATE land_queue SET state = 'queued', started_at = NULL WHERE state = 'running' AND started_at < ?1",
            [now - RUNNING_STALE_S])?;
        let running: Vec<i64> = {
            let mut st = conn.prepare("SELECT id FROM land_queue WHERE state = 'running'")?;
            let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            v
        };
        for id in running.into_iter().filter(|id| !live.contains(id)) {
            n += conn.execute("UPDATE land_queue SET state = 'queued', started_at = NULL WHERE id = ?1 AND state = 'running'", [id])?;
            tracing::warn!(id, measured = true, n_considered = 1, verdict = "land_row_orphaned_by_swap",
                "a land an earlier image of this server was running is requeued");
        }
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
        if let Ok(mut s) = LIVE_ROWS.lock() {
            s.extend(ids.iter().copied());
        }
        let st = state.clone();
        tokio::spawn(async move {
            crate::runtime_jobs::registry::guard_job("land-batch", async { run_batch(&st, entries).await; }).await;
            if let Ok(mut s) = LIVE_ROWS.lock() {
                for id in &ids {
                    s.remove(id);
                }
            }
        });
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
    // The same lock a client `amux land` holds, for the whole batch (gate,
    // push and any bisect), so neither pushes past the other (MO-4396).
    let wait = crate::api::contract::lane_setting(&home(), &first, "AMUX_LAND_LOCK_WAIT_S")
        .and_then(|v| v.trim().trim_matches('"').parse::<u64>().ok()).unwrap_or(LOCK_WAIT_S);
    let lock = match git(&tree, &["remote", "get-url", "origin"]).await {
        Ok(url) => match acquire_lock(&lock_path(&url, "main"), Duration::from_secs(wait), LOCK_POLL).await {
            LockWait::Acquired(l) => Some(l),
            LockWait::TimedOut(holder) => {
                requeue(state, &entries, format!("the land lock was held by {holder} for {wait}s; requeued")).await;
                return;
            }
        },
        Err(_) => None,
    };
    let result = land_with_receipts(&tree, entries.clone(), gate.as_deref(), &prefix, lock.as_ref(), Some(state)).await;
    drop(lock);
    match result {
        Ok(results) => record(state, &results, &lanes).await,
        Err(why) => {
            // Not a verdict on any commit: put the batch back.
            tracing::warn!(lane = first, reason = %tail(&why, 300), measured = false, n_considered = entries.len(),
                why_unmeasured = "the land could not complete", "land batch requeued");
            requeue(state, &entries, why).await;
        }
    }
}

/// Put a batch back in the queue without a verdict on any commit.
async fn requeue(state: &AppState, entries: &[Entry], why: String) {
    let ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
    let _ = state.store.write_async(move |conn| {
        for id in &ids {
            conn.execute("UPDATE land_queue SET state = 'queued', started_at = NULL, output = ?2 WHERE id = ?1", rusqlite::params![id, why])?;
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/land", axum::routing::post(enqueue_route).get(status_route))
        .route("/api/land/policy", axum::routing::get(policy_route))
        .route("/api/land/{id}", axum::routing::delete(withdraw_route))
}

/// DELETE /api/land/<id>: withdraw one QUEUED entry, by the lane that queued
/// it. A running land is never touched here (gs12-extra-2, 2026-10-07: the
/// only way back was `amux land --cancel`, which stops every land of the lane,
/// one mid-attempt included).
async fn withdraw_route(State(state): State<AppState>, headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<i64>) -> Response {
    let lane = caller(&headers);
    if lane.is_empty() {
        return (StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": "land_withdraw_needs_lane",
            "error": "send X-Amux-Session: only the lane that queued an entry may withdraw it"}))).into_response();
    }
    let row: Option<(String, String)> = state.store.read_async(move |c| {
        use rusqlite::OptionalExtension;
        Ok(c.query_row("SELECT lane, state FROM land_queue WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }).await.ok().flatten();
    let Some((owner, st)) = row else {
        return (StatusCode::NOT_FOUND, Json(json!({"ok": false, "code": "land_unknown_id", "error": format!("no land queue entry {id}")}))).into_response();
    };
    if owner != lane {
        return (StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": "land_withdraw_not_yours",
            "error": format!("entry {id} was queued by {owner}")}))).into_response();
    }
    let l2 = lane.clone();
    let done = state.store.write_async(move |conn| {
        let n = conn.execute("UPDATE land_queue SET state = 'withdrawn' WHERE id = ?1 AND lane = ?2 AND state = 'queued'",
            rusqlite::params![id, l2])?;
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    }).await;
    if !matches!(done, Ok(ref o) if o.applied) {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "land_not_queued",
            "error": format!("entry {id} is {st}; only a queued entry can be withdrawn"), "state": st}))).into_response();
    }
    tracing::info!(id, lane, measured = true, n_considered = 1, verdict = "land_withdrawn", "a lane withdrew one queued land");
    (StatusCode::OK, Json(json!({"ok": true, "id": id, "state": "withdrawn"}))).into_response()
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
        "state": r.5, "merged_sha": if r.5 == "merged" { r.6.as_deref() } else { None },
        "push_candidate_sha": if matches!(r.5.as_str(), "queued" | "running") { r.6.as_deref() } else { None }, "output": r.7.as_deref().map(|o| tail(o, 600)), "queued_at": r.8, "done_at": r.9})).collect();
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
        let out: HashMap<i64, Outcome> = land(&lane, entries, Some(gate), "", None).await.unwrap().into_iter().collect();
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
        let again = land(&lane, vec![Entry { id: 9, repo: "r".into(), lane: "l".into(), sha: a.clone() }], Some(gate), "", None).await.unwrap();
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

    /// One queued entry is withdrawn by its own lane; another lane, a running
    /// entry and a second withdraw are refused (gs12-extra-2, 2026-10-07).
    #[tokio::test]
    async fn a_lane_withdraws_one_queued_land_and_nothing_else() {
        let state = app();
        state.store.write(|conn| {
            conn.execute("INSERT INTO land_queue (id, repo, lane, sha, priority, queued_at, state) VALUES (41, 'r', 'lane-w', 'a', 0, 1, 'queued')", [])?;
            conn.execute("INSERT INTO land_queue (id, repo, lane, sha, priority, queued_at, state) VALUES (42, 'r', 'lane-w', 'b', 0, 1, 'running')", [])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        assert_eq!(call(&state, "DELETE", "/api/land/41", "someone-else", json!(null)).await.0, StatusCode::FORBIDDEN);
        assert_eq!(call(&state, "DELETE", "/api/land/41", "", json!(null)).await.0, StatusCode::FORBIDDEN);
        let (st, b) = call(&state, "DELETE", "/api/land/41", "lane-w", json!(null)).await;
        assert_eq!((st, b["state"].as_str()), (StatusCode::OK, Some("withdrawn")), "{b}");
        assert_eq!(call(&state, "DELETE", "/api/land/41", "lane-w", json!(null)).await.0, StatusCode::CONFLICT, "already withdrawn");
        assert_eq!(call(&state, "DELETE", "/api/land/42", "lane-w", json!(null)).await.0, StatusCode::CONFLICT, "a running land is untouched");
        assert_eq!(call(&state, "DELETE", "/api/land/99", "lane-w", json!(null)).await.0, StatusCode::NOT_FOUND);
        let states: Vec<String> = state.store.read().unwrap().prepare("SELECT state FROM land_queue ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().flatten().collect();
        assert_eq!(states, vec!["withdrawn", "running"]);
    }

    /// Through the real routes and job: off unless rule 5 is on for the lane;
    /// on, a queued commit is landed by the server and the wait is measured;
    /// the push policy names the queue for the lane.
    #[tokio::test]
    async fn failed_checkout_cleanup_removes_a_locked_candidate_and_releases_the_land_lock() {
        let d = tempfile::tempdir().unwrap();
        let _guard = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        let sha = commit(&lane, "feature.txt", "x\n");
        let hook = lane.join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\ngit worktree lock --reason initializing \"$PWD\"\necho fixture-checkout-failed >&2\nexit 37\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = d.path().join("locks/failed-candidate");
        let lock = match acquire_lock(&path, Duration::from_millis(100), Duration::from_millis(10)).await {
            LockWait::Acquired(lock) => lock,
            LockWait::TimedOut(_) => panic!("private lock unexpectedly held"),
        };
        let result = land(&lane, vec![Entry { id:1,repo:"r".into(),lane:"l".into(),sha }], None, "", Some(&lock)).await;
        assert!(result.unwrap_err().contains("fixture-checkout-failed"));
        assert!(!sh(&lane, &["worktree","list","--porcelain"]).contains("locked initializing"));
        assert_eq!(std::fs::read_dir(d.path().join("tmp/land")).unwrap().count(), 0);
        drop(lock);
        assert!(!path.exists());
        assert!(!sh(&lane, &["ls-tree","--name-only","origin/main"]).contains("feature.txt"));
    }

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

    /// The server names the lock exactly as `amux land` and the Mixpeek
    /// pre-push leg do: printf '%s %s' "$url" main | shasum | cut -c1-16.
    #[test]
    fn the_lock_path_matches_the_cli_key() {
        let p = lock_path("https://github.com/mixpeek/mixpeek.git", "main");
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "land-329fce997bdd914f");
    }

    /// Free: taken, pid/since/who written, released on drop. Live holder: the
    /// server waits, then times out (requeue) and leaves the lock alone. Dead
    /// holder: taken over.
    /// A self-adoption swap keeps the pid: a lock naming this server that this
    /// image does not hold is reclaimed at once; one it does hold still waits.
    #[tokio::test]
    async fn a_lock_an_earlier_image_of_this_server_held_is_reclaimed() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let path = d.path().join("locks").join("land-swap");
        let fast = Duration::from_millis(50);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("pid"), format!("{}\n", std::process::id())).unwrap();
        std::fs::write(path.join("who"), format!("{LOCK_WHO}\n")).unwrap();
        let held = match acquire_lock(&path, Duration::from_millis(300), fast).await {
            LockWait::Acquired(l) => l,
            LockWait::TimedOut(h) => panic!("an orphan from a previous image must be reclaimed, not waited on ({h})"),
        };
        // Now this image holds it: a second acquire must wait, not steal it.
        match acquire_lock(&path, Duration::from_millis(300), fast).await {
            LockWait::TimedOut(_) => {}
            LockWait::Acquired(_) => panic!("a lock this image holds must not be reclaimed"),
        }
        drop(held);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn the_server_takes_waits_for_and_takes_over_the_land_lock() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let path = d.path().join("locks").join("land-test");
        let fast = Duration::from_millis(50);
        match acquire_lock(&path, Duration::from_secs(1), fast).await {
            LockWait::Acquired(l) => {
                assert_eq!(read_pid(&path), Some(std::process::id() as i32));
                assert_eq!(std::fs::read_to_string(path.join("who")).unwrap().trim(), LOCK_WHO);
                assert!(path.join("since").exists());
                drop(l);
                assert!(!path.exists(), "released on drop");
            }
            LockWait::TimedOut(h) => panic!("a free lock must be taken, not waited on ({h})"),
        }
        let mut holder = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("pid"), format!("{}\n", holder.id())).unwrap();
        std::fs::write(path.join("who"), "gs12-data\n").unwrap();
        match acquire_lock(&path, Duration::from_millis(300), fast).await {
            LockWait::TimedOut(h) => assert!(h.contains("gs12-data") && h.contains(&holder.id().to_string()), "{h}"),
            LockWait::Acquired(_) => panic!("a live holder's lock must not be taken"),
        }
        assert_eq!(read_pid(&path), Some(holder.id() as i32), "the holder's lock is untouched");
        holder.kill().unwrap();
        holder.wait().unwrap();
        match acquire_lock(&path, Duration::from_secs(1), fast).await {
            LockWait::Acquired(_l) => assert_eq!(read_pid(&path), Some(std::process::id() as i32), "a dead holder is taken over"),
            LockWait::TimedOut(h) => panic!("a dead holder must not block ({h})"),
        }
    }

    /// The repository's pre-push land-lock leg refuses a push to main while a
    /// live pid holds the lock, unless the push carries that pid as
    /// AMUX_LAND_HOLDER_PID. A server land holding the lock passes it; the
    /// same land without the lock is refused, which is the MO-4396 failure.
    #[tokio::test]
    async fn the_pre_push_land_lock_accepts_the_servers_push_while_it_holds_the_lock() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        let lock = lock_path("test-origin", "main");
        let hook = lane.join(".git").join("hooks").join("pre-push");
        std::fs::write(&hook, format!(
            "#!/bin/sh\nlock=\"${{AMUX_LAND_LOCK:-{}}}\"\n[ -f \"$lock/pid\" ] || exit 0\npid=\"$(tr -dc '0-9' < \"$lock/pid\")\"\n[ -n \"$pid\" ] || exit 0\n[ \"$pid\" = \"${{AMUX_LAND_HOLDER_PID:-}}\" ] && exit 0\nkill -0 \"$pid\" 2>/dev/null || exit 0\necho \"land-lock: REFUSED, main is being landed by pid $pid\" >&2\nexit 1\n",
            lock.display())).unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&hook).status().unwrap();
        let held = match acquire_lock(&lock, Duration::from_secs(1), Duration::from_millis(50)).await {
            LockWait::Acquired(l) => l,
            LockWait::TimedOut(h) => panic!("{h}"),
        };
        let a = commit(&lane, "b.txt", "ok\n");
        let without = land(&lane, vec![Entry { id: 1, repo: "r".into(), lane: "l".into(), sha: a.clone() }], None, "", None).await.unwrap();
        match &without[0].1 {
            Outcome::Refused(why) => assert!(why.contains("land-lock: REFUSED"), "{why}"),
            o => panic!("a push that does not carry the holder pid must be refused while the lock is held: {o:?}"),
        }
        let with = land(&lane, vec![Entry { id: 2, repo: "r".into(), lane: "l".into(), sha: a.clone() }], None, "", Some(&held)).await.unwrap();
        assert!(matches!(with[0].1, Outcome::Merged(_)), "the holder's own push passes: {:?}", with[0].1);
        drop(held);
        assert!(!lock.exists());
    }

    #[test]
    fn the_wait_p95_is_the_95th_percentile_in_minutes() {
        let mut w: Vec<f64> = (1..=20).map(|m| m as f64 * 60.0).collect();
        assert_eq!(wait_p95(&mut w), Some(19.0));
        assert_eq!(wait_p95(&mut []), None, "no finished land is unmeasured, never zero");
    }
}
