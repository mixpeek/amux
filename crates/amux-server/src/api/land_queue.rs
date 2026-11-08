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
//! A REPOSITORY CAN DECLARE ITS OWN GATE (Z2, 2026-11-01). `amux.queueGate` in
//! `.amux/land.conf` makes this queue the one path for every land of that
//! repository, whichever lane or rule: `amux land` posts here and waits for the
//! outcome, the command is read from origin/main at each compose, and any file
//! it names runs in origin/main's bytes, so a change cannot weaken the gate that
//! judges it. Each entry is judged on main plus everything ahead of it: a batch
//! is gated as one composed tree, and a red batch bisects, so a refused entry was
//! tested alone on main plus what landed ahead of it. The push is a fast-forward
//! of exactly the tree the gate saw, or it recomposes. A repository with no
//! declaration behaves as before and says so (verdict=land_gate_undeclared, on
//! the row and in the lane's message). AMUX_LAND_QUEUE_DECLARED=0 in server.env
//! ignores every declaration (A11). See docs/orchestration-contract.md.
//!
//! The server takes the same land lock `amux land` and the repository's
//! pre-push land-lock leg use (~/.amux/locks/land-<16 hex of sha1("<url>
//! main")>, holder pid in `pid`), and pushes with AMUX_LAND_HOLDER_PID set to
//! its own pid, so a client land and a server land never push past each
//! other (MO-4396, 2026-10-06: land 5 was refused while gs12-data held it).
//!
//! Verdicts (rule 5 at /api/contract/counters): land_queued, land_merged,
//! land_refused, land_batch_bisected, worker_push_refused, land_lock_acquired,
//! land_lock_waited, land_lock_timeout_requeued, land_gate_green, land_gate_red,
//! land_gate_unmeasured, land_gate_undeclared. Enqueue refusals by the land
//! guards: land_refused_frozen, land_refused_reused_message (see check_guards).
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

#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub id: i64,
    pub repo: String,
    pub lane: String,
    pub sha: String,
    /// The escapes sent with the request, for the turn-time guard re-run.
    pub ov: GuardOverrides,
    /// The checkout the land came from, when the client named one.
    pub tree: Option<String>,
    /// A revert the queue built itself (api::land_red): it has no receipt, so
    /// the land gate's owner and trailer legs judge it and its receipt leg does not.
    pub revert: bool,
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
// What the repository declares (.amux/land.conf on origin/main)
// ---------------------------------------------------------------------------

const DEFAULT_GATE_TIMEOUT_S: u64 = 900;

/// The queue keys a repository declares, read from committed bytes. `gate` is
/// the switch: without it nothing else here applies.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoPolicy {
    pub gate: Option<String>,
    pub timeout_s: u64,
    /// amux.landFastGate: the push runs the hook's cheap legs, as a client land does.
    pub fast_push: bool,
    pub regenerate: bool,
    pub regenerate_always: bool,
    pub regenerable: Vec<String>,
    /// amux.queueChecks: the workflows that say main is green (api::land_red).
    pub checks: Vec<String>,
    pub on_red: OnRed,
}

/// amux.queueOnRed: what a red main after a land gets. `fix` unless declared.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum OnRed {
    Off,
    #[default]
    Fix,
    Revert,
}

async fn conf_all(tree: &Path, rev: &str, key: &str) -> Vec<String> {
    let blob = format!("{rev}:.amux/land.conf");
    git(tree, &["config", "--blob", &blob, "--get-all", key]).await
        .map(|o| o.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

/// Whether the owner switched declarations off for this server.
fn declarations_on() -> bool {
    crate::config::parse_env_file(&home().join("server.env")).get("AMUX_LAND_QUEUE_DECLARED")
        .is_none_or(|v| !matches!(v.trim().trim_matches('"'), "0" | "false" | "off" | "no"))
}

/// The declaration at `rev`, which is origin/main or a sha of it, never the
/// change under test.
pub async fn repo_policy(tree: &Path, rev: &str) -> RepoPolicy {
    if !declarations_on() {
        return RepoPolicy::default();
    }
    let Some(gate) = conf_all(tree, rev, "amux.queueGate").await.pop() else { return RepoPolicy::default() };
    let one = |v: Vec<String>| v.into_iter().next_back().unwrap_or_default();
    RepoPolicy {
        gate: Some(gate),
        timeout_s: one(conf_all(tree, rev, "amux.queueGateTimeout").await).parse().ok().filter(|n| *n > 0).unwrap_or(DEFAULT_GATE_TIMEOUT_S),
        fast_push: one(conf_all(tree, rev, "amux.landFastGate").await).eq_ignore_ascii_case("true"),
        regenerate: !conf_all(tree, rev, "amux.landRegenerate").await.is_empty(),
        regenerate_always: !one(conf_all(tree, rev, "amux.landRegenerateWhen").await).eq_ignore_ascii_case("conflict"),
        regenerable: conf_all(tree, rev, "amux.landRegenerable").await,
        checks: conf_all(tree, rev, "amux.queueChecks").await,
        on_red: match one(conf_all(tree, rev, "amux.queueOnRed").await).to_ascii_lowercase().as_str() {
            "revert" => OnRed::Revert,
            "off" => OnRed::Off,
            _ => OnRed::Fix,
        },
    }
}

/// `*` matches across `/`, as the bash `[[ path == glob ]]` the client uses.
fn glob_hit(glob: &str, path: &str) -> bool {
    let parts: Vec<&str> = glob.split('*').collect();
    let [first, mid @ .., last] = parts.as_slice() else { return glob == path };
    let Some(mut rest) = path.strip_prefix(first) else { return false };
    for m in mid {
        let Some(i) = rest.find(m) else { return false };
        rest = &rest[i + m.len()..];
    }
    rest.ends_with(last)
}

/// What the declared gate said about a composed tree.
#[derive(Debug, PartialEq)]
enum GateRun {
    Green,
    Red(String),
    /// It could not judge the tree, which says nothing about the entries.
    Unmeasured(String),
}

const UNRUNNABLE: &str = "amux-gate-could-not-start";

/// The program the command runs: its first word after any `VAR=value`, or the
/// script an interpreter is given. Only the program, because any other path
/// the command names is data the gate must read from the composed tree.
fn gate_program(cmd: &str) -> Option<String> {
    const INTERPRETERS: [&str; 6] = ["sh", "bash", "python", "python3", "node", "env"];
    let is_assignment = |t: &str| t.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit())
    });
    for word in cmd.split_whitespace().map(|t| t.trim_matches(|c| c == '"' || c == '\'')) {
        if is_assignment(word) || word.starts_with('-') {
            continue;
        }
        if !INTERPRETERS.contains(&word.rsplit('/').next().unwrap_or(word)) {
            return Some(word.to_string());
        }
    }
    None
}

/// The gate's program, when `main` holds it.
async fn gate_files(dir: &Path, main: &str, cmd: &str) -> Vec<String> {
    let Some(prog) = gate_program(cmd) else { return Vec::new() };
    if prog.starts_with('$') || git(dir, &["cat-file", "-e", &format!("{main}:{prog}")]).await.is_err() {
        return Vec::new();
    }
    vec![prog]
}

/// Run the declaration on the composed tree. The candidate holds origin/main's
/// copy of the program the command runs, because a change that edits its own
/// gate must still be judged by the gate main has. Everything else, the
/// script's sibling files included, comes from the composed tree.
async fn run_declared_gate(dir: &Path, cmd: &str, main: &str, tip: &str, prefix: &str, timeout_s: u64) -> GateRun {
    let files = gate_files(dir, main, cmd).await;
    for f in &files {
        if let Err(e) = git(dir, &["checkout", "-q", main, "--", f]).await {
            return GateRun::Unmeasured(format!("could not put origin/main's {f} in the candidate: {e}"));
        }
    }
    let script = format!("export AMUX_LAND_BASE={main} AMUX_LAND_TIP={tip}\n{cmd}\nrc=$?\n[ \"$rc\" = 126 ] || [ \"$rc\" = 127 ] && echo {UNRUNNABLE}\nexit \"$rc\"");
    let (ok, text) = crate::api::contract::sh(dir, &script, prefix, Duration::from_secs(timeout_s)).await;
    // The named files are origin/main's now; the tip stays what gets pushed.
    let restored = git(dir, &["reset", "-q", "--hard", tip]).await;
    if let Err(e) = restored {
        return GateRun::Unmeasured(format!("could not restore the candidate to {tip}: {e}"));
    }
    if ok {
        GateRun::Green
    } else if text.contains(UNRUNNABLE) || text.starts_with("could not run") {
        GateRun::Unmeasured(format!("the declared gate `{cmd}` could not start:\n{}", text.replace(UNRUNNABLE, "")))
    } else if text.starts_with("timed out after") {
        GateRun::Unmeasured(format!("the declared gate `{cmd}` {text}"))
    } else {
        GateRun::Red(text)
    }
}

/// The owner and receipt legs of the repository's land gate, when it has one.
/// A server push has no lane in its environment, so the pre-push hook skips
/// them as it would for a human; run them here with the lane named, or the
/// queue would be the way around `[owner]` paths (A3).
async fn land_gate_leg(tree: &Path, dir: &Path, main: &str, spans: &[Span]) -> Result<(), String> {
    let Ok(py) = git(tree, &["show", &format!("{main}:scripts/land_gate.py")]).await else { return Ok(()) };
    let Ok(review) = git(tree, &["show", &format!("{main}:scripts/land_review.py")]).await else { return Ok(()) };
    // Per candidate: batches of two repositories run at once, each with its own scripts.
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let scratch = home().join("tmp").join("land").join(format!("gate-{}-{name}", std::process::id()));
    let ready = std::fs::create_dir_all(&scratch).and(std::fs::write(scratch.join("land_gate.py"), py)).and(std::fs::write(scratch.join("land_review.py"), review));
    let mut refusal = ready.err().map(|e| format!("could not write the land gate: {e}"));
    for s in spans {
        if refusal.is_some() {
            break;
        }
        let run = tokio::process::Command::new("python3").arg(scratch.join("land_gate.py"))
            .args(["--pre-push", "--local", &s.tip, "--remote", &s.base]).current_dir(dir)
            .env("AMUX_SESSION", &s.entry.lane).env("AMUX_LAND_GATE_REQUIRED", if s.entry.revert { "0" } else { "1" })
            .kill_on_drop(true).output();
        match tokio::time::timeout(Duration::from_secs(GUARD_TIMEOUT_S), run).await {
            Ok(Ok(o)) if o.status.success() => {}
            Ok(Ok(o)) => refusal = Some(format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
            Ok(Err(e)) => refusal = Some(format!("the land gate did not start: {e}")),
            Err(_) => refusal = Some(format!("the land gate timed out after {GUARD_TIMEOUT_S}s")),
        }
    }
    if let Err(e) = std::fs::remove_dir_all(&scratch) {
        tracing::warn!(dir = %scratch.display(), error = %e, measured = true, n_considered = 1, verdict = "land_gate_scratch_left",
            "the land gate's scratch dir stayed behind; the next batch overwrites it");
    }
    refusal.map_or(Ok(()), Err)
}

/// One entry's commits on the composed tree.
#[derive(Clone, Debug)]
struct Span {
    entry: Entry,
    base: String,
    tip: String,
}

/// Replay one lane's range. A conflict only in paths the repository declares
/// regenerable keeps main's copy, as a client replay does. Ok(true) says a
/// path was resolved that way, so the composed tree needs a render.
async fn replay(dir: &Path, range: &str, regenerable: &[String]) -> Result<bool, String> {
    let mut kept_main = false;
    let mut step = git(dir, &["-c", "core.hooksPath=/dev/null", "cherry-pick", "--allow-empty", "--keep-redundant-commits", range]).await;
    loop {
        let Err(err) = step else { return Ok(kept_main) };
        let conflicted = git(dir, &["diff", "--name-only", "--diff-filter=U"]).await.unwrap_or_default();
        let paths: Vec<&str> = conflicted.lines().filter(|l| !l.is_empty()).collect();
        if paths.is_empty() || !paths.iter().all(|p| regenerable.iter().any(|g| glob_hit(g, p))) {
            return Err(err);
        }
        for p in paths {
            if git(dir, &["checkout", "--ours", "--", p]).await.is_err() {
                git(dir, &["rm", "-q", "-f", "--", p]).await?;
            } else {
                git(dir, &["add", "--", p]).await?;
            }
        }
        kept_main = true;
        // Keeping main's copy can leave the commit empty, which `--continue`
        // refuses, so commit it here: its message and trailers stay on main.
        git(dir, &["-c", "core.hooksPath=/dev/null", "commit", "-q", "--allow-empty", "-C", "CHERRY_PICK_HEAD"]).await?;
        step = match git(dir, &["-c", "core.hooksPath=/dev/null", "-c", "core.editor=true", "cherry-pick", "--continue"]).await {
            // That was the range's last commit.
            Err(e) if e.contains("no cherry-pick or revert in progress") => Ok(String::new()),
            other => other,
        };
    }
}

/// Re-render on the composed tree with the client's own `_land_regenerate`,
/// from the CLI this server embeds. A failing generator never stops the queue:
/// the tree stays unrendered and the gate judges it, as in a client land.
async fn regenerate(dir: &Path, tip: &str) -> (String, Option<String>) {
    let cli = home().join("tmp").join("land").join(format!("amux-cli-{:016x}", {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(AMUX_CLI, &mut h);
        std::hash::Hasher::finish(&h)
    }));
    if !cli.exists() {
        let t = cli.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::create_dir_all(home().join("tmp").join("land")).and(std::fs::write(&t, AMUX_CLI)).and(std::fs::rename(&t, &cli)).is_err() {
            tracing::warn!(measured = false, n_considered = 1, why_unmeasured = "could not write the embedded CLI",
                verdict = "land_regenerate_failed", "the composed tree stays unrendered");
            return (tip.to_string(), Some("could not write the embedded CLI".into()));
        }
    }
    let run = tokio::process::Command::new("bash").arg(&cli).args(["land", "--regenerate-on", tip]).current_dir(dir)
        .env("AMUX_LAND_GUARDED", "1").kill_on_drop(true).stdin(std::process::Stdio::null()).output();
    let out = tokio::time::timeout(Duration::from_secs(GATE_TIMEOUT_S), run).await;
    let (ok, text, err) = match out {
        Ok(Ok(o)) => (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned(), format!("{}: {}", o.status, String::from_utf8_lossy(&o.stderr))),
        Ok(Err(e)) => (false, String::new(), e.to_string()),
        Err(_) => (false, String::new(), "timed out".into()),
    };
    let new = text.lines().last().unwrap_or("").trim().to_string();
    if ok && new.len() == 40 && new.chars().all(|c| c.is_ascii_hexdigit()) && git(dir, &["reset", "-q", "--hard", &new]).await.is_ok() {
        tracing::info!(tip, new, measured = true, n_considered = 1, verdict = "land_regenerated", "the composed tree was re-rendered");
        return (new, None);
    }
    tracing::warn!(tip, reason = %tail(err.trim(), 300), measured = true, n_considered = 1, verdict = "land_regenerate_failed",
        "the declared regenerate step failed; the gate judges the unrendered tree");
    (tip.to_string(), Some(format!("rc ok={ok}, stdout {:?}: {}", tail(text.trim(), 200), tail(err.trim(), 500))))
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
/// The `started_at` each live row was claimed with: the claim's generation.
/// A finish writes only while the row still carries it, so a batch whose row
/// was reclaimed and re-run cannot overwrite the newer run's state (implicit
/// state machines inventory 2026-10-31, #16).
static CLAIMED_AT: std::sync::LazyLock<std::sync::Mutex<HashMap<i64, f64>>> =
    std::sync::LazyLock::new(Default::default);

fn claim_of(id: i64) -> f64 {
    CLAIMED_AT.lock().ok().and_then(|m| m.get(&id).copied()).unwrap_or(f64::NAN)
}

fn log_lost_claim(id: i64, to: &str) {
    tracing::warn!(id, to, measured = true, n_considered = 1, verdict = "land_finish_lost_claim",
        "a land batch finished after its row was reclaimed; the row keeps the newer run's state");
}

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
async fn push(dir: &Path, refspec: &str, lock: Option<&LandLock>, fast: bool) -> Result<String, String> {
    if lock.is_none() && !fast {
        return git(dir, &["push", "-q", "origin", refspec]).await;
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(["push", "-q", "origin", refspec]).kill_on_drop(true);
    if let Some(l) = lock {
        cmd.env("AMUX_LAND_HOLDER_PID", l.pid.to_string()).env("AMUX_LAND_LOCK", &l.path);
    }
    if fast {
        cmd.env("LAND_PRECHECK", "1").env("AMUX_LAND_PRECHECK_MODE", "batch");
    }
    let run = cmd.output();
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

/// ONE REUSED CANDIDATE PER REPOSITORY (gs12-extra-2, land 119, 2026-10-08:
/// running -> queued twice in 35 min on "git worktree timed out after 120s",
/// holding the mixpeek queue ~50 min). A fresh `git worktree add` of mixpeek
/// checks out 39k files, measured at 237 s under load (258 s with hooks off,
/// so hooks are not the cost), and every timeout left a partial ~1 GB
/// checkout under ~/.amux/tmp/land. The candidate is now created once (15 min
/// budget) and reset to origin/main for each land, which rewrites only what
/// changed. Old per-land c-* directories are swept. Logs
/// verdict=land_candidate_reused / land_candidate_created.
async fn candidate(tree: &Path, main: &str) -> Result<PathBuf, String> {
    let key = repo_key(tree).await?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&key, &mut h);
    let root = home().join("tmp").join("land");
    let _ = std::fs::create_dir_all(&root);
    let dir = root.join(format!("cand-{:016x}", std::hash::Hasher::finish(&h)));
    sweep_old_candidates(tree, &root).await;
    if dir.join(".git").exists() && git(&dir, &["rev-parse", "--git-dir"]).await.is_ok() {
        let _ = git(&dir, &["cherry-pick", "--abort"]).await;
        let reset = async {
            git(&dir, &["-c", "core.hooksPath=/dev/null", "checkout", "-q", "--detach", "-f", main]).await?;
            git(&dir, &["reset", "-q", "--hard", main]).await?;
            git(&dir, &["clean", "-fdq"]).await
        };
        match reset.await {
            Ok(_) => {
                tracing::info!(dir = %dir.display(), main, measured = true, n_considered = 1, verdict = "land_candidate_reused",
                    "the land candidate was reset to origin/main instead of checked out again");
                return Ok(dir);
            }
            Err(e) => tracing::warn!(dir = %dir.display(), error = %e, measured = true, n_considered = 1,
                verdict = "land_candidate_reset_failed", "the land candidate could not be reset; it is recreated"),
        }
    }
    let d = dir.to_string_lossy().into_owned();
    cleanup_candidate(tree, &dir).await;
    let _ = git(tree, &["worktree", "prune"]).await;
    let t = tree.to_string_lossy().into_owned();
    let out = crate::api::session_verbs::run_cmd("git", &["-C", &t, "-c", "core.hooksPath=/dev/null", "worktree", "add", "-q", "--detach", &d, main],
        Duration::from_secs(CANDIDATE_CREATE_S)).await;
    match out {
        Some(o) if o.status.success() => {
            tracing::info!(dir = %dir.display(), main, measured = true, n_considered = 1, verdict = "land_candidate_created",
                "created the reusable land candidate");
            Ok(dir)
        }
        other => {
            tracing::warn!(dir = %dir.display(), measured = true, n_considered = 1, verdict = "land_candidate_start_failed", "land candidate could not start; cleanup runs before requeue");
            cleanup_candidate(tree, &dir).await;
            Err(match other {
                Some(o) => format!("git worktree add failed: {}", tail(String::from_utf8_lossy(&o.stderr).trim(), 400)),
                None => format!("git worktree add timed out after {CANDIDATE_CREATE_S}s"),
            })
        }
    }
}

const CANDIDATE_CREATE_S: u64 = 900;

/// Per-land checkouts from before the reusable candidate (c-<pid>-<ts>), an
/// hour old or more, are removed with their worktree registration.
async fn sweep_old_candidates(tree: &Path, root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let Ok(registered) = git(tree, &["worktree", "list", "--porcelain"]).await else { return };
    let mut n = 0;
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() > 3600);
        if name.starts_with("c-") && old && !p.is_symlink()
            && std::fs::canonicalize(&p).ok().is_some_and(|p| registered.lines().any(|l| l == format!("worktree {}", p.display()))) {
            cleanup_candidate(tree, &p).await;
            if !p.exists() { n += 1; }
        }
    }
    if n > 0 {
        let _ = git(tree, &["worktree", "prune"]).await;
        tracing::info!(removed = n, measured = true, n_considered = n, verdict = "land_candidate_leftovers_swept",
            "removed per-land checkouts left by earlier lands");
    }
}

async fn adopt_push_receipts(tree: &Path, entries: &[Entry], main: &str, state: Option<&AppState>) -> Result<(Vec<Entry>, Vec<(i64, Outcome)>), String> {
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
        let (pending, mut adopted) = adopt_push_receipts(tree, entries, &main, state).await?;
        if pending.is_empty() { return Ok(Attempt::Done(adopted)); }
        let dir = candidate(tree, &main).await?;
        let result = compose_candidate(tree, &pending, gate, prefix, lock, (&dir, &main), state).await;
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
    let mut out: Vec<(i64, Outcome)> = Vec::new();
    let mut applied: Vec<Entry> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let policy = repo_policy(tree, main).await;
    let mut kept_main = false;
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
        let regenerable: &[String] = if policy.gate.is_some() { &policy.regenerable } else { &[] };
        match replay(dir, &format!("{base}..{}", e.sha), regenerable).await {
            Ok(resolved) => {
                kept_main |= resolved;
                let tip = git(dir, &["rev-parse", "HEAD"]).await?;
                spans.push(Span { entry: e.clone(), base: before.clone(), tip });
                applied.push(e.clone());
            }
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
    let mut merged = git(dir, &["rev-parse", "HEAD"]).await?;
    let mut regen_failed: Option<String> = None;
    if policy.gate.is_some() && policy.regenerate && (policy.regenerate_always || kept_main) {
        let (rendered, failed) = regenerate(dir, &merged).await;
        merged = rendered;
        regen_failed = failed;
        if let Some(last) = spans.last_mut() {
            last.tip = merged.clone();
        }
    }
    let lane_gate = gate.filter(|g| !g.trim().is_empty());
    let gate_verdict = if let Some(cmd) = policy.gate.as_deref() {
        match run_declared_gate(dir, cmd, main, &merged, prefix, policy.timeout_s).await {
            GateRun::Green => {
                tracing::info!(main, tip = %merged, n_entries = applied.len(), measured = true, n_considered = applied.len(), verdict = "land_gate_green",
                    "the declared gate passed the composed tree");
                "green"
            }
            GateRun::Red(text) => {
                tracing::warn!(main, tip = %merged, n_entries = applied.len(), measured = true, n_considered = applied.len(), verdict = "land_gate_red",
                    "the declared gate refused the composed tree");
                return Ok(Attempt::Red(format!("the declared gate `{cmd}` failed on origin/main {} plus everything ahead of this commit:\n{text}{}", &main[..12.min(main.len())],
                    regen_failed.map(|w| format!("\nThis tree is unrendered: the declared regenerate step failed: {w}")).unwrap_or_default()), applied, out));
            }
            GateRun::Unmeasured(why) => {
                // No verdict on any entry, so nothing merges and nothing bisects.
                tracing::warn!(main, tip = %merged, measured = false, n_considered = applied.len(), why_unmeasured = %tail(&why, 300), verdict = "land_gate_unmeasured",
                    "the declared gate could not judge the composed tree");
                let msg = format!("{why}\n\nNothing was merged and this is no verdict on your commit. The gate is declared in .amux/land.conf on origin/main; a fix to it lands with AMUX_LAND_CLIENT=1 AMUX_LAND_CLIENT_REASON=<why> amux land.");
                out.extend(applied.iter().map(|e| (e.id, Outcome::Refused(msg.clone()))));
                return Ok(Attempt::Done(out));
            }
        }
    } else if let Some(g) = lane_gate {
        let (ok, text) = crate::api::contract::sh(dir, g, prefix, Duration::from_secs(GATE_TIMEOUT_S)).await;
        if !ok {
            return Ok(Attempt::Red(format!("land gate `{g}` failed on the composed tree:\n{text}"), applied, out));
        }
        "lane"
    } else {
        tracing::warn!(main, tip = %merged, measured = false, n_considered = applied.len(), why_unmeasured = "no amux.queueGate in .amux/land.conf on origin/main and no AMUX_LAND_GATE",
            verdict = "land_gate_undeclared", "nothing tested the composed tree before this push");
        "undeclared"
    };
    if policy.gate.is_some() {
        if let Err(why) = land_gate_leg(tree, dir, main, &spans).await {
            tracing::warn!(main, tip = %merged, measured = true, n_considered = applied.len(), verdict = "land_gate_leg_refused", reason = %tail(&why, 300),
                "the repository's land gate refused the queued commits");
            return Ok(Attempt::Red(format!("the land gate refused this commit:\n{}", tail(&why, 1800)), applied, out));
        }
    }
    if let Some(state) = state {
        let rows: Vec<(i64, String, String)> = spans.iter().map(|s| (s.entry.id, s.base.clone(), s.tip.clone())).collect();
        let (intended, base, verdict) = (merged.clone(), main.to_string(), gate_verdict.to_string());
        let saved = state.store.write_async(move |conn| {
            for (id, from, to) in &rows {
                if conn.execute("UPDATE land_queue SET merged_sha=?2, base_sha=?3, landed_base=?4, landed_tip=?5, gate_verdict=?6 WHERE id=?1 AND state='running'",
                    rusqlite::params![id, intended, base, from, to, verdict])? != 1 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).await.map_err(|e| format!("land push intent was not persisted; no push attempted: {e}"))?;
        if !saved.applied { return Err("land push intent was not applied; no push attempted".into()); }
        tracing::info!(candidate = %merged, measured = true, n_considered = applied.len(),
            verdict = "land_push_intent_saved", "exact composed SHA persisted before remote push");
    }
    match push(dir, &format!("{merged}:refs/heads/main"), lock, policy.gate.is_some() && policy.fast_push).await {
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
        let (id2, m2, o2, claim) = (*id, merged.clone(), output.clone(), claim_of(*id));
        let _ = state.store.write_async(move |conn| {
            let n = conn.execute(
                "UPDATE land_queue SET state = ?2, merged_sha = ?3, output = ?4, done_at = ?5 \
                 WHERE id = ?1 AND state = 'running' AND started_at = ?6",
                rusqlite::params![id2, st, m2, o2, now, claim])?;
            if n == 0 {
                log_lost_claim(id2, st);
            }
            Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
        }).await;
        let (lane, sha) = lanes.get(id).cloned().unwrap_or_default();
        let text = match o {
            Outcome::Merged(m) => {
                tracing::info!(id, lane, sha, merged = %m, measured = true, n_considered = 1, verdict = "land_merged",
                    "the server landed a queued commit on main");
                let id3 = *id;
                let verdict: Option<String> = state.store.read_async(move |c| {
                    use rusqlite::OptionalExtension;
                    Ok(c.query_row("SELECT gate_verdict FROM land_queue WHERE id = ?1", [id3], |r| r.get(0)).optional()?.flatten())
                }).await.ok().flatten();
                let caveat = if verdict.as_deref() == Some("undeclared") {
                    " No gate tested the composed tree: this repository declares no amux.queueGate in .amux/land.conf."
                } else {
                    ""
                };
                format!("[amux land] {} landed on main (as part of {}).{caveat}", &sha[..12.min(sha.len())], &m[..12.min(m.len())])
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
    type Row = (i64, String, String, String, Option<String>, bool, Option<String>, String);
    let rows: Vec<Row> = state.store.read_async(|c| {
        let mut st = c.prepare("SELECT id, repo, lane, sha, freeze_override, allow_reused_message, tree, kind FROM land_queue WHERE state = 'queued'
                                AND repo NOT IN (SELECT repo FROM land_queue WHERE state = 'running')
                                ORDER BY priority DESC, id")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)? != 0, r.get(6)?, r.get(7)?)))?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let mut by_repo: Vec<(String, Vec<Entry>)> = Vec::new();
    for (id, repo, lane, sha, freeze, allow_reused_message, tree, kind) in rows {
        let e = Entry { id, repo: repo.clone(), lane, sha, ov: GuardOverrides { freeze, allow_reused_message }, tree, revert: kind == "revert" };
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
        // A revert lands alone, so its push says one thing to CI and to the watch.
        match entries.iter().position(|e| e.revert) {
            Some(0) => entries.truncate(1),
            Some(i) => entries.truncate(i),
            None => {}
        }
        let ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
        let ids2 = ids.clone();
        let claimed = state.store.write_async(move |conn| {
            let mut n = 0;
            for id in &ids2 {
                n += conn.execute("UPDATE land_queue SET state = 'running', started_at = ?2 WHERE id = ?1 AND state = 'queued'",
                    rusqlite::params![id, now])?;
            }
            if let Ok(mut m) = CLAIMED_AT.lock() {
                for id in &ids2 {
                    m.insert(*id, now);
                }
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
            if let Ok(mut m) = CLAIMED_AT.lock() {
                for id in &ids {
                    m.remove(id);
                }
            }
        });
    }
    started
}

async fn run_batch(state: &AppState, entries: Vec<Entry>) {
    let lanes: HashMap<i64, (String, String)> = entries.iter().map(|e| (e.id, (e.lane.clone(), e.sha.clone()))).collect();
    let first = entries[0].lane.clone();
    // The checkout the land came from, else the lane's, else the clone itself:
    // a worktree can be gone by the time a queued entry's turn comes.
    let named = entries[0].tree.as_deref().map(PathBuf::from).filter(|p| p.is_dir());
    let clone = Path::new(&entries[0].repo).parent().map(Path::to_path_buf).filter(|p| p.join(".git").exists());
    let Some(tree) = named.or_else(|| crate::api::contract::lane_tree(&first)).or(clone) else {
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
    // Turn-time land guards, under the lock: a freeze or a landed message may
    // have reached origin/main since this entry was queued. The CLI re-runs
    // them at its holder turn for the same reason.
    let mut entries = entries;
    let mut turn_refused: Vec<(i64, Outcome)> = Vec::new();
    let mut keep = Vec::with_capacity(entries.len());
    for e in entries.drain(..) {
        match check_guards(&tree, &e.lane, &e.sha, &e.ov, "turn").await {
            Ok(()) => keep.push(e),
            Err(r) => turn_refused.push((e.id, Outcome::Refused(format!(
                "refused at its turn by a land guard (verdict={} phase=turn); nothing was merged.\n{}", r.verdict, r.lines.join("\n"))))),
        }
    }
    let entries = keep;
    if !turn_refused.is_empty() {
        record(state, &turn_refused, &lanes).await;
    }
    if entries.is_empty() {
        return;
    }
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
    let ids: Vec<(i64, f64)> = entries.iter().map(|e| (e.id, claim_of(e.id))).collect();
    let _ = state.store.write_async(move |conn| {
        for (id, claim) in &ids {
            let n = conn.execute(
                "UPDATE land_queue SET state = 'queued', started_at = NULL, output = ?2 \
                 WHERE id = ?1 AND state = 'running' AND started_at = ?3",
                rusqlite::params![id, why, claim])?;
            if n == 0 {
                log_lost_claim(*id, "queued");
            }
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;
}

// ---------------------------------------------------------------------------
// Land guards (mxp-gs12 review, 2026-10-18): a frozen path and a reused
// commit message. `amux land` ran them client-side only, so a direct
// `POST /api/land` skipped both. The server now runs the CLI's own predicate
// (`_LAND_GUARD_PY` in the bash CLI, embedded at build time so the two cannot
// drift) on every enqueue and is authoritative; the CLI keeps its early check
// for a fast message. Freezes come from `.amux/land.conf` on origin/main and
// verdict lines go to ~/.amux/logs/land.log, the same sources the CLI uses.
// ---------------------------------------------------------------------------

const AMUX_CLI: &str = include_str!("../../../../amux");
const GUARD_TIMEOUT_S: u64 = 120;

/// The Python body of `_LAND_GUARD_PY='...'` in the bash CLI.
pub fn guard_py() -> Option<&'static str> {
    let start = AMUX_CLI.find("\n_LAND_GUARD_PY='")? + "\n_LAND_GUARD_PY='".len();
    let len = AMUX_CLI[start..].find('\'')?;
    Some(&AMUX_CLI[start..start + len])
}

/// Overrides the caller may pass, mirroring the CLI's environment escapes.
#[derive(Default, Clone, Debug)]
pub struct GuardOverrides {
    /// AMUX_LAND_FREEZE_OVERRIDE='<name>: <why>' for this lane's own land.
    pub freeze: Option<String>,
    /// AMUX_LAND_ALLOW_REUSED_MESSAGE=1.
    pub allow_reused_message: bool,
}

/// A guard refusal: the verdict and the guard's refusal and hint lines.
#[derive(Debug)]
pub struct GuardRefusal {
    pub verdict: &'static str,
    pub lines: Vec<String>,
}

fn land_log(lane: &str, mode: &str, phase: &str, text: &str) {
    use std::io::Write;
    let dir = home().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("land.log")) else { return };
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let _ = writeln!(f, "{ts} {lane} {mode} guard: {line} (server phase={phase})");
    }
}

async fn run_guard(tree: &Path, mode: &str, base: &str, tip: &str, ov: &GuardOverrides) -> (i32, String) {
    let Some(py) = guard_py() else { return (3, "guard error: _LAND_GUARD_PY not found in the embedded CLI".into()) };
    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg("-c").arg(py).args([mode, "origin/main", base, tip]).current_dir(tree)
        .env_remove("AMUX_LAND_FREEZE_OVERRIDE").kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    match ov.freeze.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) => { cmd.env("AMUX_LAND_FREEZE_OWN", "1").env("AMUX_LAND_FREEZE_OVERRIDE", f); }
        None => { cmd.env("AMUX_LAND_FREEZE_OWN", "0"); }
    }
    match tokio::time::timeout(Duration::from_secs(GUARD_TIMEOUT_S), cmd.output()).await {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.code().unwrap_or(3), s)
        }
        Ok(Err(e)) => (3, format!("guard error: python3 did not start: {e}")),
        Err(_) => (3, format!("guard error: timed out after {GUARD_TIMEOUT_S}s")),
    }
}

/// Run both land guards on the lane's range merge-base(origin/main, sha)..sha.
/// A guard that cannot run is logged as unmeasured and refuses nothing, as in
/// the CLI. `phase` is "enqueue" (POST /api/land) or "turn" (the batch that
/// lands it, under the land lock).
pub async fn check_guards(tree: &Path, lane: &str, sha: &str, ov: &GuardOverrides, phase: &str) -> Result<(), GuardRefusal> {
    let _ = git(tree, &["fetch", "-q", "origin", "main"]).await;
    let Ok(base) = git(tree, &["merge-base", "origin/main", sha]).await else {
        tracing::warn!(lane, sha, phase, measured = false, n_considered = 0, why_unmeasured = "no merge-base with origin/main",
            verdict = "land_guard_unmeasured", "land guards skipped");
        return Ok(());
    };
    for (mode, verdict) in [("freeze", "land_refused_frozen"), ("message", "land_refused_reused_message")] {
        let (mut rc, mut out) = run_guard(tree, mode, &base, sha, ov).await;
        if mode == "message" && rc == 1 && ov.allow_reused_message {
            out.push_str("\nverdict=land_message_reused_allowed (allow_reused_message)");
            tracing::warn!(lane, sha, phase, measured = true, n_considered = 1, verdict = "land_message_reused_allowed",
                "a reused commit message was landed on request");
            rc = 0;
        }
        if rc > 1 {
            out.push_str(&format!("\nverdict=land_{mode}_unmeasured rc={rc} (the guard could not run; nothing refused on it)"));
            tracing::warn!(lane, sha, mode, phase, rc, measured = false, n_considered = 0, why_unmeasured = %tail(out.trim(), 300),
                verdict = "land_guard_unmeasured", "a land guard could not run");
            rc = 0;
        }
        land_log(lane, mode, phase, &out);
        if rc == 1 {
            let lines: Vec<String> = out.lines().filter(|l| l.starts_with("refused:") || l.starts_with("hint:")).map(String::from).collect();
            tracing::warn!(lane, sha, phase, measured = true, n_considered = 1, verdict, detail = %tail(&lines.join(" | "), 600),
                "a land was refused by a land guard");
            return Err(GuardRefusal { verdict, lines });
        }
    }
    Ok(())
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
    let named = body.get("tree").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir());
    let found = if lane.is_empty() { None } else { named.clone().or_else(|| crate::api::contract::lane_tree(&lane)) };
    let policy = match &found {
        Some(t) => repo_policy(t, "origin/main").await,
        None => RepoPolicy::default(),
    };
    let declared = policy.gate.is_some();
    if lane.is_empty() || !(declared || queue_on(&lane)) {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "land_queue_off",
            "error": "the server land queue (contract rule 5) is not on for this lane; land the ordinary way"}))).into_response();
    }
    let Some(tree) = found else {
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
    let ov = GuardOverrides {
        freeze: body.get("freeze_override").and_then(Value::as_str).map(String::from),
        allow_reused_message: body.get("allow_reused_message").and_then(Value::as_bool).unwrap_or(false),
    };
    if let Err(r) = check_guards(&tree, &lane, &sha, &ov, "enqueue").await {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": r.verdict, "verdict": r.verdict,
            "error": r.lines.iter().find(|l| l.starts_with("refused:")).cloned().unwrap_or_else(|| "refused by a land guard".into()),
            "refusals": r.lines, "sha": sha}))).into_response();
    }
    let now = crate::config::now_f64();
    let (l2, s2, r2, why2, ov2) = (lane.clone(), sha.clone(), repo.clone(), reason.clone(), ov.clone());
    let t2 = named.map(|p| p.to_string_lossy().into_owned());
    let id = state.store.write_async(move |conn| {
        conn.execute("INSERT INTO land_queue (repo, lane, sha, priority, reason, queued_at, freeze_override, allow_reused_message, tree)
                      VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![r2, l2, s2, priority as i64, why2, now, ov2.freeze, ov2.allow_reused_message as i64, t2])?;
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
    (StatusCode::ACCEPTED, Json(json!({"ok": true, "id": qid, "sha": sha, "ahead": ahead, "declared": declared,
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
    let one = q.get("id").and_then(|v| v.parse::<i64>().ok());
    type Row = (i64, String, String, String, i64, String, Option<String>, Option<String>, f64, Option<f64>, Option<String>);
    let rows: Vec<Row> = state.store.read_async(move |c| {
        let mut st = c.prepare("SELECT id, repo, lane, sha, priority, state, merged_sha, output, queued_at, done_at, gate_verdict FROM land_queue
                                WHERE (?2 IS NULL AND (state IN ('queued','running') OR queued_at >= ?1)) OR id = ?2 ORDER BY id")?;
        let v = st.query_map(rusqlite::params![since, one], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?)))?
            .collect::<rusqlite::Result<Vec<Row>>>()?;
        Ok(v)
    }).await.unwrap_or_default();
    let mut waits: Vec<f64> = rows.iter().filter_map(|r| r.9.filter(|_| r.8 >= since).map(|d| d - r.8)).collect();
    let n = waits.len();
    let p95 = wait_p95(&mut waits);
    let items: Vec<Value> = rows.iter().map(|r| json!({"id": r.0, "repo": r.1, "lane": r.2, "sha": r.3, "priority": r.4 != 0,
        "state": r.5, "merged_sha": if r.5 == "merged" { r.6.as_deref() } else { None },
        "push_candidate_sha": if matches!(r.5.as_str(), "queued" | "running") { r.6.as_deref() } else { None }, "output": r.7.as_deref().map(|o| tail(o, if one.is_some() { 4000 } else { 600 })), "queued_at": r.8, "done_at": r.9, "gate_verdict": r.10})).collect();
    let red = crate::api::land_red::episodes(&state, since).await;
    Json(json!({"since_h": since_h, "measured": true, "n_considered": n, "wait_p95_min": p95, "red": red,
        "why_unmeasured": if n == 0 { Some("no land finished in the window") } else { None },
        "items": items, "contract": "docs/orchestration-contract.md (rule 5)"})).into_response()
}

/// Asked by the shared git guard before a worker push. `push=1` means a push
/// is about to run, so a refusal is logged here (the guard cannot log to the
/// server's verdict stream itself).
async fn policy_route(Query(q): Query<HashMap<String, String>>) -> Response {
    let lane = q.get("lane").map(|s| s.trim().to_string()).unwrap_or_default();
    let tree = q.get("tree").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir());
    let policy = match &tree {
        Some(t) => repo_policy(t, "origin/main").await,
        None => RepoPolicy::default(),
    };
    let declared = policy.gate.is_some();
    let queue = !lane.is_empty() && (declared || queue_on(&lane));
    if queue && q.get("push").is_some_and(|v| v == "1") {
        tracing::info!(lane, measured = true, n_considered = 1, verdict = "worker_push_refused",
            "a worker push to main was refused; the lane lands through the server queue");
    }
    Json(json!({"lane": lane, "queue": queue, "declared": declared, "gate": policy.gate, "tree_read": tree.is_some(), "measured": true, "n_considered": 1,
        "how": "amux land (queues the commit on the server; you get a message when it lands)"})).into_response()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git").arg("-C").arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"]).args(args).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// origin (bare) plus one clone, with identity set so cherry-pick works.
    pub(crate) fn fixture(d: &Path) -> PathBuf {
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

    pub(crate) fn commit(tree: &Path, file: &str, body: &str) -> String {
        sh(tree, &["checkout", "-q", "--detach", "origin/main"]);
        std::fs::write(tree.join(file), body).unwrap();
        sh(tree, &["add", file]);
        sh(tree, &["commit", "-qm", &format!("change {file}")]);
        sh(tree, &["rev-parse", "HEAD"])
    }

    /// gs12-extra-2's land 119: the candidate is created once per repository
    /// and every later land resets the same checkout to origin/main, dropping
    /// a previous land's leftovers; a per-land c-* dir an hour old is swept.
    #[tokio::test]
    async fn the_land_candidate_is_reused_and_reset_per_land() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        sh(&lane, &["fetch", "-q", "origin"]);
        let main = sh(&lane, &["rev-parse", "origin/main"]);
        let old = home().join("tmp").join("land").join("c-1-1");
        sh(&lane, &["worktree", "add", "-q", "--detach", &old.to_string_lossy(), &main]);
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(7200);
        std::fs::File::open(&old).unwrap().set_modified(hour_ago).unwrap();
        let first = candidate(&lane, &main).await.unwrap();
        assert!(!old.exists(), "an hour-old per-land checkout is swept");
        std::fs::write(first.join("a.txt"), "dirty\n").unwrap();
        std::fs::write(first.join("stray.txt"), "x\n").unwrap();
        let ino = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(first.join(".git")).unwrap());
        let second = candidate(&lane, &main).await.unwrap();
        assert_eq!(first, second, "one candidate per repository");
        assert_eq!(ino, std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(second.join(".git")).unwrap()),
            "the candidate is reset in place, not checked out again");
        assert_eq!(std::fs::read_to_string(second.join("a.txt")).unwrap(), "1\n");
        assert!(!second.join("stray.txt").exists());
        assert_eq!(sh(&second, &["rev-parse", "HEAD"]), main);
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
            .map(|(i, s)| Entry { id: *i, repo: "r".into(), lane: "l".into(), sha: (*s).clone(), ov: Default::default(), ..Default::default() }).collect();
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
        let again = land(&lane, vec![Entry { id: 9, repo: "r".into(), lane: "l".into(), sha: a.clone(), ov: Default::default(), ..Default::default() }], Some(gate), "", None).await.unwrap();
        assert!(matches!(again[0].1, Outcome::Merged(_)));
    }

    pub(crate) fn app() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("land-test.db")).unwrap();
        crate::test_tempdirs::keep(dir);
        AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub(crate) async fn call(state: &AppState, method: &str, uri: &str, lane: &str, body: Value) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder().method(method).uri(uri)
            .header("content-type", "application/json").header("x-amux-session", lane)
            .body(axum::body::Body::from(body.to_string())).unwrap();
        let r = routes().with_state(state.clone()).oneshot(req).await.unwrap();
        let st = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    /// A batch whose row was reclaimed and claimed again finishes into
    /// nothing: the newer run keeps the row (implicit state machines #16).
    #[tokio::test]
    async fn a_finish_after_the_row_was_reclaimed_does_not_overwrite_the_newer_run() {
        let state = app();
        state.store.write(|conn| {
            conn.execute("INSERT INTO land_queue (id, repo, lane, sha, priority, queued_at, state, started_at) VALUES (51, 'r', 'lane-r', 'a', 0, 1, 'running', 200.0)", [])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let row = |s: &AppState| -> (String, Option<f64>) {
            s.store.read().unwrap().query_row("SELECT state, started_at FROM land_queue WHERE id = 51", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        // The old batch claimed it at 100; the row was reclaimed and re-claimed at 200.
        CLAIMED_AT.lock().unwrap().insert(51, 100.0);
        record(&state, &[(51, Outcome::Merged("m-old".into()))], &HashMap::new()).await;
        assert_eq!(row(&state), ("running".into(), Some(200.0)), "the stale finisher changed nothing");
        requeue(&state, &[Entry { id: 51, repo: "r".into(), lane: "lane-r".into(), sha: "a".into(), ov: Default::default(), ..Default::default() }], "x".into()).await;
        assert_eq!(row(&state).0, "running", "nor did its requeue");
        // The live claim finishes it.
        CLAIMED_AT.lock().unwrap().insert(51, 200.0);
        record(&state, &[(51, Outcome::Merged("m-new".into()))], &HashMap::new()).await;
        assert_eq!(row(&state).0, "merged");
        CLAIMED_AT.lock().unwrap().remove(&51);
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
        commit(&lane, "feature.txt", "x\n");
        sh(&lane, &["fetch", "-q", "origin", "main"]);
        let main = sh(&lane, &["rev-parse", "origin/main"]);
        let dir = candidate(&lane, &main).await.unwrap();
        sh(&lane, &["worktree", "lock", "--reason", "initializing", &dir.to_string_lossy()]);
        let path = d.path().join("locks/failed-candidate");
        let lock = match acquire_lock(&path, Duration::from_millis(100), Duration::from_millis(10)).await {
            LockWait::Acquired(lock) => lock,
            LockWait::TimedOut(_) => panic!("private lock unexpectedly held"),
        };
        // Cleanup's consumer must remove a real locked candidate. The process
        // case additionally fails real creation before exercising requeue.
        cleanup_candidate(&lane, &dir).await;
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
    /// A direct POST /api/land runs the CLI's land guards: a commit changing a
    /// frozen path and a commit reusing a landed commit's message over other
    /// files are refused with a verdict; an ordinary commit, and either one
    /// with its explicit escape, is queued.
    #[tokio::test]
    async fn a_direct_post_is_refused_on_a_frozen_path_and_a_reused_message() {
        assert!(guard_py().is_some_and(|p| p.contains("land_freeze_checked") && p.contains("land_message_reused")),
            "the CLI's _LAND_GUARD_PY is embedded");
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let lane = fixture(h);
        let seed = h.join("seed");
        std::fs::create_dir_all(seed.join(".amux")).unwrap();
        std::fs::write(seed.join(".amux/land.conf"),
            "[freeze \"gs12-proofs\"]\n\tpath = proofs/*\n\tuntil = 2099-01-01T00:00:00Z\n\towner = t\n\treason = pinned\n").unwrap();
        sh(&seed, &["add", ".amux/land.conf"]);
        sh(&seed, &["commit", "-qm", "freeze proofs"]);
        sh(&seed, &["push", "-q", "origin", "HEAD:main"]);
        sh(&lane, &["fetch", "-q", "origin", "main"]);
        std::fs::write(h.join("sessions/lane-g.env"), format!("CC_DIR=\"{}\"\nAMUX_CONTRACT_DONE=1\n", lane.display())).unwrap();
        let state = app();

        std::fs::create_dir_all(lane.join("proofs")).unwrap();
        let frozen = commit(&lane, "proofs/row.txt", "x\n");
        let (st, b) = call(&state, "POST", "/api/land", "lane-g", json!({"sha": frozen})).await;
        assert_eq!((st, b["verdict"].as_str()), (StatusCode::CONFLICT, Some("land_refused_frozen")), "{b}");
        assert!(b["error"].as_str().unwrap_or("").contains("gs12-proofs"), "{b}");

        sh(&lane, &["checkout", "-q", "--detach", "origin/main"]);
        std::fs::write(lane.join("b.txt"), "b\n").unwrap();
        sh(&lane, &["add", "b.txt"]);
        sh(&lane, &["commit", "-qm", "base"]); // the seed commit's message, over another file
        let reused = sh(&lane, &["rev-parse", "HEAD"]);
        let (st, b) = call(&state, "POST", "/api/land", "lane-g", json!({"sha": reused})).await;
        assert_eq!((st, b["verdict"].as_str()), (StatusCode::CONFLICT, Some("land_refused_reused_message")), "{b}");

        let n: i64 = state.store.read().unwrap().query_row("SELECT count(*) FROM land_queue", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "nothing refused was queued");
        let log = std::fs::read_to_string(h.join("logs/land.log")).unwrap();
        assert!(log.contains("lane-g freeze guard: refused:") && log.contains("verdict=land_message_reused"), "{log}");

        let ok = commit(&lane, "ok.txt", "ok\n");
        assert_eq!(call(&state, "POST", "/api/land", "lane-g", json!({"sha": ok})).await.0, StatusCode::ACCEPTED);
        let (st, b) = call(&state, "POST", "/api/land", "lane-g", json!({"sha": frozen, "freeze_override": "gs12-proofs: re-proved"})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
        let (st, b) = call(&state, "POST", "/api/land", "lane-g", json!({"sha": reused, "allow_reused_message": true})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");
    }

    /// A freeze that reaches origin/main after a land was queued still stops
    /// it at its turn (verdict=land_refused_frozen phase=turn, row refused
    /// with the reason); an entry queued with freeze_override passes, read
    /// back from its row.
    #[tokio::test]
    async fn a_freeze_declared_after_enqueue_refuses_the_land_at_its_turn() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let lane = fixture(h);
        std::fs::write(h.join("sessions/lane-t.env"), format!("CC_DIR=\"{}\"\nAMUX_CONTRACT_DONE=1\n", lane.display())).unwrap();
        let state = app();
        let plain = commit(&lane, "proofs-row.txt", "x\n");
        let (st, b) = call(&state, "POST", "/api/land", "lane-t", json!({"sha": plain})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "no freeze yet: {b}");
        let overridden = commit(&lane, "proofs-other.txt", "y\n");
        let (st, b) = call(&state, "POST", "/api/land", "lane-t", json!({"sha": overridden, "freeze_override": "gs12-proofs: re-proved"})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{b}");

        let seed = h.join("seed");
        std::fs::create_dir_all(seed.join(".amux")).unwrap();
        std::fs::write(seed.join(".amux/land.conf"),
            "[freeze \"gs12-proofs\"]\n\tpath = proofs-*\n\tuntil = 2099-01-01T00:00:00Z\n\towner = t\n\treason = pinned\n").unwrap();
        sh(&seed, &["add", ".amux/land.conf"]);
        sh(&seed, &["commit", "-qm", "freeze proofs"]);
        sh(&seed, &["push", "-q", "origin", "HEAD:main"]);

        assert_eq!(tick(&state).await, 1);
        for _ in 0..300 {
            let rows: Vec<(String, String, Option<String>)> = state.store.read().unwrap()
                .prepare("SELECT sha, state, output FROM land_queue ORDER BY id").unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect();
            if rows.iter().all(|r| r.1 != "queued" && r.1 != "running") {
                assert_eq!((rows[0].0.as_str(), rows[0].1.as_str()), (plain.as_str(), "refused"), "{rows:?}");
                let why = rows[0].2.clone().unwrap_or_default();
                assert!(why.contains("verdict=land_refused_frozen phase=turn") && why.contains("gs12-proofs"), "{why}");
                assert_eq!((rows[1].0.as_str(), rows[1].1.as_str()), (overridden.as_str(), "merged"), "{rows:?}");
                let log = std::fs::read_to_string(h.join("logs/land.log")).unwrap();
                assert!(log.contains("lane-t freeze guard: refused:") && log.contains("phase=turn"), "{log}");
                assert!(log.contains("verdict=land_freeze_override"), "{log}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the batch never finished");
    }

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
        let without = land(&lane, vec![Entry { id: 1, repo: "r".into(), lane: "l".into(), sha: a.clone(), ov: Default::default(), ..Default::default() }], None, "", None).await.unwrap();
        match &without[0].1 {
            Outcome::Refused(why) => assert!(why.contains("land-lock: REFUSED"), "{why}"),
            o => panic!("a push that does not carry the holder pid must be refused while the lock is held: {o:?}"),
        }
        let with = land(&lane, vec![Entry { id: 2, repo: "r".into(), lane: "l".into(), sha: a.clone(), ov: Default::default(), ..Default::default() }], None, "", Some(&held)).await.unwrap();
        assert!(matches!(with[0].1, Outcome::Merged(_)), "the holder's own push passes: {:?}", with[0].1);
        drop(held);
        assert!(!lock.exists());
    }

    /// Put `files` on origin/main through the seed clone, and update the lane.
    pub(crate) fn seed_commit(root: &Path, lane: &Path, files: &[(&str, &str)]) {
        let seed = root.join("seed");
        sh(&seed, &["fetch", "-q", "origin", "main"]);
        sh(&seed, &["reset", "-q", "--hard", "origin/main"]);
        for (f, body) in files {
            if let Some(d) = Path::new(f).parent() {
                std::fs::create_dir_all(seed.join(d)).unwrap();
            }
            std::fs::write(seed.join(f), body).unwrap();
            sh(&seed, &["add", f]);
        }
        sh(&seed, &["commit", "-qm", "seed files"]);
        sh(&seed, &["push", "-q", "origin", "HEAD:main"]);
        sh(lane, &["fetch", "-q", "origin", "main"]);
    }

    fn entry(id: i64, sha: &str) -> Entry {
        Entry { id, repo: "r".into(), lane: "l".into(), sha: sha.into(), ..Default::default() }
    }

    pub(crate) fn on_main(lane: &Path) -> String {
        sh(lane, &["fetch", "-q", "origin"]);
        sh(lane, &["ls-tree", "-r", "--name-only", "origin/main"])
    }

    /// Z2's point. Each commit is green on main alone and only the second is
    /// red on the tree composed of what is ahead of it: it is refused, the first
    /// lands. A gate run on each commit alone would have landed both.
    #[tokio::test]
    async fn an_entry_is_tested_on_the_tree_composed_of_everything_ahead_of_it() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        seed_commit(d.path(), &lane, &[(".amux/land.conf", "[amux]\n\tqueueGate = test ! -e x.txt -o ! -e y.txt\n")]);
        let x = commit(&lane, "x.txt", "x\n");
        let y = commit(&lane, "y.txt", "y\n");
        let out: HashMap<i64, Outcome> = land(&lane, vec![entry(1, &x), entry(2, &y)], None, "", None).await.unwrap().into_iter().collect();
        assert!(matches!(out[&1], Outcome::Merged(_)), "{:?}", out[&1]);
        match &out[&2] {
            Outcome::Refused(why) => assert!(why.contains("declared gate") && why.contains("everything ahead"), "{why}"),
            o => panic!("the second entry is red on the composed tree: {o:?}"),
        }
        let files = on_main(&lane);
        assert!(files.contains("x.txt") && !files.contains("y.txt"), "{files}");
    }

    /// The gate is origin/main's: neither editing the script it names nor
    /// deleting the declaration in the change itself gets a red tree through.
    #[tokio::test]
    async fn a_change_cannot_weaken_its_own_gate() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        seed_commit(d.path(), &lane, &[
            ("gate.sh", "#!/bin/sh\ntest ! -e bad.txt\n"),
            (".amux/land.conf", "[amux]\n\tqueueGate = sh gate.sh\n"),
        ]);
        sh(&lane, &["checkout", "-q", "--detach", "origin/main"]);
        std::fs::write(lane.join("gate.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(lane.join("bad.txt"), "x\n").unwrap();
        std::fs::write(lane.join(".amux/land.conf"), "").unwrap();
        sh(&lane, &["add", "-A"]);
        sh(&lane, &["commit", "-qm", "weaken the gate"]);
        let weak = sh(&lane, &["rev-parse", "HEAD"]);
        let out = land(&lane, vec![entry(1, &weak)], None, "", None).await.unwrap();
        assert!(matches!(&out[0].1, Outcome::Refused(why) if why.contains("declared gate")), "{:?}", out[0].1);
        assert!(!on_main(&lane).contains("bad.txt"));
    }

    /// A gate that cannot start says nothing about the commit: nothing merges,
    /// nothing bisects, and the refusal names how to fix the declaration. A gate
    /// that overruns its budget is the same.
    #[tokio::test]
    async fn a_gate_that_cannot_run_refuses_without_a_verdict_and_never_passes() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        seed_commit(d.path(), &lane, &[(".amux/land.conf", "[amux]\n\tqueueGate = no-such-gate-binary --x\n")]);
        let a = commit(&lane, "b.txt", "b\n");
        let c = commit(&lane, "c.txt", "c\n");
        let out = land(&lane, vec![entry(1, &a), entry(2, &c)], None, "", None).await.unwrap();
        assert_eq!(out.len(), 2);
        for (_, o) in &out {
            assert!(matches!(o, Outcome::Refused(why) if why.contains("could not start") && why.contains("AMUX_LAND_CLIENT=1")), "{o:?}");
        }
        assert!(!on_main(&lane).contains("b.txt"));
        seed_commit(d.path(), &lane, &[(".amux/land.conf", "[amux]\n\tqueueGate = sleep 5\n\tqueueGateTimeout = 1\n")]);
        let e = commit(&lane, "e.txt", "e\n");
        let out = land(&lane, vec![entry(3, &e)], None, "", None).await.unwrap();
        assert!(matches!(&out[0].1, Outcome::Refused(why) if why.contains("timed out") && why.contains("no verdict on your commit")), "{:?}", out[0].1);
        assert!(!on_main(&lane).contains("e.txt"));
    }

    /// `landFastGate` reaches the server's push, or a declared Mixpeek would
    /// run its full hook under the queue. The hook refuses without it.
    #[tokio::test]
    async fn the_declared_fast_gate_reaches_the_servers_push() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        let hook = lane.join(".git").join("hooks").join("pre-push");
        std::fs::write(&hook, "#!/bin/sh\n[ \"$LAND_PRECHECK\" = 1 ] || { echo full-hook-ran >&2; exit 1; }\n").unwrap();
        std::process::Command::new("chmod").arg("+x").arg(&hook).status().unwrap();
        seed_commit(d.path(), &lane, &[(".amux/land.conf", "[amux]\n\tqueueGate = true\n\tlandFastGate = true\n")]);
        let a = commit(&lane, "b.txt", "b\n");
        let out = land(&lane, vec![entry(1, &a)], None, "", None).await.unwrap();
        assert!(matches!(out[0].1, Outcome::Merged(_)), "{:?}", out[0].1);
    }

    /// The owner and receipt legs run with the lane named. A push from the
    /// server has no lane, so without this the queue skips `[owner]` paths.
    #[tokio::test]
    async fn the_repositorys_land_gate_judges_a_queued_commit_as_its_lane() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        seed_commit(d.path(), &lane, &[
            (".amux/land.conf", "[amux]\n\tqueueGate = true\n"),
            ("scripts/land_review.py", "\n"),
            ("scripts/land_gate.py", "import os,sys\nprint('owner leg for', os.environ.get('AMUX_SESSION'))\nsys.exit(1 if os.environ.get('AMUX_SESSION')=='l' else 0)\n"),
        ]);
        let a = commit(&lane, "b.txt", "b\n");
        let out = land(&lane, vec![entry(1, &a)], None, "", None).await.unwrap();
        assert!(matches!(&out[0].1, Outcome::Refused(why) if why.contains("owner leg for l")), "{:?}", out[0].1);
        assert!(!on_main(&lane).contains("b.txt"));
    }

    /// A conflict only in regenerable paths keeps main's copy and the declared
    /// generator re-renders the composed tree. A conflict anywhere else is
    /// still the lane's. Runs the embedded CLI's own _land_regenerate.
    #[tokio::test]
    async fn a_declared_regenerable_conflict_is_rendered_on_the_composed_tree() {
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let lane = fixture(d.path());
        seed_commit(d.path(), &lane, &[
            ("gen/out.txt", "0\n"),
            (".amux/land.conf", "[amux]\n\tqueueGate = grep -q rendered gen/out.txt\n\tlandRegenerate = mkdir -p gen && echo rendered > gen/out.txt\n\tlandRegenerable = gen/*\n"),
        ]);
        let lanes_copy = commit(&lane, "gen/out.txt", "lane\n");
        seed_commit(d.path(), &lane, &[("gen/out.txt", "main\n")]);
        let out = land(&lane, vec![entry(1, &lanes_copy)], None, "", None).await.unwrap();
        assert!(matches!(out[0].1, Outcome::Merged(_)), "{:?}", out[0].1);
        on_main(&lane);
        assert_eq!(sh(&lane, &["show", "origin/main:gen/out.txt"]), "rendered");
        let clash = commit(&lane, "a.txt", "lane side\n");
        seed_commit(d.path(), &lane, &[("a.txt", "main side\n")]);
        let out = land(&lane, vec![entry(2, &clash)], None, "", None).await.unwrap();
        assert!(matches!(&out[0].1, Outcome::Refused(why) if why.contains("conflicts with origin/main")), "{:?}", out[0].1);
    }

    #[test]
    fn only_the_gates_program_runs_in_mains_bytes() {
        assert_eq!(gate_program("scripts/g.sh \"$AMUX_LAND_BASE\" gen/data.txt").as_deref(), Some("scripts/g.sh"));
        assert_eq!(gate_program("FOO=1 bash -e scripts/g.sh x").as_deref(), Some("scripts/g.sh"));
        assert_eq!(gate_program("LAND_PRECHECK=1 .githooks/land-precheck a b").as_deref(), Some(".githooks/land-precheck"));
        assert_eq!(gate_program("/usr/bin/python3 tools/check.py").as_deref(), Some("tools/check.py"));
        assert_eq!(gate_program("grep -q rendered gen/out.txt").as_deref(), Some("grep"));
        assert_eq!(gate_program("env python3 scripts/check.py").as_deref(), Some("scripts/check.py"), "through env");
        assert_eq!(gate_program("env FOO=1 bash scripts/check.sh").as_deref(), Some("scripts/check.sh"));
        assert_eq!(gate_program("PYTHONPATH=/opt/lib python3 scripts/check.py").as_deref(), Some("scripts/check.py"), "an assignment with a path");
        assert_eq!(gate_program("A=1 B=/x/y scripts/g.sh").as_deref(), Some("scripts/g.sh"));
        assert_eq!(gate_program("sh").as_deref(), None);
    }

    #[test]
    fn a_regenerable_glob_matches_the_way_the_client_does() {
        assert!(glob_hit("gen/*", "gen/a/b.json"));
        assert!(glob_hit("a/*/c.yaml", "a/b/c.yaml"));
        assert!(glob_hit("exact.md", "exact.md"));
        assert!(!glob_hit("gen/*", "other/gen/a"));
        assert!(!glob_hit("exact.md", "exact.md.bak"));
        assert!(!glob_hit("a/*/c", "a/b/d"));
    }

    async fn wait_rows(state: &AppState, n: usize) -> Vec<(String, Option<String>, Option<String>)> {
        for _ in 0..600 {
            let rows: Vec<(String, Option<String>, Option<String>)> = {
                let conn = state.store.read().unwrap();
                let mut st = conn.prepare("SELECT state, gate_verdict, output FROM land_queue ORDER BY id").unwrap();
                let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect();
                v
            };
            if rows.len() >= n && rows.iter().all(|r| r.0 != "queued" && r.0 != "running") {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the queue never finished");
    }

    /// Through the real routes and job: a repository's declaration reaches a
    /// lane with rule 5 off and a session with no checkout of its own, with the
    /// row carrying its evidence; the switch turns declarations off; a
    /// repository with none lands as before and says so on the row.
    #[tokio::test]
    async fn a_declaration_reaches_every_lane_and_no_declaration_says_so() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        std::fs::create_dir_all(h.join("sessions")).unwrap();
        let _g = crate::api::settings::test_env::set_home(h);
        let lane = fixture(h);
        let state = app();
        let tree = lane.to_string_lossy().to_string();
        seed_commit(h, &lane, &[(".amux/land.conf", "[amux]\n\tqueueGate = true\n")]);
        let (_, p) = call(&state, "GET", &format!("/api/land/policy?lane=anyone&tree={tree}"), "", json!(null)).await;
        assert_eq!((&p["queue"], &p["declared"], &p["gate"]), (&json!(true), &json!(true), &json!("true")), "{p}");

        let sha = commit(&lane, "feature.txt", "x\n");
        let (st, b) = call(&state, "POST", "/api/land", "anyone", json!({"sha": sha, "tree": tree})).await;
        assert_eq!((st, &b["declared"]), (StatusCode::ACCEPTED, &json!(true)), "{b}");
        assert_eq!(tick(&state).await, 1);
        let rows = wait_rows(&state, 1).await;
        assert_eq!((rows[0].0.as_str(), rows[0].1.as_deref()), ("merged", Some("green")), "{rows:?}");
        let (base, from, to): (String, String, String) = state.store.read().unwrap().query_row(
            "SELECT base_sha, landed_base, landed_tip FROM land_queue", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        on_main(&lane);
        assert_eq!(to, sh(&lane, &["rev-parse", "origin/main"]));
        assert_eq!(from, base, "one entry on main: its range starts at the base");
        let (_, one) = call(&state, "GET", "/api/land?id=1", "", json!(null)).await;
        assert_eq!(one["items"].as_array().unwrap().len(), 1, "{one}");

        // The switch: the declaration is ignored and the policy says so.
        std::fs::write(h.join("server.env"), "AMUX_LAND_QUEUE_DECLARED=0\n").unwrap();
        let (_, p) = call(&state, "GET", &format!("/api/land/policy?lane=anyone&tree={tree}"), "", json!(null)).await;
        assert_eq!((&p["queue"], &p["declared"]), (&json!(false), &json!(false)), "{p}");
        let other = commit(&lane, "other.txt", "y\n");
        let (st, b) = call(&state, "POST", "/api/land", "anyone", json!({"sha": other, "tree": tree})).await;
        assert_eq!((st, b["code"].as_str()), (StatusCode::CONFLICT, Some("land_queue_off")), "{b}");
        std::fs::remove_file(h.join("server.env")).unwrap();

        // No declaration, on a rule-5 lane: lands, and the row says nothing tested it.
        seed_commit(h, &lane, &[(".amux/land.conf", "\n")]);
        std::fs::write(h.join("sessions/lane-u.env"), format!("CC_DIR=\"{}\"\nAMUX_CONTRACT_DONE=1\n", lane.display())).unwrap();
        let plain = commit(&lane, "plain.txt", "z\n");
        assert_eq!(call(&state, "POST", "/api/land", "lane-u", json!({"sha": plain})).await.0, StatusCode::ACCEPTED);
        assert_eq!(tick(&state).await, 1);
        let rows = wait_rows(&state, 2).await;
        assert_eq!((rows[1].0.as_str(), rows[1].1.as_deref()), ("merged", Some("undeclared")), "{rows:?}");
    }

    /// A row a dead image was running comes back and lands once. Its evidence
    /// and an accepted push survive: the receipt is adopted without a replay.
    #[tokio::test]
    async fn a_declared_land_left_running_by_a_dead_server_lands_exactly_once() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        let _g = crate::api::settings::test_env::set_home(h);
        let lane = fixture(h);
        let state = app();
        seed_commit(h, &lane, &[(".amux/land.conf", "[amux]\n\tqueueGate = true\n")]);
        let tree = lane.to_string_lossy().to_string();
        let repo = repo_key(&lane).await.unwrap();
        let sha = commit(&lane, "once.txt", "x\n");
        let pushed = sha.clone();
        sh(&lane, &["push", "-q", "origin", &format!("{pushed}:refs/heads/main")]);
        let (r, t, s2, p2) = (repo.clone(), tree.clone(), sha.clone(), pushed.clone());
        state.store.write(move |conn| {
            conn.execute("INSERT INTO land_queue (id, repo, lane, sha, priority, queued_at, state, started_at, tree, merged_sha, base_sha)
                          VALUES (7, ?1, 'l', ?2, 0, 1, 'running', ?3, ?4, ?5, 'b')", rusqlite::params![r, s2, crate::config::now_f64(), t, p2])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let before = sh(&lane, &["rev-parse", "origin/main"]);
        assert_eq!(tick(&state).await, 1);
        let rows = wait_rows(&state, 1).await;
        assert_eq!(rows[0].0, "merged", "{rows:?}");
        sh(&lane, &["fetch", "-q", "origin"]);
        assert_eq!(sh(&lane, &["rev-parse", "origin/main"]), before, "the receipt was adopted, nothing replayed");
    }

    #[test]
    fn the_wait_p95_is_the_95th_percentile_in_minutes() {
        let mut w: Vec<f64> = (1..=20).map(|m| m as f64 * 60.0).collect();
        assert_eq!(wait_p95(&mut w), Some(19.0));
        assert_eq!(wait_p95(&mut []), None, "no finished land is unmeasured, never zero");
    }
}
