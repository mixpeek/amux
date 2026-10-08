//! Computer-use sandboxes (AMUX-5300): rung 3 of the access ladder.
//!
//! Ethan, 2026-09-27: "if u cant access something via amux browser, then use
//! cdp, if cdp doesnt work use https://github.com/trycua/cua". This module is
//! that third rung: a disposable Linux desktop in a Docker container, driven
//! by pixels and keystrokes, one per worker lane.
//!
//! ARCHITECTURE: amux speaks CUA's `computer-server` HTTP API directly. The
//! container runs `computer-server` (CUA's own Python process, INSIDE the
//! sandbox); its `POST /cmd {"command","params"}` endpoint answers with one
//! `data: {...}` line. That API is plain HTTP + JSON, so the Python
//! `cua-computer` SDK on the host would add a Python runtime and a subprocess
//! per action for nothing. The SDK is itself a client of the same endpoint
//! (it prefers the `/ws` websocket and falls back to `/cmd`); `/cmd` is the
//! stateless form and needs no connection lifecycle.
//!
//! STATE LIVES IN DOCKER, NOT IN THIS PROCESS. The auto-builder re-execs this
//! server on every commit, so an in-memory sandbox registry would forget every
//! container within the hour and leak them. Each container carries labels
//! (`amux-computer=<lane>`, started time, image tag) and `docker ps` is the
//! registry. The only thing stored beside it is the last-action stamp, as a
//! file per container under `~/.amux/computer/activity/`, so the idle clock
//! also survives a restart.
//!
//! RESOURCE DISCIPLINE is the point of most of this file: this box has been
//! memory- and CPU-pressured, and a forgotten VM once sat at 22 GB for five
//! days. So: a fleet-wide cap (AMUX_COMPUTER_MAX, default 2), hard memory and
//! CPU limits on every container, an idle stop after 15 minutes without an
//! action (`runtime_jobs::computer_reaper`), a sweep that removes exited,
//! duplicate and unowned containers by label, and a verdict log line for
//! every start, stop and refusal.

use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Docker label naming the owning lane. The sweep finds sandboxes by THIS,
/// never by name or image, so a container amux did not start is never touched.
pub const LABEL: &str = "amux-computer";
pub const LABEL_STARTED: &str = "amux-computer.started";
pub const LABEL_IMAGE: &str = "amux-computer.image";

/// Ports inside the container: computer-server, noVNC, and the socat relay in
/// front of Chromium's DevTools port (Chromium binds DevTools to the
/// container's loopback only, so it cannot be published directly).
pub const API_PORT: u16 = 8000;
pub const VNC_PORT: u16 = 6901;
pub const CDP_RELAY_PORT: u16 = 9223;
pub const CDP_PORT: u16 = 9222;

/// The sandbox image recipe. Its content hash is the image tag, so editing
/// the Dockerfile builds a new image on the next start and never reuses a
/// stale one under the old name.
pub const DOCKERFILE: &str = include_str!("computer/Dockerfile");

pub const DEFAULT_BASE_IMAGE: &str = "trycua/cua-ubuntu:latest";

/// The Dockerfile with its FROM line pointed at `base`.
pub fn recipe(base: &str) -> String {
    DOCKERFILE.replacen(
        &format!("FROM {DEFAULT_BASE_IMAGE}"),
        &format!("FROM {base}"),
        1,
    )
}

/// Local image tag: a hash of the exact recipe (base included), so editing
/// the Dockerfile or AMUX_COMPUTER_BASE_IMAGE builds a new image rather than
/// reusing a stale one under the old name.
pub fn image_tag(base: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(recipe(base).as_bytes());
    format!("amux-computer:{}", &hex::encode(h)[..12])
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn is_on(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

fn is_off(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// A scoped setting with the ladder every other scoped gate uses: process env
/// (`~/.amux/server.env`, the operator switch) wins, then worker > group >
/// global scope files.
pub fn scoped(home: &Path, lane: &str, key: &str) -> Option<String> {
    if let Some(v) = env_nonempty(key) {
        return Some(v);
    }
    if lane.is_empty() {
        return None;
    }
    crate::api::session_verbs::scoped_setting_in(home, lane, key)
}

/// Fleet-wide knobs. Read on every call, so an edit to server.env or a scope
/// file applies without a restart.
#[derive(Debug, Clone, Serialize)]
pub struct Limits {
    /// Max concurrently RUNNING sandboxes across the whole fleet.
    pub max: usize,
    /// `docker run --memory` (and `--memory-swap`, so it cannot spill).
    pub memory: String,
    /// `docker run --cpus`.
    pub cpus: String,
    /// Idle seconds before the reaper stops a sandbox. 0 disables that arm.
    pub idle_s: u64,
    /// Base image the local sandbox image is built FROM.
    pub base_image: String,
}

pub fn limits() -> Limits {
    Limits {
        max: env_nonempty("AMUX_COMPUTER_MAX")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        memory: env_nonempty("AMUX_COMPUTER_MEMORY").unwrap_or_else(|| "4g".into()),
        cpus: env_nonempty("AMUX_COMPUTER_CPUS").unwrap_or_else(|| "2".into()),
        idle_s: env_nonempty("AMUX_COMPUTER_IDLE_S")
            .and_then(|v| v.parse().ok())
            .unwrap_or(900),
        base_image: env_nonempty("AMUX_COMPUTER_BASE_IMAGE")
            .unwrap_or_else(|| DEFAULT_BASE_IMAGE.into()),
    }
}

/// The kill switch: AMUX_COMPUTER=0 at any scope refuses `start` for that
/// lane (or everyone, from server.env). Existing sandboxes are left to the
/// idle reaper, so flipping it never destroys a page mid-task.
pub fn enabled_for(home: &Path, lane: &str) -> bool {
    !scoped(home, lane, "AMUX_COMPUTER").is_some_and(|v| is_off(&v))
}

/// May `start` run `colima start` when Docker is down? Default OFF: booting a
/// VM that holds tens of GB is a decision, not a side effect.
pub fn colima_autostart(home: &Path, lane: &str) -> bool {
    scoped(home, lane, "AMUX_COMPUTER_COLIMA_AUTOSTART").is_some_and(|v| is_on(&v))
}

// ---------------------------------------------------------------------------
// Names and paths
// ---------------------------------------------------------------------------

/// Docker-safe container name for a lane. The short hash keeps two lanes
/// whose names sanitize alike (`a.b` / `a_b`) from colliding.
pub fn container_name(lane: &str) -> String {
    use sha2::Digest;
    let clean: String = lane
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let h = hex::encode(sha2::Sha256::digest(lane.as_bytes()));
    format!("amux-computer-{clean}-{}", &h[..6])
}

pub fn state_dir() -> PathBuf {
    crate::integrations::browser::amux_home().join("computer")
}

fn activity_file(name: &str) -> PathBuf {
    state_dir().join("activity").join(name)
}

pub fn screenshot_dir() -> PathBuf {
    crate::integrations::browser::amux_home().join("computer-screenshots")
}

pub fn now() -> i64 {
    crate::integrations::browser::now_secs_i64()
}

/// Stamp the idle clock. Called on every action verb.
pub fn touch(name: &str) {
    let p = activity_file(name);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Err(e) = std::fs::write(&p, now().to_string()) {
        tracing::warn!("[computer] could not stamp activity for {name}: {e}");
    }
}

pub fn last_action(name: &str) -> Option<i64> {
    std::fs::read_to_string(activity_file(name))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn forget(name: &str) {
    let _ = std::fs::remove_file(activity_file(name));
}

// ---------------------------------------------------------------------------
// Docker
// ---------------------------------------------------------------------------

/// The docker binary. launchd starts this server with a minimal PATH, so a
/// bare `docker` can be missing here while it works in every terminal.
pub fn docker_bin() -> Option<PathBuf> {
    which(
        "docker",
        &["/usr/local/bin/docker", "/opt/homebrew/bin/docker"],
    )
}

pub fn colima_bin() -> Option<PathBuf> {
    which(
        "colima",
        &["/usr/local/bin/colima", "/opt/homebrew/bin/colima"],
    )
}

fn which(name: &str, fallbacks: &[&str]) -> Option<PathBuf> {
    if let Ok(path) = std::env::var("PATH") {
        for d in path.split(':') {
            let p = Path::new(d).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    fallbacks.iter().map(PathBuf::from).find(|p| p.is_file())
}

pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn docker(args: &[&str], timeout: Duration) -> anyhow::Result<Out> {
    let bin = docker_bin().ok_or_else(|| anyhow::anyhow!("docker CLI not found on PATH"))?;
    run(&bin, args, None, timeout).await
}

async fn run(
    bin: &Path,
    args: &[&str],
    stdin: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<Out> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true);
    // Homebrew/colima helpers live beside docker; give the child a PATH that
    // finds them even when this server was started by launchd.
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env(
        "PATH",
        format!("{path}:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin"),
    );
    let mut child = cmd.spawn()?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).await?;
        drop(pipe);
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "`{} {}` timed out after {}s",
                bin.display(),
                args.join(" "),
                timeout.as_secs()
            )
        })??;
    Ok(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Is a Docker daemon answering? `measured` is false when the probe itself
/// could not run (no CLI), which is a different fault from a stopped daemon.
#[derive(Debug, Clone, Serialize)]
pub struct DockerProbe {
    pub measured: bool,
    pub up: bool,
    pub context: Option<String>,
    pub colima_profile: Option<String>,
    pub error: Option<String>,
}

pub async fn probe_docker() -> DockerProbe {
    if docker_bin().is_none() {
        return DockerProbe {
            measured: false,
            up: false,
            context: None,
            colima_profile: None,
            error: Some("docker CLI not found (install docker + colima)".into()),
        };
    }
    let context = docker(&["context", "show"], Duration::from_secs(5))
        .await
        .ok()
        .filter(|o| o.ok)
        .map(|o| o.stdout.trim().to_string());
    let colima_profile = colima_profile_for(context.as_deref());
    match docker(
        &["info", "--format", "{{.ServerVersion}}"],
        Duration::from_secs(10),
    )
    .await
    {
        Ok(o) if o.ok && !o.stdout.trim().is_empty() => DockerProbe {
            measured: true,
            up: true,
            context,
            colima_profile,
            error: None,
        },
        Ok(o) => DockerProbe {
            measured: true,
            up: false,
            context,
            colima_profile,
            error: Some(first_line(&o.stderr, "docker daemon did not answer")),
        },
        Err(e) => DockerProbe {
            measured: true,
            up: false,
            context,
            colima_profile,
            error: Some(e.to_string()),
        },
    }
}

/// Which colima profile backs the current docker context. `colima status`
/// with no profile checks `default`, which on this box is NOT the running one
/// (the live VM is `gs7-e`), so the answer has to come from the context name.
pub fn colima_profile_for(context: Option<&str>) -> Option<String> {
    if let Some(p) = env_nonempty("AMUX_COMPUTER_COLIMA_PROFILE") {
        return Some(p);
    }
    match context {
        Some("colima") => Some("default".into()),
        Some(c) => c.strip_prefix("colima-").map(str::to_string),
        None => None,
    }
}

fn first_line(s: &str, fallback: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

/// One amux-labelled container, as `docker ps` reports it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sandbox {
    pub id: String,
    pub name: String,
    pub lane: String,
    pub state: String,
    pub image: String,
    pub started_at: Option<i64>,
    pub api_port: Option<u16>,
    pub vnc_port: Option<u16>,
    pub cdp_port: Option<u16>,
}

impl Sandbox {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
}

/// Parse one `docker ps --format '{{json .}}'` line.
pub fn parse_ps_line(line: &str) -> Option<Sandbox> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let labels = parse_labels(&s("Labels"));
    let ports = s("Ports");
    Some(Sandbox {
        id: s("ID"),
        name: s("Names"),
        lane: labels
            .iter()
            .find(|(k, _)| k == LABEL)
            .map(|(_, v)| v.clone())
            .unwrap_or_default(),
        state: s("State"),
        image: s("Image"),
        started_at: labels
            .iter()
            .find(|(k, _)| k == LABEL_STARTED)
            .and_then(|(_, v)| v.parse().ok()),
        api_port: host_port(&ports, API_PORT),
        vnc_port: host_port(&ports, VNC_PORT),
        cdp_port: host_port(&ports, CDP_RELAY_PORT),
    })
}

/// `docker ps` renders labels as `k=v,k2=v2`.
pub fn parse_labels(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// The host port published for `inner/tcp`, from a docker Ports column such
/// as `127.0.0.1:55001->8000/tcp, 127.0.0.1:55002->6901/tcp`.
pub fn host_port(ports: &str, inner: u16) -> Option<u16> {
    let want = format!("->{inner}/tcp");
    ports.split(',').map(str::trim).find_map(|p| {
        let (host, _) = p.split_once(&want)?;
        host.rsplit(':').next()?.parse().ok()
    })
}

pub async fn list() -> anyhow::Result<Vec<Sandbox>> {
    let filter = format!("label={LABEL}");
    let o = docker(
        &["ps", "-a", "--filter", &filter, "--format", "{{json .}}"],
        Duration::from_secs(15),
    )
    .await?;
    if !o.ok {
        anyhow::bail!("docker ps failed: {}", first_line(&o.stderr, "no stderr"));
    }
    Ok(o.stdout.lines().filter_map(parse_ps_line).collect())
}

/// The `docker run` argv for a lane's sandbox. Pure, so the limits, labels
/// and loopback-only publishing are pinned by a unit test.
pub fn run_args(lim: &Limits, lane: &str, image: &str, started_at: i64) -> Vec<String> {
    let name = container_name(lane);
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name,
        "--label".into(),
        format!("{LABEL}={lane}"),
        "--label".into(),
        format!("{LABEL_STARTED}={started_at}"),
        "--label".into(),
        format!("{LABEL_IMAGE}={image}"),
        "--memory".into(),
        lim.memory.clone(),
        // Equal to --memory: no swap headroom, so the cap is the cap.
        "--memory-swap".into(),
        lim.memory.clone(),
        "--cpus".into(),
        lim.cpus.clone(),
        "--pids-limit".into(),
        "4096".into(),
        // Chromium and Kasm both want more than Docker's 64 MB /dev/shm.
        "--shm-size".into(),
        "512m".into(),
    ];
    // LOOPBACK ONLY. An unauthenticated remote-control API for a desktop must
    // never listen on the LAN; `127.0.0.1::N` also lets Docker pick a free
    // host port so two sandboxes never collide.
    for p in [API_PORT, VNC_PORT, CDP_RELAY_PORT] {
        a.push("-p".into());
        a.push(format!("127.0.0.1::{p}"));
    }
    // Kasm refuses a VNC password under 6 characters and EXITS. The port is
    // loopback-only and basic auth is disabled, so the value is not a secret.
    for e in ["VNC_PW=amux-computer", "VNCOPTIONS=-disableBasicAuth"] {
        a.push("-e".into());
        a.push(e.into());
    }
    a.push(image.into());
    a
}

// ---------------------------------------------------------------------------
// Lifecycle policy (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Seconds since the sandbox last did anything: the later of its last action
/// and its start. None when neither is known.
pub fn idle_for(now: i64, started_at: Option<i64>, last_action: Option<i64>) -> Option<i64> {
    let since = match (started_at, last_action) {
        (Some(s), Some(a)) => s.max(a),
        (Some(s), None) => s,
        (None, Some(a)) => a,
        (None, None) => return None,
    };
    Some((now - since).max(0))
}

/// Why the sweep removes a container. `None` keeps it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SweepAction {
    pub name: String,
    pub lane: String,
    pub reason: String,
}

/// Decide what the sweep removes. Rules, in order:
/// 1. no owning lane on the label: nobody can drive it, nobody will stop it;
/// 2. not running (exited/created/dead): a failed or stopped sandbox is only
///    disk and a name conflict for the next start;
/// 3. a second running sandbox for the same lane: keep the newest, since the
///    API resolves a lane to one container;
/// 4. idle past `idle_s` (0 disables this arm).
pub fn plan_sweep(
    boxes: &[Sandbox],
    now: i64,
    idle_s: u64,
    last_action: &dyn Fn(&str) -> Option<i64>,
) -> Vec<SweepAction> {
    let mut out = Vec::new();
    let mut newest: std::collections::HashMap<&str, (&str, i64)> = Default::default();
    for b in boxes.iter().filter(|b| b.running() && !b.lane.is_empty()) {
        let t = b.started_at.unwrap_or(0);
        let e = newest
            .entry(b.lane.as_str())
            .or_insert((b.name.as_str(), t));
        if t > e.1 {
            *e = (b.name.as_str(), t);
        }
    }
    for b in boxes {
        let reason = if b.lane.is_empty() {
            Some("no owning lane on the amux-computer label".to_string())
        } else if !b.running() {
            Some(format!(
                "container is {}",
                if b.state.is_empty() {
                    "not running"
                } else {
                    &b.state
                }
            ))
        } else if newest
            .get(b.lane.as_str())
            .is_some_and(|(n, _)| *n != b.name)
        {
            Some("duplicate sandbox for the lane (a newer one is kept)".to_string())
        } else if idle_s > 0 {
            idle_for(now, b.started_at, last_action(&b.name))
                .filter(|i| *i >= idle_s as i64)
                .map(|i| format!("idle {i}s >= {idle_s}s (AMUX_COMPUTER_IDLE_S)"))
        } else {
            None
        };
        if let Some(reason) = reason {
            out.push(SweepAction {
                name: b.name.clone(),
                lane: b.lane.clone(),
                reason,
            });
        }
    }
    out
}

/// The cap check. Returns the lanes holding the slots when full. The caller's
/// own lane never counts against it (it reuses its sandbox).
pub fn cap_refusal(boxes: &[Sandbox], lane: &str, max: usize) -> Option<Vec<String>> {
    let others: Vec<String> = boxes
        .iter()
        .filter(|b| b.running() && b.lane != lane)
        .map(|b| b.lane.clone())
        .collect();
    (others.len() >= max).then_some(others)
}

// ---------------------------------------------------------------------------
// computer-server protocol
// ---------------------------------------------------------------------------

/// Map an amux verb + JSON body to computer-server's `{command, params}`.
/// Pure, so every verb's wire shape is pinned by a test instead of by a live
/// container.
pub fn map_action(verb: &str, body: &Value) -> Result<(String, Value), String> {
    let num = |k: &str| {
        body.get(k)
            .and_then(Value::as_f64)
            .map(|f| f.round() as i64)
    };
    let xy = || -> Result<(i64, i64), String> {
        match (num("x"), num("y")) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(format!("{verb} needs numeric x and y")),
        }
    };
    match verb {
        "click" | "double_click" | "right_click" | "move" => {
            let (x, y) = xy()?;
            let cmd = match verb {
                "click" => "left_click",
                "move" => "move_cursor",
                other => other,
            };
            Ok((cmd.into(), json!({ "x": x, "y": y })))
        }
        "type" => {
            let text = body
                .get("text")
                .and_then(Value::as_str)
                .ok_or("type needs a text string")?;
            Ok(("type_text".into(), json!({ "text": text })))
        }
        "key" => {
            let key = body
                .get("key")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .ok_or("key needs a key name, e.g. enter or ctrl+l")?;
            // A chord is a hotkey in computer-server; a single key is press_key.
            if key.contains('+') && key.len() > 1 {
                let keys: Vec<String> = key.split('+').map(|k| k.trim().to_lowercase()).collect();
                if keys.iter().any(String::is_empty) {
                    return Err(format!("malformed key chord {key:?}"));
                }
                Ok(("hotkey".into(), json!({ "keys": keys })))
            } else {
                Ok(("press_key".into(), json!({ "key": key.to_lowercase() })))
            }
        }
        "scroll" => {
            // dy > 0 scrolls DOWN, the way a page reads. computer-server's
            // `scroll` is pynput's wheel, where positive y is UP, so flip it.
            let dx = num("dx").unwrap_or(0);
            let dy = num("dy").unwrap_or(0);
            if dx == 0 && dy == 0 {
                return Err("scroll needs a non-zero dx or dy".into());
            }
            Ok(("scroll".into(), json!({ "x": dx, "y": -dy })))
        }
        "screenshot" => Ok(("screenshot".into(), json!({ "format": "png" }))),
        other => Err(format!("unknown verb {other:?}")),
    }
}

/// computer-server's `/cmd` streams one `data: {json}` line per result.
pub fn parse_cmd_response(body: &str) -> Result<Value, String> {
    let line = body
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("data:"))
        .next_back()
        .ok_or_else(|| format!("no `data:` line in computer-server reply: {}", head(body)))?;
    let v: Value = serde_json::from_str(line.trim())
        .map_err(|e| format!("computer-server reply is not JSON ({e}): {}", head(line)))?;
    if v.get("success").and_then(Value::as_bool) == Some(false) {
        return Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("computer-server reported failure with no error text")
            .to_string());
    }
    Ok(v)
}

fn head(s: &str) -> String {
    let h: String = s.chars().take(200).collect();
    if h.trim().is_empty() {
        "<empty>".into()
    } else {
        h
    }
}

pub async fn cmd(api_port: u16, command: &str, params: Value) -> anyhow::Result<Value> {
    let r = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{api_port}/cmd"))
        .json(&json!({ "command": command, "params": params }))
        .timeout(Duration::from_secs(60))
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("computer-server answered HTTP {status}: {}", head(&body));
    }
    parse_cmd_response(&body).map_err(anyhow::Error::msg)
}

async fn server_ready(api_port: u16) -> bool {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{api_port}/status"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

// ---------------------------------------------------------------------------
// Start / stop
// ---------------------------------------------------------------------------

/// One `start` at a time, fleet-wide, so two lanes cannot both pass the cap
/// check and both launch.
static START_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Serialize)]
pub struct Started {
    pub sandbox: Sandbox,
    pub reused: bool,
    pub image: String,
    pub image_built: bool,
    pub colima_started: bool,
    pub ready_after_s: f64,
}

/// A start that did not happen, with the HTTP status and what unblocks it.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub reason: &'static str,
    pub body: Value,
}

fn refuse(status: u16, reason: &'static str, body: Value) -> Refusal {
    Refusal {
        status,
        reason,
        body,
    }
}

/// The lane's running sandbox, if any.
pub async fn find(lane: &str) -> anyhow::Result<Option<Sandbox>> {
    let name = container_name(lane);
    Ok(list()
        .await?
        .into_iter()
        .find(|b| b.name == name && b.running()))
}

pub async fn start(home: &Path, lane: &str) -> Result<Started, Refusal> {
    if !enabled_for(home, lane) {
        return Err(refuse(
            403,
            "disabled",
            json!({ "error": "computer use is disabled for this lane (AMUX_COMPUTER=0)",
                    "hint": "unset AMUX_COMPUTER at the worker, group or global scope" }),
        ));
    }
    let _guard = START_LOCK.lock().await;
    let t0 = std::time::Instant::now();

    let mut probe = probe_docker().await;
    let mut colima_started = false;
    if !probe.up {
        if !probe.measured || !colima_autostart(home, lane) {
            return Err(refuse(
                503,
                "docker_down",
                json!({
                    "error": format!("Docker is not answering: {}", probe.error.clone().unwrap_or_default()),
                    "docker": probe,
                    "hint": "start it with `colima start --profile <p>`, or set \
                             AMUX_COMPUTER_COLIMA_AUTOSTART=1 at the worker/group/global scope \
                             to let `amux computer start` do it",
                }),
            ));
        }
        let Some(colima) = colima_bin() else {
            return Err(refuse(
                503,
                "docker_down",
                json!({ "error": "Docker is down and the colima CLI was not found", "docker": probe }),
            ));
        };
        let profile = probe
            .colima_profile
            .clone()
            .unwrap_or_else(|| "default".into());
        tracing::info!("[computer] verdict=colima_start lane={lane} profile={profile} (AMUX_COMPUTER_COLIMA_AUTOSTART)");
        let out = run(
            &colima,
            &["start", "--profile", &profile],
            None,
            Duration::from_secs(600),
        )
        .await;
        match out {
            Ok(o) if o.ok => colima_started = true,
            Ok(o) => {
                return Err(refuse(
                    503,
                    "colima_failed",
                    json!({ "error": format!("colima start --profile {profile} failed: {}", first_line(&o.stderr, "no stderr")) }),
                ))
            }
            Err(e) => {
                return Err(refuse(
                    503,
                    "colima_failed",
                    json!({ "error": format!("colima start --profile {profile}: {e}") }),
                ))
            }
        }
        probe = probe_docker().await;
        if !probe.up {
            return Err(refuse(
                503,
                "docker_down",
                json!({ "error": "colima started but Docker still does not answer", "docker": probe }),
            ));
        }
    }

    let boxes = list().await.map_err(|e| {
        refuse(
            503,
            "docker_error",
            json!({ "error": format!("listing sandboxes: {e}") }),
        )
    })?;
    let name = container_name(lane);
    if let Some(b) = boxes.iter().find(|b| b.name == name && b.running()) {
        touch(&name);
        return Ok(Started {
            sandbox: b.clone(),
            reused: true,
            image: b.image.clone(),
            image_built: false,
            colima_started,
            ready_after_s: 0.0,
        });
    }
    let lim = limits();
    if let Some(holders) = cap_refusal(&boxes, lane, lim.max) {
        return Err(refuse(
            429,
            "cap",
            json!({
                "error": format!("{} of {} sandboxes are running (AMUX_COMPUTER_MAX)", holders.len(), lim.max),
                "held_by": holders,
                "hint": "ask a holder to `amux computer stop`, wait for the idle reaper, or raise AMUX_COMPUTER_MAX in ~/.amux/server.env",
            }),
        ));
    }
    // A stopped container with our name blocks `docker run --name`.
    if boxes.iter().any(|b| b.name == name) {
        let _ = docker(&["rm", "-f", &name], Duration::from_secs(60)).await;
    }

    let (image, image_built) = ensure_image(&lim).await.map_err(|e| {
        refuse(
            502,
            "image_failed",
            json!({ "error": format!("sandbox image: {e}") }),
        )
    })?;
    let args = run_args(&lim, lane, &image, now());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = docker(&argv, Duration::from_secs(120)).await.map_err(|e| {
        refuse(
            502,
            "run_failed",
            json!({ "error": format!("docker run: {e}") }),
        )
    })?;
    if !out.ok {
        return Err(refuse(
            502,
            "run_failed",
            json!({ "error": format!("docker run failed: {}", first_line(&out.stderr, "no stderr")) }),
        ));
    }
    touch(&name);
    let sandbox = match find(lane).await {
        Ok(Some(b)) => b,
        _ => {
            return Err(refuse(
                502,
                "run_failed",
                json!({ "error": "container started but is not running; see `docker logs` for it", "name": name }),
            ))
        }
    };
    let Some(api) = sandbox.api_port else {
        return Err(refuse(
            502,
            "run_failed",
            json!({ "error": "no host port published for computer-server" }),
        ));
    };
    // computer-server comes up after the Kasm desktop; ~15-40s on this box.
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    let mut polls = 0u32;
    while !server_ready(api).await {
        polls += 1;
        // A container that EXITED will never answer, and the reaper removes
        // exited ones within a minute, so waiting out the deadline would turn
        // a crash into a 180s timeout with no cause (measured on the first
        // e2e: Kasm rejected a 4-char VNC_PW and exited in 5s). Check every
        // ~10s and fail with the container's own last words.
        if polls.is_multiple_of(5) && !matches!(find(lane).await, Ok(Some(_))) {
            let logs = docker(&["logs", "--tail", "15", &name], Duration::from_secs(15))
                .await
                .map(|o| format!("{}{}", o.stdout, o.stderr))
                .unwrap_or_default();
            let _ = docker(&["rm", "-f", &name], Duration::from_secs(60)).await;
            forget(&name);
            return Err(refuse(
                502,
                "exited",
                json!({
                    "error": "the sandbox container exited before computer-server answered",
                    "container_log_tail": logs.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),
                }),
            ));
        }
        if std::time::Instant::now() > deadline {
            let _ = docker(&["rm", "-f", &name], Duration::from_secs(60)).await;
            forget(&name);
            return Err(refuse(
                504,
                "not_ready",
                json!({ "error": "computer-server did not answer within 180s; the container was removed" }),
            ));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(Started {
        sandbox,
        reused: false,
        image,
        image_built,
        colima_started,
        ready_after_s: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    })
}

/// Build the sandbox image if this Dockerfile's tag is absent. Returns the
/// tag and whether a build ran.
pub async fn ensure_image(lim: &Limits) -> anyhow::Result<(String, bool)> {
    let tag = image_tag(&lim.base_image);
    if docker(
        &["image", "inspect", &tag, "--format", "{{.Id}}"],
        Duration::from_secs(15),
    )
    .await?
    .ok
    {
        return Ok((tag, false));
    }
    let bin = docker_bin().ok_or_else(|| anyhow::anyhow!("docker CLI not found"))?;
    let recipe = recipe(&lim.base_image);
    tracing::info!(
        "[computer] verdict=image_build tag={tag} base={} (first start after a Dockerfile change)",
        lim.base_image
    );
    let t0 = std::time::Instant::now();
    let o = run(
        &bin,
        &["build", "--tag", &tag, "-"],
        Some(&recipe),
        Duration::from_secs(1800),
    )
    .await?;
    if !o.ok {
        let tail: Vec<&str> = o.stderr.lines().rev().take(8).collect();
        anyhow::bail!(
            "docker build failed after {}s: {}",
            t0.elapsed().as_secs(),
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    tracing::info!(
        "[computer] verdict=image_built tag={tag} build_s={}",
        t0.elapsed().as_secs()
    );
    Ok((tag, true))
}

/// Remove a lane's sandbox. `Ok(false)` when there was none.
pub async fn stop_name(name: &str) -> anyhow::Result<bool> {
    let o = docker(&["rm", "-f", name], Duration::from_secs(60)).await?;
    forget(name);
    rm_outcome(o.ok, &o.stderr).map_err(|e| anyhow::anyhow!("docker rm -f {name}: {e}"))
}

/// Did `docker rm -f` remove something? The stderr is checked BEFORE the exit
/// status: Docker 29 exits 0 from `rm -f` on a missing container and only
/// says so on stderr, which made a second `stop` report `removed: true` on
/// the first e2e run.
pub fn rm_outcome(ok: bool, stderr: &str) -> Result<bool, String> {
    if stderr.contains("No such container") {
        return Ok(false);
    }
    if ok {
        return Ok(true);
    }
    Err(first_line(stderr, "no stderr"))
}

// ---------------------------------------------------------------------------
// Chromium inside the sandbox
// ---------------------------------------------------------------------------

/// Chromium's argv inside the sandbox. `--no-sandbox` because Chromium's own
/// sandbox needs user namespaces a default Docker seccomp profile denies; the
/// container is the sandbox here.
pub fn chromium_args() -> Vec<&'static str> {
    vec![
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--no-first-run",
        "--no-default-browser-check",
        "--password-store=basic",
        "--start-maximized",
        "--remote-debugging-port=9222",
        "--user-data-dir=/home/kasm-user/.amux-chromium",
        "about:blank",
    ]
}

async fn cdp_version(port: u16) -> Option<Value> {
    let r = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    r.json().await.ok()
}

/// Make sure Chromium is running in the sandbox with DevTools reachable from
/// the host through the published relay port. Returns (relay port, launched).
pub async fn ensure_chromium(b: &Sandbox) -> anyhow::Result<(u16, bool)> {
    let port = b.cdp_port.ok_or_else(|| {
        anyhow::anyhow!("sandbox has no published DevTools relay port (started by an older amux?)")
    })?;
    if cdp_version(port).await.is_some() {
        return Ok((port, false));
    }
    let mut args: Vec<String> = [
        "exec",
        "-d",
        "-u",
        "1000",
        "-e",
        "DISPLAY=:1",
        "-e",
        "HOME=/home/kasm-user",
        &b.name,
        "chromium",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(chromium_args().into_iter().map(str::to_string));
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let o = docker(&argv, Duration::from_secs(30)).await?;
    if !o.ok {
        anyhow::bail!("launching chromium: {}", first_line(&o.stderr, "no stderr"));
    }
    let relay = format!("TCP-LISTEN:{CDP_RELAY_PORT},fork,reuseaddr");
    let target = format!("TCP:127.0.0.1:{CDP_PORT}");
    let o = docker(
        &["exec", "-d", &b.name, "socat", &relay, &target],
        Duration::from_secs(30),
    )
    .await?;
    if !o.ok {
        anyhow::bail!(
            "starting the DevTools relay: {}",
            first_line(&o.stderr, "no stderr")
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    while cdp_version(port).await.is_none() {
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "chromium started but DevTools did not answer through the relay within 40s"
            );
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    Ok((port, true))
}

/// Browser-level CDP session on a loopback DevTools port.
pub async fn browser_cdp(port: u16) -> anyhow::Result<crate::integrations::browser::CdpClient> {
    let v = cdp_version(port).await.ok_or_else(|| {
        anyhow::anyhow!("DevTools on 127.0.0.1:{port} did not answer /json/version")
    })?;
    let ws = v
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("/json/version carried no webSocketDebuggerUrl"))?;
    // Chromium builds the URL from the Host header it saw, which through the
    // relay is already the host-side port; normalise the host part anyway so
    // the loopback-only client accepts it.
    let path = ws.splitn(4, '/').nth(3).unwrap_or("");
    crate::integrations::browser::CdpClient::connect(&format!("ws://127.0.0.1:{port}/{path}")).await
}

/// Network.Cookie (what Storage.getCookies returns) to Network.CookieParam
/// (what Storage.setCookies takes). Session cookies (`session: true` or
/// expires <= 0) must NOT carry `expires`, or Chromium stores them as already
/// expired.
pub fn cookie_params(cookies: &[Value]) -> Vec<Value> {
    cookies
        .iter()
        .filter_map(|c| {
            let name = c.get("name")?.as_str()?;
            let domain = c.get("domain")?.as_str()?;
            let mut p = serde_json::Map::new();
            p.insert("name".into(), json!(name));
            p.insert("value".into(), c.get("value").cloned().unwrap_or(json!("")));
            p.insert("domain".into(), json!(domain));
            p.insert("path".into(), c.get("path").cloned().unwrap_or(json!("/")));
            for k in ["secure", "httpOnly", "sameSite", "priority"] {
                if let Some(v) = c.get(k) {
                    p.insert(k.into(), v.clone());
                }
            }
            let session = c.get("session").and_then(Value::as_bool).unwrap_or(false);
            if let Some(e) = c.get("expires").and_then(Value::as_f64) {
                if !session && e > 0.0 {
                    p.insert("expires".into(), json!(e));
                }
            }
            Some(Value::Object(p))
        })
        .collect()
}

/// Where a profile's cookies came from, for the response.
#[derive(Debug, Serialize)]
pub struct CookieExport {
    pub profile: String,
    pub cookies: usize,
    /// "running" when read from an amux browser already on the profile,
    /// "launched" when a headless one was started for the read and stopped.
    pub source: &'static str,
}

/// Read a saved amux profile's cookies through the amux browser's CDP.
///
/// WHY NOT COPY THE PROFILE. amux profiles are real macOS Google Chrome
/// user-data-dirs, and Chrome encrypts every cookie value with a key from the
/// macOS keychain ("Chrome Safe Storage", see `profile_combine`). Linux
/// Chromium cannot decrypt those blobs, so a copied or mounted profile arrives
/// signed OUT. CDP hands back the decrypted cookies, and reading them never
/// writes to the source profile.
pub async fn export_profile_cookies(
    profile: &str,
    lane: &str,
) -> anyhow::Result<(Vec<Value>, CookieExport)> {
    use crate::integrations::browser as chrome;
    let home = chrome::amux_home();
    let (port, launched) = match chrome::running_snapshot_for(profile) {
        Some((_, _, _, _, port)) => (port, false),
        None => {
            let s = chrome::start(&home, profile, "about:blank", lane, lane, true).await?;
            (s.cdp_port, true)
        }
    };
    let result = async {
        let mut c = browser_cdp(port).await?;
        let v = c
            .call("Storage.getCookies", json!({}), Duration::from_secs(20))
            .await?;
        Ok::<_, anyhow::Error>(
            v.get("cookies")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        )
    }
    .await;
    if launched {
        chrome::stop_profile_as_reason(&home, profile, lane, "computer: cookie export finished")
            .await;
    }
    let cookies = result?;
    let n = cookies.len();
    Ok((
        cookies,
        CookieExport {
            profile: profile.to_string(),
            cookies: n,
            source: if launched { "launched" } else { "running" },
        },
    ))
}

// ---------------------------------------------------------------------------
// Context: what a worker needs to know, derived live
// ---------------------------------------------------------------------------

/// The ladder and the verbs, as text a worker reads once. Profiles are
/// passed in so the rendering is pure.
pub fn context_text(profiles: &[(String, Vec<String>)], lim: &Limits) -> String {
    let mut s = String::new();
    s.push_str("amux computer: a disposable Linux desktop (XFCE/Kasm, Chromium, Firefox, terminal) driven by pixels.\n");
    s.push_str("Access ladder: 1) amux browser (/api/browser, amux Chrome profiles)  2) CDP through the Chrome profile selected in Browser tab → fallback settings  3) this sandbox, only when 1 and 2 cannot reach it.\n");
    s.push_str("Choose identity and purpose first with amux browser profiles / amux browser for URL. Use amux browser route for the shared ladder. Cookie evidence needs a live site check; observe again after any handoff.\n");
    s.push_str("Verbs: amux computer start | screenshot (then Read the path) | click X Y | double-click X Y | move X Y | type TEXT | key KEY (enter, ctrl+l) | scroll DX DY | open URL [--profile NAME] | stop\n");
    s.push_str("Loop: screenshot, read coordinates off the image, act, screenshot again to confirm. Stop when done.\n");
    s.push_str(&format!(
        "Limits: {} sandboxes fleet-wide, {} RAM / {} CPUs each, auto-stop after {}s idle.\n",
        lim.max, lim.memory, lim.cpus, lim.idle_s
    ));
    s.push_str("open --profile copies that amux profile's cookies (not its localStorage) into the sandbox Chromium; it never writes to the profile.\n");
    if profiles.is_empty() {
        s.push_str("Saved amux profiles with cookies: none found.\n");
    } else {
        // Cookies for a site are not proof of a login (measured in the e2e:
        // a profile listing linkedin.com held only tracking cookies, and
        // opened to the sign-in page). Say what the data shows.
        s.push_str("Saved amux profiles (profile: top sites it holds cookies for; a cookie is not proof of a login, check the screenshot):\n");
        for (name, hosts) in profiles {
            s.push_str(&format!("  {name}: {}\n", hosts.join(", ")));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> Limits {
        Limits {
            max: 2,
            memory: "4g".into(),
            cpus: "2".into(),
            idle_s: 900,
            base_image: DEFAULT_BASE_IMAGE.into(),
        }
    }

    fn sb(name: &str, lane: &str, state: &str, started: i64) -> Sandbox {
        Sandbox {
            id: name.into(),
            name: name.into(),
            lane: lane.into(),
            state: state.into(),
            image: "amux-computer:x".into(),
            started_at: Some(started),
            api_port: Some(1),
            vnc_port: Some(2),
            cdp_port: Some(3),
        }
    }

    #[test]
    fn verbs_map_to_computer_server_commands() {
        let m = |v: &str, b: Value| map_action(v, &b).unwrap();
        assert_eq!(
            m("click", json!({"x": 10, "y": 20.4})),
            ("left_click".into(), json!({"x":10,"y":20}))
        );
        assert_eq!(m("double_click", json!({"x": 1, "y": 2})).0, "double_click");
        assert_eq!(m("move", json!({"x": 1, "y": 2})).0, "move_cursor");
        assert_eq!(
            m("type", json!({"text": "hi there"})),
            ("type_text".into(), json!({"text":"hi there"}))
        );
        assert_eq!(
            m("key", json!({"key": "Enter"})),
            ("press_key".into(), json!({"key":"enter"}))
        );
        assert_eq!(
            m("key", json!({"key": "ctrl+L"})),
            ("hotkey".into(), json!({"keys":["ctrl","l"]}))
        );
        // A literal plus sign is a key, not a chord.
        assert_eq!(m("key", json!({"key": "+"})).0, "press_key");
        // dy > 0 means "down" to the caller and must reach pynput as negative.
        assert_eq!(
            m("scroll", json!({"dx": 0, "dy": 3})),
            ("scroll".into(), json!({"x":0,"y":-3}))
        );
        assert_eq!(m("screenshot", json!({})).0, "screenshot");
    }

    #[test]
    fn bad_bodies_are_refused_with_the_reason() {
        assert!(map_action("click", &json!({"x": 1}))
            .unwrap_err()
            .contains("x and y"));
        assert!(map_action("type", &json!({})).is_err());
        assert!(map_action("key", &json!({"key": " "})).is_err());
        assert!(map_action("key", &json!({"key": "ctrl++"})).is_err());
        assert!(map_action("scroll", &json!({})).is_err());
        assert!(map_action("launch_missiles", &json!({})).is_err());
    }

    #[test]
    fn cmd_response_parses_success_and_failure() {
        let ok =
            parse_cmd_response("data: {\"success\": true, \"image_data\": \"QQ==\"}\n\n").unwrap();
        assert_eq!(ok["image_data"], "QQ==");
        assert_eq!(
            parse_cmd_response("data: {\"success\": false, \"error\": \"no display\"}\n\n")
                .unwrap_err(),
            "no display"
        );
        assert!(parse_cmd_response("").unwrap_err().contains("<empty>"));
        assert!(parse_cmd_response("data: nope")
            .unwrap_err()
            .contains("not JSON"));
    }

    #[test]
    fn run_args_carry_limits_labels_and_loopback_only_ports() {
        let a = run_args(&lim(), "my.lane", "amux-computer:abc", 1700);
        let s = a.join(" ");
        assert!(s.contains(&format!("--name {}", container_name("my.lane"))));
        assert!(s.contains("--label amux-computer=my.lane"));
        // Kasm exits on a VNC password under 6 characters.
        let pw = a.iter().find_map(|x| x.strip_prefix("VNC_PW=")).unwrap();
        assert!(pw.len() >= 6, "{pw}");
        assert!(s.contains("--label amux-computer.started=1700"));
        assert!(s.contains("--memory 4g --memory-swap 4g --cpus 2"));
        for p in [API_PORT, VNC_PORT, CDP_RELAY_PORT] {
            assert!(s.contains(&format!("-p 127.0.0.1::{p}")), "{s}");
        }
        // Nothing may publish on all interfaces.
        assert!(!a
            .iter()
            .any(|x| x.starts_with("0.0.0.0") || x == &format!("{API_PORT}:{API_PORT}")));
        assert_eq!(a.last().unwrap(), "amux-computer:abc");
    }

    #[test]
    fn container_names_are_docker_safe_and_distinct() {
        let a = container_name("a.b");
        let b = container_name("a_b");
        assert_ne!(a, b);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(container_name(&"x".repeat(200)).len() < 70);
    }

    #[test]
    fn ps_line_parses_lane_ports_and_start() {
        let line = r#"{"ID":"abc","Names":"amux-computer-w-123456","State":"running","Image":"amux-computer:x","Labels":"amux-computer=w,amux-computer.started=1700,other=1","Ports":"127.0.0.1:55001->8000/tcp, 127.0.0.1:55002->6901/tcp, 127.0.0.1:55003->9223/tcp, 5901/tcp"}"#;
        let b = parse_ps_line(line).unwrap();
        assert_eq!(b.lane, "w");
        assert_eq!(b.started_at, Some(1700));
        assert_eq!(
            (b.api_port, b.vnc_port, b.cdp_port),
            (Some(55001), Some(55002), Some(55003))
        );
        assert!(b.running());
        assert_eq!(host_port("5901/tcp", 5901), None);
    }

    #[test]
    fn idle_clock_uses_the_later_of_start_and_last_action() {
        assert_eq!(idle_for(1000, Some(100), Some(900)), Some(100));
        assert_eq!(idle_for(1000, Some(950), Some(900)), Some(50));
        assert_eq!(idle_for(1000, None, None), None);
        assert_eq!(idle_for(1000, Some(2000), None), Some(0));
    }

    #[test]
    fn sweep_removes_idle_exited_duplicate_and_unowned_only() {
        let boxes = vec![
            sb("fresh", "a", "running", 1000),
            sb("idle", "b", "running", 0),
            sb("dead", "c", "exited", 900),
            sb("old-dup", "d", "running", 500),
            sb("new-dup", "d", "running", 990),
            sb("orphan", "", "running", 1000),
        ];
        let acts = |_: &str| None;
        let plan = plan_sweep(&boxes, 1000, 900, &acts);
        let names: Vec<&str> = plan.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["idle", "dead", "old-dup", "orphan"]);
        assert!(plan[0].reason.contains("idle 1000s"));
        // A recent action keeps an old sandbox alive.
        let recent = |n: &str| (n == "idle").then_some(950);
        assert!(!plan_sweep(&boxes, 1000, 900, &recent)
            .iter()
            .any(|a| a.name == "idle"));
        // idle_s = 0 disables only the idle arm.
        let off = plan_sweep(&boxes, 1000, 0, &acts);
        assert_eq!(off.len(), 3);
        assert!(!off.iter().any(|a| a.name == "idle"));
    }

    #[test]
    fn cap_counts_other_lanes_running_sandboxes_only() {
        let boxes = vec![
            sb("x", "a", "running", 1),
            sb("y", "b", "running", 1),
            sb("z", "c", "exited", 1),
        ];
        assert_eq!(
            cap_refusal(&boxes, "c", 2),
            Some(vec!["a".into(), "b".into()])
        );
        // The caller's own sandbox never blocks it (it is reused).
        assert_eq!(cap_refusal(&boxes, "a", 2), None);
        assert_eq!(cap_refusal(&boxes, "c", 3), None);
    }

    #[test]
    fn cookie_params_drop_expiry_on_session_cookies() {
        let got = cookie_params(&[
            json!({"name":"s","value":"1","domain":".x.com","path":"/","expires":-1,"session":true,"secure":true,"httpOnly":true,"sameSite":"Lax","size":2}),
            json!({"name":"p","value":"2","domain":"x.com","path":"/a","expires":1.9e9,"session":false}),
            json!({"value":"no name"}),
        ]);
        assert_eq!(got.len(), 2);
        assert!(got[0].get("expires").is_none());
        assert_eq!(got[0]["sameSite"], "Lax");
        assert!(
            got[0].get("size").is_none(),
            "read-only fields must not reach setCookies"
        );
        assert_eq!(got[1]["expires"], 1.9e9);
    }

    #[test]
    fn colima_profile_comes_from_the_docker_context() {
        if std::env::var("AMUX_COMPUTER_COLIMA_PROFILE").is_ok() {
            return;
        }
        assert_eq!(
            colima_profile_for(Some("colima-gs7-e")).as_deref(),
            Some("gs7-e")
        );
        assert_eq!(
            colima_profile_for(Some("colima")).as_deref(),
            Some("default")
        );
        assert_eq!(colima_profile_for(Some("desktop-linux")), None);
    }

    #[test]
    fn context_names_the_ladder_verbs_and_profiles() {
        let t = context_text(&[("work".into(), vec!["github.com".into()])], &lim());
        assert!(t.contains("1) amux browser"));
        assert!(t.contains("3) this sandbox"));
        assert!(t.contains("open URL [--profile NAME]"));
        assert!(t.contains("work: github.com"));
        assert!(t.contains("900s idle"));
    }

    #[test]
    fn rm_of_a_missing_container_is_not_a_removal_even_on_exit_0() {
        assert_eq!(
            rm_outcome(true, "Error response from daemon: No such container: x"),
            Ok(false)
        );
        assert_eq!(rm_outcome(true, ""), Ok(true));
        assert!(rm_outcome(false, "permission denied").is_err());
    }

    #[test]
    fn image_tag_tracks_the_dockerfile() {
        assert!(image_tag(DEFAULT_BASE_IMAGE).starts_with("amux-computer:"));
        assert!(
            recipe("other/base:1").contains("FROM other/base:1"),
            "recipe must rewrite the FROM line"
        );
        assert_ne!(image_tag(DEFAULT_BASE_IMAGE), image_tag("other/base:1"));
    }
}
