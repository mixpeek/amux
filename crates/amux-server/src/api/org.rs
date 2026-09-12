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
use crate::integrations::email::{base64url_decode, base64url_nopad};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Form, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
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
const MEMBER_SCOPE_LEVEL_HEADER: &str = "x-amux-local-member-scope-level";
const MEMBER_SCOPE_NAME_HEADER: &str = "x-amux-local-member-scope-name";
const MEMBER_ACTOR_HEADER: &str = "x-amux-local-member-actor";
const MEMBER_TEAM_ID_HEADER: &str = "x-amux-local-member-team-id";
const MEMBER_TEAM_NAME_B64_HEADER: &str = "x-amux-local-member-team-name-b64";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_org).patch(patch_org))
        .route("/members", get(list_members))
        .route(
            "/members/{id}",
            axum::routing::patch(patch_member).delete(delete_member),
        )
        .route("/teams", get(list_teams).post(create_team))
        .route(
            "/teams/{id}",
            axum::routing::patch(patch_team).delete(delete_team),
        )
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

/// Server-verified author for an invited human. The marker and value are both
/// removed from inbound requests below, so a member cannot impersonate a
/// worker (or another member) by supplying the ordinary attribution headers.
pub(crate) fn local_member_actor(headers: &HeaderMap) -> Option<&str> {
    if !is_verified_local_member(headers) {
        return None;
    }
    headers
        .get(MEMBER_ACTOR_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The resource boundary granted to a local/Tailscale member.
///
/// This is deliberately separate from `role`: every invitee remains a
/// non-admin `member`, while scope answers which workers (and therefore which
/// cards) that member can reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberScope {
    Global,
    Group(String),
    Worker(String),
}

impl MemberScope {
    pub(crate) fn level(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Group(_) => "group",
            Self::Worker(_) => "worker",
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Global => "",
            Self::Group(name) | Self::Worker(name) => name,
        }
    }

    pub(crate) fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }

    pub(crate) fn allows_worker(&self, worker: &str) -> bool {
        match self {
            Self::Global => true,
            Self::Worker(name) => name.eq_ignore_ascii_case(worker),
            Self::Group(group) => super::session_verbs::lane_groups(worker).contains(group),
        }
    }
}

fn parse_member_scope(level: &str, name: &str) -> Option<MemberScope> {
    match level.trim().to_ascii_lowercase().as_str() {
        "global" if name.trim().is_empty() => Some(MemberScope::Global),
        "group" if !name.trim().is_empty() => {
            Some(MemberScope::Group(name.trim().to_ascii_lowercase()))
        }
        "worker" if !name.trim().is_empty() => Some(MemberScope::Worker(name.trim().to_string())),
        _ => None,
    }
}

/// Read only the headers installed by [`local_member_identity`]. An inbound
/// copy is removed before cookie verification, so handlers can safely use this
/// helper for filtering as well as admission.
pub(crate) fn local_member_scope(headers: &HeaderMap) -> Option<MemberScope> {
    if !is_verified_local_member(headers) {
        return None;
    }
    let level = headers
        .get(MEMBER_SCOPE_LEVEL_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("global");
    let name = headers
        .get(MEMBER_SCOPE_NAME_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // A corrupt/unknown stored value must never widen to global. New writes
    // are validated, but fail closed if an operator edited the DB by hand.
    parse_member_scope(level, name)
        .or_else(|| Some(MemberScope::Worker("__invalid_member_scope__".into())))
}

/// Team assignment resolved from server-installed identity headers.  Scope is
/// enforced independently above, while this metadata lets the UI explain why
/// a member has that access without exposing the org administration APIs.
pub(crate) fn local_member_team(headers: &HeaderMap) -> Option<Value> {
    if !is_verified_local_member(headers) {
        return None;
    }
    let id = headers.get(MEMBER_TEAM_ID_HEADER)?.to_str().ok()?;
    let encoded = headers.get(MEMBER_TEAM_NAME_B64_HEADER)?.to_str().ok()?;
    let name = String::from_utf8(base64url_decode(encoded).ok()?).ok()?;
    (!id.is_empty()).then(|| json!({"id": id, "name": name}))
}

pub(crate) fn scoped_worker_names(scope: &MemberScope) -> Vec<String> {
    match scope {
        MemberScope::Global => Vec::new(),
        MemberScope::Worker(worker) => vec![worker.clone()],
        MemberScope::Group(group) => {
            let blocked = super::groups::blocked_names(&super::groups::amux_home());
            let mut workers: Vec<String> = std::fs::read_dir(
                super::groups::amux_home().join("sessions"),
            )
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|ext| ext.to_str()) == Some("env"))
                    .then(|| path.file_stem()?.to_str().map(str::to_string))
                    .flatten()
            })
            .filter(|worker| {
                !blocked.contains(worker) && super::session_verbs::lane_groups(worker).contains(group)
            })
            .collect();
            workers.sort();
            workers.dedup();
            workers
        }
    }
}

fn forbidden(scope: &MemberScope, resource: &str) -> Response {
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

fn resolved_worker_name(conn: &rusqlite::Connection, target: &str) -> Option<String> {
    conn.query_row(
        "SELECT display_name FROM _amux_workers WHERE id=?1 OR display_name=?1 LIMIT 1",
        [target],
        |row| row.get(0),
    )
    .optional()
    .ok()
    .flatten()
    .or_else(|| {
        super::groups::amux_home()
            .join("sessions")
            .join(format!("{target}.env"))
            .is_file()
            .then(|| target.to_string())
    })
}

/// Route-level guard for scoped invitees. Handlers that return collections or
/// accept a new worker assignment also intersect/validate inside the handler;
/// this guard closes every direct-by-id path before it can read or mutate.
pub(crate) fn authorize_local_member_request(
    state: &AppState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Option<Response> {
    let scope = local_member_scope(headers)?;
    let path = uri.path();

    // Membership and invite administration is an owner capability, not a
    // consequence of seeing the whole workspace.
    if path == "/api/org" || path.starts_with("/api/org/") {
        return Some(forbidden(&scope, "organization administration"));
    }
    if scope.is_global() {
        return None;
    }

    if path == "/api/identity" || path == "/api/events" {
        return None;
    }
    if path == "/api/sessions" {
        return (*method != Method::GET).then(|| forbidden(&scope, "fleet mutation"));
    }
    if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let worker = rest.split('/').next().unwrap_or("");
        return (!scope.allows_worker(worker)).then(|| forbidden(&scope, worker));
    }
    if path == "/api/workers" || path == "/api/workers/" {
        return Some(forbidden(&scope, "worker registry"));
    }
    if let Some(rest) = path.strip_prefix("/api/workers/") {
        let target = rest.split('/').next().unwrap_or("");
        let allowed = state
            .store
            .read()
            .ok()
            .and_then(|conn| resolved_worker_name(&conn, target))
            .is_some_and(|worker| scope.allows_worker(&worker));
        return (!allowed).then(|| forbidden(&scope, target));
    }
    if path == "/api/board" || path == "/api/board/" {
        // GET is filtered below; POST validates the requested session in the
        // handler before it writes.
        return (!matches!(*method, Method::GET | Method::POST))
            .then(|| forbidden(&scope, "board"));
    }
    if matches!(
        path,
        "/api/board/statuses"
            | "/api/board/session-gates"
            | "/api/board/contract"
            | "/api/board/themes"
    ) && *method == Method::GET
    {
        return None;
    }
    if (path == "/api/board/statuses" || path == "/api/board/session-gates")
        && *method != Method::GET
    {
        return Some(forbidden(&scope, path));
    }
    if path == "/api/board/export" && *method == Method::GET {
        return None;
    }
    if matches!(
        path,
        "/api/board/needsyou"
            | "/api/board/ready"
            | "/api/board/bulk-migrate"
            | "/api/board/clear-done"
    ) || path.starts_with("/api/board/statuses/")
        || path.starts_with("/api/board/session-gates/")
        || path == "/api/board/commit-mentions"
        || path == "/api/board/deleted-substrate"
    {
        return Some(forbidden(&scope, path));
    }
    if let Some(rest) = path.strip_prefix("/api/board/") {
        let id = rest.split('/').next().unwrap_or("");
        let session: Option<Option<String>> = state.store.read().ok().and_then(|conn| {
            conn.query_row(
                "SELECT session FROM issues WHERE id=?1 AND deleted IS NULL",
                [id],
                |row| row.get(0),
            )
            .optional()
            .ok()
        });
        return match session {
            Some(Some(worker)) if scope.allows_worker(&worker) => None,
            Some(_) => Some(forbidden(&scope, id)),
            // Preserve the handler's own 404 for an unknown id.
            None => None,
        };
    }
    if matches!(path, "/api/groups" | "/api/groups/" | "/api/tags" | "/api/tags/")
        && *method == Method::GET
    {
        return None;
    }

    Some(forbidden(&scope, path))
}

#[derive(Debug)]
struct MemberIdentity {
    id: String,
    email: String,
    scope: MemberScope,
    team_id: String,
    team_name: String,
}

fn member_cookie(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|cookies| {
        cookies.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == MEMBER_COOKIE && !value.is_empty()).then_some(value)
        })
    })
}

/// A cookie with this name remains significant even after its backing member
/// is deleted. Static shell serving uses the distinction between "no member
/// session" and "a revoked member session" so a reload cannot silently turn
/// a revoked invitee into the owner.
pub(crate) fn has_local_member_cookie(headers: &HeaderMap) -> bool {
    member_cookie(headers).is_some()
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
    req.headers_mut().remove(MEMBER_SCOPE_LEVEL_HEADER);
    req.headers_mut().remove(MEMBER_SCOPE_NAME_HEADER);
    req.headers_mut().remove(MEMBER_ACTOR_HEADER);
    req.headers_mut().remove(MEMBER_TEAM_ID_HEADER);
    req.headers_mut().remove(MEMBER_TEAM_NAME_B64_HEADER);
    req.headers_mut().remove("x-amux-user-id");
    req.headers_mut().remove("x-amux-user-email");
    let Some(token) = member_cookie(req.headers()).map(str::to_string) else {
        return next.run(req).await;
    };
    let store = state.store.clone();
    let identity = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<MemberIdentity>> {
        let conn = store.read()?;
        Ok(conn.query_row(
            "SELECT m.id, m.email, \
                    CASE WHEN t.id IS NOT NULL THEN t.scope_level \
                         WHEN COALESCE(m.team_id,'')='' THEN m.scope_level ELSE 'worker' END, \
                    CASE WHEN t.id IS NOT NULL THEN t.scope_name \
                         WHEN COALESCE(m.team_id,'')='' THEN m.scope_name ELSE '__invalid_team_scope__' END, \
                    COALESCE(t.id,''), COALESCE(t.name,'') \
             FROM org_invites i JOIN org_members m ON m.id=i.used_by \
             LEFT JOIN org_teams t ON t.id=m.team_id \
             WHERE i.token=?1 AND i.used_at IS NOT NULL",
            [&token],
            |row| {
                let level: String = row.get(2)?;
                let name: String = row.get(3)?;
                Ok(MemberIdentity {
                    id: row.get(0)?,
                    email: row.get(1)?,
                    // Corrupt rows fail closed. Defaulting here to Global
                    // would turn a typo or partial manual migration into the
                    // broadest possible grant.
                    scope: parse_member_scope(&level, &name)
                        .unwrap_or(MemberScope::Worker("__invalid_member_scope__".into())),
                    team_id: row.get(4)?,
                    team_name: row.get(5)?,
                })
            },
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
    req.headers_mut().insert(
        MEMBER_SCOPE_LEVEL_HEADER,
        HeaderValue::from_static(member.scope.level()),
    );
    if !member.scope.name().is_empty() {
        let Ok(scope_name) = HeaderValue::from_str(member.scope.name()) else {
            tracing::warn!(target: "amux::local_invite", verdict = "member_header_rejected", field = "scope_name", member_id = %member.id, "stored local member scope is not a valid HTTP header");
            return next.run(req).await;
        };
        req.headers_mut().insert(MEMBER_SCOPE_NAME_HEADER, scope_name);
    }
    req.headers_mut().insert("x-amux-user-id", id);
    req.headers_mut().insert("x-amux-user-email", email);
    if !member.team_id.is_empty() {
        let (Ok(team_id), Ok(team_name)) = (
            HeaderValue::from_str(&member.team_id),
            HeaderValue::from_str(&base64url_nopad(member.team_name.as_bytes())),
        ) else {
            tracing::warn!(target: "amux::local_invite", verdict = "member_header_rejected", field = "team", member_id = %member.id, "stored local member team is not a valid HTTP header");
            return next.run(req).await;
        };
        req.headers_mut().insert(MEMBER_TEAM_ID_HEADER, team_id);
        req.headers_mut().insert(MEMBER_TEAM_NAME_B64_HEADER, team_name);
    }
    if let Ok(actor) = HeaderValue::from_str(&format!("member:{}", member.email)) {
        req.headers_mut().insert(MEMBER_ACTOR_HEADER, actor);
    }
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

/// Public origin for a link another browser must be able to open.
///
/// HTTP/2 carries the authority and scheme as pseudo-headers; axum exposes
/// them on the URI, not in `HeaderMap`. Browsers negotiate h2 on the Tailscale
/// TLS listener, so reading only `Host` used to mint
/// `http://localhost:8824/invite/...` from the real tailnet dashboard.
/// HTTP/1.1 still supplies `Host`, while a reverse proxy remains authoritative
/// through `X-Forwarded-Proto`. With no protocol signal, HTTPS is the honest
/// default because the production Rust listener is TLS-only.
fn base_url(headers: &HeaderMap, uri: &Uri) -> String {
    let fallback = format!("localhost:{}", crate::config::canonical_port());
    let authority = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()));
    let host = match authority {
        Some(authority) => authority,
        None => {
            tracing::warn!(
                target: "amux::local_invite",
                verdict = "origin_fallback",
                fallback = %fallback,
                "invite request carried no HTTP authority; using the canonical localhost origin"
            );
            fallback
        }
    };
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase);
    let scheme = match forwarded.as_deref().or_else(|| uri.scheme_str()) {
        Some("http") => "http",
        _ => "https",
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

fn requested_scope(body: &Value) -> Result<MemberScope, Value> {
    let level = body
        .get("scope_level")
        .and_then(Value::as_str)
        .unwrap_or("global");
    let name = body.get("scope_name").and_then(Value::as_str).unwrap_or("");
    let Some(scope) = parse_member_scope(level, name) else {
        return Err(json!({
                "error": "invalid member scope",
                "scope_level": level,
                "scope_name": name,
                "valid_levels": ["global", "group", "worker"],
                "why": "group and worker scopes require a target; global must not carry one",
            }));
    };
    if matches!(scope, MemberScope::Worker(ref worker) if !super::session_verbs::valid_session_name(worker)) {
        return Err(json!({"error": "worker scope target is not a valid worker name"}));
    }
    if HeaderValue::from_str(scope.name()).is_err() || scope.name().len() > 128 {
        return Err(json!({"error": "scope target must be a short HTTP-safe worker or group name"}));
    }
    Ok(scope)
}

/// Resolve a requested scope to the canonical value stored on the member.
/// Worker APIs accept either the registry id or display name, but every other
/// access check operates on the display/session name; canonicalizing here
/// keeps an id-shaped invite from becoming a valid grant that can see nothing.
fn resolve_scope_target(
    conn: &rusqlite::Connection,
    scope: &MemberScope,
) -> Option<MemberScope> {
    match scope {
        MemberScope::Global => Some(MemberScope::Global),
        MemberScope::Worker(worker) => {
            resolved_worker_name(conn, worker).map(MemberScope::Worker)
        }
        MemberScope::Group(group) => {
            let configured = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM group_config WHERE lower(name)=lower(?1))",
                    [group],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                != 0;
            (configured
                || std::fs::read_dir(super::groups::amux_home().join("sessions"))
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        entry.path().file_stem().and_then(|s| s.to_str()).map(str::to_string)
                    })
                    .any(|worker| super::session_verbs::lane_groups(&worker).contains(group)))
            .then(|| MemberScope::Group(group.clone()))
        }
    }
}

#[derive(Debug, Clone)]
struct TeamAssignment {
    id: String,
    name: String,
    scope: MemberScope,
    exists: bool,
}

fn team_by_id(
    conn: &rusqlite::Connection,
    id: &str,
) -> rusqlite::Result<Option<TeamAssignment>> {
    conn.query_row(
        "SELECT id,name,scope_level,scope_name FROM org_teams WHERE id=?1",
        [id],
        |row| {
            let level: String = row.get(2)?;
            let name: String = row.get(3)?;
            Ok(TeamAssignment {
                id: row.get(0)?,
                name: row.get(1)?,
                scope: parse_member_scope(&level, &name)
                    .unwrap_or(MemberScope::Worker("__invalid_team_scope__".into())),
                exists: true,
            })
        },
    )
    .optional()
}

fn assignment_from_body(
    conn: &rusqlite::Connection,
    body: &Value,
) -> Result<TeamAssignment, Value> {
    if let Some(id) = body.get("team_id").and_then(Value::as_str).map(str::trim) {
        if id.is_empty() {
            return Err(json!({"error":"team_id must not be empty"}));
        }
        return team_by_id(conn, id)
            .map_err(|e| json!({"error":e.to_string()}))?
            .ok_or_else(|| json!({"error":"team not found","team_id":id}));
    }

    // Compatibility for pre-team API callers: resolve their direct scope and
    // bind it to a real team.  This keeps every accepted user team-backed
    // without making a rolling server/dashboard deployment drop old clients.
    let requested = requested_scope(body)?;
    let Some(scope) = resolve_scope_target(conn, &requested) else {
        return Err(json!({
            "error":"scope target does not exist",
            "scope_level":requested.level(),
            "scope_name":requested.name(),
        }));
    };
    let existing = conn
        .query_row(
            "SELECT id,name FROM org_teams WHERE scope_level=?1 AND scope_name=?2 \
             ORDER BY CASE WHEN id='team_global' THEN 0 ELSE 1 END,created_at,id LIMIT 1",
            rusqlite::params![scope.level(), scope.name()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|e| json!({"error":e.to_string()}))?;
    if let Some((id, name)) = existing {
        return Ok(TeamAssignment { id, name, scope, exists: true });
    }
    let suffix = ulid::Ulid::new().to_string().to_lowercase();
    let label = match &scope {
        MemberScope::Global => "Everyone".to_string(),
        MemberScope::Group(name) => format!("Group: {name} ({})", &suffix[..6]),
        MemberScope::Worker(name) => format!("Worker: {name} ({})", &suffix[..6]),
    };
    Ok(TeamAssignment {
        id: format!("team_{suffix}"),
        name: label,
        scope,
        exists: false,
    })
}

fn ensure_assignment(
    conn: &rusqlite::Connection,
    team: &TeamAssignment,
) -> rusqlite::Result<()> {
    if !team.exists {
        conn.execute(
            "INSERT INTO org_teams (id,name,scope_level,scope_name,created_at) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                team.id,
                team.name,
                team.scope.level(),
                team.scope.name(),
                chrono::Utc::now().timestamp(),
            ],
        )?;
    }
    Ok(())
}

fn invite_fingerprint(token: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))[..12].to_string()
}

#[derive(Debug)]
struct InviteView {
    workspace: String,
    email: Option<String>,
    scope: MemberScope,
    team_name: String,
}

type InviteRow = (Option<String>, i64, Option<i64>, String, String, String, String);

#[derive(Debug)]
enum InviteLookup { Live(InviteView), Missing, Used, Expired }

fn lookup_invite(conn: &rusqlite::Connection, token: &str) -> rusqlite::Result<InviteLookup> {
    let row: Option<InviteRow> = conn.query_row(
        "SELECT i.email,i.expires_at,i.used_at, \
                CASE WHEN t.id IS NOT NULL THEN t.scope_level \
                     WHEN COALESCE(i.team_id,'')='' THEN i.scope_level ELSE 'worker' END, \
                CASE WHEN t.id IS NOT NULL THEN t.scope_name \
                     WHEN COALESCE(i.team_id,'')='' THEN i.scope_name ELSE '__invalid_team_scope__' END, \
                COALESCE(t.id,''),COALESCE(t.name,'') \
         FROM org_invites i LEFT JOIN org_teams t ON t.id=i.team_id WHERE i.token=?1", [token],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
    ).optional()?;
    let Some((email, expires_at, used_at, scope_level, scope_name, _team_id, team_name)) = row else { return Ok(InviteLookup::Missing) };
    if used_at.is_some() { return Ok(InviteLookup::Used); }
    if expires_at <= chrono::Utc::now().timestamp() { return Ok(InviteLookup::Expired); }
    let workspace = conn.query_row("SELECT name FROM org WHERE id='default'", [], |row| row.get(0))
        .optional()?.unwrap_or_else(|| "My Workspace".to_string());
    let scope = parse_member_scope(&scope_level, &scope_name)
        .unwrap_or(MemberScope::Worker("__invalid_member_scope__".into()));
    Ok(InviteLookup::Live(InviteView { workspace, email, scope, team_name }))
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
            let access = if invite.scope.is_global() {
                "the whole workspace".to_string()
            } else {
                format!("{} {}", invite.scope.level(), invite.scope.name())
            };
            let body = format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Join {workspace}</title><style>{css}</style></head><body><main><div class=\"mark\">A</div><h1>Join {workspace}</h1><p>You were invited to the <strong>{team}</strong> team with access to <strong>{access}</strong>.</p><form method=\"post\"><label for=\"email\">Email</label><input id=\"email\" name=\"email\" type=\"email\" maxlength=\"254\" required autocomplete=\"email\" value=\"{email}\"{readonly}><label for=\"name\">Name</label><input id=\"name\" name=\"name\" maxlength=\"80\" autocomplete=\"name\" placeholder=\"How teammates will see you\"><button type=\"submit\">Join workspace</button></form><div class=\"note\">This signs this browser into this local Amux instance. Access follows the team’s scope.</div></main></body></html>",
                workspace = html_escape(&invite.workspace), css = INVITE_CSS, email = html_escape(email), access = html_escape(&access), team = html_escape(if invite.team_name.is_empty() { "Legacy access" } else { &invite.team_name }),
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
        let row: Option<InviteRow> = conn.query_row(
            "SELECT i.email,i.expires_at,i.used_at, \
                    CASE WHEN t.id IS NOT NULL THEN t.scope_level \
                         WHEN COALESCE(i.team_id,'')='' THEN i.scope_level ELSE 'worker' END, \
                    CASE WHEN t.id IS NOT NULL THEN t.scope_name \
                         WHEN COALESCE(i.team_id,'')='' THEN i.scope_name ELSE '__invalid_team_scope__' END, \
                    COALESCE(t.id,''),COALESCE(t.name,'') \
             FROM org_invites i LEFT JOIN org_teams t ON t.id=i.team_id WHERE i.token=?1", [&token_w],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        ).optional()?;
        let Some((bound_email, expires_at, used_at, scope_level, scope_name, team_id, _team_name)) = row else {
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
        let scope = parse_member_scope(&scope_level, &scope_name)
            .unwrap_or(MemberScope::Worker("__invalid_member_scope__".into()));
        let existing: Option<String> = conn.query_row(
            "SELECT id FROM org_members WHERE email=?1", [&email_w], |row| row.get(0),
        ).optional()?;
        let (member_id, created) = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE org_members SET name=CASE WHEN ?1='' THEN name ELSE ?1 END, \
                     scope_level=?2, scope_name=?3, team_id=?4 WHERE id=?5",
                    rusqlite::params![name_w, scope.level(), scope.name(), team_id, id],
                )?;
                (id, false)
            }
            None => {
                let display_name = if name_w.is_empty() {
                    email_w.split('@').next().unwrap_or(&email_w).to_string()
                } else { name_w.clone() };
                conn.execute(
                    "INSERT INTO org_members (id,email,name,role,joined_at,scope_level,scope_name,team_id) \
                     VALUES (?1,?2,?3,'member',?4,?5,?6,?7)",
                    rusqlite::params![member_id_candidate, email_w, display_name, now, scope.level(), scope.name(), team_id],
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
            "SELECT m.id,m.email,m.name,m.role,m.joined_at, \
                    COALESCE(t.scope_level,m.scope_level) AS scope_level, \
                    COALESCE(t.scope_name,m.scope_name) AS scope_name, \
                    COALESCE(t.id,'') AS team_id, COALESCE(t.name,'Legacy access') AS team_name \
             FROM org_members m LEFT JOIN org_teams t ON t.id=m.team_id ORDER BY m.joined_at",
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

// ---- PATCH /api/org/members/{id} -----------------------------------------

pub async fn patch_member(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(e) => return internal(e),
    };
    let team = match assignment_from_body(&conn, &body) {
        Ok(team) => team,
        Err(body) => return err(StatusCode::BAD_REQUEST, body),
    };
    drop(conn);
    let id_w = id.clone();
    let team_w = team.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM org_members WHERE id=?1)",
                [&id_w],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            ensure_assignment(conn, &team_w)?;
            let n = conn.execute(
                "UPDATE org_members SET team_id=?1,scope_level=?2,scope_name=?3 WHERE id=?4",
                rusqlite::params![team_w.id, team_w.scope.level(), team_w.scope.name(), id_w],
            )?;
            Ok(WriteOutcome {
                applied: n > 0,
                events: (n > 0)
                    .then(|| ev("org_member", &id_w, MutationKind::Updated))
                    .into_iter()
                    .collect(),
            })
        })
        .await;
    match write {
        Ok(outcome) if outcome.applied => Json(json!({
            "ok": true,
            "id": id,
            "team_id": team.id,
            "team_name": team.name,
            "scope_level": team.scope.level(),
            "scope_name": team.scope.name(),
        }))
        .into_response(),
        Ok(_) => err(StatusCode::NOT_FOUND, json!({"error": "member not found"})),
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

// ---- /api/org/teams ------------------------------------------------------

pub async fn list_teams(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        Ok(query_rows_json(
            &conn,
            "SELECT t.id,t.name,t.scope_level,t.scope_name,t.created_at, \
                    COUNT(DISTINCT m.id) AS member_count, \
                    COUNT(DISTINCT CASE WHEN i.used_at IS NULL AND i.expires_at>?1 THEN i.token END) AS pending_invite_count \
             FROM org_teams t \
             LEFT JOIN org_members m ON m.team_id=t.id \
             LEFT JOIN org_invites i ON i.team_id=t.id \
             GROUP BY t.id \
             ORDER BY CASE WHEN t.id='team_global' THEN 0 ELSE 1 END,lower(t.name)",
            &[&chrono::Utc::now().timestamp()],
        )?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

fn team_definition(
    conn: &rusqlite::Connection,
    body: &Value,
) -> Result<(String, MemberScope), Value> {
    let name: String = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect();
    if name.is_empty() {
        return Err(json!({"error":"team name required"}));
    }
    let requested = requested_scope(body)?;
    let Some(scope) = resolve_scope_target(conn, &requested) else {
        return Err(json!({
            "error":"scope target does not exist",
            "scope_level":requested.level(),
            "scope_name":requested.name(),
        }));
    };
    Ok((name, scope))
}

pub async fn create_team(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(e) => return internal(e),
    };
    let (name, scope) = match team_definition(&conn, &body) {
        Ok(value) => value,
        Err(body) => return err(StatusCode::BAD_REQUEST, body),
    };
    drop(conn);
    let id = format!("team_{}", ulid::Ulid::new().to_string().to_lowercase());
    let id_w = id.clone();
    let name_w = name.clone();
    let level = scope.level().to_string();
    let target = scope.name().to_string();
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO org_teams (id,name,scope_level,scope_name,created_at) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![id_w,name_w,level,target,chrono::Utc::now().timestamp()],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![ev("org_team", &id_w, MutationKind::Created)],
            })
        })
        .await;
    match write {
        Ok(_) => {
            tracing::info!(target:"amux::local_invite", verdict="team_created", team_id=%id,
                scope_level=scope.level(), scope_name=scope.name(), "workspace team created");
            (StatusCode::CREATED, Json(json!({
                "id":id,"name":name,"scope_level":scope.level(),"scope_name":scope.name(),
                "member_count":0,"pending_invite_count":0,
            }))).into_response()
        }
        Err(e) if e.to_string().contains("UNIQUE constraint failed: org_teams.name") => {
            err(StatusCode::CONFLICT, json!({"error":"team name already exists"}))
        }
        Err(e) => internal(e),
    }
}

pub async fn patch_team(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if id == "team_global" {
        return err(StatusCode::CONFLICT, json!({"error":"the Everyone team must remain global"}));
    }
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(e) => return internal(e),
    };
    let (name, scope) = match team_definition(&conn, &body) {
        Ok(value) => value,
        Err(body) => return err(StatusCode::BAD_REQUEST, body),
    };
    drop(conn);
    let id_w = id.clone();
    let name_w = name.clone();
    let level = scope.level().to_string();
    let target = scope.name().to_string();
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute(
                "UPDATE org_teams SET name=?1,scope_level=?2,scope_name=?3 WHERE id=?4",
                rusqlite::params![name_w,level,target,id_w],
            )?;
            if n > 0 {
                // Keep the 0059 columns coherent for rolling binaries and
                // forensic exports; current authorization reads the team.
                conn.execute(
                    "UPDATE org_members SET scope_level=?1,scope_name=?2 WHERE team_id=?3",
                    rusqlite::params![level,target,id_w],
                )?;
                conn.execute(
                    "UPDATE org_invites SET scope_level=?1,scope_name=?2 WHERE team_id=?3",
                    rusqlite::params![level,target,id_w],
                )?;
            }
            Ok(WriteOutcome {
                applied: n > 0,
                events: (n > 0).then(|| ev("org_team", &id_w, MutationKind::Updated)).into_iter().collect(),
            })
        })
        .await;
    match write {
        Ok(outcome) if outcome.applied => Json(json!({
            "ok":true,"id":id,"name":name,"scope_level":scope.level(),"scope_name":scope.name(),
        })).into_response(),
        Ok(_) => err(StatusCode::NOT_FOUND, json!({"error":"team not found"})),
        Err(e) if e.to_string().contains("UNIQUE constraint failed: org_teams.name") => {
            err(StatusCode::CONFLICT, json!({"error":"team name already exists"}))
        }
        Err(e) => internal(e),
    }
}

pub async fn delete_team(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if id == "team_global" {
        return err(StatusCode::CONFLICT, json!({"error":"the Everyone team cannot be deleted"}));
    }
    let id_w = id.clone();
    let verdict: Arc<Mutex<Option<&'static str>>> = Arc::new(Mutex::new(None));
    let verdict_w = verdict.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let references: i64 = conn.query_row(
                "SELECT (SELECT COUNT(*) FROM org_members WHERE team_id=?1) + \
                        (SELECT COUNT(*) FROM org_invites WHERE team_id=?1 AND used_at IS NULL AND expires_at>?2)",
                rusqlite::params![id_w, chrono::Utc::now().timestamp()],
                |row| row.get(0),
            )?;
            if references > 0 {
                *verdict_w.lock().expect("team delete verdict") = Some("in_use");
                return Ok(WriteOutcome { applied: false, events: vec![] });
            }
            let n = conn.execute("DELETE FROM org_teams WHERE id=?1", [&id_w])?;
            *verdict_w.lock().expect("team delete verdict") = Some(if n > 0 { "deleted" } else { "missing" });
            Ok(WriteOutcome {
                applied: n > 0,
                events: (n > 0).then(|| ev("org_team", &id_w, MutationKind::Deleted)).into_iter().collect(),
            })
        })
        .await;
    if let Err(e) = write {
        return internal(e);
    }
    let result = match *verdict.lock().expect("team delete verdict") {
        Some("deleted") => Json(json!({"ok":true})).into_response(),
        Some("in_use") => err(StatusCode::CONFLICT, json!({
            "error":"move members and revoke pending invites before deleting this team"
        })),
        _ => err(StatusCode::NOT_FOUND, json!({"error":"team not found"})),
    };
    result
}

// ---- GET /api/org/invites -------------------------------------------------

pub async fn list_invites(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let base = base_url(&headers, &uri);
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let now = chrono::Utc::now().timestamp();
        let mut rows = query_rows_json(
            &conn,
            "SELECT i.token,i.email,i.created_at,i.expires_at,i.used_at,i.used_by, \
                    COALESCE(t.scope_level,i.scope_level) AS scope_level, \
                    COALESCE(t.scope_name,i.scope_name) AS scope_name, \
                    COALESCE(t.id,'') AS team_id,COALESCE(t.name,'Legacy access') AS team_name \
             FROM org_invites i LEFT JOIN org_teams t ON t.id=i.team_id \
             WHERE i.used_at IS NULL AND i.expires_at > ?1 ORDER BY i.created_at DESC",
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
    uri: Uri,
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
    let conn = match state.store.read() {
        Ok(conn) => conn,
        Err(e) => return internal(e),
    };
    let team = match assignment_from_body(&conn, &body) {
        Ok(team) => team,
        Err(body) => return err(StatusCode::BAD_REQUEST, body),
    };
    drop(conn);
    let token = token_urlsafe(24);
    let now = chrono::Utc::now().timestamp();
    let expires = now + 7 * 86400;
    let token_w = token.clone();
    let email_w = email.clone();
    let team_w = team.clone();
    let write = state
        .store
        .write_async(move |conn| {
            ensure_assignment(conn, &team_w)?;
            conn.execute(
                "INSERT INTO org_invites \
                 (token,email,created_at,expires_at,scope_level,scope_name,team_id) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![token_w,email_w,now,expires,team_w.scope.level(),team_w.scope.name(),team_w.id],
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
                team_id=%team.id, scope_level = team.scope.level(), scope_name = team.scope.name(),
                "local invite created");
            let url = format!("{}/invite/{token}", base_url(&headers, &uri));
            (
                StatusCode::CREATED,
                Json(json!({
                    "token": token,
                    "url": url,
                    "expires_at": expires,
                    "team_id":team.id,
                    "team_name":team.name,
                    "scope_level": team.scope.level(),
                    "scope_name": team.scope.name(),
                })),
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

        // A real browser reaches the TLS listener over HTTP/2. In that
        // protocol there is no Host header: :authority and :scheme are
        // surfaced on the request URI. This is the actual Tailscale shape,
        // and the invite must remain usable from another node instead of
        // silently falling back to http://localhost.
        let (st, h2) = send(
            &app,
            "GET",
            "https://desktop.tail5ce8f5.ts.net:8824/api/org/invites",
            None,
            &[],
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{h2}");
        assert_eq!(
            h2[0]["url"],
            json!("https://desktop.tail5ce8f5.ts.net:8824/invite/livetokenlivetokenlivetoken00001")
        );

        // Default host/scheme when the headers are absent.
        let (_, inv2) = send(&app, "GET", "/api/org/invites", None, &[]).await;
        let url = inv2[0]["url"].as_str().unwrap();
        // Derived, not literal: the fallback follows this server's own port,
        // so hardcoding one here would pin the test to a deployment.
        let want = format!("https://localhost:{}/invite/", crate::config::canonical_port());
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
        assert_eq!(identity["access_scope"], json!({"level": "global", "name": ""}));

        // Authorship is derived from the verified invite cookie, not from a
        // caller-controlled creator field or ordinary worker/session header.
        // The same author must survive into both the card and its edit history.
        let (card_status, _, card_body) = raw_send(
            &app,
            "POST",
            "/api/board",
            r#"{"title":"member-authored","type":"chore","status":"backlog","creator":"spoofed-owner"}"#,
            &[
                ("cookie", cookie),
                ("content-type", "application/json"),
                ("x-amux-worker", "spoofed-worker"),
                ("x-amux-user-email", "spoofed@example.com"),
            ],
        )
        .await;
        assert_eq!(card_status, StatusCode::CREATED, "{card_body}");
        let card: Value = serde_json::from_str(&card_body).unwrap();
        assert_eq!(card["creator"], "member:guest@example.com");
        let card_id = card["id"].as_str().unwrap();
        let (patch_status, _, patch_body) = raw_send(
            &app,
            "PATCH",
            &format!("/api/board/{card_id}"),
            r#"{"desc_append":"member note"}"#,
            &[
                ("cookie", cookie),
                ("content-type", "application/json"),
                ("x-amux-session", "another-spoofed-worker"),
            ],
        )
        .await;
        assert_eq!(patch_status, StatusCode::OK, "{patch_body}");
        let patched: Value = serde_json::from_str(&patch_body).unwrap();
        assert!(
            patched["log"]
                .as_str()
                .is_some_and(|log| log.contains("member:guest@example.com: desc +11 chars")),
            "{patch_body}"
        );

        // The member shell cannot inherit the owner's bearer: real browser API
        // calls must continue to exercise the cookie boundary.
        let (shell_status, _, shell) = raw_send(&app, "GET", "/", "", &[("cookie", cookie)]).await;
        assert_eq!(shell_status, StatusCode::OK);
        assert!(shell.contains("window._AMUX_AUTH_TOKEN=\"\""), "{shell}");
        assert!(!shell.contains("window._AMUX_AUTH_TOKEN=\"owner-token\""), "{shell}");

        let (members_status, _, members_body) = raw_send(&app, "GET", "/api/org/members", "", &[("cookie", cookie)]).await;
        assert_eq!(members_status, StatusCode::FORBIDDEN, "{members_body}");
        assert!(members_body.contains("organization administration"), "{members_body}");

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
        let mut board_actor = String::new();
        for _ in 0..50 {
            board_actor = rusqlite::Connection::open(&db)
                .unwrap()
                .query_row(
                    "SELECT amux_session FROM _amux_request_log \
                     WHERE path='/api/board' AND method='POST' ORDER BY ts DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .unwrap()
                .unwrap_or_default();
            if !board_actor.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(board_actor, "member:guest@example.com");

        let member_id: String = rusqlite::Connection::open(&db).unwrap().query_row(
            "SELECT id FROM org_members WHERE email='guest@example.com'", [], |row| row.get(0)).unwrap();
        let (deleted, _, body) = raw_send(&app, "DELETE", &format!("/api/org/members/{member_id}"), "",
            &[("authorization", "Bearer owner-token")]).await;
        assert_eq!(deleted, StatusCode::OK, "{body}");
        let (revoked, _, _) = raw_send(&app, "GET", "/api/org/members", "", &[("cookie", cookie)]).await;
        assert_eq!(revoked, StatusCode::UNAUTHORIZED);
        // Revocation must survive a page reload. The stale HttpOnly cookie is
        // still stored by the browser after its member row is deleted; the
        // public shell must not treat the now-unverified request as an owner
        // and bootstrap the owner's bearer into JavaScript.
        let (shell_status, _, revoked_shell) =
            raw_send(&app, "GET", "/", "", &[("cookie", cookie)]).await;
        assert_eq!(shell_status, StatusCode::OK);
        assert!(revoked_shell.contains("window._AMUX_AUTH_TOKEN=\"\""), "{revoked_shell}");
        assert!(
            !revoked_shell.contains("window._AMUX_AUTH_TOKEN=\"owner-token\""),
            "{revoked_shell}"
        );
        let (replay, _, _) = raw_send(&app, "GET", &format!("/invite/{token}"), "", &[]).await;
        assert_eq!(replay, StatusCode::GONE);
    }

    #[tokio::test]
    async fn worker_scope_is_enforced_and_owner_rescope_reaches_the_existing_cookie() {
        let (app, dir) = full_app();
        let db = dir.path().join("org-full-test.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            for (id, name) in [("wrk_allowed", "allowed-worker"), ("wrk_other", "other-worker")] {
                conn.execute(
                    "INSERT INTO _amux_workers \
                     (id,display_name,name_aliases,cwd,provider,backend,environment,permissions,state,version,created_at,updated_at) \
                     VALUES (?1,?2,'[]','/tmp','claude','tmux','{}','[]','{\"state\":\"stopped\"}',0,'now','now')",
                    rusqlite::params![id, name],
                )
                .unwrap();
            }
        }

        let (created, _, body) = raw_send(
            &app,
            "POST",
            "/api/org/teams",
            // Team creation accepts a registry id but stores the canonical
            // display/session name used by fleet and board authorization.
            r#"{"name":"Équipe autorisée","scope_level":"worker","scope_name":"wrk_allowed"}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(created, StatusCode::CREATED, "{body}");
        let team: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(team["scope_level"], "worker");
        assert_eq!(team["scope_name"], "allowed-worker");
        let team_id = team["id"].as_str().unwrap();
        let invite_payload = json!({"email":"worker-guest@example.com","team_id":team_id}).to_string();
        let (created, _, body) = raw_send(
            &app,
            "POST",
            "/api/org/invites",
            &invite_payload,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(created, StatusCode::CREATED, "{body}");
        let invitation: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(invitation["team_id"], team_id);
        let token = invitation["token"].as_str().unwrap();
        let (accepted, headers, _) = raw_send(
            &app,
            "POST",
            &format!("/invite/{token}"),
            "email=worker-guest%40example.com&name=Scoped+Guest",
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await;
        assert_eq!(accepted, StatusCode::SEE_OTHER);
        let cookie = headers[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap();

        let (identity_status, _, identity_body) =
            raw_send(&app, "GET", "/api/identity", "", &[("cookie", cookie)]).await;
        assert_eq!(identity_status, StatusCode::OK, "{identity_body}");
        let identity: Value = serde_json::from_str(&identity_body).unwrap();
        assert_eq!(
            identity["access_scope"],
            json!({"level": "worker", "name": "allowed-worker"})
        );
        assert_eq!(identity["team"], json!({"id":team_id,"name":"Équipe autorisée"}));

        let (allowed, _, _) = raw_send(
            &app,
            "GET",
            "/api/workers/wrk_allowed",
            "",
            &[("cookie", cookie)],
        )
        .await;
        assert_ne!(allowed, StatusCode::FORBIDDEN);
        let (denied, _, denied_body) = raw_send(
            &app,
            "GET",
            "/api/workers/wrk_other",
            "",
            &[("cookie", cookie)],
        )
        .await;
        assert_eq!(denied, StatusCode::FORBIDDEN, "{denied_body}");
        let (admin, _, _) =
            raw_send(&app, "GET", "/api/org/members", "", &[("cookie", cookie)]).await;
        assert_eq!(admin, StatusCode::FORBIDDEN);
        let (sync, _, _) = raw_send(&app, "GET", "/api/sync", "", &[("cookie", cookie)]).await;
        assert_eq!(sync, StatusCode::FORBIDDEN);

        // Receipts describe workspace-wide commands. A scoped membership must
        // not read another worker's metadata through a caller-supplied filter.
        for path in ["/api/interactions/recent", "/api/interactions/int_other",
            "/api/interactions/int_other/effects", "/api/interactions/int_other/why",
            "/api/debug/interactions", "/api/state/summary?scope=worker:other"] {
            let (status, _, body) = raw_send(&app, "GET", path, "", &[("cookie", cookie)]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        }

        let member_id: String = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT id FROM org_members WHERE email='worker-guest@example.com'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (rescoped, _, rescope_body) = raw_send(
            &app,
            "PATCH",
            &format!("/api/org/members/{member_id}"),
            r#"{"team_id":"team_global"}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(rescoped, StatusCode::OK, "{rescope_body}");
        let (now_allowed, _, _) = raw_send(
            &app,
            "GET",
            "/api/workers/wrk_other",
            "",
            &[("cookie", cookie)],
        )
        .await;
        assert_ne!(now_allowed, StatusCode::FORBIDDEN, "same cookie must observe the rescope");
        let (_, _, identity_body) =
            raw_send(&app, "GET", "/api/identity", "", &[("cookie", cookie)]).await;
        let identity: Value = serde_json::from_str(&identity_body).unwrap();
        assert_eq!(identity["access_scope"], json!({"level": "global", "name": ""}));

        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE org_members SET team_id='missing-team',scope_level='global',scope_name='' WHERE id=?1",
                [&member_id],
            )
            .unwrap();
        let (_, _, identity_body) =
            raw_send(&app, "GET", "/api/identity", "", &[("cookie", cookie)]).await;
        let identity: Value = serde_json::from_str(&identity_body).unwrap();
        assert_eq!(
            identity["access_scope"],
            json!({"level": "worker", "name": "__invalid_team_scope__"})
        );
        let (fails_closed, _, _) = raw_send(
            &app,
            "GET",
            "/api/workers/wrk_other",
            "",
            &[("cookie", cookie)],
        )
        .await;
        assert_eq!(fails_closed, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn group_team_join_reaches_only_workers_with_the_exact_group_tag() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        std::fs::write(
            home.path().join("sessions/allowed-worker.env"),
            "CC_TAGS=research,priority-p1\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join("sessions/other-worker.env"),
            "CC_TAGS=research-archive\n",
        )
        .unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (app, _dir) = full_app();

        let (created, _, body) = raw_send(
            &app,
            "POST",
            "/api/org/teams",
            r#"{"name":"Research","scope_level":"group","scope_name":"research"}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(created, StatusCode::CREATED, "{body}");
        let team: Value = serde_json::from_str(&body).unwrap();
        let team_id = team["id"].as_str().unwrap();
        let payload = json!({"email":"group@example.com","team_id":team_id}).to_string();
        let (created, _, body) = raw_send(
            &app,
            "POST",
            "/api/org/invites",
            &payload,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(created, StatusCode::CREATED, "{body}");
        let invite: Value = serde_json::from_str(&body).unwrap();
        let token = invite["token"].as_str().unwrap();
        let (accepted, headers, _) = raw_send(
            &app,
            "POST",
            &format!("/invite/{token}"),
            "email=group%40example.com&name=Group+Member",
            &[("content-type", "application/x-www-form-urlencoded")],
        )
        .await;
        assert_eq!(accepted, StatusCode::SEE_OTHER);
        let cookie = headers[header::SET_COOKIE].to_str().unwrap().split(';').next().unwrap();
        let (_, _, identity_body) =
            raw_send(&app, "GET", "/api/identity", "", &[("cookie", cookie)]).await;
        let identity: Value = serde_json::from_str(&identity_body).unwrap();
        assert_eq!(identity["team"], json!({"id":team_id,"name":"Research"}));
        assert_eq!(identity["access_scope"], json!({"level":"group","name":"research"}));

        let (allowed, _, _) = raw_send(
            &app,
            "GET",
            "/api/sessions/allowed-worker/output",
            "",
            &[("cookie", cookie)],
        )
        .await;
        assert_ne!(allowed, StatusCode::FORBIDDEN);
        let (denied, _, body) = raw_send(
            &app,
            "GET",
            "/api/sessions/other-worker/output",
            "",
            &[("cookie", cookie)],
        )
        .await;
        assert_eq!(denied, StatusCode::FORBIDDEN, "{body}");
    }

    #[tokio::test]
    async fn team_lifecycle_refuses_to_delete_a_team_that_still_grants_access() {
        let (app, _dir) = full_app();
        let (created, _, body) = raw_send(
            &app,
            "POST",
            "/api/org/teams",
            r#"{"name":"Temporary","scope_level":"global","scope_name":""}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(created, StatusCode::CREATED, "{body}");
        let team: Value = serde_json::from_str(&body).unwrap();
        let team_id = team["id"].as_str().unwrap();

        let payload = json!({"team_id":team_id}).to_string();
        let (invited, _, body) = raw_send(
            &app,
            "POST",
            "/api/org/invites",
            &payload,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(invited, StatusCode::CREATED, "{body}");
        let invite: Value = serde_json::from_str(&body).unwrap();
        let token = invite["token"].as_str().unwrap();
        let (blocked, _, body) = raw_send(
            &app,
            "DELETE",
            &format!("/api/org/teams/{team_id}"),
            "",
            &[("authorization", "Bearer owner-token")],
        )
        .await;
        assert_eq!(blocked, StatusCode::CONFLICT, "{body}");

        let (revoked, _, body) = raw_send(
            &app,
            "DELETE",
            &format!("/api/org/invites/{token}"),
            "",
            &[("authorization", "Bearer owner-token")],
        )
        .await;
        assert_eq!(revoked, StatusCode::OK, "{body}");
        let (patched, _, body) = raw_send(
            &app,
            "PATCH",
            &format!("/api/org/teams/{team_id}"),
            r#"{"name":"Temporary renamed","scope_level":"global","scope_name":""}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(patched, StatusCode::OK, "{body}");
        let (deleted, _, body) = raw_send(
            &app,
            "DELETE",
            &format!("/api/org/teams/{team_id}"),
            "",
            &[("authorization", "Bearer owner-token")],
        )
        .await;
        assert_eq!(deleted, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn an_invite_refuses_a_scope_target_that_does_not_exist() {
        let (app, _dir) = full_app();
        let (status, _, body) = raw_send(
            &app,
            "POST",
            "/api/org/invites",
            r#"{"scope_level":"group","scope_name":"not-a-real-group"}"#,
            &[("authorization", "Bearer owner-token"), ("content-type", "application/json")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("scope target does not exist"), "{body}");
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
