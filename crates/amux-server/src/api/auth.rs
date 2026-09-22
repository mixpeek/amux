//! Request auth (RR-0021) — PYTHON-PARITY port of `_check_auth`
//! (amux-server.py:65421).
//!
//! The token lives at `~/.amux/auth_token` — UNDERSCORE, Python's
//! `_AUTH_TOKEN_FILE` (amux-server.py:700). This crate originally read
//! `auth-token` (dash), minted its OWN token there, and then 401'd every
//! client holding the real shared token while claiming in this very
//! docstring that the file was shared. One token, one file, both servers.
//!
//! Admission rules, in Python's order (each is Python behavior, not a new
//! decision):
//! 1. No token configured -> everything passes (tests, first-run,
//!    `AMUX_AUTH_TOKEN=none`).
//! 2. Localhost peers always pass — local sessions and CLI tools
//!    authenticate by locality, and every `curl -sk $AMUX_URL` recipe in
//!    CLAUDE.md depends on it.
//! 3. Python's public paths/prefixes pass.
//! 4. Non-API GETs pass (the dashboard shell is served unauthenticated on
//!    the LAN — same trust model as Python, see static_files.rs).
//! 5. `Authorization: Bearer <token>`.
//! 6. `?_token=<token>` — the SPA's `_authUrl` spelling (EventSource and
//!    `<img>`/`<a>` URLs cannot set headers). Python accepts ONLY `_token=`;
//!    the bare `token=` spelling this middleware once also accepted is gone
//!    (nothing in the SPA or sw.js sends it — verified by grep 2026-08-09).
//!
//! `X-Amux-UI-Token` is deliberately NOT here: it is not an auth credential.
//! Python checks it per-handler as an extra guard on DESTRUCTIVE session ops
//! (`_session_destructive_allowed`, amux-server.py:804), and its value is the
//! sha256("amux-ui-guard:"+token)[..40] derivation — never valid for
//! `_check_auth`.

use super::AppState;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::net::SocketAddr;

/// Python `_PUBLIC_PATHS` (amux-server.py:845). Kept verbatim even where the
/// Rust router has no such route yet — a path-only predicate that drifts from
/// the oracle is how the two servers end up protecting different surfaces.
const PUBLIC_PATHS: &[&str] = &[
    "/",
    "/manifest.json",
    "/sw.js",
    "/icon.svg",
    "/icon.png",
    "/icon-192.png",
    "/icon-512.png",
    "/ca",
    "/release-notes",
    "/api/release-notes",
    "/api/calendar.ics",
];

/// Python `_PUBLIC_PREFIXES` (amux-server.py:848).
const PUBLIC_PREFIXES: &[&str] = &[
    "/s/",
    "/api/share/",
    "/invite/",
    "/proxy/",
    "/api/branding/",
];

pub async fn require_bearer(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = &state.auth_token else {
        return next.run(req).await;
    };
    // An explicit owner credential wins even if this browser also carries an
    // old member cookie. Owners use that combination when testing an invite in
    // the same browser; treating them as the invitee would lock them out of the
    // very membership controls needed to repair it.
    if provided_owner_token(req.headers(), req.uri())
        .is_some_and(|t| constant_time_eq(t.as_bytes(), expected.as_bytes()))
    {
        return next.run(req).await;
    }
    // Local invitees authenticate through the revocable member cookie. Only
    // org::local_member_identity can insert this marker: it strips any inbound
    // copy before validating the cookie against org_invites -> org_members.
    // Authorization is evaluated on every request against the member row, so
    // rescoping or revoking a user takes effect without reminting their cookie.
    if super::org::is_verified_local_member(req.headers()) {
        if let Some(response) = super::org::authorize_local_member_request(
            &state,
            req.method(),
            req.uri(),
            req.headers(),
        ) {
            return response;
        }
        return next.run(req).await;
    }
    // Localhost always bypasses auth (Python parity: local sessions, CLI
    // tools). ConnectInfo is only present when the server was started with
    // into_make_service_with_connect_info (lib.rs does); absence — e.g. in
    // router-level tests — errs toward REQUIRING the token.
    //
    // AMUX_RS_NO_LOOPBACK_BYPASS=1 disables it, for e2e that must exercise
    // the TOKEN path: a browser test necessarily connects over loopback, so
    // without this knob "reject a bad token" is a check that cannot pass
    // (e2e/phase0.spec.ts asserted 401 and was reported as an auth
    // regression, 2026-08-09 — the code was right, the check was unrunnable).
    if !std::env::var("AMUX_RS_NO_LOOPBACK_BYPASS").is_ok_and(|v| v == "1")
        && req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|ci| ci.0.ip().is_loopback())
    {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if PUBLIC_PATHS.contains(&path) || PUBLIC_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return next.run(req).await;
    }
    if req.method() == Method::GET && !path.starts_with("/api/") && !path.starts_with("/proxy/") {
        return next.run(req).await;
    }
    let owner_session = super::static_files::owner_session_status(&state, req.headers());
    let bearer_present = provided_owner_token(req.headers(), req.uri()).is_some();
    let member_cookie = super::org::has_local_member_cookie(req.headers());
    let reason = if owner_session == "valid" {
        "owner_session_requires_bootstrap"
    } else if bearer_present {
        "invalid_bearer"
    } else if member_cookie {
        "unverified_member_cookie"
    } else if owner_session == "invalid" {
        "invalid_owner_session"
    } else {
        "missing_credential"
    };
    // The request log keeps this JSON reason for /api/logs/analyze, including
    // when the client's own /api/client-debug beacon is denied. A sweep no
    // longer needs a working browser beacon to explain a bootstrap 401.
    if matches!(path, "/api/workers" | "/api/sessions" | "/api/client-debug") {
        tracing::warn!(
            target: "amux::auth", verdict = "dashboard_auth_rejected",
            reason, owner_session, bearer_present, member_cookie,
            path, method = %req.method(),
            "dashboard request requires authentication"
        );
    }
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({
            "error": "unauthorized",
            "reason": reason,
            "recovery": if owner_session == "valid" { "refresh_bootstrap" } else { "open_owner_or_invite_link" }
        })),
    ).into_response()
}

/// Whether this request explicitly presented the configured owner bearer.
///
/// Static shell serving is intentionally outside [`require_bearer`] so an
/// invited member can load it using only their HttpOnly member cookie. It must
/// still be able to distinguish an explicitly-authenticated remote owner from
/// an anonymous tailnet peer without duplicating the credential parser.
pub(crate) fn has_owner_token(state: &AppState, headers: &HeaderMap, uri: &Uri) -> bool {
    state.auth_token.as_deref().is_some_and(|expected| {
        provided_owner_token(headers, uri)
            .is_some_and(|provided| constant_time_eq(provided.as_bytes(), expected.as_bytes()))
    })
}

/// The audit name for a caller who presented the owner bearer and no worker or
/// member identity. `policy.rs` minted this spelling for approval provenance;
/// it is a constant so the second user (AMUX-4755) cannot invent a third.
pub(crate) const OWNER_TOKEN_ACTOR: &str = "owner-token";

/// Name the owner when nothing else can (AMUX-4755).
///
/// THE DASHBOARD IS NOT A WORKER and has no `X-Amux-Session` to send, so every
/// write it makes resolved to `api-anonymous` — including bulk-migrate, the
/// largest destructive board action there is. Measured from the issues table:
/// 373 cards discarded in one minute on 2026-09-15, 123 on 09-16, 202 across
/// six minutes, all anonymous. Two of those bursts landed in the same minute on
/// consecutive days, and with no actor on them a peer lane read the pair as a
/// runaway daily job and re-filed on that basis. The anonymity is what made an
/// ordinary human click look like a daemon.
///
/// WHAT THIS IS ALLOWED TO CLAIM. The browser sends `Authorization: Bearer
/// <owner token>` on every API call (`static_files::inject_bootstrap` puts it
/// in the shell, `app.js::_authHeaders` sends it), and that token is compared
/// against `state.auth_token` in constant time. So "this request presented the
/// owner credential" is verified, not asserted, and `owner-token` says exactly
/// that and no more. It does NOT claim a named human: on a box with no auth
/// token configured there is no owner credential to present, this returns
/// `None`, and the caller stays `api-anonymous` — which is the honest answer
/// there rather than a name nobody could check.
///
/// Callers apply it only where the ordinary resolution already gave up. An
/// actor that resolved to a worker or a verified member keeps that name: the
/// one CLI site that sends this bearer sends `X-Amux-Session` with it, so
/// upgrading a named caller here would rename real lanes to "the owner".
pub(crate) fn owner_token_actor(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Option<&'static str> {
    has_owner_token(state, headers, uri).then_some(OWNER_TOKEN_ACTOR)
}

/// Query-only variant used by the public shell to exchange a one-time URL
/// credential for an HttpOnly owner session before a service worker can cache
/// the credential-bearing URL or reload it without the query string.
pub(crate) fn has_owner_query_token(state: &AppState, uri: &Uri) -> bool {
    let provided = uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("_token=")));
    match (state.auth_token.as_deref(), provided) {
        (Some(expected), Some(provided)) => {
            constant_time_eq(provided.as_bytes(), expected.as_bytes())
        }
        _ => false,
    }
}

fn provided_owner_token<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            uri.query()
                .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("_token=")))
        })
}

/// Constant-time comparison — a token check that leaks length-prefix timing
/// is a token check that can be brute-forced from the LAN.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Load or mint the auth token file (0600). Python mints `token_urlsafe(32)`;
/// either server minting first is fine — the other adopts the same file.
pub fn load_or_create_token(path: &std::path::Path) -> anyhow::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let token = ulid::Ulid::new().to_string().to_lowercase();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{token}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn token_minted_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("amux-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth_token");
        let t1 = load_or_create_token(&p).unwrap();
        let t2 = load_or_create_token(&p).unwrap();
        assert_eq!(t1, t2);
        assert!(!t1.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn state(token: Option<&str>) -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: token.map(String::from),
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn guarded_app(token: Option<&str>) -> Router {
        let st = state(token);
        Router::new()
            .route(
                "/api/thing",
                get(|| async { "ok" }).post(|| async { "posted" }),
            )
            .route("/api/branding/asset/{f}", get(|| async { "asset" }))
            .route(
                "/page",
                get(|| async { "page" }).post(|| async { "page-post" }),
            )
            .layer(axum::middleware::from_fn_with_state(
                st.clone(),
                require_bearer,
            ))
            .with_state(st)
    }

    async fn hit(
        app: &Router,
        method: &str,
        uri: &str,
        bearer: Option<&str>,
        peer: Option<&str>,
    ) -> StatusCode {
        let mut b = HttpRequest::builder().method(method).uri(uri);
        if let Some(t) = bearer {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let mut req = b.body(Body::empty()).unwrap();
        if let Some(ip) = peer {
            let addr: SocketAddr = format!("{ip}:55555").parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        app.clone().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn python_parity_admission_matrix() {
        let app = guarded_app(Some("tok123"));

        // Bearer with the right token passes; wrong/absent 401s.
        assert_eq!(
            hit(&app, "GET", "/api/thing", Some("tok123"), None).await,
            StatusCode::OK
        );
        assert_eq!(
            hit(&app, "GET", "/api/thing", Some("nope"), None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            hit(&app, "GET", "/api/thing", None, None).await,
            StatusCode::UNAUTHORIZED
        );

        // ?_token= (the SPA's _authUrl spelling) passes; the bare token=
        // spelling is NOT an auth form (Python parity).
        assert_eq!(
            hit(&app, "GET", "/api/thing?_token=tok123", None, None).await,
            StatusCode::OK
        );
        assert_eq!(
            hit(&app, "GET", "/api/thing?token=tok123", None, None).await,
            StatusCode::UNAUTHORIZED
        );

        // Localhost peer bypasses entirely; a LAN peer does not.
        assert_eq!(
            hit(&app, "GET", "/api/thing", None, Some("127.0.0.1")).await,
            StatusCode::OK
        );
        assert_eq!(
            hit(&app, "POST", "/api/thing", None, Some("127.0.0.1")).await,
            StatusCode::OK
        );
        assert_eq!(
            hit(&app, "GET", "/api/thing", None, Some("192.168.1.50")).await,
            StatusCode::UNAUTHORIZED
        );

        // Python public prefix: branding assets load tokenless (an <img>
        // cannot set headers).
        assert_eq!(
            hit(
                &app,
                "GET",
                "/api/branding/asset/logo.png",
                None,
                Some("192.168.1.50")
            )
            .await,
            StatusCode::OK
        );

        // Non-API GET is public (dashboard shell trust model); non-API POST
        // is not.
        assert_eq!(
            hit(&app, "GET", "/page", None, Some("192.168.1.50")).await,
            StatusCode::OK
        );
        assert_eq!(
            hit(&app, "POST", "/page", None, Some("192.168.1.50")).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn no_token_disables_auth_and_401_names_missing_credential() {
        let open = guarded_app(None);
        assert_eq!(
            hit(&open, "POST", "/api/thing", None, None).await,
            StatusCode::OK
        );

        let app = guarded_app(Some("tok123"));
        let res = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/thing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.headers().get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "error": "unauthorized", "reason": "missing_credential",
                "recovery": "open_owner_or_invite_link"
            })
        );
    }

    /// AMUX-4755. `owner-token` is a claim about a VERIFIED credential, so the
    /// cell that matters is the one where there is nothing to verify.
    ///
    /// A server with no auth token configured has no owner credential at all.
    /// Naming an owner there would be inventing an actor nobody could check,
    /// which is worse than `api-anonymous`: the anonymous name is at least
    /// true. The wrong-bearer leg is the other half — presenting A token is
    /// not presenting THE token.
    #[test]
    fn only_the_real_owner_bearer_is_named_the_owner() {
        let uri = Uri::from_static("/api/board/bulk-migrate");
        let bearer = |t: &str| {
            let mut h = HeaderMap::new();
            h.insert("authorization", format!("Bearer {t}").parse().unwrap());
            h
        };

        let configured = state(Some("tok123"));
        assert_eq!(
            owner_token_actor(&configured, &bearer("tok123"), &uri),
            Some("owner-token"),
            "the owner's own bearer is the strongest identity a browser can present"
        );
        assert_eq!(
            owner_token_actor(&configured, &bearer("not-the-token"), &uri),
            None,
            "a wrong bearer must not be named the owner"
        );
        assert_eq!(
            owner_token_actor(&configured, &HeaderMap::new(), &uri),
            None,
            "no credential is not the owner"
        );

        // THE ONE THAT KEEPS THIS HONEST: nothing to verify, so nothing named.
        // Note the bearer is the string a caller could guess from an unset
        // config; an implementation that skipped the state check would pass
        // every leg above and fail only here.
        let open = state(None);
        assert_eq!(
            owner_token_actor(&open, &bearer("tok123"), &uri),
            None,
            "a server with no owner token configured has no owner to name"
        );
        assert_eq!(owner_token_actor(&open, &HeaderMap::new(), &uri), None);
    }

    #[tokio::test]
    async fn rejected_credentials_name_the_recovery_without_leaking_or_granting_access() {
        use sha2::Digest;
        let cookie = format!(
            "__Host-amux_owner={}",
            hex::encode(sha2::Sha256::digest(b"amux-owner-session:tok123"))
        );
        let app = guarded_app(Some("tok123"));
        for (bearer, cookies, reason, recovery) in [
            (
                Some("old-secret"),
                "",
                "invalid_bearer",
                "open_owner_or_invite_link",
            ),
            (
                None,
                "amux_member=revoked-secret",
                "unverified_member_cookie",
                "open_owner_or_invite_link",
            ),
            (
                None,
                "__Host-amux_owner=old-secret",
                "invalid_owner_session",
                "open_owner_or_invite_link",
            ),
            (
                None,
                cookie.as_str(),
                "owner_session_requires_bootstrap",
                "refresh_bootstrap",
            ),
            (
                Some("old-secret"),
                cookie.as_str(),
                "owner_session_requires_bootstrap",
                "refresh_bootstrap",
            ),
        ] {
            let mut req = HttpRequest::builder()
                .uri("/api/thing")
                .header("cookie", cookies);
            if let Some(bearer) = bearer {
                req = req.header("authorization", format!("Bearer {bearer}"));
            }
            let response = app
                .clone()
                .oneshot(req.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{reason}");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"error":"unauthorized", "reason":reason, "recovery":recovery})
            );
            let rendered = String::from_utf8(bytes.to_vec()).unwrap();
            for secret in ["tok123", "old-secret", "revoked-secret", cookie.as_str()] {
                assert!(!rendered.contains(secret));
            }
        }
    }
}
