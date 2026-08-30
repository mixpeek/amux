//! Storage retention — the sweep that keeps append-only tables and cache
//! directories from being creep by construction.
//!
//! Filed after the 2026-08-10 disk-full incident: the machine reached 741MB
//! free on a 1.8TB volume with a 50-session fleet running, and transcript
//! writes started failing with ENOSPC. Most of that was build trees outside
//! amux, but the audit that followed found that **amux's own durable state has
//! almost no retention anywhere**: seven append-only tables with no pruning at
//! all, and the server's own tracing log growing for the lifetime of the
//! process. None of it was a leak. All of it was working exactly as written,
//! forever.
//!
//! # Why one job instead of pruning at each write site
//!
//! `_amux_request_log` already prunes itself, and it does it well — but it
//! does it by counting rows inside its own writer task (`SWEEP_EVERY = 1000`).
//! That works because the request log is written constantly. Applied to the
//! other tables it fails in the direction nobody notices: `media-cache` and
//! `uploads` both have correct prune logic that only runs when a NEW job
//! starts, so a fleet that stops transcoding never prunes its transcodes. A
//! cache that only evicts while it is growing is a cache that only evicts when
//! you do not need it to.
//!
//! So retention here is driven by a `PeriodicTask` — time, not traffic. The
//! tick is slow (an hour by default) because nothing here is urgent and a
//! sweeper that runs hot competes with the traffic it is measuring.
//!
//! # The timestamp units are NOT uniform, and that is the dangerous part
//!
//! This is the reason this file is a table of specs rather than a loop over
//! table names. The seven tables carry four different time representations:
//!
//! | table                  | column       | unit                        |
//! |------------------------|--------------|-----------------------------|
//! | `interaction_log`      | `ts`         | **milliseconds** (INTEGER)  |
//! | `cmd_history`          | `ts`         | **milliseconds** (INTEGER)  |
//! | `session_events`       | `ts`         | seconds (REAL)              |
//! | `token_ledger`         | `ts`         | seconds (INTEGER)           |
//! | `schedule_runs`        | `ran_at`     | seconds (INTEGER)           |
//! | `schedule_audit`       | `ts`         | seconds (INTEGER)           |
//! | `_amux_state_events`   | `at`         | **ISO-8601 TEXT**           |
//!
//! `ethos.md` records that two sessions in one evening wrote
//! `datetime(ts,'unixepoch')` against `interaction_log` and got a cutoff ~1000x
//! too small, so the filter matched the entire table — and one of them nearly
//! reported the whole historical backlog as post-fix regressions. That was a
//! READ. This file does DELETEs.
//!
//! # Both directions are wrong, and they fail in OPPOSITE ways
//!
//! Writing the guard is what surfaced this; the first version of it defended
//! the wrong direction, and the test caught that rather than the code. For
//! `DELETE WHERE ts < cutoff`:
//!
//! - **A seconds table declared `Millis`** builds a cutoff ~1000x too LARGE, so
//!   every row is "older" than it: it **deletes the entire table**. Loud,
//!   catastrophic, unrecoverable.
//! - **A millisecond table declared `Secs`** builds a cutoff ~1000x too SMALL,
//!   so no row is ever older than it: it **deletes nothing, forever**. Silent,
//!   harmless to the data, and it means the bound you think you have does not
//!   exist — which is exactly how this incident happened in the first place.
//!
//! So there are two guards, and they are not symmetric because the failures are
//! not symmetric.
//!
//! # Guard 1 (refusal): a sweep must prove it EXCLUDED something
//!
//! [`sweep_one`] never issues a `DELETE` it has not first bounded. It counts
//! the rows that would REMAIN, and if a non-empty table would be left with zero
//! rows it refuses and logs an error. A sweep that removes 100% of a table is
//! not retention, it is a unit bug — and ethos rule 7's point is that an
//! unbounded match and a correct match look identical from the rows alone.
//!
//! # Guard 2 (warning): a sweep that never fires is not a bound
//!
//! If a sweep deletes nothing from a table it should have reached, the cutoff
//! is not landing where it should. That is not dangerous, so it warns rather
//! than refuses — but it is the failure mode that leaves a table growing behind
//! a knob everyone believes is bounding it.
//!
//! # What guard 1 costs, stated plainly
//!
//! A table whose rows are ALL older than its retention — a subsystem that
//! stopped being written months ago — is indistinguishable from a unit bug from
//! the rows alone, so guard 1 refuses it too and that table does not shrink.
//! That is the deliberate side to be wrong on: a dormant table is by definition
//! not growing, the refusal is logged with the table name and the cutoff, and a
//! human can lower the knob on purpose. The inverse default (delete when
//! unsure) buys nothing and can empty a live table.
//!
//! # Every eviction is logged, or it did not happen
//!
//! A silent eviction destroys the evidence somebody needed. Every delete and
//! every file removal here emits a `tracing::info!` with the count and the
//! knob that authorised it, and the last sweep is held for
//! `GET /api/debug/storage`.

use crate::api::AppState;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::RwLock;

/// Default tick: hourly. Retention is not time-critical; the only thing that
/// matters is that it happens without traffic.
pub const STORAGE_TICK_SECS: u64 = 3600;

/// How `ts` is stored. Getting this wrong is the failure this whole module is
/// shaped around, so it is explicit per table and never inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsUnit {
    Secs,
    Millis,
    /// ISO-8601 text, compared lexicographically (which is chronological for
    /// this format, and is how the column is already queried elsewhere).
    IsoText,
}

/// One append-only table and the knob that bounds it.
#[derive(Debug, Clone, Copy)]
pub struct SweepSpec {
    pub table: &'static str,
    pub ts_col: &'static str,
    pub unit: TsUnit,
    /// Env knob. Set to 0 to disable this table's sweep entirely.
    pub env: &'static str,
    pub default_days: f64,
}

/// The tables amux appends to and never trimmed.
///
/// # The defaults are computed, not chosen
///
/// Each one is `rows/day x bytes/row x retain_days`, measured on the live DB on
/// 2026-08-10 (416MB, 24-51 days of accumulated history depending on the
/// table). The first draft of this table used round numbers picked by feel and
/// they were wrong by an order of magnitude in both directions: 365 days on
/// `token_ledger` allows 4.7M rows, and 90 days on `interaction_log` — which
/// costs 1,893 bytes/row, ~19x what `session_events` costs — allows 524MB from
/// that one table. Bytes/row varies 50x across these tables, so a single
/// uniform retention is guaranteed to be both too tight and too loose.
///
/// Steady state with the values below is ~415MB total, i.e. the DB stops
/// growing at roughly the size it is today instead of growing without limit.
/// Re-derive rather than adjust by feel:
///
/// ```sql
/// SELECT ROUND(SUM(pgsize)/1048576.0,1) FROM dbstat WHERE name='<table>';
/// SELECT COUNT(*) FROM <table> WHERE <ts_col> >= <now - 7 days>;  -- /7 = rows/day
/// ```
///
/// The first tick after this ships is a MIGRATION EVENT, not a day's work
/// (ethos rule 1's corollary — the owner digest dropped a filter and its next
/// run emitted 92 cards). Measured against the live DB, only `schedule_runs`
/// has anything to delete on tick one (1,845 of 20,626 rows); every other table
/// holds less history than its cap, so tick one is a no-op for them.
pub const SPECS: &[SweepSpec] = &[
    // 180k rows, 101 B/row, ~4,742 rows/day, 7 insert sites in board_drive driven
    // by a periodic loop — i.e. it grows whether or not anyone is using amux.
    // 90d x 4,742 x 101B = ~43MB steady state.
    SweepSpec {
        table: "session_events",
        ts_col: "ts",
        unit: TsUnit::Secs,
        env: "AMUX_SESSION_EVENTS_RETAIN_DAYS",
        default_days: 90.0,
    },
    // MILLISECONDS. The expensive one: 1,893 B/row (it stores `before`/`detail`/
    // `result` blobs), 3,236 rows/day. At 90d this single table would reach 524MB —
    // larger than the entire DB today — which is why it gets 14d and not the 90d
    // the cheaper tables can afford. 14d x 3,236 x 1,893B = ~82MB.
    SweepSpec {
        table: "interaction_log",
        ts_col: "ts",
        unit: TsUnit::Millis,
        env: "AMUX_INTERACTION_LOG_RETAIN_DAYS",
        default_days: 14.0,
    },
    // The highest row count in the DB (387k) but cheap at 104 B/row, 7,995 rows/day.
    // Cost accounting, so keep it long: it is the only record of what the fleet
    // spent. 365d would be 4.7M rows / ~300MB; 180d x 7,995 x 104B = ~150MB.
    SweepSpec {
        table: "token_ledger",
        ts_col: "ts",
        unit: TsUnit::Secs,
        env: "AMUX_TOKEN_LEDGER_RETAIN_DAYS",
        default_days: 180.0,
    },
    // 36 B/row and only 169 rows/day, but 159 DAYS of history accumulated because
    // nothing ever deleted one. The only table with anything to delete on tick one
    // (1,845 of 20,626 rows). 90d x 169 x 36B = well under 1MB.
    SweepSpec {
        table: "schedule_runs",
        ts_col: "ran_at",
        unit: TsUnit::Secs,
        env: "AMUX_SCHEDULE_RUNS_RETAIN_DAYS",
        default_days: 90.0,
    },
    // 1,735 B/row but only ~3 rows/day — attribution for schedule mutations, and
    // the thing you want when 8 schedules vanish (AMUX-1812). Keep it long; it is
    // ~1MB at 180d.
    SweepSpec {
        table: "schedule_audit",
        ts_col: "ts",
        unit: TsUnit::Secs,
        env: "AMUX_SCHEDULE_AUDIT_RETAIN_DAYS",
        default_days: 180.0,
    },
    // MILLISECONDS, and 1,629 B/row at 619 rows/day. Trimmed on exactly ONE of its
    // five insert paths today (session_verbs keeps newest 200 per session;
    // history.rs x3 and board_drive insert with no trim at all), so the per-session
    // cap is not a bound. 90d x 619 x 1,629B = ~91MB.
    SweepSpec {
        table: "cmd_history",
        ts_col: "ts",
        unit: TsUnit::Millis,
        env: "AMUX_CMD_HISTORY_RETAIN_DAYS",
        default_days: 90.0,
    },
    // ISO-8601 TEXT. The delta-sync journal: `MIN(rev)` over it answers "how far
    // back does history go", and a client asking for an older rev is told to
    // full-sync. Pruning is therefore SAFE but shortens the window in which a stale
    // client can delta-sync instead of full-syncing — and no client is 14 days
    // stale. It is also the fastest-growing table here (~3,900 rows/day at 835
    // B/row, because every fleet mutation writes a JSON payload snapshot): 365d
    // would be ~1.2GB. 14d x 3,900 x 835B = ~46MB.
    SweepSpec {
        table: "_amux_state_events",
        ts_col: "at",
        unit: TsUnit::IsoText,
        env: "AMUX_STATE_EVENTS_RETAIN_DAYS",
        default_days: 14.0,
    },
];

/// `<ENV>`: process env wins, then `server.env`, then the spec default — the
/// same precedence `AMUX_REQLOG_RETAIN_DAYS` and every other knob uses.
pub fn retain_days(spec: &SweepSpec) -> f64 {
    if let Some(d) = std::env::var(spec.env).ok().and_then(|v| v.trim().parse::<f64>().ok()) {
        return d;
    }
    crate::config::parse_env_file(&amux_home().join("server.env"))
        .get(spec.env)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(spec.default_days)
}

pub use crate::config::amux_home;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// What one table's sweep did. `Refused` is a first-class outcome, not an
/// error path: it is the guard doing its job and it must be visible.
#[derive(Debug, Clone, PartialEq)]
pub enum SweepResult {
    Disabled,
    Deleted { rows: usize, kept: i64 },
    /// The cutoff would have emptied a non-empty table — almost certainly a
    /// unit mismatch. Nothing was deleted.
    Refused { total: i64, cutoff: String },
    Error(String),
}

/// The cutoff literal for a table, expressed in that table's OWN unit.
pub fn cutoff_for(unit: TsUnit, now_secs: f64, days: f64) -> String {
    let cut = now_secs - days * 86_400.0;
    match unit {
        TsUnit::Secs => format!("{cut}"),
        TsUnit::Millis => format!("{}", (cut * 1000.0).round() as i64),
        TsUnit::IsoText => {
            // Same shape the writer emits: 2026-08-09T18:22:43.092695+00:00.
            // Lexicographic comparison is chronological for this format.
            let secs = cut.max(0.0) as i64;
            iso_utc(secs)
        }
    }
}

/// Minimal civil-time formatter (days-from-epoch, Howard Hinnant's algorithm)
/// so this file needs no new dependency for one timestamp per sweep.
fn iso_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000000+00:00")
}

/// The oldest row's timestamp, normalised to SECONDS, for guard 2. `IsoText`
/// returns None: the ISO column is compared lexicographically and cannot land
/// 1000x off, so the mismatch this guard exists to catch cannot occur there.
fn oldest_secs(conn: &Connection, spec: &SweepSpec) -> Option<f64> {
    match spec.unit {
        TsUnit::IsoText => None,
        TsUnit::Secs | TsUnit::Millis => {
            let raw: f64 = conn
                .query_row(
                    &format!("SELECT MIN({}) FROM {}", spec.ts_col, spec.table),
                    [],
                    |r| r.get(0),
                )
                .ok()?;
            Some(if spec.unit == TsUnit::Millis { raw / 1000.0 } else { raw })
        }
    }
}

/// Sweep one table. Counts what would remain BEFORE deleting anything, and
/// refuses rather than empty a table — see the module docs.
pub fn sweep_one(conn: &Connection, spec: &SweepSpec, now_secs: f64) -> SweepResult {
    let days = retain_days(spec);
    if days <= 0.0 {
        return SweepResult::Disabled;
    }
    let cutoff = cutoff_for(spec.unit, now_secs, days);

    let total: i64 = match conn.query_row(&format!("SELECT COUNT(*) FROM {}", spec.table), [], |r| {
        r.get(0)
    }) {
        Ok(v) => v,
        Err(e) => return SweepResult::Error(format!("count {}: {e}", spec.table)),
    };
    if total == 0 {
        return SweepResult::Deleted { rows: 0, kept: 0 };
    }

    // THE GUARD. Ask what survives, not what dies: an unbounded match and a
    // correct match are indistinguishable from the deleted rows alone.
    let kept: i64 = match conn.query_row(
        &format!("SELECT COUNT(*) FROM {} WHERE {} >= ?1", spec.table, spec.ts_col),
        rusqlite::params![&cutoff],
        |r| r.get(0),
    ) {
        Ok(v) => v,
        Err(e) => return SweepResult::Error(format!("guard {}: {e}", spec.table)),
    };
    if kept == 0 {
        tracing::error!(
            table = spec.table,
            column = spec.ts_col,
            unit = ?spec.unit,
            cutoff = %cutoff,
            total,
            knob = spec.env,
            "storage retention REFUSED: cutoff would delete every row — this is a \
             timestamp-unit mismatch, not an old table. Nothing deleted."
        );
        return SweepResult::Refused { total, cutoff };
    }

    match conn.execute(
        &format!("DELETE FROM {} WHERE {} < ?1", spec.table, spec.ts_col),
        rusqlite::params![&cutoff],
    ) {
        Ok(rows) => {
            if rows > 0 {
                tracing::info!(
                    table = spec.table,
                    deleted = rows,
                    kept,
                    retain_days = days,
                    knob = spec.env,
                    "storage retention sweep"
                );
            } else if let Some(oldest_secs) = oldest_secs(conn, spec) {
                // GUARD 2. Deleting nothing is safe, so this warns instead of
                // refusing — but a table holding rows the sweep should have
                // reached is a table whose bound is not working.
                //
                // TWO tells, and the first one is not the obvious one. A
                // MILLISECOND column read as seconds does not look OLD, it
                // looks like it is dated ~55,000 years in the FUTURE, so an
                // "older than retention" test alone never fires on the exact
                // mismatch this guard exists to catch. That is not a
                // hypothetical: the first version of this check tested only
                // `age > retention` and the unit test walked straight through
                // it.
                let age_days = (now_secs - oldest_secs) / 86_400.0;
                let reason = if age_days < 0.0 {
                    "the oldest row is dated in the FUTURE, which a real timestamp cannot be — \
                     this column is almost certainly milliseconds being read as seconds"
                } else if age_days > days * 1.5 {
                    "the table holds rows older than its own retention window, so the cutoff is \
                     not landing where it should"
                } else {
                    ""
                };
                if !reason.is_empty() {
                    tracing::warn!(
                        table = spec.table,
                        column = spec.ts_col,
                        unit = ?spec.unit,
                        oldest_row_age_days = age_days,
                        retain_days = days,
                        knob = spec.env,
                        reason,
                        "storage retention deleted NOTHING and should have — this knob is not \
                         bounding this table"
                    );
                }
            }
            SweepResult::Deleted { rows, kept }
        }
        Err(e) => SweepResult::Error(format!("delete {}: {e}", spec.table)),
    }
}

// ---------------------------------------------------------------------------
// Filesystem sinks
// ---------------------------------------------------------------------------

/// `AMUX_SERVER_LOG_MAX_MB` (default 64, 0 disables). Separate knob from
/// `AMUX_LOG_MAX_MB` on purpose: that one bounds each SESSION's pane log, and
/// somebody tuning per-session capture should not silently resize the server's
/// own tracing log with it.
fn server_log_max_bytes() -> u64 {
    env_u64("AMUX_SERVER_LOG_MAX_MB", 64) * 1024 * 1024
}

/// Rotate `logs/server-rs.log` by **copy-truncate**, and the choice matters.
///
/// The fd is owned by the `tracing_subscriber` appender installed at boot
/// (`lib.rs`), which offers no rotation hook and never reopens. Renaming the
/// file out from under it would leave the writer appending to the RENAMED
/// inode forever — the new `server-rs.log` would stay empty and the bound
/// would silently not exist. That is not hypothetical: `session_verbs.rs`
/// documents five live session logs pinned at exactly 10,485,760 bytes from
/// exactly this race, and the rule it draws is that rotation has ONE owner,
/// the process holding the fd.
///
/// Copy-truncate keeps that rule: we never touch the fd or the path, we copy
/// the bytes aside and truncate in place, and an `O_APPEND` writer simply
/// continues at the new end. The honest cost is a small race — lines written
/// between the copy and the truncate are lost — which is the standard
/// `logrotate copytruncate` trade and is why the marker below records the
/// rotation in the log itself rather than only in the sweep's return value.
pub fn rotate_server_log(logs_dir: &Path) -> Option<u64> {
    let max = server_log_max_bytes();
    if max == 0 {
        return None;
    }
    let log = logs_dir.join("server-rs.log");
    let size = std::fs::metadata(&log).ok()?.len();
    if size < max {
        return None;
    }
    let prev = logs_dir.join("server-rs.log.1");
    if std::fs::copy(&log, &prev).is_err() {
        return None;
    }
    // Truncate in place — do NOT rename; the tracing appender holds this fd.
    if std::fs::OpenOptions::new().write(true).truncate(true).open(&log).is_err() {
        return None;
    }
    tracing::info!(
        rolled_bytes = size,
        max_bytes = max,
        knob = "AMUX_SERVER_LOG_MAX_MB",
        previous = %prev.display(),
        "=== amux server log rotated (copy-truncate; previous generation kept as .1) ==="
    );
    Some(size)
}

/// Delete files in `dir` whose mtime is older than `max_age_secs`. Returns
/// (files removed, bytes freed). Non-recursive and never removes directories:
/// a holding area's SHAPE is somebody's, only its age is ours.
pub fn prune_dir_by_age(dir: &Path, max_age_secs: u64, label: &str) -> (usize, u64) {
    if max_age_secs == 0 {
        return (0, 0);
    }
    let now = std::time::SystemTime::now();
    let Ok(rd) = std::fs::read_dir(dir) else { return (0, 0) };
    let (mut n, mut bytes) = (0usize, 0u64);
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let age = md
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if age > max_age_secs && std::fs::remove_file(e.path()).is_ok() {
            n += 1;
            bytes += md.len();
        }
    }
    if n > 0 {
        tracing::info!(
            dir = %dir.display(),
            removed = n,
            freed_bytes = bytes,
            max_age_secs,
            knob = label,
            "storage sweep evicted files"
        );
    }
    (n, bytes)
}

/// Free bytes on the volume holding `path`.
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    let df = ["/bin/df", "/usr/bin/df"].iter().find(|c| Path::new(c).is_file())?;
    let out = std::process::Command::new(df).arg("-Pk").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let avail_kb: u64 = text.lines().last()?.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

// ---------------------------------------------------------------------------
// The tick
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct StorageReport {
    pub at: f64,
    pub tables: Vec<(String, String)>,
    pub rotated_bytes: u64,
    pub files_removed: usize,
    pub bytes_freed: u64,
    pub free_bytes: Option<u64>,
    pub took_ms: f64,
}

fn last_report_cell() -> &'static RwLock<Option<StorageReport>> {
    static CELL: std::sync::OnceLock<RwLock<Option<StorageReport>>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

pub fn last_report() -> Option<StorageReport> {
    last_report_cell().read().ok().and_then(|c| c.clone())
}

/// The directories the storage sweep REAPS BY AGE — the single authority on what
/// "ephemeral" means on this box (nissan, AMUX-3386). Each entry is (dir name
/// under home, retain-days env var, default days). `settings::is_ephemeral_path`
/// reads the NAMES from here so a config value persisted into ANY of them is
/// refused, and the reaper and the guard cannot drift: whatever gets pruned is,
/// by definition, unsafe to point durable config at. Add a scratch dir here and
/// both the reaper and the config-time-bomb guard pick it up at once.
pub const AGE_PRUNED_DIRS: &[(&str, &str, u64)] = &[
    ("media-cache", "AMUX_MEDIA_CACHE_RETAIN_DAYS", 30),
    ("uploads", "AMUX_UPLOADS_RETAIN_DAYS", 7),
    ("spin-dumps", "AMUX_SPIN_DUMPS_RETAIN_DAYS", 14),
];

pub async fn storage_tick(state: &AppState, home: &Path) -> StorageReport {
    let t0 = std::time::Instant::now();
    let now = unix_now();
    let mut rep = StorageReport { at: now, ..Default::default() };

    // DB retention runs on the writer thread — one transaction, and
    // `applied: false` so retention never bumps `_amux_rev`. A delete of aged
    // rows is not a state change any client needs to delta-sync (the
    // request-log sweep established this; without it, every sweep would look
    // like a fleet-wide mutation and trigger a sync storm).
    // Results travel back on an Arc the closure captures: `write` must return a
    // `WriteOutcome`, so there is no other channel for them, and a module-level
    // static would let two ticks (or a test) overwrite each other's results.
    let sink: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
    let sink2 = sink.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let mut out = Vec::new();
            for spec in SPECS {
                out.push((spec.table.to_string(), format!("{:?}", sweep_one(conn, spec, now))));
            }
            if let Ok(mut s) = sink2.lock() {
                *s = out;
            }
            Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
        })
        .await;
    match res {
        Err(e) => rep.tables.push(("<write>".into(), format!("Error({e})"))),
        Ok(_) => rep.tables = sink.lock().map(|s| s.clone()).unwrap_or_default(),
    }

    let logs = home.join("logs");
    rep.rotated_bytes = rotate_server_log(&logs).unwrap_or(0);

    // Age-reaped dirs, driven by the AGE_PRUNED_DIRS authority (above) so the
    // prune and settings::is_ephemeral_path read ONE list. media-cache/uploads
    // prune logic was correct but only fired while GROWING; spin-dumps are
    // incident holding areas nothing had ever removed. Running them on a timer is
    // the whole fix.
    let (mut files, mut bytes) = (0usize, 0u64);
    for (name, env_key, default_days) in AGE_PRUNED_DIRS {
        let days = env_u64(env_key, *default_days);
        let (n, b) = prune_dir_by_age(&home.join(name), days * 86_400, env_key);
        files += n;
        bytes += b;
    }
    rep.files_removed = files;
    rep.bytes_freed = bytes;
    rep.free_bytes = disk_free_bytes(home);
    rep.took_ms = t0.elapsed().as_secs_f64() * 1000.0;
    *last_report_cell().write().unwrap() = Some(rep.clone());
    rep
}

pub fn spawn(state: AppState) -> Option<super::PeriodicTask> {
    let secs = env_u64("AMUX_STORAGE_SWEEP_SECS", STORAGE_TICK_SECS);
    if secs == 0 {
        tracing::info!("storage sweep: disabled (AMUX_STORAGE_SWEEP_SECS=0)");
        return None;
    }
    let home = amux_home();
    Some(super::spawn_periodic("storage", secs, move || {
        let state = state.clone();
        let home = home.clone();
        async move {
            let r = storage_tick(&state, &home).await;
            if r.rotated_bytes > 0 || r.files_removed > 0 {
                tracing::info!(
                    rotated_bytes = r.rotated_bytes,
                    files_removed = r.files_removed,
                    bytes_freed = r.bytes_freed,
                    "storage sweep tick"
                );
            }
        }
    }))
}

/// `GET /api/debug/storage` — every knob, its current value, and what the last
/// sweep did. "How close are we?" in one request.
pub async fn debug_storage() -> axum::Json<Value> {
    let home = amux_home();
    let free = disk_free_bytes(&home);
    let r = last_report();
    axum::Json(json!({
        "free_bytes": free,
        "free_gb": free.map(|b| (b as f64 / 1e9 * 10.0).round() / 10.0),
        "knobs": SPECS.iter().map(|s| json!({
            "table": s.table, "env": s.env, "retain_days": retain_days(s),
            "ts_col": s.ts_col, "unit": format!("{:?}", s.unit),
        })).collect::<Vec<_>>(),
        "server_log_max_mb": server_log_max_bytes() / 1024 / 1024,
        "sweep_secs": env_u64("AMUX_STORAGE_SWEEP_SECS", STORAGE_TICK_SECS),
        "last": r.map(|r| json!({
            "at": r.at, "tables": r.tables, "rotated_bytes": r.rotated_bytes,
            "files_removed": r.files_removed, "bytes_freed": r.bytes_freed,
            "took_ms": r.took_ms,
        })),
    }))
}

/// Typed `Router<AppState>` to match the app router it merges into — the
/// handler itself needs no state, but a stateless `Router<()>` will not merge.
pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/api/debug/storage", axum::routing::get(debug_storage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn cutoff_is_expressed_in_each_tables_own_unit() {
        let now = 1_786_381_783.0;
        let secs = cutoff_for(TsUnit::Secs, now, 1.0);
        let ms = cutoff_for(TsUnit::Millis, now, 1.0);
        // The whole module exists because these two differ by 1000x. If a
        // refactor ever makes them equal, every ms table empties on tick one.
        let s: f64 = secs.parse().unwrap();
        let m: f64 = ms.parse().unwrap();
        assert!((m / s - 1000.0).abs() < 1.0, "millis cutoff must be 1000x the seconds cutoff");
        assert!(cutoff_for(TsUnit::IsoText, now, 1.0).starts_with("2026-08-"));
    }

    #[test]
    fn iso_formatter_matches_the_writers_shape() {
        // Cross-checked against `date -u -r 1786400563` rather than written
        // from memory — the first version of this assertion was a guess and it
        // was a day and four hours out, which is how a "failing" test gets
        // "fixed" by loosening it.
        assert_eq!(iso_utc(1_786_400_563), "2026-08-10T22:22:43.000000+00:00");
    }

    /// GUARD 1, in the direction that actually destroys data: a SECONDS table
    /// mis-declared as `Millis` builds a cutoff 1000x too large, so every row
    /// looks ancient and the sweep would empty the table.
    #[test]
    fn a_cutoff_that_would_empty_the_table_is_refused() {
        let c = mem();
        c.execute_batch("CREATE TABLE session_events (id INTEGER PRIMARY KEY, ts REAL)").unwrap();
        let now = unix_now();
        for i in 0..10 {
            c.execute(
                "INSERT INTO session_events (ts) VALUES (?1)",
                rusqlite::params![now - i as f64 * 60.0],
            )
            .unwrap();
        }
        let wrong = SweepSpec {
            table: "session_events",
            ts_col: "ts",
            unit: TsUnit::Millis, // WRONG: this table is seconds.
            env: "AMUX_TEST_NOPE",
            default_days: 1.0,
        };
        let r = sweep_one(&c, &wrong, now);
        assert!(matches!(r, SweepResult::Refused { total: 10, .. }), "got {r:?}");
        let left: i64 =
            c.query_row("SELECT COUNT(*) FROM session_events", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 10, "a refusal must not delete a single row");

        // Correctly declared, the same data keeps everything (rows are minutes
        // old, retention is a day) — which proves the refusal above came from
        // the unit and not from the data.
        let right = SweepSpec { unit: TsUnit::Secs, ..wrong };
        assert!(matches!(sweep_one(&c, &right, now), SweepResult::Deleted { rows: 0, kept: 10 }));
    }

    /// The OTHER direction, which is silent: a MILLISECOND table mis-declared
    /// as `Secs` builds a cutoff 1000x too small, deletes nothing ever, and
    /// leaves a table growing behind a knob everyone believes bounds it.
    #[test]
    fn a_cutoff_that_can_never_fire_deletes_nothing_and_is_not_refused() {
        let c = mem();
        c.execute_batch("CREATE TABLE interaction_log (id INTEGER PRIMARY KEY, ts INTEGER)")
            .unwrap();
        let now = unix_now();
        // A live table's real shape: old rows AND recent ones. (A table where
        // every row is older than retention trips guard 1 instead — see
        // `guard_1_also_refuses_a_legitimately_dormant_table`.)
        for d in [400.0, 300.0, 1.0] {
            c.execute(
                "INSERT INTO interaction_log (ts) VALUES (?1)",
                rusqlite::params![((now - d * 86_400.0) * 1000.0) as i64],
            )
            .unwrap();
        }
        let wrong = SweepSpec {
            table: "interaction_log",
            ts_col: "ts",
            unit: TsUnit::Secs, // WRONG: this table is milliseconds.
            env: "AMUX_TEST_NOPE2",
            default_days: 90.0,
        };
        // Safe, so not a refusal — but nothing is deleted despite every row
        // being 200-400 days old. Guard 2 logs the warning; the observable
        // contract is that data is untouched.
        assert_eq!(sweep_one(&c, &wrong, now), SweepResult::Deleted { rows: 0, kept: 3 });
        // The tell guard 2 keys on, and it is NOT "very old": read as seconds,
        // a millisecond timestamp is dated tens of thousands of years in the
        // FUTURE, so the age is negative.
        let age_days = oldest_secs(&c, &wrong).map(|s| (now - s) / 86_400.0).unwrap();
        assert!(age_days < 0.0, "ms-as-secs must read as a future date, got {age_days} days");

        // Declared correctly, the same rows sweep as intended.
        let right = SweepSpec { unit: TsUnit::Millis, ..wrong };
        assert_eq!(sweep_one(&c, &right, now), SweepResult::Deleted { rows: 2, kept: 1 });
    }

    #[test]
    fn old_rows_go_and_new_rows_stay() {
        let c = mem();
        c.execute_batch("CREATE TABLE session_events (id INTEGER PRIMARY KEY, ts REAL)").unwrap();
        let now = unix_now();
        for d in [200.0, 150.0, 100.0, 5.0, 1.0] {
            c.execute(
                "INSERT INTO session_events (ts) VALUES (?1)",
                rusqlite::params![now - d * 86_400.0],
            )
            .unwrap();
        }
        let spec = SweepSpec {
            table: "session_events",
            ts_col: "ts",
            unit: TsUnit::Secs,
            env: "AMUX_TEST_UNSET_SESSION_EVENTS",
            default_days: 90.0,
        };
        let r = sweep_one(&c, &spec, now);
        assert_eq!(
            r,
            SweepResult::Deleted { rows: 3, kept: 2 },
            "at 90d retention the 200/150/100-day rows go and 5d/1d stay"
        );
    }

    /// The known false positive, asserted so it is a documented property
    /// rather than a surprise. A table whose rows are ALL older than its
    /// retention (a subsystem that stopped being written months ago) is
    /// indistinguishable from a unit bug by looking at rows, so guard 1
    /// refuses and logs. That is the safe side of the trade: the cost is that
    /// a dormant table does not shrink, and a dormant table is not growing.
    /// The error names the table, so a human can lower the knob deliberately.
    #[test]
    fn guard_1_also_refuses_a_legitimately_dormant_table() {
        let c = mem();
        c.execute_batch("CREATE TABLE schedule_runs (id INTEGER PRIMARY KEY, ran_at INTEGER)")
            .unwrap();
        let now = unix_now();
        for d in [400.0, 300.0, 200.0] {
            c.execute(
                "INSERT INTO schedule_runs (ran_at) VALUES (?1)",
                rusqlite::params![(now - d * 86_400.0) as i64],
            )
            .unwrap();
        }
        let spec = SweepSpec {
            table: "schedule_runs",
            ts_col: "ran_at",
            unit: TsUnit::Secs, // CORRECT unit; the data really is all old.
            env: "AMUX_TEST_DORMANT",
            default_days: 90.0,
        };
        assert!(matches!(sweep_one(&c, &spec, now), SweepResult::Refused { total: 3, .. }));
        let left: i64 = c.query_row("SELECT COUNT(*) FROM schedule_runs", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 3, "refusing is safe: it never deletes");
    }

    #[test]
    fn disabling_a_table_with_zero_days_sweeps_nothing() {
        let c = mem();
        c.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, ts REAL)").unwrap();
        std::env::set_var("AMUX_TEST_DISABLED_KNOB", "0");
        let spec = SweepSpec {
            table: "t",
            ts_col: "ts",
            unit: TsUnit::Secs,
            env: "AMUX_TEST_DISABLED_KNOB",
            default_days: 30.0,
        };
        assert_eq!(sweep_one(&c, &spec, unix_now()), SweepResult::Disabled);
        std::env::remove_var("AMUX_TEST_DISABLED_KNOB");
    }

    #[test]
    fn rotation_truncates_in_place_and_keeps_the_fd_valid() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path();
        let log = logs.join("server-rs.log");
        std::env::set_var("AMUX_SERVER_LOG_MAX_MB", "1");
        std::fs::write(&log, vec![b'x'; 2 * 1024 * 1024]).unwrap();

        // A writer holding the fd, exactly like the tracing appender.
        use std::io::Write;
        let mut fd = std::fs::OpenOptions::new().append(true).open(&log).unwrap();

        let rolled = rotate_server_log(logs).expect("should rotate");
        assert_eq!(rolled, 2 * 1024 * 1024);
        assert_eq!(std::fs::metadata(logs.join("server-rs.log.1")).unwrap().len(), 2 * 1024 * 1024);
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0, "must truncate in place");

        // The held fd must still write to the SAME path — this is what a
        // rename would have broken, silently and permanently.
        fd.write_all(b"after\n").unwrap();
        assert!(std::fs::read_to_string(&log).unwrap().contains("after"));
        std::env::remove_var("AMUX_SERVER_LOG_MAX_MB");
    }

    #[test]
    fn prune_removes_only_files_older_than_the_age() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fresh.mp4"), b"12345").unwrap();
        let (n, _) = prune_dir_by_age(dir.path(), 86_400, "AMUX_TEST");
        assert_eq!(n, 0, "a file created just now is not stale");
        let (n, _) = prune_dir_by_age(dir.path(), 0, "AMUX_TEST");
        assert_eq!(n, 0, "0 disables the sweep entirely rather than deleting everything");
        assert!(dir.path().join("fresh.mp4").exists());
    }
}
