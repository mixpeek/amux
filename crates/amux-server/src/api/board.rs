//! Board API (RR-0049 routes + 409 gate contract + force audit; RR-0055
//! archive/restore; the list shape RR-0053's auto-capture will write into).
//!
//! Mounted at `/api/board` inside the `protected` router (api/mod.rs). This
//! is the STRANGLER-FIG surface: it serves the same `issues` rows the Python
//! server serves, in the same shapes the Python dashboard/CLI already parse —
//! a bare JSON array from the list, the `gate not acknowledged` 409 body the
//! CLI's `--checked` flow is built around, `X-Amux-Truncated` headers on the
//! capped list. Interop mappings live in `db::board_store`.
//!
//! Every status change routes through core's `apply_transition` — one state
//! machine, one code path (Invariant 3); nothing here hand-rolls a status
//! write. Gate refusals carry core's `WhyBlocked` list alongside the Python
//! keys, force bypasses are audited into the card's own log (ethos rule 6:
//! the Python board claimed force-is-logged while nothing logged it), and
//! no-op PATCHes report `applied: false` with `rev` unmoved (Invariant 37).

use super::AppState;
use crate::db::board_store::{self as bs, ArchivedFilter, IssueRow};
use crate::db::{PendingEvent, WriteOutcome};
use amux_core::board::{
    apply_transition, why_blocked, BoardTransition, TaskStatus, TransitionError,
};
use amux_core::events::Actor;
use amux_core::revision::{EntityType, MutationKind};
use amux_core::verification::{Evidence, EvidenceKind, EvidenceSource};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_board).post(create_item))
        // Static /export outranks /{id}, same as /statuses below.
        .route("/export", get(export_board))
        // Static segment outranks /{id} in axum: /statuses never collides.
        .route("/statuses", get(list_statuses).post(create_status))
        // Static /reorder outranks /{sid}; both outrank /api/board/{id}.
        .route("/statuses/reorder", axum::routing::put(reorder_statuses))
        .route(
            "/statuses/{sid}",
            axum::routing::patch(patch_status).delete(delete_status),
        )
        // Static /session-gates and /contract outrank /{id}.
        .route(
            "/session-gates",
            get(list_session_gates).patch(patch_session_gates),
        )
        .route("/contract", get(get_contract))
        // Static /derived outranks /{id}. Computed display status from durable
        // facts, never stored (AO architecture: display status is derived at
        // read time).
        .route("/derived", get(derived_board))
        // Static /ready outranks /{id}. The read side of the dependency graph
        // (AMUX-3948) — READY is a query, never a stored status.
        .route("/ready", get(ready_frontier))
        // CDC catch-up: lets clients replay missed board mutations after a
        // reconnect, keyed by the seq cursor from board_change_log.
        .route("/changes", get(board_changes))
        // Static /bulk-migrate outranks /{id}. Moving a whole column at once
        // (AMUX-4044) — a single write transaction, because backlog alone
        // holds 489 live cards and 489 sequential PATCHes is minutes of load.
        .route("/bulk-migrate", axum::routing::post(bulk_migrate))
        // Static /needsyou outranks /{id}. The one owner view (AF-318).
        .route("/needsyou", get(needsyou_queue))
        // A semantic-concern claim is deliberately separate from a card claim:
        // two cards may coexist, but two workers must not both complete/deploy
        // the same concern merely because they learned about each other late.
        .route("/overlap", post(reconcile_overlap))
        .route("/overlap/deployment-permit", get(overlap_deployment_permit))
        .route("/overlap/{coordination_id}", get(get_overlap))
        // DELETE was never registered, so the SPA's own Delete button on a
        // card 405'd — and `deleteBoardItem` removes the card optimistically
        // BEFORE the request, so the card vanished, the server kept it, and it
        // came back on the next poll. That is the reported "tons of board
        // items are not moving" (AMUX board sweep, 2026-08-09).
        // Before the /{id} wildcard, or "clear-done" is swallowed as an id.
        .route("/clear-done", post(clear_done))
        .route("/{id}", get(get_item).patch(patch_item).delete(delete_item))
        .route("/{id}/archive", post(archive_item))
        .route("/{id}/restore", post(restore_item))
        // The D1-exit pair — see the handlers below for why their 405 was the
        // most expensive shape available.
        .route("/{id}/status-request", post(status_request))
        .route("/{id}/status-update", post(status_update))
        // AMUX-3131: the claim the assignment notifications tell every session to
        // run. It was never mounted, so `amux board claim <id>` hit the GET-only
        // SPA catch-all (405) and the CLI (pre-fix) exited 0 with the card
        // untouched — AMUX-2140 one layer down. Same mechanism auto-pickup uses.
        .route("/{id}/claim", post(claim_item))
        // One write for the whole model-judged plan: the capture becomes the
        // epic that the originating message already points at, and every child
        // is created with its owner, priority and dependency edges intact.
        .route("/{id}/decompose", post(decompose_item))
        .route("/{id}/capsule", get(capsule))
        .route("/{id}/verifications", get(list_verifications))
        .route("/{id}/artifacts", get(list_artifacts).post(create_artifact))
        .route(
            "/{id}/artifacts/{aid}",
            axum::routing::patch(patch_artifact).delete(delete_artifact),
        )
}

/// GET /api/board/needsyou — THE owner view, capped (AF-318).
///
/// One view, ten rows, and the rest hidden until those clear. A queue no human
/// can drain is the same as no queue: 445 cards at a median age of 15 days is
/// not a backlog anybody triages, it is a place cards go. Ten is a number a
/// person finishes in a sitting, which is the only property that makes the
/// eleventh card reachable at all.
///
/// Ranked by `age_days * blast_radius`, not by age alone. Age alone re-creates
/// the same list in a different order and puts the 58-day card nobody is
/// waiting on above the two-day card three lanes are blocked behind. Blast
/// radius is a count of open cards depending on this one, so the ranking is by
/// what CLEARING it releases — the only importance signal the board actually
/// holds rather than infers.
///
/// `?all=1` returns the hidden remainder, for the sweep that wants the whole
/// backlog. The cap is a default, not a wall: hiding rows with no way to ask
/// for them would be the instrument lying about its own population, which is
/// exactly the AF-320 shape.
async fn needsyou_queue(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let want_all = q.get("all").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let cap: usize = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(NEEDSYOU_VIEW_CAP)
        .clamp(1, 500);
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(crate::api::measured::unmeasured(
                    json!({"error": e.to_string(), "queue": []}),
                    "the store could not be opened, so no needsyou card was read",
                )),
            )
                .into_response()
        }
    };
    let rows = match bs::list_issues(
        &conn,
        &["needsyou".to_string()],
        &[],
        ArchivedFilter::ActiveOnly,
    ) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(crate::api::measured::unmeasured(
                    json!({"error": e.to_string(), "queue": []}),
                    "the needsyou query failed, so an empty queue here is unmeasured",
                )),
            )
                .into_response()
        }
    };
    let now = crate::config::now_f64();
    // One `today` for the whole pass: computing it per row could straddle
    // midnight mid-sort and make two cards disagree about what "due today" is.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut scored: Vec<(bool, f64, Value)> = rows
        .iter()
        .map(|r| {
            let age_days = ((now - r.created as f64) / 86_400.0).max(0.0);
            let radius = bs::blast_radius(&conn, &r.id);
            // radius + 1, so a card nobody depends on still ranks by age
            // instead of scoring zero and sinking below every card forever.
            let score = age_days * (radius + 1) as f64;
            // AF-424: A PASSED DEADLINE OUTRANKS AN OLD CARD NOBODY DATED.
            //
            // Reported by gtm-engine (GE-769) and reproduced here. `due` is a
            // signal the board HOLDS — a lane stated a date on purpose — and
            // this ranking is justified in its own docstring as using "the only
            // importance signal the board actually holds rather than infers".
            // It was not using this one.
            //
            // The bias is structural, not incidental: stating a deadline
            // correlates with being NEW, new means low `age_days`, and the
            // formula rewards age. So dating a card pushed it DOWN. Measured
            // live 2026-09-02: 119 of 404 rows carry a due date, their MEDIAN
            // rank was 329 of 404, and 47 were due-today-or-overdue. AF-344,
            // "scrub 646 leaked API-key occurrences", due that day, ranked 349.
            //
            // NO FREE PARAMETER, deliberately. A weight would make me decide how
            // many days of age a deadline is worth, which is a judgement about
            // someone else's queue. This is a yes/no instead: a card whose
            // stated date has ARRIVED sorts above one whose has not, and within
            // each group the existing score is unchanged. Nothing is reordered
            // except across that one boundary, and turning it off is deleting
            // this key.
            //
            // Dates are compared as ISO strings, which is what the column holds
            // and what `is:overdue` in the dashboard already does.
            let overdue = r
                .due
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .is_some_and(|d| d <= today.as_str());
            let mut v = list_body(r, true, false);
            if let Some(o) = v.as_object_mut() {
                o.insert("age_days".into(), json!((age_days * 10.0).round() / 10.0));
                o.insert("blast_radius".into(), json!(radius));
                o.insert("score".into(), json!((score * 10.0).round() / 10.0));
                // The ask, or the fact that there is not one. A card that
                // predates AF-318 is NULL here, which is a different thing from
                // a card that answered the gate — and the owner triaging this
                // list is exactly who needs to tell them apart.
                o.insert(
                    "has_typed_ask".into(),
                    json!(bs::ask_verdict(
                        r.ask_actor.as_deref().unwrap_or(""),
                        r.ask_type.as_deref().unwrap_or(""),
                        r.ask_question.as_deref().unwrap_or(""),
                        r.ask_unblocks.as_deref().unwrap_or(""),
                    ) == bs::AskVerdict::Ok),
                );
            }
            if let Some(o) = v.as_object_mut() {
                // Say it in the payload, so a reader can see WHY a card is where
                // it is rather than inferring it from the order (rule 4).
                o.insert("overdue".into(), json!(overdue));
            }
            (overdue, score, v)
        })
        .collect();
    // Overdue first, then the existing score within each group. `true > false`,
    // so the natural descending compare puts a passed deadline on top. The
    // stored creation time and id are deterministic tie-breakers; without
    // them cards created within the same second inherited SQLite's incidental
    // row order, which could put a new far-future item ahead of older work.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| {
                a.2["created"]
                    .as_i64()
                    .unwrap_or(i64::MAX)
                    .cmp(&b.2["created"].as_i64().unwrap_or(i64::MAX))
            })
            .then_with(|| {
                a.2["id"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b.2["id"].as_str().unwrap_or(""))
            })
    });

    let total = scored.len();
    let n_overdue = scored.iter().filter(|(o, ..)| *o).count();
    let shown: Vec<Value> = if want_all {
        scored.into_iter().map(|(_, _, v)| v).collect()
    } else {
        scored.into_iter().take(cap).map(|(_, _, v)| v).collect()
    };
    let hidden = total.saturating_sub(shown.len());
    let untyped = rows
        .iter()
        .filter(|r| {
            bs::ask_verdict(
                r.ask_actor.as_deref().unwrap_or(""),
                r.ask_type.as_deref().unwrap_or(""),
                r.ask_question.as_deref().unwrap_or(""),
                r.ask_unblocks.as_deref().unwrap_or(""),
            ) != bs::AskVerdict::Ok
        })
        .count();
    // Counted from the store rather than from `rows`, which by construction
    // holds only the active ones — a count derived from the filtered set could
    // never be anything but zero, which is the check-that-cannot-fail shape.
    let archived_excluded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM issues WHERE status='needsyou' \
             AND NOT (COALESCE(archived,0)=0 AND deleted IS NULL)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    Json(crate::api::measured::measured(
        json!({
            "queue": shown,
            "total": total,
            "shown": shown.len(),
            "hidden": hidden,
            "cap": if want_all { Value::Null } else { json!(cap) },
            "untyped_legacy": untyped,
            // AF-424: NAME THE POPULATION THIS VIEW DOES NOT CONSIDER.
            //
            // `total` counts ACTIVE needsyou cards. A reader comparing it
            // against a raw `/api/board?all=1` dump sees a much larger number
            // and reasonably concludes the view is dropping live work — which
            // is exactly what happened: gtm-engine measured 523 needsyou rows
            // against this view's 404 and read the difference as 119 cards
            // "outside it entirely". They are archived (121) and deleted (4),
            // and excluding them is correct; an archived card is not
            // outstanding owner-blocked work. But nothing here SAID so, and
            // `hidden: 4` reads as the whole remainder when it is only the
            // remainder past the cap.
            "archived_excluded": archived_excluded,
            "ranked_by": "overdue first (due <= today), then age_days * (blast_radius + 1), \
                          highest first",
            "overdue": n_overdue,
            "note": "THE owner view: the cards a human is actually blocking, capped so the \
                     list can be finished. `hidden` is the remainder, reachable with ?all=1 \
                     — the cap is a default, not a hiding place. `untyped_legacy` counts \
                     cards that predate the typed-ask gate (AF-318); they were never \
                     required to name a routable actor, direct question, human act, \
                     and exit condition, so an invalid/partial ask there means \
                     unrecorded legacy state, not junk.",
        }),
        total,
    ))
    .into_response()
}

/// The creator name the queue-disposition job files under (AF-317).
///
/// Exempt from the todo WIP limit BY NAME. Its card is the one that has to
/// arrive precisely when a lane's queue is too long, so refusing it for queue
/// depth would make the mechanism suppress its own alarm.
pub const QUEUE_DISPOSITION_CREATOR: &str = "queue-disposition";

/// How many needsyou cards the owner view shows before hiding the rest.
const NEEDSYOU_VIEW_CAP: usize = 10;

// ---- GET /api/board/derived -----------------------------------------------

/// Derived display status from durable facts.
///
/// The stored `status` is the lifecycle position a worker set. The `display_status`
/// is what a human triaging the board should see, computed from timestamps, session
/// liveness, evidence, and dependency state. This endpoint returns the board with
/// both, so the dashboard can group by either.
///
/// Rules (in priority order; first match wins):
///   1. status=needsyou AND entered_state_at older than 14 days: "aged-needsyou"
///   2. status=doing AND assigned session is not active for >1h: "stalled"
///   3. source=capture AND status=todo AND age > 72h AND no log activity: "stale"
///   4. status=done AND evidence IS NOT NULL: "verified-candidate"
///   5. depends_on non-empty AND all resolved: "unblocked"
///   6. otherwise: the stored status itself
async fn derived_board(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        let rows =
            bs::list_issues(&conn, &[], &[], ArchivedFilter::ActiveOnly)?;
        let working =
            crate::api::sessions_legacy::active_python_sessions(&conn);
        let now = now_secs();

        let mut items: Vec<Value> = Vec::with_capacity(rows.len());
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();

        for r in &rows {
            let display = derive_display_status(r, now, &working, &conn);
            *counts.entry(display.clone()).or_default() += 1;
            let mut v = list_body(r, true, is_stale(r, now, &working));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("display_status".into(), json!(display));
            }
            items.push(v);
        }

        Ok((items, counts))
    })
    .await;
    let (items, counts) = match joined {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    Json(crate::api::measured::measured(
        json!({
            "items": items,
            "counts": counts,
            "total": items.len(),
            "note": "display_status is computed from durable facts, never stored. \
                     Rules: aged-needsyou (>14d), stalled (doing + session idle >1h), \
                     stale (auto-captured todo >72h, no log), verified-candidate \
                     (done + evidence), unblocked (all deps resolved). Otherwise \
                     the stored status.",
        }),
        items.len(),
    ))
    .into_response()
}

fn derive_display_status(
    row: &IssueRow,
    now: i64,
    working: &std::collections::BTreeSet<String>,
    conn: &Connection,
) -> String {
    let status = row.status.as_str();
    let age_secs = now - row.created;

    // 1. Aged needsyou: status=needsyou and sitting longer than 14 days.
    if status == "needsyou" {
        let in_state_secs = row
            .entered_state_at
            .map(|t| now - t)
            .unwrap_or(age_secs);
        if in_state_secs > 14 * 86_400 {
            return "aged-needsyou".into();
        }
    }

    // 2. Stalled: status=doing but the assigned session is not active.
    if status == "doing" {
        if let Some(sess) = row.session.as_deref().filter(|s| !s.is_empty()) {
            let idle_secs = now - row.updated;
            if idle_secs > 3600 && !working.contains(sess) {
                return "stalled".into();
            }
        }
    }

    // 3. Stale autofix/capture: auto-captured todo older than 72h with no log.
    if status == "todo" {
        let is_auto = row
            .source
            .as_deref()
            .is_some_and(|s| s == "capture" || s == "autofix");
        let no_activity = row
            .log
            .as_deref()
            .map(|l| l.trim().is_empty())
            .unwrap_or(true);
        if is_auto && age_secs > 72 * 3600 && no_activity {
            return "stale".into();
        }
    }

    // 4. Verified candidate: done with evidence recorded.
    if status == "done" && row.evidence.is_some() {
        return "verified-candidate".into();
    }

    // 5. Unblocked: has dependencies and all are resolved (done/verified/discarded).
    if !row.depends_on.is_empty() && matches!(status, "todo" | "backlog" | "doing") {
        let all_resolved = row.depends_on.iter().all(|dep_id| {
            bs::get_issue(conn, dep_id)
                .ok()
                .flatten()
                .is_some_and(|dep| {
                    matches!(
                        dep.status.as_str(),
                        "done" | "verified" | "discarded"
                    )
                })
        });
        if all_resolved {
            return "unblocked".into();
        }
    }

    // 6. Passthrough: the stored status.
    status.to_string()
}

/// Why a candidate is NOT on the ready frontier, or `None` if it is ready.
///
/// EXTRACTED so a test can drive it (AMUX-3949). The first version of this logic
/// lived inline in the handler, which needs `AppState`, so the only cells that
/// could reach it tested the STORE instead -- and a mutant that disabled the
/// blocked-on arm entirely left them all green. A predicate the tests cannot
/// call is a predicate nothing pins.
///
/// Dependency blocking is NOT here: it needs a `Connection` and is already the
/// shared `deps_blocking`. This covers the two per-row facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrontierExclusion {
    /// Carries the `blocked_on` dimension (AMUX-3949).
    Blocked,
    /// Dormant containers and event watches are not actionable queue work.
    DormantType,
    /// The continuation gate is on for this lane and the card cannot satisfy it
    /// (AMUX-3946), so `doing` would refuse it.
    NoContinuation,
}

pub(crate) fn frontier_exclusion(
    row: &bs::IssueRow,
    gate_on: bool,
) -> Option<FrontierExclusion> {
    // BOTH SPELLINGS OF BLOCKED. `blocked_on` is the dimension; `status='blocked'`
    // is the legacy status still carried by 66 cards belonging to other lanes,
    // which this work deliberately did not rewrite (ethos rule 8). A consumer
    // honouring only the new one would silently make every legacy blocked card
    // workable, which is worse than the position-destroying status it replaces
    // because at least that one was visible.
    if row.blocked_on.as_deref().is_some_and(|b| !b.trim().is_empty()) {
        return Some(FrontierExclusion::Blocked);
    }
    if bs::parse_status(&row.status) == Some(TaskStatus::Blocked) {
        return Some(FrontierExclusion::Blocked);
    }
    if matches!(row.item_type.as_str(), "tripwire" | "watch" | "epic") {
        return Some(FrontierExclusion::DormantType);
    }
    if gate_on
        && bs::continuation_verdict(row.next_action.as_deref().unwrap_or(""))
            != bs::ContinuationVerdict::Ok
    {
        return Some(FrontierExclusion::NoContinuation);
    }
    None
}

#[cfg(test)]
mod frontier_exclusion_tests {
    use super::*;

    fn row(status: &str) -> bs::IssueRow {
        let conn = crate::db::migrate::test_memdb();
        let new = bs::NewIssue {
            title: "t".into(),
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
            depends_on: vec![],
            gate: vec![],
            tags: vec![],
            ask_type: None,
            ask_question: None,
            ask_unblocks: None,
            ask_actor: None,
            // AF-367: the HTTP create path: a real POST /api/board from a lane or a human.
            source: Some("agent".into()),
            requested_by: None,
            callback_session: None,
            callback_prompt: None,
        };
        bs::create_issue(&conn, &new, 1000).expect("create")
    }

    /// AMUX-3949. The frontier must honour BOTH spellings of blocked.
    ///
    /// This cell exists because a mutant that disabled the blocked-on arm
    /// entirely left every other cell green: the logic was inline in a handler
    /// that needs AppState, so nothing could reach it. A predicate the tests
    /// cannot call is a predicate nothing pins.
    /// A LOGGED SENTINEL MUST BE PRINTABLE (AF-481).
    ///
    /// `NEW_CARD_SELF_ID` is passed to `depends_on_cycle`, which logs it as
    /// `self_id` when it finds a pre-existing cycle elsewhere on the board. It
    /// used to be "\u{0}new-card". Nineteen of those bytes in a 67 MB
    /// server-rs.log made grep call the whole file binary, and `grep -o` then
    /// returned 8 matches where `grep -c` counted 17 lines, silently, into a
    /// pipe. The repo's own log-sweep doc prescribes greps over that file.
    ///
    /// The cell asserts the property rather than the string, so any future
    /// sentinel is covered: no control characters, and still impossible as a
    /// real card id (ids are `[A-Z]+-<digits>`).
    #[test]
    fn the_new_card_sentinel_cannot_poison_a_log_or_collide_with_an_id() {
        assert!(
            !NEW_CARD_SELF_ID.chars().any(|c| c.is_control()),
            "a sentinel that reaches a log line must be printable: {NEW_CARD_SELF_ID:?}"
        );
        // NON-COLLISION, the property the NUL was chosen for and which must
        // survive the fix. A real id is uppercase letters, a hyphen and digits;
        // anything outside that alphabet is as impossible as a NUL was.
        assert!(
            NEW_CARD_SELF_ID
                .chars()
                .any(|c| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')),
            "the sentinel must contain a character no card id can: {NEW_CARD_SELF_ID:?}"
        );
        // AND IT MUST NOT BE EMPTY, which would satisfy both assertions above
        // vacuously and match every id as a substring.
        assert!(!NEW_CARD_SELF_ID.is_empty(), "an empty sentinel is not a sentinel");
    }

    #[test]
    fn the_frontier_excludes_both_spellings_of_blocked() {
        // Ready: a plain todo with a continuation, gate on.
        let mut r = row("todo");
        r.next_action = Some("Run the compatibility suite against KubeRay 1.4".into());
        assert_eq!(frontier_exclusion(&r, true), None, "this one is genuinely ready");

        // The DIMENSION excludes it, without changing its position.
        let mut d = r.clone();
        d.blocked_on = Some("waiting on the KubeRay answer".into());
        assert_eq!(frontier_exclusion(&d, true), Some(FrontierExclusion::Blocked));
        assert_eq!(d.status, "todo", "and it is still a todo, which is the point");

        // The LEGACY STATUS excludes it too. 66 cards owned by other lanes still
        // use this spelling and were deliberately not rewritten.
        let mut l = r.clone();
        l.status = "blocked".into();
        assert_eq!(frontier_exclusion(&l, true), Some(FrontierExclusion::Blocked));

        // Whitespace is not a block. `blocked_on: "  "` is an empty field, and
        // treating it as a blocker would park a card on a typo forever.
        let mut w = r.clone();
        w.blocked_on = Some("   ".into());
        assert_eq!(frontier_exclusion(&w, true), None, "an empty string is not a block");
    }

    /// AMUX-4203/RH-125: a status update on a stale-trigger watch should append
    /// evidence, not resurrect the watch into current WIP.
    #[test]
    fn dormant_types_are_never_on_the_ready_frontier() {
        let mut r = row("todo");
        r.next_action = Some("Poll the external source when the watcher fires".into());
        for item_type in ["tripwire", "watch", "epic"] {
            let mut dormant = r.clone();
            dormant.item_type = item_type.into();
            assert_eq!(
                frontier_exclusion(&dormant, false),
                Some(FrontierExclusion::DormantType),
                "{item_type} must not be claimable queue work"
            );
        }
    }

    /// The continuation arm, and the control that it only fires when the gate is
    /// ON for the lane. 51 lanes have not opted in, and excluding their cards
    /// would empty every one of their frontiers.
    #[test]
    fn the_continuation_arm_only_fires_for_an_opted_in_lane() {
        let r = row("todo"); // no next_action
        assert_eq!(
            frontier_exclusion(&r, true),
            Some(FrontierExclusion::NoContinuation),
            "with the gate on, a card `doing` would refuse must not be offered"
        );
        assert_eq!(
            frontier_exclusion(&r, false),
            None,
            "with the gate OFF this card is workable, and excluding it would empty \
             the frontier of every lane that has not opted in"
        );
    }
}

/// GET /api/board/ready — what this lane can actually work right now.
///
/// AMUX-3948, G3 in docs/design/task-workflow-engine.md. `depends_on` was
/// already CONSUMED (board_drive parks on it, auto-promotes off it, has a prose
/// fallback) and answered no QUESTION. So auto-pickup selected by queue age,
/// and a lane could be handed a card whose blocker was still open.
///
/// The plan's own cards demonstrated it while this was being built: pickup
/// claimed P4 twice, whose control needs P3, which did not exist.
///
/// READY IS A QUERY, NOT A STATUS (decision 2 on AMUX-3945). "Gates pass and it
/// is claimable" is derived from four facts that each move on their own; storing
/// it would store a fact that goes stale the moment any one of them changes.
///
/// EVERY EXCLUSION IS COUNTED. An empty frontier has at least five distinct
/// causes -- nothing queued, all of it blocked, the WIP cap is full, the lane
/// has no cards, the probe never ran -- and they call for opposite responses.
/// `measured` plus `n_considered` plus the `excluded` histogram is what lets a
/// reader tell them apart (AF-320; the diagnostic contract).
async fn ready_frontier(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let lane = q
        .get("session")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| actor_from_headers(&headers).1);
    if lane.trim().is_empty() {
        return Json(crate::api::measured::unmeasured(
            json!({"ready": [], "session": null}),
            "no lane: pass ?session=<worker> or send X-Amux-Session. A frontier with no \
             lane would be every lane's work at once, which is not an answer to \
             'what can I work'.",
        ))
        .into_response();
    }
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20).clamp(1, 200);

    let Ok(conn) = state.store.read() else {
        return Json(crate::api::measured::unmeasured(
            json!({"ready": [], "session": lane}),
            "the board store could not be read, so nothing was examined. This is NOT an \
             empty frontier.",
        ))
        .into_response();
    };

    let f = lane_frontier(&conn, &lane, limit);
    let n_considered = f.n_considered;
    let body = json!({
        "session": lane,
        // Cards that pass every computable precondition, oldest first. Capacity
        // is reported BESIDE this rather than emptying it: "nothing is ready" and
        // "you are at the cap holding something" are different answers and a
        // caller that wants to know what is next while finishing a card deserves
        // the real list.
        "ready": f.ready,
        "claimable_now": f.claimable_now,
        "wip": {"doing": f.holding.len(), "cap": f.wip_cap, "available": f.wip_available,
                "holding": f.holding},
        "excluded": {
            "blocked_by_deps": f.blocked_by_deps,
            "blocked_by_parked_dep": f.blocked_by_parked_dep,
            "missing_continuation": f.missing_continuation,
            "continuation_gate_on": f.gate_on,
        },
    });
    Json(crate::api::measured::measured(body, n_considered)).into_response()
}

/// CDC catch-up: returns board_change_log rows after a given seq.
/// Clients call this after an SSE reconnect to replay missed mutations.
async fn board_changes(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let since_seq: i64 = q
        .get("since_seq")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: usize = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(500)
        .clamp(1, 5000);

    let conn = match state.store.read() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "db read failed"})),
            )
                .into_response();
        }
    };

    match crate::runtime_jobs::cdc_poller::changes_since(&conn, since_seq, limit) {
        Ok(rows) => {
            let cursor = crate::runtime_jobs::cdc_poller::last_seq();
            Json(json!({
                "changes": rows,
                "cursor": cursor,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("query failed: {e}")})),
        )
            .into_response(),
    }
}

/// (considered, moved, refused) carried out of the bulk-migrate write closure,
/// which can only return a `WriteOutcome`. Per call, never a static: two
/// concurrent migrations would clobber a global and each would report the
/// other's numbers.
type BulkOutcome = Arc<Mutex<Option<(usize, usize, Vec<Value>)>>>;

/// Did EVERY card refuse for the same gate? (AMUX-4044)
///
/// THERE IS NO COLUMN-LEVEL GATE TO PRE-CHECK, and finding that out is the
/// whole story of this function. The first draft queried a `board_statuses`
/// table that does not exist, swallowed the error, read "ungated" and let a
/// `todo -> done` request through. Pointing it at the real `statuses` table
/// did not fix it either: a fresh schema has no gate stored on `done` at all,
/// while the live refusals reported `source: "typed"`. Gates resolve through
/// `effective_gate_trail`, which is per CARD — it reads the card's session and
/// its lane's groups — so "is this column gated" has no single answer to look
/// up. A pre-check could only ever have been a guess that disagreed with the
/// authority.
///
/// So the authority decides, per card, and this reads the result. When every
/// considered card refused with the SAME gate, the honest response is a 409
/// naming it, not a 200 saying "moved 0" and leaving the caller to infer why.
/// Measured live before this existed: `todo -> done` returned HTTP 200 with
/// `considered: 147, moved: 0` and 147 identical `GateBlocked` refusals.
fn unanimous_gate(considered: usize, moved: usize, refused: &[Value]) -> Option<String> {
    if considered == 0 || moved > 0 || refused.len() != considered {
        return None;
    }
    let first = refused.first()?.get("why")?.as_str()?.to_string();
    if !first.starts_with("GateBlocked") {
        return None;
    }
    // Every one, not just the first: a column where SOME cards are gated and
    // others failed for their own reasons is a mixed result, and flattening
    // that into one gate message would hide the rest.
    refused
        .iter()
        .all(|r| r.get("why").and_then(Value::as_str) == Some(first.as_str()))
        .then_some(first)
}

/// POST /api/board/bulk-migrate — move every live card out of one column.
///
/// Ethan, 2026-09-02: an ellipsis on a board column, "migrate all ... for
/// example in backlog to discarded".
///
/// A GATED TARGET IS REFUSED OUTRIGHT, and that is the design rather than a
/// limitation. `doing`, `review`, `done` and `verified` each carry a checklist
/// a human is supposed to answer per card; one blanket acknowledgement across
/// 489 backlog cards would assert all four of `verified`'s criteria about work
/// nobody looked at, which is exactly the claim AF-321 exists to stop. The
/// named use case, backlog to discarded, is ungated and unaffected.
///
/// `gate_ack` and `force` are deliberately NOT set below, so even if this
/// somehow ran against a gated column every card would refuse individually
/// rather than slip through. The upfront check is the honest error; the unset
/// flags are the belt.
///
/// One transaction: a half-migrated column is a worse state than a refused
/// one, and `advance` is the same transition primitive every other path uses,
/// so a card that could not move for its own reason (archived, a stale CAS)
/// is reported rather than silently skipped.
async fn bulk_migrate(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let from = body.get("from").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let to = body.get("to").and_then(Value::as_str).unwrap_or("").trim().to_string();
    // Optional: restrict to one lane's board. Absent means the global column.
    let lane = body
        .get("session")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if from.is_empty() || to.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "from and to are required", "hint": "POST {\"from\":\"backlog\",\"to\":\"discarded\"}"}),
        );
    }
    if from == to {
        return err(StatusCode::BAD_REQUEST, json!({"error": "from and to are the same column"}));
    }
    let actor = actor_from_headers(&headers).1;

    // THE GATE CHECK, once, up front, on a READ. Doing it inside the write
    // closure meant smuggling the verdict out through a fake event; a column's
    // gate is plain stored state and reading it is the honest way to ask.

    // PER-CALL, not a static: two concurrent migrations would clobber a global
    // and each would report the other's numbers. Same handle shape the rename
    // cascade uses to carry its step list out of the write closure.
    let out: BulkOutcome = Default::default();
    let out_c = out.clone();
    let (f3, t3, lane3, actor3) = (from.clone(), to.clone(), lane.clone(), actor.clone());
    let result = state
        .store
        .write_async(move |conn| {
            // AN EXPLICIT SELECTION WINS (AMUX-4058). The UI sends the ids it
            // actually rendered, so a filtered column migrates what the human
            // saw. Without this the server took a whole column and a filtered
            // view would have moved ~500 cards while showing twelve.
            //
            // Still re-checked against status and lane below by `advance`'s
            // `expected_from`, so a stale id from a view rendered minutes ago
            // refuses rather than moving a card that has since left the column.
            let explicit: Vec<String> = body
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let ids: Vec<String> = if !explicit.is_empty() { explicit } else {
                let (sql, params): (String, Vec<String>) = match &lane3 {
                    Some(l) => (
                        "SELECT id FROM issues WHERE status=?1 AND session=?2 \
                         AND deleted IS NULL AND COALESCE(archived,0)=0 ORDER BY id"
                            .into(),
                        vec![f3.clone(), l.clone()],
                    ),
                    None => (
                        "SELECT id FROM issues WHERE status=?1 \
                         AND deleted IS NULL AND COALESCE(archived,0)=0 ORDER BY id"
                            .into(),
                        vec![f3.clone()],
                    ),
                };
                let mut st = conn.prepare(&sql)?;
                let rows = st.query_map(rusqlite::params_from_iter(params), |r| r.get::<_, String>(0))?;
                rows.filter_map(Result::ok).collect()
            };
            let mut moved = 0usize;
            let mut refused: Vec<Value> = Vec::new();
            let mut events = Vec::new();
            for id in &ids {
                let opts = crate::db::advance::AdvanceOpts {
                    expected_from: Some(f3.clone()),
                    log_line: Some(format!("bulk-migrated {f3} -> {t3} by {actor3}")),
                    ..Default::default()
                };
                match crate::db::advance::advance(conn, id, &t3, &actor3, &opts)? {
                    Ok(o) => {
                        moved += 1;
                        events.extend(o.events);
                    }
                    Err(why) => refused.push(json!({"id": id, "why": format!("{why:?}")})),
                }
            }
            conn.execute(
                "INSERT INTO session_events (ts, session, type, data, source) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![
                    crate::config::now_f64(),
                    actor3,
                    "board.bulk_migrated",
                    json!({"from": f3, "to": t3, "session": lane3, "moved": moved,
                           "refused": refused.len(), "considered": ids.len()})
                    .to_string(),
                    "board-api"
                ],
            )
            .ok();
            *out_c.lock().unwrap() = Some((ids.len(), moved, refused));
            Ok(crate::db::WriteOutcome { applied: moved > 0, events })
        })
        .await;
    match result {
        Ok(_) => {
            let (considered, moved, refused) =
                out.lock().ok().and_then(|mut g| g.take()).unwrap_or((0, 0, Vec::new()));
            if let Some(why) = unanimous_gate(considered, moved, &refused) {
                return err(
                    StatusCode::CONFLICT,
                    json!({
                        "error": format!("every card in '{from}' refused the move to '{to}'"),
                        "gate": why,
                        "why": "a gate is a per-card claim; one acknowledgement for a whole \
                                column would assert it about cards nobody looked at",
                        "hint": "move these individually, or pick an ungated column",
                        "considered": considered,
                    }),
                );
            }
            Json(json!({
                "ok": true, "from": from, "to": to, "session": lane,
                "considered": considered, "moved": moved,
                // Named, not just counted: a caller that moved 480 of 489 needs
                // to know WHICH nine and why, or the number is a mystery.
                "refused": refused,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}

/// One lane's work frontier: what it could claim, and what is in the way.
///
/// EXTRACTED SO THERE IS ONE EVALUATOR (AMUX-4029). `/api/board/ready` was the
/// only caller and the only place this reasoning existed, so anything else that
/// wanted to know whether a lane had claimable work either re-derived it or did
/// without. `board.lane_idle_with_ready_work` is the second caller, and the
/// point of sharing the function rather than the shape is that a detector which
/// disagreed with the endpoint would be reporting a stall the endpoint denies.
pub(crate) struct LaneFrontier {
    pub ready: Vec<Value>,
    pub claimable_now: usize,
    pub wip_cap: usize,
    pub wip_available: usize,
    pub holding: Vec<String>,
    pub blocked_by_deps: usize,
    /// Of `blocked_by_deps`, the ones whose blocker CANNOT clear on its own:
    /// parked in `backlog`/`needsyou`, discarded, or resolving to no card.
    /// These are deadlocked, not waiting.
    pub blocked_by_parked_dep: usize,
    pub missing_continuation: usize,
    pub gate_on: bool,
    pub n_considered: usize,
}

pub(crate) fn lane_frontier(
    conn: &rusqlite::Connection,
    lane: &str,
    limit: usize,
) -> LaneFrontier {
    // CAPACITY, from the same predicate the `doing` gate refuses on: same status,
    // same type exclusions, same archived/deleted filter. A frontier that
    // disagreed with the gate would offer cards the gate then refuses, which is
    // the view/mechanism split ethos rule 1 is about.
    let holding: Vec<String> = conn
        .prepare(
            "SELECT id FROM issues WHERE session = ?1 AND status = 'doing' \
               AND deleted IS NULL AND COALESCE(archived,0) = 0 \
               AND COALESCE(type,'') NOT IN ('tripwire','watch','epic') ORDER BY id",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![lane], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    let cap = crate::runtime_jobs::board_drive::wip_cap().max(0) as usize;
    let available = cap.saturating_sub(holding.len());

    // Candidates: claimable cards this lane owns. `todo` only — `backlog` is
    // parked on a trigger and `review` is somebody else's turn.
    let ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM issues WHERE session = ?1 AND status = 'todo' \
               AND deleted IS NULL AND COALESCE(archived,0) = 0 \
               AND COALESCE(type,'') NOT IN ('tripwire','watch','epic') \
             ORDER BY COALESCE(created,0) ASC",
        )
        .and_then(|mut st| {
            st.query_map(rusqlite::params![lane], |r| r.get::<_, String>(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    let n_considered = ids.len();

    let gate_on = bs::continuation_required(Some(lane));
    let (mut blocked_by_deps, mut missing_continuation) = (0usize, 0usize);
    let mut blocked_by_parked_dep = 0usize;
    let mut ready: Vec<Value> = Vec::new();
    let now = crate::config::now_f64();

    for id in &ids {
        let Ok(Some(row)) = bs::get_issue(conn, id) else { continue };
        // THE SHARED PREDICATE, not a second spelling of it (AMUX-3814).
        let blockers = crate::runtime_jobs::board_drive::deps_blocking(conn, &row);
        if !blockers.is_empty() {
            blocked_by_deps += 1;
            // WAITING AND DEADLOCKED ARE DIFFERENT ANSWERS (AMUX-4040), and one
            // number could not tell them apart. A dependency that is `doing` or
            // `todo` will clear on its own; one that is parked in `backlog` /
            // `needsyou`, or that resolves to no card at all, will not clear
            // ever, and the lane waiting on it is stuck rather than patient.
            //
            // rtsp-connection read `blocked_by_deps: 12` over 13 todo cards. The
            // whole queue hung off ONE parked card, and nothing in the payload
            // said which kind of blocked it was, so the honest reading was
            // "busy waiting" when the truth was "will never move".
            //
            // Counted per CARD, not per edge: a card is deadlocked if ANY
            // blocker cannot clear, because it only takes one.
            if blockers.iter().any(|b| {
                bs::get_issue(conn, b).ok().flatten().is_none_or(|d| {
                    matches!(d.status.as_str(), "backlog" | "needsyou" | "discarded")
                })
            }) {
                blocked_by_parked_dep += 1;
            }
            continue;
        }
        match frontier_exclusion(&row, gate_on) {
            Some(FrontierExclusion::Blocked) => {
                blocked_by_deps += 1;
                continue;
            }
            Some(FrontierExclusion::DormantType) => {
                continue;
            }
            Some(FrontierExclusion::NoContinuation) => {
                missing_continuation += 1;
                continue;
            }
            None => {}
        }
        ready.push(json!({
            "id": row.id,
            "title": row.title,
            "type": row.item_type,
            "next_action": row.next_action,
            "epic": row.epic,
            // Time-in-state, now that it exists (AMUX-3947). NULL means the card
            // predates migration 0040 and has not moved since: not measured,
            // never zero.
            "entered_state_at": row.entered_state_at,
            "time_in_state_s": row.entered_state_at.map(|e| (now as i64 - e).max(0)),
            "age_s": (now as i64 - row.created).max(0),
        }));
    }
    ready.truncate(limit);

    LaneFrontier {
        claimable_now: available.min(ready.len()),
        ready,
        wip_cap: cap,
        wip_available: available,
        holding,
        blocked_by_deps,
        blocked_by_parked_dep,
        missing_continuation,
        gate_on,
        n_considered,
    }
}

/// GET /api/board/contract — the gate table, types, and CLI syntax.
/// Every gate-blocked 409 tells the caller to `GET /api/board/contract`
/// to understand the rules. Without this endpoint that instruction is a
/// dead link (AR-123).
async fn get_contract(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    use serde_json::json;
    let statuses = ["doing", "review", "done", "verified"];
    let mut gates = serde_json::Map::new();
    for ty in bs::KNOWN_TYPES {
        let mut ty_gates = serde_json::Map::new();
        for &st in &statuses {
            if let Some(target) = bs::parse_status(st) {
                let g = bs::default_gates_for(ty, target);
                if !g.is_empty() {
                    ty_gates.insert(st.to_string(), json!(g));
                }
            }
        }
        if !ty_gates.is_empty() {
            gates.insert(ty.to_string(), serde_json::Value::Object(ty_gates));
        }
    }
    // AF-112: the contract used to serve ONLY the type defaults — tier 5 of a
    // five-tier precedence — while enforcement resolved all five, so a custom
    // worker/group/global gate (the amux group's peer-review verified bar,
    // for one) was advertised NOWHERE and the 409's own "learn the gate at
    // /contract" pointer sent the reader to a LOWER bar than the one refusing
    // them. Two repairs: (1) ?card=<id> resolves the ACTUAL gate for a card
    // through the same effective_gate_configured enforcement uses — one
    // resolver, never two spellings; (2) the global custom tier, which is
    // card-agnostic, is served alongside the defaults whenever it exists.
    let mut card_gates: Option<serde_json::Value> = None;
    let mut global_gates = serde_json::Map::new();
    if let Ok(conn) = state.store.read() {
        for &st in &statuses {
            if let Some(target) = bs::parse_status(st) {
                if let Some(g) = bs::configured_gate(&conn, target) {
                    global_gates.insert(st.to_string(), json!(g));
                }
            }
        }
        if let Some(card_id) = q.get("card").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            match bs::get_issue(&conn, card_id) {
                Ok(Some(row)) => {
                    let mut per = serde_json::Map::new();
                    // SAY WHICH TIER EACH GATE CAME FROM (AMUX-3567). The
                    // resolved gate was already here; the SOURCE was not, and
                    // without it nothing surfaces that a worker or group carries
                    // a custom gate until you trip it. AF-168's reporter learned
                    // it from a refusal and drew the wrong mechanism from it;
                    // amux-frustrations found the answering endpoint by grepping
                    // the resolver.
                    //
                    // It varies BY TRANSITION, not just by scope — measured
                    // 2026-08-23, `group:amux` pins only `verified`,
                    // `tubescience` only `done`, `amux-cloud` both `review` and
                    // `verified` — so a per-card summary would be wrong for the
                    // transitions it did not describe. One entry per status.
                    //
                    // Same walk enforcement uses, so the contract and the
                    // refusal cannot disagree about where a bar came from.
                    let mut sources = serde_json::Map::new();
                    let groups = row
                        .session
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(crate::api::session_verbs::lane_groups)
                        .unwrap_or_default();
                    for &st in &statuses {
                        if let Some(target) = bs::parse_status(st) {
                            let (g, src) =
                                bs::effective_gate_with_source(&conn, &row, target, &groups);
                            if !g.is_empty() {
                                per.insert(st.to_string(), json!(g));
                                sources.insert(
                                    st.to_string(),
                                    json!({
                                        // AMUX-3573: `source`/`scope` are for a
                                        // client that BRANCHES on the tier; the
                                        // prose below is for a human reading a
                                        // refusal. Both, because parsing the
                                        // sentence is the alternative and it is
                                        // the kind of coupling that breaks on a
                                        // wording change nobody connects to it.
                                        "source": src.token(),
                                        "scope": src.scope(),
                                        "retype_would_change_it": src.retype_would_help(),
                                        "explain": src.explain(),
                                    }),
                                );
                            }
                        }
                    }
                    card_gates = Some(json!({
                        "card": card_id,
                        "type": row.item_type,
                        "session": row.session,
                        "gates": per,
                        "gate_sources": sources,
                        "note": "resolved through the SAME precedence enforcement uses \
                                 (card override → worker → group → global → type default) — \
                                 this is the gate a transition will actually be judged by",
                    }));
                }
                Ok(None) => {
                    card_gates = Some(json!({
                        "card": card_id,
                        "error": "no such card — the type-default table below still applies",
                    }));
                }
                Err(_) => {}
            }
        }
    }
    Json(json!({
        "types": bs::KNOWN_TYPES,
        "gates": gates,
        "gates_are": "TYPE DEFAULTS ONLY — tier 5 of 5. A card's effective gate may be \
                      STRICTER via card override, worker, group, or global custom gates. \
                      Pass ?card=<id> for the resolved gate enforcement will actually use.",
        "global_custom_gates": if global_gates.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(global_gates)
        },
        "card_effective_gates": card_gates,
        // Global done constraint (Ethan). Applies to EVERY type on top of the
        // per-type gate above, and unlike those criteria it is machine-checked
        // against the card text, so gate_ack / --checked cannot satisfy it.
        "done_requires_asset_link": {
            "rule": bs::ASSET_LINK_CRITERION,
            "accepts": "a URL, a repo file path (a/b.ext), a commit sha, or a #PR/issue reference, in the card's desc or history",
            "enforced": "server-validated on any transition to done; force bypasses it (logged); gate_ack cannot",
            "override": "set AMUX_DONE_LINK_REQUIRED=0 in a worker's / group's / global configuration to opt that level out",
        },
        // Global done constraint (AF-321). Sits IN FRONT of the asset-link rule
        // above, which it does not replace: that one is a shape check over the
        // whole desc and the card's own problem statement satisfies it (843 of
        // 1372 open cards, measured 2026-08-29). Published here for the AF-112
        // reason the `verified` block gives below.
        "done_requires_evidence": {
            "rule": "a card entering done must carry `evidence`: what was actually run or produced",
            "why": "the asset-link rule looks for a path-shaped token anywhere in the desc, which the FILING supplies — a card that names the file it intends to edit passes its own done gate before anyone touches that file",
            "accepts": "a command (backticked, or on a `$ ` line), a repo file path, a URL, a commit sha, a #PR — or `none: <reason>` (3+ words) when the card genuinely produced no artifact",
            "field": "`evidence`, writable on its own so it can be recorded BEFORE the transition that needs it",
            "enforced": "server-validated on any transition to done; force bypasses it (logged); gate_ack cannot",
            "override": "set AMUX_DONE_EVIDENCE_REQUIRED=0 in a worker's / group's / global configuration to opt that level out",
            "what_to_run": "the repo's VERIFY.md names the proof for each surface",
        },
        // Global `needsyou` constraint (AF-318). Published for the AF-112
        // reason: a gate you can only learn by tripping it teaches nobody.
        "needsyou_requires_typed_ask": {
            "rule": "a card entering needsyou must name a specific `ask_actor`, a valid `ask_type`, a direct `ask_question` containing ?, and an observable `ask_unblocks` exit",
            "why": "389 live cards were parked here at a median age of 15 days and 51% of them were not blocked on a human at all — `needsyou` was the only status that cost a worker nothing and stopped the nudge, so the ~20 real asks became unfindable",
            "ask_types": bs::ASK_TYPE_HELP.iter().map(|(k, v)| json!({"type": k, "means": v})).collect::<Vec<_>>(),
            "fields": "`ask_actor` is the specific person or external actor, `ask_question` is the direct question, and `ask_unblocks` is what ends the block — the latter two are sentences (3+ words). All four are writable on their own, so a refused transition cannot discard them",
            "enforced": "server-validated on any transition to needsyou; force bypasses it (logged); gate_ack cannot",
            "override": "set AMUX_NEEDSYOU_ASK_REQUIRED=0 in a worker's / group's / global configuration to opt that level out",
            "not_retroactive": "the 445 existing cards are untouched — the gate is on the transition. They drain by being re-asked, not by a sweep guessing on their behalf",
            "owner_view": "GET /api/board/needsyou — ranked by age x blast radius, capped at 10, ?all=1 for the remainder",
        },
        // Global `verified` constraint (Ethan, 2026-08-29). Published HERE and
        // not only in the 409, because a gate you can only learn by tripping it
        // is the AF-112 shape — the reader who most needs it is the one about
        // to be refused.
        "verified_requires_gate_checked": {
            "rule": "a multi-criterion verified gate must be acknowledged criterion by criterion; gate_ack: true is refused",
            "why": "the four default criteria fail in different ways and are checked by different acts; one boolean asserts all of them and records which you looked at nowhere",
            "enforced": "server-validated on any transition to verified whose effective gate has 2+ criteria; force bypasses it (logged and attributed)",
            "not_enforced_on": "single-criterion gates (acking one criterion is identical to checking it) and every other status, `done` included — done carries the machine-checked asset link instead",
        },
        "how_to_ack": {
            "cli": "amux board <status> <id> --checked \"criterion 1\" \"criterion 2\"",
            "api": "PATCH /api/board/<id> with gate_checked: [\"criterion 1\", ...] or gate_ack: true",
            // NAME THE FIELD AND THE COMMAND, not just the intent (AMUX-3590).
            // This said "set its type first" and stopped there, so a reader
            // knows WHAT to do and not HOW — and the writable field is `type`
            // while the gate refusal body displays the key as `item_type`. A
            // PATCH of `item_type` is ignored (honestly, via `ignored_fields`,
            // but a caller has to read the body to find out). Six cards were
            // filed mistyped in one night by guessing the field name at CREATE,
            // where no refusal exists to correct you.
            "wrong_type": "If the item has no code, set its type first — the gate is DERIVED                            from the type. CLI: `amux board type <id> <type>`. API: PATCH                            /api/board/<id> with {\"type\": \"investigation\"} — the field is                            `type`, NOT `item_type` (that one is ignored and reported in                            `ignored_fields`). Settable at creation too: POST /api/board with                            {\"title\": ..., \"type\": ...}.",
        },
        "worker_board_ownership": {
            "rule": "an identified worker may create cards only on its own board; `session` must equal the verified X-Amux-Worker/X-Amux-Session identity",
            "peer_links": "cross-worker collaboration is represented without transferring board ownership: set `reviewer` or `shepherd` to the peer and use `depends_on` for cross-board task dependencies",
            "cli": "amux board request <worker> <title> creates the card on the caller's board and links <worker> as reviewer",
            "security": "a worker cannot create an unassigned card or place a new card directly on another worker's board; anonymous/human control-plane callers retain administrative placement",
        },
        "capture_decomposition": {
            "cli": "amux board decompose <capture-id> --stdin",
            "plan_shape": {"tasks":[
                {"title":"Parse invoices", "description":"Parse and validate invoice rows", "type":"chore", "priority":1, "depends_on":[], "next_action":"Implement strict CSV parsing", "acceptance_criteria":["Malformed amounts exit nonzero with an explicit diagnostic"]},
                {"title":"Produce report", "description":"Generate a report from valid rows", "type":"chore", "priority":1, "depends_on":[1], "next_action":"Generate the customer totals", "acceptance_criteria":["The report totals match the input invoices"]}
            ]},
            "atomicity": "the root becomes an epic and all 2-50 children are created in one SQLite writer transaction; any invalid child creates zero",
            "required_per_child": ["unique title", "concrete description", "non-epic type", "p0-p3 priority", "earlier-task dependency indexes", "concrete next_action", "1-12 falsifiable acceptance_criteria"],
            "idempotency": "the normalized plan SHA-256 is durable on the root epic; an identical retry returns idempotent=true and a different retry returns 409 decomposition_plan_conflict",
            "dependency_execution": "todo/backlog claims are refused until every dependency is resolved: runtime-changing types require verified; other types may finish at done; missing or discarded dependencies remain blocking, and a committed successor is not stranded by unrelated todo queue depth",
            "graph": "GET /api/graph/board: versioned snapshot of tasks, workers, artifacts and recorded messages, typed provenance, cycle/missing-reference verification, and deterministic prerequisite-first layers. GET /api/graph/board/verify or amux board graph --check: lightweight structural preflight using the same verifier, without task prose. Structural order is not a workflow readiness or artifact-existence claim. Mutate through board/decompose/artifacts APIs; periodic board.graph_integrity detects legacy corruption.",
            "completion": "when every child is done, verified, discarded, or quarantined, board-drive closes the root epic and records the child-status summary as evidence",
        },
        // AMUX-2933 (ts-gke). The list filters WORK and were documented
        // NOWHERE — "discoverable only by guessing", and the cap was worse than
        // undocumented: silent. A lane auditing its own board got the 100
        // most-recent terminal rows fleet-wide and no signal that it was a
        // sample, so `GET /api/board` could return FEWER of its done cards than
        // `?session=<lane>` did. That reads as data, not as truncation.
        // RECOVERY, because a path nobody knows about is one nobody uses
        // (mvs-infra, 2026-08-23). They overwrote 4082 chars of merge evidence
        // on a card and recovered it in three minutes from _amux_state_events —
        // then pointed out that nothing in the API told them it was there. The
        // write guard added the same day refuses the destructive case; this is
        // for the ones that already happened, and for `force`/ack'd replaces
        // which are still allowed to destroy prose on purpose.
        "recovering_a_clobbered_desc": {
            "where": "_amux_state_events rows carry the FULL pre-mutation card snapshot in                       their payload, so a description overwritten by a PATCH is recoverable                       without any backup",
            "how": "find the row for the mutation (by card id and timestamp) and read the                     snapshot out of its payload",
            "prevention": "PATCH refuses two acts and the refusal body names which via `rule`: SIZE — a replace dropping a strict majority of a desc of 500+ chars, any writer; and AUTHORSHIP — a replace by a DIFFERENT session in which NONE of the card owner's lines survive, at any magnitude and in either direction. Length is not the test: a 54-char desc replaced by 17 chars and a 264-char desc replaced by 392 both destroyed everything and both passed the old size floors (AF-191). Both escape via desc_shrink_ack. To ADD rather than replace — almost always what a reviewer means — send PATCH /api/board/<id> {\"desc_append\": \"your note\"}. `desc_append` is a FIELD in the PATCH body, not a sub-path: POST /api/board/<id>/desc-append is NOT routed and a lane guessed it twice on 2026-08-24 (AF-187)",
            "why_this_happens": "GET /api/board OMITS `desc` (slim rows carry desc_len/                                 desc_head, and a `slim` key holding the ARRAY of dropped                                  field names — not the flag `1` this line claimed until                                  2026-08-27). An ABSENT field is not an empty one, and                                  .get(\"desc\") returns None either way — read desc_len,                                  read `slim` to see everything else that was dropped, or                                  GET the single card",
        },
        "list": {
            "endpoint": "GET /api/board",
            "returns": "a bare JSON array of items (NOT an envelope) — kept that way \
                        because every caller and the SPA index it directly",
            "filters": {
                "session": "comma-separated worker names",
                "status": "comma-separated statuses",
                "archived": "absent/\"\" = no filter · 1|true|yes = archived ONLY · \
                             any other value (0, false, …) = non-archived only",
                "done_limit": "cap on TERMINAL items (done/verified/discarded), keeping the \
                               most recently updated. 0 or negative = uncapped",
                "all": "1|true|yes = uncap the terminal set (equivalent to done_limit=0) — the \
                        complete enumeration; use this or ?status=<s> to defeat the render cap",
                "limit": "page size, applied AFTER done_limit",
                "offset": "page offset",
                "slim": "1 = trimmed item bodies (desc_head/desc_len/log_n/folded_n instead of \
                        prose) — the DEFAULT shape since AMUX-3496. Each slim row carries a \
                        `slim` key whose VALUE IS THE ARRAY of field names that row dropped \
                        (see slim_omits), so a consumer can tell a dropped field from an empty \
                        one (AF-161: a census read absence as emptiness and was 100% wrong). \
                        This said \"slim\": 1 until 2026-08-27, describing a flag where the code \
                        ships the answer — a reader who trusted it would never think to look \
                        there for WHICH fields went missing, which is the AF-161 failure with \
                        the remedy already built",
                "full": "1 = full prose bodies (desc + log). The default list is slim; a \
                         reader that needs desc/log must ask (slim=0 also honored)",
                "quota": "1 = per-status terminal quotas (verified floor 300; done/discarded \
                          share done_limit) instead of the lumped cap — the dashboard poll's \
                          shape (AMUX-3503)",
                "count": "1 = return {count, filter} instead of the rows. Counts what THIS \
                          filter would return, from the same filter+cap the list runs, so a \
                          header cannot disagree with what expanding it shows. Pass the SAME \
                          params you will fetch with — terminal statuses are capped unless \
                          you add done_limit=0 or all=1. Added for the dashboard's collapsed \
                          Archived (N) header, where shipping the set to render a number cost \
                          a measured +38% on the board poll (AMUX-3715)",
            },
            // NOT a filter — descriptive metadata about what `slim` DROPS, so it
            // lives outside `filters`. It sat inside `filters` from 64a9cb7d until
            // this commit and turned `check` red on main: board_contract_filters
            // asserts every key in `filters` is a real `ListParams` field, and a
            // descriptive key can never be one. Adding it to `ListParams` would have
            // gone green and been WRONG — it would document a query param that does
            // not exist and that axum silently drops, which is the exact defect that
            // test was written to catch. Keep `filters` strictly name -> description.
            // SERIALIZED FROM THE CONST, not restated. This was a hardcoded
            // literal of the same six fields while `SLIM_OMITS` (the list the
            // slim writer actually removes) sat 800 lines below it — two
            // definitions of one fact, in one file, neither referencing the
            // other, and already textually divergent: the literal was in a
            // different ORDER, so a diff of the two would not have looked like
            // a duplicate.
            //
            // I introduced the second one myself, in d3cc2179, in the commit
            // that closed AF-161's class. The contract's copy landed hours
            // earlier at 64a9cb7d and I never grepped for it. That is the whole
            // failure mode AF-161 names — one fact, two spellings, drift is a
            // matter of time — reintroduced by the fix for it.
            "slim_omits": SLIM_OMITS,
            "not_a_filter": {
                "q / query / search": "REFUSED with 400 — /api/board does not search, it would \
                                       return the entire board. Use /api/search?q=",
            },
            "terminal_cap": {
                "default_unscoped": 100,
                "default_scoped": 0,
                "scoped_means": "session= or status= is present — a bounded QUESTION is answered \
                                 in full; only the unbounded list is sampled",
                "why": "the unfiltered board is ~4.5MB at a cap of 100 and ~19.8MB uncapped \
                        (1186 vs 5576 items, measured 2026-08-11). amux is mobile-first, so the \
                        default stays capped — the defect was never the cap, it was the silence",
                "detect_truncation": "response headers x-amux-truncated (1|0), \
                                      x-amux-terminal-total, x-amux-terminal-returned, \
                                      x-amux-done-limit",
                "to_get_everything": "?done_limit=0 (or scope the query with session=/status=)",
                // CORRECTED by ts-gke's reconciliation, 2026-08-11. The first
                // version of this line said `?session=<worker>` full stop, and
                // that is the query for "everything I own" — NOT for "what do I
                // still have to act on". Their three counts, all correct once
                // understood: bare list 8 (capped), ?session&status=done 101,
                // the same +archived=0 → 59. The 42-card difference is archived
                // cards, which are TERMINAL AND IMMUTABLE — a status PATCH on
                // one is refused with `archived_task_immutable`
                // ("task is archived; restore it first", amux-core/src/board.rs).
                // So an audit built on 101 counts 42 cards nobody can act on,
                // and the auto-continue nudge that said 60 was right the whole
                // time: it counts the actionable set.
                "auditing_your_own_cards": "GET /api/board?session=<worker>&status=done&archived=0 \
                                            — scoped queries are uncapped, and archived=0 drops \
                                            cards that are terminal AND immutable (a status PATCH \
                                            on an archived card is refused with \
                                            archived_task_immutable). Dropping archived=0 answers \
                                            'everything I own', which is a different question and \
                                            the one that inflates a verification backlog. The bare \
                                            list answers neither.",
            },
        },
    }))
    .into_response()
}

/// GET /api/board/statuses — Python `_load_board_statuses` (amux-server.py
/// :15933): the SPA builds its kanban COLUMNS from this list, silently
/// falling back to a hardcoded default set on any failure — so a 404 here
/// meant custom Python-configured columns never rendered on the Rust origin
/// (AMUX-2596). Shape: [{id, label, mode, gate}] ordered by position;
/// Python's builtin defaults when the table is empty/absent.
async fn list_statuses(State(state): State<AppState>) -> Response {
    const DEFAULTS: [(&str, &str); 7] = [
        ("backlog", "Backlog"),
        ("todo", "To Do"),
        ("doing", "In Progress"),
        ("review", "In Review"),
        ("done", "Done"),
        ("verified", "Verified"),
        ("discarded", "Discarded"),
    ];
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut out: Vec<Value> = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT id, label, gate, mode FROM statuses ORDER BY position")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        }) {
            for (id, label, gate, mode) in rows.flatten() {
                let gate: Value = gate
                    .as_deref()
                    .and_then(|g| serde_json::from_str(g).ok())
                    .unwrap_or_else(|| json!([]));
                let mode = mode.filter(|m| !m.is_empty()).unwrap_or_else(|| "implicit".into());
                let terminal = matches!(id.as_str(), "verified" | "discarded");
                out.push(json!({ "id": id, "label": label, "mode": mode, "gate": gate, "terminal": terminal }));
            }
        }
    }
    if out.is_empty() {
        // Python: default columns, and note Python's dict here has NO
        // mode/gate keys on defaults — the SPA tolerates their absence.
        out = DEFAULTS
            .iter()
            .map(|(id, label)| {
                let terminal = matches!(*id, "verified" | "discarded");
                json!({ "id": id, "label": label, "terminal": terminal })
            })
            .collect();
    }
    Json(Value::Array(out)).into_response()
}


// ---- per-session gate overrides (AMUX-2599) -------------------------------
//
// Python `_load_session_gates` (py:16105) + the GET/PATCH pair at py:69563.
// The layer between the global per-status default and the per-card override.
//
// The SPA fetches this on EVERY board load, in the same `Promise.all` as the
// board and the status list. Its failure mode is the reason this is worth
// porting carefully: the client does
// `try { const d = await r.json(); if (d && typeof d === 'object') sessionGates = d; } catch {}`
// — and a 404 body `{"error":"not found"}` IS an object, so it was assigned
// wholesale. Every `sessionGates[worker][status]` lookup then missed, and the
// user's per-worker gates rendered as if they had been DELETED. A 404 that
// deserializes into the success path is not a missing endpoint, it is a silent
// data-loss illusion (ethos rule 4: the wrong answer must be visible).

/// GET /api/board/session-gates -> `{session: {status: [criteria]}}`.
///
/// Empty gates are dropped rather than returned as `[]`: a missing
/// (session, status) MEANS "inherit the global default for that status", and
/// an empty array would read as "this worker has an override that requires
/// nothing" — the opposite of inheritance.
async fn list_session_gates(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut out: Map<String, Value> = Map::new();
    // Python wrapped the SELECT in a bare `except: return {}` — the table is
    // absent on a fresh DB and an empty map is the honest answer there.
    if let Ok(mut stmt) = conn.prepare("SELECT session, status, gate FROM session_gates") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        }) {
            for (session, status, gate) in rows.flatten() {
                let items: Vec<String> = gate
                    .as_deref()
                    .and_then(|g| serde_json::from_str(g).ok())
                    .unwrap_or_default();
                if items.is_empty() {
                    continue;
                }
                out.entry(session)
                    .or_insert_with(|| Value::Object(Map::new()))
                    .as_object_mut()
                    .expect("just inserted an object")
                    .insert(status, json!(items));
            }
        }
    }
    Json(Value::Object(out)).into_response()
}

/// PATCH /api/board/session-gates {session|worker, status, gate[]} -> {ok:true}.
///
/// Accepts BOTH spellings of the key on purpose. Python read `session`; the
/// shipped SPA has always sent `worker` (app.js `editSessionGate`), so the
/// python endpoint would have answered 400 to its own dashboard. Rather than
/// re-ship that mismatch, take either — `worker` is the alias `aliases.rs`
/// already maps to `session` everywhere else in this API.
async fn patch_session_gates(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let key = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let (Some(session), Some(status)) = (key("session").or_else(|| key("worker")), key("status"))
    else {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "missing session or status"}),
        );
    };
    let items: Vec<String> = body
        .get("gate")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| match x {
                    Value::String(s) => s.trim().to_string(),
                    other => other.to_string().trim().to_string(),
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let write = state
        .store
        .write_async(move |conn| {
            if items.is_empty() {
                // Empty -> revert this session to the global default for that
                // status. Deleting the row is what "inherit" means here; an
                // empty-array row would be an override requiring nothing.
                conn.execute(
                    "DELETE FROM session_gates WHERE session = ?1 AND status = ?2",
                    rusqlite::params![session, status],
                )?;
            } else {
                conn.execute(
                    "INSERT INTO session_gates (session, status, gate) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(session, status) DO UPDATE SET gate = excluded.gate",
                    rusqlite::params![
                        session,
                        status,
                        serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
                    ],
                )?;
            }
            Ok(WriteOutcome {
                applied: true,
                // Board-flavoured so the SSE tick makes open dashboards refetch
                // — python called `_board_changed()` here for the same reason.
                events: vec![PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: format!("session_gates:{session}:{status}"),
                    mutation: MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- board status (column) mutations -------------------------------------
//
// Python parity, amux-server.py:69484-69560 + 69209 (reorder). The PATCH was
// the live 405 Ethan hit editing a column (request_log target
// /api/board/statuses/review, 2026-08-09) — GET was ported for AMUX-2596 and
// the mutation verbs were not.

/// POST /api/board/statuses {label} -> 201 {id,label} (py:69484).
async fn create_status(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let label = body
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if label.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "missing label" }));
    }
    // Python: slugify, then -2..-19 suffix on collision.
    let mut sid: String = label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(30)
        .collect();
    sid = sid.trim_matches('-').to_string();
    if sid.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "invalid label" }));
    }
    let out: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let out_w = out.clone();
    let label_w = label.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let existing: Vec<String> = conn
                .prepare("SELECT id FROM statuses")?
                .query_map([], |r| r.get::<_, String>(0))?
                .filter_map(Result::ok)
                .collect();
            let mut final_id = sid.clone();
            if existing.contains(&final_id) {
                for i in 2..20 {
                    let candidate = format!("{sid}-{i}");
                    if !existing.contains(&candidate) {
                        final_id = candidate;
                        break;
                    }
                }
            }
            let max_pos: i64 = conn.query_row(
                "SELECT COALESCE(MAX(position),0) FROM statuses",
                [],
                |r| r.get(0),
            )?;
            conn.execute(
                "INSERT OR IGNORE INTO statuses (id, label, position, is_builtin) VALUES (?, ?, ?, 0)",
                rusqlite::params![final_id, label_w, max_pos + 1],
            )?;
            *out_w.lock().expect("status slot poisoned") = Some(final_id.clone());
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: format!("statuses:{final_id}"),
                    mutation: MutationKind::Created,
                    payload: None,
                }],
            })
        })
        .await;
    if let Err(e) = write {
        return internal(e);
    }
    let sid = out.lock().expect("status slot poisoned").take().unwrap_or_default();
    (StatusCode::CREATED, Json(json!({ "id": sid, "label": label }))).into_response()
}

/// PATCH /api/board/statuses/{sid} {label?, gate?} -> {ok:true} (py:69550).
async fn patch_status(
    State(state): State<AppState>,
    Path(sid): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let label = body
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    // Python: "gate" present -> list of non-empty strings, else NULL.
    let gate_update = body.get("gate").map(|g| {
        let items: Vec<String> = g
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| match x {
                        Value::String(s) => s.trim().to_string(),
                        other => other.to_string().trim().to_string(),
                    })
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if items.is_empty() {
            None
        } else {
            serde_json::to_string(&items).ok()
        }
    });
    let sid_w = sid.clone();
    let write = state
        .store
        .write_async(move |conn| {
            if let Some(l) = &label {
                conn.execute(
                    "UPDATE statuses SET label = ? WHERE id = ?",
                    rusqlite::params![l, sid_w],
                )?;
            }
            if let Some(g) = &gate_update {
                conn.execute(
                    // gate_custom=1: a person edited this column's gate, so
                    // enforcement must honour it over the type default
                    // (AMUX-2641). Without the flag a stale seed row is
                    // indistinguishable from operator intent.
                    "UPDATE statuses SET gate = ?, gate_custom = 1 WHERE id = ?",
                    rusqlite::params![g, sid_w],
                )?;
            }
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: format!("statuses:{sid_w}"),
                    mutation: MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

/// DELETE /api/board/statuses/{sid} — refuse builtins; audit the bulk
/// status rewrite onto every moved card (AMUX-2491: a column delete used to
/// leave no trace) -> {ok, moved, ids[:50]} (py:69512-69549).
async fn delete_status(
    State(state): State<AppState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Response {
    const BUILTINS: [&str; 7] =
        ["backlog", "todo", "doing", "review", "done", "verified", "discarded"];
    if BUILTINS.contains(&sid.as_str()) {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "cannot delete built-in status" }),
        );
    }
    let (_, actor_name) = actor_from_headers(&headers);
    let actor = if actor_name == "api-anonymous" { "human".to_string() } else { actor_name };
    let out: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    let out_w = out.clone();
    let sid_w = sid.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let moved: Vec<String> = conn
                .prepare("SELECT id FROM issues WHERE status = ?1 AND deleted IS NULL")?
                .query_map([&sid_w], |r| r.get::<_, String>(0))?
                .filter_map(Result::ok)
                .collect();
            let mut events = Vec::new();
            for card in &moved {
                let log_line = format!(
                    "status: {sid_w} -> todo (column '{sid_w}' deleted by {actor})"
                );
                let result = crate::db::advance::advance(
                    conn,
                    card,
                    "todo",
                    &actor,
                    &crate::db::advance::AdvanceOpts {
                        force: true,
                        expected_from: Some(sid_w.clone()),
                        log_line: Some(log_line),
                        skip_continuation: true,
                        ..crate::db::advance::AdvanceOpts::default()
                    },
                )?;
                if let Ok(outcome) = result {
                    events.extend(outcome.events);
                }
            }
            conn.execute(
                "DELETE FROM statuses WHERE id = ?1 AND is_builtin = 0",
                [&sid_w],
            )?;
            *out_w.lock().expect("status slot poisoned") = Some(moved);
            Ok(WriteOutcome { applied: true, events })
        })
        .await;
    if let Err(e) = write {
        return internal(e);
    }
    let moved = out.lock().expect("status slot poisoned").take().unwrap_or_default();
    Json(json!({
        "ok": true,
        "moved": moved.len(),
        "ids": moved.iter().take(50).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// PUT /api/board/statuses/reorder {order:[ids]} -> {ok:true} (py:69210).
async fn reorder_statuses(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(order) = body.get("order").and_then(Value::as_array).filter(|a| !a.is_empty())
    else {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "missing order" }));
    };
    let ids: Vec<String> = order
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let write = state
        .store
        .write_async(move |conn| {
            for (pos, sid) in ids.iter().enumerate() {
                conn.execute(
                    "UPDATE statuses SET position = ?1 WHERE id = ?2",
                    rusqlite::params![pos as i64, sid],
                )?;
            }
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: EntityType::Task,
                    entity_id: "statuses:reorder".into(),
                    mutation: MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- shared helpers ------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn scoped_board_forbidden(scope: &super::org::MemberScope, resource: &str) -> Response {
    err(
        StatusCode::FORBIDDEN,
        json!({
            "error": "outside local member access scope",
            "scope_level": scope.level(),
            "scope_name": scope.name(),
            "resource": resource,
        }),
    )
}

use super::internal;

fn not_found(id: &str) -> Response {
    err(
        StatusCode::NOT_FOUND,
        json!({ "error": "item not found", "id": id }),
    )
}

fn no_write() -> WriteOutcome {
    WriteOutcome {
        applied: false,
        events: Vec::new(),
    }
}

/// Task event carrying the post-mutation snapshot (RR-0111a). Every board
/// event site has the freshly written row in hand inside the same write
/// closure, so the snapshot is one serialization — never a re-read — and the
/// journal can replay board state without consulting the live table.
fn ev_snap(row: &IssueRow, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Task,
        entity_id: row.id.clone(),
        mutation,
        payload: Some(row.snapshot()),
    }
}

fn finish<T>(
    slot: &Mutex<Option<T>>,
    outcome: T,
    write: WriteOutcome,
) -> rusqlite::Result<WriteOutcome> {
    *slot.lock().expect("outcome slot poisoned") = Some(outcome);
    Ok(write)
}

/// AMUX-3391: fold the silent auto-capture card into a worker's own card.
///
/// Every prompt is auto-captured as a `doing` card (creator=amux, desc
/// `**Prompt:**`, minted `notified=1` so the worker is never TOLD it exists).
/// The worker then follows the ledger rule and cards its OWN work — two cards
/// for one prompt. Measured live: 68% of capture cards were being discarded by
/// hand as duplicates of the worker's own card (the PRIMI-152/153 shape, 72
/// such pairs in 14 days). This reconciles at the write that already happens
/// (CLAUDE.md's recorded event decision — not a sweep, not a model call): when a
/// worker cards its work and a FRESH capture card is still open for its lane,
/// discard the capture in place as an audit tombstone linked to the new card, so
/// there is exactly one card and nothing to clean up.
///
/// Returns `(folded_capture_id, its SSE event)`, or `None` when nothing folds.
/// Pure over `conn` so the create handler stays readable and this is unit-tested
/// against an in-memory DB (see the tests below).
fn fold_capture_for_worker_card(
    conn: &rusqlite::Connection,
    new: &IssueRow,
    window_s: i64,
    now: i64,
) -> rusqlite::Result<Option<(String, PendingEvent)>> {
    // A capture card is minted by amux, owned by an agent, and its desc begins
    // with the `**Prompt:**` marker. Only a genuine WORKER card (not amux, not a
    // capture) triggers a fold — and it must not itself be a capture.
    let is_worker_card = !new.creator.is_empty()
        && new.creator != "amux"
        && !new.creator.starts_with("amux ") // "amux (claimed …)" is the capture actor
        && new.owner_type == "agent"
        && !new.desc.starts_with("**Prompt:**");
    if !is_worker_card {
        return Ok(None);
    }
    let Some(sess) = new.session.as_deref().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let cutoff = now - window_s;
    // The most-recent open capture card for this lane that NO earlier worker card
    // has already claimed. A worker card (other than the one just inserted)
    // created at/after the capture means this new card is a 2nd/unrelated one for
    // the lane — leave the capture alone rather than mis-fold an unrelated task.
    let cap: Option<(String, i64)> = conn
        .query_row(
            "SELECT c.id, c.rev FROM issues c \
             WHERE c.session = ?1 AND c.creator = 'amux' \
               AND c.owner_type = 'agent' AND c.status = 'doing' \
               AND substr(c.\"desc\", 1, 11) = '**Prompt:**' \
               AND c.deleted IS NULL AND c.created > ?2 \
               AND NOT EXISTS ( \
                 SELECT 1 FROM issues w WHERE w.session = c.session \
                   AND w.owner_type = 'agent' AND w.creator <> 'amux' \
                   AND w.deleted IS NULL AND w.id <> ?3 \
                   AND w.created >= c.created ) \
             ORDER BY c.created DESC LIMIT 1",
            rusqlite::params![sess, cutoff, new.id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((cap_id, cap_rev)) = cap else {
        return Ok(None);
    };
    use crate::db::advance::{self as adv, AdvanceOpts};
    let result = adv::advance(
        conn,
        &cap_id,
        "discarded",
        sess,
        &AdvanceOpts {
            force: true,
            expected_from: Some("doing".into()),
            // AF-616: THIS PATH GUESSED THE TARGET, so say so. The query
            // below picks the most recent unclaimed capture for the lane, which
            // means "the first card since the message" and not "this card is
            // about that message". The declared path (`--folded-into`, where a
            // lane names its own target) writes the same line WITHOUT this
            // marker, so a reader can tell an assertion from an inference.
            log_line: Some(format!(
                "capture folded into {} {}",
                new.id,
                bs::FOLD_INFERRED_MARKER
            )),
            skip_continuation: true,
            ..AdvanceOpts::default()
        },
    )?;
    let Ok(outcome) = result else {
        return Ok(None);
    };
    // advance() handled status + version + log + save_patched. Update desc
    // and rev (Python optimistic-concurrency counter) separately.
    conn.execute(
        "UPDATE issues SET \"desc\" = ?1, rev = ?2 WHERE id = ?3 AND deleted IS NULL",
        rusqlite::params![
            format!(
                "{}\n\n_Folded into {} — the worker carded this work directly (AMUX-3391)._",
                outcome.row.desc, new.id
            ),
            cap_rev + 1,
            cap_id,
        ],
    )?;
    tracing::info!(
        session = %sess,
        capture = %cap_id,
        folded_into = %new.id,
        "ledger: capture folded into worker card (AMUX-3391)"
    );
    let event = outcome.events.into_iter().next().unwrap_or_else(|| {
        let snap = bs::get_issue(conn, &cap_id)
            .ok()
            .flatten()
            .map(|r| r.snapshot());
        PendingEvent {
            entity_type: amux_core::revision::EntityType::Task,
            entity_id: cap_id.clone(),
            mutation: MutationKind::Updated,
            payload: snap,
        }
    });
    Ok(Some((cap_id, event)))
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Local HH:MM, matching Python's `time.strftime("%H:%M")` log stamps.
fn hhmm() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// The verified caller identity from the attribution header (AMUX-1768:
/// provenance is the header, never body text). Returns (core actor, log display
/// name). No worker registry lookup exists yet, so a named caller maps to
/// `Actor::System{component: <name>}` — honest about being unverified-as-a-
/// Worker while still carrying the name into every audit line.
///
/// BOTH SPELLINGS, and this module was the only one that took just one (AC-322).
/// `X-Amux-Worker` is canonical and `X-Amux-Session` is still honored — the rule
/// every other module already implements via [`crate::api::groups::hdr_worker`]
/// (groups, session_verbs, schedules, email, alerts, git_guard). board.rs read
/// `x-amux-session` alone, and the installed `amux` CLI is the bash script,
/// whose 14 board-path PATCH sites all send `X-Amux-Worker`. So a correctly
/// attributed CLI call was byte-identical to an anonymous one HERE and nowhere
/// else, which broke two things at once:
///
///   1. `amux board <status> --force` was unwalkable. The force check below
///      refuses `api-anonymous`, so the sanctioned CLI could not satisfy the
///      attribution requirement force demands, and its own error told the caller
///      to "use the CLI" — which is what they had done. That is ethos rule 6
///      exactly: a constraint whose sanctioned escape is unwalkable from the
///      audited path gets walked from an unaudited one (a hand-rolled curl,
///      which is where unattributed writes come from in the first place).
///   2. The cross-lane ARCHIVE guard (AMUX-2492) was blind to every bash-CLI
///      caller. `caller_lane` derives from this same name, so it was empty for
///      all of them, and an empty caller_lane disables the guard — meaning the
///      guard that stops one lane archiving another lane's card has been open
///      for the entire installed-CLI population, silently.
///
/// Fixed at the seam rather than at the CLI's call sites: one resolver here
/// fixes every already-installed CLI copy at once, and closes both effects
/// together, whereas patching curl lines fixes only the machines that upgrade.
fn actor_from_headers(headers: &HeaderMap) -> (Actor, String) {
    match super::org::local_member_actor(headers)
        .map(str::to_string)
        .or_else(|| Some(crate::api::groups::hdr_worker(headers)))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(name) => (
            Actor::System {
                component: name.to_string(),
            },
            name.to_string(),
        ),
        None => (
            Actor::System {
                component: "api-anonymous".into(),
            },
            "api-anonymous".into(),
        ),
    }
}

// ---- body shapes ---------------------------------------------------------

/// Full detail body: everything, full `desc`, full `log` (L1: the full desc
/// is never in a LIST payload; it is always here). Delegates to
/// [`IssueRow::snapshot`] — the SAME serialization the event journal records
/// as each mutation's payload (RR-0111a), so API body, journal payload, and
/// replay verification can never drift apart.
/// Designate a card whose owning lane is ISOLATED (AMUX-3713, Ethan: "the card
/// should have a designation that it is").
///
/// An isolated worker is a raw agent with the harness stripped: hidden from the
/// peer fleet list and refused as a peer send target. Its CARDS were exempt from
/// all of that — `desktop` owns 25 and not one said so — so a peer reading the
/// board saw an ordinary card with an ordinary session name and no indication
/// that the owning lane is undiscoverable and cannot be messaged. Same shape as
/// AMUX-2796: work routed to a lane that cannot receive it.
///
/// IN A HELPER BOTH PATHS CALL, because the first version lived in `list_body`
/// and the single-card GET does not go through it — `get_item` calls
/// `detail_body` directly. The test drove `list_body(row, slim=false)` and
/// passed while the live detail endpoint returned nothing, which is ethos rule
/// 7's wrong-layer failure exactly: a real property asserted in a place the
/// shipped request does not flow through. Verifying against the running server
/// is what caught it.
///
/// Resolved through `session_is_isolated`, the same predicate the fleet filter
/// and the send guard consult, so a card cannot claim a reachability the send
/// path disagrees with.
/// Publish whether this card still claims live work, BESIDE the fields a
/// consumer would otherwise derive it from (AF-555).
///
/// A row ships 48 fields including `status` and `archived` and, until this,
/// nothing that answered the question every consumer actually asks. So every
/// consumer derived it, and a derivation nobody publishes is a derivation that
/// diverges. gtm-engine's `is_active_card` read status and never `archived`,
/// so an archived card in a dispatchable status suppressed a hand-raiser
/// breach page while reaching nobody — board_drive excludes archived from
/// dispatch and every closing verb refuses it, so the alarm went quiet because
/// of a card that can be neither worked nor closed. 785 cards are archived
/// while holding a non-terminal status, so the ambiguity is not rare.
///
/// `live_reason` is here because a bare boolean cannot say WHY, and "false"
/// with no reason is the shape a consumer misreads next. It names which of the
/// four rules fired.
///
/// Rides through slim on purpose: it is NOT in `SLIM_OMITS`, because a
/// liveness verdict that disappears from the list payload is worse than one
/// that was never there — `row.get("live")` returning None reads as an answer.
fn designate_live_state(obj: &mut serde_json::Map<String, Value>, row: &IssueRow) {
    let (live, reason) = live_state(row);
    obj.insert("live".into(), json!(live));
    obj.insert("live_reason".into(), json!(reason));
}

/// The four rules, in order. Exported for tests and for any in-process caller
/// that would otherwise write a fifth copy of this.
pub(crate) fn live_state(row: &IssueRow) -> (bool, &'static str) {
    // NO `deleted` ARM: `IssueRow` carries no such field, because a deleted row
    // is filtered before it ever reaches serialization. Checked rather than
    // assumed — writing one would have compiled against a field that is not
    // there, which is how you get a rule that reads as covered and never runs.
    let Some(st) = crate::db::board_store::parse_status(&row.status) else {
        // An unparseable status is NOT reported as dead. A card whose status we
        // cannot read is a card we cannot speak for, and answering "not live"
        // would be a measurement that never ran wearing the clothes of one that
        // did (ethos rule 4).
        return (true, "status_unparseable");
    };
    if !st.claims_live_work() {
        return (false, "status_claims_no_work");
    }
    if row.archived != 0 && !st.survives_archive() {
        return (false, "archived");
    }
    if row.archived != 0 {
        return (true, "archived_but_ask_still_owed");
    }
    (true, "live")
}

fn designate_owner_reach(obj: &mut serde_json::Map<String, Value>, row: &IssueRow) {
    if !row.session.as_deref().is_some_and(crate::api::session_verbs::session_is_isolated) {
        // Absent rather than `false`: this is a rare property and a key on every
        // one of 1700+ cards saying "normal" is payload for nothing.
        return;
    }
    obj.insert("owner_isolated".into(), json!(true));
    // The MEANING, not just the flag. A bare boolean makes every consumer
    // re-derive what isolation implies, and they will not agree.
    obj.insert(
        "owner_reach".into(),
        json!("isolated (raw agent): not in the peer fleet list and refused as a peer send target. Reachable only by the owner from the dashboard — do not route this card to a peer expecting them to message the owner."),
    );
}

fn detail_body(row: &IssueRow) -> Value {
    let mut v = row.snapshot();
    // HERE, not in `list_body`: this is the function `get_item` calls for the
    // single-card GET, and `list_body`'s non-slim branch calls it too — so one
    // insertion covers both shipped paths (AMUX-3713).
    if let Some(obj) = v.as_object_mut() {
        designate_owner_reach(obj, row);
        // Same reasoning, same place: the single-card GET must carry the
        // liveness verdict too, or a consumer that fetches one card gets a
        // different contract from one that lists.
        designate_live_state(obj, row);
    }
    v
}

/// Result of one durable callback outbox pass.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CallbackDispatch {
    pub attempted: usize,
    pub queued: usize,
    pub refused: usize,
}

/// Drain terminal task callbacks through the same durable steering path as all
/// other harness messages. `only` is used by the interactive PATCH path for an
/// immediate response; the board-drive tick passes None and recovers any item
/// left pending by a crash or by a non-HTTP transition producer.
pub(crate) async fn dispatch_pending_callbacks(
    state: &AppState,
    only: Option<&str>,
) -> CallbackDispatch {
    let ids: Vec<String> = match state.store.read() {
        Ok(conn) => {
            if let Some(id) = only {
                conn.query_row(
                    "SELECT id FROM issues WHERE id=?1 AND deleted IS NULL \
                     AND callback_state IN ('pending','dispatching')",
                    rusqlite::params![id],
                    |r| r.get(0),
                )
                .map(|id| vec![id])
                .unwrap_or_default()
            } else {
                conn.prepare(
                    "SELECT id FROM issues WHERE deleted IS NULL \
                     AND callback_state IN ('pending','dispatching') ORDER BY updated LIMIT 100",
                )
                .and_then(|mut st| {
                    st.query_map([], |r| r.get::<_, String>(0))
                        .map(|rows| rows.flatten().collect())
                })
                .unwrap_or_default()
            }
        }
        Err(_) => Vec::new(),
    };
    let mut report = CallbackDispatch::default();
    for id in ids {
        report.attempted += 1;
        let stable_id = format!("task-callback-{id}");
        let staged: Arc<Mutex<Option<IssueRow>>> = Arc::new(Mutex::new(None));
        let staged_w = staged.clone();
        let id_w = id.clone();
        let stable_w = stable_id.clone();
        let _ = state
            .store
            .write_async(move |conn| {
                let Some(mut row) = bs::get_issue(conn, &id_w)? else {
                    return Ok(no_write());
                };
                if !matches!(row.callback_state.as_deref(), Some("pending" | "dispatching")) {
                    return Ok(no_write());
                }
                if !bs::dependency_is_resolved(&row.status, &row.item_type) && row.status != "discarded" {
                    tracing::warn!(marker = "dependency_callback_not_resolved", task_id = %row.id,
                        status = %row.status, measured = true, n_considered = 1,
                        "withholding pending callback because dependency no longer resolves");
                    row.callback_state = Some("armed".into());
                    row.callback_error = Some("Waiting for verified dependency resolution".into());
                    row.rev += 1;
                    row.version += 1;
                    bs::save_patched(conn, &mut row)?;
                    return Ok(WriteOutcome { applied: true, events: vec![ev_snap(&row, MutationKind::Updated)] });
                }
                row.callback_state = Some("dispatching".into());
                row.callback_message_id = Some(stable_w);
                row.callback_error = None;
                row.updated = now_secs();
                row.rev += 1;
                row.version += 1;
                bs::save_patched(conn, &mut row)?;
                *staged_w.lock().unwrap() = Some(row.clone());
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![ev_snap(&row, MutationKind::Updated)],
                })
            })
            .await;
        let Some(row) = staged.lock().ok().and_then(|mut g| g.take()) else {
            continue;
        };
        let Some(target) = row.callback_session.clone() else {
            continue;
        };
        let sender = row.session.clone().unwrap_or_else(|| "board".into());
        // Derive the return address from the durable dependency graph, never
        // from remembered prose. A callback must resume the requester's task,
        // not make the producer's child its new current-task context.
        let parents: Vec<IssueRow> = match state.store.read().and_then(|conn| {
            Ok(bs::list_issues(&conn, &[], std::slice::from_ref(&target), bs::ArchivedFilter::ActiveOnly)?)
        }) {
            Ok(rows) => rows.into_iter()
                .filter(|parent| parent.depends_on.contains(&row.id) && !bs::is_terminal_status(&parent.status))
                .collect(),
            Err(error) => {
                tracing::warn!(marker = "dependency_callback_graph_unmeasured", task_id = %row.id,
                    %error, measured = false, n_considered = 0,
                    "callback awaits a successful graph lookup before delivery");
                continue;
            }
        };
        let context_card = if parents.len() == 1 { parents[0].id.clone() } else { row.id.clone() };
        let outcome = row
            .last_result
            .as_deref()
            .or(row.evidence.as_deref())
            .unwrap_or("The complete action log and produced assets are on the task card.");
        // A FOLD IS NOT AN ABANDONMENT. `capture folded into <ID>` means the
        // lane did the right thing: amux auto-captured an inbound routed
        // message as a card, they carded the real work properly, and discarded
        // the empty shell. Telling the ROUTING lane that their peer "closed the
        // request without resolving the dependency" puts a false accusation in
        // front of the one party who will act on it.
        let folded = bs::folded_into_detail(row.log.as_deref());
        let folded_note;
        let resolution = if let Some((target, inferred)) = folded.as_ref() {
            // AF-616: an INFERRED target was chosen by adjacency and nothing
            // compared it to the capture. Saying so costs one clause and is the
            // difference between a fact and a guess for the lane reading this.
            folded_note = if *inferred {
                format!(
                    "folded this capture into {target} (target inferred from timing, \
                     not declared -- confirm it is about the capture)"
                )
            } else {
                format!("folded this capture into {target}")
            };
            folded_note.as_str()
        } else if bs::dependency_is_resolved(&row.status, &row.item_type) {
            "resolved the dependency"
        } else if bs::is_capture_shell(&row) && row.status == "discarded" {
            // AF-634. The reader of this sentence is the SENDER of a message,
            // and "closed the request without resolving the dependency" tells
            // them a request they never made was dropped. ts-gke received 19 of
            // these in a night and nearly enumerated all of them before seeing
            // the shape. Nothing about DELIVERY changes here; only the claim.
            "discarded the capture of a message you sent, which is not a request and owed you nothing"
        } else {
            "closed the request without resolving the dependency"
        };
        let mut prompt = format!(
            "[task callback {}: {}] {} {}. \
             State: {}.\nOutcome: {}\nOpen board card {} for verification evidence, action history, \
             source message, and produced assets.",
            row.id, row.title, sender, resolution, row.status, outcome, row.id
        );
        if let Ok(conn) = state.store.read() {
            for parent in &parents {
                let blocking = crate::runtime_jobs::board_drive::deps_blocking(&conn, parent);
                prompt.push_str(&format!("\nOriginal task {} — {}. Next action: {}.",
                    parent.id, parent.title, parent.next_action.as_deref().unwrap_or("Read the original task and continue its work")));
                if blocking.is_empty() {
                    prompt.push_str(" All linked dependencies are resolved. Re-read and resume this original task now through its normal board claim. Keep the links as history; they no longer block it.");
                } else {
                    prompt.push_str(&format!(" Still blocked by: {}. Do not claim it until these dependencies resolve.", blocking.join(", ")));
                }
            }
        }
        let instruction = row.callback_prompt.as_deref().map(str::trim).filter(|s| !s.is_empty());
        // Older auto-captures stored this instruction on the RETURN message,
        // addressed to the requester who was already being notified. Gemini
        // turned those receipts into repeated reviews and new capture tasks.
        let legacy_echo = instruction == Some("Notify the requesting worker with the terminal outcome and every produced asset.");
        if legacy_echo {
            tracing::warn!(task_id = %row.id, target_session = %target, measured = true,
                n_considered = 1, verdict = "callback_echo_instruction_suppressed",
                "legacy callback asked its recipient to repeat the notification");
        }
        if let Some(instruction) = instruction.filter(|_| !legacy_echo) {
            prompt.push_str("\nCallback instruction: ");
            prompt.push_str(instruction);
        } else {
            prompt.push_str("\nThis receipt already notifies you as the requester. Do not acknowledge or relay it back, or create another task merely to process this notification. Resume your original linked task only when it has remaining actionable work.");
        }
        let guard = format!("task-callback:{}", row.id);
        // Re-resolve policy at delivery time. The request was authorized when
        // it was created, but a later explicit opt-out must not be bypassed by
        // a callback that happens to have been armed earlier. A refusal remains
        // visible on the card and can be retried only by an explicit re-arm.
        let delivered: Result<String, String> =
            match crate::api::session_verbs::cross_group_send_ok(&sender, &target) {
                Err(reason) => Err(reason),
                Ok(_) => crate::api::session_verbs::steer_enqueue_idempotent(
                    state,
                    &target,
                    &prompt,
                    &guard,
                    &sender,
                    &stable_id,
                )
                .await
                .map_err(str::to_string),
            };
        let id_w = row.id.clone();
        let target_w = target.clone();
        let sender_w = sender.clone();
        let prompt_w = prompt.clone();
        let stable_w = stable_id.clone();
        let parents_w: Vec<String> = parents.iter().map(|parent| parent.id.clone()).collect();
        let context_w = context_card.clone();
        match delivered {
            Ok(_) => {
                report.queued += 1;
                let _ = state.store.write_async(move |conn| {
                    let Some(mut latest) = bs::get_issue(conn, &id_w)? else {
                        return Ok(no_write());
                    };
                    if latest.callback_message_id.as_deref() != Some(stable_w.as_str()) {
                        return Ok(no_write());
                    }
                    let now = now_secs();
                    latest.callback_state = Some("queued".into());
                    latest.callback_fired_at = Some(now);
                    latest.callback_error = None;
                    latest.updated = now;
                    latest.rev += 1;
                    latest.version += 1;
                    latest.log = Some(bs::append_log(
                        latest.log.as_deref(),
                        &chrono::Local::now().format("%H:%M").to_string(),
                        &format!("terminal callback queued to {target_w} as {stable_w}"),
                    ));
                    bs::save_patched(conn, &mut latest)?;
                    let exists = conn.query_row(
                        "SELECT 1 FROM cmd_history WHERE session=?1 AND type='task-callback' AND text LIKE ?2 LIMIT 1",
                        rusqlite::params![target_w, format!("[task callback {id_w}:%")], |_| Ok(true),
                    ).unwrap_or(false);
                    let mut events = vec![ev_snap(&latest, MutationKind::Updated)];
                    if !exists {
                        conn.execute(
                            "INSERT INTO cmd_history \
                             (text,type,session,ts,origin,card_id,delivery,queued_at) \
                             VALUES (?1,'task-callback',?2,?3,?4,?5,'queued',?3)",
                            rusqlite::params![prompt_w, target_w, now * 1000, sender_w, context_w],
                        )?;
                        let message_id = conn.last_insert_rowid();
                        for parent_id in &parents_w {
                            let Some(mut parent) = bs::get_issue(conn, parent_id)? else { continue; };
                            if !parent.depends_on.contains(&id_w) { continue; }
                            parent.log = Some(bs::append_log(parent.log.as_deref(), &hhmm(),
                                &format!("dependency {id_w} reported {} by {sender_w}; callback {stable_w}, message MSG-{message_id} completion indicator queued for this task", latest.status)));
                            parent.updated = now;
                            parent.rev += 1;
                            parent.version += 1;
                            bs::save_patched(conn, &mut parent)?;
                            events.push(ev_snap(&parent, MutationKind::Updated));
                        }
                        tracing::info!(marker = "dependency_callback_linked", task_id = %id_w,
                            original_task = %context_w, message_id, measured = true,
                            n_considered = parents_w.len(), "completion callback linked to its original work");
                        events.push(crate::db::PendingEvent {
                            entity_type: amux_core::revision::EntityType::Message,
                            entity_id: format!("MSG-{message_id}"),
                            mutation: MutationKind::Created,
                            payload: None,
                        });
                    }
                    Ok(WriteOutcome { applied: true, events })
                }).await;
            }
            Err(reason) => {
                report.refused += 1;
                let reason = reason.to_string();
                let _ = state.store.write_async(move |conn| {
                    let Some(mut latest) = bs::get_issue(conn, &id_w)? else {
                        return Ok(no_write());
                    };
                    latest.callback_state = Some("refused".into());
                    latest.callback_error = Some(reason.clone());
                    latest.updated = now_secs();
                    latest.rev += 1;
                    latest.version += 1;
                    latest.log = Some(bs::append_log(
                        latest.log.as_deref(),
                        &chrono::Local::now().format("%H:%M").to_string(),
                        &format!("terminal callback to {target_w} refused: {reason}"),
                    ));
                    bs::save_patched(conn, &mut latest)?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![ev_snap(&latest, MutationKind::Updated)],
                    })
                }).await;
            }
        }
    }
    report
}

#[cfg(test)]
mod callback_dispatch_tests {
    use super::*;

    fn state(home: &std::path::Path) -> AppState {
        let store = std::sync::Arc::new(
            crate::db::Store::open(&home.join("callback-test.db")).expect("open store"),
        );
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "callback-test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn pending_request(state: &AppState) -> String {
        request_at(state, "verified")
    }

    fn request_at(state: &AppState, status: &str) -> String {
        let status = status.to_owned();
        let new = bs::NewIssue {
            title: "Produce the launch report".into(),
            desc: "Requested by another worker.".into(),
            status: "todo".into(),
            session: Some("worker-b".into()),
            item_type: "code".into(),
            creator: "worker-a".into(),
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
            source: Some("agent".into()),
            requested_by: Some("worker-a".into()),
            callback_session: Some("worker-a".into()),
            callback_prompt: Some("Start the dependent launch step.".into()),
        };
        let id = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let id_w = id.clone();
        state.store.write(move |conn| {
            let mut row = bs::create_issue(conn, &new, 1000)?;
            row.status = status;
            row.last_result = Some("Report written to /tmp/launch-report.md".into());
            row.updated = 2000;
            row.rev += 1;
            row.version += 1;
            bs::save_patched(conn, &mut row)?;
            *id_w.lock().unwrap() = row.id;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).expect("create and complete request");
        let created_id = id.lock().unwrap().clone();
        created_id
    }

    /// Code completion has two edges: done records implementation, verified
    /// releases the graph and its durable return message. Drive the real store,
    /// dispatcher and steering outbox, including recovery, as one scenario.
    #[tokio::test(flavor = "current_thread")]
    async fn verified_dependency_returns_to_original_task_once() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::api::settings::test_env::set_home(home.path());
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        for worker in ["worker-a", "worker-b"] {
            std::fs::write(home.path().join(format!("sessions/{worker}.env")), "CC_TAGS=\"test\"\n").unwrap();
        }
        let state = state(home.path());
        let id = request_at(&state, "done");
        let child = id.clone();
        state.store.write(move |conn| {
            conn.execute("INSERT INTO issues(id,title,status,session,type,owner_type,created,updated,depends_on,next_action) \
                VALUES ('P-1','Integrate the verified report','todo','worker-a','chore','agent',1,1,?1,'Integrate the returned report into the final output')",
                [serde_json::to_string(&vec![child]).unwrap()])?;
            Ok(crate::db::WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        {
            let conn = state.store.read().unwrap();
            let parent = bs::get_issue(&conn, "P-1").unwrap().unwrap();
            assert_eq!(crate::runtime_jobs::board_drive::deps_blocking(&conn, &parent), vec![id.clone()]);
            assert_eq!(bs::get_issue(&conn, &id).unwrap().unwrap().callback_state.as_deref(), Some("armed"));
        }
        assert_eq!(dispatch_pending_callbacks(&state, Some(&id)).await.attempted, 0,
            "done without verification must neither notify nor resume the original worker");
        let child = id.clone();
        state.store.write(move |conn| {
            let mut row = bs::get_issue(conn, &child)?.unwrap();
            row.status = "verified".into(); row.updated = 3000; row.rev += 1; row.version += 1;
            row.evidence = Some("python verify_report.py -> PASS: report content checked independently".into());
            bs::save_patched(conn, &mut row)?;
            Ok(crate::db::WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        let delivered = dispatch_pending_callbacks(&state, Some(&id)).await;
        assert_eq!((delivered.attempted, delivered.queued, delivered.refused), (1, 1, 0));
        {
            let conn = state.store.read().unwrap();
            let parent = bs::get_issue(&conn, "P-1").unwrap().unwrap();
            assert!(crate::runtime_jobs::board_drive::deps_blocking(&conn, &parent).is_empty());
            assert_eq!(parent.depends_on, vec![id.clone()], "keep the linkage as durable history");
            assert!(parent.log.unwrap().contains(&format!("dependency {id} reported verified by worker-b")));
            let (text, card, origin): (String, String, String) = conn.query_row(
                "SELECT text,card_id,origin FROM cmd_history WHERE session='worker-a' AND type='task-callback'",
                [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(card, "P-1", "the worker's current task must remain the original task");
            assert_eq!(origin, "worker-b");
            assert!(text.contains("Original task P-1"));
            assert!(text.contains("All linked dependencies are resolved"));
            assert!(text.contains("Integrate the returned report"));
        }
        let child = id.clone();
        state.store.write(move |conn| {
            conn.execute("UPDATE issues SET callback_state='dispatching' WHERE id=?1",[child])?;
            Ok(crate::db::WriteOutcome {applied:true,events:vec![]})
        }).unwrap();
        assert_eq!(dispatch_pending_callbacks(&state, Some(&id)).await.queued, 1);
        let conn = state.store.read().unwrap();
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM steering_queue WHERE id=?1",
            [format!("task-callback-{id}")], |r| r.get::<_, i64>(0)).unwrap(), 1);
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM cmd_history WHERE card_id='P-1' AND type='task-callback'",
            [], |r| r.get::<_, i64>(0)).unwrap(), 1, "recovery cannot duplicate the linked message");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reopened_dependency_cannot_dispatch_a_stale_completion() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::api::settings::test_env::set_home(home.path());
        let state = state(home.path());
        let id = pending_request(&state);
        let child = id.clone();
        state.store.write(move |conn| {
            let mut row = bs::get_issue(conn, &child)?.unwrap();
            row.status = "doing".into();
            bs::save_patched(conn, &mut row)?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let result = dispatch_pending_callbacks(&state, Some(&id)).await;
        assert_eq!((result.attempted, result.queued), (1, 0));
        let conn = state.store.read().unwrap();
        assert_eq!(bs::get_issue(&conn, &id).unwrap().unwrap().callback_state.as_deref(), Some("armed"));
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM steering_queue", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn legacy_callback_receipt_does_not_request_an_acknowledgement_loop() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::api::settings::test_env::set_home(home.path());
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        for worker in ["worker-a", "worker-b"] {
            std::fs::write(home.path().join(format!("sessions/{worker}.env")), "CC_TAGS=\"test\"\n").unwrap();
        }
        let state = state(home.path());
        let id = pending_request(&state);
        let child = id.clone();
        state.store.write(move |conn| {
            conn.execute("UPDATE issues SET callback_prompt=?1 WHERE id=?2",
                rusqlite::params!["Notify the requesting worker with the terminal outcome and every produced asset.", child])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        let result = dispatch_pending_callbacks(&state, Some(&id)).await;
        assert_eq!((result.attempted, result.queued), (1, 1));
        let conn = state.store.read().unwrap();
        let prompt: String = conn.query_row("SELECT text FROM steering_queue WHERE id=?1",
            [format!("task-callback-{id}")], |r| r.get(0)).unwrap();
        assert!(prompt.contains("Report written to /tmp/launch-report.md"));
        assert!(prompt.contains("Do not acknowledge or relay it back"));
        assert!(!prompt.contains("Callback instruction:"));
        assert!(!prompt.contains("Notify the requesting worker"));
    }

    /// Happy path plus the crash window: enqueue succeeds, but the process dies
    /// before the card is marked queued. Retrying the `dispatching` row must
    /// reuse the stable queue id and the one linked Messages record.
    #[tokio::test(flavor = "current_thread")]
    async fn callback_delivery_is_board_linked_and_crash_idempotent() {
        let home = tempfile::tempdir().expect("home");
        let _home_guard = crate::api::settings::test_env::set_home(home.path());
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        std::fs::write(home.path().join("sessions/worker-a.env"), "CC_TAGS=\"a\"\n").unwrap();
        std::fs::write(home.path().join("sessions/worker-b.env"), "CC_TAGS=\"b\"\n").unwrap();
        let state = state(home.path());
        let id = pending_request(&state);

        let first = dispatch_pending_callbacks(&state, Some(&id)).await;
        assert_eq!((first.attempted, first.queued, first.refused), (1, 1, 0));
        {
            let conn = state.store.read().unwrap();
            let row = bs::get_issue(&conn, &id).unwrap().unwrap();
            assert_eq!(row.callback_state.as_deref(), Some("queued"));
            assert_eq!(row.callback_message_id.as_deref(), Some(format!("task-callback-{id}").as_str()));
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM steering_queue WHERE id=?1",
                    rusqlite::params![format!("task-callback-{id}")],
                    |r| r.get::<_, i64>(0),
                ).unwrap(),
                1
            );
            assert_eq!(
                conn.query_row(
                    "SELECT COUNT(*) FROM cmd_history WHERE card_id=?1 AND type='task-callback'",
                    rusqlite::params![id],
                    |r| r.get::<_, i64>(0),
                ).unwrap(),
                1
            );
        }

        // Simulate a crash after enqueue but before finalization. Mutations go
        // through the one writer, just like production.
        let id_w = id.clone();
        state.store.write(move |conn| {
            conn.execute(
                "UPDATE issues SET callback_state='dispatching' WHERE id=?1",
                rusqlite::params![id_w],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        }).unwrap();

        let recovered = dispatch_pending_callbacks(&state, Some(&id)).await;
        assert_eq!((recovered.attempted, recovered.queued, recovered.refused), (1, 1, 0));
        let conn = state.store.read().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM steering_queue WHERE id=?1",
                rusqlite::params![format!("task-callback-{id}")],
                |r| r.get::<_, i64>(0),
            ).unwrap(),
            1,
            "recovery must refresh the stable row, never duplicate it"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM cmd_history WHERE card_id=?1 AND type='task-callback'",
                rusqlite::params![id],
                |r| r.get::<_, i64>(0),
            ).unwrap(),
            1,
            "the Messages link is one callback, not one per retry"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn callback_to_an_isolated_worker_is_visibly_refused() {
        let home = tempfile::tempdir().expect("home");
        let _home_guard = crate::api::settings::test_env::set_home(home.path());
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        std::fs::write(
            home.path().join("sessions/worker-a.env"),
            "CC_TAGS=\"a\"\nCC_ISOLATED=1\n",
        ).unwrap();
        std::fs::write(home.path().join("sessions/worker-b.env"), "CC_TAGS=\"b\"\n").unwrap();
        let state = state(home.path());
        let id = pending_request(&state);

        let report = dispatch_pending_callbacks(&state, Some(&id)).await;
        assert_eq!((report.attempted, report.queued, report.refused), (1, 0, 1));
        let conn = state.store.read().unwrap();
        let row = bs::get_issue(&conn, &id).unwrap().unwrap();
        assert_eq!(row.callback_state.as_deref(), Some("refused"));
        assert!(row.callback_error.as_deref().unwrap_or("").contains("isolated"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM steering_queue", [], |r| r.get::<_, i64>(0)).unwrap(),
            0,
            "a refusal must not leave an immortal queued callback"
        );
    }
}

/// List body, Python-parity (AMUX-2586 fix #4). The plain list serves the
/// FULL `desc` and FULL `log` exactly as Python's `_load_board` does — the
/// SPA renders `item.desc` and reads `item.log` (the folded badge) straight
/// off the LIST payload, so the earlier L1 slimming (first-line desc,
/// `log_n` instead of `log`) silently blanked both in the dashboard.
/// `slim=1` stays the payload diet, matching Python `_board_project`: drop
/// desc/log, add `desc_len` + `log_n`. `stale` mirrors Python's
/// `_board_item_stale` flag — set ONLY when true, on both paths (Python's
/// `_BOARD_SLIM_DROP` is `("desc","log")`; `stale` rides through slim).
pub fn list_body(row: &IssueRow, slim: bool, stale: bool) -> Value {
    // The slim base never allocates the prose it will not ship (AMUX-3496):
    // this used to build the FULL snapshot (cloning desc+log, 6MB+ across a
    // live list) and then delete both keys. The derivations below read
    // row.desc/row.log by reference.
    let mut v = if slim { row.snapshot_slim() } else { detail_body(row) };
    let obj = v.as_object_mut().expect("snapshot is an object");
    // Only the SLIM branch needs this here: the non-slim branch got it inside
    // `detail_body` above, which is also the function the single-card GET calls.
    if slim {
        designate_owner_reach(obj, row);
    }
    // BOTH paths, unlike designate_owner_reach above: the full branch gets its
    // owner-reach fields inside detail_body, but liveness is inserted here so
    // the list and the single-card GET cannot drift (detail_body carries its
    // own call for the single-card route).
    designate_live_state(obj, row);
    // (see designate_owner_reach for why this exists)
    if slim {
        // AF-346: `desc` may be a bounded PREFIX here. The two derivations
        // that cannot be recomputed from one arrive beside it, from SQL; the
        // `None` arm is what every non-prefixed row and every hand-built test
        // row takes, unchanged. Deliberately NOT `unwrap_or_else(compute from
        // the prefix)` — that fallback would return a smaller number that looks
        // exactly like a real one, which is the failure a99955f7 shipped.
        let desc_len = match &row.desc_prefixed {
            Some(p) => p.desc_len,
            None => row.desc.chars().count(),
        };
        obj.insert("desc_len".into(), json!(desc_len));
        let log_n = row
            .log
            .as_deref()
            .map(|l| l.lines().filter(|x| !x.trim().is_empty()).count())
            .unwrap_or(0);
        obj.insert("log_n".into(), json!(log_n));

        // SHIP THE DERIVED FACTS, NOT THE RAW FIELDS (AMUX-2840).
        //
        // The comment above records that an earlier slimming attempt "silently
        // blanked both in the dashboard", because the SPA reads `desc` and
        // `log` straight off the LIST payload. It does — but not for their
        // content. It needs exactly two things from them in a list:
        //   app.js:19488  the first line of desc, as the card preview
        //   app.js:18866  whether desc+log contain "New task:", for the folded badge
        // Both are tiny derivations over fields that together are 81% of a
        // 4.7MB response. Computing them here costs bytes in the low hundreds
        // and lets the client stop carrying 3.5MB of prose it never renders.
        //
        // Full-text SEARCH is the third consumer and is deliberately NOT served
        // here: /api/search already indexes these cards and returns ranked hits
        // with snippets, so shipping every desc to re-implement it client-side
        // is duplicated work in the expensive direction.
        let head: String = row
            .desc
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        obj.insert("desc_head".into(), json!(head));
        let folded_n = match &row.desc_prefixed {
            Some(p) => p.folded_n,
            None => {
                row.desc.matches("New task:").count()
                    + row.log.as_deref().map(|l| l.matches("New task:").count()).unwrap_or(0)
            }
        };
        obj.insert("folded_n".into(), json!(folded_n));

        // The third derivation the list makes over desc+log (app.js:19231): the
        // LAST "NEEDS-YOU:" marker, which is what a card shows when it is
        // waiting on a human. Last rather than first — a re-marked card should
        // show its freshest question, which is the client's own rule.
        let ny = {
            let hay = format!("{}\n{}", row.desc, row.log.as_deref().unwrap_or(""));
            let mut found: Option<String> = None;
            for line in hay.lines() {
                let l = line.trim();
                let low = l.to_lowercase();
                // EVERY SPELLING THE CLIENT REGEX ACCEPTS, or the two disagree
                // about the same card. app.js's _focusAsk uses
                // /NEEDS[- ]?(?:YOU|ETHAN|HUMAN):/i, which admits the space and
                // no-separator forms for ETHAN and HUMAN too — this list had
                // only the hyphenated ones, so a card marked "NEEDS ETHAN:"
                // produced a note in the client and none here. Under slim the
                // client reads THIS field, so the divergence would have shown
                // up as the marker silently ceasing to work for those spellings
                // the moment the poll flipped.
                for m in [
                    "needs-you:", "needs you:", "needsyou:",
                    "needs-ethan:", "needs ethan:", "needsethan:",
                    "needs-human:", "needs human:", "needshuman:",
                ] {
                    if let Some(p) = low.find(m) {
                        let v = l[p + m.len()..].trim();
                        if !v.is_empty() {
                            found = Some(v.chars().take(400).collect());
                        }
                    }
                }
            }
            found
        };
        if let Some(n) = ny {
            obj.insert("needsyou_note".into(), json!(n));
        }

        // Detail-only fields the list never renders. The SPA fetches the
        // full card on demand when the detail panel opens, so these are
        // pure payload waste on the list/SSE path. Keeps depends_on
        // (is:blocked filter) and folded_n (is:folded filter).
        //
        // `reviewer` USED TO BE IN THIS LIST AND IS NOT ANY MORE (AF-161).
        // The justification above reasons from ONE consumer — "the SPA never
        // renders it" — which is true, and false for every other caller. It
        // cost a real wrong answer on 2026-08-23: amux-frustrations audited
        // their verified cards off this payload and reported 25 of 25 with no
        // reviewer. The true figure was 7 named / 18 absent. `.get("reviewer")`
        // returns None for an ABSENT key exactly as it does for an empty one,
        // so a removal here is indistinguishable from a card with no reviewer,
        // and the census was 100% wrong in the direction that looks like a
        // finding. It is one short, usually-null string per row against a
        // 4.5MB payload, and it is load-bearing for the one audit anybody runs
        // over this table. The other four stay dropped: `gate` alone is four
        // criteria strings per row.
        //
        // The prose drops were always SELF-DESCRIBING — desc_head/desc_len/
        // log_n ship in their place, so a consumer can see the omission. These
        // five were removed with nothing left behind, which is why the same
        // discovery has now been made twice, one column at a time (c207339
        // fixed the caller for `desc`). `slim: 1` below is the general remedy:
        // a consumer can refuse a slim row instead of reading absence as
        // emptiness, and `GET /api/board?describe` names exactly what is gone.
        for k in SLIM_OMITS {
            obj.remove(k);
        }
        // SAY THAT THIS ROW IS SLIM. Ten bytes against ~4.5MB, and it is the
        // only thing that lets a caller tell "the server did not send this"
        // from "the card does not have one" without knowing the drop list by
        // heart. The AMUX-3496 comment argues the KeyError on `.desc` is
        // "loud, not silently empty" — true in the idiom it assumes
        // (`row["desc"]`) and false in the one every consumer actually
        // writes (`row.get("desc")`), which returns None and says nothing.
        // `slim` NAMES WHAT IS GONE (AF-200). It used to serialize as `1`, which
        // says something was omitted and not what — so a consumer still had to
        // know the drop list by heart, which is the thing this marker exists to
        // make unnecessary. Two of the six drops shipped a companion (desc ->
        // desc_head/desc_len, log -> log_n) and four shipped no signal at all,
        // including `gate`, which governs transitions, and `last_verified_at`,
        // which is what a `verified` audit reads.
        //
        // Cost of the bare boolean, measured on 2026-08-24: reading `desc` off
        // this payload returned None for a card carrying 1809 characters, and
        // the conclusion drawn was that `amux board add --desc-file` was
        // silently dropping bodies. Three probe cards were filed bisecting a CLI
        // defect that does not exist, one step away from "fixing" a flag that
        // works. That is AF-161's own predicted next occurrence — its entry ends
        // by asking for a payload self-describing about what it omits "rather
        // than restoring one column and waiting for the next report".
        //
        // An array is truthy exactly where `1` was, and nothing tests the value:
        // the SPA detects slim via `items[0].desc !== undefined` (app.js:22577).
        obj.insert("slim".into(), json!(SLIM_OMITS));
    }
    if stale {
        obj.insert("stale".into(), json!(true));
    }
    v
}

/// Python `_board_item_stale` (amux-server.py:15671): an in-progress card
/// whose owning session is not actively working and that nobody has touched
/// for 30 minutes. `working` is the derived active-session set — the SAME
/// derivation the session list serves, so the two views cannot disagree.
pub fn is_stale(row: &IssueRow, now: i64, working: &std::collections::BTreeSet<String>) -> bool {
    if !matches!(row.status.as_str(), "doing" | "review") {
        return false;
    }
    let Some(sess) = row.session.as_deref().filter(|s| !s.is_empty()) else {
        return false;
    };
    if row.updated == 0 || now - row.updated < 1800 {
        return false;
    }
    !working.contains(sess)
}

// ---- GET /api/board ------------------------------------------------------

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub archived: Option<String>,
    #[serde(default)]
    pub done_limit: Option<i64>,
    // `?all=1` — the "give me everything" escape. Every session that hit the
    // terminal cap reached for this exact param (AMUX-3154: mixpeek-funnel,
    // mixpeek-frustrations, ts-gke all tried `?all=1`/`?limit=10000` and got the
    // capped 100-terminal view back, silently). It was an UNRECOGNISED param, so
    // axum dropped it and the default cap answered — the rule-7 failure where a
    // filter that never ran hands back a confident wrong denominator (a lane
    // auditing its `done` work off the plain list was reading ~6% of it). Now
    // recognised and honoured as `done_limit=0` (uncap terminal). The dashboard
    // render poll keeps the cap by NOT sending this; a denominator read asks for
    // it explicitly.
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub slim: Option<String>,
    // `?full=1` — the prose escape (AMUX-3496). The DEFAULT list is now
    // slim-shaped: 1,657 cards carried 5.4MB of desc + 0.7MB of log to
    // consumers that render three fields, and every derived fact the list
    // actually needs (desc_head, desc_len, log_n, folded_n, needsyou_note)
    // already ships. A reader that needs the prose says so; `.desc` on a
    // default row is now a KeyError, which is loud, not silently empty.
    // Explicit `slim=0` is honored as full for legacy callers.
    #[serde(default)]
    pub full: Option<String>,
    // `?quota=1` (AMUX-3503) — per-status terminal quotas instead of the
    // lumped cap: verified keeps its own 300-floor so a bulk-verify stays
    // visible, done/discarded share done_limit. These were the SSE board
    // push's semantics; the dashboard poll asks for them here now that it
    // renders from the fetch path and the full-push is retired.
    #[serde(default)]
    pub quota: Option<String>,
    // `?count=1` (AMUX-3715) — how many rows this filter selects, WITHOUT
    // shipping them.
    //
    // For the dashboard's collapsed "Archived (N)" header. Measured before
    // adding it: the archived set is 445KB raw / 87KB gzipped, a 38% increase
    // on the board poll, for a section that is collapsed by default — too much
    // to pay on every load of a mobile-first dashboard just to render a number.
    //
    // It runs the SAME filter+cap path the list runs and counts what that
    // returns, so the header cannot disagree with what expanding it shows
    // (ethos rule 1: a view must share the predicate of the mechanism it
    // describes). A count computed by its own SELECT would drift the moment
    // either changed.
    #[serde(default)]
    pub count: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    // SEARCH-INTENT PARAMS, RECOGNISED ONLY TO BE REFUSED (2026-08-11).
    //
    // axum drops unknown query params silently, so `/api/board?q=nudge`
    // returned THE WHOLE BOARD — 1382 rows that look exactly like search
    // results. That is the failure mode ethos rule 7 names: a filter that
    // silently matches everything hands you a confident wrong answer instead of
    // silence, and nothing about the response prompts a recheck.
    //
    // It cost a real one here: two different queries returned byte-identical
    // lists, and only comparing them by accident revealed the param was inert.
    // One query alone reads as "no such card exists" — which is how a duplicate
    // gets filed against a board that already had the card.
    //
    // Not silently honoured either, because /api/search is the real one and it
    // returns a different (ranked, typed) shape. Naming it in a 400 routes the
    // caller to the working endpoint, the same way the gate 409 publishes its
    // escape. Nothing in the SPA or CLI sends these — verified before making
    // them loud — so this cannot break a client holding a stale service worker.
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
}

/// Query keys GET /api/board actually consumes. Anything else is dropped
/// SILENTLY by axum's typed `Query<ListParams>` — which is how `?include_archived=1`
/// or a mistyped filter returned the DEFAULT view served as if it answered the
/// query (BACKE-3228; the ethos rule-7 class — a filter that never ran hands back
/// a confident wrong answer). It bit three sessions: amux-cloud read 13 archived
/// cards as 0, ts-gke brute-forced the id space, backend reported a working guard
/// as broken. A blanket 400 on unknown params is unsafe (cache-busters like
/// `?_=<ts>` are legitimate and any client may append them), so instead we NAME
/// the ignored ones in a response header + a WARN — non-breaking, and it makes
/// the silent drop impossible to miss the way `X-Amux-Done-Limit` already
/// announces the terminal cap. (`q`/`query`/`search` are here because they are
/// consumed above — refused with a 400 — so they are recognised, not ignored.)
const RECOGNISED_BOARD_PARAMS: &[&str] = &[
    "status", "session", "archived", "done_limit", "all", "slim", "full", "quota", "count", "limit", "offset", "q", "query",
    "search",
];
/// Cache-buster keys clients legitimately append; never a filter typo, so they
/// are not surfaced as "ignored" (that would be pure noise on every polled tab).
pub(crate) const BENIGN_QUERY_KEYS: &[&str] =
    &["_", "t", "v", "ts", "cb", "_t", "cache", "cachebust", "nocache"];

/// Query keys GET /api/board neither consumes nor treats as a benign
/// cache-buster — the ones a caller thinks are filtering but that did nothing.
/// Pure over the raw query so it is tested without an HTTP round-trip.
fn ignored_board_params(raw_query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for pair in raw_query.split('&') {
        let key = pair.split('=').next().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }
        let k = key.to_ascii_lowercase();
        if RECOGNISED_BOARD_PARAMS.contains(&k.as_str()) || BENIGN_QUERY_KEYS.contains(&k.as_str())
        {
            continue;
        }
        if !out.iter().any(|e| e.eq_ignore_ascii_case(key)) {
            out.push(key.to_string());
        }
    }
    out
}

fn qp_truthy(v: Option<&str>) -> bool {
    // Python lowercases before the membership test (`.lower() in ("1",
    // "true","yes")`), so `slim=TRUE` counts.
    matches!(
        v.map(|s| s.trim().to_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Bare JSON ARRAY (the Python dashboard parses exactly that shape). The
/// terminal cap ALWAYS announces itself via the header quartet the Python
/// server emits (`X-Amux-Done-Limit`/`-Truncated`/`-Terminal-Total`/
/// `-Terminal-Returned`) — a silent cap manufactured wrong absence claims
/// twice in one week (AC-291, AC-301), so the two counts come from
/// `cap_terminal` itself, never re-derived from list lengths.
/// Who tripped the terminal-cap warning? (AEAB-54)
///
/// Returns `(user_agent, session)`, both always printable so the log line has no
/// empty fields. Split out from the warn site so the FALLBACKS are testable —
/// an attribution that silently degrades to blanks is the failure it exists to
/// prevent.
///
/// The UA is capped at 60 chars: a browser UA is ~130 and would push the useful
/// part of the line off the end, while the discriminating prefix
/// (`curl/8.7.1`, `Mozilla/5.0 (iPhone...`) is in the first few.
fn truncation_caller(headers: &HeaderMap) -> (String, String) {
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| v.chars().take(60).collect::<String>())
        .unwrap_or_else(|| "(none)".into());
    let sess = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("(unattributed)")
        .to_string();
    (ua, sess)
}

#[cfg(test)]
mod truncation_caller_tests {
    use super::truncation_caller;
    use axum::http::HeaderMap;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    /// THE CASE THAT MOTIVATED THIS. Every one of the 39 calls that tripped the
    /// gate in 24h on 2026-08-24 was `curl/8.7.1` with NO session header — the
    /// gate selects for ad-hoc consumers, and an ad-hoc consumer is exactly the
    /// one that never sets attribution. So the UA has to carry the answer, and
    /// the missing session must read as a stated fact rather than a blank.
    #[test]
    fn an_unattributed_curl_is_still_identified_by_its_user_agent() {
        let (ua, sess) = truncation_caller(&h(&[("user-agent", "curl/8.7.1")]));
        assert_eq!(ua, "curl/8.7.1");
        assert_eq!(sess, "(unattributed)", "an absent session must not render as empty");
    }

    /// A worker tripping this is the serious case — the one worth chasing — so
    /// the session must survive when it IS present.
    #[test]
    fn a_session_is_reported_when_present() {
        let (_, sess) = truncation_caller(&h(&[
            ("user-agent", "curl/8.7.1"),
            ("x-amux-session", "amux-errors-and-bugs"),
        ]));
        assert_eq!(sess, "amux-errors-and-bugs");
    }

    /// Neither field may ever come back empty: an empty field in a structured
    /// log reads as "no value recorded", which is indistinguishable from the
    /// attribution never having been added at all.
    #[test]
    fn nothing_is_ever_blank_even_with_no_headers() {
        let (ua, sess) = truncation_caller(&HeaderMap::new());
        assert_eq!(ua, "(none)");
        assert_eq!(sess, "(unattributed)");
        // Present-but-empty is a distinct path from absent, and both must be
        // handled — a client CAN send `X-Amux-Session:` with no value.
        let (ua, sess) = truncation_caller(&h(&[("user-agent", ""), ("x-amux-session", "")]));
        assert_eq!(ua, "(none)");
        assert_eq!(sess, "(unattributed)");
    }

    /// The cap keeps the line readable. A real iPhone Safari UA is ~130 chars
    /// and would push `hidden=`/`terminal_total=` off the end of the line.
    #[test]
    fn a_long_user_agent_is_capped_but_keeps_its_discriminating_prefix() {
        let long = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 \
                    (KHTML, like Gecko) Version/26.6 Mobile/15E148 Safari/604.1";
        let (ua, _) = truncation_caller(&h(&[("user-agent", long)]));
        assert_eq!(ua.chars().count(), 60);
        assert!(ua.starts_with("Mozilla/5.0 (iPhone"), "the prefix is what identifies it: {ua}");
    }
}

/// `GET /api/board/export?format=md|json[&worker=][&status=][&archived=]`
///
/// # Why this is a SERVER endpoint and not more client code (AMUX-3868)
///
/// The dashboard already exports the view you are looking at (AMUX-3873), and
/// it cannot ever be a FULL export: `GET /api/board` omits `desc` entirely and
/// sends `desc_head` + `desc_len` instead, so the browser does not hold the
/// text. Getting it client-side means one request per card, which at the
/// current board is over 1,500.
///
/// `IssueRow.desc` is the complete string, so reading it here costs one query.
///
/// SCOPE IS STATED IN THE OUTPUT, never implied by its absence. A scoped export
/// that does not say it was scoped is indistinguishable from a whole-board one,
/// and it is the version that gets pasted somewhere as evidence. Both formats
/// carry the filters that ran and the count, and the unscoped case says so in
/// its own words rather than by staying quiet (ethos rule 4).
#[derive(serde::Deserialize, Default)]
pub struct ExportParams {
    format: Option<String>,
    worker: Option<String>,
    status: Option<String>,
    archived: Option<String>,
}

pub async fn export_board(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(p): Query<ExportParams>,
) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let statuses: Vec<String> = p
        .status
        .as_deref()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    let mut workers: Vec<String> = p
        .worker
        .as_deref()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    if let Some(scope) = super::org::local_member_scope(&headers).filter(|scope| !scope.is_global()) {
        if workers.iter().any(|worker| !scope.allows_worker(worker)) {
            return scoped_board_forbidden(&scope, "board export");
        }
        if workers.is_empty() {
            workers = super::org::scoped_worker_names(&scope);
            if workers.is_empty() {
                workers.push("__amux_no_permitted_worker__".into());
            }
        }
    }
    // Default ActiveOnly: an export is a working document, and silently
    // including archived cards would overstate the board. `archived=all`
    // opts in, and the header below always says which was used.
    let arch = match p.archived.as_deref().unwrap_or("") {
        "all" => ArchivedFilter::All,
        "1" | "true" | "yes" | "only" => ArchivedFilter::ArchivedOnly,
        _ => ArchivedFilter::ActiveOnly,
    };
    let rows = match bs::list_issues(&conn, &statuses, &workers, arch) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    drop(conn);

    let scope = {
        let mut parts: Vec<String> = Vec::new();
        if !workers.is_empty() {
            parts.push(format!("worker = {}", workers.join(", ")));
        }
        if !statuses.is_empty() {
            parts.push(format!("status = {}", statuses.join(", ")));
        }
        match arch {
            ArchivedFilter::All => parts.push("including archived".into()),
            ArchivedFilter::ArchivedOnly => parts.push("archived only".into()),
            ArchivedFilter::ActiveOnly => {}
        }
        parts
    };
    let stamp = chrono::Local::now().format("%Y-%m-%d").to_string();
    let want_md = matches!(p.format.as_deref(), Some("md") | Some("markdown"));

    let (body, mime, ext) = if want_md {
        let mut md = String::new();
        md.push_str("# amux board\n\n");
        md.push_str(&format!(
            "_{} issue(s) · full descriptions · exported {}_\n\n",
            rows.len(),
            chrono::Local::now().format("%Y-%m-%d %H:%M")
        ));
        if scope.is_empty() {
            md.push_str("> Whole board: every active issue, no worker or status filter.\n");
        } else {
            md.push_str(&format!(
                "> **Scoped export.** {}. This is not the whole board.\n",
                scope.join(" · ")
            ));
        }
        // Grouped by status, the order the board itself reads in.
        let order = [
            "doing", "review", "todo", "backlog", "done", "verified", "discarded",
        ];
        let mut seen: Vec<String> = Vec::new();
        for s in order.iter() {
            seen.push((*s).to_string());
        }
        for r in &rows {
            if !seen.iter().any(|s| s == &r.status) {
                seen.push(r.status.clone());
            }
        }
        for st in seen {
            let group: Vec<&IssueRow> = rows.iter().filter(|r| r.status == st).collect();
            if group.is_empty() {
                continue;
            }
            md.push_str(&format!("\n## {} ({})\n\n", st, group.len()));
            for r in group {
                md.push_str(&format!("### {} — {}\n\n", r.id, r.title));
                md.push_str(&format!(
                    "- worker: {} · type: {} · created: {}\n",
                    r.session.as_deref().unwrap_or("—"),
                    r.item_type,
                    r.created
                ));
                if let Some(g) = r.gate.as_deref().filter(|g| !g.is_empty()) {
                    md.push_str(&format!("- gate: {g}\n"));
                }
                if !r.depends_on.is_empty() {
                    md.push_str(&format!("- depends_on: {}\n", r.depends_on.join(", ")));
                }
                // The whole point of this endpoint: the COMPLETE description,
                // not a head. No truncation note, because nothing is truncated.
                if !r.desc.trim().is_empty() {
                    md.push_str(&format!("\n{}\n", r.desc.trim()));
                }
                md.push('\n');
            }
        }
        (md, "text/markdown; charset=utf-8", "md")
    } else {
        let issues: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.id, "title": r.title, "desc": r.desc, "status": r.status,
                    "session": r.session, "type": r.item_type, "creator": r.creator,
                    "gate": r.gate, "depends_on": r.depends_on, "reviewer": r.reviewer,
                    "created": r.created, "updated": r.updated, "archived": r.archived,
                    "owner_type": r.owner_type, "pinned": r.pinned,
                })
            })
            .collect();
        let v = json!({
            "exported_at": chrono::Local::now().to_rfc3339(),
            "count": issues.len(),
            "scoped": !scope.is_empty(),
            "scope": scope,
            // Said explicitly so nobody has to compare this against /api/board
            // to discover the difference.
            "desc": "complete — unlike GET /api/board, which sends desc_head/desc_len only",
            "issues": issues,
        });
        (
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into()),
            "application/json; charset=utf-8",
            "json",
        )
    };

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, mime.to_string()),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"amux-board-{stamp}.{ext}\""),
            ),
        ],
        body,
    )
        .into_response()
}

pub async fn list_board(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw_query: axum::extract::RawQuery,
    Query(p): Query<ListParams>,
) -> Response {
    // BACKE-3228: name query params we silently dropped, so a caller cannot draw
    // an absence conclusion from a filter that never ran. Computed from the RAW
    // query (typed ListParams cannot see keys it does not declare).
    let ignored = raw_query.0.as_deref().map(ignored_board_params).unwrap_or_default();
    if !ignored.is_empty() {
        tracing::warn!(
            target: "board",
            ignored = %ignored.join(","),
            "GET /api/board ignored unrecognised query param(s) — caller may be reading a \
             default view as a filtered answer (BACKE-3228)"
        );
    }
    // ETag based on global_rev — saves 3.5MB on unchanged polls.
    let rev = state.store.current_rev().map(|r| r.0).unwrap_or(0);
    let member_scope = super::org::local_member_scope(&headers);
    let scope_etag = member_scope
        .as_ref()
        .map(|scope| {
            use sha2::Digest as _;
            let digest = sha2::Sha256::digest(
                format!("{}\0{}", scope.level(), scope.name()).as_bytes(),
            );
            format!("-{}", &hex::encode(digest)[..12])
        })
        .unwrap_or_default();
    let etag_val = format!("\"board-{}{scope_etag}\"", rev);
    if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
        if inm == etag_val || inm == format!("W/{etag_val}") {
            let mut h = HeaderMap::new();
            if let Ok(v) = etag_val.parse() {
                h.insert("etag", v);
            }
            return (StatusCode::NOT_MODIFIED, h).into_response();
        }
    }

    if let Some(term) = p.q.as_deref().or(p.query.as_deref()).or(p.search.as_deref()) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "ok": false,
                "error": "/api/board does not search — it would have returned the ENTIRE board",
                "you_sent": term,
                "use_instead": format!("/api/search?q={term}"),
                "why": "This param was silently ignored until 2026-08-11, so the full board came \
                        back looking like ranked results. Refusing loudly beats answering wrongly.",
                "board_filters": ["status", "session", "archived", "done_limit", "all", "slim", "full", "quota", "count", "limit", "offset"],
            })),
        )
            .into_response();
    }
    let split = |s: &Option<String>| -> Vec<String> {
        s.as_deref()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect()
    };
    let status_f = split(&p.status);
    let mut session_f = split(&p.session);
    if let Some(scope) = member_scope.as_ref().filter(|scope| !scope.is_global()) {
        if session_f.iter().any(|worker| !scope.allows_worker(worker)) {
            return scoped_board_forbidden(scope, "board list");
        }
        if session_f.is_empty() {
            session_f = super::org::scoped_worker_names(scope);
            if session_f.is_empty() {
                // An empty SQL filter means "all sessions", so use an
                // impossible sentinel when a group currently has no workers.
                session_f.push("__amux_no_permitted_worker__".into());
            }
        }
    }
    // `archived` grammar (amux-server.py:68758 + 14025, ported on AMUX-2586 fix #5):
    //   "1"/"true"/"yes"          -> archived-only
    //   any OTHER non-empty value -> non-archived only ("0", "false", "all", "2", ...)
    //   absent or ""              -> scope-dependent (see below)
    //
    // SCOPE-DEPENDENT default (AMUX-3086 / AMUX-3107). A SCOPED list (status= or
    // session=) with `archived` absent now defaults to ActiveOnly, so the view
    // agrees with the mutation guard: an archived card is immutable
    // (amux-core/board.rs:570), and agent cleanup loops were building discard
    // candidates from `?session=X&status=done`, then PATCHing {status:discarded}
    // on the ~42 archived cards it mixed in, drawing 409 archived_task_immutable
    // (ethos rule 1: a view must share the predicate of the mechanism it
    // describes). The UNSCOPED bare list stays All: the SPA text-search full fetch
    // (?done_limit=0) relies on archived cards being in the corpus, and
    // board_api.rs pins that the bare list still includes them.
    let scoped = !status_f.is_empty() || !session_f.is_empty();
    let archived = match p.archived.as_deref().map(|s| s.to_lowercase()) {
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes") => ArchivedFilter::ArchivedOnly,
        Some(v) if !v.is_empty() => ArchivedFilter::ActiveOnly,
        _ if scoped => ArchivedFilter::ActiveOnly,
        _ => ArchivedFilter::All,
    };
    // <=0 is uncapped inside cap_terminal, matching Python's `_cap_terminal`.
    //
    // A SCOPED QUERY IS NOT CAPPED BY DEFAULT (ts-gke, 2026-08-11).
    //
    // The cap exists so the UNFILTERED board payload stays renderable — the
    // dashboard does not draw 1300 terminal cards. But `?session=X` or
    // `?status=done` is a bounded QUESTION, and answering it with the 100
    // most-recent terminal rows produces a confident wrong number with nothing
    // in the body to say so.
    //
    // Measured on the report: ts-gke holds 174 terminal cards (94 done, 60
    // verified, 20 discarded). Capping to the 100 most-recently-updated left 68
    // that happened to be `done`, so `?session=ts-gke` answered 68 where the
    // truth is 94 — and a digest built on it reported 25. Four cards named in
    // that digest were absent from the list while GET /api/board/<id> returned
    // them fine: same store, two endpoints, different answers.
    //
    // The truncation WAS reported, in x-amux-truncated / x-amux-terminal-total
    // headers. That is ethos rule 4's second layer: a tag in a store the reader
    // never opens is the same failure as no tag. Every consumer here reads
    // `curl | json.load`, which sees a bare array and no headers at all.
    //
    // An explicit ?done_limit= still wins in both cases — a caller who asks for
    // a bound gets exactly that.
    let scoped = p.session.is_some() || p.status.is_some();
    // `?all=1` uncaps the terminal set for the unscoped list — the documented,
    // now-discoverable escape from the render cap (AMUX-3154). An explicit
    // `?done_limit=N` still wins over it (a caller who names a bound gets it).
    let uncap_all = qp_truthy(p.all.as_deref());
    let done_limit = p
        .done_limit
        .unwrap_or(if scoped || uncap_all { 0 } else { 100 });
    // Shape resolution (AMUX-3496): explicit wins, default is slim.
    //   ?full=1          -> full rows (prose included)
    //   ?slim=1          -> slim (unchanged, the dashboard poll)
    //   ?slim=0          -> full (legacy spelling of "not slim", honored)
    //   neither          -> slim — the default a phone, a CLI, or an ad-hoc
    //                       curl gets is the small one (mobile-first).
    let slim = if qp_truthy(p.full.as_deref()) {
        false
    } else {
        match p.slim.as_deref() {
            Some(s) => qp_truthy(Some(s)),
            None => true,
        }
    };

    // AF-346: a slim response ships DERIVATIONS of desc, never desc, so it does
    // not need the whole string. `Full` is not a fallback here, it is the shape
    // `?full=1` / `?slim=0` asked for, and it is what every other caller of
    // these two functions still gets.
    let prose = if slim { bs::Prose::SlimDerivations } else { bs::Prose::Full };
    let quota = qp_truthy(p.quota.as_deref());
    let store = state.store.clone();
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        // Fused filter+cap with lazy hydration (AMUX-3491): the old
        // list_issues + cap_terminal pair decoded every undeleted row's
        // desc+log (~27MB of prose) to ship the ~20% that survive the cap.
        // The doing/review probe below is equivalent on the capped set —
        // those statuses are never terminal, so the cap cannot drop them.
        // ?quota=1 swaps the lumped cap for per-status quotas (AMUX-3503);
        // quota has no single truncation count, so it reports (0, 0) and
        // the truncation warn/headers stay silent for it.
        let (kept, term_total, term_kept) = if quota {
            (
                bs::list_issues_quota(
                    &conn,
                    &status_f,
                    &session_f,
                    archived,
                    done_limit.max(0) as usize,
                    prose,
                )?,
                0,
                0,
            )
        } else {
            bs::list_issues_capped(&conn, &status_f, &session_f, archived, done_limit, prose)?
        };
        // The `stale` flag needs the active-session set only when an
        // in-progress card is present (Python computes it in `_load_board`).
        let working = if kept
            .iter()
            .any(|r| matches!(r.status.as_str(), "doing" | "review"))
        {
            crate::api::sessions_legacy::active_python_sessions(&conn)
        } else {
            Default::default()
        };
        Ok((kept, term_total, term_kept, working))
    })
    .await;
    let (kept, term_total, term_kept, working) = match joined {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let total = kept.len();
    let now = now_secs();

    // COUNT-ONLY (AMUX-3715). Placed HERE, after the same filter+cap that
    // produces `kept`, so the number is literally the length of what the list
    // would have shipped — not a second SELECT that could drift from it.
    //
    // Returns an OBJECT while the list returns a bare array, which is
    // deliberate: a consumer that forgets `count=1` is in its URL gets a shape
    // error rather than a plausible-looking one-element array, and a client
    // reading `.length` on `{"count": 662}` gets undefined rather than 1.
    if qp_truthy(p.count.as_deref()) {
        return Json(json!({
            "count": total,
            "filter": {
                "status": p.status.clone(),
                "session": p.session.clone(),
                "archived": p.archived.clone(),
                "done_limit": p.done_limit,
                "quota": p.quota.clone(),
            },
            "note": "count of what THIS filter would return, computed from the same \
                     filter+cap the list runs. Terminal statuses are capped unless you pass \
                     done_limit=0 or all=1, so a count taken with different params will \
                     differ from one taken with these — pass the SAME params you will \
                     fetch with.",
        }))
        .into_response();
    }

    // TWO-FIXES (AMUX-3154): the terminal cap already reports itself in
    // x-amux-truncated / x-amux-terminal-total, but a `curl | json.load` consumer
    // reads the bare array and never sees a header (ethos rule 4, second layer: a
    // signal in a store the reader never opens is the same as no signal). So the
    // NEXT lane that reads the plain list as a `done` denominator leaves a
    // greppable trace instead of a clean-looking wrong answer. Gated to the
    // denominator-read SHAPE — an unscoped, full-card (non-slim) fetch that did
    // not ask for a bound — so the high-frequency dashboard poll (slim=1) and any
    // explicit ?all=1 / ?done_limit= caller stay silent. grep "board list truncated".
    // Gate on p.slim.is_none(), not !slim (AMUX-3496): with slim the DEFAULT,
    // !slim would silence this for exactly the ad-hoc denominator readers it
    // exists to catch. The high-frequency dashboard poll says slim=1
    // explicitly and stays silent; a bare curl (now slim-shaped, still
    // capped) warns.
    if term_total > term_kept
        && !scoped
        && !uncap_all
        && p.slim.is_none()
        && p.done_limit.is_none()
    {
        // AEAB-54: NAME THE CALLER. Without it the line says somebody may be
        // miscounting and gives the reader nothing to check — and the whole
        // point of the comment above is that the next lane "leaves a greppable
        // trace", which a trace with no subject only half does.
        //
        // user-agent FIRST, and that ordering is the finding rather than a
        // style choice. Measured 2026-08-24: all 39 calls that tripped this gate
        // in 24h carried `curl/8.7.1` and an EMPTY x-amux-session — because the
        // gate selects for ad-hoc consumers, and an ad-hoc consumer is precisely
        // the one that never sets the session header. Attributing on session
        // alone would have printed an empty field 39 times out of 39: a fix that
        // looks like one and changes nothing.
        //
        // Session is still reported when present, because a WORKER tripping this
        // is the more serious case and is the one worth chasing.
        let (ua, sess) = truncation_caller(&headers);
        tracing::warn!(
            target: "board",
            hidden = term_total - term_kept,
            terminal_total = term_total,
            terminal_returned = term_kept,
            caller_ua = %ua,
            caller_session = %sess,
            "board list truncated {} terminal card(s) to the render cap — a caller reading the \
             plain /api/board as a 'done' denominator sees a partial set. Use ?all=1 or \
             ?status=done for the full set (AMUX-3154).",
            term_total - term_kept
        );
    }

    let offset = p.offset.unwrap_or(0);
    let page: &[bs::IssueRow] = if offset >= kept.len() {
        &[]
    } else if let Some(lim) = p.limit {
        &kept[offset..(offset + lim).min(kept.len())]
    } else {
        &kept[offset..]
    };

    let items: Vec<Value> = page
        .iter()
        .map(|r| list_body(r, slim, is_stale(r, now, &working)))
        .collect();

    let mut headers = HeaderMap::new();
    let put = |h: &mut HeaderMap, k: &'static str, v: String| {
        if let Ok(val) = v.parse() {
            h.insert(k, val);
        }
    };
    put(&mut headers, "x-amux-done-limit", done_limit.to_string());
    put(
        &mut headers,
        "x-amux-truncated",
        if term_total > term_kept { "1" } else { "0" }.to_string(),
    );
    put(&mut headers, "x-amux-terminal-total", term_total.to_string());
    put(
        &mut headers,
        "x-amux-terminal-returned",
        term_kept.to_string(),
    );
    put(&mut headers, "x-amux-total", total.to_string());
    put(&mut headers, "x-amux-offset", offset.to_string());
    put(&mut headers, "x-amux-returned", items.len().to_string());
    // BACKE-3228: announce any query params we ignored, so a filter typo cannot
    // masquerade as an empty/absent result. Non-breaking (informational header).
    if !ignored.is_empty() {
        put(&mut headers, "x-amux-params-ignored", ignored.join(","));
    }
    put(&mut headers, "etag", etag_val);
    (StatusCode::OK, headers, Json(Value::Array(items))).into_response()
}

// ---- request-value helpers (bodies are raw maps: the Python dashboard
// PATCHes whole item objects, so deny_unknown_fields would break the UI;
// unknown keys are collected and REPORTED as `ignored_fields` instead of
// silently dropped — the narrower truth Invariant 37 actually needs) -------

fn body_str(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// A nullable string field: `None` = absent, `Some(None)` = explicit null
/// (clear it), `Some(Some(s))` = set.
fn body_opt_str(map: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    match map.get(key) {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => Some(v.as_str().map(str::to_string)),
    }
}

/// Validates and JSON-encodes a PATCH value for `acceptance_criteria`
/// (AF-711). `None` = clear; `Some(s)` = the string to store, already
/// JSON-encoded so the read side's `parse_json_or_raw_string` round-trips it
/// exactly — a plain string stores as a JSON string (`"..."`, reads back as
/// the identical string) and an array of strings stores as a JSON array
/// (`[...]`, matching what `board decompose` itself writes). Any other JSON
/// shape (number, bool, object, or an array with a non-string entry) is
/// rejected rather than silently coerced into a clear, which is the one
/// behavior every reporter of this bug agreed was wrong.
fn encode_acceptance_criteria(v: &Value) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(_) => Ok(Some(serde_json::to_string(v).expect("a JSON string always encodes"))),
        Value::Array(items) if items.iter().all(Value::is_string) => {
            if items.is_empty() {
                Ok(None)
            } else {
                Ok(Some(serde_json::to_string(v).expect("an array of JSON strings always encodes")))
            }
        }
        Value::Array(_) => {
            Err("acceptance_criteria array entries must all be strings".to_string())
        }
        other => Err(format!(
            "acceptance_criteria must be a string, an array of strings, or null, got {other}"
        )),
    }
}

/// tags/depends_on style list: array of strings; a bare string is coerced to
/// a one-element list (SP-539: iterating a str exploded it into one tag per
/// character — 200, no error, silently corrupted card).
fn body_str_list(v: &Value) -> Result<Vec<String>, String> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(if s.trim().is_empty() {
            Vec::new()
        } else {
            vec![s.clone()]
        }),
        Value::Array(a) => {
            let mut out = Vec::new();
            for x in a {
                match x.as_str() {
                    Some(s) if !s.trim().is_empty() => out.push(s.trim().to_string()),
                    Some(_) => {}
                    None => return Err("must be a list of strings".into()),
                }
            }
            Ok(out)
        }
        _ => Err("must be a list of strings".into()),
    }
}

fn unknown_type_response(t: &str) -> Response {
    err(
        StatusCode::BAD_REQUEST,
        json!({
            "error": format!("unknown type {t:?}"),
            "valid_types": bs::KNOWN_TYPES,
            "why": "The gate is DERIVED from type. An unknown type would silently fall back \
                    to the strictest (code) gate, which non-code work cannot satisfy without \
                    asserting a merge that never happened.",
        }),
    )
}

fn cycle_response(cycle: &[String]) -> Response {
    err(
        StatusCode::BAD_REQUEST,
        json!({
            "error": format!("circular depends_on: {}", cycle.join(" -> ")),
            "cycle": cycle,
        }),
    )
}

const VALID_STATUSES: [&str; 11] = [
    "backlog",
    "todo",
    "doing",
    "review",
    "needsyou",
    "blocked",
    "done",
    "verified",
    "discarded",
    "armed",
    "quarantined",
];

// ---- POST /api/board -----------------------------------------------------

/// The typed-ask refusal, for BOTH doors into `needsyou` (AMUX-3929).
///
/// The transition gate shipped first and worked: `amux board status <id>
/// needsyou` and `PATCH {"status":"needsyou"}` are both refused without a typed
/// ask. CREATION was never gated, and creation is the door most of this traffic
/// uses — "I found something the human must decide" is naturally expressed by
/// filing the card already parked, not by filing it in `todo` and moving it.
///
/// Measured by mixpeek-general on the live board: 491 in `needsyou`, 68 typed,
/// 423 untyped (86%), untyped median age 16 days, oldest 59. Still leaking at
/// the time of measurement — 13 untyped created in 24h against 15 typed, 40 in
/// 72h, 106 in 7d — across every lane (backend 53, amux 53, ts-gke 41,
/// gtm-engine 39, ETHAN 33), which is what makes it the API and not a habit.
///
/// ONE BUILDER, TWO CALLERS, on purpose. A second copy of this vocabulary would
/// drift from the first, and the caller must learn the SAME contract at whichever
/// door they arrive at — a create-path message that differed from the
/// transition-path message would teach two contracts for one rule.
fn needsyou_ask_refusal(verdict: bs::AskVerdict, id: &str, session: Option<&str>) -> Response {
    let (why, code) = match verdict {
        bs::AskVerdict::NoType => (
            "This card does not say what KIND of human act it is waiting on. 86% of the cards already parked here are not blocked on a human at all (423 of 491, live, untyped median age 16 days) — they are work someone stopped doing, and they are why the real asks go unanswered.",
            "needsyou_requires_ask_type",
        ),
        bs::AskVerdict::UnknownType => (
            "That is not one of the five kinds of human act. The vocabulary is closed on purpose: a block that fits none of them is not a block on a person.",
            "needsyou_ask_type_unknown",
        ),
        bs::AskVerdict::NoActor => (
            "`ask_actor` must name the specific person or external actor who can answer. Generic values such as human, user, owner, someone, or you are not a routing address.",
            "needsyou_requires_specific_actor",
        ),
        bs::AskVerdict::NoQuestion => (
            "`ask_question` has to be an actual question, in a sentence. \"Blocked on Ethan\" with no question is not an ask — that phrasing is most of what is sitting in this queue today.",
            "needsyou_ask_has_no_question",
        ),
        bs::AskVerdict::NotAQuestion => (
            "`ask_question` must be an actual direct question ending in `?`, not a status note or an instruction the worker could keep doing itself.",
            "needsyou_ask_is_not_a_question",
        ),
        bs::AskVerdict::NoUnblocks => (
            "`ask_unblocks` has to say what ENDS the block, in a sentence. Without it nobody but you can tell whether an answer has landed, so the card cannot leave this queue except by you noticing.",
            "needsyou_ask_has_no_exit",
        ),
        bs::AskVerdict::Ok => unreachable!("Ok is not a refusal"),
    };
    tracing::warn!(
        "needsyou_ask_gate: blocked {} -> needsyou for session {} (verdict {:?})",
        id,
        session.unwrap_or("-"),
        verdict
    );
    err(
        StatusCode::CONFLICT,
        json!({
            "error": "needsyou requires a typed ask",
            "code": code,
            "ok": false,
            "blocked": true,
            "item": id,
            "why": why,
            "ask_types": bs::ASK_TYPES,
            "how_to_fix": {
                "fields": "ask_actor (a named person/external actor), ask_type, ask_question (a direct question containing ?), and ask_unblocks (the observable exit).",
                "cli": "amux board needsyou <ID> --actor <name> --ask <type> --question \"...?\" --unblocks \"...\"",
                "not_an_ask": "If nobody is actually waiting on a person, this is not a needsyou card — put it back in todo/backlog, or discard it.",
            },
        }),
    )
}

#[derive(Debug)]
struct RequestParentRefusal {
    code: &'static str,
    why: String,
    candidates: Vec<String>,
}

#[derive(Debug)]
enum RequestParentResolution {
    /// A peer request may intentionally be standalone. Preserve that existing
    /// use when the requester has no active task rather than inventing a
    /// parent from an unrelated todo.
    Standalone,
    Linked(Box<IssueRow>),
    Refused(RequestParentRefusal),
}

/// What happened when a worker-side helper tried to leave evidence on the task
/// that caused it. This is deliberately a receipt, not another board card:
/// high-frequency helper calls are activity INSIDE work, and minting one task
/// per read would turn observability into the board accumulation it is meant to
/// explain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TaskActivityReceipt {
    pub measured: bool,
    pub n_considered: usize,
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

fn request_parent_is_owned(row: &IssueRow, requester: &str) -> bool {
    row.session.as_deref() == Some(requester)
        && row.owner_type == "agent"
        && row.archived == 0
}

/// A todo parent is considered current only when this requester already
/// delegated one of its open dependencies. That is the multiple-fanout case:
/// the first request requeues the parent, then subsequent requests in the same
/// turn must append to that same task instead of becoming standalone siblings.
fn is_open_delegation_parent(conn: &rusqlite::Connection, row: &IssueRow, requester: &str) -> bool {
    row.status == "todo"
        && row.depends_on.iter().any(|id| {
            conn.query_row(
                "SELECT requested_by, status FROM issues \
                 WHERE id=?1 AND deleted IS NULL AND COALESCE(archived,0)=0",
                rusqlite::params![id],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?)),
            )
            .ok()
            .is_some_and(|(requested_by, status)| {
                requested_by.as_deref() == Some(requester) && !bs::is_terminal_status(&status)
            })
        })
}

/// Resolve which requester task a peer request belongs to.
///
/// Explicit `request_parent` is authoritative. Without it, the latest durable
/// message->task link is the current-task signal; only when that is absent do
/// we accept a unique `doing` card. Multiple doing cards are refused because
/// silently selecting the wrong task would corrupt the dependency graph.
fn resolve_request_parent(
    conn: &rusqlite::Connection,
    requester: &str,
    explicit: Option<&str>,
) -> rusqlite::Result<RequestParentResolution> {
    if let Some(id) = explicit {
        let Some(row) = bs::get_issue(conn, id)? else {
            return Ok(RequestParentResolution::Refused(RequestParentRefusal {
                code: "task_request_parent_not_found",
                why: format!("request_parent {id} does not name a live task"),
                candidates: Vec::new(),
            }));
        };
        if !request_parent_is_owned(&row, requester) {
            return Ok(RequestParentResolution::Refused(RequestParentRefusal {
                code: "task_request_parent_not_owned",
                why: format!(
                    "request_parent {id} is not a live agent task owned by requester {requester}"
                ),
                candidates: Vec::new(),
            }));
        }
        if !matches!(row.status.as_str(), "doing" | "todo") {
            return Ok(RequestParentResolution::Refused(RequestParentRefusal {
                code: "task_request_parent_not_active",
                why: format!(
                    "request_parent {id} is {}, but delegation can attach only to doing or todo work",
                    row.status
                ),
                candidates: Vec::new(),
            }));
        }
        return Ok(RequestParentResolution::Linked(Box::new(row)));
    }

    let linked_ids: Vec<String> = conn
        .prepare(
            "SELECT i.id FROM cmd_history h JOIN issues i ON i.id=h.card_id \
             WHERE h.session=?1 AND i.session=?1 AND i.owner_type='agent' \
               AND i.deleted IS NULL AND COALESCE(i.archived,0)=0 \
               AND i.status IN ('doing','todo') \
             ORDER BY h.id DESC LIMIT 32",
        )?
        .query_map(rusqlite::params![requester], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    for id in linked_ids {
        if let Some(row) = bs::get_issue(conn, &id)? {
            if row.status == "doing" || is_open_delegation_parent(conn, &row, requester) {
                return Ok(RequestParentResolution::Linked(Box::new(row)));
            }
        }
    }

    let doing: Vec<String> = conn
        .prepare(
            "SELECT id FROM issues WHERE session=?1 AND status='doing' \
             AND owner_type='agent' AND deleted IS NULL AND COALESCE(archived,0)=0 \
             ORDER BY updated DESC,id",
        )?
        .query_map(rusqlite::params![requester], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect();
    match doing.as_slice() {
        [] => Ok(RequestParentResolution::Standalone),
        [id] => Ok(RequestParentResolution::Linked(Box::new(
            bs::get_issue(conn, id)?.expect("selected live parent disappeared inside one write"),
        ))),
        _ => Ok(RequestParentResolution::Refused(RequestParentRefusal {
            code: "task_request_parent_ambiguous",
            why: format!(
                "requester {requester} has multiple doing tasks; refusing to attach the delegated work to the wrong one"
            ),
            candidates: doing,
        })),
    }
}

/// Append compact worker activity to the SAME current-task predicate used by
/// peer delegation. That shared resolver matters: a bulk read and a farmed-out
/// dependency must not disagree about which parent the worker is advancing.
///
/// The payload is caller-supplied summary metadata only (counts/model/verdict),
/// never helper output or source contents. The task mutation carries a normal
/// StateEvent, so other dashboard clients see it through the existing board
/// realtime path rather than a private side channel.
fn record_session_task_activity(
    conn: &rusqlite::Connection,
    session: &str,
    explicit: Option<&str>,
    line: &str,
    now: i64,
) -> rusqlite::Result<(TaskActivityReceipt, Option<PendingEvent>)> {
    match resolve_request_parent(conn, session, explicit)? {
        RequestParentResolution::Standalone => Ok((
            TaskActivityReceipt {
                measured: true,
                n_considered: 0,
                verdict: "no_active_task".into(),
                card_id: None,
                why: Some(format!(
                    "worker {session} has no active task; the helper receipt remains in amux logs"
                )),
            },
            None,
        )),
        RequestParentResolution::Refused(refusal) => {
            let n_considered = refusal.candidates.len();
            Ok((
                TaskActivityReceipt {
                    measured: true,
                    n_considered,
                    verdict: refusal.code.into(),
                    card_id: None,
                    why: Some(refusal.why),
                },
                None,
            ))
        }
        RequestParentResolution::Linked(mut row) => {
            row.log = Some(bs::append_log(row.log.as_deref(), &hhmm(), line));
            row.updated = now;
            bs::save_patched(conn, &mut row)?;
            let event = ev_snap(&row, MutationKind::Updated);
            Ok((
                TaskActivityReceipt {
                    measured: true,
                    n_considered: 1,
                    verdict: "attached".into(),
                    card_id: Some(row.id),
                    why: None,
                },
                Some(event),
            ))
        }
    }
}

pub(crate) async fn append_session_task_activity(
    state: &AppState,
    session: &str,
    explicit: Option<&str>,
    line: &str,
) -> Result<TaskActivityReceipt, anyhow::Error> {
    let session = session.trim();
    if session.is_empty() {
        return Ok(TaskActivityReceipt {
            measured: false,
            n_considered: 0,
            verdict: "unattributed".into(),
            card_id: None,
            why: Some(
                "no X-Amux-Worker or X-Amux-Session header; no task owner could be resolved"
                    .into(),
            ),
        });
    }
    let (session, explicit, line) = (
        session.to_string(),
        explicit.map(str::to_string),
        line.to_string(),
    );
    let slot = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    state
        .store
        .write_async(move |conn| {
            let (receipt, event) = record_session_task_activity(
                conn,
                &session,
                explicit.as_deref(),
                &line,
                now_secs(),
            )?;
            let applied = event.is_some();
            *slot_w.lock().expect("task activity receipt slot poisoned") = Some(receipt);
            Ok(WriteOutcome {
                applied,
                events: event.into_iter().collect(),
            })
        })
        .await?;
    let receipt = slot
        .lock()
        .expect("task activity receipt slot poisoned")
        .clone()
        .ok_or_else(|| anyhow::anyhow!("task activity receipt was not produced"));
    receipt
}

pub async fn create_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(map) = body.as_object().cloned() else {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "body must be a JSON object" }),
        );
    };
    let title = body_str(&map, "title").unwrap_or_default().trim().to_string();
    if title.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "missing title" }));
    }

    // MO-3038: when the body OMITS `session` and the verified header is
    // present, the card is for the sender's own lane. An EXPLICIT value —
    // including explicit "" / null for a deliberately unassigned card — is
    // always respected.
    let (_, actor_name) = actor_from_headers(&headers);
    let hdr_session = if actor_name == "api-anonymous"
        || super::org::is_verified_local_member(&headers)
    {
        String::new()
    } else {
        actor_name.clone()
    };
    let session = if map.contains_key("session") {
        body_str(&map, "session").unwrap_or_default().trim().to_string()
    } else {
        hdr_session.chars().take(64).collect()
    };
    if let Some(scope) = super::org::local_member_scope(&headers).filter(|scope| !scope.is_global()) {
        if session.is_empty() || !scope.allows_worker(&session) {
            return scoped_board_forbidden(&scope, if session.is_empty() { "unassigned card" } else { &session });
        }
    }
    if !hdr_session.is_empty() && session != hdr_session {
        tracing::warn!(
            caller = %hdr_session,
            requested_owner = %(if session.is_empty() { "(unassigned)" } else { session.as_str() }),
            "cross-board card creation refused"
        );
        return err(
            StatusCode::FORBIDDEN,
            json!({
                "error": "workers may create board items only on their own board",
                "code": "cross_board_create_forbidden",
                "caller": hdr_session,
                "requested_owner": if session.is_empty() { Value::Null } else { json!(session) },
                "how_to_fix": "create the card on your own board and link the peer with reviewer, shepherd, or depends_on",
            }),
        );
    }

    let status_in = body_str(&map, "status").unwrap_or_else(|| "todo".into());
    // THE SAME PREDICATE ON THE CREATE DOOR (AMUX-3929). The transition gate
    // held — `PATCH {"status":"needsyou"}` and `amux board status <id> needsyou`
    // are both refused — while `POST {"status":"needsyou"}` returned 201 with
    // ask_type NULL. Reproduced before fixing: the same card refused on PATCH
    // that had just been created in that state.
    //
    // `force` is not read here: creation has no `force` parameter and inventing
    // one would add a bypass the transition door does not have.
    if bs::parse_status(&status_in) == Some(TaskStatus::NeedsYou) {
        let session_for_gate = body_str(&map, "session")
            .or_else(|| Some(actor_from_headers(&headers).1))
            .filter(|s| !s.trim().is_empty());
        if bs::needsyou_ask_required(session_for_gate.as_deref()) {
            let verdict = bs::ask_verdict(
                body_str(&map, "ask_actor").unwrap_or_default().as_str(),
                body_str(&map, "ask_type").unwrap_or_default().as_str(),
                body_str(&map, "ask_question").unwrap_or_default().as_str(),
                body_str(&map, "ask_unblocks").unwrap_or_default().as_str(),
            );
            if verdict != bs::AskVerdict::Ok {
                return needsyou_ask_refusal(verdict, "(new card)", session_for_gate.as_deref());
            }
        }
    }
    // AMUX-2609: a status outside the typed vocabulary may still be a real
    // user-created column. The `statuses` table is the vocabulary for those —
    // see the long note in `patch_item` for why `TaskStatus` stays closed.
    let status_raw = match bs::parse_status(&status_in) {
        Some(st) => bs::db_status_spelling(st).to_string(),
        None => {
            let id = status_in.trim().to_lowercase();
            let known = state
                .store
                .read()
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT id FROM statuses WHERE id = ?1",
                        rusqlite::params![id],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                });
            match known {
                Some(id) => id,
                None => {
                    let cols: Vec<String> = state
                        .store
                        .read()
                        .ok()
                        .map(|conn| {
                            conn.prepare("SELECT id FROM statuses ORDER BY position")
                                .and_then(|mut st| {
                                    st.query_map([], |r| r.get::<_, String>(0))
                                        .map(|rows| rows.flatten().collect::<Vec<String>>())
                                })
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    return err(
                        StatusCode::BAD_REQUEST,
                        json!({
                            "error": format!("unknown status {status_in:?}"),
                            "valid_statuses": VALID_STATUSES,
                            "configured_columns": cols,
                            "how_to_add": "POST /api/board/statuses {\"label\": \"...\"}",
                        }),
                    );
                }
            }
        }
    };

    let item_type = body_str(&map, "type")
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "code".into());
    if !bs::KNOWN_TYPES.contains(&item_type.as_str()) {
        return unknown_type_response(&item_type);
    }

    let depends_on = match map.get("depends_on") {
        None => Vec::new(),
        Some(v) => match body_str_list(v) {
            Ok(l) => l,
            Err(e) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": format!("depends_on {e}") }),
                )
            }
        },
    };
    let tags = match map.get("tags") {
        None => Vec::new(),
        Some(v) => match body_str_list(v) {
            Ok(l) => l,
            Err(e) => {
                return err(StatusCode::BAD_REQUEST, json!({ "error": format!("tags {e}") }))
            }
        },
    };
    let gate = match map.get("gate") {
        None => Vec::new(),
        Some(v) => body_str_list(v).unwrap_or_default(),
    };

    // Creator attribution (AMUX-1812): the body value is a self-reported
    // CLAIM; the verified header wins, and a disagreement is recorded.
    let claimed = body_str(&map, "creator").unwrap_or_default().trim().to_string();
    let verified_creator = (actor_name != "api-anonymous").then_some(actor_name.as_str());
    let creator = match (verified_creator, claimed.is_empty()) {
        // A local member's author is derived from the verified invite cookie.
        // Old dashboard clients still send a device-name `creator`; retaining
        // that self-reported value would make the same person appear under a
        // different author on every device and would allow deliberate spoofing.
        (Some(author), _) if super::org::is_verified_local_member(&headers) => author.to_string(),
        (Some(author), false) if author != claimed => format!("{author} (claimed {claimed})"),
        (Some(author), _) => author.to_string(),
        (None, false) => claimed,
        (None, true) => String::new(),
    };

    let owner_type = match body_str(&map, "owner_type").as_deref() {
        Some("human") => "human".to_string(),
        Some("agent") => "agent".to_string(),
        Some(_) => "human".to_string(),
        None => if session.is_empty() { "human" } else { "agent" }.to_string(),
    };

    let callback_specified = map.contains_key("callback");
    let (callback_session, callback_prompt) = match map.get("callback") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => (None, None),
        Some(Value::Bool(true)) => (Some(hdr_session.clone()), None),
        Some(Value::String(prompt)) => (Some(hdr_session.clone()), Some(prompt.trim().to_string())),
        Some(Value::Object(cb)) => {
            let target = cb
                .get("session")
                .and_then(Value::as_str)
                .unwrap_or(&hdr_session)
                .trim()
                .to_string();
            let prompt = cb
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            (Some(target), prompt)
        }
        Some(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                json!({"error": "callback must be true, false, a prompt string, or {session?, prompt?}"}),
            )
        }
    };
    if callback_specified && hdr_session.is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "a callback requires a verified X-Amux-Worker requester"}),
        );
    }
    if callback_session.as_deref().is_some_and(|s| s.is_empty()) {
        return err(StatusCode::BAD_REQUEST, json!({"error": "callback session is empty"}));
    }
    if callback_session.as_deref().is_some_and(|s| s != hdr_session) {
        return err(
            StatusCode::FORBIDDEN,
            json!({
                "error": "a task callback may return only to the verified requester",
                "requester": hdr_session,
                "callback_session": callback_session,
            }),
        );
    }
    if callback_session.is_some() && bs::is_terminal_status(&status_raw) {
        return err(
            StatusCode::BAD_REQUEST,
            json!({"error": "cannot arm a completion callback on a task created terminal"}),
        );
    }

    let known_keys = [
        "title", "desc", "status", "session", "type", "depends_on", "tags", "creator",
        "reviewer", "shepherd", "gate", "owner_type", "due", "due_time", "callback",
        "ask_actor", "ask_type", "ask_question", "ask_unblocks",
    ];
    let ignored: Vec<String> = map
        .keys()
        .filter(|k| !known_keys.contains(&k.as_str()))
        .cloned()
        .collect();

    let _intake_guard = super::board_intake::lock(&session, &owner_type).await;
    let mut intake = super::board_intake::plan(&state.store, &session, &owner_type, &title,
        &body_str(&map, "desc").unwrap_or_default()).await;
    // Reconciliation must not silently drop graph edges, explicit gates,
    // callbacks or scheduling metadata from a structured create request.
    if ["depends_on", "gate", "callback", "due", "due_time", "reviewer", "shepherd",
        "ask_actor", "ask_type", "ask_question", "ask_unblocks", "tags"].iter()
        .any(|key| map.get(*key).is_some_and(|v| !v.is_null() && v != "" && v != &json!([])))
        || matches!(item_type.as_str(), "epic" | "watch" | "tripwire") {
        intake.preserve_structured_request();
    }
    let intake_response = intake.clone();
    // A repeated/refined request should not be refused merely because the
    // existing queue is full; reconciliation adds no WIP slot.
    let intake_matches = intake.decision.action != "create";

    // AF-317: THE WIP LIMIT HAS TO COVER CREATION, or it is decorative.
    //
    // `amux board add` is how a lane files its own work and it creates directly
    // in `todo`, so gating only the PATCH transition would leave the path that
    // actually grew the queues untouched (ethos rule 7: a check pinning the
    // wrong layer is exactly as green as one pinning the right layer).
    //
    // Two exemptions, both named rather than silent. Detector cards carry
    // `session: None` and are outside the count by construction — a fault
    // report is never dropped because a lane was full. And a card from the
    // queue-disposition job is exempt BY NAME: it is the one card whose whole
    // purpose is to arrive when the queue is too long, so refusing it for queue
    // depth would be the mechanism suppressing its own alarm.
    if !intake_matches && status_raw == "todo" && owner_type == "agent" && !session.is_empty() && creator != QUEUE_DISPOSITION_CREATOR {
        let limit = bs::todo_wip_limit(Some(&session));
        if limit > 0 {
            let held_and_stalest = state.store.read().ok().map(|c| {
                (bs::todo_wip_count(&c, &session, ""), bs::stalest_todos(&c, &session, 5))
            });
            if let Some((held, stalest)) = held_and_stalest {
                if held >= limit {
                    tracing::warn!(
                        "todo_wip_gate: refused a new todo for lane {} (holding {}, limit {})",
                        session, held, limit
                    );
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": "todo queue is at its limit for this lane",
                            "code": "todo_wip_limit_reached",
                            "ok": false,
                            "blocked": true,
                            "session": session,
                            "holding": held,
                            "limit": limit,
                            "why": format!(
                                "{session} already holds {held} todo card(s) and the limit is {limit}. \
                                 `todo` is the dispatch queue: filing here is a claim that \
                                 the card is next."
                            ),
                            "close_these_first": stalest.iter().map(|(id, title, days)| json!({
                                "id": id, "title": title, "days_since_touched": days,
                                "already_undispatchable": *days >= 7,
                            })).collect::<Vec<_>>(),
                            "how_to_fix": {
                                "file_it_anyway_but_not_as_next": "POST with {\"status\":\"backlog\"} — unbounded on purpose, and where a real card that is not NEXT belongs",
                                "raise_it": "set AMUX_TODO_WIP_LIMIT=<n> in this worker's / group's / global configuration; 0 disables it"
                            }
                        })),
                    )
                        .into_response();
                }
            }
        }
    }

    let new = bs::NewIssue {
        title,
        desc: body_str(&map, "desc").unwrap_or_default(),
        status: status_raw,
        session: Some(session.clone()).filter(|s| !s.is_empty()),
        item_type,
        creator,
        owner_type,
        due: body_str(&map, "due").filter(|s| !s.trim().is_empty()),
        due_time: body_str(&map, "due_time").filter(|s| !s.trim().is_empty()),
        reviewer: body_str(&map, "reviewer").filter(|s| !s.trim().is_empty()),
        shepherd: body_str(&map, "shepherd").filter(|s| !s.trim().is_empty()),
        gate,
        depends_on,
        tags,
        ask_type: body_str(&map, "ask_type"),
        ask_question: body_str(&map, "ask_question"),
        ask_unblocks: body_str(&map, "ask_unblocks"),
        ask_actor: body_str(&map, "ask_actor"),
        // AF-367: the HTTP create path — a real POST /api/board from a lane or
        // a human, as opposed to a card a daemon filed.
        source: Some("agent".into()),
        requested_by: None,
        callback_session,
        callback_prompt,
    };

    enum Out {
        Cycle(Vec<String>),
        WipLimit(String, i64, i64),
        Created(Box<IssueRow>, bool),
    }
    let slot: Arc<Mutex<Option<Out>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    // AMUX-3391: carries the id of a capture card this create folded, so the
    // response can name it and it can be reported without re-querying.
    let folded: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let folded_w = folded.clone();
    let write = state
        .store
        .write_async(move |conn| {
            if let Some(row) = super::board_intake::apply(conn, &intake, &new.title, &new.desc, now_secs())? {
                let event = ev_snap(&row, MutationKind::Updated);
                return finish(&slot_w, Out::Created(Box::new(row), true), WriteOutcome {applied:true,events:vec![event]});
            }
            // Recheck in the writer, including a semantic target that changed
            // while the model ran. A failed merge must not bypass the WIP gate.
            if new.status == "todo" && new.owner_type == "agent" && new.creator != QUEUE_DISPOSITION_CREATOR {
                if let Some(session) = new.session.as_deref() {
                    let limit = bs::todo_wip_limit(Some(session));
                    let held = bs::todo_wip_count(conn, session, "");
                    if limit > 0 && held >= limit {
                        tracing::warn!(session, held, limit, "todo_wip_gate: refused create in writer");
                        return finish(&slot_w, Out::WipLimit(session.to_string(), held, limit), no_write());
                    }
                }
            }
            // Acyclicity is validated INSIDE the write so no interleaved
            // create can slip a cycle between check and insert. The new id
            // does not exist yet, so a placeholder self id is fine — only
            // edges out of it are being added.
            //
            // THE PLACEHOLDER IS PRINTABLE, and it was not (AF-481). It used to
            // be "\u{0}new-card": a leading NUL, chosen because no real card id
            // can contain one, which is correct and is also true of a space. The
            // sentinel reaches a LOG LINE verbatim, `depends_on_cycle` warns with
            // `self_id = %self_id` on a pre-existing cycle elsewhere on the board,
            // and a single NUL byte makes grep declare the WHOLE FILE binary.
            //
            // Measured 2026-09-04: 19 NUL bytes in a 67 MB server-rs.log, all 19
            // from this one warn, all from the same stuck cycle
            // (GE-473 -> MHC-256) retried across three days. `grep -c` still
            // counted 17 matching lines while `grep -o` returned 8, because grep
            // suppresses match OUTPUT for binary input and says nothing when the
            // output goes to a pipe. Every `grep -o` sweep over that file
            // undercounted by 53% and looked fine, and this repo's own log-sweep
            // doc prescribes greps.
            //
            // Non-collision is unchanged: card ids are `[A-Z]+-<digits>`, so the
            // space and the parentheses are as impossible as the NUL was.
            if !new.depends_on.is_empty() {
                if let Some(cycle) = bs::depends_on_cycle(conn, NEW_CARD_SELF_ID, &new.depends_on)? {
                    return finish(&slot_w, Out::Cycle(cycle), no_write());
                }
            }
            let now = now_secs();
            let mut row = bs::create_issue(conn, &new, now)?;
            row.log = Some(bs::append_log(row.log.as_deref(), &hhmm(), &format!("{}; disposition=create", intake.log_line())));
            bs::save_patched(conn, &mut row)?;
            let mut events = vec![ev_snap(&row, MutationKind::Created)];
            // AMUX-3391: fold the silent auto-capture card into this worker card
            // (see fold_capture_for_worker_card). The window is env-tunable.
            let fold_window: i64 = std::env::var("AMUX_CAPTURE_FOLD_WINDOW_S")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600);
            if let Some((cap_id, ev)) =
                fold_capture_for_worker_card(conn, &row, fold_window, now_secs())?
            {
                events.push(ev);
                *folded_w.lock().unwrap() = Some(cap_id);
            }
            finish(
                &slot_w,
                Out::Created(Box::new(row), false),
                WriteOutcome {
                    applied: true,
                    events,
                },
            )
        })
        .await;
    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("create produced no outcome"),
        Some(Out::Cycle(cycle)) => cycle_response(&cycle),
        Some(Out::WipLimit(session, held, limit)) => err(StatusCode::CONFLICT,
            json!({"ok":false,"code":"todo_wip_limit_reached","error":"todo queue is at its limit for this lane",
                "session":session,"holding":held,"limit":limit,"how_to_fix":"create in backlog or finish existing todo work"})),
        Some(Out::Created(row, reused)) => {
            let mut v = detail_body(&row);
            v["rev"] = json!(row.rev);
            v["global_rev"] = json!(reply.rev.0);
            v["intake"] = json!({"action":if reused {intake_response.decision.action.as_str()} else {"create"}, "comparison":intake_response});
            if !ignored.is_empty() {
                v["ignored_fields"] = json!(ignored);
            }
            // AMUX-3391: tell the caller the auto-capture card it just displaced,
            // so a worker sees the reconcile happened and never hand-discards it.
            if let Some(cap_id) = folded.lock().expect("folded slot poisoned").take() {
                v["folded_capture"] = json!(cap_id);
            }
            // AF-366: RECORD WHO CALLED, not only what the row now says.
            //
            // A card's `creator` is derived from a header the CALLER supplies, and
            // until now nothing anywhere recorded the request itself: a successful
            // create logged NOTHING, and the state-event payload stores the
            // resulting `creator` field, which is the very value in question. So a
            // card attributed to the wrong lane was unforensicable by anyone,
            // including the lane wearing the attribution.
            //
            // Found live: AF-363 "Test card from tubescience" and AF-364 "[ts-gke]
            // tenant-deploy engine skipped" were both stamped
            // creator=amux-frustrations, and auto-pickup then handed a lane another
            // team's deploy card to work. Neither their origin nor their intent can
            // be recovered from any record that exists.
            //
            // The board's READ path already logs `caller_ua` and `caller_session`
            // on its truncation WARN. The WRITE path logged nothing, which is
            // backwards: the read is recoverable by reading again, the write is not.
            // Reuses `truncation_caller` rather than re-deriving the pair, so the
            // honest fallbacks ("(none)", "(unattributed)") stay identical across
            // both sites instead of one growing a silent blank.
            //
            // INFO, not WARN: creating a card is the normal path and this is a
            // ledger line, not a complaint. `grep 'board card created'` is the
            // forensic index that did not exist.
            let (cua, csess) = truncation_caller(&headers);
            tracing::info!(
                card = %row.id,
                caller_session = %csess,
                caller_ua = %cua,
                stamped_creator = %(if row.creator.is_empty() { "(none)" } else { row.creator.as_str() }),
                owner_session = %row.session.as_deref().unwrap_or("(none)"),
                "board card created"
            );
            (if reused {StatusCode::OK} else {StatusCode::CREATED}, Json(v)).into_response()
        }
    }
}

// ---- GET /api/board/{id} -------------------------------------------------

fn resolve_task_asset(reference: &str, work_dir: &str) -> String {
    let reference = reference.trim();
    if reference.starts_with("file:") {
        // File URLs are absolute references, never workspace-relative strings.
        return reqwest::Url::parse(reference)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| reference.to_string());
    }
    if let Some(rest) = reference.strip_prefix("~/") {
        return std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    let is_external = reference.starts_with("http://")
        || reference.starts_with("https://")
        || reference.starts_with('#')
        || ((7..=40).contains(&reference.len())
            && reference.bytes().all(|c| c.is_ascii_hexdigit()));
    if is_external || reference.starts_with('/') || work_dir.is_empty() {
        return reference.to_string();
    }

    // A produced asset is not necessarily a file. Saved-retriever ids,
    // namespace ids and prose such as `origin commit abc123` are legitimate
    // references too. The old resolver joined EVERY non-URL value to the
    // worker directory, so the card rendered those logical ids as clickable
    // local files that could never open (observed on TUBES-2379). Resolve only
    // values that actually have a path shape. A spaced relative path is still
    // supported when it exists; otherwise whitespace means the worker put a
    // description in `ref` instead of the artifact's description field and it
    // must stay plain text rather than becoming a bogus path.
    let (path_ref, fragment) = reference
        .split_once('#')
        .map(|(p, f)| (p, Some(f)))
        .unwrap_or((reference, None));
    let path = std::path::Path::new(path_ref);
    let path_shaped = path_ref.starts_with("./")
        || path_ref.starts_with("../")
        || path_ref.contains('/')
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.') && name.len() > 1)
        || path.extension().is_some();
    if !path_shaped {
        return reference.to_string();
    }

    let append_fragment = |candidate: std::path::PathBuf| {
        let mut out = candidate.to_string_lossy().into_owned();
        if let Some(fragment) = fragment {
            out.push('#');
            out.push_str(fragment);
        }
        out
    };
    let direct = std::path::Path::new(work_dir).join(path);
    if direct.exists() {
        return append_fragment(direct);
    }

    // Workers commonly speak in repository-relative paths even when their
    // configured cwd is a subdirectory of that repository. Prefer the nearest
    // existing ancestor-relative match before manufacturing `a/b/a/b/file`.
    for ancestor in std::path::Path::new(work_dir).ancestors().skip(1) {
        let candidate = ancestor.join(path);
        if candidate.exists() {
            return append_fragment(candidate);
        }
    }

    if path_ref.chars().any(char::is_whitespace) {
        reference.to_string()
    } else {
        append_fragment(direct)
    }
}

/// Availability is measured only where doing so is local and side-effect free.
/// Fetching an arbitrary artifact URL from this endpoint would turn a board
/// render into an SSRF primitive; an external link therefore says explicitly
/// that reachability was not measured instead of pretending it passed.
fn task_asset_availability(reference: &str, resolved: &str) -> Value {
    let external = reference.starts_with("http://") || reference.starts_with("https://");
    let symbolic = reference.starts_with('#')
        || ((7..=40).contains(&reference.len())
            && reference.bytes().all(|c| c.is_ascii_hexdigit()));
    if external {
        json!({
            "state": "external",
            "measured": false,
            "why_unmeasured": "external URLs are not fetched by the server (avoids SSRF and side effects)",
        })
    } else if symbolic {
        json!({
            "state": "symbolic",
            "measured": false,
            "why_unmeasured": "commit and PR references are resolved by their repository surface",
        })
    } else {
        let exists = std::path::Path::new(resolved.split('#').next().unwrap_or(resolved)).exists();
        json!({
            "state": if exists { "available" } else { "missing" },
            "measured": true,
            "exists": exists,
        })
    }
}

#[cfg(test)]
mod task_asset_resolution_tests {
    use super::resolve_task_asset;

    #[test]
    fn logical_asset_ids_and_descriptions_do_not_become_fake_files() {
        let cwd = "/tmp/amux-asset-resolution";
        assert_eq!(resolve_task_asset("ret_8d00ed37a392f1", cwd), "ret_8d00ed37a392f1");
        assert_eq!(resolve_task_asset("origin commit 3b398814dd", cwd), "origin commit 3b398814dd");
        assert_eq!(resolve_task_asset("3b398814dd", cwd), "3b398814dd");
        assert_eq!(
            resolve_task_asset("migration/mxp.py (ts-parity-v2 sanction)", cwd),
            "migration/mxp.py (ts-parity-v2 sanction)"
        );
    }

    #[test]
    fn file_urls_decode_without_joining_the_worker_directory() {
        assert_eq!(resolve_task_asset("file:///tmp/a%20report.md", "/work"), "/tmp/a report.md");
        assert_eq!(resolve_task_asset("file://localhost/tmp/report.md", "/work"), "/tmp/report.md");
        assert_eq!(resolve_task_asset("file://remote-host/tmp/report.md", "/work"), "file://remote-host/tmp/report.md");
    }

    #[test]
    fn file_shaped_assets_resolve_without_doubling_repo_relative_prefixes() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("mixpeek");
        let cwd = repo.join("customers/tubescience");
        let file = cwd.join("migration/mxp.py");
        let dotenv = cwd.join(".env");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "# asset").unwrap();
        std::fs::write(&dotenv, "TOKEN=hidden-asset").unwrap();

        assert_eq!(
            resolve_task_asset("migration/mxp.py", cwd.to_str().unwrap()),
            file.to_string_lossy()
        );
        assert_eq!(
            resolve_task_asset(
                "customers/tubescience/migration/mxp.py#contract",
                cwd.to_str().unwrap()
            ),
            format!("{}#contract", file.to_string_lossy())
        );
        assert_eq!(
            resolve_task_asset("customers/tubescience/.env", cwd.to_str().unwrap()),
            dotenv.to_string_lossy(),
            "repo-relative dotfiles resolve from the producing worker's directory"
        );
        assert_eq!(
            resolve_task_asset(".env", cwd.to_str().unwrap()),
            dotenv.to_string_lossy(),
            "a bare dotfile resolves from the producing worker's directory"
        );
    }
}

pub async fn get_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let store = state.store.clone();
    let key = id.clone();
    let member_scope = super::org::local_member_scope(&headers).filter(|scope| !scope.is_global());
    let joined = crate::db::interactions::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        let Some(row) = bs::get_issue(&conn, &key)? else {
            return Ok(None);
        };
        let mut child_ids = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT id FROM issues WHERE epic=?1 AND deleted IS NULL ORDER BY created, id",
        )?;
        child_ids.extend(
            stmt.query_map(rusqlite::params![row.id], |r| r.get::<_, String>(0))?
                .flatten(),
        );
        let children = child_ids
            .iter()
            .filter_map(|child| bs::get_issue(&conn, child).ok().flatten())
            .filter(|child| {
                member_scope.as_ref().is_none_or(|scope| {
                    child
                        .session
                        .as_deref()
                        .is_some_and(|worker| scope.allows_worker(worker))
                })
            })
            .map(|child| {
                json!({
                    "id": child.id,
                    "title": child.title,
                    "status": child.status,
                    "session": child.session,
                    "type": child.item_type,
                    "depends_on": child.depends_on,
                    "priority": child.tags.iter().find(|t| t.starts_with('p')),
                    "evidence": child.evidence,
                    "last_result": child.last_result,
                    "next_action": child.next_action,
                })
            })
            .collect::<Vec<_>>();

        // A child inherits the source message of its epic for display, while
        // cmd_history.card_id itself remains attached to the root epic. That
        // keeps the Messages chip stable from prompt through completion.
        // A child normally inherits its epic's prompt. For a scoped member,
        // crossing that parent boundary could reveal a prompt on a card they
        // cannot open, so only use the directly-authorized card as the root.
        let message_root = if member_scope.is_some() {
            &row.id
        } else {
            row.epic.as_deref().unwrap_or(&row.id)
        };
        let mut messages = Vec::new();
        let mut msg_stmt = conn.prepare(
            "SELECT id,text,type,session,ts,origin,card_id FROM cmd_history \
             WHERE card_id IN (?1,?2) OR (session=?3 AND instr(text,?2)>0) ORDER BY ts DESC,id DESC LIMIT 100",
        )?;
        messages.extend(
            msg_stmt
                .query_map(rusqlite::params![message_root, row.id, row.session.as_deref().unwrap_or("")], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "text": r.get::<_, String>(1)?,
                        "type": r.get::<_, String>(2)?,
                        "session": r.get::<_, String>(3)?,
                        "ts": r.get::<_, i64>(4)?,
                        "origin": r.get::<_, String>(5)?,
                        "card_id": r.get::<_, Option<String>>(6)?,
                    }))
                })?
                .flatten()
                .filter(|message| message["card_id"].as_str().is_some_and(|id| id == message_root || id == row.id)
                    || card_refs(message["text"].as_str().unwrap_or("")).contains(&row.id))
                .take(20),
        );
        let mut artifacts = crate::db::artifact_store::list_for_task(&conn, &row.id)?
            .into_iter()
            .map(|a| artifact_value(&a))
            .collect::<Vec<_>>();

        // A worker normally records produced files and URLs in `evidence`,
        // `last_result`, or its append-only activity log. Requiring a second,
        // obscure artifact-registry write made those references sufficient to
        // pass the Done gate but invisible on the card — enforcement and the
        // evidence surface disagreed. Extract with the SAME parser the gate
        // uses and return every reference. Explicit registry artifacts remain
        // richer and are merged by the client.
        let mut asset_links = Vec::new();
        let mut asset_seen = std::collections::HashSet::new();
        let work_dir = row.session.as_deref()
            .map(crate::api::session_verbs::session_work_dir)
            .unwrap_or_default();
        for artifact in &mut artifacts {
            if let Some(reference) = artifact.get("ref").and_then(Value::as_str) {
                let resolved = resolve_task_asset(reference, &work_dir);
                artifact["availability"] = task_asset_availability(reference, &resolved);
                artifact["resolved_ref"] = json!(resolved);
            }
        }
        let mut add_asset = |reference: String, source: &str| {
            if reference.is_empty() || !asset_seen.insert(reference.clone()) { return; }
            let resolved_ref = resolve_task_asset(&reference, &work_dir);
            asset_links.push(json!({
                "ref": reference,
                "availability": task_asset_availability(&reference, &resolved_ref),
                "resolved_ref": resolved_ref,
                "source": source,
            }));
        };
        for (source, text) in [
            ("evidence", row.evidence.as_deref().unwrap_or("")),
            ("last result", row.last_result.as_deref().unwrap_or("")),
        ] {
            for reference in bs::asset_refs(text) { add_asset(reference, source); }
        }
        // Activity is broad: it names inputs, peer-owned dirty files, and
        // commits belonging to other cards. Only explicit output language may
        // synthesize a link; commit-report registers its exact outputs below.
        for reference in bs::output_asset_refs(row.log.as_deref().unwrap_or("")) {
            add_asset(reference, "worker output");
        }
        let mut file_stmt = conn.prepare(
            "SELECT path FROM issue_files WHERE issue_id=?1 ORDER BY added_at, path",
        )?;
        let files = file_stmt
            .query_map(rusqlite::params![row.id], |r| r.get::<_, String>(0))?
            .flatten()
            .collect::<Vec<_>>();
        for file in files { add_asset(file, "task file"); }

        // The card must say what each workflow column will require BEFORE a
        // worker tries to move it. Resolve through the exact same five-tier
        // precedence walk transition enforcement uses, including worker and
        // group scopes; listing only type defaults would be actively wrong for
        // a scoped worker.
        let groups = row.session.as_deref().filter(|s| !s.is_empty())
            .map(crate::api::session_verbs::lane_groups)
            .unwrap_or_default();
        let gate_requirements = [
            TaskStatus::Doing,
            TaskStatus::Review,
            TaskStatus::Done,
            TaskStatus::Verified,
        ].into_iter().map(|target| {
            let trail = bs::effective_gate_trail(&conn, &row, target, &groups);
            let source = trail.source.token();
            let scope = trail.source.scope();
            json!({
                "status": bs::db_status_spelling(target),
                "criteria": trail.criteria,
                "source": source,
                "scope": scope,
                "layers": trail.layers,
            })
        }).collect::<Vec<_>>();
        let verified_gate = bs::effective_gate_trail(&conn, &row, TaskStatus::Verified, &groups);
        let verification = crate::db::verification_store::coverage(&conn, &row, &verified_gate.criteria)?;
        Ok(Some((row, children, messages, artifacts, asset_links, gate_requirements, verification)))
    })
    .await;
    match joined {
        Ok(Ok(Some((row, children, messages, artifacts, asset_links, gate_requirements, verification)))) => {
            // Weak ETag for read-modify-write callers (AMUX-1711 parity).
            let mut headers = HeaderMap::new();
            if let Ok(v) = format!("W/\"{}-{}\"", row.id, row.rev).parse() {
                headers.insert("etag", v);
            }
            let mut body = detail_body(&row);
            body["children"] = json!(children);
            body["messages"] = json!(messages);
            body["artifacts"] = json!(artifacts);
            body["asset_links"] = json!(asset_links);
            body["gate_requirements"] = json!(gate_requirements);
            body["verification"] = verification;
            (StatusCode::OK, headers, Json(body)).into_response()
        }
        Ok(Ok(None)) => not_found(&id),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DecomposeTask {
    title: String,
    #[serde(default, alias = "desc")]
    description: String,
    #[serde(default = "default_decompose_type", rename = "type")]
    item_type: String,
    priority: u8,
    #[serde(default)]
    depends_on: Vec<DepRef>,
    next_action: String,
    /// Plain-language success conditions carried on the card itself. These are
    /// deliberately not the typed `_amux_criteria` verifier objects: a
    /// decomposition is authored by the worker that will execute it, while
    /// typed criteria enforce independent authorship. This field is the
    /// worker-visible draft the independent reviewer can turn into verifiers.
    #[serde(default)]
    acceptance_criteria: Vec<TextRef>,
}

/// One `depends_on` entry AS THE CALLER WROTE IT (AF-523).
///
/// The field is a 1-based index into the sibling `tasks` array, and this used
/// to be typed `Vec<usize>` — so a caller who sent something else was answered
/// by SERDE, at the body layer, before the handler that owns the real message
/// ever ran. The same mistake made two ways got two qualities of answer, and
/// the worse one came first:
///
///   depends_on: ["[incident] RCA doc: ..."]  -> 422, axum's raw rejection:
///     "tasks[1].depends_on[0]: invalid type: string \"...\", expected usize"
///   depends_on: [0]                          -> 400, amux's own:
///     "task 2 dependency 0 must name an earlier task by 1-based index"
///
/// Measured 2026-09-06 by the daily log sweep: `backend` hit the first three
/// times in 42 seconds, then the second, then got it right — four attempts for
/// one field. `expected usize` is true and does not say that the usize is a
/// position in the array they just wrote. Naming the thing you depend on is
/// what `depends_on` means everywhere else on this board, so it is the honest
/// guess, and it was the one answered by the framework.
///
/// Accepting the value here does not accept the MISTAKE — it routes it to
/// `validate_decomposition`, which already has the sentence worth reading.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum DepRef {
    Index(usize),
    Other(serde_json::Value),
}

/// Preserve a malformed criterion until amux can explain the expected shape.
/// A `Vec<String>` would let serde answer first with an opaque 422, the exact
/// failure AF-523 fixed for `depends_on` one field above.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum TextRef {
    Text(String),
    Other(serde_json::Value),
}

impl TextRef {
    fn shown(&self) -> String {
        match self {
            TextRef::Text(s) => format!("{s:?}"),
            TextRef::Other(v) => v.to_string(),
        }
    }
}

impl DepRef {
    /// How the caller wrote it, for an error message that quotes them back.
    fn shown(&self) -> String {
        match self {
            DepRef::Index(n) => n.to_string(),
            DepRef::Other(v) => v.to_string(),
        }
    }
}

fn default_decompose_type() -> String {
    "code".into()
}

#[derive(Debug, Clone, Deserialize)]
struct DecomposeBody {
    tasks: Vec<DecomposeTask>,
}

#[derive(Debug)]
struct ValidatedDecomposition {
    dependencies: Vec<Vec<usize>>,
    acceptance_criteria: Vec<Vec<String>>,
    plan_sha256: String,
}

/// Validate, and return the NORMALIZED 1-based dependency indices per task.
///
/// Returning them rather than re-reading `depends_on` downstream is what makes
/// the "not an index" branch impossible to reach twice: after this returns Ok
/// there is no `DepRef::Other` left in play, so the child-creation loop has no
/// can't-happen arm to guess at (AF-523).
fn validate_decomposition(tasks: &[DecomposeTask]) -> Result<ValidatedDecomposition, String> {
    if !(2..=50).contains(&tasks.len()) {
        return Err("decomposition requires 2 to 50 child tasks".into());
    }
    let mut out: Vec<Vec<usize>> = Vec::with_capacity(tasks.len());
    let mut all_criteria: Vec<Vec<String>> = Vec::with_capacity(tasks.len());
    let mut titles = std::collections::HashSet::new();
    for (idx, task) in tasks.iter().enumerate() {
        let n = idx + 1;
        if task.title.trim().is_empty() {
            return Err(format!("task {n} has an empty title"));
        }
        if !titles.insert(task.title.trim().to_ascii_lowercase()) {
            return Err(format!(
                "task {n} repeats the title {:?}; every child must be independently identifiable",
                task.title.trim()
            ));
        }
        if bs::continuation_verdict(&task.description) != bs::ContinuationVerdict::Ok {
            return Err(format!(
                "task {n} needs a concrete description of at least three words"
            ));
        }
        if !bs::KNOWN_TYPES.contains(&task.item_type.as_str()) || task.item_type == "epic" {
            return Err(format!(
                "task {n} has invalid leaf type {:?}",
                task.item_type
            ));
        }
        if task.priority > 3 {
            return Err(format!("task {n} priority must be 0 (highest) through 3"));
        }
        if bs::continuation_verdict(&task.next_action) != bs::ContinuationVerdict::Ok {
            return Err(format!(
                "task {n} needs a concrete next_action of at least three words"
            ));
        }
        if task.acceptance_criteria.is_empty() || task.acceptance_criteria.len() > 12 {
            return Err(format!(
                "task {n} needs 1 to 12 acceptance_criteria so its terminal state is falsifiable"
            ));
        }
        let mut criteria = Vec::with_capacity(task.acceptance_criteria.len());
        let mut seen_criteria = std::collections::HashSet::new();
        for (criterion_idx, criterion) in task.acceptance_criteria.iter().enumerate() {
            let TextRef::Text(text) = criterion else {
                return Err(format!(
                    "task {n} acceptance_criteria entry {} must be a string, got {}",
                    criterion_idx + 1,
                    criterion.shown()
                ));
            };
            let text = text.trim();
            if bs::continuation_verdict(text) != bs::ContinuationVerdict::Ok {
                return Err(format!(
                    "task {n} acceptance_criteria entry {} must be a concrete condition of at least three words",
                    criterion_idx + 1
                ));
            }
            if !seen_criteria.insert(text.to_ascii_lowercase()) {
                return Err(format!(
                    "task {n} repeats acceptance criterion {:?}",
                    text
                ));
            }
            criteria.push(text.to_string());
        }
        let mut seen = std::collections::HashSet::new();
        let mut deps: Vec<usize> = Vec::with_capacity(task.depends_on.len());
        for dep in &task.depends_on {
            let DepRef::Index(dep) = dep else {
                return Err(format!(
                    "task {n} dependency {} must name an earlier task by 1-based index \
                     (a POSITION in this request's `tasks` array, 1 for the first), not a \
                     card id or a title",
                    dep.shown()
                ));
            };
            if *dep == 0 || *dep >= n {
                return Err(format!(
                    "task {n} dependency {dep} must name an earlier task by 1-based index"
                ));
            }
            if !seen.insert(*dep) {
                return Err(format!("task {n} repeats dependency {dep}"));
            }
            deps.push(*dep);
        }
        out.push(deps);
        all_criteria.push(criteria);
    }

    // A retry is idempotent only when the normalized PLAN is the same. The
    // hash is written on the root epic before the transaction commits, so a
    // lost response, a restart, and a concurrent retry all share one durable
    // discriminator. Mutable child prose/status never participates.
    let canonical = tasks
        .iter()
        .enumerate()
        .map(|(idx, task)| {
            json!({
                "title": task.title.trim(),
                "description": task.description.trim(),
                "type": task.item_type,
                "priority": task.priority,
                "depends_on": out[idx],
                "next_action": task.next_action.trim(),
                "acceptance_criteria": all_criteria[idx],
            })
        })
        .collect::<Vec<_>>();
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(serde_json::to_vec(&canonical).expect("JSON values always serialize"));
    let plan_sha256 = format!("{:x}", digest.finalize());

    Ok(ValidatedDecomposition {
        dependencies: out,
        acceptance_criteria: all_criteria,
        plan_sha256,
    })
}

fn decomposition_plan_sha256(log: Option<&str>) -> Option<String> {
    log.unwrap_or_default()
        .split_whitespace()
        .rev()
        .find_map(|token| token.strip_prefix("plan_sha256="))
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
}

/// POST /api/board/{id}/decompose — atomically turn a capture shell into an
/// epic and create its ordered, attributed child plan.
async fn decompose_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<DecomposeBody>,
) -> Response {
    let validated = match validate_decomposition(&body.tasks) {
        Ok(d) => d,
        Err(why) => {
            tracing::warn!(
                target: "amux::board",
                epic = %id,
                verdict = "invalid_plan",
                measured = true,
                n_considered = body.tasks.len(),
                reason = %why,
                "board capture decomposition refused"
            );
            return err(StatusCode::BAD_REQUEST, json!({
                "error": why,
                "item": id,
                "verdict": "invalid_plan",
                "measured": true,
                "n_considered": body.tasks.len(),
            }));
        }
    };
    let (_, actor) = actor_from_headers(&headers);
    if actor == "api-anonymous" {
        return err(
            StatusCode::UNAUTHORIZED,
            json!({"error":"decomposition requires X-Amux-Worker or X-Amux-Session attribution"}),
        );
    }

    enum Out {
        Missing,
        NotCapture,
        WrongOwner(String),
        Already(IssueRow, Vec<IssueRow>, Option<String>),
        Created(IssueRow, Vec<IssueRow>),
    }
    let tasks = body.tasks;
    let dep_indices = validated.dependencies;
    let acceptance_criteria = validated.acceptance_criteria;
    let plan_sha256 = validated.plan_sha256;
    let submitted_plan_sha256 = plan_sha256.clone();
    let id_w = id.clone();
    let actor_w = actor.clone();
    let slot: Arc<Mutex<Option<Out>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(mut parent) = bs::get_issue(conn, &id_w)? else {
                return finish(&slot_w, Out::Missing, no_write());
            };
            let child_ids = {
                let mut stmt = conn.prepare(
                    "SELECT id FROM issues WHERE epic=?1 AND deleted IS NULL ORDER BY created,id",
                )?;
                let ids = stmt
                    .query_map(rusqlite::params![id_w], |r| r.get::<_, String>(0))?
                    .flatten()
                    .collect::<Vec<_>>();
                ids
            };
            if parent.item_type == "epic" && !child_ids.is_empty() {
                let children = child_ids
                    .iter()
                    .filter_map(|child| bs::get_issue(conn, child).ok().flatten())
                    .collect();
                let existing_plan_sha256 = decomposition_plan_sha256(parent.log.as_deref());
                return finish(
                    &slot_w,
                    Out::Already(parent, children, existing_plan_sha256),
                    no_write(),
                );
            }
            if parent.source.as_deref() != Some("capture")
                && !parent.desc.trim_start().starts_with("**Prompt:**")
            {
                return finish(&slot_w, Out::NotCapture, no_write());
            }
            if parent
                .session
                .as_deref()
                .is_some_and(|owner| owner != actor_w)
            {
                return finish(
                    &slot_w,
                    Out::WrongOwner(parent.session.clone().unwrap_or_default()),
                    no_write(),
                );
            }

            let now = now_secs();
            let stamp = chrono::Local::now().format("%H:%M").to_string();
            parent.item_type = "epic".into();
            parent.updated = now;
            parent.rev += 1;
            parent.version += 1;
            parent.log = Some(bs::append_log(
                parent.log.as_deref(),
                &stamp,
                &format!(
                    "decomposed by {actor_w} into {} ordered child task(s) plan_sha256={plan_sha256}",
                    tasks.len(),
                ),
            ));
            bs::save_patched(conn, &mut parent)?;
            let mut events = vec![ev_snap(&parent, MutationKind::Updated)];
            let mut children: Vec<IssueRow> = Vec::with_capacity(tasks.len());
            for (idx, task) in tasks.iter().enumerate() {
                let deps = dep_indices[idx]
                    .iter()
                    .map(|n| children[*n - 1].id.clone())
                    .collect::<Vec<_>>();
                let new = bs::NewIssue {
                    title: task.title.trim().to_string(),
                    desc: task.description.trim().to_string(),
                    status: if deps.is_empty() {
                        "todo".into()
                    } else {
                        "backlog".into()
                    },
                    session: parent.session.clone().or_else(|| Some(actor_w.clone())),
                    item_type: task.item_type.clone(),
                    creator: actor_w.clone(),
                    owner_type: "agent".into(),
                    due: None,
                    due_time: None,
                    reviewer: None,
                    shepherd: None,
                    gate: Vec::new(),
                    depends_on: deps,
                    tags: vec![format!("p{}", task.priority)],
                    ask_type: None,
                    ask_question: None,
                    ask_unblocks: None,
                    ask_actor: None,
                    source: Some("decomposition".into()),
                    requested_by: None,
                    callback_session: None,
                    callback_prompt: None,
                };
                let mut child = bs::create_issue(conn, &new, now)?;
                child.epic = Some(parent.id.clone());
                child.next_action = Some(task.next_action.trim().to_string());
                child.acceptance_criteria = Some(
                    serde_json::to_string(&acceptance_criteria[idx])
                        .expect("validated string criteria always serialize"),
                );
                child.log = Some(bs::append_log(
                    child.log.as_deref(),
                    &stamp,
                    &format!(
                        "created by {actor_w} from epic {} as plan step {} (priority p{})",
                        parent.id,
                        idx + 1,
                        task.priority
                    ),
                ));
                child.updated = now;
                child.rev += 1;
                child.version += 1;
                bs::save_patched(conn, &mut child)?;
                events.push(ev_snap(&child, MutationKind::Created));
                children.push(child);
            }
            finish(
                &slot_w,
                Out::Created(parent, children),
                WriteOutcome {
                    applied: true,
                    events,
                },
            )
        })
        .await;
    if let Err(e) = write {
        return internal(e);
    }
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        Some(Out::Missing) => not_found(&id),
        Some(Out::NotCapture) => err(
            StatusCode::CONFLICT,
            json!({"error":"only an auto-captured prompt can be decomposed atomically", "item":id}),
        ),
        Some(Out::WrongOwner(owner)) => err(
            StatusCode::FORBIDDEN,
            json!({"error":"capture belongs to another worker", "item":id, "owner":owner, "caller":actor}),
        ),
        Some(Out::Already(parent, children, existing_plan_sha256)) => {
            match existing_plan_sha256 {
                Some(existing) if existing == submitted_plan_sha256 => Json(json!({
                    "ok": true,
                    "id": parent.id,
                    "status": parent.status,
                    "epic": detail_body(&parent),
                    "tasks": children.iter().map(detail_body).collect::<Vec<_>>(),
                    "idempotent": true,
                    "idempotency_measured": true,
                    "plan_sha256": existing,
                }))
                .into_response(),
                Some(existing) => {
                    tracing::warn!(
                        target: "amux::board",
                        epic = %parent.id,
                        verdict = "plan_conflict",
                        measured = true,
                        n_considered = children.len(),
                        existing_plan_sha256 = %existing,
                        submitted_plan_sha256 = %submitted_plan_sha256,
                        "board capture decomposition retry differs from committed plan"
                    );
                    err(StatusCode::CONFLICT, json!({
                        "error": "capture already has a different decomposition plan",
                        "code": "decomposition_plan_conflict",
                        "item": parent.id,
                        "idempotent": false,
                        "idempotency_measured": true,
                        "measured": true,
                        "n_considered": children.len(),
                        "existing_plan_sha256": existing,
                        "submitted_plan_sha256": submitted_plan_sha256,
                        "existing_tasks": children.iter().map(|c| &c.id).collect::<Vec<_>>(),
                    }))
                }
                // Legacy decompositions predate the durable discriminator. Do
                // not pretend their payload was compared; preserve retry
                // compatibility and say exactly what could not be measured.
                None => Json(json!({
                    "ok": true,
                    "id": parent.id,
                    "status": parent.status,
                    "epic": detail_body(&parent),
                    "tasks": children.iter().map(detail_body).collect::<Vec<_>>(),
                    "idempotent": true,
                    "idempotency_measured": false,
                    "why_unmeasured": "this decomposition predates the durable plan fingerprint",
                }))
                .into_response(),
            }
        }
        Some(Out::Created(parent, children)) => {
            tracing::info!(
                target: "amux::board",
                epic = %parent.id,
                worker = %actor,
                children = children.len(),
                dependency_edges = children.iter().map(|c| c.depends_on.len()).sum::<usize>(),
                priorities = %children.iter().filter_map(|c| c.tags.iter().find(|t| t.starts_with('p'))).cloned().collect::<Vec<_>>().join(","),
                "board capture decomposed atomically"
            );
            (
                StatusCode::CREATED,
                Json(json!({
                    "ok": true,
                    "id": parent.id,
                    "status": parent.status,
                    "epic": detail_body(&parent),
                    "tasks": children.iter().map(detail_body).collect::<Vec<_>>(),
                    "plan_sha256": submitted_plan_sha256,
                })),
            )
                .into_response()
        }
        None => internal("decompose produced no outcome"),
    }
}

// Claim endpoint context continues below the self-contained overlap subsystem.
// ---- Durable semantic-overlap reconciliation (ATE-93) --------------------

/// The durable source record for a concern-level work election.  This is not a
/// new work primitive: it is a board relation between existing cards.  The
/// coordination id is stable across retries, process restart, and
/// provider/model switches, so a late reply can converge on the same record.
#[derive(Debug, Clone, Serialize)]
struct OverlapRecord {
    coordination_id: String,
    semantic_key: String,
    concern: String,
    owner_card_id: String,
    owner_session: String,
    resolution: String,
    resolution_note: Option<String>,
    resolved_by_card_id: Option<String>,
    resolved_by_session: Option<String>,
    resolved_at: Option<i64>,
    role: String,
    relation: String,
    newly_elected: bool,
    newly_linked: bool,
    participants: Vec<OverlapParticipant>,
    evidence: Vec<String>,
    assets: Vec<String>,
    callback: Option<OverlapCallback>,
    callbacks: Vec<OverlapCallback>,
}

#[derive(Debug, Clone, Serialize)]
struct OverlapParticipant {
    self_reported: bool,
    card_id: String,
    session: String,
    base_commit: String,
    head_commit: String,
    worktree: String,
    intent: String,
}

#[derive(Debug, Clone, Serialize)]
struct OverlapCallback {
    target_session: String,
    message_id: String,
    state: String,
    error: Option<String>,
}

/// Kept raw until the handler derives `session` from its server-stamped
/// header.  Letting a body nominate the claimant would re-create the exact
/// ownership ambiguity this endpoint is supposed to settle.
#[derive(Debug, Clone, Deserialize)]
struct OverlapBody {
    semantic_key: String,
    concern: String,
    card_id: String,
    peer_card_id: String,
    peer_session: String,
    base_commit: String,
    head_commit: String,
    worktree: String,
    intent: String,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    assets: Vec<String>,
    /// Only the elected owner may resolve a concern.  `scope-split` is the
    /// explicit escape that permits the linked peer to complete its now
    /// separate concern; `merged` deliberately does not permit a second Done.
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    resolution_note: Option<String>,
}

#[derive(Debug, Clone)]
struct OverlapInput {
    semantic_key: String,
    concern: String,
    card_id: String,
    session: String,
    peer_card_id: String,
    peer_session: String,
    base_commit: String,
    head_commit: String,
    worktree: String,
    intent: String,
    evidence: Vec<String>,
    assets: Vec<String>,
    resolution: Option<String>,
    resolution_note: Option<String>,
}

fn overlap_text(field: &str, raw: &str, min: usize, max: usize) -> Result<String, String> {
    let value = raw.trim();
    if value.len() < min || value.len() > max || value.chars().any(char::is_control) {
        return Err(format!("{field} must be {min}..={max} printable bytes"));
    }
    Ok(value.to_string())
}

fn overlap_refs(field: &str, refs: &[String]) -> Result<Vec<String>, String> {
    if refs.len() > 32 {
        return Err(format!("{field} accepts at most 32 references"));
    }
    let mut out = Vec::new();
    for reference in refs {
        let clean = overlap_text(field, reference, 1, 512)?;
        if !out.contains(&clean) {
            out.push(clean);
        }
    }
    Ok(out)
}

fn normalize_overlap(body: OverlapBody, session: String) -> Result<OverlapInput, String> {
    let semantic_key =
        overlap_text("semantic_key", &body.semantic_key, 3, 240)?.to_ascii_lowercase();
    let concern = overlap_text("concern", &body.concern, 3, 160)?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let card_id = overlap_text("card_id", &body.card_id, 3, 80)?;
    let peer_card_id = overlap_text("peer_card_id", &body.peer_card_id, 3, 80)?;
    if card_id == peer_card_id {
        return Err("card_id and peer_card_id must name distinct cards".into());
    }
    let peer_session = overlap_text("peer_session", &body.peer_session, 1, 160)?;
    let base_commit = overlap_text("base_commit", &body.base_commit, 7, 64)?;
    let head_commit = overlap_text("head_commit", &body.head_commit, 7, 64)?;
    for (name, sha) in [("base_commit", &base_commit), ("head_commit", &head_commit)] {
        if !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{name} must be a commit-shaped hexadecimal id"));
        }
    }
    let resolution = body
        .resolution
        .as_deref()
        .map(|v| overlap_text("resolution", v, 3, 32))
        .transpose()?;
    if let Some(ref value) = resolution {
        if !matches!(value.as_str(), "merged" | "scope-split" | "released") {
            return Err("resolution must be one of merged, scope-split, released".into());
        }
    }
    let resolution_note = match (resolution.as_deref(), body.resolution_note.as_deref()) {
        (Some(_), Some(note)) => Some(overlap_text("resolution_note", note, 3, 1_000)?),
        (Some(_), None) => return Err("resolution_note is required when resolving an overlap".into()),
        (None, _) => None,
    };
    Ok(OverlapInput {
        semantic_key,
        concern,
        card_id,
        session,
        peer_card_id,
        peer_session,
        base_commit,
        head_commit,
        worktree: overlap_text("worktree", &body.worktree, 1, 512)?,
        intent: overlap_text("intent", &body.intent, 3, 2_000)?,
        evidence: overlap_refs("evidence", &body.evidence)?,
        assets: overlap_refs("assets", &body.assets)?,
        resolution,
        resolution_note,
    })
}

fn overlap_coordination_id(semantic_key: &str, concern: &str) -> String {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(semantic_key.as_bytes());
    digest.update([0]);
    digest.update(concern.as_bytes());
    format!("OVL-{}", &format!("{:x}", digest.finalize())[..16])
}

fn overlap_row(
    conn: &rusqlite::Connection,
    coordination_id: &str,
    caller_card_id: &str,
    newly_elected: bool,
    newly_linked: bool,
    callback_target: Option<&str>,
) -> rusqlite::Result<Option<OverlapRecord>> {
    let header: Option<(String, String, String, String, String)> = conn
        .query_row(
            "SELECT semantic_key, concern, owner_card_id, owner_session, resolution \
             FROM board_overlap_coordination WHERE coordination_id=?1",
            [coordination_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((semantic_key, concern, owner_card_id, owner_session, resolution)) = header else {
        return Ok(None);
    };
    let (resolution_note, resolved_by_card_id, resolved_by_session, resolved_at) = conn.query_row(
        "SELECT resolution_note, resolved_by_card_id, resolved_by_session, resolved_at \
         FROM board_overlap_coordination WHERE coordination_id=?1",
        [coordination_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let mut members = conn.prepare(
        "SELECT card_id, session, base_commit, head_commit, worktree, intent, self_reported \
         FROM board_overlap_members WHERE coordination_id=?1 ORDER BY created_at, card_id",
    )?;
    let participants = members
        .query_map([coordination_id], |r| {
            Ok(OverlapParticipant {
                self_reported: r.get(6)?,
                card_id: r.get(0)?,
                session: r.get(1)?,
                base_commit: r.get(2)?,
                head_commit: r.get(3)?,
                worktree: r.get(4)?,
                intent: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut refs = conn.prepare(
        "SELECT kind, ref_value FROM board_overlap_refs WHERE coordination_id=?1 \
         ORDER BY kind, created_at, ref_value",
    )?;
    let mut evidence = Vec::new();
    let mut assets = Vec::new();
    for row in refs.query_map([coordination_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (kind, value) = row?;
        let values = if kind == "evidence" { &mut evidence } else { &mut assets };
        if !values.contains(&value) {
            values.push(value);
        }
    }
    // GET must expose the same durable callback lineage as POST. A delivered
    // steering receipt outranks the enqueue-time cache on the coordination.
    let mut callback_rows = conn.prepare(
        "SELECT target_session, message_id, \
                CASE WHEN EXISTS (SELECT 1 FROM steering_history h WHERE h.id=c.message_id) \
                     THEN 'delivered' ELSE c.state END, error \
         FROM board_overlap_callbacks c WHERE coordination_id=?1 ORDER BY target_session",
    )?;
    let callbacks = callback_rows.query_map([coordination_id], |r| {
        Ok(OverlapCallback {
            target_session: r.get(0)?, message_id: r.get(1)?, state: r.get(2)?, error: r.get(3)?,
        })
    })?.collect::<Result<Vec<_>, _>>()?;
    let callback = callbacks.iter()
        .find(|c| Some(c.target_session.as_str()) == callback_target).cloned();
    let role = if caller_card_id.is_empty() {
        "observer"
    } else if owner_card_id == caller_card_id {
        "owner"
    } else {
        "peer"
    };
    // Different concern names are not proof of an agreed scope split.
    let relation = match resolution.as_str() {
        "scope-split" | "merged" | "released" => resolution.clone(),
        _ if role == "owner" => "elected".into(),
        _ => "duplicate-linked".into(),
    };
    Ok(Some(OverlapRecord {
        coordination_id: coordination_id.to_string(),
        semantic_key,
        concern,
        owner_card_id,
        owner_session,
        resolution,
        resolution_note,
        resolved_by_card_id,
        resolved_by_session,
        resolved_at,
        role: role.to_string(),
        relation,
        newly_elected,
        newly_linked,
        participants,
        evidence,
        assets,
        callback,
        callbacks,
    }))
}

fn append_overlap_log(
    conn: &rusqlite::Connection,
    card_id: &str,
    line: &str,
    assets: &[String],
    now: i64,
    events: &mut Vec<PendingEvent>,
) -> rusqlite::Result<()> {
    let Some(mut card) = bs::get_issue(conn, card_id)? else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    card.log = Some(bs::append_log(card.log.as_deref(), &hhmm(), line));
    card.updated = now;
    card.rev += 1;
    card.version += 1;
    bs::save_patched(conn, &mut card)?;
    events.push(ev_snap(&card, MutationKind::Updated));
    for artifact in crate::db::artifact_store::insert_captured_refs(
        conn,
        card_id,
        assets.to_vec(),
        "durable board-overlap reconciliation asset",
        now,
    )? {
        events.push(PendingEvent {
            entity_type: EntityType::Other("artifact".into()),
            entity_id: artifact.id,
            mutation: MutationKind::Created,
            payload: None,
        });
    }
    Ok(())
}

/// The same predicate backs the friendly HTTP refusal, the deployment permit,
/// and migration 0057's trigger.  The trigger is the last line of defense;
/// this helper is what lets a caller see the elected concern, cards, and exit
/// instead of a raw SQLite abort.
#[derive(Debug, Clone, Serialize)]
struct OverlapCompletionBlocker {
    coordination_id: String,
    semantic_key: String,
    concern: String,
    owner_card_id: String,
    owner_session: String,
    resolution: String,
}

fn overlap_completion_blockers(
    conn: &rusqlite::Connection,
    card_id: Option<&str>,
    session: Option<&str>,
) -> rusqlite::Result<Vec<OverlapCompletionBlocker>> {
    let mut query = "SELECT c.coordination_id, c.semantic_key, c.concern, c.owner_card_id, c.owner_session, c.resolution \
                     FROM board_overlap_members m \
                     JOIN board_overlap_coordination c ON c.coordination_id=m.coordination_id \
                     WHERE m.card_id <> c.owner_card_id \
                       AND c.resolution NOT IN ('scope-split','released')".to_string();
    let mut params: Vec<String> = Vec::new();
    if let Some(card_id) = card_id {
        query.push_str(" AND m.card_id=?");
        params.push(card_id.to_string());
    }
    if let Some(session) = session {
        query.push_str(" AND m.session=?");
        params.push(session.to_string());
    }
    query.push_str(" ORDER BY c.created_at, c.coordination_id");
    let mut statement = conn.prepare(&query)?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok(OverlapCompletionBlocker {
                coordination_id: r.get(0)?,
                semantic_key: r.get(1)?,
                concern: r.get(2)?,
                owner_card_id: r.get(3)?,
                owner_session: r.get(4)?,
                resolution: r.get(5)?,
            })
        })?;
    let blockers = rows.collect::<Result<Vec<_>, _>>()?;
    Ok(blockers)
}

/// The one-writer transaction is the election.  A concurrent request can only
/// observe the elected header and become a linked peer; it cannot replace the
/// owner, erase provenance, or turn a late discovery into a second deploy.
fn record_overlap_on_conn(
    conn: &rusqlite::Connection,
    input: &OverlapInput,
    now: i64,
) -> rusqlite::Result<(OverlapRecord, Vec<PendingEvent>)> {
    let coordination_id = overlap_coordination_id(&input.semantic_key, &input.concern);
    let newly_elected = conn.execute(
        "INSERT OR IGNORE INTO board_overlap_coordination \
         (coordination_id, semantic_key, concern, owner_card_id, owner_session, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?6)",
        rusqlite::params![
            coordination_id,
            input.semantic_key,
            input.concern,
            input.card_id,
            input.session,
            now,
        ],
    )? > 0;
    let newly_linked = conn.execute(
        "INSERT INTO board_overlap_members \
         (coordination_id, card_id, session, base_commit, head_commit, worktree, intent, self_reported, created_at, last_seen_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,1,?8,?8) \
         ON CONFLICT(coordination_id, card_id) DO UPDATE SET \
            session=excluded.session, base_commit=excluded.base_commit, head_commit=excluded.head_commit, \
            worktree=excluded.worktree, intent=excluded.intent, self_reported=1, last_seen_at=excluded.last_seen_at \
         WHERE board_overlap_members.self_reported=0",
        rusqlite::params![
            coordination_id,
            input.card_id,
            input.session,
            input.base_commit,
            input.head_commit,
            input.worktree,
            input.intent,
            now,
        ],
    )? > 0;
    // Link the other card now, even when its worker is gone. Its provenance is
    // deliberately provisional until that worker self-reports; the callback
    // and the completion/deployment guard must not wait for a live provider.
    conn.execute(
        "INSERT OR IGNORE INTO board_overlap_members \
         (coordination_id, card_id, session, base_commit, head_commit, worktree, intent, self_reported, created_at, last_seen_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8,?8)",
        rusqlite::params![
            coordination_id,
            input.peer_card_id,
            input.peer_session,
            input.base_commit,
            input.head_commit,
            input.worktree,
            format!("peer identity linked by {}; self-report pending", input.session),
            now,
        ],
    )?;
    for (kind, refs) in [("evidence", &input.evidence), ("asset", &input.assets)] {
        for reference in refs {
            conn.execute(
                "INSERT OR IGNORE INTO board_overlap_refs \
                 (coordination_id, kind, ref_value, card_id, created_at) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![coordination_id, kind, reference, input.card_id, now],
            )?;
        }
    }
    let callback_target = (input.peer_session != input.session).then(|| input.peer_session.clone());
    if let Some(target) = &callback_target {
        let message_id = format!("overlap:{}:{}", coordination_id, target);
        conn.execute(
            "INSERT OR IGNORE INTO board_overlap_callbacks \
             (coordination_id, target_session, message_id, state, updated_at) VALUES (?1,?2,?3,'pending',?4)",
            rusqlite::params![coordination_id, target, message_id, now],
        )?;
    }

    let resolution_changed = if let Some(resolution) = &input.resolution {
        conn.execute(
            "UPDATE board_overlap_coordination \
             SET resolution=?1, resolution_note=?2, resolved_by_card_id=?3, \
                 resolved_by_session=?4, resolved_at=?5, updated_at=?5 \
             WHERE coordination_id=?6 AND owner_card_id=?3 AND owner_session=?4 \
               AND resolution <> ?1",
            rusqlite::params![
                resolution,
                input.resolution_note,
                input.card_id,
                input.session,
                now,
                coordination_id,
            ],
        )? > 0
    } else {
        false
    };

    crate::api::session_verbs::ensure_fleet_tables(conn)?;
    let elected = overlap_row(conn, &coordination_id, &input.card_id,
        newly_elected, newly_linked, callback_target.as_deref())?
        .expect("the election was inserted or already existed");
    let owner_label = format!("elected owner [{}] ({})", elected.owner_card_id, elected.owner_session);
    let prefix = format!("overlap coordination {coordination_id}:");
    // Refresh old, misleading lineage on an ordinary retry. Append, never
    // rewrite history; subsequent retries are idempotent.
    let mut stale_lineage = false;
    for id in [&input.card_id, &input.peer_card_id] {
        let card = bs::get_issue(conn, id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if let Some(line) = card.log.as_deref().unwrap_or("").lines().rev().find(|line| line.contains(&prefix)) {
            stale_lineage |= !line.contains(&owner_label) || !line.contains("; reporter [");
        }
    }
    let mut events = Vec::new();
    if newly_linked || resolution_changed || stale_lineage {
        let line = format!(
            "{prefix} concern={}; semantic={}; {owner_label}; reporter [{}] ({}) — linked [{}] ({}) — callback={}; base={} head={}; intent: {}; resolution: {}; resolution note: {}; evidence: {}; assets: {}",
            elected.concern,
            elected.semantic_key,
            input.card_id,
            input.session,
            input.peer_card_id,
            input.peer_session,
            callback_target
                .as_deref()
                .map(|target| format!("overlap:{coordination_id}:{target}"))
                .unwrap_or_else(|| "(same worker)".into()),
            input.base_commit,
            input.head_commit,
            input.intent,
            elected.resolution,
            elected.resolution_note.as_deref().unwrap_or("(pending)"),
            if elected.evidence.is_empty() { "(none)".into() } else { elected.evidence.join(", ") },
            if elected.assets.is_empty() { "(none)".into() } else { elected.assets.join(", ") },
        );
        append_overlap_log(conn, &input.card_id, &line, &elected.assets, now, &mut events)?;
        append_overlap_log(conn, &input.peer_card_id, &line, &elected.assets, now, &mut events)?;
        tracing::info!(marker = "board_overlap_lineage_recorded", coordination = %coordination_id,
            owner_card = %elected.owner_card_id, owner_session = %elected.owner_session,
            reporter_card = %input.card_id, resolution = %elected.resolution,
            "board overlap lineage records the durable election and the reporting worker separately");
        if stale_lineage {
            tracing::warn!(marker = "board_overlap_lineage_refreshed", coordination = %coordination_id,
                owner_card = %elected.owner_card_id, reporter_card = %input.card_id,
                "appended corrected overlap lineage; prior history mislabeled the reporter as owner or omitted attribution");
        }
    }
    crate::api::session_verbs::ensure_fleet_tables(conn)?;
    for (session, card_id) in [
        (&input.session, &input.card_id),
        (&input.peer_session, &input.peer_card_id),
    ] {
        conn.execute(
            "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) \
             VALUES (?1,?2,'board.overlap',?3,?4,'board-overlap')",
            rusqlite::params![
                now as f64,
                session,
                json!({"coordination_id": coordination_id, "card_id": card_id, "semantic_key": input.semantic_key, "concern": input.concern}).to_string(),
                format!("overlap:{coordination_id}:{card_id}"),
            ],
        )?;
    }
    conn.execute(
        "UPDATE board_overlap_coordination SET updated_at=?1 WHERE coordination_id=?2",
        rusqlite::params![now, coordination_id],
    )?;
    let record = overlap_row(
        conn,
        &coordination_id,
        &input.card_id,
        newly_elected,
        newly_linked,
        callback_target.as_deref(),
    )?
    .expect("overlap header was inserted or already existed");
    Ok((record, events))
}

async fn update_overlap_callback(
    state: &AppState,
    coordination_id: &str,
    target_session: &str,
    next_state: &str,
    error: Option<String>,
) {
    let (coordination_id, target_session, next_state) = (
        coordination_id.to_string(),
        target_session.to_string(),
        next_state.to_string(),
    );
    let _ = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "UPDATE board_overlap_callbacks SET state=?1, error=?2, updated_at=?3 \
                 WHERE coordination_id=?4 AND target_session=?5",
                rusqlite::params![
                    next_state,
                    error,
                    now_secs(),
                    coordination_id,
                    target_session
                ],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
}

async fn reconcile_overlap(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OverlapBody>,
) -> Response {
    let (_, session) = actor_from_headers(&headers);
    if session == "api-anonymous" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "overlap reconciliation needs X-Amux-Session; ownership cannot be elected anonymously"})),
        )
            .into_response();
    }
    let input = match normalize_overlap(body, session.clone()) {
        Ok(v) => v,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
        }
    };
    let ownership = {
        let conn = match state.store.read() {
            Ok(v) => v,
            Err(e) => return internal(e),
        };
        match (
            bs::get_issue(&conn, &input.card_id),
            bs::get_issue(&conn, &input.peer_card_id),
        ) {
            (Ok(Some(own)), Ok(Some(peer))) => (
                own.session.unwrap_or_default() == input.session,
                peer.session.unwrap_or_default() == input.peer_session,
            ),
            (Ok(None), _) | (_, Ok(None)) => return not_found("card_id or peer_card_id"),
            (Err(e), _) | (_, Err(e)) => return internal(e),
        }
    };
    if !ownership.0 {
        tracing::warn!(
            marker = "board_overlap_owner_refused",
            card = %input.card_id,
            caller = %input.session,
            "board overlap reconciliation refused: caller does not own its source card"
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "caller must own card_id; peer cards are linked but never claimed"})),
        )
            .into_response();
    }
    if !ownership.1 {
        tracing::warn!(
            marker = "board_overlap_peer_refused",
            card = %input.card_id,
            peer_card = %input.peer_card_id,
            claimed_peer = %input.peer_session,
            "board overlap reconciliation refused: peer card owner does not match peer_session"
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "peer_session must match peer_card_id's current owner; reconciliation never fabricates a peer callback target",
                "code": "board_overlap_peer_owner_mismatch",
            })),
        )
            .into_response();
    }
    if input.resolution.is_some() {
        let coordination_id = overlap_coordination_id(&input.semantic_key, &input.concern);
        let conn = match state.store.read() {
            Ok(v) => v,
            Err(e) => return internal(e),
        };
        let elected: Option<(String, String)> = match conn
            .query_row(
                "SELECT owner_card_id, owner_session FROM board_overlap_coordination WHERE coordination_id=?1",
                [&coordination_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        {
            Ok(v) => v,
            Err(e) => return internal(e),
        };
        if let Some((owner_card, owner_session)) = elected {
            if owner_card != input.card_id || owner_session != input.session {
                tracing::warn!(
                    marker = "board_overlap_resolution_refused",
                    coordination = %coordination_id,
                    caller_card = %input.card_id,
                    caller = %input.session,
                    owner_card,
                    owner_session,
                    "only the elected overlap owner may release or scope-split the concern"
                );
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "only the elected owner may reconcile this overlap",
                        "code": "board_overlap_resolution_owner_required",
                        "coordination_id": coordination_id,
                    })),
                )
                    .into_response();
            }
        }
    }
    let record_slot = Arc::new(Mutex::new(None));
    let record_slot_w = record_slot.clone();
    let input_w = input.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let (record, events) = record_overlap_on_conn(conn, &input_w, now_secs())?;
            *record_slot_w.lock().expect("overlap result slot poisoned") = Some(record);
            Ok(WriteOutcome {
                applied: true,
                events,
            })
        })
        .await;
    if let Err(e) = write {
        tracing::warn!(
            marker = "board_overlap_record_failed",
            card = %input.card_id,
            peer_card = %input.peer_card_id,
            error = %e,
            "board overlap reconciliation was not persisted; no completion/deploy claim is valid"
        );
        return internal(e);
    }
    let mut record = record_slot
        .lock()
        .expect("overlap result slot poisoned")
        .clone()
        .expect("overlap transaction produced a result");
    if record.newly_elected {
        tracing::info!(
            marker = "board_overlap_elected",
            coordination = %record.coordination_id,
            semantic = %record.semantic_key,
            concern = %record.concern,
            owner_card = %record.owner_card_id,
            owner_session = %record.owner_session,
            "board overlap ownership elected atomically"
        );
    } else if record.newly_linked {
        tracing::warn!(
            marker = "board_overlap_peer_linked",
            coordination = %record.coordination_id,
            owner_card = %record.owner_card_id,
            peer_card = %input.card_id,
            "late-discovered overlapping work linked to existing owner; peer must not independently complete/deploy"
        );
    }
    let mut delivery = "not-needed".to_string();
    if let Some(callback) = record.callback.clone() {
        if matches!(callback.state.as_str(), "pending" | "retryable") {
            let notice = format!(
                "[Board overlap {}] semantic concern `{}` is {}. Owner: [{}] / {}. Linked cards: [{}] and [{}]. Base {}, head {}. Intent: {}. Evidence: {}. Assets: {}. Reconcile before completion or deployment; this durable callback survives provider/model changes.",
                record.coordination_id,
                record.concern,
                record.relation,
                record.owner_card_id,
                record.owner_session,
                input.card_id,
                input.peer_card_id,
                input.base_commit,
                input.head_commit,
                input.intent,
                if record.evidence.is_empty() { "(none)".into() } else { record.evidence.join(", ") },
                if record.assets.is_empty() { "(none)".into() } else { record.assets.join(", ") },
            );
            match crate::api::session_verbs::steer_enqueue_idempotent(
                &state,
                &callback.target_session,
                &notice,
                &format!("board-overlap:{}", record.coordination_id),
                &input.session,
                &callback.message_id,
            )
            .await
            {
                Ok(_) => {
                    update_overlap_callback(
                        &state,
                        &record.coordination_id,
                        &callback.target_session,
                        "queued",
                        None,
                    )
                    .await;
                    delivery = "queued".into();
                }
                Err(reason) => {
                    tracing::warn!(
                        marker = "board_overlap_callback_retryable",
                        coordination = %record.coordination_id,
                        target = %callback.target_session,
                        reason,
                        "board overlap is durable but its peer callback was not queued; retry this request after the peer is recoverable"
                    );
                    update_overlap_callback(
                        &state,
                        &record.coordination_id,
                        &callback.target_session,
                        "retryable",
                        Some(reason.to_string()),
                    )
                    .await;
                    delivery = "retryable".into();
                }
            }
            let conn = match state.store.read() {
                Ok(v) => v,
                Err(e) => return internal(e),
            };
            record = match overlap_row(
                &conn,
                &record.coordination_id,
                &input.card_id,
                record.newly_elected,
                record.newly_linked,
                Some(&callback.target_session),
            ) {
                Ok(Some(v)) => v,
                Ok(None) => return internal("overlap record disappeared after callback update"),
                Err(e) => return internal(e),
            };
        } else {
            delivery = callback.state;
        }
    }
    let status = if delivery == "retryable" {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    (
        status,
        Json(json!({"ok": true, "coordination": record, "delivery": delivery})),
    )
        .into_response()
}

async fn get_overlap(
    State(state): State<AppState>,
    Path(coordination_id): Path<String>,
) -> Response {
    let conn = match state.store.read() {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    match overlap_row(&conn, &coordination_id, "", false, false, None) {
        Ok(Some(record)) => Json(json!({"ok": true, "coordination": record})).into_response(),
        Ok(None) => not_found(&coordination_id),
        Err(e) => internal(e),
    }
}

/// Builder/push preflight.  The actor is the committing worker named by the
/// commit trailer, not the process that happens to run the builder, so a stale
/// shared-checkout head cannot deploy a linked peer's concern under another
/// session's identity.  `measured` travels with the result: an unavailable
/// preflight must not be mistaken for permission by the caller.
async fn overlap_deployment_permit(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let session = q.get("session").map(String::as_str).unwrap_or("").trim();
    if session.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "session query parameter required", "measured": false})),
        )
            .into_response();
    }
    let conn = match state.store.read() {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": e.to_string(), "measured": false})),
            )
                .into_response()
        }
    };
    match overlap_completion_blockers(&conn, None, Some(session)) {
        Ok(blockers) if blockers.is_empty() => Json(json!({
            "allowed": true,
            "measured": true,
            "session": session,
            "blockers": [],
        }))
        .into_response(),
        Ok(blockers) => {
            tracing::warn!(
                marker = "board_overlap_deploy_refused",
                session,
                blockers = blockers.len(),
                coordination = %blockers[0].coordination_id,
                "deployment preflight refused: worker is a linked non-owner on an unreconciled concern"
            );
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "allowed": false,
                    "measured": true,
                    "session": session,
                    "blockers": blockers,
                    "how_to_fix": "The elected owner must record a scope-split or released reconciliation on /api/board/overlap before this worker independently deploys.",
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": e.to_string(), "measured": false, "session": session})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod overlap_reconciliation_tests {
    use super::*;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn store() -> crate::db::SharedStore {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("overlap.db")).unwrap());
        // The writer and read pool outlive this helper.  The OS cleans the test
        // directory, but the Store must retain a valid path for this test.
        std::mem::forget(dir);
        store
    }

    fn state(store: crate::db::SharedStore) -> AppState {
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn new_card(session: &str) -> bs::NewIssue {
        bs::NewIssue {
            title: format!("overlap card for {session}"),
            desc: "durable semantic overlap test card".into(),
            status: "doing".into(),
            session: Some(session.into()),
            item_type: "code".into(),
            creator: session.into(),
            owner_type: "agent".into(),
            due: None,
            due_time: None,
            reviewer: None,
            shepherd: None,
            depends_on: vec![],
            gate: vec![],
            tags: vec![],
            ask_type: None,
            ask_question: None,
            ask_unblocks: None,
            ask_actor: None,
            source: Some("agent".into()),
            requested_by: None,
            callback_session: None,
            callback_prompt: None,
        }
    }

    fn seed_card(store: &crate::db::SharedStore, session: &str) -> String {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        let card = new_card(session);
        store
            .write(move |conn| {
                let row = bs::create_issue(conn, &card, 1_700_000_000)?;
                *slot_w.lock().unwrap() = Some(row.id);
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .unwrap();
        let id = slot.lock().unwrap().clone().unwrap();
        id
    }

    fn input(
        card_id: String,
        session: &str,
        peer_card_id: String,
        peer_session: &str,
        concern: &str,
    ) -> OverlapInput {
        OverlapInput {
            semantic_key: "deployment:31768303d1d8:7c6f7b802f5a".into(),
            concern: concern.into(),
            card_id,
            session: session.into(),
            peer_card_id,
            peer_session: peer_session.into(),
            base_commit: "31768303d1d89d8db9d51aa728e8deb80ca884c5".into(),
            head_commit: "7c6f7b802f5af15cb9f4e4c4848a8329e1b72c7b".into(),
            worktree: "/tmp/overlap-fixture".into(),
            intent:
                "reconcile a divergent stale-base deployment without overwriting either concern"
                    .into(),
            evidence: vec!["git show --name-only 7c6f7b80".into()],
            assets: vec!["crates/amux-server/src/runtime_jobs/board_drive.rs".into()],
            resolution: None,
            resolution_note: None,
        }
    }

    async fn record(store: crate::db::SharedStore, input: OverlapInput) -> OverlapRecord {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        store
            .write_async(move |conn| {
                let (record, events) = record_overlap_on_conn(conn, &input, 1_700_000_001)?;
                *slot_w.lock().unwrap() = Some(record);
                Ok(WriteOutcome {
                    applied: true,
                    events,
                })
            })
            .await
            .unwrap();
        let result = slot.lock().unwrap().clone().unwrap();
        result
    }

    #[tokio::test]
    async fn collision_31768303_vs_7c6f7b80_elects_one_owner_atomically() {
        let db = store();
        let left = seed_card(&db, "handoff-producer-0907");
        let right = seed_card(&db, "handoff-consumer-0907");
        let (a, b) = tokio::join!(
            record(
                db.clone(),
                input(
                    left.clone(),
                    "handoff-producer-0907",
                    right.clone(),
                    "handoff-consumer-0907",
                    "deployment adoption"
                ),
            ),
            record(
                db.clone(),
                input(
                    right,
                    "handoff-consumer-0907",
                    left,
                    "handoff-producer-0907",
                    "deployment adoption"
                ),
            ),
        );
        assert_ne!(
            a.newly_elected, b.newly_elected,
            "exactly one writer wins the SQLite election"
        );
        assert_eq!(a.coordination_id, b.coordination_id);
        assert_eq!(a.owner_card_id, b.owner_card_id);
        assert_eq!(a.owner_session, b.owner_session);
        assert_eq!(
            a.participants.len(),
            2,
            "both late-discovered workers remain visible"
        );
    }

    #[tokio::test]
    async fn distinct_concerns_keep_independent_elections_until_explicit_resolution() {
        let db = store();
        let a = seed_card(&db, "runtime-owner");
        let b = seed_card(&db, "invariant-owner");
        let runtime = record(
            db.clone(),
            input(
                a.clone(),
                "runtime-owner",
                b.clone(),
                "invariant-owner",
                "runtime truth and recovery",
            ),
        )
        .await;
        let invariants = record(
            db,
            input(
                b,
                "invariant-owner",
                a,
                "runtime-owner",
                "invariant measured truth",
            ),
        )
        .await;
        assert_eq!(runtime.relation, "elected");
        assert_eq!(invariants.relation, "elected");
        assert_ne!(runtime.coordination_id, invariants.coordination_id);
    }

    #[tokio::test]
    async fn duplicate_cards_evidence_and_assets_converge_without_overwrite() {
        let db = store();
        let a = seed_card(&db, "first-worker");
        let b = seed_card(&db, "second-worker");
        let first = record(
            db.clone(),
            input(
                a.clone(),
                "first-worker",
                b.clone(),
                "second-worker",
                "same deploy concern",
            ),
        )
        .await;
        let mut second_input = input(b, "second-worker", a, "first-worker", "same deploy concern");
        second_input.evidence = vec!["git merge-base --is-ancestor 31768303 7c6f7b80 -> 1".into()];
        second_input.assets = vec!["crates/amux-server/src/invariants/checks.rs".into()];
        let second = record(db, second_input).await;
        assert_eq!(second.coordination_id, first.coordination_id);
        assert_eq!(second.role, "peer");
        assert_eq!(second.participants.len(), 2);
        assert!(second
            .evidence
            .iter()
            .any(|v| v.contains("show --name-only")));
        assert!(second.evidence.iter().any(|v| v.contains("merge-base")));
        assert!(second.assets.iter().any(|v| v.ends_with("board_drive.rs")));
        assert!(second.assets.iter().any(|v| v.ends_with("checks.rs")));
    }

    #[tokio::test]
    async fn late_peer_log_names_the_elected_owner_not_the_reporting_peer() {
        let db = store();
        let owner = seed_card(&db, "first-worker");
        let peer = seed_card(&db, "second-worker");
        record(db.clone(), input(owner.clone(), "first-worker", peer.clone(), "second-worker", "same concern")).await;
        let result = record(db.clone(), input(peer.clone(), "second-worker", owner.clone(), "first-worker", "same concern")).await;
        assert_eq!(result.owner_card_id, owner);
        let conn = db.read().unwrap();
        for card in [&owner, &peer] {
            let row = bs::get_issue(&conn, card).unwrap().unwrap();
            let log = row.log.unwrap();
            let latest = log.lines().last().unwrap();
            assert!(latest.contains(&format!("elected owner [{owner}] (first-worker)")), "lineage contradicts election: {latest}");
            assert!(latest.contains(&format!("reporter [{peer}] (second-worker)")), "reporter attribution lost: {latest}");
        }
    }

    #[tokio::test]
    async fn readback_keeps_resolution_provenance_and_both_durable_callbacks() {
        let db = store();
        let owner = seed_card(&db, "first-worker");
        let peer = seed_card(&db, "second-worker");
        let mut report = input(owner.clone(), "first-worker", peer.clone(), "second-worker", "same concern");
        let elected = record(db.clone(), report.clone()).await;
        let consumer = input(peer.clone(), "second-worker", owner.clone(), "first-worker", "same concern");
        record(db.clone(), consumer.clone()).await;
        report.resolution = Some("scope-split".into());
        report.resolution_note = Some("first-worker owns server; second-worker owns dashboard".into());
        record(db.clone(), report.clone()).await;
        let app = Router::new().nest("/api/board", routes()).with_state(state(db.clone()));
        let (status, body) = call(&app, "GET", &format!("/api/board/overlap/{}", elected.coordination_id), "first-worker", None).await;
        assert_eq!(status, StatusCode::OK);
        let row = &body["coordination"];
        assert_eq!(row["resolution_note"], report.resolution_note.unwrap());
        assert_eq!(row["resolved_by_card_id"], owner);
        assert_eq!(row["relation"], "scope-split");
        assert_eq!(row["callbacks"].as_array().unwrap().len(), 2);
        let refs = row["evidence"].as_array().unwrap();
        assert_eq!(refs.len(), consumer.evidence.len(), "shared references are a union, with provenance retained in storage");
        assert_eq!(row["participants"][0]["self_reported"], true);
        // Independent concern names are not evidence that a split was agreed.
        let other = record(db.clone(), input(peer, "second-worker", owner, "first-worker", "another concern")).await;
        assert_eq!(other.resolution, "pending");
        assert_ne!(other.relation, "scope-split");
    }

    #[tokio::test]
    async fn retry_appends_one_correction_to_legacy_lineage_without_rewriting_history() {
        let db = store();
        let owner = seed_card(&db, "first-worker");
        let peer = seed_card(&db, "second-worker");
        let owner_report = input(owner.clone(), "first-worker", peer.clone(), "second-worker", "same concern");
        let peer_report = input(peer.clone(), "second-worker", owner.clone(), "first-worker", "same concern");
        let elected = record(db.clone(), owner_report).await;
        record(db.clone(), peer_report.clone()).await;
        let wrong = format!("overlap coordination {}: elected owner [{}] (second-worker)", elected.coordination_id, peer);
        let wrong_w = wrong.clone();
        let cards = [owner.clone(), peer.clone()];
        db.write(move |conn| {
            for id in cards {
                conn.execute("UPDATE issues SET log=?1 WHERE id=?2", rusqlite::params![wrong_w, id])?;
            }
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
        record(db.clone(), peer_report.clone()).await;
        let corrected = bs::get_issue(&db.read().unwrap(), &peer).unwrap().unwrap().log.unwrap();
        assert!(corrected.starts_with(&wrong), "legacy history must remain intact");
        assert_eq!(corrected.lines().count(), 2, "retry must append a correction");
        assert!(corrected.lines().last().unwrap().contains(&format!("elected owner [{owner}] (first-worker)")));
        record(db.clone(), peer_report).await;
        for id in [owner, peer] {
            assert_eq!(bs::get_issue(&db.read().unwrap(), &id).unwrap().unwrap().log.unwrap(), corrected,
                "replay must correct both cards exactly once");
        }
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        session: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("x-amux-session", session);
        let payload = if let Some(body) = body {
            request = request.header("content-type", "application/json");
            axum::body::Body::from(body.to_string())
        } else {
            axum::body::Body::empty()
        };
        let response = app
            .clone()
            .oneshot(request.body(payload).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post(app: &Router, session: &str, body: Value) -> (StatusCode, Value) {
        call(app, "POST", "/api/board/overlap", session, Some(body)).await
    }

    #[tokio::test]
    async fn vanished_peer_and_model_switch_keep_the_handoff_callback_recoverable() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::api::settings::test_env::set_home(home.path());
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let db = store();
        let producer = seed_card(&db, "handoff-producer-0907");
        let consumer = seed_card(&db, "handoff-consumer-0907");
        let app = Router::new()
            .nest("/api/board", routes())
            .with_state(state(db.clone()));
        let payload = json!({
                "semantic_key": "controlled-handoff-0907",
                "concern": "deployment adoption",
                "card_id": producer,
                "peer_card_id": consumer,
                "peer_session": "handoff-consumer-0907",
                "base_commit": "31768303d1d89d8db9d51aa728e8deb80ca884c5",
                "head_commit": "7c6f7b802f5af15cb9f4e4c4848a8329e1b72c7b",
                "worktree": "/tmp/handoff-producer-0907",
                "intent": "controlled producer to consumer handoff",
                "evidence": ["producer evidence"],
                "assets": ["crates/amux-server/src/api/board.rs"]
            });
        let mut mismatched_peer = payload.clone();
        mismatched_peer["peer_session"] = json!("wrong-worker");
        let (forbidden, rejected) = post(&app, "handoff-producer-0907", mismatched_peer).await;
        assert_eq!(forbidden, StatusCode::FORBIDDEN);
        assert_eq!(rejected["code"], "board_overlap_peer_owner_mismatch");

        // Missing worker state is a durable retry, never a fabricated delivery.
        let (missing, pending) = post(&app, "handoff-producer-0907", payload.clone()).await;
        assert_eq!(missing, StatusCode::ACCEPTED);
        assert_eq!(pending["coordination"]["callback"]["state"], "retryable");
        assert_eq!(pending["coordination"]["callback"]["error"], "no-env-file");
        // Supply the fixture's own persisted identity; never depend on a real
        // worker's env file or whichever AMUX_HOME another test selected.
        std::fs::write(home.path().join("sessions/handoff-consumer-0907.env"), "CC_TAGS=\"test\"\n").unwrap();
        let (status, reply) = post(&app, "handoff-producer-0907", payload.clone()).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a recovered consumer is a durable queued handoff: {reply}"
        );
        assert_eq!(reply["delivery"], "queued");
        assert_eq!(
            reply["coordination"]["callback"]["target_session"],
            "handoff-consumer-0907"
        );
        let message_id = reply["coordination"]["callback"]["message_id"]
            .as_str()
            .unwrap()
            .to_string();
        let conn = db.read().unwrap();
        let queued: String = conn
            .query_row(
                "SELECT id FROM steering_queue WHERE id=?1 AND session='handoff-consumer-0907'",
                [&message_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            queued, message_id,
            "callback is keyed by durable worker name, not provider/model process state"
        );
        let (retry_status, retry) = post(&app, "handoff-producer-0907", payload).await;
        assert_eq!(retry_status, StatusCode::OK);
        assert_eq!(retry["coordination"]["callback"]["message_id"], message_id);
        let callbacks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM steering_queue WHERE id=?1 AND session='handoff-consumer-0907'",
                [&message_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(callbacks, 1, "a retry/model switch refreshes one stable callback, never duplicates delivery");
    }

    #[tokio::test]
    async fn linked_peer_cannot_complete_or_deploy_until_owner_scope_splits() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::api::settings::test_env::set_home(home.path());
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        for worker in ["handoff-producer-0907", "handoff-consumer-0907"] {
            std::fs::write(home.path().join(format!("sessions/{worker}.env")), "CC_TAGS=\"test\"\n").unwrap();
        }
        let db = store();
        let producer = seed_card(&db, "handoff-producer-0907");
        let consumer = seed_card(&db, "handoff-consumer-0907");
        let app = Router::new()
            .nest("/api/board", routes())
            .with_state(state(db));
        let payload = json!({
            "semantic_key": "controlled-handoff-0907",
            "concern": "deployment adoption",
            "card_id": producer,
            "peer_card_id": consumer,
            "peer_session": "handoff-consumer-0907",
            "base_commit": "31768303d1d89d8db9d51aa728e8deb80ca884c5",
            "head_commit": "7c6f7b802f5af15cb9f4e4c4848a8329e1b72c7b",
            "worktree": "/tmp/handoff-producer-0907",
            "intent": "controlled producer to consumer handoff",
            "evidence": ["git merge-base --is-ancestor 31768303 7c6f7b80 -> 1"],
            "assets": ["crates/amux-server/src/api/board.rs"]
        });
        let (created, overlap) = post(&app, "handoff-producer-0907", payload.clone()).await;
        assert_eq!(created, StatusCode::OK);
        let coordination_id = overlap["coordination"]["coordination_id"].as_str().unwrap();
        let producer_card = payload["card_id"].as_str().unwrap().to_string();
        let consumer_card = payload["peer_card_id"].as_str().unwrap().to_string();
        // Both cards carry the same durable coordination, reciprocal card id,
        // source callback id, and merged asset text.  This pins the UI-facing
        // links rather than only the hidden election rows.
        for (card, peer) in [
            (producer_card.as_str(), consumer_card.as_str()),
            (consumer_card.as_str(), producer_card.as_str()),
        ] {
            let (status, detail) = call(
                &app,
                "GET",
                &format!("/api/board/{card}"),
                "handoff-producer-0907",
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let text = detail.to_string();
            assert!(text.contains(coordination_id), "card must link the coordination: {text}");
            assert!(text.contains(peer), "card must link its peer card: {text}");
            assert!(text.contains("overlap:"), "card must link the durable callback/message: {text}");
            assert!(text.contains("board.rs"), "card must surface the merged asset reference: {text}");
        }

        let (blocked, completion) = call(
            &app,
            "PATCH",
            &format!("/api/board/{consumer_card}"),
            "handoff-consumer-0907",
            Some(json!({"status":"done", "force":true, "reason":"must not bypass overlap"})),
        )
        .await;
        assert_eq!(blocked, StatusCode::CONFLICT);
        assert_eq!(completion["code"], "board_overlap_completion_blocked");

        let (blocked_verified, verified_completion) = call(
            &app,
            "PATCH",
            &format!("/api/board/{consumer_card}"),
            "handoff-consumer-0907",
            Some(json!({"status":"verified", "force":true, "reason":"must not bypass overlap"})),
        )
        .await;
        assert_eq!(blocked_verified, StatusCode::CONFLICT);
        assert_eq!(
            verified_completion["code"],
            "board_overlap_completion_blocked"
        );

        let (deployment, permit) = call(
            &app,
            "GET",
            "/api/board/overlap/deployment-permit?session=handoff-consumer-0907",
            "handoff-consumer-0907",
            None,
        )
        .await;
        assert_eq!(deployment, StatusCode::CONFLICT);
        assert_eq!(permit["allowed"], false);
        assert_eq!(permit["blockers"][0]["coordination_id"], overlap["coordination"]["coordination_id"]);

        let mut resolve = payload;
        resolve["resolution"] = json!("scope-split");
        resolve["resolution_note"] = json!("consumer owns a distinct deployment concern after explicit review");
        let (resolved, _) = post(&app, "handoff-producer-0907", resolve).await;
        assert_eq!(resolved, StatusCode::OK, "only the elected owner can release the peer");

        let (allowed, completion) = call(
            &app,
            "PATCH",
            &format!("/api/board/{consumer_card}"),
            "handoff-consumer-0907",
            Some(json!({"status":"done", "force":true, "reason":"explicit scope split"})),
        )
        .await;
        assert_eq!(allowed, StatusCode::OK, "scope split clears the trigger and the HTTP gate: {completion}");

        let (permit_status, permit) = call(
            &app,
            "GET",
            "/api/board/overlap/deployment-permit?session=handoff-consumer-0907",
            "handoff-consumer-0907",
            None,
        )
        .await;
        assert_eq!(permit_status, StatusCode::OK);
        assert_eq!(permit["allowed"], true);
    }
}

/// Record the causal runtime marker for work the owning lane explicitly moved
/// into `doing`. `amux board doing` and `amux board claim` are both documented
/// ways to start work; only the latter used to write this marker, leaving the
/// sessions API with a live Doing card and an unrelated older claim.
///
/// The state-entry timestamp makes this idempotent per Doing lifecycle. That
/// matters because `/claim` on an already-owned Doing card is the sanctioned
/// repair path, and repeated repairs must not inflate the repeat-offer ledger.
fn ensure_owner_doing_claim(
    conn: &Connection,
    row: &IssueRow,
    actor: &str,
    source: &str,
    from: &str,
) -> rusqlite::Result<bool> {
    if row.status != "doing" || row.session.as_deref() != Some(actor) {
        return Ok(false);
    }
    let entered = row.entered_state_at.map(|at| at as f64).unwrap_or(0.0);
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_events \
         WHERE session=?1 AND type='task.claimed' AND ts>=?2 \
         AND json_extract(data,'$.issue')=?3)",
        rusqlite::params![actor, entered, row.id],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO session_events (ts,session,type,data,source) VALUES (?,?,?,?,?)",
        rusqlite::params![
            crate::config::now_f64(),
            actor,
            "task.claimed",
            json!({"issue": row.id, "status": "doing", "from": from}).to_string(),
            source,
        ],
    )?;
    tracing::info!(
        target: "amux::board",
        marker = "owner_doing_claim_recorded",
        task = %row.id,
        worker = actor,
        %source,
        %from,
        measured = true,
        n_considered = 1,
        "owner-started Doing work now has an exact runtime/board claim marker"
    );
    Ok(true)
}

async fn repair_owned_doing_claim(
    state: &AppState,
    session: &str,
    card: &str,
) -> anyhow::Result<bool> {
    let repaired = Arc::new(Mutex::new(false));
    let repaired_w = repaired.clone();
    let session = session.to_string();
    let card = card.to_string();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = bs::get_issue(conn, &card)? else {
                return Ok(WriteOutcome { applied: false, events: vec![] });
            };
            let inserted = ensure_owner_doing_claim(
                conn,
                &row,
                &session,
                "board-claim-repair",
                "doing",
            )?;
            *repaired_w.lock().expect("claim repair result slot poisoned") = inserted;
            Ok(WriteOutcome { applied: inserted, events: vec![] })
        })
        .await?;
    let inserted = *repaired.lock().expect("claim repair result slot poisoned");
    debug_assert_eq!(write.applied, inserted);
    Ok(inserted)
}

/// POST /api/board/{id}/claim — atomically take a `todo` or `backlog` card and
/// start it. The CLI and auto-pickup share its compare-and-swap ->doing,
/// assignment, and `task.claimed` event. Backlog remains claimable here while
/// auto-pickup stays todo-only, so a peer handover cannot race a parked card.
pub async fn claim_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Claimer: X-Amux-Worker / X-Amux-Session (canonical), else a body
    // {"session":...} (the bash CLI sends both). A claim with no claimer is
    // meaningless, so refuse rather than record an anonymous owner.
    let (_actor, mut session) = actor_from_headers(&headers);
    if session == "api-anonymous" {
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            if let Some(s) = v.get("session").and_then(Value::as_str) {
                let s = s.trim();
                if !s.is_empty() {
                    session = s.to_string();
                }
            }
        }
    }
    if session == "api-anonymous" || session.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "claim needs a claimer — send X-Amux-Session: <your session> (the `amux board` CLI does this for you)",
                "id": id,
            })),
        )
            .into_response();
    }
    // Read current status + owner first, so every branch reports the truth
    // (claimed / already yours / not claimable / not found) instead of a bare
    // 409 the caller cannot act on.
    let store = state.store.clone();
    let key = id.clone();
    let row = match crate::db::interactions::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(bs::get_issue(&conn, &key)?)
    })
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let Some(row) = row else {
        return not_found(&id);
    };
    let owner = row.session.clone().unwrap_or_default().trim().to_string();
    match row.status.as_str() {
        "todo" | "backlog" => {
            let from: &'static str = if row.status == "todo" { "todo" } else { "backlog" };
            if !owner.is_empty() && owner != session {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        // Name the sanctioned path (AMUX-3450): the old text said
                        // "reassign it first" while no CLI verb could — ethos
                        // rule 6's unwalkable escape, and it forced a raw PATCH.
                        "error": format!("card is assigned to '{owner}', not yours to claim — the owner (or you, deliberately) can hand it over with: amux board assign {id} {session}"),
                        "id": id, "status": from, "session": owner,
                    })),
                )
                    .into_response();
            }
            match crate::runtime_jobs::board_drive::claim_card_from_outcome(
                &state, &session, &id, from,
            )
            .await
            {
                crate::runtime_jobs::board_drive::ClaimCardOutcome::Claimed => (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true, "id": id, "status": "doing", "session": session, "claimed": true,
                    })),
                )
                    .into_response(),
                crate::runtime_jobs::board_drive::ClaimCardOutcome::DependencyBlocked(blocking) => (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "card has unfinished dependencies and cannot be claimed yet",
                        "code": "dependency_blocked",
                        "ok": false,
                        "blocked": true,
                        "id": id,
                        "status": from,
                        "session": owner,
                        "depends_on": row.depends_on,
                        "blocking": blocking,
                        "measured": true,
                        "n_considered": row.depends_on.len(),
                        "how_to_fix": "finish or explicitly resolve the blocking tasks; board-drive promotes a dependency-backed backlog card after every dependency is done or verified",
                    })),
                )
                    .into_response(),
                crate::runtime_jobs::board_drive::ClaimCardOutcome::NotApplied => {
                // Raced out of the status we read between the read above and
                // the swap (owner closed it, or a peer claimed first).
                    (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "error": format!("claim raced — the card left '{from}' between read and write; re-check its status"),
                            "id": id,
                        })),
                    )
                        .into_response()
                }
            }
        }
        "doing" if owner == session => {
            let attribution_repaired = match repair_owned_doing_claim(&state, &session, &id).await {
                Ok(v) => v,
                Err(e) => return internal(e),
            };
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true, "id": id, "status": "doing", "session": session,
                    "already": true, "attribution_repaired": attribution_repaired,
                })),
            )
                .into_response()
        }
        other => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("card is '{other}', not claimable — only a 'todo' or 'backlog' card can be claimed (move it first: amux board todo {id})"),
                "id": id, "status": other, "session": owner,
            })),
        )
            .into_response(),
    }
}

// ---- PATCH /api/board/{id} -----------------------------------------------

/// Keys PATCH writes. Everything else lands in `ignored_fields` (reported,
/// never silently dropped — AC-263).
/// Truncate for a HISTORY LINE, on chars not bytes (a multi-byte title must not
/// panic the writer) and with an ellipsis so a truncated value never reads as
/// the whole value.
/// Stand-in self id for a card that does not exist yet, used only by the
/// create-path acyclicity check (AF-481).
///
/// Printable, because it is logged. See the call site for the 19 NUL bytes that
/// turned a 67 MB log binary and cost every `grep -o` sweep over it 53% of its
/// matches, silently.
const NEW_CARD_SELF_ID: &str = "(new card)";

fn chars_truncate_log(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    format!("{}…", s.chars().take(n).collect::<String>())
}

/// Head+tail with the dropped count NAMED, for a value the log is the only copy
/// of (AF-459, reopened by gtm-engine 2026-09-04 after refusing to validate it).
///
/// `chars_truncate_log` keeps a PREFIX, and a prefix of a destroyed value
/// reproduces the exact failure this log line exists to prevent: gtm-engine lost
/// a 366-character five-item inventory, recovered four items from a prefix, and
/// the fifth was gone. The first fix raised the cap from 60 to 200 and their
/// case still lands past it, so partial recovery from a prefix survived the fix
/// that was written for it. Any FIXED prefix cap has that property for some
/// value; the question is only whose.
///
/// Two changes, and the second is the one that matters. The bound is generous
/// enough that a real trigger is kept WHOLE, and past it the middle goes rather
/// than the tail, with `[N chars elided]` in the gap. A reader then knows they
/// are holding an incomplete value instead of believing a prefix is all there
/// was, which is the difference between a recoverable loss and a silent one.
fn chars_elide_middle(s: &str, head: usize, tail: usize) -> String {
    let n = s.chars().count();
    if n <= head + tail {
        return s.to_string();
    }
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}…[{} chars elided]…{t}", n - head - tail)
}

/// The destroyed `source_ref` is kept whole up to head+tail characters.
///
/// 1800 rather than 200: gtm-engine's real loss was 366 and the previous bound
/// was chosen without one. A bound wants a measurement behind it, and the only
/// measurement available is the largest value anyone has actually lost.
const SOURCE_REF_LOG_HEAD: usize = 900;
const SOURCE_REF_LOG_TAIL: usize = 900;

/// AF-413: which fields did a REFUSAL throw away?
///
/// A PATCH is atomic: when the status transition is refused, the whole body is
/// discarded. That is the defensible choice and this does not change it. What
/// was missing is that the refusal never SAID so — the response is a rich object
/// (`blocked`, `error`, `gate`, `how_to_ack`, `why_blocked`) and every field in
/// it describes the STATUS, so a caller cannot learn from it that an unrelated
/// field went with it.
///
/// Measured three times in one session on 2026-09-02, on three different
/// rejection reasons ("gate not acknowledged", "already holding doing", "done
/// requires evidence of what was run"), one of which silently dropped a 4.2 KB
/// card body that had just been composed. It is only cheap when the caller reads
/// the card back out of habit; a script sending `{desc, status}` in one call
/// loses every desc for every card whose gate is unmet and is told nothing.
///
/// `status` is excluded because the body already names it (`attempted_status`)
/// and reporting it as discarded would bury the surprising fields in the
/// unsurprising one.
///
/// UNKNOWN KEYS ARE NOT DISCARDED. A key outside `PATCH_WRITABLE` would not have
/// been applied by a SUCCESSFUL patch either, so calling it a casualty of the
/// refusal is a wrong answer, not a cautious one — it would send a caller
/// hunting for work the refusal never destroyed. Those are `ignored_fields`, a
/// different claim, reported on the paths that can compute it.
///
/// `desc_append` IS included despite being a control key rather than a writable
/// one: it is the one control key that carries CONTENT, and it is the sanctioned
/// way to add to a card somebody else is also writing. Dropping an append
/// silently is the same loss as dropping a desc.
fn discarded_by_refusal(map: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut out: Vec<String> = map
        .keys()
        .filter(|k| k.as_str() != "status")
        .filter(|k| {
            PATCH_WRITABLE.contains(&k.as_str())
                // `folded_into` joins them: it carries CONTENT (the server
                // writes a log line from it) and a caller who is told nothing
                // changed cannot tell a registered fold from an ignored field,
                // which is the failure this whole thread is about.
                || matches!(k.as_str(), "desc_append" | "callback" | "folded_into" | "archive_outcome")
        })
        .cloned()
        .collect();
    out.sort();
    out
}

const PATCH_WRITABLE: [&str; 33] = [
    "title", "desc", "status", "session", "type", "depends_on", "tags", "reviewer", "shepherd",
    "epic", // AMUX-2992: assign/clear the epic a card rolls up under
    "due", "due_time", "owner_type", "pinned", "pos", "gate", "source_ref", "archived",
    // `amux board <status> --trigger` sends source_ref AND last_verified_at
    // together, but only the first was writable, so the stamp was silently
    // dropped into ignored_fields (reported by mixpeek-frustrations on MF-534).
    // That defeats the guard the flag exists for: the staleness view keys on
    // this field, so a parked card without it sleeps forever with a perfectly
    // good trigger and nothing ever re-checks it — "parking without it buys
    // silence with no expiry", which is the flag's own promise inverted.
    "last_verified_at",
    // AF-321: what was actually run/produced. Writable as its own field so it
    // can be recorded BEFORE the transition that needs it (the two-write shape
    // `--outcome` already uses, so a refused `done` cannot discard the very
    // text the retry needs).
    "evidence",
    // AF-318: the typed ask. Writable on their own for the same reason as
    // `evidence` — a refused `needsyou` must not discard the ask the retry
    // needs, and a lane that writes the ask first can then move the card in a
    // second call that cannot fail on content.
    "ask_type",
    "ask_question",
    "ask_unblocks",
    "ask_actor",
    // The continuation contract (AMUX-3946). Writable on their own for the same
    // reason the ask fields are: a lane can write the continuation first and
    // then move the card in a second call that cannot fail on content, so a
    // refused transition never discards what the author just wrote.
    "next_action",
    "last_result",
    "unresolved",
    // AMUX-3949. A dimension, so it is set and cleared independently of any
    // status move: blocking and unblocking a card must not require pretending
    // it changed position.
    "blocked_on",
    // Workflow engine fields (Phase 2). acceptance_criteria is a JSON array of
    // measurable conditions; decision_* fields structure type=decision cards.
    "acceptance_criteria",
    "decision_question",
    "decision_rationale",
    "decision_supersedes",
    "waiting_on",
];
/// Control keys: consumed by the PATCH protocol itself, never "ignored".
/// `authorized_by` is the cross-lane archive authorizer (AMUX-2492).
/// `desc_append` modifies how `desc` is written rather than naming a column,
/// so it is control, not writable — but it MUST be listed, or it lands in
/// `ignored_fields` and the append silently does nothing (AC-323).
#[cfg(test)]
mod af413_discarded_tests {
    use super::*;
    use serde_json::json;

    fn keys(v: serde_json::Value) -> Vec<String> {
        discarded_by_refusal(v.as_object().unwrap())
    }

    /// THE SPECIMEN. A 4.2 KB desc sent alongside a status that was gated.
    #[test]
    fn a_desc_sent_with_a_refused_status_is_named() {
        assert_eq!(keys(json!({"desc": "4.2 KB of card body", "status": "doing"})), ["desc"]);
    }

    /// The other two rejection reasons hit the same day carried `type` too.
    #[test]
    fn every_writable_field_in_the_body_is_named_sorted() {
        assert_eq!(
            keys(json!({"status": "done", "desc": "x", "type": "code", "title": "t"})),
            ["desc", "title", "type"]
        );
    }

    /// `status` is excluded: the body already names it as `attempted_status`,
    /// and listing it would bury the surprising fields in the unsurprising one.
    #[test]
    fn status_alone_discards_nothing_surprising() {
        assert!(keys(json!({"status": "doing"})).is_empty());
    }

    /// CONTROL, and the one that keeps this honest. An unknown key would not
    /// have been applied by a SUCCESSFUL patch either, so naming it as a
    /// casualty of the refusal would send the caller hunting for work that was
    /// never destroyed. Without this cell, "list every key" would pass every
    /// other cell here.
    #[test]
    fn an_unknown_key_is_not_a_casualty_of_the_refusal() {
        assert!(keys(json!({"status": "doing", "nonsense_field": 1})).is_empty());
        assert_eq!(keys(json!({"status": "doing", "nonsense_field": 1, "desc": "x"})), ["desc"]);
    }

    /// Control keys steer the operation and carry no content, so they are not
    /// losses. The exception is the next cell.
    #[test]
    fn steering_control_keys_are_not_losses() {
        assert!(keys(json!({"status": "doing", "gate_ack": true, "expect_rev": 3,
                            "force": true, "reason": "why"})).is_empty());
    }

    /// A gate refusal must not send the reader after a lever that cannot move.
    ///
    /// AF-586, tubescience: four forced closes in one day on a lane whose
    /// WORKER-scope done gate is ["Implemented and merged", "Tests / lint pass",
    /// "Peer reviewed"], applied to research and ops cards. One refusal site
    /// branched on retype_would_help(); the other hardcoded "fix the type" for
    /// everyone, so the caller retyped, got the identical refusal, and was left
    /// with --force.
    #[test]
    fn a_refusal_only_blames_the_type_when_the_type_is_the_lever() {
        use crate::db::board_store::GateSource;

        // The one case where retyping IS the fix.
        let from_type = retype_hint_for(Some(&GateSource::TypeDefault));
        assert!(from_type.contains("the TYPE is wrong"), "{from_type}");

        // Every scope that IGNORES the type must say so instead, and must not
        // tell the reader to retype.
        for src in [
            GateSource::Worker("tubescience".into()),
            GateSource::Group("amux".into()),
            GateSource::Column,
            GateSource::Card,
        ] {
            let hint = retype_hint_for(Some(&src));
            assert_ne!(hint, from_type, "{src:?} must not get the type-default advice");
            assert!(
                !hint.contains("the TYPE is wrong"),
                "{src:?} names a lever that cannot move: {hint}"
            );
        }

        // The three that are scoped ALSO say retyping will not help, in words.
        for src in [
            GateSource::Worker("tubescience".into()),
            GateSource::Group("amux".into()),
            GateSource::Column,
        ] {
            let hint = retype_hint_for(Some(&src));
            assert!(
                hint.to_lowercase().contains("retyping will not change it"),
                "{src:?} should say retyping will not change it: {hint}"
            );
        }

        // CONTROL. With no resolved source we cannot claim the type is not the
        // lever either, so the default advice stands. Without this, returning
        // the scoped explanation unconditionally would pass everything above.
        assert_eq!(retype_hint_for(None), from_type);
    }

    /// `folded_into` is a control key and must be listed, or a hand-rolled fold
    /// lands in `ignored_fields` and the caller is told nothing changed while
    /// the summary keeps rendering the four not-recorded clauses.
    /// NO RUN OF SPACES IN ANY CALLBACK SENTENCE. Second instance of this
    /// class in three cards, which is why the guard is over the REGION and not
    /// over one literal.
    ///
    /// Rust joins a string across lines with a trailing backslash. Drop it and
    /// the source still compiles, the compiler says nothing, and the sentence
    /// renders with the indentation baked in. AF-621 shipped
    /// "the          condition" into a CLI help surface; AF-634 then shipped
    /// "not a request              and owed you nothing" into the terminal
    /// callback, and I only saw it because a peer's discard notice came back to
    /// me with the gap in it. A test scoped to one function did not generalise,
    /// so this one reads every literal in the block that builds the callback.
    #[test]
    fn no_callback_sentence_renders_a_run_of_spaces() {
        let src = include_str!("board.rs");
        let start = src
            .find("let folded = bs::folded_into_detail(")
            .expect("the callback text block exists");
        let end = src[start..]
            .find("let guard = format!(\"task-callback:")
            .expect("the block ends at the delivery guard")
            + start;
        let block = &src[start..end];

        // APPLY RUST'S OWN CONTINUATION RULE FIRST. A backslash at end of line
        // eats the newline AND the next line's leading whitespace, so the raw
        // source of a CORRECTLY continued literal is full of spaces that never
        // reach the reader. Checking the raw text flags every well-formed
        // multi-line string, which is what the first cut of this test did.
        let strip_continuations = |lit: &str| -> String {
            let mut out = String::new();
            let mut rest = lit;
            while let Some(i) = rest.find("\\\n") {
                out.push_str(&rest[..i]);
                rest = rest[i + 2..].trim_start_matches([' ', '\t']);
            }
            out.push_str(rest);
            out
        };
        let mut offenders: Vec<String> = Vec::new();
        for lit in block.split('"').skip(1).step_by(2) {
            let rendered = strip_continuations(lit);
            if rendered.contains("   ") {
                offenders.push(rendered.chars().take(90).collect());
            }
        }
        assert!(
            offenders.is_empty(),
            "a dropped line-continuation leaves the indentation in the rendered \
             sentence: {offenders:?}"
        );

        // POSITIVE CONTROL: the scrape must actually be reading literals. Without
        // it, a `find` that silently matched nothing gives an empty block and an
        // empty offender list, which is the most reassuring output a dead check
        // can produce.
        let lits = block.split('"').skip(1).step_by(2).count();
        assert!(
            lits >= 5,
            "only {lits} literal(s) scanned; the block moved and this check is blind"
        );
    }

    #[test]
    fn folded_into_is_a_control_key_and_not_a_writable_column() {
        assert!(!PATCH_WRITABLE.contains(&"folded_into"), "it names no column");
        assert!(PATCH_CONTROL.contains(&"folded_into"), "and it must not be ignored");
        assert_eq!(keys(json!({"status": "discarded", "folded_into": "MS-1370"})), ["folded_into"]);
    }

    /// ...except `desc_append`, the one control key that carries CONTENT. It is
    /// the sanctioned way to add to a card someone else is also writing, so
    /// dropping an append silently is the same loss as dropping a desc.
    #[test]
    fn desc_append_is_content_and_is_reported_though_it_is_a_control_key() {
        assert!(!PATCH_WRITABLE.contains(&"desc_append"), "premise: it is NOT a writable key");
        assert!(PATCH_CONTROL.contains(&"desc_append"), "premise: it IS a control key");
        assert_eq!(keys(json!({"status": "done", "desc_append": "a peer note"})), ["desc_append"]);
    }

    /// An empty body discards nothing, and must not panic.
    #[test]
    fn an_empty_body_names_nothing() {
        assert!(keys(json!({})).is_empty());
    }
}

// Archive validation and mutation must interpret the same flag. The API has
// always accepted string spellings as well as JSON booleans/numbers; checking
// only true/1 in the outcome guard rejected requests the mutation accepted.
fn patch_archived_value(value: &Value) -> i64 {
    let raw = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    i64::from(matches!(raw.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

const PATCH_CONTROL: [&str; 13] = [
    // Persisted in the attributed archive log, not a standalone column.
    "archive_outcome",
    // The lane ASSERTS that this card was folded into another. It is not read
    // from prose: the caller names the target and the SERVER writes the
    // canonical `capture folded into <ID>` line that `folded_into()` parses.
    "folded_into",
    "expect_rev",
    "gate_ack",
    "gate_checked",
    "reverify",
    "force",
    "reason",
    "authorized_by",
    "override_doing",
    "desc_append",
    // Must be listed or it lands in `ignored_fields` and the ack silently does
    // nothing — the caller then retries forever against a refusal it believes
    // it answered (the AC-323 shape this array's comment already records).
    "desc_shrink_ack",
    // Structured write spanning the callback_* columns. Kept as one public
    // contract so callers cannot manufacture half-armed callbacks.
    "callback",
];

/// One owner-notice per (owner, card, author, NOTE TEXT) per 10 minutes: a burst
/// of IDENTICAL appends collapses to one turn-boundary message; the notes
/// themselves all land on the card regardless.
///
/// AVE-36 set the window. AMUX-3935 added the note text, because without it the
/// key could not tell a repeat from a follow-up, and a review conversation is
/// always a follow-up: context, then the verdict that rests on it. Two measured
/// instances dropped a blocking review condition and a verification result, both
/// times the later and higher-value message.
fn progress_notify_once(key: &str) -> bool {
    use std::sync::Mutex;
    static SEEN: Mutex<Option<std::collections::HashMap<String, f64>>> = Mutex::new(None);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let Ok(mut g) = SEEN.lock() else { return false };
    let m = g.get_or_insert_with(Default::default);
    if m.get(key).is_some_and(|at| now - at < 600.0) {
        return false;
    }
    m.insert(key.to_string(), now);
    true
}

#[cfg(test)]
mod progress_notify_dedupe_tests {
    use super::progress_notify_once;

    /// AMUX-3935. The dedupe key was (owner, card, author) over a 10-minute
    /// window, which cannot tell "the same note twice" from "a second, DIFFERENT
    /// note about the same card".
    ///
    /// A review conversation is always the second kind — context, then the
    /// verdict that rests on it — so the LATER message is systematically the
    /// higher-value one and it was the one dropped. Two measured instances on
    /// 2026-08-30, both from the same peer, both with that ordering: a review
    /// verdict carrying a blocking condition, and a verification result. The
    /// second suppressed note was their close-out on this very defect.
    ///
    /// Both directions asserted. Without the collapse arm this is
    /// indistinguishable from deleting the dedupe, which would restore the
    /// notification flood AVE-36 added the window for.
    #[test]
    fn a_different_note_delivers_and_an_identical_one_still_collapses() {
        let k = |note: &str| format!("owner|CARD-1|peer|{note}");

        assert!(progress_notify_once(&k("context: here is what I measured")),
                "the first note delivers");
        assert!(
            progress_notify_once(&k("verdict: ACCEPT with one blocking condition")),
            "a DIFFERENT note from the same author on the same card must deliver — this is \
             the verdict that was dropped twice"
        );
        // CONTROL: the flood protection still works. A burst of identical
        // appends is what the window exists for.
        assert!(
            !progress_notify_once(&k("context: here is what I measured")),
            "an IDENTICAL repeat inside the window must still collapse, or the fix has \
             removed flood protection rather than narrowing it"
        );
        // CONTROL: the key still separates cards and owners, so the content hash
        // has not swallowed the rest of the key.
        assert!(progress_notify_once("owner|CARD-2|peer|context: here is what I measured"),
                "the same text on a DIFFERENT card is a different notice");
        assert!(progress_notify_once("other|CARD-1|peer|context: here is what I measured"),
                "the same text to a DIFFERENT owner is a different notice");
    }
}

enum PatchOut {
    NotFound,
    /// Any pre-write refusal (400/409) with its exact body.
    Refused(StatusCode, Value),
    /// Invariant 37: nothing changed; `rev` unmoved.
    ///
    /// `all_ignored` = the body carried keys and EVERY one was unwritable, so the
    /// request could not have done anything. That is a caller error and answers
    /// 422; an ordinary no-op (a writable field set to its current value) is a
    /// successful request that changed nothing and stays 200. See the response
    /// arm for why the distinction has to be this narrow.
    Noop { body: Value, ignored: Vec<String>, all_ignored: bool },
    Applied {
        body: Value,
        ignored: Vec<String>,
        /// Fields whose value was APPLIED, but not to the field the caller
        /// named (AMUX-3791). Distinct from `ignored` on purpose: ignored
        /// means "this did not happen", diverted means "this happened
        /// somewhere else". Reporting a diversion as ignored would be a lie in
        /// the more damaging direction — a caller told their trigger was
        /// dropped goes and sets it again, or files a bug against working
        /// code, which is exactly what this card was.
        diverted: Vec<Value>,
        /// Advisories are NOT diversions (AF-469 regression, 2026-09-04). A
        /// diversion says "the key you named is not the key that changed"; an
        /// advisory says "the write landed, and here is a companion field you
        /// did not send". Merging them tripped a control written to stop an
        /// advisory firing on every source_ref write.
        advisories: Vec<Value>,
        /// (session, from_status, to_status) when a status change happened,
        /// for reactive pickup: if the transition freed the lane (done/verified/
        /// discarded), fire an immediate pickup instead of waiting 60s.
        status_transition: Option<(String, String, String)>,
        /// (owner_session, title, note) when a NON-owner appended a progress
        /// note to someone else's card (AVE-36). `amux board progress`
        /// reported success while notifying nobody, and `ask` notified — with
        /// nothing at the call site distinguishing them, a worker reporting a
        /// RESULT reached for progress and the owner missed three confirms in
        /// a row on a card they were actively working. The write that already
        /// happens gains its consequence: a named consumer (the owner), at
        /// the next turn boundary, deduped.
        progress_notify: Option<(String, String, String)>,
        /// The REVIEWER a note in `review` is actually for (AMUX-3771). Separate
        /// from `progress_notify` because owner and reviewer are different roles
        /// and the workaround this replaces was to conflate them.
        ///
        /// Boxed: adding a third String triple pushed this variant past clippy's
        /// `large_enum_variant` threshold, and `NotFound` is a unit variant that
        /// would have paid for it on every return.
        reviewer_notify: Option<Box<(String, String, String)>>,
    },
}

/// Map a (from, to) pair onto the core transition vocabulary. `None` means
/// no named transition exists — the caller falls back to the gate-checked
/// generic move (the Python board allows any->any, so refusing unmapped
/// pairs outright would break live CLI flows like todo->done).
fn named_transition(
    from: TaskStatus,
    to: TaskStatus,
    evidence: Vec<Evidence>,
    reason: String,
) -> Option<BoardTransition> {
    use TaskStatus as S;
    Some(match (from, to) {
        (S::Backlog, S::Todo) => BoardTransition::Queue,
        (S::Todo, S::Backlog) => BoardTransition::Park,
        (S::Todo, S::Doing) => BoardTransition::Start,
        (S::Doing, S::Todo) => BoardTransition::Release,
        (S::Doing, S::Review) => BoardTransition::Submit,
        (S::Review, S::Done) => BoardTransition::Approve { evidence },
        (S::Review, S::Doing) => BoardTransition::Reject { reason },
        (S::Doing, S::Done) => BoardTransition::Complete { evidence },
        (S::Done, S::Verified) => BoardTransition::Verify {
            criteria: vec![],
            evidence,
        },
        (S::Done, S::Doing) => BoardTransition::VerificationFailed { reason },
        (S::Doing, S::NeedsYou) => BoardTransition::RequestInput { question: reason },
        (S::NeedsYou, S::Doing) => BoardTransition::Resume,
        (S::Todo | S::Doing, S::Blocked) => BoardTransition::Block { reason },
        (S::Blocked, S::Todo) => BoardTransition::Unblock,
        (S::Todo | S::Backlog, S::Armed) => BoardTransition::Arm,
        (S::Armed, S::Todo) => BoardTransition::Fire { reason },
        (_, S::Discarded) => BoardTransition::Discard { reason },
        (_, S::Quarantined) => BoardTransition::Quarantine { reason },
        _ => return None,
    })
}

/// Ack evidence: one `ModelTranscript` artifact per criterion, provenance
/// `SelfReported` (an ack IS self-reported — never inflate it to
/// Independent). This is what `satisfied_by` matches against the
/// `ModelJudgment` verifiers in `bs::core_gates`.
fn ack_evidence(actor: &str, criteria: &[String], via: &str) -> Vec<Evidence> {
    let now = chrono::Utc::now();
    criteria
        .iter()
        .map(|c| Evidence {
            kind: EvidenceKind::ModelTranscript,
            description: format!("acknowledged by {actor} via {via}: {c}"),
            artifact: None,
            produced_at: now,
            source: EvidenceSource::SelfReported,
        })
        .collect()
}

/// The Python-compatible gate 409 (the CLI parses `error`, `gate`,
/// `item_type`, `attempted_status`, `valid_types` — grep amux-server.py
/// "gate not acknowledged"). Core's serialized refusal rides along under
/// `why_blocked`/`kind`: it cannot be merged flat because core spells the
/// list `blocked` while the Python contract's `blocked` is the boolean the
/// CLI-side incident (orch MO-2952) made load-bearing.
/// Normalize a gate criterion for ACK MATCHING (AF-160 / AMUX-3532).
///
/// Acknowledgement was exact string containment, and one criterion in the
/// `amux` group's `verified` gate reads:
///
/// ```text
/// Peer-reviewed by a DIFFERENT worker in group `amux` (name them)
/// ```
///
/// The parenthetical is an INSTRUCTION to the acking agent. Under exact
/// matching the only ack that passes is the criterion verbatim, "(name them)"
/// included — so following the instruction inside the criterion is what makes
/// the ack fail. That is ethos rule 3 exactly, and its practical effect is to
/// route the criterion carrying the most judgment in the gate toward the two
/// mechanisms carrying the least: `gate_ack` (acknowledge everything at once,
/// which per-criterion acks exist to prevent) and `force`.
///
/// Two more traps rode along, both of which cost a retry on AF-66: DIFFERENT is
/// uppercase in the criterion and lowercase in ordinary prose, and `amux` is in
/// BACKTICKS, so a shell ate them unless escaped and the sent string silently
/// differed from the one the caller believed they sent.
///
/// So: case-fold, drop backticks, drop ONE trailing parenthetical, collapse
/// whitespace. Exact matching is still tried FIRST at the call site, so nothing
/// that passes today can stop passing; this only widens.
///
/// It cannot make two distinct criteria collide unless they differ ONLY by a
/// trailing parenthetical or by case, in which case they were already
/// indistinguishable to a human reading the 409.
fn ack_norm(s: &str) -> String {
    let mut t = s.trim().to_lowercase().replace('`', "");
    if t.ends_with(')') {
        if let Some(i) = t.rfind('(') {
            t.truncate(i);
        }
    }
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Does this criterion ASK FOR A NAME? (AF-160)
///
/// The marker is the criterion's own words. A gate that says "name them" and
/// then records no name is not collecting the fact it exists to collect —
/// measured fleet-wide 2026-08-23: 148 of 1632 live verified cards named a
/// peer, and 45 of 1381 archived ones. 91% of the board passed this gate with
/// nothing machine-readable behind it.
fn criterion_wants_a_name(c: &str) -> bool {
    c.to_lowercase().contains("name them")
}

/// The exit for a card whose WORK belongs to another lane (AF-506).
///
/// Reported by `backend`, hit live on MI-4155 during autonomous backlog triage.
/// A lane holding a card that is not its work has no honest state to move it to:
/// `backlog` re-feeds that same lane's auto-pickup, `todo` re-queues after a
/// cooldown, `needsyou` reads as blocked on Ethan rather than on a peer, and
/// `review` — which the DISPATCHER's own card text recommends — gates on acking
/// "Implemented and self-tested" / "Diff / PR is up", which a card you are
/// ROUTING AWAY cannot truthfully claim. Every move is a lie or a loop, which is
/// ethos rule 3: a legitimate state with no truthful path forward.
///
/// The gate is RIGHT to refuse; what was missing is that the refusal knew only
/// one way out. Reassignment is not a bypass and is deliberately kept out of the
/// `or_force` family: it does not skip the gate, it moves the card to whoever
/// the gate is asking about, and that lane satisfies it honestly.
///
/// Two wordings because two different things are true. When the caller is not
/// the owner, the owner can be NAMED. When the caller IS the owner — backend's
/// case, since the pickup had already assigned it to them — nothing here can
/// tell whether the work belongs elsewhere, so it is stated as a conditional
/// and the loop is spelled out. Neither wording asserts the card is misassigned
/// (AF-169: a hint that cannot apply must not print as though it does).
fn reassign_exit(card: &str, owner: Option<&str>, caller: &str) -> Value {
    let owner = owner.map(str::trim).filter(|o| !o.is_empty());
    let mine = owner.is_none_or(|o| o == caller);
    if let (false, Some(o)) = (mine, owner) {
        return json!({
            "when": format!(
                "This card is owned by {o:?}, not by you. If the work is theirs, you do not need to satisfy this gate at all — hand it back."
            ),
            "how": format!("amux board assign {card} {o} && amux board todo {card}"),
            "effect": format!("dispatches to {o}, not to you"),
            "not_a_bypass": "this does not skip the gate; it moves the card to the lane the gate is asking about, and they satisfy it honestly",
        });
    }
    json!({
        "when": "If this card's WORK belongs to another lane, hand it over instead of acking a criterion you cannot truthfully claim. You own it right now, so nothing here can tell whether that is the case — only you can.",
        "how": format!("amux board assign {card} <owning-lane> && amux board todo {card}"),
        "effect": "dispatches to THEM, not back to you. Moving it to `backlog` or `todo` while you still own it re-feeds your own auto-pickup and it returns.",
        "not_a_bypass": "this does not skip the gate; it moves the card to the lane the gate is asking about, and they satisfy it honestly",
    })
}

#[cfg(test)]
mod reassign_exit_tests {
    use super::*;

    /// A card owned by someone else NAMES them, so the reader can evaluate the
    /// advice without another call. This is the case the caller can act on
    /// immediately.
    #[test]
    fn a_card_owned_by_a_peer_names_that_peer_in_the_command() {
        let v = reassign_exit("MI-4155", Some("mvs-infra"), "backend");
        assert!(v["when"].as_str().unwrap().contains("\"mvs-infra\""), "{v:#}");
        assert!(
            v["how"].as_str().unwrap() == "amux board assign MI-4155 mvs-infra && amux board todo MI-4155",
            "{v:#}"
        );
        assert!(v["effect"].as_str().unwrap().contains("dispatches to mvs-infra"), "{v:#}");
    }

    /// THE REPORTED CASE. The pickup had already assigned MI-4155 to backend, so
    /// they OWNED the card they needed to hand away. Nothing here can tell
    /// whether the work belongs elsewhere, so it must be a conditional the
    /// reader resolves — and it must name the loop, which is the part that cost
    /// them two round trips.
    #[test]
    fn a_card_you_already_own_states_the_condition_and_names_the_loop() {
        let v = reassign_exit("MI-4155", Some("backend"), "backend");
        assert!(v["when"].as_str().unwrap().starts_with("If this card's WORK belongs"), "{v:#}");
        assert!(v["when"].as_str().unwrap().contains("only you can"), "{v:#}");
        assert!(
            v["effect"].as_str().unwrap().contains("re-feeds your own auto-pickup"),
            "the loop that sent the card back twice is not named: {v:#}"
        );
        assert!(v["how"].as_str().unwrap().contains("<owning-lane>"), "{v:#}");
    }

    /// An unowned card behaves like one you own: amux cannot name a peer, so it
    /// must not pretend to. An empty or whitespace owner is the same state as
    /// none, not a lane called "".
    #[test]
    fn an_unowned_card_never_invents_an_owner() {
        for owner in [None, Some(""), Some("   ")] {
            let v = reassign_exit("AF-1", owner, "amux-frustrations");
            assert!(
                v["how"].as_str().unwrap().contains("<owning-lane>"),
                "owner {owner:?} produced a named command: {v:#}"
            );
        }
    }

    /// No run of spaces reaches the reader. Every string here is a Rust literal
    /// spanning several source lines, and the difference between a continuation
    /// that joins them and one that does not is invisible in the source and
    /// obvious in the output — this shipped once, and it was a live probe that
    /// showed "the                              gate is asking about".
    #[test]
    fn no_arm_leaks_source_indentation_into_the_text() {
        for (owner, caller) in [(Some("peer"), "me"), (Some("me"), "me"), (None, "me")] {
            let v = reassign_exit("AF-1", owner, caller);
            for key in ["when", "how", "effect", "not_a_bypass"] {
                let s = v[key].as_str().unwrap_or_default();
                assert!(
                    !s.contains("  "),
                    "{owner:?}/{key} carries source indentation into the reader's text: {s:?}"
                );
            }
        }
    }

    /// It is NOT a bypass, and every arm says so. The `force` family skips the
    /// gate; this moves the card to the lane the gate is asking about, and that
    /// distinction is the whole reason this can be offered beside a refusal
    /// without weakening it.
    #[test]
    fn every_arm_says_it_is_not_a_bypass() {
        for (owner, caller) in [(Some("peer"), "me"), (Some("me"), "me"), (None, "me")] {
            let v = reassign_exit("AF-1", owner, caller);
            let s = v["not_a_bypass"].as_str().unwrap_or("");
            assert!(s.contains("does not skip the gate"), "{owner:?}: {v:#}");
        }
    }
}

fn gate_409(
    row: &IssueRow,
    eff_gate: &[String],
    target_raw: &str,
    wb: &[amux_core::board::WhyBlocked],
    gate_source: Option<&bs::GateSource>,
    caller_lane: &str,
) -> Value {
    let checked_args = eff_gate
        .iter()
        .map(|g| format!("{:?}", g))
        .collect::<Vec<_>>()
        .join(" ");
    // A HINT THAT CANNOT APPLY MUST NOT PRINT (AF-169).
    //
    // This body always said "set its type — the gate is DERIVED from the type",
    // which is true only when the TYPE DEFAULT is what refused. For a card in a
    // worker- or group-scoped gate, retyping changes nothing: the operator
    // retypes, re-runs, and gets the identical refusal. AF-168's reporter did
    // exactly that on TUBES-2053 (code -> research), watched the done gate not
    // move, and concluded the gate was pinned per-card. The hint is what sent
    // them there. It cost amux-frustrations a retry too, on a card that was not
    // mistyped.
    //
    // `gate_source` comes from the resolver's own walk, so the advice and the
    // enforcement cannot disagree — the same predicate, not a second reading of
    // it. Where retyping DOES help the hint still prints; where it does not,
    // the body names the scope the bar actually came from and points at
    // /api/board/session-gates, which answers "does this scope have a custom
    // gate" in one call and which nothing surfaces until you trip it.
    let mut how_to_ack = serde_json::Map::new();
    how_to_ack.insert("gate_ack".into(), json!(true));
    how_to_ack.insert("or_gate_checked".into(), json!(eff_gate));
    how_to_ack.insert(
        "contract".into(),
        json!(format!(
            "GET /api/board/contract?card={} (the RESOLVED gate for this card — the bare \
             contract lists only type defaults, AF-112)",
            row.id
        )),
    );
    match gate_source {
        Some(src) if !src.retype_would_help() => {
            how_to_ack.insert("gate_source".into(), json!(src.explain()));
        }
        _ => {
            how_to_ack.insert(
                "wrong_type?".into(),
                json!("If this item has no code, set its type \
                       (escalation/blocker/investigation/ops/research/chore/doc) — the gate \
                       is DERIVED from the type. Never ack a merge that did not happen."),
            );
        }
    }
    let how_to_ack = Value::Object(how_to_ack);
    json!({
        "error": "gate not acknowledged",
        "ok": false,
        "blocked": true,
        "gate": eff_gate,
        "attempted_status": target_raw,
        "item": row.id,
        "item_type": row.item_type,
        "how_to_ack": how_to_ack,
        // AF-506. The refusal used to teach exactly one exit — satisfy the gate —
        // so a lane holding someone else's card had to rediscover reassignment or
        // pick a dead end. Same mechanism as `how_to_ack`: the response already
        // knows how to teach a CLI invocation.
        "or_reassign": reassign_exit(&row.id, row.session.as_deref(), caller_lane),
        "cli": format!("amux board {target_raw} {} --checked {checked_args}", row.id),
        "valid_types": bs::KNOWN_TYPES,
        "kind": "gate_blocked",
        "why_blocked": wb,
    })
}

/// `POST /api/board/clear-done` — the dashboard's "Clear done" button.
///
/// It was never routed on rust (AMUX-2630): the SPA optimistically hid the done
/// cards, POSTed, got a 405 from the GET-only catch-all, and the cards came
/// back on the next refresh. A button that appears to work and silently does
/// nothing is worse than a missing one.
///
/// ARCHIVES, never deletes. The cards are the user's record of what happened,
/// and "clear from my board" is a view operation — `archived=1` removes them
/// from every default view while leaving them recoverable. Deleting user
/// content as the side effect of a tidy-up button is the ethos-rule-8 failure.
pub async fn clear_done(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (_, actor) = actor_from_headers(&headers);
    // HOW MANY, not merely "ok" (ethos rule 4). A bare success is
    // indistinguishable from the dead button this card is about: both leave
    // the caller with no way to tell "archived 957" from "matched nothing".
    // The count is the UPDATE's own rowcount, so it cannot drift from what
    // the write did — and the SPA renders it instead of a silent hide.
    let slot: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(
                "UPDATE issues SET archived = 1, updated = strftime('%s','now') \
                 WHERE status = 'done' AND COALESCE(archived,0) = 0 AND deleted IS NULL",
                [],
            )?;
            *slot_w.lock().unwrap() = Some(n as i64);
            Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await;
    match res {
        Ok(_) => {
            let n = slot.lock().unwrap().unwrap_or(0);
            tracing::info!(actor = %actor, archived = n, "board: cleared done cards (archived)");
            // `action` travels WITH the count because the count alone loses the
            // load-bearing fact: these cards still exist. A client that reads
            // only "archived: 957" must not have to guess whether 957 rows were
            // destroyed.
            (
                StatusCode::OK,
                Json(json!({"ok": true, "archived": n, "action": "archived"})),
            )
                .into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": format!("clear-done failed: {e}")}),
        ),
    }
}

/// Does replacing `old` with `new` DESTROY prose that belongs to another lane?
/// (AMUX-3576, floors removed under AF-191.)
///
/// Extracted so the write site and its test share ONE definition. The test used
/// to restate the predicate with a comment asking the next editor to keep them
/// in step by hand, which is a paraphrase of the shipped code rather than the
/// shipped code — the exact shape ethos rule 7 warns about, and the reason the
/// two floors below could be wrong for weeks with a green test beside them.
///
/// SURVIVAL, NOT SIZE. The rule declares itself to be about destruction and used
/// to gate on `before >= 200` and a 200-character NET LOSS. Both of the acts it
/// names walked straight through:
///
///   amux-cloud, reproduced on scratch AC-391 (2026-08-24): a peer PATCH
///   replaced their 54-char desc with 17 chars. 54 < 200, no rule fired,
///   `applied: true`.
///
///   amux-frustrations, reproduced on scratch AF-194 the same day: a peer PATCH
///   replaced a 264-char desc with 392 chars of unrelated text. The net loss was
///   ZERO because it GREW, no rule fired, `applied: true`, every character gone.
///
/// A length delta cannot express content destruction: a longer replacement
/// destroys exactly as much as a shorter one. So the question is whether any of
/// the owner's LINES survive verbatim. Nothing to tune, and it separates the two
/// cases the floors were reaching for — a peer fixing a typo in one line of a
/// multi-line write-up leaves the rest intact and passes, while a wholesale
/// replace takes every line with it at any magnitude, in either direction.
///
/// THE COST, stated rather than discovered later: for a SINGLE-LINE desc there
/// is no other line to survive, so any cross-lane replace of it is refused —
/// including a genuine typo fix. That is amux-cloud's incident exactly, the
/// escape is one field (`desc_shrink_ack`) that the refusal prints, and
/// `desc_append` is there to add instead. A peer silently destroying a
/// one-sentence card is the worse trade.
/// Every field the slim list payload omits, in ONE definition shared by the
/// writer and its test (AF-200).
///
/// The test used to restate this as its own literal beside the code, which is a
/// paraphrase rather than the shipped list — the shape ethos rule 7 warns about,
/// and how `reviewer` could be dropped for weeks with a green test next to it
/// (AF-161). A seventh omission now cannot be added without this array, and the
/// array is what callers are told.
pub(crate) const SLIM_OMITS: [&str; 6] = [
    "desc",
    "due_time",
    "gate",
    "last_verified_at",
    "log",
    "source_ref",
];

pub(crate) fn desc_replace_destroys_peer_prose(
    owner: &str,
    writer: &str,
    old: &str,
    new: &str,
) -> bool {
    let owner = owner.trim();
    let writer = writer.trim();
    if owner.is_empty() || writer.is_empty() || owner == writer {
        return false;
    }
    let old_trimmed = old.trim();
    if old_trimmed.is_empty() || new.contains(old_trimmed) {
        return false;
    }
    let mut lines = old.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    if lines.peek().is_none() {
        return false;
    }
    // No floor. A re-added `&& old.len() - new.len() >= 200` is the exact defect
    // AMUX-3576 was filed about, and it came back through the shared index on
    // 2026-08-24 (AF-182): this line was floorless in ac7b9e33 and had the floor
    // again by c971756b, whose own message said both floors were gone. If you are
    // about to add a threshold here, read `the_desc_shrink_refusal_shows_how_to_
    // append_not_just_the_field_name` first — the LONGER-replacement case has a
    // net loss of zero and is the one a floor cannot see.
    !lines.any(|l| new.contains(l))
}

/// The "is the TYPE the lever?" hint for a gate refusal, in ONE place.
///
/// Retyping only changes the gate when the gate CAME from the type. A worker-,
/// group-, column- or card-scoped gate ignores the type entirely, so telling
/// that caller to fix the type names a lever that cannot move: they retype, get
/// the identical refusal, and are left with --force.
///
/// It lives here because two refusal sites answered this question differently.
/// One branched on `GateSource::retype_would_help()`; the other hardcoded "the
/// TYPE is wrong, fix the type" for every caller. tubescience hit the second on
/// 2026-09-08, four forced closes in one day on a lane whose WORKER-scope done
/// gate is ["Implemented and merged", "Tests / lint pass", "Peer reviewed"],
/// applied to research and ops cards (AF-586). Two copies of one rule is how
/// they came apart, so there is one copy now.
///
/// Whether those criteria SHOULD apply to a research card is a separate and
/// still-open decision. This only stops a refusal sending the reader after a fix
/// that cannot work.
pub(crate) fn retype_hint_for(gate_source: Option<&bs::GateSource>) -> String {
    match gate_source {
        Some(src) if !src.retype_would_help() => src.explain(),
        _ => "If these criteria don't fit the work, the TYPE is wrong, so fix the type \
              rather than the truth."
            .to_string(),
    }
}

pub async fn patch_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(map) = body.as_object().cloned() else {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "body must be a JSON object" }),
        );
    };
    if let Some(scope) = super::org::local_member_scope(&headers).filter(|scope| !scope.is_global()) {
        if map.contains_key("session") {
            let target = body_str(&map, "session").unwrap_or_default();
            if target.is_empty() || !scope.allows_worker(&target) {
                return scoped_board_forbidden(
                    &scope,
                    if target.is_empty() { "unassigned card" } else { &target },
                );
            }
        }
    }
    // AF-413: computed HERE, before `map` moves into the write closure, because
    // the refusal that needs it is built inside that closure and answered after
    // it. Cheap (a key scan) and unconditional: a value only read on the refusal
    // path is not worth a branch, and making it conditional would put the field
    // behind the same reasoning that left it missing in the first place.
    let discarded_on_refusal = discarded_by_refusal(&map);
    let (actor, actor_name) = actor_from_headers(&headers);
    // ATTRIBUTION IS REQUIRED FOR FORCE (ts-gke 2026-08-03; Python parity
    // amux-server.py ~70111). Fires on `force` ITSELF, never `eff_gate &&
    // force`: the incident specimen was a watch card whose todo->discarded
    // had NO gate, so a gate-conditioned check cannot fail on the case that
    // motivated it.
    if map.get("force").and_then(Value::as_bool).unwrap_or(false) && actor_name == "api-anonymous" {
        return err(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "force requires attribution",
                "why": "force bypasses the checks; the judgment then rests with whoever forced it, so the ledger must name them. An unattributed force is an audit row that records only that something happened.",
                "how": "send X-Amux-Session: <your session> (the `amux board` CLI does this for you). Or satisfy the gate honestly — if it does not fit the work, the TYPE is wrong; fix the type, not the truth.",
            }),
        );
    }
    // A REASON IS REQUIRED FOR FORCE, for the same reason attribution is
    // (AMUX-3464, 2026-08-26). The check above fixed WHO and stopped there,
    // and the half it left undone was 100% broken in production: of the 41
    // force audit lines this board has ever written, 41 read `reason=` with
    // nothing after the `=`. Not "mostly empty" — never once populated. The
    // format string advertises the judgment and the field behind it was
    // decoration, which is ethos rule 6 exactly: a bypass that claims to be
    // audited and records only that something happened.
    //
    // It was not operators withholding it, either. `amux board --force` has
    // always REFUSED to run without a reason, then sent it as `desc_append`
    // instead of `reason` — 9 of those 41 cards carry a good "[FORCED] <why>"
    // in their desc while their ledger line says nothing. So the sanctioned
    // path collected the answer and dropped it, and the server accepted the
    // blank without a murmur. Fixing only one end leaves the class alive:
    // the CLI now sends `reason`, and this makes a blank one impossible to
    // write from ANY caller, including the raw PATCHes that produced 25 of
    // the 41 (the AMUX-2325 shape — a hand-rolled curl off the audited path).
    //
    // Fires on `force` ITSELF, not on `eff_gate && force`, for the reason the
    // block above records: the specimens include gateless todo->discarded
    // moves, so a gate-conditioned check cannot fail on them.
    if map.get("force").and_then(Value::as_bool).unwrap_or(false)
        && body_str(&map, "reason").is_none_or(|r| r.trim().is_empty())
    {
        // Named marker, because the refusal alone is not self-announcing: a
        // 400 here groups with every other board-PATCH 400 in
        // /api/logs/analyze, so "who is still forcing blind" would be
        // invisible in the one place people already look. `id` names the card
        // and `actor` names the lane, which is what routes the fix.
        tracing::warn!(
            marker = "force_without_reason",
            card = %id,
            actor = %actor_name,
            "force refused: no reason supplied — caller is off the audited path (AMUX-3464)"
        );
        return err(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "force requires a reason",
                "why": "force bypasses the checks, so the card's log line IS the audit. `reason=` with nothing after it names an actor and records no judgment, which is indistinguishable from no audit at all — every force ever written to this board before AMUX-3464 was that shape.",
                "how": "amux board <status> <ID> --force \"<why you are bypassing the gate>\" — the CLI sends both the ledger `reason` and a [FORCED] note on the card. Raw PATCH: add \"reason\": \"<why>\". Or satisfy the gate honestly — if it does not fit the work, the TYPE is wrong; fix the type, not the truth.",
            }),
        );
    }
    let force_actor = actor_name.clone();
    // AF-701: a VERIFIED local member (Ethan, from the dashboard) is always
    // allowed and stays exempt from the cross-lane archive guard below —
    // captured separately from `caller_lane` so the exemption survives while
    // the next comment's bug does not.
    let is_local_member = super::org::is_verified_local_member(&headers);
    // Python `_hdr_worker`: "" when the header is absent — the cross-lane
    // archive guard used to ALSO exempt a truly anonymous caller this way
    // (AMUX-2492's original text: "only fires for a NAMED caller"), which
    // reads as a design choice and was actually the hole: an unnamed caller
    // is the one that most needs the guard, not the one that should skip it.
    // Measured by mixpeek-orchestrator 2026-09-10/11: a 62-card anonymous
    // PATCH sweep (zero X-Amux-Session/Worker header, 14-15 byte bodies,
    // one card per request over 6 minutes) archived cards across at least
    // seven OTHER lanes' namespaces with no resistance, because `caller_lane`
    // was empty and the guard below only checked `!caller_lane.is_empty()`.
    // Fixed at the guard site (AF-701), not here: `caller_lane` still needs
    // to read "" for every other caller (notify targets, log lines) that
    // legitimately treats an anonymous/local-member caller as unnamed.
    let caller_lane = if actor_name == "api-anonymous" || is_local_member {
        String::new()
    } else {
        actor_name.clone()
    };

    let slot: Arc<Mutex<Option<PatchOut>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let caller_for_notify = caller_lane.clone();

    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = bs::get_issue(conn, &id_w)? else {
                return finish(&slot_w, PatchOut::NotFound, no_write());
            };

            // Optimistic concurrency: expect_rev checks the PYTHON counter.
            // Conflict outranks everything — a stale caller must learn their
            // view is old before any other verdict.
            if let Some(exp) = map.get("expect_rev").and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            }) {
                if exp != row.rev {
                    return finish(
                        &slot_w,
                        PatchOut::Refused(
                            StatusCode::CONFLICT,
                            json!({
                                "error": "rev conflict",
                                "current_rev": row.rev,
                                "expected": exp,
                                "item": detail_body(&row),
                                "hint": "re-read, re-apply your change to the current item, retry with the new rev",
                            }),
                        ),
                        no_write(),
                    );
                }
            }

            let mut ignored: Vec<String> = map
                .keys()
                .filter(|k| {
                    !PATCH_WRITABLE.contains(&k.as_str()) && !PATCH_CONTROL.contains(&k.as_str())
                })
                .cloned()
                .collect();
            if !ignored.is_empty() {
                tracing::warn!(target: "amux::board", verdict="patch_fields_ignored", item=%id_w,
                    fields=?ignored, measured=true, n_considered=ignored.len(), "board PATCH contains ignored fields");
            }
            if let Some(outcome) = map.get("archive_outcome") {
                let archiving = map.get("archived").is_some_and(|v| patch_archived_value(v) == 1);
                let reason = outcome.as_str().filter(|s| !s.trim().is_empty());
                if !archiving || reason.is_none() {
                    return finish(&slot_w, PatchOut::Refused(StatusCode::BAD_REQUEST,
                        json!({"error":"archive_outcome requires archived=true and a nonempty string", "item":row.id})), no_write());
                }
                if row.archived == 1 {
                    return finish(&slot_w, PatchOut::Refused(StatusCode::CONFLICT,
                        json!({"error":"card is already archived; unarchive before recording another archive_outcome", "item":row.id})), no_write());
                }
            }
            // Filled by the source_ref arm below when a trigger is rerouted to
            // the card body. Empty on every other write.
            let mut diverted: Vec<Value> = Vec::new();
            // Separate from `diverted` on purpose — see the advisory push below.
            let mut advisories: Vec<Value> = Vec::new();
            // EVERY key unwritable = the request was unusable (AEAB/#134 review,
            // reported by tsukimiya). Narrow on purpose: a MIXED body such as
            // {"status":"done","item_type":"code"} where the card is already
            // `done` is also a no-op with something ignored, and it must NOT
            // 422 — the caller's `status` key was legitimate and the response
            // already names the typo. Only "nothing you sent was writable"
            // is unambiguously the caller's mistake.
            let all_ignored = !map.is_empty() && ignored.len() == map.len();

            // ---- stage non-status field changes onto a working copy ------
            // (staged BEFORE the gate check so a PATCH changing type and
            // status together gates on the NEW type — the Python handler's
            // own rule.)
            let mut next = row.clone();
            let mut changed: Vec<String> = Vec::new();
            let mut tags_change: Option<Vec<String>> = None;

            // A worker cannot create on its own board and then move the card
            // onto a peer's board through PATCH. Administrative callers remain
            // able to reassign, while a verified worker may only name itself as
            // the destination. Peer collaboration belongs in reviewer,
            // shepherd, and depends_on links instead of the ownership field.
            if !caller_lane.is_empty() && map.contains_key("session") {
                let requested_owner = body_opt_str(&map, "session")
                    .flatten()
                    .filter(|owner| !owner.trim().is_empty());
                if requested_owner.as_deref() != Some(caller_lane.as_str()) {
                    tracing::warn!(
                        caller = %caller_lane,
                        card = %id_w,
                        requested_owner = ?requested_owner,
                        "cross-board card reassignment refused"
                    );
                    return finish(
                        &slot_w,
                        PatchOut::Refused(
                            StatusCode::FORBIDDEN,
                            json!({
                                "error": "workers may assign board items only to their own board",
                                "code": "cross_board_reassignment_forbidden",
                                "caller": caller_lane,
                                "requested_owner": requested_owner,
                                "how_to_fix": "keep the card on your own board and link the peer with reviewer, shepherd, or depends_on",
                            }),
                        ),
                        no_write(),
                    );
                }
            }

            if let Some(t) = body_str(&map, "title") {
                if t != next.title {
                    next.title = t;
                    changed.push("title".into());
                    // AF-703: a title correction lands on the card you edited
                    // and never on the cards that CITE it — gtm-engine measured
                    // three same-evening instances (a cost overstated in one
                    // title, a count understated in another, a third card's
                    // `blocked_on` quoting the first FIGURE an hour after it was
                    // corrected). Surfacing every citing card at edit time is
                    // the cheapest fix named: it does not rewrite anyone else's
                    // text (ethos rule 8 — whose data is this), it lets the
                    // author of the change see what they just orphaned.
                    if let Ok(mut stmt) = conn.prepare(
                        "SELECT id, title FROM issues WHERE deleted IS NULL AND id != ?1 \
                         AND (title LIKE '%'||?1||'%' OR \"desc\" LIKE '%'||?1||'%' \
                         OR COALESCE(blocked_on,'') LIKE '%'||?1||'%') LIMIT 20",
                    ) {
                        if let Ok(rows) = stmt.query_map(rusqlite::params![&next.id], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                        }) {
                            let citing: Vec<Value> = rows
                                .flatten()
                                .map(|(cid, ctitle)| json!({ "id": cid, "title": ctitle }))
                                .collect();
                            if !citing.is_empty() {
                                advisories.push(json!({
                                    "field": "title",
                                    "why": format!(
                                        "{} card(s) mention {} in their own title/desc/blocked_on \
                                         — a correction here may need to reach them too",
                                        citing.len(),
                                        next.id
                                    ),
                                    "citing_cards": citing,
                                }));
                            }
                        }
                    }
                }
            }
            // `desc_append` appends instead of the destructive replace (Python
            // parity, amux-server.py:69887). The cutover dropped it, so every
            // `amux board progress` since has printed "progress noted" and
            // written NOTHING — AC-323, and the sanctioned way CLAUDE.md tells
            // sessions to record an outcome before a gate transition.
            //
            // Python's own comment records the harsher version of this bug: the
            // field was accepted, ignored, and the destructive replace ran
            // anyway — ~20 silent wipes in one day, nine cards rebuilt from
            // /history. Both natural shapes work, so the obvious guess is right:
            //   {desc_append: "text"}             -> old + "\n" + text
            //   {desc: "text", desc_append: true} -> old + "\n" + text
            //   {desc_append: false}              -> plain replace semantics
            let mut appended_note: Option<String> = None;
            let desc_effective: Option<String> = match map.get("desc_append") {
                None | Some(Value::Bool(false)) => body_str(&map, "desc"),
                Some(v) => {
                    let text = match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Bool(true) => body_str(&map, "desc"),
                        _ => None,
                    };
                    match text {
                        Some(t) if !t.is_empty() => {
                            // Kept for the AVE-36 owner notice below.
                            appended_note = Some(t.trim().chars().take(400).collect());
                            let old = next.desc.trim_end();
                            Some(if old.is_empty() {
                                t.trim().to_string()
                            } else {
                                format!("{old}\n{t}").trim().to_string()
                            })
                        }
                        // Empty/non-string append is a no-op, NOT a wipe.
                        _ => body_str(&map, "desc"),
                    }
                }
            };
            // A HAND-ROLLED FOLD MUST BE AS READABLE AS A PEER-DRIVEN ONE.
            //
            // b3db93fd taught the terminal summary and the task callback to
            // recognise a fold, by reading the `capture folded into <ID>` line
            // that the peer-fold path writes. mixpeek-frustrations then measured
            // the gap that leaves, on their own cards:
            //
            //   MS-1369  discarded   server fold line present: True
            //   SP-713   discarded   present: True
            //   MF-888   discarded   present: FALSE
            //   MF-890   discarded   present: FALSE
            //   MF-893   discarded   present: FALSE
            //
            // The last three were folded BY HAND (`board progress` then `board
            // discard`), which authors no such line, so they still rendered with
            // the four not-recorded clauses this fix exists to remove. The
            // predicate was right and its input was not always written.
            //
            // So the caller names the target and the server writes the line.
            // That is an assertion a lane makes about its OWN action, which is
            // categorically different from inferring a fold out of a free-text
            // progress note — the classifier both reporters explicitly ruled
            // out, because guessing intent from prose fails open in the
            // expensive direction.
            if let Some(target) = body_str(&map, "folded_into")
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
            {
                // A card folded into ITSELF is nonsense and would make the
                // summary point at the shell the reader is already looking at.
                if target != next.id {
                    let old_log = next.log.as_deref().unwrap_or("").trim_end().to_string();
                    let line = format!("capture folded into {target}");
                    next.log = Some(if old_log.is_empty() {
                        line
                    } else {
                        format!("{old_log}\n{line}")
                    });
                }
            }
            // Nullable epoch seconds. An explicit null CLEARS (re-arming a
            // trigger for re-verification); absent leaves it alone.
            if let Some(v) = map.get("last_verified_at") {
                let next_v = match v {
                    Value::Null => None,
                    Value::Number(n) => n.as_i64(),
                    Value::String(s) => s.trim().parse::<i64>().ok(),
                    _ => next.last_verified_at,
                };
                if next_v != next.last_verified_at {
                    next.last_verified_at = next_v;
                    changed.push("last_verified_at".into());
                }
            }
            if let Some(d) = desc_effective {
                if d != next.desc {
                    // A REPLACE THAT DESTROYS MOST OF A DESCRIPTION MUST SAY SO
                    // AT WRITE TIME (mvs-infra, 2026-08-23).
                    //
                    // Reported as a near-data-loss recovered from
                    // _amux_state_events: a worker listed the board, read
                    // `desc` as empty, and PATCHed a fresh description over
                    // 4082 characters of merge evidence. The delta was already
                    // computed — the History line said "desc -4082 chars" — and
                    // it was written where only someone reading the card
                    // afterwards would find it. The discriminator existed and
                    // reached nobody at the moment it could still prevent
                    // anything, which is the same defect as AMUX-3562 one
                    // subsystem over.
                    //
                    // The reader-side half is a trap that cannot be fixed by
                    // shipping more fields: GET /api/board already sends
                    // `desc_len`, `desc_head` and `slim: true`, and the caller
                    // still read absence as emptiness, because `.get("desc")`
                    // returns None and says nothing. So the guard belongs on
                    // the WRITE, where the truth is known regardless of how the
                    // caller read the list.
                    //
                    // Deliberately narrow. Clearing a short desc, growing one,
                    // or trimming a little are all untouched; this needs a
                    // majority of a SUBSTANTIAL description to disappear, which
                    // is rare and is worth one explicit field. `desc_append`
                    // never trips it — it only ever grows.
                    let before = next.desc.chars().count();
                    let after = d.chars().count();
                    let acked = map
                        .get("desc_shrink_ack")
                        .map(crate::api::py_truthy)
                        .unwrap_or(false);
                    let forced = map.get("force").map(crate::api::py_truthy).unwrap_or(false);
                    // TWO RULES, and the second is about a different act
                    // (AMUX-3576). Size catches the owner clobbering their own
                    // long write-up. It does NOT catch a peer clobbering
                    // someone else's, because that act is not distinguished by
                    // magnitude: AF-180 went 3055 -> 1958, a 36% drop that sits
                    // comfortably under any threshold that avoids crying wolf
                    // on ordinary trims, and it destroyed a peer's review notes
                    // exactly as thoroughly as the 60% one next to it did.
                    //
                    // The honest discriminator is WHOSE PROSE IS BEING
                    // DESTROYED (amux-frustrations' framing, sharper than my
                    // "non-owner"): a reviewer replacing their own earlier note
                    // is fine, the owner rewriting their own card is fine, and
                    // a reviewer replacing the OWNER's write-up is the act. That
                    // is a comparison rather than a threshold, so it can be
                    // exact instead of tuned.
                    //
                    // `!d.contains(old)` is what makes it about DESTRUCTION
                    // rather than size: any append, or any rewrite that keeps
                    // what was there, passes untouched however much it adds.
                    //
                    // I am the specimen. I destroyed ~6400 characters across
                    // AF-178, AF-180 and AF-182 while acking reviews, having
                    // shipped the size rule an hour earlier and written the
                    // commit message explaining why desc_append exists.
                    //
                    // THE TWO NUMERIC FLOORS ARE GONE (AF-191, 2026-08-24), and
                    // they were the whole remaining hole. This rule declares
                    // itself to be about destruction and then gated itself on
                    // `before >= 200` and a 200-character NET LOSS, so both of
                    // the acts it names walked straight through:
                    //
                    //   amux-cloud, reproduced on scratch AC-391: a peer PATCH
                    //   replaced their 54-char desc with 17 chars. 54 < 200, so
                    //   no rule fired. Destroyed, silently, `applied: true`.
                    //
                    //   me, reproduced on scratch AF-194: a peer PATCH replaced
                    //   a 264-char desc with 392 chars of unrelated text. Net
                    //   loss ZERO because it GREW, so no rule fired. Every
                    //   character of the original gone, `applied: true`.
                    //
                    // A length delta cannot express content destruction — a
                    // longer replacement destroys exactly as much as a shorter
                    // one — and this is the file's own preference, stated eight
                    // lines up: "a comparison rather than a threshold, so it can
                    // be exact instead of tuned". The floors were the tuned part
                    // and they were tuned against the wrong quantity.
                    //
                    // WHAT REPLACES THEM, with no threshold at all: did the
                    // owner's prose SURVIVE? Take the longest line they wrote —
                    // the most substantial single thing on the card — and ask
                    // whether it is still present. Nothing to tune, and it
                    // separates the two cases the floors were reaching for:
                    //   - a peer fixing a typo in a multi-line desc leaves the
                    //     longest line intact, so it passes as before;
                    //   - a wholesale replace takes the anchor with it, at any
                    //     magnitude and in either direction.
                    // For a single-line desc the anchor IS the desc, so a peer
                    // replacing it is refused. That is amux-cloud's incident
                    // exactly, and the escape is one field (`desc_shrink_ack`)
                    // which the refusal prints, or `desc_append` to add instead.
                    let owner_now = row.session.clone().unwrap_or_default().trim().to_string();
                    let destroys_peer_prose =
                        desc_replace_destroys_peer_prose(&owner_now, &caller_lane, &next.desc, &d);
                    if !acked && !forced && (destroys_peer_prose || (before >= 500 && after * 2 < before)) {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                            StatusCode::CONFLICT,
                            json!({
                                // NAME WHICH RULE FIRED, because the remedy
                                // differs. The size rule usually means the
                                // caller read `desc` off the slim list and took
                                // absence for emptiness; the authorship rule
                                // means they read it fine and used replace
                                // semantics on someone else's prose. Telling a
                                // reviewer to go re-read the list is the wrong
                                // advice for the second and was the only advice
                                // available before AMUX-3576.
                                "error": if destroys_peer_prose {
                                    format!(
                                        "refusing to replace {owner_now}'s description on \
                                         {id_w} — you are {caller_lane}, and none of their \
                                         {before} characters survive it (their longest line is \
                                         gone). Length is not the test and this refusal fires \
                                         whether your text is shorter or longer. To add your \
                                         note below theirs — almost always what a reviewer or a \
                                         commenter means — resend as PATCH /api/board/{id_w} \
                                         with \"desc_append\": \"your note\". It is a FIELD, \
                                         not a sub-path. If you really do mean to replace their \
                                         text, resend with desc_shrink_ack: true."
                                    )
                                } else {
                                    format!(
                                        "refusing to shrink desc from {before} to {after} chars \
                                         ({} lost). If you read this card from GET /api/board, \
                                         note that the list OMITS `desc` (it ships \
                                         desc_len/desc_head and slim:true) — an absent field is \
                                         not an empty one. Re-read GET /api/board/{id_w} first. \
                                         To append instead, resend with \"desc_append\": \
                                         \"your text\" in place of \"desc\" — a FIELD, not a \
                                         sub-path. If the replace is intended, resend with \
                                         desc_shrink_ack: true.",
                                        before - after
                                    )
                                },
                                "id": id_w,
                                "ok": false,
                                "blocked": true,
                                "kind": "desc_shrink_blocked",
                                "rule": if destroys_peer_prose { "authorship" } else { "size" },
                                "owner": owner_now,
                                "writer": caller_lane,
                                "desc_len_before": before,
                                "desc_len_after": after,
                                "ack_field": "desc_shrink_ack",
                                "append_instead": "desc_append",
                                // The NAME alone reads as a path on an API whose board
                                // resource has six POST sub-paths (archive, restore,
                                // status-request, status-update, claim). Ship the shape
                                // beside it so a caller can copy a working request out of
                                // the refusal (AF-187, and AMUX-2325 one endpoint over).
                                "append_example": {
                                    "method": "PATCH",
                                    "path": format!("/api/board/{id_w}"),
                                    "body": {"desc_append": "your text"},
                                },
                            }),
                            ),
                            no_write(),
                        );
                    }
                    next.desc = d;
                    changed.push("desc".into());
                }
            }
            // Nullable string columns: explicit null/"" clears, absent leaves.
            let set_opt =
                |key: &str, field: &mut Option<String>, changed: &mut Vec<String>| {
                    if let Some(v) = body_opt_str(&map, key) {
                        let v = v.filter(|s| !s.trim().is_empty());
                        if *field != v {
                            *field = v;
                            changed.push(key.into());
                        }
                    }
                };
            set_opt("session", &mut next.session, &mut changed);
            set_opt("reviewer", &mut next.reviewer, &mut changed);
            set_opt("shepherd", &mut next.shepherd, &mut changed);
            set_opt("epic", &mut next.epic, &mut changed); // AMUX-2992: assign/clear a card's epic
            set_opt("due", &mut next.due, &mut changed);
            set_opt("due_time", &mut next.due_time, &mut changed);
            set_opt("evidence", &mut next.evidence, &mut changed);
            set_opt("ask_type", &mut next.ask_type, &mut changed);
            set_opt("ask_question", &mut next.ask_question, &mut changed);
            set_opt("ask_unblocks", &mut next.ask_unblocks, &mut changed);
            set_opt("ask_actor", &mut next.ask_actor, &mut changed);
            set_opt("next_action", &mut next.next_action, &mut changed);
            // Once a terminal transition has recorded the authoritative
            // board summary, a replaying provider payload must not replace it
            // with stale prose and turn an idempotent retry into a second
            // applied write. A new terminal entry still flows through the
            // normal setter and save_patched replaces it atomically.
            let reopening_terminal_card = bs::is_terminal_status(&row.status)
                && map
                    .get("status")
                    .and_then(Value::as_str)
                    .and_then(bs::parse_status)
                    .is_some_and(|target| {
                        !bs::is_terminal_status(bs::db_status_spelling(target))
                    });
            let terminal_summary_locked = bs::is_terminal_status(&row.status)
                && !reopening_terminal_card
                && row
                    .last_result
                    .as_deref()
                    .is_some_and(|summary| summary.starts_with("Final outcome:"));
            if terminal_summary_locked && map.contains_key("last_result") {
                ignored.push("last_result".into());
            } else {
                set_opt("last_result", &mut next.last_result, &mut changed);
            }
            set_opt("unresolved", &mut next.unresolved, &mut changed);
            set_opt("blocked_on", &mut next.blocked_on, &mut changed);
            // AF-711: acceptance_criteria used to route through `set_opt` like a
            // plain nullable string column, via `body_opt_str`'s
            // `Some(v) => Some(v.as_str().map(str::to_string))`. An incoming
            // JSON ARRAY (the shape `board decompose` itself writes, and the
            // shape every caller of a field literally named "criteria, plural"
            // would reasonably send) makes `Value::as_str()` return `None` —
            // INDISTINGUISHABLE from an explicit null-clears-the-field request
            // in that same match arm. The write proceeded as a silent CLEAR,
            // not a no-op: it destroyed seven tubescience cards' and three
            // mixpeek-general cards' acceptance criteria in two separate
            // incidents before this was caught, with the response still
            // reporting `applied:true`.
            //
            // Fixed by encoding whatever the caller sent (string or array of
            // strings) as JSON before storing, matching the encoding
            // `board decompose` already uses — so a plain string PATCH also
            // reads back as the identical string, not silently unrenderable
            // (see `parse_json_or_raw_string` on the read side) — and
            // rejecting any other JSON shape outright rather than coercing it
            // into a clear.
            if let Some(v) = map.get("acceptance_criteria") {
                match encode_acceptance_criteria(v) {
                    Ok(encoded) => {
                        if next.acceptance_criteria != encoded {
                            next.acceptance_criteria = encoded;
                            changed.push("acceptance_criteria".into());
                        }
                    }
                    Err(msg) => {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({"error": msg, "item": row.id}),
                            ),
                            no_write(),
                        );
                    }
                }
            }
            set_opt("decision_question", &mut next.decision_question, &mut changed);
            set_opt("decision_rationale", &mut next.decision_rationale, &mut changed);
            set_opt("decision_supersedes", &mut next.decision_supersedes, &mut changed);
            set_opt("waiting_on", &mut next.waiting_on, &mut changed);
            if next.epic != row.epic {
                if let Some(parent) = next.epic.as_deref() {
                    if let Some(cycle) = bs::epic_cycle(conn, &row.id, parent)? {
                        return finish(&slot_w, PatchOut::Refused(StatusCode::BAD_REQUEST,
                            json!({"error":format!("circular epic: {}",cycle.join(" -> ")),
                                "code":"lineage_cycle", "cycle":cycle})), no_write());
                    }
                }
            }
            if let Some(spec) = map.get("callback") {
                if caller_lane.is_empty() {
                    return finish(
                        &slot_w,
                        PatchOut::Refused(
                            StatusCode::BAD_REQUEST,
                            json!({"error": "a callback requires a verified X-Amux-Worker requester"}),
                        ),
                        no_write(),
                    );
                }
                // A delegated card's return contract belongs to its verified
                // requester. The recipient may finish the work, but cannot
                // silently disable the callback or redirect it to itself/a
                // third worker. Standalone cards have no requester and may be
                // armed by the verified worker editing them.
                if let Some(requester) = next.requested_by.as_deref() {
                    if requester != caller_lane {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::FORBIDDEN,
                                json!({
                                    "error": "only the verified requester may change this task callback",
                                    "requester": requester,
                                    "caller": caller_lane,
                                }),
                            ),
                            no_write(),
                        );
                    }
                }
                if matches!(next.callback_state.as_deref(), Some("pending" | "dispatching" | "queued")) {
                    return finish(
                        &slot_w,
                        PatchOut::Refused(
                            StatusCode::CONFLICT,
                            json!({
                                "error": "this task callback has already fired or is being dispatched",
                                "callback": next.snapshot()["callback"].clone(),
                            }),
                        ),
                        no_write(),
                    );
                }
                let disabling = matches!(spec, Value::Null | Value::Bool(false));
                if !disabling && bs::is_terminal_status(&next.status) {
                    return finish(
                        &slot_w,
                        PatchOut::Refused(
                            StatusCode::CONFLICT,
                            json!({"error": "cannot arm a completion callback after a task is already terminal"}),
                        ),
                        no_write(),
                    );
                }
                let callback_owner = next
                    .requested_by
                    .clone()
                    .unwrap_or_else(|| caller_lane.clone());
                let (target, prompt) = match spec {
                    Value::Null | Value::Bool(false) => (None, None),
                    Value::Bool(true) => (Some(callback_owner.clone()), None),
                    Value::String(s) => (
                        Some(callback_owner.clone()),
                        Some(s.trim().to_string()).filter(|s| !s.is_empty()),
                    ),
                    Value::Object(cb) => {
                        let target = cb
                            .get("session")
                            .and_then(Value::as_str)
                            .unwrap_or(&callback_owner)
                            .trim()
                            .to_string();
                        let prompt = cb
                            .get("prompt")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        (Some(target), prompt)
                    }
                    _ => {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({"error": "callback must be true, false, a prompt string, or {session?, prompt?}"}),
                            ),
                            no_write(),
                        )
                    }
                };
                if target.as_deref().is_some_and(|t| t != callback_owner) {
                    return finish(
                        &slot_w,
                        PatchOut::Refused(
                            StatusCode::FORBIDDEN,
                            json!({
                                "error": "a task callback may return only to the verified requester",
                                "requester": callback_owner,
                                "callback_session": target,
                            }),
                        ),
                        no_write(),
                    );
                }
                if next.callback_session != target || next.callback_prompt != prompt {
                    next.callback_session = target;
                    next.callback_prompt = prompt;
                    next.callback_state = next.callback_session.as_ref().map(|_| "armed".into());
                    next.callback_message_id = None;
                    next.callback_fired_at = None;
                    next.callback_error = None;
                    if next.callback_session.is_some() && next.requested_by.is_none() {
                        next.requested_by = Some(caller_lane.clone());
                    }
                    changed.push("callback".into());
                }
            }
            // A TRIGGER MUST NOT EAT AN AUTOFIX SIGNATURE (AMUX-3686).
            //
            // `source_ref` has two owners. autofix stores its fault signature
            // there (`autofix:<sig>`), and `open_card_for_fault` reads it to
            // suppress a duplicate filing. `amux board backlog --trigger` also
            // writes it — the external condition a parked card waits on — and
            // that write is a plain overwrite.
            //
            // So parking an autofix card exactly as the board's own idle nudge
            // prescribes DESTROYS the dedupe key, and the next tick files a
            // duplicate of the card you just parked. Measured 2026-08-24 on
            // AMUX-3651: parked with a trigger, AMUX-3685 filed minutes later
            // for the same fault. The sanctioned instruction caused it, which
            // is why a rule telling people to be careful would not help.
            //
            // NARROW ON PURPOSE: a trigger replacing another TRIGGER is normal
            // and still allowed. Only an `autofix:` prefix is protected,
            // because it is the only value with a second reader.
            //
            // The parked semantics survive untouched: board_drive tests that
            // source_ref is NON-EMPTY (board_drive.rs:1677, :2422) and never
            // reads the value, so the signature parks the card just as well as
            // the trigger text did. What the trigger loses is a place to put
            // its prose, and the card body is where a human reads it anyway —
            // so it goes there rather than being dropped.
            {
                let incoming = body_str(&map, "source_ref");
                let protected = next
                    .source_ref
                    .as_deref()
                    .is_some_and(|cur| cur.starts_with("autofix:"));
                match (incoming, protected) {
                    (Some(t), true) if !t.starts_with("autofix:") => {
                        tracing::warn!(
                            card = %next.id, trigger = %t, kept = %next.source_ref.clone().unwrap_or_default(),
                            "board: refused to overwrite an autofix signature with a trigger \
                             (AMUX-3686) — the card stays parked and the dedupe key survives; \
                             the condition is recorded in the card body"
                        );
                        // Not silent, and not lost: the reader of this card sees
                        // the condition, which is the only consumer that ever
                        // needed the TEXT.
                        let note = format!(
                            "\n\nPARKED ON: {t}\n(recorded here rather than in source_ref, which \
                             holds this card's autofix dedupe signature — AMUX-3686)"
                        );
                        next.desc.push_str(&note);
                        if !changed.iter().any(|c| c == "desc") {
                            changed.push("desc".into());
                        }
                        // AND TELL THE CALLER, not only the card (AMUX-3791).
                        // The WARN above reaches a log nobody is tailing and
                        // the note reaches a reader who opens the card; the
                        // operator who ran the command saw "→ backlog" and
                        // nothing else. This is the ONE write where the field
                        // you named is deliberately not the field that
                        // changed, so verifying the obvious way — read back
                        // source_ref — returns the old value and reads as a
                        // silent drop. That false negative cost a probe
                        // against a live card and very nearly a bug report
                        // against code doing exactly the right thing.
                        diverted.push(json!({
                            "field": "source_ref",
                            "landed_in": "desc",
                            "value": t,
                            "why": "this card's autofix dedupe signature occupies source_ref \
                                    and overwriting it would let the detector file duplicates \
                                    (AMUX-3686)",
                        }));
                    }
                    _ => set_opt("source_ref", &mut next.source_ref, &mut changed),
                }
                // An explicit trigger write is the same parking action through
                // PATCH as through `amux board backlog --trigger`. Requiring the
                // caller to know about a second field made a successful park
                // immediately drain again (AMUX-4168). Stamp the action here,
                // including reasserting the same trigger and autofix diversions.
                // Explicit timestamps/null still win; unrelated edits never
                // refresh a trigger and cannot keep a backlog parked forever.
                if body_str(&map, "source_ref").is_some_and(|v| {
                    !v.trim().is_empty() && !v.starts_with("autofix:")
                }) && !map.contains_key("last_verified_at")
                {
                    next.last_verified_at = Some(now_secs());
                    if !changed.iter().any(|c| c == "last_verified_at") {
                        changed.push("last_verified_at".into());
                    }
                    tracing::warn!(
                        card = %next.id,
                        verdict = "trigger_timestamp_repaired",
                        "board: explicit trigger write supplied no timestamp; recorded parking time to prevent immediate re-drain"
                    );
                    advisories.push(json!({
                        "field": "last_verified_at",
                        "set_to": next.last_verified_at,
                        "why": "Recorded the explicit trigger write's parking time, as the CLI does, so auto-drain waits for the trigger recheck window.",
                    }));
                }
            }
            if let Some(ot) = body_str(&map, "owner_type") {
                let ot = if ot == "agent" { "agent" } else { "human" }.to_string();
                if ot != next.owner_type {
                    next.owner_type = ot;
                    changed.push("owner_type".into());
                }
            }
            if let Some(p) = map.get("pinned") {
                let p = match p {
                    Value::Bool(b) => i64::from(*b),
                    v => v.as_i64().unwrap_or(0),
                };
                if p != next.pinned {
                    next.pinned = p;
                    changed.push("pinned".into());
                }
            }
            if let Some(p) = map.get("pos").and_then(|v| v.as_f64()) {
                if (p - next.pos).abs() > f64::EPSILON {
                    next.pos = p;
                    changed.push("pos".into());
                }
            }
            // `archived` via PATCH — Python parity (AMUX-2492, py:70294):
            // the SPA's card archive and the harness cleanup PATCH this
            // field. Python's coercion: str(v).lower() in (1,true,yes,on).
            // Cross-lane ARCHIVING (a named caller hiding another lane's
            // card) requires `authorized_by` — it removes the card from
            // every view and autonomy loop, a termination in effect.
            // UN-archiving is never gated, or the un-do is unreachable.
            if let Some(v) = map.get("archived") {
                let arc_v = patch_archived_value(v);
                if arc_v == 1 {
                    let owner = row.session.clone().unwrap_or_default().trim().to_string();
                    let authorized = map
                        .get("authorized_by")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("");
                    // AF-701: `is_local_member` outranks the identity check — a
                    // verified local member (Ethan) is always allowed, same as
                    // before. What changed is that a caller who is NEITHER named
                    // NOR the local member (a truly anonymous request) no longer
                    // gets waved through just because `caller_lane` reads "".
                    // That emptiness used to mean "skip the check"; it now means
                    // "cannot possibly equal `owner`", which is the same rule
                    // this block already applies to every named non-owner.
                    if !is_local_member && !owner.is_empty() && owner != caller_lane
                        && authorized.is_empty()
                    {
                        let caller_desc =
                            if caller_lane.is_empty() { "An anonymous caller" } else { &caller_lane };
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({
                                    "error": "cross-lane destruction requires authorized_by",
                                    "why": format!(
                                        "{caller_desc} is archiving {}, which belongs to {owner}. \
                                         Archiving hides it from every board view AND every \
                                         autonomy loop, so it is a termination in effect even \
                                         though the status is untouched.",
                                        row.id
                                    ),
                                    "how": format!(
                                        "add {{\"authorized_by\": \"<who asked>\"}}, or use \
                                         `amux board archive {} --authorized-by \"<who>\"`. An \
                                         anonymous caller (no X-Amux-Session/X-Amux-Worker header) \
                                         must send this header or authorized_by — there is no \
                                         exemption for an unnamed caller.",
                                        row.id
                                    ),
                                    "card_owner": owner,
                                }),
                            ),
                            no_write(),
                        );
                    }
                    // AF-701: archiving a card that is still `needsyou` hides an
                    // unanswered ask instead of resolving it — measured fleet-wide
                    // by mixpeek-orchestrator at 98 such cards, most weeks old,
                    // several with no attributable actor at all.
                    //
                    // AF-712 generalizes this from needsyou alone to every
                    // non-terminal status: measured fleet-wide at 885
                    // archived+non-terminal rows (todo/doing/backlog/blocked/
                    // needsyou), UP from AF-555's 785-of-2133 baseline two weeks
                    // earlier — new instances kept forming because only needsyou
                    // was ever gated. `survives_archive()` (amux-core::board)
                    // still names needsyou as the one status that is DESIGNED to
                    // stay non-terminal while archived (the ask is still owed);
                    // every other non-terminal status has no such design intent,
                    // it is just a card nobody closed before hiding it.
                    //
                    // NOT gated on "the same request also changes status": tried
                    // that first, and `next.archived` is set (a few lines below)
                    // before the status-transition code runs later in this same
                    // function, which reads `apply_transition`'s own archived-
                    // check against a task that this request has already marked
                    // archived — so a combined {"archived":true,"status":"x"} PATCH
                    // was refused ArchivedTaskImmutable regardless of what this
                    // gate did. That escape never worked; documenting it as a
                    // valid path would have been the ethos-rule-3 lie this fix
                    // exists to remove. The real two-step path (change status
                    // to a terminal one in one request, archive in the next)
                    // still works and needs no help from this gate.
                    if !bs::is_terminal_status(&row.status) {
                        let outcome = map
                            .get("archive_outcome")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .unwrap_or("");
                        if outcome.is_empty() {
                            let why = if row.status == "needsyou" {
                                format!(
                                    "{} is in needsyou, which means someone is still owed \
                                     an answer. Archiving it removes it from the owner's \
                                     queue and every autonomy loop without answering the \
                                     ask, which is how it goes quiet instead of getting \
                                     resolved.",
                                    row.id
                                )
                            } else {
                                format!(
                                    "{} is still {}, not a terminal status (done/verified/\
                                     discarded). Archiving it removes it from every board \
                                     view and autonomy loop while the work is still open, \
                                     which is how an archived-but-unfinished card accumulates \
                                     silently instead of ever getting resolved (AF-712).",
                                    row.id, row.status
                                )
                            };
                            let how = if row.status == "needsyou" {
                                "add \
                                 {\"archive_outcome\": \"<answered|withdrawn|discarded, and why>\"} \
                                 to archive it while recording why, or first PATCH \
                                 `status` away from needsyou in its own request (the \
                                 ask was answered or the card is done), then archive \
                                 it in a second request."
                            } else {
                                "add \
                                 {\"archive_outcome\": \"<why this is being hidden while still open>\"} \
                                 to archive it while recording why, or first PATCH \
                                 `status` to done/verified/discarded in its own request, \
                                 then archive it in a second request."
                            };
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::BAD_REQUEST,
                                    json!({
                                        "error": "archiving a non-terminal card requires an outcome",
                                        "why": why,
                                        "how": how,
                                        "status": row.status,
                                        "item": row.id,
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }
                }
                if arc_v != next.archived {
                    next.archived = arc_v;
                    changed.push("archived".into());
                    if arc_v == 1 {
                        if let Some(o) = map.get("archive_outcome").and_then(Value::as_str) {
                            let o = o.trim();
                            if !o.is_empty() {
                                next.log = Some(bs::append_log(
                                    next.log.as_deref(),
                                    &hhmm(),
                                    &format!("{actor_name}: archive_outcome: {o}"),
                                ));
                            }
                        }
                    }
                }
            }
            if let Some(t) = body_str(&map, "type") {
                let t = t.trim().to_lowercase();
                if !t.is_empty() {
                    if !bs::KNOWN_TYPES.contains(&t.as_str()) {
                        // Reject at the door: an unknown type silently
                        // inherits the code gate non-code work cannot satisfy.
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({
                                    "error": format!("unknown type {t:?}"),
                                    "valid_types": bs::KNOWN_TYPES,
                                    "why": "The gate is DERIVED from type. An unknown type would silently fall back to the strictest (code) gate, which non-code work cannot satisfy without asserting a merge that never happened.",
                                }),
                            ),
                            no_write(),
                        );
                    }
                    if t != next.item_type {
                        next.item_type = t;
                        changed.push("type".into());
                        // AMUX-3058: a non-empty `gate` OVERRIDE pins the gate
                        // over the type — effective_gate returns row.gate before
                        // deriving from item_type — so retyping to escape a wrong
                        // gate (ethos rule 3's sanctioned escape) was a DEAD END
                        // while an override stood, including one that matched no
                        // type's default (a code-criteria override on a non-code
                        // card, TUBES-1622). Retyping is an explicit statement that
                        // the card's KIND changed and the gate derives from the
                        // kind, so a stale override is dropped here and the gate
                        // re-derives from the new type. A caller that wants a custom
                        // gate on the retyped card sends `gate` in this SAME PATCH:
                        // the gate handler below runs after this and re-sets it.
                        if next.gate.is_some() {
                            next.gate = None;
                            changed.push("gate".into());
                            tracing::info!(
                                target: "amux::board", id = %next.id,
                                "cleared a stale gate override on retype so the gate re-derives from the new type (AMUX-3058)"
                            );
                        }
                    }
                }
            }
            if let Some(v) = map.get("gate") {
                let list = match body_str_list(v) {
                    Ok(l) => l,
                    Err(e) => {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({ "error": format!("gate {e}") }),
                            ),
                            no_write(),
                        )
                    }
                };
                let new_gate = if list.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&list).unwrap_or_default())
                };
                if next.gate_criteria() != list {
                    next.gate = new_gate;
                    changed.push("gate".into());
                }
            }
            if let Some(v) = map.get("depends_on") {
                let deps = match body_str_list(v) {
                    Ok(l) => l,
                    Err(e) => {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({ "error": format!("depends_on {e}") }),
                            ),
                            no_write(),
                        )
                    }
                };
                if deps != next.depends_on {
                    if let Some(cycle) = bs::depends_on_cycle(conn, &row.id, &deps)? {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({
                                    "error": format!("circular depends_on: {}", cycle.join(" -> ")),
                                    "cycle": cycle,
                                }),
                            ),
                            no_write(),
                        );
                    }
                    next.depends_on = deps;
                    changed.push("depends_on".into());
                }
            }
            if let Some(v) = map.get("tags") {
                let tags = match body_str_list(v) {
                    Ok(l) => l,
                    Err(e) => {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::BAD_REQUEST,
                                json!({ "error": format!("tags {e}") }),
                            ),
                            no_write(),
                        )
                    }
                };
                let mut a = tags.clone();
                let mut b = next.tags.clone();
                a.sort();
                b.sort();
                if a != b {
                    next.tags = tags.clone();
                    tags_change = Some(tags);
                    changed.push("tags".into());
                }
            }

            // ---- status transition through the core state machine --------
            let mut status_event: Option<(String, String)> = None;

            // ---- user-created columns (AMUX-2609) ------------------------
            //
            // Python's board columns are fully dynamic: create a column, drag
            // cards into it. Rust's `TaskStatus` is a closed enum, so
            // `parse_status` returned None and the PATCH bounced 400 — while
            // the SPA had ALREADY moved the card optimistically and cached it.
            // The user saw the card sit in the new column behind a bare
            // "Error: 400" toast until the next poll silently snapped it back.
            //
            // This deliberately does NOT add a `Custom(String)` cell to
            // `TaskStatus`. That enum is the BUILTIN LIFECYCLE the state
            // machine reasons about, and widening it would:
            //   * lose `Copy`, breaking ~40 by-value sites (20 of them
            //     `match task.status` inside `apply_transition` alone);
            //   * break `db_status_spelling`'s `&'static str` return, which
            //     cannot express an owned custom id;
            //   * leave `disposition_is_total_over_every_status` iterating
            //     `TaskStatus::ALL`, a `const [TaskStatus; 11]` that CANNOT
            //     contain a `Custom` — so the totality PROOF silently narrows
            //     to builtins while still passing (ethos rule 7: a check that
            //     can no longer fail);
            //   * and route every custom move through `BoardTransition::Force`
            //     (no `named_transition` arm exists), filling the one audited
            //     bypass trail with routine traffic until it means nothing
            //     (ethos rule 6).
            // `amux_core::workflow` already models dynamic columns properly
            // (ColumnId + ColumnRole::Custom); a `Custom` variant here would be
            // a THIRD spelling of the same idea.
            //
            // So the vocabulary is read from where users actually create
            // columns — the `statuses` table. Both `issues.status` and that
            // table are raw strings already, so nothing migrates. The card's
            // required semantics fall out for free: `board_drive`'s pickup and
            // the WIP-1 guard compare raw SQL against 'todo'/'doing', and the
            // terminal/rot checks against their own literals, so a custom
            // column is non-WIP, non-terminal and never auto-picked WITHOUT a
            // single new exclusion list (ethos rule 1: an exemption nobody
            // maintains is how things go invisible).
            //
            // A transition is UNMODELLED when EITHER end is outside the typed
            // vocabulary. Handling only the "into" direction would build a
            // roach motel — cards could enter a custom column and never leave —
            // which is precisely what ethos rule 3 forbids: every legitimate
            // state needs a truthful exit.
            let unmodelled_status = body_str(&map, "status").filter(|s| {
                bs::parse_status(s).is_none() || bs::parse_status(&next.status).is_none()
            });
            if let Some(target_in) = unmodelled_status {
                let target_typed = bs::parse_status(&target_in);
                let target_raw = match target_typed {
                    Some(t) => bs::status_to_db(t, &next.status),
                    None => target_in.trim().to_lowercase(),
                };

                // NOTE for whoever wires the orchestrator half (AMUX-2631/2/3,
                // de8a079): `db::workflow_store::load_workflow` is the richer
                // reader of this same `statuses` table and models column ROLE
                // and terminal behaviour. It is the right oracle once planning
                // needs to reason about custom columns; the membership check
                // here is deliberately the narrowest question ("does this
                // column exist"), against the same table, so the two cannot
                // disagree about what a column IS.
                //
                // The gate for an unmodelled move: a typed target keeps its
                // normal derived/override gate; a custom target uses the gate
                // the column itself carries (`statuses.gate`, written by the
                // column editor). Without this a custom column would be a
                // gate-shaped hole in the board.
                let eff_gate: Vec<String> = match target_typed {
                    Some(t) => bs::effective_gate_configured(conn, &next, t),
                    None => {
                        let found: Option<Option<String>> = conn
                            .query_row(
                                "SELECT gate FROM statuses WHERE id = ?1",
                                rusqlite::params![target_raw],
                                |r| r.get::<_, Option<String>>(0),
                            )
                            .ok();
                        let Some(gate_json) = found else {
                            // Neither a builtin nor a column that exists: a
                            // real typo. Name BOTH vocabularies and how to add
                            // one, so the refusal is actionable rather than a
                            // list the caller has already read.
                            let mut cols: Vec<String> = Vec::new();
                            if let Ok(mut stmt) =
                                conn.prepare("SELECT id FROM statuses ORDER BY position")
                            {
                                if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
                                    cols = rows.flatten().collect();
                                }
                            }
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::BAD_REQUEST,
                                    json!({
                                        "error": format!("unknown status {target_in:?}"),
                                        "valid_statuses": VALID_STATUSES,
                                        "configured_columns": cols,
                                        "how_to_add": "POST /api/board/statuses {\"label\": \"...\"}",
                                    }),
                                ),
                                no_write(),
                            );
                        };
                        gate_json
                            .as_deref()
                            .and_then(|g| serde_json::from_str::<Vec<String>>(g).ok())
                            .unwrap_or_default()
                    }
                };

                if target_raw != next.status {
                    let force = map.get("force").and_then(Value::as_bool).unwrap_or(false);
                    let reason = body_str(&map, "reason").unwrap_or_default();
                    let mut ack_via: Option<String> = None;
                    if !eff_gate.is_empty() && !force {
                        let gc = map.get("gate_checked").and_then(Value::as_array).map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(|s| s.trim().to_string())
                                .collect::<Vec<_>>()
                        });
                        if let Some(gc) = &gc {
                            // Exact FIRST, normalized as a fallback (AF-160):
                            // widening only, so no ack that passes today stops.
                            let missing: Vec<&String> = eff_gate
                                .iter()
                                .filter(|c| {
                                    !gc.contains(c)
                                        && !gc.iter().any(|g| ack_norm(g) == ack_norm(c))
                                })
                                .collect();
                            if !missing.is_empty() {
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::CONFLICT,
                                        json!({
                                            "error": "gate_checked does not match the gate",
                                            "ok": false,
                                            "blocked": true,
                                            "gate": eff_gate,
                                            "missing": missing,
                                            "you_sent": gc,
                                            "attempted_status": target_raw,
                                            "item": row.id,
                                            "item_type": next.item_type,
                                        }),
                                    ),
                                    no_write(),
                                );
                            }
                            ack_via =
                                Some(format!("gate_checked ({}/{})", gc.len(), eff_gate.len()));
                        } else if map.get("gate_ack").and_then(Value::as_bool).unwrap_or(false) {
                            ack_via = Some("gate_ack".into());
                        } else {
                            // `why_blocked` is deliberately EMPTY here: core
                            // cannot compute it for a column it does not
                            // model, and an empty list says exactly that
                            // rather than inventing a reason.
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    gate_409(&next, &eff_gate, &target_raw, &[], None, &caller_lane),
                                ),
                                no_write(),
                            );
                        }
                    }

                    let from_raw = next.status.clone();
                    let stamp = hhmm();
                    if let Some(via) = &ack_via {
                        next.log = Some(bs::append_log(
                            next.log.as_deref(),
                            &stamp,
                            &format!("{actor_name}: gate satisfied via {via} for {target_raw}"),
                        ));
                    }
                    // NOT logged as a force: this is an ordinary move between
                    // configured columns. Calling it a bypass would be the
                    // ethos-rule-6 failure in reverse — an audit line that
                    // cries wolf is as useless as one that never fires.
                    let line = if force {
                        format!("force by {force_actor}: {from_raw}->{target_raw} reason={reason}")
                    } else {
                        format!("{actor_name}: {from_raw} -> {target_raw} (user column)")
                    };
                    next.log = Some(bs::append_log(next.log.as_deref(), &stamp, &line));
                    // Gap 4: waiting_on side effects before status change.
                    crate::db::advance::apply_status_side_effects(&mut next, &target_raw);
                    next.status = target_raw.clone();
                    next.version += 1;
                    status_event = Some((from_raw, target_raw));
                    changed.push("status".into());
                }
            } else if let Some(target_in) = body_str(&map, "status") {
                let Some(target) = bs::parse_status(&target_in) else {
                    return finish(
                        &slot_w,
                        PatchOut::Refused(
                            StatusCode::BAD_REQUEST,
                            json!({
                                "error": format!("unknown status {target_in:?}"),
                                "valid_statuses": VALID_STATUSES,
                            }),
                        ),
                        no_write(),
                    );
                };
                let from = bs::parse_status(&next.status);
                let rechecking_verified = from == Some(TaskStatus::Verified) && target == TaskStatus::Verified
                    && map.get("reverify").and_then(Value::as_bool) == Some(true);
                if from != Some(target) || rechecking_verified {
                    let Some(from) = from else {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::CONFLICT,
                                json!({
                                    // Unreachable since AMUX-2609: the
                                    // `unmodelled_status` branch above claims
                                    // every case where either end is outside
                                    // the typed vocabulary. Kept as an honest
                                    // 409 rather than an unwrap so a future
                                    // edit to that predicate degrades to a
                                    // refusal instead of a panic — and no
                                    // longer instructs the caller to go use a
                                    // server that was retired.
                                    "error": format!(
                                        "current status {:?} is outside the typed vocabulary \
                                         and was not routed to the user-column path",
                                        next.status
                                    ),
                                }),
                            ),
                            no_write(),
                        );
                    };
                    let Some(task) = next.to_task() else {
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::CONFLICT,
                                json!({ "error": "row cannot be mapped to a core task" }),
                            ),
                            no_write(),
                        );
                    };
                    let force = map.get("force").and_then(Value::as_bool).unwrap_or(false);
                    let reason = body_str(&map, "reason").unwrap_or_default();
                    // A CAPTURE IS AN ENVELOPE, NOT A PARKABLE UNIT OF WORK
                    // (MR-174, mvs-research, 2026-09-09).
                    //
                    // The server already has exactly three honest exits for an
                    // auto-captured prompt: discard it when it is not work,
                    // reshape its body into one self-contained task, or
                    // decompose it atomically into an epic and ordered children.
                    // The board-drive nudge teaches those exits and exempts the
                    // untouched envelope from WIP for the same reason. The PATCH
                    // transition path nevertheless allowed a fourth exit:
                    // `doing -> backlog` (or `todo`) while the body was still
                    // the captured prompt.
                    //
                    // MR-174 walked that hole exactly. `status-update` claimed
                    // the direct human command as doing, the worker recorded a
                    // material status and an artifact, then the same command
                    // envelope moved back to backlog with a 14-day revisit and
                    // a prose trigger. The Messages link and history were all
                    // correct; the current board still erased the active-work
                    // disposition and the drive loop quite correctly treated
                    // the fresh trigger as parked. That is a state-machine
                    // contradiction, not a polling problem.
                    //
                    // Refuse only the retreat of an UNRESHAPED capture. A real
                    // task whose author rewrites `desc` in this same atomic
                    // PATCH no longer matches `is_capture_shell` and may be
                    // parked normally. New captures may still start in backlog,
                    // terminal dispositions still work, and an attributed,
                    // reasoned force remains the audited escape. This improves
                    // with the model: it requires the model to make the semantic
                    // judgment the harness cannot make, then preserves it.
                    if from == TaskStatus::Doing
                        && matches!(target, TaskStatus::Backlog | TaskStatus::Todo)
                        && bs::is_capture_shell(&next)
                        && !force
                    {
                        tracing::warn!(
                            target: "amux::board",
                            marker = "capture_requeue_refused",
                            card = %next.id,
                            worker = %next.session.as_deref().unwrap_or("-"),
                            actor = %actor_name,
                            from = %next.status,
                            to = %bs::db_status_spelling(target),
                            measured = true,
                            "worked capture envelope refused requeue without a semantic disposition"
                        );
                        return finish(
                            &slot_w,
                            PatchOut::Refused(
                                StatusCode::CONFLICT,
                                json!({
                                    "error": "captured command requires a disposition before requeue",
                                    "code": "capture_requeue_requires_disposition",
                                    "ok": false,
                                    "blocked": true,
                                    "measured": true,
                                    "item": next.id,
                                    "attempted_status": bs::db_status_spelling(target),
                                    "preserved_status": next.status,
                                    "why": "This card is still the auto-captured human command envelope. It was already claimed as active work; putting the unchanged envelope back in a queue loses the command's disposition and makes completed, delegated, and blocked work indistinguishable.",
                                    "how_to_fix": {
                                        "not_work": format!("amux board discard {} --outcome-stdin", next.id),
                                        "one_task": format!("amux board retitle {} \"<standalone task title>\" --desc-stdin", next.id),
                                        "several_tasks": format!("amux board decompose {} --stdin", next.id),
                                        "work_finished": "move the envelope to review/done with its outcome and evidence",
                                        "still_actively_working": "leave it in doing and post status/next_action/unresolved",
                                        "audited_escape": format!("amux board {} {} --force \"<why the capture itself must be requeued>\"", bs::db_status_spelling(target), next.id),
                                    },
                                }),
                            ),
                            no_write(),
                        );
                    }
                    // ONE-DOING-PER-SESSION (AMUX-1707 parity). Python's WIP
                    // filters verbatim: archived cards and dormant types
                    // (tripwire/watch) do not hold WIP — both were real
                    // incidents. The escape names the attributed CLI command
                    // (AMUX-2325: an escape publishable only in HTTP terms
                    // routes agents off the audited path).
                    let override_doing = map
                        .get("override_doing")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if target == TaskStatus::Doing
                        && task.status != TaskStatus::Doing
                        && !force
                        && !override_doing
                    {
                        if let Some(sess) = next.session.as_deref().filter(|s| !s.is_empty()) {
                            let holding: Vec<String> = conn
                                .prepare(
                                    "SELECT id FROM issues WHERE session = ?1 \
                                     AND status = 'doing' AND id != ?2 \
                                     AND deleted IS NULL AND COALESCE(archived,0) = 0 \
                                     AND COALESCE(type,'') NOT IN ('tripwire','watch','epic') \
                                     ORDER BY id",
                                )
                                .and_then(|mut st| {
                                    st.query_map(rusqlite::params![sess, next.id], |r| {
                                        r.get::<_, String>(0)
                                    })
                                    .map(|rows| rows.filter_map(Result::ok).collect())
                                })
                                .unwrap_or_default();
                            if !holding.is_empty() {
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::CONFLICT,
                                        json!({
                                            "error": "already holding doing",
                                            "ok": false,
                                            "blocked": true,
                                            "session": sess,
                                            "holding": holding,
                                            "cli": format!(
                                                "amux board doing {} --override-doing",
                                                next.id
                                            ),
                                        }),
                                    ),
                                    no_write(),
                                );
                            }
                        }
                    }
                    // RR-0048d (Invariant 50): leaving todo requires authored
                    // acceptance criteria — enforcement opt-in during
                    // coexistence (AMUX_RS_REQUIRE_CRITERIA=1); force bypasses
                    // WITH its audit line like every other gate.
                    if task.status == TaskStatus::Todo
                        && target != TaskStatus::Todo
                        && !target.is_terminal()
                        && !force
                    {
                        match crate::api::criteria::todo_exit_permitted(conn, &next.id) {
                            Ok(Ok(())) => {}
                            Ok(Err(msg)) => {
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::CONFLICT,
                                        json!({
                                            "error": "acceptance criteria required",
                                            "ok": false,
                                            "blocked": true,
                                            "item": next.id,
                                            "detail": msg,
                                            "or_reassign": reassign_exit(
                                                &next.id, next.session.as_deref(), &caller_lane,
                                            ),
                                        }),
                                    ),
                                    no_write(),
                                );
                            }
                            Err(e) => {
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        json!({ "error": e.to_string() }),
                                    ),
                                    no_write(),
                                );
                            }
                        }
                    }
                    // Resolve the gate AND the tier that produced it in one walk
                    // (AF-169): the refusal's advice must come from the same
                    // predicate the enforcement used, not a second reading.
                    let groups = next
                        .session
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(crate::api::session_verbs::lane_groups)
                        .unwrap_or_default();
                    let gate_trail = bs::effective_gate_trail(conn, &next, target, &groups);
                    let authz_line = gate_trail.log_line();
                    let eff_gate = gate_trail.criteria.clone();
                    let gate_src = Some(gate_trail.source.clone());
                    let gates = bs::core_gates(&eff_gate, target);
                    let target_raw = bs::status_to_db(target, &next.status);

                    // Global done-link constraint (Ethan, 2026-08-17): a card
                    // cannot enter `done` without pointing at the artifact it
                    // produced. It sits ALONGSIDE the ack gate, not inside it, so
                    // it is MACHINE-VALIDATED against the card text here and a
                    // `gate_ack` can never fake it (ethos rule 7). `force`
                    // bypasses it like any gate, and a per-worker/group/global
                    // `AMUX_DONE_LINK_REQUIRED=0` opts out (resolved worker >
                    // group > global by `done_link_required`).
                    let link_required = !force
                        && target == TaskStatus::Done
                        && bs::done_link_required(next.session.as_deref());
                    if link_required {
                        // EVIDENCE COUNTS AS THE LINK, because it is one
                        // (AMUX-3914). This gate scanned desc and log only,
                        // while `--evidence` — which AF-321 SEPARATELY REQUIRES
                        // on the same call — writes its own column. So the two
                        // verbs a lane reaches for first, `status-update` and
                        // `--evidence`, both wrote where this gate could not
                        // look, and it refused cards whose artifact was already
                        // recorded in the sanctioned place. Measured three times
                        // on 2026-08-30: mixpeek-general on MG-1538 (outcome in
                        // log, desc_len 0), and twice by amux, the second with a
                        // real commit sha sitting in --evidence.
                        //
                        // Evidence may satisfy the separate evidence gate with
                        // a reproducible command, but a command is not a
                        // produced asset. Keep the documented honest no-asset
                        // escape, and otherwise require an actual pointer.
                        // The explicit artifact registry is the canonical
                        // structured path. Requiring its ref to be duplicated
                        // in prose made a successful `amux board artifact`
                        // write insufficient to close its own task.
                        let registered_artifacts =
                            crate::db::artifact_store::list_for_task(conn, &next.id)?;
                        let retired_refs: HashSet<&str> = registered_artifacts
                            .iter()
                            .filter(|a| crate::db::artifact_store::is_retired_state(&a.state))
                            .map(|a| a.ref_value.as_str())
                            .collect();
                        let text_has_active_asset = |text: &str| {
                            bs::asset_refs(text)
                                .iter()
                                .any(|reference| !retired_refs.contains(reference.as_str()))
                        };
                        // A durable invalid/superseded disposition must apply
                        // everywhere evidence is consumed, not only in the UI.
                        // Otherwise the retired ref can still satisfy this
                        // gate by being repeated in --evidence.
                        let evidence_names_artifact = next.evidence.as_deref().is_some_and(|e| {
                            text_has_active_asset(e)
                                || (e.trim().to_ascii_lowercase().starts_with("none:")
                                    && bs::evidence_verdict(e) == bs::EvidenceVerdict::Ok)
                        });
                        let registered_artifact = registered_artifacts.iter().any(|a| {
                            !a.ref_value.trim().is_empty()
                                && !crate::db::artifact_store::is_retired_state(&a.state)
                        });
                        let has_link = text_has_active_asset(&next.desc)
                            || next.log.as_deref().is_some_and(text_has_active_asset)
                            || evidence_names_artifact
                            || registered_artifact;
                        if !has_link {
                            // Surfaces so a sweep catches the next one without a
                            // human noticing (two-fixes rule): grep
                            // `done_link_gate` in server-rs.log, and the
                            // structured `code` separates these from other 409s
                            // in /api/logs/analyze.
                            tracing::warn!(
                                "done_link_gate: blocked {} -> done for session {} (no asset link on the card)",
                                next.id,
                                next.session.as_deref().unwrap_or("-")
                            );
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    json!({
                                        "error": "done requires a link to the created asset",
                                        "code": "done_requires_asset_link",
                                        "ok": false,
                                        "blocked": true,
                                        "item": next.id,
                                        "attempted_status": target_raw,
                                        "or_reassign": reassign_exit(
                                            &next.id, next.session.as_deref(), &caller_lane,
                                        ),
                                        "why": "A card cannot be marked done without pointing at the artifact it produced: a URL, a repo file path, a commit sha, or a #PR/issue. This is a global constraint and gate_ack cannot satisfy it.",
                                        "how_to_fix": {
                                            "add_link": "PATCH /api/board/<id> with a desc containing the URL / file path / commit / #PR, then retry done.",
                                            // A COMMAND IS NOT ACCEPTED HERE, and this string used to say it was (AF-406).
                                            // Control-tested 2026-09-02 on a throwaway card, all three
                                            // through `amux board done --evidence-stdin`:
                                            //   a command (curl -sk "$AMUX_URL/api/logs/analyze?...") -> BLOCKED
                                            //   a repo path (crates/.../request_log.rs)              -> ACCEPTED
                                            //   a sha (ccefbcb6)                                     -> ACCEPTED
                                            // So the detector works and the one shape this text
                                            // advertised first was the one it rejects. That is a
                                            // documented escape that fails when walked (ethos rule 6),
                                            // shown to every lane that hits this gate -- 292 refusals
                                            // across 27 lanes in the 24h before this was found.
                                            // Whether a command SHOULD count as an artifact is a
                                            // separate question and belongs to whoever owns the gate;
                                            // this only stops the message promising it.
                                            "or_evidence": "--evidence naming the artifact satisfies this gate too (a repo path, URL, sha or #PR). A bare command does NOT: it says how to reproduce the finding, not what the work created. This card's evidence is empty or has nothing checkable in it.",
                                            "no_artifact": "If the work genuinely produced none, say so: evidence starting `none: <reason>` (three words or more) is accepted and counted, not a bypass.",
                                            "override_for_this_worker": "set AMUX_DONE_LINK_REQUIRED=0 in this worker's (or its group's, or the global) configuration.",
                                            "force": "true (explicit bypass; logged)"
                                        }
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }

                    // AF-321 sits BEHIND the link rule above, deliberately. The link check is a SHAPE
                    // check over the whole desc, so the card's own PROBLEM
                    // STATEMENT satisfies it: measured on the live board
                    // 2026-08-29, 843 of 1372 open cards (61%) passed it on
                    // their filed text with no work done, because a card that
                    // names the file it intends to edit contains a path. The
                    // evidence column cannot be filled that way — nobody has
                    // written it yet when the card is filed. Same opt-out
                    // ladder, same `force` bypass, both audited.
                    //
                    // ORDER MATTERS: the coarser rule answers first, so a card
                    // with no artifact anywhere still gets the older, broader
                    // message it has always got, and this narrower one fires
                    // only once that has been satisfied.
                    let evidence_required = !force
                        && target == TaskStatus::Done
                        && bs::done_evidence_required(next.session.as_deref());
                    if evidence_required {
                        let ev = next.evidence.clone().unwrap_or_default();
                        let verdict = bs::evidence_verdict(&ev);
                        if verdict != bs::EvidenceVerdict::Ok {
                            let (why, code) = match verdict {
                                bs::EvidenceVerdict::Missing => (
                                    "This card records nothing that was run or produced. `done` is where work stops on this board (3302 done against 3631 verified), so closing one has to name the proof: the command, the URL exercised, the screenshot path, the commit.",
                                    "done_requires_evidence",
                                ),
                                bs::EvidenceVerdict::NoArtifact => (
                                    "The evidence on this card is prose with nothing in it to check. Name the artifact: a command in backticks, a repo path, a URL, a commit sha, or a #PR.",
                                    "done_evidence_has_no_artifact",
                                ),
                                bs::EvidenceVerdict::UnexplainedNone => (
                                    "`none:` is the honest answer when a card genuinely produced no artifact, but it needs the reason after it — that text is what makes the escape countable instead of a blind spot.",
                                    "done_evidence_none_unexplained",
                                ),
                                bs::EvidenceVerdict::Ok => unreachable!(),
                            };
                            // Two-fixes rule: grep `done_evidence_gate` in
                            // server-rs.log, and the structured `code` splits
                            // these from other 409s in /api/logs/analyze.
                            tracing::warn!(
                                "done_evidence_gate: blocked {} -> done for session {} (verdict {:?})",
                                next.id,
                                next.session.as_deref().unwrap_or("-"),
                                verdict
                            );
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    json!({
                                        "error": "done requires evidence of what was run",
                                        "code": code,
                                        "ok": false,
                                        "blocked": true,
                                        "item": next.id,
                                        "attempted_status": target_raw,
                                        "why": why,
                                        "recorded_evidence": next.evidence,
                                        // AF-506, second pass. This refusal fires
                                        // BEFORE the gate-ack one, so a lane routing
                                        // a card away hits it FIRST and never sees the
                                        // exit added to gate_409 — measured against the
                                        // live server on the very commit that added it.
                                        // Same friction (a refusal that teaches one way
                                        // out), same remedy: if the work is another
                                        // lane's, you should not be closing this card at
                                        // all.
                                        "or_reassign": reassign_exit(
                                            &next.id, next.session.as_deref(), &caller_lane,
                                        ),
                                        "how_to_fix": {
                                            "cli": format!("amux board done {} --evidence-stdin  (heredoc; inline text is evaluated by YOUR shell)", next.id),
                                            "api": "PATCH /api/board/<id> with {\"evidence\": \"...\"} — writable on its own, so record it first and the transition cannot discard it",
                                            "accepted": [
                                                "a command, in backticks or on a `$ ` line",
                                                "a repo file path, a URL, a commit sha, or #PR",
                                                "`none: <reason>` when the card genuinely produced no artifact (stored and counted, not a bypass)"
                                            ],
                                            "what_to_run": "the repo's VERIFY.md names the proof for each surface",
                                            "override_for_this_worker": "set AMUX_DONE_EVIDENCE_REQUIRED=0 in this worker's (or its group's, or the global) configuration.",
                                            "force": "true (explicit bypass; logged)"
                                        }
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }

                    // AF-317 (a): A LANE'S `todo` IS A DISPATCH QUEUE, NOT A PILE.
                    //
                    // Ethan, 2026-08-29: "some workers have an infinite # of
                    // growing backlogs and todo then they go idle."
                    //
                    // AF-317's "median age 28.8 days" counted ARCHIVED cards and
                    // does not hold: live it is 88 todo cards at a median of 0.8
                    // days. What justifies a ceiling is DEPTH on a few lanes —
                    // 22 lanes hold a live todo, 4 are over 5 (11, 9, 8, 6).
                    //
                    // The refusal LISTS THE STALEST CARDS FIRST, and that is not
                    // a nicety. `board_drive` already stops dealing any todo
                    // nobody has touched in 7 days, and measured 2026-08-30, 4
                    // of the 72 live agent todo cards were already past that
                    // edge — counted in a pickup trace that only prints when the
                    // queue is otherwise EMPTY, so a lane with one live card
                    // never saw it. Those cards are the answer to "what do I
                    // close first" because they are already not being worked.
                    let wip_limit = if force || target != TaskStatus::Todo {
                        0
                    } else {
                        bs::todo_wip_limit(next.session.as_deref())
                    };
                    // owner_type is checked here and not folded into the count:
                    // a human-owned card is not the dispatcher's to deal, so
                    // capping it would be capping the owner's own queue.
                    let lane = next.session.clone().unwrap_or_default();
                    if wip_limit > 0 && !lane.is_empty() && next.owner_type == "agent" {
                        let held = bs::todo_wip_count(conn, &lane, &next.id);
                        if held >= wip_limit {
                            let stalest = bs::stalest_todos(conn, &lane, 5);
                            tracing::warn!(
                                "todo_wip_gate: blocked {} -> todo for lane {} (holding {}, limit {})",
                                next.id,
                                lane,
                                held,
                                wip_limit
                            );
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    json!({
                                        "error": "todo queue is at its limit for this lane",
                                        "code": "todo_wip_limit_reached",
                                        "ok": false,
                                        "blocked": true,
                                        "item": next.id,
                                        "attempted_status": target_raw,
                                        "session": lane,
                                        "holding": held,
                                        "limit": wip_limit,
                                        "why": format!(
                                            "{lane} already holds {held} todo card(s) and the limit is {wip_limit}. \
                                             `todo` is the dispatch queue: a card here is a \
                                             claim that it is next. `backlog` is unbounded and \
                                             is where a real card that is not NEXT belongs."
                                        ),
                                        // NOT a generic "close something". These are the
                                        // specific cards the dispatcher has already stopped
                                        // dealing, newest-untouched last.
                                        "close_these_first": stalest.iter().map(|(id, title, days)| json!({
                                            "id": id,
                                            "title": title,
                                            "days_since_touched": days,
                                            "already_undispatchable": *days >= 7,
                                        })).collect::<Vec<_>>(),
                                        "how_to_fix": {
                                            "not_next": format!("amux board backlog <ID> --trigger \"<what re-arms it>\" — `backlog` is unbounded on purpose and is where a real card that is not NEXT belongs"),
                                            "not_a_unit_of_work": "amux board discard <ID>",
                                            "finish_one": "amux board done <ID> --evidence-stdin",
                                            "raise_it": "set AMUX_TODO_WIP_LIMIT=<n> in this worker's / group's / global configuration; 0 disables it",
                                            "force": "true (explicit bypass; logged)"
                                        }
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }

                    // AF-317 (b): `blocked` MUST NAME WHAT IT IS WAITING ON.
                    //
                    // Measured 2026-08-30 over LIVE cards (AF-317's 70 counted
                    // archived rows; live it is 32): 31 of the 32 open blocked
                    // cards are older than a week, 16 name a `depends_on` and 19
                    // carry a trigger. A block with no named condition has
                    // nobody watching for the unblock, so it is not blocked, it
                    // is abandoned with a nicer status. This is the one AF-317
                    // statistic that survived re-measurement.
                    let blocked_gate = !force
                        && target == TaskStatus::Blocked
                        && bs::blocked_needs_watch(next.session.as_deref());
                    if blocked_gate {
                        let has_dep = !next.depends_on.is_empty();
                        // `--trigger` lands in source_ref (AMUX-3686); a trigger
                        // there is a condition something re-checks.
                        let has_trigger =
                            next.source_ref.as_deref().is_some_and(|t| t.split_whitespace().count() >= 3);
                        if !has_dep && !has_trigger {
                            tracing::warn!(
                                "blocked_watch_gate: blocked {} -> blocked for session {} (no depends_on, no trigger)",
                                next.id,
                                next.session.as_deref().unwrap_or("-")
                            );
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    json!({
                                        "error": "blocked must name what it is waiting on",
                                        "code": "blocked_needs_a_watch",
                                        "ok": false,
                                        "blocked": true,
                                        "item": next.id,
                                        "attempted_status": target_raw,
                                        "why": "A block with no named condition has nobody watching for the unblock. Measured 2026-08-30 over live cards: 31 of the 32 open blocked cards are older than a week — which is what a status with no exit looks like.",
                                        "how_to_fix": {
                                            "on_another_card": "PATCH {\"depends_on\": [\"<ID>\"]} — the card that has to land first",
                                            "on_a_condition": "amux board backlog <ID> --trigger \"<the condition that re-arms it>\" — re-checked, so the card comes back on its own",
                                            "on_a_person": "amux board needsyou <ID> --ask <type> --question \"...\" --unblocks \"...\" (AF-318)",
                                            "override_for_this_worker": "set AMUX_BLOCKED_NEEDS_WATCH=0 in this worker's / group's / global configuration.",
                                            "force": "true (explicit bypass; logged)"
                                        }
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }

                    // AF-318: `needsyou` MUST NAME THE HUMAN ACT IT IS WAITING ON.
                    //
                    // Measured 2026-08-29: 445 cards parked here, median 15 days,
                    // oldest 58 — and 227 of them (51%) are not human-blocked at
                    // all. Their titles are plain engineering work ("Compute
                    // Utilization Audit", "Fix Namespace Pollution"). The cause is
                    // that `needsyou` is the only status which costs a worker
                    // nothing and stops the idle nudge, so it collects everything a
                    // worker decided to stop doing, and the ~20 real asks become
                    // unfindable inside the rest.
                    //
                    // The gate is on the TRANSITION, never on the 445 already
                    // there: a retroactive sweep would be this same guess made
                    // once more, at scale, by the party least able to check it
                    // A force may bypass a workflow gate with an attributed
                    // audit trail. It must NOT let a linked peer publish a
                    // second completion: overlap ownership is work identity,
                    // not a checklist preference. Migration 0057 repeats this
                    // predicate for alternate writers; this branch supplies an
                    // actionable 409 instead of a raw SQLite error.
                    if matches!(target, TaskStatus::Done | TaskStatus::Verified) {
                        match overlap_completion_blockers(conn, Some(&next.id), None) {
                            Ok(blockers) if !blockers.is_empty() => {
                                let blocker = &blockers[0];
                                tracing::warn!(
                                    marker = "board_overlap_completion_refused",
                                    card = %next.id,
                                    attempted = %bs::db_status_spelling(target),
                                    coordination = %blocker.coordination_id,
                                    owner_card = %blocker.owner_card_id,
                                    owner_session = %blocker.owner_session,
                                    "linked non-owner completion refused before an independent Done/Verified declaration"
                                );
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::CONFLICT,
                                        json!({
                                            "error": "overlap reconciliation blocks independent completion",
                                            "code": "board_overlap_completion_blocked",
                                            "ok": false,
                                            "blocked": true,
                                            "item": next.id,
                                            "attempted_status": bs::db_status_spelling(target),
                                            "overlap": blocker,
                                            "how_to_fix": "Do not force this. Ask the elected owner to record an explicit scope-split or released reconciliation; merged work remains owned by the elected card.",
                                        }),
                                    ),
                                    no_write(),
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(
                                    marker = "board_overlap_completion_unmeasured",
                                    card = %next.id,
                                    error = %e,
                                    "completion refused because overlap ownership could not be measured"
                                );
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::SERVICE_UNAVAILABLE,
                                        json!({
                                            "error": "overlap ownership could not be measured; refusing completion rather than guessing",
                                            "code": "board_overlap_completion_unmeasured",
                                            "measured": false,
                                        }),
                                    ),
                                    no_write(),
                                );
                            }
                        }
                    }
                    // (ethos rule 8). They drain by being re-asked.
                    // THE CONTINUATION GATE, on the same door and by the same
                    // shape (AMUX-3946). A card entering `doing` must say what
                    // the next actor should DO, so that a reader arriving with
                    // no conversation history can act on it.
                    //
                    // Measured in one session, eight cards claimed cold: the two
                    // carrying a reproduction and a stated next step closed in a
                    // single pass; AMUX-3854 reads "make it so this is all
                    // automatic" against a deleted screenshot and cannot be
                    // worked by anyone, including its author.
                    //
                    // `force` bypasses it, exactly as it bypasses the ask gate.
                    // A gate with no truthful escape is one people route around
                    // (ethos rule 3), and every force is already audited.
                    //
                    // ON THE TRANSITION, never retroactively on cards already in
                    // `doing`. Same reasoning AF-318 recorded for the 445: a
                    // retroactive sweep is a guess made at scale by the party
                    // least able to check it.
                    let continuation_required = !force
                        && bs::continuation_applies(target)
                        && bs::continuation_required(next.session.as_deref());
                    if continuation_required {
                        let verdict =
                            bs::continuation_verdict(next.next_action.as_deref().unwrap_or(""));
                        if verdict != bs::ContinuationVerdict::Ok {
                            let (why, code) = match verdict {
                                bs::ContinuationVerdict::Missing => (
                                    "This card does not say what to DO next. Claiming it means someone will arrive here later, possibly you after a compaction, with no conversation history — and `desc` describes the problem, not the next move. One sentence.",
                                    "doing_requires_next_action",
                                ),
                                bs::ContinuationVerdict::NotASentence => (
                                    "`next_action` has to be a sentence a stranger could act on. \"wip\" and \"continue\" name no action; three words is the floor at which somebody has had to think about the reader.",
                                    "doing_next_action_not_a_sentence",
                                ),
                                bs::ContinuationVerdict::Ok => unreachable!(),
                            };
                            // Two-fix rule, same shape as `needsyou_ask_gate`
                            // below: grep `continuation_gate` in server-rs.log,
                            // and the structured `code` splits these out in
                            // /api/logs/analyze. A gate whose refusals are
                            // invisible cannot tell you it is too strict.
                            tracing::warn!(
                                "continuation_gate: blocked {} -> doing for session {} (verdict {:?})",
                                next.id,
                                next.session.as_deref().unwrap_or("-"),
                                verdict
                            );
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::BAD_REQUEST,
                                    json!({
                                        "error": code,
                                        "why": why,
                                        "how": "amux board next <ID> \"<what the next actor should do>\"",
                                        "fields": "next_action is what to do next; last_result is what the previous attempt produced; unresolved is what is still open. Only next_action is gated — a card should not have to invent an open question to be claimable.",
                                        "escape": "amux board doing <ID> --force  (audited, and it is the honest move when the next action genuinely is not knowable yet)",
                                        "scope": "This gate is on `doing` only, and only for lanes that have opted in.",
                                        "override_for_this_worker": "set AMUX_CONTINUATION_REQUIRED=0 in this worker's (or its group's, or the global) configuration.",
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }
                    let ask_required = !force
                        && target == TaskStatus::NeedsYou
                        && bs::needsyou_ask_required(next.session.as_deref());
                    if ask_required {
                        let verdict = bs::ask_verdict(
                            next.ask_actor.as_deref().unwrap_or(""),
                            next.ask_type.as_deref().unwrap_or(""),
                            next.ask_question.as_deref().unwrap_or(""),
                            next.ask_unblocks.as_deref().unwrap_or(""),
                        );
                        if verdict != bs::AskVerdict::Ok {
                            let (why, code) = match verdict {
                                bs::AskVerdict::NoType => (
                                    "This card does not say what KIND of human act it is waiting on. 51% of the cards already parked here are not blocked on a human at all (389 of them, live, median age 15 days) — they are work someone stopped doing, and they are why the real asks go unanswered.",
                                    "needsyou_requires_ask_type",
                                ),
                                bs::AskVerdict::UnknownType => (
                                    "That is not one of the five kinds of human act. The vocabulary is closed on purpose: a block that fits none of them is not a block on a person.",
                                    "needsyou_ask_type_unknown",
                                ),
                                bs::AskVerdict::NoActor => (
                                    "`ask_actor` must name the specific person or external actor who can answer. Generic placeholders are not routable and are how cards disappear into a human-shaped queue.",
                                    "needsyou_requires_specific_actor",
                                ),
                                bs::AskVerdict::NoQuestion => (
                                    "`ask_question` has to be an actual question, in a sentence. \"Blocked on Ethan\" with no question is not an ask — that phrasing is most of what is sitting in this queue today.",
                                    "needsyou_ask_has_no_question",
                                ),
                                bs::AskVerdict::NotAQuestion => (
                                    "`ask_question` must be an actual direct question containing `?`, not a status note or an instruction the worker could continue itself.",
                                    "needsyou_ask_is_not_a_question",
                                ),
                                bs::AskVerdict::NoUnblocks => (
                                    "`ask_unblocks` has to say what ENDS the block, in a sentence. Without it nobody but you can tell whether an answer has landed, so the card cannot leave this queue except by you noticing.",
                                    "needsyou_ask_has_no_exit",
                                ),
                                bs::AskVerdict::Ok => unreachable!(),
                            };
                            // Two-fixes rule: grep `needsyou_ask_gate` in
                            // server-rs.log; the structured `code` splits these
                            // from other 409s in /api/logs/analyze.
                            tracing::warn!(
                                "needsyou_ask_gate: blocked {} -> needsyou for session {} (verdict {:?})",
                                next.id,
                                next.session.as_deref().unwrap_or("-"),
                                verdict
                            );
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    json!({
                                        "error": "needsyou requires a typed ask",
                                        "code": code,
                                        "ok": false,
                                        "blocked": true,
                                        "item": next.id,
                                        "attempted_status": target_raw,
                                        "why": why,
                                        "recorded_ask": {
                                            "ask_type": next.ask_type,
                                            "ask_actor": next.ask_actor,
                                            "ask_question": next.ask_question,
                                            "ask_unblocks": next.ask_unblocks,
                                        },
                                        "ask_types": bs::ASK_TYPE_HELP.iter()
                                            .map(|(k, v)| json!({"type": k, "means": v}))
                                            .collect::<Vec<_>>(),
                                        "how_to_fix": {
                                            "cli": format!("amux board needsyou {} --actor <name> --ask <type> --question \"...?\" --unblocks \"...\"", next.id),
                                            "api": "PATCH /api/board/<id> with {\"ask_actor\":\"named person\",\"ask_type\":\"...\",\"ask_question\":\"...?\",\"ask_unblocks\":\"...\"} — record them before the transition",
                                            "if_it_is_not_human_blocked": "then it is not `needsyou`. Use `backlog --trigger \"<condition>\"` for an external wait that re-arms itself, or leave it in `doing` and work the blocker.",
                                            "override_for_this_worker": "set AMUX_NEEDSYOU_ASK_REQUIRED=0 in this worker's (or its group's, or the global) configuration.",
                                            "force": "true (explicit bypass; logged)"
                                        }
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }

                    // DISCARD-ORPHAN DETECTOR (AMUX-3323). A real-work capture
                    // (the connectors + MDAI epics) was discarded on the decompose
                    // nudge's advice, which abandoned the top-level request and
                    // orphaned its open children. WARN-only, never a block: most
                    // discards are honest (status questions, journals, single-card
                    // dedups). It fires only on the umbrella smell — discarding a
                    // card that still OWNS open epic children, or whose desc points
                    // at 2+ still-open cards — so the next wrongful discard
                    // self-announces in /api/logs/analyze without a human noticing
                    // (two-fixes rule). grep `discard_orphans` in server-rs.log.
                    if target == TaskStatus::Discarded && !force {
                        let is_open = |rid: &str| -> bool {
                            conn.query_row(
                                "SELECT 1 FROM issues WHERE id=?1 AND deleted IS NULL \
                                 AND status NOT IN ('done','verified','discarded','quarantined')",
                                [rid],
                                |_| Ok(()),
                            )
                            .is_ok()
                        };
                        let mut orphans: Vec<String> = Vec::new();
                        // (a) cards whose `epic` points at THIS card and are still open.
                        if let Ok(mut st) = conn.prepare(
                            "SELECT id FROM issues WHERE epic=?1 AND deleted IS NULL \
                             AND status NOT IN ('done','verified','discarded','quarantined') LIMIT 8",
                        ) {
                            if let Ok(rows) = st.query_map([&next.id], |r| r.get::<_, String>(0)) {
                                orphans.extend(rows.flatten());
                            }
                        }
                        // (b) desc points at 2+ distinct still-open cards (the umbrella
                        // pointer). A single open reference is a dedup and stays quiet.
                        let refs: Vec<String> = card_refs(&next.desc)
                            .into_iter()
                            .filter(|r| *r != next.id && is_open(r))
                            .collect();
                        if refs.len() >= 2 {
                            for r in refs {
                                if !orphans.contains(&r) {
                                    orphans.push(r);
                                }
                            }
                        }
                        if !orphans.is_empty() {
                            tracing::warn!(
                                "discard_orphans: {} discarded while it still owns or points at open work {:?} — if this is a real request decomposed into unfinished children, promote it to an epic instead of discarding (AMUX-3323)",
                                next.id,
                                orphans
                            );
                        }
                    }

                    // Gate acknowledgement (AMUX-1719: gate_checked must
                    // MATCH the effective gate — every criterion present).
                    let mut evidence: Vec<Evidence> = Vec::new();
                    let mut ack_via: Option<String> = None;
                    if !eff_gate.is_empty() && !force {
                        let gc = map.get("gate_checked").and_then(Value::as_array).map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(|s| s.trim().to_string())
                                .collect::<Vec<_>>()
                        });
                        if let Some(gc) = &gc {
                            // Exact FIRST, normalized as a fallback (AF-160):
                            // widening only, so no ack that passes today stops.
                            let missing: Vec<&String> = eff_gate
                                .iter()
                                .filter(|c| {
                                    !gc.contains(c)
                                        && !gc.iter().any(|g| ack_norm(g) == ack_norm(c))
                                })
                                .collect();
                            if !missing.is_empty() {
                                // ASK THE SAME QUESTION THE SIBLING SITE ASKS (AF-586).
                                //
                                // This refusal told every caller "the TYPE is wrong, fix the
                                // type" unconditionally, which is only true when the gate came
                                // FROM the type. A worker-, group- or column-scoped gate ignores
                                // the type, so the advice names a lever that cannot move: the
                                // caller retypes, gets the identical refusal, and is left with
                                // --force.
                                //
                                // Reported by tubescience 2026-09-08, four forced closes in one
                                // day on a lane whose WORKER-scope done gate is ["Implemented and
                                // merged", "Tests / lint pass", "Peer reviewed"], applied to
                                // research and ops cards. GateSource::retype_would_help() exists
                                // for exactly this question and the other refusal site already
                                // calls it; this one never did.
                                //
                                // Whether those criteria SHOULD apply to a research card is a
                                // separate decision and still open. This only stops the refusal
                                // sending the reader after a fix that cannot work.
                                //
                                // Bound before the json! because a `match` used as a macro value
                                // breaks its delimiter parsing.
                                let retype_hint = retype_hint_for(gate_src.as_ref());
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::CONFLICT,
                                        json!({
                                            "error": "gate_checked does not match the gate",
                                            "ok": false,
                                            "blocked": true,
                                            "gate": eff_gate,
                                            "missing": missing,
                                            "you_sent": gc,
                                            "attempted_status": target_raw,
                                            "item": row.id,
                                            "item_type": next.item_type,
                                            "how_to_ack": {
                                                "gate_checked": eff_gate,
                                                "or_gate_ack": true,
                                                "or_force": "true (explicit bypass; logged)",
                                                "contract": format!("GET /api/board/contract?card={} (the RESOLVED gate for this card — the bare contract lists only type defaults, AF-112)", next.id),
                                                "wrong_type?": retype_hint,
                                            },
                                        }),
                                    ),
                                    no_write(),
                                );
                            }
                            ack_via = Some(format!("gate_checked ({}/{})", gc.len(), eff_gate.len()));
                        } else if map.get("gate_ack").and_then(Value::as_bool).unwrap_or(false) {
                            // ONE BOOLEAN CANNOT STAND FOR FOUR CLAIMS AT
                            // `verified` (Ethan, 2026-08-29: "i thought our
                            // gates per status were clear, maybe make them
                            // stronger enforced").
                            //
                            // `verified` is the board's highest claim and its
                            // default gate is four INDEPENDENT assertions — CI
                            // green, deployed, confirmed in prod, zero
                            // regressions — that fail in different ways and are
                            // checked by different acts. `gate_ack: true`
                            // asserts all four with a single bit, and nothing
                            // afterwards records which of them the acker
                            // actually looked at. That is the "name which
                            // clause you tested" failure the frustrations rule
                            // names, one status down.
                            //
                            // Measured before shipping, fleet-wide, on every
                            // ack this board has ever recorded:
                            //
                            //   target    gate_checked  gate_ack  ack share
                            //   done              2881       342      10.6%
                            //   verified          1303       302      18.8%
                            //   review             428        31       6.8%
                            //   doing              342        16       4.5%
                            //
                            // So 81% of verifications already enumerate, and
                            // this refuses the other 19% — a real number with a
                            // cheap truthful remedy, not a wall (ethos rule 3).
                            // The remedy is walkable with sanctioned tooling,
                            // checked by reading the CLI rather than assuming:
                            // `amux`'s refusal printer keys on `gate` being in
                            // the error text and echoes `d["gate"]` back as a
                            // ready `--checked "..." "..."` line, so this body
                            // carries both and the operator gets the exact
                            // command. Refusing on a gate whose remedy needed a
                            // hand-rolled PATCH would manufacture the
                            // unattributed writes this system depends on being
                            // attributed (AMUX-2325).
                            //
                            // TWO DELIBERATE NARROWINGS, both so the check
                            // cannot fire where it would only be ceremony:
                            //
                            // 1. `verified` only. `done` carries 10.6% and is
                            //    already machine-gated on an asset link, so it
                            //    has a check a blanket ack cannot fake. Leaving
                            //    it out is a decision, not an oversight.
                            // 2. Multi-criterion gates only. Blanket-acking a
                            //    ONE-criterion gate is byte-identical to
                            //    checking it — the non-code default at
                            //    `verified` is the single "Outcome confirmed to
                            //    still hold", and refusing that would extract a
                            //    retype of the same sentence for no information.
                            if target == TaskStatus::Verified && eff_gate.len() > 1 {
                                tracing::warn!(
                                    "verified_blanket_ack: blocked {} -> verified for session {} ({} criteria acked with one boolean)",
                                    next.id,
                                    next.session.as_deref().unwrap_or("-"),
                                    eff_gate.len()
                                );
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::CONFLICT,
                                        json!({
                                            "error": "verified needs the gate checked criterion by criterion, not a blanket gate_ack",
                                            "code": "verified_requires_gate_checked",
                                            "ok": false,
                                            "blocked": true,
                                            "item": next.id,
                                            "item_type": next.item_type,
                                            "attempted_status": target_raw,
                                            "gate": eff_gate,
                                            "why": format!(
                                                "these {} criteria fail in different ways and are checked by different acts; one boolean asserts all of them and records which you looked at nowhere",
                                                eff_gate.len()
                                            ),
                                            "how_to_fix": {
                                                "cli": format!("amux board verified {} --checked <each criterion>", next.id),
                                                "api": "PATCH with gate_checked: [ ...every criterion... ]",
                                                "if_one_is_not_true": "do not ack it. If the criterion does not fit the work, the TYPE is wrong — fix the type, not the truth.",
                                                "force": "true with a reason (explicit bypass; logged and attributed)",
                                            },
                                        }),
                                    ),
                                    no_write(),
                                );
                            }
                            ack_via = Some("gate_ack".into());
                        }
                        match &ack_via {
                            Some(via) => {
                                evidence = ack_evidence(&actor_name, &eff_gate, via);
                            }
                            None => {
                                let wb = why_blocked(&task, target, &gates, &[]);
                                return finish(
                                    &slot_w,
                                    PatchOut::Refused(
                                        StatusCode::CONFLICT,
                                        gate_409(&next, &eff_gate, &target_raw, &wb, gate_src.as_ref(), &caller_lane),
                                    ),
                                    no_write(),
                                );
                            }
                        }
                    }

                    // A GATE THAT SAYS "NAME THEM" MUST COLLECT THE NAME (AF-160).
                    //
                    // Acking the criterion asserts a peer reviewed it. Nothing
                    // recorded WHO, so the assertion was unfalsifiable and the
                    // field went unset on 91% of the board (148 of 1632 live
                    // verified cards named a peer; 45 of 1381 archived). The
                    // `reviewer` column and `amux board reviewer <id> <who>` /
                    // `--reviewer` already exist — the gate simply never asked
                    // for what it was demanding in prose.
                    //
                    // THE PREDICATE IS reviewer != THE CARD'S OWNER, NOT
                    // reviewer != WHOEVER IS TYPING. The first draft of this
                    // rule (amux-frustrations', corrected by its own author
                    // before it shipped) compared against the ACTING session,
                    // which would have refused both real verifications on this
                    // board within the hour:
                    //
                    //   AF-161  owner=amux              reviewer=amux-frustrations  acting=amux-frustrations
                    //   AF-16   owner=amux-frustrations reviewer=amux               acting=amux
                    //
                    // In both, reviewer == acting — and that is the CORRECT
                    // shape, because criterion 3 says the peer verifies it
                    // THEMSELVES, so the peer signing off IS the one acting.
                    // The two cards are mirror images, and a rule derived from
                    // either alone looks right until the first card pointing
                    // the other way. Copy the predicate from the case that must
                    // PASS, not from the case that must fail.
                    //
                    // Validated before shipping, against every verified card
                    // rather than a constructed fixture: admits 147 of 148 live
                    // (192 of 193 including archived) and refuses exactly one,
                    // AMUX-2409, where owner and reviewer are both
                    // amux-homepage. One refusal, and it is the self-review the
                    // criterion exists to prevent — so the predicate is neither
                    // uniformly permissive nor uniformly strict on real data.
                    //
                    // The escape is walkable with sanctioned tooling ONLY,
                    // checked by walking it (AMUX-2325): `amux board reviewer
                    // AF-66 amux` set it in one call, no raw curl, attribution
                    // intact. Refusing on a gate whose remedy needs a
                    // hand-rolled PATCH would manufacture the unattributed
                    // writes this gate system depends on being attributed.
                    if !force && eff_gate.iter().any(|c| criterion_wants_a_name(c)) {
                        let reviewer = next.reviewer.as_deref().unwrap_or("").trim().to_string();
                        let owner = next.session.as_deref().unwrap_or("").trim().to_string();
                        let bad = if reviewer.is_empty() {
                            Some("no reviewer is recorded on this card")
                        } else if !owner.is_empty() && reviewer.eq_ignore_ascii_case(&owner) {
                            Some("the reviewer is the card's own owner, which is a self-review")
                        } else {
                            None
                        };
                        if let Some(why) = bad {
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    json!({
                                        "error": format!("gate asks you to name the peer, and {why}"),
                                        "ok": false,
                                        "blocked": true,
                                        "item": row.id,
                                        "attempted_status": target_raw,
                                        "criterion": eff_gate.iter().find(|c| criterion_wants_a_name(c)),
                                        "reviewer": next.reviewer,
                                        "owner": next.session,
                                        "why": "acking \"name them\" without a name is an \
                                                unfalsifiable assertion — 91% of verified cards \
                                                carry no peer name at all (AF-160)",
                                        "how_to_fix": {
                                            "cli": format!("amux board reviewer {} <peer-session>", row.id),
                                            "or_in_the_same_call": format!(
                                                "amux board {} {} --reviewer <peer-session> --checked ...",
                                                target_raw, row.id
                                            ),
                                            "rule": "the reviewer must be a DIFFERENT session from \
                                                     the card's owner; the peer doing the sign-off \
                                                     acting on it themselves is correct and expected",
                                        },
                                    }),
                                ),
                                no_write(),
                            );
                        }
                    }

                    // Discharge the gate HERE, with core's OWN predicate
                    // (`why_blocked` is the same function `apply_transition`'s
                    // gate_check runs — the view shares the predicate of the
                    // mechanism). It must happen at this boundary because the
                    // ack protocol is the API's: half the named transitions
                    // (Start, Resume, Queue, ...) carry no evidence slot, so
                    // handing `gates` to `apply_transition` would refuse an
                    // ack that was just verified criterion-by-criterion. The
                    // transition below therefore runs with the gate already
                    // discharged (empty gate slice), evidence recorded in the
                    // card log. `force` skips this check but never the audit.
                    if !force {
                        let wb = why_blocked(&task, target, &gates, &evidence);
                        if !wb.is_empty() {
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    gate_409(&next, &eff_gate, &target_raw, &wb, gate_src.as_ref(), &caller_lane),
                                ),
                                no_write(),
                            );
                        }
                    }

                    let now = chrono::Utc::now();
                    let tx = if force {
                        BoardTransition::Force {
                            status: target,
                            reason: reason.clone(),
                        }
                    } else {
                        // No named transition (e.g. todo->done, which the
                        // Python board serves constantly) applies through
                        // core as an attributed direct set — the gate was
                        // discharged above, one code path for the write.
                        named_transition(from, target, evidence.clone(), reason.clone())
                            .unwrap_or_else(|| BoardTransition::Force {
                                status: target,
                                reason: format!(
                                    "direct status set via PATCH (no named {} -> {} transition)",
                                    bs::db_status_spelling(from),
                                    bs::db_status_spelling(target)
                                ),
                            })
                    };

                    match apply_transition(&task, tx, &actor, &[], now) {
                        Ok(updated) => {
                            let from_raw = next.status.clone();
                            let stamp = hhmm();
                            if let Some(via) = &ack_via {
                                next.log = Some(bs::append_log(
                                    next.log.as_deref(),
                                    &stamp,
                                    &format!(
                                        "{actor_name}: gate satisfied via {via} for {target_raw}"
                                    ),
                                ));
                            }
                            let line = if force {
                                // The audited bypass (ethos rule 6): the force
                                // MUST leave a trace, on the card itself.
                                format!(
                                    "force by {force_actor}: {from_raw}->{target_raw} reason={reason}"
                                )
                            } else {
                                format!("{actor_name}: {from_raw} -> {target_raw}")
                            };
                            next.log = Some(bs::append_log(next.log.as_deref(), &stamp, &line));
                            // AMUX-3607, Ethan's 2026-08-05 directive: every
                            // action a worker takes is logged WITH the
                            // permission scope that allowed it, each layer
                            // individually. Written on EVERY transition,
                            // including the ones no layer gated — "nothing
                            // required this, at any tier" is an authorisation
                            // answer, not an absence of one, and a trail that
                            // only appears when something blocked would make
                            // the permissive case the invisible case.
                            next.log =
                                Some(bs::append_log(next.log.as_deref(), &stamp, &authz_line));
                            // Gap 4: waiting_on side effects before status change.
                            crate::db::advance::apply_status_side_effects(&mut next, &target_raw);
                            next.status = target_raw.clone();
                            next.version = i64::try_from(updated.version).unwrap_or(next.version + 1);

                            // REVISIT DATE ON THE TWO STATUSES NOTHING DRAINS
                            // (Ethan, 2026-08-29: "some workers have an
                            // infinite # of growing backlogs and todo then they
                            // go idle"). `backlog` and `needsyou` are the only
                            // statuses with no gate AND no exit an automated
                            // loop can produce, and they held 963 of the 1029
                            // open cards. A card entering either without a date
                            // is a decision nobody made; see
                            // `bs::default_revisit_days` for the measurement
                            // and for why this is a default rather than a gate.
                            //
                            // A caller-supplied `due` in this same PATCH is
                            // already in `next.due` (`set_opt` runs well above
                            // this block), so this only ever fills a blank —
                            // it cannot overwrite a date someone chose.
                            if next.due.as_deref().unwrap_or("").trim().is_empty() {
                                if let Some(days) =
                                    bs::default_revisit_days(target, next.session.as_deref())
                                {
                                    let when = bs::revisit_date(days);
                                    next.log = Some(bs::append_log(
                                        next.log.as_deref(),
                                        &stamp,
                                        &format!(
                                            "revisit {when} (default {days}d for {target_raw}): a \
                                             {target_raw} card with no revisit date is one nobody \
                                             looks at again — change or clear it if that is wrong"
                                        ),
                                    ));
                                    next.due = Some(when);
                                    changed.push("due".into());
                                }
                            }
                            status_event = Some((from_raw, target_raw));
                            changed.push("status".into());
                        }
                        Err(TransitionError::NoOp) if rechecking_verified && !force => {
                            next.log = Some(bs::append_log(next.log.as_deref(), &hhmm(),
                                &format!("{actor_name}: verified gate rechecked; {}", authz_line)));
                            next.last_verified_at = Some(now_secs());
                            changed.push("last_verified_at".into());
                        }
                        Err(TransitionError::NoOp) => { /* nothing to do */ }
                        Err(TransitionError::GateBlocked { blocked }) => {
                            return finish(
                                &slot_w,
                                PatchOut::Refused(
                                    StatusCode::CONFLICT,
                                    gate_409(&next, &eff_gate, &target_raw, &blocked, gate_src.as_ref(), &caller_lane),
                                ),
                                no_write(),
                            );
                        }
                        Err(e) => {
                            // InvalidTransition / NotArmable / Archived...:
                            // the serialized core error IS the body, plus the
                            // Python-style flags so no reader mistakes a
                            // refusal for success.
                            let mut body = serde_json::to_value(&e)
                                .unwrap_or_else(|_| json!({"kind": "transition_error"}));
                            body["error"] = json!(e.to_string());
                            body["ok"] = json!(false);
                            body["blocked"] = json!(true);
                            body["attempted_status"] = json!(target_raw);
                            body["item"] = json!(row.id);
                            return finish(
                                &slot_w,
                                PatchOut::Refused(StatusCode::CONFLICT, body),
                                no_write(),
                            );
                        }
                    }
                }
            }

            if changed.is_empty() {
                // Invariant 37: nothing changed -> applied:false, rev/version
                // untouched, unknown keys named.
                return finish(
                    &slot_w,
                    PatchOut::Noop {
                        body: detail_body(&row),
                        ignored,
                        all_ignored,
                    },
                    no_write(),
                );
            }

            // THE CARD IS THE SOURCE OF TRUTH FOR ITS OWN HISTORY (Ethan,
            // 2026-08-10: "make sure that board tasks maintain as the source of
            // truth (updates, history, changes, etc.) all go into that board
            // task as history — this should be amux wide").
            //
            // Only `status` was ever logged. The other seventeen writable fields
            // changed SILENTLY: a card could be retyped, reassigned, re-scoped,
            // un-archived, have its gate rewritten or its whole description
            // replaced, and the card itself would carry no trace of any of it.
            // The rev counter moved, which tells you SOMETHING happened and
            // nothing about what — and rev is not on the card a human reads.
            //
            // So every accepted change now leaves a line naming the actor and
            // the fields. Deliberately ONE line per PATCH rather than one per
            // field: a PATCH is the atomic unit a caller performed, and
            // splitting it would make an ordinary two-field edit read like two
            // separate decisions.
            //
            // VALUES ARE SUMMARISED, NOT COPIED. `desc` can be thousands of
            // characters and this log is read in a UI panel; a history that
            // reproduces every description in full stops being readable, which
            // is the ethos-5 failure (at volume it becomes a log nobody reads).
            // Short scalars are shown because for `type`, `session` and
            // `reviewer` the VALUE is the decision.
            {
                let noisy: std::collections::HashSet<&str> =
                    ["status", "pos", "last_verified_at"].into_iter().collect();
                let mut parts: Vec<String> = Vec::new();
                for f in changed.iter().filter(|f| !noisy.contains(f.as_str())) {
                    let part = match f.as_str() {
                        // The two free-text fields: report the SHAPE of the
                        // edit, since the new value is already on the card and
                        // the useful fact is that it moved and by how much.
                        "desc" => {
                            let before = row.desc.chars().count() as i64;
                            let after = next.desc.chars().count() as i64;
                            let delta = after - before;
                            if delta > 0 {
                                format!("desc +{delta} chars")
                            } else if delta < 0 {
                                format!("desc {delta} chars")
                            } else {
                                "desc rewritten".to_string()
                            }
                        }
                        "title" => format!("title -> {}", chars_truncate_log(&next.title, 60)),
                        "type" => format!("type -> {}", next.item_type),
                        "session" => format!(
                            "session -> {}",
                            next.session.as_deref().unwrap_or("(unassigned)")
                        ),
                        "reviewer" => format!(
                            "reviewer -> {}",
                            next.reviewer.as_deref().unwrap_or("(none)")
                        ),
                        "owner_type" => format!("owner_type -> {}", next.owner_type),
                        // THE ONE FIELD WHERE THE LOG NAMES WHAT WAS DESTROYED
                        // RATHER THAN WHAT ARRIVED (AF-459).
                        //
                        // The rule three comments up — "VALUES ARE SUMMARISED,
                        // NOT COPIED ... the new value is already on the card" —
                        // is right for every other field and inverts for this
                        // one. `--trigger` is a plain overwrite; only an
                        // `autofix:` prefix is protected (AMUX-3686 above), and
                        // that narrowness is deliberate. So when a trigger
                        // replaces a trigger, the OLD value exists nowhere: not
                        // on the card, not in /api/history, not in this log,
                        // which recorded the bare word "source_ref". There is no
                        // redundancy to trade against readability here, because
                        // the column that just got written WAS the only copy.
                        //
                        // gtm-engine lost a five-item inventory from 2026-08-09
                        // this way, probing whether --trigger works on an
                        // archived card (it does). They recovered four items
                        // from a prefix they had happened to print earlier in
                        // their own transcript. The fifth is gone. Second known
                        // clobber of this field on that board.
                        //
                        // The old value is kept WHOLE where the arriving one is
                        // truncated at 60, and the asymmetry is the point: the
                        // arriving value is ON THE CARD, and this line is the
                        // only copy of the one being destroyed.
                        //
                        // It said "kept LONG (200)" and named the hazard it was
                        // still committing (AF-459). gtm-engine refused to
                        // validate it and measured the boundary: 88 and 158
                        // chars survive, 208 and beyond are head-truncated to
                        // 201, and their real loss was 366. A prefix cap fails
                        // for some value whatever the number; see
                        // chars_elide_middle for why the middle goes instead.
                        "source_ref" => {
                            let before = row.source_ref.as_deref().unwrap_or("");
                            let after = next.source_ref.as_deref().unwrap_or("(cleared)");
                            if before.is_empty() {
                                format!("source_ref -> {}", chars_truncate_log(after, 60))
                            } else {
                                format!(
                                    "source_ref: WAS {} -> {}",
                                    chars_elide_middle(
                                        before,
                                        SOURCE_REF_LOG_HEAD,
                                        SOURCE_REF_LOG_TAIL
                                    ),
                                    chars_truncate_log(after, 60)
                                )
                            }
                        }
                        "archived" => {
                            // ARCHIVING A TRIGGER-BEARING CARD DE-ARMS IT, SILENTLY
                            // (AMUX-3715, reported by tubescience). `archived`
                            // hides a card from every board view AND every
                            // autonomy loop — advance, pickup, rot — so a
                            // `--trigger` recorded in `source_ref` will never
                            // fire again. The card looks parked-on-a-condition
                            // and is actually parked forever.
                            //
                            // Measured when they reported it: 202 archived cards
                            // fleet-wide carry a non-empty source_ref, across at
                            // least six lanes (tubescience 77, mvs-infra 22,
                            // amux 9). Not one of them will fire.
                            //
                            // This is ethos rule 1's exemption lesson again, on
                            // a different field: when you exempt something from
                            // a loop, name what still reaches it — and if the
                            // answer is nothing, the exemption did not make it
                            // cheap, it made it invisible. Same shape as the
                            // armed watches that were findable only by
                            // scrolling past them.
                            //
                            // RECORDED, NOT REFUSED. tubescience archives
                            // trigger-bearing cards deliberately and keeps the
                            // conditions in committed docs, so refusing would
                            // break a working flow to protect them from a
                            // choice they are making on purpose (ethos rule 8).
                            // The log line is what a future lane needs, since it
                            // outlives the response nobody kept.
                            if next.archived == 1 {
                                match next.source_ref.as_deref().map(str::trim) {
                                    Some(t) if !t.is_empty() => format!(
                                        "ARCHIVED — and this card carried a live trigger, which                                          archiving DE-ARMS: archived cards are excluded from                                          every autonomy loop, so it will not fire. Trigger was:                                          {t}"
                                    ),
                                    _ => "ARCHIVED".into(),
                                }
                            } else {
                                "restored".into()
                            }
                        }
                        "pinned" => {
                            if next.pinned == 1 { "pinned".into() } else { "unpinned".into() }
                        }
                        other => other.to_string(),
                    };
                    parts.push(part);
                }
                if !parts.is_empty() {
                    next.log = Some(bs::append_log(
                        next.log.as_deref(),
                        &hhmm(),
                        &format!("{actor_name}: {}", parts.join(", ")),
                    ));
                }
            }

            // Writes bump rev (the Python counter) AND version (the Rust one).
            next.rev = row.rev + 1;
            if !changed.contains(&"status".to_string()) {
                next.version = row.version + 1;
            }
            next.updated = now_secs();
            bs::save_patched(conn, &mut next)?;
            if next.status == "verified" && (row.status != "verified" || map.get("reverify").and_then(Value::as_bool) == Some(true)) && !map.get("force").and_then(Value::as_bool).unwrap_or(false) {
                let groups = next.session.as_deref().map(crate::api::session_verbs::lane_groups).unwrap_or_default();
                let trail = bs::effective_gate_trail(conn, &next, TaskStatus::Verified, &groups);
                // Persist exactly the gate that passed normal transition validation.
                // This is an attributed acknowledgement, never a fabricated harness run.
                crate::db::verification_store::insert(conn, &crate::db::verification_store::VerificationRow {
                    id: format!("VER-{}", ulid::Ulid::new().to_string().to_lowercase()),
                    task_id: next.id.clone(),
                    verifier: json!({"type":"gate_acknowledgement","source":trail.source.token(),"scope":trail.source.scope()}).to_string(),
                    criteria: json!(trail.criteria).to_string(),
                    evidence: json!({"evidence":next.evidence,"reviewer":next.reviewer}).to_string(),
                    verdict: "acknowledged".into(), reason: None, run_detail: None,
                    actor: actor_name.clone(), created_at: now_secs(), criteria_version: 0,
                    harness_version: None, duration_ms: 0,
                })?;
                tracing::info!(target: "amux::verification", card = %next.id, actor = %actor_name,
                    criteria_count = trail.criteria.len(), "verified gate acknowledgement recorded");
            }
            if let Some((from, to)) = &status_event {
                if to == "doing" {
                    ensure_owner_doing_claim(
                        conn,
                        &next,
                        &actor_name,
                        "board-patch",
                        from,
                    )?;
                }
            }
            // AF-137 / AMUX-3464: retiring an auto-filed REPORT re-arms its
            // detector. The filing dedupe is a PERMANENT session_events idem
            // row ("a restart must not refile"), so a discarded report whose
            // idem survives would suppress every future recurrence of the same
            // signature — the signal dies with the card. Deleting the row on
            // the discard transition makes recurrence file a FRESH card (which
            // now reaches a lane via AMUX_AUTOFIX_SESSION). Doctrine intact:
            // nothing here closes on green — this fires only when a WORKER
            // discards, and makes that judgment signal-safe. source_ref and
            // the idem key are the same "autofix:<signature>" string.
            if next.status == "discarded" && row.status != "discarded" {
                if let Some(sr) = next
                    .source_ref
                    .as_deref()
                    .filter(|s| s.starts_with("autofix:"))
                    // OCCURRENCE-class reports are judged forever (AMUX-3472):
                    // an outlier signature names specific requests (it carries
                    // the newest row's ts), and re-arming it refiled the SAME
                    // specimen while it sat inside the detector's window — the
                    // identical card back with zero new information. A new
                    // occurrence mints a new signature and files regardless of
                    // this idem, so keeping it loses nothing. CONDITION-class
                    // signatures (invariant streaks, p95-vs-norm, CI) keep the
                    // re-arm: their refile requires the condition to be live
                    // again, which is exactly the signal.
                    // AMUX-3591 adds `5xx|` for the same reason. Its signature
                    // now carries occurrence identity too, so re-arming it
                    // refiled the SAME rows while they sat inside the window:
                    // one hang filed AMUX-3581, discarding it refiled
                    // AMUX-3589, discarding that refiled AMUX-3591 — identical
                    // signature, zero new information, a lane-turn each round.
                    // A NEW 5xx mints a new signature and files regardless of
                    // this idem, so keeping it loses nothing.
                    // AMUX-3633 adds `invariant|` on the SAME reasoning, and it
                    // makes the sentence above literally true rather than nearly
                    // so. "Their refile requires the condition to be live AGAIN"
                    // was the intent; what the code did was refile while it was
                    // merely STILL live. For a long-running known breach those
                    // are different: the ledger invariant shipped at 14573a02
                    // filed AMUX-3631, a worker discarded it as owned work, and
                    // the re-arm refiled AMUX-3633 within the hour at 20
                    // evaluations — byte-identical, zero new information, one
                    // lane-turn per round. The same loop 5xx had.
                    //
                    // The signature now carries the incident's `first_seen`, so
                    // a recovery-then-refail opens a new `_amux_invariant_incident`
                    // row, mints a NEW signature and files regardless of this
                    // idem. "Live again" keeps its signal; "still live" stops
                    // costing a turn.
                    .filter(|s| {
                        !s.starts_with("autofix:latency|outlier|")
                            && !s.starts_with("autofix:5xx|")
                            && !s.starts_with("autofix:invariant|")
                    })
                {
                    let n = conn.execute(
                        "DELETE FROM session_events WHERE idem = ?1",
                        rusqlite::params![sr],
                    )?;
                    if n > 0 {
                        tracing::info!(
                            target: "autofix", card = %next.id, signature = %sr,
                            "report discarded — detector RE-ARMED (idem cleared); \
                             recurrence files fresh"
                        );
                    }
                }
            }
            if let Some(tags) = &tags_change {
                bs::set_tags(conn, &next.id, tags, next.updated)?;
            }
            // KEEP THE `needsyou` STATUS AND THE `needs:you` TAG IN STEP.
            //
            // They are two spellings of one fact and the readers are split
            // across them: the status is what EXCLUDES a card (auto-pickup
            // takes `status='todo'`, the advance path takes `status IN
            // ('doing','review')`), while every mechanism that SURFACES a
            // human-blocked card keys on the TAG — the dashboard's
            // `is:needsyou` view and Focus mode, the 3-day re-nag (which
            // JOINs issue_tags), and board_drive's "the human owes the
            // answer, not the lane" branch.
            //
            // So setting the canonical status alone parked the card where
            // nothing hands it out AND nothing brings it back — strictly
            // worse than leaving it in `todo`, and reached by taking the
            // DOCUMENTED transition (core: `Doing -> NeedsYou`, "stuck on the
            // user, with the exact question"). Measured on the live board
            // 2026-08-11: 23 of 38 open `needsyou` cards carried no tag,
            // across six sessions, including four SLA breaches aged 127-194h
            // that the re-nag structurally could not see.
            //
            // Syncing here rather than teaching each reader both spellings is
            // deliberate: one write fixes five consumers, and there is no
            // second predicate left to drift.
            if let Some((from_raw, to_raw)) = &status_event {
                let was = bs::parse_status(from_raw);
                let now_st = bs::parse_status(to_raw);
                let is_ny = |s: Option<TaskStatus>| s == Some(TaskStatus::NeedsYou);
                // An explicit `tags` in the same PATCH is the caller stating
                // intent; it wins over this sync either way.
                let caller_set_ny = tags_change.as_ref().is_some_and(|t| {
                    t.iter().any(|x| x.to_lowercase().starts_with("needs:you"))
                });
                if is_ny(now_st) && !is_ny(was) {
                    bs::add_needs_you_tag(conn, &next.id, next.updated)?;
                } else if is_ny(was) && !is_ny(now_st) && !caller_set_ny {
                    // The answer landed (`NeedsYou -> Doing` is core's
                    // "the user answered"). Leaving the tag on would re-nag
                    // the lane about a question that is no longer open —
                    // the re-nag only skips done/verified/discarded.
                    bs::clear_needs_you_tags(conn, &next.id)?;
                }
            }
            let mutation = match &status_event {
                Some((f, t)) => MutationKind::StatusChanged {
                    from: f.clone(),
                    to: t.clone(),
                },
                None => MutationKind::Updated,
            };
            let event = ev_snap(&next, mutation);
            let st = status_event.as_ref().map(|(f, t)| {
                (
                    next.session.clone().unwrap_or_default(),
                    f.clone(),
                    t.clone(),
                )
            });
            // AVE-36: a non-owner's append earns the owner a notice. Self-notes
            // and unattributed callers notify nobody (the automation that
            // appends server-side carries no session header on purpose).
            // WHO ACTUALLY NEEDS THIS NOTE (AMUX-3771, two fresh instances from
            // backend).
            //
            // This targeted the OWNER and only the owner, so a card in `review`
            // with a cross-group reviewer notified nobody: the owner posting a
            // review request IS the caller, `owner != caller_lane` is false, and
            // the reviewer was never considered. BACKE-3467 sat in `review`
            // reading healthy while waiting on no one, and was only caught when a
            // human pointed it out. The workaround was to reassign card
            // OWNERSHIP to the reviewer, which conflates the two roles.
            //
            // A reviewer is the party the note is FOR when a card is in review.
            // Notifying them is not a second feature; it is the first one
            // addressed correctly.
            let appended_note_for_reviewer = appended_note.clone();
            let reviewer_target = next
                .reviewer
                .clone()
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty() && *r != caller_lane)
                .filter(|r| Some(r.as_str()) != next.session.as_deref())
                .filter(|_| bs::parse_status(&next.status) == Some(TaskStatus::Review));
            let progress_notify = appended_note.and_then(|note| {
                let owner = next.session.clone().unwrap_or_default();
                (!owner.is_empty() && !caller_lane.is_empty() && owner != caller_lane)
                    .then(|| (owner, next.title.clone(), note))
            });
            // A NOTE ON AN ARCHIVED CARD REACHES NOBODY, AND THE WRITE SUCCEEDS
            // (ts-gke, 2026-08-31).
            //
            // Archiving hides a card from every board view AND every autonomy
            // loop. Appending to one still returns 200, so the sender gets a
            // success with no signal that the card is invisible. That is the
            // failure this whole family is about: recorded, reported as
            // delivered, reaching nobody.
            //
            // Measured by ts-gke across the board: 1,334 ARCHIVED cards were
            // updated in the last 3 days and 1,199 archived cards are
            // amux-owned, so this is happening at volume rather than as an edge
            // case. Three of my own cards were hit by it last night --
            // AMUX-3771, AMUX-3119 and AMUX-3780 all took peer notes while
            // archived by a board sweep, and two of them carried live findings I
            // only saw because the peer chased me.
            //
            // WARN, NOT REFUSE. The note itself is worth keeping: it is the
            // record, and refusing the write would lose content to protect a
            // notification. Their framing, and it is the right trade: turn a
            // SILENT loss into a VISIBLE one.
            let mut body = detail_body(&next);
            if appended_note_for_reviewer.is_some() && next.archived == 1 {
                body["note_reaches_nobody"] = json!(true);
                body["archived_warning"] = json!(
                    "this card is ARCHIVED: the note is saved, but the card is hidden from \
                     every board view and every autonomy loop, so nobody will see it. \
                     `amux board unarchive <id>` if the note needs an audience."
                );
                tracing::warn!(
                    note_on_archived_card = %next.id,
                    owner = %next.session.as_deref().unwrap_or("-"),
                    caller = %caller_lane,
                    "board note appended to an ARCHIVED card — write succeeded, nobody will see it"
                );
            }
            finish(
                &slot_w,
                PatchOut::Applied {
                    body,
                    ignored,
                    diverted,
                    advisories,
                    status_transition: st,
                    progress_notify,
                    // FIRE ON THE TRANSITION INTO REVIEW, not only on a note
                    // append (AMUX-3771, second pass, found by backend).
                    //
                    // The first version was `.zip(appended_note)`, so it fired
                    // ONLY when somebody appended prose to a card already in
                    // review. The natural act is the opposite one: set a
                    // reviewer and MOVE the card to review, which appends no
                    // note. backend re-ran their BACKE-3467 shape both ways --
                    // raw PATCH {status:review} and `amux board review` -- and
                    // my code emitted nothing at all: not the notify, not the
                    // refusal, not the report. I had fixed the rarer trigger.
                    //
                    // Entering review is the request. A note on an
                    // already-in-review card is a follow-up, and both should
                    // reach the reviewer.
                    reviewer_notify: reviewer_target.clone().and_then(|r| {
                        let entering_review = status_event
                            .as_ref()
                            .is_some_and(|(f, t)| f != t && t == "review");
                        match (appended_note_for_reviewer.clone(), entering_review) {
                            (Some(note), _) => Some(Box::new((r, next.title.clone(), note))),
                            (None, true) => Some(Box::new((
                                r,
                                next.title.clone(),
                                String::new(),
                            ))),
                            (None, false) => None,
                        }
                    }),
                },
                WriteOutcome {
                    applied: true,
                    events: vec![event],
                },
            )
        })
        .await;

    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("patch produced no outcome"),
        Some(PatchOut::NotFound) => not_found(&id),
        Some(PatchOut::Refused(status, mut body)) => {
            // AF-413: say what the refusal threw away, at the ONE place every
            // refusal converges. There are 16 sites that build a refusal body;
            // decorating each would cover today's and miss the next one, which
            // is the rule-1 shape this card is an instance of.
            //
            // ALWAYS EMITTED, INCLUDING EMPTY. `discarded: []` means "nothing
            // beyond the status was lost"; the key's ABSENCE would mean a server
            // that does not compute this at all, and a caller cannot tell those
            // apart if it is omitted when empty (ethos rule 4).
            let dropped = discarded_on_refusal;
            if dropped.iter().any(|key| key == "archive_outcome") {
                tracing::warn!(target: "amux::board", verdict="archive_outcome_refused", item=%id,
                    measured=true, n_considered=1, "archive outcome was not applied; the entire PATCH was refused");
            }
            if !dropped.is_empty() {
                body["discarded_note"] = json!(format!(
                    "the transition was refused, so the WHOLE body was discarded — \
                     {} was NOT applied and must be re-sent, separately from the status",
                    dropped.join(", ")
                ));
            }
            body["discarded"] = json!(dropped);
            err(status, body)
        }
        Some(PatchOut::Noop { mut body, ignored, all_ignored }) => {
            body["applied"] = json!(false);
            if !ignored.is_empty() {
                body["ignored_fields"] = json!(ignored);
                body["ignored_note"] = json!(
                    "these keys are not writable via PATCH and were NOT applied; \
                     the rest of this response reflects the card as stored"
                );
                // NAME THE FIELD THEY MEANT (AF-476). Telling a caller a key is
                // unwritable answers "why did nothing happen" and leaves "what
                // should I have sent" to guesswork. For keys that are CLI FLAG
                // names rather than column names, the answer is exact and cheap.
                //
                // `trigger` is the whole reason this exists. It is the flag
                // `amux board <status> <id> --trigger "..."`, which writes
                // source_ref AND stamps last_verified_at — so a raw PATCH of
                // {"trigger": ...} writes nothing at all. Measured by the
                // 2026-09-04 log sweep: 226 such PATCHes from `backend` in 80
                // seconds across ~220 distinct cards, every one a 422 that could
                // not have done anything. I had made the identical mistake myself
                // earlier the same day, which is what made it recognisable.
                //
                // Related to AF-469 but not the same: there the caller sent the
                // right column and missed its companion, so the write LANDED and
                // the card re-drained forever. Here the write is a complete
                // no-op. Same root — the CLI flag and the API field have
                // different names, and only one path stamps both.
                let hints: Vec<Value> = ignored
                    .iter()
                    .filter_map(|k| match k.as_str() {
                        "trigger" => Some(json!({
                            "sent": "trigger",
                            "meant": ["source_ref", "last_verified_at"],
                            "how": "`amux board <status> <id> --trigger \"...\"` writes both. A raw PATCH must send source_ref AND last_verified_at (unix seconds) itself, or the card re-drains forever (AF-469).",
                        })),
                        _ => None,
                    })
                    .collect();
                if !hints.is_empty() {
                    body["ignored_hints"] = json!(hints);
                }
            }
            // 422 WHEN NOTHING YOU SENT WAS WRITABLE.
            //
            // Reported by tsukimiya reviewing #134: "a PATCH of item_type returns
            // 200 with a bumped rev and silently does nothing — which cost you six
            // mistyped cards in one night". The rev-bump and the silence are
            // already fixed (`applied:false`, rev unmoved, the key named in
            // `ignored_fields`), and the body has been honest for a while. The
            // STATUS CODE was not: a caller checking `r.ok` or `resp.status`
            // still read success, which is AC-227's trap exactly — `d.get('ok',
            // True)` defaulting True is how a refused write got reported as done.
            //
            // Scoped to all_ignored rather than to every no-op, because those are
            // different facts. A writable field set to its current value is a
            // SUCCESSFUL request that changed nothing, and 422 would be a lie in
            // the other direction — plus it would break every caller that PATCHes
            // idempotently. Only "no key you sent can be written" is the caller's
            // mistake, and only that answers 422.
            let code = if all_ignored { StatusCode::UNPROCESSABLE_ENTITY } else { StatusCode::OK };
            (code, Json(body)).into_response()
        }
        Some(PatchOut::Applied {
            mut body,
            ignored,
            diverted,
            advisories,
            status_transition,
            progress_notify,
            reviewer_notify,
        }) => {
            body["applied"] = json!(true);
            if !advisories.is_empty() {
                body["advisories"] = json!(advisories);
            }
            body["global_rev"] = json!(reply.rev.0);
            // SEPARATE KEY FROM `ignored_fields`, because they are opposite
            // facts and a caller acts differently on each: ignored means set it
            // somewhere else, diverted means it is already set, stop looking in
            // the field you named (AMUX-3791).
            if !diverted.is_empty() {
                body["diverted_fields"] = json!(diverted);
            }
            if !ignored.is_empty() {
                body["ignored_fields"] = json!(ignored);
                body["ignored_note"] = json!(
                    "these keys are not writable via PATCH and were NOT applied; \
                     the rest of this response reflects the card as stored"
                );
            }
            // AVE-36: the note landed; now say honestly whether the OWNER was
            // told. "progress noted" with nobody notified is how three confirms
            // in a row went unread on a card its owner was actively working.
            if let Some((owner, title, note)) = progress_notify {
                if !crate::api::session_verbs::is_running(&owner).await {
                    body["owner_notified"] = json!(false);
                    body["owner_notify_reason"] = json!(format!(
                        "owner session '{owner}' is not running — the note is on the card but \
                         nobody was told; re-run `amux board ask {id}` when they are up if it \
                         needs their attention"
                    ));
                // THE NOTE'S CONTENT IS PART OF THE KEY (AMUX-3935).
            //
            // The key was (owner, card, author) with a 10-minute window, which
            // collapses "the same note twice" and "a second, DIFFERENT note
            // about the same card" into one case. A review conversation is
            // necessarily the second kind: context first, then the verdict that
            // rests on it — so the later message is systematically the
            // higher-value one, and it is the one that was dropped.
            //
            // Two instances on 2026-08-30, both from mixpeek-homepage-claude,
            // both with that ordering. The first dropped a review verdict
            // carrying a BLOCKING condition on how AMUX-3920 should close; it
            // reached me only because they appended a pointer to a third card.
            // The second dropped a verification result on AMUX-3933 — and the
            // note it suppressed was their close-out on THIS defect, which is
            // as self-demonstrating as it gets.
            //
            // Flood protection is preserved exactly: a burst of IDENTICAL
            // appends still collapses to one notice. What no longer collapses is
            // a note that says something new.
            } else if !progress_notify_once(&format!(
                "{owner}|{id}|{caller_for_notify}|{:x}",
                {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    note.hash(&mut h);
                    h.finish()
                }
            )) {
                    body["owner_notified"] = json!(false);
                    body["owner_notify_reason"] = json!(
                        "an IDENTICAL note from you on this card was already delivered in the \
                         last 10 minutes (deduped); the note itself is saved. A note with \
                         different text always delivers (AMUX-3935)."
                    );
                } else {
                    let prompt = format!(
                        // "[board note on ...]", NOT "[amux board note on ...]".
                        // There is no `note` verb; the old header read as a
                        // command a lane could run, in the one position — the
                        // first bracket of a delivered message — where an agent
                        // is most likely to treat text as an instruction.
                        // AMUX-3707's sweep of every `amux board <verb>` the
                        // server emits is what surfaced it.
                        "[board note on {id}: {}] {caller_for_notify} appended a progress \
                         note to YOUR card:\n{note}\n(Full note is on the card {id}. This notice \
                         is delivery of a peer's note, not a status request.)",
                        title.chars().take(60).collect::<String>()
                    );
                    // REPORT WHAT THE ENQUEUE ACTUALLY DID (AMUX-3938). This
                    // was `let _ =` followed by an unconditional
                    // `owner_notified: true`, so the sender was told the owner
                    // had been notified even when the chokepoint REFUSED — an
                    // isolated lane, an archived lane, a permanently blocked
                    // target. That is the same sender/recipient disagreement
                    // this card family is about: from the sender it looked
                    // delivered, from the owner it never arrived, and neither
                    // side could see the mismatch. `steer_enqueue` is
                    // `#[must_use]` precisely so this is a decision, not an
                    // oversight; the honest handling is to pass the refusal on.
                    match crate::api::session_verbs::steer_enqueue(
                        &state,
                        &owner,
                        &prompt,
                        "board-progress",
                        &caller_for_notify,
                    )
                    .await
                    {
                        Ok(_) => {
                            body["owner_notified"] = json!(true);
                            body["owner_notify_note"] = json!(format!(
                                "{owner} will see the note at their next turn boundary"
                            ));
                        }
                        Err(reason) => {
                            body["owner_notified"] = json!(false);
                            body["owner_notify_reason"] = json!(format!(
                                "the note is saved on the card, but {owner} could NOT be \
                                 notified: {reason}"
                            ));
                            tracing::warn!(
                                board_note_undelivered = %owner,
                                card = %id,
                                sender = %caller_for_notify,
                                reason = reason,
                                "board note saved but owner not notified"
                            );
                        }
                    }
                }
            }
            // DELIVER TO THE REVIEWER (AMUX-3771). A card in `review` names the
            // party the note is for; notifying only the owner is how BACKE-3467
            // sat reading healthy while waiting on nobody.
            //
            // REACHABILITY IS CHECKED AND REPORTED, never silently skipped. The
            // whole defect class here is a request that looks delivered from one
            // side and never arrived on the other, so a refusal that produced
            // silence would be the same bug wearing a fix.
            if let Some(boxed) = reviewer_notify {
                let (reviewer, title, note) = *boxed;
                let owner_for_check = body
                    .get("session")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                match crate::api::session_verbs::reviewer_unreachable_reason(
                    &owner_for_check,
                    &reviewer,
                ) {
                    Some(why) => {
                        body["reviewer_notified"] = json!(false);
                        body["reviewer_notify_reason"] = json!(format!(
                            "the note is on the card, but reviewer '{reviewer}' could NOT be \
                             told: {why}"
                        ));
                        tracing::warn!(
                            reviewer_unreachable = %reviewer,
                            owner = %owner_for_check,
                            "board note: reviewer named on a card in review cannot be reached"
                        );
                    }
                    None => {
                        // SELF-CHECKING RATHER THAN PRECONDITIONED, and the
                        // rejected alternative is the point (amux-frustrations,
                        // 2026-08-31).
                        //
                        // Steering waits for a turn boundary: measured mean lag
                        // 166s, max 3607s, 32% over a minute. So a review
                        // request can sit pending for hours while the card is
                        // reviewed and closed, and arrive asserting a premise
                        // that has expired.
                        //
                        // `steer_enqueue_precond` exists for exactly that and is
                        // WRONG HERE. It drops on any `rev` change, and its own
                        // docstring says why that is safe for its callers:
                        // "every producer here is a periodic trigger, so if the
                        // condition still holds it fires again on the next tick".
                        // A review request is not periodic. Dropped means gone,
                        // so preconditioning it would silently swallow requests
                        // whenever anybody appended a note -- recreating the
                        // exact defect this card reports.
                        //
                        // A message that survives and explains itself beats one
                        // that might vanish. The cost of a stale arrival is one
                        // reader glancing at a closed card; the cost of a
                        // dropped request is a review that reaches nobody.
                        let body_line = if note.trim().is_empty() {
                            "The card has just been moved into review.".to_string()
                        } else {
                            note.clone()
                        };
                        let prompt = format!(
                            "[review requested on {}: {}] {caller_for_notify} is waiting on \
                             YOUR review:\n{body_line}\n(You are named REVIEWER on this card. The \
                             full note is on it. This was queued when the card entered review \
                             and delivers at your next turn boundary, so if it has since left \
                             `review` it was already handled and you can ignore this.)",
                            body.get("id").and_then(Value::as_str).unwrap_or(""),
                            title.chars().take(60).collect::<String>()
                        );
                        match crate::api::session_verbs::steer_enqueue(
                            &state,
                            &reviewer,
                            &prompt,
                            "board-progress",
                            &caller_for_notify,
                        )
                        .await
                        {
                            Ok(_) => {
                                body["reviewer_notified"] = json!(true);
                                body["reviewer_notify_note"] = json!(format!(
                                    "{reviewer} will see the review request at their next turn \
                                     boundary"
                                ));
                            }
                            Err(reason) => {
                                body["reviewer_notified"] = json!(false);
                                body["reviewer_notify_reason"] = json!(format!(
                                    "the note is on the card, but reviewer '{reviewer}' could \
                                     NOT be notified: {reason}"
                                ));
                                tracing::warn!(
                                    reviewer_undelivered = %reviewer,
                                    reason = reason,
                                    "board note: review request not delivered"
                                );
                            }
                        }
                    }
                }
            }
            // Immediate path for an interactive terminal transition. The
            // periodic board-drive pass is the crash/non-HTTP recovery path;
            // doing both gives the caller real-time callback status without
            // sacrificing durability.
            let callback_pending = body
                .pointer("/callback/state")
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "pending" | "dispatching"));
            if callback_pending {
                let dispatch = dispatch_pending_callbacks(&state, Some(&id)).await;
                body["callback_dispatch"] = json!({
                    "attempted": dispatch.attempted,
                    "queued": dispatch.queued,
                    "refused": dispatch.refused,
                });
                if let Ok(conn) = state.store.read() {
                    if let Ok(Some(latest)) = bs::get_issue(&conn, &id) {
                        body["callback"] = latest.snapshot()["callback"].clone();
                        body["log"] = latest.snapshot()["log"].clone();
                        body["rev"] = latest.snapshot()["rev"].clone();
                    }
                }
            }
            // REACTIVE DRIVE through the same gates as the periodic sweep.
            if let Some((session, _from, to)) = status_transition {
                if matches!(to.as_str(), "done" | "verified" | "discarded")
                    && !session.is_empty()
                {
                    let st = state.clone();
                    crate::db::interactions::spawn(async move {
                        let _ = crate::runtime_jobs::board_drive::drive_session(&st, &session).await;
                        crate::api::session_verbs::steer_deliver_for_session(&st, &session).await;
                    });
                }
            }
            (StatusCode::OK, Json(body)).into_response()
        }
    }
}

#[cfg(test)]
mod capture_requeue_tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::HeaderValue;

    fn fixture() -> (AppState, crate::db::SharedStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            crate::db::Store::open(&dir.path().join("capture-requeue.db"))
                .expect("open store"),
        );
        // Store owns live SQLite handles after this helper returns.
        std::mem::forget(dir);
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "capture-requeue-test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, store)
    }

    fn seed_capture(store: &crate::db::SharedStore) -> String {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        store
            .write(move |conn| {
                let row = bs::create_issue(
                    conn,
                    &bs::NewIssue {
                        title: "Clear out this worker's board then grind it all out".into(),
                        desc: "**Prompt:** clear out this worker's board then grind it all out"
                            .into(),
                        status: "doing".into(),
                        session: Some("mvs-research".into()),
                        item_type: "chore".into(),
                        creator: "amux".into(),
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
                        source: Some("capture".into()),
                        requested_by: None,
                        callback_session: None,
                        callback_prompt: None,
                    },
                    1_788_955_507,
                )?;
                *slot_w.lock().expect("slot") = Some(row.id);
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .expect("seed capture");
        let id = slot.lock().expect("slot").clone().expect("capture id");
        id
    }

    fn worker_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-amux-session", HeaderValue::from_static("mvs-research"));
        headers
    }

    async fn patch(state: &AppState, id: &str, body: Value) -> (StatusCode, Value) {
        let response = patch_item(
            State(state.clone()),
            Path(id.to_string()),
            worker_headers(),
            Json(body),
        )
        .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response");
        (status, serde_json::from_slice(&bytes).expect("json response"))
    }

    /// MR-174 is the production specimen: status-update claimed the captured
    /// command, the lane registered its report asset, then PATCH moved the same
    /// untouched envelope doing -> backlog. The board faithfully rendered that
    /// last mutation, so it looked as though no work had happened and the fresh
    /// trigger parked it outside the drain loop.
    #[tokio::test]
    async fn a_worked_capture_envelope_cannot_disappear_back_into_a_queue() {
        let (state, store) = fixture();
        let id = seed_capture(&store);

        for target in ["backlog", "todo"] {
            let (status, body) = patch(&state, &id, json!({"status": target})).await;
            assert_eq!(status, StatusCode::CONFLICT, "{target}: {body}");
            assert_eq!(body["code"], "capture_requeue_requires_disposition");
            assert_eq!(body["preserved_status"], "doing");
            assert_eq!(body["attempted_status"], target);
            assert_eq!(
                bs::get_issue(&store.read().expect("read"), &id)
                    .expect("query")
                    .expect("card")
                    .status,
                "doing",
                "a refusal must not mutate the card"
            );
        }
    }

    /// The guard requires the model to make the semantic decision; it does not
    /// ban parking. Once the same atomic PATCH replaces the prompt envelope with
    /// a standalone task, the normal transition is available again.
    #[tokio::test]
    async fn reshaping_one_real_task_preserves_the_normal_backlog_exit() {
        let (state, store) = fixture();
        let id = seed_capture(&store);
        let desc = "Re-run every MVS verification job whose named prerequisite has cleared; record each result and produced artifact.";
        let (status, body) = patch(
            &state,
            &id,
            json!({
                "desc": desc,
                "status": "backlog",
                "source_ref": "Resume when MR-157 supplies the staging experiment path"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["applied"], true);
        assert_eq!(body["status"], "backlog");
        assert_eq!(body["desc"], desc);
    }

    /// The existing audited escape stays real. This is intentionally a
    /// positive control: deleting force support would make the primary refusal
    /// test pass while creating a state with no truthful exit.
    #[tokio::test]
    async fn an_attributed_reasoned_force_can_requeue_the_envelope() {
        let (state, store) = fixture();
        let id = seed_capture(&store);
        let (status, body) = patch(
            &state,
            &id,
            json!({
                "status": "backlog",
                "force": true,
                "reason": "the source message was revoked before its semantic disposition could be recorded"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "backlog");
        let log = body["log"].as_str().unwrap_or_default();
        assert!(log.contains("force by mvs-research: doing->backlog reason="), "{log}");
        assert!(log.contains("source message was revoked"), "{log}");
    }
}

#[cfg(test)]
mod af701_archive_guard_tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::HeaderValue;

    fn fixture() -> (AppState, crate::db::SharedStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            crate::db::Store::open(&dir.path().join("af701-archive-guard.db")).expect("open store"),
        );
        std::mem::forget(dir);
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "af701-archive-guard-test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, store)
    }

    fn seed(store: &crate::db::SharedStore, session: &str, status: &str) -> String {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        let session = session.to_string();
        let status = status.to_string();
        store
            .write(move |conn| {
                let row = bs::create_issue(
                    conn,
                    &bs::NewIssue {
                        title: "AF-701 fixture card".into(),
                        desc: "fixture".into(),
                        status,
                        session: Some(session),
                        item_type: "chore".into(),
                        creator: "test".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        tags: vec![],
                        ask_type: Some("decision".into()),
                        ask_question: Some("is this ok?".into()),
                        ask_unblocks: Some("the fixture proceeds".into()),
                        ask_actor: Some("ethan".into()),
                        source: Some("test".into()),
                        requested_by: None,
                        callback_session: None,
                        callback_prompt: None,
                    },
                    1_700_000_000,
                )?;
                *slot_w.lock().unwrap() = Some(row.id);
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .expect("seed");
        let id = slot.lock().unwrap().clone().unwrap();
        id
    }

    fn owner_headers(session: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-amux-session", HeaderValue::from_str(session).unwrap());
        headers
    }

    fn local_member_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-amux-local-member-verified", HeaderValue::from_static("1"));
        headers
    }

    async fn patch_as(state: &AppState, id: &str, headers: HeaderMap, body: Value) -> (StatusCode, Value) {
        let response = patch_item(State(state.clone()), Path(id.to_string()), headers, Json(body)).await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("read response");
        (status, serde_json::from_slice(&bytes).expect("json response"))
    }

    fn current(store: &crate::db::SharedStore, id: &str) -> bs::IssueRow {
        bs::get_issue(&store.read().expect("read"), id)
            .expect("query")
            .expect("card")
    }

    // ---- ask (b): the cross-lane guard must not exempt an unnamed caller ----

    #[tokio::test]
    async fn an_anonymous_caller_cannot_archive_a_named_owners_card() {
        let (state, store) = fixture();
        let id = seed(&store, "some-lane", "todo");
        let (status, body) =
            patch_as(&state, &id, HeaderMap::new(), json!({"archived": true})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "cross-lane destruction requires authorized_by");
        assert_eq!(current(&store, &id).archived, 0, "a refusal must not mutate the card");
    }

    #[tokio::test]
    async fn an_anonymous_caller_can_archive_with_authorized_by() {
        let (state, store) = fixture();
        let id = seed(&store, "some-lane", "todo");
        let (status, _body) = patch_as(
            &state,
            &id,
            HeaderMap::new(),
            json!({"archived": true, "authorized_by": "ethan", "archive_outcome": "no longer needed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(current(&store, &id).archived, 1);
    }

    #[tokio::test]
    async fn a_verified_local_member_can_still_archive_any_lanes_card_unauthorized() {
        // Regression control: the human-from-the-dashboard exemption must
        // survive closing the anonymous hole, or archiving breaks for Ethan.
        let (state, store) = fixture();
        let id = seed(&store, "some-lane", "todo");
        let (status, _body) = patch_as(
            &state,
            &id,
            local_member_headers(),
            json!({"archived": true, "archive_outcome": "no longer needed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(current(&store, &id).archived, 1);
    }

    #[tokio::test]
    async fn a_named_caller_archiving_their_own_card_needs_no_authorization() {
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "todo");
        let (status, _body) = patch_as(
            &state,
            &id,
            owner_headers("mvs-research"),
            json!({"archived": true, "archive_outcome": "no longer needed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(current(&store, &id).archived, 1);
    }

    #[tokio::test]
    async fn a_named_caller_archiving_a_different_lanes_card_is_still_refused() {
        let (state, store) = fixture();
        let id = seed(&store, "some-other-lane", "todo");
        let (status, body) =
            patch_as(&state, &id, owner_headers("mvs-research"), json!({"archived": true})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(current(&store, &id).archived, 0);
    }

    // ---- ask (a): archiving a needsyou card requires an outcome ----

    #[tokio::test]
    async fn archiving_a_needsyou_card_is_refused_without_an_outcome() {
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "needsyou");
        let (status, body) =
            patch_as(&state, &id, owner_headers("mvs-research"), json!({"archived": true})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "archiving a non-terminal card requires an outcome");
        let row = current(&store, &id);
        assert_eq!(row.archived, 0, "a refusal must not mutate the card");
        assert_eq!(row.status, "needsyou");
    }

    #[tokio::test]
    async fn archiving_a_needsyou_card_succeeds_with_archive_outcome() {
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "needsyou");
        let (status, body) = patch_as(
            &state,
            &id,
            owner_headers("mvs-research"),
            json!({"archived": true, "archive_outcome": "answered informally, no longer needed"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = current(&store, &id);
        assert_eq!(row.archived, 1);
        assert_eq!(row.status, "needsyou", "the outcome does not itself change status");
        assert!(
            row.log.as_deref().unwrap_or_default().contains("archive_outcome: answered informally"),
            "{:?}",
            row.log
        );
    }

    #[tokio::test]
    async fn archive_outcome_validation_uses_the_archive_flags_existing_coercion() {
        let (state, store) = fixture();
        for flag in [json!(true), json!(1), json!("1"), json!("true"), json!("TRUE"), json!(" yes "), json!("ON")] {
            let id = seed(&store, "mvs-research", "done");
            let (status, body) = patch_as(&state, &id, owner_headers("mvs-research"),
                json!({"archived":flag, "archive_outcome":"Exact compatibility reason"})).await;
            assert_eq!(status, StatusCode::OK, "flag={flag}: {body}");
            let row = current(&store, &id);
            assert_eq!(row.archived, 1);
            assert!(row.log.as_deref().unwrap_or_default().contains("archive_outcome: Exact compatibility reason"));
        }
        for flag in [json!(false), json!(0), json!("false"), json!("off"), json!(null), json!({}), json!([]), json!(2)] {
            let id = seed(&store, "mvs-research", "done");
            let (status, body) = patch_as(&state, &id, owner_headers("mvs-research"),
                json!({"archived":flag, "archive_outcome":"Must not be applied"})).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "flag={flag}: {body}");
            assert_eq!(current(&store, &id).archived, 0);
            assert!(body["discarded"].as_array().unwrap().contains(&json!("archive_outcome")));
        }
    }

    #[tokio::test]
    async fn archive_outcome_without_an_archive_is_explicitly_refused() {
        let (state, store) = fixture();
        for input in [json!({"archive_outcome":"must not vanish"}), json!({"archived":true,"archive_outcome":17})] {
            let id = seed(&store, "mvs-research", "done");
            let (status, body) = patch_as(&state, &id, owner_headers("mvs-research"), input).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body["discarded"].as_array().unwrap().contains(&json!("archive_outcome")), "{body}");
            assert_eq!(current(&store, &id).archived, 0);
        }
    }

    #[tokio::test]
    async fn a_new_outcome_on_an_already_archived_card_is_not_silently_lost() {
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "done");
        let input = json!({"archived":true,"archive_outcome":"original reason"});
        let (status, body) = patch_as(&state, &id, owner_headers("mvs-research"), input.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = patch_as(&state, &id, owner_headers("mvs-research"), json!({"archived":true,"archive_outcome":"different reason"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let row = current(&store, &id);
        assert!(row.log.as_deref().unwrap().contains("archive_outcome: original reason"));
        assert!(!row.log.as_deref().unwrap().contains("different reason"));
    }

    #[tokio::test]
    async fn a_combined_archive_and_status_change_in_one_request_is_still_refused_without_an_outcome() {
        // Tried making this combination the escape hatch first; it cannot work
        // (see the comment on the gate in patch_item), so this pins that a
        // combined request gets THIS gate's refusal rather than silently
        // archiving, silently changing status, or hitting the unrelated
        // ArchivedTaskImmutable error with no mention of needsyou at all.
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "needsyou");
        let (status, body) = patch_as(
            &state,
            &id,
            owner_headers("mvs-research"),
            json!({"archived": true, "status": "discarded"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "archiving a non-terminal card requires an outcome");
        let row = current(&store, &id);
        assert_eq!(row.archived, 0);
        assert_eq!(row.status, "needsyou");
    }

    #[tokio::test]
    async fn the_two_step_path_still_works_status_change_then_archive() {
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "needsyou");
        let (s1, b1) =
            patch_as(&state, &id, owner_headers("mvs-research"), json!({"status": "discarded"}))
                .await;
        assert_eq!(s1, StatusCode::OK, "{b1}");
        let (s2, b2) =
            patch_as(&state, &id, owner_headers("mvs-research"), json!({"archived": true})).await;
        assert_eq!(s2, StatusCode::OK, "{b2}");
        let row = current(&store, &id);
        assert_eq!(row.archived, 1);
        assert_eq!(row.status, "discarded");
    }

    #[tokio::test]
    async fn a_terminal_card_can_still_be_archived_with_no_outcome() {
        // CONTROL: a card that is ALREADY terminal (done/verified/discarded)
        // must be unaffected by this gate, or the gate blocks the ordinary
        // "archive what's finished" case it is not meant to touch.
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "done");
        let (status, body) =
            patch_as(&state, &id, owner_headers("mvs-research"), json!({"archived": true})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(current(&store, &id).archived, 1);
    }

    #[tokio::test]
    async fn archiving_a_non_terminal_non_needsyou_card_is_also_refused_without_an_outcome() {
        // AF-712: the needsyou-only gate let every OTHER non-terminal status
        // (todo/doing/backlog/blocked/review) accumulate archived+non-terminal
        // silently — 885 such cards measured fleet-wide. The gate generalizes
        // to any non-terminal status, not just needsyou.
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "backlog");
        let (status, body) =
            patch_as(&state, &id, owner_headers("mvs-research"), json!({"archived": true})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "archiving a non-terminal card requires an outcome");
        let row = current(&store, &id);
        assert_eq!(row.archived, 0, "a refusal must not mutate the card");
        assert_eq!(row.status, "backlog");
    }

    #[tokio::test]
    async fn archiving_a_non_terminal_non_needsyou_card_succeeds_with_archive_outcome() {
        let (state, store) = fixture();
        let id = seed(&store, "mvs-research", "doing");
        let (status, body) = patch_as(
            &state,
            &id,
            owner_headers("mvs-research"),
            json!({"archived": true, "archive_outcome": "superseded by a later card"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["ignored_fields"].as_array().is_none_or(|fields| !fields.contains(&json!("archive_outcome"))), "persisted outcome must not be reported ignored: {body}");
        let row = current(&store, &id);
        assert_eq!(row.archived, 1);
        assert_eq!(row.status, "doing", "the outcome does not itself change status");
        assert!(row.log.as_deref().unwrap_or_default().contains("mvs-research: archive_outcome: superseded by a later card"), "exact attributed outcome must survive readback: {:?}", row.log);
    }
}

#[cfg(test)]
mod af711_acceptance_criteria_tests {
    use super::*;
    use axum::body::to_bytes;

    fn fixture() -> (AppState, crate::db::SharedStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            crate::db::Store::open(&dir.path().join("af711-acceptance-criteria.db"))
                .expect("open store"),
        );
        std::mem::forget(dir);
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "af711-acceptance-criteria-test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, store)
    }

    fn seed(store: &crate::db::SharedStore, acceptance_criteria: Option<&str>) -> String {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        let acceptance_criteria = acceptance_criteria.map(str::to_string);
        store
            .write(move |conn| {
                let mut row = bs::create_issue(
                    conn,
                    &bs::NewIssue {
                        title: "AF-711 fixture card".into(),
                        desc: "fixture".into(),
                        status: "doing".into(),
                        session: Some("mvs-research".into()),
                        item_type: "code".into(),
                        creator: "test".into(),
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
                        source: Some("test".into()),
                        requested_by: None,
                        callback_session: None,
                        callback_prompt: None,
                    },
                    1_700_000_000,
                )?;
                row.acceptance_criteria = acceptance_criteria;
                bs::save_patched(conn, &mut row)?;
                *slot_w.lock().unwrap() = Some(row.id);
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .expect("seed");
        let id = slot.lock().unwrap().clone().unwrap();
        id
    }

    async fn patch_as(state: &AppState, id: &str, body: Value) -> (StatusCode, Value) {
        let response =
            patch_item(State(state.clone()), Path(id.to_string()), HeaderMap::new(), Json(body))
                .await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("read response");
        (status, serde_json::from_slice(&bytes).expect("json response"))
    }

    fn current(store: &crate::db::SharedStore, id: &str) -> bs::IssueRow {
        bs::get_issue(&store.read().expect("read"), id).expect("query").expect("card")
    }

    #[tokio::test]
    async fn an_array_patch_stores_and_reads_back_the_array_instead_of_clearing_it() {
        let (state, store) = fixture();
        let id = seed(&store, None);
        let (status, body) = patch_as(
            &state,
            &id,
            json!({"acceptance_criteria": ["first condition", "second condition"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["acceptance_criteria"],
            json!(["first condition", "second condition"]),
            "the PATCH response must echo the array back, not null: {body}"
        );
        let row = current(&store, &id);
        assert_eq!(
            row.snapshot()["acceptance_criteria"],
            json!(["first condition", "second condition"]),
            "a subsequent read must still show the array, not a silently cleared field"
        );
    }

    #[tokio::test]
    async fn a_string_patch_reads_back_the_identical_string() {
        let (state, store) = fixture();
        let id = seed(&store, None);
        let (status, body) =
            patch_as(&state, &id, json!({"acceptance_criteria": "a single plain-text condition"}))
                .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["acceptance_criteria"],
            json!("a single plain-text condition"),
            "a plain string must read back as the identical string, not null: {body}"
        );
        let row = current(&store, &id);
        assert_eq!(
            row.snapshot()["acceptance_criteria"],
            json!("a single plain-text condition")
        );
    }

    /// The real-world shape both incidents actually took: mixpeek-general's
    /// three lost cards all cleared acceptance_criteria in a PATCH that ALSO
    /// touched other fields in the same call, not an acceptance_criteria-only
    /// body.
    #[tokio::test]
    async fn a_mixed_field_patch_does_not_lose_acceptance_criteria() {
        let (state, store) = fixture();
        let id = seed(&store, None);
        let (status, body) = patch_as(
            &state,
            &id,
            json!({
                "acceptance_criteria": ["survives a mixed-field patch"],
                "next_action": "keep going",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["acceptance_criteria"], json!(["survives a mixed-field patch"]));
        assert_eq!(body["next_action"], json!("keep going"));
        let row = current(&store, &id);
        assert_eq!(
            row.snapshot()["acceptance_criteria"],
            json!(["survives a mixed-field patch"])
        );
    }

    #[tokio::test]
    async fn an_invalid_shape_is_rejected_and_does_not_clear_an_existing_value() {
        let (state, store) = fixture();
        let id = seed(&store, Some(&serde_json::to_string(&["already set"]).unwrap()));
        let (status, body) = patch_as(&state, &id, json!({"acceptance_criteria": 5})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let row = current(&store, &id);
        assert_eq!(
            row.snapshot()["acceptance_criteria"],
            json!(["already set"]),
            "a rejected write must never silently clear the existing value: {body}"
        );
    }

    #[tokio::test]
    async fn an_array_with_a_non_string_entry_is_rejected_and_does_not_clear_an_existing_value() {
        let (state, store) = fixture();
        let id = seed(&store, Some(&serde_json::to_string(&["already set"]).unwrap()));
        let (status, body) =
            patch_as(&state, &id, json!({"acceptance_criteria": ["fine", 5]})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let row = current(&store, &id);
        assert_eq!(row.snapshot()["acceptance_criteria"], json!(["already set"]));
    }

    #[tokio::test]
    async fn an_explicit_null_still_clears_it() {
        let (state, store) = fixture();
        let id = seed(&store, Some(&serde_json::to_string(&["already set"]).unwrap()));
        let (status, body) = patch_as(&state, &id, json!({"acceptance_criteria": null})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = current(&store, &id);
        assert_eq!(row.acceptance_criteria, None);
    }

    /// AF-711's second, distinct bug: legacy content stored as a plain
    /// (non-JSON-encoded) string by the old buggy write path must still be
    /// VISIBLE on read — not silently substituted with null just because it
    /// does not parse as JSON.
    #[tokio::test]
    async fn legacy_non_json_content_reads_back_as_the_raw_string_not_null() {
        let (_state, store) = fixture();
        let id = seed(&store, Some("a plain string stored before this fix, not JSON-encoded"));
        let row = current(&store, &id);
        assert_eq!(
            row.snapshot()["acceptance_criteria"],
            json!("a plain string stored before this fix, not JSON-encoded"),
            "legacy non-JSON content must not read back as null"
        );
    }
}

#[cfg(test)]
mod af703_citing_cards_tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::HeaderValue;

    fn fixture() -> (AppState, crate::db::SharedStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            crate::db::Store::open(&dir.path().join("af703-citing-cards.db")).expect("open store"),
        );
        std::mem::forget(dir);
        let state = AppState {
            store: store.clone(),
            started: std::time::Instant::now(),
            build_hash: "af703-citing-cards-test".into(),
            auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, store)
    }

    fn seed(store: &crate::db::SharedStore, title: &str, desc: &str) -> String {
        let slot = Arc::new(Mutex::new(None));
        let slot_w = slot.clone();
        let title = title.to_string();
        let desc = desc.to_string();
        store
            .write(move |conn| {
                let row = bs::create_issue(
                    conn,
                    &bs::NewIssue {
                        title,
                        desc,
                        status: "todo".into(),
                        session: Some("gtm-engine".into()),
                        item_type: "chore".into(),
                        creator: "test".into(),
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
                        source: Some("test".into()),
                        requested_by: None,
                        callback_session: None,
                        callback_prompt: None,
                    },
                    1_700_000_000,
                )?;
                *slot_w.lock().unwrap() = Some(row.id);
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .expect("seed");
        let id = slot.lock().unwrap().clone().unwrap();
        id
    }

    fn owner_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-amux-session", HeaderValue::from_static("gtm-engine"));
        headers
    }

    async fn patch_as(state: &AppState, id: &str, body: Value) -> (StatusCode, Value) {
        let response =
            patch_item(State(state.clone()), Path(id.to_string()), owner_headers(), Json(body)).await;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("read response");
        (status, serde_json::from_slice(&bytes).expect("json response"))
    }

    /// AF-703, the GE-607/GE-610 specimen: GE-610's title overstated a cost,
    /// GE-607's `blocked_on` quoted the same figure an hour after GE-610 was
    /// corrected. This pins that the correction NAMES the citing card instead
    /// of landing silently on GE-610 alone.
    #[tokio::test]
    async fn renaming_a_card_surfaces_other_cards_that_cite_its_id() {
        let (state, store) = fixture();
        let cited = seed(&store, "GTM spend needs ~$810 to close", "original estimate");
        let citing = seed(&store, "Follow-up decision", &format!("blocked_on: waiting on {cited}'s ~$810 approval"));

        let (status, body) =
            patch_as(&state, &cited, json!({"title": "GTM spend needs ~$520 to close"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let advisories = body["advisories"].as_array().expect("advisories array");
        let title_advisory = advisories
            .iter()
            .find(|a| a["field"] == json!("title"))
            .expect("a title advisory must be present");
        let cited_ids: Vec<&str> = title_advisory["citing_cards"]
            .as_array()
            .expect("citing_cards array")
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert!(cited_ids.contains(&citing.as_str()), "{cited_ids:?}");
    }

    #[tokio::test]
    async fn renaming_a_card_nobody_cites_adds_no_advisory() {
        let (state, store) = fixture();
        let lonely = seed(&store, "Nobody references this one", "desc");
        let (status, body) =
            patch_as(&state, &lonely, json!({"title": "Still nobody references this one"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["advisories"].is_null() || body["advisories"].as_array().unwrap().is_empty());
    }

    /// CONTROL: writing the SAME title is a no-op per the existing `t !=
    /// next.title` guard, and must not run the citation search at all.
    #[tokio::test]
    async fn writing_the_identical_title_does_not_search_for_citations() {
        let (state, store) = fixture();
        let cited = seed(&store, "Unchanged title", "desc");
        let _citing = seed(&store, "Cites it", &format!("see {cited}"));
        let (status, body) = patch_as(&state, &cited, json!({"title": "Unchanged title"})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["advisories"].is_null() || body["advisories"].as_array().unwrap().is_empty());
    }
}

// ---- POST /api/board/{id}/archive + /restore (RR-0055) -------------------

async fn archive_restore(
    state: AppState,
    id: String,
    headers: HeaderMap,
    body: Option<Value>,
    restore: bool,
) -> Response {
    let (actor, actor_name) = actor_from_headers(&headers);
    let reason = body
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    enum Out {
        NotFound,
        Refused(Value),
        Noop(Value),
        Applied(Value),
    }
    let slot: Arc<Mutex<Option<Out>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = bs::get_issue(conn, &id_w)? else {
                return finish(&slot_w, Out::NotFound, no_write());
            };
            let Some(task) = row.to_task() else {
                return finish(
                    &slot_w,
                    Out::Refused(json!({
                        "error": format!(
                            "current status {:?} is not in the shared vocabulary; \
                             move it to a builtin status first (PATCH status)",
                            row.status
                        ),
                    })),
                    no_write(),
                );
            };
            let tx = if restore {
                BoardTransition::Restore {
                    reason: reason.clone(),
                }
            } else {
                BoardTransition::Archive {
                    reason: reason.clone(),
                }
            };
            match apply_transition(&task, tx, &actor, &[], chrono::Utc::now()) {
                Ok(updated) => {
                    let mut next = row.clone();
                    next.archived = i64::from(updated.archived);
                    let verb = if restore { "restored" } else { "archived" };
                    let line = if reason.is_empty() {
                        format!("{actor_name}: {verb}")
                    } else {
                        format!("{actor_name}: {verb} — {reason}")
                    };
                    next.log = Some(bs::append_log(next.log.as_deref(), &hhmm(), &line));
                    next.rev = row.rev + 1;
                    next.version = i64::try_from(updated.version).unwrap_or(row.version + 1);
                    next.updated = now_secs();
                    bs::save_patched(conn, &mut next)?;
                    let event = ev_snap(&next, MutationKind::Updated);
                    finish(
                        &slot_w,
                        Out::Applied(detail_body(&next)),
                        WriteOutcome {
                            applied: true,
                            events: vec![event],
                        },
                    )
                }
                // Already in the requested archive state: honest no-op,
                // rev unmoved (Invariant 37).
                Err(TransitionError::NoOp) => finish(&slot_w, Out::Noop(detail_body(&row)), no_write()),
                Err(e) => finish(
                    &slot_w,
                    Out::Refused(json!({ "error": e.to_string() })),
                    no_write(),
                ),
            }
        })
        .await;
    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("archive/restore produced no outcome"),
        Some(Out::NotFound) => not_found(&id),
        Some(Out::Refused(body)) => err(StatusCode::CONFLICT, body),
        Some(Out::Noop(mut body)) => {
            body["applied"] = json!(false);
            (StatusCode::OK, Json(body)).into_response()
        }
        Some(Out::Applied(mut body)) => {
            body["applied"] = json!(true);
            body["global_rev"] = json!(reply.rev.0);
            (StatusCode::OK, Json(body)).into_response()
        }
    }
}

pub async fn archive_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    archive_restore(state, id, headers, body.map(|Json(v)| v), false).await
}

pub async fn restore_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    archive_restore(state, id, headers, body.map(|Json(v)| v), true).await
}

// ---- DELETE /api/board/{id} ---------------------------------------------

/// SOFT delete, Python parity: stamp `deleted` and the row disappears from
/// every read path (all of them filter `deleted IS NULL`) while staying on
/// disk for forensics. The SPA has always called this; it simply had no
/// handler, and the 405 was invisible because the card had already been
/// removed from the local list.
pub async fn delete_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (_actor, actor_name) = actor_from_headers(&headers);
    enum Out {
        NotFound,
        Deleted(Value),
    }
    let slot: Arc<Mutex<Option<Out>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let who = actor_name.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(row) = bs::get_issue(conn, &id_w)? else {
                return finish(&slot_w, Out::NotFound, no_write());
            };
            // Record WHO deleted it before the row leaves every read path —
            // a delete that leaves no trace of its author is the audit hole
            // ethos rule 6 is about, and the log column survives soft delete.
            let mut logged = row.clone();
            logged.log = Some(bs::append_log(
                logged.log.as_deref(),
                &hhmm(),
                &format!("{who}: deleted"),
            ));
            logged.rev = row.rev + 1;
            logged.version = row.version + 1;
            logged.updated = now_secs();
            bs::save_patched(conn, &mut logged)?;
            if !bs::soft_delete(conn, &id_w)? {
                return finish(&slot_w, Out::NotFound, no_write());
            }
            let event = ev_snap(&logged, MutationKind::Deleted);
            finish(
                &slot_w,
                Out::Deleted(json!({"ok": true, "deleted": true, "id": id_w})),
                WriteOutcome {
                    applied: true,
                    events: vec![event],
                },
            )
        })
        .await;
    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("delete produced no outcome"),
        Some(Out::NotFound) => not_found(&id),
        Some(Out::Deleted(mut body)) => {
            body["global_rev"] = json!(reply.rev.0);
            (StatusCode::OK, Json(body)).into_response()
        }
    }
}

// ---- POST /api/board/{id}/status-request · /status-update ----------------
//
// The D1-exit pair (AMUX-2174), and the ethos inverse of terminal scraping:
// amux does not INFER a card's status, it routes a request and the owning
// session's own model AUTHORS the answer onto the card. ethos.md records this
// as the reason the board is the source of truth.
//
// Both were lost in the Rust cutover and answered 405 with an EMPTY body, which
// is the worst available failure for this pair, because every layer that
// mentions them kept telling the fleet to use them:
//   - `amux board ask` / `amux board status-update` (the CLI's own help)
//   - the SPA card menu's "ask for status" (`_askCardStatus`, app.js)
//   - the advance nudge: "post a status-update / mark its blocker"
//   - the board contract's `board_is_source_of_truth` clause
// This is AMUX-2140's shape a second time: following the sanctioned instruction
// exactly is what produced the failure. It also escaped the route census,
// because that enumerates SPA and CLI call sites and the CLI reaches these by
// hand-rolled curl — so the endpoint the CLI most depends on is exactly the one
// the "does every caller have a route" invariant could not see.

/// The size cap for a status update. Python applied 1200 (py:69770) on the theory
/// that an update is a SUMMARY, not a transcript sink. But 1200 chars silently
/// amputated real cross-group HANDOFFS mid-sentence and still returned
/// {"ok":true} (AMUX-3079), so the default is raised, made configurable, and any
/// truncation is now reported loudly by the handler rather than being silent.
fn status_update_max() -> usize {
    std::env::var("AMUX_STATUS_UPDATE_MAX")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(8000)
}

async fn status_request(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let question: String = body
        .as_ref()
        .and_then(|Json(v)| v.get("question"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(400)
        .collect();
    let (_actor, requester) = actor_from_headers(&headers);
    let requester = if requester.is_empty() || requester == "api-anonymous" {
        headers
            .get("X-Amux-User-Email")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("Ethan")
            .to_string()
    } else {
        requester
    };

    let (session, title, terminal) = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return internal(e),
        };
        match bs::get_issue(&conn, &id) {
            Ok(Some(row)) => (
                row.session.clone().unwrap_or_default(),
                row.title.clone(),
                bs::is_terminal_status(&row.status),
            ),
            Ok(None) => return not_found(&id),
            Err(e) => return internal(e),
        }
    };
    let session = session.trim().to_string();
    // A stale client can still invoke the old Refresh/status-request path after
    // the card closed. Do not route that request to a provider: the board's
    // terminal outcome is authoritative. Still append the attempted action so
    // the durable record explains why no provider message appeared.
    if terminal {
        let question_part = if question.is_empty() {
            String::new()
        } else {
            format!(" — \\\"{question}\\\"")
        };
        let line = format!(
            "status request by {requester} ignored: card is terminal; authoritative Final outcome preserved{question_part}"
        );
        if let Err(e) = append_card_log(&state, &id, &line, None).await {
            return internal(e);
        }
        tracing::info!(
            target: "amux::board", marker = "terminal_status_request_preserved",
            task = %id, requester = %requester,
            "terminal status request was recorded without provider delivery"
        );
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "delivered": false,
                "session": session,
                "reason": "card is terminal; authoritative Final outcome is preserved",
                "logged": true,
            })),
        )
            .into_response();
    }
    if session.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "delivered": false,
                        "reason": "card has no owning session to ask"})),
        )
            .into_response();
    }
    if !crate::api::session_verbs::is_running(&session).await {
        // Honest offline path (ethos rule 7): never fake a live answer, and say
        // plainly that nothing was queued, so the caller knows to ask again
        // rather than waiting on a delivery that will not happen.
        return Json(json!({
            "ok": false, "delivered": false,
            "reason": format!("session '{session}' is not running"),
            "hint": "the request is not queued; ask again when the session runs",
        }))
        .into_response();
    }

    let q_part = if question.is_empty() {
        String::new()
    } else {
        format!(": {question}")
    };
    let prompt = format!(
        "[amux status request on {id}: {}] {requester} asks for a status update{q_part}.\n\
         Reply by running:  amux board status-update {id} \"<what's done, what's next, any blocker>\"\n\
         That posts to the BOARD, which is the source of truth — a chat reply alone does not update the card.",
        title.chars().take(80).collect::<String>()
    );
    // Delivered at the next TURN BOUNDARY via the one steering queue, never a
    // direct send: the decision recorded in ethos.md ("Board state changes are
    // delivered at turn boundaries") is that a running agent cannot consume an
    // event faster than its next turn anyway.
    // READ THE RESULT, DO NOT ASSERT IT (AMUX-3713). This was `let _ =`
    // followed by a hardcoded `"delivered": true`, which is the two halves of
    // one contradiction: `steer_enqueue` is `#[must_use]` and its own attribute
    // text says `let _ =` means "the refusal is deliberately unreported here",
    // and the very next line then reported delivery unconditionally.
    //
    // The enqueue REFUSES a permanently-blocked target (no-env-file, archived),
    // so for exactly the lanes a status request cannot reach, the caller was
    // told it had been delivered — and the card log got the same false line
    // written into it, which is worse, because that one outlives the response.
    let queued =
        crate::api::session_verbs::steer_enqueue(&state, &session, &prompt, "status-request", &requester)
            .await;

    let line = match &queued {
        Ok(_) if question.is_empty() => {
            format!("status requested by {requester} (routed to {session})")
        }
        Ok(_) => format!("status requested by {requester} — \"{question}\" (routed to {session})"),
        // The card log records WHAT HAPPENED, not what was attempted. A reader
        // scanning history for "why did nobody answer" gets the reason here
        // instead of a routing line that never routed.
        Err(reason) => format!(
            "status request by {requester} NOT delivered to {session}: {reason} — the lane \
             cannot receive, so nobody was asked"
        ),
    };
    if let Err(e) = append_card_log(&state, &id, &line, None).await {
        return internal(e);
    }
    match queued {
        Ok(_) => Json(json!({"ok": true, "delivered": true, "session": session,
                    "message": format!("asked {session} to post a status update to {id}")}))
        .into_response(),
        Err(reason) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "delivered": false,
                "session": session,
                "blocked_reason": reason,
                "error": format!(
                    "{session} cannot receive a status request ({reason}), so none was sent. \
                     The card log records the refusal."
                ),
            })),
        )
            .into_response(),
    }
}

async fn status_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let cap = status_update_max();
    let full: String = body
        .as_ref()
        .and_then(|Json(v)| v.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let original_chars = full.chars().count();
    let truncated = original_chars > cap;
    let text: String = full.chars().take(cap).collect();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "text required"}))).into_response();
    }
    if truncated {
        // Never silent again (AMUX-3079): the caller and a log sweep must both
        // see that a handoff was stored as a fragment.
        tracing::warn!(
            target: "board",
            id = %id, original_chars, cap,
            "status-update TRUNCATED to the cap and stored a fragment; raise \
             AMUX_STATUS_UPDATE_MAX or split the update",
        );
    }
    let (_actor, actor_name) = actor_from_headers(&headers);
    let actor = if actor_name.is_empty() || actor_name == "api-anonymous" {
        "session".to_string()
    } else {
        actor_name
    };
    {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return internal(e),
        };
        match bs::get_issue(&conn, &id) {
            Ok(Some(_)) => {}
            // Python appended to a card it never checked existed, so a typo'd
            // id reported {"ok": true} and wrote nothing anyone could find.
            Ok(None) => return not_found(&id),
            Err(e) => return internal(e),
        }
    }
    let update = match apply_status_update(
        &state,
        &id,
        &format!("STATUS ({actor}): {text}"),
        bs::output_asset_refs(&text),
        &actor,
        &text,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    if update.claimed {
        tracing::info!(
            target: "amux::board", marker = "status_update_claimed_exact",
            task = %id, worker = %actor, from = %update.prior_status,
            "worker status update atomically claimed its exact actionable card"
        );
        crate::api::session_verbs::emit_event(
            &state, &actor, "task.claimed",
            Some(json!({"issue": id, "status": "doing"})), None,
            "board-status-update",
        ).await;
    } else if matches!(update.prior_status.as_str(), "todo" | "backlog") {
        tracing::warn!(
            target: "amux::board", marker = "status_update_claim_refused",
            task = %id, worker = %actor, owner = %update.owner,
            status = %update.prior_status, verdict = %update.claim_verdict,
            "worker status update was stored but did not claim the actionable card"
        );
    }
    let captured = &update.captured;
    if !captured.is_empty() {
        tracing::info!(
            target: "amux::board",
            task = %id,
            worker = %actor,
            captured = captured.len(),
            refs = ?captured.iter().map(|a| a.ref_value.as_str()).collect::<Vec<_>>(),
            "board task artifacts auto-captured from worker output"
        );
    }
    let mut resp = Json(json!({
        "ok": true, "id": id, "actor": actor,
        "chars": text.chars().count(),
        "original_chars": original_chars,
        "truncated": truncated,
        "claimed": update.claimed,
        "status": update.status,
        "claim_verdict": update.claim_verdict,
        "artifacts_captured": captured.len(),
        "artifact_refs": captured.iter().map(|a| a.ref_value.clone()).collect::<Vec<_>>(),
    }))
    .into_response();
    if truncated {
        resp.headers_mut()
            .insert("x-amux-truncated", axum::http::HeaderValue::from_static("1"));
    }
    resp
}

/// Distinct card ids (`PREFIX-123`) referenced in free text, first-seen order.
/// The discard-orphan detector (AMUX-3323) uses this to spot an umbrella capture
/// whose desc points at several children right before it is discarded — the
/// shape that abandoned the connectors + MDAI epics.
fn card_refs(text: &str) -> Vec<String> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\b[A-Z][A-Z0-9]+-\d+\b").expect("card ref regex")
    });
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let id = m.as_str().to_string();
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Capsule endpoint (Phase 2a): L1 context for agent consumption.
// ---------------------------------------------------------------------------

/// GET /api/board/{id}/capsule
///
/// Returns the L1 continuation capsule: the minimal structured context an agent
/// needs to pick up a task with zero conversation history. Deliberately small
/// (300-800 tokens), designed for agent context windows.
async fn capsule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let row = match bs::get_issue(&conn, &id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let ac = bs::parse_json_or_raw_string(row.acceptance_criteria.as_deref());
    let files: Vec<String> = conn
        .prepare("SELECT path FROM issue_files WHERE issue_id = ?1")
        .and_then(|mut stmt| {
            stmt.query_map(rusqlite::params![id], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()
        })
        .unwrap_or_default();
    let deps_status: Vec<Value> = row.depends_on.iter().filter_map(|dep_id| {
        bs::get_issue(&conn, dep_id).ok().flatten().map(|d| {
            json!({"id": d.id, "title": d.title, "status": d.status})
        })
    }).collect();
    let verifications = crate::db::verification_store::list_for_task(&conn, &id)
        .unwrap_or_default();
    let last_verification = verifications.first().map(|v| {
        json!({"verdict": v.verdict, "actor": v.actor, "at": v.created_at})
    });
    (
        StatusCode::OK,
        Json(json!({
            "id": row.id,
            "title": row.title,
            "type": row.item_type,
            "status": row.status,
            "session": row.session,
            "next_action": row.next_action,
            "last_result": row.last_result,
            "unresolved": row.unresolved,
            "blocked_on": row.blocked_on,
            "evidence": row.evidence,
            "acceptance_criteria": ac,
            "depends_on": deps_status,
            "artifacts": files,
            "gate": row.gate_criteria(),
            "last_verification": last_verification,
            "entered_state_at": row.entered_state_at,
            "decision_question": row.decision_question,
            "decision_rationale": row.decision_rationale,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Verification history (Phase 1a): structured records of every verify attempt.
// ---------------------------------------------------------------------------

/// GET /api/board/{id}/verifications
async fn list_verifications(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    if bs::get_issue(&conn, &id).ok().flatten().is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    let rows = match crate::db::verification_store::list_for_task(&conn, &id) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let items: Vec<Value> = rows.iter().map(|r| {
        json!({
            "id": r.id,
            "task_id": r.task_id,
            "verdict": r.verdict,
            "reason": r.reason,
            "actor": r.actor,
            "created_at": r.created_at,
        })
    }).collect();
    (StatusCode::OK, Json(json!(items))).into_response()
}

// ---------------------------------------------------------------------------
// Artifact CRUD (Phase 3a): per-task artifact registry.
// ---------------------------------------------------------------------------

fn artifact_value(r: &crate::db::artifact_store::ArtifactRow) -> Value {
    json!({
        "id": r.id,
        "task_id": r.task_id,
        "kind": r.kind,
        "ref": r.ref_value,
        "state": r.state,
        "description": r.description,
        "created_at": r.created_at,
        "updated_at": r.updated_at,
    })
}

/// GET /api/board/{id}/artifacts
async fn list_artifacts(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    if bs::get_issue(&conn, &id).ok().flatten().is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    }
    let rows = match crate::db::artifact_store::list_for_task(&conn, &id) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let items: Vec<Value> = rows.iter().map(artifact_value).collect();
    (StatusCode::OK, Json(json!(items))).into_response()
}

/// Does this write error mean "the card does not exist" (AF-477)?
///
/// EXTRACTED so the NARROWNESS is testable. The guard lives inline in a match
/// arm otherwise, and the only mutation reachable there touches the Err side —
/// so widening it to catch EVERY error would send a genuine storage fault to
/// 404 and no test would notice. That gap was recorded on AF-477 as unproven
/// rather than implied away; this is what closes it.
///
/// The closure returns `anyhow::Error`, so the rusqlite variant has to be
/// downcast rather than matched (E0308 if you try, caught at compile).
fn is_missing_task(e: &anyhow::Error) -> bool {
    e.downcast_ref::<rusqlite::Error>()
        .is_some_and(|r| matches!(r, rusqlite::Error::QueryReturnedNoRows))
}

/// POST /api/board/{id}/artifacts
async fn create_artifact(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let kind = match body.get("kind").and_then(|v| v.as_str()) {
        Some(k) if !k.trim().is_empty() => k.trim().to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "kind required"}))).into_response()
        }
        Some(_) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "kind cannot be blank"}))).into_response()
        }
    };
    let ref_value = match body.get("ref").and_then(|v| v.as_str()) {
        Some(r) if !r.trim().is_empty() => r.trim().to_string(),
        None => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "ref required"}))).into_response()
        }
        Some(_) => {
            tracing::warn!(
                marker = "artifact_blank_ref",
                task = %id,
                "board artifact registration refused: a blank reference cannot be opened"
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "artifact ref cannot be blank",
                    "code": "artifact_ref_blank",
                    "task_id": id,
                })),
            )
                .into_response();
        }
    };
    let state_val = body
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("created")
        .trim()
        .to_string();
    let desc = body.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
    if !crate::db::artifact_store::KNOWN_KINDS.contains(&kind.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "unknown artifact kind",
            "kind": kind,
            "valid_kinds": crate::db::artifact_store::KNOWN_KINDS,
        }))).into_response();
    }
    if !crate::db::artifact_store::ARTIFACT_STATES.contains(&state_val.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": "unknown artifact state",
            "state": state_val,
            "valid_states": crate::db::artifact_store::ARTIFACT_STATES,
        }))).into_response();
    }
    let (_, actor) = actor_from_headers(&headers);
    let now = chrono::Utc::now().timestamp();
    let aid = format!("ART-{}", ulid::Ulid::new().to_string().to_lowercase());
    let row = crate::db::artifact_store::ArtifactRow {
        id: aid.clone(),
        task_id: id.clone(),
        kind: kind.clone(),
        ref_value: ref_value.clone(),
        state: state_val.clone(),
        description: desc,
        created_at: now,
        updated_at: now,
    };
    let aid_out = aid.clone();
    let actor_w = actor.clone();
    let kind_w = row.kind.clone();
    let state_w = row.state.clone();
    let ref_w = row.ref_value.clone();
    let task_id_w = id.clone();
    let write = state.store.write_async(move |conn| {
        let Some(mut task) = bs::get_issue(conn, &task_id_w)? else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        crate::db::artifact_store::insert(conn, &row)?;
        let stamp = chrono::Local::now().format("%H:%M").to_string();
        task.log = Some(bs::append_log(
            task.log.as_deref(),
            &stamp,
            &format!("artifact ({actor_w}): {kind_w}/{state_w} {ref_w}"),
        ));
        task.updated = now;
        task.rev += 1;
        task.version += 1;
        bs::save_patched(conn, &mut task)?;
        Ok(crate::db::WriteOutcome {
            applied: true,
            events: vec![
                crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("artifact".into()),
                    entity_id: aid.clone(),
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: None,
                },
                ev_snap(&task, MutationKind::Updated),
            ],
        })
    }).await;
    match write {
        Ok(_) => {
            tracing::info!(
                target: "amux::board",
                task = %id,
                artifact = %aid_out,
                worker = %actor,
                kind = %kind,
                state = %state_val,
                reference = %ref_value,
                "board task artifact registered"
            );
            (StatusCode::CREATED, Json(json!({
                "id": aid_out,
                "task_id": id,
                "kind": kind,
                "ref": ref_value,
                "state": state_val,
                "actor": actor,
            }))).into_response()
        }
        // A MISSING CARD IS A 404, NOT A 500 (AF-475). The closure signals
        // "no such task" with rusqlite::Error::QueryReturnedNoRows, which fell
        // into the arm below and answered `500 Query returned no rows` — a raw
        // storage error as the entire body, on a request whose only fault was
        // naming a card that does not exist. Found by the 2026-09-04 log sweep:
        // one row, mixpeek-cicd, 0.25ms, and in the analyze output it is
        // indistinguishable from a genuine server fault.
        //
        // That indistinguishability is the cost: the sweep's contract says a
        // 500 is ALWAYS a finding, so a client error wearing a 500 buys a real
        // investigation every time it appears.
        Err(e) if is_missing_task(&e) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such task", "task_id": id})),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// PATCH /api/board/{id}/artifacts/{aid}
async fn patch_artifact(
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let new_state = body.get("state").and_then(|v| v.as_str()).map(|s| s.to_string());
    let new_desc = body.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
    if new_state.is_none() && new_desc.is_none() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "nothing to update"}))).into_response();
    }
    if let Some(ref new_state) = new_state {
        if !crate::db::artifact_store::ARTIFACT_STATES.contains(&new_state.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "unknown artifact state",
                    "state": new_state,
                    "valid_states": crate::db::artifact_store::ARTIFACT_STATES,
                })),
            )
                .into_response();
        }
    }
    let now = chrono::Utc::now().timestamp();
    let (_, actor) = actor_from_headers(&headers);
    let task_for_log = id.clone();
    let aid_for_log = aid.clone();
    let state_for_log = new_state.clone();
    let write = state.store.write_async(move |conn| {
        if bs::get_issue(conn, &id)?.is_none() {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        let existing = crate::db::artifact_store::get(conn, &aid)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if existing.task_id != id {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        if let Some(ref s) = new_state {
            crate::db::artifact_store::update_state(conn, &aid, s, now)?;
        }
        if let Some(ref d) = new_desc {
            conn.execute(
                "UPDATE _amux_task_artifacts SET description = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![d, now, aid],
            )?;
        }
        Ok(crate::db::WriteOutcome {
            applied: true,
            events: vec![crate::db::PendingEvent {
                entity_type: amux_core::revision::EntityType::Other("artifact".into()),
                entity_id: aid.clone(),
                mutation: amux_core::revision::MutationKind::Updated,
                payload: None,
            }],
        })
    }).await;
    match write {
        Ok(_) => {
            if let Some(ref disposition) = state_for_log {
                if crate::db::artifact_store::is_retired_state(disposition) {
                    tracing::warn!(
                        target: "amux::board",
                        marker = "artifact_retired",
                        task = %task_for_log,
                        artifact = %aid_for_log,
                        worker = %actor,
                        state = %disposition,
                        measured = true,
                        n_considered = 1,
                        "board artifact retained for audit but retired from valid produced assets"
                    );
                }
            }
            (StatusCode::OK, Json(json!({"ok": true}))).into_response()
        }
        Err(e) if is_missing_task(&e) => {
            tracing::warn!(
                marker = "artifact_target_missing",
                task = %task_for_log,
                artifact = %aid_for_log,
                operation = "patch",
                "board artifact mutation refused: artifact is absent or belongs to another task"
            );
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "artifact not found on this task",
                    "code": "artifact_target_missing",
                    "task_id": task_for_log,
                    "artifact_id": aid_for_log,
                })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// DELETE /api/board/{id}/artifacts/{aid}
async fn delete_artifact(
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
) -> Response {
    let task_for_log = id.clone();
    let aid_for_log = aid.clone();
    let write = state.store.write_async(move |conn| {
        if bs::get_issue(conn, &id)?.is_none() {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        let n = crate::db::artifact_store::delete_for_task(conn, &id, &aid)?;
        if n == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(crate::db::WriteOutcome {
            applied: true,
            events: vec![crate::db::PendingEvent {
                entity_type: amux_core::revision::EntityType::Other("artifact".into()),
                entity_id: aid.clone(),
                mutation: amux_core::revision::MutationKind::Deleted,
                payload: None,
            }],
        })
    }).await;
    match write {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if is_missing_task(&e) => {
            tracing::warn!(
                marker = "artifact_target_missing",
                task = %task_for_log,
                artifact = %aid_for_log,
                operation = "delete",
                "board artifact deletion refused: artifact is absent or belongs to another task"
            );
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "artifact not found on this task",
                    "code": "artifact_target_missing",
                    "task_id": task_for_log,
                    "artifact_id": aid_for_log,
                })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod discard_orphan_tests {
    use super::card_refs;

    #[test]
    fn card_refs_extracts_distinct_ids_in_order() {
        let got = card_refs("decomposed into AMUX-3324, AMUX-3192 and AMUX-3324 again; GE-1 too");
        assert_eq!(
            got,
            vec![
                "AMUX-3324".to_string(),
                "AMUX-3192".to_string(),
                "GE-1".to_string()
            ]
        );
    }

    #[test]
    fn card_refs_empty_when_no_ids() {
        assert!(card_refs("just prose, no ids, port 8822").is_empty());
    }
}

#[derive(Debug, Clone)]
struct StatusUpdateResult {
    captured: Vec<crate::db::artifact_store::ArtifactRow>,
    claimed: bool,
    prior_status: String,
    status: String,
    owner: String,
    claim_verdict: String,
}

/// Store a worker's progress and, when eligible, claim that worker's exact
/// card in the same serialized transaction (GCA-153 / ATE-41).
async fn apply_status_update(
    state: &AppState,
    id: &str,
    line: &str,
    refs: Vec<String>,
    actor: &str,
    progress: &str,
) -> Result<StatusUpdateResult, rusqlite::Error> {
    let (id, line, actor, progress, stamp) = (
        id.to_string(), line.to_string(), actor.to_string(),
        progress.to_string(), hhmm(),
    );
    let result = Arc::new(Mutex::new(None));
    let result_w = result.clone();
    state.store.write_async(move |conn| {
        let row = bs::get_issue(conn, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let prior_status = row.status.clone();
        let owner = row.session.as_deref().unwrap_or("").trim().to_string();
        let mut claimed = false;
        let mut status = prior_status.clone();
        let mut events = Vec::new();

        let claim_verdict = if !matches!(prior_status.as_str(), "todo" | "backlog") {
            "status_not_actionable".to_string()
        } else if actor == "session" {
            "actor_unattributed".to_string()
        } else if row.owner_type != "agent" {
            "owner_not_agent".to_string()
        } else if owner != actor {
            "owner_mismatch".to_string()
        } else if row.archived != 0 {
            "archived".to_string()
        } else if row.tags.iter().any(|tag| tag.to_ascii_lowercase().starts_with("needs:you")) {
            "needs_you".to_string()
        } else if row.waiting_on.as_deref().is_some_and(|v| !v.trim().is_empty()) {
            "waiting".to_string()
        } else if crate::runtime_jobs::board_drive::fresh_source_ref_trigger(&row, now_secs()) {
            "external_trigger".to_string()
        } else if !crate::runtime_jobs::board_drive::deps_blocking(conn, &row).is_empty() {
            "dependency_blocked".to_string()
        } else if let Some(exclusion) = frontier_exclusion(&row, false) {
            match exclusion {
                FrontierExclusion::Blocked => "blocked".to_string(),
                FrontierExclusion::DormantType => "dormant_type".to_string(),
                FrontierExclusion::NoContinuation => unreachable!("continuation gate is off"),
            }
        } else if bs::continuation_required(Some(&actor))
            && bs::continuation_verdict(&progress) != bs::ContinuationVerdict::Ok
        {
            "continuation_missing".to_string()
        } else {
            let holding: Vec<String> = conn.prepare(
                "SELECT id FROM issues WHERE session=?1 AND status='doing' AND id!=?2 \
                 AND deleted IS NULL AND COALESCE(archived,0)=0 \
                 AND COALESCE(type,'') NOT IN ('tripwire','watch','epic') \
                 AND NOT (creator='amux' AND substr(COALESCE(\"desc\",''),1,11)='**Prompt:**') \
                 AND NOT EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id=issues.id \
                                 AND lower(t.tag) LIKE 'needs:you%') ORDER BY id"
            )?.query_map(rusqlite::params![&actor, &id], |r| r.get::<_, String>(0))?
                .filter_map(Result::ok).collect();
            if !holding.is_empty() {
                "wip_conflict".to_string()
            } else {
                if bs::continuation_required(Some(&actor)) {
                    conn.execute(
                        "UPDATE issues SET next_action=?1 WHERE id=?2",
                        rusqlite::params![&progress, &id],
                    )?;
                }
                let opts = crate::db::advance::AdvanceOpts {
                    expected_from: Some(prior_status.clone()),
                    assign_to: Some(actor.clone()),
                    log_line: Some(format!("Claimed by status update from {actor}")),
                    force: false,
                    skip_continuation: false,
                    gate_ack: true,
                    ..Default::default()
                };
                match crate::db::advance::advance(conn, &id, "doing", &actor, &opts)? {
                    Ok(outcome) => {
                        claimed = true;
                        status = "doing".to_string();
                        events.extend(outcome.events);
                        "claimed".to_string()
                    }
                    Err(_) => "transition_refused".to_string(),
                }
            }
        };

        // `advance` appended the claim audit line, so read that before adding progress.
        let current_log = if claimed { bs::get_issue(conn, &id)?.and_then(|r| r.log) } else { row.log };
        let next = bs::append_log(current_log.as_deref(), &stamp, &line);
        conn.execute("UPDATE issues SET log=?1 WHERE id=?2", rusqlite::params![next, &id])?;
        let inserted = crate::db::artifact_store::insert_captured_refs(
            conn, &id, refs,
            &format!("automatically captured from status update by {actor}"), now_secs(),
        )?;
        if bs::is_terminal_status(&prior_status)
            && row
                .last_result
                .as_deref()
                .is_some_and(|summary| summary.starts_with("Final outcome:"))
        {
            tracing::info!(
                target: "amux::board", marker = "terminal_status_update_preserved",
                task = %id, worker = %actor,
                "terminal status update appended its audit evidence without replacing Final outcome"
            );
        }
        events.extend(inserted.iter().map(|artifact| crate::db::PendingEvent {
            entity_type: amux_core::revision::EntityType::Other("artifact".into()),
            entity_id: artifact.id.clone(),
            mutation: amux_core::revision::MutationKind::Created,
            payload: None,
        }));
        *result_w.lock().expect("status update result slot poisoned") = Some(StatusUpdateResult {
            captured: inserted, claimed, prior_status, status, owner, claim_verdict,
        });
        Ok(WriteOutcome { applied: true, events })
    }).await.map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(
        std::io::Error::other(e.to_string()),
    )))?;
    let outcome = result.lock().expect("status update result slot poisoned")
        .clone().ok_or(rusqlite::Error::QueryReturnedNoRows);
    outcome
}

/// Append one stamped line for requests that carry no worker progress claim.
async fn append_card_log(
    state: &AppState,
    id: &str,
    line: &str,
    capture: Option<(Vec<String>, String)>,
) -> Result<Vec<crate::db::artifact_store::ArtifactRow>, rusqlite::Error> {
    let (id, line, stamp) = (id.to_string(), line.to_string(), hhmm());
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_w = captured.clone();
    state
        .store
        .write_async(move |conn| {
            let existing: Option<String> = conn
                .query_row("SELECT log FROM issues WHERE id=?1", [&id], |r| r.get(0))
                .unwrap_or(None);
            let next = bs::append_log(existing.as_deref(), &stamp, &line);
            conn.execute(
                "UPDATE issues SET log=?1 WHERE id=?2",
                rusqlite::params![next, &id],
            )?;
            let inserted = match capture {
                Some((refs, description)) => crate::db::artifact_store::insert_captured_refs(
                    conn,
                    &id,
                    refs,
                    &description,
                    now_secs(),
                )?,
                None => Vec::new(),
            };
            let events = inserted
                .iter()
                .map(|artifact| crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("artifact".into()),
                    entity_id: artifact.id.clone(),
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: None,
                })
                .collect();
            *captured_w.lock().expect("artifact capture slot poisoned") = inserted;
            Ok(WriteOutcome { applied: true, events })
        })
        .await
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string()))))?;
    let inserted = captured
        .lock()
        .expect("artifact capture slot poisoned")
        .clone();
    Ok(inserted)
}

#[cfg(test)]
mod param_tests {
    use super::ignored_board_params;

    /// BACKE-3228: a mistyped/unknown filter must be reported ignored, a real
    /// filter must NOT, and a cache-buster must not create noise.
    #[test]
    fn ignored_params_names_typos_not_real_filters_or_cachebusters() {
        // The exact case that bit amux-cloud: a plausible-but-wrong name.
        assert_eq!(ignored_board_params("include_archived=1"), vec!["include_archived"]);
        assert_eq!(ignored_board_params("done=1&limits=5"), vec!["done", "limits"]);
        // Real filters are consumed, never flagged.
        assert!(ignored_board_params("session=amux&status=todo&archived=1&done_limit=0").is_empty());
        assert!(ignored_board_params("slim=1&limit=10&offset=5").is_empty());
        // q/query/search are consumed (refused with a 400), so not "ignored".
        assert!(ignored_board_params("q=nudge").is_empty());
        // Cache-busters are benign, not surfaced (would be noise on every poll).
        assert!(ignored_board_params("_=1699999999&session=amux").is_empty());
        assert!(ignored_board_params("t=123&cb=x").is_empty());
        // Case-insensitive on the key; de-duplicated.
        assert_eq!(ignored_board_params("Foo=1&foo=2"), vec!["Foo"]);
        // Empty / no query -> nothing.
        assert!(ignored_board_params("").is_empty());
        // Mixed: only the typo is named, alongside a real filter + cache-buster.
        assert_eq!(ignored_board_params("session=x&includearchived=1&_=9"), vec!["includearchived"]);
    }
}

#[cfg(test)]
mod isolation_designation_tests {
    use super::*;

    /// AMUX-3713: a card whose owning lane is ISOLATED says so, and an ordinary
    /// one does not.
    ///
    /// Ethan asked for this after the verification: isolated mode works — the
    /// peer fleet list hides those workers and a peer send is refused 403 — but
    /// their CARDS were exempt from all of it. `desktop` owns 25 board cards and
    /// not one carried any indication that the owning lane is undiscoverable and
    /// cannot be messaged, so a peer reading the board would route work to a
    /// session it has no way to reach.
    ///
    /// BOTH DIRECTIONS. Without the negative cell, stamping every card
    /// `owner_isolated: true` would pass the first and make the designation
    /// meaningless — a flag that is always on is not a flag.
    ///
    /// The lanes are addressed through `session_is_isolated`, the SAME predicate
    /// the fleet filter and the send guard consult, so the card cannot claim a
    /// reachability the send path disagrees with (ethos rule 1).
    #[test]
    fn a_card_owned_by_an_isolated_lane_is_designated_and_an_ordinary_one_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::api::settings::test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).expect("sessions");
        std::fs::write(dir.path().join("sessions/raw.env"), "CC_ISOLATED=\"1\"\n").expect("w");
        std::fs::write(dir.path().join("sessions/normal.env"), "CC_TAGS=\"amux\"\n").expect("w");

        // PREMISE, asserted rather than assumed: if the env home did not take,
        // both lanes read as non-isolated and the negative cell below passes
        // for the wrong reason.
        assert!(
            crate::api::session_verbs::session_is_isolated("raw"),
            "premise: the fixture's CC_ISOLATED must be visible to the predicate"
        );
        assert!(!crate::api::session_verbs::session_is_isolated("normal"));

        let card = |sess: &str| {
            let mut r = IssueRow { id: "T-1".into(), ..Default::default() };
            r.session = Some(sess.to_string());
            r
        };

        // DRIVE WHAT THE HTTP HANDLERS DRIVE. The first version of this test
        // called `list_body(row, slim=false)` for the detail case and passed,
        // while the live GET /api/board/<id> returned nothing — `get_item`
        // calls `detail_body` directly and never goes through `list_body`. A
        // real property, asserted one layer above where the request flows
        // (ethos rule 7). Caught by curling the running server, not by the
        // suite. `bodies` now names the actual entry point of each path.
        type Body = fn(&IssueRow) -> Value;
        let bodies: [(&str, Body); 3] = [
            ("detail_body (GET /api/board/{id})", |r| detail_body(r)),
            ("list_body non-slim", |r| list_body(r, false, false)),
            ("list_body slim", |r| list_body(r, true, false)),
        ];
        for (slim, f) in bodies {
            let iso = f(&card("raw"));
            assert_eq!(
                iso["owner_isolated"],
                json!(true),
                "slim={slim}: an isolated lane's card must be designated: {iso}"
            );
            let reach = iso["owner_reach"].as_str().unwrap_or_default();
            assert!(
                reach.contains("refused as a peer send target"),
                "slim={slim}: the designation must say what it MEANS, not just that it is \
                 true — a bare boolean makes every consumer re-derive the implication: {iso}"
            );

            // THE NEGATIVE. Absent, not `false`: a key on every ordinary card is
            // payload for nothing, and the list ships 1700+ of them.
            let ord = f(&card("normal"));
            assert!(
                ord.get("owner_isolated").is_none(),
                "slim={slim}: an ordinary lane's card must carry no designation at all: {ord}"
            );
            assert!(ord.get("owner_reach").is_none(), "slim={slim}: {ord}");
        }
    }
}

#[cfg(test)]
mod slim_tests {
    /// A SCOPED query must answer completely (ts-gke, 2026-08-11).
    ///
    /// The terminal cap made `?session=X` under-report by 26 of 94 done cards
    /// while the body carried no sign of it, and a digest built on that list
    /// reported 25. This pins the DEFAULT, which is the thing that was wrong:
    /// unfiltered caps, scoped does not, and an explicit ?done_limit always
    /// wins so a caller who asks for a bound still gets one.
    #[test]
    fn a_scoped_query_is_not_capped_by_default() {
        let d = |session: Option<&str>, status: Option<&str>, explicit: Option<i64>| {
            let scoped = session.is_some() || status.is_some();
            explicit.unwrap_or(if scoped { 0 } else { 100 })
        };
        assert_eq!(d(None, None, None), 100, "the unfiltered board still caps — the dashboard cannot draw 1300 terminal cards");
        assert_eq!(d(Some("ts-gke"), None, None), 0, "?session= must answer completely");
        assert_eq!(d(None, Some("done"), None), 0, "?status= must answer completely");
        assert_eq!(d(Some("ts-gke"), Some("done"), None), 0, "both together too");
        // An explicit bound is honoured in BOTH shapes — otherwise this change
        // would have taken away a caller's ability to ask for a small page.
        assert_eq!(d(Some("ts-gke"), None, Some(5)), 5, "explicit done_limit wins when scoped");
        assert_eq!(d(None, None, Some(5)), 5, "and when unfiltered");
        assert_eq!(d(None, None, Some(0)), 0, "an explicit 0 still means uncapped");
    }

    /// `?all=1` is the discoverable escape from the terminal cap (AMUX-3154).
    ///
    /// Every session that hit the cap reached for this exact param
    /// (mixpeek-funnel, mixpeek-frustrations, ts-gke tried `?all=1`/`?limit=N`)
    /// and got the capped 100-terminal view back, because `all` was UNRECOGNISED
    /// and axum dropped it — the rule-7 failure where a filter that never ran
    /// returns a confident wrong denominator (a lane auditing its `done` work off
    /// the plain list read ~6% of it). This pins that `?all=1` now uncaps, that
    /// the dashboard render poll (which omits it) still caps, that an explicit
    /// ?done_limit still wins, and — the half that makes it real — that `all` is
    /// a RECOGNISED param and not silently dropped like it was.
    #[test]
    fn all_1_uncaps_the_unfiltered_terminal_set() {
        // Mirror the real derivation at list_board: unscoped, `?all=1`, explicit.
        let d = |uncap_all: bool, explicit: Option<i64>| {
            let scoped = false; // the unscoped list is the case that was wrong
            explicit.unwrap_or(if scoped || uncap_all { 0 } else { 100 })
        };
        assert_eq!(d(false, None), 100, "the bare list still caps — the dashboard render poll omits ?all=1");
        assert_eq!(d(true, None), 0, "?all=1 must answer completely — the escape every capped caller tried");
        assert_eq!(d(true, Some(5)), 5, "an explicit done_limit wins even alongside ?all=1");
        // The half that was the actual bug: an unrecognised `all` is dropped, so
        // the cap answers and the escape silently no-ops.
        assert!(RECOGNISED_BOARD_PARAMS.contains(&"all"), "?all must be recognised");
        assert!(ignored_board_params("all=1").is_empty(), "?all=1 must not be reported as ignored");
    }

    use super::*;
    use crate::db::board_store::IssueRow;

    /// The desc-clobber guard, tested as the two rules it actually is
    /// (91648fbc + AMUX-3576), against the four REAL cards from the incidents.
    ///
    /// Rule 1, SIZE: catches the owner clobbering their own long write-up.
    /// mvs-infra's report — 4082 chars of merge evidence replaced by a short
    /// note, because they read `desc` off the list, which omits it, and took
    /// absence for emptiness.
    ///
    /// Rule 2, AUTHORSHIP: catches a peer clobbering someone else's prose at
    /// ANY magnitude. Size cannot: AF-180 went 3055 -> 1958, a 36% drop that
    /// sits under any threshold which avoids crying wolf on ordinary trims, and
    /// it destroyed a peer's review notes exactly as thoroughly as the 60% one
    /// beside it. The discriminator is whose prose is destroyed, which is a
    /// comparison and can be exact rather than tuned.
    ///
    /// The controls carry the weight, as before: a refusal met during ordinary
    /// work becomes a reflexive ack, which turns a safety property into a
    /// keystroke. So an append, a typo fix on a peer's card, and the owner
    /// editing their own must all pass untouched.
    #[test]
    fn the_desc_clobber_guard_catches_both_acts_and_nothing_ordinary() {
        // Mirrors the predicates at the write site. If those change, this must
        // change with them and deliberately.
        let size = |before: usize, after: usize| before >= 500 && after * 2 < before;
        // THE SHIPPED PREDICATE, not a restatement of it. This used to be a
        // closure mirroring the write site, with a comment asking the next
        // editor to keep the two in step by hand — which is testing a paraphrase
        // (ethos rule 7), and is how the two numeric floors below stayed wrong
        // for weeks with this test green beside them.
        let authorship = desc_replace_destroys_peer_prose;
        // Multi-LINE, because a desc is prose and the rule now asks whether any
        // of the owner's lines survive. A single 3000-character run of 'x' is
        // not a description, and building the controls out of one is the
        // fixture-domain error ethos rule 7 names: it cannot express "one line
        // was edited and the rest were not", which is the whole distinction
        // between a typo fix and a clobber.
        let text = |n: usize| {
            let mut out = String::new();
            for i in 0..n.div_ceil(60) {
                out.push_str(&format!("line {i} of the owner's write-up, about sixty chars.\n"));
            }
            out
        };

        // -- the real incidents ------------------------------------------------
        // mvs-infra / MI-4746: 4082 chars of merge evidence -> a short note.
        assert!(size(4082 + 120, 120), "the reported near-data-loss must be refused");

        // AF-180: 3055 -> 1958 by a REVIEWER on the author's card. The size
        // rule misses it; that miss is the entire reason AMUX-3576 exists.
        assert!(!size(3055, 1958), "36% is under the size bar — this is the gap, stated");
        assert!(
            authorship("amux-frustrations", "amux", &text(3055), "my replacement note"),
            "a reviewer replacing the author's prose must be refused at ANY magnitude"
        );

        // AF-179: the same act at 46%, which the live guard already refused.
        assert!(authorship("amux-frustrations", "amux", &text(4573), "shorter note"));

        // AF-191, the two live specimens the numeric floors let through. Both
        // were reproduced against the running server on scratch cards, and both
        // returned `applied: true` with the owner's text gone.
        assert!(
            authorship("amux-cloud", "amux", "their whole one-line description", "17 chars ok"),
            "amux-cloud's specimen: a 54-char desc replaced by 17. The old `before >= 200` \
             floor let it through, and the friction it reported is a peer destroying a SHORT \
             card, which is most cards"
        );
        assert!(
            authorship(
                "amux-frustrations",
                "amux-cloud",
                &text(264),
                &format!("{}{}", "TOTALLY DIFFERENT CONTENT. ".repeat(14), "and longer.")
            ),
            "my specimen: a LONGER replacement destroys everything and lost zero characters \
             net, so the old delta floor could never fire on it"
        );

        // -- CONTROLS: every one of these is legitimate --------------------------
        let orig = text(3000);
        assert!(
            !authorship("amux-frustrations", "amux", &orig, &format!("{orig}\n\nmy review")),
            "APPENDING to a peer's write-up must never trip it — it keeps what was there"
        );
        assert!(
            !authorship("amux", "amux", &text(3055), &text(1958)),
            "the OWNER editing their own card down is ordinary work"
        );
        assert!(
            !authorship("amux-frustrations", "", &text(3055), &text(1958)),
            "an unattributed caller has no authorship to compare — the size rule still applies, \
             but this rule must not fire on an empty writer"
        );
        assert!(
            !authorship("", "amux", &text(3055), &text(1958)),
            "nor on an ownerless card"
        );
        // A typo fix on a peer's card: one line edited, the rest untouched.
        // THIS is the control the net-loss floor was standing in for, and it
        // still passes without a threshold, because the other lines survive.
        let typo_fixed = text(3000).replacen("about sixty chars.", "about sixty charz.", 1);
        assert!(
            !authorship("amux-frustrations", "amux", &text(3000), &typo_fixed),
            "correcting a typo in ONE line of a peer's write-up leaves the rest and must pass"
        );
        assert!(
            !authorship("amux-frustrations", "amux", &text(3000), &text(2900)),
            "trimming a peer's card while keeping most of their lines must pass"
        );
        // THE FLOOR THAT IS GONE, asserted in its new direction so removing it
        // cannot be quietly undone. A short desc used to be exempt at any cost;
        // it is amux-cloud's incident.
        assert!(
            authorship("amux-frustrations", "amux", "one short line of theirs", "mine instead"),
            "a SHORT desc is not exempt any more — 54 chars was the reported incident"
        );

        // -- the size rule's own controls, unchanged ----------------------------
        assert!(!size(4000, 2400), "trimming 40% of a long desc is an ordinary edit");
        assert!(!size(4000, 2000), "exactly half must NOT trip — the bar is a strict majority");
        assert!(!size(400, 0), "clearing a SHORT desc is not a data loss worth blocking");
        assert!(!size(120, 4082), "growth must never trip it");
    }

    /// SLIM MUST CARRY WHAT THE LIST ACTUALLY RENDERS (AMUX-2840).
    ///
    /// This is pinned because the same slimming was tried before and reverted:
    /// `list_body`'s own doc says an earlier first-line-desc + `log_n` version
    /// "silently blanked both in the dashboard". It blanked them because it
    /// removed the fields without replacing what the SPA derives FROM them.
    /// A payload diet that drops a rendered value is a regression wearing a
    /// performance win, and it fails silently — the card just looks empty.
    /// AF-555. A row shipped 48 fields including `status` and `archived` and
    /// nothing that answered the question every consumer asks, so every
    /// consumer derived it — and gtm-engine's derivation, in another repo and
    /// another language, read status and never `archived`, silently
    /// suppressing a hand-raiser breach page.
    #[test]
    fn a_row_publishes_whether_it_is_live_so_no_consumer_has_to_derive_it() {
        let live = IssueRow { id: "T-1".into(), status: "todo".into(), ..Default::default() };
        assert_eq!(live_state(&live), (true, "live"));

        // The exact shape that suppressed the page: archived, but in a status
        // that dispatch would otherwise treat as workable.
        let ghost = IssueRow {
            id: "T-2".into(),
            status: "todo".into(),
            archived: 1,
            ..Default::default()
        };
        assert_eq!(live_state(&ghost), (false, "archived"));

        // gtm-engine's carve-out: a needs:you ask is still owed after archiving,
        // and a blanket rule would re-mint a breach over a live human ask.
        let ask = IssueRow {
            id: "T-3".into(),
            status: "needsyou".into(),
            archived: 1,
            ..Default::default()
        };
        assert_eq!(live_state(&ask), (true, "archived_but_ask_still_owed"));

        let done = IssueRow { id: "T-4".into(), status: "verified".into(), ..Default::default() };
        assert_eq!(live_state(&done), (false, "status_claims_no_work"));

        // A status we cannot parse is NOT reported dead: a measurement that did
        // not run must not wear the clothes of one that did.
        let weird = IssueRow { id: "T-5".into(), status: "banana".into(), ..Default::default() };
        assert_eq!(live_state(&weird), (true, "status_unparseable"));
    }

    /// The verdict must survive slimming. A liveness field that disappears from
    /// the list payload is worse than one that never existed: `row.get("live")`
    /// returning None reads as an answer, which is the exact failure `desc`
    /// caused (AF-161) and that `slim` names its drops to prevent.
    #[test]
    fn the_live_verdict_rides_through_slim_and_is_on_both_serializers() {
        let row = IssueRow {
            id: "T-6".into(),
            status: "todo".into(),
            archived: 1,
            ..Default::default()
        };
        for (label, v) in [
            ("full list", list_body(&row, false, false)),
            ("slim list", list_body(&row, true, false)),
            ("single-card GET", detail_body(&row)),
        ] {
            assert_eq!(v["live"], serde_json::json!(false), "{label} must carry live");
            assert_eq!(v["live_reason"], serde_json::json!("archived"), "{label} reason");
        }
        assert!(
            !SLIM_OMITS.contains(&"live") && !SLIM_OMITS.contains(&"live_reason"),
            "adding either to SLIM_OMITS reintroduces the absent-key-reads-as-answer bug"
        );
    }

    #[test]
    fn slim_drops_the_prose_but_keeps_the_two_things_the_list_renders() {
        let row = IssueRow {
            id: "T-1".into(),
            title: "a card".into(),
            desc: "First line is the preview.\nNew task: folded one\nNew task: folded two".into(),
            log: Some("`10:00` did a thing\n`10:01` New task: folded three".into()),
            item_type: "code".into(),
            ..Default::default()
        };

        let full = list_body(&row, false, false);
        assert!(full["desc"].is_string(), "the plain list still serves full desc");
        assert!(full["log"].is_string(), "and full log");

        let slim = list_body(&row, true, false);
        // The diet itself.
        assert!(slim["desc"].is_null(), "slim must not ship the prose");
        assert!(slim["log"].is_null());
        // ...and the two derivations that make it safe to drop them.
        assert_eq!(
            slim["desc_head"], "First line is the preview.",
            "app.js:19488 renders the first line as the card preview"
        );
        assert_eq!(
            slim["folded_n"], 3,
            "app.js:18866 counts 'New task:' across desc AND log for the folded badge"
        );
        assert_eq!(slim["desc_len"], row.desc.chars().count());
    }

    /// The third derivation (app.js:19231). LAST marker wins, not first — a
    /// re-marked card must show its freshest question, which is the client's own
    /// rule and the reason a naive `find` would be wrong.
    #[test]
    fn slim_carries_the_latest_needsyou_marker() {
        let row = IssueRow {
            desc: "NEEDS-YOU: the stale one\nsome prose".into(),
            log: Some("`10:00` moved\nNEEDS-YOU: the fresh one".into()),
            ..Default::default()
        };
        assert_eq!(list_body(&row, true, false)["needsyou_note"], "the fresh one");

        // Spelling variants the client accepts, case-insensitively.
        for spelling in ["NEEDS-YOU:", "needs you:", "NEEDSYOU:", "Needs-Ethan:", "needs-human:"] {
            let r = IssueRow { desc: format!("{spelling} answer me"), ..Default::default() };
            assert_eq!(
                list_body(&r, true, false)["needsyou_note"], "answer me",
                "spelling {spelling} must be recognised"
            );
        }

        // ABSENT means ABSENT: the key is omitted rather than served as an empty
        // string, so a client can distinguish "no marker" from "a blank marker".
        let plain = IssueRow { desc: "ordinary card".into(), ..Default::default() };
        assert!(list_body(&plain, true, false).get("needsyou_note").is_none());
    }

    /// Every spelling app.js's /NEEDS[- ]?(?:YOU|ETHAN|HUMAN):/i accepts must
    /// produce a note here, or the slim client and the full client disagree
    /// about the same card. The three ETHAN/HUMAN space and no-separator forms
    /// were missing until 2026-08-11.
    #[test]
    fn needsyou_matches_every_spelling_the_client_regex_accepts() {
        for spelling in [
            "NEEDS-YOU:", "NEEDS YOU:", "NEEDSYOU:",
            "NEEDS-ETHAN:", "NEEDS ETHAN:", "NEEDSETHAN:",
            "NEEDS-HUMAN:", "NEEDS HUMAN:", "NEEDSHUMAN:",
            "needs-you:", "needs ethan:",
        ] {
            let row = IssueRow {
                id: "X-1".into(),
                title: "a card".into(),
                desc: format!("{spelling} answer me"),
                item_type: "code".into(),
                ..Default::default()
            };
            assert_eq!(
                list_body(&row, true, false)["needsyou_note"], "answer me",
                "spelling {spelling:?} must yield a note — the client regex accepts it"
            );
        }
        // A marker with nothing after it is not a marker.
        let empty = IssueRow { desc: "NEEDS-YOU:   ".into(), ..Default::default() };
        assert!(list_body(&empty, true, false).get("needsyou_note").is_none());
    }

    /// The preview must be bounded and must not panic on multi-byte text — it
    /// is built with `chars().take()`, not a byte slice, and an empty desc is
    /// ordinary rather than an error.
    #[test]
    fn the_preview_is_bounded_and_multibyte_safe() {
        let long = IssueRow { desc: "é".repeat(400), ..Default::default() };
        let v = list_body(&long, true, false);
        assert_eq!(v["desc_head"].as_str().unwrap().chars().count(), 120);

        let empty = IssueRow { desc: String::new(), ..Default::default() };
        let v = list_body(&empty, true, false);
        assert_eq!(v["desc_head"], "");
        assert_eq!(v["folded_n"], 0);

        // Leading blank lines are skipped: the preview is the first line with
        // CONTENT, not the first line.
        let padded = IssueRow { desc: "\n\n  \nreal content here".into(), ..Default::default() };
        assert_eq!(list_body(&padded, true, false)["desc_head"], "real content here");
    }

    /// AF-160 / AMUX-3532. The criterion's OWN INSTRUCTION must be followable.
    ///
    /// `Peer-reviewed by a DIFFERENT worker in group `amux` (name them)` told the
    /// acking agent to supply a name, and exact string matching then rejected any
    /// ack that supplied one. Every case below is a real string that was sent and
    /// refused, or a shell-mangled form of one.
    #[test]
    fn an_ack_that_follows_the_criterions_own_instruction_is_accepted() {
        let crit = "Peer-reviewed by a DIFFERENT worker in group `amux` (name them)";

        // The whole point: filling in the parenthetical must MATCH.
        assert_eq!(ack_norm("Peer-reviewed by a different worker in group amux (amux)"), ack_norm(crit));
        // Case, which differs between the criterion and ordinary prose.
        assert_eq!(ack_norm("peer-reviewed by a different worker in group `amux` (name them)"), ack_norm(crit));
        // Backticks, which a shell eats unless escaped — so the string sent
        // silently differs from the one the caller believes they sent.
        assert_eq!(ack_norm("Peer-reviewed by a DIFFERENT worker in group amux (name them)"), ack_norm(crit));
        // Verbatim still matches, or this would be a migration rather than a widening.
        assert_eq!(ack_norm(crit), ack_norm(crit));

        // AND IT MUST STILL DISCRIMINATE. Normalization that collapsed distinct
        // criteria would turn a per-criterion ack into `gate_ack` wearing a
        // costume — the exact mechanism this gate exists to prevent.
        let others = [
            "Functionality change is live and exercised, not just merged",
            "That peer verified it themselves rather than taking the author's word",
            "No regression in what it touched",
        ];
        for o in others {
            assert_ne!(ack_norm(o), ack_norm(crit), "{o} must not satisfy the peer criterion");
        }
        // Only a TRAILING parenthetical is dropped, never arbitrary text.
        assert_ne!(ack_norm("Peer-reviewed by a DIFFERENT worker"), ack_norm(crit));

        // And the detector fires on the criterion that asks, not on its neighbours.
        assert!(criterion_wants_a_name(crit));
        for o in others {
            assert!(!criterion_wants_a_name(o));
        }
    }

    /// AF-161. `reviewer` SURVIVES the slim list, and a slim row SAYS it is slim.
    ///
    /// This test exists at this layer on purpose. `snapshot_slim` already had a
    /// guard — `snapshot_slim_is_snapshot_minus_prose` — and it passed the whole
    /// time the bug was live, because the drop happens one layer UP in
    /// `list_body`, not in the snapshot. A check that pins the wrong layer is
    /// exactly as green as one that pins the right one, and it certified a
    /// payload that was snapshot-minus-prose-minus-five-more for weeks.
    ///
    /// The cost of the absence was a census reported as 25 of 25 verified cards
    /// with no reviewer, when the truth was 7 named and 18 absent. `.get()`
    /// returns None for a removed key and for an empty value alike, so the
    /// wrong answer arrived looking like a finding.
    #[test]
    fn a_slim_row_keeps_the_reviewer_and_declares_that_it_is_slim() {
        let named = IssueRow {
            reviewer: Some("amux-frustrations".into()),
            source_ref: Some("ref".into()),
            ..Default::default()
        };
        let slim = list_body(&named, true, false);

        // The field the census needed, present and correct.
        assert_eq!(
            slim["reviewer"], "amux-frustrations",
            "reviewer must survive the slim list — its absence is what made the audit wrong"
        );
        // A row with NO reviewer must still carry the key, or the caller is back
        // to guessing: absent and null have to be distinguishable from each other
        // only by the value, never by the key.
        let anon = IssueRow::default();
        let slim_anon = list_body(&anon, true, false);
        assert!(
            slim_anon.get("reviewer").is_some(),
            "the key must be present even when null, or absence still reads as emptiness"
        );
        assert!(slim_anon["reviewer"].is_null());

        // The self-description, which is the general remedy rather than the
        // one-column one.
        // A slim row must say WHAT is gone, not merely that something is
        // (AF-200). Compared against the shipped const rather than a literal
        // restated here, so the two cannot drift.
        assert_eq!(
            slim["slim"],
            json!(SLIM_OMITS),
            "a slim row must ENUMERATE its omissions — `1` tells a consumer something \
             was dropped and leaves it guessing which, which is how a 1809-char desc \
             read as empty"
        );

        // Still slim: the expensive drops stay dropped, or this test would be
        // pinning the absence of the optimisation instead of the presence of
        // the fix.
        for gone in SLIM_OMITS {
            assert!(
                slim.get(gone).is_none(),
                "{gone} must stay out of the slim list — it is the payload diet's whole point"
            );
        }

        // And the FULL body is unchanged by any of this: it carries everything,
        // and it must not sprout a `slim` marker.
        let full = list_body(&named, false, false);

        // THE NON-CIRCULAR ASSERTION, and the only one here that can catch a
        // WRONG const. Everything above iterates SLIM_OMITS, so the code and the
        // test read the same list and agree with each other by construction —
        // shrink the const and both stop checking the dropped field together.
        // That is AF-161's defect exactly: a real property, asserted at a layer
        // the bug does not pass through.
        //
        // So derive the omissions from the two payloads and require the const to
        // MATCH REALITY. A field dropped without being declared, or declared
        // without being dropped, fails here and nowhere else.
        let full_keys: std::collections::BTreeSet<&str> =
            full.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let slim_keys: std::collections::BTreeSet<&str> =
            slim.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        let actually_omitted: Vec<&str> =
            full_keys.difference(&slim_keys).copied().collect();
        let declared: Vec<&str> = {
            let mut d: Vec<&str> = SLIM_OMITS.to_vec();
            d.sort_unstable();
            d
        };
        assert_eq!(
            actually_omitted, declared,
            "SLIM_OMITS must name exactly the fields the slim body drops. Left is what \
             the payloads actually differ by, right is what the const claims — a field \
             in one and not the other is a consumer being told the wrong thing about \
             what it may trust."
        );
        assert_eq!(full["reviewer"], "amux-frustrations");
        assert!(full.get("desc").is_some());
        assert!(full.get("slim").is_none(), "a full row must not claim to be slim");
    }

    // ---- AMUX-3391: auto-fold the silent capture card into the worker's own ----

    fn fold_db() -> rusqlite::Connection {
        crate::db::migrate::test_memdb()
    }

    fn fold_card(creator: &str, status: &str, desc: &str, session: &str) -> bs::NewIssue {
        bs::NewIssue {
            title: "t".into(),
            desc: desc.into(),
            status: status.into(),
            session: Some(session.into()),
            item_type: "code".into(),
            creator: creator.into(),
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
            // AF-367: the HTTP create path: a real POST /api/board from a lane or a human.
            source: Some("agent".into()),
            requested_by: None,
            callback_session: None,
            callback_prompt: None,
        }
    }

    /// The core reconcile: a worker carding its own work folds its lane's fresh
    /// auto-capture card, discarding it in place with a tombstone that links to
    /// the worker card — so there is one card, not the two that 68% of the time
    /// ended in a hand discard.
    #[test]
    fn a_worker_card_folds_the_fresh_capture_for_its_lane() {
        let conn = fold_db();
        let cap =
            bs::create_issue(&conn, &fold_card("amux", "doing", "**Prompt:** do the thing", "lane"), 1000)
                .unwrap();
        let worker =
            bs::create_issue(&conn, &fold_card("lane", "todo", "Fix the thing", "lane"), 1010).unwrap();

        let folded = fold_capture_for_worker_card(&conn, &worker, 600, 1010).unwrap();
        assert_eq!(
            folded.as_ref().map(|(id, _)| id.as_str()),
            Some(cap.id.as_str()),
            "the worker card must fold its lane's fresh capture"
        );
        let got = bs::get_issue(&conn, &cap.id).unwrap().unwrap();
        assert_eq!(got.status, "discarded", "the folded capture is discarded in place");
        assert!(
            got.desc.contains(&format!("Folded into {}", worker.id)),
            "the tombstone links to the worker card"
        );
    }

    /// AF-616: the auto-fold CHOSE this target by adjacency, so the line it
    /// writes must carry the inference marker. mixpeek-frustrations' specimen
    /// is a report captured as AF-613 and folded into AF-615, an unrelated
    /// finding carded in the same minute; the summary asserted that fold in the
    /// same words a lane's own `--folded-into` would have produced.
    ///
    /// This pins the LABEL, not the choice. Narrowing the window is a change to
    /// every lane's board and is not made here.
    #[test]
    fn an_auto_fold_records_that_it_inferred_the_target() {
        let conn = fold_db();
        let cap =
            bs::create_issue(&conn, &fold_card("amux", "doing", "**Prompt:** do the thing", "lane"), 1000)
                .unwrap();
        let worker =
            bs::create_issue(&conn, &fold_card("lane", "todo", "Fix the thing", "lane"), 1010).unwrap();
        fold_capture_for_worker_card(&conn, &worker, 600, 1010).unwrap();

        let got = bs::get_issue(&conn, &cap.id).unwrap().unwrap();
        let log = got.log.as_deref().unwrap_or("");
        assert!(
            log.contains(&format!("capture folded into {} {}", worker.id, bs::FOLD_INFERRED_MARKER)),
            "the adjacency-chosen target must be labelled as inferred; log was: {log}"
        );
        // AND IT MUST STILL RESOLVE. A marker that broke the id would turn a
        // readable fold back into the dropped-request rendering b3db93fd fixed.
        assert_eq!(
            bs::folded_into_detail(Some(log)),
            Some((worker.id.clone(), true)),
            "the labelled line must still parse to the target"
        );
    }

    /// The negative controls — each is a case where a fold would be WRONG, and a
    /// filter that folded everything would look identical to a correct one from
    /// the happy-path test alone (ethos rule 7). The capture must stay `doing`.
    #[test]
    fn fold_leaves_a_capture_alone_when_it_should_not_fire() {
        // (a) an amux-created card is the capture actor, never a folder.
        {
            let conn = fold_db();
            let cap = bs::create_issue(&conn, &fold_card("amux", "doing", "**Prompt:** x", "lane"), 1000)
                .unwrap();
            let other =
                bs::create_issue(&conn, &fold_card("amux", "doing", "**Prompt:** y", "lane"), 1010)
                    .unwrap();
            assert!(
                fold_capture_for_worker_card(&conn, &other, 600, 1010).unwrap().is_none(),
                "a capture card must not fold another capture"
            );
            assert_eq!(bs::get_issue(&conn, &cap.id).unwrap().unwrap().status, "doing");
        }
        // (b) a capture in a DIFFERENT lane is not this worker's to fold.
        {
            let conn = fold_db();
            let cap = bs::create_issue(&conn, &fold_card("amux", "doing", "**Prompt:** x", "laneA"), 1000)
                .unwrap();
            let worker =
                bs::create_issue(&conn, &fold_card("laneB", "todo", "Fix", "laneB"), 1010).unwrap();
            assert!(fold_capture_for_worker_card(&conn, &worker, 600, 1010).unwrap().is_none());
            assert_eq!(
                bs::get_issue(&conn, &cap.id).unwrap().unwrap().status,
                "doing",
                "another lane's capture is untouched"
            );
        }
        // (c) a capture older than the fold window is not the worker's current
        // prompt — it stays for its lane rather than being swallowed.
        {
            let conn = fold_db();
            let cap = bs::create_issue(&conn, &fold_card("amux", "doing", "**Prompt:** x", "lane"), 100)
                .unwrap();
            let worker =
                bs::create_issue(&conn, &fold_card("lane", "todo", "Fix", "lane"), 2000).unwrap();
            assert!(
                fold_capture_for_worker_card(&conn, &worker, 600, 2000).unwrap().is_none(),
                "a capture older than the window is not this prompt"
            );
            assert_eq!(bs::get_issue(&conn, &cap.id).unwrap().unwrap().status, "doing");
        }
    }

    /// The mis-fold guard (the NOT EXISTS clause): if a worker has ALREADY carded
    /// work after a capture, a SECOND, distinct worker card must not swallow that
    /// capture — otherwise an unrelated task would absorb the prompt's card. This
    /// exercises the guard directly with the capture still `doing`.
    #[test]
    fn a_second_worker_card_does_not_fold_an_already_owned_capture() {
        let conn = fold_db();
        let cap = bs::create_issue(&conn, &fold_card("amux", "doing", "**Prompt:** x", "lane"), 1000)
            .unwrap();
        // A prior worker card for the lane already exists after the capture.
        let _prior =
            bs::create_issue(&conn, &fold_card("lane", "doing", "Fix A", "lane"), 1010).unwrap();
        let newer =
            bs::create_issue(&conn, &fold_card("lane", "todo", "Fix B", "lane"), 1020).unwrap();
        assert!(
            fold_capture_for_worker_card(&conn, &newer, 600, 1020).unwrap().is_none(),
            "a capture a prior worker card already owns must not be re-folded"
        );
        assert_eq!(bs::get_issue(&conn, &cap.id).unwrap().unwrap().status, "doing");
    }

    /// Bulk helpers are work inside a task, not new tasks. The receipt must
    /// land on the uniquely active card and emit the same task-update event a
    /// dashboard already knows how to consume.
    #[test]
    fn helper_activity_attaches_to_the_active_task_and_emits_its_snapshot() {
        let conn = fold_db();
        let card =
            bs::create_issue(&conn, &fold_card("amux", "doing", "Current task", "lane"), 1000)
                .unwrap();
        let (receipt, event) = record_session_task_activity(
            &conn,
            "lane",
            None,
            "delegated bulk read completed: 2 file(s), 900 line(s), via haiku, 12ms",
            2000,
        )
        .unwrap();

        assert_eq!(receipt.verdict, "attached");
        assert_eq!(receipt.card_id.as_deref(), Some(card.id.as_str()));
        assert_eq!(receipt.n_considered, 1);
        let updated = bs::get_issue(&conn, &card.id).unwrap().unwrap();
        assert!(updated.log.unwrap().contains("delegated bulk read completed"));
        assert_eq!(updated.updated, 2000, "the activity must move board recency");
        let event = event.expect("a visible task mutation needs a realtime event");
        assert_eq!(event.entity_type, EntityType::Task);
        assert_eq!(event.entity_id, card.id);
        assert!(event.payload.is_some(), "replay needs the post-write task snapshot");
    }

    /// Negative controls: no parent means an honest no-op; several candidates
    /// are refused rather than writing evidence onto whichever row sorted first.
    #[test]
    fn helper_activity_never_invents_or_guesses_a_parent_task() {
        let conn = fold_db();
        let (none, event) =
            record_session_task_activity(&conn, "lane", None, "helper ran", 2000).unwrap();
        assert_eq!(none.verdict, "no_active_task");
        assert_eq!(none.n_considered, 0);
        assert!(event.is_none());

        let a = bs::create_issue(&conn, &fold_card("lane", "doing", "A", "lane"), 1000)
            .unwrap();
        let b = bs::create_issue(&conn, &fold_card("lane", "doing", "B", "lane"), 1001)
            .unwrap();
        let (ambiguous, event) =
            record_session_task_activity(&conn, "lane", None, "wrong card", 2000).unwrap();
        assert_eq!(ambiguous.verdict, "task_request_parent_ambiguous");
        assert_eq!(ambiguous.n_considered, 2);
        assert!(event.is_none());
        for id in [a.id, b.id] {
            let row = bs::get_issue(&conn, &id).unwrap().unwrap();
            assert!(!row.log.unwrap_or_default().contains("wrong card"));
        }
    }
}

#[cfg(test)]
mod bulk_migrate_tests {

    /// AF-477. The 404 guard must be NARROW: only "the card does not exist"
    /// becomes a 404, and every other storage fault stays a 500.
    ///
    /// This exists because the integration test could not prove it. That test
    /// drives the real handler, so the only mutation it can reach touches the
    /// Err arm — widening the guard to catch EVERY error would send a genuine DB
    /// fault to 404 and the integration test would still pass, because its
    /// success case is an Ok. The gap was recorded on the card as unproven; this
    /// closes it by testing the classification directly.
    #[test]
    fn only_a_missing_row_is_a_missing_task() {
        // The one that IS a missing card.
        assert!(
            super::is_missing_task(&anyhow::Error::new(rusqlite::Error::QueryReturnedNoRows)),
            "QueryReturnedNoRows is how the closure signals 'no such task'"
        );
        // A DIFFERENT rusqlite error is a real fault and must stay a 500.
        assert!(
            !super::is_missing_task(&anyhow::Error::new(
                rusqlite::Error::ExecuteReturnedResults
            )),
            "a genuine storage fault must NOT be reported to the caller as 404"
        );
        // And a non-rusqlite error must not be swallowed either.
        assert!(
            !super::is_missing_task(&anyhow::anyhow!("pool exhausted")),
            "an error that is not a rusqlite error at all must stay a 500"
        );
    }
    use super::*;

    /// AMUX-4044. The safety property of bulk migrate is that a GATED column
    /// cannot be filled in one click, so this is the cell that has to hold.
    ///
    /// Measured on the live board when this shipped: `doing`, `review`, `done`
    /// and `verified` carry gates; `backlog`, `todo` and `discarded` do not.
    /// One acknowledgement across 489 backlog cards would assert all four of
    /// `verified`'s criteria about work nobody opened, which is the claim
    /// AF-321 exists to refuse.
    /// AMUX-4044. The response must say WHY a whole column refused.
    ///
    /// Built from the live specimen: `todo -> done` returned HTTP 200 with
    /// `considered: 147, moved: 0` and 147 identical
    /// `GateBlocked{criteria:["Implemented and merged","Tests / lint pass"]}`
    /// refusals. Every card was correctly stopped by `advance` — the belt held
    /// — but the caller got a success with a zero in it and had to infer the
    /// reason from an array.
    #[test]
    fn a_column_that_unanimously_refuses_a_gate_is_reported_as_a_gate() {
        let gb = |id: &str| {
            json!({"id": id, "why": "GateBlocked { criteria: [\"Implemented and merged\"] }"})
        };

        // THE SPECIMEN, shrunk: every considered card refused the same gate.
        let all = vec![gb("A-1"), gb("A-2"), gb("A-3")];
        assert!(
            unanimous_gate(3, 0, &all).is_some(),
            "a column where every card refused one gate must be reported as gated"
        );

        // PARTIAL SUCCESS IS NOT A GATE. If anything moved, the operation did
        // what it said and the refusals are per-card business.
        assert_eq!(unanimous_gate(3, 1, &all[..2]), None, "a partial move is not a gate refusal");

        // MIXED REASONS MUST NOT FLATTEN. One gate message over a column whose
        // cards failed for different reasons hides everything but the first.
        let mixed = vec![gb("A-1"), json!({"id": "A-2", "why": "Stale { actual: \"doing\" }"})];
        assert_eq!(
            unanimous_gate(2, 0, &mixed),
            None,
            "different refusals are a mixed result, not one gate"
        );

        // A non-gate unanimous refusal is also not a gate.
        let stale = vec![json!({"id": "A-1", "why": "Stale { actual: \"doing\" }"})];
        assert_eq!(unanimous_gate(1, 0, &stale), None, "only GateBlocked reads as a gate");

        // Nothing considered is nothing to report.
        assert_eq!(unanimous_gate(0, 0, &[]), None, "an empty column is not a gate refusal");
    }

}
