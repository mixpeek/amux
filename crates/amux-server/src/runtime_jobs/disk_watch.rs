//! Disk watch — report regenerable caches that are growing, and delete nothing.
//!
//! # Why this reports instead of capping
//!
//! DESKT-12 purged 43.4GB of ML caches and DESKT-13 thinned 24 APFS snapshots.
//! Both outcomes were gone within days: `~/.cache` refilled and the snapshots
//! regenerated, because a one-shot purge of a LIVE cache has no durable
//! outcome by construction. The obvious next move was a scheduled cap, and
//! measuring first is what stopped it being built.
//!
//! The measurement, from a completed scan of 310,218 directories: 337 GiB sits
//! in nominally-regenerable paths, but only **17.3 GiB of it is untouched for
//! more than 30 days**. Everything large is hot — the shared cargo target dir
//! and a peer repo's `target/` were both written the same day. So an age-based
//! cap reclaims a rounding error, and a size-based cap aggressive enough to
//! matter deletes build artifacts that ~50 lanes are actively using, turning
//! free disk into fleet-wide cold rebuilds. Neither is a good trade, and the
//! owner picked the third option: report, and leave the judgment with the human
//! (ethos rule 8 — report and recommend, do not sweep).
//!
//! That also keeps this honest as models improve. A threshold that auto-deletes
//! is a policy frozen into code; a report is an observation a better reader can
//! act on differently.
//!
//! # The interval is read from the database, not held in memory
//!
//! `PeriodicTask` is in-memory and dies with the process, and this server
//! re-execs whenever ANY lane commits to the shared checkout — often several
//! times an hour. A weekly in-memory timer would therefore essentially never
//! fire, and would fail *silently*, which is the exact shape ethos.md records
//! under D1: the scan-demotion optimisation lived in memory, so a restart
//! removed it most of the time and nothing said so.
//!
//! So the tick is hourly and cheap, and "has a week passed?" is answered by
//! `reclaim_scans` — durable state that survives every restart. The job is
//! idempotent per scan: one card per scan id, ever, enforced by `source_ref`.

use crate::api::AppState;
use crate::db::board_store as bs;
use serde_json::json;

/// Registry id. `spawn_periodic` derives this job's kill switch from the name.
const JOB: &str = "disk-watch";

/// How often the job wakes. Not how often it scans — see the module docs.
const TICK_SECS: u64 = 3600;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Minimum gap between watcher-initiated scans.
fn scan_every_secs() -> u64 {
    env_u64("AMUX_DISK_WATCH_EVERY_SECS", 7 * 86_400)
}

/// A regenerable path at or above this many GiB is worth telling someone about.
///
/// 25 GiB is not a tuned parameter: it is the floor at which a single path is
/// a meaningful fraction of this volume. Anything smaller is noise on a 1.8TB
/// disk and would file cards nobody acts on, which is how a report becomes
/// something people filter out.
fn floor_bytes() -> u64 {
    env_u64("AMUX_DISK_WATCH_FLOOR_GB", 25) * 1024 * 1024 * 1024
}

/// Free space below which the volume is worth LOOKING AT now rather than at the
/// next weekly wake (AEAB-59 / LR-70).
///
/// The module doc argues against a threshold and is right about the thing it
/// argues about: "a threshold that auto-deletes" is a bad trade. This job
/// deletes nothing — the commit that added it says so in its title — so a
/// threshold that REPORTS is a different object and that reasoning does not
/// reach it. Recorded here because otherwise this looks like it contradicts a
/// decision already taken.
///
/// Measured 2026-08-25: the volume was at 0.94 GB free and the next watcher scan
/// was due 2026-08-29, four days out, because the trigger was a pure 7-day
/// interval with no free-space condition anywhere in it. Nothing in amux was
/// going to look.
fn low_free_bytes() -> u64 {
    env_u64("AMUX_DISK_WATCH_LOW_FREE_GB", 5) * 1024 * 1024 * 1024
}

/// Minimum gap between LOW-SPACE scans. Without it a volume parked under the
/// floor starts a walk every hour forever — the scan-storm the whole feature is
/// built to avoid, and self-inflicted load on the resource being investigated
/// (the amux-cloud spin-catcher, ethos rule 7: what does the detector COST, and
/// is the cost paid in the same resource as the fault).
fn low_free_every_secs() -> u64 {
    env_u64("AMUX_DISK_WATCH_LOW_FREE_EVERY_SECS", 6 * 3600)
}

/// Should a scan start now? Split out from `tick` so BOTH triggers are testable
/// without a store, a walker or a clock — the interval one never was.
///
/// `free` is None when statfs failed: fall back to the interval alone rather
/// than treating an unreadable volume as full, which would start a scan every
/// grace window on a machine that cannot even measure itself.
pub(crate) fn should_scan(
    now: i64,
    newest_finished: Option<i64>,
    free: Option<u64>,
    low_free: u64,
    every: i64,
    low_every: i64,
) -> bool {
    let age = newest_finished.map(|fin| now - fin);
    // No completed scan ever: the interval rule already says yes, and it must
    // keep saying yes independently of the disk reading.
    let Some(age) = age else { return true };
    if age > every {
        return true;
    }
    matches!(free, Some(f) if f < low_free) && age > low_every
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Growth {
    pub path: String,
    pub bytes: u64,
    /// Same path in the previous completed scan, if it appeared there.
    pub was: Option<u64>,
}

impl Growth {
    fn delta_gib(&self) -> Option<f64> {
        self.was
            .map(|w| (self.bytes as f64 - w as f64) / 1_073_741_824.0)
    }
}

fn gib(b: u64) -> f64 {
    b as f64 / 1_073_741_824.0
}

/// The two most recent COMPLETED scans, newest first.
///
/// This USED to require `status='done'`, on the stated grounds that "comparing a
/// partial scan against a complete one would report every unvisited path as
/// having shrunk to nothing". That rationale reads as sound and is FALSE for this
/// code — and it was costing almost every scan (DESKT-28).
///
/// Measured 2026-08-25: `reclaim_scans` all-time was
/// `{done: 1, interrupted: 2, cancelled: 2, stalled: 1}`, because a scan walks
/// every root in 6-35 minutes while this server re-execs to adopt new builds
/// every ~23 minutes. So the filter admitted ONE scan in six, and threw away
/// partial scans holding 124 and 119 real findings (482 GB and 1039 GB).
///
/// The feared shrink cannot happen, and two independent layers stop it:
///   - [`growth_for`] iterates the CURRENT scan's findings and looks each path up
///     in the previous one, so a path the previous scan never reached yields
///     `was = None`. A path the CURRENT scan never reached is simply not selected.
///   - [`render`] states `None` as "no previous reading to compare" and says in
///     its own comment that a delta from an unmeasured path is a fabricated trend.
///
/// Both are pinned by `a_partial_previous_scan_never_manufactures_a_shrink` and
/// `a_partial_current_scan_reports_a_subset_never_a_shrink`, which construct
/// exactly the case the old comment feared and assert it does not occur.
///
/// What IS still required, and is the real invariant:
///   - `finished_at IS NOT NULL` — a RUNNING scan's totals are still moving, so
///     comparing against one would be reading a number mid-write.
///   - at least one finding — a scan that recorded nothing makes every path look
///     new, which is the fabricated-trend failure wearing the other face.
fn last_two_completed(conn: &rusqlite::Connection) -> Vec<(String, i64)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT s.id, s.finished_at FROM reclaim_scans s
          WHERE s.finished_at IS NOT NULL
            AND s.status <> 'running'
            AND EXISTS (SELECT 1 FROM reclaim_findings f WHERE f.scan_id = s.id)
          ORDER BY s.finished_at DESC LIMIT 2",
    ) else {
        return vec![];
    };
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// Regenerable paths in `scan_id` at or above the floor, with their size in
/// `prev_id` when they appeared there.
pub(crate) fn growth_for(
    conn: &rusqlite::Connection,
    scan_id: &str,
    prev_id: Option<&str>,
    floor: u64,
) -> Vec<Growth> {
    // `regenerable` rather than a category list: the walk sets that flag at the
    // point it decides what a finding IS, so reading it here cannot drift from
    // the producer the way a re-derived category filter would (ethos rule 1 —
    // a view must share the predicate of the mechanism it describes).
    let Ok(mut stmt) = conn.prepare(
        "SELECT path, bytes FROM reclaim_findings
          WHERE scan_id=?1 AND regenerable=1 AND bytes >= ?2
          ORDER BY bytes DESC",
    ) else {
        return vec![];
    };
    let rows: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params![scan_id, floor as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();

    rows.into_iter()
        .map(|(path, bytes)| {
            let was = prev_id.and_then(|p| {
                conn.query_row(
                    "SELECT bytes FROM reclaim_findings WHERE scan_id=?1 AND path=?2",
                    rusqlite::params![p, &path],
                    |r| r.get::<_, i64>(0),
                )
                .ok()
                .map(|b| b as u64)
            });
            Growth { path, bytes: bytes.max(0) as u64, was }
        })
        .collect()
}

/// The card body. Separate from the filing so it can be tested without a store.
pub(crate) fn render(scan_id: &str, rows: &[Growth], free_gib: f64, total_gib: f64) -> String {
    let mut s = String::new();
    s.push_str(
        "Regenerable caches at or above the reporting floor. **Nothing has been moved or \
         deleted** — this is a report, and every path below is yours to decide about.\n\n",
    );
    s.push_str(&format!(
        "Volume: {free_gib:.0} GiB free of {total_gib:.0} GiB ({:.0}% used).\n\n",
        (1.0 - free_gib / total_gib.max(1.0)) * 100.0
    ));
    for g in rows {
        match g.delta_gib() {
            Some(d) if d.abs() >= 1.0 => s.push_str(&format!(
                "- `{}` is **{:.1} GiB** ({}{:.1} GiB since the last scan)\n",
                g.path,
                gib(g.bytes),
                if d > 0.0 { "+" } else { "" },
                d
            )),
            Some(_) => s.push_str(&format!(
                "- `{}` is **{:.1} GiB** (flat since the last scan)\n",
                g.path,
                gib(g.bytes)
            )),
            // Absence of a previous reading is stated, never rendered as a
            // change from zero. A "+82 GiB this week" on a path that simply was
            // not measured before is a fabricated trend.
            None => s.push_str(&format!(
                "- `{}` is **{:.1} GiB** (no previous reading to compare)\n",
                g.path,
                gib(g.bytes)
            )),
        }
    }
    s.push_str(&format!(
        "\nFrom reclaim scan `{scan_id}`. Reclaim any of these from the dashboard's Disk \
         Cleanup tab, which quarantines first so a mistake is restorable.\n\n\
         Note before acting on the big ones: shared cargo target dirs are a warm cache the \
         whole fleet builds against, so clearing one buys disk and costs every lane a cold \
         rebuild.\n"
    ));
    s
}

/// Has a card already been filed for this scan?
fn already_filed(conn: &rusqlite::Connection, scan_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM issues WHERE source_ref=?1 AND deleted IS NULL LIMIT 1",
        [format!("diskwatch:{scan_id}")],
        |_| Ok(()),
    )
    .is_ok()
}

/// Hourly size sample of the regenerable paths amux itself mandates, so growth
/// is visible as a SERIES rather than as two spot readings after the fact.
///
/// # Why this exists next to the full scan rather than instead of it
///
/// The full reclaim scan is the better instrument and it is not being replaced.
/// But measured 2026-08-25, it has completed ONCE in its lifetime — `reclaim_scans`
/// reads `{done: 1, interrupted: 2, cancelled: 2, stalled: 1}`. It walks every root
/// in 6-35 minutes while this server re-execs to adopt new builds every 22.8
/// minutes on average (66 starts in 25h), so most scans lose that race by
/// construction and `api/reclaim.rs` marks them `interrupted` at the next startup.
/// Last completed scan was 2026-08-21, four days before the volume hit 100%.
///
/// The consequence was concrete: `~/.amux/rust-build-target` went from 40 GB to
/// 97 GB in 19 hours and nothing said so. That growth is exactly what the scan
/// exists to surface (DESKT-26/27).
///
/// This is the cheap half that cannot lose the race. One known directory instead
/// of every root: measured at **9 seconds** for 323,855 files, against the full
/// scan's 6-35 minutes. It fits inside the restart window with three orders of
/// magnitude to spare, so it produces a reading every hour whatever the fleet is
/// doing.
///
/// # Hardlink accounting, because it produced a real contradiction
///
/// Sizes are summed ONCE PER INODE, which is what `du` does. A naive per-file sum
/// of this same directory reports 252.7 GB where `du` reports 97 GB, because cargo
/// hardlinks artifacts heavily. Both numbers are "right" and only one matches the
/// free space a reader is comparing against, so the wrong one reads as a 2.6x
/// discrepancy and invites the conclusion that the measurement is broken.
fn report_regenerable_growth() {
    use std::collections::HashMap;
    use std::sync::Mutex;
    static LAST: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

    let mut now_sizes: HashMap<String, u64> = HashMap::new();
    for path in watched_regenerable_paths() {
        let Some((bytes, files)) = du_bytes(&path) else { continue };
        let key = path.to_string_lossy().to_string();
        let gb = bytes as f64 / 1_073_741_824.0;

        let prev = LAST
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|m| m.get(&key).copied()));
        match prev {
            // First reading of a path is a BASELINE, not growth — the same rule
            // the scan-diff path already follows (see the test named for it).
            None => tracing::info!(
                job = JOB,
                path = %key,
                gb = format!("{gb:.1}"),
                files,
                "regenerable path baseline"
            ),
            Some(p) => {
                let delta = gb - (p as f64 / 1_073_741_824.0);
                tracing::info!(
                    job = JOB,
                    path = %key,
                    gb = format!("{gb:.1}"),
                    delta_gb = format!("{delta:+.1}"),
                    files,
                    "regenerable path size"
                );
            }
        }
        now_sizes.insert(key, bytes);
    }
    if let Ok(mut g) = LAST.lock() {
        *g = Some(now_sizes);
    }
}

/// The regenerable paths amux itself creates or mandates, so watching them is
/// amux's business rather than a guess about the user's disk.
///
/// Deliberately short. `~/.amux/rust-build-target` is here because CLAUDE.md
/// MANDATES one shared `CARGO_TARGET_DIR` for the whole fleet — that rule
/// replaced ~37 per-session target trees that filled the volume on 2026-08-10,
/// and it has no upper bound of its own. `AMUX_DISK_WATCH_PATHS` (colon or comma
/// separated) adds more without a code change.
fn watched_regenerable_paths() -> Vec<std::path::PathBuf> {
    let mut v = vec![crate::config::amux_home().join("rust-build-target")];
    if let Ok(extra) = std::env::var("AMUX_DISK_WATCH_PATHS") {
        for p in extra.split([':', ',']).map(str::trim).filter(|p| !p.is_empty()) {
            v.push(std::path::PathBuf::from(p));
        }
    }
    v.retain(|p| p.exists());
    v
}

/// `(bytes, files)` for a directory tree, counting each INODE once — `du`
/// semantics. Returns `None` when the path cannot be read at all, which must not
/// be reported as a size of zero.
fn du_bytes(root: &std::path::Path) -> Option<(u64, u64)> {
    use std::collections::HashSet;
    use std::os::unix::fs::MetadataExt;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![root.to_path_buf()];
    let mut read_any = false;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        read_any = true;
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else if seen.insert((md.dev(), md.ino())) {
                bytes += md.len();
                files += 1;
            }
        }
    }
    read_any.then_some((bytes, files))
}

/// Say out loud, hourly, when free space is low.
///
/// # Why this is here and not where you would expect
///
/// The disk verdict already existed in two places and NEITHER produced a log
/// line as the disk fell. `/health` carries `disk.state` and is polled
/// constantly, but /health SERVES a field, it does not log. `metrics.rs` logs,
/// but `collect_system_metrics` only runs when something requests
/// /api/metrics, and nothing polls that.
///
/// Measured on 2026-08-24: the volume fell from 144 GB to 13 GB overnight and
/// sat at 100% capacity, and `server-rs.log` contains ZERO warnings across the
/// whole fall. The one match in the file is the old pre-fix line, emitted the
/// previous evening because a human happened to curl the endpoint by hand.
///
/// That is the second time this exact gap has been found in two days, and the
/// first fix (caea1945) corrected the THRESHOLD while leaving the cadence
/// alone — its own commit message diagnosed the cadence problem and then did
/// not fix it. So the log line belongs on something that already ticks whether
/// or not anyone is looking. This job runs hourly, which is frequent enough to
/// catch an overnight fall and far too slow to be noise.
fn report_disk_pressure() {
    let h = crate::api::health::disk_health();
    let Some(free) = h.free_gb else {
        tracing::warn!(job = JOB, "disk pressure UNKNOWN — the volume could not be read");
        return;
    };
    match h.state {
        "critical" => tracing::warn!(
            job = JOB,
            free_gb = free,
            state = "critical",
            "DISK CRITICALLY LOW — under 25 GB free. Builds and DB writes will start \
             failing. Check APFS local snapshots first (`tmutil listlocalsnapshots /`): \
             they pin blocks from deleted files, so deleting things frees `du` and not \
             `df` until they are thinned (DESKT-25)"
        ),
        "warn" => tracing::warn!(
            job = JOB,
            free_gb = free,
            state = "warn",
            "disk headroom low — under 75 GB free, with runway to act. On 2026-08-24 \
             this volume fell 144 GB -> 13 GB in a night (DESKT-25)"
        ),
        _ => tracing::debug!(job = JOB, free_gb = free, state = h.state, "disk"),
    }
}

async fn tick(state: AppState) {
    report_disk_pressure();
    report_regenerable_growth();
    let (scans, running) = {
        let Ok(conn) = state.store.read() else { return };
        let running: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reclaim_scans WHERE status='running'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (last_two_completed(&conn), running)
    };

    let now = crate::api::reclaim::now_secs();
    let newest = scans.first().cloned();
    // AEAB-59: TWO triggers now — the weekly cadence, and a low-free-space one
    // with its own shorter grace window. Read the volume from statfs rather than
    // from the last scan's stored df_free, which is a number from up to seven
    // days ago and is exactly the staleness this is fixing.
    let free_now = crate::api::reclaim::df_bytes(std::path::Path::new(
        &std::env::var("HOME").unwrap_or_else(|_| "/".into()),
    ))
    .map(|(free, _total)| free);
    let stale = should_scan(
        now,
        newest.as_ref().map(|(_, fin)| *fin),
        free_now,
        low_free_bytes(),
        scan_every_secs() as i64,
        low_free_every_secs() as i64,
    );

    // Kick a scan when the newest completed one has aged out, or when the volume
    // is low and the last look is older than the short grace window. Never two at
    // once: the reclaim API refuses that anyway, and a walker competing with
    // another walker is the disruption this whole feature avoids.
    if stale && running == 0 {
        match crate::api::reclaim::start_background_scan(&state).await {
            Ok(id) => tracing::info!(job = JOB, scan = %id, "disk watch started a scan"),
            Err(e) => tracing::warn!(job = JOB, error = %e, "disk watch could not start a scan"),
        }
        return;
    }

    let Some((scan_id, _)) = newest else { return };
    let prev = scans.get(1).map(|(id, _)| id.clone());

    let (rows, free, total) = {
        let Ok(conn) = state.store.read() else { return };
        if already_filed(&conn, &scan_id) {
            return;
        }
        let rows = growth_for(&conn, &scan_id, prev.as_deref(), floor_bytes());
        let (free, total) = conn
            .query_row(
                "SELECT COALESCE(df_free,0), COALESCE(df_total,0) FROM reclaim_scans WHERE id=?1",
                [&scan_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap_or((0, 0));
        (rows, free, total)
    };

    // Nothing over the floor is the healthy state and files nothing. The INFO
    // is what distinguishes "ran and found nothing" from "never ran" — a job
    // whose silence is ambiguous is one nobody can tell is broken.
    if rows.is_empty() {
        tracing::info!(job = JOB, scan = %scan_id, "disk watch: nothing over the floor");
        return;
    }

    let title = format!(
        "{} regenerable cache(s) over {} GiB — review, nothing deleted",
        rows.len(),
        floor_bytes() / (1024 * 1024 * 1024)
    );
    let desc = render(&scan_id, &rows, gib(free as u64), gib(total as u64));
    let src = format!("diskwatch:{scan_id}");
    let n = rows.len();

    let res = state
        .store
        .write_async(move |conn| {
            // Re-checked inside the writer so two ticks cannot race a double file.
            if already_filed(conn, &scan_id) {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            let new = bs::NewIssue {
                title: title.clone(),
                desc: desc.clone(),
                status: "todo".into(),
                session: None,
                // `ops`, not `code`: there is nothing to implement or merge, so
                // a code gate could not be satisfied honestly and the card
                // would rot or be force-closed (ethos rule 3).
                item_type: "ops".into(),
                creator: JOB.into(),
                // owner_type=human on purpose. This card asks a person to make
                // a call about their own files; an agent-owned one would be
                // auto-picked up, and the whole point of the owner choosing
                // "report only" was that the deciding stays with them.
                owner_type: "human".into(),
                due: None,
                due_time: None,
                reviewer: None,
                shepherd: None,
                gate: vec![],
                depends_on: vec![],
                tags: vec!["disk".into(), "reclaim".into()],
            };
            let row = bs::create_issue(conn, &new, crate::api::reclaim::now_secs())?;
            conn.execute(
                "UPDATE issues SET source_ref = ?1 WHERE id = ?2",
                rusqlite::params![src, row.id],
            )?;
            tracing::info!(job = JOB, card = %row.id, paths = n, "disk watch filed a report");
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: row.id.clone(),
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: Some(json!({"id": row.id, "source": JOB})),
                }],
            })
        })
        .await;
    if let Err(e) = res {
        tracing::error!(job = JOB, error = %e, "disk watch could not file its report");
    }
}

pub fn spawn(state: AppState) -> super::PeriodicTask {
    super::spawn_periodic(JOB, TICK_SECS, move || {
        let st = state.clone();
        async move { tick(st).await }
    })
}

#[cfg(test)]
mod tests {

    // ── AEAB-59: the low-space trigger, and the cells that keep it honest ──
    //
    // WEEK/LOW/GRACE are named so a reader can see which rule each case is
    // about. The pairs matter more than the individual cases: every "fires"
    // has a neighbour that must NOT fire, because a trigger that is always
    // true is the same defect as one that never is (ethos rule 7 — a threshold
    // below the baseline is reporting that the machine is ON).
    const WEEK: i64 = 7 * 86_400;
    const GRACE: i64 = 6 * 3_600;
    const LOW: u64 = 5 * 1024 * 1024 * 1024;
    const PLENTY: u64 = 40 * 1024 * 1024 * 1024;

    /// The bug this card is about: 0.94 GB free, last scan 3 days ago, and the
    /// old rule said "not for another 4 days". The interval alone cannot
    /// express urgency.
    #[test]
    fn a_low_volume_is_looked_at_without_waiting_for_the_weekly_wake() {
        let now = 1_000_000;
        let three_days_ago = now - 3 * 86_400;
        assert!(
            super::should_scan(now, Some(three_days_ago), Some(LOW - 1), LOW, WEEK, GRACE),
            "under the floor and 3 days since the last look must scan"
        );
        // THE CONTROL. Same age, healthy volume: the weekly rule still governs
        // and must say no, or the low-space path has simply removed the cadence.
        assert!(
            !super::should_scan(now, Some(three_days_ago), Some(PLENTY), LOW, WEEK, GRACE),
            "plenty of space at 3 days must still wait for the weekly wake"
        );
    }

    /// THE SCAN-STORM CELL. A volume parked under the floor must not start a
    /// walk every hour: that is self-inflicted load aimed at the resource being
    /// investigated, which is the amux-cloud spin-catcher's exact failure.
    #[test]
    fn a_low_volume_still_respects_its_own_grace_window() {
        let now = 1_000_000;
        assert!(
            !super::should_scan(now, Some(now - 60), Some(LOW - 1), LOW, WEEK, GRACE),
            "a scan a minute ago must not be repeated just because space is low"
        );
        assert!(
            super::should_scan(now, Some(now - GRACE - 1), Some(LOW - 1), LOW, WEEK, GRACE),
            "past the grace window it may look again"
        );
    }

    /// The weekly cadence is untouched — this is additive, and a regression
    /// here would trade one blind spot for another.
    #[test]
    fn the_weekly_cadence_still_governs_a_healthy_volume() {
        let now = 10 * 86_400;
        assert!(
            super::should_scan(now, Some(now - WEEK - 1), Some(PLENTY), LOW, WEEK, GRACE),
            "aged past a week must scan regardless of free space"
        );
        assert!(
            !super::should_scan(now, Some(now - WEEK + 1), Some(PLENTY), LOW, WEEK, GRACE),
            "inside the week with space to spare must not"
        );
    }

    /// statfs FAILED. An unreadable volume must fall back to the interval, not
    /// read as full — otherwise a machine that cannot measure itself starts a
    /// walk every grace window forever, and `unwrap_or_default()` on a disk
    /// reading is how a fallback becomes a fault.
    #[test]
    fn an_unreadable_volume_falls_back_to_the_interval_rather_than_reading_as_full() {
        let now = 1_000_000;
        assert!(
            !super::should_scan(now, Some(now - GRACE - 1), None, LOW, WEEK, GRACE),
            "no reading must NOT be treated as low"
        );
        assert!(
            super::should_scan(now, Some(now - WEEK - 1), None, LOW, WEEK, GRACE),
            "no reading still honours the weekly cadence"
        );
    }

    /// Never scanned at all: the interval rule already answered yes and must
    /// keep doing so independently of the disk reading, including when statfs
    /// fails on a first boot.
    #[test]
    fn a_volume_never_scanned_is_scanned_whatever_the_disk_says() {
        assert!(super::should_scan(1_000_000, None, Some(PLENTY), LOW, WEEK, GRACE));
        assert!(super::should_scan(1_000_000, None, None, LOW, WEEK, GRACE));
    }
    use super::*;
    use std::sync::Arc;

    fn store() -> (crate::db::SharedStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        (s, dir)
    }

    fn seed_scan(s: &crate::db::SharedStore, id: &str, status: &str, fin: i64) {
        let (id, status) = (id.to_string(), status.to_string());
        s.write(move |c| {
            c.execute(
                "INSERT INTO reclaim_scans (id, started_at, finished_at, status, roots, df_free, df_total)
                 VALUES (?1, ?2, ?2, ?3, '[]', 100000000000, 1000000000000)",
                rusqlite::params![id, fin, status],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();
    }

    fn seed_finding(s: &crate::db::SharedStore, scan: &str, path: &str, gib: u64, regen: bool) {
        let (scan, path) = (scan.to_string(), path.to_string());
        s.write(move |c| {
            c.execute(
                "INSERT INTO reclaim_findings
                   (scan_id, category, path, bytes, file_count, mtime, regenerable, detail)
                 VALUES (?1,'build',?2,?3,1,0,?4,'')",
                rusqlite::params![scan, path, (gib * 1_073_741_824) as i64, regen as i64],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .unwrap();
    }

    /// The floor selects, `regenerable` gates, and the previous reading is
    /// carried across — with a control proving the query excluded something,
    /// since an unbounded match and a correct one look identical from the rows.
    #[test]
    fn growth_reports_only_regenerable_paths_over_the_floor() {
        let (s, _d) = store();
        seed_scan(&s, "S2", "done", 200);
        seed_scan(&s, "S1", "done", 100);
        seed_finding(&s, "S1", "/big", 40, true);
        seed_finding(&s, "S2", "/big", 55, true);
        seed_finding(&s, "S2", "/small", 3, true); // under the floor
        seed_finding(&s, "S2", "/precious", 90, false); // not regenerable

        let conn = s.read().unwrap();
        let rows = growth_for(&conn, "S2", Some("S1"), 25 * 1_073_741_824);

        assert_eq!(rows.len(), 1, "floor and regenerable must both bite: {rows:?}");
        assert_eq!(rows[0].path, "/big");
        assert_eq!(rows[0].was, Some(40 * 1_073_741_824));
        assert!((rows[0].delta_gib().unwrap() - 15.0).abs() < 0.01);

        // Control: /precious is over the floor and IS present, so its absence
        // above is the regenerable flag and not an empty table.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM reclaim_findings WHERE scan_id='S2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3, "the rows the filter rejected must actually exist");
    }

    /// A path with no previous reading must not be rendered as growth from
    /// zero. "+82 GiB this week" on a path that was never measured is a
    /// fabricated trend, and it is the number a reader would act on.
    #[test]
    fn a_path_with_no_previous_reading_is_not_reported_as_growth() {
        let g = Growth { path: "/new".into(), bytes: 82 * 1_073_741_824, was: None };
        let out = render("S9", &[g], 100.0, 1000.0);
        assert!(out.contains("no previous reading"), "{out}");
        assert!(!out.contains("+82"), "must not invent a delta: {out}");
    }

    /// Comparing against a partial scan would report every unvisited path as
    /// having shrunk to nothing, so only completed scans are candidates.
    #[test]
    fn scans_with_findings_are_compared_whatever_their_terminal_status() {
        let (s, _d) = store();
        // SUPERSEDES only_completed_scans_are_compared. That test pinned
        // `status='done'`, whose stated rationale is disproved by
        // a_partial_previous_scan_never_manufactures_a_shrink. Requiring `done`
        // admitted 1 scan in 6 on the real box and discarded partial scans
        // holding 124 and 119 findings (DESKT-28).
        seed_scan(&s, "OLD_DONE", "done", 100);
        seed_finding(&s, "OLD_DONE", "/a", 5, true);
        seed_scan(&s, "STALLED", "stalled", 150);
        seed_finding(&s, "STALLED", "/a", 6, true);
        seed_scan(&s, "CANCELLED", "cancelled", 175);
        seed_finding(&s, "CANCELLED", "/a", 7, true);

        let conn = s.read().unwrap();
        let got: Vec<String> = last_two_completed(&conn).into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            got,
            vec!["CANCELLED".to_string(), "STALLED".to_string()],
            "the two most recent scans WITH FINDINGS win, regardless of terminal status"
        );
    }

    /// The two things that genuinely must still be excluded, or widening the
    /// filter trades one fabricated trend for another.
    #[test]
    fn a_running_scan_and_an_empty_scan_are_still_excluded() {
        let (s, _d) = store();
        seed_scan(&s, "REAL", "done", 100);
        seed_finding(&s, "REAL", "/a", 5, true);
        // Totals still moving — comparing against it reads a number mid-write.
        seed_scan(&s, "RUNNING", "running", 150);
        seed_finding(&s, "RUNNING", "/a", 6, true);
        // Recorded nothing — would make every path look new.
        seed_scan(&s, "EMPTY", "interrupted", 175);

        let conn = s.read().unwrap();
        let got: Vec<String> = last_two_completed(&conn).into_iter().map(|(id, _)| id).collect();
        assert_eq!(got, vec!["REAL".to_string()], "got {got:?}");
    }

    /// One card per scan, forever. A weekly job on an hourly tick would
    /// otherwise file the same report 168 times.
    #[tokio::test]
    async fn a_scan_is_reported_at_most_once() {
        let (s, _d) = store();
        let state = AppState {
            store: s.clone(),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let now = crate::api::reclaim::now_secs();
        seed_scan(&s, "S1", "done", now);
        seed_finding(&s, "S1", "/huge", 90, true);

        tick(state.clone()).await;
        tick(state.clone()).await;
        tick(state.clone()).await;

        let conn = s.read().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE source_ref='diskwatch:S1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "three ticks on one scan must file exactly one card");

        // And it is owned by a human, so board_drive never auto-picks it up:
        // the owner chose report-only precisely so the deciding stays theirs.
        let (owner, kind): (String, String) = conn
            .query_row(
                "SELECT owner_type, type FROM issues WHERE source_ref='diskwatch:S1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(owner, "human");
        assert_eq!(kind, "ops");
    }

    // ---- DESKT-27: the cheap sampler that cannot lose the re-exec race ----

    /// `du_bytes` must count each INODE once. A naive per-file sum of the real
    /// shared target dir reports 252.7 GB where `du` reports 97 GB, because cargo
    /// hardlinks heavily — and only the du-shaped number is comparable to the
    /// free space a reader is looking at.
    #[test]
    fn du_bytes_counts_a_hardlink_once() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        std::fs::write(&a, vec![7u8; 4096]).unwrap();
        let (before, files_before) = du_bytes(d.path()).expect("readable");
        std::fs::hard_link(&a, d.path().join("b")).unwrap();
        let (after, files_after) = du_bytes(d.path()).expect("readable");
        assert_eq!(
            before, after,
            "a hardlink adds a NAME, not bytes — {before} -> {after} means du semantics are wrong"
        );
        assert_eq!(files_before, files_after, "and it must not double-count the file either");
    }

    /// …while a genuinely new file DOES count, or the test above is satisfied by
    /// a function that always returns the same number.
    #[test]
    fn du_bytes_still_counts_a_real_second_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), vec![7u8; 4096]).unwrap();
        let (before, _) = du_bytes(d.path()).unwrap();
        std::fs::write(d.path().join("c"), vec![9u8; 8192]).unwrap();
        let (after, _) = du_bytes(d.path()).unwrap();
        assert!(after > before, "a distinct file must add bytes: {before} -> {after}");
    }

    /// An unreadable path is None, never Some(0). Reporting a missing tree as
    /// "0 GB" would render as a dramatic reclaim in the delta line.
    #[test]
    fn an_unreadable_path_is_none_not_zero() {
        assert!(du_bytes(std::path::Path::new("/no-such-dir-9f3a/nope")).is_none());
    }

    /// Nested directories are summed, or the sampler would report only the top
    /// level of a cargo tree — which is where almost none of the bytes are.
    #[test]
    fn du_bytes_descends() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("x/y/z");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f"), vec![1u8; 16384]).unwrap();
        let (bytes, files) = du_bytes(d.path()).expect("readable");
        assert_eq!(files, 1);
        assert!(bytes >= 16384, "nested bytes must be counted, got {bytes}");
    }

    /// The watch list must always include the shared cargo target dir — that is
    /// the path CLAUDE.md mandates the whole fleet share, and the one whose
    /// unbounded growth this sampler exists to make visible (DESKT-26).
    #[test]
    fn the_shared_cargo_target_is_always_watched_when_present() {
        let shared = crate::config::amux_home().join("rust-build-target");
        let watched = watched_regenerable_paths();
        if shared.exists() {
            assert!(watched.contains(&shared), "shared target dir must be watched: {watched:?}");
        }
        // Every returned path must exist — a non-existent entry would log a
        // baseline of nothing, forever.
        for p in &watched {
            assert!(p.exists(), "watched path does not exist: {p:?}");
        }
    }

    // ---- DESKT-28: is the `status='done'` filter's stated hazard real? ----

    /// The filter's own comment says including a partial scan "would report every
    /// unvisited path as having shrunk to nothing". This constructs exactly that
    /// case and asserts it does NOT happen — because two other layers already
    /// prevent it, which is why the filter is discarding usable data for free.
    #[test]
    fn a_partial_previous_scan_never_manufactures_a_shrink() {
        let (s, _d) = store();
        // PARTIAL: interrupted before it ever reached /b.
        seed_scan(&s, "PARTIAL", "interrupted", 100);
        seed_finding(&s, "PARTIAL", "/a", 10, true);
        // COMPLETE: saw both.
        seed_scan(&s, "FULL", "done", 200);
        seed_finding(&s, "FULL", "/a", 12, true);
        seed_finding(&s, "FULL", "/b", 50, true);

        let conn = s.read().unwrap();
        let rows = growth_for(&conn, "FULL", Some("PARTIAL"), 1);

        let a = rows.iter().find(|g| g.path == "/a").expect("/a present");
        let b = rows.iter().find(|g| g.path == "/b").expect("/b present");

        // /a was in both -> a real delta.
        assert_eq!(a.was, Some(10 * 1_073_741_824), "/a should compare against the partial");
        // /b was NEVER VISITED by the partial scan. The feared behaviour is that it
        // reads as "shrunk to nothing". It must instead read as "no previous reading".
        assert_eq!(
            b.was, None,
            "an unvisited path must have NO previous reading, not a zero to shrink from"
        );
        // And nothing anywhere may report a negative delta from this.
        for g in &rows {
            if let Some(d) = g.delta_gib() {
                assert!(d >= 0.0, "phantom shrink on {}: {d}", g.path);
            }
        }
        // The rendered card must SAY so rather than fabricate a trend.
        let out = render("FULL", &rows, 100.0, 1000.0);
        assert!(out.contains("/b` is **50.0 GiB** (no previous reading to compare)"), "{out}");
        assert!(!out.contains("-50.0"), "no fabricated shrink in the card: {out}");
    }

    /// The mirror: a PARTIAL scan as the CURRENT one reports only what it reached.
    /// Paths it never got to are simply absent — they cannot be reported as
    /// shrunk, because the query iterates the current scan's own findings.
    #[test]
    fn a_partial_current_scan_reports_a_subset_never_a_shrink() {
        let (s, _d) = store();
        seed_scan(&s, "FULL", "done", 100);
        seed_finding(&s, "FULL", "/a", 10, true);
        seed_finding(&s, "FULL", "/b", 50, true);
        seed_scan(&s, "PARTIAL", "interrupted", 200);
        seed_finding(&s, "PARTIAL", "/a", 14, true);

        let conn = s.read().unwrap();
        let rows = growth_for(&conn, "PARTIAL", Some("FULL"), 1);

        assert_eq!(rows.len(), 1, "only the path it reached: {rows:?}");
        assert_eq!(rows[0].path, "/a");
        assert_eq!(rows[0].was, Some(10 * 1_073_741_824));
        assert!(
            !rows.iter().any(|g| g.path == "/b"),
            "/b was never visited by the current scan, so it must not appear at all"
        );
    }
}
