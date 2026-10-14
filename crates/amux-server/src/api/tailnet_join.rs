//! `/api/tailnet/*`: add this amux server to the owner's own tailnet
//! (docs/design/cloud-self-service.md 8.5).
//!
//! Join runs `tailscale up` with a short timeout. A node that needs a login
//! leaves Tailscale's login link in `tailscale status --json` (`AuthURL`), so the
//! dashboard shows that link and the owner approves the device in their own
//! tailnet; nothing about their tailnet passes through amux Cloud. An auth key
//! joins without the link. Once Running, `tailscale serve` publishes this server
//! at `https://<host>.<tailnet>.ts.net`, where amux's existing tailnet sign-in
//! (`static_files/tailnet_auth.rs`) maps people to members.
//!
//! Owner only: a worker or a scoped member gets 403. A machine where the
//! server's user may not drive tailscaled says so with Tailscale's own words and
//! the fix (`tailscale set --operator=<user>`, done once as root by bootstrap.sh).

use axum::{
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

use super::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/join", post(join))
        .route("/leave", post(leave))
}

fn tailscale_bin() -> String {
    if let Ok(b) = std::env::var("AMUX_TAILSCALE_BIN") {
        return b;
    }
    for c in [
        "/usr/bin/tailscale",
        "/usr/local/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/tailscale",
    ] {
        if std::path::Path::new(c).exists() {
            return c.to_string();
        }
    }
    "tailscale".to_string()
}

async fn run(args: &[&str], timeout: Duration) -> Result<(bool, String, String), String> {
    let fut = Command::new(tailscale_bin()).args(args).output();
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => Err(format!("tailscale {} did not finish in {}s", args.join(" "), timeout.as_secs())),
        Ok(Err(e)) => Err(format!("tailscale is not installed or cannot run: {e}")),
        Ok(Ok(o)) => Ok((
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )),
    }
}

/// What the dashboard needs from `tailscale status --json`.
pub(crate) fn summarize(status: &Value) -> Value {
    let state = status["BackendState"].as_str().unwrap_or("Unknown");
    let dns = status["Self"]["DNSName"].as_str().unwrap_or("").trim_end_matches('.');
    let auth = status["AuthURL"].as_str().filter(|u| u.starts_with("https://"));
    json!({
        "measured": true,
        "state": state,
        "joined": state == "Running",
        "needs_login": state == "NeedsLogin" || state == "NeedsMachineAuth",
        "auth_url": auth,
        "tailnet": status["CurrentTailnet"]["Name"].as_str(),
        "hostname": if dns.is_empty() { Value::Null } else { json!(dns) },
        "url": if state == "Running" && !dns.is_empty() { json!(format!("https://{dns}")) } else { Value::Null },
    })
}

async fn read_status() -> Value {
    match run(&["status", "--json"], Duration::from_secs(10)).await {
        Ok((_, out, err)) => match serde_json::from_str::<Value>(&out) {
            Ok(v) => summarize(&v),
            Err(_) => json!({"measured": false, "why_unmeasured": format!("tailscale status printed no JSON: {}", err.trim())}),
        },
        Err(e) => json!({"measured": false, "why_unmeasured": e}),
    }
}

async fn status() -> Json<Value> {
    Json(read_status().await)
}

fn owner_only(headers: &HeaderMap) -> Result<(), (StatusCode, Json<Value>)> {
    if super::standing_approvals::is_owner_request(headers) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, Json(json!({"error": "only the server's owner can change its tailnet"}))))
    }
}

fn hostname() -> String {
    let raw = std::env::var("AMUX_SLUG")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(hostname_from_os)
        .unwrap_or_else(|| "server".into());
    let clean: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("amux-{}", clean.trim_matches('-'))
}

fn hostname_from_os() -> Option<String> {
    std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

async fn join(headers: HeaderMap, body: Option<Json<Value>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    owner_only(&headers)?;
    let key = body.as_ref().and_then(|b| b["auth_key"].as_str()).map(str::trim).filter(|k| !k.is_empty());
    let host = hostname();
    let mut args: Vec<String> = vec!["up".into(), format!("--hostname={host}"), "--timeout=8s".into()];
    if let Some(k) = key {
        if !k.starts_with("tskey-") {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "a Tailscale auth key starts with tskey-"}))));
        }
        args.push(format!("--auth-key={k}"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let up = run(&argv, Duration::from_secs(20)).await;
    if let Ok((false, _, err)) = &up {
        let e = err.trim();
        if e.contains("Access denied") || e.contains("operator") {
            tracing::warn!(target: "amux::tailnet_join", verdict = "operator_missing", "tailscale up refused: {e}");
            return Err((StatusCode::CONFLICT, Json(json!({
                "error": e,
                "fix": "run once as root: tailscale set --operator=$(whoami)",
            }))));
        }
    }
    let st = read_status().await;
    if st["joined"] == json!(true) {
        let port = std::env::var("AMUX_RS_PORT").unwrap_or_else(|_| "8824".into());
        let target = format!("https+insecure://127.0.0.1:{port}");
        match run(&["serve", "--bg", &target], Duration::from_secs(20)).await {
            Ok((true, _, _)) => {}
            Ok((false, _, err)) => tracing::warn!(target: "amux::tailnet_join", verdict = "serve_failed", "tailscale serve failed: {}", err.trim()),
            Err(e) => tracing::warn!(target: "amux::tailnet_join", verdict = "serve_failed", "tailscale serve failed: {e}"),
        }
    }
    tracing::info!(target: "amux::tailnet_join", verdict = "join_attempted", state = %st["state"], "tailnet join");
    Ok(Json(st))
}

async fn leave(headers: HeaderMap) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    owner_only(&headers)?;
    let _ = run(&["serve", "reset"], Duration::from_secs(10)).await;
    match run(&["logout"], Duration::from_secs(20)).await {
        Ok((true, _, _)) => {}
        Ok((false, _, err)) => return Err((StatusCode::BAD_GATEWAY, Json(json!({"error": err.trim()})))),
        Err(e) => return Err((StatusCode::BAD_GATEWAY, Json(json!({"error": e})))),
    }
    tracing::info!(target: "amux::tailnet_join", verdict = "left", "tailnet left");
    Ok(Json(read_status().await))
}

#[cfg(test)]
mod tests {
    use super::summarize;
    use serde_json::json;

    #[test]
    fn running_node_reports_its_tailnet_url() {
        let s = summarize(&json!({"BackendState": "Running", "Self": {"DNSName": "amux-alice.tail1234.ts.net."},
                                  "CurrentTailnet": {"Name": "alice@example.com"}}));
        assert_eq!(s["joined"], true);
        assert_eq!(s["url"], "https://amux-alice.tail1234.ts.net");
        assert_eq!(s["tailnet"], "alice@example.com");
        assert_eq!(s["auth_url"], serde_json::Value::Null);
    }

    #[test]
    fn needs_login_surfaces_the_login_link_only_when_it_is_https() {
        let s = summarize(&json!({"BackendState": "NeedsLogin", "AuthURL": "https://login.tailscale.com/a/abc123"}));
        assert_eq!(s["needs_login"], true);
        assert_eq!(s["auth_url"], "https://login.tailscale.com/a/abc123");
        assert_eq!(s["url"], serde_json::Value::Null);
        let bad = summarize(&json!({"BackendState": "NeedsLogin", "AuthURL": "javascript:alert(1)"}));
        assert_eq!(bad["auth_url"], serde_json::Value::Null);
    }

    #[test]
    fn stopped_or_unknown_is_not_joined() {
        assert_eq!(summarize(&json!({"BackendState": "Stopped"}))["joined"], false);
        assert_eq!(summarize(&json!({}))["state"], "Unknown");
    }
}
