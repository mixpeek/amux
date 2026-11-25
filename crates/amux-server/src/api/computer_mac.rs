//! `/api/computer/mac/*`: a worker uses this Mac's own apps through the
//! driver the amux Mac app runs (gate and mechanics: `integrations::computer_mac`).
//!
//! - `GET  /status`  is the driver up, which permissions it has, this worker's switches
//! - `GET  /tools`   the driver's tools, each marked free / app / refused
//! - `GET  /activity?session=` what workers did, newest first, with screenshots
//! - `POST /call {tool, args}`  one tool call, gated per app; a screenshot in
//!   the reply is saved under ~/.amux/computer-screenshots and returned as `screenshot`
//!
//! Every call logs one `[computer-mac] verdict=` line naming the worker, the
//! tool and the app, so `amux audit` and a log sweep see each action.

use super::AppState;
use crate::integrations::computer_mac as mac;
use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/tools", get(tools))
        .route("/call", post(call))
        .route("/activity", get(activity))
        .route("/broker/next", get(broker_next))
        .route("/broker/result", post(broker_result))
}

/// `GET /broker/next?wait=S`: the Mac app's long poll for the next call
/// that passed the gate in `/call`. Owner only: the app sends the owner
/// bearer, and nothing else may take a call meant for the driver. It waits
/// only when asked (the app asks for 25s), so a bare GET answers at once.
async fn broker_next(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "computer_mac_broker_refused", "take computer-use calls for the Mac app's driver") {
        return *r;
    }
    let wait = q
        .get("wait")
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::ZERO)
        .min(mac::POLL_WAIT);
    match mac::broker().next(wait).await {
        Some(job) => Json(json!({ "job": job })).into_response(),
        None => Json(json!({ "job": null })).into_response(),
    }
}

/// `POST /broker/result {id, result} | {id, error}`: the app's answer to a
/// job. Owner only. The answer goes to the `/call` handler waiting on that id
/// in this process; no route reads it back.
async fn broker_result(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "computer_mac_broker_refused", "answer computer-use calls for the Mac app's driver") {
        return *r;
    }
    let Some(id) = body.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "send {id, result} or {id, error}", "code": "bad_request" }));
    };
    let r = match (body.get("result"), body.get("error")) {
        (_, Some(e)) if !e.is_null() => Err(e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())),
        (Some(v), _) => Ok(v.clone()),
        _ => return err(StatusCode::BAD_REQUEST, json!({ "error": "send {id, result} or {id, error}", "code": "bad_request" })),
    };
    if mac::broker().reply(id, r) {
        Json(json!({ "ok": true })).into_response()
    } else {
        tracing::info!(measured = true, n_considered = 1, verdict = "computer_mac_result_unclaimed",
            "[computer-mac] a result arrived for a call nobody waits on (timed out, or never issued)");
        err(StatusCode::NOT_FOUND, json!({ "error": "no call waits on that id", "code": "unknown_id" }))
    }
}

fn err(code: StatusCode, body: Value) -> Response {
    (code, Json(body)).into_response()
}

/// The worker is the one the request's X-Amux-Session header names, which the
/// identity middleware checks against that worker's token. A body `session`
/// is never an identity (amux-security, 2026-11-23): it used to win over the
/// header, so a worker could act under another worker's app list and switches,
/// dodge its own outside-content stamp, and file cards and activity in that
/// worker's name. `call` refuses a body that names anyone else.
fn worker(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn home() -> std::path::PathBuf {
    crate::integrations::browser::amux_home()
}

/// `?session=` names a worker for the owner (the dashboard's Computer tab);
/// a worker reads only its own. The reply then carries that worker's switches
/// and its open ask for a Mac app, if one waits on the owner.
async fn status(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let home = home();
    let named = q.get("session").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let w = match (worker(&headers), named) {
        (Some(c), Some(n)) if c != n => {
            return err(
                StatusCode::FORBIDDEN,
                json!({ "error": "a worker reads its own status: drop `session` or name yourself", "code": "status_as_self" }),
            );
        }
        (Some(c), _) => Some(c),
        (None, Some(n)) => {
            if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "computer_mac_status_refused", "read a worker's Mac switches") {
                return *r;
            }
            Some(n)
        }
        (None, None) => None,
    };
    let ask = w.as_deref().and_then(|w| super::capabilities::open_ask(&home, w, "mac_apps")).map(|a| {
        json!({ "resource": a.get("resource"), "asked_at": a.get("asked_at"),
                "approval_ref": format!("capability:{}:mac_apps", w.as_deref().unwrap_or("")) })
    });
    let policy = w.as_deref().map(|w| mac::Policy::for_worker(&home, w));
    let switches =
        policy.map(|p| json!({ "apps": p.apps, "any_app": p.any_app, "foreground": p.foreground }));
    match mac::endpoint(&home) {
        Err(e) => Json(
            json!({ "up": false, "measured": true, "why": e, "worker": w, "switches": switches, "ask": ask }),
        )
        .into_response(),
        Ok(ep) => {
            let connected = mac::broker().connected();
            let perms = mac::call("check_permissions", &json!({}), None).await;
            Json(json!({
                "up": connected, "measured": true, "app": ep.app, "driver_pid": ep.pid, "app_polling": connected,
                "permissions": perms.as_ref().ok(), "permissions_error": perms.err(),
                "worker": w, "switches": switches, "ask": ask,
                "keys": { "apps": mac::APPS_KEY, "any_app": mac::ANY_APP_KEY, "foreground": mac::FOREGROUND_KEY },
            }))
            .into_response()
        }
    }
}

async fn tools() -> Response {
    let ep = match mac::endpoint(&home()) {
        Ok(e) => e,
        Err(e) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "error": e, "code": "driver_down" }),
            )
        }
    };
    let out = tokio::process::Command::new(&ep.binary)
        .arg("list-tools")
        .env("CUA_DRIVER_TELEMETRY", "0")
        .output()
        .await;
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let list: Vec<Value> = text
        .lines()
        .filter_map(|l| l.split_once(": "))
        .filter(|(n, _)| !n.contains(' '))
        .map(|(n, d)| {
            let class = match mac::classify(n) {
                mac::Class::Free => json!("free"),
                mac::Class::App => json!("app"),
                mac::Class::Refused(why) => json!({ "refused": why }),
            };
            json!({ "name": n, "class": class, "description": d })
        })
        .collect();
    Json(json!({ "measured": !list.is_empty(), "n_considered": list.len(), "tools": list,
                 "describe": "cua-driver describe <tool> on the driver binary prints a tool's input schema" }))
    .into_response()
}

async fn call(headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(json!({}));
    let named = body.get("session").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if !named.is_empty() && worker(&headers).as_deref() != Some(named) {
        tracing::warn!(named, caller = worker(&headers).as_deref().unwrap_or(""), measured = true, n_considered = 1,
            verdict = "computer_mac_act_as_self", "[computer-mac] refused: the body named another worker");
        return err(
            StatusCode::FORBIDDEN,
            json!({ "error": "a worker acts as itself: drop `session` or name the worker in X-Amux-Session", "code": "act_as_self" }),
        );
    }
    let Some(w) = worker(&headers) else {
        tracing::warn!("[computer-mac] verdict=refused reason=unattributed");
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "which worker? send X-Amux-Session (the amux CLI does)", "code": "unattributed" }),
        );
    };
    let tool = body
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let args = body.get("args").cloned().unwrap_or(json!({}));
    if tool.is_empty() || !args.is_object() {
        return err(
            StatusCode::BAD_REQUEST,
            json!({ "error": "send {tool, args}: args a JSON object", "code": "bad_request" }),
        );
    }
    let home = home();
    if let Err(e) = mac::endpoint(&home) {
        tracing::warn!("[computer-mac] verdict=driver_down worker={w} tool={tool}: {e}");
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": e, "code": "driver_down", "sent": false,
            "next": "the owner turns on Settings > Computer use in the amux Mac app" }),
        );
    }
    // Fail closed: the only path to the driver is the app's poll.
    if !mac::broker().connected() {
        tracing::warn!("[computer-mac] verdict=computer_mac_app_not_polling worker={w} tool={tool}");
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "the amux Mac app is not connected for computer use; nothing was sent", "code": "app_not_polling", "sent": false,
            "next": "the owner opens the amux Mac app on this Mac with Computer use on (an app from before 2026-11-23 needs Check for Updates)" }),
        );
    }
    let policy = mac::Policy::for_worker(&home, &w);
    let (pids, mut apps) = mac::targets(&tool, &args);
    for p in pids {
        apps.push(mac::app_for_pid(p).await);
    }
    let label = apps
        .iter()
        .map(mac::App::label)
        .collect::<Vec<_>>()
        .join(", ");
    let window_id = args.get("window_id").and_then(Value::as_i64);
    let pid = apps.iter().find_map(|a| a.pid);
    // One row per call, refused or not: the owner's view of what workers do.
    let note = |verdict: &str, code: Option<&str>, shot: Option<String>, ms: u128| {
        mac::record_activity(
            &home,
            &json!({ "ts": chrono::Utc::now().timestamp_millis(), "worker": w, "tool": tool,
                     "apps": label, "pid": pid, "window_id": window_id, "verdict": verdict,
                     "detail": mac::step_detail(&tool, &args),
                     "code": code, "screenshot": shot, "ms": ms as u64 }),
        )
    };
    let refuse = |r: mac::Refusal| {
        tracing::warn!(
            "[computer-mac] verdict=refused code={} worker={w} tool={tool} app={label:?}",
            r.code
        );
        note("refused", Some(r.code), None, 0);
        // The owner is asked on the board, once per worker (capabilities.rs).
        match r.code {
            "app_not_allowed" => {
                let id = apps
                    .iter()
                    .find(|a| !mac::allowed(&policy, a))
                    .map(|a| {
                        if a.bundle_id.is_empty() {
                            a.name.clone()
                        } else {
                            a.bundle_id.clone()
                        }
                    })
                    .unwrap_or_default();
                super::capabilities::ask(&home, &w, "mac_apps", &label, &id);
            }
            "outside_content" => super::capabilities::ask(&home, &w, "mac_outside", &label, ""),
            _ => {}
        }
        let mut body = r.json();
        if matches!(r.code, "app_not_allowed" | "outside_content") {
            // The ask is already filed; a worker told "ask the owner" asks a
            // second time in chat and then waits for a "try again".
            body["asked"] = json!(true);
            body["next"] = json!(
                "the owner has a Needs-you card for this; you get a message here when it is approved, then retry. Do not route around it"
            );
        }
        err(StatusCode::FORBIDDEN, body)
    };
    if let Err(r) = mac::decide(&tool, &args, &apps, &policy) {
        return refuse(r);
    }
    let acting = mac::acts(&tool) || mac::step_tools(&args).iter().any(|t| mac::acts(t));
    let targets: Vec<String> = apps
        .iter()
        .map(|a| a.bundle_id.clone())
        .filter(|b| !b.is_empty())
        .collect();
    if acting {
        for b in &targets {
            match mac::yield_to_person(&home, b).await {
                Ok(waited) if waited > 0.2 => {
                    tracing::info!("[computer-mac] verdict=yielded_to_person worker={w} app={b} waited_s={waited:.1}")
                }
                Ok(_) => {}
                Err(r) => return refuse(r),
            }
        }
    }
    let front_before = if acting {
        mac::frontmost(&home).await
    } else {
        None
    };
    let dir = crate::integrations::computer::screenshot_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // The call still runs; only its screenshot has nowhere to go.
        tracing::warn!(
            "[computer-mac] verdict=screenshot_dir_unwritable dir={}: {e}",
            dir.display()
        );
    }
    let stem = format!(
        "mac-{}",
        crate::integrations::browser::safe_file_component(&w)
    );
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let shot = dir.join(format!("{stem}-{ms}.png"));
    let t0 = std::time::Instant::now();
    let res = mac::call(&tool, &args, Some(&shot)).await;
    // An action that pulled its app forward gives the person theirs back
    // (launch_app and bring_to_front ask for the front, so they keep it).
    let mut front_given_back = None;
    if acting && !matches!(tool.as_str(), "launch_app" | "bring_to_front") {
        if let Some(before) = front_before.filter(|b| !targets.contains(b)) {
            let after = mac::frontmost(&home).await;
            if after.as_ref().is_some_and(|a| targets.contains(a))
                && mac::give_front_back(&home, &before).await
            {
                tracing::info!(
                    "[computer-mac] verdict=front_given_back worker={w} to={before} from={}",
                    after.unwrap_or_default()
                );
                front_given_back = Some(before);
            }
        }
    }
    match res {
        Ok(mut v) => {
            let has_shot = shot.is_file();
            let pruned = if has_shot {
                super::computer::prune_shots(&dir, &stem)
            } else {
                0
            };
            let shot_s = has_shot.then(|| shot.display().to_string());
            tracing::info!(
                "[computer-mac] verdict=called worker={w} tool={tool} app={label:?} ms={} screenshot={}",
                t0.elapsed().as_millis(),
                shot_s.as_deref().unwrap_or("-")
            );
            note("called", None, shot_s.clone(), t0.elapsed().as_millis());
            if let Some(o) = v.as_object_mut() {
                // The image itself is on disk; a base64 copy would only bloat the reply.
                strip_images(o);
            }
            Json(json!({ "ok": true, "worker": w, "tool": tool, "apps": label, "result": v,
                         "screenshot": shot_s, "pruned": pruned, "front_given_back": front_given_back }))
            .into_response()
        }
        Err(e) => {
            tracing::warn!(
                "[computer-mac] verdict=driver_error worker={w} tool={tool} app={label:?}: {e}"
            );
            note(
                "driver_error",
                Some("driver_error"),
                None,
                t0.elapsed().as_millis(),
            );
            err(
                StatusCode::BAD_GATEWAY,
                json!({ "error": e, "code": "driver_error", "tool": tool }),
            )
        }
    }
}

/// `GET /activity?session=<worker>&limit=N`: the newest calls first, each
/// with its screenshot path, for the dashboard and the Mac app's preview.
/// A worker reads only its own rows; every worker's rows are the owner's
/// (amux-security, 2026-11-23: any caller could read every worker's calls).
async fn activity(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let limit = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30usize)
        .min(500);
    let named = q.get("session").map(|s| s.trim()).filter(|s| !s.is_empty());
    let caller = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let who = match caller {
        Some(w) if named.is_some_and(|n| n != w) => {
            tracing::warn!(named = named.unwrap_or(""), caller = w, measured = true, n_considered = 1,
                verdict = "computer_mac_activity_as_self", "[computer-mac] refused: a worker asked for another worker's activity");
            return err(
                StatusCode::FORBIDDEN,
                json!({ "error": "a worker reads its own activity: drop `session` or name yourself", "code": "activity_as_self" }),
            );
        }
        Some(w) => Some(w),
        None => {
            if let Err(r) = super::land_approvals::owner_only(&state, &headers, &uri, "computer_mac_activity_refused", "read every worker's Mac activity") {
                return *r;
            }
            named
        }
    };
    let (rows, n) = mac::activity(&home(), who, limit);
    Json(json!({ "measured": true, "n_considered": n, "returned": rows.len(), "rows": rows }))
        .into_response()
}

/// Middleware on the email read routes: a worker's successful read is outside
/// content for the Mac gate. The owner's own reads (no X-Amux-Session) are not.
pub async fn note_email_read(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    note_email_read_in(home(), req, next).await
}

pub async fn note_email_read_in(
    home: std::path::PathBuf,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let reading = req.method() == axum::http::Method::GET;
    let w = worker(req.headers());
    let resp = next.run(req).await;
    if let (true, Some(w)) = (reading && resp.status().is_success(), w) {
        mac::note_outside_content(&home, &w, "email");
    }
    resp
}

/// Drops base64 image blocks (MCP `{"type":"image","data":...}`) from a reply.
fn strip_images(o: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Array(content)) = o.get_mut("content") {
        content.retain(|c| c.get("type").and_then(Value::as_str) != Some("image"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    async fn read_with(home: &std::path::Path, status: StatusCode, worker: Option<&str>) {
        let h = home.to_path_buf();
        let app: Router = Router::new()
            .route("/inbox", get(move || async move { status }))
            .route_layer(axum::middleware::from_fn(move |req, next| {
                note_email_read_in(h.clone(), req, next)
            }));
        let mut req = axum::http::Request::builder().uri("/inbox");
        if let Some(w) = worker {
            req = req.header("x-amux-session", w);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), status);
    }

    #[tokio::test]
    async fn a_body_session_never_picks_the_worker() {
        let mut h = HeaderMap::new();
        h.insert("x-amux-session", "w1".parse().unwrap());
        let r = call(h, Some(Json(json!({"session": "w2", "tool": "list_apps", "args": {}})))).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "a worker cannot act as another");
        let r = call(HeaderMap::new(), Some(Json(json!({"session": "w2", "tool": "list_apps", "args": {}})))).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "no header and a named worker is not that worker");
        let r = call(HeaderMap::new(), Some(Json(json!({"tool": "list_apps", "args": {}})))).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "no header is unattributed");
    }

    #[tokio::test]
    async fn email_read_notes_outside_content() {
        let d = tempfile::tempdir().unwrap();
        read_with(d.path(), StatusCode::OK, Some("w1")).await;
        assert!(
            mac::outside_seen(d.path(), "w1").is_some(),
            "a worker's successful read is noted"
        );
        read_with(d.path(), StatusCode::NOT_IMPLEMENTED, Some("w2")).await;
        assert!(
            mac::outside_seen(d.path(), "w2").is_none(),
            "a failed read carried nothing"
        );
        read_with(d.path(), StatusCode::OK, None).await;
        let noted = std::fs::read_dir(d.path().join("computer/outside-content"))
            .unwrap()
            .count();
        assert_eq!(
            noted, 1,
            "only w1 was noted: not the failed read, not the owner's own"
        );
    }

    /// The broker's two routes are the owner's: a worker, or a caller with no
    /// bearer, can neither take a call meant for the driver nor answer one.
    #[tokio::test]
    async fn only_the_owner_polls_for_or_answers_driver_calls() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = routes().with_state(state);
        let ans = serde_json::json!({ "id": "x", "result": {} });
        for (h, why) in [
            (vec![("x-amux-session", "w1")], "a worker"),
            (vec![("x-amux-session", "w1"), ("authorization", "Bearer owner-secret")], "a worker with the bearer"),
            (vec![], "no identity and no bearer"),
        ] {
            let (st, _) = call(&app, "GET", "/broker/next?wait=0", &h, None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{why} polling");
            let (st, _) = call(&app, "POST", "/broker/result", &h, Some(ans.clone())).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{why} answering");
        }
        assert!(!mac::broker().connected(), "a refused poll does not connect the app");
        let owner = [("authorization", "Bearer owner-secret")];
        let (st, v) = call(&app, "GET", "/broker/next?wait=0", &owner, None).await;
        assert_eq!((st, v["job"].is_null()), (StatusCode::OK, true));
        assert!(mac::broker().connected());
        let (st, v) = call(&app, "POST", "/broker/result", &owner, Some(ans)).await;
        assert_eq!((st, v["code"].as_str()), (StatusCode::NOT_FOUND, Some("unknown_id")));
    }

    /// With the driver published but no app polling, /call sends nothing.
    #[tokio::test]
    async fn a_call_with_no_app_polling_fails_closed() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        std::fs::create_dir_all(d.path().join("cua")).unwrap();
        std::fs::write(mac::endpoint_path(d.path()),
            format!(r#"{{"binary":"/bin/sh","pid":{},"app":"com.amux.desktop"}}"#, std::process::id())).unwrap();
        let app = routes().with_state(test_state());
        let (st, v) = call(&app, "POST", "/call", &[("x-amux-session", "w1")],
            Some(serde_json::json!({ "tool": "list_apps", "args": {} }))).await;
        assert_eq!((st, v["code"].as_str(), v["sent"].as_bool()), (StatusCode::SERVICE_UNAVAILABLE, Some("app_not_polling"), Some(false)), "{v}");
    }

    /// A worker reads its own Mac activity; every worker's is the owner's.
    #[tokio::test]
    async fn a_worker_reads_only_its_own_activity() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let d = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(d.path());
        let f = mac::activity_file(d.path());
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "{\"worker\":\"w1\",\"tool\":\"click\"}\n{\"worker\":\"w2\",\"tool\":\"click\"}\n").unwrap();
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = routes().with_state(state);
        let workers = |v: &Value| -> Vec<String> {
            v["rows"].as_array().unwrap().iter().map(|r| r["worker"].as_str().unwrap().to_string()).collect()
        };
        let (st, v) = call(&app, "GET", "/activity", &[("x-amux-session", "w1")], None).await;
        assert_eq!((st, workers(&v)), (StatusCode::OK, vec!["w1".to_string()]), "a worker with no session named sees its own");
        let (st, v) = call(&app, "GET", "/activity?session=w2", &[("x-amux-session", "w1")], None).await;
        assert_eq!((st, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("activity_as_self")));
        let (st, _) = call(&app, "GET", "/activity", &[], None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "no identity and no bearer");
        let (st, v) = call(&app, "GET", "/activity", &[("authorization", "Bearer owner-secret")], None).await;
        assert_eq!((st, workers(&v)), (StatusCode::OK, vec!["w2".to_string(), "w1".to_string()]), "the owner sees every worker");
        let (st, v) = call(&app, "GET", "/activity?session=w2", &[("authorization", "Bearer owner-secret")], None).await;
        assert_eq!((st, workers(&v)), (StatusCode::OK, vec!["w2".to_string()]));
    }
}
