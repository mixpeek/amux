//! Tell a schedule's owning worker that its schedule failed, and ask it to fix
//! the cause (Ethan, 2026-10-13: "if a scheduler is failing the worker should
//! know and fix").
//!
//! # What existed before
//!
//! A failing schedule was visible only to someone reading the Scheduler page:
//! `schedule_runs.status` said `error`/`refused`, `run_stats_24h.failures`
//! counted it, and the row showed "Failing". The one worker-facing path was
//! `LiveDeliverer::wake_owner` (removed here, superseded by this module),
//! which fired ONLY for a shell schedule whose `exit_actions` mapped the exit
//! code to `alert`. A plain non-zero exit, a
//! timeout, a spawn error and every refused tmux delivery told nobody.
//! SCHED-108 exited 1 and was earlier refused because its worker was archived;
//! the worker never heard about either.
//!
//! # What this does
//!
//! Every recorded run outcome is passed to [`on_run_outcome`]. It is two
//! primitives, schedules and messages, and nothing new:
//!
//! - The streak and the last notice live on the `schedules` row
//!   (`failure_streak`, `failure_notice`, migration 0120), so a restart does
//!   not re-notify and `GET /api/schedules/<id>` shows the worker what it was
//!   told.
//! - The notice to a worker goes through `steer_enqueue_store`, the same
//!   steering queue the old `wake_owner`, board-drive and peer relays use. It lands at
//!   the worker's next turn boundary. The sender is `harness:scheduler`, a
//!   system origin: this is the server telling a worker about its OWN
//!   schedule, the same way the schedule's own fires reach it, so the
//!   worker-to-worker group boundary does not apply. A worker-origin send
//!   still meets that boundary unchanged.
//! - When the worker cannot be told (isolated, archived, paused, stopped,
//!   unregistered, or the queue refused), the notice goes to the owner as a
//!   typed `needsyou` card, the surface amux already uses for "the owner must
//!   act" (see `prod_change::new_notice`). One open card per schedule; later
//!   notices append to it rather than stacking cards.
//!
//! # Dedupe (ethos rule 5)
//!
//! Once per failure streak (the first failure after a pass), again when the
//! error signature changes (failure kind + exit code + a hash of the last
//! error line with digits folded, so a counter in the message does not count
//! as a new error), and again every [`ESCALATE_AFTER`] further failures with
//! the same signature. A pass resets the streak. A run refused by the
//! background reserve neither fails nor passes: nothing is broken.
//!
//! Every decision is logged with a stable verdict:
//! `schedule_failure_notified`, `schedule_failure_routed_owner`,
//! `schedule_failure_suppressed_dupe`, `schedule_failure_streak_reset`,
//! `schedule_paused_owner_gone` (see [`should_pause_orphan`]).

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::scheduler::{DurableSchedule, RunOutcome};
use crate::db::{PendingEvent, SharedStore, WriteOutcome};

/// Same-signature failures after the last notice before telling again.
pub const ESCALATE_AFTER: i64 = 3;
/// Consecutive failures after which a schedule whose worker is gone is paused.
pub const PAUSE_ORPHAN_AFTER: i64 = 3;

/// THE HARNESS FIXES WHAT NOBODY ELSE CAN (Ethan, 2026-10-15: "if a scheduler
/// fails, amux harness should try to fix"). A schedule whose worker is
/// archived or no longer registered has no one left to repair it, and it kept
/// firing: SCHED-640 failed 572 times in a row against a deleted VM, SCHED-583
/// 286 times, SCHED-586 192, each failure re-raising the owner's card. After
/// [`PAUSE_ORPHAN_AFTER`] consecutive failures the harness pauses it
/// (reversible: re-enable it, ideally after moving it to a live worker) and
/// says so on the schedule and its card, instead of asking the owner again.
pub fn should_pause_orphan(cause: &str, streak: i64) -> bool {
    matches!(cause, "archived" | "unregistered") && streak >= PAUSE_ORPHAN_AFTER
}
/// The sender stamped on the queued notice: a system origin, not a worker.
pub const SENDER: &str = "harness:scheduler";
/// Lines of output carried in the notice.
pub const TAIL_LINES: usize = 40;
/// The reserve's refusal text (`usage::reserve_pause_note("schedule")`).
const RESERVE_PREFIX: &str = "schedule PAUSED: the plan window";

/// The steering guard. Named per schedule so a newer notice for the same
/// schedule replaces an older one still queued rather than stacking.
pub fn guard_for(id: &str) -> String {
    format!("sched-failure:{id}")
}

/// The `source_ref` on the owner card, one open card per schedule.
pub fn owner_card_ref(id: &str) -> String {
    format!("sched-failure:{id}")
}

/// What one run outcome means for the streak.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// Neither: the background reserve held the fire back.
    Neutral,
    Fail(Failure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// `exit`, `refused`, `failed` (timeout, spawn error, failed delivery), or
    /// `credential` (an expired user credential, whatever the exit path).
    pub kind: &'static str,
    pub exit_code: Option<i32>,
    /// The line the signature is computed from.
    pub error_line: String,
    /// The full reason, as recorded on the run row.
    pub detail: String,
    /// Output tail for a shell run, empty otherwise.
    pub tail: String,
}

impl Failure {
    pub fn signature(&self) -> String {
        format!(
            "{}:{}:{:016x}",
            self.kind,
            self.exit_code.map(|c| c.to_string()).unwrap_or_default(),
            fnv1a(&normalize(&self.error_line))
        )
    }
}

/// The verdict for one outcome. A failure whose text is an expired USER
/// credential gets kind `credential` (GS-12 finding 5: 881 runs failed on
/// expired gcloud user tokens and each read as its own error).
pub fn classify(outcome: &RunOutcome) -> Verdict {
    match classify_raw(outcome) {
        Verdict::Fail(mut f) => {
            let text = format!("{}\n{}\n{}", f.detail, f.error_line, f.tail);
            if super::schedule_lifecycle::credential_cause(&text).is_some() {
                f.kind = "credential";
            }
            Verdict::Fail(f)
        }
        other => other,
    }
}

fn classify_raw(outcome: &RunOutcome) -> Verdict {
    match outcome {
        RunOutcome::Delivered { .. } | RunOutcome::Queued { .. } | RunOutcome::ShellOk { .. } => {
            Verdict::Pass
        }
        RunOutcome::Refused { reason } if reason.starts_with(RESERVE_PREFIX) => Verdict::Neutral,
        // A goal-continuation fire skipped because the goal is met, the worker
        // is busy or was just continued: working as designed, not a failure.
        RunOutcome::Refused { reason } if reason.starts_with(super::schedule_wake::GATE_PREFIX) => {
            Verdict::Neutral
        }
        RunOutcome::Refused { reason } => Verdict::Fail(Failure {
            kind: "refused",
            exit_code: None,
            error_line: reason.clone(),
            detail: format!("refused: {reason}"),
            tail: String::new(),
        }),
        RunOutcome::Failed { reason } => Verdict::Fail(Failure {
            kind: "failed",
            exit_code: None,
            error_line: reason.clone(),
            detail: format!("failed: {reason}"),
            tail: String::new(),
        }),
        RunOutcome::ShellError { note, output } => {
            let tail = output.as_ref().map(|o| o.tail.clone()).unwrap_or_default();
            let error_line = last_line(&tail).unwrap_or_else(|| note.clone());
            Verdict::Fail(Failure {
                kind: "exit",
                exit_code: output.as_ref().map(|o| o.exit_code),
                error_line,
                detail: note.clone(),
                tail,
            })
        }
    }
}

fn last_line(s: &str) -> Option<String> {
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("[output"))
        .map(str::to_string)
}

/// Digit runs fold to `#` and whitespace collapses, so "3 repairs needed" and
/// "4 repairs needed" are one error, while a different message is another.
fn normalize(s: &str) -> String {
    let mut out = String::new();
    let mut prev_digit = false;
    let mut prev_space = false;
    for c in s.trim().chars().take(400) {
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('#');
            }
            prev_digit = true;
            prev_space = false;
        } else if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
            prev_digit = false;
        } else {
            out.push(c);
            prev_digit = false;
            prev_space = false;
        }
    }
    out
}

/// Stable across builds and restarts (a `DefaultHasher` is not promised to be).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The last notice for the current streak, as stored in `failure_notice`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub at: i64,
    /// The streak length when this notice went out.
    pub streak: i64,
    /// `first`, `changed` or `escalation`.
    pub kind: String,
    pub signature: String,
    pub error: String,
    /// `worker` or `owner`.
    pub route: String,
    /// The worker told, or the worker that could not be told when `route` is owner.
    pub worker: String,
    /// Why the owner was told instead of the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Steering queue id (worker) or card id (owner). None until delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    /// Same-signature failures since this notice that were not re-sent.
    #[serde(default)]
    pub suppressed: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Notify(&'static str),
    Suppress,
    Reset,
    Nothing,
}

/// The dedupe rule, with every input already read. Returns the new streak.
pub fn decide(prev_streak: i64, prev: Option<&Notice>, v: &Verdict) -> (i64, Action) {
    match v {
        Verdict::Pass if prev_streak > 0 => (0, Action::Reset),
        Verdict::Pass => (0, Action::Nothing),
        Verdict::Neutral => (prev_streak, Action::Nothing),
        Verdict::Fail(f) => {
            let n = prev_streak + 1;
            let Some(p) = prev.filter(|_| prev_streak > 0) else {
                return (n, Action::Notify("first"));
            };
            if p.signature != f.signature() {
                (n, Action::Notify("changed"))
            } else if n - p.streak >= ESCALATE_AFTER {
                (n, Action::Notify("escalation"))
            } else {
                (n, Action::Suppress)
            }
        }
    }
}

/// Who should hear about a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Worker,
    Owner { cause: String },
}

/// The routing rule. `refusal` is `schedule_target_refusal`'s answer, the
/// one definition the deliverer and the `schedule.target_can_receive`
/// invariant already share. An isolated worker gets no amux-injected prompt:
/// its schedule's own fires are owner configuration and reach it, a failure
/// notice is amux steering and does not.
pub(crate) fn route_for(
    refusal: Option<crate::api::session_verbs::TargetRefusal>,
    running: bool,
) -> Route {
    match refusal {
        Some(r) => Route::Owner { cause: r.cause().to_string() },
        None if !running => Route::Owner { cause: "stopped".into() },
        None => Route::Worker,
    }
}

/// Is the worker's process up? A trait so tests do not need a tmux fleet.
#[async_trait::async_trait]
pub trait WorkerProbe: Send + Sync {
    async fn running(&self, name: &str) -> bool;
}

pub struct LiveProbe;

#[async_trait::async_trait]
impl WorkerProbe for LiveProbe {
    async fn running(&self, name: &str) -> bool {
        crate::api::session_verbs::is_running(name).await
    }
}

pub(crate) async fn resolve_route(store: &SharedStore, worker: &str, probe: &dyn WorkerProbe) -> Route {
    use crate::api::session_verbs::{permanently_blocked_resolved, schedule_target_refusal, TargetRefusal};
    let mut refusal = schedule_target_refusal(worker);
    // A rust-managed worker has no env file; its `_amux_workers` row proves it
    // exists, the same overturn `permanently_blocked_resolved` applies.
    if refusal == Some(TargetRefusal::Unregistered) {
        let w = worker.to_string();
        let exists = store
            .read_async(move |c| Ok(permanently_blocked_resolved(c, &w).is_none()))
            .await
            .unwrap_or(false);
        if exists {
            refusal = None;
        }
    }
    let running = refusal.is_none() && probe.running(worker).await;
    route_for(refusal, running)
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_default()
}

/// The notice text. Compact, and everything needed to act without a lookup.
pub fn notice_text(
    sched: &DurableSchedule,
    f: &Failure,
    streak: i64,
    kind: &str,
    source: &str,
    ran_at: i64,
) -> String {
    let id = sched.id();
    let title = sched.str_field("title");
    let skind = if sched.str_field("kind") == "shell" { "shell" } else { "tmux" };
    let head = match kind {
        "escalation" => format!(
            "[amux] SCHEDULE STILL FAILING: {id} \"{title}\" has failed {streak} runs in a row with the same error."
        ),
        "changed" => format!(
            "[amux] SCHEDULE FAILING, NEW ERROR: {id} \"{title}\" failed again ({streak} in a row) and the error changed."
        ),
        _ => format!("[amux] SCHEDULE FAILED: {id} \"{title}\", your schedule, failed ({streak} in a row)."),
    };
    let mut out = format!(
        "{head}\n\nWhen: {} ({source})\nRan ({skind}): {}\nResult: {}\n",
        fmt_ts(ran_at),
        truncate(sched.str_field("command"), 200),
        truncate(&f.detail, 300),
    );
    if !f.tail.trim().is_empty() {
        out.push_str(&format!(
            "\nLast output (up to {TAIL_LINES} lines):\n{}\n",
            tail_lines(&f.tail, TAIL_LINES)
        ));
    }
    if f.kind == "credential" {
        out.push_str(&format!("\n{}\n", super::schedule_lifecycle::CREDENTIAL_HINT));
    }
    out.push_str(&format!(
        "\nAsk: fix the cause (the script, its inputs, or the schedule itself), then rerun it with \
         POST /api/schedules/{id}/run (curl -sk -X POST -H \"X-Amux-Session: $AMUX_SESSION\" \
         \"$(amux url)/api/schedules/{id}/run\") and confirm it passes: GET /api/schedules/{id} \
         shows failure_streak 0. If this failure is expected by design, say so and change the \
         schedule (command, exit_actions or cadence) so it stops reporting failure.\n\
         You will not be told again about this same error until it changes or fails \
         {ESCALATE_AFTER} more times."
    ));
    out
}

/// Act on one recorded run outcome. Never fails the caller: a notice that
/// could not be sent is logged, and the run row already says what happened.
pub async fn on_run_outcome(store: &SharedStore, sched: &DurableSchedule, outcome: &RunOutcome, source: &str) {
    on_run_outcome_with(store, sched, outcome, source, &LiveProbe).await
}

pub async fn on_run_outcome_with(
    store: &SharedStore,
    sched: &DurableSchedule,
    outcome: &RunOutcome,
    source: &str,
    probe: &dyn WorkerProbe,
) {
    if let Err(e) = handle(store, sched, outcome, source, probe).await {
        tracing::warn!(schedule = %sched.id(), error = %e, verdict = "schedule_failure_notice_error",
            measured = true, n_considered = 1, "could not process a schedule run outcome for the failure notice");
    }
}

async fn handle(
    store: &SharedStore,
    sched: &DurableSchedule,
    outcome: &RunOutcome,
    source: &str,
    probe: &dyn WorkerProbe,
) -> anyhow::Result<()> {
    let verdict = classify(outcome);
    let worker = sched.str_field("session").trim().to_string();
    // Routing needs a process probe; only pay it for a failure.
    let route = match &verdict {
        Verdict::Fail(_) => Some(resolve_route(store, &worker, probe).await),
        _ => None,
    };
    let now = chrono::Utc::now().timestamp();
    let id = sched.id().to_string();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None::<(i64, Action, Option<Notice>)>));
    let slot_w = slot.clone();
    let (v_w, route_w, worker_w, id_w) = (verdict.clone(), route.clone(), worker.clone(), id.clone());
    store
        .write_async(move |conn| {
            use rusqlite::OptionalExtension;
            let row: Option<(i64, Option<String>)> = conn
                .query_row(
                    "SELECT COALESCE(failure_streak,0), failure_notice FROM schedules WHERE id=?1",
                    [&id_w],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((prev_streak, raw)) = row else {
                return Ok(WriteOutcome { applied: false, events: vec![] });
            };
            let prev: Option<Notice> = raw.as_deref().and_then(|s| serde_json::from_str(s).ok());
            let (streak, action) = decide(prev_streak, prev.as_ref(), &v_w);
            let notice: Option<Notice> = match (action, &v_w) {
                (Action::Notify(kind), Verdict::Fail(f)) => {
                    let (r, reason) = match &route_w {
                        Some(Route::Owner { cause }) => ("owner", Some(cause.clone())),
                        _ => ("worker", None),
                    };
                    Some(Notice {
                        at: now,
                        streak,
                        kind: kind.to_string(),
                        signature: f.signature(),
                        error: truncate(&f.error_line, 300),
                        route: r.into(),
                        worker: worker_w.clone(),
                        reason,
                        delivery: None,
                        suppressed: 0,
                    })
                }
                (Action::Suppress, _) => prev.clone().map(|mut p| {
                    p.suppressed += 1;
                    p
                }),
                // A reset keeps the last notice for the record; the streak says it is over.
                _ => prev.clone(),
            };
            let changed = streak != prev_streak || notice != prev;
            if changed {
                conn.execute(
                    "UPDATE schedules SET failure_streak=?1, failure_notice=?2 WHERE id=?3",
                    rusqlite::params![
                        streak,
                        notice.as_ref().and_then(|n| serde_json::to_string(n).ok()),
                        id_w
                    ],
                )?;
            }
            *slot_w.lock().expect("failure slot") = Some((streak, action, notice));
            Ok(WriteOutcome {
                applied: changed,
                events: if changed {
                    vec![PendingEvent {
                        entity_type: amux_core::revision::EntityType::Schedule,
                        entity_id: id_w.clone(),
                        mutation: amux_core::revision::MutationKind::Updated,
                        payload: None,
                    }]
                } else {
                    vec![]
                },
            })
        })
        .await?;
    let Some((streak, action, notice)) = slot.lock().expect("failure slot").take() else {
        return Ok(());
    };
    if let Verdict::Fail(f) = &verdict {
        if f.kind == "credential" {
            if matches!(action, Action::Notify(_)) {
                tracing::warn!(schedule = %id, worker = %worker, streak, cause = "expired_user_credential",
                    verdict = "schedule_run_credential_expired", measured = true, n_considered = 1,
                    "schedule failed on an expired user credential; one cause, not a new error");
            } else {
                tracing::info!(schedule = %id, worker = %worker, streak, cause = "expired_user_credential",
                    verdict = "schedule_run_credential_expired", measured = true, n_considered = 1,
                    "schedule failed on an expired user credential again");
            }
        }
    }
    // Before the dedupe below: a repeat of the same error is suppressed as a
    // notice, but it still counts toward pausing an orphaned schedule.
    if let (Verdict::Fail(f), Some(Route::Owner { cause })) = (&verdict, &route) {
        if should_pause_orphan(cause, streak) {
            let card = pause_orphan(store, &id, &worker, cause, streak, &f.error_line, now).await?;
            tracing::warn!(schedule = %id, worker = %worker, cause = %cause, streak, card = %card,
                verdict = "schedule_paused_owner_gone", measured = true, n_considered = 1,
                "schedule kept failing and its worker is gone; the harness paused it");
            if let Some(mut n) = notice {
                n.route = "paused".into();
                n.delivery = Some(if card.is_empty() { "paused".into() } else { card });
                store_notice(store, &id, &n).await?;
            }
            return Ok(());
        }
    }
    let f = match (&verdict, action) {
        (_, Action::Reset) => {
            tracing::info!(schedule = %id, worker = %worker, verdict = "schedule_failure_streak_reset",
                measured = true, n_considered = 1, "schedule passed; failure streak reset");
            return Ok(());
        }
        (_, Action::Suppress) => {
            tracing::info!(schedule = %id, recipient = %notice.as_ref().map(|n| n.worker.clone()).unwrap_or_default(),
                streak, last_notice_streak = notice.as_ref().map(|n| n.streak).unwrap_or(0),
                verdict = "schedule_failure_suppressed_dupe", measured = true, n_considered = 1,
                "same failure as the last notice; not re-sent until it changes or escalates");
            return Ok(());
        }
        (Verdict::Fail(f), Action::Notify(_)) => f.clone(),
        _ => return Ok(()),
    };
    let Some(mut notice) = notice else { return Ok(()) };
    let ran_at = now;
    let text = notice_text(sched, &f, streak, &notice.kind, source, ran_at);

    if notice.route == "worker" {
        match crate::api::session_verbs::steer_enqueue_store(store, &worker, &text, &guard_for(&id), SENDER).await {
            Ok(qid) => {
                tracing::info!(schedule = %id, recipient = %worker, streak, kind = %notice.kind,
                    queue_id = %qid, verdict = "schedule_failure_notified", measured = true, n_considered = 1,
                    "told the owning worker its schedule failed");
                notice.delivery = Some(qid);
                store_notice(store, &id, &notice).await?;
                return Ok(());
            }
            Err(why) => {
                notice.route = "owner".into();
                notice.reason = Some(format!("steering refused: {why}"));
            }
        }
    }
    let cause = notice.reason.clone().unwrap_or_default();
    let card = owner_card(store, sched, &worker, &cause, &text, now).await?;
    tracing::warn!(schedule = %id, recipient = "owner", worker = %worker, cause = %cause, streak,
        kind = %notice.kind, card = %card, verdict = "schedule_failure_routed_owner",
        measured = true, n_considered = 1,
        "schedule failed and its worker cannot be told; routed to the owner");
    notice.delivery = Some(card);
    store_notice(store, &id, &notice).await
}

/// Disable the schedule and close the harness's own open card for it with the
/// reason. Returns that card's id, or "" when there was none.
async fn pause_orphan(
    store: &SharedStore,
    id: &str,
    worker: &str,
    cause: &str,
    streak: i64,
    error_line: &str,
    now: i64,
) -> anyhow::Result<String> {
    let note = format!(
        "\n\n---\n{} Paused by the harness: {id} failed {streak} times in a row and its worker {worker} {}, \
         so nobody can repair it. Last error: {}. Re-enable it in Scheduler once it has a live worker.",
        fmt_ts(now),
        cause_sentence(cause),
        truncate(error_line, 200)
    );
    let (id_w, source_ref) = (id.to_string(), owner_card_ref(id));
    let (worker_w, cause_w) = (worker.to_string(), cause.to_string());
    let slot = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let slot_w = slot.clone();
    store
        .write_async(move |conn| {
            use rusqlite::OptionalExtension;
            // The 10-16 orphan pauses left no schedule_audit row (GS-12 review),
            // so the harness's own disable was the one change nobody could trace.
            let why = format!("paused by the harness: failed {streak} times in a row and its worker {worker_w} {}", cause_sentence(&cause_w));
            let was = conn.execute(
                "UPDATE schedules SET enabled=0, updated=?2, disabled_reason=?3 WHERE id=?1 AND enabled=1",
                rusqlite::params![id_w, now, why],
            )?;
            if was > 0 {
                super::scheduler::insert_audit(conn, &id_w, "enabled", "1", "0", "harness:schedule_failure", SENDER)?;
            }
            let open: Option<String> = conn
                .query_row(
                    "SELECT id FROM issues WHERE source_ref=?1 AND COALESCE(archived,0)=0 \
                     AND status NOT IN ('done','verified','discarded') ORDER BY rowid DESC LIMIT 1",
                    [&source_ref],
                    |r| r.get(0),
                )
                .optional()?;
            let mut events = vec![PendingEvent {
                entity_type: amux_core::revision::EntityType::Schedule,
                entity_id: id_w.clone(),
                mutation: amux_core::revision::MutationKind::Updated,
                payload: None,
            }];
            if let Some(card) = open {
                // The card asked "fix it, move it, or disable it?"; the harness
                // disabled it, so the question is answered.
                conn.execute(
                    "UPDATE issues SET \"desc\" = COALESCE(\"desc\",'') || ?1, status='done', updated=?2, \
                     rev = COALESCE(rev,0) + 1 WHERE id=?3",
                    rusqlite::params![note, now, card],
                )?;
                events.push(PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: card.clone(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: Some(json!({"id": card, "source": "schedule_failure"})),
                });
                *slot_w.lock().expect("card slot") = card;
            }
            Ok(WriteOutcome { applied: true, events })
        })
        .await?;
    let card = slot.lock().expect("card slot").clone();
    Ok(card)
}

async fn store_notice(store: &SharedStore, id: &str, n: &Notice) -> anyhow::Result<()> {
    let (id, raw) = (id.to_string(), serde_json::to_string(n)?);
    store
        .write_async(move |conn| {
            conn.execute("UPDATE schedules SET failure_notice=?1 WHERE id=?2", rusqlite::params![raw, id])?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await?;
    Ok(())
}

fn cause_sentence(cause: &str) -> String {
    match cause {
        "isolated" => "is isolated, so amux does not inject notices into it".into(),
        "archived" => "is archived".into(),
        "paused" => "is paused".into(),
        "stopped" => "is not running".into(),
        "unregistered" => "is not a registered worker".into(),
        "no_target" => "is not set (the schedule names no worker)".into(),
        other => format!("could not be told ({other})"),
    }
}

/// One open needsyou card per schedule; a later notice appends to it.
async fn owner_card(
    store: &SharedStore,
    sched: &DurableSchedule,
    worker: &str,
    cause: &str,
    text: &str,
    now: i64,
) -> anyhow::Result<String> {
    let id = sched.id().to_string();
    let who = if worker.is_empty() { "(none)".to_string() } else { worker.to_string() };
    let title = format!("{id} is failing and its worker {who} {}", cause_sentence(cause));
    let desc = format!("{title}.\n\nThe notice the worker would have received:\n\n{text}");
    let source_ref = owner_card_ref(&id);
    let new = crate::db::board_store::NewIssue {
        title: title.clone(),
        desc: desc.clone(),
        status: "needsyou".into(),
        session: None,
        item_type: "decision".into(),
        creator: SENDER.into(),
        owner_type: "human".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags: vec!["schedule-failure".into()],
        ask_type: Some("decision".into()),
        next_action: None,
        acceptance_criteria: None,
        ask_question: Some(format!(
            "{id} keeps failing and {who} cannot be told: fix it yourself, move the schedule to a live worker, or disable it?"
        )),
        ask_unblocks: Some(format!("{id} runs cleanly again, or stops running.")),
        ask_actor: Some(crate::api::turn_end::owner_name()),
        source: Some("schedule_failure".into()),
        requested_by: None,
        callback_session: None,
        callback_prompt: None,
    };
    let slot = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let slot_w = slot.clone();
    store
        .write_async(move |conn| {
            use rusqlite::OptionalExtension;
            let open: Option<String> = conn
                .query_row(
                    "SELECT id FROM issues WHERE source_ref=?1 AND COALESCE(archived,0)=0 \
                     AND status NOT IN ('done','verified','discarded') ORDER BY rowid DESC LIMIT 1",
                    [&source_ref],
                    |r| r.get(0),
                )
                .optional()?;
            let (card, mutation) = match open {
                Some(card) => {
                    conn.execute(
                        "UPDATE issues SET \"desc\" = COALESCE(\"desc\",'') || ?1, updated = ?2, rev = COALESCE(rev,0) + 1 WHERE id = ?3",
                        rusqlite::params![format!("\n\n---\n{}\n\n{text_a}", fmt_ts(now), text_a = desc), now, card],
                    )?;
                    (card, amux_core::revision::MutationKind::Updated)
                }
                None => {
                    let row = crate::db::board_store::create_issue(conn, &new, now)?;
                    conn.execute("UPDATE issues SET source_ref=?1 WHERE id=?2", rusqlite::params![source_ref, row.id])?;
                    (row.id, amux_core::revision::MutationKind::Created)
                }
            };
            *slot_w.lock().expect("card slot") = card.clone();
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: card.clone(),
                    mutation,
                    payload: Some(json!({"id": card, "source": "schedule_failure"})),
                }],
            })
        })
        .await?;
    let card = slot.lock().expect("card slot").clone();
    Ok(card)
}

/// `failure_notice` is stored as JSON text; expose it parsed on the schedule
/// object so a worker reading `GET /api/schedules/<id>` gets fields, not a string.
pub fn expose(v: &mut serde_json::Value) {
    if let Some(raw) = v.get("failure_notice").and_then(|x| x.as_str()).map(str::to_string) {
        v["failure_notice"] = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use crate::runtime_jobs::scheduler::{get_schedule, insert_schedule, ShellOutput};
    use serde_json::{Map, Value};
    use std::sync::Arc;

    struct Up(bool);
    #[async_trait::async_trait]
    impl WorkerProbe for Up {
        async fn running(&self, _name: &str) -> bool {
            self.0
        }
    }

    fn sched(id: &str, worker: &str) -> DurableSchedule {
        let mut m = Map::new();
        for (k, v) in [
            ("id", Value::from(id)),
            ("title", Value::from("Signup loop")),
            ("session", Value::from(worker)),
            ("command", Value::from("scripts/signup-check.sh --repair")),
            ("kind", Value::from("shell")),
            ("sched_type", Value::from("recurring")),
            ("schedule_expr", Value::from("daily at 9am")),
            ("next_run", Value::from("2099-01-01 09:00")),
            ("enabled", Value::from(1)),
            ("created", Value::from(1)),
            ("updated", Value::from(1)),
        ] {
            m.insert(k.into(), v);
        }
        DurableSchedule::from_map(m)
    }

    fn exit(code: i32, last: &str) -> RunOutcome {
        let tail: String = (1..=60).map(|i| format!("line {i}\n")).collect::<String>() + last;
        RunOutcome::ShellError {
            note: format!("exit {code}: {last}"),
            output: Some(ShellOutput { exit_code: code, tail }),
        }
    }

    fn ok() -> RunOutcome {
        RunOutcome::ShellOk { note: None, output: Some(ShellOutput { exit_code: 0, tail: String::new() }) }
    }

    struct Fx {
        _dir: tempfile::TempDir,
        db: std::path::PathBuf,
        store: SharedStore,
    }

    fn fixture(workers: &[(&str, &str)]) -> (Fx, crate::api::settings::test_env::HomeGuard) {
        let dir = tempfile::tempdir().unwrap();
        let g = crate::api::settings::test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::create_dir_all(dir.path().join("env")).unwrap();
        for (n, body) in workers {
            std::fs::write(dir.path().join(format!("sessions/{n}.env")), body).unwrap();
        }
        let db = dir.path().join("sf.db");
        let store: SharedStore = Arc::new(Store::open(&db).unwrap());
        (Fx { _dir: dir, db, store }, g)
    }

    async fn add(store: &SharedStore, s: DurableSchedule) {
        store
            .write_async(move |c| {
                insert_schedule(c, &s)?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
    }

    /// Queued notices for `worker`, drained so the next step counts only new ones.
    async fn take_queue(store: &SharedStore, worker: &str) -> Vec<(String, String)> {
        let w = worker.to_string();
        let w2 = w.clone();
        let rows = store
            .read_async(move |c| {
                let exists: bool = c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='steering_queue')",
                    [],
                    |r| r.get(0),
                )?;
                if !exists {
                    return Ok(vec![]);
                }
                let mut st = c.prepare("SELECT text, COALESCE(guard,'') FROM steering_queue WHERE session=?1")?;
                let v = st
                    .query_map([&w], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(v)
            })
            .await
            .unwrap();
        store
            .write_async(move |c| {
                let _ = c.execute("DELETE FROM steering_queue WHERE session=?1", [&w2]);
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        rows
    }

    async fn owner_cards(store: &SharedStore, id: &str) -> Vec<(String, String, String, String)> {
        let r = owner_card_ref(id);
        store
            .read_async(move |c| {
                let mut st = c.prepare("SELECT id, status, title, \"desc\" FROM issues WHERE source_ref=?1")?;
                let v = st
                    .query_map([&r], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(v)
            })
            .await
            .unwrap()
    }

    async fn state(store: &SharedStore, id: &str) -> (i64, Option<Notice>) {
        let id = id.to_string();
        store
            .read_async(move |c| {
                let s = get_schedule(c, &id)?.unwrap();
                let mut v = s.to_json();
                expose(&mut v);
                Ok((
                    v["failure_streak"].as_i64().unwrap_or(0),
                    serde_json::from_value(v["failure_notice"].clone()).ok(),
                ))
            })
            .await
            .unwrap()
    }

    fn fail(code: i32, line: &str) -> Verdict {
        Verdict::Fail(Failure {
            kind: "exit",
            exit_code: Some(code),
            error_line: line.into(),
            detail: String::new(),
            tail: String::new(),
        })
    }

    #[test]
    fn decide_dedupes_per_streak_and_escalates() {
        let f1 = fail(1, "1 repair(s) needed");
        let Verdict::Fail(ref ff) = f1 else { unreachable!() };
        let n = Notice {
            at: 0,
            streak: 1,
            kind: "first".into(),
            signature: ff.signature(),
            error: String::new(),
            route: "worker".into(),
            worker: "w".into(),
            reason: None,
            delivery: None,
            suppressed: 0,
        };
        assert_eq!(decide(0, None, &f1), (1, Action::Notify("first")));
        assert_eq!(decide(0, Some(&n), &f1), (1, Action::Notify("first")), "a new streak always notifies");
        assert_eq!(decide(1, Some(&n), &fail(1, "7 repair(s) needed")), (2, Action::Suppress),
            "a counter in the error is the same error");
        assert_eq!(decide(2, Some(&n), &f1), (3, Action::Suppress));
        assert_eq!(decide(3, Some(&n), &f1), (4, Action::Notify("escalation")));
        assert_eq!(decide(1, Some(&n), &fail(2, "boom")), (2, Action::Notify("changed")));
        assert_eq!(decide(3, Some(&n), &Verdict::Pass), (0, Action::Reset));
        assert_eq!(decide(0, None, &Verdict::Pass), (0, Action::Nothing));
        assert_eq!(decide(2, Some(&n), &Verdict::Neutral), (2, Action::Nothing));
        assert_eq!(
            classify(&RunOutcome::Refused { reason: "schedule PAUSED: the plan window is 80% used".into() }),
            Verdict::Neutral,
            "the background reserve is not a failure"
        );
        assert!(matches!(classify(&RunOutcome::Failed { reason: "timed out after 600s".into() }), Verdict::Fail(_)));
    }

    /// The whole lifecycle against a real store and the real steering queue:
    /// first failure tells the worker once, a repeat is suppressed, a changed
    /// error tells again, three more identical failures escalate, a restarted
    /// store does not re-notify, and a pass resets the streak.
    #[tokio::test]
    async fn a_failing_schedule_tells_its_worker_once_per_streak() {
        let (fx, _g) = fixture(&[("mixpeek-funnel", "CC_TAGS=plg\n")]);
        let store = fx.store.clone();
        add(&store, sched("SCHED-108", "mixpeek-funnel")).await;
        let s = sched("SCHED-108", "mixpeek-funnel");
        let up = Up(true);

        on_run_outcome_with(&store, &s, &exit(1, "1 repair(s) needed"), "cron-rs", &up).await;
        let q = take_queue(&store, "mixpeek-funnel").await;
        assert_eq!(q.len(), 1, "first failure tells the worker exactly once: {q:?}");
        let (text, guard) = &q[0];
        assert_eq!(guard, "sched-failure:SCHED-108");
        assert!(text.contains("SCHED-108") && text.contains("Signup loop"), "{text}");
        assert!(text.contains("POST /api/schedules/SCHED-108/run"), "rerun instruction: {text}");
        assert!(text.contains("1 repair(s) needed") && text.contains("line 60"), "output tail: {text}");
        assert!(!text.contains("line 20\n"), "tail is bounded to the last {TAIL_LINES} lines");
        assert!(!text.contains('\u{2014}'), "no em-dashes in the notice");
        let (streak, n) = state(&store, "SCHED-108").await;
        let n = n.expect("notice persisted");
        assert_eq!((streak, n.streak, n.route.as_str(), n.worker.as_str()), (1, 1, "worker", "mixpeek-funnel"));
        assert!(n.delivery.is_some());

        on_run_outcome_with(&store, &s, &exit(1, "2 repair(s) needed"), "cron-rs", &up).await;
        assert!(take_queue(&store, "mixpeek-funnel").await.is_empty(), "identical failure is suppressed");
        assert_eq!(state(&store, "SCHED-108").await.1.unwrap().suppressed, 1);

        on_run_outcome_with(&store, &s, &exit(2, "config file missing"), "cron-rs", &up).await;
        let q = take_queue(&store, "mixpeek-funnel").await;
        assert_eq!(q.len(), 1, "a changed error tells again");
        assert!(q[0].0.contains("NEW ERROR"), "{}", q[0].0);

        for _ in 0..2 {
            on_run_outcome_with(&store, &s, &exit(2, "config file missing"), "cron-rs", &up).await;
            assert!(take_queue(&store, "mixpeek-funnel").await.is_empty());
        }
        on_run_outcome_with(&store, &s, &exit(2, "config file missing"), "cron-rs", &up).await;
        let q = take_queue(&store, "mixpeek-funnel").await;
        assert_eq!(q.len(), 1, "three more identical failures escalate");
        assert!(q[0].0.contains("STILL FAILING") && q[0].0.contains("6 runs in a row"), "{}", q[0].0);

        // Restart: a new Store over the same file must remember the notice.
        drop(store);
        let store2: SharedStore = Arc::new(Store::open(&fx.db).unwrap());
        on_run_outcome_with(&store2, &s, &exit(2, "config file missing"), "cron-rs", &up).await;
        assert!(take_queue(&store2, "mixpeek-funnel").await.is_empty(), "restart does not re-notify");
        assert_eq!(state(&store2, "SCHED-108").await.0, 7);

        on_run_outcome_with(&store2, &s, &ok(), "cron-rs", &up).await;
        assert_eq!(state(&store2, "SCHED-108").await.0, 0, "a pass resets the streak");
        on_run_outcome_with(&store2, &s, &exit(2, "config file missing"), "cron-rs", &up).await;
        assert_eq!(take_queue(&store2, "mixpeek-funnel").await.len(), 1, "a new streak notifies again");
        assert!(owner_cards(&store2, "SCHED-108").await.is_empty(), "a reachable worker never pages the owner");
    }

    /// An isolated worker gets no amux-injected notice; the owner is told.
    #[tokio::test]
    async fn an_isolated_worker_is_not_injected_and_the_owner_is_told() {
        let (fx, _g) = fixture(&[("raw", "CC_TAGS=plg\nCC_ISOLATED=1\n")]);
        let store = fx.store.clone();
        add(&store, sched("SCHED-9", "raw")).await;
        on_run_outcome_with(&store, &sched("SCHED-9", "raw"), &exit(1, "boom"), "cron-rs", &Up(true)).await;
        assert!(take_queue(&store, "raw").await.is_empty(), "nothing queued into an isolated worker");
        let cards = owner_cards(&store, "SCHED-9").await;
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(cards[0].1, "needsyou");
        assert!(cards[0].2.contains("SCHED-9") && cards[0].2.contains("isolated"), "{}", cards[0].2);
        let n = state(&store, "SCHED-9").await.1.unwrap();
        assert_eq!((n.route.as_str(), n.reason.as_deref()), ("owner", Some("isolated")));
        assert_eq!(n.delivery.as_deref(), Some(cards[0].0.as_str()));
        // A changed error appends to the same open card rather than stacking.
        on_run_outcome_with(&store, &sched("SCHED-9", "raw"), &exit(3, "other"), "cron-rs", &Up(true)).await;
        let cards = owner_cards(&store, "SCHED-9").await;
        assert_eq!(cards.len(), 1, "one open owner card per schedule");
        assert!(cards[0].3.contains("other"), "{}", cards[0].3);
    }

    /// An archived worker is not woken or unarchived; the owner is told why.
    #[tokio::test]
    async fn an_archived_worker_routes_to_the_owner_and_stays_archived() {
        let (fx, _g) = fixture(&[("mixpeek-funnel", "CC_TAGS=plg\nCC_ARCHIVED=1\n")]);
        let store = fx.store.clone();
        add(&store, sched("SCHED-108", "mixpeek-funnel")).await;
        let refused = RunOutcome::Refused { reason: "target 'mixpeek-funnel' is archived".into() };
        on_run_outcome_with(&store, &sched("SCHED-108", "mixpeek-funnel"), &refused, "cron-rs", &Up(false)).await;
        assert!(take_queue(&store, "mixpeek-funnel").await.is_empty(), "nothing queued into an archived worker");
        let cards = owner_cards(&store, "SCHED-108").await;
        assert_eq!(cards.len(), 1);
        assert!(
            cards[0].2.contains("SCHED-108 is failing and its worker mixpeek-funnel is archived"),
            "{}",
            cards[0].2
        );
        let env = std::fs::read_to_string(crate::config::amux_home().join("sessions/mixpeek-funnel.env")).unwrap();
        assert!(env.contains("CC_ARCHIVED=1"), "the worker stays archived");
    }

    /// A schedule whose worker is archived is paused by the harness on its
    /// third consecutive failure, and the owner card it raised is closed with
    /// the reason (SCHED-640 had failed 572 times, 2026-10-15).
    #[tokio::test]
    async fn the_harness_pauses_a_schedule_whose_worker_is_gone() {
        let (fx, _g) = fixture(&[("gs12-compute", "CC_ARCHIVED=1\n")]);
        let store = fx.store.clone();
        add(&store, sched("SCHED-640", "gs12-compute")).await;
        let fail = exit(1, "ERROR: instance gs12-proof-2 was not found");
        for _ in 0..2 {
            on_run_outcome_with(&store, &sched("SCHED-640", "gs12-compute"), &fail, "cron-rs", &Up(false)).await;
        }
        let enabled = |store: SharedStore| async move {
            store.read().unwrap().query_row("SELECT enabled FROM schedules WHERE id='SCHED-640'", [], |r| r.get::<_, i64>(0)).unwrap()
        };
        assert_eq!(enabled(store.clone()).await, 1, "two failures: still enabled, the owner card is open");
        assert_eq!(owner_cards(&store, "SCHED-640").await.len(), 1);
        on_run_outcome_with(&store, &sched("SCHED-640", "gs12-compute"), &fail, "cron-rs", &Up(false)).await;
        assert_eq!(enabled(store.clone()).await, 0, "third failure: the harness paused it");
        let cards = owner_cards(&store, "SCHED-640").await;
        assert_eq!(cards.len(), 1, "no new card is raised");
        assert_eq!(cards[0].1, "done", "the one owner card is closed");
        let (status, desc): (String, String) = store.read().unwrap()
            .query_row("SELECT status, \"desc\" FROM issues WHERE source_ref='sched-failure:SCHED-640'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(status, "done");
        assert!(desc.contains("Paused by the harness") && desc.contains("instance gs12-proof-2 was not found"), "{desc}");
        assert!(!should_pause_orphan("stopped", 50), "a stopped worker can come back; it is not paused");
        assert!(!should_pause_orphan("isolated", 50));
    }

    /// The notice is system input about the worker's own schedule, so the
    /// worker-to-worker group boundary does not stop it. The boundary itself
    /// is unchanged: a worker-origin send across groups is still refused.
    #[tokio::test]
    async fn the_notice_crosses_no_group_boundary_and_the_boundary_still_holds() {
        let (fx, _g) = fixture(&[("mixpeek-funnel", "CC_TAGS=plg\n"), ("amux-app", "CC_TAGS=amux\n")]);
        let store = fx.store.clone();
        add(&store, sched("SCHED-108", "mixpeek-funnel")).await;
        on_run_outcome_with(&store, &sched("SCHED-108", "mixpeek-funnel"), &exit(1, "x"), "cron-rs", &Up(true)).await;
        assert_eq!(take_queue(&store, "mixpeek-funnel").await.len(), 1, "the scheduler's notice is delivered");
        let peer = crate::api::session_verbs::steer_enqueue_store(&store, "mixpeek-funnel", "hi", "", "amux-app").await;
        assert_eq!(peer, Err("worker_group_boundary"), "a worker-origin send across groups is still refused");
    }
}
