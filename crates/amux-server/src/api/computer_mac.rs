//! `/api/computer/mac/*`: a worker uses this Mac's own apps through the
//! driver the amux Mac app runs (gate and mechanics: `integrations::computer_mac`).
//!
//! - `GET  /status`  is the driver up, which permissions it has, this worker's switches
//! - `GET  /tools`   the driver's tools, each marked free / app / refused
//! - `POST /call {tool, args}`  one tool call, gated per app; a screenshot in
//!   the reply is saved under ~/.amux/computer-screenshots and returned as `screenshot`
//!
//! Every call logs one `[computer-mac] verdict=` line naming the worker, the
//! tool and the app, so `amux audit` and a log sweep see each action.

use super::AppState;
use crate::integrations::computer_mac as mac;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/tools", get(tools))
        .route("/call", post(call))
}

fn err(code: StatusCode, body: Value) -> Response {
    (code, Json(body)).into_response()
}

fn worker(body: Option<&Value>, headers: &HeaderMap) -> Option<String> {
    body.and_then(|b| b.get("session"))
        .and_then(Value::as_str)
        .or_else(|| headers.get("x-amux-session").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn home() -> std::path::PathBuf {
    crate::integrations::browser::amux_home()
}

async fn status(headers: HeaderMap) -> Response {
    let home = home();
    let w = worker(None, &headers);
    let policy = w.as_deref().map(|w| mac::Policy::for_worker(&home, w));
    let switches =
        policy.map(|p| json!({ "apps": p.apps, "any_app": p.any_app, "foreground": p.foreground }));
    match mac::endpoint(&home) {
        Err(e) => Json(
            json!({ "up": false, "measured": true, "why": e, "worker": w, "switches": switches }),
        )
        .into_response(),
        Ok(ep) => {
            let perms = mac::call(&ep, "check_permissions", &json!({}), None).await;
            Json(json!({
                "up": true, "measured": true, "app": ep.app, "driver_pid": ep.pid,
                "permissions": perms.as_ref().ok(), "permissions_error": perms.err(),
                "worker": w, "switches": switches,
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
    let Some(w) = worker(Some(&body), &headers) else {
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
    let ep = match mac::endpoint(&home) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("[computer-mac] verdict=driver_down worker={w} tool={tool}: {e}");
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "error": e, "code": "driver_down", "sent": false,
                "next": "the owner turns on Settings > Computer use in the amux Mac app" }),
            );
        }
    };
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
    if let Err(r) = mac::decide(&tool, &args, &apps, &policy) {
        tracing::warn!(
            "[computer-mac] verdict=refused code={} worker={w} tool={tool} app={label:?}",
            r.code
        );
        return err(StatusCode::FORBIDDEN, r.json());
    }
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
    match mac::call(&ep, &tool, &args, Some(&shot)).await {
        Ok(mut v) => {
            let has_shot = shot.is_file();
            let pruned = if has_shot {
                super::computer::prune_shots(&dir, &stem)
            } else {
                0
            };
            tracing::info!(
                "[computer-mac] verdict=called worker={w} tool={tool} app={label:?} ms={} screenshot={}",
                t0.elapsed().as_millis(),
                if has_shot { shot.display().to_string() } else { "-".into() }
            );
            if let Some(o) = v.as_object_mut() {
                // The image itself is on disk; a base64 copy would only bloat the reply.
                strip_images(o);
            }
            Json(json!({ "ok": true, "worker": w, "tool": tool, "apps": label, "result": v,
                         "screenshot": has_shot.then(|| shot.display().to_string()), "pruned": pruned }))
            .into_response()
        }
        Err(e) => {
            tracing::warn!(
                "[computer-mac] verdict=driver_error worker={w} tool={tool} app={label:?}: {e}"
            );
            err(
                StatusCode::BAD_GATEWAY,
                json!({ "error": e, "code": "driver_error", "tool": tool }),
            )
        }
    }
}

/// Drops base64 image blocks (MCP `{"type":"image","data":...}`) from a reply.
fn strip_images(o: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Array(content)) = o.get_mut("content") {
        content.retain(|c| c.get("type").and_then(Value::as_str) != Some("image"));
    }
}
