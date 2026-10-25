//! The amux ChatGPT app (AMUX-5396, Ethan 2026-09-30: "make and publish an
//! amux chatgpt plugin").
//!
//! ChatGPT apps are remote MCP servers. This module is that server: a
//! stateless Streamable HTTP endpoint at `POST /mcp`, plus the OAuth 2.1
//! authorization server ChatGPT needs to connect to it (PKCE S256 only,
//! dynamic client registration, refresh with rotation).
//!
//! # Tools
//!
//! Six, all through amux's own HTTP API on loopback so every gate, stamp and
//! refusal the dashboard gets applies here unchanged, and a refusal reaches
//! ChatGPT verbatim: `list_workers`, `read_worker`, `list_board` (read), and
//! `message_worker`, `add_card`, `update_card_status` (write). No tool
//! deletes, kills, archives, or discards anything. A message from ChatGPT
//! carries a server-written `[amux-origin: chatgpt ...]` stamp naming the
//! grant, and never a worker header, so it cannot pass as a session.
//!
//! # The trust rule (why the loopback bypass is not enough here)
//!
//! `auth.rs` admits every loopback request, and the public path to this
//! endpoint (the cloud tunnel relay) connects FROM loopback. So:
//!
//! 1. `/mcp` never consults `require_bearer`. It sits outside that layer and
//!    admits a request only with a live OAuth access token minted here, for
//!    this resource, unrevoked and unexpired. Loopback earns nothing.
//! 2. Approving a connection is an owner act. The approve route sits behind
//!    `require_bearer` AND refuses when the request carries any sign it was
//!    relayed: the tunnel relay's own `x-amux-tunnel-relay` stamp, or a
//!    `forwarded` / `x-forwarded-for` header, unless the request also presents
//!    the owner token or a valid owner session itself. It also refuses a
//!    worker origin (`x-amux-session` / `x-amux-worker`) and a local member.
//!    A plain dashboard on this machine or the LAN with the owner token
//!    passes; a request that came in from the internet does not.
//! 3. The relay in MCP mode only forwards the OAuth and `/mcp` paths (see
//!    `runtime_jobs::tunnel::mcp_path_allowed`), so `/api/*` is unreachable
//!    through it at all. Rule 2 is the second wall, for a relay started in
//!    the older whole-port mode.
//!
//! # The connect flow
//!
//! ChatGPT registers (`/oauth/register`), sends the user to
//! `/oauth/authorize`, which records a PENDING request and shows a short code.
//! The owner approves that code in Settings > ChatGPT. The authorize page polls
//! `/oauth/authorize/status` with a secret only that page holds, receives the
//! one-time code, and redirects back to ChatGPT with `code`, `state` and `iss`.
//!
//! Every refusal logs a `verdict` and every grant, revoke and tool call
//! writes a line to `~/.amux/logs/chatgpt-app-audit.jsonl`.

use super::AppState;
use crate::config::{amux_home, now_f64};
use axum::body::Bytes;
use axum::extract::{Path as AxPath, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const ACCESS_TTL_S: f64 = 3600.0;
pub const REFRESH_TTL_S: f64 = 30.0 * 86400.0;
pub const CODE_TTL_S: f64 = 120.0;
pub const REQUEST_TTL_S: f64 = 900.0;
pub const SCOPES: &[&str] = &["amux:read", "amux:write"];
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const RELAY_HEADER: &str = "x-amux-tunnel-relay";
const PUBLIC_BASE_HEADER: &str = "x-amux-public-base";
/// The cloud front door's workspace callback (see `redirect_allowed`).
pub(crate) const FRONT_DOOR_CALLBACK: &str = "https://cloud.amux.io/oauth/workspace/callback";

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn rand_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

pub(crate) fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// RFC 7636 S256: BASE64URL(SHA256(verifier)) with no padding.
pub(crate) fn pkce_s256_ok(verifier: &str, challenge: &str) -> bool {
    use base64::Engine as _;
    if !(43..=128).contains(&verifier.len())
        || !verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
    {
        return false;
    }
    let got = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    super::auth::constant_time_eq(got.as_bytes(), challenge.as_bytes())
}

/// ChatGPT's two callback shapes, plus loopback for local MCP clients such as
/// the MCP Inspector. `AMUX_MCP_REDIRECT_ALLOW` (comma-separated prefixes)
/// adds more; it cannot remove these.
pub(crate) fn redirect_allowed(uri: &str, extra: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(uri) else {
        return false;
    };
    if u.fragment().is_some() || !u.username().is_empty() {
        return false;
    }
    let host = u.host_str().unwrap_or("");
    let path = u.path();
    let chatgpt = u.scheme() == "https"
        && host == "chatgpt.com"
        && u.port().is_none()
        && (path == "/connector_platform_oauth_redirect" || path.starts_with("/connector/oauth/"));
    let loopback = u.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    // The cloud front door (cloud/gateway/chatgpt_front.py) is itself an
    // OAuth client of this workspace: exactly this callback and nothing else
    // on that host. It lets one directory listing reach every user's own
    // amux; the owner still approves the code here.
    let front_door = uri == FRONT_DOOR_CALLBACK;
    let listed = extra
        .split(',')
        .map(str::trim)
        .filter(|p| p.starts_with("https://") && p.len() > "https://x/".len())
        .any(|p| uri.starts_with(p));
    chatgpt || loopback || front_door || listed
}

fn extra_redirects() -> String {
    std::env::var("AMUX_MCP_REDIRECT_ALLOW").unwrap_or_default()
}

/// The public origin this request was addressed to. The tunnel relay stamps
/// `x-amux-public-base` (after stripping any inbound copy); otherwise it is
/// the host the client used: the `Host` header on HTTP/1.1, the `:authority`
/// (carried in the request URI) on HTTP/2, which sends no `Host` header. The
/// first live run fell back to `localhost` over HTTP/2 for exactly that reason.
pub(crate) fn public_base(headers: &HeaderMap, uri: &Uri) -> String {
    if let Some(b) = headers
        .get(PUBLIC_BASE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| s.starts_with("https://"))
    {
        return b.to_string();
    }
    let ok = |h: &&str| !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c));
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(ok)
        .or_else(|| uri.authority().map(|a| a.as_str()).filter(ok))
        .unwrap_or("localhost");
    format!("https://{host}")
}

fn resource_of(base: &str) -> String {
    format!("{base}/mcp")
}

fn audit(entry: Value) {
    use std::io::Write;
    let path = amux_home().join("logs").join("chatgpt-app-audit.jsonl");
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{entry}");
    }
}

fn oauth_error(code: StatusCode, error: &str, desc: &str, verdict: &str) -> Response {
    tracing::warn!(target: "amux::chatgpt_app", verdict, error, desc, measured = true, n_considered = 1,
        "chatgpt app: oauth request refused");
    let mut r = (code, Json(json!({"error": error, "error_description": desc}))).into_response();
    r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
    r
}

fn form(body: &Bytes) -> Vec<(String, String)> {
    super::fs::parse_qs(&String::from_utf8_lossy(body))
}

fn kv_get<'a>(kv: &'a [(String, String)], k: &str) -> &'a str {
    super::fs::qs_get(kv, k).unwrap_or("")
}

fn normalize_scope(requested: &str) -> Option<String> {
    let asked: Vec<&str> = requested.split_whitespace().collect();
    if asked.is_empty() {
        return Some(SCOPES.join(" "));
    }
    if asked.iter().any(|s| !SCOPES.contains(s)) {
        return None;
    }
    Some(SCOPES.iter().filter(|s| asked.contains(s)).copied().collect::<Vec<_>>().join(" "))
}

/// Run one write on the writer thread and hand its value back.
async fn write_ret<T, F>(state: &AppState, f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let s2 = slot.clone();
    state
        .store
        .write_async(move |c| {
            let v = f(c)?;
            *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await?;
    let v = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    v.ok_or_else(|| anyhow::anyhow!("write produced no value"))
}

// ---------------------------------------------------------------------------
// Storage (pure over a Connection, so tests drive it directly)
// ---------------------------------------------------------------------------

pub(crate) fn db_register_client(c: &Connection, name: &str, uris: &[String], now: f64) -> rusqlite::Result<String> {
    let id = format!("amux-mcp-{}", rand_hex(12));
    c.execute(
        "INSERT INTO chatgpt_oauth_clients(client_id, client_name, redirect_uris, created) VALUES(?1,?2,?3,?4)",
        params![id, name, serde_json::to_string(uris).unwrap_or_default(), now],
    )?;
    Ok(id)
}

pub(crate) fn db_client(c: &Connection, id: &str) -> rusqlite::Result<Option<(String, Vec<String>)>> {
    c.query_row(
        "SELECT client_name, redirect_uris FROM chatgpt_oauth_clients WHERE client_id=?1",
        [id],
        |r| {
            let uris: String = r.get(1)?;
            Ok((r.get::<_, String>(0)?, serde_json::from_str(&uris).unwrap_or_default()))
        },
    )
    .optional()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn db_create_request(
    c: &Connection,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    scope: &str,
    resource: &str,
    state: &str,
    poll_secret: &str,
    now: f64,
) -> rusqlite::Result<(String, String)> {
    let id = format!("cg-{}", rand_hex(8));
    let raw = rand_hex(4).to_uppercase();
    let user_code = format!("{}-{}", &raw[..4], &raw[4..]);
    c.execute(
        "INSERT INTO chatgpt_oauth_requests(id, client_id, redirect_uri, code_challenge, scope, resource, state, poll_hash, user_code, created)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![id, client_id, redirect_uri, challenge, scope, resource, state, sha256_hex(poll_secret), user_code, now],
    )?;
    Ok((id, user_code))
}

/// pending -> approved | denied. Only a pending, unexpired request moves.
pub(crate) fn db_decide(c: &Connection, id: &str, approve: bool, now: f64) -> rusqlite::Result<bool> {
    let n = c.execute(
        "UPDATE chatgpt_oauth_requests SET status=?2, decided_at=?3
         WHERE id=?1 AND status='pending' AND created > ?4",
        params![id, if approve { "approved" } else { "denied" }, now, now - REQUEST_TTL_S],
    )?;
    Ok(n == 1)
}

pub(crate) enum PollOutcome {
    Pending,
    Denied,
    Expired,
    Unknown,
    /// redirect_uri, state, one-time code
    Approved(String, String, String),
}

/// The authorize page's poll. Mints the code once, on the first poll after
/// approval, and hands it only to the holder of the poll secret.
pub(crate) fn db_poll(c: &Connection, id: &str, poll_secret: &str, now: f64) -> rusqlite::Result<PollOutcome> {
    let row: Option<(String, String, String, String, f64, Option<String>)> = c
        .query_row(
            "SELECT status, poll_hash, redirect_uri, state, created, code_hash FROM chatgpt_oauth_requests WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((status, poll_hash, redirect_uri, st, created, code_hash)) = row else {
        return Ok(PollOutcome::Unknown);
    };
    if !super::auth::constant_time_eq(poll_hash.as_bytes(), sha256_hex(poll_secret).as_bytes()) {
        return Ok(PollOutcome::Unknown);
    }
    match status.as_str() {
        "denied" | "revoked" => Ok(PollOutcome::Denied),
        "approved" if code_hash.is_none() => {
            let code = rand_hex(24);
            c.execute(
                "UPDATE chatgpt_oauth_requests SET code_hash=?2, code_expires=?3 WHERE id=?1",
                params![id, sha256_hex(&code), now + CODE_TTL_S],
            )?;
            Ok(PollOutcome::Approved(redirect_uri, st, code))
        }
        "approved" => Ok(PollOutcome::Expired),
        _ if created < now - REQUEST_TTL_S => Ok(PollOutcome::Expired),
        _ => Ok(PollOutcome::Pending),
    }
}

pub(crate) struct Issued {
    pub access: String,
    pub refresh: String,
    pub scope: String,
    pub grant_id: String,
}

fn insert_pair(c: &Connection, grant: &str, client: &str, scope: &str, resource: &str, now: f64) -> rusqlite::Result<Issued> {
    let access = format!("amuxat_{}", rand_hex(32));
    let refresh = format!("amuxrt_{}", rand_hex(32));
    for (tok, kind, ttl) in [(&access, "access", ACCESS_TTL_S), (&refresh, "refresh", REFRESH_TTL_S)] {
        c.execute(
            "INSERT INTO chatgpt_oauth_tokens(token_hash, kind, grant_id, client_id, scope, resource, created, expires)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![sha256_hex(tok), kind, grant, client, scope, resource, now, now + ttl],
        )?;
    }
    Ok(Issued { access, refresh, scope: scope.to_string(), grant_id: grant.to_string() })
}

/// Authorization-code exchange. Every check runs inside the one write, and
/// the code is burned before tokens exist, so a replay finds it used.
#[allow(clippy::too_many_arguments)]
pub(crate) fn db_redeem_code(
    c: &Connection,
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    verifier: &str,
    resource: &str,
    now: f64,
) -> rusqlite::Result<Result<Issued, &'static str>> {
    type CodeRow = (String, String, String, String, String, String, f64, i64, String);
    let row: Option<CodeRow> = c
        .query_row(
            "SELECT id, client_id, redirect_uri, code_challenge, scope, resource, code_expires, code_used, status
             FROM chatgpt_oauth_requests WHERE code_hash=?1",
            [sha256_hex(code)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
        )
        .optional()?;
    let Some((id, cid, ruri, challenge, scope, res, expires, used, status)) = row else {
        return Ok(Err("unknown_code"));
    };
    if used != 0 {
        // A replayed code is the textbook sign of interception: kill the grant.
        c.execute("UPDATE chatgpt_oauth_tokens SET revoked_at=?2 WHERE grant_id=?1 AND revoked_at IS NULL", params![id, now])?;
        c.execute("UPDATE chatgpt_oauth_requests SET status='revoked' WHERE id=?1", [&id])?;
        return Ok(Err("code_replayed"));
    }
    c.execute("UPDATE chatgpt_oauth_requests SET code_used=1 WHERE id=?1", [&id])?;
    if status != "approved" {
        return Ok(Err("grant_not_approved"));
    }
    if expires < now {
        return Ok(Err("code_expired"));
    }
    if cid != client_id {
        return Ok(Err("client_mismatch"));
    }
    if ruri != redirect_uri {
        return Ok(Err("redirect_uri_mismatch"));
    }
    if !resource.is_empty() && resource != res {
        return Ok(Err("resource_mismatch"));
    }
    if !pkce_s256_ok(verifier, &challenge) {
        return Ok(Err("pkce_failed"));
    }
    insert_pair(c, &id, &cid, &scope, &res, now).map(Ok)
}

/// Refresh with rotation: the presented refresh token dies either way.
pub(crate) fn db_refresh(c: &Connection, refresh: &str, client_id: &str, now: f64) -> rusqlite::Result<Result<Issued, &'static str>> {
    let h = sha256_hex(refresh);
    let row: Option<(String, String, String, String, f64, Option<f64>)> = c
        .query_row(
            "SELECT grant_id, client_id, scope, resource, expires, revoked_at FROM chatgpt_oauth_tokens WHERE token_hash=?1 AND kind='refresh'",
            [&h],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((grant, cid, scope, res, expires, revoked)) = row else {
        return Ok(Err("unknown_refresh_token"));
    };
    if revoked.is_some() {
        return Ok(Err("refresh_token_revoked"));
    }
    c.execute("UPDATE chatgpt_oauth_tokens SET revoked_at=?2 WHERE token_hash=?1", params![h, now])?;
    if expires < now {
        return Ok(Err("refresh_token_expired"));
    }
    if cid != client_id {
        return Ok(Err("client_mismatch"));
    }
    let live: Option<String> = c
        .query_row("SELECT status FROM chatgpt_oauth_requests WHERE id=?1", [&grant], |r| r.get(0))
        .optional()?;
    if live.as_deref() != Some("approved") {
        return Ok(Err("grant_revoked"));
    }
    insert_pair(c, &grant, &cid, &scope, &res, now).map(Ok)
}

pub(crate) struct Bearer {
    pub grant_id: String,
    pub client_id: String,
    pub scope: String,
}

pub(crate) fn db_check_access(c: &Connection, token: &str, resource: &str, now: f64) -> rusqlite::Result<Result<Bearer, &'static str>> {
    let row: Option<(String, String, String, String, f64, Option<f64>)> = c
        .query_row(
            "SELECT grant_id, client_id, scope, resource, expires, revoked_at FROM chatgpt_oauth_tokens WHERE token_hash=?1 AND kind='access'",
            [sha256_hex(token)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((grant_id, client_id, scope, res, expires, revoked)) = row else {
        return Ok(Err("unknown_token"));
    };
    if revoked.is_some() {
        return Ok(Err("token_revoked"));
    }
    if expires < now {
        return Ok(Err("token_expired"));
    }
    if res != resource {
        return Ok(Err("audience_mismatch"));
    }
    Ok(Ok(Bearer { grant_id, client_id, scope }))
}

pub(crate) fn db_revoke_grant(c: &Connection, grant: &str, now: f64) -> rusqlite::Result<usize> {
    c.execute("UPDATE chatgpt_oauth_requests SET status='revoked', decided_at=?2 WHERE id=?1 AND status='approved'", params![grant, now])?;
    c.execute("UPDATE chatgpt_oauth_tokens SET revoked_at=?2 WHERE grant_id=?1 AND revoked_at IS NULL", params![grant, now])
}

fn db_list(c: &Connection, now: f64) -> rusqlite::Result<Vec<Value>> {
    let mut st = c.prepare(
        "SELECT r.id, r.client_id, COALESCE(k.client_name,''), r.redirect_uri, r.scope, r.resource, r.status, r.user_code, r.created, r.decided_at, r.last_used
         FROM chatgpt_oauth_requests r LEFT JOIN chatgpt_oauth_clients k ON k.client_id=r.client_id
         WHERE r.status IN ('approved') OR (r.status='pending' AND r.created > ?1)
         ORDER BY r.created DESC LIMIT 100",
    )?;
    let rows = st.query_map([now - REQUEST_TTL_S], |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?, "client_id": r.get::<_, String>(1)?, "client_name": r.get::<_, String>(2)?,
            "redirect_uri": r.get::<_, String>(3)?, "scope": r.get::<_, String>(4)?, "resource": r.get::<_, String>(5)?,
            "status": r.get::<_, String>(6)?, "user_code": r.get::<_, String>(7)?, "created": r.get::<_, f64>(8)?,
            "decided_at": r.get::<_, Option<f64>>(9)?, "last_used": r.get::<_, Option<f64>>(10)?,
        }))
    })?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Outside `require_bearer`: every one of these authenticates for itself.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(protected_resource_meta))
        .route("/.well-known/oauth-protected-resource/mcp", get(protected_resource_meta))
        .route("/.well-known/oauth-authorization-server", get(auth_server_meta))
        .route("/.well-known/openid-configuration", get(auth_server_meta))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/authorize/status", get(authorize_status))
        .route("/oauth/token", post(token))
        .route("/mcp", post(mcp).get(mcp_not_allowed).delete(mcp_not_allowed))
}

/// Behind `require_bearer`, and `owner_refusal` on top of it.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/chatgpt-app", get(owner_list))
        .route("/api/chatgpt-app/requests/{id}/approve", post(owner_approve))
        .route("/api/chatgpt-app/requests/{id}/deny", post(owner_deny))
        .route("/api/chatgpt-app/grants/{id}/revoke", post(owner_revoke))
}

pub(crate) fn protected_resource_body(base: &str) -> Value {
    json!({
        "resource": resource_of(base),
        "authorization_servers": [base],
        "scopes_supported": SCOPES,
        "bearer_methods_supported": ["header"],
        "resource_name": "amux",
        "resource_documentation": "https://amux.io/",
    })
}

async fn protected_resource_meta(headers: HeaderMap, uri: Uri) -> Response {
    Json(protected_resource_body(&public_base(&headers, &uri))).into_response()
}

pub(crate) fn auth_server_body(base: &str) -> Value {
    json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": SCOPES,
        "authorization_response_iss_parameter_supported": true,
        "client_id_metadata_document_supported": false,
    })
}

async fn auth_server_meta(headers: HeaderMap, uri: Uri) -> Response {
    Json(auth_server_body(&public_base(&headers, &uri))).into_response()
}

async fn register(State(state): State<AppState>, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let uris: Vec<String> = v
        .get("redirect_uris")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    if uris.is_empty() || uris.len() > 10 {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", "redirect_uris must list 1 to 10 URIs", "chatgpt_register_no_redirects");
    }
    let extra = extra_redirects();
    if let Some(bad) = uris.iter().find(|u| !redirect_allowed(u, &extra)) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri",
            &format!("{bad} is not an allowed redirect; ChatGPT's callbacks and loopback are. Add others with AMUX_MCP_REDIRECT_ALLOW."),
            "chatgpt_register_redirect_refused");
    }
    let method = v.get("token_endpoint_auth_method").and_then(Value::as_str).unwrap_or("none");
    if method != "none" {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client_metadata",
            "only public clients (token_endpoint_auth_method=none, with PKCE) are supported", "chatgpt_register_auth_method_refused");
    }
    let name: String = v.get("client_name").and_then(Value::as_str).unwrap_or("").chars().take(80).collect();
    let (n2, u2) = (name.clone(), uris.clone());
    let now = now_f64();
    match write_ret(&state, move |c| db_register_client(c, &n2, &u2, now)).await {
        Ok(id) => {
            audit(json!({"ts": now, "event": "client_registered", "client_id": id, "client_name": name, "redirect_uris": uris}));
            (StatusCode::CREATED, Json(json!({
                "client_id": id, "client_id_issued_at": now as i64, "client_name": name,
                "redirect_uris": uris, "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"],
            }))).into_response()
        }
        Err(e) => oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string(), "chatgpt_register_store_failed"),
    }
}

pub(crate) fn wants_json(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("application/json"))
}

fn html_page(title: &str, body: &str) -> Response {
    let page = format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>{t}</title><style>body{{font:16px -apple-system,system-ui,sans-serif;background:#111;color:#eee;display:flex;\
         min-height:100vh;align-items:center;justify-content:center;margin:0}}main{{max-width:420px;padding:24px}}\
         code{{font-size:28px;letter-spacing:3px;background:#222;padding:6px 12px;border-radius:6px}}.dim{{color:#999}}</style></head>\
         <body><main>{body}</main></body></html>",
        t = crate::integrations::email::html_escape(title)
    );
    let mut r = Html(page).into_response();
    r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
    r.headers_mut().insert("x-frame-options", HeaderValue::from_static("DENY"));
    r
}

async fn authorize(State(state): State<AppState>, headers: HeaderMap, uri: Uri, RawQuery(q): RawQuery) -> Response {
    let kv = super::fs::parse_qs(q.as_deref().unwrap_or(""));
    let (client_id, redirect_uri) = (kv_get(&kv, "client_id").to_string(), kv_get(&kv, "redirect_uri").to_string());
    let store = state.store.clone();
    let cid = client_id.clone();
    let client = tokio::task::spawn_blocking(move || store.read().ok().and_then(|c| db_client(&c, &cid).ok().flatten()))
        .await
        .ok()
        .flatten();
    // Errors before the redirect_uri is trusted must NOT redirect (RFC 6749 4.1.2.1).
    let Some((client_name, uris)) = client else {
        tracing::warn!(target: "amux::chatgpt_app", verdict = "chatgpt_authorize_unknown_client", client_id, "chatgpt app: authorize for an unregistered client");
        return html_page("amux", "<h2>Unknown client</h2><p class=dim>This connection was not registered with this amux. Start the connection again from ChatGPT.</p>");
    };
    if !uris.contains(&redirect_uri) || !redirect_allowed(&redirect_uri, &extra_redirects()) {
        tracing::warn!(target: "amux::chatgpt_app", verdict = "chatgpt_authorize_redirect_mismatch", client_id, redirect_uri, "chatgpt app: authorize redirect_uri not registered");
        return html_page("amux", "<h2>Redirect not allowed</h2><p class=dim>The return address does not match this client's registration.</p>");
    }
    let bounce = |err: &str, desc: &str, verdict: &str| -> Response {
        tracing::warn!(target: "amux::chatgpt_app", verdict, err, "chatgpt app: authorize refused");
        let sep = if redirect_uri.contains('?') { '&' } else { '?' };
        let loc = format!(
            "{redirect_uri}{sep}error={}&error_description={}&state={}&iss={}",
            err, crate::integrations::email::urlencode(desc),
            crate::integrations::email::urlencode(kv_get(&kv, "state")),
            crate::integrations::email::urlencode(&public_base(&headers, &uri))
        );
        (StatusCode::FOUND, [(axum::http::header::LOCATION, loc)]).into_response()
    };
    if kv_get(&kv, "response_type") != "code" {
        return bounce("unsupported_response_type", "only response_type=code", "chatgpt_authorize_response_type");
    }
    let challenge = kv_get(&kv, "code_challenge").to_string();
    if kv_get(&kv, "code_challenge_method") != "S256" || challenge.len() < 43 {
        return bounce("invalid_request", "PKCE with code_challenge_method=S256 is required", "chatgpt_authorize_pkce_missing");
    }
    let Some(scope) = normalize_scope(kv_get(&kv, "scope")) else {
        return bounce("invalid_scope", "supported scopes: amux:read amux:write", "chatgpt_authorize_bad_scope");
    };
    let base = public_base(&headers, &uri);
    let resource = resource_of(&base);
    let asked_res = kv_get(&kv, "resource");
    if !asked_res.is_empty() && asked_res.trim_end_matches('/') != resource {
        return bounce("invalid_target", "resource must be this server's /mcp URL", "chatgpt_authorize_resource_mismatch");
    }
    let poll_secret = rand_hex(24);
    let st = kv_get(&kv, "state").to_string();
    let (c2, r2, ch2, sc2, res2, st2, ps2) = (client_id.clone(), redirect_uri.clone(), challenge, scope.clone(), resource.clone(), st, poll_secret.clone());
    let now = now_f64();
    let made = write_ret(&state, move |c| db_create_request(c, &c2, &r2, &ch2, &sc2, &res2, &st2, &ps2, now)).await;
    let (id, user_code) = match made {
        Ok(v) => v,
        Err(e) => return bounce("server_error", &e.to_string(), "chatgpt_authorize_store_failed"),
    };
    audit(json!({"ts": now, "event": "grant_requested", "request": id, "client_id": client_id, "client_name": client_name, "scope": scope, "resource": resource}));
    tracing::warn!(target: "amux::chatgpt_app", verdict = "chatgpt_grant_pending_owner", request = %id, user_code = %user_code,
        "chatgpt app: a connection is waiting for the owner's approval in Settings > ChatGPT");
    // A programmatic client (the cloud front door) asks for JSON: the same
    // facts the page below shows whoever opened it, so nothing new leaks.
    // The approval itself still happens in Settings > ChatGPT.
    if wants_json(&headers) {
        let mut r = Json(json!({
            "status": "pending", "request": id, "user_code": user_code,
            "poll": format!("oauth/authorize/status?id={id}&k={poll_secret}"),
        }))
        .into_response();
        r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
        return r;
    }
    let esc = crate::integrations::email::html_escape;
    let body = format!(
        "<h2>Approve in amux</h2><p>{who} wants to use your amux ({scope}).</p>\
         <p>Open amux on a device you own, go to <b>Settings &gt; ChatGPT</b>, and approve this code:</p>\
         <p><code>{code}</code></p><p class=dim id=s>Waiting for approval. This page continues by itself.</p>\
         <script>(function(){{var u='oauth/authorize/status?id={id}&k={k}';var p=location.pathname.replace(/oauth\\/authorize$/,'');\
         function t(){{fetch(p+u,{{cache:'no-store'}}).then(function(r){{return r.json()}}).then(function(d){{\
         if(d.redirect){{location.replace(d.redirect);return}}if(d.status!=='pending'){{document.getElementById('s').textContent=d.message||d.status;return}}\
         setTimeout(t,2000)}}).catch(function(){{setTimeout(t,4000)}})}}t()}})()</script>",
        who = esc(if client_name.is_empty() { "A ChatGPT connector" } else { &client_name }),
        scope = esc(&scope),
        code = esc(&user_code),
        id = id,
        k = poll_secret,
    );
    html_page("Approve amux connection", &body)
}

async fn authorize_status(State(state): State<AppState>, headers: HeaderMap, uri: Uri, RawQuery(q): RawQuery) -> Response {
    let kv = super::fs::parse_qs(q.as_deref().unwrap_or(""));
    let (id, k) = (kv_get(&kv, "id").to_string(), kv_get(&kv, "k").to_string());
    let now = now_f64();
    let out = write_ret(&state, move |c| db_poll(c, &id, &k, now)).await;
    let body = match out {
        Ok(PollOutcome::Approved(ruri, st, code)) => {
            let sep = if ruri.contains('?') { '&' } else { '?' };
            let enc = crate::integrations::email::urlencode;
            json!({"status": "approved", "redirect": format!("{ruri}{sep}code={}&state={}&iss={}", enc(&code), enc(&st), enc(&public_base(&headers, &uri)))})
        }
        Ok(PollOutcome::Pending) => json!({"status": "pending"}),
        Ok(PollOutcome::Denied) => json!({"status": "denied", "message": "The owner declined this connection."}),
        Ok(PollOutcome::Expired) => json!({"status": "expired", "message": "This request expired. Start the connection again from ChatGPT."}),
        Ok(PollOutcome::Unknown) => json!({"status": "unknown", "message": "Unknown request."}),
        Err(e) => json!({"status": "error", "message": e.to_string()}),
    };
    let mut r = Json(body).into_response();
    r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
    r
}

async fn token(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let kv = form(&body);
    let now = now_f64();
    let client_id = kv_get(&kv, "client_id").to_string();
    let issued = match kv_get(&kv, "grant_type") {
        "authorization_code" => {
            let (code, ruri, ver) = (kv_get(&kv, "code").to_string(), kv_get(&kv, "redirect_uri").to_string(), kv_get(&kv, "code_verifier").to_string());
            let res = kv_get(&kv, "resource").trim_end_matches('/').to_string();
            let cid = client_id.clone();
            write_ret(&state, move |c| db_redeem_code(c, &code, &cid, &ruri, &ver, &res, now)).await
        }
        "refresh_token" => {
            let rt = kv_get(&kv, "refresh_token").to_string();
            let cid = client_id.clone();
            write_ret(&state, move |c| db_refresh(c, &rt, &cid, now)).await
        }
        other => {
            return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", &format!("grant_type {other:?}"), "chatgpt_token_bad_grant_type");
        }
    };
    let _ = headers;
    match issued {
        Ok(Ok(i)) => {
            audit(json!({"ts": now, "event": "token_issued", "grant": i.grant_id, "client_id": client_id, "grant_type": kv_get(&kv, "grant_type")}));
            let mut r = Json(json!({
                "access_token": i.access, "token_type": "Bearer", "expires_in": ACCESS_TTL_S as i64,
                "refresh_token": i.refresh, "scope": i.scope,
            }))
            .into_response();
            r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
            r
        }
        Ok(Err(why)) => {
            audit(json!({"ts": now, "event": "token_refused", "client_id": client_id, "why": why}));
            oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", why, "chatgpt_token_refused")
        }
        Err(e) => oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", &e.to_string(), "chatgpt_token_store_failed"),
    }
}

fn challenge(base: &str, error: Option<&str>) -> String {
    let mut s = format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\", scope=\"{}\"", SCOPES.join(" "));
    if let Some(e) = error {
        s.push_str(&format!(", error=\"{e}\""));
    }
    s
}

fn unauthorized(base: &str, why: &str) -> Response {
    tracing::warn!(target: "amux::chatgpt_app", verdict = "chatgpt_mcp_unauthorized", why, measured = true, n_considered = 1,
        "chatgpt app: /mcp refused a request without a live access token");
    let mut r = (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized", "why": why}))).into_response();
    if let Ok(v) = HeaderValue::from_str(&challenge(base, (why != "missing_token").then_some("invalid_token"))) {
        r.headers_mut().insert(axum::http::header::WWW_AUTHENTICATE, v);
    }
    r
}

async fn mcp_not_allowed() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(axum::http::header::ALLOW, "POST")], "This MCP endpoint is stateless: POST JSON-RPC only.").into_response()
}

async fn mcp(State(state): State<AppState>, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    let base = public_base(&headers, &uri);
    let resource = resource_of(&base);
    let Some(tok) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
    else {
        return unauthorized(&base, "missing_token");
    };
    let store = state.store.clone();
    let res2 = resource.clone();
    let now = now_f64();
    let checked = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let c = store.read()?;
        Ok(db_check_access(&c, &tok, &res2, now)?)
    })
    .await;
    let bearer = match checked {
        Ok(Ok(Ok(b))) => b,
        Ok(Ok(Err(why))) => return unauthorized(&base, why),
        _ => return unauthorized(&base, "token_store_unreadable"),
    };
    let gid = bearer.grant_id.clone();
    let _ = state.store.write_async(move |c| {
        c.execute("UPDATE chatgpt_oauth_requests SET last_used=?2 WHERE id=?1", params![gid, now])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await;

    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))).into_response(),
    };
    if req.is_array() {
        return Json(rpc_error(Value::Null, -32600, "batches are not supported")).into_response();
    }
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = id else {
        // A notification (notifications/initialized and friends): no reply body.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let out = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let v = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[1] };
            rpc_ok(id, json!({
                "protocolVersion": v,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "amux", "title": "amux", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "amux runs a fleet of coding-agent workers. Use list_workers to see them, read_worker to see what one is doing, message_worker to instruct one, and the board tools for tasks.",
            }))
        }
        "ping" => rpc_ok(id, json!({})),
        "tools/list" => rpc_ok(id, json!({"tools": tool_defs()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let res = call_tool(&state, &bearer, &base, &name, &args).await;
            rpc_ok(id, res)
        }
        other => rpc_error(id, -32601, &format!("method not found: {other}")),
    };
    Json(out).into_response()
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn write_tool(name: &str) -> bool {
    matches!(name, "message_worker" | "add_card" | "update_card_status")
}

pub(crate) fn tool_defs() -> Value {
    let ro = json!({"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false});
    let rw = json!({"readOnlyHint": false, "destructiveHint": false, "openWorldHint": false});
    json!([
        {"name": "list_workers", "title": "List workers",
         "description": "List the amux workers (coding-agent sessions) with their status, groups and description. Archived workers are hidden unless include_archived is true.",
         "inputSchema": {"type": "object", "properties": {
            "group": {"type": "string", "description": "Only workers in this group"},
            "include_archived": {"type": "boolean"}}},
         "annotations": ro},
        {"name": "read_worker", "title": "Read a worker's recent output",
         "description": "Return the recent terminal transcript of one worker, newest last.",
         "inputSchema": {"type": "object", "required": ["name"], "properties": {
            "name": {"type": "string"},
            "lines": {"type": "integer", "minimum": 10, "maximum": 400, "description": "How many recent lines (default 120)"}}},
         "annotations": ro},
        {"name": "message_worker", "title": "Send a message to a worker",
         "description": "Deliver a message to a worker as a new instruction. It is stamped as coming from ChatGPT. amux's delivery rules apply: a busy worker gets it queued, a paused one when it resumes.",
         "inputSchema": {"type": "object", "required": ["name", "text"], "properties": {
            "name": {"type": "string"}, "text": {"type": "string", "maxLength": 8000}}},
         "annotations": rw},
        {"name": "list_board", "title": "List board cards",
         "description": "List open cards on the amux board (archived and finished cards are left out unless status asks for them).",
         "inputSchema": {"type": "object", "properties": {
            "session": {"type": "string", "description": "Only cards owned by this worker"},
            "status": {"type": "string", "enum": ["backlog", "todo", "doing", "done", "verified"]},
            "limit": {"type": "integer", "minimum": 1, "maximum": 200}}},
         "annotations": ro},
        {"name": "add_card", "title": "Add a board card",
         "description": "Create a card on the amux board. amux may fold it into an existing card with the same work; the result says which happened.",
         "inputSchema": {"type": "object", "required": ["title"], "properties": {
            "title": {"type": "string", "maxLength": 300}, "desc": {"type": "string", "maxLength": 8000},
            "session": {"type": "string", "description": "Worker that owns the card"},
            "status": {"type": "string", "enum": ["backlog", "todo"]}}},
         "annotations": rw},
        {"name": "update_card_status", "title": "Move a board card",
         "description": "Move a card to backlog, todo, doing or done. Board gates apply (done needs evidence); a refusal is returned as the board wrote it.",
         "inputSchema": {"type": "object", "required": ["id", "status"], "properties": {
            "id": {"type": "string"}, "status": {"type": "string", "enum": ["backlog", "todo", "doing", "done"]},
            "evidence": {"type": "string", "maxLength": 4000}}},
         "annotations": rw},
    ])
}

fn tool_result(v: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&v).unwrap_or_default();
    json!({"content": [{"type": "text", "text": text}], "structuredContent": v, "isError": is_error})
}

fn tool_err(msg: &str) -> Value {
    tool_result(json!({"error": msg}), true)
}

fn local_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// One call to amux's own API as the owner. The OAuth check already ran; this
/// hop is the dashboard's path, so every gate the dashboard meets applies.
async fn local(state: &AppState, grant: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let url = format!("https://127.0.0.1:{}{path}", crate::legacy_port::canonical_port());
    let mut rb = local_client().request(method, &url).header("x-amux-client", format!("chatgpt-app; grant={grant}"));
    if let Some(t) = &state.auth_token {
        rb = rb.bearer_auth(t);
    }
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    match rb.send().await {
        Ok(r) => {
            let code = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            (code, serde_json::from_str(&text).unwrap_or(json!({"body": text.chars().take(2000).collect::<String>()})))
        }
        Err(e) => (0, json!({"error": format!("amux API unreachable on loopback: {e}")})),
    }
}

fn enc(s: &str) -> String {
    crate::integrations::email::urlencode(s)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

async fn call_tool(state: &AppState, b: &Bearer, _base: &str, name: &str, args: &Value) -> Value {
    let now = now_f64();
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    if write_tool(name) && !b.scope.split_whitespace().any(|x| x == "amux:write") {
        tracing::warn!(target: "amux::chatgpt_app", verdict = "chatgpt_tool_scope_refused", tool = name, grant = %b.grant_id, "chatgpt app: write tool without amux:write");
        return tool_err("this connection was approved read-only (amux:read); reconnect with amux:write to use this tool");
    }
    audit(json!({"ts": now, "event": "tool_call", "tool": name, "grant": b.grant_id, "client_id": b.client_id,
        "target": args.get("name").or_else(|| args.get("id")).cloned().unwrap_or(Value::Null)}));
    let g = b.grant_id.as_str();
    match name {
        "list_workers" => {
            let (code, v) = local(state, g, reqwest::Method::GET, "/api/sessions", None).await;
            if code != 200 {
                return tool_result(json!({"status": code, "response": v}), true);
            }
            let group = s("group");
            let all = args.get("include_archived").and_then(Value::as_bool).unwrap_or(false);
            let rows: Vec<Value> = v
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|w| all || !w.get("archived").and_then(Value::as_bool).unwrap_or(false))
                .filter(|w| group.is_empty() || w.get("tags").and_then(Value::as_array).is_some_and(|t| t.iter().any(|x| x.as_str() == Some(group.as_str()))))
                .take(300)
                .map(|w| json!({
                    "name": w.get("name"), "running": w.get("running"), "status": w.get("status"),
                    "agent_state": w.get("agent_state"), "groups": w.get("tags"), "desc": w.get("desc"),
                    "model": w.get("model"), "archived": w.get("archived"), "last_activity": w.get("last_activity"),
                }))
                .collect();
            tool_result(json!({"count": rows.len(), "workers": rows}), false)
        }
        "read_worker" => {
            let n = s("name");
            if !super::session_verbs::valid_session_name(&n) {
                return tool_err("name must be an existing worker name");
            }
            let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(120).clamp(10, 400) as usize;
            let (code, v) = local(state, g, reqwest::Method::GET, &format!("/api/sessions/{}/peek?lines={}", enc(&n), lines.max(200)), None).await;
            if code != 200 {
                return tool_result(json!({"status": code, "response": v}), true);
            }
            let raw = v.get("history").and_then(Value::as_str).filter(|h| !h.is_empty())
                .or_else(|| v.get("output").and_then(Value::as_str)).unwrap_or("");
            let text = strip_ansi(raw);
            let all: Vec<&str> = text.lines().collect();
            let tail = all[all.len().saturating_sub(lines)..].join("\n");
            let tail: String = if tail.len() > 24_000 { tail[tail.len() - 24_000..].chars().collect() } else { tail };
            tool_result(json!({"name": n, "lines": tail.lines().count(), "transcript": tail}), false)
        }
        "message_worker" => {
            let (n, text) = (s("name"), s("text"));
            if !super::session_verbs::valid_session_name(&n) || text.is_empty() {
                return tool_err("name and text are required");
            }
            let text: String = text.chars().take(8000).collect();
            let stamped = format!(
                "[amux-origin: chatgpt (owner-approved connector, grant {g}), stamped by the amux server]\n\n{text}"
            );
            let (code, v) = local(state, g, reqwest::Method::POST, &format!("/api/sessions/{}/send", enc(&n)), Some(json!({"text": stamped}))).await;
            tool_result(json!({"status": code, "response": v}), !(200..300).contains(&code))
        }
        "list_board" => {
            let sess = s("session");
            let status = s("status");
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 200) as usize;
            let path = if sess.is_empty() { "/api/board?all=1".to_string() } else { format!("/api/board?session={}", enc(&sess)) };
            let (code, v) = local(state, g, reqwest::Method::GET, &path, None).await;
            if code != 200 {
                return tool_result(json!({"status": code, "response": v}), true);
            }
            let rows: Vec<Value> = v
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|c| !c.get("archived").and_then(Value::as_bool).unwrap_or(false) && c.get("archived").and_then(Value::as_i64).unwrap_or(0) == 0)
                .filter(|c| {
                    let st = c.get("status").and_then(Value::as_str).unwrap_or("");
                    if status.is_empty() { !matches!(st, "done" | "verified" | "discarded") } else { st == status }
                })
                .take(limit)
                .map(|c| json!({"id": c.get("id"), "title": c.get("title"), "status": c.get("status"),
                    "session": c.get("session"), "updated": c.get("updated"), "desc_head": c.get("desc_head")}))
                .collect();
            tool_result(json!({"count": rows.len(), "population": "live cards (not archived; finished cards only when status asks)", "cards": rows}), false)
        }
        "add_card" => {
            let title = s("title");
            if title.is_empty() {
                return tool_err("title is required");
            }
            let st = match s("status").as_str() { "backlog" => "backlog", _ => "todo" };
            let mut body = json!({"title": title.chars().take(300).collect::<String>(), "status": st});
            let desc = s("desc");
            body["desc"] = json!(format!("{}{}(added from ChatGPT, grant {g})", desc, if desc.is_empty() { "" } else { "\n\n" }));
            let sess = s("session");
            if !sess.is_empty() {
                body["session"] = json!(sess);
            }
            let (code, v) = local(state, g, reqwest::Method::POST, "/api/board", Some(body)).await;
            let created = code == 201 || v.get("card_created").and_then(Value::as_bool) == Some(true);
            tool_result(json!({"status": code, "created": created,
                "meaning": if created { "a new card" } else if (200..300).contains(&code) { "amux folded this into an existing card" } else { "refused" },
                "response": v}), !(200..300).contains(&code))
        }
        "update_card_status" => {
            let (id, st) = (s("id"), s("status"));
            if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || !matches!(st.as_str(), "backlog" | "todo" | "doing" | "done") {
                return tool_err("id and a status of backlog, todo, doing or done are required");
            }
            let mut body = json!({"status": st});
            let ev = s("evidence");
            if !ev.is_empty() {
                body["evidence"] = json!(ev);
            }
            let (code, v) = local(state, g, reqwest::Method::PATCH, &format!("/api/board/{id}"), Some(body)).await;
            if !(200..300).contains(&code) {
                return tool_result(json!({"status": code, "refused_by_board": v}), true);
            }
            // Read back: a 2xx is not proof the status is what was asked.
            let (_, after) = local(state, g, reqwest::Method::GET, &format!("/api/board/{id}"), None).await;
            let now_status = after.get("status").cloned().unwrap_or(Value::Null);
            let landed = now_status.as_str() == Some(st.as_str());
            tool_result(json!({"status": code, "requested": st, "board_now_says": now_status, "landed": landed}), !landed)
        }
        other => tool_err(&format!("unknown tool {other}")),
    }
}

// ---------------------------------------------------------------------------
// Owner surface
// ---------------------------------------------------------------------------

/// Rule 2 of the module docstring, pure so the test pins it.
pub(crate) fn owner_refusal(headers: &HeaderMap, uri: &Uri, owner_token: Option<&str>, owner_session_valid: bool) -> Option<&'static str> {
    let has = |h: &str| headers.get(h).is_some();
    if has("x-amux-session") || has("x-amux-worker") {
        return Some("worker_origin");
    }
    if super::org::is_verified_local_member(headers) {
        return Some("local_member");
    }
    let relayed = has(RELAY_HEADER) || has("forwarded") || has("x-forwarded-for");
    if relayed {
        let token_ok = owner_token.is_some_and(|exp| {
            super::auth::provided_owner_token(headers, uri).is_some_and(|t| super::auth::constant_time_eq(t.as_bytes(), exp.as_bytes()))
        });
        if !token_ok && !owner_session_valid {
            return Some("relayed_without_owner_credential");
        }
    }
    None
}

fn guard(state: &AppState, headers: &HeaderMap, uri: &Uri, action: &str) -> Option<Response> {
    if super::session_verbs::admin_origin(headers, "chatgpt_connection").is_some() { return None; }
    let valid = super::static_files::owner_session_status(state, headers) == "valid";
    let why = owner_refusal(headers, uri, state.auth_token.as_deref(), valid)?;
    tracing::warn!(target: "amux::chatgpt_app", verdict = "chatgpt_owner_action_refused", why, action, measured = true, n_considered = 1,
        "chatgpt app: only the owner, directly, can approve or revoke a ChatGPT connection");
    audit(json!({"ts": now_f64(), "event": "owner_action_refused", "action": action, "why": why}));
    Some((StatusCode::FORBIDDEN, Json(json!({"error": "only the owner can approve or revoke ChatGPT connections", "why": why}))).into_response())
}

async fn owner_list(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = guard(&state, &headers, &uri, "list") {
        return r;
    }
    let store = state.store.clone();
    let now = now_f64();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> { Ok(db_list(&*store.read()?, now)?) }).await;
    match rows {
        Ok(Ok(items)) => {
            let tun = crate::runtime_jobs::tunnel::snapshot();
            let mcp_url = if tun.running && tun.mcp_only { tun.url.as_deref().map(|u| format!("{}/mcp", u.trim_end_matches('/'))) } else { None };
            Json(json!({"ok": true, "measured": true, "n_considered": items.len(), "items": items,
                "public_mcp_url": mcp_url, "tunnel_running": tun.running, "tunnel_mcp_only": tun.mcp_only})).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "measured": false, "why_unmeasured": "store unreadable"}))).into_response(),
    }
}

async fn decide(state: AppState, headers: HeaderMap, uri: Uri, id: String, approve: bool) -> Response {
    let action = if approve { "approve" } else { "deny" };
    if let Some(r) = guard(&state, &headers, &uri, action) {
        return r;
    }
    let now = now_f64();
    let id2 = id.clone();
    match write_ret(&state, move |c| db_decide(c, &id2, approve, now)).await {
        Ok(true) => {
            audit(json!({"ts": now, "event": if approve { "grant_approved" } else { "grant_denied" }, "request": id}));
            Json(json!({"ok": true, "id": id, "status": if approve { "approved" } else { "denied" }})).into_response()
        }
        Ok(false) => (StatusCode::CONFLICT, Json(json!({"ok": false, "error": "no pending, unexpired request with that id"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))).into_response(),
    }
}

async fn owner_approve(State(state): State<AppState>, headers: HeaderMap, uri: Uri, AxPath(id): AxPath<String>) -> Response {
    decide(state, headers, uri, id, true).await
}

async fn owner_deny(State(state): State<AppState>, headers: HeaderMap, uri: Uri, AxPath(id): AxPath<String>) -> Response {
    decide(state, headers, uri, id, false).await
}

async fn owner_revoke(State(state): State<AppState>, headers: HeaderMap, uri: Uri, AxPath(id): AxPath<String>) -> Response {
    if let Some(r) = guard(&state, &headers, &uri, "revoke") {
        return r;
    }
    let now = now_f64();
    let id2 = id.clone();
    match write_ret(&state, move |c| db_revoke_grant(c, &id2, now)).await {
        Ok(n) => {
            audit(json!({"ts": now, "event": "grant_revoked", "grant": id, "tokens_revoked": n}));
            Json(json!({"ok": true, "id": id, "tokens_revoked": n})).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let c = Connection::open(dir.path().join("t.db")).unwrap();
        c.execute_batch(include_str!("../../migrations/0098_chatgpt_app_oauth.sql")).unwrap();
        (dir, c)
    }

    fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let st = AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&dir.path().join("s.db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: Some("owner-tok".into()),
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (dir, st)
    }

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn pkce_s256_matches_the_rfc_7636_vector_and_nothing_else() {
        assert!(pkce_s256_ok(VERIFIER, CHALLENGE));
        assert!(!pkce_s256_ok(VERIFIER, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cX"));
        assert!(!pkce_s256_ok("short", CHALLENGE));
        // plain is not accepted: the challenge equal to the verifier fails.
        assert!(!pkce_s256_ok(VERIFIER, VERIFIER));
    }

    #[test]
    fn redirect_allow_list_admits_chatgpt_and_loopback_only() {
        assert!(redirect_allowed("https://chatgpt.com/connector_platform_oauth_redirect", ""));
        assert!(redirect_allowed("https://chatgpt.com/connector/oauth/abc123", ""));
        assert!(redirect_allowed("http://localhost:6274/oauth/callback", ""));
        assert!(!redirect_allowed("https://chatgpt.com.evil.io/connector_platform_oauth_redirect", ""));
        assert!(!redirect_allowed("https://evil.io/connector_platform_oauth_redirect", ""));
        assert!(!redirect_allowed("http://chatgpt.com/connector_platform_oauth_redirect", ""));
        assert!(!redirect_allowed("https://chatgpt.com/other", ""));
        assert!(!redirect_allowed("https://user@chatgpt.com/connector_platform_oauth_redirect", ""));
        assert!(redirect_allowed("https://claude.ai/api/mcp/auth_callback", "https://claude.ai/api/mcp/"));
        // The cloud front door's callback, exactly: not its host, not a prefix.
        assert!(redirect_allowed(FRONT_DOOR_CALLBACK, ""));
        assert!(!redirect_allowed("https://cloud.amux.io/oauth/workspace/callback/x", ""));
        assert!(!redirect_allowed("https://cloud.amux.io/other", ""));
        assert!(!redirect_allowed("http://cloud.amux.io/oauth/workspace/callback", ""));
    }

    fn approved_request(c: &Connection, now: f64) -> (String, String, String) {
        let cid = db_register_client(c, "ChatGPT", &["https://chatgpt.com/connector_platform_oauth_redirect".into()], now).unwrap();
        let (id, _) = db_create_request(c, &cid, "https://chatgpt.com/connector_platform_oauth_redirect", CHALLENGE,
            "amux:read amux:write", "https://x.example/mcp", "st", "pollsecret", now).unwrap();
        assert!(matches!(db_poll(c, &id, "pollsecret", now).unwrap(), PollOutcome::Pending));
        assert!(db_decide(c, &id, true, now).unwrap());
        let PollOutcome::Approved(_, st, code) = db_poll(c, &id, "pollsecret", now).unwrap() else { panic!("not approved") };
        assert_eq!(st, "st");
        (cid, id, code)
    }

    #[test]
    fn the_code_needs_the_poll_secret_and_is_minted_once() {
        let (_d, c) = conn();
        let now = 1000.0;
        let cid = db_register_client(&c, "n", &["http://localhost/cb".into()], now).unwrap();
        let (id, _) = db_create_request(&c, &cid, "http://localhost/cb", CHALLENGE, "amux:read", "r", "", "k", now).unwrap();
        assert!(db_decide(&c, &id, true, now).unwrap());
        assert!(matches!(db_poll(&c, &id, "wrong", now).unwrap(), PollOutcome::Unknown));
        assert!(matches!(db_poll(&c, &id, "k", now).unwrap(), PollOutcome::Approved(..)));
        assert!(matches!(db_poll(&c, &id, "k", now).unwrap(), PollOutcome::Expired));
    }

    #[test]
    fn code_exchange_checks_pkce_redirect_client_and_burns_the_code() {
        let (_d, c) = conn();
        let now = 1000.0;
        let (cid, _id, code) = approved_request(&c, now);
        let ruri = "https://chatgpt.com/connector_platform_oauth_redirect";
        assert_eq!(db_redeem_code(&c, &code, &cid, ruri, "x".repeat(43).as_str(), "", now).unwrap().err(), Some("pkce_failed"));
        // The failed attempt above burned it; a correct retry is now a replay.
        assert_eq!(db_redeem_code(&c, &code, &cid, ruri, VERIFIER, "", now).unwrap().err(), Some("code_replayed"));

        let (cid, _id, code) = approved_request(&c, now);
        assert_eq!(db_redeem_code(&c, &code, &cid, "http://localhost/other", VERIFIER, "", now).unwrap().err(), Some("redirect_uri_mismatch"));
        let (cid, _id, code) = approved_request(&c, now);
        assert_eq!(db_redeem_code(&c, &code, "other-client", ruri, VERIFIER, "", now).unwrap().err(), Some("client_mismatch"));
        let (cid2, _id, code) = approved_request(&c, now);
        assert_eq!(db_redeem_code(&c, &code, &cid2, ruri, VERIFIER, "", now + CODE_TTL_S + 1.0).unwrap().err(), Some("code_expired"));
        let _ = cid;
    }

    #[test]
    fn tokens_expire_rotate_and_die_with_their_grant() {
        let (_d, c) = conn();
        let now = 1000.0;
        let (cid, gid, code) = approved_request(&c, now);
        let ruri = "https://chatgpt.com/connector_platform_oauth_redirect";
        let i = db_redeem_code(&c, &code, &cid, ruri, VERIFIER, "https://x.example/mcp", now).unwrap().unwrap();
        let res = "https://x.example/mcp";
        assert!(db_check_access(&c, &i.access, res, now).unwrap().is_ok());
        assert_eq!(db_check_access(&c, &i.access, "https://other/mcp", now).unwrap().err(), Some("audience_mismatch"));
        assert_eq!(db_check_access(&c, &i.access, res, now + ACCESS_TTL_S + 1.0).unwrap().err(), Some("token_expired"));
        assert_eq!(db_check_access(&c, &i.refresh, res, now).unwrap().err(), Some("unknown_token"), "a refresh token is not an access token");

        let j = db_refresh(&c, &i.refresh, &cid, now + 10.0).unwrap().unwrap();
        assert_eq!(db_refresh(&c, &i.refresh, &cid, now + 11.0).unwrap().err(), Some("refresh_token_revoked"), "rotation kills the old one");
        assert!(db_check_access(&c, &j.access, res, now + 10.0).unwrap().is_ok());

        assert!(db_revoke_grant(&c, &gid, now + 20.0).unwrap() >= 2);
        assert_eq!(db_check_access(&c, &j.access, res, now + 21.0).unwrap().err(), Some("token_revoked"));
        assert_eq!(db_refresh(&c, &j.refresh, &cid, now + 21.0).unwrap().err(), Some("refresh_token_revoked"));
    }

    #[test]
    fn a_pending_request_cannot_be_redeemed_and_expires() {
        let (_d, c) = conn();
        let cid = db_register_client(&c, "n", &["http://localhost/cb".into()], 0.0).unwrap();
        let (id, _) = db_create_request(&c, &cid, "http://localhost/cb", CHALLENGE, "amux:read", "r", "", "k", 0.0).unwrap();
        assert!(!db_decide(&c, &id, true, REQUEST_TTL_S + 1.0).unwrap(), "an expired request cannot be approved");
        assert!(matches!(db_poll(&c, &id, "k", REQUEST_TTL_S + 1.0).unwrap(), PollOutcome::Expired));
    }

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn owner_actions_refuse_relayed_requests_without_the_owner_credential() {
        let uri: Uri = "/api/chatgpt-app/requests/x/approve".parse().unwrap();
        assert_eq!(owner_refusal(&hdrs(&[]), &uri, Some("tok"), false), None, "a direct dashboard request passes");
        assert_eq!(owner_refusal(&hdrs(&[(RELAY_HEADER, "1")]), &uri, Some("tok"), false), Some("relayed_without_owner_credential"));
        assert_eq!(owner_refusal(&hdrs(&[("x-forwarded-for", "1.2.3.4")]), &uri, Some("tok"), false), Some("relayed_without_owner_credential"));
        assert_eq!(owner_refusal(&hdrs(&[(RELAY_HEADER, "1"), ("authorization", "Bearer tok")]), &uri, Some("tok"), false), None);
        assert_eq!(owner_refusal(&hdrs(&[(RELAY_HEADER, "1"), ("authorization", "Bearer nope")]), &uri, Some("tok"), false), Some("relayed_without_owner_credential"));
        assert_eq!(owner_refusal(&hdrs(&[(RELAY_HEADER, "1")]), &uri, None, true), None, "a valid owner session passes");
        assert_eq!(owner_refusal(&hdrs(&[("x-amux-session", "amux")]), &uri, Some("tok"), false), Some("worker_origin"));
    }

    fn app(st: AppState) -> Router {
        Router::new().merge(public_routes()).with_state(st)
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
        let r = app.clone().oneshot(req).await.unwrap();
        let (s, h) = (r.status(), r.headers().clone());
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (s, h, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    fn rpc(method: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::post("/mcp").header("content-type", "application/json").header("host", "amux.test");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {}}).to_string())).unwrap()
    }

    /// The loopback bypass must not reach /mcp. A request with no token (the
    /// shape a tunnel relay produces, from 127.0.0.1) is refused with the
    /// discovery challenge ChatGPT needs to start OAuth.
    #[tokio::test]
    async fn mcp_refuses_loopback_without_a_token_and_points_at_discovery() {
        let (_d, st) = state();
        let a = app(st);
        let mut req = rpc("tools/list", None);
        req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 5555))));
        let (s, h, _) = call(&a, req).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let w = h.get("www-authenticate").unwrap().to_str().unwrap();
        assert!(w.contains("resource_metadata=\"https://amux.test/.well-known/oauth-protected-resource\""), "{w}");
        let (s, _, _) = call(&a, rpc("tools/list", Some("amuxat_bogus"))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn full_flow_register_authorize_approve_token_and_tools_list() {
        let (_d, st) = state();
        let a = app(st.clone());
        let (s, _, reg) = call(&a, Request::post("/oauth/register").header("host", "amux.test").header("content-type", "application/json")
            .body(Body::from(json!({"client_name": "ChatGPT", "redirect_uris": ["https://chatgpt.com/connector_platform_oauth_redirect"]}).to_string())).unwrap()).await;
        assert_eq!(s, StatusCode::CREATED);
        let cid = reg["client_id"].as_str().unwrap().to_string();
        let (s, _, _) = call(&a, Request::post("/oauth/register").header("content-type", "application/json")
            .body(Body::from(json!({"redirect_uris": ["https://evil.io/cb"]}).to_string())).unwrap()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "an unlisted redirect is refused at registration");

        let q = format!("/oauth/authorize?response_type=code&client_id={cid}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=xyz&resource={}",
            enc("https://chatgpt.com/connector_platform_oauth_redirect"), enc("https://amux.test/mcp"));
        let r = a.clone().oneshot(Request::get(&q).header("host", "amux.test").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let page = String::from_utf8(axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
        let id = page.split("status?id=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let k = page.split("&k=").nth(1).unwrap().split('\'').next().unwrap().to_string();

        let (_, _, p) = call(&a, Request::get(format!("/oauth/authorize/status?id={id}&k={k}")).header("host", "amux.test").body(Body::empty()).unwrap()).await;
        assert_eq!(p["status"], "pending");
        let now = now_f64();
        let id2 = id.clone();
        assert!(write_ret(&st, move |c| db_decide(c, &id2, true, now)).await.unwrap());
        let (_, _, p) = call(&a, Request::get(format!("/oauth/authorize/status?id={id}&k={k}")).header("host", "amux.test").body(Body::empty()).unwrap()).await;
        let redirect = p["redirect"].as_str().unwrap();
        assert!(redirect.starts_with("https://chatgpt.com/connector_platform_oauth_redirect?code="), "{redirect}");
        assert!(redirect.contains("state=xyz") && redirect.contains("iss=https%3A%2F%2Famux.test"), "{redirect}");
        let code = redirect.split("code=").nth(1).unwrap().split('&').next().unwrap();

        let form = format!("grant_type=authorization_code&code={code}&client_id={cid}&redirect_uri={}&code_verifier={VERIFIER}&resource={}",
            enc("https://chatgpt.com/connector_platform_oauth_redirect"), enc("https://amux.test/mcp"));
        let (s, _, t) = call(&a, Request::post("/oauth/token").header("host", "amux.test").header("content-type", "application/x-www-form-urlencoded").body(Body::from(form)).unwrap()).await;
        assert_eq!(s, StatusCode::OK, "{t}");
        let at = t["access_token"].as_str().unwrap();

        let (s, _, init) = call(&a, rpc("initialize", Some(at))).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(init["result"]["serverInfo"]["name"], "amux");
        let (_, _, tl) = call(&a, rpc("tools/list", Some(at))).await;
        let names: Vec<&str> = tl["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_workers", "read_worker", "message_worker", "list_board", "add_card", "update_card_status"]);
        for t in tl["result"]["tools"].as_array().unwrap() {
            assert_eq!(t["annotations"]["destructiveHint"], false, "no destructive tool ships");
            assert_eq!(t["annotations"]["readOnlyHint"], !write_tool(t["name"].as_str().unwrap()));
        }
        let (s, _, _) = call(&a, Request::post("/mcp").header("host", "amux.test").header("authorization", format!("Bearer {at}"))
            .body(Body::from(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string())).unwrap()).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        let (_, _, e) = call(&a, rpc("nope/nope", Some(at))).await;
        assert_eq!(e["error"]["code"], -32601);
        // Same token, different public host: the audience no longer matches.
        let mut req = rpc("tools/list", Some(at));
        req.headers_mut().insert("host", HeaderValue::from_static("elsewhere.test"));
        assert_eq!(call(&a, req).await.0, StatusCode::UNAUTHORIZED);
    }

    /// The cloud front door is an OAuth client of this workspace: it registers
    /// its one callback, asks authorize for JSON, and gets the same request id,
    /// user code and poll secret the page would show. Nothing is approved by
    /// asking; the grant stays pending until the owner decides.
    #[tokio::test]
    async fn the_front_door_gets_the_pending_request_as_json_and_it_stays_pending() {
        let (_d, st) = state();
        let a = app(st);
        let (s, _, reg) = call(&a, Request::post("/oauth/register").header("host", "amux.test").header("content-type", "application/json")
            .body(Body::from(json!({"client_name": "amux cloud", "redirect_uris": [FRONT_DOOR_CALLBACK]}).to_string())).unwrap()).await;
        assert_eq!(s, StatusCode::CREATED, "{reg}");
        let cid = reg["client_id"].as_str().unwrap();
        let q = format!("/oauth/authorize?response_type=code&client_id={cid}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256&state=s1",
            enc(FRONT_DOOR_CALLBACK));
        let (s, _, j) = call(&a, Request::get(&q).header("host", "amux.test").header("accept", "application/json").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(j["status"], "pending");
        assert!(j["user_code"].as_str().unwrap().len() >= 4, "{j}");
        let poll = j["poll"].as_str().unwrap();
        assert!(poll.starts_with("oauth/authorize/status?id=") && poll.contains("&k="), "{poll}");
        let (_, _, p) = call(&a, Request::get(format!("/{poll}")).header("host", "amux.test").body(Body::empty()).unwrap()).await;
        assert_eq!(p["status"], "pending", "asking for JSON approves nothing");
    }

    #[test]
    fn public_base_reads_the_http2_authority_when_there_is_no_host_header() {
        let uri: Uri = "https://amux.example:8824/mcp".parse().unwrap();
        assert_eq!(public_base(&HeaderMap::new(), &uri), "https://amux.example:8824");
        let mut h = HeaderMap::new();
        h.insert("host", HeaderValue::from_static("lan.example"));
        assert_eq!(public_base(&h, &uri), "https://lan.example", "an HTTP/1.1 Host header wins");
        assert_eq!(public_base(&HeaderMap::new(), &"/mcp".parse().unwrap()), "https://localhost");
    }

    #[tokio::test]
    async fn metadata_names_one_issuer_and_s256() {
        let (_d, st) = state();
        let a = app(st);
        let (_, _, pr) = call(&a, Request::get("/.well-known/oauth-protected-resource").header(PUBLIC_BASE_HEADER, "https://ab12.t.amux.io/").body(Body::empty()).unwrap()).await;
        assert_eq!(pr["resource"], "https://ab12.t.amux.io/mcp");
        let (_, _, asm) = call(&a, Request::get("/.well-known/oauth-authorization-server").header(PUBLIC_BASE_HEADER, "https://ab12.t.amux.io").body(Body::empty()).unwrap()).await;
        assert_eq!(asm["issuer"], pr["authorization_servers"][0]);
        assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]));
        let (s, _, _) = call(&a, Request::get("/mcp").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    }
}
