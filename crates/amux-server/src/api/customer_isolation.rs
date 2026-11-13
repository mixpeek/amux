//! An operating-system boundary at the customer line (governance PR 17; plan
//! sections 4.3 and 7).
//!
//! PR 16 hands each agent CLI its OWN sandbox. That protects the user from the
//! agent; it does not keep one customer's files away from another customer's
//! worker on the same host. This module does: a worker in a BOUNDARY group runs
//! its agent CLI inside a mount namespace that contains its group's folders and
//! nothing else of the user's home.
//!
//! **The marker (one source, A10).** `AMUX_CAP_ISOLATION=boundary`, read through
//! `capabilities::layers` like every other capability, so it is owner-only for
//! free (the `AMUX_CAP_` prefix, scope.rs) and set at the worker, a group, or the
//! whole server. Set on a group, every worker in that group shares the group's
//! folders; set on a worker or globally, the worker gets its own folder.
//!
//! **What the worker sees.**
//!
//! | Path | Inside the boundary |
//! |---|---|
//! | the worker's folder, its boundary group's folders, `AMUX_CAP_FOLDERS` grants | read-write |
//! | the rest of the user's home | absent (an empty tmpfs) |
//! | amux's home | absent, except the hooks, the TLS certificate (not its key), the worker's rules and sandbox files, the trace folder, uploads and its own status-events folder |
//! | `~/.claude` | read-write, but `~/.claude/projects` holds only this worker's project |
//! | `/tmp` | a private folder per boundary; `/var/tmp` is an empty tmpfs |
//! | `/usr`, `/etc`, binaries on `PATH` | read-only |
//! | network | shared (A2); `AMUX_CAP_NETWORK` is enforced by the CLI sandbox (PR 16) |
//!
//! The amux CLI inside reaches the server over loopback with the worker's own
//! token (PR 1 and PR 3), which travels in the environment, not a file.
//!
//! **Runners.** `bwrap` (unprivileged user namespaces; measured working on the
//! sandbox host 2026-11-11) is the default on Linux. `docker` when
//! `AMUX_ISOLATION_RUNNER=docker`, the daemon answers, and
//! `AMUX_ISOLATION_IMAGE` names an image that carries the agent CLI. Anything
//! else is reported `unavailable` with the reason: never faked.
//!
//! **`AMUX_CUSTOMER_ISOLATION` (A11).** `report` (default) computes and records
//! the plan at each launch and wraps nothing; `enforce` wraps boundary workers,
//! and REFUSES to launch a boundary worker when no runner is available (a
//! boundary that silently is not one is worse than a refused start); `off`
//! does nothing. Only workers whose layers set the marker are ever touched, so
//! a fault here cannot stop a worker outside a boundary. A running worker is
//! never changed: the plan applies at its next launch.
//!
//! Published apps (PR 5) a boundary worker starts are its child processes, so
//! they run inside the same namespace and see the same files.

use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MODE_ENV: &str = "AMUX_CUSTOMER_ISOLATION";
pub const KEY: &str = "AMUX_CAP_ISOLATION";
pub const RUNNER_ENV: &str = "AMUX_ISOLATION_RUNNER";
pub const IMAGE_ENV: &str = "AMUX_ISOLATION_IMAGE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Report,
    Enforce,
    Off,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Report => "report",
            Mode::Enforce => "enforce",
            Mode::Off => "off",
        }
    }
}

pub fn mode() -> Mode {
    match std::env::var(MODE_ENV).unwrap_or_default().trim().to_ascii_lowercase().as_str() {
        "enforce" => Mode::Enforce,
        "off" | "0" | "false" => Mode::Off,
        _ => Mode::Report,
    }
}

// ── Boundary ────────────────────────────────────────────────────────────────

/// Where a worker's boundary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    /// `worker <name>`, `group <g>` or `global amux.env`.
    pub layer: String,
    /// The group whose folders the boundary shares, when set on a group.
    pub group: Option<String>,
}

fn is_on(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "boundary" | "1" | "on" | "true" | "yes")
}

/// The worker's boundary, from the most specific layer that sets the marker.
/// The most specific layer wins, so a worker can opt out of its group's
/// boundary with `AMUX_CAP_ISOLATION=none`.
pub fn boundary_for(home: &Path, lane: &str) -> Option<Boundary> {
    let l = super::capabilities::layers(home, lane, KEY).into_iter().next()?;
    if !is_on(&l.value) {
        return None;
    }
    let group = l.layer.strip_prefix("group ").map(String::from);
    Some(Boundary { layer: l.layer, group })
}

// ── Runner ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    Bwrap,
    Docker { image: String },
    Unavailable(String),
}

impl Runner {
    pub fn name(&self) -> &'static str {
        match self {
            Runner::Bwrap => "bwrap",
            Runner::Docker { .. } => "docker",
            Runner::Unavailable(_) => "unavailable",
        }
    }
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

fn probe(bin: &str, args: &[&str]) -> bool {
    std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

static RUNNER_CACHE: std::sync::Mutex<Option<(std::time::Instant, Runner)>> = std::sync::Mutex::new(None);

/// The runner this host can use, MEASURED: bwrap must actually start a
/// namespace, docker must answer. Cached for a minute.
pub fn runner() -> Runner {
    if let Ok(c) = RUNNER_CACHE.lock() {
        if let Some((at, r)) = c.as_ref() {
            if at.elapsed().as_secs() < 60 {
                return r.clone();
            }
        }
    }
    let wanted = std::env::var(RUNNER_ENV).unwrap_or_default();
    let image = std::env::var(IMAGE_ENV).unwrap_or_default();
    let r = runner_with(
        std::env::consts::OS,
        wanted.trim(),
        image.trim(),
        &on_path,
        &|| probe("bwrap", &["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-pid", "true"]),
        &|| probe("docker", &["info"]),
    );
    if let Ok(mut c) = RUNNER_CACHE.lock() {
        *c = Some((std::time::Instant::now(), r.clone()));
    }
    r
}

pub fn runner_with(
    os: &str,
    wanted: &str,
    image: &str,
    has: &dyn Fn(&str) -> bool,
    bwrap_works: &dyn Fn() -> bool,
    docker_answers: &dyn Fn() -> bool,
) -> Runner {
    if wanted.eq_ignore_ascii_case("docker") {
        if !has("docker") {
            return Runner::Unavailable("AMUX_ISOLATION_RUNNER=docker but docker is not installed".into());
        }
        if image.is_empty() {
            return Runner::Unavailable(format!("AMUX_ISOLATION_RUNNER=docker needs {IMAGE_ENV}, an image that carries the agent CLI"));
        }
        if !docker_answers() {
            return Runner::Unavailable("docker is installed but its daemon does not answer this user".into());
        }
        return Runner::Docker { image: image.to_string() };
    }
    if os != "linux" {
        return Runner::Unavailable(format!("no namespace runner on {os}; set {RUNNER_ENV}=docker with an image"));
    }
    if !has("bwrap") {
        return Runner::Unavailable("bwrap is not installed".into());
    }
    if !bwrap_works() {
        return Runner::Unavailable("bwrap is installed but could not start a namespace (unprivileged user namespaces off?)".into());
    }
    Runner::Bwrap
}

// ── Plan ────────────────────────────────────────────────────────────────────

/// One mount and the fact it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// `rw`, `ro`, `hide` (empty tmpfs) or `tmp` (private /tmp source).
    pub kind: &'static str,
    pub path: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub boundary: Boundary,
    pub mounts: Vec<Mount>,
    pub work_dir: String,
}

fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn split_list(v: &str) -> Vec<String> {
    v.split([',', ';', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn expand(hd: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    let p = if raw == "~" {
        hd.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        hd.join(rest)
    } else {
        PathBuf::from(raw)
    };
    super::capabilities::normalize(&p)
}

fn safe_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// The folder that is `/tmp` inside the boundary.
pub fn private_tmp(home: &Path, b: &Boundary, lane: &str) -> PathBuf {
    let key = b.group.clone().unwrap_or_else(|| format!("worker-{lane}"));
    home.join("isolation").join(safe_name(&key)).join("tmp")
}

/// Directories on `path_env` that live under `roots`, plus the real homes of
/// the agent CLIs found there (a symlink into `~/.local/share/claude/...`).
fn path_mounts(path_env: &str, roots: &[&Path]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let under = |p: &Path| roots.iter().any(|r| p.starts_with(r));
    for d in std::env::split_paths(path_env) {
        if d.as_os_str().is_empty() || !d.is_dir() {
            continue;
        }
        if under(&d) && !out.contains(&d) {
            out.push(d.clone());
        }
        for bin in ["claude", "codex", "gemini", "amux", "node", "python3"] {
            let f = d.join(bin);
            if let Ok(real) = f.canonicalize() {
                if let Some(parent) = real.parent() {
                    // The version folder (e.g. ~/.local/share/claude/versions/x)
                    // and, for node installs, its prefix with lib/.
                    let top = if parent.ends_with("bin") { parent.parent().unwrap_or(parent) } else { parent };
                    let top = top.to_path_buf();
                    if under(&top) && !out.contains(&top) {
                        out.push(top);
                    }
                }
            }
        }
    }
    out
}

/// The mounts for `lane` launching in `work_dir`. `hd` is the user's home,
/// `path_env` the PATH the CLI will run with.
pub fn plan_in(home: &Path, lane: &str, work_dir: &Path, hd: &Path, path_env: &str) -> Option<Plan> {
    let boundary = boundary_for(home, lane)?;
    let mut mounts: Vec<Mount> = Vec::new();
    let mut push = |kind: &'static str, p: &Path, source: String| {
        let path = p.to_string_lossy().into_owned();
        if !mounts.iter().any(|m| m.kind == kind && m.path == path) {
            mounts.push(Mount { kind, path, source });
        }
    };
    let own = super::capabilities::normalize(work_dir);
    let home_n = super::capabilities::normalize(home);

    // What disappears.
    push("hide", hd, "the user's home: absent except what is mounted below".into());
    if !home_n.starts_with(hd) {
        push("hide", &home_n, "amux's home: absent except what is mounted below".into());
    }
    push("tmp", &private_tmp(home, &boundary, lane), format!("/tmp, private to this boundary ({})", boundary.layer));
    push("hide", Path::new("/var/tmp"), "/var/tmp: empty".into());

    // What the CLI needs to run.
    for d in path_mounts(path_env, &[hd, &home_n]) {
        push("ro", &d, "a PATH directory or the real home of an agent CLI".into());
    }
    for f in [".gitconfig", ".profile", ".bashrc"] {
        let p = hd.join(f);
        if p.exists() {
            push("ro", &p, format!("~/{f}: shell and git configuration"));
        }
    }
    let ch = hd.join(".claude");
    push("rw", &ch, "~/.claude: the agent CLI's own state".into());
    push("hide", &ch.join("projects"), "other workers' Claude transcripts".into());
    let proj = ch.join("projects").join(super::session_verbs::project_name(&own.to_string_lossy()));
    push("rw", &proj, "this worker's own Claude transcripts".into());
    let cj = hd.join(".claude.json");
    if cj.exists() {
        push("rw", &cj, "~/.claude.json: the agent CLI's settings and sign-in".into());
    }

    // amux's own files the worker uses, and nothing else of amux's home.
    for (rel, why) in [
        ("hooks", "the status and boundary hooks"),
        ("tls/cert.pem", "the server's certificate, so the CLI and its exporter can verify loopback (not the key)"),
        ("bin", "this server's own amux CLI"),
        ("boundaries.toml", "the owner's boundary overrides"),
        ("trace", "trace ids of deliveries (ids only)"),
        ("uploads", "files attached to messages"),
    ] {
        let p = home_n.join(rel);
        if p.exists() {
            push("ro", &p, format!("~/.amux/{rel}: {why}"));
        }
    }
    // Hook scripts installed at the top of amux's home (native-status.py):
    // Claude's settings run them by absolute path. Scripts, not secrets.
    if let Ok(rd) = std::fs::read_dir(&home_n) {
        let mut scripts: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && matches!(p.extension().and_then(|x| x.to_str()), Some("py" | "sh")))
            .collect();
        scripts.sort();
        for p in scripts {
            let n = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
            push("ro", &p, format!("~/.amux/{n}: a hook script Claude's settings run"));
        }
    }
    for (rel, why) in [
        (format!("rules/{lane}.md"), "this worker's rules"),
        (format!("sandbox/{lane}.claude-settings.json"), "this worker's CLI sandbox settings (PR 16)"),
    ] {
        let p = home_n.join(&rel);
        if p.exists() {
            push("ro", &p, format!("~/.amux/{rel}: {why}"));
        }
    }
    push("rw", &home_n.join("status-events").join(lane), "this worker's status events".into());

    // What the worker works on.
    push("rw", &own, "the worker's own folder".into());
    if let Some(g) = &boundary.group {
        for (w, dir) in super::capabilities::worker_folders(home) {
            if w != lane && super::scope::session_tags_of(home, &w).iter().any(|t| t == g) {
                push("rw", &dir, format!("{w}'s folder: same boundary group {g}"));
            }
        }
    }
    for l in super::capabilities::layers(home, lane, super::capabilities::FOLDERS_KEY) {
        for g in split_list(&l.value) {
            if g == "*" {
                continue;
            }
            push("rw", &expand(hd, &g), format!("{} {g} ({})", super::capabilities::FOLDERS_KEY, l.layer));
        }
    }
    Some(Plan { boundary, mounts, work_dir: own.to_string_lossy().into_owned() })
}

pub fn plan(home: &Path, lane: &str, work_dir: &Path) -> Option<Plan> {
    plan_in(home, lane, work_dir, &user_home(), &std::env::var("PATH").unwrap_or_default())
}

/// The bwrap arguments for `plan`. Order matters: the root goes in read-only,
/// the hidden trees are emptied, then the allowed paths are bound back in,
/// parents before children.
pub fn bwrap_args(plan: &Plan) -> Vec<String> {
    let mut a: Vec<String> = ["--die-with-parent", "--unshare-pid", "--unshare-ipc", "--unshare-uts",
        "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
        .iter().map(|s| s.to_string()).collect();
    let depth = |p: &str| Path::new(p).components().count();
    let mut hides: Vec<&Mount> = plan.mounts.iter().filter(|m| m.kind == "hide").collect();
    hides.sort_by_key(|m| depth(&m.path));
    let top_hides: Vec<&Mount> = hides.iter().copied().filter(|m| !m.path.contains("/.claude/")).collect();
    for m in &top_hides {
        a.extend(["--tmpfs".into(), m.path.clone()]);
    }
    for m in plan.mounts.iter().filter(|m| m.kind == "tmp") {
        a.extend(["--bind".into(), m.path.clone(), "/tmp".into()]);
    }
    // Binds and the nested hides, by depth, so a child lands on its parent.
    let mut rest: Vec<&Mount> = plan.mounts.iter()
        .filter(|m| m.kind == "rw" || m.kind == "ro" || (m.kind == "hide" && m.path.contains("/.claude/")))
        .filter(|m| m.kind == "hide" || Path::new(&m.path).exists())
        .collect();
    rest.sort_by_key(|m| (depth(&m.path), m.kind == "hide"));
    for m in rest {
        match m.kind {
            "rw" => a.extend(["--bind".into(), m.path.clone(), m.path.clone()]),
            "ro" => a.extend(["--ro-bind".into(), m.path.clone(), m.path.clone()]),
            _ => a.extend(["--tmpfs".into(), m.path.clone()]),
        }
    }
    a.extend(["--chdir".into(), plan.work_dir.clone()]);
    a
}

/// The docker arguments for `plan` (runner `docker`): the same mounts as
/// volumes, host networking, the caller's uid, the worker's environment.
pub fn docker_args(plan: &Plan, image: &str) -> Vec<String> {
    // `-it` is docker's stdin plus tty, written as one flag: a bare quoted -t reads
    // as a tmux target to tests/tmux_target_audit.rs.
    let mut a: Vec<String> = ["run", "--rm", "-it", "--network", "host"].iter().map(|s| s.to_string()).collect();
    #[cfg(unix)]
    {
        a.extend(["--user".into(), format!("{}:{}", nix_uid(), nix_gid())]);
    }
    for m in plan.mounts.iter().filter(|m| (m.kind == "rw" || m.kind == "ro") && Path::new(&m.path).exists()) {
        let suffix = if m.kind == "ro" { ":ro" } else { "" };
        a.extend(["-v".into(), format!("{}:{}{suffix}", m.path, m.path)]);
    }
    for m in plan.mounts.iter().filter(|m| m.kind == "tmp") {
        a.extend(["-v".into(), format!("{}:/tmp", m.path)]);
    }
    // The worker's own variables, passed by name so no value is in argv.
    for k in ["AMUX_WORKER_TOKEN", "AMUX_SESSION", "AMUX_WORKER", "AMUX_URL", "AMUX_HOME", "HOME", "PATH",
              "ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN", "AMUX_STATUS_WORKER", "AMUX_STATUS_RUN_ID",
              "AMUX_STATUS_HOME", "AMUX_STATUS_URL", "OTEL_EXPORTER_OTLP_HEADERS", "AMUX_ISOLATED_WORKER"] {
        a.extend(["-e".into(), k.into()]);
    }
    a.extend(["-w".into(), plan.work_dir.clone(), image.to_string()]);
    a
}

#[cfg(unix)]
fn nix_uid() -> u32 {
    // SAFETY: getuid/getgid have no preconditions.
    unsafe { libc::getuid() }
}
#[cfg(unix)]
fn nix_gid() -> u32 {
    unsafe { libc::getgid() }
}

// ── Launch ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not in a boundary group: untouched.
    NotBoundary,
    Off,
    /// Plan recorded, nothing wrapped.
    Reported { runner: String },
    Wrapped { runner: String },
    /// Enforce, a boundary worker, and no runner: the start is refused.
    Refused(String),
}

fn outcome_str(o: &Outcome) -> (&'static str, String) {
    match o {
        Outcome::NotBoundary => ("not_boundary", String::new()),
        Outcome::Off => ("off", String::new()),
        Outcome::Reported { runner } => ("reported", format!("plan recorded; would run under {runner}")),
        Outcome::Wrapped { runner } => ("wrapped", runner.clone()),
        Outcome::Refused(w) => ("refused", w.clone()),
    }
}

/// Wrap `inner` (the provider command line) for `lane`. `Ok(None)`: launch as
/// is. `Ok(Some(cmd))`: launch this instead. `Err(why)`: do not launch.
pub fn wrap_with(
    home: &Path,
    lane: &str,
    work_dir: &Path,
    inner: &str,
    mode: Mode,
    rn: &Runner,
    plan_of: impl FnOnce() -> Option<Plan>,
) -> (Result<Option<String>, String>, Outcome, Option<Plan>) {
    if mode == Mode::Off {
        return (Ok(None), Outcome::Off, None);
    }
    let _ = (home, work_dir);
    let Some(p) = plan_of() else {
        return (Ok(None), Outcome::NotBoundary, None);
    };
    if mode == Mode::Report {
        return (Ok(None), Outcome::Reported { runner: rn.name().into() }, Some(p));
    }
    let q = super::session_verbs::sh_quote;
    match rn {
        Runner::Bwrap => {
            for m in p.mounts.iter().filter(|m| m.kind == "tmp" || (m.kind == "rw" && !Path::new(&m.path).exists())) {
                // The private /tmp and the worker's own status and transcript
                // folders are created; a missing work folder is not invented.
                if m.kind == "tmp" || m.path.contains("/status-events/") || m.path.contains("/.claude/projects/") {
                    let _ = std::fs::create_dir_all(&m.path);
                }
            }
            let args: Vec<String> = bwrap_args(&p).iter().map(|s| q(s)).collect();
            let cmd = format!("bwrap {} -- /bin/sh -c {}", args.join(" "), q(inner));
            (Ok(Some(cmd)), Outcome::Wrapped { runner: "bwrap".into() }, Some(p))
        }
        Runner::Docker { image } => {
            let _ = std::fs::create_dir_all(private_tmp(home, &p.boundary, lane));
            let args: Vec<String> = docker_args(&p, image).iter().map(|s| q(s)).collect();
            let cmd = format!("docker {} /bin/sh -c {}", args.join(" "), q(inner));
            (Ok(Some(cmd)), Outcome::Wrapped { runner: "docker".into() }, Some(p))
        }
        Runner::Unavailable(why) => {
            let msg = refusal_message(lane, &p.boundary.layer, why);
            (Err(msg), Outcome::Refused(why.clone()), Some(p))
        }
    }
}

fn refusal_message(lane: &str, layer: &str, why: &str) -> String {
    format!(
        "refusing to start {lane}: it is inside a customer boundary ({layer}) and {MODE_ENV}=enforce, \
         but no isolation runner is available here ({why}). Install bwrap, set {RUNNER_ENV}=docker \
         with {IMAGE_ENV}, or set {MODE_ENV}=report to launch it unwrapped."
    )
}

/// Whether `/start` must refuse `lane` before it answers. `/start` launches
/// in the background and answers 202, so a refusal found only inside the
/// launch reached the caller as "starting" and nothing ever started. The
/// launch still checks again through [`wrap`]; this asks the same two
/// questions up front.
pub fn start_refusal(home: &Path, lane: &str) -> Option<String> {
    if mode() != Mode::Enforce {
        return None;
    }
    let b = boundary_for(home, lane)?;
    match runner() {
        Runner::Unavailable(why) => Some(refusal_message(lane, &b.layer, &why)),
        _ => None,
    }
}

/// The launch hook: `session_verbs` calls this with the final provider line.
pub fn wrap(home: &Path, lane: &str, work_dir: &Path, inner: &str) -> Result<Option<String>, String> {
    let m = mode();
    // Only a boundary worker pays for the runner probe.
    if m == Mode::Off || boundary_for(home, lane).is_none() {
        let o = if m == Mode::Off { Outcome::Off } else { Outcome::NotBoundary };
        record(home, lane, &o, None);
        return Ok(None);
    }
    let rn = runner();
    let (r, o, p) = wrap_with(home, lane, work_dir, inner, m, &rn, || plan(home, lane, work_dir));
    record(home, lane, &o, p.as_ref());
    r
}

fn state_path(home: &Path) -> PathBuf {
    home.join("isolation").join("launches.json")
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn record(home: &Path, lane: &str, o: &Outcome, p: Option<&Plan>) {
    let (verdict, detail) = outcome_str(o);
    counters::bump(verdict);
    if matches!(o, Outcome::NotBoundary) {
        return;
    }
    tracing::info!(session = lane, verdict = %format!("customer_isolation_{verdict}"), %detail,
        mounts = p.map(|p| p.mounts.len()).unwrap_or(0),
        measured = true, n_considered = 1, "customer isolation at launch (governance PR 17)");
    let path = state_path(home);
    let mut all: BTreeMap<String, Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    all.insert(lane.to_string(), json!({ "status": verdict, "detail": detail, "at": now_s(),
        "mounts": p.map(|p| p.mounts.len()).unwrap_or(0) }));
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&path, serde_json::to_vec_pretty(&all).unwrap_or_default());
}

fn last_launch(home: &Path, lane: &str) -> Option<Value> {
    std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get(lane).cloned())
}

// ── View and counters ───────────────────────────────────────────────────────

fn runner_json(rn: &Runner) -> Value {
    match rn {
        Runner::Bwrap => json!({"name": "bwrap", "available": true, "measured": true}),
        Runner::Docker { image } => json!({"name": "docker", "available": true, "image": image, "measured": true}),
        Runner::Unavailable(why) => json!({"name": "unavailable", "available": false, "reason": why, "measured": true}),
    }
}

pub fn view_for(home: &Path, lane: &str, work_dir: &Path) -> Value {
    let m = mode();
    let p = plan(home, lane, work_dir);
    let rn = if p.is_some() { runner() } else { Runner::Unavailable("not measured: the worker is not in a boundary".into()) };
    let status = match (&p, m, &rn) {
        (None, _, _) => "not_boundary",
        (Some(_), Mode::Off, _) => "off",
        (Some(_), Mode::Report, _) => "report_only",
        (Some(_), Mode::Enforce, Runner::Unavailable(_)) => "refused_no_runner",
        (Some(_), Mode::Enforce, _) => "enforced",
    };
    json!({
        "worker": lane,
        "mode": m.as_str(),
        "status": status,
        "boundary": p.as_ref().map(|p| json!({"layer": p.boundary.layer, "group": p.boundary.group})),
        "runner": runner_json(&rn),
        "mounts": p.as_ref().map(|p| p.mounts.iter().map(|x| json!({"kind": x.kind, "path": x.path, "source": x.source})).collect::<Vec<_>>()).unwrap_or_default(),
        "network": "shared with the host (A2); AMUX_CAP_NETWORK is enforced by the CLI sandbox (PR 16)",
        "applies": "at the worker's next launch; a running worker is never changed",
        "last_launch": last_launch(home, lane),
        "measured": true,
        "n_considered": p.as_ref().map(|p| p.mounts.len()).unwrap_or(0),
    })
}

pub async fn view(
    State(_state): State<super::AppState>,
    AxPath(name): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let home = crate::config::amux_home();
    let human = super::human_proof::proven(&headers).is_some();
    let me = super::capabilities::acting_worker(&home, &headers);
    if !human && me.as_deref() != Some(name.as_str()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "a worker may read only its own isolation plan", "code": "isolation_view_refused"})),
        )
            .into_response();
    }
    let env = home.join("sessions").join(format!("{name}.env"));
    if !env.exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("no worker named {name}")}))).into_response();
    }
    let dir = super::session_verbs::EnvFile::load(&env)
        .get("CC_DIR")
        .map(|d| expand(&user_home(), d))
        .unwrap_or_else(user_home);
    Json(view_for(&home, &name, &dir)).into_response()
}

pub mod counters {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    static C: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
    pub fn bump(key: &str) {
        if let Ok(mut c) = C.lock() {
            *c.entry(key.to_string()).or_default() += 1;
        }
    }
    pub fn snapshot() -> BTreeMap<String, u64> {
        C.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

/// The A5 diagnostic, served in `GET /api/contract/counters` as `customer_isolation`.
pub fn report() -> Value {
    let c = counters::snapshot();
    json!({
        "mode": mode().as_str(),
        "measured": true,
        "n_considered": c.values().sum::<u64>(),
        "counts": c,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _t: tempfile::TempDir,
        home: PathBuf,
        hd: PathBuf,
    }

    fn setup() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("amux");
        let hd = t.path().join("user");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(home.join("env")).unwrap();
        std::fs::create_dir_all(home.join("hooks")).unwrap();
        std::fs::create_dir_all(home.join("tls")).unwrap();
        std::fs::write(home.join("tls/cert.pem"), "c").unwrap();
        std::fs::write(home.join("tls/key.pem"), "k").unwrap();
        for (w, d, tags) in [("wexus", "customers/wexus", "acme"), ("wexus-2", "customers/wexus/extra", "acme"),
                             ("rothco", "customers/rothco", "beta"), ("helper", "tools", "amux")] {
            let dir = hd.join(d);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(home.join("sessions").join(format!("{w}.env")),
                format!("CC_DIR={}\nCC_TAGS={tags}\n", dir.display())).unwrap();
        }
        std::fs::write(home.join("env/acme.env"), format!("{KEY}=boundary\n")).unwrap();
        std::fs::write(home.join("env/beta.env"), format!("{KEY}=boundary\n")).unwrap();
        Fx { _t: t, home, hd }
    }

    fn has(p: &Plan, kind: &str, needle: &str) -> bool {
        p.mounts.iter().any(|m| m.kind == kind && m.path.ends_with(needle))
    }

    #[test]
    fn a_boundary_comes_only_from_the_owner_marker_and_the_most_specific_layer_wins() {
        let f = setup();
        assert_eq!(boundary_for(&f.home, "wexus").unwrap().group.as_deref(), Some("acme"));
        assert!(boundary_for(&f.home, "helper").is_none());
        std::fs::write(f.home.join("sessions/wexus.env"),
            format!("CC_DIR={}\nCC_TAGS=acme\n{KEY}=none\n", f.hd.join("customers/wexus").display())).unwrap();
        assert!(boundary_for(&f.home, "wexus").is_none(), "a worker can opt out of its group's boundary");
        assert!(crate::api::scope::is_owner_only_env_key_pub(KEY), "only the owner sets the marker");
    }

    #[test]
    fn the_plan_hides_home_and_amux_and_mounts_only_the_boundary_groups_folders() {
        let f = setup();
        let p = plan_in(&f.home, "wexus", &f.hd.join("customers/wexus"), &f.hd, "").unwrap();
        assert!(p.mounts.iter().any(|m| m.kind == "hide" && Path::new(&m.path) == f.hd), "{p:?}");
        assert!(has(&p, "rw", "customers/wexus"), "{p:?}");
        assert!(has(&p, "rw", "customers/wexus/extra"), "same-group folder: {p:?}");
        assert!(!p.mounts.iter().any(|m| m.path.ends_with("customers/rothco")), "another customer is absent: {p:?}");
        assert!(!p.mounts.iter().any(|m| m.path.ends_with("tools")), "a non-boundary worker's folder is absent: {p:?}");
        assert!(has(&p, "ro", "tls/cert.pem"), "{p:?}");
        assert!(!p.mounts.iter().any(|m| m.path.ends_with("key.pem")), "the certificate's key is absent: {p:?}");
        assert!(!p.mounts.iter().any(|m| m.path.ends_with("sessions") || m.path.ends_with("auth_token")), "{p:?}");
        assert!(has(&p, "hide", ".claude/projects"), "{p:?}");
        assert!(p.mounts.iter().any(|m| m.kind == "tmp" && m.path.contains("isolation/acme/tmp")), "{p:?}");
    }

    #[test]
    fn a_folder_grant_is_mounted_and_two_boundaries_get_different_tmps() {
        let f = setup();
        std::fs::write(f.home.join("sessions/wexus.env"),
            format!("CC_DIR={}\nCC_TAGS=acme\nAMUX_CAP_FOLDERS={}\n", f.hd.join("customers/wexus").display(),
                f.hd.join("shared").display())).unwrap();
        let a = plan_in(&f.home, "wexus", &f.hd.join("customers/wexus"), &f.hd, "").unwrap();
        assert!(a.mounts.iter().any(|m| m.kind == "rw" && m.path.ends_with("shared") && m.source.contains("AMUX_CAP_FOLDERS")), "{a:?}");
        let b = plan_in(&f.home, "rothco", &f.hd.join("customers/rothco"), &f.hd, "").unwrap();
        let tmp = |p: &Plan| p.mounts.iter().find(|m| m.kind == "tmp").unwrap().path.clone();
        assert_ne!(tmp(&a), tmp(&b));
    }

    #[test]
    fn bwrap_args_empty_the_home_before_binding_and_bind_parents_before_children() {
        let f = setup();
        let p = plan_in(&f.home, "wexus", &f.hd.join("customers/wexus"), &f.hd, "").unwrap();
        let a = bwrap_args(&p);
        let pos = |flag: &str, path: &str| a.windows(2).position(|w| w[0] == flag && w[1] == path);
        let hd = f.hd.to_string_lossy().into_owned();
        let own = super::super::capabilities::normalize(&f.hd.join("customers/wexus")).to_string_lossy().into_owned();
        let child = super::super::capabilities::normalize(&f.hd.join("customers/wexus/extra")).to_string_lossy().into_owned();
        let hide = pos("--tmpfs", &hd).expect("home emptied");
        let bind = pos("--bind", &own).expect("own folder bound");
        let bind_child = pos("--bind", &child).expect("same-group folder bound");
        assert!(hide < bind && bind < bind_child, "{a:?}");
        assert!(a.iter().any(|x| x == "--unshare-pid"));
        assert_eq!(a.last().map(String::as_str), Some(own.as_str()), "chdir to the work folder");
    }

    #[test]
    fn report_wraps_nothing_enforce_wraps_and_a_missing_runner_refuses() {
        let f = setup();
        let wd = f.hd.join("customers/wexus");
        let pl = || plan_in(&f.home, "wexus", &wd, &f.hd, "");
        let (r, o, _) = wrap_with(&f.home, "wexus", &wd, "claude", Mode::Report, &Runner::Bwrap, pl);
        assert_eq!((r, o), (Ok(None), Outcome::Reported { runner: "bwrap".into() }));
        let (r, o, _) = wrap_with(&f.home, "wexus", &wd, "claude --x", Mode::Enforce, &Runner::Bwrap, pl);
        let cmd = r.unwrap().unwrap();
        assert!(cmd.starts_with("bwrap ") && cmd.ends_with("/bin/sh -c 'claude --x'"), "{cmd}");
        assert_eq!(o, Outcome::Wrapped { runner: "bwrap".into() });
        let (r, o, _) = wrap_with(&f.home, "wexus", &wd, "claude", Mode::Enforce,
            &Runner::Unavailable("bwrap is not installed".into()), pl);
        assert!(r.unwrap_err().contains("refusing to start wexus"));
        assert!(matches!(o, Outcome::Refused(_)));
    }

    #[test]
    fn a_worker_outside_any_boundary_is_never_wrapped_even_when_no_runner_exists() {
        let f = setup();
        let wd = f.hd.join("tools");
        let (r, o, _) = wrap_with(&f.home, "helper", &wd, "claude", Mode::Enforce,
            &Runner::Unavailable("none".into()), || plan_in(&f.home, "helper", &wd, &f.hd, ""));
        assert_eq!((r, o), (Ok(None), Outcome::NotBoundary));
        let (r, o, _) = wrap_with(&f.home, "wexus", &wd, "claude", Mode::Off, &Runner::Bwrap, || unreachable!());
        assert_eq!((r, o), (Ok(None), Outcome::Off));
    }

    #[test]
    fn the_runner_is_measured_never_assumed() {
        let yes = |_: &str| true;
        assert_eq!(runner_with("linux", "", "", &yes, &|| true, &|| false), Runner::Bwrap);
        assert!(matches!(runner_with("linux", "", "", &yes, &|| false, &|| false), Runner::Unavailable(_)));
        assert!(matches!(runner_with("macos", "", "", &yes, &|| true, &|| true), Runner::Unavailable(_)));
        assert!(matches!(runner_with("linux", "docker", "", &yes, &|| true, &|| true), Runner::Unavailable(_)));
        assert!(matches!(runner_with("linux", "docker", "img", &yes, &|| true, &|| false), Runner::Unavailable(_)));
        assert_eq!(runner_with("macos", "docker", "img", &yes, &|| false, &|| true), Runner::Docker { image: "img".into() });
    }
}
