//! Provisioning from an identity provider: SCIM 2.0 (RFC 7643/7644) over the
//! people amux already has (governance PR 20; plan 4.1 and phase 4).
//!
//! | SCIM | amux |
//! |---|---|
//! | User | an `org_members` row; `userName` is the email, `active` is "not offboarded" |
//! | Group | an `org_teams` row; a member belongs to one team, so adding a person to a group moves them there |
//! | deactivate (`active: false`) or delete | the same offboarding as `POST /api/org/members/{email}/offboard` (governance PR 13): sign-ins and invites revoked, schedules paused, sponsored workers back to the owner, standing approvals revoked, approved apps back to pending |
//!
//! A provisioned person starts with no access (a worker scope naming no
//! worker) unless `AMUX_SCIM_DEFAULT_TEAM` names a team; a group made through
//! SCIM starts the same way, and the owner gives it a scope in Team settings.
//! The provider decides who is in; the owner still decides what a team sees.
//!
//! The provider authenticates with a bearer token the owner issues at
//! `POST /api/scim/tokens` (shown once; only its SHA-256 is kept) and revokes
//! with `DELETE /api/scim/tokens/{id}`. Every change is an `identity` ledger
//! row whose actor is `policy:scim:<token id>`, on behalf of the owner.

use super::schedule_authority;
use super::AppState;
use axum::{
    body::Bytes,
    extract::{Path as AxPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

/// The scope name of a person or team with no access yet.
pub const UNASSIGNED: &str = "__unassigned__";
const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
const MAX_PAGE: i64 = 500;

pub(crate) mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static REQUESTS: AtomicU64 = AtomicU64::new(0);
    pub static UNAUTHORIZED: AtomicU64 = AtomicU64::new(0);
    pub static CREATED: AtomicU64 = AtomicU64::new(0);
    pub static DEACTIVATED: AtomicU64 = AtomicU64::new(0);
    pub static REACTIVATED: AtomicU64 = AtomicU64::new(0);
    pub static DELETED: AtomicU64 = AtomicU64::new(0);
    pub static GROUP_CHANGES: AtomicU64 = AtomicU64::new(0);
    pub static TOKENS_ISSUED: AtomicU64 = AtomicU64::new(0);
    pub static TOKENS_REVOKED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ── Responses ───────────────────────────────────────────────────────────────

fn scim(status: StatusCode, v: Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/scim+json")], v.to_string()).into_response()
}

fn error(status: StatusCode, detail: &str, scim_type: Option<&str>) -> Response {
    let mut v = json!({"schemas": [ERROR_SCHEMA], "status": status.as_u16().to_string(), "detail": detail});
    if let Some(t) = scim_type {
        v["scimType"] = json!(t);
    }
    scim(status, v)
}

fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.to_rfc3339()).unwrap_or_default()
}

// ── Auth ────────────────────────────────────────────────────────────────────

/// The id of the live SCIM token this request presented.
fn authenticate(conn: &Connection, headers: &HeaderMap) -> rusqlite::Result<Option<String>> {
    let Some(tok) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim)
        .filter(|t| t.starts_with("scim_"))
    else {
        return Ok(None);
    };
    let id: Option<String> = conn
        .query_row("SELECT id FROM scim_tokens WHERE token_hash=?1 AND revoked_at IS NULL", [super::mfa::sha256_hex(tok)], |r| r.get(0))
        .optional()?;
    if let Some(id) = &id {
        conn.execute("UPDATE scim_tokens SET last_used_at=?1 WHERE id=?2", rusqlite::params![now(), id])?;
    }
    Ok(id)
}

fn actor(token_id: &str) -> String {
    format!("policy:scim:{token_id}")
}

fn record(conn: &Connection, token_id: &str, action: &str, resource: &str, outcome: &str) {
    super::ledger::append(conn, &super::ledger::Event {
        event_id: format!("identity:{action}:{}", ulid::Ulid::new()),
        ts_ms: chrono::Utc::now().timestamp_millis(),
        kind: "identity".into(),
        actor: actor(token_id),
        on_behalf_of: Some("owner".into()),
        authority: Some("owner_policy".into()),
        action: Some(action.into()),
        resource: Some(resource.into()),
        decision: Some("allowed".into()),
        outcome: Some(outcome.into()),
        ..Default::default()
    });
}

/// One SCIM call: authenticate, then run `f` in a single write. `f` answers
/// `(status, body)`; side effects that must run outside the write (offboarding
/// has its own) are returned as `after`.
async fn call<F>(state: &AppState, headers: &HeaderMap, f: F) -> Result<(String, Value), Response>
where
    F: FnOnce(&Connection, &str) -> rusqlite::Result<Value> + Send + 'static,
{
    counters::bump(&counters::REQUESTS);
    let h = headers.clone();
    let res = super::mfa::write_value(state, move |c| {
        let Some(id) = authenticate(c, &h)? else {
            return Ok(json!({"ok": false, "unauthorized": true}));
        };
        let mut v = f(c, &id)?;
        v["token_id"] = json!(id);
        v["ok"] = json!(true);
        Ok(v)
    })
    .await
    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, &e, None))?;
    if res["unauthorized"] == json!(true) {
        counters::bump(&counters::UNAUTHORIZED);
        tracing::warn!(verdict = "scim_unauthorized", "a SCIM call presented no live SCIM token");
        return Err(error(StatusCode::UNAUTHORIZED, "a live SCIM bearer token is required", None));
    }
    Ok((res["token_id"].as_str().unwrap_or("").to_string(), res))
}

/// Turn what `call` returned into the SCIM response it describes.
fn reply(v: &Value) -> Response {
    if let Some(e) = v.get("error") {
        let status = StatusCode::from_u16(e["status"].as_u64().unwrap_or(400) as u16).unwrap_or(StatusCode::BAD_REQUEST);
        return error(status, e["detail"].as_str().unwrap_or(""), e["scimType"].as_str());
    }
    let status = StatusCode::from_u16(v["status"].as_u64().unwrap_or(200) as u16).unwrap_or(StatusCode::OK);
    if status == StatusCode::NO_CONTENT {
        return StatusCode::NO_CONTENT.into_response();
    }
    scim(status, v["body"].clone())
}

fn fail(status: u16, detail: &str, scim_type: Option<&str>) -> Value {
    json!({"error": {"status": status, "detail": detail, "scimType": scim_type}})
}

// ── Users ───────────────────────────────────────────────────────────────────

fn user_json(conn: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    conn.query_row(
        "SELECT m.id, m.email, COALESCE(m.name,''), m.joined_at, m.offboarded_at, COALESCE(m.external_id,''), \
                COALESCE(t.id,''), COALESCE(t.name,'') \
         FROM org_members m LEFT JOIN org_teams t ON t.id=m.team_id WHERE m.id=?1",
        [id],
        |r| {
            let (id, email, name, joined, off, ext, tid, tname): (String, String, String, i64, Option<i64>, String, String, String) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?);
            let mut u = json!({
                "schemas": [USER_SCHEMA],
                "id": id,
                "userName": email,
                "displayName": name,
                "name": {"formatted": name},
                "emails": [{"value": email, "primary": true, "type": "work"}],
                "active": off.is_none(),
                "groups": if tid.is_empty() { json!([]) } else { json!([{"value": tid, "display": tname}]) },
                "meta": {"resourceType": "User", "created": iso(joined), "location": format!("/scim/v2/Users/{id}")},
            });
            if !ext.is_empty() {
                u["externalId"] = json!(ext);
            }
            Ok(u)
        },
    )
    .optional()
}

/// `attr eq "value"`, the one filter providers send to look a person up.
pub fn parse_filter(f: &str) -> Option<(String, String)> {
    let lower = f.to_ascii_lowercase();
    let at = lower.find(" eq ")?;
    let attr = f[..at].trim().to_string();
    let val = f[at + 4..].trim().trim_matches('"').replace("\\\"", "\"");
    Some((attr, val))
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<i64>,
    count: Option<i64>,
}

fn list_users(conn: &Connection, q: &ListQuery) -> rusqlite::Result<Value> {
    let (clause, arg) = match q.filter.as_deref().and_then(parse_filter) {
        Some((a, v)) => match a.to_ascii_lowercase().as_str() {
            "username" | "emails.value" | "emails" => ("WHERE lower(email)=lower(?1)", v),
            "externalid" => ("WHERE external_id=?1", v),
            "id" => ("WHERE id=?1", v),
            _ => ("WHERE 0 AND ?1 IS NOT NULL", v),
        },
        None => ("WHERE ?1 IS NOT NULL", String::new()),
    };
    page(conn, &format!("SELECT id FROM org_members {clause} ORDER BY joined_at, id"), &arg, q, user_json)
}

fn page(
    conn: &Connection,
    sql: &str,
    arg: &str,
    q: &ListQuery,
    one: fn(&Connection, &str) -> rusqlite::Result<Option<Value>>,
) -> rusqlite::Result<Value> {
    let ids: Vec<String> = conn.prepare(sql)?.query_map([arg], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let start = q.start_index.unwrap_or(1).max(1);
    let count = q.count.unwrap_or(100).clamp(0, MAX_PAGE);
    let mut res = Vec::new();
    for id in ids.iter().skip((start - 1) as usize).take(count as usize) {
        if let Some(u) = one(conn, id)? {
            res.push(u);
        }
    }
    Ok(json!({"schemas": [LIST_SCHEMA], "totalResults": ids.len(), "startIndex": start, "itemsPerPage": res.len(), "Resources": res}))
}

fn truthy(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn display_name(body: &Value) -> Option<String> {
    body["displayName"]
        .as_str()
        .or_else(|| body["name"]["formatted"].as_str())
        .map(str::to_string)
        .or_else(|| {
            let g = body["name"]["givenName"].as_str().unwrap_or("");
            let f = body["name"]["familyName"].as_str().unwrap_or("");
            let n = format!("{g} {f}").trim().to_string();
            (!n.is_empty()).then_some(n)
        })
        .map(|n| n.chars().take(80).collect())
}

/// The new person's starting team: `AMUX_SCIM_DEFAULT_TEAM`, or none.
fn default_team(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let Some(t) = std::env::var("AMUX_SCIM_DEFAULT_TEAM").ok().filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    conn.query_row("SELECT id FROM org_teams WHERE id=?1 OR name=?1 COLLATE NOCASE", [t.trim()], |r| r.get(0)).optional()
}

/// Create a person. Returns the SCIM body, or a SCIM failure.
pub fn create_user(conn: &Connection, token_id: &str, body: &Value) -> rusqlite::Result<Value> {
    let Some(email) = body["userName"].as_str().map(|e| e.trim().to_ascii_lowercase()).filter(|e| e.contains('@') && !e.contains(char::is_whitespace)) else {
        return Ok(fail(400, "userName must be the person's email address", Some("invalidValue")));
    };
    let exists: Option<String> = conn.query_row("SELECT id FROM org_members WHERE lower(email)=?1", [&email], |r| r.get(0)).optional()?;
    if exists.is_some() {
        return Ok(fail(409, "a member with this userName already exists", Some("uniqueness")));
    }
    let id = ulid::Ulid::new().to_string().to_lowercase();
    let name = display_name(body).unwrap_or_else(|| email.split('@').next().unwrap_or("").to_string());
    let team = default_team(conn)?;
    let (level, scope) = if team.is_some() { ("global", "") } else { ("worker", UNASSIGNED) };
    let at = now();
    let active = truthy(&body["active"]).unwrap_or(true);
    conn.execute(
        "INSERT INTO org_members (id,email,name,role,joined_at,scope_level,scope_name,team_id,external_id,provisioned_by,offboarded_at,offboarded_by) \
         VALUES (?1,?2,?3,'member',?4,?5,?6,?7,?8,?9,?10,?11)",
        rusqlite::params![
            id, email, name, at, level, scope, team.unwrap_or_default(),
            body["externalId"].as_str(), actor(token_id),
            (!active).then_some(at), (!active).then(|| actor(token_id)),
        ],
    )?;
    record(conn, token_id, "scim_user_created", &format!("member:{email}"), if active { "applied" } else { "applied_inactive" });
    Ok(json!({"status": 201, "body": user_json(conn, &id)?}))
}

/// What a PATCH or PUT asks of a person.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UserChange {
    pub active: Option<bool>,
    pub name: Option<String>,
    pub external_id: Option<String>,
    pub user_name: Option<String>,
}

/// Read a SCIM PatchOp (Okta and Entra spellings both) into one change.
pub fn patch_user_change(body: &Value) -> Result<UserChange, String> {
    let mut c = UserChange::default();
    let ops = body["Operations"].as_array().ok_or("a PatchOp needs Operations")?;
    for op in ops {
        let kind = op["op"].as_str().unwrap_or("").to_ascii_lowercase();
        if kind != "replace" && kind != "add" {
            return Err(format!("op {kind} is not supported on a User"));
        }
        let path = op["path"].as_str().unwrap_or("");
        let pairs: Vec<(String, Value)> = if path.is_empty() {
            op["value"].as_object().ok_or("a PatchOp with no path needs an object value")?.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        } else {
            vec![(path.to_string(), op["value"].clone())]
        };
        for (k, v) in pairs {
            match k.as_str() {
                "active" => c.active = Some(truthy(&v).ok_or("active must be true or false")?),
                "displayName" | "name.formatted" => c.name = v.as_str().map(str::to_string),
                "name" => c.name = display_name(&json!({"name": v})),
                "externalId" => c.external_id = v.as_str().map(str::to_string),
                "userName" => c.user_name = v.as_str().map(|s| s.trim().to_ascii_lowercase()),
                _ => {} // attributes amux does not keep are ignored, as RFC 7644 3.5.2 allows
            }
        }
    }
    Ok(c)
}

/// Apply a change inside the write, except deactivation, which is returned
/// for `offboard_run` (it does its own write and the apps file).
fn apply_user_change(conn: &Connection, token_id: &str, id: &str, c: &UserChange) -> rusqlite::Result<Value> {
    let row: Option<(String, Option<i64>)> =
        conn.query_row("SELECT email, offboarded_at FROM org_members WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let Some((email, off)) = row else {
        return Ok(fail(404, "no such User", None));
    };
    if c.user_name.as_deref().is_some_and(|u| u != email) {
        return Ok(fail(400, "userName is the person's identity here and cannot change; deprovision and provision again", Some("mutability")));
    }
    if let Some(n) = &c.name {
        conn.execute("UPDATE org_members SET name=?1 WHERE id=?2", rusqlite::params![n, id])?;
    }
    if let Some(x) = &c.external_id {
        conn.execute("UPDATE org_members SET external_id=?1 WHERE id=?2", rusqlite::params![x, id])?;
    }
    let mut deactivate = false;
    match (c.active, off.is_some()) {
        (Some(false), false) => deactivate = true,
        (Some(true), true) => {
            conn.execute("UPDATE org_members SET offboarded_at=NULL, offboarded_by=NULL WHERE id=?1", [id])?;
            record(conn, token_id, "scim_user_reactivated", &format!("member:{email}"), "applied");
            counters::bump(&counters::REACTIVATED);
        }
        _ => {}
    }
    if c.name.is_some() || c.external_id.is_some() {
        record(conn, token_id, "scim_user_updated", &format!("member:{email}"), "applied");
    }
    Ok(json!({"status": 200, "email": email, "deactivate": deactivate}))
}

async fn finish_user(state: &AppState, token_id: &str, id: &str, v: Value, deleting: bool) -> Response {
    if v.get("error").is_some() {
        return reply(&v);
    }
    let email = v["email"].as_str().unwrap_or("").to_string();
    if v["deactivate"] == json!(true) || deleting {
        match schedule_authority::offboard_run(state, &email, &actor(token_id), false).await {
            Ok(_) => {
                counters::bump(&counters::DEACTIVATED);
                tracing::warn!(member = %email, verdict = "scim_deprovisioned", "the identity provider deprovisioned a member; they were offboarded");
            }
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e, None),
        }
    }
    let (id2, tok2, email2) = (id.to_string(), token_id.to_string(), email.clone());
    let res = super::mfa::write_value(state, move |c| {
        if deleting {
            c.execute("DELETE FROM org_members WHERE id=?1", [&id2])?;
            record(c, &tok2, "scim_user_deleted", &format!("member:{email2}"), "applied");
            return Ok(json!({"ok": true, "status": 204}));
        }
        Ok(json!({"ok": true, "status": 200, "body": user_json(c, &id2)?}))
    })
    .await;
    match res {
        Ok(v) => {
            if deleting {
                counters::bump(&counters::DELETED);
            }
            reply(&v)
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e, None),
    }
}

fn parse_body(body: &Bytes) -> Result<Value, Box<Response>> {
    serde_json::from_slice(body).map_err(|e| Box::new(error(StatusCode::BAD_REQUEST, &format!("body is not JSON: {e}"), Some("invalidSyntax"))))
}

async fn users_get(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    match call(&state, &headers, move |c, _| Ok(json!({"body": list_users(c, &q)?}))).await {
        Ok((_, v)) => reply(&v),
        Err(r) => r,
    }
}

async fn users_post(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    match call(&state, &headers, move |c, tok| create_user(c, tok, &b)).await {
        Ok((_, v)) => {
            if v.get("error").is_none() {
                counters::bump(&counters::CREATED);
            }
            reply(&v)
        }
        Err(r) => r,
    }
}

async fn user_get(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    match call(&state, &headers, move |c, _| {
        Ok(match user_json(c, &id)? {
            Some(u) => json!({"body": u}),
            None => fail(404, "no such User", None),
        })
    })
    .await
    {
        Ok((_, v)) => reply(&v),
        Err(r) => r,
    }
}

async fn user_change(state: AppState, headers: HeaderMap, id: String, change: Result<UserChange, String>) -> Response {
    let change = match change {
        Ok(c) => c,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e, Some("invalidSyntax")),
    };
    let id2 = id.clone();
    match call(&state, &headers, move |c, tok| apply_user_change(c, tok, &id2, &change)).await {
        Ok((tok, v)) => finish_user(&state, &tok, &id, v, false).await,
        Err(r) => r,
    }
}

async fn user_patch(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>, body: Bytes) -> Response {
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    if !b["schemas"].as_array().is_some_and(|s| s.iter().any(|x| x == PATCH_SCHEMA)) && b.get("Operations").is_none() {
        return error(StatusCode::BAD_REQUEST, "a PATCH body is a PatchOp", Some("invalidSyntax"));
    }
    user_change(state, headers, id, patch_user_change(&b)).await
}

async fn user_put(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>, body: Bytes) -> Response {
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let change = UserChange {
        active: truthy(&b["active"]),
        name: display_name(&b),
        external_id: b["externalId"].as_str().map(str::to_string),
        user_name: b["userName"].as_str().map(|s| s.trim().to_ascii_lowercase()),
    };
    user_change(state, headers, id, Ok(change)).await
}

async fn user_delete(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let id2 = id.clone();
    match call(&state, &headers, move |c, _| {
        let email: Option<String> = c.query_row("SELECT email FROM org_members WHERE id=?1", [&id2], |r| r.get(0)).optional()?;
        Ok(match email {
            Some(e) => json!({"email": e}),
            None => fail(404, "no such User", None),
        })
    })
    .await
    {
        Ok((tok, v)) => finish_user(&state, &tok, &id, v, true).await,
        Err(r) => r,
    }
}

// ── Groups ──────────────────────────────────────────────────────────────────

fn group_json(conn: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    let Some((name, created)): Option<(String, i64)> =
        conn.query_row("SELECT name, created_at FROM org_teams WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
    else {
        return Ok(None);
    };
    let members: Vec<Value> = conn
        .prepare("SELECT id, email FROM org_members WHERE team_id=?1 ORDER BY email")?
        .query_map([id], |r| Ok(json!({"value": r.get::<_, String>(0)?, "display": r.get::<_, String>(1)?})))?
        .collect::<Result<_, _>>()?;
    Ok(Some(json!({
        "schemas": [GROUP_SCHEMA],
        "id": id,
        "displayName": name,
        "members": members,
        "meta": {"resourceType": "Group", "created": iso(created), "location": format!("/scim/v2/Groups/{id}")},
    })))
}

fn list_groups(conn: &Connection, q: &ListQuery) -> rusqlite::Result<Value> {
    let (clause, arg) = match q.filter.as_deref().and_then(parse_filter) {
        Some((a, v)) if a.eq_ignore_ascii_case("displayName") => ("WHERE name=?1 COLLATE NOCASE", v),
        Some((a, v)) if a.eq_ignore_ascii_case("id") => ("WHERE id=?1", v),
        Some((_, v)) => ("WHERE 0 AND ?1 IS NOT NULL", v),
        None => ("WHERE ?1 IS NOT NULL", String::new()),
    };
    page(conn, &format!("SELECT id FROM org_teams {clause} ORDER BY created_at, id"), &arg, q, group_json)
}

/// Put a person in a team (they leave their previous one), or take them out
/// of it with no access left.
fn set_membership(conn: &Connection, token_id: &str, team: &str, member: &str, add: bool) -> rusqlite::Result<bool> {
    let email: Option<String> = conn.query_row("SELECT email FROM org_members WHERE id=?1", [member], |r| r.get(0)).optional()?;
    let Some(email) = email else { return Ok(false) };
    let n = if add {
        conn.execute("UPDATE org_members SET team_id=?1 WHERE id=?2", rusqlite::params![team, member])?
    } else {
        conn.execute(
            "UPDATE org_members SET team_id='', scope_level='worker', scope_name=?1 WHERE id=?2 AND team_id=?3",
            rusqlite::params![UNASSIGNED, member, team],
        )?
    };
    if n > 0 {
        record(conn, token_id, if add { "scim_group_member_added" } else { "scim_group_member_removed" }, &format!("member:{email} team:{team}"), "applied");
        counters::bump(&counters::GROUP_CHANGES);
    }
    Ok(n > 0)
}

fn create_group(conn: &Connection, token_id: &str, body: &Value) -> rusqlite::Result<Value> {
    let Some(name) = body["displayName"].as_str().map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(fail(400, "displayName is required", Some("invalidValue")));
    };
    let taken: Option<String> = conn.query_row("SELECT id FROM org_teams WHERE name=?1 COLLATE NOCASE", [name], |r| r.get(0)).optional()?;
    if taken.is_some() {
        return Ok(fail(409, "a group with this displayName already exists", Some("uniqueness")));
    }
    let id = format!("team_{}", ulid::Ulid::new().to_string().to_lowercase());
    // No access until the owner gives the team a scope in Team settings.
    conn.execute(
        "INSERT INTO org_teams (id,name,scope_level,scope_name,created_at) VALUES (?1,?2,'worker',?3,?4)",
        rusqlite::params![id, name, UNASSIGNED, now()],
    )?;
    record(conn, token_id, "scim_group_created", &format!("team:{id}"), "applied");
    for m in body["members"].as_array().into_iter().flatten() {
        if let Some(mid) = m["value"].as_str() {
            set_membership(conn, token_id, &id, mid, true)?;
        }
    }
    Ok(json!({"status": 201, "body": group_json(conn, &id)?}))
}

/// `members[value eq "id"]` names one member in a remove path.
fn path_member(path: &str) -> Option<String> {
    let inner = path.strip_prefix("members[")?.strip_suffix(']')?;
    parse_filter(inner).filter(|(a, _)| a == "value").map(|(_, v)| v)
}

fn patch_group(conn: &Connection, token_id: &str, id: &str, body: &Value) -> rusqlite::Result<Value> {
    if conn.query_row("SELECT 1 FROM org_teams WHERE id=?1", [id], |_| Ok(())).optional()?.is_none() {
        return Ok(fail(404, "no such Group", None));
    }
    let Some(ops) = body["Operations"].as_array() else {
        return Ok(fail(400, "a PatchOp needs Operations", Some("invalidSyntax")));
    };
    for op in ops {
        let kind = op["op"].as_str().unwrap_or("").to_ascii_lowercase();
        let path = op["path"].as_str().unwrap_or("");
        let values: Vec<String> = op["value"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["value"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        match (kind.as_str(), path) {
            ("add", "members") => {
                for m in &values {
                    set_membership(conn, token_id, id, m, true)?;
                }
            }
            ("remove", "members") => {
                for m in &values {
                    set_membership(conn, token_id, id, m, false)?;
                }
            }
            ("remove", p) if path_member(p).is_some() => {
                set_membership(conn, token_id, id, &path_member(p).unwrap_or_default(), false)?;
            }
            ("replace", "members") => {
                let current: Vec<String> =
                    conn.prepare("SELECT id FROM org_members WHERE team_id=?1")?.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?;
                for m in current.iter().filter(|m| !values.contains(m)) {
                    set_membership(conn, token_id, id, m, false)?;
                }
                for m in values.iter().filter(|m| !current.contains(m)) {
                    set_membership(conn, token_id, id, m, true)?;
                }
            }
            ("replace", "displayName") | ("replace", "") => {
                let name = op["value"].as_str().or_else(|| op["value"]["displayName"].as_str());
                if let Some(n) = name.map(str::trim).filter(|n| !n.is_empty()) {
                    conn.execute("UPDATE org_teams SET name=?1 WHERE id=?2", rusqlite::params![n, id])?;
                    record(conn, token_id, "scim_group_renamed", &format!("team:{id}"), "applied");
                }
            }
            _ => return Ok(fail(400, &format!("op {kind} {path} is not supported on a Group"), Some("invalidPath"))),
        }
    }
    Ok(json!({"status": 200, "body": group_json(conn, id)?}))
}

fn delete_group(conn: &Connection, token_id: &str, id: &str) -> rusqlite::Result<Value> {
    if id == "team_global" {
        return Ok(fail(400, "the Everyone team is the owner's and cannot be deleted by a provider", Some("mutability")));
    }
    if conn.query_row("SELECT 1 FROM org_teams WHERE id=?1", [id], |_| Ok(())).optional()?.is_none() {
        return Ok(fail(404, "no such Group", None));
    }
    let members: Vec<String> = conn.prepare("SELECT id FROM org_members WHERE team_id=?1")?.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for m in &members {
        set_membership(conn, token_id, id, m, false)?;
    }
    conn.execute("DELETE FROM org_teams WHERE id=?1", [id])?;
    record(conn, token_id, "scim_group_deleted", &format!("team:{id}"), "applied");
    Ok(json!({"status": 204}))
}

async fn groups_get(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    match call(&state, &headers, move |c, _| Ok(json!({"body": list_groups(c, &q)?}))).await {
        Ok((_, v)) => reply(&v),
        Err(r) => r,
    }
}

async fn groups_post(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    match call(&state, &headers, move |c, tok| create_group(c, tok, &b)).await {
        Ok((_, v)) => reply(&v),
        Err(r) => r,
    }
}

async fn group_get(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    match call(&state, &headers, move |c, _| {
        Ok(match group_json(c, &id)? {
            Some(g) => json!({"body": g}),
            None => fail(404, "no such Group", None),
        })
    })
    .await
    {
        Ok((_, v)) => reply(&v),
        Err(r) => r,
    }
}

async fn group_patch(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>, body: Bytes) -> Response {
    let b = match parse_body(&body) {
        Ok(b) => b,
        Err(r) => return *r,
    };
    match call(&state, &headers, move |c, tok| patch_group(c, tok, &id, &b)).await {
        Ok((_, v)) => reply(&v),
        Err(r) => r,
    }
}

async fn group_delete(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    match call(&state, &headers, move |c, tok| delete_group(c, tok, &id)).await {
        Ok((_, v)) => reply(&v),
        Err(r) => r,
    }
}

async fn service_provider_config() -> Response {
    scim(
        StatusCode::OK,
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
            "patch": {"supported": true},
            "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
            "filter": {"supported": true, "maxResults": MAX_PAGE},
            "changePassword": {"supported": false},
            "sort": {"supported": false},
            "etag": {"supported": false},
            "authenticationSchemes": [{"type": "oauthbearertoken", "name": "OAuth Bearer Token",
                "description": "a token the owner issues at POST /api/scim/tokens", "primary": true}],
        }),
    )
}

/// `/scim/v2`, outside the owner's auth: the provider's own token is checked
/// on every call.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/scim/v2/ServiceProviderConfig", get(service_provider_config))
        .route("/scim/v2/Users", get(users_get).post(users_post))
        .route("/scim/v2/Users/{id}", get(user_get).patch(user_patch).put(user_put).delete(user_delete))
        .route("/scim/v2/Groups", get(groups_get).post(groups_post))
        .route("/scim/v2/Groups/{id}", get(group_get).patch(group_patch).delete(group_delete))
}

// ── Tokens (the owner's) ────────────────────────────────────────────────────

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/scim/tokens", get(tokens_list).post(token_issue))
        .route("/api/scim/tokens/{id}", delete(token_revoke))
}

fn owner_only(headers: &HeaderMap, what: &str) -> Option<Response> {
    if matches!(super::human_proof::proven(headers), Some(super::human_proof::Proven::Owner)) {
        return super::mfa::step_up_gate(headers, what);
    }
    Some((StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": "owner_only", "error": format!("{what} is the owner's alone")}))).into_response())
}

#[derive(Deserialize, Default)]
pub struct TokenBody {
    #[serde(default)]
    label: String,
}

async fn token_issue(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<TokenBody>) -> Response {
    if let Some(r) = owner_only(&headers, "issuing a SCIM token") {
        return r;
    }
    let mut raw = [0u8; 32];
    if getrandom::fill(&mut raw).is_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "error": "the OS random source failed"}))).into_response();
    }
    let token = format!("scim_{}", hex::encode(raw));
    let id = format!("scimtok_{}", ulid::Ulid::new().to_string().to_lowercase());
    let (t2, id2) = (token.clone(), id.clone());
    let label: String = b.label.chars().take(80).collect();
    let res = super::mfa::write_value(&state, move |c| {
        c.execute(
            "INSERT INTO scim_tokens (id, token_hash, label, created_by, created_at) VALUES (?1,?2,?3,'owner',?4)",
            rusqlite::params![id2, super::mfa::sha256_hex(&t2), label, now()],
        )?;
        super::mfa::record(c, "owner", "owner_credential", "scim_token_issued", &format!("scim_token:{id2}"), "allowed", "applied");
        Ok(json!({"ok": true}))
    })
    .await;
    match res {
        Ok(_) => {
            counters::bump(&counters::TOKENS_ISSUED);
            Json(json!({"ok": true, "id": id, "token": token, "base_url": "/scim/v2",
                "note": "shown once; amux keeps only its hash"})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e}))).into_response(),
    }
}

async fn tokens_list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !matches!(super::human_proof::proven(&headers), Some(super::human_proof::Proven::Owner)) {
        return (StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": "owner_only"}))).into_response();
    }
    let rows = state
        .store
        .read_async(|c| {
            let v: Vec<Value> = c
                .prepare("SELECT id, label, created_at, last_used_at, revoked_at FROM scim_tokens ORDER BY created_at")?
                .query_map([], |r| {
                    Ok(json!({"id": r.get::<_, String>(0)?, "label": r.get::<_, String>(1)?, "created_at": r.get::<_, i64>(2)?,
                        "last_used_at": r.get::<_, Option<i64>>(3)?, "revoked_at": r.get::<_, Option<i64>>(4)?}))
                })?
                .collect::<Result<_, _>>()?;
            Ok(v)
        })
        .await
        .unwrap_or_default();
    Json(json!({"ok": true, "tokens": rows, "measured": true, "n_considered": rows.len()})).into_response()
}

async fn token_revoke(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    if let Some(r) = owner_only(&headers, "revoking a SCIM token") {
        return r;
    }
    let res = super::mfa::write_value(&state, move |c| {
        let n = c.execute("UPDATE scim_tokens SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL", rusqlite::params![now(), id])?;
        if n > 0 {
            super::mfa::record(c, "owner", "owner_credential", "scim_token_revoked", &format!("scim_token:{id}"), "allowed", "applied");
        }
        Ok(json!({"ok": true, "revoked": n > 0}))
    })
    .await;
    match res {
        Ok(v) => {
            if v["revoked"] == json!(true) {
                counters::bump(&counters::TOKENS_REVOKED);
            }
            Json(v).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e}))).into_response(),
    }
}

/// `scim` in `/api/contract/counters`.
pub fn report() -> Value {
    use counters::*;
    json!({
        "measured": true,
        "n_considered": get(&REQUESTS),
        "default_team_env": "AMUX_SCIM_DEFAULT_TEAM",
        "counts": {
            "requests": get(&REQUESTS),
            "unauthorized": get(&UNAUTHORIZED),
            "created": get(&CREATED),
            "deactivated": get(&DEACTIVATED),
            "reactivated": get(&REACTIVATED),
            "deleted": get(&DELETED),
            "group_changes": get(&GROUP_CHANGES),
            "tokens_issued": get(&TOKENS_ISSUED),
            "tokens_revoked": get(&TOKENS_REVOKED),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    fn token(c: &Connection) -> (String, HeaderMap) {
        c.execute("INSERT INTO scim_tokens (id, token_hash, created_by, created_at) VALUES ('t1', ?1, 'owner', 0)", [super::super::mfa::sha256_hex("scim_abc")]).unwrap();
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer scim_abc".parse().unwrap());
        ("t1".into(), h)
    }

    #[test]
    fn only_a_live_token_authenticates() {
        let c = db();
        let (_, h) = token(&c);
        assert_eq!(authenticate(&c, &h).unwrap().as_deref(), Some("t1"));
        let mut wrong = HeaderMap::new();
        wrong.insert(header::AUTHORIZATION, "Bearer scim_abd".parse().unwrap());
        assert_eq!(authenticate(&c, &wrong).unwrap(), None);
        assert_eq!(authenticate(&c, &HeaderMap::new()).unwrap(), None);
        c.execute("UPDATE scim_tokens SET revoked_at=1", []).unwrap();
        assert_eq!(authenticate(&c, &h).unwrap(), None, "a revoked token");
        let stored: String = c.query_row("SELECT token_hash FROM scim_tokens", [], |r| r.get(0)).unwrap();
        assert!(!stored.contains("abc"));
    }

    #[test]
    fn create_starts_with_no_access_and_rejects_duplicates() {
        let c = db();
        let v = create_user(&c, "t1", &json!({"userName": "New@X.io", "name": {"givenName": "N", "familyName": "W"}, "externalId": "okta-1"})).unwrap();
        assert_eq!(v["status"], 201);
        assert_eq!(v["body"]["userName"], "new@x.io");
        assert_eq!(v["body"]["active"], true);
        assert_eq!(v["body"]["externalId"], "okta-1");
        let (level, name): (String, String) = c.query_row("SELECT scope_level, scope_name FROM org_members", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((level.as_str(), name.as_str()), ("worker", UNASSIGNED));
        assert_eq!(create_user(&c, "t1", &json!({"userName": "new@x.io"})).unwrap()["error"]["status"], 409);
        assert_eq!(create_user(&c, "t1", &json!({"userName": "nope"})).unwrap()["error"]["status"], 400);
        let listed = list_users(&c, &ListQuery { filter: Some("userName eq \"NEW@x.io\"".into()), ..Default::default() }).unwrap();
        assert_eq!(listed["totalResults"], 1);
        let none = list_users(&c, &ListQuery { filter: Some("userName eq \"other@x.io\"".into()), ..Default::default() }).unwrap();
        assert_eq!(none["totalResults"], 0);
    }

    #[test]
    fn patch_reads_okta_and_entra_spellings() {
        let okta = json!({"schemas": [PATCH_SCHEMA], "Operations": [{"op": "replace", "value": {"active": false}}]});
        assert_eq!(patch_user_change(&okta).unwrap().active, Some(false));
        let entra = json!({"Operations": [{"op": "Replace", "path": "active", "value": "False"}]});
        assert_eq!(patch_user_change(&entra).unwrap().active, Some(false));
        let rename = json!({"Operations": [{"op": "replace", "path": "displayName", "value": "Bea"}]});
        assert_eq!(patch_user_change(&rename).unwrap().name.as_deref(), Some("Bea"));
        assert!(patch_user_change(&json!({"Operations": [{"op": "remove", "path": "active"}]})).is_err());
    }

    #[test]
    fn deactivate_is_handed_to_offboarding_and_reactivate_clears_it() {
        let c = db();
        let v = create_user(&c, "t1", &json!({"userName": "a@x.io"})).unwrap();
        let id = v["body"]["id"].as_str().unwrap().to_string();
        let r = apply_user_change(&c, "t1", &id, &UserChange { active: Some(false), ..Default::default() }).unwrap();
        assert_eq!(r["deactivate"], true, "deactivation goes through offboard_run");
        c.execute("UPDATE org_members SET offboarded_at=5", []).unwrap();
        let r = apply_user_change(&c, "t1", &id, &UserChange { active: Some(true), ..Default::default() }).unwrap();
        assert_eq!(r["deactivate"], false);
        let off: Option<i64> = c.query_row("SELECT offboarded_at FROM org_members", [], |r| r.get(0)).unwrap();
        assert_eq!(off, None);
        let r = apply_user_change(&c, "t1", &id, &UserChange { user_name: Some("b@x.io".into()), ..Default::default() }).unwrap();
        assert_eq!(r["error"]["scimType"], "mutability");
    }

    #[test]
    fn groups_are_teams_with_no_access_until_the_owner_scopes_them() {
        let c = db();
        let a = create_user(&c, "t1", &json!({"userName": "a@x.io"})).unwrap()["body"]["id"].as_str().unwrap().to_string();
        let g = create_group(&c, "t1", &json!({"displayName": "Finance", "members": [{"value": a}]})).unwrap();
        let gid = g["body"]["id"].as_str().unwrap().to_string();
        assert_eq!(g["body"]["members"][0]["value"], a);
        let (level, name): (String, String) = c.query_row("SELECT scope_level, scope_name FROM org_teams WHERE id=?1", [&gid], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((level.as_str(), name.as_str()), ("worker", UNASSIGNED));
        let p = patch_group(&c, "t1", &gid, &json!({"Operations": [{"op": "remove", "path": format!("members[value eq \"{a}\"]")}]})).unwrap();
        assert_eq!(p["body"]["members"], json!([]));
        let team: String = c.query_row("SELECT team_id FROM org_members WHERE id=?1", [&a], |r| r.get(0)).unwrap();
        assert_eq!(team, "");
        assert_eq!(delete_group(&c, "t1", "team_global").unwrap()["error"]["status"], 400);
        assert_eq!(delete_group(&c, "t1", &gid).unwrap()["status"], 204);
        assert_eq!(create_group(&c, "t1", &json!({"displayName": "everyone"})).unwrap()["error"]["status"], 409);
    }

    #[test]
    fn filters_parse() {
        assert_eq!(parse_filter("userName eq \"a@x.io\""), Some(("userName".into(), "a@x.io".into())));
        assert_eq!(parse_filter("displayName EQ \"Fin\""), Some(("displayName".into(), "Fin".into())));
        assert_eq!(path_member("members[value eq \"m1\"]").as_deref(), Some("m1"));
    }
}
