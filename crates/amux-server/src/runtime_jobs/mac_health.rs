//! macOS process health sweep (2026-08-30).
//!
//! Five categories of runaway process cost Ethan a power cycle or a restart
//! and have no other reaper:
//!
//! 1. **Orphaned Ray workers** (`ray::*`). Ray actors that outlive their
//!    cluster — detached actors in particular — pin worker processes forever.
//!    `ray stop` itself does not kill them. If the raylet is gone and these are
//!    still present, they are zombies: no work will ever reach them, but they
//!    hold CPU slots, memory, and file descriptors. Safe to kill when:
//!    the raylet is not running AND the worker has been running for longer than
//!    a short grace period (covers the teardown window).
//!
//! 2. **Orphaned `rustc` debug processes** that outlived their parent cargo run.
//!    Only relevant when a `cargo check` or `cargo test` was interrupted mid-
//!    flight. Detected by: parent PID is 1 (reparented to init = orphan) and
//!    the command line includes the debug target dir.
//!
//! 3. **Orphaned Playwright Chrome roots** using a Playwright temporary
//!    profile whose launcher is gone. Browser-owned profiles are handled by
//!    `browser_reaper`; this covers launcher crashes before ownership exists.
//!
//! 4. **Ghost Claude Code processes** — `claude` processes whose tmux pane no
//!    longer exists. The existing `ghost_rescue.rs` handles the amux-managed
//!    subset; this watches the raw-count ceiling (AMUX_MAC_HEALTH_MAX_CLAUDE)
//!    and logs when it is exceeded so the operator knows before the OOM.
//!
//! 5. **True process-table zombies** (`state=Z`). The child is already dead, so
//!    signalling it cannot clean anything. The sweep calls non-blocking
//!    `waitpid` only for an aged child owned by this amux-server process;
//!    zombies owned by another application are reported and left alone.
//!
//! WHAT THIS WILL NOT DO:
//! - Kill a process whose state it cannot verify. Silence is not dead.
//! - Kill processes the raylet is still using (raylet running = ray is live).
//! - Kill rustc processes whose parent is NOT pid 1 (they may still be running).
//! - Take any action on processes it cannot identify by full command line.
//! - Reap a zombie whose parent is not this exact server process.
//!
//! SAFE: every termination is SIGTERM, not SIGKILL, and every target class has
//! a predicate that cannot be satisfied by a legitimate process doing real work.

use std::time::Duration;

const JOB: &str = "mac-health";
const ZOMBIE_LOG_SAMPLE: usize = 8;

fn tick_secs() -> u64 {
    std::env::var("AMUX_MAC_HEALTH_TICK_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1800) // 30 minutes
}

/// Maximum number of `claude` processes before a WARN fires.
/// Default 60 — matches the observed fleet size plus headroom.
fn max_claude() -> usize {
    std::env::var("AMUX_MAC_HEALTH_MAX_CLAUDE")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(60)
}

/// How old an orphaned ray:: worker must be (seconds) before it is eligible
/// for reaping. Guards against killing workers during a graceful shutdown.
fn ray_orphan_grace_s() -> u64 {
    std::env::var("AMUX_MAC_HEALTH_RAY_GRACE_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(120)
}

fn rustc_orphan_grace_s() -> u64 {
    std::env::var("AMUX_MAC_HEALTH_RUSTC_GRACE_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(600)
}

fn zombie_grace_s() -> u64 {
    std::env::var("AMUX_MAC_HEALTH_ZOMBIE_GRACE_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(60)
}

/// True when a local raylet process is running.
/// Uses [r]aylet trick so the grep cannot match itself.
fn raylet_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", "[r]aylet"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns (pid, elapsed_seconds, command_excerpt) for each `ray::` worker
/// process whose parent is no longer the raylet (i.e., reparented to PID 1
/// or whose ppid doesn't exist in the process table).
///
/// Only called when `raylet_running()` is false — when the raylet is alive,
/// every ray:: worker is legitimate.
fn orphaned_ray_workers(grace_s: u64) -> Vec<(u32, u64, String)> {
    // ps -A -o pid,ppid,etime,command= outputs one line per process.
    // We filter for lines starting with "ray::" (detached actor workers).
    // etime format: [[DD-]HH:]MM:SS
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,etime=,command="])
        .output()
    else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut result = Vec::new();
    for line in text.lines() {
        let Some((pid, ppid, etime, cmd_owned)) = ps_row(line) else {
            continue;
        };
        let cmd = cmd_owned.as_str();
        if !cmd.starts_with("ray::") {
            continue;
        }
        // Parse etime to seconds. Format: [[DD-]HH:]MM:SS
        let elapsed_s = parse_etime(etime).unwrap_or(0);
        if elapsed_s < grace_s {
            continue;
        }
        // ppid == 1 = reparented to init (orphan). Also catch ppid that is no
        // longer in the process table (the raylet exited).
        if ppid == 1 || !pid_exists(ppid) {
            result.push((pid, elapsed_s, cmd.chars().take(60).collect()));
        }
    }
    result
}

fn pid_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse ps etime field [[DD-]HH:]MM:SS into total seconds.
fn parse_etime(s: &str) -> Option<u64> {
    // Split on '-' first (days), then on ':'
    let (days, rest) = if let Some((d, r)) = s.split_once('-') {
        (d.parse::<u64>().ok()?, r)
    } else {
        (0, s)
    };
    let parts: Vec<&str> = rest.split(':').collect();
    match parts.as_slice() {
        [mm, ss] => {
            Some(days * 86400 + mm.parse::<u64>().ok()? * 60 + ss.parse::<u64>().ok()?)
        }
        [hh, mm, ss] => {
            Some(days * 86400 + hh.parse::<u64>().ok()? * 3600
                + mm.parse::<u64>().ok()? * 60
                + ss.parse::<u64>().ok()?)
        }
        _ => None,
    }
}

/// Parse one `ps -o pid=,ppid=,etime=,command=` row into (pid, ppid, etime, command).
///
/// SPLIT ON WHITESPACE RUNS, NOT SINGLE SPACES (AMUX-3972). `ps` RIGHT-ALIGNS
/// its numeric columns, so a real row begins with padding:
///
///   "  5923     1     05:28 /Applications/Google Chrome.app/..."
///
/// The previous code was `line.splitn(4, ' ').filter(|s| !s.is_empty())`, which
/// consumes the first three SINGLE spaces — all of them padding — and yields
/// ["", "5923", "", "   1     05:28 /Applications/..."]. Filtering the empties
/// happens AFTER splitn has already committed its split points, so the result
/// has 2 elements, `parts.len() < 4` fires, and the row is skipped. Every row,
/// every pass. Measured on this box: 45 matching Chrome rows, 0 parsed.
///
/// The filter READS like it handles padding and cannot. That is why this is a
/// function now: the identical block existed at two call sites (the ray-worker
/// sweep and the orphaned-Chrome reaper) and both were dead in the same way.
///
/// The command is re-joined on single spaces. Every caller does `contains` on a
/// substring with no whitespace runs in it, so that is lossless for this use.
fn ps_row(line: &str) -> Option<(u32, u32, &str, String)> {
    let mut it = line.split_whitespace();
    let pid = it.next()?.parse::<u32>().ok()?;
    let ppid = it.next()?.parse::<u32>().ok()?;
    let etime = it.next()?;
    let cmd = it.collect::<Vec<&str>>().join(" ");
    if cmd.is_empty() {
        return None;
    }
    Some((pid, ppid, etime, cmd))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HealthProcess {
    pid: u32,
    ppid: u32,
    state: String,
    elapsed_s: u64,
    command: String,
}

fn health_process_rows(text: &str) -> Vec<HealthProcess> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let state = fields.next()?.to_string();
            let elapsed_s = parse_etime(fields.next()?)?;
            let command = fields.collect::<Vec<_>>().join(" ");
            if command.is_empty() {
                return None;
            }
            Some(HealthProcess { pid, ppid, state, elapsed_s, command })
        })
        .collect()
}

fn health_process_snapshot() -> Option<Vec<HealthProcess>> {
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,state=,etime=,command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(health_process_rows(&String::from_utf8_lossy(&out.stdout)))
}

fn orphaned_debug_rustc(rows: &[HealthProcess], grace_s: u64) -> Vec<HealthProcess> {
    rows.iter()
        .filter(|p| {
            p.ppid == 1
                && p.elapsed_s >= grace_s
                && (p.command.starts_with("rustc ") || p.command.contains("/rustc "))
                && p.command.contains("--out-dir")
                && p.command.contains("target/debug")
        })
        .cloned()
        .collect()
}

fn owned_zombie_children(
    rows: &[HealthProcess],
    grace_s: u64,
    server_pid: u32,
) -> (Vec<u32>, Vec<HealthProcess>) {
    let mut owned = Vec::new();
    let mut visible = Vec::new();
    for child in rows.iter().filter(|p| p.state.starts_with('Z') && p.elapsed_s >= grace_s) {
        visible.push(child.clone());
        if child.ppid == server_pid {
            owned.push(child.pid);
        }
    }
    (owned, visible)
}

fn zombie_log_sample(zombies: &[HealthProcess]) -> String {
    zombies
        .iter()
        .take(ZOMBIE_LOG_SAMPLE)
        .map(|p| format!("pid={}/ppid={}/age={}s", p.pid, p.ppid, p.elapsed_s))
        .collect::<Vec<_>>()
        .join(", ")
}

fn reap_owned_zombie(pid: u32) -> Result<bool, String> {
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid is restricted to one PID observed as an aged zombie
    // whose parent is this process. WNOHANG guarantees the health tick cannot
    // block if another waiter wins the race.
    let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if rc == pid as libc::pid_t {
        Ok(true)
    } else if rc == 0 {
        Ok(false)
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

/// Minimum age (seconds) before an orphaned Playwright Chrome is eligible for
/// reaping. Short grace covers the window between Chrome launch and the parent
/// Playwright process registering the PID. Default 300s (5 minutes).
fn playwright_chrome_grace_s() -> u64 {
    std::env::var("AMUX_MAC_HEALTH_PLAYWRIGHT_GRACE_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(300)
}

/// Returns the PIDs of orphaned Playwright-launched Chrome processes.
///
/// Shape: `--user-data-dir=/var/folders/…/T/.tmp*/playwright-auth/profile`
/// with PPID == 1 (parent Playwright process exited, Chrome reparented to
/// init). Each Playwright session spawns ~8-9 Chrome helper processes; we
/// only kill the root (PPID=1) and let the helpers die naturally.
fn orphaned_playwright_chromes(grace_s: u64) -> (Vec<(u32, u64)>, usize) {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,etime=,command="])
        .output()
    else {
        // 0 parsed: `ps` itself failed, so the sweep did not run. The caller
        // WARNs on this rather than reading it as a clean machine.
        return (vec![], 0);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut result = Vec::new();
    // HOW MANY ROWS THE PROBE COULD ACTUALLY READ (AMUX-3972, ethos rule 4).
    //
    // The old parse skipped 100% of rows and the job logged "no orphaned
    // Playwright Chrome processes" — at 15:15:24 on 2026-08-31, with SIX live.
    // A zero that means "none exist" and a zero that means "I could not read a
    // single line" printed the same sentence, so the reaper looked healthy for
    // as long as it was dead. Publishing the population makes those two
    // different outputs.
    let mut parsed = 0usize;
    for line in text.lines() {
        let Some((pid, ppid, etime, cmd_owned)) = ps_row(line) else {
            continue;
        };
        parsed += 1;
        if ppid != 1 {
            continue; // only reap the root; helpers die with it
        }
        let cmd = cmd_owned.as_str();
        // Must be a Chrome process with a Playwright temp-profile user-data-dir.
        let is_chrome = cmd.contains("Google Chrome") || cmd.contains("Chromium");
        let has_playwright_tmpdir = cmd.contains("/T/.tmp") && cmd.contains("playwright-auth/profile");
        if !is_chrome || !has_playwright_tmpdir {
            continue;
        }
        let elapsed_s = parse_etime(etime).unwrap_or(0);
        if elapsed_s >= grace_s {
            result.push((pid, elapsed_s));
        }
    }
    (result, parsed)
}

/// Count running `claude` processes and warn if over threshold.
///
/// `None` means the count could not be taken — which is NOT zero, and the two
/// must not share a rendering (ethos rule 4).
///
/// THIS RETURNED 0 FOREVER. It ran `pgrep -c -x claude`, and macOS pgrep has
/// no `-c`: the command printed its usage to stderr, exited non-zero, and
/// `.unwrap_or(0)` turned that into a count of zero. So the ceiling below could
/// never be crossed and the warning could never fire. Measured 2026-09-10 with
/// 78 real claude processes against a max of 60: the tick logged
/// `claude_count=0 max_claude=60` while the host sat at 95% swap and macOS was
/// killing workers. A check that cannot fail is not a check (ethos rule 7).
fn check_claude_count(max: usize) -> Option<usize> {
    let out = std::process::Command::new("pgrep").args(["-x", "claude"]).output().ok()?;
    // pgrep exits 1 with no output when nothing matches, which IS a real zero.
    // Any other failure is an un-measured count and must stay None.
    let code = out.status.code().unwrap_or(-1);
    if code > 1 {
        tracing::warn!(
            job = JOB, code, stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "mac-health: could not count claude processes — the ceiling is UNENFORCED \
             until this is fixed, and a zero here would be a lie"
        );
        return None;
    }
    let count = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()).count();
    if count > max {
        tracing::warn!(
            job = JOB,
            count,
            max,
            "mac-health: claude process count exceeds ceiling. \
             This many processes compete for memory and CPU. \
             Check for ghost lanes with `amux ls` and stop idle ones."
        );
    }
    Some(count)
}

/// Where sysctl actually is, most-specific first. `/usr/sbin` is absent from
/// launchd's PATH; the bare name is kept last so a non-standard host still works.
const SYSCTL_PATHS: [&str; 2] = ["/usr/sbin/sysctl", "sysctl"];

/// Swap percentage in use, or None when it cannot be read.
fn swap_used_pct() -> Option<f64> {
    // ABSOLUTE PATH, BECAUSE LAUNCHD'S PATH IS NOT A SHELL'S.
    // This server runs under launchd with
    // PATH=~/.cargo/bin:~/.local/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin
    // — no /usr/sbin, which is where sysctl lives. Bare `sysctl` therefore
    // failed to spawn and the probe returned None on every tick: the first
    // deploy of this arm logged `swap_pct=-1 swap_measured=false` while the
    // host really was at 95%. It is the documented launchd-PATH trap in
    // CLAUDE.md, hit again. pgrep, ps and tmux all resolve on that PATH, which
    // is why only this one broke.
    let out = SYSCTL_PATHS
        .iter()
        .find_map(|bin| std::process::Command::new(bin).args(["-n", "vm.swapusage"]).output().ok())
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // "total = 32768.00M  used = 31284.00M  free = 1484.00M  (encrypted)"
    let grab = |key: &str| -> Option<f64> {
        let at = text.find(key)?;
        text[at + key.len()..]
            .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .next()?
            .parse::<f64>()
            .ok()
    };
    let (total, used) = (grab("total")?, grab("used")?);
    if total <= 0.0 { return None; }
    Some(used / total * 100.0)
}

/// tmux sessions created by an amux TEST harness, older than `grace_s`.
///
/// These are the amux-owned share of memory pressure and nothing else reaps
/// them: they carry no registered worker, so no lane owns them, and each holds
/// a claude process. Measured 2026-09-10 on a host at 96% swap: 40 such panes
/// alive, 25 of them leaked by amux's own lifecycle e2e spec across earlier
/// runs, together holding 19 claude processes.
///
/// Scoped to prefixes a harness mints, never to a worker name. A real lane is
/// somebody's work in progress and is not this job's to end.
fn stale_test_panes(grace_s: u64) -> Vec<String> {
    const HARNESS: [&str; 3] = ["e2e-", "board-reviewer-", "callback-b-"];
    let out = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name} #{session_created}"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() { return Vec::new(); }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (name, created) = line.rsplit_once(' ')?;
            let bare = name.strip_prefix("amux-")?;
            if !HARNESS.iter().any(|p| bare.starts_with(p)) { return None; }
            let age = now.saturating_sub(created.trim().parse::<u64>().ok()?);
            (age >= grace_s).then(|| name.to_string())
        })
        .collect()
}

fn test_pane_grace_s() -> u64 {
    std::env::var("AMUX_TEST_PANE_GRACE_S").ok().and_then(|v| v.parse().ok()).unwrap_or(1800)
}

/// The top memory consumers, aggregated by command, as one log line.
///
/// Printed only under pressure, because that is when somebody needs to know
/// WHERE the memory went — and a ranking is the thing a swap percentage cannot
/// tell you. Measured on this host it was the discriminator: claude 14.8 GB
/// over 55 processes, and fseventsd holding 8.8 GB in ONE process, which is a
/// system daemon no scheduler should be killing.
fn top_memory_consumers(n: usize) -> String {
    let Ok(out) = std::process::Command::new("ps").args(["-eo", "rss=,comm="]).output() else {
        return String::from("(unavailable)");
    };
    let mut by_cmd: std::collections::HashMap<String, (u64, usize)> = std::collections::HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        let Some((rss, comm)) = line.split_once(char::is_whitespace) else { continue };
        let Ok(rss) = rss.trim().parse::<u64>() else { continue };
        let name = comm.trim().rsplit('/').next().unwrap_or(comm.trim()).to_string();
        let e = by_cmd.entry(name).or_insert((0, 0));
        e.0 += rss;
        e.1 += 1;
    }
    let mut rows: Vec<_> = by_cmd.into_iter().collect();
    rows.sort_by_key(|(_, (rss, _))| std::cmp::Reverse(*rss));
    rows.iter()
        .take(n)
        .map(|(cmd, (rss, procs))| format!("{cmd}={:.1}GB/{procs}p", *rss as f64 / 1_048_576.0))
        .collect::<Vec<_>>()
        .join(" ")
}

fn mem_reap_swap_pct() -> f64 {
    std::env::var("AMUX_MEM_REAP_SWAP_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(85.0)
}

fn one_pass() {
    let grace = ray_orphan_grace_s();
    let pw_grace = playwright_chrome_grace_s();
    let rustc_grace = rustc_orphan_grace_s();
    let zombie_grace = zombie_grace_s();
    let max_claude = max_claude();

    // --- Ray orphan sweep ---
    if !raylet_running() {
        let orphans = orphaned_ray_workers(grace);
        if !orphans.is_empty() {
            tracing::warn!(
                job = JOB,
                count = orphans.len(),
                grace_s = grace,
                "mac-health: orphaned ray:: workers with no live raylet — sending SIGTERM"
            );
            for (pid, age_s, cmd) in &orphans {
                tracing::info!(
                    job = JOB, pid, age_s, cmd = %cmd,
                    "mac-health: SIGTERM orphaned ray:: worker"
                );
                let _ = std::process::Command::new("kill").args([&pid.to_string()]).output();
            }
        } else {
            tracing::debug!(job = JOB, "mac-health: no orphaned ray:: workers");
        }
    } else {
        tracing::debug!(job = JOB, "mac-health: raylet running, skipping ray orphan sweep");
    }

    // --- Orphaned rustc + true zombie sweep ---
    let mut rustc_reaped = 0usize;
    let mut zombies_seen = 0usize;
    let mut zombies_reaped = 0usize;
    match health_process_snapshot() {
        Some(rows) if !rows.is_empty() => {
            let rustc = orphaned_debug_rustc(&rows, rustc_grace);
            for process in &rustc {
                tracing::warn!(
                    job = JOB,
                    pid = process.pid,
                    age_s = process.elapsed_s,
                    "mac-health: SIGTERM orphaned debug rustc whose cargo parent is gone"
                );
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &process.pid.to_string()])
                    .output();
            }
            rustc_reaped = rustc.len();

            let (owned, zombies) =
                owned_zombie_children(&rows, zombie_grace, std::process::id());
            zombies_seen = zombies.len();
            if !zombies.is_empty() {
                let foreign = zombies.len().saturating_sub(owned.len());
                let sample = zombie_log_sample(&zombies);
                tracing::warn!(
                    job = JOB,
                    count = zombies.len(),
                    owned_by_this_server = owned.len(),
                    foreign_parent = foreign,
                    sample = %sample,
                    sample_limit = ZOMBIE_LOG_SAMPLE,
                    "mac-health: true zombies detected; foreign children are report-only"
                );
            }
            for pid in owned {
                match reap_owned_zombie(pid) {
                    Ok(true) => {
                        zombies_reaped += 1;
                        tracing::info!(job = JOB, pid, "mac-health: reaped owned zombie child");
                    }
                    Ok(false) => tracing::info!(
                        job = JOB,
                        pid,
                        "mac-health: zombie disappeared before reap (another waiter won)"
                    ),
                    Err(error) => tracing::warn!(
                        job = JOB,
                        pid,
                        %error,
                        "mac-health: waitpid failed; zombie was NOT reported as reaped"
                    ),
                }
            }
        }
        _ => tracing::warn!(
            job = JOB,
            "mac-health: process-state snapshot unreadable; rustc/zombie cleanup did NOT run"
        ),
    }

    // --- Orphaned Playwright Chrome sweep ---
    let (pw_orphans, pw_rows_parsed) = orphaned_playwright_chromes(pw_grace);
    if pw_rows_parsed == 0 {
        // `ps -A` on a live machine always has rows. Zero parsed means the
        // PROBE is broken, not that the machine is quiet — say so loudly rather
        // than reporting a clean sweep.
        tracing::warn!(
            job = JOB,
            "mac-health: parsed 0 rows from `ps -A` — the orphan sweep did NOT run. \
             This is a broken probe, not a clean machine (AMUX-3972)."
        );
    }
    if !pw_orphans.is_empty() {
        tracing::warn!(
            job = JOB,
            count = pw_orphans.len(),
            grace_s = pw_grace,
            "mac-health: orphaned Playwright temp-profile Chrome processes — sending SIGTERM"
        );
        for (pid, age_s) in &pw_orphans {
            tracing::info!(
                job = JOB, pid, age_s,
                "mac-health: SIGTERM orphaned Playwright Chrome root"
            );
            let _ = std::process::Command::new("kill").args([&pid.to_string()]).output();
        }
    } else {
        tracing::debug!(
            job = JOB,
            rows_parsed = pw_rows_parsed,
            "mac-health: no orphaned Playwright Chrome processes"
        );
    }

    // --- Memory pressure: reap what amux owns, and NAME the rest ---
    let swap_pct = swap_used_pct();
    let mut panes_reaped = 0usize;
    if swap_pct.is_some_and(|p| p >= mem_reap_swap_pct()) {
        for name in stale_test_panes(test_pane_grace_s()) {
            if std::process::Command::new("tmux")
                .args(["kill-session", "-t", &name])
                .status()
                .is_ok_and(|st| st.success())
            {
                panes_reaped += 1;
                tracing::info!(job = JOB, pane = %name, "mac-health: reaped a stale test pane under memory pressure");
            }
        }
        tracing::warn!(
            job = JOB,
            swap_pct = swap_pct.unwrap_or(-1.0) as i64,
            threshold_pct = mem_reap_swap_pct() as i64,
            panes_reaped,
            top_consumers = %top_memory_consumers(5),
            knob = "AMUX_MEM_REAP_SWAP_PCT",
            "mac-health: host is under memory pressure — reaped amux's own stale test panes. \
             Anything named above that is not amux's is a human's call, not this job's."
        );
    }

    // --- Claude process count ---
    let claude_count = check_claude_count(max_claude);
    tracing::info!(
        job = JOB,
        claude_count = claude_count.map(|c| c as i64).unwrap_or(-1),
        // The discriminator the old line lacked: a real 0 and a failed probe
        // rendered identically, so nobody could tell a quiet host from a blind
        // one. -1 is never a process count.
        claude_count_measured = claude_count.is_some(),
        max_claude,
        swap_pct = swap_pct.unwrap_or(-1.0) as i64,
        swap_measured = swap_pct.is_some(),
        panes_reaped,
        ray_alive = raylet_running(),
        playwright_chromes_reaped = pw_orphans.len(),
        rustc_reaped,
        zombies_seen,
        zombies_reaped,
        "mac-health tick"
    );
}

pub fn spawn() -> super::PeriodicTask {
    let interval = Duration::from_secs(tick_secs());
    super::spawn_periodic_every(JOB, interval, || async {
        tokio::task::spawn_blocking(one_pass).await.ok();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etime_parses_correctly() {
        assert_eq!(parse_etime("01:30"), Some(90));
        assert_eq!(parse_etime("01:01:00"), Some(3660));
        assert_eq!(parse_etime("1-02:00:00"), Some(93600));
        assert_eq!(parse_etime("00:05"), Some(5));
        // Malformed -> None, not a panic.
        assert_eq!(parse_etime("bad"), None);
    }

    #[test]
    fn the_pressure_arm_reaps_only_harness_panes_and_only_when_stale() {
        // The rule this job must not get wrong: a real lane is somebody's work
        // in progress. Only prefixes a HARNESS mints are eligible, and only
        // once they are old enough that no run still owns them.
        const HARNESS: [&str; 3] = ["e2e-", "board-reviewer-", "callback-b-"];
        let eligible = |bare: &str| HARNESS.iter().any(|p| bare.starts_with(p));

        // Real workers, including ones whose names merely CONTAIN a harness
        // word — a prefix test and a substring test differ exactly here.
        for lane in [
            "amux", "backend", "mixpeek-orchestrator", "amux-testing-e2e",
            "gtm-e2e-runner", "my-callback-b-worker",
        ] {
            assert!(!eligible(lane), "{lane} is a real lane and must never be reaped");
        }
        for harness in [
            "e2e-life-desktop-1788996104537",
            "board-reviewer-ios-safari-1788976843246",
            "callback-b-desktop-1788849272894",
        ] {
            assert!(eligible(harness), "{harness} is harness scaffolding and should be eligible");
        }

        // Age gates independently of the name: a pane from a RUNNING test is
        // not stale, and reaping it would kill the run that owns it.
        let grace = test_pane_grace_s();
        assert!(grace > 0, "a zero grace would reap panes belonging to a live run");
        assert!(mem_reap_swap_pct() > 0.0 && mem_reap_swap_pct() <= 100.0);
    }

    #[test]
    fn swap_and_consumers_report_absence_rather_than_a_false_zero() {
        // Both feed a decision to KILL things, so "could not measure" must not
        // arrive as a number. swap_used_pct returns Option and the tick logs
        // swap_measured beside it; the ranking says so in words.
        // On macOS this MUST measure. Returning None here is the launchd-PATH
        // bug: /usr/sbin is not on the server's PATH, so a bare `sysctl` never
        // spawns and the arm silently never fires. Asserting Some is what
        // makes that a red test instead of a quiet -1 in a log nobody reads.
        #[cfg(target_os = "macos")]
        {
            let p = swap_used_pct().expect("swap must be measurable on macOS — check SYSCTL_PATHS");
            assert!((0.0..=100.0).contains(&p), "swap pct out of range: {p}");
        }
        let top = top_memory_consumers(3);
        assert!(!top.is_empty(), "the consumer ranking must never render as an empty string");
    }

    #[test]
    fn ray_orphan_grace_filters_young_processes() {
        // parse_etime + grace check: a 60s-old worker with a 120s grace is kept.
        let age = parse_etime("01:00").unwrap_or(0);
        assert!(age < 120, "60s < 120s grace, should not be reaped");
        let age2 = parse_etime("03:00").unwrap_or(0);
        assert!(age2 >= 120, "180s >= 120s grace, eligible");
    }

    #[test]
    fn orphaned_debug_rustc_is_reaped_but_live_and_release_builds_are_not() {
        let rows = health_process_rows(
            " 101 1 S 11:00 /toolchain/bin/rustc crate.rs --out-dir /repo/target/debug/deps\n\
             102 99 S 11:00 /toolchain/bin/rustc crate.rs --out-dir /repo/target/debug/deps\n\
             103 1 S 11:00 /toolchain/bin/rustc crate.rs --out-dir /repo/target/release/deps\n\
             104 1 S 00:20 /toolchain/bin/rustc crate.rs --out-dir /repo/target/debug/deps",
        );
        let selected = orphaned_debug_rustc(&rows, 600);
        assert_eq!(selected.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![101]);
    }

    #[test]
    fn zombie_cleanup_only_selects_children_owned_by_this_server() {
        let rows = health_process_rows(
            " 200 1 S 20:00 /opt/amux/bin/amux-server\n\
             201 200 Z 05:00 [helper] <defunct>\n\
             300 1 S 20:00 /Applications/Other.app/other\n\
             301 300 Z 05:00 [child] <defunct>\n\
             401 1 Z 05:00 [orphan] <defunct>\n\
             402 200 Z 00:10 [young] <defunct>",
        );
        let (owned, zombies) = owned_zombie_children(&rows, 60, 200);
        assert_eq!(owned, vec![201]);
        assert_eq!(zombies.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![201, 301, 401]);
    }

    #[test]
    fn zombie_warning_sample_is_bounded_but_keeps_ownership_context() {
        let rows = (0..12)
            .map(|n| HealthProcess {
                pid: 100 + n,
                ppid: 200 + n,
                state: "Z".into(),
                elapsed_s: 300 + u64::from(n),
                command: "[child] <defunct>".into(),
            })
            .collect::<Vec<_>>();
        let sample = zombie_log_sample(&rows);
        assert_eq!(sample.split(", ").count(), ZOMBIE_LOG_SAMPLE);
        assert!(sample.contains("pid=100/ppid=200/age=300s"));
        assert!(!sample.contains("pid=108/"), "the log sample must stay bounded: {sample}");
    }

    #[test]
    fn malformed_and_empty_process_snapshots_fail_closed() {
        assert!(health_process_rows("").is_empty());
        assert!(health_process_rows("not a ps row\n1 nope Z 1:00 cmd").is_empty());
        let rows = health_process_rows(" 5 1 ? bad /toolchain/bin/rustc --out-dir /x/target/debug");
        assert!(orphaned_debug_rustc(&rows, 0).is_empty());
        assert_eq!(owned_zombie_children(&rows, 0, 1), (Vec::new(), Vec::new()));
    }
}

#[cfg(test)]
mod ps_row_tests {
    use super::*;

    /// A REAL row, captured verbatim from `ps -A -o pid=,ppid=,etime=,command=`
    /// on 2026-08-31 while eight orphaned Chromes were live. The leading spaces
    /// are the whole point: `ps` right-aligns pid and ppid, and it was that
    /// padding the old `splitn(4, ' ')` consumed.
    const REAL_ROW: &str =
        " 5923     1       10:41 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome \
--user-data-dir=/var/folders/0x/T/.tmpLbY4NA/playwright-auth/profile";

    /// GUARDS THE FIXTURE ITSELF. If someone "tidies" the leading whitespace out
    /// of REAL_ROW, every assertion below still passes and stops testing the bug
    /// — the padding IS the input under test.
    #[test]
    fn the_fixture_is_actually_padded_or_it_tests_nothing() {
        assert!(
            REAL_ROW.starts_with(' '),
            "REAL_ROW must keep ps's right-alignment padding; without it this \
             module cannot fail on the defect it exists for"
        );
        // And the old predicate really is defeated by it, so the cell below is
        // not merely asserting that a correct parser is correct.
        let old: Vec<&str> = REAL_ROW.splitn(4, ' ').filter(|s| !s.is_empty()).collect();
        assert!(
            old.len() < 4,
            "the pre-AMUX-3972 parse must FAIL on this row (got {} parts) — if it \
             succeeds, this fixture no longer reproduces the bug",
            old.len()
        );
    }

    #[test]
    fn a_padded_ps_row_parses_into_its_four_fields() {
        let (pid, ppid, etime, cmd) = ps_row(REAL_ROW).expect("a real ps row must parse");
        assert_eq!(pid, 5923);
        assert_eq!(ppid, 1);
        assert_eq!(etime, "10:41");
        assert!(cmd.starts_with("/Applications/Google Chrome.app/"), "cmd was: {cmd}");
        // The command must survive intact through the re-join, or the reaper's
        // `contains` checks silently stop matching.
        assert!(cmd.contains("playwright-auth/profile"), "cmd was: {cmd}");
        assert!(cmd.contains("/T/.tmp"), "cmd was: {cmd}");
    }

    #[test]
    fn the_reaper_selects_that_row_end_to_end() {
        // The predicate the reaper actually applies, against the real row.
        let (_, ppid, _, cmd) = ps_row(REAL_ROW).unwrap();
        assert_eq!(ppid, 1, "orphaned to init");
        assert!(cmd.contains("Google Chrome"));
        assert!(cmd.contains("/T/.tmp") && cmd.contains("playwright-auth/profile"));
    }

    #[test]
    fn rows_without_a_command_are_rejected_rather_than_half_parsed() {
        assert!(ps_row("  123   1   00:01").is_none(), "no command field");
        assert!(ps_row("").is_none());
        assert!(ps_row("not a ps row at all").is_none(), "pid must be numeric");
    }
}
