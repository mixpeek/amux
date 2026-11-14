//! Memory curation ("dreaming"): ask a worker whose amux memory has grown large
//! to rewrite it.
//!
//! Every launch loads a worker's whole memory (`compose_lane_context`), and the
//! only write verb was `amux memory add`, so memory only grew: on 2026-11-14,
//! 30 of 209 memory files were over 12 KB, the largest 52 KB, and pairs of
//! lanes carried byte-identical copies. Nothing ever removed a stale entry.
//!
//! The worker does the curation itself, in its own turn, because judging what
//! is stale is the model's job and gets better as models do. This job only
//! decides WHEN to ask:
//!
//!   - the memory file is over `AMUX_MEMORY_DREAM_KB` (default 12),
//!   - it changed since the last ask, and the last ask is at least
//!     `INTERVAL_S` old,
//!   - the worker is running, and its scope does not set `AMUX_MEMORY_DREAM=0`.
//!
//! The ask goes through `steer_enqueue`, which refuses isolated and paused
//! lanes, so an isolated worker is never prompted. Each ask is recorded as a
//! `memory.dream` session event. Each pass logs `memory_dream_pass` with
//! how many it considered and asked, zero included.

use crate::api::AppState;
use serde_json::json;
use std::path::Path;

pub const GUARD: &str = "memory-dream";
const ENABLE_KEY: &str = "AMUX_MEMORY_DREAM";
const KB_KEY: &str = "AMUX_MEMORY_DREAM_KB";
const DEFAULT_KB: u64 = 12;
/// At most one ask per worker per week.
pub const INTERVAL_S: f64 = 7.0 * 86400.0;
const TICK_S: u64 = 6 * 3600;
const BOOT_DELAY_S: u64 = 300;

/// Why a worker is or is not asked. `Ask` is the only one that enqueues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    Ask,
    Small,
    Unchanged,
    TooSoon,
}

/// The selection rule, pure so the tests pin it. `last` is the previous ask's
/// time and the memory mtime it saw.
pub fn due(bytes: u64, mtime: f64, last: Option<(f64, f64)>, now: f64, min_kb: u64) -> Due {
    if bytes <= min_kb * 1024 {
        return Due::Small;
    }
    match last {
        Some((_, seen_mtime)) if mtime <= seen_mtime => Due::Unchanged,
        Some((asked, _)) if now - asked < INTERVAL_S => Due::TooSoon,
        _ => Due::Ask,
    }
}

fn is_off(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
}

/// Process env wins, then worker > group > global. Default ON.
pub(crate) fn enabled_in(home: &Path, lane: &str, process_value: Option<&str>) -> bool {
    if let Some(v) = process_value.filter(|v| !v.trim().is_empty()) {
        return !is_off(v);
    }
    crate::api::session_verbs::scoped_setting_in(home, lane, ENABLE_KEY)
        .as_deref()
        .map(|v| !is_off(v))
        .unwrap_or(true)
}

fn min_kb() -> u64 {
    std::env::var(KB_KEY).ok().and_then(|v| v.trim().parse().ok()).filter(|&k| k > 0).unwrap_or(DEFAULT_KB)
}

/// The ask. It names the size, what to keep, and the one write verb.
pub fn ask_text(bytes: u64, min_kb: u64) -> String {
    format!(
        "Your amux memory is {kb} KB, and every launch loads all of it. Curate it when you finish your current step:\n\n\
         1. Read it: `amux memory`.\n\
         2. Keep only what you cannot look up again: the owner's preferences and corrections, decisions and why, and pointers to where things live. \
         Drop what the repo, git history, CLAUDE.md or the board already records, anything superseded, and duplicates.\n\
         3. Merge entries that say the same thing, and replace stale facts with current ones.\n\
         4. Save the whole result once: `amux memory curate --stdin <<'EOF'` ... `EOF`. The previous version is kept, so nothing is lost.\n\n\
         Aim for under {min_kb} KB, then go back to your work. \
         (memory curation; opt out with AMUX_MEMORY_DREAM=0 in your scope)",
        kb = bytes.div_ceil(1024),
    )
}

/// The last ask for each lane: (time, memory mtime it saw).
fn last_asks(conn: &rusqlite::Connection) -> std::collections::HashMap<String, (f64, f64)> {
    let mut out = std::collections::HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT session, ts, COALESCE(json_extract(data,'$.mtime'),0) FROM session_events \
         WHERE type='memory.dream' ORDER BY ts",
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))) {
        for (s, ts, m) in rows.flatten() {
            out.insert(s, (ts, m));
        }
    }
    out
}

/// One pass. Returns the workers asked.
pub async fn run_once(state: &AppState, home: &Path) -> Vec<String> {
    let mem_dir = home.join("memory");
    let min_kb = min_kb();
    let now = crate::config::now_f64();
    let last = match state.store.read() {
        Ok(c) => last_asks(&c),
        Err(_) => {
            tracing::warn!(measured = false, verdict = "memory_dream_unmeasured", "memory-dream: database unreadable, asked nobody");
            return vec![];
        }
    };
    let process_value = std::env::var(ENABLE_KEY).ok();
    let mut considered = 0usize;
    let mut asked = Vec::new();
    let Ok(entries) = std::fs::read_dir(&mem_dir) else {
        tracing::warn!(measured = false, verdict = "memory_dream_unmeasured", dir = %mem_dir.display(), "memory-dream: memory dir unreadable");
        return asked;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Some(lane) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
        if path.extension().and_then(|x| x.to_str()) != Some("md")
            || !crate::api::session_verbs::env_path(&lane).exists()
        {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        considered += 1;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if due(meta.len(), mtime, last.get(&lane).copied(), now, min_kb) != Due::Ask
            || !enabled_in(home, &lane, process_value.as_deref())
            || !crate::api::session_verbs::is_running(&lane).await
        {
            continue;
        }
        match crate::api::session_verbs::steer_enqueue(state, &lane, &ask_text(meta.len(), min_kb), GUARD, "").await {
            Ok(queue_id) => {
                let (l, data) = (lane.clone(), json!({"bytes": meta.len(), "mtime": mtime, "queue_id": queue_id}).to_string());
                let recorded = state
                    .store
                    .write_async(move |conn| {
                        conn.execute(
                            "INSERT INTO session_events (ts, session, type, data, source) VALUES (?1, ?2, 'memory.dream', ?3, 'memory-dream')",
                            rusqlite::params![crate::config::now_f64(), l, data],
                        )?;
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    })
                    .await;
                if let Err(e) = recorded {
                    // Unrecorded, the next pass would ask again within the week.
                    tracing::warn!(lane = %lane, error = %e, verdict = "memory_dream_unrecorded",
                        "memory-dream: the ask was queued but not recorded");
                }
                tracing::info!(lane = %lane, bytes = meta.len(), verdict = "memory_dream_asked", "memory-dream: asked the worker to curate its memory");
                asked.push(lane);
            }
            Err(reason) => {
                tracing::info!(lane = %lane, reason, verdict = "memory_dream_refused", "memory-dream: the steering queue refused the ask");
            }
        }
    }
    tracing::info!(measured = true, n_considered = considered, asked = asked.len(), min_kb,
        verdict = "memory_dream_pass", "memory-dream: pass finished");
    asked
}

/// The registered loop (`registry::ids::MEMORY_DREAM`). Each pass is bracketed
/// by tick_start/tick_end, so a stalled or panicking pass shows on
/// /api/system-jobs; spawn_loop restarts the loop if it dies.
pub async fn dream_loop(state: AppState) {
    use crate::runtime_jobs::registry::{self, ids};
    tokio::time::sleep(std::time::Duration::from_secs(BOOT_DELAY_S)).await;
    loop {
        registry::tick_start(ids::MEMORY_DREAM);
        run_once(&state, &crate::config::amux_home()).await;
        registry::tick_end(ids::MEMORY_DREAM);
        tokio::time::sleep(std::time::Duration::from_secs(TICK_S)).await;
    }
}

pub const TICK_SECS: u64 = TICK_S;

#[cfg(test)]
mod tests {
    use super::*;

    const KB: u64 = 1024;

    #[test]
    fn only_a_large_changed_memory_not_asked_this_week_is_asked() {
        let now = 1_000_000.0;
        assert_eq!(due(12 * KB, 5.0, None, now, 12), Due::Small, "at the threshold is small");
        assert_eq!(due(13 * KB, 5.0, None, now, 12), Due::Ask, "large and never asked");
        assert_eq!(due(13 * KB, 5.0, Some((now - 10.0, 5.0)), now, 12), Due::Unchanged, "no edit since the last ask");
        assert_eq!(due(13 * KB, 9.0, Some((now - 10.0, 5.0)), now, 12), Due::TooSoon, "edited, but asked this week");
        assert_eq!(due(13 * KB, 9.0, Some((now - INTERVAL_S, 5.0)), now, 12), Due::Ask, "edited and a week has passed");
    }

    #[test]
    fn a_scope_can_turn_it_off_and_the_process_env_wins() {
        let dir = tempfile::tempdir().unwrap();
        assert!(enabled_in(dir.path(), "w", None), "on by default");
        assert!(!enabled_in(dir.path(), "w", Some("0")));
        assert!(enabled_in(dir.path(), "w", Some("1")));
    }

    #[test]
    fn the_ask_names_the_size_and_the_curate_verb() {
        let t = ask_text(52 * KB, 12);
        assert!(t.contains("52 KB") && t.contains("amux memory curate --stdin") && t.contains("AMUX_MEMORY_DREAM=0"));
        assert!(!t.contains('\u{2014}'), "no em-dashes in worker-facing text");
    }
}
