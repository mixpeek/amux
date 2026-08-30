//! `POST /api/git/staged-guard` — the shared-checkout staged-state guard.
//!
//! # Invariant comments in this file must be PINNED, not narrated
//!
//! A 2026-08-21 mutation sweep (amux-frustrations, AF-127 follow-up) found
//! four protection-losing invariants here that were correct, deliberate,
//! explained in a comment — and held by nothing: forcing each passed all
//! 1171 tests. This file's comments are good enough to convince a reader an
//! invariant is enforced, which is exactly when nothing goes red. The rule,
//! deliberately NARROW so it stays true: a comment stating an invariant
//! whose violation LOSES PROTECTION (blocks stop firing, records outlive
//! their window, the newest observation gets dropped) must have a test that
//! fails when the enforcing line is removed — mutation-check it before
//! trusting it. Noise-adding invariants are exempt; a rule covering
//! everything becomes a chore and then a lie.
//!
//! # Why this file exists at all
//!
//! The endpoint is called by `.git/hooks/amux-staged-guard`, a script the
//! Python server generated per work_dir and which is therefore **installed on
//! machines and checkouts this repo cannot see**. Python is retired; the
//! generator went with it; the hooks did not. So the contract below is not a
//! design — it is a transcription of what the installed clients already speak,
//! recovered from `git show 792ce1f^:amux-server.py` (`_staged_guard_check`
//! py:19400, `_staged_guard_window` py:19187, route py:71813) and from the
//! generated hook on disk. **The server adapts to the hooks, never the
//! reverse.**
//!
//! Between the rust cutover and this file, `POST /api/git/staged-guard`
//! answered 405 — 1,147 times an hour — and the hook's `except Exception:
//! return 0  # fail open` swallowed every one of them. The guard printed
//! nothing and exited 0 on every commit, fleet-wide, which is precisely the
//! silent-permissiveness failure it exists to prevent (it was already
//! silently skipped once before, AC-261). That is the reason this module
//! reports `undecided` / `degraded` instead of quietly returning an empty
//! verdict: an empty `foreign` list and "I could not see half the fleet" are
//! the same bytes on the wire, and the hook cannot tell them apart unless the
//! server says so (ethos rule 4).
//!
//! # Request (from the hook)
//!
//! ```json
//! {"session": "<name>", "dir": "<git toplevel>", "paths": ["<repo-relative>"],
//!  "op": "commit", "guard_version": 2}
//! ```
//! plus `X-Amux-Session` (or `X-Amux-Worker`). The **header wins** over
//! `body.session`: provenance is the server-verified origin, never body text
//! (AMUX-1768). `op` and `guard_version` are optional — old hooks send
//! neither.
//!
//! # Response (what the installed hooks read)
//!
//! ```json
//! {"ok": true,
//!  "foreign":   [{"path","owner","age_secs","provenance","has_unstaged_changes","why"}],
//!  "shared":    [{"path","owner","peer","age_secs","has_unstaged_changes"}],
//!  "unclaimed": [{"path","has_unstaged_changes"}],
//!  "cotenants": ["<session>"], "window_secs": 21600}
//! ```
//! Every key is present on EVERY return path, including the disabled and
//! no-cotenant short circuits — python's own comment on that: a caller that
//! reads `d["shared"]` should get `[]`, not `None`. `foreign` non-empty means
//! the hook exits 1 and the commit is blocked.
//!
//! Fields this server ADDS (old hooks ignore them; the v2 hook prints them):
//! `undecided` + `reason` (no verdict was computable — do not read the empty
//! lists as "all clear"), `degraded` (a verdict was computed but may
//! UNDER-report, with the reasons), and `hook_outdated` (the caller sent no
//! `guard_version`, so it is a pre-rust hook that swallows server errors).
//!
//! # The classification, verbatim from python
//!
//! Attribution comes from each session's own JSONL transcript, not from git
//! state — git state is exactly what is ambiguous when N agents share one
//! index (AMUX-1730). Sessions are paired by **git repo root**, not by CC_DIR
//! string (AMUX-2337: in a monorepo those differ, and pairing by string left
//! 23 of 36 lanes invisible to the guard while it returned 200).
//!
//! - `foreign`  — a peer's first-hand write, no first-hand write of yours:
//!   BLOCKED. Also fires when you have no record of the path at all.
//! - `shared`   — you edited it too (or it is yours with unstaged changes):
//!   warned, never blocked. Blocking would deadlock a genuinely shared file.
//! - `unclaimed` — staged, and NO session has an edit record inside the
//!   window: warned. Not blockable (no owner to defer to) but silence here is
//!   what let 762e06e sweep a peer's staged work.

use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::session_verbs::{
    all_session_workdirs, iter_jsonl_tail, session_jsonl_path, session_work_dir,
};

/// py:19187 `_staged_guard_window` — floor 600s, default 6h.
const WINDOW_DEFAULT: f64 = 21600.0;
const WINDOW_FLOOR: f64 = 600.0;
/// AF-27: a self-edit counts as "strictly fresher" than a peer's only if it beats
/// it by MORE than this. NOT a tuning knob (ethos would object to one) — it is the
/// UNIT CONVERSION between the two clocks being compared: the committer's inferred
/// ts is a file MTIME (disk), the peer's is a transcript ts (process wall-clock),
/// and below the skew between them the ordering carries no information. Confirmed
/// by amux-frustrations' forensic: the AF-27 verdicts were bimodal — one coincident
/// near-tie at 0.2s (which flips sign across the two clocks — a coin toss) and five
/// real wins at 3462-14331s. Set at the skew floor: comfortably above sub-second
/// noise, three orders of magnitude below the real gap, so it blocks the near-tie
/// (762e06e reopens otherwise) without touching a genuine win.
const RECENCY_SKEW_MARGIN_S: f64 = 5.0;
/// py `_SHELL_WRITE_SLACK` — how far a file's mtime may sit from a Bash
/// command's timestamp and still count as that command's write.
const SHELL_WRITE_SLACK_DEFAULT: f64 = 180.0;
/// py `_iter_jsonl_tail(jf, max_bytes=4_000_000)`.
const JSONL_TAIL_BYTES: u64 = 4_000_000;
/// py `_edit_paths_cache` — 30s per (session, window, provenance).
const EDIT_CACHE_TTL: f64 = 30.0;
/// py `rel_paths[:2000]`. Truncation is REPORTED (see `degraded`): a silently
/// dropped path is an unguarded path.
const MAX_PATHS: usize = 2000;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// True the first time a notification key is seen in the last hour.
///
/// In-memory ON PURPOSE, with the cost named rather than hidden: this process
/// re-execs whenever the builder installs a new binary, so a restart forgets
/// and an owner may get one duplicate notice. That is the right trade here —
/// the failure this dedupes is spam from a retried pre-commit hook, and the
/// failure a persistent store would prevent is one extra message. Compare
/// ethos D1, where in-memory state silently DISABLED an optimisation; here the
/// worst case is a message arriving twice, which is self-evident to the reader.
pub fn notify_once(key: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static SEEN: std::sync::OnceLock<Mutex<HashMap<String, f64>>> = std::sync::OnceLock::new();
    let m = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    let now = now_epoch();
    let Ok(mut g) = m.lock() else { return true };
    g.retain(|_, t| now - *t < 3600.0);
    if g.contains_key(key) {
        return false;
    }
    g.insert(key.to_string(), now);
    true
}

fn now_epoch() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

use crate::config::env_f64;

fn window_secs() -> f64 {
    env_f64("AMUX_STAGED_GUARD_WINDOW_SECS", WINDOW_DEFAULT).max(WINDOW_FLOOR)
}

fn guard_enabled() -> bool {
    !matches!(
        std::env::var("AMUX_STAGED_GUARD").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// `os.path.realpath` semantics: resolve symlinks, but NEVER require the path
/// to exist. `Path::canonicalize` requires existence, and a staged path that
/// no longer exists is not an edge case here — it is a staged DELETION, one of
/// the two sweeps that happened on this checkout tonight. Falling back to the
/// unresolved string would key deletions under a different realpath than the
/// transcript's, so a peer's deletion would classify as `unclaimed` instead of
/// `foreign`: the guard would go quiet on exactly the case it was reported for.
fn realpath(p: &Path) -> String {
    if let Ok(c) = p.canonicalize() {
        return c.to_string_lossy().into_owned();
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    while let Some(parent) = cur.parent().map(Path::to_path_buf) {
        let Some(fname) = cur.file_name().map(|f| f.to_os_string()) else { break };
        tail.push(fname);
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out.to_string_lossy().into_owned();
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        cur = parent;
    }
    p.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

async fn git_out(dir: &str, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Has `owner` already COMMITTED their work on `path`, more recently than the
/// edit record that triggered the notice?
///
/// The victim-side notice fires on EDIT RECORDS, which cannot tell "edited and
/// committed" from "edited and still staged". Measured 2026-08-15: four notices
/// in one day for files whose owner had already committed, against one real
/// absorption. A recipient has to run `git log` every time to find out which.
///
/// This does not SUPPRESS the notice — the asymmetry forbids it. A false alarm
/// costs a `git log`; a missed alarm costs work that the sweep exists to save.
/// So the notice keeps firing and gains the discriminator instead, which is the
/// ethos answer: make the instrument express the difference rather than guess.
///
/// Authorship is by the Amux-Session TRAILER, not `%an` — every lane on this
/// machine commits as the same person, so `%an` cannot discriminate (CLAUDE.md
/// deploy section).
/// Is the NEWEST commit touching `path` the owner's own? (AMUX-3677)
///
/// Answers "is the owner's work in local HEAD", which is the question neither
/// `owner_committed_since` (defeated by a peer refreshing the owner's mtime
/// record) nor `LandedOnOrigin` (unreachable on a lane ahead of origin) can
/// answer on a checkout with unpushed commits.
///
/// Deliberately the NEWEST commit and not merely "any commit of theirs": if a
/// peer has committed this path since, the owner's bytes may have been changed
/// or reverted by that commit, and only the owner can judge it.
async fn owner_owns_newest_commit(dir: &str, path: &str, owner: &str) -> Option<String> {
    let out = git_out(
        dir,
        &["log", "-1", "--format=%h%x09%(trailers:key=Amux-Session,valueonly,separator=)", "--", path],
    )
    .await?;
    let mut it = out.trim().split('\t');
    let sha = it.next()?.trim();
    let who = it.next().unwrap_or("").trim();
    // An UNTRAILERED commit is not the owner's for this purpose. Reading a
    // missing trailer as a match would hand out receipts on anyone's commit,
    // which is the loud-to-quiet direction this guard must not take.
    (!sha.is_empty() && !owner.is_empty() && who == owner).then(|| sha.to_string())
}

pub(crate) async fn owner_committed_since(
    dir: &str,
    path: &str,
    owner: &str,
    edit_age_secs: i64,
) -> Option<String> {
    let out = git_out(
        dir,
        &["log", "-8", "--format=%h%x09%ct%x09%(trailers:key=Amux-Session,valueonly,separator=)", "--", path],
    )
    .await?;
    let now = now_epoch() as i64;
    for line in out.lines() {
        let mut it = line.split('\t');
        let (sha, ts, who) = (it.next()?, it.next()?, it.next().unwrap_or("").trim());
        if who != owner {
            continue;
        }
        let ts: i64 = ts.trim().parse().ok()?;
        // Their commit is STRICTLY NEWER than the edit that triggered this notice,
        // so the work in question is already theirs and in history.
        //
        // Strict, not `<=`: at equal age the two are indistinguishable at
        // one-second resolution, and the tie must fall to "not settled". Getting
        // this backwards tells a victim "nothing at risk" about work that is
        // still only staged — the one direction this function must never get
        // wrong. Caught by the 0-second case in the test below, not by review.
        if (now - ts) < edit_age_secs {
            return Some(sha.to_string());
        }
    }
    None
}

/// What actually happened to a path, for the victim notice. Three states,
/// because two of them need OPPOSITE responses and the first version of this
/// collapsed them (amux, 2026-08-15).
#[derive(Debug, PartialEq)]
pub(crate) enum PathFate {
    /// The owner committed it themselves. Nothing to do.
    SettledByOwner(String),
    /// The BYTES are in HEAD, but under someone else's commit — an absorption
    /// that already happened cleanly. The CODE is safe; only the REASONING is
    /// lost, so the response is "record it on the card", never "check this one".
    /// Keying only on the owner's own trailer reported this as at-risk, which
    /// cries wolf on every absorption that already went fine.
    AbsorbedBy(String, String),
    /// The worktree bytes are byte-identical to origin/main (AMUX-3445): the
    /// work is LANDED, whatever local HEAD thinks. Graft-push lanes ship by
    /// building a dangling commit from origin bytes and pushing it, so the
    /// shared checkout's local HEAD never advances and every grafted edit
    /// permanently reads "differs from HEAD, no commit of yours" — which made
    /// the at-risk warning fire on every peer commit sweeping the file,
    /// forever (two identical warnings in one hour, both resolving to
    /// byte-identical no-ops). Nothing identical to origin can be absorbed or
    /// reverted; carries origin's newest sha on the path for the receipt.
    LandedOnOrigin(String),
    /// The path differs from HEAD and the owner has no commit for it: the work
    /// is genuinely uncommitted and a sweep would take it. This is the state
    /// the AC-355 block exists to prevent.
    AtRisk,
}

/// The victim notice's per-path line, pure so the wording is testable
/// (MG-1484). Returns (line, counts_as_at_risk). The distinction the incident
/// demanded: an AtRisk path whose owner's record is a RESTORE carries no
/// authored content — "your WORK is at risk" plus "record your reasoning"
/// operated on an empty set, and the reader had to disprove the warning by
/// hand. Say what the record actually was.
pub(crate) fn victim_path_line(pth: &str, fate: &PathFate, provenance: &str) -> (String, bool) {
    match fate {
        PathFate::SettledByOwner(sha) => {
            (format!("  {pth}  — already committed by you in {sha}; nothing at risk"), false)
        }
        // Absorbed but SAFE. The bytes are in HEAD under someone else's
        // commit, so the code is fine and only the reasoning is stranded —
        // point at the card, do not send anyone hunting for lost work.
        // Reporting this as at-risk is what cries wolf on every absorption
        // that went fine.
        PathFate::AbsorbedBy(sha, who) => (
            format!(
                "  {pth}  — absorbed into {sha} under `{who}`; your CODE is safe, \
                 record the REASONING on the card"
            ),
            false,
        ),
        // AMUX-3445: landed on origin = a receipt, never a warning. The old
        // at-risk line here cost both sides a reconciliation cycle per peer
        // commit, forever, on graft-push lanes whose local HEAD never moves.
        PathFate::LandedOnOrigin(sha) => (
            format!(
                "  {pth}  — byte-identical to origin/main{}; nothing can be absorbed or \
                 reverted (local HEAD is just behind — graft-push lane)",
                if sha.is_empty() { String::new() } else { format!(" (as of {sha})") }
            ),
            false,
        ),
        PathFate::AtRisk if provenance == "restore" => (
            format!(
                "  {pth}  — your only recorded touch here is a RESTORE from a committed ref \
                 (no authored content of yours; MG-1484). Nothing of your work can be lost; \
                 if the restore mattered, re-check the path after their commit lands"
            ),
            false,
        ),
        // AN OBSERVED CLAIM IS CIRCUMSTANTIAL, and says so (AMUX-3778).
        //
        // The record is a cwd mtime the Bash hook pair caught, not a recorded
        // write. On a shared checkout that CANNOT distinguish your write from a
        // peer's write in the same window: the hook walks the command's cwd and
        // claims every file whose mtime moved, with no check that the command
        // touched it. Live specimen (AMUX-3763): mixpeek-general was warned
        // about radio-canada's brand-new file and both lanes lost a turn.
        //
        // Still flagged, still `true` for the at-risk count — the guard exists
        // because absorption is real and under-warning is the expensive
        // direction. What changes is that the sentence stops asserting a
        // recorded write it does not have.
        PathFate::AtRisk if provenance == "observed" => (
            format!(
                "  {pth}  — differs from HEAD and you have no commit for it. NOTE: your claim \
                 here is OBSERVED (a file mtime moved during one of your Bash commands), not a \
                 recorded edit. On a shared checkout a peer writing under your cwd looks \
                 identical from here, so check whether this is actually yours before acting"
            ),
            true,
        ),
        PathFate::AtRisk => (
            format!(
                "  {pth}  — differs from HEAD and you have no commit for it; \
                 the WORK ITSELF is at risk — CHECK THIS ONE"
            ),
            true,
        ),
    }
}

/// Decide between the three. Content-in-HEAD is checked with `git diff HEAD`,
/// which answers "are these bytes committed by ANYONE" — the question the
/// trailer-only check could not ask.
pub(crate) async fn path_fate(
    dir: &str,
    path: &str,
    owner: &str,
    edit_age_secs: i64,
    provenance: &str,
) -> PathFate {
    if let Some(sha) = owner_committed_since(dir, path, owner, edit_age_secs).await {
        return PathFate::SettledByOwner(sha);
    }
    // AN INFERRED RECORD IS NOT EVIDENCE THE OWNER WROTE ANYTHING (AMUX-3677).
    //
    // `owner_committed_since` asks whether the owner's commit is newer than the
    // EDIT that triggered this notice. For an `inferred` record that operand is
    // an mtime which moved during one of the owner's Bash commands — and a
    // PEER'S WRITE PRODUCES EXACTLY THAT. So the committing peer refreshes the
    // victim's record past the victim's own commit, and the check that should
    // have said "settled" misses.
    //
    // The rescue below cannot fire either: `LandedOnOrigin` needs the bytes to
    // match origin/main, and on a lane with unpushed commits — this repo, most
    // of the time, 44 commits deep while the push was blocked — that arm is
    // unreachable by construction. AMUX-3445 added it for the MIRROR lane, a
    // graft-push checkout whose local HEAD never advances; the lane whose HEAD
    // is AHEAD had no equivalent.
    //
    // Specimen, 2026-08-24 16:00: `browser.rs` reported "differs from HEAD and
    // you have no commit for it; the WORK ITSELF is at risk" to amux, whose
    // 02197674 was the newest commit on that path, twenty minutes old, with a
    // clean worktree.
    //
    // So: if the owner's record is INFERRED and the newest commit touching the
    // path is the OWNER'S OWN, their work is in HEAD and the current dirt
    // belongs to the committer. `firsthand` is deliberately excluded — a real
    // recorded edit newer than the owner's commit IS at risk, and that is the
    // direction this must never get wrong.
    if provenance == "inferred" {
        if let Some(sha) = owner_owns_newest_commit(dir, path, owner).await {
            return PathFate::SettledByOwner(sha);
        }
    }
    // Empty `git diff HEAD -- path` means the working tree matches HEAD, i.e.
    // whatever was written is committed — by someone.
    let dirty = git_out(dir, &["diff", "HEAD", "--name-only", "--", path])
        .await
        .map(|o| !o.trim().is_empty())
        .unwrap_or(true); // unreadable -> assume at risk, never reassure
    if dirty {
        // AMUX-3445: worktree-vs-HEAD is the wrong risk discriminator on a
        // graft-push checkout — local HEAD never advances there, so a landed
        // graft reads dirty forever. The discriminator that matters is
        // worktree-vs-ORIGIN: bytes identical to origin/main cannot be
        // absorbed or reverted. `git_out` is None on nonzero exit, so a real
        // difference, a missing origin ref, or any git failure all fall
        // through to AtRisk — the loud direction stays the default.
        //
        // BOTH trees, because the commit takes the STAGED blob (backend's
        // amendment, measured same-day: worktree == origin while the INDEX
        // held a pre-graft copy 44 lines behind — a receipt on that state
        // talks the victim out of checking while the pending commit would
        // revert their landed lines). Worktree-clean AND index-clean against
        // origin, or it stays AtRisk.
        if git_out(dir, &["diff", "--quiet", "origin/main", "--", path]).await.is_some()
            && git_out(dir, &["diff", "--quiet", "--cached", "origin/main", "--", path])
                .await
                .is_some()
        {
            let sha = git_out(dir, &["log", "origin/main", "-1", "--format=%h", "--", path])
                .await
                .unwrap_or_default()
                .trim()
                .to_string();
            return PathFate::LandedOnOrigin(sha);
        }
        return PathFate::AtRisk;
    }
    let last = git_out(
        dir,
        &["log", "-1", "--format=%h%x09%(trailers:key=Amux-Session,valueonly,separator=)", "--", path],
    )
    .await
    .unwrap_or_default();
    let mut it = last.trim().split('\t');
    let sha = it.next().unwrap_or("").trim().to_string();
    let who = it.next().unwrap_or("").trim().to_string();
    if sha.is_empty() {
        return PathFate::AtRisk;
    }
    // Self-absorption is SETTLED, not absorption. This arm is reachable with
    // who == owner when the owner's newest EDIT RECORD postdates their own
    // last commit (a peer's write moved the file's mtime under one of the
    // owner's commands, refreshing the record), so owner_committed_since
    // correctly misses — and the notice then read "absorbed into a08d120
    // under `amux`", ADDRESSED TO amux, about amux's own commit. Your own
    // commit carrying your bytes is nothing to reconcile.
    if who == owner {
        return PathFate::SettledByOwner(sha);
    }
    PathFate::AbsorbedBy(sha, if who.is_empty() { "(untrailered)".into() } else { who })
}

/// py:19380 `_repo_root`, memoized — a commit is latency-sensitive and this is
/// called once per session per check.
///
/// Deliberately diverges on ONE point: python memoized the empty string too,
/// so a directory probed before its repo existed stayed permanently
/// unresolvable, and unresolvable means the cotenant pairing silently falls
/// back to string equality (the AMUX-2337 blindness). Failures are not cached.
static REPO_ROOT_MEMO: Mutex<Option<BTreeMap<String, String>>> = Mutex::new(None);

async fn repo_root(dir: &str) -> Option<String> {
    if dir.is_empty() {
        return None;
    }
    if let Ok(g) = REPO_ROOT_MEMO.lock() {
        if let Some(hit) = g.as_ref().and_then(|m| m.get(dir)).cloned() {
            return Some(hit);
        }
    }
    let root = git_out(dir, &["rev-parse", "--show-toplevel"])
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    if let Ok(mut g) = REPO_ROOT_MEMO.lock() {
        g.get_or_insert_with(BTreeMap::new).insert(dir.to_string(), root.clone());
    }
    Some(root)
}

// ---------------------------------------------------------------------------
// Transcript-derived edit records (py:19207 `_session_recent_edit_paths`)
// ---------------------------------------------------------------------------

const EDIT_TOOL_NAMES: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// py `_PATHLIKE_RE`. Deliberately loose — the mtime check is what decides
/// ownership, not this regex.
fn pathlike_re() -> &'static regex::Regex {
    static RE: Mutex<Option<&'static regex::Regex>> = Mutex::new(None);
    let mut g = RE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = *g {
        return r;
    }
    // The pattern is a compile-time constant and known good; the leak is a
    // one-time 'static promotion, not per-call.
    let r: &'static regex::Regex = Box::leak(Box::new(
        regex::Regex::new(r"[\w./~-]*[\w-]+\.[A-Za-z0-9]{1,8}").expect("static regex"),
    ));
    *g = Some(r);
    r
}

/// One session's edit records plus whether we could actually SEE its
/// transcript. `transcript_found=false` is the difference between "this
/// session edited nothing" and "this session is invisible to the guard" —
/// identical `paths` maps, opposite meanings, and only the second one can let
/// a sweep through.
#[derive(Clone, Default)]
struct EditScan {
    paths: HashMap<String, f64>,
    /// Paths whose LATEST record came from a restore-shaped command
    /// (`git checkout <ref> -- p`, `git restore`). An edit record is not
    /// authored content (MG-1484): a restore writes bytes that equal a
    /// committed ref, so telling its author "your WORK is at risk" hands them
    /// a remedy that operates on an empty set. A later Edit/Write or mutating
    /// Bash on the same path clears the mark — kind follows the latest record.
    restores: HashMap<String, f64>,
    transcript_found: bool,
}

type EditCacheKey = (String, u64, bool);
static EDIT_CACHE: Mutex<Option<HashMap<EditCacheKey, (f64, EditScan)>>> = Mutex::new(None);

fn parse_ts(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_millis()) / 1000.0);
    }
    // `datetime.fromisoformat` also accepts a naive stamp; python then read it
    // as local time. Claude's transcripts are always Z-suffixed, so this is a
    // fallback, and UTC is the safe reading: guessing local could shift a
    // record by hours and silently drop it out of the window.
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|n| n.and_utc().timestamp() as f64)
}

/// AMUX-3128: does a shell command write a file via an output redirection?
/// `> f`, `>> f`, `&> f` write a file; fd dups (`2>&1`, `>&2`) do not. Used to
/// keep `is_pure_read_command` from treating `echo x > f` as a read.
fn has_output_redirection(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'>' {
            let mut j = i + 1;
            if j < b.len() && b[j] == b'>' {
                j += 1;
            }
            while j < b.len() && matches!(b[j], b' ' | b'\t') {
                j += 1;
            }
            // `>&...` is a file-descriptor dup/merge, not a write to a file.
            if j < b.len() && b[j] != b'&' {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// AMUX-3128: a purely read-only shell command must not mint an INFERRED edit
/// record. `recent_edit_paths` claims a Bash-named path when the file's mtime
/// moved within the slack window, but that cannot tell "I wrote foo.md" from "I
/// READ foo.md while a peer wrote it": a lane that ran `head -40 digests/x.md`
/// (a read) was flagged as a co-author of the digest because its mtime happened
/// to move under the digest producer's write. So if EVERY segment of the command
/// leads with a known read-only tool AND there is no output redirection, claim
/// nothing. Conservative on purpose: anything unrecognized (`sed -i`, `cp`, `mv`,
/// python heredocs, a prefixed `sudo head`) falls through to the mtime gate
/// exactly as before, so no real inferred WRITE loses its attribution — only the
/// unambiguous readers are excluded.
const READ_ONLY_VERBS: &[&str] = &[
    "ls", "cat", "head", "tail", "less", "more", "grep", "egrep", "fgrep",
    "rg", "ag", "wc", "stat", "file", "find", "cmp", "diff", "sort", "uniq",
    "cut", "column", "od", "xxd", "hexdump", "tree", "du", "basename",
    "dirname", "realpath", "readlink", "sha256sum", "md5sum", "nl", "tac",
    "pwd", "echo", "printf",
    // `cd` cannot modify anything, and its ABSENCE silently undid the
    // git-read exemption directly below: `git show f` was correctly a read,
    // while `cd /repo && git show f` was not, because the FIRST segment's
    // verb decided the whole command. `cd` is the most common prefix in this
    // repo's own workflow, so the exemption was defeated in the majority of
    // real invocations — 117 of 191 inferred-edit records in 24h had
    // verb=cd (AEAB-24). Safe because a real mutation still trips either the
    // output-redirection check above or its own non-read verb in a later
    // segment; `cd x && rm f`, `cd x && sed -i`, `cd x && git commit` and
    // `cd x && cat > f <<EOF` all remain non-read, and the test below pins
    // exactly that.
    "cd",
    ];
const GIT_READ_SUBCMDS: &[&str] = &[
    "show", "log", "diff", "status", "blame", "grep", "cat-file", "shortlog",
    "describe", "rev-parse", "rev-list", "ls-files", "ls-tree", "reflog",
    "whatchanged", "annotate", "name-rev", "show-ref", "for-each-ref",
    ];
fn is_pure_read_command(cmd: &str) -> bool {
    if has_output_redirection(cmd) {
        return false;
    }
    // `git <read-subcommand>` is how a careful VERIFIER reads a file — `git show`,
    // `git log --stat`, `git diff`, `git grep`, `git blame` to chase a specific
    // row (AMUX-3128 follow-up, gtm-ticker). The verb is `git`, which is NOT in
    // READ_ONLY_VERBS, so these minted an inferred edit record and flagged the
    // reader as co-author of the file they were only inspecting — blocking the
    // real committer, and worse, punishing careful verification: the harder a peer
    // checks your output, the more it blocks you, which trains toward GN=1 where
    // the guard stops protecting anything. WRITE subcommands (add/commit/checkout/
    // reset/restore/rm/mv/apply/stash/merge/rebase/pull/am/…) are ABSENT on
    // purpose, so a real working-tree mutation still falls through to the mtime
    // gate and keeps its attribution.
    let mut saw = false;
    for seg in cmd.split(['|', ';', '&', '\n', '(', ')', '`']) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        // AF-126: a comment segment writes nothing. `set -x; # note; cat f`
        // used to force the whole command non-read because verb `#` is not a
        // read verb — and with the mtime gate, that minted records off a
        // PEER's concurrent write (measured live: a lane's union-merge READS
        // of frustrations.md claimed the file while its author was writing
        // it, costing them a full-diff reconcile of their own commit).
        if seg.starts_with('#') {
            continue;
        }
        saw = true;
        let Some(tok) = seg.split_whitespace().next() else {
            return false;
        };
        let verb = Path::new(tok)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(tok);
        if verb == "git" {
            // Find the subcommand, skipping global flags and the argument of the
            // arg-taking ones (`-C <dir>`, `-c <cfg>`), then require it be read-only.
            // A bare `git` or a write/unknown subcommand falls through (return
            // false) exactly as before — conservative by design.
            let mut rest = seg.split_whitespace().skip(1);
            let mut sub = None;
            while let Some(t) = rest.next() {
                if t == "-C" || t == "-c" {
                    rest.next(); // consume its argument
                    continue;
                }
                if t.starts_with('-') {
                    continue; // other global flag, e.g. --no-pager
                }
                sub = Some(t);
                break;
            }
            match sub {
                Some(s) if GIT_READ_SUBCMDS.contains(&s) => continue,
                _ => return false,
            }
        }
        if !READ_ONLY_VERBS.contains(&verb) {
            return false;
        }
    }
    saw
}

/// MG-1484: is this command a RESTORE and nothing else? True when every
/// segment is a read, a git read, or a `git checkout`/`git restore` — the two
/// verbs that write bytes equal to a committed ref rather than authoring
/// anything — with no output redirection and at least one restore segment.
/// A mixed command (`git checkout origin/main -- f && sed -i … f`) is NOT a
/// restore: the sed authored content, so the record must read authored.
/// Conservative both ways: anything unrecognized falls through to authored,
/// which at worst repeats the old (over-warning) behavior.
fn is_restore_only_command(cmd: &str) -> bool {
    if has_output_redirection(cmd) {
        return false;
    }
    let mut saw_restore = false;
    for seg in cmd.split(['|', ';', '&', '\n', '(', ')', '`']) {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if is_pure_read_command(seg) {
            continue;
        }
        let Some(tok) = seg.split_whitespace().next() else {
            return false;
        };
        let verb = Path::new(tok).file_name().and_then(|s| s.to_str()).unwrap_or(tok);
        if verb != "git" {
            return false;
        }
        let mut rest = seg.split_whitespace().skip(1);
        let mut sub = None;
        while let Some(t) = rest.next() {
            if t == "-C" || t == "-c" {
                rest.next();
                continue;
            }
            if t.starts_with('-') {
                continue;
            }
            sub = Some(t);
            break;
        }
        match sub {
            Some("checkout") | Some("restore") => saw_restore = true,
            _ => return false,
        }
    }
    saw_restore
}

// ---------------------------------------------------------------------------
// OBSERVED edit records (AF-123)
// ---------------------------------------------------------------------------
//
// 75% of AF-27 blocks (105/140 in the retained log) hit lanes with
// firsthand=0 — and those lanes edit through Bash because bypass-permissions
// sessions are INSTRUCTED to prefer it, so firsthand records are a signal the
// harness itself makes unobtainable for them, then ranks them down for
// lacking (ethos rule 3). The inferred extractor cannot fix this by parsing
// harder: the specimen write was a `python3 - <<'PY'` heredoc rewriting the
// extensionless file `amux`, invisible to a pathlike regex twice over.
//
// So the record is OBSERVED instead of parsed: a Bash hook pair marks t0
// before the command and reports every file whose mtime moved during it.
// Observed records rank WITH firsthand — they are facts about the disk, not
// guesses about a command string — and no quoting can hide an mtime.

/// Matches the guard's default window; observed rows older than this are
/// pruned at write.
const OBSERVED_WINDOW_S: f64 = 21_600.0;
const OBSERVED_MAX_ROWS: usize = 500;

fn observed_key(session: &str) -> String {
    format!("observed_edits:{session}")
}

/// AF-130: an observed record must carry the FILE's write time, not the
/// hook's run time. The PostToolUse hook fires after the WHOLE Bash command,
/// so for the dominant bypass-permissions shape — edit and commit in ONE
/// compound call — a hook-time stamp postdates the commit by construction,
/// and `owner_committed_since` (strictly-newer commit wins) can then NEVER
/// return SettledByOwner for an observed record. The loud AtRisk line fired
/// on correctly-committed work on every such commit, which is how the one
/// notice that must be believed teaches lanes to skim it. So the hook sends
/// the mtime it already read (`{"path": .., "mtime": ..}`), and a bare
/// string row (an older installed hook copy) keeps hook-time — coverage
/// degrades toward over-warning, never toward silence. A future mtime clamps
/// to `now` so a skewed clock cannot mint a record that outlives the window.
pub(crate) fn parse_observed_reports(body: &Value, now: f64) -> Vec<(String, f64)> {
    body.get("paths")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::String(p) => Some((p.clone(), now)),
                    Value::Object(_) => {
                        let p = v.get("path").and_then(Value::as_str)?.to_string();
                        let ts = v
                            .get("mtime")
                            .and_then(Value::as_f64)
                            .map(|m| m.min(now))
                            .unwrap_or(now);
                        Some((p, ts))
                    }
                    _ => None,
                })
                .filter(|(p, _)| !p.trim().is_empty())
                .take(OBSERVED_MAX_ROWS)
                .map(|(p, ts)| (realpath(Path::new(&p)), ts))
                .collect()
        })
        .unwrap_or_default()
}

/// POST /api/git/observed-edits — the hook's report: `{paths: [..]}` with
/// `X-Amux-Session` naming the lane. Rows are bare paths or
/// `{path, mtime}` objects (see `parse_observed_reports`). Merged
/// newest-wins, pruned by window and row cap, stored in prefs (no migration:
/// rows are small and windowed).
pub async fn observed_edits(
    axum::extract::State(state): axum::extract::State<super::AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> (StatusCode, axum::Json<Value>) {
    let session = super::alerts::hdr_worker(&headers);
    if session.is_empty() || session == "api-anonymous" {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "X-Amux-Session required — an unattributed observation attributes nothing"})),
        );
    }
    let now = now_epoch();
    let paths = parse_observed_reports(&body, now);
    if paths.is_empty() {
        return (StatusCode::OK, axum::Json(json!({"ok": true, "stored": 0})));
    }
    let key = observed_key(&session);
    let n = paths.len();
    let write = state.store.write_async(move |conn| {
        let prior: HashMap<String, f64> = conn
            .query_row("SELECT value FROM prefs WHERE key=?1", [&key], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();
        let mut merged = prior;
        merged.retain(|_, ts| now - *ts <= OBSERVED_WINDOW_S);
        for (p, ts) in paths {
            let slot = merged.entry(p).or_insert(0.0);
            if ts > *slot {
                *slot = ts;
            }
        }
        // Row cap: drop OLDEST first, never newest — the newest observation
        // is the one the next commit is about.
        if merged.len() > OBSERVED_MAX_ROWS {
            let mut rows: Vec<(String, f64)> = merged.into_iter().collect();
            rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            rows.truncate(OBSERVED_MAX_ROWS);
            merged = rows.into_iter().collect();
        }
        conn.execute(
            "INSERT INTO prefs (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, serde_json::to_string(&merged).unwrap_or_default()],
        )?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    });
    match write.await {
        Ok(_) => (StatusCode::OK, axum::Json(json!({"ok": true, "stored": n}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"error": e.to_string()})),
        ),
    }
}

/// Load a session's observed records inside the window.
fn load_observed(conn: &rusqlite::Connection, session: &str, window: f64) -> HashMap<String, f64> {
    let now = now_epoch();
    conn.query_row(
        "SELECT value FROM prefs WHERE key=?1",
        [observed_key(session)],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| serde_json::from_str::<HashMap<String, f64>>(&v).ok())
    .map(|m| m.into_iter().filter(|(_, ts)| now - *ts <= window).collect())
    .unwrap_or_default()
}

/// AC-355's gate bit, pure so it is PINNED: any LIVE blind cotenant forces
/// unclaimed staged paths to BLOCK. "UNKNOWN treated as SAFE" was the whole
/// AC-355 bug, and the 2026-08-21 mutation sweep found this derivation held
/// by nothing — forcing it constant in EITHER direction passed all 1171
/// tests (forcing `false` silently re-opens AC-355). The construction site
/// in `staged_guard_inner` is the one residual unpinned line (a hardcoded
/// constant there would still evade; pinning it needs a liveness seam the
/// endpoint does not yet have).
fn gates_unclaimed(blind_live: &[String]) -> bool {
    !blind_live.is_empty()
}

/// Merge observed records into the guard inputs AT FIRSTHAND RANK (AF-123).
/// Pure, so the rank claim is testable: a lane whose only signal is observed
/// must read exactly like a lane that used the Edit tool.
pub(crate) fn apply_observed(
    inputs: &mut GuardInputs,
    mine_obs: &HashMap<String, f64>,
    theirs_obs: &[(String, HashMap<String, f64>)],
) {
    // AMUX-3497: an observed row EXPLAINED by the other side's TRANSCRIPT
    // record of the same path at the same instant is one write seen twice —
    // the mtime the observer's Bash window caught is the tool edit the other
    // session's transcript already attributes. Merging the echo minted a
    // phantom co-editor (live specimen: a session whose window held only
    // HTTP probes was named co-editor of board_store.rs, costing the
    // committer a wipe-apology sweep to an innocent peer). The echo test
    // runs against the ENTRY state of the firsthand sets — after the loop
    // below starts inserting, "firsthand" no longer means transcript.
    //
    // Both drops degrade toward protection: dropping a peer's echo of MY
    // transcript edit removes a warn about my own write; dropping MY echo of
    // a peer's transcript edit removes my counterclaim, so their firsthand
    // BLOCKS me (the AF-19 tie already blocked deliberately). The margin is
    // RECENCY_SKEW_MARGIN_S — the same transcript-clock vs mtime unit
    // conversion that constant exists for.
    let echo_of_theirs = |p: &String, ts: &f64| -> bool {
        inputs.theirs_firsthand.contains(p)
            && matches!(inputs.theirs.get(p), Some((_, tts)) if (*ts - *tts).abs() <= RECENCY_SKEW_MARGIN_S)
    };
    let echo_of_mine = |p: &String, ts: &f64| -> bool {
        inputs.mine_firsthand.contains(p)
            && matches!(inputs.mine.get(p), Some(mts) if (*ts - *mts).abs() <= RECENCY_SKEW_MARGIN_S)
    };
    let mine_keep: Vec<(String, f64)> = mine_obs
        .iter()
        .filter(|(p, ts)| !echo_of_theirs(p, ts))
        .map(|(p, ts)| (p.clone(), *ts))
        .collect();
    let theirs_keep: Vec<(String, String, f64)> = theirs_obs
        .iter()
        .flat_map(|(owner, obs)| {
            obs.iter()
                .filter(|(p, ts)| !echo_of_mine(p, ts))
                .map(move |(p, ts)| (owner.clone(), p.clone(), *ts))
        })
        .collect();
    for (p, ts) in mine_keep {
        // PROVENANCE FOR MY OWN CLAIM (AMUX-3662), the mirror of
        // `theirs_observed_only` below. Transcript rows are all loaded before
        // this runs, so `mine_firsthand` at this point means "I have a RECORDED
        // write here" — anything else is an mtime my Bash window caught, which
        // on a shared checkout includes writes I did not make.
        //
        // Checked BEFORE the insert, or the insert three lines down would make
        // every observed row look firsthand and mark nothing.
        if !inputs.mine_firsthand.contains(&p) {
            inputs.mine_observed_only.insert(p.clone());
        }
        let slot = inputs.mine.entry(p.clone()).or_insert(0.0);
        if ts > *slot {
            *slot = ts;
        }
        inputs.mine_firsthand.insert(p);
    }
    for (owner, p, ts) in theirs_keep {
        match inputs.theirs.get(&p) {
            Some((_, cur)) if *cur >= ts => {}
            _ => {
                // Kind-follows-latest: an observed write is authored
                // content as far as anyone can tell, never a restore.
                inputs.theirs_restore.remove(&p);
                // The WINNING claim for this path is now observation-based
                // (transcript rows are all loaded before this runs, so
                // nothing re-wins after; a losing observed row marks nothing).
                inputs.theirs.insert(p.clone(), (owner, ts));
                inputs.theirs_observed_only.insert(p.clone());
            }
        }
        inputs.theirs_firsthand.insert(p);
    }
}

/// AMUX-3446: the committer's own firsthand edit CONTENT per file, from their
/// transcript — new_string/content of Edit/Write/MultiEdit/NotebookEdit calls
/// inside the window, concatenated per abs realpath, capped so a rewrite-heavy
/// session cannot balloon a request. This is what staged hunks are accounted
/// against: peer records expire, the committer's own content at commit time
/// does not (you commit right after editing).
fn firsthand_edit_content(name: &str, since_secs: f64, cap_per_file: usize) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let Some(jf) = session_jsonl_path(name) else { return out };
    let cutoff = now_epoch() - since_secs;
    for e in iter_jsonl_tail(&jf, JSONL_TAIL_BYTES) {
        let Some(ts) = e.get("timestamp").and_then(Value::as_str).and_then(parse_ts) else {
            continue;
        };
        if ts < cutoff {
            continue;
        }
        let Some(content) = e.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
        else {
            continue;
        };
        for blk in content {
            if blk.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let tool = blk.get("name").and_then(Value::as_str).unwrap_or("");
            if !EDIT_TOOL_NAMES.contains(&tool) {
                continue;
            }
            let inp = blk.get("input");
            let fp = inp
                .and_then(|i| i.get("file_path"))
                .or_else(|| inp.and_then(|i| i.get("notebook_path")))
                .and_then(Value::as_str)
                .unwrap_or("");
            if fp.is_empty() {
                continue;
            }
            let slot = out.entry(realpath(Path::new(fp))).or_default();
            let mut push = |s: &str| {
                if slot.len() < cap_per_file && !s.is_empty() {
                    slot.push('\n');
                    slot.push_str(&s.chars().take(cap_per_file - slot.len().min(cap_per_file)).collect::<String>());
                }
            };
            if let Some(i) = inp {
                if let Some(s) = i.get("new_string").and_then(Value::as_str) {
                    push(s);
                }
                if let Some(s) = i.get("content").and_then(Value::as_str) {
                    push(s);
                }
                if let Some(s) = i.get("new_source").and_then(Value::as_str) {
                    push(s);
                }
                if let Some(edits) = i.get("edits").and_then(Value::as_array) {
                    for ed in edits {
                        if let Some(s) = ed.get("new_string").and_then(Value::as_str) {
                            push(s);
                        }
                    }
                }
            }
        }
    }
    out
}

/// AMUX-3446, the pure half: which staged ADDED lines does the committer's own
/// content NOT contain? Trivial lines (short after trim) are auto-accounted —
/// a bare brace or blank appears everywhere and would drown the signal. A miss
/// here means: on a shared checkout, this staged line is likely a PEER's
/// in-flight hunk riding a per-file `git add` (the 7797e45 sweep shape).
pub(crate) fn unaccounted_added_lines(added: &[String], own_content: &str) -> Vec<String> {
    added
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.len() >= 8)
        .filter(|l| !own_content.contains(*l))
        .map(str::to_string)
        .collect()
}

/// AMUX-3128 surfacing half: every INFERRED edit record (a Bash command whose
/// named path moved mtime within slack) is logged so the class stays countable.
/// A future false co-authorship now leaves a trace naming the verb — and if a
/// sweep sees a READ verb here (head/cat/ls...), a reader slipped
/// `is_pure_read_command` and the exclusion list is missing it. Deduped per
/// (session, path basename, verb) for an hour so legit `sed -i` churn cannot spam.
static INFERRED_WARNED: Mutex<Option<HashMap<String, f64>>> = Mutex::new(None);

/// AF-126: the verb of the segment that actually FAILED the read test — the
/// one the WARN's diagnostic sentence is about. Naming the FIRST segment
/// instead put `verb=cd` on 6,173 of 10,722 retained WARN lines (cd is
/// read-only under AEAB-24; a LATER segment wrote), and an operator applying
/// the sentence as written would have concluded ~75% false co-authorship
/// fleet-wide — the number was in a draft card before its author read the
/// function. Returns None for a pure-read command; "redirect" when only the
/// output redirection blocks.
fn first_blocking_verb(cmd: &str) -> Option<String> {
    for seg in cmd.split(['|', ';', '&', '\n', '(', ')', '`']) {
        let seg = seg.trim();
        if seg.is_empty() || seg.starts_with('#') {
            continue;
        }
        let Some(tok) = seg.split_whitespace().next() else {
            return Some("?".into());
        };
        let verb =
            Path::new(tok).file_name().and_then(|s| s.to_str()).unwrap_or(tok).to_string();
        if verb == "git" {
            let mut rest = seg.split_whitespace().skip(1);
            let mut sub = None;
            while let Some(t) = rest.next() {
                if t == "-C" || t == "-c" {
                    rest.next();
                    continue;
                }
                if t.starts_with('-') {
                    continue;
                }
                sub = Some(t);
                break;
            }
            match sub {
                Some(s) if GIT_READ_SUBCMDS.contains(&s) => continue,
                Some(s) => return Some(format!("git-{s}")),
                None => return Some("git".into()),
            }
        }
        if !READ_ONLY_VERBS.contains(&verb.as_str()) {
            return Some(verb);
        }
    }
    if has_output_redirection(cmd) {
        return Some("redirect".into());
    }
    None
}

fn warn_inferred_edit(session: &str, abs_path: &str, cmd: &str) {
    let verb = cmd
        .split(['|', ';', '&', '\n', '(', ')', '`'])
        .filter_map(|s| s.split_whitespace().next())
        .next()
        .map(|t| Path::new(t).file_name().and_then(|s| s.to_str()).unwrap_or(t))
        .unwrap_or("?");
    // The segment that FAILED the read test — the sentence below is about
    // THIS one, not the first (AF-126). `verb=` stays for grep continuity.
    let blocked_by = first_blocking_verb(cmd).unwrap_or_else(|| "?".into());
    let base = Path::new(abs_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(abs_path);
    let now = now_epoch();
    let key = format!("{session}\u{1}{base}\u{1}{verb}");
    if let Ok(mut g) = INFERRED_WARNED.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        if m.get(&key).is_some_and(|at| now - at < 3600.0) {
            return;
        }
        m.insert(key, now);
    }
    tracing::warn!(
        target: "staged_guard",
        "[staged-guard/inferred-edit AMUX-3128] session={} path={} verb={} blocked_by={} — \
         ownership INFERRED from a bash command, not a firsthand Edit/Write. A READ verb in \
         blocked_by means is_pure_read_command missed a reader and may be minting false \
         co-authorship; a WRITE verb there is the record working as designed (AF-126: the \
         first segment's verb is usually `cd` and says nothing).",
        if session.is_empty() { "(none)" } else { session },
        base,
        verb,
        blocked_by,
    );
}

/// py:19207. `{abs realpath: epoch ts}` of files this session edited inside
/// the window, read from its own JSONL transcript.
///
/// Two provenances, and callers must choose (AMUX-2456): FIRST-HAND is an
/// Edit/Write/MultiEdit `tool_use` naming the file — the transcript says this
/// session wrote it. INFERRED is a Bash command naming a path whose mtime
/// moved within the slack window; that catches `sed -i` and heredocs, but
/// mtime is a SHARED resource on a shared checkout, so it can claim a peer's
/// write. `firsthand_only` drops the inferred half.
fn recent_edit_paths(name: &str, since_secs: f64, firsthand_only: bool) -> EditScan {
    let now = now_epoch();
    let key: EditCacheKey = (name.to_string(), since_secs.to_bits(), firsthand_only);
    if let Ok(g) = EDIT_CACHE.lock() {
        if let Some((at, scan)) = g.as_ref().and_then(|m| m.get(&key)) {
            if now - at < EDIT_CACHE_TTL {
                return scan.clone();
            }
        }
    }
    let mut scan = EditScan::default();
    let slack = env_f64("AMUX_SHELL_WRITE_SLACK", SHELL_WRITE_SLACK_DEFAULT);
    // Base for RELATIVE paths named in shell commands. Python's comment: without
    // it the per-block `except` swallowed a NameError and the shell-write
    // detection was silently inert — shipped and doing nothing.
    let work_hint = session_work_dir(name);
    if let Some(jf) = session_jsonl_path(name) {
        scan.transcript_found = true;
        let cutoff = now - since_secs;
        for e in iter_jsonl_tail(&jf, JSONL_TAIL_BYTES) {
            let Some(ts) = e.get("timestamp").and_then(Value::as_str).and_then(parse_ts) else {
                continue;
            };
            if ts < cutoff {
                continue;
            }
            let Some(content) = e.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
            else {
                continue;
            };
            for blk in content {
                if blk.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let tool = blk.get("name").and_then(Value::as_str).unwrap_or("");
                if tool == "Bash" {
                    if firsthand_only {
                        continue;
                    }
                    let cmd = blk
                        .get("input")
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if cmd.is_empty() {
                        continue;
                    }
                    // AMUX-3128: a read-only command (head/ls/cat/grep...) names a
                    // path but writes nothing. The mtime gate below cannot tell a
                    // read from a write, so a PEER's write moving the mtime under
                    // a reader would mint a false co-author. Reads attribute nothing.
                    if is_pure_read_command(cmd) {
                        continue;
                    }
                    for m in pathlike_re().find_iter(cmd) {
                        let cand = m.as_str().trim_matches(|c| "'\"),;:".contains(c));
                        if cand.len() < 4 {
                            continue;
                        }
                        let abs = if Path::new(cand).is_absolute() {
                            PathBuf::from(cand)
                        } else if work_hint.is_empty() {
                            continue
                        } else {
                            Path::new(&work_hint).join(cand)
                        };
                        // MTIME IS WHAT KEEPS THIS HONEST: naming a path is not
                        // writing it. `grep x foo.rs` mentions foo.rs and changes
                        // nothing, so a path is only claimed if the file actually
                        // moved while the command ran.
                        let Ok(md) = std::fs::metadata(&abs) else { continue };
                        if !md.is_file() {
                            continue;
                        }
                        let Some(mt) = md
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_secs_f64())
                        else {
                            continue;
                        };
                        if (mt - ts).abs() <= slack {
                            let ap = realpath(&abs);
                            if mt > *scan.paths.get(&ap).unwrap_or(&0.0) {
                                warn_inferred_edit(name, &ap, cmd);
                                // Kind follows the latest record (MG-1484).
                                if is_restore_only_command(cmd) {
                                    scan.restores.insert(ap.clone(), mt);
                                } else {
                                    scan.restores.remove(&ap);
                                }
                                scan.paths.insert(ap, mt);
                            }
                        }
                    }
                    continue;
                }
                if !EDIT_TOOL_NAMES.contains(&tool) {
                    continue;
                }
                let inp = blk.get("input");
                let fp = inp
                    .and_then(|i| i.get("file_path"))
                    .or_else(|| inp.and_then(|i| i.get("notebook_path")))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !fp.is_empty() {
                    let rp = realpath(Path::new(fp));
                    // A firsthand Edit after a restore is authored content.
                    scan.restores.remove(&rp);
                    scan.paths.insert(rp, ts);
                }
            }
        }
    }
    if let Ok(mut g) = EDIT_CACHE.lock() {
        g.get_or_insert_with(HashMap::new).insert(key, (now, scan.clone()));
    }
    scan
}

// ---------------------------------------------------------------------------
// Classification — pure, so the FIRING path is testable (ethos rule 7)
// ---------------------------------------------------------------------------

/// Everything the verdict is computed from. Split out so `classify` takes no
/// filesystem, no git and no clock: the tests below fire the BLOCK path
/// directly, because a guard that has never been observed refusing is theatre.
#[derive(Default)]
pub(crate) struct GuardInputs {
    /// abs realpath -> ts, requesting session, both provenances.
    pub mine: HashMap<String, f64>,
    /// abs realpath, requesting session, first-hand only.
    pub mine_firsthand: HashSet<String>,
    /// abs realpath -> (owner session, newest ts), all cotenants.
    pub theirs: HashMap<String, (String, f64)>,
    /// abs realpath, any cotenant, first-hand only.
    pub theirs_firsthand: HashSet<String>,
    /// abs realpath whose WINNING peer claim came from an OBSERVED (mtime)
    /// row rather than a transcript record (AMUX-3497). An observed row is a
    /// fact about the disk, but on a shared checkout its attribution to the
    /// observing session is an inference — any concurrent session's write
    /// lands in the observer's mtime window. classify() uses this to say HOW
    /// a co-edit signal knows what it claims, so a phantom co-editor is
    /// labeled as possibly your own write seen twice instead of asserted.
    pub theirs_observed_only: HashSet<String>,
    /// abs realpath where the REQUESTING session's own claim is an OBSERVED
    /// (mtime) row rather than a transcript record (AMUX-3662).
    ///
    /// The mirror of `theirs_observed_only`, and its absence was the bug. The
    /// guard tracked provenance for the PEER's claim and not for the reader's,
    /// so `mine_age_secs` rendered identically whether it came from a write
    /// this session recorded or from an mtime that moved during one of its Bash
    /// commands. On a shared checkout those are very different facts.
    ///
    /// Live specimen 2026-08-24: probing `api/board.rs` returned
    /// `age_secs: 455, mine_age_secs: 455, owner: amux-frustrations` with no
    /// signal of any kind. The PEER's claim was real (commit 8575cc6f touched
    /// that file at 12:18:08); MY only contact was `sed -n '2270,2300p'`, a
    /// read. The equal ages are the tell of one write seen twice, and the
    /// response presented both claims in the same shape, so which one was
    /// inferred was not recoverable from the output.
    ///
    /// That symmetry is worse than a missing warning, because it gets read in
    /// whichever direction the reader already leans: the day before, the same
    /// signature was read as "the peer is a phantom co-editor" and cost a
    /// wipe-apology sweep to an innocent peer. On this specimen the phantom was
    /// the reader's own.
    pub mine_observed_only: HashSet<String>,
    /// abs realpath whose WINNING owner's latest record is a restore
    /// (MG-1484): an edit record without authored content. Drives the
    /// `provenance` field on foreign verdicts so the victim notice never
    /// tells a restorer their work is at risk.
    pub theirs_restore: HashSet<String>,
    /// abs realpath of files with UNSTAGED changes right now.
    pub dirty: HashSet<String>,
    /// Is ANY cotenant invisible to this verdict (transcript unreadable)?
    ///
    /// AC-355's block is scoped to this, on AMUX-2936's own measurement: the
    /// absorption vector IS blind cotenants — an invisible lane cannot produce a
    /// `foreign` row, so its staged work lands in `unclaimed`. With full
    /// attribution, `unclaimed` means something different and much less
    /// dangerous: nobody touched it in the window, most often the committer's
    /// own pre-window or tool-generated work. Blocking that too would add a
    /// blocking class to a guard already refusing ~18/hour, and AMUX-2936 named
    /// exactly that as what gets the guard switched off (ethos rule 3 — a
    /// constraint people cannot live with is one they disable).
    pub blind_cotenant: bool,
}

#[derive(Default)]
pub(crate) struct Verdict {
    pub foreign: Vec<Value>,
    pub shared: Vec<Value>,
    pub unclaimed: Vec<Value>,
    /// A peer's work is being SPLIT by this commit (AF-190).
    ///
    /// One row per (staged file a peer co-edited, that peer's dirty files this
    /// commit leaves behind). It is a hazard about the COMMIT, not about the
    /// tree, and it is the one thing no other check here can see.
    pub split_risk: Vec<Value>,
}

/// How the winning owner's claim to a path arose (MG-1484): `firsthand` (an
/// Edit/Write in their transcript), `inferred` (a Bash command + mtime), or
/// `restore` (a checkout/restore from a committed ref — an edit record with
/// NO authored content, which consumers must not call "work at risk").
fn provenance_of(inp: &GuardInputs, ap: &str) -> &'static str {
    if inp.theirs_restore.contains(ap) {
        "restore"
    // OBSERVED BEFORE FIRSTHAND, and the order is the whole fix (AMUX-3778).
    //
    // `apply_observed` inserts observed rows INTO `theirs_firsthand` on
    // purpose — AF-123, so a Bash-editing lane stops being penalised for a
    // signal the harness makes unobtainable for it (ethos rule 3). Correct,
    // and it made this function report every observed row as `firsthand`,
    // because the firsthand check came first and the set had just been
    // widened to include them.
    //
    // So the VICTIM NOTICE asserted a recorded write where the evidence was a
    // cwd mtime — while the JSON verdict, three hundred lines up, already
    // reported `their_provenance: "observed"` correctly off the same set. The
    // machine-readable field was right and the sentence a human reads was
    // wrong, which is the worse half to have wrong.
    //
    // This does NOT demote the record. Rank is unchanged and AF-123 holds; the
    // claim is still made, it is just described honestly.
    } else if inp.theirs_observed_only.contains(ap) {
        "observed"
    } else if inp.theirs_firsthand.contains(ap) {
        "firsthand"
    } else {
        "inferred"
    }
}

/// py:19470-19545. `paths` is (repo-relative, absolute realpath) pairs.
pub(crate) fn classify(
    paths: &[(String, String)],
    now: f64,
    window: f64,
    inp: &GuardInputs,
) -> Verdict {
    let mut v = Verdict::default();
    for (rel, ap) in paths {
        if rel.trim().is_empty() {
            continue;
        }
        let hit = inp.theirs.get(ap);
        let is_dirty = inp.dirty.contains(ap);
        // PROVENANCE STRENGTH, not mere presence (AF-19). `mine` includes the
        // INFERRED half, and this branch `continue`s — so an inferred
        // self-claim used to SUPPRESS the block below and downgrade it to a
        // note. Measured on the incident: a staged test file amux had never
        // opened classified `shared` because their `git add` named it while
        // its mtime was moving, and 762e06e swept it. A first-hand peer claim
        // outranks an inferred self-claim; both first-hand or both inferred is
        // still genuinely shared.
        if !inp.mine_firsthand.contains(ap) {
            if let Some((owner, ts)) = hit {
                // AF-27: a first-hand peer claim outranks an INFERRED self-claim,
                // but ownership was RECENCY-BLIND — a peer's stale first-hand edit
                // (measured: 14,355s / 4h old) outranked the committer's OWN edit
                // made 23.8s ago on the same path, blocking a commit whose every
                // staged hunk was the committer's (in_mine=true, in_firsthand=false).
                // The forensic split was decisive: of 405 per-path verdicts, 399 were
                // true positives (no self-record) and only the 6 with a self-record
                // AND a fresher self-edit were wrong. So: if the committer HAS an edit
                // record here and it is NEWER than the peer's, the fresher edit wins —
                // fall through to `shared` (warned, never blocked). The direction the
                // card carried was right ("you commit seconds after editing"); the
                // mechanism was backwards — not "your fresh edit is missing" but "the
                // peer's stale claim wins anyway". The 399 shape (no self-record, or a
                // genuinely fresher peer) still blocks.
                // Block UNLESS the committer's own edit is STRICTLY fresher than
                // the peer's. Ties block, and that is deliberate: the 762e06e sweep
                // (AF-19) is a TIE — the committer's inferred claim there is the
                // peer's own concurrent write caught by the same mtime event, so
                // the two timestamps coincide. Only a clear gap (AF-27's real case:
                // committer 23.8s vs peer 14,355s) proves the committer edited
                // AFTER the peer and owns the current content. No self-record at
                // all (the 399 shape) also blocks.
                // Per-path both sides: inp.mine[ap] is the committer's edit ts to
                // THIS path, ts is the peer's to THIS path (not a set-wide newest —
                // that asymmetry would under-block). Fresher by MORE than the
                // clock-skew margin only; a within-skew lead is a coin toss and blocks.
                let committer_fresher =
                    inp.mine.get(ap).map(|m| *m > *ts + RECENCY_SKEW_MARGIN_S).unwrap_or(false);
                if inp.theirs_firsthand.contains(ap) && !committer_fresher {
                    v.foreign.push(json!({
                        "path": rel,
                        "owner": owner,
                        "age_secs": (now - ts).max(0.0) as i64,
                        "provenance": provenance_of(inp, ap),
                        "has_unstaged_changes": is_dirty,
                        // Python emitted "your claim is inferred" here
                        // unconditionally, including when the committer had NO
                        // claim on the path at all — a false statement about
                        // the reader's own work, printed at the moment they are
                        // deciding whether to override. Same verdict, honest
                        // sentence: only say the claim was inferred when there
                        // actually is one (AF-26 applied to its own wording).
                        "why": if inp.mine.contains_key(ap) {
                            "they wrote it (transcript); your claim is inferred".to_string()
                        } else {
                            format!(
                                "they wrote it (transcript); you have no edit record on this path in the last {}m — basis is edit records, not the staged diff",
                                (window / 60.0) as i64
                            )
                        },
                    }));
                    continue;
                }
            }
        }
        if inp.mine.contains_key(ap) {
            if hit.is_some() || is_dirty {
                // `peer` distinguishes the two reasons this fires, which the
                // owner field alone cannot (AF-24): a real co-editor, versus
                // YOUR OWN file that merely has unstaged changes. The renderer
                // said "also edited by session '(unknown)'" for the second
                // case — a phantom co-editor on a solo edit, which is how a
                // real co-edit warning stops being read.
                let mut row = json!({
                    "path": rel,
                    "owner": hit.map(|(o, _)| o.as_str()).unwrap_or("(unknown)"),
                    "peer": hit.is_some(),
                    "age_secs": hit.map(|(_, ts)| (now - ts).max(0.0) as i64).unwrap_or(0),
                    // The committer's own edit age on this path (AMUX-3436):
                    // what lets a consumer ask owner_committed_since whether
                    // that edit is already settled in HEAD — a settled-mine +
                    // dirty-theirs path is NOT a contest, and telling its
                    // owner to stage per-hunk sends them into a needless
                    // git add -p over zero hunks of their own.
                    "mine_age_secs": inp.mine.get(ap).map(|ts| (now - ts).max(0.0) as i64),
                    // SAY WHERE EACH CLAIM CAME FROM (AMUX-3662). `mine_age_secs`
                    // used to render identically for a write this session
                    // RECORDED and an mtime that merely moved during one of its
                    // Bash commands, and only the peer's side ever carried a
                    // provenance marker. Two claims, same shape, one of them
                    // inferred and no way to tell which — so the equal-age
                    // coincidence gets read in whichever direction the reader
                    // already leans.
                    //
                    // Stated as a fact rather than a verdict: this does not
                    // decide who wrote the file, it says how each side knows.
                    "mine_provenance": if !inp.mine.contains_key(ap) {
                        "none"
                    } else if inp.mine_observed_only.contains(ap) {
                        "observed"
                    } else {
                        "transcript"
                    },
                    "their_provenance": if inp.theirs_observed_only.contains(ap) {
                        "observed"
                    } else {
                        "transcript"
                    },
                    "has_unstaged_changes": is_dirty,
                });
                // AMUX-3497, rule-4 half: when the peer's claim is an OBSERVED
                // mtime that COINCIDES with the committer's own record, the
                // co-edit signal may be one write seen through two sessions'
                // Bash windows — say so, instead of asserting a co-editor the
                // reader will go apologize to. Old hooks ignore unknown keys.
                //
                // AF-179 WIDENS THE GATE AND STOPS IT DECIDING. The conjunct
                // below used to be `coincident`, a 5-SECOND window, and that
                // encodes exactly one story: two sessions observed ONE write.
                // It cannot express the story that actually produces a wrong
                // name — one session's ONGOING AUTHORSHIP, sampled once by a
                // peer's long Bash window. Measured on the reported specimen:
                // amux authored scripts/token-baseline.py until ~20:29 and
                // committed at 20:30; amux-frustrations' `cargo test` walk had
                // sampled it at 20:10 and filed an observed record. The gap was
                // ~1000s against a 5s margin, 200x over, so the caveat stayed
                // silent and the guard asserted a co-editor who had never opened
                // the file. The two clocks drift apart in proportion to how long
                // the real author kept working, so the hedge was LEAST able to
                // fire exactly where the false attribution is most confusing.
                //
                // The fix is not a wider window. Picking a window at all is the
                // tell that we are guessing (ethos rule 7), and the server
                // genuinely cannot resolve observed-vs-observed — the comment on
                // case (d) in the tests has said so all along. So stop deciding
                // and state PROVENANCE, which is a fact: an observed claim is an
                // mtime that moved during that session's Bash command, not a
                // write that session recorded. The skew now only picks WORDING;
                // it no longer gates whether the reader is told anything.
                let coincident = hit
                    .map(|(_, tts)| {
                        inp.mine
                            .get(ap)
                            .map(|m| (m - tts).abs() <= RECENCY_SKEW_MARGIN_S)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if hit.is_some() && inp.theirs_observed_only.contains(ap) {
                    let gap = hit.and_then(|(_, tts)| inp.mine.get(ap).map(|m| m - tts));
                    let signal = match (coincident, gap) {
                        (true, _) => "observed mtime coinciding with your own edit — possibly \
                             one write seen through two sessions' Bash windows, not a real \
                             co-editor (AMUX-3497)"
                            .to_string(),
                        (false, Some(g)) if g > 0.0 => format!(
                            "OBSERVED claim, not a recorded write: that session's Bash command \
                             saw this file's mtime move. Your own record is {}s NEWER, so their \
                             sample may be a snapshot of YOUR ongoing authorship rather than an \
                             edit of theirs (AF-179)",
                            g as i64
                        ),
                        (false, _) => "OBSERVED claim, not a recorded write: that session's \
                             Bash command saw this file's mtime move under the repo, which on a \
                             shared checkout includes writes it did not make (AF-179)"
                            .to_string(),
                    };
                    row.as_object_mut()
                        .expect("shared row is an object")
                        .insert("co_signal".into(), json!(signal));
                }
                v.shared.push(row);
            }
            continue;
        }
        if let Some((owner, ts)) = hit {
            v.foreign.push(json!({
                "path": rel,
                "owner": owner,
                "age_secs": (now - ts).max(0.0) as i64,
                "provenance": provenance_of(inp, ap),
                "has_unstaged_changes": is_dirty,
                // WHY, recorded on the verdict (AF-26): a block that says only
                // "edited by X" cannot be told apart from a block that is
                // wrong. The basis is edit RECORDS in a window — this never
                // reads the staged patch — so say that where the reader is.
                "why": format!(
                    "no edit of yours on this path in the last {}m; basis is edit records, not the staged diff",
                    (window / 60.0) as i64
                ),
            }));
            continue;
        }
        // AC-355: UNCLAIMED BLOCKS **WHEN A COTENANT IS INVISIBLE**. It used to warn, on the reasoning that
        // there is "no owner to defer to" — which treated UNKNOWN as SAFE, and
        // that is the whole bug. Staged, and no session can account for it,
        // means someone put it in the shared index and the guard cannot see
        // who; on a shared checkout the likeliest who is a peer whose
        // transcript is unreadable.
        //
        // Measured twice in one day, both PURE sweeps where the committer had
        // no record of the file at all: 7d0e95d took a peer's invariants/
        // checks.rs + monitor.rs, and 6217dc0 — MY OWN commit improving this
        // very guard — took a peer's api/alerts.rs + the amux CLI. The guard
        // warned on both and blocked neither. A guard-improvement commit being
        // itself a sweep is the argument.
        //
        // Emitted as `foreign` rather than a new key on purpose: installed
        // hooks live on checkouts this repo cannot see and block on exactly one
        // thing, `foreign` being non-empty (module docs — the server adapts to
        // the hooks, never the reverse). A new field would be ignored by every
        // hook already on disk, which is the same silence in a new spelling.
        //
        // NOT CLOSED by this, and named so a reader does not over-trust the
        // block: the BOTH-EDITED case. When two sessions edit the same file the
        // shared index holds one blob — whoever `git add`-ed last — and the
        // committer HAS a record for that path, so this never fires while the
        // staged bytes may still be the peer's. That needs content-level
        // detection (staged blob vs the committer's last-written bytes) or
        // per-lane worktrees (MI-4183).
        if !inp.blind_cotenant {
            // Fully-attributed checkout: nobody's, but nobody is hidden either.
            // Stays a warning, as before.
            v.unclaimed.push(json!({"path": rel, "has_unstaged_changes": is_dirty}));
            continue;
        }
        v.foreign.push(json!({
            "path": rel,
            "owner": "",
            "age_secs": 0,
            "has_unstaged_changes": is_dirty,
            "why": format!(
                "staged, but NO session has an edit record for it in the last {}m — including \
                 you — AND a cotenant on this checkout is invisible to the guard right now, so \
                 the likeliest owner is the lane we cannot see. Committing it ships their work \
                 under your message. If it is genuinely yours, AMUX_VERIFIED_SOLO=1 after \
                 checking `git diff --cached -- {}`",
                (window / 60.0) as i64,
                rel
            ),
        }));
    }
    split_risk(paths, inp, &mut v);
    v
}

/// A COMMIT that cannot compile, from a TREE that can (AF-190).
///
/// THE SPECIMEN: `53ae4b8b` was the tip of `origin/main` and did not build.
/// Staging `api/board.rs` took ~16 lines of a peer's in-flight AMUX-3607 wiring
/// out of the same FILE, including a call to `effective_gate_trail`, which was
/// defined in `db/board_store.rs` and still uncommitted in their tree. The
/// caller's `cargo check` passed and the pre-commit gate passed, both correctly:
/// they check the TREE, which held the peer's definition. Nothing anywhere
/// builds the COMMIT.
///
/// The pathspec rule does not reach this. `git commit -- <paths>` protects
/// against sweeping whole unrelated FILES; the peer's work was in a file the
/// committer was legitimately editing, so file-granular staging takes it whole.
///
/// WHY THIS IS ALMOST FREE: the guard already holds both halves. It knows which
/// staged files a peer co-edited (that is `foreign`/`shared`) and it knows which
/// paths are `dirty`. One cross-reference turns them into the hazard. What it
/// costs is a set intersection over data already in memory.
///
/// IT STATES A FACT, IT DOES NOT POSE A QUESTION, and that is the load-bearing
/// design choice rather than a stylistic one. The guard ALREADY printed the
/// discriminating number for this specimen ("34 insertions / 9 deletions — if
/// that is MORE than you wrote, their work is in it") and its author read past
/// it, because 34 looked about right for the edit they had made. A number
/// matching your expectation is exactly where a check gets skipped. Any remedy
/// that asks the committer to do arithmetic fails the same way.
///
/// SILENT unless a peer who co-edited a STAGED file also has dirty work OUTSIDE
/// the commit. A warning that fires on every commit is one nobody reads, which
/// is how the insertion-count line came to be ignored in the first place.
fn split_risk(paths: &[(String, String)], inp: &GuardInputs, v: &mut Verdict) {
    let staged: HashSet<&String> = paths.iter().map(|(_, ap)| ap).collect();
    // Which peers co-edited something in THIS commit, and which staged file
    // implicated each. Ordered so the output is stable for a test and for a
    // human diffing two runs.
    let mut by_owner: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
    for (rel, ap) in paths {
        if let Some((owner, _)) = inp.theirs.get(ap) {
            by_owner.entry(owner.as_str()).or_default().push(rel);
        }
    }
    for (owner, staged_rels) in by_owner {
        // The same peer's other work, still dirty, and NOT in this commit.
        let mut left: Vec<&String> = inp
            .theirs
            .iter()
            .filter(|(p, (o, _))| o == owner && inp.dirty.contains(*p) && !staged.contains(p))
            .map(|(p, _)| p)
            .collect();
        if left.is_empty() {
            continue;
        }
        left.sort();
        v.split_risk.push(json!({
            "owner": owner,
            "staged": staged_rels,
            "left_dirty": left,
            "why": format!(
                "you are committing {} — which '{owner}' co-edited — while {} of THEIR files \
                 are dirty and NOT in this commit. A symbol added on one side may be missing \
                 from the other, so this commit can fail to build even though your tree \
                 compiles: your tree has their uncommitted half and the commit does not. \
                 Nothing else here checks that, because `cargo check` and the pre-commit gate \
                 both build the TREE. Either include their files, or leave their lines out of \
                 yours (`git add -p`), or confirm with them.",
                staged_rels.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
                left.len()
            ),
        }));
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Every response carries EVERY key python's did, on every path — including
/// the short circuits. A caller reading `d["shared"]` must get `[]`, never
/// `None`; shape-inconsistent returns are how a consumer ends up special-casing
/// one branch and silently mishandling the other.
#[derive(Default)]
struct Envelope {
    verdict: Verdict,
    cotenants: Vec<String>,
    window: f64,
    /// AMUX-3446: staged ADDED lines the committer's own firsthand edit
    /// content cannot account for. The one ownership signal that SURVIVES
    /// record expiry — peer records age out (that is how 7797e45 swept a
    /// peer's table row silently), but the committer's own edit content is
    /// fresh at commit time by construction. Advisory, never blocking: a
    /// shell-edited file has no firsthand content and is skipped entirely,
    /// and a partial Edit+sed workflow can false-positive, so the hook WARNs.
    unaccounted: Vec<Value>,
    /// A verdict WAS computed but may UNDER-report: a peer we could not see, a
    /// git call that failed, paths truncated.
    degraded: Vec<String>,
    /// Set when NO verdict was computable. The empty lists are then not "all
    /// clear", and the v2 hook says so out loud rather than exiting 0 silently.
    undecided: Option<String>,
    hook_outdated: bool,
    /// AF-127: the row id of this verdict in guard_verdicts. The v6 hook
    /// carries it in its block marker so the eventual outcome report attaches
    /// to THIS decision by id rather than by a nearest-match guess.
    verdict_id: Option<i64>,
}

impl Envelope {
    fn json(self) -> Value {
        json!({
            "ok": true,
            "foreign": self.verdict.foreign,
            "shared": self.verdict.shared,
            "unclaimed": self.verdict.unclaimed,
            // AF-190. A NEW KEY, not folded into `foreign`, and the reason is
            // the one stated a few lines up in `classify`: installed hooks block
            // on `foreign` being non-empty, and this is a WARNING about a build,
            // not a claim that the staged bytes are somebody else's. Putting it
            // in `foreign` would start blocking commits that are perfectly
            // legitimate. Older hooks ignore the key, which is the correct
            // degradation for an advisory.
            "split_risk": self.verdict.split_risk,
            "unaccounted": self.unaccounted,
            "cotenants": self.cotenants,
            "window_secs": self.window as i64,
            "undecided": self.undecided.is_some(),
            "reason": self.undecided.unwrap_or_default(),
            "degraded": self.degraded,
            "hook_outdated": self.hook_outdated,
            "verdict_id": self.verdict_id,
        })
    }
}

/// Sessions still running a pre-rust hook — the one whose `except: return 0`
/// hid the 405 for the entire cutover. Warned once per session per hour: the
/// hook fires ~1,147 times an hour fleet-wide, and a detector that logs on
/// every call spends the same resource (log volume) as the faults it is
/// hunting (ethos rule 7).
static OUTDATED_WARNED: Mutex<Option<HashMap<String, f64>>> = Mutex::new(None);

/// Is the caller a PRE-RUST git hook, or merely a different modern client?
///
/// `guard_version` alone cannot answer this, and treating it as if it could
/// produced a fleet-wide false warning. `scripts/git-hooks/git-shared-guard.py`
/// is a Claude Code PreToolUse hook — a different component with its own
/// lifecycle — and it POSTs `{session, dir, paths, op: "discard"}` to this
/// endpoint without a `guard_version`. So every lane running a CURRENT git hook
/// (GUARD_VERSION 6) was told hourly that its hook was outdated.
///
/// The remedy the warning prints made it worse rather than merely noisy:
/// "Reinstall: scripts/install-hooks.sh" reinstalls the GIT hooks, which were
/// already current, so a lane following the instruction exactly saw no change
/// and the warning returned within the hour. A warning whose sanctioned remedy
/// cannot satisfy it is the AMUX-2140 shape, and it accused the wrong component
/// besides.
///
/// `op` is the discriminator the payload already carries. A pre-rust hook sends
/// NEITHER field; every modern client sends at least `op`. Keying on that fixes
/// the whole class rather than one client, which matters because the next
/// non-git-hook caller of this endpoint would otherwise join the warning too.
///
/// Measured 2026-08-24 before the fix: 9 distinct (lane, checkout) pairs warned
/// per hour, indefinitely, including this checkout whose hook was byte-identical
/// to the tracked source.
fn hook_is_outdated(guard_version: i64, has_explicit_op: bool) -> bool {
    guard_version < 2 && !has_explicit_op
}

fn warn_outdated_hook(session: &str, dir: &str) {
    let now = now_epoch();
    let key = format!("{session}\u{1}{dir}");
    if let Ok(mut g) = OUTDATED_WARNED.lock() {
        let m = g.get_or_insert_with(HashMap::new);
        if m.get(&key).is_some_and(|at| now - at < 3600.0) {
            return;
        }
        m.insert(key, now);
    }
    tracing::warn!(
        target: "staged_guard",
        "[staged-guard] OUTDATED HOOK: {} in {} sent no guard_version — that hook swallows \
         server errors (`except Exception: return 0`) and printed nothing for the whole \
         405 window. Reinstall: {}",
        if session.is_empty() { "(no session)" } else { session },
        dir,
        outdated_hook_remedy(dir)
    );
}

/// The command that actually reinstalls the guard IN `dir` (AF-156).
///
/// This used to be the constant `scripts/install-hooks.sh`, which is only
/// correct for a caller already inside the amux checkout. Measured 2026-08-27 by
/// amux-frustrations: this checkout emits ZERO of these warnings. All 272 in one
/// day came from lanes under /Users/ethan/Dev/mixpeek, where
/// `scripts/install-hooks.sh` does not exist at any path — so 100% of recipients
/// were handed an instruction that cannot resolve. A remedy that cannot be run
/// is worse than none: it reads as actionable and costs the reader the attempt.
/// Ethos rule 3 wants a truthful path forward in every legitimate state.
///
/// `install-hooks.sh <dir>` is the foreign-checkout mode, which by its own header
/// installs the guard and NEVER writes the other repo's `pre-commit`. `dir` is
/// already in the warn line, so the server had the data and was printing a
/// constant beside it.
///
/// Honest in both states: a runnable absolute command when `AMUX_REPO` names the
/// checkout, and a visibly-unfilled placeholder when it does not — never a
/// plausible path that silently resolves to nothing.
pub(crate) fn outdated_hook_remedy(dir: &str) -> String {
    let base = std::env::var("AMUX_REPO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "<your amux checkout>".to_string());
    format!("{base}/scripts/install-hooks.sh {dir}")
}

/// The HTTP entry point — a REAL commit attempt, so owners get notified.
pub async fn staged_guard(
    axum::extract::State(state): axum::extract::State<super::AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    staged_guard_inner(Some(state), headers, body).await
}

/// The verdict itself. `state` is `None` for INTERNAL callers, and that is the
/// whole reason it is optional rather than threaded: `commit_nudge` calls this
/// to ask "who owns these paths", which is a background probe and not a commit.
/// Notifying an owner from it would tell them their file was being swept every
/// time a nudge tick ran — a notice that is false, repeated, and precisely the
/// noise that gets a channel muted (ethos rule 5).
pub async fn staged_guard_inner(
    state: Option<super::AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    // Parsed by hand rather than via `Json<T>`: axum's rejection is a
    // PLAIN-TEXT 400, which the hook's `json.loads` throws on — straight into
    // the `except` that started this incident. Every answer from here is JSON.
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let obj = parsed.as_object();
    let get_str = |k: &str| -> String {
        obj.and_then(|o| o.get(k)).and_then(Value::as_str).unwrap_or("").trim().to_string()
    };

    // Server-verified origin wins over the claimed body session (AMUX-1768).
    let origin = super::alerts::hdr_worker(&headers);
    let session: String =
        if origin.is_empty() { get_str("session") } else { origin }.chars().take(64).collect();
    let wd_raw = get_str("dir");
    let op_raw = get_str("op");
    let op: String = {
        if op_raw.is_empty() { "commit".into() } else { op_raw.chars().take(24).collect() }
    };
    let guard_version =
        obj.and_then(|o| o.get("guard_version")).and_then(Value::as_i64).unwrap_or(0);
    let hook_outdated = hook_is_outdated(guard_version, !op_raw.is_empty());
    if hook_outdated {
        warn_outdated_hook(&session, &wd_raw);
    }

    if wd_raw.is_empty() {
        // py:71821 — the one non-200, kept byte-compatible.
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "dir required"})));
    }
    let window = window_secs();
    if !guard_enabled() {
        let mut v = Envelope { window, hook_outdated, ..Default::default() }.json();
        v["enabled"] = json!(false);
        return (StatusCode::OK, Json(v));
    }

    let raw_paths: Vec<String> = obj
        .and_then(|o| o.get("paths"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();

    let wd = std::fs::canonicalize(&wd_raw)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| wd_raw.clone());

    let mut degraded: Vec<String> = Vec::new();

    // ---- cotenants, paired by REPO ROOT (AMUX-2337) -----------------------
    let all_dirs: BTreeMap<String, String> = all_session_workdirs();
    if all_dirs.is_empty() {
        // The one genuinely undecidable state: with no session inventory there
        // are no cotenants to compare against, so the verdict would be an
        // empty `foreign` — indistinguishable from "your commit is clean". Say
        // so instead of answering it.
        return (
            StatusCode::OK,
            Json(
                Envelope {
                    window,
                    degraded,
                    hook_outdated,
                    undecided: Some(format!(
                        "no session inventory readable ({}/sessions) — cotenants unknown, so an \
                         empty verdict here means NOTHING was compared",
                        amux_home().display()
                    )),
                    ..Default::default()
                }
                .json(),
            ),
        );
    }
    let wd_root = match repo_root(&wd).await {
        Some(r) => r,
        None => {
            degraded.push(format!(
                "`git -C {wd} rev-parse --show-toplevel` failed — falling back to the raw \
                 directory for cotenant pairing, which misses lanes whose CC_DIR is a \
                 subdirectory of the repo (AMUX-2337)"
            ));
            wd.clone()
        }
    };
    let mut cotenants: Vec<String> = Vec::new();
    for (other, od) in &all_dirs {
        if other == &session {
            continue;
        }
        let root = repo_root(od).await.unwrap_or_else(|| od.clone());
        if root == wd_root {
            cotenants.push(other.clone());
        }
    }

    if cotenants.is_empty() || raw_paths.is_empty() {
        return (
            StatusCode::OK,
            Json(Envelope { cotenants, window, degraded, hook_outdated, ..Default::default() }.json()),
        );
    }

    let truncated = raw_paths.len() > MAX_PATHS;
    if truncated {
        degraded.push(format!(
            "{} staged paths, only the first {MAX_PATHS} were classified — the remainder are \
             UNGUARDED in this verdict",
            raw_paths.len()
        ));
    }
    let rel_paths: Vec<String> = raw_paths.into_iter().take(MAX_PATHS).collect();

    // ---- unstaged-right-now (AC-297): the working tree is ground truth ----
    let mut dirty: HashSet<String> = HashSet::new();
    match git_out(&wd, &["diff", "--name-only", "-z"]).await {
        Some(out) => {
            for p in out.split('\0').filter(|p| !p.trim().is_empty()) {
                dirty.insert(realpath(&Path::new(&wd).join(p)));
            }
        }
        None => degraded.push(format!(
            "`git -C {wd} diff --name-only` failed — `has_unstaged_changes` is unknown, so \
             co-edit notices may render as NOTE where they should be WARNING"
        )),
    }

    // ---- transcripts (blocking IO, off the async executor) ----------------
    let scan_session = session.clone();
    let scan_cotenants = cotenants.clone();
    let scan = tokio::task::spawn_blocking(move || {
        let mine = if scan_session.is_empty() {
            EditScan::default()
        } else {
            recent_edit_paths(&scan_session, window, false)
        };
        let mine_fh = if scan_session.is_empty() {
            EditScan::default()
        } else {
            recent_edit_paths(&scan_session, window, true)
        };
        let mut theirs: HashMap<String, (String, f64)> = HashMap::new();
        let mut theirs_fh: HashSet<String> = HashSet::new();
        let mut theirs_restore: HashSet<String> = HashSet::new();
        let mut blind: Vec<String> = Vec::new();
        for other in &scan_cotenants {
            let fh = recent_edit_paths(other, window, true);
            if !fh.transcript_found {
                blind.push(other.clone());
            }
            theirs_fh.extend(fh.paths.keys().cloned());
            let full = recent_edit_paths(other, window, false);
            for (p, ts) in &full.paths {
                match theirs.get(p) {
                    Some((_, cur)) if *cur >= *ts => {}
                    _ => {
                        // Provenance follows the WINNING owner (MG-1484).
                        if full.restores.contains_key(p) {
                            theirs_restore.insert(p.clone());
                        } else {
                            theirs_restore.remove(p);
                        }
                        theirs.insert(p.clone(), (other.clone(), *ts));
                    }
                }
            }
        }
        (mine, mine_fh, theirs, theirs_fh, theirs_restore, blind)
    })
    .await;

    let (mine, mine_fh, theirs, theirs_fh, theirs_restore, blind) = match scan {
        Ok(t) => t,
        Err(e) => {
            // NEVER a 500. A 5xx reaches the hook as an exception and the whole
            // point of this file is that an exception there is invisible.
            return (
                StatusCode::OK,
                Json(
                    Envelope {
                        cotenants,
                        window,
                        degraded,
                        hook_outdated,
                        undecided: Some(format!(
                            "transcript scan did not complete ({e}) — nothing was compared"
                        )),
                        ..Default::default()
                    }
                    .json(),
                ),
            );
        }
    };

    if !mine.transcript_found && !session.is_empty() {
        // Not undecided — a missing self-transcript makes the guard STRICTER
        // (nothing is claimed as yours), so the verdict is still safe. But it
        // is the reason a commit can be blocked on your own work, which is
        // exactly the AF-26 false positive, so name it where the operator is.
        degraded.push(format!(
            "no transcript found for '{session}' — none of the staged paths can be claimed as \
             yours, so this verdict is stricter than it should be"
        ));
    }
    // PARTITION THE BLIND BY LIVENESS (AMUX-2936's measurement, acted on).
    // The signal below ran for ~5h and answered the card's question with a
    // number nobody guessed: 881 warnings, dominated by the mixpeek checkout
    // where THIRTY-TWO cotenants are blind on every commit — nearly all of
    // them long-retired project lanes. A warning that fires 32 names at every
    // committer on the busiest repo is alarm fatigue by construction; the one
    // real signal (a LIVE lane whose transcript cannot be read) drowns in it.
    //
    // Liveness is the discriminator because it is checkable without file
    // mtimes (proven unreliable for this exact question earlier tonight:
    // amux-helper's env mtime said 98 days idle while it committed the same
    // day). A RUNNING blind cotenant can be mid-edit right now — that stays
    // the loud case. A STOPPED one can only contribute pre-stop edits inside
    // the window; possible, so it is still disclosed, but compactly and
    // without the per-name klaxon.
    // ABSENT IS NOT BLIND (AMUX-2936, decided 2026-08-15 with amux).
    //
    // `blind` already means exactly one thing: no resolvable transcript. Split
    // on liveness and the two halves are different FACTS, not degrees:
    //
    //   no transcript + RUNNING  -> BLIND. A live lane we cannot see. It may be
    //                               staging work right now, so it gates the
    //                               AC-355 block and stays the loud case.
    //   no transcript + stopped  -> ABSENT. Not a lane the guard cannot see —
    //                               a lane that is not there. No recorded
    //                               activity at all, so there is no invisible
    //                               work for it to hide.
    //
    // THE EXCLUSION KEYS ON "NO RESOLVABLE TRANSCRIPT", never on liveness alone
    // and never on env mtime. A STOPPED lane WITH a transcript is a real
    // cotenant and keeps blocking — its pre-stop staged work is precisely the
    // sweep target AC-355 exists for. Folding this into a "stopped" or
    // "stale-env" flag re-breaks that case, which is why the two are separate
    // verdicts and separate messages.
    //
    // The fact that settled it: amux-helper made every commit on this checkout
    // report PARTIAL forever. Measured — newest commit 2026-07-28 (18 days, by
    // author AND committer date, so no rebase is hiding it), not running, no
    // pane, env file 85 bytes from May 5, no resolvable transcript. AMUX-2936
    // had rejected the env-mtime proxy on the grounds that it "committed to this
    // repo TODAY"; that turned out to be false, so the card was stuck on a fact
    // rather than a judgement.
    let mut blind_live: Vec<String> = Vec::new();
    let mut absent: Vec<String> = Vec::new();
    for b in &blind {
        if crate::api::session_verbs::is_running(b).await {
            blind_live.push(b.clone());
        } else {
            absent.push(b.clone());
        }
    }
    if !absent.is_empty() {
        // Reported, not silent — but as what it is. "Unreadable transcript"
        // invited the reader to imagine hidden work; "no such lane" does not.
        degraded.push(format!(
            "{} cotenant(s) with no recorded activity at all — {} — treated as ABSENT, not \
             blind: not running, no resolvable transcript, so there is no invisible work to \
             hide. These do NOT gate the unattributable-path block.",
            absent.len(),
            absent.join(", ")
        ));
    }
    if !blind_live.is_empty() {
        let blind = &blind_live;
        // The dangerous direction: their edits are invisible, so the verdict
        // UNDER-reports and a sweep of their work would pass silently.
        degraded.push(format!(
            "no transcript for RUNNING cotenant(s) {} — their edits are INVISIBLE to this \
             verdict; an empty result does not clear their files",
            blind.join(", ")
        ));
        // AND SAY SO WHERE IT CAN BE COUNTED (AMUX-2936). Until now this
        // verdict was returned to the hook and nowhere else: the operator saw
        // "PARTIAL — no transcript for cotenant(s) X" on their own terminal and
        // the server recorded nothing, so the rate was unmeasurable from the
        // logs. That is the reason AMUX-2936 could not be decided — its own
        // instruction was "measure the base rate before choosing a design", and
        // the base rate had no trace to measure.
        //
        // It matters more than a missing counter usually would, because this is
        // the ONLY class through which an absorption passes silently. `foreign`
        // already exits 1 and blocks the commit (740 blocks in 40 hours, so the
        // blocking half demonstrably works). A blind cotenant cannot produce a
        // `foreign` row at all — their edits are invisible — so their files land
        // in `unclaimed`, which is explicitly not blockable, and the sweep goes
        // through with a warning nobody is obliged to read.
        //
        // Coverage also DECAYS: a session that has stopped has no live
        // transcript, so every retired lane permanently widens the blind set
        // while the guard keeps answering 200.
        tracing::warn!(
            target: "staged_guard",
            "[staged-guard/AMUX-2936] blind-cotenant verdict for {} in {}: {} invisible ({}), \
             {} staged path(s) — an absorption of THEIR work would pass silently here",
            if session.is_empty() { "(no session)" } else { &session },
            wd_root,
            blind.len(),
            blind.join(","),
            rel_paths.len()
        );
    }

    let now = now_epoch();
    let pairs: Vec<(String, String)> = rel_paths
        .iter()
        .map(|rel| (rel.clone(), realpath(&Path::new(&wd).join(rel))))
        .collect();
    let inputs = GuardInputs {
        mine: mine.paths,
        mine_firsthand: mine_fh.paths.keys().cloned().collect(),
        // Filled by apply_observed, same as theirs_observed_only below.
        mine_observed_only: HashSet::new(),
        theirs,
        theirs_firsthand: theirs_fh,
        theirs_observed_only: HashSet::new(), // filled by apply_observed
        theirs_restore,
        dirty,
            // Live or stopped alike: a stopped lane's PRE-STOP staged work is exactly
        // what gets swept, and the degraded message already admits those edits are
        // invisible. Liveness answers "can they be editing now", which is a
        // different question from "could this staged file be theirs".
        // BLIND only — absent lanes are excluded (see the partition above).
        blind_cotenant: gates_unclaimed(&blind_live),
};
    // AF-123: merge OBSERVED records (the Bash hook pair's mtime reports) at
    // firsthand rank, for the committer and every cotenant. This is what ends
    // the structural firsthand=0 penalty on Bash-editing lanes: their writes
    // become facts here regardless of how the command spelled the path.
    let mut inputs = inputs;
    if let Some(st) = state.as_ref() {
        if let Ok(conn) = st.store.read() {
            let mine_obs = if session.is_empty() {
                HashMap::new()
            } else {
                load_observed(&conn, &session, window)
            };
            let theirs_obs: Vec<(String, HashMap<String, f64>)> = cotenants
                .iter()
                .map(|c| (c.clone(), load_observed(&conn, c, window)))
                .filter(|(_, m)| !m.is_empty())
                .collect();
            apply_observed(&mut inputs, &mine_obs, &theirs_obs);
        }
    }
    let v = classify(&pairs, now, window, &inputs);

    // AMUX-3446: account each staged path's ADDED lines against the
    // committer's own firsthand edit content. Skipped entirely for a path
    // with no firsthand content (a shell-edited file would false-positive on
    // every line), and never blocking — but a hit is the one signal that
    // survives peer-record expiry, which is the hole 7797e45 shipped through.
    let mut unaccounted_rows: Vec<Value> = Vec::new();
    if !session.is_empty() {
        let own = tokio::task::spawn_blocking({
            let s = session.clone();
            let w = window;
            move || firsthand_edit_content(&s, w, 400_000)
        })
        .await
        .unwrap_or_default();
        for (rel, ap) in &pairs {
            let Some(content) = own.get(ap) else { continue };
            let Some(diff) = git_out(&wd, &["diff", "--cached", "--unified=0", "--", rel]).await
            else {
                continue;
            };
            let added: Vec<String> = diff
                .lines()
                .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                .map(|l| l[1..].to_string())
                .collect();
            let missing = unaccounted_added_lines(&added, content);
            if missing.is_empty() {
                continue;
            }
            unaccounted_rows.push(json!({
                "path": rel,
                "count": missing.len(),
                "lines": missing.iter().take(5).map(|l| l.chars().take(120).collect::<String>()).collect::<Vec<_>>(),
                "note": "staged ADDED lines matching nothing you edited firsthand in the window — on a shared checkout these are likely a peer's in-flight hunks riding your git add (AMUX-3446; a per-file add stages whatever is in the file). If they are yours via shell edits, proceed; otherwise stage per-hunk (git add -p).",
            }));
        }
    }

    if !v.foreign.is_empty() {
        // BLOCK-TIME FORENSICS (AF-27). A block is the expensive verdict and
        // the only one that was undiagnosable after the fact — by the time
        // anyone looked, the claim set had been recomputed and the trail was
        // gone. Logged AT the decision: what the committer's claim set held,
        // how it was sourced, how old its newest entry was.
        let newest = inputs.mine.values().copied().fold(0.0_f64, f64::max);
        tracing::warn!(
            target: "staged_guard",
            // `newest_any`, not `newest` (renamed 2026-08-14). It is the newest entry
            // across the committer's WHOLE claim set, which is NOT what classify()
            // compares — classify uses inputs.mine[path], per-path on both sides. The
            // old name invited exactly that conflation: reading these lines, the margin
            // between a peer's per-path claim and a SET-WIDE newest looked like the
            // decision the code makes, and a reviewer could not tell from the log alone
            // whether an under-block was possible. It was not, but confirming that meant
            // reading the Rust, which is the failure this forensic exists to prevent.
            // mine_age below is the per-path value the comparison actually uses.
            "[staged-guard/AF-27] {} blocked {} in {} — window={}s mine={} (firsthand={}, newest_any={}) \
             cotenants={}; per-path: {}",
            if session.is_empty() { "(no session)" } else { &session },
            op,
            wd_root,
            window as i64,
            inputs.mine.len(),
            inputs.mine_firsthand.len(),
            if newest > 0.0 { format!("{:.1}s ago", now - newest) } else { "none".into() },
            cotenants.len(),
            v.foreign
                .iter()
                .take(8)
                .map(|f| {
                    let rel = f["path"].as_str().unwrap_or("");
                    let ap = realpath(&Path::new(&wd).join(rel));
                    format!(
                        "{rel} in_mine={} in_firsthand={} mine_age={} owner={} age={}s",
                        inputs.mine.contains_key(&ap),
                        inputs.mine_firsthand.contains(&ap),
                        // THE value classify() weighs against the peer's per-path ts.
                        // Without it a reader can only see the set-wide newest_any and
                        // cannot compute the real margin — which is why the 6 defect
                        // verdicts of 2026-08-14 had to be escalated as a question
                        // rather than answered from the log.
                        match inputs.mine.get(&ap) {
                            Some(ts) => format!("{:.1}s", now - ts),
                            None => "absent".into(),
                        },
                        f["owner"].as_str().unwrap_or(""),
                        f["age_secs"].as_i64().unwrap_or(0)
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        );

        // TELL THE OWNER, NOT ONLY THE COMMITTER (AMUX-2923).
        //
        // Everything above is addressed to the session doing the absorbing,
        // and that half works — it warned me by name the day this card was
        // filed. The half that does not exist is the mirror: the session whose
        // staged file is about to be swept learns NOTHING, because the guard
        // runs in the committer's process and the victim is not in the room.
        //
        // On 2026-08-11 an e2e change of mine landed inside a peer's commit
        // titled "fix(status): a codex picker reads waiting, not idle". The
        // code survived; the reasoning did not, and proving it was not LOST
        // cost ~10 minutes of archaeology. CLAUDE.md already records the worse
        // version — a peer's commit reverting another agent's uncommitted
        // rewrite of that very file.
        //
        // The server is the only party that can see both sides, and it sees
        // them right here. `steer_enqueue` is the same durable path board_drive
        // uses, so this is the existing messages primitive rather than a new
        // channel (delivered at the owner's next turn boundary, which is the
        // soonest they could act on it anyway).
        if let Some(st) = state.as_ref() {
            let mut by_owner: BTreeMap<String, Vec<(String, i64, String)>> = BTreeMap::new();
            for f in &v.foreign {
                let owner = f["owner"].as_str().unwrap_or("").to_string();
                let path = f["path"].as_str().unwrap_or("").to_string();
                if owner.is_empty() || owner == session || path.is_empty() {
                    continue;
                }
                let age = f["age_secs"].as_i64().unwrap_or(0);
                let prov = f["provenance"].as_str().unwrap_or("inferred").to_string();
                by_owner.entry(owner).or_default().push((path, age, prov));
            }
            for (owner, paths) in by_owner {
                // DEDUPE, because a pre-commit hook fires on every attempt and
                // a session that keeps retrying a blocked commit would
                // otherwise message the owner once per keystroke-adjacent
                // retry. Keyed on (owner, committer, path-set) for an hour.
                let path_names: Vec<String> = paths.iter().map(|(p, _, _)| p.clone()).collect();
                let key = format!("{owner}|{session}|{}", path_names.join(","));
                if !notify_once(&key) {
                    continue;
                }
                // Per path, say whether the owner's work is ALREADY COMMITTED. The
                // notice fires on edit records, which cannot distinguish "edited and
                // committed" from "edited and still staged" — so it used to hand the
                // recipient a `git log` to run every time. Now it runs it for them.
                let mut lines: Vec<String> = Vec::new();
                let mut n_at_risk = 0usize;
                for (pth, age, prov) in paths.iter().take(10) {
                    let fate = path_fate(&wd, pth, &owner, *age, prov).await;
                    let (line, at_risk) = victim_path_line(pth, &fate, prov);
                    if at_risk {
                        n_at_risk += 1;
                    }
                    lines.push(line);
                }
                let all_settled = n_at_risk == 0;
                // AF-130: the victim notice was delivered as a session message
                // and NEVER logged, so `grep -c 'WORK ITSELF is at risk'`
                // returned 0 across the whole retained window — nobody could
                // count how often it fired or how often it was wrong (the
                // reporter said "n=1 because n=1 is what the instrument
                // permits"). WARN when an at-risk line ships (the loud fate
                // must be countable and auditable for false positives), INFO
                // for the all-settled shape.
                if n_at_risk > 0 {
                    tracing::warn!(
                        target: "amux::git_guard", victim = %owner, committer = %session,
                        paths = paths.len(), at_risk = n_at_risk,
                        "victim notice sent: WORK ITSELF is at risk lines included"
                    );
                } else {
                    tracing::info!(
                        target: "amux::git_guard", victim = %owner, committer = %session,
                        paths = paths.len(),
                        "victim notice sent: all paths settled/absorbed/landed"
                    );
                }
                let list = lines.join("\n");
                let more = paths.len().saturating_sub(10);
                let verdict = if all_settled {
                    "\n\nEVERY path above is already committed by you, so this is almost certainly \
                     noise — the notice fires on EDIT RECORDS and cannot tell \"edited and committed\" \
                     from \"edited and staged\" on its own. Kept rather than suppressed because a \
                     false alarm costs you a glance and a missed one costs work."
                } else {
                    "\n\nAt least one path has no commit of yours since the edit — that is the one to \
                     reconcile."
                };
                let text = format!(
                    "[amux staged-guard] Session `{}` is committing in {} and the staged set \
                     includes {} file(s) whose edit records are YOURS:\n{}{}\n\n\
                     This is the mirror of the warning they got. If those are edits you had \
                     staged or in flight, they may land under THEIR commit message — the code \
                     usually survives, the reasoning does not.\n\n\
                     Check with:  git log -2 --stat -- {}\n\
                     If your work was absorbed, do not rewrite shared history — record the \
                     reasoning where it belongs (a follow-up commit, or the card) and say so.{}",
                    session,
                    wd_root,
                    paths.len(),
                    list,
                    if more > 0 { format!("\n  … and {more} more") } else { String::new() },
                    path_names.first().cloned().unwrap_or_default(),
                    verdict,
                );
                let _ = crate::api::session_verbs::steer_enqueue(st, &owner, &text, "staged-guard", "")
                    .await;
                tracing::warn!(
                    target: "staged_guard",
                    "[staged-guard/AMUX-2923] notified {owner}: {session} is staging {} of their path(s) in {}",
                    paths.len(), wd_root
                );
            }
        }
    }

    // AF-127: record the verdict as a ROW, not only a log line. The guard
    // logged BLOCK/ALLOW and never what happened next, so a true positive
    // (audited override) and a false positive (reflexive ack) were
    // byte-identical and no guard change could be measured against a
    // false-positive rate. One row per verdict is the denominator; the
    // outcome half is filled by POST /api/git/guard-outcome.
    let mut verdict_id: Option<i64> = None;
    if let Some(st) = state.as_ref() {
        let vk = if v.foreign.is_empty() { "allow" } else { "block" };
        let paths_json = serde_json::to_string(
            &v.foreign.iter().filter_map(|f| f["path"].as_str()).take(40).collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        let mut prov: HashMap<String, i64> = HashMap::new();
        for f in &v.foreign {
            *prov
                .entry(f["provenance"].as_str().unwrap_or("inferred").to_string())
                .or_insert(0) += 1;
        }
        let prov_json = serde_json::to_string(&prov).unwrap_or_default();
        let (n_f, n_s, n_u) =
            (v.foreign.len() as i64, v.shared.len() as i64, v.unclaimed.len() as i64);
        let (sess_c, dir_c, vk_s) = (session.clone(), wd_root.clone(), vk.to_string());
        let id_slot = std::sync::Arc::new(Mutex::new(None::<i64>));
        let id_w = id_slot.clone();
        let (now_ts, gv) = (now, guard_version);
        // Failure to record must never fail the guard call — the verdict the
        // hook is waiting on is the product; the row is the instrument.
        let _ = st
            .store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO guard_verdicts (ts, session, dir, verdict, n_foreign, n_shared, \
                     n_unclaimed, paths, provenance, guard_version) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![
                        now_ts, sess_c, dir_c, vk_s, n_f, n_s, n_u, paths_json, prov_json, gv
                    ],
                )?;
                *id_w.lock().unwrap() = Some(conn.last_insert_rowid());
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        verdict_id = *id_slot.lock().unwrap();
    }

    (
        StatusCode::OK,
        Json(
            Envelope { verdict: v, cotenants, window, degraded, hook_outdated, unaccounted: unaccounted_rows, verdict_id, ..Default::default() }
                .json(),
        ),
    )
}

/// POST /api/git/guard-outcome — the hook's follow-up report (AF-127): what
/// resolved a block. The one design rule, from amux-frustrations' correction
/// and accepted before a line was written: `proceeded` comes from the
/// DECLARED override (AMUX_VERIFIED_SOLO vs AMUX_ALLOW_FOREIGN — different
/// claims the hook actually saw set), NEVER from correlation. Inferring it
/// from a later commit's shape is the D1 scraper pattern and bundles the two
/// opposite cases this table exists to separate. `trimmed`/`reallowed` are
/// reported by the hook from a direct comparison of the next staged set
/// (basis=observed); aborted is never written at all — it is computed at
/// read time from unresolved-and-old rows (/api/debug/guard-outcomes).
pub async fn guard_outcome(
    axum::extract::State(state): axum::extract::State<super::AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> (StatusCode, axum::Json<Value>) {
    let session = super::alerts::hdr_worker(&headers);
    if session.is_empty() || session == "api-anonymous" {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "X-Amux-Session required — an unattributed outcome audits nothing"})),
        );
    }
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let resolution = s("resolution");
    if !matches!(resolution.as_str(), "proceeded" | "trimmed" | "reallowed") {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": format!("resolution must be proceeded|trimmed|reallowed, got {resolution:?}"),
            })),
        );
    }
    let override_used = s("override");
    let basis = s("basis");
    // The cell everyone's false-positive rate depends on must be the honestly
    // reported one: a `proceeded` without the declared override is exactly the
    // inference this endpoint exists to refuse.
    if resolution == "proceeded"
        && !matches!(override_used.as_str(), "allow_foreign" | "verified_solo")
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "proceeded requires override = allow_foreign|verified_solo — \
                          it is the declared claim, not a correlation guess",
            })),
        );
    }
    if !matches!(basis.as_str(), "declared" | "observed") {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "basis must be declared|observed (inferred is server-assigned)"})),
        );
    }
    let verdict_id = body.get("verdict_id").and_then(Value::as_i64);
    let dir = s("dir");
    if verdict_id.is_none() && dir.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "need verdict_id or dir (for the nearest unresolved block)"})),
        );
    }
    let elapsed = body.get("elapsed_s").and_then(Value::as_f64);
    let now = now_epoch();
    let (sess_c, res_c, ov_c, basis_c) =
        (session.clone(), resolution.clone(), override_used.clone(), basis.clone());
    let linked = std::sync::Arc::new(Mutex::new((0usize, String::new())));
    let linked_w = linked.clone();
    let write = state.store.write_async(move |conn| {
        use rusqlite::OptionalExtension;
        let ov: Option<&str> = (!ov_c.is_empty()).then_some(ov_c.as_str());
        // Resolve to an id first (both branches), and only the reporter's own
        // unresolved row — a peer must not be able to close someone else's
        // block. Without an id (marker lost, or the block predates the v6
        // hook) the newest unresolved block for session+dir inside the window
        // is taken, labeled 'nearest' — the one server-assigned inference.
        let (target_id, link): (Option<i64>, &str) = if let Some(id) = verdict_id {
            (
                conn.query_row(
                    // verdict='block' also here: only blocks HAVE outcomes,
                    // and an id from a stale marker must not close an allow
                    // row that happens to share it.
                    "SELECT id FROM guard_verdicts WHERE id=?1 AND session=?2 \
                     AND verdict='block' AND resolution IS NULL",
                    rusqlite::params![id, sess_c],
                    |r| r.get(0),
                )
                .optional()?,
                "marker",
            )
        } else {
            (
                conn.query_row(
                    "SELECT id FROM guard_verdicts WHERE session=?1 AND dir=?2 \
                     AND verdict='block' AND resolution IS NULL AND ts > ?3 \
                     ORDER BY ts DESC LIMIT 1",
                    rusqlite::params![sess_c, dir, now - OBSERVED_WINDOW_S],
                    |r| r.get(0),
                )
                .optional()?,
                "nearest",
            )
        };
        let mut n = 0usize;
        if let Some(tid) = target_id {
            n = conn.execute(
                "UPDATE guard_verdicts SET resolution=?1, override_used=?2, outcome_basis=?3, \
                 outcome_ts=?4, outcome_elapsed_s=?5, outcome_link=?6 WHERE id=?7",
                rusqlite::params![res_c, ov, basis_c, now, elapsed, link, tid],
            )?;
            // Close the episode's earlier retry-blocks as 'superseded' —
            // each retry inserts a fresh block row, and leaving them
            // unresolved would inflate the inferred-aborted cell with rows
            // whose episode demonstrably ENDED. Same session, same dir (read
            // off the attached row so the by-id branch needs no dir), older
            // id, inside the window. Labeled inferred: this is bookkeeping
            // derived from the attach, not a report.
            conn.execute(
                "UPDATE guard_verdicts SET resolution='superseded', outcome_basis='inferred', \
                 outcome_ts=?1, outcome_link='episode' \
                 WHERE session=?2 AND dir=(SELECT dir FROM guard_verdicts WHERE id=?3) \
                 AND verdict='block' AND resolution IS NULL AND id < ?3 AND ts > ?4",
                rusqlite::params![now, sess_c, tid, now - OBSERVED_WINDOW_S],
            )?;
        }
        *linked_w.lock().unwrap() = (n, link.to_string());
        Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
    });
    match write.await {
        Ok(_) => {
            let (n, link) = linked.lock().unwrap().clone();
            // Countable at the source (rule 4): every outcome is a log line,
            // so the resolution mix is a grep before it is a query.
            tracing::info!(
                target: "staged_guard",
                "[staged-guard/AF-127] outcome {} (basis={}, override={}) from {} — {} ({} row)",
                resolution, basis,
                if override_used.is_empty() { "-" } else { &override_used },
                session,
                if n > 0 { "attached" } else { "NO OPEN VERDICT MATCHED" },
                link,
            );
            (
                StatusCode::OK,
                axum::Json(json!({
                    "ok": true,
                    "attached": n > 0,
                    "link": link,
                    // n == 0 is a real answer, said plainly: the hook's ledger
                    // records it rather than reading ok:true as attached.
                    "note": if n > 0 { "" } else { "no unresolved block matched — marker stale, or the verdict predates guard_verdicts" },
                })),
            )
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"error": e.to_string()})))
        }
    }
}

/// GET /api/debug/guard-outcomes?since_h=24 — the read the card demands: the
/// verdict/outcome mix, computable in both directions from day one. `aborted`
/// is COMPUTED here (unresolved block older than the guard window), never
/// written into a row — the one cell that would otherwise need a sweep.
pub async fn guard_outcomes_debug(
    axum::extract::State(state): axum::extract::State<super::AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, axum::Json<Value>) {
    let since_h: f64 = q.get("since_h").and_then(|v| v.parse().ok()).unwrap_or(24.0);
    let now = now_epoch();
    let cutoff = now - since_h * 3600.0;
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"error": e.to_string()})),
            )
        }
    };
    let count = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> i64 {
        conn.query_row(sql, p, |r| r.get(0)).unwrap_or(-1)
    };
    let c = &cutoff as &dyn rusqlite::ToSql;
    let abort_edge = now - OBSERVED_WINDOW_S;
    let a = &abort_edge as &dyn rusqlite::ToSql;
    let mut recent = Vec::new();
    if let Ok(mut st) = conn.prepare(
        "SELECT id, ts, session, dir, verdict, n_foreign, resolution, override_used, \
         outcome_basis, outcome_link, outcome_elapsed_s FROM guard_verdicts \
         WHERE ts > ?1 AND verdict='block' ORDER BY ts DESC LIMIT 20",
    ) {
        let rows = st.query_map([cutoff], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "ts": r.get::<_, f64>(1)?,
                "session": r.get::<_, String>(2)?,
                "dir": r.get::<_, String>(3)?,
                "verdict": r.get::<_, String>(4)?,
                "n_foreign": r.get::<_, i64>(5)?,
                "resolution": r.get::<_, Option<String>>(6)?,
                "override": r.get::<_, Option<String>>(7)?,
                "basis": r.get::<_, Option<String>>(8)?,
                "link": r.get::<_, Option<String>>(9)?,
                "elapsed_s": r.get::<_, Option<f64>>(10)?,
            }))
        });
        if let Ok(rows) = rows {
            recent = rows.flatten().collect();
        }
    }
    (
        StatusCode::OK,
        axum::Json(json!({
            "since_h": since_h,
            "verdicts": {
                "allow": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='allow'", &[c]),
                "block": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block'", &[c]),
            },
            "block_outcomes": {
                "proceeded_verified_solo": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block' AND resolution='proceeded' AND override_used='verified_solo'", &[c]),
                "proceeded_allow_foreign": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block' AND resolution='proceeded' AND override_used='allow_foreign'", &[c]),
                "trimmed": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block' AND resolution='trimmed'", &[c]),
                "reallowed": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block' AND resolution='reallowed'", &[c]),
                "superseded": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block' AND resolution='superseded'", &[c]),
                // COMPUTED, labeled with its own uncertainty: no report and
                // past the window. A pending block (recent, unresolved) is a
                // different cell — collapsing them would manufacture aborts
                // out of every block younger than the window.
                "aborted_or_walked_away_inferred": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block' AND resolution IS NULL AND ts < ?2", &[c, a]),
                "pending": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND verdict='block' AND resolution IS NULL AND ts >= ?2", &[c, a]),
            },
            "links": {
                "marker": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND outcome_link='marker'", &[c]),
                "nearest": count("SELECT COUNT(*) FROM guard_verdicts WHERE ts>?1 AND outcome_link='nearest'", &[c]),
            },
            "recent_blocks": recent,
            "note": "proceeded cells are DECLARED (the committer's own override env var); trimmed/reallowed are hook-OBSERVED staged-set comparisons; aborted is inferred at read time and named so. -1 = query failed, never silently 0.",
        })),
    )
}

use crate::config::amux_home;

// ---------------------------------------------------------------------------
// Tests — every one of these fires the BLOCK path or a bypass of it. A guard
// whose refusal has never been observed is theatre, and this one has already
// regressed to silence twice (AC-261, then the rust cutover's 405).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// The victim-notice discriminator (2026-08-15). This decides whether a
    /// recipient is told "nothing at risk", so a WRONG `Some` is the dangerous
    /// direction — it tells someone to relax about work that was actually
    /// absorbed. Both directions are asserted against a real git repo, because
    /// the whole function is `git log` behaviour and a mock would only prove the
    /// mock.
    ///
    /// Authorship is the Amux-Session TRAILER, never %an: every lane on this
    /// machine commits as the same person, so %an cannot discriminate at all.
    /// The three-state refinement (amux, 2026-08-15). The first version keyed
    /// only on the VICTIM'S OWN trailer, so an absorption that had already
    /// happened cleanly — bytes safely in HEAD under the absorber's commit —
    /// reported as "NO commit of yours; CHECK THIS ONE". That is a false alarm
    /// on the most common outcome, and it points the reader at recovering work
    /// that was never lost. Absorbed-but-safe and about-to-be-swept need
    /// OPPOSITE responses: one is "record the reasoning on the card", the other
    /// is "your work is at risk".
    #[tokio::test]
    async fn path_fate_separates_absorbed_but_safe_from_genuinely_at_risk() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_string_lossy().to_string();
        let git = |args: &[&str]| {
            std::process::Command::new("git").args(args).current_dir(&d).output().unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "Shared Name"]);

        // alice's work, committed by BOB (the absorption that already happened).
        std::fs::write(dir.path().join("absorbed.txt"), "alice wrote this\n").unwrap();
        git(&["add", "absorbed.txt"]);
        git(&["commit", "-q", "-m", "bob's message\n\nAmux-Session: bob"]);

        // SELF-absorption collapses to settled (2026-08-21, fired live): bob's
        // edit record postdating his own commit lands here with who == owner,
        // and "absorbed into <sha> under `bob`" ADDRESSED TO BOB reads as
        // someone having taken work that nobody took. edit_age=0 forces
        // owner_committed_since to miss, which is the live mechanism.
        match path_fate(&d, "absorbed.txt", "bob", 0, "firsthand").await {
            PathFate::SettledByOwner(_) => {}
            other => panic!("your own commit is settled, never an absorption: {other:?}"),
        }

        match path_fate(&d, "absorbed.txt", "alice", 3600, "firsthand").await {
            PathFate::AbsorbedBy(_, who) => assert_eq!(who, "bob",
                "must name WHO absorbed it, so the victim knows where the reasoning went"),
            other => panic!("expected AbsorbedBy, got {other:?} — this is the cry-wolf case"),
        }

        // alice's work that is NOT in HEAD: genuinely at risk.
        std::fs::write(dir.path().join("atrisk.txt"), "uncommitted\n").unwrap();
        assert_eq!(path_fate(&d, "atrisk.txt", "alice", 3600, "firsthand").await, PathFate::AtRisk);

        // and a file alice committed herself stays settled.
        std::fs::write(dir.path().join("mine.txt"), "alice\n").unwrap();
        git(&["add", "mine.txt"]);
        git(&["commit", "-q", "-m", "alice\n\nAmux-Session: alice"]);
        assert!(matches!(path_fate(&d, "mine.txt", "alice", 3600, "firsthand").await,
                         PathFate::SettledByOwner(_)));

        // AMUX-3445, the graft-push shape: backend's work landed on ORIGIN via
        // a dangling commit, local HEAD never advanced, worktree carries the
        // origin bytes. Two identical at-risk warnings fired in one hour on
        // exactly this — both resolving to byte-identical no-ops — because
        // worktree-vs-HEAD reads a landed graft as dirty forever.
        // The base commit is a PEER's: a graft lane never commits locally, so
        // owner_committed_since must miss (a backend-authored base here made
        // the case exit early as SettledByOwner — also calm, but not the arm
        // this test exists to pin).
        std::fs::write(dir.path().join("grafted.txt"), "v1\n").unwrap();
        git(&["add", "grafted.txt"]);
        git(&["commit", "-q", "-m", "base\n\nAmux-Session: alice"]);
        let base = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap().trim().to_string();
        std::fs::write(dir.path().join("grafted.txt"), "landed v2\n").unwrap();
        git(&["add", "grafted.txt"]);
        git(&["commit", "-q", "-m", "graft\n\nAmux-Session: backend"]);
        let graft = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap().trim().to_string();
        git(&["update-ref", "refs/remotes/origin/main", &graft]);
        git(&["reset", "-q", "--hard", &base]);
        std::fs::write(dir.path().join("grafted.txt"), "landed v2\n").unwrap();
        // backend's amendment, THE TRAP ARM: worktree == origin bytes but the
        // INDEX still holds the pre-graft blob (reset --hard left it at v1).
        // The commit takes the staged blob, so committing HERE would revert
        // the landed lines — a receipt on this state talks the victim out of
        // checking. Must stay AtRisk. (The first fix's own fixture sat in
        // exactly this state and asserted the receipt, which is the proof the
        // amendment was needed.)
        assert_eq!(
            path_fate(&d, "grafted.txt", "backend", 3600, "firsthand").await,
            PathFate::AtRisk,
            "stale staged blob under origin-identical worktree is the revert-in-waiting"
        );
        // Receipt only when BOTH trees match origin: stage the origin bytes.
        git(&["add", "grafted.txt"]);
        match path_fate(&d, "grafted.txt", "backend", 3600, "firsthand").await {
            PathFate::LandedOnOrigin(sha) => {
                assert!(!sha.is_empty(), "the receipt must carry origin's sha")
            }
            other => panic!("bytes identical to origin in BOTH trees cannot be at risk: {other:?}"),
        }
        // Control: worktree differing from BOTH HEAD and origin is the real
        // at-risk case, and it must stay loud.
        std::fs::write(dir.path().join("grafted.txt"), "novel v3, uncommitted\n").unwrap();
        assert_eq!(path_fate(&d, "grafted.txt", "backend", 0, "firsthand").await, PathFate::AtRisk);
    }

    /// AMUX-3677, rebuilt from the notice's own text and the repo state that
    /// produced it.
    ///
    /// A peer commits a path; their write refreshes the victim's INFERRED
    /// (Bash+mtime) record past the victim's own commit, so
    /// `owner_committed_since` misses. The path is dirty, so `LandedOnOrigin`
    /// is consulted — and on a lane with unpushed commits it can never match.
    /// Result: "the WORK ITSELF is at risk" about work in local HEAD.
    ///
    /// The fixture deliberately has NO `origin/main` at all, which is the
    /// strongest form of "ahead of origin" and makes the old rescue path
    /// provably unavailable rather than merely unlikely.
    #[tokio::test]
    async fn an_inferred_record_over_the_owners_own_newest_commit_is_settled_not_at_risk() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_string_lossy().to_string();
        let git = |args: &[&str]| {
            std::process::Command::new("git").args(args).current_dir(&d).output().unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);

        // alice commits the file — this is the work the notice claims is at risk.
        std::fs::write(dir.path().join("shared.rs"), "alice's work\n").unwrap();
        git(&["add", "shared.rs"]);
        git(&["commit", "-q", "-m", "alice's change\n\nAmux-Session: alice"]);

        // bob is mid-commit: the path is dirty against HEAD right now.
        std::fs::write(dir.path().join("shared.rs"), "alice's work\nbob's line\n").unwrap();

        // PREMISE, asserted so neither arm below is vacuously green: the path
        // IS dirty, and alice's commit IS the newest on it.
        assert!(
            git_out(&d, &["diff", "HEAD", "--name-only", "--", "shared.rs"])
                .await
                .map(|o| !o.trim().is_empty())
                .unwrap_or(false),
            "fixture must be dirty, or AtRisk is never reached"
        );
        assert!(owner_owns_newest_commit(&d, "shared.rs", "alice").await.is_some());

        // TREATMENT: alice's record is INFERRED — an mtime that moved when bob
        // wrote. edit_age 0 makes `owner_committed_since` certainly miss, which
        // is what the peer's refresh does in the real case.
        match path_fate(&d, "shared.rs", "alice", 0, "inferred").await {
            PathFate::SettledByOwner(sha) => assert!(!sha.is_empty()),
            other => panic!("inferred record over alice's own newest commit must be settled: {other:?}"),
        }

        // CONTROL 1, AND THE ONE THAT MATTERS. A FIRSTHAND record is a real
        // recorded edit of alice's that is NOT in her commit — genuinely at
        // risk. Trading a false alarm for a missed one is the only way this
        // change can do harm, so it is asserted rather than reasoned about.
        assert!(
            matches!(path_fate(&d, "shared.rs", "alice", 0, "firsthand").await, PathFate::AtRisk),
            "a recorded edit newer than the owner's commit must STILL be at risk"
        );

        // CONTROL 2: if the newest commit is a PEER's, alice's bytes may have
        // been changed by it and only she can judge — no receipt.
        git(&["add", "shared.rs"]);
        git(&["commit", "-q", "-m", "bob's change\n\nAmux-Session: bob"]);
        std::fs::write(dir.path().join("shared.rs"), "alice's work\nbob's line\nmore\n").unwrap();
        assert!(
            owner_owns_newest_commit(&d, "shared.rs", "alice").await.is_none(),
            "alice no longer owns the newest commit"
        );

        // CONTROL 3: an UNTRAILERED commit is nobody's. Reading a missing
        // trailer as a match would hand out receipts on any commit at all.
        std::fs::write(dir.path().join("other.rs"), "x\n").unwrap();
        git(&["add", "other.rs"]);
        git(&["commit", "-q", "-m", "no trailer here"]);
        assert!(owner_owns_newest_commit(&d, "other.rs", "alice").await.is_none());
        assert!(owner_owns_newest_commit(&d, "other.rs", "").await.is_none());
    }

    #[tokio::test]
    async fn owner_committed_since_distinguishes_committed_from_still_staged() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_string_lossy().to_string();
        let git = |args: &[&str]| {
            std::process::Command::new("git").args(args).current_dir(&d).output().unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "Shared Name"]);

        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "mine\n\nAmux-Session: alice"]);
        std::fs::write(dir.path().join("b.txt"), "one\n").unwrap();
        git(&["add", "b.txt"]);
        git(&["commit", "-q", "-m", "theirs\n\nAmux-Session: bob"]);

        // alice edited a.txt 3600s ago and HAS committed since -> settled.
        assert!(
            owner_committed_since(&d, "a.txt", "alice", 3600).await.is_some(),
            "alice committed a.txt after her edit; the notice must say nothing is at risk"
        );
        // bob has no commit touching a.txt at all -> unsettled, must NOT reassure.
        assert!(
            owner_committed_since(&d, "a.txt", "bob", 3600).await.is_none(),
            "bob never committed a.txt — reassuring him would be the dangerous direction"
        );
        // alice's commit is OLDER than a 0-second-old edit: she edited again
        // just now and has not committed it. Must not reassure.
        assert!(
            owner_committed_since(&d, "a.txt", "alice", 0).await.is_none(),
            "an edit newer than the last commit is exactly the at-risk case"
        );
        // %an is identical for both lanes, so a name-based check would pass all
        // three above; the trailer is what makes this discriminate.
        let log = String::from_utf8(git(&["log", "--format=%an"]).stdout).unwrap();
        assert!(log.lines().all(|l| l == "Shared Name"), "fixture must share %an: {log:?}");
    }

    use super::*;

    fn pair(rel: &str) -> (String, String) {
        (rel.to_string(), format!("/repo/{rel}"))
    }

    fn peer_wrote(path: &str, owner: &str, ts: f64) -> GuardInputs {
        let mut g = GuardInputs::default();
        g.theirs.insert(format!("/repo/{path}"), (owner.into(), ts));
        g.theirs_firsthand.insert(format!("/repo/{path}"));
        g
    }

    /// AF-190, rebuilt from the incident's own artifact rather than a
    /// convenient shape.
    ///
    /// `53ae4b8b` was the tip of origin/main and did not compile. Staging
    /// `api/board.rs` took ~16 lines of `amux`'s in-flight AMUX-3607 wiring from
    /// the same file, including a call to `effective_gate_trail` whose
    /// definition lived in `db/board_store.rs` and was still uncommitted in
    /// their tree. `cargo check` and the pre-commit gate both passed, correctly:
    /// they build the TREE, which had the peer's definition, and nothing builds
    /// the COMMIT.
    ///
    /// THE CONTROL IS HALF THE TEST. A warning that fires on every commit is one
    /// nobody reads, and that is not hypothetical here: the guard already
    /// printed a discriminating insertion count for this specimen and its author
    /// read past it. So `split_risk` must be SILENT when the peer has nothing
    /// dirty outside the commit, and silent when the dirty file is theirs but
    /// already staged (including it is the fix, so warning about it would be
    /// telling someone off for doing the right thing).
    #[test]
    fn a_commit_that_splits_a_peers_work_names_what_it_leaves_behind() {
        let mut g = peer_wrote("crates/amux-server/src/api/board.rs", "amux", 100.0);
        // The peer's other half: same lane, dirty, NOT staged.
        g.theirs.insert(
            "/repo/crates/amux-server/src/db/board_store.rs".into(),
            ("amux".into(), 100.0),
        );
        g.dirty.insert("/repo/crates/amux-server/src/db/board_store.rs".into());
        g.mine_firsthand.insert("/repo/crates/amux-server/src/api/board.rs".into());

        let staged = [pair("crates/amux-server/src/api/board.rs")];
        let v = classify(&staged, 200.0, 3600.0, &g);
        assert_eq!(v.split_risk.len(), 1, "{:?}", v.split_risk);
        let r = &v.split_risk[0];
        assert_eq!(r["owner"], "amux");
        assert!(
            r["left_dirty"].as_array().unwrap()[0]
                .as_str()
                .unwrap()
                .ends_with("db/board_store.rs"),
            "it must NAME the file being left behind, not merely say one exists: {r}"
        );
        let why = r["why"].as_str().unwrap();
        assert!(
            why.contains("NOT in this commit"),
            "the hazard is about the COMMIT, so say so: {why}"
        );
        assert!(
            why.contains("fail to build"),
            "state the consequence — this is the check that catches an unbuildable commit \
             from a compiling tree: {why}"
        );
        assert!(
            !why.contains('?'),
            "STATE A FACT, never ask the committer a question. The guard already asked one \
             for this exact specimen (\"if that is MORE than you wrote...\") and its author \
             read past it, because the number matched what they expected: {why}"
        );

        // CONTROL 1: the peer has nothing dirty outside the commit.
        let mut quiet = peer_wrote("crates/amux-server/src/api/board.rs", "amux", 100.0);
        quiet.mine_firsthand.insert("/repo/crates/amux-server/src/api/board.rs".into());
        assert!(
            classify(&staged, 200.0, 3600.0, &quiet).split_risk.is_empty(),
            "a co-edited staged file alone is not a split — every commit on this checkout \
             would warn, and a warning that always fires is one nobody reads"
        );

        // CONTROL 2: their other file is dirty AND staged. Including it is the
        // remedy, so warning about it would punish the correct behaviour.
        let both = [
            pair("crates/amux-server/src/api/board.rs"),
            pair("crates/amux-server/src/db/board_store.rs"),
        ];
        assert!(
            classify(&both, 200.0, 3600.0, &g).split_risk.is_empty(),
            "their two halves are BOTH in the commit, which is exactly the fix"
        );

        // CONTROL 3: a DIFFERENT peer's dirty file. The hazard is a symbol split
        // across one lane's work; unrelated dirt from a third lane is noise, and
        // attributing it here would make the message wrong as well as loud.
        let mut other = peer_wrote("crates/amux-server/src/api/board.rs", "amux", 100.0);
        other.mine_firsthand.insert("/repo/crates/amux-server/src/api/board.rs".into());
        other.theirs.insert("/repo/unrelated.rs".into(), ("desktop".into(), 100.0));
        other.dirty.insert("/repo/unrelated.rs".into());
        assert!(
            classify(&staged, 200.0, 3600.0, &other).split_risk.is_empty(),
            "only the co-editor's OWN dirty files can split their own symbol"
        );
    }

    /// AMUX-3497, rebuilt from the live specimen: a session whose Bash window
    /// held only HTTP probes was named co-editor of board_store.rs, because a
    /// CONCURRENT session's tool edit moved the mtime inside that window and
    /// the observed row attributed the write to the observer. An observed row
    /// explained by the other side's TRANSCRIPT record at the same instant is
    /// one write seen twice and must attribute nothing; past the skew margin
    /// it is a real second write and must keep protecting.
    #[test]
    fn an_observed_echo_of_a_transcript_edit_attributes_nothing() {
        // (a) THE SPECIMEN: committer transcript-firsthand at t=1000; the
        // peer's observed row carries the same write's mtime (within skew).
        // The echo must not mint a co-editor: no shared row, no foreign row.
        let mut g = GuardInputs::default();
        g.mine.insert("/repo/board_store.rs".into(), 1000.0);
        g.mine_firsthand.insert("/repo/board_store.rs".into());
        let mut probe_lane = HashMap::new();
        probe_lane.insert("/repo/board_store.rs".to_string(), 1002.0);
        apply_observed(&mut g, &HashMap::new(), &[("amux-cloud".to_string(), probe_lane)]);
        let v = classify(&[pair("board_store.rs")], 2000.0, 3600.0, &g);
        assert!(v.foreign.is_empty());
        assert!(
            v.shared.is_empty(),
            "the phantom co-editor NOTE must not fire on an mtime echo: {:?}",
            v.shared
        );

        // (b) CONTROL — the same peer row 900s LATER than the transcript edit
        // is a real second write; dropping it too would unprotect genuinely
        // co-edited files. It stays, and classifies shared.
        let mut g = GuardInputs::default();
        g.mine.insert("/repo/board_store.rs".into(), 1000.0);
        g.mine_firsthand.insert("/repo/board_store.rs".into());
        let mut real_edit = HashMap::new();
        real_edit.insert("/repo/board_store.rs".to_string(), 1900.0);
        apply_observed(&mut g, &HashMap::new(), &[("amux-cloud".to_string(), real_edit)]);
        let v = classify(&[pair("board_store.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1, "a real later write must still warn");

        // (c) MIRROR: MY observed echo of a PEER's transcript edit drops my
        // counterclaim, so their firsthand BLOCKS my commit (the AF-19 tie
        // blocked deliberately; the echo drop must not soften it to shared).
        let mut g = peer_wrote("theirs.rs", "alice", 1000.0);
        let mut my_echo = HashMap::new();
        my_echo.insert("/repo/theirs.rs".to_string(), 1001.0);
        apply_observed(&mut g, &my_echo, &[]);
        let v = classify(&[pair("theirs.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.foreign.len(), 1, "my echo of alice's write is not a claim");
        assert!(v.shared.is_empty());

        // (d) OBSERVED-vs-OBSERVED coincidence cannot be resolved server-side
        // (two Bash windows saw one mtime; either could own it) — both claims
        // stay, but the shared row must SAY how it knows (rule 4): co_signal
        // names the ambiguity instead of asserting a co-editor.
        let mut g = GuardInputs::default();
        let mut mine_obs = HashMap::new();
        mine_obs.insert("/repo/both.rs".to_string(), 1500.0);
        let mut peer_obs = HashMap::new();
        peer_obs.insert("/repo/both.rs".to_string(), 1501.0);
        apply_observed(&mut g, &mine_obs, &[("bob".to_string(), peer_obs)]);
        let v = classify(&[pair("both.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1);
        assert!(
            v.shared[0]["co_signal"].as_str().unwrap_or("").contains("AMUX-3497"),
            "ambiguous mtime co-signal must name itself: {:?}",
            v.shared[0]
        );

        // (e) co_signal control: a TRANSCRIPT peer claim coinciding with mine
        // carries no ambiguity marker — the transcript says who wrote it.
        let mut g = peer_wrote("t.rs", "alice", 1000.0);
        g.mine.insert("/repo/t.rs".into(), 1001.0);
        g.mine_firsthand.insert("/repo/t.rs".into());
        let v = classify(&[pair("t.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1);
        assert!(v.shared[0].get("co_signal").is_none(), "{:?}", v.shared[0]);
    }

    /// AMUX-3662, rebuilt from the live specimen rather than a convenient shape.
    ///
    /// Probing `api/board.rs` returned `age_secs: 455, mine_age_secs: 455,
    /// owner: amux-frustrations` and no signal of any kind. Their claim was
    /// REAL (commit 8575cc6f touched that file at 12:18:08); my only contact
    /// was `sed -n '2270,2300p'`, a read. So the phantom was MINE — and
    /// `co_signal` correctly stayed silent, because it only fires when the
    /// PEER's claim is observed.
    ///
    /// Both claims rendered in the same shape, which is worse than a missing
    /// warning: the day before, the identical equal-age signature was read as
    /// "the peer is the phantom" and cost a wipe-apology sweep to an innocent
    /// peer. A symmetric instrument answering an asymmetric question gets read
    /// whichever way the reader already leans.
    #[test]
    fn a_shared_row_says_whether_each_side_recorded_the_write_or_only_saw_the_mtime() {
        // THE SPECIMEN. Peer wrote it for real (transcript); I only observed
        // the mtime move during a command that merely READ the file. The skew
        // is deliberately wider than RECENCY_SKEW_MARGIN_S so my echo is NOT
        // dropped — that drop is a different rule with its own cells above, and
        // this one is about what the reader is told when a claim survives.
        let mut g = peer_wrote("board.rs", "amux-frustrations", 1000.0);
        let mut my_echo = HashMap::new();
        my_echo.insert("/repo/board.rs".to_string(), 1600.0);
        apply_observed(&mut g, &my_echo, &[]);
        let v = classify(&[pair("board.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1, "{:?}", v.shared);
        assert_eq!(
            v.shared[0]["mine_provenance"], "observed",
            "my claim came from an mtime, and the row must say so: {:?}",
            v.shared[0]
        );
        assert_eq!(
            v.shared[0]["their_provenance"], "transcript",
            "their claim IS a recorded write: {:?}",
            v.shared[0]
        );

        // CONTROL 1: the exact inverse must read the exact opposite way.
        let mut g = GuardInputs::default();
        g.mine.insert("/repo/board.rs".into(), 1000.0);
        g.mine_firsthand.insert("/repo/board.rs".into());
        let mut peer_obs = HashMap::new();
        peer_obs.insert("/repo/board.rs".to_string(), 1600.0);
        apply_observed(&mut g, &HashMap::new(), &[("bob".to_string(), peer_obs)]);
        let v = classify(&[pair("board.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1, "{:?}", v.shared);
        assert_eq!(
            v.shared[0]["mine_provenance"], "transcript",
            "a write I RECORDED must never read as inferred: {:?}",
            v.shared[0]
        );
        assert_eq!(v.shared[0]["their_provenance"], "observed", "{:?}", v.shared[0]);

        // CONTROL 2, AND IT IS THE LOAD-BEARING ONE. I hold a transcript record
        // AND my own Bash window also caught the mtime — the ordinary case when
        // you edit a file and then run anything that touches the tree.
        //
        // Control 1 does not reach it: with no observed row of mine, the
        // marking loop never runs for that path, so a mutation that marks
        // EVERYTHING observed still leaves it empty and Control 1 passes. That
        // mutation survived the first draft of this test, which is the whole
        // reason this case exists — a field hardcoded to "observed" would tell
        // every author their own recorded work is an inference, and nothing
        // above would have gone red.
        let mut g = peer_wrote("board.rs", "bob", 1500.0);
        g.mine.insert("/repo/board.rs".into(), 1000.0);
        g.mine_firsthand.insert("/repo/board.rs".into());
        let mut my_own_echo = HashMap::new();
        my_own_echo.insert("/repo/board.rs".to_string(), 1010.0);
        apply_observed(&mut g, &my_own_echo, &[]);
        let v = classify(&[pair("board.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1, "{:?}", v.shared);
        assert_eq!(
            v.shared[0]["mine_provenance"], "transcript",
            "a transcript record OUTRANKS my own mtime echo of the same write — \
             seeing your own edit land is not a second, weaker claim: {:?}",
            v.shared[0]
        );
    }

    /// AF-179, rebuilt from the reported specimen's own numbers.
    ///
    /// amux committed `scripts/token-baseline.py`, a file they wrote from
    /// scratch, and the guard told them it "was also edited by session
    /// 'amux-frustrations' 6m ago". That session never opened it: a two-minute
    /// `cargo test` had walked the repo and filed an OBSERVED record for every
    /// mtime that moved inside its window, including the real author's.
    ///
    /// The hedge that exists for this (AMUX-3497) stayed silent, because it was
    /// gated on the two timestamps agreeing within RECENCY_SKEW_MARGIN_S. The
    /// author kept writing until ~20:29 and the peer's walk had sampled at
    /// 20:10, so the gap was ~1000s against a 5s margin. The longer the real
    /// author works, the further apart the clocks, so the hedge was least able
    /// to fire exactly where the wrong name is hardest to dismiss.
    ///
    /// THE CONTROL IS THE SECOND HALF AND IT IS A LOGIC CONTROL: a peer claim
    /// from a TRANSCRIPT still carries no marker, because a transcript records
    /// who wrote it and there is nothing to hedge. Widening the gate to "any
    /// observed-only peer claim" must not widen it to firsthand ones.
    #[test]
    fn a_long_authorship_sampled_by_a_peers_bash_window_is_marked_as_observed() {
        // The specimen: I authored at t=2029, their walk sampled me at t=2010.
        let mut g = GuardInputs::default();
        let mut mine = HashMap::new();
        mine.insert("/repo/token-baseline.py".to_string(), 2029.0);
        let mut peer = HashMap::new();
        peer.insert("/repo/token-baseline.py".to_string(), 2010.0);
        apply_observed(&mut g, &mine, &[("amux-frustrations".to_string(), peer)]);
        let v = classify(&[pair("token-baseline.py")], 2100.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1, "{:?}", v.shared);
        let sig = v.shared[0]["co_signal"].as_str().unwrap_or("");
        assert!(
            sig.contains("AF-179"),
            "a 19s gap is 4x the 5s skew margin and the old gate said nothing here: {:?}",
            v.shared[0]
        );
        assert!(
            sig.contains("OBSERVED claim, not a recorded write"),
            "state the PROVENANCE — that is the fact, where 'is this a real co-editor' is a \
             guess the server cannot make: {sig:?}"
        );
        assert!(
            sig.contains("NEWER"),
            "my record is newer than their sample, which is the direction that says they \
             sampled MY authorship: {sig:?}"
        );

        // The same shape at the real specimen's distance, ~1000s, must also
        // speak. A window-based gate gets quieter as the gap grows; provenance
        // does not depend on the gap at all.
        let mut g = GuardInputs::default();
        let mut mine = HashMap::new();
        mine.insert("/repo/token-baseline.py".to_string(), 3010.0);
        let mut peer = HashMap::new();
        peer.insert("/repo/token-baseline.py".to_string(), 2010.0);
        apply_observed(&mut g, &mine, &[("amux-frustrations".to_string(), peer)]);
        let v = classify(&[pair("token-baseline.py")], 3100.0, 3600.0, &g);
        assert!(
            v.shared[0]["co_signal"].as_str().unwrap_or("").contains("1000s NEWER"),
            "quote the real gap so the reader can weigh it: {:?}",
            v.shared[0]
        );

        // CONTROL — a peer's TRANSCRIPT claim carries no marker, at any gap.
        // The transcript says who wrote it; hedging it would teach readers to
        // ignore the hedge on the claims that are genuinely ambiguous.
        let mut g = peer_wrote("t.rs", "alice", 1000.0);
        g.mine.insert("/repo/t.rs".into(), 3000.0);
        g.mine_firsthand.insert("/repo/t.rs".into());
        let v = classify(&[pair("t.rs")], 4000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1, "{:?}", v.shared);
        assert!(
            v.shared[0].get("co_signal").is_none(),
            "a firsthand peer claim is not ambiguous and must not be hedged: {:?}",
            v.shared[0]
        );
    }

    /// A CURRENT hook was told hourly that it was outdated, and the remedy
    /// could not fix it (log sweep, 2026-08-24).
    ///
    /// `git-shared-guard.py` is a Claude Code PreToolUse hook, not a git hook,
    /// and it POSTs `{session, dir, paths, op: "discard"}` here with no
    /// `guard_version`. Keying "outdated" on that field alone made every lane
    /// running GUARD_VERSION 6 look pre-rust: 9 distinct (lane, checkout) pairs
    /// warned per hour, including this checkout, whose hook was byte-identical
    /// to the tracked source.
    ///
    /// THE SPECIMEN IS THE SECOND CELL. The first is the control: a genuinely
    /// pre-rust hook sends neither field and must STILL warn, or the fix has not
    /// narrowed the warning, it has deleted it.
    #[test]
    fn a_modern_client_without_guard_version_is_not_an_outdated_git_hook() {
        // CONTROL — neither field: a real pre-rust hook. Must still warn.
        assert!(
            hook_is_outdated(0, false),
            "a hook sending neither op nor guard_version is the case this warning exists for"
        );
        // THE SPECIMEN — git-shared-guard.py: op present, no guard_version.
        assert!(
            !hook_is_outdated(0, true),
            "a client that sends `op` is modern; accusing it names the wrong component and \
             prescribes a reinstall that cannot change anything"
        );
        // A current git hook: both fields.
        assert!(!hook_is_outdated(6, true));
        // And version alone is still sufficient, so a future client that sends a
        // version but no op is not swept back in.
        assert!(!hook_is_outdated(2, false));
    }

    /// AMUX-3128 FIRING PATH: a read-only command must not mint an inferred edit,
    /// so a lane inspecting a peer's file is never flagged as its co-author. The
    /// reported specimen (`ls -t digests/`, `head -40 digests/<f>`) leads this list.
    #[test]
    fn read_only_commands_attribute_nothing() {
        for cmd in [
            "ls -t digests/",
            "head -40 digests/2026-08-15.md",
            "cat crates/foo.rs",
            "grep -n needle src/lib.rs",
            "tail -20 logs/server.log",
            "wc -l a.py",
            "stat file.json",
            "diff a.txt b.txt",
            "find . -name x.rs",
            "head foo.md | grep x",
            "cat a.md | head -5 | wc -l",
        ] {
            assert!(is_pure_read_command(cmd), "should be pure read: {cmd}");
        }
    }

    /// AF-123: observed records rank WITH firsthand. The 75%-of-blocks lane
    /// bias exists because Bash-editing lanes can never mint a firsthand
    /// record; an observed mtime report is a fact about the disk, so a lane
    /// whose only signal is observed must classify exactly like one that used
    /// the Edit tool — here, the committer's fresher observed edit turns a
    /// would-be AF-27 block into a shared warning.
    #[test]
    fn observed_records_rank_with_firsthand_and_lift_the_bash_lane_penalty() {
        // Without observed: peer firsthand vs committer NOTHING -> foreign block.
        let g = peer_wrote("f.rs", "alice", 1000.0);
        let v = classify(&[pair("f.rs")], 2000.0, 3600.0, &g);
        assert_eq!(v.foreign.len(), 1, "control: the bash lane is blocked today");

        // With an observed record 900s fresher than alice's claim: firsthand
        // rank + the AF-27 recency rule -> shared (warned), never blocked.
        let mut g = peer_wrote("f.rs", "alice", 1000.0);
        let mut mine_obs = HashMap::new();
        mine_obs.insert("/repo/f.rs".to_string(), 1900.0);
        apply_observed(&mut g, &mine_obs, &[]);
        let v = classify(&[pair("f.rs")], 2000.0, 3600.0, &g);
        assert!(v.foreign.is_empty(), "{:?}", v.foreign);
        assert_eq!(v.shared.len(), 1, "both claims real -> shared, warned not blocked");

        // AF-125, the cell where ONLY RANK decides — written across the
        // reverse-recency case amux-frustrations' mutation exposed. The
        // committer's observed record is 900s STALER than the peer's
        // firsthand claim, so the AF-27 recency rule alone would block
        // (committer_fresher=false); firsthand rank skips that branch
        // entirely and both real claims read shared. Deleting the
        // mine_firsthand insert in apply_observed fails THIS assert on
        // behavior, not on the insert's own inverse.
        let mut g = peer_wrote("stale-mine.rs", "alice", 1900.0);
        let mut mine_obs = HashMap::new();
        mine_obs.insert("/repo/stale-mine.rs".to_string(), 1000.0);
        apply_observed(&mut g, &mine_obs, &[]);
        let v = classify(&[pair("stale-mine.rs")], 2000.0, 3600.0, &g);
        assert!(
            v.foreign.is_empty(),
            "rank must carry the staler-self cell — recency alone blocks it: {:?}",
            v.foreign
        );
        assert_eq!(v.shared.len(), 1, "two real claims are a contest, not a sweep");
        // Mechanism check, deliberately BELOW the cell (amux-frustrations'
        // AF-125 correction, 2026-08-21): asserting the insert directly is the
        // mutation's own inverse, and placed above the behavioral cell it
        // panicked first and masked it in a plain mutation run. It stays
        // because a right verdict via the wrong channel (recency) lapses; it
        // just must never be the first thing the mutation hits.
        assert!(g.mine_firsthand.contains("/repo/stale-mine.rs"));

        // A PEER's observed record beats their stale entry and clears
        // restore-kind (kind follows the latest record).
        let mut g = GuardInputs::default();
        g.theirs.insert("/repo/g.rs".into(), ("bob".into(), 100.0));
        g.theirs_restore.insert("/repo/g.rs".into());
        let mut bob_obs = HashMap::new();
        bob_obs.insert("/repo/g.rs".to_string(), 500.0);
        apply_observed(&mut g, &HashMap::new(), &[("bob".to_string(), bob_obs)]);
        assert_eq!(g.theirs.get("/repo/g.rs").map(|(_, t)| *t), Some(500.0));
        assert!(!g.theirs_restore.contains("/repo/g.rs"));
        assert!(g.theirs_firsthand.contains("/repo/g.rs"));
    }

    /// AF-130, rebuilt from the incident's own timeline: f84a485 committed at
    /// 12:26:04, the observed record for the SAME `cat >>` minted at 12:26:38
    /// because the hook fires after the whole compound command — so the
    /// record postdated the commit and `owner_committed_since` (strictly
    /// newer wins) could never settle it. The false AtRisk notice fired on
    /// correctly-committed work, on every edit-then-commit-in-one-Bash-call,
    /// which is the dominant bypass-permissions shape. The fix is stamping
    /// the REPORTED mtime; these cells pin the parse.
    #[test]
    fn observed_report_stamps_the_file_mtime_not_the_hook_time() {
        let now = 2000.0;
        // The AF-130 cell: a reported mtime in the past is KEPT. Reverting the
        // fix (stamping `now`) fails here on behavior.
        let body = serde_json::json!({"paths": [{"path": "/repo/x.rs", "mtime": 1500.0}]});
        let rows = parse_observed_reports(&body, now);
        assert_eq!(
            rows,
            vec![("/repo/x.rs".to_string(), 1500.0)],
            "a reported mtime must survive as the record's timestamp — hook-run \
             time postdates the commit in the one-Bash-call shape and manufactures \
             false AtRisk notices"
        );
        // Bare string (older installed hook copy): hook-time stamp, i.e. the
        // pre-AF-130 behavior — degraded toward over-warning, never silence.
        let body = serde_json::json!({"paths": ["/repo/y.rs"]});
        assert_eq!(parse_observed_reports(&body, now), vec![("/repo/y.rs".to_string(), now)]);
        // A future mtime clamps to now: a skewed clock must not mint a record
        // that outlives the pruning window.
        let body = serde_json::json!({"paths": [{"path": "/repo/z.rs", "mtime": 99999.0}]});
        assert_eq!(parse_observed_reports(&body, now), vec![("/repo/z.rs".to_string(), now)]);
        // Junk rows (no path, wrong types, empty) are skipped, not defaulted.
        let body = serde_json::json!({"paths": [{"mtime": 1.0}, 42, "", {"path": "  "}]});
        assert!(parse_observed_reports(&body, now).is_empty());
    }

    /// AC-355's gate bit, both directions (mutation-sweep survivor #1: forcing
    /// the derivation constant passed the whole suite either way, and the
    /// `false` direction re-opens the exact bug — unclaimed paths never block).
    #[test]
    fn a_live_blind_cotenant_gates_unclaimed_paths() {
        assert!(gates_unclaimed(&["blind-lane".into()]),
                "a live blind cotenant must force unclaimed paths to block — UNKNOWN as SAFE is AC-355");
        assert!(!gates_unclaimed(&[]),
                "no blind cotenant, no gate — forcing this true would block every unclaimed path everywhere");
    }

    /// The observed-edits STORE half (mutation-sweep survivors #2-4): the parse
    /// half got pinned because it got reviewed, and the retention/cap/ordering
    /// semantics next to it had nothing — deleting the window prune, reversing
    /// the cap sort, or removing the cap entirely each passed all 1171 tests.
    /// All three through the real handler against a real store.
    #[tokio::test]
    async fn observed_store_prunes_the_window_and_the_cap_drops_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        let mk_state = || crate::api::AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let hdrs = || {
            let mut h = HeaderMap::new();
            h.insert("x-amux-session", "obs-pin".parse().unwrap());
            h
        };
        let post = |body: Value| {
            let st = mk_state();
            let h = hdrs();
            async move { observed_edits(axum::extract::State(st), h, axum::Json(body)).await }
        };
        let read_map = || -> HashMap<String, f64> {
            store
                .read()
                .unwrap()
                .query_row(
                    "SELECT value FROM prefs WHERE key='observed_edits:obs-pin'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default()
        };
        let now = now_epoch();
        // Window prune (L726): a record far beyond OBSERVED_WINDOW_S is
        // dropped from the PRIOR map on the next write. Without the retain it
        // lives forever and a stale observation vetoes AF-27 indefinitely.
        let _ = post(json!({"paths": [{"path": "/repo-obs/ancient.rs", "mtime": now - 30_000.0}]})).await;
        let _ = post(json!({"paths": [{"path": "/repo-obs/fresh.rs", "mtime": now}]})).await;
        let m = read_map();
        assert!(m.contains_key("/repo-obs/fresh.rs"));
        assert!(
            !m.contains_key("/repo-obs/ancient.rs"),
            "a record older than the window must be pruned at the next write, not live forever"
        );
        // Cap (L764) + sort direction (L762): 1 fresh + 500 batch = 501 rows,
        // one over the cap. The cap must hold at exactly OBSERVED_MAX_ROWS and
        // the victim must be the OLDEST — "the newest observation is the one
        // the next commit is about".
        let batch: Vec<Value> = (0..OBSERVED_MAX_ROWS)
            .map(|i| json!({"path": format!("/repo-obs/p{i:03}.rs"), "mtime": now - 600.0 + i as f64}))
            .collect();
        let _ = post(json!({"paths": batch})).await;
        let m = read_map();
        assert_eq!(m.len(), OBSERVED_MAX_ROWS, "the row cap must actually cap");
        assert!(
            m.contains_key("/repo-obs/fresh.rs"),
            "the NEWEST record must survive the cap — a reversed sort drops it first"
        );
        assert!(
            !m.contains_key("/repo-obs/p000.rs"),
            "the OLDEST record is the cap's victim, per the design comment"
        );
    }

    /// AF-127, the design's two load-bearing refusals plus the episode
    /// bookkeeping, exercised through the real handler:
    /// - `proceeded` without a declared override is REJECTED — inferring it
    ///   is the D1 scraper pattern and bundles the audited override with the
    ///   reflexive ack, the two cases this table exists to separate.
    /// - a peer cannot close someone else's block.
    /// - attaching an outcome closes the episode's older retry-blocks as
    ///   'superseded', so they cannot inflate the inferred-aborted cell.
    #[tokio::test]
    async fn guard_outcome_takes_declared_overrides_and_closes_the_episode() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        let mk_state = || crate::api::AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let now = now_epoch();
        store
            .write(move |conn| {
                // alice's retry episode (1 then 2), and bob's separate block.
                for (id, sess, d) in
                    [(1, "alice", "/repo"), (2, "alice", "/repo"), (3, "bob", "/other")]
                {
                    conn.execute(
                        "INSERT INTO guard_verdicts (id, ts, session, dir, verdict, n_foreign, guard_version) \
                         VALUES (?1, ?2, ?3, ?4, 'block', 1, 6)",
                        rusqlite::params![id, now - 100.0 + id as f64, sess, d],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let hdrs = |sess: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-amux-session", sess.parse().unwrap());
            h
        };
        let call = |sess: &str, body: Value| {
            let st = mk_state();
            let h = hdrs(sess);
            async move {
                guard_outcome(axum::extract::State(st), h, axum::Json(body)).await
            }
        };
        // Refusal 1: proceeded without the declared override.
        let (code, _) = call(
            "alice",
            json!({"resolution": "proceeded", "basis": "declared", "verdict_id": 2}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "proceeded must carry the declared override");
        // Refusal 1b (amux-frustrations' review gap): a CLIENT may not claim
        // basis="inferred" — that label is server-assigned only, and it is
        // the property the read-time arithmetic leans on. Before this cell,
        // widening the whitelist passed the entire suite.
        let (code, _) = call(
            "alice",
            json!({"resolution": "trimmed", "basis": "inferred", "verdict_id": 2}),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "inferred is server-assigned, never client-claimed");
        // Refusal 2: bob cannot close alice's row.
        let (code, body) = call(
            "bob",
            json!({"resolution": "proceeded", "basis": "declared",
                   "override": "verified_solo", "verdict_id": 2}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.0["attached"], false, "a peer must not close someone else's block");
        // The real attach: alice, by id, SOLO declared.
        let (code, body) = call(
            "alice",
            json!({"resolution": "proceeded", "basis": "declared",
                   "override": "verified_solo", "verdict_id": 2, "elapsed_s": 42.0}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.0["attached"], true);
        assert_eq!(body.0["link"], "marker");
        let row = |id: i64| -> (Option<String>, Option<String>, Option<String>) {
            store
                .read()
                .unwrap()
                .query_row(
                    "SELECT resolution, override_used, outcome_link FROM guard_verdicts WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        };
        assert_eq!(
            row(2),
            (Some("proceeded".into()), Some("verified_solo".into()), Some("marker".into())),
            "the declared override is the record — SOLO and ALLOW_FOREIGN are different claims"
        );
        assert_eq!(
            row(1).0,
            Some("superseded".into()),
            "the episode's earlier retry-block must close as superseded, not linger toward aborted"
        );
        assert_eq!(row(3).0, None, "bob's unrelated block is untouched");

        // Both clauses of the by-id attach query, pinned (amux-frustrations'
        // second sweep: dropping EITHER passed the whole 1171-test suite,
        // leaving the stale-marker guard documented-but-unenforced — the
        // comment on the query was quietly doing a test's job).
        //
        // Clause 1, `AND verdict='block'`: an id from a stale marker must not
        // close an ALLOW row that happens to share it.
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO guard_verdicts (id, ts, session, dir, verdict, guard_version) \
                     VALUES (4, ?1, 'alice', '/repo', 'allow', 6)",
                    rusqlite::params![now_epoch() - 10.0],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let (code, body) = call(
            "alice",
            json!({"resolution": "proceeded", "basis": "declared",
                   "override": "verified_solo", "verdict_id": 4}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.0["attached"], false, "an allow row must never take an outcome by id");
        assert_eq!(row(4).0, None, "the allow row stays outcome-free");
        // Clause 2, `AND resolution IS NULL`: a resolved block must not be
        // closed twice — a late or duplicate report must not overwrite the
        // outcome that already attached.
        let (code, body) = call(
            "alice",
            json!({"resolution": "trimmed", "basis": "observed", "verdict_id": 2}),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.0["attached"], false, "a resolved block must refuse a second outcome");
        assert_eq!(
            row(2),
            (Some("proceeded".into()), Some("verified_solo".into()), Some("marker".into())),
            "the first outcome survives — a duplicate report must not rewrite history"
        );
    }

    /// AMUX-3446, rebuilt from the incident's own bytes: my staged diff for
    /// 7797e45 carried MY row (present in my Edit new_string) and DESKTOP's
    /// row (present in nobody's — their records had expired). Content
    /// accounting names exactly the swept line, and it never consulted a peer
    /// record, which is the property the card demands. Trivial lines are
    /// auto-accounted so braces cannot drown the signal.
    #[test]
    fn content_accounting_names_the_swept_peer_hunk_without_peer_records() {
        let added = vec![
            r#"    RouteEntry { path: "/api/connectors/accounts", methods: &["GET"] },"#.to_string(),
            r#"    RouteEntry { path: "/api/reclaim/skipped", methods: &["GET", "DELETE"] },"#.to_string(),
            "}".to_string(), // trivial: auto-accounted
        ];
        let my_edit_content = r#"
    RouteEntry { path: "/api/connectors", methods: &["GET"] },
    RouteEntry { path: "/api/connectors/accounts", methods: &["GET"] },
    RouteEntry { path: "/api/connectors/{id}/credentials", methods: &["POST"] },
"#;
        let missing = unaccounted_added_lines(&added, my_edit_content);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(missing[0].contains("/api/reclaim/skipped"), "{missing:?}");
        // Everything accounted -> silence (the check CAN pass).
        let all_mine = unaccounted_added_lines(&added[..1], my_edit_content);
        assert!(all_mine.is_empty(), "{all_mine:?}");
    }

    /// MG-1484: a restore writes bytes equal to a committed ref — an edit
    /// record with no authored content. The command classifier must separate
    /// pure restores from anything that also AUTHORS.
    #[test]
    fn restore_only_commands_are_recognized_and_mixed_ones_are_not() {
        for cmd in [
            "git checkout origin/main -- docs/api-reference/openapi.json",
            "git restore --source=origin/main frustrations.md",
            "git checkout -- src/lib.rs",
            "cd /repo && git checkout origin/main -- a.md b.md",
            "git -C /repo checkout origin/main -- f.md && git status",
        ] {
            assert!(is_restore_only_command(cmd), "should be restore-only: {cmd}");
        }
        for cmd in [
            // The incident's own remedy paragraph names this shape: restore
            // then author. The sed half is authored content.
            "git checkout origin/main -- f.md && sed -i 's/a/b/' f.md",
            "sed -i 's/a/b/' f.rs",
            "git commit -m x",
            "git checkout origin/main -- f.md > out.log",
            "head -3 f.md",     // pure read, restores nothing
        ] {
            assert!(!is_restore_only_command(cmd), "should NOT be restore-only: {cmd}");
        }
    }

    /// MG-1484 wording: a restore-provenance path must never read as "WORK at
    /// risk" — the remedy that line prescribes operates on an empty set.
    #[test]
    fn a_restore_only_touch_is_never_reported_as_work_at_risk() {
        let (line, at_risk) = victim_path_line("docs/openapi.json", &PathFate::AtRisk, "restore");
        assert!(!at_risk, "a restore carries no authored content");
        assert!(line.contains("RESTORE"), "{line}");
        assert!(!line.contains("CHECK THIS ONE"), "{line}");
        // Authored content stays the loud case.
        let (line, at_risk) = victim_path_line("src/lib.rs", &PathFate::AtRisk, "firsthand");
        assert!(at_risk);
        assert!(line.contains("CHECK THIS ONE"), "{line}");
        // Settled and absorbed keep their calm wording regardless.
        let (line, at_risk) =
            victim_path_line("a.rs", &PathFate::SettledByOwner("abc123".into()), "restore");
        assert!(!at_risk);
        assert!(line.contains("nothing at risk"), "{line}");
        // AMUX-3445: landed-on-origin is a receipt, never a warning.
        let (line, at_risk) =
            victim_path_line("g.rs", &PathFate::LandedOnOrigin("def456".into()), "firsthand");
        assert!(!at_risk, "identical-to-origin cannot be at risk");
        assert!(line.contains("origin/main") && line.contains("def456"), "{line}");
    }

    /// MG-1484: the foreign verdict carries the owner's claim PROVENANCE so
    /// every consumer (victim notice, nudges, external tripwires) can say
    /// "you restored this" instead of "you edited this".
    /// AMUX-3778: an OBSERVED claim must not be described as a recorded write.
    ///
    /// `apply_observed` inserts observed rows into `theirs_firsthand` on
    /// purpose (AF-123 — a Bash-editing lane must not be penalised for a signal
    /// the harness denies it). That widened the set the provenance lookup
    /// checked FIRST, so every observed row reported as `firsthand`, and the
    /// victim notice asserted a recorded write over a cwd mtime. The JSON
    /// verdict already reported "observed" correctly off the same data, so the
    /// machine field was right and the human sentence was wrong.
    ///
    /// The specimen is AMUX-3763: mixpeek-general warned about radio-canada's
    /// brand-new file, both lanes interrupted.
    #[test]
    fn an_observed_claim_reads_as_circumstantial_and_a_firsthand_one_does_not() {
        let mut inp = GuardInputs::default();
        let p = "/repo/customers/radio-canada/pipeline/s04_faces.py".to_string();
        // Exactly what apply_observed does: rank it WITH firsthand, and mark
        // how it was learned.
        inp.theirs_firsthand.insert(p.clone());
        inp.theirs_observed_only.insert(p.clone());
        assert_eq!(
            provenance_of(&inp, &p),
            "observed",
            "an observed row is ranked with firsthand but is not firsthand EVIDENCE"
        );

        let (line, at_risk) = victim_path_line("s04_faces.py", &PathFate::AtRisk, "observed");
        assert!(at_risk, "it is still flagged — under-warning is the expensive direction");
        assert!(
            line.contains("OBSERVED"),
            "the notice must say how the claim was learned: {line}"
        );
        assert!(
            !line.contains("the WORK ITSELF is at risk"),
            "and must not assert a recorded write it does not have: {line}"
        );

        // CONTROL 1: a genuine firsthand edit still warns at FULL strength.
        // Without this the fix could have defanged the guard and every
        // assertion above would still pass — the acceptance criterion that
        // matters most on AMUX-3778.
        let mut inp2 = GuardInputs::default();
        inp2.theirs_firsthand.insert(p.clone());
        assert_eq!(provenance_of(&inp2, &p), "firsthand");
        let (line2, at_risk2) = victim_path_line("s04_faces.py", &PathFate::AtRisk, "firsthand");
        assert!(at_risk2);
        assert!(
            line2.contains("the WORK ITSELF is at risk"),
            "a recorded edit must keep the loud line: {line2}"
        );

        // CONTROL 2: restore still wins over both, or the new arm has silently
        // reordered a case that was already correct.
        let mut inp3 = GuardInputs::default();
        inp3.theirs_restore.insert(p.clone());
        inp3.theirs_firsthand.insert(p.clone());
        inp3.theirs_observed_only.insert(p.clone());
        assert_eq!(provenance_of(&inp3, &p), "restore");
    }

    #[test]
    fn foreign_verdicts_carry_the_owners_claim_provenance() {
        let mut g = peer_wrote("f.md", "alice", 1000.0);
        g.theirs_restore.insert("/repo/f.md".into());
        let v = classify(&[pair("f.md")], 2000.0, 3600.0, &g);
        assert_eq!(v.foreign.len(), 1, "{:?}", v.foreign);
        assert_eq!(v.foreign[0]["provenance"], serde_json::json!("restore"));
        // Without the restore mark, a firsthand claim reads firsthand.
        let g2 = peer_wrote("f.md", "alice", 1000.0);
        let v2 = classify(&[pair("f.md")], 2000.0, 3600.0, &g2);
        assert_eq!(v2.foreign[0]["provenance"], serde_json::json!("firsthand"));
    }

    /// AMUX-3436: shared rows carry the COMMITTER's edit age, so the nudge can
    /// ask owner_committed_since whether that edit is already settled — the
    /// input the settled-mine demotion runs on.
    #[test]
    fn shared_rows_carry_the_committers_edit_age() {
        let mut g = peer_wrote("f.md", "alice", 1000.0);
        g.mine.insert("/repo/f.md".into(), 1500.0);
        g.mine_firsthand.insert("/repo/f.md".into());
        let v = classify(&[pair("f.md")], 2000.0, 3600.0, &g);
        assert_eq!(v.shared.len(), 1, "both firsthand -> shared: {:?}", v.foreign);
        assert_eq!(v.shared[0]["mine_age_secs"], serde_json::json!(500));
    }

    /// AF-126: the WARN must name the segment that FAILED the read test, not
    /// the first segment — `verb=cd` on 6,173 of 10,722 retained lines read
    /// as ~75% false co-authorship until the function was consulted. And a
    /// comment segment must never force a command non-read: with the mtime
    /// gate, that minted records off a PEER's concurrent write (measured
    /// live, a 73/539 false-CONTESTED reconcile).
    #[test]
    fn the_blocking_segment_is_named_and_comments_never_force_non_read() {
        // The histogram's top shape: cd leads, sed blocks.
        assert_eq!(
            first_blocking_verb("cd /repo && sed -i 's/a/b/' f.rs").as_deref(),
            Some("sed")
        );
        // Pure read -> no blocker (this command mints nothing anyway).
        assert_eq!(first_blocking_verb("cd /repo && cat f.rs | head -3"), None);
        // Redirection is the blocker when every verb reads.
        assert_eq!(
            first_blocking_verb("cat template > out.md").as_deref(),
            Some("redirect")
        );
        // Git write subcommand named with its verb.
        assert_eq!(
            first_blocking_verb("cd x && git commit -m y").as_deref(),
            Some("git-commit")
        );
        // Comment segments are skipped: this command is a pure read despite
        // `#` and would previously have minted off a peer's concurrent mtime.
        assert!(is_pure_read_command("# note\ncat frustrations.md"));
        assert_eq!(first_blocking_verb("# note\ncat frustrations.md"), None);
        // ...but a comment plus a real write still blocks, named correctly.
        assert_eq!(
            first_blocking_verb("# note\nsed -i 's/a/b/' f.rs").as_deref(),
            Some("sed")
        );
    }

    /// The other half of the discriminator, and the anti-regression guarantee:
    /// a real inferred WRITE (the case the mechanism exists for) is NOT pure-read,
    /// so it still falls through to the mtime gate and keeps attributing.
    #[test]
    fn writers_and_unrecognized_commands_fall_through() {
        for cmd in [
            "sed -i 's/a/b/' f.rs",
            "cat template > out.md",
            "echo hi >> log.txt",
            "cmd &> out.log",
            "cp a.txt b.txt",
            "mv x.md y.md",
            "tee report.md",
            "python3 - <<'PY'\nopen('x.md','w')\nPY",
            "head foo.md && sed -i s/a/b/ bar.rs",
            "sudo head secret.md",
        ] {
            assert!(!is_pure_read_command(cmd), "should NOT be pure read: {cmd}");
        }
    }

    /// AMUX-3128 RECURRED (gtm-ticker): a careful VERIFIER reads a digest with
    /// `git show` / `git log --stat` / `git diff` / `git grep` / `git blame`, whose
    /// verb is `git` (absent from READ_ONLY_VERBS), so each minted an inferred edit
    /// and flagged the reader as co-author — blocking the committer and PUNISHING
    /// verification (the harder they check, the more they block; trains to GN=1).
    /// The exact commands the reporting verifier uses lead this list; git WRITES
    /// must still fall through so a real mutation keeps its attribution.
    #[test]
    fn git_read_subcommands_attribute_nothing_but_writes_fall_through() {
        for cmd in [
            "git show HEAD:digests/2026-08-15.md",
            "git log --stat -- digests/2026-08-15.md",
            "git diff HEAD~1 -- digests/2026-08-15.md",
            "git grep -n breach digests/",
            "git blame digests/2026-08-15.md",
            "git -C /repo show abc123",
            "git --no-pager log --oneline -5",
            "git cat-file -p HEAD:a.md",
            "git show X | grep -n row",
        ] {
            assert!(is_pure_read_command(cmd), "git read should attribute nothing: {cmd}");
        }
        for cmd in [
            "git add digests/2026-08-15.md",
            "git commit -m x",
            "git checkout -- digests/2026-08-15.md",
            "git reset --hard HEAD",
            "git restore digests/2026-08-15.md",
            "git apply patch.diff",
            "git stash pop",
            "git",
            "git show X && git add Y",
        ] {
            assert!(!is_pure_read_command(cmd), "git write/unknown must fall through: {cmd}");
        }
    }

    #[test]
    fn output_redirection_is_a_write_but_fd_dup_is_not() {
        assert!(has_output_redirection("echo x > f.txt"));
        assert!(has_output_redirection("cat a >> b"));
        assert!(has_output_redirection("cmd &> out.log"));
        assert!(has_output_redirection("foo > $LOG"));
        assert!(!has_output_redirection("make 2>&1"));
        assert!(!has_output_redirection("cmd >&2"));
        assert!(!has_output_redirection("head -40 digests/x.md"));
        assert!(!has_output_redirection("ls -t digests/"));
    }

    /// THE FIRING PATH. A staged file a peer wrote and this session never
    /// touched must BLOCK — this is the case the guard exists for and the case
    /// that has been silently unenforced since the cutover.
    #[test]
    fn peer_edited_path_blocks() {
        let inp = peer_wrote("amux-server.py", "peer-lane", 1000.0);
        let v = classify(&[pair("amux-server.py")], 1600.0, 21600.0, &inp);
        assert_eq!(v.foreign.len(), 1, "a peer's staged file MUST block");
        assert_eq!(v.foreign[0]["owner"], json!("peer-lane"));
        assert_eq!(v.foreign[0]["age_secs"], json!(600));
        // The `why` must not assert a claim the committer does not have. This
        // assertion failed on the first cut — python's wording said "your claim
        // is inferred" even with an EMPTY claim set — which is what the message
        // is for: telling the reader what was actually compared.
        let why = v.foreign[0]["why"].as_str().unwrap();
        assert!(why.contains("no edit record on this path"), "misleading why: {why}");
        assert!(why.contains("edit records, not the staged diff"), "why: {why}");
        assert!(v.shared.is_empty() && v.unclaimed.is_empty());
    }

    /// A staged DELETION of a peer's file blocks identically — the path no
    /// longer exists on disk, which is why `realpath` must not require it to.
    #[test]
    fn peer_deleted_path_still_blocks() {
        let inp = peer_wrote("gone.rs", "peer-lane", 1500.0);
        let v = classify(&[pair("gone.rs")], 1600.0, 21600.0, &inp);
        assert_eq!(v.foreign.len(), 1, "a staged deletion of a peer's file MUST block");
    }

    /// AF-19, the regression that let 762e06e through: an INFERRED self-claim (a
    /// Bash command whose mtime move coincided with the peer's write) must not
    /// outrank a peer's first-hand write. Reconciled with AF-27: the real 762e06e
    /// is a TIE — the inferred claim IS the peer's concurrent write, same mtime
    /// event, so committer_ts == peer_ts. A claim no fresher than the peer's does
    /// not prove ownership, so the tie still BLOCKS. (The original fixture used a
    /// clearly-fresher committer, which AF-27 showed is a different case that must
    /// NOT block — see `a_clearly_fresher_self_edit_beats_a_stale_firsthand_peer`.)
    /// AF-156: the OUTDATED HOOK remedy must be runnable BY ITS AUDIENCE.
    ///
    /// The old constant `scripts/install-hooks.sh` resolved for nobody who
    /// received it — every one of 272 warnings in a day came from lanes under
    /// /Users/ethan/Dev/mixpeek, where that path does not exist. The remedy must
    /// therefore carry the TARGET DIR (install-hooks.sh's foreign-checkout mode,
    /// which never writes the other repo's pre-commit).
    ///
    /// The second cell is the one that matters: with no AMUX_REPO the string
    /// must be VISIBLY unfilled, not a plausible relative path that silently
    /// resolves to nothing. A remedy that looks runnable and is not costs the
    /// reader the attempt, which is worse than printing none.
    #[test]
    fn the_outdated_hook_remedy_names_the_target_dir_and_never_fakes_a_path() {
        let d = "/Users/ethan/Dev/mixpeek/server/mvs";
        // SAFETY: single-threaded unit test, restored below.
        let prev = std::env::var("AMUX_REPO").ok();
        std::env::set_var("AMUX_REPO", "/Users/ethan/Dev/amux");
        let r = super::outdated_hook_remedy(d);
        assert_eq!(r, format!("/Users/ethan/Dev/amux/scripts/install-hooks.sh {d}"));
        assert!(r.contains(d), "the remedy must name the dir it is fixing: {r}");

        std::env::remove_var("AMUX_REPO");
        let bare = super::outdated_hook_remedy(d);
        assert!(
            bare.starts_with("<your amux checkout>"),
            "with no AMUX_REPO the path must be visibly unfilled, not a plausible \
             relative path that resolves nowhere: {bare}"
        );
        assert!(bare.contains(d), "and it must still name the target dir: {bare}");
        match prev {
            Some(v) => std::env::set_var("AMUX_REPO", v),
            None => std::env::remove_var("AMUX_REPO"),
        }
    }

    #[test]
    fn inferred_self_claim_no_fresher_than_the_peer_still_blocks() {
        let mut inp = peer_wrote("test_x.py", "peer-lane", 1000.0);
        inp.mine.insert("/repo/test_x.py".into(), 1000.0); // inferred, same instant as the peer
        let v = classify(&[pair("test_x.py")], 1600.0, 21600.0, &inp);
        assert_eq!(v.foreign.len(), 1, "an inferred self-claim no fresher than the peer must still block");
        assert!(v.foreign[0]["why"].as_str().unwrap().contains("your claim is inferred"));
    }

    /// AF-27: the committer's OWN edit is clearly fresher than the peer's stale
    /// first-hand one (real case: committer 23.8s ago, peer 14,355s / 4h ago). The
    /// committer edited AFTER the peer and owns the current content, so this must
    /// be SHARED (warned), never foreign. amux-frustrations' per-path forensic
    /// isolated exactly this shape — 6 of 405 verdicts (in_mine=true,
    /// in_firsthand=false, committer fresher) — from the 399 true positives.
    #[test]
    fn a_clearly_fresher_self_edit_beats_a_stale_firsthand_peer() {
        let mut inp = peer_wrote("hand_raiser.py", "peer-lane", 1000.0); // peer, older
        inp.mine.insert("/repo/hand_raiser.py".into(), 1580.0); // committer, clearly fresher
        let v = classify(&[pair("hand_raiser.py")], 1600.0, 21600.0, &inp);
        assert!(v.foreign.is_empty(), "a clearly fresher self-edit must not block: {:?}", v.foreign);
        assert_eq!(v.shared.len(), 1, "it is shared (warned), not foreign");
        assert_eq!(v.shared[0]["peer"], json!(true));
    }

    /// AF-27 margin (amux-frustrations forensic): the one coincident near-tie was
    /// 0.2s, a sign that flips across the disk-mtime/transcript-ts clocks. A self
    /// edit fresher by LESS than the skew margin is not meaningfully fresher and
    /// must STILL block — else the 762e06e tie reopens on the next coincident write.
    #[test]
    fn a_within_skew_fresher_self_edit_still_blocks() {
        let mut inp = peer_wrote("coincident.rs", "peer-lane", 1000.0);
        // Fresher by (margin - 1)s — inside the skew, so it does NOT count.
        inp.mine.insert("/repo/coincident.rs".into(), 1000.0 + RECENCY_SKEW_MARGIN_S - 1.0);
        let v = classify(&[pair("coincident.rs")], 1600.0, 21600.0, &inp);
        assert_eq!(v.foreign.len(), 1, "a within-skew freshness lead must still block");
    }

    /// The legitimate claim: both sessions really edited it. Warned, never
    /// blocked — blocking would deadlock a genuinely shared file.
    #[test]
    fn firsthand_on_both_sides_is_shared_not_foreign() {
        let mut inp = peer_wrote("shared.rs", "peer-lane", 1000.0);
        inp.mine.insert("/repo/shared.rs".into(), 1200.0);
        inp.mine_firsthand.insert("/repo/shared.rs".into());
        let v = classify(&[pair("shared.rs")], 1600.0, 21600.0, &inp);
        assert!(v.foreign.is_empty());
        assert_eq!(v.shared.len(), 1);
        assert_eq!(v.shared[0]["peer"], json!(true));
    }

    /// AF-24: your OWN file with unstaged changes is not a co-edit. `peer`
    /// false is what stops the renderer inventing a session named "(unknown)".
    #[test]
    fn own_dirty_file_is_shared_without_a_phantom_peer() {
        let mut inp = GuardInputs::default();
        inp.mine.insert("/repo/mine.rs".into(), 1200.0);
        inp.mine_firsthand.insert("/repo/mine.rs".into());
        inp.dirty.insert("/repo/mine.rs".into());
        let v = classify(&[pair("mine.rs")], 1600.0, 21600.0, &inp);
        assert_eq!(v.shared.len(), 1);
        assert_eq!(v.shared[0]["peer"], json!(false));
        assert_eq!(v.shared[0]["owner"], json!("(unknown)"));
        assert_eq!(v.shared[0]["has_unstaged_changes"], json!(true));
    }

    /// THE BELT (AMUX-2443): staged, no record from anyone. Not blocked, but
    /// never silent — silence is what made 762e06e a sweep.
    #[test]
    fn unrecorded_path_blocks_because_unknown_is_not_safe() {
        // CONTRACT CHANGE, AC-355. This test previously asserted the opposite —
        // foreign empty, unclaimed populated, i.e. warn-and-ship — and it was
        // encoding the bug: "no session can account for this staged file" was
        // treated as safe because there was no owner to defer to. Two sweeps in
        // one day went through that gap, including the commit that was fixing
        // this guard's own notice.
        //
        // Kept and inverted rather than deleted, so the change of contract is
        // visible in the diff to anyone who wonders why unclaimed now blocks.
        // Blind cotenant present: this is the sweep case, and it must BLOCK.
        let blind = GuardInputs { blind_cotenant: true, ..Default::default() };
        let v = classify(&[pair("mystery.txt")], 1600.0, 21600.0, &blind);
        assert_eq!(v.foreign.len(), 1, "an unattributable staged path must BLOCK");
        assert_eq!(v.foreign[0]["path"], json!("mystery.txt"));
        assert_eq!(v.foreign[0]["owner"], json!(""), "no owner can be named — that IS the finding");
        // The refusal must be actionable or it gets routed around: name the
        // escape and the exact diff command (the AMUX-2325 lesson).
        let why = v.foreign[0]["why"].as_str().unwrap();
        assert!(why.contains("AMUX_VERIFIED_SOLO"), "escape not named: {why}");
        assert!(why.contains("git diff --cached"), "no way to check it: {why}");
        // It blocks via `foreign` specifically, because that is the only field
        // installed hooks act on (module docs). A new key would be ignored by
        // every hook already on disk.
        assert!(v.unclaimed.is_empty(), "must not ALSO sit in the non-blocking list");

        // FULLY ATTRIBUTED checkout: nobody is hidden, so the same path is NOT a
        // sweep — it is most often the committer's own pre-window or generated
        // work. It stays a warning. Without this scoping the block adds a class
        // to a guard already refusing ~18/hour, which AMUX-2936 measured and
        // named as what gets the guard disabled outright.
        let clear = classify(&[pair("mystery.txt")], 1600.0, 21600.0, &GuardInputs::default());
        assert!(clear.foreign.is_empty(), "must NOT block when every cotenant is visible");
        assert_eq!(clear.unclaimed.len(), 1, "still disclosed, just not blocking");
    }

    /// The envelope shape the installed hooks parse. Every key on every path,
    /// including the short circuits: a hook reading `d["shared"]` gets `[]`.
    #[test]
    fn envelope_carries_every_key_the_hook_reads() {
        let v = Envelope { window: 21600.0, hook_outdated: true, ..Default::default() }.json();
        for k in ["ok", "foreign", "shared", "unclaimed", "cotenants", "window_secs"] {
            assert!(v.get(k).is_some(), "missing key the installed hook reads: {k}");
        }
        assert!(v["foreign"].is_array() && v["shared"].is_array() && v["unclaimed"].is_array());
        assert_eq!(v["undecided"], json!(false));
        assert_eq!(v["hook_outdated"], json!(true));
    }

    /// `undecided` must never look like a clean verdict.
    #[test]
    fn undecided_is_distinguishable_from_all_clear() {
        let v = Envelope {
            window: 21600.0,
            undecided: Some("cotenants unknown".into()),
            ..Default::default()
        }
        .json();
        assert_eq!(v["undecided"], json!(true));
        assert_eq!(v["reason"], json!("cotenants unknown"));
        assert!(v["foreign"].as_array().unwrap().is_empty());
    }

    /// `realpath` on a path that does not exist must still resolve its
    /// existing ancestors — otherwise staged deletions key differently from
    /// the transcript records and drop out of the comparison entirely.
    #[test]
    fn realpath_resolves_nonexistent_leaf() {
        let dir = std::env::temp_dir();
        let real_dir = dir.canonicalize().unwrap();
        let got = realpath(&dir.join("definitely-not-here-amux-guard.rs"));
        assert_eq!(got, real_dir.join("definitely-not-here-amux-guard.rs").to_string_lossy());
    }

    #[test]
    fn window_has_a_floor() {
        // py:19187: max(600, env). A tiny window would make almost everything
        // `unclaimed` and nothing `foreign` — a guard that never blocks.
        assert!(window_secs() >= WINDOW_FLOOR);
    }
}

#[cfg(test)]
mod cd_is_not_a_mutation {
    use super::*;

    /// AEAB-24. `cd` was missing from READ_ONLY_VERBS, and its absence silently
    /// undid the git-read exemption that AMUX-3128 added right above it.
    ///
    /// `is_pure_read_command` splits on `| ; & \n ( ) \``, takes the FIRST token of
    /// each segment as that segment's verb, and returns false the moment one verb
    /// is not read-only. So a leading `cd` — which cannot modify anything —
    /// decided the whole command, and `cd /repo && git show f` minted an inferred
    /// edit record naming the reader as co-author of a file they only inspected.
    ///
    /// That is the exact harm the git-read exemption exists to prevent, and its
    /// own comment says so: "the harder a peer checks your output, the more it
    /// blocks you, which trains toward GN=1 where the guard stops protecting
    /// anything." The exemption worked for a bare `git show` and was defeated by
    /// the most common prefix in this repo's workflow — 117 of 191 inferred-edit
    /// records in one 24h window had verb=cd.
    ///
    /// The SAFETY half of this test is the load-bearing half. Adding a verb to
    /// READ_ONLY_VERBS widens what the guard treats as harmless, and a widening
    /// that goes too far stops the guard protecting anything while leaving every
    /// other test green.
    #[test]
    fn cd_is_a_read_verb_but_never_launders_a_mutation() {
        // Reads that a leading `cd` used to misclassify.
        for cmd in [
            "cd /tmp",
            "cd /tmp && cat f.md",
            "cd /repo && git show origin/main:frustrations.md",
            "cd /repo && grep -n needle src/lib.rs",
        ] {
            assert!(is_pure_read_command(cmd), "should be a pure read: {cmd}");
        }

        // SAFETY — every one of these still mutates, and must still be attributed.
        // `cd` must not launder the mutation that follows it.
        for cmd in [
            "cd /tmp && echo hi > f.md",              // output redirection
            "cd /tmp && cat > f.md <<'EOF'\nx\nEOF",  // heredoc write
            "cd /tmp && rm f.md",                     // non-read verb
            "cd /tmp && sed -i '' s/a/b/ f.md",       // in-place edit
            "cd /repo && git commit -am x",           // git WRITE subcommand
            "cd /repo && git add -A",
        ] {
            assert!(!is_pure_read_command(cmd), "must NOT be a pure read: {cmd}");
        }
    }
}
