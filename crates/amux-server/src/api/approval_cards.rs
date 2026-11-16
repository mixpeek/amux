//! Approvals are Needs-you cards (governance PR 12).
//!
//! WHAT THIS IS. Five things wait on a person: a held external email
//! (email_approval.rs), a spend grant (grants.rs, from a vault charge), a
//! publish request (public_links.rs), an owner-gated land (land_approvals.rs)
//! and a capability ask (capabilities.rs). Each used to surface its own way, or
//! not at all. Now each files ONE `needsyou` card (A1: the board is the
//! surface, there is no approval primitive), and resolving that card from the
//! board or the phone calls the item's OWN approve or reject path with the
//! caller's proven identity. There is no second approval store.
//!
//! ONE SOURCE PER FACT (A10). `_amux_approval_cards` only links an item to its
//! card. Whether the item is approved stays in the item's own record (the
//! email draft, the grant file, the link, the land record, the capability env).
//! Who decided is read from the ledger row of the deciding request (its
//! `span_id`), not copied here. Every kind's own endpoint resolves the card on
//! success, so the direct endpoints and the card can never disagree, and the
//! board refuses to move an undecided approval card any other way.
//!
//! SEPARATION OF DUTIES. Workers and apps never decide. A member never decides
//! their own request, and a scoped member decides only for workers in scope.
//! The kinds keep their own checks too (creator-cannot-approve on email).
//!
//! STANDING APPROVALS. "Approve and remember" on a decided card writes a row in
//! the existing `standing_approvals` table with an exact, structured key
//! (`source = approval:<kind>`, `require_terms = <key>`): the same recipients
//! for an email, the same merchant up to the same amount for a spend, the same
//! kind of link for a publish. The next identical request goes through without
//! a card, and the use is logged in `standing_approval_uses`. These rows are
//! applied by the server, so they are not copied into MEMORY.md or matched by
//! keyword. Land (one exact diff per approval) and capabilities (a grant is
//! already permanent) cannot be remembered.
//!
//! OFF SWITCH (A11): `AMUX_APPROVAL_CARDS=off` files no cards and leaves every
//! kind's own endpoint exactly as it was.

use std::path::Path;
use std::sync::OnceLock;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use super::AppState;

pub const MODE_ENV: &str = "AMUX_APPROVAL_CARDS";
const CREATOR: &str = "approval-cards";
/// Kinds a standing approval can answer.
const REMEMBERABLE: [&str; 3] = ["email", "grant", "publish"];

static STORE: OnceLock<crate::db::SharedStore> = OnceLock::new();

pub fn install_store(store: crate::db::SharedStore) {
    if STORE.set(store).is_err() {
        tracing::debug!("store already installed; keeping the first");
    }
}

pub fn enabled() -> bool {
    !matches!(
        std::env::var(MODE_ENV).unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "off" | "0" | "false" | "no"
    )
}

pub mod counters {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    static C: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
    pub fn bump(key: &str) {
        if let Ok(mut m) = C.lock() {
            *m.entry(key.to_string()).or_insert(0) += 1;
        }
    }
    pub fn snapshot() -> BTreeMap<String, u64> {
        C.lock().map(|m| m.clone()).unwrap_or_default()
    }
}

pub fn approval_ref(kind: &str, item_id: &str) -> String {
    format!("{kind}:{item_id}")
}

/// What a card asks. `meta` carries what the card's actions need later.
#[derive(Clone, Debug)]
pub struct Ask {
    pub kind: &'static str,
    pub item_id: String,
    pub requester: String,
    pub title: String,
    pub question: String,
    pub unblocks: String,
    pub desc: String,
    pub ask_type: &'static str,
    pub expires_at: Option<f64>,
    pub meta: Value,
}

/// A linked item, as stored.
#[derive(Clone, Debug)]
pub struct Link {
    pub approval_ref: String,
    pub kind: String,
    pub item_id: String,
    pub card_id: Option<String>,
    pub requester: String,
    pub meta: Value,
    pub expires_at: Option<f64>,
    pub decided_at: Option<f64>,
    pub decision: Option<String>,
}

fn row_to_link(r: &rusqlite::Row<'_>) -> rusqlite::Result<Link> {
    let meta: String = r.get(5)?;
    Ok(Link {
        approval_ref: r.get(0)?,
        kind: r.get(1)?,
        item_id: r.get(2)?,
        card_id: r.get(3)?,
        requester: r.get(4)?,
        meta: serde_json::from_str(&meta).unwrap_or_else(|_| json!({})),
        expires_at: r.get(6)?,
        decided_at: r.get(7)?,
        decision: r.get(8)?,
    })
}

const LINK_COLS: &str =
    "approval_ref, kind, item_id, card_id, requester, meta, expires_at, decided_at, decision";

pub fn get_in(conn: &Connection, kind: &str, item_id: &str) -> rusqlite::Result<Option<Link>> {
    conn.query_row(
        &format!("SELECT {LINK_COLS} FROM _amux_approval_cards WHERE approval_ref = ?1"),
        [approval_ref(kind, item_id)],
        row_to_link,
    )
    .optional()
}

pub fn for_card_in(conn: &Connection, card: &str) -> rusqlite::Result<Option<Link>> {
    conn.query_row(
        &format!("SELECT {LINK_COLS} FROM _amux_approval_cards WHERE card_id = ?1"),
        [card],
        row_to_link,
    )
    .optional()
}

fn read_link(kind: &str, item_id: &str) -> Option<Link> {
    let store = STORE.get()?;
    let conn = store.read().ok()?;
    get_in(&conn, kind, item_id).ok().flatten()
}

fn new_issue(ask: &Ask) -> crate::db::board_store::NewIssue {
    let short = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    crate::db::board_store::NewIssue {
        title: short(&ask.title, 180),
        desc: format!(
            "{}\n\nApproval: {} (decide it here: Approve or Reject. Moving this card any other way is refused, \
             so the card and the {} record cannot disagree.)",
            ask.desc,
            approval_ref(ask.kind, &ask.item_id),
            ask.kind
        ),
        status: "needsyou".into(),
        session: None,
        item_type: "decision".into(),
        creator: CREATOR.into(),
        owner_type: "human".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags: vec!["approval".into(), format!("approval:{}", ask.kind)],
        ask_type: Some(ask.ask_type.into()),
        next_action: None,
        acceptance_criteria: None,
        ask_question: Some(short(&ask.question, 300)),
        ask_unblocks: Some(short(&ask.unblocks, 300)),
        ask_actor: Some(crate::api::turn_end::owner_name()),
        source: Some(format!("approval:{}", ask.kind)),
        requested_by: (!ask.requester.is_empty()).then(|| ask.requester.clone()),
        callback_session: None,
        callback_prompt: None,
    }
}

/// Link `ask` and file its card, once. Returns the new card id, or `None` when
/// the item already has an undecided card (dedupe on the approval ref). An
/// item asked again after a decision (a capability crossed again) gets a new card.
pub fn file_in(conn: &Connection, ask: &Ask, now: f64) -> rusqlite::Result<Option<String>> {
    let n = conn.execute(
        "INSERT INTO _amux_approval_cards
         (approval_ref, kind, item_id, requester, meta, filed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(approval_ref) DO UPDATE SET
            card_id = excluded.card_id, requester = excluded.requester, meta = excluded.meta,
            filed_at = excluded.filed_at, expires_at = excluded.expires_at, decided_at = NULL,
            decision = NULL, reason = NULL, trace_id = NULL, span_id = NULL, standing_id = NULL
          WHERE _amux_approval_cards.decided_at IS NOT NULL",
        rusqlite::params![
            approval_ref(ask.kind, &ask.item_id),
            ask.kind,
            ask.item_id,
            ask.requester,
            ask.meta.to_string(),
            now,
            ask.expires_at
        ],
    )?;
    if n == 0 {
        return Ok(None);
    }
    let row = crate::db::board_store::create_issue(conn, &new_issue(ask), now as i64)?;
    conn.execute(
        "UPDATE _amux_approval_cards SET card_id = ?1 WHERE approval_ref = ?2",
        rusqlite::params![row.id, approval_ref(ask.kind, &ask.item_id)],
    )?;
    Ok(Some(row.id))
}

/// Link an item whose card its own module already filed (land, capability).
pub fn link_in(
    conn: &Connection,
    kind: &str,
    item_id: &str,
    card: &str,
    requester: &str,
    meta: &Value,
    now: f64,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "INSERT INTO _amux_approval_cards
         (approval_ref, kind, item_id, card_id, requester, meta, filed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(approval_ref) DO UPDATE SET
            card_id = excluded.card_id, requester = excluded.requester, meta = excluded.meta,
            filed_at = excluded.filed_at, expires_at = excluded.expires_at, decided_at = NULL,
            decision = NULL, reason = NULL, trace_id = NULL, span_id = NULL, standing_id = NULL
          WHERE _amux_approval_cards.decided_at IS NOT NULL",
        rusqlite::params![approval_ref(kind, item_id), kind, item_id, card, requester, meta.to_string(), now],
    )? > 0)
}

fn created_event(card: &str, kind: &str) -> crate::db::PendingEvent {
    crate::db::PendingEvent {
        entity_type: amux_core::revision::EntityType::Task,
        entity_id: card.to_string(),
        mutation: amux_core::revision::MutationKind::Created,
        payload: Some(json!({"id": card, "source": format!("approval:{kind}")})),
    }
}

/// File the card for `ask` (awaited). Never fails the caller: a failure is
/// counted and logged, and the item's own endpoint still works.
pub async fn file(ask: Ask) {
    if !enabled() {
        counters::bump("off");
        return;
    }
    let Some(store) = STORE.get().cloned() else {
        counters::bump("unfiled");
        return;
    };
    let kind = ask.kind;
    let reff = approval_ref(ask.kind, &ask.item_id);
    let made = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let made_w = made.clone();
    let res = store
        .write_async(move |conn| {
            let card = file_in(conn, &ask, crate::config::now_f64())?;
            let events = card.iter().map(|c| created_event(c, ask.kind)).collect::<Vec<_>>();
            if let Ok(mut m) = made_w.lock() {
                *m = card;
            }
            Ok(crate::db::WriteOutcome { applied: !events.is_empty(), events })
        })
        .await;
    let card = made.lock().ok().and_then(|m| m.clone());
    match (&res, &card) {
        (Ok(_), Some(id)) => {
            counters::bump(&format!("{kind}.filed"));
            tracing::info!(approval = %reff, card = %id, measured = true, n_considered = 1,
                verdict = "approval_card_filed", "an approval waits on a person as a Needs-you card");
        }
        (Ok(_), None) => counters::bump(&format!("{kind}.deduped")),
        (Err(e), _) => {
            counters::bump("file_failed");
            tracing::warn!(approval = %reff, error = %e, measured = false,
                verdict = "approval_card_unfiled", "an approval's card could not be filed");
        }
    }
}

/// [`file`] from synchronous code (the vault charge gate).
pub fn file_spawn(ask: Ask) {
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        h.spawn(file(ask));
    } else {
        counters::bump("unfiled");
    }
}

/// Link an existing card (land, capability), awaited.
pub async fn link(kind: &'static str, item_id: String, card: String, requester: String, meta: Value) {
    if !enabled() {
        return;
    }
    let Some(store) = STORE.get().cloned() else { return };
    let res = store
        .write_async(move |conn| {
            link_in(conn, kind, &item_id, &card, &requester, &meta, crate::config::now_f64())?;
            Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
        })
        .await;
    if res.is_err() {
        counters::bump("link_failed");
    } else {
        counters::bump(&format!("{kind}.linked"));
    }
}

/// Record the decision on the link and close the card. Returns the card id it
/// moved. `decision` is `approved`, `rejected` or `expired`.
#[allow(clippy::too_many_arguments)]
pub fn resolve_in(
    conn: &Connection,
    kind: &str,
    item_id: &str,
    decision: &str,
    by: &str,
    reason: Option<&str>,
    trace: Option<(&str, &str)>,
    standing_id: Option<i64>,
    now: f64,
) -> rusqlite::Result<Option<String>> {
    let reff = approval_ref(kind, item_id);
    let n = conn.execute(
        "UPDATE _amux_approval_cards
            SET decided_at = ?2, decision = ?3, reason = ?4, trace_id = ?5, span_id = ?6, standing_id = ?7
          WHERE approval_ref = ?1 AND decided_at IS NULL",
        rusqlite::params![reff, now, decision, reason, trace.map(|t| t.0), trace.map(|t| t.1), standing_id],
    )?;
    if n == 0 {
        return Ok(None);
    }
    let card: Option<String> = conn
        .query_row("SELECT card_id FROM _amux_approval_cards WHERE approval_ref = ?1", [&reff], |r| r.get(0))
        .optional()?
        .flatten();
    let Some(card) = card else { return Ok(None) };
    let dest = if decision == "approved" { "done" } else { "discarded" };
    // Its own module may already have closed it (land closes its card).
    let now_status: Option<String> = conn
        .query_row("SELECT status FROM issues WHERE id = ?1", [&card], |r| r.get(0))
        .optional()?;
    if now_status.as_deref() == Some(dest) {
        return Ok(Some(card));
    }
    let line = format!(
        "{decision} by {by}{}{}",
        reason.map(|r| format!(": {}", r.chars().take(300).collect::<String>())).unwrap_or_default(),
        standing_id.map(|s| format!(" (standing approval SA-{s})")).unwrap_or_default()
    );
    let opts = crate::db::advance::AdvanceOpts {
        gate_ack: true,
        skip_continuation: true,
        reason: Some(line.clone()),
        log_line: Some(format!("{reff} {line}")),
        ..Default::default()
    };
    match crate::db::advance::advance(conn, &card, dest, CREATOR, &opts)? {
        Ok(_) => Ok(Some(card)),
        Err(refusal) => {
            tracing::warn!(approval = %reff, card = %card, refusal = ?refusal, measured = true, n_considered = 1,
                verdict = "approval_card_unmoved", "the approval was decided but its card would not move");
            Ok(None)
        }
    }
}

/// The item was decided by its own endpoint: close its card. Awaited by the
/// kinds' handlers so the card is closed when they answer.
pub async fn resolved(kind: &'static str, item_id: &str, decision: &'static str, by: &str, reason: Option<String>) {
    resolved_with(kind, item_id, decision, by, reason, None).await
}

pub async fn resolved_with(
    kind: &'static str,
    item_id: &str,
    decision: &'static str,
    by: &str,
    reason: Option<String>,
    standing_id: Option<i64>,
) {
    if !enabled() {
        return;
    }
    let Some(store) = STORE.get().cloned() else { return };
    let trace = super::trace::current();
    let item = item_id.to_string();
    let by = by.to_string();
    let res = store
        .write_async(move |conn| {
            let t = trace.as_ref().map(|t| (t.trace_id.as_str(), t.span_id.as_str()));
            let moved = resolve_in(conn, kind, &item, decision, &by, reason.as_deref(), t, standing_id,
                crate::config::now_f64())?;
            Ok(crate::db::WriteOutcome { applied: moved.is_some(), events: vec![] })
        })
        .await;
    match res {
        Ok(_) => counters::bump(&format!("{kind}.{decision}")),
        Err(e) => {
            counters::bump("resolve_failed");
            tracing::warn!(kind, item = %item_id, error = %e, measured = false,
                verdict = "approval_card_unresolved", "an approval was decided but its card was not closed");
        }
    }
}

/// Close the cards of items whose own TTL ran out (spend grants: 1h).
pub fn expire_in(conn: &Connection, now: f64) -> rusqlite::Result<Vec<String>> {
    let due: Vec<(String, String)> = {
        let mut st = conn.prepare(
            "SELECT kind, item_id FROM _amux_approval_cards
              WHERE decided_at IS NULL AND expires_at IS NOT NULL AND expires_at < ?1",
        )?;
        let rows = st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::new();
    for (kind, item) in due {
        if let Some(card) = resolve_in(conn, &kind, &item, "expired", "its own time limit", None, None, None, now)? {
            out.push(card);
        }
    }
    Ok(out)
}

// ── Separation of duties ───────────────────────────────────────────────────

fn duty_refusal(code: &str, why: String) -> Response {
    counters::bump(&format!("duty.{code}"));
    tracing::warn!(code, why = %why, measured = true, n_considered = 1, verdict = "approval_duty_refused",
        "an approval was refused by separation of duties");
    (StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": code, "error": why}))).into_response()
}

/// Who may decide `link`: never a worker or app, never the member who asked,
/// a scoped member only for workers in scope.
pub fn duties_for(link: &Link, headers: &HeaderMap, home: &Path) -> Option<Response> {
    if super::org::policy_actor(headers).is_some() {
        return None;
    }
    if headers.contains_key(super::app_identity::ACTOR_HEADER) {
        return Some(duty_refusal("app_cannot_decide", "a published app never decides an approval".into()));
    }
    if let Some(member) = super::org::local_member_actor(headers) {
        let me = format!("member:{member}");
        if !link.requester.is_empty() && (link.requester == me || link.requester == member) {
            return Some(duty_refusal(
                "requester_cannot_approve",
                format!("{member} asked for {} and cannot also decide it", link.approval_ref),
            ));
        }
        if let Some(scope) = super::org::local_member_scope(headers).filter(|s| !s.is_global()) {
            if !link.requester.is_empty() && !link.requester.starts_with("member:") && !scope.allows_worker(&link.requester) {
                return Some(duty_refusal(
                    "out_of_scope",
                    format!("{member}'s access does not cover {}, which asked for {}", link.requester, link.approval_ref),
                ));
            }
        }
        return None;
    }
    if let Some(w) = super::capabilities::acting_worker(home, headers) {
        return Some(duty_refusal(
            "worker_cannot_decide",
            format!("{w} is a worker; only a person decides {}", link.approval_ref),
        ));
    }
    None
}

/// The kinds' handlers call this first. `None` when unlinked or allowed.
pub fn duties(kind: &str, item_id: &str, headers: &HeaderMap) -> Option<Response> {
    if !enabled() {
        return None;
    }
    let link = read_link(kind, item_id)?;
    duties_for(&link, headers, &crate::config::amux_home())
}

// ── Standing approvals ─────────────────────────────────────────────────────

/// The exact key a remembered decision matches on.
pub fn email_key(externals: &[String]) -> String {
    let mut v: Vec<String> = externals.iter().map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty()).collect();
    v.sort();
    v.dedup();
    format!("to:{}", v.join(","))
}

/// The card for a held external email. The draft stays the source; the card
/// is where a person sees and decides it. Refiling a reused draft is a no-op.
pub fn email_ask(id: &str, lane: &str, endpoint: &str, to: &str, subject: &str, externals: &[String]) -> Ask {
    Ask {
        kind: "email",
        item_id: id.to_string(),
        requester: lane.to_string(),
        title: format!("Send {lane}'s email to {}: {}", to.chars().take(80).collect::<String>(), subject.chars().take(80).collect::<String>()),
        question: format!("Send {lane}'s {endpoint} to {} (\"{}\")?", externals.join(", "), subject.chars().take(120).collect::<String>()),
        unblocks: "The email goes out exactly as drafted. Rejecting discards it; nothing is sent.".into(),
        desc: format!("{lane} drafted an external {endpoint} that is held for a person (AMUX-3510).\n\nTo: {to}\nSubject: {subject}"),
        ask_type: "customer_outbound",
        expires_at: None,
        meta: json!({"key": email_key(externals), "endpoint": endpoint}),
    }
}

pub fn grant_key(merchant: &str) -> String {
    format!("merchant:{}", merchant.trim().to_ascii_lowercase())
}

pub fn publish_key(link_kind: &str) -> String {
    format!("publish:{}", link_kind.trim().to_ascii_lowercase())
}

/// A live standing approval for exactly this request, if one exists.
pub fn standing_in(conn: &Connection, kind: &str, worker: &str, key: &str, amount: Option<f64>, now: i64)
    -> rusqlite::Result<Option<i64>> {
    let mut st = conn.prepare(
        "SELECT id, max_amount_usd, max_per_day FROM standing_approvals
          WHERE revoked = 0 AND source = ?1 AND scope = ?2 AND require_terms = ?3
            AND (expires_at IS NULL OR expires_at > ?4)
          ORDER BY id",
    )?;
    let rows = st.query_map(
        rusqlite::params![format!("approval:{kind}"), format!("worker:{worker}"), key, now],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, Option<i64>>(2)?)),
    )?;
    for row in rows {
        let (id, max_amount, max_per_day) = row?;
        if let (Some(max), Some(a)) = (max_amount, amount) {
            if a > max + 1e-9 {
                continue;
            }
        }
        if let Some(cap) = max_per_day {
            if super::standing_approvals::uses_in_last_day(conn, id, now) >= cap {
                continue;
            }
        }
        return Ok(Some(id));
    }
    Ok(None)
}

pub fn note_use_in(conn: &Connection, id: i64, worker: &str, reff: &str, ask: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO standing_approval_uses (approval_id, ts, session, door, verdict, ask, reference)
         VALUES (?1, ?2, ?3, 'approval', 'applied', ?4, ?5)",
        rusqlite::params![id, now, worker, ask.chars().take(300).collect::<String>(), reff],
    )?;
    Ok(())
}

/// Does a remembered decision cover this request? On a match the use is
/// logged (awaited) and its id returned.
pub async fn standing_use(kind: &'static str, worker: &str, key: &str, amount: Option<f64>, reff: &str, ask: &str) -> Option<i64> {
    if !enabled() {
        return None;
    }
    let store = STORE.get()?.clone();
    let now = crate::config::now_f64() as i64;
    let id = {
        let conn = store.read().ok()?;
        standing_in(&conn, kind, worker, key, amount, now).ok().flatten()?
    };
    let (w, r, a) = (worker.to_string(), reff.to_string(), ask.to_string());
    if let Err(e) = store
        .write_async(move |conn| {
            note_use_in(conn, id, &w, &r, &a, now)?;
            Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
        })
        .await {
        tracing::warn!(error = %e, verdict = "approval_standing_use_unrecorded", "the use of a remembered decision was not recorded");
    }
    counters::bump(&format!("{kind}.standing_applied"));
    tracing::info!(kind, worker, standing = id, measured = true, n_considered = 1,
        verdict = "approval_standing_applied", "a remembered decision answered this request; no card filed");
    Some(id)
}

/// [`standing_use`] from synchronous code.
pub fn standing_use_sync(kind: &'static str, worker: &str, key: &str, amount: Option<f64>, reff: &str, ask: &str) -> Option<i64> {
    if !enabled() {
        return None;
    }
    let store = STORE.get()?.clone();
    let now = crate::config::now_f64() as i64;
    let id = {
        let conn = store.read().ok()?;
        standing_in(&conn, kind, worker, key, amount, now).ok().flatten()?
    };
    let (w, r, a) = (worker.to_string(), reff.to_string(), ask.to_string());
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        h.spawn(async move {
            if let Err(e) = store
                .write_async(move |conn| {
                    note_use_in(conn, id, &w, &r, &a, now)?;
                    Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
                })
                .await {
                tracing::warn!(error = %e, verdict = "approval_standing_use_unrecorded", "the use of a remembered decision was not recorded");
            }
        });
    }
    counters::bump(&format!("{kind}.standing_applied"));
    tracing::info!(kind, worker, standing = id, measured = true, n_considered = 1,
        verdict = "approval_standing_applied", "a remembered decision answered this request; no grant filed");
    Some(id)
}

/// Write the standing approval for an approved `link`.
pub fn remember_in(conn: &Connection, link: &Link, granted_by: &str, until: Option<i64>, now: i64)
    -> Result<i64, String> {
    if !REMEMBERABLE.contains(&link.kind.as_str()) {
        return Err(format!(
            "a {} approval cannot be remembered: {}",
            link.kind,
            if link.kind == "land" { "each owner-gated diff is approved on its own" }
            else { "granting a capability is already permanent" }
        ));
    }
    if link.decision.as_deref() != Some("approved") {
        return Err(format!("{} is not approved", link.approval_ref));
    }
    let worker = link.requester.trim();
    if worker.is_empty() || worker.starts_with("member:") {
        return Err("only a worker's request can be remembered".into());
    }
    let key = link.meta["key"].as_str().unwrap_or_default().to_string();
    if key.is_empty() {
        return Err(format!("{} has no match key", link.approval_ref));
    }
    let (category, allowed, amount) = match link.kind.as_str() {
        "email" => ("customer_outbound", format!("{worker} may email {} without asking", key.trim_start_matches("to:")), None),
        "grant" => {
            let amt = link.meta["amount_usd"].as_f64();
            ("budget", format!("{worker} may charge {} up to ${:.2} without asking",
                key.trim_start_matches("merchant:"), amt.unwrap_or(0.0)), amt)
        }
        _ => ("access", format!("{worker} may publish a {} link without asking", key.trim_start_matches("publish:")), None),
    };
    conn.execute(
        "INSERT INTO standing_approvals
         (title, allowed, category, limits, max_amount_usd, require_terms, scope, granted_by, granted_at, source, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            format!("Always: {} for {worker}", link.kind),
            allowed,
            category,
            format!("exactly {key}; from {}", link.approval_ref),
            amount,
            key,
            format!("worker:{worker}"),
            granted_by,
            now,
            format!("approval:{}", link.kind),
            until
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

// ── Deciding a card ────────────────────────────────────────────────────────

fn bad(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"ok": false, "error": msg}))).into_response()
}

/// Call the item's OWN approve or reject path with the caller's headers.
pub async fn dispatch(state: &AppState, headers: HeaderMap, link: &Link, approve: bool, body: Value) -> Response {
    let id = link.item_id.clone();
    match link.kind.as_str() {
        "email" => {
            let ctx = std::sync::Arc::new(super::email::EmailCtx {
                client: std::sync::Arc::new(crate::integrations::email::GmailClient::new_default()),
                registry: crate::integrations::global_registry().clone(),
            });
            let b = (!body.is_null() && body.as_object().is_some_and(|o| !o.is_empty())).then_some(Json(body));
            if approve {
                super::email::approve(axum::Extension(ctx), headers, AxPath(id), b).await
            } else {
                super::email::reject(axum::Extension(ctx), headers, AxPath(id), b).await
            }
        }
        "grant" => {
            if approve {
                super::grants::approve(headers, AxPath(id)).await
            } else {
                super::grants::reject(headers, AxPath(id)).await
            }
        }
        "publish" => {
            if approve {
                super::static_files::public_links::approve(State(state.clone()), headers, AxPath(id)).await
            } else {
                super::static_files::public_links::revoke(State(state.clone()), headers, AxPath(id)).await
            }
        }
        "land" => {
            let uri = Uri::from_static("/api/land/approvals");
            let mut b = json!({"patch_id": id});
            if let Some(n) = body.get("reason").and_then(Value::as_str) {
                b["note"] = json!(n);
            }
            if approve {
                super::land_approvals::approve_route(State(state.clone()), headers, uri, Json(b)).await
            } else {
                super::land_approvals::reject_route(State(state.clone()), headers, uri, Json(b)).await
            }
        }
        "capability" => {
            let (lane, cap) = id.split_once(':').unwrap_or((id.as_str(), ""));
            if !approve {
                // Keeping the refusal: the ask stays recorded, so the worker
                // is not asked about again until a grant clears it.
                let by = super::human_proof::proven(&headers).map(|p| p.actor()).unwrap_or_else(|| "unproven".into());
                let reason = body.get("reason").and_then(Value::as_str).map(str::to_string);
                resolved("capability", &id, "rejected", &by, reason).await;
                return Json(json!({"ok": true, "rejected": link.approval_ref})).into_response();
            }
            let Some(write) = link.meta.get("grant").filter(|g| g.is_object()).cloned() else {
                return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "grant_by_hand",
                    "error": format!("granting '{cap}' to {lane} is not a single setting"),
                    "next": link.meta.get("hint").cloned().unwrap_or(Value::Null)}))).into_response();
            };
            let mut req = axum::http::Request::builder().method("PUT").uri("/api/scope");
            for (k, v) in headers.iter() {
                req = req.header(k, v);
            }
            let req = req
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({"level": "worker", "name": lane, "capability": "env", "value": write}).to_string(),
                ));
            match req {
                Ok(req) => super::scope::handler(State(state.clone()), req).await,
                Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not build the grant request: {e}")),
            }
        }
        other => bad(StatusCode::BAD_REQUEST, &format!("unknown approval kind '{other}'")),
    }
}

#[derive(serde::Deserialize)]
pub struct DecideBody {
    approval_ref: String,
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    remember: Option<Value>,
    #[serde(default)]
    body: Option<Value>,
}

/// POST /api/approvals/decide {approval_ref, decision: approve|reject, reason?,
/// remember?: {until?: epoch_s} | true, body?: the kind's own body}
async fn decide(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<DecideBody>) -> Response {
    let approve = match b.decision.as_str() {
        "approve" => true,
        "reject" | "decline" => false,
        _ => return bad(StatusCode::BAD_REQUEST, "decision must be approve or reject"),
    };
    let Some((kind, item)) = b.approval_ref.split_once(':') else {
        return bad(StatusCode::BAD_REQUEST, "approval_ref is <kind>:<id>");
    };
    let link = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return bad(StatusCode::SERVICE_UNAVAILABLE, &e.to_string()),
        };
        get_in(&conn, kind, item).ok().flatten()
    };
    let Some(link) = link else {
        return bad(StatusCode::NOT_FOUND, "no such approval card");
    };
    if let Some(r) = duties_for(&link, &headers, &crate::config::amux_home()) {
        return r;
    }
    if link.decided_at.is_some() {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "already_decided",
            "decision": link.decision, "approval_ref": link.approval_ref}))).into_response();
    }
    let remember = b.remember.clone().filter(|v| !matches!(v, Value::Bool(false) | Value::Null));
    if remember.is_some() {
        if !approve {
            return bad(StatusCode::BAD_REQUEST, "only an approval can be remembered");
        }
        if !REMEMBERABLE.contains(&link.kind.as_str()) {
            return bad(StatusCode::BAD_REQUEST, &format!("a {} approval cannot be remembered", link.kind));
        }
    }
    let mut body = b.body.clone().unwrap_or(Value::Null);
    if let Some(r) = &b.reason {
        if body.is_null() {
            body = json!({});
        }
        body["reason"] = json!(r);
    }
    let resp = dispatch(&state, headers.clone(), &link, approve, body).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 512 * 1024).await.unwrap_or_default();
    let mut out: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        counters::bump(&format!("{}.decide_refused", link.kind));
        return (status, Json(out)).into_response();
    }
    // A capability grant goes through the scope write, which clears the ask;
    // close its card here (the other kinds close their own).
    if link.kind == "capability" && approve {
        let by = super::human_proof::proven(&headers).map(|p| p.actor()).unwrap_or_else(|| "unproven".into());
        resolved("capability", &link.item_id, "approved", &by, b.reason.clone()).await;
    }
    if let Some(rem) = remember {
        let granted_by = super::human_proof::proven(&headers).map(|p| p.actor()).unwrap_or_else(|| "unproven".into());
        let until = rem.get("until").and_then(Value::as_i64);
        let (kind, item) = (link.kind.clone(), link.item_id.clone());
        let made_cell = std::sync::Arc::new(std::sync::Mutex::new(None::<Result<i64, String>>));
        let made_w = made_cell.clone();
        let res = state
            .store
            .write_async(move |conn| {
                let fresh = get_in(conn, &kind, &item)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                let made = remember_in(conn, &fresh, &granted_by, until, crate::config::now_f64() as i64);
                if let Ok(id) = &made {
                    conn.execute("UPDATE _amux_approval_cards SET standing_id = ?2 WHERE approval_ref = ?1",
                        rusqlite::params![fresh.approval_ref, id])?;
                }
                if let Ok(mut m) = made_w.lock() {
                    *m = Some(made);
                }
                Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
            })
            .await;
        let made = made_cell.lock().ok().and_then(|mut m| m.take());
        match (res, made) {
            (Ok(_), Some(Ok(id))) => {
                counters::bump(&format!("{}.remembered", link.kind));
                out["standing_approval"] = json!(id);
            }
            (_, Some(Err(e))) => out["standing_approval_error"] = json!(e),
            (Err(e), _) => out["standing_approval_error"] = json!(e.to_string()),
            _ => out["standing_approval_error"] = json!("not recorded"),
        }
    }
    out["approval_ref"] = json!(link.approval_ref);
    out["card"] = json!(link.card_id);
    (status, Json(out)).into_response()
}

/// The board's PATCH of a linked card: an undecided approval moves only
/// through its own decision. `done` approves, `discarded` rejects, anything
/// else is refused with the way to decide it.
pub async fn board_move(state: &AppState, headers: &HeaderMap, card: &str, to: &str, reason: Option<String>)
    -> Option<Response> {
    if !enabled() {
        return None;
    }
    let link = {
        let conn = state.store.read().ok()?;
        for_card_in(&conn, card).ok().flatten()?
    };
    if link.decided_at.is_some() {
        return None;
    }
    let approve = match to {
        "done" | "verified" => true,
        "discarded" => false,
        _ => {
            counters::bump("board_move_refused");
            return Some((StatusCode::CONFLICT, Json(json!({
                "ok": false, "code": "approval_card",
                "error": format!("{card} is the approval {}; decide it rather than move it", link.approval_ref),
                "approve": {"method": "POST", "path": "/api/approvals/decide", "body": {"approval_ref": link.approval_ref, "decision": "approve"}},
                "reject": {"method": "POST", "path": "/api/approvals/decide", "body": {"approval_ref": link.approval_ref, "decision": "reject"}},
                "or": "PATCH status=done approves it, status=discarded rejects it",
            }))).into_response());
        }
    };
    if let Some(r) = duties_for(&link, headers, &crate::config::amux_home()) {
        return Some(r);
    }
    let body = reason.map(|r| json!({"reason": r})).unwrap_or(Value::Null);
    let resp = dispatch(state, headers.clone(), &link, approve, body).await;
    if resp.status().is_success() && link.kind == "capability" && approve {
        let by = super::human_proof::proven(headers).map(|p| p.actor()).unwrap_or_else(|| "unproven".into());
        resolved("capability", &link.item_id, "approved", &by, None).await;
    }
    counters::bump(&format!("{}.board_{}", link.kind, if approve { "approve" } else { "reject" }));
    Some(resp)
}

// ── Reading ────────────────────────────────────────────────────────────────

/// Who decided, from the ledger row of the deciding request (A10).
fn decider_in(conn: &Connection, span: &str) -> Option<(String, String)> {
    conn.query_row(
        "SELECT actor, COALESCE(authority,'') FROM _amux_ledger WHERE span_id = ?1 ORDER BY seq LIMIT 1",
        [span],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn view_in(conn: &Connection, open_only: bool, limit: i64) -> rusqlite::Result<Value> {
    let mut st = conn.prepare(&format!(
        "SELECT {LINK_COLS}, filed_at, reason, trace_id, span_id, standing_id FROM _amux_approval_cards
          {} ORDER BY filed_at DESC LIMIT ?1",
        if open_only { "WHERE decided_at IS NULL" } else { "" }
    ))?;
    let rows = st.query_map([limit], |r| {
        Ok((row_to_link(r)?, r.get::<_, f64>(9)?, r.get::<_, Option<String>>(10)?, r.get::<_, Option<String>>(11)?,
            r.get::<_, Option<String>>(12)?, r.get::<_, Option<i64>>(13)?))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (l, filed, reason, trace_id, span, standing) = row?;
        let (decided_by, authority, verified, measured) = match (&l.decided_at, &span) {
            (Some(_), Some(s)) => match decider_in(conn, s) {
                Some((actor, auth)) => {
                    let v = actor == "owner" || actor.starts_with("member:") || actor.starts_with("policy:")
                        || actor == super::auth::OWNER_TOKEN_ACTOR;
                    (json!(actor), json!(auth), json!(v), true)
                }
                None => (Value::Null, Value::Null, Value::Null, false),
            },
            (Some(_), None) => (Value::Null, Value::Null, Value::Null, standing.is_some() || l.decision.as_deref() == Some("expired")),
            _ => (Value::Null, Value::Null, Value::Null, true),
        };
        items.push(json!({
            "approval_ref": l.approval_ref, "kind": l.kind, "item_id": l.item_id, "card": l.card_id,
            "requester": l.requester, "filed_at": filed, "expires_at": l.expires_at,
            "decided_at": l.decided_at, "decision": l.decision, "reason": reason,
            "decided_by": decided_by, "authority": authority, "approver_verified": verified,
            "decider_measured": measured, "trace_id": trace_id, "span_id": span, "standing_approval": standing,
            "remember": REMEMBERABLE.contains(&l.kind.as_str()),
        }));
    }
    let n = items.len();
    Ok(json!({"items": items, "measured": true, "n_considered": n,
        "note": "decided_by is the ledger actor of the deciding request (joined on span_id); null with decider_measured=false means that ledger row was not found"}))
}

#[derive(serde::Deserialize)]
struct ListQuery {
    #[serde(default)]
    open: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"measured": false, "error": e.to_string()}))).into_response(),
    };
    let open = q.open.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"));
    match view_in(&conn, open, q.limit.unwrap_or(100).clamp(1, 500)) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"measured": false, "error": e.to_string()}))).into_response(),
    }
}

/// The triage queue's view of linked cards: each card item learns its
/// approval; an email's card folds into the email item (one ask, not two).
pub fn annotate_in(conn: &Connection, items: &mut Vec<Value>) {
    let Ok(mut st) = conn.prepare(&format!("SELECT {LINK_COLS} FROM _amux_approval_cards WHERE decided_at IS NULL")) else {
        return;
    };
    let Ok(rows) = st.query_map([], row_to_link) else { return };
    let links: Vec<Link> = rows.filter_map(Result::ok).collect();
    let mut email_cards: Map<String, Value> = Map::new();
    for l in &links {
        if l.kind == "email" {
            if let Some(c) = &l.card_id {
                email_cards.insert(l.item_id.clone(), json!(c));
            }
        }
    }
    let approval = |l: &Link| json!({"ref": l.approval_ref, "kind": l.kind, "id": l.item_id,
        "remember": REMEMBERABLE.contains(&l.kind.as_str()), "expires_at": l.expires_at});
    items.retain(|it| {
        let card = it["card"].as_str().unwrap_or("");
        !(it["kind"] == "card" && links.iter().any(|l| l.kind == "email" && l.card_id.as_deref() == Some(card)))
    });
    for it in items.iter_mut() {
        if it["kind"] == "email" {
            let id = it["approval_id"].as_str().unwrap_or("").to_string();
            if let Some(l) = links.iter().find(|l| l.kind == "email" && l.item_id == id) {
                it["approval"] = approval(l);
                if let Some(c) = email_cards.get(&id) {
                    it["card"] = c.clone();
                }
            }
            continue;
        }
        let card = it["card"].as_str().unwrap_or("").to_string();
        if let Some(l) = links.iter().find(|l| l.card_id.as_deref() == Some(card.as_str())) {
            it["approval"] = approval(l);
        }
    }
}

pub fn report() -> Value {
    json!({"mode": if enabled() { "on" } else { "off" }, "measured": true, "counts": counters::snapshot()})
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/approvals/decide", axum::routing::post(decide))
        .route("/api/approvals/cards", axum::routing::get(list))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A migrated database and a writable connection to it (the store's
    /// pooled connections are read-only; its writer owns the writes).
    fn conn() -> (tempfile::TempDir, crate::db::Store, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ac.db");
        let store = crate::db::Store::open(&path).unwrap();
        let c = Connection::open(&path).unwrap();
        (dir, store, c)
    }

    fn ask(kind: &'static str, id: &str, requester: &str, meta: Value) -> Ask {
        Ask {
            kind,
            item_id: id.into(),
            requester: requester.into(),
            title: format!("Approve {kind} {id}"),
            question: format!("Approve {kind} {id}?"),
            unblocks: "Approving lets the worker continue.".into(),
            desc: "test".into(),
            ask_type: "decision",
            expires_at: None,
            meta,
        }
    }

    fn status(c: &Connection, card: &str) -> String {
        c.query_row("SELECT status FROM issues WHERE id = ?1", [card], |r| r.get(0)).unwrap()
    }

    #[test]
    fn one_card_per_item_and_the_decision_closes_it() {
        let (_d, _store, c) = conn();
        let a = ask("grant", "g1", "wexus", json!({"key": "merchant:acme", "amount_usd": 20.0}));
        let card = file_in(&c, &a, 100.0).unwrap().expect("first ask files a card");
        assert_eq!(status(&c, &card), "needsyou");
        assert!(file_in(&c, &a, 101.0).unwrap().is_none(), "the same item never files a second card");
        let n: i64 = c.query_row("SELECT COUNT(*) FROM issues WHERE source = 'approval:grant'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let moved = resolve_in(&c, "grant", "g1", "approved", "owner-token", None, Some(("t", "s")), None, 102.0).unwrap();
        assert_eq!(moved.as_deref(), Some(card.as_str()));
        assert_eq!(status(&c, &card), "done");
        assert!(resolve_in(&c, "grant", "g1", "rejected", "x", None, None, None, 103.0).unwrap().is_none(),
            "a decided item is never decided twice");
        assert_eq!(status(&c, &card), "done");
        let rej = file_in(&c, &ask("publish", "p1", "wexus", json!({})), 104.0).unwrap().unwrap();
        resolve_in(&c, "publish", "p1", "rejected", "owner-token", Some("no"), None, None, 105.0).unwrap();
        assert_eq!(status(&c, &rej), "discarded");
    }

    #[test]
    fn expiry_closes_only_items_past_their_own_ttl() {
        let (_d, _store, c) = conn();
        let mut a = ask("grant", "old", "w", json!({}));
        a.expires_at = Some(50.0);
        let old = file_in(&c, &a, 10.0).unwrap().unwrap();
        let mut b = ask("grant", "new", "w", json!({}));
        b.expires_at = Some(500.0);
        let new = file_in(&c, &b, 10.0).unwrap().unwrap();
        let closed = expire_in(&c, 100.0).unwrap();
        assert_eq!(closed, vec![old.clone()]);
        assert_eq!(status(&c, &old), "discarded");
        assert_eq!(status(&c, &new), "needsyou");
    }

    fn link(kind: &str, requester: &str) -> Link {
        Link {
            approval_ref: approval_ref(kind, "x"),
            kind: kind.into(),
            item_id: "x".into(),
            card_id: Some("C-1".into()),
            requester: requester.into(),
            meta: json!({"key": "to:a@b.co"}),
            expires_at: None,
            decided_at: None,
            decision: None,
        }
    }

    #[test]
    fn workers_apps_and_the_requesting_member_never_decide() {
        let home = tempfile::tempdir().unwrap();
        let l = link("grant", "wexus");
        let mut w = HeaderMap::new();
        w.insert("x-amux-session", "rothco".parse().unwrap());
        assert!(duties_for(&l, &w, home.path()).is_some(), "a worker claim is refused");
        let mut app = HeaderMap::new();
        app.insert(super::super::app_identity::ACTOR_HEADER, "app:abc".parse().unwrap());
        assert!(duties_for(&l, &app, home.path()).is_some(), "an app is refused");
        let policy = super::super::org::policy_headers("policy:auto_approve");
        assert!(duties_for(&l, &policy, home.path()).is_none(), "an owner policy may decide within its limits");
        assert!(duties_for(&l, &HeaderMap::new(), home.path()).is_none(), "a person (no worker claim) passes to the kind's own proof check");
        // A member (the markers the member middleware installs).
        let member = |email: &str, level: &str, name: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-amux-local-member-verified", "1".parse().unwrap());
            h.insert("x-amux-local-member-actor", email.parse().unwrap());
            h.insert("x-amux-session", format!("member:{email}").parse().unwrap());
            h.insert("x-amux-local-member-scope-level", level.parse().unwrap());
            h.insert("x-amux-local-member-scope-name", name.parse().unwrap());
            h
        };
        let own = link("grant", "member:ann@x.co");
        let r = duties_for(&own, &member("ann@x.co", "global", ""), home.path());
        assert!(r.is_some_and(|r| r.status() == StatusCode::FORBIDDEN), "a member never decides their own request");
        assert!(duties_for(&l, &member("bob@x.co", "global", ""), home.path()).is_none(), "a global member decides a worker's request");
        assert!(duties_for(&l, &member("bob@x.co", "worker", "rothco"), home.path()).is_some(),
            "a member scoped to rothco cannot decide wexus's request");
        assert!(duties_for(&l, &member("bob@x.co", "worker", "wexus"), home.path()).is_none(),
            "a member scoped to wexus can");
    }

    #[test]
    fn a_remembered_approval_matches_exactly_and_logs_its_use() {
        let (_d, _store, c) = conn();
        let mut l = link("grant", "wexus");
        l.meta = json!({"key": "merchant:acme", "amount_usd": 40.0});
        assert!(remember_in(&c, &l, "owner-token", None, 1000).is_err(), "an undecided item cannot be remembered");
        l.decision = Some("approved".into());
        let id = remember_in(&c, &l, "owner-token", Some(5000), 1000).unwrap();
        assert_eq!(standing_in(&c, "grant", "wexus", "merchant:acme", Some(40.0), 2000).unwrap(), Some(id));
        assert_eq!(standing_in(&c, "grant", "wexus", "merchant:acme", Some(40.01), 2000).unwrap(), None, "above the remembered amount");
        assert_eq!(standing_in(&c, "grant", "rothco", "merchant:acme", Some(1.0), 2000).unwrap(), None, "another worker");
        assert_eq!(standing_in(&c, "grant", "wexus", "merchant:other", Some(1.0), 2000).unwrap(), None, "another merchant");
        assert_eq!(standing_in(&c, "grant", "wexus", "merchant:acme", Some(1.0), 6000).unwrap(), None, "past its end");
        note_use_in(&c, id, "wexus", "grant:g2", "charge", 2000).unwrap();
        let uses: i64 = c.query_row("SELECT COUNT(*) FROM standing_approval_uses WHERE approval_id = ?1 AND door = 'approval'",
            [id], |r| r.get(0)).unwrap();
        assert_eq!(uses, 1);
        c.execute("UPDATE standing_approvals SET revoked = 1 WHERE id = ?1", [id]).unwrap();
        assert_eq!(standing_in(&c, "grant", "wexus", "merchant:acme", Some(1.0), 2000).unwrap(), None, "revoked");
        let mut land = link("land", "wexus");
        land.decision = Some("approved".into());
        assert!(remember_in(&c, &land, "owner-token", None, 1000).is_err(), "a land approval is per diff");
    }

    #[test]
    fn an_email_card_folds_into_its_email_item() {
        let (_d, _store, c) = conn();
        let card = file_in(&c, &ask("email", "apr_1", "wexus", json!({"key": "to:a@b.co"})), 1.0).unwrap().unwrap();
        let g = file_in(&c, &ask("grant", "g9", "wexus", json!({})), 1.0).unwrap().unwrap();
        let mut items = vec![
            json!({"kind": "card", "card": card}),
            json!({"kind": "card", "card": g}),
            json!({"kind": "email", "approval_id": "apr_1", "card": ""}),
        ];
        annotate_in(&c, &mut items);
        assert_eq!(items.len(), 2, "the email's own card is not a second ask: {items:?}");
        assert_eq!(items[0]["approval"]["ref"], "grant:g9");
        assert_eq!(items[1]["approval"]["ref"], "email:apr_1");
        assert_eq!(items[1]["card"], json!(card));
    }
}
