//! Each worker's agent CLI runs inside its OWN sandbox, configured from the
//! worker's capabilities (governance PR 16; plan section 4.3).
//!
//! amux does not build a sandbox (A2: the CLI ships one). It writes the CLI's
//! sandbox settings from the same folder facts `capabilities.rs` decides with
//! (A10), so the sandbox and the `/api/fs` boundary cannot disagree:
//!
//! | Rule | Source |
//! |---|---|
//! | write: the user's home and temp dirs | broad by default (A2): a worker keeps full reach outside other workers' folders |
//! | deny write: another worker's folder | `capabilities::worker_folders`, minus folders that contain this worker's own and `AMUX_CAP_FOLDERS` grants |
//! | deny read + write: amux's own secrets | the owner token, `server.env`, connector grants, Gmail tokens, the vault, minted tokens, other workers' and other groups' env files |
//! | deny read: other workers' folders | only when the owner sets `AMUX_CAP_READ_FOLDERS=own` |
//! | network | unrestricted (`*`) unless the owner sets `AMUX_CAP_NETWORK=<domains>` |
//!
//! The settings ride `claude --settings <file>`, a layer of its own: the
//! user's `~/.claude/settings.json` and the project's settings files are never
//! written. Claude's sandbox covers its Bash tool; its Read and Edit tools are
//! held to the same paths by `permissions.deny` rules in the same file.
//!
//! MEASURED 2026-11-10 on the sandbox host (Claude Code 2.1.322): the sandbox
//! needs `bwrap` AND `socat` on Linux. With socat missing, Claude printed a
//! warning and ran every command WITHOUT the sandbox. So the runtime check
//! below is what `status` reports, and enforce mode sets
//! `sandbox.failIfUnavailable`, so a host without the runtime refuses to run
//! a shell rather than claiming a boundary it does not have.
//!
//! Codex: `codex sandbox -c sandbox_mode=workspace-write` (codex-cli 0.167.0)
//! did not stop a write outside its writable root in the same measurement, so
//! Codex lanes are reported `unsupported`, never claimed.
//!
//! `AMUX_CLI_SANDBOX` (A11): `report` (default) writes the plan with the
//! sandbox disabled and does not pass it to the CLI; `enforce` enables it and
//! passes it; `off` writes nothing. Isolated workers get it the same way: it is
//! configuration, a boundary like `apply_to_isolated`, not a prompt (A9).

use axum::{
    extract::{Path as AxPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MODE_ENV: &str = "AMUX_CLI_SANDBOX";
pub const NETWORK_KEY: &str = "AMUX_CAP_NETWORK";
pub const READ_KEY: &str = "AMUX_CAP_READ_FOLDERS";

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

// ── Runtime ─────────────────────────────────────────────────────────────────

/// Whether the CLI's sandbox can actually run on this host, and what is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    pub available: bool,
    pub missing: Vec<String>,
    pub platform: &'static str,
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

pub fn runtime() -> Runtime {
    runtime_with(std::env::consts::OS, on_path)
}

pub fn runtime_with(os: &'static str, has: impl Fn(&str) -> bool) -> Runtime {
    match os {
        // Seatbelt ships with macOS.
        "macos" => Runtime { available: true, missing: vec![], platform: "macos" },
        "linux" => {
            let missing: Vec<String> =
                ["bwrap", "socat"].iter().filter(|b| !has(b)).map(|b| b.to_string()).collect();
            Runtime { available: missing.is_empty(), missing, platform: "linux" }
        }
        other => Runtime { available: false, missing: vec![format!("no sandbox backend on {other}")], platform: "other" },
    }
}

// ── Plan ────────────────────────────────────────────────────────────────────

/// One rule and the capability fact it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub rule: &'static str,
    pub path: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub entries: Vec<Entry>,
    pub network: Vec<String>,
    pub network_source: String,
}

fn user_home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn split_list(v: &str) -> Vec<String> {
    v.split([',', ';', '\n', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn expand(home_dir: &Path, raw: &str) -> PathBuf {
    let raw = raw.trim();
    let p = if raw == "~" {
        home_dir.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir.join(rest)
    } else {
        PathBuf::from(raw)
    };
    super::capabilities::normalize(&p)
}

/// amux's own files a worker's shell must never read or write: the owner
/// token and every stored credential. Other workers' env files and the env
/// files of groups this worker is not in are listed too; its own are already
/// in its environment.
pub fn secret_paths(home: &Path, lane: &str) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for f in ["auth_token", "server.env", "front-door-owner", "connectors", "gmail-tokens",
              "worker-tokens", "app-tokens", "ledger", "email-approvals"] {
        out.push((home.join(f), format!("amux secret ~/.amux/{f}")));
    }
    if let Ok(rd) = std::fs::read_dir(home) {
        let mut vault: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("vault")))
            .collect();
        vault.sort();
        for p in vault {
            let Some(n) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
            out.push((p, format!("amux secret ~/.amux/{n}")));
        }
    }
    if let Ok(rd) = std::fs::read_dir(home.join("sessions")) {
        let mut others: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("env"))
            .filter(|p| p.file_stem().and_then(|s| s.to_str()) != Some(lane))
            .collect();
        others.sort();
        for p in others {
            let Some(w) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
            out.push((p, format!("another worker's env file ({w})")));
        }
    }
    let mine: std::collections::BTreeSet<String> =
        super::scope::session_tags_of(home, lane).into_iter().collect();
    if let Ok(rd) = std::fs::read_dir(home.join("env")) {
        let mut groups: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("env"))
            .filter(|p| p.file_stem().and_then(|s| s.to_str()).is_some_and(|g| !mine.contains(g)))
            .collect();
        groups.sort();
        for p in groups {
            let Some(g) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
            out.push((p, format!("env file of group {g}, which this worker is not in")));
        }
    }
    out
}

/// The sandbox rules for `lane`, each with the capability fact behind it.
pub fn plan(home: &Path, lane: &str, work_dir: &Path) -> Plan {
    plan_in(home, lane, work_dir, &user_home())
}

pub fn plan_in(home: &Path, lane: &str, work_dir: &Path, hd: &Path) -> Plan {
    let mut entries = Vec::new();
    let own = super::capabilities::normalize(work_dir);
    let push = |entries: &mut Vec<Entry>, rule: &'static str, p: &Path, source: String| {
        entries.push(Entry { rule, path: p.to_string_lossy().into_owned(), source });
    };

    // Writes: broad by default.
    push(&mut entries, "allowWrite", &own, "the worker's own folder".into());
    push(&mut entries, "allowWrite", hd, "the user's home: broad by default (A2)".into());
    for t in ["/tmp", "/var/tmp"] {
        push(&mut entries, "allowWrite", Path::new(t), "temporary files".into());
    }

    // Grants: AMUX_CAP_FOLDERS, as capabilities.rs reads them.
    let mut granted: Vec<PathBuf> = Vec::new();
    let mut grant_all: Option<String> = None;
    for l in super::capabilities::layers(home, lane, super::capabilities::FOLDERS_KEY) {
        for g in split_list(&l.value) {
            if g == "*" {
                grant_all.get_or_insert_with(|| l.layer.clone());
                continue;
            }
            let p = expand(hd, &g);
            push(&mut entries, "allowWrite", &p, format!("{} {g} ({})", super::capabilities::FOLDERS_KEY, l.layer));
            granted.push(p);
        }
    }

    // Other workers' folders, as capabilities::folder_decision draws them.
    let read_own_only = super::capabilities::layers(home, lane, READ_KEY)
        .first()
        .map(|l| (l.value.trim().eq_ignore_ascii_case("own"), l.layer.clone()));
    if grant_all.is_none() {
        for (w, dir) in super::capabilities::worker_folders(home) {
            if w == lane || dir == own || own.starts_with(&dir) || granted.iter().any(|g| dir.starts_with(g)) {
                continue;
            }
            push(&mut entries, "denyWrite", &dir, format!("{w}'s folder: another worker's (capabilities boundary)"));
            if let Some((true, layer)) = &read_own_only {
                push(&mut entries, "denyRead", &dir, format!("{READ_KEY}=own ({layer})"));
            }
        }
    }

    for (p, why) in secret_paths(home, lane) {
        push(&mut entries, "denyRead", &p, why.clone());
        push(&mut entries, "denyWrite", &p, why);
    }

    let (network, network_source) = match super::capabilities::layers(home, lane, NETWORK_KEY).first() {
        Some(l) => (split_list(&l.value), format!("{NETWORK_KEY} ({})", l.layer)),
        None => (vec!["*".to_string()], "unrestricted by default (A2)".to_string()),
    };
    Plan { entries, network, network_source }
}

fn paths(plan: &Plan, rule: &str) -> Vec<String> {
    let mut v: Vec<String> = plan.entries.iter().filter(|e| e.rule == rule).map(|e| e.path.clone()).collect();
    v.dedup();
    v
}

/// The Claude Code settings layer for `plan`. `enabled` is false in report
/// mode, so the file shows what WOULD be enforced and changes nothing.
pub fn claude_settings(plan: &Plan, enabled: bool) -> Value {
    let mut deny: Vec<String> = Vec::new();
    for e in &plan.entries {
        match e.rule {
            "denyRead" => {
                deny.push(format!("Read(/{}/**)", e.path));
                deny.push(format!("Read(/{})", e.path));
            }
            "denyWrite" => {
                deny.push(format!("Edit(/{}/**)", e.path));
                deny.push(format!("Edit(/{})", e.path));
            }
            _ => {}
        }
    }
    deny.sort();
    deny.dedup();
    let mut v = json!({
        "sandbox": {
            "enabled": enabled,
            "failIfUnavailable": enabled,
            "filesystem": {
                "allowWrite": paths(plan, "allowWrite"),
                "denyWrite": paths(plan, "denyWrite"),
                "denyRead": paths(plan, "denyRead"),
            },
            "network": { "allowedDomains": plan.network },
        },
    });
    if enabled {
        v["permissions"] = json!({ "deny": deny });
    }
    v
}

pub fn settings_path(home: &Path, lane: &str) -> PathBuf {
    home.join("sandbox").join(format!("{lane}.claude-settings.json"))
}

// ── Launch ──────────────────────────────────────────────────────────────────

/// What a launch did, kept for the view and the counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Settings written and passed to the CLI.
    Enforced { runtime_available: bool },
    /// Settings written with the sandbox disabled, not passed.
    Reported,
    /// The provider has no sandbox amux can configure honestly.
    Unsupported(String),
    Off,
    Failed(String),
}

/// Write the worker's settings layer and return the flags to append to the
/// CLI command (empty unless enforcing a Claude lane).
pub fn launch_opts(home: &Path, lane: &str, provider: &str, work_dir: &Path) -> String {
    let (opts, outcome) = launch_opts_with(home, lane, provider, work_dir, mode(), &runtime());
    record(home, lane, &outcome);
    opts
}

pub fn launch_opts_with(
    home: &Path,
    lane: &str,
    provider: &str,
    work_dir: &Path,
    mode: Mode,
    rt: &Runtime,
) -> (String, Outcome) {
    if mode == Mode::Off {
        return (String::new(), Outcome::Off);
    }
    if provider != "claude" {
        let why = if provider == "codex" {
            "codex-cli 0.167.0's workspace-write sandbox did not confine writes when measured".to_string()
        } else {
            format!("no sandbox settings amux can write for {provider}")
        };
        return (String::new(), Outcome::Unsupported(why));
    }
    let p = plan(home, lane, work_dir);
    let enforce = mode == Mode::Enforce;
    let body = claude_settings(&p, enforce);
    let path = settings_path(home, lane);
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return (String::new(), Outcome::Failed(e.to_string()));
        }
    }
    if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap_or_default()) {
        return (String::new(), Outcome::Failed(e.to_string()));
    }
    if !enforce {
        return (String::new(), Outcome::Reported);
    }
    let opts = format!(" --settings {}", super::session_verbs::sh_quote(&path.to_string_lossy()));
    (opts, Outcome::Enforced { runtime_available: rt.available })
}

fn outcome_str(o: &Outcome) -> (&'static str, String) {
    match o {
        Outcome::Enforced { runtime_available: true } => ("enforced", String::new()),
        Outcome::Enforced { runtime_available: false } => (
            "enforced_runtime_unavailable",
            "the CLI refuses to run its shell here (failIfUnavailable) until the runtime is installed".into(),
        ),
        Outcome::Reported => ("reported", "settings written with the sandbox disabled; not passed".into()),
        Outcome::Unsupported(w) => ("unsupported", w.clone()),
        Outcome::Off => ("off", String::new()),
        Outcome::Failed(e) => ("failed", e.clone()),
    }
}

fn state_path(home: &Path) -> PathBuf {
    home.join("sandbox").join("launches.json")
}

fn record(home: &Path, lane: &str, o: &Outcome) {
    let (verdict, detail) = outcome_str(o);
    counters::bump(verdict);
    tracing::info!(session = lane, verdict = %format!("cli_sandbox_{verdict}"), %detail,
        measured = true, n_considered = 1, "cli sandbox at launch (governance PR 16)");
    let p = state_path(home);
    let mut all: BTreeMap<String, Value> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    all.insert(lane.to_string(), json!({ "status": verdict, "detail": detail, "at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) }));
    if let Some(d) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(d) {
            tracing::warn!(error = %e, verdict = "cli_sandbox_state_unsaved", "the sandbox launch record folder could not be created");
        }
    }
    if let Err(e) = std::fs::write(&p, serde_json::to_vec_pretty(&all).unwrap_or_default()) {
        tracing::warn!(error = %e, verdict = "cli_sandbox_state_unsaved", "the sandbox launch record was not saved");
    }
}

fn last_launch(home: &Path, lane: &str) -> Option<Value> {
    std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get(lane).cloned())
}

// ── View and counters ───────────────────────────────────────────────────────

pub fn view_for(home: &Path, lane: &str, work_dir: &Path) -> Value {
    let p = plan(home, lane, work_dir);
    let rt = runtime();
    let m = mode();
    let status = match (m, rt.available) {
        (Mode::Off, _) => "off",
        (Mode::Report, _) => "report_only",
        (Mode::Enforce, true) => "enforced",
        (Mode::Enforce, false) => "unavailable",
    };
    json!({
        "worker": lane,
        "mode": m.as_str(),
        "status": status,
        "runtime": { "available": rt.available, "missing": rt.missing, "platform": rt.platform, "measured": true },
        "entries": p.entries.iter().map(|e| json!({"rule": e.rule, "path": e.path, "source": e.source})).collect::<Vec<_>>(),
        "network": { "allowed_domains": p.network, "source": p.network_source },
        "settings_file": settings_path(home, lane),
        "last_launch": last_launch(home, lane),
        "covers": "Claude's Bash tool through its sandbox; its Read and Edit tools through permissions.deny in the same file",
        "measured": true,
        "n_considered": p.entries.len(),
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
            Json(json!({"error": "a worker may read only its own sandbox", "code": "sandbox_view_refused"})),
        )
            .into_response();
    }
    let env = home.join("sessions").join(format!("{name}.env"));
    if !env.exists() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("no worker named {name}")}))).into_response();
    }
    let dir = super::session_verbs::EnvFile::load(&env).get("CC_DIR").map(|d| expand(&user_home(), d)).unwrap_or_else(user_home);
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

/// The A5 diagnostic, served in `GET /api/contract/counters` as `cli_sandbox`.
pub fn report() -> Value {
    let c = counters::snapshot();
    let rt = runtime();
    json!({
        "mode": mode().as_str(),
        "runtime": { "available": rt.available, "missing": rt.missing, "platform": rt.platform },
        "measured": true,
        "n_considered": c.values().sum::<u64>(),
        "counts": c,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("amux");
        let hd = t.path().join("user");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::create_dir_all(home.join("env")).unwrap();
        for (w, d, tags) in [("wexus", "customers/wexus", "customers"), ("rothco", "customers/rothco", "customers"),
                             ("helper", "", "amux")] {
            let dir = hd.join(d);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(home.join("sessions").join(format!("{w}.env")),
                format!("CC_DIR={}\nCC_TAGS={tags}\n", dir.display())).unwrap();
        }
        std::fs::write(home.join("env").join("customers.env"), "X=1\n").unwrap();
        std::fs::write(home.join("env").join("ops.env"), "Y=1\n").unwrap();
        std::fs::write(home.join("auth_token"), "t").unwrap();
        std::fs::write(home.join("vault-secrets.json"), "{}").unwrap();
        (t, home, hd)
    }

    fn has(p: &Plan, rule: &str, needle: &str) -> bool {
        p.entries.iter().any(|e| e.rule == rule && e.path.ends_with(needle))
    }

    #[test]
    fn another_customers_folder_is_denied_and_the_own_folder_is_writable() {
        let (_t, home, hd) = setup();
        let p = plan_in(&home, "wexus", &hd.join("customers/wexus"), &hd);
        assert!(has(&p, "allowWrite", "customers/wexus"), "{p:?}");
        assert!(has(&p, "denyWrite", "customers/rothco"), "{p:?}");
        // The home-folder worker's folder CONTAINS wexus's: shared ground, not denied.
        assert!(!p.entries.iter().any(|e| e.rule == "denyWrite" && Path::new(&e.path) == hd), "{p:?}");
        // Reads stay broad by default.
        assert!(!has(&p, "denyRead", "customers/rothco"), "{p:?}");
        assert_eq!(p.network, vec!["*".to_string()]);
    }

    #[test]
    fn secrets_and_other_workers_env_files_are_denied_but_its_own_is_not() {
        let (_t, home, hd) = setup();
        let p = plan_in(&home, "wexus", &hd.join("customers/wexus"), &hd);
        for s in ["auth_token", "server.env", "connectors", "gmail-tokens", "vault-secrets.json", "sessions/rothco.env", "env/ops.env"] {
            assert!(has(&p, "denyRead", s), "{s}: {p:?}");
        }
        assert!(!has(&p, "denyRead", "sessions/wexus.env"), "{p:?}");
        assert!(!has(&p, "denyRead", "env/customers.env"), "{p:?}");
    }

    #[test]
    fn a_folder_grant_lifts_the_deny_and_the_owner_can_narrow_reads_and_network() {
        let (_t, home, hd) = setup();
        std::fs::write(home.join("sessions/wexus.env"),
            format!("CC_DIR={}\nCC_TAGS=customers\nAMUX_CAP_FOLDERS={}\nAMUX_CAP_NETWORK=api.github.com,*.amux.io\n",
                hd.join("customers/wexus").display(), hd.join("customers/rothco").display())).unwrap();
        std::fs::write(home.join("env/customers.env"), "AMUX_CAP_READ_FOLDERS=own\n").unwrap();
        let p = plan_in(&home, "wexus", &hd.join("customers/wexus"), &hd);
        assert!(!has(&p, "denyWrite", "customers/rothco"), "{p:?}");
        assert!(has(&p, "allowWrite", "customers/rothco"), "{p:?}");
        assert_eq!(p.network, vec!["api.github.com".to_string(), "*.amux.io".to_string()]);
        assert!(p.network_source.contains("worker wexus"), "{}", p.network_source);
        // rothco is granted, so nothing else is a boundary; add one to see the read deny.
        std::fs::create_dir_all(hd.join("customers/wtso")).unwrap();
        std::fs::write(home.join("sessions/wtso.env"), format!("CC_DIR={}\n", hd.join("customers/wtso").display())).unwrap();
        let p = plan_in(&home, "wexus", &hd.join("customers/wexus"), &hd);
        assert!(has(&p, "denyRead", "customers/wtso"), "{p:?}");
    }

    #[test]
    fn report_mode_writes_a_disabled_layer_and_passes_nothing() {
        let (_t, home, hd) = setup();
        let rt = Runtime { available: true, missing: vec![], platform: "linux" };
        let (opts, o) = launch_opts_with(&home, "wexus", "claude", &hd.join("customers/wexus"), Mode::Report, &rt);
        assert_eq!((opts.as_str(), &o), ("", &Outcome::Reported));
        let v: Value = serde_json::from_slice(&std::fs::read(settings_path(&home, "wexus")).unwrap()).unwrap();
        assert_eq!(v["sandbox"]["enabled"], false);
        assert!(v.get("permissions").is_none(), "{v}");
    }

    #[test]
    fn enforce_mode_passes_the_layer_and_fails_closed_without_a_runtime() {
        let (_t, home, hd) = setup();
        let rt = runtime_with("linux", |b| b == "bwrap");
        assert_eq!(rt.missing, vec!["socat".to_string()]);
        let (opts, o) = launch_opts_with(&home, "wexus", "claude", &hd.join("customers/wexus"), Mode::Enforce, &rt);
        assert!(opts.contains("--settings") && opts.contains("wexus.claude-settings.json"), "{opts}");
        assert_eq!(o, Outcome::Enforced { runtime_available: false });
        let v: Value = serde_json::from_slice(&std::fs::read(settings_path(&home, "wexus")).unwrap()).unwrap();
        assert_eq!(v["sandbox"]["enabled"], true);
        assert_eq!(v["sandbox"]["failIfUnavailable"], true);
        let deny = v["permissions"]["deny"].as_array().unwrap();
        assert!(deny.iter().any(|d| d.as_str().unwrap().contains("auth_token")), "{v}");
        assert!(deny.iter().any(|d| d.as_str().unwrap().starts_with("Edit(/") && d.as_str().unwrap().contains("customers/rothco")), "{v}");
    }

    #[test]
    fn codex_and_off_never_claim_a_sandbox() {
        let (_t, home, hd) = setup();
        let rt = Runtime { available: true, missing: vec![], platform: "linux" };
        let (opts, o) = launch_opts_with(&home, "wexus", "codex", &hd, Mode::Enforce, &rt);
        assert!(opts.is_empty() && matches!(o, Outcome::Unsupported(_)), "{o:?}");
        let (opts, o) = launch_opts_with(&home, "wexus", "claude", &hd, Mode::Off, &rt);
        assert!(opts.is_empty() && o == Outcome::Off);
        assert!(!settings_path(&home, "wexus").exists());
    }
}
