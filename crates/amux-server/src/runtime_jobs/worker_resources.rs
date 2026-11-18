//! Per-worker resource sampling: CPU, RSS, process count per worker, stored
//! every 30s so "which worker was burning cores at 03:00" has an answer.
//!
//! Reuses the host_guard's tmux-pane-to-worker mapping, then aggregates per
//! worker and writes to `worker_resource_samples`. The same tick also checks
//! per-worker resource caps: a worker whose total RSS exceeds
//! `AMUX_WORKER_MAX_RSS_MB` gets a WARN log so the metrics endpoint can
//! surface it.
//!
//! Retention is 7 days by default (`AMUX_WORKER_METRICS_RETAIN_DAYS`).

use crate::api::AppState;
use crate::db::WriteOutcome;
use std::collections::HashMap;

use super::registry::ids;

const JOB: &str = ids::WORKER_RESOURCES;

fn env_u64(k: &str, default: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn tick_secs() -> u64 {
    env_u64("AMUX_WORKER_RESOURCES_EVERY_SECS", 30).max(10)
}

fn retain_days() -> u64 {
    env_u64("AMUX_WORKER_METRICS_RETAIN_DAYS", 7).max(1)
}

fn global_max_rss_mb() -> u64 {
    env_u64("AMUX_WORKER_MAX_RSS_MB", 0)
}

fn max_concurrent_active() -> u64 {
    env_u64("AMUX_MAX_CONCURRENT_ACTIVE", 0)
}

/// One worker's aggregated resource snapshot.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct WorkerSnapshot {
    pub session: String,
    pub rss_kb: u64,
    pub cpu_pct: f64,
    pub n_procs: u32,
    pub n_threads: u32,
}

/// Walk the parent chain to find which worker owns a process.
/// Each pane's direct child with that child's children, sorted, per worker.
/// amux launches the provider through `sh -c "cd … && … claude …"`, so the
/// direct child is that `sh` and Claude is one level down; a provider started
/// directly is the direct child, and its own children inherit its environment.
fn provider_trees(parents: &HashMap<u32, u32>, panes: &HashMap<u32, String>) -> Vec<(String, Vec<u32>)> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in parents {
        children.entry(ppid).or_default().push(pid);
    }
    let mut trees = Vec::new();
    for (&shell, worker) in panes {
        let mut direct = children.get(&shell).cloned().unwrap_or_default();
        direct.sort_unstable();
        for child in direct {
            let mut tree = vec![child];
            let mut below = children.get(&child).cloned().unwrap_or_default();
            below.sort_unstable();
            tree.extend(below);
            trees.push((worker.clone(), tree));
        }
    }
    trees
}

fn find_owner<'a>(
    pid: u32,
    panes: &'a HashMap<u32, String>,
    parents: &HashMap<u32, u32>,
) -> Option<&'a String> {
    let mut p = pid;
    for _ in 0..64 {
        if let Some(w) = panes.get(&p) {
            return Some(w);
        }
        p = *parents.get(&p)?;
        if p <= 1 {
            return None;
        }
    }
    None
}

/// Sample every worker's resource usage from ps + tmux pane mapping.
pub fn sample_workers() -> Vec<WorkerSnapshot> {
    let ps_out = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,time=,etime=,nice=,comm="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());

    let pane_out = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{session_name} #{pane_pid}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());

    let (Some(ps_text), Some(pane_text)) = (ps_out, pane_out) else {
        return Vec::new();
    };

    let panes = super::host_guard::parse_panes(&pane_text);
    if panes.is_empty() {
        return Vec::new();
    }

    // First pass: build parent map and collect RSS per pid
    let mut parents: HashMap<u32, u32> = HashMap::new();
    let mut pid_rss: HashMap<u32, u64> = HashMap::new();

    for line in ps_text.lines() {
        let mut it = line.split_whitespace();
        let pid: u32 = match it.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let ppid: u32 = match it.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let rss_kb: u64 = match it.next().and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        parents.insert(pid, ppid);
        pid_rss.insert(pid, rss_kb);
    }

    // Record which Claude config folder each worker's provider runs under (read
    // once per process tree), so transcript readers follow a wrapper's
    // CLAUDE_CONFIG_DIR.
    for (worker, tree) in provider_trees(&parents, &panes) {
        crate::api::claude_config::observe_tree(&worker, &tree);
    }

    // Map pids to workers and aggregate
    let mut by_worker: HashMap<String, WorkerSnapshot> = HashMap::new();
    for (&pid, &rss) in &pid_rss {
        let w = if let Some(w) = panes.get(&pid) {
            w.clone()
        } else if let Some(w) = find_owner(
            parents.get(&pid).copied().unwrap_or(0),
            &panes,
            &parents,
        ) {
            w.clone()
        } else {
            continue;
        };

        let snap = by_worker.entry(w.clone()).or_insert_with(|| WorkerSnapshot {
            session: w,
            ..Default::default()
        });
        snap.rss_kb += rss;
        snap.n_procs += 1;
    }

    // Second ps call for instantaneous CPU%
    if let Some(cpu_out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,pcpu="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    {
        for line in cpu_out.lines() {
            let mut it = line.split_whitespace();
            let pid: u32 = match it.next().and_then(|s| s.parse().ok()) {
                Some(v) => v,
                None => continue,
            };
            let cpu: f64 = match it.next().and_then(|s| s.parse().ok()) {
                Some(v) => v,
                None => continue,
            };
            if cpu < 0.01 {
                continue;
            }

            let w = if let Some(w) = panes.get(&pid) {
                w.clone()
            } else if let Some(w) = find_owner(
                parents.get(&pid).copied().unwrap_or(0),
                &panes,
                &parents,
            ) {
                w.clone()
            } else {
                continue;
            };

            if let Some(snap) = by_worker.get_mut(&w) {
                snap.cpu_pct += cpu;
            }
        }
    }

    by_worker.into_values().collect()
}

/// Store one tick's samples and prune past retention. `write_async` already
/// runs this inside the writer's BEGIN IMMEDIATE, so it must not open its own
/// transaction: the first version did, and every batch failed with "cannot
/// start a transaction within a transaction", leaving the table empty.
fn insert_samples(
    conn: &rusqlite::Connection,
    snapshots: &[WorkerSnapshot],
    retain: u64,
) -> rusqlite::Result<WriteOutcome> {
    for s in snapshots {
        conn.execute(
            "INSERT INTO worker_resource_samples \
             (session, rss_kb, cpu_pct, n_procs, n_threads) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                s.session,
                s.rss_kb as i64,
                s.cpu_pct,
                s.n_procs,
                s.n_threads
            ],
        )?;
    }
    conn.execute(
        "DELETE FROM worker_resource_samples WHERE ts < datetime('now', ?1)",
        [format!("-{retain} days")],
    )?;
    Ok(WriteOutcome {
        applied: true,
        events: vec![],
    })
}

async fn tick(state: AppState) {
    let snapshots = tokio::task::spawn_blocking(sample_workers)
        .await
        .unwrap_or_default();

    if snapshots.is_empty() {
        return;
    }

    let max_rss = global_max_rss_mb();
    for s in &snapshots {
        let rss_mb = s.rss_kb / 1024;
        if max_rss > 0 && rss_mb > max_rss {
            tracing::warn!(
                target: "amux::worker_resources",
                verdict = "worker_rss_over_limit",
                worker = %s.session,
                rss_mb = rss_mb,
                limit_mb = max_rss,
                measured = true,
                n_considered = snapshots.len(),
                "worker {} RSS {} MB exceeds cap {} MB",
                s.session, rss_mb, max_rss
            );
        }
    }

    let n = snapshots.len();
    let retain = retain_days();
    let outcome = state
        .store
        .write_async(move |conn| insert_samples(conn, &snapshots, retain))
        .await;

    if let Err(e) = outcome {
        tracing::warn!(job = JOB, %e, "worker-resources sample batch was not stored");
    } else {
        tracing::debug!(job = JOB, workers = n, "sampled worker resources");
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, tick_secs(), move || {
        let st = state.clone();
        async move { tick(st).await }
    })
}

/// Live snapshot for the API.
pub fn latest() -> Vec<WorkerSnapshot> {
    sample_workers()
}

/// The concurrent-worker cap, for board_drive and the API.
pub fn max_concurrent() -> u64 {
    max_concurrent_active()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_owner_walks_parent_chain() {
        let mut panes = HashMap::new();
        panes.insert(200u32, "test-worker".to_string());
        let mut parents = HashMap::new();
        parents.insert(300u32, 201u32);
        parents.insert(201u32, 200u32);
        parents.insert(200u32, 100u32);
        assert_eq!(
            find_owner(300, &panes, &parents).map(String::as_str),
            Some("test-worker")
        );
        assert_eq!(find_owner(400, &panes, &parents), None);
    }

    #[test]
    fn a_provider_under_the_launch_shell_is_in_its_workers_tree() {
        // tv-player on a real host, 2026-11-17: pane zsh 32837 runs
        // `/bin/sh -c "cd … && … claude …"` (35143), and Claude is 35144.
        let mut panes = HashMap::new();
        panes.insert(32837u32, "tv-player".to_string());
        let mut parents = HashMap::new();
        parents.insert(35143u32, 32837u32);
        parents.insert(35144u32, 35143u32);
        parents.insert(35200u32, 35144u32); // an MCP server under Claude: too deep
        parents.insert(99u32, 1u32); // not in any pane
        assert_eq!(provider_trees(&parents, &panes), vec![("tv-player".to_string(), vec![35143, 35144])]);
    }

    #[test]
    fn max_concurrent_defaults_to_zero() {
        // A temp home clears every key the machine's server.env could have
        // leaked into this process (AF-1007: this Mac sets it to 60).
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        assert_eq!(max_concurrent_active(), 0);
    }

    #[tokio::test]
    async fn a_tick_batch_is_stored_through_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("wr.db")).unwrap();
        let snaps = vec![WorkerSnapshot {
            session: "w1".into(),
            rss_kb: 2048,
            cpu_pct: 1.5,
            n_procs: 3,
            n_threads: 9,
        }];
        store
            .write_async(move |conn| insert_samples(conn, &snaps, 7))
            .await
            .expect("the batch must commit inside the writer's transaction");
        let n: i64 = store
            .read()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM worker_resource_samples WHERE session='w1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }
}
