//! In-flight `.mdai` runs: one run per entry node, joinable, cancellable, and
//! visible to any client that asks.
//!
//! WHY THIS EXISTS (Priorities.mdai, 2026-10-14). A run used to live inside the
//! HTTP request that started it, and nothing knew about it but that request.
//! Opening a node in the dashboard starts a refresh run, so ten opens inside
//! three minutes (the owner's phone and nine browser contexts on the same host)
//! started TEN full runs of a 21-node chain. Each one re-ran every upstream
//! window on sonnet (190 duplicate calls, $0.30-$0.55 each), then ten opus
//! synthesis calls over a 475 KB prompt fired at once and all ten died at the
//! 240s budget. The browser had long since stopped listening; the threads did
//! not stop, and the "running…" chip was a guess held in one tab's memory.
//!
//! Three properties fix that class rather than the one incident:
//!
//! 1. **Single flight per entry node.** A second request for a node that is
//!    already running JOINS the run in flight instead of starting another.
//!    `mdai_run_joined` is logged at WARN so a sweep can see how often callers
//!    would have duplicated work.
//! 2. **The run outlives the request, and its state is the server's.** A client
//!    that reloads mid-run asks [`snapshot`] and sees the same run, its elapsed
//!    time and the node it is on, rather than a lost spinner.
//! 3. **Cancel is real.** [`request_cancel`] sets a flag the model helper checks
//!    while it waits on the child, so the CLI process is stopped (its process
//!    group is killed) instead of running out its budget in the background.

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// What a finished run answered: the HTTP status and body a waiting caller
/// receives, kept so a client that reloads after the end still learns it.
#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub status: u16,
    pub body: Value,
    pub finished_ms: i64,
}

#[derive(Default, Debug)]
pub(crate) struct Progress {
    /// Root-relative path of the node currently being computed.
    pub node: String,
    /// The model the current node is calling, when it is calling one.
    pub model: String,
    /// When the current model call started (ms), or 0 between calls.
    pub call_started_ms: i64,
    /// Nodes finished so far (cached ones included).
    pub nodes_done: usize,
    /// Model calls actually spent so far.
    pub calls: usize,
}

pub(crate) struct ActiveRun {
    pub key: String,
    pub started_ms: i64,
    pub session: Option<String>,
    pub cancel: Arc<AtomicBool>,
    pub joiners: AtomicUsize,
    pub progress: Mutex<Progress>,
    done: Mutex<Option<Outcome>>,
    cv: Condvar,
}

impl ActiveRun {
    pub fn set_node(&self, node: &str, model: &str, calling: bool) {
        if let Ok(mut p) = self.progress.lock() {
            p.node = node.to_string();
            p.model = model.to_string();
            p.call_started_ms = if calling { now_ms() } else { 0 };
        }
    }
    pub fn node_done(&self, called_model: bool) {
        if let Ok(mut p) = self.progress.lock() {
            p.nodes_done += 1;
            if called_model {
                p.calls += 1;
            }
            p.call_started_ms = 0;
        }
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    pub fn json(&self) -> Value {
        let now = now_ms();
        let p = self.progress.lock().map(|p| {
            json!({
                "node": p.node,
                "model": p.model,
                "call_elapsed_ms": if p.call_started_ms > 0 { now - p.call_started_ms } else { 0 },
                "nodes_done": p.nodes_done,
                "calls": p.calls,
            })
        });
        json!({
            "path": self.key,
            "running": true,
            "started_ms": self.started_ms,
            "elapsed_ms": now - self.started_ms,
            "session": self.session,
            "cancel_requested": self.cancelled(),
            "joined": self.joiners.load(Ordering::SeqCst),
            "progress": p.unwrap_or(Value::Null),
        })
    }
}

static ACTIVE: LazyLock<Mutex<HashMap<String, Arc<ActiveRun>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// The last outcome per node, so a client that reloads after a run ended can
/// still be told how it ended. Bounded: oldest entries are dropped past 256.
static LAST: LazyLock<Mutex<HashMap<String, (i64, Outcome)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const LAST_CAP: usize = 256;

/// Start a run for `key`, or join the one already in flight. Returns the run
/// and whether this caller JOINED (true) rather than started it.
pub(crate) fn begin(key: &str, session: Option<String>) -> (Arc<ActiveRun>, bool) {
    let mut map = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(run) = map.get(key) {
        run.joiners.fetch_add(1, Ordering::SeqCst);
        tracing::warn!(target: "amux::mdai", path = key, started_ms = run.started_ms,
            joiners = run.joiners.load(Ordering::SeqCst), verdict = "mdai_run_joined",
            measured = true, n_considered = map.len(),
            "mdai run requested while the same node was already running; joined it \
             instead of starting a duplicate chain");
        return (run.clone(), true);
    }
    let run = Arc::new(ActiveRun {
        key: key.to_string(),
        started_ms: now_ms(),
        session,
        cancel: Arc::new(AtomicBool::new(false)),
        joiners: AtomicUsize::new(0),
        progress: Mutex::new(Progress::default()),
        done: Mutex::new(None),
        cv: Condvar::new(),
    });
    map.insert(key.to_string(), run.clone());
    (run, false)
}

/// Record the outcome, wake every waiter and retire the run.
pub(crate) fn finish(run: &Arc<ActiveRun>, outcome: Outcome) {
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.len() >= LAST_CAP {
            if let Some(oldest) = last.iter().min_by_key(|(_, (_, o))| o.finished_ms).map(|(k, _)| k.clone()) {
                last.remove(&oldest);
            }
        }
        last.insert(run.key.clone(), (run.started_ms, outcome.clone()));
    }
    {
        let mut map = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(&run.key).is_some_and(|r| Arc::ptr_eq(r, run)) {
            map.remove(&run.key);
        }
    }
    let mut done = run.done.lock().unwrap_or_else(|e| e.into_inner());
    *done = Some(outcome);
    run.cv.notify_all();
}

/// Block until the run finishes.
pub(crate) fn wait(run: &ActiveRun) -> Outcome {
    let mut done = run.done.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(o) = done.as_ref() {
            return o.clone();
        }
        done = run.cv.wait(done).unwrap_or_else(|e| e.into_inner());
    }
}

pub(crate) fn active(key: &str) -> Option<Arc<ActiveRun>> {
    ACTIVE.lock().ok()?.get(key).cloned()
}

/// The state a client shows for one node: running (with progress), or the
/// last outcome this process saw, or nothing.
pub(crate) fn snapshot(key: &str) -> Value {
    if let Some(run) = active(key) {
        return run.json();
    }
    let last = LAST.lock().ok().and_then(|m| m.get(key).cloned());
    match last {
        Some((started_ms, o)) => json!({
            "path": key,
            "running": false,
            "last": {
                "started_ms": started_ms,
                "finished_ms": o.finished_ms,
                "status": o.status,
                "ok": (200..300).contains(&o.status),
                "error": o.body.get("error").cloned().unwrap_or(Value::Null),
                "cycle": o.body.get("cycle").cloned().unwrap_or(Value::Null),
            },
        }),
        None => json!({ "path": key, "running": false, "last": null }),
    }
}

/// Every run in flight, for the list page.
pub(crate) fn all() -> Vec<Value> {
    let map = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<Value> = map.values().map(|r| r.json()).collect();
    v.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    v
}

/// Ask a run to stop. Returns false when nothing is running for `key`.
pub(crate) fn request_cancel(key: &str) -> bool {
    match active(key) {
        Some(run) => {
            run.cancel.store(true, Ordering::SeqCst);
            tracing::warn!(target: "amux::mdai", path = key,
                elapsed_ms = now_ms() - run.started_ms, verdict = "mdai_run_cancel_requested",
                measured = true, n_considered = 1, "mdai run cancel requested");
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Per-thread run control: the cancel flag and the per-node budget reach the
// model helper without threading new parameters through every ModelClient.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub(crate) struct Control {
    pub cancel: Option<Arc<AtomicBool>>,
    pub budget: Option<std::time::Duration>,
}

thread_local! {
    static CONTROL: RefCell<Control> = RefCell::new(Control::default());
}

/// Run `f` with `ctl` installed for this thread, restoring the previous one.
pub(crate) fn with_control<R>(ctl: Control, f: impl FnOnce() -> R) -> R {
    let prev = CONTROL.with(|c| std::mem::replace(&mut *c.borrow_mut(), ctl));
    let out = f();
    CONTROL.with(|c| *c.borrow_mut() = prev);
    out
}

/// The control installed on this thread, so a worker thread that computes part
/// of the same run (parallel upstream nodes) can install the same cancel flag.
pub(crate) fn current_control() -> Control {
    CONTROL.with(|c| c.borrow().clone())
}

/// Has the run on this thread been asked to stop?
pub(crate) fn cancel_requested() -> bool {
    CONTROL.with(|c| {
        c.borrow()
            .cancel
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
    })
}

/// The per-node budget override on this thread, if the node set one.
pub(crate) fn budget_override() -> Option<std::time::Duration> {
    CONTROL.with(|c| c.borrow().budget)
}

/// Replace only the budget (the cancel flag stays), for the node about to call.
pub(crate) fn set_budget(budget: Option<std::time::Duration>) {
    CONTROL.with(|c| c.borrow_mut().budget = budget);
}

// ---------------------------------------------------------------------------
// Per-node computation lock: two different runs that share an upstream node
// compute it once; the second waits and then finds the first one's output in
// the cache. Held only around the cache check and the model call, never while
// resolving sources, so lock order cannot form a cycle.
// ---------------------------------------------------------------------------

static NODE_LOCKS: LazyLock<Mutex<HashMap<std::path::PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn node_lock(path: &std::path::Path) -> Arc<Mutex<()>> {
    let mut m = NODE_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if m.len() > 4096 {
        m.retain(|_, l| Arc::strong_count(l) > 1);
    }
    m.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_request_joins_the_run_in_flight() {
        let (a, joined_a) = begin("t/join.mdai", None);
        let (b, joined_b) = begin("t/join.mdai", None);
        assert!(!joined_a && joined_b);
        assert!(Arc::ptr_eq(&a, &b), "the joiner must get the same run");
        finish(&a, Outcome { status: 200, body: json!({"output": "x"}), finished_ms: now_ms() });
        assert_eq!(wait(&b).status, 200);
        assert!(active("t/join.mdai").is_none(), "a finished run is retired");
        let (_c, joined_c) = begin("t/join.mdai", None);
        assert!(!joined_c, "after it finished, the next request starts a new run");
        let c = active("t/join.mdai").unwrap();
        finish(&c, Outcome { status: 504, body: json!({"error": "timed out"}), finished_ms: now_ms() });
        let s = snapshot("t/join.mdai");
        assert_eq!(s["running"], false);
        assert_eq!(s["last"]["error"], "timed out");
    }

    #[test]
    fn cancel_reaches_the_thread_running_the_node() {
        let (run, _) = begin("t/cancel.mdai", None);
        let ctl = Control { cancel: Some(run.cancel.clone()), budget: None };
        let seen = with_control(ctl, || {
            assert!(!cancel_requested());
            assert!(request_cancel("t/cancel.mdai"));
            cancel_requested()
        });
        assert!(seen);
        assert!(!cancel_requested(), "control is restored after the run");
        finish(&run, Outcome { status: 499, body: json!({}), finished_ms: now_ms() });
        assert!(!request_cancel("t/cancel.mdai"), "nothing to cancel once finished");
    }
}

// ---- Surviving a server restart ------------------------------------------
//
// WHY (2026-10-15). Every commit to main restarts the server, and main moved
// about every 15 minutes that day. A run lives on a thread of this process, so
// a restart erased it without a trace: Priorities.mdai and
// Fleet-Friction-Opportunities.mdai were cut off at 289s and again at 604s,
// `/active` answered `last: null`, and nothing ran them again. A long chain
// could not finish on a busy day at all.
//
// A run now writes a small journal file when it starts and removes it when it
// ends, however it ends. A file still present at boot is a run the restart cut
// off; the boot resumes it (completed upstream nodes are cache hits, so only
// the interrupted work repeats), up to RESUME_MAX attempts.

/// Attempts after the first before a run that keeps being cut off is dropped.
pub(crate) const RESUME_MAX: u32 = 3;

fn journal_dir() -> std::path::PathBuf {
    crate::config::amux_home().join("mdai").join("inflight")
}

fn journal_path(key: &str) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    journal_dir().join(format!("{:016x}.json", h.finish()))
}

/// Record that `key` is running (attempt 0 is the first try).
pub(crate) fn journal_start(key: &str, session: Option<&str>, attempt: u32) {
    let dir = journal_dir();
    let body = json!({ "key": key, "session": session, "attempt": attempt, "started_ms": now_ms() });
    let res = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(journal_path(key), body.to_string()));
    if let Err(e) = res {
        tracing::warn!(target: "amux::mdai", path = key, error = %e, verdict = "mdai_run_journal_write_failed",
            measured = true, n_considered = 1, "could not record the run; a restart would lose it");
    }
}

/// The run for `key` ended (finished, failed or cancelled): forget it.
pub(crate) fn journal_clear(key: &str) {
    let _ = std::fs::remove_file(journal_path(key));
}

/// Runs a previous process started and never ended: (key, session, attempt).
pub(crate) fn journal_pending() -> Vec<(String, Option<String>, u32)> {
    let Ok(rd) = std::fs::read_dir(journal_dir()) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let parsed = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        match parsed.as_ref().and_then(|v| v["key"].as_str().map(|k| (k.to_string(), v))) {
            Some((key, v)) => out.push((key, v["session"].as_str().map(str::to_string),
                v["attempt"].as_u64().unwrap_or(0) as u32)),
            None => { let _ = std::fs::remove_file(&p); }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod restart_journal_tests {
    use super::*;

    #[test]
    fn a_started_run_is_pending_until_it_ends() {
        let key = format!("journal-test-{}.mdai", now_ms());
        journal_start(&key, Some("owner"), 1);
        let found = journal_pending().into_iter().find(|(k, _, _)| *k == key);
        assert_eq!(found, Some((key.clone(), Some("owner".to_string()), 1)), "a run in flight is journaled");
        journal_clear(&key);
        assert!(journal_pending().iter().all(|(k, _, _)| *k != key), "an ended run leaves no journal entry");
    }
}
