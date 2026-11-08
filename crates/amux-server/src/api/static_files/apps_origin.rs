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
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOKEN_TTL: Duration = Duration::from_secs(60);
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

static TOKENS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);
static SESSIONS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);
/// Site views: a folder of HTML the owner opened in the Files tab, served as a
/// small website on the apps origin under an id that is its own key (view id
/// -> the folder). Held in memory; the viewer asks for a new one when it lapses.
static VIEWS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);

/// The script every HTML page on the apps origin loads (app_viewer.js). A
/// hidden name, so it never shadows a shared file (hidden files are never
/// served) and is unlikely to shadow an app's own route.
const VIEWER_PATH: &str = "/.amux-viewer.js";
const VIEWER_JS: &str = include_str!("app_viewer.js");
/// HTML larger than this goes out without the viewer script.
const INJECT_MAX: usize = 16 * 1024 * 1024;

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
    if tail.split('?').next() == Some(VIEWER_PATH) {
        return viewer_js();
    }
    if let Some(root) = view_root(&id) {
        return serve_view(&id, &root, &uri, &tail, &method, &headers).await;
    }
    let Some(link): Option<Link> = public_links::approved_internal(&id) else {
        // A site view that lapsed (or a server restart dropped it) looks like
        // an unknown id. Inside the viewer, ask it to open the page again.
        if fetch_dest(&headers) == "iframe" {
            return reopen_in_viewer(&id, &tail, "view_gone");
        }
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
        // The session is gone (it lapsed, the server restarted, or the page
        // was opened in a tab of its own). A page inside the viewer asks the
        // viewer to open it again; a page opened in a tab goes to the Apps
        // tab, which opens the app at that page. Before this, both were a dead
        // end ("Open this app from the Apps tab"), and every link the app had
        // opened in a new tab landed on it (owner, 2026-10-29).
        match (fetch_dest(&headers), &method) {
            ("iframe", _) => return reopen_in_viewer(&id, &tail, "no_session"),
            ("document", &Method::GET) => {
                if let Some(to) = dashboard_origins(&headers, &uri).into_iter().next() {
                    tracing::info!(id = %id, measured = true, n_considered = 1, verdict = "app_origin_sent_to_apps_tab",
                        "an app page opened without its session was sent to the Apps tab to open there");
                    return (StatusCode::SEE_OTHER, [(header::LOCATION, format!("{to}/#view=apps&open={id}&at={}", pct(&tail)))]).into_response();
                }
            }
            _ => {}
        }
        return refuse(StatusCode::FORBIDDEN, "Open this app from the Apps tab", open_from_pages, "no_session", &id);
    }
    if is_file {
        if uri.path() == format!("/app/{id}") {
            return axum::response::Redirect::permanent(&format!("/app/{id}/")).into_response();
        }
        let r = public_links::serve_link_file(&link, uri.path(), &tail, &method, &headers).await;
        let r = with_viewer(r, &id, &method).await;
        return frameable_by_dashboard(r, &headers, &uri, &id);
    }
    let Some(local) = app_relay::local_listener(link.port).await else {
        return refuse(StatusCode::BAD_GATEWAY, "This app is not running",
            &format!("<p>Nothing is listening on port {} on this machine. Ask the worker that runs it to start it again.</p>", link.port),
            "not_running", &id);
    };
    let r = app_relay::relay(local, link.port, &format!("/app/{id}"), &tail, method.clone(), &headers, body).await;
    let r = with_viewer(r, &id, &method).await;
    frameable_by_dashboard(r, &headers, &uri, &id)
}

fn fetch_dest(headers: &HeaderMap) -> &str {
    headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// %XX-encode all but unreserved bytes and '/', for a path carried in a URL.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

fn viewer_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        VIEWER_JS,
    )
        .into_response()
}

/// A page in the viewer whose session or view is gone asks the viewer (the
/// dashboard, which checks the message came from its own frame) to open the
/// same page again with a fresh handoff. Without a viewer it reads as before.
fn reopen_in_viewer(id: &str, tail: &str, why: &'static str) -> Response {
    tracing::info!(id = %id, why, measured = true, n_considered = 1, verdict = "app_origin_reopen_asked",
        "a page in the app viewer lost its session; the viewer was asked to open it again");
    let msg = json!({ "amux_app_reopen": 1, "id": id, "at": tail }).to_string().replace('<', "\\u003c");
    let body = format!(
        "<p>Opening this page again&hellip; If it does not open, open the app from the Apps tab in amux.</p>\
         <script>try{{parent.postMessage({msg},'*')}}catch(e){{}}</script>"
    );
    app_relay::page(StatusCode::FORBIDDEN, "Opening again", &body)
}

/// Put the viewer script into an HTML page: after `<head ...>`, else after
/// `<html ...>`, else after the doctype, never before it (that would switch
/// the page to quirks mode).
pub(crate) fn inject_viewer(html: &str, id: &str) -> String {
    let tag = format!("<script src=\"/app/{id}{VIEWER_PATH}\"></script>");
    let lower = html.to_ascii_lowercase();
    let after = |open: &str| -> Option<usize> {
        let mut from = 0;
        while let Some(i) = lower[from..].find(open) {
            let at = from + i + open.len();
            // `<header>` is not `<head>`.
            if lower[at..].starts_with(|c: char| c == '>' || c.is_ascii_whitespace()) {
                return lower[at..].find('>').map(|j| at + j + 1);
            }
            from = at;
        }
        None
    };
    let at = after("<head").or_else(|| after("<html")).or_else(|| after("<!doctype")).unwrap_or(0);
    let mut out = String::with_capacity(html.len() + tag.len());
    out.push_str(&html[..at]);
    out.push_str(&tag);
    out.push_str(&html[at..]);
    out
}

async fn with_viewer(r: Response, id: &str, method: &Method) -> Response {
    let h = r.headers();
    let is_html = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|ct| ct.starts_with("text/html"));
    let encoded = h.get(header::CONTENT_ENCODING).is_some_and(|v| v.as_bytes() != b"identity");
    let too_big = h.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok()).is_some_and(|n| n > INJECT_MAX);
    if r.status() != StatusCode::OK || method != Method::GET || !is_html || encoded || too_big {
        return r;
    }
    let (mut parts, body) = r.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, INJECT_MAX).await else {
        tracing::warn!(id = %id, measured = true, n_considered = 1, verdict = "app_viewer_inject_failed",
            "an HTML page on the apps origin could not be read to add the viewer script");
        return app_relay::page(StatusCode::BAD_GATEWAY, "The page stopped mid-response", "<p>Try again.</p>");
    };
    let html = inject_viewer(&String::from_utf8_lossy(&bytes), id);
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ETAG);
    Response::from_parts(parts, Body::from(html))
}

fn view_root(id: &str) -> Option<String> {
    with(&VIEWS, |m| m.get(id).map(|(root, _)| root.clone()))
}

/// A site view: the folder's files as a small website. The view id is the
/// key (128 random bits, minted only for a signed-in person, 12 hours), and
/// every page runs sandboxed as an opaque origin, like the Files tab's
/// preview always has, so a page from a downloaded folder cannot read the
/// owner's apps on this origin.
async fn serve_view(id: &str, root: &str, uri: &Uri, tail: &str, method: &Method, headers: &HeaderMap) -> Response {
    if uri.path() == format!("/app/{id}") {
        return axum::response::Redirect::permanent(&format!("/app/{id}/")).into_response();
    }
    let link = Link {
        id: id.to_string(),
        kind: "internal".into(),
        port: 0,
        path: root.to_string(),
        title: String::new(),
        created_by: "site_view".into(),
        created_at: 0,
        approved_by: "owner".into(),
    };
    let r = public_links::serve_link_file(&link, uri.path(), tail, method, headers).await;
    if r.status() == StatusCode::NOT_FOUND {
        tracing::info!(id = %id, measured = true, n_considered = 1, verdict = "app_site_view_not_found",
            "a site view asked for a page that is not in its folder");
    }
    let r = with_viewer(r, id, method).await;
    let mut r = frameable_by_dashboard(r, headers, uri, id);
    let h = r.headers_mut();
    h.append(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(SITE_SANDBOX));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.remove(header::SET_COOKIE);
    r
}

const SITE_SANDBOX: &str = "sandbox allow-scripts allow-popups allow-forms allow-popups-to-escape-sandbox allow-modals allow-downloads";

/// The folder a page's site lives in. Starts at the page's folder, climbs as
/// many levels as the page's own `../` links need (`../design-system/x.css`),
/// then on through parents that have their own index.html, stopping at a git
/// root. Never HOME, `/` or a top-level folder: a page directly in one of
/// those is served alone.
pub(crate) fn site_root(file: &Path, home: &Path) -> PathBuf {
    let shallow = |p: &Path| p == home || p.components().count() <= 2;
    let Some(dir) = file.parent() else { return file.to_path_buf() };
    if shallow(dir) {
        return file.to_path_buf();
    }
    let need = std::fs::metadata(file)
        .ok()
        .filter(|m| m.len() <= 2 * 1024 * 1024)
        .and_then(|_| std::fs::read_to_string(file).ok())
        .map(|t| levels_up(&t))
        .unwrap_or(0);
    let mut root = dir.to_path_buf();
    for _ in 0..need {
        match root.parent() {
            Some(p) if !shallow(p) => root = p.to_path_buf(),
            _ => break,
        }
    }
    for _ in 0..4 {
        if root.join(".git").exists() {
            break;
        }
        match root.parent() {
            Some(p) if !shallow(p) && p.join("index.html").is_file() => root = p.to_path_buf(),
            _ => break,
        }
    }
    root
}

/// The most `../` a page's src/href/action attributes climb (at most 4).
fn levels_up(html: &str) -> usize {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#"(?i)\b(?:src|href|action)\s*=\s*["']?((?:\./)?(?:\.\./)+)"#).expect("levels regex"));
    re.captures_iter(html).map(|c| c[1].matches("../").count()).max().unwrap_or(0).min(4)
}

#[derive(serde::Deserialize)]
pub(crate) struct SiteReq {
    path: String,
}

/// `POST /api/page-links/site-view` `{path}` on the dashboard origin: a
/// signed-in person opens an HTML file from the Files tab as part of its
/// folder's website, on the apps origin. Answers the URL to frame and the
/// folder it serves, or the reason it cannot (the dashboard then falls back
/// to the single-file preview).
pub(crate) async fn site_view(State(state): State<AppState>, headers: HeaderMap, uri: Uri, Json(req): Json<SiteReq>) -> Response {
    let bad = |st: StatusCode, msg: &str| (st, Json(json!({ "error": msg }))).into_response();
    if public_links::person_of(&state, &headers).is_none() {
        tracing::warn!(measured = true, n_considered = 1, verdict = "app_site_view_refused",
            "a site view was asked for by something other than a signed-in person");
        return bad(StatusCode::FORBIDDEN, "only the owner, signed in to the dashboard, can open a site view");
    }
    let Ok(file) = Path::new(req.path.trim()).canonicalize() else {
        return bad(StatusCode::NOT_FOUND, "no such file");
    };
    if !file.is_file() {
        return bad(StatusCode::BAD_REQUEST, "give the path of a file");
    }
    if let Some(why) = public_links::file_refused(&file) {
        tracing::info!(path = %file.display(), why, measured = true, n_considered = 1, verdict = "app_site_view_refused",
            "a site view was refused for a path amux does not serve");
        return bad(StatusCode::FORBIDDEN, why);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).and_then(|h| h.canonicalize().ok()).unwrap_or_default();
    let root = Some(site_root(&file, &home)).filter(|r| public_links::file_refused(r).is_none()).unwrap_or_else(|| file.clone());
    let rel: Vec<String> = file
        .strip_prefix(&root)
        .map(|r| r.components().map(|c| pct(&c.as_os_str().to_string_lossy()).replace('/', "%2F")).collect())
        .unwrap_or_default();
    let base = match apps_url(&headers, &uri) {
        Ok(b) => b,
        Err(why) => return bad(StatusCode::SERVICE_UNAVAILABLE, &format!("the apps origin is not configured on this server: {why}")),
    };
    let Some(id) = public_links::new_id() else {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "no secure randomness available");
    };
    let root_s = root.to_string_lossy().into_owned();
    with(&VIEWS, |m| m.insert(id.clone(), (root_s.clone(), Instant::now() + SESSION_TTL)));
    tracing::info!(id = %id, root = %root_s, page = %rel.join("/"), measured = true, n_considered = 1, verdict = "app_site_view_issued",
        "an HTML file was opened as its folder's website on the apps origin");
    Json(json!({
        "url": format!("{base}/app/{id}/{}", rel.join("/")),
        "root": root_s,
        "id": id,
        "expires_in_s": SESSION_TTL.as_secs(),
    }))
    .into_response()
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

    #[test]
    fn the_viewer_script_goes_in_the_head_and_never_before_the_doctype() {
        let id = "f".repeat(32);
        let tag = format!("<script src=\"/app/{id}/.amux-viewer.js\"></script>");
        let out = inject_viewer("<!DOCTYPE html><html lang=en><HEAD><title>t</title></HEAD><body><header>h</header></body></html>", &id);
        assert!(out.starts_with(&format!("<!DOCTYPE html><html lang=en><HEAD>{tag}<title>")), "{out}");
        // `<header>` is not `<head>`.
        let out = inject_viewer("<!doctype html><body><header>x</header></body>", &id);
        assert!(out.starts_with(&format!("<!doctype html>{tag}<body><header>")), "{out}");
        assert_eq!(inject_viewer("<p>bare</p>", &id), format!("{tag}<p>bare</p>"));
    }

    #[test]
    fn a_site_is_the_folder_its_pages_climb_to_and_never_home() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().canonicalize().unwrap();
        let site = h.join("work/demos");
        std::fs::create_dir_all(site.join("landing")).unwrap();
        std::fs::create_dir_all(site.join("plain")).unwrap();
        std::fs::write(site.join("index.html"), "<a href=\"landing/core.html\">x</a>").unwrap();
        std::fs::write(site.join("landing/core.html"), "<link href=\"../design/a.css\"><script src=\"./../shared/k.js\"></script>").unwrap();
        std::fs::write(site.join("plain/page.html"), "<p>no links</p>").unwrap();
        assert_eq!(site_root(&site.join("landing/core.html"), &h), site, "its ../ links need the parent");
        assert_eq!(site_root(&site.join("index.html"), &h), site);
        // No ../ links, but the parent is the site's home page.
        assert_eq!(site_root(&site.join("plain/page.html"), &h), site);
        // A page directly in HOME is served alone, never all of HOME.
        std::fs::write(h.join("solo.html"), "<a href=\"../../etc/x\">x</a>").unwrap();
        assert_eq!(site_root(&h.join("solo.html"), &h), h.join("solo.html"));
        // Climbing stops below HOME however many ../ a page asks for.
        std::fs::write(site.join("landing/deep.html"), "<a href=\"../../../../x\">x</a>").unwrap();
        assert_eq!(site_root(&site.join("landing/deep.html"), &h), h.join("work"));
        assert_eq!(levels_up("<img SRC='../../a.png'><a href=\"../b\">"), 2);
        assert_eq!(levels_up("<a href=\"https://x/../y\">"), 0);
    }
}
