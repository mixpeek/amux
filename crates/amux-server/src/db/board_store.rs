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
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::HashMap;

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
pub const KNOWN_TYPES: [&str; 11] = [
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

/// True when `text` contains at least one pointer to a produced artifact: an
/// http(s) URL, a markdown link, a repo-relative file path (`a/b.ext`), a
/// commit-sha-shaped token (7..=40 hex as a whole word), or a `#<number>`
/// PR/issue reference. Deliberately generous on ACCEPT (a false accept only
/// lets an honest-looking card through; a false reject would block real work),
/// but it CAN fail: a done card that is pure prose with no artifact reference
/// has none of these, which is exactly the case this gate exists to stop.
pub fn has_asset_link(text: &str) -> bool {
    if text.contains("http://") || text.contains("https://") || text.contains("](") {
        return true;
    }
    // `#123` PR/issue reference.
    let b = text.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'#' && b.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    for raw in text.split_whitespace() {
        // Keep path/word chars; drop surrounding prose punctuation.
        let tok = raw.trim_matches(|c: char| {
            !c.is_ascii_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-'
        });
        // Repo-relative path: has a '/', last segment carries a short alnum ext.
        if let Some((dir, last)) = tok.rsplit_once('/') {
            if !dir.is_empty() {
                if let Some((stem, ext)) = last.rsplit_once('.') {
                    if !stem.is_empty()
                        && (1..=8).contains(&ext.len())
                        && ext.chars().all(|c| c.is_ascii_alphanumeric())
                    {
                        return true;
                    }
                }
            }
        }
        // Commit sha: the whole token is 7..=40 hex digits.
        let hex = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if (7..=40).contains(&hex.len()) && hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return true;
        }
    }
    false
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
    pub last_verified_at: Option<i64>,
    /// Rust per-row version (migration 0002). Bumped alongside `rev`.
    pub version: i64,
    pub tags: Vec<String>,
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
     i.epic, i.closed_at, GROUP_CONCAT(t.tag)";

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
        created: r.get(7)?,
        updated: r.get(8)?,
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
) -> rusqlite::Result<(Vec<IssueRow>, usize, usize)> {
    let light = light_rows(conn, status_filter, session_filter, archived)?;
    let (kept_light, term_total, term_kept) =
        cap_terminal_by(light, done_limit, |r| &r.status, |r| r.updated);
    Ok((hydrate_light(conn, &kept_light)?, term_total, term_kept))
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
) -> rusqlite::Result<Vec<IssueRow>> {
    let light = light_rows(conn, status_filter, session_filter, archived)?;
    let kept_light = terminal_quota_by(light, done_limit, |r| &r.status, |r| r.updated);
    hydrate_light(conn, &kept_light)
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
            updated: r.get(6)?,
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

/// Pass 2: hydrate survivors only, preserving pass-1 order. Chunked well
/// under SQLITE_MAX_VARIABLE_NUMBER's historical floor of 999.
fn hydrate_light(conn: &Connection, kept_light: &[LightRow]) -> rusqlite::Result<Vec<IssueRow>> {
    let mut by_id: std::collections::HashMap<String, IssueRow> = std::collections::HashMap::new();
    for chunk in kept_light.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM issues i LEFT JOIN issue_tags t ON t.issue_id = i.id \
             WHERE i.deleted IS NULL AND i.id IN ({marks}) GROUP BY i.id"
        ))?;
        let params: Vec<&dyn rusqlite::types::ToSql> =
            chunk.iter().map(|r| &r.id as &dyn rusqlite::types::ToSql).collect();
        for row in stmt.query_map(params.as_slice(), issue_from_row)? {
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
             notified, pinned, archived, rev, version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             0, 0, 0, 0, 0)",
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
    conn.execute(
        "UPDATE issues SET title = ?1, \"desc\" = ?2, status = ?3, session = ?4, due = ?5, \
             due_time = ?6, owner_type = ?7, pinned = ?8, pos = ?9, gate = ?10, shepherd = ?11, \
             type = ?12, archived = ?13, depends_on = ?14, reviewer = ?15, log = ?16, \
             rev = ?17, version = ?18, updated = ?19, source_ref = ?20, last_verified_at = ?21, \
             epic = ?22, closed_at = ?23 \
         WHERE id = ?24 AND deleted IS NULL",
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
            row.id,
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
        let conn = Connection::open_in_memory().expect("memdb");
        conn.execute_batch(
            "CREATE TABLE issues (
                id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', desc TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'todo', session TEXT, creator TEXT NOT NULL DEFAULT '',
                due TEXT, created INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL DEFAULT 0,
                owner_type TEXT NOT NULL DEFAULT 'agent', due_time TEXT, pinned INTEGER DEFAULT 0,
                gcal_event_id TEXT, pos REAL DEFAULT 0, notified INTEGER DEFAULT 0, gate TEXT,
                shepherd TEXT, type TEXT NOT NULL DEFAULT 'code', archived INTEGER DEFAULT 0,
                depends_on TEXT, reviewer TEXT, log TEXT, rev INTEGER DEFAULT 0,
                source_ref TEXT, last_verified_at INTEGER, version INTEGER DEFAULT 0,
                epic TEXT, closed_at INTEGER, deleted INTEGER);
             CREATE TABLE issue_tags (issue_id TEXT, tag TEXT, added_at REAL,
                PRIMARY KEY (issue_id, tag));",
        )
        .expect("schema");

        // SQLite is dynamically typed: binding an i64 stores INTEGER, binding a
        // &str stores TEXT, in the same column. That is how the legacy rows
        // exist at all, and it is what lets one table hold both here.
        for (id, bind) in [
            ("INT-1", &1787840686i64 as &dyn rusqlite::ToSql),
            ("TXT-1", &"1787840686" as &dyn rusqlite::ToSql),
            ("TXT-2", &" 1787840686 " as &dyn rusqlite::ToSql), // whitespace, trimmed
        ] {
            conn.execute(
                "INSERT INTO issues (id, last_verified_at) VALUES (?1, ?2)",
                params![id, bind],
            )
            .expect("insert");
        }
        // Genuine absence, and the unreadable case that must not be mistaken for it.
        conn.execute("INSERT INTO issues (id) VALUES ('NUL-1')", []).expect("insert");
        conn.execute(
            "INSERT INTO issues (id, last_verified_at) VALUES ('BAD-1', 'yesterday')",
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
        assert_eq!(at("INT-1"), Some(1787840686), "the normal INTEGER case");
        assert_eq!(at("TXT-1"), Some(1787840686), "legacy TEXT must not crash or drop");
        assert_eq!(at("TXT-2"), Some(1787840686), "legacy TEXT is trimmed");
        assert_eq!(at("NUL-1"), None, "NULL is genuine absence");
        assert_eq!(at("BAD-1"), None, "unreadable text degrades to None (and warns)");
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
        assert!(has_asset_link("shipped as 53a868f"));
        assert!(has_asset_link("closes #106"));
        // A short hex-ish word is not a sha, a bare year is too short.
        assert!(!has_asset_link("the cafe was open in 2026"));
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
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE issues (
                id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', desc TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'todo', session TEXT, creator TEXT NOT NULL DEFAULT '',
                due TEXT, created INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL DEFAULT 0,
                owner_type TEXT NOT NULL DEFAULT 'agent', due_time TEXT, pinned INTEGER DEFAULT 0,
                gcal_event_id TEXT, pos REAL DEFAULT 0, notified INTEGER DEFAULT 0, gate TEXT,
                shepherd TEXT, type TEXT NOT NULL DEFAULT 'code', archived INTEGER DEFAULT 0,
                depends_on TEXT, reviewer TEXT, log TEXT, rev INTEGER DEFAULT 0,
                source_ref TEXT, last_verified_at INTEGER, version INTEGER DEFAULT 0,
                epic TEXT, closed_at INTEGER, deleted INTEGER);
             CREATE TABLE issue_tags (issue_id TEXT, tag TEXT, added_at REAL,
                PRIMARY KEY (issue_id, tag));
             CREATE TABLE issue_counters (prefix TEXT PRIMARY KEY, next_n INTEGER NOT NULL);",
        )
        .unwrap();
        conn
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
                "INSERT INTO issues (id, title, desc, status, session, updated, pos, pinned, \
                                     archived, log, deleted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
                list_issues_capped(&conn, &status_f, &session_f, archived, limit).unwrap();
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
        let fused_q = list_issues_quota(&conn, &[], &[], ArchivedFilter::All, 2).unwrap();
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
            list_issues_capped(&conn, &[], &[], ArchivedFilter::All, 0).unwrap();
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
            last_verified_at: None,
            closed_at: None,
            version: 0,
            tags: vec![],
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
        assert_eq!(
            default_gates_for("decision", TaskStatus::Done),
            default_gates_for("code", TaskStatus::Done)
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
            id: "T-1".into(), title: String::new(), desc: String::new(),
            status: "doing".into(), session: None, creator: String::new(),
            due: None, created: 0, updated: 0, owner_type: "agent".into(),
            due_time: None, pinned: 0, gcal_event_id: None, pos: 0.0, notified: 0,
            gate: gate.map(String::from), shepherd: None, item_type: item_type.into(),
            archived: 0, depends_on: vec![], reviewer: None, epic: None, log: None, rev: 0,
            source_ref: None, last_verified_at: None, closed_at: None, version: 0, tags: vec![],
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
