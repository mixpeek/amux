//! Connectors (integrations): the provider registry + list/status + the secure
//! credential paste. A connector is NOT a new subsystem — scope, env, MCP and
//! browser are the primitives it composes (docs/design/connectors.md,
//! docs/design/connectors-setup.md). This module owns only what those
//! primitives do not:
//!
//!   1. the PROVIDER REGISTRY ([`REGISTRY`]) — a const table so "add another
//!      connector" is one row plus Ethan supplying its client/key. This is the
//!      trivial-to-add mechanism the design promises; the tab renders whatever
//!      the registry declares, so a new provider needs no new endpoint or UI.
//!   2. `GET /api/connectors` — every provider with its connection STATUS, the
//!      env-var NAMES it needs and whether each is SET (masked, never the
//!      value), the OAuth redirect URI to register, and the human setup note.
//!   3. `POST /api/connectors/{id}/credentials` — Ethan pastes a key/secret in
//!      the tab; it is written to `~/.amux/server.env` (the ONE place credential
//!      VALUES live — never the repo, never the DB, never a log) via
//!      [`crate::api::settings::set_server_env_key`], redacted from logs, and
//!      only ever echoed back masked.
//!   4. the OAUTH BROKER (AMUX-3192, Ethan 2026-08-20: "I only need to do it
//!      once for each account") — `auth` starts a grant (state + PKCE +
//!      pending file), the public `callback` exchanges the code and writes the
//!      per-account token store (`~/.amux/connectors/<family>/<account>.json`,
//!      mirrored into `~/.amux/gmail-tokens/` when the grant covers Gmail),
//!      `token` hands any worker a live bearer (SA delegation or the stored
//!      user grant, refreshed by the broker), and `GET /api/connectors/accounts`
//!      is the consolidated per-account health view with the one reconnect
//!      action per broken account. See the broker section below.
//!
//! SCOPE (global / group / worker) is the existing `connectors` scope
//! capability (scope.rs); the tab drives it through `PUT /api/scope` with
//! `capability=connectors`, so this module never re-implements scoping. That is
//! the whole point of the design: a connector is a scopable capability, and the
//! precedence, write-authorization and audit row come from the scope primitive
//! for free.
//!
//! Single-codebase rule: nothing here branches on cloud vs local. A connector
//! is configured or it is not (env presence), and the gateway injects whatever
//! differs. The redirect URI follows [`crate::config::canonical_port`] so it is
//! correct in the 8824 laptop and the 8822 container without a build flag.

use super::AppState;
use crate::config::{amux_home, canonical_port, parse_env_file};
use crate::integrations::email::{
    base64url_nopad, connected_accounts_in, html_escape, HttpTransport, ReqwestTransport,
    DEFAULT_TOKEN_URI,
};
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Broker context: the HTTP transport (mockable in tests, like
/// [`super::gmail_auth::GmailAuthCtx`]) plus the amux home the token store
/// lives under. Only the broker endpoints (auth/callback/token/accounts) use
/// it; list/credentials/test keep reading the process-global home.
pub struct ConnectorsCtx {
    pub http: Arc<dyn HttpTransport>,
    pub home: PathBuf,
}

fn default_ctx() -> Arc<ConnectorsCtx> {
    Arc::new(ConnectorsCtx { http: Arc::new(ReqwestTransport::new()), home: amux_home() })
}

/// Auth model for a provider. OAuth needs a client id/secret plus a browser
/// grant; ApiKey needs one pasted secret. Not every connector is OAuth —
/// Granola is key-only — and the registry says so per row rather than assuming.
#[derive(Clone, Copy)]
enum Auth {
    ApiKey {
        key_env: &'static str,
    },
    OAuth2 {
        client_id_env: &'static str,
        client_secret_env: &'static str,
        /// Path this family's callback route is mounted at. Slack joins it to
        /// the canonical origin as its redirect URI. Google connectors do NOT:
        /// they hand Google the gmail redirect URI instead (the one already
        /// registered on the shared client — see [`google_redirect_uri`]), and
        /// the gmail callback delegates their states back to this module.
        callback_path: &'static str,
        scopes: &'static str,
    },
}

#[derive(Clone, Copy)]
struct Provider {
    id: &'static str,
    label: &'static str,
    category: &'static str,
    auth: Auth,
    /// One-line note about what Ethan must do out-of-band (a plan tier, admin
    /// consent, an API to enable). Shown in the tab; empty for none.
    setup_note: &'static str,
    /// A docs/console URL the tab links for setup; empty for none.
    docs: &'static str,
    /// A cheap authenticated endpoint the Test button hits to verify the
    /// credential actually WORKS, not merely that it is present (AMUX-3339).
    /// GET with `Authorization: Bearer <cred>`; 2xx means the connection is live.
    test_url: &'static str,
}

/// The registry. Add a connector by adding a row here (plus Ethan supplying its
/// client id/secret or API key). The five in the design plus Slack.
const REGISTRY: &[Provider] = &[
    Provider {
        id: "granola",
        label: "Granola",
        category: "Notes / transcripts",
        auth: Auth::ApiKey { key_env: "GRANOLA_API_KEY" },
        setup_note: "Business or Enterprise plan required to mint a key: Granola desktop -> Settings -> Connectors -> API keys (grn_...). Key-only, no OAuth.",
        docs: "https://public-api.granola.ai",
        test_url: "https://public-api.granola.ai/v1/notes?limit=1",
    },
    Provider {
        id: "google-gmail",
        label: "Gmail",
        category: "Google",
        auth: Auth::OAuth2 {
            client_id_env: "GOOGLE_OAUTH_CLIENT_ID",
            client_secret_env: "GOOGLE_OAUTH_CLIENT_SECRET",
            callback_path: "/api/connectors/google/callback",
            scopes: "https://www.googleapis.com/auth/gmail.modify",
        },
        setup_note: "Enable the Gmail API on the GCP project. All Google connectors share one OAuth client; register the redirect URI below on it.",
        docs: "https://console.cloud.google.com/apis/library/gmail.googleapis.com",
        test_url: "https://gmail.googleapis.com/gmail/v1/users/me/profile",
    },
    Provider {
        id: "google-calendar",
        label: "Google Calendar",
        category: "Google",
        auth: Auth::OAuth2 {
            client_id_env: "GOOGLE_OAUTH_CLIENT_ID",
            client_secret_env: "GOOGLE_OAUTH_CLIENT_SECRET",
            callback_path: "/api/connectors/google/callback",
            scopes: "https://www.googleapis.com/auth/calendar",
        },
        setup_note: "Enable the Google Calendar API on the GCP project (shares the one Google OAuth client).",
        docs: "https://console.cloud.google.com/apis/library/calendar-json.googleapis.com",
        test_url: "https://www.googleapis.com/calendar/v3/users/me/calendarList?maxResults=1",
    },
    Provider {
        id: "google-drive",
        label: "Google Drive",
        category: "Google",
        auth: Auth::OAuth2 {
            client_id_env: "GOOGLE_OAUTH_CLIENT_ID",
            client_secret_env: "GOOGLE_OAUTH_CLIENT_SECRET",
            callback_path: "/api/connectors/google/callback",
            // Drive + Docs: the DWD service account is granted both, and editing a
            // Doc (the common follow-on to creating a Drive file) needs the Docs
            // scope. A minted token from /token therefore works for both APIs.
            scopes: "https://www.googleapis.com/auth/drive https://www.googleapis.com/auth/documents",
        },
        setup_note: "Enable the Google Drive + Docs APIs on the GCP project (shares the one Google OAuth client).",
        docs: "https://console.cloud.google.com/apis/library/drive.googleapis.com",
        test_url: "https://www.googleapis.com/drive/v3/about?fields=user",
    },
    Provider {
        id: "google-admin",
        label: "Google Admin",
        category: "Google",
        auth: Auth::OAuth2 {
            client_id_env: "GOOGLE_OAUTH_CLIENT_ID",
            client_secret_env: "GOOGLE_OAUTH_CLIENT_SECRET",
            callback_path: "/api/connectors/google/callback",
            scopes: "https://www.googleapis.com/auth/admin.directory.user.readonly https://www.googleapis.com/auth/admin.directory.group.readonly",
        },
        setup_note: "Needs a Workspace super-admin. Enable the Admin SDK API and consent the admin.directory scopes as the super-admin, or use a service account with domain-wide delegation.",
        docs: "https://console.cloud.google.com/apis/library/admin.googleapis.com",
        test_url: "https://admin.googleapis.com/admin/directory/v1/users?maxResults=1&customer=my_customer",
    },
    Provider {
        id: "slack",
        label: "Slack",
        category: "Chat",
        auth: Auth::OAuth2 {
            client_id_env: "SLACK_CLIENT_ID",
            client_secret_env: "SLACK_CLIENT_SECRET",
            callback_path: "/api/connectors/slack/callback",
            scopes: "channels:read chat:write channels:history",
        },
        setup_note: "Create a Slack app, add the redirect URI below, and paste its client id + secret.",
        docs: "https://api.slack.com/apps",
        test_url: "https://slack.com/api/auth.test",
    },
];

fn provider(id: &str) -> Option<&'static Provider> {
    REGISTRY.iter().find(|p| p.id == id)
}

/// The env-var NAMES a provider needs, in paste order. OAuth needs two (client
/// id + secret); ApiKey needs one.
fn env_keys(p: &Provider) -> Vec<&'static str> {
    match p.auth {
        Auth::ApiKey { key_env } => vec![key_env],
        Auth::OAuth2 {
            client_id_env,
            client_secret_env,
            ..
        } => vec![client_id_env, client_secret_env],
    }
}

/// This server's origin for redirect URIs, canonical-port so it is right in
/// both the laptop (8824) and the container (8822) with no build flag.
fn origin() -> String {
    format!("https://localhost:{}", canonical_port())
}

/// The redirect URI every google-family grant uses: the GMAIL one, on purpose
/// (AMUX-3427). It is the only URI proven registered on the shared OAuth
/// client — every working Gmail token was minted through it — so reusing it
/// deletes the "register a second URI in the console" step. The first live
/// Reconnect dead-ended on exactly that step: Google answered
/// `Error 400: redirect_uri_mismatch` BEFORE redirecting, a wall amux can
/// never observe from its own callback. The gmail callback recognises
/// connectors-minted states and delegates back to [`delegate_gmail_callback`].
/// `GMAIL_REDIRECT_URI` in server.env overrides both flows with one knob.
fn google_redirect_uri() -> String {
    super::gmail_auth::gmail_redirect_uri()
}

/// The Workspace user an SA-minted token should impersonate. In CLOUD the
/// gateway authenticates the user and injects `X-Amux-User-Email` (and strips any
/// client-supplied copy), so binding to it means a caller can only mint a token
/// for THEMSELVES, never the whole domain — the primis/nissan caution. LOCALLY
/// the header is absent, so we fall back to the single configured subject
/// (`GOOGLE_SA_SUBJECT`), which is the box owner. Single codebase: no cloud
/// branch, just presence/absence of the gateway-injected header.
///
/// SECURITY CONTRACT: this trusts `X-Amux-User-Email` as authenticated identity.
/// That trust is only sound because the cloud gateway is the sole writer of
/// `X-Amux-*` and drops inbound copies from clients. If that ever stops holding,
/// this becomes domain-wide impersonation — keep the gateway's header-strip in
/// step with this.
fn impersonation_subject(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get("x-amux-user-email")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some(v.to_string());
    }
    super::google_sa::sa_config().map(|(_, subject)| subject)
}

/// Does a provider have an OAuth token on disk? Tokens live under
/// `~/.amux/connectors/<provider>/` (chmod-600 files the broker writes). Gmail's
/// legacy tokens under `~/.amux/gmail-tokens/` also count the Google connectors
/// as authorized, since they share the client.
fn has_token(p: &Provider) -> bool {
    let home = amux_home();
    let dir = home.join("connectors").join(p.id);
    let any_json = |d: &std::path::Path| -> bool {
        std::fs::read_dir(d)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.path().extension().is_some_and(|x| x == "json"))
            })
            .unwrap_or(false)
    };
    if any_json(&dir) {
        return true;
    }
    // The family store: one Google grant per account covers every Google
    // connector (the broker's union grant), so any google-family token
    // authorizes them all.
    if p.category == "Google"
        && (any_json(&home.join("connectors").join("google"))
            || any_json(&home.join("gmail-tokens")))
    {
        return true;
    }
    false
}

/// Non-empty value of an env key, server.env first (the write target), then the
/// process env — the same precedence config.rs uses. Returns the VALUE, only for
/// presence/masking; never emitted raw.
fn env_val(file_env: &std::collections::BTreeMap<String, String>, key: &str) -> Option<String> {
    file_env
        .get(key)
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var(key).ok().filter(|v| !v.trim().is_empty()))
}

/// Resolve a connector credential VALUE: server.env / process env first (an
/// explicit override), then, for GOOGLE connectors, the shared
/// `gmail-oauth-client.json` that `gmail_auth` already uses — so the four Google
/// connectors REUSE the one OAuth client Ethan already configured, with no secret
/// copied into server.env by hand (AMUX-3341, and the connectors-setup "reuse
/// this one" note). The value is for presence/masking and the server's own OAuth
/// flow only; never emitted raw.
fn resolve_cred_in(
    home: &std::path::Path,
    file_env: &std::collections::BTreeMap<String, String>,
    category: &str,
    key: &str,
) -> Option<String> {
    if let Some(v) = env_val(file_env, key) {
        return Some(v);
    }
    if category == "Google" {
        if let Some((cid, csec)) = crate::api::gmail_auth::google_oauth_client_file(home) {
            return match key {
                "GOOGLE_OAUTH_CLIENT_ID" => Some(cid),
                "GOOGLE_OAUTH_CLIENT_SECRET" => Some(csec),
                _ => None,
            };
        }
    }
    None
}

fn resolve_cred(
    file_env: &std::collections::BTreeMap<String, String>,
    category: &str,
    key: &str,
) -> Option<String> {
    resolve_cred_in(&amux_home(), file_env, category, key)
}

// ---- OAuth broker: family model, pending state, token store ----------------
//
// The broker exists so Ethan authorizes each ACCOUNT once and every worker
// shares the grant (2026-08-20 ask: "implement the scaffolding so that I only
// need to do it once for each account"). Three pieces:
//
//   family    — Google connectors share ONE OAuth client and ONE callback, so
//               they share one grant too: a Google authorization requests the
//               UNION of the non-admin Google scopes plus the Gmail scopes,
//               and its token is stored once per account at
//               `~/.amux/connectors/google/<account>.json`. The same callback
//               also mirrors the token into `~/.amux/gmail-tokens/<account>.json`
//               (the exact legacy shape integrations/email.rs reads) when the
//               grant covers Gmail — so a single approval repairs the email
//               subsystem AND every Google connector AND worker token mints.
//   pending   — `~/.amux/connectors/pending.json`, the gmail-pending shape plus
//               a `family` field: single-use, 1h TTL, survives a restart.
//   token     — `POST /api/connectors/{id}/token` mints from the service
//               account when configured (unchanged), and otherwise (or when
//               `?account=` names a user-granted account) from the stored
//               user grant, refreshing through the broker so no session ever
//               re-authorizes.

/// The grant family a provider belongs to: Google rows share client, callback
/// and (via the union grant) token; everything else is its own family.
fn family_of(p: &Provider) -> &'static str {
    if p.category == "Google" {
        "google"
    } else {
        p.id
    }
}

/// Union of scopes ONE Google grant should carry, so one approval per account
/// covers email + calendar + drive/docs + worker mints. `google-admin` is
/// excluded from the default union (its scopes need a Workspace super-admin
/// and fail on consumer accounts like a personal gmail); a grant STARTED from
/// google-admin adds them.
fn google_union_scopes(requesting: &Provider) -> String {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for p in REGISTRY.iter().filter(|p| p.category == "Google") {
        if p.id == "google-admin" && requesting.id != "google-admin" {
            continue;
        }
        if let Auth::OAuth2 { scopes, .. } = p.auth {
            set.extend(scopes.split_whitespace().map(String::from));
        }
    }
    set.extend(super::gmail_auth::GMAIL_SCOPES.iter().map(|s| s.to_string()));
    set.into_iter().collect::<Vec<_>>().join(" ")
}

fn pending_path(home: &std::path::Path) -> PathBuf {
    home.join("connectors").join("pending.json")
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

const PENDING_TTL_S: f64 = 3600.0;

/// Persist a pending grant (gmail_auth's shape + `family`): prune stale, 0600.
fn pending_save(
    home: &std::path::Path,
    state: &str,
    family: &str,
    account: &str,
    verifier: Option<&str>,
) -> std::io::Result<()> {
    let p = pending_path(home);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut d: Map<String, Value> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let now = now_ts();
    d.retain(|_, v| now - v.get("ts").and_then(Value::as_f64).unwrap_or(0.0) < PENDING_TTL_S);
    d.insert(
        state.to_string(),
        json!({ "family": family, "account": account, "verifier": verifier, "ts": now }),
    );
    std::fs::write(&p, Value::Object(d).to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// SINGLE-USE take (removed even when expired); `(family, account, verifier)`
/// while fresh.
fn pending_take(home: &std::path::Path, state: &str) -> Option<(String, String, Option<String>)> {
    let p = pending_path(home);
    let mut d: Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
    let e = d.remove(state)?;
    let _ = std::fs::write(&p, Value::Object(d).to_string());
    if now_ts() - e.get("ts").and_then(Value::as_f64).unwrap_or(0.0) > PENDING_TTL_S {
        return None;
    }
    Some((
        e.get("family").and_then(Value::as_str)?.to_string(),
        e.get("account").and_then(Value::as_str).unwrap_or("").to_string(),
        e.get("verifier").and_then(Value::as_str).map(String::from),
    ))
}

fn token_urlsafe(nbytes: usize) -> String {
    let mut bytes = vec![0u8; nbytes];
    OsRng.fill_bytes(&mut bytes);
    base64url_nopad(&bytes)
}

/// Family token store path: `~/.amux/connectors/<family>/<account>.json`.
fn store_path(home: &std::path::Path, family: &str, account: &str) -> PathBuf {
    home.join("connectors").join(family).join(format!("{account}.json"))
}

/// Accounts with a stored grant for a family (file stems of the store dir).
fn store_accounts(home: &std::path::Path, family: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(home.join("connectors").join(family))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == "json") {
                        p.file_stem().map(|s| s.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn write_store_file(path: &std::path::Path, body: &Value) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, body.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// GET /api/connectors — the registry with per-provider status. `?worker=` is
/// accepted for parity with the scope explain link but the status here is
/// global (credential presence + token); per-scope enablement is the scope
/// read.
async fn list() -> Response {
    let file_env = parse_env_file(&amux_home().join("server.env"));
    let items: Vec<Value> = REGISTRY
        .iter()
        .map(|p| {
            let keys = env_keys(p);
            let key_status: Vec<Value> = keys
                .iter()
                .map(|k| {
                    let v = resolve_cred(&file_env, p.category, k);
                    json!({
                        "name": k,
                        "set": v.is_some(),
                        "masked": v.as_deref().map(super::settings::mask_secret),
                    })
                })
                .collect();
            let all_creds_set = key_status.iter().all(|k| k["set"].as_bool().unwrap_or(false));
            // Where the credential came from, so the tab can say "reusing the
            // existing Google client" rather than looking un-configured (AMUX-3341).
            let in_server_env = keys.iter().all(|k| env_val(&file_env, k).is_some());
            let cred_source = if p.category == "Google" && super::google_sa::sa_config().is_some() {
                json!("service account (domain-wide delegation)")
            } else if !all_creds_set {
                Value::Null
            } else if in_server_env {
                json!("server.env")
            } else {
                json!("gmail-oauth-client.json (reused)")
            };
            let (kind, oauth) = match p.auth {
                Auth::ApiKey { .. } => ("apikey", Value::Null),
                Auth::OAuth2 {
                    callback_path,
                    scopes,
                    ..
                } => (
                    "oauth2",
                    json!({
                        "redirect_uri": format!("{}{}", origin(), callback_path),
                        "scopes": scopes,
                    }),
                ),
            };
            // A Google connector backed by the service-account domain-wide
            // delegation is usable with no per-user grant (AMUX-3347) — but ONLY
            // if the key file still exists. sa_usable() checks that; reading
            // "connected" off a configured-but-missing key is the dishonest
            // status that turned a moved key into a silent 502 (AMUX-3383).
            let sa_configured = p.category == "Google" && super::google_sa::sa_config().is_some();
            let sa_available = p.category == "Google" && super::google_sa::sa_usable();
            let sa_key_gone = sa_configured && !sa_available;
            // Status ladder, most-blocked first.
            let status = if sa_available {
                "connected"
            } else if !all_creds_set {
                "needs_credentials"
            } else if kind == "oauth2" && !has_token(p) {
                "needs_auth"
            } else {
                "connected"
            };
            // The actionable half of the status. "connected" meant only
            // "credentials exist somewhere" and handed a session nothing — a
            // caller had to know a key path existed (nissan, AMUX-3362). When a
            // token CAN be minted right now — from the SA or from a stored user
            // grant — name the endpoint that returns one, so "connected" comes
            // with the door, not just the label.
            let grant_accounts = store_accounts(&amux_home(), family_of(p));
            // Legacy gmail-tokens serve ONLY the gmail connector: their grant
            // never covered calendar/drive, so claiming those usable off a
            // gmail token would be the "connected but useless" lie again.
            let legacy_gmail = p.id == "google-gmail"
                && !connected_accounts_in(&amux_home()).is_empty();
            let token_endpoint = if sa_available || !grant_accounts.is_empty() || legacy_gmail {
                json!(format!("/api/connectors/{}/token", p.id))
            } else {
                Value::Null
            };
            json!({
                "id": p.id,
                "label": p.label,
                "category": p.category,
                "auth": kind,
                "oauth": oauth,
                "env_keys": key_status,
                "status": status,
                "cred_source": cred_source,
                // usable = a SESSION can obtain a working credential from amux
                // right now (POST token_endpoint). Distinct from "connected",
                // which for an OAuth-broker-pending connector is not yet usable.
                "usable": !token_endpoint.is_null(),
                "token_endpoint": token_endpoint,
                // Name the exact failure when the SA is configured but its key
                // file is gone, so the connectors UI (and a session) sees a
                // config problem instead of concluding the whole provider is down.
                "detail": if sa_key_gone {
                    json!("Service-account key file is configured but missing on disk — re-paste the key or fix GOOGLE_SA_KEY_FILE in server.env")
                } else {
                    Value::Null
                },
                "setup_note": p.setup_note,
                "docs": p.docs,
            })
        })
        .collect();
    Json(json!({
        "connectors": items,
        "origin": origin(),
        "note": "Paste credential VALUES here; they are written to ~/.amux/server.env and never returned. When a connector reports a token_endpoint, POST it to mint a ready-to-use short-lived bearer (no key path to know). Set scope (global/group/worker) via the Scope tab or the per-connector scope control (PUT /api/scope, capability=connectors).",
    }))
    .into_response()
}

/// POST /api/connectors/{id}/credentials — Ethan pastes this provider's
/// key(s)/secret(s). Body: `{ "<ENV_NAME>": "<value>", ... }`, restricted to the
/// env keys THIS provider declares (a paste for one provider can never write an
/// arbitrary env key). Values go to server.env and are redacted from the log and
/// the response.
async fn set_credentials(Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(p) = provider(&id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("unknown connector '{id}'")}))).into_response();
    };
    let allowed = env_keys(p);
    let Some(obj) = body.as_object() else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "body must be a JSON object of {ENV_NAME: value}"}))).into_response();
    };
    let home = amux_home();
    let mut written: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    for (k, v) in obj {
        if !allowed.contains(&k.as_str()) {
            rejected.push(k.clone());
            continue;
        }
        let val = v.as_str().unwrap_or("").trim();
        if val.is_empty() {
            rejected.push(k.clone());
            continue;
        }
        if let Err(e) = super::settings::set_server_env_key(&home, k, val) {
            tracing::warn!("connector_credentials: write failed for {} key {}: {}", id, k, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("write failed for {k}")}))).into_response();
        }
        written.push(k.clone());
    }
    // Redacted audit — names of keys written, NEVER the values (two-fixes rule:
    // grep `connector_credentials` to see who set what, without leaking it).
    tracing::info!(
        "connector_credentials: {} wrote {:?} to server.env (rejected {:?})",
        id,
        written,
        rejected
    );
    if written.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "nothing written",
                "rejected": rejected,
                "expected_keys": allowed,
                "why": "only this connector's declared env keys are accepted, and values must be non-empty",
            })),
        )
            .into_response();
    }
    // server.env is read at startup (setdefault); a fresh paste needs the value
    // in the PROCESS env to take effect without a restart. Mirror it in-process
    // so a just-pasted key works immediately for this run (settings.rs does the
    // same for ANTHROPIC_API_KEY).
    let file_env = parse_env_file(&home.join("server.env"));
    for k in &written {
        if let Some(v) = file_env.get(k) {
            // SAFETY: single-threaded config mutation at request time, same
            // pattern settings.rs uses; value came from our own atomic write.
            std::env::set_var(k, v);
        }
    }
    Json(json!({
        "ok": true,
        "connector": id,
        "written": written,
        "rejected": rejected,
        "note": "stored in ~/.amux/server.env; restart is not required for this run. Values are never returned.",
    }))
    .into_response()
}

/// POST /api/connectors/{id}/auth?account=<email> — begin an OAuth grant.
/// Returns the provider consent URL to open (the tab pops it); the callback
/// completes the exchange and writes the shared token store, after which every
/// worker mints from the ONE grant (`POST /api/connectors/{id}/token`).
///
/// `id` may be a provider (`google-drive`) or the family alias `google` — a
/// Google grant is family-wide either way (union scopes), so there is exactly
/// one authorize-once action per account, not one per provider. Google
/// requires `?account=` (it becomes the `login_hint`, so Ethan's browser picks
/// the right profile, and it names the token file if the userinfo lookup
/// cannot). For ApiKey providers there is nothing to authorize.
async fn begin_auth(
    Extension(ctx): Extension<Arc<ConnectorsCtx>>,
    Path(id): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let p = if id == "google" {
        REGISTRY.iter().find(|p| p.category == "Google")
    } else {
        provider(&id)
    };
    let Some(p) = p else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("unknown connector '{id}'")}))).into_response();
    };
    let account = q.get("account").map(|s| s.trim().to_string()).unwrap_or_default();
    let file_env = parse_env_file(&ctx.home.join("server.env"));
    match p.auth {
        Auth::ApiKey { .. } => Json(json!({
            "ok": true,
            "auth": "apikey",
            "note": "key-only connector: paste the API key, no browser grant needed",
        }))
        .into_response(),
        Auth::OAuth2 {
            client_id_env,
            callback_path,
            scopes,
            ..
        } => {
            let family = family_of(p);
            if family == "google" && account.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "ok": false,
                        "error": "account required for a Google grant",
                        "how": format!("POST /api/connectors/{id}/auth?account=<email> — the account names the grant so every worker can address it, and becomes the login_hint so the right browser profile is picked"),
                    })),
                )
                    .into_response();
            }
            let Some(client_id) = resolve_cred_in(&ctx.home, &file_env, p.category, client_id_env)
            else {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "ok": false,
                        "error": "client credentials not set",
                        "need_env": client_id_env,
                        "how": "paste the OAuth client id + secret first (this connector's credentials form)",
                    })),
                )
                    .into_response();
            };
            // Google reuses the ALREADY-REGISTERED gmail redirect URI (see
            // google_redirect_uri); only Slack gets the connectors path.
            let redirect_uri = if p.category == "Google" {
                google_redirect_uri()
            } else {
                format!("{}{}", origin(), callback_path)
            };
            let state = token_urlsafe(24);
            let (url, verifier) = if p.category == "Google" {
                // One grant covers the whole Google family: union scopes, so
                // Ethan approves ONCE per account (see the broker doc above).
                let scope = google_union_scopes(p);
                let verifier = token_urlsafe(64);
                let challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
                (
                    format!(
                        "https://accounts.google.com/o/oauth2/auth?response_type=code&client_id={}\
                         &redirect_uri={}&scope={}&state={state}&access_type=offline\
                         &include_granted_scopes=true&prompt=consent&login_hint={}\
                         &code_challenge={challenge}&code_challenge_method=S256",
                        urlencode(&client_id),
                        urlencode(&redirect_uri),
                        urlencode(&scope),
                        urlencode(&account),
                    ),
                    Some(verifier),
                )
            } else {
                (
                    format!(
                        "https://slack.com/oauth/v2/authorize?client_id={}&redirect_uri={}&scope={}&state={state}",
                        urlencode(&client_id),
                        urlencode(&redirect_uri),
                        urlencode(scopes),
                    ),
                    None,
                )
            };
            // PROBE the authorize URL before handing it out (AMUX-3427, second
            // strike). Token presence was first used as registration evidence
            // and it LIED: a stored token proves a grant worked with whatever
            // URI was registered when it was MINTED, and this deployment's URI
            // moved ports since (8822→8824) with the console never updated.
            // Google's authorize endpoint renders the mismatch page instantly
            // with no interaction, so probe the EXACT url the user will open.
            // This is the only moment amux can see the failure at all: on a
            // mismatch Google blocks BEFORE redirecting, so a dead grant never
            // reaches our callback or logs.
            let registered: Value = if p.category == "Google" {
                match ctx.http.get(&url, None).await {
                    Ok((_, body)) => {
                        let text =
                            body.as_str().map(str::to_string).unwrap_or_else(|| body.to_string());
                        json!(!text.contains("redirect_uri_mismatch"))
                    }
                    // Probe unreachable: unknown beats a guess in either direction.
                    Err(_) => Value::Null,
                }
            } else {
                Value::Null
            };
            if registered == json!(false) {
                tracing::warn!(
                    "connector_auth: Google REJECTS redirect_uri={} for this client \
                     (redirect_uri_mismatch) — the grant will dead-end at Google's error page. \
                     Register the URI once: https://console.cloud.google.com/apis/credentials/oauthclient/{}",
                    redirect_uri,
                    client_id
                );
            }
            if let Err(e) = pending_save(&ctx.home, &state, family, &account, verifier.as_deref()) {
                // Without the pending entry the callback CANNOT succeed —
                // failing loudly beats handing out a URL that dead-ends.
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("could not persist pending auth state: {e}") })),
                )
                    .into_response();
            }
            tracing::info!(
                "connector_auth: {} grant started for account={} family={} redirect_uri={}",
                p.id,
                account,
                family,
                redirect_uri
            );
            Json(json!({
                "ok": true,
                "auth": "oauth2",
                "authorize_url": url,
                "redirect_uri": redirect_uri,
                // Live-probed against Google (see above): true = the consent
                // page renders; false = Google rejects the URI and the ONE fix
                // is registering it on the client; null = probe unreachable.
                "redirect_uri_registered": registered,
                "fix_if_unregistered": if registered == json!(false) {
                    json!(format!(
                        "add {} under Authorized redirect URIs at https://console.cloud.google.com/apis/credentials/oauthclient/{}",
                        redirect_uri, client_id
                    ))
                } else {
                    Value::Null
                },
                "account": account,
                "family": family,
                "prerequisite": if p.category == "Google" {
                    super::gmail_auth::oauth_prerequisite(&client_id, &redirect_uri)
                } else {
                    json!({
                        "requirement": "The redirect URI below must be added to the Slack app's OAuth & Permissions -> Redirect URLs, or Slack rejects the grant.",
                        "console_url": "https://api.slack.com/apps",
                        "add_redirect_uri": redirect_uri,
                    })
                },
                "note": "open authorize_url in a browser and approve — the callback completes the exchange and stores the grant for every worker. One approval per account.",
            }))
            .into_response()
        }
    }
}

/// Minimal percent-encoding for query values (mirrors gmail_auth's urlencode).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(serde::Deserialize)]
pub struct CallbackParams {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn cb_page(status: StatusCode, body: String) -> Response {
    (status, Html(format!("<html><body>{body}</body></html>"))).into_response()
}

/// GET /api/connectors/{family}/callback — the OAuth broker's code→token
/// exchange (AMUX-3192). Mounted on the PUBLIC router like the Gmail callback:
/// the provider's redirect carries no bearer token, and the single-use
/// server-minted state is the guard.
///
/// On success the grant is written to the family token store
/// (`~/.amux/connectors/<family>/<account>.json`), and — when the grant covers
/// Gmail — mirrored into `~/.amux/gmail-tokens/<account>.json` in the exact
/// legacy shape, so ONE approval also repairs the email subsystem. Every
/// worker then mints from the stored grant; nobody re-authorizes.
async fn callback(
    Extension(ctx): Extension<Arc<ConnectorsCtx>>,
    Path(url_family): Path<String>,
    Query(p): Query<CallbackParams>,
) -> Response {
    let code = p.code.unwrap_or_default().trim().to_string();
    let state = p.state.unwrap_or_default().trim().to_string();
    let error = p.error.unwrap_or_default().trim().to_string();
    let file_env = parse_env_file(&ctx.home.join("server.env"));
    if !error.is_empty() {
        tracing::warn!("connector_oauth_callback: {} answered error={}", url_family, error);
        if error.contains("redirect_uri_mismatch") && url_family == "google" {
            let cid = resolve_cred_in(&ctx.home, &file_env, "Google", "GOOGLE_OAUTH_CLIENT_ID")
                .unwrap_or_default();
            let uri = google_redirect_uri();
            let pre = super::gmail_auth::oauth_prerequisite(&cid, &uri);
            return cb_page(
                StatusCode::BAD_REQUEST,
                format!(
                    "<h2>Sign-in blocked: redirect URI not registered</h2>\
                     <p>Open <a href=\"https://console.cloud.google.com/apis/credentials\">the credentials console</a>, \
                     open OAuth client <code>{}</code> and add <code>{}</code> under Authorized redirect URIs, then retry.</p>",
                    html_escape(pre["client_id"].as_str().unwrap_or("")),
                    html_escape(&uri)
                ),
            );
        }
        return cb_page(
            StatusCode::BAD_REQUEST,
            format!("<h2>Auth failed: {}</h2><p>Close this tab.</p>", html_escape(&error)),
        );
    }
    let entry = if state.is_empty() { None } else { pending_take(&ctx.home, &state) };
    let Some((family, hint_account, verifier)) = entry.filter(|_| !code.is_empty()) else {
        return cb_page(
            StatusCode::BAD_REQUEST,
            "<h2>Invalid or expired auth request.</h2><p>Close this tab and re-run the connect action.</p>".into(),
        );
    };
    if family != url_family {
        // A state minted for one family arriving on another's callback is a
        // misconfigured redirect, not a user error — say which is which.
        return cb_page(
            StatusCode::BAD_REQUEST,
            format!(
                "<h2>Family mismatch</h2><p>This grant was started for <b>{}</b> but the redirect landed on the <b>{}</b> callback. Check the registered redirect URI.</p>",
                html_escape(&family),
                html_escape(&url_family)
            ),
        );
    }
    complete_exchange(&ctx, family, code, hint_account, verifier).await
}

/// Called by the gmail callback when a state it does not own arrives: if the
/// state is a connectors-minted grant (the google family hands Google the
/// gmail redirect URI precisely because it is the registered one — see
/// [`google_redirect_uri`]), complete it here. Returns None when the state is
/// unknown to the connectors store too, so the gmail callback renders its own
/// invalid/expired page. Single-use order matters: the caller must miss in
/// ITS pending store before this take.
pub(crate) async fn delegate_gmail_callback(
    http: Arc<dyn HttpTransport>,
    home: &std::path::Path,
    state: &str,
    code: &str,
) -> Option<Response> {
    let (family, hint_account, verifier) = pending_take(home, state)?;
    if family != "google" {
        // A slack state can only land here via a mis-registered redirect —
        // name it rather than exchanging against the wrong token endpoint.
        return Some(cb_page(
            StatusCode::BAD_REQUEST,
            format!(
                "<h2>Family mismatch</h2><p>This grant was started for <b>{}</b> but the \
                 redirect landed on the gmail callback. Check the registered redirect URI.</p>",
                html_escape(&family)
            ),
        ));
    }
    let ctx = ConnectorsCtx { http, home: home.to_path_buf() };
    Some(complete_exchange(&ctx, family, code.to_string(), hint_account, verifier).await)
}

/// The code→token exchange + store write shared by BOTH entry points: the
/// connectors callback above, and the gmail callback delegating a
/// connectors-minted state that arrived on the gmail redirect URI.
async fn complete_exchange(
    ctx: &ConnectorsCtx,
    family: String,
    code: String,
    hint_account: String,
    verifier: Option<String>,
) -> Response {
    let file_env = parse_env_file(&ctx.home.join("server.env"));
    let category = if family == "google" { "Google" } else { "Chat" };
    let (id_key, secret_key) = if family == "google" {
        ("GOOGLE_OAUTH_CLIENT_ID", "GOOGLE_OAUTH_CLIENT_SECRET")
    } else {
        ("SLACK_CLIENT_ID", "SLACK_CLIENT_SECRET")
    };
    let (Some(client_id), Some(client_secret)) = (
        resolve_cred_in(&ctx.home, &file_env, category, id_key),
        resolve_cred_in(&ctx.home, &file_env, category, secret_key),
    ) else {
        return cb_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("<h2>Token exchange failed</h2><pre>client credentials for {family} are no longer set</pre>"),
        );
    };
    // Must match the auth request byte-for-byte: google grants were issued
    // with the gmail redirect URI (the registered one), slack with its own.
    let redirect_uri = if family == "google" {
        google_redirect_uri()
    } else {
        format!("{}/api/connectors/{}/callback", origin(), family)
    };
    let token_uri = if family == "google" {
        DEFAULT_TOKEN_URI.to_string()
    } else {
        "https://slack.com/api/oauth.v2.access".to_string()
    };
    let mut form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code),
        ("client_id".to_string(), client_id.clone()),
        ("client_secret".to_string(), client_secret.clone()),
        ("redirect_uri".to_string(), redirect_uri),
    ];
    if let Some(v) = verifier {
        form.push(("code_verifier".to_string(), v));
    }
    let (status, body) = match ctx.http.post_form(&token_uri, &form).await {
        Ok(pair) => pair,
        Err(e) => {
            return cb_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("<h2>Token exchange failed</h2><pre>{}</pre>", html_escape(&e)),
            )
        }
    };
    let access = body.get("access_token").and_then(Value::as_str).unwrap_or("");
    // Slack answers HTTP 200 with {"ok": false} on failure — status alone lies.
    let slack_not_ok = family != "google" && body.get("ok").is_some_and(|v| v == false);
    if status >= 400 || access.is_empty() || slack_not_ok {
        tracing::warn!(
            "connector_oauth_callback: {} exchange failed (HTTP {status}, error={})",
            family,
            body.get("error").and_then(|v| v.as_str()).unwrap_or("?")
        );
        return cb_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "<h2>Token exchange failed</h2><pre>{}</pre>",
                html_escape(&format!("HTTP {status}: {body}"))
            ),
        );
    }
    // Resolve the ACCOUNT the grant is actually for. Google: ask userinfo (the
    // union grant carries userinfo.email), so approving with a different
    // browser profile than the login_hint cannot mis-file the token. Slack:
    // the workspace name. Fall back to the hint from pending.
    let account = if family == "google" {
        match ctx.http.get("https://www.googleapis.com/oauth2/v2/userinfo", Some(access)).await {
            Ok((st, v)) if st < 400 => v
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| hint_account.clone()),
            _ => hint_account.clone(),
        }
    } else {
        body.get("team")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| if hint_account.is_empty() { "workspace".into() } else { hint_account.clone() })
    };
    let granted_scopes = body.get("scope").and_then(Value::as_str).unwrap_or("").to_string();
    let expires_at = body
        .get("expires_in")
        .and_then(Value::as_f64)
        .map(|s| now_ts() + s);
    let refresh = body.get("refresh_token").cloned().unwrap_or(Value::Null);
    let store = json!({
        "token": access,
        "refresh_token": refresh,
        "token_uri": if family == "google" { DEFAULT_TOKEN_URI } else { &token_uri },
        "client_id": client_id,
        "client_secret": client_secret,
        "scopes": granted_scopes,
        "expires_at": expires_at,
    });
    if let Err(e) = write_store_file(&store_path(&ctx.home, &family, &account), &store) {
        return cb_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("<h2>Token exchange failed</h2><pre>{}</pre>", html_escape(&e.to_string())),
        );
    }
    // Gmail mirror: when the grant covers Gmail, write the legacy token file
    // in the EXACT shape integrations/email.rs::load_token_file reads — one
    // approval repairs the email API too. Scope-gated so a grant that did not
    // include Gmail never fakes a working mailbox.
    let mut mirrored = false;
    if family == "google" && granted_scopes.contains("auth/gmail.") {
        let legacy = json!({
            "token": access,
            "refresh_token": store["refresh_token"],
            "token_uri": DEFAULT_TOKEN_URI,
            "client_id": store["client_id"],
            "client_secret": store["client_secret"],
        });
        let dir = ctx.home.join("gmail-tokens");
        mirrored = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(format!("{account}.json")), legacy.to_string()))
            .is_ok();
    }
    tracing::info!(
        "connector_oauth_callback: {} grant stored for {} (gmail_mirror={}, scopes=[{}])",
        family,
        account,
        mirrored,
        granted_scopes
    );
    cb_page(
        StatusCode::OK,
        format!(
            "<h2>✓ {} connected</h2><p>The grant is stored once and shared by every worker{}. You can close this tab.</p>",
            html_escape(&account),
            if mirrored { " (email API included)" } else { "" }
        ),
    )
}

/// POST /api/connectors/{id}/test — verify the connection actually WORKS, not
/// merely that a credential is present (AMUX-3339). Makes one cheap authenticated
/// call to the provider and reports the real outcome. For ApiKey connectors this
/// runs today; for OAuth ones it reports "connect first" until the token exchange
/// (broker) lands, since there is no stored access token to present yet. The
/// bearer value is NEVER logged — only the provider id, HTTP status and latency
/// (grep `connector_test`).
async fn test_connection(Path(id): Path<String>) -> Response {
    let Some(p) = provider(&id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("unknown connector '{id}'")}))).into_response();
    };
    let file_env = parse_env_file(&amux_home().join("server.env"));
    let bearer = match p.auth {
        Auth::ApiKey { key_env } => match env_val(&file_env, key_env) {
            Some(k) => k,
            None => {
                return Json(json!({
                    "ok": false,
                    "status": "needs_credentials",
                    "detail": format!("set {key_env} first — nothing to test yet"),
                }))
                .into_response()
            }
        },
        Auth::OAuth2 { scopes, .. } => {
            // Mixpeek Google connectors mint an impersonated token via the
            // service-account domain-wide delegation (AMUX-3347) — no per-user
            // browser grant. If no SA is configured, fall back to the honest
            // "connect first" until the OAuth broker (AMUX-3192) lands.
            if p.category == "Google" && super::google_sa::sa_config().is_some() {
                match super::google_sa::mint_token(scopes).await {
                    Ok(tok) => tok,
                    Err(e) => {
                        tracing::warn!("connector_test: {} SA delegation failed: {}", id, e);
                        return Json(json!({
                            "ok": false,
                            "status": "needs_auth",
                            "detail": format!("service-account delegation failed: {e}. If this is 'unauthorized_client', a Workspace super-admin must authorize this connector's scope for the SA in Admin console -> Security -> API controls -> Domain-wide delegation."),
                        }))
                        .into_response();
                    }
                }
            } else {
                return Json(json!({
                    "ok": false,
                    "status": "needs_auth",
                    "detail": "Connect first — configure a service account (GOOGLE_SA_KEY_FILE) or wait for the OAuth token exchange (AMUX-3192).",
                }))
                .into_response();
            }
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Json(json!({"ok": false, "status": "error", "detail": format!("client build failed: {e}")}))
                .into_response()
        }
    };
    let started = std::time::Instant::now();
    let resp = client
        .get(p.test_url)
        .header("Authorization", format!("Bearer {bearer}"))
        .send()
        .await;
    let ms = started.elapsed().as_millis();
    match resp {
        Ok(r) => {
            let code = r.status().as_u16();
            let ok = r.status().is_success();
            tracing::info!("connector_test: {id} -> {code} ({ok}) in {ms}ms");
            Json(json!({
                "ok": ok,
                "status": if ok { "connected" } else { "error" },
                "http_status": code,
                "elapsed_ms": ms,
                "detail": if ok {
                    format!("live: HTTP {code} in {ms}ms")
                } else {
                    format!("provider returned HTTP {code} — the key may be invalid or lack scope")
                },
            }))
            .into_response()
        }
        Err(e) => {
            let msg = if e.is_timeout() {
                "timed out after 10s".to_string()
            } else if e.is_connect() {
                "could not reach the provider".to_string()
            } else {
                format!("request failed: {e}")
            };
            tracing::warn!("connector_test: {id} failed ({msg}) in {ms}ms");
            Json(json!({"ok": false, "status": "error", "detail": msg, "elapsed_ms": ms})).into_response()
        }
    }
}

/// POST /api/connectors/{id}/token — hand the caller a ready-to-use bearer for a
/// Google connector, so a SESSION never has to know a key path exists (nissan,
/// AMUX-3362). The token is minted through the service-account domain-wide
/// delegation, impersonating the requesting Workspace user (see
/// [`impersonation_subject`]). The raw SA key is NEVER returned or logged, and
/// the minted token is NEVER logged. Optional `?scopes=` overrides the provider's
/// Did Google REFUSE us, as opposed to something breaking on the way there?
///
/// `mint_token` returns two families of error and they want opposite statuses.
/// When the token endpoint answers non-2xx it relays Google's own OAuth error
/// code (`format!("{err}: {desc}")`), and those codes are a CLOSED SET defined
/// by RFC 6749 section 5.2 — not a fuzzy phrase, which is what makes this a
/// safe match rather than a guess. Everything else is a local or network fault:
/// missing config, unreadable key file, bad PEM, sign failure, transport error,
/// a body with no access_token.
///
/// A refusal is 403 + `needs_auth`: Google worked, our SA is not authorized, and
/// the body already names who can grant it. A fault stays 502, because that IS
/// amux failing and must not hide behind a refusal (AMUX-3738).
pub(crate) fn delegation_refusal(err: &str) -> bool {
    const OAUTH_REFUSALS: [&str; 5] = [
        "unauthorized_client",
        "invalid_client",
        "invalid_grant",
        "access_denied",
        "invalid_scope",
    ];
    let code = err.split(':').next().unwrap_or("").trim();
    OAUTH_REFUSALS.contains(&code)
}

/// declared scopes (space- or comma-delimited). On failure the endpoint surfaces
/// Google's own error (e.g. `unauthorized_client`), which names the Admin-console
/// fix — the same honest-error path `test_connection` uses.
async fn mint_connector_token(
    Extension(ctx): Extension<Arc<ConnectorsCtx>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(p) = provider(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown connector '{id}'")})),
        )
            .into_response();
    };
    let default_scopes = match p.auth {
        Auth::OAuth2 { scopes, .. } => scopes,
        Auth::ApiKey { .. } => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "status": "unsupported",
                    "detail": format!("{id} is an API-key connector — there is no token to mint; the server uses its key directly (Test button)."),
                })),
            )
                .into_response();
        }
    };
    let scope = q
        .get("scopes")
        .map(|s| s.replace(',', " "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_scopes.to_string());
    let family = family_of(p);
    let req_account = q.get("account").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let stored = store_accounts(&ctx.home, family);
    // Route the mint. An explicit `?account=` that has a stored USER grant wins
    // (the SA cannot impersonate outside its Workspace domain — a personal
    // gmail account is only reachable through its own grant). Otherwise the SA
    // path is unchanged. Otherwise a single stored grant is unambiguous.
    let user_account: Option<String> = match &req_account {
        Some(a) if stored.contains(a) => Some(a.clone()),
        // Legacy gmail-tokens serve ONLY the gmail connector: their grant never
        // covered calendar/drive, so minting them for another provider would
        // hand out a token that cannot do what the caller asked (the
        // "connected but useless" lie the list() endpoint already refuses).
        Some(a) if p.id == "google-gmail"
            && ctx.home.join("gmail-tokens").join(format!("{a}.json")).exists() =>
        {
            Some(a.clone())
        }
        Some(a) => {
            // Named an account nobody holds a grant for: if the SA can
            // impersonate it, fall through to the SA path with it as subject;
            // otherwise the honest answer names how to connect it.
            if !(p.category == "Google" && super::google_sa::sa_usable()) {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "ok": false,
                        "status": "needs_auth",
                        "detail": format!("no stored grant for account '{a}' (family {family})"),
                        "stored_accounts": stored,
                        "connect": format!("POST /api/connectors/{family}/auth?account={a} → open authorize_url, approve once; every worker shares the grant"),
                    })),
                )
                    .into_response();
            }
            None
        }
        None => None,
    };
    if user_account.is_none() && !(p.category == "Google" && super::google_sa::sa_config().is_some())
    {
        // No SA: fall back to the user-grant store. One stored account is
        // unambiguous; several need `?account=`; none is an honest "connect
        // first" naming the exact action.
        let fallback = match stored.len() {
            1 => Some(stored[0].clone()),
            0 if p.id == "google-gmail" => {
                // Legacy gmail-tokens can still serve google-gmail mints (and
                // only gmail — see the routing note above).
                let gm = connected_accounts_in(&ctx.home);
                if gm.len() == 1 { Some(gm[0].clone()) } else { None }
            }
            _ => None,
        };
        let Some(acct) = fallback else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "status": "needs_auth",
                    "detail": if stored.is_empty() {
                        format!("no grant stored for family {family} and no service account configured")
                    } else {
                        format!("several accounts hold grants for family {family} — name one with ?account=")
                    },
                    "stored_accounts": stored,
                    "connect": format!("POST /api/connectors/{family}/auth?account=<email> → open authorize_url, approve once"),
                })),
            )
                .into_response();
        };
        return mint_from_user_grant(&ctx, p, family, &acct, &scope).await;
    }
    if let Some(acct) = user_account {
        return mint_from_user_grant(&ctx, p, family, &acct, &scope).await;
    }
    let Some(subject) = impersonation_subject(&headers) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "status": "error", "detail": "no impersonation subject (GOOGLE_SA_SUBJECT unset and no X-Amux-User-Email)"})),
        )
            .into_response();
    };
    let subject = req_account.unwrap_or(subject);
    match super::google_sa::mint_token_as(&scope, &subject).await {
        Ok(tok) => {
            // Log the FACT of a mint (id, subject, scope, lifetime) so a sweep can
            // see who minted what — but NEVER the token itself (grep connector_token).
            tracing::info!(
                "connector_token: {id} minted for {subject} scope=[{scope}] expires_in={}",
                tok.expires_in
            );
            Json(json!({
                "ok": true,
                "access_token": tok.access_token,
                "token_type": "Bearer",
                "expires_in": tok.expires_in,
                "scope": scope,
                "subject": subject,
                "usage": "Authorization: Bearer <access_token> — e.g. GET https://www.googleapis.com/drive/v3/files",
            }))
            .into_response()
        }
        Err(e) => {
            // A REFUSAL IS NOT A GATEWAY FAULT (AMUX-3738). Google's token
            // endpoint answering `unauthorized_client` means it worked
            // correctly and said no: our service account is not authorized for
            // these scopes, and a named human (a Workspace super-admin) must
            // grant it. Nothing in amux is broken, and 502 sent a reader here
            // instead of to the Admin console.
            //
            // The sibling path already knew this. `connector_test` returns
            // `status: "needs_auth"` for the IDENTICAL failure while this one
            // returned `status: "error"` at 502 — two verdicts about one fact,
            // and the wrong one was the one that files autofix cards.
            //
            // A genuine fault (unreadable key file, bad PEM, network) keeps
            // 502: those ARE amux failing and must not hide behind a refusal.
            let refusal = delegation_refusal(&e);
            if refusal {
                tracing::warn!("connector_token: {id} delegation REFUSED for {subject}: {e}");
            } else {
                tracing::warn!("connector_token: {id} mint failed for {subject}: {e}");
            }
            (
                if refusal { StatusCode::FORBIDDEN } else { StatusCode::BAD_GATEWAY },
                Json(json!({
                    "ok": false,
                    "status": if refusal { "needs_auth" } else { "error" },
                    "detail": format!("service-account delegation failed: {e}. If this is 'unauthorized_client', a Workspace super-admin must authorize this connector's scopes for the SA in Admin console -> Security -> API controls -> Domain-wide delegation."),
                })),
            )
                .into_response()
        }
    }
}

/// Mint a bearer from a stored USER grant, refreshing through the broker when
/// stale — the path that makes "authorize once per account" real for workers.
/// Response shape matches the SA mint. `invalid_grant` names the ONE reconnect
/// action instead of a bare 502, and WARNs so a log sweep sees the rot.
async fn mint_from_user_grant(
    ctx: &ConnectorsCtx,
    p: &Provider,
    family: &str,
    account: &str,
    scope: &str,
) -> Response {
    let fam_path = store_path(&ctx.home, family, account);
    let legacy_path = ctx.home.join("gmail-tokens").join(format!("{account}.json"));
    let (path, legacy) = if fam_path.exists() {
        (fam_path, false)
    } else if family == "google" && legacy_path.exists() {
        (legacy_path, true)
    } else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "status": "needs_auth",
                "detail": format!("no stored grant for {account} (family {family})"),
                "connect": format!("POST /api/connectors/{family}/auth?account={account}"),
            })),
        )
            .into_response();
    };
    let Some(tf) = std::fs::read_to_string(&path).ok().and_then(|r| serde_json::from_str::<Value>(&r).ok())
    else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "status": "error", "detail": format!("grant file for {account} is unreadable")})),
        )
            .into_response();
    };
    let s = |k: &str| tf.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    // Fresh stored token (60s safety margin): serve it without a round trip.
    // Legacy gmail files carry no expires_at, so they always refresh — which
    // is also what keeps their stored `token` field usable for email.rs.
    let expires_at = tf.get("expires_at").and_then(Value::as_f64);
    if let Some(exp) = expires_at {
        let left = exp - now_ts();
        if left > 60.0 && !s("token").is_empty() {
            return Json(json!({
                "ok": true,
                "access_token": s("token"),
                "token_type": "Bearer",
                "expires_in": left as u64,
                "scope": tf.get("scopes").and_then(Value::as_str).unwrap_or(scope),
                "subject": account,
                "source": "user-grant (stored)",
                "usage": "Authorization: Bearer <access_token>",
            }))
            .into_response();
        }
    }
    let refresh = s("refresh_token");
    if refresh.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "status": "needs_reauth",
                "detail": format!("the stored grant for {account} has no refresh token — re-approve once and every worker is repaired"),
                "reconnect": format!("POST /api/connectors/{family}/auth?account={account}"),
            })),
        )
            .into_response();
    }
    let token_uri = {
        let t = s("token_uri");
        if t.is_empty() { DEFAULT_TOKEN_URI.to_string() } else { t }
    };
    let form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh.clone()),
        ("client_id".to_string(), s("client_id")),
        ("client_secret".to_string(), s("client_secret")),
    ];
    match ctx.http.post_form(&token_uri, &form).await {
        Ok((st, body)) if st < 400 => {
            let access = body.get("access_token").and_then(Value::as_str).unwrap_or("");
            if access.is_empty() {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"ok": false, "status": "error", "detail": "refresh answered without an access token"})),
                )
                    .into_response();
            }
            let expires_in = body.get("expires_in").and_then(Value::as_f64).unwrap_or(3599.0);
            // Persist so the NEXT mint (any worker) skips the round trip; the
            // legacy gmail shape keeps its exact five fields.
            let updated = if legacy {
                json!({
                    "token": access,
                    "refresh_token": refresh,
                    "token_uri": token_uri,
                    "client_id": s("client_id"),
                    "client_secret": s("client_secret"),
                })
            } else {
                let mut m = tf.as_object().cloned().unwrap_or_default();
                m.insert("token".into(), json!(access));
                m.insert("expires_at".into(), json!(now_ts() + expires_in));
                Value::Object(m)
            };
            let _ = write_store_file(&path, &updated);
            tracing::info!(
                "connector_token: {} minted from user grant for {} (family {family}) expires_in={}",
                p.id,
                account,
                expires_in as u64
            );
            Json(json!({
                "ok": true,
                "access_token": access,
                "token_type": "Bearer",
                "expires_in": expires_in as u64,
                "scope": tf.get("scopes").and_then(Value::as_str).unwrap_or(scope),
                "subject": account,
                "source": "user-grant (refreshed)",
                "usage": "Authorization: Bearer <access_token>",
            }))
            .into_response()
        }
        Ok((_, body)) if body.to_string().contains("invalid_grant") => {
            tracing::warn!(
                "connector_token: {} user grant for {account} is REVOKED (invalid_grant) — needs one re-approval",
                p.id
            );
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "status": "needs_reauth",
                    "detail": format!("the grant for {account} was revoked or expired (invalid_grant). One re-approval repairs every worker."),
                    "reconnect": format!("POST /api/connectors/{family}/auth?account={account} → open authorize_url, approve"),
                })),
            )
                .into_response()
        }
        Ok((st, body)) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok": false, "status": "error", "detail": format!("token refresh failed: HTTP {st}: {body}")})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok": false, "status": "error", "detail": format!("token refresh failed: {e}")})),
        )
            .into_response(),
    }
}

// ---- GET /api/connectors/accounts — the consolidated per-ACCOUNT view ------

/// Rollup cache: the probes behind this (gmail getProfile, Google refresh) are
/// real provider round trips, and BOTH the endpoint and the autofix detector
/// read the rollup — one probe per account per 300s, however many readers.
/// Keyed by HOME so two homes can never serve each other's rollup — in prod
/// there is one home, but tests run many temp homes in one process, and an
/// unkeyed cache let one test's fixture answer another test's tick.
static ROLLUP_CACHE: Mutex<std::collections::BTreeMap<PathBuf, (Instant, Value)>> =
    Mutex::new(std::collections::BTreeMap::new());
const ROLLUP_TTL_S: u64 = 300;

/// Canary targets: the cheapest real call per Google service, gated on the
/// grant actually covering it (a gmail-only grant must read `not_granted` for
/// calendar, never red — the inverse of the "connected but useless" lie).
/// A refreshable token proves only the TOKEN leg; these prove the API serves.
const CANARY_TARGETS: &[(&str, &str, &str)] = &[
    ("gmail", "https://gmail.googleapis.com/gmail/v1/users/me/profile", "auth/gmail."),
    (
        "calendar",
        "https://www.googleapis.com/calendar/v3/users/me/calendarList?maxResults=1",
        "auth/calendar",
    ),
    ("drive", "https://www.googleapis.com/drive/v3/about?fields=user", "auth/drive"),
];

/// Build the per-account rollup: every account amux holds any credential for,
/// which families cover it, each one's LIVE health, and — when broken — the
/// ONE reconnect action. This is the view Ethan's "do it once per account"
/// resolves through: each row is one approval at most.
pub(crate) async fn accounts_rollup(http: &Arc<dyn HttpTransport>, home: &std::path::Path, bypass_cache: bool) -> Value {
    if !bypass_cache {
        if let Some((at, v)) = ROLLUP_CACHE.lock().expect("rollup cache").get(home).cloned() {
            if at.elapsed().as_secs() < ROLLUP_TTL_S {
                return v;
            }
        }
    }
    let mut rows: std::collections::BTreeMap<String, Map<String, Value>> = Default::default();
    // Per-account canary legs (AMUX-3430): the CONTINUES-working proof, one
    // cheap real API call per granted service on every cache refresh.
    let mut canaries: std::collections::BTreeMap<String, Map<String, Value>> = Default::default();
    let legacy_gmail: std::collections::BTreeSet<String> =
        connected_accounts_in(home).into_iter().collect();
    // Gmail (legacy store) — health via the SAME probe /api/gmail/accounts uses.
    // That probe is a real getProfile round trip, so it doubles as the gmail
    // canary leg for these accounts (mirrored below, never pinged twice).
    for a in &legacy_gmail {
        let health = super::gmail_auth::health_for(http.clone(), home, a).await;
        rows.entry(a.clone()).or_default().insert("gmail".into(), json!(health));
        canaries.entry(a.clone()).or_default().insert(
            "gmail".into(),
            json!({"status": health, "via": "gmail_probe", "checked_at": now_ts()}),
        );
    }
    // Family stores. Google health: fresh expires_at is ok on its face; stale
    // exercises the refresh token — the needs_reauth discriminator.
    for family in ["google", "slack"] {
        for a in store_accounts(home, family) {
            let path = store_path(home, family, &a);
            let tf: Value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|r| serde_json::from_str(&r).ok())
                .unwrap_or(json!({}));
            let health = if family != "google" {
                // Slack bot tokens do not expire; presence says nothing about
                // whether the workspace still honors it — the canary below is
                // the live check.
                "ok".to_string()
            } else if tf.get("expires_at").and_then(Value::as_f64).is_some_and(|e| e - now_ts() > 60.0) {
                "ok".to_string()
            } else {
                probe_google_refresh(http, &path, &tf).await
            };
            rows.entry(a.clone()).or_default().insert(family.into(), json!(health));
            let legs = canaries.entry(a.clone()).or_default();
            if family == "google" {
                canary_google_legs(http, &path, &health, legacy_gmail.contains(&a), legs).await;
            } else if health == "ok" {
                let token = tf.get("token").and_then(Value::as_str).unwrap_or("");
                legs.insert("slack".into(), canary_slack_leg(http, token).await);
            }
        }
    }
    let mut accounts: Vec<Value> = Vec::new();
    let mut needs_reauth: Vec<Value> = Vec::new();
    for (account, families) in rows {
        let broken: Vec<String> = families
            .iter()
            .filter(|(_, v)| v.as_str() == Some("needs_reauth"))
            .map(|(k, _)| k.clone())
            .collect();
        let reconnect_family = if broken.iter().any(|f| f == "gmail" || f == "google") {
            Some("google")
        } else if broken.first().map(String::as_str) == Some("slack") {
            Some("slack")
        } else {
            None
        };
        let reconnect = reconnect_family.map(|f| {
            format!("POST /api/connectors/{f}/auth?account={account} → open authorize_url, approve once")
        });
        if let Some(rc) = &reconnect {
            needs_reauth.push(json!({"account": account, "broken": broken, "reconnect": rc}));
        }
        accounts.push(json!({
            "account": account,
            "families": families,
            "canary": canaries.get(&account).cloned().map(Value::Object),
            "needs_reauth": reconnect.is_some(),
            "reconnect": reconnect,
        }));
    }
    // Persist the canary map with a timestamp: last-checked must survive the
    // frequent self-adopt restarts, or silence after a restart reads as
    // health (in-memory state is fiction — ethos D1).
    let canary_file = json!({
        "checked_at": now_ts(),
        "accounts": canaries.iter().map(|(k, v)| (k.clone(), Value::Object(v.clone()))).collect::<Map<_, _>>(),
    });
    let _ = std::fs::create_dir_all(home.join("connectors")).and_then(|_| {
        std::fs::write(
            home.join("connectors").join("canary.json"),
            serde_json::to_string_pretty(&canary_file).unwrap_or_default(),
        )
    });
    let out = json!({
        "accounts": accounts,
        "needs_reauth": needs_reauth,
        "note": "One row per account; one approval repairs every family listed as needs_reauth (a Google grant covers gmail + calendar + drive/docs at once and every worker shares it). canary = one real API call per granted service on every refresh (~5min): ok / api_error / not_granted / unreachable, with checked_at.",
    });
    ROLLUP_CACHE
        .lock()
        .expect("rollup cache")
        .insert(home.to_path_buf(), (Instant::now(), out.clone()));
    out
}

/// Exercise a stored Google grant's refresh token; persist a successful
/// refresh so the next reader skips the round trip. Same discriminator as the
/// gmail probe: `invalid_grant` → needs_reauth, anything else unusable →
/// not_connected.
async fn probe_google_refresh(http: &Arc<dyn HttpTransport>, path: &std::path::Path, tf: &Value) -> String {
    let s = |k: &str| tf.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let refresh = s("refresh_token");
    if refresh.is_empty() {
        return "needs_reauth".into();
    }
    let token_uri = {
        let t = s("token_uri");
        if t.is_empty() { DEFAULT_TOKEN_URI.to_string() } else { t }
    };
    let form = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh),
        ("client_id".to_string(), s("client_id")),
        ("client_secret".to_string(), s("client_secret")),
    ];
    match http.post_form(&token_uri, &form).await {
        Ok((st, body)) if st < 400 => {
            let access = body.get("access_token").and_then(Value::as_str).unwrap_or("");
            if access.is_empty() {
                return "not_connected".into();
            }
            let expires_in = body.get("expires_in").and_then(Value::as_f64).unwrap_or(3599.0);
            let mut m = tf.as_object().cloned().unwrap_or_default();
            m.insert("token".into(), json!(access));
            m.insert("expires_at".into(), json!(now_ts() + expires_in));
            let _ = write_store_file(path, &Value::Object(m));
            "ok".into()
        }
        Ok((_, body)) if body.to_string().contains("invalid_grant") => "needs_reauth".into(),
        _ => "not_connected".into(),
    }
}

/// Ping every service the Google grant covers with one cheap real call
/// (AMUX-3430). Distinctions that matter downstream: `api_error` (the provider
/// ANSWERED with a failure — the canary finding) vs `unreachable` (transport
/// error — network trouble is not connector rot, never a card) vs
/// `not_granted` (scope-gated, neutral). When the account also has a legacy
/// gmail token, the gmail leg was already proven by health_for's real
/// getProfile round trip and is mirrored, not pinged twice.
async fn canary_google_legs(
    http: &Arc<dyn HttpTransport>,
    store: &std::path::Path,
    health: &str,
    gmail_already_probed: bool,
    legs: &mut Map<String, Value>,
) {
    if health != "ok" {
        // No usable token: every covered leg inherits the grant's state so
        // the canary never claims silence where the grant itself is broken.
        legs.insert("grant".into(), json!({"status": health, "checked_at": now_ts()}));
        return;
    }
    // Re-read: probe_google_refresh may have just rotated the token in place.
    let tf: Value = std::fs::read_to_string(store)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or(json!({}));
    let token = tf.get("token").and_then(Value::as_str).unwrap_or("");
    let scopes = tf.get("scopes").and_then(Value::as_str).unwrap_or("");
    for (svc, url, marker) in CANARY_TARGETS {
        if !scopes.contains(marker) {
            legs.insert((*svc).into(), json!({"status": "not_granted"}));
            continue;
        }
        if *svc == "gmail" && gmail_already_probed {
            continue;
        }
        let leg = match http.get(url, Some(token)).await {
            Ok((st, _)) if st < 400 => json!({"status": "ok", "http": st, "checked_at": now_ts()}),
            Ok((st, body)) => json!({
                "status": "api_error",
                "http": st,
                "detail": body.to_string().chars().take(200).collect::<String>(),
                "checked_at": now_ts(),
            }),
            Err(e) => json!({"status": "unreachable", "detail": e, "checked_at": now_ts()}),
        };
        legs.insert((*svc).into(), leg);
    }
}

/// Slack canary: auth.test is the cheapest authenticated call and answers
/// `{"ok":false}` with HTTP 200 on a dead token, so the body is the verdict.
async fn canary_slack_leg(http: &Arc<dyn HttpTransport>, token: &str) -> Value {
    match http.get("https://slack.com/api/auth.test", Some(token)).await {
        Ok((st, body)) if st < 400 && body.get("ok").is_some_and(|v| v == true) => {
            json!({"status": "ok", "checked_at": now_ts()})
        }
        Ok((st, body)) => json!({
            "status": "api_error",
            "http": st,
            "detail": body.get("error").and_then(Value::as_str).unwrap_or("").to_string(),
            "checked_at": now_ts(),
        }),
        Err(e) => json!({"status": "unreachable", "detail": e, "checked_at": now_ts()}),
    }
}

async fn accounts_view(Extension(ctx): Extension<Arc<ConnectorsCtx>>) -> Response {
    Json(accounts_rollup(&ctx.http, &ctx.home, false).await).into_response()
}

pub fn routes() -> Router<AppState> {
    routes_with(default_ctx())
}

pub fn routes_with(ctx: Arc<ConnectorsCtx>) -> Router<AppState> {
    Router::new()
        .route("/api/connectors", get(list))
        .route("/api/connectors/accounts", get(accounts_view))
        .route("/api/connectors/{id}/credentials", post(set_credentials))
        .route("/api/connectors/{id}/auth", post(begin_auth))
        .route("/api/connectors/{id}/test", post(test_connection))
        .route("/api/connectors/{id}/token", post(mint_connector_token))
        .layer(Extension(ctx))
}

/// The PUBLIC callback (provider redirects carry no bearer; the single-use
/// server-minted state is the guard — same mount rationale as gmail_auth).
pub fn callback_routes() -> Router<AppState> {
    callback_routes_with(default_ctx())
}

pub fn callback_routes_with(ctx: Arc<ConnectorsCtx>) -> Router<AppState> {
    Router::new()
        .route("/api/connectors/{family}/callback", get(callback))
        .layer(Extension(ctx))
}

#[cfg(test)]
mod tests {
    /// AMUX-3738: a delegation REFUSAL is 403 + needs_auth, a real fault stays
    /// 502.
    ///
    /// The filed card was `POST /api/connectors/{id}/token -> HTTP 502` with a
    /// body saying "unauthorized_client ... a Workspace super-admin must
    /// authorize this connector's scopes". Google worked correctly and said no.
    /// Nothing in amux was broken, and 502 sent a reader here instead of to the
    /// Admin console.
    ///
    /// The sibling path already knew: `connector_test` returns
    /// `status: "needs_auth"` for the IDENTICAL failure while the token path
    /// said `status: "error"` at 502. Two verdicts about one fact, and the
    /// wrong one was the one that files autofix cards.
    ///
    /// The refusal set is RFC 6749 section 5.2 — a closed set, which is what
    /// makes this a safe match rather than a phrase guess.
    #[test]
    fn a_delegation_refusal_is_not_a_gateway_fault() {
        // Exactly the shape mint_token relays: "{err}: {desc}".
        for e in [
            "unauthorized_client: Client is unauthorized to retrieve access tokens",
            "invalid_client: The OAuth client was not found.",
            "invalid_grant: Invalid grant: account not found",
            "access_denied: ",
            "invalid_scope: bad scope",
        ] {
            assert!(delegation_refusal(e), "Google refusing us is not amux failing: {e}");
        }

        // CONTROLS: every OTHER mint_token failure is a genuine fault and must
        // KEEP 502. Without these the classifier could return true always, the
        // assertions above would pass, and a real outage would hide behind a
        // needs_auth label — which is the expensive direction here, because
        // nobody investigates a permissions notice.
        for e in [
            "GOOGLE_SA_KEY_FILE / GOOGLE_SA_SUBJECT not set",
            "read SA key file: No such file or directory (os error 2)",
            "parse SA key JSON: expected value at line 1",
            "SA private key is not valid RSA PEM: bad base64",
            "sign JWT: InvalidKeyFormat",
            "http client: builder error",
            "token exchange request: connection refused",
            "token exchange body: expected value",
            "token exchange returned no access_token",
        ] {
            assert!(!delegation_refusal(e), "a real fault must keep 502: {e}");
        }

        // A refusal-looking word INSIDE a description must not flip a fault.
        // The code is the FIRST field; matching anywhere would make
        // "token exchange body: unauthorized_client" a refusal when the body
        // failed to parse.
        assert!(
            !delegation_refusal("token exchange body: unauthorized_client somewhere"),
            "only the leading OAuth code counts, not the description text"
        );
    }

    use super::*;

    #[test]
    fn registry_ids_are_unique_and_have_env_keys() {
        let mut seen = std::collections::HashSet::new();
        for p in REGISTRY {
            assert!(seen.insert(p.id), "duplicate connector id {}", p.id);
            assert!(!env_keys(p).is_empty(), "{} declares no env keys", p.id);
        }
    }

    #[test]
    fn oauth_providers_expose_a_canonical_port_redirect() {
        // The redirect must follow canonical_port, never a hardcoded 8822 (the
        // exact bug that dead-ended the Gmail flow). Assert the port matches.
        let want = format!(":{}", canonical_port());
        for p in REGISTRY {
            if let Auth::OAuth2 { callback_path, .. } = p.auth {
                let uri = format!("{}{}", origin(), callback_path);
                assert!(uri.contains(&want), "{} redirect {} lost canonical port", p.id, uri);
                assert!(!uri.contains(":8822") || want == ":8822", "{} pins retired 8822", p.id);
            }
        }
    }

    #[test]
    fn impersonation_subject_prefers_the_gateway_header_and_ignores_blank() {
        // Cloud path: the gateway-injected user is impersonated verbatim, so a
        // minted token is bound to the requester, not the whole domain.
        let mut h = HeaderMap::new();
        h.insert("x-amux-user-email", "alice@mixpeek.com".parse().unwrap());
        assert_eq!(
            impersonation_subject(&h).as_deref(),
            Some("alice@mixpeek.com")
        );
        // A blank header must never become the subject — it falls through to the
        // configured subject (or None), never impersonates "   ".
        let mut blank = HeaderMap::new();
        blank.insert("x-amux-user-email", "   ".parse().unwrap());
        assert_ne!(impersonation_subject(&blank).as_deref(), Some("   "));
    }

    #[test]
    fn drive_connector_grants_drive_and_docs_so_a_minted_token_can_edit_a_doc() {
        // A Drive token that could not touch the Docs API would repeat the exact
        // "connected but useless" gap this endpoint exists to close: creating a
        // Doc is the common follow-on and needs the documents scope.
        let drive = REGISTRY.iter().find(|p| p.id == "google-drive").unwrap();
        match drive.auth {
            Auth::OAuth2 { scopes, .. } => {
                assert!(scopes.contains("auth/drive"), "drive scope missing");
                assert!(
                    scopes.contains("auth/documents"),
                    "docs scope missing — a minted Drive token could not edit a Doc"
                );
            }
            _ => panic!("google-drive should be OAuth2"),
        }
    }

    #[test]
    fn google_connectors_share_one_oauth_client() {
        // The design's premise: one client, many APIs. If these ever diverge,
        // the setup checklist (one redirect, one consent) silently breaks.
        let google: Vec<_> = REGISTRY.iter().filter(|p| p.category == "Google").collect();
        assert!(google.len() >= 4);
        for p in &google {
            if let Auth::OAuth2 { client_id_env, callback_path, .. } = p.auth {
                assert_eq!(client_id_env, "GOOGLE_OAUTH_CLIENT_ID", "{} uses a different client", p.id);
                assert_eq!(callback_path, "/api/connectors/google/callback");
            }
        }
    }

    // ---- OAuth broker tests: mocked transport + temp homes only. ----------
    // Placeholder strings, never real credentials; every token write lands in
    // a tempdir.

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    type RecordedCall = (String, String, Option<String>, Option<Value>);

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

    fn home_with_google_client() -> tempfile::TempDir {
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

    fn app_with(http: Arc<MockHttp>, home: &std::path::Path) -> (axum::Router, tempfile::TempDir) {
        let ctx = Arc::new(ConnectorsCtx { http, home: home.to_path_buf() });
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("c.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
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
    async fn google_auth_carries_union_scopes_pkce_and_pending_state() {
        let home = home_with_google_client();
        // The registration probe hits the REAL authorize URL: first grant sees
        // Google's mismatch page, second sees the consent page — the response
        // must discriminate (AMUX-3427: token-presence as evidence lied).
        let (app, _d) = app_with(
            MockHttp::new(vec![
                (
                    "GET",
                    "accounts.google.com/o/oauth2/auth",
                    200,
                    json!("<html>Error 400: redirect_uri_mismatch</html>"),
                ),
                (
                    "GET",
                    "accounts.google.com/o/oauth2/auth",
                    200,
                    json!("<html>Sign in with Google</html>"),
                ),
            ]),
            home.path(),
        );

        // Family alias, account required.
        let (st, v) = send_json(&app, "POST", "/api/connectors/google/auth").await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        let (st, v) =
            send_json(&app, "POST", "/api/connectors/google/auth?account=hello%40amux.io").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let url = v["authorize_url"].as_str().unwrap();

        // ONE grant covers the family: gmail + calendar + drive/docs scopes in
        // one consent — this is the "authorize once per account" property.
        let scope = qparam(url, "scope").unwrap();
        for want in ["gmail.send", "gmail.modify", "auth%2Fcalendar", "auth%2Fdrive", "auth%2Fdocuments", "userinfo.email"] {
            assert!(scope.contains(want), "union scope missing {want}: {scope}");
        }
        // Admin scopes stay out of the default union (they fail on consumer
        // accounts); google-admin starts its own wider grant.
        assert!(!scope.contains("admin.directory"), "{scope}");
        assert_eq!(qparam(url, "access_type"), Some("offline"));
        assert_eq!(qparam(url, "code_challenge_method"), Some("S256"));
        assert_eq!(qparam(url, "login_hint"), Some(urlencode(ACCT).as_str()));
        // The redirect URI is the GMAIL one — the only URI already registered
        // on the shared client. Issuing /api/connectors/google/callback here
        // is the regression that dead-ended the first live Reconnect at
        // Google's own `Error 400: redirect_uri_mismatch` wall (AMUX-3427).
        assert!(
            v["redirect_uri"].as_str().unwrap().ends_with("/api/gmail/callback"),
            "google grant must reuse the registered gmail redirect URI: {}",
            v["redirect_uri"]
        );
        // The probe saw Google's mismatch page: the response must say so AND
        // name the one fix, instead of letting the grant dead-end silently at
        // a wall amux cannot observe.
        assert_eq!(v["redirect_uri_registered"], json!(false));
        assert!(
            v["fix_if_unregistered"].as_str().unwrap().contains("console.cloud.google.com"),
            "{v}"
        );

        // Pending state persisted (family + verifier), challenge derived S256.
        let state = qparam(url, "state").unwrap();
        let pending: Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join("connectors").join("pending.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(pending[state]["family"], json!("google"));
        assert_eq!(pending[state]["account"], json!(ACCT));
        let verifier = pending[state]["verifier"].as_str().unwrap();
        let want_challenge = base64url_nopad(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(qparam(url, "code_challenge"), Some(want_challenge.as_str()));

        // A provider id (not the alias) starts the SAME family-wide grant.
        let (st, v2) =
            send_json(&app, "POST", "/api/connectors/google-drive/auth?account=hello%40amux.io")
                .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v2["family"], json!("google"));
        // Second probe rendered the consent page: registered, no fix needed.
        assert_eq!(v2["redirect_uri_registered"], json!(true));
        assert_eq!(v2["fix_if_unregistered"], Value::Null);
    }

    #[tokio::test]
    async fn callback_exchanges_writes_family_store_and_gmail_mirror_once() {
        let home = home_with_google_client();
        let http = MockHttp::new(vec![
            (
                "FORM",
                "oauth2.googleapis.com/token",
                200,
                json!({ "access_token": "PLACEHOLDER_AT", "refresh_token": "PLACEHOLDER_RT",
                         "expires_in": 3599,
                         "scope": "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/gmail.send https://www.googleapis.com/auth/drive" }),
            ),
            // The account is RESOLVED from the grant, not trusted from the hint.
            ("GET", "oauth2/v2/userinfo", 200, json!({ "email": ACCT })),
        ]);
        let (app, _d) = app_with(http.clone(), home.path());

        let (_, v) =
            send_json(&app, "POST", "/api/connectors/google/auth?account=hint%40other.io").await;
        let state = qparam(v["authorize_url"].as_str().unwrap(), "state").unwrap().to_string();
        let (st, page) = send(
            &app,
            "GET",
            &format!("/api/connectors/google/callback?code=authcode123&state={state}"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{page}");
        assert!(page.contains("connected"), "{page}");
        assert!(page.contains("email API included"), "{page}");

        // The exchange carried the PKCE verifier and the SAME redirect URI the
        // auth request was issued with — the registered gmail one (AMUX-3427).
        // (calls() also records the begin_auth registration probe, so select
        // the token exchange by method rather than position.)
        let form =
            http.calls().into_iter().find(|c| c.0 == "FORM").expect("token exchange").3.unwrap();
        assert_eq!(form["grant_type"], json!("authorization_code"));
        assert!(form["redirect_uri"].as_str().unwrap().ends_with("/api/gmail/callback"));
        assert!(form["code_verifier"].as_str().is_some());

        // Family store: one file per ACCOUNT (named by userinfo, not the hint).
        let fam: Value = serde_json::from_str(
            &std::fs::read_to_string(
                home.path().join("connectors").join("google").join(format!("{ACCT}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(fam["token"], json!("PLACEHOLDER_AT"));
        assert_eq!(fam["refresh_token"], json!("PLACEHOLDER_RT"));
        assert!(fam["expires_at"].as_f64().unwrap() > now_ts());
        assert!(!home.path().join("connectors").join("google").join("hint@other.io.json").exists());

        // Gmail mirror: EXACT legacy shape (integrations/email.rs reads this),
        // so the one approval repaired the email subsystem too.
        let legacy: Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join("gmail-tokens").join(format!("{ACCT}.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            legacy,
            json!({
                "token": "PLACEHOLDER_AT",
                "refresh_token": "PLACEHOLDER_RT",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "PLACEHOLDER_ID.apps.googleusercontent.com",
                "client_secret": "PLACEHOLDER_SECRET",
            })
        );

        // Single-use state: a replay cannot mint a second grant.
        let (st, page) = send(
            &app,
            "GET",
            &format!("/api/connectors/google/callback?code=authcode123&state={state}"),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(page.contains("Invalid or expired"), "{page}");
    }

    #[tokio::test]
    async fn gmail_callback_completes_a_connectors_state_it_does_not_own() {
        // The SHIPPED flow (AMUX-3427): a google-family grant hands Google the
        // gmail redirect URI (the registered one), so the code+state land on
        // /api/gmail/callback — which must recognise the connectors state and
        // complete the family exchange, not render "Invalid or expired".
        let home = home_with_google_client();
        let http = MockHttp::new(vec![
            (
                "FORM",
                "oauth2.googleapis.com/token",
                200,
                json!({ "access_token": "PLACEHOLDER_AT", "refresh_token": "PLACEHOLDER_RT",
                         "expires_in": 3599,
                         "scope": "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/drive" }),
            ),
            ("GET", "oauth2/v2/userinfo", 200, json!({ "email": ACCT })),
        ]);
        let (app, _d) = app_with(http.clone(), home.path());
        let (_, v) =
            send_json(&app, "POST", "/api/connectors/google/auth?account=hint%40other.io").await;
        let state = qparam(v["authorize_url"].as_str().unwrap(), "state").unwrap().to_string();

        // Google redirects to the GMAIL callback, not the connectors one.
        let gctx = Arc::new(crate::api::gmail_auth::GmailAuthCtx::new(
            http.clone(),
            home.path().to_path_buf(),
        ));
        let dir2 = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir2.path().join("c.db")).unwrap();
        let gstate = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let gapp = Router::new()
            .merge(crate::api::gmail_auth::callback_routes_with(gctx))
            .with_state(gstate);
        let (st, page) =
            send(&gapp, "GET", &format!("/api/gmail/callback?code=authcode123&state={state}")).await;
        assert_eq!(st, StatusCode::OK, "{page}");
        assert!(page.contains("connected"), "{page}");

        // The delegated exchange wrote the family store AND the gmail mirror.
        assert!(home
            .path()
            .join("connectors")
            .join("google")
            .join(format!("{ACCT}.json"))
            .exists());
        assert!(home.path().join("gmail-tokens").join(format!("{ACCT}.json")).exists());
    }

    #[tokio::test]
    async fn token_serves_fresh_grant_without_a_round_trip_and_refreshes_stale() {
        let home = tempfile::tempdir().unwrap();
        let fam_dir = home.path().join("connectors").join("google");
        std::fs::create_dir_all(&fam_dir).unwrap();
        // Fresh token: served straight from the store — zero provider calls.
        std::fs::write(
            fam_dir.join(format!("{ACCT}.json")),
            json!({
                "token": "PLACEHOLDER_FRESH", "refresh_token": "PLACEHOLDER_RT",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "CID", "client_secret": "CSEC",
                "scopes": "https://www.googleapis.com/auth/drive",
                "expires_at": now_ts() + 3000.0,
            })
            .to_string(),
        )
        .unwrap();
        let http = MockHttp::new(vec![]);
        let (app, _d) = app_with(http.clone(), home.path());
        let (st, v) = send_json(
            &app,
            "POST",
            "/api/connectors/google-drive/token?account=hello%40amux.io",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["access_token"], json!("PLACEHOLDER_FRESH"));
        assert_eq!(v["subject"], json!(ACCT));
        assert!(http.calls().is_empty(), "fresh grant must not hit the provider");

        // Stale token: refreshed through the broker and PERSISTED, so the next
        // worker's mint is free.
        std::fs::write(
            fam_dir.join(format!("{ACCT}.json")),
            json!({
                "token": "PLACEHOLDER_OLD", "refresh_token": "PLACEHOLDER_RT",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "CID", "client_secret": "CSEC",
                "scopes": "https://www.googleapis.com/auth/drive",
                "expires_at": now_ts() - 10.0,
            })
            .to_string(),
        )
        .unwrap();
        let http2 = MockHttp::new(vec![(
            "FORM",
            "oauth2.googleapis.com/token",
            200,
            json!({ "access_token": "PLACEHOLDER_NEW", "expires_in": 3599 }),
        )]);
        let (app2, _d2) = app_with(http2.clone(), home.path());
        let (st, v) = send_json(
            &app2,
            "POST",
            "/api/connectors/google-drive/token?account=hello%40amux.io",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["access_token"], json!("PLACEHOLDER_NEW"));
        let persisted: Value = serde_json::from_str(
            &std::fs::read_to_string(fam_dir.join(format!("{ACCT}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted["token"], json!("PLACEHOLDER_NEW"));
        assert!(persisted["expires_at"].as_f64().unwrap() > now_ts() + 3000.0);
    }

    #[tokio::test]
    async fn revoked_grant_names_the_one_reconnect_action() {
        let home = tempfile::tempdir().unwrap();
        let fam_dir = home.path().join("connectors").join("google");
        std::fs::create_dir_all(&fam_dir).unwrap();
        std::fs::write(
            fam_dir.join(format!("{ACCT}.json")),
            json!({
                "token": "", "refresh_token": "PLACEHOLDER_RT",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_id": "CID", "client_secret": "CSEC",
                "expires_at": now_ts() - 10.0,
            })
            .to_string(),
        )
        .unwrap();
        let http = MockHttp::new(vec![(
            "FORM",
            "oauth2.googleapis.com/token",
            400,
            json!({ "error": "invalid_grant" }),
        )]);
        let (app, _d) = app_with(http, home.path());
        let (st, v) = send_json(
            &app,
            "POST",
            "/api/connectors/google-drive/token?account=hello%40amux.io",
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["status"], json!("needs_reauth"));
        assert!(
            v["reconnect"].as_str().unwrap().contains("/api/connectors/google/auth?account=hello@amux.io"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn a_gmail_only_legacy_token_never_serves_a_calendar_or_drive_mint() {
        // The grant behind ~/.amux/gmail-tokens/ never covered calendar/drive.
        // Serving it for those providers hands out a token that cannot do what
        // the caller asked — the "connected but useless" lie. The honest answer
        // is needs_auth naming the connect action.
        let home = tempfile::tempdir().unwrap();
        let gm = home.path().join("gmail-tokens");
        std::fs::create_dir_all(&gm).unwrap();
        std::fs::write(
            gm.join(format!("{ACCT}.json")),
            json!({ "token": "PLACEHOLDER_GMAIL_ONLY", "refresh_token": "PLACEHOLDER_RT",
                     "token_uri": "https://oauth2.googleapis.com/token",
                     "client_id": "CID", "client_secret": "CSEC" })
            .to_string(),
        )
        .unwrap();
        let http = MockHttp::new(vec![]);
        let (app, _d) = app_with(http.clone(), home.path());
        for provider in ["google-calendar", "google-drive"] {
            let (st, v) = send_json(
                &app,
                "POST",
                &format!("/api/connectors/{provider}/token?account=hello%40amux.io"),
            )
            .await;
            assert_ne!(st, StatusCode::OK, "{provider} must not mint from a gmail-only grant: {v}");
            assert_eq!(v["status"], json!("needs_auth"), "{v}");
            assert!(v["connect"].as_str().unwrap().contains("/api/connectors/google/auth"), "{v}");
        }
        // gmail itself still mints from the legacy file (refresh path).
        let http2 = MockHttp::new(vec![(
            "FORM",
            "oauth2.googleapis.com/token",
            200,
            json!({ "access_token": "PLACEHOLDER_NEW", "expires_in": 3599 }),
        )]);
        let (app2, _d2) = app_with(http2, home.path());
        let (st, v) = send_json(
            &app2,
            "POST",
            "/api/connectors/google-gmail/token?account=hello%40amux.io",
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["access_token"], json!("PLACEHOLDER_NEW"));
    }

    #[tokio::test]
    async fn accounts_rollup_flags_needs_reauth_with_one_reconnect_per_account() {
        let home = tempfile::tempdir().unwrap();
        // A gmail-only account whose refresh answers invalid_grant (the
        // esteininger21 shape), and a healthy family-store account.
        let gm = home.path().join("gmail-tokens");
        std::fs::create_dir_all(&gm).unwrap();
        std::fs::write(
            gm.join("broken@x.io.json"),
            json!({ "token": "PLACEHOLDER_DEAD", "refresh_token": "PLACEHOLDER_RT",
                     "token_uri": "https://oauth2.googleapis.com/token",
                     "client_id": "CID", "client_secret": "CSEC" })
            .to_string(),
        )
        .unwrap();
        let fam = home.path().join("connectors").join("google");
        std::fs::create_dir_all(&fam).unwrap();
        std::fs::write(
            fam.join("healthy@x.io.json"),
            json!({ "token": "PLACEHOLDER_OK", "refresh_token": "PLACEHOLDER_RT2",
                     "token_uri": "https://oauth2.googleapis.com/token",
                     "client_id": "CID", "client_secret": "CSEC",
                     "expires_at": now_ts() + 3000.0 })
            .to_string(),
        )
        .unwrap();
        let http: Arc<dyn HttpTransport> = MockHttp::new(vec![
            ("GET", "/profile", 401, json!({ "error": "unauthorized" })),
            ("FORM", "oauth2.googleapis.com/token", 400, json!({ "error": "invalid_grant" })),
        ]);
        let v = accounts_rollup(&http, home.path(), true).await;
        let accounts = v["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2, "{v}");
        let broken =
            accounts.iter().find(|a| a["account"] == json!("broken@x.io")).expect("broken row");
        assert_eq!(broken["families"]["gmail"], json!("needs_reauth"), "{v}");
        assert!(broken["reconnect"]
            .as_str()
            .unwrap()
            .contains("/api/connectors/google/auth?account=broken@x.io"));
        let healthy =
            accounts.iter().find(|a| a["account"] == json!("healthy@x.io")).expect("healthy row");
        assert_eq!(healthy["families"]["google"], json!("ok"));
        assert_eq!(healthy["needs_reauth"], json!(false));
        assert_eq!(v["needs_reauth"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn canary_pings_granted_services_and_discriminates_the_failure_kinds() {
        // The continues-working proof (AMUX-3430): a refreshable token proves
        // only the TOKEN leg. Here the token is fresh, gmail SERVES, calendar
        // ANSWERS 500 — and drive is simply not granted, which must read
        // neutral, never red.
        let home = tempfile::tempdir().unwrap();
        let fam = home.path().join("connectors").join("google");
        std::fs::create_dir_all(&fam).unwrap();
        std::fs::write(
            fam.join(format!("{ACCT}.json")),
            json!({ "token": "PLACEHOLDER_OK", "refresh_token": "PLACEHOLDER_RT",
                     "token_uri": "https://oauth2.googleapis.com/token",
                     "client_id": "CID", "client_secret": "CSEC",
                     "scopes": "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/calendar",
                     "expires_at": now_ts() + 3000.0 })
            .to_string(),
        )
        .unwrap();
        let http: Arc<dyn HttpTransport> = MockHttp::new(vec![
            ("GET", "gmail/v1/users/me/profile", 200, json!({ "emailAddress": ACCT })),
            ("GET", "calendar/v3/users/me/calendarList", 500, json!({ "error": "backendError" })),
        ]);
        let v = accounts_rollup(&http, home.path(), true).await;
        let row = v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["account"] == json!(ACCT))
            .expect("account row")
            .clone();
        assert_eq!(row["canary"]["gmail"]["status"], json!("ok"), "{v}");
        assert_eq!(row["canary"]["calendar"]["status"], json!("api_error"), "{v}");
        assert_eq!(row["canary"]["calendar"]["http"], json!(500), "{v}");
        assert_eq!(row["canary"]["drive"]["status"], json!("not_granted"), "{v}");
        assert!(row["canary"]["gmail"]["checked_at"].as_f64().unwrap() > 0.0);
        // The grant itself stays green — an API-side failure is a DIFFERENT
        // state from token rot and must not trigger the reconnect flow.
        assert_eq!(row["needs_reauth"], json!(false), "{v}");
        // Last-checked survives restarts: the canary map is persisted.
        let persisted: Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join("connectors").join("canary.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted["accounts"][ACCT]["calendar"]["status"], json!("api_error"));
        assert!(persisted["checked_at"].as_f64().unwrap() > 0.0);
    }
}
