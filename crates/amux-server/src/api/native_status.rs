//! Ordered, passive Codex/Claude lifecycle observations. The local spool survives
//! network/server outages; every accepted edge shares the existing report/SSE
//! projection. Observation never sends a prompt or creates a board item.
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
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(crate) fn begin_launch(name: &str, provider: &str) -> std::io::Result<(String, f64)> {
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
    std::fs::read_to_string(root().join(name).join("current.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
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
    let outcome = state
        .store
        .write_async(move |conn| apply(conn, &name, &body, &generation(&name)))
        .await;
    match outcome {
        Ok(out) => Json(json!({"ok":true,"applied":out.applied})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":e.to_string()})),
        )
            .into_response(),
    }
}

/// Recover delivery without another model turn. Replay only complete, atomic
/// files, in sequence; ACK deletion follows the committed database transaction.
pub(crate) fn replay(state: &AppState) {
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
        let current = dir.parent().unwrap_or(dir).join("current.json");
        if state
            .store
            .write(move |conn| {
                let launch = std::fs::read_to_string(current)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or(Value::Null);
                apply(conn, &worker, &body, &launch)
            })
            .is_ok()
        {
            let _ = std::fs::remove_file(path);
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
