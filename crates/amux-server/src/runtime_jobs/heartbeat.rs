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

/// WHY amux was absent for a [`Gap`].
///
/// 0021 named the outage; 0022 proved it was a real absence and not a stopped
/// heartbeat. Neither could say what killed it, and the two causes want
/// opposite fixes: a host that went down is a UPS/power/panic problem, while a
/// host that stayed up while amux vanished is the **gui/501 signature** — every
/// amux LaunchAgent's lifetime is the GUI LOGIN SESSION, so anything that kills
/// that session kills amux no matter what `KeepAlive` says.
///
/// The discriminator is one sysctl: this boot's kernel boot instant either
/// falls inside the gap or it does not. It was available the whole time.
/// Establishing it by hand for the 2026-08-22 outage meant reading a 6 MB
/// WindowServer `.ips` and an 11 MB spin report to find a watchdog kill during
/// a Screen-Recording TCC preflight (DESKT-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DowntimeCause {
    /// Kernel boot instant is INSIDE the gap: the HOST restarted.
    Machine,
    /// Kernel boot instant PREDATES the gap: the host stayed up and only amux
    /// went away. Process death, a kill, or the login session dying.
    ProcessOnly,
}

impl DowntimeCause {
    pub fn as_str(self) -> &'static str {
        match self {
            DowntimeCause::Machine => "machine",
            DowntimeCause::ProcessOnly => "process-only",
        }
    }

    fn from_str(v: &str) -> Option<Self> {
        match v {
            "machine" => Some(DowntimeCause::Machine),
            "process-only" => Some(DowntimeCause::ProcessOnly),
            _ => None,
        }
    }
}

/// Classify a gap from the kernel's boot instant.
///
/// Split out from [`boot_tx`] and taking plain numbers so the decision itself is
/// testable without a kernel: the shipped path calls exactly this function.
///
/// `None` in, `None` out — a boot time we could not read must not be laundered
/// into a confident `process-only`, which is the claim that would send someone
/// to rebuild the launchd setup over a power cut.
fn classify(down_from: f64, machine_boot_at: Option<f64>) -> Option<DowntimeCause> {
    machine_boot_at.map(|boot| {
        if boot > down_from {
            // The kernel booted after amux's last confirmed beat, so the host
            // went down somewhere inside the gap.
            DowntimeCause::Machine
        } else {
            DowntimeCause::ProcessOnly
        }
    })
}

/// The kernel's boot instant, as unix seconds.
///
/// The `cfg` here is a PLATFORM difference, not a deployment branch: macOS has
/// no `/proc`, Linux has no `KERN_BOOTTIME` sysctl. The same binary ships to
/// both and takes whichever arm its kernel provides, so the single-codebase
/// rule is intact — there is no `if IS_CLOUD` hiding in this.
#[cfg(target_os = "macos")]
fn machine_boot_at() -> Option<f64> {
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut len = std::mem::size_of::<libc::timeval>();
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    // SAFETY: mib is a 2-element CTL_KERN/KERN_BOOTTIME query and the output
    // buffer is exactly the `timeval` the kernel writes for it.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            &mut tv as *mut libc::timeval as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || tv.tv_sec <= 0 {
        return None;
    }
    Some(tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0)
}

/// Linux (the cloud image) reads `btime` from `/proc/stat`, which is the
/// ABSOLUTE boot instant. `/proc/uptime` would need `now - uptime`, and that
/// subtraction drifts with every clock adjustment — the wrong property for a
/// value being compared against a stored wall-clock timestamp.
#[cfg(not(target_os = "macos"))]
fn machine_boot_at() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|b| *b > 0.0)
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
    /// Requests SERVED inside the gap — the discriminator this detector shipped
    /// without (AF-99).
    ///
    /// `Some(0)` the server really was absent. `Some(n>0)` the server was UP and
    /// the HEARTBEAT stopped, which is a different fault wearing the same shape.
    /// `None` could not be measured, so neither claim is made.
    pub requests_during: Option<i64>,
    /// WHY amux was absent — see [`DowntimeCause`]. `None` means the kernel boot
    /// instant could not be read, which is deliberately distinct from
    /// `ProcessOnly`: "we could not look" and "the host never rebooted" send a
    /// reader to completely different subsystems.
    pub cause: Option<DowntimeCause>,
}

impl Gap {
    /// True only when nothing was served. This is the predicate every consumer
    /// must use, because this table exists to be SUBTRACTED from other
    /// durations, and subtracting a stretch the server was actually serving
    /// makes stalls look shorter than they were.
    pub fn is_real_downtime(&self) -> bool {
        self.requests_during == Some(0)
    }
}

/// Requests served strictly inside a candidate gap.
///
/// `None` when the count cannot be taken at all (no request log yet, e.g. a
/// fresh DB or a test fixture) — deliberately distinct from `Some(0)`, because
/// "nothing was served" and "we could not look" support opposite conclusions and
/// collapsing them is the exact shape of the bug this repairs.
fn requests_between(conn: &rusqlite::Connection, from: f64, to: f64) -> Option<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM _amux_request_log WHERE ts > ?1 AND ts < ?2",
        rusqlite::params![from, to],
        |r| r.get::<_, i64>(0),
    )
    .ok()
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
    machine_boot_at: Option<f64>,
) -> rusqlite::Result<Option<Gap>> {
    let prev = stamp(conn, now)?;
    let gap = match prev {
        Some(p) if now - p > min_gap => Some(Gap {
            down_from: p,
            up_at: now,
            seconds: now - p,
            // Asked BEFORE the row is filed, in the same transaction, so the row
            // can never exist without the fact that qualifies it (AF-99).
            requests_during: requests_between(conn, p, now),
            // Same discipline for the cause: classified here, stored with the
            // row, so the row can never exist without saying why (DESKT-22).
            cause: classify(p, machine_boot_at),
        }),
        _ => None,
    };
    if let Some(g) = gap {
        // Filed either way. A heartbeat that stopped while the server ran is a
        // real fault too, and burying it would repeat this module's founding
        // mistake — an absence is not a signal. The column is what keeps it from
        // being SUBTRACTED as downtime.
        conn.execute(
            "INSERT INTO server_downtime (down_from, up_at, seconds, port, requests_during, cause) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                g.down_from,
                g.up_at,
                g.seconds,
                port as i64,
                g.requests_during,
                g.cause.map(|c| c.as_str()),
            ],
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
    // Read ONCE, out here, and pass it in: the write closure runs on the writer
    // thread under BEGIN IMMEDIATE, and a syscall inside that critical section
    // would hold the cross-process lock for no reason.
    let booted_at = machine_boot_at();
    let res = store.write(move |conn| {
        boot_tx(conn, now, min_gap, port, booted_at)?;
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
                "SELECT down_from, up_at, seconds, requests_during, cause FROM server_downtime \
                 WHERE up_at = ?1 ORDER BY id DESC LIMIT 1",
                [now],
                |r| {
                    Ok(Gap {
                        down_from: r.get(0)?,
                        up_at: r.get(1)?,
                        seconds: r.get(2)?,
                        requests_during: r.get(3)?,
                        cause: r
                            .get::<_, Option<String>>(4)?
                            .as_deref()
                            .and_then(DowntimeCause::from_str),
                    })
                },
            )
            .ok()
        });

    // Only a gap with NOTHING served is downtime, and only downtime reaches
    // `/health`'s `downtime_before_boot_s`. A heartbeat gap published there would
    // be subtracted by every consumer that polls it (AF-99).
    let _ = BOOT_GAP.set(gap.filter(|g| g.is_real_downtime()));

    match gap {
        Some(g) if g.is_real_downtime() => tracing::warn!(
            down_from = %crate::api::request_log::local_when(g.down_from),
            up_at = %crate::api::request_log::local_when(g.up_at),
            seconds = g.seconds,
            minutes = (g.seconds / 60.0).round(),
            cause = g.cause.map(|c| c.as_str()).unwrap_or("unknown"),
            // The cause is spelled out in prose as well as in the field, because
            // the field is only useful to someone who already knows the
            // vocabulary. Whoever reads this line is usually reading it for the
            // first time, at 2am, wondering why the fleet stopped.
            explain = %match g.cause {
                Some(DowntimeCause::Machine) =>
                    "the HOST restarted inside this window (kernel boot instant falls in \
                     the gap) — look at power, UPS, panics and thermals, not at amux",
                Some(DowntimeCause::ProcessOnly) =>
                    "the host STAYED UP and only amux went away (kernel boot predates the \
                     gap). amux's launchd jobs are LaunchAgents in gui/501, whose lifetime \
                     is the GUI LOGIN SESSION — a WindowServer crash, a logout or a session \
                     kill takes them all down regardless of KeepAlive (DESKT-22)",
                None =>
                    "cause NOT classified — the kernel boot instant could not be read, so \
                     host-restart and process-death cannot be told apart for this row",
            },
            "amux was DOWN — no server was running for this stretch. Durations reported \
             across it (steer queue age, rot timers, missed schedules) include time with \
             no amux, not just a stalled lane (AEAB-29)"
        ),
        // The fault this detector could not previously name. Loud, because a
        // liveness probe that stopped while the thing it watches kept running is
        // worse than one that never ran: everything downstream reads healthy.
        Some(g) if g.requests_during.unwrap_or(0) > 0 => tracing::warn!(
            down_from = %crate::api::request_log::local_when(g.down_from),
            up_at = %crate::api::request_log::local_when(g.up_at),
            seconds = g.seconds,
            requests_during = g.requests_during,
            "HEARTBEAT GAP, NOT DOWNTIME — the server served requests throughout this \
             stretch, so the beat loop stopped while amux kept running. Recorded, but NOT \
             counted as downtime and NOT published on /health (AF-99)"
        ),
        Some(g) => tracing::warn!(
            down_from = %crate::api::request_log::local_when(g.down_from),
            up_at = %crate::api::request_log::local_when(g.up_at),
            seconds = g.seconds,
            "heartbeat gap recorded but UNVERIFIED — the request log could not be counted, \
             so whether amux was absent or merely not beating is unknown. Not counted as \
             downtime (AF-99)"
        ),
        None => {}
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
    // `requests_during = 0` strictly: NULL (unmeasured) and > 0 (heartbeat gap)
    // are both excluded. This is the SUBTRACTED number, so the safe direction is
    // to under-subtract — failing to discount a real outage overstates a lane's
    // stall, while discounting a stretch the server was serving hides one (AF-99).
    let mut stmt = match conn.prepare(
        "SELECT down_from, up_at FROM server_downtime \
         WHERE up_at >= ?1 AND down_from <= ?2 AND requests_during = 0",
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
        // Through the migration RUNNER, not execute_batch: `-- ADDCOL:` lines are
        // SQL comments, so a batch-applied fixture silently lacks every column
        // added that way — including `requests_during`, the one AF-99 turns on.
        for sql in [
            include_str!("../../migrations/0010_request_log.sql"),
            include_str!("../../migrations/0021_heartbeat.sql"),
            include_str!("../../migrations/0022_downtime_requests_during.sql"),
            include_str!("../../migrations/0028_downtime_cause.sql"),
        ] {
            crate::db::migrate::apply_one(&c, sql).unwrap();
        }
        c
    }

    /// Put N request rows inside a window, so a gap can be given real traffic.
    fn serve(c: &Connection, from: f64, to: f64, n: usize) {
        for i in 0..n {
            let ts = from + (to - from) * ((i + 1) as f64 / (n + 1) as f64);
            c.execute(
                "INSERT INTO _amux_request_log (ts, method, path, family, status, latency_ms) \
                 VALUES (?1, 'GET', '/api/board', '/api/board', 200, 1.0)",
                rusqlite::params![ts],
            )
            .unwrap();
        }
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
        let gap = boot_tx(&c, 1_000.0, 120.0, 8824, None).unwrap();
        assert!(gap.is_none(), "an empty heartbeat table means no history, not a four-hour outage");
        assert!(outages(&c).is_empty());
    }

    /// The other NEGATIVE half, and the one that matters in production: this
    /// server re-execs to adopt new builds several times a day. If a deploy
    /// filed an outage, the signal would be pure noise within a week.
    #[test]
    fn a_quick_restart_is_not_an_outage() {
        let c = db();
        boot_tx(&c, 1_000.0, 120.0, 8824, None).unwrap();
        // 5s later: a self-adopt restart, well inside the 120s floor.
        let gap = boot_tx(&c, 1_005.0, 120.0, 8824, None).unwrap();
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
        let gap = boot_tx(&c, back_up, 120.0, 8824, None).unwrap().expect("4h26m must register");
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
        assert!(boot_tx(&c, 120.0, 120.0, 1, None).unwrap().is_none(), "exactly at the floor is not an outage");
        stamp(&c, 0.0).unwrap();
        assert!(boot_tx(&c, 120.5, 120.0, 1, None).unwrap().is_some(), "past the floor is");
    }

    /// The two-process dedupe. 8823 and 8824 share one DB and boot together;
    /// the writer thread's BEGIN IMMEDIATE serialises these two calls, so the
    /// second must see the FRESH stamp the first just wrote and stay quiet.
    /// Without this the fleet would file every outage twice.
    #[test]
    fn only_the_process_that_wins_the_race_files_the_outage() {
        let c = db();
        stamp(&c, 0.0).unwrap();
        let first = boot_tx(&c, 10_000.0, 120.0, 8824, None).unwrap();
        let second = boot_tx(&c, 10_000.1, 120.0, 8823, None).unwrap();
        assert!(first.is_some(), "the first process through sees the stale stamp");
        assert!(second.is_none(), "the second sees a stamp 0.1s old — no second row");
        assert_eq!(outages(&c).len(), 1);
    }

    /// A beat must keep the stamp warm, or a long-running server would report
    /// its own uptime as an outage on the next restart.
    #[test]
    fn beats_keep_the_stamp_warm_so_uptime_is_never_read_as_downtime() {
        let c = db();
        boot_tx(&c, 0.0, 120.0, 1, None).unwrap();
        for t in 1..=400 {
            stamp(&c, t as f64 * 15.0).unwrap();   // 100 minutes of 15s beats
        }
        let gap = boot_tx(&c, 400.0 * 15.0 + 5.0, 120.0, 1, None).unwrap();
        assert!(gap.is_none(), "a served 100 minutes must not be filed as a 100-minute outage");
        assert!(outages(&c).is_empty());
    }

    #[test]
    fn downtime_within_reports_only_the_overlap() {
        let c = db();
        stamp(&c, 0.0).unwrap();
        boot_tx(&c, 1_000.0, 120.0, 1, None).unwrap();   // outage [0, 1000]

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

    // ---- AF-99: a gap is only downtime if nothing was served ----------------
    //
    // Built from the incident's own numbers: /api/debug/downtime reported
    // 2026-08-19 09:04:20 -> 21:43:38 (759.3 min) as "no amux server running"
    // while _amux_request_log held 33,455 rows spanning 09:04:25 -> 21:43:27.

    #[test]
    fn a_gap_with_traffic_inside_it_is_a_heartbeat_gap_not_downtime() {
        let c = db();
        let (down_from, up_at) = (1_787_144_660.0, 1_787_190_218.0); // the real bracket
        c.execute("INSERT INTO server_heartbeat (id, beat_at) VALUES (1, ?1)", [down_from])
            .unwrap();
        serve(&c, down_from, up_at, 40);

        let gap = boot_tx(&c, up_at, 120.0, 8824, None).unwrap().expect("a gap is still recorded");
        assert_eq!(gap.requests_during, Some(40));
        assert!(
            !gap.is_real_downtime(),
            "the server served 40 requests inside this stretch — calling it downtime is \
             the bug, and downtime_within() would SUBTRACT it from every lane's stall"
        );
        assert_eq!(outages(&c).len(), 1, "still filed: a stopped heartbeat is its own fault");
        assert_eq!(
            downtime_within(&c, down_from - 1.0, up_at + 1.0),
            0.0,
            "a heartbeat gap must contribute ZERO subtractable downtime"
        );
    }

    #[test]
    fn a_gap_with_no_traffic_is_real_downtime_and_is_subtracted() {
        let c = db();
        let (down_from, up_at) = (1_000.0, 20_000.0);
        c.execute("INSERT INTO server_heartbeat (id, beat_at) VALUES (1, ?1)", [down_from])
            .unwrap();
        // deliberately NO serve() — and rows OUTSIDE the window must not count
        serve(&c, 100.0, 900.0, 5);
        serve(&c, 20_100.0, 20_900.0, 5);

        let gap = boot_tx(&c, up_at, 120.0, 8824, None).unwrap().expect("gap");
        assert_eq!(gap.requests_during, Some(0), "traffic outside the bracket is not inside it");
        assert!(gap.is_real_downtime());
        assert_eq!(downtime_within(&c, down_from, up_at), 19_000.0);
    }

    /// The direction that matters: this is the SUBTRACTED number, so an
    /// unverifiable row must not be subtracted. Under-subtracting overstates a
    /// lane's stall; over-subtracting HIDES one.
    #[test]
    fn an_unverified_row_is_never_subtracted() {
        let c = db();
        c.execute(
            "INSERT INTO server_downtime (down_from, up_at, seconds, port, requests_during) \
             VALUES (1000.0, 20000.0, 19000.0, 8824, NULL)",
            [],
        )
        .unwrap();
        assert_eq!(downtime_within(&c, 1_000.0, 20_000.0), 0.0);
    }

    /// The migration must repair the row that is already wrong in production,
    /// not just qualify future ones.
    #[test]
    fn the_backfill_corrects_a_row_filed_before_the_column_existed() {
        let c = db();
        let (down_from, up_at) = (1_787_144_660.0, 1_787_190_218.0);
        c.execute(
            "INSERT INTO server_downtime (down_from, up_at, seconds, port) \
             VALUES (?1, ?2, ?3, 8824)",
            rusqlite::params![down_from, up_at, up_at - down_from],
        )
        .unwrap();
        serve(&c, down_from, up_at, 33);
        assert_eq!(
            downtime_within(&c, down_from, up_at),
            0.0,
            "NULL is not subtracted even before the backfill"
        );

        crate::db::migrate::apply_one(
            &c,
            include_str!("../../migrations/0023_downtime_backfill.sql"),
        )
        .unwrap();

        let served: Option<i64> = c
            .query_row("SELECT requests_during FROM server_downtime", [], |r| r.get(0))
            .unwrap();
        assert_eq!(served, Some(33), "the backfill must count the traffic that was served");
        assert_eq!(downtime_within(&c, down_from, up_at), 0.0);
    }


    // ---- DESKT-22: WHY was amux absent? ----

    /// The NEGATIVE half, and the one that matters most: an unreadable boot time
    /// must NOT be laundered into a confident `process-only`. That claim sends
    /// someone to rebuild the launchd setup over what may have been a power cut.
    #[test]
    fn an_unreadable_boot_time_classifies_as_nothing() {
        assert_eq!(classify(1_000.0, None), None);
    }

    /// Rebuilt from the incident this column was added for (DESKT-20):
    /// the host booted 2026-08-19 20:37 and was still up on 2026-08-22 when
    /// WindowServer was watchdog-killed mid Screen-Recording TCC preflight,
    /// taking the GUI login session and every gui/501 agent with it.
    /// amux was absent 09:32:24 -> 09:34:31, 126.88s.
    #[test]
    fn the_2026_08_22_windowserver_kill_reads_as_process_only() {
        let boot = 1_787_186_259.0; // Aug 19 20:37:39, from kern.boottime
        let down_from = 1_787_405_544.0; // Aug 22 09:32:24
        let up_at = down_from + 126.88;

        assert_eq!(
            classify(down_from, Some(boot)),
            Some(DowntimeCause::ProcessOnly),
            "the host never rebooted — the login session died, which is the gui/501 signature"
        );

        // And it must survive the round trip through the row, because the WARN
        // is read live while the ROW is what anyone asks weeks later.
        let c = db();
        stamp(&c, down_from).unwrap();
        let gap = boot_tx(&c, up_at, 120.0, 8824, Some(boot)).unwrap().expect("126.88s is an outage");
        assert_eq!(gap.cause, Some(DowntimeCause::ProcessOnly));
        let stored: Option<String> = c
            .query_row("SELECT cause FROM server_downtime ORDER BY id DESC LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored.as_deref(), Some("process-only"));
    }

    /// The other real incident, from 0021's own header: 2026-08-18, a hardware
    /// undervoltage fault killed the machine at 14:03, it came back at 15:18,
    /// and amux stayed absent until a human logged in at the console at 18:28.
    ///
    /// The cause is `machine` even though MOST of the 4h26m was the gui/501
    /// delay — the host is what went down, and the login-session dependency is
    /// what made the outage four times longer than the hardware fault. Both
    /// facts are true and the column reports the first one; conflating them is
    /// what this test pins.
    #[test]
    fn the_2026_08_18_hardware_fault_reads_as_machine() {
        let down_from = 1_000_000.0; // 14:03, last confirmed beat
        let machine_back = down_from + 75.0 * 60.0; // 15:18, kernel boots
        let up_at = down_from + (4.0 * 3600.0 + 26.0 * 60.0); // 18:29, console login

        assert_eq!(
            classify(down_from, Some(machine_back)),
            Some(DowntimeCause::Machine),
            "the kernel boot instant falls INSIDE the gap, so the host restarted"
        );

        let c = db();
        stamp(&c, down_from).unwrap();
        let gap = boot_tx(&c, up_at, 120.0, 8824, Some(machine_back)).unwrap().expect("4h26m");
        assert_eq!(gap.cause, Some(DowntimeCause::Machine));
    }

    /// The boundary, stated explicitly so a later refactor cannot slide it:
    /// a kernel that booted at exactly the last confirmed beat did not reboot
    /// during the gap.
    #[test]
    fn the_boundary_between_machine_and_process_only_is_the_last_beat() {
        assert_eq!(classify(1_000.0, Some(1_000.0)), Some(DowntimeCause::ProcessOnly));
        assert_eq!(classify(1_000.0, Some(1_000.1)), Some(DowntimeCause::Machine));
    }

    /// The two causes must not collapse to the same string — the whole point is
    /// that they route a reader to different subsystems.
    #[test]
    fn the_causes_are_distinguishable_on_the_wire() {
        assert_ne!(DowntimeCause::Machine.as_str(), DowntimeCause::ProcessOnly.as_str());
        for c in [DowntimeCause::Machine, DowntimeCause::ProcessOnly] {
            assert_eq!(DowntimeCause::from_str(c.as_str()), Some(c), "round trip");
        }
        assert_eq!(DowntimeCause::from_str("wat"), None);
    }

    /// This machine must be able to answer the question at all. A classifier
    /// whose only input is unreadable in production is theatre — and the
    /// platform read is exactly the kind of thing that compiles everywhere and
    /// returns None on the host you actually ship to.
    #[test]
    fn the_kernel_boot_instant_is_readable_on_this_host() {
        let b = machine_boot_at().expect("kern.boottime / proc btime must be readable");
        let now = crate::runtime_jobs::registry::unix_now();
        assert!(b > 0.0 && b < now, "boot instant {b} must be a past unix time (now {now})");
        // Sanity: nobody's uptime is 20 years. Catches a units mix-up, which is
        // the failure mode that would silently make every gap read as `machine`.
        assert!(now - b < 20.0 * 365.0 * 86_400.0, "implausible uptime — units wrong?");
    }
}
