//! AMUX-4203: a refused socket is not proof that its server died.
//! Compare the responding tmux identity with kernel socket owners, without
//! reading process arguments (worker launch arguments can contain credentials).

use crate::invariants::InvariantResult;
use serde::Serialize;
use std::{collections::BTreeSet, process::Stdio, time::Duration};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SocketOwner {
    pub pid: u32,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub measured: bool,
    pub n_considered: usize,
    pub why_unmeasured: Option<String>,
    pub socket_path: String,
    pub responding_pid: Option<u32>,
    pub probe_error: Option<String>,
    pub owners: Vec<SocketOwner>,
    pub verdict: String,
}

fn normalized_path(path: &str) -> String {
    // Canonicalize the parent, not the socket: the directory entry may already
    // belong to a replacement, or be missing while its original owner lives.
    let p = std::path::Path::new(path);
    match (
        p.parent().and_then(|p| p.canonicalize().ok()),
        p.file_name(),
    ) {
        (Some(parent), Some(name)) => parent.join(name).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    unsafe { libc::geteuid() }
}

fn default_socket() -> String {
    if let Ok(tmux) = std::env::var("TMUX") {
        if let Some((path, _)) = tmux.split_once(',') {
            if !path.is_empty() {
                return normalized_path(path);
            }
        }
    }
    let root = std::env::var("TMUX_TMPDIR").unwrap_or_else(|_| "/tmp".into());
    normalized_path(&format!("{root}/tmux-{}/default", euid()))
}

async fn output(bin: &str, args: &[&str]) -> Result<std::process::Output, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(3), cmd.output())
        .await
        .map_err(|_| format!("{bin} probe timed out after 3s"))?
        .map_err(|e| format!("{bin} probe failed: {e}"))
}

async fn output_any(bins: &[&str], args: &[&str]) -> Result<std::process::Output, String> {
    let mut errors = Vec::new();
    for bin in bins {
        match output(bin, args).await {
            Ok(out) => return Ok(out),
            Err(error) => errors.push(error),
        }
    }
    Err(errors.join("; "))
}

fn lsof_stderr_benign(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr);
    text.is_empty()
        || text.lines().all(|l| {
            let t = l.trim();
            t.is_empty()
                || t.starts_with("lsof: WARNING: can't stat()")
                || t.starts_with("Output information may be incomplete")
        })
}

// lsof exits 1 for "no matches"; stat warnings about unrelated mounts
// (Docker overlay/nsfs, fuse.portal) do not make that answer unmeasured.
fn lsof_enumerated(o: &std::process::Output) -> bool {
    o.status.success()
        || (o.status.code() == Some(1) && o.stdout.is_empty() && lsof_stderr_benign(&o.stderr))
}

fn parse_owners(raw: &str) -> Vec<SocketOwner> {
    let mut pid = None;
    let mut is_tmux = false;
    let mut owners = BTreeSet::new();
    for line in raw.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse::<u32>().ok();
            is_tmux = false;
        } else if let Some(command) = line.strip_prefix('c') {
            is_tmux = command == "tmux" || command == "tmux: server";
        } else if let Some(path) = line.strip_prefix('n').filter(|p| p.starts_with('/')) {
            if let Some(pid) = pid.filter(|_| is_tmux) {
                // Linux lsof may append socket type/state after the pathname.
                let path = path.split(" type=").next().unwrap_or(path);
                owners.insert((pid, normalized_path(path)));
            }
        }
    }
    owners
        .into_iter()
        .map(|(pid, path)| SocketOwner { pid, path })
        .collect()
}

fn verdict(pid: Option<u32>, owners: &[SocketOwner], path: &str, measured: bool) -> &'static str {
    if !measured {
        return "unmeasured";
    }
    let matching: BTreeSet<_> = owners
        .iter()
        .filter(|o| o.path == path)
        .map(|o| o.pid)
        .collect();
    if matching.len() > 1 {
        "multiple_socket_owners"
    } else if pid.is_none() && !matching.is_empty() {
        "live_server_unreachable"
    } else if pid.is_some() {
        "ok"
    } else {
        "no_server"
    }
}

fn socket_ownership_failure(verdict: &str) -> bool {
    matches!(
        verdict,
        "multiple_socket_owners" | "live_server_unreachable"
    )
}

pub async fn observe() -> Observation {
    observe_with_evidence(true).await
}

async fn observe_with_evidence(capture: bool) -> Observation {
    let uid = euid().to_string();
    let lsof_args = ["-nP", "-a", "-U", "-u", &uid, "-c", "tmux", "-Fpcn"];
    let (identity, sockets) = tokio::join!(
        output(
            "tmux",
            &["-N", "display-message", "-p", "#{pid}|#{socket_path}"]
        ),
        output_any(&["lsof", "/usr/sbin/lsof", "/usr/bin/lsof"], &lsof_args)
    );
    let mut path = default_socket();
    let mut pid = None;
    let probe_error = match identity {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            match text
                .trim()
                .split_once('|')
                .filter(|(p, s)| p.parse::<u32>().is_ok() && s.starts_with('/'))
            {
                Some((p, s)) => {
                    pid = p.parse().ok();
                    path = normalized_path(s);
                    None
                }
                None => Some("tmux returned no usable server identity".into()),
            }
        }
        Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Some(e),
    };
    let (owners, error) = match sockets {
        Ok(o) if lsof_enumerated(&o) => (parse_owners(&String::from_utf8_lossy(&o.stdout)), None),
        Ok(o) => (
            vec![],
            Some(format!(
                "lsof could not enumerate socket owners: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
        ),
        Err(e) => (vec![], Some(e)),
    };
    let measured = error.is_none();
    let verdict = verdict(pid, &owners, &path, measured).to_string();
    let observation = Observation {
        measured,
        n_considered: owners.len(),
        why_unmeasured: error,
        socket_path: path,
        responding_pid: pid,
        probe_error,
        owners,
        verdict,
    };
    if capture && socket_ownership_failure(&observation.verdict) {
        capture_stall_evidence(&observation, "socket_ownership", None).await;
    }
    observation
}

/// Capture the first probe timeout without depending on the main runtime's
/// ability to schedule its invariant monitor during a host-wide stall.
pub(crate) fn capture_after_probe_timeout(probe: serde_json::Value) {
    // Unit fixtures intentionally create hung children; they must not probe
    // the operator's real fleet or write incident samples into their home.
    #[cfg(test)]
    drop(probe);
    #[cfg(not(test))]
    {
        let now = crate::config::now_f64();
        if !crate::log_dedupe::first_this_bucket("tmux-timeout-sample", (now / 60.0) as i64) {
            return;
        }
        if let Err(error) = std::thread::Builder::new()
            .name("tmux-timeout-evidence".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(async {
                        let observation = observe_with_evidence(false).await;
                        capture_stall_evidence(&observation, "fleet_probe_timeout", Some(probe))
                            .await;
                    }),
                    Err(error) => tracing::warn!(target: "amux::tmux", %error,
                    verdict = "stall_evidence_runtime_failed", "timeout evidence was not measured"),
                }
            })
        {
            tracing::warn!(target: "amux::tmux", %error,
                verdict = "stall_evidence_thread_failed", "timeout evidence was not measured");
        }
    }
}

/// Parse one `top -l 1 -stats pid,mem,cmprs,cpu,time,command` row's size field.
///
/// top writes sizes with a unit suffix (`4096B`, `7904K`, `10M`, `16G`) and the
/// suffix is what carries the magnitude, so a numeric parse alone reads 16G as
/// sixteen. Returns bytes.
fn top_size_bytes(field: &str) -> Option<u64> {
    let (num, mult) = match field.chars().last()? {
        'B' => (&field[..field.len() - 1], 1u64),
        'K' => (&field[..field.len() - 1], 1024),
        'M' => (&field[..field.len() - 1], 1024 * 1024),
        'G' => (&field[..field.len() - 1], 1024 * 1024 * 1024),
        '0'..='9' => (field, 1),
        _ => return None,
    };
    num.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| (v * mult as f64) as u64)
}

/// (CC_DIR, lane) for every worker env file. Read once per evidence capture,
/// not per process.
fn lane_dirs() -> Vec<(String, String)> {
    let dir = crate::api::session_verbs::home().join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(lane) = path.file_stem().and_then(|x| x.to_str()) else {
            continue;
        };
        let cfg = crate::api::session_verbs::EnvFile::load(&path);
        if let Some(d) = cfg.get("CC_DIR").filter(|d| !d.is_empty()) {
            out.push((d.to_string(), lane.to_string()));
        }
    }
    out
}

/// Which lane owns a process, from its EXECUTABLE PATH.
///
/// AMUX-4617 asks for the owner of each heavy process. The obvious signal is the
/// process's cwd, and it is unavailable here: `lsof -p <pid> -d cwd` returns
/// nothing on this host even for amux-server's own pid, with no permission error
/// printed, which is a TCC/SIP restriction rather than a flag mistake. The
/// executable path needs no per-pid permission at all, and it is enough for the
/// case this card is about, because a lane's dev server runs out of the lane's
/// own tree (`.../ai-for-smbs/smb-workspace/backend/.venv/bin/python`).
///
/// LONGEST PREFIX WINS, and that is not a detail. Lane CC_DIRs NEST: measured
/// 2026-09-16 there are 102 distinct ones and `/Users/ethan/Dev` is itself a
/// lane's CC_DIR, so those ai-for-smbs python processes match three lanes at
/// once. Reporting the first match would name whichever lane happened to sort
/// first and blame the wrong owner, which is worse than reporting none: the
/// whole point is to tell someone their process is running, and telling the
/// wrong someone is how AMUX-4550's audit says noise gets made.
///
/// Boundary-anchored: `/a/bc` must not match a lane rooted at `/a/b`.
fn owner_from_exe_path<'a>(exe: &str, lanes: &'a [(String, String)]) -> Option<&'a str> {
    let mut best: Option<(&str, usize)> = None;
    for (dir, lane) in lanes {
        let d = dir.trim_end_matches('/');
        if d.is_empty() {
            continue;
        }
        let under = exe
            .strip_prefix(d)
            .is_some_and(|rest| rest.starts_with('/'));
        if !under {
            continue;
        }
        if best.is_none_or(|(_, n)| d.len() > n) {
            best = Some((lane.as_str(), d.len()));
        }
    }
    best.map(|(lane, _)| lane)
}

/// Rank host processes by FOOTPRINT (resident + compressed) rather than RSS.
///
/// AMUX-4617. `top_rss` ranks by resident memory and that is the wrong
/// discriminator on a machine with memory compression. The specimen: the
/// procwarden menubar agent held 27 GB of footprint, 26 GB of it compressed,
/// with 4 MB resident, so it sat nowhere near the top of an RSS ranking while
/// being the largest consumer on the box (AF-875).
///
/// Still true here, measured 2026-09-16 on live output: the largest process by
/// footprint is 44.00 GB against 16.00 GB resident, so an RSS ranking
/// understates it by 28 GB.
///
/// Pure, so the ranking is testable without a host under memory pressure. That
/// matters more than usual here: the condition this exists to catch cannot be
/// produced on demand.
/// `top` is the only source of the compressed figure and its COMMAND column is
/// TRUNCATED (`com.apple.Virtua`), so it cannot name an owner. `ps` carries the
/// full executable path and is already being collected for `host_processes`, so
/// the two are joined on pid rather than shelling out a third time.
fn footprint_summary(top_raw: &str, ps_raw: &str, lanes: &[(String, String)]) -> serde_json::Value {
    let mut exe_by_pid: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    for line in ps_raw.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 6 {
            continue;
        }
        if let Ok(pid) = f[0].parse::<u32>() {
            exe_by_pid.insert(pid, f[5..].join(" "));
        }
    }
    let mut rows = Vec::new();
    for line in top_raw.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        // top marks the sampling process with a trailing `*`.
        let pid = match f[0].trim_end_matches('*').parse::<u32>() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let (Some(mem), Some(cmprs)) = (top_size_bytes(f[1]), top_size_bytes(f[2])) else {
            continue;
        };
        let cpu = f[3].parse::<f64>().unwrap_or(0.0);
        if !cpu.is_finite() {
            continue;
        }
        let exe = exe_by_pid.get(&pid).cloned().unwrap_or_default();
        let owner = owner_from_exe_path(&exe, lanes);
        rows.push(serde_json::json!({
            "pid": pid,
            "footprint_bytes": mem + cmprs,
            "resident_bytes": mem,
            "compressed_bytes": cmprs,
            "cpu_pct": cpu,
            // top's own column, truncated, kept because it is what top saw.
            "command": f[4..].join(" "),
            "executable": exe,
            // null, not "unknown": a process outside every lane tree HAS no lane
            // owner, and saying so is different from failing to look.
            "owner_lane": owner,
        }));
    }
    let count = rows.len();
    rows.sort_by_key(|r| std::cmp::Reverse(r["footprint_bytes"].as_u64()));
    serde_json::json!({
        "measured": count > 0,
        "n_considered": count,
        "why_unmeasured": if count == 0 { Some("top returned no parseable process rows") } else { None },
        "top_footprint": rows.into_iter().take(20).collect::<Vec<_>>(),
    })
}

fn host_process_summary(raw: &str) -> serde_json::Value {
    let mut rows = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(state), Some(cpu), Some(rss)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(cpu), Ok(rss)) = (
            pid.parse::<u32>(),
            ppid.parse::<u32>(),
            cpu.parse::<f64>(),
            rss.parse::<u64>(),
        ) else {
            continue;
        };
        if !cpu.is_finite() {
            continue;
        }
        rows.push(serde_json::json!({"pid": pid, "ppid": ppid, "state": state,
            "cpu_pct": cpu, "rss_kib": rss, "executable": fields.collect::<Vec<_>>().join(" ")}));
    }
    let count = rows.len();
    rows.sort_by(|a, b| {
        b["cpu_pct"]
            .as_f64()
            .partial_cmp(&a["cpu_pct"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let cpu: Vec<_> = rows.iter().take(20).cloned().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row["rss_kib"].as_u64()));
    serde_json::json!({"measured": count > 0, "n_considered": count,
        "why_unmeasured": if count == 0 { Some("ps returned no parseable process rows") } else { None },
        "top_cpu": cpu, "top_rss": rows.into_iter().take(20).collect::<Vec<_>>()})
}

async fn capture_stall_evidence(
    observation: &Observation,
    trigger: &str,
    probe: Option<serde_json::Value>,
) {
    let now = crate::config::now_f64();
    let key = format!("tmux-stall-evidence:{trigger}:{:?}", observation.owners);
    let bucket = if trigger == "fleet_probe_timeout" {
        (now / 60.0) as i64
    } else {
        crate::log_dedupe::hour_bucket(now)
    };
    if !crate::log_dedupe::first_this_bucket(&key, bucket) {
        return;
    }
    let dir = crate::api::session_verbs::home().join("logs");
    let receipt = dir.join(format!("tmux-stall-{}-{trigger}.json", now as u64));
    let mut evidence = serde_json::json!({"at": now, "trigger": trigger, "probe": probe, "socket_ownership": observation, "samples": []});
    let (processes, uptime) = tokio::join!(
        output("ps", &["-A", "-o", "pid=,ppid=,stat=,pcpu=,rss=,comm="]),
        output("uptime", &[]),
    );
    // Taken BEFORE the match below consumes `processes`: the owner join needs
    // the same ps output, and running ps twice would sample two different
    // instants for one report.
    let ps_for_owner = match &processes {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    };
    evidence["host_processes"] = match processes {
        Ok(out) if out.status.success() => {
            host_process_summary(&String::from_utf8_lossy(&out.stdout))
        }
        result => serde_json::json!({"measured": false, "n_considered": 0,
            "why_unmeasured": match result { Ok(out) => out.status.to_string(), Err(error) => error }}),
    };
    // AMUX-4617. Ranked apart from `host_processes` rather than replacing it:
    // `ps` has no footprint field at all, so this needs `top`, and the two
    // answer different questions. RSS says what is resident NOW, footprint says
    // what the process is holding, and on a compressing host those diverge by
    // tens of GB. Both are kept so a reader can see the divergence, which is
    // the evidence that the ranking changed anything.
    let lanes = lane_dirs();
    evidence["host_footprint"] = match output(
        "top",
        &[
            "-l",
            "1",
            "-n",
            "200",
            "-stats",
            "pid,mem,cmprs,cpu,command",
        ],
    )
    .await
    {
        Ok(out) if out.status.success() => {
            footprint_summary(&String::from_utf8_lossy(&out.stdout), &ps_for_owner, &lanes)
        }
        result => serde_json::json!({"measured": false, "n_considered": 0,
            "why_unmeasured": match result { Ok(out) => out.status.to_string(), Err(error) => error }}),
    };
    evidence["host_uptime_load"] = match uptime {
        Ok(out) if out.status.success() => serde_json::json!({"measured": true, "n_considered": 1,
            "output": String::from_utf8_lossy(&out.stdout).trim(),
            "available_parallelism": std::thread::available_parallelism().ok().map(|n| n.get())}),
        result => serde_json::json!({"measured": false, "n_considered": 0,
            "why_unmeasured": match result { Ok(out) => out.status.to_string(), Err(error) => error }}),
    };
    for owner in observation
        .owners
        .iter()
        .filter(|o| o.path == observation.socket_path)
    {
        let pid = owner.pid.to_string();
        // No argv or environment: both can contain the worker's credentials.
        let stats = output(
            "ps",
            &["-p", &pid, "-o", "pid=,ppid=,stat=,pcpu=,rss=,comm="],
        )
        .await;
        let mut sample = serde_json::json!({"pid": owner.pid, "process_stats": stats.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())});
        if cfg!(target_os = "macos") {
            let path = dir.join(format!(
                "tmux-stall-{}-{}-{trigger}.sample.txt",
                now as u64, owner.pid
            ));
            let path_str = path.to_string_lossy().into_owned();
            let result = output("/usr/bin/sample", &[&pid, "1", "-file", &path_str]).await;
            let has_stacks =
                std::fs::read_to_string(&path).is_ok_and(|text| text.contains("Thread_"));
            sample["stack_sample"] = serde_json::json!({"path": path,
                "measured": result.as_ref().is_ok_and(|o| o.status.success()) && has_stacks,
                "has_sampled_threads": has_stacks, "error": result.err()});
        }
        evidence["samples"].as_array_mut().unwrap().push(sample);
    }
    let result =
        std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&receipt, evidence.to_string()));
    match result {
        Ok(()) => {
            tracing::warn!(target: "amux::tmux", verdict = "live_server_stall_evidence_saved", trigger,
                ownership_verdict = %observation.verdict, path = %receipt.display(),
                "tmux incident evidence saved with host load and process summary")
        }
        Err(error) => {
            tracing::warn!(target: "amux::tmux", verdict = "stall_evidence_write_failed", %error,
            "could not persist tmux stall evidence")
        }
    }
}

impl Observation {
    pub fn invariant(&self) -> InvariantResult {
        let id = "session.tmux_socket_has_one_live_owner";
        let result = match self.verdict.as_str() {
            "unmeasured" => InvariantResult::unknown(id, self.why_unmeasured.as_deref().unwrap_or("socket ownership was not measured")),
            "multiple_socket_owners" | "live_server_unreachable" => InvariantResult::fail(id,
                "one reachable tmux server per socket path",
                format!("{}: responding_pid={:?}, socket={}, owners={:?}; workers can still be alive behind an unreachable or replaced socket",
                    self.verdict, self.responding_pid, self.socket_path, self.owners)),
            _ => InvariantResult::pass(id),
        };
        if socket_ownership_failure(&self.verdict)
            && crate::log_dedupe::first_this_bucket(
                &format!("tmux-ownership:{}:{:?}", self.verdict, self.owners),
                crate::log_dedupe::hour_bucket(crate::config::now_f64()),
            )
        {
            tracing::warn!(target: "amux::tmux", verdict = %self.verdict,
                socket = %self.socket_path, responding_pid = ?self.responding_pid,
                owners = ?self.owners, "tmux socket ownership disagrees with fleet discovery; preserve live workers before attempting recovery (AMUX-4203)");
        }
        result.evidence(serde_json::to_value(self).unwrap_or_default())
    }

    fn may_create_server(&self) -> Result<bool, String> {
        // -N closes the race between this successful probe and new-session:
        // even if the listener stops answering, tmux cannot replace it.
        if self.responding_pid.is_some() {
            return Ok(false);
        }
        if !self.measured || self.owners.iter().any(|o| o.path == self.socket_path) {
            return Err(format!("tmux server is unreachable; refusing to replace its socket. verdict={}, socket={}, owners={:?}, probe_error={:?}, why_unmeasured={:?}; GET /api/debug/tmux", self.verdict, self.socket_path, self.owners, self.probe_error, self.why_unmeasured));
        }
        Ok(true)
    }
}

/// Env var a test sets to say it MEANS to spawn on a real tmux server.
pub const SPAWN_OVERRIDE: &str = "AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME";

/// Directory prefixes that mean "this AMUX_HOME is a throwaway".
///
/// `/tmp` and `/private/tmp` are the same volume on macOS and either spelling
/// can reach the caller, so both are listed rather than resolved.
const THROWAWAY_PREFIXES: &[&str] = &[
    "/tmp/",
    "/private/tmp/",
    "/var/folders/",
    "/private/var/folders/",
];

/// Whether a worker may be spawned given the AMUX_HOME it would be spawned from.
///
/// `test_env::set_home` isolates AMUX_HOME and NOTHING ISOLATES TMUX, so any
/// test whose path reaches `start_session` creates a real session on the
/// machine's real tmux server, and the test cannot tell: from inside, every
/// call returns as though it worked.
///
/// That is not hypothetical. A fixture lane called `client-left-fixture` was
/// created this way, running Claude Code in /Users/ethan. The server adopted it
/// ("re-armed pipe-pane"), the staged guard named it as a committer, and
/// ba203699 permanently carries `Amux-Committer: client-left-fixture` for a
/// lane that never existed (AMUX-4602, AMUX-4724).
///
/// PURE, so the rule can be tested without a tmux server: the whole class of
/// bug here is code that only misbehaves when it reaches the real one.
///
/// This REFUSES rather than redirecting to a private socket. Measured over the
/// full suite by instrumenting `start_session` itself: exactly 2 of ~2940 tests
/// reach it, `pause_prevents_legacy_start_before_provider_launch` and
/// `pause_legacy_failure_and_resume_failure_are_honest`, and BOTH are asserting
/// that a paused lane is refused, so both return before any tmux call. Nothing
/// exercises the spawn, so a private socket would be isolating a path no test
/// travels. `SPAWN_OVERRIDE` keeps that door open for the test that one day
/// wants it.
pub(crate) fn spawn_allowed_from(home: &std::path::Path, override_on: bool) -> Result<(), String> {
    if override_on {
        return Ok(());
    }
    let h = home.to_string_lossy();
    // Trailing separator matters: `/tmp/x` is a throwaway and a hypothetical
    // `/tmpdata/amux` is not.
    let h_slash = if h.ends_with('/') {
        h.to_string()
    } else {
        format!("{h}/")
    };
    for p in THROWAWAY_PREFIXES {
        if h_slash.starts_with(p) {
            return Err(format!(
                "refusing to spawn a worker: AMUX_HOME is {h}, a throwaway directory, so this is almost certainly a test. \
                 Nothing isolates tmux from AMUX_HOME, so the spawn would land on the real tmux server and create a live \
                 Claude Code session on this machine (AMUX-4724). Set {SPAWN_OVERRIDE}=1 if the spawn is genuinely intended."
            ));
        }
    }
    Ok(())
}

/// `spawn_allowed_from` against the live environment.
pub(crate) fn spawn_allowed_here() -> Result<(), String> {
    let on = std::env::var(SPAWN_OVERRIDE)
        .map(|v| v == "1")
        .unwrap_or(false);
    let r = spawn_allowed_from(&crate::api::session_verbs::home(), on);
    if let Err(error) = &r {
        tracing::warn!(target: "amux::tmux", verdict = "spawn_refused_throwaway_home", %error,
            "worker start refused before tmux could create a session from a test home");
    }
    r
}

/// Whether new-session may create a server. False means pass tmux's -N flag.
pub async fn may_create_server() -> Result<bool, String> {
    let observation = observe().await;
    let _ = observation.invariant();
    let result = observation.may_create_server();
    if let Err(error) = &result {
        tracing::warn!(target: "amux::tmux", verdict = "spawn_refused_live_or_unmeasured_server", %error,
            "worker start refused before tmux could replace a live fleet's socket");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AMUX-4724. Nothing isolates tmux from AMUX_HOME, so a test that reaches
    /// the spawn creates a real session on the operator's machine.
    #[test]
    fn a_throwaway_amux_home_may_not_spawn_a_worker() {
        // Every throwaway prefix refuses, asserted one at a time rather than
        // through a loop over the same constant the code reads: a loop over
        // THROWAWAY_PREFIXES would pass for an empty list.
        for h in [
            "/tmp/amux-test-home",
            "/private/tmp/claude-501/x/y/scratch",
            "/var/folders/0x/abc/T/.tmpXYZ",
            "/private/var/folders/0x/abc/T/.tmpXYZ",
        ] {
            assert!(
                spawn_allowed_from(std::path::Path::new(h), false).is_err(),
                "must refuse to spawn from throwaway home {h}"
            );
        }

        // POSITIVE CONTROL: a real home still spawns. Without this the cell
        // passes for a guard that refuses everything and stops the fleet.
        assert!(
            spawn_allowed_from(std::path::Path::new("/Users/ethan/.amux"), false).is_ok(),
            "a real AMUX_HOME must still be allowed to spawn"
        );

        // PREFIX, NOT SUBSTRING. `/tmpdata` is not under `/tmp`, and a
        // starts_with check without the separator would refuse it forever.
        assert!(
            spawn_allowed_from(std::path::Path::new("/tmpdata/amux"), false).is_ok(),
            "/tmpdata is not a throwaway directory"
        );

        // THE THROWAWAY ROOT ITSELF, with no trailing separator. This cell
        // exists because a mutation that deleted the normalization stayed GREEN:
        // the assertion above is carried by the prefix's own trailing slash, so
        // it tested nothing about the normalization. `"/tmp".starts_with("/tmp/")`
        // is false, so without it an AMUX_HOME of exactly /tmp spawns.
        for root in ["/tmp", "/private/tmp", "/var/folders"] {
            assert!(
                spawn_allowed_from(std::path::Path::new(root), false).is_err(),
                "the throwaway root {root} itself must refuse, not just paths under it"
            );
        }

        // The override is the documented way out, so a test that genuinely
        // means to spawn is not left without a path (ethos rule 3).
        assert!(
            spawn_allowed_from(std::path::Path::new("/tmp/amux-test-home"), true).is_ok(),
            "{SPAWN_OVERRIDE} must permit a deliberate spawn"
        );

        // And the refusal has to SAY so, or the next author reads it as a wall.
        let msg = spawn_allowed_from(std::path::Path::new("/tmp/h"), false).unwrap_err();
        assert!(
            msg.contains(SPAWN_OVERRIDE),
            "refusal must name its override: {msg}"
        );
        assert!(
            msg.contains("/tmp/h"),
            "refusal must name the home it refused: {msg}"
        );
    }

    /// AMUX-4617. RSS is the wrong discriminator on a host with memory
    /// compression, and the specimen is the reason: the procwarden menubar agent
    /// held 27 GB with 26 GB of it compressed and 4 MB resident, so it ranked
    /// nowhere near the top by RSS while being the largest consumer on the box.
    #[test]
    fn processes_rank_by_footprint_so_a_compressed_hog_cannot_hide() {
        // THE SPECIMEN, in top's own format. procwarden is last by resident
        // memory and first by footprint, which is the whole inversion.
        let raw = "\
PID    MEM CMPRS %CPU COMMAND
101    4096B 26G 0.1 procwarden
202    2G 0B 5.0 honest-big
303    900M 100M 1.0 middling
bogus  1G 1G 1.0 unparseable
404    NaN 1G 1.0 bad-number
";
        let ps = "\
101 1 S 0.1 4 /Users/e/Dev/smb/backend/.venv/bin/python
202 1 S 5.0 2097152 /usr/bin/honest
303 1 S 1.0 921600 /Users/e/Dev/other/bin/tool
";
        // CC_DIRs NEST in reality: measured 2026-09-16, /Users/ethan/Dev is
        // itself a lane's CC_DIR alongside /Users/ethan/Dev/ai-for-smbs, so a
        // lane process matches several. The fixture reproduces that.
        let lanes = vec![
            ("/Users/e/Dev".to_string(), "broad-lane".to_string()),
            ("/Users/e/Dev/smb".to_string(), "smb-lane".to_string()),
            ("/Users/e/Dev/oth".to_string(), "decoy-lane".to_string()),
        ];
        let v = footprint_summary(raw, ps, &lanes);
        assert_eq!(v["measured"], true);
        // The header, the non-numeric pid and the NaN row are all dropped.
        assert_eq!(
            v["n_considered"], 3,
            "only parseable process rows count: {v}"
        );

        let top = &v["top_footprint"];
        assert_eq!(
            top[0]["pid"], 101,
            "the compressed hog must rank FIRST: {top}"
        );
        assert_eq!(top[0]["footprint_bytes"], 26u64 * 1024 * 1024 * 1024 + 4096);
        assert_eq!(
            top[0]["resident_bytes"], 4096,
            "and it is tiny resident, which is why RSS missed it"
        );

        // POSITIVE CONTROL: a genuinely resident process still ranks by its real
        // size, so this is a better ranking rather than one that simply prefers
        // compression.
        assert_eq!(top[1]["pid"], 202);
        assert_eq!(top[1]["compressed_bytes"], 0);

        // THE ORDER IS THE CLAIM. Under the old RSS ranking 101 would be LAST of
        // the three; asserting only that it appears would pass for a ranking
        // that never changed.
        let order: Vec<u64> = top
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["pid"].as_u64().unwrap())
            .collect();
        assert_eq!(
            order,
            vec![101, 202, 303],
            "ranked by footprint, not residency: {top}"
        );

        // Unit suffixes carry the magnitude: a numeric parse alone reads 16G as
        // sixteen and would sort it below a 900M row.
        assert_eq!(top_size_bytes("16G"), Some(16 * 1024 * 1024 * 1024));
        assert_eq!(top_size_bytes("7904K"), Some(7904 * 1024));
        assert_eq!(top_size_bytes("4096B"), Some(4096));
        assert_eq!(top_size_bytes("12"), Some(12));
        assert_eq!(top_size_bytes("NaN"), None);

        // An empty probe is UNMEASURED, not a healthy host with no processes.
        // OWNER, from the executable path, because cwd is unavailable on this
        // host. The longest matching CC_DIR wins: pid 101 is under both
        // /Users/e/Dev and /Users/e/Dev/smb, and naming the broad one would
        // blame the wrong lane.
        assert_eq!(
            top[0]["owner_lane"], "smb-lane",
            "longest CC_DIR prefix must win: {top}"
        );
        // A process outside every lane tree has NO owner, which is null rather
        // than a guess or the string "unknown".
        assert!(
            top[1]["owner_lane"].is_null(),
            "/usr/bin/honest belongs to no lane: {top}"
        );
        // BOUNDARY, not substring: /Users/e/Dev/other must not match a lane
        // rooted at /Users/e/Dev/oth.
        assert_eq!(
            top[2]["owner_lane"], "broad-lane",
            "decoy-lane is a prefix of the string, not of the path: {top}"
        );

        let none = footprint_summary("", "", &lanes);
        assert_eq!(none["measured"], false);
        assert_eq!(none["n_considered"], 0);
        assert!(
            none["why_unmeasured"].is_string(),
            "silence must say why: {none}"
        );
    }

    #[test]
    fn tmux_host_evidence_ranks_cpu_and_memory_independently() {
        let summary = host_process_summary(
            "11 1 R 150.0 1024 /usr/bin/busy\n22 1 S 0.1 999999 /Applications/Memory User\n",
        );
        assert_eq!(summary["measured"], true);
        assert_eq!(summary["n_considered"], 2);
        assert_eq!(summary["top_cpu"][0]["pid"], 11);
        assert_eq!(summary["top_rss"][0]["pid"], 22);
        assert_eq!(
            summary["top_rss"][0]["executable"],
            "/Applications/Memory User"
        );
        let invalid = host_process_summary("ps failed\n11 1 R NaN 5 bad\n");
        assert_eq!(invalid["measured"], false);
        assert_eq!(invalid["n_considered"], 0);
        assert!(invalid["why_unmeasured"].is_string());
    }

    #[test]
    fn tmux_socket_invariant_negative_control() {
        let owners = parse_owners("p3179\nctmux\nf6\nn/socket/default\np52849\nctmux\nf6\nn/socket/default\np42\nctmux-client\nn/socket/default\n");
        assert_eq!(owners.len(), 2);
        assert_eq!(
            verdict(Some(52849), &owners, "/socket/default", true),
            "multiple_socket_owners"
        );
        assert_eq!(
            verdict(None, &owners[..1], "/socket/default", true),
            "live_server_unreachable"
        );
        assert_eq!(
            verdict(Some(3179), &owners[..1], "/socket/default", true),
            "ok"
        );
        assert_eq!(
            verdict(None, &owners, "/different/socket", true),
            "no_server"
        );
        assert_eq!(verdict(None, &[], "/socket/default", false), "unmeasured");
        assert!(socket_ownership_failure("multiple_socket_owners"));
        assert!(socket_ownership_failure("live_server_unreachable"));
        assert!(!socket_ownership_failure("ok"));
        assert!(!socket_ownership_failure("no_server"));
    }

    #[test]
    fn tmux_start_requires_proof_before_replacing_socket() {
        let mut o = Observation {
            measured: true,
            n_considered: 1,
            why_unmeasured: None,
            socket_path: "/socket/default".into(),
            responding_pid: None,
            probe_error: Some("connection refused".into()),
            owners: vec![SocketOwner {
                pid: 3179,
                path: "/socket/default".into(),
            }],
            verdict: "live_server_unreachable".into(),
        };
        assert!(o.may_create_server().is_err());
        o.responding_pid = Some(3179);
        assert_eq!(o.may_create_server(), Ok(false));
        o.responding_pid = None;
        o.owners.clear();
        assert_eq!(o.may_create_server(), Ok(true));
        o.measured = false;
        assert!(o.may_create_server().is_err());
    }

    fn lsof_output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    // Shape of lsof 4.98.0 stderr on a Docker host with no tmux server; paths are placeholders.
    const DOCKER_HOST_STDERR: &str = "\
lsof: WARNING: can't stat() overlay file system /var/lib/docker/overlay2/<id>/merged
      Output information may be incomplete.
lsof: WARNING: can't stat() nsfs file system /run/docker/netns/<id>
      Output information may be incomplete.
lsof: WARNING: can't stat() fuse.portal file system /run/user/<uid>/doc
      Output information may be incomplete.
";

    #[test]
    fn lsof_no_owners_with_mount_stat_warnings_is_measured() {
        let o = lsof_output(1, "", DOCKER_HOST_STDERR);
        assert!(
            lsof_enumerated(&o),
            "stat warnings about unrelated mounts must not make an empty result unmeasured"
        );
        assert!(lsof_enumerated(&lsof_output(1, "", "")));
        assert!(lsof_enumerated(&lsof_output(
            0,
            "p1\nctmux\nn/tmp/tmux-1000/default\n",
            DOCKER_HOST_STDERR
        )));
    }

    #[test]
    fn lsof_real_failures_stay_unmeasured() {
        let real = format!("{DOCKER_HOST_STDERR}lsof: can't open /proc: Permission denied\n");
        assert!(
            !lsof_enumerated(&lsof_output(1, "", &real)),
            "a genuine error beside benign warnings is still a failure"
        );
        assert!(!lsof_enumerated(&lsof_output(
            1,
            "",
            "lsof: illegal option character: Z\n"
        )));
        assert!(
            !lsof_enumerated(&lsof_output(1, "p1\n", DOCKER_HOST_STDERR)),
            "exit 1 with output is not the no-match case"
        );
        assert!(!lsof_enumerated(&lsof_output(2, "", DOCKER_HOST_STDERR)));
    }
}
