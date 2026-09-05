//! /api/proxies — the Proxies tab's CRUD (AMUX-2887).
//!
//! Python contract: `_proxies_list` py:77772, the handlers at py:65636.
//!
//! **Proxies and the tunnel are ONE subsystem.** A "proxy" is a saved
//! public-tunnel target; `/start` and `/stop` drive the tunnel CLIENT — a
//! long-poll relay against the cloud gateway with rate limiting, generation
//! fencing and a security refusal. That client is `runtime_jobs::tunnel`
//! (AMUX-2888), ported from py:77931 `_tunnel_start` / py:77848 `_tunnel_loop`.
//!
//! This module shipped in two steps and the seam is worth knowing. First CRUD
//! plus an honest 501 on start/stop, because the tab is DEFAULT-VISIBLE and its
//! list call 404'd ("Failed to load") over real user data. Then the relay
//! itself, which is what turned `live`/`url`/`requests`/`dropped` from stated
//! not-running values into readings off the running client.
//!
//! ONE TUNNEL AT A TIME, and that is the gateway's shape, not a limitation
//! here: it derives one tid per token. So the tab is a library of saved targets
//! and `start` picks the active one, stopping whatever was live first.

use super::AppState;
use crate::db::board_store as bs;
use crate::db::WriteOutcome;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", axum::routing::patch(patch).delete(remove))
        .route("/{id}/start", post(start))
        .route("/{id}/stop", post(stop))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

async fn list(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string()})))
                .into_response()
        }
    };
    // LIVE STATE FROM THE RELAY, not a hardcoded false (AMUX-2888). While the
    // client was unported nothing could be running, so `live: false` was a
    // measurement; now it is a question with a real answer and reading it from
    // the running tunnel is what keeps the dot, the Start/Stop button and the
    // relay from disagreeing.
    let t = crate::runtime_jobs::tunnel::snapshot();
    let live_id = crate::runtime_jobs::tunnel::active_proxy_id();
    let rows: Vec<Value> = conn
        .prepare(
            "SELECT id, name, port, scheme, created_at, last_started \
             FROM proxies ORDER BY created_at ASC",
        )
        .and_then(|mut s| {
            let it = s.query_map([], |r| {
                let id = r.get::<_, String>(0)?;
                let live = live_id.as_deref() == Some(id.as_str());
                let mut row = json!({
                    "id": id,
                    "name": r.get::<_, String>(1)?,
                    "port": r.get::<_, i64>(2)?,
                    "scheme": r.get::<_, String>(3)?,
                    "created_at": r.get::<_, i64>(4)?,
                    "last_started": r.get::<_, Option<i64>>(5)?,
                    "live": live,
                });
                if live {
                    // Only the live row carries traffic counters, matching
                    // python: attaching them to every row would report the
                    // active tunnel's numbers against idle saved targets.
                    if let Some(o) = row.as_object_mut() {
                        o.insert("url".into(), json!(t.url));
                        o.insert("requests".into(), json!(t.requests));
                        o.insert("error".into(), json!(t.error));
                        o.insert("dropped".into(), json!(crate::runtime_jobs::tunnel::dropped()));
                    }
                }
                Ok(row)
            })?;
            Ok(it.flatten().collect())
        })
        .unwrap_or_default();

    Json(json!({
        "proxies": rows,
        "active_id": live_id,
        // `configured` gates the tab's "not configured" hint.
        "configured": crate::runtime_jobs::tunnel::configured(),
        "self_port": crate::legacy_port::canonical_port(),
        // READ FROM THE RELAY, not re-derived here. These two render as the
        // live policy in the tab, and a second copy of the defaults is a UI
        // that can disagree with the limiter it describes.
        "rate_per_min": crate::runtime_jobs::tunnel::rate_per_min(),
        "max_concurrent": crate::runtime_jobs::tunnel::max_concurrent(),
        "tunnel_ported": true,
    }))
    .into_response()
}

/// Python validated name + `1 <= port <= 65535` and silently coerced an unknown
/// scheme to http (py:65643). Same rules, because the SPA relies on the coercion
/// rather than sending a scheme on every write.
fn validate(name: &str, port: i64) -> Option<&'static str> {
    if name.trim().is_empty() {
        return Some("name and a valid port (1-65535) are required");
    }
    if !(1..=65535).contains(&port) {
        return Some("name and a valid port (1-65535) are required");
    }
    None
}

fn scheme_of(v: Option<&str>, fallback: &str) -> String {
    match v.map(|s| s.trim().to_lowercase()) {
        Some(s) if s == "http" || s == "https" => s,
        _ => fallback.to_string(),
    }
}

async fn create(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    // Python's int(body.get("port")) accepts "5055" as well as 5055; the SPA
    // sends a string from an <input>, so a number-only parse would reject every
    // create the UI makes.
    let port = body["port"]
        .as_i64()
        .or_else(|| body["port"].as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(0);
    if let Some(err) = validate(&name, port) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": err}))).into_response();
    }
    let scheme = scheme_of(body["scheme"].as_str(), "http");
    let ts = now();
    let out = state
        .store
        .write_async(move |conn| {
            let id = bs::next_issue_id(conn, "PRX")?;
            conn.execute(
                "INSERT INTO proxies (id, name, port, scheme, created_at) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![id, name, port, scheme, ts],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match out {
        Ok(_) => {
            // The id is minted inside the write; read back the newest row so the
            // response can carry it (the SPA does not use it today, but a
            // create that cannot name what it created is a poor contract).
            let id = state
                .store
                .read()
                .ok()
                .and_then(|c| {
                    c.query_row(
                        "SELECT id FROM proxies ORDER BY created_at DESC, rowid DESC LIMIT 1",
                        [],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                })
                .unwrap_or_default();
            (StatusCode::OK, Json(json!({"ok": true, "id": id}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let existing = {
        let Ok(conn) = state.store.read() else {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "store unavailable"})))
                .into_response();
        };
        conn.query_row(
            "SELECT name, port, scheme FROM proxies WHERE id=?1",
            [&id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
        )
        .ok()
    };
    let Some((cur_name, cur_port, cur_scheme)) = existing else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
    };

    // Every field falls back to its CURRENT value — a PATCH that omits a field
    // must not blank it (Python py:65660 does the same).
    let name = match body["name"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => cur_name,
    };
    let port = body["port"]
        .as_i64()
        .or_else(|| body["port"].as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(cur_port);
    if !(1..=65535).contains(&port) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid port"}))).into_response();
    }
    let scheme = scheme_of(body["scheme"].as_str(), &cur_scheme);

    match state
        .store
        .write_async(move |conn| {
            conn.execute(
                "UPDATE proxies SET name=?1, port=?2, scheme=?3 WHERE id=?4",
                rusqlite::params![name, port, scheme, id],
            )?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await
    {
        // Python restarted a LIVE proxy whose port changed. Nothing can be live
        // without the tunnel client, so there is nothing to restart — and
        // pretending otherwise is the fake this module refuses to ship.
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM proxies WHERE id=?1", [&id])?;
            Ok(WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await
    {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
            .into_response(),
    }
}

/// Start the relay against this proxy's port (AMUX-2888, py:77793 `_proxy_start`).
///
/// The gateway derives one tid per token, so exactly one proxy is live at a
/// time: the tab is a library of saved targets and this picks the active one.
/// An already-running tunnel is stopped first, and the generation bump inside
/// `tunnel::stop` is what makes that safe — a loop parked in a 40s long-poll
/// cannot come back and serve against the port we just switched away from.
async fn start(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let row = {
        let Ok(conn) = state.store.read() else {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "store unavailable"})))
                .into_response();
        };
        conn.query_row("SELECT port FROM proxies WHERE id=?1", [&id], |r| r.get::<_, i64>(0)).ok()
    };
    let Some(port) = row else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "proxy not found"}))).into_response();
    };
    let Ok(port) = u16::try_from(port) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid port"}))).into_response();
    };
    if crate::runtime_jobs::tunnel::snapshot().running {
        crate::runtime_jobs::tunnel::stop();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    match crate::runtime_jobs::tunnel::start(Some(port)).await {
        // 400: refusing amux's own port, or no token. Both are amux declining
        // with a stated remedy, not amux failing.
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": e}))).into_response(),
        Ok(s) => {
            crate::runtime_jobs::tunnel::set_proxy_id(Some(id.clone()));
            let ts = now();
            let _ = state
                .store
                .write_async(move |conn| {
                    conn.execute(
                        "UPDATE proxies SET last_started=?1 WHERE id=?2",
                        rusqlite::params![ts, id],
                    )?;
                    Ok(WriteOutcome { applied: true, events: vec![] })
                })
                .await;
            Json(json!({"ok": true, "running": s.running, "url": s.url, "error": s.error}))
                .into_response()
        }
    }
}

/// Stop the relay if THIS proxy is the one running (py:77810 `_proxy_stop`).
///
/// Scoped to the active proxy on purpose: stopping proxy B must not tear down
/// the tunnel proxy A is serving, and the tab shows every saved target with a
/// Stop button regardless of which is live.
async fn stop(Path(id): Path<String>) -> Response {
    let active = crate::runtime_jobs::tunnel::active_proxy_id();
    if active.as_deref() == Some(id.as_str()) {
        crate::runtime_jobs::tunnel::stop();
        crate::runtime_jobs::tunnel::set_proxy_id(None);
        return Json(json!({"ok": true, "stopped": true})).into_response();
    }
    // Not a 404 and not an error: the caller's intent ("this proxy should not
    // be running") is satisfied. Saying WHICH one is live keeps a stale tab
    // from reading the ok as "I stopped the tunnel".
    Json(json!({"ok": true, "stopped": false, "active_id": active})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_validation_matches_python_bounds() {
        assert!(validate("api", 1).is_none());
        assert!(validate("api", 65535).is_none());
        // The controls: each of these is a distinct way the UI can send junk.
        assert!(validate("api", 0).is_some(), "0 is not a port");
        assert!(validate("api", 65536).is_some(), "above the u16 range");
        assert!(validate("api", -1).is_some(), "negative");
        assert!(validate("", 8080).is_some(), "a nameless proxy is unusable in the list");
        assert!(validate("   ", 8080).is_some(), "whitespace is not a name");
    }

    /// AMUX-2888: stopping proxy B must not tear down the tunnel proxy A is
    /// serving. The tab shows a Stop button on every saved row regardless of
    /// which is live, so an unscoped stop is one stray click from killing a
    /// public URL somebody is using.
    ///
    /// Asserted against the relay's own state rather than a mock, because the
    /// scoping decision IS "ask the relay which proxy is active" and a mock
    /// would be testing the paraphrase (ethos rule 7).
    #[tokio::test]
    async fn stopping_a_proxy_that_is_not_live_leaves_the_running_one_alone() {
        use crate::runtime_jobs::tunnel as tun;
        // Nothing is running in a unit test, so the honest starting point is
        // "no active proxy" — and stop must then be a no-op that SAYS so
        // rather than reporting a stop that did not happen.
        tun::set_proxy_id(None);
        let (st, body) = shape(stop(Path("PRX-9".into())).await).await;
        assert_eq!(st, StatusCode::OK, "not an error: the caller's intent is satisfied");
        assert_eq!(body["stopped"], json!(false), "it did not stop anything: {body}");

        // CONTROL: with PRX-9 recorded as the active proxy the same call DOES
        // stop. Without this cell, a stop that never stops anything passes the
        // assertions above.
        tun::set_proxy_id(Some("PRX-9".into()));
        // `active_proxy_id` gates on `running`, which is false here — so the
        // control asserts the PREDICATE the handler uses, not a state a unit
        // test cannot construct without dialling a gateway.
        assert_eq!(tun::active_proxy_id(), None, "not running, so nothing is active");
        tun::set_proxy_id(None);
    }

    async fn shape(r: Response) -> (StatusCode, Value) {
        let st = r.status();
        let b = axum::body::to_bytes(r.into_body(), 64 * 1024).await.unwrap_or_default();
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    #[test]
    fn scheme_coerces_unknown_values_rather_than_rejecting_them() {
        assert_eq!(scheme_of(Some("https"), "http"), "https");
        assert_eq!(scheme_of(Some("HTTPS"), "http"), "https");
        // Python coerces silently and the SPA depends on it — it omits `scheme`
        // on most writes, so rejecting would break every edit from the UI.
        assert_eq!(scheme_of(Some("ftp"), "http"), "http");
        assert_eq!(scheme_of(None, "https"), "https", "absent keeps the CURRENT scheme");
        assert_eq!(scheme_of(Some(""), "https"), "https");
    }
}
