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
use axum::http::{Method, StatusCode};
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
const PUBLIC_PREFIXES: &[&str] = &["/s/", "/api/share/", "/invite/", "/proxy/", "/api/branding/"];

pub async fn require_bearer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(expected) = &state.auth_token else {
        return next.run(req).await;
    };
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
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| {
            req.uri()
                .query()
                .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("_token=")))
        });
    match provided {
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes()) => next.run(req).await,
        // Python's exact 401: JSON body, not bare text (the SPA and CLI both
        // parse the body; a text/plain 401 reads as a broken server).
        _ => (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            "{\"error\": \"unauthorized\"}",
        )
            .into_response(),
    }
}

/// Constant-time comparison — a token check that leaks length-prefix timing
/// is a token check that can be brute-forced from the LAN.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
            .route("/api/thing", get(|| async { "ok" }).post(|| async { "posted" }))
            .route("/api/branding/asset/{f}", get(|| async { "asset" }))
            .route("/page", get(|| async { "page" }).post(|| async { "page-post" }))
            .layer(axum::middleware::from_fn_with_state(st.clone(), require_bearer))
            .with_state(st)
    }

    async fn hit(app: &Router, method: &str, uri: &str, bearer: Option<&str>, peer: Option<&str>) -> StatusCode {
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
        assert_eq!(hit(&app, "GET", "/api/thing", Some("tok123"), None).await, StatusCode::OK);
        assert_eq!(hit(&app, "GET", "/api/thing", Some("nope"), None).await, StatusCode::UNAUTHORIZED);
        assert_eq!(hit(&app, "GET", "/api/thing", None, None).await, StatusCode::UNAUTHORIZED);

        // ?_token= (the SPA's _authUrl spelling) passes; the bare token=
        // spelling is NOT an auth form (Python parity).
        assert_eq!(hit(&app, "GET", "/api/thing?_token=tok123", None, None).await, StatusCode::OK);
        assert_eq!(hit(&app, "GET", "/api/thing?token=tok123", None, None).await, StatusCode::UNAUTHORIZED);

        // Localhost peer bypasses entirely; a LAN peer does not.
        assert_eq!(hit(&app, "GET", "/api/thing", None, Some("127.0.0.1")).await, StatusCode::OK);
        assert_eq!(hit(&app, "POST", "/api/thing", None, Some("127.0.0.1")).await, StatusCode::OK);
        assert_eq!(hit(&app, "GET", "/api/thing", None, Some("192.168.1.50")).await, StatusCode::UNAUTHORIZED);

        // Python public prefix: branding assets load tokenless (an <img>
        // cannot set headers).
        assert_eq!(hit(&app, "GET", "/api/branding/asset/logo.png", None, Some("192.168.1.50")).await, StatusCode::OK);

        // Non-API GET is public (dashboard shell trust model); non-API POST
        // is not.
        assert_eq!(hit(&app, "GET", "/page", None, Some("192.168.1.50")).await, StatusCode::OK);
        assert_eq!(hit(&app, "POST", "/page", None, Some("192.168.1.50")).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn no_token_disables_auth_and_401_body_is_pythons_json() {
        let open = guarded_app(None);
        assert_eq!(hit(&open, "POST", "/api/thing", None, None).await, StatusCode::OK);

        let app = guarded_app(Some("tok123"));
        let res = app
            .clone()
            .oneshot(HttpRequest::builder().uri("/api/thing").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            res.headers().get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({ "error": "unauthorized" }));
    }
}
