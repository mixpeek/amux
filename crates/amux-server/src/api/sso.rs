//! Single sign-on through any OpenID Connect provider (governance PR 20; plan
//! 4.1 and phase 4): Google, Okta, Entra ID or anything else that publishes
//! `/.well-known/openid-configuration`.
//!
//! The flow is the authorization-code flow with PKCE (S256), a single-use
//! `state` and a `nonce`. The ID token's signature is checked against the
//! provider's published keys (RS256 or ES256 only), and so are its issuer,
//! audience, expiry and nonce. The provider must say the email is verified
//! (`email_verified: true`); `AMUX_OIDC_ALLOW_UNVERIFIED_EMAIL=1` waives that
//! for a provider that never sends it, and is reported.
//!
//! A sign-in signs in an EXISTING member (matched by email). It does not make
//! a second kind of session: it writes a used `org_invites` row and sets the
//! same `amux_member` cookie an invite link does, so everything that already
//! reads a member (scopes, the ledger actor, offboarding) reads this one. The
//! row carries `via = 'sso'`, expires after `AMUX_SSO_SESSION_HOURS` (12), and
//! `mfa_at` when the provider's `amr` claim names a second factor.
//!
//! A person who is not a member is refused, unless the owner turned on
//! just-in-time provisioning for their domain (`AMUX_OIDC_JIT_DOMAINS`); such a
//! member starts in the team `AMUX_OIDC_JIT_TEAM`, or with no access at all.
//! An offboarded member is refused.
//!
//! Requiring SSO for members is `AMUX_SSO_REQUIRED` (A11): `report` (default)
//! counts member sessions that did not come through SSO, `enforce` sends them
//! to `/sso/login`, `off` does neither. It never touches the owner: the owner's
//! own credential (bearer, or the sign-in door's owner proof) is the
//! break-glass path and always works.
//!
//! | setting | meaning |
//! |---|---|
//! | `AMUX_OIDC_ISSUER` | the issuer URL; discovery is read from it (https, or http on loopback) |
//! | `AMUX_OIDC_CLIENT_ID`, `AMUX_OIDC_CLIENT_SECRET` | the client registered with the provider (the secret may be empty for a public client) |
//! | `AMUX_OIDC_REDIRECT_URL` | defaults to `https://<host>/sso/callback` |
//! | `AMUX_OIDC_SCOPES` | defaults to `openid email profile` |

use super::AppState;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use super::worker_identity::Mode;

pub const REQUIRED_ENV: &str = "AMUX_SSO_REQUIRED";
const PENDING_TTL: Duration = Duration::from_secs(600);
const PENDING_MAX: usize = 2000;
const CACHE_TTL: Duration = Duration::from_secs(600);
/// `amr` values (RFC 8176) that mean more than a password was shown.
const MFA_AMR: &[&str] = &["mfa", "otp", "hwk", "swk", "sc", "fido", "fpt", "face", "iris", "retina", "vbm"];

pub(crate) mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static STARTED: AtomicU64 = AtomicU64::new(0);
    pub static SIGNED_IN: AtomicU64 = AtomicU64::new(0);
    pub static PROVISIONED: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static MFA_FROM_IDP: AtomicU64 = AtomicU64::new(0);
    pub static REQUIRED_WOULD_REFUSE: AtomicU64 = AtomicU64::new(0);
    pub static REQUIRED_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// What the owner configured.
#[derive(Debug, Clone)]
pub struct Config {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: Option<String>,
    pub scopes: String,
    pub jit_domains: Vec<String>,
    pub jit_team: Option<String>,
    pub allow_unverified_email: bool,
    pub session_hours: i64,
}

pub fn config() -> Option<Config> {
    Some(Config {
        issuer: env("AMUX_OIDC_ISSUER")?.trim_end_matches('/').to_string(),
        client_id: env("AMUX_OIDC_CLIENT_ID")?,
        client_secret: env("AMUX_OIDC_CLIENT_SECRET").unwrap_or_default(),
        redirect_url: env("AMUX_OIDC_REDIRECT_URL"),
        scopes: env("AMUX_OIDC_SCOPES").unwrap_or_else(|| "openid email profile".into()),
        jit_domains: env("AMUX_OIDC_JIT_DOMAINS")
            .map(|v| v.split(',').map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase()).filter(|d| !d.is_empty()).collect())
            .unwrap_or_default(),
        jit_team: env("AMUX_OIDC_JIT_TEAM"),
        allow_unverified_email: matches!(env("AMUX_OIDC_ALLOW_UNVERIFIED_EMAIL").as_deref(), Some("1") | Some("true")),
        session_hours: env("AMUX_SSO_SESSION_HOURS").and_then(|v| v.parse().ok()).filter(|h| *h > 0).unwrap_or(12),
    })
}

pub fn configured() -> bool {
    config().is_some()
}

pub fn required_mode() -> Mode {
    Mode::parse(std::env::var(REQUIRED_ENV).ok().as_deref())
}

/// https anywhere; plain http only to this machine (a test issuer).
pub fn issuer_allowed(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

// ── PKCE, state, nonce ──────────────────────────────────────────────────────

fn b64url(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn random_token(n: usize) -> Result<String, String> {
    super::mfa::random_bytes(n).map(|b| b64url(&b))
}

pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

#[derive(Debug, Clone)]
struct Pending {
    verifier: String,
    nonce: String,
    next: String,
    redirect_uri: String,
    at: Instant,
}

fn pending() -> &'static Mutex<HashMap<String, Pending>> {
    static P: OnceLock<Mutex<HashMap<String, Pending>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

fn put_pending(state: &str, p: Pending) {
    let mut m = pending().lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, v| v.at.elapsed() < PENDING_TTL);
    if m.len() >= PENDING_MAX {
        // A flood of unfinished sign-ins: drop the oldest rather than grow.
        if let Some(oldest) = m.iter().min_by_key(|(_, v)| v.at).map(|(k, _)| k.clone()) {
            m.remove(&oldest);
        }
    }
    m.insert(state.to_string(), p);
}

/// A `state` is good once and for ten minutes.
fn take_pending(state: &str) -> Option<Pending> {
    let mut m = pending().lock().unwrap_or_else(|e| e.into_inner());
    m.remove(state).filter(|p| p.at.elapsed() < PENDING_TTL)
}

/// A same-site path to land on after sign-in, never another origin.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\") && !n.contains(['\r', '\n']) => n.to_string(),
        _ => "/".into(),
    }
}

// ── Discovery and keys ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
}

type Cached<T> = Mutex<HashMap<String, (Instant, T)>>;

fn discovery_cache() -> &'static Cached<Discovery> {
    static C: OnceLock<Cached<Discovery>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn jwks_cache() -> &'static Cached<jsonwebtoken::jwk::JwkSet> {
    static C: OnceLock<Cached<jsonwebtoken::jwk::JwkSet>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

async fn discover(issuer: &str) -> Result<Discovery, String> {
    if let Some((at, d)) = discovery_cache().lock().unwrap_or_else(|e| e.into_inner()).get(issuer) {
        if at.elapsed() < CACHE_TTL {
            return Ok(d.clone());
        }
    }
    let url = format!("{issuer}/.well-known/openid-configuration");
    let d: Discovery = http()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("discovery: {e}"))?
        .error_for_status()
        .map_err(|e| format!("discovery: {e}"))?
        .json()
        .await
        .map_err(|e| format!("discovery: {e}"))?;
    // OIDC Discovery 4.3: the document's issuer is the one configured.
    if d.issuer.trim_end_matches('/') != issuer {
        return Err(format!("discovery names issuer {} but {issuer} is configured", d.issuer));
    }
    for u in [&d.authorization_endpoint, &d.token_endpoint, &d.jwks_uri] {
        if !issuer_allowed(u) {
            return Err(format!("discovery endpoint {u} is not https"));
        }
    }
    discovery_cache().lock().unwrap_or_else(|e| e.into_inner()).insert(issuer.to_string(), (Instant::now(), d.clone()));
    Ok(d)
}

async fn jwks(uri: &str, refresh: bool) -> Result<jsonwebtoken::jwk::JwkSet, String> {
    if !refresh {
        if let Some((at, k)) = jwks_cache().lock().unwrap_or_else(|e| e.into_inner()).get(uri) {
            if at.elapsed() < CACHE_TTL {
                return Ok(k.clone());
            }
        }
    }
    let k: jsonwebtoken::jwk::JwkSet = http()
        .get(uri)
        .send()
        .await
        .map_err(|e| format!("keys: {e}"))?
        .error_for_status()
        .map_err(|e| format!("keys: {e}"))?
        .json()
        .await
        .map_err(|e| format!("keys: {e}"))?;
    jwks_cache().lock().unwrap_or_else(|e| e.into_inner()).insert(uri.to_string(), (Instant::now(), k.clone()));
    Ok(k)
}

// ── The ID token ────────────────────────────────────────────────────────────

/// What a verified ID token says about the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    pub name: String,
    pub subject: String,
    pub mfa: bool,
}

/// Check an ID token against a key set: signature, issuer, audience, expiry
/// and nonce, then the email. Pure, so the unit tests drive it directly.
pub fn verify_id_token(
    token: &str,
    keys: &jsonwebtoken::jwk::JwkSet,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    allow_unverified_email: bool,
) -> Result<Identity, String> {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation};
    let head = jsonwebtoken::decode_header(token).map_err(|e| format!("id token header: {e}"))?;
    if !matches!(head.alg, Algorithm::RS256 | Algorithm::ES256) {
        return Err(format!("id token signed with {:?}; only RS256 and ES256 are accepted", head.alg));
    }
    let jwk = match &head.kid {
        Some(kid) => keys.find(kid).ok_or_else(|| format!("no published key {kid}"))?,
        None if keys.keys.len() == 1 => &keys.keys[0],
        None => return Err("id token names no key and the provider publishes several".into()),
    };
    let key = DecodingKey::from_jwk(jwk).map_err(|e| format!("published key: {e}"))?;
    let mut v = Validation::new(head.alg);
    v.set_audience(&[client_id]);
    v.set_issuer(&[issuer, &format!("{issuer}/")]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    v.leeway = 60;
    let data = jsonwebtoken::decode::<Value>(token, &key, &v).map_err(|e| format!("id token: {e}"))?;
    let c = data.claims;
    if c["nonce"].as_str() != Some(nonce) {
        return Err("id token nonce does not match this sign-in".into());
    }
    let email = c["email"].as_str().map(|e| e.trim().to_ascii_lowercase()).filter(|e| e.contains('@'))
        .ok_or("the provider sent no email address")?;
    let verified = match &c["email_verified"] {
        Value::Bool(b) => *b,
        Value::String(s) => s == "true",
        _ => false,
    };
    if !verified && !allow_unverified_email {
        return Err("the provider did not say this email address is verified".into());
    }
    let mfa = c["amr"].as_array().is_some_and(|a| a.iter().filter_map(|v| v.as_str()).any(|m| MFA_AMR.contains(&m)));
    Ok(Identity {
        email,
        name: c["name"].as_str().unwrap_or("").chars().take(80).collect(),
        subject: c["sub"].as_str().unwrap_or("").to_string(),
        mfa,
    })
}

// ── The member it signs in ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// A session token for this member id.
    Session { member_id: String, token: String, provisioned: bool },
    NotAMember,
    Offboarded,
}

/// Sign `who` in: find (or, for an allowed domain, provision) the member and
/// mint their session row. Runs inside one write.
pub fn sign_in(conn: &Connection, cfg: &Config, who: &Identity, at: i64) -> rusqlite::Result<SignIn> {
    let row: Option<(String, Option<i64>)> = conn
        .query_row("SELECT id, offboarded_at FROM org_members WHERE lower(email)=lower(?1)", [&who.email], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (member_id, provisioned) = match row {
        Some((_, Some(_))) => return Ok(SignIn::Offboarded),
        Some((id, None)) => (id, false),
        None => {
            let domain = who.email.rsplit('@').next().unwrap_or("");
            if !cfg.jit_domains.iter().any(|d| d == domain) {
                return Ok(SignIn::NotAMember);
            }
            let team: Option<String> = match &cfg.jit_team {
                Some(t) => conn.query_row("SELECT id FROM org_teams WHERE id=?1 OR name=?1 COLLATE NOCASE", [t], |r| r.get(0)).optional()?,
                None => None,
            };
            let id = ulid::Ulid::new().to_string().to_lowercase();
            let name = if who.name.is_empty() { who.email.split('@').next().unwrap_or("").to_string() } else { who.name.clone() };
            // With no team the member starts with nothing: a worker scope that
            // names no worker. The owner gives access in Team settings.
            let (level, scope_name) = if team.is_some() { ("global", "") } else { ("worker", super::scim::UNASSIGNED) };
            conn.execute(
                "INSERT INTO org_members (id,email,name,role,joined_at,scope_level,scope_name,team_id,provisioned_by) \
                 VALUES (?1,?2,?3,'member',?4,?5,?6,?7,'sso')",
                rusqlite::params![id, who.email, name, at, level, scope_name, team.unwrap_or_default()],
            )?;
            (id, true)
        }
    };
    let token = random_token(32).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    conn.execute(
        "INSERT INTO org_invites (token,email,created_at,expires_at,used_at,used_by,via,mfa_at) VALUES (?1,?2,?3,?4,?3,?5,'sso',?6)",
        rusqlite::params![token, who.email, at, at + cfg.session_hours * 3600, member_id, who.mfa.then_some(at)],
    )?;
    Ok(SignIn::Session { member_id, token, provisioned })
}

// ── HTTP ────────────────────────────────────────────────────────────────────

/// Outside auth: the provider's redirect carries no amux credential; the
/// single-use `state` and PKCE are the guard.
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/sso/login", get(login)).route("/sso/callback", get(callback))
}

/// Owner and admin view of the configuration (protected).
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/sso/status", get(status))
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Html(format!(
            "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
             <title>{t} · amux</title><body style='font:15px/1.5 system-ui;max-width:30rem;margin:3rem auto;padding:0 1rem'>\
             <h1 style='font-size:1.3rem'>{t}</h1><p id=msg>{b}</p></body>",
            t = esc(title),
            b = esc(body)
        )),
    )
        .into_response()
}

fn redirect_uri(cfg: &Config, headers: &HeaderMap) -> String {
    if let Some(u) = &cfg.redirect_url {
        return u.clone();
    }
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("https://{host}/sso/callback")
}

#[derive(Deserialize, Default)]
pub struct LoginQuery {
    next: Option<String>,
}

async fn login(headers: HeaderMap, Query(q): Query<LoginQuery>) -> Response {
    let Some(cfg) = config() else {
        return page(StatusCode::NOT_FOUND, "Single sign-on is not set up", "Ask the owner for an invite link.");
    };
    if !issuer_allowed(&cfg.issuer) {
        return page(StatusCode::SERVICE_UNAVAILABLE, "Single sign-on is misconfigured", "The identity provider must be reached over https.");
    }
    let d = match discover(&cfg.issuer).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(verdict = "sso_discovery_failed", error = %e, "the identity provider's configuration could not be read");
            return page(StatusCode::BAD_GATEWAY, "The identity provider did not answer", &e);
        }
    };
    let (state, verifier, nonce) = match (random_token(24), random_token(48), random_token(24)) {
        (Ok(s), Ok(v), Ok(n)) => (s, v, n),
        _ => return page(StatusCode::SERVICE_UNAVAILABLE, "Sign-in is unavailable", "The server's random source failed."),
    };
    let redirect = redirect_uri(&cfg, &headers);
    let url = format!(
        "{}{}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        d.authorization_endpoint,
        if d.authorization_endpoint.contains('?') { '&' } else { '?' },
        enc(&cfg.client_id),
        enc(&redirect),
        enc(&cfg.scopes),
        state,
        nonce,
        pkce_challenge(&verifier)
    );
    put_pending(&state, Pending { verifier, nonce, next: safe_next(q.next.as_deref()), redirect_uri: redirect, at: Instant::now() });
    counters::bump(&counters::STARTED);
    (StatusCode::SEE_OTHER, [(header::LOCATION, url), (header::CACHE_CONTROL, "no-store".to_string())]).into_response()
}

fn enc(s: &str) -> String {
    crate::integrations::email::urlencode(s)
}

#[derive(Deserialize, Default)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn refused(state: &AppState, who: &str, reason: &str, detail: &str) -> Response {
    counters::bump(&counters::REFUSED);
    tracing::warn!(verdict = "sso_signin_refused", who, reason, detail, "a single sign-on was refused");
    let (w, r) = (who.to_string(), reason.to_string());
    if let Err(e) = super::mfa::write_value(state, move |c| {
        super::mfa::record(c, &w, "oidc", "sso_signin", &w, "refused", &r);
        Ok(json!({"ok": false}))
    })
    .await
    {
        tracing::warn!(verdict = "sso_refusal_unrecorded", error = %e, "a refused sign-in could not be written to the ledger");
    }
    let body = match reason {
        "not_a_member" => "This account is not a member of this workspace. Ask the owner to add you.",
        "offboarded" => "This account was removed from this workspace.",
        _ => detail,
    };
    page(StatusCode::FORBIDDEN, "Sign-in refused", body)
}

async fn callback(State(state): State<AppState>, Query(q): Query<CallbackQuery>) -> Response {
    let Some(cfg) = config() else {
        return page(StatusCode::NOT_FOUND, "Single sign-on is not set up", "Ask the owner for an invite link.");
    };
    if let Some(e) = q.error {
        return refused(&state, "unknown", "provider_error", &format!("{e}: {}", q.error_description.unwrap_or_default())).await;
    }
    let Some(p) = q.state.as_deref().and_then(take_pending) else {
        return refused(&state, "unknown", "state_unknown", "This sign-in link expired or was already used. Start again.").await;
    };
    let Some(code) = q.code else {
        return refused(&state, "unknown", "no_code", "The provider sent no code.").await;
    };
    let d = match discover(&cfg.issuer).await {
        Ok(d) => d,
        Err(e) => return page(StatusCode::BAD_GATEWAY, "The identity provider did not answer", &e),
    };
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", p.redirect_uri.clone()),
        ("client_id", cfg.client_id.clone()),
        ("code_verifier", p.verifier.clone()),
    ];
    if !cfg.client_secret.is_empty() {
        form.push(("client_secret", cfg.client_secret.clone()));
    }
    let body = form.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&");
    let tok: Value = match http()
        .post(&d.token_endpoint)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
        Ok(r) => {
            let s = r.status();
            return refused(&state, "unknown", "token_exchange_failed", &format!("The provider refused the code ({s}).")).await;
        }
        Err(e) => return refused(&state, "unknown", "token_exchange_failed", &e.to_string()).await,
    };
    let Some(id_token) = tok["id_token"].as_str() else {
        return refused(&state, "unknown", "no_id_token", "The provider sent no ID token.").await;
    };
    let mut keys = match jwks(&d.jwks_uri, false).await {
        Ok(k) => k,
        Err(e) => return page(StatusCode::BAD_GATEWAY, "The identity provider did not answer", &e),
    };
    // A rotated key: fetch once more before refusing.
    if let Ok(h) = jsonwebtoken::decode_header(id_token) {
        if h.kid.as_deref().is_some_and(|k| keys.find(k).is_none()) {
            if let Ok(k) = jwks(&d.jwks_uri, true).await {
                keys = k;
            }
        }
    }
    let who = match verify_id_token(id_token, &keys, &cfg.issuer, &cfg.client_id, &p.nonce, cfg.allow_unverified_email) {
        Ok(w) => w,
        Err(e) => return refused(&state, "unknown", "id_token_invalid", &e).await,
    };
    let (cfg2, who2) = (cfg.clone(), who.clone());
    let res = super::mfa::write_value(&state, move |c| {
        let at = chrono::Utc::now().timestamp();
        Ok(match sign_in(c, &cfg2, &who2, at)? {
            SignIn::Session { member_id, token, provisioned } => {
                let actor = format!("member:{}", who2.email);
                if provisioned {
                    super::mfa::record(c, "policy:sso_jit", "owner_policy", "member_provisioned", &actor, "allowed", "applied");
                }
                super::mfa::record(c, &actor, "oidc", "sso_signin", &actor, "allowed", if who2.mfa { "applied_mfa" } else { "applied" });
                json!({"ok": true, "member_id": member_id, "token": token, "provisioned": provisioned})
            }
            SignIn::NotAMember => json!({"ok": false, "reason": "not_a_member"}),
            SignIn::Offboarded => json!({"ok": false, "reason": "offboarded"}),
        })
    })
    .await;
    match res {
        Ok(v) if v["ok"] == json!(true) => {
            counters::bump(&counters::SIGNED_IN);
            if who.mfa {
                counters::bump(&counters::MFA_FROM_IDP);
            }
            if v["provisioned"] == json!(true) {
                counters::bump(&counters::PROVISIONED);
            }
            tracing::info!(verdict = "sso_signed_in", member = %who.email, mfa = who.mfa, "a member signed in through the identity provider");
            let cookie = super::org::member_cookie_header(v["token"].as_str().unwrap_or(""), cfg.session_hours * 3600);
            (StatusCode::SEE_OTHER, [(header::LOCATION, p.next), (header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".into())])
                .into_response()
        }
        Ok(v) => {
            let reason = v["reason"].as_str().unwrap_or("refused").to_string();
            refused(&state, &format!("member:{}", who.email), &reason, "").await
        }
        Err(e) => page(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in failed", &e),
    }
}

async fn status(headers: HeaderMap) -> Response {
    let owner = matches!(super::human_proof::proven(&headers), Some(super::human_proof::Proven::Owner));
    if !owner && !super::org::local_member_is_admin(&headers) && !super::org::local_member_is_auditor(&headers) {
        return (StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": "owner_only"}))).into_response();
    }
    Json(report()).into_response()
}

/// `sso` in `/api/contract/counters`.
pub fn report() -> Value {
    use counters::*;
    let cfg = config();
    json!({
        "configured": cfg.is_some(),
        "issuer": cfg.as_ref().map(|c| c.issuer.clone()),
        "client_id": cfg.as_ref().map(|c| c.client_id.clone()),
        "redirect_url": cfg.as_ref().and_then(|c| c.redirect_url.clone()),
        "jit_domains": cfg.as_ref().map(|c| c.jit_domains.clone()).unwrap_or_default(),
        "allow_unverified_email": cfg.as_ref().is_some_and(|c| c.allow_unverified_email),
        "required": required_mode().as_str(),
        "required_env": REQUIRED_ENV,
        "break_glass": "the owner's own credential (bearer or sign-in door) is never sent to SSO",
        "measured": true,
        "n_considered": get(&STARTED),
        "counts": {
            "started": get(&STARTED),
            "signed_in": get(&SIGNED_IN),
            "provisioned": get(&PROVISIONED),
            "refused": get(&REFUSED),
            "mfa_from_provider": get(&MFA_FROM_IDP),
            "required_would_refuse": get(&REQUIRED_WOULD_REFUSE),
            "required_refused": get(&REQUIRED_REFUSED),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    /// A P-256 key made for this test run; nothing is committed.
    fn test_key() -> &'static (Vec<u8>, String, String) {
        static K: OnceLock<(Vec<u8>, String, String)> = OnceLock::new();
        K.get_or_init(|| {
            use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let kp = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
            let pt = kp.public_key().as_ref();
            (pkcs8.as_ref().to_vec(), b64url(&pt[1..33]), b64url(&pt[33..65]))
        })
    }

    fn keys() -> jsonwebtoken::jwk::JwkSet {
        let (_, x, y) = test_key();
        serde_json::from_value(json!({"keys": [{"kty": "EC", "kid": "k1", "alg": "ES256", "use": "sig", "crv": "P-256", "x": x, "y": y}]})).unwrap()
    }

    fn token(claims: Value, alg: jsonwebtoken::Algorithm) -> String {
        let mut h = Header::new(alg);
        h.kid = Some("k1".into());
        encode(&h, &claims, &EncodingKey::from_ec_der(&test_key().0)).unwrap()
    }

    fn claims() -> Value {
        json!({"iss": "https://idp.test", "aud": "amux", "sub": "123", "exp": chrono::Utc::now().timestamp() + 300,
               "nonce": "n1", "email": "A@X.io", "email_verified": true, "name": "A", "amr": ["pwd", "mfa"]})
    }

    #[test]
    fn a_good_id_token_names_the_person_and_their_factor() {
        let who = verify_id_token(&token(claims(), jsonwebtoken::Algorithm::ES256), &keys(), "https://idp.test", "amux", "n1", false).unwrap();
        assert_eq!(who, Identity { email: "a@x.io".into(), name: "A".into(), subject: "123".into(), mfa: true });
    }

    #[test]
    fn every_claim_that_matters_is_checked() {
        let k = keys();
        let check = |c: Value| verify_id_token(&token(c, jsonwebtoken::Algorithm::ES256), &k, "https://idp.test", "amux", "n1", false);
        let with = |k: &str, v: Value| {
            let mut c = claims();
            c[k] = v;
            c
        };
        assert!(check(with("iss", json!("https://evil.test"))).unwrap_err().contains("id token"), "issuer");
        assert!(check(with("aud", json!("someone-else"))).unwrap_err().contains("id token"), "audience");
        assert!(check(with("exp", json!(1000))).unwrap_err().contains("id token"), "expiry");
        assert!(check(with("nonce", json!("other"))).unwrap_err().contains("nonce"), "nonce");
        assert!(check(with("email_verified", json!(false))).unwrap_err().contains("verified"), "verified email");
        assert!(!check(with("amr", json!(["pwd"]))).unwrap().mfa, "a password alone is not a second factor");
        // a tampered payload fails the signature
        let t = token(claims(), jsonwebtoken::Algorithm::ES256);
        let mut parts: Vec<String> = t.split('.').map(str::to_string).collect();
        parts[1] = b64url(serde_json::to_vec(&with("email", json!("boss@x.io"))).unwrap().as_slice());
        assert!(verify_id_token(&parts.join("."), &k, "https://idp.test", "amux", "n1", false).is_err(), "signature");
        // an HMAC token is refused before any key is tried
        let hs = encode(&Header::new(jsonwebtoken::Algorithm::HS256), &claims(), &EncodingKey::from_secret(b"x")).unwrap();
        assert!(verify_id_token(&hs, &k, "https://idp.test", "amux", "n1", false).unwrap_err().contains("only RS256"));
    }

    #[test]
    fn pkce_is_base64url_sha256_of_the_verifier() {
        // Computed independently: base64.urlsafe_b64encode(hashlib.sha256(v).digest()).rstrip('=')
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r7wW1gFWl2_k-I"), "ypy5HYobCFzN7joNvmoz-ItUAiLVWUgG9qMqydWzuUg");
    }

    #[test]
    fn next_stays_on_this_site_and_issuers_are_https() {
        assert_eq!(safe_next(Some("/messages")), "/messages");
        for bad in ["//evil.test", "https://evil.test", "/\\evil.test", "x"] {
            assert_eq!(safe_next(Some(bad)), "/", "{bad}");
        }
        assert!(issuer_allowed("https://accounts.google.com"));
        assert!(issuer_allowed("http://127.0.0.1:9999"));
        assert!(!issuer_allowed("http://idp.example.com"));
        assert!(!issuer_allowed("http://127.0.0.1.evil.test"));
    }

    #[test]
    fn a_state_is_good_once() {
        put_pending("s1", Pending { verifier: "v".into(), nonce: "n".into(), next: "/".into(), redirect_uri: "r".into(), at: Instant::now() });
        assert!(take_pending("s1").is_some());
        assert!(take_pending("s1").is_none());
    }

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    fn cfg(jit: &[&str]) -> Config {
        Config {
            issuer: "https://idp.test".into(),
            client_id: "amux".into(),
            client_secret: String::new(),
            redirect_url: None,
            scopes: "openid email".into(),
            jit_domains: jit.iter().map(|s| s.to_string()).collect(),
            jit_team: None,
            allow_unverified_email: false,
            session_hours: 12,
        }
    }

    fn who(email: &str, mfa: bool) -> Identity {
        Identity { email: email.into(), name: String::new(), subject: "s".into(), mfa }
    }

    #[test]
    fn sign_in_maps_to_an_existing_member_and_never_invents_one() {
        let c = db();
        c.execute("INSERT INTO org_members (id,email,name,role,joined_at) VALUES ('m1','a@x.io','A','member',0)", []).unwrap();
        let SignIn::Session { member_id, token, provisioned } = sign_in(&c, &cfg(&[]), &who("a@x.io", true), 1000).unwrap() else { panic!() };
        assert_eq!((member_id.as_str(), provisioned), ("m1", false));
        let (via, mfa_at, exp, used_by): (String, Option<i64>, i64, String) = c
            .query_row("SELECT via, mfa_at, expires_at, used_by FROM org_invites WHERE token=?1", [&token], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        assert_eq!((via.as_str(), mfa_at, exp, used_by.as_str()), ("sso", Some(1000), 1000 + 12 * 3600, "m1"));
        assert_eq!(sign_in(&c, &cfg(&[]), &who("b@x.io", false), 1000).unwrap(), SignIn::NotAMember);
        let n: i64 = c.query_row("SELECT COUNT(*) FROM org_members", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "no member was made for an unknown person");
    }

    #[test]
    fn jit_provisions_only_allowed_domains_with_no_access_and_offboarded_is_refused() {
        let c = db();
        let SignIn::Session { provisioned, .. } = sign_in(&c, &cfg(&["x.io"]), &who("new@x.io", false), 1).unwrap() else { panic!() };
        assert!(provisioned);
        let (level, name, by): (String, String, String) =
            c.query_row("SELECT scope_level, scope_name, provisioned_by FROM org_members WHERE email='new@x.io'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((level.as_str(), name.as_str(), by.as_str()), ("worker", super::super::scim::UNASSIGNED, "sso"));
        assert_eq!(sign_in(&c, &cfg(&["x.io"]), &who("new@y.io", false), 1).unwrap(), SignIn::NotAMember);
        c.execute("UPDATE org_members SET offboarded_at=5 WHERE email='new@x.io'", []).unwrap();
        assert_eq!(sign_in(&c, &cfg(&["x.io"]), &who("new@x.io", false), 6).unwrap(), SignIn::Offboarded);
    }
}
