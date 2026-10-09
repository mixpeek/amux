//! Host guard: one computed pressure level for the machine, automation that
//! backs off when it is critical, and long-running worker tool processes
//! lowered in priority when the host is strained.
//!
//! # Why this exists
//!
//! Everything amux had for host resources OBSERVED (`host_metrics`,
//! `disk_watch`, `memory_consumers`) or reaped one named class of orphan
//! (`mac_health`). Nothing connected "the machine is drowning" to what amux
//! itself does next, and nothing noticed a worker's own tool process running
//! away. Measured 2026-09-26 on a 28-core host: load average 36 to 45 for
//! hours, while the board kept dispatching and schedules kept firing, and a
//! worker's `grep -r` over 21 GB of transcripts had held a full core for 73
//! minutes with no signal anywhere.
//!
//! # What it does, and deliberately does not
//!
//! 1. **Level.** Every tick samples load per core (5-minute average, so one
//!    burst does not flap the level), macOS memory pressure (or Linux
//!    MemAvailable) and free disk on the amux home volume, and publishes
//!    `ok | strained | critical` with the reasons. A probe that cannot run is
//!    reported as unmeasured and never treated as pressure.
//! 2. **Automation backs off at critical.** [`admit_automation`] is asked by
//!    loops that START work on their own (board dispatch, schedule fires,
//!    automated worker starts). At critical they skip that tick and retry on
//!    the next one; nothing is dropped. Owner actions never consult it.
//! 3. **Runaway tool processes are reniced, never killed.** A process under a
//!    worker's pane that has averaged most of a core for a long stretch gets
//!    its nice value raised while the host is strained or worse. That is
//!    reversible and costs the work nothing but speed. Killing stays a human
//!    decision (ethos rule 8); the endpoint lists every candidate so the owner
//!    can decide.
//!
//! `GET /api/metrics/host/pressure` serves the latest sample.

use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock, RwLock};

use super::registry::ids;

const JOB: &str = ids::HOST_GUARD;

fn env_f64(k: &str, default: f64) -> f64 {
    std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Load per core (5-minute average) at which the host is strained / critical.
fn load_strained() -> f64 { env_f64("AMUX_HOST_LOAD_STRAINED", 1.5) }
fn load_critical() -> f64 { env_f64("AMUX_HOST_LOAD_CRITICAL", 2.5) }
/// Free disk (GiB) on the amux home volume below which the host is strained / critical.
fn disk_strained_gb() -> f64 { env_f64("AMUX_HOST_DISK_STRAINED_GB", 40.0) }
fn disk_critical_gb() -> f64 { env_f64("AMUX_HOST_DISK_CRITICAL_GB", 10.0) }
/// Runaway: a worker tool process older than this, averaging at least
/// `AMUX_RUNAWAY_AVG_CORES` of CPU over its whole life.
fn runaway_min_secs() -> f64 { env_f64("AMUX_RUNAWAY_MIN_SECS", 900.0) }
fn runaway_avg_cores() -> f64 { env_f64("AMUX_RUNAWAY_AVG_CORES", 0.8) }
/// Nice value a runaway is lowered to. `AMUX_RUNAWAY_RENICE=0` reports only.
fn renice_enabled() -> bool { std::env::var("AMUX_RUNAWAY_RENICE").map(|v| v.trim() != "0").unwrap_or(true) }
const RENICE_TO: i32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Strained,
    Critical,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Strained => "strained",
            Level::Critical => "critical",
        }
    }
}

/// Raw readings. `None` means that probe did not run.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Readings {
    pub ncpu: usize,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    /// macOS `kern.memorystatus_vm_pressure_level` (1 normal, 2 warn, 4 critical),
    /// or on Linux 1/2/4 derived from MemAvailable.
    pub mem_level: Option<u8>,
    pub disk_free_gb: Option<f64>,
}

/// Classify readings into a level plus the reasons that produced it. Pure.
pub fn classify(r: &Readings) -> (Level, Vec<String>) {
    let mut level = Level::Ok;
    let mut reasons = Vec::new();
    let mut raise = |l: Level, why: String| {
        if l > Level::Ok {
            reasons.push(why);
        }
        if l > level {
            level = l;
        }
    };
    if let (Some(l5), n) = (r.load5, r.ncpu.max(1)) {
        let per = l5 / n as f64;
        let l = if per >= load_critical() { Level::Critical } else if per >= load_strained() { Level::Strained } else { Level::Ok };
        raise(l, format!("load {per:.2}/core over 5 min ({l5:.1} on {n} cores)"));
    }
    if let Some(m) = r.mem_level {
        let l = match m { 4.. => Level::Critical, 2..=3 => Level::Strained, _ => Level::Ok };
        raise(l, format!("memory pressure level {m}"));
    }
    if let Some(gb) = r.disk_free_gb {
        let l = if gb < disk_critical_gb() { Level::Critical } else if gb < disk_strained_gb() { Level::Strained } else { Level::Ok };
        raise(l, format!("{gb:.0} GiB free on the amux home volume"));
    }
    (level, reasons)
}

/// One worker tool process judged a runaway.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Runaway {
    pub worker: String,
    pub pid: u32,
    pub command: String,
    pub elapsed_s: u64,
    pub avg_cores: f64,
    pub nice: i32,
    /// "reniced", "already_low", "report_only" (host ok, or renice disabled), or "renice_failed: ..."
    pub action: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostPressure {
    pub level: Level,
    pub reasons: Vec<String>,
    pub readings: Readings,
    /// False only when NO probe produced a reading.
    pub measured: bool,
    pub why_unmeasured: Option<String>,
    /// Worker tool processes considered for the runaway check.
    pub n_considered: usize,
    pub runaways: Vec<Runaway>,
    /// Automated ticks skipped because the host was critical, by loop, since boot.
    pub deferred: BTreeMap<String, u64>,
    pub sampled_at: i64,
}

#[cfg(not(test))]
fn slot() -> &'static RwLock<Option<HostPressure>> {
    static S: OnceLock<RwLock<Option<HostPressure>>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(None))
}

/// Per test thread, so a test that makes the host critical cannot defer an
/// unrelated scheduler or board test running beside it.
#[cfg(test)]
fn slot() -> &'static RwLock<Option<HostPressure>> {
    thread_local! {
        static S: &'static RwLock<Option<HostPressure>> = Box::leak(Box::new(RwLock::new(None)));
    }
    S.with(|s| *s)
}

/// Test hook: make this thread's latest sample `level` (fresh, measured), or none.
#[cfg(test)]
pub fn set_level_for_test(level: Option<Level>) {
    *slot().write().unwrap() = level.map(|level| HostPressure {
        level, reasons: vec!["test pressure".into()], readings: Readings::default(), measured: true,
        why_unmeasured: None, n_considered: 0, runaways: vec![], deferred: BTreeMap::new(),
        sampled_at: chrono::Utc::now().timestamp(),
    });
}

/// (deferred count per loop, last time each loop's deferral was logged)
type Deferrals = (BTreeMap<String, u64>, HashMap<String, i64>);

fn deferrals() -> &'static Mutex<Deferrals> {
    static D: OnceLock<Mutex<Deferrals>> = OnceLock::new();
    D.get_or_init(|| Mutex::new((BTreeMap::new(), HashMap::new())))
}

/// The latest sample, if the guard has run.
pub fn current() -> Option<HostPressure> {
    let mut snap = slot().read().ok().and_then(|g| g.clone())?;
    if let Ok(d) = deferrals().lock() {
        snap.deferred = d.0.clone();
    }
    Some(snap)
}

/// The reasons, when the latest sample is measured, critical and fresh
/// (under 3 minutes old). A missing or stale sample is never critical, so a
/// dead guard can never freeze the fleet. Does not count a deferral.
pub fn critical_now() -> Option<String> {
    let p = slot().read().ok().and_then(|g| g.clone())?;
    let now = chrono::Utc::now().timestamp();
    (p.measured && p.level == Level::Critical && now - p.sampled_at <= 180).then(|| p.reasons.join("; "))
}

/// Should an AUTOMATED loop start new work right now? False only while
/// [`critical_now`]. A refusal is counted and logged (at most once a minute
/// per loop).
pub fn admit_automation(what: &str) -> bool {
    let Some(reasons) = critical_now() else { return true };
    let now = chrono::Utc::now().timestamp();
    if let Ok(mut d) = deferrals().lock() {
        *d.0.entry(what.to_string()).or_insert(0) += 1;
        let last = d.1.get(what).copied().unwrap_or(0);
        if now - last >= 60 {
            d.1.insert(what.to_string(), now);
            let total = d.0[what];
            tracing::warn!(target: "amux::host_guard", verdict = "host_pressure_deferred", loop_name = what,
                deferred_total = total, reasons = %reasons, measured = true, n_considered = 1,
                "host is critical; automated {what} skipped this tick and will retry");
        }
    }
    false
}

// ---- probes ---------------------------------------------------------------

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `{ 18.59 31.13 39.70 }` (macOS sysctl) or `18.59 31.13 39.70 ...` (/proc/loadavg).
pub fn parse_loadavg(s: &str) -> Option<(f64, f64)> {
    let mut it = s.split(|c: char| c.is_whitespace() || c == '{' || c == '}').filter(|t| !t.is_empty());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn probe_load() -> Option<(f64, f64)> {
    run("sysctl", &["-n", "vm.loadavg"])
        .and_then(|s| parse_loadavg(&s))
        .or_else(|| std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| parse_loadavg(&s)))
}

/// Linux: map MemAvailable/MemTotal onto the macOS 1/2/4 scale.
pub fn linux_mem_level(meminfo: &str) -> Option<u8> {
    let field = |k: &str| {
        meminfo.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<f64>().ok())
    };
    let (avail, total) = (field("MemAvailable:")?, field("MemTotal:")?);
    let pct = avail / total.max(1.0);
    Some(if pct < 0.05 { 4 } else if pct < 0.10 { 2 } else { 1 })
}

fn probe_mem_level() -> Option<u8> {
    run("sysctl", &["-n", "kern.memorystatus_vm_pressure_level"])
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| linux_mem_level(&s)))
}

/// `df -Pk <path>` last line, 4th column = available KiB.
pub fn parse_df_free_gb(s: &str) -> Option<f64> {
    let kb: f64 = s.lines().last()?.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb / 1024.0 / 1024.0)
}

fn probe_disk_free_gb() -> Option<f64> {
    let home = crate::config::amux_home();
    run("df", &["-Pk", &home.to_string_lossy()]).and_then(|s| parse_df_free_gb(&s))
}

pub fn sample_readings() -> Readings {
    let (load1, load5) = probe_load().map(|(a, b)| (Some(a), Some(b))).unwrap_or((None, None));
    Readings {
        ncpu: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        load1,
        load5,
        mem_level: probe_mem_level(),
        disk_free_gb: probe_disk_free_gb(),
    }
}

// ---- runaway detection ------------------------------------------------------

/// ps `time`: `MM:SS.ss`, `HH:MM:SS` or `D-HH:MM:SS` (minutes can exceed 59).
pub fn parse_cputime(s: &str) -> Option<f64> {
    parse_clock(s)
}

/// ps `etime`: `[[D-]HH:]MM:SS`.
pub fn parse_etime(s: &str) -> Option<f64> {
    parse_clock(s)
}

fn parse_clock(s: &str) -> Option<f64> {
    let s = s.trim();
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, s),
    };
    let parts: Vec<f64> = rest.split(':').map(|p| p.parse::<f64>().ok()).collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [m, sec] => m * 60.0 + sec,
        [h, m, sec] => h * 3600.0 + m * 60.0 + sec,
        _ => return None,
    };
    Some(days * 86400.0 + secs)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub cpu_s: f64,
    pub elapsed_s: f64,
    pub nice: i32,
    pub comm: String,
}

/// Parse `ps -axo pid=,ppid=,time=,etime=,nice=,comm=`.
pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let cpu_s = parse_cputime(it.next()?)?;
            let elapsed_s = parse_etime(it.next()?)?;
            let nice = it.next()?.parse().ok()?;
            let comm = it.collect::<Vec<_>>().join(" ");
            Some(Proc { pid, ppid, cpu_s, elapsed_s, nice, comm })
        })
        .collect()
}

/// Agent CLIs are the worker itself, not a tool it ran; never reniced.
fn is_agent_cli(comm: &str) -> bool {
    let base = comm.rsplit('/').next().unwrap_or(comm);
    matches!(base, "claude" | "codex" | "gemini" | "opencode" | "muse" | "grok" | "tmux" | "amux-server-rs" | "amux-server")
}

/// Worker tool processes that meet the runaway bar. `panes` maps a pane's
/// root pid to its worker name. Pure, so the criteria are testable.
pub fn find_runaways(procs: &[Proc], panes: &HashMap<u32, String>, min_secs: f64, avg_cores: f64) -> (Vec<(String, Proc, f64)>, usize) {
    let parent: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let owner = |mut pid: u32| -> Option<&String> {
        for _ in 0..64 {
            if let Some(w) = panes.get(&pid) {
                return Some(w);
            }
            pid = *parent.get(&pid)?;
            if pid <= 1 {
                return None;
            }
        }
        None
    };
    let mut considered = 0;
    let mut out = Vec::new();
    for p in procs {
        if panes.contains_key(&p.pid) || is_agent_cli(&p.comm) {
            continue;
        }
        let Some(worker) = owner(p.ppid) else { continue };
        considered += 1;
        if p.elapsed_s < min_secs {
            continue;
        }
        let avg = p.cpu_s / p.elapsed_s.max(1.0);
        if avg >= avg_cores {
            out.push((worker.clone(), p.clone(), avg));
        }
    }
    out.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    (out, considered)
}

/// tmux `#{session_name} #{pane_pid}` lines; amux sessions are `amux-<worker>`.
pub fn parse_panes(text: &str) -> HashMap<u32, String> {
    text.lines()
        .filter_map(|l| {
            let (sess, pid) = l.trim().rsplit_once(' ')?;
            let worker = sess.strip_prefix("amux-").unwrap_or(sess);
            Some((pid.parse().ok()?, worker.to_string()))
        })
        .collect()
}

fn renice(pid: u32) -> Result<(), String> {
    let out = std::process::Command::new("renice")
        .args(["-n", &RENICE_TO.to_string(), "-p", &pid.to_string()])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().chars().take(120).collect())
    }
}

fn check_runaways(level: Level) -> (Vec<Runaway>, usize) {
    let Some(ps) = run("ps", &["-axo", "pid=,ppid=,time=,etime=,nice=,comm="]) else { return (vec![], 0) };
    let panes = run("tmux", &["list-panes", "-a", "-F", "#{session_name} #{pane_pid}"]).map(|s| parse_panes(&s)).unwrap_or_default();
    if panes.is_empty() {
        return (vec![], 0);
    }
    let (found, considered) = find_runaways(&parse_ps(&ps), &panes, runaway_min_secs(), runaway_avg_cores());
    let act = level >= Level::Strained && renice_enabled();
    let runaways = found
        .into_iter()
        .take(50)
        .map(|(worker, p, avg)| {
            let action = if p.nice >= RENICE_TO {
                "already_low".to_string()
            } else if !act {
                "report_only".to_string()
            } else {
                match renice(p.pid) {
                    Ok(()) => {
                        tracing::warn!(target: "amux::host_guard", verdict = "host_runaway_reniced", worker = %worker, pid = p.pid,
                            command = %p.comm, avg_cores = format!("{avg:.2}"), elapsed_s = p.elapsed_s as u64, host = level.as_str(),
                            measured = true, n_considered = considered,
                            "worker tool process has averaged {avg:.2} cores for {}s; lowered its priority to nice {RENICE_TO}", p.elapsed_s as u64);
                        "reniced".to_string()
                    }
                    Err(e) => format!("renice_failed: {e}"),
                }
            };
            Runaway { worker, pid: p.pid, command: p.comm.chars().take(160).collect(), elapsed_s: p.elapsed_s as u64,
                      avg_cores: (avg * 100.0).round() / 100.0, nice: p.nice, action }
        })
        .collect();
    (runaways, considered)
}

// ---- tick -------------------------------------------------------------------

async fn tick() {
    let prev = slot().read().ok().and_then(|g| g.as_ref().map(|p| p.level));
    let (readings, (level, reasons), (runaways, considered)) = tokio::task::spawn_blocking(|| {
        let r = sample_readings();
        let c = classify(&r);
        let rw = check_runaways(c.0);
        (r, c, rw)
    })
    .await
    .unwrap_or_else(|_| (Readings::default(), (Level::Ok, vec![]), (vec![], 0)));
    let measured = readings.load5.is_some() || readings.mem_level.is_some() || readings.disk_free_gb.is_some();
    if measured && prev != Some(level) {
        let from = prev.map(Level::as_str).unwrap_or("unknown");
        if level > Level::Ok {
            tracing::warn!(target: "amux::host_guard", verdict = "host_pressure_changed", from, to = level.as_str(),
                reasons = %reasons.join("; "), measured = true, n_considered = 3, "host pressure {from} -> {}", level.as_str());
        } else {
            tracing::info!(target: "amux::host_guard", verdict = "host_pressure_changed", from, to = level.as_str(),
                measured = true, n_considered = 3, "host pressure {from} -> ok");
        }
    }
    let snap = HostPressure {
        level,
        reasons,
        readings,
        measured,
        why_unmeasured: (!measured).then(|| "no probe (load, memory, disk) produced a reading".to_string()),
        n_considered: considered,
        runaways,
        deferred: BTreeMap::new(),
        sampled_at: chrono::Utc::now().timestamp(),
    };
    if let Ok(mut g) = slot().write() {
        *g = Some(snap);
    }
}

/// Every 30s: the level has to be fresh for `admit_automation` to trust it.
pub fn spawn() -> super::PeriodicTask {
    super::spawn_periodic(JOB, 30, || async { tick().await })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live probe of THIS machine, report-only (level Ok never renices).
    /// `cargo test -p amux-server --lib -- --ignored host_guard_live_probe --nocapture`
    #[test]
    #[ignore]
    fn host_guard_live_probe() {
        let r = sample_readings();
        let (level, reasons) = classify(&r);
        let (runaways, considered) = check_runaways(Level::Ok);
        println!("readings {r:?}\nlevel {level:?} {reasons:?}\nconsidered {considered}");
        for x in &runaways {
            println!("  {} pid {} {:.2} cores {}s nice {} [{}] {}", x.worker, x.pid, x.avg_cores, x.elapsed_s, x.nice, x.action, x.command);
        }
        assert!(r.load5.is_some() && r.disk_free_gb.is_some(), "probes must read on a real host");
        assert!(runaways.iter().all(|x| x.action != "reniced"), "an ok-level check must never renice");
    }

    #[test]
    fn classify_takes_the_worst_reading_and_names_it() {
        let r = Readings { ncpu: 28, load1: Some(40.0), load5: Some(42.0), mem_level: Some(1), disk_free_gb: Some(565.0) };
        let (l, why) = classify(&r);
        assert_eq!(l, Level::Strained, "{why:?}");
        assert!(why[0].contains("1.50/core"), "{why:?}");
        let r = Readings { ncpu: 28, load5: Some(10.0), mem_level: Some(4), disk_free_gb: Some(565.0), ..Default::default() };
        assert_eq!(classify(&r).0, Level::Critical);
        let r = Readings { ncpu: 28, load5: Some(10.0), mem_level: Some(1), disk_free_gb: Some(5.0), ..Default::default() };
        let (l, why) = classify(&r);
        assert_eq!((l, why.len()), (Level::Critical, 1));
        assert!(why[0].contains("GiB free"));
        // A probe that did not run is not pressure.
        assert_eq!(classify(&Readings { ncpu: 8, ..Default::default() }), (Level::Ok, vec![]));
    }

    #[test]
    fn probe_parsers_read_real_formats() {
        assert_eq!(parse_loadavg("{ 18.59 31.13 39.70 }\n"), Some((18.59, 31.13)));
        assert_eq!(parse_loadavg("0.50 0.40 0.30 1/200 999"), Some((0.5, 0.4)));
        assert_eq!(parse_cputime("336:10.17"), Some(336.0 * 60.0 + 10.17));
        assert_eq!(parse_etime("03-19:48:29"), Some(3.0 * 86400.0 + 19.0 * 3600.0 + 48.0 * 60.0 + 29.0));
        assert_eq!(parse_etime("19:43"), Some(19.0 * 60.0 + 43.0));
        assert_eq!(parse_etime("garbage"), None);
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s5 1953000000 1300000000 592445440 70% /System/Volumes/Data\n";
        assert!((parse_df_free_gb(df).unwrap() - 565.0).abs() < 1.0);
        assert_eq!(linux_mem_level("MemTotal: 1000 kB\nMemAvailable: 30 kB\n"), Some(4));
        assert_eq!(linux_mem_level("MemTotal: 1000 kB\nMemAvailable: 500 kB\n"), Some(1));
    }

    #[test]
    fn only_long_hot_tool_processes_under_a_worker_pane_are_runaways() {
        let ps = "\
  100     1   0:01.00   02:00:00  0 tmux
  200   100   5:00.00   02:00:00  0 -bash
  201   200  30:00.00   02:00:00  0 claude
  300   201  73:00.00   01:13:00  0 grep
  301   201   0:10.00   01:13:00  0 sleep
  302   201   5:00.00   00:05:00  0 cargo
  400     1 9000:00.00  03-00:00:00 0 VirtualMachine
";
        let procs = parse_ps(ps);
        assert_eq!(procs.len(), 7);
        let panes = parse_panes("amux-mixpeek-homepage-claude 200\nother 999\n");
        assert_eq!(panes.get(&200).map(String::as_str), Some("mixpeek-homepage-claude"));
        let (found, considered) = find_runaways(&procs, &panes, 900.0, 0.8);
        // grep (73 min at ~1 core) is found; sleep is idle, cargo too young,
        // claude is the agent itself, the VM is not under any worker.
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].0.as_str(), found[0].1.pid), ("mixpeek-homepage-claude", 300));
        assert_eq!(considered, 3);
    }

    #[test]
    fn automation_is_deferred_only_by_a_fresh_measured_critical_sample() {
        let now = chrono::Utc::now().timestamp();
        let set = |level: Level, measured: bool, age: i64| {
            *slot().write().unwrap() = Some(HostPressure {
                level, reasons: vec!["test".into()], readings: Readings::default(), measured,
                why_unmeasured: None, n_considered: 0, runaways: vec![], deferred: BTreeMap::new(),
                sampled_at: now - age,
            });
        };
        set(Level::Strained, true, 0);
        assert!(admit_automation("t-loop"), "strained never blocks");
        set(Level::Critical, false, 0);
        assert!(admit_automation("t-loop"), "an unmeasured sample never blocks");
        set(Level::Critical, true, 600);
        assert!(admit_automation("t-loop"), "a stale sample never blocks (a dead guard must not freeze the fleet)");
        set(Level::Critical, true, 0);
        assert!(!admit_automation("t-loop"));
        assert!(current().unwrap().deferred.get("t-loop").copied().unwrap_or(0) >= 1, "a deferral is counted");
        *slot().write().unwrap() = None;
        assert!(admit_automation("t-loop"), "no sample admits");
    }
}
