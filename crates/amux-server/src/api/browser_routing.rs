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
    /// Owner choices, retained independently for each saved Amux profile.
    #[serde(default)]
    pub profile_routes: std::collections::BTreeMap<String, ProfileRoute>,
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRoute {
    #[serde(default)]
    pub chrome_profile: String,
    #[serde(default)]
    pub cua_profile: String,
    #[serde(default)]
    pub allow_cua: bool,
}
impl Config {
    fn selected(&self, profile: &str) -> Self {
        let mut selected = self.clone();
        selected.native_profile = profile.to_owned();
        if let Some(route) = self.profile_routes.get(profile) {
            selected.chrome_profile = route.chrome_profile.clone();
            selected.cua_profile = route.cua_profile.clone();
            selected.allow_cua = route.allow_cua;
        } else if profile != self.native_profile || !self.profile_routes.is_empty() {
            // Another account's last-saved fallback is not this account's choice.
            selected.chrome_profile.clear();
            selected.cua_profile.clear();
            selected.allow_cua = false;
        }
        selected
    }
    fn retain_choices(&mut self, mut previous: Self) {
        if !previous.native_profile.is_empty() {
            previous
                .profile_routes
                .entry(previous.native_profile.clone())
                .or_insert(ProfileRoute {
                    chrome_profile: previous.chrome_profile,
                    cua_profile: previous.cua_profile,
                    allow_cua: previous.allow_cua,
                });
        }
        self.profile_routes = previous.profile_routes;
        if !self.native_profile.is_empty() {
            self.profile_routes.insert(
                self.native_profile.clone(),
                ProfileRoute {
                    chrome_profile: self.chrome_profile.clone(),
                    cua_profile: self.cua_profile.clone(),
                    allow_cua: self.allow_cua,
                },
            );
        }
    }
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
    use super::{request_lock, Config};

    #[test]
    fn owner_choices_are_retained_per_profile_without_cross_account_defaults() {
        let mut work: Config = serde_json::from_value(serde_json::json!({"native_profile":"work","chrome_profile":"Profile 14","cua_profile":"work","allow_cua":true})).unwrap();
        assert_eq!(work.selected("work").chrome_profile, "Profile 14");
        assert!(
            work.selected("other-customer").chrome_profile.is_empty(),
            "legacy fallback belongs only to its selected account"
        );
        work.retain_choices(Config::default());
        let mut personal: Config = serde_json::from_value(serde_json::json!({"native_profile":"personal","chrome_profile":"Profile 11","cua_profile":"personal","allow_cua":false})).unwrap();
        personal.retain_choices(work);
        assert_eq!(personal.selected("work").chrome_profile, "Profile 14");
        assert!(personal.selected("work").allow_cua);
        assert_eq!(personal.selected("personal").chrome_profile, "Profile 11");
        assert!(!personal.selected("unconfigured").allow_cua);
        assert!(personal.selected("unconfigured").chrome_profile.is_empty());
        let saved = serde_json::to_vec(&personal).unwrap();
        let reloaded: Config = serde_json::from_slice(&saved).unwrap();
        assert_eq!(reloaded.selected("work").cua_profile, "work");
    }

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
async fn save(headers: HeaderMap, Json(mut c): Json<Config>) -> Response {
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
    if !c.profile_routes.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "save a route by selecting its native_profile; profile_routes is read-only",
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
    c.retain_choices(load());
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
        "advance",
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
    // Status must remain observable while cold CUA provisioning owns the lane.
    // The receipt is atomically published; this read reports pending, never ready.
    let _guard = if r.verb == "status" {
        lock.try_lock().ok()
    } else {
        Some(lock.lock().await)
    };
    let pending = _guard.is_none();
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
    .unwrap_or(&c.native_profile)
    .to_owned();
    if let Err(denied) = super::browser_scope::profile_allowed(&r.session, &chosen) {
        return denied.response();
    }
    c = c.selected(&chosen);
    // Revalidate the actual active fallback, not just the original Amux
    // profile or a newly edited owner choice. Cleanup may close a revoked tab.
    if r.verb != "start"
        && r.verb != "stop"
        && matches!(
            receipt.get("backend").and_then(Value::as_str),
            Some("cdp" | "cua")
        )
    {
        if let Some(active) = receipt.get("profile").and_then(Value::as_str) {
            if let Err(denied) = super::browser_scope::profile_allowed(&r.session, active) {
                return denied.response();
            }
        }
    }
    if pending {
        tracing::info!(session=%r.session, measured=true, n_considered=1,
            verdict="browser_route_request_pending", "route status observed during an in-flight request");
        return (StatusCode::ACCEPTED, Json(json!({
            "ok":true,"pending":true,"running":null,"measured":true,"n_considered":1,
            "note":"A route request is still running. Wait for its background task; cold CUA provisioning can take up to 30 minutes. Do not start another state/action request or kill processes.",
            "route":receipt
        }))).into_response();
    }
    // Serialize publication/launch of the shared Chrome snapshot across workers.
    // Unlike a filesystem lease, this recovers on driver/server termination.
    let mut chrome_profiles = std::collections::BTreeSet::new();
    if !c.chrome_profile.is_empty() {
        chrome_profiles.insert(c.chrome_profile.clone());
    }
    for route in std::iter::once(&receipt).chain(
        receipt
            .get("previous_routes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten(),
    ) {
        if route.get("backend").and_then(Value::as_str) == Some("cdp") {
            if let Some(profile) = route.get("profile").and_then(Value::as_str) {
                chrome_profiles.insert(profile.to_owned());
            }
        }
    }
    let mut chrome_guards = Vec::new();
    for profile in chrome_profiles {
        chrome_guards.push(
            request_lock(format!("chrome:{profile}"))
                .await
                .lock_owned()
                .await,
        );
    }
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
        &chosen
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
    // CUA may provision the exact desktop image (Docker's bounded 30-minute
    // build). An outer 240s limit otherwise repeatedly cancels every cold build.
    // Ordinary driver calls retain their own shorter transport/action limits.
    let deadline_s = if c.allow_cua && r.verb != "stop" {
        2700
    } else {
        240
    };
    let result =
        tokio::time::timeout(Duration::from_secs(deadline_s), child.wait_with_output()).await;
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
            format!("browser route exceeded {deadline_s} seconds"),
        ),
    }
}
