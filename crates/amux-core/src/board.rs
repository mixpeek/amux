//! The board: canonical state of all work (RR-0005, RR-0011; Invariants 3, 4,
//! 18, 19).
//!
//! The board is not a visualization layer. It is the system of record for what
//! work exists, who owns it, and where it is in its lifecycle (Invariant 3).
//! Every status change goes through [`apply_transition`] — one function, one
//! code path, audited by construction. No work happens off-board.
//!
//! Three separations this module enforces by shape, each bought by a real
//! incident:
//!
//! - **Task state != execution state** (Invariant 19). Nothing here knows
//!   about sessions, rate limits, or crashes. A rate limit changes execution
//!   state, never board state.
//! - **Done != Verified** (Invariant 7). `Done` is a worker's claim; `Verified`
//!   is the harness's conclusion, reached through gates whose criteria are
//!   [`VerifierKind`]s. A failed verification returns the task to `Doing` —
//!   it revokes the claim, not the work.
//! - **Archive is a FLAG, not a status.** Archiving parks a card without
//!   destroying where it was in its lifecycle; restore brings it back exactly.
//!   (The is:armed incident chain, ethos rule 1, was archived cards silently
//!   dropping out of every surfacing mechanism — a flag the queries must
//!   consciously include is at least greppable; a status overwrite loses the
//!   card's actual state forever.)
//!
//! Gates are first-class entities (Invariant 18): a blocked transition returns
//! [`WhyBlocked`] — gate id, criterion, missing evidence, suggested command —
//! never an opaque "gate failed".

use crate::events::Actor;
use crate::ids::{CriterionId, GateId, TaskId, WorkerId};
use crate::scope::Scope;
use crate::stall::WaitingFor;
use crate::verification::{Evidence, EvidenceKind, VerifierKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Vocabulary: statuses, types, relations
// ---------------------------------------------------------------------------

/// Board status — the semantic, user-visible lifecycle (Invariant 19). This is
/// the status set the Python board actually uses (builtin columns plus the
/// live `needsyou`/`blocked` columns and the armed state for dormant cards),
/// carried over so migration is a rename, not a re-modeling.
///
/// `Archived` is deliberately NOT here: it is a flag on [`Task`], orthogonal
/// to status, so Archive/Restore round-trips preserve lifecycle position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Parked, deliberately NOT auto-claimed — triage is a human's call.
    Backlog,
    /// Queued and dispatchable.
    Todo,
    /// In progress.
    Doing,
    /// Awaiting review.
    Review,
    /// Stuck on the user — the exact question should be on the card.
    NeedsYou,
    /// Blocked on something structured (a dependency edge or an external
    /// condition). There is no `blocked_by_user` (Invariant 10).
    Blocked,
    /// The worker's CLAIM that the work is complete. Unverified (Invariant 7).
    Done,
    /// The harness's conclusion, with evidence. Terminal.
    Verified,
    /// Deliberately abandoned. Terminal.
    Discarded,
    /// A dormant tripwire/watch card waiting on its firing event. Never
    /// auto-picked; never silently invisible either — its disposition is a
    /// structured wait, not an exemption (the inert-watch incident, ethos
    /// rule 1: an exemption list with no surfacing mechanism is invisibility).
    Armed,
    /// Execution limits exhausted and decomposition failed twice
    /// (Invariant 47): the anti-livelock terminal. A quarantined task is
    /// counted in FleetProgress — terminal but never invisible.
    Quarantined,
}

impl TaskStatus {
    /// Every status, for totality tests and exhaustive UI enumeration.
    pub const ALL: [TaskStatus; 11] = [
        TaskStatus::Backlog,
        TaskStatus::Todo,
        TaskStatus::Doing,
        TaskStatus::Review,
        TaskStatus::NeedsYou,
        TaskStatus::Blocked,
        TaskStatus::Done,
        TaskStatus::Verified,
        TaskStatus::Discarded,
        TaskStatus::Armed,
        TaskStatus::Quarantined,
    ];

    /// Terminal STATUSES. Invariant 10's terminal set also includes archived
    /// (a flag here — see [`Task::archived`]).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Verified | TaskStatus::Discarded | TaskStatus::Quarantined
        )
    }
}

/// What kind of work a card is. Gates DERIVE from type (ethos rule 3): when a
/// gate does not fit, the honest fix is to correct the type, not to bypass the
/// gate — 1,143 of 1,215 open cards typed `code` is what forcing one type
/// produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemType {
    Code,
    Escalation,
    Blocker,
    Investigation,
    Ops,
    Research,
    Chore,
    Doc,
    Tripwire,
    Watch,
    /// A grouping container: other cards roll up under it via `issues.epic`
    /// (AMUX-2992, Ethan 2026-08-12 — "epic = a card, children link up"). Not a
    /// dormant type; its gate is the non-code default ("Outcome recorded"), an
    /// epic being done when its work is accounted for.
    Epic,
}

impl ItemType {
    /// Every variant, so predicates over the set can be DERIVED rather than
    /// hand-listed. The `is_dormant` comment below records what a re-typed
    /// literal already cost here once; this exists so the next one does not
    /// have to be written at all.
    pub const ALL: [ItemType; 11] = [
        ItemType::Code,
        ItemType::Escalation,
        ItemType::Blocker,
        ItemType::Investigation,
        ItemType::Ops,
        ItemType::Research,
        ItemType::Chore,
        ItemType::Doc,
        ItemType::Tripwire,
        ItemType::Watch,
        ItemType::Epic,
    ];

    /// The wire/DB spelling — the same snake_case serde emits, so a value
    /// built from this matches what is stored in `issues.type`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemType::Code => "code",
            ItemType::Escalation => "escalation",
            ItemType::Blocker => "blocker",
            ItemType::Investigation => "investigation",
            ItemType::Ops => "ops",
            ItemType::Research => "research",
            ItemType::Chore => "chore",
            ItemType::Doc => "doc",
            ItemType::Tripwire => "tripwire",
            ItemType::Watch => "watch",
            ItemType::Epic => "epic",
        }
    }

    /// Dormant types: cards that arm and wait for an event instead of being
    /// worked. Only these may take [`BoardTransition::Arm`]. Kept as ONE
    /// predicate so the exemption ("never auto-picked") and the surfacing
    /// mechanism (`TaskStatus::Armed` disposition) cannot drift apart — the
    /// Python system's `('tripwire','watch')` literal was re-typed in at
    /// least two places and the drift was an incident.
    pub fn is_dormant(&self) -> bool {
        matches!(self, ItemType::Tripwire | ItemType::Watch)
    }
}

/// Typed relations between tasks (Invariant 4). The dependency graph IS the
/// project plan: "runnable" is derived centrally from it, not from a flat
/// queue scan. `DependsOn` edges are denormalized onto [`Task::depends_on`]
/// for the graph helpers below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRelation {
    Blocks,
    DependsOn,
    RelatedTo,
    ParentOf,
    ChildOf,
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

/// A unit of work. ONE unit: something that can be honestly done or not done.
/// If no single state finishes it, it is not one card (the 451-folds incident,
/// ethos rule 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub desc: String,
    pub status: TaskStatus,
    /// The worker currently holding this card, if any. Assignment is board
    /// state; what that worker's session is DOING right now is execution
    /// state and lives elsewhere (Invariant 19).
    pub worker: Option<WorkerId>,
    pub item_type: ItemType,
    /// Who created the card. An `Actor`, not a string: unattributed writes are
    /// how eight schedules once vanished with zero forensic trail (AMUX-1812).
    pub creator: Actor,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Archive FLAG — orthogonal to status so Restore returns the card to
    /// exactly the lifecycle position it left.
    pub archived: bool,
    pub pinned: bool,
    /// Denormalized `DependsOn` edges. Kept acyclic — [`would_cycle`] rejects
    /// a circular edge at creation time.
    pub depends_on: Vec<TaskId>,
    /// Reviewer named via [`BoardTransition::RequestReview`]. Cards created
    /// without one were the unprotected-card bug (commit `aabcd9d`).
    pub reviewer: Option<Actor>,
    /// Per-task gate override: when set, the ONLY gate that applies to this
    /// task is the one with this id (if present in the effective set). The
    /// honest escape from an ill-fitting gate is still retyping the card;
    /// this exists for the cases where a scope's gate genuinely does not
    /// describe one specific card.
    pub gate_override: Option<GateId>,
    pub tags: Vec<String>,
    /// Entity version (Invariant 35). Bumped on every applied transition;
    /// no-op transitions are REJECTED (Invariant 37) so a bumped version
    /// always means something changed.
    pub version: u64,
}

impl Task {
    /// Construct a brand-new card. Creation is a constructor, not a
    /// transition: [`apply_transition`] operates on an existing task, so
    /// applying [`BoardTransition::Create`] to one returns
    /// [`TransitionError::CreateOnExisting`]. New cards land in `Todo` —
    /// callers park (`Park`) or arm (`Arm`) afterwards as needed.
    pub fn create(
        id: TaskId,
        title: impl Into<String>,
        item_type: ItemType,
        creator: Actor,
        now: DateTime<Utc>,
    ) -> Self {
        Task {
            id,
            title: title.into(),
            desc: String::new(),
            status: TaskStatus::Todo,
            worker: None,
            item_type,
            creator,
            created_at: now,
            updated_at: now,
            archived: false,
            pinned: false,
            depends_on: Vec::new(),
            reviewer: None,
            gate_override: None,
            tags: Vec::new(),
            version: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Gates (Invariant 18)
// ---------------------------------------------------------------------------

/// A first-class gate entity: criteria that must hold before a task may ENTER
/// the guarded status.
///
/// `scope` is carried for provenance and explainability — why-blocked names
/// which layer's gate blocked you. Assembling the effective, scope-resolved
/// gate set for a task is the caller's job via `crate::scope` (one resolver,
/// used everywhere); [`apply_transition`] receives gates already resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate {
    pub id: GateId,
    pub scope: Scope,
    /// The status this gate guards entry INTO (e.g. a gate on `Done` fires on
    /// `Complete`/`Approve`; a gate on `Verified` fires on `Verify`).
    pub guards: TaskStatus,
    /// Which item types this gate derives for; `None` = all types. This is
    /// how "gates derive from type" is expressed (RR-0011: gate derivation
    /// per (item_type, scope)): retyping a card changes which gates apply —
    /// the honest exit ethos rule 3 demands.
    pub applies_to_types: Option<Vec<ItemType>>,
    pub criteria: Vec<GateCriterion>,
}

/// One criterion inside a gate. The verifier is the unified
/// [`VerifierKind`] (Invariant 18: "GateEvaluator is now VerifierKind") —
/// the same spec verification uses, so there is exactly one evaluation path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateCriterion {
    pub description: String,
    pub verifier: VerifierKind,
    /// Required criteria block the transition when unsatisfied; optional ones
    /// inform but never block (and never short-circuit a verification run).
    pub required: bool,
}

impl GateCriterion {
    /// Whether submitted evidence satisfies this criterion, by SHAPE: the
    /// evidence set contains an artifact of the kind this criterion's
    /// verifier produces. Core is pure — it cannot re-run the verifier — so
    /// the contract is: the gate runner executed [`VerifierKind`]s and minted
    /// [`Evidence`] for the ones that passed; core checks the trail is
    /// present. An empty evidence list therefore CAN fail this check (ethos
    /// rule 7: a check that cannot fail is theatre).
    pub fn satisfied_by(&self, evidence: &[Evidence]) -> bool {
        let needed = self.verifier.evidence_kind();
        evidence.iter().any(|e| e.kind == needed)
    }
}

/// One line of the `why-blocked` answer (Invariant 18). Everything a blocked
/// caller needs to move honestly: which gate, which criterion, what evidence
/// is missing, and the command that would produce it. Never an opaque
/// "gate failed" — and never a sanctioned instruction that cannot actually be
/// executed (AMUX-2140: the documented escape must be walkable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhyBlocked {
    pub gate: GateId,
    /// The criterion's description, verbatim.
    pub criterion: String,
    /// The evidence kind that was not found in the submission.
    pub missing: EvidenceKind,
    /// One-line command to produce the missing evidence, when one exists.
    pub suggested_command: Option<String>,
}

/// The gates that apply to `task` entering `target`: guard status matches,
/// item type derives (ethos rule 3), and a per-task override — when set —
/// narrows the set to exactly that gate.
pub fn applicable_gates<'a>(gates: &'a [Gate], task: &Task, target: TaskStatus) -> Vec<&'a Gate> {
    gates
        .iter()
        .filter(|g| {
            if let Some(ov) = &task.gate_override {
                if g.id != *ov {
                    return false;
                }
            }
            if g.guards != target {
                return false;
            }
            match &g.applies_to_types {
                None => true,
                Some(types) => types.contains(&task.item_type),
            }
        })
        .collect()
}

/// The `why-blocked` query (Invariant 18): every required, unsatisfied
/// criterion across the gates guarding `target`, with the missing evidence
/// and the suggested command. Empty means the transition is not gate-blocked.
///
/// This is the SAME function [`apply_transition`] uses to refuse a gated
/// transition — the query and the enforcement cannot disagree, because a view
/// must share the predicate of the mechanism it claims to describe (ethos
/// rule 1, the five-filters night).
pub fn why_blocked(
    task: &Task,
    target: TaskStatus,
    effective_gates: &[Gate],
    evidence: &[Evidence],
) -> Vec<WhyBlocked> {
    let mut out = Vec::new();
    for gate in applicable_gates(effective_gates, task, target) {
        for criterion in &gate.criteria {
            if criterion.required && !criterion.satisfied_by(evidence) {
                out.push(WhyBlocked {
                    gate: gate.id.clone(),
                    criterion: criterion.description.clone(),
                    missing: criterion.verifier.evidence_kind(),
                    suggested_command: criterion.verifier.suggested_command(),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Transitions (Invariant 3)
// ---------------------------------------------------------------------------

/// Every way a task moves. The plan's Invariant 3 list, extended with the
/// transitions the canonical status set requires — `NeedsYou`, `Blocked` and
/// `Armed` would otherwise be reachable only through `Force`, and a state
/// with no honest path in is a state the model will lie its way into
/// (ethos rule 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoardTransition {
    /// Present for protocol/audit completeness; creation itself is
    /// [`Task::create`] (a transition needs an existing task to apply to).
    Create { title: String, item_type: ItemType },
    /// Backlog -> Todo: triaged into the dispatchable queue.
    Queue,
    /// Todo -> Backlog: parked. Backlog is deliberately not auto-claimed.
    Park,
    /// Todo -> Todo, assigning the worker. Claiming someone else's claimed
    /// card is refused — claims are atomic and exclusive (Invariant 3).
    Claim { worker: WorkerId },
    /// Unassign / requeue: Todo|Doing -> Todo, clearing the worker (e.g. a
    /// lease reclaim after a crash — the board sees the requeue, never the
    /// crash itself, per Invariant 19).
    Release,
    /// Todo -> Doing. A worker actor starting an unassigned card implicitly
    /// claims it (start-is-claim keeps the honest path one step, not two).
    Start,
    /// Doing -> Review, no named reviewer.
    Submit,
    /// Doing -> Review, naming the reviewer.
    RequestReview { reviewer: Actor },
    /// Review -> Done. Carries the reviewer's evidence because entry into
    /// `Done` may be gated and a gated transition with no way to present
    /// evidence is unsatisfiable (the field defaults empty on the wire).
    Approve {
        #[serde(default)]
        evidence: Vec<Evidence>,
    },
    /// Review -> Doing: back to work, with the reason.
    Reject { reason: String },
    /// Doing -> Done: the worker's claim, with evidence (Invariant 7).
    Complete { evidence: Vec<Evidence> },
    /// Done -> Verified: the harness's conclusion (Invariant 7). `criteria`
    /// are the acceptance-criterion ids the verification covered; the store
    /// links the full `Verification` record.
    Verify {
        criteria: Vec<CriterionId>,
        evidence: Vec<Evidence>,
    },
    /// Done -> Doing: verification failed, the claim is revoked
    /// (`VerificationResult::Failed` returns the task to in-progress).
    VerificationFailed { reason: String },
    /// Doing -> NeedsYou: stuck on the user, with the exact question.
    RequestInput { question: String },
    /// NeedsYou -> Doing: the user answered.
    Resume,
    /// Todo|Doing -> Blocked, with a structured reason.
    Block { reason: String },
    /// Blocked -> Todo: re-enters the queue.
    Unblock,
    /// Todo|Backlog -> Armed. Only dormant types (tripwire/watch) arm.
    Arm,
    /// Armed -> Todo: the watched event happened; the card is live work now.
    Fire { reason: String },
    /// Any non-terminal -> Discarded. Verified work cannot be discarded —
    /// it is history; archive it instead.
    Discard { reason: String },
    /// Anti-livelock terminal (Invariant 47): produced by the
    /// orchestrator when ExecutionLimits are exhausted AND decomposition
    /// failed twice. Never a human verb — humans discard.
    Quarantine { reason: String },
    /// The audited bypass: any status -> any status, gates skipped. Tolerable
    /// ONLY because judgment stays with a named actor: the store MUST persist
    /// a `DurableEvent` carrying the actor and reason. The Python board
    /// advertised force-is-logged in two places while nothing logged it
    /// (ethos rule 6) — do not repeat that; `apply_transition`'s `actor`
    /// parameter exists so no call site can even reach a force without
    /// naming who forced.
    Force { status: TaskStatus, reason: String },
    /// Set the archived flag; status untouched.
    Archive { reason: String },
    /// Clear the archived flag; the card resumes exactly where it was.
    Restore { reason: String },
}

/// Why a transition was refused. Serializable because these ARE the API error
/// bodies (the 409 shape) — a well-designed refusal teaches the caller what
/// to do next (Invariant 18), on the sanctioned path (AMUX-2325).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransitionError {
    #[error("cannot {action} from {from:?}: {reason}")]
    InvalidTransition {
        from: TaskStatus,
        action: String,
        reason: String,
    },
    /// The full why-blocked answer rides in the refusal — the caller never
    /// needs a second query to learn what to do.
    #[error("blocked by {} unmet gate criterion(s)", .blocked.len())]
    GateBlocked { blocked: Vec<WhyBlocked> },
    #[error("task is archived; restore it first")]
    ArchivedTaskImmutable,
    /// The transition would change nothing. Refused rather than applied so a
    /// bumped version always means a real change (Invariants 35/37: no-op
    /// mutations do not increment version).
    #[error("transition changes nothing (no-op mutations must not bump version)")]
    NoOp,
    #[error("create applies to no existing task; use Task::create")]
    CreateOnExisting,
    #[error("only tripwire/watch cards arm (this card is {item_type:?}); retype it if the shape does not fit")]
    NotArmable { item_type: ItemType },
    #[error("task {task} is already claimed by {holder}")]
    AlreadyClaimed { task: TaskId, holder: WorkerId },
}

/// Clone-and-mutate helper: every applied transition bumps the version and
/// stamps `updated_at` in exactly one place.
fn finish(task: &Task, now: DateTime<Utc>, mutate: impl FnOnce(&mut Task)) -> Task {
    let mut t = task.clone();
    mutate(&mut t);
    t.version += 1;
    t.updated_at = now;
    t
}

/// Gate check for entering `target`. Uniform across ALL status-changing
/// transitions — any status may be guarded, and there is exactly one
/// enforcement path (Invariant 3). Staying in place is not an entry, so
/// same-status mutations (Claim, Archive) skip it.
fn gate_check(
    task: &Task,
    target: TaskStatus,
    gates: &[Gate],
    evidence: &[Evidence],
) -> Result<(), TransitionError> {
    if target == task.status {
        return Ok(());
    }
    let blocked = why_blocked(task, target, gates, evidence);
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(TransitionError::GateBlocked { blocked })
    }
}

/// The board's transactional state machine (Invariant 3): one function, one
/// code path. Pure — the caller supplies `now`, persists the returned task,
/// and emits the `DurableEvent` attributing the change to `actor`.
///
/// Rules enforced here:
/// - archived tasks are immutable except `Restore` (`Archive` again is a
///   no-op refusal);
/// - gates guard ENTRY into a status, checked with the evidence the
///   transition carries; `Force` bypasses gates but never attribution;
/// - no-ops are refused, so version bumps always mean change (Invariant 37);
/// - task state never encodes execution state (Invariant 19): there is no
///   transition for "rate limited" or "crashed" on purpose.
pub fn apply_transition(
    task: &Task,
    transition: BoardTransition,
    actor: &Actor,
    effective_gates: &[Gate],
    now: DateTime<Utc>,
) -> Result<Task, TransitionError> {
    use BoardTransition as T;
    use TaskStatus as S;

    // Archived cards are frozen: Restore is the only way forward, and a
    // second Archive is a refused no-op (not an error class of its own —
    // nothing would change).
    if task.archived {
        return match &transition {
            T::Restore { .. } => Ok(finish(task, now, |t| t.archived = false)),
            T::Archive { .. } => Err(TransitionError::NoOp),
            _ => Err(TransitionError::ArchivedTaskImmutable),
        };
    }

    let invalid = |action: &str, reason: &str| TransitionError::InvalidTransition {
        from: task.status,
        action: action.to_string(),
        reason: reason.to_string(),
    };

    match transition {
        T::Create { .. } => Err(TransitionError::CreateOnExisting),

        T::Queue => match task.status {
            S::Backlog => {
                gate_check(task, S::Todo, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Todo))
            }
            _ => Err(invalid("queue", "only a backlog card queues into todo")),
        },

        T::Park => match task.status {
            S::Todo => {
                gate_check(task, S::Backlog, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Backlog))
            }
            _ => Err(invalid("park", "only queued (todo) work parks to backlog")),
        },

        T::Claim { worker } => match task.status {
            S::Todo => match &task.worker {
                Some(holder) if *holder == worker => Err(TransitionError::NoOp),
                Some(holder) => Err(TransitionError::AlreadyClaimed {
                    task: task.id.clone(),
                    holder: holder.clone(),
                }),
                None => Ok(finish(task, now, |t| t.worker = Some(worker))),
            },
            _ => Err(invalid("claim", "claiming is for queued (todo) work")),
        },

        T::Release => match (task.status, &task.worker) {
            (S::Todo, None) => Err(TransitionError::NoOp),
            (S::Todo, Some(_)) => Ok(finish(task, now, |t| t.worker = None)),
            (S::Doing, _) => {
                gate_check(task, S::Todo, effective_gates, &[])?;
                Ok(finish(task, now, |t| {
                    t.status = S::Todo;
                    t.worker = None;
                }))
            }
            _ => Err(invalid(
                "release",
                "only todo/doing work releases back to the queue",
            )),
        },

        T::Start => match task.status {
            S::Todo => {
                // A worker may not start a card another worker holds.
                if let (Some(holder), Actor::Worker { id }) = (&task.worker, actor) {
                    if holder != id {
                        return Err(TransitionError::AlreadyClaimed {
                            task: task.id.clone(),
                            holder: holder.clone(),
                        });
                    }
                }
                gate_check(task, S::Doing, effective_gates, &[])?;
                Ok(finish(task, now, |t| {
                    t.status = S::Doing;
                    // Start-is-claim: a worker starting unassigned work takes it.
                    if t.worker.is_none() {
                        if let Actor::Worker { id } = actor {
                            t.worker = Some(id.clone());
                        }
                    }
                }))
            }
            _ => Err(invalid(
                "start",
                "work starts from todo (resume, unblock and fire have their own paths)",
            )),
        },

        T::Submit => match task.status {
            S::Doing => {
                gate_check(task, S::Review, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Review))
            }
            _ => Err(invalid(
                "submit",
                "only in-progress (doing) work submits for review",
            )),
        },

        T::RequestReview { reviewer } => match task.status {
            S::Doing => {
                gate_check(task, S::Review, effective_gates, &[])?;
                Ok(finish(task, now, |t| {
                    t.status = S::Review;
                    t.reviewer = Some(reviewer);
                }))
            }
            _ => Err(invalid(
                "request_review",
                "only in-progress (doing) work goes to review",
            )),
        },

        T::Approve { evidence } => match task.status {
            S::Review => {
                gate_check(task, S::Done, effective_gates, &evidence)?;
                Ok(finish(task, now, |t| t.status = S::Done))
            }
            _ => Err(invalid("approve", "approval is the exit from review only")),
        },

        T::Reject { .. } => match task.status {
            S::Review => Ok(finish(task, now, |t| t.status = S::Doing)),
            _ => Err(invalid("reject", "rejection is the exit from review only")),
        },

        T::Complete { evidence } => match task.status {
            S::Doing => {
                gate_check(task, S::Done, effective_gates, &evidence)?;
                Ok(finish(task, now, |t| t.status = S::Done))
            }
            _ => Err(invalid(
                "complete",
                "done is claimed from doing (review exits via approve)",
            )),
        },

        T::Verify { evidence, .. } => match task.status {
            S::Done => {
                gate_check(task, S::Verified, effective_gates, &evidence)?;
                Ok(finish(task, now, |t| t.status = S::Verified))
            }
            _ => Err(invalid(
                "verify",
                "only done work verifies — done is the claim, verified is the conclusion",
            )),
        },

        T::VerificationFailed { .. } => match task.status {
            // Invariant 7: a failed verification returns the task to
            // in-progress. The claim is revoked, not the work.
            S::Done => Ok(finish(task, now, |t| t.status = S::Doing)),
            _ => Err(invalid(
                "verification_failed",
                "only a done claim can fail verification",
            )),
        },

        T::RequestInput { .. } => match task.status {
            S::Doing => {
                gate_check(task, S::NeedsYou, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::NeedsYou))
            }
            S::NeedsYou => Err(TransitionError::NoOp),
            _ => Err(invalid(
                "request_input",
                "only in-progress work can wait on the user",
            )),
        },

        T::Resume => match task.status {
            S::NeedsYou => {
                gate_check(task, S::Doing, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Doing))
            }
            _ => Err(invalid("resume", "resume answers a needs_you card only")),
        },

        T::Block { .. } => match task.status {
            S::Todo | S::Doing => {
                gate_check(task, S::Blocked, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Blocked))
            }
            S::Blocked => Err(TransitionError::NoOp),
            _ => Err(invalid("block", "only queued or in-progress work blocks")),
        },

        T::Unblock => match task.status {
            S::Blocked => {
                gate_check(task, S::Todo, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Todo))
            }
            _ => Err(invalid("unblock", "only a blocked card unblocks")),
        },

        T::Arm => {
            if !task.item_type.is_dormant() {
                // Ethos rule 3: the exit is retyping the card, not forcing.
                return Err(TransitionError::NotArmable {
                    item_type: task.item_type,
                });
            }
            match task.status {
                S::Todo | S::Backlog => {
                    gate_check(task, S::Armed, effective_gates, &[])?;
                    Ok(finish(task, now, |t| t.status = S::Armed))
                }
                S::Armed => Err(TransitionError::NoOp),
                _ => Err(invalid("arm", "cards arm from todo or backlog")),
            }
        }

        T::Fire { .. } => match task.status {
            S::Armed => {
                gate_check(task, S::Todo, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Todo))
            }
            _ => Err(invalid("fire", "only an armed card fires")),
        },

        T::Discard { .. } => match task.status {
            S::Verified => Err(invalid(
                "discard",
                "verified work is history; archive it instead",
            )),
            S::Discarded => Err(TransitionError::NoOp),
            _ => {
                // Uniform gate check: a gate CAN guard `Discarded` — the
                // ts-gke incident was a live watch card force-discarded by an
                // unattributed caller precisely because todo->discarded had
                // no gate in its path (ethos rule 7).
                gate_check(task, S::Discarded, effective_gates, &[])?;
                Ok(finish(task, now, |t| t.status = S::Discarded))
            }
        },

        T::Quarantine { .. } => match task.status {
            s if s.is_terminal() => Err(invalid(
                "quarantine",
                "already terminal — quarantine marks exhausted LIVE work",
            )),
            _ => Ok(finish(task, now, |t| t.status = S::Quarantined)),
        },

        T::Force { status, .. } => {
            if status == task.status {
                return Err(TransitionError::NoOp);
            }
            // Gates skipped, attribution not: the caller MUST emit the
            // DurableEvent with `actor` and the reason (ethos rule 6 — an
            // unaudited bypass that claims to be audited is worse than an
            // honest one).
            Ok(finish(task, now, |t| t.status = status))
        }

        T::Archive { .. } => {
            // task.archived is false here (the prelude handled true).
            Ok(finish(task, now, |t| t.archived = true))
        }

        T::Restore { .. } => {
            // Not archived (prelude handled the archived case): nothing to do.
            Err(TransitionError::NoOp)
        }
    }
}

// ---------------------------------------------------------------------------
// Disposition (Invariant 10) and the dependency graph (Invariant 4)
// ---------------------------------------------------------------------------

/// Exactly one of these for every task — the no-stall guarantee's foundation
/// (Invariant 10): "nothing is driving this" is an impossible state, not a
/// thing the stall detector discovers afterward.
///
/// Adjacently tagged (`kind`/`detail`): `Waiting` is a newtype variant
/// wrapping the sibling `WaitingFor` enum, which internal tagging cannot
/// nest reliably — adjacent tagging is the convention's sanctioned fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum TaskDisposition {
    /// Can be picked up now.
    Runnable,
    /// Someone is working on it.
    Assigned { worker: WorkerId },
    /// Blocked, with a structured reason a dashboard can show inline.
    Waiting(WaitingFor),
    /// Nothing left to drive. (The task itself says why: terminal status or
    /// the archived flag.)
    Terminal,
}

/// The first dependency of `task` not yet satisfied, scanning `depends_on` in
/// order. A dependency is met when its task is `Done` or `Verified`; a
/// dependency that cannot be FOUND in `board` is conservatively unmet — an
/// absent row proves nothing (the empty-grep lesson, ethos rule 7: a probe
/// that could not have found the thing is not evidence). Pass the full board.
///
/// A `Discarded` dependency is deliberately unmet: the dependent card then
/// waits, visibly, on a card someone abandoned — which is a question for a
/// human, not something to silently wave through.
pub fn first_unmet_dependency(task: &Task, board: &[Task]) -> Option<TaskId> {
    task.depends_on
        .iter()
        .find(|dep| match board.iter().find(|t| &&t.id == dep) {
            Some(d) => !matches!(d.status, TaskStatus::Done | TaskStatus::Verified),
            None => true,
        })
        .cloned()
}

/// TOTAL mapping from task to disposition (Invariant 10): every task resolves
/// to exactly one variant, enforced by the exhaustive match — a status added
/// to [`TaskStatus`] without a disposition is a compile error, not a runtime
/// discovery.
///
/// `effective_gates` lets a `Done` card name the gate its verification is
/// waiting on; pass `&[]` when gate context is unavailable and an ungated
/// done card reads as `Runnable` (its next action — verification — can run
/// immediately; the plan is executable, Invariant 45).
pub fn disposition(task: &Task, board: &[Task], effective_gates: &[Gate]) -> TaskDisposition {
    // Archived is terminal regardless of status (Invariant 10's terminal set).
    if task.archived {
        return TaskDisposition::Terminal;
    }
    match task.status {
        TaskStatus::Verified | TaskStatus::Discarded | TaskStatus::Quarantined => {
            TaskDisposition::Terminal
        }

        // Parked awaiting human triage — deliberately not auto-claimed, so
        // the named next actor is the user (ethos rule 8: triage is theirs).
        TaskStatus::Backlog => TaskDisposition::Waiting(WaitingFor::User),

        TaskStatus::Todo => {
            if let Some(dep) = first_unmet_dependency(task, board) {
                TaskDisposition::Waiting(WaitingFor::Dependency { on: dep })
            } else {
                // An OWNER on a todo card is a routing constraint (queued
                // for that worker), not an execution claim — the lease is
                // the claim (Invariant 19: task state != execution state).
                // Treating owned-todo as Assigned made every tagged queue
                // card invisible to the orchestrator, which is exactly the
                // Python board's L3 shape: 380 todos nobody was told to run.
                TaskDisposition::Runnable
            }
        }

        TaskStatus::Doing => match &task.worker {
            Some(worker) => TaskDisposition::Assigned {
                worker: worker.clone(),
            },
            // In-flight work with no worker is the HUMAN's own in-flight work
            // (the 21-cards lesson, ethos rule 8) — never auto-swept, waiting
            // on its person.
            None => TaskDisposition::Waiting(WaitingFor::User),
        },

        // Review and needs_you both wait on a person's next move. (When the
        // sibling WaitingFor grows a variant that can name a worker-reviewer,
        // Review should use it; User is the honest nearest cell today.)
        TaskStatus::Review | TaskStatus::NeedsYou => TaskDisposition::Waiting(WaitingFor::User),

        TaskStatus::Blocked => {
            if let Some(dep) = first_unmet_dependency(task, board) {
                TaskDisposition::Waiting(WaitingFor::Dependency { on: dep })
            } else {
                TaskDisposition::Waiting(WaitingFor::ExternalCondition {
                    desc: "blocked with no recorded dependency; add a depends_on edge or unblock"
                        .to_string(),
                })
            }
        }

        // Done -> Verified is the next action. If a gate guards Verified, the
        // card is waiting on that gate (why_blocked names the criteria);
        // ungated, verification itself is immediately runnable (Invariant 7).
        TaskStatus::Done => {
            match applicable_gates(effective_gates, task, TaskStatus::Verified).first() {
                Some(gate) => TaskDisposition::Waiting(WaitingFor::Gate {
                    gate: gate.id.clone(),
                }),
                None => TaskDisposition::Runnable,
            }
        }

        // An armed card waits on its firing event — a structured wait, NOT an
        // exemption. This is what keeps a dormant watch visible to the stall
        // machinery instead of findable only by scrolling past it (the inert
        // watch incident, ethos rule 1).
        TaskStatus::Armed => TaskDisposition::Waiting(WaitingFor::ExternalCondition {
            desc: format!(
                "armed {:?} card: waiting on its firing event (never auto-picked)",
                task.item_type
            ),
        }),
    }
}

/// Runnable now, derived centrally from the dependency graph (Invariant 4):
/// this is literally "disposition == Runnable" over the board — the view
/// shares the predicate of the mechanism by construction (ethos rule 1).
/// Called without gate context, so an ungated `Done` card appears (its
/// verification is runnable work).
pub fn runnable(tasks: &[Task]) -> Vec<&Task> {
    tasks
        .iter()
        .filter(|t| matches!(disposition(t, tasks, &[]), TaskDisposition::Runnable))
        .collect()
}

/// Detect a cycle in a `DependsOn` edge set. `edges` are `(task, depends_on)`
/// pairs. Returns the cycle as the tasks along it, in edge order (the first
/// element depends on the second, and the last depends on the first), or
/// `None` for a DAG.
///
/// Deterministic: nodes and neighbors are visited in sorted order, so the
/// same input always names the same cycle — a wrong answer here must be
/// reproducible to be debuggable (ethos rule 4).
pub fn detect_cycle(edges: &[(TaskId, TaskId)]) -> Option<Vec<TaskId>> {
    let mut adjacency: BTreeMap<&TaskId, Vec<&TaskId>> = BTreeMap::new();
    let mut nodes: BTreeSet<&TaskId> = BTreeSet::new();
    for (from, to) in edges {
        adjacency.entry(from).or_default().push(to);
        nodes.insert(from);
        nodes.insert(to);
    }
    for neighbors in adjacency.values_mut() {
        neighbors.sort();
    }

    // 0 = unvisited, 1 = on the current path, 2 = fully explored.
    let mut state: BTreeMap<&TaskId, u8> = nodes.iter().map(|&n| (n, 0u8)).collect();

    for &start in &nodes {
        if state[start] != 0 {
            continue;
        }
        // Iterative DFS: (node, index of next neighbor to try). An explicit
        // stack because a board's dependency chain can be long and core must
        // not assume a deep call stack.
        let mut stack: Vec<(&TaskId, usize)> = vec![(start, 0)];
        state.insert(start, 1);

        while let Some((node, next)) = stack.last().copied() {
            let neighbors = adjacency.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if next < neighbors.len() {
                stack.last_mut().expect("stack is non-empty").1 += 1;
                let candidate = neighbors[next];
                match state[candidate] {
                    0 => {
                        state.insert(candidate, 1);
                        stack.push((candidate, 0));
                    }
                    1 => {
                        // Back-edge: the cycle is the path from `candidate`'s
                        // position on the stack up to the current node.
                        let pos = stack
                            .iter()
                            .position(|(n, _)| *n == candidate)
                            .expect("a grey node is on the current path");
                        return Some(stack[pos..].iter().map(|(n, _)| (*n).clone()).collect());
                    }
                    _ => {} // fully explored: no cycle through here
                }
            } else {
                state.insert(node, 2);
                stack.pop();
            }
        }
    }
    None
}

/// Would adding `new_deps` to `task_id` create a cycle? The creation-time
/// check (Invariant 4): circular `DependsOn` is rejected before it exists,
/// not discovered by a hung scheduler afterward. Returns the offending cycle
/// for the error message, or `None` if the edges are safe to add.
pub fn would_cycle(
    existing: &[Task],
    task_id: &TaskId,
    new_deps: &[TaskId],
) -> Option<Vec<TaskId>> {
    let mut edges: Vec<(TaskId, TaskId)> = Vec::new();
    for t in existing {
        for dep in &t.depends_on {
            edges.push((t.id.clone(), dep.clone()));
        }
    }
    for dep in new_deps {
        edges.push((task_id.clone(), dep.clone()));
    }
    detect_cycle(&edges)
}

// ---------------------------------------------------------------------------
// Prompt capture: the ledger duty (ethos rules 2 and 5)
// ---------------------------------------------------------------------------

/// Control words that steer the task ALREADY in flight rather than starting
/// a new one. A card for "continue" is noise that buries the cards that mean
/// something. Mirrors the Python board's `_AUTOTASK_SKIP` set.
const CAPTURE_SKIP: [&str; 26] = [
    "continue", "go", "yes", "y", "no", "n", "ok", "okay", "yep", "yeah", "sure",
    "stop", "wait", "retry", "again", "next", "done", "thanks", "ty", "k",
    "proceed", "resume", "keep going", "carry on", "do it", "sounds good",
];

/// Conversational lead-ins stripped so the derived title is the ACTION
/// ("Please can you fix X" -> "Fix X"). Checked case-insensitively,
/// repeatedly, longest-first at each step.
const CAPTURE_FILLER: [&str; 19] = [
    "i would like you to ", "i want you to ", "i need you to ", "could you please ",
    "can you please ", "would you please ", "i want to ", "i need to ", "we need to ",
    "we should ", "could you ", "can you ", "would you ", "will you ", "let's ",
    "lets ", "please ", "kindly ", "pls ",
];

/// Derive a board-card title from a prompt's own first clause — COMPUTED,
/// never a model call (ethos rule 2: the Python system paid a full
/// `claude -p` boot, ~12-15k input tokens, for a 3-word label, and the
/// throttle that cost forced is why most commands never reached the board).
/// Mirrors Python `_autotask_title` + the `_AUTOTASK_SKIP` guards.
///
/// Returns `None` when the text is NOT a task: a control word steering the
/// current work, a too-short fragment, a bare slash command (drives the
/// harness, not the work), or an explicit `[no-board]` opt-out. `None`
/// means "do not mint a ledger card", not "untitled".
pub fn title_from_prompt(text: &str) -> Option<String> {
    let mut t = text.trim();
    // Explicit opt-out marker, checked BEFORE stamp-stripping so the marker
    // itself is never mistaken for a timestamp prefix.
    let lower_all = t.to_lowercase();
    if lower_all.starts_with("[no-board]") || lower_all.starts_with("[no_board]") {
        return None;
    }
    // Drop leading "[03:47 PM] " / "[amux-origin: ...]" style stamps.
    while t.starts_with('[') {
        match t.find(']') {
            Some(i) => t = t[i + 1..].trim_start(),
            None => break,
        }
    }
    let collapsed = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let bare = collapsed
        .trim_end_matches(['.', '!', '?'])
        .trim()
        .to_lowercase();
    if collapsed.chars().count() < 12 || CAPTURE_SKIP.contains(&bare.as_str()) {
        return None;
    }
    // A bare slash command drives the harness, not the work.
    if collapsed.starts_with('/') && !collapsed.contains(' ') {
        return None;
    }

    // First sentence/clause: cut after ". " / "! " / "? " or at "; ".
    let mut head: &str = &collapsed;
    let chars: Vec<(usize, char)> = collapsed.char_indices().collect();
    for w in chars.windows(2) {
        let ((i, c), (_, next)) = (w[0], w[1]);
        if matches!(c, '.' | '!' | '?' | ';') && next == ' ' {
            head = &collapsed[..i + c.len_utf8()];
            break;
        }
    }
    let mut head = head.trim_matches([' ', '-', '–', '—', ':', ',']).to_string();

    // Strip conversational filler, repeatedly (they stack: "ok please ...").
    loop {
        let lower = head.to_lowercase();
        let Some(f) = CAPTURE_FILLER.iter().find(|f| lower.starts_with(*f)) else {
            break;
        };
        head = head[f.len()..]
            .trim_start_matches([' ', '-', '–', '—', ':', ','])
            .to_string();
    }
    head = head
        .trim_end_matches(['.', '!', '?', ',', ';', ' '])
        .to_string();

    // Sentence-case a lowercase opener.
    let mut out: String = head;
    if let Some(first) = out.chars().next() {
        if first.is_lowercase() {
            let upper: String = first.to_uppercase().collect();
            out = format!("{upper}{}", &out[first.len_utf8()..]);
        }
    }
    // Cap at 64 chars, cut on a word boundary (247 of 379 live Python titles
    // ran past 60 and ended mid-thought before this rule).
    if out.chars().count() > 64 {
        let cut: String = out.chars().take(63).collect();
        let cut = match cut.rfind(' ') {
            Some(i) => cut[..i].trim_end_matches([' ', '-', '–', '—', ':', ',']).to_string(),
            None => cut,
        };
        out = format!("{cut}…");
    }
    if out.is_empty() {
        // The clause dissolved (all filler): fall back to the raw text.
        out = collapsed.chars().take(60).collect();
    }
    Some(out)
}

/// A pure status / information query about existing work: "status on MSG-29602?",
/// "any update on the deploy?". These are answered inline and produce no
/// deliverable, yet capture minted them as type=code/doing — which a question can
/// never take through the `code` gate (ethos rule 3) and which held the WIP slot
/// until a manual discard (three times in one session before this: AMUX-3330).
/// Returning true here means "do not mint a board WORK card"; the prompt still
/// lands in `cmd_history`, so the Messages ledger keeps every prompt.
///
/// Deliberately TIGHT. The two errors are asymmetric: a FALSE POSITIVE silences a
/// real task (silent work, the worst failure), a FALSE NEGATIVE just leaves
/// today's minor manual-discard friction. So a match needs BOTH a status-query
/// opener AND a trailing `?`, which a request carrying a real imperative
/// ("is it done? if not, add a retry") does not have. A work request phrased as a
/// question ("can you fix X?", "does this build?") does not start with a status
/// opener and is left to card as normal.
pub fn is_status_query(text: &str) -> bool {
    let mut t = text.trim();
    // Drop the same leading "[03:47 PM] " / "[amux-origin: ...]" stamps
    // title_from_prompt strips, so a stamped status query still matches.
    while t.starts_with('[') {
        match t.find(']') {
            Some(i) => t = t[i + 1..].trim_start(),
            None => break,
        }
    }
    let collapsed = t.split_whitespace().collect::<Vec<_>>().join(" ");
    // A long prompt is not a bare query, whatever it opens with.
    if collapsed.chars().count() > 100 {
        return false;
    }
    let lower = collapsed.to_lowercase();
    // Bare "status" (with optional trailing punctuation) is the whole prompt.
    if lower.trim_end_matches(['.', '!', '?', ' ']) == "status" {
        return true;
    }
    // Otherwise it must be an actual QUESTION: requiring the trailing `?` keeps a
    // trailing imperative from reading as a pure query and silencing real work.
    if !collapsed.trim_end().ends_with('?') {
        return false;
    }
    const STATUS_OPENERS: &[&str] = &[
        "status on ", "status of ", "status for ", "status update", "status report",
        "what's the status", "whats the status", "what is the status",
        "how's the status", "hows the status",
        "any update on", "any updates on", "any news on", "any progress on",
        "update on ", "where are we on", "where are we with", "where do we stand",
        "is it done", "are we done", "did you finish",
        // Pure explanation-seeking questions (AMUX-3408: "why is the photo
        // analysis worker downloading qwen if i'm using gemini?" carded as
        // code/doing and held a WIP slot). The ≤100-char cap and mandatory
        // trailing `?` above already exclude compound ask-then-fix prompts;
        // a why-question whose ANSWER spawns work gets its card from the
        // follow-up prompt that asks for the work.
        "why is ", "why are ", "why does ", "why did ", "why was ", "why were ",
    ];
    STATUS_OPENERS.iter().any(|o| lower.starts_with(o))
}

/// Bare demonstratives/pronouns: words whose referent lives OUTSIDE the title.
const DEICTIC: [&str; 9] = ["this", "that", "these", "those", "it", "they", "them", "here", "there"];

/// Words that, following a demonstrative, mean it was used as a bare SUBJECT
/// ("this should…", "that broke…") rather than as a determiner with its own
/// noun ("this endpoint…", "that migration…"). The distinction is the whole
/// precision of the check: "This endpoint returns 500 for archived cards" is
/// perfectly dispatchable and must NOT be flagged, while "This should be one
/// row" is not dispatchable by anyone who was not in the room.
/// Kept generous on purpose: the two errors are not symmetric. A false
/// positive costs one extra sentence in one turn; a false negative is a card
/// nobody can action, which is the defect. Precision is preserved by the
/// determiner cases pinned in `self_contained_titles_are_left_alone`.
const DEICTIC_VERBS: [&str; 46] = [
    "should", "shouldn't", "is", "isn't", "are", "aren't", "was", "wasn't", "were",
    "needs", "need", "will", "won't", "can", "can't", "must", "does", "doesn't",
    "has", "hasn't", "have", "looks", "seems", "seemed", "breaks", "broke", "broken",
    "fail", "fails", "failed", "work", "works", "worked", "happened", "went",
    "stopped", "started", "keeps", "kept", "got", "gets", "did", "didn't",
    "still", "just", "also",
];

/// Imperative openers that carry no object of their own.
const IMPERATIVES: [&str; 14] = [
    "fix", "update", "change", "remove", "delete", "add", "move", "revert",
    "check", "make", "do", "redo", "undo", "adjust",
];

/// Why a captured title cannot be dispatched, or `None` when it is fine
/// (AMUX-2604).
///
/// A prompt is spoken INTO a context — a screen both parties are looking at, a
/// card just discussed — and capture cannot see any of it. "This should be one
/// row" and "fix the logo" are perfectly clear when said and meaningless three
/// hours later in a queue, which is how the amux-rust lane ended up holding
/// cards nobody (including their author) could action.
///
/// This is deliberately a COMPUTED check, never a model call: it runs on every
/// captured prompt, and paying a CLI boot to label a card is the exact waste
/// ethos rule 2 exists to stop. The model DOES get involved — later, in the
/// worker that received the prompt, which is the only party that ever had the
/// missing context. Capture's job is just to notice and say so.
///
/// Returns the reason so the nudge can name it rather than assert a vague
/// defect (ethos rule 4: the record must explain itself).
pub fn title_needs_self_description(title: &str) -> Option<&'static str> {
    let words: Vec<String> = title
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    let n = words.len();
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");

    // A long title has usually said enough to stand alone even if it opens
    // with a demonstrative, so the checks below are all length-bounded. The
    // bound is what keeps this from flagging most of the board.
    if n > 8 {
        return None;
    }

    // 1. Bare demonstrative SUBJECT: "This should be one row", "that broke".
    if DEICTIC.contains(&w(0)) && (n == 1 || DEICTIC_VERBS.contains(&w(1))) {
        return Some("it opens with a bare \"this/that/it\" whose referent is not in the title");
    }
    // 2. Bare pronoun subject — "it", "they", "there" are never determiners.
    if ["it", "they", "them", "here", "there"].contains(&w(0)) {
        return Some("it opens with a pronoun whose referent is not in the title");
    }
    // 3. Imperative with a deictic object: "fix this", "make it one row".
    for i in 0..n.saturating_sub(1).min(3) {
        if IMPERATIVES.contains(&w(i)) && DEICTIC.contains(&w(i + 1)) {
            return Some("its object is \"this/that/it\" rather than the thing itself");
        }
    }
    // 4. A short imperative on a DEFINITE object: "Fix the logo". The
    //    definite article is the tell — "the logo" presupposes a referent both
    //    parties can see, and the queue cannot. Measured against 907 live
    //    titles: without the determiner requirement this also flagged "Fix
    //    Namespace Pollution", "Revert VPA Configuration" and two more, all of
    //    which NAME their subject outright and are perfectly dispatchable.
    //    Requiring the article is what separates presupposing a referent from
    //    stating one.
    if n <= 4 && IMPERATIVES.contains(&w(0)) && ["the", "a", "an", "this", "that", "it"].contains(&w(1)) {
        return Some("it points at \"the <thing>\" without saying which one or where");
    }
    None
}

/// Is `verified` a MEANINGFUL tier for this item type, or make-work?
///
/// AMUX-2816 / AMUX-2782. `verified` means one thing in this repo: confirmed
/// working end-to-end IN PRODUCTION — CI green, deployed, exercised, no
/// regressions. That is a real and expensive claim about `code`, and about the
/// types that change a running system.
///
/// For a `doc`, a `chore`, an `investigation` or a `research` finding there is
/// no production to confirm anything in. Their verified gate reads "Outcome
/// confirmed to still hold", which is a RE-READ of a card somebody already
/// closed — precisely the shape of make-work that produced 294 advance
/// nudges/day against 25 human prompts before the `done` tier was removed.
///
/// THE MEASUREMENT THIS DECIDES. Verification collapsed from 256/day to 2/day
/// when the advance loop stopped selecting `done`, leaving 1,153 unarchived
/// done cards. The tempting fix is to drive all 1,153 toward `verified`. Most of
/// them should never go there: a doc is finished when it is written, and asking
/// a lane to re-confirm it is spending a turn to learn nothing.
///
/// So this narrows the target BEFORE anything is built to chase it. Whatever
/// eventually drives done -> verified must read this, or it will nag lanes about
/// cards that were already done in every sense that matters.
///
/// `done` stays terminal for everything else. That is not a demotion; it is the
/// honest end state, and saying so stops the board implying a tier nothing
/// reaches.
pub fn verified_is_meaningful(item_type: ItemType) -> bool {
    match item_type {
        // Changes a running system: there is a production to confirm in.
        ItemType::Code | ItemType::Ops | ItemType::Blocker | ItemType::Tripwire => true,
        // Nothing ships, so nothing can be confirmed in prod. `done` is the end.
        ItemType::Doc
        | ItemType::Chore
        | ItemType::Investigation
        | ItemType::Research
        | ItemType::Escalation
        // An epic is a grouping container — it ships nothing itself, so there is
        // no prod to confirm; it is done when its children/outcome are recorded.
        | ItemType::Epic
        | ItemType::Watch => false,
    }
}


#[cfg(test)]
mod capture_tests {
    use super::*;

    /// as_str must equal what serde emits, or a SQL filter built from it
    /// silently matches nothing for that type.
    #[test]
    fn item_type_as_str_matches_the_serde_spelling() {
        for t in ItemType::ALL {
            let ser = serde_json::to_string(&t).expect("serialize");
            assert_eq!(
                format!("\"{}\"", t.as_str()),
                ser,
                "as_str and serde disagree for {t:?}"
            );
        }
    }

    #[test]
    fn control_words_and_short_fragments_mint_no_card() {
        for s in ["continue", "yes", "ok", "keep going", "do it", "  go  ", "Retry."] {
            assert_eq!(title_from_prompt(s), None, "{s:?} is steering, not a task");
        }
        assert_eq!(title_from_prompt("fix it"), None, "too short to be a brief");
        assert_eq!(title_from_prompt("/compact"), None, "bare slash = harness");
        assert_eq!(
            title_from_prompt("[no-board] what's the status of the deploy?"),
            None,
            "explicit opt-out"
        );
    }

    #[test]
    fn status_queries_are_detected_but_real_work_is_not() {
        // The recurring shapes that were mis-carded (AMUX-3330).
        for s in [
            "status on MSG-29602?",
            "whats the status of MSG-29588?",
            "[09:04 PM] what's the status of AMUX-3?",
            "any update on the deploy?",
            "where are we on the connectors work?",
            "status",
            "status?",
            "is it done?",
            // The live specimen that minted AMUX-3408 (a pure why-question,
            // answered in-conversation, no unit of work).
            "[08:29 AM] why is the photo analysis worker downloading qwen if i'm using gemini?",
        ] {
            assert!(is_status_query(s), "{s:?} should read as a status query");
        }
        // Real work MUST still card — a false positive here silences a task.
        for s in [
            "build the connectors shit",
            "can you fix the OAuth redirect?",
            "does this build?",
            "is it done? if not, add a retry to the fetch",
            "let me paste keys in the connectors tab and define scope",
            "MSG-29602 and MSG-29588 too",
            "make sure dictation goes into the messages list as well",
            "status page is broken, rebuild it with the new layout and ship?",
            // A why-question with a trailing IMPERATIVE is work, and the
            // mandatory `?` gate keeps it carded (no trailing question mark).
            "why is the build red — fix it",
            // Over-100-chars is not a bare query whatever it opens with.
            "why is the deploy pipeline failing on the third stage after the cache restore step when the lockfile has not changed at all?",
        ] {
            assert!(!is_status_query(s), "{s:?} is real work, must still card");
        }
    }

    #[test]
    fn first_clause_becomes_the_title_with_stamps_and_filler_stripped() {
        assert_eq!(
            title_from_prompt("[03:47 PM] please fix the flaky auth test. It fails on CI only."),
            Some("Fix the flaky auth test".into())
        );
        assert_eq!(
            title_from_prompt("can you please update the deploy docs; the old runbook is stale"),
            Some("Update the deploy docs".into())
        );
        // A slash SKILL with a real brief still files a card.
        assert_eq!(
            title_from_prompt("/deploy the gateway change to staging"),
            Some("/deploy the gateway change to staging".into())
        );
    }

    #[test]
    fn long_briefs_cut_on_a_word_boundary_at_64() {
        let long = "Audit every mechanism that keeps workers adherent to their assigned board \
                    cards and write the per-mechanism verdicts into the final report";
        let t = title_from_prompt(long).unwrap();
        assert!(t.chars().count() <= 64, "{} chars: {t}", t.chars().count());
        assert!(t.ends_with('…'), "{t}");
        assert!(!t.trim_end_matches('…').ends_with(' '), "{t}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verification::EvidenceSource;

    fn t0() -> DateTime<Utc> {
        DateTime::from_timestamp(1_754_000_000, 0).unwrap()
    }
    fn t1() -> DateTime<Utc> {
        DateTime::from_timestamp(1_754_000_060, 0).unwrap()
    }

    // Fixed ULIDs: 26 chars, Crockford base32 (no I, L, O, U).
    fn tid(tail: &str) -> TaskId {
        TaskId::from_ulid(format!("01JGXV0000000000000000{tail}").parse().unwrap())
    }
    fn wid(tail: &str) -> WorkerId {
        WorkerId::from_ulid(format!("01JGXV0000000000000000{tail}").parse().unwrap())
    }
    fn gid(tail: &str) -> GateId {
        GateId::from_ulid(format!("01JGXV0000000000000000{tail}").parse().unwrap())
    }

    fn sys() -> Actor {
        Actor::System {
            component: "test".into(),
        }
    }
    fn worker_actor(tail: &str) -> Actor {
        Actor::Worker { id: wid(tail) }
    }

    fn mk(status: TaskStatus) -> Task {
        let mut t = Task::create(tid("AAAA"), "test card", ItemType::Code, sys(), t0());
        t.status = status;
        t
    }

    fn command_evidence() -> Evidence {
        Evidence {
            kind: EvidenceKind::CommandOutput,
            description: "cargo test: 42 passed".into(),
            artifact: Some("/tmp/test.log".into()),
            produced_at: t0(),
            source: EvidenceSource::Independent,
        }
    }

    /// A gate on entering Done, derived only for code cards.
    fn done_gate() -> Gate {
        Gate {
            id: gid("GAT0"),
            scope: Scope::Global,
            guards: TaskStatus::Done,
            applies_to_types: Some(vec![ItemType::Code]),
            criteria: vec![GateCriterion {
                description: "Tests / lint pass".into(),
                verifier: VerifierKind::Command {
                    cmd: "cargo test --workspace".into(),
                    expected_exit: 0,
                },
                required: true,
            }],
        }
    }

    // -- state machine -------------------------------------------------------

    #[test]
    fn happy_path_lifecycle_bumps_version_each_step() {
        let gates = [done_gate()];
        let t = mk(TaskStatus::Todo);

        let t = apply_transition(
            &t,
            BoardTransition::Claim {
                worker: wid("BBBB"),
            },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Todo);
        assert_eq!(t.worker, Some(wid("BBBB")));

        let t = apply_transition(
            &t,
            BoardTransition::Start,
            &worker_actor("BBBB"),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Doing);

        let t = apply_transition(
            &t,
            BoardTransition::RequestReview {
                reviewer: Actor::Human {
                    name: "ethan".into(),
                },
            },
            &worker_actor("BBBB"),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Review);
        assert!(t.reviewer.is_some());

        let t = apply_transition(
            &t,
            BoardTransition::Approve {
                evidence: vec![command_evidence()],
            },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Done);

        let t = apply_transition(
            &t,
            BoardTransition::Verify {
                criteria: vec![],
                evidence: vec![command_evidence()],
            },
            &sys(),
            &gates,
            t1(),
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Verified);
        assert!(t.status.is_terminal());
        // create=1, then five applied transitions.
        assert_eq!(t.version, 6);
        assert_eq!(t.updated_at, t1());
    }

    #[test]
    fn state_machine_rejects_invalid_transitions() {
        let cases: Vec<(TaskStatus, BoardTransition)> = vec![
            // Verify from todo: done is the only status a claim verifies from.
            (
                TaskStatus::Todo,
                BoardTransition::Verify {
                    criteria: vec![],
                    evidence: vec![],
                },
            ),
            (
                TaskStatus::Doing,
                BoardTransition::Approve { evidence: vec![] },
            ),
            (TaskStatus::Done, BoardTransition::Start),
            (TaskStatus::Todo, BoardTransition::Resume),
            (
                TaskStatus::Todo,
                BoardTransition::Fire { reason: "x".into() },
            ),
            (TaskStatus::Doing, BoardTransition::Unblock),
            (TaskStatus::Backlog, BoardTransition::Submit),
            (
                TaskStatus::Verified,
                BoardTransition::Discard { reason: "x".into() },
            ),
            (
                TaskStatus::Review,
                BoardTransition::Complete { evidence: vec![] },
            ),
        ];
        for (status, tx) in cases {
            let t = mk(status);
            let err = apply_transition(&t, tx, &sys(), &[], t0()).unwrap_err();
            assert!(
                matches!(err, TransitionError::InvalidTransition { from, .. } if from == status),
                "expected InvalidTransition from {status:?}, got {err:?}"
            );
        }
        // Create on an existing task is its own refusal.
        let err = apply_transition(
            &mk(TaskStatus::Todo),
            BoardTransition::Create {
                title: "x".into(),
                item_type: ItemType::Code,
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap_err();
        assert_eq!(err, TransitionError::CreateOnExisting);
    }

    #[test]
    fn archive_restore_round_trip_preserves_lifecycle_position() {
        let t = mk(TaskStatus::Doing);
        let v0 = t.version;

        let archived = apply_transition(
            &t,
            BoardTransition::Archive {
                reason: "parking".into(),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap();
        assert!(archived.archived);
        assert_eq!(
            archived.status,
            TaskStatus::Doing,
            "archive is a flag, not a status"
        );
        assert_eq!(archived.version, v0 + 1);

        // Archived cards are immutable except restore.
        let err =
            apply_transition(&archived, BoardTransition::Start, &sys(), &[], t0()).unwrap_err();
        assert_eq!(err, TransitionError::ArchivedTaskImmutable);
        // Double-archive changes nothing and must not bump version.
        let err = apply_transition(
            &archived,
            BoardTransition::Archive {
                reason: "again".into(),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap_err();
        assert_eq!(err, TransitionError::NoOp);

        let restored = apply_transition(
            &archived,
            BoardTransition::Restore {
                reason: "back".into(),
            },
            &sys(),
            &[],
            t1(),
        )
        .unwrap();
        assert!(!restored.archived);
        assert_eq!(
            restored.status,
            TaskStatus::Doing,
            "restore returns the card exactly where it was"
        );
        assert_eq!(restored.worker, t.worker);
        assert_eq!(restored.title, t.title);
        assert_eq!(restored.version, v0 + 2);

        // Restore on an unarchived card is a no-op refusal.
        let err = apply_transition(
            &restored,
            BoardTransition::Restore {
                reason: "again".into(),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap_err();
        assert_eq!(err, TransitionError::NoOp);
    }

    #[test]
    fn claim_is_exclusive_and_reclaim_is_noop() {
        let t = mk(TaskStatus::Todo);
        let t = apply_transition(
            &t,
            BoardTransition::Claim {
                worker: wid("BBBB"),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap();

        let err = apply_transition(
            &t,
            BoardTransition::Claim {
                worker: wid("CCCC"),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap_err();
        assert!(
            matches!(err, TransitionError::AlreadyClaimed { holder, .. } if holder == wid("BBBB"))
        );

        let err = apply_transition(
            &t,
            BoardTransition::Claim {
                worker: wid("BBBB"),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            TransitionError::NoOp,
            "re-claiming your own card changes nothing"
        );

        // Another worker cannot start it either.
        let err = apply_transition(&t, BoardTransition::Start, &worker_actor("CCCC"), &[], t0())
            .unwrap_err();
        assert!(matches!(err, TransitionError::AlreadyClaimed { .. }));
    }

    #[test]
    fn start_by_a_worker_implicitly_claims() {
        let t = mk(TaskStatus::Todo);
        let t =
            apply_transition(&t, BoardTransition::Start, &worker_actor("BBBB"), &[], t0()).unwrap();
        assert_eq!(t.status, TaskStatus::Doing);
        assert_eq!(t.worker, Some(wid("BBBB")));
    }

    #[test]
    fn release_on_unassigned_todo_is_noop() {
        // Invariant 37: a mutation that changes nothing is refused, so a
        // bumped version always means a real change.
        let err = apply_transition(
            &mk(TaskStatus::Todo),
            BoardTransition::Release,
            &sys(),
            &[],
            t0(),
        )
        .unwrap_err();
        assert_eq!(err, TransitionError::NoOp);
    }

    #[test]
    fn verification_failed_returns_the_claim_to_doing() {
        // Invariant 7: Failed -> task returns to in-progress.
        let t = mk(TaskStatus::Done);
        let t = apply_transition(
            &t,
            BoardTransition::VerificationFailed {
                reason: "prod smoke failed".into(),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Doing);
    }

    #[test]
    fn arm_only_fits_dormant_types_and_fire_makes_work() {
        let code = mk(TaskStatus::Todo);
        let err = apply_transition(&code, BoardTransition::Arm, &sys(), &[], t0()).unwrap_err();
        assert!(matches!(
            err,
            TransitionError::NotArmable {
                item_type: ItemType::Code
            }
        ));

        let mut watch = mk(TaskStatus::Todo);
        watch.item_type = ItemType::Watch;
        let armed = apply_transition(&watch, BoardTransition::Arm, &sys(), &[], t0()).unwrap();
        assert_eq!(armed.status, TaskStatus::Armed);

        let fired = apply_transition(
            &armed,
            BoardTransition::Fire {
                reason: "event hit".into(),
            },
            &sys(),
            &[],
            t0(),
        )
        .unwrap();
        assert_eq!(
            fired.status,
            TaskStatus::Todo,
            "a fired watch becomes live work"
        );
    }

    // -- gates ---------------------------------------------------------------

    #[test]
    fn gate_blocks_complete_and_why_blocked_names_the_way_forward() {
        let gates = [done_gate()];
        let t = mk(TaskStatus::Doing);

        let err = apply_transition(
            &t,
            BoardTransition::Complete { evidence: vec![] },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap_err();
        let TransitionError::GateBlocked { blocked } = err else {
            panic!("expected GateBlocked, got {err:?}");
        };
        // The why-blocked shape (Invariant 18): gate id, criterion, missing
        // evidence, suggested command — the caller needs no second query.
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].gate, gid("GAT0"));
        assert_eq!(blocked[0].criterion, "Tests / lint pass");
        assert_eq!(blocked[0].missing, EvidenceKind::CommandOutput);
        assert_eq!(
            blocked[0].suggested_command.as_deref(),
            Some("cargo test --workspace")
        );

        // The standalone query agrees with the enforcement — same function.
        let q = why_blocked(&t, TaskStatus::Done, &gates, &[]);
        assert_eq!(q, blocked);
    }

    #[test]
    fn matching_evidence_satisfies_the_gate() {
        let gates = [done_gate()];
        let t = mk(TaskStatus::Doing);
        let done = apply_transition(
            &t,
            BoardTransition::Complete {
                evidence: vec![command_evidence()],
            },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(done.status, TaskStatus::Done);
    }

    #[test]
    fn gates_derive_from_item_type() {
        // The same gate does not derive for a chore card: retyping is the
        // honest exit from an ill-fitting gate (ethos rule 3).
        let gates = [done_gate()];
        let mut chore = mk(TaskStatus::Doing);
        chore.item_type = ItemType::Chore;
        let done = apply_transition(
            &chore,
            BoardTransition::Complete { evidence: vec![] },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(done.status, TaskStatus::Done);
    }

    #[test]
    fn gate_override_narrows_to_the_named_gate() {
        let gates = [done_gate()];
        let mut t = mk(TaskStatus::Doing);
        t.gate_override = Some(gid("GAT1")); // not the done gate
        let done = apply_transition(
            &t,
            BoardTransition::Complete { evidence: vec![] },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(
            done.status,
            TaskStatus::Done,
            "override excluded the scope gate"
        );
    }

    #[test]
    fn force_bypasses_gates_but_a_same_status_force_is_noop() {
        let gates = [done_gate()];
        let t = mk(TaskStatus::Doing);
        let forced = apply_transition(
            &t,
            BoardTransition::Force {
                status: TaskStatus::Done,
                reason: "hotfix, evidence in PR".into(),
            },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap();
        assert_eq!(forced.status, TaskStatus::Done);

        let err = apply_transition(
            &forced,
            BoardTransition::Force {
                status: TaskStatus::Done,
                reason: "again".into(),
            },
            &sys(),
            &gates,
            t0(),
        )
        .unwrap_err();
        assert_eq!(err, TransitionError::NoOp);
    }

    // -- disposition (Invariant 10) ------------------------------------------

    #[test]
    fn disposition_is_total_over_every_status() {
        for status in TaskStatus::ALL {
            let task = mk(status);
            // Must resolve — the exhaustive match makes a missing status a
            // compile error; this test pins the semantic corners.
            let d = disposition(&task, &[], &[]);
            if status.is_terminal() {
                assert!(matches!(d, TaskDisposition::Terminal), "{status:?}");
            }
            // Archived is terminal regardless of status (Invariant 10).
            let mut archived = mk(status);
            archived.archived = true;
            assert!(
                matches!(disposition(&archived, &[], &[]), TaskDisposition::Terminal),
                "{status:?} archived"
            );
        }

        // Semantic spot checks for the non-obvious cells.
        assert!(matches!(
            disposition(&mk(TaskStatus::Todo), &[], &[]),
            TaskDisposition::Runnable
        ));
        assert!(matches!(
            disposition(&mk(TaskStatus::Backlog), &[], &[]),
            TaskDisposition::Waiting(WaitingFor::User)
        ));
        assert!(matches!(
            disposition(&mk(TaskStatus::NeedsYou), &[], &[]),
            TaskDisposition::Waiting(WaitingFor::User)
        ));
        // Doing with no worker is the human's own in-flight work (ethos rule 8).
        assert!(matches!(
            disposition(&mk(TaskStatus::Doing), &[], &[]),
            TaskDisposition::Waiting(WaitingFor::User)
        ));
        let mut assigned = mk(TaskStatus::Doing);
        assigned.worker = Some(wid("BBBB"));
        assert!(matches!(
            disposition(&assigned, &[], &[]),
            TaskDisposition::Assigned { worker } if worker == wid("BBBB")
        ));
        // Armed waits on its firing event — structured, never invisible.
        let mut armed = mk(TaskStatus::Armed);
        armed.item_type = ItemType::Watch;
        assert!(matches!(
            disposition(&armed, &[], &[]),
            TaskDisposition::Waiting(WaitingFor::ExternalCondition { .. })
        ));
        // A done card gated on Verified waits on that gate, by id.
        let gates = [Gate {
            guards: TaskStatus::Verified,
            ..done_gate()
        }];
        assert!(matches!(
            disposition(&mk(TaskStatus::Done), &[], &gates),
            TaskDisposition::Waiting(WaitingFor::Gate { gate }) if gate == gid("GAT0")
        ));
    }

    // -- dependency graph (Invariant 4) --------------------------------------

    #[test]
    fn runnable_derives_from_the_dependency_graph() {
        // The Invariant 4 picture: A and B can run concurrently; C cannot
        // start until both are done.
        let mut a = mk(TaskStatus::Todo);
        a.id = tid("AAAA");
        let mut b = mk(TaskStatus::Todo);
        b.id = tid("BBBB");
        let mut c = mk(TaskStatus::Todo);
        c.id = tid("CCCC");
        c.depends_on = vec![a.id.clone(), b.id.clone()];

        let board = vec![a.clone(), b.clone(), c.clone()];
        let ready: Vec<&TaskId> = runnable(&board).iter().map(|t| &t.id).collect();
        assert_eq!(ready, vec![&a.id, &b.id]);

        // C names WHICH dependency it waits on — the first unmet one.
        match disposition(&c, &board, &[]) {
            TaskDisposition::Waiting(WaitingFor::Dependency { on }) => assert_eq!(on, a.id),
            other => panic!("expected Waiting(Dependency), got {other:?}"),
        }

        // Both dependencies verified -> C becomes runnable.
        a.status = TaskStatus::Verified;
        b.status = TaskStatus::Verified;
        let board = vec![a, b, c.clone()];
        let ready: Vec<&TaskId> = runnable(&board).iter().map(|t| &t.id).collect();
        assert_eq!(ready, vec![&c.id]);
    }

    #[test]
    fn unknown_dependency_is_conservatively_unmet() {
        // A dep the board slice cannot find proves nothing about doneness.
        let mut c = mk(TaskStatus::Todo);
        c.depends_on = vec![tid("ZZZZ")];
        assert_eq!(first_unmet_dependency(&c, &[]), Some(tid("ZZZZ")));
    }

    #[test]
    fn detect_cycle_finds_the_loop_and_clears_a_dag() {
        let (a, b, c) = (tid("AAAA"), tid("BBBB"), tid("CCCC"));

        let dag = vec![(c.clone(), a.clone()), (c.clone(), b.clone())];
        assert_eq!(detect_cycle(&dag), None);

        let cyclic = vec![
            (a.clone(), b.clone()),
            (b.clone(), c.clone()),
            (c.clone(), a.clone()),
        ];
        let cycle = detect_cycle(&cyclic).expect("cycle exists");
        assert_eq!(cycle.len(), 3);
        for node in [&a, &b, &c] {
            assert!(cycle.contains(node), "cycle names every node in the loop");
        }
    }

    #[test]
    fn would_cycle_rejects_circular_depends_on_at_creation() {
        let mut a = mk(TaskStatus::Todo);
        a.id = tid("AAAA");
        a.depends_on = vec![tid("BBBB")];
        let mut b = mk(TaskStatus::Todo);
        b.id = tid("BBBB");
        let board = vec![a, b];

        // B gaining a dependency on A would close the loop.
        assert!(would_cycle(&board, &tid("BBBB"), &[tid("AAAA")]).is_some());
        // A fresh card depending on B is fine.
        assert!(would_cycle(&board, &tid("CCCC"), &[tid("BBBB")]).is_none());
    }

    // -- serde shapes --------------------------------------------------------

    #[test]
    fn serde_shapes_are_snake_case_and_tagged() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::NeedsYou).unwrap(),
            "\"needs_you\""
        );
        assert_eq!(
            serde_json::to_string(&ItemType::Tripwire).unwrap(),
            "\"tripwire\""
        );

        let tx = serde_json::to_value(BoardTransition::Start).unwrap();
        assert_eq!(tx["kind"], "start");
        let tx = serde_json::to_value(BoardTransition::Complete { evidence: vec![] }).unwrap();
        assert_eq!(tx["kind"], "complete");

        // Approve's evidence defaults empty on the wire (older callers).
        let approve: BoardTransition = serde_json::from_str(r#"{"kind":"approve"}"#).unwrap();
        assert!(matches!(approve, BoardTransition::Approve { evidence } if evidence.is_empty()));

        // The 409 body: a gate refusal serializes with the full why-blocked
        // answer inline.
        let err = TransitionError::GateBlocked {
            blocked: vec![WhyBlocked {
                gate: gid("GAT0"),
                criterion: "Tests / lint pass".into(),
                missing: EvidenceKind::CommandOutput,
                suggested_command: Some("cargo test --workspace".into()),
            }],
        };
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "gate_blocked");
        assert_eq!(v["blocked"][0]["criterion"], "Tests / lint pass");
        assert_eq!(v["blocked"][0]["missing"], "command_output");
        assert_eq!(
            v["blocked"][0]["suggested_command"],
            "cargo test --workspace"
        );
    }
}

#[cfg(test)]
mod self_description_tests {
    use super::*;

    /// The card's own examples, plus the shapes the amux-rust queue actually
    /// filled up with.
        /// The narrowing decision (AMUX-2816), pinned so nobody widens it back by
    /// accident. The measurement behind it: verification went 256/day -> 2/day
    /// leaving 1,153 unverified done cards, and most of those should never reach
    /// `verified` at all.
    #[test]
    fn verified_is_only_meaningful_where_there_is_a_production_to_confirm_in() {
        // Ships something a user runs: the 4-part prod gate is a real claim.
        for t in [ItemType::Code, ItemType::Ops, ItemType::Blocker, ItemType::Tripwire] {
            assert!(verified_is_meaningful(t), "{t:?} changes a running system");
        }
        // Ships nothing. Their verified gate is "outcome confirmed to still
        // hold" — a re-read of a closed card, which costs a lane's turn to learn
        // nothing. `done` is the honest end state.
        for t in [
            ItemType::Doc,
            ItemType::Chore,
            ItemType::Investigation,
            ItemType::Research,
            ItemType::Escalation,
            ItemType::Watch,
        ] {
            assert!(!verified_is_meaningful(t), "{t:?} has no prod to confirm in");
        }
    }

#[test]
    fn deictic_titles_are_flagged() {
        for s in [
            "This should be one row",
            "Fix the logo",
            "that broke again",
            "It needs to be centered",
            "fix this",
            "Make it one row",
            "Do that instead",
            "these are wrong",
            "there is a bug",
            "Update this",
        ] {
            assert!(
                title_needs_self_description(s).is_some(),
                "{s:?} has no referent and should be flagged"
            );
        }
    }

    /// The half that matters more. A filter that flags EVERYTHING is the same
    /// defect as one that flags nothing, except it is confidently wrong and
    /// would nag every worker on every prompt (ethos rule 7). These must all
    /// come back clean, and several open with the very words the check looks
    /// for — "this"/"that" as DETERMINERS, which have their referent right
    /// there in the title.
    #[test]
    fn self_contained_titles_are_left_alone() {
        for s in [
            "This endpoint returns 500 for archived cards",
            "That migration drops the wrong index",
            "Fix the flaky auth test that only fails on CI",
            "Update the deploy docs; the old runbook is stale",
            "Add a route for /api/board/clear-done",
            "Rename the amux session and re-pin its harness name",
            "Board gates derive from item type",
            "Deploy the gateway change to staging",
        ] {
            assert_eq!(
                title_needs_self_description(s),
                None,
                "{s:?} is dispatchable and must NOT be flagged"
            );
        }
    }

    /// A long title stands on its own even when it opens deictically — the
    /// length bound is what stops this flagging most of the board.
    #[test]
    fn length_bounds_the_check() {
        assert!(title_needs_self_description("This is wrong").is_some());
        assert_eq!(
            title_needs_self_description(
                "This is wrong because the archived filter runs before the join and drops rows"
            ),
            None
        );
    }

    /// It composes with the capture path it actually runs in: whatever
    /// title_from_prompt yields is what gets checked, filler stripped and all.
    #[test]
    fn it_runs_on_the_derived_title_not_the_raw_prompt() {
        let t = title_from_prompt("could you please fix this").expect("mints a card");
        assert_eq!(t, "Fix this");
        assert!(title_needs_self_description(&t).is_some());

        let t2 = title_from_prompt("please add a route for /api/board/clear-done").unwrap();
        assert_eq!(title_needs_self_description(&t2), None, "{t2:?}");
    }
}
