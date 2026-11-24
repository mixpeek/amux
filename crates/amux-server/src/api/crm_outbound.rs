//! Outbound in the CRM (owner 2026-11-24: "use every crm feature for amux
//! outbound and as u do so any gaps in automatically making the outbound
//! effective personal timely etc should be built into the crm").
//!
//! Sequences: a named list of steps, each a channel, an action and how many days
//! after the previous step it is due, with an optional template.
//!   GET  /api/crm/sequences, POST {name, goal?, steps, stop_on_reply?, assignee?}
//!   GET  /api/crm/sequences/{id}       the sequence, its enrollments and per-step results
//!   PATCH /api/crm/sequences/{id}, DELETE (soft)
//!   POST /api/crm/sequences/{id}/enroll  {contact_ids} or {filter: {grade, tag, status, source, limit}}
//!   POST /api/crm/sequences/{id}/stop    {contact_id}
//!   GET  /api/crm/contacts/{id}/brief    everything a drafter needs to make a touch personal
//!
//! Timely: enrolling plans step 1; a step done plans the next, due its days later;
//! crm-watch emits crm.touch.due once for each open touch whose day has come, so its
//! assignee wakes. Effective: a reply stops the person's sequences (stop_on_reply),
//! and a reply asking to stop marks them unsubscribed. Safe: no outward touch for a
//! person who unsubscribed, bounced or is do_not_contact, no second open touch on the
//! same channel, and none within AMUX_CRM_TOUCH_COOLDOWN_DAYS (default 2) of another
//! outward touch outside its own sequence. Personal: a template fills {first_name},
//! {name}, {company}, {role} and {signal} (the newest thing a watched channel showed),
//! and the brief carries the rest.
//! Verdicts: crm_touch_suppressed, crm_touch_duplicate, crm_touch_cooldown,
//! crm_sequence_enrolled, crm_sequence_advanced, crm_sequence_stopped_on_reply,
//! crm_contact_unsubscribed_by_reply, crm_touch_due_emitted.

use super::calendar::query_rows_json;
use super::crm_accounts::{ev, now_secs, source_of};
use super::{internal, AppState};
use crate::db::board_store::next_issue_id;
use crate::db::WriteOutcome;
use amux_core::revision::MutationKind;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sequences", get(list_sequences).post(create_sequence))
        .route("/sequences/{id}", get(get_sequence).patch(patch_sequence).delete(delete_sequence))
        .route("/sequences/{id}/enroll", post(enroll))
        .route("/sequences/{id}/stop", post(stop))
        .route("/contacts/{id}/brief", get(brief_route))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn slot<T>(s: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn outward(action: &str) -> bool {
    !matches!(action, "note" | "follow" | "like")
}

fn day_plus(days: i64) -> String {
    (chrono::Local::now().date_naive() + chrono::Duration::days(days.max(0))).format("%Y-%m-%d").to_string()
}

/// Why an outward touch may not be planned: (verdict, message, the existing touch).
pub(crate) type Refusal = (&'static str, String, String);

/// Why an outward touch may not be planned for this person now.
pub(crate) fn guard(conn: &rusqlite::Connection, contact: &str, channel: &str, action: &str, sequence: &str, force: bool) -> Option<Refusal> {
    if contact.is_empty() || !outward(action) {
        return None;
    }
    let stop: Option<(Option<i64>, Option<i64>, String)> = conn
        .query_row("SELECT unsubscribed_at, bounced_at, status FROM crm_contacts WHERE id=?1 AND deleted IS NULL", [contact], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .ok();
    if let Some((u, b, st)) = stop {
        let why = if u.is_some() { "unsubscribed" } else if b.is_some() { "bounced" } else if st == "do_not_contact" { "do_not_contact" } else { "" };
        if !why.is_empty() {
            return Some(("crm_touch_suppressed", format!("this person is {why}; nothing outward goes to them"), String::new()));
        }
    }
    if force {
        return None;
    }
    let open: Option<String> = conn
        .query_row(
            "SELECT id FROM crm_touches WHERE contact_id=?1 AND channel=?2 AND deleted IS NULL AND status IN ('planned','drafted','approved') \
             AND action NOT IN ('note','follow','like') LIMIT 1",
            [contact, channel],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = open {
        return Some(("crm_touch_duplicate", format!("{id} is already open for this person on {channel}"), id));
    }
    let days = crate::config::env_i64("AMUX_CRM_TOUCH_COOLDOWN_DAYS", 2);
    if days > 0 {
        let recent: Option<String> = conn
            .query_row(
                "SELECT id FROM crm_touches WHERE contact_id=?1 AND deleted IS NULL AND status='done' AND done_at>=?2 \
                 AND action NOT IN ('note','follow','like') AND (?3='' OR sequence<>?3) ORDER BY done_at DESC LIMIT 1",
                rusqlite::params![contact, now_secs() - days * 86_400, sequence],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = recent {
            return Some(("crm_touch_cooldown", format!("{id} reached this person within {days} day(s); wait, or send force: true"), id));
        }
    }
    None
}

/// The newest thing a watched channel of the person or their company showed in 30 days.
fn signal(conn: &rusqlite::Connection, contact: &str, account: &str) -> String {
    conn.query_row(
        "SELECT json_extract(data,'$.summary') FROM session_events WHERE type='crm.channel.changed' AND ts>=?3 \
         AND (json_extract(data,'$.owner_id')=?1 OR (?2<>'' AND json_extract(data,'$.owner_id')=?2)) ORDER BY id DESC LIMIT 1",
        rusqlite::params![contact, account, (now_secs() - 30 * 86_400) as f64],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// A template with the person's fields filled; an unknown field is left as written.
pub(crate) fn fill(conn: &rusqlite::Connection, template: &str, contact: &str) -> String {
    if template.is_empty() {
        return String::new();
    }
    let row: Option<(String, String, String, String, String)> = conn
        .query_row(
            "SELECT c.name, COALESCE(NULLIF(a.name,''), c.company), c.role, c.account_id, c.id FROM crm_contacts c \
             LEFT JOIN crm_accounts a ON a.id=c.account_id WHERE c.id=?1",
            [contact],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok();
    let Some((name, company, role, account, id)) = row else { return template.to_string() };
    let first = name.split_whitespace().next().unwrap_or("").to_string();
    template
        .replace("{first_name}", &first)
        .replace("{name}", &name)
        .replace("{company}", &company)
        .replace("{role}", &role)
        .replace("{signal}", &signal(conn, &id, &account))
}

fn steps_of(conn: &rusqlite::Connection, seq: &str) -> Option<(Vec<Value>, String)> {
    conn.query_row("SELECT steps, assignee FROM crm_sequences WHERE id=?1 AND deleted IS NULL", [seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ok()
        .map(|(s, a)| (serde_json::from_str::<Vec<Value>>(&s).unwrap_or_default(), a))
}

/// Plans step `n` (1-based) of a sequence for a person. Returns the touch id.
fn plan_step(conn: &rusqlite::Connection, seq: &str, contact: &str, n: usize, fallback_assignee: &str, source: &str) -> rusqlite::Result<Option<String>> {
    let Some((steps, seq_assignee)) = steps_of(conn, seq) else { return Ok(None) };
    let Some(step) = steps.get(n - 1) else { return Ok(None) };
    let s = |k: &str| step.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let body = fill(conn, &s("template"), contact);
    let assignee = [s("assignee"), seq_assignee, fallback_assignee.to_string()].into_iter().find(|a| !a.is_empty()).unwrap_or_default();
    let id = next_issue_id(conn, "TCH")?;
    let now = now_secs();
    conn.execute(
        "INSERT INTO crm_touches (id,contact_id,channel,action,body,due_date,assignee,status,sequence,step,source,created,updated) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
        rusqlite::params![&id, contact, s("channel"), s("action"), &body, day_plus(step.get("day").and_then(Value::as_i64).unwrap_or(0)),
            &assignee, if body.is_empty() { "planned" } else { "drafted" }, seq, n as i64, source, now],
    )?;
    conn.execute("UPDATE crm_enrollments SET step=?3, updated=?4 WHERE sequence_id=?1 AND contact_id=?2", rusqlite::params![seq, contact, n as i64, now])?;
    Ok(Some(id))
}

/// After a touch is done: plan its sequence's next step, or finish the enrollment.
pub(crate) fn advance(conn: &rusqlite::Connection, touch: &str) -> rusqlite::Result<Option<String>> {
    let row: Option<(String, String, i64, String)> = conn
        .query_row("SELECT sequence, contact_id, step, assignee FROM crm_touches WHERE id=?1", [touch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .ok();
    let Some((seq, contact, step, assignee)) = row else { return Ok(None) };
    if seq.is_empty() || contact.is_empty() {
        return Ok(None);
    }
    let active: bool = conn
        .query_row("SELECT status='active' FROM crm_enrollments WHERE sequence_id=?1 AND contact_id=?2", [&seq, &contact], |r| r.get(0))
        .unwrap_or(false);
    if !active {
        return Ok(None);
    }
    let next = plan_step(conn, &seq, &contact, step as usize + 1, &assignee, &format!("sequence:{seq}"))?;
    match &next {
        Some(id) => tracing::info!(target: "amux::crm", sequence = %seq, contact = %contact, touch = %id, step = step + 1, measured = true, n_considered = 1,
            verdict = "crm_sequence_advanced", "the next step of a sequence is planned"),
        None => {
            conn.execute("UPDATE crm_enrollments SET status='finished', updated=?3 WHERE sequence_id=?1 AND contact_id=?2", rusqlite::params![&seq, &contact, now_secs()])?;
        }
    }
    Ok(next)
}

/// A person wrote back: stop the sequences that stop on a reply, and honour a
/// request to stop. Returns how many open touches were skipped.
pub(crate) fn on_reply(conn: &rusqlite::Connection, contact: &str, text: &str) -> rusqlite::Result<usize> {
    let now = now_secs();
    let t = text.to_lowercase();
    if ["unsubscribe", "remove me", "stop emailing", "stop contacting", "take me off", "do not contact", "don't contact"].iter().any(|k| t.contains(k)) {
        let n = conn.execute("UPDATE crm_contacts SET unsubscribed_at=?2, updated=?2 WHERE id=?1 AND unsubscribed_at IS NULL", rusqlite::params![contact, now])?;
        if n > 0 {
            tracing::info!(target: "amux::crm", contact = %contact, measured = true, n_considered = 1, verdict = "crm_contact_unsubscribed_by_reply",
                "a reply asked to stop; the person is unsubscribed and nothing outward goes to them");
        }
    }
    let stopped = conn.execute(
        "UPDATE crm_touches SET status='skipped', result='stopped: they replied', updated=?2 WHERE contact_id=?1 AND deleted IS NULL \
         AND status IN ('planned','drafted','approved') AND sequence IN \
         (SELECT e.sequence_id FROM crm_enrollments e JOIN crm_sequences s ON s.id=e.sequence_id WHERE e.contact_id=?1 AND e.status='active' AND s.stop_on_reply=1)",
        rusqlite::params![contact, now],
    )?;
    let ended = conn.execute(
        "UPDATE crm_enrollments SET status='replied', updated=?2 WHERE contact_id=?1 AND status='active' \
         AND sequence_id IN (SELECT id FROM crm_sequences WHERE stop_on_reply=1)",
        rusqlite::params![contact, now],
    )?;
    if ended > 0 {
        tracing::info!(target: "amux::crm", contact = %contact, sequences = ended, touches_skipped = stopped, measured = true, n_considered = ended,
            verdict = "crm_sequence_stopped_on_reply", "a reply stopped the person's sequences");
    }
    Ok(stopped)
}

/// Everything a drafter needs to make a touch personal, in one read.
pub(crate) fn brief(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    let Some(person) = query_rows_json(conn, "SELECT * FROM crm_contacts WHERE id=?1 AND deleted IS NULL", &[&id as &dyn rusqlite::ToSql])?.into_iter().next() else {
        return Ok(None);
    };
    let account = person["account_id"].as_str().unwrap_or("").to_string();
    let company = if account.is_empty() { Value::Null } else {
        query_rows_json(conn, "SELECT * FROM crm_accounts WHERE id=?1", &[&account as &dyn rusqlite::ToSql])?.into_iter().next().unwrap_or(Value::Null)
    };
    let p = &[&id as &dyn rusqlite::ToSql, &account as &dyn rusqlite::ToSql];
    let channels = query_rows_json(conn,
        "SELECT id, owner_kind, kind, handle, url, watch, last_checked_at, last_changed_at, substr(last_items,1,1500) AS last_items_head FROM crm_channels \
         WHERE deleted IS NULL AND ((owner_kind='contact' AND owner_id=?1) OR (?2<>'' AND owner_kind='account' AND owner_id=?2))", p)?;
    let signals = query_rows_json(conn,
        "SELECT ts, json_extract(data,'$.kind') AS kind, json_extract(data,'$.summary') AS summary, json_extract(data,'$.url') AS url FROM session_events \
         WHERE type='crm.channel.changed' AND (json_extract(data,'$.owner_id')=?1 OR (?2<>'' AND json_extract(data,'$.owner_id')=?2)) ORDER BY id DESC LIMIT 10", p)?;
    let activity = query_rows_json(conn,
        "SELECT date, type, direction, subject, substr(notes,1,600) AS notes FROM crm_interactions WHERE contact_id=?1 ORDER BY created DESC LIMIT 15", &p[..1])?;
    let touches = query_rows_json(conn,
        "SELECT id, channel, action, status, sequence, step, due_date, substr(body,1,600) AS body, result FROM crm_touches WHERE contact_id=?1 AND deleted IS NULL ORDER BY created DESC LIMIT 15", &p[..1])?;
    let deals = query_rows_json(conn,
        "SELECT id, title, stage, amount, next_step FROM crm_deals WHERE deleted IS NULL AND (account_id=?2 AND ?2<>'' OR contact_ids LIKE '%\"' || ?1 || '\"%')", p)?;
    let sequences = query_rows_json(conn,
        "SELECT e.sequence_id, s.name, e.status, e.step FROM crm_enrollments e JOIN crm_sequences s ON s.id=e.sequence_id WHERE e.contact_id=?1", &p[..1])?;
    let lookalikes = if account.is_empty() { vec![] } else { super::crm_accounts::lookalikes_of(conn, "account", &account)? };
    let replied = activity.iter().find(|a| a["direction"] == "in").cloned().unwrap_or(Value::Null);
    let can_reach = guard(conn, id, "email", "email", "", true).map(|(v, m, _)| json!({ "ok": false, "verdict": v, "why": m })).unwrap_or(json!({ "ok": true }));
    Ok(Some(json!({
        "person": person, "company": company, "channels": channels, "signals": signals, "activity": activity, "last_inbound": replied,
        "touches": touches, "deals": deals, "sequences": sequences, "lookalikes": lookalikes, "can_reach": can_reach,
        "measured": true, "n_considered": activity.len() + touches.len() + signals.len(),
    })))
}

async fn brief_route(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> { Ok(brief(&*store.read()?, &id)?) }).await {
        Ok(Ok(Some(b))) => Json(b).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

fn valid_steps(v: &Value) -> Result<String, String> {
    let steps = v.as_array().filter(|a| !a.is_empty()).ok_or("steps is a non-empty list of {day, channel, action, template?, assignee?}")?;
    for (i, s) in steps.iter().enumerate() {
        let ch = s.get("channel").and_then(Value::as_str).unwrap_or("");
        let ac = s.get("action").and_then(Value::as_str).unwrap_or("");
        if !super::crm_agent::KINDS.contains(&ch) || !super::crm_agent::ACTIONS.contains(&ac) {
            return Err(format!("step {}: channel is one of {}; action one of {}", i + 1, super::crm_agent::KINDS.join(", "), super::crm_agent::ACTIONS.join(", ")));
        }
        if s.get("day").is_some_and(|d| d.as_i64().is_none_or(|d| d < 0)) {
            return Err(format!("step {}: day is a whole number of days after the previous step", i + 1));
        }
    }
    Ok(v.to_string())
}

async fn list_sequences(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let r = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        Ok(query_rows_json(&*store.read()?,
            "SELECT s.*, (SELECT COUNT(*) FROM crm_enrollments e WHERE e.sequence_id=s.id) AS enrolled, \
             (SELECT COUNT(*) FROM crm_enrollments e WHERE e.sequence_id=s.id AND e.status='active') AS active, \
             (SELECT COUNT(*) FROM crm_enrollments e WHERE e.sequence_id=s.id AND e.status='replied') AS replied \
             FROM crm_sequences s WHERE s.deleted IS NULL ORDER BY s.created DESC", &[])?)
    })
    .await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

async fn create_sequence(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name required" }));
    }
    let steps = match valid_steps(body.get("steps").unwrap_or(&Value::Null)) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let source = source_of(&body, &headers);
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let assignee = { let a = s("assignee"); if a.is_empty() { source.strip_prefix("worker:").unwrap_or("").to_string() } else { a } };
    let goal = s("goal");
    let stop = body.get("stop_on_reply").and_then(Value::as_bool).unwrap_or(true);
    let out: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let id = next_issue_id(conn, "SEQ")?;
            conn.execute(
                "INSERT INTO crm_sequences (id,name,goal,steps,stop_on_reply,assignee,source,created,updated) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                rusqlite::params![&id, &name, &goal, &steps, stop as i64, &assignee, &source, now_secs()],
            )?;
            *slot(&w) = Some(id.clone());
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_sequence", &id, MutationKind::Created)] })
        })
        .await;
    match res {
        Ok(_) => (StatusCode::CREATED, Json(json!({ "id": slot(&out).take(), "ok": true }))).into_response(),
        Err(e) => internal(e),
    }
}

/// A sequence with its enrollments and, per step, how its touches went and how
/// many people replied after it.
fn sequence_view(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    let Some(mut seq) = query_rows_json(conn, "SELECT * FROM crm_sequences WHERE id=?1 AND deleted IS NULL", &[&id as &dyn rusqlite::ToSql])?.into_iter().next() else {
        return Ok(None);
    };
    let steps: Vec<Value> = serde_json::from_str(seq["steps"].as_str().unwrap_or("[]")).unwrap_or_default();
    let enrollments = query_rows_json(conn,
        "SELECT e.*, c.name AS contact_name, c.email FROM crm_enrollments e LEFT JOIN crm_contacts c ON c.id=e.contact_id WHERE e.sequence_id=?1 ORDER BY e.created",
        &[&id as &dyn rusqlite::ToSql])?;
    let mut per_step = Vec::new();
    for n in 1..=steps.len() as i64 {
        let count = |st: &str| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM crm_touches WHERE sequence=?1 AND step=?2 AND status=?3 AND deleted IS NULL", rusqlite::params![id, n, st], |r| r.get(0)).unwrap_or(0)
        };
        let replied: i64 = conn
            .query_row("SELECT COUNT(*) FROM crm_enrollments WHERE sequence_id=?1 AND status='replied' AND step=?2", rusqlite::params![id, n], |r| r.get(0))
            .unwrap_or(0);
        let done = count("done");
        per_step.push(json!({ "step": n, "channel": steps[n as usize - 1]["channel"], "action": steps[n as usize - 1]["action"],
            "planned": count("planned"), "drafted": count("drafted"), "approved": count("approved"), "done": done,
            "skipped": count("skipped"), "failed": count("failed"), "replied": replied }));
    }
    let reached: i64 = conn
        .query_row("SELECT COUNT(DISTINCT contact_id) FROM crm_touches WHERE sequence=?1 AND status='done' AND deleted IS NULL", [id], |r| r.get(0))
        .unwrap_or(0);
    let replied = enrollments.iter().filter(|e| e["status"] == "replied").count() as i64;
    seq["steps"] = json!(steps);
    seq["enrollments"] = json!(enrollments);
    seq["per_step"] = json!(per_step);
    // Reply rate over the people a touch actually reached; null until someone is reached.
    seq["reached"] = json!(reached);
    seq["reply_rate"] = if reached > 0 { json!((replied as f64 / reached as f64 * 1000.0).round() / 1000.0) } else { Value::Null };
    seq["measured"] = json!(true);
    seq["n_considered"] = json!(enrollments.len());
    Ok(Some(seq))
}

async fn get_sequence(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> { Ok(sequence_view(&*store.read()?, &id)?) }).await {
        Ok(Ok(Some(v))) => Json(v).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

async fn patch_sequence(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let steps = match body.get("steps") {
        Some(v) => match valid_steps(v) {
            Ok(s) => Some(s),
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
        },
        None => None,
    };
    let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    let (name, goal, assignee) = (s("name"), s("goal"), s("assignee"));
    let stop = body.get("stop_on_reply").and_then(Value::as_bool);
    let res = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(
                "UPDATE crm_sequences SET name=COALESCE(?2,name), goal=COALESCE(?3,goal), steps=COALESCE(?4,steps), assignee=COALESCE(?5,assignee), \
                 stop_on_reply=COALESCE(?6,stop_on_reply), updated=?7 WHERE id=?1 AND deleted IS NULL",
                rusqlite::params![&id, name, goal, steps, assignee, stop.map(|b| b as i64), now_secs()],
            )?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_sequence", &id, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

async fn delete_sequence(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let res = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            let n = conn.execute("UPDATE crm_sequences SET deleted=?2, updated=?2 WHERE id=?1 AND deleted IS NULL", rusqlite::params![&id, now])?;
            conn.execute("UPDATE crm_enrollments SET status='stopped', updated=?2 WHERE sequence_id=?1 AND status='active'", rusqlite::params![&id, now])?;
            conn.execute("UPDATE crm_touches SET status='skipped', result='sequence deleted', updated=?2 WHERE sequence=?1 AND status IN ('planned','drafted','approved')", rusqlite::params![&id, now])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_sequence", &id, MutationKind::Deleted)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

/// Enrolls people and plans step 1 for each. Who cannot be reached, or is
/// already in it, is listed with the reason rather than enrolled.
pub(crate) fn enroll_people(conn: &rusqlite::Connection, seq: &str, ids: &[String], by: &str) -> rusqlite::Result<Option<Value>> {
    let Some((steps, _)) = steps_of(conn, seq) else { return Ok(None) };
    let first = steps.first().cloned().unwrap_or(Value::Null);
    let (ch, ac) = (first["channel"].as_str().unwrap_or(""), first["action"].as_str().unwrap_or(""));
    let (mut enrolled, mut skipped) = (Vec::new(), Vec::new());
    let worker = by.strip_prefix("worker:").unwrap_or("");
    for cid in ids {
        let exists: bool = conn.query_row("SELECT 1 FROM crm_contacts WHERE id=?1 AND deleted IS NULL", [cid], |_| Ok(true)).unwrap_or(false);
        if !exists {
            skipped.push(json!({ "contact_id": cid, "verdict": "not_found" }));
            continue;
        }
        let already: Option<String> = conn.query_row("SELECT status FROM crm_enrollments WHERE sequence_id=?1 AND contact_id=?2", [seq, cid], |r| r.get(0)).ok();
        if let Some(st) = already {
            skipped.push(json!({ "contact_id": cid, "verdict": "already_enrolled", "status": st }));
            continue;
        }
        if let Some((v, why, existing)) = guard(conn, cid, ch, ac, seq, false) {
            skipped.push(json!({ "contact_id": cid, "verdict": v, "why": why, "existing": existing }));
            continue;
        }
        let now = now_secs();
        conn.execute("INSERT INTO crm_enrollments (sequence_id,contact_id,enrolled_by,created,updated) VALUES (?1,?2,?3,?4,?4)", rusqlite::params![seq, cid, by, now])?;
        let touch = plan_step(conn, seq, cid, 1, worker, &format!("sequence:{seq}"))?;
        enrolled.push(json!({ "contact_id": cid, "touch_id": touch }));
    }
    tracing::info!(target: "amux::crm", sequence = %seq, enrolled = enrolled.len(), skipped = skipped.len(), measured = true, n_considered = ids.len(),
        verdict = "crm_sequence_enrolled", "people enrolled in a sequence");
    Ok(Some(json!({ "ok": true, "enrolled": enrolled, "skipped": skipped, "n_considered": ids.len() })))
}

async fn enroll(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let by = source_of(&json!({}), &headers);
    let out: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let mut ids: Vec<String> = body.get("contact_ids").and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            if let Some(f) = body.get("filter").filter(|f| f.is_object()) {
                let g = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let limit = f.get("limit").and_then(Value::as_i64).unwrap_or(25).clamp(1, 500);
                let mut q = conn.prepare(
                    "SELECT c.id FROM crm_contacts c WHERE c.deleted IS NULL \
                     AND (?1='' OR instr(?1, c.icp_grade) > 0 AND c.icp_grade<>'') AND (?2='' OR c.status=?2) AND (?3='' OR c.source=?3) \
                     AND (?4='' OR EXISTS (SELECT 1 FROM crm_tags t WHERE t.contact_id=c.id AND t.tag=?4)) \
                     AND NOT EXISTS (SELECT 1 FROM crm_enrollments e WHERE e.sequence_id=?5 AND e.contact_id=c.id) \
                     ORDER BY c.icp_grade='' , c.icp_grade, c.updated DESC LIMIT ?6")?;
                let found: Vec<String> = q.query_map(rusqlite::params![g("grade").to_uppercase(), g("status"), g("source"), g("tag"), &id, limit], |r| r.get(0))?
                    .filter_map(Result::ok).collect();
                ids.extend(found);
            }
            if ids.is_empty() {
                *slot(&w) = Some(json!({ "error": "send contact_ids or a filter that matches someone", "code": "nobody" }));
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            let v = enroll_people(conn, &id, &ids, &by)?;
            let applied = v.as_ref().and_then(|v| v["enrolled"].as_array()).is_some_and(|a| !a.is_empty());
            *slot(&w) = Some(v.unwrap_or(json!({ "error": "not found", "code": "not_found" })));
            Ok(WriteOutcome { applied, events: if applied { vec![ev("crm_sequence", &id, MutationKind::Updated), ev("crm_touch", "", MutationKind::Created)] } else { vec![] } })
        })
        .await;
    if let Err(e) = res {
        return internal(e);
    }
    let v = slot(&out).take().unwrap_or(Value::Null);
    match v.get("code").and_then(Value::as_str) {
        Some("not_found") => err(StatusCode::NOT_FOUND, v),
        Some(_) => err(StatusCode::BAD_REQUEST, v),
        None => Json(v).into_response(),
    }
}

async fn stop(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let cid = body.get("contact_id").and_then(Value::as_str).unwrap_or("").to_string();
    let res = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            let n = conn.execute("UPDATE crm_enrollments SET status='stopped', updated=?3 WHERE sequence_id=?1 AND contact_id=?2 AND status='active'", rusqlite::params![&id, &cid, now])?;
            conn.execute("UPDATE crm_touches SET status='skipped', result='stopped', updated=?3 WHERE sequence=?1 AND contact_id=?2 AND status IN ('planned','drafted','approved')", rusqlite::params![&id, &cid, now])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_sequence", &id, MutationKind::Updated), ev("crm_contact", &cid, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

static DUE: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// Open touches whose day has come, each announced once as crm.touch.due.
pub(crate) fn take_due(conn: &rusqlite::Connection, today: &str) -> rusqlite::Result<Vec<Value>> {
    let rows = query_rows_json(conn,
        "SELECT t.id, t.contact_id, COALESCE(c.name,'') AS contact_name, COALESCE(a.name,'') AS account_name, t.channel, t.action, t.status, \
         t.sequence, t.step, t.due_date, t.assignee FROM crm_touches t LEFT JOIN crm_contacts c ON c.id=t.contact_id LEFT JOIN crm_accounts a ON a.id=c.account_id \
         WHERE t.deleted IS NULL AND t.status IN ('planned','drafted','approved') AND t.due_date<>'' AND t.due_date<=?1 AND t.due_announced_at IS NULL ORDER BY t.due_date LIMIT 200",
        &[&today as &dyn rusqlite::ToSql])?;
    let now = now_secs();
    let mut out = Vec::new();
    for mut r in rows {
        let id = r["id"].as_str().unwrap_or("").to_string();
        conn.execute("UPDATE crm_touches SET due_announced_at=?2 WHERE id=?1", rusqlite::params![&id, now])?;
        let cid = r["contact_id"].as_str().unwrap_or("").to_string();
        r["next"] = json!(match r["status"].as_str() {
            Some("planned") => "draft it: read the brief, then amux crm draft <id> --stdin; the owner approves it",
            Some("drafted") => "waiting for the owner's approval in the CRM tab (To approve)",
            _ => "approved: do it now, then amux crm done <id>",
        });
        if !cid.is_empty() {
            r["brief_url"] = json!(format!("/api/crm/contacts/{cid}/brief"));
        }
        out.push(r);
    }
    Ok(out)
}

pub(crate) async fn announce_due(state: &AppState) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let res = state
        .store
        .write_async(move |conn| {
            let d = take_due(conn, &today)?;
            let applied = !d.is_empty();
            *slot(&DUE) = d;
            Ok(WriteOutcome { applied, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(target: "amux::crm", error = %e, measured = false, n_considered = 0, verdict = "crm_touch_due_unreadable", "due touches could not be read");
        return;
    }
    let due = std::mem::take(&mut *slot(&DUE));
    if !due.is_empty() {
        tracing::info!(target: "amux::crm", n = due.len(), measured = true, n_considered = due.len(), verdict = "crm_touch_due_emitted", "touches whose day has come");
    }
    for d in due {
        super::crm_agent::emit(state, "crm.touch.due", d).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::standing_approvals::tests::{call, test_state};

    async fn app_with_people() -> (Router, AppState, Vec<String>) {
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(crate::api::crm_accounts::routes()).merge(crate::api::crm_agent::routes()).merge(routes()).with_state(state.clone());
        let owner = [("authorization", "Bearer owner-secret")];
        let mut ids = vec![];
        for (n, e, g) in [("Ada Lovelace", "ada@acme.io", "A"), ("Bo Chen", "bo@beta.io", "B"), ("Cy Dee", "cy@gamma.io", "D")] {
            let (_, p) = call(&app, "POST", "/contacts", &owner, Some(json!({"name": n, "email": e, "company": "Acme", "icp_grade": g, "role": "CTO"}))).await;
            ids.push(p["id"].as_str().unwrap().to_string());
        }
        (app, state, ids)
    }

    #[tokio::test]
    async fn a_sequence_plans_each_step_when_the_last_is_done_and_stops_on_a_reply() {
        let (app, state, ids) = app_with_people().await;
        let owner = [("authorization", "Bearer owner-secret")];
        let w = [("x-amux-session", "amux-gtm")];
        let (st, s) = call(&app, "POST", "/sequences", &w, Some(json!({"name": "amux outbound v1", "steps": [
            {"day": 0, "channel": "email", "action": "email", "template": "Hi {first_name}, as {role} at {company}..."},
            {"day": 3, "channel": "linkedin", "action": "dm"}]}))).await;
        assert_eq!(st, StatusCode::CREATED);
        let seq = s["id"].as_str().unwrap().to_string();
        let (_, e) = call(&app, "POST", &format!("/sequences/{seq}/enroll"), &w, Some(json!({"filter": {"grade": "AB"}}))).await;
        assert_eq!(e["enrolled"].as_array().unwrap().len(), 2, "A and B, not D: {e}");
        let ada_touch = e["enrolled"].as_array().unwrap().iter().find(|x| x["contact_id"] == ids[0].as_str()).unwrap()["touch_id"].as_str().unwrap().to_string();
        let (_, t) = call(&app, "GET", &format!("/touches?contact_id={}", ids[0]), &w, None).await;
        assert_eq!((t[0]["status"].as_str(), t[0]["body"].as_str(), t[0]["assignee"].as_str()), (Some("drafted"), Some("Hi Ada, as CTO at Acme..."), Some("amux-gtm")), "filled from the template");

        // Due today: announced once.
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let due = state.store.write_async(move |c| { let d = take_due(c, &today)?; *slot(&DUE) = d; Ok(WriteOutcome { applied: true, events: vec![] }) }).await;
        assert!(due.is_ok());
        let d = std::mem::take(&mut *slot(&DUE));
        assert_eq!(d.len(), 2);
        assert!(d[0]["brief_url"].as_str().unwrap().ends_with("/brief"));
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let again = state.store.write_async(move |c| { let d = take_due(c, &today)?; *slot(&DUE) = d; Ok(WriteOutcome { applied: true, events: vec![] }) }).await;
        assert!(again.is_ok());
        assert!(std::mem::take(&mut *slot(&DUE)).is_empty(), "each due touch is announced once");

        // Approve, do it: step 2 is planned three days out.
        call(&app, "POST", &format!("/touches/{ada_touch}/approve"), &owner, None).await;
        let (st, _) = call(&app, "POST", &format!("/touches/{ada_touch}/done"), &w, Some(json!({"result": "sent"}))).await;
        assert_eq!(st, StatusCode::OK);
        let (_, t) = call(&app, "GET", &format!("/touches?contact_id={}", ids[0]), &w, None).await;
        let next = t.as_array().unwrap().iter().find(|x| x["step"] == 2).expect("step 2 planned");
        assert_eq!((next["channel"].as_str(), next["status"].as_str(), next["due_date"].as_str()), (Some("linkedin"), Some("planned"), Some(day_plus(3).as_str())));

        // She replies asking to stop: the sequence ends, step 2 is skipped, she is unsubscribed.
        let ada = ids[0].clone();
        static N: Mutex<usize> = Mutex::new(0);
        state.store.write_async(move |c| { *slot(&N) = on_reply(c, &ada, "please remove me from your list")?; Ok(WriteOutcome { applied: true, events: vec![] }) }).await.unwrap();
        assert_eq!(*slot(&N), 1, "step 2 is skipped");
        let (_, v) = call(&app, "GET", &format!("/sequences/{seq}"), &w, None).await;
        let ada = v["enrollments"].as_array().unwrap().iter().find(|x| x["contact_id"] == ids[0].as_str()).unwrap();
        assert_eq!(ada["status"], "replied");
        assert_eq!((v["reached"].as_i64(), v["reply_rate"].as_f64()), (Some(1), Some(1.0)));
        let (st, r) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ids[0], "channel": "x", "action": "dm", "body": "hi"}))).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_touch_suppressed")), "nothing outward after an unsubscribe");
    }

    #[tokio::test]
    async fn no_second_open_touch_on_a_channel_and_a_cooldown_after_an_outward_touch() {
        let (app, _state, ids) = app_with_people().await;
        let owner = [("authorization", "Bearer owner-secret")];
        let w = [("x-amux-session", "amux-gtm")];
        let (_, t) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ids[1], "channel": "email", "action": "email", "body": "one"}))).await;
        let first = t["id"].as_str().unwrap().to_string();
        let (st, r) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ids[1], "channel": "email", "action": "email", "body": "two"}))).await;
        assert_eq!((st, r["code"].as_str(), r["existing"].as_str()), (StatusCode::CONFLICT, Some("crm_touch_duplicate"), Some(first.as_str())));
        let (st, _) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ids[1], "channel": "email", "action": "note", "body": "a note is fine"}))).await;
        assert_eq!(st, StatusCode::CREATED);
        call(&app, "POST", &format!("/touches/{first}/approve"), &owner, None).await;
        call(&app, "POST", &format!("/touches/{first}/done"), &w, Some(json!({"result": "sent"}))).await;
        let (st, r) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ids[1], "channel": "linkedin", "action": "dm", "body": "so soon"}))).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_touch_cooldown")));
        let (st, _) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ids[1], "channel": "linkedin", "action": "dm", "body": "owner says go", "force": true}))).await;
        assert_eq!(st, StatusCode::CREATED, "force passes the cooldown");
    }

    #[tokio::test]
    async fn a_brief_carries_the_person_company_activity_touches_and_whether_they_can_be_reached() {
        let (app, _state, ids) = app_with_people().await;
        let owner = [("authorization", "Bearer owner-secret")];
        call(&app, "POST", &format!("/contacts/{}/interactions", ids[2]), &owner, Some(json!({"type": "email", "notes": "liked the demo", "direction": "in"}))).await;
        let (st, b) = call(&app, "GET", &format!("/contacts/{}/brief", ids[2]), &owner, None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!((b["person"]["name"].as_str(), b["can_reach"]["ok"].as_bool()), (Some("Cy Dee"), Some(true)));
        assert_eq!(b["last_inbound"]["notes"].as_str(), Some("liked the demo"));
        call(&app, "PATCH", &format!("/contacts/{}", ids[2]), &owner, Some(json!({"status": "do_not_contact"}))).await;
        let (_, b) = call(&app, "GET", &format!("/contacts/{}/brief", ids[2]), &owner, None).await;
        assert_eq!(b["can_reach"]["verdict"].as_str(), Some("crm_touch_suppressed"));
    }
}
