//! Apps inside amux run on their OWN origin, never the dashboard's.
//!
//! An app served on the dashboard's origin can call amux's API as the owner:
//! it shares the owner's cookies, can read the dashboard page, and can script a
//! window it opens (red-team, 2026-10-25). So the server binds a second
//! listener, `AMUX_APPS_PORT` (default: the amux port + 1), whose router serves
//! nothing but `/app/<id>/...`: an app a person added to the Apps tab, relayed from its
//! loopback port (app_relay.rs). On an amux Cloud server Caddy exposes it as
//! `https://<domain>:8443`; `AMUX_APPS_URL` names that address.
//!
//! The dashboard's sign-in does not reach that origin. Opening an app from
//! The Apps tab asks the dashboard origin for a one-time handoff
//! (`POST /api/page-links/<id>/open`, a signed-in person only); the apps origin
//! trades the 60-second, single-use token for a cookie scoped to that one app
//! (`amux_app_<id>`, HttpOnly, Path=/app/<id>/), held in memory here. Only a
//! cookie value this server issued for that id counts, so a cookie planted by
//! another app (cookies are shared across ports) opens nothing.
use super::app_relay;
use super::public_links::{self, Link};
use super::AppState;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOKEN_TTL: Duration = Duration::from_secs(60);
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

static TOKENS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);
static SESSIONS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);

/// The apps listener's port, or None when `AMUX_APPS_PORT=0` turns it off.
pub(crate) fn apps_port(main_port: u16) -> Option<u16> {
    match std::env::var("AMUX_APPS_PORT").ok().and_then(|v| v.trim().parse::<u16>().ok()) {
        Some(0) => None,
        Some(p) => Some(p),
        None => main_port.checked_add(1),
    }
}

/// The origin apps are served at, as the browser reaches it: `AMUX_APPS_URL`
/// (a cloud server's `https://<domain>:8443`), else the host the dashboard was
/// opened at on the apps port.
pub(crate) fn apps_url(headers: &HeaderMap, uri: &Uri) -> Result<String, &'static str> {
    let home = crate::config::amux_home();
    if let Some(u) = crate::api::settings::effective_env(&home, "AMUX_APPS_URL") {
        let u = u.trim().trim_end_matches('/');
        if u.starts_with("https://") && !u[8..].contains('/') && u.len() > 8 {
            return Ok(u.to_string());
        }
    }
    let port = apps_port(crate::config::canonical_port()).ok_or("AMUX_APPS_PORT=0 turns the apps origin off")?;
    // HTTP/2 carries the host as :authority, which reaches axum in the URI and
    // not as a Host header (the e2e browser's requests had no Host at all).
    let authority = uri.authority().map(|a| a.as_str().to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or(authority)
        .ok_or("the request names no host")?;
    let host = app_relay::host_only(host.split(',').next().unwrap_or("").trim());
    if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-[]:".contains(&b)) {
        return Err("the request's host is not a plain host name");
    }
    Ok(format!("https://{host}:{port}"))
}

fn with<T>(m: &Mutex<Option<HashMap<String, (String, Instant)>>>, f: impl FnOnce(&mut HashMap<String, (String, Instant)>) -> T) -> T {
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    map.retain(|_, (_, until)| *until > now);
    f(map)
}

/// A one-time handoff token for app `id`.
pub(crate) fn issue_token(id: &str) -> Option<String> {
    let t = public_links::new_id()?;
    with(&TOKENS, |m| m.insert(t.clone(), (id.to_string(), Instant::now() + TOKEN_TTL)));
    Some(t)
}

/// Spend a handoff token: true once, for the id it was issued for.
pub(crate) fn take_token(token: &str, id: &str) -> bool {
    with(&TOKENS, |m| m.remove(token)).is_some_and(|(for_id, _)| for_id == id)
}

fn new_session(id: &str) -> Option<String> {
    let s = public_links::new_id()?;
    with(&SESSIONS, |m| m.insert(s.clone(), (id.to_string(), Instant::now() + SESSION_TTL)));
    Some(s)
}

/// Whether any `amux_app_<id>` cookie on the request is a session this server
/// issued for `id`. Planted cookies of the same name are ignored, not trusted.
pub(crate) fn has_session(id: &str, headers: &HeaderMap) -> bool {
    let name = format!("amux_app_{id}");
    let values: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|all| all.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .filter(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
        .collect();
    !values.is_empty() && with(&SESSIONS, |m| values.iter().any(|v| m.get(v).is_some_and(|(for_id, _)| for_id == id)))
}

/// `POST /api/page-links/<id>/open` on the dashboard origin: a signed-in
/// person gets a one-time link that opens app `id` on the apps origin.
pub(crate) async fn open(State(state): State<AppState>, headers: HeaderMap, uri: Uri, AxPath(id): AxPath<String>) -> Response {
    let bad = |st: StatusCode, msg: &str| (st, Json(json!({ "error": msg }))).into_response();
    if public_links::person_of(&state, &headers).is_none() {
        tracing::warn!(id = %id, measured = true, n_considered = 1, verdict = "app_origin_open_refused",
            "an app handoff was asked for by something other than a signed-in person");
        return bad(StatusCode::FORBIDDEN, "only the owner, signed in to the dashboard, can open an app");
    }
    let Some(link) = public_links::approved_internal(&id) else {
        return bad(StatusCode::NOT_FOUND, "no approved app inside amux with that id");
    };
    let base = match apps_url(&headers, &uri) {
        Ok(b) => b,
        Err(why) => {
            tracing::warn!(id = %id, why, measured = true, n_considered = 1, verdict = "app_origin_unconfigured",
                "an app could not be opened: the apps origin's address is unknown");
            return bad(StatusCode::SERVICE_UNAVAILABLE, &format!("the apps origin is not configured on this server: {why}"));
        }
    };
    let Some(token) = issue_token(&link.id) else {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "no secure randomness available");
    };
    tracing::info!(id = %link.id, port = link.port, measured = true, n_considered = 1, verdict = "app_origin_handoff_issued",
        "one-time link to an app inside amux issued");
    Json(json!({ "url": format!("{base}/app/{}/?amux_t={token}", link.id), "expires_in_s": TOKEN_TTL.as_secs() })).into_response()
}

/// The apps listener's whole router: `/app/<id>/...` and nothing else.
pub(crate) fn router() -> Router {
    Router::new()
        .route("/app/{id}", axum::routing::any(serve))
        .route("/app/{id}/", axum::routing::any(serve))
        .route("/app/{id}/{*rest}", axum::routing::any(serve))
        .fallback(|| async { (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "Not found.\n") })
}

fn refuse(status: StatusCode, title: &str, body: &str, why: &'static str, id: &str) -> Response {
    tracing::info!(id = %id, why, measured = true, n_considered = 1, verdict = "app_origin_refused", "apps origin refused a request");
    app_relay::page(status, title, body)
}

/// `/app/<id>/a/b?x=1` -> (id, "/a/b?x=1"), with the handoff token taken out
/// of the query and returned on its own.
pub(crate) fn parse_app(uri: &Uri) -> Option<(String, String, Option<String>)> {
    let rest = uri.path().strip_prefix("/app/")?;
    let (id, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut token = None;
    let kept: Vec<&str> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter(|kv| match kv.strip_prefix("amux_t=") {
            Some(t) => {
                token = Some(t.to_string());
                false
            }
            None => true,
        })
        .collect();
    let mut tail = tail.to_string();
    if !kept.is_empty() {
        tail.push('?');
        tail.push_str(&kept.join("&"));
    }
    Some((id.to_string(), tail, token))
}

async fn serve(method: Method, headers: HeaderMap, uri: Uri, body: Body) -> Response {
    let Some((id, tail, token)) = parse_app(&uri) else { return StatusCode::NOT_FOUND.into_response() };
    let open_from_pages = "<p>Open this app from the Apps tab in amux. A link to it works only for the \
        signed-in owner, through the Apps tab.</p>";
    if !public_links::valid_id(&id) {
        return refuse(StatusCode::NOT_FOUND, "Not an app", open_from_pages, "malformed_id", &id);
    }
    let Some(link): Option<Link> = public_links::approved_internal(&id) else {
        return refuse(StatusCode::NOT_FOUND, "This app is not in Apps", open_from_pages, "not_approved", &id);
    };
    let is_file = !link.path.is_empty();
    if !is_file && public_links::refused_port(link.port).is_some() {
        return refuse(StatusCode::FORBIDDEN, "amux does not open this port", open_from_pages, "refused_port", &id);
    }
    if let Some(t) = token {
        if !take_token(&t, &id) {
            return refuse(StatusCode::FORBIDDEN, "This link was used already",
                "<p>An app link from the Apps tab works once, for a minute. Open the app from the Apps tab again.</p>", "token_spent", &id);
        }
        let Some(session) = new_session(&id) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        tracing::info!(id = %id, measured = true, n_considered = 1, verdict = "app_origin_session_started",
            "an app inside amux was opened from the Apps tab");
        let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, format!("/app/{id}{tail}"))]).into_response();
        let cookie = format!("amux_app_{id}={session}; Path=/app/{id}/; Max-Age={}; HttpOnly; Secure; SameSite=Lax", SESSION_TTL.as_secs());
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            r.headers_mut().insert(header::SET_COOKIE, v);
        }
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return r;
    }
    if !has_session(&id, &headers) {
        return refuse(StatusCode::FORBIDDEN, "Open this app from the Apps tab", open_from_pages, "no_session", &id);
    }
    if is_file {
        if uri.path() == format!("/app/{id}") {
            return axum::response::Redirect::permanent(&format!("/app/{id}/")).into_response();
        }
        let r = public_links::serve_link_file(&link, uri.path(), &tail, &method, &headers).await;
        return frameable_by_dashboard(r, &headers, &uri, &id);
    }
    let Some(local) = app_relay::local_listener(link.port).await else {
        return refuse(StatusCode::BAD_GATEWAY, "This app is not running",
            &format!("<p>Nothing is listening on port {} on this machine. Ask the worker that runs it to start it again.</p>", link.port),
            "not_running", &id);
    };
    let r = app_relay::relay(local, link.port, &format!("/app/{id}"), &tail, method, &headers, body).await;
    frameable_by_dashboard(r, &headers, &uri, &id)
}

/// The dashboard origins that open apps: the configured or learned public
/// address, and the apps origin's host on amux's own port.
fn dashboard_origins(headers: &HeaderMap, uri: &Uri) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(u) = public_links::public_url(&crate::config::amux_home()) {
        out.push(u);
    }
    // HTTP/2 carries the host as :authority, in the URI, with no Host header.
    let authority = uri.authority().map(|a| a.as_str().to_string());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).map(str::to_string).or(authority).unwrap_or_default();
    let host = app_relay::host_only(&host);
    if !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-[]:".contains(&b)) {
        out.push(format!("https://{host}:{}", crate::config::canonical_port()));
    }
    out
}

/// Pages shows an app in a frame on the dashboard's origin, a different port
/// from this one. An app that forbids framing (`X-Frame-Options: SAMEORIGIN`,
/// or a CSP `frame-ancestors`) rendered as a blank box in the Apps tab (sandbox,
/// 2026-10-26: mtp-demo's sign-in page). The owner chose to open it inside
/// amux, so the dashboard may frame it, and nothing else may.
fn frameable_by_dashboard(mut r: Response, headers: &HeaderMap, uri: &Uri, id: &str) -> Response {
    let h = r.headers_mut();
    let had_xfo = h.remove(header::X_FRAME_OPTIONS).is_some();
    let csp: Vec<HeaderValue> = h.get_all(header::CONTENT_SECURITY_POLICY).iter().cloned().collect();
    h.remove(header::CONTENT_SECURITY_POLICY);
    let mut had_ancestors = false;
    for v in csp {
        let Ok(v) = v.to_str() else { continue };
        let kept: Vec<&str> = v
            .split(';')
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .filter(|d| {
                let fa = d.to_ascii_lowercase().starts_with("frame-ancestors");
                had_ancestors |= fa;
                !fa
            })
            .collect();
        if let Ok(v) = HeaderValue::from_str(&kept.join("; ")) {
            if !kept.is_empty() {
                h.append(header::CONTENT_SECURITY_POLICY, v);
            }
        }
    }
    let origins = dashboard_origins(headers, uri);
    let ancestors = if origins.is_empty() { "frame-ancestors 'self'".to_string() } else { format!("frame-ancestors {}", origins.join(" ")) };
    if let Ok(v) = HeaderValue::from_str(&ancestors) {
        h.append(header::CONTENT_SECURITY_POLICY, v);
    }
    if had_xfo || had_ancestors {
        tracing::info!(id = %id, had_xfo, had_ancestors, measured = true, n_considered = 1, verdict = "app_origin_framing_replaced",
            "an app inside amux forbade framing; only the dashboard may frame it now");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handoff_token_opens_its_own_app_once() {
        let a = "a".repeat(32);
        let b = "b".repeat(32);
        let t = issue_token(&a).unwrap();
        assert!(!take_token(&t, &b), "a token for one app opened another");
        let t = issue_token(&a).unwrap();
        assert!(take_token(&t, &a));
        assert!(!take_token(&t, &a), "a token worked twice");
        assert!(!take_token("nope", &a));
    }

    #[test]
    fn only_a_session_this_server_issued_for_that_app_counts() {
        let a = "c".repeat(32);
        let b = "d".repeat(32);
        let s = new_session(&a).unwrap();
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("amux_app_{a}=planted; amux_app_{a}={s}").parse().unwrap());
        assert!(has_session(&a, &h), "the issued value among planted ones");
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("amux_app_{a}=planted").parse().unwrap());
        assert!(!has_session(&a, &h), "a planted cookie opened the app");
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("amux_app_{b}={s}").parse().unwrap());
        assert!(!has_session(&b, &h), "a session for one app opened another");
    }

    #[test]
    fn the_apps_origin_is_the_dashboard_host_on_the_apps_port() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "localhost:18891".parse().unwrap());
        let u = apps_url(&h, &Uri::from_static("/api/page-links/x/open"));
        assert!(u.as_deref().is_ok_and(|u| u.starts_with("https://localhost:")), "{u:?}");
        // HTTP/2: no Host header, the authority is in the URI.
        let u = apps_url(&HeaderMap::new(), &Uri::from_static("https://sandbox.example:8824/api/page-links/x/open"));
        assert!(u.as_deref().is_ok_and(|u| u.starts_with("https://sandbox.example:")), "{u:?}");
    }

    #[test]
    fn only_the_dashboard_may_frame_an_app_whatever_the_app_said() {
        let mut r = Response::new(Body::empty());
        r.headers_mut().insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
        r.headers_mut().insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'self'; frame-ancestors 'none'"));
        // HTTP/2: no Host header, the authority is in the URI (the e2e browser).
        let u = Uri::from_static("https://sandbox.example:8443/app/x/");
        let r = frameable_by_dashboard(r, &HeaderMap::new(), &u, "x");
        assert!(r.headers().get(header::X_FRAME_OPTIONS).is_none(), "X-Frame-Options kept: the Apps tab shows a blank box");
        let csp: Vec<&str> = r.headers().get_all(header::CONTENT_SECURITY_POLICY).iter().filter_map(|v| v.to_str().ok()).collect();
        assert!(csp.contains(&"default-src 'self'"), "the app's own policy was dropped: {csp:?}");
        assert!(!csp.iter().any(|c| c.contains("'none'")), "the app's frame-ancestors kept: {csp:?}");
        let fa = csp.iter().find(|c| c.starts_with("frame-ancestors")).expect("no frame-ancestors: any site may frame it");
        assert!(fa.contains("https://sandbox.example:") && !fa.contains('*'), "{fa}");
    }

    #[test]
    fn the_token_leaves_the_query_and_the_rest_stays() {
        let u: Uri = "/app/abc/x/y?amux_t=tok&v=1".parse().unwrap();
        assert_eq!(parse_app(&u), Some(("abc".into(), "/x/y?v=1".into(), Some("tok".into()))));
        let u: Uri = "/app/abc".parse().unwrap();
        assert_eq!(parse_app(&u), Some(("abc".into(), "/".into(), None)));
    }
}
