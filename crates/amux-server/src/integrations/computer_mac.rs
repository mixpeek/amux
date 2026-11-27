//! Computer use on this Mac's own apps, through trycua's `cua-driver`
//! (owner, 2026-11-22: build Athena's computer use into amux; teardown in
//! `research/athena-mac-app-teardown.md`).
//!
//! The Mac app (desktop/amux-computer.swift) runs the driver embedded, so it
//! uses Amux's own Accessibility and Screen Recording grants. The driver is
//! the app's child over stdin and stdout (`cua-driver mcp --direct
//! --embedded`): it has no socket, so nothing but the app can reach it. The
//! app writes `<AMUX_HOME>/cua/endpoint.json` naming the driver's pid, and
//! long-polls this server for work over the owner-bearer API ([`Broker`]).
//! A worker's tool call is queued for it only after a gate this file owns:
//!
//! - every tool is classified: free (no app: list_apps, list_windows, ...),
//!   app (acts on or reads one app), or refused (kill_app, run_script, the
//!   clipboard, the full display, its own config and updates, its browser
//!   tools, whose job `amux browser` does);
//! - an app tool names its app by `pid` (or `bundle_id`/`name` to launch),
//!   and the app must be in the worker's `AMUX_MAC_APPS` (scoped like every
//!   other switch: worker > group > global, process env first);
//! - terminals, automation apps, password managers, System Settings, the
//!   macOS security prompts and amux itself are refused even when listed,
//!   unless `AMUX_MAC_ANY_APP=1`;
//! - foreground delivery (the driver taking the owner's real pointer) needs
//!   `AMUX_MAC_FOREGROUND=1`.
//!
//! Each refusal carries a `code` and a `next` line saying what to do instead,
//! as Athena's do, so a model recovers rather than retrying the same call.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const APPS_KEY: &str = "AMUX_MAC_APPS";
pub const ANY_APP_KEY: &str = "AMUX_MAC_ANY_APP";
pub const FOREGROUND_KEY: &str = "AMUX_MAC_FOREGROUND";
/// Unix seconds until which this worker may act after reading outside
/// content. The owner's Approve on the card writes it (capabilities.rs).
pub const OUTSIDE_UNTIL_KEY: &str = "AMUX_MAC_OUTSIDE_UNTIL";
/// How long outside content (an email, a web page) counts as in a worker's
/// context, and how long one approval opens the gate for.
pub const OUTSIDE_WINDOW_S: i64 = 30 * 60;
pub const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// What the Mac app publishes when the driver is up.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub binary: PathBuf,
    pub pid: i32,
    pub app: String,
}

pub fn endpoint_path(home: &Path) -> PathBuf {
    home.join("cua").join("endpoint.json")
}

/// The running driver, or why there is none (said to the caller as is).
pub fn endpoint(home: &Path) -> Result<Endpoint, String> {
    let p = endpoint_path(home);
    let raw = std::fs::read_to_string(&p).map_err(|_| {
        "the amux Mac app is not running computer use (no cua/endpoint.json). Turn on Settings > \
         Computer use in the amux Mac app on this Mac"
            .to_string()
    })?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", p.display()))?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let pid = v.get("pid").and_then(Value::as_i64).unwrap_or(0) as i32;
    let ep = Endpoint {
        binary: PathBuf::from(s("binary")),
        pid,
        app: s("app"),
    };
    // An app from before the broker publishes a socket and runs a daemon that
    // answers anyone on it (amux-security, 2026-11-23). Refuse it rather than
    // talk to it; the app's update ends that daemon and removes the socket.
    if !s("socket").is_empty() || home.join("cua").join("driver.sock").exists() {
        tracing::warn!(measured = true, n_considered = 1, verdict = "computer_mac_socket_driver_refused",
            "[computer-mac] the Mac app runs the old socket driver; nothing is sent to it");
        return Err("the amux Mac app runs an old computer-use driver that listens on a socket; \
                    update the amux Mac app (Check for Updates) and nothing is sent until then"
            .to_string());
    }
    if pid <= 0 || !alive(pid) {
        return Err(format!(
            "the computer-use driver the Mac app started (pid {pid}) is not running; reopen the amux Mac app"
        ));
    }
    if !ep.binary.is_file() {
        return Err(format!("driver binary {} is missing", ep.binary.display()));
    }
    Ok(ep)
}

fn alive(pid: i32) -> bool {
    // kill(pid, 0): permission errors still mean the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// Needs no app: discovery, permissions, guides, sessions.
    Free,
    /// Reads or acts on one app, named by pid (or bundle id/name to launch).
    App,
    Refused(&'static str),
}

pub fn classify(tool: &str) -> Class {
    match tool {
        "list_apps" | "list_windows" | "get_screen_size" | "check_permissions" | "get_guide"
        | "get_cursor_position" | "health_report" | "list_sessions" | "get_session"
        | "start_session" | "end_session" | "get_agent_cursor_state" | "set_agent_cursor_enabled"
        | "set_agent_cursor_motion" | "set_agent_cursor_theme" | "get_recording_state" => Class::Free,
        "get_window_state" | "click" | "double_click" | "right_click" | "drag" | "scroll"
        | "type_text" | "press_key" | "hotkey" | "set_value" | "invoke_menu" | "launch_app"
        | "bring_to_front" | "set_window_frame" | "move_cursor" | "run_steps"
        | "parse_visual_regions" => Class::App,
        "kill_app" => Class::Refused("ending an app is the owner's call"),
        "run_script" => Class::Refused("a script runs many actions past the per-app gate; use run_steps"),
        "clipboard_read" | "clipboard_write" => Class::Refused("the clipboard is the owner's"),
        "get_desktop_state" => Class::Refused(
            "the whole display can show anything the owner has open; read one app's window with get_window_state",
        ),
        "set_config" | "get_config" | "check_for_update" | "install_extension" | "install_ffmpeg"
        | "agent_display" => Class::Refused("the Mac app owns the driver's configuration"),
        "start_recording" | "stop_recording" | "replay_trajectory" => {
            Class::Refused("recording is not offered through amux yet")
        }
        t if t.starts_with("browser_") || t == "page" || t == "get_browser_state" => Class::Refused(
            "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
        ),
        "get_accessibility_tree" => Class::Refused("deprecated by the driver; use get_window_state"),
        _ => Class::Refused("not a tool amux knows; GET /api/computer/mac/tools lists them"),
    }
}

/// Apps refused even when listed, unless AMUX_MAC_ANY_APP=1: (bundle id, name, why).
pub const BLOCKED: &[(&str, &str, &str)] = &[
    (
        "com.apple.Terminal",
        "Terminal",
        "a terminal runs any command as the owner",
    ),
    (
        "com.googlecode.iterm2",
        "iTerm",
        "a terminal runs any command as the owner",
    ),
    (
        "dev.warp.Warp-Stable",
        "Warp",
        "a terminal runs any command as the owner",
    ),
    (
        "net.kovidgoyal.kitty",
        "kitty",
        "a terminal runs any command as the owner",
    ),
    (
        "org.alacritty",
        "Alacritty",
        "a terminal runs any command as the owner",
    ),
    (
        "com.mitchellh.ghostty",
        "Ghostty",
        "a terminal runs any command as the owner",
    ),
    (
        "com.github.wez.wezterm",
        "WezTerm",
        "a terminal runs any command as the owner",
    ),
    (
        "co.zeit.hyper",
        "Hyper",
        "a terminal runs any command as the owner",
    ),
    (
        "com.apple.ScriptEditor2",
        "Script Editor",
        "an automation app runs anything as the owner",
    ),
    (
        "com.apple.Automator",
        "Automator",
        "an automation app runs anything as the owner",
    ),
    (
        "com.apple.shortcuts",
        "Shortcuts",
        "an automation app runs anything as the owner",
    ),
    (
        "com.stairways.keyboardmaestro.editor",
        "Keyboard Maestro",
        "an automation app runs anything as the owner",
    ),
    (
        "org.hammerspoon.Hammerspoon",
        "Hammerspoon",
        "an automation app runs anything as the owner",
    ),
    (
        "com.1password.1password",
        "1Password",
        "a password manager holds the owner's secrets",
    ),
    (
        "com.agilebits.onepassword7",
        "1Password 7",
        "a password manager holds the owner's secrets",
    ),
    (
        "com.bitwarden.desktop",
        "Bitwarden",
        "a password manager holds the owner's secrets",
    ),
    (
        "com.apple.keychainaccess",
        "Keychain Access",
        "a password manager holds the owner's secrets",
    ),
    (
        "com.apple.Passwords",
        "Passwords",
        "a password manager holds the owner's secrets",
    ),
    (
        "com.apple.systempreferences",
        "System Settings",
        "System Settings changes this Mac for everyone",
    ),
    (
        "com.apple.SecurityAgent",
        "SecurityAgent",
        "a macOS security prompt is the owner's to answer",
    ),
    (
        "com.apple.loginwindow",
        "loginwindow",
        "a macOS security prompt is the owner's to answer",
    ),
    (
        "com.apple.MobileSMS",
        "Messages",
        "it sends messages as the owner; use amux's messaging (the imessage skill)",
    ),
    (
        "com.apple.mail",
        "Mail",
        "it sends email as the owner; email goes through the amux email API",
    ),
    (
        "com.apple.Safari",
        "Safari",
        "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
    ),
    (
        "com.google.Chrome",
        "Google Chrome",
        "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
    ),
    (
        "com.google.Chrome.canary",
        "Google Chrome Canary",
        "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
    ),
    (
        "com.brave.Browser",
        "Brave Browser",
        "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
    ),
    (
        "company.thebrowser.Browser",
        "Arc",
        "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
    ),
    (
        "com.microsoft.edgemac",
        "Microsoft Edge",
        "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
    ),
    (
        "org.mozilla.firefox",
        "Firefox",
        "web pages go through amux browser (amux browser route), which has the owner's profiles and audit",
    ),
    (
        "com.apple.finder",
        "Finder",
        "it opens any file or app as the owner, terminals and scripts included",
    ),
    (
        "com.apple.ActivityMonitor",
        "Activity Monitor",
        "it quits and kills any process as the owner",
    ),
    (
        "com.microsoft.VSCode",
        "Code",
        "its built-in terminal runs any command as the owner",
    ),
    (
        "com.todesktop.230313mzl4w4u92",
        "Cursor",
        "its built-in terminal runs any command as the owner",
    ),
    (
        "dev.zed.Zed",
        "Zed",
        "its built-in terminal runs any command as the owner",
    ),
    (
        "com.apple.dt.Xcode",
        "Xcode",
        "it builds and runs code and scripts as the owner",
    ),
    (
        "com.anthropic.claudefordesktop",
        "Claude",
        "an agent with the owner's tools and sign-ins",
    ),
    (
        "com.openai.chat",
        "ChatGPT",
        "an agent with the owner's sign-ins",
    ),
    (
        "com.amux.desktop",
        "Amux",
        "a worker steering amux itself could approve its own requests",
    ),
    (
        "com.amux.desktop.dev",
        "Amux Dev",
        "a worker steering amux itself could approve its own requests",
    ),
];

/// One app a call touches, as far as the request names it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct App {
    pub bundle_id: String,
    pub name: String,
    pub pid: Option<i64>,
}

impl App {
    pub fn label(&self) -> String {
        match (self.name.is_empty(), self.bundle_id.is_empty()) {
            (false, false) => format!("{} ({})", self.name, self.bundle_id),
            (false, true) => self.name.clone(),
            (true, false) => self.bundle_id.clone(),
            _ => self
                .pid
                .map(|p| format!("pid {p}"))
                .unwrap_or_else(|| "an unnamed app".into()),
        }
    }
}

/// The worker's switches, already resolved for its scope.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub apps: Vec<String>,
    pub any_app: bool,
    pub foreground: bool,
    /// When this worker last read outside content through amux (email).
    pub outside_seen_at: Option<i64>,
    /// AMUX_MAC_OUTSIDE_UNTIL: an approval to act anyway, until then.
    pub outside_until: i64,
    pub now: i64,
}

fn on(v: Option<String>) -> bool {
    matches!(
        v.as_deref().map(str::trim),
        Some("1" | "true" | "on" | "yes")
    )
}

impl Policy {
    /// Process env first, then the worker > group > global scope files.
    pub fn for_worker(home: &Path, worker: &str) -> Policy {
        let get = |k: &str| {
            crate::environment::resolve_in(home, crate::environment::Scope::Lane(worker), k, crate::environment::process(k).as_deref())
                .map(|r| r.value)
        };
        Policy {
            apps: get(APPS_KEY)
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            any_app: on(get(ANY_APP_KEY)),
            foreground: on(get(FOREGROUND_KEY)),
            outside_seen_at: outside_seen(home, worker),
            outside_until: get(OUTSIDE_UNTIL_KEY)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0),
            now: chrono::Utc::now().timestamp(),
        }
    }
}

// ── Outside content (Athena's prompt-injection gate) ──
//
// A worker that just read an email from someone else may be carrying that
// person's instructions. Until the owner says otherwise, such a worker may
// still READ Mac apps but not act in them: the first action files one
// Needs-you card, and Approve opens a 30-minute window.

/// One file per worker, so two workers noting at once never overwrite each
/// other (a shared file's read-modify-write lost one of them).
fn outside_file(home: &Path, worker: &str) -> PathBuf {
    home.join("computer").join("outside-content").join(format!(
        "{}.json",
        crate::integrations::browser::safe_file_component(worker)
    ))
}

/// Records that `worker` read outside content (`source` names what).
pub fn note_outside_content(home: &Path, worker: &str, source: &str) {
    let f = outside_file(home, worker);
    let body = json!({ "at": chrono::Utc::now().timestamp(), "source": source }).to_string();
    let res = std::fs::create_dir_all(f.parent().unwrap_or(home)).and_then(|_| {
        // Written beside and renamed over, so a reader never sees half a file.
        let tmp = f.with_extension(format!("json.{}", std::process::id()));
        std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &f))
    });
    match res {
        Ok(()) => tracing::info!(
            "[computer-mac] verdict=outside_content_noted worker={worker} source={source}"
        ),
        Err(e) => {
            tracing::warn!("[computer-mac] verdict=outside_content_unrecorded worker={worker}: {e}")
        }
    }
}

pub fn outside_seen(home: &Path, worker: &str) -> Option<i64> {
    let t = std::fs::read_to_string(outside_file(home, worker)).ok()?;
    let v: Value = serde_json::from_str(&t).ok()?;
    v.get("at")?.as_i64()
}

/// Tools that change something in an app, as opposed to reading it.
/// run_steps is judged by its steps (a run of reads only reads).
pub fn acts(tool: &str) -> bool {
    classify(tool) == Class::App
        && !matches!(
            tool,
            "get_window_state" | "parse_visual_regions" | "run_steps"
        )
}

/// A refusal the model can act on: what happened, that nothing was sent, what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub error: String,
    pub next: String,
}

impl Refusal {
    pub fn json(&self) -> Value {
        json!({ "error": self.error, "code": self.code, "next": self.next, "sent": false })
    }
}

pub fn blocked(app: &App) -> Option<&'static str> {
    BLOCKED
        .iter()
        .find(|(b, n, _)| {
            (!app.bundle_id.is_empty() && b.eq_ignore_ascii_case(&app.bundle_id))
                || (!app.name.is_empty() && n.eq_ignore_ascii_case(&app.name))
        })
        .map(|(_, _, why)| *why)
}

/// An app the system resolved from a pid passes when its bundle id or its
/// name is listed. An app the CALLER named (launch_app, `app`) has no pid and
/// passes only when every field it gave is listed: launch_app opens by bundle
/// id when it has one, so an allowed name beside another app's bundle id used
/// to let that other app in (amux-security, 2026-11-23).
pub fn allowed(policy: &Policy, app: &App) -> bool {
    let listed = |v: &str| policy.apps.iter().any(|a| a == "*" || a.eq_ignore_ascii_case(v));
    let fields = [app.bundle_id.as_str(), app.name.as_str()];
    let mut given = fields.iter().filter(|v| !v.is_empty());
    if app.pid.is_some() {
        given.any(|v| listed(v))
    } else {
        let given: Vec<_> = given.collect();
        !given.is_empty() && given.iter().all(|v| listed(v))
    }
}

/// pids, and apps named for launch, anywhere in a call's arguments,
/// including each step of run_steps.
pub fn targets(tool: &str, args: &Value) -> (Vec<i64>, Vec<App>) {
    let mut pids = Vec::new();
    let mut named = Vec::new();
    fn one(tool: &str, a: &Value, pids: &mut Vec<i64>, named: &mut Vec<App>) {
        for p in [a.get("pid"), a.pointer("/target/pid")]
            .into_iter()
            .flatten()
        {
            if let Some(n) = p.as_i64() {
                pids.push(n);
            }
        }
        if tool == "launch_app" || a.get("app").is_some() {
            let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let app = App {
                bundle_id: s("bundle_id"),
                name: if tool == "launch_app" {
                    s("name")
                } else {
                    s("app")
                },
                pid: None,
            };
            if !app.bundle_id.is_empty() || !app.name.is_empty() {
                named.push(app);
            }
        }
    }
    one(tool, args, &mut pids, &mut named);
    for step in args
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let t = step.get("tool").and_then(Value::as_str).unwrap_or("");
        let a = step.get("args").unwrap_or(&Value::Null);
        one(t, a, &mut pids, &mut named);
    }
    pids.sort_unstable();
    pids.dedup();
    (pids, named)
}

/// The step tools of a run_steps call.
pub fn step_tools(args: &Value) -> Vec<String> {
    args.get("steps")
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .map(|st| {
                    st.get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn uses_foreground(args: &Value) -> bool {
    let fg = |a: &Value| {
        a.get("delivery_mode").and_then(Value::as_str) == Some("foreground")
            || a.get("foreground_fallback").and_then(Value::as_bool) == Some(true)
    };
    fg(args)
        || args
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|s| s.get("args").is_some_and(fg))
}

/// The gate, given the apps the call resolved to. Pure, so it is tested
/// without a driver.
pub fn decide(tool: &str, args: &Value, apps: &[App], policy: &Policy) -> Result<(), Refusal> {
    let mut tools = vec![tool.to_string()];
    if tool == "run_steps" {
        let steps = step_tools(args);
        // One level only: targets() and this check read the outer steps, so a
        // run_steps inside a step would carry tools and pids past both.
        if steps.iter().any(|t| t == "run_steps") {
            return Err(Refusal {
                code: "tool_refused",
                error: "run_steps cannot contain run_steps. Nothing was sent.".into(),
                next: "put every step in one run_steps call".into(),
            });
        }
        tools.extend(steps);
    }
    for t in &tools {
        if let Class::Refused(why) = classify(t) {
            return Err(Refusal {
                code: "tool_refused",
                error: format!("{t} is not offered through amux: {why}. Nothing was sent."),
                next: "GET /api/computer/mac/tools lists what is; read a window with get_window_state, act with click, type_text, press_key, set_value or run_steps".into(),
            });
        }
    }
    if classify(tool) == Class::Free && tools.len() == 1 {
        return Ok(());
    }
    if apps.is_empty() {
        return Err(Refusal {
            code: "target_required",
            error: format!("{tool} must name its app, and this call does not. Nothing was sent."),
            next: "pass the app's pid (list_apps gives it; keep pid beside an element_token), or bundle_id/name for launch_app".into(),
        });
    }
    if uses_foreground(args) && !policy.foreground {
        return Err(Refusal {
            code: "foreground_not_allowed",
            error: "foreground delivery takes the owner's real pointer and keyboard, and is off for this worker. Nothing was sent.".into(),
            next: "use the default background delivery; if an app only answers foreground input, ask the owner to set AMUX_MAC_FOREGROUND=1 for this worker".into(),
        });
    }
    for app in apps {
        if !policy.any_app {
            if let Some(why) = blocked(app) {
                return Err(Refusal {
                    code: "app_blocked",
                    error: format!("{} is refused: {why}. Nothing was sent.", app.label()),
                    next: "do this another way (a worker's own shell, amux browser), or ask the owner; only AMUX_MAC_ANY_APP=1 lifts this".into(),
                });
            }
        }
        if !allowed(policy, app) {
            let id = if app.bundle_id.is_empty() {
                app.name.clone()
            } else {
                app.bundle_id.clone()
            };
            return Err(Refusal {
                code: "app_not_allowed",
                error: format!("this worker may not use {} on this Mac. Nothing was sent.", app.label()),
                next: format!(
                    "ask the owner to add {id} to {APPS_KEY} for this worker (Scope tab; comma-separated bundle ids or names, * for any app that is not blocked)"
                ),
            });
        }
    }
    let acting = acts(tool) || step_tools(args).iter().any(|t| acts(t));
    let fresh = policy
        .outside_seen_at
        .is_some_and(|at| policy.now - at < OUTSIDE_WINDOW_S);
    if acting && fresh && policy.now >= policy.outside_until {
        return Err(Refusal {
            code: "outside_content",
            error: "this worker read outside content (an email from someone else) in the last 30 minutes, so it may read apps but not act in them until the owner approves. Nothing was sent.".into(),
            next: "the owner has a Needs-you card for it; reads (get_window_state, list_windows) still work meanwhile".into(),
        });
    }
    Ok(())
}

/// Bundle id and name of a running process, from LaunchServices.
pub async fn app_for_pid(pid: i64) -> App {
    let field = |key: &'static str| async move {
        let out = tokio::process::Command::new("/usr/bin/lsappinfo")
            .args(["info", "-only", key, &pid.to_string()])
            .output()
            .await
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        // "CFBundleIdentifier"="com.apple.finder"
        let v = s.rsplit_once('=')?.1.trim().trim_matches('"').to_string();
        (!v.is_empty() && v != "[ NULL ]").then_some(v)
    };
    App {
        bundle_id: field("bundleid").await.unwrap_or_default(),
        name: field("name").await.unwrap_or_default(),
        pid: Some(pid),
    }
}

// ── The person at the Mac (Athena's "yield to the person") ──

/// Before acting in the app that is in front, wait while the person typed or
/// clicked in the last YIELD_IDLE_S, up to YIELD_WAIT_S, then give up.
pub const YIELD_IDLE_S: f64 = 1.5;
pub const YIELD_WAIT_S: f64 = 5.0;

/// A private test server (AMUX_MAC_TEST=1) reads the person from
/// <home>/computer/test-person.json ({"front": bundle, "idle_s": n}, or
/// "fronts": a list answered one per call).
fn test_person(home: &Path) -> Option<Value> {
    if std::env::var("AMUX_MAC_TEST").ok().as_deref() != Some("1") {
        return None;
    }
    let t = std::fs::read_to_string(home.join("computer").join("test-person.json")).ok()?;
    serde_json::from_str(&t).ok()
}

/// The bundle id of the app in front.
pub async fn frontmost(home: &Path) -> Option<String> {
    if let Some(mut t) = test_person(home) {
        // "fronts": [a, b, ...] answers one per call, the last one sticking,
        // so a test can have an action pull its app forward.
        if let Some(seq) = t.get_mut("fronts").and_then(Value::as_array_mut) {
            let first = seq.first().and_then(Value::as_str).map(str::to_string);
            if seq.len() > 1 {
                seq.remove(0);
                let f = home.join("computer").join("test-person.json");
                if let Err(e) = std::fs::write(&f, t.to_string()) {
                    tracing::warn!("[computer-mac] verdict=test_person_unsaved: {e}");
                }
            }
            return first;
        }
        return t.get("front").and_then(Value::as_str).map(str::to_string);
    }
    let asn = tokio::process::Command::new("/usr/bin/lsappinfo")
        .arg("front")
        .output()
        .await
        .ok()?;
    let asn = String::from_utf8_lossy(&asn.stdout).trim().to_string();
    let out = tokio::process::Command::new("/usr/bin/lsappinfo")
        .args(["info", "-only", "bundleid", &asn])
        .output()
        .await
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    let v = s.rsplit_once('=')?.1.trim().trim_matches('"').to_string();
    (!v.is_empty() && v != "[ NULL ]").then_some(v)
}

/// Seconds since the person last used the keyboard, mouse or trackpad.
pub async fn idle_s(home: &Path) -> Option<f64> {
    if let Some(t) = test_person(home) {
        return t.get("idle_s").and_then(Value::as_f64);
    }
    let out = tokio::process::Command::new("/usr/sbin/ioreg")
        .args(["-c", "IOHIDSystem", "-d", "4"])
        .output()
        .await
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().find(|l| l.contains("\"HIDIdleTime\""))?;
    let ns: f64 = line.rsplit_once('=')?.1.trim().parse().ok()?;
    Some(ns / 1e9)
}

/// Waits for the person to pause when they are working in `bundle`.
/// Ok(waited seconds), or the refusal to send.
pub async fn yield_to_person(home: &Path, bundle: &str) -> Result<f64, Refusal> {
    if bundle.is_empty() || frontmost(home).await.as_deref() != Some(bundle) {
        return Ok(0.0);
    }
    let t0 = std::time::Instant::now();
    loop {
        match idle_s(home).await {
            Some(i) if i < YIELD_IDLE_S => {}
            _ => return Ok(t0.elapsed().as_secs_f64()),
        }
        if t0.elapsed().as_secs_f64() >= YIELD_WAIT_S {
            return Err(Refusal {
                code: "person_active",
                error: format!("the owner is typing or clicking in {bundle} right now, so nothing was sent there."),
                next: "do something else and come back, or work in an app that is not in front".into(),
            });
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Brings `bundle` back to the front (an action that pulled its target
/// forward gives the person their app back).
pub async fn give_front_back(home: &Path, bundle: &str) -> bool {
    if test_person(home).is_some() {
        // A test records what it would have done.
        let f = home.join("computer").join("test-front-given-back");
        return std::fs::write(f, bundle).is_ok();
    }
    tokio::process::Command::new("/usr/bin/open")
        .args(["-g", "-b", bundle])
        .status()
        .await
        .is_ok_and(|s| s.success())
}

// ── Activity: what each worker did, for the owner to watch ──

/// One line saying what a step did, for the owner's Computer tab: the text
/// typed, the key pressed, the element or point clicked, the app launched.
/// Typed text is the owner's own record of what was typed in their apps.
pub fn step_detail(tool: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let clip = |t: String| if t.chars().count() > 120 { t.chars().take(119).collect::<String>() + "…" } else { t };
    let at = || match (args.get("x").and_then(Value::as_f64), args.get("y").and_then(Value::as_f64)) {
        (Some(x), Some(y)) => format!("at {x:.0},{y:.0}"),
        _ => String::new(),
    };
    let target = || {
        let e = s("element_token");
        if e.is_empty() { at() } else { format!("element {e}") }
    };
    match tool {
        "type_text" | "type_text_chars" => format!("typed “{}”", clip(s("text"))),
        "set_value" => format!("set {} to “{}”", target(), clip(s("value"))),
        "press_key" | "hotkey" => {
            let k = s("key");
            let mods = args.get("modifiers").and_then(Value::as_array).map(|m| m.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("+")).unwrap_or_default();
            format!("pressed {}{}", if mods.is_empty() { String::new() } else { mods + "+" }, if k.is_empty() { s("keys") } else { k })
        }
        "click" | "double_click" | "right_click" => format!("clicked {}", target()).trim().to_string(),
        "scroll" => format!("scrolled {} {}", s("direction"), target()).trim().to_string(),
        "launch_app" => format!("opened {}", if s("bundle_id").is_empty() { s("name") } else { s("bundle_id") }),
        "run_steps" => format!("{} steps", args.get("steps").and_then(Value::as_array).map(Vec::len).unwrap_or(0)),
        _ => String::new(),
    }
}

pub fn activity_file(home: &Path) -> PathBuf {
    home.join("computer").join("mac-activity.jsonl")
}

const ACTIVITY_KEEP: usize = 2000;

/// Appends one row; trims the file to the newest ACTIVITY_KEEP rows when it doubles.
pub fn record_activity(home: &Path, row: &Value) {
    use std::io::Write;
    let f = activity_file(home);
    if let Some(d) = f.parent() {
        if let Err(e) = std::fs::create_dir_all(d) {
            tracing::warn!("[computer-mac] verdict=activity_unrecorded: {e}");
            return;
        }
    }
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&f)
        .and_then(|mut h| writeln!(h, "{row}"));
    if let Err(e) = res {
        tracing::warn!("[computer-mac] verdict=activity_unrecorded: {e}");
        return;
    }
    if let Ok(t) = std::fs::read_to_string(&f) {
        let lines: Vec<&str> = t.lines().collect();
        if lines.len() > ACTIVITY_KEEP * 2 {
            let keep = lines[lines.len() - ACTIVITY_KEEP..].join("\n") + "\n";
            if let Err(e) = std::fs::write(&f, keep) {
                tracing::warn!("[computer-mac] verdict=activity_trim_failed: {e}");
            }
        }
    }
}

/// The newest rows first, for one worker or all, and how many rows were
/// looked at (the whole feed: the population the answer was drawn from).
pub fn activity(home: &Path, worker: Option<&str>, limit: usize) -> (Vec<Value>, usize) {
    let t = std::fs::read_to_string(activity_file(home)).unwrap_or_default();
    let all: Vec<Value> = t
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let n = all.len();
    let rows = all
        .into_iter()
        .rev()
        .filter(|v| worker.is_none_or(|w| v.get("worker").and_then(Value::as_str) == Some(w)))
        .take(limit)
        .collect();
    (rows, n)
}

// ── The broker: the only way a call reaches the driver ──

/// The app is connected while it polled within this long. Its long poll
/// waits [`POLL_WAIT`] and comes straight back, so a gap past this is an app
/// that stopped polling.
pub const POLL_FRESH: Duration = Duration::from_secs(40);
/// Longest a poll waits for work before answering "none".
pub const POLL_WAIT: Duration = Duration::from_secs(25);

/// One call for the app's driver: the MCP tool and its arguments, already
/// through the gate.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub tool: String,
    pub args: Value,
}

type Reply = tokio::sync::oneshot::Sender<Result<Value, String>>;

/// Calls waiting for the Mac app, and the replies they wait on. A reply goes
/// back to the waiting handler in this process, keyed by the job's random id;
/// nothing serves it over the API.
#[derive(Default)]
pub struct Broker {
    queue: std::sync::Mutex<std::collections::VecDeque<Job>>,
    waiting: std::sync::Mutex<std::collections::HashMap<String, Reply>>,
    work: tokio::sync::Notify,
    last_poll: std::sync::atomic::AtomicI64,
}

#[cfg(not(test))]
pub fn broker() -> &'static Broker {
    static B: std::sync::OnceLock<Broker> = std::sync::OnceLock::new();
    B.get_or_init(Broker::default)
}

/// Per test thread, so one test's poll cannot make another's app "connected".
#[cfg(test)]
pub fn broker() -> &'static Broker {
    thread_local! {
        static B: &'static Broker = Box::leak(Box::default());
    }
    B.with(|b| *b)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn job_id() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| format!("no random source for the call id: {e}"))?;
    Ok(hex::encode(b))
}

impl Broker {
    /// Did the app poll recently enough to count as connected?
    pub fn connected(&self) -> bool {
        let last = self.last_poll.load(std::sync::atomic::Ordering::Relaxed);
        last > 0 && now_ms() - last <= POLL_FRESH.as_millis() as i64
    }

    /// The app's poll: the next job, waiting up to `wait` for one.
    pub async fn next(&self, wait: Duration) -> Option<Job> {
        self.last_poll.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.work.notified();
            if let Some(j) = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop_front() {
                return Some(j);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                self.last_poll.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                return None;
            }
        }
    }

    /// The app's answer to job `id`. False when nobody waits on that id (it
    /// timed out, or the id was never issued).
    pub fn reply(&self, id: &str, r: Result<Value, String>) -> bool {
        let tx = self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(id);
        match tx {
            Some(tx) => tx.send(r).is_ok(),
            None => false,
        }
    }

    /// Queues one call and waits for the app's answer. Fails closed: with no
    /// app polling nothing is queued.
    pub async fn run(&self, tool: &str, args: &Value, timeout: Duration) -> Result<Value, String> {
        if !self.connected() {
            tracing::warn!(tool, measured = true, n_considered = 1, verdict = "computer_mac_app_not_polling",
                "[computer-mac] no Mac app is polling for calls; nothing was sent");
            return Err("the amux Mac app is not connected for computer use (it has not polled in the last \
                        40s); nothing was sent. Open the amux Mac app on this Mac with Computer use on"
                .to_string());
        }
        let id = job_id()?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.clone(), tx);
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(Job { id: id.clone(), tool: tool.to_string(), args: args.clone() });
        self.work.notify_one();
        let out = tokio::time::timeout(timeout, rx).await;
        // Whatever happened, this id is finished: a late reply finds no one.
        self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|j| j.id != id);
        match out {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the call was dropped before the Mac app answered".into()),
            Err(_) => {
                tracing::warn!(tool, measured = true, n_considered = 1, verdict = "computer_mac_call_timeout",
                    "[computer-mac] the Mac app did not answer within {}s", timeout.as_secs());
                Err(format!("the Mac app's driver did not answer {tool} within {}s", timeout.as_secs()))
            }
        }
    }
}

/// An MCP `tools/call` result as the app posts it, as the caller sees it:
/// the first image written to `shot`, an error result an error.
pub fn take_result(result: Value, shot: Option<&Path>) -> Result<Value, String> {
    use base64::Engine as _;
    if let (Some(p), Some(blocks)) = (shot, result.get("content").and_then(Value::as_array)) {
        if let Some(data) = blocks
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("image"))
            .and_then(|b| b.get("data").and_then(Value::as_str))
        {
            match base64::engine::general_purpose::STANDARD.decode(data) {
                Ok(bytes) => {
                    if let Err(e) = std::fs::write(p, bytes) {
                        tracing::warn!("[computer-mac] verdict=screenshot_write_failed path={}: {e}", p.display());
                    }
                }
                Err(e) => tracing::warn!("[computer-mac] verdict=screenshot_undecodable: {e}"),
            }
        }
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text: Vec<&str> = result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        return Err(if text.is_empty() { result.to_string() } else { text.join(" ") });
    }
    // The shape `cua-driver call` printed, which callers already read: the
    // structured content when the tool has one, else its text.
    if let Some(sc) = result.get("structuredContent").filter(|v| v.is_object()) {
        return Ok(sc.clone());
    }
    let text: Vec<&str> = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    let text = text.join("\n");
    Ok(serde_json::from_str::<Value>(text.trim())
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "text": text })))
}

/// Runs one tool on the app's driver through the [`Broker`]. `shot`
/// receives the first image, if any. There is no other path to the driver.
pub async fn call(tool: &str, args: &Value, shot: Option<&Path>) -> Result<Value, String> {
    let r = broker().run(tool, args, CALL_TIMEOUT).await?;
    take_result(r, shot)
}

#[cfg(test)]
mod broker_tests {
    use super::*;

    #[test]
    fn a_step_says_what_it_did() {
        assert_eq!(step_detail("type_text", &json!({"pid": 1, "text": "hello"})), "typed “hello”");
        assert_eq!(step_detail("press_key", &json!({"key": "return", "modifiers": ["cmd"]})), "pressed cmd+return");
        assert_eq!(step_detail("click", &json!({"element_token": "s1:9"})), "clicked element s1:9");
        assert_eq!(step_detail("click", &json!({"x": 10.4, "y": 20})), "clicked at 10,20");
        assert_eq!(step_detail("launch_app", &json!({"bundle_id": "com.tinyspeck.slackmacgap"})), "opened com.tinyspeck.slackmacgap");
        assert_eq!(step_detail("list_apps", &json!({})), "");
        let long = "x".repeat(300);
        let d = step_detail("type_text", &json!({"text": long}));
        assert!(d.ends_with("…”"), "{d}");
        assert_eq!(d.chars().count(), "typed “".chars().count() + 119 + 2, "119 characters, an ellipsis and the closing quote");
    }

    #[tokio::test]
    async fn with_no_app_polling_nothing_is_queued() {
        let b = Broker::default();
        let e = b.run("click", &json!({"pid": 1}), Duration::from_secs(1)).await.unwrap_err();
        assert!(e.contains("not connected"), "{e}");
        assert!(b.queue.lock().unwrap().is_empty() && b.waiting.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_call_goes_to_the_poller_and_its_answer_comes_back_in_process() {
        let b: &'static Broker = Box::leak(Box::default());
        assert!(b.next(Duration::ZERO).await.is_none(), "an empty poll answers none");
        assert!(b.connected(), "a poll connects the app");
        let app = tokio::spawn(async move {
            let job = b.next(Duration::from_secs(5)).await.expect("a job");
            assert_eq!((job.tool.as_str(), &job.args), ("click", &json!({"pid": 7})));
            assert_eq!(job.id.len(), 32, "a random id");
            assert!(!b.reply("not-the-id", Ok(json!({}))), "a wrong id reaches no one");
            assert!(b.reply(&job.id, Ok(json!({"content": [{"type": "text", "text": "done"}]}))));
            job.id
        });
        let r = b.run("click", &json!({"pid": 7}), Duration::from_secs(5)).await.unwrap();
        assert_eq!(r["content"][0]["text"], "done");
        let id = app.await.unwrap();
        assert!(!b.reply(&id, Ok(json!({}))), "an answered id is finished");
    }

    #[tokio::test]
    async fn a_call_the_app_never_answers_times_out_and_is_forgotten() {
        let b = Broker::default();
        b.next(Duration::ZERO).await;
        let e = b.run("click", &json!({}), Duration::from_millis(50)).await.unwrap_err();
        assert!(e.contains("did not answer"), "{e}");
        assert!(b.queue.lock().unwrap().is_empty() && b.waiting.lock().unwrap().is_empty());
    }

    #[test]
    fn a_result_writes_its_image_and_an_error_result_is_an_error() {
        use base64::Engine as _;
        let d = tempfile::tempdir().unwrap();
        let shot = d.path().join("s.png");
        let png = base64::engine::general_purpose::STANDARD.encode(b"PNGBYTES");
        let r = take_result(json!({"content": [{"type": "image", "data": png, "mimeType": "image/png"}, {"type": "text", "text": "ok"}],
                                   "structuredContent": {"windows": [{"window_id": 7}]}, "isError": false}), Some(&shot)).unwrap();
        assert_eq!(std::fs::read(&shot).unwrap(), b"PNGBYTES");
        assert_eq!(r, json!({"windows": [{"window_id": 7}]}), "the structured content, as `cua-driver call` printed it");
        let r = take_result(json!({"content": [{"type": "text", "text": "Found 1 window"}]}), None).unwrap();
        assert_eq!(r, json!({"text": "Found 1 window"}));
        let e = take_result(json!({"content": [{"type": "text", "text": "no such window"}], "isError": true}), None).unwrap_err();
        assert_eq!(e, "no such window");
    }

    #[test]
    fn an_endpoint_that_names_a_socket_or_leaves_one_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let cua = d.path().join("cua");
        std::fs::create_dir_all(&cua).unwrap();
        let me = std::process::id();
        let ep = |extra: &str| format!(r#"{{"binary":"/bin/sh","pid":{me},"app":"com.amux.desktop"{extra}}}"#);
        std::fs::write(endpoint_path(d.path()), ep("")).unwrap();
        assert!(endpoint(d.path()).is_ok());
        std::fs::write(endpoint_path(d.path()), ep(r#","socket":"/x/driver.sock""#)).unwrap();
        assert!(endpoint(d.path()).unwrap_err().contains("old computer-use driver"));
        std::fs::write(endpoint_path(d.path()), ep("")).unwrap();
        std::fs::write(cua.join("driver.sock"), "").unwrap();
        assert!(endpoint(d.path()).unwrap_err().contains("old computer-use driver"), "a stale driver.sock is refused");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(b: &str, n: &str) -> App {
        App {
            bundle_id: b.into(),
            name: n.into(),
            pid: Some(1),
        }
    }
    fn pol(apps: &[&str]) -> Policy {
        Policy {
            apps: apps.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn free_tools_need_no_app() {
        assert!(decide("list_apps", &json!({}), &[], &pol(&[])).is_ok());
        assert!(decide("check_permissions", &json!({}), &[], &pol(&[])).is_ok());
    }

    #[test]
    fn refused_tools_are_refused_with_a_next_step() {
        for t in [
            "kill_app",
            "run_script",
            "clipboard_read",
            "get_desktop_state",
            "browser_click",
            "set_config",
            "made_up",
        ] {
            let r = decide(t, &json!({}), &[], &pol(&["*"])).unwrap_err();
            assert_eq!(r.code, "tool_refused", "{t}");
            assert!(!r.next.is_empty());
        }
    }

    #[test]
    fn outside_content_stops_actions_until_approved_and_lets_reads_through() {
        let s = [app("com.spotify.client", "Spotify")];
        let mut p = pol(&["*"]);
        p.now = 10_000;
        p.outside_seen_at = Some(10_000 - 60);
        let click = json!({"pid": 1});
        assert_eq!(
            decide("click", &click, &s, &p).unwrap_err().code,
            "outside_content"
        );
        let steps = json!({"steps": [{"tool": "type_text", "args": {"pid": 1, "text": "x"}}]});
        assert_eq!(
            decide("run_steps", &steps, &s, &p).unwrap_err().code,
            "outside_content"
        );
        assert!(
            decide("get_window_state", &click, &s, &p).is_ok(),
            "reads still work"
        );
        p.outside_until = 10_000 + 600;
        assert!(
            decide("click", &click, &s, &p).is_ok(),
            "an approval opens it"
        );
        p.outside_until = 0;
        p.outside_seen_at = Some(10_000 - OUTSIDE_WINDOW_S - 1);
        assert!(
            decide("click", &click, &s, &p).is_ok(),
            "old outside content no longer counts"
        );
    }

    #[test]
    fn a_run_of_reads_is_not_an_action() {
        // Review F2: run_steps counted as acting whatever its steps were.
        let s = [app("com.spotify.client", "Spotify")];
        let mut p = pol(&["*"]);
        p.now = 10_000;
        p.outside_seen_at = Some(10_000 - 60);
        let reads =
            json!({"steps": [{"tool": "get_window_state", "args": {"pid": 1, "window_id": 2}}]});
        assert!(decide("run_steps", &reads, &s, &p).is_ok());
        assert!(!acts("run_steps"));
    }

    #[test]
    fn two_workers_noting_outside_content_both_land() {
        // Review F3: a shared file's read-modify-write dropped one of two.
        let d = tempfile::tempdir().unwrap();
        let hs: Vec<_> = (0..16)
            .map(|i| {
                let h = d.path().to_path_buf();
                std::thread::spawn(move || note_outside_content(&h, &format!("w{i}"), "email"))
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        for i in 0..16 {
            assert!(
                outside_seen(d.path(), &format!("w{i}")).is_some(),
                "w{i} lost"
            );
        }
    }

    #[test]
    fn nested_run_steps_is_refused() {
        // Review F1: an inner run_steps hid clipboard_read and blocked pids from the gate.
        let a = json!({"pid": 844, "steps": [{"tool": "run_steps", "args": {"steps": [{"tool": "clipboard_read", "args": {}}]}}]});
        let r = decide(
            "run_steps",
            &a,
            &[app("com.spotify.client", "Spotify")],
            &pol(&["*"]),
        )
        .unwrap_err();
        assert_eq!(r.code, "tool_refused");
        assert!(r.error.contains("cannot contain run_steps"));
    }

    #[test]
    fn a_refused_tool_inside_run_steps_refuses_the_whole_call() {
        let a = json!({"steps": [{"tool": "click", "args": {"pid": 5}}, {"tool": "clipboard_write", "args": {}}]});
        assert_eq!(
            decide(
                "run_steps",
                &a,
                &[app("com.spotify.client", "Spotify")],
                &pol(&["*"])
            )
            .unwrap_err()
            .code,
            "tool_refused"
        );
    }

    #[test]
    fn an_app_tool_must_name_its_app() {
        assert_eq!(
            decide("click", &json!({"x": 1, "y": 2}), &[], &pol(&["*"]))
                .unwrap_err()
                .code,
            "target_required"
        );
    }

    #[test]
    fn apps_must_be_listed_for_the_worker() {
        let s = app("com.spotify.client", "Spotify");
        let r = decide(
            "click",
            &json!({"pid": 1}),
            std::slice::from_ref(&s),
            &pol(&[]),
        )
        .unwrap_err();
        assert_eq!(r.code, "app_not_allowed");
        assert!(r.next.contains("com.spotify.client") && r.next.contains(APPS_KEY));
        assert!(decide(
            "click",
            &json!({"pid": 1}),
            std::slice::from_ref(&s),
            &pol(&["com.spotify.client"])
        )
        .is_ok());
        assert!(
            decide(
                "click",
                &json!({"pid": 1}),
                std::slice::from_ref(&s),
                &pol(&["spotify"])
            )
            .is_ok(),
            "names match too, any case"
        );
        assert!(decide("click", &json!({"pid": 1}), &[s], &pol(&["*"])).is_ok());
    }

    #[test]
    fn blocked_apps_stay_blocked_even_when_listed_or_starred() {
        for (b, n) in [
            ("com.apple.Terminal", "Terminal"),
            ("com.1password.1password", "1Password"),
            ("com.apple.systempreferences", "System Settings"),
            ("com.amux.desktop", "Amux"),
        ] {
            let r = decide("click", &json!({"pid": 1}), &[app(b, n)], &pol(&["*", b])).unwrap_err();
            assert_eq!(r.code, "app_blocked", "{n}");
        }
        let mut p = pol(&["*"]);
        p.any_app = true;
        assert!(decide(
            "click",
            &json!({"pid": 1}),
            &[app("com.apple.Terminal", "Terminal")],
            &p
        )
        .is_ok());
    }

    #[test]
    fn a_launch_needs_every_field_it_names_listed() {
        let launch = |b: &str, n: &str| App { bundle_id: b.into(), name: n.into(), pid: None };
        let p = pol(&["Notes"]);
        let r = decide("launch_app", &json!({}), &[launch("com.example.Other", "Notes")], &p).unwrap_err();
        assert_eq!(r.code, "app_not_allowed", "an allowed name beside another app's bundle id");
        assert!(decide("launch_app", &json!({}), &[launch("", "Notes")], &p).is_ok());
        let p = pol(&["Notes", "com.apple.Notes"]);
        assert!(decide("launch_app", &json!({}), &[launch("com.apple.Notes", "Notes")], &p).is_ok());
        // A pid the system resolved still passes on either listed field.
        assert!(decide("click", &json!({"pid": 1}), &[app("com.apple.Notes", "Notes")], &pol(&["Notes"])).is_ok());
    }

    #[test]
    fn apps_that_send_browse_open_or_kill_as_the_owner_are_blocked() {
        for (b, n) in [
            ("com.apple.MobileSMS", "Messages"),
            ("com.apple.mail", "Mail"),
            ("com.apple.Safari", "Safari"),
            ("com.google.Chrome", "Google Chrome"),
            ("com.apple.finder", "Finder"),
            ("com.apple.ActivityMonitor", "Activity Monitor"),
            ("com.microsoft.VSCode", "Code"),
            ("com.anthropic.claudefordesktop", "Claude"),
        ] {
            let r = decide("click", &json!({"pid": 1}), &[app(b, n)], &pol(&["*"])).unwrap_err();
            assert_eq!(r.code, "app_blocked", "{n}");
            let named = App { bundle_id: String::new(), name: n.into(), pid: None };
            assert_eq!(decide("launch_app", &json!({}), &[named], &pol(&["*"])).unwrap_err().code, "app_blocked", "{n} by name");
        }
    }

    #[test]
    fn launch_by_name_is_gated_like_a_pid() {
        let (pids, named) = targets("launch_app", &json!({"name": "Terminal"}));
        assert!(pids.is_empty());
        assert_eq!(
            decide(
                "launch_app",
                &json!({"name": "Terminal"}),
                &named,
                &pol(&["*"])
            )
            .unwrap_err()
            .code,
            "app_blocked"
        );
    }

    #[test]
    fn foreground_needs_its_switch() {
        let s = [app("com.spotify.client", "Spotify")];
        let a = json!({"pid": 1, "delivery_mode": "foreground"});
        assert_eq!(
            decide("click", &a, &s, &pol(&["*"])).unwrap_err().code,
            "foreground_not_allowed"
        );
        let steps = json!({"steps": [{"tool": "click", "args": {"pid": 1, "delivery_mode": "foreground"}}]});
        assert_eq!(
            decide("run_steps", &steps, &s, &pol(&["*"]))
                .unwrap_err()
                .code,
            "foreground_not_allowed"
        );
        let mut p = pol(&["*"]);
        p.foreground = true;
        assert!(decide("click", &a, &s, &p).is_ok());
    }

    #[test]
    fn targets_reads_pids_from_args_targets_and_steps() {
        let a = json!({"pid": 3, "target": {"pid": 4}, "steps": [{"tool": "click", "args": {"pid": 9}}, {"tool": "type_text", "args": {"pid": 3}}]});
        assert_eq!(targets("run_steps", &a).0, vec![3, 4, 9]);
    }
}
