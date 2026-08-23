//! /api/metrics + /api/debug/fleet (Phase 9, Invariants 34/40).
//!
//! Queue depth is a health signal (Invariant 34): a growing command queue
//! or dead-letter count is the fleet telling you delivery is failing, and
//! it must be readable where people already look (ethos rule 4) — one
//! endpoint, no log spelunking.

use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;
use std::path::PathBuf;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(metrics))
        .route("/fleet", axum::routing::get(fleet))
        .route("/replay", axum::routing::get(replay))
}

/// GET /api/metrics/replay — audit replay (RR-0111a): fold the event journal
/// to HEAD and compare against the live tables. Divergences come back NAMED
/// (entity + fields + both values), horizon entities are reported instead of
/// fabricated, and every list cap announces itself in the body.
async fn replay(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(crate::db::replay::verify_replay(&conn)?)
    })
    .await;
    let report = match joined {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    match serde_json::to_value(&report) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap_or(-1)
}

// ---- system metrics (shell-command based, matching Python's non-psutil fallback) ----

fn cmd_output(program: &str, args: &[&str]) -> Option<String> {
    let resolved = resolve_bin(program);
    std::process::Command::new(resolved)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// launchd gives a minimal PATH; resolve common binaries to full paths.
fn resolve_bin(name: &str) -> &str {
    match name {
        "sysctl" => "/usr/sbin/sysctl",
        "vm_stat" => "/usr/bin/vm_stat",
        "hostname" => "/bin/hostname",
        "df" => "/bin/df",
        "ps" => "/bin/ps",
        "pgrep" => "/usr/bin/pgrep",
        "tmux" => {
            if std::path::Path::new("/usr/local/bin/tmux").exists() {
                "/usr/local/bin/tmux"
            } else if std::path::Path::new("/opt/homebrew/bin/tmux").exists() {
                "/opt/homebrew/bin/tmux"
            } else {
                "tmux"
            }
        }
        other => other,
    }
}

fn collect_system_metrics() -> serde_json::Value {
    let hostname = cmd_output("hostname", &[]).unwrap_or_default().trim().to_string();
    let mut sys = serde_json::Map::new();
    sys.insert("hostname".into(), json!(hostname));
    sys.insert("psutil".into(), json!(false));

    // Load average
    if let Some(sysctl_out) = cmd_output("sysctl", &["-n", "vm.loadavg"]) {
        // Format: "{ 1.23 4.56 7.89 }"
        let nums: Vec<f64> = sysctl_out
            .trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace())
            .split_whitespace()
            .filter_map(|s| s.parse::<f64>().ok())
            .map(|v| (v * 100.0).round() / 100.0)
            .collect();
        if !nums.is_empty() {
            sys.insert("load_avg".into(), json!(nums));
        }
    }
    // CPU count
    if let Some(ncpu_s) = cmd_output("sysctl", &["-n", "hw.logicalcpu"]) {
        if let Ok(n) = ncpu_s.trim().parse::<u64>() {
            sys.insert("cpu_count".into(), json!(n));
        }
    }

    // RAM via sysctl + vm_stat (macOS)
    if cfg!(target_os = "macos") {
        if let (Some(memsize_s), Some(vmstat_s)) = (
            cmd_output("sysctl", &["-n", "hw.memsize"]),
            cmd_output("vm_stat", &[]),
        ) {
            if let Ok(total_bytes) = memsize_s.trim().parse::<u64>() {
                let page_size: u64 = cmd_output("sysctl", &["-n", "hw.pagesize"])
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(16384);
                let mut pages = std::collections::HashMap::new();
                for line in vmstat_s.lines() {
                    if let Some(rest) = line.strip_prefix("Pages ") {
                        if let Some((key, val)) = rest.split_once(':') {
                            if let Ok(n) = val.trim().trim_end_matches('.').parse::<u64>() {
                                pages.insert(key.trim().to_string(), n * page_size);
                            }
                        }
                    }
                }
                let free = pages.get("free").unwrap_or(&0)
                    + pages.get("speculative").unwrap_or(&0);
                let used = total_bytes.saturating_sub(free);
                sys.insert(
                    "ram_total_mb".into(),
                    json!((total_bytes as f64 / 1_048_576.0 * 10.0).round() / 10.0),
                );
                sys.insert(
                    "ram_used_mb".into(),
                    json!((used as f64 / 1_048_576.0 * 10.0).round() / 10.0),
                );
                sys.insert(
                    "ram_percent".into(),
                    json!(if total_bytes > 0 {
                        (used as f64 / total_bytes as f64 * 1000.0).round() / 10.0
                    } else {
                        0.0
                    }),
                );
            }
        }
        // Swap
        if let Some(swap_s) = cmd_output("sysctl", &["-n", "vm.swapusage"]) {
            fn parse_mb(s: &str) -> Option<f64> {
                s.trim().strip_suffix('M').and_then(|n| n.trim().parse().ok())
            }
            let parts: Vec<&str> = swap_s.split("  ").collect();
            let mut total_mb = 0.0f64;
            let mut used_mb = 0.0f64;
            for part in &parts {
                if let Some(rest) = part.trim().strip_prefix("total = ") {
                    total_mb = parse_mb(rest).unwrap_or(0.0);
                } else if let Some(rest) = part.trim().strip_prefix("used = ") {
                    used_mb = parse_mb(rest).unwrap_or(0.0);
                }
            }
            sys.insert("swap_total_mb".into(), json!((total_mb * 10.0).round() / 10.0));
            sys.insert("swap_used_mb".into(), json!((used_mb * 10.0).round() / 10.0));
        }
        // Uptime
        if let Some(bt) = cmd_output("sysctl", &["-n", "kern.boottime"]) {
            if let Some(sec_str) = bt.split("sec = ").nth(1).and_then(|s| s.split(',').next()) {
                if let Ok(boot_sec) = sec_str.trim().parse::<i64>() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    sys.insert("uptime_seconds".into(), json!(now - boot_sec));
                }
            }
        }
    }

    // Disk, measured on the volume that actually holds the user's data.
    //
    // This used to run `df -k /` and it was wrong on every modern macOS: `/` is
    // the SEALED READ-ONLY system snapshot (~11GB, essentially static), while
    // everything a user owns lives on `/System/Volumes/Data`. So the gauge read
    // 0.6% used on 2026-08-16 while the data volume was 90% full and writes
    // were failing — a green light on a machine in trouble, which is worse than
    // no gauge because it is trusted (ethos rule 7). Asking statfs about $HOME
    // asks about the right volume by construction, with no mount point to keep
    // in step. Same helper the reclaim scanner reports free space with, so the
    // two can never disagree.
    if let Some((free, total)) = super::reclaim::df_bytes(&super::reclaim::home_dir()) {
        let used = total.saturating_sub(free);
        let pct = if total > 0 {
            used as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        sys.insert(
            "disk_total_gb".into(),
            json!((total as f64 / 1_073_741_824.0 * 10.0).round() / 10.0),
        );
        sys.insert(
            "disk_used_gb".into(),
            json!((used as f64 / 1_073_741_824.0 * 10.0).round() / 10.0),
        );
        sys.insert(
            "disk_free_gb".into(),
            json!((free as f64 / 1_073_741_824.0 * 10.0).round() / 10.0),
        );
        sys.insert("disk_percent".into(), json!((pct * 10.0).round() / 10.0));
        // The comment that used to sit here claimed this was "self-announcing:
        // visible in a log sweep without anyone opening the dashboard". It was
        // not, on both counts, and the claim is why nobody re-checked it
        // (DESKT-21):
        //
        //   1. `collect_system_metrics` runs ONLY when something requests
        //      /api/metrics. Nothing polls it. Measured 2026-08-23: across
        //      36 hours of server-rs.log the string "disk critically full"
        //      appeared ZERO times, at 91.6% full the whole time — and appeared
        //      exactly once, immediately, the moment a human curled the
        //      endpoint. A signal that requires you to already be looking is
        //      the thing this comment promised it was not.
        //   2. `pct >= 90.0` is BELOW this machine's healthy baseline. The data
        //      volume sits at 91.6% with 156 GB free and nothing wrong, so the
        //      moment anything did start polling, this would fire on every
        //      call forever — a detector reporting that the disk exists
        //      (ethos rule 7).
        //
        // Fixing only the cadence would have converted a silent check into
        // continuous noise, so the threshold had to move first. The verdict now
        // comes from `health::disk_state`, which is thresholded on ABSOLUTE free
        // space (one cargo target tree is 10-15 GB) rather than a percentage of
        // a 1.8 TB volume. Sharing that one function is deliberate: a view must
        // share the predicate of the mechanism it describes, or the two drift.
        //
        // The continuously-polled home for this is /health, which now carries
        // `disk` beside `mem`. This stays as the metrics-surface mirror.
        let free_gb = free as f64 / 1_073_741_824.0;
        match crate::api::health::disk_state(Some(free_gb)) {
            "critical" => tracing::warn!(
                disk_percent = pct,
                free_gb,
                state = "critical",
                "disk critically full — under 10 GB free is less than one cargo target \
                 tree; builds and DB writes will start failing (DESKT-21)"
            ),
            "warn" => tracing::warn!(
                disk_percent = pct,
                free_gb,
                state = "warn",
                "disk headroom low — under 50 GB free, a few builds from the 2026-08-10 \
                 ENOSPC (DESKT-21)"
            ),
            _ => {}
        }
    }

    serde_json::Value::Object(sys)
}

fn sessions_dir() -> PathBuf {
    let home = std::env::var("AMUX_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
    });
    home.join("sessions")
}

fn memory_dir() -> PathBuf {
    let home = std::env::var("AMUX_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
    });
    home.join("memory")
}

fn collect_session_metrics() -> Vec<serde_json::Value> {
    let sdir = sessions_dir();
    let mdir = memory_dir();
    let entries = match std::fs::read_dir(&sdir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    // Discover tmux sessions + pane PIDs
    let tmux_pids = tmux_session_pids();

    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = fname.strip_suffix(".env") else { continue };
        if name.contains(".meta") {
            continue;
        }
        // Check archived
        let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
        if content.lines().any(|l| l.trim() == "CC_ARCHIVED=1") {
            continue;
        }

        let tmux_name = format!("amux-{name}");
        let active = tmux_pids.contains_key(&tmux_name);
        let mut s = json!({
            "name": name,
            "pids": [],
            "cpu_percent": 0.0,
            "rss_mb": 0.0,
            "memory_file_kb": 0.0,
            "tokens_today": 0,
            "last_active": null,
            "active": active,
        });

        // Memory file size
        let mem_file = mdir.join(format!("{name}.md"));
        if let Ok(meta) = std::fs::metadata(&mem_file) {
            s["memory_file_kb"] = json!((meta.len() as f64 / 1024.0 * 10.0).round() / 10.0);
        }

        // Process stats: find all descendant PIDs then ps them
        if active {
            if let Some(pane_pid) = tmux_pids.get(&tmux_name) {
                let mut all_pids = vec![*pane_pid];
                collect_descendants(*pane_pid, &mut all_pids);
                let pid_list = all_pids
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                if let Some(ps_out) =
                    cmd_output("ps", &["-o", "pid=,rss=,pcpu=", "-p", &pid_list])
                {
                    let mut pids = Vec::new();
                    let mut total_rss = 0u64;
                    let mut total_cpu = 0.0f64;
                    for line in ps_out.lines() {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 3 {
                            if let Ok(pid) = parts[0].parse::<u64>() {
                                pids.push(pid);
                            }
                            total_rss += parts[1].parse::<u64>().unwrap_or(0);
                            total_cpu += parts[2].parse::<f64>().unwrap_or(0.0);
                        }
                    }
                    s["pids"] = json!(pids);
                    s["rss_mb"] = json!((total_rss as f64 / 1024.0 * 10.0).round() / 10.0);
                    s["cpu_percent"] = json!((total_cpu * 10.0).round() / 10.0);
                }
            }
        }

        sessions.push(s);
    }
    sessions
}

fn collect_descendants(pid: u64, out: &mut Vec<u64>) {
    if let Some(pgrep_out) = cmd_output("pgrep", &["-P", &pid.to_string()]) {
        for line in pgrep_out.lines() {
            if let Ok(child) = line.trim().parse::<u64>() {
                out.push(child);
                collect_descendants(child, out);
            }
        }
    }
}

fn tmux_session_pids() -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
    let Some(out) = cmd_output(
        "tmux",
        &["list-sessions", "-F", "#{session_name} #{session_id}"],
    ) else {
        return map;
    };
    for line in out.lines() {
        let name = line.split_whitespace().next().unwrap_or("").to_string();
        if !name.starts_with("amux-") {
            continue;
        }
        // Get pane PID for this session.
        //
        // pane_target(), not a hand-spelled `={name}`: this is a PANE-level
        // command, and the helper's trailing colon is load-bearing — `={name}`
        // names the session, `={name}:` names its ACTIVE WINDOW. Without it
        // `list-panes` can report a different window's pane, so the pid we
        // cache here would belong to the wrong process.
        //
        // Caught by tests/tmux_target_audit.rs, which exists precisely because
        // a non-exact or wrong-level `-t` lands in a sibling session's pane
        // whenever the exact session is briefly absent.
        // Bound as `pt` rather than inlined: the audit matches the -t argument
        // TEXTUALLY against the sanctioned binding names, so an inline call —
        // even the correct one — reads as hand-spelled. Keeping the convention
        // is what lets a grep-shaped check stay reliable.
        let pt = crate::backend::tmux::pane_target(&name);
        if let Some(pane_out) = cmd_output(
            "tmux",
            &["list-panes", "-t", &pt, "-F", "#{pane_pid}"],
        ) {
            if let Some(pid_s) = pane_out.lines().next() {
                if let Ok(pid) = pid_s.trim().parse::<u64>() {
                    map.insert(name, pid);
                }
            }
        }
    }
    map
}

fn collect_server_metrics(started: std::time::Instant) -> serde_json::Value {
    json!({
        "pid": std::process::id(),
        "uptime_seconds": started.elapsed().as_secs(),
        "thread_count": cmd_output("ps", &["-M", "-p", &std::process::id().to_string()])
            .map(|s| s.lines().count().saturating_sub(1))
            .unwrap_or(0),
    })
}

async fn metrics(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    // -1 = "query failed", never silently 0: a metric that cannot be read
    // must not report an empty queue (ethos rule 7).
    let mut body = json!({
        "rev": state.store.current_rev().map(|r| r.0).unwrap_or(0),
        "uptime_s": state.started.elapsed().as_secs(),
        "workers": {
            "total": count(&conn, "SELECT COUNT(*) FROM _amux_workers WHERE json_extract(state,'$.deleted_at') IS NULL"),
            "live_sessions": count(&conn, "SELECT COUNT(*) FROM _amux_sessions WHERE ended_at IS NULL"),
        },
        "queues": {
            "commands_queued": count(&conn, "SELECT COUNT(*) FROM _amux_commands WHERE state LIKE '%queued%'"),
            "commands_in_flight": count(&conn, "SELECT COUNT(*) FROM _amux_commands WHERE state LIKE '%dispatched%' OR state LIKE '%delivered%'"),
            "dead_letters": count(&conn, "SELECT COUNT(*) FROM _amux_commands WHERE state LIKE '%dead_lettered%'"),
            "messages_undelivered": count(&conn, "SELECT COUNT(*) FROM _amux_messages WHERE delivery LIKE '%queued%'"),
        },
        "board": {
            "open": count(&conn, "SELECT COUNT(*) FROM issues WHERE deleted IS NULL AND COALESCE(archived,0)=0 AND status NOT IN ('done','verified','discarded')"),
            "quarantined": count(&conn, "SELECT COUNT(*) FROM issues WHERE deleted IS NULL AND status = 'quarantined'"),
        },
        "leases": {
            "live": count(&conn, "SELECT COUNT(*) FROM _amux_leases WHERE expires_at > datetime('now')"),
            "total": count(&conn, "SELECT COUNT(*) FROM _amux_leases"),
        },
        "turns_recorded": count(&conn, "SELECT COUNT(*) FROM _amux_turns"),
        "events_journal": count(&conn, "SELECT COUNT(*) FROM _amux_state_events"),
    });
    // System/session/server metrics — shell commands run off the runtime
    let started = state.started;
    match tokio::task::spawn_blocking(move || {
        (collect_system_metrics(), collect_session_metrics(), collect_server_metrics(started))
    })
    .await
    {
        Ok((sys, sess, srv)) => {
            body["system"] = sys;
            body["sessions"] = json!(sess);
            body["server"] = srv;
        }
        Err(e) => {
            tracing::warn!("system metrics collection failed: {e}");
        }
    }
    Json(body).into_response()
}

async fn fleet(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    // The last published heartbeat + fleet-state events, straight from the
    // journal every consumer shares. STORAGE FORMAT, resolved 2026-08-09
    // after two sessions fixed the same mismatch in opposite directions:
    // the writer (db/mod.rs apply_write) now stores the BARE tag
    // ("fleet_progress"); the `{"kind":"other","data":name}` object form
    // was an accident of a no-op trim_matches, and while it was the format,
    // every `entity_type = '<tag>'` filter (this fn, the RR-0044b dedupe,
    // window_stats) silently matched nothing — a check that could not fail,
    // ethos rule 7. Rows written before the fix still carry the object
    // form, so this reads BOTH.
    let last = |name: &str| -> Option<String> {
        let legacy = serde_json::to_string(&amux_core::revision::EntityType::Other(name.into()))
            .unwrap_or_default();
        conn.query_row(
            "SELECT entity_id FROM _amux_state_events WHERE entity_type IN (?1, ?2)
             ORDER BY rev DESC LIMIT 1",
            [name, legacy.as_str()],
            |r| r.get(0),
        )
        .ok()
    };
    let parse = |s: Option<String>| {
        s.and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    };

    // Per-provider fleet state (RR-0044b): the dashboard's "Exhausted,
    // resets in 2h 14m, 14 workers parked" card. Derived from the SAME
    // worker rows by the SAME core function the runtime uses to park
    // workers — this view cannot disagree with the mechanism it describes
    // (ethos rule 1).
    let providers: serde_json::Value = {
        use amux_core::provider_fleet::{derive, ProviderState, DEFAULT_RESUME_STAGGER_SECS};
        let stagger = std::env::var("AMUX_RS_RESUME_STAGGER_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_RESUME_STAGGER_SECS);
        match crate::orchestrator::runtime::hydrate_workers(&conn) {
            Ok(workers) => derive(&workers, chrono::Utc::now(), stagger)
                .into_iter()
                .map(|(pid, p)| {
                    let (state, reset_at) = match &p.state {
                        ProviderState::Available => ("available", None),
                        ProviderState::QuotaExhausted { reset_at, .. } => {
                            ("quota_exhausted", reset_at.map(|r| r.to_rfc3339()))
                        }
                        ProviderState::Unknown => ("unknown", None),
                    };
                    (
                        pid.as_str().to_string(),
                        json!({
                            "state": state,
                            "reset_at": reset_at,
                            "workers_parked": p.affected_workers.len(),
                            "workers_total": p.workers.len(),
                        }),
                    )
                })
                .collect::<serde_json::Map<String, serde_json::Value>>()
                .into(),
            // A provider view that cannot be read must say so, never
            // report an empty (healthy-looking) fleet (ethos rule 7).
            Err(e) => json!({ "error": e.to_string() }),
        }
    };

    Json(json!({
        "last_heartbeat": parse(last("fleet_progress")),
        "last_fleet_state_change": parse(last("fleet_state")),
        "last_exhaustion_action": parse(last("exhaustion")),
        "providers": providers,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use crate::api::{router, AppState};
    use crate::db::{SharedStore, Store, WriteOutcome};
    use amux_core::worker::{WorkerConfig, WorkerState};
    use axum::body::Body;
    use axum::http::Request;
    use chrono::Utc;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Regression guard for the wrong-volume disk gauge.
    ///
    /// `df -k /` on APFS measures the sealed read-only system snapshot, which
    /// is ~11GB and never moves, so the dashboard reported 0.6% used while the
    /// data volume was 90% full. The tell is cheap and specific: the system
    /// snapshot is tiny, so a `disk_total_gb` in that range means the reading
    /// came from the wrong volume again.
    #[test]
    fn disk_metrics_measure_the_data_volume_not_the_sealed_system_snapshot() {
        let sys = super::collect_system_metrics();
        let total = sys["disk_total_gb"].as_f64().expect("disk_total_gb present");
        let used = sys["disk_used_gb"].as_f64().expect("disk_used_gb present");
        let pct = sys["disk_percent"].as_f64().expect("disk_percent present");

        // The data volume on any real machine dwarfs the ~11GB system snapshot.
        assert!(
            total > 50.0,
            "disk_total_gb = {total} — that is the sealed system volume, not the data volume"
        );
        assert!(used <= total, "used ({used}) cannot exceed total ({total})");

        // And it must agree with statfs on $HOME, which is where user data is.
        let (free, statfs_total) =
            crate::api::reclaim::df_bytes(&crate::api::reclaim::home_dir()).expect("statfs");
        let statfs_total_gb = statfs_total as f64 / 1_073_741_824.0;
        assert!(
            (total - statfs_total_gb).abs() < 1.0,
            "metrics reports {total} GB total but statfs on $HOME reports {statfs_total_gb} GB"
        );
        let expect_pct = (statfs_total - free) as f64 / statfs_total as f64 * 100.0;
        assert!(
            (pct - expect_pct).abs() < 1.0,
            "metrics reports {pct}% used but the data volume is at {expect_pct}%"
        );
    }

    fn app() -> (axum::Router, SharedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        (router(state), store, dir)
    }

    fn seed_worker(store: &SharedStore, n: u128, provider: &str, state: WorkerState) {
        let id = amux_core::ids::WorkerId::from_ulid(ulid::Ulid::from_parts(
            1_700_000_000_000,
            n,
        ));
        let provider = provider.to_string();
        store
            .write(move |conn| {
                let row = crate::db::queries::WorkerRow::new(
                    &id,
                    &WorkerConfig {
                        display_name: format!("w{n}"),
                        name_aliases: vec![],
                        cwd: "/tmp".into(),
                        provider: amux_core::provider::ProviderId(provider.clone()),
                        model: None,
                        backend: amux_core::session::BackendId::herdr(),
                        environment: Default::default(),
                        permissions: vec![],
                        group: None,
                    },
                    "2026-01-01T00:00:00Z",
                );
                crate::db::queries::insert_worker(conn, &row)?;
                crate::db::queries::update_worker_state(
                    conn,
                    id.as_str(),
                    &state,
                    "2026-01-01T00:00:00Z",
                )?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }

    /// RR-0044b metrics shape: /api/metrics/fleet reports per-provider
    /// {state, reset_at, workers_parked, workers_total}, from the same
    /// derivation the runtime parks with.
    #[tokio::test]
    async fn fleet_metrics_report_per_provider_state() {
        let (app, store, _dir) = app();
        let reset = Utc::now() + chrono::Duration::hours(2);
        seed_worker(&store, 31, "claude", WorkerState::RateLimited { reset_at: Some(reset) });
        seed_worker(&store, 32, "claude", WorkerState::Idle { since: Utc::now() });
        seed_worker(&store, 33, "codex", WorkerState::Idle { since: Utc::now() });

        let res = app
            .clone()
            .oneshot(Request::builder().uri("/api/metrics/fleet").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let claude = &v["providers"]["claude"];
        assert_eq!(claude["state"], "quota_exhausted", "{v}");
        assert_eq!(claude["reset_at"], reset.to_rfc3339(), "{v}");
        assert_eq!(claude["workers_parked"], 1, "{v}");
        assert_eq!(claude["workers_total"], 2, "{v}");

        let codex = &v["providers"]["codex"];
        assert_eq!(codex["state"], "available", "{v}");
        assert_eq!(codex["reset_at"], serde_json::Value::Null, "{v}");
        assert_eq!(codex["workers_parked"], 0, "{v}");
        assert_eq!(codex["workers_total"], 1, "{v}");
    }

    /// The `last()` lookups must find what the runtime writes (the stored
    /// entity_type is serde-encoded JSON, not the bare name — this test
    /// fails against the bare-name query that shipped originally).
    #[tokio::test]
    async fn fleet_metrics_surface_the_last_heartbeat() {
        let (app, store, _dir) = app();
        let rt = crate::orchestrator::runtime::Runtime {
            store: store.clone(),
            backends: vec![],
            tick_secs: 3,
            heartbeat_every: 1,
            breaker: amux_core::circuit::FleetCircuitBreaker {
                window_budget_tokens: u64::MAX,
                window_secs: 3600,
                min_progress_per_window: 0,
                max_failures_per_window: 1000,
            },
            fleet_state: std::sync::Mutex::new(amux_core::circuit::FleetState::Normal),
            protocol: None,
            pickup_unowned: false,
            resume_stagger_secs: 5,
        };
        rt.tick_once(true).await.unwrap(); // heartbeat tick

        let res = app
            .clone()
            .oneshot(Request::builder().uri("/api/metrics/fleet").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            v["last_heartbeat"].is_object(),
            "heartbeat published by the runtime must be readable here: {v}"
        );
    }
}
