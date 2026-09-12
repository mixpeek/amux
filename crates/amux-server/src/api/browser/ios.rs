//! Real local iOS Safari, using Apple's WebDriver. Shares the browser verbs
//! and audit trail; never impersonates iOS by changing Chrome's user agent.
use super::*;
use base64::Engine;
use serde::Serialize;
use std::sync::LazyLock;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

static DRIVER: LazyLock<Mutex<Option<Driver>>> = LazyLock::new(|| Mutex::new(None));
const ELEMENT: &str = "element-6066-11e4-a52e-4f735466cecf";
const LIMITATIONS: &str = "Real local iOS Safari. Native input requires the configured Appium/XCTest driver; Safari-only mode uses WebKit editing commands. Chrome profiles and CDP inspection are unavailable. Physical-device behavior requires separate tests.";

#[derive(Serialize, Deserialize)]
struct Driver {
    owner: String,
    port: u16,
    pid: u32,
    id: String,
    udid: String,
    capabilities: Value,
    #[serde(default)]
    native: bool,
    #[serde(skip)]
    child: Option<Child>,
}

type Result<T> = std::result::Result<T, (StatusCode, String)>;
fn failure(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, e.to_string())
}
fn reply(result: Result<Value>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err((status, error)) => {
            tracing::warn!(target: "amux::browser_ios", verdict="failed", %status, %error, "iOS browser operation failed");
            err(status, json!({"error":error,"backend":"ios-simulator"}))
        }
    }
}
fn owner(value: Option<&str>, headers: &HeaderMap) -> Result<String> {
    explicit_session(value, headers).ok_or((StatusCode::BAD_REQUEST,
        "iOS browser requires explicit session or X-Amux-Session; anonymous callers cannot own Safari".into()))
}
fn record_path() -> std::path::PathBuf {
    chrome::amux_home().join("browser-ios.json")
}
async fn persist(d: &Driver) -> Result<()> {
    let path = record_path();
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .map_err(failure)?;
    let bytes = serde_json::to_vec(d).map_err(failure)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes).await.map_err(failure)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(failure)?;
    }
    tokio::fs::rename(tmp, path).await.map_err(failure)
}
async fn lock_driver() -> Result<tokio::sync::MutexGuard<'static, Option<Driver>>> {
    let mut guard = tokio::time::timeout(Duration::from_secs(5), DRIVER.lock())
        .await
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                "iOS browser is busy; retry after the current operation completes".into(),
            )
        })?;
    if guard.is_none() {
        match tokio::fs::read(record_path()).await {
            Ok(bytes) => *guard = Some(serde_json::from_slice(&bytes).map_err(failure)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(failure(e)),
        }
    }
    Ok(guard)
}
fn owned<'a>(slot: &'a mut Option<Driver>, session: &str) -> Result<&'a mut Driver> {
    let d = slot.as_mut().ok_or((
        StatusCode::CONFLICT,
        "No iOS browser session; POST /api/browser/ios/start first".into(),
    ))?;
    if d.owner != session {
        return Err((StatusCode::CONFLICT, format!("iOS Safari is owned by {:?}; that worker must stop its session before another worker starts", d.owner)));
    }
    Ok(d)
}
fn target_owned<'a>(
    slot: &'a mut Option<Driver>,
    session: &str,
    headers: &HeaderMap,
) -> Result<&'a mut Driver> {
    let d = owned(slot, session)?;
    if headers
        .get("X-Amux-Simulator")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|id| id != d.udid)
    {
        return Err((StatusCode::CONFLICT, "Selected simulator differs from the running session; stop the original device session before switching".into()));
    }
    Ok(d)
}
async fn webdriver(
    port: u16,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    // First native launch may build XCTest. Ordinary operations retain a
    // short deadline; neither transport nor response body can wait forever.
    let deadline = if path == "/session" { 180 } else { 35 };
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(deadline))
        .build()
        .map_err(failure)?;
    let verb = method.to_string();
    let transport_error = |error: reqwest::Error| {
        let timed_out = error.is_timeout();
        tracing::warn!(target:"amux::browser_ios",verdict="webdriver_transport_failed",measured=true,
            n_considered=1,method=%verb,operation=%path,deadline_s=deadline,timed_out,
            "iOS WebDriver did not acknowledge the operation");
        (if timed_out { StatusCode::GATEWAY_TIMEOUT } else { StatusCode::BAD_GATEWAY },
            format!("Safari WebDriver {verb} {path}: {}; deadline {deadline}s; action outcome may be unknown",
                if timed_out { "timed out" } else { "transport failed" }))
    };
    let mut request = client.request(method, format!("http://127.0.0.1:{port}{path}"));
    if let Some(b) = body {
        request = request.json(&b);
    }
    let response = request.send().await.map_err(&transport_error)?;
    let status = response.status();
    let data: Value = response.json().await.map_err(transport_error)?;
    if !status.is_success() || data["value"]["error"].is_string() {
        // Don't log URLs, scripts or typed text echoed by the browser.
        return Err(failure(format!(
            "Safari WebDriver {verb} {path} HTTP {status}: {}",
            data["value"]["error"].as_str().unwrap_or("request failed")
        )));
    }
    Ok(data["value"].clone())
}
impl Driver {
    async fn command(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        webdriver(
            self.port,
            method,
            &format!("/session/{}{path}", self.id),
            body,
        )
        .await
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.command(reqwest::Method::POST, path, Some(body)).await
    }
    async fn eval(&self, expression: &str, args: Value) -> Result<Value> {
        self.post(
            "/execute/sync",
            json!({"script":format!("return ({expression});"),"args":args}),
        )
        .await
    }
}
async fn simctl(args: &[&str]) -> Result<Value> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("/usr/bin/xcrun")
            .arg("simctl")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| failure("simctl inventory timed out after 10s"))?
    .map_err(failure)?;
    if !output.status.success() {
        return Err(failure("simctl unavailable; install/select Xcode and an iOS Simulator runtime on the server Mac"));
    }
    serde_json::from_slice(&output.stdout).map_err(failure)
}
fn inventory(data: &Value) -> Vec<Value> {
    let runtimes = data["runtimes"].as_array().cloned().unwrap_or_default();
    let mut targets = Vec::new();
    for runtime in runtimes.iter().filter(|r| {
        r["isAvailable"] == true
            && r["identifier"]
                .as_str()
                .is_some_and(|s| s.contains(".iOS-"))
    }) {
        let key = runtime["identifier"].as_str().unwrap_or("");
        for device in data["devices"][key]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|d| d["isAvailable"] == true)
        {
            targets.push(json!({"udid":device["udid"],"name":device["name"],"version":runtime["version"],"state":device["state"],
                "label":format!("iOS {} · {}",runtime["version"].as_str().unwrap_or("?"),device["name"].as_str().unwrap_or("Simulator"))}));
        }
    }
    targets.sort_by_key(|v| {
        (
            v["state"] != "Booted",
            v["label"].as_str().unwrap_or("").to_owned(),
        )
    });
    targets
}
async fn targets() -> Response {
    match simctl(&["list", "--json"]).await {
        Ok(data) => {
            let list = inventory(&data);
            Json(json!({"measured":true,"n_considered":list.len(),"targets":list,"limitations":LIMITATIONS})).into_response()
        }
        Err((_, reason)) => {
            tracing::info!(target: "amux::browser_ios", measured=false, why_unmeasured=%reason, "iOS simulator discovery unavailable");
            Json(json!({"measured":false,"n_considered":0,"targets":[],"why_unmeasured":reason,"limitations":LIMITATIONS})).into_response()
        }
    }
}

async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let result = async {
        let session = owner(body["session"].as_str(), &headers)?;
        let udid = body["udid"].as_str().filter(|s| !s.is_empty()).ok_or((StatusCode::BAD_REQUEST,"udid required; GET /api/browser/ios/targets".into()))?;
        let url = body["url"].as_str().unwrap_or("about:blank");
        if !url.starts_with("https://") && !url.starts_with("http://") && url != "about:blank" {
            return Err((StatusCode::BAD_REQUEST, "url must be http(s) or about:blank".into()));
        }
        let mut slot = lock_driver().await?;
        if slot.is_some() {
            let d = owned(&mut slot, &session)?;
            if d.udid != udid { return Err((StatusCode::CONFLICT,"Stop your current iOS browser before changing simulator devices".into())); }
            d.post("/url", json!({"url":url})).await?;
        } else {
            let devices = inventory(&simctl(&["list","--json"]).await?);
            let target = devices.iter().find(|d| d["udid"] == udid).ok_or((StatusCode::BAD_REQUEST,"Simulator no longer available; refresh targets".into()))?;
            // Require an explicitly booted target. Starting Safari must not silently
            // boot several GB of additional simulator on a contended worker host.
            if target["state"] != "Booted" { return Err((StatusCode::CONFLICT,"Open this device in Simulator first, then retry Go".into())); }
            let native_port = match std::env::var("AMUX_IOS_WEBDRIVER_PORT") {
                Ok(value) => Some(value.parse::<u16>().ok().filter(|p| *p > 0).ok_or(failure("AMUX_IOS_WEBDRIVER_PORT must be a valid nonzero port"))?),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => return Err(failure("AMUX_IOS_WEBDRIVER_PORT is invalid")),
            };
            let (port, child) = if let Some(port) = native_port { (port, None) } else {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(failure)?;
                let port = listener.local_addr().map_err(failure)?.port();
                drop(listener);
                let child = Command::new("/usr/bin/safaridriver").args(["--port", &port.to_string()])
                    .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                    .kill_on_drop(true).spawn().map_err(failure)?;
                (port, Some(child))
            };
            let native = native_port.is_some();
            let mut d = Driver { owner:session.clone(), port, pid:child.as_ref().and_then(Child::id).unwrap_or(0), id:String::new(), udid:udid.into(), capabilities:Value::Null, native, child };
            let ready = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if webdriver(port, reqwest::Method::GET, "/status", None).await.is_ok() { break; }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }).await;
            if ready.is_err() { return Err(failure("iOS WebDriver did not become ready; start the configured local driver")); }
            let mut caps = if native { json!({
                "platformName":"iOS","browserName":"Safari","appium:automationName":"XCUITest",
                "appium:udid":udid,"appium:platformVersion":target["version"],"appium:nativeWebTap":true,
                "appium:noReset":true,"appium:forceAppLaunch":false,"appium:shouldTerminateApp":false,
                "appium:safariInitialUrl":url,"appium:waitForIdleTimeout":1
            }) } else { json!({"platformName":"iOS","browserName":"Safari","safari:useSimulator":true,"safari:deviceUDID":udid,"acceptInsecureCerts":true}) };
            if native {
                if let Ok(native_url) = std::env::var("AMUX_IOS_NATIVE_URL") {
                    let parsed = reqwest::Url::parse(&native_url).map_err(failure)?;
                    if parsed.scheme() != "http" || parsed.host_str() != Some("127.0.0.1") || parsed.port().is_none()
                        || std::env::var("AMUX_IOS_NATIVE_UDID").ok().as_deref() != Some(udid) {
                        return Err(failure("Native driver URL must use loopback and match AMUX_IOS_NATIVE_UDID"));
                    }
                    caps["appium:webDriverAgentUrl"] = json!(native_url);
                }
            }
            let created = webdriver(port, reqwest::Method::POST, "/session", Some(json!({"capabilities":{"alwaysMatch":caps}}))).await?;
            d.id = created["sessionId"].as_str().ok_or(failure("WebDriver did not return sessionId"))?.into();
            d.capabilities = created["capabilities"].clone();
            let matches = if native {d.capabilities["udid"] == udid && d.capabilities["platformVersion"] == target["version"]}
                else {d.capabilities["safari:useSimulator"] == true && d.capabilities["safari:deviceUDID"] == udid};
            if !matches {
                let _ = d.command(reqwest::Method::DELETE,"",None).await;
                return Err(failure("WebDriver returned the wrong simulator; session rejected"));
            }
            // Persist ownership before navigation; a failed load still has a
            // truthful stop/retry path, including after a server restart.
            persist(&d).await?;
            *slot = Some(d);
            owned(&mut slot,&session)?.post("/timeouts",json!({"pageLoad":30000,"script":30000,"implicit":0})).await?;
            owned(&mut slot,&session)?.post("/url",json!({"url":url})).await?;
        }
        let d = owned(&mut slot,&session)?;
        let landed = d.command(reqwest::Method::GET,"/url",None).await?;
        let result = json!({"ok":true,"backend":"ios-simulator","session":session,"launch_url":landed,"capabilities":d.capabilities,"limitations":LIMITATIONS});
        drop(slot);
        record_browser_event(&state,Some(&session),&session,"started",json!({"backend":"ios-simulator","requested_url":audit_url(url)})).await;
        Ok(result)
    }.await;
    reply(result)
}
async fn status(headers: HeaderMap, Query(q): Query<SessionQuery>) -> Response {
    reply(async {
        let session = owner(q.session.as_deref(),&headers)?;
        let mut slot = lock_driver().await?;
        let Some(d) = slot.as_mut() else { return Ok(json!({"running":false,"measured":true,"n_considered":0,"backend":"ios-simulator"})); };
        let probe = d.command(reqwest::Method::GET,"/url",None).await;
        Ok(json!({"running":probe.is_ok(),"measured":probe.is_ok(),"n_considered":1,"owner":d.owner,"owned":d.owner==session,
            "capabilities":d.capabilities,"backend":"ios-simulator","why_unmeasured":probe.err().map(|(_,s)|s),"limitations":LIMITATIONS}))
    }.await)
}
async fn stop(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    reply(async {
        let session = owner(body["session"].as_str(),&headers)?;
        let mut slot = lock_driver().await?;
        let d = target_owned(&mut slot,&session,&headers)?;
        let deleted = d.command(reqwest::Method::DELETE,"",None).await;
        if let Some(child) = d.child.as_mut() {
            child.kill().await.map_err(failure)?;
        } else if d.pid > 0 {
            // Recovered after restart: verify PID identity before signaling it.
            let output = Command::new("/bin/ps").args(["-p", &d.pid.to_string(), "-o", "command="]).output().await.map_err(failure)?;
            let command = String::from_utf8_lossy(&output.stdout);
            if command.trim() == format!("/usr/bin/safaridriver --port {}",d.port) {
                Command::new("/bin/kill").args(["-TERM",&d.pid.to_string()]).status().await.map_err(failure)?;
            } else if deleted.is_err() && !command.trim().is_empty() {
                tracing::warn!(target: "amux::browser_ios", verdict="stale_pid_released", "Recorded safaridriver PID was reused; unrelated process left untouched");
            }
        }
        if d.native {
            if let Err(error) = deleted {
                if !error.1.contains("invalid session id") {return Err(error);}
            }
        }
        tokio::fs::remove_file(record_path()).await.map_err(failure)?;
        *slot=None;
        drop(slot);
        record_browser_event(&state,Some(&session),&session,"stopped",json!({"backend":"ios-simulator"})).await;
        Ok(json!({"ok":true,"backend":"ios-simulator","session":session}))
    }.await)
}
async fn state_verb(headers: HeaderMap, Query(q): Query<SessionQuery>) -> Response {
    reply(
        async {
            let session = owner(q.session.as_deref(), &headers)?;
            let mut slot = lock_driver().await?;
            let mut v = target_owned(&mut slot, &session, &headers)?
                .eval(&chrome::state_js(), json!([]))
                .await?;
            v["text"] = json!(chrome::obs_cap(
                v["text"].as_str().unwrap_or(""),
                chrome::obs_state_cap()
            ));
            v["ok"] = json!(true);
            v["backend"] = json!("ios-simulator");
            v["session"] = json!(session);
            Ok(v)
        }
        .await,
    )
}
async fn screenshot(headers: HeaderMap, Query(q): Query<SessionQuery>) -> Response {
    reply(async {
        let session=owner(q.session.as_deref(),&headers)?;
        let mut slot=lock_driver().await?;
        let d=target_owned(&mut slot,&session,&headers)?;
        let (encoded, viewport) = if d.native {
            let viewport = d.eval("({w:screen.width,h:screen.height})",json!([])).await?;
            (d.command(reqwest::Method::GET,"/screenshot",None).await?, viewport)
        } else {
        // Safari's full screenshot includes system insets (62 CSS px above
        // content on the measured iPhone), despite innerHeight excluding them.
        // Element Screenshot uses web-content coordinates. A transparent,
        // noninteractive viewport rectangle gives exact pixels without guessed
        // device-specific inset tables or image processing.
        let capture=d.eval("(function(){var e=document.createElement('div');e.setAttribute('aria-hidden','true');e.style.cssText='all:initial;position:fixed;left:0;top:0;width:'+innerWidth+'px;height:'+innerHeight+'px;pointer-events:none;background:transparent;';document.documentElement.appendChild(e);return {element:e,viewport:{w:innerWidth,h:innerHeight}}})()",json!([])).await?;
        let element_id=capture["element"][ELEMENT].as_str().ok_or(failure("Safari did not return the viewport capture element"))?;
        let image=d.command(reqwest::Method::GET,&format!("/element/{element_id}/screenshot"),None).await;
        let removed=d.eval("arguments[0].remove()",json!([capture["element"]])).await;
        removed?;
        let encoded=image?;
            (encoded.clone(), capture["viewport"].clone())
        };
        let bytes=base64::engine::general_purpose::STANDARD.decode(encoded.as_str().ok_or(failure("Safari returned no screenshot"))?).map_err(failure)?;
        let file=shot_path(&session);
        tokio::fs::create_dir_all(file.parent().unwrap()).await.map_err(failure)?;
        tokio::fs::write(&file,&bytes).await.map_err(failure)?;
        Ok(json!({"ok":true,"path":file,"size":bytes.len(),"viewport":viewport,"coordinate_space":if d.native {"device-points"} else {"web-viewport"},"backend":"ios-simulator","session":session,
            "serve":format!("/api/browser/ios/screenshot/file?session={}",urlenc(&session))}))
    }.await)
}
fn shot_path(session: &str) -> std::path::PathBuf {
    chrome::amux_home()
        .join("browser-screenshots")
        .join(format!("ios-{}.png", chrome::safe_file_component(session)))
}
async fn screenshot_file(headers: HeaderMap, Query(q): Query<SessionQuery>) -> Response {
    let result = async {
        let session = owner(q.session.as_deref(), &headers)?;
        let mut slot = lock_driver().await?;
        target_owned(&mut slot, &session, &headers)?;
        tokio::fs::read(shot_path(&session)).await.map_err(failure)
    }
    .await;
    match result {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => reply(Err(e)),
    }
}
fn key(value: &str) -> Option<&'static str> {
    Some(match value {
        "Enter" => "\u{e007}",
        "Tab" => "\u{e004}",
        "Backspace" => "\u{e003}",
        "Delete" => "\u{e017}",
        "Escape" => "\u{e00c}",
        "ArrowLeft" => "\u{e012}",
        "ArrowUp" => "\u{e013}",
        "ArrowRight" => "\u{e014}",
        "ArrowDown" => "\u{e015}",
        "Home" => "\u{e011}",
        "End" => "\u{e010}",
        "PageUp" => "\u{e00e}",
        "PageDown" => "\u{e00f}",
        _ => return None,
    })
}
async fn element(d: &Driver, body: &Value) -> Result<String> {
    let value = if let Some(selector) = body["selector"].as_str() {
        d.post("/element", json!({"using":"css selector","value":selector}))
            .await?
    } else if let Some(index) = body["index"].as_u64() {
        d.eval(
            "(window.__amux_els||[])[arguments[0]] || null",
            json!([index]),
        )
        .await?
    } else {
        d.command(reqwest::Method::GET, "/element/active", None)
            .await?
    };
    value[ELEMENT].as_str().map(str::to_owned).ok_or((
        StatusCode::BAD_REQUEST,
        "Element missing or stale; GET /state again".into(),
    ))
}
// DOM hit testing cannot see UIKit's keyboard. A WebTap on a covered page
// button otherwise types whichever keyboard key occupies that screen point.
async fn clear_native_keyboard(d: &Driver) -> Result<bool> {
    let path = "/appium/device/is_keyboard_shown";
    let shown = d.command(reqwest::Method::GET, path, None).await?;
    if shown == false {
        return Ok(false);
    }
    if shown != true {
        return Err(failure(
            "Native keyboard visibility is unavailable; page tap refused",
        ));
    }
    tracing::warn!(target:"amux::browser_ios",session=%d.owner,verdict="keyboard_blocks_page_tap",measured=true,n_considered=1,"dismissing native keyboard before locating the requested page control");
    // Safari's Done lives in its input accessory toolbar, outside the native
    // keyboard subtree searched by Appium's generic hideKeyboard command.
    // Class chain uses native XCTest queries; XPath serializes the entire
    // accessibility tree and stalls on large live terminal histories.
    let context = d.command(reqwest::Method::GET, "/context", None).await?;
    if !context.is_string() || context == "NATIVE_APP" {
        return Err(failure("Safari web context unavailable; page tap refused"));
    }
    d.post("/context", json!({"name":"NATIVE_APP"})).await?;
    let dismissed=async {
        let matches=d.post("/elements",json!({"using":"-ios class chain","value":"**/XCUIElementTypeToolbar/**/XCUIElementTypeButton[`name == 'Done' AND visible == 1`]"})).await?;
        let matches=matches.as_array().filter(|v|v.len()==1).ok_or(failure("Safari keyboard Done control unavailable or ambiguous; page tap refused"))?;
        let id=matches[0][ELEMENT].as_str().ok_or(failure("Native Done control has no element identity"))?;
        d.post(&format!("/element/{id}/click"),json!({})).await
    }.await;
    // Always restore the web context, including native lookup/tap refusal.
    let restored = d.post("/context", json!({"name":context})).await;
    if let Err((status, error)) = dismissed {
        return Err((status, format!("Keyboard dismissal failed: {error}; web context restored={}", restored.is_ok())));
    }
    restored?;
    if d.command(reqwest::Method::GET, path, None).await? != false {
        return Err((
            StatusCode::CONFLICT,
            "Native keyboard still covers the page; tap refused without dispatch".into(),
        ));
    }
    Ok(true)
}

async fn perform(d: &Driver, body: &Value) -> Result<Value> {
    let action = body["action"].as_str().unwrap_or("");
    match action {
        "eval"=> {
            let script=body["script"].as_str().filter(|s|!s.trim().is_empty()).ok_or((StatusCode::BAD_REQUEST,"script expression required".into()))?;
            let value=d.post("/execute/async",json!({"script":"var done=arguments[arguments.length-1],source=arguments[0];Promise.resolve().then(function(){return (0,eval)(source)}).then(function(result){done({ok:true,result:result===undefined?null:result})},function(){done({ok:false})})","args":[script]})).await?;
            if value["ok"] != true {return Err((StatusCode::BAD_REQUEST,"Browser script failed; inspect the page state before retrying".into()));}
            Ok(value["result"].clone())
        },
        "back"=>d.post("/back",json!({})).await,
        "scroll"=> {
            let dx=body["dx"].as_f64().unwrap_or(0.0);
            let dy=body["dy"].as_f64().unwrap_or(0.0);
            if !d.native {return d.eval("window.scrollBy(arguments[0],arguments[1])",json!([dx,dy])).await;}
            let screen=d.eval("({width:screen.width,height:screen.height})",json!([])).await?;
            let w=screen["width"].as_f64().filter(|v|*v>80.0).ok_or(failure("Simulator screen width unavailable"))?;
            let h=screen["height"].as_f64().filter(|v|*v>300.0).ok_or(failure("Simulator screen height unavailable"))?;
            let x=body["x"].as_f64().unwrap_or(w/2.0).clamp(20.0,w-20.0);
            let y=body["y"].as_f64().unwrap_or(h*0.6).clamp(100.0,h-140.0);
            d.post("/execute/sync",json!({"script":"mobile: dragFromToForDuration","args":[{"duration":0.15,
                "fromX":x,"fromY":y,"toX":(x-dx).clamp(20.0,w-20.0),"toY":(y-dy).clamp(100.0,h-140.0)}]})).await?;
            Ok(json!({"input_method":"xcuitest","dispatched":true,"coordinate_space":"device-points"}))
        },
        "click"=> {
            if body["selector"].is_string() || body["index"].is_u64() {
                let id=element(d,body).await?;
                if d.native {
                    let keyboard_dismissed=clear_native_keyboard(d).await?;
                    let id=element(d,body).await?;
                    d.eval("arguments[0].scrollIntoView({block:'nearest',inline:'center',behavior:'instant'})",json!([{ELEMENT:id}])).await?;
                    // Let Safari settle any scroll-snap before checking the hit target.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if d.eval("(function(e){var r=e.getBoundingClientRect();var x=r.x+r.width/2,y=r.y+r.height/2,v=visualViewport;var visible=!v||(x>=v.offsetLeft&&x<=v.offsetLeft+v.width&&y>=v.offsetTop&&y<=v.offsetTop+v.height);var hit=document.elementFromPoint(x,y);return !!(r.width&&r.height&&visible&&hit&&e.contains(hit))})(arguments[0])",json!([{ELEMENT:id}])).await? != true {
                        return Err((StatusCode::BAD_REQUEST,"Element is hidden or covered; scroll it into view or close the covering overlay".into()));
                    }
                    d.post(&format!("/element/{id}/click"),json!({})).await?;
                    return Ok(json!({"input_method":"xcuitest","dispatched":true,"keyboard_dismissed":keyboard_dismissed}));
                }
                let point=d.eval("(function(e){e.scrollIntoView({block:'center',inline:'center'});var r=e.getBoundingClientRect();if(!r.width||!r.height)throw Error('Element is not visible');return {x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)}})(arguments[0])",json!([{ELEMENT:id}])).await?;
                d.post("/actions",json!({"actions":[{"type":"pointer","id":"pointer","parameters":{"pointerType":"mouse"},"actions":[
                    {"type":"pointerMove","duration":0,"origin":"viewport","x":point["x"],"y":point["y"]},
                    {"type":"pointerDown","button":0},{"type":"pointerUp","button":0}]}]})).await
            } else if let (Some(x),Some(y))=(body["x"].as_f64(),body["y"].as_f64()) {
                if d.native { return d.post("/execute/sync",json!({"script":"mobile: tap","args":[{"x":x,"y":y}]})).await; }
                d.post("/actions",json!({"actions":[{"type":"pointer","id":"pointer","parameters":{"pointerType":"mouse"},"actions":[
                    {"type":"pointerMove","duration":0,"origin":"viewport","x":x.round() as i64,"y":y.round() as i64},
                    {"type":"pointerDown","button":0},{"type":"pointerUp","button":0}]}]})).await
            } else { Err((StatusCode::BAD_REQUEST,"click needs selector, index, or x,y".into())) }
        },
        "key"=> {
            let value=key(body["key"].as_str().unwrap_or("")).ok_or((StatusCode::BAD_REQUEST,"Unsupported WebDriver key".into()))?;
            d.post("/actions",json!({"actions":[{"type":"key","id":"keyboard","actions":[
                {"type":"keyDown","value":value},{"type":"keyUp","value":value}]}]})).await
        },
        "type"|"input"=> {
            let text=body["text"].as_str().ok_or((StatusCode::BAD_REQUEST,"text required".into()))?;
            if action=="input" && !body["index"].is_u64() && !body["selector"].is_string() { return Err((StatusCode::BAD_REQUEST,"input needs index or selector".into())); }
            // Simulator clicks can acknowledge before UIKit has focused the
            // WebKit field. Wait for an editable focus, never type into body.
            if action=="type" && !body["selector"].is_string() && !body["index"].is_u64() {
                let mut focused=false;
                for _ in 0..10 {
                    if d.eval("!!document.activeElement && (document.activeElement.matches('input,textarea') || document.activeElement.isContentEditable)",json!([])).await? == true { focused=true;break; }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                if !focused { return Err((StatusCode::CONFLICT,"No editable field is focused; click a field or use input with selector".into())); }
            }
            let id=element(d,body).await?;
            if d.native {
                if action=="input" { d.post(&format!("/element/{id}/clear"),json!({})).await?; }
                d.post(&format!("/element/{id}/value"),json!({"text":text,"value":[text]})).await?;
                return Ok(json!({"input_method":"xcuitest","dispatched":true}));
            }
            // iOS 26.5 WebDriver sends trusted keydown/keyup but no edit: its
            // Element Send Keys returns success with the input still empty.
            // Use WebKit's editing operation explicitly, not a value setter or
            // silent retry. It emits input events and preserves editor undo.
            // The result and durable trail name the method; this is NOT proof
            // of native keyboard behavior. Never echo the field's contents.
            let result=d.eval(r#"(function(e,text,replace){
                if(!e || !(e.matches('input,textarea') || e.isContentEditable)) throw Error('Focus an editable field first');
                if(e.disabled || e.readOnly) throw Error('Field is not editable');
                e.focus();
                if(replace) { if(e.select)e.select();else {var r=document.createRange();r.selectNodeContents(e);var s=getSelection();s.removeAllRanges();s.addRange(r);} }
                var applied=document.execCommand('insertText',false,text);
                return {input_method:'webkit-editor',applied:applied,native_keyboard_tested:false};
            })(arguments[0],arguments[1],arguments[2])"#,json!([{ELEMENT:id},text,action=="input"])).await?;
            if result["applied"] != true { return Err(failure("WebKit text edit was rejected; field was not changed")); }
            Ok(result)
        },
        "extract"=>d.eval(&chrome::state_js(),json!([])).await,
        _=>Err((StatusCode::NOT_IMPLEMENTED,format!("iOS Safari action {action:?} unavailable; supported: eval, click, type, input, key, scroll, back, extract"))),
    }
}
async fn action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let result=async {
        let session=owner(body["session"].as_str(),&headers)?;
        let mut slot=lock_driver().await?;
        let result=perform(target_owned(&mut slot,&session,&headers)?,&body).await;
        drop(slot);
        record_browser_event(&state,Some(&session),&session,"action",json!({"backend":"ios-simulator","action":body["action"],"input_method":result.as_ref().ok().and_then(|v|v.get("input_method")),"ok":result.is_ok(),"http_status":result.as_ref().err().map(|(s,_)|s.as_u16()).unwrap_or(200)})).await;
        Ok(json!({"ok":true,"backend":"ios-simulator","session":session,"data":{"result":result?}}))
    }.await;
    reply(result)
}
async fn unsupported() -> Response {
    err(
        StatusCode::NOT_IMPLEMENTED,
        json!({"error":LIMITATIONS,"backend":"ios-simulator","measured":false,"n_considered":0}),
    )
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/targets", get(targets))
        .route("/start", post(start))
        .route("/status", get(status))
        .route("/stop", post(stop))
        .route("/state", get(state_verb))
        .route("/screenshot", get(screenshot))
        .route("/screenshot/file", get(screenshot_file))
        .route("/action", post(action))
        .route("/inspect", get(unsupported))
        .route("/inspect/clear", post(unsupported))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_scroll_dispatches_a_bounded_device_gesture() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/session/test/execute/sync",
            post(|Json(v): Json<Value>| async move {
                if v["script"] == "mobile: dragFromToForDuration" {
                    assert_eq!(v["args"][0]["fromX"], 200.0);
                    assert_eq!(v["args"][0]["fromY"], 500.0);
                    assert_eq!(v["args"][0]["toY"], 100.0);
                    assert_eq!(v["args"][0]["duration"], 0.15);
                    Json(json!({"value":null}))
                } else {
                    Json(json!({"value":{"width":402,"height":874}}))
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let d = Driver {
            owner: "test".into(),
            port,
            pid: 0,
            id: "test".into(),
            udid: "test".into(),
            capabilities: Value::Null,
            native: true,
            child: None,
        };
        let result = perform(&d, &json!({"action":"scroll","dy":10000,"x":200,"y":500}))
            .await
            .unwrap();
        assert_eq!(result["input_method"], "xcuitest");
        assert_eq!(result["coordinate_space"], "device-points");
        server.abort();
    }
    #[tokio::test]
    async fn native_click_dismisses_keyboard_or_refuses_without_dispatch() {
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        };
        for mode in ["refused", "still-visible", "dismissed", "dismiss-and-restore-refused"] {
            let refuse = mode != "dismissed";
            let visible = Arc::new(AtomicBool::new(true));
            let native_context = Arc::new(AtomicBool::new(false));
            let taps = Arc::new(AtomicUsize::new(0));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let keyboard = visible.clone();
            let dismiss = visible.clone();
            let clicks = taps.clone();
            let contexts = native_context.clone();
            let app = Router::new()
                .route(
                    "/session/test/element",
                    post(|| async { Json(json!({"value":{ELEMENT:"save"}})) }),
                )
                .route(
                    "/session/test/elements",
                    post(|Json(v): Json<Value>| async move {
                        assert_eq!(v["using"], "-ios class chain",
                            "keyboard lookup must not serialize the entire live page as XPath XML");
                        assert!(v["value"]
                            .as_str()
                            .unwrap()
                            .contains("XCUIElementTypeToolbar"));
                        Json(json!({"value":[{ELEMENT:"native-done"}]}))
                    }),
                )
                .route(
                    "/session/test/context",
                    get(|| async { Json(json!({"value":"WEBVIEW_test"})) }).post(
                        move |Json(v): Json<Value>| {
                            let contexts = contexts.clone();
                            async move {
                                if mode == "dismiss-and-restore-refused" && v["name"] != "NATIVE_APP" {
                                    return Json(json!({"value":{"error":"restoration refused"}}));
                                }
                                contexts.store(v["name"] == "NATIVE_APP", Ordering::SeqCst);
                                Json(json!({"value":null}))
                            }
                        },
                    ),
                )
                .route(
                    "/session/test/appium/device/is_keyboard_shown",
                    get(move || {
                        let keyboard = keyboard.clone();
                        async move { Json(json!({"value":keyboard.load(Ordering::SeqCst)})) }
                    }),
                )
                .route(
                    "/session/test/execute/sync",
                    post(|| async { Json(json!({"value":true})) }),
                )
                .route(
                    "/session/test/element/native-done/click",
                    post(move || {
                        let dismiss = dismiss.clone();
                        async move {
                            if mode == "refused" || mode == "dismiss-and-restore-refused" {
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(json!({"value":{"error":"invalid element state"}})),
                                );
                            }
                            if mode == "dismissed" {
                                dismiss.store(false, Ordering::SeqCst);
                            }
                            (StatusCode::OK, Json(json!({"value":null})))
                        }
                    }),
                )
                .route(
                    "/session/test/element/save/click",
                    post(move || {
                        let clicks = clicks.clone();
                        async move {
                            clicks.fetch_add(1, Ordering::SeqCst);
                            Json(json!({"value":null}))
                        }
                    }),
                );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let d = Driver {
                owner: "test".into(),
                port,
                pid: 0,
                id: "test".into(),
                udid: "test".into(),
                capabilities: Value::Null,
                native: true,
                child: None,
            };
            let result = perform(&d, &json!({"action":"click","selector":"#save"})).await;
            assert_eq!(native_context.load(Ordering::SeqCst), mode == "dismiss-and-restore-refused",
                "restore is attempted even after dismissal refusal");
            if mode == "dismiss-and-restore-refused" {
                let error = &result.as_ref().unwrap_err().1;
                assert!(error.contains("invalid element state"), "original failure must survive: {error}");
                assert!(error.contains("web context restored=false"), "restore failure must remain explicit: {error}");
            }
            if refuse {
                assert!(
                    result.is_err(),
                    "a native keyboard must not receive the intended DOM tap"
                );
                assert_eq!(taps.load(Ordering::SeqCst), 0);
            } else {
                assert!(result.is_ok());
                assert!(!visible.load(Ordering::SeqCst));
                assert_eq!(taps.load(Ordering::SeqCst), 1);
            }
            server.abort();
        }
    }
    #[test]
    fn discovered_version_comes_from_runtime_not_frozen_user_agent() {
        let d = json!({"runtimes":[{"isAvailable":true,"identifier":"com.apple.CoreSimulator.SimRuntime.iOS-26-5","version":"26.5"}],
            "devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-5":[{"isAvailable":true,"name":"iPhone 17","udid":"abc","state":"Booted"},{"isAvailable":false,"name":"gone"}]}});
        let t = inventory(&d);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0]["label"], "iOS 26.5 · iPhone 17");
    }
    #[test]
    fn anonymous_and_other_workers_cannot_drive_or_stop_owner() {
        assert!(owner(None, &HeaderMap::new()).is_err());
        let mut slot = Some(Driver {
            owner: "worker-a".into(),
            port: 1,
            pid: 0,
            id: "test".into(),
            udid: "abc".into(),
            capabilities: Value::Null,
            native: false,
            child: None,
        });
        assert_eq!(
            owned(&mut slot, "worker-b").err().unwrap().0,
            StatusCode::CONFLICT
        );
        assert!(owned(&mut slot, "worker-a").is_ok());
    }

    #[test]
    fn selected_device_cannot_drive_previous_device_binding() {
        let mut slot = Some(Driver {
            owner: "worker-a".into(),
            port: 1,
            pid: 0,
            id: "test".into(),
            udid: "abc".into(),
            capabilities: Value::Null,
            native: false,
            child: None,
        });
        let mut headers = HeaderMap::new();
        headers.insert("X-Amux-Simulator", "other".parse().unwrap());
        assert_eq!(
            target_owned(&mut slot, "worker-a", &headers)
                .err()
                .unwrap()
                .0,
            StatusCode::CONFLICT
        );
    }
    #[tokio::test]
    async fn webdriver_error_is_not_a_success_or_a_secret_echo() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app=Router::new().route("/session/test/execute/async",post(||async {
            (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"value":{"error":"javascript error","message":"secret typed payload"}})))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let d = Driver {
            owner: "test".into(),
            port,
            pid: 0,
            id: "test".into(),
            udid: "abc".into(),
            capabilities: Value::Null,
            native: false,
            child: None,
        };
        let error = perform(&d, &json!({"action":"eval","script":"1+1"}))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
        assert!(error.1.contains("javascript error"));
        assert!(!error.1.contains("secret"));
        server.abort();
    }
    #[tokio::test]
    async fn typing_names_editor_method_and_keys_use_real_key_actions() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route(
                "/session/test/element",
                post(|| async { Json(json!({"value":{ELEMENT:"input1"}})) }),
            )
            .route(
                "/session/test/element/active",
                get(|| async { Json(json!({"value":{ELEMENT:"input1"}})) }),
            )
            .route(
                "/session/test/execute/sync",
                post(|Json(v): Json<Value>| async move {
                    assert_eq!(v["args"][1], "hello");
                    assert!(v["script"].as_str().unwrap().contains("execCommand"));
                    Json(json!({"value":{"applied":true,"input_method":"webkit-editor"}}))
                }),
            )
            .route(
                "/session/test/actions",
                post(|Json(v): Json<Value>| async move {
                    assert_eq!(v["actions"][0]["type"], "key");
                    assert_eq!(v["actions"][0]["actions"][0]["value"], "\u{e004}");
                    Json(json!({"value":"tabbed"}))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let d = Driver {
            owner: "test".into(),
            port,
            pid: 0,
            id: "test".into(),
            udid: "abc".into(),
            capabilities: Value::Null,
            native: false,
            child: None,
        };
        assert_eq!(
            perform(
                &d,
                &json!({"action":"type","selector":"#field","text":"hello"})
            )
            .await
            .unwrap()["input_method"],
            "webkit-editor"
        );
        assert_eq!(
            perform(&d, &json!({"action":"key","key":"Tab"}))
                .await
                .unwrap(),
            "tabbed"
        );
        assert_eq!(
            perform(&d, &json!({"action":"viewport","device":"iphone"}))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_IMPLEMENTED
        );
        server.abort();
    }
}
