//! Ordered, passive Codex/Claude lifecycle observations. The local spool survives
//! network/server outages; every accepted edge shares the existing report/SSE
//! projection. Observation never sends a prompt or creates a board item.
//!
//! One consumer hangs off an applied `Stop` edge: the turn-end owner-ask
//! classifier (`turn_end::on_turn_end`, AMUX-5234). It is the same consumer the
//! legacy Stop hook drives, it runs detached from this write, and it refuses
//! isolated lanes itself, so an isolated lane's observation stays passive.
use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    crate::config::amux_home().join("status-events")
}

/// Ship the passive observer with the server, not with a one-time install.
/// A worker-token rollout otherwise leaves older installed hooks producing
/// thousands of 403s while the checked-in hook is already correct.
pub(crate) fn ensure_observer(home: &Path) -> std::io::Result<bool> {
    const SCRIPT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/hooks/native-status.py"));
    let path = home.join("native-status.py");
    if std::fs::read(&path).ok().as_deref() == Some(SCRIPT.as_bytes()) { return Ok(false); }
    std::fs::create_dir_all(home)?;
    let tmp = home.join(format!(".native-status-{}.tmp", ulid::Ulid::new()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(SCRIPT.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() { let _ = std::fs::remove_file(&tmp); }
    result?;
    tracing::info!(measured = true, n_considered = 1, verdict = "native_status_observer_adopted",
        "installed passive observer now matches this server's bundled bytes");
    Ok(true)
}
/// The Claude events the observer listens on, as scripts/hooks/
/// install-native-status-hooks.py registers them (keep the two in step).
const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PermissionRequest",
    "PostToolUse", "PreCompact", "PostCompact", "Stop", "Notification", "PostToolUseFailure",
    "StopFailure",
];

/// `shlex.quote`, so the command is byte-identical to what install.sh writes.
fn sh_word(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// Register the observer in Claude's settings when it is missing there, the
/// same entries the installer writes. Returns whether the file was written.
///
/// Only install.sh and the cloud bootstrap/update scripts used to register it,
/// so a host that missed them had no hooks: on the sandbox (2026-10-25) every
/// worker read "unknown" with "hooks are not reaching amux", mtp-demo included
/// while five of its subagents were running. Settings that already carry every
/// entry are left byte-for-byte alone; otherwise only amux's own entries are
/// replaced, never another hook.
pub(crate) fn ensure_claude_hooks(settings: &Path, script: &Path) -> std::io::Result<bool> {
    let command = format!("python3 {} claude", sh_word(&script.to_string_lossy()));
    let mut data: Value = match std::fs::read_to_string(settings) {
        Ok(t) => serde_json::from_str(&t)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", settings.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };
    let ours = |h: &Value| h["command"].as_str().is_some_and(|c| c.contains("native-status.py"));
    let present = |data: &Value, ev: &str| {
        data["hooks"][ev].as_array().is_some_and(|gs| gs.iter().any(|g| {
            g["hooks"].as_array().is_some_and(|hs| hs.iter().any(|h| h["command"] == json!(command)))
        }))
    };
    let total = data["hooks"].as_object().map_or(0, |o| {
        o.values().filter_map(Value::as_array).flatten()
            .filter_map(|g| g["hooks"].as_array()).flatten().filter(|h| ours(h)).count()
    });
    if CLAUDE_HOOK_EVENTS.iter().all(|ev| present(&data, ev)) && total == CLAUDE_HOOK_EVENTS.len() {
        return Ok(false);
    }
    if !data.is_object() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "settings is not a JSON object"));
    }
    if !data["hooks"].is_object() {
        data["hooks"] = json!({});
    }
    let hooks = data["hooks"].as_object_mut().expect("hooks is an object");
    for groups in hooks.values_mut() {
        if let Some(gs) = groups.as_array_mut() {
            for g in gs.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    hs.retain(|h| !ours(h));
                }
            }
            gs.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
        }
    }
    for ev in CLAUDE_HOOK_EVENTS {
        let mut group = json!({"hooks": [{"type": "command", "command": command, "timeout": 3}]});
        if matches!(*ev, "PreToolUse" | "PermissionRequest" | "PostToolUse" | "PostToolUseFailure") {
            group["matcher"] = json!(".*");
        }
        let slot = hooks.entry(ev.to_string()).or_insert_with(|| json!([]));
        if !slot.is_array() {
            *slot = json!([]);
        }
        slot.as_array_mut().expect("array").push(group);
    }
    if let Some(dir) = settings.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = settings.with_extension(format!("status-hook-{}.tmp", ulid::Ulid::new()));
    let result = (|| {
        std::fs::write(&tmp, serde_json::to_string_pretty(&data).unwrap_or_default() + "\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(settings).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&tmp, settings)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(true)
}

/// The tools amux denies every Claude on the host. The Artifact tool publishes to
/// claude.ai, outside amux, where the owner cannot list, serve or turn the page
/// off: on sandbox.amux.io (2026-10-25) PPD-payroll-demo answered with
/// claude.ai/artifact/... and left nothing in its workspace, because that host's
/// settings carried no deny (the owner's Mac denies it by hand).
const CLAUDE_DENIED_TOOLS: &[&str] = &["Artifact", "Artifact(*)"];

/// Add [`CLAUDE_DENIED_TOOLS`] to `permissions.deny` when any is missing; every
/// other key and entry is kept. Returns whether the file was written.
pub(crate) fn ensure_claude_denies(settings: &Path) -> std::io::Result<bool> {
    let mut data: Value = match std::fs::read_to_string(settings) {
        Ok(t) => serde_json::from_str(&t)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {e}", settings.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e),
    };
    let has = |data: &Value, t: &str| data["permissions"]["deny"].as_array().is_some_and(|a| a.iter().any(|v| v == t));
    if CLAUDE_DENIED_TOOLS.iter().all(|t| has(&data, t)) {
        return Ok(false);
    }
    if !data.is_object() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "settings is not a JSON object"));
    }
    if !data["permissions"].is_object() {
        data["permissions"] = json!({});
    }
    if !data["permissions"]["deny"].is_array() {
        data["permissions"]["deny"] = json!([]);
    }
    for t in CLAUDE_DENIED_TOOLS {
        if !has(&data, t) {
            data["permissions"]["deny"].as_array_mut().expect("array").push(json!(t));
        }
    }
    if let Some(dir) = settings.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = settings.with_extension(format!("deny-{}.tmp", ulid::Ulid::new()));
    let result = (|| {
        std::fs::write(&tmp, serde_json::to_string_pretty(&data).unwrap_or_default() + "\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(settings).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o600);
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        }
        std::fs::rename(&tmp, settings)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(true)
}

/// [`ensure_claude_hooks`] for this server's own Claude, logged. Only when the
/// server uses the default home (`~/.amux`): a test server runs under a
/// throwaway AMUX_HOME with the real HOME, and must not point the user's
/// Claude at a temporary observer.
pub(crate) fn ensure_claude_hooks_here() {
    let home = crate::config::amux_home();
    let default_home = std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".amux"));
    if default_home.as_deref() != Some(home.as_path()) {
        return;
    }
    let settings = super::session_verbs::claude_home().join("settings.json");
    match ensure_claude_hooks(&settings, &home.join("native-status.py")) {
        Ok(true) => tracing::warn!(measured = true, n_considered = 1, verdict = "claude_status_hooks_installed",
            settings = %settings.display(),
            "Claude settings did not register the status observer; registered it so workers report their state"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, measured = false, n_considered = 1, verdict = "claude_status_hooks_install_failed",
            settings = %settings.display(), "could not register the status observer; Claude workers will read unknown"),
    }
    match ensure_claude_denies(&settings) {
        Ok(true) => tracing::warn!(measured = true, n_considered = 1, verdict = "claude_tool_denies_installed",
            settings = %settings.display(), denied = ?CLAUDE_DENIED_TOOLS,
            "Claude settings allowed tools amux denies (claude.ai Artifacts); denied them"),
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, measured = false, n_considered = 1, verdict = "claude_tool_denies_install_failed",
            settings = %settings.display(), "could not deny the claude.ai Artifact tool in Claude settings"),
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn begin_launch(name: &str, provider: &str) -> std::io::Result<(String, f64)> {
    ensure_observer(&crate::config::amux_home())?;
    let run = format!("{:032x}", u128::from(ulid::Ulid::new()));
    let ts = crate::config::now_f64();
    let dir = root().join(name);
    std::fs::create_dir_all(&dir)?;
    let temp = dir.join("current.tmp");
    std::fs::write(
        &temp,
        json!({"run_id": run, "provider": provider, "started": ts}).to_string(),
    )?;
    std::fs::rename(temp, dir.join("current.json"))?;
    Ok((run, ts))
}
fn start_from_generation(launch: &Value, fallback: f64) -> f64 {
    launch["started"]
        .as_f64()
        .filter(|ts| ts.is_finite() && *ts > 0.0)
        .unwrap_or(fallback)
}

/// SessionStart occurs before the server finishes confirming startup. The
/// receipt's timestamp is not the beginning of the provider's lifetime.
pub(crate) fn launch_started_at(name: &str, fallback: f64) -> f64 {
    start_from_generation(&generation(name), fallback)
}

pub(crate) fn owns_report(name: &str, report: &Value) -> bool {
    report["native_status"].as_bool() == Some(true)
        && report["run_id"] == generation(name)["run_id"]
}
fn generation(name: &str) -> Value {
    let launch = std::fs::read_to_string(root().join(name).join("current.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    if launch_predates_session(&launch, tmux_created(name)) {
        warn_stale_generation_once(name, &launch);
        return Value::Null;
    }
    launch
}

/// A launch record is only this process's if the server wrote it for the
/// tmux session that is running now. `current.json` is written ONLY by the
/// server's own start (`begin_launch`), after it creates the tmux session, so
/// a record OLDER than the session means something else started the worker
/// (the `amux start` CLI path launches Claude itself and sets no
/// AMUX_STATUS_* vars). Honouring that record made the previous launch's
/// last native report "own" the worker forever: every fresh hook report was
/// discarded (`owns_report`), and `launch_started_at` judged "this life"
/// against a start days old. Measured 2026-09-26: 22 of 32 running workers
/// read status off the screen only, all holding a native SessionEnd from the
/// 20:57 restart a day earlier. Unknown session time (no tmux) keeps the old
/// behaviour.
pub(crate) fn launch_predates_session(launch: &Value, session_created: Option<f64>) -> bool {
    match (launch["started"].as_f64(), session_created) {
        (Some(started), Some(created)) => started + 5.0 < created,
        _ => false,
    }
}

fn warn_stale_generation_once(name: &str, launch: &Value) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let key = format!("{name}:{}", launch["run_id"].as_str().unwrap_or(""));
    let Ok(mut g) = SEEN.lock() else { return };
    if g.get_or_insert_with(HashSet::new).insert(key) {
        tracing::warn!(session = name, run_id = %launch["run_id"].as_str().unwrap_or(""),
            verdict = "native_status_generation_stale", measured = true, n_considered = 1,
            "native status launch record predates this worker's tmux session (started outside the \
             server); ignoring it so hook reports decide status");
    }
}

/// `#{session_created}` for `amux-<name>`, from one `tmux list-sessions`
/// shared by every caller and refreshed at most every 10s (callers include
/// every hook report and every sessions-list build).
fn tmux_created(name: &str) -> Option<f64> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static CACHE: Mutex<Option<(f64, HashMap<String, f64>)>> = Mutex::new(None);
    let now = crate::config::now_f64();
    let mut g = CACHE.lock().ok()?;
    if g.as_ref().is_none_or(|(at, _)| now - at > 10.0) {
        let map = std::process::Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name} #{session_created}"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| {
                        let (n, t) = l.trim().rsplit_once(' ')?;
                        Some((n.to_string(), t.parse::<f64>().ok()?))
                    })
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        *g = Some((now, map));
    }
    g.as_ref()?.1.get(&format!("amux-{name}")).copied()
}

/// Reject replay from an earlier launch/turn and transport reordering. Receipt
/// time is diagnostic only: it must never renew the evidence's freshness.
fn refusal(event: &Value, previous: &Value, launch: &Value, now: f64) -> Option<&'static str> {
    let run = event["run_id"].as_str().unwrap_or("");
    if run.is_empty() || launch["run_id"].as_str() != Some(run) {
        return Some("previous_launch");
    }
    if event["provider"] != launch["provider"] {
        return Some("wrong_provider");
    }
    let seq = event["sequence"].as_u64().unwrap_or(0);
    if seq == 0 {
        return Some("invalid_sequence");
    }
    let ts = event["event_ts"].as_f64().unwrap_or(0.0);
    if !ts.is_finite() || ts < launch["started"].as_f64().unwrap_or(f64::MAX) || ts > now + 5.0 {
        return Some("invalid_event_time");
    }
    if previous["run_id"].as_str() == Some(run) {
        if seq <= previous["sequence"].as_u64().unwrap_or(0) {
            return Some("duplicate_or_reordered");
        }
        if ts < previous["ts"].as_f64().unwrap_or(0.0) {
            return Some("older_observation");
        }
        let turn = event["turn_id"].as_str().unwrap_or("");
        let previous_turn = previous["turn_id"].as_str().unwrap_or("");
        if !turn.is_empty()
            && !previous_turn.is_empty()
            && turn != previous_turn
            && event["event"] != "UserPromptSubmit"
            && event["event"] != "SessionStart"
        {
            return Some("previous_turn");
        }
    }
    if !matches!(
        event["state"].as_str(),
        Some("active" | "idle" | "waiting" | "blocked" | "error")
    ) {
        return Some("invalid_state");
    }
    None
}

fn apply(
    conn: &rusqlite::Connection,
    name: &str,
    event: &Value,
    launch: &Value,
) -> rusqlite::Result<WriteOutcome> {
    super::session_verbs::ensure_fleet_tables(conn)?;
    let mut reports: Value = conn
        .query_row(
            "SELECT value FROM prefs WHERE key='session_reports'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let now = crate::config::now_f64();
    if let Some(reason) = refusal(event, &reports[name], launch, now) {
        tracing::debug!(session=name, verdict="native_status_refused", reason, sequence=?event["sequence"], "ignored status observation");
        return Ok(WriteOutcome {
            applied: false,
            events: vec![],
        });
    }
    let prev = reports[name].clone();
    let mut report = event.clone();
    // The prompt text rides the hook only so `pane_prompts` can record a
    // pane-typed prompt (F8(c)). It must not be copied into the fleet-wide
    // `session_reports` blob or every status event row.
    if let Some(map) = report.as_object_mut() {
        map.remove("prompt");
    }
    report["ts"] = event["event_ts"].clone();
    report["received_at"] = json!(now);
    report["origin"] = json!(name);
    if prev["run_id"] == event["run_id"] {
        for key in ["tokens", "subagents"] {
            if let Some(v) = prev.get(key) {
                report[key] = v.clone();
            }
        }
    }
    if event["model"].as_str().unwrap_or("").is_empty() {
        report["model"] = prev["model"].clone();
    }
    // Missing turn IDs on tool hooks must not discard a known turn boundary.
    if event["turn_id"].as_str().unwrap_or("").is_empty() && prev["run_id"] == event["run_id"] {
        report["turn_id"] = prev["turn_id"].clone();
    }
    reports[name] = report.clone();
    conn.execute("INSERT INTO prefs(key,value) VALUES('session_reports',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [reports.to_string()])?;
    conn.execute("INSERT INTO session_events(ts,session,type,data,source) VALUES(?1,?2,'session.native_status',?3,'native-hook')",
        rusqlite::params![now, name, report.to_string()])?;
    if !super::session_verbs::session_is_isolated(name) {
        crate::db::board_store::refresh_lease_heartbeat(conn, name, now as i64)?;
    }
    // An owner `/clear` is "stop what you were going to do". Queued
    // needs-input auto-approvals (`ni-auto-*`, `ni-back-*`) were otherwise
    // delivered one per turn AFTER the clear, so the lane kept resuming work
    // the owner had just cleared (amux-cloud, 2026-09-30: eight approvals
    // from one tick, re-arriving after every /clear and Ctrl-C). The card
    // note and the ledger row still record the approval; only the push is
    // dropped. Owner-typed and other system rows are untouched.
    if event["event"] == "SessionStart" && event["start_source"] == "clear" {
        let dropped = conn.execute(
            "DELETE FROM steering_queue WHERE session=?1 AND (id LIKE 'ni-auto-%' OR id LIKE 'ni-back-%')",
            [name],
        )?;
        tracing::info!(session=name, verdict="steering_cleared_on_clear", dropped, "owner /clear dropped queued needs-input auto-approvals");
    }
    super::sessions_legacy::invalidate_sessions_runtime_cache();
    tracing::debug!(session=name, verdict="native_status_applied", event=?event["event"], sequence=?event["sequence"], "applied provider lifecycle observation");
    let (mutation, payload) = session_event(
        prev["state"].as_str().unwrap_or(""),
        event["state"].as_str().unwrap_or(""),
        report,
    );
    Ok(WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: amux_core::revision::EntityType::Session,
            entity_id: name.to_owned(),
            mutation,
            payload,
        }],
    })
}

/// The state event for one lifecycle observation. A real transition carries
/// the report; an observation that leaves the state unchanged is a bare
/// `Updated` (the sessions refresh still fires, since SSE invalidates on any
/// Session event). amux.db, 2026-10-15: 87K of the newest 200K state events
/// were "status_changed active -> active" with a ~5 KB report each, the bulk
/// of `_amux_state_events`' 6.7 GB, and the dashboard reads no payload of a
/// `state` message.
fn session_event(from: &str, to: &str, report: Value) -> (amux_core::revision::MutationKind, Option<Value>) {
    if from == to {
        (amux_core::revision::MutationKind::Updated, None)
    } else {
        (
            amux_core::revision::MutationKind::StatusChanged { from: from.into(), to: to.into() },
            Some(report),
        )
    }
}

pub(crate) fn history(conn: &rusqlite::Connection, name: &str) -> Vec<Value> {
    let Ok(mut statement) = conn.prepare("SELECT data FROM session_events WHERE session=?1 AND type='session.native_status' ORDER BY id DESC LIMIT 50") else { return vec![]; };
    let Ok(rows) = statement.query_map([name], |r| r.get::<_, String>(0)) else {
        return vec![];
    };
    rows.flatten()
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect()
}

pub(crate) async fn post(state: &AppState, name: &str, body: &Value) -> Response {
    if !valid_name(name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid worker"})),
        )
            .into_response();
    }
    let name = name.to_owned();
    let body = body.clone();
    let prompt = submitted_prompt(&body);
    let (worker, event) = (name.clone(), body.clone());
    let outcome = state
        .store
        .write_async(move |conn| apply(conn, &name, &body, &generation(&name)))
        .await;
    match outcome {
        Ok(out) => {
            // Only an APPLIED edge: a duplicate or replayed observation of the
            // same submit must not write the prompt twice.
            if out.applied {
                if let Some(prompt) = prompt {
                    super::pane_prompts::record(state, &worker, &prompt).await;
                }
                if event["event"] == "Stop" {
                    let sid = event["session_id"].as_str().unwrap_or("").to_string();
                    crate::db::interactions::spawn(super::turn_end::on_turn_end(
                        state.clone(),
                        worker,
                        sid,
                    ));
                }
            }
            Json(json!({"ok":true,"applied":out.applied})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
    }
}

/// The prompt a UserPromptSubmit observation carries, if any. Older hooks and
/// the chat adapter send none, which is simply "nothing to record".
fn submitted_prompt(event: &Value) -> Option<String> {
    (event["event"] == "UserPromptSubmit")
        .then(|| event["prompt"].as_str())
        .flatten()
        .filter(|p| !p.trim().is_empty())
        .map(str::to_owned)
}

/// Recover delivery without another model turn. Replay only complete, atomic
/// files, in sequence; ACK deletion follows the committed database transaction.
pub(crate) fn replay(state: &AppState) {
    if let Err(error) = ensure_observer(&crate::config::amux_home()) {
        tracing::warn!(%error, measured = false, n_considered = 1, verdict = "native_status_observer_adoption_failed",
            "observer installation drift could not be repaired; retained status spool still replays");
    }
    observer_error_census(&crate::config::amux_home());
    ensure_claude_hooks_here();
    let Ok(workers) = std::fs::read_dir(root()) else {
        return;
    };
    for worker in workers.flatten().take(2048) {
        let name = worker.file_name().to_string_lossy().into_owned();
        if !valid_name(&name) {
            continue;
        }
        let launch = generation(&name);
        let Some(run) = launch["run_id"].as_str() else {
            continue;
        };
        if !valid_name(run) {
            continue;
        }
        replay_run(state, &name, &worker.path().join(run));
    }
}

/// Byte offset already counted in `status-observer-errors.jsonl`, per file.
static OBSERVER_ERRORS_SEEN: std::sync::Mutex<Option<(PathBuf, u64)>> = std::sync::Mutex::new(None);

/// Count what the passive observer wrote to `status-observer-errors.jsonl`
/// since the last pass, by `error_type` and worker, and WARN when it is not
/// zero.
///
/// The hook appended 16,081 lines there (9,065 TimeoutError, 5,131 HTTPError
/// on 2026-10-18) and nothing read the file, so ~690 hook timeouts in one lane
/// surfaced only in a transcript review. The hook now also writes
/// `lock_budget_exceeded` (it gave up waiting for the spool lock rather than
/// hold the provider's turn) and `slow_hook`. The first pass of a process
/// reports the backlog once, then only new lines are counted.
pub(crate) fn observer_error_census(home: &Path) -> Option<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let path = home.join("status-observer-errors.jsonl");
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut seen = OBSERVER_ERRORS_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let (backlog, from) = match seen.as_ref() {
        Some((p, off)) if *p == path && *off <= len => (false, *off),
        _ => (true, 0),
    };
    if len == from {
        *seen = Some((path, len));
        return None;
    }
    // A backlog read is bounded to the newest 4 MiB; its first, cut line is
    // skipped below.
    const BACKLOG_CAP: u64 = 4 << 20;
    let cut = backlog && len > BACKLOG_CAP;
    let from = if cut { len - BACKLOG_CAP } else { from };
    let mut buf = String::new();
    let read = std::fs::File::open(&path).and_then(|mut f| {
        f.seek(SeekFrom::Start(from))?;
        // A partial trailing line is left for the next pass.
        f.take(len - from).read_to_string(&mut buf)
    });
    if let Err(error) = read {
        tracing::warn!(%error, measured = false, n_considered = 0, verdict = "native_status_observer_errors_unreadable",
            "could not read the passive observer's error log");
        return None;
    }
    let complete = buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let mut by_type: std::collections::BTreeMap<String, u64> = Default::default();
    let mut by_worker: std::collections::BTreeMap<String, u64> = Default::default();
    let mut n = 0u64;
    for line in buf[..complete].lines().skip(usize::from(cut)) {
        let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        n += 1;
        *by_type.entry(v["error_type"].as_str().unwrap_or("unparsed").to_string()).or_default() += 1;
        *by_worker.entry(v["worker"].as_str().unwrap_or("").to_string()).or_default() += 1;
    }
    *seen = Some((path, from + complete as u64));
    if n == 0 {
        return None;
    }
    let mut workers: Vec<(String, u64)> = by_worker.into_iter().collect();
    workers.sort_by_key(|w| std::cmp::Reverse(w.1));
    workers.truncate(8);
    let summary = json!({"errors": n, "by_type": by_type, "top_workers": workers, "backlog": backlog});
    if backlog {
        tracing::info!(measured = true, n_considered = n, verdict = "native_status_observer_errors_backlog",
            summary = %summary, "passive observer error log on disk at startup, counted once");
    } else {
        tracing::warn!(measured = true, n_considered = n, verdict = "native_status_observer_errors",
            summary = %summary, "passive status hook recorded errors since the last pass (timeouts, lock budget overruns, slow hooks)");
    }
    Some(summary)
}

fn replay_run(state: &AppState, name: &str, dir: &Path) {
    let Ok(files) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = files
        .flatten()
        .map(|f| f.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    files.sort();
    for path in files.into_iter().take(512) {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(body) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let worker = name.to_owned();
        let prompt = submitted_prompt(&body);
        let current = dir.parent().unwrap_or(dir).join("current.json");
        if let Ok(reply) = state.store.write(move |conn| {
            let launch = std::fs::read_to_string(current)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null);
            apply(conn, &worker, &body, &launch)
        }) {
            // A prompt submitted while the server was down still belongs in
            // the ledger once the spool is replayed (F8(c)).
            if let (true, Some(prompt), Ok(rt)) =
                (reply.applied, prompt, tokio::runtime::Handle::try_current())
            {
                let (st, n) = (state.clone(), name.to_owned());
                rt.spawn(async move { super::pane_prompts::record(&st, &n, &prompt).await });
            }
            let _ = std::fs::remove_file(path);
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    /// gs12 review: the hook's error log had 16K lines and no reader.
    #[test]
    fn observer_errors_are_counted_once_by_type_then_only_new_lines() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status-observer-errors.jsonl");
        let line = |t: &str, w: &str| format!("{}\n", serde_json::json!({"at":1.0,"worker":w,"error_type":t}));
        std::fs::write(&path, line("TimeoutError", "a") + &line("TimeoutError", "b")).unwrap();
        let first = super::observer_error_census(dir.path()).expect("backlog counted");
        assert_eq!(first["backlog"], true);
        assert_eq!(first["errors"], 2);
        assert_eq!(first["by_type"]["TimeoutError"], 2);
        assert!(super::observer_error_census(dir.path()).is_none(), "nothing new, nothing reported");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(line("lock_budget_exceeded", "a").as_bytes()).unwrap();
        f.write_all(b"{\"error_type\":\"partial").unwrap();
        let next = super::observer_error_census(dir.path()).expect("new line counted");
        assert_eq!(next["backlog"], false);
        assert_eq!(next["errors"], 1, "a partial trailing line waits for the next pass");
        assert_eq!(next["by_type"]["lock_budget_exceeded"], 1);
        f.write_all(b"\"}\n").unwrap();
        assert_eq!(super::observer_error_census(dir.path()).unwrap()["by_type"]["partial"], 1);
    }

    /// An unchanged state is recorded as a bare update; only a real transition
    /// carries the report.
    #[test]
    fn an_unchanged_state_records_no_transition_and_no_report() {
        let report = serde_json::json!({"state":"active","detail":"x".repeat(5000)});
        let (m, p) = super::session_event("active", "active", report.clone());
        assert!(matches!(m, amux_core::revision::MutationKind::Updated));
        assert!(p.is_none());
        let (m, p) = super::session_event("active", "idle", report);
        assert!(matches!(m, amux_core::revision::MutationKind::StatusChanged { ref from, ref to } if from == "active" && to == "idle"));
        assert!(p.is_some());
    }

    use super::*;

    #[test]
    fn the_artifact_tool_is_denied_once_and_other_permissions_kept() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, r#"{"model":"opus","permissions":{"allow":["Bash"],"deny":["WebFetch"]}}"#).unwrap();
        assert!(ensure_claude_denies(&settings).unwrap());
        let d: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(d["model"], "opus");
        assert_eq!(d["permissions"]["allow"], json!(["Bash"]));
        assert_eq!(d["permissions"]["deny"], json!(["WebFetch", "Artifact", "Artifact(*)"]));
        let written = std::fs::read(&settings).unwrap();
        assert!(!ensure_claude_denies(&settings).unwrap(), "already denied: not rewritten");
        assert_eq!(std::fs::read(&settings).unwrap(), written);
        let fresh = dir.path().join("none").join("settings.json");
        assert!(ensure_claude_denies(&fresh).unwrap());
        assert!(std::fs::read_to_string(&fresh).unwrap().contains("Artifact(*)"));
    }

    #[test]
    fn claude_hooks_are_registered_once_and_other_hooks_kept() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let script = Path::new("/home/amux/.amux/native-status.py");
        std::fs::write(&settings, r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"python3 guard.py"}]},{"hooks":[{"type":"command","command":"python3 /old/native-status.py claude"}]}]}}"#).unwrap();
        assert!(ensure_claude_hooks(&settings, script).unwrap());
        let d: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(d["model"], "opus");
        let pre = d["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre[0]["hooks"][0]["command"], "python3 guard.py", "another hook is kept");
        assert!(!d.to_string().contains("/old/"), "a stale observer path is replaced");
        for ev in CLAUDE_HOOK_EVENTS {
            assert!(d["hooks"][ev].to_string().contains("python3 /home/amux/.amux/native-status.py claude"), "{ev}");
        }
        let written = std::fs::read(&settings).unwrap();
        assert!(!ensure_claude_hooks(&settings, script).unwrap(), "complete settings are not rewritten");
        assert_eq!(std::fs::read(&settings).unwrap(), written);
        let fresh = dir.path().join("none").join("settings.json");
        assert!(ensure_claude_hooks(&fresh, Path::new("/a path/native-status.py")).unwrap());
        assert!(std::fs::read_to_string(&fresh).unwrap().contains("python3 '/a path/native-status.py' claude"));
    }

    #[test]
    fn bundled_observer_repairs_drift_atomically_without_rewriting_current_bytes() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("native-status.py");
        std::fs::write(&path, "obsolete observer").unwrap();
        assert!(ensure_observer(home.path()).unwrap());
        let bytes = std::fs::read(&path).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("X-Amux-Worker-Token"));
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!ensure_observer(home.path()).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(ensure_observer(home.path()).is_err());
        assert!(path.is_dir());
        assert!(!std::fs::read_dir(home.path()).unwrap().any(|p| p.unwrap().file_name().to_string_lossy().starts_with(".native-status-")));
    }
    fn event(seq: u64, ts: f64, turn: &str, kind: &str) -> Value {
        json!({"run_id":"run","provider":"codex","sequence":seq,"event_ts":ts,"state":"active","turn_id":turn,"event":kind})
    }
    #[test]
    fn native_replay_recovers_all_edges_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&tmp.path().join("test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let folder = tmp.path().join("worker/run");
        std::fs::create_dir_all(&folder).unwrap();
        let now = crate::config::now_f64();
        std::fs::write(
            folder.parent().unwrap().join("current.json"),
            json!({"run_id":"run","provider":"codex","started":now-20.0}).to_string(),
        )
        .unwrap();
        for (seq, kind, status) in [
            (1, "UserPromptSubmit", "active"),
            (2, "PermissionRequest", "blocked"),
            (3, "PostToolUse", "active"),
            (4, "Stop", "idle"),
        ] {
            let mut e = event(seq, now - 10.0 + seq as f64, "t", kind);
            e["state"] = json!(status);
            std::fs::write(folder.join(format!("{seq:020}.json")), e.to_string()).unwrap();
        }
        // These files stand for a network outage across the entire turn.
        replay_run(&state, "native-hermetic-test", &folder);
        let conn = state.store.read().unwrap();
        let count: u64 = conn
            .query_row(
                "SELECT count(*) FROM session_events WHERE type='session.native_status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 4);
        let reports: String = conn
            .query_row(
                "SELECT value FROM prefs WHERE key='session_reports'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let report = &serde_json::from_str::<Value>(&reports).unwrap()["native-hermetic-test"];
        assert_eq!(report["state"], "idle");
        assert_eq!(report["sequence"], 4);
        assert!(report["ts"].as_f64().unwrap() < report["received_at"].as_f64().unwrap());
        drop(conn);
        // A lost ACK repeats delivery; it cannot regress state or duplicate history.
        std::fs::write(
            folder.join("00000000000000000001.json"),
            event(1, now - 9.0, "t", "UserPromptSubmit").to_string(),
        )
        .unwrap();
        replay_run(&state, "native-hermetic-test", &folder);
        assert_eq!(std::fs::read_dir(folder).unwrap().count(), 0);
        let conn = state.store.read().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM session_events WHERE type='session.native_status'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            4
        );
    }

    #[test]
    fn the_submitted_prompt_is_read_from_the_submit_edge_and_never_stored_in_status() {
        let mut e = event(1, 100.0, "t", "UserPromptSubmit");
        assert_eq!(submitted_prompt(&e), None, "older hooks send no prompt");
        e["prompt"] = json!("land the PRs");
        assert_eq!(submitted_prompt(&e).as_deref(), Some("land the PRs"));
        let mut other = event(2, 101.0, "t", "PostToolUse");
        other["prompt"] = json!("land the PRs");
        assert_eq!(submitted_prompt(&other), None);

        let tmp = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&tmp.path().join("test.db")).unwrap();
        let now = crate::config::now_f64();
        let launch = json!({"run_id":"run","provider":"codex","started":now-20.0});
        let mut e = event(1, now - 1.0, "t", "UserPromptSubmit");
        e["prompt"] = json!("a private instruction");
        store
            .write(move |c| apply(c, "native-prompt-strip", &e, &launch))
            .unwrap();
        let conn = store.read().unwrap();
        let reports: String = conn
            .query_row(
                "SELECT value FROM prefs WHERE key='session_reports'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let events: String = conn
            .query_row(
                "SELECT data FROM session_events WHERE session='native-prompt-strip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!reports.contains("a private instruction"));
        assert!(!events.contains("a private instruction"));
    }

    #[test]
    fn an_owner_clear_drops_only_queued_auto_approvals() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&tmp.path().join("test.db")).unwrap();
        let now = crate::config::now_f64();
        let launch = json!({"run_id":"run","provider":"claude","started":now-20.0});
        store
            .write(|c| {
                super::super::session_verbs::ensure_fleet_tables(c)?;
                for id in ["ni-auto-a", "ni-back-b", "steer-owner"] {
                    c.execute(
                        "INSERT INTO steering_queue(id,session,text,queued_at) VALUES(?1,'native-clear',?1,1.0)",
                        [id],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let left = |store: &crate::db::Store| -> Vec<String> {
            let conn = store.read().unwrap();
            let mut s = conn.prepare("SELECT id FROM steering_queue WHERE session='native-clear' ORDER BY id").unwrap();
            s.query_map([], |r| r.get(0)).unwrap().flatten().collect()
        };
        // A fresh start is not a clear: nothing is dropped.
        let mut start = event(1, now - 2.0, "", "SessionStart");
        start["provider"] = json!("claude");
        start["state"] = json!("idle");
        start["start_source"] = json!("startup");
        let l = launch.clone();
        store.write(move |c| apply(c, "native-clear", &start, &l)).unwrap();
        assert_eq!(left(&store), ["ni-auto-a", "ni-back-b", "steer-owner"]);
        let mut clear = event(2, now - 1.0, "", "SessionStart");
        clear["provider"] = json!("claude");
        clear["state"] = json!("idle");
        clear["start_source"] = json!("clear");
        store.write(move |c| apply(c, "native-clear", &clear, &launch)).unwrap();
        assert_eq!(left(&store), ["steer-owner"], "owner-typed rows survive a /clear");
    }

    #[test]
    fn native_start_edge_precedes_start_confirmation_without_becoming_stale() {
        let start = start_from_generation(&json!({"started":100.0}), 104.0);
        assert_eq!(start, 100.0);
        assert!(super::super::sessions_legacy::report_applies(
            "idle", 101.0, start, 105.0
        ));
        assert_eq!(start_from_generation(&Value::Null, 104.0), 104.0);
        assert!(!super::super::sessions_legacy::report_applies(
            "idle", 99.0, start, 105.0
        ));
    }

    #[test]
    fn a_launch_record_older_than_the_session_is_not_this_process() {
        let launch = json!({"run_id": "r1", "started": 1000.0});
        // CLI start: tmux session created long after the server's last launch.
        assert!(launch_predates_session(&launch, Some(90_000.0)));
        // Server start: tmux session first, then begin_launch (same second or later).
        assert!(!launch_predates_session(&launch, Some(1000.0)));
        assert!(!launch_predates_session(&launch, Some(996.0)));
        // A server restart inside an older tmux session keeps a newer launch.
        assert!(!launch_predates_session(&launch, Some(500.0)));
        // Unknown session time or no record: no change from before.
        assert!(!launch_predates_session(&launch, None));
        assert!(!launch_predates_session(&Value::Null, Some(90_000.0)));
    }

    #[test]
    fn native_ordering_and_generation() {
        let launch = json!({"run_id":"run","provider":"codex","started":100.0});
        let mut prev = event(3, 102.0, "b", "UserPromptSubmit");
        prev["ts"] = json!(102.0);
        assert_eq!(
            refusal(&event(4, 103.0, "b", "PostToolUse"), &prev, &launch, 104.0),
            None
        );
        for seq in [1, 2, 3] {
            assert_eq!(
                refusal(&event(seq, 103.0, "b", "Stop"), &prev, &launch, 104.0),
                Some("duplicate_or_reordered")
            );
        }
        assert_eq!(
            refusal(&event(4, 103.0, "a", "Stop"), &prev, &launch, 104.0),
            Some("previous_turn")
        );
        assert_eq!(
            refusal(&event(4, 101.0, "b", "Stop"), &prev, &launch, 104.0),
            Some("older_observation")
        );
        assert_eq!(
            refusal(&event(4, 99.0, "b", "Stop"), &prev, &launch, 104.0),
            Some("invalid_event_time")
        );
        assert_eq!(
            refusal(&event(4, 200.0, "b", "Stop"), &prev, &launch, 104.0),
            Some("invalid_event_time")
        );
        let restarted = json!({"run_id":"new","provider":"codex","started":103.0});
        assert_eq!(
            refusal(&event(4, 103.0, "b", "Stop"), &prev, &restarted, 104.0),
            Some("previous_launch")
        );
    }
}
