//! `/api/computer/*`: CUA computer-use sandboxes, one per lane (AMUX-5300).
//! Mechanics, caps and the reasoning behind them live in
//! `integrations::computer`; this file is request mapping and verdict lines.
//!
//! Routes (nested at `/api/computer`), every one keyed by `X-Amux-Session`
//! (or a `session` body/query field):
//! - `POST /start`                      launch or reuse the lane's sandbox
//! - `GET  /status`                     every sandbox, fleet-wide, plus caps
//! - `GET  /context`                    ladder, verbs, profiles (text + JSON)
//! - `POST /screenshot`                 PNG to ~/.amux/computer-screenshots, returns {path}
//! - `POST /click|/double_click|/move {x,y}`
//! - `POST /type {text}` / `POST /key {key}` / `POST /scroll {dx,dy}`
//! - `POST /open {url, profile?}`       Chromium in the sandbox, optionally signed in
//! - `POST /stop`                       remove the lane's sandbox
//! - anything else                      the route catalog as a 404

use super::AppState;
use crate::integrations::computer as cu;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/start", post(start))
        .route("/status", get(status))
        .route("/context", get(context))
        .route("/screenshot", post(screenshot).get(screenshot_get))
        .route(
            "/click",
            post(|h: HeaderMap, b: Option<Json<Value>>| act("click", h, b)),
        )
        .route(
            "/double_click",
            post(|h: HeaderMap, b: Option<Json<Value>>| act("double_click", h, b)),
        )
        .route(
            "/move",
            post(|h: HeaderMap, b: Option<Json<Value>>| act("move", h, b)),
        )
        .route(
            "/type",
            post(|h: HeaderMap, b: Option<Json<Value>>| act("type", h, b)),
        )
        .route(
            "/key",
            post(|h: HeaderMap, b: Option<Json<Value>>| act("key", h, b)),
        )
        .route(
            "/scroll",
            post(|h: HeaderMap, b: Option<Json<Value>>| act("scroll", h, b)),
        )
        .route("/open", post(open))
        .route("/stop", post(stop))
        .fallback(catalog)
}

const CATALOG: &[&str] = &[
    "POST /api/computer/start",
    "GET  /api/computer/status",
    "GET  /api/computer/context",
    "POST /api/computer/screenshot",
    "POST /api/computer/click {x,y}",
    "POST /api/computer/double_click {x,y}",
    "POST /api/computer/move {x,y}",
    "POST /api/computer/type {text}",
    "POST /api/computer/key {key}   (enter, tab, ctrl+l)",
    "POST /api/computer/scroll {dx,dy}   (dy > 0 scrolls down)",
    "POST /api/computer/open {url, profile?}",
    "POST /api/computer/stop",
];

async fn catalog() -> Response {
    err(
        StatusCode::NOT_FOUND,
        json!({ "error": "no such /api/computer route", "routes": CATALOG }),
    )
}

fn err(code: StatusCode, body: Value) -> Response {
    (code, Json(body)).into_response()
}

/// The lane a call acts for: explicit `session` field, then the
/// `X-Amux-Session` header. No default: a sandbox is per-lane, and an
/// anonymous caller would share (and stop) one bucket with every other
/// anonymous caller.
fn lane(body: Option<&Value>, headers: &HeaderMap) -> Result<String, Box<Response>> {
    body.and_then(|b| b.get("session"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-amux-session")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| {
            tracing::warn!("[computer] verdict=refused reason=unattributed (no X-Amux-Session)");
            Box::new(err(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": "which lane? send X-Amux-Session (the amux CLI does) or a `session` field",
                }),
            ))
        })
}

fn code(n: u16) -> StatusCode {
    StatusCode::from_u16(n).unwrap_or(StatusCode::BAD_GATEWAY)
}

/// The lane's running sandbox, or the response explaining why there is none.
async fn sandbox_for(lane: &str) -> Result<cu::Sandbox, Box<Response>> {
    match cu::find(lane).await {
        Ok(Some(b)) => Ok(b),
        Ok(None) => Err(Box::new(err(
            StatusCode::CONFLICT,
            json!({
                "error": format!("lane {lane:?} has no running sandbox"),
                "hint": "amux computer start (POST /api/computer/start)",
            }),
        ))),
        Err(e) => Err(Box::new(err(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": format!("docker: {e}"), "hint": "is colima running? GET /api/computer/status" }),
        ))),
    }
}

// ---------------------------------------------------------------------------

async fn start(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v);
    let lane = match lane(body.as_ref(), &headers) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let home = crate::api::session_verbs::home();
    match cu::start(&home, &lane).await {
        Ok(s) => {
            tracing::info!(
                "[computer] verdict={} lane={lane} name={} image={} built={} colima_started={} ready_s={}",
                if s.reused { "reused" } else { "started" },
                s.sandbox.name,
                s.image,
                s.image_built,
                s.colima_started,
                s.ready_after_s
            );
            let ctx = context_payload().await;
            let vnc = s.sandbox.vnc_port.map(|p| format!("http://127.0.0.1:{p}/"));
            Json(json!({
                "ok": true,
                "lane": lane,
                "reused": s.reused,
                "name": s.sandbox.name,
                "image": s.image,
                "image_built": s.image_built,
                "colima_started": s.colima_started,
                "ready_after_s": s.ready_after_s,
                "vnc_url": vnc,
                "limits": cu::limits(),
                // Shown once, here, so a worker learns the verbs and which
                // profile carries which login without a second call.
                "context": ctx["text"],
            }))
            .into_response()
        }
        Err(r) => {
            let detail = r
                .body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            tracing::warn!(
                "[computer] verdict=refused lane={lane} reason={} status={} detail={detail}",
                r.reason,
                r.status,
            );
            let mut b = r.body;
            if let Some(o) = b.as_object_mut() {
                o.insert("verdict".into(), json!("refused"));
                o.insert("reason".into(), json!(r.reason));
            }
            err(code(r.status), b)
        }
    }
}

async fn stop(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v);
    let lane = match lane(body.as_ref(), &headers) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let name = cu::container_name(&lane);
    match cu::stop_name(&name).await {
        Ok(removed) => {
            tracing::info!(
                "[computer] verdict={} lane={lane} name={name} by=request",
                if removed { "stopped" } else { "stop_noop" }
            );
            Json(json!({ "ok": true, "lane": lane, "name": name, "removed": removed }))
                .into_response()
        }
        Err(e) => {
            tracing::warn!("[computer] verdict=stop_failed lane={lane} name={name}: {e}");
            err(StatusCode::BAD_GATEWAY, json!({ "error": e.to_string() }))
        }
    }
}

async fn status() -> Response {
    let docker = cu::probe_docker().await;
    let lim = cu::limits();
    if !docker.up {
        // `measured` says whether the listing ran; an empty list from a
        // stopped daemon is not "no sandboxes".
        return Json(json!({
            "measured": false,
            "why_unmeasured": docker.error.clone(),
            "docker": docker,
            "limits": lim,
            "sandboxes": [],
        }))
        .into_response();
    }
    let boxes = match cu::list().await {
        Ok(b) => b,
        Err(e) => {
            return Json(json!({
                "measured": false,
                "why_unmeasured": e.to_string(),
                "docker": docker,
                "limits": lim,
                "sandboxes": [],
            }))
            .into_response()
        }
    };
    let now = cu::now();
    let rows: Vec<Value> = boxes
        .iter()
        .map(|b| {
            let idle = cu::idle_for(now, b.started_at, cu::last_action(&b.name));
            json!({
                "lane": b.lane,
                "name": b.name,
                "state": b.state,
                "image": b.image,
                "started_at": b.started_at,
                "idle_s": idle,
                "idle_stop_in_s": match (idle, lim.idle_s) {
                    (_, 0) => Value::Null,
                    (Some(i), s) => json!((s as i64 - i).max(0)),
                    (None, _) => Value::Null,
                },
                "api_port": b.api_port,
                "vnc_url": b.vnc_port.map(|p| format!("http://127.0.0.1:{p}/")),
                "chromium_devtools_port": b.cdp_port,
            })
        })
        .collect();
    let running = boxes.iter().filter(|b| b.running()).count();
    Json(json!({
        "measured": true,
        "n_considered": boxes.len(),
        "running": running,
        "slots_free": lim.max.saturating_sub(running),
        "docker": docker,
        "limits": lim,
        "sandboxes": rows,
    }))
    .into_response()
}

async fn context() -> Response {
    Json(context_payload().await).into_response()
}

/// Saved amux profiles and the sites they hold cookies for, derived from each cookie jar (the same
/// derivation `/api/browser/profiles` publishes as `signed_in_to`).
async fn context_payload() -> Value {
    let profiles = tokio::task::spawn_blocking(|| {
        let home = crate::integrations::browser::amux_home();
        let chrome_dir = crate::integrations::browser::chrome_user_data_dir();
        crate::integrations::browser::list_profiles(&home, false)
            .into_iter()
            .filter_map(|p| {
                let dir =
                    crate::integrations::browser::resolve_profile_dir(&home, &chrome_dir, &p.name);
                let (n, hosts) = super::browser::profile_contents(&dir);
                (n.unwrap_or(0) > 0 && !hosts.is_empty()).then(|| {
                    // Top six hosts by cookie count; the full list is on
                    // /api/browser/profiles.
                    (p.name, hosts.into_iter().take(6).collect::<Vec<_>>())
                })
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let lim = cu::limits();
    let cards:Vec<Value>=crate::integrations::browser::list_profiles(&crate::integrations::browser::amux_home(),false).iter().filter(|p|!matches!(p.role.as_str(),"test"|"deprecated")).map(super::browser::profile_selection_card).collect();
    json!({
        "text": format!("{}\nProfile selection cards (check scope before use): {}",cu::context_text(&profiles, &lim),serde_json::to_string(&cards).unwrap_or_default()),
        "selection_cards":cards,
        "profiles": profiles.iter().map(|(n, h)| json!({"name": n, "top_sites": h})).collect::<Vec<_>>(),
        "profiles_note": "top six sites per profile by cookie count; full lists at GET /api/browser/profiles",
        "ladder": [
            "1. amux browser: /api/browser/* on amux Chrome profiles",
            "2. CDP using the owner-selected Chrome fallback in Browser tab",
            "3. amux computer: this sandbox, only when 1 and 2 cannot reach it",
        ],
        "routes": CATALOG,
        "limits": lim,
    })
}

async fn act(verb: &'static str, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(json!({}));
    let lane = match lane(Some(&body), &headers) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let (command, params) = match cu::map_action(verb, &body) {
        Ok(x) => x,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let b = match sandbox_for(&lane).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let Some(api) = b.api_port else {
        return err(
            StatusCode::BAD_GATEWAY,
            json!({ "error": "sandbox publishes no computer-server port" }),
        );
    };
    cu::touch(&b.name);
    match cu::cmd(api, &command, params.clone()).await {
        Ok(_) => Json(
            json!({ "ok": true, "lane": lane, "verb": verb, "command": command, "params": params }),
        )
        .into_response(),
        Err(e) => {
            tracing::warn!(
                "[computer] verdict=action_failed lane={lane} verb={verb} command={command}: {e}"
            );
            err(
                StatusCode::BAD_GATEWAY,
                json!({ "error": e.to_string(), "command": command }),
            )
        }
    }
}

/// Screenshots kept per lane. Older ones are deleted, and the response says
/// how many, so the retention is never silent.
const KEEP_SHOTS: usize = 30;

async fn screenshot_get(headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let body = q.get("session").map(|s| json!({ "session": s }));
    screenshot(headers, body.map(Json)).await
}

async fn screenshot(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    use base64::Engine;
    let body = body.map(|Json(v)| v);
    let lane = match lane(body.as_ref(), &headers) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let b = match sandbox_for(&lane).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    let Some(api) = b.api_port else {
        return err(
            StatusCode::BAD_GATEWAY,
            json!({ "error": "sandbox publishes no computer-server port" }),
        );
    };
    cu::touch(&b.name);
    let v = match cu::cmd(api, "screenshot", json!({ "format": "png" })).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("[computer] verdict=screenshot_failed lane={lane}: {e}");
            return err(StatusCode::BAD_GATEWAY, json!({ "error": e.to_string() }));
        }
    };
    let Some(bytes) = v
        .get("image_data")
        .and_then(Value::as_str)
        .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
    else {
        return err(
            StatusCode::BAD_GATEWAY,
            json!({ "error": "screenshot reply carried no decodable image_data" }),
        );
    };
    let dir = cu::screenshot_dir();
    let _ = std::fs::create_dir_all(&dir);
    let stem = crate::integrations::browser::safe_file_component(&lane);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{stem}-{ms}.png"));
    if let Err(e) = std::fs::write(&path, &bytes) {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("writing {}: {e}", path.display()) }),
        );
    }
    let pruned = prune_shots(&dir, &stem);
    let (w, h) = png_size(&bytes).unzip();
    Json(json!({
        "ok": true,
        "lane": lane,
        "path": path.display().to_string(),
        "size": bytes.len(),
        "width": w,
        "height": h,
        "kept_per_lane": KEEP_SHOTS,
        "pruned": pruned,
    }))
    .into_response()
}

fn prune_shots(dir: &std::path::Path, stem: &str) -> usize {
    let prefix = format!("{stem}-");
    let mut shots: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.strip_prefix(&prefix)
                    .and_then(|r| r.strip_suffix(".png"))
                    .is_some_and(|ts| ts.chars().all(|c| c.is_ascii_digit()))
            })
        })
        .collect();
    shots.sort();
    let excess = shots.len().saturating_sub(KEEP_SHOTS);
    shots
        .iter()
        .take(excess)
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// Width and height from a PNG's IHDR chunk.
fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(b[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(b[20..24].try_into().ok()?);
    Some((w, h))
}

/// Where scoped profile access (AMUX-5307) plugs in: one call site, allow-all
/// until that lands.
/// The same rule the amux browser enforces (AMUX-5307): worker > group >
/// global allow/deny, default allow all, owner always allowed.
fn check_profile(lane: &str, profile: &str) -> Result<(), Box<super::browser_scope::ProfileDenied>> {
    super::browser_scope::profile_allowed(lane, profile)
}

async fn open(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(json!({}));
    let lane = match lane(Some(&body), &headers) {
        Ok(l) => l,
        Err(r) => return *r,
    };
    let Some(url) = body
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "open needs a url" }),
        );
    };
    let profile = body
        .get("profile")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if let Some(p) = profile {
        if let Err(denied) = check_profile(&lane, p) {
            tracing::warn!(
                "[computer] verdict=refused lane={lane} reason=profile_denied profile={p}: {}",
                denied.reason
            );
            return denied.response();
        }
    }
    let b = match sandbox_for(&lane).await {
        Ok(b) => b,
        Err(r) => return *r,
    };
    cu::touch(&b.name);
    let (port, launched) = match cu::ensure_chromium(&b).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("[computer] verdict=chromium_failed lane={lane}: {e}");
            return err(StatusCode::BAD_GATEWAY, json!({ "error": e.to_string() }));
        }
    };
    let mut cookies_out = Value::Null;
    if let Some(p) = profile {
        let (cookies, export) = match cu::export_profile_cookies(p, &lane).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(
                    "[computer] verdict=cookie_export_failed lane={lane} profile={p}: {e}"
                );
                return err(
                    StatusCode::BAD_GATEWAY,
                    json!({ "error": format!("reading profile {p:?} through the amux browser: {e}") }),
                );
            }
        };
        let params = cu::cookie_params(&cookies);
        let set = async {
            let mut c = cu::browser_cdp(port).await?;
            c.call(
                "Storage.setCookies",
                json!({ "cookies": params }),
                Duration::from_secs(20),
            )
            .await
        }
        .await;
        if let Err(e) = set {
            tracing::warn!("[computer] verdict=cookie_import_failed lane={lane} profile={p}: {e}");
            return err(
                StatusCode::BAD_GATEWAY,
                json!({ "error": format!("Storage.setCookies in the sandbox: {e}") }),
            );
        }
        tracing::info!(
            "[computer] verdict=profile_cookies lane={lane} profile={p} cookies={} source={}",
            export.cookies,
            export.source
        );
        cookies_out = json!({
            "profile": p,
            "imported": export.cookies,
            "read_from": export.source,
            "note": "cookies only; localStorage/IndexedDB logins do not carry over",
        });
    }
    // Target.createTarget, not `/json/new?url=`: the shared cdp_new_tab helper
    // sends `?url=<encoded>`, which Chromium reads as the literal URL
    // "url=https%3A..." and opens about:blank (seen on the first e2e run).
    let opened = async {
        let mut c = cu::browser_cdp(port).await?;
        c.call(
            "Target.createTarget",
            json!({ "url": url }),
            Duration::from_secs(20),
        )
        .await
    }
    .await;
    match opened {
        Ok(tab) => Json(json!({
            "ok": true,
            "lane": lane,
            "url": url,
            "chromium_launched": launched,
            "tab": tab.get("targetId"),
            "cookies": cookies_out,
            "next": "amux computer screenshot, then act on what it shows",
        }))
        .into_response(),
        Err(e) => err(
            StatusCode::BAD_GATEWAY,
            json!({ "error": format!("opening the tab: {e}") }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_size_reads_ihdr() {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend_from_slice(&1280u32.to_be_bytes());
        b.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(png_size(&b), Some((1280, 800)));
        assert_eq!(png_size(b"nope"), None);
    }

    #[test]
    fn lane_requires_attribution() {
        let mut h = HeaderMap::new();
        assert!(lane(None, &h).is_err());
        h.insert("x-amux-session", "w1".parse().unwrap());
        assert_eq!(lane(None, &h).unwrap(), "w1");
        assert_eq!(lane(Some(&json!({"session": "w2"})), &h).unwrap(), "w2");
    }

    #[test]
    fn prune_keeps_the_newest_per_lane_and_ignores_others() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..(KEEP_SHOTS + 3) {
            std::fs::write(d.path().join(format!("w-{:013}.png", 1000 + i)), b"x").unwrap();
        }
        std::fs::write(d.path().join("w-x-1.png"), b"x").unwrap();
        std::fs::write(d.path().join("other-1.png"), b"x").unwrap();
        assert_eq!(prune_shots(d.path(), "w"), 3);
        assert!(!d.path().join(format!("w-{:013}.png", 1000)).exists());
        assert!(d
            .path()
            .join(format!("w-{:013}.png", 1000 + KEEP_SHOTS + 2))
            .exists());
        assert!(
            d.path().join("w-x-1.png").exists(),
            "another lane's prefix-sharing file is untouched"
        );
        assert!(d.path().join("other-1.png").exists());
    }
}
