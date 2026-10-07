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
    Ok(WriteOutcome {
        applied: true,
        events: vec![PendingEvent {
            entity_type: amux_core::revision::EntityType::Session,
            entity_id: name.to_owned(),
            mutation: amux_core::revision::MutationKind::StatusChanged {
                from: prev["state"].as_str().unwrap_or("").into(),
                to: event["state"].as_str().unwrap_or("").into(),
            },
            payload: Some(report),
        }],
    })
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
    use super::*;

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
