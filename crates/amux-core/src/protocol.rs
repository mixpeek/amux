//! Typed command/event protocol and pure command-queue semantics (RR-0006,
//! Invariants 5 and 34).
//!
//! Invariant 5 is the D1 exit: the terminal is an adapter, not the control
//! plane. The orchestrator speaks [`WorkerCommand`] down and [`WorkerEvent`]
//! up; whether an event came from a structured hook or a terminal scrape is
//! the adapter's secret, and the consumer never knows. Note
//! `DeliverMessage(MessageId)`, NOT `Steer(String)`: messages are durable
//! entities (Invariant 29), so the command carries a reference to the durable
//! row, never the payload — the Python system lost pending steering text on
//! every restart precisely because the text lived in the command.
//!
//! Invariant 34: every queue has an explicit delivery contract. Everything
//! here is pure — [`next_state`] is a function of (state, transition,
//! attempts), so the contract (Queued -> Dispatched -> Delivered ->
//! Confirmed; Failed from any pre-Confirmed state; retry until exhaustion,
//! then DeadLettered) is testable with no backend, no DB, and no clock. The
//! server persists [`QueuedCommand`] rows and drives them through this
//! machine; a dead letter is a system failure and gets a `DurableEvent`
//! (Invariant 24) at the persistence layer, not here.
//!
//! Per-worker event SEQUENCING (gap detection) is deliberately not on
//! [`WorkerEvent`]: causal ordering is a property of the event channel, so
//! the transport wraps events in its own envelope. Core defines what
//! happened, not how it travelled.

use crate::ids::{CommandId, MessageId, TaskId, TurnId, WorkerId};
use crate::provider::ProviderId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Commands (orchestrator -> worker)
// ---------------------------------------------------------------------------

/// Everything the orchestrator can ask a worker to do (Invariant 5). This is
/// the WHOLE control vocabulary — anything not expressible here goes through
/// a Message, not through a new side channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum WorkerCommand {
    /// Start work on a board task.
    ExecuteTask(TaskId),
    /// Resume/continue whatever the worker was doing.
    Continue,
    /// Deliver a durable message by reference (Invariant 29 — never the
    /// text itself; the text survives restarts in the message store).
    DeliverMessage(MessageId),
    /// Run verification for a task (Invariant 7: done != verified).
    Verify(TaskId),
    /// Review a task (peer review, distinct from verification).
    Review(TaskId),
    /// Stop current work. `DeliveryTiming::Immediate` — overrides turn
    /// boundaries.
    Cancel,
    /// Suspend the worker. Immediate.
    Pause,
    /// Unsuspend the worker. Immediate.
    Resume,
    /// Re-supply durable memories as a bounded prompt turn (#73). The
    /// command carries only the trigger reason (Invariant 29): visible
    /// memories are resolved live at delivery from the canonical store,
    /// so a queued resupply can never deliver a stale copy.
    MemoryResupply { reason: MemoryResupplyReason },
}

/// Why a memory re-supply was requested. Recorded on the command for
/// observability and idempotency scoping. Delivery behaves the same
/// for every reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryResupplyReason {
    /// Fresh agent start with no provider conversation to continue.
    SessionStart,
    /// Resume whose conversation ref was lost (restart, dropped ref).
    Resume,
    /// Re-assert durable facts after provider-managed compaction.
    PostCompaction,
}

// ---------------------------------------------------------------------------
// Events (worker -> orchestrator)
// ---------------------------------------------------------------------------

/// Everything a worker can tell the harness (Invariant 5). Hooks emit these
/// directly; scrapers infer them as a fallback — the consumer processes
/// `WorkerEvent`s either way, which is what lets scrapers shrink to liveness
/// checks as hook coverage grows (D1 exit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// The worker process is up.
    Started,
    /// A turn began (Invariant 6: the turn is first-class, so its start is
    /// an event, not a heuristic).
    TurnStarted { turn_id: TurnId },
    /// Mid-turn progress.
    Progress(ProgressReport),
    /// The worker yielded, waiting on something external.
    Waiting(WaitReason),
    /// A tool was invoked.
    ToolUsed(ToolEvent),
    /// Redacted, bounded transcript material for durable per-turn replay.
    /// The runtime applies redaction again at persistence time; producers do
    /// not get to assert that content is safe merely by constructing this.
    TraceObserved(TurnTrace),
    /// The worker mutated a task (claimed, advanced, annotated).
    TaskUpdated(TaskId),
    /// A turn ended.
    TurnCompleted(TurnResult),
    /// Provider rate limit observed (hook or scrape — provenance is the
    /// adapter's concern).
    RateLimited(RateLimit),
    /// Context window running low; payload is percent REMAINING (0-100).
    ContextLow(u8),
    /// The worker hit an error it could not absorb.
    Failed(Failure),
    /// The worker process exited.
    Exited(ExitStatus),
}

/// Mid-turn progress. Small on purpose: this is a heartbeat with a headline,
/// not a transcript — transcripts live in logs (Invariant 30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressReport {
    /// One-line, human-readable "what I'm doing".
    pub summary: String,
    /// Tokens consumed so far this turn, when the runtime reports it.
    pub tokens_used: Option<u64>,
}

/// Why a worker yielded. Free-form because wait causes are provider-shaped
/// and open-ended; the harness reacts to WAITING, not to the reason text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitReason {
    /// Machine-usable short cause ("permission_prompt", "user_input").
    pub reason: String,
    /// Optional human detail.
    pub detail: Option<String>,
}

/// A structured tool invocation (Invariant 30: structured events for
/// machines; the tool's raw output goes to logs, correlated by turn).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEvent {
    /// Tool name ("Bash", "Edit", an MCP tool id).
    pub tool: String,
    /// Short human-readable summary of the call, not the full payload.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTraceKind {
    Prompt,
    ProviderEvent,
    CommandOutput,
    SystemAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnTrace {
    pub turn_id: TurnId,
    pub kind: TurnTraceKind,
    pub content: String,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnResult {
    pub turn_id: TurnId,
    /// One-line outcome ("completed", "yielded: waiting on review").
    pub outcome: String,
}

/// A worker-level failure the harness should react to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub reason: String,
    /// Whether retrying the same work is plausible. The worker's claim, not
    /// the harness's conclusion.
    pub retryable: bool,
}

/// Process exit description. Both fields optional because backends differ in
/// what they can observe (Invariant 20's rule applied to exits: never invent
/// a code that was not reported).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitStatus {
    pub code: Option<i32>,
    /// Signal number, when killed by signal (unix backends).
    pub signal: Option<i32>,
}

/// A rate-limit observation. `raw` keeps the original provider text so a
/// wrong normalization is diagnosable from the data we kept (ethos rule 4)
/// — the API-error scraper was fixed twice in one day because the original
/// string was gone by the time anyone looked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    pub kind: RateLimitKind,
    /// When the limit lifts, if the provider said.
    pub reset_at: Option<DateTime<Utc>>,
    pub provider: ProviderId,
    /// The unparsed provider message this was normalized from.
    pub raw: Option<String>,
}

/// Which quota bucket tripped. `Unknown` is a first-class honest answer, not
/// an error: scrapes often see THAT a limit hit without seeing WHICH.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitKind {
    PerMinute,
    Daily,
    Weekly,
    SubscriptionCap,
    /// Credit/balance exhaustion (extra-usage credits, spend caps, 402
    /// budget errors). Distinct from time-window limits: it clears on a
    /// PAYMENT or balance change, not on a clock — the fleet's rate-limit
    /// handling must not schedule a wait for something a timer cannot fix
    /// (AF-14: a credit cap clears on its remedy).
    Credit,
    Unknown,
}

// ---------------------------------------------------------------------------
// Command queue (Invariant 34)
// ---------------------------------------------------------------------------

/// Lifecycle of a queued command. Linear on the happy path, with `Failed`
/// reachable from every pre-Confirmed state and `DeadLettered` the terminal
/// admission that the system wanted something to happen and it did not —
/// which is a reportable failure (dashboard badge + DurableEvent), never a
/// silent drop. The Python system's steering messages vanished on restart
/// with no trace; this enum is the shape of "no trace" being impossible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CommandState {
    /// Persisted, not yet handed to the agent protocol.
    Queued,
    /// Handed to the agent protocol; awaiting receipt acknowledgement.
    Dispatched,
    /// The agent acknowledged receipt.
    Delivered,
    /// A `WorkerEvent` confirmed the worker acted on it. Terminal.
    Confirmed,
    /// A delivery attempt failed. Recoverable via `Retry`.
    Failed { reason: String },
    /// Retries exhausted. Terminal; carries the last failure reason.
    DeadLettered { reason: String },
}

impl CommandState {
    /// Stable name for errors/logs.
    pub fn name(&self) -> &'static str {
        match self {
            CommandState::Queued => "queued",
            CommandState::Dispatched => "dispatched",
            CommandState::Delivered => "delivered",
            CommandState::Confirmed => "confirmed",
            CommandState::Failed { .. } => "failed",
            CommandState::DeadLettered { .. } => "dead_lettered",
        }
    }

    /// Terminal states accept no further transitions.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            CommandState::Confirmed | CommandState::DeadLettered { .. }
        )
    }
}

/// When a command may be handed to the worker (Invariant 34). Cancel/Pause/
/// Resume ride `Immediate`; messages and context refresh ride
/// `AtTurnBoundary` (Invariant 6: the boundary is a real event, not a
/// heuristic); `WhenIdle` keeps low-priority work from interrupting a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryTiming {
    Immediate,
    AtTurnBoundary,
    WhenIdle,
}

/// A freshness assertion revalidated AT DELIVERY TIME (Invariants 34/38). A
/// command can be true when queued and false when delivered — stale nudges
/// telling workers to act on work whose state had since changed were a whole
/// recurring bug family in the Python system. A failed precondition means
/// the command expires; it is never delivered anyway.
///
/// `entity` is an opaque key (an entity id string); the lookup closure maps
/// it to current `(version, status)`. Core stays pure — it asserts against
/// whatever state the caller can see, and an entity the lookup cannot find
/// fails the precondition (gone is the ultimate staleness).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum CommandPrecondition {
    /// The entity's version must still be exactly this (Invariant 35's
    /// entity version doing freshness duty).
    EntityVersion { entity: String, version: u64 },
    /// The entity must still be in this status.
    EntityStatus { entity: String, status: String },
    /// All sub-preconditions must hold.
    And(Vec<CommandPrecondition>),
}

impl CommandPrecondition {
    /// Evaluate against current state. `lookup` returns `(version, status)`
    /// for an entity key, or `None` if the entity no longer exists — which
    /// evaluates to `false` for every assertion form, because a command
    /// about a vanished entity is precisely the stale delivery this exists
    /// to stop.
    pub fn evaluate(&self, lookup: &dyn Fn(&str) -> Option<(u64, String)>) -> bool {
        match self {
            CommandPrecondition::EntityVersion { entity, version } => {
                matches!(lookup(entity), Some((v, _)) if v == *version)
            }
            CommandPrecondition::EntityStatus { entity, status } => {
                matches!(lookup(entity), Some((_, s)) if s == *status)
            }
            CommandPrecondition::And(parts) => parts.iter().all(|p| p.evaluate(lookup)),
        }
    }
}

/// A command in the per-worker queue. DB-backed at the persistence layer
/// because commands must survive restarts (Invariant 34); this struct is the
/// pure row. `idempotency_key` exists so the restart reconciliation loop can
/// re-enqueue safely: a duplicate key returns the existing command instead
/// of double-delivering.
///
/// Human-authored messages carry `precondition: None` and always deliver;
/// automation (nudges, advances, reassignment) carries assertions
/// (Invariant 38).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedCommand {
    pub id: CommandId,
    /// The worker this queue belongs to (queues are per-worker).
    pub worker: WorkerId,
    pub command: WorkerCommand,
    pub state: CommandState,
    /// Caller-chosen dedup key; duplicates return the existing result
    /// without re-dispatch.
    pub idempotency_key: String,
    /// When the command was enqueued. Supplied by the caller — core never
    /// reads a clock.
    pub queued_at: DateTime<Utc>,
    /// Failed delivery attempts so far (incremented on `Fail`).
    pub attempts: u32,
    pub timing: DeliveryTiming,
    pub precondition: Option<CommandPrecondition>,
}

impl QueuedCommand {
    /// Fresh command: `Queued`, zero attempts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: CommandId,
        worker: WorkerId,
        command: WorkerCommand,
        idempotency_key: String,
        queued_at: DateTime<Utc>,
        timing: DeliveryTiming,
        precondition: Option<CommandPrecondition>,
    ) -> Self {
        Self {
            id,
            worker,
            command,
            state: CommandState::Queued,
            idempotency_key,
            queued_at,
            attempts: 0,
            timing,
            precondition,
        }
    }

    /// Apply a transition, mutating `state` (and `attempts` on `Fail`).
    /// `max_attempts` is passed in, not stored: retry budget is queue
    /// policy, not a property of the command (default 3 per Invariant 34).
    pub fn apply(
        &mut self,
        transition: CommandTransition,
        max_attempts: u32,
    ) -> Result<(), CommandTransitionError> {
        let is_fail = matches!(transition, CommandTransition::Fail { .. });
        let next = next_state(&self.state, &transition, self.attempts, max_attempts)?;
        if is_fail {
            self.attempts += 1;
        }
        self.state = next;
        Ok(())
    }
}

/// The inputs to the command state machine. Split from [`CommandState`] so
/// the machine is driven by NAMED causes, not by callers assigning arbitrary
/// states — assignment is how illegal jumps (Queued -> Confirmed) happen
/// silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandTransition {
    /// Hand to the agent protocol.
    Dispatch,
    /// Agent acknowledged receipt.
    Deliver,
    /// WorkerEvent confirmed execution.
    Confirm,
    /// A delivery attempt failed.
    Fail { reason: String },
    /// Requeue a failed command — or dead-letter it when the retry budget
    /// is spent.
    Retry,
}

impl CommandTransition {
    pub fn name(&self) -> &'static str {
        match self {
            CommandTransition::Dispatch => "dispatch",
            CommandTransition::Deliver => "deliver",
            CommandTransition::Confirm => "confirm",
            CommandTransition::Fail { .. } => "fail",
            CommandTransition::Retry => "retry",
        }
    }
}

/// A transition the contract forbids. Carries both sides so the error is
/// diagnosable from itself (ethos rule 4) — "invalid transition" with no
/// operands is the kind of message that sends the reader to the wrong cause.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid command transition: {transition:?} from state {from:?}")]
pub struct CommandTransitionError {
    pub from: &'static str,
    pub transition: &'static str,
}

/// The pure command-queue state machine (Invariant 34). Enforces:
///
/// - `Queued -> Dispatched -> Delivered -> Confirmed` (happy path, in order)
/// - `Fail` from any pre-Confirmed state (`Queued`/`Dispatched`/`Delivered`)
/// - `Failed -> Queued` via `Retry` while `attempts < max_attempts`, else
///   `Failed -> DeadLettered` (carrying the last failure reason)
/// - `Confirmed` and `DeadLettered` are terminal
///
/// `attempts` is the count of failures already recorded (the caller's
/// [`QueuedCommand::apply`] increments it on `Fail`), so with
/// `max_attempts = 3` the third failure's `Retry` dead-letters.
pub fn next_state(
    state: &CommandState,
    transition: &CommandTransition,
    attempts: u32,
    max_attempts: u32,
) -> Result<CommandState, CommandTransitionError> {
    use CommandState as S;
    use CommandTransition as T;

    let invalid = || {
        Err(CommandTransitionError {
            from: state.name(),
            transition: transition.name(),
        })
    };

    match (state, transition) {
        (S::Queued, T::Dispatch) => Ok(S::Dispatched),
        (S::Dispatched, T::Deliver) => Ok(S::Delivered),
        (S::Delivered, T::Confirm) => Ok(S::Confirmed),
        // Failure is legal from every pre-Confirmed state: enqueue can fail
        // (worker gone), dispatch can fail (protocol down), delivered work
        // can fail to confirm (timeout).
        (S::Queued | S::Dispatched | S::Delivered, T::Fail { reason }) => Ok(S::Failed {
            reason: reason.clone(),
        }),
        (S::Failed { reason }, T::Retry) => {
            if attempts >= max_attempts {
                Ok(S::DeadLettered {
                    reason: reason.clone(),
                })
            } else {
                Ok(S::Queued)
            }
        }
        _ => invalid(),
    }
}

// ---------------------------------------------------------------------------
// FIFO ordering (Invariant 34: FIFO within a worker's queue)
// ---------------------------------------------------------------------------

/// FIFO comparison key: enqueue time, tie-broken by id. `CommandId` is a
/// ULID, so the tiebreak is itself creation-ordered — two commands minted in
/// the same instant still sort deterministically.
fn fifo_key(c: &QueuedCommand) -> (DateTime<Utc>, &CommandId) {
    (c.queued_at, &c.id)
}

/// Sort a queue into FIFO order in place.
pub fn fifo_sort(queue: &mut [QueuedCommand]) {
    queue.sort_by(|a, b| fifo_key(a).cmp(&fifo_key(b)));
}

/// The next command due for dispatch: the OLDEST still-`Queued` entry,
/// regardless of slice order. Commands already in flight (Dispatched/
/// Delivered) or terminal are skipped — FIFO governs dispatch order, it
/// does not block on in-flight completion.
pub fn next_queued(queue: &[QueuedCommand]) -> Option<&QueuedCommand> {
    queue
        .iter()
        .filter(|c| matches!(c.state, CommandState::Queued))
        .min_by(|a, b| fifo_key(a).cmp(&fifo_key(b)))
}

/// Idempotency lookup: the existing command for a key, if any. Enqueue paths
/// call this first and return the existing result instead of re-dispatching
/// (Invariant 34 — restart reconciliation replays enqueues, and this is what
/// makes the replay safe).
pub fn find_by_idempotency_key<'a>(
    queue: &'a [QueuedCommand],
    key: &str,
) -> Option<&'a QueuedCommand> {
    queue.iter().find(|c| c.idempotency_key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ulid(s: &str) -> ulid::Ulid {
        s.parse().unwrap()
    }

    fn t(secs: u32) -> DateTime<Utc> {
        chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, secs)
            .unwrap()
    }

    fn cmd(id_ulid: &str, key: &str, queued_at: DateTime<Utc>) -> QueuedCommand {
        QueuedCommand::new(
            CommandId::from_ulid(ulid(id_ulid)),
            WorkerId::from_ulid(ulid("01JGXV0000000000000000TEST")),
            WorkerCommand::Continue,
            key.to_string(),
            queued_at,
            DeliveryTiming::AtTurnBoundary,
            None,
        )
    }

    // -- state machine ------------------------------------------------------

    #[test]
    fn happy_path_queued_dispatched_delivered_confirmed() {
        let mut c = cmd("01JGXV0000000000000000AAAA", "k1", t(0));
        assert_eq!(c.state, CommandState::Queued);
        c.apply(CommandTransition::Dispatch, 3).unwrap();
        assert_eq!(c.state, CommandState::Dispatched);
        c.apply(CommandTransition::Deliver, 3).unwrap();
        assert_eq!(c.state, CommandState::Delivered);
        c.apply(CommandTransition::Confirm, 3).unwrap();
        assert_eq!(c.state, CommandState::Confirmed);
        assert!(c.state.is_terminal());
        assert_eq!(c.attempts, 0);
    }

    #[test]
    fn illegal_jumps_rejected() {
        // Skipping a stage is exactly what assigning states directly would
        // have allowed silently.
        let queued = CommandState::Queued;
        let err = next_state(&queued, &CommandTransition::Deliver, 0, 3).unwrap_err();
        assert_eq!(err.from, "queued");
        assert_eq!(err.transition, "deliver");
        assert!(next_state(&queued, &CommandTransition::Confirm, 0, 3).is_err());
        assert!(next_state(&CommandState::Dispatched, &CommandTransition::Confirm, 0, 3).is_err());
    }

    #[test]
    fn terminal_states_accept_nothing() {
        for terminal in [
            CommandState::Confirmed,
            CommandState::DeadLettered {
                reason: "gone".into(),
            },
        ] {
            for tr in [
                CommandTransition::Dispatch,
                CommandTransition::Deliver,
                CommandTransition::Confirm,
                CommandTransition::Fail {
                    reason: "x".into(),
                },
                CommandTransition::Retry,
            ] {
                assert!(
                    next_state(&terminal, &tr, 0, 3).is_err(),
                    "{} should reject {}",
                    terminal.name(),
                    tr.name()
                );
            }
        }
    }

    #[test]
    fn fail_reachable_from_every_pre_confirmed_state() {
        for from in [
            CommandState::Queued,
            CommandState::Dispatched,
            CommandState::Delivered,
        ] {
            let next = next_state(
                &from,
                &CommandTransition::Fail {
                    reason: "protocol down".into(),
                },
                0,
                3,
            )
            .unwrap();
            assert_eq!(
                next,
                CommandState::Failed {
                    reason: "protocol down".into()
                }
            );
        }
        // ...but not from Confirmed: a confirmed command cannot retro-fail.
        assert!(next_state(
            &CommandState::Confirmed,
            &CommandTransition::Fail { reason: "x".into() },
            0,
            3
        )
        .is_err());
    }

    #[test]
    fn dead_letter_on_retry_exhaustion() {
        // max_attempts = 3: fail/retry cycles requeue twice, the third
        // failure's retry dead-letters, keeping the last failure reason so
        // the dead letter is diagnosable from itself.
        let mut c = cmd("01JGXV0000000000000000AAAA", "k1", t(0));
        for round in 1..=3u32 {
            c.apply(CommandTransition::Dispatch, 3).unwrap();
            c.apply(
                CommandTransition::Fail {
                    reason: format!("attempt {round} timed out"),
                },
                3,
            )
            .unwrap();
            assert_eq!(c.attempts, round);
            c.apply(CommandTransition::Retry, 3).unwrap();
            if round < 3 {
                assert_eq!(c.state, CommandState::Queued, "round {round} should requeue");
            }
        }
        assert_eq!(
            c.state,
            CommandState::DeadLettered {
                reason: "attempt 3 timed out".into()
            }
        );
        assert!(c.state.is_terminal());
    }

    // -- FIFO ---------------------------------------------------------------

    #[test]
    fn fifo_orders_by_enqueue_time_then_id() {
        let a = cmd("01JGXV0000000000000000AAAA", "ka", t(2));
        let b = cmd("01JGXV0000000000000000BBBB", "kb", t(1));
        let c = cmd("01JGXV0000000000000000CCCC", "kc", t(1)); // same time as b -> id breaks tie
        let mut q = vec![a.clone(), c.clone(), b.clone()];
        fifo_sort(&mut q);
        assert_eq!(
            q.iter().map(|x| x.idempotency_key.as_str()).collect::<Vec<_>>(),
            vec!["kb", "kc", "ka"]
        );
    }

    #[test]
    fn next_queued_skips_in_flight_and_terminal() {
        let mut oldest = cmd("01JGXV0000000000000000AAAA", "ka", t(0));
        oldest.apply(CommandTransition::Dispatch, 3).unwrap(); // in flight
        let mut dead = cmd("01JGXV0000000000000000BBBB", "kb", t(1));
        dead.apply(
            CommandTransition::Fail {
                reason: "x".into(),
            },
            0, // budget already spent -> retry dead-letters
        )
        .unwrap();
        dead.apply(CommandTransition::Retry, 0).unwrap();
        assert!(matches!(dead.state, CommandState::DeadLettered { .. }));
        let ready = cmd("01JGXV0000000000000000CCCC", "kc", t(2));

        let q = vec![ready.clone(), dead, oldest];
        let next = next_queued(&q).unwrap();
        assert_eq!(next.idempotency_key, "kc");
    }

    #[test]
    fn next_queued_empty_when_nothing_pending() {
        let mut c = cmd("01JGXV0000000000000000AAAA", "ka", t(0));
        c.apply(CommandTransition::Dispatch, 3).unwrap();
        assert!(next_queued(&[c]).is_none());
        assert!(next_queued(&[]).is_none());
    }

    #[test]
    fn idempotency_key_finds_existing_command() {
        let a = cmd("01JGXV0000000000000000AAAA", "dup", t(0));
        let q = vec![a.clone()];
        // The enqueue path checks this before minting a new command, so a
        // replayed enqueue returns the existing row instead of re-dispatching.
        assert_eq!(find_by_idempotency_key(&q, "dup").unwrap().id, a.id);
        assert!(find_by_idempotency_key(&q, "fresh").is_none());
    }

    // -- preconditions ------------------------------------------------------

    #[test]
    fn precondition_evaluation() {
        // Current state: tsk-1 at version 17, status "review".
        let lookup = |entity: &str| -> Option<(u64, String)> {
            (entity == "tsk-1").then(|| (17u64, "review".to_string()))
        };

        let version_ok = CommandPrecondition::EntityVersion {
            entity: "tsk-1".into(),
            version: 17,
        };
        let version_stale = CommandPrecondition::EntityVersion {
            entity: "tsk-1".into(),
            version: 16,
        };
        let status_ok = CommandPrecondition::EntityStatus {
            entity: "tsk-1".into(),
            status: "review".into(),
        };
        let status_moved = CommandPrecondition::EntityStatus {
            entity: "tsk-1".into(),
            status: "verified".into(),
        };

        assert!(version_ok.evaluate(&lookup));
        assert!(!version_stale.evaluate(&lookup));
        assert!(status_ok.evaluate(&lookup));
        assert!(!status_moved.evaluate(&lookup));

        // And = all must hold.
        let both = CommandPrecondition::And(vec![version_ok.clone(), status_ok.clone()]);
        assert!(both.evaluate(&lookup));
        let mixed = CommandPrecondition::And(vec![version_ok, status_moved]);
        assert!(!mixed.evaluate(&lookup));
        // Vacuous And holds (no assertions = nothing stale).
        assert!(CommandPrecondition::And(vec![]).evaluate(&lookup));
    }

    #[test]
    fn precondition_fails_when_entity_gone() {
        // Entity deleted between queue and delivery: every assertion form
        // is false — a command about a vanished entity is the stale
        // delivery preconditions exist to stop (Invariant 38).
        let lookup = |_: &str| -> Option<(u64, String)> { None };
        assert!(!CommandPrecondition::EntityVersion {
            entity: "tsk-gone".into(),
            version: 1,
        }
        .evaluate(&lookup));
        assert!(!CommandPrecondition::EntityStatus {
            entity: "tsk-gone".into(),
            status: "todo".into(),
        }
        .evaluate(&lookup));
    }

    // -- serde shape --------------------------------------------------------

    #[test]
    fn command_and_event_serialize_tagged() {
        let cmd = WorkerCommand::DeliverMessage(MessageId::from_ulid(ulid(
            "01JGXV0000000000000000TEST",
        )));
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"kind\":\"deliver_message\""), "{json}");
        let back: WorkerCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);

        let ev = WorkerEvent::RateLimited(RateLimit {
            kind: RateLimitKind::Weekly,
            reset_at: Some(t(30)),
            provider: ProviderId::new("claude-code"),
            raw: Some("weekly limit reached".into()),
        });
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"kind\":\"rate_limited\""), "{json}");
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}
