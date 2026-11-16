//! A published app is its own principal, with no amux access by default
//! (governance PR 5).
//!
//! WHAT THIS REPLACES. A published app (`amux publish PORT`, served at
//! `/pub/<id>/` or inside amux on the apps origin) is a process a worker
//! started on this machine. The relay strips amux cookies and the bearer from
//! what VISITORS send it (app_relay.rs), but the app process itself could call
//! `http://127.0.0.1:<amux>/api/...` with no credential, passing as "local"
//! (governance PR 3 now counts that), or with the worker token it inherited
//! from the worker's shell, passing as that worker. A public web page could so
//! reach the owner's whole fleet through the app behind it.
//!
//! WHAT AN APP IS NOW. Each port-backed link gets an app credential at publish
//! time, returned once and stored only as its sha256 (`app_token_sha256` in
//! public-links.json). The app sends it as `X-Amux-App-Token`. A request that
//! carries a valid one is attributed `app:<id>`, on behalf of whoever
//! published it, and may make only the calls a PERSON allowed (`api_allow`):
//! none by default. A worker can ask for access when it publishes
//! (`api_requested`); approving the link approves exactly that list, and the
//! Apps tab shows it before the owner decides. An app's token never proves a
//! worker or the owner: the worker headers it sends are dropped.
//!
//! WHERE AN APP'S UNCREDENTIALED CALL IS CAUGHT. On Linux the server maps a
//! loopback caller's socket to its process (`/proc/net/tcp*`, `/proc/<pid>/fd`)
//! and knows a call came from a published app's process tree (the process
//! listening on the app's port and its children), even when it presents
//! nothing or presents the worker token it inherited. Elsewhere the report says
//! `origin_detection.measured: false` rather than reading as "no app called".
//!
//! WHAT A FAILED CHECK COSTS is `AMUX_PUBLISHED_APP_API` (A11):
//! - `report` (default): allowed as before, counted per app in
//!   `GET /api/contract/counters` under `published_app`, logged once per app
//!   and kind, `published_app_api_would_be_refused`;
//! - `enforce`: refused 403 `published_app_api_refused` with what to do;
//! - `off`: app tokens ignored, nothing detected or counted.
//!
//! STILL OPEN, and said in the report: an app process runs as the owner's
//! unix user, so it can read `~/.amux/auth_token` and present the OWNER token.
//! Nothing here can tell that from the owner. Governance PRs 16 and 17 (each
//! worker and its apps in the agent CLI's sandbox, each customer in its own
//! container) close it; this one does not.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub use super::worker_identity::Mode;

pub const MODE_ENV: &str = "AMUX_PUBLISHED_APP_API";
pub const TOKEN_HEADER: &str = "x-amux-app-token";
/// `app:<id>`, stamped by [`resolve`] for a valid token. Stripped from inbound
/// requests, so only the server produces it.
pub(crate) const ACTOR_HEADER: &str = "x-amux-internal-app-actor";
/// Who published the app: the principal it acts on behalf of.
pub(crate) const ON_BEHALF_HEADER: &str = "x-amux-internal-app-on-behalf";
const INVALID: &str = "app:?invalid";

pub fn mode() -> Mode {
    #[cfg(test)]
    if let Some(m) = test_mode::MODE.with(|c| c.get()) {
        return m;
    }
    Mode::parse(std::env::var(MODE_ENV).ok().as_deref())
}

#[cfg(test)]
pub(crate) mod test_mode {
    use super::Mode;
    use std::cell::Cell;
    thread_local! {
        pub static MODE: Cell<Option<Mode>> = const { Cell::new(None) };
    }
    pub fn set(m: Option<Mode>) {
        MODE.with(|c| c.set(m));
    }
}

// ── The credential ──────────────────────────────────────────────────────────

pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// A fresh app token and its hash. `None` without secure randomness: the
/// caller then makes no link rather than one with a guessable credential.
pub fn mint() -> Option<(String, String)> {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .ok()?;
    let token = format!("app_{}", hex::encode(b));
    let sha = sha256_hex(&token);
    Some((token, sha))
}

// ── The allowlist ───────────────────────────────────────────────────────────

/// Paths no allowlist can open to an app, whoever approves it: an app must not
/// change its own access, the members, sign-in, or mint credentials.
const NEVER: &[&str] = &["/api/page-links", "/api/org", "/api/auth", "/api/connectors", "/api/vault", "/api/grants", "/api/scope"];

/// Normalise and validate one allowlist entry: `METHOD /api/path`, where
/// METHOD is GET, POST, PUT, PATCH, DELETE or `*`, a `*` segment matches one
/// path segment and a trailing `/**` matches the rest.
pub fn parse_rule(raw: &str) -> Result<String, String> {
    let mut it = raw.split_whitespace();
    let (Some(m), Some(p), None) = (it.next(), it.next(), it.next()) else {
        return Err(format!("{raw:?}: write it as \"METHOD /api/path\", e.g. \"POST /api/sessions/my-worker/send\""));
    };
    let m = m.to_ascii_uppercase();
    if !matches!(m.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "*") {
        return Err(format!("{raw:?}: the method is GET, POST, PUT, PATCH, DELETE or *"));
    }
    if !p.starts_with("/api/") || p.contains("..") || p.contains('?') {
        return Err(format!("{raw:?}: the path starts with /api/ and has no .. or query"));
    }
    if NEVER.iter().any(|n| p == *n || p.starts_with(&format!("{n}/")) || n.starts_with(p.trim_end_matches("/**")) && p.ends_with("/**")) {
        return Err(format!("{raw:?}: an app can never be given {}", NEVER.join(", ")));
    }
    Ok(format!("{m} {p}"))
}

pub fn parse_rules(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for r in raw {
        let r = parse_rule(r)?;
        if !out.contains(&r) {
            out.push(r);
        }
    }
    if out.len() > 20 {
        return Err("at most 20 rules".into());
    }
    Ok(out)
}

/// Whether `method path` is allowed by `rules`.
pub fn allows(rules: &[String], method: &str, path: &str) -> bool {
    if NEVER.iter().any(|n| path == *n || path.starts_with(&format!("{n}/"))) {
        return false;
    }
    let want: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    rules.iter().any(|r| {
        let Some((m, p)) = r.split_once(' ') else { return false };
        if m != "*" && !m.eq_ignore_ascii_case(method) {
            return false;
        }
        let pat: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let mut i = 0;
        for (k, seg) in pat.iter().enumerate() {
            if *seg == "**" && k == pat.len() - 1 {
                return true;
            }
            match want.get(i) {
                Some(w) if *seg == "*" || seg == w => i += 1,
                _ => return false,
            }
        }
        i == want.len()
    })
}

// ── Resolving the token (outermost middleware) ──────────────────────────────

/// A published app, as the request path needs it.
#[derive(Clone, Debug)]
pub struct AppLink {
    pub id: String,
    pub created_by: String,
    pub allow: Vec<String>,
}

fn app_links() -> Vec<(String, AppLink)> {
    super::static_files::public_links::load(&crate::config::amux_home())
        .into_iter()
        .filter(|l| l.port > 0 && !l.app_token_sha256.is_empty())
        .map(|l| (l.app_token_sha256.clone(), AppLink { id: l.id, created_by: l.created_by, allow: l.api_allow }))
        .collect()
}

/// Strip every inbound app marker and, if the request carries an app token,
/// stamp who it is. Called from `org::local_member_identity`, before the
/// request logger and auth, so both see `app:<id>` as the caller. The token
/// itself is taken off the request: no handler ever sees it.
pub fn resolve(headers: &mut HeaderMap) {
    headers.remove(ACTOR_HEADER);
    headers.remove(ON_BEHALF_HEADER);
    let Some(v) = headers.remove(TOKEN_HEADER) else { return };
    if mode() == Mode::Off {
        return;
    }
    let presented = v.to_str().unwrap_or("").trim().to_string();
    let sha = sha256_hex(&presented);
    let found = (!presented.is_empty())
        .then(|| app_links().into_iter().find(|(h, _)| super::auth::constant_time_eq(h.as_bytes(), sha.as_bytes())))
        .flatten();
    // An app is never a worker, whatever it inherited from the worker's shell.
    for h in ["x-amux-session", "x-amux-worker", super::worker_identity::TOKEN_HEADER] {
        headers.remove(h);
    }
    match found {
        Some((_, app)) => {
            headers.insert(ACTOR_HEADER, HeaderValue::from_str(&format!("app:{}", app.id)).unwrap_or(HeaderValue::from_static(INVALID)));
            if let Ok(v) = HeaderValue::from_str(&app.created_by) {
                headers.insert(ON_BEHALF_HEADER, v);
            }
        }
        None => {
            headers.insert(ACTOR_HEADER, HeaderValue::from_static(INVALID));
        }
    }
}

/// `app:<id>` when [`resolve`] proved an app on this request.
pub fn actor(headers: &HeaderMap) -> Option<&str> {
    headers.get(ACTOR_HEADER).and_then(|v| v.to_str().ok()).filter(|a| *a != INVALID)
}

// ── Counters ────────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy)]
struct Count {
    allowed: u64,
    outside_allowlist: u64,
    uncredentialed: u64,
    inherited_worker_token: u64,
    owner_token: u64,
    refused: u64,
}

#[derive(Default)]
struct Counters {
    considered: u64,
    invalid_token: u64,
    origin_lookups: u64,
    origin_unmeasured: u64,
    by_app: BTreeMap<String, Count>,
    logged: HashSet<String>,
}

fn counters() -> &'static Mutex<Counters> {
    static C: OnceLock<Mutex<Counters>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn bump(app: &str, f: impl FnOnce(&mut Count)) {
    if let Ok(mut c) = counters().lock() {
        c.considered += 1;
        // Bounded: one row per published app; past 200 they share `other`.
        let key = if c.by_app.contains_key(app) || c.by_app.len() < 200 { app.to_string() } else { "other".into() };
        f(c.by_app.entry(key).or_default());
    }
}

fn first_time(key: String) -> bool {
    counters().lock().map(|mut c| c.logged.insert(key)).unwrap_or(false)
}

// ── The decisions ───────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Not an app request (or `off`): the rest of auth decides.
    NotApp,
    /// An app's call on its allowlist: admitted, attributed `app:<id>`.
    Admit,
    /// An app's call without access, counted in `report`: the rest of auth
    /// decides as if no app were involved (so loopback rules still hold).
    Continue,
    /// An app's call, refused.
    Refuse(&'static str),
}

/// What a request from an app's process presented instead of its app token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presented {
    Nothing,
    /// The worker token it inherited from the worker's shell.
    WorkerToken,
    /// The owner token, read from disk: the app is not the owner.
    OwnerToken,
}

/// Decide for a request [`resolve`] stamped with an app token.
pub fn gate_token(headers: &HeaderMap, method: &str, path: &str) -> Gate {
    gate_token_in(mode(), headers, method, path, &app_links().into_iter().map(|(_, a)| a).collect::<Vec<_>>())
}

pub fn gate_token_in(mode: Mode, headers: &HeaderMap, method: &str, path: &str, apps: &[AppLink]) -> Gate {
    let Some(stamp) = headers.get(ACTOR_HEADER).and_then(|v| v.to_str().ok()) else { return Gate::NotApp };
    if mode == Mode::Off {
        return Gate::NotApp;
    }
    if stamp == INVALID {
        if let Ok(mut c) = counters().lock() {
            c.considered += 1;
            c.invalid_token += 1;
        }
        if first_time("invalid".into()) {
            tracing::warn!(target: "amux::auth", method, path, measured = true, n_considered = 1,
                verdict = "published_app_token_invalid", "a request carried an app token that matches no published app");
        }
        return if mode == Mode::Enforce { Gate::Refuse("app_token_invalid") } else { Gate::Continue };
    }
    let id = stamp.trim_start_matches("app:");
    let allow = apps.iter().find(|a| a.id == id).map(|a| a.allow.clone()).unwrap_or_default();
    if allows(&allow, method, path) {
        bump(id, |c| c.allowed += 1);
        return Gate::Admit;
    }
    bump(id, |c| c.outside_allowlist += 1);
    decide(mode, id, "outside_allowlist", method, path)
}

/// Decide for a loopback request from app `id`'s process tree that did not
/// present its app token.
pub fn gate_origin(id: &str, presented: Presented, method: &str, path: &str) -> Gate {
    gate_origin_in(mode(), id, presented, method, path)
}

pub fn gate_origin_in(mode: Mode, id: &str, presented: Presented, method: &str, path: &str) -> Gate {
    if mode == Mode::Off {
        return Gate::NotApp;
    }
    let kind = match presented {
        Presented::Nothing => {
            bump(id, |c| c.uncredentialed += 1);
            "uncredentialed"
        }
        Presented::WorkerToken => {
            bump(id, |c| c.inherited_worker_token += 1);
            "inherited_worker_token"
        }
        Presented::OwnerToken => {
            bump(id, |c| c.owner_token += 1);
            "owner_token_from_app"
        }
    };
    decide(mode, id, kind, method, path)
}

fn decide(mode: Mode, id: &str, kind: &'static str, method: &str, path: &str) -> Gate {
    let new = first_time(format!("{id} {kind}"));
    if mode == Mode::Enforce {
        bump(id, |c| c.refused += 1);
        if new {
            tracing::warn!(target: "amux::auth", app = id, kind, method, path, measured = true, n_considered = 1,
                verdict = "published_app_api_refused", "a published app called the amux API without access; refused");
        }
        return Gate::Refuse(kind);
    }
    if new {
        tracing::warn!(target: "amux::auth", app = id, kind, method, path, measured = true, n_considered = 1,
            verdict = "published_app_api_would_be_refused",
            "a published app called the amux API without access; allowed in report mode (AMUX_PUBLISHED_APP_API=enforce refuses it)");
    }
    Gate::Continue
}

pub fn refused(kind: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "forbidden",
            "reason": "published_app_api_refused",
            "kind": kind,
            "why": "a published app has no amux access unless a person gave it some: anyone on the web can reach the app",
            "how_to_fix": "send the app's own credential (X-Amux-App-Token, from AMUX_APP_TOKEN) and ask the owner to allow the calls it needs in the Apps tab (`amux publish PORT --api \"POST /api/...\"` asks when publishing)",
            "turn_off": format!("{MODE_ENV}=report (or off) in server.env, then restart"),
        })),
    )
        .into_response()
}

// ── Which process is calling (Linux) ────────────────────────────────────────

/// Whether origin detection can run on this OS.
pub fn origin_supported() -> bool {
    cfg!(target_os = "linux") && std::path::Path::new("/proc/net/tcp").exists()
}

struct Tree {
    at: Instant,
    /// The listening sockets it was built from: when they change (an app
    /// started, stopped or restarted) the tree is rebuilt at once, so an app
    /// that starts listening is never missed for a cache period.
    listen: std::collections::BTreeSet<u64>,
    /// pid -> app id, for every process in a published app's tree.
    pids: HashMap<u32, String>,
}

fn tree_cache() -> &'static Mutex<Option<Tree>> {
    static T: OnceLock<Mutex<Option<Tree>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

/// `(local_port, inode, state)` for every row of /proc/net/tcp and tcp6.
fn proc_sockets() -> Vec<(u16, u64, String)> {
    let mut out = Vec::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 {
                continue;
            }
            let Some(port) = cols[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok()) else { continue };
            let Ok(inode) = cols[9].parse::<u64>() else { continue };
            out.push((port, inode, cols[3].to_string()));
        }
    }
    out
}

fn socket_inodes(pid: u32) -> HashSet<u64> {
    let mut out = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for e in rd.flatten() {
            if let Ok(t) = std::fs::read_link(e.path()) {
                let t = t.to_string_lossy();
                if let Some(n) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')).and_then(|s| s.parse().ok()) {
                    out.insert(n);
                }
            }
        }
    }
    out
}

fn all_pids() -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // pid (comm) state ppid ...; comm may hold spaces, so split after ')'.
        let ppid = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)).and_then(|p| p.parse().ok()).unwrap_or(0);
        out.push((pid, ppid));
    }
    out
}

/// The listening sockets on published apps' ports: inode -> app id.
fn app_listeners(socks: &[(u16, u64, String)], ports: &[(u16, String)]) -> HashMap<u64, String> {
    socks
        .iter()
        .filter(|(_, _, st)| st == "0A")
        .filter_map(|(p, ino, _)| ports.iter().find(|(port, _)| port == p).map(|(_, id)| (*ino, id.clone())))
        .collect()
}

/// Rebuild pid -> app for every published app listening now: the listener and
/// all its descendants. Never the server itself.
fn build_tree(listen: &HashMap<u64, String>) -> HashMap<u32, String> {
    let mut tree = HashMap::new();
    if listen.is_empty() {
        return tree;
    }
    let me = std::process::id();
    let procs = all_pids();
    for (pid, _) in &procs {
        if *pid == me {
            continue;
        }
        for ino in socket_inodes(*pid) {
            if let Some(id) = listen.get(&ino) {
                tree.insert(*pid, id.clone());
            }
        }
    }
    // Descendants, to a fixed point (bounded by the process count).
    loop {
        let before = tree.len();
        for (pid, ppid) in &procs {
            if *pid != me && !tree.contains_key(pid) {
                if let Some(id) = tree.get(ppid).cloned() {
                    tree.insert(*pid, id);
                }
            }
        }
        if tree.len() == before {
            break;
        }
    }
    tree
}

pub enum Origin {
    App(String),
    NotApp,
    Unmeasured,
}

/// Which published app (if any) owns the client end of this loopback socket.
pub fn origin_of(peer: SocketAddr) -> Origin {
    if mode() == Mode::Off || !peer.ip().is_loopback() {
        return Origin::NotApp;
    }
    if !origin_supported() {
        if let Ok(mut c) = counters().lock() {
            c.origin_unmeasured += 1;
        }
        return Origin::Unmeasured;
    }
    let ports: Vec<(u16, String)> = super::static_files::public_links::load(&crate::config::amux_home())
        .into_iter()
        .filter(|l| l.port > 0)
        .map(|l| (l.port, l.id))
        .collect();
    if ports.is_empty() {
        return Origin::NotApp;
    }
    let socks = proc_sockets();
    let listen = app_listeners(&socks, &ports);
    let listen_set: std::collections::BTreeSet<u64> = listen.keys().copied().collect();
    let pids = {
        let mut g = tree_cache().lock().unwrap_or_else(|e| e.into_inner());
        // Rebuilt when the listeners change, and every 15 s for children an
        // app started since (the full /proc walk is the expensive part).
        if g.as_ref().is_none_or(|t| t.listen != listen_set || t.at.elapsed() > Duration::from_secs(15)) {
            *g = Some(Tree { at: Instant::now(), pids: build_tree(&listen), listen: listen_set });
        }
        g.as_ref().map(|t| t.pids.clone()).unwrap_or_default()
    };
    if let Ok(mut c) = counters().lock() {
        c.origin_lookups += 1;
    }
    if pids.is_empty() {
        return Origin::NotApp;
    }
    let candidates: HashSet<u64> = socks.into_iter().filter(|(p, _, _)| *p == peer.port()).map(|(_, i, _)| i).collect();
    if candidates.is_empty() {
        return Origin::NotApp;
    }
    for (pid, id) in &pids {
        if !socket_inodes(*pid).is_disjoint(&candidates) {
            return Origin::App(id.clone());
        }
    }
    Origin::NotApp
}

/// Invalidate the process-tree cache (a link was created, removed or renamed).
pub fn forget_tree() {
    if let Ok(mut g) = tree_cache().lock() {
        *g = None;
    }
}

// ── The A5 diagnostic ───────────────────────────────────────────────────────

/// Served in `GET /api/contract/counters` under `published_app`.
pub fn report() -> Value {
    let links = super::static_files::public_links::load(&crate::config::amux_home());
    let apps: Vec<_> = links.iter().filter(|l| l.port > 0).collect();
    let with_token = apps.iter().filter(|l| !l.app_token_sha256.is_empty()).count();
    let c = counters().lock().ok();
    let (considered, invalid, lookups, unmeasured, by_app) = c
        .as_ref()
        .map(|c| {
            let rows: Value = c
                .by_app
                .iter()
                .map(|(k, n)| {
                    (k.clone(), json!({
                        "allowed": n.allowed, "outside_allowlist": n.outside_allowlist,
                        "uncredentialed": n.uncredentialed, "inherited_worker_token": n.inherited_worker_token,
                        "owner_token": n.owner_token, "refused": n.refused,
                    }))
                })
                .collect::<serde_json::Map<_, _>>()
                .into();
            (c.considered, c.invalid_token, c.origin_lookups, c.origin_unmeasured, rows)
        })
        .unwrap_or((0, 0, 0, 0, json!({})));
    json!({
        "mode": mode().as_str(),
        "measured": true,
        "n_considered": considered,
        "since": "server start",
        "apps": apps.len(),
        "apps_with_credential": with_token,
        "apps_without_credential": apps.len() - with_token,
        "invalid_token": invalid,
        "by_app": by_app,
        "origin_detection": {
            "supported": origin_supported(),
            "measured": origin_supported(),
            "lookups": lookups,
            "unmeasured_requests": unmeasured,
            "how": "a loopback caller's socket is matched to the process tree listening on a published app's port (/proc)",
        },
        "still_open": "an app process runs as the owner's unix user and can read the owner token file. A loopback request carrying it from the app's process tree is caught here on Linux (owner_token), but a process the app detaches from its tree, a non-Linux host, or a request that does not come over loopback is not. Closed by governance PRs 16-17 (sandbox, container), not by this check.",
        "how_to_enforce": format!("{MODE_ENV}=enforce in server.env, then restart, once by_app shows only `allowed` (an app that needs access gets it in the Apps tab)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamped(actor: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(ACTOR_HEADER, HeaderValue::from_str(actor).unwrap());
        h
    }

    #[test]
    fn rules_parse_and_match_one_segment_per_star() {
        let r = parse_rules(&["post /api/sessions/*/send".into(), "GET /api/board/**".into()]).unwrap();
        assert_eq!(r, vec!["POST /api/sessions/*/send", "GET /api/board/**"]);
        assert!(allows(&r, "POST", "/api/sessions/wexus/send"));
        assert!(!allows(&r, "GET", "/api/sessions/wexus/send"), "the method is part of the rule");
        assert!(!allows(&r, "POST", "/api/sessions/wexus/send/extra"), "a star is one segment");
        assert!(allows(&r, "GET", "/api/board/AMUX-1/log"));
        assert!(!allows(&r, "GET", "/api/sessions"));
        assert!(!allows(&[], "GET", "/api/sessions"), "no rules, no access");
    }

    #[test]
    fn no_rule_can_open_an_apps_own_access_or_credentials() {
        assert!(parse_rule("POST /api/page-links/x/approve").is_err());
        assert!(parse_rule("POST /api/connectors/google-gmail/token").is_err());
        assert!(parse_rule("* /api/**").is_err(), "a wildcard over everything would include them");
        assert!(parse_rule("GET /etc/passwd").is_err());
        assert!(parse_rule("GET").is_err());
        // Even a stored rule that slipped past parsing never opens them.
        assert!(!allows(&["* /api/**".into()], "POST", "/api/page-links/x/approve"));
    }

    #[test]
    fn an_app_token_is_held_to_its_allowlist_counted_in_report_refused_in_enforce() {
        let apps = vec![AppLink { id: "a1".into(), created_by: "w".into(), allow: vec!["GET /api/sessions".into()] }];
        let h = stamped("app:a1");
        assert_eq!(gate_token_in(Mode::Enforce, &h, "GET", "/api/sessions", &apps), Gate::Admit);
        assert_eq!(gate_token_in(Mode::Report, &h, "POST", "/api/board", &apps), Gate::Continue);
        assert_eq!(gate_token_in(Mode::Enforce, &h, "POST", "/api/board", &apps), Gate::Refuse("outside_allowlist"));
        assert_eq!(gate_token_in(Mode::Off, &h, "POST", "/api/board", &apps), Gate::NotApp);
        assert_eq!(gate_token_in(Mode::Enforce, &stamped(INVALID), "GET", "/api/sessions", &apps), Gate::Refuse("app_token_invalid"));
        assert_eq!(gate_token_in(Mode::Enforce, &HeaderMap::new(), "GET", "/api/sessions", &apps), Gate::NotApp);
        let r = report();
        assert!(r["by_app"]["a1"]["outside_allowlist"].as_u64().unwrap() >= 2, "{r}");
        assert!(r["by_app"]["a1"]["refused"].as_u64().unwrap() >= 1, "{r}");
    }

    #[test]
    fn an_apps_uncredentialed_or_inherited_call_is_counted_then_refused() {
        assert_eq!(gate_origin_in(Mode::Report, "a2", Presented::Nothing, "GET", "/api/sessions"), Gate::Continue);
        assert_eq!(gate_origin_in(Mode::Enforce, "a2", Presented::WorkerToken, "GET", "/api/sessions"), Gate::Refuse("inherited_worker_token"));
        assert_eq!(gate_origin_in(Mode::Enforce, "a2", Presented::OwnerToken, "GET", "/api/sessions"), Gate::Refuse("owner_token_from_app"));
        assert_eq!(gate_origin_in(Mode::Off, "a2", Presented::Nothing, "GET", "/api/sessions"), Gate::NotApp);
        let r = report();
        assert!(r["by_app"]["a2"]["uncredentialed"].as_u64().unwrap() >= 1);
        assert!(r["by_app"]["a2"]["inherited_worker_token"].as_u64().unwrap() >= 1);
        assert!(r["by_app"]["a2"]["owner_token"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn resolve_drops_the_token_and_any_worker_claim() {
        let mut h = HeaderMap::new();
        h.insert(TOKEN_HEADER, HeaderValue::from_static("app_not_a_real_one"));
        h.insert("x-amux-session", HeaderValue::from_static("wexus"));
        h.insert(ACTOR_HEADER, HeaderValue::from_static("app:forged"));
        test_mode::set(Some(Mode::Report));
        resolve(&mut h);
        test_mode::set(None);
        assert!(h.get(TOKEN_HEADER).is_none(), "handlers never see the token");
        assert!(h.get("x-amux-session").is_none(), "an app is never a worker");
        assert_eq!(h.get(ACTOR_HEADER).unwrap(), INVALID, "an unknown token is marked invalid, a forged stamp is gone");
        assert_eq!(actor(&h), None);
        // No token at all: a forged stamp is simply removed.
        let mut h2 = stamped("app:forged");
        resolve(&mut h2);
        assert!(h2.get(ACTOR_HEADER).is_none());
    }
}
