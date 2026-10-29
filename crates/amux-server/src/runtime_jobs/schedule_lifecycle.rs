//! Schedule lifecycle: when a schedule starts, when it ends, who may turn it
//! back on, and what happens to it when its worker goes away.
//!
//! # Why (GS-12 review, finding 5, 2026-10-18)
//!
//! A schedule had a start and no end. Measured on one orchestration:
//!
//! - 49 one-off jobs were written as fixed-date yearly crons (`5 0 3 10 *`)
//!   because nothing else could say "at this minute, once". 40 of them now
//!   point at 2027 and five are still enabled.
//! - Seven of those one-offs were refused by the background reserve on their
//!   only fire. The reserve retry covered `sched_type=once` only, so a fixed
//!   date cron advanced a year and the work never happened.
//! - 33 enabled schedules belonged to archived lanes, about 2,200 fires a day
//!   with nobody to read them. Archiving touched cards and never schedules.
//! - The owner disabled SCHED-173 and a worker re-enabled it 70 minutes later;
//!   nothing told the worker the disable was the owner's.
//! - 276 runs in flight at a server restart were recorded as `error`, the same
//!   word as a real failure.
//! - 881 runs failed on expired gcloud USER tokens, each read as its own error.
//!
//! # What this adds (schedules stay one primitive)
//!
//! - Bounds on the row (migration 0125): `not_before` (`after` in the API),
//!   `until`, `max_runs` with `cron_runs` counting scheduled fires. The fire
//!   path ends a schedule the moment a bound is reached and writes
//!   `disabled_reason` plus an audit row, so a disabled row says why.
//! - [`is_fixed_date_cron`]: a cron pinned to one minute of one day of one
//!   month. Classified `yearly`, warned about at create, and treated as a
//!   one-shot by the reserve deferral.
//! - [`pause_for_retired_worker`]: archiving or deleting a worker pauses every
//!   enabled schedule it owns, one WARN line per schedule.
//! - The owner lock (`locked_by`/`lock_reason`/`locked_at`): set when the
//!   owner disables, cleared when the owner enables. A worker's enable or Run
//!   now on a locked schedule is refused with the reason.
//! - [`credential_cause`]: one name for "an expired user credential" so 881
//!   identical failures read as one cause.
//!
//! Every decision logs a stable verdict: `schedule_bound_reached`,
//! `schedule_bound_deferred_start`, `schedule_paused_worker_retired`,
//! `schedule_owner_lock_set`, `schedule_owner_lock_refused`,
//! `schedule_disable_notified`, `schedule_disable_notice_skipped`,
//! `schedule_run_interrupted`, `schedule_run_credential_expired`.

use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, Timelike};
use rusqlite::Connection;

use super::scheduler::{fmt_minute, insert_audit, parse_minute, resolve_local, DurableSchedule, ScheduleExpr};

/// Audit source for a harness-ended schedule.
pub const BOUND_SOURCE: &str = "schedule-bound-rs";
/// `by_who` for harness-made changes (the 10-16 orphan pauses had no row).
pub const HARNESS_BY: &str = "harness:scheduler";

/// Normalise a caller's datetime to the stored local minute format
/// (`%Y-%m-%dT%H:%M`, the same clock `next_run` uses).
///
/// Accepts RFC 3339 (converted to local), `YYYY-MM-DDTHH:MM[:SS]`,
/// `YYYY-MM-DD HH:MM[:SS]` (local), a bare `YYYY-MM-DD` (local midnight), and a
/// time relative to now (`in 20m`, `+20m`, `90s`, `1h30m`, `in 2 hours`; see
/// [`parse_relative`]). Run times are whole minutes, so a relative time is
/// rounded UP to the next minute: `in 90s` at 10:00:20 is 10:02.
pub fn parse_when(raw: &str) -> Result<String, String> {
    parse_when_at(raw, Local::now())
}

/// [`parse_when`] against an injected clock, for tests.
pub fn parse_when_at(raw: &str, now: DateTime<Local>) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty datetime".into());
    }
    if let Some(d) = parse_relative(s)? {
        let at = now + d;
        // Round up: a minute-resolution clock must not fire before the delay.
        let floor = at.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(at);
        let up = if floor < at { floor + ChronoDuration::minutes(1) } else { floor };
        return Ok(fmt_minute(up));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(fmt_minute(t.with_timezone(&Local)));
    }
    for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return resolve_local(n).map(fmt_minute).ok_or_else(|| format!("{s:?} is not a local time"));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(n) = d.and_hms_opt(0, 0, 0) {
            return resolve_local(n).map(fmt_minute).ok_or_else(|| format!("{s:?} is not a local time"));
        }
    }
    Err(format!(
        "unparseable datetime {s:?}: use YYYY-MM-DDTHH:MM (local time), RFC 3339, or a delay \
         like 'in 20m', '+90s', '1h30m', 'in 2 hours'"
    ))
}

/// Longest relative delay accepted: one year.
const RELATIVE_MAX_S: i64 = 366 * 86_400;

/// A delay relative to now: an optional `in ` or `+`, then one or more
/// `<number><unit>` groups (spaces allowed). Units: s/sec/second, m/min/minute,
/// h/hr/hour, d/day, each with an optional plural `s`.
///
/// `Ok(None)` means "this is not a relative time" (try the absolute forms);
/// `Err` means it looked relative and was wrong (a bare number, an unknown
/// unit, zero), which is worth saying rather than falling through to
/// "unparseable datetime".
pub fn parse_relative(raw: &str) -> Result<Option<ChronoDuration>, String> {
    let s = raw.trim().to_ascii_lowercase();
    let (body, marked) = if let Some(r) = s.strip_prefix('+') {
        (r.trim_start().to_string(), true)
    } else if let Some(r) = s.strip_prefix("in ") {
        (r.trim_start().to_string(), true)
    } else {
        (s.clone(), false)
    };
    // A date (2026-10-21) starts with four digits and a dash; leave it alone.
    let datelike = body.contains(['-', ':']);
    if body.is_empty() || !body.starts_with(|c: char| c.is_ascii_digit()) || (datelike && !marked) {
        return if marked { Err(format!("{raw:?}: expected a delay like 'in 20m' or '+1h30m'")) } else { Ok(None) };
    }
    let mut total: i64 = 0;
    let mut rest = body.as_str();
    let mut groups = 0;
    while !rest.is_empty() {
        let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return Err(format!("{raw:?}: expected a number at {rest:?}"));
        }
        let n: i64 = rest[..digits].parse().map_err(|_| format!("{raw:?}: number too large"))?;
        rest = rest[digits..].trim_start();
        let ulen = rest.chars().take_while(|c| c.is_ascii_alphabetic()).count();
        let unit = &rest[..ulen];
        let secs = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "" => return Err(format!("{raw:?}: {n} needs a unit (s, m, h or d)")),
            other => return Err(format!("{raw:?}: unknown unit {other:?} (use s, m, h or d)")),
        };
        total = total.saturating_add(n.saturating_mul(secs));
        rest = rest[ulen..].trim_start();
        rest = rest.strip_prefix("and ").map(str::trim_start).unwrap_or(rest);
        groups += 1;
    }
    if groups == 0 || total <= 0 {
        return Err(format!("{raw:?}: a delay must be more than zero"));
    }
    if total > RELATIVE_MAX_S {
        return Err(format!("{raw:?}: a delay may be at most 366 days; use an absolute date"));
    }
    Ok(Some(ChronoDuration::seconds(total)))
}

/// A 5-field cron whose minute, hour, day of month and month are single
/// numbers and whose weekday is `*`: it fires on one date a year. Agents used
/// it for "once, at this minute" (49 such rows in one orchestration), and
/// after its date it silently re-arms for next year.
pub fn is_fixed_date_cron(expr: &str) -> bool {
    let f: Vec<&str> = expr.split_whitespace().collect();
    f.len() == 5
        && f[..4].iter().all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
        && f[4] == "*"
}

/// The create/PATCH note for a fixed-date cron with no bound. States, never
/// refuses.
///
/// Since 2026-10-19 such a row fires ONCE and ends (see [`Bounds::end_reason`]):
/// the note used to say it "fires every YEAR", and that was what happened, so
/// SCHED-694, a one-shot TubeScience production roll that ran on 10-16, sat
/// enabled with its next fire on 2027-10-16. A yearly reminder is still a
/// legitimate thing to want; an explicit `max_runs` keeps it.
pub fn fixed_date_note(expr: &str) -> String {
    format!(
        "schedule_expr '{expr}' names one date and no year, so it fires ONCE, at the first \
         occurrence after the schedule was created, and then ends (disabled_reason says so). \
         To repeat it every year, set max_runs to the number of years. For a plain one-off \
         once:'YYYY-MM-DDTHH:MM' says the same thing with the year in it. \
         Classified cadence_class 'yearly'."
    )
}

/// The lifecycle bounds read off a schedule row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bounds {
    pub not_before: Option<String>,
    pub until: Option<String>,
    pub max_runs: Option<i64>,
    pub cron_runs: i64,
    /// For a fixed-date cron with no `max_runs`: the first occurrence after the
    /// row was created (`%Y-%m-%dT%H:%M` local), which is its only fire. `None`
    /// for every other schedule, and when it cannot be computed.
    pub fixed_date_once: Option<String>,
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

impl Bounds {
    pub fn of(s: &DurableSchedule) -> Self {
        let max_runs = match s.raw().get("max_runs") {
            None | Some(serde_json::Value::Null) => None,
            _ => Some(s.i64_field("max_runs", 0)),
        };
        let fixed_date_once = match (max_runs, s.schedule_expr()) {
            (None, Some(e)) if is_fixed_date_cron(e) => fixed_date_first(e, s.raw().get("created")),
            _ => None,
        };
        Bounds {
            not_before: nonempty(s.str_field("not_before")),
            until: nonempty(s.str_field("until")),
            max_runs,
            cron_runs: s.i64_field("cron_runs", 0),
            fixed_date_once,
        }
    }

    /// The first occurrence of `expr` after `now` that is not before
    /// `not_before`.
    pub fn first_after(&self, expr: &ScheduleExpr, now: DateTime<Local>) -> Option<DateTime<Local>> {
        let base = match self.not_before.as_deref().and_then(parse_minute) {
            Some(nb) if nb - ChronoDuration::minutes(1) > now => nb - ChronoDuration::minutes(1),
            _ => now,
        };
        expr.next_run_after(base)
    }

    /// Is `next` (a stored `next_run` string) earlier than `not_before`?
    pub fn too_early(&self, next: &str) -> bool {
        matches!(self.not_before.as_deref(), Some(nb) if !next.is_empty() && next < nb)
    }

    /// Why the schedule is over, given the fire it would make next. `None`
    /// means it may fire. Plain string compare is safe: both sides are the
    /// fixed `%Y-%m-%dT%H:%M` local format `due_schedules` already compares.
    pub fn end_reason(&self, next: Option<&str>) -> Option<String> {
        // A fixed-date cron with no max_runs is a one-shot: it ends after its
        // fire, and a row that already fired (cron_runs counts it) or whose
        // date passed unfired (its next fire is a later year) ends rather than
        // firing a year late. A reserve refusal gives cron_runs back and the
        // capacity retry re-enables a row its fire ended, so the date is kept.
        if let Some(first) = self.fixed_date_once.as_deref() {
            if self.cron_runs >= 1 {
                return Some(format!(
                    "fixed-date cron fired its one date ({first}, {} scheduled fire(s)); it names \
                     no year, so it ended instead of re-arming for next year. Set max_runs to repeat it",
                    self.cron_runs
                ));
            }
            if let Some(n) = next.filter(|n| !n.is_empty() && *n > first) {
                return Some(format!(
                    "fixed-date cron missed its one date ({first}) and would next fire {n}; it names \
                     no year, so it ended instead of firing a year late. Set max_runs to repeat it"
                ));
            }
        }
        if let Some(m) = self.max_runs {
            if self.cron_runs >= m {
                return Some(format!(
                    "max_runs {m} reached ({} scheduled fire(s))",
                    self.cron_runs
                ));
            }
        }
        if let (Some(u), Some(n)) = (self.until.as_deref(), next.filter(|n| !n.is_empty())) {
            if n > u {
                return Some(format!("until {u} reached (the next fire would be {n})"));
            }
        }
        None
    }
}

/// The first occurrence of a fixed-date cron after `created` (epoch seconds,
/// as the row stores it), in the stored minute format. `None` when the row
/// has no readable `created` or the expression does not parse.
fn fixed_date_first(expr: &str, created: Option<&serde_json::Value>) -> Option<String> {
    let secs = created.and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))?;
    let at = DateTime::from_timestamp(secs as i64, 0)?.with_timezone(&Local);
    let e = ScheduleExpr::parse(expr).ok()?;
    e.next_run_after(at - ChronoDuration::minutes(1)).map(fmt_minute)
}

/// Disable a schedule because a bound was reached. Writes `disabled_reason`
/// and two audit rows (the enabled flip and the reason). Returns whether it
/// was enabled before.
pub fn end_schedule(conn: &Connection, id: &str, reason: &str) -> rusqlite::Result<bool> {
    let now = chrono::Utc::now().timestamp();
    let n = conn.execute(
        "UPDATE schedules SET enabled=0, disabled_reason=?2, updated=?3 WHERE id=?1 AND enabled=1",
        rusqlite::params![id, reason, now],
    )?;
    if n > 0 {
        insert_audit(conn, id, "enabled", "1", "0", BOUND_SOURCE, HARNESS_BY)?;
        insert_audit(conn, id, "disabled_reason", "", reason, BOUND_SOURCE, HARNESS_BY)?;
        tracing::info!(schedule = id, reason, verdict = "schedule_bound_reached",
            measured = true, n_considered = 1, "schedule reached its bound and was disabled");
    }
    Ok(n > 0)
}

/// Pause every enabled schedule a retired worker owns (archive or delete).
///
/// Default is PAUSE, reversible: re-enable after moving the schedule to a live
/// worker. The owner sees each one named in the archive/delete response and
/// in one WARN line per schedule. A schedule that kept firing into an archived
/// lane is the failure this removes; before it, only three consecutive failures
/// paused one (`schedule_failure::should_pause_orphan`), and a green orphan such
/// as an every-1m poll ran forever.
pub fn pause_for_retired_worker(
    conn: &Connection,
    session: &str,
    why: &str,
    by: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    if session.trim().is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, title FROM schedules WHERE deleted IS NULL AND enabled=1 AND session=?1 ORDER BY id",
        )?;
        let it = stmt.query_map([session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let now = chrono::Utc::now().timestamp();
    let reason = format!(
        "paused because its worker {session} was {why}. Move it to a live worker (PATCH session) \
         and re-enable it, or delete it."
    );
    let source = format!("harness:worker-{why}");
    for (id, title) in &rows {
        conn.execute(
            "UPDATE schedules SET enabled=0, disabled_reason=?2, updated=?3 WHERE id=?1",
            rusqlite::params![id, reason, now],
        )?;
        insert_audit(conn, id, "enabled", "1", "0", &source, by)?;
        tracing::warn!(schedule = %id, title = %title, session, why, by,
            verdict = "schedule_paused_worker_retired", measured = true, n_considered = rows.len(),
            "worker retired; its enabled schedule was paused so it stops firing into a gone lane");
    }
    tracing::info!(session, why, paused = rows.len(), measured = true, n_considered = rows.len(),
        verdict = "schedule_retired_worker_sweep", "checked a retired worker's schedules");
    Ok(rows)
}

/// The sentence the archive/delete response carries about schedules.
pub fn retired_sentence(paused: &[(String, String)]) -> String {
    if paused.is_empty() {
        return "no enabled schedules".into();
    }
    let ids: Vec<&str> = paused.iter().map(|(id, _)| id.as_str()).collect();
    format!(
        "paused {} enabled schedule(s): {} (re-enable after moving each to a live worker)",
        paused.len(),
        ids.join(", ")
    )
}

/// Error text that means an expired or revoked USER credential (gcloud user
/// accounts under session control expire daily). Lowercase substrings; one
/// list feeds both the classifier and the SQL count so they cannot drift.
pub const USER_CREDENTIAL_PATTERNS: [&str; 7] = [
    "reauthentication failed",
    "cannot prompt during non-interactive execution",
    "(gcloud.auth.print-access-token)",
    "there was a problem refreshing your current auth tokens",
    "invalid_grant",
    "token has been expired or revoked",
    "gcloud auth login",
];

/// The cause name for a failure, if its text is an expired user credential.
pub fn credential_cause(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    USER_CREDENTIAL_PATTERNS
        .iter()
        .any(|p| t.contains(p))
        .then_some("expired_user_credential")
}

/// A SQL boolean over `schedule_runs.note`/`output_tail` matching
/// [`USER_CREDENTIAL_PATTERNS`]. The patterns are constants (no caller
/// input), and single quotes are doubled anyway.
pub fn credential_sql_predicate() -> String {
    let parts: Vec<String> = USER_CREDENTIAL_PATTERNS
        .iter()
        .map(|p| {
            let p = p.replace('\'', "''");
            format!(
                "instr(lower(COALESCE(note,'')), '{p}') > 0 OR instr(lower(COALESCE(output_tail,'')), '{p}') > 0"
            )
        })
        .collect();
    format!("({})", parts.join(" OR "))
}

/// The hint carried in a failure notice for a credential failure.
pub const CREDENTIAL_HINT: &str = "This is an expired USER credential, not a bug in the script: \
     user accounts under session control expire daily and cannot re-prompt in a schedule. Run the \
     command as a service account (for gcloud, --account=<service account> or \
     CLOUDSDK_CORE_ACCOUNT) so it stops expiring.";

/// Runs the server will reconcile at startup, per schedule: rows still
/// `running` when the process died. Read before the reconcile so each
/// schedule gets its own WARN line naming it.
pub fn running_rows_by_schedule(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT schedule_id, COUNT(*) FROM schedule_runs WHERE status='running' GROUP BY schedule_id ORDER BY schedule_id",
    )?;
    let it = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    it.collect()
}

/// After the reconcile: stamp each affected schedule so its own row says the
/// last run was interrupted (AF-648's `last_delivery`), and log each one.
pub fn stamp_interrupted(conn: &Connection, affected: &[(String, i64)]) -> rusqlite::Result<()> {
    for (id, n) in affected {
        conn.execute(
            "UPDATE schedules SET last_delivery='interrupted', \
             last_refusal_reason='the server restarted while this run was in flight; its outcome is unknown' \
             WHERE id=?1",
            [id],
        )?;
        tracing::warn!(schedule = %id, runs = n, verdict = "schedule_run_interrupted",
            measured = true, n_considered = affected.len(),
            "a run was in flight when the server stopped; recorded as interrupted (outcome unknown)");
    }
    Ok(())
}

/// Text told to a worker when someone else disabled its schedule.
pub fn disabled_notice_text(sched: &DurableSchedule, by: &str, owner: bool, reason: &str) -> String {
    let id = sched.id();
    let title = sched.str_field("title");
    let who = if owner { format!("the owner ({by})") } else { by.to_string() };
    let lock = if owner {
        "It is LOCKED: a worker cannot re-enable it or Run it now. Ask the owner if you think it \
         should run again."
    } else {
        "You can re-enable it if it should run, but say why on your card first."
    };
    format!(
        "[amux] SCHEDULE DISABLED: {id} \"{title}\", your schedule, was disabled by {who}.\n\
         Reason: {reason}\n{lock}\nGET /api/schedules/{id} shows locked_by, lock_reason and disabled_reason."
    )
}

/// Tell the owning worker that someone else disabled its schedule, through the
/// same steering queue and system sender the failure notice uses. Returns what
/// happened, for the PATCH response.
///
/// An isolated, archived or unregistered worker gets nothing injected (an
/// isolated lane takes no amux prompts). For those the lock is what holds: a
/// re-enable attempt is refused with the owner's reason, so the worker learns
/// it at the one moment it matters.
pub async fn notify_disabled(
    store: &crate::db::SharedStore,
    sched: &DurableSchedule,
    by: &str,
    owner: bool,
    reason: &str,
) -> serde_json::Value {
    use super::schedule_failure::{resolve_route, LiveProbe, Route, SENDER};
    let worker = sched.str_field("session").trim().to_string();
    let id = sched.id().to_string();
    let route = resolve_route(store, &worker, &LiveProbe).await;
    if let Route::Owner { cause } = &route {
        // A stopped or paused worker still reads its queue on its next turn.
        if !matches!(cause.as_str(), "stopped" | "paused") {
            tracing::info!(schedule = %id, worker = %worker, cause = %cause,
                verdict = "schedule_disable_notice_skipped", measured = true, n_considered = 1,
                "schedule disabled by someone else; its worker cannot be told, the lock refusal carries the reason");
            return serde_json::json!({"route": "none", "worker": worker, "cause": cause});
        }
    }
    let text = disabled_notice_text(sched, by, owner, reason);
    match crate::api::session_verbs::steer_enqueue_store(store, &worker, &text, &format!("sched-disabled:{id}"), SENDER).await {
        Ok(qid) => {
            tracing::info!(schedule = %id, worker = %worker, by, owner, queue_id = %qid,
                verdict = "schedule_disable_notified", measured = true, n_considered = 1,
                "told the owning worker its schedule was disabled by someone else");
            serde_json::json!({"route": "worker", "worker": worker, "queue_id": qid})
        }
        Err(why) => {
            tracing::warn!(schedule = %id, worker = %worker, cause = why,
                verdict = "schedule_disable_notice_skipped", measured = true, n_considered = 1,
                "could not queue the disable notice for the owning worker");
            serde_json::json!({"route": "none", "worker": worker, "cause": why})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(fields: serde_json::Value) -> DurableSchedule {
        DurableSchedule::from_map(fields.as_object().unwrap().clone())
    }

    /// Epoch seconds for a local wall-clock minute, the way `created` is stored.
    fn created_at(local: &str) -> i64 {
        parse_minute(local).unwrap().timestamp()
    }

    #[test]
    fn an_unbounded_fixed_date_cron_fires_once_then_ends() {
        // SCHED-694 shape: created 10-12, dated 10-16 04:00, no max_runs.
        let r = row(json!({"schedule_expr": "0 4 16 10 *", "created": created_at("2026-10-12T20:37"), "cron_runs": 0}));
        let b = Bounds::of(&r);
        assert_eq!(b.fixed_date_once.as_deref(), Some("2026-10-16T04:00"));
        // Due on its date: it may fire.
        assert_eq!(b.end_reason(Some("2026-10-16T04:00")), None);
        // The fire path's post-fire check (cron_runs + 1, next = a year on) ends it.
        let after = Bounds { cron_runs: 1, ..b.clone() };
        let why = after.end_reason(Some("2027-10-16T04:00")).expect("ends after its one fire");
        assert!(why.contains("fired its one date (2026-10-16T04:00"), "{why}");
    }

    #[test]
    fn a_fixed_date_cron_that_already_fired_or_missed_its_date_ends_instead_of_firing() {
        // Already re-armed for 2027 after firing (the rows in the database today).
        let fired = Bounds::of(&row(json!({"schedule_expr": "0 4 16 10 *", "created": created_at("2026-10-12T20:37"), "cron_runs": 1})));
        assert!(fired.end_reason(Some("2027-10-16T04:00")).unwrap().contains("fired its one date"));
        // Never fired, but its date passed (SCHED-615 shape): next is a year late.
        let missed = Bounds::of(&row(json!({"schedule_expr": "10 15 7 10 *", "created": created_at("2026-10-05T12:00"), "cron_runs": 0})));
        let why = missed.end_reason(Some("2027-10-07T15:10")).expect("a year-late fire ends");
        assert!(why.contains("missed its one date (2026-10-07T15:10)"), "{why}");
    }

    #[test]
    fn max_runs_keeps_a_yearly_reminder_and_other_crons_are_untouched() {
        let yearly = Bounds::of(&row(json!({"schedule_expr": "0 9 14 2 *", "created": created_at("2026-01-01T00:00"), "cron_runs": 1, "max_runs": 10})));
        assert_eq!(yearly.fixed_date_once, None);
        assert_eq!(yearly.end_reason(Some("2027-02-14T09:00")), None);
        let daily = Bounds::of(&row(json!({"schedule_expr": "0 9 * * *", "created": created_at("2026-01-01T00:00"), "cron_runs": 40})));
        assert_eq!(daily.fixed_date_once, None);
        assert_eq!(daily.end_reason(Some("2026-10-20T09:00")), None);
    }

    #[test]
    fn parse_when_normalises_the_accepted_spellings() {
        assert_eq!(parse_when("2026-10-21T09:05").unwrap(), "2026-10-21T09:05");
        assert_eq!(parse_when("2026-10-21 09:05:33").unwrap(), "2026-10-21T09:05");
        assert_eq!(parse_when("2026-10-21").unwrap(), "2026-10-21T00:00");
        let rfc = parse_when("2026-10-21T13:05:00Z").unwrap();
        assert_eq!(rfc.len(), 16, "RFC 3339 converts to the local minute format: {rfc}");
        assert!(parse_when("next tuesday").is_err());
        assert!(parse_when("  ").is_err());
    }

    #[test]
    fn parse_when_accepts_a_delay_and_rounds_it_up_to_the_minute() {
        let now = resolve_local(NaiveDateTime::parse_from_str("2026-10-29T10:00:20", "%Y-%m-%dT%H:%M:%S").unwrap()).unwrap();
        let at = |s: &str| parse_when_at(s, now).unwrap();
        assert_eq!(at("in 20m"), "2026-10-29T10:21", "10:20:20 rounds up to 10:21");
        assert_eq!(at("+20m"), "2026-10-29T10:21");
        assert_eq!(at("20m"), "2026-10-29T10:21");
        assert_eq!(at("90s"), "2026-10-29T10:02", "10:01:50 rounds up to 10:02");
        assert_eq!(at("40s"), "2026-10-29T10:01");
        assert_eq!(at("2h"), "2026-10-29T12:01");
        assert_eq!(at("1h30m"), "2026-10-29T11:31");
        assert_eq!(at("1h 30m"), "2026-10-29T11:31");
        assert_eq!(at("in 2 hours"), "2026-10-29T12:01");
        assert_eq!(at("in 1 hour and 5 minutes"), "2026-10-29T11:06");
        assert_eq!(at("10 min"), "2026-10-29T10:11");
        assert_eq!(at("3hr"), "2026-10-29T13:01");
        assert_eq!(at("1d"), "2026-10-30T10:01");
        assert_eq!(at("IN 5M"), "2026-10-29T10:06");
        // On a whole minute, nothing to round.
        let whole = resolve_local(NaiveDateTime::parse_from_str("2026-10-29T10:00:00", "%Y-%m-%dT%H:%M:%S").unwrap()).unwrap();
        assert_eq!(parse_when_at("5m", whole).unwrap(), "2026-10-29T10:05");
        // Absolute forms still parse as before.
        assert_eq!(at("2026-10-21T09:05"), "2026-10-21T09:05");
        assert_eq!(at("2026-10-21"), "2026-10-21T00:00");
        for bad in ["in 20", "+", "20 fortnights", "0m", "in 0s", "in soon", "5m x", "400d", "+2026-10-21"] {
            let e = parse_when_at(bad, now).unwrap_err();
            assert!(!e.is_empty(), "{bad} is refused");
        }
        assert!(parse_when_at("in 20", now).unwrap_err().contains("needs a unit"));
    }

    #[test]
    fn a_fixed_date_cron_is_told_apart_from_a_recurring_one() {
        for e in ["5 0 3 10 *", "0 4 16 10 *", "59 19 7 10 *"] {
            assert!(is_fixed_date_cron(e), "{e} names one date");
        }
        for e in ["0 0 1 * *", "*/5 * * * *", "0 9 * * 1", "0 9 1 1 1", "0 9 1,15 10 *", "every 1h", "0 0 1 1"] {
            assert!(!is_fixed_date_cron(e), "{e} is not a fixed-date cron");
        }
    }

    #[test]
    fn bounds_end_on_max_runs_and_on_until_and_not_before() {
        let b = Bounds::of(&row(json!({"max_runs": 2, "cron_runs": 1, "until": "2026-10-21T09:00"})));
        assert_eq!(b.max_runs, Some(2));
        assert!(b.end_reason(Some("2026-10-20T09:00")).is_none(), "1 of 2 fires, before until");
        assert!(b.end_reason(Some("2026-10-22T09:00")).unwrap().contains("until"));
        let done = Bounds { cron_runs: 2, ..b.clone() };
        assert!(done.end_reason(Some("2026-10-20T09:00")).unwrap().contains("max_runs 2"));
        let unbounded = Bounds::of(&row(json!({"max_runs": null, "until": ""})));
        assert_eq!(unbounded, Bounds::default());
        assert!(unbounded.end_reason(Some("2099-01-01T00:00")).is_none());

        let nb = Bounds { not_before: Some("2026-11-01T00:00".into()), ..Bounds::default() };
        assert!(nb.too_early("2026-10-31T23:59"));
        assert!(!nb.too_early("2026-11-01T00:00"));
        let now = parse_minute("2026-10-18T12:00").unwrap();
        let first = nb.first_after(&ScheduleExpr::parse("daily at 09:00").unwrap(), now).unwrap();
        assert_eq!(fmt_minute(first), "2026-11-01T09:00");
        let ev = nb.first_after(&ScheduleExpr::parse("every 30m").unwrap(), now).unwrap();
        assert!(fmt_minute(ev).as_str() >= "2026-11-01T00:00", "{}", fmt_minute(ev));
    }

    #[test]
    fn the_credential_classifier_and_its_sql_agree() {
        let gcloud = "ERROR: (gcloud.auth.print-access-token) Reauthentication failed. cannot prompt during non-interactive execution.";
        assert_eq!(credential_cause(gcloud), Some("expired_user_credential"));
        assert_eq!(credential_cause("exit 1: no mvs-primary pod found"), None);
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE schedule_runs (note TEXT, output_tail TEXT);").unwrap();
        conn.execute("INSERT INTO schedule_runs VALUES (?1, NULL)", [gcloud]).unwrap();
        conn.execute("INSERT INTO schedule_runs VALUES ('exit 1', 'no pod found')", []).unwrap();
        conn.execute("INSERT INTO schedule_runs VALUES ('exit 1', 'ERROR: invalid_grant: Bad Request')", []).unwrap();
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM schedule_runs WHERE {}", credential_sql_predicate()), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2, "the SQL count matches the classifier over the same rows");
    }

    #[test]
    fn retiring_a_worker_pauses_only_its_enabled_schedules_with_an_audit_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        store
            .write(|conn| {
                for (id, sess, en) in [("S1", "gone", 1), ("S2", "gone", 0), ("S3", "live", 1)] {
                    conn.execute(
                        "INSERT INTO schedules (id,title,session,command,enabled,created,updated) VALUES (?1,?1,?2,'x',?3,0,0)",
                        rusqlite::params![id, sess, en],
                    )?;
                }
                let paused = pause_for_retired_worker(conn, "gone", "archived", "owner")?;
                assert_eq!(paused, vec![("S1".to_string(), "S1".to_string())]);
                assert!(retired_sentence(&paused).contains("S1"));
                let (en, why): (i64, String) = conn.query_row(
                    "SELECT enabled, disabled_reason FROM schedules WHERE id='S1'", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
                assert_eq!(en, 0);
                assert!(why.contains("gone was archived"), "{why}");
                let live: i64 = conn.query_row("SELECT enabled FROM schedules WHERE id='S3'", [], |r| r.get(0))?;
                assert_eq!(live, 1, "another worker's schedule is untouched");
                let audit: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM schedule_audit WHERE schedule_id='S1' AND source='harness:worker-archived' AND by_who='owner'",
                    [], |r| r.get(0))?;
                assert_eq!(audit, 1, "the pause is in the audit trail");
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
    }
}
