//! Gmail OAuth flow (SPA long-tail port): `/api/gmail/auth` +
//! `/api/gmail/callback` + `/api/gmail/accounts` (+ account delete and the
//! legacy /connect tombstone), ported from the Python `_gmail_auth_url` /
//! callback handlers so BOTH servers mint and consume the SAME artifacts:
//!
//! - OAuth client config: `~/.amux/gmail-oauth-client.json` (`installed` or
//!   `web` node). NO credential values live in this file or its tests;
//!   absent/incomplete config answers an honest 503 naming the missing keys
//!   (Python answers 500 — the 503 is deliberate: "not configured" is a
//!   service state, not a server bug).
//! - Scopes, redirect URI (canonical-port `/api/gmail/callback`),
//!   `access_type=offline include_granted_scopes=true prompt=consent
//!   login_hint=<account>` and S256 PKCE match google_auth_oauthlib's Flow.
//! - Pending state persists to `~/.amux/gmail-pending.json` in Python's
//!   exact `{state: {account, verifier, ts}}` shape, 1h TTL, single-use —
//!   so a consent click survives a server restart AND either server can
//!   complete a flow the other started (the 2026-07-31 cotenant-restart
//!   incident is what this file exists for).
//! - The callback writes `~/.amux/gmail-tokens/<email>.json` in the exact
//!   shape `integrations::email::GmailClient::load_token_file` reads
//!   (`token`/`refresh_token`/`token_uri`/`client_id`/`client_secret`) —
//!   the round-trip test writes with THIS module and reads with THAT one.
//! - `/accounts` reports `health` from a REAL Gmail `getProfile` round trip
//!   (cached 300s), because token-file presence is not connectedness
//!   (2026-08-07: `accounts` said connected while every read failed).
//!   `invalid_grant` on refresh discriminates `needs_reauth` from
//!   `not_connected`, exactly like Python.
//! - The `__gcal__` shared-callback case (calendar OAuth) is NOT ported:
//!   it answers an honest 501 naming the missing capability instead of
//!   silently dropping the grant.
//! - Mount note: the callback is registered on the PUBLIC router. Python
//!   guards it only by the localhost auth bypass — Google's redirect
//!   carries no bearer token, so behind `require_bearer` (which has no
//!   localhost bypass) every consent click would 401. The endpoint holds no
//!   secrets: it consumes a single-use state we minted and writes a token
//!   file only Google's code exchange can produce.

use super::AppState;
use crate::integrations::email::{
    connected_accounts_in, default_amux_home, html_escape, urlencode, HttpTransport,
    ReqwestTransport, DEFAULT_TOKEN_URI, GMAIL_BASE,
};
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const GMAIL_SCOPES: [&str; 4] = [
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/gmail.send",
    "https://www.googleapis.com/auth/gmail.settings.basic",
    "https://www.googleapis.com/auth/userinfo.email",
];
/// The OAuth callback URI handed to Google in the auth request and echoed back
/// in the token exchange. It MUST match a redirect URI registered on the OAuth
/// client byte-for-byte, so it is overridable via the `GMAIL_REDIRECT_URI` env
/// (`~/.amux/server.env`) for a deployment whose registered URI is not the
/// default.
///
/// The default follows [`crate::config::canonical_port`] rather than a hardcoded
/// literal. The old constant hardcoded the retired 8822 address and dead-ended
/// the moment that bind was dropped (AMUX-2943 / AMUX-3026): Google redirected the
/// browser to a port nothing answered, so amux never saw the callback and no
/// account could re-authorize. Deriving it from the canonical port means the
/// callback is always served where this server actually listens, and a future
/// port move is a console re-registration rather than a second code change.
pub fn gmail_redirect_uri() -> String {
    std::env::var("GMAIL_REDIRECT_URI")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            format!(
                "http://localhost:{}/api/gmail/callback",
                crate::config::canonical_port()
            )
        })
}
const DEFAULT_AUTH_URI: &str = "https://accounts.google.com/o/oauth2/auth";
const PENDING_TTL_S: f64 = 3600.0;
const HEALTH_TTL_S: u64 = 300;

// ---- context + routers -----------------------------------------------------

pub struct GmailAuthCtx {
    pub http: Arc<dyn HttpTransport>,
    pub home: PathBuf,
    health_cache: Mutex<HashMap<String, (Instant, String)>>,
}

impl GmailAuthCtx {
    pub fn new(http: Arc<dyn HttpTransport>, home: PathBuf) -> Self {
        Self { http, home, health_cache: Mutex::new(HashMap::new()) }
    }
}

fn default_ctx() -> Arc<GmailAuthCtx> {
    Arc::new(GmailAuthCtx::new(Arc::new(ReqwestTransport::new()), default_amux_home()))
}

/// Bearer-protected routes (absolute paths — merged, not nested, so the
/// public callback below cannot collide with a nest wildcard).
pub fn routes() -> Router<AppState> {
    routes_with(default_ctx())
}

pub fn routes_with(ctx: Arc<GmailAuthCtx>) -> Router<AppState> {
    Router::new()
        .route("/api/gmail/accounts", get(accounts))
        .route("/api/gmail/auth", get(auth_url))
        .route("/api/gmail/account", axum::routing::delete(delete_account))
        .route("/api/gmail/connect", axum::routing::post(connect_legacy))
        .layer(Extension(ctx))
}

/// The PUBLIC callback (see the mount note in the module docs).
pub fn callback_routes() -> Router<AppState> {
    callback_routes_with(default_ctx())
}

pub fn callback_routes_with(ctx: Arc<GmailAuthCtx>) -> Router<AppState> {
    Router::new().route("/api/gmail/callback", get(callback)).layer(Extension(ctx))
}

// ---- shared helpers --------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn html(status: StatusCode, body: String) -> Response {
    (status, Html(body)).into_response()
}

fn token_urlsafe(nbytes: usize) -> String {
    let mut bytes = vec![0u8; nbytes];
    OsRng.fill_bytes(&mut bytes);
    crate::integrations::email::base64url_nopad(&bytes)
}

struct ClientConfig {
    client_id: String,
    client_secret: String,
    auth_uri: String,
    token_uri: String,
}

/// Load `<home>/gmail-oauth-client.json` (`installed` or `web`). The error
/// string NAMES what is missing — a credential nobody can enumerate is one
/// nobody has (docs/credentials.md).
fn client_config(home: &Path) -> Result<ClientConfig, String> {
    let p = home.join("gmail-oauth-client.json");
    let raw = std::fs::read_to_string(&p).map_err(|_| {
        // Python's message, verbatim.
        "Gmail OAuth client not configured. Save credentials to ~/.amux/gmail-oauth-client.json"
            .to_string()
    })?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("gmail-oauth-client.json is not valid JSON: {e}"))?;
    let node = v
        .get("installed")
        .or_else(|| v.get("web"))
        .ok_or_else(|| {
            "gmail-oauth-client.json has no \"installed\" or \"web\" node".to_string()
        })?;
    let s = |k: &str, d: &str| {
        node.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).unwrap_or(d).to_string()
    };
    let cfg = ClientConfig {
        client_id: s("client_id", ""),
        client_secret: s("client_secret", ""),
        auth_uri: s("auth_uri", DEFAULT_AUTH_URI),
        token_uri: s("token_uri", DEFAULT_TOKEN_URI),
    };
    if cfg.client_id.is_empty() || cfg.client_secret.is_empty() {
        return Err(
            "gmail-oauth-client.json is missing client_id and/or client_secret under \
             \"installed\"/\"web\""
                .to_string(),
        );
    }
    Ok(cfg)
}

/// The shared Google OAuth client `(client_id, client_secret)` from
/// `gmail-oauth-client.json`, exposed so the connectors platform can REUSE the
/// one client Ethan already configured instead of duplicating a secret into
/// server.env (the connectors-setup "reuse this one" note). Returns `None` when
/// the file is absent or missing either field. The values are for the server's
/// own OAuth flow and presence/masking only, never emitted raw.
pub(crate) fn google_oauth_client_file(home: &Path) -> Option<(String, String)> {
    client_config(home).ok().map(|c| (c.client_id, c.client_secret))
}

fn pending_path(home: &Path) -> PathBuf {
    home.join("gmail-pending.json")
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python `_gmail_pending_save`: prune >1h entries, add ours, 0600.
fn pending_save(home: &Path, state: &str, account: &str, verifier: &str) -> std::io::Result<()> {
    let p = pending_path(home);
    let mut d: Map<String, Value> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let now = now_ts();
    d.retain(|_, v| now - v.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < PENDING_TTL_S);
    d.insert(
        state.to_string(),
        json!({ "account": account, "verifier": verifier, "ts": now }),
    );
    std::fs::write(&p, Value::Object(d).to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Python `_gmail_pending_restore`: SINGLE-USE (the entry is removed even
/// when expired), returns (account, verifier) while fresh.
fn pending_take(home: &Path, state: &str) -> Option<(String, Option<String>)> {
    let p = pending_path(home);
    let mut d: Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
    let e = d.remove(state)?;
    let _ = std::fs::write(&p, Value::Object(d).to_string());
    if now_ts() - e.get("ts").and_then(Value::as_f64).unwrap_or(0.0) > PENDING_TTL_S {
        return None;
    }
    let account = e.get("account").and_then(Value::as_str)?.to_string();
    let verifier = e.get("verifier").and_then(Value::as_str).map(String::from);
    Some((account, verifier))
}

// ---- GET /api/gmail/auth ---------------------------------------------------

#[derive(serde::Deserialize)]
pub struct AuthParams {
    #[serde(default)]
    account: Option<String>,
}

/// The manual Google Cloud console step OAuth cannot automate: the redirect URI
/// must be registered on the client, or Google rejects the sign-in BEFORE it
/// reaches amux. Copy-paste-ready guidance, returned at auth time and on a
/// redirect_uri_mismatch so a reauth is self-service (AMUX-3352, refresh-house:
/// an undiscoverable console step blocked a reauth for ~3 days). pub(crate) so
/// the connectors broker returns the same guidance for its own Google flow.
pub(crate) fn oauth_prerequisite(client_id: &str, redirect_uri: &str) -> Value {
    json!({
        "requirement": "The redirect URI below must be registered on this OAuth client in the Google Cloud console (APIs & Services -> Credentials -> the OAuth 2.0 Client ID -> Authorized redirect URIs), or Google rejects the sign-in before it reaches amux.",
        "console_url": "https://console.cloud.google.com/apis/credentials",
        "client_id": client_id,
        "add_redirect_uri": redirect_uri,
    })
}

/// The same guidance as an HTML block for the callback error page.
fn prerequisite_html(client_id: &str, redirect_uri: &str) -> String {
    format!(
        "<p>Google rejected the sign-in because the redirect URI is not registered on the OAuth client.</p>\
         <p><b>One-time fix:</b></p><ol>\
         <li>Open <a href=\"https://console.cloud.google.com/apis/credentials\">console.cloud.google.com/apis/credentials</a></li>\
         <li>Open the OAuth 2.0 Client ID <code>{}</code></li>\
         <li>Add <code>{}</code> under <b>Authorized redirect URIs</b> and Save</li>\
         <li>Re-run the reauth (GET /api/gmail/auth) and open the URL again</li></ol>",
        html_escape(client_id),
        html_escape(redirect_uri)
    )
}

pub async fn auth_url(
    Extension(ctx): Extension<Arc<GmailAuthCtx>>,
    Query(p): Query<AuthParams>,
) -> Response {
    let account = p.account.unwrap_or_default().trim().to_string();
    if account.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "account required" }));
    }
    let cfg = match client_config(&ctx.home) {
        Ok(c) => c,
        // Honest 503: a missing credential is a service state with a named
        // way out, never a hardcoded fallback.
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": e })),
    };
    let state = token_urlsafe(24);
    // S256 PKCE, like google_auth_oauthlib's autogenerated verifier.
    let verifier = token_urlsafe(64);
    let challenge =
        crate::integrations::email::base64url_nopad(&Sha256::digest(verifier.as_bytes()));
    let scope = urlencode(&GMAIL_SCOPES.join(" "));
    let redirect_uri = gmail_redirect_uri();
    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={scope}&state={state}\
         &access_type=offline&include_granted_scopes=true&prompt=consent&login_hint={}\
         &code_challenge={challenge}&code_challenge_method=S256",
        cfg.auth_uri,
        urlencode(&cfg.client_id),
        urlencode(&redirect_uri),
        urlencode(&account),
    );
    if let Err(e) = pending_save(&ctx.home, &state, &account, &verifier) {
        // Without the pending entry the callback CANNOT succeed — failing
        // loudly beats handing out a URL that dead-ends (ethos rule 3).
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("could not persist pending auth state: {e}") }),
        );
    }
    // THE CALLBACK PORT MUST BE ONE WE ANSWER. redirect_uri now follows
    // canonical_port() (AMUX-2943 / AMUX-3026), so the default is always served
    // where this server listens. This check only fires when a GMAIL_REDIRECT_URI
    // env override names a port this process does NOT answer, a genuine
    // misconfiguration rather than the old hardcoded-8822 default.
    //
    // WITHOUT THIS CHECK THE FAILURE IS PERFECTLY SILENT: Google serves the
    // consent screen, the user approves, and the browser is redirected to a
    // port nothing listens on. amux never receives the request, so no handler
    // runs, nothing is logged, and the only symptom is a browser error page on
    // a machine that may not be the one running amux. Raised by amux-cloud on
    // AC-337, which predicted exactly this. The warning is emitted at the one
    // moment the operator is present and acting: when they ask for the URL.
    //
    // The OTHER failure mode is undetectable here: for a web OAuth client the
    // redirect_uri must ALSO be registered on the client in the Google Cloud
    // console, and Google rejects a mismatch before any redirect, so amux never
    // sees it. The info log below records the exact redirect_uri issued, so a
    // reauth that keeps failing stays diagnosable from the request log (the
    // two-fixes rule) instead of silent.
    tracing::info!(
        target: "gmail_auth",
        "[gmail] auth URL issued for {account} with redirect_uri={redirect_uri} \
         (must be registered on the OAuth client in the Google Cloud console)"
    );
    let redirect_port = redirect_uri
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|hostport| hostport.rsplit(':').next())
        .and_then(|p| p.parse::<u16>().ok());
    let canonical = crate::config::canonical_port();
    if redirect_port.is_some_and(|p| p != canonical) {
        let warning = format!(
            "This authorization WILL FAIL at the redirect. The GMAIL_REDIRECT_URI override points \
             at port {} but this server answers {}, so Google would send the browser to a port \
             nothing is listening on and amux would never see the callback. FIX: unset \
             GMAIL_REDIRECT_URI in server.env to use the canonical-port default, or set it to a \
             callback URL on port {}. Card: AMUX-3026.",
            redirect_port.unwrap_or(0),
            canonical,
            canonical
        );
        tracing::warn!(
            target: "gmail_auth",
            "[gmail/AMUX-3026] auth URL issued for {account} with a callback on port {} that this \
             server does not answer (canonical {}); the GMAIL_REDIRECT_URI override dead-ends",
            redirect_port.unwrap_or(0),
            canonical
        );
        return Json(json!({
            "url": url,
            "account": account,
            "warning": warning,
            "prerequisite": oauth_prerequisite(&cfg.client_id, &redirect_uri),
        }))
        .into_response();
    }
    Json(json!({
        "url": url,
        "account": account,
        "prerequisite": oauth_prerequisite(&cfg.client_id, &redirect_uri),
    }))
    .into_response()
}

// ---- GET /api/gmail/callback -----------------------------------------------

#[derive(serde::Deserialize)]
pub struct CallbackParams {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

pub async fn callback(
    Extension(ctx): Extension<Arc<GmailAuthCtx>>,
    Query(p): Query<CallbackParams>,
) -> Response {
    let code = p.code.unwrap_or_default().trim().to_string();
    let state = p.state.unwrap_or_default().trim().to_string();
    let error = p.error.unwrap_or_default().trim().to_string();
    if !error.is_empty() {
        // A redirect_uri_mismatch is the reauth-blocking case (AMUX-3352):
        // render the one-time console fix instead of a dead-end "Auth failed".
        if error.contains("redirect_uri_mismatch") {
            let cid = client_config(&ctx.home).map(|c| c.client_id).unwrap_or_default();
            return html(
                StatusCode::BAD_REQUEST,
                format!(
                    "<html><body><h2>Sign-in blocked: redirect URI not registered</h2>{}</body></html>",
                    prerequisite_html(&cid, &gmail_redirect_uri())
                ),
            );
        }
        // Python's page (values escaped here — same content, no reflected
        // markup).
        return html(
            StatusCode::BAD_REQUEST,
            format!(
                "<html><body><h2>Auth failed: {}</h2><p>Close this tab.</p></body></html>",
                html_escape(&error)
            ),
        );
    }
    let entry = if state.is_empty() { None } else { pending_take(&ctx.home, &state) };
    let Some((account, verifier)) = entry.filter(|_| !code.is_empty()) else {
        // Not ours? The connectors broker's google-family grants hand Google
        // THIS redirect URI on purpose — it is the one already registered on
        // the shared client — so their states land here (AMUX-3427). Single-use
        // order: our own pending store missed above, now try theirs.
        if !state.is_empty() && !code.is_empty() {
            if let Some(resp) = super::connectors::delegate_gmail_callback(
                ctx.http.clone(),
                &ctx.home,
                &state,
                &code,
            )
            .await
            {
                return resp;
            }
        }
        return html(
            StatusCode::BAD_REQUEST,
            "<html><body><h2>Invalid or expired auth request.</h2>\
             <p>Close this tab and try again.</p></body></html>"
                .to_string(),
        );
    };
    if account == "__gcal__" {
        // Python saves a calendar token here; this server has no calendar
        // OAuth consumer. Say so rather than dropping the grant silently.
        return html(
            StatusCode::NOT_IMPLEMENTED,
            "<html><body><h2>Calendar OAuth is not implemented in this server.</h2>\
             <p>The Python amux-server handles the __gcal__ flow — re-run the \
             consent link against it.</p></body></html>"
                .to_string(),
        );
    }
    let cfg = match client_config(&ctx.home) {
        Ok(c) => c,
        Err(e) => {
            return html(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "<html><body><h2>Token exchange failed</h2><pre>{}</pre></body></html>",
                    html_escape(&e)
                ),
            )
        }
    };
    let mut form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code),
        ("client_id".to_string(), cfg.client_id.clone()),
        ("client_secret".to_string(), cfg.client_secret.clone()),
        ("redirect_uri".to_string(), gmail_redirect_uri()),
    ];
    if let Some(v) = verifier {
        form.push(("code_verifier".to_string(), v));
    }
    let exchanged = ctx.http.post_form(&cfg.token_uri, &form).await;
    let (status, body) = match exchanged {
        Ok(pair) => pair,
        Err(e) => {
            return html(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "<html><body><h2>Token exchange failed</h2><pre>{}</pre></body></html>",
                    html_escape(&e)
                ),
            )
        }
    };
    let access = body.get("access_token").and_then(Value::as_str).unwrap_or("");
    if status >= 400 || access.is_empty() {
        // If the exchange itself reports a redirect_uri_mismatch, surface the
        // one-time console fix rather than a raw HTTP dump (AMUX-3352).
        if body.to_string().contains("redirect_uri_mismatch") {
            return html(
                StatusCode::BAD_REQUEST,
                format!(
                    "<html><body><h2>Sign-in blocked: redirect URI not registered</h2>{}</body></html>",
                    prerequisite_html(&cfg.client_id, &gmail_redirect_uri())
                ),
            );
        }
        return html(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "<html><body><h2>Token exchange failed</h2><pre>{}</pre></body></html>",
                html_escape(&format!("HTTP {status}: {body}"))
            ),
        );
    }
    // Python's exact token-file shape — the one integrations/email.rs
    // `load_token_file` reads. The formats must be ONE.
    let tokens_dir = ctx.home.join("gmail-tokens");
    if let Err(e) = std::fs::create_dir_all(&tokens_dir) {
        return html(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "<html><body><h2>Token exchange failed</h2><pre>{}</pre></body></html>",
                html_escape(&e.to_string())
            ),
        );
    }
    let token_file = json!({
        "token": access,
        "refresh_token": body.get("refresh_token").cloned().unwrap_or(Value::Null),
        "token_uri": cfg.token_uri,
        "client_id": cfg.client_id,
        "client_secret": cfg.client_secret,
    });
    if let Err(e) = std::fs::write(tokens_dir.join(format!("{account}.json")), token_file.to_string())
    {
        return html(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "<html><body><h2>Token exchange failed</h2><pre>{}</pre></body></html>",
                html_escape(&e.to_string())
            ),
        );
    }
    html(
        StatusCode::OK,
        format!(
            "<html><body><h2>✓ {} connected!</h2><p>You can close this tab.</p></body></html>",
            html_escape(&account)
        ),
    )
}

// ---- GET /api/gmail/accounts -----------------------------------------------

pub async fn accounts(Extension(ctx): Extension<Arc<GmailAuthCtx>>) -> Response {
    let accts = connected_accounts_in(&ctx.home);
    let mut health: Map<String, Value> = Map::new();
    let mut needs_reauth: Vec<String> = Vec::new();
    for a in &accts {
        let state = account_health(&ctx, a).await;
        if state == "needs_reauth" {
            needs_reauth.push(a.clone());
        }
        health.insert(a.clone(), json!(state));
    }
    Json(json!({
        "accounts": accts,
        "health": health,
        "needs_reauth": needs_reauth,
        "note": "`accounts` = a token file exists. `health` = the token still works. \
                 A revoked token stays in `accounts` and fails every read, so check \
                 `health` before concluding a mailbox is usable.",
        "reauth": "GET /api/gmail/auth?account=<email>, open the url, approve",
    }))
    .into_response()
}

/// Python `_gmail_account_health`: 'ok' | 'needs_reauth' | 'not_connected',
/// from a REAL authenticated round trip (getProfile), cached 300s. A token
/// file that exists but cannot be exercised is exactly the disagreement
/// this field exists to expose.
async fn account_health(ctx: &GmailAuthCtx, account: &str) -> String {
    if let Some((at, state)) = ctx.health_cache.lock().expect("health cache").get(account) {
        if at.elapsed().as_secs() < HEALTH_TTL_S {
            return state.clone();
        }
    }
    let state = probe_health(ctx, account).await;
    ctx.health_cache
        .lock()
        .expect("health cache")
        .insert(account.to_string(), (Instant::now(), state.clone()));
    state
}

/// Uncached health probe for one Gmail account, callable without a
/// [`GmailAuthCtx`] — the connectors accounts rollup uses this so the
/// consolidated per-account view and `/api/gmail/accounts` share ONE
/// discriminator (`ok` / `needs_reauth` / `not_connected`) instead of two
/// probes that could disagree. Caching is the CALLER's job (the rollup holds
/// its own 300s cache; `/accounts` keeps the ctx cache).
pub(crate) async fn health_for(http: Arc<dyn HttpTransport>, home: &Path, account: &str) -> String {
    let ctx = GmailAuthCtx::new(http, home.to_path_buf());
    probe_health(&ctx, account).await
}

async fn probe_health(ctx: &GmailAuthCtx, account: &str) -> String {
    let path = ctx.home.join("gmail-tokens").join(format!("{account}.json"));
    let Some(tf) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    else {
        return "not_connected".into();
    };
    let profile_url = format!("{GMAIL_BASE}/profile");
    let access = tf.get("token").and_then(Value::as_str).unwrap_or("");
    if !access.is_empty() {
        match ctx.http.get(&profile_url, Some(access)).await {
            Ok((st, _)) if st < 400 => return "ok".into(),
            Ok((401, _)) => {} // fall through to refresh
            _ => return "not_connected".into(),
        }
    }
    // Stored access token stale/absent: exercise the refresh token — the
    // discriminator between needs_reauth (invalid_grant) and not_connected.
    let refresh = tf.get("refresh_token").and_then(Value::as_str).unwrap_or("");
    if refresh.is_empty() {
        return "not_connected".into();
    }
    let s = |k: &str| tf.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (cid, csec) = match (s("client_id"), s("client_secret")) {
        (i, s2) if !i.is_empty() && !s2.is_empty() => (i, s2),
        _ => match client_config(&ctx.home) {
            Ok(c) => (c.client_id, c.client_secret),
            Err(_) => return "not_connected".into(),
        },
    };
    let token_uri = {
        let t = s("token_uri");
        if t.is_empty() { DEFAULT_TOKEN_URI.to_string() } else { t }
    };
    let form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh.to_string()),
        ("client_id".to_string(), cid.clone()),
        ("client_secret".to_string(), csec.clone()),
    ];
    match ctx.http.post_form(&token_uri, &form).await {
        Ok((st, body)) if st < 400 => {
            let new_access = body.get("access_token").and_then(Value::as_str).unwrap_or("");
            if new_access.is_empty() {
                return "not_connected".into();
            }
            // Persist in Python's shape so the next reader skips the refresh
            // (same behavior as _gmail_load_creds).
            let _ = std::fs::write(
                &path,
                json!({
                    "token": new_access,
                    "refresh_token": refresh,
                    "token_uri": token_uri,
                    "client_id": cid,
                    "client_secret": csec,
                })
                .to_string(),
            );
            match ctx.http.get(&profile_url, Some(new_access)).await {
                Ok((st2, _)) if st2 < 400 => "ok".into(),
                _ => "not_connected".into(),
            }
        }
        Ok((_, body)) => {
            if body.to_string().contains("invalid_grant") {
                "needs_reauth".into()
            } else {
                "not_connected".into()
            }
        }
        Err(_) => "not_connected".into(),
    }
}

// ---- DELETE /api/gmail/account ---------------------------------------------

#[derive(serde::Deserialize)]
pub struct AccountParams {
    #[serde(default)]
    account: Option<String>,
}

pub async fn delete_account(
    Extension(ctx): Extension<Arc<GmailAuthCtx>>,
    Query(p): Query<AccountParams>,
) -> Response {
    let account = p.account.unwrap_or_default().trim().to_string();
    let path = ctx.home.join("gmail-tokens").join(format!("{account}.json"));
    if !account.is_empty() && path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    Json(json!({ "ok": true })).into_response()
}

// ---- POST /api/gmail/connect (legacy tombstone) ----------------------------

pub async fn connect_legacy() -> Response {
    err(
        StatusCode::GONE,
        json!({ "error": "Use the /api/gmail/auth URL flow — open the URL in a browser, it will redirect back automatically." }),
    )
}

// ---------------------------------------------------------------------------
// Tests — mocked transport + temp homes only. Placeholder strings, never
// real credentials; every token write lands in a tempdir.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::email::{base64url_nopad, GmailClient};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn prerequisite_names_the_console_client_and_redirect_uri() {
        // AMUX-3352: the reauth-blocking console step must be spelled out.
        let v = oauth_prerequisite("492989726165-abc.apps.googleusercontent.com", "https://localhost:8824/api/gmail/callback");
        assert_eq!(v["console_url"], "https://console.cloud.google.com/apis/credentials");
        assert_eq!(v["client_id"], "492989726165-abc.apps.googleusercontent.com");
        assert_eq!(v["add_redirect_uri"], "https://localhost:8824/api/gmail/callback");
        let h = prerequisite_html("492989726165-abc.apps.googleusercontent.com", "https://localhost:8824/api/gmail/callback");
        assert!(h.contains("492989726165-abc.apps.googleusercontent.com"));
        assert!(h.contains("https://localhost:8824/api/gmail/callback"));
        assert!(h.contains("Authorized redirect URIs"));
    }

    /// (method, url, bearer, body) as seen on the mocked wire.
    type RecordedCall = (String, String, Option<String>, Option<Value>);

    /// Scripted transport (same shape as the email.rs test mock): first
    /// (method, url-substring) match pops; records calls incl. bearer.
    struct MockHttp {
        calls: Mutex<Vec<RecordedCall>>,
        script: Mutex<Vec<(String, String, u16, Value)>>,
    }

    impl MockHttp {
        fn new(script: Vec<(&str, &str, u16, Value)>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(
                    script.into_iter().map(|(m, u, s, v)| (m.into(), u.into(), s, v)).collect(),
                ),
            })
        }
        fn answer(
            &self,
            method: &str,
            url: &str,
            bearer: Option<&str>,
            body: Option<&Value>,
        ) -> Result<(u16, Value), String> {
            self.calls.lock().unwrap().push((
                method.into(),
                url.into(),
                bearer.map(String::from),
                body.cloned(),
            ));
            let mut script = self.script.lock().unwrap();
            if let Some(pos) =
                script.iter().position(|(m, sub, _, _)| m == method && url.contains(sub.as_str()))
            {
                let (_, _, status, v) = script.remove(pos);
                return Ok((status, v));
            }
            Err(format!("mock has no answer for {method} {url}"))
        }
        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for MockHttp {
        async fn get(&self, url: &str, bearer: Option<&str>) -> Result<(u16, Value), String> {
            self.answer("GET", url, bearer, None)
        }
        async fn post_json(
            &self,
            url: &str,
            bearer: Option<&str>,
            body: &Value,
        ) -> Result<(u16, Value), String> {
            self.answer("POST", url, bearer, Some(body))
        }
        async fn post_form(
            &self,
            url: &str,
            form: &[(String, String)],
        ) -> Result<(u16, Value), String> {
            let v = Value::Object(form.iter().map(|(k, val)| (k.clone(), json!(val))).collect());
            self.answer("FORM", url, None, Some(&v))
        }
    }

    const ACCT: &str = "hello@amux.io";

    fn home_with_client_config() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gmail-oauth-client.json"),
            json!({ "installed": {
                "client_id": "PLACEHOLDER_ID.apps.googleusercontent.com",
                "client_secret": "PLACEHOLDER_SECRET",
            }})
            .to_string(),
        )
        .unwrap();
        dir
    }

    fn dummy_state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("g.db")).unwrap();
        (
            AppState {
                store: Arc::new(store),
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
            },
            dir,
        )
    }

    fn app_with(http: Arc<MockHttp>, home: &Path) -> (axum::Router, tempfile::TempDir) {
        let ctx = Arc::new(GmailAuthCtx::new(http, home.to_path_buf()));
        let (state, dir) = dummy_state();
        let router = Router::new()
            .merge(routes_with(ctx.clone()))
            .merge(callback_routes_with(ctx))
            .with_state(state);
        (router, dir)
    }

    async fn send(app: &axum::Router, method: &str, path: &str) -> (StatusCode, String) {
        let req = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn send_json(app: &axum::Router, method: &str, path: &str) -> (StatusCode, Value) {
        let (st, body) = send(app, method, path).await;
        (st, serde_json::from_str(&body).unwrap_or(Value::String(body)))
    }

    fn qparam<'a>(url: &'a str, key: &str) -> Option<&'a str> {
        url.split(['?', '&']).find_map(|kv| kv.strip_prefix(&format!("{key}=")))
    }

    #[tokio::test]
    async fn auth_url_pinned_against_pythons_scopes_and_flow_params() {
        let home = home_with_client_config();
        let (app, _d) = app_with(MockHttp::new(vec![]), home.path());
        let (st, v) = send_json(&app, "GET", "/api/gmail/auth?account=hello%40amux.io").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["account"], json!(ACCT));
        let url = v["url"].as_str().unwrap();
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/auth?"), "{url}");
        // The four Python scopes, space-joined then percent-encoded.
        let want_scope = urlencode(
            "https://www.googleapis.com/auth/gmail.modify \
             https://www.googleapis.com/auth/gmail.send \
             https://www.googleapis.com/auth/gmail.settings.basic \
             https://www.googleapis.com/auth/userinfo.email",
        );
        assert_eq!(qparam(url, "scope"), Some(want_scope.as_str()), "{url}");
        assert_eq!(qparam(url, "response_type"), Some("code"));
        assert_eq!(qparam(url, "access_type"), Some("offline"));
        assert_eq!(qparam(url, "prompt"), Some("consent"));
        assert_eq!(qparam(url, "include_granted_scopes"), Some("true"));
        assert_eq!(qparam(url, "login_hint"), Some(urlencode(ACCT).as_str()));
        assert_eq!(
            qparam(url, "client_id"),
            Some(urlencode("PLACEHOLDER_ID.apps.googleusercontent.com").as_str())
        );
        assert_eq!(qparam(url, "redirect_uri"), Some(urlencode(&gmail_redirect_uri()).as_str()));
        assert_eq!(qparam(url, "code_challenge_method"), Some("S256"));

        // Pending state persisted in Python's file shape, challenge derived
        // from the stored verifier (S256).
        let state = qparam(url, "state").unwrap();
        let pending: Value =
            serde_json::from_str(&std::fs::read_to_string(home.path().join("gmail-pending.json")).unwrap())
                .unwrap();
        let entry = &pending[state];
        assert_eq!(entry["account"], json!(ACCT));
        assert!(entry["ts"].as_f64().unwrap() > 0.0);
        let verifier = entry["verifier"].as_str().unwrap();
        let want_challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(qparam(url, "code_challenge"), Some(want_challenge.as_str()));
    }

    #[tokio::test]
    async fn missing_config_is_honest_503_and_missing_account_400() {
        let empty_home = tempfile::tempdir().unwrap();
        let (app, _d) = app_with(MockHttp::new(vec![]), empty_home.path());
        let (st, v) = send_json(&app, "GET", "/api/gmail/auth?account=x%40y.z").await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert_eq!(
            v["error"],
            json!("Gmail OAuth client not configured. Save credentials to ~/.amux/gmail-oauth-client.json")
        );

        let (st, v) = send_json(&app, "GET", "/api/gmail/auth").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], json!("account required"));

        // Config present but keys blank: the 503 NAMES the missing keys.
        let half_home = tempfile::tempdir().unwrap();
        std::fs::write(
            half_home.path().join("gmail-oauth-client.json"),
            json!({ "installed": { "client_id": "" } }).to_string(),
        )
        .unwrap();
        let (app2, _d2) = app_with(MockHttp::new(vec![]), half_home.path());
        let (st, v) = send_json(&app2, "GET", "/api/gmail/auth?account=x%40y.z").await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert!(v["error"].as_str().unwrap().contains("client_id"), "{v}");
        assert!(v["error"].as_str().unwrap().contains("client_secret"), "{v}");
    }

    #[tokio::test]
    async fn callback_exchanges_code_and_writes_the_one_token_format() {
        let home = home_with_client_config();
        let http = MockHttp::new(vec![(
            "FORM",
            "oauth2.googleapis.com/token",
            200,
            json!({ "access_token": "PLACEHOLDER_AT", "refresh_token": "PLACEHOLDER_RT",
                     "expires_in": 3599, "scope": "openid" }),
        )]);
        let (app, _d) = app_with(http.clone(), home.path());

        // Mint the URL (writes pending), then hit the callback like Google's
        // redirect would.
        let (_, v) = send_json(&app, "GET", "/api/gmail/auth?account=hello%40amux.io").await;
        let state = qparam(v["url"].as_str().unwrap(), "state").unwrap().to_string();
        let pending: Value =
            serde_json::from_str(&std::fs::read_to_string(home.path().join("gmail-pending.json")).unwrap())
                .unwrap();
        let verifier = pending[&state]["verifier"].as_str().unwrap().to_string();

        let (st, page) =
            send(&app, "GET", &format!("/api/gmail/callback?code=authcode123&state={state}")).await;
        assert_eq!(st, StatusCode::OK, "{page}");
        assert!(page.contains("connected!"), "{page}");

        // The exchange carried Python's grant, our redirect URI, and the
        // PKCE verifier from the pending file.
        let form = http.calls()[0].3.clone().unwrap();
        assert_eq!(form["grant_type"], json!("authorization_code"));
        assert_eq!(form["code"], json!("authcode123"));
        assert_eq!(form["redirect_uri"], json!(gmail_redirect_uri()));
        assert_eq!(form["code_verifier"], json!(verifier));

        // Token file: EXACT Python shape, nothing extra.
        let tok_path = home.path().join("gmail-tokens").join(format!("{ACCT}.json"));
        let tok: Value = serde_json::from_str(&std::fs::read_to_string(&tok_path).unwrap()).unwrap();
        assert_eq!(
            tok,
            json!({
                "token": "PLACEHOLDER_AT",
                "refresh_token": "PLACEHOLDER_RT",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "PLACEHOLDER_ID.apps.googleusercontent.com",
                "client_secret": "PLACEHOLDER_SECRET",
            })
        );

        // Round-trip with the EMAIL module's own loader: write with ours,
        // read with theirs — the formats must be ONE. GmailClient must pick
        // up the stored access token and put it on the wire as the bearer.
        let reader_http = MockHttp::new(vec![(
            "GET",
            "settings/sendAs",
            200,
            json!({ "sendAs": [{ "sendAsEmail": ACCT, "signature": "SIG_FROM_GMAIL" }] }),
        )]);
        let client = GmailClient::new(reader_http.clone(), home.path().to_path_buf());
        assert_eq!(client.connected_accounts(), vec![ACCT.to_string()]);
        assert_eq!(client.get_signature(ACCT).await, "SIG_FROM_GMAIL");
        assert_eq!(reader_http.calls()[0].2.as_deref(), Some("PLACEHOLDER_AT"));

        // Single-use state: replaying the callback must fail.
        let (st, page) =
            send(&app, "GET", &format!("/api/gmail/callback?code=authcode123&state={state}")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(page.contains("Invalid or expired"), "{page}");
    }

    #[tokio::test]
    async fn callback_error_paths_match_python() {
        let home = home_with_client_config();
        let (app, _d) = app_with(MockHttp::new(vec![]), home.path());

        let (st, page) = send(&app, "GET", "/api/gmail/callback?error=access_denied").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(page.contains("Auth failed: access_denied"), "{page}");

        let (st, page) = send(&app, "GET", "/api/gmail/callback?code=x&state=never-minted").await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(page.contains("Invalid or expired auth request."), "{page}");

        // A state without a code is invalid too (Python: `not entry or not
        // code`).
        let (_, v) = send_json(&app, "GET", "/api/gmail/auth?account=a%40b.c").await;
        let state = qparam(v["url"].as_str().unwrap(), "state").unwrap().to_string();
        let (st, page) = send(&app, "GET", &format!("/api/gmail/callback?state={state}")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(page.contains("Invalid or expired"), "{page}");

        // Exchange failure surfaces as the Python 500 page, and no token
        // file appears.
        let http = MockHttp::new(vec![(
            "FORM",
            "oauth2.googleapis.com/token",
            400,
            json!({ "error": "invalid_grant" }),
        )]);
        let (app2, _d2) = app_with(http, home.path());
        let (_, v) = send_json(&app2, "GET", "/api/gmail/auth?account=a%40b.c").await;
        let state = qparam(v["url"].as_str().unwrap(), "state").unwrap().to_string();
        let (st, page) =
            send(&app2, "GET", &format!("/api/gmail/callback?code=bad&state={state}")).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(page.contains("Token exchange failed"), "{page}");
        assert!(!home.path().join("gmail-tokens").join("a@b.c.json").exists());
    }

    #[tokio::test]
    async fn accounts_reports_health_from_a_real_round_trip() {
        let home = home_with_client_config();
        let tokens = home.path().join("gmail-tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        std::fs::write(
            tokens.join(format!("{ACCT}.json")),
            json!({
                "token": "PLACEHOLDER_LIVE",
                "refresh_token": "PLACEHOLDER_RT",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "PLACEHOLDER_ID",
                "client_secret": "PLACEHOLDER_SECRET",
            })
            .to_string(),
        )
        .unwrap();
        // Healthy account: profile answers 200.
        let http = MockHttp::new(vec![("GET", "/profile", 200, json!({ "emailAddress": ACCT }))]);
        let (app, _d) = app_with(http.clone(), home.path());
        let (st, v) = send_json(&app, "GET", "/api/gmail/accounts").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["accounts"], json!([ACCT]));
        assert_eq!(v["health"][ACCT], json!("ok"));
        assert_eq!(v["needs_reauth"], json!([]));
        assert!(v["note"].as_str().unwrap().contains("token file exists"), "{v}");
        // The probe exercised the stored token, not just the file's existence.
        assert_eq!(http.calls()[0].2.as_deref(), Some("PLACEHOLDER_LIVE"));

        // Revoked account: profile 401, refresh answers invalid_grant ->
        // needs_reauth (the discriminator, not a generic failure).
        let http2 = MockHttp::new(vec![
            ("GET", "/profile", 401, json!({ "error": "unauthorized" })),
            ("FORM", "oauth2.googleapis.com/token", 400, json!({ "error": "invalid_grant" })),
        ]);
        let (app2, _d2) = app_with(http2, home.path());
        let (_, v2) = send_json(&app2, "GET", "/api/gmail/accounts").await;
        assert_eq!(v2["health"][ACCT], json!("needs_reauth"), "{v2}");
        assert_eq!(v2["needs_reauth"], json!([ACCT]));
    }

    #[tokio::test]
    async fn full_router_auth_boundary_callback_public_accounts_gated() {
        // Through the REAL assembled router with a bearer token set: the
        // OAuth callback must answer without credentials (Google's redirect
        // cannot send any), while the other gmail routes stay behind auth.
        // This pins the mount decision in api/mod.rs, not a re-declaration
        // of it.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("full.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: Some("SECRET_BEARER".into()),
        };
        let app = crate::api::router(state);

        // Callback reachable with NO auth: a bogus state gets the Python
        // "invalid" page — NOT a 401.
        let (st, page) = send(&app, "GET", "/api/gmail/callback?code=x&state=nope").await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{page}");
        assert!(page.contains("Invalid or expired"), "{page}");

        // The protected routes 401 without the bearer. (No with-bearer
        // request here: the full router's default ctx points at the REAL
        // ~/.amux and would probe live accounts — tests never touch that.)
        for path in ["/api/gmail/accounts", "/api/gmail/auth?account=a%40b.c"] {
            let (st, _) = send(&app, "GET", path).await;
            assert_eq!(st, StatusCode::UNAUTHORIZED, "{path} must be bearer-gated");
        }
    }

    #[tokio::test]
    async fn delete_account_removes_token_and_connect_is_gone() {
        let home = tempfile::tempdir().unwrap();
        let tokens = home.path().join("gmail-tokens");
        std::fs::create_dir_all(&tokens).unwrap();
        let tok = tokens.join("bye@x.co.json");
        std::fs::write(&tok, "{}").unwrap();
        let (app, _d) = app_with(MockHttp::new(vec![]), home.path());
        let (st, v) = send_json(&app, "DELETE", "/api/gmail/account?account=bye%40x.co").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["ok"], json!(true));
        assert!(!tok.exists());

        let (st, v) = send_json(&app, "POST", "/api/gmail/connect").await;
        assert_eq!(st, StatusCode::GONE);
        assert!(v["error"].as_str().unwrap().contains("/api/gmail/auth"), "{v}");
    }
}
