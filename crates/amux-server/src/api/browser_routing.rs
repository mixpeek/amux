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

/// browser-methods.toml as committed: the backend ladder and the agent methods.
pub const METHODS_DEFAULT: &str = include_str!("../../../../browser-methods.toml");
/// Adapters shipped with this build, by protocol.
const ADAPTERS: &[(&str, &str)] = &[("opagent-v1", include_str!("../../../../scripts/browser-agents/opagent-v1.mjs"))];

fn methods_layer(text: &str) -> Result<toml::Table, String> {
    toml::from_str::<toml::Table>(text).map_err(|e| e.to_string())
}

/// The effective methods registry: the committed file, then the owner's
/// ~/.amux/browser-methods.toml (a top-level `ladder` replaces the order; an
/// [[agent]] with a known id replaces that agent's fields; a new id adds one).
/// `adapter` (a path to the owner's own adapter module) is honoured only from
/// the owner's file. Errors are returned beside the result, never fatal.
pub fn methods() -> (Value, Vec<String>) {
    let mut errors = Vec::new();
    let mut base = methods_layer(METHODS_DEFAULT).unwrap_or_else(|e| { errors.push(format!("browser-methods.toml: {e}")); toml::Table::new() });
    for a in base.get_mut("agent").and_then(|v| v.as_array_mut()).into_iter().flatten() {
        if let Some(t) = a.as_table_mut() { t.remove("adapter"); }
    }
    let path = crate::config::amux_home().join("browser-methods.toml");
    if let Ok(text) = std::fs::read_to_string(&path) {
        match methods_layer(&text) {
            Ok(over) => {
                if let Some(l) = over.get("ladder") { base.insert("ladder".into(), l.clone()); }
                let agents = base.entry("agent").or_insert_with(|| toml::Value::Array(vec![]));
                if let (Some(list), Some(add)) = (agents.as_array_mut(), over.get("agent").and_then(|v| v.as_array())) {
                    for a in add.iter().filter_map(|v| v.as_table()) {
                        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                        match list.iter_mut().find(|x| x.get("id").and_then(|v| v.as_str()) == Some(id)) {
                            Some(existing) => { if let Some(t) = existing.as_table_mut() { t.extend(a.clone()); } }
                            None => list.push(toml::Value::Table(a.clone())),
                        }
                    }
                }
            }
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    let mut v = serde_json::to_value(&base).unwrap_or(Value::Null);
    if let Some(agents) = v.get_mut("agent").and_then(|a| a.as_array_mut()) {
        for a in agents.iter_mut() {
            if let Some(p) = a.get("adapter").and_then(Value::as_str).map(str::to_string) {
                a["adapter_path"] = json!(p);
            }
        }
    }
    let out = json!({
        "ladder": v.get("ladder").cloned().unwrap_or(json!(["amux","cdp","cua"])),
        "backends": v.get("backend").cloned().unwrap_or(json!([])),
        "agents": v.get("agent").cloned().unwrap_or(json!([])),
    });
    (out, errors)
}

/// Write each shipped adapter beside the driver (content-addressed) and
/// return protocol -> path for the driver's dynamic import.
fn adapter_modules() -> std::io::Result<Value> {
    let dir = root().join("agents");
    std::fs::create_dir_all(&dir)?;
    let mut map = serde_json::Map::new();
    for (protocol, source) in ADAPTERS {
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(source.as_bytes()));
        let file = dir.join(format!("{protocol}-{}.mjs", &hash[..16]));
        if std::fs::read(&file).ok().as_deref() != Some(source.as_bytes()) {
            let tmp = dir.join(format!("{protocol}-{}.tmp", ulid::Ulid::new()));
            std::fs::write(&tmp, source)?;
            std::fs::rename(&tmp, &file)?;
        }
        map.insert((*protocol).to_string(), json!(file.display().to_string()));
    }
    Ok(Value::Object(map))
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
    fn a_route_call_is_audited_with_outcome_and_without_typed_secrets() {
        use super::route_audit_event;
        use axum::http::StatusCode;
        let body = serde_json::json!({"action":"type","text":"hunter2-password","selector":"#pw"});
        let v = serde_json::json!({"ok":true,"url":"https://app.example/login?token=abc123secret&q=1","route":{"backend":"amux","profile":"netsuite"}});
        let e = route_audit_event("action", &body, &v, StatusCode::OK, 42);
        let s = e.to_string();
        assert!(!s.contains("hunter2"), "typed text never recorded: {s}");
        assert!(!s.contains("abc123secret"), "credential-shaped query values redacted: {s}");
        assert_eq!((e["verb"].as_str(), e["action"].as_str(), e["backend"].as_str(), e["profile"].as_str()),
            (Some("action"), Some("type"), Some("amux"), Some("netsuite")));
        assert_eq!((e["ok"].as_bool(), e["latency_ms"].as_u64(), e["via"].as_str()), (Some(true), Some(42), Some("route")));
        assert_eq!(e["typed_chars"], 16);

        let failed = route_audit_event("start", &serde_json::json!({"url":"https://x.example/"}),
            &serde_json::json!({"error":"page did not load; api_key=sk-live-SECRETSECRET1234"}), StatusCode::BAD_GATEWAY, 7);
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["target_url"], "https://x.example/");
        assert!(!failed.to_string().contains("SECRETSECRET1234"), "errors pass through the secret redactor: {failed}");
    }

    #[test]
    fn the_committed_methods_registry_parses_with_its_ladder_and_a_disabled_opagent() {
        let t = super::methods_layer(super::METHODS_DEFAULT).expect("browser-methods.toml parses");
        let ladder: Vec<&str> = t["ladder"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(ladder, ["amux", "cdp", "cua"]);
        let agents = t["agent"].as_array().unwrap();
        let op = agents.iter().find(|a| a["id"].as_str() == Some("opagent")).expect("opagent agent");
        assert_eq!(op["protocol"].as_str(), Some("opagent-v1"));
        assert_eq!(op["enabled"].as_bool(), Some(false), "an agent sees signed-in pages; it ships disabled");
        assert!(super::ADAPTERS.iter().any(|(p, _)| *p == "opagent-v1"), "every shipped protocol has an adapter");
    }

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
    Json(json!({"config":load(),"chrome_profiles":profiles,"measured":true,"n_considered":profiles.len(),"ladder":methods().0["ladder"],"methods":methods().0,"methods_errors":methods().1,"note":"CDP uses a persistent isolated copy of the selected Chrome profile. CUA uses the selected saved Amux profile; sandbox transfer currently covers cookies only."})).into_response()
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
/// Every route verb, on every backend, leaves one `browser.route.<verb>` event
/// in the session ledger beside the native `browser.*` events: who, which
/// backend and profile, what was asked (through the same redaction as native
/// actions), the page it ended on, the outcome and how long it took. A route
/// was a tracing line only, so "what did this worker click, on which page"
/// had no answer for the main way workers drive a browser (2026-10-27).
async fn request(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(r): Json<Request>,
) -> Response {
    let started = std::time::Instant::now();
    let (session, verb, body) = (r.session.clone(), r.verb.clone(), r.body.clone());
    let response = request_inner(State(state.clone()), headers, Json(r)).await;
    let (parts, b) = response.into_parts();
    let bytes = axum::body::to_bytes(b, 256 << 20).await.unwrap_or_default();
    let mut v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let event = route_audit_event(&verb, &body, &v, parts.status, started.elapsed().as_millis() as u64);
    super::browser::record_browser_event(&state, Some(&session), &session, &format!("route.{verb}"), event).await;
    if v.is_null() {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }
    // A route hands back the CDP port of the browser it drove; the raw port
    // is withheld unless the caller says why the route cannot do the job.
    let why = body.get("cdp_reason").and_then(Value::as_str);
    super::cdp_proxy::gate(&state, &session, why, &mut v,
        &[("/cdp_port", "/profile"), ("/route/cdp_port", "/route/profile"), ("/state/cdp_port", "/state/profile")]).await;
    (parts.status, Json(v)).into_response()
}

/// The audit record for one route call. Pure, so redaction is testable.
fn route_audit_event(verb: &str, body: &Value, v: &Value, status: StatusCode, latency_ms: u64) -> Value {
    let first_str = |ptrs: &[&str]| ptrs.iter().find_map(|p| v.pointer(p).and_then(Value::as_str)).map(str::to_string);
    let page = first_str(&["/url", "/data/url", "/state/url", "/page/url", "/launch_url"]).unwrap_or_default();
    let mut ev = if verb == "action" {
        super::browser::action_audit_fields(body, &page)
    } else {
        json!({"url": super::browser::audit_url(&page), "sensitive_values_recorded": false})
    };
    let ok = status.is_success() && v.get("ok").and_then(Value::as_bool) != Some(false) && v.get("error").is_none();
    let error = v.get("error").map(|e| {
        let t = e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string());
        let t = super::session_verbs::redact_secrets(&t);
        t.chars().take(300).collect::<String>()
    });
    if let Some(o) = ev.as_object_mut() {
        o.insert("verb".into(), json!(verb));
        o.insert("url_after".into(), json!(super::browser::audit_url(&page)));
        if let Some(target) = body.get("url").and_then(Value::as_str) {
            o.insert("target_url".into(), json!(super::browser::audit_url(target)));
        }
        o.insert("backend".into(), v.pointer("/route/backend").cloned().unwrap_or(Value::Null));
        o.insert("profile".into(), v.pointer("/route/profile").cloned().unwrap_or(Value::Null));
        o.insert("page_ready".into(), v.get("page_ready").cloned().unwrap_or(Value::Null));
        o.insert("http_status".into(), json!(status.as_u16()));
        o.insert("ok".into(), json!(ok));
        o.insert("error".into(), error.map(Value::from).unwrap_or(Value::Null));
        o.insert("latency_ms".into(), json!(latency_ms));
        if let Some(shot) = first_str(&["/path", "/screenshot_path", "/data/path"]) {
            o.insert("screenshot".into(), json!(shot));
        }
        o.insert("via".into(), json!("route"));
    }
    ev
}

async fn request_inner(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut r): Json<Request>,
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
        "goal",
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
    // A start that names no profile gets the profile chosen for its site, by
    // the same selector as `amux browser for` (found 2026-10-17: a route to the
    // TubeScience app opened the saved default `ethan` and hit a sign-in page
    // while `amux browser for` named `ethan-tubescience`). The saved native
    // profile is the fallback when the URL names no public site.
    let named = if r.verb == "start" {
        r.body.get("profile").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
    } else {
        receipt.get("selected_profile").and_then(Value::as_str).map(str::to_owned)
    };
    let mut chosen_by = "named";
    let chosen = match named {
        Some(p) => p,
        None => {
            let url = r.body.get("url").and_then(Value::as_str).unwrap_or("").to_owned();
            let worker = r.session.clone();
            let site_pick = if r.verb == "start" && !url.is_empty() {
                tokio::task::spawn_blocking(move || super::browser::profile_for_url(&worker, &url)).await.ok().flatten()
            } else {
                None
            };
            match site_pick.filter(|c| !c.profile.is_empty()) {
                Some(c) => {
                    chosen_by = "site";
                    tracing::info!(session = %r.session, profile = %c.profile, signed_in = c.signed_in, why = %c.why,
                        measured = true, n_considered = 1, verdict = "browser_route_profile_for_site",
                        "route start named no profile; using the profile chosen for the site");
                    c.profile
                }
                None => {
                    chosen_by = "default";
                    c.native_profile.clone()
                }
            }
        }
    };
    if r.verb == "start" {
        if let Some(o) = r.body.as_object_mut() {
            o.entry("profile").or_insert(json!(chosen));
            o.insert("profile_chosen_by".into(), json!(chosen_by));
        }
    }
    if let Err(denied) = super::browser_scope::profile_allowed(&r.session, &chosen) {
        return denied.response();
    }
    // Replacement includes cleanup of the previous route. Reauthorize that
    // route's original profile just as explicit stop does, before any mutation.
    if r.verb == "start" {
        if let Some(previous) = receipt.get("selected_profile").and_then(Value::as_str) {
            if previous != chosen {
                if let Err(denied) = super::browser_scope::profile_allowed(&r.session, previous) {
                    tracing::warn!(session=%r.session, previous_profile=previous,
                        selected_profile=%chosen, measured=true, n_considered=2,
                        verdict="browser_route_replacement_scope_denied",
                        "replacement refused before cleaning an out-of-scope route");
                    return denied.response();
                }
            }
        }
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
    // One spelling of each action for every backend: `evaluate` is eval,
    // `fill` is input, and `help` lists them (the same table as the native
    // /api/browser/action).
    if r.verb == "action" {
        let canon = super::browser::normalize_action_body(&mut r.body);
        if canon.is_empty() || canon == "help" {
            return Json(super::browser::actions_help(&canon)).into_response();
        }
    }
    // EVERY ROUTE VERB IS USE, `state` and `keepalive` included. The idle
    // reaper read only amux's native verbs and the receipt's mtime, so a worker
    // keeping its browser alive by polling `route state` every 2 minutes lost
    // it after 5 (NetSuite, 2026-10-26). Touched before the verb runs: an
    // attempt is use even when the verb then fails.
    if r.verb != "stop" {
        crate::integrations::browser::touch_verb_for_session(&r.session);
        for p in ["profile", "selected_profile"] {
            if let Some(p) = receipt.get(p).and_then(Value::as_str) {
                crate::integrations::browser::touch_verb(p);
            }
        }
        let path = root().join("sessions").join(format!("{}.json", &receipt_key[..24]));
        if let Ok(f) = std::fs::File::options().append(true).open(&path) {
            let _ = f.set_modified(std::time::SystemTime::now());
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
    let (methods, method_errors) = methods();
    for e in &method_errors {
        tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "browser_methods_unloadable", "a browser-methods layer did not parse; using the rest");
    }
    context["methods"] = methods;
    if r.verb == "goal" {
        match adapter_modules() {
            Ok(m) => context["agent_modules"] = m,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("could not write the browser agent adapters: {e}")),
        }
    }
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
    let deadline_s = if r.verb == "goal" || (c.allow_cua && r.verb != "stop") {
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
                tracing::info!(verdict="browser_route_result",session=%r.session,verb=%r.verb,backend,status=code.as_u16(),attempts=?v.get("attempts").or_else(||v.pointer("/route/attempts")),cleanup_verdict=?v.get("cleanup_verdict"),cleanup_events=?v.get("cleanup_events"),"browser ladder request completed");
                (code, Json(v)).into_response()
            }
            Err(e) => {
                // Driver input and output can contain credentials and page
                // data. Retain process/size evidence without logging either.
                tracing::warn!(verdict="browser_route_driver_invalid_json",session=%r.session,verb=%r.verb,exit_code=?out.status.code(),stdout_bytes=out.stdout.len(),stderr_bytes=out.stderr.len(),"browser driver returned invalid JSON");
                error(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "browser driver did not return JSON (exit {}, stdout {} bytes, stderr {} bytes): {e}",
                        out.status, out.stdout.len(), out.stderr.len()
                    ),
                )
            }
        },
        Ok(Err(e)) => error(StatusCode::BAD_GATEWAY, e),
        Err(_) => error(
            StatusCode::GATEWAY_TIMEOUT,
            format!("browser route exceeded {deadline_s} seconds"),
        ),
    }
}
