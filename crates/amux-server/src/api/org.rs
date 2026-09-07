//! Org API (SPA long-tail port): `/api/org*` over the LIVE `org` /
//! `org_members` / `org_invites` tables, route- and field-compatible with
//! the Python handlers — the cloud gateway consumes these shapes.
//!
//! Parity decisions, recorded so they are not "fixed" later:
//! - `GET /api/org` LAZILY CREATES the singleton `('default', 'My
//!   Workspace')` row, exactly like Python's `_get_org()` — the org exists
//!   the first time anyone asks about it.
//! - Invite URLs are built from the REQUEST's `Host` header +
//!   `X-Forwarded-Proto` (https only when the gateway says https, http
//!   otherwise, default host `localhost:<canonical port>`) — Python's shape
//!   `f"{scheme}://{host}/invite/{token}"`, with the fallback host derived
//!   from this server's own port instead of Python's 8822 literal.
//! - Tokens are `secrets.token_urlsafe(24)`-shaped (24 CSPRNG bytes,
//!   base64url, no padding — 32 chars); invites expire in 7 days; the
//!   invites list hides used AND expired rows.
//! - DELETE member/invite answer `{"ok": true}` without existence checks
//!   (Python does not 404 there).
//! - `/invite/{token}` is the public landing + accept flow. Acceptance mints
//!   an HttpOnly member cookie backed by the USED invite row; deleting the
//!   member therefore revokes every later request without another session
//!   table or auth primitive.

use super::calendar::query_rows_json;
use super::AppState;
use crate::db::{PendingEvent, WriteOutcome};
use crate::integrations::email::base64url_nopad;
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Form, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const MEMBER_COOKIE: &str = "amux_member";
const VERIFIED_MEMBER_HEADER: &str = "x-amux-local-member-verified";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_org).patch(patch_org))
        .route("/members", get(list_members))
        .route("/members/{id}", axum::routing::delete(delete_member))
        .route("/invites", get(list_invites).post(create_invite))
        .route("/invites/{token}", axum::routing::delete(delete_invite))
}

/// Public invite acceptance is mounted outside `require_bearer`.
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/invite/{token}", get(invite_page).post(accept_invite))
}

/// True only for the internal marker inserted by [`local_member_identity`].
/// The middleware removes an inbound copy before doing its database lookup, so
/// this cannot be asserted by a Tailscale/LAN client itself.
pub(crate) fn is_verified_local_member(headers: &HeaderMap) -> bool {
    headers.get(VERIFIED_MEMBER_HEADER).and_then(|v| v.to_str().ok()) == Some("1")
}

#[derive(Debug)]
struct MemberIdentity {
    id: String,
    email: String,
}

fn member_cookie(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|cookies| {
        cookies.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == MEMBER_COOKIE && !value.is_empty()).then_some(value)
        })
    })
}

/// Resolve a local invitee before auth and before the request logger.
///
/// A used invite is the durable session capability. Joining through
/// `org_members` on every request makes member deletion immediate revocation.
/// Verified headers then feed both `/api/identity` and the existing request-log
/// caller resolution; no parallel identity/logging substrate is introduced.
pub async fn local_member_identity(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // Never trust the internal marker from the wire.
    req.headers_mut().remove(VERIFIED_MEMBER_HEADER);
    let Some(token) = member_cookie(req.headers()).map(str::to_string) else {
        return next.run(req).await;
    };
    let store = state.store.clone();
    let identity = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<MemberIdentity>> {
        let conn = store.read()?;
        Ok(conn.query_row(
            "SELECT m.id, m.email FROM org_invites i \
             JOIN org_members m ON m.id=i.used_by \
             WHERE i.token=?1 AND i.used_at IS NOT NULL",
            [&token],
            |row| Ok(MemberIdentity { id: row.get(0)?, email: row.get(1)? }),
        ).optional()?)
    }).await;
    let member = match identity {
        Ok(Ok(Some(member))) => member,
        Ok(Ok(None)) => return next.run(req).await,
        Ok(Err(e)) => {
            tracing::warn!(target: "amux::local_invite", verdict = "identity_lookup_failed", error = %e, "local member cookie could not be verified");
            return next.run(req).await;
        }
        Err(e) => {
            tracing::warn!(target: "amux::local_invite", verdict = "identity_lookup_failed", error = %e, "local member identity task failed");
            return next.run(req).await;
        }
    };
    let Ok(id) = HeaderValue::from_str(&member.id) else {
        tracing::warn!(target: "amux::local_invite", verdict = "member_header_rejected", field = "id", "stored local member identity is not a valid HTTP header");
        return next.run(req).await;
    };
    let Ok(email) = HeaderValue::from_str(&member.email) else {
        tracing::warn!(target: "amux::local_invite", verdict = "member_header_rejected", field = "email", member_id = %member.id, "stored local member identity is not a valid HTTP header");
        return next.run(req).await;
    };
    req.headers_mut().insert(VERIFIED_MEMBER_HEADER, HeaderValue::from_static("1"));
    req.headers_mut().insert("x-amux-user-id", id);
    req.headers_mut().insert("x-amux-user-email", email);
    if !req.headers().contains_key("x-amux-worker") && !req.headers().contains_key("x-amux-session") {
        if let Ok(actor) = HeaderValue::from_str(&format!("member:{}", member.email)) {
            req.headers_mut().insert("x-amux-session", actor);
        }
    }
    next.run(req).await
}

// ---- shared helpers -------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn ev(entity: &str, id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Other(entity.into()),
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// `secrets.token_urlsafe(n)`: n CSPRNG bytes, base64url, no padding.
fn token_urlsafe(nbytes: usize) -> String {
    let mut bytes = vec![0u8; nbytes];
    OsRng.fill_bytes(&mut bytes);
    base64url_nopad(&bytes)
}

/// Python: `scheme = "https" if X-Forwarded-Proto == "https" else "http"`,
/// host from the Host header. The fallback host is this server's OWN port
/// (`config::canonical_port()`), not Python's 8822 literal — an invite link is
/// mailed to a person and outlives the process, so minting it against the
/// retired address hands out a URL with an expiry date on it.
fn base_url(headers: &HeaderMap) -> String {
    let fallback = format!("localhost:{}", crate::config::canonical_port());
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&fallback);
    let scheme = if headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        == Some("https")
    {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{host}")
}

/// Python `_get_org()`: SELECT-or-INSERT the singleton row. Returns whether
/// the row was created (so the write's applied/events stay honest).
fn ensure_org(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM org WHERE id='default'", [], |r| r.get(0))?;
    if n == 0 {
        conn.execute(
            "INSERT INTO org (id, name, created_at) VALUES ('default','My Workspace',?1)",
            [chrono::Utc::now().timestamp()],
        )?;
        return Ok(true);
    }
    Ok(false)
}

fn html_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&#39;")
}

fn valid_email(email: &str) -> bool {
    email.len() <= 254
        && !email.is_empty()
        && !email.chars().any(char::is_whitespace)
        && email.matches('@').count() == 1
        && email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
}

fn invite_fingerprint(token: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))[..12].to_string()
}

#[derive(Debug)]
struct InviteView { workspace: String, email: Option<String> }

#[derive(Debug)]
enum InviteLookup { Live(InviteView), Missing, Used, Expired }

fn lookup_invite(conn: &rusqlite::Connection, token: &str) -> rusqlite::Result<InviteLookup> {
    let row: Option<(Option<String>, i64, Option<i64>)> = conn.query_row(
        "SELECT email, expires_at, used_at FROM org_invites WHERE token=?1", [token],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    let Some((email, expires_at, used_at)) = row else { return Ok(InviteLookup::Missing) };
    if used_at.is_some() { return Ok(InviteLookup::Used); }
    if expires_at <= chrono::Utc::now().timestamp() { return Ok(InviteLookup::Expired); }
    let workspace = conn.query_row("SELECT name FROM org WHERE id='default'", [], |row| row.get(0))
        .optional()?.unwrap_or_else(|| "My Workspace".to_string());
    Ok(InviteLookup::Live(InviteView { workspace, email }))
}

fn invite_error(status: StatusCode, title: &str, detail: &str) -> Response {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><style>{}</style></head><body><main><div class=\"mark\">A</div><h1>{}</h1><p>{}</p></main></body></html>",
        html_escape(title), INVITE_CSS, html_escape(title), html_escape(detail),
    );
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

const INVITE_CSS: &str = r#"
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;padding:20px;
background:#0b0b0e;color:#eee;font:15px/1.5 ui-sans-serif,system-ui,-apple-system,sans-serif}
main{width:min(440px,100%);padding:32px;border:1px solid #303038;border-radius:16px;background:#15151a;
box-shadow:0 24px 80px #0008}.mark{display:grid;place-items:center;width:38px;height:38px;border-radius:10px;
background:#a78bfa;color:#0b0b0e;font-weight:800;margin-bottom:22px}h1{font-size:24px;line-height:1.2;margin:0 0 10px}
p{color:#aaa;margin:0 0 22px}label{display:block;color:#bbb;font-size:13px;margin:14px 0 6px}
input{width:100%;padding:11px 12px;border:1px solid #3b3b45;border-radius:8px;background:#0d0d11;color:#eee;font:inherit}
input:focus{outline:2px solid #a78bfa55;border-color:#a78bfa}button{width:100%;margin-top:22px;padding:12px;
border:0;border-radius:8px;background:#a78bfa;color:#0b0b0e;font:700 15px inherit;cursor:pointer}.note{font-size:12px;color:#777;margin-top:14px}
"#;

/// Public invite landing page. Tokens never appear in logs; rejected links use
/// a short one-way fingerprint so a sweep can group repeated failures without
/// turning the log into a credential store.
async fn invite_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let token_read = token.clone();
    let store = state.store.clone();
    let found = tokio::task::spawn_blocking(move || -> anyhow::Result<InviteLookup> {
        let conn = store.read()?;
        Ok(lookup_invite(&conn, &token_read)?)
    }).await;
    match found {
        Ok(Ok(InviteLookup::Live(invite))) => {
            let email = invite.email.as_deref().unwrap_or("");
            let readonly = if invite.email.is_some() { " readonly" } else { "" };
            let body = format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Join {workspace}</title><style>{css}</style></head><body><main><div class=\"mark\">A</div><h1>Join {workspace}</h1><p>You were invited to collaborate in this Amux workspace.</p><form method=\"post\"><label for=\"email\">Email</label><input id=\"email\" name=\"email\" type=\"email\" maxlength=\"254\" required autocomplete=\"email\" value=\"{email}\"{readonly}><label for=\"name\">Name</label><input id=\"name\" name=\"name\" maxlength=\"80\" autocomplete=\"name\" placeholder=\"How teammates will see you\"><button type=\"submit\">Join workspace</button></form><div class=\"note\">This signs this browser into this local Amux instance.</div></main></body></html>",
                workspace = html_escape(&invite.workspace), css = INVITE_CSS, email = html_escape(email),
            );
            (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
        }
        Ok(Ok(InviteLookup::Missing)) => {
            tracing::warn!(target: "amux::local_invite", verdict = "rejected", reason = "missing", invite = %invite_fingerprint(&token), "local invite rejected");
            invite_error(StatusCode::GONE, "Invite not found", "Ask the workspace owner for a new link.")
        }
        Ok(Ok(InviteLookup::Used)) => {
            tracing::warn!(target: "amux::local_invite", verdict = "rejected", reason = "used", invite = %invite_fingerprint(&token), "local invite rejected");
            invite_error(StatusCode::GONE, "Invite already used", "Ask the workspace owner for a new link.")
        }
        Ok(Ok(InviteLookup::Expired)) => {
            tracing::warn!(target: "amux::local_invite", verdict = "rejected", reason = "expired", invite = %invite_fingerprint(&token), "local invite rejected");
            invite_error(StatusCode::GONE, "Invite expired", "Ask the workspace owner for a new link.")
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "amux::local_invite", verdict = "landing_failed", error = %e, "local invite landing failed");
            internal(e)
        }
        Err(e) => {
            tracing::warn!(target: "amux::local_invite", verdict = "landing_failed", error = %e, "local invite landing task failed");
            internal(e)
        }
    }
}

#[derive(Debug, Deserialize)]
struct InviteAcceptForm {
    #[serde(default)] email: String,
    #[serde(default)] name: String,
}

#[derive(Debug)]
enum AcceptOutcome {
    Accepted { member_id: String, email: String }, Missing, Used, Expired, EmailMismatch,
}

async fn accept_invite(
    State(state): State<AppState>, Path(token): Path<String>, Form(form): Form<InviteAcceptForm>,
) -> Response {
    let email = form.email.trim().to_lowercase();
    let name: String = form.name.trim().chars().take(80).collect();
    if !valid_email(&email) {
        tracing::warn!(target: "amux::local_invite", verdict = "rejected", reason = "invalid_email", invite = %invite_fingerprint(&token), "local invite rejected");
        return invite_error(StatusCode::BAD_REQUEST, "Valid email required", "Enter the email address you want teammates to see.");
    }
    let member_id_candidate = ulid::Ulid::new().to_string().to_lowercase();
    let token_w = token.clone();
    let email_w = email.clone();
    let name_w = name.clone();
    let outcome: Arc<Mutex<Option<AcceptOutcome>>> = Arc::new(Mutex::new(None));
    let outcome_w = outcome.clone();
    let write = state.store.write_async(move |conn| {
        let now = chrono::Utc::now().timestamp();
        let row: Option<(Option<String>, i64, Option<i64>)> = conn.query_row(
            "SELECT email, expires_at, used_at FROM org_invites WHERE token=?1", [&token_w],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        let Some((bound_email, expires_at, used_at)) = row else {
            *outcome_w.lock().expect("accept outcome") = Some(AcceptOutcome::Missing);
            return Ok(WriteOutcome { applied: false, events: vec![] });
        };
        if used_at.is_some() {
            *outcome_w.lock().expect("accept outcome") = Some(AcceptOutcome::Used);
            return Ok(WriteOutcome { applied: false, events: vec![] });
        }
        if expires_at <= now {
            *outcome_w.lock().expect("accept outcome") = Some(AcceptOutcome::Expired);
            return Ok(WriteOutcome { applied: false, events: vec![] });
        }
        if bound_email.as_deref().is_some_and(|bound| bound != email_w) {
            *outcome_w.lock().expect("accept outcome") = Some(AcceptOutcome::EmailMismatch);
            return Ok(WriteOutcome { applied: false, events: vec![] });
        }
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM org_members WHERE email=?1", [&email_w], |row| row.get(0),
        ).optional()?;
        let (member_id, created) = match existing {
            Some(id) => {
                if !name_w.is_empty() {
                    conn.execute("UPDATE org_members SET name=?1 WHERE id=?2", rusqlite::params![name_w, id])?;
                }
                (id, false)
            }
            None => {
                let display_name = if name_w.is_empty() {
                    email_w.split('@').next().unwrap_or(&email_w).to_string()
                } else { name_w.clone() };
                conn.execute(
                    "INSERT INTO org_members (id,email,name,role,joined_at) VALUES (?1,?2,?3,'member',?4)",
                    rusqlite::params![member_id_candidate, email_w, display_name, now],
                )?;
                (member_id_candidate, true)
            }
        };
        conn.execute("UPDATE org_invites SET used_at=?1, used_by=?2 WHERE token=?3",
            rusqlite::params![now, member_id, token_w])?;
        let mut events = vec![ev("org_invite", &token_w, MutationKind::Updated)];
        events.push(ev("org_member", &member_id,
            if created { MutationKind::Created } else { MutationKind::Updated }));
        *outcome_w.lock().expect("accept outcome") = Some(AcceptOutcome::Accepted {
            member_id, email: email_w,
        });
        Ok(WriteOutcome { applied: true, events })
    }).await;
    if let Err(e) = write {
        tracing::warn!(target: "amux::local_invite", verdict = "accept_failed", error = %e, "local invite acceptance write failed");
        return internal(e);
    }
    let verdict = outcome.lock().expect("accept outcome").take();
    match verdict {
        Some(AcceptOutcome::Accepted { member_id, email }) => {
            tracing::info!(target: "amux::local_invite", verdict = "accepted", member_id = %member_id, email = %email, "local invite accepted");
            let cookie = format!("{MEMBER_COOKIE}={token}; Path=/; Max-Age=31536000; HttpOnly; Secure; SameSite=Lax");
            (StatusCode::SEE_OTHER, [(header::LOCATION, "/"), (header::SET_COOKIE, cookie.as_str())], "").into_response()
        }
        Some(AcceptOutcome::Missing) => invite_error(StatusCode::GONE, "Invite not found", "Ask the workspace owner for a new link."),
        Some(AcceptOutcome::Used) => invite_error(StatusCode::GONE, "Invite already used", "Ask the workspace owner for a new link."),
        Some(AcceptOutcome::Expired) => invite_error(StatusCode::GONE, "Invite expired", "Ask the workspace owner for a new link."),
        Some(AcceptOutcome::EmailMismatch) => {
            tracing::warn!(target: "amux::local_invite", verdict = "rejected", reason = "email_mismatch", invite = %invite_fingerprint(&token), "local invite rejected");
            invite_error(StatusCode::FORBIDDEN, "Different email required", "This invitation is tied to another email address.")
        }
        None => internal("invite acceptance completed without a verdict"),
    }
}

// ---- GET /api/org ---------------------------------------------------------

pub async fn get_org(State(state): State<AppState>) -> Response {
    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let created = ensure_org(conn)?;
            let mut org = query_rows_json(conn, "SELECT * FROM org WHERE id='default'", &[])?
                .pop()
                .unwrap_or_else(|| json!({}));
            let members: i64 =
                conn.query_row("SELECT COUNT(*) FROM org_members", [], |r| r.get(0))?;
            let now = chrono::Utc::now().timestamp();
            let invites: i64 = conn.query_row(
                "SELECT COUNT(*) FROM org_invites WHERE used_at IS NULL AND expires_at > ?1",
                [now],
                |r| r.get(0),
            )?;
            org["member_count"] = json!(members);
            org["invite_count"] = json!(invites);
            *slot_w.lock().expect("slot") = Some(org);
            let events =
                if created { vec![ev("org", "default", MutationKind::Created)] } else { vec![] };
            Ok(WriteOutcome { applied: created, events })
        })
        .await;
    match write {
        Ok(_) => {
            let org = slot.lock().expect("slot").take().unwrap_or_else(|| json!({}));
            Json(org).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- PATCH /api/org -------------------------------------------------------

pub async fn patch_org(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    // Python: body.get("name", "").strip()[:80] — char truncation.
    let name: String = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name required" }));
    }
    let name_w = name.clone();
    let write = state
        .store
        .write_async(move |conn| {
            ensure_org(conn)?;
            conn.execute("UPDATE org SET name=?1 WHERE id='default'", [&name_w])?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev("org", "default", MutationKind::Updated)],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true, "name": name })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET /api/org/members -------------------------------------------------

pub async fn list_members(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        Ok(query_rows_json(
            &conn,
            "SELECT id, email, name, role, joined_at FROM org_members ORDER BY joined_at",
            &[],
        )?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/org/members/{id} -----------------------------------------

pub async fn delete_member(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM org_members WHERE id=?1", [&id_w])?;
            let events = if n > 0 {
                vec![ev("org_member", &id_w, MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---- GET /api/org/invites -------------------------------------------------

pub async fn list_invites(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let base = base_url(&headers);
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let now = chrono::Utc::now().timestamp();
        let mut rows = query_rows_json(
            &conn,
            "SELECT token, email, created_at, expires_at, used_at, used_by \
             FROM org_invites WHERE used_at IS NULL AND expires_at > ?1 \
             ORDER BY created_at DESC",
            &[&now],
        )?;
        for r in &mut rows {
            let tok = r.get("token").and_then(Value::as_str).unwrap_or("").to_string();
            r["url"] = json!(format!("{base}/invite/{tok}"));
        }
        Ok(rows)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- POST /api/org/invites ------------------------------------------------

pub async fn create_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // Python: `body.get("email", "").strip().lower() or None`.
    let email: Option<String> = body
        .get("email")
        .and_then(Value::as_str)
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty());
    if email.as_deref().is_some_and(|value| !valid_email(value)) {
        tracing::warn!(target: "amux::local_invite", verdict = "create_rejected", reason = "invalid_email", "local invite creation rejected");
        return err(StatusCode::BAD_REQUEST, json!({ "error": "valid email required" }));
    }
    let token = token_urlsafe(24);
    let now = chrono::Utc::now().timestamp();
    let expires = now + 7 * 86400;
    let token_w = token.clone();
    let email_w = email.clone();
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at) VALUES (?1,?2,?3,?4)",
                rusqlite::params![token_w, email_w, now, expires],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev("org_invite", &token_w, MutationKind::Created)],
            })
        })
        .await;
    match write {
        Ok(_) => {
            tracing::info!(target: "amux::local_invite", verdict = "created",
                bound_email = email.as_deref().unwrap_or("open"), expires_at = expires,
                "local invite created");
            let url = format!("{}/invite/{token}", base_url(&headers));
            (
                StatusCode::CREATED,
                Json(json!({ "token": token, "url": url, "expires_at": expires })),
            )
                .into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/org/invites/{token} --------------------------------------

pub async fn delete_invite(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let tok_w = token.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM org_invites WHERE token=?1", [&tok_w])?;
            let events = if n > 0 {
                vec![ev("org_invite", &tok_w, MutationKind::Deleted)]
            } else {
                vec![]
            };
            Ok(WriteOutcome { applied: n > 0, events })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Tests — temp-DB stores; Python-shaped rows round-trip column by column.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use axum::body::Body;
    use axum::http::{header, HeaderMap, Request};
    use tower::ServiceExt;

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("org-api-test.db")).unwrap();
        let state = AppState {
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let router = Router::new().nest("/api/org", routes()).with_state(state);
        (router, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    fn full_app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("org-full-test.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: Some("owner-token".into()),
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (crate::api::router(state), dir)
    }

    async fn raw_send(
        app: &axum::Router, method: &str, path: &str, body: &str, headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in headers { request = request.header(*name, *value); }
        let response = app.clone().oneshot(request.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn get_lazily_creates_the_default_org() {
        let (app, dir) = app();
        let (st, v) = send(&app, "GET", "/api/org", None, &[]).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["id"], json!("default"));
        assert_eq!(v["name"], json!("My Workspace"));
        assert_eq!(v["member_count"], json!(0));
        assert_eq!(v["invite_count"], json!(0));
        assert!(v["created_at"].as_i64().unwrap() > 0);
        // The row is persisted, not synthesized per-request.
        let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM org WHERE id='default'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn patch_renames_with_python_validation_and_80_char_cap() {
        let (app, _dir) = app();
        let (st, e) = send(&app, "PATCH", "/api/org", Some(json!({ "name": "  " })), &[]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("name required"));

        let long = "x".repeat(100);
        let (st, r) = send(&app, "PATCH", "/api/org", Some(json!({ "name": long })), &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["name"].as_str().unwrap().len(), 80);
        let (_, v) = send(&app, "GET", "/api/org", None, &[]).await;
        assert_eq!(v["name"].as_str().unwrap().len(), 80);
    }

    #[tokio::test]
    async fn python_shaped_member_and_invite_rows_round_trip() {
        let (app, dir) = app();
        {
            // Rows exactly as the Python server writes them.
            let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
            conn.execute(
                "INSERT INTO org (id, name, created_at) VALUES ('default','Mixpeek HQ',1753000000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO org_members (id, email, name, role, joined_at) \
                 VALUES ('tok_member_0001','a@x.co',NULL,'member',1753000100)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO org_members (id, email, name, role, joined_at) \
                 VALUES ('tok_member_0002','b@x.co','Bee','admin',1753000050)",
                [],
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at) \
                 VALUES ('livetokenlivetokenlivetoken00001','c@x.co',?1,?2)",
                rusqlite::params![now, now + 86400],
            )
            .unwrap();
            // Used and expired invites must be hidden from the list.
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at, used_at, used_by) \
                 VALUES ('usedtoken0000000000000000000000x',NULL,?1,?2,?1,'d@x.co')",
                rusqlite::params![now, now + 86400],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO org_invites (token, email, created_at, expires_at) \
                 VALUES ('expiredtoken00000000000000000000',NULL,?1,?2)",
                rusqlite::params![now - 86400 * 8, now - 60],
            )
            .unwrap();
        }

        // GET /api/org counts members + only-live invites.
        let (_, org) = send(&app, "GET", "/api/org", None, &[]).await;
        assert_eq!(org["name"], json!("Mixpeek HQ"));
        assert_eq!(org["created_at"], json!(1753000000));
        assert_eq!(org["member_count"], json!(2));
        assert_eq!(org["invite_count"], json!(1));

        // Members: joined_at ASC ordering, exact Python projection.
        let (st, m) = send(&app, "GET", "/api/org/members", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        let arr = m.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], json!("tok_member_0002"));
        assert_eq!(arr[0]["name"], json!("Bee"));
        assert_eq!(arr[0]["role"], json!("admin"));
        assert_eq!(arr[0]["joined_at"], json!(1753000050));
        assert_eq!(arr[1]["id"], json!("tok_member_0001"));
        assert_eq!(arr[1]["name"], Value::Null);

        // Invites: live row only, URL built from Host + X-Forwarded-Proto.
        let (st, inv) = send(
            &app,
            "GET",
            "/api/org/invites",
            None,
            &[("Host", "cloud.amux.io"), ("X-Forwarded-Proto", "https")],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let arr = inv.as_array().unwrap();
        assert_eq!(arr.len(), 1, "{inv}");
        assert_eq!(arr[0]["token"], json!("livetokenlivetokenlivetoken00001"));
        assert_eq!(arr[0]["email"], json!("c@x.co"));
        assert_eq!(arr[0]["used_at"], Value::Null);
        assert_eq!(
            arr[0]["url"],
            json!("https://cloud.amux.io/invite/livetokenlivetokenlivetoken00001")
        );

        // Default host/scheme when the headers are absent.
        let (_, inv2) = send(&app, "GET", "/api/org/invites", None, &[]).await;
        let url = inv2[0]["url"].as_str().unwrap();
        // Derived, not literal: the fallback follows this server's own port,
        // so hardcoding one here would pin the test to a deployment.
        let want = format!("http://localhost:{}/invite/", crate::config::canonical_port());
        assert!(url.starts_with(&want), "{url} should start with {want}");
    }

    #[tokio::test]
    async fn create_invite_mints_python_shaped_token_and_expiry() {
        let (app, dir) = app();
        let before = chrono::Utc::now().timestamp();
        let (st, r) = send(
            &app,
            "POST",
            "/api/org/invites",
            Some(json!({ "email": "  NewHire@X.Co " })),
            &[("Host", "myhost:9"), ("X-Forwarded-Proto", "https")],
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{r}");
        let token = r["token"].as_str().unwrap();
        // token_urlsafe(24) shape: 32 urlsafe chars, no padding.
        assert_eq!(token.len(), 32, "{token}");
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(r["url"], json!(format!("https://myhost:9/invite/{token}")));
        let exp = r["expires_at"].as_i64().unwrap();
        assert!(exp >= before + 7 * 86400 && exp <= before + 7 * 86400 + 60, "{exp}");

        // Stored row: email lowercased; empty email stores NULL.
        let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
        let email: Option<String> = conn
            .query_row("SELECT email FROM org_invites WHERE token=?1", [token], |r| r.get(0))
            .unwrap();
        assert_eq!(email.as_deref(), Some("newhire@x.co"));
        let (st, r2) = send(&app, "POST", "/api/org/invites", Some(json!({})), &[]).await;
        assert_eq!(st, StatusCode::CREATED);
        let email2: Option<String> = conn
            .query_row(
                "SELECT email FROM org_invites WHERE token=?1",
                [r2["token"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(email2, None);
    }

    #[tokio::test]
    async fn deletes_answer_ok_and_remove_rows() {
        let (app, dir) = app();
        {
            let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
            conn.execute(
                "INSERT INTO org_members (id, email, role, joined_at) \
                 VALUES ('mem1','gone@x.co','member',1)",
                [],
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            conn.execute(
                "INSERT INTO org_invites (token, created_at, expires_at) VALUES ('tok1',?1,?2)",
                rusqlite::params![now, now + 100],
            )
            .unwrap();
        }
        let (st, r) = send(&app, "DELETE", "/api/org/members/mem1", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        let (st, r) = send(&app, "DELETE", "/api/org/invites/tok1", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        // Python answers ok for a missing row too.
        let (st, r) = send(&app, "DELETE", "/api/org/members/never-existed", None, &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["ok"], json!(true));
        let conn = rusqlite::Connection::open(dir.path().join("org-api-test.db")).unwrap();
        let m: i64 = conn.query_row("SELECT COUNT(*) FROM org_members", [], |r| r.get(0)).unwrap();
        let i: i64 = conn.query_row("SELECT COUNT(*) FROM org_invites", [], |r| r.get(0)).unwrap();
        assert_eq!((m, i), (0, 0));
    }

    #[tokio::test]
    async fn invite_acceptance_authenticates_attributes_and_revokes_a_local_member() {
        let (app, dir) = full_app();
        let (created, _, body) = raw_send(&app, "POST", "/api/org/invites",
            r#"{"email":"guest@example.com"}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json"),
              ("host", "tailnet-host:8824"), ("x-forwarded-proto", "https")]).await;
        assert_eq!(created, StatusCode::CREATED, "{body}");
        let invitation: Value = serde_json::from_str(&body).unwrap();
        let token = invitation["token"].as_str().unwrap();
        assert_eq!(invitation["url"], json!(format!("https://tailnet-host:8824/invite/{token}")));

        let (landing, _, html) = raw_send(&app, "GET", &format!("/invite/{token}"), "", &[]).await;
        assert_eq!(landing, StatusCode::OK, "{html}");
        assert!(html.contains("guest@example.com") && html.contains("Join My Workspace"), "{html}");

        let (accepted, headers, _) = raw_send(&app, "POST", &format!("/invite/{token}"),
            "email=guest%40example.com&name=Guest+User",
            &[("content-type", "application/x-www-form-urlencoded")]).await;
        assert_eq!(accepted, StatusCode::SEE_OTHER);
        assert_eq!(headers[header::LOCATION], "/");
        let set_cookie = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("Secure") && set_cookie.contains("SameSite=Lax"), "{set_cookie}");
        let cookie = set_cookie.split(';').next().unwrap();

        let (identity_status, _, identity_body) = raw_send(&app, "GET", "/api/identity", "", &[("cookie", cookie)]).await;
        assert_eq!(identity_status, StatusCode::OK, "{identity_body}");
        let identity: Value = serde_json::from_str(&identity_body).unwrap();
        assert_eq!(identity["email"], "guest@example.com");
        assert_eq!(identity["is_local_member"], true);
        assert_eq!(identity["is_cloud"], false);

        // The member shell cannot inherit the owner's bearer: real browser API
        // calls must continue to exercise the cookie boundary.
        let (shell_status, _, shell) = raw_send(&app, "GET", "/", "", &[("cookie", cookie)]).await;
        assert_eq!(shell_status, StatusCode::OK);
        assert!(shell.contains("window._AMUX_AUTH_TOKEN=\"\""), "{shell}");
        assert!(!shell.contains("window._AMUX_AUTH_TOKEN=\"owner-token\""), "{shell}");

        let (members_status, _, members_body) = raw_send(&app, "GET", "/api/org/members", "", &[("cookie", cookie)]).await;
        assert_eq!(members_status, StatusCode::OK, "{members_body}");
        assert!(members_body.contains("guest@example.com"), "{members_body}");

        let db = dir.path().join("org-full-test.db");
        let mut actor = String::new();
        for _ in 0..50 {
            actor = rusqlite::Connection::open(&db).unwrap().query_row(
                "SELECT amux_session FROM _amux_request_log WHERE path='/api/org/members' ORDER BY ts DESC LIMIT 1",
                [], |row| row.get(0)).optional().unwrap().unwrap_or_default();
            if !actor.is_empty() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(actor, "member:guest@example.com");

        let member_id: String = rusqlite::Connection::open(&db).unwrap().query_row(
            "SELECT id FROM org_members WHERE email='guest@example.com'", [], |row| row.get(0)).unwrap();
        let (deleted, _, body) = raw_send(&app, "DELETE", &format!("/api/org/members/{member_id}"), "",
            &[("authorization", "Bearer owner-token")]).await;
        assert_eq!(deleted, StatusCode::OK, "{body}");
        let (revoked, _, _) = raw_send(&app, "GET", "/api/org/members", "", &[("cookie", cookie)]).await;
        assert_eq!(revoked, StatusCode::UNAUTHORIZED);
        let (replay, _, _) = raw_send(&app, "GET", &format!("/invite/{token}"), "", &[]).await;
        assert_eq!(replay, StatusCode::GONE);
    }

    #[tokio::test]
    async fn email_bound_invite_refuses_a_different_email_without_consuming_it() {
        let (app, _dir) = full_app();
        let (_, _, body) = raw_send(&app, "POST", "/api/org/invites",
            r#"{"email":"right@example.com"}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")]).await;
        let invitation: Value = serde_json::from_str(&body).unwrap();
        let token = invitation["token"].as_str().unwrap();
        let (wrong, _, _) = raw_send(&app, "POST", &format!("/invite/{token}"),
            "email=wrong%40example.com&name=Wrong",
            &[("content-type", "application/x-www-form-urlencoded")]).await;
        assert_eq!(wrong, StatusCode::FORBIDDEN);
        let (still_live, _, _) = raw_send(&app, "GET", &format!("/invite/{token}"), "", &[]).await;
        assert_eq!(still_live, StatusCode::OK);
    }
}
