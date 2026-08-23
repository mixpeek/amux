//! `/health` (RR-0021): build hash, uptime, store status, global revision.
//!
//! The Python workflow's hard-won rule — "bracket any measurement with
//! /health's build" — carries over verbatim: `build` is a content hash of
//! the running binary, so a restart with the same code and a restart with
//! different code are distinguishable (ethos rule 4).

use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
    pub build: String,
    /// The commit the binary was built from (AMUX-3454), stamped by build.rs.
    /// `build` discriminates BINARIES but cannot answer "does this build
    /// contain commit X", and the AF-82 time-comparison fails when two
    /// commits land seconds apart — which produced two wasted verification
    /// rounds in one afternoon (a build adopted mid-window was read as
    /// containing the later commit). `-dirty` suffix when the tree had
    /// uncommitted changes under crates/ at build time (the local builder
    /// compiles the working tree, so a bare sha would overclaim);
    /// "unknown" outside a git checkout (the cloud image).
    pub commit: &'static str,
    pub uptime_s: u64,
    pub rev: Option<u64>,
    pub store: &'static str,
    pub pid: u32,
    pub server: &'static str,
    /// Open descriptors / the process's own RLIMIT_NOFILE soft limit.
    ///
    /// AMUX-2812: on 2026-08-10 this process hit macOS's launchd default of 256
    /// and every open and spawn began failing with EMFILE. `/health` then went
    /// SILENT — which from the outside is indistinguishable from a dead
    /// machine, so the dashboard simply read "offline" and the first stretch of
    /// the investigation went looking for a crash that had not happened.
    ///
    /// Answering "out of descriptors" while out of descriptors is not reliably
    /// possible: the reply needs a socket. So the useful move is the one BEFORE
    /// that — publish the number continuously, so the approach is visible in
    /// the instrument everyone already polls, and `degraded` arrives while the
    /// server can still say it. `None` when unmeasurable, which is honestly
    /// different from zero.
    pub fds: Option<FdHealth>,
    /// Invariant-monitor confidence (AMUX-2625): healthy | degraded | unknown |
    /// unhealthy, from the last monitor roll-up. `unknown` when the monitor has
    /// not run yet OR its evidence is stale — the four states this card is
    /// about, on the endpoint every consumer already polls. Separate from
    /// `status` on purpose: `status` gates request health (store/fds), this
    /// carries the fleet-invariant verdict, and one must not mask the other.
    pub confidence: &'static str,
    /// Age of the evidence behind `confidence`, seconds. `None` if the monitor
    /// has never recorded a verdict. A growing age with `confidence:unknown` is
    /// a wedged monitor — visible instead of a frozen `healthy`.
    pub confidence_age_s: Option<f64>,
    /// Seconds amux was NOT RUNNING immediately before this boot (AEAB-29).
    ///
    /// `uptime_s` alone cannot distinguish "restarted 60s ago to adopt a build"
    /// from "restarted 60s ago after four hours down", and on 2026-08-18 that
    /// was exactly the question — the user's prompts had been going nowhere and
    /// the only evidence was a GAP between two log lines, which nothing counted.
    /// This publishes the gap on the endpoint every consumer already polls.
    ///
    /// `None` is honestly different from `0.0`: it means no outage was recorded
    /// for this boot — either there was none, or the heartbeat could not be
    /// stamped (which logs a WARN of its own rather than reporting health).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downtime_before_boot_s: Option<f64>,
    /// WHY that downtime happened: `machine` (the host restarted inside the
    /// window) or `process-only` (the host stayed up and only amux went away —
    /// the gui/501 signature). Absent when there was no downtime OR when the
    /// kernel boot instant could not be read; those are different, and the WARN
    /// in `heartbeat::record_boot` distinguishes them in prose. Published here
    /// because /health is where consumers already look (DESKT-22).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downtime_before_boot_cause: Option<&'static str>,
    /// Host memory state (AMUX-3397). The 2026-08-19 kernel panic
    /// (memory/swap exhaustion, AMUX-3396) killed the whole fleet and was
    /// invisible to every amux instrument before, during, and after. Same
    /// reasoning as `fds`: a host that is dying of memory exhaustion will
    /// soon be unable to say so, so the useful move is publishing the
    /// approach continuously on the endpoint everyone already polls.
    pub mem: MemHealth,
    /// Free space where amux keeps its DB and logs. Same rationale as `mem`
    /// (DESKT-21): the ENOSPC that took this machine to 741 MB free on
    /// 2026-08-10 was invisible to every amux instrument at the time.
    pub disk: DiskHealth,
    /// Tailnet reachability and node-key expiry (DESKT-24). Absent until the
    /// first `tailnet-watch` tick completes — deliberately distinct from a
    /// present reading whose `state` is `unknown`, which means a tick RAN and
    /// could not determine the answer. Read from a cache, never sampled here:
    /// forking the tailscale CLI per /health request would be a probe whose
    /// cost lands on the endpoint the whole fleet polls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<crate::runtime_jobs::tailnet_watch::TailnetHealth>,
}

#[derive(Serialize)]
pub struct FdHealth {
    pub open: usize,
    pub limit: u64,
    pub ratio: f64,
}

#[derive(Serialize)]
pub struct MemHealth {
    /// The KERNEL's own verdict, not a tuned amux threshold (ethos rule 7:
    /// prefer the structurally-present signal): macOS
    /// `kern.memorystatus_vm_pressure_level` — 1 normal, 2 warn, 4 critical
    /// (jetsam imminent). `None` when unmeasurable (non-macOS).
    pub pressure_level: Option<u32>,
    /// The level spelled out, so a reader does not need the mapping.
    pub pressure: &'static str,
    pub swap_used_mb: Option<f64>,
    pub swap_total_mb: Option<f64>,
}

#[derive(Serialize)]
pub struct DiskHealth {
    pub free_gb: Option<f64>,
    pub total_gb: Option<f64>,
    /// "ok" | "warn" | "critical" | "unknown"
    pub state: &'static str,
}

/// Free space on the volume holding `~/.amux`, published for the same reason as
/// [`mem_health`]: a host about to fail writes will soon be unable to say so,
/// so the useful move is putting the approach on the endpoint everyone polls.
///
/// # Why the threshold is ABSOLUTE GB and not a percentage
///
/// A percentage is the wrong unit here and would be a detector that fires on the
/// healthy baseline (ethos rule 7). This volume is 1.8 TB and sits at 91% used
/// while perfectly fine — a "90% used" alarm would be reporting that the disk
/// exists. What actually breaks is absolute headroom against a fixed-size
/// consumer: one debug cargo target tree is 10-15 GB, and on 2026-08-10 this
/// machine reached 741 MB free with a 50-session fleet and writes failing with
/// ENOSPC (the incident CLAUDE.md's shared-target-dir rule exists for).
///
/// So: critical below 10 GB (a single build cannot complete), warn below 50 GB
/// (a few builds from trouble). At today's 170 GB free this reads `ok`, and it
/// would have read `critical` throughout the 2026-08-10 incident. It can fail.
pub fn disk_health() -> DiskHealth {
    // statvfs is POSIX and present on both macOS and Linux, so unlike mem_health
    // this needs no cfg split at all.
    let free_total = statvfs_free_total(&crate::config::amux_home());
    match free_total {
        Some((free_gb, total_gb)) => DiskHealth {
            free_gb: Some(free_gb),
            total_gb: Some(total_gb),
            state: disk_state(Some(free_gb)),
        },
        // "unknown" and "ok" must never collapse: an unreadable disk is not a
        // healthy one, and reporting it as ok is how a silent probe gets trusted.
        None => DiskHealth { free_gb: None, total_gb: None, state: disk_state(None) },
    }
}

/// The threshold decision, split out so it is testable without a disk — the
/// shipped path calls exactly this.
pub(crate) fn disk_state(free_gb: Option<f64>) -> &'static str {
    match free_gb {
        Some(g) if g < 10.0 => "critical",
        Some(g) if g < 50.0 => "warn",
        Some(_) => "ok",
        None => "unknown",
    }
}

/// `(free_gb, total_gb)` for the filesystem containing `path`, walking UP to the
/// nearest existing ancestor.
///
/// The walk is not defensive padding: `statvfs` fails with ENOENT on a path that
/// does not exist, and `~/.amux` does not exist until the server creates it — so
/// without this, a fresh install reports `unknown` for the whole first run, and
/// CI (which has no `~/.amux` at all) reported it always. The volume is the same
/// one either way; the leaf's existence is irrelevant to the question being asked.
///
/// Found by CI going red on the test that was supposed to prove this reader
/// works (DESKT-21). The test asserted "readable on THIS host" and encoded my
/// host's layout as the premise — the failure was real and the assertion was
/// right to fire.
fn statvfs_free_total(path: &std::path::Path) -> Option<(f64, f64)> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if let Some(v) = statvfs_exact(p) {
            return Some(v);
        }
        cur = p.parent();
    }
    None
}

/// `statvfs` on exactly this path, no fallback.
fn statvfs_exact(path: &std::path::Path) -> Option<(f64, f64)> {
    let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: cpath is a NUL-terminated path and st is a properly sized statvfs
    // the kernel fills on success.
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    // f_frsize is the fragment size the f_* block counts are expressed in;
    // f_bsize is the preferred IO size and is NOT always the same number.
    // Using the wrong one silently scales every reading.
    let unit = if st.f_frsize > 0 { st.f_frsize as f64 } else { st.f_bsize as f64 };
    const GB: f64 = 1_073_741_824.0;
    // f_bavail, not f_bfree: bfree counts root-reserved blocks that amux (which
    // does not run as root) can never actually use.
    Some((st.f_bavail as f64 * unit / GB, st.f_blocks as f64 * unit / GB))
}

/// One syscall per field on macOS (`sysctlbyname`), `/proc/meminfo` on Linux
/// — no subprocess, for the fd_health reason: spawning costs the resources
/// being measured, and fails exactly when the condition it reports is present.
pub fn mem_health() -> MemHealth {
    let level = pressure_level();
    let swap = swap_usage();
    MemHealth {
        pressure_level: level,
        pressure: match level {
            Some(1) => "normal",
            Some(2) => "warn",
            Some(4) => "critical",
            Some(_) | None => "unknown",
        },
        swap_used_mb: swap.map(|(u, _)| u),
        swap_total_mb: swap.map(|(_, t)| t),
    }
}

#[cfg(target_os = "macos")]
fn sysctl_by_name<T>(name: &str) -> Option<T> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut val = std::mem::MaybeUninit::<T>::uninit();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: the kernel writes at most `len` bytes into a T-sized buffer;
    // we only assume_init when it reports success AND filled the whole T.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            val.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<T>()).then(|| unsafe { val.assume_init() })
}

#[cfg(target_os = "macos")]
fn pressure_level() -> Option<u32> {
    sysctl_by_name::<libc::c_int>("kern.memorystatus_vm_pressure_level").map(|v| v as u32)
}

#[cfg(target_os = "macos")]
fn swap_usage() -> Option<(f64, f64)> {
    sysctl_by_name::<libc::xsw_usage>("vm.swapusage")
        .map(|x| (x.xsu_used as f64 / 1048576.0, x.xsu_total as f64 / 1048576.0))
}

#[cfg(not(target_os = "macos"))]
fn pressure_level() -> Option<u32> {
    // Linux exposes PSI, not a discrete level; mapping PSI to warn/critical
    // would be a tuned parameter invented here, so it is honestly absent.
    None
}

#[cfg(not(target_os = "macos"))]
fn swap_usage() -> Option<(f64, f64)> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let get = |k: &str| {
        s.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<f64>().ok())
    };
    let total_kb = get("SwapTotal:")?;
    let free_kb = get("SwapFree:")?;
    Some(((total_kb - free_kb) / 1024.0, total_kb / 1024.0))
}

/// The same `AMUX_FD_MAX_RATIO` the autofix detector triggers on, read from the
/// same env var — so `/health` saying `degraded` and a card being filed are the
/// SAME condition, not two thresholds that drift apart. A view must share the
/// predicate of the mechanism it describes (ethos rule 1).
fn fd_ceiling() -> f64 {
    std::env::var("AMUX_FD_MAX_RATIO")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.7)
        .clamp(0.1, 0.99)
}

/// Costs one descriptor and NO subprocess — `lsof` would fail exactly when the
/// condition it reports is present, and each attempt would consume the resource
/// being measured (ethos rule 7: what does the detector cost, and is it paid in
/// the same resource as the fault?).
fn fd_health() -> Option<FdHealth> {
    let open = std::fs::read_dir("/dev/fd").ok()?.count();
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes into a fully-initialised local; no aliasing.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 || rl.rlim_cur == 0 {
        return None;
    }
    let limit = rl.rlim_cur;
    Some(FdHealth { open, limit, ratio: open as f64 / limit as f64 })
}

pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    // A store that cannot answer the revision query is degraded — surface
    // that instead of a green lie (ethos rule 7: a check must be able to
    // fail).
    let (rev, store, code) = match state.store.current_rev() {
        Ok(rev) => (Some(rev.0), "ok", StatusCode::OK),
        Err(_) => (None, "hung", StatusCode::SERVICE_UNAVAILABLE),
    };
    let fds = fd_health();
    // Descriptor pressure degrades `status` but does NOT fail the request: the
    // server is still serving, and returning 503 here would take the fleet down
    // on a warning. `status` is the field that carries the warning; `store` is
    // the one that gates the code.
    let fd_tight = fds.as_ref().is_some_and(|f| f.ratio >= fd_ceiling());
    // Cheap read of the monitor's last cached verdict — no DB, no re-run, and
    // correct (unknown) even if the monitor is dead (AMUX-2625).
    let (conf, conf_age) =
        crate::invariants::health_confidence(chrono::Utc::now().timestamp() as f64);
    (
        code,
        Json(Health {
            status: if store != "ok" {
                "degraded"
            } else if fd_tight {
                "degraded-fds"
            } else {
                "ok"
            },
            build: state.build_hash.clone(),
            commit: env!("AMUX_BUILD_COMMIT"),
            uptime_s: state.started.elapsed().as_secs(),
            rev,
            store,
            pid: std::process::id(),
            server: "amux-rust",
            fds,
            confidence: conf.as_str(),
            confidence_age_s: conf_age,
            downtime_before_boot_s: crate::runtime_jobs::heartbeat::boot_gap()
                .map(|g| g.seconds),
            downtime_before_boot_cause: crate::runtime_jobs::heartbeat::boot_gap()
                .and_then(|g| g.cause)
                .map(|c| c.as_str()),
            mem: mem_health(),
            disk: disk_health(),
            tailnet: crate::runtime_jobs::tailnet_watch::cached(),
        }),
    )
}

/// GET /api/debug/tmux — runs the exact fleet-discovery command from INSIDE
/// this process and reports argv, exit status, output sizes, and the env
/// that determines the socket. Exists because the live launchd instance
/// served running=0 for the whole fleet while the same binary in a login
/// shell served 49, and no log line could say why (ethos rule 4: the
/// instrument must express the discriminator, from the consumer's vantage).
pub async fn debug_tmux() -> axum::Json<serde_json::Value> {
    let out = std::process::Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_activity}\t#{session_created}",
        ])
        .output();
    let which = std::process::Command::new("which").arg("tmux").output();
    axum::Json(match out {
        Ok(o) => serde_json::json!({
            "spawn": "ok",
            "exit": o.status.to_string(),
            "stdout_bytes": o.stdout.len(),
            "stdout_lines": String::from_utf8_lossy(&o.stdout).lines().count(),
            "stdout_head": String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or(""),
            "stderr": String::from_utf8_lossy(&o.stderr).trim(),
            "which_tmux": which.ok().map(|w| String::from_utf8_lossy(&w.stdout).trim().to_string()),
            "env_path": std::env::var("PATH").unwrap_or_default(),
            "env_tmux_tmpdir": std::env::var("TMUX_TMPDIR").ok(),
            "env_tmpdir": std::env::var("TMPDIR").ok(),
            "env_tmux": std::env::var("TMUX").ok(),
            "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
        }),
        Err(e) => serde_json::json!({ "spawn": "failed", "error": e.to_string() }),
    })
}

/// GET /api/debug/scan returns the terminal scan loop's last pass, so a
/// demotion that leaves no trace is not mistaken for a scan that found nothing
/// (ethos rule 4, the D1 "scan" deviation). Advertised in ethos.md and the
/// system-jobs registry (`detail: Some("/api/debug/scan")`) but unrouted until
/// AF-80: a claimed-but-not-implemented instrument (ethos rule 6). Reports which lanes
/// were demoted off pane-scraping and on what basis (their own protocol voice
/// vs the backend's native agent-status report), which were actually captured,
/// which captures or native reads failed, and the per-worker dedupe hash that
/// gates re-firing a still-on-screen banner. A `null` last_pass_at means the
/// loop has not completed a pass yet; if that persists past AMUX_RS_SCAN_SECS,
/// `/api/system-jobs` names the `terminal-scan` job as stalled.
pub async fn debug_scan() -> axum::Json<serde_json::Value> {
    let now = crate::runtime_jobs::registry::unix_now();
    match crate::orchestrator::scan::last_scan_state() {
        Some(s) => axum::Json(serde_json::json!({
            "note": "the terminal scan loop's last pass, the FALLBACK voice for hookless \
                     workers. A lane in demoted_structured spoke for itself (live protocol \
                     session); one in demoted_native was reported by its backend (herdr \
                     agent_status); one in scanned had its pane captured because neither \
                     voice was available. A skip here leaves a trace on purpose (ethos rule 4).",
            "now": now,
            "last_pass_at": s.last_pass_at,
            "last_pass_age_s": s.last_pass_at.map(|t| now - t),
            "scan_secs": std::env::var("AMUX_RS_SCAN_SECS").ok(),
            "scanned": s.report.scanned,
            "demoted_structured": s.report.demoted_structured,
            "demoted_native": s.report.demoted_native,
            "native_status_failures": s.report.native_status_failures,
            "capture_failures": s.report.capture_failures,
            "events_applied": s.report.events_applied,
            "deduped": s.deduped,
        })),
        None => axum::Json(serde_json::json!({
            "note": "the terminal scan loop has not completed a pass yet, so no demotion \
                     decisions to report. If this persists past AMUX_RS_SCAN_SECS, check \
                     /api/system-jobs for the 'terminal-scan' job.",
            "now": now,
            "last_pass_at": serde_json::Value::Null,
            "scanned": Vec::<String>::new(),
            "demoted_structured": Vec::<String>::new(),
            "demoted_native": Vec::<String>::new(),
            "native_status_failures": Vec::<String>::new(),
            "capture_failures": Vec::<String>::new(),
            "events_applied": 0,
            "deduped": serde_json::Map::new(),
        })),
    }
}

/// GET /api/debug/downtime — the recorded outages, most recent first (AEAB-29).
///
/// The CATALOG row for the `heartbeat` job advertises this path, and an
/// advertised-but-unrouted instrument is ethos rule 6 (an audit trail that is
/// claimed and not implemented). It answers the question that had no answer on
/// 2026-08-18: was amux down, when, and for how long?
pub async fn debug_downtime(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return axum::Json(serde_json::json!({ "error": e.to_string() }));
        }
    };
    let mut rows: Vec<serde_json::Value> = Vec::new();
    // A query that FAILS must not render as "there were never any outages".
    // Caught live on AF-99: with migrations 0022/0023 written but not registered,
    // `requests_during` did not exist, prepare() failed, and this endpoint served
    // `"outages": []` — which is exactly what a healthy server with no history
    // looks like. I read that as the false row having been corrected. An empty
    // list and a broken query must be distinguishable (ethos rule 4).
    let mut query_error: Option<String> = None;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT down_from, up_at, seconds, port, requests_during FROM server_downtime \
         ORDER BY up_at DESC LIMIT 100",
    ) {
        if let Ok(it) = stmt.query_map([], |r| {
            let served: Option<i64> = r.get(4)?;
            // The row's own verdict, so a reader never has to re-derive the rule
            // (AF-99). It is the same predicate downtime_within() subtracts on.
            let (kind, subtractable) = match served {
                Some(0) => ("downtime", true),
                Some(_) => ("heartbeat-gap", false),
                None => ("unverified", false),
            };
            Ok(serde_json::json!({
                "down_from": crate::api::request_log::local_when(r.get::<_, f64>(0)?),
                "up_at": crate::api::request_log::local_when(r.get::<_, f64>(1)?),
                "down_from_ts": r.get::<_, f64>(0)?,
                "up_at_ts": r.get::<_, f64>(1)?,
                "seconds": r.get::<_, f64>(2)?,
                "minutes": (r.get::<_, f64>(2)? / 60.0 * 10.0).round() / 10.0,
                "noticed_by_port": r.get::<_, Option<i64>>(3)?,
                "requests_during": served,
                "kind": kind,
                "counts_as_downtime": subtractable,
            }))
        }) {
            rows.extend(it.filter_map(|r| r.ok()));
        } else {
            query_error = Some("server_downtime rows could not be read".into());
        }
    } else {
        query_error = Some(
            "server_downtime could not be queried — the schema is older than this \
             binary expects (is a migration unregistered or unapplied?)"
                .into(),
        );
    }
    let last_beat: Option<f64> = conn
        .query_row("SELECT beat_at FROM server_heartbeat WHERE id = 1", [], |r| r.get(0))
        .ok();
    axum::Json(serde_json::json!({
        "note": "Gaps in the liveness heartbeat. `down_from` is the LAST CONFIRMED \
                 BEAT, so the true stop is within one beat interval after it — the \
                 number is deliberately the one that can be proved rather than a guess. \
                 READ `kind` BEFORE USING A ROW: only `downtime` (requests_during = 0) \
                 means no server was running, and only those are subtracted by \
                 downtime_within(). `heartbeat-gap` means the server SERVED requests \
                 throughout — the beat loop stopped, amux did not — and `unverified` \
                 means the request log could not be counted. Filing all three was AF-99: \
                 a row claiming 759 minutes of downtime covered 33,455 served requests, \
                 and this table is built to be SUBTRACTED from steer-queue age, rot \
                 timers and missed schedules.",
        "beat_interval_s": crate::runtime_jobs::heartbeat::beat_interval().as_secs(),
        "min_gap_s": crate::runtime_jobs::heartbeat::min_gap_s(),
        "last_beat_at": last_beat.map(crate::api::request_log::local_when),
        "this_boot_followed_downtime_s": crate::runtime_jobs::heartbeat::boot_gap().map(|g| g.seconds),
        "outages": rows,
        // Present ONLY when the read failed, so a consumer can tell an empty
        // history from an unreadable one.
        "error": query_error,
    }))
}

#[cfg(test)]
mod disk_tests {
    use super::*;

    /// The NEGATIVE half: "could not read" must never render as healthy. An
    /// unreadable disk reported as `ok` is the silent probe that gets trusted.
    #[test]
    fn unknown_is_not_ok() {
        assert_eq!(disk_state(None), "unknown");
        assert_ne!(disk_state(None), disk_state(Some(500.0)));
    }

    /// Rebuilt from the incident this exists for: 2026-08-10, ~37 abandoned
    /// cargo target trees took this volume to 741 MB free and writes started
    /// failing with ENOSPC while a 50-session fleet ran.
    #[test]
    fn the_2026_08_10_enospc_would_have_read_critical() {
        assert_eq!(disk_state(Some(0.741)), "critical");
    }

    /// And the half that keeps it from being an alarm that is always on: this
    /// volume is 1.8 TB at 91% used and perfectly fine. A percentage threshold
    /// would fire here permanently, which is a detector reporting that the disk
    /// exists (ethos rule 7).
    #[test]
    fn todays_170gb_free_at_91_percent_used_reads_ok() {
        assert_eq!(disk_state(Some(170.0)), "ok");
    }

    /// Boundaries pinned so a later refactor cannot slide them silently.
    #[test]
    fn the_thresholds_discriminate_at_their_own_boundaries() {
        assert_eq!(disk_state(Some(9.99)), "critical");
        assert_eq!(disk_state(Some(10.0)), "warn", "one cargo target tree of headroom");
        assert_eq!(disk_state(Some(49.99)), "warn");
        assert_eq!(disk_state(Some(50.0)), "ok");
    }

    /// The statvfs read must actually work. A reader that compiles everywhere
    /// and returns None on the host you ship to is theatre, and would render as
    /// `unknown` forever with nobody noticing.
    ///
    /// The first version of this asserted against `~/.amux` and turned CI red:
    /// that directory does not exist on a fresh runner, `statvfs` returns
    /// ENOENT, and the reading was `None`. The assertion was CORRECT to fire —
    /// the defect was in the reader, which now walks up to an existing
    /// ancestor. This version anchors on a path that exists everywhere so it
    /// tests the READER rather than my home directory's layout.
    #[test]
    fn a_real_volume_is_readable() {
        let (free, total) = statvfs_free_total(std::path::Path::new("/"))
            .expect("statvfs on / must be readable on any host that can run this test");
        assert!(free > 0.0 && total > 0.0, "free {free} total {total}");
        assert!(free <= total, "free {free} cannot exceed total {total}");
        // Catches an f_frsize/f_bsize units mix-up, the failure that would
        // silently scale every reading by 8x or 512x.
        assert!(total < 1_000_000.0, "implausible volume size {total} GB — units wrong?");
    }

    /// The ancestor walk is the fix for the CI break, so it gets its own test
    /// with a path that CANNOT exist — otherwise the fix is only exercised on
    /// hosts that happen to be missing `~/.amux`, which is the opposite of
    /// where it will be read.
    #[test]
    fn a_path_that_does_not_exist_yet_still_reports_its_volume() {
        let missing = std::path::Path::new("/this-does-not-exist-9f3a/nor/does/this");
        assert!(!missing.exists(), "fixture must actually be absent to test anything");
        assert!(
            statvfs_exact(missing).is_none(),
            "the non-walking reader must fail here, or the walk below proves nothing"
        );
        let (free, total) = statvfs_free_total(missing)
            .expect("the walk must reach / and report the volume anyway");
        assert!(free > 0.0 && total > 0.0);
        assert_ne!(disk_state(Some(free)), "unknown");
    }

    /// And the shipped entry point must be healthy here, whatever `~/.amux`'s
    /// state is — this is the assertion that would have caught the original
    /// bug from the consumer's side.
    #[test]
    fn disk_health_never_reports_unknown_on_a_working_host() {
        let h = disk_health();
        let free = h.free_gb.expect("disk_health must report free space on any host");
        let total = h.total_gb.expect("disk_health must report total space");
        assert!(free > 0.0 && total > 0.0, "free {free} total {total}");
        assert!(free <= total, "free {free} cannot exceed total {total}");
        assert_ne!(h.state, "unknown", "a readable volume must not report unknown");
    }
}
