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
        Ok(o) if lsof_enumerated(&o) => {
            (parse_owners(&String::from_utf8_lossy(&o.stdout)), None)
        }
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
        if let Err(error) = std::thread::Builder::new().name("tmux-timeout-evidence".into()).spawn(move || {
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(async {
                    let observation = observe_with_evidence(false).await;
                    capture_stall_evidence(&observation, "fleet_probe_timeout", Some(probe)).await;
                }),
                Err(error) => tracing::warn!(target: "amux::tmux", %error,
                    verdict = "stall_evidence_runtime_failed", "timeout evidence was not measured"),
            }
        }) {
            tracing::warn!(target: "amux::tmux", %error,
                verdict = "stall_evidence_thread_failed", "timeout evidence was not measured");
        }
    }
}

fn host_process_summary(raw: &str) -> serde_json::Value {
    let mut rows = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(state), Some(cpu), Some(rss)) =
            (fields.next(), fields.next(), fields.next(), fields.next(), fields.next()) else { continue };
        let (Ok(pid), Ok(ppid), Ok(cpu), Ok(rss)) =
            (pid.parse::<u32>(), ppid.parse::<u32>(), cpu.parse::<f64>(), rss.parse::<u64>()) else { continue };
        if !cpu.is_finite() { continue; }
        rows.push(serde_json::json!({"pid": pid, "ppid": ppid, "state": state,
            "cpu_pct": cpu, "rss_kib": rss, "executable": fields.collect::<Vec<_>>().join(" ")}));
    }
    let count = rows.len();
    rows.sort_by(|a,b| b["cpu_pct"].as_f64().partial_cmp(&a["cpu_pct"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    let cpu: Vec<_> = rows.iter().take(20).cloned().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row["rss_kib"].as_u64()));
    serde_json::json!({"measured": count > 0, "n_considered": count,
        "why_unmeasured": if count == 0 { Some("ps returned no parseable process rows") } else { None },
        "top_cpu": cpu, "top_rss": rows.into_iter().take(20).collect::<Vec<_>>()})
}

async fn capture_stall_evidence(observation: &Observation, trigger: &str, probe: Option<serde_json::Value>) {
    let now = crate::config::now_f64();
    let key = format!("tmux-stall-evidence:{trigger}:{:?}", observation.owners);
    let bucket = if trigger == "fleet_probe_timeout" { (now / 60.0) as i64 } else { crate::log_dedupe::hour_bucket(now) };
    if !crate::log_dedupe::first_this_bucket(&key, bucket) {
        return;
    }
    let dir = crate::api::session_verbs::home().join("logs");
    let receipt = dir.join(format!("tmux-stall-{}-{trigger}.json", now as u64));
    let mut evidence =
        serde_json::json!({"at": now, "trigger": trigger, "probe": probe, "socket_ownership": observation, "samples": []});
    let (processes, uptime) = tokio::join!(
        output("ps", &["-A", "-o", "pid=,ppid=,stat=,pcpu=,rss=,comm="]),
        output("uptime", &[]),
    );
    evidence["host_processes"] = match processes {
        Ok(out) if out.status.success() => host_process_summary(&String::from_utf8_lossy(&out.stdout)),
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
            let has_stacks = std::fs::read_to_string(&path).is_ok_and(|text| text.contains("Thread_"));
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

    #[test]
    fn tmux_host_evidence_ranks_cpu_and_memory_independently() {
        let summary = host_process_summary("11 1 R 150.0 1024 /usr/bin/busy\n22 1 S 0.1 999999 /Applications/Memory User\n");
        assert_eq!(summary["measured"], true);
        assert_eq!(summary["n_considered"], 2);
        assert_eq!(summary["top_cpu"][0]["pid"], 11);
        assert_eq!(summary["top_rss"][0]["pid"], 22);
        assert_eq!(summary["top_rss"][0]["executable"], "/Applications/Memory User");
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
        assert!(lsof_enumerated(&o), "stat warnings about unrelated mounts must not make an empty result unmeasured");
        assert!(lsof_enumerated(&lsof_output(1, "", "")));
        assert!(lsof_enumerated(&lsof_output(0, "p1\nctmux\nn/tmp/tmux-1000/default\n", DOCKER_HOST_STDERR)));
    }

    #[test]
    fn lsof_real_failures_stay_unmeasured() {
        let real = format!("{DOCKER_HOST_STDERR}lsof: can't open /proc: Permission denied\n");
        assert!(!lsof_enumerated(&lsof_output(1, "", &real)), "a genuine error beside benign warnings is still a failure");
        assert!(!lsof_enumerated(&lsof_output(1, "", "lsof: illegal option character: Z\n")));
        assert!(!lsof_enumerated(&lsof_output(1, "p1\n", DOCKER_HOST_STDERR)), "exit 1 with output is not the no-match case");
        assert!(!lsof_enumerated(&lsof_output(2, "", DOCKER_HOST_STDERR)));
    }
}
