//! Campaigns in the CRM (owner, 2026-11-27: "Do we have the concept of a
//! campaign?" ... "yes build it").
//!
//! A campaign is one go-to-market push: its goal and use case (the IP check,
//! creative-dna), the ICP it targets, the opportunity's static app, the
//! mailboxes its email goes out from, its dates and its status. Its sequences
//! and the deals they open point to it (`campaign_id`), and the funnel adds up
//! across them (GET /api/crm/funnel?group=campaign). It is CRM data: workers
//! still draft, the owner still approves, the scheduler still times the steps.
//!
//!   GET    /api/crm/campaigns                 each with its sequences, enrolled, won and value signed
//!   POST   /api/crm/campaigns                 {name, goal?, use_case?, icp?, asset_url?, mailboxes?, starts?, ends?, status?, owner?, notes?}
//!   GET    /api/crm/campaigns/{id}            the campaign, its sequences, its funnel and its deals
//!   PATCH  /api/crm/campaigns/{id}, DELETE (soft; its sequences keep running, unlinked)
//!   PATCH  /api/crm/sequences/{id} {campaign_id}   puts a sequence in a campaign ("" takes it out)
//!
//! Status: `draft` (being set up), `active`, `paused`, `done`. Only an active
//! campaign (or a sequence in none) takes new enrollments
//! (`crm_campaign_not_active`); a paused or done one also holds its touches:
//! the owner cannot approve them (`crm_campaign_paused`) and their due days
//! are not announced until it is active again.
//!
//! A step's `{asset_url}` is the person's open deal's app, else the campaign's;
//! `{campaign}` is its name. The person's brief lists the campaigns they are in.

use super::calendar::query_rows_json;
use super::crm_accounts::{ev, now_secs, source_of};
use super::{internal, AppState};
use crate::db::board_store::next_issue_id;
use crate::db::WriteOutcome;
use amux_core::revision::MutationKind;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/campaigns", get(list_route).post(create_route))
        .route("/campaigns/{id}", get(get_route).patch(patch_route).delete(delete_route))
}

pub const STATUSES: &[&str] = &["draft", "active", "paused", "done"];
const TEXT_FIELDS: &[&str] = &["name", "goal", "use_case", "icp", "asset_url", "starts", "ends", "owner", "notes"];

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// The campaign a sequence belongs to, as (id, name, status, asset_url), when it has one.
pub(crate) fn of_sequence(conn: &rusqlite::Connection, seq: &str) -> Option<(String, String, String, String)> {
    if seq.is_empty() {
        return None;
    }
    conn.query_row(
        "SELECT c.id, c.name, c.status, c.asset_url FROM crm_sequences s JOIN crm_campaigns c ON c.id=s.campaign_id \
         WHERE s.id=?1 AND c.deleted IS NULL",
        [seq],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .ok()
}

/// Why a sequence takes no new enrollment now: its campaign is not active.
pub(crate) fn enroll_refusal(conn: &rusqlite::Connection, seq: &str) -> Option<String> {
    match of_sequence(conn, seq) {
        Some((id, name, status, _)) if status != "active" => Some(format!("{id} ({name}) is {status}: only an active campaign takes new people")),
        _ => None,
    }
}

/// Why a touch may not be approved now: its sequence's campaign is paused or done.
pub(crate) fn hold_refusal(conn: &rusqlite::Connection, touch: &str) -> Option<String> {
    let seq: String = conn.query_row("SELECT sequence FROM crm_touches WHERE id=?1", [touch], |r| r.get(0)).ok()?;
    match of_sequence(conn, &seq) {
        Some((id, name, status, _)) if status == "paused" || status == "done" => Some(format!("{id} ({name}) is {status}; resume it to send its touches")),
        _ => None,
    }
}

/// The campaign a person's deal belongs to: the newest campaign among the sequences they are enrolled in.
pub(crate) fn of_contact(conn: &rusqlite::Connection, contact: &str) -> String {
    conn.query_row(
        "SELECT s.campaign_id FROM crm_enrollments e JOIN crm_sequences s ON s.id=e.sequence_id WHERE e.contact_id=?1 AND s.campaign_id<>'' \
         ORDER BY e.created DESC LIMIT 1",
        [contact],
        |r| r.get(0),
    )
    .unwrap_or_default()
}

/// A step's template with the campaign's fields: `{campaign}` its name, and
/// `{asset_url}` its app when the person's open deal has none of its own
/// (the deal's own app, filled after, wins).
pub(crate) fn fill_campaign(conn: &rusqlite::Connection, template: &str, seq: &str, contact: &str) -> String {
    let Some((_, name, _, asset)) = of_sequence(conn, seq) else { return template.to_string() };
    let mut t = template.replace("{campaign}", &name);
    if t.contains("{asset_url}") && !asset.is_empty() {
        let deal_asset: String = super::crm_close::open_deal_for(conn, contact)
            .and_then(|(d, _)| conn.query_row("SELECT asset_url FROM crm_deals WHERE id=?1", [&d], |r| r.get(0)).ok())
            .unwrap_or_default();
        if deal_asset.is_empty() {
            t = t.replace("{asset_url}", &asset);
        }
    }
    t
}

fn mailboxes_of(v: Option<&Value>) -> Result<Option<String>, String> {
    let Some(v) = v else { return Ok(None) };
    let list: Vec<String> = match v {
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
        Value::String(s) => s.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
        _ => return Err("mailboxes is a list of addresses".into()),
    };
    if let Some(bad) = list.iter().find(|m| !m.contains('@') || !m.contains('.')) {
        return Err(format!("{bad} is not an email address"));
    }
    Ok(Some(json!(list).to_string()))
}

fn status_of(v: Option<&Value>) -> Result<Option<String>, String> {
    match v.and_then(Value::as_str).map(|s| s.trim().to_lowercase()) {
        None => Ok(None),
        Some(s) if STATUSES.contains(&s.as_str()) => Ok(Some(s)),
        Some(_) => Err(format!("status is one of {}", STATUSES.join(", "))),
    }
}

/// A campaign row with its parts as arrays and its numbers: sequences,
/// enrolled people, and from the funnel won and the value signed.
fn rollup(conn: &rusqlite::Connection, mut c: Value) -> rusqlite::Result<Value> {
    let id = c["id"].as_str().unwrap_or("").to_string();
    if let Some(s) = c.get("mailboxes").and_then(Value::as_str) {
        c["mailboxes"] = serde_json::from_str::<Value>(s).unwrap_or(json!([]));
    }
    let n = |sql: &str| -> i64 { conn.query_row(sql, [&id], |r| r.get(0)).unwrap_or(0) };
    c["sequences"] = json!(n("SELECT COUNT(*) FROM crm_sequences WHERE campaign_id=?1 AND deleted IS NULL"));
    let mut q = HashMap::new();
    q.insert("group".to_string(), "campaign".to_string());
    q.insert("campaign".to_string(), id.clone());
    let f = super::crm_close::funnel(conn, &q)?;
    let g = f["groups"].get(0).cloned().unwrap_or(json!({}));
    for k in ["enrolled", "contacted", "replied", "booked", "demoed", "won", "lost", "won_amount_usd", "mrr_usd"] {
        c[k] = g.get(k).cloned().unwrap_or(json!(0));
    }
    c["open_deals"] = json!(n("SELECT COUNT(*) FROM crm_deals WHERE campaign_id=?1 AND deleted IS NULL AND stage NOT IN ('won','lost')"));
    Ok(c)
}

async fn list_route(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let rows = query_rows_json(&conn,
            "SELECT * FROM crm_campaigns WHERE deleted IS NULL ORDER BY CASE status WHEN 'active' THEN 0 WHEN 'draft' THEN 1 WHEN 'paused' THEN 2 ELSE 3 END, created DESC", &[])?;
        let out: Vec<Value> = rows.into_iter().map(|c| rollup(&conn, c)).collect::<rusqlite::Result<_>>()?;
        Ok(Value::Array(out))
    })
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

async fn get_route(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let conn = store.read()?;
        let Some(c) = query_rows_json(&conn, "SELECT * FROM crm_campaigns WHERE id=?1 AND deleted IS NULL", &[&id as &dyn rusqlite::ToSql])?.pop() else {
            return Ok(None);
        };
        let mut c = rollup(&conn, c)?;
        c["sequence_list"] = json!(query_rows_json(&conn,
            "SELECT s.id, s.name, s.goal, s.steps, s.assignee, (SELECT COUNT(*) FROM crm_enrollments e WHERE e.sequence_id=s.id) AS enrolled, \
             (SELECT COUNT(*) FROM crm_enrollments e WHERE e.sequence_id=s.id AND e.status='active') AS active \
             FROM crm_sequences s WHERE s.campaign_id=?1 AND s.deleted IS NULL ORDER BY s.created", &[&id as &dyn rusqlite::ToSql])?);
        c["deals"] = json!(query_rows_json(&conn,
            "SELECT d.id, d.title, d.stage, d.amount, d.next_step, COALESCE(a.name,'') AS account_name FROM crm_deals d LEFT JOIN crm_accounts a ON a.id=d.account_id \
             WHERE d.campaign_id=?1 AND d.deleted IS NULL ORDER BY d.updated DESC", &[&id as &dyn rusqlite::ToSql])?);
        let mut q = HashMap::new();
        q.insert("campaign".to_string(), id.clone());
        c["funnel"] = super::crm_close::funnel(&conn, &q)?;
        Ok(Some(c))
    })
    .await
    {
        Ok(Ok(Some(v))) => Json(v).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

async fn create_route(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let name = s("name");
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name required" }));
    }
    let mailboxes = match mailboxes_of(body.get("mailboxes")) {
        Ok(m) => m.unwrap_or_else(|| "[]".into()),
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let status = match status_of(body.get("status")) {
        Ok(st) => st.unwrap_or_else(|| "active".into()),
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let source = source_of(&body, &headers);
    let f: Vec<String> = ["goal", "use_case", "icp", "asset_url", "starts", "ends", "owner", "notes"].iter().map(|k| s(k)).collect();
    let out: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let id = next_issue_id(conn, "CMP")?;
            conn.execute(
                "INSERT INTO crm_campaigns (id,name,goal,use_case,icp,asset_url,mailboxes,starts,ends,status,owner,notes,source,created,updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)",
                rusqlite::params![&id, &name, &f[0], &f[1], &f[2], &f[3], &mailboxes, &f[4], &f[5], &status, &f[6], &f[7], &source, now_secs()],
            )?;
            *w.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id.clone());
            Ok(WriteOutcome { applied: true, events: vec![ev("crm_campaign", &id, MutationKind::Created)] })
        })
        .await;
    match res {
        Ok(_) => {
            let id = out.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            tracing::info!(target: "amux::crm", campaign = ?id, measured = true, n_considered = 1, verdict = "crm_campaign_created", "a campaign was created");
            (StatusCode::CREATED, Json(json!({ "id": id, "ok": true }))).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn patch_route(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut sets: Vec<(&str, SqlValue)> = Vec::new();
    for f in TEXT_FIELDS {
        if let Some(v) = body.get(*f).and_then(Value::as_str) {
            if *f == "name" && v.trim().is_empty() {
                return err(StatusCode::BAD_REQUEST, json!({ "error": "a campaign keeps a name" }));
            }
            sets.push((f, SqlValue::Text(v.trim().to_string())));
        }
    }
    match mailboxes_of(body.get("mailboxes")) {
        Ok(Some(m)) => sets.push(("mailboxes", SqlValue::Text(m))),
        Ok(None) => {}
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    }
    let status = match status_of(body.get("status")) {
        Ok(st) => st,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    if let Some(st) = status {
        sets.push(("status", SqlValue::Text(st)));
    }
    if sets.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "nothing to change" }));
    }
    let res = state
        .store
        .write_async(move |conn| {
            let mut params: Vec<SqlValue> = sets.iter().map(|(_, v)| v.clone()).collect();
            let cols: Vec<String> = sets.iter().enumerate().map(|(i, (f, _))| format!("{f}=?{}", i + 1)).collect();
            params.push(SqlValue::Integer(now_secs()));
            params.push(SqlValue::Text(id.clone()));
            let n = conn.execute(
                &format!("UPDATE crm_campaigns SET {}, updated=?{} WHERE id=?{} AND deleted IS NULL", cols.join(", "), params.len() - 1, params.len()),
                rusqlite::params_from_iter(params.iter()),
            )?;
            // A pause never marks its touches announced, so active again they are announced at the next pass.
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_campaign", &id, MutationKind::Updated)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(o) if o.applied => Json(json!({ "ok": true })).into_response(),
        Ok(_) => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
        Err(e) => internal(e),
    }
}

async fn delete_route(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let res = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("UPDATE crm_campaigns SET deleted=?2, updated=?2 WHERE id=?1 AND deleted IS NULL", rusqlite::params![&id, now_secs()])?;
            // Its sequences and deals keep going, no longer in a campaign.
            conn.execute("UPDATE crm_sequences SET campaign_id='' WHERE campaign_id=?1", [&id])?;
            conn.execute("UPDATE crm_deals SET campaign_id='' WHERE campaign_id=?1", [&id])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_campaign", &id, MutationKind::Deleted)] } else { vec![] } })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::standing_approvals::tests::{call, test_state};

    const OWNER: [(&str, &str); 1] = [("authorization", "Bearer owner-secret")];
    const W: [(&str, &str); 1] = [("x-amux-session", "amux-gtm")];

    #[tokio::test]
    async fn a_campaign_groups_its_sequences_fills_its_app_owns_its_deals_and_pausing_holds_it() {
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(crate::api::crm_accounts::routes()).merge(crate::api::crm_agent::routes())
            .merge(crate::api::crm_outbound::routes()).merge(crate::api::crm_close::routes()).merge(routes()).with_state(state.clone());
        let (st, _) = call(&app, "POST", "/campaigns", &OWNER, Some(json!({"name": "IP check", "mailboxes": ["not-an-address"]}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a mailbox is an address");
        let (st, c) = call(&app, "POST", "/campaigns", &OWNER, Some(json!({"name": "IP check Q1", "goal": "book IP check demos", "use_case": "ip-check",
            "icp": "media libraries with licensing exposure", "asset_url": "https://apps.example/ip-check", "mailboxes": "Ethan@trymixpeek.com, ethan@getmixpeek.com"}))).await;
        assert_eq!(st, StatusCode::CREATED);
        let cid = c["id"].as_str().unwrap().to_string();

        // Two sequences in it: one made inside, one moved in.
        let tpl = json!([{"channel": "email", "action": "email", "template": "Hi {first_name}, for {campaign}: {asset_url}"}, {"day": 3, "channel": "linkedin", "action": "dm"}]);
        let (_, s1) = call(&app, "POST", "/sequences", &W, Some(json!({"name": "IP email", "campaign_id": cid, "steps": tpl}))).await;
        let (_, s2) = call(&app, "POST", "/sequences", &W, Some(json!({"name": "IP X", "steps": [{"channel": "linkedin", "action": "dm", "template": "hi"}]}))).await;
        let (s1, s2) = (s1["id"].as_str().unwrap().to_string(), s2["id"].as_str().unwrap().to_string());
        let (st, _) = call(&app, "PATCH", &format!("/sequences/{s2}"), &W, Some(json!({"campaign_id": "CMP-nope"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a sequence joins only a campaign that exists");
        call(&app, "PATCH", &format!("/sequences/{s2}"), &W, Some(json!({"campaign_id": cid}))).await;

        let person = |n: &'static str| { let app = app.clone(); async move {
            let (_, p) = call(&app, "POST", "/contacts", &OWNER, Some(json!({"name": n, "email": format!("{}@acme.io", n.to_lowercase()), "linkedin": format!("linkedin.com/in/{n}")}))).await;
            p["id"].as_str().unwrap().to_string() } };
        let (ada, bo) = (person("Ada").await, person("Bo").await);
        let (_, e) = call(&app, "POST", &format!("/sequences/{s1}/enroll"), &W, Some(json!({"contact_ids": [ada]}))).await;
        let t1 = e["enrolled"][0]["touch_id"].as_str().unwrap().to_string();
        let (_, touches) = call(&app, "GET", &format!("/touches?contact_id={ada}"), &OWNER, None).await;
        assert_eq!(touches[0]["body"], "Hi Ada, for IP check Q1: https://apps.example/ip-check", "the campaign's name and app reach the draft");
        call(&app, "POST", &format!("/sequences/{s2}/enroll"), &W, Some(json!({"contact_ids": [bo]}))).await;

        // The funnel adds up across both sequences.
        let (_, f) = call(&app, "GET", "/funnel?group=campaign", &OWNER, None).await;
        let g = f["groups"].as_array().unwrap().iter().find(|g| g["key"] == cid.as_str()).unwrap().clone();
        assert_eq!((g["name"].as_str(), g["enrolled"].as_i64()), (Some("IP check Q1"), Some(2)));
        let (_, v) = call(&app, "GET", &format!("/campaigns/{cid}"), &OWNER, None).await;
        assert_eq!((v["sequences"].as_i64(), v["enrolled"].as_i64(), v["sequence_list"].as_array().map(Vec::len)), (Some(2), Some(2), Some(2)));
        assert_eq!(v["mailboxes"], json!(["ethan@trymixpeek.com", "ethan@getmixpeek.com"]));

        // A deal opened from its sequence belongs to it; the brief lists it.
        let (_, ix) = call(&app, "POST", &format!("/contacts/{ada}/interactions"), &W, Some(json!({"type": "email", "direction": "in", "notes": "interested"}))).await;
        assert!(ix["id"].is_string());
        let (_, deals) = call(&app, "GET", "/deals", &OWNER, None).await;
        assert_eq!(deals[0]["campaign_id"], cid.as_str());
        let (_, b) = call(&app, "GET", &format!("/contacts/{ada}/brief"), &OWNER, None).await;
        assert_eq!(b["campaigns"][0]["asset_url"], "https://apps.example/ip-check");

        // Paused: no new people, no approvals; resumed, both again.
        let (_, carl) = call(&app, "POST", "/contacts", &OWNER, Some(json!({"name": "Carl", "email": "carl@acme.io"}))).await;
        call(&app, "PATCH", &format!("/campaigns/{cid}"), &OWNER, Some(json!({"status": "paused"}))).await;
        let (st, r) = call(&app, "POST", &format!("/sequences/{s1}/enroll"), &W, Some(json!({"contact_ids": [carl["id"]]}))).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::BAD_REQUEST, Some("crm_campaign_not_active")));
        let (_, d) = call(&app, "POST", "/touches", &W, Some(json!({"contact_id": bo, "channel": "x", "action": "dm", "body": "hi", "sequence": s2}))).await;
        let (st, r) = call(&app, "POST", &format!("/touches/{}/approve", d["id"].as_str().unwrap()), &OWNER, None).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::CONFLICT, Some("crm_campaign_paused")));
        let take = || { let state = state.clone(); async move {
            let out: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
            let w = out.clone();
            state.store.write_async(move |conn| {
                *w.lock().unwrap() = crate::api::crm_outbound::take_due(conn, "2099-01-01")?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            }).await.unwrap();
            let v = out.lock().unwrap().clone();
            v } };
        let due = take().await;
        assert!(due.iter().all(|t| t["sequence"] != s1.as_str() && t["sequence"] != s2.as_str()), "a paused campaign's due days wait: {due:?}");
        call(&app, "PATCH", &format!("/campaigns/{cid}"), &OWNER, Some(json!({"status": "active"}))).await;
        let (st, _) = call(&app, "POST", &format!("/touches/{t1}/approve"), &OWNER, None).await;
        assert_eq!(st, StatusCode::CONFLICT, "Ada replied, so her open touch was stopped; the refusal is not the campaign's");
        let (st, _) = call(&app, "POST", &format!("/touches/{}/approve", d["id"].as_str().unwrap()), &OWNER, None).await;
        assert_eq!(st, StatusCode::OK, "resumed, its touches can be approved");
        let due = take().await;
        assert!(due.iter().any(|t| t["sequence"] == s2.as_str()), "resumed, its due days are announced");

        // Deleted: its sequences keep running, unlinked.
        call(&app, "DELETE", &format!("/campaigns/{cid}"), &OWNER, None).await;
        let (_, seqs) = call(&app, "GET", "/sequences", &OWNER, None).await;
        assert!(seqs.as_array().unwrap().iter().all(|s| s["campaign_id"] == ""), "{seqs}");
    }
}
