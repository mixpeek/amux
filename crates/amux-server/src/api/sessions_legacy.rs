//! GET /api/sessions — the PYTHON-SHAPED session list (RR-0075 enabler).
//!
//! The alias layer rewrites legacy PATHS, but the SPA also expects the
//! Python RESPONSE SHAPE: a bare array of `{name, status, preview, ...}`.
//! The modern /api/workers envelope (items/total, display_name, typed
//! state) is right for new clients; this projection is what lets the
//! 44k-line dashboard render workers today, unchanged. It is registered
//! BEFORE the rewrite middleware so it wins over the path alias.

use super::AppState;
use crate::backend::tmux::pane_target;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// WorkerState -> the Python status vocabulary the SPA's badges render.
fn python_status(state_json: &str) -> &'static str {
    // state_json is the row's JSON WorkerState; match on the tag cheaply.
    if state_json.contains("\"active\"") {
        "active"
    } else if state_json.contains("\"idle\"") {
        "idle"
    } else if state_json.contains("\"waiting\"") {
        "waiting"
    } else if state_json.contains("\"rate_limited\"") {
        "rate-limited"
    } else if state_json.contains("\"error\"") {
        "error"
    } else if state_json.contains("\"starting\"") {
        "starting"
    } else {
        "" // stopped renders as blank in the Python list
    }
}

// ---- status derivation (AMUX-2589) ---------------------------------------
//
// Python's `status` is its scanner's judgment (pane regex) overridden by a
// fresh self-report (amux-server.py:20201-20263). The Rust server runs no
// scanner (D1: scrapers are the deviation, not the goal), so the honest
// equivalents are, in Python-precedence order:
//   base:  the Python scanner's own LAST PERSISTED judgment — the
//          session.working/idle/waiting transition it writes to
//          `session_events` (py:20268-20270, the D1 report-endpoint shape:
//          a durable store the producer already writes) — guarded against
//          staleness (pre-restart events discarded; an `active` with no
//          pane output for AMUX_ACTIVE_HEARTBEAT_S is not active);
//          falling back to tmux activity (<60s = active, else idle).
//   over:  self_report when fresh, with Python's ASYMMETRIC freshness
//          (py:20233-20263): `idle` does not decay (the only exit is a
//          prompt, which fires UserPromptSubmit -> a new report; window
//          AMUX_HOOKS_LIVE_IDLE_S=86400), `active`/`waiting` do
//          (AMUX_HOOKS_LIVE_S=1800), and a stale `active` report (older
//          than the heartbeat, AMUX_ACTIVE_HEARTBEAT_S=120) never
//          overrides — a long turn is byte-identical to a wedged one.
//   last:  CONTRADICTION — physical evidence overrides a stale `idle`
//          (AMUX-2646, below).
//   "" :   not running.
//
// AMUX-2646 — "it is running but says idle". The asymmetric window above
// says an `idle` report never decays, on the reasoning that "the only exit
// from idle is a prompt, and every prompt fires UserPromptSubmit". That
// premise is false in at least four reachable ways, and each of them leaves
// a lane permanently mislabelled because nothing else in the derivation
// could ever disagree:
//
//   1. The UserPromptSubmit POST is best-effort (`curl -m 2`, no retry). The
//      server re-execs on every save of its own source on a shared checkout,
//      so a dropped report is routine, not exotic.
//   2. `report_post` then REFUSES every `tool-hook` heartbeat for the rest of
//      the turn ("a heartbeat must not resurrect a finished turn",
//      AMUX-2538) — the one signal that could self-heal is suppressed by
//      design, correctly, for a different reason.
//   3. Anything can write any state for any session over `/report`; a hand
//      -run hook test wrote `{"state":"idle","source":"stop-hook-test"}` onto
//      a LIVE working lane and it stuck for 1076s until a human noticed.
//   4. Work resumes without a prompt: a backgrounded command re-invoking the
//      agent, a resumed session, a hookless provider (gemini/codex) whose
//      stale claude-era report outlives its hooks.
//
// A claim that no evidence can contradict is not a status, it is an axiom.
// So `idle` still survives SILENCE for the full 24h — a parked lane must not
// be re-scraped forever, which is the asymmetry's real purpose — but it does
// not survive CONTRADICTION: a pane that is unambiguously mid-turn AND has
// painted within AMUX_IDLE_CONTRADICTION_S overrides an idle report older
// than that same window. Both halves are required, and the "has painted"
// half is what keeps this from re-reading a parked lane's scrollback: a lane
// that quotes "esc to interrupt" in a transcript it wrote hours ago (a real
// self-block here, AMUX-2642) emits no output, so it is never probed.
//
// KNOWN residual, measured 2026-08-09 against the live fleet (114/116
// exact): Python emits "" for a RUNNING session whose pane shows no
// recognizable agent UI (claude exited to a shell). That cell exists only
// in the pane regex; this derivation reads idle for it. Re-implementing
// the regex would deepen D1, so the residual is documented, not coded away.

fn env_secs(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Derive the waiting_reason from a pane capture: "permission_prompt",
/// "user_input", "rate_limit", or "" (not waiting / unknown).
///
/// Mirrors the logic in backend::adapter but runs against the preview
/// pane content that build_array already has in hand. Does not spawn
/// any subprocess.
fn derive_waiting_reason(raw: &str) -> &'static str {
    if raw.is_empty() {
        return "";
    }
    let clean = strip_ansi(raw);
    let low = clean.to_lowercase();

    if crate::api::session_verbs::is_rate_limit_menu(raw) {
        return "rate_limit";
    }
    if low.contains("do you want to proceed") {
        return "permission_prompt";
    }
    if low.contains("approve") && !low.contains("bypass permissions on") {
        let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
        for l in lines.iter().rev().take(5) {
            if l.to_lowercase().contains("approve") && !l.to_lowercase().contains("esc to interrupt") {
                return "permission_prompt";
            }
        }
    }
    if low.contains("enter to select") || low.contains("esc to cancel") {
        return "user_input";
    }
    if clean.contains("Resume from summary") && clean.contains("Resume full session") {
        return "user_input";
    }
    ""
}

/// The whole-fleet pane snapshot, shared by every reader inside the TTL.
///
/// A process global rather than a field on `AppState`: `FleetSignals::load` is
/// a free function called from three places that do not share a handle, and
/// threading one through would be a wider change to files other lanes are
/// editing. The value is a pure cache — dropping it costs one re-capture and
/// changes no verdict.
#[allow(clippy::type_complexity)]
fn pane_cache() -> &'static std::sync::Mutex<(f64, BTreeMap<String, String>)> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<(f64, BTreeMap<String, String>)>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new((0.0, BTreeMap::new())))
}

/// Response-level cache for `build_array`: the serialized JSON string + the
/// epoch it was computed at. At 3,714 req/hr (~1/s) with each call spawning
/// ~100 tmux subprocesses for previews + N git subprocesses + ~226 filesystem
/// reads, a 2s TTL collapses the real work by ~2x while being invisible to a
/// human polling the dashboard.
struct ListSnapshot {
    /// When the build that produced `json` was entered.
    stamp: f64,
    /// The serialized array; empty = no snapshot (cold or invalidated).
    json: String,
    /// `SESSIONS_EPOCH` at build start — serving requires it unchanged.
    epoch: u64,
    /// `registry_fingerprint()` at build start — see that function.
    registry: u64,
}

fn build_array_cache() -> &'static std::sync::Mutex<ListSnapshot> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ListSnapshot>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        std::sync::Mutex::new(ListSnapshot {
            stamp: 0.0,
            json: String::new(),
            epoch: 0,
            registry: 0,
        })
    })
}

/// Invalidation epoch (AMUX-2960): bumped by [`invalidate_sessions_cache`].
/// A builder snapshots it before building and only writes its result back if
/// no invalidation landed mid-build. Without this, a build that STARTED
/// before a worker create finishes AFTER the create's invalidation and stamps
/// the pre-create list into the cache — resurrecting exactly the staleness
/// the invalidation was for.
static SESSIONS_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Order-independent fingerprint of WHICH workers exist: the set of `*.env`
/// stems in the sessions dir.
///
/// This is the structural half of AMUX-2960. The per-call-site
/// `invalidate_sessions_cache()` discipline failed twice in one week — the
/// AMUX-2926 config-write hole, then `create_session_legacy`/`delete_post`
/// writing the registry with no invalidation, which made the worker-card-counts
/// e2e flaky-red for a day (the SPA's one post-reload fetch served the
/// pre-create fleet and SSE never corrected it). A guard on the substrate
/// covers the NEXT forgotten call site too, and any out-of-band write (a human
/// `rm`, the bash CLI).
///
/// Deliberately the `.env` NAME SET, not the dir mtime: `.meta.json` files in
/// the same dir churn on every send fleet-wide, so an mtime guard would
/// invalidate ~every request and resurrect the AR-135 pool-starvation
/// stampede this cache exists to prevent. Content edits inside an env file
/// don't move the set — those paths already invalidate explicitly.
fn registry_fingerprint() -> u64 {
    use std::hash::{Hash, Hasher};
    let dir = amux_home().join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut acc = 0u64;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("env") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                stem.hash(&mut h);
                acc ^= h.finish(); // XOR: order-independent, names are unique
            }
        }
    }
    acc
}

/// Drop the cached session list so the very next GET rebuilds (AMUX-2926).
///
/// Any write that changes a worker's config must call this. Python invalidated
/// its equivalent cache on every config write, and the rust config-write path
/// carried a comment saying it did not need to — "this origin computes the list
/// per request, so the write IS the refresh". That was TRUE when written and
/// stopped being true when the 2s cache landed (7ca14b5, a later commit).
/// Nothing failed; the comment just quietly became a lie, and for up to 2s
/// after a config write the list served the OLD value.
///
/// That mattered because tags configure GROUP ISOLATION and the gate reads them
/// live: the messaging gate saw the new tag while the dashboard still showed the
/// old one, so an operator could tag a lane, see no change, and re-tag or give
/// up while the isolation behaviour had already moved underneath them (found by
/// amux-frustrations while peer-verifying AMUX-2916).
///
/// Cheap by construction — it clears one string; the next reader pays the
/// rebuild it would have paid 2s later anyway.
pub fn invalidate_sessions_cache() {
    // Epoch first: an in-flight builder checks it AFTER building, so bumping
    // before the clear means no interleaving lets a pre-bump build survive.
    SESSIONS_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut c) = build_array_cache().lock() {
        c.stamp = 0.0;
        c.json.clear();
    }
    tracing::debug!(target: "amux::sessions", "sessions list cache invalidated by a config write");
}

/// Git branch cache: dir -> (branch, epoch). Branches change on the scale of
/// minutes; re-running `git rev-parse` per directory on every request is pure
/// waste.
fn git_branch_cache() -> &'static std::sync::Mutex<(f64, BTreeMap<String, String>)> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<(f64, BTreeMap<String, String>)>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new((0.0, BTreeMap::new())))
}

/// Preview capture cache: name -> raw pane text, with a TTL matching the
/// status-pane cache. Previews are the dominant cost in build_array (~100
/// tmux capture-pane subprocesses per call).
fn preview_cache() -> &'static std::sync::Mutex<(f64, BTreeMap<String, String>)> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<(f64, BTreeMap<String, String>)>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new((0.0, BTreeMap::new())))
}

/// One `tmux list-sessions` line -> (name, last-painted, created).
///
/// Pulled out of `load` so the ACTIVITY RULE is testable without a tmux
/// server: it is the rule that was silently wrong for the whole fleet, and a
/// test that re-types the parse inline would have agreed with whatever it was
/// re-typing. Returns `None` for a line that does not carry at least a name.
fn parse_list_sessions_line(l: &str) -> Option<(&str, Option<i64>, Option<i64>)> {
    let mut it = l.split(':');
    let name = it.next()?;
    if name.is_empty() {
        return None;
    }
    let (a, c, w) = (it.next(), it.next(), it.next());
    // max(session_activity, window_activity) — see `FleetSignals::activity`.
    let last_paint: Option<i64> = a
        .and_then(|x| x.parse().ok())
        .into_iter()
        .chain(w.and_then(|x| x.parse::<i64>().ok()))
        .max();
    Some((name, last_paint, c.and_then(|x| x.parse().ok())))
}

/// Signals the derivation reads, loaded once per request and shared with the
/// board's `stale` computation (`active_python_sessions`) so the two can
/// never disagree about who is working.
pub struct FleetSignals {
    /// tmux session name (`amux-<n>`) -> when its pane last PAINTED, i.e.
    /// `max(#{session_activity}, #{window_activity})`.
    ///
    /// It has to be the max, and that is not a belt-and-braces choice.
    /// `#{session_activity}` does not track pane output for a DETACHED
    /// session, and every amux lane is detached: measured on tmux 3.6a,
    /// 2026-08-09, 60 of 63 live sessions had a `session_activity` more than
    /// 60s older than their `window_activity`, and `amux-rust` — mid-turn,
    /// spinner repainting ~6/s — reported a `session_activity` that had not
    /// moved in 34.5 HOURS (it was still equal to `session_created`).
    ///
    /// Everything downstream read that as silence, so the two places this
    /// derivation consults physical liveness were both dead: the
    /// `now - act < 60` fallback could never say `active`, and the guard that
    /// demotes a stale `active` transition fired for EVERY session on every
    /// request. The fleet's status was therefore whatever the self-reports
    /// said and nothing else — which is precisely why one wrong report could
    /// not be contradicted by anything.
    pub activity: BTreeMap<String, i64>,
    /// tmux session name -> `#{session_created}`.
    pub created: BTreeMap<String, i64>,
    /// Live tmux session names.
    pub running: BTreeSet<String>,
    /// tmux session names whose pane is sitting in a bare SHELL — the tmux
    /// session exists but the agent inside it is gone. `stop` deliberately
    /// leaves the tmux session alive (Python parity), so tmux-existence alone
    /// says "there is a window", not "there is a worker": a stopped lane read
    /// as running=true forever on the card while `/api/sessions/<n>/info`
    /// (which checks the pane) said false. Two answers to one question, and
    /// the card is the one the user is looking at — clicking Stop appeared to
    /// do nothing. Measured 2026-08-09.
    pub shell_only: BTreeSet<String>,
    /// The persisted self-report store (prefs `session_reports`,
    /// amux-server.py:3943) — the same bytes Python hydrates at boot.
    pub reports: serde_json::Value,
    /// session -> (status, ts) of its latest working/idle/waiting transition.
    pub transitions: BTreeMap<String, (String, f64)>,
    /// session -> ts of its latest `session.started` event.
    pub started: BTreeMap<String, f64>,
    /// session name -> raw pane capture, for lanes that PAINTED recently.
    ///
    /// The only physical evidence in this struct: everything else is a claim
    /// somebody wrote down. Populated by [`FleetSignals::capture_panes`] and
    /// read only through [`FleetSignals::pane_of`], which re-applies the same
    /// candidacy predicate the capture used — so a caller that captures more
    /// (the session list, which already has every running pane in hand for
    /// previews) and one that captures less (the board) still derive the same
    /// status for the same lane. A view that disagrees with the mechanism it
    /// describes is worse than no view.
    pub panes: BTreeMap<String, String>,
    /// session -> newest mtime across its SUBAGENT transcripts
    /// (`~/.claude/projects/<proj>/<conv>/subagents/*.jsonl`).
    ///
    /// A lane's Stop hook fires when the MAIN turn ends, so a lane whose
    /// background agents are still working reports `idle` — correctly, about
    /// the main turn, and misleadingly about the lane. Measured 2026-08-11:
    /// primis read `idle` while a subagent had written 20 seconds earlier.
    ///
    /// This is the structured answer to that, not a pane marker: it is durable,
    /// survives a lane that is not painting, and does not break when Claude
    /// Code changes a glyph (D1). One walk per pass — 21ms for 1844 transcripts
    /// on this machine, 0.16% of a scan tick.
    pub subagent_activity: BTreeMap<String, f64>,
    pub now: f64,
}

impl FleetSignals {
    pub fn load(conn: &rusqlite::Connection) -> Self {
        let mut activity = BTreeMap::new();
        let mut created = BTreeMap::new();
        let mut running = BTreeSet::new();
        // The Ok() only means the SPAWN worked — tmux exiting non-zero (no
        // server, wrong socket) still lands here with empty stdout, and an
        // empty fleet is indistinguishable from a dead probe (ethos rule 4;
        // live incident 2026-08-09: launchd build served running=0 for 116
        // cards while 62 tmux sessions ran, with nothing in the log).
        //
        // Separator is ':' NOT '\t': under launchd there is no LANG, and in
        // the POSIX locale tmux sanitizes non-printable output chars to '_',
        // so a tab-separated format came back as `name_123_456` and every
        // parse silently missed (the same 2026-08-09 incident — /api/debug/tmux
        // is what caught it). ':' is safe because tmux forbids it in session
        // names (target syntax), and printable chars are never sanitized.
        //
        // `#{window_activity}` is the 4th field because `#{session_activity}`
        // is not a liveness signal for a detached session — see the `activity`
        // field's doc for the measurement. It resolves to the session's
        // CURRENT window; amux creates one window per session, and the agent
        // runs in it.
        let tmux_out = std::process::Command::new("tmux")
            .args([
                "list-sessions",
                "-F",
                "#{session_name}:#{session_activity}:#{session_created}:#{window_activity}",
            ])
            .output();
        match &tmux_out {
            Ok(o) if !o.status.success() => tracing::warn!(
                status = %o.status,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "tmux list-sessions failed — fleet will read as not-running"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "tmux spawn failed — fleet will read as not-running"
            ),
            _ => {}
        }
        if let Ok(o) = tmux_out {
            for l in String::from_utf8_lossy(&o.stdout).lines() {
                let Some((n, a, c)) = parse_list_sessions_line(l) else {
                    continue;
                };
                running.insert(n.to_string());
                if let Some(ts) = a {
                    activity.insert(n.to_string(), ts);
                }
                if let Some(ts) = c {
                    created.insert(n.to_string(), ts);
                }
            }
        }
        // ONE extra batched tmux call for the whole fleet (not per session):
        // which panes are a bare shell. `#{pane_current_command}` is the
        // foreground command, so an agent shows as `claude`/`node`/`codex`
        // and a stopped lane shows as `bash`. A session with several panes
        // counts as shell-only only if EVERY pane is a shell.
        let mut shell_only = BTreeSet::new();
        if let Ok(o) = std::process::Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{session_name}:#{pane_pid}:#{pane_current_command}"])
            .output()
        {
            const SHELLS: [&str; 8] = ["bash", "zsh", "sh", "fish", "dash", "ksh", "tcsh", "csh"];
            let mut any_live: BTreeSet<String> = BTreeSet::new();
            let mut seen: BTreeSet<String> = BTreeSet::new();
            // Panes whose FOREGROUND command is a shell but which might still host
            // an agent as a CHILD: (session, pane_pid). Collected here and probed
            // below only for sessions not already proven live by another pane.
            let mut shell_panes: Vec<(String, String)> = Vec::new();
            for l in String::from_utf8_lossy(&o.stdout).lines() {
                // format is session:pid:cmd, but a session NAME can contain ':',
                // so split from the RIGHT twice: cmd, then pid.
                let Some((rest, cmd)) = l.rsplit_once(':') else { continue };
                let Some((sess, pid)) = rest.rsplit_once(':') else { continue };
                seen.insert(sess.to_string());
                let cmd = cmd.trim().trim_start_matches('-');
                if !SHELLS.contains(&cmd) {
                    any_live.insert(sess.to_string());
                } else {
                    shell_panes.push((sess.to_string(), pid.trim().to_string()));
                }
            }
            // AMUX-3: a foreground SHELL can still host a live agent as a child.
            // ai-video-editor read as not-running while claude (working, 80k tokens)
            // ran as a child of its pane's bash, so #{pane_current_command}="bash".
            // The live is_running() already uses pgrep -P; do the same here so the
            // fleet list agrees with it instead of hiding a running lane behind a
            // shell it never actually returned to. Bounded: only shell-foreground
            // panes whose session is not already proven live by another pane.
            for (sess, pid) in shell_panes {
                if any_live.contains(&sess) || pid.is_empty() {
                    continue;
                }
                if let Ok(ch) = std::process::Command::new("pgrep").args(["-P", &pid]).output() {
                    if !ch.stdout.iter().all(|b| b.is_ascii_whitespace()) {
                        any_live.insert(sess.clone());
                    }
                }
            }
            for s in seen {
                if !any_live.contains(&s) {
                    shell_only.insert(s);
                }
            }
        }
        let reports = conn
            .query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        // Both event queries tolerate the table being absent (a fresh Rust-only
        // AMUX_HOME): no events simply means the activity fallback decides.
        let mut transitions = BTreeMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT session, type, MAX(ts) FROM session_events \
             WHERE type IN ('session.working','session.idle','session.waiting') \
             GROUP BY session",
        ) {
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            });
            if let Ok(rows) = rows {
                for row in rows.flatten() {
                    let st = match row.1.as_str() {
                        "session.working" => "active",
                        "session.waiting" => "waiting",
                        _ => "idle",
                    };
                    transitions.insert(row.0, (st.to_string(), row.2));
                }
            }
        }
        let mut started = BTreeMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT session, MAX(ts) FROM session_events \
             WHERE type='session.started' GROUP BY session",
        ) {
            if let Ok(rows) =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))
            {
                for (s, ts) in rows.flatten() {
                    started.insert(s, ts);
                }
            }
        }
        FleetSignals {
            activity,
            created,
            running,
            shell_only,
            reports,
            transitions,
            started,
            panes: BTreeMap::new(),
            subagent_activity: scan_subagent_activity(),
            now: chrono::Utc::now().timestamp() as f64,
        }
    }

    /// Is there a WORKER in this tmux session, not merely a tmux session?
    /// This is the question the card is asking, and the one
    /// `session_verbs::is_running` answers for `/info`, restart and delete.
    /// Call this instead of touching `running` directly, or the two answers
    /// drift again.
    pub fn agent_running(&self, tmux_name: &str) -> bool {
        // The tmux SESSION must exist either way — a self-report cannot resurrect a
        // lane whose session is gone.
        if !self.running.contains(tmux_name) {
            return false;
        }
        // Primary: the pane scrape says an agent is the foreground command (or, via
        // the pgrep rescue, a child of a foreground shell).
        if !self.shell_only.contains(tmux_name) {
            return true;
        }
        // AF-82 / D1: a lane that SELF-REPORTED an active agent recently is running,
        // even when the pane scrape reads shell-only — an agent launched as a child
        // of a wrapper shell, or nested a level deeper than the pgrep rescue reaches.
        // The self-report is the harness reporting its own state (the D1 exit) and
        // the scrape is the FALLBACK; `running` ignored it while `status` already
        // honoured it, so ai-video-editor read running=False with an `active` report
        // 57s old. Same freshness guards the status override uses (from_this_life +
        // live), active/waiting only (an idle report does not assert a live agent).
        if let Some(name) = tmux_name.strip_prefix("amux-") {
            if let Some(rep) = self.reports.get(name) {
                let st = rep["state"].as_str().unwrap_or("");
                let ts = rep["ts"].as_f64().unwrap_or(0.0);
                let from_this_life = self.started.get(name).copied().unwrap_or(0.0) <= ts;
                let live = self.now - ts < env_secs("AMUX_HOOKS_LIVE_S", 1800.0);
                if from_this_life && live && (st == "active" || st == "waiting") {
                    return true;
                }
            }
        }
        false
    }

    /// How recent must physical evidence be to falsify a reported `idle`, and
    /// how old must that report be before evidence is allowed to falsify it?
    ///
    /// One number for both halves because it is one question: how long after a
    /// lane last spoke do we keep taking its word for it. Inside the window the
    /// report wins (it is the D1 exit — the harness reporting its own state
    /// beats any scrape of it, and this is where the report/repaint race
    /// lives); outside it, a pane that is demonstrably mid-turn wins.
    fn contradiction_window(&self) -> f64 {
        env_secs("AMUX_IDLE_CONTRADICTION_S", 60.0)
    }

    /// Is this lane's pane worth reading, and worth believing?
    ///
    /// ONE predicate, two callers — [`Self::capture_panes`] decides what to
    /// capture and [`Self::pane_of`] decides what to believe. If they ever
    /// drift, the derivation reads a pane the capture never took (or refuses
    /// one it did) and two readers of the same struct disagree about the same
    /// lane. Keeping it here also means a caller CANNOT make a parked lane's
    /// scrollback count as evidence by stuffing the map.
    pub fn pane_probe_candidate(&self, name: &str) -> bool {
        let act = self.activity.get(&format!("amux-{name}")).copied().unwrap_or(0) as f64;
        self.now - act < self.contradiction_window()
    }

    /// Raw pane for a lane whose evidence is admissible: recently painted and
    /// non-empty.
    ///
    /// An EMPTY capture is `None`, never "no markers, therefore idle". A herdr
    /// lane refuses a history read while it is working, so mid-turn its
    /// capture is empty BY DESIGN — reading that as idle would label a working
    /// lane idle, which is this whole bug in a different costume.
    fn pane_of(&self, name: &str) -> Option<&str> {
        if !self.pane_probe_candidate(name) {
            return None;
        }
        let raw = self.panes.get(name)?;
        (!raw.trim().is_empty()).then_some(raw.as_str())
    }

    /// Does the pane show UNAMBIGUOUS work — the evidence that may contradict
    /// a claim of idle?
    ///
    /// Composed from the two detectors that already exist rather than a third
    /// one: `pane_bar_says_generating` (the status bar's `esc to interrupt`,
    /// scoped to the bottom 3 lines by AMUX-2642 so a lane quoting the phrase
    /// cannot self-block) and `detect_claude_status` (the live spinner). Both
    /// answer "is the MAIN turn generating", which is the same question the
    /// steering gate asks — so a lane this reports as working is exactly a
    /// lane that would refuse a mid-turn delivery. A second spelling of the
    /// detector would be a second thing to keep in step with Claude Code's UI.
    /// Has a background agent of this lane written within the contradiction
    /// window? Reported `idle` describes the MAIN turn; this describes the lane.
    fn subagents_working(&self, name: &str) -> bool {
        // A SUBAGENT'S CADENCE IS NOT THE MAIN PANE'S. The main pane paints ~6/s,
        // so 60s of silence (contradiction_window) means it went stale. A
        // subagent transcript is touched ONLY when the subagent emits a message
        // or tool result — an xhigh-effort THINKING subagent can go minutes
        // between writes while very much working. Gating on the 60s window read
        // a lane whose subagent was "still thinking with xhigh effort" as IDLE
        // while it was visibly crunching (primis, 2026-08-13: header IDLE over a
        // "✻ Crunching… still thinking" pane with 2 live agents). Use a window
        // sized to the subagent write cadence, not the pane's. idle -> active is
        // one-way, so the worst case of a generous window is a bounded LATE
        // CORRECTION after the agents actually finish — never a false "busy" that
        // sticks, which is the property this whole derivation protects.
        let window = env_secs("AMUX_SUBAGENT_WORKING_S", 240.0);
        // AMUX-3048: an EVENT-DRIVEN live count, when the lane reports one, is the
        // durable answer the mtime window could not give. A subagent transcript's
        // mtime cannot tell "thinking, will write in 90s" from "finished 30s ago";
        // a start (PreToolUse:Task) / stop (SubagentStop) event pair can. A
        // positive reported count means a subagent is live RIGHT NOW even while
        // its transcript sits silent (the xhigh-thinking case, AMUX-3030), so it
        // flips the lane working where the mtime window read it idle.
        //
        // This is the SAFE half of the durable exit: it only ADDS a working
        // verdict (OR with the window), so it cannot regress a hookless lane —
        // gemini/codex send no such event, so there is no `subagents` key and the
        // verdict is pure mtime, unchanged. The count-AUTHORITATIVE "off"
        // direction (a count of 0 overriding a still-warm mtime — AMUX-3047's
        // up-to-4-minute false WORKING after a turn is done) is deliberately NOT
        // wired here yet: it needs a leak-safe reset that does not zero a live
        // run_in_background agent (which outlives the main turn, AMUX-2904).
        // Tracked as the follow-up on AMUX-3048.
        let reported_live = self.reported_subagent_count(name).is_some_and(|c| c > 0);
        reported_live
            || self
                .subagent_activity
                .get(name)
                .is_some_and(|m| self.now - m < window)
    }

    /// The raw event-driven live-subagent count a lane last reported (AMUX-3048),
    /// or `None` when the lane has never reported one (a hookless / mtime-only
    /// lane, e.g. gemini/codex). Exposed in the sessions payload so a LEAKED
    /// count — a lost SubagentStop pinning a lane "working" — is diagnosable
    /// rather than hidden. It is also the field the count-AUTHORITATIVE "off"
    /// direction follow-up (AMUX-3047) will read to override a warm mtime.
    fn reported_subagent_count(&self, name: &str) -> Option<i64> {
        self.reports
            .get(name)
            .and_then(|r| r.get("subagents"))
            .and_then(|s| s.get("count"))
            .and_then(serde_json::Value::as_i64)
    }

    fn pane_says_working(&self, name: &str) -> bool {
        let Some(raw) = self.pane_of(name) else {
            return false;
        };
        // THE BAR PHRASE ALONE NO LONGER PROVES A GENERATING MAIN TURN
        // (AMUX-2959, Ethan at 1am: "this worker says working" over an idle
        // prompt). Claude Code now shows "esc to interrupt" in the status bar
        // whenever BACKGROUND AGENTS exist — including with the main turn idle
        // at an empty composer. Three lanes read WORKING that way while their
        // agents sat "awaiting input" (transcripts quiet, agents_working:false
        // in the same payload — the response was disagreeing with itself).
        //
        // pane_bar_says_generating's own doc records the ambiguity and decides
        // fail-CLOSED, which is right for its other consumer (steer_decide:
        // reading ambiguous as busy defers a message; reading it as idle types
        // into a live turn). For DISPLAY the costs invert: this predicate
        // feeds the idle->active contradiction, whose contract is UNAMBIGUOUS
        // work — and an agents-hint bar over an idle composer is ambiguous by
        // the detector's own documentation. So here the bar phrase only counts
        // when the bar does NOT carry the agents hint; a generating main turn
        // with agents still flips via the spinner (detect == "active"), which
        // the streaming-essay counterexample in that doc block also shows.
        let bar_generating = crate::api::session_verbs::pane_bar_says_generating(raw);
        let bar_has_agents = {
            let clean = crate::backend::adapter::strip_ansi(raw);
            clean
                .lines()
                .filter(|l| !l.trim().is_empty())
                .rev()
                .take(3)
                .any(|l| l.contains("agents") || l.contains("agent ·"))
        };
        (bar_generating && !bar_has_agents)
            || crate::api::session_verbs::detect_claude_status(raw) == "active"
    }

    /// Capture the panes that could contradict a report.
    ///
    /// Only lanes that painted inside the contradiction window — typically a
    /// handful of a 60-lane fleet (measured: 4 of 63 on 2026-08-09). A lane
    /// that has not painted cannot be mid-turn: Claude Code repaints its
    /// spinner roughly six times a second.
    ///
    /// Behind a 2s cache, because the typical case is not the one that hurts.
    /// Measured on this box: 4 painting lanes cost 44ms, but a fleet-wide
    /// broadcast puts all 63 lanes in the candidate set and costs 473ms — and
    /// this runs on the board's `stale` computation, which the dashboard polls.
    /// A TTL two orders of magnitude below the contradiction window cannot
    /// change a verdict, and it makes the board and the session list read the
    /// SAME frame rather than two captures 20ms apart.
    pub fn capture_panes(&mut self) {
        let ttl = env_secs("AMUX_PANE_CACHE_TTL_S", 2.0);
        let cache = pane_cache();
        if let Ok(c) = cache.lock() {
            if self.now - c.0 < ttl {
                self.panes = c.1.clone();
                return;
            }
        }
        let names: Vec<String> = self
            .running
            .iter()
            .filter_map(|t| t.strip_prefix("amux-"))
            .filter(|n| self.pane_probe_candidate(n))
            .map(String::from)
            .collect();
        for chunk in names.chunks(12) {
            let handles: Vec<_> = chunk
                .iter()
                .map(|name| {
                    let n = name.clone();
                    std::thread::spawn(move || {
                        let pt = pane_target(&format!("amux-{n}"));
                        let out = std::process::Command::new("tmux")
                            .args(["capture-pane", "-t", &pt, "-p", "-e", "-S", "-30"])
                            .output()
                            .ok()?;
                        Some((n, String::from_utf8_lossy(&out.stdout).trim().to_string()))
                    })
                })
                .collect();
            for h in handles {
                if let Ok(Some((n, raw))) = h.join() {
                    self.panes.insert(n, raw);
                }
            }
        }
        // Store even an EMPTY result: "nothing was painting" is an answer, and
        // a cache that only remembers hits re-probes hardest exactly when the
        // fleet is quiet and there is nothing to find.
        if let Ok(mut c) = pane_cache().lock() {
            *c = (self.now, self.panes.clone());
        }
    }

    /// Lanes whose pane was actually read, with the evidence verdict for each.
    ///
    /// The consistency check (`invariants::checks::status_agrees_with_pane`)
    /// reads its two sides from here and from `derive_status` — one struct,
    /// one capture, one pair of detectors. A check that re-derives either side
    /// its own way is a second implementation that can drift from the thing it
    /// audits, and then its verdict is about itself.
    ///
    /// Excludes shell-only lanes: those have a tmux window and no worker, so
    /// "the card disagrees with the pane" is not a meaningful question for
    /// them.
    pub fn probed_lanes(&self) -> Vec<(String, bool)> {
        self.panes
            .keys()
            .filter(|n| self.agent_running(&format!("amux-{n}")))
            .filter(|n| self.pane_of(n).is_some())
            .map(|n| (n.clone(), self.pane_says_working(n)))
            .collect()
    }

    /// Python's status value for one session (see the derivation note above).
    pub fn derive_status(&self, name: &str, running: bool) -> String {
        if !running {
            return String::new();
        }
        let heartbeat = env_secs("AMUX_ACTIVE_HEARTBEAT_S", 120.0);
        let act = self
            .activity
            .get(&format!("amux-{name}"))
            .copied()
            .unwrap_or(0) as f64;
        let mut status: Option<String> = None;
        if let Some((st, ts)) = self.transitions.get(name) {
            // A transition from before the session's last (re)start describes
            // a previous life — Python never emits a transition out of the ""
            // state, so a restart leaves the old row behind (verified: the
            // guard flipped 1 live mismatch on 2026-08-09).
            if self.started.get(name).copied().unwrap_or(0.0) <= *ts {
                if st == "active" && self.now - act > heartbeat {
                    // An active session paints its pane continuously; silence
                    // past the heartbeat means the transition went stale.
                    status = Some("idle".into());
                } else {
                    status = Some(st.clone());
                }
            }
        }
        // No transition: prefer the PANE over the activity timestamp when the
        // pane is admissible. A timestamp says something painted; the pane
        // says what. `detect_claude_status` returning "" is the documented
        // Python residual (an agentless shell) and reads idle here, as it did
        // before — the fallback below stays for a lane with no readable pane,
        // which after `capture_panes` means a silent one (idle) or a herdr
        // lane mid-turn (empty capture, and `act` is fresh, so: active).
        let mut status = status.unwrap_or_else(|| {
            match self.pane_of(name).map(crate::api::session_verbs::detect_claude_status) {
                Some(v) if v == "active" || v == "waiting" => v,
                Some(_) => "idle".into(),
                None if self.now - act < 60.0 => "active".into(),
                None => "idle".into(),
            }
        });
        // self_report override — Python's exact gate (py:20248-20263).
        let mut idle_report_age: Option<f64> = None;
        if let Some(rep) = self.reports.get(name) {
            let st = rep["state"].as_str().unwrap_or("");
            // ts is time.time() — a FLOAT. as_i64() on it is None, which
            // silently read every report as epoch-0 (the age_s bug).
            let ts = rep["ts"].as_f64().unwrap_or(0.0);
            // A report from BEFORE the session's last (re)start describes a
            // PREVIOUS LIFE — the same guard the transition block above has
            // had all along, missing here. Found live 2026-08-11: board-exp-1
            // switched claude -> codex, its hours-old claude `idle` report
            // (24h trust window) outranked the codex trust picker the pane
            // was showing, and a lane blocked on input read idle. A restarted
            // claude lane loses nothing: its hooks re-report on the first
            // turn, and until then the pane and activity decide — which is
            // exactly right for the boot window.
            let from_this_life = self.started.get(name).copied().unwrap_or(0.0) <= ts;
            let age = self.now - ts;
            let stale_active = st == "active" && age > heartbeat;
            let live = age
                < if st == "idle" {
                    env_secs("AMUX_HOOKS_LIVE_IDLE_S", 86400.0)
                } else {
                    env_secs("AMUX_HOOKS_LIVE_S", 1800.0)
                };
            if from_this_life
                && !stale_active
                && live
                && matches!(st, "active" | "idle" | "waiting")
            {
                status = st.to_string();
                if st == "idle" {
                    idle_report_age = Some(age);
                }
            }
        }
        // CONTRADICTION (AMUX-2646). `idle` survives silence, never
        // contradiction. Fires only when BOTH halves hold: the claim is older
        // than the window (a fresh report is still the authority — D1), and
        // the pane both painted inside the window and shows the main turn
        // generating. It can only ever flip idle -> active, so a missed frame
        // costs a late correction, never a false "busy".
        if status == "idle"
            && idle_report_age.map(|a| a > self.contradiction_window()).unwrap_or(true)
            && self.pane_says_working(name)
        {
            status = "active".into();
        }
        // A PICKER CONTRADICTS IDLE TOO (AMUX-2952's status half). The rule
        // above only ever flips idle -> active on a GENERATING pane, so a lane
        // sitting at an input-required selector kept reading `idle` — measured
        // live on tubescience 2026-08-11: the pane showed AskUserQuestion's
        // "Ready to submit your answers?" while the header said IDLE, and
        // Ethan pressed Enter into a lane nothing had flagged as waiting.
        // `waiting` is the one state whose whole purpose is to summon a human;
        // mislabelling it idle is strictly worse than mislabelling work,
        // because nothing and nobody is coming.
        //
        // Same shape as the rule above on purpose: one-way (idle -> waiting),
        // gated on the same report-age window, evidence from the same
        // admissible pane. A missed frame costs a late correction, never a
        // false "waiting".
        if status == "idle"
            && idle_report_age.map(|a| a > self.contradiction_window()).unwrap_or(true)
        {
            if let Some(raw) = self.pane_of(name) {
                if crate::api::session_verbs::detect_claude_status(raw) == "waiting" {
                    status = "waiting".into();
                }
            }
        }
        // SUBAGENTS ARE THE LANE WORKING TOO (AMUX-2904) — but a FRESH idle
        // self-report outranks the subagent-mtime window, exactly as it does
        // over a working pane. This is now LITERALLY "the same one-way rule as
        // the pane contradiction above": same `idle_report_age >
        // contradiction_window` gate, same admissible evidence, still only ever
        // idle -> active. The gate was MISSING here while the comment above
        // claimed sameness (Ethan, 2026-08-13: "says working but it appears
        // done", over a "✻ Crunched for 1m 7s" idle prompt with a ~30s-old
        // stop-hook idle report and 2 background agents). A stopped main turn is
        // a stop-hook idle report, and a stopped main turn means its FOREGROUND
        // subagents have necessarily finished — so a fresh idle report is the
        // stronger signal and must win for the window, instead of the 240s
        // `AMUX_SUBAGENT_WORKING_S` mtime window pinning the header WORKING for
        // up to four minutes after the turn was done. AMUX-2904 is unchanged: a
        // main turn ACTIVE with foreground subagents has NOT stopped, so there
        // is no fresh idle report (`idle_report_age` is None -> `unwrap_or(true)`
        // -> the flip still fires), and once a real idle report ages past the
        // window a still-writing subagent flips it active as the bounded late
        // correction the window was always documented to cost.
        if status == "idle"
            && idle_report_age.map(|a| a > self.contradiction_window()).unwrap_or(true)
            && self.subagents_working(name)
        {
            status = "active".into();
        }
        status
    }
}


/// lane -> newest subagent-transcript mtime, in ONE pass over
/// `~/.claude/projects/<proj>/<conversation>/subagents/`.
///
/// The owning lane resolves through `session_verbs::conversation_owner` —
/// meta claim first, LAST title record second — the same resolution the
/// token-ledger indexer and the /subagents endpoint use, not a fourth
/// spelling of it. The first cut here read the parent's FIRST line, which
/// reintroduced the staleness AMUX-2612 fixed: the `amux` lane's conversation
/// is still titled 'amux-rust' on line 0, so its agents were attributed to a
/// lane that no longer exists and `amux` itself read as having none.
///
/// Only conversations with subagent activity in the last hour resolve an
/// owner — every consumer asks about minutes, and the title fallback is a
/// bounded tail read that is not worth paying for July's transcripts.
///
/// TTL-cached: FleetSignals::load runs per /api/sessions request AND the
/// stuck-composer sweep reads the same map; the underlying answer cannot
/// change faster than agents write, so 10s of staleness is free.
pub(crate) fn scan_subagent_activity() -> BTreeMap<String, f64> {
    use std::sync::{Mutex, OnceLock};
    type Cache = (f64, BTreeMap<String, f64>);
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    let now = chrono::Utc::now().timestamp() as f64;
    let cache = C.get_or_init(|| Mutex::new((0.0, BTreeMap::new())));
    if let Ok(g) = cache.lock() {
        if now - g.0 < 10.0 {
            return g.1.clone();
        }
    }
    let projects = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claude/projects");
    let mut out: BTreeMap<String, f64> = BTreeMap::new();
    let claims = crate::api::session_verbs::conversation_claims();
    let Ok(projs) = std::fs::read_dir(&projects) else { return out };
    for proj in projs.flatten() {
        let Ok(entries) = std::fs::read_dir(proj.path()) else { continue };
        for e in entries.flatten() {
            let conv = e.path();
            if conv.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let subs = conv.with_extension("").join("subagents");
            let Ok(files) = std::fs::read_dir(&subs) else { continue };
            let mut newest = 0.0f64;
            for f in files.flatten() {
                if !f.file_name().to_string_lossy().ends_with(".jsonl") {
                    continue;
                }
                if let Some(m) = f
                    .metadata()
                    .ok()
                    .and_then(|md| md.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                {
                    newest = newest.max(m.as_secs_f64());
                }
            }
            if newest <= 0.0 || now - newest > 3600.0 {
                continue;
            }
            let owner = crate::api::session_verbs::conversation_owner(&conv, &claims);
            if owner.is_empty() {
                continue;
            }
            let slot = out.entry(owner).or_insert(0.0);
            if newest > *slot {
                *slot = newest;
            }
        }
    }
    if let Ok(mut g) = cache.lock() {
        *g = (now, out.clone());
    }
    out
}

/// The set of sessions currently `active` — the board's `stale` flag reads
/// this (Python: `_session_prev_status[sess] == "active"`, py:15671-15697).
/// Shares `FleetSignals` with the session list: one derivation, two readers.
pub fn active_python_sessions(conn: &rusqlite::Connection) -> BTreeSet<String> {
    let mut signals = FleetSignals::load(conn);
    // The board must see the same evidence the session list sees, or a lane
    // reads active on one screen and idle on the other. Bounded: only lanes
    // that painted inside the contradiction window are probed.
    signals.capture_panes();
    let mut out = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(amux_home().join("sessions")) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let running = signals.agent_running(&format!("amux-{name}"));
        if signals.derive_status(name, running) == "active" {
            out.insert(name.to_string());
        }
    }
    out
}

// ---- preview (AMUX-2588) -------------------------------------------------

/// Python's strip_ansi (amux-server.py:20225) — ported verbatim, OSC
/// hyperlink forms included: Claude panes emit `\x1b]8;` constantly, and a
/// simpler regex leaves fragments the intelligibility filter then rejects.
pub(crate) fn strip_ansi(s: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            "\\x1b\\[[0-9;?]*[a-zA-Z]|\\x1b\\]8;[^\\x1b]*\\x1b\\\\|\\x1b\\][^\\x07]*\\x07|\\x1b\\][^\\x1b]*\\x1b\\\\|\\x1b[()][A-Z0-9]|\\x1b[\\x20-\\x2f]*[\\x40-\\x7e]",
        )
        .expect("strip_ansi regex")
    });
    re.replace_all(s, "").into_owned()
}

fn chars_truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python's preview pair (amux-server.py:20224-20316): the scalar is the
/// last non-blank RAW line, sliced to 120 chars THEN stripped (that order is
/// Python's); `preview_lines` is an ARRAY of up to 5 intelligible lines —
/// the SPA calls `.map()` on it (app.js:2602), so the previous line COUNT
/// failed its `&& s.preview_lines.length` check and previews silently never
/// rendered on the Rust side (AMUX-2588).
fn preview_of(raw: &str) -> (String, Vec<String>) {
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let preview = lines
        .iter()
        .rev()
        .map(|l| strip_ansi(&chars_truncate(l, 120)))
        .find(|cl| {
            let lower = cl.to_lowercase();
            let n = cl.chars().count();
            if n <= 2 { return false; }
            if cl.contains("\u{23f5}\u{23f5}")
                || lower.contains("bypass permissions")
                || lower.contains("plan mode")
                || cl.starts_with('\u{276f}')
            {
                return false;
            }
            let alnum = cl.chars().filter(|c| c.is_alphanumeric() || *c == ' ').count();
            n <= 3 || (alnum as f64) / (n as f64) >= 0.3
        })
        .unwrap_or_default();
    let mut intelligible: Vec<String> = Vec::new();
    for l in &lines {
        let cl = strip_ansi(l).trim().to_string();
        if cl.is_empty() {
            continue;
        }
        let lower = cl.to_lowercase();
        if cl.contains("⏵⏵") || lower.contains("bypass permissions") || lower.contains("plan mode")
        {
            continue;
        }
        let n_chars = cl.chars().count();
        let alnum = cl.chars().filter(|c| c.is_alphanumeric() || *c == ' ').count();
        if n_chars > 3 && (alnum as f64) / (n_chars as f64) < 0.3 {
            continue;
        }
        if n_chars <= 2 {
            continue;
        }
        let distinct: BTreeSet<char> = cl.chars().filter(|c| *c != ' ').collect();
        if distinct.len() <= 2 {
            continue;
        }
        intelligible.push(chars_truncate(&cl, 200));
    }
    let preview_lines: Vec<String> = if intelligible.is_empty() {
        // Fallback: last few non-empty stripped lines (spinner/tool output).
        let start = lines.len().saturating_sub(8);
        let cleaned: Vec<String> = lines[start..]
            .iter()
            .map(|l| chars_truncate(strip_ansi(l).trim(), 200))
            .filter(|l| !l.is_empty())
            .collect();
        let s = cleaned.len().saturating_sub(5);
        cleaned[s..].to_vec()
    } else {
        let s = intelligible.len().saturating_sub(5);
        intelligible[s..].to_vec()
    };
    (preview, preview_lines)
}

/// Saved-log tail for a STOPPED session (py:20218-20223): last 16KB of
/// ~/.amux/logs/<name>.log, last 30 lines.
fn stopped_session_raw(name: &str) -> String {
    let p = amux_home().join("logs").join(format!("{name}.log"));
    let Ok(mut f) = std::fs::File::open(&p) else {
        return String::new();
    };
    use std::io::{Read, Seek, SeekFrom};
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if size > 16_384 {
        let _ = f.seek(SeekFrom::Start(size - 16_384));
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(30);
    lines[start..].join("\n")
}

// ---- misc shared helpers -------------------------------------------------

use crate::config::amux_home;

/// ~/.amux/sessions/<name>.meta.json (py:_load_meta) — last_send,
/// last_started, task_summary live here.
///
/// Cached per build_array call via a process-global with a 2s TTL.
/// load_meta is called TWICE per session in build_array (once in
/// python_fleet_sessions, once in board linkage) — 226 filesystem reads
/// per request collapsed to ~113.
fn load_meta(name: &str) -> serde_json::Value {
    fn meta_cache() -> &'static std::sync::Mutex<(f64, BTreeMap<String, serde_json::Value>)> {
        static CACHE: std::sync::OnceLock<std::sync::Mutex<(f64, BTreeMap<String, serde_json::Value>)>> =
            std::sync::OnceLock::new();
        CACHE.get_or_init(|| std::sync::Mutex::new((0.0, BTreeMap::new())))
    }
    let now = chrono::Utc::now().timestamp() as f64;
    if let Ok(c) = meta_cache().lock() {
        if now - c.0 < 2.0 {
            if let Some(v) = c.1.get(name) {
                return v.clone();
            }
        }
    }
    let p = amux_home().join("sessions").join(format!("{name}.meta.json"));
    let val = std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    if let Ok(mut c) = meta_cache().lock() {
        if now - c.0 >= 2.0 {
            c.1.clear();
            c.0 = now;
        }
        c.1.insert(name.to_string(), val.clone());
    }
    val
}

/// Pick the worker's task label and its source from the freshness verdicts.
///
/// Pure so the precedence — and especially the SUMMARY freshness gate — can be
/// tested without a DB or meta files. The gate is the fix for the 2026-08-13
/// "these task names are out of date" report: `summary` is a point-in-time label
/// that nothing refreshes, so an ungated `!summary.is_empty()` let an unstamped
/// relic outrank both the live board card and the honest desc, permanently. Both
/// `board_fresh` and `summary_fresh` carry the SAME rule (`ts > 0 && age <= 24h`)
/// so the two time-sensitive sources age out identically; a stale summary falls
/// through to a stale board title, then to desc.
fn resolve_task_name(
    board_title: Option<&str>,
    board_fresh: bool,
    summary: &str,
    summary_fresh: bool,
    desc: &str,
) -> (String, &'static str) {
    if board_fresh {
        (board_title.unwrap_or_default().to_string(), "board")
    } else if summary_fresh {
        (summary.to_string(), "summary")
    } else if let Some(t) = board_title {
        (t.to_string(), "board")
    } else {
        (desc.to_string(), "desc")
    }
}

/// The legacy array as a JSON string, shared by the GET handler and the
/// SSE `sessions` pushes (one serializer, two transports).
///
/// Cached with a short TTL: the real work (tmux subprocesses, filesystem
/// reads, git calls) costs ~80-950ms and runs ~1/s from dashboard polling.
/// A 2s-stale response is invisible to a human and halves the subprocess
/// load.
pub fn legacy_sessions_array(store: &crate::db::SharedStore) -> anyhow::Result<String> {
    let ttl = env_secs("AMUX_SESSIONS_CACHE_TTL_S", 2.0);
    let now = chrono::Utc::now().timestamp() as f64;
    let epoch_now = SESSIONS_EPOCH.load(std::sync::atomic::Ordering::SeqCst);
    if let Ok(c) = build_array_cache().lock() {
        if now - c.stamp < ttl && !c.json.is_empty() && c.epoch == epoch_now {
            // Substrate guard (AMUX-2960): a fresh-looking snapshot whose
            // worker SET no longer matches the registry on disk means an
            // env file was created/deleted by a path that never called
            // invalidate_sessions_cache(). Rebuild — and say so, because
            // this line firing is how the next missing call site announces
            // itself instead of shipping another flaky-stale list.
            if c.registry == registry_fingerprint() {
                return Ok(c.json.clone());
            }
            tracing::info!(
                target: "amux::sessions",
                "sessions registry changed on disk without an API invalidation — rebuilding \
                 (a write path is missing invalidate_sessions_cache, or an out-of-band env-file write)"
            );
        }
    }
    // SINGLE-FLIGHT, STALE-WHILE-REVALIDATE (AR-135). The build holds a pooled
    // read connection across ~100 tmux + git subprocesses (80-950ms), and the
    // pool is only CPU-count deep. When the 2s TTL expired under a client
    // burst, EVERY concurrent request became a builder, each holding a
    // connection for the better part of a second — and the pool starved.
    // Measured 08-10 13:03-13:05: ten "timed out waiting for connection" 5xxs
    // across /api/sessions, /api/board/statuses, /api/board/session-gates and
    // /api/calendar.ics, real iPhone/macOS clients; two more 08-11 14:38. The
    // victims were endpoints that never shell out at all — they just could not
    // get a connection because five copies of THIS function held them.
    //
    // try_lock, never lock: this runs on the async executor, so blocking here
    // would trade pool starvation for executor starvation. Exactly one caller
    // rebuilds; everyone else gets the last snapshot, which for a 2s-TTL list
    // is at worst a couple of seconds staler than they hoped — the same
    // trade the cache itself already made.
    static FLIGHT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Ok(_flight) = FLIGHT.try_lock() else {
        if let Ok(c) = build_array_cache().lock() {
            // Losers may serve a somewhat-stale snapshot (that is the
            // stale-while-revalidate trade), but never one from before an
            // invalidation — post-invalidation the json is empty, so they
            // fall through and build.
            if !c.json.is_empty() && c.epoch == epoch_now {
                return Ok(c.json.clone());
            }
        }
        // Cold start with a builder already in flight: fall through and build
        // anyway — an empty answer would render an empty fleet as truth.
        return {
            let conn = store.read()?;
            let arr = build_array(&conn)?;
            let json = serde_json::to_string(&arr)?;
            Ok(json)
        };
    };
    // Double-check under the flight lock: the previous holder may have just
    // refreshed, and rebuilding immediately would waste its work.
    if let Ok(c) = build_array_cache().lock() {
        if now - c.stamp < ttl
            && !c.json.is_empty()
            && c.epoch == epoch_now
            && c.registry == registry_fingerprint()
        {
            return Ok(c.json.clone());
        }
    }
    // Snapshot both guards BEFORE the build: a create/delete racing the build
    // then fails the epoch check (API path) or the fingerprint check on the
    // next read (out-of-band path), instead of hiding inside the snapshot.
    let epoch_start = SESSIONS_EPOCH.load(std::sync::atomic::Ordering::SeqCst);
    let registry_start = registry_fingerprint();
    let conn = store.read()?;
    let arr = build_array(&conn)?;
    let json = serde_json::to_string(&arr)?;
    if SESSIONS_EPOCH.load(std::sync::atomic::Ordering::SeqCst) == epoch_start {
        if let Ok(mut c) = build_array_cache().lock() {
            *c = ListSnapshot {
                stamp: now,
                json: json.clone(),
                epoch: epoch_start,
                registry: registry_start,
            };
        }
    } else {
        // The write-back race, caught: this build predates an invalidation.
        // The caller still gets its (self-built, fresh-enough) answer; the
        // CACHE must not, or the invalidation is undone.
        tracing::debug!(
            target: "amux::sessions",
            "session-list build raced an invalidation — snapshot discarded, not cached"
        );
    }
    Ok(json)
}

pub async fn list_sessions_legacy(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    match legacy_sessions_array(&state.store) {
        Ok(json) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            filter_isolated_for_peer(&json, &headers),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Whether the caller is a PEER worker rather than the owner. A peer's request
/// carries a server-verified worker/session header (the `amux` CLI stamps it);
/// the owner's dashboard is a browser and sends neither. Same owner-vs-peer
/// split the send guard uses (empty origin = owner).
fn caller_is_peer(headers: &axum::http::HeaderMap) -> bool {
    ["x-amux-worker", "x-amux-session"].iter().any(|k| {
        headers
            .get(*k)
            .and_then(|v| v.to_str().ok())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    })
}

/// ISOLATED (AMUX-3232): strip isolated (raw-agent) workers from the fleet list
/// a PEER sees, so they are undiscoverable to other sessions. The OWNER
/// dashboard sees the full fleet (it must still control them), so the shared
/// cache stays owner-complete and the peer view is applied per request here.
/// Greppable trace on a real strip so the exclusion is not silent.
fn filter_isolated_for_peer(json: &str, headers: &axum::http::HeaderMap) -> String {
    if !caller_is_peer(headers) {
        return json.to_string();
    }
    let Ok(mut arr) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return json.to_string();
    };
    let before = arr.len();
    arr.retain(|s| !s.get("isolated").and_then(serde_json::Value::as_bool).unwrap_or(false));
    let hidden = before - arr.len();
    if hidden > 0 {
        tracing::debug!(hidden, "sessions list: isolated worker(s) hidden from a peer caller");
    }
    serde_json::to_string(&arr).unwrap_or_else(|_| json.to_string())
}

// ---- POST /api/sessions — CREATE a fleet worker --------------------------
//
// The cutover carried GET across and left POST behind, so the dashboard's
// "New worker" dialog has been 405ing: the toast said "Create failed: error
// 405" and the dialog stayed open, which reads as the Create button doing
// nothing. `POST /api/workers` is NOT the same thing — it inserts a row in
// the `workers` table, a different substrate from the ~/.amux/sessions/*.env
// registry this list (and tmux, and every session verb) reads, so a worker
// created there is invisible to the fleet.
//
// A fleet worker IS its env file. This writes exactly the file the Python
// server wrote (`# updated:` header, K="V", 0600 atomic) and nothing else —
// `/start` does the rest, as it already does for a duplicated session.

/// Python's sanitizer, same as `duplicate`'s: anything outside
/// `[A-Za-z0-9_-]` becomes `-`, so a name can never escape the sessions dir.
fn sanitize_session_name(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect()
}

/// `# updated:` header + K="V" lines, 0600, atomic rename — byte-compatible
/// with `EnvFile::write` in session_verbs (which is private to that module;
/// duplicating ~15 lines here is cheaper than widening a file another lane is
/// actively editing).
fn write_env_file(path: &std::path::Path, pairs: &[(&str, String)]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = format!(
        "# updated: {}\n",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.6f")
    );
    for (k, v) in pairs {
        out.push_str(&format!("{k}=\"{v}\"\n"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("env"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(out.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Provider-shaped model -> env wiring for a newly created worker. Pure and
/// unit-tested (ethos rule 7) so the rule cannot be re-derived subtly wrong at
/// the call site, and matches env_config::render_worker_env so the create route
/// and the env-apply route agree about one worker's model (ethos rule 4).
///
/// Returns `(cc_flags, cc_model, resolved_model)`:
/// - Agent CLIs (claude/codex/gemini): the model rides in `cc_flags` as
///   `--model X`; `cc_model` is empty. An empty caller model falls back to
///   `default_model`.
/// - Ollama: the model rides in `cc_model` (the ollama start arm reads CC_MODEL
///   and never appends CC_FLAGS); `cc_flags` stays empty unless the caller sent
///   explicit `flags`. `default_model` is NEVER applied — a local-model worker
///   must not inherit the Claude default (AMUX-3182); an empty model lets the
///   start path use the ollama default (qwen3.8:27b).
///
/// Explicit `flags` always win (AMUX-3114): honoured verbatim as `cc_flags`.
/// `resolved_model` is what the create response echoes so a defaulted/unpinned
/// model is visible at create time.
pub(crate) fn worker_model_env(
    provider: &str,
    raw_model: &str,
    explicit_flags: &str,
    default_model: &str,
) -> (String, String, String) {
    let is_ollama = provider == "ollama";
    let model = if is_ollama {
        raw_model.to_string()
    } else if raw_model.is_empty() {
        default_model.to_string()
    } else {
        raw_model.to_string()
    };
    let cc_flags = if !explicit_flags.is_empty() {
        explicit_flags.to_string()
    } else if !is_ollama && !model.is_empty() {
        format!("--model {model}")
    } else {
        String::new()
    };
    let cc_model = if is_ollama { model.clone() } else { String::new() };
    let resolved_model = if is_ollama {
        model
    } else {
        cc_flags
            .split_whitespace()
            .skip_while(|t| *t != "--model")
            .nth(1)
            .unwrap_or("")
            .to_string()
    };
    (cc_flags, cc_model, resolved_model)
}

pub async fn create_session_legacy(
    State(_state): State<AppState>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null);
    let s = |k: &str| {
        body.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let raw_name = s("name");
    if raw_name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name is required"})),
        )
            .into_response();
    }
    let name = sanitize_session_name(&raw_name);
    if name.is_empty() || name.starts_with('.') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("'{raw_name}' is not a usable worker name")})),
        )
            .into_response();
    }
    // A worktree create needs `git worktree add` + branch bookkeeping that
    // does not exist here yet. REFUSE loudly rather than create a plain
    // worker and let the user believe they got an isolated checkout — a
    // silently-ignored option is the failure mode this whole sweep is about.
    if body.get("worktree").and_then(serde_json::Value::as_bool) == Some(true) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "worktree creation is not implemented on this server yet — \
                          uncheck 'Use worktree' to create a normal worker"
            })),
        )
            .into_response();
    }
    let path = amux_home().join("sessions").join(format!("{name}.env"));
    if path.exists() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": format!("session '{name}' already exists")})),
        )
            .into_response();
    }
    let dir = s("dir");
    if !dir.is_empty() && !std::path::Path::new(&dir).is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("working directory '{dir}' does not exist")})),
        )
            .into_response();
    }
    let provider = {
        let p = s("provider");
        if p.is_empty() { "claude".to_string() } else { p }
    };
    // Provider-shaped model -> env wiring, factored into worker_model_env so the
    // rule is unit-tested (ethos rule 7) instead of re-derived here, and cannot
    // silently drift from the ollama start path that reads it. In short: agent
    // CLIs pin `--model` in CC_FLAGS; ollama's model is CC_MODEL (the start arm
    // reads CC_MODEL and never appends CC_FLAGS); the Claude default model never
    // touches a local-model worker. Before this, an ollama worker created here
    // got CC_FLAGS="--model <claude-default>" and no CC_MODEL, so it silently ran
    // qwen3.8:27b while its row displayed a Claude model and any non-default
    // local model the caller picked was dropped (AMUX-3182).
    let explicit_flags = s("flags");
    let default_model = crate::api::settings::get_default_model(&amux_home());
    let (cc_flags, cc_model, resolved_model) = worker_model_env(
        &provider,
        s("model").trim(),
        explicit_flags.trim(),
        &default_model,
    );
    let mut pairs: Vec<(&str, String)> = vec![("CC_DIR", dir.clone())];
    let creator = s("creator");
    if !creator.is_empty() {
        pairs.push(("CC_CREATOR", creator));
    }
    if provider != "claude" {
        pairs.push(("CC_PROVIDER", provider.clone()));
    }
    if !cc_model.is_empty() {
        pairs.push(("CC_MODEL", cc_model.clone()));
    }
    if !cc_flags.is_empty() {
        pairs.push(("CC_FLAGS", cc_flags.clone()));
    }
    // ACCEPT tags AS AN ARRAY, which is what the dashboard and API send
    // (AMUX-3114). `s("tags")` only matched a STRING, so `{"tags":["gtm"]}` read
    // "" and the worker was created with NO groups, the same silent drop the
    // PATCH handler was already fixed for.
    let tags = match body.get("tags") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|x| x.as_str().map(str::trim))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(","),
        _ => s("tags"),
    };
    if !tags.is_empty() {
        pairs.push(("CC_TAGS", tags.clone()));
    }
    let desc = s("desc");
    if !desc.is_empty() {
        pairs.push(("CC_DESC", desc));
    }
    if let Err(e) = write_env_file(&path, &pairs) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("could not write session env: {e}")})),
        )
            .into_response();
    }
    // The worker now exists on disk; the cached list must not outlive that
    // fact (AMUX-2960). Without this the creator's own next fetch — the
    // dashboard reloading after its Create dialog, the worker-card-counts
    // e2e reloading after seeding — served the PRE-create fleet for up to
    // TTL, and SSE never corrected it (this handler emits no revision
    // event, a residual noted on the card).
    invalidate_sessions_cache();
    (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "name": name,
            "dir": dir,
            "provider": provider,
            "running": false,
            "archived": false,
            // Echo what was actually stored so a dropped or defaulted field is
            // visible in the create response, not only via a later GET
            // (AMUX-3114): flags is the effective CC_FLAGS, model the resolved
            // model ("" = unpinned / ambient default), tags the stored groups.
            "flags": cc_flags,
            "model": resolved_model,
            "tags": tags,
        })),
    )
        .into_response()
}

/// The PYTHON fleet's sessions, from the same sources the Python server
/// reads: ~/.amux/sessions/*.env registry + live tmux state. Read-only —
/// the Rust server OBSERVES the Python fleet during coexistence; managing
/// it stays Python's job until cutover. Without this the dashboard on the
/// Rust port says "no workers yet" while 60+ real sessions run (Ethan's
/// first verification finding).
/// Sessions quarantined via blocked-sessions.txt — the Python "archived"
/// flag's source of truth (CC_BLOCKED_SESSIONS, amux-server.py:65).
fn blocked_names(home: &std::path::Path) -> std::collections::BTreeSet<String> {
    std::fs::read_to_string(home.join("blocked-sessions.txt"))
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Test-only fleet suppression. The handler reads `amux_home()` + live tmux at
/// CALL time, so a unit test on a temp DB still merges the machine's real
/// fleet — `legacy_sessions_route_serves_workers…` failed with 117 rows on a
/// box running 116 sessions, and broke every full-suite run (2026-08-09, two
/// lanes hit it). Named deviation: the root fix is capturing home in AppState
/// at startup instead of re-reading env per request (carded); until then this
/// is the only race-free way to keep the unit test's verdict machine-independent.
#[cfg(test)]
pub(crate) static SUPPRESS_FLEET_FOR_TEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn python_fleet_sessions(signals: &FleetSignals) -> Vec<serde_json::Value> {
    #[cfg(test)]
    if SUPPRESS_FLEET_FOR_TEST.load(std::sync::atomic::Ordering::Relaxed) {
        return vec![];
    }
    let home = amux_home();
    let sessions_dir = home.join("sessions");
    let blocked = blocked_names(&home);
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return vec![];
    };
    let mut out = vec![];
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let env = crate::config::parse_env_file(&path);
        let tmux = format!("amux-{name}");
        let is_running = signals.agent_running(&tmux);
        // CC_ARCHIVED=1 is Python's session-archive marker (amux-server.py
        // :20346) — blocked-sessions.txt is QUARANTINE, a different thing;
        // conflating them reported 0 archived against a fleet with dozens.
        let archived = env.get("CC_ARCHIVED").map(|v| v == "1").unwrap_or(false)
            || blocked.contains(&name);
        let flags = env.get("CC_FLAGS").cloned().unwrap_or_default();
        let backend = env
            .get("CC_BACKEND")
            .map(|b| b.trim().to_lowercase())
            .filter(|b| b == "herdr")
            .unwrap_or_else(|| "tmux".into());
        // Python's session_created is the TMUX session's creation time
        // (tinfo["created"], 0 when not running) — not the env file's mtime.
        let session_created = signals.created.get(&tmux).copied().unwrap_or(0);
        // Python's last_activity is meta.last_send falling back to
        // meta.last_started (py:20207-20211) — DELIBERATELY not tmux
        // activity, which updates every snapshot tick and made every lane
        // look equally busy.
        let meta = load_meta(&name);
        let last_activity = {
            let send = meta["last_send"].as_i64().unwrap_or(0);
            if send != 0 { send } else { meta["last_started"].as_i64().unwrap_or(0) }
        };
        let mut status = signals.derive_status(&name, is_running);
        // A lane parked on a real picker is WAITING, never idle (AMUX-2834). The
        // derivation above cannot see a picker — it reads self-reports and tmux
        // activity, and a lane at a prompt is producing neither. The sweep in
        // session_verbs stamps this after a pane capture; read it here rather
        // than capturing again, which would be ~113 tmux calls per request.
        // Only overrides a NON-active status: if the lane is genuinely
        // generating, that is the more urgent truth and the picker reading is
        // stale by definition.
        if is_running && status != "active" && meta["input_required_since"].as_i64().unwrap_or(0) > 0
        {
            status = "waiting".to_string();
        }
        // STUCK COMPOSER (AMUX-2904): genuinely TYPED text sits under `❯`
        // with no live turn and no live agents — an Enter that never landed,
        // or a human's committed-but-unsubmitted command. Same shape as the
        // picker above: blocked on a human, the opposite of idle. The sweep
        // stamps it (through composer_state's dim-vs-typed discrimination —
        // NOT a stripped read of the ❯ line, which is the 2026-08-09
        // 13-lane false positive); here it becomes the state the fleet list
        // shows. Ghost-rescue auto-submits the amux-prefixed subset; this
        // surfaces the rest instead of deciding for a human.
        if is_running && status != "active" && meta["composer_stuck_since"].as_i64().unwrap_or(0) > 0
        {
            status = "waiting".to_string();
        }
        out.push(json!({
            "archived": archived,
            // Why a `waiting` lane is waiting, and proof a lane is genuinely
            // busy: the dashboard renders both — a status with no visible
            // reason is a status nobody can act on (ethos rule 4).
            "composer_stuck_since": meta["composer_stuck_since"].as_i64().unwrap_or(0),
            "composer_preview": meta["composer_preview"].as_str().unwrap_or(""),
            "agents_working": signals.subagents_working(&name),
            // AMUX-3048: the raw event-driven count behind agents_working, so a
            // LEAKED count (a lost SubagentStop pinning a lane "working") is
            // diagnosable rather than hidden — null on a hookless/mtime-only lane.
            "subagents_live": signals.reported_subagent_count(&name),
            // The lightning button's state derives from THIS field in the
            // SPA (isYolo checks flags for the provider's skip-permissions
            // flag) — a card without flags renders the wrong YOLO badge
            // (Ethan: "the lightning button isn't correct").
            "flags": flags,
            // The YOLO badge's source of truth, computed by the SAME function the
            // toggle acts on (`session_verbs::yolo_enabled`). The SPA previously
            // derived this itself as `flags.includes(...) || !!s.auto_continue`,
            // but `auto_continue` below is `standing_orders_on`, which is
            // DEFAULT-ON — so lanes with no skip-permissions flag rendered a YOLO
            // badge and users trusted a worker not to stop for approval when it
            // would. Ship the verdict, not the ingredients.
            "yolo": crate::api::session_verbs::yolo_enabled(
                &flags,
                env.get("CC_AUTO_CONTINUE").map(|v| v.as_str()),
            ),
            "creator": env.get("CC_CREATOR").cloned().unwrap_or_default(),
            "backend": backend,
            // Same predicate as board_drive's nudge gate — the view must not
            // disagree with the mechanism it describes. That means the SCOPED
            // one (AMUX-2930): reading the worker env alone reported
            // auto_continue=true for a lane whose group or global env had
            // turned standing orders off, so the card said "on" while the
            // nudger said "off". `standing_orders` is the master switch's own
            // state, exposed so the SPA can show WHY a lane is quiet without
            // re-deriving the layering.
            "auto_continue": crate::api::session_verbs::standing_orders_on(&name, "CC_AUTO_CONTINUE"),
            "auto_pickup": crate::api::session_verbs::standing_orders_on(&name, "CC_AUTO_PICKUP"),
            "standing_orders": crate::api::session_verbs::standing_orders_on(&name, "CC_STANDING_ORDERS"),
            "worktree": env.get("CC_WORKTREE").cloned().unwrap_or_default(),
            "worktree_repo": env.get("CC_WORKTREE_REPO").cloned().unwrap_or_default(),
            "mcp": env.get("CC_MCP").cloned().unwrap_or_default(),
            "session_created": session_created,
            "last_activity": last_activity,
            // Scanner-internal state the Python server holds in memory with
            // no durable trace (rate/credit limits, API errors, the model
            // detector) stays a correct-TYPED honest empty (Invariant 20:
            // never invent). `status` is no longer in that set — it derives
            // above from stores the Python scanner itself persists.
            "active_model": "",
            "api_error": false,
            "api_error_code": "",
            "api_error_count": 0,
            // COMPUTED, NOT HARDCODED (AMUX-2820). These were literal `false`
            // and `0`, with a comment calling them "a correct-TYPED honest
            // empty (Invariant 20: never invent)". That was right at cutover
            // and became a lie by omission the moment nothing filled them:
            // `false` and "not computed" are byte-identical over JSON, so every
            // consumer read a lane parked on Claude Code's rate-limit menu as
            // HEALTHY. mvs-infra sat there with two of Ethan's messages queued
            // behind it and /api/sessions reported status=idle,
            // credit_limited=false the whole time. Nothing downstream — not the
            // log sweep, not autofix, not the invariants monitor — could see a
            // condition its own field says is absent (ethos rule 4).
            //
            // The writer is the rate-limit detector in session_verbs, which
            // stamps meta when it sees the menu and clears it when it answers.
            // Read from meta because THIS LOOP ALREADY LOADS IT — computing it
            // here from a pane capture would cost ~113 tmux calls per request.
            "credit_limited": meta["rate_limited_since"].as_i64().unwrap_or(0) > 0,
            "credit_limit_model": meta["rate_limited_model"].as_str().unwrap_or(""),
            "credit_limited_since": meta["rate_limited_since"].as_i64().unwrap_or(0),
            "rate_limit_banner": meta["rate_limited_since"].as_i64().unwrap_or(0) > 0,
            "rate_limit_weekly": meta["rate_limited_weekly"].as_bool().unwrap_or(false),
            "rate_limited_until": meta["rate_limited_until"].as_i64().unwrap_or(0),
            "last_human_ts": 0,
            "waiting_since": 0,
            "self_report": serde_json::Value::Null,
            // Filled from the shared steering_queue table in build_array —
            // Python's card shape (py:20373), entries {id,text,queued_at,guard}.
            "steering": [],
            "tokens": {"input": 0, "output": 0, "total": 0},
            "preview_lines": [],
            "task_source": "",
            "task_time": 0,
            "task_updated": 0,
            "task_board_id": "",
            "task_board_age": 0,
            "sched_on": 0,
            "sched_off": 0,
            "name": name,
            // Rate-limit IS a status (Ethan, 2026-08-16: "capture workers rate
            // limit as a status"). A lane blocked on a provider usage limit must be
            // distinguishable in the fleet view, not hidden behind idle/waiting with
            // only the side boolean `credit_limited` two lines down. Derives from the
            // same meta stamp the rate_limit_sweep now keeps set for the whole
            // blocked window (menu OR post-menu banner).
            "status": if meta["rate_limited_since"].as_i64().unwrap_or(0) > 0 {
                json!("rate_limited")
            } else {
                json!(status.clone())
            },
            "running": is_running,
            "provider": env.get("CC_PROVIDER").cloned().unwrap_or_else(|| "claude".into()),
            "model": env.get("CC_MODEL").cloned().unwrap_or_default(),
            "dir": env.get("CC_DIR").cloned().unwrap_or_default(),
            "preview": "",
            "task_name": "",
            "desc": env.get("CC_DESC").cloned().unwrap_or_default(),
            // TRIMMED, matching Python's t.strip(): CC_TAGS="mvs, gtm"
            // otherwise yields " gtm" beside "gtm" — TWO gtm groups in the
            // UI (Ethan's finding).
            "tags": env.get("CC_TAGS").map(|t| t.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>()).unwrap_or_default(),
            "pinned": env.get("CC_PINNED").map(|v| v == "1").unwrap_or(false),
            // ISOLATED (AMUX-3232): a raw agent (tmux + the CLI, no amux
            // harness). The OWNER dashboard reads this to show the toggle state /
            // badge; the peer-facing list strips these entries entirely in
            // list_sessions_legacy (a peer must not even see it exists).
            "isolated": env.get("CC_ISOLATED").map(|v| v == "1").unwrap_or(false),
            "steering_queue": [],
            "managed_by": "python",
        }));
    }
    out
}

/// pub(crate): session_verbs' bare GET /api/sessions/{name} serves ONE
/// record from the SAME array (py:74892 — the natural URL answers the
/// natural shape).
pub(crate) fn build_array(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<serde_json::Value>> {
    let mut signals = FleetSignals::load(conn);
    // Before any status is derived: the pane is the only signal that can
    // contradict a self-report, and a report that nothing can contradict is
    // what shipped a working lane as `idle` for 1076s (AMUX-2646).
    signals.capture_panes();
    let signals = signals;
    let mut stmt = conn.prepare(
        "SELECT w.display_name, w.state, w.provider, w.model, w.cwd,
                (SELECT COUNT(*) FROM _amux_sessions s
                 WHERE s.worker_id = w.id AND s.ended_at IS NULL) AS live
         FROM _amux_workers w
         WHERE json_extract(w.state, '$.deleted_at') IS NULL
         ORDER BY w.display_name",
    )?;
    let rows = stmt.query_map([], |r| {
        let name: String = r.get(0)?;
        let state_json: String = r.get(1)?;
        let provider: String = r.get(2)?;
        let model: Option<String> = r.get(3)?;
        let cwd: String = r.get(4)?;
        let live: i64 = r.get(5)?;
        Ok(json!({
            // The Python list's load-bearing fields; ones the Rust side
            // cannot honestly fill yet are present-and-empty, NOT omitted —
            // the SPA indexes into them.
            "name": name,
            "status": python_status(&state_json),
            "running": live > 0,
            "provider": provider,
            "model": model.unwrap_or_default(),
            "dir": cwd,
            "preview": "",
            "preview_lines": [],
            "task_name": "",
            "task_source": "",
            "task_board_id": "",
            "task_updated": 0,
            "task_board_age": 0,
            "last_activity": 0,
            "pinned": false,
            "desc": "",
            "tags": [],
            "steering_queue": [],
        }))
    })?;
    let mut out: Vec<serde_json::Value> = rows.collect::<Result<_, _>>()?;
    // The Python fleet rides alongside Rust-managed workers, deduped by
    // name (a name registered in BOTH belongs to the Rust row — it carries
    // real state).
    let rust_names: std::collections::BTreeSet<String> = out
        .iter()
        .filter_map(|v| v["name"].as_str().map(|s| s.to_lowercase()))
        .collect();
    for s in python_fleet_sessions(&signals) {
        if let Some(n) = s["name"].as_str() {
            if !rust_names.contains(&n.to_lowercase()) {
                out.push(s);
            }
        }
    }
    // Board linkage per card, Python's exact query + precedence
    // (py:20187-20197, 20348-20365): ORDER BY updated ASC with dict
    // overwrite so the NEWEST-touched doing card wins (the 2026-07-22
    // wrong-task bug), then board-if-fresh(24h) -> meta task_summary ->
    // stale board title -> CC_DESC.
    {
        let mut stmt = conn.prepare(
            "SELECT session, id, title, COALESCE(updated, 0) FROM issues
             WHERE status = 'doing' AND deleted IS NULL AND session IS NOT NULL
             ORDER BY updated ASC",
        )?;
        let mut doing: BTreeMap<String, (String, String, i64)> = BTreeMap::new();
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })? {
            let (sess, id, title, updated) = row?;
            doing.insert(sess, (id, title, updated));
        }
        let now = signals.now as i64;
        for v in out.iter_mut() {
            let Some(name) = v["name"].as_str().map(String::from) else {
                continue;
            };
            let board = doing.get(&name);
            let board_updated = board.map(|(_, _, u)| *u).unwrap_or(0);
            let board_fresh = board.is_some() && now - board_updated <= 86400;
            let meta = load_meta(&name);
            let summary = meta["task_summary"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let summary_ts = meta["task_summary_ts"].as_i64().unwrap_or(0);
            // GATE THE SUMMARY BY FRESHNESS, exactly as `board_fresh` above gates
            // the board card (Ethan, 2026-08-13: "these task names are out of
            // date"). A summary is a POINT-IN-TIME label that nothing refreshes,
            // so an ungated `!summary.is_empty()` meant a frozen relic
            // ("Luke's Wilderness Tales" on the Obsidian lane) outranked the live
            // board card AND the honest desc, forever. Two things made these
            // relics: they are unstamped (`task_summary_ts == 0`, written before
            // AMUX-2676 added the stamp) and there is no automated writer that
            // re-stamps them. `ts > 0 && age <= 24h` is the SAME rule the board
            // uses, so a summary now ages out the way a doing card does; an
            // unstamped one is treated as unknown-age and skipped. A stale
            // summary falls through to the stale board title, then to desc — the
            // worker's role, which is honest rather than a wrong task claim.
            let summary_fresh = !summary.is_empty() && summary_ts > 0 && now - summary_ts <= 86400;
            let desc = v["desc"].as_str().unwrap_or("").to_string();
            let (tname, tsrc) = resolve_task_name(
                board.map(|(_, t, _)| t.as_str()),
                board_fresh,
                &summary,
                summary_fresh,
                &desc,
            );
            v["task_name"] = json!(tname);
            v["task_source"] = json!(tsrc);
            v["task_board_id"] =
                json!(if tsrc == "board" { board.map(|(i, _, _)| i.clone()).unwrap_or_default() } else { String::new() });
            // A summary-sourced task now carries its own stamp (AMUX-2676);
            // it is 0 only for tasks written before that existed, and 0 still
            // means "unknown" rather than "just now" — the client must not
            // render an age it does not have.
            v["task_updated"] = json!(match tsrc {
                "board" => board_updated,
                "summary" => meta["task_summary_ts"].as_i64().unwrap_or(0),
                _ => 0,
            });
            v["task_board_age"] = json!(
                if board.is_some() && board_updated != 0 && !board_fresh {
                    (now - board_updated).max(0)
                } else {
                    0
                }
            );
        }
    }

    // Schedule counts per session — Python's exact aggregation
    // (amux-server.py:20179).
    {
        let mut stmt = conn.prepare(
            "SELECT session, SUM(CASE WHEN enabled=1 THEN 1 ELSE 0 END) o,
                    SUM(CASE WHEN enabled=1 THEN 0 ELSE 1 END) f
             FROM schedules
             WHERE deleted IS NULL AND session IS NOT NULL AND session != ''
             GROUP BY session",
        )?;
        let sched: std::collections::BTreeMap<String, (i64, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?
            .flatten()
            .collect();
        for v in out.iter_mut() {
            if let Some(name) = v["name"].as_str() {
                if let Some((on, off)) = sched.get(name) {
                    v["sched_on"] = json!(on);
                    v["sched_off"] = json!(off);
                }
            }
        }
    }

    // steering: Python's card carries the session's queued steering entries
    // (py:20373, `_steering_queue.get(name, [])`) — and that queue is
    // persisted in the shared steering_queue TABLE (INSERT on enqueue,
    // DELETE on delivery, py:8632/8796), so the durable store IS the
    // in-memory queue's mirror. Entry shape matches Python's hydrate
    // (py:11873): {id, text, queued_at, guard} with guard "" for NULL.
    {
        let mut steering: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, session, text, queued_at, COALESCE(guard,'') \
             FROM steering_queue ORDER BY queued_at ASC",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            }) {
                for (id, session, text, queued_at, guard) in rows.flatten() {
                    // `system`: amux's own push (board-drive, sched:…), not a
                    // human's queued message — the SPA separates the surfaces
                    // and Clear-all spares these (AMUX-2922).
                    let system =
                        crate::api::session_verbs::steer_guard_is_system(&guard);
                    steering.entry(session).or_default().push(json!({
                        "id": id, "text": text, "queued_at": queued_at, "guard": guard,
                        "system": system,
                    }));
                }
            }
        }
        for v in out.iter_mut() {
            if let Some(name) = v["name"].as_str() {
                if let Some(q) = steering.get(name) {
                    v["steering"] = json!(q);
                }
            }
        }
    }

    // self_report from the SHARED persisted store (prefs key
    // 'session_reports', amux-server.py:3943) — the same bytes Python
    // hydrates at boot, not its memory. state/ts/source -> Python's
    // {state, age_s, source} card shape (py:20429).
    if signals.reports.is_object() {
        for v in out.iter_mut() {
            if let Some(name) = v["name"].as_str() {
                if let Some(rep) = signals.reports.get(name) {
                    // ts is time.time() — a FLOAT; as_i64() read it as 0 and
                    // age_s came out as the whole epoch (found 2026-08-09).
                    let ts = rep["ts"].as_f64().unwrap_or(0.0);
                    v["self_report"] = json!({
                        "state": rep["state"].as_str().unwrap_or(""),
                        "age_s": ((signals.now - ts).max(0.0)) as i64,
                        "source": rep["source"].as_str().unwrap_or(""),
                    });
                    // AMUX-2676: a REPORTED model/token count replaces the
                    // honest-empty above. Still never invented — the empty
                    // stays empty unless the harness itself said otherwise,
                    // which is the whole point of preferring the report
                    // endpoint over a scraper.
                    if let Some(m) = rep["model"].as_str().filter(|m| !m.is_empty()) {
                        v["active_model"] = json!(m);
                    }
                    // Same over-window rejection the compaction path applies
                    // (a5b272e). Without it the two disagree about one fact:
                    // /api/sessions rendered this session at 3,156,510 tokens
                    // while the trigger rejected the identical number as not a
                    // context size. A dashboard showing an impossible value is
                    // how the number stops being questioned — and it is the
                    // only lane on the fleet where the two paths could differ,
                    // so the disagreement would have stayed invisible.
                    let plausible = rep["tokens"]["total"]
                        .as_u64()
                        .map(|t| t <= crate::api::session_verbs::context_window())
                        .unwrap_or(false);
                    if rep["tokens"].is_object() && plausible {
                        v["tokens"] = rep["tokens"].clone();
                    }
                }
            }
        }
    }

    // FALLBACK TO THE TRANSCRIPT when the report carried neither field.
    //
    // The report path above is correct and stays PREFERRED — this only fills a
    // gap it cannot currently reach. Measured 2026-08-11: 42 lanes reporting, 2
    // with a model, 1 with tokens, because Claude Code loads hook config at
    // SESSION START and every running lane predates the settings change that
    // repointed the hook at the extracting script. All 292 sampled report POSTs
    // carried the predecessor's byte-exact 37/39/41-byte body. No edit on disk
    // reaches a command string already baked into a running process.
    //
    // Without tokens, orchestrator/compaction.rs is never called, so no lane
    // ever auto-compacts — which is the thing Ethan asked for in as many words.
    // A capability that exists and reaches nobody is the failure ethos rule 1
    // names, and waiting for ~47 lanes to restart is not a fix.
    //
    // Never invents: absent transcript, unreadable file, or records carrying
    // neither field all leave the honest empty in place.
    for v in out.iter_mut() {
        let Some(name) = v["name"].as_str().map(str::to_string) else { continue };
        let need_model = v["active_model"].as_str().unwrap_or("").is_empty();
        let need_tokens = v["tokens"]["total"].as_u64().unwrap_or(0) == 0;
        if !need_model && !need_tokens {
            continue;
        }
        let (m, t) = crate::api::session_verbs::transcript_evidence(&name);
        if need_model {
            if let Some(m) = m {
                v["active_model"] = json!(m);
                v["model_source"] = json!("transcript");
            }
        }
        if need_tokens {
            if let Some(t) = t {
                v["tokens"] = json!({"input": t, "output": 0, "total": t});
                v["tokens_source"] = json!("transcript");
            }
        }
    }

    // branch: bounded parallel git lookups, deduped by directory (many
    // sessions share a checkout — one git call per DISTINCT dir).
    // Cached with a 30s TTL: branches change on the scale of minutes.
    {
        let git_ttl = env_secs("AMUX_GIT_BRANCH_CACHE_TTL_S", 30.0);
        let mut branches: std::collections::BTreeMap<String, String> =
            if let Ok(c) = git_branch_cache().lock() {
                if signals.now - c.0 < git_ttl {
                    c.1.clone()
                } else {
                    Default::default()
                }
            } else {
                Default::default()
            };
        if branches.is_empty() {
            let dirs: std::collections::BTreeSet<String> = out
                .iter()
                .filter_map(|v| v["dir"].as_str())
                .filter(|d| !d.is_empty())
                .map(String::from)
                .collect();
            let dir_list: Vec<String> = dirs.into_iter().collect();
            for chunk in dir_list.chunks(12) {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|d| {
                        let d = d.clone();
                        std::thread::spawn(move || {
                            let out = std::process::Command::new("git")
                                .args(["-C", &d, "rev-parse", "--abbrev-ref", "HEAD"])
                                .output()
                                .ok()?;
                            out.status.success().then(|| {
                                (d, String::from_utf8_lossy(&out.stdout).trim().to_string())
                            })
                        })
                    })
                    .collect();
                for h in handles {
                    if let Ok(Some((d, b))) = h.join() {
                        branches.insert(d, b);
                    }
                }
            }
            if let Ok(mut c) = git_branch_cache().lock() {
                *c = (signals.now, branches.clone());
            }
        }
        for v in out.iter_mut() {
            let b = v["dir"].as_str().and_then(|d| branches.get(d)).cloned().unwrap_or_default();
            v["branch"] = json!(b);
        }
    }

    // Previews: RUNNING sessions get a bounded parallel tmux capture (30
    // lines like Python's batch, py:20137); STOPPED sessions get the saved
    // log tail (py:20218-20223). Both feed Python's preview pair: scalar +
    // the preview_lines ARRAY the SPA maps over (AMUX-2588).
    //
    // Cached with a TTL matching the status-pane cache: previews are the
    // dominant cost (~100 tmux capture-pane subprocesses per uncached call).
    {
        let preview_ttl = env_secs("AMUX_PREVIEW_CACHE_TTL_S", 3.0);
        let cached_previews: Option<BTreeMap<String, String>> =
            if let Ok(c) = preview_cache().lock() {
                if signals.now - c.0 < preview_ttl && !c.1.is_empty() {
                    Some(c.1.clone())
                } else {
                    None
                }
            } else {
                None
            };

        let raws = if let Some(cached) = cached_previews {
            cached
        } else {
            let names: Vec<(String, bool)> = out
                .iter()
                .filter_map(|v| {
                    let n = v["name"].as_str()?.to_string();
                    let running = v["running"].as_bool().unwrap_or(false);
                    Some((n, running))
                })
                .collect();
            // Seed from the status probe's captures — same command, same 30 lines.
            let mut raws: std::collections::BTreeMap<String, String> = signals
                .panes
                .iter()
                .filter(|(_, raw)| !raw.trim().is_empty())
                .map(|(n, raw)| (n.clone(), raw.clone()))
                .collect();
            let names: Vec<(String, bool)> =
                names.into_iter().filter(|(n, _)| !raws.contains_key(n)).collect();
            for chunk in names.chunks(12) {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|(name, running)| {
                        let n = name.clone();
                        let running = *running;
                        std::thread::spawn(move || {
                            if running {
                                let pt = pane_target(&format!("amux-{n}"));
                                let out = std::process::Command::new("tmux")
                                    .args(["capture-pane", "-t", &pt, "-p", "-e", "-S", "-30"])
                                    .output()
                                    .ok()?;
                                Some((n, String::from_utf8_lossy(&out.stdout).trim().to_string()))
                            } else {
                                let raw = stopped_session_raw(&n);
                                (!raw.is_empty()).then_some((n, raw))
                            }
                        })
                    })
                    .collect();
                for h in handles {
                    if let Ok(Some((n, p))) = h.join() {
                        raws.insert(n, p);
                    }
                }
            }
            if let Ok(mut c) = preview_cache().lock() {
                *c = (signals.now, raws.clone());
            }
            raws
        };
        for v in out.iter_mut() {
            if let Some(name) = v["name"].as_str() {
                if let Some(raw) = raws.get(name) {
                    let (preview, lines) = preview_of(raw);
                    v["preview"] = json!(preview);
                    v["preview_lines"] = json!(lines);
                    let wr = derive_waiting_reason(raw);
                    if !wr.is_empty() {
                        v["waiting_reason"] = json!(wr);
                        if v["status"].as_str() != Some("active") {
                            v["status"] = json!("waiting");
                        }
                    }
                }
            }
        }
    }

    // Python's exact sort (py:20456-20457): pinned first, running next,
    // active/waiting before idle/blank, then most-recent human activity.
    let status_rank = |s: &str| -> i64 {
        match s {
            "active" | "waiting" => 0,
            _ => 1,
        }
    };
    out.sort_by(|a, b| {
        let key = |v: &serde_json::Value| {
            (
                !v["pinned"].as_bool().unwrap_or(false),
                !v["running"].as_bool().unwrap_or(false),
                status_rank(v["status"].as_str().unwrap_or("")),
                -v["last_activity"].as_i64().unwrap_or(0),
            )
        };
        key(a).cmp(&key(b))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISOLATED (AMUX-3232): the peer-facing fleet list strips isolated
    /// (raw-agent) workers so peers cannot discover them, while the OWNER
    /// dashboard (no worker header) sees the full fleet. The normal worker is the
    /// negative control that lets this test actually fail.
    #[test]
    fn peer_list_hides_isolated_workers_owner_sees_all() {
        let arr = serde_json::to_string(&serde_json::json!([
            {"name": "normal", "isolated": false},
            {"name": "secret", "isolated": true},
        ]))
        .unwrap();

        // A PEER carries a server-stamped worker header, so the isolated worker
        // is gone and the normal one remains.
        let mut peer = axum::http::HeaderMap::new();
        peer.insert("x-amux-worker", "some-peer".parse().unwrap());
        let peer_view: Vec<serde_json::Value> =
            serde_json::from_str(&filter_isolated_for_peer(&arr, &peer)).unwrap();
        let peer_names: Vec<&str> =
            peer_view.iter().filter_map(|s| s["name"].as_str()).collect();
        assert_eq!(peer_names, vec!["normal"], "peer must not see the isolated worker");

        // The OWNER dashboard sends no worker/session header, so it sees BOTH.
        let owner = axum::http::HeaderMap::new();
        let owner_view: Vec<serde_json::Value> =
            serde_json::from_str(&filter_isolated_for_peer(&arr, &owner)).unwrap();
        let owner_names: Vec<&str> =
            owner_view.iter().filter_map(|s| s["name"].as_str()).collect();
        assert_eq!(owner_names, vec!["normal", "secret"], "owner sees the full fleet");

        // An EMPTY worker header is the owner, not a peer (the send guard's same
        // rule), so it must not trigger the strip.
        let mut empty = axum::http::HeaderMap::new();
        empty.insert("x-amux-worker", "".parse().unwrap());
        assert!(!caller_is_peer(&empty), "an empty header is the owner, not a peer");
    }

    /// AMUX-3182: the create modal could not honestly make an ollama worker.
    /// An ollama worker's model must land in CC_MODEL (the start arm reads it),
    /// never as `--model` in CC_FLAGS, and the CLAUDE default model must never
    /// be applied to a local-model worker. Each assertion carries a positive
    /// control on the SAME inputs so a vacuous pass is impossible (ethos rule 7).
    #[test]
    fn worker_model_env_wires_ollama_to_cc_model_not_flags() {
        // Ollama + a chosen model -> CC_MODEL, and NO --model in CC_FLAGS.
        let (flags, model, resolved) = worker_model_env("ollama", "qwen3.8:27b", "", "opus");
        assert_eq!(model, "qwen3.8:27b", "ollama model must be CC_MODEL");
        assert!(flags.is_empty(), "ollama must not put --model in CC_FLAGS, got {flags:?}");
        assert!(!flags.contains("--model"), "ollama CC_FLAGS must never carry --model");
        assert_eq!(resolved, "qwen3.8:27b", "resolved model echoed to the response");
        // POSITIVE CONTROL: identical inputs, codex provider -> the model DOES
        // ride in CC_FLAGS as --model and CC_MODEL is empty. Proves the ollama
        // branch actually diverges rather than the assertions being vacuous.
        let (cflags, cmodel, cresolved) = worker_model_env("codex", "qwen3.8:27b", "", "opus");
        assert_eq!(cflags, "--model qwen3.8:27b");
        assert!(cmodel.is_empty(), "agent CLIs have no CC_MODEL");
        assert_eq!(cresolved, "qwen3.8:27b");
        // Grok is an agent CLI too: model rides in CC_FLAGS, never CC_MODEL.
        let (gflags, gmodel, gresolved) = worker_model_env("grok", "grok-4.6", "", "opus");
        assert_eq!(gflags, "--model grok-4.6");
        assert!(gmodel.is_empty(), "grok must not use the ollama CC_MODEL path");
        assert_eq!(gresolved, "grok-4.6");

        // Ollama + NO model -> CC_MODEL empty (start uses the ollama default),
        // and the CLAUDE default ("opus") must appear NOWHERE. This is the exact
        // incident: pre-fix this produced CC_FLAGS="--model opus".
        let (flags2, model2, resolved2) = worker_model_env("ollama", "", "", "opus");
        assert!(model2.is_empty(), "no claude default in CC_MODEL for ollama");
        assert!(flags2.is_empty(), "no claude default in CC_FLAGS for ollama");
        assert!(!flags2.contains("opus") && resolved2 != "opus", "claude default leaked to an ollama worker");
        // POSITIVE CONTROL: claude + no model DOES apply the default.
        let (dflags, dmodel, _) = worker_model_env("claude", "", "", "opus");
        assert_eq!(dflags, "--model opus", "claude default must apply to a claude worker");
        assert!(dmodel.is_empty());

        // Ollama + a NON-default local model is honoured, not dropped.
        let (_, model3, _) = worker_model_env("ollama", "qwen2.5vl:7b", "", "opus");
        assert_eq!(model3, "qwen2.5vl:7b", "a non-default ollama model pick must be kept");

        // Explicit flags win for both, and ollama keeps its model in CC_MODEL.
        let (eflags, emodel, _) = worker_model_env("ollama", "qwen3.8:27b", "--sandbox danger", "opus");
        assert_eq!(eflags, "--sandbox danger", "explicit flags honoured verbatim (AMUX-3114)");
        assert_eq!(emodel, "qwen3.8:27b", "ollama model stays in CC_MODEL alongside explicit flags");
    }

    /// The 2026-08-13 "task names are out of date" bug: a stale, UNSTAMPED
    /// `task_summary` ("Luke's Wilderness Tales" on the Obsidian lane) outranked
    /// the honest desc because summary had no freshness gate. The gate must make
    /// an unstamped/stale summary lose to desc, while a fresh summary still wins.
    #[test]
    fn task_name_precedence_gates_a_stale_summary() {
        // Unstamped/stale summary (summary_fresh=false), no board -> desc, NOT
        // the frozen relic. This is the exact incident.
        let (name, src) = resolve_task_name(None, false, "Luke's Wilderness Tales", false, "Ethan's personal notes");
        assert_eq!(src, "desc", "a stale summary must not be the task name");
        assert_eq!(name, "Ethan's personal notes");

        // A FRESH summary still wins over desc — the gate does not kill the
        // feature, only stale values.
        let (name, src) = resolve_task_name(None, false, "Draft the county reply", true, "role desc");
        assert_eq!(src, "summary");
        assert_eq!(name, "Draft the county reply");

        // A fresh board card outranks even a fresh summary (the ledger is truth).
        let (name, src) = resolve_task_name(Some("AMUX-9 do the thing"), true, "a summary", true, "desc");
        assert_eq!(src, "board");
        assert_eq!(name, "AMUX-9 do the thing");

        // Stale board beats desc but loses to a fresh summary; a stale summary
        // with a stale board falls to the stale board (last resort before desc).
        let (name, src) = resolve_task_name(Some("old board title"), false, "stale summary", false, "desc");
        assert_eq!(src, "board");
        assert_eq!(name, "old board title");

        // Nothing anywhere -> desc, never an empty task claim from a blank summary.
        let (name, src) = resolve_task_name(None, false, "", false, "just the role");
        assert_eq!(src, "desc");
        assert_eq!(name, "just the role");
    }

    /// AMUX-2904. A lane's Stop hook fires when the MAIN turn ends, so a lane
    /// whose background agents are still working self-reports `idle` —
    /// correctly about the turn, misleadingly about the lane. Measured
    /// 2026-08-11: primis read `idle` with a subagent write 20s old.
    ///
    /// One-way, like the pane contradiction beside it: idle -> active only, so
    /// a missed signal is a late correction and never a false "busy".
    #[test]
    fn a_lane_with_live_subagents_is_not_idle() {
        let mut sig = signals();
        sig.now = 1_000_000.0;
        // CONTROL FIRST: with no subagent activity the lane stays idle, or the
        // assertion below proves nothing.
        assert!(!sig.subagents_working("primis"));

        // A recent write contradicts `idle`.
        sig.subagent_activity.insert("primis".into(), sig.now - 20.0);
        assert!(sig.subagents_working("primis"), "a 20s-old subagent write must contradict idle");

        // THE INCIDENT (2026-08-13): a subagent "still thinking with xhigh
        // effort" writes nothing for a stretch, so its newest transcript write is
        // minutes old while it is very much working. A 90s-old write is PAST the
        // 60s contradiction_window that used to gate this — the lane read IDLE
        // while crunching. It must now read as working (subagent cadence window).
        sig.subagent_activity.insert("primis".into(), sig.now - 90.0);
        assert!(
            sig.subagents_working("primis"),
            "a 90s-old subagent write (a thinking agent between writes) must still contradict idle"
        );

        // Stale activity does NOT. An agent that finished an hour ago is not
        // evidence the lane is busy now — the window is generous, not unbounded.
        sig.subagent_activity.insert("primis".into(), sig.now - 86_400.0);
        assert!(!sig.subagents_working("primis"), "stale subagent activity must not pin a lane active");

        // Scoped per lane.
        sig.subagent_activity.insert("other".into(), sig.now - 5.0);
        assert!(!sig.subagents_working("primis"));
        assert!(sig.subagents_working("other"));
    }

    /// AMUX-3048: the EVENT-DRIVEN count, when a lane reports one, flips the lane
    /// working even with NO recent transcript write — the xhigh-thinking case the
    /// mtime window could not catch (AMUX-3030). Additive: a lane that reports no
    /// subagent event (gemini/codex, or one that spawned none) is pure mtime.
    #[test]
    fn reported_subagent_count_drives_working_over_a_silent_mtime() {
        let mut sig = signals();
        sig.now = 1_000_000.0;
        // CONTROL: no report, no mtime -> not working, or nothing below proves out.
        assert!(!sig.subagents_working("primis"));

        // A live count with NO transcript activity at all still reads working —
        // exactly what the mtime window missed.
        sig.reports = serde_json::json!({
            "primis": {"state": "idle", "subagents": {"count": 2, "ts": sig.now - 5.0}}
        });
        assert!(
            sig.subagents_working("primis"),
            "a reported live count must contradict idle even with a silent transcript"
        );

        // Count back to 0 with no mtime -> not working (the count invents nothing).
        sig.reports = serde_json::json!({
            "primis": {"subagents": {"count": 0, "ts": sig.now - 5.0}}
        });
        assert!(!sig.subagents_working("primis"), "count 0 with no mtime must read idle");

        // A hookless lane (no `subagents` key) is unaffected: pure mtime fallback.
        sig.reports = serde_json::json!({"gemini-lane": {"state": "active"}});
        sig.subagent_activity.insert("gemini-lane".into(), sig.now - 30.0);
        assert!(sig.subagents_working("gemini-lane"), "hookless lane still uses the mtime window");
        sig.subagent_activity.insert("gemini-lane".into(), sig.now - 86_400.0);
        assert!(!sig.subagents_working("gemini-lane"), "stale mtime on a hookless lane reads idle");
    }

    #[test]
    fn status_vocabulary_matches_python() {
        assert_eq!(python_status(r#"{"state":"active","turn":null}"#), "active");
        assert_eq!(python_status(r#"{"state":"idle","since":"x"}"#), "idle");
        assert_eq!(python_status(r#"{"state":"rate_limited","reset_at":null}"#), "rate-limited");
        assert_eq!(python_status(r#"{"state":"stopped"}"#), "");
    }

    pub(super) fn signals() -> FleetSignals {
        FleetSignals {
            activity: BTreeMap::new(),
            created: BTreeMap::new(),
            running: BTreeSet::new(),
            shell_only: BTreeSet::new(),
            reports: serde_json::Value::Null,
            subagent_activity: BTreeMap::new(),
            transitions: BTreeMap::new(),
            started: BTreeMap::new(),
            panes: BTreeMap::new(),
            now: 1_000_000.0,
        }
    }

    /// AF-82 / D1: `running` must honour a fresh self-report, not only the pane
    /// scrape. A lane launched as a child of a wrapper shell scrapes shell_only,
    /// but an `active` report 57s old means an agent IS running (the exact
    /// ai-video-editor case). The scrape is the fallback; ignoring the report is
    /// what showed a working lane as stopped.
    #[test]
    fn agent_running_honours_a_fresh_active_self_report_over_a_shell_scrape() {
        let mut s = signals();
        let tmux = "amux-avetest";
        s.running.insert(tmux.into()); // the tmux session exists
        s.shell_only.insert(tmux.into()); // but the pane scrapes as a bare shell
        assert!(!s.agent_running(tmux), "shell scrape + no report reads not-running");

        // A fresh active report from THIS life -> running, despite the shell scrape.
        s.started.insert("avetest".into(), s.now - 1000.0);
        s.reports = serde_json::json!({ "avetest": { "state": "active", "ts": s.now - 57.0 } });
        assert!(s.agent_running(tmux), "a 57s-old active self-report means an agent is running");

        // A PREVIOUS-LIFE report (before the session (re)started) must NOT count.
        s.started.insert("avetest".into(), s.now - 10.0);
        assert!(!s.agent_running(tmux), "a report from before the last start is a dead life");

        // An idle report does not assert a live agent.
        s.started.insert("avetest".into(), s.now - 1000.0);
        s.reports = serde_json::json!({ "avetest": { "state": "idle", "ts": s.now - 5.0 } });
        assert!(!s.agent_running(tmux), "an idle report does not make it running");

        // A STALE active report (older than the live window) must NOT count.
        s.reports = serde_json::json!({ "avetest": { "state": "active", "ts": s.now - 4000.0 } });
        assert!(!s.agent_running(tmux), "a stale active report (>30min) is not a live agent");

        // The session GONE: no report resurrects it.
        s.running.clear();
        s.reports = serde_json::json!({ "avetest": { "state": "active", "ts": s.now - 5.0 } });
        assert!(!s.agent_running(tmux), "a report cannot resurrect a lane whose session is gone");

        // Normal case: a non-shell foreground pane is running with no report at all.
        s.running.insert(tmux.into());
        s.shell_only.clear();
        s.reports = serde_json::Value::Null;
        assert!(s.agent_running(tmux), "a non-shell foreground pane is running");
    }

    #[test]
    fn status_blank_when_not_running() {
        let s = signals();
        assert_eq!(s.derive_status("x", false), "");
    }

    #[test]
    fn status_active_on_recent_activity_idle_otherwise() {
        let mut s = signals();
        s.activity.insert("amux-x".into(), 999_970); // 30s ago
        assert_eq!(s.derive_status("x", true), "active");
        s.activity.insert("amux-x".into(), 999_000); // 1000s ago
        assert_eq!(s.derive_status("x", true), "idle");
    }

    #[test]
    fn status_prefers_persisted_transition_including_waiting() {
        let mut s = signals();
        s.activity.insert("amux-x".into(), 999_000);
        s.transitions.insert("x".into(), ("waiting".into(), 999_900.0));
        assert_eq!(s.derive_status("x", true), "waiting");
    }

    #[test]
    fn stale_active_transition_demotes_to_idle() {
        let mut s = signals();
        // Transition says active, but the pane has been silent 1000s (>120).
        s.activity.insert("amux-x".into(), 999_000);
        s.transitions.insert("x".into(), ("active".into(), 999_100.0));
        assert_eq!(s.derive_status("x", true), "idle");
    }

    #[test]
    fn pre_restart_transition_is_discarded() {
        let mut s = signals();
        s.activity.insert("amux-x".into(), 999_970);
        s.transitions.insert("x".into(), ("waiting".into(), 900.0));
        s.started.insert("x".into(), 999_000.0); // restarted AFTER the event
        assert_eq!(s.derive_status("x", true), "active"); // falls to activity
    }

    #[test]
    fn self_report_overrides_with_asymmetric_freshness() {
        let mut s = signals();
        s.activity.insert("amux-x".into(), 999_970); // scrape would say active
        // A 4h-old idle report STILL wins (idle does not decay, py:20233).
        s.reports = json!({"x": {"state": "idle", "ts": 985_600.0, "source": "stop-hook"}});
        assert_eq!(s.derive_status("x", true), "idle");
        // A 4h-old ACTIVE report licenses nothing (heartbeat lapsed).
        s.reports = json!({"x": {"state": "active", "ts": 985_600.0, "source": "hb"}});
        s.activity.insert("amux-x".into(), 999_000);
        assert_eq!(s.derive_status("x", true), "idle");
        // A fresh waiting report wins over the activity fallback.
        s.reports = json!({"x": {"state": "waiting", "ts": 999_990.0, "source": "hook"}});
        assert_eq!(s.derive_status("x", true), "waiting");
    }

    /// A report from BEFORE the session's last (re)start is a PREVIOUS LIFE
    /// and licenses nothing — live specimen 2026-08-11: board-exp-1 switched
    /// claude -> codex, and its hours-old claude `idle` report (24h idle
    /// window) outranked the codex trust picker on the pane, reading a lane
    /// blocked on input as idle. The control half: the same report with no
    /// restart after it keeps its authority.
    #[test]
    fn a_report_from_before_the_last_restart_is_a_previous_life() {
        let mut s = signals();
        // Pane paints a picker; activity fresh so the pane is admissible.
        s.activity.insert("amux-x".into(), 999_970);
        s.panes.insert("x".into(), "Do you trust this directory?\n\u{203a} 1. Yes, continue\n  2. No, quit\n  Press enter to continue".into());
        // CONTROL: a FRESH idle report (inside the contradiction window) with
        // no restart recorded still wins over the picker on the pane — the
        // report is the D1 authority and this is the report/repaint race.
        //
        // This control used to use a 4h-OLD idle report and expect "idle".
        // That pinned the exact defect Ethan hit live on tubescience
        // (2026-08-11): a stale idle outranking a visible AskUserQuestion
        // picker, with him pressing Enter into a lane nothing had flagged as
        // waiting. The waiting-contradiction now lets the pane show through
        // once the report is older than the window, so the control moves
        // INSIDE the window — which is what "no restart, report wins" was
        // always supposed to mean.
        s.reports = json!({"x": {"state": "idle", "ts": 999_970.0, "source": "stop-hook"}});
        assert_eq!(s.derive_status("x", true), "idle");
        // A STALE idle report (4h) over the same picker: the pane's waiting
        // shows through — no restart needed. This is tonight's fix.
        s.reports = json!({"x": {"state": "idle", "ts": 985_600.0, "source": "stop-hook"}});
        assert_eq!(s.derive_status("x", true), "waiting");
        // The lane restarted AFTER the report (provider switch): the report
        // is void and the pane's waiting shows through.
        s.started.insert("x".into(), 999_000.0);
        assert_eq!(s.derive_status("x", true), "waiting");
    }

    #[test]
    fn a_fresh_idle_report_outranks_the_subagent_window() {
        // gtm-engine, 2026-08-13 (Ethan: "says working but it appears done"):
        // the main turn ENDED — a stop-hook posted a fresh idle report and the
        // pane showed "✻ Crunched for 1m 7s" at an empty prompt — but a
        // BACKGROUND subagent had written 30s ago, inside the 240s
        // AMUX_SUBAGENT_WORKING_S window. The subagent contradiction flipped
        // idle->active with NO report-age gate (the pane and waiting
        // contradictions both have one), so the header read WORKING for up to
        // 240s after the turn was done. FAILS on the pre-fix code (active),
        // passes on the gated rule (idle).
        let mut s = signals();
        s.reports = json!({"x": {"state": "idle", "ts": s.now - 30.0, "source": "stop-hook"}});
        s.subagent_activity.insert("x".into(), s.now - 30.0);
        assert_eq!(
            s.derive_status("x", true),
            "idle",
            "a fresh stop-hook idle report (main turn stopped -> foreground \
             subagents finished) outranks the subagent-mtime window"
        );

        // AMUX-2904 PRESERVED: a main turn ACTIVE with foreground subagents has
        // not stopped, so there is NO fresh idle report (idle_report_age None ->
        // unwrap_or(true)) and live subagents still flip idle->active.
        let mut s = signals();
        s.subagent_activity.insert("x".into(), s.now - 30.0);
        assert_eq!(
            s.derive_status("x", true),
            "active",
            "with no fresh idle report, live subagents still flip idle->active"
        );

        // BOUNDED LATE CORRECTION: once the idle report ages past the
        // contradiction window (60s), a still-writing subagent flips it active —
        // the generous window's only documented cost, unchanged by the gate.
        let mut s = signals();
        s.reports = json!({"x": {"state": "idle", "ts": s.now - 120.0, "source": "stop-hook"}});
        s.subagent_activity.insert("x".into(), s.now - 30.0);
        assert_eq!(
            s.derive_status("x", true),
            "active",
            "a stale idle report (past the window) lets live subagents show through"
        );
    }

    #[test]
    fn preview_lines_is_a_filtered_array_of_strings() {
        let raw = "\u{1b}[1mDoing the work\u{1b}[0m\n\
                   ⏵⏵ bypass permissions on\n\
                   ══════════════════════\n\
                   ok\n\
                   Implemented the fix in board.rs\n\
                   x\n";
        let (preview, lines) = preview_of(raw);
        // Scalar preview: last intelligible line (skips ⏵⏵, short, low-alnum).
        assert_eq!(preview, "Implemented the fix in board.rs");
        // Array: bars (low alnum ratio), the ⏵⏵ line, and <=2-char lines
        // are dropped; ANSI is stripped from kept lines.
        assert_eq!(lines, vec!["Doing the work", "Implemented the fix in board.rs"]);
    }

    #[test]
    fn preview_skips_status_bar_line() {
        let raw = "Some output text\n\
                   \u{276f}\u{a0}\n\
                   ──────\n\
                   \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)\n";
        let (preview, _) = preview_of(raw);
        assert_eq!(preview, "Some output text");
    }

    #[test]
    fn preview_lines_falls_back_to_raw_tail_when_nothing_intelligible() {
        // Every line >3 chars with alnum ratio < 0.3 -> nothing intelligible.
        let raw = "════\n────\n╭──╮\n";
        let (_, lines) = preview_of(raw);
        // Fallback keeps the stripped non-empty tail lines (py:20314-20316).
        assert_eq!(lines, vec!["════", "────", "╭──╮"]);
    }

    #[test]
    fn preview_truncates_at_python_lengths() {
        let long = "a".repeat(300);
        let (preview, lines) = preview_of(&long);
        assert_eq!(preview.chars().count(), 120);
        assert_eq!(lines[0].chars().count(), 200);
    }
}

// ---------------------------------------------------------------------------
// AMUX-2646 — "it is running but says idle".
//
// The frames below are VERBATIM captures of the live fleet on 2026-08-09,
// not constructed ones. That matters: the convenient fixture is convenient
// precisely because it lacks the property that made the incident. Two of
// these were built by hand first and were wrong in ways that would have made
// the suite pass against the bug —
//
//   * a "generating" frame with `esc to interrupt` on the bar. The lane that
//     was actually mislabelled (`amux-rust`) had NO such bar; its only mark
//     was a live spinner. A suite built on the first frame alone would have
//     been green against the specimen it exists for.
//   * an "idle with background agents" frame carrying `esc to interrupt` on
//     the bar — the shape of the theory `pane_bar_says_generating` records
//     itself REJECTING ("empty ❯ + esc to interrupt = idle with background
//     agents"). Whether that frame exists decides whether the override below
//     is safe at all, and it had never been measured either way. It was here:
//     across four live lanes, this Claude Code build prints `esc to interrupt`
//     only while the MAIN turn is generating — an idle lane with two agents
//     shows `⏵⏵ bypass permissions on (shift+tab to cycle) · ← 2 agents` and
//     a completed-turn marker (`✻ Churned for 2m 57s`). So the bar is a sound
//     work signal, and the rejection in that function was right for a reason
//     nobody had confirmed. IDLE_WITH_AGENTS below is the real frame; if a
//     future Claude Code starts painting `esc to interrupt` for background
//     agents, that test goes red and this override needs rethinking, which is
//     the whole point of keeping the frame rather than a paraphrase of it.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod status_truth {
    use super::tests::signals;
    use super::*;

    /// Live `amux`, mid-turn: spinner AND `esc to interrupt` on the bar.
    const WORKING_BAR: &str = "\
2436    const _active = document.activeElement;
\u{273b} Doing\u{2026} (3m 56s \u{b7} \u{2193} 6.8k tokens)
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
\u{276f}
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt \u{b7} \u{2190} 2 agents";

    /// Live `amux-rust`, mid-turn — THE SPECIMEN. Nothing on the status bar;
    /// the spinner is the only evidence. This is the lane that showed `idle`
    /// on its card for 1076s while it was demonstrably working.
    const WORKING_SPINNER_ONLY: &str = "\
\u{273b} Nesting\u{2026} (4m 24s \u{b7} \u{2193} 6.4k tokens)
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
\u{276f} [05:08 PM] this worker is doing work but there isnt anything in inprogress
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)";

    /// Live `amux-frustrations`: genuinely idle, two BACKGROUND agents still
    /// running. The completed-turn marker (`for 2m 57s`), an empty composer,
    /// and no `esc to interrupt`. This is the frame that must NOT be read as
    /// work, or every finished lane with agents flips to active forever.
    const IDLE_WITH_AGENTS: &str = "\
  Left at done, not verified \u{2014} live behaviour confirmed.
\u{273b} Churned for 2m 57s
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
\u{276f}
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} \u{2190} 2 agents";

    /// Live `uitest-a`: the agent exited, tmux session still up.
    const SHELL_PROMPT: &str = "\
tmp$ unset ANTHROPIC_API_KEY
tmp$ claude --model claude-opus-4-6 --dangerously-skip-permissions
Resume this session with:
claude --resume \"uitest-a\"
tmp$";

    /// A permission selector — waiting on a human, not working.
    const WAITING_SELECTOR: &str = "\
Do you want to proceed?
\u{276f} 1. Yes
  2. No, and tell Claude what to do differently (esc)
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)";

    /// The usage-limit menu (D2). Also a human decision, never work.
    const RATE_LIMIT_MENU: &str = "\
Claude usage limit reached. Your limit will reset at 3pm.
\u{276f} 1. Wait and continue
  2. Switch to a different model";

    /// A herdr lane MID-TURN. herdr refuses a history read while it is
    /// working, so the capture is empty BY DESIGN — the one frame where
    /// "no markers" must not mean "idle".
    const HERDR_MID_TURN: &str = "";

    /// One row of the truth table.
    struct Case {
        what: &'static str,
        /// (state, age_s, source)
        report: Option<(&'static str, f64, &'static str)>,
        /// (state, age_s)
        transition: Option<(&'static str, f64)>,
        pane: Option<&'static str>,
        /// How long ago the pane last painted.
        activity_age_s: f64,
        running: bool,
        expect: &'static str,
    }

    fn run(c: &Case) -> String {
        let mut s = signals();
        s.activity.insert("x".into(), 0); // never matched: keys are `amux-<n>`
        s.activity.insert("amux-x".into(), (s.now - c.activity_age_s) as i64);
        if c.running {
            s.running.insert("amux-x".into());
        }
        if let Some((st, age, src)) = c.report {
            s.reports = json!({"x": {"state": st, "ts": s.now - age, "source": src}});
        }
        if let Some((st, age)) = c.transition {
            s.transitions.insert("x".into(), (st.into(), s.now - age));
        }
        if let Some(p) = c.pane {
            s.panes.insert("x".into(), p.into());
        }
        s.derive_status("x", c.running)
    }

    /// THE TABLE. Every cell is a (report, age, source, pane, activity,
    /// running) combination with the status it must produce.
    #[test]
    fn status_truth_table() {
        let cases = [
            // ---- the bug, in both of its live shapes -------------------
            Case {
                what: "STALE idle report + pane mid-turn (bar) = THE BUG",
                report: Some(("idle", 1076.0, "stop-hook-test")),
                transition: None,
                pane: Some(WORKING_BAR),
                activity_age_s: 1.0,
                running: true,
                expect: "active",
            },
            Case {
                what: "STALE idle report + pane mid-turn (spinner only) = THE SPECIMEN",
                report: Some(("idle", 1076.0, "stop-hook-test")),
                transition: None,
                pane: Some(WORKING_SPINNER_ONLY),
                activity_age_s: 1.0,
                running: true,
                expect: "active",
            },
            Case {
                what: "a DAY-old idle report loses to a live pane just the same",
                report: Some(("idle", 80_000.0, "stop-hook")),
                transition: None,
                pane: Some(WORKING_BAR),
                activity_age_s: 2.0,
                running: true,
                expect: "active",
            },
            // ---- the grace window: a fresh report is still the authority
            Case {
                what: "FRESH idle report wins over the pane (report/repaint race)",
                report: Some(("idle", 3.0, "stop-hook")),
                transition: None,
                pane: Some(WORKING_BAR),
                activity_age_s: 1.0,
                running: true,
                expect: "idle",
            },
            Case {
                what: "fresh idle report + quiet pane: plain idle",
                report: Some(("idle", 5.0, "stop-hook")),
                transition: None,
                pane: Some(IDLE_WITH_AGENTS),
                activity_age_s: 1.0,
                running: true,
                expect: "idle",
            },
            // ---- silence is NOT contradiction --------------------------
            Case {
                what: "stale idle + a parked lane that has not painted: stays idle",
                report: Some(("idle", 9_000.0, "stop-hook")),
                transition: None,
                // The pane still holds a mid-turn frame in scrollback, but
                // nothing has painted for an hour: not evidence.
                pane: Some(WORKING_BAR),
                activity_age_s: 3_600.0,
                running: true,
                expect: "idle",
            },
            Case {
                what: "idle lane WITH BACKGROUND AGENTS is idle, not active",
                report: Some(("idle", 700.0, "stop-hook")),
                transition: None,
                pane: Some(IDLE_WITH_AGENTS),
                activity_age_s: 2.0,
                running: true,
                expect: "idle",
            },
            // ---- no report at all (hookless lane, dropped POST) --------
            Case {
                what: "no report + working pane",
                report: None,
                transition: None,
                pane: Some(WORKING_SPINNER_ONLY),
                activity_age_s: 1.0,
                running: true,
                expect: "active",
            },
            Case {
                what: "no report + shell prompt (agent exited)",
                report: None,
                transition: None,
                pane: Some(SHELL_PROMPT),
                activity_age_s: 1.0,
                running: true,
                expect: "idle",
            },
            Case {
                what: "no report + selector = waiting on a human",
                report: None,
                transition: None,
                pane: Some(WAITING_SELECTOR),
                activity_age_s: 1.0,
                running: true,
                expect: "waiting",
            },
            Case {
                what: "no report + usage-limit menu = waiting (never invented as active)",
                report: None,
                transition: None,
                pane: Some(RATE_LIMIT_MENU),
                activity_age_s: 1.0,
                running: true,
                expect: "waiting",
            },
            // ---- active reports ---------------------------------------
            Case {
                what: "fresh active report + dead/unreadable pane: believed",
                report: Some(("active", 10.0, "tool-hook")),
                transition: None,
                pane: Some(HERDR_MID_TURN),
                activity_age_s: 1.0,
                running: true,
                expect: "active",
            },
            Case {
                what: "STALE active report (past the heartbeat) never overrides",
                report: Some(("active", 4_000.0, "tool-hook")),
                transition: None,
                pane: Some(IDLE_WITH_AGENTS),
                activity_age_s: 2.0,
                running: true,
                expect: "idle",
            },
            // ---- herdr: an empty capture is not evidence of anything ---
            Case {
                what: "herdr lane, empty capture, painting: NOT idle",
                report: None,
                transition: None,
                pane: Some(HERDR_MID_TURN),
                activity_age_s: 2.0,
                running: true,
                expect: "active",
            },
            Case {
                what: "herdr lane, empty capture, silent for an hour: idle",
                report: None,
                transition: None,
                pane: Some(HERDR_MID_TURN),
                activity_age_s: 3_600.0,
                running: true,
                expect: "idle",
            },
            // ---- transitions and liveness ------------------------------
            Case {
                what: "waiting transition survives (no report, no pane)",
                report: None,
                transition: Some(("waiting", 100.0)),
                pane: None,
                activity_age_s: 3_600.0,
                running: true,
                expect: "waiting",
            },
            Case {
                what: "stale active transition demotes when the pane is silent",
                report: None,
                transition: Some(("active", 900.0)),
                pane: None,
                activity_age_s: 1_000.0,
                running: true,
                expect: "idle",
            },
            Case {
                what: "not running is blank, whatever anything else says",
                report: Some(("active", 1.0, "tool-hook")),
                transition: None,
                pane: Some(WORKING_BAR),
                activity_age_s: 1.0,
                running: false,
                expect: "",
            },
        ];
        let mut failed = vec![];
        for c in &cases {
            let got = run(c);
            if got != c.expect {
                failed.push(format!("  {}\n     want {:?}, got {:?}", c.what, c.expect, got));
            }
        }
        assert!(failed.is_empty(), "status truth table:\n{}", failed.join("\n"));
    }

    /// THE PROPERTY, over the full product of the table's inputs: a lane whose
    /// pane is unambiguously mid-turn is never reported `idle` — unless an
    /// idle report younger than the contradiction window is standing behind
    /// it, which is the one deliberate exception (the report is the D1
    /// authority and this is where the report/repaint race lives).
    ///
    /// Exhaustive rather than random: the input space is small enough to
    /// enumerate, and an enumerated space cannot get lucky.
    #[test]
    fn no_input_combination_reports_idle_over_a_working_pane() {
        let states = ["idle", "active", "waiting", "error", "bogus"];
        let ages = [0.0, 1.0, 59.0, 61.0, 121.0, 1_076.0, 1_801.0, 86_401.0];
        let sources = ["stop-hook", "tool-hook", "prompt-hook", "stop-hook-test", ""];
        let working_panes = [WORKING_BAR, WORKING_SPINNER_ONLY];
        let act_ages = [0.0, 1.0, 30.0, 59.0];
        let transitions = [None, Some(("idle", 10.0)), Some(("active", 900.0)), Some(("waiting", 5.0))];
        let mut checked = 0usize;
        for pane in working_panes {
            for act in act_ages {
                for tr in transitions {
                    for st in states {
                        for age in ages {
                            for src in sources {
                                let c = Case {
                                    what: "property",
                                    report: Some((st, age, src)),
                                    transition: tr,
                                    pane: Some(pane),
                                    activity_age_s: act,
                                    running: true,
                                    expect: "",
                                };
                                let got = run(&c);
                                checked += 1;
                                let grace = st == "idle" && age <= 60.0;
                                assert!(
                                    got != "idle" || grace,
                                    "idle over a working pane: report=({st},{age}s,{src}) \
                                     transition={tr:?} activity_age={act}s"
                                );
                            }
                        }
                    }
                }
            }
        }
        // The enumeration must have RUN. A property test over an empty product
        // passes vacuously and looks identical to one that proved something.
        assert_eq!(checked, 2 * 4 * 4 * 5 * 8 * 5);
        // And the exception must be REACHABLE, or "unless grace" is dead prose
        // rather than a documented carve-out.
        assert_eq!(
            run(&Case {
                what: "grace is reachable",
                report: Some(("idle", 5.0, "stop-hook")),
                transition: None,
                pane: Some(WORKING_BAR),
                activity_age_s: 0.0,
                running: true,
                expect: "idle",
            }),
            "idle"
        );
    }

    /// The two detectors this composes must actually discriminate the frames.
    /// If `pane_says_working` returned true for everything, the table above
    /// would still pass on its bug rows while quietly breaking every idle row
    /// — so assert the evidence function's verdict directly, per frame.
    #[test]
    fn evidence_discriminates_between_the_live_frames() {
        let mut s = signals();
        s.activity.insert("amux-x".into(), s.now as i64);
        let verdict = |s: &mut FleetSignals, raw: &str| {
            s.panes.insert("x".into(), raw.into());
            s.pane_says_working("x")
        };
        assert!(verdict(&mut s, WORKING_BAR), "bar `esc to interrupt` is work");
        assert!(verdict(&mut s, WORKING_SPINNER_ONLY), "a live spinner is work");
        assert!(!verdict(&mut s, IDLE_WITH_AGENTS), "background agents are NOT the main turn");
        assert!(!verdict(&mut s, SHELL_PROMPT), "a shell is not work");
        assert!(!verdict(&mut s, WAITING_SELECTOR), "waiting on a human is not work");
        assert!(!verdict(&mut s, RATE_LIMIT_MENU), "a usage-limit menu is not work");
        assert!(!verdict(&mut s, HERDR_MID_TURN), "an empty capture proves nothing");
    }

    /// Evidence must be admissible only while it is FRESH — this is the half
    /// that keeps `idle survives silence` true, and the half a reader is most
    /// likely to delete as redundant.
    #[test]
    fn stale_evidence_is_inadmissible_however_loud_it_is() {
        let mut s = signals();
        s.panes.insert("x".into(), WORKING_BAR.into());
        s.activity.insert("amux-x".into(), (s.now - 61.0) as i64);
        assert!(!s.pane_says_working("x"), "a pane that has not painted in 61s is not evidence");
        s.activity.insert("amux-x".into(), (s.now - 59.0) as i64);
        assert!(s.pane_says_working("x"), "…and one that painted 59s ago is");
    }

    /// The capture predicate and the belief predicate are the same predicate.
    /// If they drift, the board (which captures a few panes) and the session
    /// list (which has every running pane in hand) derive different statuses
    /// for the same lane, and the user sees a card that contradicts itself.
    #[test]
    fn a_pane_the_probe_would_not_have_taken_is_not_believed() {
        let mut s = signals();
        s.activity.insert("amux-x".into(), (s.now - 6_000.0) as i64);
        assert!(!s.pane_probe_candidate("x"));
        // A caller stuffs the map anyway (a superset capture, or a test).
        s.panes.insert("x".into(), WORKING_BAR.into());
        assert!(!s.pane_says_working("x"), "belief must re-apply the capture predicate");
        assert_eq!(s.derive_status("x", true), "idle");
    }

    /// THE LIVE-FLEET CONSISTENCY CHECK, read-only, on demand:
    ///
    /// ```text
    /// CARGO_TARGET_DIR=/tmp/amux-status-target cargo test -p amux-server \
    ///   sessions_legacy::status_truth::live_fleet -- --ignored --nocapture
    /// ```
    ///
    /// `#[ignore]` because it reads the machine's real fleet, so it is not a
    /// CI check — it is the sweep instrument. It opens `~/.amux/amux.db`
    /// READ-ONLY (never the live DB read-write: this is real user data) and
    /// captures panes, which is what `tmux capture-pane -p` already does on
    /// every dashboard poll.
    ///
    /// It exists because the ONLY thing that caught AMUX-2646 was a human
    /// noticing a terminal. This is that human, as a command, in one second.
    #[test]
    #[ignore = "reads the live fleet; run explicitly with --ignored"]
    fn live_fleet_status_matches_pane_truth() {
        let home = std::env::var("HOME").unwrap_or_default();
        let db = format!("{home}/.amux/amux.db");
        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap_or_else(|e| panic!("live db {db} unreadable: {e}"));
        let mut s = FleetSignals::load(&conn);
        assert!(!s.running.is_empty(), "no tmux fleet visible — probe is broken, not fleet empty");
        s.capture_panes();
        let probed = s.probed_lanes();
        let mut bad = vec![];
        for (name, working) in &probed {
            let status = s.derive_status(name, true);
            let rep = s.reports.get(name).cloned().unwrap_or(json!({}));
            if *working && status == "idle" {
                bad.push(format!(
                    "  {name}: card=idle but the pane is mid-turn \
                     (report={} age={:.0}s source={} origin={})",
                    rep["state"].as_str().unwrap_or("-"),
                    s.now - rep["ts"].as_f64().unwrap_or(s.now),
                    rep["source"].as_str().unwrap_or("-"),
                    rep["origin"].as_str().unwrap_or("-"),
                ));
            }
        }
        // The whole registry, not only the probed lanes: a status histogram is
        // how a REGRESSION in the other direction shows up (everything flipping
        // to active), which a disagreement count of 0 would happily hide.
        let mut hist: BTreeMap<String, usize> = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(amux_home().join("sessions")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("env") {
                    continue;
                }
                let Some(n) = p.file_stem().and_then(|x| x.to_str()) else { continue };
                let running = s.agent_running(&format!("amux-{n}"));
                let st = s.derive_status(n, running);
                *hist.entry(if st.is_empty() { "<blank>".into() } else { st }).or_default() += 1;
            }
        }
        println!(
            "live fleet: {} tmux sessions, {} painted inside the probe window, \
             {} of those mid-turn, DISAGREEMENTS: {}\n  status histogram: {:?}",
            s.running.len(),
            probed.len(),
            probed.iter().filter(|(_, w)| *w).count(),
            bad.len(),
            hist
        );
        for l in &bad {
            println!("{l}");
        }
        assert!(bad.is_empty(), "card/pane disagreements:\n{}", bad.join("\n"));
    }

    /// tmux's `session_activity` does not move for a DETACHED session, and
    /// every amux lane is detached — so the parser must take the max with
    /// `window_activity` or the fleet's only liveness signal reads as
    /// permanent silence (measured: 60/63 lanes, one of them 34.5h stale).
    #[test]
    fn activity_is_the_max_of_session_and_window() {
        // The REAL line `amux-rust` produced while it was mid-turn, through
        // the REAL parser. `session_activity` had not moved since the session
        // was created 34.5h earlier; `window_activity` was current.
        let line = "amux-rust:1786206640:1786206640:1786330900";
        assert_eq!(
            parse_list_sessions_line(line),
            Some(("amux-rust", Some(1_786_330_900), Some(1_786_206_640))),
            "window activity must win when it is newer — this is the whole fleet's \
             only liveness signal"
        );
        // The other direction still works, and a short line does not panic.
        assert_eq!(
            parse_list_sessions_line("amux-x:200:100:50"),
            Some(("amux-x", Some(200), Some(100)))
        );
        assert_eq!(parse_list_sessions_line("amux-x:200"), Some(("amux-x", Some(200), None)));
        assert_eq!(parse_list_sessions_line(""), None);
    }
}
