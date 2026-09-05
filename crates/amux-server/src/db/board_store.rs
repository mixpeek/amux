//! Board store: the Python `issues` table <-> core [`Task`] interop shim
//! (Phase 2, RR-0049/RR-0053/RR-0055; strangler-fig rule from the plan).
//!
//! THE RUST API READS AND WRITES THE SAME ROWS THE PYTHON SERVER SERVES.
//! Phase 11's rollback requirement is that the Python server keeps working
//! against this DB at any moment, so every mapping here preserves the Python
//! vocabulary byte-for-byte:
//!
//! - `issues.id` is the wire id ("AMUX-123"). Core [`TaskId`]s are derived
//!   deterministically from it ([`internal_id`]) and never persisted.
//! - `status` is stored in the Python spelling (`needsyou`, not `needs_you`).
//!   Reads accept both; writes preserve whatever spelling the row already
//!   used ([`status_to_db`]) so a Rust write never rewrites Python's
//!   vocabulary in shared rows.
//! - `created`/`updated` are unix INTEGER seconds (0001_baseline: `created
//!   INTEGER NOT NULL`), never RFC3339 strings.
//! - `log` is the append-only history; lines are `` `HH:MM` <text> ``
//!   exactly as `_append_board_log` writes them ([`append_log`]).
//! - id minting replicates `_next_issue_id` / `_prefix_from_session` over
//!   the shared `issue_counters` table, so the two servers can never mint
//!   colliding ids.
//! - `deleted IS NULL` (soft delete) is filtered in EVERY query, and
//!   [`save_patched`] deliberately never touches Python-owned columns it
//!   does not model (`creator`, `created`, `notified`, `gcal_event_id`,
//!   `deleted`).

use amux_core::board::{self, Gate, GateCriterion, ItemType, Task, TaskStatus};
use amux_core::events::Actor;
use amux_core::ids::{GateId, TaskId};
use amux_core::verification::VerifierKind;
use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Ids: semantic ("AMUX-123") on the wire and in the DB, TaskId internally
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit. Stable, dependency-free; collisions across a board-sized
/// id space are negligible and would only affect in-memory graph checks.
fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// INTEROP SHIM (Phase 11 migrates ids): derive the internal core [`TaskId`]
/// deterministically from the semantic `issues.id`. The semantic id is the
/// only identity that exists in the shared DB and on the wire; the core state
/// machine and graph helpers want `TaskId`s, so we mint one per semantic id
/// via a fixed hash (timestamp part 0 so the mapping is pure). Never persist
/// these — the row keeps the semantic id, and API payloads always use it.
pub fn internal_id(semantic: &str) -> TaskId {
    TaskId::from_ulid(ulid::Ulid::from_parts(0, u128::from(fnv64(semantic))))
}

/// Deterministic [`GateId`] for the synthesized ack-gate on a target status —
/// same shim as [`internal_id`]: stable so `why_blocked` output is
/// reproducible (ethos rule 4), never persisted.
fn gate_id_for(target: TaskStatus) -> GateId {
    GateId::from_ulid(ulid::Ulid::from_parts(
        0,
        u128::from(fnv64(&format!("board-gate:{}", db_status_spelling(target)))),
    ))
}

// ---------------------------------------------------------------------------
// Status vocabulary: Python spellings <-> TaskStatus
// ---------------------------------------------------------------------------

/// Parse a stored/requested status into core vocabulary. Accepts BOTH
/// `needsyou` (the Python DB spelling — see amux-server.py's
/// `('needsyou','review','blocked','backlog')` queries) and core's
/// serde spelling `needs_you`, plus the Python `_STATUS_ALIASES` synonyms,
/// so a row written by either server parses on both sides.
pub fn parse_status(raw: &str) -> Option<TaskStatus> {
    match raw.trim().to_lowercase().as_str() {
        "backlog" => Some(TaskStatus::Backlog),
        "todo" => Some(TaskStatus::Todo),
        "doing" | "wip" | "in_progress" | "inprogress" => Some(TaskStatus::Doing),
        "review" | "in_review" | "inreview" | "in review" => Some(TaskStatus::Review),
        "needsyou" | "needs_you" => Some(TaskStatus::NeedsYou),
        "blocked" => Some(TaskStatus::Blocked),
        "done" | "resolved" | "complete" | "completed" | "closed" => Some(TaskStatus::Done),
        "verified" => Some(TaskStatus::Verified),
        "discarded" => Some(TaskStatus::Discarded),
        "armed" => Some(TaskStatus::Armed),
        "quarantined" => Some(TaskStatus::Quarantined),
        _ => None,
    }
}

/// The Python DB spelling for each status (what a FRESH write uses). Note
/// `needsyou` — the live board's own spelling, NOT core's `needs_you`.
pub fn db_status_spelling(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Backlog => "backlog",
        TaskStatus::Todo => "todo",
        TaskStatus::Doing => "doing",
        TaskStatus::Review => "review",
        TaskStatus::NeedsYou => "needsyou",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Verified => "verified",
        TaskStatus::Discarded => "discarded",
        TaskStatus::Armed => "armed",
        TaskStatus::Quarantined => "quarantined",
    }
}

/// The string to WRITE for `target`, given the raw spelling currently in the
/// row: if the row already spells this status (e.g. a legacy `needs_you`
/// written by hand), keep that exact spelling — do not rewrite Python's
/// vocabulary in shared rows. Otherwise use the Python default spelling.
pub fn status_to_db(target: TaskStatus, prior_raw: &str) -> String {
    if parse_status(prior_raw) == Some(target) {
        prior_raw.to_string()
    } else {
        db_status_spelling(target).to_string()
    }
}

// ---------------------------------------------------------------------------
// Item types
// ---------------------------------------------------------------------------

/// The Python `_ITEM_TYPES` tuple, verbatim (order preserved for the
/// `valid_types` field the CLI prints).
pub const KNOWN_TYPES: [&str; 12] = [
    "code",
    "escalation",
    "blocker",
    "investigation",
    "ops",
    "research",
    "chore",
    "doc",
    "tripwire",
    "watch",
    // Grouping container (AMUX-2992). NOTE: this list duplicates
    // `ItemType::ALL` and must be kept in step with it by hand — the enum's own
    // doc calls that out; a future cleanup should derive one from the other.
    "epic",
    // AF-323. Added because the board was already STORING it: five live cards
    // carried `type: decision` while this very list made the create path refuse
    // it, so two components disagreed about the same fact and the disagreement
    // was load-bearing — an unlisted type falls through `core_item_type` to
    // `Code`, the strictest gate, and those five cards could not be closed
    // honestly by anyone.
    "decision",
];

/// Core [`ItemType`] for GATE purposes. Unknown/legacy strings map to `Code`
/// — the strictest gate — matching Python's `_item_type_gate` fallthrough
/// ("never silently weaken a gate"). The raw string itself stays on the row
/// and in API payloads; this mapping is only ever used to derive gates and
/// drive the state machine.
pub fn core_item_type(raw: &str) -> ItemType {
    match raw.trim().to_lowercase().as_str() {
        "escalation" => ItemType::Escalation,
        "blocker" => ItemType::Blocker,
        "investigation" => ItemType::Investigation,
        "ops" => ItemType::Ops,
        "research" => ItemType::Research,
        "chore" => ItemType::Chore,
        "doc" => ItemType::Doc,
        "tripwire" => ItemType::Tripwire,
        "watch" => ItemType::Watch,
        "epic" => ItemType::Epic,
        "decision" => ItemType::Decision,
        _ => ItemType::Code,
    }
}

// ---------------------------------------------------------------------------
// Gates: the Python type-derived tables, ported verbatim
// ---------------------------------------------------------------------------

/// Default gate criteria for (item type, target status) — the Python tables
/// ported EXACTLY (amux-server.py `_TYPE_GATES` + the `statuses.gate`
/// bootstrap seeds that `code` falls through to). Strings must stay
/// byte-identical: `gate_checked` acks are matched by exact string against
/// these on BOTH servers, so a drifted criterion here would make an ack
/// minted against one server unusable against the other.
///
/// This is the FLOOR of the precedence ladder — the scoped tiers
/// (card > worker > group > global column) live in
/// [`effective_gate_scoped`] and land here only when nothing above matched.
/// The one GLOBAL `done` constraint (Ethan, 2026-08-17): a card cannot be
/// marked done without pointing at the artifact it produced. It sits ALONGSIDE
/// the type/scope gate ladder, not inside it — the type-derived criteria are
/// satisfied by an honest ack, but this one is MACHINE-VALIDATED in the board
/// handler against the card's own text (`has_asset_link`), so `gate_ack` cannot
/// fake it and only a real link satisfies it (ethos rule 7: a check that cannot
/// fail is theatre). This constant is the human-facing LABEL for it, shown in
/// `/api/board/contract`. It is on by default everywhere and overridable per
/// worker / group / global through the environment primitive
/// (`AMUX_DONE_LINK_REQUIRED=0`), resolved worker > group > global — the same
/// ladder every scoped setting uses, so the override is not a second spelling
/// of "scoped policy".
pub const ASSET_LINK_CRITERION: &str =
    "Link to the created asset is on the card (URL, file path, commit, or #PR)";

/// The env key that turns the global done-link constraint off for a scope.
pub const DONE_LINK_REQUIRED_KEY: &str = "AMUX_DONE_LINK_REQUIRED";

/// Whether the done-link constraint applies to a card owned by `session`.
/// Default ON; a worker (or its group, or global) opts out with
/// `AMUX_DONE_LINK_REQUIRED` in {0,false,off,no}. Resolved through the same
/// worker > group > global env ladder as every other scoped setting, so the
/// override lives in the environment primitive rather than a new store. An
/// unowned card (no session) always gets the global default. Called by the
/// board handler to decide whether to validate a link before allowing `done`.
pub fn done_link_required(session: Option<&str>) -> bool {
    fn is_off(v: &str) -> bool {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
    }
    // A PROCESS-ENV override wins: `AMUX_DONE_LINK_REQUIRED` in ~/.amux/server.env
    // (loaded into process env at startup) is the global operator switch, and it
    // is also how the test rigs turn the gate off for the mechanics/lifecycle
    // suites that are not testing it. Unset falls through to the per-worker /
    // group / global scope FILES below.
    if let Ok(v) = std::env::var(DONE_LINK_REQUIRED_KEY) {
        if !v.trim().is_empty() {
            return !is_off(&v);
        }
    }
    let lane = match session.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return true,
    };
    match crate::api::session_verbs::scoped_setting_in(
        &crate::api::session_verbs::home(),
        lane,
        DONE_LINK_REQUIRED_KEY,
    ) {
        Some(v) => !is_off(&v),
        None => true,
    }
}

/// Scope key for the AF-321 evidence requirement on `done`.
pub const DONE_EVIDENCE_REQUIRED_KEY: &str = "AMUX_DONE_EVIDENCE_REQUIRED";

/// Is the AF-321 evidence requirement enforced for `session`?
///
/// Same resolver shape as [`done_link_required`] on purpose: process env wins
/// (the operator switch in `~/.amux/server.env`, and how the test rigs that are
/// not testing this gate turn it off), then the worker > group > global scope
/// files. Default ON — an advisory rule that loses to the mechanism is exactly
/// what this card is about, and ethos rule 1 asks for opt-out, not opt-in.
pub fn done_evidence_required(session: Option<&str>) -> bool {
    fn is_off(v: &str) -> bool {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
    }
    if let Ok(v) = std::env::var(DONE_EVIDENCE_REQUIRED_KEY) {
        if !v.trim().is_empty() {
            return !is_off(&v);
        }
    }
    let lane = match session.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return true,
    };
    match crate::api::session_verbs::scoped_setting_in(
        &crate::api::session_verbs::home(),
        lane,
        DONE_EVIDENCE_REQUIRED_KEY,
    ) {
        Some(v) => !is_off(&v),
        None => true,
    }
}

/// Why a piece of evidence was refused, or that it was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceVerdict {
    /// Names an artifact: a command, path, URL, sha or #N.
    Ok,
    /// Nothing recorded at all.
    Missing,
    /// Prose with nothing to check: "implemented", "done", "fixed it".
    NoArtifact,
    /// `none:` with no reason after it. The escape exists, but an unexplained
    /// escape is the thing it is meant to prevent.
    UnexplainedNone,
}

/// Does `text` say what was actually run or produced?
///
/// Accepts three shapes:
///   1. anything [`has_asset_link`] recognises (URL, repo path, sha, `#N`);
///   2. a shell invocation — a line starting `$ `, or fenced/backticked text;
///   3. the honest no-artifact answer `none: <reason>`.
///
/// (3) is not a loophole, it is ethos rule 3: an escalation that closed because
/// the owner decided, or a watch that stood down, produces no artifact, and a
/// gate with no truthful path in a legitimate state forces a lie. It is stored
/// verbatim so `evidence LIKE 'none:%'` counts them, which is the difference
/// between an escape and a blind spot.
pub fn evidence_verdict(text: &str) -> EvidenceVerdict {
    let t = text.trim();
    if t.is_empty() {
        return EvidenceVerdict::Missing;
    }
    if let Some(rest) = t.strip_prefix("none:").or_else(|| t.strip_prefix("NONE:")) {
        // A reason, not a shrug. "none: n/a" is 3 chars and says nothing, so
        // require enough words that someone had to think.
        return if rest.split_whitespace().count() >= 3 {
            EvidenceVerdict::Ok
        } else {
            EvidenceVerdict::UnexplainedNone
        };
    }
    if has_asset_link(t) {
        return EvidenceVerdict::Ok;
    }
    // A command: `$ cargo test`, or anything backticked/fenced.
    if t.contains('`') || t.lines().any(|l| l.trim_start().starts_with("$ ")) {
        return EvidenceVerdict::Ok;
    }
    EvidenceVerdict::NoArtifact
}

/// Scope key for the AF-318 typed-ask requirement on `needsyou`.
pub const NEEDSYOU_ASK_REQUIRED_KEY: &str = "AMUX_NEEDSYOU_ASK_REQUIRED";

/// The five kinds of human act a card can be waiting on.
///
/// Five, and closed. An open vocabulary would re-admit the 227 cards this gate
/// exists to keep out: every one of them could be described in free text, and
/// none of them fits any of these without saying something untrue. That is the
/// point — a card whose block does not name a human ACT is not blocked on a
/// human, it is a card someone stopped working on.
///
/// `judgment` is the deliberate soft one and it is still not a catch-all: it
/// means a call only the owner's taste can settle, which is a real category
/// (ethos rule 3 wants a truthful path for it) and NOT "I would like a second
/// opinion".
pub const ASK_TYPES: [&str; 5] = ["decision", "access", "credential", "external", "judgment"];

/// What each type means, printed in the refusal so the reader picks correctly
/// on the first try rather than by guessing at five bare words.
pub const ASK_TYPE_HELP: [(&str, &str); 5] = [
    ("decision", "a choice only the owner can make — direction, priority, or a trade-off with no right answer"),
    ("access", "you cannot reach something: a repo, a console, an environment, a person"),
    ("credential", "a secret, token, key or sign-in only the owner can supply"),
    ("external", "waiting on a third party — a vendor, a support ticket, another company's deploy"),
    ("judgment", "a call only the owner's taste settles: is this good enough, does this read right"),
];

/// Is the AF-318 typed-ask requirement enforced for `session`?
///
/// Same resolver as [`done_link_required`] and [`done_evidence_required`]:
/// process env wins, then worker > group > global scope. Default ON.
pub fn needsyou_ask_required(session: Option<&str>) -> bool {
    fn is_off(v: &str) -> bool {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
    }
    if let Ok(v) = std::env::var(NEEDSYOU_ASK_REQUIRED_KEY) {
        if !v.trim().is_empty() {
            return !is_off(&v);
        }
    }
    let lane = match session.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return true,
    };
    match crate::api::session_verbs::scoped_setting_in(
        &crate::api::session_verbs::home(),
        lane,
        NEEDSYOU_ASK_REQUIRED_KEY,
    ) {
        Some(v) => !is_off(&v),
        None => true,
    }
}

/// Why a typed ask was refused, or that it was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskVerdict {
    /// Names a type, a question and what ends the block.
    Ok,
    /// No `ask_type` at all — the untyped move this gate exists to refuse.
    NoType,
    /// An `ask_type` outside the closed vocabulary.
    UnknownType,
    /// No specific person/external actor, or a generic placeholder.
    NoActor,
    /// No question, or too short to be one.
    NoQuestion,
    /// Prose was supplied, but it is not actually phrased as a question.
    NotAQuestion,
    /// No statement of what ends the block.
    NoUnblocks,
}

/// A sentence, not a shrug. Three words is the floor at which someone has had
/// to think about the reader; "n/a", "blocked", "ask Ethan" all fall under it,
/// and all three are real specimens from the 445.
fn is_a_sentence(s: &str) -> bool {
    s.split_whitespace().count() >= 3
}

/// Does this card say who is being asked what, and what ends the block?
pub fn ask_verdict(actor: &str, ask_type: &str, question: &str, unblocks: &str) -> AskVerdict {
    let t = ask_type.trim().to_ascii_lowercase();
    if t.is_empty() {
        return AskVerdict::NoType;
    }
    if !ASK_TYPES.contains(&t.as_str()) {
        return AskVerdict::UnknownType;
    }
    let actor = actor.trim().to_ascii_lowercase();
    if actor.is_empty()
        || matches!(actor.as_str(), "human" | "user" | "owner" | "someone" | "you" | "me")
    {
        return AskVerdict::NoActor;
    }
    if !is_a_sentence(question) {
        return AskVerdict::NoQuestion;
    }
    if !question.contains('?') {
        return AskVerdict::NotAQuestion;
    }
    if !is_a_sentence(unblocks) {
        return AskVerdict::NoUnblocks;
    }
    AskVerdict::Ok
}

// ---------------------------------------------------------------------------
// The continuation contract (AMUX-3946)
// ---------------------------------------------------------------------------

pub const CONTINUATION_REQUIRED_KEY: &str = "AMUX_CONTINUATION_REQUIRED";

/// Is the continuation gate on for this lane?
///
/// OPT-IN, which is the one place this deliberately departs from ethos rule 1's
/// "prefer opt-out". The rule's question is who receives a capability by
/// default; this is a COST, and it lands on 52 lanes at once. amux opts in
/// first and eats it, per the dogfooding rule, and the default flips once there
/// is a measurement rather than a guess about what it costs.
///
/// THE MEASUREMENT EXISTS NOW (2026-08-31, AF-355). It says do not flip yet, and
/// the reason is that the two doors cost different amounts:
///
/// * The MANUAL door (`board.rs`, on the transition) costs one extra
///   `amux board next` per future claim, and nothing up front — cards already in
///   `doing` are not re-gated. Cheap.
/// * AUTO-PICKUP (`board_drive.rs`) SKIPS a card whose next_action is not Ok.
///   Measured across /api/board: 10 of 2042 cards carry a next_action at all,
///   all ten written by the one lane that has the gate on, and 4 of 1220
///   pickup-eligible cards would pass. Flipping the default makes pickup skip
///   ~1216 of 1220 and every lane on the drive loop goes idle.
///
/// That circularity is the finding: lanes do not write the field because the gate
/// is off, and the gate cannot go on until they do. So "just flip it" and "leave
/// it" are both wrong, and the way through is to turn it on for a few lanes with
/// active backlogs and measure idle time against a matched set that stays off.
///
/// The tempting shortcut — gate the manual door fleet-wide, leave pickup open —
/// is the same-card-opposite-answers defect that `board_drive.rs`'s own comment
/// rejects (AMUX-3929's shape). Named here so it is not re-derived.
///
/// Structured exactly like `needsyou_ask_required` so the two cannot drift:
/// env var wins, then the per-lane scoped setting. Only the DEFAULT differs,
/// and it differs on purpose.
pub fn continuation_required(session: Option<&str>) -> bool {
    fn is_on(v: &str) -> bool {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
    }
    if let Ok(v) = std::env::var(CONTINUATION_REQUIRED_KEY) {
        if !v.trim().is_empty() {
            return is_on(&v);
        }
    }
    let lane = match session.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return false,
    };
    match crate::api::session_verbs::scoped_setting_in(
        &crate::api::session_verbs::home(),
        lane,
        CONTINUATION_REQUIRED_KEY,
    ) {
        Some(v) => is_on(&v),
        None => false,
    }
}

/// Why a claim was refused for want of a continuation, or that it was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationVerdict {
    /// Carries a next action a stranger could act on.
    Ok,
    /// Nothing at all in `next_action`.
    Missing,
    /// Present but not a sentence: "wip", "continue", "n/a".
    NotASentence,
}

/// Does this card say what the next actor should DO?
///
/// NO AUTO-SEED, and this is a correction to the plan as first written. Seeding
/// `next_action` from the card's own `desc` would make the gate satisfiable
/// without anyone thinking about the reader, which converts an enforcing gate
/// into a warn-only one with extra steps. AMUX-3854 is the proof: its desc is
/// "make it so this is all automatic", so a desc-seed would have produced
/// exactly the useless next action this gate exists to refuse, and it would
/// have looked like content.
///
/// AF-241 is a live card about dashboard toggles that control nothing. A gate
/// that anything can satisfy becomes one of those.
///
/// The floor is three words, borrowed from `is_a_sentence` above rather than
/// re-spelled, because the specimens are the same shape ("wip", "continue",
/// "still working on it" is four and passes, which is the honest edge: this
/// gate can force a sentence, not sincerity).
///
/// Deliberately NO upper bound. A length ceiling would be enforced by
/// truncation, and truncating the one field whose job is to survive the
/// author's context is the opposite of the point. The 300-800 token budget is
/// documented guidance in the migration, not a check.
pub fn continuation_verdict(next_action: &str) -> ContinuationVerdict {
    if next_action.trim().is_empty() {
        return ContinuationVerdict::Missing;
    }
    if !is_a_sentence(next_action) {
        return ContinuationVerdict::NotASentence;
    }
    ContinuationVerdict::Ok
}

/// Does entering `status` require a continuation?
///
/// SCOPED TO `doing` FOR NOW, and the narrowness is deliberate. `doing` is the
/// state where the gap actually bites: a lane claims a card, works, stops, and
/// the next reader has nothing. `review` already carries a reviewer and a diff,
/// `needsyou` is covered by the typed ask (0037), `backlog` is parked on an
/// external trigger, and terminal states have an outcome instead.
///
/// Widening this is a one-line change and a new cell. Starting wide would have
/// meant refusing transitions in four states on day one for a field nobody has
/// written yet.
pub fn continuation_applies(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Doing)
}

/// How much else is waiting behind this card (AF-318's owner view).
///
/// Deliberately a COUNT OF DEPENDENTS and not a guess at importance. A card
/// nobody is waiting on can sit for 58 days and cost nothing; a card three
/// lanes are blocked behind costs three lanes a day. The owner's scarce
/// resource is attention, so the ranking has to be by what clearing it
/// RELEASES, which is the one thing the board actually knows.
pub fn blast_radius(conn: &Connection, id: &str) -> i64 {
    let like = format!("%{id}%");
    conn.query_row(
        "SELECT COUNT(*) FROM issues WHERE deleted IS NULL AND archived = 0 \
         AND status NOT IN ('done','verified','discarded') \
         AND depends_on IS NOT NULL AND depends_on LIKE ?1",
        [&like],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Scope key for the AF-317 per-lane WIP limit on `todo`.
pub const TODO_WIP_LIMIT_KEY: &str = "AMUX_TODO_WIP_LIMIT";

/// How many `todo` cards one lane may hold. 0 disables the limit for that scope.
///
/// Five, because `todo` is the DISPATCH QUEUE and a dispatch queue longer than a
/// lane can work is not a queue, it is a pile.
///
/// THE JUSTIFICATION IS NARROWER THAN AF-317 CLAIMED, corrected here after the
/// first version shipped. That card's "358 todo cards, median age 28.8 days"
/// counted ARCHIVED rows; live it is 88 cards at a median of 0.8 days, so the
/// queue is not old. What survives is depth on a few lanes and Ethan's own
/// report: measured 2026-08-30, 22 lanes hold a live todo and 4 are over 5
/// (11, 9, 8, 6). Ethan, 2026-08-29: "some workers have an infinite # of
/// growing backlogs and todo then they go idle."
///
/// The limit is a CEILING ON QUEUEING, not on working: `backlog` is unbounded on
/// purpose and is where a card that is real but not next belongs.
pub fn todo_wip_limit(session: Option<&str>) -> i64 {
    let read = |v: String| v.trim().parse::<i64>().ok().filter(|n| *n >= 0);
    if let Ok(v) = std::env::var(TODO_WIP_LIMIT_KEY) {
        if let Some(n) = read(v) {
            return n;
        }
    }
    let lane = match session.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return TODO_WIP_LIMIT_DEFAULT,
    };
    crate::api::session_verbs::scoped_setting_in(
        &crate::api::session_verbs::home(),
        lane,
        TODO_WIP_LIMIT_KEY,
    )
    .and_then(read)
    .unwrap_or(TODO_WIP_LIMIT_DEFAULT)
}

/// Default ceiling on a lane's `todo` queue.
///
/// RAISED FROM 5 TO 20 on 2026-08-30, hours after shipping, because the number
/// that justified 5 did not survive. AF-317 asked for "start at 5" against a
/// measured "todo median age 28.8 days" — and that figure counted ARCHIVED
/// cards. Live it is 88 todo cards at a median of 0.8 DAYS. The queues are not
/// stale, so a working limit was the wrong instrument.
///
/// At 5 it fired 16 times in two hours against five lanes, eight of them at
/// `mvs-infra`, which is the single most active board user in the fleet (3,682
/// card events in 34h). Refusing the most productive lane's next card, to
/// enforce a ceiling derived from a statistic that turned out to be an artifact,
/// is a cost with nothing on the other side of it.
///
/// 20 is above every lane's live depth today (max 11, ETHAN) so it does not
/// interfere with normal work, and it still catches the pathology the card was
/// actually filed about — Ethan, 2026-08-29: "some workers have an infinite #
/// of growing backlogs and todo then they go idle." A ceiling is supposed to be
/// unhit in normal operation; `the_todo_wip_limit_refuses_the_next_card_and_
/// names_what_to_close` pins that it still fires, by setting the env override.
pub const TODO_WIP_LIMIT_DEFAULT: i64 = 20;

/// The predicate the WIP limit counts over.
///
/// Deliberately the SAME shape `board_drive`'s dispatch selector uses
/// (`owner_type='agent'`, real session, dormant types excluded): a limit that
/// counted rows the dispatcher never deals would refuse work over a queue that
/// does not exist. Detector-filed cards have `session IS NULL`, so they are
/// outside this by construction rather than by an exemption someone has to
/// remember — a fault report is never dropped because a lane was full.
const TODO_WIP_WHERE: &str = "session=?1 AND status='todo' AND owner_type='agent' \
     AND deleted IS NULL AND COALESCE(archived,0)=0 \
     AND COALESCE(type,'') NOT IN ('tripwire','watch','epic')";

/// How many `todo` cards this lane is holding, by the same predicate the
/// dispatcher selects from.
pub fn todo_wip_count(conn: &Connection, session: &str, excluding: &str) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM issues WHERE {TODO_WIP_WHERE} AND id <> ?2"),
        rusqlite::params![session, excluding],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// The lane's stalest `todo` cards: id, title, days since anyone touched it.
///
/// This is what the WIP refusal prints, and the choice of ORDER is the point.
/// Sorting by `updated` puts the cards the dispatcher has ALREADY stopped
/// dealing at the top — measured 2026-08-30 over live cards, 4 of the 72 in the
/// dispatch pool are past the 7-day freshness edge, median 9.9 days untouched,
/// and invisible to everyone. So the answer to "what do I close first" is the same list as
/// "what is already not being worked", and the refusal hands over both.
pub fn stalest_todos(conn: &Connection, session: &str, n: usize) -> Vec<(String, String, i64)> {
    let now = crate::config::now_f64();
    let mut out = Vec::new();
    if let Ok(mut st) = conn.prepare(&format!(
        "SELECT id, title, updated FROM issues WHERE {TODO_WIP_WHERE} \
         ORDER BY updated ASC LIMIT ?2"
    )) {
        if let Ok(rows) = st.query_map(rusqlite::params![session, n as i64], |r| {
            let updated: f64 = r.get(2)?;
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, ((now - updated) / 86_400.0) as i64))
        }) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Scope key for the AF-317 requirement that `blocked` name what it waits on.
pub const BLOCKED_NEEDS_WATCH_KEY: &str = "AMUX_BLOCKED_NEEDS_WATCH";

/// Must a card entering `blocked` name what would unblock it?
pub fn blocked_needs_watch(session: Option<&str>) -> bool {
    fn is_off(v: &str) -> bool {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
    }
    if let Ok(v) = std::env::var(BLOCKED_NEEDS_WATCH_KEY) {
        if !v.trim().is_empty() {
            return !is_off(&v);
        }
    }
    let lane = match session.filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => return true,
    };
    match crate::api::session_verbs::scoped_setting_in(
        &crate::api::session_verbs::home(),
        lane,
        BLOCKED_NEEDS_WATCH_KEY,
    ) {
        Some(v) => !is_off(&v),
        None => true,
    }
}

/// The env keys that tune (or disable) the automatic revisit date. `0` turns
/// the default off for a scope; a positive integer is a number of days.
pub const BACKLOG_REVISIT_DAYS_KEY: &str = "AMUX_BACKLOG_REVISIT_DAYS";
pub const NEEDSYOU_REVISIT_DAYS_KEY: &str = "AMUX_NEEDSYOU_REVISIT_DAYS";

/// How many days ahead to stamp a revisit date on a card entering `target`,
/// or `None` for statuses that do not get one.
///
/// # Why these two statuses have a default and the others do not
///
/// `backlog` and `needsyou` were the only two statuses in the whole vocabulary
/// with NO gate (`default_gates_for` returns `&[]` for both) and no exit any
/// automated loop can produce: `backlog` waits for a trigger, `needsyou` waits
/// for a human. Everything else has a next actor. Measured fleet-wide
/// 2026-08-29, that combination had eaten the board:
///
/// | status     | open cards | with a due date | with any revisit condition |
/// |------------|-----------:|----------------:|---------------------------:|
/// | `backlog`  |        589 |               0 |                34 (6%)     |
/// | `needsyou` |        374 |               2 |                16 (4%)     |
/// | `todo`     |         64 |               - |                    -       |
///
/// 963 of the 1029 open cards (94%) sat in the two statuses nothing drains,
/// against 64 the drive loop can dispatch — and one lane held 219 backlog
/// cards, 147 of them untouched for over two weeks, while reporting idle.
/// That is the state Ethan named: "some workers have an infinite # of growing
/// backlogs and todo then they go idle". The drive loop is not at fault and
/// its own trace says so correctly ("its queue is real and its workable queue
/// is empty"); the defect is one level up, in a board that lets a card enter a
/// status from which nothing can ever move it.
///
/// # Why a DEFAULT and not a gate
///
/// The obvious fix is to refuse the transition until the caller names a
/// revisit condition. That would be an opt-in mechanism wearing a gate's
/// clothes: it reaches only the callers who learn the flag, and 96% of current
/// practice would start failing at once (ethos rule 3 — a constraint every
/// legitimate state trips is not a constraint, it is a wall). Stamping a date
/// reaches every card by default and nothing has to be acknowledged, so
/// `gate_ack` cannot fake it and no honest transition is refused (rule 1:
/// prefer opt-out over opt-in).
///
/// The date is the OWNER'S to change and the card carries the stamp in its own
/// log, so a wrong default is visible and one PATCH away from corrected. It
/// never deletes, archives or discards anything — the sweep that would is
/// rule 8's territory and stays Ethan's call (AMUX-2499).
pub fn default_revisit_days(target: TaskStatus, session: Option<&str>) -> Option<i64> {
    let (key, fallback) = match target {
        TaskStatus::Backlog => (BACKLOG_REVISIT_DAYS_KEY, 14),
        TaskStatus::NeedsYou => (NEEDSYOU_REVISIT_DAYS_KEY, 3),
        _ => return None,
    };
    let raw = scoped_or_process_env(key, session);
    let days = match raw {
        Some(v) => v.trim().parse::<i64>().ok()?,
        None => fallback,
    };
    (days > 0).then_some(days)
}

/// Read a setting from the process env first (the global operator switch in
/// `~/.amux/server.env`), then the worker > group > global scope ladder — the
/// same resolution order [`done_link_required`] uses, shared rather than
/// re-derived so the two cannot drift.
fn scoped_or_process_env(key: &str, session: Option<&str>) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    let lane = session.filter(|s| !s.is_empty())?;
    crate::api::session_verbs::scoped_setting_in(
        &crate::api::session_verbs::home(),
        lane,
        key,
    )
}

/// `YYYY-MM-DD`, `days` from now in LOCAL time — the format every existing
/// `due` value on the board already uses, so a stamped date sorts and compares
/// against a hand-set one by plain string comparison.
pub fn revisit_date(days: i64) -> String {
    (chrono::Local::now() + chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

/// True when `due` is a date that has arrived (today or earlier), by string
/// comparison against today in local time. A malformed or empty `due` is NOT
/// due — an unparseable date must never promote a card, because "I could not
/// read this" and "the date arrived" are different answers (ethos rule 4).
pub fn revisit_arrived(due: Option<&str>, today: &str) -> bool {
    let d = due.unwrap_or("").trim();
    // Exactly `YYYY-MM-DD`: 10 chars, digits and dashes in the right places.
    let ok = d.len() == 10
        && d.as_bytes()[4] == b'-'
        && d.as_bytes()[7] == b'-'
        && d.bytes().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 { c == b'-' } else { c.is_ascii_digit() }
        });
    ok && d <= today
}

/// Extract every distinct pointer to a produced artifact from worker-authored
/// card text. This is the canonical parser used by BOTH the done-link gate and
/// the card detail response: a reference accepted as proof must not disappear
/// from the UI that is supposed to show that proof.
///
/// Accepted shapes are http(s) URLs, markdown-link targets, absolute or
/// repo-relative file paths (`a/b.ext`), commit-sha-shaped tokens (7..=40 hex),
/// and `#<number>` PR/issue references. Order is stable within each shape and
/// duplicates are removed. Markdown targets are visited first so their clean
/// target wins over the surrounding punctuation a token scan would see.
pub fn asset_refs(text: &str) -> Vec<String> {
    static MARKDOWN: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static NUMBER_REF: OnceLock<Regex> = OnceLock::new();
    let markdown = MARKDOWN
        .get_or_init(|| Regex::new(r#"\[[^\]]*\]\(\s*([^\s\)]+)"#).expect("asset markdown regex"));
    let url = URL.get_or_init(|| Regex::new(r#"https?://[^\s<>\"']+"#).expect("asset url regex"));
    let number_ref = NUMBER_REF
        .get_or_init(|| Regex::new(r"(?:^|\s)(#\d+)\b").expect("asset ref regex"));

    fn file_like_component(part: &str) -> bool {
        if let Some(name) = part.strip_prefix('.') {
            return !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
        }
        let Some((stem, ext)) = part.rsplit_once('.') else { return false };
        !stem.is_empty()
            && stem.chars().any(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '-'))
            && (1..=12).contains(&ext.len())
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
    }
    fn ambiguous_joined_files(value: &str) -> bool {
        !value.contains("://")
            && value
                .split('/')
                .filter(|part| !part.starts_with('.') && file_like_component(part))
                .count()
                > 1
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |raw: &str| {
        let clean = raw
            .trim()
            .trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}'));
        // `plan.md/result-a.txt/result-b.txt` is a prose list missing spaces,
        // not one produced file. Accepting it manufactured a clickable path in
        // the repo that could never exist. A dotted directory is possible, but
        // two file-shaped path components are ambiguous evidence and should be
        // written as separate pointers by the worker.
        if !clean.is_empty()
            && !ambiguous_joined_files(clean)
            && seen.insert(clean.to_string())
        {
            out.push(clean.to_string());
        }
    };

    for caps in markdown.captures_iter(text) {
        if let Some(target) = caps.get(1) {
            push(target.as_str());
        }
    }
    for m in url.find_iter(text) {
        push(m.as_str());
    }
    for raw in text.split_whitespace() {
        // A compiler/test flag can contain a perfectly file-shaped value, but
        // the flag itself is not a produced asset (`--config=e2e/x.ts` was
        // rendered as a missing file on ATE-37). Keep it out before trimming.
        if raw.starts_with('-') || raw.contains('=') {
            continue;
        }
        let tok = raw.trim_matches(|c: char| {
            !c.is_ascii_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-' && c != '~'
        });
        if tok.is_empty() || tok.contains("://") || raw.contains("](") {
            continue;
        }
        if let Some((dir, last)) = tok.rsplit_once('/') {
            if !dir.is_empty() || tok.starts_with('/') {
                let hidden_file = last.strip_prefix('.').is_some_and(|name| {
                    !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                });
                let ordinary_file = last.rsplit_once('.').is_some_and(|(stem, ext)| {
                    !stem.is_empty()
                        && stem
                            .chars()
                            .any(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '-'))
                        && (1..=12).contains(&ext.len())
                        && ext.chars().all(|c| c.is_ascii_alphanumeric())
                        && ext.chars().any(|c| c.is_ascii_alphabetic())
                });
                if hidden_file || ordinary_file {
                    push(tok);
                    continue;
                }
            }
        } else if file_like_component(tok)
            && tok
                .rsplit_once('.')
                .is_some_and(|(_, ext)| ext.chars().any(|c| c.is_ascii_alphabetic()))
        {
            // A produced file is very often reported as a filename because
            // the worker already named the containing folder in the previous
            // sentence. Requiring a slash made `launch.mp4` disappear from the
            // card even though `./launch.mp4` was accepted. The alphabetic
            // extension guard keeps versions such as `release.2026` out.
            push(tok);
            continue;
        }
        if (7..=40).contains(&tok.len()) && tok.bytes().all(|c| c.is_ascii_hexdigit()) {
            push(tok);
        }
    }
    for caps in number_ref.captures_iter(text) {
        if let Some(reference) = caps.get(1) {
            push(reference.as_str());
        }
    }
    out
}

/// References in model-authored prose that are explicitly presented as
/// outputs. Generic activity text routinely names input files, peer-owned
/// dirty files, or a different card's commit; treating every path-like token
/// as produced is the ATE-39 misattribution class.
pub fn output_asset_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    const MARKERS: [&str; 8] = [
        "produced ",
        "created ",
        "artifact: ",
        "artifacts: ",
        "output: ",
        "outputs: ",
        "wrote ",
        "generated ",
    ];
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        for marker in MARKERS {
            let Some(i) = lower.find(marker) else { continue };
            let tail = &line[i + marker.len()..];
            for reference in asset_refs(tail) {
                if seen.insert(reference.clone()) {
                    out.push(reference);
                }
            }
            break;
        }
    }
    out
}

/// True when `text` contains at least one produced-asset pointer. Deliberately
/// a projection of [`asset_refs`] so transition enforcement and card rendering
/// cannot grow two subtly different definitions of evidence.
pub fn has_asset_link(text: &str) -> bool {
    !asset_refs(text).is_empty()
}

pub fn default_gates_for(item_type_raw: &str, target: TaskStatus) -> Vec<String> {
    let ty = core_item_type(item_type_raw);
    let list: &[&str] = match (ty, target) {
        // Dormant types (tripwire/watch): honest gates for what they ARE.
        (ItemType::Tripwire | ItemType::Watch, TaskStatus::Doing) => &[
            "Trigger condition documented on the card",
            "Armed and monitoring",
        ],
        (ItemType::Tripwire | ItemType::Watch, TaskStatus::Review) => {
            &["Fired: evidence of the triggering event recorded"]
        }
        (ItemType::Tripwire | ItemType::Watch, TaskStatus::Done) => {
            &["Fired and handled, or deliberately stood down (which, and why, on the card)"]
        }
        (ItemType::Tripwire | ItemType::Watch, TaskStatus::Verified) => {
            &["Outcome confirmed (handled recurrence, or stand-down still correct)"]
        }
        // Code (and unknown-typed legacy rows): the global status defaults.
        (ItemType::Code, TaskStatus::Doing) => &[
            "Scope & acceptance criteria are clear",
            "No blocking dependency",
            "Has an owner",
        ],
        (ItemType::Code, TaskStatus::Review) => &[
            "Implemented and self-tested",
            "Diff / PR is up",
            "Ready for another set of eyes",
        ],
        (ItemType::Code, TaskStatus::Done) => &["Implemented and merged", "Tests / lint pass"],
        (ItemType::Code, TaskStatus::Verified) => &[
            "CI/CD green (if e2e infra is unavailable, note why — that is not a failure)",
            "Deployed to prod",
            "Confirmed working in prod",
            "Zero regressions",
        ],
        // Decision (AF-323): a card whose only output is an answer from the
        // person who owns the call. The non-code default below would ALMOST fit,
        // but its `done` bar ("what happened, and why it is closed") is silent on
        // the one thing that must be on the card — WHO decided, and WHAT they
        // chose. An unrecorded decider is how a settled question gets re-asked,
        // and re-asking is the cost this type exists to stop.
        //
        // `doing` deliberately does NOT ask for an owner in the code sense. The
        // owner of a decision is the person who will answer, not the lane holding
        // the card, and requiring "has an owner" of a lane that is waiting on
        // Ethan asks it to assert something false.
        (ItemType::Decision, TaskStatus::Doing) => &[
            "The choice is stated as a question with its options",
            "Named the person whose call this is",
        ],
        (ItemType::Decision, TaskStatus::Review) => {
            &["Options and their trade-offs are written up", "Ready for the decider"]
        }
        (ItemType::Decision, TaskStatus::Done) => {
            &["The decision is recorded on the card: what was chosen, by whom, and when"]
        }
        (ItemType::Decision, TaskStatus::Verified) => {
            &["The decision has been acted on, and still holds"]
        }
        // Every other (non-code, non-dormant) type: the honest non-code bar.
        (_, TaskStatus::Doing) => &["Scope is clear", "Has an owner"],
        (_, TaskStatus::Review) => &["Findings written up", "Ready for another set of eyes"],
        (_, TaskStatus::Done) => {
            &["Outcome recorded in the item (what happened, and why it is closed)"]
        }
        (_, TaskStatus::Verified) => &["Outcome confirmed to still hold"],
        // No status outside doing/review/done/verified is gated by default.
        _ => &[],
    };
    list.iter().map(|s| s.to_string()).collect()
}

/// The gate actually enforced for `row` entering `target` — the full scoped
/// ladder (worker > group > global; see [`effective_gate_scoped`]), with the
/// worker's groups resolved from its CC_TAGS.
///
/// [`effective_gate`] remains for callers with no DB handle and is the ladder
/// minus every stored tier. They must not drift: this one delegates rather
/// than re-deriving, so a change to type defaults cannot land in one and not
/// the other.
pub fn effective_gate_configured(
    conn: &rusqlite::Connection,
    row: &IssueRow,
    target: TaskStatus,
) -> Vec<String> {
    let groups = row
        .session
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(crate::api::session_verbs::lane_groups)
        .unwrap_or_default();
    effective_gate_scoped(conn, row, target, &groups)
}

/// The full gate precedence, with the worker's groups passed in so tests can
/// exercise every tier hermetically (lane_groups reads env files under
/// AMUX_HOME, which parallel tests cannot safely fake).
///
/// Most specific first (RR-0051, Ethan 2026-08-11: "worker takes priority
/// over all, followed by group, then global"):
///   1. the card's own `gate` override — one card, deliberately special;
///   2. WORKER: `session_gates` row for the card's session — this table was
///      written by the SPA's per-worker gate editor since AMUX-2599 and,
///      until today, read by NOTHING at enforcement time: a user could author
///      a worker gate, watch the UI display it, and have every transition
///      judged by a different one (ethos rule 6 — the claim without the
///      implementation);
///   3. GROUP: `session_gates` rows keyed `group:<name>` for each of the
///      worker's groups (CC_TAGS), unioned in sorted-group order when the
///      worker is in several — all its groups' bars apply, deterministically;
///   4. GLOBAL: the operator-authored column gate (`statuses.gate_custom`);
///   5. the type-aware defaults.
///
/// Every tier fails CLOSED to the next: an absent, empty, or malformed row
/// means "inherit", never "no gate" (an empty gate would silently open the
/// strictest transitions on the board — same rule as `configured_gate`).
pub fn effective_gate_scoped(
    conn: &rusqlite::Connection,
    row: &IssueRow,
    target: TaskStatus,
    groups: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    effective_gate_with_source(conn, row, target, groups).0
}

/// WHICH TIER PRODUCED THE GATE (AF-169).
///
/// The refusal body has always told operators "the gate is DERIVED from the
/// type — set its type", and that is true only when the TYPE DEFAULT is what
/// refused. For a card in a scope with a custom gate, retyping changes nothing
/// and the operator gets a retyped card and the same refusal. AF-168's reporter
/// retyped TUBES-2053 code -> research, watched the done gate not re-derive, and
/// concluded the override was pinned per-card; the hint is what sent them there.
///
/// The precedence walk already knows which tier won — it returns the moment one
/// matches — so the source costs nothing to report and is returned alongside
/// rather than re-derived by a second walk. Two walks would be two spellings of
/// the precedence to keep in step, which is the duplication this file's own
/// `KNOWN_TYPES` comment warns about one type over.
///
/// It varies by TRANSITION, not just by scope: measured 2026-08-23,
/// `group:amux` pins only `verified`, `tubescience` only `done`, `amux-cloud`
/// both `review` and `verified`. So the same card can have a type-derived gate
/// at one transition and a scope-derived one at the next, and a hint keyed on
/// "does this scope have an override" would be wrong half the time.
pub fn effective_gate_with_source(
    conn: &rusqlite::Connection,
    row: &IssueRow,
    target: TaskStatus,
    groups: &std::collections::BTreeSet<String>,
) -> (Vec<String>, GateSource) {
    let t = effective_gate_trail(conn, row, target, groups);
    (t.criteria, t.source)
}

/// One tier of the gate precedence, recorded AS CONSULTED (AMUX-3607).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GateLayer {
    /// `card` | `worker` | `group` | `column` | `type_default`.
    pub layer: &'static str,
    /// Which worker, which group, which type — the identity of the scope that
    /// was asked, so a reader can go look at the same row.
    pub scope: Option<String>,
    /// `applied` | `outranked` | `silent` | `not_applicable`.
    ///
    /// `outranked` is the load-bearing one and the reason this type exists.
    /// Under the old early-returning walk it was UNOBSERVABLE: when the card
    /// override won, nothing ever asked the worker layer, so a layer that held
    /// a real gate and a layer that held nothing were the same absence. An
    /// authorisation trail that cannot say "this rule existed and lost" answers
    /// "what applied" but not "why was this allowed", which is the question.
    pub verdict: &'static str,
    /// What this layer WOULD have imposed. Present on `outranked` too, because
    /// the rejected rule is the content of the answer, not context for it.
    pub criteria: Vec<String>,
}

/// The whole precedence walk, with every tier's verdict.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GateTrail {
    pub criteria: Vec<String>,
    #[serde(skip)]
    pub source: GateSource,
    /// Highest precedence first, always all five tiers.
    pub layers: Vec<GateLayer>,
}

impl GateTrail {
    /// The trail as ONE line for `issues.log` (AMUX-3607).
    ///
    /// Goes on the card's own append-only history rather than into a new store,
    /// deliberately. That log is where the transition is already recorded, it is
    /// not reaped, `/api/why` already reads it, and the History tab already
    /// renders it — so the authorisation record lands where someone asking "why
    /// was this allowed" is already looking, instead of in a table they would
    /// have to know to open. Ethos rule 4's second layer: a tag in a store the
    /// reader never opens is the same failure as no tag.
    ///
    /// Compact and greppable on purpose. `grep 'authz:' ` finds every
    /// authorisation decision on a card; `grep 'outranked'` finds every one
    /// where a rule existed and lost, which is the question the winning layer
    /// alone cannot answer.
    ///
    /// The count is the number of criteria that tier held, so a reader can tell
    /// an outranked tier with a real bar from one with a trivial one without
    /// the line carrying every criterion string.
    pub fn log_line(&self) -> String {
        let parts: Vec<String> = self
            .layers
            .iter()
            .map(|l| {
                let name = match (&l.scope, l.layer) {
                    // The card tier's scope is the card's own id, which the log
                    // line already lives on — repeating it is noise.
                    (_, "card") | (_, "column") | (None, _) => l.layer.to_string(),
                    (Some(s), "type_default") => format!("type:{s}"),
                    (Some(s), n) => format!("{n}:{s}"),
                };
                match l.verdict {
                    "silent" => format!("{name}=silent"),
                    "not_applicable" => format!("{name}=n/a"),
                    v => format!("{name}={v}({})", l.criteria.len()),
                }
            })
            .collect();
        format!("authz: {}", parts.join(" "))
    }
}

/// THE precedence walk. `effective_gate_with_source` is a projection of this,
/// deliberately, so there is one implementation and not two spellings to keep in
/// step (the duplication this file's own `KNOWN_TYPES` comment warns about, and
/// the shape ethos rule 1's corollary names: a view that re-derives its
/// predicate instead of sharing it drifts the moment either side changes).
///
/// It no longer early-returns. That costs, measured 2026-08-24 before writing
/// it rather than after: `session_gates` holds 4 rows behind a PK autoindex on
/// (session, status) and `statuses` holds 7, so consulting every tier is a
/// handful of trivial indexed probes. The early return was saving nothing worth
/// the blindness it caused.
pub fn effective_gate_trail(
    conn: &rusqlite::Connection,
    row: &IssueRow,
    target: TaskStatus,
    groups: &std::collections::BTreeSet<String>,
) -> GateTrail {
    let session = row.session.as_deref().filter(|s| !s.is_empty());

    // Consult everything FIRST, decide after. Interleaving the two is what made
    // "consulted and empty" and "never asked" indistinguishable.
    let card = row.gate_criteria();
    let worker = session.and_then(|s| scoped_gate(conn, s, target));
    let mut group_merged: Vec<String> = Vec::new();
    let mut group_hits: Vec<String> = Vec::new();
    if session.is_some() {
        for group in groups {
            if let Some(list) = scoped_gate(conn, &format!("group:{group}"), target) {
                group_hits.push(group.clone());
                for c in list {
                    if !group_merged.contains(&c) {
                        group_merged.push(c);
                    }
                }
            }
        }
    }
    let column = configured_gate(conn, target);
    // `default_gates_for`, NOT `effective_gate`: the latter returns the CARD
    // OVERRIDE when one exists, so using it here made the type tier report the
    // card's criteria as its own — a tier claiming a rule it does not hold, in
    // the one record meant to say which rule came from where. Caught by
    // asserting the audit line as a whole string; a substring check would have
    // passed. Equivalent for the WINNER, which reaches this tier only when the
    // override is empty and the two agree by definition.
    let type_default = default_gates_for(&row.item_type, target);

    let (criteria, source, winner) = if !card.is_empty() {
        (card.clone(), GateSource::Card, "card")
    } else if let Some(g) = worker.clone() {
        (g, GateSource::Worker(session.unwrap_or("").to_string()), "worker")
    } else if !group_merged.is_empty() {
        (
            group_merged.clone(),
            GateSource::Group(groups.iter().cloned().collect::<Vec<_>>().join(", ")),
            "group",
        )
    } else if let Some(c) = column.clone() {
        (c, GateSource::Column, "column")
    } else {
        (type_default.clone(), GateSource::TypeDefault, "type_default")
    };

    // `held` = this tier actually had a rule. A tier that held one and did not
    // win was OUTRANKED; one that held nothing was SILENT and could never have
    // applied. Same row count, opposite meanings.
    let layer = |name: &'static str, scope: Option<String>, held: Option<Vec<String>>| -> GateLayer {
        let (verdict, criteria) = match held {
            _ if name == winner => ("applied", criteria.clone()),
            Some(c) => ("outranked", c),
            None => ("silent", vec![]),
        };
        GateLayer { layer: name, scope, verdict, criteria }
    };

    let layers = vec![
        layer("card", Some(row.id.clone()), (!card.is_empty()).then_some(card)),
        // A card with no session has no worker or group tier to consult at all.
        // Reporting that as `silent` would claim an empty answer from a scope
        // nobody asked, which is the same over-claim one layer along.
        match session {
            Some(s) => layer("worker", Some(s.to_string()), worker),
            None => GateLayer {
                layer: "worker",
                scope: None,
                verdict: "not_applicable",
                criteria: vec![],
            },
        },
        match session {
            Some(_) => layer(
                "group",
                (!group_hits.is_empty()).then(|| group_hits.join(", ")),
                (!group_merged.is_empty()).then_some(group_merged),
            ),
            None => GateLayer {
                layer: "group",
                scope: None,
                verdict: "not_applicable",
                criteria: vec![],
            },
        },
        layer("column", None, column),
        layer("type_default", Some(row.item_type.clone()), Some(type_default)),
    ];

    GateTrail { criteria, source, layers }
}

/// Which tier of the precedence produced a card's gate for one transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateSource {
    /// The card's own `gate` column — one card, deliberately special.
    Card,
    /// A `session_gates` row for the card's own worker.
    Worker(String),
    /// One or more `group:<name>` rows, unioned.
    Group(String),
    /// The operator-authored column gate (`statuses.gate_custom`).
    Column,
    /// The item type's default — the ONLY tier retyping can change.
    TypeDefault,
}

impl GateSource {
    /// Can the operator change this gate by retyping the card? Only the type
    /// default derives from the type; every other tier ignores it, which is
    /// exactly what makes the `wrong_type?` hint false elsewhere.
    pub fn retype_would_help(&self) -> bool {
        matches!(self, GateSource::TypeDefault)
    }

    /// The tier as a stable token, for a CLIENT that must branch on it rather
    /// than show it (AMUX-3573).
    ///
    /// `explain()` below is prose and it is the right thing for a refusal body,
    /// but the SPA needs to decide whether to render a badge and which one, and
    /// the only alternatives to a token are parsing that sentence or inferring
    /// from `retype_would_help` — which cannot separate Worker from Group from
    /// Column, the three tiers a human most needs told apart. Kept short and
    /// lowercase because it is an identifier, not a label.
    pub fn token(&self) -> &'static str {
        match self {
            GateSource::Card => "card",
            GateSource::Worker(_) => "worker",
            GateSource::Group(_) => "group",
            GateSource::Column => "column",
            GateSource::TypeDefault => "type",
        }
    }

    /// The named scope the gate came from (`amux`, `group:amux`, …), or empty
    /// for tiers that have no scope to name. Separate from `token` so a client
    /// can render "group amux" without string-splitting the token.
    pub fn scope(&self) -> String {
        match self {
            GateSource::Worker(w) => w.clone(),
            GateSource::Group(g) => g.clone(),
            GateSource::Card | GateSource::Column | GateSource::TypeDefault => String::new(),
        }
    }

    /// A sentence for the refusal body, so the operator learns WHERE the bar
    /// came from instead of being sent to change something irrelevant.
    pub fn explain(&self) -> String {
        match self {
            GateSource::Card => "this card carries its own `gate` override, so the type is \
                 not what refused; edit the card's gate or ack it honestly"
                .to_string(),
            GateSource::Worker(w) => format!(
                "this gate comes from the `{w}` WORKER scope, not from the item type — \
                 retyping will NOT change it. See GET /api/board/session-gates."
            ),
            GateSource::Group(g) => format!(
                "this gate comes from the GROUP scope ({g}), not from the item type — \
                 retyping will NOT change it. See GET /api/board/session-gates."
            ),
            GateSource::Column => "this gate comes from the operator-authored COLUMN gate, \
                 not from the item type — retyping will NOT change it"
                .to_string(),
            GateSource::TypeDefault => "this gate is the item TYPE's default, so correcting \
                 the type is the honest fix if the type is wrong"
                .to_string(),
        }
    }
}

/// One scope's gate row from `session_gates` (scope key is a session name or
/// `group:<name>`), or None when the row is absent, empty, or unreadable —
/// every "cannot tell" inherits the next tier rather than opening the gate.
fn scoped_gate(
    conn: &rusqlite::Connection,
    scope: &str,
    target: TaskStatus,
) -> Option<Vec<String>> {
    let id = status_to_db(target, "");
    let gate: Option<String> = conn
        .query_row(
            "SELECT gate FROM session_gates WHERE session = ?1 AND status = ?2",
            rusqlite::params![scope, id],
            |r| r.get(0),
        )
        .ok()?;
    let list: Vec<String> = serde_json::from_str(&gate?).ok()?;
    let list: Vec<String> = list
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

/// The operator-authored gate for a column, or None.
///
/// Returns None for a seeded row, an empty list, or unreadable JSON — every
/// "cannot tell" answer falls back to the type defaults rather than to an empty
/// gate. An empty gate would mean NO gate, so a malformed row must never read as
/// permission (it would silently open the strictest transitions on the board).
pub fn configured_gate(conn: &rusqlite::Connection, target: TaskStatus) -> Option<Vec<String>> {
    let id = status_to_db(target, "");
    let (gate, custom): (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT gate, gate_custom FROM statuses WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    if custom.unwrap_or(0) != 1 {
        return None;
    }
    let list: Vec<String> = serde_json::from_str(&gate?).ok()?;
    let list: Vec<String> = list
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

pub fn effective_gate(row: &IssueRow, target: TaskStatus) -> Vec<String> {
    let over = row.gate_criteria();
    if !over.is_empty() {
        return over;
    }
    default_gates_for(&row.item_type, target)
}

/// Wrap criterion strings into a core [`Gate`] guarding `target`, so
/// [`board::apply_transition`] / [`board::why_blocked`] enforce and explain
/// them through the one shared code path. Criteria verify as `ModelJudgment`
/// (an acknowledgement IS a judgment call, recorded as `ModelTranscript`
/// evidence when the caller acks honestly) — the free verifier kinds land
/// when gates become first-class stored entities (RR-0051/Invariant 18).
pub fn core_gates(criteria: &[String], target: TaskStatus) -> Vec<Gate> {
    if criteria.is_empty() {
        return Vec::new();
    }
    vec![Gate {
        id: gate_id_for(target),
        scope: amux_core::scope::Scope::Global,
        guards: target,
        applies_to_types: None,
        criteria: criteria
            .iter()
            .map(|c| GateCriterion {
                description: c.clone(),
                verifier: VerifierKind::ModelJudgment { prompt: c.clone() },
                required: true,
            })
            .collect(),
    }]
}

// ---------------------------------------------------------------------------
// Log convention
// ---------------------------------------------------------------------------

/// Append one history line exactly the way Python's `_append_board_log`
/// does: `` (log.rstrip() + "\n`HH:MM` " + line).strip() `` — so logs written
/// by either server interleave without corrupting each other's lines.
pub fn append_log(existing: Option<&str>, hhmm: &str, line: &str) -> String {
    let base = existing.unwrap_or("").trim_end();
    format!("{base}\n`{hhmm}` {line}").trim().to_string()
}

// ---------------------------------------------------------------------------
// Id minting (shared issue_counters table)
// ---------------------------------------------------------------------------

/// Replicates Python `_prefix_from_session`: 'my-project' -> "MP",
/// single-word 'orch' -> "ORCH", empty -> "AMUX". Both servers must derive
/// the identical prefix or the shared counters stop preventing collisions.
pub fn prefix_from_session(session: &str) -> String {
    let words: Vec<&str> = session
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|w| !w.is_empty())
        .collect();
    let clean = |s: String| -> String {
        s.chars()
            .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .take(5)
            .collect()
    };
    let raw = match words.len() {
        0 => return "AMUX".into(),
        1 => clean(words[0].to_uppercase()),
        _ => clean(
            words
                .iter()
                .filter_map(|w| w.chars().next())
                .collect::<String>()
                .to_uppercase(),
        ),
    };
    if raw.is_empty() {
        "AMUX".into()
    } else {
        raw
    }
}

/// Replicates Python `_next_issue_id` byte-for-byte against the SAME
/// `issue_counters` rows: seed the prefix at 1, post-increment, return
/// `<prefix>-<n>`. Because both servers use this one shared counter, ids
/// minted by either can never collide.
pub fn next_issue_id(conn: &Connection, prefix: &str) -> rusqlite::Result<String> {
    conn.execute(
        "INSERT OR IGNORE INTO issue_counters (prefix, next_n) VALUES (?1, 1)",
        params![prefix],
    )?;
    let n: i64 = conn.query_row(
        "UPDATE issue_counters SET next_n = next_n + 1 WHERE prefix = ?1 RETURNING next_n - 1",
        params![prefix],
        |r| r.get(0),
    )?;
    Ok(format!("{prefix}-{n}"))
}

// ---------------------------------------------------------------------------
// Row struct + queries
// ---------------------------------------------------------------------------

/// One live `issues` row, Python column shapes preserved: raw status/type
/// strings, unix-second ints, JSON-array TEXT columns decoded to vecs.
///
/// `Default` is derived so a test can name the ONE field it cares about and
/// leave the other twenty-five alone. A test that has to spell out an entire row
/// to check one property is a test that stops being written.
#[derive(Debug, Clone, Default)]
pub struct IssueRow {
    /// The semantic id ("AMUX-123") — the wire identity. See [`internal_id`].
    pub id: String,
    pub title: String,
    pub desc: String,
    /// RAW status spelling as stored (e.g. `needsyou`). Parse via
    /// [`parse_status`]; write via [`status_to_db`].
    pub status: String,
    /// Owner worker NAME (the Python board speaks names, not WorkerIds).
    pub session: Option<String>,
    pub creator: String,
    pub due: Option<String>,
    /// Unix seconds (INTEGER in the live schema).
    pub created: i64,
    /// Unix seconds.
    pub updated: i64,
    pub owner_type: String,
    pub due_time: Option<String>,
    pub pinned: i64,
    pub gcal_event_id: Option<String>,
    pub pos: f64,
    pub notified: i64,
    /// Card-level gate override: JSON array TEXT, or NULL.
    pub gate: Option<String>,
    pub shepherd: Option<String>,
    /// RAW type string — legacy values are exposed as-is; [`core_item_type`]
    /// maps them for gate/state-machine purposes only.
    pub item_type: String,
    pub archived: i64,
    /// Decoded from the JSON-array TEXT column (semantic ids).
    pub depends_on: Vec<String>,
    pub reviewer: Option<String>,
    /// The epic this card rolls up under: the semantic id of a type=epic card,
    /// or NULL (AMUX-2992). Not a foreign key — a dangling id reads as no-epic.
    pub epic: Option<String>,
    /// When this card entered a TERMINAL status (done/verified/discarded), unix
    /// seconds; cleared when it leaves one (AMUX-3609).
    ///
    /// NULL means NOT RECORDED, never "not closed". Most of the board predates
    /// the column and its journal rows were reaped at 14 days, so a consumer
    /// that reads NULL as still-open will be wrong about almost every card.
    /// Filter on `closed_at IS NOT NULL` when you need a date; `IS NULL` means
    /// nothing.
    pub closed_at: Option<i64>,
    /// Append-only history (see [`append_log`]); NULL until first line.
    pub log: Option<String>,
    /// The Python optimistic-concurrency counter (`expect_rev` checks this).
    pub rev: i64,
    pub source_ref: Option<String>,
    /// What was actually RUN or produced to close this card (AF-321): a
    /// command, a URL exercised, a screenshot path, a commit sha.
    ///
    /// Its own column on purpose. `done_requires_asset_link` looks for a
    /// path/sha/URL-shaped token anywhere in `desc`, which the card's own
    /// PROBLEM STATEMENT supplies: measured 2026-08-29, 843 of 1372 open cards
    /// (61%) satisfied that gate on their filed text with no work done. A field
    /// nobody has written yet cannot be filled by the statement of the problem,
    /// which is the whole property this column buys.
    ///
    /// NULL means NOT RECORDED, never "no evidence exists" — most of the board
    /// predates the column. The honest "there is no artifact" answer is the
    /// text `none: <reason>`, which is stored, countable, and deliberately NOT
    /// the same as NULL.
    pub evidence: Option<String>,
    /// Which of [`ASK_TYPES`] this card is waiting on (AF-318).
    ///
    /// NULL means NOT RECORDED, never "no ask exists" — the 445 cards that
    /// predate the column are all NULL, and a sweep that reads NULL as
    /// "untyped, therefore junk" would discard real asks along with the rest.
    /// The gate applies to the TRANSITION, so history is left as it is and the
    /// backlog is drained by re-asking, not by a migration guessing.
    pub ask_type: Option<String>,
    /// One sentence: what is being asked.
    pub ask_question: Option<String>,
    /// One sentence: what ends the block. The half that makes an ask
    /// falsifiable — without it nobody but the author can tell whether an
    /// answer landed.
    pub ask_unblocks: Option<String>,
    /// Specific person or external actor whose response is required. Generic
    /// values such as "human" are refused by the needsyou gate.
    pub ask_actor: Option<String>,
    /// The continuation contract (AMUX-3946). `next_action` is what the next
    /// actor should DO and is the only one gated; `last_result` is what the
    /// previous attempt produced; `unresolved` is what is still open.
    ///
    /// `unresolved` is deliberately ungated: requiring it would make every card
    /// invent an open question to be claimable, and a manufactured question is
    /// worse than none because it reads as a real one.
    /// When the card entered its CURRENT status (AMUX-3947). `None` means the
    /// card predates migration 0040 and has not moved since: not measured, and
    /// consumers must render it that way rather than as zero.
    pub entered_state_at: Option<i64>,
    /// WHERE the card came from, as a KIND: `agent` for a real create through the
    /// API, `capture` for an auto-captured human prompt, and the name of the job
    /// for a card a runtime loop filed. NULL means the row predates the field
    /// (AF-367) and is NOT a claim about which population it belonged to.
    pub source: Option<String>,
    /// What this card is waiting for, orthogonal to `status` (AMUX-3949). NULL
    /// means not blocked. A card keeps its lifecycle POSITION and separately
    /// says it is stuck, which `status='blocked'` could not express.
    ///
    /// The legacy `blocked` status is grandfathered and still in use by 66 cards
    /// belonging to other lanes, so every consumer must honour BOTH spellings.
    pub blocked_on: Option<String>,
    pub next_action: Option<String>,
    pub last_result: Option<String>,
    pub unresolved: Option<String>,
    pub last_verified_at: Option<i64>,
    /// Rust per-row version (migration 0002). Bumped alongside `rev`.
    pub version: i64,
    pub tags: Vec<String>,
    /// JSON array of measurable acceptance criteria (migration 0045).
    /// NULL means not specified, not "no criteria exist".
    pub acceptance_criteria: Option<String>,
    /// Structured decision fields (migration 0045). Only meaningful when
    /// `item_type == "decision"`.
    pub decision_question: Option<String>,
    pub decision_rationale: Option<String>,
    /// Semantic id of the decision this one supersedes.
    pub decision_supersedes: Option<String>,
    /// Structured wait, orthogonal to status (migration 0048). JSON object:
    /// `{"actor":"human","type":"judgment","question":"...","unblocks":"..."}`.
    /// NULL means not waiting on anyone specific. A card keeps its lifecycle
    /// position and separately declares who it is waiting on.
    pub waiting_on: Option<String>,
    /// Server-verified worker that created this card for a different worker.
    /// This is board state, not provider conversation context.
    pub requested_by: Option<String>,
    /// Optional worker to notify after the card first enters a terminal state.
    /// For peer requests the API constrains this to `requested_by`.
    pub callback_session: Option<String>,
    /// Optional instruction appended to the factual terminal notification.
    pub callback_prompt: Option<String>,
    /// `armed` -> `pending` -> `dispatching` -> `queued`, or `refused`.
    pub callback_state: Option<String>,
    /// Stable steering id used to make crash recovery idempotent.
    pub callback_message_id: Option<String>,
    /// Unix seconds when callback delivery was durably queued.
    pub callback_fired_at: Option<i64>,
    /// Visible refusal/recovery detail; never hidden in logs alone.
    pub callback_error: Option<String>,
    /// Set ONLY when `desc` holds a bounded PREFIX rather than the whole
    /// string, which the slim list does to stop hydrating ~30 MB of prose per
    /// call (AF-346). `None` means `desc` is complete and every consumer
    /// behaves exactly as it did before this field existed.
    ///
    /// It carries the two derivations that cannot be recomputed from a prefix.
    /// The other three can: `desc_head` is the first non-empty line (verified
    /// identical from a 512-char prefix on 8,260 live cards), `log_n` reads
    /// `log`, which is still whole, and `needsyou_note` needs prose only for
    /// rows that carry a marker, which are hydrated in full.
    pub desc_prefixed: Option<DescPrefixed>,
}

/// What a truncated `desc` cannot answer for itself (AF-346).
///
/// Both come from SQL beside the prefix. They are NOT recomputable in Rust from
/// what was hydrated, which is exactly why they are carried rather than derived
/// a second time: a fallback that silently computed them from the prefix would
/// return a smaller number that looks like a real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescPrefixed {
    /// `desc.chars().count()` over the WHOLE string.
    pub desc_len: usize,
    /// `"New task:"` occurrences across the whole `desc` AND `log`.
    pub folded_n: usize,
}

impl IssueRow {
    /// The card-level gate override as a criterion list ([] when unset).
    pub fn gate_criteria(&self) -> Vec<String> {
        match &self.gate {
            None => Vec::new(),
            Some(s) => serde_json::from_str::<Vec<serde_json::Value>>(s)
                .map(|v| {
                    v.into_iter()
                        .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// RR-0111a: the canonical replay snapshot of this row — also the API's
    /// detail body (`api::board::detail_body` delegates here). ONE function
    /// serializes the row at event-write time and at verify time, so
    /// `db::replay::verify_replay`'s comparison cannot drift from what the
    /// journal recorded.
    ///
    /// `tags` are SORTED: the live read assembles them via `GROUP_CONCAT`,
    /// whose order SQLite does not define, while an event snapshot carries
    /// the caller's staging order — without one canonical order, replay
    /// verification would report phantom tag divergences on identical sets.
    pub fn snapshot(&self) -> serde_json::Value {
        self.snapshot_fields(true)
    }

    /// The one serializer behind [`snapshot`](Self::snapshot) (prose included:
    /// replay/journal/detail contract, unchanged) and
    /// [`snapshot_slim`](Self::snapshot_slim) (prose never allocated). Map
    /// equality in serde_json is key-set equality, so the split cannot change
    /// what replay verification compares.
    fn snapshot_fields(&self, with_prose: bool) -> serde_json::Value {
        let mut tags = self.tags.clone();
        tags.sort();
        let mut v = serde_json::json!({
            "id": self.id,
            "title": self.title,
            "status": self.status,
            "session": self.session,
            "shepherd": self.shepherd,
            "type": self.item_type,
            "creator": self.creator,
            // AF-367: WHERE the card came from, as a kind. `creator` cannot
            // answer it — the capture daemon and the amux LANE both stamp
            // "amux" there, 49 of 90 in one 24h window belonging to other
            // lanes. NULL on rows that predate the field, which means "unknown"
            // and not a claim about which population it was.
            "source": self.source,
            "due": self.due,
            "due_time": self.due_time,
            "created": self.created,
            "updated": self.updated,
            "owner_type": self.owner_type,
            "pinned": self.pinned,
            "pos": self.pos,
            "archived": self.archived,
            "depends_on": self.depends_on,
            "reviewer": self.reviewer,
            "epic": self.epic,
            "source_ref": self.source_ref,
            "last_verified_at": self.last_verified_at,
            // In BOTH snapshots, for the reason `closed_at` gives below: the
            // question this column exists to answer ("which done cards closed
            // without real evidence") is a LIST query, so withholding it from
            // the list body would ship the column and hide it from its only
            // caller.
            "evidence": self.evidence,
            // In BOTH snapshots for the same reason: "which needsyou cards
            // carry a real ask" is a LIST query, and the owner view ranks on it.
            "ask_type": self.ask_type,
            "ask_question": self.ask_question,
            "ask_unblocks": self.ask_unblocks,
            "ask_actor": self.ask_actor,
            "entered_state_at": self.entered_state_at,
            "blocked_on": self.blocked_on,
            "next_action": self.next_action,
            "last_result": self.last_result,
            "unresolved": self.unresolved,
            "acceptance_criteria": self.acceptance_criteria.as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
            "decision_question": self.decision_question,
            "decision_rationale": self.decision_rationale,
            "decision_supersedes": self.decision_supersedes,
            "waiting_on": self.waiting_on.as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()),
            "requested_by": self.requested_by,
            "callback": self.callback_session.as_ref().map(|session| serde_json::json!({
                "session": session,
                "prompt": self.callback_prompt,
                "trigger": "terminal",
                "state": self.callback_state,
                "message_id": self.callback_message_id,
                "fired_at": self.callback_fired_at,
                "error": self.callback_error,
            })),
            // In BOTH snapshots deliberately, i.e. NOT in `slim_omits`. The
            // motivating question ("which cards closed in this window") is a
            // LIST query, so omitting it from the list body would ship the
            // column and withhold it from its only caller — the AF-161 shape,
            // twice already (`desc` at c207339, `reviewer` at AF-161).
            "closed_at": self.closed_at,
            "rev": self.rev,
            "gate": self.gate_criteria(),
            "tags": tags,
            "version": self.version,
        });
        if with_prose {
            let obj = v.as_object_mut().expect("snapshot_fields is an object");
            obj.insert("desc".into(), serde_json::json!(self.desc));
            obj.insert("log".into(), serde_json::json!(self.log));
        }
        v
    }

    /// [`snapshot`](Self::snapshot) without the prose columns (AMUX-3496).
    /// The slim list used to build the FULL snapshot per row — cloning desc
    /// and log, 6MB+ across a live list — and then delete both keys. This
    /// never allocates the prose at all. Both snapshots are the same
    /// [`snapshot_fields`](Self::snapshot_fields) call, so they cannot drift;
    /// `snapshot_slim_is_snapshot_minus_prose` pins it anyway.
    pub fn snapshot_slim(&self) -> serde_json::Value {
        self.snapshot_fields(false)
    }

    /// Bridge into the core [`Task`] so every status change runs through
    /// [`board::apply_transition`]. `None` when the stored status string is
    /// not in the shared vocabulary (a custom Python lane) — callers must
    /// refuse the transition honestly rather than guess.
    ///
    /// `worker` is always `None`: `issues.session` is an owner NAME, not a
    /// claim by `WorkerId` — atomic claims/leases land with RR-0052.
    /// NO CARD MAY VANISH (AMUX-2632).
    ///
    /// This opened `parse_status(&self.status)?`, so a status outside the
    /// closed vocabulary returned None — and the orchestrator's one caller did
    /// `else { continue }`. A card in an operator-created column was therefore
    /// INVISIBLE to the orchestrator: not blocked, not waiting, not reported,
    /// simply absent, with no log line anywhere saying so.
    ///
    /// That was theoretical until `board.rs` gained its `unmodelled_status`
    /// branch, which lets a card MOVE INTO a custom column. It is now reachable
    /// by the documented path — `POST /api/board/statuses` then a move — and
    /// latent only because all eleven live statuses happen to parse.
    ///
    /// An unmodelled column maps to [`TaskStatus::Blocked`], which is exactly
    /// what it is: blocked on configuration the orchestrator cannot model. The
    /// ENUM STAYS CLOSED — `parse_status` still returns None, because "security
    /// review" is genuinely not a member of the shared vocabulary and teaching
    /// the parser to guess would make every consumer's match arm a lie. The
    /// mapping belongs here, at the boundary, where the raw string is still
    /// available to whoever needs to name the column.
    pub fn to_task(&self) -> Option<Task> {
        let status = parse_status(&self.status).unwrap_or(TaskStatus::Blocked);
        let creator = if self.creator.trim().is_empty() {
            Actor::System {
                component: "python-board".into(),
            }
        } else {
            Actor::Human {
                name: self.creator.clone(),
            }
        };
        Some(Task {
            id: internal_id(&self.id),
            title: self.title.clone(),
            desc: self.desc.clone(),
            status,
            worker: None,
            item_type: core_item_type(&self.item_type),
            creator,
            created_at: ts(self.created),
            updated_at: ts(self.updated),
            archived: self.archived != 0,
            pinned: self.pinned != 0,
            depends_on: self.depends_on.iter().map(|d| internal_id(d)).collect(),
            reviewer: self.reviewer.as_ref().map(|n| Actor::Human { name: n.clone() }),
            gate_override: None,
            tags: self.tags.clone(),
            version: u64::try_from(self.version).unwrap_or(0),
        })
    }
}

fn ts(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
}

/// Shared column list so row indices cannot drift from the query text.
/// `desc` is quoted — it is an SQL keyword. `deleted` is never selected;
/// it is filtered in every WHERE instead (soft delete, Python semantics).
const COLS: &str = "i.id, i.title, i.\"desc\", i.status, i.session, i.creator, i.due, \
     i.created, i.updated, i.owner_type, i.due_time, COALESCE(i.pinned,0), \
     i.gcal_event_id, COALESCE(i.pos,0), COALESCE(i.notified,0), i.gate, i.shepherd, \
     i.type, COALESCE(i.archived,0), i.depends_on, i.reviewer, i.log, \
     COALESCE(i.rev,0), i.source_ref, i.last_verified_at, COALESCE(i.version,0), \
     i.epic, i.closed_at, GROUP_CONCAT(t.tag), i.evidence, \
     i.ask_type, i.ask_question, i.ask_unblocks, \
     i.next_action, i.last_result, i.unresolved, i.entered_state_at, i.blocked_on, \
     i.source, i.acceptance_criteria, i.decision_question, i.decision_rationale, \
     i.decision_supersedes, i.waiting_on, i.requested_by, i.callback_session, \
     i.callback_prompt, i.callback_state, i.callback_message_id, \
     i.callback_fired_at, i.callback_error, i.ask_actor";

/// Read an INTEGER-typed timestamp column that some row may hold as REAL or TEXT.
///
/// `updated` is declared INTEGER and every writer is supposed to store one, so
/// `r.get::<_, i64>()` reads it — and rusqlite's `FromSql` is strict per storage
/// type, so ONE row holding a REAL fails the whole query. On 2026-08-30 three
/// rows written by the queue-disposition job with an f64 timestamp took
/// `GET /api/board` down fleet-wide with `Invalid column type Real at index: 6,
/// name: updated`: 12,959 correct rows, 3 wrong ones, and the board served a
/// 500 to every session.
///
/// The writer is fixed. This is the second half, and it is the one that matters:
/// a list read over ~13,000 rows must not be all-or-nothing on the storage type
/// of a single cell. Same shape `last_verified_at` already uses below for the
/// Python-era TEXT case — that precedent existed and this column did not have it.
fn ts_i64(r: &Row<'_>, idx: usize) -> rusqlite::Result<i64> {
    Ok(match r.get::<_, rusqlite::types::Value>(idx)? {
        rusqlite::types::Value::Integer(n) => n,
        rusqlite::types::Value::Real(f) => f as i64,
        // A NUMBER IN A DIFFERENT STORAGE CLASS IS THE SAME TIMESTAMP.
        // TEXT THAT IS NOT A NUMBER IS NOT A TIMESTAMP AT ALL (AMUX-3906).
        //
        // The point of this helper is that one row written as f64 must not fail
        // a 13,000-row list read, because the VALUE is right and only its
        // storage class is wrong. `'not-an-integer'` is a different situation:
        // there is no timestamp in the cell, and substituting 0 invents one —
        // the card then renders as 1970 and the corruption is never seen.
        //
        // It also silently disarmed a probe. `board_probe_fails_on_a_row_the_
        // mapper_cannot_decode` (AF-332) corrupts `created` to exactly that
        // string to prove /health's board read is not a liveness ping; with
        // `unwrap_or(0.0)` the row decoded fine and the probe went green on a
        // database it was meant to call broken.
        rusqlite::types::Value::Text(t) => {
            let t = t.trim();
            t.parse::<f64>().map(|f| f as i64).map_err(|_| {
                rusqlite::Error::FromSqlConversionFailure(
                    idx,
                    rusqlite::types::Type::Text,
                    format!("timestamp column holds non-numeric text {t:?}").into(),
                )
            })?
        }
        rusqlite::types::Value::Null => 0,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                idx,
                other.data_type(),
                "timestamp column holds a value that is not a number".into(),
            ))
        }
    })
}

fn issue_from_row(r: &Row<'_>) -> rusqlite::Result<IssueRow> {
    let depends_raw: Option<String> = r.get(19)?;
    let depends_on = depends_raw
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(s).ok())
        .map(|v| {
            v.into_iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let tags_csv: Option<String> = r.get(28)?;
    let tags = tags_csv
        .unwrap_or_default()
        .split(',')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    Ok(IssueRow {
        id: r.get(0)?,
        title: r.get(1)?,
        desc: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
        status: r.get(3)?,
        session: r.get(4)?,
        creator: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        due: r.get(6)?,
        // `created` IS THE SAME SHAPE AS `updated` AND WAS LEFT STRICT
        // (AMUX-3906). AF-317's fix made `updated` tolerant because that is the
        // column three queue-disposition rows had written as f64, taking
        // GET /api/board down fleet-wide over 3 bad cells in ~13,000 rows. The
        // column NEXT TO IT is declared INTEGER, read strictly, and carries the
        // identical all-or-nothing failure — a fix to the instance rather than
        // to the class. Nothing had written a REAL there yet, which is the only
        // reason it had not fired.
        created: ts_i64(r, 7)?,
        updated: ts_i64(r, 8)?,
        owner_type: r.get::<_, Option<String>>(9)?.unwrap_or_else(|| "human".into()),
        due_time: r.get(10)?,
        pinned: r.get(11)?,
        gcal_event_id: r.get(12)?,
        pos: r.get(13)?,
        notified: r.get(14)?,
        gate: r.get(15)?,
        shepherd: r.get(16)?,
        item_type: r.get::<_, Option<String>>(17)?.unwrap_or_else(|| "code".into()),
        archived: r.get(18)?,
        depends_on,
        reviewer: r.get(20)?,
        log: r.get(21)?,
        rev: r.get(22)?,
        source_ref: r.get(23)?,
        evidence: r.get(29)?,
        ask_type: r.get(30)?,
        ask_question: r.get(31)?,
        ask_unblocks: r.get(32)?,
        entered_state_at: r.get(36)?,
        blocked_on: r.get(37)?,
        source: r.get(38)?,
        acceptance_criteria: r.get(39)?,
        decision_question: r.get(40)?,
        decision_rationale: r.get(41)?,
        decision_supersedes: r.get(42)?,
        waiting_on: r.get(43)?,
        requested_by: r.get(44)?,
        callback_session: r.get(45)?,
        callback_prompt: r.get(46)?,
        callback_state: r.get(47)?,
        callback_message_id: r.get(48)?,
        callback_fired_at: r.get(49)?,
        callback_error: r.get(50)?,
        ask_actor: r.get(51)?,
        next_action: r.get(33)?,
        last_result: r.get(34)?,
        unresolved: r.get(35)?,
        // Some Python-era databases stored this column as TEXT despite the
        // schema saying INTEGER (legacy-data mismatch, not a live schema
        // bug) — rusqlite's FromSql is strict per storage type, so EITHER
        // `Option<i64>` alone (fails on legacy TEXT) or `Option<String>`
        // alone (fails on the normal, correct INTEGER case — the majority)
        // errors on one side or the other. Reading as the type-erased
        // `Value` and matching both storage shapes is the only form that
        // works for both; unparseable/other becomes None rather than
        // crashing on startup.
        last_verified_at: match r.get::<_, rusqlite::types::Value>(24)? {
            rusqlite::types::Value::Integer(n) => Some(n),
            rusqlite::types::Value::Text(s) => match s.trim().parse::<i64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    // SAY THAT A VALUE WAS THERE AND COULD NOT BE READ. None
                    // here renders as "never verified", which is a claim about
                    // the card rather than about the read, and it is
                    // indistinguishable from the honest NULL below. Any output
                    // that can read empty has to publish whether the
                    // measurement ran (.claude/rules/ethos.md rule 4).
                    tracing::warn!(
                        raw = %s,
                        "last_verified_at holds text that is not an integer — reporting the \
                         card as never verified, which may be wrong; this is legacy data, \
                         not a live schema bug"
                    );
                    None
                }
            },
            // Null is genuine absence: the card really has never been verified,
            // and warning on it would fire for most of the board.
            rusqlite::types::Value::Null => None,
            other => {
                tracing::warn!(
                    kind = ?std::mem::discriminant(&other),
                    "last_verified_at holds neither an integer, text nor null"
                );
                None
            }
        },
        version: r.get(25)?,
        epic: r.get(26)?,
        closed_at: r.get(27)?,
        tags,
        // Positional mapping only; the AF-346 prefix flag is read BY NAME in
        // `hydrate_light`, which is the only place it can be true.
        desc_prefixed: None,
    })
}

/// One card by semantic id, tags joined, soft-delete filtered.
pub fn get_issue(conn: &Connection, id: &str) -> rusqlite::Result<Option<IssueRow>> {
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM issues i LEFT JOIN issue_tags t ON t.issue_id = i.id \
             WHERE i.id = ?1 AND i.deleted IS NULL GROUP BY i.id"
        ),
        params![id],
        issue_from_row,
    )
    .optional()
}

/// Archived filter for the list (`archived` query param), Python's grammar
/// (amux-server.py:14025): absent/"" = no filter, truthy = archived-only,
/// any other value = non-archived only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchivedFilter {
    /// Only archived=0 rows (`archived=0` — and any other non-truthy value).
    ActiveOnly,
    /// Only archived=1 rows (`archived=1`/`true`/`yes`).
    ArchivedOnly,
    /// No filter (the `archived` param absent or empty).
    All,
}

/// Filtered, Python-sorted board list. Filters run BEFORE any terminal cap
/// (the AC-291/AC-301 lesson: cap the filtered set, not the population it is
/// drawn from) — [`cap_terminal`] is a separate step the API applies after.
/// Status filter values are canonicalized on both sides so `needs_you`
/// matches a `needsyou` row.
/// AF-332: a BOUNDED board read through the REAL row mapper, for /health.
///
/// WHY NOT `SELECT 1`. On 2026-08-30 `GET /api/board` returned 500 to every
/// session in the fleet for ~20 minutes and nothing alarmed. The failure was in
/// ROW MAPPING, not in the query or the connection, so `current_rev()` answered
/// fine and `/health` stayed green throughout. A liveness probe that does not
/// deserialize a row cannot see that class at all, and this one exists
/// specifically to see it: same `COLS`, same `issue_from_row`, so a schema
/// drift or a serializer panic fails HERE the way it fails in `list_issues`.
///
/// `LIMIT 1` because the point is to exercise the path, not to measure the
/// board. /health is polled often and `list_issues` is unbounded.
///
/// Returns the number of rows actually mapped, so the caller can distinguish
/// "mapped a row" from "the table is empty" - which are the same `ok` to a
/// probe that only reports success, and different facts (AF-320).
pub fn probe_board_read(conn: &Connection) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM issues i LEFT JOIN issue_tags t ON t.issue_id = i.id \
         WHERE i.deleted IS NULL GROUP BY i.id LIMIT 1"
    ))?;
    let mut mapped = 0usize;
    for row in stmt.query_map([], issue_from_row)? {
        row?;
        mapped += 1;
    }
    Ok(mapped)
}

pub fn list_issues(
    conn: &Connection,
    status_filter: &[String],
    session_filter: &[String],
    archived: ArchivedFilter,
) -> rusqlite::Result<Vec<IssueRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM issues i LEFT JOIN issue_tags t ON t.issue_id = i.id \
         WHERE i.deleted IS NULL GROUP BY i.id"
    ))?;
    let canon = |s: &str| -> String {
        parse_status(s)
            .map(|st| db_status_spelling(st).to_string())
            .unwrap_or_else(|| s.trim().to_lowercase())
    };
    let want_status: Vec<String> = status_filter.iter().map(|s| canon(s)).collect();
    let mut rows = Vec::new();
    for row in stmt.query_map([], issue_from_row)? {
        let row = row?;
        if !want_status.is_empty() && !want_status.contains(&canon(&row.status)) {
            continue;
        }
        if !session_filter.is_empty()
            && !session_filter.contains(&row.session.clone().unwrap_or_default())
        {
            continue;
        }
        match archived {
            ArchivedFilter::ActiveOnly if row.archived != 0 => continue,
            ArchivedFilter::ArchivedOnly if row.archived == 0 => continue,
            _ => {}
        }
        rows.push(row);
    }
    // Python sort: pinned first, then explicitly-positioned (pos != 0) by pos
    // ascending, then the rest by updated descending.
    rows.sort_by(|a, b| board_order(a.pinned, a.pos, a.updated, b.pinned, b.pos, b.updated));
    Ok(rows)
}

/// The one Python board ordering, shared by [`list_issues`] and the light pass
/// in [`list_issues_capped`] so the two can never sort differently: pinned
/// first, then explicitly-positioned (pos != 0) by pos ascending, then updated
/// descending.
fn board_order(
    a_pinned: i64,
    a_pos: f64,
    a_updated: i64,
    b_pinned: i64,
    b_pos: f64,
    b_updated: i64,
) -> std::cmp::Ordering {
    b_pinned
        .cmp(&a_pinned)
        .then_with(|| i32::from(a_pos == 0.0).cmp(&i32::from(b_pos == 0.0)))
        .then_with(|| a_pos.partial_cmp(&b_pos).unwrap_or(std::cmp::Ordering::Equal))
        .then_with(|| b_updated.cmp(&a_updated))
}

/// Filter/sort/cap fields only — what [`list_issues_capped`]'s first pass
/// reads for every row, so the prose columns (desc 23MB+, log 3.5MB on the
/// live table) are decoded only for rows that actually ship.
struct LightRow {
    id: String,
    status: String,
    session: Option<String>,
    archived: i64,
    pinned: i64,
    pos: f64,
    updated: i64,
}

/// [`list_issues`] + [`cap_terminal`] fused, decoding heavy columns only for
/// survivors (AMUX-3491). The single-pass shape decoded EVERY undeleted row's
/// desc+log — 8,335 rows / ~27MB of prose on the live DB — to ship the 1,657
/// that survive the default filter+cap, and it did so on every list request:
/// 215ms avg where 2026-08-09 measured 28ms, tracking table growth rather
/// than payload size (30MB and 0.7MB responses cost the same ~250ms).
///
/// Pass 1 reads only [`LightRow`] columns, applies the same canon filters,
/// the same [`board_order`], and the same cap; pass 2 hydrates the kept ids
/// chunk-wise through the identical COLS+tags query [`get_issue`] uses.
/// Returns `(kept, terminal_total, terminal_kept)` — [`cap_terminal`]'s exact
/// contract. A row deleted between the passes is dropped, which is the same
/// answer a request a moment later would give.
pub fn list_issues_capped(
    conn: &Connection,
    status_filter: &[String],
    session_filter: &[String],
    archived: ArchivedFilter,
    done_limit: i64,
    prose: Prose,
) -> rusqlite::Result<(Vec<IssueRow>, usize, usize)> {
    let light = light_rows(conn, status_filter, session_filter, archived)?;
    let (kept_light, term_total, term_kept) =
        cap_terminal_by(light, done_limit, |r| &r.status, |r| r.updated);
    Ok((hydrate_light(conn, &kept_light, prose)?, term_total, term_kept))
}

/// [`list_issues_capped`]'s sibling with [`sse_terminal_quota`] semantics
/// instead of the lumped cap (AMUX-3503): verified keeps its own 300-floor
/// quota so a bulk-verify stays visible, done/discarded share `done_limit`.
/// The dashboard poll uses this (`?quota=1`) now that it renders from the
/// fetch path rather than the retired SSE full-push.
pub fn list_issues_quota(
    conn: &Connection,
    status_filter: &[String],
    session_filter: &[String],
    archived: ArchivedFilter,
    done_limit: usize,
    prose: Prose,
) -> rusqlite::Result<Vec<IssueRow>> {
    let light = light_rows(conn, status_filter, session_filter, archived)?;
    let kept_light = terminal_quota_by(light, done_limit, |r| &r.status, |r| r.updated);
    hydrate_light(conn, &kept_light, prose)
}

/// Pass 1 shared by the capped and quota lists: filter + sort over the
/// no-prose columns. ONE loader on purpose — two spellings of the filter
/// canon or the sort would drift exactly the way the predicate rule warns.
fn light_rows(
    conn: &Connection,
    status_filter: &[String],
    session_filter: &[String],
    archived: ArchivedFilter,
) -> rusqlite::Result<Vec<LightRow>> {
    let canon = |s: &str| -> String {
        parse_status(s)
            .map(|st| db_status_spelling(st).to_string())
            .unwrap_or_else(|| s.trim().to_lowercase())
    };
    let want_status: Vec<String> = status_filter.iter().map(|s| canon(s)).collect();
    // ORDER BY i.id matches the practical row order of the joined GROUP BY
    // query in list_issues, so ties in board_order break identically on both
    // paths (sort_by is stable; the input order is the tiebreaker).
    let mut stmt = conn.prepare(
        "SELECT i.id, i.status, i.session, COALESCE(i.archived,0), COALESCE(i.pinned,0), \
                COALESCE(i.pos,0), i.updated \
         FROM issues i WHERE i.deleted IS NULL ORDER BY i.id",
    )?;
    let mut light: Vec<LightRow> = Vec::new();
    for row in stmt.query_map([], |r| {
        Ok(LightRow {
            id: r.get(0)?,
            status: r.get(1)?,
            session: r.get(2)?,
            archived: r.get(3)?,
            pinned: r.get(4)?,
            pos: r.get(5)?,
            updated: ts_i64(r, 6)?,
        })
    })? {
        let row = row?;
        if !want_status.is_empty() && !want_status.contains(&canon(&row.status)) {
            continue;
        }
        if !session_filter.is_empty()
            && !session_filter.contains(&row.session.clone().unwrap_or_default())
        {
            continue;
        }
        match archived {
            ArchivedFilter::ActiveOnly if row.archived != 0 => continue,
            ArchivedFilter::ArchivedOnly if row.archived == 0 => continue,
            _ => {}
        }
        light.push(row);
    }
    light.sort_by(|a, b| board_order(a.pinned, a.pos, a.updated, b.pinned, b.pos, b.updated));
    Ok(light)
}

/// How much of `desc` a list hydration needs (AF-346).
///
/// `/api/board` hydrated 37.6 MB of prose per call, measured 2026-09-04 over
/// the 8,260 rows a default call keeps, for a response that ships none of it.
/// The slim body does not ship `desc`, but it ships FIVE derivations of it, so
/// the obvious fix (stop selecting the column) blanked every card preview on
/// the fleet dashboard when it was tried: a99955f7, reverted by b1227af0.
///
/// This is the version that keeps all five exact. Four of them need at most a
/// bounded prefix or the (much smaller) `log`; the fifth, `needsyou_note`,
/// needs whole prose only for rows that actually carry a marker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prose {
    /// Every row's `desc` in full. What every caller got before AF-346.
    Full,
    /// A bounded prefix, plus whole `desc` for the rows that need it. Only
    /// correct for a caller that ships `list_body(.., slim = true)`.
    SlimDerivations,
}

/// Characters of `desc` hydrated for a row the slim list will only derive from.
///
/// 512 rather than 120 (the `desc_head` cap) because the head is the first
/// NON-EMPTY line, so leading blank lines have to fit too. Verified against
/// every live card: `desc_head` computed from a 512-char prefix is identical to
/// the same computation over the whole string on 8,260 of 8,260 rows. The guard
/// that would catch a regression here is
/// `a_prefixed_desc_produces_the_same_slim_derivations_as_a_whole_one`, which
/// fails on `desc_head` if this number shrinks.
const DESC_PREFIX_CHARS: usize = 512;

/// The nine marker spellings `list_body`'s `needsyou_note` accepts, as a SQL
/// predicate over both prose columns.
///
/// It must stay a SUPERSET of what the extractor matches, which it is: the
/// extractor requires the same nine literals, so a row this misses cannot
/// contain one. Over-hydrating is free; under-hydrating silently drops a card's
/// question from the owner view.
///
/// THE COLON IS LOAD-BEARING. A `%needs%` predicate was measured and rejected
/// on 2026-08-30 for matching 4,725 of 8,260 rows and saving 17%. With the
/// colon the same nine spellings match 287, holding 6.9% of the prose, and 367
/// of the 371 matches across the whole table really do yield a note.
fn needsyou_marker_sql() -> String {
    const MARKERS: [&str; 9] = [
        "needs-you:", "needs you:", "needsyou:",
        "needs-ethan:", "needs ethan:", "needsethan:",
        "needs-human:", "needs human:", "needshuman:",
    ];
    let mut parts: Vec<String> = Vec::new();
    for col in ["i.\"desc\"", "i.log"] {
        for m in MARKERS {
            parts.push(format!("COALESCE({col},'') LIKE '%{m}%'"));
        }
    }
    parts.join(" OR ")
}

/// `COLS` with the `desc` column swapped for `expr`, aliased back to `desc`.
///
/// Runtime substitution rather than a second hand-maintained column list: two
/// spellings of fifty columns drift, and the drift would be a wrong VALUE in
/// some field nobody is looking at. `cols_names_desc_exactly_once` fails if a
/// future edit renames or duplicates the needle, so a silent no-match is not
/// available.
fn cols_with_desc(expr: &str) -> String {
    let out = COLS.replacen(DESC_COL, expr, 1);
    debug_assert_ne!(out, COLS, "COLS no longer contains {DESC_COL}");
    out
}

/// The exact text `COLS` uses for the desc column. Named so the test and the
/// substitution cannot disagree about it.
const DESC_COL: &str = "i.\"desc\"";

/// Pass 2: hydrate survivors only, preserving pass-1 order. Chunked well
/// under SQLITE_MAX_VARIABLE_NUMBER's historical floor of 999.
fn hydrate_light(
    conn: &Connection,
    kept_light: &[LightRow],
    prose: Prose,
) -> rusqlite::Result<Vec<IssueRow>> {
    // The projection and the two extra numbers, or neither. Building both here
    // keeps the "when is desc a prefix" decision in ONE place: a row is
    // prefixed exactly when `desc_prefixed` comes back 1, and that flag is
    // computed by the same CASE that chose the projection, so the value and the
    // claim about it cannot disagree.
    let (cols, prefixed) = match prose {
        Prose::Full => (COLS.to_string(), false),
        Prose::SlimDerivations => {
            let marker = needsyou_marker_sql();
            // WHY A NUL ESCAPES TO THE FULL COLUMN. SQLite's LENGTH() on TEXT
            // stops at the first NUL byte, so `desc_len` would be short for any
            // card carrying one. Two live cards do today (MF-563: a NUL at
            // offset 3,561 of 10,063 chars, so LENGTH reports 3,561;
            // AMUX-2925: ten of them, first at 410 of 2,413). NULs arrive from
            // pasted terminal output and will recur, and `instr(desc, char(0))`
            // isolates exactly those two rows out of 8,260. Hydrating them
            // whole is cheaper than shipping a quietly wrong length.
            let full_desc_when =
                format!("instr(COALESCE(i.\"desc\",''), char(0)) > 0 OR {marker}");
            let desc_expr = format!(
                "CASE WHEN {full_desc_when} THEN i.\"desc\" \
                 ELSE substr(COALESCE(i.\"desc\",''), 1, {DESC_PREFIX_CHARS}) END"
            );
            // `desc_len` is only READ when the row was prefixed, and the CASE
            // above guarantees a prefixed row has no NUL, so plain LENGTH() is
            // exact on every row that uses it.
            let extra = format!(
                ", LENGTH(COALESCE(i.\"desc\",'')) AS d_len, \
                 (LENGTH(COALESCE(i.\"desc\",'')||COALESCE(i.log,'')) \
                  - LENGTH(REPLACE(COALESCE(i.\"desc\",'')||COALESCE(i.log,''),'New task:',''))) / 9 \
                 AS d_folded, \
                 CASE WHEN {full_desc_when} THEN 0 ELSE 1 END AS d_prefixed"
            );
            (format!("{}{}", cols_with_desc(&desc_expr), extra), true)
        }
    };
    let mut by_id: std::collections::HashMap<String, IssueRow> = std::collections::HashMap::new();
    for chunk in kept_light.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT {cols} FROM issues i LEFT JOIN issue_tags t ON t.issue_id = i.id \
             WHERE i.deleted IS NULL AND i.id IN ({marks}) GROUP BY i.id"
        ))?;
        let params: Vec<&dyn rusqlite::types::ToSql> =
            chunk.iter().map(|r| &r.id as &dyn rusqlite::types::ToSql).collect();
        // Read the derived columns BY NAME. `issue_from_row` maps fifty columns
        // positionally, so appending to that list by index is a standing invite
        // to an off-by-one that silently reads the neighbouring field.
        let rows = stmt.query_map(params.as_slice(), |r| {
            let mut row = issue_from_row(r)?;
            if prefixed && r.get::<_, i64>("d_prefixed")? == 1 {
                row.desc_prefixed = Some(DescPrefixed {
                    desc_len: r.get::<_, i64>("d_len")?.max(0) as usize,
                    folded_n: r.get::<_, i64>("d_folded")?.max(0) as usize,
                });
            }
            Ok(row)
        })?;
        for row in rows {
            let row = row?;
            by_id.insert(row.id.clone(), row);
        }
    }
    Ok(kept_light.iter().filter_map(|l| by_id.remove(&l.id)).collect())
}

/// The Python `_BOARD_TERMINAL` set for the done_limit cap. NOTE: this is
/// the PAYLOAD-size cap's notion of terminal (done/verified/discarded, per
/// `_cap_terminal`), which is narrower than core's `is_terminal` — `done` is
/// capped although semantically non-terminal, and `quarantined` is included
/// here as the Rust addition core introduced (Python has no such status, so
/// no Python row can ever carry it).
fn cap_terminal_status(raw: &str) -> bool {
    matches!(
        raw.trim().to_lowercase().as_str(),
        "done" | "verified" | "discarded" | "quarantined"
    )
}

/// Cap terminal-status items to the `limit` most recently updated, AFTER
/// filtering — Python `_cap_terminal`, ported with its exact return contract:
/// `(kept, terminal_total, terminal_kept)`, `limit <= 0` -> uncapped with
/// `(_, 0, 0)`. Active items are never capped; order is preserved.
pub fn cap_terminal(items: Vec<IssueRow>, limit: i64) -> (Vec<IssueRow>, usize, usize) {
    cap_terminal_by(items, limit, |r| &r.status, |r| r.updated)
}

/// The cap algorithm itself, generic over the two fields it reads so
/// [`list_issues_capped`]'s light pass runs the IDENTICAL logic (not a
/// re-derivation that can drift — the predicate-sharing rule).
fn cap_terminal_by<T>(
    items: Vec<T>,
    limit: i64,
    status_of: impl Fn(&T) -> &str,
    updated_of: impl Fn(&T) -> i64,
) -> (Vec<T>, usize, usize) {
    if limit <= 0 {
        return (items, 0, 0);
    }
    let term_idx: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, r)| cap_terminal_status(status_of(r)))
        .map(|(i, _)| i)
        .collect();
    let total = term_idx.len();
    if total as i64 <= limit {
        return (items, total, total);
    }
    let mut by_updated = term_idx.clone();
    by_updated.sort_by(|a, b| updated_of(&items[*b]).cmp(&updated_of(&items[*a])));
    let keep: std::collections::HashSet<usize> =
        by_updated.into_iter().take(limit as usize).collect();
    let kept = items
        .into_iter()
        .enumerate()
        .filter(|(i, r)| !cap_terminal_status(status_of(r)) || keep.contains(i))
        .map(|(_, r)| r)
        .collect();
    (kept, total, limit as usize)
}

/// Python `_load_board(done_limit=100)`'s terminal quotas — the SSE board
/// channel's shape (amux-server.py:15825-15860): active items unlimited,
/// `verified` gets its OWN quota of max(done_limit, 300) so the flood of
/// `done` cannot crowd prod-confirmed work out of the UI, and done/discarded
/// share done_limit. Both quotas keep the most recently UPDATED. Discovered
/// live 2026-08-09: ~130 cards were verified in bulk and the Rust SSE push
/// (single lumped 100-cap) showed 9 of them while Python showed 141.
pub fn sse_terminal_quota(items: Vec<IssueRow>, done_limit: usize) -> Vec<IssueRow> {
    terminal_quota_by(items, done_limit, |r| &r.status, |r| r.updated)
}

/// The quota algorithm itself, generic over the two fields it reads — the
/// same split as [`cap_terminal_by`], for the same reason: the light pass in
/// [`list_issues_quota`] must run IDENTICAL logic, not a re-derivation.
/// (AMUX-3503 moved the dashboard's board view from the SSE push onto the
/// fetch path, so the quota had to follow the data or the 2026-08-09
/// bulk-verify incident — 141 verified visible on Python, 9 on Rust —
/// comes back through the front door.)
fn terminal_quota_by<T>(
    items: Vec<T>,
    done_limit: usize,
    status_of: impl Fn(&T) -> &str,
    updated_of: impl Fn(&T) -> i64,
) -> Vec<T> {
    let verified_limit = done_limit.max(300);
    let keep_top = |status_match: &dyn Fn(&str) -> bool, limit: usize| -> std::collections::HashSet<usize> {
        let mut idx: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, r)| status_match(&status_of(r).trim().to_lowercase()))
            .map(|(i, _)| i)
            .collect();
        idx.sort_by(|a, b| updated_of(&items[*b]).cmp(&updated_of(&items[*a])));
        idx.into_iter().take(limit).collect()
    };
    let keep_verified = keep_top(&|s: &str| s == "verified", verified_limit);
    let keep_done = keep_top(&|s: &str| matches!(s, "done" | "discarded"), done_limit);
    items
        .into_iter()
        .enumerate()
        .filter(|(i, r)| match status_of(r).trim().to_lowercase().as_str() {
            "verified" => keep_verified.contains(i),
            "done" | "discarded" => keep_done.contains(i),
            _ => true,
        })
        .map(|(_, r)| r)
        .collect()
}

/// Fields for a new card. Everything the Python POST persists (reviewer and
/// depends_on included — accepting a field and dropping it is worse than
/// rejecting it, per the POST handler's own comment).
pub struct NewIssue {
    pub title: String,
    pub desc: String,
    /// RAW status spelling to store (already canonicalized by the API).
    pub status: String,
    pub session: Option<String>,
    pub item_type: String,
    pub creator: String,
    pub owner_type: String,
    pub due: Option<String>,
    pub due_time: Option<String>,
    pub reviewer: Option<String>,
    pub shepherd: Option<String>,
    /// Card-level gate override criteria ([] = none).
    pub gate: Vec<String>,
    pub depends_on: Vec<String>,
    pub tags: Vec<String>,
    /// The typed ask, for a card FILED straight into `needsyou` (AMUX-3929).
    ///
    /// The insert used to omit these three columns entirely, so a create that
    /// supplied a perfectly good ask stored NULL and the card landed in the
    /// untyped population it was trying to stay out of. The create-side gate
    /// that now demands them would otherwise be demanding data it discards,
    /// which is worse than the hole it closes.
    pub ask_type: Option<String>,
    pub ask_question: Option<String>,
    pub ask_unblocks: Option<String>,
    pub ask_actor: Option<String>,
    /// WHO the card came from, as a KIND rather than a name: `agent` for a real
    /// create, `capture` for an auto-captured human prompt (AF-367).
    ///
    /// `creator` cannot answer this. `mint_capture_card` stamps `creator: "amux"`
    /// for every captured prompt and the amux LANE stamps the same string for
    /// cards it authors, so 49 of 90 cards carrying that value in one 24h window
    /// belonged to other lanes entirely. `owner_type` does not split them either;
    /// it reads `agent` for both populations.
    ///
    /// `None` writes NULL, which means "predates the discriminator" and NOT a
    /// claim about which population a row belonged to. Nothing is backfilled:
    /// guessing retroactively would manufacture the confident wrong attribution
    /// this field exists to end.
    pub source: Option<String>,
    /// Verified requester when one worker files work for another.
    pub requested_by: Option<String>,
    /// Optional terminal callback target (normally the requester).
    pub callback_session: Option<String>,
    pub callback_prompt: Option<String>,
}

/// Insert a new card, replicating the Python POST exactly: id minted from
/// the shared counter, `pos` = (min non-zero pos in the column) - 1024 (new
/// card at the top of its lane), int timestamps, `notified` 0. Returns the
/// row as stored.
pub fn create_issue(conn: &Connection, new: &NewIssue, now: i64) -> rusqlite::Result<IssueRow> {
    let prefix = prefix_from_session(new.session.as_deref().unwrap_or(""));
    let id = next_issue_id(conn, &prefix)?;
    let min_pos: f64 = conn.query_row(
        "SELECT COALESCE(MIN(NULLIF(pos, 0)), 0) FROM issues WHERE status = ?1 AND deleted IS NULL",
        params![new.status],
        |r| r.get(0),
    )?;
    let pos = min_pos - 1024.0;
    let gate_json = if new.gate.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&new.gate).unwrap_or_default())
    };
    let dep_json = if new.depends_on.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&new.depends_on).unwrap_or_default())
    };
    conn.execute(
        "INSERT INTO issues (id, title, \"desc\", status, session, shepherd, type, creator, \
             due, due_time, created, updated, owner_type, pos, gate, reviewer, depends_on, \
             ask_type, ask_question, ask_unblocks, entered_state_at, source, \
             requested_by, callback_session, callback_prompt, callback_state, ask_actor, \
             notified, pinned, archived, rev, version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, 0, 0, 0, 0, 0)",
        params![
            id,
            new.title,
            new.desc,
            new.status,
            new.session.as_deref().filter(|s| !s.is_empty()),
            new.shepherd,
            new.item_type,
            new.creator,
            new.due,
            new.due_time,
            now,
            now,
            new.owner_type,
            pos,
            gate_json,
            new.reviewer,
            dep_json,
            new.ask_type.as_deref().filter(|x| !x.trim().is_empty()),
            new.ask_question.as_deref().filter(|x| !x.trim().is_empty()),
            new.ask_unblocks.as_deref().filter(|x| !x.trim().is_empty()),
            // A new card enters its first status NOW, so this is measured from
            // the start and only PRE-0040 rows carry the honest NULL.
            now,
            new.source.as_deref().filter(|x| !x.trim().is_empty()),
            new.requested_by.as_deref().filter(|x| !x.trim().is_empty()),
            new.callback_session.as_deref().filter(|x| !x.trim().is_empty()),
            new.callback_prompt.as_deref().filter(|x| !x.trim().is_empty()),
            new.callback_session.as_ref().map(|_| "armed"),
            new.ask_actor.as_deref().filter(|x| !x.trim().is_empty()),
        ],
    )?;
    for tag in &new.tags {
        conn.execute(
            "INSERT OR IGNORE INTO issue_tags (issue_id, tag, added_at) VALUES (?1, ?2, ?3)",
            params![id, tag, now],
        )?;
    }
    // A card FILED straight into `needsyou` needs the tag too. The PATCH path
    // syncs on the status TRANSITION, which a create never produces — so the
    // sync's own blind spot was the one case that never gets a second chance:
    // a card created blocked-on-a-human and never touched again. Caught by
    // running the shipped path rather than the transition I had in mind; 1 of
    // the 23 cards in the live census got there this way.
    if parse_status(&new.status) == Some(TaskStatus::NeedsYou) {
        add_needs_you_tag(conn, &id, now)?;
    }
    Ok(get_issue(conn, &id)?.expect("row just inserted"))
}

/// SOFT-delete a card: stamp `deleted` so every query in this module (all of
/// which filter `deleted IS NULL`) stops returning it. Python's DELETE
/// /api/board/{id} does exactly this, and the row stays for forensics.
/// Returns false when the id does not resolve to a live row.
///
/// This is the one write that legitimately touches `deleted` — [`save_patched`]
/// deliberately excludes it (see its note), which is why the delete path needs
/// its own statement rather than a patched row.
pub fn soft_delete(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let now = Utc::now().timestamp();
    let n = conn.execute(
        "UPDATE issues SET deleted = ?2, updated = ?2 WHERE id = ?1 AND deleted IS NULL",
        params![id, now],
    )?;
    Ok(n > 0)
}

/// Write back a patched row. Only columns this API models are touched —
/// `creator`, `created`, `notified`, `gcal_event_id` and `deleted` are
/// deliberately NOT in the SET list so a Rust write can never corrupt a
/// Python-owned column it does not understand (Phase 11 rollback safety).
/// The caller is responsible for having bumped `rev`, `version` and
/// `updated` on the struct (writes bump rev AND version).
/// The statuses that mean a card is closed. One list, used by the write rule
/// and by migration 0031's backfill, so the two cannot disagree about what
/// "closed" means.
pub const TERMINAL_STATUSES: [&str; 3] = ["done", "verified", "discarded"];

pub fn is_terminal_status(s: &str) -> bool {
    TERMINAL_STATUSES.contains(&s)
}

/// `closed_at` for the row about to be written (AMUX-3609).
///
/// Lives INSIDE `save_patched` rather than at the nine call sites that change a
/// status, deliberately. A rule spread across nine callers is a rule seven of
/// them will eventually be written without — and ethos rule 6 is explicit that
/// the fix for that shape is to make the honest path the only path, not to
/// write a note asking people to remember.
///
/// It reads the PREVIOUS status from the database, because the transition is
/// what the timestamp is about. The tempting stateless version — "if the status
/// is terminal and `closed_at` is NULL, stamp now" — is wrong in the one case
/// that matters most: almost every closed card on this board predates the
/// column and backfilled to NULL, so the next unrelated `desc` append to a card
/// closed in June would have stamped it closed today. That is a fabricated date
/// wearing the authority of a real one, which is worse than the NULL it
/// replaces.
fn closed_at_for_write(conn: &Connection, row: &IssueRow) -> Option<i64> {
    let prev: Option<String> = conn
        .query_row("SELECT status FROM issues WHERE id = ?1", params![row.id], |r| r.get(0))
        .ok();
    let was = prev.as_deref().map(is_terminal_status);
    let now_terminal = is_terminal_status(&row.status);
    match (was, now_terminal) {
        // Closing. Stamp the write time the caller already put on `updated`,
        // so a card's close time and its last-touch agree at the moment of
        // closing and diverge only afterwards, which is the whole point.
        (Some(false), true) => Some(row.updated),
        // Reopening. A card that leaves a terminal status is not closed, and
        // leaving a stale timestamp behind would make `closed_at IS NOT NULL`
        // mean "was closed once" while reading like "is closed".
        (Some(true), false) => None,
        // Not a transition across the boundary (including the row not existing
        // yet, where `prev` is None): carry whatever the row holds. This is the
        // arm that protects an old card's NULL from being overwritten by an
        // unrelated edit.
        _ => row.closed_at,
    }
}

/// When this write should say the card entered its status (AMUX-3947).
///
/// Mirrors [`closed_at_for_write`] exactly, including its third arm, because
/// the two answer the same shape of question: read the PREVIOUS status from the
/// row on disk, and stamp only on an actual transition.
///
/// The arm that matters is the last one. An ordinary edit -- a progress note, a
/// desc rewrite, an evidence append -- must NOT move this timestamp, or
/// "in review for 9 days" silently becomes "in review for 0 days" every time
/// somebody touches the card, and the field reports the opposite of the
/// bottleneck it exists to surface.
fn entered_state_at_for_write(conn: &Connection, row: &IssueRow) -> Option<i64> {
    let prev: Option<String> = conn
        .query_row("SELECT status FROM issues WHERE id = ?1", params![row.id], |r| r.get(0))
        .ok();
    match prev {
        // A real transition: this write is the moment of entry.
        Some(p) if p != row.status => Some(row.updated),
        // Same status: carry whatever is there, INCLUDING None. A card that
        // predates the column keeps its NULL until it actually moves, which is
        // the honest answer and is why nothing was backfilled.
        Some(_) => row.entered_state_at,
        // Row not on disk yet. Carry the caller's value rather than inventing
        // one; `create_issue` stamps it at insert.
        None => row.entered_state_at,
    }
}

pub fn save_patched(conn: &Connection, row: &mut IssueRow) -> rusqlite::Result<usize> {
    let dep_json = if row.depends_on.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&row.depends_on).unwrap_or_default())
    };
    // Written back ONTO the row, not merely into the UPDATE. `replay_roundtrip`
    // caught this: the state-event journal snapshots the caller's struct, so a
    // value computed only for the SQL params was absent from the journal and
    // replaying it could no longer reproduce the live row. Anything derived
    // inside this function has to land on the row or the journal quietly stops
    // being a faithful record — which is the one property replay depends on.
    row.closed_at = closed_at_for_write(conn, row);
    row.entered_state_at = entered_state_at_for_write(conn, row);
    // A callback is armed by the request create/edit and becomes an outbox
    // item at the ONE write choke point every status transition uses. This is
    // intentionally not a PATCH-handler side effect: board-drive, epic
    // completion and future transition producers all call save_patched too.
    let previous_status: Option<String> = conn
        .query_row("SELECT status FROM issues WHERE id = ?1", params![row.id], |r| r.get(0))
        .ok();
    if previous_status
        .as_deref()
        .is_some_and(|s| !is_terminal_status(s))
        && is_terminal_status(&row.status)
        && row.callback_session.as_deref().is_some_and(|s| !s.trim().is_empty())
        && row.callback_state.as_deref() == Some("armed")
    {
        row.callback_state = Some("pending".into());
        row.callback_error = None;
    }
    conn.execute(
        "UPDATE issues SET title = ?1, \"desc\" = ?2, status = ?3, session = ?4, due = ?5, \
             due_time = ?6, owner_type = ?7, pinned = ?8, pos = ?9, gate = ?10, shepherd = ?11, \
             type = ?12, archived = ?13, depends_on = ?14, reviewer = ?15, log = ?16, \
             rev = ?17, version = ?18, updated = ?19, source_ref = ?20, last_verified_at = ?21, \
             epic = ?22, closed_at = ?23, evidence = ?24, ask_type = ?25, \
             ask_question = ?26, ask_unblocks = ?27, next_action = ?28, \
             last_result = ?29, unresolved = ?30, entered_state_at = ?31, \
             blocked_on = ?32, acceptance_criteria = ?34, \
             decision_question = ?35, decision_rationale = ?36, \
             decision_supersedes = ?37, waiting_on = ?38, requested_by = ?39, \
             callback_session = ?40, callback_prompt = ?41, callback_state = ?42, \
             callback_message_id = ?43, callback_fired_at = ?44, callback_error = ?45, \
             ask_actor = ?46 \
         WHERE id = ?33 AND deleted IS NULL",
        params![
            row.title,
            row.desc,
            row.status,
            row.session.as_deref().filter(|s| !s.is_empty()),
            row.due,
            row.due_time,
            row.owner_type,
            row.pinned,
            row.pos,
            row.gate,
            row.shepherd,
            row.item_type,
            row.archived,
            dep_json,
            row.reviewer,
            row.log,
            row.rev,
            row.version,
            row.updated,
            row.source_ref,
            row.last_verified_at,
            row.epic,
            row.closed_at,
            row.evidence,
            row.ask_type,
            row.ask_question,
            row.ask_unblocks,
            row.next_action,
            row.last_result,
            row.unresolved,
            row.entered_state_at,
            row.blocked_on,
            row.id,
            row.acceptance_criteria,
            row.decision_question,
            row.decision_rationale,
            row.decision_supersedes,
            row.waiting_on,
            row.requested_by,
            row.callback_session,
            row.callback_prompt,
            row.callback_state,
            row.callback_message_id,
            row.callback_fired_at,
            row.callback_error,
            row.ask_actor,
        ],
    )
}

/// Replace the tag set (Python PATCH semantics: `tags` is the full new set).
pub fn set_tags(conn: &Connection, id: &str, tags: &[String], now: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM issue_tags WHERE issue_id = ?1", params![id])?;
    for tag in tags {
        conn.execute(
            "INSERT OR IGNORE INTO issue_tags (issue_id, tag, added_at) VALUES (?1, ?2, ?3)",
            params![id, tag, now],
        )?;
    }
    Ok(())
}

/// The canonical "blocked on a human" tag. Every consumer matches it as a
/// PREFIX (`lower(tag) LIKE 'needs:you%'`), so a sub-tagged ask like
/// `needs:you:decision` counts — board_drive's re-nag, its pickup exclusion
/// and its advance-path branch all already use that shape, and the helpers
/// below exist so a fourth caller cannot spell it a fifth way.
pub const NEEDS_YOU_TAG: &str = "needs:you";

/// Does this card carry any `needs:you*` tag?
pub fn has_needs_you_tag(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM issue_tags WHERE issue_id = ?1 \
         AND lower(tag) LIKE 'needs:you%')",
        params![id],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n != 0)
}

/// Stamp `needs:you` unless the card already carries one. Returns whether a
/// row was written.
///
/// `added_at` is the ASK CLOCK: board_drive ages the ask from
/// `MIN(issue_tags.added_at)`, deliberately not from `issues.updated`, because
/// `updated` is last-touch and the cards carrying the most commentary were
/// exactly the ones whose stale-ask check could never fire (AC-178). Stamping
/// at the transition is what makes that clock mean "when the human was asked".
pub fn add_needs_you_tag(conn: &Connection, id: &str, now: i64) -> rusqlite::Result<bool> {
    if has_needs_you_tag(conn, id)? {
        return Ok(false);
    }
    let n = conn.execute(
        "INSERT OR IGNORE INTO issue_tags (issue_id, tag, added_at) VALUES (?1, ?2, ?3)",
        params![id, NEEDS_YOU_TAG, now],
    )?;
    Ok(n > 0)
}

/// Drop every `needs:you*` tag. Returns how many rows went.
pub fn clear_needs_you_tags(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM issue_tags WHERE issue_id = ?1 AND lower(tag) LIKE 'needs:you%'",
        params![id],
    )
}

/// Would giving `self_id` the dependency set `new_deps` create a cycle?
/// Returns the cycle as SEMANTIC ids for the error message, or `None` when
/// acyclic. Uses core's [`board::detect_cycle`] over the whole board's
/// `DependsOn` edges (self's existing edges are replaced by `new_deps`,
/// matching PATCH replace semantics).
pub fn depends_on_cycle(
    conn: &Connection,
    self_id: &str,
    new_deps: &[String],
) -> rusqlite::Result<Option<Vec<String>>> {
    let mut names: HashMap<TaskId, String> = HashMap::new();
    let intern = |sem: &str, names: &mut HashMap<TaskId, String>| -> TaskId {
        let t = internal_id(sem);
        names.entry(t.clone()).or_insert_with(|| sem.to_string());
        t
    };
    let mut edges: Vec<(TaskId, TaskId)> = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, depends_on FROM issues \
         WHERE deleted IS NULL AND depends_on IS NOT NULL AND depends_on != ''",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, dep_json) = row?;
        if id == self_id {
            continue; // replaced by new_deps below
        }
        if let Ok(deps) = serde_json::from_str::<Vec<serde_json::Value>>(&dep_json) {
            for d in deps.iter().filter_map(|v| v.as_str()) {
                let from = intern(&id, &mut names);
                let to = intern(d, &mut names);
                edges.push((from, to));
            }
        }
    }
    for d in new_deps {
        let from = intern(self_id, &mut names);
        let to = intern(d, &mut names);
        edges.push((from, to));
    }
    // REFUSE ONLY A CYCLE THIS CALLER IS PART OF (AC-335).
    //
    // The graph above is EVERY depends_on edge on the board, and detect_cycle
    // returns the first cycle it finds anywhere in it. So one stale cycle
    // between two unrelated cards made every subsequent depends_on write fail —
    // with an error naming two ids the caller had never touched, which reads as
    // "your edit is circular" when it is not.
    //
    // Live specimen: GE-473 <-> MHC-256, two cards owned by other lanes and BOTH
    // already closed (done and verified). Setting AC-331 -> AC-330, which shares
    // no node with either, was refused as "circular depends_on: GE-473 ->
    // MHC-256". Board-wide, for everyone, until someone broke a cycle between two
    // finished cards nobody was looking at.
    //
    // The check is sound because new edges all originate at `self_id`: adding
    // them can only create cycles that pass THROUGH self_id. A cycle without
    // self_id therefore pre-existed this request and is not this caller's to fix.
    //
    // Pre-existing cycles are still real board damage, so they are logged rather
    // than swallowed — the caller is unblocked, and the problem stays visible to
    // whoever owns those cards.
    let self_tid = internal_id(self_id);
    Ok(board::detect_cycle(&edges).and_then(|cycle| {
        let named: Vec<String> = cycle
            .iter()
            .map(|t| {
                names
                    .get(t)
                    .cloned()
                    .unwrap_or_else(|| t.as_str().to_string())
            })
            .collect();
        if cycle.contains(&self_tid) {
            Some(named)
        } else {
            tracing::warn!(
                cycle = %named.join(" -> "),
                self_id = %self_id,
                "pre-existing depends_on cycle elsewhere on the board — not blocking this write (AC-335)"
            );
            None
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AF-332. The probe must catch what `current_rev()` cannot.
    ///
    /// The outage this exists for: `GET /api/board` 500'd for the whole fleet
    /// for ~20 minutes while /health reported store:"ok" the entire time,
    /// because the failure was in ROW MAPPING and the store check only asks
    /// whether the connection answers a revision query. Nothing else amux has
    /// covered it either - no invariant, no autofix detector, and a dashboard
    /// showing an empty board that reads identically to a quiet one.
    ///
    /// So the cell that matters is not "the probe returns Ok on a good DB".
    /// It is "the probe FAILS on a row the mapper cannot decode, in a database
    /// whose connection is perfectly healthy". A `SELECT 1` liveness check
    /// passes that case, which is precisely why it would not have helped.
    #[test]
    fn board_probe_fails_on_a_row_the_mapper_cannot_decode() {
        let conn = crate::db::migrate::test_memdb();
        conn.execute(
            "INSERT INTO issues (id,title,status,created,updated) VALUES ('AF-1','t','todo',1,1)",
            [],
        )
        .unwrap();

        // Healthy baseline: the mapper decodes the row.
        assert_eq!(probe_board_read(&conn).unwrap(), 1, "a good row must map");

        // The CONTROL that makes the cell mean something: the connection is
        // fine and a revision-style query still succeeds. This is the state
        // /health reported as "ok" for 20 minutes.
        assert!(
            conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok(),
            "the connection must be healthy, or this cell proves nothing"
        );

        // Now corrupt a column's TYPE, not the connection. SQLite is
        // dynamically typed, so this is exactly how a schema drift or a bad
        // write reaches the mapper in production.
        conn.execute("UPDATE issues SET created = 'not-an-integer'", []).unwrap();

        assert!(
            probe_board_read(&conn).is_err(),
            "the probe must FAIL on an undecodable row - if it passes here it \
             is a liveness ping wearing a board read's name, and the AF-332 \
             outage recurs invisibly"
        );
        // And the connection is STILL healthy, which is the whole point.
        assert!(
            conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok(),
            "the store check would still say ok here - that is the gap"
        );
    }

    /// `last_verified_at` must read back from BOTH storage shapes.
    ///
    /// The naive fix for the legacy-TEXT crash is `Option<i64>`, and it is
    /// wrong in a way that passes a legacy-only test: rusqlite's `FromSql` is
    /// strict per stored type, so it handles the TEXT rows and breaks every
    /// INTEGER one, which is the majority of installs. `Option<String>` fails
    /// the mirror image. So a cell that only covers the legacy shape is GREEN
    /// against the fix that breaks everyone else, which is why both directions
    /// are here and why the integer cell is not decoration.
    #[test]
    fn last_verified_at_reads_back_from_both_integer_and_legacy_text_storage() {
        let conn = crate::db::migrate::test_memdb();

        // SQLite is dynamically typed: binding an i64 stores INTEGER, binding a
        // &str stores TEXT, in the same column. That is how the legacy rows
        // exist at all, and it is what lets one table hold both here.
        for (id, bind) in [
            ("INT-1", &1787840686i64 as &dyn rusqlite::ToSql),
            ("TXT-1", &"1787840686" as &dyn rusqlite::ToSql),
            ("TXT-2", &" 1787840686 " as &dyn rusqlite::ToSql), // whitespace, trimmed
        ] {
            // title/created/updated are NOT NULL with NO default in the real
            // schema. The hand-rolled fixture this test used to carry declared
            // `title TEXT NOT NULL DEFAULT ''`, so these inserts were valid
            // against a schema that does not exist — the fixture was more
            // PERMISSIVE than production, which is the sharper half of AF-328:
            // drift does not only hide columns, it can hide constraints.
            conn.execute(
                "INSERT INTO issues (id, title, created, updated, last_verified_at) \
                 VALUES (?1, ?1, 0, 0, ?2)",
                params![id, bind],
            )
            .expect("insert");
        }
        // Genuine absence, and the unreadable case that must not be mistaken for it.
        conn.execute(
            "INSERT INTO issues (id, title, created, updated) VALUES ('NUL-1','NUL-1',0,0)",
            [],
        )
        .expect("insert");
        conn.execute(
            "INSERT INTO issues (id, title, created, updated, last_verified_at) \
             VALUES ('BAD-1','BAD-1',0,0,'yesterday')",
            [],
        )
        .expect("insert");

        // FIXTURE GUARD: prove the two storage shapes really are different in
        // the table. Without this, a binding that quietly coerced everything to
        // one type would make the whole test pass for the wrong reason.
        let kinds: Vec<String> = conn
            .prepare("SELECT typeof(last_verified_at) FROM issues ORDER BY id")
            .and_then(|mut st| {
                st.query_map([], |r| r.get::<_, String>(0))
                    .map(|it| it.flatten().collect())
            })
            .expect("typeof");
        assert!(
            kinds.contains(&"integer".to_string()) && kinds.contains(&"text".to_string()),
            "the fixture must actually hold both storage shapes, got {kinds:?}"
        );

        let at = |id: &str| get_issue(&conn, id).expect("read").expect("row").last_verified_at;
        // PER-ROW STORAGE, not just "the set holds both shapes" (AF-328).
        //
        // The guard above is satisfied as long as SOME row is text and SOME row
        // is integer, and BAD-1 supplies the text on its own. That let two
        // assertions below claim to exercise the legacy-TEXT arm while their
        // rows were stored as INTEGER: SQLite applies INTEGER affinity on write,
        // so a numeric STRING is converted before it is ever stored, whitespace
        // and all. Asserting each row's own typeof is what makes the label and
        // the fact agree.
        let kind = |id: &str| -> String {
            conn.query_row("SELECT typeof(last_verified_at) FROM issues WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .expect("typeof")
        };
        assert_eq!(kind("INT-1"), "integer");
        assert_eq!(kind("TXT-1"), "integer", "a numeric string is coerced by INTEGER affinity");
        assert_eq!(kind("TXT-2"), "integer", "whitespace does not defeat affinity either");
        assert_eq!(kind("BAD-1"), "text", "only a NON-numeric string survives as text");

        assert_eq!(at("INT-1"), Some(1787840686), "the normal INTEGER case");
        assert_eq!(at("TXT-1"), Some(1787840686), "a numeric string, coerced to INTEGER on write");
        assert_eq!(at("TXT-2"), Some(1787840686), "same, with surrounding whitespace");
        assert_eq!(at("NUL-1"), None, "NULL is genuine absence");
        assert_eq!(at("BAD-1"), None, "unreadable text degrades to None (and warns)");

        // WHAT THIS TEST CANNOT REACH, said out loud rather than left implied.
        // The reader's `Value::Text(s) => s.trim().parse::<i64>()` SUCCESS arm is
        // not reachable through SQL: any text that parses as an integer is
        // converted to one on write, and any text that survives as text does not
        // parse. It guards rows written by something that bypassed affinity.
        // Measured on the live DB 2026-08-30: 1,423 integer, 11,572 null, ZERO
        // text — so the condition is currently hypothetical, and this test
        // covers the failure branch only.
    }

    #[test]
    fn revisit_arrived_reads_only_a_well_formed_date_and_can_fail() {
        // The whole point of the predicate: today and earlier are due.
        assert!(bs_revisit("2026-08-28", "2026-08-29"));
        assert!(bs_revisit("2026-08-29", "2026-08-29"));
        // The future is not due. Without this the drain promotes everything
        // the moment a date is stamped, which is the opposite of the feature.
        assert!(!bs_revisit("2026-08-30", "2026-08-29"));
        assert!(!bs_revisit("2026-12-01", "2026-08-29"));
        // ABSENT IS NOT DUE. 589 of 589 backlog cards had no due date when
        // this shipped; reading None as "" and comparing "" <= today would
        // have promoted the entire fleet backlog into `todo` on the first tick.
        assert!(!revisit_arrived(None, "2026-08-29"));
        assert!(!bs_revisit("", "2026-08-29"));
        assert!(!bs_revisit("   ", "2026-08-29"));
        // UNPARSEABLE IS NOT DUE either — "I could not read this" and "the
        // date arrived" are different answers (ethos rule 4). Every one of
        // these string-compares as <= "2026-08-29" and must still be refused.
        assert!(!bs_revisit("2026-8-29", "2026-08-29"));
        assert!(!bs_revisit("2026/08/28", "2026-08-29"));
        assert!(!bs_revisit("yesterday", "2026-08-29"));
        assert!(!bs_revisit("2026-08-28T09:00", "2026-08-29"));
        assert!(!bs_revisit("20260828", "2026-08-29"));
    }

    fn bs_revisit(due: &str, today: &str) -> bool {
        revisit_arrived(Some(due), today)
    }

    #[test]
    fn revisit_default_applies_to_the_two_undrained_statuses_only() {
        // The two statuses with no gate and no automated exit get a date.
        assert_eq!(default_revisit_days(TaskStatus::Backlog, None), Some(14));
        assert_eq!(default_revisit_days(TaskStatus::NeedsYou, None), Some(3));
        // Every status that HAS a next actor must not be stamped — a `doing`
        // card with a due date would read as a deadline nobody set.
        for st in [
            TaskStatus::Todo,
            TaskStatus::Doing,
            TaskStatus::Review,
            TaskStatus::Done,
            TaskStatus::Verified,
            TaskStatus::Blocked,
            TaskStatus::Discarded,
        ] {
            assert_eq!(default_revisit_days(st, None), None, "{st:?} must not be stamped");
        }
    }

    #[test]
    fn revisit_date_is_the_stored_due_format_and_sorts_against_a_hand_set_one() {
        let d = revisit_date(14);
        assert_eq!(d.len(), 10, "must be YYYY-MM-DD, got {d}");
        // The format must be the one `revisit_arrived` accepts, or the stamp
        // and the reader disagree and nothing ever promotes. Round-trip it
        // through the real predicate rather than a second regex.
        assert!(!revisit_arrived(Some(&d), &revisit_date(0)), "14d out must not be due today");
        assert!(revisit_arrived(Some(&revisit_date(-1)), &revisit_date(0)));
    }

    #[test]
    fn asset_link_detector_can_fail_and_accepts_real_pointers() {
        // Pure prose with no artifact reference must FAIL (ethos rule 7).
        assert!(!has_asset_link("Fixed it and closed out"));
        assert!(!has_asset_link("addressed the feedback from review"));
        assert!(!has_asset_link(""));
        // Each real pointer shape must PASS.
        assert!(has_asset_link("see https://amux.io/x for details"));
        assert!(has_asset_link("wrote it up in [the doc](docs/x.md)"));
        assert!(has_asset_link("landed in docs/design/connectors.md"));
        assert!(has_asset_link("crates/amux-server/src/api/board.rs updated"));
        assert!(has_asset_link("produced video-moderation-launch.mp4"));
        assert!(has_asset_link("and video-moderation-launch-9x16.mp4"));
        assert!(has_asset_link("shipped as 53a868f"));
        assert!(has_asset_link("closes #106"));
        // A short hex-ish word is not a sha, a bare year is too short.
        assert!(!has_asset_link("the cafe was open in 2026"));
        assert!(!has_asset_link("result-a.txt/result-b.txt"));
        assert!(!has_asset_link("plan.md/result-a.txt/result-b.txt"));
        assert!(asset_refs("[ghost](plan.md/result-a.txt)").is_empty());
        // Command flags and elapsed-time result text are not produced files.
        // Both appeared as clickable, missing artifacts on ATE-37 because the
        // old token parser looked only for a dotted tail.
        assert!(!has_asset_link("--config=e2e/playwright.config.ts"));
        assert!(!has_asset_link("3.0m"));
        assert!(!has_asset_link("finished in 42.7s"));
        assert!(!has_asset_link("processed 1.93M rows"));
        assert_eq!(
            asset_refs("customers/tubescience/.env"),
            vec!["customers/tubescience/.env"],
            "a hidden dotfile is still a produced file"
        );
        assert_eq!(
            asset_refs("customers/.private/result.json"),
            vec!["customers/.private/result.json"],
            "hidden path components must not make a real file ambiguous"
        );

        // The card renderer consumes the SAME parser and must receive every
        // produced asset, not just the first boolean proof that let Done pass.
        assert_eq!(
            asset_refs(
                "wrote [report](docs/report.md), screenshot /tmp/run.png and https://example.test/a; \
                 commit 53a868f, PR #106; report again docs/report.md"
            ),
            vec![
                "docs/report.md",
                "https://example.test/a",
                "/tmp/run.png",
                "53a868f",
                "#106",
            ]
        );

        assert_eq!(
            output_asset_refs("Preserving unrelated sessions_legacy.rs while investigating"),
            Vec::<String>::new(),
            "an input/peer file mention is not a produced output"
        );
        assert_eq!(
            output_asset_refs("Produced result.md and /tmp/screenshot.png"),
            vec!["result.md".to_string(), "/tmp/screenshot.png".to_string()]
        );
    }

    /// ONE ROW WITH A REAL TIMESTAMP MUST NOT TAKE THE WHOLE LIST DOWN.
    ///
    /// The live incident, as a test. `updated` is declared INTEGER; the
    /// queue-disposition job wrote three rows with an f64, and because
    /// rusqlite's FromSql is strict per storage type, `GET /api/board` returned
    /// `Invalid column type Real at index: 6, name: updated` for EVERY session
    /// — 12,959 good rows made unreadable by 3 bad ones.
    ///
    /// The control is the point: the good row must still come back with its
    /// exact value, or a reader that silently zeroed every timestamp would pass
    /// this too.
    /// AMUX-3906. Both INTEGER timestamp columns in the row mapper must go
    /// through the tolerant reader, not just the one that happened to break.
    ///
    /// AF-317 made `updated` tolerant because three queue-disposition rows had
    /// written it as f64, failing GET /api/board fleet-wide on 3 bad cells in
    /// ~13,000 rows. `created` sits at the next index, is declared INTEGER, and
    /// was still read strictly — the identical all-or-nothing hazard, unfired
    /// only because nothing had written a REAL there yet. That is a fix to the
    /// instance rather than to the class.
    ///
    /// Source-level because the failure is a column being ADDED or REVERTED to a
    /// strict read, which no behavioural test over today's data can see: the
    /// hazard needs a REAL in the cell, and a correct database never has one.
    #[test]
    fn every_integer_timestamp_in_the_row_mapper_is_read_tolerantly() {
        let src = include_str!("board_store.rs");
        let start = src.find("fn issue_from_row").expect("the row mapper exists");
        let body = &src[start..start + 4000];
        for field in ["created", "updated"] {
            let line = body
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("{field}:")))
                .unwrap_or_else(|| panic!("`{field}` is read in issue_from_row"));
            assert!(
                line.contains("ts_i64("),
                "`{field}` is an INTEGER-declared timestamp and must use ts_i64, or one row \
                 holding a REAL fails the entire list read for every session (AF-317 took the \
                 board down fleet-wide over 3 such cells). Got: {line}"
            );
        }
    }

    #[test]
    fn a_single_real_timestamp_does_not_fail_the_whole_list_read() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE issues (id TEXT PRIMARY KEY, status TEXT, session TEXT,
                archived INTEGER DEFAULT 0, pinned INTEGER DEFAULT 0, pos REAL DEFAULT 0,
                updated INTEGER, deleted INTEGER);",
        )
        .unwrap();
        // SQLite is dynamically typed, so this stores a REAL in an INTEGER column
        // exactly as the job did.
        conn.execute("INSERT INTO issues VALUES ('A-1','todo','x',0,0,0,?1,NULL)", [1788076327.487f64])
            .unwrap();
        conn.execute("INSERT INTO issues VALUES ('A-2','todo','x',0,0,0,?1,NULL)", [1788076327i64])
            .unwrap();

        let mut st = conn
            .prepare("SELECT id, status, session, archived, pinned, pos, updated FROM issues ORDER BY id")
            .unwrap();
        let rows: Vec<(String, i64)> = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, ts_i64(r, 6)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("a REAL in an INTEGER column must not fail the read");
        assert_eq!(rows.len(), 2, "both rows must come back");
        assert_eq!(rows[0].1, 1788076327, "the REAL row truncates to its second");
        // CONTROL: the correct row is unchanged, so a reader that zeroed
        // everything would fail here rather than pass.
        assert_eq!(rows[1].1, 1788076327, "the INTEGER row must be exact");

        // And the strict read is what USED to happen — pinned so the test is
        // known to be exercising the real hazard and not a hypothetical one.
        let mut st = conn.prepare("SELECT updated FROM issues WHERE id='A-1'").unwrap();
        assert!(
            st.query_row([], |r| r.get::<_, i64>(0)).is_err(),
            "if a strict i64 read of this cell succeeds, the fixture no longer reproduces the bug"
        );
    }

    /// THE GAP THIS CARD IS ABOUT, pinned so it cannot be argued away.
    ///
    /// Every accepting case in the test above pairs a path with an outcome verb
    /// ("landed in", "updated", "shipped as"), which reads as though the check
    /// requires one. It does not: it looks for a path-shaped token anywhere in
    /// the text, so the card's own PROBLEM STATEMENT satisfies it. Measured on
    /// the live board 2026-08-29: 843 of 1372 open cards (61%) passed this gate
    /// on their filed text with no work done.
    #[test]
    fn asset_link_cannot_tell_a_plan_from_an_outcome() {
        let plan = "Location: crates/amux-server/src/api/board.rs. \
                    Add the check there. Source: docs/fleet-friction-review.md";
        assert!(
            has_asset_link(plan),
            "the done link gate is satisfied by a card that has done nothing yet"
        );
        // Which is precisely why evidence is a separate column: this same text,
        // as evidence, is a plan, but nothing in its SHAPE says so — the
        // discrimination comes from the field being one nobody has written yet
        // when the card is filed, not from parsing the prose.
        assert_eq!(evidence_verdict(plan), EvidenceVerdict::Ok);
    }

    #[test]
    fn evidence_verdict_separates_proof_from_prose() {
        // Prose with nothing to re-run: the closes this card exists to stop.
        for prose in ["implemented", "done", "fixed it and closed out", "addressed review"] {
            assert_eq!(evidence_verdict(prose), EvidenceVerdict::NoArtifact, "{prose}");
        }
        assert_eq!(evidence_verdict(""), EvidenceVerdict::Missing);
        assert_eq!(evidence_verdict("   \n  "), EvidenceVerdict::Missing);

        // Things a reader can actually check.
        assert_eq!(evidence_verdict("ran `cargo test -p amux-server`, 412 passed"), EvidenceVerdict::Ok);
        assert_eq!(evidence_verdict("$ scripts/test-contended.sh -p amux-server"), EvidenceVerdict::Ok);
        assert_eq!(evidence_verdict("shipped as 53a868f"), EvidenceVerdict::Ok);
        assert_eq!(evidence_verdict("verified at https://amux.io/board"), EvidenceVerdict::Ok);
        assert_eq!(evidence_verdict("screenshot at /tmp/shots/board-mobile.png"), EvidenceVerdict::Ok);

        // The honest no-artifact answer (ethos rule 3) — and its abuse.
        assert_eq!(
            evidence_verdict("none: owner decided to stand this down, no code changed"),
            EvidenceVerdict::Ok
        );
        assert_eq!(evidence_verdict("none: n/a"), EvidenceVerdict::UnexplainedNone);
        assert_eq!(evidence_verdict("none:"), EvidenceVerdict::UnexplainedNone);
    }

    #[test]
    fn done_link_rule_is_a_handler_constraint_not_a_gate_criterion() {
        // The label is stable (the contract shows it).
        assert!(ASSET_LINK_CRITERION.starts_with("Link to the created asset"));
        // It is enforced in the handler, NOT folded into any gate list — no
        // type default across any status carries it, so the ack ladder tests
        // stay clean and a `gate_ack` never touches it.
        for ty in ["code", "investigation", "chore", "doc", "watch"] {
            for st in [
                TaskStatus::Doing,
                TaskStatus::Review,
                TaskStatus::Done,
                TaskStatus::Verified,
            ] {
                assert!(
                    !default_gates_for(ty, st).contains(&ASSET_LINK_CRITERION.to_string()),
                    "type {ty} / {st:?} default must not embed the link label"
                );
            }
        }
        // The default is ON when no scope opts out.
        assert!(done_link_required(Some("no-such-lane-xyz")));
        assert!(done_link_required(None));
    }

    fn tag_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE issue_tags (issue_id TEXT, tag TEXT, added_at REAL,
                PRIMARY KEY (issue_id, tag));",
        )
        .unwrap();
        conn
    }

    /// Every consumer matches this family as a PREFIX (`LIKE 'needs:you%'`), so
    /// a sub-tagged ask must count as already-asked — otherwise a card carrying
    /// `needs:you:decision` gets a second, duplicate `needs:you` stamped on it
    /// and the ask clock (`MIN(added_at)`) is silently reset to now.
    #[test]
    fn needs_you_helpers_match_the_prefix_every_consumer_uses() {
        for existing in ["needs:you", "needs:you:decision", "NEEDS:YOU"] {
            let conn = tag_db();
            conn.execute(
                "INSERT INTO issue_tags VALUES ('C-1', ?1, 100.0)",
                params![existing],
            )
            .unwrap();
            assert!(has_needs_you_tag(&conn, "C-1").unwrap(), "{existing} must count as asked");
            assert!(
                !add_needs_you_tag(&conn, "C-1", 999).unwrap(),
                "{existing} is already an ask — stamping a second resets the clock"
            );
            let kept: f64 = conn
                .query_row("SELECT MIN(added_at) FROM issue_tags WHERE issue_id='C-1'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(kept, 100.0, "{existing}: the original ask time must survive");
            assert_eq!(clear_needs_you_tags(&conn, "C-1").unwrap(), 1);
            assert!(!has_needs_you_tag(&conn, "C-1").unwrap());
        }
    }

    /// A card FILED straight into `needsyou` — never PATCHed, so no status
    /// transition ever fires — must still carry the ask. This is the case with
    /// no second chance: nothing touches the card again, so if the tag is not
    /// stamped at creation it is never stamped at all.
    ///
    /// Found by exercising the shipped POST path after fixing only the PATCH
    /// path, which is the ethos-rule-1 nesting trap: after adding a surfacing
    /// mechanism, ask what the mechanism itself filters out. 1 of the 23 cards
    /// in the 2026-08-11 live census got there this way.
    #[test]
    fn a_card_filed_directly_into_needsyou_carries_the_ask() {
        for (status, want) in [("needsyou", true), ("needs_you", true), ("todo", false)] {
            let conn = create_db();
            let row = create_issue(&conn, &new_card(status), 1000).expect("create");
            assert_eq!(row.status, status, "the fixture must actually store {status}");
            assert_eq!(
                has_needs_you_tag(&conn, &row.id).unwrap(),
                want,
                "filed as {status}: expected tagged={want}"
            );
        }
    }

    fn create_db() -> Connection {
        crate::db::migrate::test_memdb()
    }

    /// AMUX-3949. THE CARD'S CHECK: a card blocked in `review` and one blocked
    /// in `doing` must report DIFFERENT positions.
    ///
    /// Under `status='blocked'` they were the same state and the position was
    /// destroyed on the way in, which is exactly what you need when the block
    /// clears.
    #[test]
    fn blocked_is_a_dimension_so_the_lifecycle_position_survives() {
        let conn = create_db();
        let mut in_review = create_issue(&conn, &new_card("review"), 1000).expect("a");
        let mut in_doing = create_issue(&conn, &new_card("doing"), 1000).expect("b");

        in_review.blocked_on = Some("waiting on the KubeRay answer".into());
        in_doing.blocked_on = Some("waiting on the KubeRay answer".into());
        save_patched(&conn, &mut in_review).expect("save a");
        save_patched(&conn, &mut in_doing).expect("save b");

        let a = get_issue(&conn, &in_review.id).expect("read").expect("row");
        let b = get_issue(&conn, &in_doing.id).expect("read").expect("row");
        assert_eq!(a.status, "review", "a blocked card keeps where it was");
        assert_eq!(b.status, "doing", "and so does the other one");
        assert_ne!(a.status, b.status, "which is the whole point: two positions, one block");
        assert_eq!(a.blocked_on.as_deref(), Some("waiting on the KubeRay answer"));

        // CLEARING IS INDEPENDENT of any status move. Blocking and unblocking
        // must not require pretending the card changed position.
        in_doing.blocked_on = None;
        save_patched(&conn, &mut in_doing).expect("clear");
        let b2 = get_issue(&conn, &in_doing.id).expect("read").expect("row");
        assert_eq!(b2.blocked_on, None, "the dimension clears");
        assert_eq!(b2.status, "doing", "and the position is untouched by the clear");
    }

    /// CONTROL, and the one that keeps this from being a regression: the LEGACY
    /// `status='blocked'` spelling still exists on 66 cards owned by other lanes,
    /// which this work deliberately did not rewrite (ethos rule 8). Both
    /// spellings must remain recognisable as blocked.
    ///
    /// A consumer honouring only the new field would silently make every legacy
    /// blocked card workable -- worse than the position-destroying status it
    /// replaces, because at least that one was visible.
    #[test]
    fn the_legacy_blocked_status_is_still_blocked() {
        let conn = create_db();
        let legacy = create_issue(&conn, &new_card("blocked"), 1000).expect("legacy");
        let row = get_issue(&conn, &legacy.id).expect("read").expect("row");
        assert_eq!(row.status, "blocked", "the legacy spelling is untouched");
        assert_eq!(
            row.blocked_on, None,
            "and it was NOT backfilled: 66 of the 67 belong to other lanes"
        );
        // The frontier's candidate query is `status='todo'`, so a legacy blocked
        // card is excluded by position. Asserted here so that if anyone widens
        // that query, this cell says what it costs.
        assert_ne!(row.status, "todo", "a legacy blocked card is not a todo candidate");
    }

    /// AMUX-3948. THE CARD'S OWN CHECK: a card whose blocker is open must not
    /// appear on the frontier, and must appear the moment the blocker closes.
    ///
    /// Driven through `deps_blocking`, the SHARED predicate the drive loop uses,
    /// rather than a re-derivation. AMUX-3814 is why: a parallel re-derivation of
    /// "which statuses are terminal" swept in an extra one and reddened an
    /// invariant for 8 days.
    #[test]
    fn a_card_appears_on_the_frontier_the_moment_its_blocker_closes() {
        let conn = create_db();
        let mut blocker = create_issue(&conn, &new_card("todo"), 1000).expect("blocker");
        let mut dependent = create_issue(&conn, &new_card("todo"), 1000).expect("dependent");
        dependent.depends_on = vec![blocker.id.clone()];
        save_patched(&conn, &mut dependent).expect("save deps");

        let blocked = crate::runtime_jobs::board_drive::deps_blocking(&conn, &dependent);
        assert_eq!(blocked, vec![blocker.id.clone()], "an open blocker blocks");

        // CONTROL, and the half that stops this passing for the wrong reason: a
        // card with NO dependencies is never blocked. Without it, a predicate
        // that returned "blocked" for everything would satisfy the assertion
        // above and the frontier would always be empty.
        assert!(
            crate::runtime_jobs::board_drive::deps_blocking(&conn, &blocker).is_empty(),
            "a card with no dependencies must never be blocked"
        );

        // Close the blocker -> the dependent becomes ready in the same tick.
        blocker.status = "done".into();
        blocker.updated = 2000;
        save_patched(&conn, &mut blocker).expect("close blocker");
        assert!(
            crate::runtime_jobs::board_drive::deps_blocking(&conn, &dependent).is_empty(),
            "closing the blocker must free the dependent immediately"
        );

        // A DEPENDENCY THAT RESOLVES TO NOTHING MUST NOT BLOCK, and this arm
        // was NOT covered until a mutant said so. `None => true` survived the
        // control above, because a card with an EMPTY depends_on never enters
        // the filter at all -- so "a card with no dependencies is unblocked"
        // is true whatever the None arm does. Two different things were both
        // called "no dependency".
        //
        // The behaviour is the function's own documented rule: an id that
        // resolves to nothing cannot be worked, and treating it as a blocker
        // parks the holder forever.
        dependent.depends_on = vec!["AMUX-DOES-NOT-EXIST".into()];
        save_patched(&conn, &mut dependent).expect("save phantom dep");
        assert!(
            crate::runtime_jobs::board_drive::deps_blocking(&conn, &dependent).is_empty(),
            "a dependency on a card that does not exist must not park the holder forever"
        );

        // A DISCARDED blocker frees it too. Discard is a judgement, not a pause,
        // and treating it as still-blocking parks the dependent forever.
        let mut b2 = create_issue(&conn, &new_card("todo"), 1000).expect("b2");
        dependent.depends_on = vec![b2.id.clone()];
        save_patched(&conn, &mut dependent).expect("save deps 2");
        assert!(!crate::runtime_jobs::board_drive::deps_blocking(&conn, &dependent).is_empty());
        b2.status = "discarded".into();
        b2.updated = 3000;
        save_patched(&conn, &mut b2).expect("discard b2");
        assert!(
            crate::runtime_jobs::board_drive::deps_blocking(&conn, &dependent).is_empty(),
            "a discarded blocker is resolved, not pending"
        );
    }

    /// AMUX-3947. entered_state_at records the TRANSITION, and an ordinary edit
    /// must not move it.
    ///
    /// The second half is the one that carries the value. Stamping on every
    /// write is easy and would pass a naive "the column is populated" check
    /// while making "in review for 9 days" read as 0 days the moment anybody
    /// appends a progress note -- reporting the opposite of the bottleneck this
    /// column exists to surface.
    #[test]
    fn entered_state_at_records_the_transition_not_the_touch() {
        let conn = create_db();
        let mut row = create_issue(&conn, &new_card("todo"), 1000).expect("create");
        assert_eq!(row.entered_state_at, Some(1000), "a new card enters its first status now");

        // 1. A real transition re-stamps.
        row.status = "doing".into();
        row.updated = 2000;
        save_patched(&conn, &mut row).expect("save");
        assert_eq!(row.entered_state_at, Some(2000), "moving status stamps the moment of entry");

        // 2. THE ARM THAT MATTERS: an ordinary edit does NOT.
        row.desc = "a progress note, five days later".into();
        row.updated = 7000;
        save_patched(&conn, &mut row).expect("save");
        assert_eq!(
            row.entered_state_at,
            Some(2000),
            "an edit is not a transition; moving this would erase the age it measures"
        );
        let back = get_issue(&conn, &row.id).expect("read").expect("row");
        assert_eq!(back.entered_state_at, Some(2000), "and it survives the round trip");

        // 3. Moving again re-stamps, so the field tracks the CURRENT state.
        row.status = "review".into();
        row.updated = 9000;
        save_patched(&conn, &mut row).expect("save");
        assert_eq!(row.entered_state_at, Some(9000));
    }

    /// A PRE-0040 CARD KEEPS ITS NULL until it actually moves.
    ///
    /// Nothing was backfilled, and this pins why: `updated` is the last TOUCH,
    /// so backfilling from it would have reported a card sitting in review since
    /// August as "in review for 0 days" if anyone had appended a note that day.
    /// NULL means not measured, which is true and is what consumers must render.
    #[test]
    fn a_card_that_predates_the_column_reads_unmeasured_not_zero() {
        let conn = create_db();
        conn.execute(
            "INSERT INTO issues (id,title,status,created,updated) \
             VALUES ('OLD-1','legacy','review',1,5000)",
            [],
        )
        .unwrap();
        let mut row = get_issue(&conn, "OLD-1").expect("read").expect("row");
        assert_eq!(row.entered_state_at, None, "no backfill: absence is the honest answer");

        // An unrelated edit must NOT invent a value for it.
        row.desc = "touched".into();
        row.updated = 6000;
        save_patched(&conn, &mut row).expect("save");
        assert_eq!(
            row.entered_state_at, None,
            "touching a legacy card must not fabricate a state-entry time"
        );

        // It self-heals the moment the card actually moves.
        row.status = "done".into();
        row.updated = 8000;
        save_patched(&conn, &mut row).expect("save");
        assert_eq!(row.entered_state_at, Some(8000), "a real move makes it measured");
    }

    /// AMUX-3946. The continuation gate's predicate, both arms.
    ///
    /// The refusal is the headline; the ACCEPTANCE is what stops the fix from
    /// being a blanket refusal that lanes route around with --force.
    #[test]
    fn a_claim_needs_a_next_action_a_stranger_could_act_on() {
        assert_eq!(continuation_verdict(""), ContinuationVerdict::Missing);
        assert_eq!(continuation_verdict("   "), ContinuationVerdict::Missing);
        // Real specimens of the shrug this exists to refuse.
        assert_eq!(continuation_verdict("wip"), ContinuationVerdict::NotASentence);
        assert_eq!(continuation_verdict("continue"), ContinuationVerdict::NotASentence);
        // ACCEPTED, and it has to be, or the gate is unsatisfiable.
        assert_eq!(
            continuation_verdict("Rerun compatibility test 07 against KubeRay 1.4"),
            ContinuationVerdict::Ok
        );
        // THE HONEST EDGE, stated rather than hidden: this gate can force a
        // sentence, not sincerity. "still working on it" is four words and
        // passes. Three words is the floor at which somebody has had to think
        // about the reader, and no predicate here can do better than that.
        assert_eq!(continuation_verdict("still working on it"), ContinuationVerdict::Ok);
    }

    /// SCOPE. The gate is on `doing` and nowhere else, and the other states are
    /// asserted rather than assumed: a gate that quietly applied to `review`
    /// too would refuse transitions nobody was warned about.
    #[test]
    fn the_continuation_gate_applies_to_doing_and_nothing_else() {
        assert!(continuation_applies(TaskStatus::Doing));
        for st in [
            TaskStatus::Todo,
            TaskStatus::Review,
            TaskStatus::NeedsYou,
            TaskStatus::Done,
            TaskStatus::Verified,
            TaskStatus::Discarded,
        ] {
            assert!(!continuation_applies(st), "{st:?} must not be gated by Phase 1");
        }
    }

    /// OPT-IN, and OFF by default. Decision 3 on AMUX-3945: this is a cost, and
    /// it lands on 52 lanes at once, so amux eats it first.
    ///
    /// The env override is asserted in both directions because a flag that can
    /// only be turned ON is not a flag, and this one has to be switchable off
    /// by a lane it is hurting without a deploy.
    #[test]
    fn the_continuation_gate_is_off_until_a_lane_opts_in() {
        // A guard so this cannot leak into other tests in the binary.
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var(CONTINUATION_REQUIRED_KEY, v),
                    None => std::env::remove_var(CONTINUATION_REQUIRED_KEY),
                }
            }
        }
        let _r = Restore(std::env::var(CONTINUATION_REQUIRED_KEY).ok());

        std::env::remove_var(CONTINUATION_REQUIRED_KEY);
        assert!(!continuation_required(Some("some-lane")), "default is OFF");
        assert!(!continuation_required(None), "an unattributed caller is not gated");

        std::env::set_var(CONTINUATION_REQUIRED_KEY, "1");
        assert!(continuation_required(Some("some-lane")), "env can turn it on");
        std::env::set_var(CONTINUATION_REQUIRED_KEY, "0");
        assert!(!continuation_required(Some("some-lane")), "and off again");
    }

    /// THE WRITE MUST SURVIVE THE ROUND TRIP.
    ///
    /// Adding a column to a struct and to a SELECT while forgetting the UPDATE
    /// is the exact shape of the silent-drop bugs this file already records
    /// (AC-323: a field that lands in `ignored_fields` and does nothing). The
    /// gate would then refuse a card whose `next_action` had been written and
    /// discarded, which is worse than having no gate.
    #[test]
    fn the_continuation_fields_survive_save_and_reload() {
        let conn = create_db();
        let mut row = create_issue(&conn, &new_card("todo"), 1000).expect("create");
        assert_eq!(row.next_action, None, "a fresh card carries no continuation");

        row.next_action = Some("Rerun compatibility test 07 against KubeRay 1.4".into());
        row.last_result = Some("E2E 07 failed on namespace-scoped discovery".into());
        row.unresolved = Some("Do multiple namespaces need support?".into());
        save_patched(&conn, &mut row).expect("save");

        let back = get_issue(&conn, &row.id).expect("read").expect("row");
        assert_eq!(back.next_action.as_deref(), Some("Rerun compatibility test 07 against KubeRay 1.4"));
        assert_eq!(back.last_result.as_deref(), Some("E2E 07 failed on namespace-scoped discovery"));
        assert_eq!(back.unresolved.as_deref(), Some("Do multiple namespaces need support?"));
    }

    /// AMUX-3609. The write rule lives in `save_patched`, so these drive the
    /// real function rather than a paraphrase of it.
    ///
    /// The third case is the one that motivated putting the rule behind the
    /// previous status instead of behind `closed_at IS NULL`: almost every
    /// closed card on this board predates the column and backfilled to NULL, so
    /// a stateless rule would stamp a card closed in June with today's date on
    /// its next unrelated edit. A fabricated date wearing the authority of a
    /// real one is worse than the NULL it replaces.
    #[test]
    fn closed_at_records_the_transition_not_the_touch() {
        let conn = create_db();
        let mut row = create_issue(&conn, &new_card("doing"), 1000).expect("create");
        assert_eq!(row.closed_at, None, "an open card has no close time");

        // 1. Closing stamps it.
        row.status = "done".into();
        row.updated = 2000;
        save_patched(&conn, &mut row).unwrap();
        let after_close = get_issue(&conn, &row.id).unwrap().unwrap();
        assert_eq!(after_close.closed_at, Some(2000), "closing must stamp the close time");

        // 2. An UNRELATED edit while already closed must not move it. This is
        //    what makes the field mean "when it closed" rather than "when it
        //    was last touched while closed", which would just be `updated`
        //    again and would reproduce the whole bug one column over.
        let mut touched = after_close.clone();
        touched.desc = "a later comment".into();
        touched.updated = 5000;
        save_patched(&conn, &mut touched).unwrap();
        assert_eq!(
            get_issue(&conn, &row.id).unwrap().unwrap().closed_at,
            Some(2000),
            "commenting on a closed card must not restamp its close time"
        );

        // 3. THE FABRICATION CASE. A card that is already closed and carries a
        //    NULL close time (every pre-column row whose journal was reaped)
        //    must stay NULL through an unrelated edit. Honest ignorance beats a
        //    confident wrong date.
        conn.execute(
            "UPDATE issues SET closed_at = NULL WHERE id = ?1",
            params![row.id],
        )
        .unwrap();
        let mut legacy = get_issue(&conn, &row.id).unwrap().unwrap();
        assert_eq!(legacy.closed_at, None, "fixture must actually be NULL or this proves nothing");
        legacy.desc = "another comment".into();
        legacy.updated = 9000;
        save_patched(&conn, &mut legacy).unwrap();
        assert_eq!(
            get_issue(&conn, &row.id).unwrap().unwrap().closed_at,
            None,
            "an unrelated edit must NOT invent a close date for a card that never recorded one"
        );

        // 4. Reopening clears it. Leaving a stale stamp would make
        //    `closed_at IS NOT NULL` mean "was closed once" while reading like
        //    "is closed".
        // The card is currently `done` with a NULL stamp (case 3 left it there),
        // so REOPEN first — setting `done` on a card that is already `done` is
        // not a transition and would prove nothing. The first draft of this
        // test did exactly that and went red, which is the check working.
        let mut back = get_issue(&conn, &row.id).unwrap().unwrap();
        back.status = "doing".into();
        back.updated = 9_500;
        save_patched(&conn, &mut back).unwrap();
        assert_eq!(get_issue(&conn, &row.id).unwrap().unwrap().closed_at, None);

        let mut reclosed = get_issue(&conn, &row.id).unwrap().unwrap();
        reclosed.status = "done".into();
        reclosed.updated = 10_000;
        save_patched(&conn, &mut reclosed).unwrap();
        assert_eq!(
            get_issue(&conn, &row.id).unwrap().unwrap().closed_at,
            Some(10_000),
            "re-closing stamps the LATEST close, matching what the 0031 backfill picks (MAX, not MIN)"
        );

        let mut back2 = get_issue(&conn, &row.id).unwrap().unwrap();
        back2.status = "doing".into();
        back2.updated = 11_000;
        save_patched(&conn, &mut back2).unwrap();
        assert_eq!(
            get_issue(&conn, &row.id).unwrap().unwrap().closed_at,
            None,
            "reopening must clear the close time"
        );
    }

    /// All three terminal statuses stamp, and a non-terminal one does not.
    /// Without the negative this passes just as well against `is_terminal_status`
    /// returning true for everything, which would stamp every card on every
    /// write and make the column another spelling of `updated`.
    #[test]
    fn every_terminal_status_closes_and_no_other_one_does() {
        for st in ["done", "verified", "discarded"] {
            let conn = create_db();
            let mut row = create_issue(&conn, &new_card("doing"), 1000).expect("create");
            row.status = st.into();
            row.updated = 4242;
            save_patched(&conn, &mut row).unwrap();
            assert_eq!(
                get_issue(&conn, &row.id).unwrap().unwrap().closed_at,
                Some(4242),
                "{st} is terminal and must stamp"
            );
        }
        for st in ["todo", "doing", "review", "backlog"] {
            let conn = create_db();
            let mut row = create_issue(&conn, &new_card("todo"), 1000).expect("create");
            row.status = st.into();
            row.updated = 4242;
            save_patched(&conn, &mut row).unwrap();
            assert_eq!(
                get_issue(&conn, &row.id).unwrap().unwrap().closed_at,
                None,
                "{st} is not terminal and must not stamp"
            );
        }
    }

    /// A peer request is durable board state, and its callback becomes an
    /// outbox item at the shared transition choke point exactly once. This
    /// deliberately uses `save_patched` directly rather than the HTTP handler:
    /// board-drive, epic completion and future actors all reach this path too.
    #[test]
    fn terminal_transition_arms_one_durable_peer_callback() {
        let conn = create_db();
        let mut new = new_card("todo");
        new.requested_by = Some("requester".into());
        new.callback_session = Some("requester".into());
        new.callback_prompt = Some("Start the dependent release card.".into());
        let mut row = create_issue(&conn, &new, 1000).expect("create request");

        assert_eq!(row.requested_by.as_deref(), Some("requester"));
        assert_eq!(row.callback_state.as_deref(), Some("armed"));
        assert_eq!(row.snapshot()["callback"]["session"], "requester");

        // Ordinary progress must not fire the callback.
        row.desc.push_str("progress");
        row.updated = 2000;
        save_patched(&conn, &mut row).expect("save progress");
        assert_eq!(row.callback_state.as_deref(), Some("armed"));

        // The first non-terminal -> terminal edge creates the pending outbox.
        row.status = "done".into();
        row.updated = 3000;
        save_patched(&conn, &mut row).expect("finish");
        assert_eq!(row.callback_state.as_deref(), Some("pending"));
        assert_eq!(row.closed_at, Some(3000));

        // Later terminal edits carry the pending state; they do not re-arm or
        // mint a second delivery.
        row.desc.push_str(" more detail");
        row.updated = 4000;
        save_patched(&conn, &mut row).expect("terminal detail edit");
        let stored = get_issue(&conn, &row.id).unwrap().unwrap();
        assert_eq!(stored.callback_state.as_deref(), Some("pending"));
        assert!(stored.callback_message_id.is_none());
    }

    #[test]
    fn needsyou_requires_a_routable_actor_a_real_question_and_an_exit() {
        assert_eq!(
            ask_verdict(
                "Ethan",
                "decision",
                "Which launch date should we use?",
                "The selected date is recorded on the card."
            ),
            AskVerdict::Ok
        );
        assert_eq!(
            ask_verdict(
                "human",
                "decision",
                "Which launch date should we use?",
                "The selected date is recorded on the card."
            ),
            AskVerdict::NoActor
        );
        assert_eq!(
            ask_verdict(
                "vendor-support",
                "external",
                "Waiting for their deployment response",
                "Their deployment response is attached to the card."
            ),
            AskVerdict::NotAQuestion
        );
        assert_eq!(
            ask_verdict("Ethan", "judgment", "Does this read well?", "done"),
            AskVerdict::NoUnblocks
        );
    }

    /// The column must reach the LIST, not only the full card. The board's slim
    /// payload has now dropped a needed column twice (`desc` at c207339,
    /// `reviewer` at AF-161), and the motivating question here — which cards
    /// closed in this window — is a list query, so omitting it would ship the
    /// column and withhold it from its only caller.
    #[test]
    fn closed_at_is_in_the_slim_list_payload_not_only_the_full_card() {
        let conn = create_db();
        let mut row = create_issue(&conn, &new_card("doing"), 1000).expect("create");
        row.status = "done".into();
        row.updated = 7777;
        save_patched(&conn, &mut row).unwrap();
        let closed = get_issue(&conn, &row.id).unwrap().unwrap();
        assert_eq!(closed.snapshot()["closed_at"], 7777);
        assert_eq!(
            closed.snapshot_slim()["closed_at"],
            7777,
            "a list consumer must be able to read the close time without fetching every card"
        );
    }

    fn new_card(status: &str) -> NewIssue {
        NewIssue {
            title: "Ask Ethan about pricing".into(),
            desc: String::new(),
            status: status.into(),
            session: Some("lane".into()),
            item_type: "code".into(),
            creator: "lane".into(),
            owner_type: "agent".into(),
            due: None,
            due_time: None,
            reviewer: None,
            shepherd: None,
            gate: vec![],
            depends_on: vec![],
            tags: vec![],
            ask_type: None,
            ask_question: None,
            ask_unblocks: None,
            ask_actor: None,
            source: None,
            requested_by: None,
            callback_session: None,
            callback_prompt: None,
        }
    }

    /// AMUX-3496 — snapshot_slim must be snapshot minus exactly {desc, log},
    /// on a row where every optional field is POPULATED (an empty row would
    /// pass with half the fields missing from both sides). If this fails,
    /// someone added a field to one serialization path and not the other.
    #[test]
    fn snapshot_slim_is_snapshot_minus_prose() {
        let conn = create_db();
        conn.execute(
            "INSERT INTO issues (id, title, desc, status, session, creator, due, created, \
                                 updated, owner_type, due_time, pinned, gcal_event_id, pos, \
                                 gate, shepherd, type, archived, depends_on, reviewer, log, \
                                 rev, source_ref, last_verified_at, version, epic) \
             VALUES ('F-1','t','prose body','doing','lane','me','2026-09-01', 100, 200, \
                     'agent','09:00', 1, 'gcal-1', 2.5, '[\"g1\"]', 'shep', 'code', 0, \
                     '[\"D-1\"]', 'rev-lane', 'log line', 3, 'src-ref', 150, 2, 'E-1')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO issue_tags VALUES ('F-1','b',1.0),('F-1','a',2.0)", []).unwrap();
        let row = get_issue(&conn, "F-1").unwrap().unwrap();
        let mut full = row.snapshot();
        let slim = row.snapshot_slim();
        // The prose keys exist in full (with real content — the fixture check)
        // and nowhere in slim.
        let fo = full.as_object_mut().unwrap();
        assert_eq!(fo.remove("desc").unwrap(), serde_json::json!("prose body"));
        assert!(fo.remove("log").unwrap().as_str().is_some());
        assert_eq!(full, slim, "snapshot_slim drifted from snapshot minus prose");
    }

    /// AF-346's TRAP, written BEFORE that optimisation lands rather than after.
    ///
    /// AF-346 proposes hydrating the slim list without `desc`/`log`, on the
    /// premise that "the slim serializer then drops both". It does not. The slim
    /// branch of `list_body` makes FIVE derivations over those two columns —
    /// `desc_len`, `log_n`, `desc_head`, `folded_n` and the NEEDS-YOU marker —
    /// and the SPA renders three of them. Hydrating without the prose blanks all
    /// five, for every card, silently.
    ///
    /// NOTHING WOULD HAVE CAUGHT IT. `capped_two_pass_equals_the_single_pass_it_
    /// replaced` below guards `hydrate_light` and does catch a prose-free
    /// hydration (verified by mutation: blanking desc/log there reds it) — but
    /// AF-346's own shape is a SEPARATE slim hydrate with `hydrate_light` kept
    /// for other callers, so that guard would not cover the new path. And
    /// `list_body`'s own slim tests construct `IssueRow` BY HAND with the prose
    /// populated, so they exercise the serializer and can never observe what
    /// hydration supplied. A test per component, none over the seam — the same
    /// shape as AF-429 and AF-438.
    ///
    /// The card's proposed test ("a slim list response must contain no
    /// desc/log") passes TODAY and would pass after the change, so it cannot
    /// discriminate. This one goes through the real hydration path and asserts
    /// the derived facts survive it.
    #[test]
    fn the_slim_lists_derived_facts_survive_whatever_hydration_supplies() {
        let conn = create_db();
        let now = 1_788_000_000i64;
        conn.execute(
            "INSERT INTO issues (id, title, \"desc\", status, session, created, updated, log, type) \
             VALUES ('D-1', 'a card', ?1, 'todo', 's', ?2, ?2, ?3, 'code')",
            rusqlite::params![
                "First line is the preview.\nNew task: folded one",
                now,
                "`10:00` did a thing\n`10:01` New task: folded two",
            ],
        )
        .unwrap();

        // BOTH hydrations, same assertions. The doc comment above predicted that
        // AF-346 would add "a SEPARATE slim hydrate ... so that guard would not
        // cover the new path". It does not have to be a separate guard: running
        // the identical assertions over both modes is what makes the new path
        // unable to differ from the old one, and a mode added later without a
        // row here fails to compile rather than silently going uncovered.
        for prose in [Prose::Full, Prose::SlimDerivations] {
            let (kept, _, _) =
                list_issues_capped(&conn, &[], &[], ArchivedFilter::All, 100, prose).unwrap();
            let row = kept.iter().find(|r| r.id == "D-1").expect("the seeded card");
            let slim = crate::api::board::list_body(row, true, false);

            // The diet still holds: the prose itself is not shipped.
            assert!(slim["desc"].is_null(), "slim must not ship the prose ({prose:?})");
            assert!(slim["log"].is_null(), "{prose:?}");

            // ...and every derivation over it survived the round trip. These are
            // the assertions the card's proposed test does not make.
            assert_eq!(
                slim["desc_head"], "First line is the preview.",
                "app.js renders this as the card preview — blank means every card lost its preview ({prose:?})"
            );
            assert_eq!(slim["folded_n"], 2, "counts 'New task:' across desc AND log ({prose:?})");
            assert_eq!(slim["desc_len"], 47, "{prose:?}");
            assert_eq!(slim["log_n"], 2, "{prose:?}");
        }
    }

    /// AF-346 — `cols_with_desc` substitutes into `COLS`, and a no-match is silent.
    ///
    /// `replacen` returns the input unchanged when the needle is absent, so a
    /// future rename of the desc column would leave the slim hydration selecting
    /// the WHOLE prose while still reporting rows as prefixed. That fails
    /// nothing at runtime and gives back the bug this card exists to fix, with
    /// the optimisation still apparently in place.
    ///
    /// Exactly once, not at-least-once: two occurrences and `replacen(.., 1)`
    /// would swap the first and leave the second selecting raw prose.
    #[test]
    fn cols_names_desc_exactly_once_so_the_substitution_cannot_silently_miss() {
        assert_eq!(
            COLS.matches(DESC_COL).count(),
            1,
            "COLS must name {DESC_COL} exactly once — cols_with_desc substitutes into it"
        );
        // A SENTINEL, not a realistic expression. The production replacement is a
        // CASE that reads `i."desc"` itself, so "the needle is gone afterwards"
        // is false for the real call and would be a test that only its own
        // fixture can pass. What must hold is that the substitution landed in
        // the projection and displaced the bare column.
        let swapped = cols_with_desc("'SENTINEL'");
        assert_ne!(swapped, COLS, "the substitution must actually change the projection");
        assert!(swapped.contains("'SENTINEL'"), "the expression must reach the projection");
        assert!(
            !swapped.contains(DESC_COL),
            "a substitution that leaves the bare column behind selects the prose anyway: {swapped}"
        );
    }

    /// AF-346 — the derivations must survive a `desc` that arrives TRUNCATED.
    ///
    /// The test above seeds a 47-character desc, so `Prose::SlimDerivations`
    /// hydrates it whole and the prefix path never runs. Everything that can go
    /// wrong with this optimisation is on the other side of that boundary, so
    /// every string here is deliberately built to straddle it.
    ///
    /// The comparison is against the SAME derivations computed from the whole
    /// prose, taken from `Prose::Full` in the same test. Hardcoded expectations
    /// would drift with the fixture and, worse, would let both sides be wrong
    /// together.
    #[test]
    fn a_prefixed_desc_produces_the_same_slim_derivations_as_a_whole_one() {
        let conn = create_db();
        let now = 1_788_000_000i64;
        // LEADING BLANK AND WHITESPACE-ONLY LINES, so `desc_head` exercises the
        // "first NON-EMPTY line" rule rather than "first line" — the two agree
        // on almost every real card, which is how the difference stayed
        // invisible while it was measured as equivalent on 8,260 rows.
        let head = "HEAD LINE, the card preview";
        let pad = "padding that pushes past the prefix boundary. ".repeat(40);
        // `New task:` markers AFTER the 512-char cut, so `folded_n` cannot be
        // recomputed from what was hydrated. A fallback that counted the prefix
        // would return 0 here and 0 is a plausible-looking answer.
        let plain = format!("\n\n   \n{head}\n{pad}\nNew task: alpha\nNew task: beta\n");
        assert!(plain.chars().count() > DESC_PREFIX_CHARS, "the fixture must straddle the cut");
        // A marker BEYOND the cut: this row must take the full-desc escape, or
        // the owner view silently loses the card's question.
        let marked = format!("{plain}NEEDS-YOU: does the escape fire?\n");
        // A NUL beyond the cut. SQLite LENGTH() stops at one, so this row must
        // also escape to the full column or `desc_len` comes back short.
        // The NUL goes in as an argument: `\u{0}` inside a format! literal reads
        // as a format placeholder to anyone skimming, and this cannot be misread.
        let nulled = format!("{plain}{}tail after the nul\n", '\u{0}');
        for (id, desc) in [("P-1", &plain), ("P-2", &marked), ("P-3", &nulled)] {
            conn.execute(
                "INSERT INTO issues (id, title, \"desc\", status, session, created, updated, log, type) \
                 VALUES (?1, 'a card', ?2, 'todo', 's', ?3, ?3, ?4, 'code')",
                rusqlite::params![id, desc, now, "`10:00` one\n\n`10:01` two\n\n\n`10:02` three"],
            )
            .unwrap();
        }

        let (full, _, _) =
            list_issues_capped(&conn, &[], &[], ArchivedFilter::All, 100, Prose::Full).unwrap();
        let (slim, _, _) =
            list_issues_capped(&conn, &[], &[], ArchivedFilter::All, 100, Prose::SlimDerivations)
                .unwrap();

        // POSITIVE CONTROL FIRST. Without it every assertion below is vacuous:
        // if the prefix never engaged, the two hydrations are the same bytes and
        // "they agree" is a tautology.
        let p1 = slim.iter().find(|r| r.id == "P-1").unwrap();
        assert_eq!(
            p1.desc.chars().count(),
            DESC_PREFIX_CHARS,
            "P-1 must actually arrive truncated, or this test proves nothing"
        );
        assert!(p1.desc_prefixed.is_some(), "and must SAY it is truncated");
        // The two escapes must NOT be truncated, and must say so the same way.
        for id in ["P-2", "P-3"] {
            let r = slim.iter().find(|r| r.id == id).unwrap();
            assert!(
                r.desc_prefixed.is_none(),
                "{id} carries a marker or a NUL, so it must escape to the whole column"
            );
            let f = full.iter().find(|r| r.id == id).unwrap();
            assert_eq!(r.desc, f.desc, "{id}'s escape must hydrate the SAME bytes");
        }

        // Now the claim: identical output, whichever way the row was loaded.
        for id in ["P-1", "P-2", "P-3"] {
            let f = crate::api::board::list_body(
                full.iter().find(|r| r.id == id).unwrap(), true, false);
            let s = crate::api::board::list_body(
                slim.iter().find(|r| r.id == id).unwrap(), true, false);
            for k in ["desc_len", "desc_head", "log_n", "folded_n", "needsyou_note"] {
                assert_eq!(f[k], s[k], "{id}: `{k}` differs between hydrations");
            }
            // Named individually too, so a failure says WHICH derivation broke
            // rather than only that two blobs differ.
            assert_eq!(s["desc_head"], head, "{id}: the preview must skip the blank lines");
            assert_eq!(s["folded_n"], 2, "{id}: both markers are past the cut");
            assert_eq!(s["log_n"], 3, "{id}: blank log lines are not entries");
        }
        // And the marker, which is the derivation the prefix cannot serve at all.
        let m = crate::api::board::list_body(
            slim.iter().find(|r| r.id == "P-2").unwrap(), true, false);
        assert_eq!(m["needsyou_note"], "does the escape fire?");
        let n = crate::api::board::list_body(
            slim.iter().find(|r| r.id == "P-3").unwrap(), true, false);
        assert_eq!(
            n["desc_len"], nulled.chars().count(),
            "a NUL-carrying desc must report its REAL length; SQLite LENGTH() stops at the NUL"
        );
    }

    /// AMUX-3491 — list_issues_capped is an OPTIMIZATION and must be
    /// byte-equivalent to the single-pass it replaced, across every axis the
    /// two passes could disagree on: filter canon, sort ties (shared
    /// `updated`), pins, explicit pos, the terminal cap, archived scoping,
    /// tags (the join only the hydration pass runs), and a deleted row.
    /// Rows compare by (id, desc, tags, log) so a hydration that dropped or
    /// misordered prose cannot pass on ids alone.
    #[test]
    fn capped_two_pass_equals_the_single_pass_it_replaced() {
        let conn = create_db();
        let statuses = ["todo", "done", "verified", "doing", "discarded", "backlog", "needsyou"];
        for i in 0..40 {
            let id = format!("C-{i:02}");
            conn.execute(
                // `created` mirrors `updated` (?6): the real schema has it NOT NULL
                // with no default, which the old hand-rolled fixture hid behind
                // `created INTEGER NOT NULL DEFAULT 0` (AF-328).
                "INSERT INTO issues (id, title, desc, status, session, updated, created, pos, pinned, \
                                     archived, log, deleted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    id,
                    format!("card {i}"),
                    format!("desc-{i} with prose"),
                    statuses[i % statuses.len()],
                    format!("lane{}", i % 3),
                    1000 + ((i % 7) as i64) * 10, // deliberate updated ties
                    if i % 5 == 0 { i as f64 } else { 0.0 },
                    i64::from(i % 11 == 0),
                    i64::from(i % 6 == 0),
                    if i % 4 == 0 { Some(format!("log-{i}")) } else { None },
                    if i == 39 { Some(1i64) } else { None },
                ],
            )
            .unwrap();
            if i % 3 == 0 {
                conn.execute(
                    "INSERT INTO issue_tags VALUES (?1, 'zeta', 1.0), (?1, 'alpha', 2.0)",
                    params![format!("C-{i:02}")],
                )
                .unwrap();
            }
        }
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cases: Vec<(Vec<String>, Vec<String>, ArchivedFilter, i64)> = vec![
            (vec![], vec![], ArchivedFilter::All, 5), // cap must engage
            (vec![], vec![], ArchivedFilter::All, 0), // uncapped
            (s(&["done"]), vec![], ArchivedFilter::ActiveOnly, 3),
            (vec![], s(&["lane1"]), ArchivedFilter::All, 2),
            (s(&["needs_you"]), vec![], ArchivedFilter::All, 100), // canon spelling
            (vec![], vec![], ArchivedFilter::ArchivedOnly, 1),
        ];
        let mut cap_engaged_somewhere = false;
        for (status_f, session_f, archived, limit) in cases {
            let (single, st, sk) =
                cap_terminal(list_issues(&conn, &status_f, &session_f, archived).unwrap(), limit);
            let (fused, ft, fk) =
                list_issues_capped(&conn, &status_f, &session_f, archived, limit, Prose::Full).unwrap();
            let key =
                |r: &IssueRow| (r.id.clone(), r.desc.clone(), r.tags.clone(), r.log.clone());
            assert_eq!(
                single.iter().map(key).collect::<Vec<_>>(),
                fused.iter().map(key).collect::<Vec<_>>(),
                "rows diverged for {status_f:?}/{session_f:?}/{archived:?}/limit={limit}"
            );
            assert_eq!((st, sk), (ft, fk), "cap counts diverged for limit={limit}");
            if st > sk {
                cap_engaged_somewhere = true;
            }
        }
        // The equivalence means nothing if no case ever engaged the cap.
        assert!(cap_engaged_somewhere, "fixture too small: the terminal cap never engaged");

        // AMUX-3503: the QUOTA two-pass must equal quota-over-single-pass the
        // same way. done_limit=2 engages the done/discarded quota (fixture
        // holds ~11 such rows) while verified rides its 300-floor untrimmed —
        // both branches exercised, asserted non-vacuously below.
        let single_q = sse_terminal_quota(
            list_issues(&conn, &[], &[], ArchivedFilter::All).unwrap(),
            2,
        );
        let fused_q = list_issues_quota(&conn, &[], &[], ArchivedFilter::All, 2, Prose::Full).unwrap();
        let key = |r: &IssueRow| (r.id.clone(), r.desc.clone(), r.tags.clone(), r.log.clone());
        assert_eq!(
            single_q.iter().map(key).collect::<Vec<_>>(),
            fused_q.iter().map(key).collect::<Vec<_>>(),
            "quota rows diverged between single-pass and two-pass"
        );
        let done_kept =
            fused_q.iter().filter(|r| matches!(r.status.as_str(), "done" | "discarded")).count();
        let verified_kept = fused_q.iter().filter(|r| r.status == "verified").count();
        assert_eq!(done_kept, 2, "the done/discarded quota must have engaged");
        assert!(verified_kept > 2, "verified must ride its own floor, not the done quota");
        // Nor if the deleted row leaked into either path.
        let (all, _, _) =
            list_issues_capped(&conn, &[], &[], ArchivedFilter::All, 0, Prose::Full).unwrap();
        assert!(all.iter().all(|r| r.id != "C-39"), "deleted row must stay invisible");
        assert!(!all.is_empty());
    }

    /// CONTROL: the helpers must not be matching everything. An unrelated tag is
    /// neither an ask nor collateral damage when the ask is cleared.
    #[test]
    fn needs_you_helpers_leave_unrelated_tags_alone() {
        let conn = tag_db();
        conn.execute("INSERT INTO issue_tags VALUES ('C-1','needs:review',100.0)", []).unwrap();
        assert!(!has_needs_you_tag(&conn, "C-1").unwrap(), "needs:review is not an ask");
        assert!(add_needs_you_tag(&conn, "C-1", 200).unwrap(), "the first ask must be stamped");
        assert_eq!(clear_needs_you_tags(&conn, "C-1").unwrap(), 1, "only the ask goes");
        let left: String =
            conn.query_row("SELECT tag FROM issue_tags WHERE issue_id='C-1'", [], |r| r.get(0))
                .unwrap();
        assert_eq!(left, "needs:review", "clearing the ask must not take other tags with it");
    }

    #[test]
    fn a_stale_cycle_elsewhere_does_not_block_an_unrelated_edge() {
        // AC-335. GE-473 <-> MHC-256 is a real cycle between two CLOSED cards
        // owned by other lanes. Adding AC-331 -> AC-330, which shares no node
        // with it, was refused board-wide as "circular depends_on: GE-473 ->
        // MHC-256". New edges originate only at self_id, so a cycle without
        // self_id cannot be this caller's doing.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE issues (id TEXT PRIMARY KEY, depends_on TEXT, deleted INT);
             INSERT INTO issues VALUES ('GE-473',  '[\"MHC-256\"]', NULL);
             INSERT INTO issues VALUES ('MHC-256', '[\"GE-473\"]',  NULL);
             INSERT INTO issues VALUES ('AC-331',  '',               NULL);
             INSERT INTO issues VALUES ('AC-330',  '',               NULL);",
        )
        .unwrap();

        // The unrelated edge must be ALLOWED even though the board has a cycle.
        let unrelated = depends_on_cycle(&conn, "AC-331", &["AC-330".to_string()]).unwrap();
        assert!(
            unrelated.is_none(),
            "a stale cycle between two other cards blocked an unrelated edge: {unrelated:?}"
        );

        // CONTROL: a genuinely circular edge MUST still be refused, or this fix
        // would have removed the protection instead of scoping it.
        let real = depends_on_cycle(&conn, "AC-330", &["AC-331".to_string()]).unwrap();
        assert!(
            real.is_none(),
            "AC-330 -> AC-331 is not yet a cycle (AC-331 has no deps stored)"
        );
        conn.execute("UPDATE issues SET depends_on='[\"AC-330\"]' WHERE id='AC-331'", [])
            .unwrap();
        let real = depends_on_cycle(&conn, "AC-330", &["AC-331".to_string()]).unwrap();
        assert!(
            real.is_some(),
            "a real cycle through self_id must still be refused"
        );
    }

    #[test]
    fn sse_terminal_quota_gives_verified_its_own_floor() {
        // 400 verified + 150 done + 5 doing: Python keeps ALL doing, the 300
        // newest verified, and the 100 newest done — the lumped 100-cap
        // showed 9 of a 141-card bulk-verify while Python showed all of it.
        let mk = |i: i64, status: &str| IssueRow {
            desc_prefixed: None,
            id: format!("T-{i}"),
            title: String::new(),
            desc: String::new(),
            status: status.into(),
            session: None,
            creator: String::new(),
            due: None,
            created: i,
            updated: i,
            owner_type: "human".into(),
            due_time: None,
            pinned: 0,
            gcal_event_id: None,
            pos: 0.0,
            notified: 0,
            gate: None,
            shepherd: None,
            item_type: "code".into(),
            archived: 0,
            depends_on: vec![],
            reviewer: None,
            epic: None,
            log: None,
            rev: 0,
            source_ref: None,
            evidence: None,
            ask_type: None,
            ask_question: None,
            ask_unblocks: None,
            ask_actor: None,
            entered_state_at: None,
            blocked_on: None,
            next_action: None,
            last_result: None,
            unresolved: None,
            last_verified_at: None,
            closed_at: None,
            version: 0,
            tags: vec![],
            source: None,
            acceptance_criteria: None, decision_question: None,
            decision_rationale: None, decision_supersedes: None,
            waiting_on: None,
            requested_by: None,
            callback_session: None,
            callback_prompt: None,
            callback_state: None,
            callback_message_id: None,
            callback_fired_at: None,
            callback_error: None,
        };
        let mut items: Vec<IssueRow> = Vec::new();
        for i in 0..400 {
            items.push(mk(i, "verified"));
        }
        for i in 400..550 {
            items.push(mk(i, "done"));
        }
        for i in 550..555 {
            items.push(mk(i, "doing"));
        }
        let kept = sse_terminal_quota(items, 100);
        let count = |s: &str| kept.iter().filter(|r| r.status == s).count();
        assert_eq!(count("verified"), 300);
        assert_eq!(count("done"), 100);
        assert_eq!(count("doing"), 5);
        // The newest survive: verified 399 kept, verified 0 evicted.
        assert!(kept.iter().any(|r| r.id == "T-399"));
        assert!(!kept.iter().any(|r| r.id == "T-0"));
    }

    #[test]
    fn internal_id_is_deterministic_and_distinct() {
        assert_eq!(internal_id("AMUX-1"), internal_id("AMUX-1"));
        assert_ne!(internal_id("AMUX-1"), internal_id("AMUX-2"));
        assert!(internal_id("AMUX-1").as_str().starts_with("tsk_"));
    }

    #[test]
    fn status_spellings_round_trip_the_python_vocabulary() {
        // Both spellings parse; the DB default spelling is the Python one.
        assert_eq!(parse_status("needsyou"), Some(TaskStatus::NeedsYou));
        assert_eq!(parse_status("needs_you"), Some(TaskStatus::NeedsYou));
        assert_eq!(db_status_spelling(TaskStatus::NeedsYou), "needsyou");
        // Writing back the status a row already has preserves ITS spelling.
        assert_eq!(status_to_db(TaskStatus::NeedsYou, "needs_you"), "needs_you");
        assert_eq!(status_to_db(TaskStatus::NeedsYou, "todo"), "needsyou");
        // Python _STATUS_ALIASES.
        assert_eq!(parse_status("in_review"), Some(TaskStatus::Review));
        assert_eq!(parse_status("resolved"), Some(TaskStatus::Done));
        assert_eq!(parse_status("wip"), Some(TaskStatus::Doing));
        assert_eq!(parse_status("someday"), None);
    }

    #[test]
    fn prefix_derivation_matches_python() {
        assert_eq!(prefix_from_session("my-project"), "MP");
        assert_eq!(prefix_from_session("orch"), "ORCH");
        assert_eq!(prefix_from_session("amux-cloud"), "AC");
        assert_eq!(prefix_from_session(""), "AMUX");
        assert_eq!(prefix_from_session("---"), "AMUX");
        assert_eq!(prefix_from_session("general-canvas-apps"), "GCA");
    }

    #[test]
    fn append_log_matches_python_format() {
        assert_eq!(append_log(None, "12:01", "x -> y"), "`12:01` x -> y");
        assert_eq!(
            append_log(Some("`09:00` created\n"), "12:01", "a: todo -> doing"),
            "`09:00` created\n`12:01` a: todo -> doing"
        );
    }

    #[test]
    fn gate_table_matches_python() {
        assert_eq!(
            default_gates_for("code", TaskStatus::Done),
            vec!["Implemented and merged", "Tests / lint pass"]
        );
        assert_eq!(
            default_gates_for("escalation", TaskStatus::Done),
            vec!["Outcome recorded in the item (what happened, and why it is closed)"]
        );
        // Unknown/legacy types inherit the strictest (code) gate.
        //
        // The example was `decision` until AF-323 made it a real type. Swapping
        // it keeps the assertion — an unlisted type still gets the code gate —
        // and drops the claim it had quietly acquired, that decision cards
        // BELONG at that bar. Five live cards did, and none of their owners
        // could close one honestly, because "Implemented and merged" is not a
        // sentence anyone can say truthfully about a choice Ethan made.
        assert_eq!(
            default_gates_for("task", TaskStatus::Done),
            default_gates_for("code", TaskStatus::Done)
        );
        // And the type that came out of that fall-through now has its own bar,
        // which names the decider — the field whose absence lets a settled
        // question get re-asked.
        assert_eq!(
            default_gates_for("decision", TaskStatus::Done),
            vec!["The decision is recorded on the card: what was chosen, by whom, and when"]
        );
        assert_eq!(
            default_gates_for("watch", TaskStatus::Review),
            vec!["Fired: evidence of the triggering event recorded"]
        );
        // Ungated statuses stay ungated.
        assert!(default_gates_for("code", TaskStatus::Todo).is_empty());
    }
}

#[cfg(test)]
mod configured_gate_tests {
    use super::*;

    fn row(item_type: &str, gate: Option<&str>) -> IssueRow {
        IssueRow {
            desc_prefixed: None,
            id: "T-1".into(), title: String::new(), desc: String::new(),
            status: "doing".into(), session: None, creator: String::new(),
            due: None, created: 0, updated: 0, owner_type: "agent".into(),
            due_time: None, pinned: 0, gcal_event_id: None, pos: 0.0, notified: 0,
            gate: gate.map(String::from), shepherd: None, item_type: item_type.into(),
            archived: 0, depends_on: vec![], reviewer: None, epic: None, log: None, rev: 0,
            source_ref: None, evidence: None, ask_type: None, ask_question: None,
            ask_unblocks: None, ask_actor: None,
            entered_state_at: None,
            blocked_on: None,
            next_action: None,
            last_result: None,
            unresolved: None, last_verified_at: None, closed_at: None,
            version: 0, tags: vec![],
            source: None,
            acceptance_criteria: None, decision_question: None,
            decision_rationale: None, decision_supersedes: None,
            waiting_on: None,
            requested_by: None, callback_session: None, callback_prompt: None,
            callback_state: None, callback_message_id: None,
            callback_fired_at: None, callback_error: None,
        }
    }

    /// `gate_custom` defaults absent; a row written by the seed has no flag.
    fn conn_with(gate: Option<&str>, custom: Option<i64>) -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE statuses (id TEXT PRIMARY KEY, label TEXT, position INTEGER,
             is_builtin INTEGER, gate TEXT, mode TEXT, gate_custom INTEGER);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO statuses (id,label,position,is_builtin,gate,mode,gate_custom)
             VALUES ('done','Done',4,1,?1,'implicit',?2)",
            rusqlite::params![gate, custom],
        )
        .unwrap();
        c
    }

    // TRAP 1, and the reason this is not "prefer statuses.gate when set".
    // The table is TYPE-BLIND and was seeded from the CODE defaults. Honouring a
    // seeded row would put "Implemented and merged / Tests / lint pass" on a doc
    // card — the unsatisfiable gate that made 1,143 of 1,215 cards type `code`.
    #[test]
    fn a_seeded_row_does_not_override_type_aware_defaults() {
        let c = conn_with(
            Some(r#"["Implemented and merged","Tests / lint pass"]"#),
            None, // seeded: no human wrote this
        );
        let doc = effective_gate_configured(&c, &row("doc", None), TaskStatus::Done);
        assert_eq!(
            doc,
            default_gates_for("doc", TaskStatus::Done),
            "a doc card must keep its own gate when the column was never customised"
        );
        assert!(
            !doc.contains(&"Implemented and merged".to_string()),
            "the code gate must not leak onto a doc card: {doc:?}"
        );
    }

    // TRAP 2: "differs from the current default" cannot mean "customised",
    // because the seed DRIFTS — `verified` already diverges in the live DB.
    // Only the explicit flag counts.
    #[test]
    fn a_stale_seed_that_differs_from_the_default_is_still_not_a_customisation() {
        let c = conn_with(Some(r#"["CI/CD green (incl. e2e)"]"#), None);
        let got = effective_gate_configured(&c, &row("code", None), TaskStatus::Done);
        assert_eq!(got, default_gates_for("code", TaskStatus::Done));
    }

    #[test]
    fn an_operator_authored_gate_is_honoured() {
        let c = conn_with(Some(r#"["Signed off by Ethan","Screenshot attached"]"#), Some(1));
        let got = effective_gate_configured(&c, &row("code", None), TaskStatus::Done);
        assert_eq!(got, vec!["Signed off by Ethan", "Screenshot attached"]);
    }

    /// A card's own override is MORE specific than a column default and keeps
    /// winning — otherwise configuring a column would silently retype every
    /// deliberately-special card on the board.
    #[test]
    fn a_card_override_still_beats_a_configured_column() {
        let c = conn_with(Some(r#"["Column rule"]"#), Some(1));
        let got = effective_gate_configured(
            &c,
            &row("code", Some(r#"["This card only"]"#)),
            TaskStatus::Done,
        );
        assert_eq!(got, vec!["This card only"]);
    }

    // ---- scoped gates: worker > group > global (RR-0051, 2026-08-11) ------

    fn add_session_gates(c: &rusqlite::Connection) {
        c.execute_batch(
            "CREATE TABLE session_gates (session TEXT NOT NULL, status TEXT NOT NULL,
             gate TEXT, PRIMARY KEY (session, status));",
        )
        .unwrap();
    }

    fn scope_gate(c: &rusqlite::Connection, scope: &str, status: &str, gate: &str) {
        c.execute(
            "INSERT INTO session_gates (session, status, gate) VALUES (?1, ?2, ?3)",
            rusqlite::params![scope, status, gate],
        )
        .unwrap();
    }

    fn row_for(session: &str, item_type: &str, gate: Option<&str>) -> IssueRow {
        let mut r = row(item_type, gate);
        r.session = Some(session.into());
        r
    }

    fn groups(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// AMUX-3607. The trail must record every tier CONSULTED, not only the
    /// winner, and must separate a tier that HELD a rule and lost from one that
    /// held nothing.
    ///
    /// That distinction is the whole feature. Under the old early-returning
    /// walk `outranked` was structurally unobservable: when the card override
    /// won, nothing ever asked the worker layer, so "a worker gate existed and
    /// was overridden" and "no worker gate exists" were the same silence. A
    /// trail that only names the winner answers "what applied" and cannot
    /// answer "why was this allowed", which is the question Ethan's directive
    /// actually asks.
    ///
    /// One fixture, every verdict, because the claim is that the four are TOLD
    /// APART. Asserting `applied` alone would pass against a trail that labels
    /// every other tier identically, which is the version worth catching.
    #[test]
    fn the_trail_says_which_layers_lost_and_which_had_nothing_to_say() {
        let c = conn_with(None, None);
        add_session_gates(&c);
        // A worker gate AND a group gate both exist and both LOSE to the card
        // override. Under the old walk neither was ever read.
        scope_gate(&c, "backend", "done", r#"["worker rule"]"#);
        scope_gate(&c, "group:ops", "done", r#"["group rule"]"#);
        let row = row_for("backend", "code", Some(r#"["card rule"]"#));

        let t = effective_gate_trail(&c, &row, TaskStatus::Done, &groups(&["ops"]));
        assert_eq!(t.criteria, vec!["card rule"], "the winner must not change");
        assert_eq!(t.source, GateSource::Card);

        let by = |n: &str| t.layers.iter().find(|l| l.layer == n).expect("every tier is present");
        assert_eq!(t.layers.len(), 5, "all five tiers, always: {:?}", t.layers);

        assert_eq!(by("card").verdict, "applied");
        assert_eq!(by("card").criteria, vec!["card rule"]);

        // THE POINT. Both held a real rule and lost, and the rule they held is
        // reported — a rejected rule is the content of the answer to "why not
        // something else", not context for it.
        assert_eq!(by("worker").verdict, "outranked");
        assert_eq!(by("worker").criteria, vec!["worker rule"]);
        assert_eq!(by("worker").scope.as_deref(), Some("backend"), "name the scope so it can be re-read");
        assert_eq!(by("group").verdict, "outranked");
        assert_eq!(by("group").criteria, vec!["group rule"]);
        assert_eq!(by("group").scope.as_deref(), Some("ops"));

        // Consulted, held nothing: could never have applied. Different fact,
        // different word.
        assert_eq!(by("column").verdict, "silent");
        assert!(by("column").criteria.is_empty());

        // The type default always holds something, so it is outranked here
        // rather than silent.
        assert_eq!(by("type_default").verdict, "outranked");
        assert!(!by("type_default").criteria.is_empty());

        // A SESSIONLESS card never had a worker or group tier to ask. Calling
        // that `silent` would report an empty answer from a scope nobody
        // queried, which is the same over-claim one layer along.
        let mut orphan = row_for("backend", "code", None);
        orphan.session = None;
        let t2 = effective_gate_trail(&c, &orphan, TaskStatus::Done, &groups(&["ops"]));
        let by2 = |n: &str| t2.layers.iter().find(|l| l.layer == n).unwrap();
        assert_eq!(by2("worker").verdict, "not_applicable");
        assert_eq!(by2("group").verdict, "not_applicable");
        assert_eq!(by2("type_default").verdict, "applied", "with no scope the type default wins");
    }

    /// The one-line audit form. Asserted as a WHOLE STRING rather than by
    /// substring: this line is the permanent authorisation record on the card,
    /// and a substring check passes against a version that silently drops a
    /// tier, which is the one failure that matters here.
    #[test]
    fn the_audit_line_names_every_tier_and_its_verdict() {
        let c = conn_with(None, None);
        add_session_gates(&c);
        scope_gate(&c, "backend", "done", r#"["worker rule"]"#);
        scope_gate(&c, "group:ops", "done", r#"["g1","g2"]"#);
        let row = row_for("backend", "code", Some(r#"["card rule"]"#));
        let t = effective_gate_trail(&c, &row, TaskStatus::Done, &groups(&["ops"]));
        assert_eq!(
            t.log_line(),
            "authz: card=applied(1) worker:backend=outranked(1) group:ops=outranked(2) \
column=silent type:code=outranked(2)"
        );

        // The permissive case must still produce a line. "Nothing required this,
        // at any tier" is an authorisation answer; a trail that only appeared
        // when something blocked would make the permissive case the invisible
        // one, which is backwards for an audit record.
        let plain = row_for("nobody", "chore", None);
        let t2 = effective_gate_trail(&c, &plain, TaskStatus::Backlog, &groups(&[]));
        let line = t2.log_line();
        assert!(line.starts_with("authz: "), "{line}");
        assert_eq!(line.matches('=').count(), 5, "all five tiers, always: {line}");
        assert!(line.contains("card=silent"), "{line}");
    }

    /// `effective_gate_with_source` is a PROJECTION of the trail, not a second
    /// walk. Pinned because two spellings of a precedence is exactly the
    /// duplication this file warns about elsewhere, and the failure mode is
    /// silent: they agree until one is edited.
    #[test]
    fn the_summary_and_the_trail_cannot_disagree_about_the_winner() {
        let c = conn_with(None, None);
        add_session_gates(&c);
        scope_gate(&c, "backend", "done", r#"["worker rule"]"#);
        scope_gate(&c, "group:ops", "verified", r#"["group rule"]"#);
        for (row, target) in [
            (row_for("backend", "code", Some(r#"["card"]"#)), TaskStatus::Done),
            (row_for("backend", "code", None), TaskStatus::Done),
            (row_for("backend", "code", None), TaskStatus::Verified),
            (row_for("backend", "investigation", None), TaskStatus::Review),
        ] {
            let t = effective_gate_trail(&c, &row, target, &groups(&["ops"]));
            let (crit, src) = effective_gate_with_source(&c, &row, target, &groups(&["ops"]));
            assert_eq!((crit, src), (t.criteria.clone(), t.source.clone()), "{target:?}");
            // And the applied layer must be the one the source names.
            let applied: Vec<&str> =
                t.layers.iter().filter(|l| l.verdict == "applied").map(|l| l.layer).collect();
            assert_eq!(applied.len(), 1, "exactly one tier applies: {applied:?}");
        }
    }

    /// AMUX-3567 REVIEW (amux-frustrations): the SOURCE at every rung.
    ///
    /// The gate VALUES were covered rung by rung; the tier that produced them
    /// was not, anywhere. Measured by mutation on the shipped tree: making
    /// `retype_would_help` return true for `Worker` AND `Group` left the entire
    /// `-p amux-server` suite green (1264 passed, the one failure an unrelated
    /// env-dependent alerts cell). So the advice "retyping will not change it"
    /// could invert for the two tiers the whole feature exists to surface and
    /// nothing would go red.
    ///
    /// That matters because the WRONG answer here is the incident: AF-168's
    /// reporter retyped TUBES-2053 on a worker-scoped gate, watched it not
    /// re-derive, and concluded the override was pinned per-card. Worker and
    /// Group are exactly the rungs that were never asserted.
    ///
    /// One test for the whole ladder rather than five, because the property is
    /// a mapping and the interesting failure is two rungs agreeing when they
    /// should differ.
    #[test]
    fn the_gate_source_names_the_tier_that_won_at_every_rung() {
        // TYPE DEFAULT — nothing configured anywhere.
        let c = conn_with(None, None);
        add_session_gates(&c);
        let (_, src) = effective_gate_with_source(
            &c, &row_for("backend", "code", None), TaskStatus::Done, &groups(&[]));
        assert_eq!(src, GateSource::TypeDefault);
        assert!(src.retype_would_help(), "the type default is the ONE rung retyping moves");
        assert!(src.explain().contains("TYPE"), "{}", src.explain());

        // COLUMN — an operator-authored gate on the status itself.
        let c = conn_with(Some(r#"["Global column rule"]"#), Some(1));
        add_session_gates(&c);
        let (g, src) = effective_gate_with_source(
            &c, &row_for("backend", "code", None), TaskStatus::Done, &groups(&[]));
        assert_eq!(g, vec!["Global column rule"]);
        assert_eq!(src, GateSource::Column);
        assert!(!src.retype_would_help(), "retyping cannot clear a column gate");

        // GROUP — the worker has none, a group does. This rung and the next are
        // the ones the mutation proved uncovered.
        let c = conn_with(None, None);
        add_session_gates(&c);
        scope_gate(&c, "group:ops", "done", r#"["Group rule"]"#);
        let (g, src) = effective_gate_with_source(
            &c, &row_for("backend", "code", None), TaskStatus::Done, &groups(&["ops"]));
        assert_eq!(g, vec!["Group rule"]);
        assert_eq!(src, GateSource::Group("ops".into()));
        assert!(!src.retype_would_help(), "a GROUP gate ignores the item type: {}", src.explain());
        assert!(src.explain().contains("GROUP scope"), "{}", src.explain());
        assert!(src.explain().contains("session-gates"), "point at the endpoint that answers it: {}", src.explain());

        // WORKER — beats the group, and names the worker so the reader can go
        // look. This is TUBES-2053's shape exactly.
        scope_gate(&c, "backend", "done", r#"["Worker rule"]"#);
        let (g, src) = effective_gate_with_source(
            &c, &row_for("backend", "code", None), TaskStatus::Done, &groups(&["ops"]));
        assert_eq!(g, vec!["Worker rule"]);
        assert_eq!(src, GateSource::Worker("backend".into()));
        assert!(!src.retype_would_help(), "a WORKER gate ignores the item type: {}", src.explain());
        assert!(src.explain().contains("`backend`"), "name the worker, or the reader cannot go look: {}", src.explain());

        // CARD — beats everything above it.
        let (g, src) = effective_gate_with_source(
            &c,
            &row_for("backend", "code", Some(r#"["Only this card"]"#)),
            TaskStatus::Done,
            &groups(&["ops"]),
        );
        assert_eq!(g, vec!["Only this card"]);
        assert_eq!(src, GateSource::Card);
        assert!(!src.retype_would_help());

        // AND THE DISCRIMINATOR THE MUTATION EXPOSED: exactly one rung says yes.
        // Asserted as a set so a future rung cannot be added silently on the
        // wrong side of it.
        let yes = [
            GateSource::Card,
            GateSource::Worker("w".into()),
            GateSource::Group("g".into()),
            GateSource::Column,
            GateSource::TypeDefault,
        ]
        .iter()
        .filter(|s| s.retype_would_help())
        .count();
        assert_eq!(yes, 1, "retyping moves the type default and nothing else");
    }

    /// The whole ladder in one specimen: worker, group and global all
    /// configured, worker wins ("worker takes priority over all").
    #[test]
    fn a_worker_gate_beats_group_and_global() {
        let c = conn_with(Some(r#"["Global column rule"]"#), Some(1));
        add_session_gates(&c);
        scope_gate(&c, "backend", "done", r#"["Worker rule"]"#);
        scope_gate(&c, "group:ops", "done", r#"["Group rule"]"#);
        let got = effective_gate_scoped(
            &c,
            &row_for("backend", "code", None),
            TaskStatus::Done,
            &groups(&["ops"]),
        );
        assert_eq!(got, vec!["Worker rule"]);
    }

    #[test]
    fn a_group_gate_applies_when_the_worker_has_none_and_beats_global() {
        let c = conn_with(Some(r#"["Global column rule"]"#), Some(1));
        add_session_gates(&c);
        scope_gate(&c, "group:ops", "done", r#"["Group rule"]"#);
        let got = effective_gate_scoped(
            &c,
            &row_for("backend", "code", None),
            TaskStatus::Done,
            &groups(&["ops"]),
        );
        assert_eq!(got, vec!["Group rule"]);
    }

    /// A worker in several groups answers to ALL of them: union, in sorted
    /// group order, deduplicated — deterministic however the fleet is tagged.
    #[test]
    fn multiple_groups_union_deterministically() {
        let c = conn_with(None, None);
        add_session_gates(&c);
        scope_gate(&c, "group:ops", "done", r#"["Shared rule","Ops rule"]"#);
        scope_gate(&c, "group:gtm", "done", r#"["Gtm rule","Shared rule"]"#);
        let got = effective_gate_scoped(
            &c,
            &row_for("backend", "code", None),
            TaskStatus::Done,
            &groups(&["ops", "gtm"]),
        );
        // BTreeSet iterates sorted: gtm's list first, then ops', dedup on merge.
        assert_eq!(got, vec!["Gtm rule", "Shared rule", "Ops rule"]);
    }

    /// The card's own override is still the most specific thing on the board.
    #[test]
    fn a_card_override_beats_every_scoped_tier() {
        let c = conn_with(None, None);
        add_session_gates(&c);
        scope_gate(&c, "backend", "done", r#"["Worker rule"]"#);
        let got = effective_gate_scoped(
            &c,
            &row_for("backend", "code", Some(r#"["This card only"]"#)),
            TaskStatus::Done,
            &groups(&[]),
        );
        assert_eq!(got, vec!["This card only"]);
    }

    /// UNHAPPY: an empty or malformed scoped row INHERITS the next tier, it
    /// never opens the gate — same fail-closed rule as the column gate. A
    /// worker row of `[]` therefore does not exempt the worker from its
    /// group's bar.
    #[test]
    fn a_malformed_or_empty_scoped_row_inherits_not_opens() {
        for bad in ["not json", "[]", r#"["","  "]"#] {
            let c = conn_with(None, None);
            add_session_gates(&c);
            scope_gate(&c, "backend", "done", bad);
            scope_gate(&c, "group:ops", "done", r#"["Group rule"]"#);
            let got = effective_gate_scoped(
                &c,
                &row_for("backend", "code", None),
                TaskStatus::Done,
                &groups(&["ops"]),
            );
            assert_eq!(got, vec!["Group rule"], "worker row {bad:?} must inherit the group tier");
        }
    }

    /// UNHAPPY: a DB without the session_gates table (predates AMUX-2599)
    /// must neither panic nor open the gate — the ladder continues below.
    #[test]
    fn a_missing_session_gates_table_inherits_the_global_tier() {
        let c = conn_with(Some(r#"["Global column rule"]"#), Some(1));
        let got = effective_gate_scoped(
            &c,
            &row_for("backend", "code", None),
            TaskStatus::Done,
            &groups(&["ops"]),
        );
        assert_eq!(got, vec!["Global column rule"]);
    }

    /// A sessionless card (no owner lane) skips the scoped tiers entirely —
    /// there is no worker or group to scope to.
    #[test]
    fn a_sessionless_card_uses_the_global_ladder() {
        let c = conn_with(None, None);
        add_session_gates(&c);
        scope_gate(&c, "group:ops", "done", r#"["Group rule"]"#);
        let got = effective_gate_scoped(&c, &row("code", None), TaskStatus::Done, &groups(&["ops"]));
        assert_eq!(got, default_gates_for("code", TaskStatus::Done));
    }

    /// Every "cannot tell" answer must fall back to the defaults, NEVER to an
    /// empty gate — an empty gate means NO gate, so a malformed row would
    /// silently open the strictest transitions on the board.
    #[test]
    fn malformed_or_empty_configuration_never_reads_as_permission() {
        for bad in [Some("not json"), Some("[]"), Some(r#"["","  "]"#), None] {
            let c = conn_with(bad, Some(1));
            let got = effective_gate_configured(&c, &row("code", None), TaskStatus::Done);
            assert_eq!(
                got,
                default_gates_for("code", TaskStatus::Done),
                "input {bad:?} must fall back to defaults, not open the gate"
            );
            assert!(!got.is_empty(), "input {bad:?} produced an EMPTY gate");
        }
    }

    /// A missing `statuses` table (a DB that predates the column editor) must
    /// not panic or open the gate.
    #[test]
    fn a_db_without_the_statuses_table_falls_back_quietly() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        let got = effective_gate_configured(&c, &row("code", None), TaskStatus::Done);
        assert_eq!(got, default_gates_for("code", TaskStatus::Done));
    }
}
