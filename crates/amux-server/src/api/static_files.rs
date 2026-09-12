//! Embedded dashboard serving (RR-0021 + Phase 8 bootstrap injection).
//!
//! Files come from amux-dashboard's `static/` at compile time. index.html
//! gets its AMUX-BOOTSTRAP block substituted at serve time — the same
//! values the Python server injects (amux-server.py:65679). The owner's bearer
//! is injected only for a browser on this machine or a request that explicitly
//! presents it. A remote browser carrying a verified local-member cookie gets
//! the shell WITHOUT that bearer and continues through its revocable cookie.

use super::AppState;
use amux_dashboard::DashboardAssets;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Router};
use sha2::Digest;
use std::net::{IpAddr, SocketAddr};

const OWNER_COOKIE: &str = "__Host-amux_owner";

/// The public iCal URL the dashboard's Subscribe button shows.
///
/// This read `AMUX_S3_ICAL_URL` — A VARIABLE NOTHING SETS. The documented and
/// actually-configured spelling is `AMUX_S3_BUCKET` + `AMUX_S3_KEY`
/// (+ `AMUX_S3_REGION`), which is what CLAUDE.md tells operators to put in
/// server.env and what the feed uploader already uses. So the button rendered
/// an empty string on a machine with the feed fully configured and working, and
/// there was no way to subscribe Apple Calendar from the dashboard at all
/// (AMUX-2772).
///
/// An explicit `AMUX_S3_ICAL_URL` still wins, so an operator who publishes the
/// feed somewhere other than S3 is not overridden. Otherwise it is composed from
/// the vars that exist.
///
/// The composed value is a SECRET-BEARING URL: the key is a random token and the
/// bucket denies listing, so the token IS the access control. It is injected
/// into a localhost, auth-gated page — the same place it has always been shown —
/// and must never be logged, committed, or written to a board card.
fn ical_subscribe_url() -> String {
    if let Ok(u) = std::env::var("AMUX_S3_ICAL_URL") {
        if !u.trim().is_empty() {
            return u.trim().to_string();
        }
    }
    let bucket = std::env::var("AMUX_S3_BUCKET").unwrap_or_default();
    let key = std::env::var("AMUX_S3_KEY").unwrap_or_default();
    if bucket.trim().is_empty() || key.trim().is_empty() {
        return String::new(); // not configured: an honest empty, not a broken URL
    }
    let region = std::env::var("AMUX_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    format!(
        "https://{}.s3.{}.amazonaws.com/{}",
        bucket.trim(),
        region.trim(),
        key.trim().trim_start_matches('/')
    )
}

/// The SPA catch-all. **This `/{*path}` route out-competes a NESTED router's
/// `.fallback()` in the full app composition** — a lesson that cost two live
/// incidents and is recorded here, at the catch-all itself, because it stays
/// true for anything mounted alongside it.
///
/// A nested router that handles its unmatched paths via `.fallback()` will
/// silently serve index.html instead: that is how the SPA's group picker broke
/// (AMUX-2594) and how an auth probe was misled into reporting an
/// unauthenticated 200 on /api/fs. Any router that must answer arbitrary
/// sub-paths needs EXPLICIT `/` + `/{*rest}` routes, not a fallback.
///
/// (Carried over from py_proxy's passthrough router, whose forwarder was
/// deleted in AMUX-2906 — the code went, the hazard did not.)
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(index))
        // `any`, not `get` (AF-61). GET-only meant a POST/PATCH/DELETE to an
        // UNKNOWN /api/* path never reached the JSON-404 below — axum's method
        // router answered a bare 405 with an EMPTY body first. Measured
        // 2026-08-15: 9 rows of `POST /api/board/{id}/backlog`, a route that has
        // never existed, from two lanes; each got 405 and nothing to act on,
        // while the equivalent GET answers `{"error": "not found"}`.
        // `serve_path` still refuses to hand the SPA shell to a non-GET.
        .route("/{*path}", axum::routing::any(serve_path))
}

/// The retired port this request arrived on, if it did. Inserted by the legacy
/// listener's own middleware, so it is `Some` only when the request physically
/// came in on that socket — never from a client-supplied `Host` header, which
/// would let any client trigger the migration prompt against the real origin.
type Legacy = Option<axum::Extension<crate::legacy_port::OnLegacyListener>>;
type Peer = Option<Extension<ConnectInfo<SocketAddr>>>;

fn legacy_port_of(l: Legacy) -> Option<u16> {
    l.map(|axum::Extension(crate::legacy_port::OnLegacyListener(p))| p)
}

fn peer_ip(peer: Peer) -> Option<IpAddr> {
    peer.map(|Extension(ConnectInfo(addr))| addr.ip())
}

async fn index(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    peer: Peer,
    legacy: Legacy,
) -> Response {
    serve_shell(&state, legacy_port_of(legacy), &headers, &uri, peer_ip(peer)).await
}

async fn serve_path(
    State(state): State<AppState>,
    headers: HeaderMap,
    method: axum::http::Method,
    uri: Uri,
    peer: Peer,
    legacy: Legacy,
) -> Response {
    let path = uri.path().trim_start_matches('/');
    // UNKNOWN /api/* paths reach this catch-all (the API router only claims
    // registered routes) and must answer the Python server's JSON 404 — not
    // the SPA shell. Serving 200 text/html here made a probe conclude
    // "GET /api/fs?path=/tmp returns 200 with NO token": the "endpoint" was
    // this fallback handing back index.html (ethos rule 4 — the instrument
    // could not express "no such route").
    if path.starts_with("api/") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            "{\"error\": \"not found\"}",
        )
            .into_response();
    }
    // Non-API, non-GET: 405 as before. The SPA shell is a GET-only artifact and
    // handing it back for a POST would be worse than the bare 405 this replaces
    // — the whole point of the JSON 404 above is that a caller can tell "no such
    // route" from "here is a page".
    if method != axum::http::Method::GET && method != axum::http::Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if matches!(path, "business" | "business/" | "business/index.html") {
        return serve_shell(&state, legacy_port_of(legacy), &headers, &uri, peer_ip(peer)).await;
    }
    match DashboardAssets::get(path) {
        Some(content) => {
            let mime = mime_for(path);
            let mut resp =
                ([(header::CONTENT_TYPE, mime)], content.data.into_owned()).into_response();
            if path == "sw.js" {
                resp.headers_mut().insert(
                    header::CACHE_CONTROL,
                    "no-cache".parse().unwrap(),
                );
            }
            resp
        }
        // SPA fallback: unknown NON-API paths get the shell so client routing
        // works offline-first.
        None => serve_shell(&state, legacy_port_of(legacy), &headers, &uri, peer_ip(peer)).await,
    }
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|cookies| {
        cookies.split(';').find_map(|part| {
            let (cookie_name, value) = part.trim().split_once('=')?;
            (cookie_name == name && !value.is_empty()).then_some(value)
        })
    })
}

fn owner_session_value(state: &AppState) -> Option<String> {
    let owner_auth = state.auth_token.as_deref()?;
    let mut hash = sha2::Sha256::new();
    hash.update(format!("amux-owner-session:{owner_auth}"));
    Some(hex::encode(hash.finalize()))
}

fn has_owner_session(state: &AppState, headers: &HeaderMap) -> bool {
    match (owner_session_value(state), cookie_value(headers, OWNER_COOKIE)) {
        (Some(expected), Some(provided)) => {
            super::auth::constant_time_eq(provided.as_bytes(), expected.as_bytes())
        }
        _ => false,
    }
}

// A cookie proves access to the bootstrap, not to API mutations. Keep that
// distinction in diagnostics without recording the cookie or the owner token.
pub(crate) fn owner_session_status(state: &AppState, headers: &HeaderMap) -> &'static str {
    if has_owner_session(state, headers) {
        "valid"
    } else if cookie_value(headers, OWNER_COOKIE).is_some() {
        "invalid"
    } else {
        "missing"
    }
}

fn establish_owner_session(state: &AppState) -> Response {
    let Some(value) = owner_session_value(state) else {
        return Redirect::to("/").into_response();
    };
    let cookie = format!(
        "{OWNER_COOKIE}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=2592000"
    );
    // Redirect to /api/_clear_sw, which is outside the SW's intercept scope
    // (the SW passes through all /api/* paths). That page unregisters the
    // stale SW, clears caches, then redirects to /. Without this, an old SW
    // serves a cached HTML shell that predates the cookie and the auth token
    // is missing (iOS "connecting forever" bug).
    let mut response = Redirect::to("/api/_clear_sw").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("hex owner-session cookie is a valid header"),
    );
    tracing::info!(
        target: "amux::local_invite",
        verdict = "owner_session_established",
        "verified owner access was exchanged for an HttpOnly session"
    );
    response
}

/// Tiny HTML page that unregisters service workers and clears caches,
/// then redirects to /. Served at /api/_clear_sw so the old SW (which
/// passes through /api/* paths) cannot intercept it.
pub async fn clear_sw_landing() -> Response {
    const PAGE: &str = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>amux</title></head><body>
<p style="font-family:system-ui;text-align:center;margin-top:40vh">Refreshing...</p>
<script>
(async () => {
  try {
    const regs = await navigator.serviceWorker.getRegistrations();
    await Promise.all(regs.map(r => r.unregister()));
  } catch(e) {}
  try {
    const keys = await caches.keys();
    await Promise.all(keys.map(k => caches.delete(k)));
  } catch(e) {}
  location.replace('/');
})();
</script></body></html>"#;
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8"),
         (header::CACHE_CONTROL, "no-store")],
        PAGE,
    ).into_response()
}

mod tailnet_auth;

async fn serve_shell(
    state: &AppState,
    legacy: Option<u16>,
    headers: &HeaderMap,
    uri: &Uri,
    peer: Option<IpAddr>,
) -> Response {
    // Remove the bearer from the address bar before app.js or the service
    // worker starts. The resulting HttpOnly cookie survives the SW's canonical
    // `/` fetch and reload without leaving the bearer in history or caches.
    if super::auth::has_owner_query_token(state, uri) {
        return establish_owner_session(state);
    }
    // Same-owner Tailscale devices may opt into the existing HttpOnly owner
    // session. Use the real socket peer and daemon identity, never a forwarded
    // IP, hostname, or a claim supplied by the browser. Invitees remain scoped.
    if state.auth_token.is_some() && !has_owner_session(state, headers)
        && !super::org::has_local_member_cookie(headers)
    {
        if let Some(ip) = peer {
            if tailnet_auth::verified(ip).await { return establish_owner_session(state); }
        }
    }
    serve_index(state, legacy, headers, uri, peer)
}

fn request_authority(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|authority| authority.to_string()))
}

fn owner_bootstrap_allowed(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
    peer: Option<IpAddr>,
) -> bool {
    // An explicit owner credential always wins, including when the owner is
    // recovering a browser that still carries an old member cookie.
    if super::auth::has_owner_token(state, headers, uri) || has_owner_session(state, headers) {
        return true;
    }

    let verified_member = super::org::is_verified_local_member(headers);
    let has_member_cookie = super::org::has_local_member_cookie(headers);
    if verified_member || has_member_cookie {
        if has_member_cookie && !verified_member && state.auth_token.is_some() {
            tracing::warn!(
                target: "amux::local_invite",
                verdict = "revoked_member_bootstrap_withheld",
                "a dashboard reload carried an unverified member cookie; owner credentials were withheld"
            );
        }
        return false;
    }

    let authority = request_authority(headers, uri);
    let local = super::fs::browser_is_on_this_machine(peer, authority.as_deref());
    if let Some((peer_field, authority_field)) = withheld_bootstrap_record(
        state.auth_token.is_some(),
        local,
        peer,
        authority.as_deref(),
    ) {
        // AF-639: the ONLY server-side record that a browser was handed a
        // tokenless shell. Withholding is correct and deliberate (see
        // `inject_bootstrap`), and it is INVISIBLE: the window that receives
        // it then 401s on every /api call for the life of the tab, and the
        // request log shows those refusals with nothing naming a cause.
        // Measured 2026-09-09 on this fleet: 28,355 401s in 24h from one
        // laptop over Tailscale, at the SPA's 5s poll cadence, with zero log
        // lines explaining them.
        //
        // The two fields ARE the predicate rather than context around it:
        // `peer` is who connected and `authority` is the name they addressed,
        // and the shell is withheld precisely when that name does not resolve
        // back to that peer. A reader who has both can reproduce the verdict.
        tracing::warn!(
            target: "amux::shell_bootstrap",
            verdict = "remote_bootstrap_withheld",
            peer = %peer_field,
            authority = %authority_field,
            "served a tokenless dashboard shell: the addressed host does not resolve to this peer, so every /api request from that window will 401 until it loads the shell with ?_token="
        );
    }
    local
}

/// Whether this shell-serve is the withheld case, and the two fields the log
/// line carries: `(peer, authority)`.
///
/// Split out from the `tracing::warn!` above so the decision is testable.
/// Capturing the emitted line instead is NOT a workable check here: `tracing`
/// caches interest per callsite for the whole PROCESS, the suite runs tests in
/// parallel threads of one process, and a sibling test that reaches this code
/// path with no subscriber installed caches the callsite as disabled for every
/// later test. Measured 2026-09-09 while writing this: a capture-based version
/// passed run alone and failed in the suite, with a self-check probe proving
/// the capture harness itself was working. That is a test whose result depends
/// on scheduling, which is worse than no test.
///
/// What this cannot see is whether the warn is WIRED to it. That is checked
/// against the running server instead, with the curl repro on AF-639.
fn withheld_bootstrap_record(
    auth_configured: bool,
    local: bool,
    peer: Option<IpAddr>,
    authority: Option<&str>,
) -> Option<(String, String)> {
    // No token configured means nothing 401s, so there is nothing to report
    // and a line here would bury the real ones.
    if local || !auth_configured {
        return None;
    }
    Some((
        peer.map_or_else(|| "unknown".to_string(), |p| p.to_string()),
        authority.unwrap_or("").to_string(),
    ))
}

fn serve_index(
    state: &AppState,
    legacy: Option<u16>,
    headers: &HeaderMap,
    uri: &Uri,
    peer: Option<IpAddr>,
) -> Response {
    let asset = if uri.path() == "/business" || uri.path().starts_with("/business/") {
        "business/index.html"
    } else {
        "index.html"
    };
    let Some(index) = DashboardAssets::get(asset) else {
        return (StatusCode::NOT_FOUND, "dashboard not embedded").into_response();
    };
    let html = String::from_utf8_lossy(&index.data).into_owned();
    let owner_access = owner_bootstrap_allowed(state, headers, uri, peer);
    let member_verified = super::org::is_verified_local_member(headers);
    let owner_session = owner_session_status(state, headers);
    let verdict = if owner_access || member_verified || state.auth_token.is_none() {
        "dashboard_bootstrap_authenticated"
    } else {
        "dashboard_bootstrap_access_required"
    };
    tracing::info!(
        target: "amux::auth",
        verdict,
        owner_access,
        owner_session,
        member_verified,
        member_cookie = super::org::has_local_member_cookie(headers),
        bearer_present = headers.contains_key(header::AUTHORIZATION),
        peer_loopback = peer.map(|ip| ip.is_loopback()),
        // Deliberately no URL/query, cookie, or credential values.
        "dashboard bootstrap access decision"
    );
    let injected = inject_bootstrap(
        &html,
        state,
        legacy,
        owner_access,
    );
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        injected,
    )
        .into_response()
}

/// Replace the marked bootstrap block with live values. The UI token is
/// derived exactly as the Python does (sha256("amux-ui-guard:"+AUTH)[..40],
/// amux-server.py:801) so a dashboard served by either server produces
/// headers the OTHER server also accepts during coexistence.
/// `legacy` is `Some(port)` when this document is being served on the RETIRED
/// listener.
///
/// # Why the shell is the only place this can be fixed
///
/// The iPhone PWA was installed from `https://localhost:8822`, so the install
/// is a bookmark to that ORIGIN and everything it fetches is a relative
/// `/api/...` on it — ~3,200 requests an hour that no server-side change and no
/// process restart can move, because there is no process to restart. The
/// manifest cannot fix it either: `start_url` and `scope` must be same-origin
/// as the manifest itself (a port change IS a different origin), so a manifest
/// served on 8822 that points at 8824 is invalid and browsers fall back to the
/// document URL. That leaves exactly one lever — the document — and exactly one
/// thing it can do: tell the client, in the client, to go to the canonical
/// origin. See `_amuxLegacyOriginMigrate` in app.js for what it does with this.
///
/// `_AMUX_LEGACY_PORT` is 0 on the canonical listener, so the SPA's check is
/// "did the server say I am on the retired port", not "does my URL look odd" —
/// the client never has to know either number.
fn inject_bootstrap(html: &str, state: &AppState, legacy: Option<u16>, owner_access: bool) -> String {
    const BEGIN: &str = "<!-- AMUX-BOOTSTRAP-BEGIN";
    const END: &str = "<!-- AMUX-BOOTSTRAP-END -->";
    let (Some(b), Some(e)) = (html.find(BEGIN), html.find(END)) else {
        return html.to_string(); // no markers: serve untouched, never corrupt
    };
    let owner_auth = state.auth_token.clone().unwrap_or_default();
    // Invited and anonymous remote browsers never inherit the owner's bearer
    // from the public SPA bootstrap. Locality or an explicitly supplied owner
    // credential is required.
    let auth = if owner_access { owner_auth.clone() } else { String::new() };
    let ui_token = if owner_auth.is_empty() {
        String::new()
    } else {
        let mut h = sha2::Sha256::new();
        h.update(format!("amux-ui-guard:{owner_auth}"));
        hex::encode(h.finalize())[..40].to_string()
    };
    // AF-639: an EMPTY `_AMUX_AUTH_TOKEN` has two causes the SPA must not
    // confuse. Auth disabled entirely (no token configured) means nothing will
    // 401 and there is nothing to tell anyone. Withheld from a remote browser
    // means EVERY /api call will 401 for the life of that window, and no
    // reload can fix it because the server will withhold again. Only the
    // second is worth a human's attention, and the client cannot derive which
    // one it is from the empty string alone.
    let auth_withheld = !owner_access && !owner_auth.is_empty();
    let home = std::env::var("HOME").unwrap_or_default();
    let jstr = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let block = format!(
        "<!-- AMUX-BOOTSTRAP-BEGIN (injected at serve time) -->\n<script>\
         window._AMUX_S3_ICAL_URL={};window._AMUX_AUTH_TOKEN={};window._AMUX_HOME={};\
         window._AMUX_POSTHOG_KEY={};window._AMUX_POSTHOG_HOST={};window._AMUX_USER_EMAIL={};\
         window._AMUX_USER_ID={};window._AMUX_UI_TOKEN={};window._AMUX_DEFAULT_MODEL={};\
         window._AMUX_LEGACY_PORT={};window._AMUX_CANONICAL_PORT={};\
         window._AMUX_AUTH_WITHHELD={};</script>\n",
        jstr(&ical_subscribe_url()),
        jstr(&auth),
        jstr(&home),
        jstr(&std::env::var("POSTHOG_KEY").unwrap_or_default()),
        jstr(&std::env::var("POSTHOG_HOST").unwrap_or_else(|_| "https://us.i.posthog.com".into())),
        jstr(&std::env::var("AMUX_USER_EMAIL").unwrap_or_default()),
        jstr(&std::env::var("AMUX_USER_ID").unwrap_or_default()),
        jstr(&ui_token),
        // The REAL configured default, not a hardcoded guess — the settings
        // sweep caught the select showing sonnet after a PATCH (finding #2).
        jstr(&crate::api::settings::get_default_model(
            &std::env::var("AMUX_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                        .join(".amux")
                }),
        )),
        legacy.unwrap_or(0),
        crate::legacy_port::canonical_port(),
        auth_withheld,
    );
    let with_bootstrap = format!("{}{}{}", &html[..b], block, &html[e..]);
    // Client update adoption is the SSE ping's job, exactly like Python
    // (amux-server.py:65292): every ping carries `v` = the embedded APP_VER
    // (see sse.rs::ping_payload) and the SPA self-reloads on mismatch,
    // rate-limited and SW-nudged. The earlier /health-polling banner is
    // deliberately GONE (Ethan 2026-08-09: "frontend clients should also
    // restart just like the python server") — a banner on backend-only
    // build changes was noise Python never showed, and the reload it
    // offered is now automatic when it matters (client code changed).
    // CRM is removed from the Rust build (Ethan, 2026-08-09): hide its tab
    // and view via the serve-time layer — the extracted SPA stays
    // byte-identical, the decision lives HERE where it is one greppable
    // line to reverse.
    let crm_hide = r#"<style>/* AMUX-FEATURE-FLAGS (injected) */
[onclick="switchView('crm')"], #crm-view { display: none !important; }
</style>
"#;
    with_bootstrap.replacen("</body>", &format!("{crm_hide}</body>"), 1)
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("webmanifest") => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    fn state(token: Option<&str>) -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            store,
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: token.map(String::from),
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    #[tokio::test]
    async fn business_shell_preserves_the_existing_bootstrap_identity_boundary() {
        let state = state(Some("business-test-owner-token"));
        for (host, address, expected) in [
            ("localhost:8824", "127.0.0.1:12345", true),
            ("business.example.test", "203.0.113.4:12345", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            let response = serve_path(State(state.clone()), headers,
                axum::http::Method::GET, "/business/".parse().unwrap(),
                Some(Extension(ConnectInfo(address.parse().unwrap()))), None).await;
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 100_000).await.unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(body.contains("<title>Amux Business</title>"));
            assert!(body.contains("/business/assets/"));
            assert_eq!(body.contains("window._AMUX_AUTH_TOKEN=\"business-test-owner-token\""), expected);
        }
    }

    #[test]
    fn bootstrap_injects_auth_and_derived_ui_token() {
        let html = "<head><!-- AMUX-BOOTSTRAP-BEGIN x -->old<!-- AMUX-BOOTSTRAP-END --></head>";
        let out = inject_bootstrap(html, &state(Some("tok123")), None, true);
        assert!(out.contains("window._AMUX_AUTH_TOKEN=\"tok123\""));
        // Python-parity UI token: sha256("amux-ui-guard:tok123")[..40]
        let mut h = sha2::Sha256::new();
        h.update("amux-ui-guard:tok123");
        let expect = &hex::encode(h.finalize())[..40];
        assert!(out.contains(expect), "{out}");
        assert!(!out.contains("old"), "placeholder block replaced");
    }

    #[test]
    fn invited_member_bootstrap_withholds_owner_bearer_but_keeps_ui_guard() {
        let html = "<head><!-- AMUX-BOOTSTRAP-BEGIN x -->old<!-- AMUX-BOOTSTRAP-END --></head>";
        let owner = inject_bootstrap(html, &state(Some("tok123")), None, true);
        let member = inject_bootstrap(html, &state(Some("tok123")), None, false);
        assert!(member.contains("window._AMUX_AUTH_TOKEN=\"\""), "{member}");
        assert!(!member.contains("window._AMUX_AUTH_TOKEN=\"tok123\""), "{member}");
        let owner_guard = owner.split("window._AMUX_UI_TOKEN=").nth(1)
            .and_then(|value| value.split(';').next()).unwrap();
        assert!(member.contains(&format!("window._AMUX_UI_TOKEN={owner_guard}")), "{member}");
    }

    /// AF-639. A remote browser is denied the owner bearer on purpose, and
    /// until now the server said nothing about it. Measured on this fleet:
    /// 28,355 401s in 24 hours from one laptop over Tailscale, at the SPA's 5s
    /// poll cadence, and not one log line naming the cause.
    #[test]
    fn the_withheld_bootstrap_record_reports_the_predicate_and_only_the_real_case() {
        let peer: std::net::IpAddr = "203.0.113.5".parse().unwrap();

        // The case that produced the 28k refusals: auth on, browser remote.
        let (p, a) = withheld_bootstrap_record(true, false, Some(peer), Some("desktop.example:8824"))
            .expect("a remote browser denied the bearer must be reported");
        assert_eq!(p, "203.0.113.5", "the peer is half the predicate");
        assert_eq!(a, "desktop.example:8824", "the authority is the other half");

        // The three cases that must stay SILENT, each for its own reason.
        assert!(
            withheld_bootstrap_record(true, true, Some(peer), Some("h")).is_none(),
            "a local browser gets the bearer; there is nothing to report"
        );
        assert!(
            withheld_bootstrap_record(false, false, Some(peer), Some("h")).is_none(),
            "with no token configured NOTHING 401s, so this line would bury the real ones"
        );
        assert!(
            withheld_bootstrap_record(false, true, Some(peer), Some("h")).is_none(),
            "neither condition holds"
        );

        // A request with no authority at all is still the withheld case, and
        // the empty field is the answer rather than a reason to say nothing.
        let (p2, a2) = withheld_bootstrap_record(true, false, None, None)
            .expect("an unknown peer is still a withheld shell");
        assert_eq!(p2, "unknown");
        assert_eq!(a2, "");
    }

    /// The predicate the log line describes must be the one the caller acts
    /// on. Reading `owner_bootstrap_allowed` end to end is what catches the
    /// two drifting apart, which no test of the pure function alone can see.
    #[test]
    fn owner_bootstrap_allowed_and_the_withheld_record_agree_on_a_remote_browser() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "amux.invalid".parse().unwrap());
        let uri: Uri = "/".parse().unwrap();
        // TEST-NET-3 (RFC 5737) with a reserved TLD (RFC 2606): even on a host
        // with a wildcard resolver, `amux.invalid` cannot resolve TO this
        // peer, so the verdict is the same everywhere this runs.
        let peer: std::net::IpAddr = "203.0.113.5".parse().unwrap();

        let allowed = owner_bootstrap_allowed(&state(Some("tok123")), &headers, &uri, Some(peer));
        assert!(!allowed, "a remote browser must not be bootstrapped with the owner bearer");
        assert!(
            withheld_bootstrap_record(true, allowed, Some(peer), Some("amux.invalid")).is_some(),
            "the same inputs that withhold the bearer must also produce the log record"
        );

        // And the shell built from that decision really is tokenless, so the
        // record describes a window that will 401 rather than a hypothesis.
        let html = "<head><!-- AMUX-BOOTSTRAP-BEGIN x -->old<!-- AMUX-BOOTSTRAP-END --></head>";
        let shell = inject_bootstrap(html, &state(Some("tok123")), None, allowed);
        assert!(shell.contains("window._AMUX_AUTH_TOKEN=\"\""), "{shell}");
        assert!(shell.contains("window._AMUX_AUTH_WITHHELD=true;"), "{shell}");
    }

    /// AF-639, client half. An empty `_AMUX_AUTH_TOKEN` has two causes with
    /// opposite consequences, and the SPA cannot tell them apart from the
    /// empty string.
    #[test]
    fn the_shell_says_whether_an_empty_token_means_withheld_or_auth_disabled() {
        let html = "<head><!-- AMUX-BOOTSTRAP-BEGIN x -->old<!-- AMUX-BOOTSTRAP-END --></head>";
        let owner = inject_bootstrap(html, &state(Some("tok123")), None, true);
        let withheld = inject_bootstrap(html, &state(Some("tok123")), None, false);
        let no_auth = inject_bootstrap(html, &state(None), None, false);

        assert!(owner.contains("window._AMUX_AUTH_WITHHELD=false;"), "{owner}");
        assert!(withheld.contains("window._AMUX_AUTH_WITHHELD=true;"), "{withheld}");

        // Auth disabled: the token is empty here too and NOTHING will 401.
        // Reporting "withheld" would put a permanent banner in front of every
        // user of a tokenless server.
        assert!(no_auth.contains("window._AMUX_AUTH_TOKEN=\"\""), "{no_auth}");
        assert!(no_auth.contains("window._AMUX_AUTH_WITHHELD=false;"), "{no_auth}");
    }

    /// The bootstrap block is one Rust string literal held together by
    /// backslash line continuations, and dropping one does not fail to
    /// compile: it renders as a run of spaces inside the emitted JavaScript.
    /// This lane shipped that exact bug twice (AF-621, AF-634), so the guard
    /// covers the whole script rather than the line just added to it.
    #[test]
    fn the_injected_script_carries_no_run_of_spaces_from_a_dropped_continuation() {
        let html = "<head><!-- AMUX-BOOTSTRAP-BEGIN x -->old<!-- AMUX-BOOTSTRAP-END --></head>";
        let out = inject_bootstrap(html, &state(Some("tok123")), None, true);
        let script = out
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("the injected block has a script element");
        assert!(
            !script.contains("  "),
            "a dropped line continuation renders as a run of spaces: {script:?}"
        );
        // The guard is worth nothing if the block it reads is empty.
        assert!(script.contains("window._AMUX_AUTH_WITHHELD="), "{script:?}");
        assert!(script.len() > 200, "script suspiciously short: {script:?}");
    }

    #[test]
    fn no_update_banner_is_injected() {
        // Client adoption rides the SSE ping's `v` (sse.rs::ping_payload,
        // Python parity) — the old /health-polling banner must stay gone,
        // or a backend-only deploy shows UI Python never showed.
        let html = "<head><!-- AMUX-BOOTSTRAP-BEGIN x -->old<!-- AMUX-BOOTSTRAP-END --></head><body></body>";
        let s = state(Some("tok"));
        let out = inject_bootstrap(html, &s, None, true);
        assert!(!out.contains("AMUX-UPDATE-WATCH"));
        assert!(!out.contains("amux-update-bar"));
        // The CRM feature-flag layer still injects.
        assert!(out.contains("AMUX-FEATURE-FLAGS"));
    }

    /// The migration signal must be ON only for documents actually served by
    /// the retired listener — and it must actually be ON there.
    ///
    /// Both halves are load-bearing and neither alone is a check. If it were
    /// always 0 the PWA would never be told to move and the port would never
    /// drain (the failure that is invisible: nothing appears broken). If it
    /// were always non-zero, every desktop client already on 8824 would be
    /// prompted to migrate to where it already is — a loop the user cannot
    /// exit. The bug this pins is the easy one to write: reading the port from
    /// the `Host` header, which any client can set, instead of from which
    /// SOCKET the request arrived on.
    #[test]
    fn legacy_marker_is_injected_only_when_served_on_the_retired_port() {
        let html = "<head><!-- AMUX-BOOTSTRAP-BEGIN x -->old<!-- AMUX-BOOTSTRAP-END --></head><body></body>";
        let s = state(Some("tok"));

        let canonical = inject_bootstrap(html, &s, None, true);
        assert!(
            canonical.contains("window._AMUX_LEGACY_PORT=0"),
            "a document served on the canonical port must report legacy 0, or every \
             already-migrated client is told to migrate: {canonical}"
        );

        let from_legacy = inject_bootstrap(html, &s, Some(8822), true);
        assert!(
            from_legacy.contains("window._AMUX_LEGACY_PORT=8822"),
            "a document served on the retired port must say so — this is the ONLY \
             signal the installed PWA can ever receive: {from_legacy}"
        );
        // The canonical port has to travel with it: the client builds the target
        // origin from its own hostname plus this number, so a LAN or tailscale
        // client is sent somewhere that exists rather than to `localhost`.
        assert!(
            from_legacy.contains(&format!(
                "window._AMUX_CANONICAL_PORT={}",
                crate::legacy_port::canonical_port()
            )),
            "{from_legacy}"
        );
    }

    #[test]
    fn missing_markers_serve_untouched() {
        let html = "<head>no markers</head>";
        assert_eq!(inject_bootstrap(html, &state(None), None, false), html);
    }

    async fn shell(
        app: &Router,
        uri: &str,
        cookie: Option<&str>,
        peer: Option<&str>,
    ) -> String {
        use tower::ServiceExt;
        let mut builder = axum::http::Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let mut request = builder.body(axum::body::Body::empty()).unwrap();
        if let Some(peer) = peer {
            let addr: SocketAddr = format!("{peer}:50000").parse().unwrap();
            request.extensions_mut().insert(ConnectInfo(addr));
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn owner_bearer_is_not_bootstrapped_to_remote_or_revoked_member_shells() {
        let app = routes().with_state(state(Some("tok123")));

        let remote = shell(&app, "https://remote-node.example/", None, None).await;
        assert!(remote.contains("window._AMUX_AUTH_TOKEN=\"\""), "{remote}");

        let local = shell(
            &app,
            "https://desktop.tailnet.example/",
            None,
            Some("127.0.0.1"),
        )
        .await;
        assert!(local.contains("window._AMUX_AUTH_TOKEN=\"tok123\""), "{local}");

        // Deleting a member removes the DB row but their HttpOnly cookie stays
        // in the browser. Even on the owner's machine that stale cookie must
        // not change roles on reload and inherit the owner credential.
        let revoked = shell(
            &app,
            "https://desktop.tailnet.example/",
            Some("amux_member=revoked-invite-token"),
            Some("127.0.0.1"),
        )
        .await;
        assert!(revoked.contains("window._AMUX_AUTH_TOKEN=\"\""), "{revoked}");
        assert!(!revoked.contains("window._AMUX_AUTH_TOKEN=\"tok123\""), "{revoked}");

        // A remote owner can still authenticate explicitly. Exchange the URL
        // token for an HttpOnly cookie before serving any credential-bearing
        // HTML, so the service worker's canonical `/` cache and reload retain
        // the owner session without retaining the URL token.
        use tower::ServiceExt;
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("https://remote-node.example/?_token=tok123")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/api/_clear_sw");
        let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("Secure"), "{set_cookie}");
        assert!(!set_cookie.contains("tok123"), "the raw bearer must not be copied into the cookie");
        let owner_cookie = set_cookie.split(';').next().unwrap();
        let explicit = shell(&app, "https://remote-node.example/", Some(owner_cookie), None).await;
        assert!(explicit.contains("window._AMUX_AUTH_TOKEN=\"tok123\""), "{explicit}");
    }

    /// AF-61: the GET-only version of this test passed for months while every
    /// NON-GET to an unknown /api path got a bare 405 with an empty body —
    /// axum's method router answering before the JSON-404 branch was reached.
    /// Measured: 9 `POST /api/board/{id}/backlog` rows from two lanes, a route
    /// that never existed, each given nothing to act on. A test that exercises
    /// only the method that already worked cannot fail on the one that did not.
    #[tokio::test]
    async fn unknown_api_path_is_a_json_404_for_every_method_not_just_get() {
        use tower::ServiceExt;
        for m in ["POST", "PATCH", "DELETE", "PUT", "GET"] {
            let app = routes().with_state(state(Some("tok")));
            let res = app
                .oneshot(
                    axum::http::Request::builder()
                        .method(m)
                        // The real specimen, not a convenient one.
                        .uri("/api/board/AF-49/backlog")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{m} must 404, not 405");
            assert_eq!(
                res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
                "application/json",
                "{m} must get JSON a caller can parse"
            );
        }
        // A non-GET to an unknown NON-api path must still NOT get the SPA shell:
        // handing back HTML for a POST would be worse than the 405 it replaces.
        let app = routes().with_state(state(Some("tok")));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/some/client/route")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED, "no SPA shell for a POST");
    }

    #[tokio::test]
    async fn unknown_api_path_is_a_json_404_not_the_spa_shell() {
        use tower::ServiceExt;
        let app = routes().with_state(state(Some("tok")));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/definitely-not-a-route?x=1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({ "error": "not found" }));

        // Non-API unknown paths still get the SPA shell (client routing).
        let res = routes()
            .with_state(state(Some("tok")))
            .oneshot(
                axum::http::Request::builder()
                    .uri("/some/client/route")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
        assert!(ct.starts_with("text/html"), "{ct}");
    }
}
