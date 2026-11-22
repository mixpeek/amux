//! Computer use on this Mac's own apps, through trycua's `cua-driver`
//! (owner, 2026-11-22: build Athena's computer use into amux; teardown in
//! `research/athena-mac-app-teardown.md`).
//!
//! The Mac app (desktop/amux-computer.swift) runs the driver embedded, so it
//! uses Amux's own Accessibility and Screen Recording grants, and writes
//! `<AMUX_HOME>/cua/endpoint.json` naming the binary and its socket. The
//! server forwards a worker's tool call to it with `cua-driver call`, after a
//! gate this file owns:
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
pub const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// What the Mac app publishes when the driver is up.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub binary: PathBuf,
    pub socket: PathBuf,
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
        socket: PathBuf::from(s("socket")),
        pid,
        app: s("app"),
    };
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
            std::env::var(k)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| crate::api::session_verbs::scoped_setting_in(home, worker, k))
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
        }
    }
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

pub fn allowed(policy: &Policy, app: &App) -> bool {
    policy.apps.iter().any(|a| {
        a == "*"
            || (!app.bundle_id.is_empty() && a.eq_ignore_ascii_case(&app.bundle_id))
            || (!app.name.is_empty() && a.eq_ignore_ascii_case(&app.name))
    })
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

/// Runs one tool on the driver. `shot` receives the first image, if any.
pub async fn call(
    ep: &Endpoint,
    tool: &str,
    args: &Value,
    shot: Option<&Path>,
) -> Result<Value, String> {
    let mut cmd = tokio::process::Command::new(&ep.binary);
    cmd.arg("call").arg("--socket").arg(&ep.socket);
    if let Some(p) = shot {
        cmd.arg("--screenshot-out-file").arg(p);
    }
    cmd.arg(tool)
        .arg(args.to_string())
        .env("CUA_DRIVER_TELEMETRY", "0")
        .kill_on_drop(true);
    let out = tokio::time::timeout(CALL_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "the driver did not answer {tool} within {}s",
                CALL_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("starting {}: {e}", ep.binary.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let body = stdout.trim();
    let parsed: Option<Value> = body
        .find('{')
        .and_then(|i| serde_json::from_str(&body[i..]).ok());
    match (out.status.success(), parsed) {
        (true, Some(v)) => Ok(v),
        (true, None) => Ok(json!({ "text": body })),
        (false, Some(v)) => Err(v
            .get("error")
            .map(|e| e.to_string())
            .unwrap_or_else(|| v.to_string())),
        (false, None) => Err(format!(
            "{} {}",
            body.chars().take(400).collect::<String>(),
            stderr
                .lines()
                .filter(|l| !l.contains("telemetry"))
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(400)
                .collect::<String>()
        )
        .trim()
        .to_string()),
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
