//! Shared browser ladder: owned Amux profile, selected Chrome snapshot via
//! direct CDP, then the existing CUA desktop. Decisions stay with the worker.
use super::AppState;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::LazyLock, time::Duration};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub native_profile: String,
    #[serde(default)]
    pub chrome_profile: String,
    #[serde(default)]
    pub cua_profile: String,
    #[serde(default)]
    pub allow_cua: bool,
}
fn root() -> PathBuf {
    crate::config::amux_home().join("browser-routing")
}
fn load() -> Config {
    std::fs::read(root().join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or_default()
}
static REQUESTS: LazyLock<
    tokio::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
> = LazyLock::new(|| tokio::sync::Mutex::new(std::collections::HashMap::new()));
async fn request_lock(key: String) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut locks = REQUESTS.lock().await;
    locks.retain(|_, l| std::sync::Arc::strong_count(l) > 1);
    locks
        .entry(key)
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::request_lock;

    #[tokio::test]
    async fn selected_chrome_profile_serializes_workers_and_recovers_after_abort() {
        let name = format!("chrome:test:{}", ulid::Ulid::new());
        let first = request_lock(name.clone()).await;
        let second = request_lock(name).await;
        let other = request_lock(format!("chrome:test:{}", ulid::Ulid::new())).await;
        let (ready, notified) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _lease = first.lock().await;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        notified.await.unwrap();
        assert!(
            second.try_lock().is_err(),
            "same profile must serialize workers"
        );
        assert!(
            other.try_lock().is_ok(),
            "independent profiles do not block"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            second.try_lock().is_ok(),
            "terminated driver cannot strand a lease"
        );
    }
}
fn error(code: StatusCode, text: impl ToString) -> Response {
    (code, Json(json!({"error":text.to_string()}))).into_response()
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/config", get(config).post(save))
        .route("/request", post(request))
}
async fn config() -> Response {
    let chrome_root = crate::integrations::browser::chrome_user_data_dir();
    let v: Value = std::fs::read(chrome_root.join("Local State"))
        .ok()
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or(Value::Null);
    let profiles:Vec<Value>=v.pointer("/profile/info_cache").and_then(Value::as_object).map(|cache|cache.iter().map(|(name,m)|json!({"name":name,"label":m.get("name"),"identity":m.get("user_name"),"on_disk":chrome_root.join(name).is_dir()})).collect()).unwrap_or_default();
    Json(json!({"config":load(),"chrome_profiles":profiles,"measured":true,"n_considered":profiles.len(),"ladder":["amux","cdp","cua"],"note":"CDP uses a persistent isolated copy of the selected Chrome profile. CUA uses the selected saved Amux profile; sandbox transfer currently covers cookies only."})).into_response()
}
async fn save(headers: HeaderMap, Json(c): Json<Config>) -> Response {
    // The fallback identity is an owner choice, never a worker's silent edit.
    if headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| !s.trim().is_empty())
    {
        return error(
            StatusCode::FORBIDDEN,
            "only the owner may change the shared browser routing configuration",
        );
    }
    let chrome = crate::integrations::browser::chrome_user_data_dir();
    if !c.chrome_profile.is_empty()
        && (!crate::integrations::browser::list_chrome_profiles().contains(&c.chrome_profile)
            || !chrome.join(&c.chrome_profile).is_dir())
    {
        return error(
            StatusCode::BAD_REQUEST,
            "select an existing Chrome profile directory",
        );
    }
    let profiles = crate::integrations::browser::list_profiles(&crate::config::amux_home(), false);
    for name in [&c.native_profile, &c.cua_profile] {
        if !name.is_empty() && !profiles.iter().any(|p| &p.name == name && p.on_disk) {
            return error(
                StatusCode::BAD_REQUEST,
                format!("saved profile does not exist: {name}"),
            );
        }
    }
    let out = (|| -> anyhow::Result<()> {
        std::fs::create_dir_all(root())?;
        let temp = root().join(format!("config-{}.tmp", ulid::Ulid::new()));
        std::fs::write(&temp, serde_json::to_vec_pretty(&c)?)?;
        std::fs::rename(temp, root().join("config.json"))?;
        Ok(())
    })();
    match out {
        Ok(()) => {
            tracing::info!(verdict="browser_routing_config_saved",native_profile=%c.native_profile,chrome_profile=%c.chrome_profile,cua_profile=%c.cua_profile,allow_cua=c.allow_cua,"browser route configuration saved");
            Json(json!({"ok":true,"config":c})).into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    verb: String,
    #[serde(default)]
    body: Value,
    session: String,
}
async fn request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(r): Json<Request>,
) -> Response {
    let verbs = [
        "start",
        "navigate",
        "state",
        "screenshot",
        "action",
        "stop",
        "keepalive",
        "inspect",
        "inspect/clear",
        "status",
    ];
    if !verbs.contains(&r.verb.as_str()) {
        return error(StatusCode::BAD_REQUEST, "unsupported browser route verb");
    }
    if r.session.trim().is_empty() || r.session.contains(['/', '\\', '\0']) {
        return error(StatusCode::BAD_REQUEST, "explicit session required");
    }
    if let Some(h) = headers
        .get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
    {
        if h != r.session {
            return error(
                StatusCode::FORBIDDEN,
                "session differs from caller identity",
            );
        }
    }
    // Read the receipt under its session lock, so a concurrent start cannot
    // change the selected identity between authorization and execution.
    let lock = request_lock(format!("session:{}", r.session)).await;
    let _guard = lock.lock().await;
    let mut c = load();
    if c.native_profile.is_empty() {
        c.native_profile = super::browser::default_browser_profile(&r.session).0;
    }
    use sha2::{Digest, Sha256};
    let receipt_key = format!("{:x}", Sha256::digest(r.session.as_bytes()));
    let receipt: Value = std::fs::read(
        root()
            .join("sessions")
            .join(format!("{}.json", &receipt_key[..24])),
    )
    .ok()
    .and_then(|b| serde_json::from_slice(&b).ok())
    .unwrap_or(Value::Null);
    let chosen = if r.verb == "start" {
        r.body
            .get("profile")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    } else {
        receipt.get("selected_profile").and_then(Value::as_str)
    }
    .unwrap_or(&c.native_profile);
    if let Err(denied) = super::browser_scope::profile_allowed(&r.session, chosen) {
        return denied.response();
    }
    // Serialize publication/launch of the shared Chrome snapshot across workers.
    // Unlike a filesystem lease, this recovers on driver/server termination.
    let chrome_lock = request_lock(format!("chrome:{}", c.chrome_profile)).await;
    let _chrome_guard = chrome_lock.lock().await;
    let home = crate::config::amux_home();
    let script_source = include_str!("../../../../scripts/browser-route-driver.mjs");
    let script_hash = format!("{:x}", Sha256::digest(script_source.as_bytes()));
    let script = root().join(format!("driver-{}.mjs", &script_hash[..24]));
    if let Err(e) = std::fs::create_dir_all(root()).and_then(|_| {
        let cached = std::fs::read(&script).ok();
        if cached.as_deref() == Some(script_source.as_bytes()) {
            Ok(())
        } else {
            if cached.is_some() {
                tracing::warn!(verdict="browser_routing_driver_cache_repaired",source_hash=%script_hash,"restoring the compiled browser driver before execution");
            }
            let temp = root().join(format!("driver-{}.tmp", ulid::Ulid::new()));
            std::fs::write(&temp, script_source)?;
            std::fs::rename(temp, &script)
        }
    }) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    let port = crate::config::canonical_port().to_string();
    let context = json!({"base":format!("https://127.0.0.1:{port}"),"home":home,"session":r.session,"config":c,"chrome_root":crate::integrations::browser::chrome_user_data_dir(),"chrome_binary":crate::integrations::browser::chrome_binary(),"token":state.auth_token});
    let mut context = context;
    // An unused fallback must not prevent an allowed native profile from
    // opening. Enforce each fallback's quiet policy verdict when it is tried.
    let policy =
        super::browser_scope::Policy::for_worker(&super::session_verbs::home(), &r.session);
    context["chrome_access"] = json!(policy.decide(&c.chrome_profile));
    context["cua_access"] = json!(policy.decide(if c.cua_profile.is_empty() {
        chosen
    } else {
        &c.cua_profile
    }));
    let profiles = crate::integrations::browser::list_profiles(&home, false);
    context["identity"] = json!(profiles
        .iter()
        .find(|p| p.name == chosen)
        .map(|p| p.identity.as_str())
        .unwrap_or(""));
    context["cua_identity"] = json!(profiles
        .iter()
        .find(|p| p.name == c.cua_profile)
        .map(|p| p.identity.as_str())
        .unwrap_or(""));
    let chrome_state: Value =
        std::fs::read(crate::integrations::browser::chrome_user_data_dir().join("Local State"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
    context["chrome_identity"] = chrome_state
        .pointer("/profile/info_cache")
        .and_then(|v| v.get(&c.chrome_profile))
        .and_then(|v| v.get("user_name"))
        .cloned()
        .unwrap_or(json!(""));
    let input = json!({"context":context,"verb":r.verb,"body":r.body});
    let mut child = match tokio::process::Command::new("node")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(p) => p,
        Err(e) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Node.js 22+ required for browser routing: {e}"),
            )
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(input.to_string().as_bytes()).await {
            return error(StatusCode::BAD_GATEWAY, e);
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(240), child.wait_with_output()).await;
    match result {
        Ok(Ok(out)) => match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(v) => {
                let code = v
                    .get("status")
                    .and_then(Value::as_u64)
                    .and_then(|n| StatusCode::from_u16(n as u16).ok())
                    .unwrap_or(StatusCode::OK);
                let backend = v
                    .pointer("/route/backend")
                    .and_then(Value::as_str)
                    .unwrap_or("none");
                tracing::info!(verdict="browser_route_result",session=%r.session,verb=%r.verb,backend,status=code.as_u16(),attempts=?v.get("attempts").or_else(||v.pointer("/route/attempts")),"browser ladder request completed");
                (code, Json(v)).into_response()
            }
            Err(e) => error(
                StatusCode::BAD_GATEWAY,
                format!("browser driver did not return JSON: {e}"),
            ),
        },
        Ok(Err(e)) => error(StatusCode::BAD_GATEWAY, e),
        Err(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "browser route exceeded 240 seconds",
        ),
    }
}
