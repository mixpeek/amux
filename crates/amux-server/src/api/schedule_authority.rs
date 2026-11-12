//! Delegated authority that outlives the delegator (governance PR 13).
//!
//! A schedule fires as its target worker long after someone created it. The
//! creator may since have left, lost scope over that worker, or the worker may
//! be gone. So:
//!
//! - **At creation** the schedule keeps the creator the server PROVED
//!   (`schedules.created_by`, `created_by_verified`), from the same resolver
//!   the ledger uses (`ledger::principal`). Rows from before this change are
//!   backfilled from `schedule_audit.by_who`, which was only ever claimed, so
//!   they stay unverified.
//! - **At every cron firing** [`gate`] re-checks that authority. A lapsed one
//!   is counted and recorded in the ledger. In `enforce` the schedule is
//!   paused (disabled with `disabled_reason='authority_lapsed'`, never
//!   deleted) and ONE Needs-you card per schedule asks the owner to re-own or
//!   delete it. `report` (the default) fires as before and only counts.
//! - **Offboarding** a member ([`offboard`]) revokes their sign-in and pauses
//!   or reassigns what they own, one ledger event per item (ethos 8: pause and
//!   report, never sweep their work away).
//!
//! `AMUX_SCHEDULE_AUTHORITY=report|enforce|off` (A11).

use crate::api::AppState;
use crate::db::{SharedStore, WriteOutcome};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const MODE_ENV: &str = "AMUX_SCHEDULE_AUTHORITY";
/// What `disabled_reason` says when the gate paused a schedule.
pub const LAPSED_REASON: &str = "authority_lapsed";
/// What `disabled_reason` says when offboarding paused a schedule.
pub const OFFBOARD_REASON: &str = "creator_offboarded";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Report,
    Enforce,
}

pub fn mode() -> Mode {
    match std::env::var(MODE_ENV).unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "off" | "0" | "false" => Mode::Off,
        "enforce" => Mode::Enforce,
        _ => Mode::Report,
    }
}

static STORE: OnceLock<SharedStore> = OnceLock::new();

pub fn install_store(store: SharedStore) {
    let _ = STORE.set(store);
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/schedules/{id}/reown", post(reown))
        .route("/api/workers/{name}/sponsor", put(set_sponsor))
        .route("/api/org/members/{email}/offboard", post(offboard))
}

pub(crate) mod counters {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    static C: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
    pub fn bump(key: &str) {
        if let Ok(mut m) = C.lock() {
            *m.entry(key.to_string()).or_default() += 1;
        }
    }
    pub fn snapshot() -> BTreeMap<String, u64> {
        C.lock().map(|m| m.clone()).unwrap_or_default()
    }
}

// ── The creator ────────────────────────────────────────────────────────────

/// The creator to stamp on a new schedule: the proven actor, and whether it
/// was proven. An unproven request keeps what it claimed, marked unverified,
/// so the row never names a person who did not prove themselves.
pub fn creator_of(home: &Path, headers: &HeaderMap) -> (String, bool) {
    let p = super::ledger::principal(home, headers);
    if p.actor == "anonymous" {
        let claimed = p.claimed.unwrap_or_else(|| "unattributed".into());
        return (claimed.chars().take(96).collect(), false);
    }
    (p.actor, true)
}

// ── The decision ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The creator still has authority over the target.
    Allow(String),
    /// The creator was never proven (a row from before this change). Fires,
    /// counted: refusing every legacy schedule would be a sweep (ethos 8).
    Unverified(String),
    /// The authority is gone; the reason names what lapsed.
    Lapsed(String),
}

fn worker_exists(home: &Path, worker: &str) -> bool {
    !worker.is_empty() && home.join("sessions").join(format!("{worker}.env")).is_file()
}

/// A member who can still sign in, and the workers they reach. `None`: no
/// such member, offboarded, or every sign-in of theirs revoked.
pub(crate) fn member_scope(conn: &Connection, email: &str) -> Option<super::org::MemberScope> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT CASE WHEN t.id IS NOT NULL THEN t.scope_level \
                         WHEN COALESCE(m.team_id,'')='' THEN m.scope_level ELSE 'worker' END, \
                    CASE WHEN t.id IS NOT NULL THEN t.scope_name \
                         WHEN COALESCE(m.team_id,'')='' THEN m.scope_name ELSE '__invalid_team_scope__' END \
             FROM org_members m LEFT JOIN org_teams t ON t.id=m.team_id \
             WHERE lower(m.email)=lower(?1) AND m.offboarded_at IS NULL \
               AND EXISTS (SELECT 1 FROM org_invites i WHERE i.used_by=m.id AND i.used_at IS NOT NULL)",
            [email],
            |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?.unwrap_or_default())),
        )
        .optional()
        .ok()
        .flatten();
    let (level, name) = row?;
    Some(super::org::parse_member_scope(&level, &name).unwrap_or(super::org::MemberScope::Worker("__invalid_member_scope__".into())))
}

/// Does `created_by` still have authority to make `target` act?
pub fn decide(conn: &Connection, home: &Path, created_by: &str, verified: bool, target: &str) -> Decision {
    if !target.is_empty() && !worker_exists(home, target) {
        return Decision::Lapsed(format!("its worker {target} no longer exists"));
    }
    if !verified {
        return Decision::Unverified(format!("created by '{created_by}', a name that was never proven"));
    }
    if created_by == "owner" || created_by.starts_with("policy:") {
        return Decision::Allow(created_by.to_string());
    }
    if let Some(email) = created_by.strip_prefix("member:") {
        return match member_scope(conn, email) {
            None => Decision::Lapsed(format!("{created_by} is no longer an active member")),
            Some(scope) if !target.is_empty() && !scope.allows_worker(target) => {
                Decision::Lapsed(format!("{created_by} no longer has scope over {target}"))
            }
            Some(_) => Decision::Allow(created_by.to_string()),
        };
    }
    if let Some(w) = created_by.strip_prefix("worker:") {
        return if worker_exists(home, w) {
            Decision::Allow(created_by.to_string())
        } else {
            Decision::Lapsed(format!("the worker that created it, {w}, no longer exists"))
        };
    }
    if let Some(id) = created_by.strip_prefix("app:") {
        let live = super::static_files::public_links::load(home).iter().any(|l| l.id == id && !l.approved_by.is_empty());
        return if live {
            Decision::Allow(created_by.to_string())
        } else {
            Decision::Lapsed(format!("the app that created it, {id}, is no longer approved"))
        };
    }
    Decision::Unverified(format!("created by '{created_by}'"))
}

/// What the cron path should do with a due schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Fire,
    /// Paused (enforce): the caller must not fire it.
    Paused,
}

/// Re-check a due schedule's authority right before it fires. Never fails the
/// scheduler: a read or write error lets the schedule fire and is counted.
pub async fn gate(store: &SharedStore, id: &str) -> Gate {
    gate_with(store, &crate::config::amux_home(), id, mode()).await
}

pub async fn gate_with(store: &SharedStore, home: &Path, id: &str, mode: Mode) -> Gate {
    if mode == Mode::Off {
        return Gate::Fire;
    }
    counters::bump("considered");
    let (rid, h) = (id.to_string(), home.to_path_buf());
    let read = store
        .read_async(move |c| {
            let row: Option<(Option<String>, i64, String)> = c
                .query_row(
                    "SELECT created_by, COALESCE(created_by_verified,0), COALESCE(session,'') FROM schedules WHERE id=?1",
                    [&rid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            Ok(row.map(|(by, v, target)| {
                let by = by.unwrap_or_else(|| "unattributed".into());
                let d = decide(c, &h, &by, v != 0, &target);
                (by, target, d)
            }))
        })
        .await;
    let (by, target, decision) = match read {
        Ok(Some(x)) => x,
        Ok(None) => return Gate::Fire,
        Err(e) => {
            counters::bump("check_failed");
            tracing::warn!(schedule = id, error = %e, measured = false, verdict = "schedule_authority_unchecked",
                "could not read a schedule's creator; firing it unchecked");
            return Gate::Fire;
        }
    };
    match decision {
        Decision::Allow(_) => {
            counters::bump("allowed");
            Gate::Fire
        }
        Decision::Unverified(why) => {
            counters::bump("unverified");
            tracing::debug!(schedule = id, why = %why, verdict = "schedule_authority_unverified", "schedule fired with an unproven creator");
            Gate::Fire
        }
        Decision::Lapsed(why) => {
            let enforce = mode == Mode::Enforce;
            counters::bump(if enforce { "skipped" } else { "would_skip" });
            tracing::warn!(schedule = id, creator = %by, target = %target, why = %why, measured = true, n_considered = 1,
                verdict = if enforce { "schedule_authority_skipped" } else { "schedule_authority_would_skip" },
                "a schedule's creator no longer has authority over its worker");
            record(store, id, &by, &target, &why, enforce).await;
            if enforce {
                pause(store, id, LAPSED_REASON, "schedule-authority").await;
                file_card(store, home, id, &by, &target, &why).await;
                Gate::Paused
            } else {
                Gate::Fire
            }
        }
    }
}

async fn record(store: &SharedStore, id: &str, by: &str, target: &str, why: &str, enforce: bool) {
    let ev = super::ledger::Event {
        event_id: format!("schedauth:{id}:{}", ulid::Ulid::new()),
        ts_ms: chrono::Utc::now().timestamp_millis(),
        kind: "schedule_authority".into(),
        actor: "policy:scheduler".into(),
        on_behalf_of: Some(by.to_string()),
        authority: Some(format!("schedule_creator:{by}")),
        action: Some("fire".into()),
        resource: Some(format!("schedule:{id} -> worker:{target}")),
        decision: Some(if enforce { "denied" } else { "would_deny" }.into()),
        outcome: Some(if enforce { format!("paused: {why}") } else { format!("fired (report): {why}") }),
        ..Default::default()
    };
    let _ = store
        .write_async(move |conn| {
            super::ledger::append(conn, &ev);
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .await;
}

/// Disable a schedule with a reason, and audit it. Pausing is reversible; the
/// row, its history and its creator stay.
async fn pause(store: &SharedStore, id: &str, reason: &str, source: &str) {
    let (rid, reason, source) = (id.to_string(), reason.to_string(), source.to_string());
    let res = store
        .write_async(move |conn| {
            let n = pause_in(conn, &rid, &reason, &source, "schedule-authority")?;
            Ok(WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        counters::bump("pause_failed");
        tracing::warn!(schedule = id, error = %e, measured = false, verdict = "schedule_authority_pause_failed",
            "could not pause a schedule whose authority lapsed");
    }
}

fn pause_in(conn: &Connection, id: &str, reason: &str, source: &str, by: &str) -> rusqlite::Result<usize> {
    let n = conn.execute(
        "UPDATE schedules SET enabled=0, disabled_reason=?1, updated=?2 WHERE id=?3 AND enabled=1 AND deleted IS NULL",
        rusqlite::params![reason, chrono::Utc::now().timestamp(), id],
    )?;
    if n > 0 {
        crate::runtime_jobs::scheduler::insert_audit(conn, id, "enabled", "1", "0", source, by)?;
    }
    Ok(n)
}

// ── The card ───────────────────────────────────────────────────────────────

fn asks_file(home: &Path) -> PathBuf {
    home.join("schedule-authority-asks.json")
}

fn read_asks(home: &Path) -> Map<String, Value> {
    std::fs::read_to_string(asks_file(home)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// A re-owned or deleted schedule may ask again if it lapses again.
pub fn clear_ask(home: &Path, id: &str) {
    let mut all = read_asks(home);
    if all.remove(id).is_some() {
        let _ = crate::integrations::secure_store::write(&asks_file(home), Value::Object(all).to_string().as_bytes());
    }
}

/// One Needs-you card per schedule: re-own it or delete it.
async fn file_card(store: &SharedStore, home: &Path, id: &str, by: &str, target: &str, why: &str) {
    let mut all = read_asks(home);
    if all.contains_key(id) {
        counters::bump("card_deduped");
        return;
    }
    all.insert(id.to_string(), json!({"creator": by, "target": target, "why": why, "asked_at": crate::config::now_f64()}));
    if crate::integrations::secure_store::write(&asks_file(home), Value::Object(all).to_string().as_bytes()).is_err() {
        counters::bump("card_failed");
        return;
    }
    let new = crate::db::board_store::NewIssue {
        title: format!("Schedule {id} was paused: {}", why.chars().take(90).collect::<String>()),
        desc: format!(
            "Schedule {id} fires as worker {target}. It was created by {by}, and {why}, so it was paused \
             (AMUX_SCHEDULE_AUTHORITY=enforce).\n\nTo keep it: POST /api/schedules/{id}/reown (it becomes yours and is \
             re-enabled). To drop it: delete the schedule. To leave it paused, move this card to discarded."
        ),
        status: "needsyou".into(),
        session: None,
        item_type: "decision".into(),
        creator: "schedule-authority".into(),
        owner_type: "human".into(),
        due: None,
        due_time: None,
        reviewer: None,
        shepherd: None,
        gate: vec![],
        depends_on: vec![],
        tags: vec!["schedule-authority".into(), format!("schedule:{id}")],
        ask_type: Some("access".into()),
        next_action: None,
        acceptance_criteria: None,
        ask_question: Some(format!("Should schedule {id} keep running as {target} under your authority?")),
        ask_unblocks: Some(format!("Re-owning it lets schedule {id} fire again for {target}.")),
        ask_actor: Some(crate::api::turn_end::owner_name()),
        source: Some("schedule-authority".into()),
        requested_by: Some(format!("schedule:{id}")),
        callback_session: None,
        callback_prompt: None,
    };
    let now = crate::config::now_f64() as i64;
    let made = store
        .write_async(move |conn| {
            let row = crate::db::board_store::create_issue(conn, &new, now)?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Task,
                    entity_id: row.id.clone(),
                    mutation: amux_core::revision::MutationKind::Created,
                    payload: Some(json!({"id": row.id, "source": "schedule-authority"})),
                }],
            })
        })
        .await;
    match made {
        Ok(_) => {
            counters::bump("card_filed");
            tracing::info!(schedule = id, measured = true, n_considered = 1, verdict = "schedule_authority_card_filed",
                "asked the owner to re-own or delete a paused schedule");
        }
        Err(e) => {
            counters::bump("card_failed");
            tracing::warn!(schedule = id, error = %e, measured = false, verdict = "schedule_authority_card_unfiled",
                "a paused schedule could not file its card");
        }
    }
}

// ── Who may act here ───────────────────────────────────────────────────────

/// A person, never a worker or an app. `Err` is the response to send.
fn person(headers: &HeaderMap, gate: &'static str) -> Result<String, Box<Response>> {
    if super::app_identity::actor(headers).is_some() || super::email::hdr_worker(headers).is_some_and(|w| !w.starts_with("member:")) {
        return Err(Box::new((StatusCode::FORBIDDEN, Json(json!({
            "error": "only a person can do this, not a worker or an app",
            "code": "person_required",
        }))).into_response()));
    }
    match super::human_proof::check(headers, gate) {
        super::human_proof::Verdict::Proven(p) => Ok(match p {
            super::human_proof::Proven::Owner => "owner".into(),
            other => other.actor(),
        }),
        super::human_proof::Verdict::Unproven => Ok("unproven".into()),
        super::human_proof::Verdict::Refused => Err(Box::new(super::human_proof::refusal(gate))),
    }
}

// ── Re-own ─────────────────────────────────────────────────────────────────

/// `POST /api/schedules/{id}/reown`: the caller takes the schedule over. It
/// becomes theirs (proven), and a schedule the gate or offboarding paused is
/// re-enabled. A scoped member may re-own only within their scope.
pub async fn reown(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let who = match person(&headers, "schedule_reown") {
        Ok(w) => w,
        Err(r) => return *r,
    };
    let scope = super::org::local_member_scope(&headers);
    let (rid, who_w) = (id.clone(), who.clone());
    let verified = who != "unproven";
    let outside = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let outside_w = outside.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let target: Option<String> = conn
                .query_row("SELECT session FROM schedules WHERE id=?1 AND deleted IS NULL", [&rid], |r| r.get(0))
                .optional()?;
            let Some(target) = target else {
                return Ok(WriteOutcome { applied: false, events: vec![] });
            };
            if let Some(s) = &scope {
                if !target.is_empty() && !s.allows_worker(&target) {
                    outside_w.store(true, std::sync::atomic::Ordering::SeqCst);
                    return Ok(WriteOutcome { applied: false, events: vec![] });
                }
            }
            let old: String = conn
                .query_row("SELECT COALESCE(created_by,'') FROM schedules WHERE id=?1", [&rid], |r| r.get(0))
                .unwrap_or_default();
            conn.execute(
                "UPDATE schedules SET created_by=?1, created_by_verified=?2, \
                   enabled=CASE WHEN disabled_reason IN (?3,?4) THEN 1 ELSE enabled END, \
                   disabled_reason=CASE WHEN disabled_reason IN (?3,?4) THEN NULL ELSE disabled_reason END, \
                   updated=?5 WHERE id=?6",
                rusqlite::params![who_w, i64::from(verified), LAPSED_REASON, OFFBOARD_REASON,
                    chrono::Utc::now().timestamp(), rid],
            )?;
            crate::runtime_jobs::scheduler::insert_audit(conn, &rid, "created_by", &old, &who_w, "schedule-reown", &who_w)?;
            super::ledger::append(conn, &super::ledger::Event {
                event_id: format!("reown:{rid}:{}", ulid::Ulid::new()),
                ts_ms: chrono::Utc::now().timestamp_millis(),
                kind: "schedule_authority".into(),
                actor: who_w.clone(),
                authority: Some(if verified { "proven_person" } else { "unproven" }.into()),
                action: Some("reown".into()),
                resource: Some(format!("schedule:{rid}")),
                decision: Some("allowed".into()),
                outcome: Some(format!("creator {old} -> {who_w}")),
                ..Default::default()
            });
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if outside.load(std::sync::atomic::Ordering::SeqCst) {
        return (StatusCode::FORBIDDEN, Json(json!({
            "error": "this schedule targets a worker outside your scope", "code": "out_of_scope"}))).into_response();
    }
    match res.map(|r| r.applied) {
        Ok(true) => {
            clear_ask(&crate::config::amux_home(), &id);
            counters::bump("reowned");
            Json(json!({"ok": true, "id": id, "created_by": who, "created_by_verified": verified})).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": "no such schedule"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

// ── Sponsors ───────────────────────────────────────────────────────────────

/// Who answers for `worker`: its row, else the owner.
pub fn sponsor_of(conn: &Connection, worker: &str) -> String {
    conn.query_row("SELECT sponsor FROM worker_sponsors WHERE worker=?1", [worker], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
        .unwrap_or_else(|| "owner".into())
}

/// `PUT /api/workers/{name}/sponsor {"sponsor": "owner" | "member:<email>"}`,
/// owner only (members cannot reach org administration).
pub async fn set_sponsor(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(name): AxPath<String>,
    Json(body): Json<Value>,
) -> Response {
    let who = match person(&headers, "worker_sponsor") {
        Ok(w) => w,
        Err(r) => return *r,
    };
    if super::org::local_member_scope(&headers).is_some() && !super::org::local_member_is_admin(&headers) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "only the owner or an admin sets a sponsor"}))).into_response();
    }
    let sponsor = body["sponsor"].as_str().unwrap_or("").trim().to_string();
    if sponsor != "owner" && !sponsor.starts_with("member:") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "sponsor is 'owner' or 'member:<email>'"}))).into_response();
    }
    let (n, s, w) = (name.clone(), sponsor.clone(), who.clone());
    let res = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO worker_sponsors(worker,sponsor,set_at,set_by) VALUES(?1,?2,?3,?4) \
                 ON CONFLICT(worker) DO UPDATE SET sponsor=excluded.sponsor, set_at=excluded.set_at, set_by=excluded.set_by",
                rusqlite::params![n, s, chrono::Utc::now().timestamp(), w],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match res {
        Ok(_) => Json(json!({"ok": true, "worker": name, "sponsor": sponsor})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

// ── Offboarding ────────────────────────────────────────────────────────────

/// Everything a member owns, and what offboarding does with each.
pub fn offboard_plan(conn: &Connection, home: &Path, email: &str) -> rusqlite::Result<Option<(String, Vec<Value>)>> {
    let member: Option<(String, Option<i64>)> = conn
        .query_row("SELECT id, offboarded_at FROM org_members WHERE lower(email)=lower(?1)", [email], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    let Some((mid, _)) = member else { return Ok(None) };
    let actor = format!("member:{email}");
    let mut items = Vec::new();
    let mut q = conn.prepare("SELECT token FROM org_invites WHERE used_by=?1 AND used_at IS NOT NULL")?;
    for t in q.query_map([&mid], |r| r.get::<_, String>(0))? {
        let t = t?;
        items.push(json!({"kind": "session", "id": format!("{}…", t.chars().take(6).collect::<String>()),
            "token": t, "action": "revoke"}));
    }
    let mut q = conn.prepare("SELECT token FROM org_invites WHERE lower(COALESCE(email,''))=lower(?1) AND used_at IS NULL AND expires_at > ?2")?;
    for t in q.query_map(rusqlite::params![email, chrono::Utc::now().timestamp()], |r| r.get::<_, String>(0))? {
        let t = t?;
        items.push(json!({"kind": "invite", "id": format!("{}…", t.chars().take(6).collect::<String>()),
            "token": t, "action": "revoke"}));
    }
    let mut q = conn.prepare(
        "SELECT id, title, session, enabled FROM schedules WHERE created_by=?1 AND deleted IS NULL")?;
    for r in q.query_map([&actor], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))? {
        let (id, title, target, enabled) = r?;
        items.push(json!({"kind": "schedule", "id": id, "title": title, "target": target,
            "action": if enabled != 0 { "pause" } else { "already_paused" }}));
    }
    let mut q = conn.prepare("SELECT worker FROM worker_sponsors WHERE sponsor=?1")?;
    for w in q.query_map([&actor], |r| r.get::<_, String>(0))? {
        items.push(json!({"kind": "worker_sponsor", "id": w?, "action": "reassign_to_owner"}));
    }
    let mut q = conn.prepare("SELECT id, title FROM standing_approvals WHERE granted_by=?1 AND revoked=0")?;
    for r in q.query_map([&actor], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (id, title) = r?;
        items.push(json!({"kind": "standing_approval", "id": id, "title": title, "action": "revoke"}));
    }
    for l in super::static_files::public_links::load(home) {
        if l.approved_by == actor {
            items.push(json!({"kind": "published_app", "id": l.id, "title": l.title, "action": "return_to_pending"}));
        }
    }
    Ok(Some((mid, items)))
}

#[derive(serde::Deserialize, Default)]
pub struct OffboardQuery {
    #[serde(default)]
    dry_run: Option<String>,
}

/// `POST /api/org/members/{email}/offboard[?dry_run=1]`, owner only. Revokes
/// the member's sign-ins and pending invites, pauses their schedules, hands
/// the workers they sponsor back to the owner, revokes the standing approvals
/// they granted, and returns the apps they approved to pending. Nothing is
/// deleted. One ledger event per item.
pub async fn offboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxPath(email): AxPath<String>,
    Query(q): Query<OffboardQuery>,
) -> Response {
    let who = match person(&headers, "member_offboard") {
        Ok(w) => w,
        Err(r) => return *r,
    };
    let dry = q.dry_run.as_deref().is_some_and(|v| matches!(v, "1" | "true" | "yes"));
    let home = crate::config::amux_home();
    let (e, h) = (email.clone(), home.clone());
    let plan = state.store.read_async(move |c| Ok(offboard_plan(c, &h, &e)?)).await;
    let (mid, items) = match plan {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({"error": "no such member"}))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    };
    let shown: Vec<Value> = items
        .iter()
        .map(|i| {
            let mut i = i.clone();
            if let Some(o) = i.as_object_mut() {
                o.remove("token");
            }
            i
        })
        .collect();
    if dry {
        return Json(json!({"ok": true, "dry_run": true, "member": email, "items": shown,
            "measured": true, "n_considered": shown.len()})).into_response();
    }
    // Published apps live in their own file: return them to pending first.
    let actor = format!("member:{email}");
    let apps_paused = super::static_files::public_links::return_to_pending(&home, &actor);
    let (e, who_w, items_w) = (email.clone(), who.clone(), items.clone());
    let res = state
        .store
        .write_async(move |conn| {
            let now = chrono::Utc::now().timestamp();
            let mut done = 0usize;
            for it in &items_w {
                let kind = it["kind"].as_str().unwrap_or("");
                let outcome = match kind {
                    "session" => {
                        conn.execute("UPDATE org_invites SET used_at=NULL, expires_at=0 WHERE token=?1",
                            [it["token"].as_str().unwrap_or("")])?;
                        "revoked"
                    }
                    "invite" => {
                        conn.execute("UPDATE org_invites SET expires_at=0 WHERE token=?1",
                            [it["token"].as_str().unwrap_or("")])?;
                        "revoked"
                    }
                    "schedule" => {
                        let id = it["id"].as_str().unwrap_or("");
                        if pause_in(conn, id, OFFBOARD_REASON, "member-offboard", &who_w)? > 0 { "paused" } else { "already_paused" }
                    }
                    "worker_sponsor" => {
                        conn.execute("UPDATE worker_sponsors SET sponsor='owner', set_at=?1, set_by=?2 WHERE worker=?3",
                            rusqlite::params![now, who_w, it["id"].as_str().unwrap_or("")])?;
                        "reassigned_to_owner"
                    }
                    "standing_approval" => {
                        conn.execute("UPDATE standing_approvals SET revoked=1, revoked_at=?1, revoked_by=?2 WHERE id=?3",
                            rusqlite::params![now, who_w, it["id"].as_i64().unwrap_or(-1)])?;
                        "revoked"
                    }
                    "published_app" => "returned_to_pending",
                    _ => continue,
                };
                let shown_id = it["id"].to_string().trim_matches('"').to_string();
                super::ledger::append(conn, &super::ledger::Event {
                    event_id: format!("offboard:{e}:{kind}:{shown_id}:{}", ulid::Ulid::new()),
                    ts_ms: chrono::Utc::now().timestamp_millis(),
                    kind: "offboard".into(),
                    actor: who_w.clone(),
                    on_behalf_of: Some(format!("member:{e}")),
                    authority: Some(if who_w == "unproven" { "unproven" } else { "proven_person" }.into()),
                    action: Some(format!("offboard_{kind}")),
                    resource: Some(format!("{kind}:{shown_id}")),
                    decision: Some("allowed".into()),
                    outcome: Some(outcome.into()),
                    ..Default::default()
                });
                done += 1;
            }
            conn.execute("UPDATE org_members SET offboarded_at=?1, offboarded_by=?2 WHERE id=?3",
                rusqlite::params![now, who_w, mid])?;
            Ok(WriteOutcome { applied: done > 0, events: vec![] })
        })
        .await;
    match res {
        Ok(_) => {
            counters::bump("offboarded");
            tracing::warn!(member = %email, by = %who, items = items.len(), apps = apps_paused.len(), measured = true,
                n_considered = items.len(), verdict = "member_offboarded", "a member was offboarded; their work was paused, not deleted");
            Json(json!({"ok": true, "dry_run": false, "member": email, "items": shown,
                "measured": true, "n_considered": shown.len()})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// The `schedule_authority` block for `/api/contract/counters`.
pub fn report() -> Value {
    let c = counters::snapshot();
    let n = c.get("considered").copied().unwrap_or(0);
    json!({
        "mode": match mode() { Mode::Off => "off", Mode::Report => "report", Mode::Enforce => "enforce" },
        "env": MODE_ENV,
        "measured": true,
        "n_considered": n,
        "counts": c,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    fn worker(home: &Path, name: &str) {
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::write(home.join("sessions").join(format!("{name}.env")), "CC_TAGS=customers\n").unwrap();
    }

    fn member(c: &Connection, email: &str, level: &str, name: &str) {
        let id = format!("m-{email}");
        c.execute("INSERT INTO org_members(id,email,role,joined_at,scope_level,scope_name) VALUES(?1,?2,'member',0,?3,?4)",
            rusqlite::params![id, email, level, name]).unwrap();
        c.execute("INSERT INTO org_invites(token,email,created_at,expires_at,used_at,used_by) VALUES(?1,?2,0,9999999999,1,?3)",
            rusqlite::params![format!("tok-{email}"), email, id]).unwrap();
    }

    #[test]
    fn an_active_member_with_scope_keeps_authority_and_loses_it_when_scope_moves() {
        let dir = tempfile::tempdir().unwrap();
        worker(dir.path(), "wexus");
        let c = conn();
        member(&c, "m@x.io", "worker", "wexus");
        assert_eq!(decide(&c, dir.path(), "member:m@x.io", true, "wexus"), Decision::Allow("member:m@x.io".into()));
        c.execute("UPDATE org_members SET scope_name='rothco' WHERE email='m@x.io'", []).unwrap();
        assert!(matches!(decide(&c, dir.path(), "member:m@x.io", true, "wexus"), Decision::Lapsed(w) if w.contains("no longer has scope")));
    }

    #[test]
    fn a_revoked_sign_in_or_offboarded_member_has_no_authority() {
        let dir = tempfile::tempdir().unwrap();
        worker(dir.path(), "wexus");
        let c = conn();
        member(&c, "m@x.io", "global", "");
        c.execute("UPDATE org_invites SET used_at=NULL WHERE email='m@x.io'", []).unwrap();
        assert!(matches!(decide(&c, dir.path(), "member:m@x.io", true, "wexus"), Decision::Lapsed(w) if w.contains("no longer an active member")));
    }

    #[test]
    fn a_missing_worker_lapses_even_for_the_owner_and_legacy_rows_stay_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn();
        assert!(matches!(decide(&c, dir.path(), "owner", true, "gone"), Decision::Lapsed(_)));
        worker(dir.path(), "wexus");
        assert_eq!(decide(&c, dir.path(), "owner", true, "wexus"), Decision::Allow("owner".into()));
        assert!(matches!(decide(&c, dir.path(), "api-create", false, "wexus"), Decision::Unverified(_)));
        assert!(matches!(decide(&c, dir.path(), "worker:gone", true, "wexus"), Decision::Lapsed(_)));
    }

    #[test]
    fn a_row_written_after_the_migration_has_no_creator_until_create_stamps_one() {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c.execute("INSERT INTO schedules(id,title,session,command,created,updated) VALUES('S1','t','w','x',0,0)", []).unwrap();
        let (by, v): (String, i64) = c.query_row("SELECT COALESCE(created_by,''), created_by_verified FROM schedules WHERE id='S1'",
            [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((by.as_str(), v), ("", 0), "a row written after the migration has no creator until create() stamps one");
    }

    #[test]
    fn the_offboard_plan_lists_everything_and_pause_never_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let c = conn();
        member(&c, "m@x.io", "global", "");
        c.execute("INSERT INTO schedules(id,title,session,command,created,updated,created_by,created_by_verified) \
                   VALUES('S1','t','w','x',0,0,'member:m@x.io',1)", []).unwrap();
        c.execute("INSERT INTO standing_approvals(title,allowed,category,granted_by,granted_at) VALUES('a','b','budget','member:m@x.io',0)", []).unwrap();
        c.execute("INSERT INTO worker_sponsors(worker,sponsor,set_at) VALUES('w','member:m@x.io',0)", []).unwrap();
        let (_, items) = offboard_plan(&c, dir.path(), "m@x.io").unwrap().unwrap();
        let kinds: Vec<&str> = items.iter().filter_map(|i| i["kind"].as_str()).collect();
        for k in ["session", "schedule", "standing_approval", "worker_sponsor"] {
            assert!(kinds.contains(&k), "{k} missing from {kinds:?}");
        }
        assert_eq!(pause_in(&c, "S1", OFFBOARD_REASON, "t", "owner").unwrap(), 1);
        let (en, reason, del): (i64, String, Option<i64>) = c.query_row(
            "SELECT enabled, disabled_reason, deleted FROM schedules WHERE id='S1'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((en, reason.as_str(), del), (0, OFFBOARD_REASON, None));
    }
}
