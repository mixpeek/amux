//! Server liveness heartbeat, and the downtime it makes visible (AEAB-29).
//!
//! # Why this exists
//!
//! On 2026-08-18 amux was down for 4h26m. A hardware undervoltage fault killed
//! the machine at 14:03 EDT; it came back at 15:18, but amux's LaunchAgents are
//! `Aqua` session-type, so nothing started until a human logged in at the
//! console at 18:28. The user found out the way everyone finds out — their
//! prompts stopped working — and asked what was wrong with the server.
//!
//! The product recorded the outage NOWHERE. The only evidence was a gap between
//! two lines in `server-rs.log`, and **an absence is not a signal**: nothing
//! counted it, nothing named it, and the restart emitted the same
//! `starting amux-rust` it emits after a routine 5-second auto-adopt reload.
//! Byte-identical framing for a 5s blip and a four-hour outage.
//!
//! That is ethos rule 4 — when this goes wrong, what will someone see, and will
//! they see it WHERE THEY ALREADY LOOK? So the gap is now named at WARN, stored
//! so it can be asked for after the fact (which is the only time anyone asks),
//! and published on `/health`, the endpoint every consumer already polls.
//!
//! # Why it matters beyond "amux was down"
//!
//! Every duration amux reports is measured against wall-clock time that may
//! include hours the server did not exist, and nothing marks which. On the day
//! this was written a steering message showed `queued=1 oldest_min=2131
//! reason=not-running` — some of those minutes were a genuinely wedged lane and
//! some were minutes with no server at all. Those are different faults with
//! different fixes and the log could not tell them apart. Same for schedules
//! that did not fire and rot timers that aged. [`downtime_within`] exists so a
//! reader can subtract the part that was not the lane's fault.
//!
//! # The two-process race is the dedupe
//!
//! Two amux-server processes share one DB (8823 and 8824), so a naive
//! "read stamp, then write stamp" would have both report the same outage. The
//! read and the write happen inside ONE [`crate::db::Store::write`] closure,
//! which the writer thread runs under `BEGIN IMMEDIATE` — atomic across
//! processes. Whichever process gets there first sees the stale stamp and files
//! the gap; the second sees a fresh one and says nothing. One outage, one row.

use crate::db::{Store, WriteOutcome};
use std::sync::OnceLock;
use std::time::Duration;

/// The job's registry id, and — because `spawn_periodic` derives the per-job
/// isolation switch from the name — the source of `AMUX_HEARTBEAT_SECS`.
pub const JOB: &str = crate::runtime_jobs::registry::ids::HEARTBEAT;

/// How often the live server stamps the heartbeat row.
pub fn beat_interval() -> Duration {
    Duration::from_secs(
        std::env::var("AMUX_HEARTBEAT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(15)
            .clamp(1, 600),
    )
}

/// The gap that counts as an OUTAGE rather than a restart.
///
/// This server re-execs to adopt a new build, and that costs a few seconds; the
/// heartbeat interval adds up to another. The default is deliberately far above
/// both, because the expensive error here is crying outage on every deploy — a
/// detector that fires on the healthy baseline is reporting that the machine is
/// ON (ethos rule 7). It is NOT tuned to catch the smallest possible gap.
pub fn min_gap_s() -> f64 {
    std::env::var("AMUX_DOWNTIME_MIN_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(120.0)
        .max(1.0)
}

/// A stretch with no live amux, bracketed by the last confirmed beat and this boot.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Gap {
    /// Last moment the server is KNOWN to have been alive. The true stop is
    /// somewhere in `(down_from, down_from + beat_interval]` — naming the last
    /// confirmed beat instead of a guess keeps the number honest.
    pub down_from: f64,
    pub up_at: f64,
    pub seconds: f64,
}

static BOOT_GAP: OnceLock<Option<Gap>> = OnceLock::new();

/// The outage that preceded THIS boot, if there was one. `None` also means
/// "not measured yet" on a process that never called [`record_boot`], which is
/// why `/health` reports it as an absent field rather than a zero.
pub fn boot_gap() -> Option<Gap> {
    BOOT_GAP.get().copied().flatten()
}

/// SQL body shared by boot and tick so the two cannot drift: read the previous
/// stamp, write the current one, and hand back what was there before.
fn stamp(conn: &rusqlite::Connection, now: f64) -> rusqlite::Result<Option<f64>> {
    let prev: Option<f64> = conn
        .query_row("SELECT beat_at FROM server_heartbeat WHERE id = 1", [], |r| r.get(0))
        .ok();
    conn.execute(
        "INSERT INTO server_heartbeat (id, beat_at) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET beat_at = excluded.beat_at",
        [now],
    )?;
    Ok(prev)
}

/// The whole boot decision, as one function against one connection, so the
/// tests exercise the SHIPPED path rather than a paraphrase of it.
///
/// The read and the write are deliberately in the same call: [`record_boot`]
/// runs this inside `Store::write`, whose writer thread wraps it in
/// `BEGIN IMMEDIATE`, which is what makes it atomic across the two
/// amux-server processes that share this database. Split them and both
/// processes would file the same outage.
fn boot_tx(
    conn: &rusqlite::Connection,
    now: f64,
    min_gap: f64,
    port: u16,
) -> rusqlite::Result<Option<Gap>> {
    let prev = stamp(conn, now)?;
    let gap = match prev {
        Some(p) if now - p > min_gap => Some(Gap { down_from: p, up_at: now, seconds: now - p }),
        _ => None,
    };
    if let Some(g) = gap {
        conn.execute(
            "INSERT INTO server_downtime (down_from, up_at, seconds, port) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![g.down_from, g.up_at, g.seconds, port as i64],
        )?;
    }
    Ok(gap)
}

/// Called once at startup, BEFORE the beat loop. Records this boot's stamp and,
/// if the previous one is older than [`min_gap_s`], files and logs the outage.
///
/// Returns the gap so the caller can log it with the rest of the startup
/// banner; it is also cached for `/health`.
pub fn record_boot(store: &Store, port: u16) -> Option<Gap> {
    // An isolated/test server must NOT stamp the fleet's row: a warm stamp from
    // a process that is not the fleet would mask a real outage, which is the
    // one thing this module exists to make visible. The predicate is BORROWED
    // from the spawn path rather than re-derived here — a view that re-invents
    // the filter of the mechanism it describes is how the two drift.
    if let Some(reason) = crate::runtime_jobs::fleet_isolation_reason(JOB) {
        tracing::info!(
            switch = %reason,
            "heartbeat: not stamping — this server is isolated from the fleet, and a \
             stamp from a non-fleet process would hide the next real outage"
        );
        let _ = BOOT_GAP.set(None);
        return None;
    }
    let now = crate::runtime_jobs::registry::unix_now();
    let min_gap = min_gap_s();
    let res = store.write(move |conn| {
        boot_tx(conn, now, min_gap, port)?;
        // `applied: false` is LOAD-BEARING, not laziness. `applied` bumps the
        // global revision, and the global revision is what SSE delta-sync hangs
        // off — so a heartbeat that "applied" would make every connected
        // dashboard refetch the world every 15 seconds, forever. That is the
        // ethos rule 7 trap where the detector's cost is paid in the same
        // resource as the fault it watches. The row still commits; `applied`
        // only controls whether subscribers are told something changed, and
        // nothing user-visible did.
        Ok(WriteOutcome { applied: false, events: vec![] })
    });

    if let Err(e) = res {
        // A heartbeat that cannot be written must not be mistaken for a server
        // that was never down. Say so loudly and leave BOOT_GAP unset.
        tracing::warn!(error = %e, "heartbeat: could not stamp boot — downtime detection is OFF for this run");
        return None;
    }

    // Re-derive the gap from the row we just displaced. The write closure cannot
    // return a value, so this reads `server_downtime` for the row keyed to this
    // boot — which also proves the insert actually landed, rather than trusting
    // that it did (ethos rule 6: verify the operand you just wrote).
    let gap = store
        .read()
        .ok()
        .and_then(|c| {
            c.query_row(
                "SELECT down_from, up_at, seconds FROM server_downtime \
                 WHERE up_at = ?1 ORDER BY id DESC LIMIT 1",
                [now],
                |r| Ok(Gap { down_from: r.get(0)?, up_at: r.get(1)?, seconds: r.get(2)? }),
            )
            .ok()
        });

    let _ = BOOT_GAP.set(gap);

    if let Some(g) = gap {
        tracing::warn!(
            down_from = %crate::api::request_log::local_when(g.down_from),
            up_at = %crate::api::request_log::local_when(g.up_at),
            seconds = g.seconds,
            minutes = (g.seconds / 60.0).round(),
            "amux was DOWN — no server was running for this stretch. Durations reported \
             across it (steer queue age, rot timers, missed schedules) include time with \
             no amux, not just a stalled lane (AEAB-29)"
        );
    }
    gap
}

/// One tick of the beat loop.
pub fn beat(store: &Store) {
    let now = crate::runtime_jobs::registry::unix_now();
    if let Err(e) = store.write(move |conn| {
        stamp(conn, now)?;
        // See record_boot: never bump the global revision from a heartbeat.
        Ok(WriteOutcome { applied: false, events: vec![] })
    }) {
        tracing::warn!(error = %e, "heartbeat: stamp failed");
    }
}

/// Seconds of KNOWN downtime inside `[from, to]`, for readers that want to say
/// how much of a long duration was the server's absence rather than a lane's.
pub fn downtime_within(conn: &rusqlite::Connection, from: f64, to: f64) -> f64 {
    let mut stmt = match conn.prepare(
        "SELECT down_from, up_at FROM server_downtime WHERE up_at >= ?1 AND down_from <= ?2",
    ) {
        Ok(s) => s,
        Err(_) => return 0.0,
    };
    let rows = stmt.query_map([from, to], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?)));
    match rows {
        Ok(it) => it
            .filter_map(|r| r.ok())
            .map(|(a, b)| (b.min(to) - a.max(from)).max(0.0))
            .sum(),
        Err(_) => 0.0,
    }
}


/// Spawns the beat loop. Registered like every other background job, so a
/// heartbeat that stopped ticking is visible in `/api/system-jobs` — a dead
/// liveness probe that nobody can see is the failure this module is about,
/// one level up.
pub fn spawn(store: std::sync::Arc<Store>) -> super::PeriodicTask {
    let secs = beat_interval().as_secs();
    super::spawn_periodic(JOB, secs, move || {
        let store = store.clone();
        async move {
            let _ = tokio::task::spawn_blocking(move || beat(&store)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Builds the schema from the SHIPPED migration file, not from a hand-typed
    /// copy. A test schema that has drifted from the real one proves nothing
    /// about the real one.
    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(include_str!("../../migrations/0021_heartbeat.sql")).unwrap();
        c
    }

    fn outages(c: &Connection) -> Vec<(f64, f64, f64)> {
        let mut st = c
            .prepare("SELECT down_from, up_at, seconds FROM server_downtime ORDER BY id")
            .unwrap();
        let v: Vec<_> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        v
    }

    /// The NEGATIVE half. Without this, every assertion below is satisfied by a
    /// function that files an outage unconditionally.
    #[test]
    fn a_first_ever_boot_is_not_an_outage() {
        let c = db();
        let gap = boot_tx(&c, 1_000.0, 120.0, 8824).unwrap();
        assert!(gap.is_none(), "an empty heartbeat table means no history, not a four-hour outage");
        assert!(outages(&c).is_empty());
    }

    /// The other NEGATIVE half, and the one that matters in production: this
    /// server re-execs to adopt new builds several times a day. If a deploy
    /// filed an outage, the signal would be pure noise within a week.
    #[test]
    fn a_quick_restart_is_not_an_outage() {
        let c = db();
        boot_tx(&c, 1_000.0, 120.0, 8824).unwrap();
        // 5s later: a self-adopt restart, well inside the 120s floor.
        let gap = boot_tx(&c, 1_005.0, 120.0, 8824).unwrap();
        assert!(gap.is_none(), "a 5s restart must not read as downtime");
        assert!(outages(&c).is_empty());
    }

    /// The POSITIVE half, rebuilt from the incident's own numbers rather than
    /// from a conveniently-shaped case: 2026-08-18, last beat 14:03 EDT, server
    /// back 18:29 EDT, 4h26m.
    #[test]
    fn the_2026_08_18_outage_is_named_with_its_real_span() {
        let c = db();
        let last_beat = 1_000_000.0;
        let back_up = last_beat + (4.0 * 3600.0 + 26.0 * 60.0);
        // Seed the heartbeat as the dead server left it.
        stamp(&c, last_beat).unwrap();
        let gap = boot_tx(&c, back_up, 120.0, 8824).unwrap().expect("4h26m must register");
        assert_eq!(gap.down_from, last_beat);
        assert_eq!(gap.up_at, back_up);
        assert!((gap.seconds - 15_960.0).abs() < 0.001, "seconds = {}", gap.seconds);
        let rows = outages(&c);
        assert_eq!(rows.len(), 1, "one outage, one row");
        assert!((rows[0].2 / 60.0 - 266.0).abs() < 0.001, "4h26m = 266 minutes");
    }

    /// The boundary, asserted from BOTH sides — a threshold tested only from
    /// the far side is a threshold whose comparison could be inverted.
    #[test]
    fn the_threshold_discriminates_at_its_own_boundary() {
        let c = db();
        stamp(&c, 0.0).unwrap();
        assert!(boot_tx(&c, 120.0, 120.0, 1).unwrap().is_none(), "exactly at the floor is not an outage");
        stamp(&c, 0.0).unwrap();
        assert!(boot_tx(&c, 120.5, 120.0, 1).unwrap().is_some(), "past the floor is");
    }

    /// The two-process dedupe. 8823 and 8824 share one DB and boot together;
    /// the writer thread's BEGIN IMMEDIATE serialises these two calls, so the
    /// second must see the FRESH stamp the first just wrote and stay quiet.
    /// Without this the fleet would file every outage twice.
    #[test]
    fn only_the_process_that_wins_the_race_files_the_outage() {
        let c = db();
        stamp(&c, 0.0).unwrap();
        let first = boot_tx(&c, 10_000.0, 120.0, 8824).unwrap();
        let second = boot_tx(&c, 10_000.1, 120.0, 8823).unwrap();
        assert!(first.is_some(), "the first process through sees the stale stamp");
        assert!(second.is_none(), "the second sees a stamp 0.1s old — no second row");
        assert_eq!(outages(&c).len(), 1);
    }

    /// A beat must keep the stamp warm, or a long-running server would report
    /// its own uptime as an outage on the next restart.
    #[test]
    fn beats_keep_the_stamp_warm_so_uptime_is_never_read_as_downtime() {
        let c = db();
        boot_tx(&c, 0.0, 120.0, 1).unwrap();
        for t in 1..=400 {
            stamp(&c, t as f64 * 15.0).unwrap();   // 100 minutes of 15s beats
        }
        let gap = boot_tx(&c, 400.0 * 15.0 + 5.0, 120.0, 1).unwrap();
        assert!(gap.is_none(), "a served 100 minutes must not be filed as a 100-minute outage");
        assert!(outages(&c).is_empty());
    }

    #[test]
    fn downtime_within_reports_only_the_overlap() {
        let c = db();
        stamp(&c, 0.0).unwrap();
        boot_tx(&c, 1_000.0, 120.0, 1).unwrap();   // outage [0, 1000]

        // Fully inside the window.
        assert!((downtime_within(&c, 0.0, 2_000.0) - 1_000.0).abs() < 0.001);
        // Half overlapping: only the overlap counts, or a caller subtracting
        // this from a duration would go negative.
        assert!((downtime_within(&c, 500.0, 2_000.0) - 500.0).abs() < 0.001);
        // Disjoint — the negative control. Without it, a function returning the
        // full span unconditionally passes both assertions above.
        assert_eq!(downtime_within(&c, 2_000.0, 3_000.0), 0.0);
    }

    /// The clamps are not decoration: `AMUX_HEARTBEAT_SECS=0` reaches
    /// `spawn_periodic`'s isolation check and stops the job, so if it ALSO
    /// reached `Duration::from_secs(0)` the tick loop would busy-spin.
    #[test]
    fn the_interval_can_never_be_zero() {
        assert!(beat_interval().as_secs() >= 1);
        assert!(min_gap_s() >= 1.0);
    }
}
