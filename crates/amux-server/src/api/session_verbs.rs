//! /api/sessions/{name} + /api/sessions/{name}/{verb} — NATIVE session verbs
//! (AMUX-2598 cutover: "the rust version isn't using any python, just rust").
//!
//! This retires row 1 of py_proxy::PROXIED_FAMILIES. Every verb the SPA calls
//! is answered from Rust against the same fleet substrate Python manages:
//! `~/.amux/sessions/<name>.env` (registry), `<name>.meta.json` (meta),
//! `~/.amux/logs/<name>.log` (pipe-pane logs), tmux sessions named
//! `amux-<name>`, and the shared SQLite DB (steering_queue, steering_history,
//! share_tokens, session_events, cmd_history, send_dedup, issues, prefs).
//!
//! Porting map (amux-server.py, line numbers checked 2026-08-09):
//! - dispatch block            py:74873-76757
//! - peek                      py:74985-75136 (shape: history/live/output +
//!   output_lines/history_lines/output_is_viewport_only/hint — AMUX-1807 and
//!   the 2026-07-27 "swallowed message" incident are load-bearing here)
//! - transcript renderer       py:5833-5957 (_render_session_transcript)
//! - send choreography         py:25432-25715 (send_text)
//! - start choreography        py:24218-24887 (start_session)
//! - stop choreography         py:24943-25054 (stop_session)
//! - config PATCH              py:76327-76755 (rename cascade, provider/model/
//!   effort/yolo restart, dir restart, desc/tags/pin/branch/mcp/new_conversation)
//! - share                     py:65953-65999
//!
//! tmux L2: every target string is built from backend::tmux::{session_target,
//! pane_target} — the exact-match `=name` vs pane-level `=name:` split that
//! took the fleet down on 2026-08-08 lives in ONE place. All `-F` formats use
//! ':' separators, never '\t' (locale sanitization incident 2026-08-09).
//!
//! Residual gaps vs Python, named honestly (each returns a correct-typed
//! response or an explicit error — never silent):
//! - no autotask/board-labelling on send (Python's model-call feature)
//! - no _verify_submitted JSONL evidence gate after send (reports "sent" once
//!   keys landed; Python additionally greps the JSONL)
//! - no boot board-digest briefing on start (standing instructions ARE re-sent)
//! - no _install_amux_commit_hook / _auto_trust_dir / _ensure_memory side
//!   effects (Python's loops still own those during coexistence)
//! - commit-report attaches to the in-flight card but skips the cross-session
//!   sweep notice (py:76008-76230)
//! - env-explain / memory-explain answer 501 with a pointer (layered env
//!   composition is not ported yet)
//! - iTerm2-backed sessions (CC_ITERM2_SESSION_ID) answer 501 (0 in the fleet)

use super::AppState;
use crate::api::fs::{body_str, parse_body, parse_qs, qs_get};
use crate::api::sessions_legacy::strip_ansi;
use crate::backend::tmux::{pane_target, session_target};
use crate::backend::{BackendStatus, ProcessRef};
use amux_core::ids::WorkerId;
use amux_core::provider::ProviderCapabilities;
use axum::extract::{Path as AxumPath, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Compile-once regex at the use site: each expansion gets its own static.
///
/// This file's pane detectors run on the steering tick, the status sweep and
/// every peek — measured 2026-08-11 (AMUX-2906 survey): 41 `Regex::new`
/// compiles per call on those paths while 25 other sites in the same file
/// already used the OnceLock idiom by hand. One spelling, zero per-call
/// compiles. Only for STATIC patterns — a dynamic pattern would cache its
/// first value forever and silently ignore every later one.
macro_rules! cached_re {
    ($pat:expr) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| regex::Regex::new($pat).unwrap())
    }};
}

const OP_TIMEOUT: Duration = Duration::from_secs(5);
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// Python: MAX_LOG_BYTES = 10MB (py:892).
const MAX_LOG_BYTES: usize = 10 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Fleet paths (Python: CC_HOME/CC_SESSIONS/CC_LOGS/CC_MEMORY/CC_TRANSCRIPTS,
// py:59-69; CLAUDE_HOME py:862). Read at call time like sessions_legacy — the
// AppState-captured-home refactor is a named deviation there.
// ---------------------------------------------------------------------------

pub(crate) fn home() -> PathBuf {
    std::env::var("AMUX_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
    })
}
fn sessions_dir() -> PathBuf {
    home().join("sessions")
}
fn logs_dir() -> PathBuf {
    home().join("logs")
}
fn memory_dir() -> PathBuf {
    home().join("memory")
}
fn transcripts_dir() -> PathBuf {
    home().join("transcripts")
}
fn claude_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude")
}
fn env_path(name: &str) -> PathBuf {
    sessions_dir().join(format!("{name}.env"))
}
fn meta_path(name: &str) -> PathBuf {
    sessions_dir().join(format!("{name}.meta.json"))
}
fn log_path(name: &str) -> PathBuf {
    logs_dir().join(format!("{name}.log"))
}
fn plain_log_path(name: &str) -> PathBuf {
    // Hidden subdir so it never collides with a real `<name>.log` (py:5457).
    logs_dir().join(".plain").join(format!("{name}.log"))
}
fn mem_file(name: &str) -> PathBuf {
    memory_dir().join(format!("{name}.md"))
}

/// Python's `_VALID_SESSION_NAME_RE` (py:25529): `^[a-zA-Z0-9_.\-]+$`.
pub(crate) fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn is_session_blocked(name: &str) -> bool {
    std::fs::read_to_string(home().join("blocked-sessions.txt"))
        .map(|t| {
            t.lines()
                .map(str::trim)
                .any(|l| !l.is_empty() && !l.starts_with('#') && l == name)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Session .env I/O. Python's parse keeps dict insertion order and _write_env
// rewrites `# updated: <iso>` + K="V" with 0600 atomic replace (py:4180-4283).
// Ordered Vec so a rewrite preserves the user's key order.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct EnvFile {
    pairs: Vec<(String, String)>,
}

/// A temp path unique to THIS CALL, for the write-then-rename dance (AF-104).
///
/// It used to be keyed on `std::process::id()` alone, which is constant for the
/// life of the server — so two concurrent writes of the SAME env file inside one
/// process chose the SAME temp path. The first rename consumed it and the second
/// got ENOENT. Observed in production 2026-08-19 13:18:22.734: two POST
/// /api/sessions arriving in the same millisecond from one browser (a double
/// submit), one 201 and one
/// `500 could not write session env: No such file or directory (os error 2)`.
///
/// The 500 was the mild outcome. The same collision has the loser truncating and
/// rewriting the winner's temp file WHILE it is being written, so a torn env file
/// can be renamed into place and a worker boots with half its environment.
///
/// pid still distinguishes processes (two servers, or a test binary sharing the
/// dir); the counter distinguishes calls within one. Callers in BOTH modules use
/// this one function, because the previous duplication is exactly why the bug had
/// to be found and fixed twice.
pub(crate) fn unique_tmp_path(path: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("env"),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

impl EnvFile {
    fn load(path: &Path) -> Self {
        let mut pairs = Vec::new();
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self { pairs };
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                continue;
            }
            let v = v.trim();
            let v = if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
                || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
            {
                &v[1..v.len() - 1]
            } else {
                v
            };
            match pairs.iter_mut().find(|(pk, _)| pk == k) {
                Some((_, pv)) => *pv = v.to_string(),
                None => pairs.push((k.to_string(), v.to_string())),
            }
        }
        Self { pairs }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }
    fn set(&mut self, key: &str, value: &str) {
        match self.pairs.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value.to_string(),
            None => self.pairs.push((key.to_string(), value.to_string())),
        }
    }
    fn remove(&mut self, key: &str) {
        self.pairs.retain(|(k, _)| k != key);
    }

    /// Python `_write_env` (py:4252): `# updated:` header + K="V", atomic 0600.
    fn write(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut out = format!("# updated: {}\n", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.6f"));
        for (k, v) in &self.pairs {
            out.push_str(&format!("{k}=\"{v}\"\n"));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = unique_tmp_path(path);
        {
            let mut f = std::fs::File::create(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            f.write_all(out.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

pub(crate) fn parse_env(name: &str) -> EnvFile {
    EnvFile::load(&env_path(name))
}

/// One setting for a lane, resolved WORKER > GROUP > GLOBAL (AMUX-2930).
///
/// `parse_env` reads a single file — the worker's own env — which is right for
/// per-worker facts like CC_DIR or CC_PROVIDER. It is wrong for POLICY, and
/// standing-order switches are policy: `/api/scope` has advertised `env` at
/// `["global", "group", "worker"]` since the cutover, backed by real files
/// (`~/.amux/amux.env`, `~/.amux/env/<group>.env`,
/// `~/.amux/sessions/<worker>.env`), and the scope UI writes all three. But
/// every consumer read the worker file only, so turning a standing order off
/// globally or for a group wrote a file that nothing consulted — the setting
/// appeared to save and changed nothing.
///
/// That is ethos rule 1's exact shape: a view that does not share the predicate
/// of the mechanism it describes. The mechanism is fixed here rather than the
/// view narrowed, because the scoped view is what Ethan asked for.
///
/// GLOBAL is the amux.env that lane shells already source at startup, so this
/// does not introduce a new file or a new spelling — it makes the gates read
/// the layer that was always there. Worker wins over group, group over global;
/// first match wins, so an explicit `=1` at worker level overrides a `=0` set
/// fleet-wide, which is the direction a per-worker exception has to run.
/// The resolution itself, over an explicit home.
///
/// Parameterised for the same reason `config::resolve_home` is: `AMUX_HOME` is
/// process-global and `cargo test` runs a binary's tests in PARALLEL, so tests
/// that set it race each other over ONE `amux.env` — three of these tests
/// failed exactly that way on the first run, each having its global layer
/// rewritten mid-assertion by a sibling. Serialising them with a mutex or
/// `--test-threads=1` would have made them green while quietly making them
/// order-dependent.
/// The env files that make up a lane's scope, IN SOURCE ORDER: global, then each
/// group, then the worker (AMUX-3106). Only files that exist are returned.
///
/// Source order IS the precedence, because `source` lets the last assignment
/// win — so this yields worker > group > global, matching `scoped_setting_in`
/// exactly. That match is the point: `scoped_setting_in` resolves ONE key for a
/// gate to read, this delivers the WHOLE layer into the launched process, and if
/// their precedence ever diverged a key would mean one thing to a gate and
/// another inside the lane's own shell.
///
/// Parameterised on `home` for the reason `scoped_setting_in` spells out:
/// `AMUX_HOME` is process-global and cargo runs a binary's tests in PARALLEL, so
/// tests that set it race each other over one `amux.env`.
///
/// Groups come from CC_TAGS in the worker's own file — the same source
/// `lane_groups` reads, not a second spelling.
pub(crate) fn scope_env_layers(home: &std::path::Path, lane: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let global = home.join("amux.env");
    if global.exists() {
        out.push(global);
    }
    let worker_file = home.join("sessions").join(format!("{lane}.env"));
    let groups: std::collections::BTreeSet<String> = EnvFile::load(&worker_file)
        .get("CC_TAGS")
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().trim_matches('"').to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for g in groups {
        let gp = home.join("env").join(format!("{g}.env"));
        if gp.exists() {
            out.push(gp);
        }
    }
    if worker_file.exists() {
        out.push(worker_file);
    }
    out
}

pub(crate) fn scoped_setting_in(home: &std::path::Path, lane: &str, key: &str) -> Option<String> {
    fn nonempty(v: &str) -> Option<String> {
        let t = v.trim();
        (!t.is_empty()).then(|| t.to_string())
    }
    let worker = EnvFile::load(&home.join("sessions").join(format!("{lane}.env")));
    if let Some(v) = worker.get(key).and_then(nonempty) {
        return Some(v);
    }
    // Groups come from the worker's own CC_TAGS — the same source
    // `lane_groups` uses, not a second spelling of "which groups is this in".
    let groups: Vec<String> = worker
        .get("CC_TAGS")
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().trim_matches('"').to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for g in groups {
        let path = home.join("env").join(format!("{g}.env"));
        if let Some(v) = EnvFile::load(&path).get(key).and_then(nonempty) {
            return Some(v);
        }
    }
    EnvFile::load(&home.join("amux.env")).get(key).and_then(nonempty)
}

fn provider_of(cfg: &EnvFile) -> String {
    let p = cfg.get_or("CC_PROVIDER", "claude").trim().to_lowercase();
    if SESSION_PROVIDERS.contains(&p.as_str()) && !p.is_empty() {
        p
    } else {
        "claude".into()
    }
}

fn work_dir_of(cfg: &EnvFile) -> String {
    let wd = cfg.get_or("CC_DIR", "").trim();
    if wd.is_empty() {
        return String::new();
    }
    expanduser(wd)
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| expanduser(wd).to_string_lossy().into_owned())
}

pub(crate) fn session_work_dir(name: &str) -> String {
    work_dir_of(&parse_env(name))
}

fn expanduser(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest);
    }
    if p == "~" {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default());
    }
    PathBuf::from(p)
}

// ---------------------------------------------------------------------------
// Meta I/O (py:12229-12251).
// ---------------------------------------------------------------------------

fn load_meta(name: &str) -> Map<String, Value> {
    std::fs::read_to_string(meta_path(name))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn save_meta(name: &str, meta: &Map<String, Value>) {
    let _ = std::fs::create_dir_all(sessions_dir());
    let _ = std::fs::write(meta_path(name), Value::Object(meta.clone()).to_string());
}

fn update_meta(name: &str, updates: &[(&str, Value)]) {
    let mut meta = load_meta(name);
    for (k, v) in updates {
        meta.insert((*k).to_string(), v.clone());
    }
    save_meta(name, &meta);
}

fn meta_str(meta: &Map<String, Value>, key: &str) -> String {
    meta.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// Read an integer out of a session's meta.
///
/// ALWAYS reach for this instead of `load_meta(name)["key"]`. `load_meta`
/// returns a `serde_json::Map`, NOT a `Value`, and the two index identically to
/// the eye while behaving oppositely: `Value["missing"]` yields `Null`, but
/// `Map["missing"]` forwards to `BTreeMap::index` and PANICS with "no entry
/// found for key". That is not a hypothetical — it took the whole server down
/// on the first boot after the Python->Rust cutover (see below), because no
/// pre-cutover `*.meta.json` carries `rate_limited_since` and the sweep indexes
/// it on every lane.
fn meta_i64(meta: &Map<String, Value>, key: &str) -> i64 {
    meta.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// tmux ops. Targets come ONLY from backend::tmux's L2 helpers; the fleet's
// tmux name is `amux-<name>` (py:4307 tmux_name — legacy cmux-/cc- migration
// dropped: nothing in the fleet carries those prefixes anymore).
// ---------------------------------------------------------------------------

fn tmux_name(name: &str) -> String {
    format!("amux-{name}")
}
/// Session-level target (`=amux-<n>`), exact match (py:4323 tmux_target notes
/// the 2026-08-08 prefix-match kill; L2 keeps the format in tmux.rs).
fn st(name: &str) -> String {
    session_target(&tmux_name(name))
}
/// Pane-level target (`=amux-<n>:`).
fn pt(name: &str) -> String {
    pane_target(&tmux_name(name))
}

async fn run_cmd(bin: &str, args: &[&str], timeout: Duration) -> Option<std::process::Output> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => Some(out),
        _ => None,
    }
}

async fn tmux(args: &[&str]) -> Option<std::process::Output> {
    run_cmd("tmux", args, OP_TIMEOUT).await
}

async fn tmux_sessions_set() -> std::collections::BTreeSet<String> {
    let Some(out) = tmux(&["list-sessions", "-F", "#{session_name}"]).await else {
        return Default::default();
    };
    if !out.status.success() {
        return Default::default();
    }
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim().to_string()).collect()
}

/// tmux capture-pane -e; lines<=0 → visible screen only (py:4406).
pub(crate) async fn tmux_capture(name: &str, lines: i64) -> String {
    if session_backend(name) == "herdr" {
        return herdr_capture(name, lines.max(1)).await;
    }
    let pt = pt(name);
    let start;
    let mut args = vec!["capture-pane", "-t", pt.as_str(), "-p", "-e"];
    if lines > 0 {
        start = format!("-{lines}");
        args.push("-S");
        args.push(&start);
    }
    match run_cmd("tmux", &args, CAPTURE_TIMEOUT).await {
        Some(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

/// `#{alternate_on}` probe (py:4480). herdr keeps scrollback under alt.
async fn tmux_alt_screen(name: &str) -> bool {
    if session_backend(name) == "herdr" {
        return false;
    }
    let pt = pt(name);
    match tmux(&["display-message", "-t", &pt, "-p", "#{alternate_on}"]).await {
        Some(out) => String::from_utf8_lossy(&out.stdout).trim() == "1",
        None => false,
    }
}

pub(crate) async fn send_key(name: &str, key: &str) {
    let pt = pt(name);
    let _ = tmux(&["send-keys", "-t", &pt, key]).await;
}
async fn send_literal(name: &str, text: &str) -> bool {
    let pt = pt(name);
    matches!(tmux(&["send-keys", "-t", &pt, "-l", text]).await, Some(o) if o.status.success())
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

// ---------------------------------------------------------------------------
// Backend selection (py:4673-4692). CC_BACKEND wins, then AMUX_BACKEND env.
// ---------------------------------------------------------------------------

fn backend_of_cfg(cfg: &EnvFile) -> String {
    let b = cfg.get_or("CC_BACKEND", "").trim().to_lowercase();
    if b == "herdr" || b == "tmux" {
        return b;
    }
    let ab = std::env::var("AMUX_BACKEND").unwrap_or_default().trim().to_lowercase();
    if ab == "herdr" {
        "herdr".into()
    } else {
        "tmux".into()
    }
}
fn session_backend(name: &str) -> String {
    backend_of_cfg(&parse_env(name))
}
fn iterm2_id(cfg: &EnvFile) -> String {
    cfg.get_or("CC_ITERM2_SESSION_ID", "").trim().to_string()
}

// ---------------------------------------------------------------------------
// herdr ops through the BACKEND (backend::herdr), not a second CLI client.
//
// ADDRESSING IS THE WHOLE POINT. A herdr-hosted worker is a herdr *workspace*
// labelled `amux-{worker_id}` — `backend_ref` owns that derivation (Invariant
// 43). The path retired here spoke to herdr directly and derived a herdr
// *agent name* from the DISPLAY NAME instead, which nothing ever created:
// measured 2026-08-20 against herdr 0.8.0, `agent get sphinx-frontend` answers
// `agent_not_found` while `workspace list` shows the pane under
// `amux-wrk_01M0EJKXQ8Q4P2FPKE0730GVP3`. So every herdr peek read BLANK and
// every herdr send reported "not running" — the two symptoms this replaces.
//
// Delegating also means one herdr client in the process: the backend already
// owns `--session` routing, the JSON envelope, timeouts, and a pane-id cache
// with cold-retry on eviction. Duplicating that here is how the two copies
// drifted into disagreeing about what a worker is called.
// ---------------------------------------------------------------------------

/// The store this server opened, by the SAME rule `ServerConfig::load` uses:
/// `AMUX_DB` if set, else `<home>/amux.db`, where `<home>` comes from
/// `config::amux_home` — `AMUX_HOME`, legacy `CC_HOME`, `~/.amux`, `/.amux`.
/// `server.env` is exported into the process env at load, so `AMUX_DB` is
/// visible here.
///
/// Both halves are deliberate. Ignoring `AMUX_DB` would point at a file that
/// is usually absent, so every worker would look unknown and herdr peek/send
/// would answer "not addressable" on a correctly configured server. And
/// resolving the home locally is what config.rs's own history warns about:
/// `CC_HOME` used to be honoured by one of ten copies, which is how one
/// server came to read two data directories. There is one resolver; this uses
/// it. (`home()` and `dirs_home()` in this file still predate it and skip
/// `CC_HOME` — untouched here, but they are the same hazard.)
fn store_db_path() -> PathBuf {
    match std::env::var("AMUX_DB") {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
        _ => crate::config::amux_home().join("amux.db"),
    }
}

/// The herdr backend plus this worker's process ref, or `None` when herdr is
/// not a configured backend, the backend set was never published (unit tests,
/// migrate-only mode), or `name` is not a worker in the store.
///
/// `None` is "cannot address", never "not running": callers map it to a read
/// failure rather than an empty terminal, so a misconfiguration cannot
/// masquerade as a quiet session.
fn herdr_target(name: &str) -> Option<(Arc<dyn crate::backend::SessionBackend>, ProcessRef)> {
    let backend = crate::backend::process_backend("herdr")?;
    let conn = rusqlite::Connection::open_with_flags(
        store_db_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    // Resolves ids, display names, and post-rename aliases (Invariant 17) —
    // the same lookup the worker API uses, so an alias that peeks also sends.
    let row = crate::db::queries::get_worker(&conn, name).ok().flatten()?;
    let id = WorkerId::parse(&row.id).ok()?;
    Some((
        backend,
        ProcessRef { backend_ref: crate::backend::backend_ref(&id), pid: None },
    ))
}

async fn herdr_agent_running(name: &str) -> bool {
    let Some((backend, proc)) = herdr_target(name) else {
        return false;
    };
    // Only a positive `Running` counts. An `Err` here is the herdr server
    // being unreadable, which is NOT evidence the worker stopped (#84's rule,
    // applied to a single worker instead of the fleet).
    matches!(backend.status(&proc).await, Ok(BackendStatus::Running))
}

async fn herdr_capture(name: &str, lines: i64) -> String {
    let Some((backend, proc)) = herdr_target(name) else {
        return String::new();
    };
    let lines = lines.clamp(1, u32::MAX as i64) as u32;
    match backend.capture(&proc, lines).await {
        Ok(out) => out.trim().to_string(),
        Err(e) => {
            tracing::debug!(worker = name, "herdr capture failed: {e}");
            String::new()
        }
    }
}

/// A herdr-backed lane the backend cannot address: no herdr backend
/// configured (`AMUX_HERDR_SESSION` unset), or no worker row to derive the
/// workspace label from. A CAPABILITY GAP, not a failed delivery — classified
/// 501 by `send_failure_status`, which reads this same constant so the
/// producer and the classifier cannot drift apart.
pub(crate) const HERDR_NOT_ADDRESSABLE: &str = "herdr lane is not addressable";

/// Submit `text` to the worker's agent. Shared by `send` and by the `/exit`
/// stop path, so both address the pane the same way.
async fn herdr_prompt(name: &str, text: &str) -> Result<(), String> {
    let Some((backend, proc)) = herdr_target(name) else {
        return Err(HERDR_NOT_ADDRESSABLE.into());
    };
    backend.send_text(&proc, text).await.map_err(|e| e.to_string())
}

async fn herdr_send(name: &str, text: &str) -> (bool, String) {
    // ONE resolution for the whole send: the status probe, the picker read and
    // the delivery all address the same pane, and a rename mid-send cannot
    // land the text in a different worker than the one we checked.
    let Some((backend, proc)) = herdr_target(name) else {
        return (false, HERDR_NOT_ADDRESSABLE.into());
    };
    match backend.status(&proc).await {
        Ok(BackendStatus::Running) => {}
        Ok(_) => return (false, "not running".into()),
        // A herdr server we cannot read is NOT a stopped worker (#84's rule).
        // Reporting "not running" here would auto-wake a lane that is already
        // up the moment herdr answers again.
        Err(e) => return (false, format!("herdr status unreadable: {e}")),
    }
    let cap = backend.capture(&proc, 15).await.unwrap_or_default();
    if !cap.trim().is_empty() && at_resume_picker(cap.trim()) {
        return (false, "session is in resume picker".into());
    }
    match backend.send_text(&proc, text).await {
        Ok(()) => (true, "sent".into()),
        // NAME THE REASON. "herdr prompt failed" was one string for a closed
        // workspace, a pane herdr does not recognize as an agent, and a dead
        // herdr server — three different fixes for the human reading it.
        Err(e) => (false, format!("herdr prompt failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Text utilities (py:5346-5455, 5958-6010).
// ---------------------------------------------------------------------------

/// Blank-run collapse, ANSI-aware (py:5359 _collapse_blank_runs, keep=1).
fn collapse_blank_runs(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blanks = 0;
    for ln in text.split('\n') {
        if strip_ansi(ln).trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push("");
            }
        } else {
            blanks = 0;
            out.push(ln);
        }
    }
    out.join("\n")
}

/// py:5393 _strip_scroll_pill — Claude's "Jump to bottom (click) ↓" overlay.
fn strip_scroll_pill(text: &str) -> String {
    if !text.contains("Jump to bottom") {
        return text.to_string();
    }
    let re = cached_re!(r"\s*Jump to bottom \(click\)\s*[↓]?\s*");
    re.replace_all(text, " ").into_owned()
}

fn launch_markers() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"--dangerously-skip-permissions\s+--name\b|unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT|Resume this session with:|claude --resume\b|The default interactive shell is now zsh|please visit https://support\.apple\.com/kb/HT208050",
        )
        .expect("launch markers regex")
    })
}

/// py:5405 _strip_launch_noise — cut through amux's relaunch scaffolding.
fn strip_launch_noise(text: &str) -> String {
    // This cheap pre-filter must not be able to disagree with
    // `launch_markers()`, which is the regex that actually decides what gets
    // cut. It could: the marker set matches `claude --resume`, but the filter
    // did not, so a RESUMED pane — whose launch line carries `--resume` and
    // (before AMUX-2612) no `--name` — returned here unstripped and handed the
    // whole boot command line to every state/model heuristic downstream. Both
    // existing fixtures happened to contain `--name`, so nothing could fail on
    // it. `--resume` is listed here for the same reason it is listed there.
    if text.is_empty()
        || (!text.contains("--name")
            && !text.contains("--resume")
            && !text.contains("Resume this session with")
            && !text.contains("shell is now zsh"))
    {
        return text.to_string();
    }
    let bare_prompt = cached_re!(r"^[A-Za-z0-9._-]{1,24}\$$");
    let lines: Vec<&str> = text.split('\n').collect();
    let mut last: isize = -1;
    for (i, ln) in lines.iter().enumerate() {
        if launch_markers().is_match(&strip_ansi(ln)) {
            last = i as isize;
        }
    }
    if last < 0 {
        return text.to_string();
    }
    let mut j = (last + 1) as usize;
    while j < lines.len() {
        let clean = strip_ansi(lines[j]).trim().to_string();
        if clean.is_empty() || bare_prompt.is_match(&clean) {
            j += 1;
        } else {
            break;
        }
    }
    let kept = &lines[j..];
    if !kept.iter().any(|l| !strip_ansi(l).trim().is_empty()) {
        return text.to_string();
    }
    kept.join("\n")
}

fn cursor_move_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[(?:\d+;\d+H|\?25[lh]|\d+[ABCD]|H)").unwrap())
}

/// py:5346 _log_looks_torn.
fn log_looks_torn(text: &str) -> bool {
    if text.len() < 2000 {
        return false;
    }
    let c = cursor_move_re().find_iter(text).count();
    c >= 20 && (c as f64) / (text.len() as f64 / 1024.0) >= 2.0
}

/// py:5958 _trim_live_overlap — live frame minus what the transcript covers.
fn trim_live_overlap(transcript: &str, live: &str) -> String {
    if transcript.is_empty() || live.is_empty() {
        return live.to_string();
    }
    fn norm(s: &str) -> String {
        let s = strip_ansi(s);
        let re = cached_re!(
            "[*#`_|\u{2502}\u{250c}\u{2510}\u{2514}\u{2518}\u{251c}\u{2524}\u{252c}\u{2534}\u{253c}\u{2500}=>\u{2022}\u{00b7}\u{00bb}\u{276f}\u{23bf}\u{23fa}\u{273b}\u{2726}\u{25cf}]+"
        );
        let s = re.replace_all(&s, " ");
        let ws = cached_re!(r"\s+");
        ws.replace_all(s.trim(), " ").to_lowercase()
    }
    let tlines: Vec<&str> = transcript.split('\n').collect();
    let tail_start = tlines.len().saturating_sub(140);
    let tail_norm: Vec<String> = tlines[tail_start..]
        .iter()
        .map(|x| norm(x))
        .filter(|n| n.chars().count() >= 12)
        .collect();
    let tail_set: std::collections::BTreeSet<&str> = tail_norm.iter().map(|s| s.as_str()).collect();
    let long_tail: Vec<&String> = tail_norm.iter().filter(|n| n.chars().count() >= 46).collect();
    let in_transcript = |n: &str| -> bool {
        if n.chars().count() < 12 {
            return false;
        }
        if tail_set.contains(n) {
            return true;
        }
        if n.chars().count() >= 24 {
            for tv in &long_tail {
                if n.contains(tv.as_str()) || tv.contains(n) {
                    return true;
                }
            }
        }
        false
    };
    let ll: Vec<&str> = live.split('\n').collect();
    let matches: Vec<usize> =
        ll.iter().enumerate().filter(|(_, x)| in_transcript(&norm(x))).map(|(i, _)| i).collect();
    if matches.len() < 3 {
        return live.to_string();
    }
    let after = matches[matches.len() - 1] + 1;
    ll[after.min(ll.len())..].join("\n").trim_start_matches('\n').to_string()
}

fn chars_truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// Pane-state detectors (py:8229-8330, 18466-18580). D1 territory: these are
// the scraper FALLBACK Python still runs; ported verbatim-enough that the
// send/start choreography behaves the same.
// ---------------------------------------------------------------------------

const PROMPT_GLYPHS: [char; 3] = ['\u{276f}', '\u{203a}', '>'];

fn is_prompt_line(s: &str) -> bool {
    s.chars().next().map(|c| PROMPT_GLYPHS.contains(&c)).unwrap_or(false)
}

/// py:8229 _claude_ui_visible (claude + codex + gemini markers).
fn claude_ui_visible(clean_output: &str) -> bool {
    let lines: Vec<&str> = clean_output.lines().filter(|l| !l.trim().is_empty()).collect();
    let shell_prompt = cached_re!(r"^.*[$%]\s");
    let n = lines.len();
    for l in &lines[n.saturating_sub(3)..] {
        let ls = l.trim().to_lowercase();
        if shell_prompt.is_match(&ls) {
            continue;
        }
        // Permission-mode footer, ALL modes. The list used to hold only the
        // bypass (⏵⏵ / "bypass permissions") and "plan mode" footers, so a
        // session booted WITHOUT --dangerously-skip-permissions (the default
        // for a worker created from the dashboard modal) shows the footer
        // "⏸ manual mode on · ? for shortcuts" and was NEVER detected as ready.
        // send_after_ready then polled for its whole timeout and dropped the
        // create-modal start prompt on the floor (AMUX-3055). "for shortcuts"
        // is the mode-independent idle-composer footer Claude Code prints in
        // every permission mode and no shell prints it, so it is a safe
        // positive marker that closes the whole mode family at once.
        if l.contains("\u{23f5}\u{23f5}")
            || ls.contains("bypass permissions")
            || ls.contains("plan mode")
            || ls.contains("manual mode")
            || ls.contains("for shortcuts")
        {
            return true;
        }
        if ls.contains("codex")
            && (ls.contains("full-auto") || ls.contains("suggest") || ls.contains("workspace")
                || ls.contains("approval") || ls.contains("-a never"))
        {
            return true;
        }
    }
    for l in &lines[n.saturating_sub(12)..] {
        let s = l.trim();
        if let Some(c) = s.chars().next() {
            if ('\u{2700}'..='\u{27bf}').contains(&c) && c != '\u{276f}' {
                return true;
            }
        }
    }
    let gpt_re = cached_re!(r"gpt-\d|o[34][-m]");
    for l in &lines[n.saturating_sub(12)..] {
        let s = l.trim();
        let sl = s.to_lowercase();
        if s.starts_with('\u{2022}') && sl.contains("working") && sl.contains("esc to interrupt") {
            return true;
        }
        if s.contains('\u{00b7}') && gpt_re.is_match(s) {
            return true;
        }
    }
    let head: Vec<&str> = lines.iter().take(15).copied().collect();
    let tail20: Vec<&str> = lines[n.saturating_sub(20)..].to_vec();
    let has_codex = head.iter().chain(tail20.iter()).any(|l| l.to_lowercase().contains("codex"));
    if has_codex {
        for l in &lines[n.saturating_sub(5)..] {
            let ls = l.trim();
            if ls == ">" || ls.starts_with("> ") || ls.starts_with('\u{203a}') {
                return true;
            }
            if ls.contains('\u{00b7}') && (ls.contains("gpt-") || ls.contains("o3") || ls.contains("o4")) {
                return true;
            }
        }
    }
    let head20: Vec<&str> = lines.iter().take(20).copied().collect();
    let tail12: Vec<&str> = lines[n.saturating_sub(12)..].to_vec();
    let has_gemini =
        head20.iter().chain(tail12.iter()).any(|l| l.to_lowercase().contains("gemini"));
    if has_gemini {
        for l in &lines[n.saturating_sub(8)..] {
            let ls = l.trim().to_lowercase();
            if ls == ">" || ls.starts_with("> ") || ls.starts_with('\u{203a}') {
                return true;
            }
            if ls.contains("gemini-") || ls.contains("yolo") || ls.contains("approval") {
                return true;
            }
        }
    }
    false
}

/// py:8288 _at_resume_picker.
fn at_resume_picker(clean_output: &str) -> bool {
    !clean_output.is_empty()
        && (clean_output.contains("Resume Session")
            || clean_output.contains("Type to Search")
            || clean_output.contains("Enter to select")
            || clean_output.contains("Esc to cancel"))
        && clean_output.contains('\u{2315}')
}

/// py:8307 _at_shell_prompt.
fn at_shell_prompt(clean_output: &str) -> bool {
    if claude_ui_visible(clean_output) {
        return false;
    }
    let lines: Vec<&str> = clean_output.lines().filter(|l| !l.trim().is_empty()).collect();
    let ends = cached_re!(r"[$%]\s*$");
    let leaks = cached_re!(r"^\S+[$%]\s");
    for l in &lines[lines.len().saturating_sub(5)..] {
        let ls = l.trim();
        if ends.is_match(ls) && !ls.contains('\u{276f}') {
            return true;
        }
        if leaks.is_match(ls) && !ls.contains('\u{276f}') {
            return true;
        }
    }
    false
}

/// py:18479 _detect_claude_status → 'active' | 'waiting' | 'idle' | ''.
/// Is this pane sitting on Claude Code's RATE-LIMIT menu?
///
/// ```text
/// What do you want to do?
/// > 1. Stop and wait for limit to reset
///   2. Switch to usage credits
///   3. Switch to Team plan
/// ```
///
/// THE DISTINCTION THIS EXISTS TO DRAW (AMUX-2820). `detect_claude_status`
/// returns "waiting" for this menu and for an AskUserQuestion picker alike, and
/// everything downstream then refuses to type — correctly for the second and
/// catastrophically for the first:
///
///   - An AskUserQuestion picker is A QUESTION FOR THE USER. Typing into it, or
///     the picker-closing Escape, REJECTS a pending tool call. It must wait, and
///     the deadline must not override it (the 2026-07-15 kill).
///   - A rate-limit menu is INFRASTRUCTURE. Nobody is being asked anything a
///     model or a human needs to weigh; option 1 is "wait", which is what the
///     lane would do anyway. amux owns this one (ethos D2 — the POLICY is the
///     human's, set once via `rate_limit_action`, not a decision per occurrence).
///
/// Conflating them is a PERMANENT DEADLOCK, observed live on mvs-infra
/// 2026-08-10: two of Ethan's messages queued 400s+ behind a menu that nothing
/// would ever answer, because the send path parked on the selector and no other
/// code path dismisses one. Pressing Enter in the dashboard just queued another
/// message behind the same menu.
///
/// Matched on all three option lines plus the question, not on any one of them:
/// "Stop and wait for limit to reset" alone could appear in a transcript being
/// discussed, and this decides whether amux presses a key.
pub(crate) fn is_rate_limit_menu(raw: &str) -> bool {
    let clean = strip_ansi(raw).to_lowercase();
    clean.contains("what do you want to do?")
        && clean.contains("stop and wait for limit to reset")
        && clean.contains("switch to usage credits")
}

/// Claude's "you have hit your usage limit, continuing on usage credits" BANNER.
/// This is the state Ethan sees ("workers with a rate limit") that the menu misses:
/// a lane past the menu, still working but ON CREDITS (2026-08-16, general-canvas-
/// apps / backend / sherpa showed it while amux's menu-only detection reported 0).
///
/// Reliability rests on two things, and the caller MUST honour the first. First,
/// the caller passes ONLY THE FOOTER (the last ~8 lines of the live frame, the
/// Claude status region above the input box), never the whole pane: the earlier
/// whole-pane scrape flagged `amux` for CODING the banner strings, because that
/// code sits in scrollback (ethos rule 7, content-vs-state), and Claude renders
/// this banner one line above the spinner, so footer-scoping excludes scrollback.
/// Second, it matches Claude's FULL user-facing SENTENCE, not the bare words, and
/// a lane discussing usage credits does not emit that exact sentence as a footer
/// line. Validated live across all 47 running lanes: flags exactly the 3 on-credits
/// lanes and leaves amux (mid-edit on this very code) clean.
pub(crate) fn is_rate_limited_credit_banner(footer: &str) -> bool {
    // Claude uses a curly apostrophe in "you're"; normalise before matching.
    let clean = strip_ansi(footer).to_lowercase().replace('\u{2019}', "'");
    clean.contains("usage-credits to finish what you're working on")
        || clean.contains("reached your usage limit and")
        // The 2026-08 wording: "⚠ Usage limit reached · continuing automatically
        // at 2am · esc to cancel". Found the hard way on 2026-08-19: 11 lanes
        // sat limit-parked while /api/sessions reported credit_limited for NONE
        // of them, so the fleet view showed a wall of "needs input" and the one
        // question it exists to answer — which workers are limited right now —
        // was answered wrong. Matched through the interpunct separator (a
        // sentence no lane emits as prose), footer-scoped like the others.
        || clean.contains("usage limit reached \u{00b7} continuing automatically at")
}

/// The policy for what amux does with a rate-limit menu (ethos D2).
///
/// `wait` (default) presses 1. `off` leaves the menu for a human and reports it.
/// Deliberately a POLICY set once, not a prompt per occurrence: a human pressing
/// 1 on sixty lanes is not a workflow, and the answer is the same every time.
pub(crate) fn rate_limit_action() -> String {
    std::env::var("AMUX_RATE_LIMIT_ACTION")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "wait".into())
}

/// Is this pane sitting on Claude Code's RESUME-MODE selector?
///
/// ```text
/// This session is 2d old and 725.5k tokens.
/// Resuming the full session will consume a substantial portion of your
/// usage limits. We recommend resuming from a summary.
/// ❯ 1. Resume from summary (recommended)
///   2. Resume full session as-is
///   3. Don't ask me again
/// ```
///
/// Shown by `claude --resume` on old/large sessions — which is every lane after
/// a machine reboot (the 2026-08-19 panic restart parked a third of the fleet
/// on this exact selector). Like the rate-limit menu it is INFRASTRUCTURE, not
/// a question anyone needs to weigh per-occurrence: Ethan set the policy once
/// (2026-08-19, "resume from summary; it should work across different models").
///
/// Matched on the two numbered option LINES (line-anchored, optional highlight
/// pointer) plus the third option's text, never on the header sentence — the
/// header carries the session's age and token count, which vary per lane and
/// per model, and matching prose is how the credit-banner scrape false-flagged
/// lanes that were merely CODING the strings (2026-08-16). Line-anchored
/// `1.`/`2.` shapes do not appear when this source file itself is on screen
/// (quoted strings carry a leading `"`).
pub(crate) fn is_resume_mode_prompt(raw: &str) -> bool {
    let clean = strip_ansi(raw).to_lowercase().replace('\u{2019}', "'");
    let opt1 = cached_re!(r"(?m)^\s*(?:[\u{276f}>]\s*)?1\.\s+resume from summary");
    let opt2 = cached_re!(r"(?m)^\s*(?:[\u{276f}>]\s*)?2\.\s+resume full session");
    opt1.is_match(&clean) && opt2.is_match(&clean) && clean.contains("don't ask me again")
}

/// The policy for the resume-mode selector (ethos D2, same shape as
/// `rate_limit_action`).
///
/// `summary` (default) presses the digit 1 — "Resume from summary". `off`
/// leaves the selector for a human, and the sweep then stamps it
/// input-required like any other picker. The digit, NOT Enter: Enter selects
/// whatever happens to be highlighted, and a stray Down before the answer
/// lands would silently pick "Resume full session" (burns the usage limits
/// this prompt exists to protect) or "Don't ask me again" (a persistent
/// config change amux must never make by accident — ethos rule 8).
///
/// SCOPE (Ethan, 2026-08-19): this menu and the rate-limit menu are the ONLY
/// prompts amux auto-answers (yolo's own permission flags aside). Do not
/// generalize this mechanism to other selectors.
pub(crate) fn resume_mode_action() -> String {
    std::env::var("AMUX_RESUME_MODE_ACTION")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "summary".into())
}

/// A transient 5xx / Overloaded API-error banner sitting at the TAIL of the pane
/// — the lane is currently STUCK on it (a retry would help), not merely that a
/// 529 happened earlier and the lane moved on. Anchored on the marker line
/// (`API Error: 5xx`) and tail-only (last ~8 non-empty lines), so a lane that
/// recovered and produced newer output does not read as errored. Ethan
/// 2026-08-18: capture this as its own status (tested on @backend, which had a
/// 529 Overloaded and read as `idle`).
pub(crate) fn has_current_api_error(raw_output: &str) -> bool {
    if raw_output.is_empty() {
        return false;
    }
    let clean = strip_ansi(raw_output);
    let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    let n = lines.len();
    let re = cached_re!(r"(?i)^(?:[\x{23fa}\x{25cf}\x{2022}]\s*)?API Error:\s*5\d\d\b");
    lines[n.saturating_sub(8)..].iter().any(|l| re.is_match(l.trim()))
}

/// A spinner frame whose glyph is OUTSIDE the dingbat/braille ranges
/// (AMUX-3426). Claude Code's spinner cycles through ·✢✶✻✽* — the plain
/// middle-dot and asterisk phases are ordinary characters, so a capture
/// landing on one of them read a GENERATING turn as idle. The live specimen:
/// autodesk, 21m48s into a turn, frame `· Befuddling… (21m 48s · ↓ 15.0k
/// tokens)`, with the bar's esc-to-interrupt rightly discounted for its
/// agents hint (AMUX-2959) — so nothing was left to say active, the ❯
/// fallback said idle, and the idle->active contradiction rule (whose
/// evidence is THIS detector) could never fire. Recognise the frame by its
/// SHAPE, not its glyph: a verb ending in … plus the elapsed-time
/// parenthetical only the spinner line carries. A prose bullet quoting an
/// ellipsis has no `(21m 48s` suffix.
fn is_plain_spinner_line(s: &str) -> bool {
    let Some(first) = s.chars().next() else { return false };
    (first == '\u{b7}' || first == '*')
        && s.contains('\u{2026}')
        && cached_re!(r"\((\d+h )?(\d+m )?\d+s\b").is_match(s)
}

/// The class-kill half of AMUX-3426: when the ❯ fallback is about to say
/// idle, a spinner-SHAPED line (… + elapsed-time suffix, symbol first char)
/// that none of the glyph rules classified means a spinner variant this
/// parser does not know. Say so ONCE per glyph — the next variant announces
/// itself in the log instead of costing another screenshot investigation.
fn warn_once_on_unrecognized_spinner(lines: &[&str]) {
    use std::sync::Mutex;
    static SEEN: Mutex<Option<std::collections::BTreeSet<char>>> = Mutex::new(None);
    let timer_re = cached_re!(r"\((\d+h )?(\d+m )?\d+s\b");
    let n = lines.len();
    for l in lines[n.saturating_sub(30)..].iter() {
        let s = l.trim();
        if !s.contains('\u{2026}')
            || is_prompt_line(s)
            || is_plain_spinner_line(s)
            || !timer_re.is_match(s)
        {
            continue;
        }
        let Some(c) = s.chars().next() else { continue };
        if c.is_alphanumeric()
            || ('\u{2700}'..='\u{27bf}').contains(&c)
            || ('\u{2800}'..='\u{28ff}').contains(&c)
        {
            continue;
        }
        let mut g = SEEN.lock().unwrap_or_else(|p| p.into_inner());
        let set = g.get_or_insert_with(Default::default);
        if set.insert(c) {
            tracing::warn!(
                "unrecognized spinner glyph U+{:04X} on a generating-shaped line ({:?}) — \
                 detect_claude_status reads this frame as idle; teach the glyph to the \
                 spinner rules (AMUX-3426 class)",
                c as u32,
                s.chars().take(48).collect::<String>()
            );
        }
    }
}

pub(crate) fn detect_claude_status(raw_output: &str) -> String {
    if raw_output.is_empty() {
        return String::new();
    }
    let clean = strip_ansi(raw_output);
    let lines: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    let n = lines.len();
    let reading_re = cached_re!(r"^Reading \d+ file");
    // 0. Active spinner, wide window.
    for l in lines[n.saturating_sub(30)..].iter().rev() {
        let s = l.trim();
        if is_prompt_line(s) {
            continue;
        }
        if let Some(c) = s.chars().next() {
            if ('\u{2700}'..='\u{27bf}').contains(&c) && s.contains('\u{2026}') {
                return "active".into();
            }
            // GEMINI/CODEX spin with BRAILLE glyphs (U+2800-28FF), not the
            // dingbat range above — found live 2026-08-11 (AMUX-2913): a
            // gemini tool turn (`⠙ Thinking... (esc to cancel, 9s)`) read as
            // not-active for its entire run, and a stale picker stamp showed
            // through as `waiting` while the lane was demonstrably working.
            // A line STARTING with a braille char is spinner chrome; prose
            // does not start mid-word with braille.
            if ('\u{2800}'..='\u{28ff}').contains(&c) {
                return "active".into();
            }
        }
        // The ·/* spinner phases — ordinary chars, matched by SHAPE (AMUX-3426).
        if is_plain_spinner_line(s) {
            return "active".into();
        }
        if s.starts_with("Running\u{2026}") || reading_re.is_match(s) {
            return "active".into();
        }
    }
    // 1. Status bar in the bottom 3 lines.
    let mut status_bar = String::new();
    for l in lines[n.saturating_sub(3)..].iter().rev() {
        let ls = l.trim();
        let lsl = ls.to_lowercase();
        if ls.contains("\u{23f5}\u{23f5}") || lsl.contains("bypass permissions") || lsl.contains("plan mode") {
            status_bar = lsl;
            break;
        }
    }
    if status_bar.is_empty() {
        if clean.contains("Resume from summary") && clean.contains("Resume full session") {
            return "waiting".into();
        }
        for l in &lines[n.saturating_sub(5)..] {
            if l.to_lowercase().contains("esc to interrupt") {
                return "active".into();
            }
        }
    }
    // 2. Bottom-up scan of the last 12 lines.
    let completed_re = cached_re!(r" for \d+\s*[hms]\b");
    for l in lines[n.saturating_sub(12)..].iter().rev() {
        let s = l.trim();
        let sl = s.to_lowercase();
        if let Some(c) = s.chars().next() {
            if ('\u{2700}'..='\u{27bf}').contains(&c) && s.contains('\u{2026}') && !is_prompt_line(s) {
                return "active".into();
            }
            if ('\u{2700}'..='\u{27bf}').contains(&c)
                && !s.contains('\u{2026}')
                && completed_re.is_match(s)
                && !is_prompt_line(s)
            {
                return "idle".into();
            }
        }
        if s.starts_with("Running\u{2026}") || reading_re.is_match(s) {
            return "active".into();
        }
        // Waiting: selector cursor / numbered options with a footer hint.
        if (sl.contains("do you want") || sl.contains("would you like"))
            && (clean.contains("\u{276f} 1.") || clean.contains("1. Yes"))
        {
            return "waiting".into();
        }
        if sl.contains("esc to cancel") && (clean.contains("\u{276f} 1.") || sl.contains("enter to select")) {
            return "waiting".into();
        }
    }
    if clean.contains("\u{276f} 1.") || (clean.contains("\u{2502} \u{276f} 1.") ) {
        return "waiting".into();
    }
    // CODEX spells its selector cursor `›` (U+203A), not `❯` (U+276F) — found
    // live 2026-08-11 (AMUX-2913): a codex lane parked on its trust-directory
    // picker read `idle`, the exact needs-input-invisible failure AMUX-2834
    // fixed for Claude Code. Requires the footer hint alongside the cursor so
    // prose that merely QUOTES a numbered list cannot read as a picker (the
    // AMUX-2642 self-block class).
    let lower = clean.to_lowercase();
    if clean.contains("\u{203a} 1.")
        && (lower.contains("press enter to continue") || lower.contains("enter to select"))
    {
        return "waiting".into();
    }
    // GEMINI's picker cursor is `●` (U+25CF) inside a `│`-bordered box —
    // third provider, third selector spelling, found on the same day as the
    // codex one. The border char on the cursor's own line is the chrome
    // anchor prose cannot fake (same trick as the `│ ❯ 1.` claude form).
    if clean.contains("\u{2502} \u{25cf} 1.") {
        return "waiting".into();
    }
    if clean.contains('\u{276f}') {
        // About to settle on idle: if a generating-shaped line sits in frame
        // unclassified, that is the next AMUX-3426 — make it self-announce.
        warn_once_on_unrecognized_spinner(&lines);
        return "idle".into();
    }
    String::new()
}

/// Is the lane mid-turn according to its STATUS BAR — the only place that
/// string is a status rather than a word?
///
/// py:25650 tests `"esc to interrupt" in tmux_capture(name, 12)`, i.e. a
/// substring match over the whole pane. That is a content-dependent, permanent
/// self-block, and it cost amux-rust four hours on 2026-08-09: the lane was
/// genuinely idle (composer empty, status bar `⏵⏵ bypass permissions on
/// (shift+tab to cycle) · ← 2 agents`, self-report `idle`), but lines 26-27 of
/// its own pane were prose it had just written ABOUT the string —
/// `Workers with "bypass permissions on" + "esc to interrupt" on the status bar
/// were misdetected as IDLE`. Every steering delivery therefore refused with
/// "session started generating", the tick took one row per lane oldest-first
/// and stopped, and 10 messages sat queued for up to 229 minutes. The lane most
/// likely to hit this is the one working on the terminal-scraping code, which
/// is exactly the lane it hit.
///
/// So: look only at the bottom 3 non-blank lines. That also fixes the inverse
/// "esc to interrupt" on the STATUS BAR means the lane is generating — unless
/// the prompt ❯ is also visible, which means idle with background agents.
///
/// This is a scraper reading a rendered UI, so it is D1 debt either way — the
/// durable answer is the lane's own reported state, which `steer_lane_at_boundary`
/// already prefers. This narrows the fallback's blast radius; it does not make
/// the fallback good.
pub(crate) fn pane_bar_says_generating(raw_output: &str) -> bool {
    let clean = strip_ansi(raw_output);
    let nonblank: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
    let has_esc = nonblank
        .iter()
        .rev()
        .take(3)
        .any(|l| l.to_lowercase().contains("esc to interrupt"));
    // AN EMPTY COMPOSER DOES NOT MAKE IT IDLE. A peer's in-flight edit added a
    // `prompt_visible` exception on the theory that "empty ❯ + esc to interrupt
    // = idle with background agents". Measured against a lane provably mid-turn
    // (a 3000-word essay streaming), the frame is:
    //
    //   '  In medical science, twenty-three holds particular significance…'
    //   '────────────────────────────────────────────────────────────────'
    //   '❯\u{a0}'
    //   '────────────────────────────────────────────────────────────────'
    //   '  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← 2 agents'
    //
    // i.e. Claude Code ALWAYS draws its composer, empty, while generating — so
    // the exception describes the generating case, not the idle one. (The edit
    // also did not match its own fixture: `ends_with('❯')` is false for `❯\u{a0}`,
    // so its own test failed.)
    //
    // The ambiguous case it was reaching for is real — a lane can be idle while
    // background agents run — and it is settled by the SELF-REPORT, not the
    // pane: `steer_decide` reads the report first and only falls back here for
    // a hookless lane. For that fallback the asymmetry decides it. Reading an
    // ambiguous frame as BUSY costs at most `AMUX_STEER_MAX_AGE_S`, after which
    // the message is delivered anyway; reading it as IDLE types into a live turn,
    // which is the regression 336097d exists to prevent. So: fail closed.
    has_esc
}

/// py:18676 _clean_gemini_frame — keep only the LAST instance of each chrome
/// line class.
fn clean_gemini_frame(text: &str) -> String {
    let patterns = [
        r"^\s*workspace \(/directory\)",
        r"^\s*/model\s*$",
        r"no sandbox",
        r"^\s*Auto\s*$",
        r"^\s*YOLO Ctrl\+Y",
        r"^\s*\? for shortcuts",
        r"^\s*\d+ GEMINI\.md file",
    ];
    let lines: Vec<&str> = text.split('\n').collect();
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    let mut keep = vec![true; lines.len()];
    for p in patterns {
        let re = regex::Regex::new(p).unwrap();
        let idxs: Vec<usize> =
            plain.iter().enumerate().filter(|(_, pl)| re.is_match(pl)).map(|(i, _)| i).collect();
        for i in idxs.iter().take(idxs.len().saturating_sub(1)) {
            keep[*i] = false;
        }
    }
    lines
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, l)| *l)
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Claude-project JSONL plumbing (py:20535 _project_name, py:8166
// _iter_jsonl_tail, py:5483-5661 jsonl path resolution).
// ---------------------------------------------------------------------------

/// Claude's project-dir encoding: EVERY non-alphanumeric char becomes '-'.
fn project_name(work_dir: &str) -> String {
    let resolved = expanduser(work_dir)
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| expanduser(work_dir).to_string_lossy().into_owned());
    resolved.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Parsed entries from the tail of a JSONL file (bounded read).
pub(crate) fn iter_jsonl_tail(path: &Path, max_bytes: u64) -> Vec<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if size > max_bytes && f.seek(SeekFrom::Start(size - max_bytes)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if size > max_bytes {
        lines.next(); // discard partial first line
    }
    lines.filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect()
}

/// The `--resume`/`--name` flags for a claude launch (AMUX-2612).
///
/// Pure so the decision is testable: the branch it came from needs a session
/// env, a meta file and a transcript on disk to reach, which is why the
/// original defect had no test that could fail on it.
///
/// **`--name` rides along with `--resume`.** Dropping it was not a python port
/// regression — python's `session_flag` was either/or too — but it leaves a
/// renamed session's harness pinned to the name it was BORN with, forever,
/// because every subsequent start takes the resume branch. Live specimen:
/// session `amux` resumes conv 1dd2cd21, whose transcript carries
/// `customTitle: 'amux-rust'` on all 923 name records out to line 20748 — the
/// name it had before the rename. The rename cascade migrates env, meta, tmux
/// and every DB reference; the one thing it cannot reach is the running
/// harness's own idea of its name, and this is where that converges.
///
/// Measured, not assumed: `claude -p --resume <id> --name X` exits 0, does NOT
/// fork a new conversation, and APPENDS a fresh `customTitle: X` record to the
/// existing transcript. Line 0 keeps the original name for the life of the
/// file, which is why the reader half ([`transcript_display_name`]) must take
/// the LAST record and not the first — either half alone is inert.
fn claude_session_flag(name: &str, conv_id: &str, resumable: bool) -> String {
    if resumable && !conv_id.is_empty() {
        format!("--resume {conv_id} --name {}", sh_quote(name))
    } else {
        format!("--name {}", sh_quote(name))
    }
}

/// The display name a transcript CURRENTLY carries — the last `customTitle` /
/// `sessionName` record in the file, not the first (AMUX-2612).
///
/// A Claude transcript is append-only and `--name` writes a NEW record rather
/// than rewriting line 0, so the first line holds the name the conversation
/// was BORN with for the life of the file. Reading it was a silent staleness
/// bug with no expiry: session `amux` was renamed from `amux-rust` and its
/// transcript's line 0 still says 'amux-rust' 20,748 lines later. Anything
/// resolving a session by title therefore matched the dead name and could
/// never match the live one — and, worse, made the `--name` re-pin on the
/// resume path invisible, so fixing only the writer half would have looked
/// like it did nothing.
///
/// Bounded on purpose: the tail read is capped, and the first line is the
/// fallback when the cap contains no name record (a short or long-silent
/// transcript), so this is never more expensive than the scan it replaces by
/// more than one tail read.
fn transcript_display_name(jf: &Path) -> Option<String> {
    const NAME_TAIL_BYTES: u64 = 256 * 1024;
    let pick = |rec: &Value| -> Option<String> {
        for k in ["customTitle", "sessionName"] {
            if let Some(s) = rec.get(k).and_then(Value::as_str) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
        None
    };
    // Latest wins: iterate the tail forward and keep the last hit.
    let mut latest = None;
    for rec in iter_jsonl_tail(jf, NAME_TAIL_BYTES) {
        if let Some(n) = pick(&rec) {
            latest = Some(n);
        }
    }
    if latest.is_some() {
        return latest;
    }
    use std::io::BufRead;
    let f = std::fs::File::open(jf).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(f).read_line(&mut first).ok()?;
    pick(&serde_json::from_str::<Value>(&first).ok()?)
}

/// conv-id (transcript file stem) -> owning lane, from every session meta's
/// `cc_conversation_id` claim. TTL-cached: it reads ~113 small meta files and
/// its consumers run per fleet pass.
///
/// This is the CLAIM side of conversation ownership — a decision amux
/// recorded — as opposed to the transcript's own title records, which are
/// labels that go stale across a rename (session `amux` still titled
/// 'amux-rust' 20k lines later). AMUX-2612 established the precedence for
/// `session_jsonl_path`; `conversation_owner` below applies the same order to
/// every other reader so there is one answer to "whose conversation is this",
/// not four.
pub(crate) fn conversation_claims() -> std::collections::BTreeMap<String, String> {
    use std::sync::{Mutex, OnceLock};
    type Cache = (f64, std::collections::BTreeMap<String, String>);
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    let now = now_f64();
    let cache = C.get_or_init(|| Mutex::new((0.0, std::collections::BTreeMap::new())));
    if let Ok(g) = cache.lock() {
        if now - g.0 < 30.0 && !g.1.is_empty() {
            return g.1.clone();
        }
    }
    let mut map = std::collections::BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("env") {
                continue;
            }
            let Some(lane) = p.file_stem().and_then(|s| s.to_str()) else { continue };
            let cid = meta_str(&load_meta(lane), "cc_conversation_id");
            if !cid.is_empty() {
                map.insert(cid, lane.to_string());
            }
        }
    }
    if let Ok(mut g) = cache.lock() {
        *g = (now, map.clone());
    }
    map
}

/// The owning lane of a conversation transcript: the meta CLAIM first, the
/// transcript's LAST title record second, "" for an ad-hoc conversation amux
/// does not own.
///
/// Every reader of "which lane does this transcript belong to" goes through
/// here. Before this there were four spellings and three of them read the
/// FIRST line — reintroducing the staleness AMUX-2612 fixed, which is how the
/// `amux` lane's subagents, tokens and model were all being attributed to
/// `amux-rust`, its pre-rename name (Ethan, 2026-08-11: "ensure they're
/// always in sync").
pub(crate) fn conversation_owner(
    conv: &Path,
    claims: &std::collections::BTreeMap<String, String>,
) -> String {
    if let Some(stem) = conv.file_stem().and_then(|s| s.to_str()) {
        if let Some(lane) = claims.get(stem) {
            return lane.clone();
        }
    }
    transcript_display_name(conv).unwrap_or_default()
}

/// Model + total context tokens read straight out of a lane's own transcript.
///
/// WHY THE SERVER DOES THIS ITSELF (2026-08-11, AMUX-2829/AMUX-2676): the
/// reporting hook DOES extract both, correctly — verified end to end, an 86-byte
/// rich body with model and tokens. It reaches nobody. Every real lane posts a
/// 37/39/41-byte `{state, source}` body, which is the byte-exact shape of the
/// PREDECESSOR script, because CLAUDE CODE LOADS HOOK CONFIG AT SESSION START
/// and all ~47 lanes were started before settings.json was repointed. Measured
/// over 292 report POSTs: 0 rich bodies from a real session, 3 from my own
/// synthetic tests. Editing files on disk cannot fix that — the command string
/// is already baked into each running process.
///
/// So the evidence has to come from somewhere a running lane cannot withhold.
/// The transcript is the harness's own structured record, the server can already
/// resolve it per session, and reading it is not the terminal-scraping D1 warns
/// about: no rendered UI, no regex over a pane, just the JSONL the harness
/// writes. A REPORT still wins when one carries these fields — this only fills
/// the gap, so the D1 direction (the model reporting its own state) is intact
/// and this quietly stops being load-bearing as lanes restart.
///
/// Tail-bounded and TTL-cached: /api/sessions is polled hard and there are ~47
/// lanes, so an uncached full read would be a file scan per lane per poll.
pub(crate) fn transcript_evidence(name: &str) -> (Option<String>, Option<u64>) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    type Cache = HashMap<String, (f64, Option<String>, Option<u64>)>;
    static EV: OnceLock<Mutex<Cache>> = OnceLock::new();
    let ttl = std::env::var("AMUX_TRANSCRIPT_EVIDENCE_TTL_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(15.0);
    let now = now_f64();
    let cache = EV.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(g) = cache.lock() {
        if let Some((at, m, t)) = g.get(name) {
            if now - at < ttl {
                return (m.clone(), *t);
            }
        }
    }
    let (mut model, mut tokens) = (None, None);
    if let Some(path) = session_jsonl_path(name) {
        if let Ok(f) = std::fs::File::open(&path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            // Tail only. A long-running lane's transcript reaches hundreds of
            // MB; the last records are the only ones that describe NOW.
            const TAIL: u64 = 512_000;
            let mut buf = String::new();
            let mut rdr = std::io::BufReader::new(f);
            if len > TAIL {
                use std::io::Seek;
                let _ = rdr.seek(std::io::SeekFrom::Start(len - TAIL));
            }
            use std::io::Read;
            let _ = rdr.read_to_string(&mut buf);
            // Backwards: the LAST record that carries each field wins, and
            // stopping early avoids parsing the whole tail once both are found.
            for line in buf.lines().rev() {
                if model.is_some() && tokens.is_some() {
                    break;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
                let msg = &v["message"];
                if model.is_none() {
                    if let Some(m) = msg["model"].as_str().filter(|m| !m.is_empty()) {
                        model = Some(m.to_string());
                    }
                }
                if tokens.is_none() {
                    let u = if msg["usage"].is_object() { &msg["usage"] } else { &v["usage"] };
                    if u.is_object() {
                        let g = |k: &str| u[k].as_u64().unwrap_or(0);
                        let t = g("input_tokens")
                            + g("cache_read_input_tokens")
                            + g("cache_creation_input_tokens")
                            + g("output_tokens");
                        if t > 0 {
                            tokens = Some(t);
                        }
                    }
                }
            }
        }
    }
    if let Ok(mut g) = cache.lock() {
        g.insert(name.to_string(), (now, model.clone(), tokens));
    }
    (model, tokens)
}

/// Newest JSONL for a session (py:5590 _session_jsonl_path_uncached): meta
/// conv-id first, then title match, then the single unclaimed candidate.
pub(crate) fn session_jsonl_path(name: &str) -> Option<PathBuf> {
    let cfg = parse_env(name);
    let wd = cfg.get_or("CC_DIR", "").trim().to_string();
    if wd.is_empty() {
        return None;
    }
    let meta = load_meta(name);
    let conv_id = meta_str(&meta, "cc_conversation_id");
    let cc_cwd = meta_str(&meta, "cc_cwd");
    if !conv_id.is_empty() {
        for base in [cc_cwd.trim(), wd.as_str()] {
            if base.is_empty() {
                continue;
            }
            let cand = claude_home().join("projects").join(project_name(base)).join(format!("{conv_id}.jsonl"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let project_dir = claude_home().join("projects").join(project_name(&wd));
    let Ok(rd) = std::fs::read_dir(&project_dir) else { return None };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|p| p.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, p)))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.0));
    if files.is_empty() {
        return None;
    }
    if files.len() == 1 {
        return Some(files[0].1.clone());
    }
    // Exclude conversations claimed by SIBLING sessions; only a single
    // unclaimed candidate may be returned (shared-workdir bleed guard).
    //
    // Hoisted above the title match (AMUX-2612): it used to guard only the
    // single-unclaimed fallback below, so a session could still be handed a
    // conversation another session's meta EXPLICITLY claims, purely because a
    // stale display name matched. That is the reported incident — session
    // `amux` renamed from `amux-rust`, its live transcript still titled
    // 'amux-rust', so a lane by that name would resolve to `amux`'s own
    // conversation and every file `amux` edited was reported as "also edited
    // by amux-rust" on a clean self-authored commit. A name is a label; a meta
    // claim is a decision, and the decision outranks the label.
    let mut owned = std::collections::BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("env") {
                continue;
            }
            let Some(other) = p.file_stem().and_then(|s| s.to_str()) else { continue };
            if other == name {
                continue;
            }
            let ocid = meta_str(&load_meta(other), "cc_conversation_id");
            if !ocid.is_empty() {
                owned.insert(ocid);
            }
        }
    }
    for (_, jf) in &files {
        if jf.file_stem().and_then(|s| s.to_str()).map(|s| owned.contains(s)).unwrap_or(false) {
            continue;
        }
        if transcript_display_name(jf).as_deref() == Some(name) {
            return Some(jf.clone());
        }
    }
    let unclaimed: Vec<&PathBuf> = files
        .iter()
        .map(|(_, p)| p)
        .filter(|p| {
            p.file_stem().and_then(|s| s.to_str()).map(|s| !owned.contains(s)).unwrap_or(true)
        })
        .collect();
    if unclaimed.len() == 1 {
        Some(unclaimed[0].clone())
    } else {
        None
    }
}

/// All `rollout-*.jsonl` files under `~/.codex/sessions`, newest first.
/// Codex nests them `YYYY/MM/DD/`; the walk is depth-bounded (3) so it cannot
/// wander, and `session_jsonl_path`'s "newest mtime wins" discipline is applied
/// the same way here.
fn codex_rollout_files() -> Vec<(std::time::SystemTime, PathBuf)> {
    // Codex writes to the OS home (`~/.codex`), NOT amux's `home()` (`~/.amux`);
    // the two differ and the first cut pointed at `~/.amux/.codex`, so every
    // resolution missed, which the debug trace in `codex_transcript_events`
    // surfaced immediately (ethos rule 4). Same `$HOME`-based path shape as
    // `claude_home()`.
    let root = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex/sessions");
    let mut out: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(std::time::SystemTime, PathBuf)>) {
        if depth > 3 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, depth + 1, out);
            } else if p
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with("rollout-") && s.ends_with(".jsonl"))
                .unwrap_or(false)
            {
                if let Some(t) = p.metadata().ok().and_then(|m| m.modified().ok()) {
                    out.push((t, p));
                }
            }
        }
    }
    walk(&root, 0, &mut out);
    out.sort_by_key(|e| std::cmp::Reverse(e.0));
    out
}

/// The `cwd` a rollout was recorded in, read from its first line
/// (`session_meta`), so this stats one small read per candidate, not the whole
/// file.
fn rollout_cwd(path: &Path) -> Option<String> {
    let Ok(f) = std::fs::File::open(path) else { return None };
    let mut reader = std::io::BufReader::new(f);
    let mut line = String::new();
    use std::io::BufRead;
    reader.read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    v.pointer("/payload/cwd")
        .and_then(|c| c.as_str())
        .map(str::to_string)
}

/// Map a codex/ollama worker to its live Codex rollout file. Mirrors
/// `session_jsonl_path`'s resolution order: a recorded session-id claim first
/// (deterministic: the id is the filename UUID), then the worker's `CC_DIR`
/// cwd with newest mtime winning. `None` when nothing matches, which makes the
/// caller fall back to the raw terminal (never a wrong transcript).
pub(crate) fn codex_rollout_path(name: &str) -> Option<PathBuf> {
    let cfg = parse_env(name);
    let wd = cfg.get_or("CC_DIR", "").trim().to_string();
    let sid = meta_str(&load_meta(name), "codex_session_id");
    let files = codex_rollout_files();
    if files.is_empty() {
        return None;
    }
    // 1. session-id claim: the id is embedded in the filename, so this is a
    //    string match, no file read.
    if !sid.is_empty() {
        if let Some((_, p)) = files.iter().find(|(_, p)| {
            p.file_name().and_then(|s| s.to_str()).map(|s| s.contains(&sid)).unwrap_or(false)
        }) {
            return Some(p.clone());
        }
    }
    // 2. cwd match, newest first. Bounded to the most recent candidates so a
    //    years-deep history never turns one tab-open into thousands of reads.
    if !wd.is_empty() {
        for (_, p) in files.iter().take(80) {
            if rollout_cwd(p).as_deref() == Some(wd.as_str()) {
                return Some(p.clone());
            }
        }
    }
    None
}

/// A codex/ollama worker's transcript as the uniform [`TranscriptEvent`] model
/// (AMUX-3201 slice 1). Reads a bounded tail of the rollout so a long session
/// stays cheap, parses it via the shared `opencode::events` projection, and
/// keeps the last `max_events` so the client renders the live end of the
/// conversation, not megabytes of history.
fn codex_transcript_events(
    name: &str,
    max_events: usize,
) -> Option<Vec<crate::opencode::events::TranscriptEvent>> {
    let Some(path) = codex_rollout_path(name) else {
        // Self-announcing (ethos rule 4): a codex/ollama worker whose rollout
        // can't be resolved returns an EMPTY transcript that looks identical to
        // "the worker hasn't spoken yet". Trace it so "why is my codex
        // transcript blank" is answerable from ~/.amux/logs, not a guess.
        tracing::debug!(
            session = %name,
            "codex/ollama transcript: no rollout file resolved (cwd/session-id miss); peek falls back to raw terminal"
        );
        return None;
    };
    let lines = iter_jsonl_tail(&path, 4_000_000);
    let mut evs = crate::opencode::events::codex_rollout_transcript(&lines);
    if evs.len() > max_events {
        evs = evs.split_off(evs.len() - max_events);
    }
    Some(evs)
}

// ---------------------------------------------------------------------------
// Markdown → ANSI transcript renderer (py:5675-5957). Fail-safe: any panic
// risk is avoided structurally; the table renderer clamps widths.
// ---------------------------------------------------------------------------

const MD_BASE: &str = "\x1b[39m";

fn md_inline(s: &str) -> String {
    use std::sync::OnceLock;
    static CODE: OnceLock<regex::Regex> = OnceLock::new();
    static BOLD1: OnceLock<regex::Regex> = OnceLock::new();
    static BOLD2: OnceLock<regex::Regex> = OnceLock::new();
    let code = CODE.get_or_init(|| regex::Regex::new(r"`([^`\n]+)`").unwrap());
    let bold1 = BOLD1.get_or_init(|| regex::Regex::new(r"\*\*([^*\n]+?)\*\*").unwrap());
    let bold2 = BOLD2.get_or_init(|| regex::Regex::new(r"__([^_\n]+?)__").unwrap());
    let s = code.replace_all(s, format!("\x1b[38;5;153m$1{MD_BASE}").as_str());
    let s = bold1.replace_all(&s, "\x1b[1m$1\x1b[22m");
    let s = bold2.replace_all(&s, "\x1b[1m$1\x1b[22m");
    // Italic (Python uses lookarounds the regex crate lacks); the simple
    // single-star form is rare in transcripts — bold/code carry the weight.
    s.into_owned()
}

fn md_table_sep_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^\s*\|?\s*:?-{2,}:?\s*(?:\|\s*:?-{2,}:?\s*)+\|?\s*$").unwrap()
    })
}

fn md_render_table(block: &[&str], max_width: usize) -> String {
    fn cells(row: &str) -> Vec<String> {
        let mut r = row.trim();
        r = r.strip_prefix('|').unwrap_or(r);
        r = r.strip_suffix('|').unwrap_or(r);
        r.split('|').map(|c| c.trim().to_string()).collect()
    }
    fn strip_md(s: &str) -> String {
        let re1 = cached_re!(r"`([^`]+)`");
        let re2 = cached_re!(r"\*\*([^*]+)\*\*");
        let re3 = cached_re!(r"__([^_]+)__");
        let s = re1.replace_all(s, "$1");
        let s = re2.replace_all(&s, "$1");
        re3.replace_all(&s, "$1").into_owned()
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    rows.push(cells(block[0]).iter().map(|c| strip_md(c)).collect());
    for r in &block[2..] {
        rows.push(cells(r).iter().map(|c| strip_md(c)).collect());
    }
    let ncol = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncol == 0 {
        return block.join("\n");
    }
    for r in rows.iter_mut() {
        r.resize(ncol, String::new());
    }
    let natural: Vec<usize> = (0..ncol)
        .map(|ci| rows.iter().map(|r| r[ci].chars().count()).max().unwrap_or(0))
        .collect();
    let avail = std::cmp::max(ncol * 6, max_width.saturating_sub(3 * ncol + 1));
    let mut widths = natural.clone();
    let mut guard = 0;
    while widths.iter().sum::<usize>() > avail && guard < 10000 {
        guard += 1;
        let mx = (0..ncol).max_by_key(|c| widths[*c]).unwrap();
        if widths[mx] <= 6 {
            break;
        }
        widths[mx] -= 1;
    }
    for w in widths.iter_mut() {
        if *w == 0 {
            *w = 1;
        }
    }
    fn wrap_cell(text: &str, w: usize) -> Vec<String> {
        if text.is_empty() {
            return vec![String::new()];
        }
        let mut out = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let wl = word.chars().count();
            if line.is_empty() {
                if wl <= w {
                    line = word.to_string();
                } else {
                    for chunk in word.chars().collect::<Vec<_>>().chunks(w) {
                        out.push(chunk.iter().collect());
                    }
                }
            } else if line.chars().count() + 1 + wl <= w {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                if wl <= w {
                    line = word.to_string();
                } else {
                    for chunk in word.chars().collect::<Vec<_>>().chunks(w) {
                        out.push(chunk.iter().collect());
                    }
                }
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
        if out.is_empty() {
            out.push(String::new());
        }
        out
    }
    let render_row = |cs: &[String], header: bool| -> String {
        let wrapped: Vec<Vec<String>> = (0..ncol).map(|ci| wrap_cell(&cs[ci], widths[ci])).collect();
        let h = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);
        let mut out = Vec::new();
        for k in 0..h {
            let mut parts = Vec::new();
            for (ci, cellw) in wrapped.iter().enumerate() {
                let seg = cellw.get(k).cloned().unwrap_or_default();
                let pad = format!("{}{}", seg, " ".repeat(widths[ci].saturating_sub(seg.chars().count())));
                if header && !seg.is_empty() {
                    parts.push(format!("\x1b[1m{pad}\x1b[22m"));
                } else {
                    parts.push(pad);
                }
            }
            out.push(format!("\u{2502} {} \u{2502}", parts.join(" \u{2502} ")));
        }
        out.join("\n")
    };
    let bar = |l: char, m: char, r: char| -> String {
        let mid: Vec<String> = widths.iter().map(|w| "\u{2500}".repeat(w + 2)).collect();
        format!("{l}{}{r}", mid.join(&m.to_string()))
    };
    let mut res = vec![bar('\u{250c}', '\u{252c}', '\u{2510}'), render_row(&rows[0], true), bar('\u{251c}', '\u{253c}', '\u{2524}')];
    for r in &rows[1..] {
        res.push(render_row(r, false));
    }
    res.push(bar('\u{2514}', '\u{2534}', '\u{2518}'));
    res.join("\n")
}

fn md_to_ansi(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let header_re = cached_re!(r"^(#{1,6})\s+(.*)$");
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let ln = lines[i];
        if ln.trim().starts_with('|')
            && ln.matches('|').count() >= 2
            && i + 1 < lines.len()
            && md_table_sep_re().is_match(lines[i + 1])
        {
            let mut blk = vec![ln, lines[i + 1]];
            let mut j = i + 2;
            while j < lines.len() && lines[j].trim().starts_with('|') && lines[j].matches('|').count() >= 2 {
                blk.push(lines[j]);
                j += 1;
            }
            out.push(md_render_table(&blk, 100));
            i = j;
            continue;
        }
        if let Some(c) = header_re.captures(ln) {
            out.push(format!("\x1b[1m{}\x1b[22m", md_inline(&c[2])));
            i += 1;
            continue;
        }
        out.push(md_inline(ln));
        i += 1;
    }
    out.join("\n")
}

fn user_echo_ansi(txt: &str) -> String {
    format!(
        "\x1b[38;5;239m\x1b[48;5;237m\u{276f} \x1b[38;5;231m{}\x1b[39m\x1b[49m",
        txt.replace('\n', "\n  ")
    )
}

fn tool_brief(inp: &Value) -> String {
    let Some(obj) = inp.as_object() else { return String::new() };
    for k in ["command", "file_path", "path", "pattern", "query", "url", "prompt", "description", "old_string"] {
        if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
            let v = v.replace('\n', " ").trim().to_string();
            if !v.is_empty() {
                let t = chars_truncate(&v, 90);
                return if v.chars().count() > 90 { format!("{t}\u{2026}") } else { t };
            }
        }
    }
    String::new()
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| {
                if let Some(s) = x.as_str() {
                    Some(s.to_string())
                } else if x["type"] == json!("text") {
                    x["text"].as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// py:5833 _render_session_transcript — clean ANSI render of the JSONL tail.
/// The worker's most recent FULL assistant message, text only — no tool calls,
/// no thinking blocks, no UI chrome. This is what read-aloud sends (Ethan:
/// "read the most recent full message from the worker ... be intelligent about
/// what to send"): the last assistant turn that actually SAID something, taken
/// from the STRUCTURED transcript rather than a scrape of the rendered pane
/// (which carries spinners and "✻ Churned for 2m" status lines that must never
/// be read aloud). Reuses the same `session_jsonl_path` + `iter_jsonl_tail` the
/// transcript renderer uses, so it cannot disagree with it about where the
/// transcript is or how it is parsed (D1: a real interface, not a scrape).
fn last_assistant_message(name: &str, max_chars: usize) -> String {
    let Some(path) = session_jsonl_path(name) else { return String::new() };
    let mut last = String::new();
    for o in iter_jsonl_tail(&path, 5_000_000) {
        if o["type"].as_str() != Some("assistant") {
            continue;
        }
        let Some(msg) = o.get("message").and_then(|m| m.as_object()) else { continue };
        let blocks: Vec<Value> = match msg.get("content") {
            Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
            Some(Value::Array(a)) => a.clone(),
            _ => continue,
        };
        let mut parts: Vec<String> = Vec::new();
        for b in &blocks {
            // text only — skip tool_use, tool_result, thinking.
            if b["type"].as_str() == Some("text") {
                let t = b["text"].as_str().unwrap_or("").trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
        // iter_jsonl_tail is chronological, so overwriting ends on the LAST
        // assistant turn that had text. A turn that was pure tool_use/thinking
        // (no text block) is skipped rather than blanking the result.
        if !parts.is_empty() {
            last = parts.join("\n\n");
        }
    }
    last.chars().take(max_chars).collect()
}

fn render_session_transcript(name: &str, max_chars: usize) -> String {
    let Some(path) = session_jsonl_path(name) else { return String::new() };
    let max_read = std::cmp::max(max_chars * 5, 5_000_000) as u64;
    let mut out: Vec<String> = Vec::new();
    let sysrem = cached_re!(r"(?s)<system-reminder>.*?</system-reminder>");
    let tasknote = cached_re!(r"(?s)<task-notification>.*?</task-notification>");
    let caveat = cached_re!(r"(?s)<local-command-caveat>.*?</local-command-caveat>");
    let cmd_re = cached_re!(r"(?s)<command-name>(.*?)</command-name>");
    let arg_re = cached_re!(r"(?s)<command-args>(.*?)</command-args>");
    let out_re = cached_re!(r"(?s)<local-command-stdout>(.*?)</local-command-stdout>");
    for o in iter_jsonl_tail(&path, max_read) {
        let t = o["type"].as_str().unwrap_or("");
        if t != "user" && t != "assistant" {
            continue;
        }
        let Some(msg) = o.get("message").and_then(|m| m.as_object()) else { continue };
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let blocks: Vec<Value> = match msg.get("content") {
            Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
            Some(Value::Array(a)) => a.clone(),
            _ => continue,
        };
        for b in blocks {
            let bt = b["type"].as_str().unwrap_or("");
            match bt {
                "text" => {
                    let mut txt = b["text"].as_str().unwrap_or("").trim().to_string();
                    if txt.is_empty() {
                        continue;
                    }
                    if role == "user" {
                        if txt.contains("<system-reminder>")
                            || txt.contains("<task-notification>")
                            || txt.contains("<local-command-caveat>")
                        {
                            txt = sysrem.replace_all(&txt, "").into_owned();
                            txt = tasknote.replace_all(&txt, "").into_owned();
                            txt = caveat.replace_all(&txt, "").into_owned();
                            txt = txt.trim().to_string();
                            if txt.is_empty() {
                                continue;
                            }
                        }
                        let m_cmd = cmd_re.captures(&txt);
                        let m_out = out_re.captures(&txt);
                        if m_cmd.is_some() || m_out.is_some() {
                            if let Some(mc) = &m_cmd {
                                let mut cmd_line = mc[1].trim().to_string();
                                if !cmd_line.is_empty() {
                                    if let Some(ma) = arg_re.captures(&txt) {
                                        let a = ma[1].trim();
                                        if !a.is_empty() {
                                            cmd_line = format!("{cmd_line} {a}");
                                        }
                                    }
                                    out.push(user_echo_ansi(&cmd_line));
                                }
                            }
                            if let Some(mo) = &m_out {
                                let body = mo[1].trim();
                                if !body.is_empty() {
                                    for (k, ln) in body.split('\n').take(6).enumerate() {
                                        let prefix = if k == 0 { "  \u{23bf}  " } else { "     " };
                                        out.push(format!("\x1b[38;5;246m{}{}\x1b[0m", prefix, ln.trim_end()));
                                    }
                                }
                            }
                            out.push(String::new());
                            continue;
                        }
                        out.push(user_echo_ansi(&txt));
                    } else {
                        let body = md_to_ansi(&txt).replace('\n', "\n  ");
                        out.push(format!("\x1b[38;5;231m\u{23fa}\x1b[39m {body}\x1b[0m"));
                    }
                    out.push(String::new());
                }
                "tool_use" => {
                    let nm = b["name"].as_str().unwrap_or("tool");
                    let arg = tool_brief(&b["input"]);
                    let suffix = if arg.is_empty() { String::new() } else { format!("({arg})") };
                    out.push(format!("\x1b[38;5;114m\u{23fa}\x1b[39m \x1b[1m{nm}\x1b[0m{suffix}"));
                }
                "tool_result" => {
                    let raw = tool_result_text(&b["content"]);
                    let mut rlines: Vec<&str> = raw.split('\n').map(|l| l.trim_end()).collect();
                    while rlines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
                        rlines.remove(0);
                    }
                    while rlines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                        rlines.pop();
                    }
                    if !rlines.is_empty() {
                        const MAXL: usize = 6;
                        const MAXW: usize = 200;
                        for (k, ln) in rlines.iter().take(MAXL).enumerate() {
                            let mut ln = (*ln).to_string();
                            if ln.chars().count() > MAXW {
                                ln = format!("{}\u{2026}", chars_truncate(&ln, MAXW));
                            }
                            let prefix = if k == 0 { "  \u{23bf}  " } else { "     " };
                            out.push(format!("\x1b[38;5;246m{prefix}{ln}\x1b[0m"));
                        }
                        if rlines.len() > MAXL {
                            let extra = rlines.len() - MAXL;
                            let word = if extra != 1 { " more lines" } else { " more line" };
                            out.push(format!("\x1b[38;5;246m     \u{2026} +{extra}{word}\x1b[0m"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut text = out.join("\n").trim_matches('\n').to_string();
    if text.chars().count() > max_chars {
        let chars: Vec<char> = text.chars().collect();
        text = chars[chars.len() - max_chars..].iter().collect();
        if let Some(nl) = text.find('\n') {
            if nl > 0 {
                text = text[nl + 1..].to_string();
            }
        }
    }
    text
}

// ---------------------------------------------------------------------------
// Flag-string helpers (py:22390-22614): shlex-equivalent split/quote, model /
// effort / yolo manipulation. split_flags errs on unbalanced quotes exactly
// where Python's shlex raises ValueError (the "don't wipe the user's flags"
// contract).
// ---------------------------------------------------------------------------

pub const SESSION_PROVIDERS: [&str; 5] = ["claude", "codex", "gemini", "iterm2", "ollama"];
const PROVIDER_YOLO_FLAGS: [&str; 3] = [
    "--dangerously-skip-permissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "--yolo",
];
const VALID_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn split_flags(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err("No closing quotation".into()),
                        },
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '\\' => match chars.next() {
                Some(x) => {
                    in_word = true;
                    cur.push(x);
                }
                None => return Err("No escaped character".into()),
            },
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// POSIX single-quote escaping (shlex.quote parity).
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b'@' | b'%' | b'+' | b',')
        })
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn shell_quote_flags(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    match split_flags(s) {
        Ok(tokens) => tokens.iter().map(|t| sh_quote(t)).collect::<Vec<_>>().join(" "),
        Err(_) => sh_quote(s),
    }
}

fn strip_token_from_flags(flags: &str, flag: &str) -> Result<String, String> {
    if flags.is_empty() {
        return Ok(String::new());
    }
    let tokens = split_flags(flags)?;
    let eq_form = format!("{flag}=");
    let mut filtered = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == flag {
            // Value-aware: consume the next token as this flag's value only
            // when it is not itself a flag. Stripping a boolean flag (e.g.
            // --dangerously-skip-permissions) must never eat its neighbour —
            // that would silently delete an unrelated flag (found while fixing
            // the duplicate --model incident, 2026-08-09).
            if i + 1 < tokens.len() && !tokens[i + 1].starts_with('-') {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if t.starts_with(&eq_form) {
            i += 1;
            continue;
        }
        filtered.push(t.clone());
        i += 1;
    }
    Ok(filtered.iter().map(|t| sh_quote(t)).collect::<Vec<_>>().join(" "))
}

fn strip_model_from_flags(flags: &str) -> Result<String, String> {
    strip_token_from_flags(flags, "--model")
}

/// Flag names present in a flags string: `--x v` and `--x=v` both yield `--x`.
fn flag_names(flags: &str) -> Vec<String> {
    let Ok(tokens) = split_flags(flags) else { return Vec::new() };
    tokens
        .iter()
        .filter(|t| t.starts_with('-'))
        .map(|t| t.split('=').next().unwrap_or(t).to_string())
        .collect()
}

/// Defaults are defaults: any flag name the session (CC_FLAGS), the resume
/// choreography (session_flag) or the caller (extra_flags) already carries is
/// stripped from CC_DEFAULT_FLAGS before assembly, so the session's value is
/// the only one on the command line.
///
/// Why (2026-08-09 incident): defaults.env carried `--model claude-opus-4-6`
/// and the naive concat produced `claude --model claude-opus-4-6 --model
/// claude-fable-5 ...` fleet-wide (Claude Code last-wins, so sessions ran the
/// right model by argument-ORDER luck, and any future reordering or a parser
/// that rejects duplicates flips every session's model silently). Generic by
/// token name — --model, --effort, --max-tokens, whatever defaults.env grows
/// next — session wins, never the default. Python has the same naive concat
/// (amux-server.py:24498); fixed right here per the port mandate.
fn dedupe_default_flags(default_flags: &str, overrides: &[&str]) -> String {
    let mut out = default_flags.to_string();
    for src in overrides {
        for name in flag_names(src) {
            match strip_token_from_flags(&out, &name) {
                Ok(next) => out = next,
                // Malformed defaults: leave as-is; build_claude_cmd quotes the
                // raw string verbatim, same as before this dedupe existed.
                Err(_) => return out,
            }
        }
    }
    out
}

/// The value carried by `--name` in a flags string; `--name v` and `--name=v`
/// both. Empty when the flag is absent or the flags do not parse.
fn flag_value(flags: &str, name: &str) -> String {
    let Ok(tokens) = split_flags(flags) else { return String::new() };
    let eq = format!("{name}=");
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == name && i + 1 < tokens.len() {
            return tokens[i + 1].clone();
        }
        if let Some(v) = tokens[i].strip_prefix(&eq) {
            return v.to_string();
        }
        i += 1;
    }
    String::new()
}

fn extract_model_from_flags(flags: &str) -> String {
    flag_value(flags, "--model")
}

const MODEL_ID_MAX_LEN: usize = 100;

fn validate_model_name(value: &Value) -> Result<String, String> {
    let Some(s) = value.as_str() else { return Err("model must be a string".into()) };
    let normalized = s.trim().to_string();
    if normalized.chars().count() > MODEL_ID_MAX_LEN {
        return Err(format!("model name too long (max {MODEL_ID_MAX_LEN} chars)"));
    }
    let re = cached_re!(r"^[A-Za-z0-9._:\[\]@/+][A-Za-z0-9._:\[\]@/+\-]*$");
    if !normalized.is_empty() && !re.is_match(&normalized) {
        return Err("invalid model name (allowed: alphanumeric and ._:[]@/+-, no leading hyphen)".into());
    }
    Ok(normalized)
}

fn validate_effort(value: &Value) -> Result<String, String> {
    let Some(s) = value.as_str() else { return Err("effort must be a string".into()) };
    let normalized = s.trim().to_lowercase();
    if !normalized.is_empty() && !VALID_EFFORTS.contains(&normalized.as_str()) {
        return Err(format!("invalid effort (allowed: {})", VALID_EFFORTS.join(", ")));
    }
    Ok(normalized)
}

fn set_effort_flag(flags: &str, effort: &str) -> Result<String, String> {
    let base = strip_token_from_flags(flags, "--effort")?;
    if effort.is_empty() {
        return Ok(base);
    }
    Ok(if base.is_empty() { format!("--effort {effort}") } else { format!("{base} --effort {effort}") })
}

fn provider_yolo_flag(provider: &str) -> &'static str {
    match provider {
        "codex" => "--dangerously-bypass-approvals-and-sandbox",
        "gemini" => "--yolo",
        _ => "--dangerously-skip-permissions",
    }
}

fn strip_provider_yolo_flags(flags: &str) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let Ok(tokens) = split_flags(flags) else {
        let mut out = flags.to_string();
        for f in PROVIDER_YOLO_FLAGS {
            out = out.replace(f, "");
        }
        let re = cached_re!(r"--approval-mode(?:=|\s+)yolo\b");
        return re.replace_all(&out, "").trim().to_string();
    };
    let mut filtered = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if PROVIDER_YOLO_FLAGS.contains(&t.as_str()) {
            i += 1;
            continue;
        }
        if t == "--approval-mode" && i + 1 < tokens.len() && tokens[i + 1] == "yolo" {
            i += 2;
            continue;
        }
        if t == "--approval-mode=yolo" {
            i += 1;
            continue;
        }
        filtered.push(t.clone());
        i += 1;
    }
    filtered.iter().map(|t| sh_quote(t)).collect::<Vec<_>>().join(" ")
}

/// Standing order (Ethan 2026-08-11: "whenever idle, take care of any
/// non-terminal board task"): the board-drive continue-nudge is ON by
/// default; CC_AUTO_CONTINUE=0 opts a lane out. Ethos rule 1 — prefer
/// opt-out for anything that expands what a session can do; the opt-in
/// version reached 2 lanes of ~50.
///
/// ONE predicate for the mechanism (board_drive) and the view
/// (/api/sessions `auto_continue`): a view that disagrees with the
/// mechanism it describes is worse than no view.
pub(crate) fn auto_continue_on(val: Option<&str>) -> bool {
    !matches!(
        val.map(|v| v.trim().to_lowercase()).as_deref(),
        Some("0") | Some("false") | Some("no") | Some("off")
    )
}

/// Is a standing order live for this lane? Scoped worker > group > global,
/// with `CC_STANDING_ORDERS` as the master off-switch (AMUX-2930).
///
/// Ethan, 2026-08-11: "I should be able to shut off standing orders like 'Hey
/// you have stuff in your to-do. Keep going.' … on the group, global, or
/// individual worker level … configurable but also obviously have defaults."
///
/// TWO LEVELS ON PURPOSE, and no more. `CC_STANDING_ORDERS=0` silences every
/// board-drive nudge at whatever scope it is set — one knob, the one to reach
/// for. The per-class keys (`CC_AUTO_PICKUP`, `CC_AUTO_CONTINUE`) stay for the
/// finer cut, and are checked only when the master has not already said no.
/// A third spelling would be a second way to express the same thing, which is
/// how the board keeps growing predicates that disagree.
///
/// DEFAULT IS ON, at every level, and that is deliberate rather than inherited:
/// ethos rule 1 — the opt-IN version of auto-continue reached 2 lanes out of
/// ~50, so a capability nobody is enrolled in is decoration. Off must be a
/// choice someone made, and now it is a choice they can make once, for
/// everyone, instead of 50 times.
pub fn standing_orders_on(lane: &str, key: &str) -> bool {
    standing_orders_on_in(&home(), lane, key)
}

/// Same, over an explicit home — see [`scoped_setting_in`] for why.
pub fn standing_orders_on_in(home: &std::path::Path, lane: &str, key: &str) -> bool {
    if !auto_continue_on(scoped_setting_in(home, lane, "CC_STANDING_ORDERS").as_deref()) {
        return false;
    }
    auto_continue_on(scoped_setting_in(home, lane, key).as_deref())
}

/// THE predicate for "is this lane in YOLO mode". Public so the sessions
/// payload can serialize the very verdict the toggle acts on, instead of the
/// SPA re-deriving it — a view that re-derives a predicate drifts from it the
/// moment either side changes, and this one did.
///
/// The bug that made this public: the SPA computed its badge as
/// `flags.includes(skipPermissions) || !!s.auto_continue`, and `auto_continue`
/// in the payload is `standing_orders_on(...)`, which is DEFAULT-ON at every
/// level. So a lane with no skip-permissions flag at all rendered a YOLO badge,
/// and a worker sat blocked on "This command requires approval" for 11 hours
/// while its card claimed it would never stop to ask.
///
/// Note the comment below already forbade exactly that, server-side, and was
/// right: the default-on nudge must never imply skip-permissions. The client
/// did it anyway, through a field carrying the default-on value. That is why
/// the fix is to SHARE the predicate rather than to restate it correctly in a
/// second place.
pub fn yolo_enabled(flags: &str, cc_auto_continue: Option<&str>) -> bool {
    PROVIDER_YOLO_FLAGS.iter().any(|f| flags.contains(f))
        || flags.contains("--approval-mode=yolo")
        || flags.contains("--approval-mode yolo")
        // The EXPLICIT flag, deliberately NOT auto_continue_on(): setting
        // CC_AUTO_CONTINUE=1 asks for a worker that never stops, which
        // implies skip-permissions. The DEFAULT-on nudge must not — routing
        // the default through here would flip the whole fleet to YOLO as a
        // side effect of a scheduling change.
        || matches!(cc_auto_continue, Some("1" | "true" | "yes"))
}

fn is_yolo_enabled(flags: &str, cfg: &EnvFile) -> bool {
    yolo_enabled(flags, cfg.get("CC_AUTO_CONTINUE"))
}

fn default_model_for_provider(provider: &str) -> String {
    match provider {
        "codex" => "gpt-5.5".into(),
        "gemini" => "auto".into(),
        // Ollama runs via `codex --oss --local-provider ollama --model <model>`.
        // Read from the ONE source rather than repeating the literal: this arm
        // and `OllamaAdapter::default` were two spellings of the same fact, and
        // the comment that used to sit here ("this box has qwen3.8:27b pulled")
        // was a fact about one machine compiled into a public server. See
        // `static_providers::ollama_default_model` (DESKT-6).
        "ollama" => crate::provider::static_providers::ollama_default_model(),
        _ => get_default_model(),
    }
}

/// The base binary the LAUNCH BUILDER invokes for a provider — the first token
/// of the command the launch match below emits. This is the SINGLE SOURCE: the
/// launch arms build their command from it, and the `provider.launch_matches_adapter`
/// health invariant reads the SAME function, so the check cannot drift from the
/// launcher (the invariants module's same-source rule).
///
/// RR-0043 / AMUX-3153: ollama moved from a bare `ollama run` REPL to
/// `codex --oss --local-provider ollama`, but only the CLI and the provider
/// ADAPTER were migrated — the server launch arm was left on `ollama run` while
/// the adapter advertised hooks=true, so a dashboard-launched worker got a
/// hookless REPL and the capability report lied. Nothing joined the launcher to
/// the adapter to notice. Mapping the launch binary here, and asserting it
/// against the adapter, is what makes the next such divergence self-announce.
pub fn launch_base_binary(provider: &str) -> &'static str {
    match provider {
        // ollama runs codex under the hood (`--oss --local-provider ollama`).
        "codex" | "ollama" => "codex",
        "gemini" => "gemini",
        // claude, iterm2, and anything unknown launch via build_claude_cmd,
        // whose default binary is `claude` (overridable by AMUX_CLAUDE_CMD).
        _ => "claude",
    }
}

fn provider_label(provider: &str) -> &str {
    match provider {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "gemini" => "Gemini",
        "iterm2" => "iTerm2",
        "ollama" => "Ollama",
        other => {
            if other.is_empty() {
                "Claude Code"
            } else {
                other
            }
        }
    }
}

fn get_default_model() -> String {
    let defaults = EnvFile::load(&home().join("defaults.env"));
    let m = extract_model_from_flags(defaults.get_or("CC_DEFAULT_FLAGS", ""));
    if m.is_empty() { "sonnet".into() } else { m }
}

// ---------------------------------------------------------------------------
// Shared-DB helpers. All writes ride the store's writer thread; every table
// is CREATEd IF NOT EXISTS first so a fresh Rust-only AMUX_HOME (unit tests)
// works — on the live shared DB these are no-ops against Python's schema.
// ---------------------------------------------------------------------------

fn ensure_fleet_tables(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL NOT NULL,
            session TEXT NOT NULL DEFAULT '', type TEXT NOT NULL,
            data TEXT, idem TEXT, source TEXT NOT NULL DEFAULT '');
         CREATE UNIQUE INDEX IF NOT EXISTS idx_sev_idem ON session_events(idem) WHERE idem IS NOT NULL;
         CREATE TABLE IF NOT EXISTS steering_queue (
            id TEXT PRIMARY KEY, session TEXT NOT NULL, text TEXT NOT NULL,
            queued_at REAL NOT NULL, guard TEXT);
         CREATE TABLE IF NOT EXISTS steering_history (
            id TEXT PRIMARY KEY, session TEXT NOT NULL, text TEXT NOT NULL,
            queued_at REAL, delivered_at REAL NOT NULL);
         CREATE TABLE IF NOT EXISTS share_tokens (
            token TEXT PRIMARY KEY, session TEXT NOT NULL,
            perms TEXT NOT NULL DEFAULT 'output', created_at INTEGER NOT NULL,
            expires_at INTEGER, label TEXT NOT NULL DEFAULT '');
         CREATE TABLE IF NOT EXISTS send_dedup (
            session TEXT NOT NULL, msg_id TEXT NOT NULL, ts INTEGER NOT NULL,
            PRIMARY KEY (session, msg_id));
         CREATE TABLE IF NOT EXISTS cmd_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL,
            type TEXT NOT NULL DEFAULT 'direct', session TEXT NOT NULL DEFAULT '',
            ts INTEGER NOT NULL, origin TEXT NOT NULL DEFAULT '');
         CREATE TABLE IF NOT EXISTS prefs (key TEXT PRIMARY KEY, value TEXT);",
    )?;
    // Python's steering_queue predates `guard` and gained it via ALTER; a DB
    // created by Python's schema block lacks it. Add-if-missing, ignore
    // "duplicate column".
    let _ = conn.execute("ALTER TABLE steering_queue ADD COLUMN guard TEXT", []);
    // WHO SENT IT (AMUX-2785). A stalled queue's one actionable notification is
    // to the SENDER — they hold the false belief ("I sent it") and they are the
    // only party who can act — and until now the row did not record who that
    // was, so the stall warning had nobody to tell and went to a log instead.
    let _ = conn.execute("ALTER TABLE steering_queue ADD COLUMN sender TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE cmd_history ADD COLUMN origin TEXT NOT NULL DEFAULT ''", []);
    // Dead-lettered vs delivered must be distinguishable in history
    // (AMUX-3110; migration 0024 carries the same ADDCOL for migrated DBs,
    // this covers a DB bootstrapped through this legacy path).
    let _ = conn.execute("ALTER TABLE steering_history ADD COLUMN outcome TEXT", []);
    Ok(())
}

use crate::config::now_f64;
fn now_i64() -> i64 {
    now_f64() as i64
}

/// py:7593 _emit_event — append-only, idempotent on `idem`, never fails the
/// caller.
pub(crate) async fn emit_event(state: &AppState, session: &str, etype: &str, data: Option<Value>, idem: Option<String>, source: &str) {
    emit_event_store(&state.store, session, etype, data, idem, source).await
}

/// [`emit_event`] addressed by store — see [`steer_enqueue_store`] for why
/// both variants exist rather than a second copy of the write.
pub(crate) async fn emit_event_store(store: &crate::db::SharedStore, session: &str, etype: &str, data: Option<Value>, idem: Option<String>, source: &str) {
    let session = session.to_string();
    let etype = etype.to_string();
    let source = source.to_string();
    let _ = store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            conn.execute(
                "INSERT OR IGNORE INTO session_events (ts, session, type, data, idem, source) VALUES (?,?,?,?,?,?)",
                rusqlite::params![
                    now_f64(),
                    session,
                    etype,
                    data.map(|d| d.to_string()),
                    idem,
                    source
                ],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
}

/// The secret-redaction pass Python applies before any chat text lands in a
/// DB row (py:8676 _cmd_hist_record / py:8655 steer history — AMUX-2525).
/// Same pattern family as the pipe-pane redactor (py:21478).
fn redact_secrets(text: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"((?:mxp|usr|ret)_sk)_[A-Za-z0-9_-]+|((?:AMUX_MIXPEEK_OPS_TOKEN|ANTHROPIC_API_KEY|OPENAI_API_KEY|GOOGLE_MAPS_API_KEY|GOOGLE_API_KEY|CLOUDFLARE_API_TOKEN|ELEVENLABS_API_KEY|POSTHOG_KEY|POSTHOG_PERSONAL_API_KEY)=)[^\s\r\n]+|(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]+|sk-ant-[A-Za-z0-9_-]+|sk-proj-[A-Za-z0-9_-]+|sk[_-][A-Za-z0-9]{32,}|AIza[0-9A-Za-z_-]{30,}|(?:phx|phc)_[A-Za-z0-9]+",
        )
        .expect("redact regex")
    });
    re.replace_all(text, |caps: &regex::Captures| {
        if let Some(p) = caps.get(1) {
            format!("{}_REDACTED", p.as_str())
        } else if let Some(p) = caps.get(2) {
            format!("{}REDACTED", p.as_str())
        } else {
            "SECRET_REDACTED".to_string()
        }
    })
    .into_owned()
}

const CMD_HIST_KEEP: i64 = 200;

/// How a message actually reached the worker (migration 0014).
///
/// A DELIVERY FACT, not an inference from `type`. The distinction earns its
/// place because of the raw-tmux incident: when `POST /api/workers/<n>/send`
/// 405'd, the CLI injected keystrokes with no origin stamp, no audit row and
/// unverified arrival — and those messages recorded byte-identically to a clean
/// direct send, because `type` has no cell for "delivered by a degraded path".
///
/// `None` is a real answer: rows written before 0014, and any path that has not
/// been taught to declare itself, stay NULL. Never defaulted to `Direct` —
/// asserting a delivery path we did not observe is exactly the
/// unknown-rendered-as-healthy bug.
#[derive(Debug, Clone, Copy, PartialEq)]
// NOTE — there is deliberately no `Fallback` variant. The raw-tmux degradation
// happens in the CLI, client-side, precisely WHEN THE SERVER IS UNREACHABLE, so
// this recorder is structurally blind to it: if the server could record it,
// there would have been no fallback. A variant nothing can construct is a
// vocabulary entry pretending to be an observation (and clippy's
// never-constructed warning is right to refuse it). Recording that path needs
// the CLI to report it retroactively on reconnect — AMUX-2670.
pub(crate) enum Delivery {
    /// Handed to a live session at the moment of the request.
    Direct,
    /// Parked on the steering queue, delivered at a later turn boundary.
    Queued,
}

impl Delivery {
    fn as_str(self) -> &'static str {
        match self {
            Delivery::Direct => "direct",
            Delivery::Queued => "queued",
        }
    }
}

// `cmd_hist_record` (py:8676 _cmd_hist_record) IS DELETED, not merely unused
// (AF-159). It was a one-line wrapper hardcoding `DeliveryMeta::direct()`, and
// its single caller was the STEER branch — so the only thing it ever did in
// production was record a message that had just been enqueued as though it had
// been delivered straight to the lane. Fixing that caller left the wrapper
// with no users and every future caller the same trap, ready to be reached for
// by name because it is the short one. The recorder that survives
// (`cmd_hist_record_full`) makes you state the delivery kind, which is the
// point: there is no longer a spelling of "record this" that quietly asserts
// direct delivery.

/// A scheduled command that was DELIVERED (py parity: origin = the schedule's
/// title, so a peek shows scheduled commands distinctly from a human's). Only
/// called once delivery is confirmed — recording history for a command that
/// never landed is how the Messages tab starts disagreeing with the run log.
pub(crate) async fn cmd_hist_record_schedule(
    state: &AppState,
    session: &str,
    text: &str,
    origin: &str,
) {
    cmd_hist_record_full(state, session, text, "schedule", origin, false, DeliveryMeta::direct()).await
}

/// The full recorder. `queued_at` is the moment the message entered the
/// steering queue (`None` for anything not queued), so the wait a message
/// endured is answerable from the row the Messages tab already reads instead of
/// requiring a join against `steering_history` that nothing performs.
/// How a message reached a lane, and whether it was ever seen to submit.
///
/// One value rather than three parameters: they are written together, read
/// together, and are meaningless apart — `queued_at` without `delivery` cannot
/// say what it timed. (It also keeps the recorder under clippy's argument
/// limit, which is the same argument stated as a lint.)
#[derive(Default, Clone, Copy)]
pub(crate) struct DeliveryMeta<'a> {
    pub delivery: Option<Delivery>,
    pub queued_at_ms: Option<i64>,
    /// AMUX-2643. None means "not verified", NEVER "failed" — the queued path
    /// has submitted nothing yet. Inventing one here would be the mislabelling
    /// 0014 exists to end.
    ///
    /// This used to end "and the deliverer stamps a verdict when it lands".
    /// NOTHING DOES (AMUX-3541): `UPDATE cmd_history` appears twice in this
    /// crate and both set `card_id`. The claim was made in four places and
    /// implemented in none, so a queued row's verdict stays None forever. The
    /// promise is removed rather than softened, because a docstring that
    /// describes an unbuilt mechanism is what got `delivered_at` believed.
    pub submit_verdict: Option<&'a str>,
}

impl DeliveryMeta<'_> {
    /// A send handed straight to a live lane, with nothing to verify.
    pub(crate) fn direct() -> Self {
        DeliveryMeta {
            delivery: Some(Delivery::Direct),
            ..Default::default()
        }
    }

    /// A message put on the STEERING QUEUE, which has submitted nothing yet
    /// (AF-159). `queued_at` is when the wait started; `submit_verdict` stays
    /// None because that is the documented, correct value for a message that
    /// has not been submitted — the whole point of the column.
    ///
    /// This exists because the steer branch used `direct()` for a message it
    /// had just handed to `steer_enqueue`, so a third of `type='user'` traffic
    /// was recorded as delivery='direct' with a blank verdict. Measured
    /// 2026-08-23 over 7 days: 313 confirmed, 144 blank, 18 retried — and the
    /// 144 were not a hole in the direct path, they were STEERING wearing the
    /// wrong label. The card that found them concluded "none are steering or
    /// queued: every one is delivery='direct'", which the label made true and
    /// the code made false.
    ///
    /// That is why the mislabelling matters more than the blank: blank is
    /// CORRECT for a queued message and WRONG for a direct one, so one label
    /// made the same value mean two things and neither could be read.
    pub(crate) fn queued(at_ms: i64) -> Self {
        DeliveryMeta {
            delivery: Some(Delivery::Queued),
            queued_at_ms: Some(at_ms),
            submit_verdict: None,
        }
    }
}

/// How recently an identical message must have been recorded for the second
/// one to count as a duplicate DELIVERY rather than a person repeating
/// themselves. Two minutes: long enough to span a retry, a restart and a queue
/// drain; short enough that "run the same command again" is not flagged.
const DUP_DELIVERY_WINDOW_MS: i64 = 120_000;

/// Mint a ledger card for a HUMAN prompt delivered via the send path and return
/// its row, or None when the prompt is steering/control (`title_from_prompt`
/// None: `[no-board]`, control words, bare slash commands, <12 chars) or the
/// worker already holds an open agent card (the prompt is steering work in
/// flight, not a new task).
///
/// WHY THIS EXISTS (AMUX-3071): the Python server's `_autotask_from_command`
/// carded every human command and stamped `cmd_history.card_id`. That path was
/// NOT ported to the Rust send/steer flow at the 792ce1f cutover (2026-08-09) —
/// only the orchestrator's `_amux_messages` DeliverMessage path got
/// `capture_prompt_card`, which the tmux fleet's send path never touches. Result:
/// 330 human prompts from 2026-08-09 onward were recorded with `card_id=NULL` and
/// left no board trace at all — the "no silent work" ledger discipline silently
/// stopped working. This restores it for the send path, mirroring
/// `capture_prompt_card`'s logic (title-from-prompt, open-card dedup, `doing`
/// mint, `capture: session prompt` log marker, `notified=1`). Runs inside the
/// caller's write transaction so the card and the `cmd_history.card_id` link are
/// atomic; the self-description STEER nudge stays orchestrator-only (it needs an
/// async enqueue), but the durable `needs-self-description` TAG is set here so a
/// needy card is still findable.
/// Redact secret-shaped substrings before a prompt is written to the FLEET-
/// READABLE board (AMUX-3384: a NetSuite login password a user typed was
/// auto-captured verbatim into a shared card title+desc; any session GETs
/// /api/board/<id>). Deliberately over-eager — masking a rare long identifier is
/// fine, a credential transiting shared state is not. It kills the LABELLED
/// (`password: …`, `secret=…`), the KNOWN-PREFIX (sk-/ghp_/AKIA/JWT…), the PEM
/// and the long HIGH-ENTROPY shapes. It CANNOT catch a bare, unlabelled,
/// low-entropy password — the inherent limit of pattern redaction — so it is a
/// mitigation, not a guarantee. Applied at BOTH capture sites (this one and the
/// orchestrator's capture_prompt_card) via the body they both derive title+desc
/// from.
pub(crate) fn redact_prompt_secrets(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut out = s.to_string();
    // 1. Labelled secrets: keep the label + separator, mask the value. The label
    //    match allows an env-var-style prefix/suffix (AWS_SECRET_ACCESS_KEY=…),
    //    where `\b` fails because `_` is a word char. Separators: `= : := is was`.
    let kv = cached_re!(
        r"(?i)([A-Za-z0-9]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|secret[_-]?key|private[_-]?key|client[_-]?secret|credential)[A-Za-z0-9_]*)([ \t]*(?:[:=]|:=|is|was)[ \t]*)(\S{3,})"
    );
    out = kv.replace_all(&out, "${1}${2}[REDACTED]").to_string();
    // 1b. `Authorization: Bearer <token>` / bare `Bearer <token>`.
    let bearer = cached_re!(r"(?i)\bbearer[ \t]+([A-Za-z0-9._~+/\-]{8,}=*)");
    out = bearer.replace_all(&out, "Bearer [REDACTED]").to_string();
    // 2. Known token prefixes (stripe / github / aws / slack / google / openai / JWT).
    let pref = cached_re!(
        r"(?i)\b(sk-[A-Za-z0-9]{20,}|sk_(?:live|test)_[A-Za-z0-9]{16,}|pk_(?:live|test)_[A-Za-z0-9]{16,}|rk_(?:live|test)_[A-Za-z0-9]{16,}|ghp_[A-Za-z0-9]{20,}|gh[opsu]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|A[KS]IA[0-9A-Z]{12,}|AIza[0-9A-Za-z_\-]{30,}|ya29\.[A-Za-z0-9_\-]{20,}|eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]+)\b"
    );
    out = pref.replace_all(&out, "[REDACTED]").to_string();
    // 3. PEM private-key blocks.
    let pem = cached_re!(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----");
    out = pem.replace_all(&out, "[REDACTED PRIVATE KEY]").to_string();
    // 4. Long high-entropy runs (>=44 chars, past a 40-hex git sha) that MIX case
    //    or carry base64 punctuation — real keys, not prose or a plain hash.
    let tok = cached_re!(r"[A-Za-z0-9+/_-]{44,}={0,2}");
    out = tok
        .replace_all(&out, |c: &regex::Captures| {
            let m = &c[0];
            let has_upper = m.bytes().any(|b| b.is_ascii_uppercase());
            let has_lower = m.bytes().any(|b| b.is_ascii_lowercase());
            let has_punct = m.bytes().any(|b| matches!(b, b'+' | b'/' | b'=' | b'-'));
            if (has_upper && has_lower) || has_punct {
                "[REDACTED]".to_string()
            } else {
                m.to_string() // pure-hex / single-case run (git sha, hex hash): leave it
            }
        })
        .to_string();
    out
}

fn mint_capture_card(
    conn: &rusqlite::Connection,
    session_name: &str,
    body: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<crate::db::board_store::IssueRow>> {
    // Redact secret shapes BEFORE anything derives a title/desc from the prompt —
    // both come from `body`, and the board is fleet-readable (AMUX-3384).
    let redacted = redact_prompt_secrets(body);
    if redacted != body {
        // Surface that the control FIRED (two-fixes rule) without logging the
        // value — so a sweep can confirm redaction is active and see how often a
        // secret is typed. grep "ledger: redacted secret".
        tracing::info!(
            session = %session_name,
            "ledger: redacted secret-shaped content from a captured prompt before carding (AMUX-3384)"
        );
    }
    let body = redacted.as_str();
    let Some(title) = amux_core::board::title_from_prompt(body) else {
        return Ok(None); // steering / control / [no-board] — mint nothing
    };
    if session_name.trim().is_empty() {
        return Ok(None);
    }
    // A pure status / info query ("status on MSG-29602?", "any update on X?") is
    // answered inline and produces no deliverable, so it is NOT a board work card:
    // the old capture minted it type=code/doing, which a question can never take
    // through the `code` gate (ethos rule 3) and which held the WIP slot until a
    // manual discard (three times in one session — AMUX-3330). It stays in
    // cmd_history (the Messages ledger keeps every prompt); it just does not become
    // a work card, exactly like the steering/control prompts title_from_prompt
    // already declines. Surface it (two-fixes rule): grep
    // "ledger: status-query not carded" to audit the detector, so a wrong
    // suppression of a REAL task is findable in the logs rather than silent.
    if amux_core::board::is_status_query(body) {
        tracing::info!(
            session = %session_name,
            "ledger: status-query not carded (recorded in cmd_history only) — AMUX-3330"
        );
        return Ok(None);
    }
    // AMUX-3147: the old dedup skipped capturing ANY new task whenever the session
    // held ANY open agent card — so only the FIRST task of a work-session reached
    // the board and every later prompt was silent ("none of these have board
    // items"). It applied a STEERING-path guard to genuine new user tasks: this
    // path is `is_user` only, and a user prompt IS a new task (orchestrator steers
    // arrive via the delivered path, not here). Manual work cards also counted, so
    // being mid-work on ANY card blanked the ledger entirely.
    //
    // Narrowed to the guard's real purpose: don't double-card a RAPID re-send of
    // one thought (the user pastes a spec across two sends). Skip ONLY when THIS
    // mechanism minted a capture card (`desc` starts with the `**Prompt:**`
    // marker) for this session within the window; a distinct task seconds+ later
    // still cards, and a manual work card never blocks a capture. Captures mint
    // `doing` (never re-dispatched), so an extra card cannot re-run work — the
    // AMUX-2613 double-run the old dedup was conflated with stays fixed by the
    // `doing` mint, not by this skip.
    let window_s: i64 = std::env::var("AMUX_CAPTURE_DEDUP_WINDOW_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(45);
    let cutoff = (now_ms / 1000) - window_s;
    let recent_capture: i64 = conn.query_row(
        "SELECT COUNT(*) FROM issues WHERE session = ?1 AND deleted IS NULL \
         AND owner_type = 'agent' AND creator = 'amux' AND status = 'doing' \
         AND substr(desc, 1, 11) = '**Prompt:**' AND created > ?2",
        rusqlite::params![session_name, cutoff],
        |r| r.get(0),
    )?;
    if recent_capture > 0 {
        // Surface the skip (two-fixes rule): a dropped capture must leave a trace,
        // not just an absent "auto-captured" line. grep "ledger: capture deduped".
        tracing::info!(
            session = %session_name,
            window_s,
            "ledger: capture deduped — a capture card was minted <{window_s}s ago (rapid re-send guard)"
        );
        return Ok(None);
    }
    let needs_self = amux_core::board::title_needs_self_description(&title);
    let desc_body: String = body.chars().take(300).collect();
    let mut row = crate::db::board_store::create_issue(
        conn,
        &crate::db::board_store::NewIssue {
            title,
            desc: format!("**Prompt:** {desc_body}"),
            // `doing`, NOT `todo`: an owned `todo` ledger card is Runnable to the
            // planner and its prompt was re-dispatched, double-running every
            // direct prompt (AMUX-2613). `doing` + agent owner is Assigned, never
            // re-dispatched.
            status: "doing".into(),
            session: Some(session_name.to_string()),
            item_type: "code".into(),
            creator: "amux".into(),
            owner_type: "agent".into(),
            due: None,
            due_time: None,
            reviewer: None,
            shepherd: None,
            gate: vec![],
            depends_on: vec![],
            tags: if needs_self.is_some() {
                vec!["needs-self-description".to_string()]
            } else {
                vec![]
            },
        },
        now_ms / 1000,
    )?;
    let stamp = chrono::Local::now().format("%H:%M").to_string();
    row.log = Some(crate::db::board_store::append_log(
        row.log.as_deref(),
        &stamp,
        "capture: session prompt",
    ));
    if let Some(reason) = needs_self {
        row.log = Some(crate::db::board_store::append_log(
            row.log.as_deref(),
            &stamp,
            &format!("capture: title needs self-description — {reason}"),
        ));
    }
    crate::db::board_store::save_patched(conn, &row)?;
    // `notified` is outside save_patched's SET list; set it so the assignment
    // notifier never re-announces a prompt the worker already received live.
    conn.execute("UPDATE issues SET notified = 1 WHERE id = ?1", rusqlite::params![row.id])?;
    Ok(Some(row))
}

pub(crate) async fn cmd_hist_record_full(
    state: &AppState,
    session: &str,
    text: &str,
    ctype: &str,
    origin: &str,
    skip_board: bool,
    meta: DeliveryMeta<'_>,
) {
    if session.is_empty() || text.is_empty() {
        return;
    }
    let session = session.to_string();
    let text = redact_secrets(text);
    let ctype = ctype.to_string();
    let origin: String = origin.chars().take(80).collect();
    let delivery = meta.delivery.map(|d| d.as_str().to_string());
    let submit_verdict = meta.submit_verdict.map(|v| v.to_string());
    let queued_at_ms = meta.queued_at_ms;
    let now_ms = now_i64() * 1000;

    // DUPLICATE-DELIVERY DETECTOR (Ethan's standing rule, 2026-08-11: fix the
    // bug AND make the next one surface in the logs).
    //
    // send_dedup only catches a retry carrying the SAME msg_id, its rows are
    // pruned after 600s, and a dedupe hit writes nothing anywhere — so "was
    // this delivered twice?" had no answer in amux at all. Establishing that
    // ONE report was a false alarm took grepping a 1.6MB pane log and
    // hand-deduping redraw captures, because a pipe-pane log re-captures a
    // visible line on every repaint ("53 occurrences" meant one delivery).
    //
    // This does NOT suppress the second delivery. Two identical sends can be
    // deliberate, and silently dropping one would turn a visible annoyance into
    // an invisible data-loss bug — strictly worse. It announces, and lets a
    // human or a sweep decide.
    let dup_prior: Option<(i64, i64)> = {
        let session_q = session.clone();
        let text_q = text.clone();
        match state.store.read() {
            Ok(conn) => conn
                .query_row(
                    "SELECT id, ts FROM cmd_history WHERE session=?1 AND text=?2 \
                     AND ts > ?3 ORDER BY ts DESC LIMIT 1",
                    rusqlite::params![session_q, text_q, now_ms - DUP_DELIVERY_WINDOW_MS],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok(),
            Err(_) => None,
        }
    };
    if let Some((prior_id, prior_ts)) = dup_prior {
        let age_s = (now_ms - prior_ts) as f64 / 1000.0;
        tracing::warn!(
            session = %session, prior_id, age_s,
            preview = %chars_truncate(&text, 80),
            "duplicate delivery: identical text already recorded for this lane"
        );
        emit_event(
            state,
            &session,
            "message.duplicate",
            Some(json!({
                "prior_id": prior_id, "age_s": age_s, "type": ctype,
                "chars": text.chars().count(),
                "preview": chars_truncate(&text, 120),
            })),
            None,
            "cmd-history",
        )
        .await;
    }

    let is_user = ctype == "user";
    // Carry the recorded row id out of the write so auto-capture can link the card.
    let msg_row_id = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    // delivered_at IS NOT A COPY OF ts ANY MORE (AMUX-3541).
    //
    // Both columns were `now_ms`, unconditionally, on every row. Measured
    // 2026-08-23 before this change: delivered_at == ts on 3854 of 3854 rows,
    // max gap 0 — INCLUDING all 40 `delivery='queued'` rows, where a real
    // delivery time must show a gap, because the entire point of queueing is
    // waiting for a turn boundary. The column was the insert time wearing a
    // delivery time's name, and being populated 100% of the time is exactly
    // what made it believable: AF-159 reasoned "all 144 have delivered_at set,
    // so these landed" — a correct inference from the name and the docstring,
    // and false.
    //
    // A DIRECT send really was delivered when it was recorded, so `now_ms` is
    // true there and stays. A QUEUED one has not been delivered, so the honest
    // value is NULL — and NULL is what makes the missing deliverer COUNTABLE
    // rather than invisible. Four comments in this file promise "the deliverer
    // stamps delivered_at when it lands" and nothing does (`UPDATE cmd_history`
    // appears twice, both setting card_id). Until that is built, queued rows
    // accumulate with a NULL delivered_at, and
    //
    //     SELECT COUNT(*) FROM cmd_history WHERE delivery='queued' AND delivered_at IS NULL
    //
    // is a number somebody can watch. Before this change the same defect was
    // unobservable, because the wrong answer and the right one were the same
    // bytes.
    //
    // Safe to NULL: no client reads this column. app.js's only `delivered_at`
    // (line 6661, "Sent <ago>") is fed by the steering-history endpoint, whose
    // separate `steering_history` table IS stamped at real delivery by the
    // deliverer — which is also where the timestamp for the eventual backfill
    // will come from.
    let delivered_at_ms = match meta.delivery {
        Some(Delivery::Queued) => None,
        _ => Some(now_ms),
    };
    let msg_row_id_w = msg_row_id.clone();
    let cap_session = session.clone();
    let cap_text = text.clone();
    let _ = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            conn.execute(
                "INSERT INTO cmd_history (text, type, session, ts, origin, delivery, queued_at, delivered_at, submit_verdict) \
                 VALUES (?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    text, ctype, session, now_ms, origin,
                    delivery, queued_at_ms, delivered_at_ms, submit_verdict
                ],
            )?;
            msg_row_id_w.store(conn.last_insert_rowid(), std::sync::atomic::Ordering::SeqCst);
            conn.execute(
                "DELETE FROM cmd_history WHERE session=?1 AND id NOT IN \
                 (SELECT id FROM cmd_history WHERE session=?1 ORDER BY ts DESC LIMIT ?2)",
                rusqlite::params![session, CMD_HIST_KEEP],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;

    // NO SILENT WORK (AMUX-3071): mint a ledger card for a HUMAN prompt and link
    // it to the message row. Separate write so a capture failure can never roll
    // back the message record — the message is the durable entity, the card its
    // consequence (CLAUDE.md: hang the consequence off the write that happened).
    // Gated on ctype=="user": inter-session ("session") and scheduler ("schedule")
    // messages are not the recipient's task and must not spam the board.
    // ISOLATED (AMUX-3232): a raw agent leaves no board trace; its prompts are
    // not auto-captured as ledger cards. It has no session/URL to run `amux
    // board`, so a card minted here would name work nobody can drive, and the
    // accountability sweep is likewise told to skip it.
    if is_user && !skip_board && !session_is_isolated(&cap_session) {
        let row_id = msg_row_id.load(std::sync::atomic::Ordering::SeqCst);
        if row_id > 0 {
            let minted: std::sync::Arc<std::sync::Mutex<Option<String>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let minted_w = minted.clone();
            let sess_log = cap_session.clone();
            let res = state
                .store
                .write_async(move |conn| match mint_capture_card(conn, &cap_session, &cap_text, now_ms)? {
                    Some(row) => {
                        conn.execute(
                            "UPDATE cmd_history SET card_id = ?1 WHERE id = ?2",
                            rusqlite::params![row.id, row_id],
                        )?;
                        *minted_w.lock().unwrap() = Some(row.id.clone());
                        let ev = crate::db::PendingEvent {
                            entity_type: amux_core::revision::EntityType::Task,
                            entity_id: row.id.clone(),
                            mutation: amux_core::revision::MutationKind::Created,
                            payload: Some(row.snapshot()),
                        };
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![ev] })
                    }
                    None => Ok(crate::db::WriteOutcome { applied: false, events: vec![] }),
                })
                .await;
            match res {
                // Positive log signal (two-fixes rule): if auto-capture silently
                // stops again, the absence of these lines while user prompts keep
                // arriving — plus the cmd_history.card_id NULL rate — is the
                // detector. grep "ledger: auto-captured".
                Ok(_) => {
                    if let Some(cid) = minted.lock().unwrap().take() {
                        tracing::info!(session = %sess_log, card_id = %cid,
                            "ledger: auto-captured board card from delivered prompt");
                    }
                }
                Err(e) => tracing::warn!(session = %sess_log, error = %e,
                    "ledger auto-capture FAILED; prompt recorded without a board card"),
            }
        }
    }
}

/// py:8595 _steer_enqueue — durable queue row + message.queued event.
/// Dedup-on-enqueue: identical text (or same guard) replaces, never stacks.
///
/// `sender` is the origin lane (the server-verified `X-Amux-Session` stamp) or
/// "" for an automated producer, which the `guard` already identifies. It is
/// recorded so a stalled queue can tell the one party who is holding a false
/// belief about it — see [`lane_block_reason`] and `warn_on_stalled_lanes`.
pub(crate) async fn steer_enqueue(
    state: &AppState,
    name: &str,
    text: &str,
    guard: &str,
    sender: &str,
) -> String {
    steer_enqueue_store(&state.store, name, text, guard, sender).await
}

/// The same enqueue, addressed by STORE rather than by `AppState`.
///
/// Both this and [`emit_event`] only ever needed the store; the `AppState`
/// parameter is what kept background producers — the orchestrator, which holds
/// a `SharedStore` and no `AppState` — from reaching the one delivery path.
/// The alternative was a second enqueue sitting next to this one, and two
/// spellings of "queue a message for a lane" is precisely the duplication that
/// then has to be kept in step forever (CLAUDE.md's primitives rule; ethos D6
/// on what one duplicated seam already costs). One implementation, two
/// addressing modes.
pub(crate) async fn steer_enqueue_store(
    store: &crate::db::SharedStore,
    name: &str,
    text: &str,
    guard: &str,
    sender: &str,
) -> String {
    let msg_id = format!("steer-{}", (now_f64() * 1000.0) as i64);
    let id = msg_id.clone();
    // The id we RETURN must be the row that actually exists. When a guarded
    // re-enqueue updates the prior row in place (AMUX-3557) the freshly minted
    // id is never inserted, and handing it back would give the caller a
    // message id that matches nothing in the queue — a lookup that answers
    // "not found" for a message that is sitting there.
    let effective_id = std::sync::Arc::new(std::sync::Mutex::new(msg_id.clone()));
    let effective_id_w = effective_id.clone();
    let session = name.to_string();
    let text_s = text.to_string();
    let guard_s = guard.to_string();
    let sender_s = sender.to_string();
    let _ = store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            // A GUARDED RE-ENQUEUE UPDATES IN PLACE AND KEEPS `queued_at`
            // (AMUX-3557). This was DELETE-then-INSERT, which looks like the
            // same thing and is not: the new row got `now_f64()`, so a guarded
            // message RESET ITS OWN AGE on every re-enqueue. Delivery is
            // `ORDER BY queued_at ASC` — oldest first — so a message that is
            // re-queued faster than the queue drains can NEVER be the oldest,
            // and loses to anything else in the queue forever. The guard
            // written to protect the message is what starved it.
            //
            // Measured 2026-08-23 on `backend`: auto-compact re-enqueued 300
            // times, consecutive gaps of 4 to 117 seconds, because the trigger
            // re-evaluates on every state report and the condition (pct=7)
            // cannot clear until the lane actually compacts. 78 of those landed
            // in a single hour.
            //
            // Keeping `queued_at` is also the honest value: the message HAS
            // been waiting since it was first queued. The newest TEXT wins
            // because that is the guard's purpose (one pending answer, current
            // content); the age is not part of the content.
            let existing: Option<String> = if guard_s.is_empty() {
                None
            } else {
                conn.query_row(
                    "SELECT id FROM steering_queue WHERE session=?1 AND guard=?2 LIMIT 1",
                    rusqlite::params![session, guard_s],
                    |r| r.get(0),
                )
                .ok()
            };
            if let Some(prior) = existing {
                conn.execute(
                    "UPDATE steering_queue SET text=?1, sender=?2 WHERE id=?3",
                    rusqlite::params![text_s, sender_s, prior],
                )?;
                if let Ok(mut g) = effective_id_w.lock() {
                    *g = prior;
                }
            } else {
                conn.execute(
                    "DELETE FROM steering_queue WHERE session=?1 AND text=?2",
                    rusqlite::params![session, text_s],
                )?;
                conn.execute(
                    "INSERT OR REPLACE INTO steering_queue(id, session, text, queued_at, guard, sender) VALUES(?,?,?,?,?,?)",
                    rusqlite::params![
                        id,
                        session,
                        text_s,
                        now_f64(),
                        if guard_s.is_empty() { None } else { Some(guard_s.clone()) },
                        sender_s
                    ],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    emit_event_store(
        store,
        name,
        "message.queued",
        Some(json!({"chars": text.chars().count(), "preview": chars_truncate(text, 120), "guard": if guard.is_empty() { Value::Null } else { json!(guard) }})),
        Some(format!("q:{msg_id}")),
        "steering",
    )
    .await;
    // The row that exists, not the one we minted (AMUX-3557).
    effective_id.lock().map(|g| g.clone()).unwrap_or(msg_id)
}

/// py:25236 _send_dedup_seen — idempotency across client retries, persisted
/// because the loss window IS a server restart.
async fn send_dedup_seen(state: &AppState, name: &str, msg_id: &str) -> bool {
    let session = name.to_string();
    let msg_id = msg_id.to_string();
    let reply = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            conn.execute("DELETE FROM send_dedup WHERE ts < ?", [now_i64() - 600])?;
            let dup = conn
                .execute(
                    "INSERT INTO send_dedup (session, msg_id, ts) VALUES (?,?,?)",
                    rusqlite::params![session, msg_id, now_i64()],
                )
                .is_err();
            Ok(crate::db::WriteOutcome {
                applied: !dup,
                events: vec![],
            })
        })
        .await;
    match reply {
        Ok(r) => !r.applied,
        Err(_) => false, // dedup is best-effort; never block a send on it
    }
}

async fn send_dedup_forget(state: &AppState, name: &str, msg_id: &str) {
    let session = name.to_string();
    let msg_id = msg_id.to_string();
    let _ = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            conn.execute(
                "DELETE FROM send_dedup WHERE session=? AND msg_id=?",
                rusqlite::params![session, msg_id],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
}

// ---------------------------------------------------------------------------
// is_running (py:4372): tmux session present + not a bare shell + the pane
// shell has a child. herdr: agent presence. iTerm2: unsupported (501 at the
// verb layer; here it reads not-running).
// ---------------------------------------------------------------------------

/// Does the pane's shell have a live child process? `Some(true)` = a child is
/// running (claude or similar), `Some(false)` = shell confirmed childless,
/// `None` = could not determine (tmux/pgrep unavailable or errored). This is
/// the process-level discriminator the scrape detectors cannot fake: a pane
/// whose shell has a child is hosting SOMETHING, however the frame reads.
async fn pane_has_live_child(name: &str) -> Option<bool> {
    let stq = st(name);
    let out = tmux(&["list-panes", "-t", &stq, "-F", "#{pane_pid}"]).await?;
    if !out.status.success() {
        return None;
    }
    let pid = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
    if pid.is_empty() {
        return None;
    }
    let ch = run_cmd("pgrep", &["-P", &pid], OP_TIMEOUT).await?;
    Some(!ch.stdout.iter().all(|b| b.is_ascii_whitespace()))
}

pub(crate) async fn is_running(name: &str) -> bool {
    let cfg = parse_env(name);
    if !iterm2_id(&cfg).is_empty() {
        return false;
    }
    if backend_of_cfg(&cfg) == "herdr" {
        return herdr_agent_running(name).await;
    }
    let tmux_sess = tmux_name(name);
    if !tmux_sessions_set().await.contains(&tmux_sess) {
        return false;
    }
    let output = tmux_capture(name, 10).await;
    if output.is_empty() {
        return true;
    }
    if at_shell_prompt(&output) {
        return false;
    }
    // Shell alive but childless == claude gone even without a visible prompt.
    if pane_has_live_child(name).await == Some(false) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// send_text (py:25432) — the delivery choreography. Ported: restart guard,
// resume-picker guard, auto-wake, boot-in-flight wait, status-gated Escape
// discipline (the 1.3s double-Escape rule), C-u clear, paste-buffer for >400
// chars, @/slash picker handling, steering enqueue for waiting selectors,
// and — AMUX-2629 — the SUBMISSION EVIDENCE GATE (`verify_submitted`).
//
// KEYSTROKE DELIVERY IS BEST-EFFORT AND ALWAYS WILL BE. `tmux send-keys`
// succeeding proves the bytes reached the pty, nothing more: whether Claude
// Code's TUI turned them into a submitted message is a fact only Claude Code
// holds. That is why "sent" may never be inferred from the send-keys exit
// code — it must be READ BACK from an artifact the TUI writes.
//
// The durable fix is not here. It is protocol delivery, where submission is
// an ACK rather than an inference (opencode::structured, AgentProtocol). As
// of 2026-08-09 that path cannot carry ANY interactive lane — see
// `lane_has_protocol_path` — so this choreography is still the only one, and
// this gate is what keeps it honest until the protocol takes over.
// ---------------------------------------------------------------------------


/// Does `text` read as an ANSWER to the picker currently on `pane`?
///
/// AMUX-2823. A queued message and a queued KEYPRESS are different objects and
/// the steering queue only models the first. A prompt is text the model reads;
/// late delivery is fine. A picker answer is only meaningful WHILE THAT PICKER
/// IS UP, and delivering it afterwards is worse than dropping it — it becomes an
/// instruction the model tries to obey.
///
/// Live specimen: Ethan typed "1. Stop and wait for limit to reset" at a
/// rate-limit menu. It queued (selector guard), the menu was dismissed before it
/// drained, and the queue then typed the literal string into an empty prompt.
/// mvs-infra spent 1m41s reasoning about it.
///
/// DELIBERATELY NARROW. Not every message queued during a picker is an answer to
/// it — "go fix the tests" typed while a picker happens to be up is meant for
/// afterwards, and voiding that would be data loss. So this requires the text to
/// MATCH A VISIBLE OPTION: the bare option number, or a prefix of the option's
/// own words. Anything else is treated as an ordinary prompt and delivered
/// normally.
pub(crate) fn answers_visible_picker(text: &str, pane: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t.is_empty() || t.chars().count() > 120 {
        return false; // a paragraph is not a menu answer
    }
    let clean = strip_ansi(pane).to_lowercase();
    // Option lines look like "  1. stop and wait for limit to reset" or
    // "❯ 2. switch to usage credits".
    let mut options: Vec<(String, String)> = Vec::new();
    for line in clean.lines() {
        let l = line.trim().trim_start_matches(['\u{276f}', '>', ' ']).trim();
        let Some((num, rest)) = l.split_once(". ") else { continue };
        let num = num.trim();
        if num.len() == 1 && num.chars().all(|c| c.is_ascii_digit()) && !rest.trim().is_empty() {
            options.push((num.to_string(), rest.trim().to_string()));
        }
    }
    if options.is_empty() {
        return false; // no picker visible: nothing to be an answer to
    }
    for (num, label) in &options {
        // "1", "1." or "1. stop and wait…" — the shapes a person types or a UI
        // sends when clicking a numbered option.
        if t == *num || t == format!("{num}.") {
            return true;
        }
        let stripped = t
            .trim_start_matches(num.as_str())
            .trim_start_matches('.')
            .trim();
        if !stripped.is_empty() && (label.starts_with(stripped) || stripped.starts_with(label.as_str())) {
            return true;
        }
    }
    false
}

pub(crate) fn at_picker_text(text: &str) -> bool {
    // py:25538 _AT_PICKER_RE = r'(?:^|\s)@\S' — an @ that STARTS a token, plus
    // a leading slash. NOT `contains('@')`: that also matches emails
    // (mhoward@lucihub.com) and backticked `@backend` mentions, which
    // force-queued any such message while the session generated even in Send
    // mode (py's 2026-07-17 fix, dropped in the rust port and restored here).
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?:^|\s)@\S").expect("at-picker regex"));
    re.is_match(text) || text.trim_start().starts_with('/')
}

// py:25349 `_pending_input` has NO rust counterpart on purpose. It returned a
// String and could not say WHICH KIND of text it found, which is the whole
// defect below; `composer_state` replaces it so there is exactly one way to
// read the composer and it requires the raw capture. The box-unwrapping python
// did (the ❯ line alone is not the whole message when it hard-wraps at the pane
// width — the "random" ghost, 2026-07-10) is preserved inside it.
/// What Claude Code's composer is showing, and — the part that matters —
/// whether pressing Enter would submit anything.
///
/// THIS IS THE STATE CHECK THAT WAS MISSING (2026-08-09, second finding).
/// `_pending_input` (py:25349) strips ANSI and then reads the ❯ line, which
/// makes Claude Code's DIM SUGGESTION indistinguishable from text a person or
/// amux actually typed. Both render as `❯ <words>` once the escapes are gone.
/// They are not the same thing at all: Enter submits typed text and is a
/// NO-OP on a suggestion (measured directly — Escape+Enter on a `Try "fix lint
/// errors"` placeholder submitted nothing).
///
/// That single conflation produced a fleet-wide false positive: a scan reported
/// 13 lanes "holding unsubmitted text for hours" (`backend` "continue with the
/// queue", `ethan-dev` "push it", `mvs-infra` "Run the MVS prod health loop per
/// the runbook", …). All 13 composers were EMPTY. Someone then pressed Enter,
/// C-m and Escape+Enter on them and reported that none worked — correctly, and
/// for a reason nobody could see: there was nothing to submit. The next step
/// after that would have been submitting 13 stale instructions into live lanes.
///
/// The discriminator is in the bytes tmux already gives us and the ANSI strip
/// was throwing away: **SGR 2 (dim)**. Verified in both directions on a live
/// pane —
///
/// ```text
/// placeholder: "\x1b[39m❯\u{a0}\x1b[2mTry \"fix lint errors\"\x1b[0m"
/// real input:  "\x1b[39m❯\u{a0}[10:20 PM] look at @/Users/…/README.md please"
/// ```
///
/// — and real input never uses dim: plain words, an `@`-mention and a >400-char
/// paste all render undimmed, and a slash command is COLOURED (38;5;153), not
/// dimmed. So: text is real if and only if some of it is not dim.
///
/// Callers must pass the RAW capture (`capture-pane -e`). Passing a
/// pre-stripped frame silently re-creates the bug, which is why this takes
/// `raw_frame` and does its own stripping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposerState {
    /// No ❯ composer drawn — cold boot, a non-Claude frame, or a full-screen
    /// view. NOT "submitted" and NOT "empty" (AC-271).
    NotVisible,
    /// Composer drawn and holding nothing.
    Empty,
    /// Composer drawn and empty, showing Claude Code's dim suggestion. Enter
    /// here does nothing; there is nothing stuck.
    Placeholder(String),
    /// Real pending input. Enter would submit this.
    Typed(String),
    /// The background-conversation manager (`←`) is open over the lane. Its
    /// composer is NOT the lane's — keystrokes sent now compose a NEW task
    /// instead of a message to this session. A send must refuse, not type.
    BackgroundManager,
}

impl ComposerState {
    /// The text a caller may act on: real input only.
    pub(crate) fn typed(&self) -> Option<&str> {
        match self {
            ComposerState::Typed(t) => Some(t),
            _ => None,
        }
    }
}

/// True while SGR 2 is in effect at each character. Per line: tmux's `-e`
/// output re-emits the attribute context at the start of every line, and the
/// composer's own placeholder closes with `\x1b[0m` on the same line.
fn dim_mask(line: &str) -> (String, String) {
    let (mut plain, mut dim) = (String::new(), String::new());
    let mut is_dim = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Consume the escape sequence; only SGR (`[ … m`) affects dimness.
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut body = String::new();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        if c2 == 'm' {
                            for code in body.split(';') {
                                match code.trim() {
                                    "2" => is_dim = true,
                                    "0" | "" | "22" => is_dim = false,
                                    _ => {}
                                }
                            }
                        }
                        break;
                    }
                    body.push(c2);
                }
            } else {
                // OSC / charset / other: skip to its terminator.
                for c2 in chars.by_ref() {
                    if c2 == '\u{7}' || c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if is_dim {
            dim.push(c);
        } else {
            plain.push(c);
        }
    }
    (plain, dim)
}

pub(crate) fn composer_state(raw_frame: &str) -> ComposerState {
    let clean = strip_ansi(raw_frame);
    // The manager view owns the keyboard: its own status bar says so. Positive
    // match on the chrome Claude Code prints, not on a timer or an absence.
    //
    // THE HEADLINE MUST START A LINE (AMUX-2680). A bare `contains` matched the
    // phrase anywhere in the frame — including a lane's own PROSE ABOUT the
    // feature. On 2026-08-10 the `amux` lane investigated the agents panel
    // (AMUX-2635), quoted the headline twice in its written report, and locked
    // itself out: every send returned 500 "press esc or enter in the pane to
    // collapse it" for 6.6 hours, 4 steering messages queued undelivered, and
    // Escape could not clear it because the text was ordinary scrollback, not a
    // modal. Both quotes were mid-sentence (`... opened 'Your conversation moved
    // to the background - 4 awaiting input' = the`); the real view prints the
    // headline as its own line. That one positional constraint separates them.
    //
    // Keep the two-marker alternative below: it is the manager's footer chrome
    // and is what makes this robust if the headline is ever reworded. Do NOT
    // relax this back to a bare contains — any session that merely DISCUSSES
    // background conversations becomes unreachable, which is the D1 hazard
    // (scraping rendered UI as the control plane) with a self-referential twist:
    // the lane investigating the feature is the one it silences.
    let headline_own_line = clean
        .lines()
        .any(|l| l.trim_start().starts_with("Your conversation moved to the background"));
    if headline_own_line
        || (clean.contains("enter to collapse") && clean.contains("ctrl+x to delete all"))
    {
        return ComposerState::BackgroundManager;
    }
    // Work on RAW lines so the dim attribute survives; index by the stripped
    // form so the ❯ scan is unchanged from python.
    let raw_lines: Vec<&str> = raw_frame.lines().filter(|l| !strip_ansi(l).trim().is_empty()).collect();
    let stripped: Vec<String> = raw_lines.iter().map(|l| strip_ansi(l)).collect();
    let Some(idx) = stripped
        .iter()
        .rposition(|l| matches!(l.trim().chars().next(), Some('\u{276f}') | Some('\u{203a}')))
    else {
        return ComposerState::NotVisible;
    };
    let mut block: Vec<&str> = vec![raw_lines[idx]];
    for (i, s) in stripped.iter().enumerate().skip(idx + 1) {
        let t = s.trim();
        if matches!(t.chars().next(), Some('\u{2500}') | Some('\u{23f5}')) {
            break;
        }
        block.push(raw_lines[i]);
    }
    let (mut plain, mut dim) = (String::new(), String::new());
    for (n, l) in block.iter().enumerate() {
        let (mut p, d) = dim_mask(l);
        if n == 0 {
            // Drop the prompt glyph itself; it is chrome, never content.
            p = p.trim_start().trim_start_matches(['\u{276f}', '\u{203a}', ' ', '\u{a0}', '\t']).to_string();
        }
        plain.extend(p.split_whitespace());
        dim.extend(d.split_whitespace());
    }
    if !plain.is_empty() {
        ComposerState::Typed(plain)
    } else if !dim.is_empty() {
        ComposerState::Placeholder(dim)
    } else {
        ComposerState::Empty
    }
}

/// The stable prefix of every background-conversation refusal. Callers (and
/// [`send_failure_status`]) key on THIS, not on the whole sentence, so the
/// actionable tail can be reworded without silently reclassifying the refusal
/// back to a 500.
pub(crate) const BG_VIEW_REFUSAL_PREFIX: &str =
    "session is in the background-conversation view";

/// Policy knob for the background-manager guard, owned by the human exactly
/// like D2's `rate_limit_action`: `collapse` (default — press esc, verify,
/// deliver) or `refuse` (detect and leave the view for a person). Default is
/// opt-OUT because a fleet whose lanes go unreachable is the failure ethos
/// rule 1 is about; anything unrecognised means the default.
fn bg_view_collapse_enabled() -> bool {
    !matches!(
        std::env::var("AMUX_BG_VIEW_ACTION").unwrap_or_default().trim(),
        "refuse" | "off" | "0"
    )
}

/// The refusal text, naming WHICH of the three honest states we are in — the
/// caller cannot act on "it is in the background view" alone, because the next
/// step differs: wait (mid-turn), look at the pane (esc did not work), or
/// change the pref. Every variant keeps [`BG_VIEW_REFUSAL_PREFIX`].
fn bg_view_refusal(generating: bool) -> String {
    let tail = if !bg_view_collapse_enabled() {
        " — press esc or enter in the pane to collapse it \
         (auto-collapse is off: AMUX_BG_VIEW_ACTION=refuse)"
    } else if generating {
        " — the lane is mid-turn, so amux did not press esc (it would interrupt the run); \
         retry at the next turn boundary or press esc in the pane"
    } else {
        " — amux pressed esc and the view did not collapse; press esc or enter in the pane"
    };
    format!(
        "{BG_VIEW_REFUSAL_PREFIX} (its composer starts a new task, not a message to this \
         session){tail}"
    )
}

/// The HTTP status a `(false, msg)` outcome from the session-verb path
/// deserves, plus a static next step for the SPA to render.
///
/// **A 500 must mean "amux broke".** Everything below is amux working
/// correctly and DECLINING, and shipping a refusal as 500 is not cosmetic:
/// 15 of the 19 errors in the 6h window before this was written were one
/// refusal (the background-conversation guard) wearing a 500, and
/// `runtime_jobs::autofix` files a board card for every distinct 5xx
/// signature — so a miscoded refusal spends a lane's whole turn on a non-bug.
/// 409 is this repo's idiom for "declined, and here is the way out" (the board
/// gates answer 409 with a `cli:` hint); the hint here is that field.
///
/// Derived from the outcome string for the same reason [`submit_verdict_of`]
/// is — `send_text` hands `(bool, String)` to a long list of callers and
/// widening that signature would touch every one of them — and it pays the
/// same price for drift. That price is covered by
/// `every_send_failure_literal_is_classified`, which extracts every
/// `(false, "…")` literal in THIS FILE at compile time and fails if one of
/// them is neither a known refusal nor a known hard failure. A new refusal
/// added later cannot silently land in the 500 bucket.
pub(crate) fn send_failure_status(msg: &str) -> (StatusCode, Option<&'static str>) {
    let m = msg.trim();
    // THE WRAPPER CARRIES THE REAL OUTCOME. A send to a stopped lane auto-wakes
    // it, and a wake that legitimately declines comes back as
    // "auto-wake failed: session is archived; wake it first" — a refusal with an
    // obvious next step, wearing a prefix that reads like a crash. Classifying
    // the wrapper buried every one of them at 500. Recurse on the inner message
    // instead; a genuinely broken wake ("auto-wake failed: tmux refused") still
    // falls through to 500 because the inner text is unclassified.
    if let Some(inner) = m.strip_prefix("auto-wake failed: ") {
        return send_failure_status(inner);
    }
    // --- 404: the target does not exist.
    if m.starts_with("session '") && m.ends_with("not found") {
        return (StatusCode::NOT_FOUND, Some("GET /api/sessions lists the live lanes"));
    }
    // --- 400: the request itself is malformed. Retrying verbatim cannot work.
    if m == "invalid session name" {
        return (StatusCode::BAD_REQUEST, Some("session names are letters, digits, '.', '_' and '-'"));
    }
    if m.starts_with("key '") && m.ends_with("not in allowed set") {
        return (StatusCode::BAD_REQUEST, Some("the allowed keys are named in the message"));
    }
    // --- 501: honest degradation. The capability is ABSENT, not broken — the
    //     same shape as /api/email/search on an unconnected account, which is
    //     deliberately not a bug and deliberately not filed by the autofix.
    if m.starts_with("herdr-backed session start is not ported") {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Some("herdr session start is not ported to this origin — start the lane from herdr"),
        );
    }
    if m.starts_with(HERDR_NOT_ADDRESSABLE) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Some("this lane is herdr-backed but has no addressable workspace — check AMUX_HERDR_SESSION and that the worker exists"),
        );
    }
    if m.starts_with("iTerm2-backed sessions are not supported") {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Some("this lane is iTerm2-backed; use a tmux- or herdr-backed lane"),
        );
    }
    // --- 409: amux declined because of the lane's CURRENT state. Every one of
    //     these names the state and a way out, and every one becomes deliverable
    //     on its own without anybody fixing amux.
    let conflict: &[(&str, &str)] = &[
        ("not running", "POST /api/sessions/<name>/start, or send again to auto-wake it"),
        // The keys landed and Claude Code did not take them. amux did its job
        // and the composer declined, so the text is still in the input box —
        // recoverable, and the caller needs to know it is NOT delivered.
        // `submit_verdict_of` already calls this "stuck"; the status code was
        // the only place still calling it a crash.
        (
            "not submitted",
            "the text is sitting in the lane's composer, NOT delivered — retry at the next              turn boundary",
        ),
        ("session is in resume picker", "choose a conversation in the pane, or send the Escape key"),
        (
            BG_VIEW_REFUSAL_PREFIX,
            "the text was NOT delivered; retry once the pane leaves the background-conversation view",
        ),
        ("session at a selector", "a prompt is open in the pane — answer it, then retry"),
        ("session started generating", "retry at the next turn boundary, or POST with deliver_now"),
        ("session is blocked", "remove the lane from ~/.amux/blocked-sessions.txt"),
        ("session is archived", "POST /api/sessions/<name>/wake first"),
        ("terminal client attached", "a terminal client owns the size — detach it, or resize there"),
        ("no agents panel on screen", "open the agents panel in the pane (left arrow) first"),
        ("could not enter agent select mode", "the pane did not enter select mode — retry"),
        ("agent panel closed mid-navigation", "the panel closed under the navigation — retry"),
    ];
    for (needle, hint) in conflict {
        if m.starts_with(needle) {
            return (StatusCode::CONFLICT, Some(hint));
        }
    }
    // --- 500: unhandled. tmux/keys/fs actually failed. THIS is what the
    //     autofix watcher should be spending lanes on.
    (StatusCode::INTERNAL_SERVER_ERROR, None)
}

/// A verb's `(ok, msg)` rendered as its HTTP answer, classified by
/// [`send_failure_status`]. Shared by `archive`/`wake`/`reset` so they cannot
/// disagree with `send` about what "session is blocked" costs — all three used
/// to answer a flat 500 for every failure, refusals included.
fn verb_resp(ok: bool, msg: String) -> Response {
    let (code, fix) = if ok { (StatusCode::OK, None) } else { send_failure_status(&msg) };
    let mut body = json!({"ok": ok, "message": msg});
    if let Some(fix) = fix {
        body["fix"] = json!(fix);
    }
    jresp(code, body)
}

/// Durable submission evidence (py:25373 `_jsonl_user_msg_since`): true if
/// `text` already landed in the session's conversation JSONL as a user message
/// stamped AFTER `since`.
///
/// The pane can lie mid-repaint (resize rewrap; the ~1s gap before the spinner
/// paints after a submit); the JSONL append happens AT submission and cannot.
/// The `since` gate uses the message's OWN timestamp, not file mtime, so an
/// older identical text — a second "continue" minutes later — cannot count as
/// this send.
pub(crate) fn jsonl_user_msg_since(name: &str, text: &str, since: f64) -> bool {
    let needle = text.trim();
    if needle.is_empty() {
        return false;
    }
    let Some(p) = session_jsonl_path(name) else { return false };
    // 256KiB tail, same budget as python's f.seek(size - 262144).
    jsonl_records_have(&iter_jsonl_tail(&p, 262_144), needle, since)
}

/// The evidence scan itself, over already-parsed records — pure so it can be
/// tested against a planted transcript rather than a mock of the file reader.
pub(crate) fn jsonl_records_have(recs: &[Value], needle: &str, since: f64) -> bool {
    if needle.trim().is_empty() {
        return false;
    }
    let needle = needle.trim();
    for rec in recs.iter().rev() {
        let msg = &rec["message"];
        let role = msg["role"].as_str().or_else(|| rec["type"].as_str()).unwrap_or("");
        if role != "user" {
            continue;
        }
        let hit = match &msg["content"] {
            Value::String(s) => s.contains(needle),
            Value::Array(items) => items
                .iter()
                .any(|c| c["text"].as_str().map(|t| t.contains(needle)).unwrap_or(false)),
            _ => false,
        };
        if !hit {
            continue;
        }
        let Some(ts) = rec["timestamp"].as_str().and_then(parse_iso8601) else { continue };
        if ts >= since - 2.0 {
            // small slack for clock/rounding, as python
            return true;
        }
    }
    false
}

/// What ONE pane frame says about our message. Pure: everything the verifier
/// decides from a frame is decided here, so the decision can be tested against
/// the real incident's frames instead of against a paraphrase of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameRead {
    /// Claude Code has not drawn its composer at all. NOT "submitted" (AC-271).
    NoUi,
    /// The composer is drawn and does not hold our text.
    Cleared,
    /// Our text is still sitting in the composer, and the lane is generating —
    /// that is queued input, which submits at the turn boundary.
    StillThereGenerating,
    /// Our text is still sitting in the composer with the lane idle. Nothing is
    /// going to submit it.
    StillThereIdle,
}

pub(crate) fn read_frame(raw: &str, tail_sq: &str) -> FrameRead {
    let state = composer_state(raw);
    // No composer, or a composer that is not this lane's (the background
    // manager): we cannot read our own message back, so we assert nothing.
    if matches!(state, ComposerState::NotVisible | ComposerState::BackgroundManager) {
        return FrameRead::NoUi;
    }
    // Only REAL input counts as "still there". A dim suggestion that happens to
    // repeat our text is not our message sitting unsent — and treating it as
    // one would make the verifier press Escape+Enter and re-submit a message
    // that already landed.
    let still_there = state.typed().map(|p| p.contains(tail_sq)).unwrap_or(false);
    if !still_there {
        return FrameRead::Cleared;
    }
    if detect_claude_status(raw) == "active" {
        FrameRead::StillThereGenerating
    } else {
        FrameRead::StillThereIdle
    }
}

/// The (ok, message) a send reports for a given verdict. Pure, so the contract
/// "a message we could not confirm is NEVER reported as sent" is asserted
/// directly instead of inferred from a live pane.
/// The durable submission verdict for a send, derived from the outcome string
/// `send_outcome` produced (AMUX-2643).
///
/// Derived from the message rather than threaded as a value because
/// `send_text` returns `(bool, String)` to a long list of callers, and widening
/// that signature to carry a third field would touch every one of them for a
/// column. The cost of deriving is drift: someone edits an outcome string and
/// the mapping silently starts returning None. That is paid for by
/// `every_send_outcome_maps_to_a_verdict`, which enumerates every
/// (Submission, generating, retried) combination through `send_outcome` and
/// fails if any of them stops classifying — so the strings and this function
/// cannot separate without a red test.
///
/// Returns None only for inputs that are not send outcomes at all (the steering
/// deliverer, schedules). None means "not verified", never "failed".
pub(crate) fn submit_verdict_of(msg: &str) -> Option<&'static str> {
    let m = msg.trim();
    if m.starts_with("not submitted") {
        return Some("stuck");
    }
    // Order matters: a retry that SUCCEEDED is still a success, but it must not
    // be smoothed into a clean send — the dropped Enter is the countable signal
    // that this lane's keystroke path is failing.
    if m.contains("on retry") {
        return Some("retried");
    }
    if m.contains("could not be verified") || m == "unverified" {
        return Some("unverified");
    }
    if m.starts_with("sent") {
        return Some("confirmed");
    }
    // "queued (steering) — ..." and friends: nothing was submitted yet, so
    // there is no verdict to record. It stays None: no deliverer stamps one
    // later, despite what this comment used to promise (AMUX-3541).
    None
}

pub(crate) fn send_outcome(sub: Submission, generating: bool, retried: bool) -> (bool, String) {
    match sub {
        Submission::Confirmed if generating && retried => {
            (true, "sent (queued while generating, submitted on retry)".into())
        }
        Submission::Confirmed if generating => (true, "sent (queued while generating)".into()),
        // A retry SUCCEEDED, which is not the same as a clean send: the first
        // Enter was dropped. Callers and the request log see the difference, so
        // "the keystroke path is failing on this lane" is countable instead of
        // being smoothed into a success.
        Submission::Confirmed if retried => (true, "sent (Enter was dropped; submitted on retry)".into()),
        Submission::Confirmed => (true, "sent".into()),
        Submission::Stuck if generating => (
            false,
            "not submitted — text is sitting in the input box (mid-turn Enter was not accepted)".into(),
        ),
        Submission::Stuck => (
            false,
            "not submitted — text is sitting in the input box (autocomplete popup ate the Enter?)".into(),
        ),
        // Honest third state (ethos rule 3): the composer could not be read, so
        // we assert neither success nor failure. `ok` stays true so a caller
        // does not double-send; the message says exactly what is not known and
        // send_post turns it into submitted=null / submission="unverified".
        Submission::Unverified => (
            true,
            "sent (keys delivered; submission could not be verified — no input box was drawn)".into(),
        ),
    }
}

/// RFC3339 / ISO-8601 (`...Z` or `+00:00`) → unix seconds. Kept local: the
/// only consumer is the submission gate, and a wrong parse there must fail
/// CLOSED (None → no evidence) rather than manufacture a timestamp.
fn parse_iso8601(s: &str) -> Option<f64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut y = date.splitn(3, '-');
    let (yy, mm, dd): (i64, i64, i64) =
        (y.next()?.parse().ok()?, y.next()?.parse().ok()?, y.next()?.parse().ok()?);
    // Trailing zone: Z, +HH:MM or -HH:MM.
    let (time, off_s) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (t, z) = rest.split_at(i);
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let z = &z[1..];
        let (zh, zm) = z.split_once(':').unwrap_or((z, "0"));
        (t, sign * (zh.parse::<i64>().ok()? * 3600 + zm.parse::<i64>().ok()? * 60))
    } else {
        (rest, 0)
    };
    let mut tp = time.splitn(3, ':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec: f64 = tp.next().unwrap_or("0").parse().ok()?;
    // days from civil (Howard Hinnant's algorithm) — no chrono in this crate.
    let yy2 = if mm <= 2 { yy - 1 } else { yy };
    let era = if yy2 >= 0 { yy2 } else { yy2 - 399 } / 400;
    let yoe = yy2 - era * 400;
    let doy = (153 * (if mm > 2 { mm - 3 } else { mm + 9 }) + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hh * 3600 + mi * 60 - off_s) as f64 + sec)
}

/// What the evidence says about a just-sent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Submission {
    /// Positively observed as submitted (composer released it, or the JSONL
    /// has it stamped after this send began).
    Confirmed,
    /// Our text is STILL sitting in the input box with no evidence of
    /// submission — this is the AMUX-2629 failure, and callers must NOT
    /// report success.
    Stuck,
    /// Could not tell (no UI drawn yet, capture failed, pane torn). Biased to
    /// "do not double-send" — the caller reports the uncertainty rather than
    /// claiming either outcome.
    Unverified,
}

/// Confirm a just-sent message actually SUBMITTED (py:25421 `_verify_submitted`).
///
/// `send-keys` succeeding only means the keystrokes reached the pane. An
/// autocomplete picker (opened by an @mention) can swallow the Enter, and — the
/// AMUX-2629 specimen — a mid-turn Enter can simply not register, leaving the
/// message unsent in the input box while the API reports "sent" and the
/// steering queue dequeues it. That is the "steering queue clears without
/// delivering" bug.
///
/// Returns `Confirmed` once the input prompt no longer holds our text. If it
/// still does AND the session is idle, a picker likely ate the Enter → press
/// Escape+Enter to submit (`retry_keys`), spaced ≥1.3s from any earlier Escape
/// because two Escapes inside ~1s read as a double-press and EAT the pending
/// message. Biased to `Unverified` rather than `Stuck` when uncertain, so we
/// never double-send.
async fn verify_submitted(
    name: &str,
    text: &str,
    esc_at: Option<std::time::Instant>,
    sent_at: f64,
    retry_keys: bool,
) -> (Submission, bool) {
    let mut retried = false;
    let tail: String = text.trim().chars().rev().take(16).collect::<Vec<_>>().into_iter().rev().collect();
    if tail.is_empty() {
        return (Submission::Confirmed, retried);
    }
    // Compare space-insensitively: the input box hard-wraps long messages at
    // the pane width, splitting the tail across visual lines at arbitrary
    // points.
    let tail_sq: String = tail.split_whitespace().collect();
    let mut esc_at = esc_at;
    let mut cleared_once = false;
    let mut stuck_looks = 0;
    let mut no_ui_looks = 0;
    for _ in 0..5 {
        sleep_ms(300).await;
        let raw = tmux_capture(name, 25).await;
        match read_frame(&raw, &tail_sq) {
            // NO INPUT BOX AT ALL IS "NOT READY", NOT "SUBMITTED" (AC-271). A
            // successful submit leaves the composer rendered and EMPTY — the ❯
            // line is still there. So the absence of any ❯/› means Claude Code
            // has not drawn its UI yet, which on a cold session is normal for
            // several seconds; two "clear looks" 0.3s apart inside that window
            // would report success while the text renders into the box
            // AFTERWARDS and sits there. Measured 2026-08-06 (amux-cloud): a
            // schedule fired into a container that was still waking, send
            // reported "sent", _run_schedule recorded ok, and the worker never
            // ran — 9 workers across 3 customer envs.
            FrameRead::NoUi => {
                no_ui_looks += 1;
                if no_ui_looks <= 12 {
                    // ~4s more; a cold Claude Code needs it
                    sleep_ms(300).await;
                    continue;
                }
                // UI never appeared — do NOT claim this submitted. An "ok" that
                // means "typed into nothing" is what let 9 dead triggers read as
                // successful runs.
                return (Submission::Unverified, retried);
            }
            FrameRead::Cleared => {
                // Looks submitted — but right after a Claude restart the typed
                // text can render into the box AFTER our first look (keystrokes
                // buffered through boot), so one clear look is not proof.
                // Require two.
                if cleared_once {
                    return (Submission::Confirmed, retried);
                }
                cleared_once = true;
                continue;
            }
            // Text still in the box while Claude is generating: it is queued
            // input that submits at the turn end — don't touch it (Escape would
            // interrupt).
            FrameRead::StillThereGenerating => return (Submission::Confirmed, retried),
            FrameRead::StillThereIdle => {}
        }
        cleared_once = false;
        // ONE stuck look is not proof either: for ~1s after a successful submit
        // the pane still shows the echoed text and no spinner yet (worse during
        // a resize repaint), which reads exactly like "stuck + idle". Acting on
        // that single torn look sent Escape — INTERRUPTING the just-started turn,
        // which restores the message into the input box — then Enter, RESUBMITTING
        // it (ts-gke duplicate, 2026-07-13, copies 1.65s apart).
        stuck_looks += 1;
        if stuck_looks < 2 {
            continue;
        }
        // Durable evidence beats the pane: the conversation JSONL gets the user
        // message appended at submission. If it is there stamped after this send
        // began, it submitted and the pane read is a repaint lie.
        if sent_at > 0.0 && jsonl_user_msg_since(name, text, sent_at) {
            return (Submission::Confirmed, retried);
        }
        if !retry_keys {
            return (Submission::Stuck, retried);
        }
        // Idle with our text genuinely stuck → press Escape (closes a picker
        // WITHOUT selecting an entry; a bare Enter would pick one and rewrite an
        // @path) then Enter. Any two Escapes within ~1s read as a double-press
        // and EAT the pending message (v2.1.205), so space each retry's Escape
        // ≥1.3s from the previous one, including the one send_text itself sent.
        if let Some(at) = esc_at {
            let elapsed = at.elapsed();
            if elapsed < Duration::from_millis(1300) {
                tokio::time::sleep(Duration::from_millis(1300) - elapsed).await;
            }
        }
        send_key(name, "Escape").await;
        esc_at = Some(std::time::Instant::now());
        sleep_ms(60).await;
        send_key(name, "Enter").await;
        // A retry is EVIDENCE THE SEND PATH FAILED, not a routine step, so it
        // is logged at WARN and reported back to the caller (`retried` in the
        // send response). A silent retry would hide exactly the signal that
        // says "the keystroke path is dropping Enters on this lane".
        retried = true;
        tracing::warn!(
            session = %name,
            "send: Enter did not submit — retried Escape+Enter (keystroke delivery failure)"
        );
        stuck_looks = 0;
    }
    let raw = tmux_capture(name, 25).await;
    if !matches!(read_frame(&raw, &tail_sq), FrameRead::StillThereIdle) {
        return (Submission::Confirmed, retried);
    }
    // Last resort before reporting a failure (which makes callers re-send):
    // trust the durable JSONL record over a possibly-torn final frame.
    if sent_at > 0.0 && jsonl_user_msg_since(name, text, sent_at) {
        (Submission::Confirmed, retried)
    } else {
        (Submission::Stuck, retried)
    }
}

/// Per-session send lock (py:25432's `_get_send_lock`). Two choreographies
/// typing into one pane interleave into garbage, and — the reason this landed
/// with AMUX-2629 — the ghost-rescue sweep must never fire an Enter into a pane
/// a send is mid-way through typing: that Enter would submit a half-typed
/// message. Separate from `session_op_lock` on purpose: a send must not queue
/// behind a 60s start choreography.
pub(crate) fn session_send_lock(name: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::Mutex<Option<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::Mutex::new(None);
    let mut g = LOCKS.lock().unwrap();
    g.get_or_insert_with(std::collections::HashMap::new)
        .entry(name.to_string())
        .or_default()
        .clone()
}

/// Does this lane have a STRUCTURED PROTOCOL path, where delivery is an ACK
/// instead of a keystroke we hope lands?
///
/// Today the honest answer is `false` for every interactive lane, and the
/// reason is structural rather than a missing wire-up:
/// `opencode::structured::StructuredCliProtocol` runs `claude --print` — one
/// prompt, one child process, no stdin session — so its own
/// `deliver_message` returns `Rejected` for exactly the mid-turn case that
/// dropped Ethan's message on 2026-08-09. It hosts orchestrator WORKERS, not
/// the `~/.amux/sessions/*.env` fleet the send verb addresses, and on this
/// machine at the time of writing `_amux_sessions` held 0 live rows.
///
/// It is a function, not a constant, so the day an interactive lane IS
/// protocol-hosted, the send path and the ghost-rescue sweep both switch on
/// one edit here rather than needing to be found.
pub(crate) fn lane_has_protocol_path(state: &AppState, name: &str) -> bool {
    // A lane is protocol-driven when a live `_amux_sessions` row names its
    // backend ref. `amux-<name>` is the L2 ref shape for tmux-hosted lanes.
    let want = tmux_name(name);
    state
        .store
        .read()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM _amux_sessions WHERE backend_ref = ?1 AND ended_at IS NULL",
                rusqlite::params![want],
                |r| r.get::<_, i64>(0),
            )
            .ok()
        })
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// Every registered, non-archived lane, whatever backend hosts it.
///
/// ONE enumeration for the whole process: `keystroke_lanes` below is a FILTERED
/// view of this, and the board-drive sweep takes it unfiltered. Two independent
/// dir-walks would be two predicates that drift, and a lane visible to one loop
/// and invisible to the other is precisely how a fleet-wide job silently stops
/// covering part of the fleet.
pub(crate) fn all_lane_names() -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(sessions_dir()) else { return vec![] };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("env"))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect();
    names.sort();
    names.retain(|n| parse_env(n).get("CC_ARCHIVED") != Some("1"));
    names
}

/// Lanes whose ONLY delivery channel is keystrokes: registered, not archived,
/// tmux-hosted, and with no structured-protocol session. This is the set the
/// ghost-rescue sweep may act on — and the set that shrinks to nothing when
/// interactive lanes become protocol-driven, which is that job's exit.
pub(crate) fn keystroke_lanes(state: &AppState) -> Vec<String> {
    let mut names = all_lane_names();
    names.retain(|n| {
        let cfg = parse_env(n);
        iterm2_id(&cfg).is_empty()
            && backend_of_cfg(&cfg) == "tmux"
            && !lane_has_protocol_path(state, n)
    });
    names
}

async fn send_after_ready(state: AppState, name: String, text: String, timeout_s: u64) {
    // py:24889 _send_after_ready — wait for Claude's input prompt, then send.
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    while std::time::Instant::now() < deadline {
        let out = tmux_capture(&name, 15).await;
        if !out.is_empty() {
            let clean = strip_ansi(&out);
            if claude_ui_visible(&clean) && !at_resume_picker(&clean) {
                sleep_ms(1200).await;
                let _ = send_text_boxed(&state, &name, &text, false).await;
                return;
            }
        }
        sleep_ms(500).await;
    }
    // TIMED OUT WITH THE MESSAGE UNDELIVERED (AMUX-3055). This path used to
    // return silently, so a dropped create-modal start prompt left NO trace
    // anywhere (no log line, no event, no board card) and the only symptom was
    // an empty session a human had to notice. That invisibility is exactly what
    // the repo's "every bug fix is two fixes" rule exists to kill: the next drop
    // of this class now self-announces in the server log AND the session-events
    // feed, so a sweep finds it without anyone watching the pane.
    tracing::warn!(
        session = %name,
        timeout_s,
        chars = text.chars().count(),
        "send_after_ready: Claude UI never became ready before timeout; start/wake prompt DROPPED undelivered"
    );
    emit_event(
        &state,
        &name,
        "session.prompt_dropped",
        Some(json!({
            "reason": "ui_not_ready_before_timeout",
            "timeout_s": timeout_s,
            "chars": text.chars().count(),
        })),
        None,
        "send-after-ready",
    )
    .await;
}

/// Which delivery mode a send takes: `true` = tmux paste-buffer, `false` =
/// `send-keys -l` (typing).
///
/// Pure, and separated from `send_text_inner` on purpose. The decision
/// otherwise sits in the middle of a ~400-line async fn that needs a live tmux
/// pane to reach, which is precisely the shape that ships untested — and this
/// one shipped wrong (AMUX-2909: short text typed into a generating lane, the
/// mode measured as lossy 1/1).
pub(crate) fn must_paste(generating: bool, chars: usize, picker_shaped: bool) -> bool {
    // `generating` first because it is the one that was missing: mid-turn, paste
    // is the only mode measured as non-lossy, regardless of size or shape.
    generating || chars > 400 || picker_shaped
}

pub(crate) async fn send_text(state: &AppState, name: &str, text: &str, defer_if_busy: bool) -> (bool, String) {
    send_text_inner(state, name, text, defer_if_busy, false, false, false).await
}

/// WHAT "sent" MEANS, as a pure function of the send's own verdict string.
///
/// `send_post` derived `submitted`/`submission` inline, and the scheduler now
/// needs the identical classification to put on a run row. Two spellings of the
/// same fact drift (D6), and this one decides whether an audit row says a
/// command was delivered — so it is computed in ONE place and both callers read
/// it. Returns `(submitted, submission)`; `None` is the honest third state, not
/// a failure (ethos rule 3).
pub(crate) fn submission_verdict(ok: bool, msg: &str) -> (Option<bool>, &'static str) {
    if msg.starts_with("queued") {
        (None, "deferred")
    } else if msg.contains("could not be verified") {
        (None, "unverified")
    } else if ok {
        (Some(true), "confirmed")
    } else {
        (Some(false), "not_submitted")
    }
}

/// What an automated delivery actually did.
///
/// Note there is no `ok` field. `ok` is the send path's "did the request do its
/// job" bit, and it is true for a queued message, for an unverified one, and for
/// a message still sitting in the composer — reading it as "delivered" is the
/// exact conflation that put `status:"ok"` on 20k run rows. The caller must
/// decide from `submitted` and `refused`, which cannot be misread that way.
pub(crate) struct AutoDelivery {
    /// The send path's own verdict string, verbatim — it is what a human reads
    /// in the run history and in the run-now toast.
    pub message: String,
    /// `Some(true/false)` once read back from Claude Code's artifacts; `None`
    /// while queued, or when the composer could not be read.
    pub submitted: Option<bool>,
    pub submission: &'static str,
    /// The steering-queue row this message is waiting in, when queued. It is the
    /// only handle anyone has on a message that has not landed yet, so it rides
    /// on the run row rather than being discoverable only by grepping a table.
    pub queue_id: Option<String>,
    /// Terminal refusal: nothing was delivered and nothing is pending. The
    /// caller must record this as a REFUSAL, never as a run that succeeded.
    pub refused: bool,
}

/// THE DELIVERY ENTRY POINT FOR AUTOMATED SENDERS (AMUX-2647) — the scheduler.
///
/// Deliberately NOT `send_text(.., defer_if_busy = true)`, which is the HTTP
/// send's rule: that only defers on a live SELECTOR, so a merely GENERATING lane
/// falls straight through and gets typed into mid-turn. For a human pressing
/// send that is a considered trade; for a scheduled command nobody is watching
/// it is how a fire lands in the middle of someone's turn at 3am.
///
/// So an automated send follows the STEERING rule instead: deliver at a turn
/// boundary, otherwise park on the steering queue and let `steer_deliver_loop`
/// own the wait — including the `AMUX_STEER_MAX_AGE_S` deadline past which the
/// message goes into the running turn anyway. That loop is also the only
/// delivery path with a retry behind it, which is what a fire needs: the
/// scheduler has already advanced `next_run` by the time we get here, so a
/// message dropped on the floor is not fired again.
///
/// Three outcomes and no fourth: delivered (verified), queued (with the row id),
/// or refused (with the reason). `ok == true` never means "delivered" on its
/// own — read `submitted`.
pub(crate) async fn deliver_automated(
    state: &AppState,
    name: &str,
    text: &str,
    guard: &str,
) -> AutoDelivery {
    let refuse = |why: String| AutoDelivery {
        message: why,
        submitted: Some(false),
        submission: "not_submitted",
        queue_id: None,
        refused: true,
    };
    let classify = |ok: bool, msg: String| {
        let (submitted, submission) = submission_verdict(ok, &msg);
        AutoDelivery { message: msg, submitted, submission, queue_id: None, refused: !ok }
    };

    if name.trim().is_empty() {
        return refuse("schedule has no target session".into());
    }
    if !env_path(name).exists() {
        return refuse(format!("target '{name}' is not a registered session"));
    }
    // A schedule must never be a wake path for an ARCHIVED lane (Ethan,
    // 2026-08-02). The send path would refuse anyway, but refusing here keeps
    // the reason in the run row instead of surfacing as a nightly `error`
    // forever. Unarchiving is a human's call (ethos rule 8).
    if parse_env(name).get("CC_ARCHIVED") == Some("1") {
        return refuse(format!("target '{name}' is archived — not delivered, not woken"));
    }
    // A stopped (but not archived) lane is `send_text`'s auto-wake path, exactly
    // as under Python. It must NOT be queued: `steer_deliver_loop` skips lanes
    // that are not running and leaves the row pending, so a queued command for a
    // stopped lane would wait forever while the run row claimed it was pending.
    if !is_running(name).await {
        let (ok, msg) = send_text(state, name, text, false).await;
        return classify(ok, msg);
    }

    // Age 0: a fire is new, so it is never overdue yet — the deadline belongs to
    // the steering loop, which re-evaluates it every tick with the real age.
    let decision = steer_delivery_for(state, name, 0.0).await;
    if decision == SteerDelivery::AtBoundary {
        // `from_steering = true` makes the callee REFUSE rather than type into a
        // turn that started between the gate and the send. A lost race then
        // falls through to the queue below instead of being reported as failed.
        let (ok, msg) = send_text_inner(state, name, text, false, true, false, false).await;
        if ok {
            return classify(ok, msg);
        }
        tracing::info!(
            session = %name, reason = %msg,
            "automated send lost the boundary race — parking on the steering queue"
        );
    }
    // No sender lane: this is an automated producer, and `guard` already names
    // which one. An empty sender means the stall notice has no lane to tell —
    // which is the truth, rather than a guess that would misattribute it.
    let queue_id = steer_enqueue(state, name, text, guard, "").await;
    AutoDelivery {
        message: format!("queued (steering) — delivers to '{name}' at its next turn boundary"),
        submitted: None,
        submission: "deferred",
        queue_id: Some(queue_id),
        refused: false,
    }
}

fn send_text_boxed<'a>(
    state: &'a AppState,
    name: &'a str,
    text: &'a str,
    defer_if_busy: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = (bool, String)> + Send + 'a>> {
    Box::pin(send_text_inner(state, name, text, defer_if_busy, false, false, false))
}

async fn send_text_inner(
    state: &AppState,
    name: &str,
    text: &str,
    defer_if_busy: bool,
    from_steering: bool,
    // OVERDUE delivery (AMUX-2642): the message has waited past
    // `AMUX_STEER_MAX_AGE_S` for a boundary that is not coming, so it goes into
    // the running turn and Claude Code folds it in at its own boundary. Only
    // the steering tick sets this, and only after the deadline.
    allow_mid_turn: bool,
    // The caller already knows the session is idle (e.g. the Stop hook just
    // reported it). Skip the pane-based generating checks — the hook report
    // IS the authority (D1 exit: reported state outranks the scrape).
    hook_confirmed_idle: bool,
) -> (bool, String) {
    let cfg = parse_env(name);
    if !iterm2_id(&cfg).is_empty() {
        return (false, "iTerm2-backed sessions are not supported by the rust origin yet".into());
    }
    if backend_of_cfg(&cfg) == "herdr" {
        return herdr_send(name, text).await;
    }
    let boot_in_flight = {
        let meta = load_meta(name);
        let last_started = meta.get("last_started").and_then(|v| v.as_i64()).unwrap_or(0);
        now_i64() - last_started < 20
    };
    let mut out_st = tmux_capture(name, 15).await;
    if !out_st.is_empty() && at_resume_picker(&strip_ansi(&out_st)) {
        return (false, "session is in resume picker".into());
    }
    // The background-conversation manager (opened with `←`) draws its OWN
    // composer over the lane, and it starts a NEW task rather than messaging
    // this session — so typing here silently addresses the wrong thing.
    //
    // COLLAPSE, VERIFY, THEN DELIVER (AMUX-2681). The original guard refused
    // and named the key. That is safe but UNACTIONABLE: the only actor who can
    // press esc is a human at the pane, and the entire point of a lane is that
    // nobody is at the pane. Measured cost of the pure refusal, from
    // `_amux_request_log`: the `amux` lane sat unreachable for 5.04h across 12
    // sends from 3 distinct clients (2026-08-10 06:35:49 -> 11:37:58) with
    // steering queued behind it and 22 todo cards untouched.
    //
    // ESCAPE IS THE ONLY KEY PRESSED. The footer also offers `enter` (opens the
    // SELECTED row — a DIFFERENT conversation if the cursor has moved) and
    // `ctrl+x` (deletes all). Escape returns to this conversation, and on an
    // idle lane with an empty composer it is a no-op. That no-op is what makes
    // it safe when the DETECTOR is wrong, which it demonstrably is: AMUX-2680
    // had the headline matching a lane's own prose for 6.6h, and tmux rewrap
    // can still land that phrase at a line start.
    //
    // ...but the no-op holds ONLY while the lane is idle. Escape mid-turn
    // INTERRUPTS Claude Code, so pressing it on a false positive would upgrade
    // "unreachable" to "work destroyed" — the detector paying its cost in the
    // same resource as the fault (ethos rule 7). So the keypress is gated on
    // the frame NOT reading active, and a generating lane is refused instead.
    //
    // THE VERDICT IS A POSITIVE RE-READ, NEVER A TIMER: after the keypress we
    // re-capture and require `composer_state` to have LEFT BackgroundManager.
    // If it has not, we refuse exactly as before — so this can only ever add a
    // delivery that used to fail, never lose one that used to work.
    if !out_st.is_empty() && composer_state(&out_st) == ComposerState::BackgroundManager {
        let generating = detect_claude_status(&out_st) == "active";
        let mut collapsed = false;
        if bg_view_collapse_enabled() && !generating {
            send_key(name, "Escape").await;
            sleep_ms(400).await;
            let re = tmux_capture(name, 15).await;
            if !re.is_empty() && composer_state(&re) != ComposerState::BackgroundManager {
                collapsed = true;
                out_st = re;
            }
        }
        if !collapsed {
            return (false, bg_view_refusal(generating));
        }
    }
    let mut needs_wake = false;
    if !out_st.is_empty() && at_shell_prompt(&strip_ansi(&out_st)) {
        needs_wake = true; // terminal visible but Claude has exited
    } else if !is_running(name).await {
        needs_wake = true;
    }
    if needs_wake {
        if boot_in_flight {
            let st2 = state.clone();
            let (n, t) = (name.to_string(), text.to_string());
            tokio::spawn(async move { send_after_ready(st2, n, t, 30).await });
            return (true, "sent (waiting for in-flight boot)".into());
        }
        if !env_path(name).exists() {
            return (false, "not running".into());
        }
        // Auto-wake parity (py:25463): start, then deliver once ready.
        let (ok, msg) = start_session(state, name, "", false).await;
        if !ok {
            return (false, format!("auto-wake failed: {msg}"));
        }
        let st2 = state.clone();
        let (n, t) = (name.to_string(), text.to_string());
        tokio::spawn(async move { send_after_ready(st2, n, t, 60).await });
        return (true, "sent (auto-woke)".into());
    }
    let mut text = text.to_string();
    if text.is_empty() {
        // Suggested-prompt extraction (py:25501): pull the ❯ suggestion.
        let pane = tmux_capture(name, 0).await;
        let clean = strip_ansi(&pane);
        // A SELECTOR IS NOT A SUGGESTION. ACCEPT THE HIGHLIGHTED OPTION WITH AN
        // ENTER KEYPRESS (AMUX-3054, generalising AMUX-2952). An empty send is
        // the user pressing "Enter" at the picker. Whatever the picker's shape,
        // numbered ("❯ 1. Submit answers"), a bare label ("❯ Yes"), or a
        // footered list, the correct action is the Enter KEY, which accepts the
        // highlighted default. Extracting the ❯ line and DELIVERING IT AS TEXT
        // re-types the label into a UI that reads KEY events, and since
        // AMUX-2909 pastes picker-shaped panes the label lands as one
        // bracketed-paste event that the picker swallows whole, so the Enter
        // never lands, silently, while the response still says "sent". AMUX-2952
        // pressed Enter for NUMBERED options only; a non-numbered highlighted
        // option still slipped through to the paste path, and a footered picker
        // returned "no suggestion found" so a composer empty-send with no client
        // fallback did nothing. Gate on the SELECTOR STATE, not on the option's
        // numbering, so every picker shape is covered by one rule.
        //
        // A rate-limit menu is ALSO a "waiting" selector, but it has its own
        // handler below that STAMPS `rate_limited_since` before pressing Enter
        // (AMUX-2820). Returning here would drop that stamp, so it is excluded
        // and falls through to the dedicated path.
        if detect_claude_status(&pane) == "waiting" && !is_rate_limit_menu(&pane) {
            let (ok, msg) = send_keys_op(name, "Enter").await;
            // WARN, not info: an empty send landing on a selector is exactly the
            // AMUX-3054 report, and this line is the countable signal a log sweep
            // reads to see the class recur (the two-fix rule). `ok=false` here is
            // a genuine keystroke-delivery failure worth paging on.
            tracing::warn!(
                session = %name, ok, detail = %msg,
                "[picker-enter/AMUX-3054] empty send at a selector: pressed Enter to accept the \
                 highlighted option instead of pasting a label; keys must reach a picker as a KEYPRESS"
            );
            return (
                ok,
                if ok {
                    "pressed Enter on the picker (accepted the highlighted option)".into()
                } else {
                    format!("picker Enter failed: {msg}")
                },
            );
        }
        let nonblank: Vec<&str> = clean.lines().filter(|l| !l.trim().is_empty()).collect();
        let footer: Vec<&str> = nonblank[nonblank.len().saturating_sub(4)..]
            .iter()
            .filter(|l| !l.trim_start().starts_with('\u{276f}'))
            .copied()
            .collect();
        if footer.iter().any(|l| {
            let ll = l.to_lowercase();
            ll.contains("to navigate") || ll.contains("enter to select")
        }) {
            return (true, "no suggestion found".into());
        }
        for line in clean.lines().rev() {
            let line = line.trim();
            if line.starts_with('\u{276f}') || line.starts_with('>') {
                let suggested = line.trim_start_matches(['\u{276f}', '>', '\u{a0}', ' ']).trim();
                if !suggested.is_empty() {
                    // A NUMBERED OPTION IS A PICKER, NOT A SUGGESTION
                    // (AMUX-2952, Ethan live: "i keep sending enter when it
                    // needs input and its not doing anything").
                    //
                    // At an AskUserQuestion selector the ❯ line reads
                    // "❯ 1. Submit answers". Extracting that as a suggestion
                    // and DELIVERING IT AS TEXT re-types the label into a UI
                    // that reads KEY EVENTS — and since AMUX-2909 routes
                    // picker-shaped panes to bracketed PASTE, the whole string
                    // is swallowed in one piece. Nothing happens, silently,
                    // every time. The footer guard above ("enter to select")
                    // does not save us: AskUserQuestion's review screen has no
                    // such footer.
                    //
                    // What accepting a highlighted option actually takes is the
                    // Enter KEY. Scoped to options that start "N." so a plain
                    // suggested prompt ("❯ retry the failing test") still goes
                    // through the text path unchanged.
                    //
                    // AMUX-3054 note: the SELECTOR-STATE check above now catches
                    // every picker `detect_claude_status` recognises as
                    // "waiting", numbered or not, so reaching HERE means a
                    // numbered picker that detection did NOT classify as waiting.
                    // That is a `detect_claude_status` gap, so this fallback logs
                    // WARN. A nonzero count is the tell that the detector needs
                    // the shape added, and it stops the label ever being pasted.
                    let is_numbered = suggested
                        .split_once('.')
                        .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
                    if is_numbered {
                        let (ok, msg) = send_keys_op(name, "Enter").await;
                        tracing::warn!(
                            session = %name, ok, detail = %msg,
                            "[picker-enter/AMUX-2952] empty send at a numbered selector detect_claude_status \
                             missed: pressed Enter instead of pasting the label (detection gap)"
                        );
                        return (
                            ok,
                            if ok {
                                format!("pressed Enter on the picker (highlighted: {suggested})")
                            } else {
                                format!("picker Enter failed: {msg}")
                            },
                        );
                    }
                    text = suggested.to_string();
                    break;
                }
            }
        }
        if text.is_empty() {
            return (true, "no suggestion found".into());
        }
    }
    let (mut generating, mut waiting) = if hook_confirmed_idle {
        (false, false)
    } else {
        let status = detect_claude_status(&tmux_capture(name, 12).await);
        (status == "active", status == "waiting")
    };
    // A RATE-LIMIT MENU IS NOT A QUESTION FOR THE USER — answer it and carry on
    // (AMUX-2820). Without this the lane deadlocks: the send parks on the
    // selector, nothing else dismisses a menu, and every later send queues
    // behind the same one. Live specimen: mvs-infra held two messages for 400s+
    // and pressing Enter in the dashboard only added a third.
    if waiting {
        let pane = tmux_capture(name, 30).await;
        // The resume-mode selector is amux's to answer, same as the rate-limit
        // menu below (D2; policy set once by Ethan 2026-08-19). Answering here
        // unblocks THIS send instead of parking it behind a selector the sweep
        // would only clear on its next pass.
        if resume_mode_action() != "off" && is_resume_mode_prompt(&pane) {
            let (ok, msg) = send_keys_op(name, "1").await;
            tracing::warn!(
                session = %name, ok, detail = %msg,
                "answered the resume-mode selector with '1. Resume from summary' — amux owns this prompt (D2)"
            );
            emit_event(
                state,
                name,
                "session.resume_mode_answered",
                Some(json!({"choice": "resume-from-summary", "ok": ok, "detail": msg})),
                None,
                "status",
            )
            .await;
            if ok {
                waiting = false;
            }
        }
        if rate_limit_action() != "off" && is_rate_limit_menu(&pane) {
            // STAMP IT BEFORE ANSWERING, so the condition is visible even if the
            // keypress fails — and so a lane whose policy is `off` still shows
            // up as limited rather than merely stuck. /api/sessions reads these
            // keys for credit_limited (AMUX-2820); without the stamp the whole
            // fleet reports healthy while a lane is parked.
            update_meta(
                name,
                &[
                    ("rate_limited_since", json!(now_i64())),
                    ("rate_limited_model", json!(parse_env(name).get_or("CC_MODEL", ""))),
                ],
            );
            // Enter selects the highlighted default, option 1 "stop and wait".
            // NOT 2 or 3 — those spend money, and choosing to spend is a human's
            // call however cheap it looks from here (ethos rule 8).
            let (ok, msg) = send_keys_op(name, "Enter").await;
            tracing::warn!(
                session = %name, ok, detail = %msg,
                "answered the rate-limit menu with 'stop and wait' — amux owns this prompt (D2)"
            );
            emit_event(
                state,
                name,
                "session.rate_limit_answered",
                Some(json!({"choice": "stop-and-wait", "ok": ok, "detail": msg})),
                None,
                "rate-limit",
            )
            .await;
            if ok {
                // The menu is gone, but the lane is blocked until the limit resets.
                // Do NOT clear the stamp here: the rate_limit_sweep clears it when
                // the lane RESUMES generating (its own activity), so the status
                // persists for the blocked window without any banner-word scrape.
                // Only unblock this send.
                waiting = false;
            }
        }
    }
    if defer_if_busy && waiting {
        // A live selector parks an automated send (py:25545; the
        // AskUserQuestion kill of 2026-07-15).
        steer_enqueue(state, name, &text, "", "").await;
        return (true, "queued (steering) — session at a selector, delivers when it resolves".into());
    }
    if waiting && from_steering {
        // A live selector is NOT overridden by the deadline: typing here and
        // the picker-closing Escape would REJECT the pending tool, which is a
        // destructive answer to a question the user has not answered
        // (AskUserQuestion kill, 2026-07-15). Overdue or not, it waits.
        return (false, "session at a selector — retry at next idle boundary".into());
    }
    // PICKER-SHAPED TEXT IS NOT SPECIAL ANY MORE — it just has to be PASTED.
    //
    // py:25676 parks @/slash text whenever the lane is generating, because
    // TYPING it opens Claude Code's autocomplete, the picker eats the Enter and
    // the message sits unsubmitted (the "15:06 @image ghost", 2026-07-10), and
    // the picker-closing Escape would interrupt the turn. That is all true of
    // keystrokes. Measured on a live pane 2026-08-09: typed mid-turn,
    // `@/Users/…/README.md` was lost 1/1 — the composer held it and the turn
    // ended without it. The SAME text delivered mid-turn via
    // `load-buffer` + `paste-buffer -p` was accepted 4/4, each one recorded by
    // Claude Code as a `queue-operation: enqueue` and folded into the turn.
    //
    // A bracketed paste is not a keystroke sequence, so the autocomplete never
    // opens and there is no picker to fight. So the fix is not a better guard
    // around the picker, it is not opening it: picker-shaped text takes the
    // paste path regardless of length. This is what makes the overdue delivery
    // below safe for the three of amux's five queued messages that carry an
    // `@`, which under the old branch could never be delivered to a busy lane
    // at all.
    //
    // `use_paste` is computed BELOW, after the generating re-check, because
    // being mid-turn is itself a reason to paste (AMUX-2909).
    let mut esc_at: Option<std::time::Instant> = None;
    // Exclusive use of the pane for the whole type+Enter+verify choreography.
    // Without it a second send (or the ghost-rescue sweep) can fire an Enter
    // between our `send-keys -l` and our own Enter, submitting a half-typed
    // message. Taken here, AFTER every early return above, so a refusal never
    // holds the lock.
    let send_lock = session_send_lock(name);
    let _send_guard = send_lock.lock().await;
    // `sent_at` bounds the JSONL evidence window: an OLDER identical message
    // (a second "continue" minutes later) must not count as this send.
    let sent_at = now_f64();
    if !generating {
        // Fresh re-check right before the Escape (py:25597): "esc to interrupt"
        // in the STATUS BAR is the reliable generating signal. Scoped to the
        // bar, never the transcript — see `pane_bar_says_generating` for the
        // four-hour queue freeze the unscoped substring match caused.
        //
        // Skipped when hook_confirmed_idle: the hook report IS the authority
        // (D1 exit). Re-scraping the pane here overrode the hook for sessions
        // idle with background agents ("esc to interrupt" on the bar from
        // agents, not from generation), freezing the steering queue for 2h+.
        if !hook_confirmed_idle && pane_bar_says_generating(&tmux_capture(name, 12).await) {
            generating = true;
            if from_steering && !allow_mid_turn {
                return (false, "session started generating — retry at next turn boundary".into());
            }
        } else if !waiting {
            send_key(name, "Escape").await;
            esc_at = Some(std::time::Instant::now());
            sleep_ms(50).await;
        }
    }
    // A GENERATING LANE ALWAYS GETS THE PASTE PATH (AMUX-2909). Ethan's report:
    // a message typed into a lane that is visibly working. The delivery MODE is
    // the whole defect — the measurement recorded a few lines above is amux's
    // own, on a live pane: typed mid-turn, the text was LOST 1/1 (the composer
    // held it and the turn ended without it); the SAME text pasted mid-turn was
    // accepted 4/4, each recorded by Claude Code as `queue-operation: enqueue`
    // and folded into the turn. That evidence was already in this file, and the
    // paste path was still gated on length/picker-shape alone, so every SHORT
    // human message — the dashboard composer's whole output — took the lossy
    // mode precisely when the lane was busiest.
    //
    // Note what this deliberately is NOT: a deferral to the next turn boundary.
    // Claude Code already has a queue and enqueues pasted text itself, so
    // holding the message would add latency and duplicate a queue the harness
    // owns — amux's job is to hand the text over in the form the harness
    // accepts, not to build a second queue in front of it. Idle lanes are
    // untouched: they still type, with no added latency.
    let use_paste = must_paste(generating, text.chars().count(), at_picker_text(&text));
    if generating {
        // Mid-turn delivery is the risky case, so make every instance
        // self-announcing rather than reconstructable (ethos rule 4 / the
        // two-fix rule): before this, nothing recorded WHICH MODE a message was
        // delivered in, so "was it typed or pasted, and was the lane mid-turn?"
        // — the exact question this bug turns on — could not be answered from
        // anything amux kept. A sweep can now count mid-turn deliveries, and
        // `mode="type"` here should be structurally impossible.
        // AEAB-25. This was a single `warn!` whose TEXT asserted "mode=type here is
        // a regression" while the structured field carried the mode that actually
        // happened. 25 of 25 records, all-time, were mode="paste" — the mode the
        // same sentence calls correct. It has never once fired on the condition it
        // describes, so grepping it returns only healthy deliveries, and a REAL
        // mode=type would be one line among them, indistinguishable.
        //
        // The comment above says the intent: make every mid-turn delivery
        // self-announcing so a sweep can count them, and `mode="type"` "should be
        // structurally impossible". That is instrumentation, not an alarm. So the
        // record stays for every delivery — the sweep keeps working — and only the
        // genuinely impossible case is a WARN.
        if use_paste {
            tracing::info!(
                session = %name, mode = "paste", chars = text.chars().count(),
                from_steering, allow_mid_turn,
                "delivering to a GENERATING lane by paste — the mode measured as \
                 non-lossy mid-turn (AMUX-2909)"
            );
        } else {
            tracing::warn!(
                session = %name, mode = "type", chars = text.chars().count(),
                from_steering, allow_mid_turn,
                "delivering to a GENERATING lane by TYPE — paste is the only mode \
                 measured as non-lossy mid-turn (AMUX-2909); this is the regression"
            );
        }
    }
    send_key(name, "C-u").await;
    sleep_ms(40).await;
    if use_paste {
        // Named tmux buffer + paste-buffer -p (py:25630). Also the picker-safe
        // path — see `use_paste` above.
        let buf_name = format!("amux-{}-{}", name, (now_f64() * 1000.0) as i64);
        let tmp = std::env::temp_dir().join(format!("{buf_name}.txt"));
        if std::fs::write(&tmp, &text).is_err() {
            return (false, "could not stage paste buffer".into());
        }
        let tmp_s = tmp.to_string_lossy().into_owned();
        let ptq = pt(name);
        let ok1 = matches!(tmux(&["load-buffer", "-b", &buf_name, &tmp_s]).await, Some(o) if o.status.success());
        let ok2 = ok1
            && matches!(
                tmux(&["paste-buffer", "-p", "-b", &buf_name, "-t", &ptq]).await,
                Some(o) if o.status.success()
            );
        let _ = tmux(&["delete-buffer", "-b", &buf_name]).await;
        let _ = std::fs::remove_file(&tmp);
        if !ok2 {
            return (false, "paste-buffer failed".into());
        }
    } else if !send_literal(name, &text).await {
        return (false, "send-keys failed".into());
    }
    sleep_ms(20).await;
    // Only reachable if picker-shaped text was TYPED, which `use_paste` now
    // prevents. Kept as a belt-and-braces closer rather than deleted: if a
    // future change routes picker text back through send-keys, the Escape that
    // makes it submittable is still here.
    if !generating && !use_paste && at_picker_text(&text) {
        // Close the autocomplete picker so Enter submits (py:25655), spaced
        // ≥1.3s from the leading Escape — a closer pair eats the message.
        if let Some(at) = esc_at {
            let elapsed = at.elapsed();
            if elapsed < Duration::from_millis(1300) {
                tokio::time::sleep(Duration::from_millis(1300) - elapsed).await;
            }
        }
        send_key(name, "Escape").await;
        sleep_ms(60).await;
    }
    send_key(name, "Enter").await;
    // ------------------------------------------------------------------
    // THE EVIDENCE GATE (AMUX-2629). Everything above proves only that bytes
    // reached the pty. "sent" is a claim about Claude Code's state, so it is
    // read back from Claude Code's own artifacts — the composer contents and
    // the conversation JSONL — never inferred from the send-keys exit code.
    //
    // The mid-turn branch is NOT exempt, and that exemption is what shipped
    // the incident: Python returned "sent (queued while generating)" without
    // looking, Rust inherited it, and on 2026-08-09 a mid-turn Enter simply
    // did not register — the message sat in amux-rust's composer for 10m50s
    // (typed 20:55:25, entered the transcript 21:06:15 only when a human
    // pressed Enter) while the API had answered 200 {"ok":true}.
    //
    // Mid-turn we retry with a BARE Enter and never an Escape: Escape mid-turn
    // is an INTERRUPT that kills the running response (py:25597's warning, and
    // this session's own "[Request interrupted by user]" records). Idle, the
    // Escape+Enter pair is correct because a picker may be holding the Enter.
    // ------------------------------------------------------------------
    let (first, retried) = verify_submitted(name, &text, esc_at, sent_at, !generating).await;
    if first == Submission::Stuck && generating {
        // One bare-Enter retry, then re-read the evidence. No sleep-tuning: the
        // retry is gated on the OBSERVED composer contents, not on a guess
        // about how long Claude Code needs. Bare Enter and never Escape —
        // Escape mid-turn is an interrupt.
        tracing::warn!(
            session = %name,
            "send: mid-turn Enter was not accepted — retrying with a bare Enter (keystroke delivery failure)"
        );
        send_key(name, "Enter").await;
        let (second, _) = verify_submitted(name, &text, None, sent_at, false).await;
        return send_outcome(second, generating, true);
    }
    send_outcome(first, generating, retried)
}

/// py:25815 send_keys — allowed control keys only.
const ALLOWED_TMUX_KEYS: [&str; 47] = [
    "Enter", "Escape", "Tab", "BTab", "Space", "BSpace", "Up", "Down", "Left", "Right", "Home",
    "End", "PageUp", "PageDown", "IC", "DC", "C-c", "C-d", "C-z", "C-l", "C-a", "C-e", "C-k",
    "C-u", "C-r", "C-p", "C-n", "C-b", "C-f", "C-w", "C-o", "C-x", "M-b", "M-f", "M-d", "F1",
    "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
];
// "1": the resume-mode auto-answer selects BY DIGIT (see resume_mode_action —
// Enter would take whatever is highlighted, including "Don't ask me again").
const ALLOWED_TMUX_CHAR_KEYS: [&str; 5] = ["y", "n", "q", "x", "1"];

async fn send_keys_op(name: &str, keys: &str) -> (bool, String) {
    if !is_running(name).await {
        return (false, "not running".into());
    }
    if !ALLOWED_TMUX_KEYS.contains(&keys) && !ALLOWED_TMUX_CHAR_KEYS.contains(&keys) {
        return (false, format!("key '{keys}' not in allowed set"));
    }
    let ptq = pt(name);
    match tmux(&["send-keys", "-t", &ptq, keys]).await {
        Some(o) if o.status.success() => (true, "sent".into()),
        Some(o) => (false, String::from_utf8_lossy(&o.stderr).into_owned()),
        None => (false, "timeout sending keys".into()),
    }
}

// ---------------------------------------------------------------------------
// start_session (py:24218) — the launch choreography. Claude path faithful
// (resume via cc_conversation_id/cc_session_name-less UUID, --name fresh
// start, MCP registry, profile sourcing, HISTFILE, pipe-pane logging, the
// resume-picker/fresh-retry fallback). codex/gemini: command construction
// ported minus provider trust/memory side effects (gaps named). herdr: 501.
// ---------------------------------------------------------------------------

fn mcp_registry_path() -> Option<PathBuf> {
    // py:24151 — ~/.amux/mcp.json, seeded by Python from the repo. The rust
    // origin only CONSUMES an existing registry; seeding stays Python's.
    let p = home().join("mcp.json");
    if p.exists() { Some(p) } else { None }
}

fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
}

fn tmux_cols() -> String {
    std::env::var("AMUX_TMUX_COLS").ok().filter(|v| v.parse::<u32>().is_ok()).unwrap_or_else(|| "220".into())
}
fn tmux_rows() -> String {
    std::env::var("AMUX_TMUX_ROWS").ok().filter(|v| v.parse::<u32>().is_ok()).unwrap_or_else(|| "50".into())
}

/// The `pipe-pane` writer program (py:21478 `_log_pipe_command` was
/// redaction-only; AMUX-2628 rewrote it).
///
/// **Why this is not `for line in sys.stdin.buffer`.** That is what shipped,
/// and it froze every log on the fleet for over an hour with `pane_pipe=1`
/// and the writer process alive — the failure looks exactly like "piping is
/// fine, the session just went quiet". `readline()` blocks until a **line
/// feed**, and a full-screen TUI redraws in place with CARRIAGE RETURNS and
/// cursor-positioning escapes: measured on a real `amux-frustrations.log`,
/// 106,081 CR bytes against 2,506 LF bytes, a 42:1 ratio. So the reader sat
/// on a partial "line" accumulating megabytes in its internal buffer (the two
/// actively-working lanes' writers were carrying ~3MB of RSS above the idle
/// baseline) and wrote nothing. Reproduced on a throwaway pane: 221 bytes on
/// disk after 4,000 CR-terminated frames, then 175,118 bytes the instant one
/// LF arrived.
///
/// Note that `python3 -u` does NOT fix this — `-u` unbuffers stdout, and the
/// block is on the READ side. The fix has to be chunked reads plus treating
/// CR as a terminator, which is what this program does.
///
/// It also strips ANSI at write time, because the raw capture was 54.6%
/// escape bytes and unreadable as a log. Cursor MOVEMENT becomes whitespace
/// rather than being deleted (`G`/`H`/`C` alone account for 530k sequences in
/// that one file); deleting it jams the reflowed words together
/// ("whoseoutcomeisartifacts"). Set `AMUX_LOG_RAW=1` to keep the byte stream
/// verbatim. Real-capture measurement: 11,291,687 -> 4,663,903 bytes (58.7%).
///
/// The knobs are INTERPOLATED, not read from the environment by the program:
/// `pipe-pane` children are forked by the **tmux server**, so they inherit
/// tmux's environment and would never see `~/.amux/server.env`.
fn log_pipe_command(log_path: &Path) -> String {
    const PROG: &str = r#"import os,re,select,sys,time
LOG=sys.argv[1]; MAXB=int(sys.argv[2]); RAW=sys.argv[3]=='1'; FLUSH=int(sys.argv[4])/1000.0
SEC=re.compile(rb'((?:mxp|usr|ret)_sk)_[A-Za-z0-9_-]+|((?:AMUX_MIXPEEK_OPS_TOKEN|ANTHROPIC_API_KEY|OPENAI_API_KEY|GOOGLE_MAPS_API_KEY|GOOGLE_API_KEY|CLOUDFLARE_API_TOKEN|ELEVENLABS_API_KEY|POSTHOG_KEY|POSTHOG_PERSONAL_API_KEY)=)[^\s\r\n]+|(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]+|sk-ant-[A-Za-z0-9_-]+|sk-proj-[A-Za-z0-9_-]+|sk[_-][A-Za-z0-9]{32,}|AIza[0-9A-Za-z_-]{30,}|(?:phx|phc)_[A-Za-z0-9]+')
def repl(m):
    if m.group(1): return m.group(1)+b'_REDACTED'
    if m.group(2): return m.group(2)+b'REDACTED'
    return b'SECRET_REDACTED'
CSI=re.compile(rb'\x1b\[[0-9;?]*([a-zA-Z])')
def csi(m):
    f=m.group(1)
    if f in b'GCHfd': return b' '
    if f in b'ABEF': return b'\n'
    return b''
OTHER=re.compile(rb'\x1b\]8;[^\x1b]*\x1b\\|\x1b\][^\x07]*\x07|\x1b\][^\x1b]*\x1b\\|\x1b[()][A-Z0-9]|\x1b[\x20-\x2f]*[\x40-\x7e]')
CTL=re.compile(rb'[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]')
PAD=re.compile(rb'(?<=\S)[ \t]{2,}')
fh=open(LOG,'ab',0)
def rot():
    global fh
    if MAXB<=0: return
    try: n=os.fstat(fh.fileno()).st_size
    except OSError: return
    if n<MAXB: return
    try:
        fh.close(); os.replace(LOG,LOG+'.1')
    except OSError: pass
    fh=open(LOG,'ab',0)
    fh.write(b'=== amux log rotated '+time.strftime('%Y-%m-%dT%H:%M:%S').encode()+b' ('+str(n).encode()+b' bytes rolled to .1) ===\n')
prev=None
def emit(seg,redraw):
    global prev
    s=SEC.sub(repl,seg)
    if RAW:
        try: fh.write(s+b'\n')
        except OSError: return
        rot(); return
    s=PAD.sub(b' ',CTL.sub(b'',OTHER.sub(b'',CSI.sub(csi,s))))
    parts=s.split(b'\n')
    for i,p in enumerate(parts):
        p=p.rstrip()
        if not p.strip(): continue
        if (redraw or i<len(parts)-1) and p==prev: continue
        prev=p
        try: fh.write(p+b'\n')
        except OSError: return
        rot()
if RAW:
    while True:
        c=os.read(0,65536)
        if not c: break
        try: fh.write(SEC.sub(repl,c))
        except OSError: break
        rot()
    sys.exit(0)
buf=b''; last_rx=time.monotonic(); held=None; HARD=1<<20
while True:
    r,_,_=select.select([0],[],[],0.25)
    eof=False
    if r:
        c=os.read(0,65536)
        if c: buf+=c; last_rx=time.monotonic()
        else: eof=True
    while buf:
        j=buf.find(b'\n'); k=buf.find(b'\r')
        if j<0 and k<0: break
        if k>=0 and (j<0 or k<j):
            if k==len(buf)-1 and not eof: break
            if buf[k+1:k+2]==b'\n': emit(buf[:k],False); buf=buf[k+2:]
            else: emit(buf[:k],True); buf=buf[k+1:]
        else: emit(buf[:j],False); buf=buf[j+1:]
        held=None
    now=time.monotonic()
    held=(held or now) if buf else None
    if buf:
        if len(buf)>HARD or eof or now-last_rx>=FLUSH:
            emit(buf,True); buf=b''; held=None
        elif now-held>=FLUSH*4:
            i=max(buf.rfind(b' '),buf.rfind(b'\t'))
            if i>=0: emit(buf[:i+1],True); buf=buf[i+1:]; held=now
    if eof: break
if buf: emit(buf,True)
"#;
    format!(
        "python3 -c {} {} {} {} {}",
        sh_quote(PROG),
        sh_quote(&log_path.to_string_lossy()),
        log_rotate_bytes(),
        if log_raw_capture() { 1 } else { 0 },
        log_flush_ms(),
    )
}

/// Size cap for a single session log before it rolls to `<name>.log.1`.
/// `AMUX_LOG_MAX_MB=0` disables rotation. Default 32MB: the incident's own
/// specimen was an 11MB file with no rotation at all, and two generations at
/// 32MB is a bounded 64MB per session.
fn log_rotate_bytes() -> u64 {
    std::env::var("AMUX_LOG_MAX_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(32) * 1024 * 1024
}

/// `AMUX_LOG_RAW=1` keeps the verbatim byte stream (colour, cursor escapes)
/// instead of readable text. Secrets are still redacted in both modes.
fn log_raw_capture() -> bool {
    matches!(std::env::var("AMUX_LOG_RAW").unwrap_or_default().trim(), "1" | "true" | "yes")
}

/// How long the pane must be QUIET before an unterminated trailing fragment
/// is written out. Deliberately a quiet-detector rather than a periodic
/// timer: a periodic flush cuts live spinner frames in half mid-word.
fn log_flush_ms() -> u64 {
    std::env::var("AMUX_LOG_FLUSH_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v >= 100)
        .unwrap_or(2000)
}

async fn poll_shell_prompt(name: &str, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while std::time::Instant::now() < deadline {
        let out = tmux_capture(name, 5).await;
        if !out.is_empty() && at_shell_prompt(&strip_ansi(&out)) {
            return true;
        }
        sleep_ms(150).await;
    }
    false
}

async fn type_line(name: &str, line: &str) {
    let _ = send_literal(name, line).await;
    sleep_ms(100).await;
    send_key(name, "Enter").await;
}

fn build_claude_cmd(cfg: &EnvFile, flags: &str, default_flags: &str, session_flag: &str, extra_flags: &str) -> String {
    let custom = std::env::var("AMUX_CLAUDE_CMD").unwrap_or_default().trim().to_string();
    let mut cmd = if custom.is_empty() { "claude".to_string() } else { custom };
    // Session flags override defaults — see dedupe_default_flags for the
    // 2026-08-09 duplicate --model incident this prevents.
    let default_flags = dedupe_default_flags(default_flags, &[flags, session_flag, extra_flags]);
    if !default_flags.is_empty() {
        cmd = format!("{cmd} {}", shell_quote_flags(&default_flags));
    }
    if !flags.is_empty() {
        cmd = format!("{cmd} {}", shell_quote_flags(flags));
    }
    if !session_flag.is_empty() {
        cmd = format!("{cmd} {session_flag}");
    }
    if !extra_flags.is_empty() {
        cmd = format!("{cmd} {}", shell_quote_flags(extra_flags));
    }
    // ISOLATED (AMUX-3232): a raw agent gets no MCP servers injected. The
    // whole harness is stripped, and `--mcp-config` is part of it. Guarded on
    // the worker's own env, the same map CC_MCP itself lives in.
    if !env_flag_on(cfg.get("CC_ISOLATED")) {
        let mcp_val = cfg.get_or("CC_MCP", "").trim().to_lowercase();
        if !matches!(mcp_val.as_str(), "off" | "none" | "0") {
            if let Some(reg) = mcp_registry_path() {
                cmd = format!("{cmd} --mcp-config {}", sh_quote(&reg.to_string_lossy()));
            }
        }
        if mcp_val == "chrome" {
            let chrome = home().join("mcp-chrome.json");
            if chrome.exists() {
                cmd = format!("{cmd} --mcp-config {}", sh_quote(&chrome.to_string_lossy()));
            }
        }
    }
    if !cmd.contains("--model") {
        cmd = format!("{cmd} --model sonnet");
    }
    cmd
}

/// Per-session choreography lock — Python parity (`_get_session_lock`,
/// py:24231 wraps the whole start choreography).
///
/// Why (2026-08-09 amux incident): the rust origin had NO lock, so a model
/// swap's restart choreography (stop → relaunch) interleaved with a second
/// Start pressed from the dashboard, and two choreographies typed into the
/// SAME pane concurrently — the session log shows repeated respawned shells,
/// relaunches and instant exits over an 80-second window, which the owner
/// experienced as "the fresh claude exited immediately". Each start/stop now
/// owns the pane exclusively; a queued second start finds claude running and
/// returns "already running" instead of typing over a healthy boot.
fn session_op_lock(name: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::Mutex<Option<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::Mutex::new(None);
    let mut g = LOCKS.lock().unwrap();
    g.get_or_insert_with(std::collections::HashMap::new)
        .entry(name.to_string())
        .or_default()
        .clone()
}

/// Seed per-directory folder-trust so a freshly-spawned claude in `work_dir`
/// does not stop at the first-run "Do you trust the files in this folder?"
/// dialog (AC-346). Claude persists trust as
/// `projects[<dir>].hasTrustDialogAccepted=true` in `~/.claude.json`; there is
/// NO global flag (amux-cloud confirmed the config surface — trust is
/// per-project only), so a static Dockerfile cannot cover future verticals'
/// dirs. It must be seeded at launch, when CC_DIR is known.
///
/// STRICTLY FAIL-OPEN. This runs on the path that spawns EVERY worker, so a
/// missing / locked / malformed `~/.claude.json` must NEVER block a launch: every
/// error returns quietly and the worst case is the status quo (the gate is not
/// bypassed), never a wedged launch. Merge-preserving — the whole document is
/// read, ONE nested field is set, and it is written back, so `oauthAccount`,
/// other projects, and the theme are untouched. A present-but-unparseable file
/// is left ALONE rather than risk clobbering real state. No-op locally where the
/// dir is already trusted (the common case), so it is a genuine single-codebase
/// no-op there, not an env branch.
fn seed_dir_trust(work_dir: &str) {
    if work_dir.is_empty() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = std::path::Path::new(&home).join(".claude.json");
    let doc: Value = match std::fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(_) => return, // malformed: never clobber a file we cannot safely merge
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(_) => return,
    };
    let Some(updated) = trust_seed_merge(doc, work_dir) else {
        // None => already trusted (no write) or an unmergeable shape (left alone).
        return;
    };
    let Ok(body) = serde_json::to_string(&updated) else { return };
    // Atomic write: temp + rename, so a concurrent reader (another worker's
    // claude) never sees a truncated document.
    let tmp = path.with_extension("json.amux-trust-tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Pure merge for [`seed_dir_trust`], split out so the shape handling is tested
/// without touching `$HOME`. Returns `Some(doc)` with
/// `projects[dir].hasTrustDialogAccepted=true` folded in, or `None` when it is
/// already `true` (nothing to write) or the document is not a mergeable object
/// (leave the real file alone).
fn trust_seed_merge(mut doc: Value, dir: &str) -> Option<Value> {
    let root = doc.as_object_mut()?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    let entry = projects
        .entry(dir.to_string())
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    if entry.get("hasTrustDialogAccepted") == Some(&Value::Bool(true)) {
        return None;
    }
    entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    Some(doc)
}

/// Codex analog of [`seed_dir_trust`] (AC-346 / AMUX-3159 seed direction). A
/// `codex` process — which is how BOTH the `codex` and `ollama` providers launch
/// (ollama runs `codex --oss --local-provider ollama`) — stops at its own
/// "Do you trust the contents of this directory?" dialog the first time it runs
/// in a directory. codex persists that decision in `~/.codex/config.toml` as
///
/// ```toml
/// [projects."/abs/dir"]
/// trust_level = "trusted"
/// ```
///
/// (surface confirmed 2026-08-15: every dir codex has trusted, including amux's
/// own e2e dirs, is recorded in exactly this shape). So SEED it up front and the
/// worker starts straight at the composer — make the state correct rather than
/// detect the picker and press Enter, which is the D1 terminal-scraping pattern
/// the ethos is trying to leave (amux, 2026-08-15).
///
/// APPEND-ONLY and strictly FAIL-OPEN. This runs on the worker-spawn path, so a
/// missing / unreadable / unparseable config must NEVER block a launch. It only
/// APPENDS a `[projects."<dir>"]` table when the dir has NO entry yet, so codex's
/// own `model` / `personality` / `[notice]` settings and every other project are
/// preserved byte-for-byte — nothing existing is rewritten. A dir codex (or the
/// user) has ALREADY decided on, trusted OR untrusted, is left untouched: we do
/// not override an existing choice, and appending a duplicate table header would
/// be a TOML error.
fn seed_codex_dir_trust(work_dir: &str) {
    if work_dir.is_empty() {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else { return };
    let path = std::path::Path::new(&home).join(".codex/config.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return, // unreadable: fail-open, never block a launch
    };
    if codex_dir_already_known(&text, work_dir) {
        return;
    }
    // Append the trust table without touching a single existing byte.
    let mut body = text;
    if !body.is_empty() {
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push('\n');
    }
    body.push_str(&format!("[projects.\"{work_dir}\"]\ntrust_level = \"trusted\"\n"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic write: temp + rename, so a concurrent codex reader never sees a torn
    // file (same discipline as seed_dir_trust).
    let tmp = path.with_extension("toml.amux-trust-tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Whether codex's config already records ANY trust decision for `dir`. PARSED,
/// not a text scan, so header spacing/quoting cannot fool it. Returns `true` when
/// the dir has an entry — seeding is then a no-op that respects codex's or the
/// user's existing choice — and, crucially, `true` for an UNPARSEABLE config too:
/// we refuse to append into a file we cannot understand rather than risk a
/// duplicate table or corruption. Pure, so it is tested without a real `~/.codex`.
fn codex_dir_already_known(config_text: &str, dir: &str) -> bool {
    let Ok(doc) = config_text.parse::<toml::Value>() else {
        return true; // fail-safe: never append into a file we can't parse
    };
    doc.get("projects")
        .and_then(|p| p.as_table())
        .map(|projects| projects.contains_key(dir))
        .unwrap_or(false)
}

/// The CHEAP, synchronous reasons a session cannot start — the ones knowable
/// without the op-lock or a tmux query. Returns the human reason, or None if
/// nothing cheap blocks the launch.
///
/// This exists because `POST /api/sessions/<n>/start` answers an optimistic 202
/// `{ok:true,"starting"}` BEFORE `start_session` runs (AMUX-2557), so a start
/// that was always going to be refused — an archived scaffold, a blocked lane —
/// reported success to the caller and its refusal reached only the event stream,
/// never `server-rs.log`: indistinguishable from a real start (AMUX-3364, found
/// on cloud POC containers). The `/start` handler now calls this SYNCHRONOUSLY
/// and answers `{ok:false,error}` for these cases, so the caller learns the truth
/// and the refusal is logged.
///
/// SEAM NOTE (ethos D6): these predicates are ALSO checked inside `start_session`,
/// which stays authoritative — it additionally holds the op-lock and does the
/// async `is_running` check (a running session is "already running", a success,
/// which is why is_running is deliberately NOT here). This is a small, stable
/// mirror of five invariant preconditions, not a second control plane; keep the
/// two in step if a precondition is added.
pub(crate) fn start_block_reason(name: &str, cfg: &EnvFile) -> Option<String> {
    if !valid_session_name(name) {
        return Some("invalid session name".into());
    }
    if !env_path(name).exists() {
        return Some(format!("session '{name}' not found"));
    }
    if is_session_blocked(name) {
        return Some("session is blocked; remove it from blocked-sessions.txt first".into());
    }
    if !iterm2_id(cfg).is_empty() {
        return Some("iTerm2-backed sessions are not supported by the rust origin yet".into());
    }
    if backend_of_cfg(cfg) == "herdr" {
        return Some("herdr-backed session start is not ported to the rust origin yet".into());
    }
    if cfg.get("CC_ARCHIVED") == Some("1") {
        return Some("session is archived; wake it first".into());
    }
    None
}

async fn start_session(state: &AppState, name: &str, extra_flags: &str, skip_conv_id: bool) -> (bool, String) {
    if !valid_session_name(name) {
        return (false, "invalid session name".into());
    }
    // Serialize with any concurrent start/stop on this session — see
    // session_op_lock for the incident this prevents. Held for the whole
    // choreography; restart_for_swap composes stop+start sequentially so each
    // leg takes the lock in turn (no re-entrancy).
    let op_lock = session_op_lock(name);
    let _op = op_lock.lock().await;
    if is_session_blocked(name) {
        return (false, "session is blocked; remove it from blocked-sessions.txt first".into());
    }
    let f = env_path(name);
    if !f.exists() {
        return (false, format!("session '{name}' not found"));
    }
    let cfg = parse_env(name);
    // ISOLATED (AMUX-3232): computed once here from the worker's own env so the
    // spawn path can strip the harness (env injection below, --mcp-config in
    // build_claude_cmd). Read from cfg, the same map every other CC_* flag on
    // this launch uses.
    let isolated = env_flag_on(cfg.get("CC_ISOLATED"));
    if !iterm2_id(&cfg).is_empty() {
        return (false, "iTerm2-backed sessions are not supported by the rust origin yet".into());
    }
    if backend_of_cfg(&cfg) == "herdr" {
        return (
            false,
            "herdr-backed session start is not ported to the rust origin yet (gap named in api/session_verbs.rs)".into(),
        );
    }
    if is_running(name).await {
        return (true, "already running".into());
    }
    if cfg.get("CC_ARCHIVED") == Some("1") {
        return (false, "session is archived; wake it first".into());
    }
    let work_dir = {
        let wd = cfg.get_or("CC_DIR", "").trim();
        let wd = if wd.is_empty() {
            std::env::var("HOME").unwrap_or_default()
        } else {
            wd.to_string()
        };
        expanduser(&wd)
            .canonicalize()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| expanduser(&wd).to_string_lossy().into_owned())
    };
    // AC-346: seed per-directory folder-trust so the claude about to launch in
    // work_dir does not stop at the first-run "trust this folder?" dialog. Runs
    // BEFORE the pane starts claude; strictly fail-open (see seed_dir_trust).
    seed_dir_trust(&work_dir);
    // AMUX-3159 seed direction (codex analog of AC-346): a codex/ollama worker
    // launches `codex`, which has its OWN "trust this directory?" dialog. Seed its
    // trust config up front so the worker starts at the composer instead of parking
    // on the picker — make the state correct, don't detect-and-dismiss (D1). Same
    // fail-open discipline; only touches ~/.codex for codex-launching providers.
    if matches!(provider_of(&cfg).as_str(), "codex" | "ollama") {
        seed_codex_dir_trust(&work_dir);
    }
    let mut flags = cfg.get_or("CC_FLAGS", "").to_string();
    #[cfg(unix)]
    {
        // Claude Code rejects --dangerously-skip-permissions as root (py:24242).
        if libc_geteuid() == 0 && flags.contains("--dangerously-skip-permissions") {
            flags = flags.replace("--dangerously-skip-permissions", "").trim().to_string();
        }
    }
    let mut meta = load_meta(name);
    let provider = provider_of(&cfg);
    let uuid_re = cached_re!(r"^[0-9a-fA-F-]{36}$");
    // Resume strategy (py:24250-24295). The Rust origin resolves via the
    // conversation UUID (deterministic, hook-reported); the name-indexed
    // lookup Python layers on top needs its session-name index, which is a
    // coexistence gap — a stale UUID still falls back to a fresh --name start.
    let mut session_flag = String::new();
    if !skip_conv_id && provider == "claude" {
        let conv_id = meta_str(&meta, "cc_conversation_id");
        let mut resumable = false;
        if !conv_id.is_empty() && uuid_re.is_match(&conv_id) {
            let conv_file =
                claude_home().join("projects").join(project_name(&work_dir)).join(format!("{conv_id}.jsonl"));
            if conv_file.exists() {
                resumable = true;
            }
        }
        session_flag = claude_session_flag(name, &conv_id, resumable);
    }
    let defaults = EnvFile::load(&home().join("defaults.env"));
    let default_flags = defaults.get_or("CC_DEFAULT_FLAGS", "").to_string();

    // The launch binary per provider, from the single source the health
    // invariant also reads (RR-0043 / AMUX-3153). The arms below build from it
    // so the check and the launcher cannot disagree about what gets run.
    let base_bin = launch_base_binary(&provider);
    let cmd = match provider.as_str() {
        "codex" => {
            // py:24380 — codex command construction (trust-db side effect not
            // ported).
            let codex_session_id = meta_str(&meta, "codex_session_id");
            let mut codex_flags = flags.clone();
            let codex_yolo = PROVIDER_YOLO_FLAGS.iter().any(|f| codex_flags.contains(f));
            if codex_yolo {
                codex_flags = strip_provider_yolo_flags(&codex_flags);
            }
            let mut opts = String::new();
            if !codex_flags.is_empty() {
                opts += &format!(" {}", shell_quote_flags(&codex_flags));
            }
            if !extra_flags.is_empty() {
                opts += &format!(" {}", shell_quote_flags(extra_flags));
            }
            if !opts.contains("--model") && !opts.contains("-m ") {
                opts += " --model gpt-5.5";
            }
            if !opts.contains("--dangerously-bypass") && !opts.contains("-a ") {
                opts += if codex_yolo { " --dangerously-bypass-approvals-and-sandbox" } else { " -a never" };
            }
            if !codex_yolo && !opts.contains("--dangerously-bypass") && !opts.contains("--sandbox") && !opts.contains("-s ") {
                opts += " --sandbox workspace-write";
            }
            let logs = logs_dir().to_string_lossy().into_owned();
            if !opts.contains(&logs) {
                opts += &format!(" --add-dir {}", sh_quote(&logs));
            }
            if let Some(gr) = run_cmd("git", &["-C", &work_dir, "rev-parse", "--show-toplevel"], OP_TIMEOUT).await {
                if gr.status.success() {
                    let root = String::from_utf8_lossy(&gr.stdout).trim().to_string();
                    if root != work_dir && !opts.contains(&root) {
                        opts += &format!(" --add-dir {}", sh_quote(&root));
                    }
                    let git_dir = format!("{root}/.git");
                    if Path::new(&git_dir).is_dir() && !opts.contains(&git_dir) {
                        opts += &format!(" --add-dir {}", sh_quote(&git_dir));
                    }
                }
            }
            if !codex_session_id.is_empty() {
                format!("{base_bin} resume{opts} {codex_session_id}")
            } else {
                format!("{base_bin}{opts}")
            }
        }
        "gemini" => {
            // py:24443 — gemini command (GEMINI.md memory bridge not ported).
            let mut gflags = flags.clone();
            let gyolo = PROVIDER_YOLO_FLAGS.iter().any(|f| gflags.contains(f))
                || gflags.contains("--approval-mode=yolo")
                || gflags.contains("--approval-mode yolo");
            if gyolo {
                gflags = strip_provider_yolo_flags(&gflags);
            }
            let mut opts = String::new();
            if !gflags.is_empty() {
                opts += &format!(" {}", shell_quote_flags(&gflags));
            }
            if !extra_flags.is_empty() {
                opts += &format!(" {}", shell_quote_flags(extra_flags));
            }
            if !opts.contains("--model") && !opts.contains("-m ") {
                opts += " --model auto";
            }
            if gyolo && !opts.contains("--yolo") && !opts.contains("--approval-mode") {
                opts += " --yolo";
            }
            if !opts.contains("--skip-trust") {
                opts += " --skip-trust";
            }
            let logs = logs_dir().to_string_lossy().into_owned();
            opts += &format!(" --include-directories {}", sh_quote(&logs));
            let gemini_session_id = meta_str(&meta, "gemini_session_id");
            if !gemini_session_id.is_empty() {
                format!("{base_bin}{opts} --resume {}", sh_quote(&gemini_session_id))
            } else {
                let new_id = ulid::Ulid::new().to_string().to_lowercase();
                meta.insert("gemini_session_id".into(), json!(new_id));
                format!("{base_bin}{opts} --session-id {}", sh_quote(&new_id))
            }
        }
        "ollama" => {
            // Ollama workers run through `codex --oss --local-provider ollama`
            // so they get a full coding agent (file editing, hooks, structured
            // events) instead of a bare `ollama run` REPL. (RR-0043 / AMUX-3153)
            // Model comes from CC_MODEL (env_config.rs routes the worker's model
            // field there); falls back to the provider default (qwen3.8:27b).
            let model = {
                let m = cfg.get_or("CC_MODEL", "").trim().to_string();
                if m.is_empty() { default_model_for_provider("ollama") } else { m }
            };
            // An ollama worker's model belongs in CC_MODEL (read just above); a
            // `--model` in CC_FLAGS is inert here — this arm launches with
            // CC_MODEL and never appends CC_FLAGS, so that flag is silently
            // ignored and the worker runs a DIFFERENT model than a CC_FLAGS-based
            // view (its own row) reports.
            //
            // This WARN is LOAD-BEARING, not merely defensive (amux-frustrations,
            // AMUX-3182 review). The create path can STILL produce this input: a
            // caller who passes explicit `flags` containing `--model X` gets
            // CC_FLAGS="--model X" AND CC_MODEL=<model> written together
            // (worker_model_env honours explicit flags verbatim per AMUX-3114),
            // and env_config::render_worker_env has no explicit-flags concept at
            // all, so the two env routes DIVERGE for exactly this shape. This is
            // the only thing standing between that input and a silently-wrong
            // model, so do NOT read "AMUX-3182 fixed the create path" as licence
            // to delete it. It also self-announces any residual pre-fix worker,
            // so a sweep of /api/logs finds it rather than a human noticing the
            // wrong model first (the two-fixes rule).
            if flags.contains("--model") {
                tracing::warn!(
                    session = %name,
                    cc_flags = %flags,
                    cc_model = %model,
                    "ollama worker carries a --model in CC_FLAGS (inert; model comes from CC_MODEL) — explicit-flags or pre-fix mis-wire, see AMUX-3182"
                );
            }
            let ollama_yolo = PROVIDER_YOLO_FLAGS.iter().any(|f| flags.contains(f));
            let mut opts = format!(" --oss --local-provider ollama --model {}", sh_quote(&model));
            if !opts.contains("--dangerously-bypass") && !opts.contains("-a ") {
                opts += if ollama_yolo { " --dangerously-bypass-approvals-and-sandbox" } else { " -a never" };
            }
            // Default codex sandbox is read-only; explicitly opt into workspace-write
            // so file editing actually works (matches the codex arm's same logic at
            // line ~4867; without this flag writes are OS-sandboxed away).
            if !ollama_yolo && !opts.contains("--dangerously-bypass") && !opts.contains("--sandbox") && !opts.contains("-s ") {
                opts += " --sandbox workspace-write";
            }
            // The global ~/.codex/config.toml commonly sets
            // model_reasoning_effort=xhigh, which HANGS local ollama models (they
            // have no extended-thinking path — a worker sits "Working" forever,
            // AH-81). Force low here so the SERVER launch path applies it. The
            // OllamaAdapter (static_providers.rs, 3fc489c) already does this, but
            // the launch goes through THIS arm, not the adapter's build_command —
            // the adapter-vs-launch-arm drift the launch-matches-adapter invariant
            // guards (AMUX-3155). Adding it here is what makes a dashboard- or
            // (post-AMUX-3164 convergence) CLI-launched ollama worker responsive.
            if !opts.contains("model_reasoning_effort") {
                opts += " -c model_reasoning_effort=low";
            }
            if let Some(gr) = run_cmd("git", &["-C", &work_dir, "rev-parse", "--show-toplevel"], OP_TIMEOUT).await {
                if gr.status.success() {
                    let root = String::from_utf8_lossy(&gr.stdout).trim().to_string();
                    if root != work_dir && !opts.contains(&root) {
                        opts += &format!(" --add-dir {}", sh_quote(&root));
                    }
                    let git_dir = format!("{root}/.git");
                    if Path::new(&git_dir).is_dir() && !opts.contains(&git_dir) {
                        opts += &format!(" --add-dir {}", sh_quote(&git_dir));
                    }
                }
            }
            format!("{base_bin}{opts}")
        }
        _ => build_claude_cmd(&cfg, &flags, &default_flags, &session_flag, extra_flags),
    };

    // Shell setup line (py:24532): unset Claude env markers, source profile,
    // cd, source the global agent credentials.
    let mut has_oauth = false;
    let mut shell_rc = String::new();
    if provider != "codex" && provider != "gemini" && provider != "ollama" {
        shell_rc.push_str("unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT; ");
        if let Ok(t) = std::fs::read_to_string(PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                has_oauth = py_truthy(&v["oauthAccount"]);
            }
        }
        if has_oauth {
            shell_rc.push_str("unset ANTHROPIC_API_KEY; ");
        }
    }
    let home_dir = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    for rc in [".zprofile", ".bash_profile", ".profile"] {
        let p = home_dir.join(rc);
        if p.exists() {
            shell_rc.push_str(&format!(
                "source {} 2>/dev/null; cd {}; ",
                sh_quote(&p.to_string_lossy()),
                sh_quote(&work_dir)
            ));
            break;
        }
    }
    let amux_env = home().join("amux.env");
    if amux_env.exists() {
        shell_rc.push_str(&format!(
            "set -a; source {} 2>/dev/null; set +a; ",
            sh_quote(&amux_env.to_string_lossy())
        ));
    } else {
        shell_rc.push_str(&format!("cd {}; ", sh_quote(&work_dir)));
    }
    // SCOPE LAYERS: global -> group(s) -> worker (AMUX-3106).
    //
    // `/api/scope` has advertised `env` at ["global","group","worker"] since the
    // cutover and the scope UI writes all three files (scope.rs `env_file`), but
    // launch sourced ONLY the global `amux.env` above — so setting anything at
    // group or worker level wrote a file that never reached the running session.
    // It saved, and it changed nothing. Same shape as the standing-order bug that
    // produced `scoped_setting_in`: that fixed the READ path for one key at a
    // time; this fixes DELIVERY of the whole layer into the process.
    //
    // Ordering is the mechanism, not a detail: `source` lets the LAST assignment
    // win, so global-then-group-then-worker yields worker > group > global —
    // exactly `scoped_setting_in`'s precedence. If the two ever disagreed, a key
    // would resolve one way when a gate read it and another way inside the lane's
    // shell, which is the kind of split nobody would think to look for.
    //
    // Groups come from CC_TAGS in the worker's own env file — the same source
    // `lane_groups` reads. Deliberately not a second spelling of "which groups
    // is this in".
    //
    // This is the prerequisite the connectors design names (docs/design/connectors.md,
    // "The one real gap"): scoping a connector to a worker has nowhere to land
    // until below-global scope actually reaches the worker.
    //
    // The global layer is sourced just above (unchanged, so its `cd` fallback
    // behaviour is untouched); scope_env_layers returns it too, so skip it here
    // rather than sourcing it twice.
    for f in scope_env_layers(&home(), name) {
        if f == amux_env {
            continue;
        }
        shell_rc.push_str(&format!(
            "set -a; source {} 2>/dev/null; set +a; ",
            sh_quote(&f.to_string_lossy())
        ));
    }
    if provider != "codex" && provider != "gemini" && provider != "ollama" && has_oauth {
        shell_rc.push_str("unset ANTHROPIC_API_KEY; ");
    }
    let mut env_args: Vec<String> = Vec::new();
    if has_oauth {
        env_args.push("-e".into());
        env_args.push("ANTHROPIC_API_KEY=".into());
    } else if let Ok(v) = std::env::var("ANTHROPIC_API_KEY") {
        if !v.is_empty() {
            env_args.push("-e".into());
            env_args.push(format!("ANTHROPIC_API_KEY={v}"));
        }
    }
    for k in [
        "ANTHROPIC_API_BASE", "OPENAI_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY",
        "GOOGLE_GENAI_USE_VERTEXAI", "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION",
    ] {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() {
                env_args.push("-e".into());
                env_args.push(format!("{k}={v}"));
            }
        }
    }

    let tmux_sess = tmux_name(name);
    let tmux_exists = tmux_sessions_set().await.contains(&tmux_sess);
    if tmux_exists {
        // Reuse the surviving tmux session (py:24589).
        let output = tmux_capture(name, 10).await;
        if at_shell_prompt(&strip_ansi(&output)) {
            send_key(name, "C-c").await;
            sleep_ms(100).await;
            send_key(name, "C-u").await;
            sleep_ms(100).await;
            type_line(name, "HISTFILE=/dev/null").await;
            poll_shell_prompt(name, 3000).await;
            type_line(name, &format!("cd {}", sh_quote(&work_dir))).await;
            poll_shell_prompt(name, 3000).await;
        } else {
            send_key(name, "C-c").await;
            sleep_ms(3000).await;
            let out2 = tmux_capture(name, 10).await;
            if !at_shell_prompt(&strip_ansi(&out2)) {
                let ptq = pt(name);
                let sh = user_shell();
                let _ = tmux(&["respawn-pane", "-k", "-t", &ptq, &sh]).await;
                sleep_ms(1000).await;
                type_line(name, &shell_rc).await;
                poll_shell_prompt(name, 3000).await;
            } else {
                send_key(name, "C-u").await;
                sleep_ms(100).await;
                type_line(name, "HISTFILE=/dev/null").await;
                poll_shell_prompt(name, 3000).await;
                type_line(name, &format!("cd {}", sh_quote(&work_dir))).await;
                poll_shell_prompt(name, 3000).await;
            }
        }
    } else {
        // Fresh tmux session hosting the user's login shell (py:24647).
        let cols = tmux_cols();
        let rows = tmux_rows();
        let scheme = if std::env::args().any(|a| a == "--no-tls") { "http" } else { "https" };
        let mut args: Vec<String> = vec![
            "new-session".into(), "-d".into(), "-s".into(), tmux_sess.clone(),
            "-n".into(), name.into(), "-c".into(), work_dir.clone(),
            "-x".into(), cols, "-y".into(), rows,
        ];
        // ISOLATED (AMUX-3232): a raw agent is "just tmux plus Claude Code". The
        // amux harness reaches a lane through these four env vars. AMUX_SESSION
        // and AMUX_URL are what the global Claude Code hooks
        // (Stop/UserPromptSubmit/PostToolUse self-report) and every `amux`
        // command key on, so NOT injecting them is the root-cause suppression:
        // the hooks no-op, board attribution has no session to name, and the
        // session cannot discover the fleet. The OWNER still peeks/sends because
        // amux drives the pane with send-keys + capture from OUTSIDE, which needs
        // nothing set inside the session. Suppressing at the injection point (not
        // downstream) is why a better model does not route around it.
        if isolated {
            tracing::info!(
                session = %name,
                "spawn: ISOLATED worker, amux harness suppressed (no AMUX_SESSION/AMUX_URL/AMUX_WORKER env, no --mcp-config); owner peek/send still work, peers cannot discover or target it"
            );
        } else {
            args.extend([
                "-e".into(), format!("TMUX_SESSION_NAME={name}"),
                "-e".into(), format!("AMUX_WORKER={name}"),
                "-e".into(), format!("AMUX_SESSION={name}"),
                // The port THIS server answers on, never a literal: a new lane
                // must reach the server that started it. The old hardcoded 8822
                // outlived its own deployment: it kept minting the retired
                // address into every new session locally, and it forced the
                // cloud image to bind 8822 to match (cloud/docker/Dockerfile
                // named this line).
                "-e".into(), format!("AMUX_URL={scheme}://localhost:{}", crate::config::canonical_port()),
            ]);
        }
        args.extend(env_args.iter().cloned());
        args.push(user_shell());
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        match run_cmd("tmux", &args_ref, Duration::from_secs(10)).await {
            Some(o) if o.status.success() => {}
            Some(o) => return (false, String::from_utf8_lossy(&o.stderr).into_owned()),
            None => return (false, "tmux not found or timed out".into()),
        }
        let stq = st(name);
        let _ = tmux(&["set-option", "-t", &stq, "remain-on-exit", "on"]).await;
        let _ = tmux(&["set-option", "-t", &stq, "allow-rename", "off"]).await;
        let _ = tmux(&["set-window-option", "-t", &stq, "automatic-rename", "off"]).await;
        let _ = tmux(&["rename-window", "-t", &stq, name]).await;
        type_line(name, &shell_rc).await;
        poll_shell_prompt(name, 3000).await;
    }
    if has_oauth && provider != "codex" && provider != "gemini" {
        type_line(name, "unset ANTHROPIC_API_KEY").await;
        poll_shell_prompt(name, 3000).await;
    }
    // Launch the provider command.
    let _ = send_literal(name, &cmd).await;
    sleep_ms(150).await;
    send_key(name, "Enter").await;
    // Wait for the agent UI (py:24717).
    let mut launched = false;
    for i in 0..20 {
        sleep_ms(500).await;
        let out = tmux_capture(name, 10).await;
        if !out.is_empty() {
            let clean = strip_ansi(&out);
            if claude_ui_visible(&clean) {
                launched = true;
                break;
            }
            if i >= 10 && at_shell_prompt(&clean) {
                break;
            }
            if i >= 6 && at_resume_picker(&clean) {
                break;
            }
        }
    }
    if !launched && !skip_conv_id && provider == "claude" {
        let mut out_check = strip_ansi(&tmux_capture(name, 10).await);
        if at_resume_picker(&out_check) {
            // Escape ONLY on a positive picker match (the ⌕ glyph + picker
            // text, at_resume_picker) — never on a mere launch timeout. A slow
            // boot (MCP startup) exhausts the watch loop looking exactly like
            // this branch's entry condition, and an Escape/C-c fired into a
            // healthy booting TUI kills it (2026-08-09 hardening).
            send_key(name, "Escape").await;
            sleep_ms(500).await;
            send_key(name, "C-c").await;
            sleep_ms(2000).await;
            for _ in 0..10 {
                let o = strip_ansi(&tmux_capture(name, 10).await);
                if at_shell_prompt(&o) {
                    break;
                }
                sleep_ms(500).await;
            }
            meta.remove("cc_session_name");
            meta.remove("cc_conversation_id");
            save_meta(name, &meta);
            out_check = strip_ansi(&tmux_capture(name, 10).await);
        } else if !at_shell_prompt(&out_check) && pane_has_live_child(name).await == Some(true) {
            // Not a picker, not a shell, and the pane's shell HAS a child:
            // claude is alive but its UI was not recognized within the watch
            // window (slow MCP boot, an unrecognized frame). Give it one more
            // window instead of falling through to a retry that would C-c a
            // healthy process. Scrape says nothing; the process table says
            // running — trust the process table (ethos rule 7: positive
            // evidence over a timed-out detector).
            for _ in 0..10 {
                sleep_ms(500).await;
                let o = strip_ansi(&tmux_capture(name, 10).await);
                if claude_ui_visible(&o) {
                    launched = true;
                    break;
                }
                if at_shell_prompt(&o) {
                    break;
                }
            }
            out_check = strip_ansi(&tmux_capture(name, 10).await);
        }
        // Retype only on POSITIVE evidence claude is gone: the frame reads as
        // a shell prompt AND the pane shell is childless. at_shell_prompt
        // alone can false-positive on TUI frames whose bottom lines end in
        // '%'/'$' (context-percent status lines), and a C-c + full command
        // retyped into a live claude becomes a garbage prompt to the model.
        if !launched
            && at_shell_prompt(&out_check)
            && pane_has_live_child(name).await != Some(true)
        {
            // --resume failed: fresh start with --name (py:24762).
            meta.remove("cc_session_name");
            meta.remove("cc_conversation_id");
            save_meta(name, &meta);
            send_key(name, "C-c").await;
            sleep_ms(100).await;
            send_key(name, "C-u").await;
            sleep_ms(100).await;
            let fresh_flag = format!("--name {}", sh_quote(name));
            let cmd_fresh = build_claude_cmd(&cfg, &flags, &default_flags, &fresh_flag, extra_flags);
            let _ = send_literal(name, &cmd_fresh).await;
            sleep_ms(150).await;
            send_key(name, "Enter").await;
            for _ in 0..10 {
                sleep_ms(500).await;
                let out2 = tmux_capture(name, 10).await;
                if !out2.is_empty() && claude_ui_visible(&strip_ansi(&out2)) {
                    launched = true;
                    break;
                }
            }
            if !launched {
                let out3 = strip_ansi(&tmux_capture(name, 10).await);
                if at_shell_prompt(&out3) {
                    meta.insert("start_error".into(), json!("both resume and fresh start failed"));
                    save_meta(name, &meta);
                    return (false, "Claude failed to start".into());
                }
            }
        }
    }
    // CODEX SELF-UPDATE EXIT (AMUX-2921, observed live 2026-08-11): on a
    // version bump codex updates itself via npm, prints "🎉 Update ran
    // successfully! Please restart Codex." and EXITS — the launch watch
    // times out on a bare shell and the lane is dead while start_session
    // reports started. Relaunch ONCE, gated on the positive marker AND a
    // childless shell — never on the timeout alone, which is the same
    // no-blind-retype rule the claude fallback above follows (a retype into
    // a live TUI is a garbage prompt to the model).
    if !launched && provider == "codex" {
        let o = strip_ansi(&tmux_capture(name, 30).await);
        if o.contains("Update ran successfully")
            && at_shell_prompt(&o)
            && pane_has_live_child(name).await != Some(true)
        {
            tracing::warn!(session = %name,
                "codex self-updated and exited on launch — relaunching once");
            emit_event(
                state,
                name,
                "session.codex_update_relaunch",
                Some(json!({"detected_by": "start_session"})),
                None,
                "start_session",
            )
            .await;
            let _ = send_literal(name, &cmd).await;
            sleep_ms(150).await;
            send_key(name, "Enter").await;
            let mut relaunched = false;
            for _ in 0..20 {
                sleep_ms(500).await;
                let o2 = strip_ansi(&tmux_capture(name, 10).await);
                if claude_ui_visible(&o2) {
                    relaunched = true;
                    break;
                }
            }
            if !relaunched {
                // The one relaunch did not take either — say so durably
                // instead of reporting a started lane that is a dead shell.
                tracing::warn!(session = %name,
                    "codex relaunch after self-update did not reach the UI");
                meta.insert("start_error".into(), json!("codex exited after self-update; relaunch did not reach the UI"));
                save_meta(name, &meta);
            }
        }
    }
    // Stream output to the session log (py:24800).
    let _ = std::fs::create_dir_all(logs_dir());
    let lp = log_path(name);
    {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&lp) {
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
            let _ = f.write_all(format!("\n\n=== Session started: {ts} ===\n\n").as_bytes());
        }
    }
    let ptq = pt(name);
    let pipe_cmd = log_pipe_command(&lp);
    // NO `-o` here. tmux's `-o` means "only open a pipe if none exists", and
    // its implementation closes the existing pipe and then declines to open a
    // replacement — i.e. on an ALREADY-PIPED pane it is a toggle OFF. Starting
    // a session whose pane was still piped therefore DISABLED its logging,
    // silently and with a success exit code. Measured on tmux 3.6a: arm ->
    // pane_pipe=1, arm again with -o -> pane_pipe=0, again -> 1. Half the live
    // fleet (29 of 60 panes) was sitting unpiped from exactly this. Plain
    // `pipe-pane` is what we want and is idempotent: tmux closes any existing
    // pipe before running the new command, so re-arming is always safe.
    let _ = tmux(&["pipe-pane", "-t", &ptq, &pipe_cmd]).await;
    meta.remove("start_error");
    meta.insert("last_started".into(), json!(now_i64()));
    let count = meta.get("start_count").and_then(|v| v.as_i64()).unwrap_or(0);
    meta.insert("start_count".into(), json!(count + 1));
    let pending_reload = meta.remove("pending_log_reload").is_some();
    let pending_reason = meta
        .remove("pending_log_reload_reason")
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    save_meta(name, &meta);
    if pending_reload && log_path(name).exists() {
        let prompt = log_reload_prompt(name, &pending_reason);
        let st2 = state.clone();
        let n = name.to_string();
        tokio::spawn(async move { send_after_ready(st2, n, prompt, 60).await });
    }
    // Standing instruction re-send (py:24833). Board digest briefing: gap.
    let instr = meta_str(&load_meta(name), "instructions").trim().to_string();
    if !instr.is_empty() {
        let st2 = state.clone();
        let n = name.to_string();
        tokio::spawn(async move { send_after_ready(st2, n, instr, 60).await });
    }
    emit_event(
        state,
        name,
        "session.started",
        Some(json!({"resumed": !meta_str(&load_meta(name), "cc_conversation_id").is_empty()})),
        None,
        "start_session",
    )
    .await;
    (true, "started".into())
}

#[cfg(unix)]
fn libc_geteuid() -> u32 {
    // std has no geteuid without the libc crate; the UID check only matters
    // for root containers. Read /proc-less macOS via `id -u` once.
    use std::sync::OnceLock;
    static UID: OnceLock<u32> = OnceLock::new();
    *UID.get_or_init(|| {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(1000)
    })
}

fn log_reload_prompt(name: &str, reason: &str) -> String {
    let lp = log_path(name);
    let size = lp.metadata().map(|m| m.len()).unwrap_or(0);
    let size_mb = size as f64 / (1024.0 * 1024.0);
    let cap_mb = MAX_LOG_BYTES / (1024 * 1024);
    let reason_text = if reason.is_empty() { "session swap" } else { reason };
    format!(
        "Before continuing, load the previous amux terminal context.\n\n\
         The log tail captured for this {reason_text} is at:\n{}\n\n\
         Read that file now. It contains up to the last {cap_mb} MB of this \
         session's terminal history ({size_mb:.1} MB currently saved). Use it \
         as continuity context for the work in this session. Do not summarize it \
         back unless asked.",
        lp.display()
    )
}

// ---------------------------------------------------------------------------
// stop_session (py:24943): record the resumable name, /exit gracefully, wait
// for the shell, hard-kill on timeout. tmux stays alive.
// ---------------------------------------------------------------------------

async fn stop_session(name: &str) -> (bool, String) {
    if !valid_session_name(name) {
        return (false, "invalid session name".into());
    }
    // Same exclusion as start_session (see session_op_lock): a stop typing
    // /exit into a pane a concurrent start is booting is exactly the 2026-08-09
    // interleaving incident.
    let op_lock = session_op_lock(name);
    let _op = op_lock.lock().await;
    let cfg = parse_env(name);
    if backend_of_cfg(&cfg) == "herdr" {
        if !herdr_agent_running(name).await {
            return (true, "not running".into());
        }
        let mut meta = load_meta(name);
        if meta_str(&meta, "cc_session_name").is_empty() {
            meta.insert("cc_session_name".into(), json!(name));
            save_meta(name, &meta);
        }
        // Same addressing as `send` — a stop that typed /exit at the retired
        // agent name reached nothing, then reported "stopped" after waiting
        // 15s for the exit it never asked for.
        if let Err(e) = herdr_prompt(name, "/exit").await {
            return (false, format!("herdr /exit failed: {e}"));
        }
        for _ in 0..30 {
            sleep_ms(500).await;
            if !herdr_agent_running(name).await {
                return (true, "stopped".into());
            }
        }
        return (true, "stopped (hard-kill unavailable on rust origin — pane close is a gap)".into());
    }
    let tmux_sess = tmux_name(name);
    if !tmux_sessions_set().await.contains(&tmux_sess) {
        return (true, "not running".into());
    }
    let output = tmux_capture(name, 10).await;
    let mut meta = load_meta(name);
    if at_shell_prompt(&strip_ansi(&output)) {
        if meta_str(&meta, "cc_session_name").is_empty() {
            meta.insert("cc_session_name".into(), json!(name));
            save_meta(name, &meta);
        }
        return (true, "not running".into());
    }
    // Claude-pid/session-name introspection (py:24974 reads the running
    // process's name file) is not ported; the /rename fallback below pins the
    // resumable name to the amux name, which is what the fresh-start path
    // records anyway.
    let _ = send_literal(name, &format!("/rename {name}")).await;
    sleep_ms(150).await;
    send_key(name, "Enter").await;
    sleep_ms(800).await;
    meta.insert("cc_session_name".into(), json!(name));
    save_meta(name, &meta);
    // Detach pipe-pane before shell-visible commands (py:24995).
    let stq = st(name);
    let _ = tmux(&["pipe-pane", "-t", &stq]).await;
    send_key(name, "C-u").await;
    sleep_ms(100).await;
    let _ = send_literal(name, "/exit").await;
    sleep_ms(150).await;
    send_key(name, "Enter").await;
    for _ in 0..30 {
        sleep_ms(500).await;
        let out = tmux_capture(name, 10).await;
        if at_shell_prompt(&strip_ansi(&out)) {
            return (true, "stopped".into());
        }
    }
    // Hard kill: the pane shell's children (py:25028).
    if let Some(out) = tmux(&["list-panes", "-t", &stq, "-F", "#{pane_pid}"]).await {
        if out.status.success() {
            let pid = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string();
            if !pid.is_empty() {
                let _ = run_cmd("pkill", &["-9", "-P", &pid], OP_TIMEOUT).await;
            }
        }
    }
    type_line(name, "stty sane").await;
    sleep_ms(1000).await;
    (true, "stopped (hard-kill)".into())
}

async fn kill_tmux_session(name: &str) {
    let stq = st(name);
    let _ = tmux(&["kill-session", "-t", &stq]).await;
}

/// py:25055 archive_session — scrollback→log, stop, kill tmux, CC_ARCHIVED=1,
/// card cascade.
async fn archive_session(state: &AppState, name: &str) -> (bool, String) {
    let f = env_path(name);
    if !f.exists() {
        return (false, format!("session '{name}' not found"));
    }
    let cfg = parse_env(name);
    if is_running(name).await {
        let raw = if backend_of_cfg(&cfg) == "herdr" {
            herdr_capture(name, 50000).await
        } else {
            let ptq = pt(name);
            match run_cmd("tmux", &["capture-pane", "-t", &ptq, "-p", "-S", "-"], Duration::from_secs(30)).await {
                Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => String::new(),
            }
        };
        if !raw.trim().is_empty() {
            let _ = std::fs::create_dir_all(logs_dir());
            let data = raw.into_bytes();
            let start = data.len().saturating_sub(MAX_LOG_BYTES);
            let _ = std::fs::write(log_path(name), &data[start..]);
        }
        let _ = stop_session(name).await;
    }
    kill_tmux_session(name).await;
    let mut cfg = parse_env(name);
    cfg.set("CC_ARCHIVED", "1");
    if cfg.write(&f).is_err() {
        return (false, "could not write session env".into());
    }
    archive_session_issues(state, name, 1).await;
    (true, "archived".into())
}

/// py:25107 _archive_session_issues — flip the archived bit on the lane's
/// cards, both directions.
async fn archive_session_issues(state: &AppState, name: &str, flag: i64) {
    let name = name.to_string();
    let _ = state
        .store
        .write_async(move |conn| {
            let n = conn
                .execute(
                    "UPDATE issues SET archived=?1, updated=?2 WHERE session=?3 AND deleted IS NULL AND archived!=?1",
                    rusqlite::params![flag, now_i64(), name],
                )
                .unwrap_or(0);
            Ok(crate::db::WriteOutcome {
                applied: n > 0,
                events: if n > 0 {
                    vec![crate::db::PendingEvent {
                        entity_type: amux_core::revision::EntityType::Other("issue".into()),
                        entity_id: name.clone(),
                        mutation: amux_core::revision::MutationKind::Updated,
                        payload: None,
                    }]
                } else {
                    vec![]
                },
            })
        })
        .await;
}

/// py:25137 reset_session — drop the conversation, keep the lane.
async fn reset_session(state: &AppState, name: &str) -> (bool, String) {
    let f = env_path(name);
    if !f.exists() {
        return (false, format!("session '{name}' not found"));
    }
    if is_session_blocked(name) {
        return (false, "session is blocked; remove it from blocked-sessions.txt first".into());
    }
    if is_running(name).await {
        let ptq = pt(name);
        if let Some(o) = run_cmd("tmux", &["capture-pane", "-t", &ptq, "-p", "-S", "-"], Duration::from_secs(30)).await {
            let raw = String::from_utf8_lossy(&o.stdout).into_owned();
            if !raw.trim().is_empty() {
                let data = raw.into_bytes();
                let start = data.len().saturating_sub(MAX_LOG_BYTES);
                let _ = std::fs::write(log_path(name), &data[start..]);
            }
        }
        let _ = stop_session(name).await;
        kill_tmux_session(name).await;
    }
    let mut meta = load_meta(name);
    meta.remove("cc_conversation_id");
    meta.remove("cc_session_name");
    save_meta(name, &meta);
    let (ok, msg) = start_session(state, name, "", false).await;
    if ok {
        (true, "reset — fresh conversation, lane intact".into())
    } else {
        (false, msg)
    }
}

/// py:25184 wake_session — clear CC_ARCHIVED, un-archive cards, start.
async fn wake_session(state: &AppState, name: &str) -> (bool, String) {
    if is_session_blocked(name) {
        return (false, "session is blocked; remove it from blocked-sessions.txt first".into());
    }
    let f = env_path(name);
    if !f.exists() {
        return (false, format!("session '{name}' not found"));
    }
    let mut cfg = parse_env(name);
    cfg.remove("CC_ARCHIVED");
    if cfg.write(&f).is_err() {
        return (false, "could not write session env".into());
    }
    archive_session_issues(state, name, 0).await;
    start_session(state, name, "", false).await
}

/// py:25832 _resize_pane — refuse when a real client is attached.
async fn resize_pane(name: &str, cols: i64, rows: i64) -> (bool, String) {
    // AMUX-2634: refresh the VIEWER LEASE first, before any early return.
    // `resize-window` makes tmux pin `window-size manual`, so this call used to
    // narrow the worker's output permanently; `runtime_jobs::pane_size` now
    // restores the pane once the lease goes stale. It must be refreshed on the
    // "already sized" path too — a reader sitting still at one width is still a
    // reader, and refreshing only on CHANGE would yank the pane back under them.
    crate::runtime_jobs::pane_size::note_resize(&tmux_name(name));
    // Never shrink below spawn width: a narrow peek must not degrade the worker.
    // The SPA has overflow-x:auto so the viewer scrolls instead.
    let spawn_cols = crate::runtime_jobs::pane_size::configured_cols() as i64;
    let cols = cols.clamp(spawn_cols, 300);
    let rows = rows.clamp(20, 100);
    let stq = st(name);
    if let Some(o) = tmux(&["list-clients", "-t", &stq, "-F", "#{client_name}"]).await {
        if o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty() {
            return (false, "terminal client attached — its size wins".into());
        }
    }
    if let Some(o) = tmux(&["display-message", "-p", "-t", &stq, "#{window_width}x#{window_height}"]).await {
        if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == format!("{cols}x{rows}") {
            return (true, "already sized".into());
        }
    }
    let cs = cols.to_string();
    let rs = rows.to_string();
    let _ = tmux(&["resize-window", "-t", &stq, "-x", &cs, "-y", &rs]).await;
    (true, format!("resized to {cols}x{rows}"))
}

// ---------------------------------------------------------------------------
// Agent panel navigation (py:25861-25959) — every key gated on a fresh
// capture; the Background dialog is cancelled, never confirmed.
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Saved-log helpers (py:5175-5478).
// ---------------------------------------------------------------------------

fn load_session_log(name: &str, tail_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let lp = log_path(name);
    let Ok(mut f) = std::fs::File::open(&lp) else { return String::new() };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if tail_bytes > 0 && size > tail_bytes && f.seek(SeekFrom::Start(size - tail_bytes)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// py:5175 save_session_log (throttle omitted — the rust origin saves on the
/// peek path only when it actually captured something new; the Python
/// throttle exists to protect a 10MB rewrite loop under polling, which the
/// append path below avoids equally well by being append-only until cap).
fn save_session_log(name: &str, content: &str) {
    if content.trim().is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(logs_dir());
    let lp = log_path(name);
    let data = content.as_bytes();
    // APPEND-ONLY. This used to read the whole file, concatenate, and rewrite
    // the last MAX_LOG_BYTES whenever the cap was crossed — two defects at
    // once. First, the pipe-pane writer holds an O_APPEND fd on this same
    // file, so a read-modify-write races it and silently drops whatever the
    // writer appended between the read and the write. Second, it made size
    // discipline a policy owned by TWO components that disagreed: this one
    // trimmed to 10MB, the writer rolls at AMUX_LOG_MAX_MB. Five logs on the
    // live fleet sat at exactly 10,485,760 bytes from this path.
    //
    // Rotation now has exactly one owner — the writer, which holds the fd. A
    // session with no writer can drift over the cap until its next start arms
    // one; that is bounded and comprehensible, which two racing trimmers were
    // not.
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&lp) {
        let _ = f.write_all(data);
    }
}

/// py:5464 _write_plain_log — ANSI-stripped mirror for the session to Read.
fn write_plain_log(name: &str) -> Option<(PathBuf, usize)> {
    let lp = log_path(name);
    if !lp.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&lp)
        .unwrap_or_else(|_| String::from_utf8_lossy(&std::fs::read(&lp).unwrap_or_default()).into_owned());
    let clean = collapse_blank_runs(&strip_ansi(&text));
    let cp = plain_log_path(name);
    std::fs::create_dir_all(cp.parent()?).ok()?;
    std::fs::write(&cp, clean.as_bytes()).ok()?;
    Some((cp, clean.len()))
}

/// py:22616 _capture_log_tail_for_reload — persist the last MAX_LOG_BYTES of
/// output before a provider/model/effort/yolo swap.
async fn capture_log_tail_for_reload(name: &str, reason: &str) -> bool {
    if !valid_session_name(name) {
        return false;
    }
    let _ = std::fs::create_dir_all(logs_dir());
    let lp = log_path(name);
    let mut chunks: Vec<u8> = Vec::new();
    let existing = load_session_log(name, MAX_LOG_BYTES as u64);
    chunks.extend_from_slice(existing.as_bytes());
    let mut captured = String::new();
    let mut was_piped = false;
    if is_running(name).await {
        let ptq = pt(name);
        // Detaching the pipe is deliberate: the whole-file rewrite below would
        // otherwise race the writer's appends. But it has to be put BACK —
        // this used to detach and return, so any provider/model/effort/yolo
        // swap left the session permanently unlogged with nothing reporting it.
        was_piped = pane_is_piped(name).await;
        let _ = tmux(&["pipe-pane", "-t", &ptq]).await;
        if let Some(o) = run_cmd("tmux", &["capture-pane", "-t", &ptq, "-p", "-S", "-"], Duration::from_secs(30)).await {
            captured = String::from_utf8_lossy(&o.stdout).into_owned();
        }
    }
    if !captured.trim().is_empty() {
        let safe_reason = reason.replace('\n', " ").trim().to_string();
        let safe_reason = if safe_reason.is_empty() { "session swap".to_string() } else { safe_reason };
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
        let marker = format!("\n\n=== Captured before {safe_reason}: {ts} ===\n\n");
        let cap_text = if tmux_alt_screen(name).await {
            collapse_blank_runs(&captured)
        } else {
            captured
        };
        chunks.extend_from_slice(marker.as_bytes());
        chunks.extend_from_slice(cap_text.as_bytes());
    }
    if chunks.is_empty() {
        if was_piped {
            rearm_log_pipe(name).await;
        }
        return false;
    }
    let start = chunks.len().saturating_sub(MAX_LOG_BYTES);
    let ok = std::fs::write(&lp, &chunks[start..]).is_ok();
    if was_piped {
        rearm_log_pipe(name).await;
    }
    ok
}

/// Is this pane currently piped? `#{pane_pipe}` is tmux's own answer, and it
/// is the field the stale-log verdict in `/api/debug/logs` keys off.
async fn pane_is_piped(name: &str) -> bool {
    let ptq = pt(name);
    match tmux(&["list-panes", "-t", &ptq, "-F", "#{pane_pipe}"]).await {
        Some(o) => String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim() == "1"),
        None => false,
    }
}

/// Re-attach the log pipe. Same construction as `start_session` and, like it,
/// without `-o` — see the comment there for why `-o` silently disables the
/// pipe it is supposed to guard.
async fn rearm_log_pipe(name: &str) {
    let _ = std::fs::create_dir_all(logs_dir());
    let lp = log_path(name);
    let ptq = pt(name);
    let pipe_cmd = log_pipe_command(&lp);
    let _ = tmux(&["pipe-pane", "-t", &ptq, &pipe_cmd]).await;
}

/// Previous generation produced by the writer's rotation.
fn rotated_log_path(name: &str) -> PathBuf {
    logs_dir().join(format!("{name}.log.1"))
}

/// The signals `/api/debug/logs` correlates for ONE session.
pub(crate) struct LogSignals {
    pub running: bool,
    pub piped: bool,
    /// Seconds since the log file was last written. `None` = no log file.
    pub log_age_s: Option<i64>,
    /// Seconds since the pane last painted. `None` = never.
    pub pane_activity_age_s: Option<i64>,
    /// Seconds since the pipe-pane writer was armed. 0 = no writer known.
    pub writer_age_s: i64,
    pub stale_s: i64,
}

/// The verdict, as a pure function of the signals (AF-96).
///
/// This logic used to live inline in the handler, which meant the alarm it
/// exists to raise could not be shown to FIRE. AMUX-2628's own entry claimed it
/// had been "extracted as a pure function and unit-tested against this
/// incident's own numbers so the alarm is demonstrably able to fire" — it had
/// not, and that gap is the precise half ethos rule 7 protects: an instrument
/// nobody can test is one nobody can trust to be more than decoration.
///
/// `stale` is the discriminating verdict: piping is on, and the pane painted
/// MORE RECENTLY than the log was written. Everything else here exists to keep
/// that cell honest — `recovering` and `idle` are the two states that otherwise
/// masquerade as it.
pub(crate) fn log_verdict(s: &LogSignals) -> (&'static str, String) {
    if !s.running {
        return ("not-running", "no tmux pane for this session".to_string());
    }
    if !s.piped {
        return ("unpiped", "pane is running but pipe-pane is OFF — nothing is being logged".to_string());
    }
    match (s.log_age_s, s.pane_activity_age_s) {
        (Some(la), Some(aa)) if la > s.stale_s && aa < la => {
            // Was a writer even present when that output happened? If the writer
            // is YOUNGER than the gap, the missing output predates it and the
            // pipe is merely waiting for the pane to speak again — a different,
            // non-actionable state.
            if s.writer_age_s > 0 && s.writer_age_s < aa {
                (
                    "recovering",
                    format!(
                        "pipe re-armed {}s ago; the {la}s-old log predates it and the pane has not written since",
                        s.writer_age_s
                    ),
                )
            } else {
                (
                    "stale",
                    format!("piping on but no write in {la}s while the pane was active {aa}s ago"),
                )
            }
        }
        (None, _) => ("stale", "piping on but no log file exists".to_string()),
        (Some(la), _) => {
            if la > s.stale_s {
                ("idle", format!("no write in {la}s, but the pane is idle too"))
            } else {
                ("ok", format!("last write {la}s ago"))
            }
        }
    }
}

/// GET /api/debug/logs — per-session logging health.
///
/// This endpoint exists because of the AMUX-2628 failure MODE, not merely the
/// bug: every log on the fleet stopped growing and NOTHING said so for over an
/// hour. Each individual signal looked healthy — `pane_pipe` was 1, the writer
/// process was alive holding the right inode, the file existed — and the only
/// way to see the fault was to correlate three facts that no single view put
/// side by side. So this reports the correlation, and computes the VERDICT
/// rather than leaving it to whoever is reading (ethos rule 4: the instrument
/// has to be able to express the discriminator).
///
/// The discriminating verdict is `stale`: piping is on and the pane has been
/// active more recently than the log was written. That is the exact shape of
/// this incident and it is what a sweep can key on tomorrow.
pub async fn debug_logs(RawQuery(q): RawQuery) -> Response {
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let stale_s: u64 = qs_first(&qs, "stale_s", "120").parse().unwrap_or(120);
    let now = now_i64();

    // One tmux call for the whole fleet: name, pipe state, and when the
    // session last produced activity. Correlating per session would be 3N
    // spawns and would also sample the three fields at different instants.
    let mut panes: BTreeMap<String, (bool, i64)> = BTreeMap::new();
    let mut unmanaged = 0usize;
    if let Some(o) = tmux(&[
        "list-panes",
        "-a",
        "-F",
        // window_activity, NOT session_activity. Measured on tmux 3.6a: a
        // pane writing output moves #{window_activity} and leaves
        // #{session_activity} untouched (it tracks session-level use). Keying
        // the stale verdict on session_activity made every busy pane look
        // dormant — the verdict would have answered, plausibly, and wrongly.
        "#{session_name}\t#{pane_pipe}\t#{window_activity}",
    ])
    .await
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let mut it = line.split('\t');
            let (Some(sess), Some(pipe), Some(act)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let Some(name) = sess.strip_prefix("amux-") else { continue };
            // Only sessions amux MANAGES. A tmux session with no `<name>.env`
            // was not created by the session verbs, so amux never armed a pipe
            // for it and "unpiped" is not a fault. Counting them made
            // `healthy` permanently false on this machine (12 leftover
            // zz-*/smprobe-* panes), and a verdict that can never be green is
            // one nobody reads — the same defect as a threshold below the
            // baseline. The skipped count is reported rather than dropped, so
            // the exemption cannot hide a real session that lost its env file.
            if !env_path(name).exists() {
                unmanaged += 1;
                continue;
            }
            let e = panes.entry(name.to_string()).or_insert((false, 0));
            e.0 |= pipe.trim() == "1";
            e.1 = e.1.max(act.trim().parse::<i64>().unwrap_or(0));
        }
    }

    // Writer liveness, one `ps` for the fleet. "pipe on but the writer died"
    // and "pipe on, writer alive, nothing arriving" are different bugs and
    // were indistinguishable during the incident.
    let mut writers: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    // -ww: without it macOS ps truncates the command line, and the writer's
    // argv is ~3.3KB of embedded program with the log path at the very END —
    // i.e. exactly the part that gets cut, so every writer would read as absent.
    if let Some(o) = run_cmd("ps", &["-axww", "-o", "pid=,lstart=,command="], OP_TIMEOUT).await {
        let dir = logs_dir().to_string_lossy().into_owned();
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let Some(idx) = line.find(&dir) else { continue };
            // The writer's own signature, not "python3": the CHILD process is
            // spelled `.../MacOS/Python -c ...` on macOS, so a python3 test
            // silently matched nothing and reported every writer dead.
            if !line.contains("sys.argv") {
                continue;
            }
            let tail = &line[idx + dir.len()..];
            let file = tail.trim_start_matches('/').split_whitespace().next().unwrap_or("");
            let Some(base) = file.strip_suffix(".log") else { continue };
            let mut head = line.split_whitespace();
            let Some(Ok(pid)) = head.next().map(str::parse::<i64>) else { continue };
            // lstart is a fixed 5-field ctime string ("Sun Aug  9 21:34:13
            // 2026"). How long the WRITER has existed is what separates "this
            // pipe is broken now" from "this pipe was just re-armed and the
            // pane has not spoken since" — without it the surface stays red
            // after a correct fix, which is how a verdict stops being read.
            let stamp: Vec<&str> = head.clone().take(5).collect();
            let started = if stamp.len() == 5 {
                {
                    use chrono::TimeZone as _;
                    chrono::NaiveDateTime::parse_from_str(&stamp.join(" "), "%a %b %e %H:%M:%S %Y")
                        .ok()
                        .and_then(|d| chrono::Local.from_local_datetime(&d).single())
                        .map(|d: chrono::DateTime<chrono::Local>| d.timestamp())
                        .unwrap_or(0)
                }
            } else {
                0
            };
            writers.entry(base.to_string()).or_insert((pid, started));
        }
    }

    let mut names: Vec<String> = panes.keys().cloned().collect();
    if let Ok(rd) = std::fs::read_dir(logs_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("log") {
                if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                    if panes.contains_key(s) {
                        continue;
                    }
                    if env_path(s).exists() {
                        names.push(s.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();

    let mut rows = Vec::new();
    let (mut ok, mut stale, mut unpiped, mut idle, mut recovering) = (0, 0, 0, 0, 0);
    for name in &names {
        let (piped, activity) = panes.get(name).copied().unwrap_or((false, 0));
        let running = panes.contains_key(name);
        let lp = log_path(name);
        let md = lp.metadata().ok();
        let size = md.as_ref().map(|m| m.len());
        let log_age = md
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| now - d.as_secs() as i64);
        let act_age = if activity > 0 { Some(now - activity) } else { None };

        // The verdict is `log_verdict`'s to make, not this handler's (AF-96) —
        // the counters below are the only thing that stays here, because they
        // are a property of the sweep rather than of one session.
        let (verdict, detail) = log_verdict(&LogSignals {
            running,
            piped,
            log_age_s: log_age,
            pane_activity_age_s: act_age,
            writer_age_s: writers.get(name).map(|(_, st)| now - st).unwrap_or(0),
            stale_s: stale_s as i64,
        });
        match verdict {
            "unpiped" => unpiped += 1,
            "recovering" => recovering += 1,
            "stale" => stale += 1,
            "idle" => idle += 1,
            "ok" => ok += 1,
            _ => {}
        }
        rows.push(json!({
            "name": name,
            "running": running,
            "pipe": piped,
            "writer_pid": writers.get(name).map(|(p, _)| *p),
            "writer_age_s": writers.get(name).map(|(_, st)| now - st),
            "log_bytes": size,
            "log_age_s": log_age,
            "pane_activity_age_s": act_age,
            "rotated_bytes": rotated_log_path(name).metadata().map(|m| m.len()).ok(),
            "verdict": verdict,
            "detail": detail,
        }));
    }

    j200(json!({
        "checked_at": now,
        "stale_s": stale_s,
        // Panes named amux-* that amux does not manage (no session env file).
        // Named, not silently dropped.
        "unmanaged_panes_skipped": unmanaged,
        "config": {
            "raw_capture": log_raw_capture(),
            "rotate_max_bytes": log_rotate_bytes(),
            "flush_ms": log_flush_ms(),
        },
        "counts": {
            "total": rows.len(), "ok": ok, "stale": stale,
            "unpiped": unpiped, "idle": idle, "recovering": recovering,
            "not_running": rows.len() - ok - stale - unpiped - idle - recovering,
        },
        // The whole point of the endpoint: a sweep reads this one boolean.
        "healthy": stale == 0 && unpiped == 0,
        "sessions": rows,
    }))
}

fn mark_pending_log_reload(name: &str, reason: &str) {
    update_meta(
        name,
        &[("pending_log_reload", json!(now_i64())), ("pending_log_reload_reason", json!(reason))],
    );
}

// ---------------------------------------------------------------------------
// get_claude_stats (py:9619), cc tasks (py:5505), git info (py:21236).
// ---------------------------------------------------------------------------

fn get_claude_stats(work_dir: &str) -> Value {
    if work_dir.is_empty() {
        return json!({"tokens": 0, "last_active": ""});
    }
    let project_dir = claude_home().join("projects").join(project_name(work_dir));
    let Ok(rd) = std::fs::read_dir(&project_dir) else {
        return json!({"tokens": 0, "last_active": ""});
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|p| p.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, p)))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.0));
    let Some((_, newest)) = files.first() else {
        return json!({"tokens": 0, "last_active": ""});
    };
    let mut total_in: i64 = 0;
    let mut total_out: i64 = 0;
    let mut last_ts = String::new();
    for entry in iter_jsonl_tail(newest, 5_000_000) {
        if let Some(ts) = entry["timestamp"].as_str() {
            if !ts.is_empty() {
                last_ts = ts.to_string();
            }
        }
        let usage = &entry["message"]["usage"];
        if usage.is_object() {
            total_in += usage["input_tokens"].as_i64().unwrap_or(0);
            total_in += usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
            total_out += usage["output_tokens"].as_i64().unwrap_or(0);
        }
    }
    json!({"tokens": total_in + total_out, "last_active": last_ts})
}

fn plan_stale_hide_secs() -> f64 {
    std::env::var("AMUX_PLAN_STALE_HIDE_HOURS").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(24.0) * 3600.0
}

/// py:5505 _session_cc_tasks — Claude Code's native task list, read-only.
async fn session_cc_tasks(name: &str) -> Value {
    let empty = json!({"tasks": [], "counts": {}, "active": Value::Null, "total": 0});
    let owner = meta_str(&load_meta(name), "cc_session_name");
    if !owner.is_empty() && owner != name {
        return json!({"tasks": [], "counts": {}, "active": Value::Null, "total": 0,
                      "_suppressed": format!("cross-linked to {owner}")});
    }
    let Some(p) = session_jsonl_path(name) else { return empty };
    let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { return empty };
    let tdir = claude_home().join("tasks").join(stem);
    if !tdir.is_dir() {
        return empty;
    }
    // Fresh-splash guard (py:5540): a brand-new conversation with no turns
    // must not surface the dead conversation's plan.
    let raw = tmux_capture(name, 40).await;
    if !raw.is_empty() {
        let clean = strip_ansi(&raw);
        if let Some(i) = clean.rfind("Claude Code v") {
            let after = &clean[i..];
            if !after.contains('\u{23fa}') && !after.contains('\u{25cf}') {
                return empty;
            }
        }
    }
    let mut tasks: Vec<(f64, Value)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&tdir) {
        for e in rd.flatten() {
            let path = e.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !fname.ends_with(".json") || !fname.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(d) = serde_json::from_str::<Value>(&text) else { continue };
            if !d.is_object() {
                continue;
            }
            let mtime = path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let stem = fname.trim_end_matches(".json");
            let id = d["id"].as_str().map(String::from).unwrap_or_else(|| {
                if d["id"].is_number() { d["id"].to_string() } else { stem.to_string() }
            });
            tasks.push((
                mtime,
                json!({
                    "id": id,
                    "subject": d["subject"].as_str().or(d["activeForm"].as_str()).unwrap_or("").trim(),
                    "activeForm": d["activeForm"].as_str().unwrap_or("").trim(),
                    "status": if d["status"].is_string() { d["status"].clone() } else { json!("pending") },
                    "blockedBy": d["blockedBy"].as_array().map(|a| a.iter().map(|x| json!(x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))).collect::<Vec<_>>()).unwrap_or_default(),
                }),
            ));
        }
    }
    tasks.sort_by_key(|(_, t)| {
        t["id"].as_str().and_then(|s| s.parse::<i64>().ok()).unwrap_or(1_000_000)
    });
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for (_, t) in &tasks {
        *counts.entry(t["status"].as_str().unwrap_or("pending").to_string()).or_insert(0) += 1;
    }
    let active = tasks
        .iter()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, t)| t.clone())
        .unwrap_or(Value::Null);
    let updated_at = tasks.iter().map(|(m, _)| *m).fold(0.0_f64, f64::max) as i64;
    if updated_at > 0 && now_f64() - updated_at as f64 > plan_stale_hide_secs() {
        return empty;
    }
    json!({
        "tasks": tasks.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>(),
        "counts": counts,
        "active": active,
        "total": tasks.len(),
        "updated_at": updated_at,
    })
}

async fn git_out(wd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut full = vec!["-C", wd];
    full.extend_from_slice(args);
    let out = run_cmd("git", &full, timeout).await?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

/// py:21236 _git_info (cache omitted — one caller per request here; the
/// Python cache defends a 60-session polling loop this origin doesn't run).
async fn git_info(work_dir: &str, detail: bool) -> Value {
    if work_dir.is_empty() {
        return json!({"branch": "", "repo": ""});
    }
    let branch = git_out(work_dir, &["branch", "--show-current"], Duration::from_secs(2))
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let repo = if branch.is_empty() {
        String::new()
    } else {
        git_out(work_dir, &["rev-parse", "--show-toplevel"], Duration::from_secs(2))
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let mut result = json!({"branch": branch, "repo": repo});
    if !detail || branch.is_empty() {
        return result;
    }
    let mut ahead_base = String::new();
    let mut ahead: Vec<String> = Vec::new();
    for base in ["main", "master", "dev", "develop"] {
        if let Some(out) = git_out(work_dir, &["log", &format!("{base}..HEAD"), "--oneline", "--no-decorate"], OP_TIMEOUT).await {
            ahead_base = base.to_string();
            ahead = out.trim().lines().filter(|l| !l.is_empty()).map(String::from).collect();
            break;
        }
    }
    result["ahead_base"] = json!(ahead_base);
    result["ahead"] = json!(ahead);
    let status: Vec<String> = git_out(work_dir, &["status", "--short"], OP_TIMEOUT)
        .await
        .map(|o| o.trim().lines().filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    result["dirty"] = json!(!status.is_empty());
    result["status"] = json!(status);
    fn parse_numstat(out: &str) -> Vec<Value> {
        out.trim()
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.len() == 3 {
                    Some(json!({
                        "file": parts[2],
                        "added": parts[0].parse::<i64>().unwrap_or(0),
                        "deleted": parts[1].parse::<i64>().unwrap_or(0),
                    }))
                } else {
                    None
                }
            })
            .collect()
    }
    result["files_unstaged"] = json!(
        git_out(work_dir, &["diff", "--numstat"], OP_TIMEOUT).await.map(|o| parse_numstat(&o)).unwrap_or_default()
    );
    result["files_staged"] = json!(
        git_out(work_dir, &["diff", "--cached", "--numstat"], OP_TIMEOUT)
            .await
            .map(|o| parse_numstat(&o))
            .unwrap_or_default()
    );
    if !ahead_base.is_empty() && !ahead.is_empty() {
        result["files_committed"] = json!(
            git_out(work_dir, &["diff", &format!("{ahead_base}..HEAD"), "--numstat"], OP_TIMEOUT)
                .await
                .map(|o| parse_numstat(&o))
                .unwrap_or_default()
        );
    } else {
        result["files_committed"] = json!([]);
    }
    result["remote_url"] = json!(
        git_out(work_dir, &["remote", "get-url", "origin"], Duration::from_secs(3))
            .await
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    );
    result["unpushed"] = json!(
        git_out(work_dir, &["log", "@{u}..HEAD", "--oneline", "--no-decorate"], OP_TIMEOUT)
            .await
            .map(|o| o.trim().lines().filter(|l| !l.is_empty()).count())
            .unwrap_or(0)
    );
    result
}

/// py:18858/18877 — dirty files scoped to this session's territory.
pub(crate) fn all_session_workdirs() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("env") {
                continue;
            }
            let Some(n) = p.file_stem().and_then(|s| s.to_str()) else { continue };
            let wd = session_work_dir(n);
            if !wd.is_empty() {
                out.insert(n.to_string(), wd);
            }
        }
    }
    out
}

async fn session_dirty_files(name: &str, work_dir: &str) -> Vec<String> {
    let wd = expanduser(work_dir)
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| work_dir.to_string());
    let mut args: Vec<String> = vec!["status".into(), "--porcelain".into(), "--".into(), ".".into()];
    for (other, od) in all_session_workdirs() {
        if other == name {
            continue;
        }
        if od != wd && format!("{od}/").starts_with(&format!("{wd}/")) {
            if let Ok(rel) = Path::new(&od).strip_prefix(&wd) {
                args.push(format!(":(exclude){}", rel.display()));
            }
        }
    }
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    match git_out(&wd, &args_ref, Duration::from_secs(10)).await {
        Some(out) => out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.chars().skip(3).collect::<String>().trim().to_string())
            .collect(),
        None => vec![],
    }
}

// ---------------------------------------------------------------------------
// peek (py:74985) — the full response assembly. Server-side caches are
// omitted (Python's defend a polling dashboard against re-rendering a 120KB
// transcript; the render below is bounded and this origin serves one client
// per request — reintroduce a cache if the SPA's poll cadence lands here).
// ---------------------------------------------------------------------------

/// AF-128 (split from AR-110): the pane's OWN geometry, reported where the reader is.
///
/// AR-110's recorded cost was not the narrow pane — it was a wrong root cause and a
/// shipped CSS change that had to be reverted, because a narrow PANE and a narrow
/// RENDER present identically in peek and nothing in the response could tell them
/// apart. The narrowing itself is fixed (`runtime_jobs::pane_size` undoes tmux's
/// `window-size manual` pin), but a pane can still be narrow for reasons nobody
/// caused: a terminal resize, an 80-column launch — two lanes sit at 80 today
/// against a 220 spawn width. So the ambiguity outlived the fix, which is the half
/// worth closing: fixing a cause removes one instance, fixing the instrument
/// removes the misdiagnosis.
///
/// Deliberately NO threshold and no "this pane looks narrow" verdict. Picking a
/// column count at which to warn is the guess this repo's ethos names as the tell
/// (`ethos.md`: "prefer the structurally-absent signal over the tuned parameter"),
/// and the reader comparing 50 against the 220 they see elsewhere needs no help
/// from a constant. Report the fact; let the consumer judge.
async fn tmux_pane_geometry(name: &str) -> Option<(i64, i64)> {
    if session_backend(name) == "herdr" {
        return None;
    }
    let pt = pt(name);
    let out = tmux(&[
        "display-message", "-t", &pt, "-p", "#{window_width}x#{window_height}",
    ])
    .await?;
    parse_pane_geometry(&String::from_utf8_lossy(&out.stdout))
}

/// The parse half of [`tmux_pane_geometry`], split out so it is testable without
/// tmux. Every rejection below is a shape tmux really produces: an unknown format
/// specifier comes back as the literal string, a dead session gives empty stdout,
/// and `display-message` on a missing target prints an error to stderr leaving
/// stdout blank. Returning None for all of them is why a caller can treat
/// `pane_cols` as absent-or-true rather than absent-or-wrong.
fn parse_pane_geometry(raw: &str) -> Option<(i64, i64)> {
    let (w, h) = raw.trim().split_once('x')?;
    let cols: i64 = w.trim().parse().ok()?;
    let rows: i64 = h.trim().parse().ok()?;
    // A zero or negative dimension is not a pane, it is tmux failing to answer.
    if cols <= 0 || rows <= 0 {
        return None;
    }
    Some((cols, rows))
}

async fn peek_response(name: &str, lines: i64, live_only: bool, no_trim: bool) -> Value {
    let provider = provider_of(&parse_env(name));
    if live_only {
        let output = strip_scroll_pill(&tmux_capture(name, lines).await);
        let live = if output.is_empty() { String::new() } else { strip_launch_noise(output.trim()) };
        // The live=1 trim needs the transcript the CLIENT is displaying; the
        // rust origin re-renders it (bounded) instead of a process cache.
        let live = if !live.is_empty() && !no_trim {
            let tr = render_session_transcript(name, 120_000);
            if tr.is_empty() { live } else { trim_live_overlap(&tr, &live) }
        } else {
            live
        };
        let lv = if live.is_empty() { "(no output)".to_string() } else { collapse_blank_runs(&live) };
        return json!({"name": name, "live_only": true, "live": lv, "output": lv});
    }
    let mut output = strip_scroll_pill(&tmux_capture(name, lines).await);
    if provider == "gemini" {
        output = clean_gemini_frame(&tmux_capture(name, 0).await);
    }
    let tmux_lines = if output.is_empty() { 0 } else { output.lines().count() };
    let is_alt = tmux_alt_screen(name).await;
    if is_alt {
        let (transcript, output) = if provider != "claude" {
            // Non-Claude alt-screen TUIs repaint in place: the LIVE frame is
            // the whole truthful state (py:75040).
            (String::new(), clean_gemini_frame(&tmux_capture(name, 0).await))
        } else {
            (render_session_transcript(name, 120_000), output)
        };
        let mut live = if output.is_empty() { String::new() } else { strip_launch_noise(output.trim()) };
        if !transcript.is_empty() && !live.is_empty() {
            live = trim_live_overlap(&transcript, &live);
        }
        let live_out = if !live.is_empty() {
            collapse_blank_runs(&live)
        } else if transcript.is_empty() {
            "(no output)".to_string()
        } else {
            String::new()
        };
        // AMUX-1807: `output` mirrors the CURRENT terminal frame for API
        // consumers; never empty for a running session.
        let mut out_compat = live_out.clone();
        if out_compat.is_empty() && !output.is_empty() {
            out_compat = collapse_blank_runs(&strip_launch_noise(output.trim()));
        }
        let history = if transcript.is_empty() { String::new() } else { collapse_blank_runs(&transcript) };
        let ol = out_compat.lines().filter(|l| !l.trim().is_empty()).count();
        let hl = history.lines().filter(|l| !l.trim().is_empty()).count();
        let mut resp = json!({
            "name": name,
            "history": history,
            "live": live_out,
            "output": out_compat,
            "output_lines": ol,
            "history_lines": hl,
            // `output` is the CURRENT TERMINAL FRAME — never scrollback. A
            // full-screen prompt clears the screen and `output` collapses to
            // the modal (the 2026-07-27 "swallowed message" diagnosis). State
            // the structural fact instead of guessing at the cause.
            "output_is_viewport_only": true,
        });
        if hl > ol + 20 {
            resp["hint"] = json!(format!(
                "`output` is only the current terminal frame ({ol} line(s)) — a full-screen \
                 prompt can push all of a session's work off-viewport. Read `history` \
                 ({hl} lines) for what it was actually doing."
            ));
        }
        return resp;
    }
    // Normal screen (py:75117).
    if !output.is_empty() && tmux_lines >= 30 {
        save_session_log(name, &output);
        return json!({"name": name, "output": collapse_blank_runs(&strip_launch_noise(&output))});
    }
    let mut saved = load_session_log(name, 65_536);
    if !saved.is_empty() && log_looks_torn(&saved) {
        let clean = render_session_transcript(name, 120_000);
        if !clean.is_empty() {
            saved = clean;
        }
    }
    if !saved.is_empty() {
        let live = if output.is_empty() { String::new() } else { strip_launch_noise(output.trim()) };
        let combined = if !live.is_empty() && !saved.trim_end().ends_with(&live) {
            format!("{}\n\n{}\n", saved.trim_end(), live)
        } else {
            saved
        };
        return json!({"name": name, "output": collapse_blank_runs(&combined), "saved": true});
    }
    let fallback = if output.is_empty() { "(no output)".to_string() } else { output };
    json!({"name": name, "output": collapse_blank_runs(&fallback)})
}

// ---------------------------------------------------------------------------
// Misc verb support: transcripts backup (py:6112), memory sharing (py:21012),
// inherited instruction files (py:21782).
// ---------------------------------------------------------------------------

fn backup_session_jsonl(name: &str, reason: &str) -> Option<String> {
    let wd = session_work_dir(name);
    if wd.is_empty() {
        return None;
    }
    let project_dir = claude_home().join("projects").join(project_name(&wd));
    let Ok(rd) = std::fs::read_dir(&project_dir) else { return None };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|p| p.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, p)))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.0));
    let (_, src) = files.first()?;
    let dest_dir = transcripts_dir().join(name);
    std::fs::create_dir_all(&dest_dir).ok()?;
    let ts = chrono::Local::now().format("%Y%m%dT%H%M%S");
    let dest = dest_dir.join(format!("{ts}_{reason}_{}", src.file_name()?.to_string_lossy()));
    std::fs::copy(src, &dest).ok()?;
    // Prune to the newest 20 (py:6142).
    if let Ok(rd) = std::fs::read_dir(&dest_dir) {
        let mut backups: Vec<(std::time::SystemTime, PathBuf)> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .filter_map(|p| p.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, p)))
            .collect();
        backups.sort_by_key(|e| e.0);
        let excess = backups.len().saturating_sub(20);
        for (_, old) in backups.into_iter().take(excess) {
            let _ = std::fs::remove_file(old);
        }
    }
    Some(dest.to_string_lossy().into_owned())
}

fn list_session_transcripts(name: &str) -> Vec<Value> {
    let dest_dir = transcripts_dir().join(name);
    let Ok(rd) = std::fs::read_dir(&dest_dir) else { return vec![] };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|p| p.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, p)))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.0));
    files
        .into_iter()
        .filter_map(|(_, f)| {
            let md = f.metadata().ok()?;
            let mtime = md.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            Some(json!({
                "name": f.file_name()?.to_string_lossy(),
                "size": md.len(),
                "mtime": mtime,
            }))
        })
        .collect()
}

const MEM_MARKER: &str = "<!-- amux:session-memory -->";
const MEM_TOPIC_FILE: &str = "amux-api.md";

/// The fleet roster every worker gets, regenerated on each write.
///
/// Ethan: "make sure all workers are always aware of each others: name, groups,
/// and descriptions so they can auto discover".
///
/// It goes into MEMORY.md — the file the session already reads by default —
/// rather than behind an endpoint, because a roster you have to know to fetch
/// is not discovery (ethos rule 1: who receives this WITHOUT opting in).
///
/// DERIVED, never hand-maintained: names, groups and descriptions come from the
/// session env files that are already the source of truth for the worker list,
/// so a roster cannot drift from the fleet the way a checked-in list would.
///
/// Excludes archived lanes and the reader itself — "who else is out there" is
/// the question, and 50 rows of which one is you is noise.
fn fleet_roster() -> String {
    let mut rows: Vec<(String, String, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("env"))
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
            .collect();
        names.sort();
        for other in names {
            // NO SELF-EXCLUSION (AMUX-2831). MEMORY.md is keyed on the PROJECT
            // DIRECTORY, not the lane, so a shared checkout has ONE file for
            // every lane in it — 17 lanes share ~/Dev/mixpeek. A roster that
            // omitted "me" was therefore correct for exactly one reader (the
            // lane that wrote last) and wrong for the other sixteen, who read a
            // list including themselves and omitting the writer. Last-writer-wins,
            // which is this card's subject, and my own feature was an instance.
            //
            // Listing everyone is the only shape true for every reader of a
            // shared file. A lane identifies itself by $AMUX_SESSION, which it
            // always has; the file cannot know who is reading it.
            let env = crate::config::parse_env_file(&sessions_dir().join(format!("{other}.env")));
            if env.get("CC_ARCHIVED").map(|v| v == "1").unwrap_or(false) {
                continue;
            }
            // ISOLATED (AMUX-3232): a raw agent is undiscoverable to peers, so it
            // never appears in the fleet roster written into every OTHER worker's
            // shared memory table. Same skip as archived.
            if env_flag_on(env.get("CC_ISOLATED").map(String::as_str)) {
                continue;
            }
            let groups = env
                .get("CC_TAGS")
                .map(|t| {
                    t.split([',', ' '])
                        .map(str::trim)
                        .filter(|x| !x.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let desc = env.get("CC_DESC").cloned().unwrap_or_default();
            rows.push((other, groups, desc));
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n## Fleet — who else is running (auto-generated, do not edit)\n\n         Every live worker is listed, INCLUDING YOU — this file is shared by every lane in \
this directory, so it cannot omit the reader. You are the one whose name matches $AMUX_SESSION.\n\n\
Reach any of them with `amux send <name> --stdin` (origin-stamped).          Peek before interrupting: `curl -sk $AMUX_URL/api/sessions/<name>/peek?lines=200`.\n\n         | worker | groups | description |\n|---|---|---|\n",
    );
    for (n, g, d) in rows.iter().take(120) {
        let d = d.replace('|', "\\|").chars().take(110).collect::<String>();
        out.push_str(&format!("| `{n}` | {} | {} |\n", if g.is_empty() { "—" } else { g }, if d.is_empty() { "—" } else { &d }));
    }
    out.push_str(&format!("\n{} peer worker(s). Same-group peers share memory, env and gates.\n", rows.len()));
    out
}


/// Rewrite EVERY live worker's composed memory, so the fleet roster in each one
/// reflects the fleet as it is now.
///
/// THE ROSTER IS FLEET-WIDE DATA WRITTEN PER-WORKER, and that asymmetry is the
/// bug this closes. `write_claude_memory` was reachable from exactly one place —
/// a PATCH to a worker's OWN memory — so worker A getting a description left
/// workers B..Z holding a roster that predates it. Measured 2026-08-10: all 48
/// live workers had a description and 0 of 224 MEMORY.md files carried a roster
/// at all. Capability that reaches nobody is the ethos-1 failure, and a roster
/// nobody has is indistinguishable from no roster.
///
/// Called when a worker's IDENTITY changes (description, groups, name) — rare,
/// and O(fleet) small file writes when it happens. Deliberately not on a timer:
/// nothing about the roster decays on its own, so a periodic rewrite would be
/// churn with no signal behind it.
pub(crate) fn refresh_fleet_rosters() -> usize {
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else { return 0 };
    let mut n = 0usize;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|x| x.to_str()) else { continue };
        if parse_env(name).get("CC_ARCHIVED") == Some("1") {
            continue;
        }
        let wd = session_work_dir(name);
        if wd.is_empty() {
            continue; // no project dir means no MEMORY.md to compose into
        }
        write_claude_memory(name, &wd);
        n += 1;
    }
    n
}

/// The worker-memory block as it appears in Claude's MEMORY.md, NAMED with the
/// lane that wrote it. Empty when the lane has recorded nothing — a header over
/// nothing is itself a claim.
///
/// AMUX-2831. The content is PER-WORKER (mem_file(name)); the destination is
/// PER-CWD. Measured 2026-08-11: 40 of 113 lanes share a cwd, 18 share
/// ~/Dev/mixpeek and 6 share ~/Dev/amux, all reading ONE MEMORY.md. The harm
/// the card names is not that the file is shared — that is Claude Code's keying
/// and amux cannot change it — but that a lane "READS A PEER'S MEMORY AND
/// CANNOT TELL". An unlabelled block implicitly claims to be the reader's own
/// notes, which is false for every lane except the last writer.
///
/// Same rule fleet_roster follows: content in a shared file must be true for
/// EVERY reader, because the file cannot know who is reading it.
fn compose_worker_block(name: &str, session_content: &str) -> String {
    if session_content.trim().is_empty() {
        return String::new();
    }
    format!(
        "## Worker memory — `{name}`\n\n         <!-- Written by the {name} lane. If you are not {name}, this is a peer's \
         memory: useful context, not your own notes. Your own is at \
         ~/.amux/memory/<your-worker>.md and reaches this file when you write it. -->\n\n{}",
        session_content.trim()
    )
}

fn write_claude_memory(name: &str, work_dir: &str) {
    let pname = project_name(work_dir);
    let session_file = mem_file(name);
    let global_file = memory_dir().join("_global.md");

    let global_content = std::fs::read_to_string(&global_file).unwrap_or_default();
    let session_content = std::fs::read_to_string(&session_file).unwrap_or_default();

    let mut parts = Vec::new();
    if !global_content.trim().is_empty() {
        parts.push(format!(
            "- [amux inter-session API]({MEM_TOPIC_FILE}) — \
             sessions/peek/send, board, notes, CRM, browser, Drive. Read it when you \
             need the call shapes; it is also in ~/.claude/CLAUDE.md."
        ));
    }
    parts.push(MEM_MARKER.to_string());
    let worker_block = compose_worker_block(name, &session_content);
    if !worker_block.is_empty() {
        parts.push(worker_block);
    }
    let composed = parts.join("\n\n") + "\n";

    let claude_mem_dir = claude_home().join("projects").join(&pname).join("memory");
    let claude_mem_file = claude_mem_dir.join("MEMORY.md");

    if std::fs::create_dir_all(&claude_mem_dir).is_err() {
        return;
    }
    if !global_content.trim().is_empty() {
        let _ = std::fs::write(
            claude_mem_dir.join(MEM_TOPIC_FILE),
            global_content.trim().to_owned() + "\n",
        );
    }
    // The roster rides on the SAME write, so it is refreshed whenever the
    // session's memory is — no separate job to fall behind the fleet.
    let composed = composed + &fleet_roster();
    let _ = std::fs::write(&claude_mem_file, &composed);
}

fn memory_shared_with(name: &str) -> Vec<String> {
    let wd = session_work_dir(name);
    if wd.is_empty() {
        return vec![];
    }
    let pname = project_name(&wd);
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        let mut names: Vec<String> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("env"))
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
            .collect();
        names.sort();
        for other in names {
            if other == name {
                continue;
            }
            let owd = session_work_dir(&other);
            if !owd.is_empty() && project_name(&owd) == pname {
                out.push(other);
            }
        }
    }
    out
}

fn mem_inherit_files() -> Vec<String> {
    std::env::var("AMUX_MEMORY_INHERIT_FILES")
        .unwrap_or_else(|_| "CLAUDE.md".into())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn add_inherited(out: &mut Vec<Value>, level: &str, path: &Path) {
    let exists = path.is_file();
    let (bytes, text) = if exists {
        let t = std::fs::read_to_string(path).unwrap_or_default();
        (t.len(), t)
    } else {
        (0, String::new())
    };
    out.push(json!({
        "level": level,
        "kind": "inherited",
        "path": path.to_string_lossy(),
        "exists": exists,
        "bytes": bytes,
        "text": text,
    }));
}

fn inherited_instruction_files(work_dir: &str, names: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    let home_dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()));
    for n in names {
        add_inherited(&mut out, "user", &claude_home().join(n));
    }
    if work_dir.is_empty() {
        return out;
    }
    let wd = expanduser(work_dir).canonicalize().unwrap_or_else(|_| expanduser(work_dir));
    let mut chain = vec![wd.clone()];
    let mut cur = wd;
    loop {
        if cur == home_dir || cur.parent().is_none() || !cur.starts_with(&home_dir) {
            break;
        }
        let Some(parent) = cur.parent() else { break };
        cur = parent.to_path_buf();
        chain.push(cur.clone());
    }
    for d in chain.iter().rev() {
        for n in names {
            add_inherited(&mut out, "project", &d.join(n));
        }
    }
    out
}

fn no_board_re() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)^\s*\[no[_-]?board\]\s*").unwrap())
}

// ---------------------------------------------------------------------------
// HTTP layer. Two routes matching the retired proxy shape; dispatch mirrors
// Python's (method, action, subid) tree so unknown verbs 404/405 the same.
// ---------------------------------------------------------------------------

fn jresp(status: StatusCode, v: Value) -> Response {
    (status, Json(v)).into_response()
}
fn j200(v: Value) -> Response {
    jresp(StatusCode::OK, v)
}
fn not_found() -> Response {
    jresp(StatusCode::NOT_FOUND, json!({"error": "not found"}))
}

/// py:801 _UI_TOKEN — sha256("amux-ui-guard:" + AUTH_TOKEN)[:40].
fn ui_token(state: &AppState) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(format!("amux-ui-guard:{}", state.auth_token.clone().unwrap_or_default()).as_bytes());
    hex::encode(h.finalize()).chars().take(40).collect()
}

/// py:804 _session_destructive_allowed.
fn session_destructive_allowed(state: &AppState, headers: &HeaderMap) -> bool {
    if matches!(
        std::env::var("AMUX_ALLOW_AGENT_SESSION_DELETE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        return true;
    }
    headers
        .get("x-amux-ui-token")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == ui_token(state))
        .unwrap_or(false)
}

/// Origin header, py:15208 _hdr_worker precedence (X-Amux-Worker first,
/// legacy X-Amux-Session).
fn hdr_worker(headers: &HeaderMap) -> String {
    for k in ["x-amux-worker", "x-amux-session"] {
        if let Some(v) = headers.get(k).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Steering DELIVERY loop (py:8959 _steer_deliver_tick + py:8811
// _steer_try_deliver). AMUX-2617.
//
// The producer shipped without its consumer. `steer_enqueue` is wired at three
// call sites and writes durable `steering_queue` rows; nothing ever took a row
// OUT except an explicit user cancel or a session delete, and `send_text_inner`
// carries a `from_steering` parameter that NOTHING passed `true`. So a message
// queued to a busy lane was stored perfectly and never delivered: amux-rust sat
// IDLE with 9 QUEUED, the oldest 2h6m old, all of them Ethan's.
//
// This is the same shape as the pickup outage (AMUX-2616) — the cutover carried
// the API surface across and left the background loops behind. Both present as
// "idle with work waiting", which is the state the fleet should never be in.
//
// STATE COMES FROM THE HARNESS FIRST, scrape only as fallback. Python's tick was
// hardwired to the Claude pane detector, which is why a Gemini lane sat 2h at
// IDLE with 1 QUEUED (its comment says so). Since the fleet now runs herdr and
// opencode as well as tmux+claude, keying delivery on a pane regex would rebuild
// that bug for every new backend. `session_status` already resolves the D1
// reported state and falls back to the scrape for hookless lanes, and
// `send_text_inner` already dispatches herdr via `herdr_send` — so delivery is
// backend-agnostic by construction rather than by a per-backend branch here.
// pub so lib.rs registers this loop's REAL cadence with runtime_jobs::registry
// rather than a copy of it — a displayed interval that can disagree with the
// sleep is how a healthy job reads as stalled (or worse, the reverse).
pub const STEER_TICK_SECS: u64 = 5;

/// Is this lane at a turn boundary — i.e. may a queued message be delivered NOW?
///
/// Report-first, scrape as fallback. The harness KNOWS its turn boundaries and
/// posts them (D1); the pane regex only infers them, and infers nothing at all
/// for a non-Claude frame — which is exactly how a Gemini lane sat 2h at IDLE
/// with 1 QUEUED under Python. So a lane that reports its own state is believed,
/// and only a lane with no report falls back to the pane.
///
/// Fails CLOSED: anything not positively known to be idle returns false. A
/// message that waits one more 5s tick costs nothing; a message delivered
/// mid-turn is the bug this whole path exists to prevent.
/// How a queued message may be delivered right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SteerDelivery {
    /// The lane is at a turn boundary: deliver normally.
    AtBoundary,
    /// The lane is mid-turn but the message is OVERDUE — deliver into the
    /// running turn and let Claude Code fold it in at its own boundary.
    OverdueMidTurn,
    /// Not now.
    Hold,
}

/// WHY A LANE CANNOT RECEIVE AT ALL — as distinct from being merely BUSY.
///
/// AMUX-2785. To a sender these two look identical (silence) and they are
/// nothing alike. "Busy" resolves on its own: the lane reaches a boundary, or
/// the `AMUX_STEER_MAX_AGE_S` deadline forces the message in mid-turn. "Not
/// running" and "no env file" resolve NEVER — no deadline can rescue a lane
/// that cannot receive, which is precisely what AMUX-2642 did not anticipate
/// when it added the deadline. Three lanes sat 4-15h in that state
/// (`amux-agent` no-env-file, `amux-rust-execution` not-running,
/// `mixpeek-orchestrator` not-running) while every sender believed the message
/// had landed, because the send response says "queued for next turn boundary"
/// for both cases.
///
/// Pure, so the regression corpus is those three real lanes rather than a
/// convenient fixture; [`lane_block_reason`] is the I/O wrapper.
pub(crate) fn lane_block_reason_from(
    env_exists: bool,
    archived: bool,
    running: bool,
) -> Option<&'static str> {
    if !env_exists {
        return Some("no-env-file");
    }
    if archived {
        return Some("archived");
    }
    if !running {
        return Some("not-running");
    }
    None
}

/// [`lane_block_reason_from`] against the real filesystem and tmux.
///
/// ONE predicate, shared by the drain loop, the point of send, the queue
/// listing and the debug endpoint. It exists because those disagreed: the drain
/// loop asked the question inline and the SEND PATH DID NOT ASK IT AT ALL, so
/// the response promised delivery the loop would never make. That is the
/// ethos-1 view/predicate split — a view must share the predicate of the
/// mechanism it claims to describe, and a send response is a view of the
/// delivery loop.
pub(crate) async fn lane_block_reason(name: &str) -> Option<&'static str> {
    let env_exists = env_path(name).exists();
    let archived = env_exists && parse_env(name).get("CC_ARCHIVED") == Some("1");
    // Don't pay a tmux query for a lane already known unreachable.
    let running = env_exists && !archived && is_running(name).await;
    lane_block_reason_from(env_exists, archived, running)
}

/// The sentence a sender gets. It states what will HAPPEN, not merely what is
/// wrong: "queued" on its own is the string that manufactured the false belief,
/// and replacing it with a bare reason code would leave the sender to guess
/// whether waiting helps. For these reasons it does not.
pub(crate) fn block_reason_explain(reason: &str, name: &str) -> String {
    match reason {
        "no-env-file" => format!(
            "NOT DELIVERABLE — '{name}' has no session env file, so it is not a registered worker. \
             The message is stored, but it is not waiting for a turn boundary and no deadline will \
             force it through. Check the name, or create the worker."
        ),
        "archived" => format!(
            "NOT DELIVERABLE — '{name}' is archived. The message is stored, but nothing wakes an \
             archived lane; un-archiving it is a human's call."
        ),
        "not-running" => format!(
            "NOT DELIVERABLE — '{name}' is not running. The message is stored, but the delivery loop \
             skips stopped lanes, so it waits for the lane to be STARTED, not for it to be free. \
             No deadline will force it through."
        ),
        other => format!("NOT DELIVERABLE — '{name}': {other}."),
    }
}

/// How long a queued message may wait for an idle boundary before it is
/// delivered mid-turn anyway. `AMUX_STEER_MAX_AGE_S`, default 10 minutes.
pub(crate) fn steer_max_age_s() -> f64 {
    std::env::var("AMUX_STEER_MAX_AGE_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(600.0)
}

/// Grace before a permanently-unroutable steering row is dead-lettered
/// (AMUX-3110). One hour, not the 600s delivery deadline: the deadline is
/// about a LIVE lane being slow to reach a turn boundary, this is about a
/// TARGET that does not exist — the grace only covers the recreate/rename
/// race, after which no amount of waiting changes the verdict.
pub(crate) fn steer_dead_letter_s() -> f64 {
    std::env::var("AMUX_STEER_DEAD_LETTER_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v >= 60.0)
        .unwrap_or(3600.0)
}

/// The dead-letter decision, pure so the matrix is testable without a fleet
/// (AMUX-3110). Some(reason) = move the row to steering_history as
/// dead:<reason>; None = leave it queued.
///
/// Only `no-env-file` and `archived` qualify — the class actually measured
/// leaking (11 of 11 unroutable rows, oldest 10.3 days). `not-running` never
/// dead-letters on age: the 2026-08-19 panic held 41 REGISTERED lanes down
/// for 6.5h and every queued row delivered on restart, and no age threshold
/// can tell that outage from a dead lane.
pub(crate) fn steer_dead_letter_verdict(reason: &str, age_s: f64) -> Option<&'static str> {
    match reason {
        "no-env-file" if age_s > steer_dead_letter_s() => Some("no-env-file"),
        "archived" if age_s > steer_dead_letter_s() => Some("archived"),
        _ => None,
    }
}

/// THE DELIVERY DECISION (AMUX-2642). Pure, so the case that matters — a lane
/// that is NEVER idle — is testable without a fleet.
///
/// Waiting for an idle boundary is right, and it is also how a continuously
/// busy lane starves forever: the `amux` session held five messages from 22:06
/// to 22:28 with a 6-second-old `active` self-report, correctly working the
/// whole time, while the sender watched nothing happen and concluded the lane
/// was hung. amux-rust did the same with ten.
///
/// Neither extreme is right, and the reason is recorded in this repo: shipping
/// "always send now" produced the owner's complaint that started the boundary
/// gate ("i sent as a queue but it looks like it was sent directly even though
/// this worker was still working"), and "always wait for idle" is the
/// starvation above. So: boundary first, and a deadline. Past the deadline the
/// message goes into the running turn, where CLAUDE CODE queues it and folds it
/// in at its own turn boundary — real queue semantics implemented by the agent
/// instead of by amux waiting indefinitely. A message the owner sent twenty
/// minutes ago that has never been seen is strictly worse than one that arrives
/// a turn early.
pub(crate) fn steer_decide(reported: Option<&str>, pane_idle: Option<bool>, age_s: f64, max_age_s: f64) -> SteerDelivery {
    let idle = match reported {
        // The lane's own report wins (D1): the harness knows its boundaries.
        Some(st) => st == "idle",
        // Hookless lane: the pane. `None` means "cannot tell" — for a herdr
        // lane mid-turn the capture is empty BY DESIGN — and must not read as
        // idle.
        None => pane_idle.unwrap_or(false),
    };
    if idle {
        return SteerDelivery::AtBoundary;
    }
    if age_s >= max_age_s {
        return SteerDelivery::OverdueMidTurn;
    }
    SteerDelivery::Hold
}

pub(crate) async fn steer_lane_at_boundary(state: &AppState, name: &str) -> bool {
    // 1. Self-report (hooks). "active" = mid-turn, "waiting" = at a selector.
    let reported: Option<String> = state
        .store
        .read()
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        })
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v[name]["state"].as_str().map(str::to_string));
    if let Some(st) = reported {
        return st == "idle";
    }
    // 2. Hookless lane: the pane. Empty capture means "cannot tell" — and for a
    // herdr lane mid-turn the capture is empty BY DESIGN (herdr refuses a
    // history read while working/blocked), so treating empty as idle would
    // deliver into exactly the state we are trying to avoid.
    let raw = tmux_capture(name, 12).await;
    if raw.trim().is_empty() {
        return false;
    }
    pane_is_at_boundary(&raw)
}

/// Is this pane at a turn boundary? Composed so the GATE and the SEND PATH read
/// the same frame the same way.
///
/// They did not, for one build, and it produced a deadlock that looked like a
/// bug in neither half: `detect_claude_status` reports a bypass bar containing
/// "esc to interrupt" as IDLE (17c5a3c's inverse, still live in that function),
/// so the gate said "at a boundary", passed `allow_mid_turn = false`, and the
/// send path — which reads the bar correctly — refused with "session started
/// generating". Every tick, forever, with the overdue deadline never consulted
/// because the gate never believed the lane was busy. A view that disagrees
/// with the mechanism it describes is worse than no view (ethos rule 1).
pub(crate) fn pane_is_at_boundary(raw: &str) -> bool {
    !pane_bar_says_generating(raw) && detect_claude_status(raw) == "idle"
}

/// The same two signals `steer_lane_at_boundary` reads, fed into
/// [`steer_decide`] together with the message's age.
pub(crate) async fn steer_delivery_for(state: &AppState, name: &str, age_s: f64) -> SteerDelivery {
    let reported: Option<String> = state
        .store
        .read()
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        })
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v[name]["state"].as_str().map(str::to_string));
    let pane_idle = if reported.is_some() {
        None
    } else {
        let raw = tmux_capture(name, 12).await;
        if raw.trim().is_empty() { None } else { Some(pane_is_at_boundary(&raw)) }
    };
    steer_decide(reported.as_deref(), pane_idle, age_s, steer_max_age_s())
}

/// One pass: deliver queued steering to every lane that is at a turn boundary.
/// Returns how many messages were delivered (the count is the test's handle —
/// a loop whose only evidence is "no rows left" cannot distinguish delivered
/// from dropped).
/// Pull the board card id out of an auto-pickup "work it now" prompt, if this
/// text IS one. Keyed on the literal template minted by `board_drive.rs` (the
/// sole producer of "[amux auto-pickup] Claimed board card <ID> from your
/// queue"); any other steering text returns None, so a non-pickup message is
/// never voided. KEEP IN STEP with that template: if its wording changes this
/// returns None and the AMUX-3052 stale-pickup guard silently goes dark, so the
/// two are commented as a pair and pinned by `pickup_card_id_parses_the_template`.
fn pickup_card_id(text: &str) -> Option<String> {
    const ANCHOR: &str = "Claimed board card ";
    const TAIL: &str = " from your queue";
    let start = text.find(ANCHOR)? + ANCHOR.len();
    let rest = text.get(start..)?;
    let end = rest.find(TAIL)?;
    let id = rest.get(..end)?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// AMUX-3052 void decision, extracted from `steer_deliver_tick` so BOTH legs are
/// unit-testable in isolation (a drop-guard that silently drops everything passes
/// a drop-only suite — gtm-engine's negative control). Given a queued message's
/// guard, its text, and the card's LIVE status at delivery time (None = the row
/// is gone), return Some(card) to VOID the pickup or None to DELIVER it.
///
/// Keyed ONLY on the live status, and queue duration is deliberately not a
/// parameter, so no later edit can re-key the guard on "it waited a long time" —
/// the exact wrong fix gtm-engine's two legs rule out: GE-626 (done 229ms after
/// the claim, delivered 18.7s later) must DROP, and MS-1188 (still `doing` at
/// delivery after a 578s wait, closed only afterward) must DELIVER.
/// AF-135 second instance (2026-08-22): keyed on OWNERSHIP as well as status.
/// A pickup for AF-132 was composed while the card was the recipient's, then a
/// peer reassigned + claimed it; at delivery the card WAS 'doing' — under the
/// other session — so the status-only check delivered "work it now" for a card
/// the recipient was not allowed to touch, aimed at a file the owner had 174
/// uncommitted lines in (AMUX-1315's exact hazard, reachable by following an
/// amux instruction literally). `card_session: None` (couldn't read, or the
/// column was empty) DELIVERS — matching the read-failure posture above: a
/// valid pickup delivered beats one dropped on a transient gap.
fn pickup_stale_void(
    guard: &str,
    text: &str,
    card_status: Option<&str>,
    card_session: Option<&str>,
    recipient: &str,
) -> Option<String> {
    if !guard.starts_with("board-drive") {
        return None; // not a board-drive delivery — never void a user/inter-session message
    }
    let card = pickup_card_id(text)?; // not a single-card pickup (e.g. a nudge) — deliver
    if card_status != Some("doing") {
        return Some(card); // moved off 'doing' (closed/bounced) or gone — void
    }
    match card_session {
        Some(owner) if owner != recipient => Some(card), // a peer took it — void
        _ => None, // still the recipient's actionable claimed card — deliver
    }
}

pub async fn steer_deliver_tick(state: &AppState) -> usize {
    // Only lanes that actually HAVE a queue: costs nothing on an empty fleet,
    // and keeps the pane captures below proportional to real work.
    let queued: Vec<(String, String, String, f64, String, String)> = {
        let Ok(conn) = state.store.read() else { return 0 };
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, session, text, queued_at, COALESCE(guard,''), COALESCE(sender,'') \
             FROM steering_queue ORDER BY queued_at ASC",
        ) else {
            return 0;
        };
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .map(|it| it.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        rows
    };
    if queued.is_empty() {
        return 0;
    }
    // ONE DELIVERY per lane per tick, oldest first. Delivering a whole backlog
    // into a single turn boundary would concatenate 9 unrelated instructions
    // into one prompt; the next tick takes the next one once the lane is idle
    // again, which is what "delivers at the turn boundary" has to mean to be
    // worth anything.
    //
    // ONE DELIVERY, NOT ONE ATTEMPT. This used to `continue` to the next LANE
    // as soon as a row refused, so a single undeliverable row froze its lane's
    // entire queue — amux-rust, 2026-08-09: its oldest row refused on every
    // tick for 229 minutes and the nine messages behind it never got a turn.
    // A queue where one bad row stops all the others is not a queue. So a
    // refusal now moves to that lane's NEXT row within the same tick, and the
    // stuck row is retried (it stays queued, ordering preserved for everything
    // that CAN go) rather than blocking.
    let mut delivered_lanes: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Per-lane block reason, evaluated once per tick and CACHED AS A VALUE —
    // not a checked-set. Every row of a blocked lane must reach the
    // dead-letter decision below, not just the first (AMUX-3110): with the old
    // set + short-circuit, a 10-day backlog on a dead lane would drain one row
    // per tick at best and in practice never, because the skip re-ran first.
    let mut lane_blocks: std::collections::HashMap<String, Option<&'static str>> =
        std::collections::HashMap::new();
    let mut delivered = 0usize;
    for (id, session, text, queued_at, guard, sender) in queued {
        if delivered_lanes.contains(&session) {
            continue; // this lane already got its one delivery this tick
        }
        // VOID A STALE AUTO-PICKUP (AMUX-3052) before any per-lane gating, so a
        // pickup for a card that was closed after the claim is dropped even if
        // the lane is momentarily unreachable. The claim is an atomic CAS on
        // status='todo' (claim_card), so the card is 'doing' at claim time; the
        // "work it now" prompt then waits in this queue until the lane's next
        // turn, and if the OWNER closes the card in that gap the lane is told to
        // redo finished work. gtm-engine measured ~6% of pickup deliveries onto
        // an already-closed card across 9 lanes; GE-626 was closed 229ms after
        // the claim and delivered 18.7s later — so any queue wait is enough, and
        // the fix belongs HERE at the delivery boundary, not in latency tuning.
        // The re-check reads the LIVE issues.status row — NOT session_events
        // (records the claim but not the close on this path) and NOT
        // issues.updated (stale at the claim second), per gtm-engine's forensics.
        if guard.starts_with("board-drive") {
            if let Some(card_id) = pickup_card_id(&text) {
                // Ok(Some(status)) found · Ok(None) deleted · Err → read failed,
                // so DO NOT void — delivering a valid pickup beats dropping one on
                // a transient read error.
                let live: Option<Option<(String, String)>> = match state.store.read() {
                    Ok(conn) => match conn.query_row(
                        "SELECT status, COALESCE(session,'') FROM issues WHERE id=?1 AND deleted IS NULL",
                        rusqlite::params![card_id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    ) {
                        Ok(s) => Some(Some(s)),
                        Err(rusqlite::Error::QueryReturnedNoRows) => Some(None),
                        Err(_) => None,
                    },
                    Err(_) => None,
                };
                if let Some(row) = live {
                    let status = row.as_ref().map(|(st, _)| st.clone());
                    let owner = row.as_ref().map(|(_, ow)| ow.clone()).filter(|o| !o.is_empty());
                    // The read succeeded (Some); route the void/deliver decision
                    // through the unit-tested authority so the two never drift.
                    if pickup_stale_void(&guard, &text, status.as_deref(), owner.as_deref(), &session)
                        .is_some()
                    {
                        let now = now_f64();
                        let delay_s = now - queued_at;
                        let became = status.unwrap_or_else(|| "gone".into());
                        // Two-fixes log signal: `became` + `delay_s` ARE the
                        // decision->delivery delta gtm-engine asked for, and the
                        // message.voided event makes the fleet-wide rate queryable
                        // (GET /api/logs or the events store) without a grep.
                        tracing::warn!(
                            session = %session, id = %id, card = %card_id,
                            became = %became, delay_s,
                            "voided a stale auto-pickup — card left 'doing' before delivery \
                             (AMUX-3052); dispatching 'work it now' would redo finished work"
                        );
                        let hist_text = format!(
                            "[VOIDED: card {card_id} left 'doing' -> {became}] {}",
                            redact_secrets(&text)
                        );
                        let (id2, sess2) = (id.clone(), session.clone());
                        let _ = state
                            .store
                            .write_async(move |conn| {
                                ensure_fleet_tables(conn)?;
                                conn.execute("DELETE FROM steering_queue WHERE id=?", [&id2])?;
                                conn.execute(
                                    "INSERT OR REPLACE INTO steering_history(id, session, text, queued_at, delivered_at) \
                                     VALUES(?,?,?,?,?)",
                                    rusqlite::params![id2, sess2, hist_text, queued_at, now],
                                )?;
                                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                            })
                            .await;
                        emit_event(
                            state,
                            &session,
                            "message.voided",
                            Some(json!({
                                "reason": "pickup-stale", "card": card_id,
                                "became": became, "delay_s": delay_s,
                                "delivered": false,
                                "preview": chars_truncate(&text, 80),
                            })),
                            Some(format!("void:{id}")),
                            "board-drive",
                        )
                        .await;
                        continue;
                    }
                } else {
                    // READ FAILED (Err), so the stale-check could not run: fall
                    // through and DELIVER (fail-open — losing a valid pickup is
                    // worse than a spurious one). But emit a signal, because the
                    // void event only fires on the DROP path, so a stale pickup let
                    // through here would otherwise be invisible. Without this, a
                    // degrading DB makes the measured void rate FALL while the true
                    // stale rate rises, reading as the fix working better exactly
                    // when the guard has stopped running (gtm-engine, AMUX-3052).
                    // Same message.voided type with delivered=true keeps the
                    // denominator (caught + let-through) queryable in one place; a
                    // nonzero count here is independently page-worthy — the guard is
                    // deployed but blind.
                    tracing::warn!(
                        session = %session, id = %id, card = %card_id,
                        "auto-pickup stale-check read FAILED — delivering (fail-open); \
                         the AMUX-3052 guard could not run for this pickup, so a stale \
                         one could slip through uncounted"
                    );
                    emit_event(
                        state,
                        &session,
                        "message.voided",
                        Some(json!({
                            "reason": "pickup-check-failed", "card": card_id,
                            "delivered": true,
                            "preview": chars_truncate(&text, 80),
                        })),
                        Some(format!("checkfail:{id}")),
                        "board-drive",
                    )
                    .await;
                }
            }
        }
        // Per-lane preconditions are evaluated once, not per row — via the SAME
        // predicate the send path and the queue listing use, so a lane can never
        // be told "queued, delivers at the next boundary" by one and skipped as
        // unreachable by the other (AMUX-2785).
        let block = match lane_blocks.get(&session) {
            Some(b) => *b,
            None => {
                let b = lane_block_reason(&session).await;
                lane_blocks.insert(session.clone(), b);
                b
            }
        };
        if let Some(reason) = block {
            // DEAD-LETTER (AMUX-3110). The comment that used to live here said
            // "the expiry sweep owns dead lanes" — no such sweep was ever
            // written, so a row whose target never materialises was retried
            // every 5s forever. Measured before this fix: 13 rows, 11
            // unroutable, the oldest 10.3 days; the same dead target
            // (amux-rust) re-queued six days after its first ghost was
            // hand-deleted, which is the proof that cleanup does not hold —
            // producers keep writing to dead names, so the queue needs an exit.
            //
            // Scope is the MEASURED class only: no-env-file and archived are
            // permanently unroutable while the state holds (no delivery loop
            // ever wakes them). `not-running` is deliberately NOT here — the
            // 2026-08-19 panic took 41 registered lanes down for 6.5h and
            // every queued row delivered on restart; age cannot distinguish
            // that outage from a dead lane, so a registered-but-stopped lane
            // keeps its queue. The grace window exists for the recreate/rename
            // race (rename-remap has its own path).
            if let Some(dead) = steer_dead_letter_verdict(reason, now_f64() - queued_at) {
                let (id2, sess2, text2) = (id.clone(), session.clone(), text.clone());
                let dead2 = dead.to_string();
                let _ = state
                    .store
                    .write_async(move |conn| {
                        ensure_fleet_tables(conn)?;
                        conn.execute("DELETE FROM steering_queue WHERE id=?", [&id2])?;
                        conn.execute(
                            "INSERT OR REPLACE INTO steering_history(id, session, text, queued_at, delivered_at, outcome) \
                             VALUES(?,?,?,?,?,?)",
                            rusqlite::params![id2, sess2, text2, queued_at, now_f64(), format!("dead:{dead2}")],
                        )?;
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    })
                    .await;
                tracing::warn!(
                    session = %session, id = %id, reason, sender = %sender,
                    age_s = (now_f64() - queued_at) as i64,
                    "dead-lettered a steering row — target permanently unroutable; \
                     the message is preserved in steering_history with outcome dead:{reason}"
                );
                emit_event(
                    state,
                    &session,
                    "session.steer_dead_lettered",
                    Some(json!({
                        "id": id, "reason": reason, "sender": sender,
                        "age_s": (now_f64() - queued_at) as i64,
                        "text_preview": text.chars().take(160).collect::<String>(),
                    })),
                    Some(format!("steerdead:{id}")),
                    "steering",
                )
                .await;
            } else {
                skip(&session, &id, reason);
            }
            continue;
        }
        // THE TURN-BOUNDARY GATE. This has to live HERE, in the caller.
        // `from_steering` inside send_text_inner only refuses on a selector, or
        // on generating-AND-picker-text; a plain message to a merely GENERATING
        // lane falls straight through and delivers. Python never relied on that
        // — its tick computed the state and only called _steer_try_deliver at a
        // boundary. Shipping without this gate delivered Ethan's queued message
        // into a working lane within minutes ("i sent as a queue but it looks
        // like it was sent directly even though this worker was still
        // working"), which defeats the entire point of queueing.
        // VOID A STALE PICKER ANSWER rather than typing it as prose
        // (AMUX-2823). A keypress is only meaningful while its picker is up;
        // once it is gone the same characters become an instruction the model
        // will try to obey. Ethan's "1. Stop and wait for limit to reset"
        // delivered into an empty prompt and cost mvs-infra 1m41s.
        if guard == "selector-answer" {
            let pane = tmux_capture(&session, 30).await;
            if !answers_visible_picker(&text, &pane) {
                tracing::warn!(
                    session = %session, id = %id,
                    "voided a stale picker answer — the menu it answered is gone; \
                     delivering it as text would be an instruction nobody gave"
                );
                let (id2, sess2, text2) = (id.clone(), session.clone(), text.clone());
                let _ = state
                    .store
                    .write_async(move |conn| {
                        ensure_fleet_tables(conn)?;
                        conn.execute("DELETE FROM steering_queue WHERE id=?", [&id2])?;
                        conn.execute(
                            "INSERT OR REPLACE INTO steering_history(id, session, text, queued_at, delivered_at) \
                             VALUES(?,?,?,?,?)",
                            rusqlite::params![
                                id2,
                                sess2,
                                format!("[VOIDED: picker gone] {}", redact_secrets(&text2)),
                                queued_at,
                                now_f64()
                            ],
                        )?;
                        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                    })
                    .await;
                emit_event(
                    state,
                    &session,
                    "message.voided",
                    Some(json!({"reason": "picker-gone", "preview": chars_truncate(&text, 80)})),
                    Some(format!("void:{id}")),
                    "steering",
                )
                .await;
                continue;
            }
        }
        let age = now_f64() - queued_at;
        let decision = steer_delivery_for(state, &session, age).await;
        let mid_turn = match decision {
            SteerDelivery::Hold => {
                skip(&session, &id, "not-at-turn-boundary (within max age)");
                // A younger row cannot be older than this one, so no row on
                // this lane can be overdue either: stop walking it.
                delivered_lanes.insert(session.clone());
                continue;
            }
            SteerDelivery::OverdueMidTurn => true,
            SteerDelivery::AtBoundary => false,
        };
        // from_steering=true is still passed: it makes the callee REFUSE rather
        // than re-queue if the lane starts generating between this check and the
        // send, so a lost race leaves the row where it is instead of duplicating.
        let (ok, msg) = send_text_inner(state, &session, &text, false, true, mid_turn, false).await;
        if !ok {
            skip(&session, &id, &format!("send-refused: {msg}"));
            continue; // NEXT ROW for this lane, not the next lane
        }
        if mid_turn {
            tracing::warn!(
                session = %session, id = %id, age_min = (age / 60.0) as i64,
                "steering delivered MID-TURN — it waited past AMUX_STEER_MAX_AGE_S for a boundary that never came"
            );
        }
        // Delivered — move the row to history under the SAME contract the
        // manual deliver path uses (redacted text, queued_at preserved).
        let (id2, sess2, text2) = (id.clone(), session.clone(), text.clone());
        let _ = state
            .store
            .write_async(move |conn| {
                ensure_fleet_tables(conn)?;
                let queued_at: f64 = conn
                    .query_row(
                        "SELECT queued_at FROM steering_queue WHERE id=?",
                        [&id2],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| now_f64());
                conn.execute("DELETE FROM steering_queue WHERE id=?", [&id2])?;
                conn.execute(
                    "INSERT OR REPLACE INTO steering_history(id, session, text, queued_at, delivered_at) \
                     VALUES(?,?,?,?,?)",
                    rusqlite::params![id2, sess2, redact_secrets(&text2), queued_at, now_f64()],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        delivered += 1;
        delivered_lanes.insert(session.clone());
        steer_skips().lock().unwrap().remove(&session);
        // NO SILENT WORK on the QUEUED path (AMUX-3148). The DIRECT send path mints
        // a ledger card for a human prompt (cmd_hist_record_full, AMUX-3071), but
        // this steering-queue deliverer never did — so a prompt to a BUSY lane
        // (which is MOST prompts to an active agent) was delivered and left no
        // board trace. amux's own session went from 89 capture cards to zero the
        // week after the cutover for exactly this reason ("none of these have board
        // items wtf"): its prompts queue while it is mid-turn and drain through
        // HERE, past the one place that cards. Mirror the direct path's predicate:
        //   guard == ""    — not a board-drive nudge / auto-pickup / self-describe
        //   sender == ""   — a human/dashboard send, not a peer relay (which carries
        //                    the server-verified origin and is type='session', never
        //                    the recipient's own task — same split as 9720 vs 9722)
        //   title Some     — a real task, not control text / [no-board] / a keypress
        // Separate write so a capture failure can never roll back the delivery, and
        // IDEMPOTENT: skip if this exact prompt was already carded (the enqueue path
        // may have minted at record time), so a queued message is never double-carded.
        if guard.is_empty()
            && sender.is_empty()
            && amux_core::board::title_from_prompt(&text).is_some()
        {
            let (sess3, text3) = (session.clone(), text.clone());
            let now_ms = (now_f64() * 1000.0) as i64;
            let minted: std::sync::Arc<std::sync::Mutex<Option<String>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            let minted_w = minted.clone();
            let res = state
                .store
                .write_async(move |conn| {
                    // Already carded (enqueue-time direct mint)? Never double-card.
                    let already: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM cmd_history \
                             WHERE session = ?1 AND text = ?2 AND card_id IS NOT NULL",
                            rusqlite::params![sess3, text3],
                            |r| r.get(0),
                        )
                        .unwrap_or(0);
                    if already > 0 {
                        return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                    }
                    match mint_capture_card(conn, &sess3, &text3, now_ms)? {
                        Some(row) => {
                            // Link the most recent uncarded cmd_history row for this
                            // prompt, if the enqueue recorded one without carding it.
                            conn.execute(
                                "UPDATE cmd_history SET card_id = ?1 WHERE id = \
                                 (SELECT id FROM cmd_history WHERE session = ?2 AND text = ?3 \
                                  AND card_id IS NULL ORDER BY id DESC LIMIT 1)",
                                rusqlite::params![row.id, sess3, text3],
                            )?;
                            *minted_w.lock().unwrap() = Some(row.id.clone());
                            let ev = crate::db::PendingEvent {
                                entity_type: amux_core::revision::EntityType::Task,
                                entity_id: row.id.clone(),
                                mutation: amux_core::revision::MutationKind::Created,
                                payload: Some(row.snapshot()),
                            };
                            Ok(crate::db::WriteOutcome { applied: true, events: vec![ev] })
                        }
                        None => Ok(crate::db::WriteOutcome { applied: false, events: vec![] }),
                    }
                })
                .await;
            match res {
                // Positive + failure log signals (two-fixes rule): the queued path now
                // announces its captures the same way the direct path does, so a
                // future silent stop is a queryable absence, not an invisible one.
                Ok(_) => {
                    if let Some(cid) = minted.lock().unwrap().take() {
                        tracing::info!(session = %session, id = %id, card_id = %cid,
                            "ledger: auto-captured board card from STEERING-delivered prompt (AMUX-3148)");
                    }
                }
                Err(e) => tracing::warn!(session = %session, error = %e,
                    "ledger auto-capture FAILED on steering delivery; prompt delivered without a board card"),
            }
        }
        // The metadata AMUX-2643's "direct vs queued" view needs, recorded on
        // EVERY delivery path: how it was queued, how long it waited, whether
        // it went in at a boundary or mid-turn, and the submission verdict.
        emit_event(
            state,
            &session,
            "message.delivered",
            Some(json!({
                "id": id,
                "via": "steering",
                "mode": if mid_turn { "overdue-mid-turn" } else { "at-boundary" },
                "queued_age_s": age as i64,
                "submission": msg,
                "preview": chars_truncate(&redact_secrets(&text), 120),
            })),
            Some(format!("steer-delivered:{id}")),
            "steering",
        )
        .await;
        tracing::info!(session = %session, id = %id, detail = %msg, mode = if mid_turn { "overdue-mid-turn" } else { "at-boundary" }, "steering delivered");
    }
    // A lane whose oldest row keeps refusing must become VISIBLE. Four hours of
    // silent refusal is the ethos-4 failure in this incident: the skip left no
    // trace anywhere, so finding it took a hand-written DB read. The age comes
    // from `queued_at`, so this needs no new state to be true after a restart.
    warn_on_stalled_lanes(state).await;
    delivered
}

/// Why each lane's last skipped row was skipped. In-memory and therefore
/// FICTION ACROSS A RESTART — deliberately: it is a debug surface, and the
/// durable fact (how long the oldest row has been queued) lives in
/// `steering_queue.queued_at` where the stall warning reads it from.
type SkipRecord = (String, String, f64); // (row id, reason, when)
fn steer_skips() -> &'static std::sync::Mutex<BTreeMap<String, SkipRecord>> {
    static M: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, SkipRecord>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

/// Has this exact stall already been LOGGED in this hour, for this lane and this
/// condition? Returns true the first time and false after, for the same key.
///
/// AEAB-13. `warn_on_stalled_lanes` emitted its `tracing::warn!` on every tick
/// with no memory, while the `emit_event` calls immediately below it were
/// already deduped on `steer-stalled:{lane}:{cond}:{bucket}` — hourly, per lane,
/// per condition — with a comment naming the exact hazard: "no idem at all fires
/// every tick, which is the nag AC-310 was filed about." The event path was
/// fixed; the log line was left behind.
///
/// The cost was not theoretical. One undeliverable message to a lane that had
/// been dead six days produced 921 of the 1004 log lines since the last restart
/// — 92% of the log — and 3570 lines across the preceding 24 hours. It buried a
/// first-ever `database is locked` line and two false scheduler warnings during
/// a log review that existed to find exactly those.
///
/// This deliberately REUSES the event path's key rather than inventing a second
/// cadence: two spellings of "the same stall" would drift, and the whole defect
/// is that one surface deduped and the other did not.
///
/// The MECHANISM now lives in `crate::log_dedupe` for the same reason, one level
/// up: AEAB-45 found the identical shape in the disk-ranking warning (1,336
/// identical lines in 24h, 77% of the window), and copying this function there
/// would have been the second spelling this comment warns about. See that module
/// for why the cadence is hourly rather than once-ever.
fn stall_log_first_this_bucket(key: &str, bucket: i64) -> bool {
    crate::log_dedupe::first_this_bucket(key, bucket)
}

fn skip(session: &str, id: &str, reason: &str) {
    tracing::debug!(session = %session, id = %id, reason = %reason, "steering skipped");
    steer_skips()
        .lock()
        .unwrap()
        .insert(session.to_string(), (id.to_string(), reason.to_string(), now_f64()));
}

/// Lanes whose oldest queued message is older than the delivery deadline are
/// announced at WARN with the last refusal reason. Keyed to the SAME number the
/// deliverer uses, so "stalled" always means "past the point where this should
/// have gone out no matter what" — a warning threshold that disagrees with the
/// mechanism it describes is the ethos-1 view/predicate split.
fn steer_stall_warn_s() -> f64 {
    steer_max_age_s()
}

async fn warn_on_stalled_lanes(state: &AppState) {
    let rows: Vec<(String, f64, i64, String)> = {
        let Ok(conn) = state.store.read() else { return };
        let Ok(mut stmt) = conn.prepare(
            "SELECT session, MIN(queued_at), COUNT(*), COALESCE(GROUP_CONCAT(DISTINCT sender),'') \
             FROM steering_queue GROUP BY session",
        ) else {
            return;
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    };
    let now = now_f64();
    let skips = steer_skips().lock().unwrap().clone();
    for (session, oldest, count, senders) in rows {
        let age = now - oldest;
        if age < steer_stall_warn_s() {
            continue;
        }
        let reason = skips
            .get(&session)
            .map(|(_, r, _)| r.clone())
            .unwrap_or_else(|| "unknown (no skip recorded since this process started)".into());
        // `blocked`/`cond`/`bucket` are computed HERE rather than below so the log
        // line can share the event path's dedupe key. Same call, same frequency —
        // only the order moved.
        let blocked = lane_block_reason(&session).await;
        let bucket = crate::log_dedupe::hour_bucket(now);
        let cond = blocked.unwrap_or("busy-past-deadline");
        if stall_log_first_this_bucket(&format!("{session}:{cond}"), bucket) {
            tracing::warn!(
                session = %session,
                queued = count,
                oldest_min = (age / 60.0) as i64,
                reason = %reason,
                "steering queue STALLED — messages are not reaching this lane"
            );
        }

        // ETHOS RULE 4, which is the whole of this card: "when this goes wrong,
        // what will someone see — and will they see it WHERE THEY ALREADY
        // LOOK?" A `tracing::warn!` is neither. Nothing reached the sender, who
        // is the one party holding a false belief ("I sent it") and the only one
        // who can act on it; the reporter of the original incident concluded
        // "the amux session is stuck", which is the wrong diagnosis this silence
        // produces.
        //
        // So the stall becomes a durable event on the LANE (where anyone
        // debugging that lane looks) and on each SENDER (where the belief
        // lives). It carries the SKIP REASON, because `not-running` and
        // `no-env-file` are actionable and completely different from `busy` —
        // collapsing them to "queued" is what made this invisible.
        // Dedupe per lane per CONDITION per hour. A permanent idem would fire
        // once and stay silent if the lane recovered and re-stalled; no idem at
        // all fires every tick, which is the nag AC-310 was filed about.
        let detail = match blocked {
            Some(r) => block_reason_explain(r, &session),
            None => format!(
                "'{session}' is reachable but has not reached a turn boundary — {count} message(s) \
                 waiting, oldest {}m. It should have been forced through mid-turn by now; the last \
                 refusal was: {reason}.",
                (age / 60.0) as i64
            ),
        };
        let payload = json!({
            "lane": session,
            "queued": count,
            "oldest_age_s": age as i64,
            "deliverable": blocked.is_none(),
            "blocked_reason": blocked,
            "last_skip_reason": reason,
            "detail": detail,
        });
        emit_event(
            state,
            &session,
            "steering.stalled",
            Some(payload.clone()),
            Some(format!("steer-stalled:{session}:{cond}:{bucket}")),
            "steering",
        )
        .await;
        for sender in senders.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if sender == session {
                continue; // already told, above
            }
            emit_event(
                state,
                sender,
                "steering.undelivered",
                Some(payload.clone()),
                Some(format!("steer-undelivered:{sender}:{session}:{cond}:{bucket}")),
                "steering",
            )
            .await;
        }
    }
}

/// GET /api/debug/steering — per-lane queue depth, oldest age, and why the last
/// skipped row was skipped. Exists because this incident was undiagnosable from
/// the outside: the tick logged only successes, so a lane that had refused
/// every 5 seconds for four hours looked exactly like a lane with nothing to do.
async fn steering_debug(State(state): State<AppState>) -> Response {
    let rows: Vec<(String, f64, i64, String)> = {
        let Ok(conn) = state.store.read() else {
            return jresp(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "store unavailable"}));
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT session, MIN(queued_at), COUNT(*), COALESCE(GROUP_CONCAT(DISTINCT sender),'') \
             FROM steering_queue GROUP BY session",
        ) else {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "query failed"}));
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    };
    let now = now_f64();
    let skips = steer_skips().lock().unwrap().clone();
    let mut lanes: Vec<Value> = Vec::new();
    for (session, oldest, count, senders) in rows {
        let (last_id, reason, at) = skips
            .get(&session)
            .cloned()
            .unwrap_or_else(|| (String::new(), String::new(), 0.0));
        // LIVE, not remembered. `last_skip_reason` is in-memory and empty after
        // a restart — which is exactly when someone is most likely to be
        // reading this endpoint — so it could report a 15-hour stall with no
        // reason at all. This one is computed on the spot from the same
        // predicate the deliverer uses.
        let blocked = lane_block_reason(&session).await;
        lanes.push(json!({
            "session": session,
            "queued": count,
            "oldest_age_s": (now - oldest) as i64,
            "stalled": now - oldest >= steer_stall_warn_s(),
            "overdue": now - oldest >= steer_max_age_s(),
            "deliverable": blocked.is_none(),
            "blocked_reason": blocked,
            "senders": senders.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>(),
            "last_skip_id": last_id,
            "last_skip_reason": reason,
            "last_skip_age_s": if at > 0.0 { json!((now - at) as i64) } else { Value::Null },
        }));
    }
    j200(json!({
        "lanes": lanes,
        "stall_warn_s": steer_stall_warn_s(),
        "max_age_s": steer_max_age_s(),
        "max_age_env": "AMUX_STEER_MAX_AGE_S",
        "note": "last_skip_* is in-memory and resets when the server restarts; queued/oldest_age_s are durable",
    }))
}

/// Deliver the oldest queued steering message for ONE specific session.
/// Called reactively when a session reports "idle" — the report IS the turn
/// boundary, so there is no need to re-check `steer_lane_at_boundary` (the
/// caller just wrote "idle" into session_reports). This closes the race where
/// the 5s poll missed a < 1s idle window between turns: 9 messages sat queued
/// for over 2 hours while the session processed direct user input.
pub async fn steer_deliver_for_session(state: &AppState, session: &str) -> bool {
    let session_s = session.to_string();
    // OLDEST-FIRST, but walk past a row that refuses (AMUX-2629 head-of-line):
    // taking only `LIMIT 1` meant one undeliverable message froze the lane's
    // whole queue, which is how amux-rust accumulated 10 messages over 229
    // minutes while this function ran on every idle report and did nothing.
    let rows: Vec<(String, String, f64)> = state
        .store
        .read()
        .ok()
        .and_then(|conn| {
            conn.prepare(
                "SELECT id, text, queued_at FROM steering_queue WHERE session=?1 ORDER BY queued_at ASC",
            )
            .and_then(|mut st| {
                st.query_map([&session_s], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .map(|it| it.flatten().collect::<Vec<_>>())
            })
            .ok()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        return false;
    }
    if !env_path(session).exists() {
        return false;
    }
    if !is_running(session).await {
        skip(session, "", "not-running");
        return false;
    }
    let mut id = String::new();
    let mut text = String::new();
    let mut sent = None;
    let mut was_mid_turn = false;
    for (rid, rtext, queued_at) in rows {
        // This function is called BECAUSE the lane just reported idle, so the
        // boundary is not in question; the age still decides whether a lane
        // that flickers idle-then-busy gets an overdue delivery.
        let age = now_f64() - queued_at;
        let mid = age >= steer_max_age_s();
        // hook_confirmed_idle=true: the Stop hook just reported idle — trust
        // it over the pane scrape. The pane may still show "esc to interrupt"
        // from background agents, which is NOT generation.
        let (ok, msg) = send_text_inner(state, session, &rtext, false, true, mid, true).await;
        if ok {
            id = rid;
            text = rtext;
            was_mid_turn = mid;
            sent = Some((msg, age));
            break;
        }
        skip(session, &rid, &format!("send-refused: {msg}"));
    }
    let Some((msg, age)) = sent else { return false };
    steer_skips().lock().unwrap().remove(session);
    let (id2, sess2, text2) = (id.clone(), session_s.clone(), text.clone());
    let _ = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            let queued_at: f64 = conn
                .query_row(
                    "SELECT queued_at FROM steering_queue WHERE id=?",
                    [&id2],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| now_f64());
            conn.execute("DELETE FROM steering_queue WHERE id=?", [&id2])?;
            conn.execute(
                "INSERT OR REPLACE INTO steering_history(id, session, text, queued_at, delivered_at) \
                 VALUES(?,?,?,?,?)",
                rusqlite::params![id2, sess2, redact_secrets(&text2), queued_at, now_f64()],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    emit_event(
        state,
        session,
        "message.delivered",
        Some(json!({
            "id": id,
            "via": "steering",
            "mode": if was_mid_turn { "overdue-mid-turn" } else { "at-boundary" },
            "queued_age_s": age as i64,
            "submission": msg,
            "preview": chars_truncate(&redact_secrets(&text), 120),
        })),
        Some(format!("steer-delivered:{id}")),
        "steering",
    )
    .await;
    tracing::info!(session = %session, id = %id, detail = %msg, "steering delivered (reactive)");
    true
}

// ---------------------------------------------------------------------------
// pipe-pane RECONCILER (AMUX-2671).
//
// `pipe-pane` is attached in start_session and NOWHERE ELSE — there was no
// reconciler anywhere in runtime_jobs. So a pane that loses its writer (it
// died, or the pane predates a writer fix) stays unlogged forever and nothing
// notices: pane_pipe=0 looks identical to a lane that simply has not been
// started. Found on rec-gov — a live `node` agent, 1 child, logging nothing.
//
// SAFETY, from the code rather than assumed: plain `pipe-pane` (NO `-o`) is
// idempotent — tmux closes any existing pipe before running the new command.
// Do NOT add `-o`: it means "only if none exists" but is implemented as
// close-then-decline, i.e. a toggle OFF on an already-piped pane, which once
// silently unlogged 29 of 60 panes.
//
// The writer comes from log_pipe_command() and is never hand-rolled, because
// that function is where REDACTION lives (sk-ant-, ANTHROPIC_API_KEY=, ghp_,
// AIza..., sk-proj-, POSTHOG_KEY). Re-arming a pane with a bare `cat` works and
// redacts nothing — I did exactly that by hand while diagnosing this and had to
// detach it. "Re-arming is safe" is true of the tmux VERB, not of an arbitrary
// command handed to it.
// ---------------------------------------------------------------------------

/// Should this pane be re-armed? Pure, so the discriminator is testable without
/// a tmux server — and so the NEGATIVE case is pinned as tightly as the
/// positive one.
///
/// `children == 0` is a BARE SHELL: the tmux session outlived its agent. Piping
/// those would spray shell noise into per-worker logs for lanes that have no
/// worker; 10 of the 11 unpiped panes measured were exactly that (disposable
/// smprobe*/zz-* test lanes) and only ONE was a live agent.
fn should_rearm_pipe(pane_pipe: i64, children: usize) -> bool {
    pane_pipe == 0 && children > 0
}

/// One reconciliation pass. Returns how many panes were re-armed — a count, so
/// "the reconciler ran and found nothing" is distinguishable from "it did not
/// run", which is the failure this whole module exists to make visible.
pub async fn pipe_reconcile_tick() -> usize {
    let Some(out) = tmux(&[
        "list-panes", "-a", "-F", "#{session_name} #{pane_pipe} #{pane_pid}",
    ])
    .await
    else {
        return 0; // tmux unreachable: not a reconciliation, and not a pass
    };
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut rearmed = 0usize;
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(sess), Some(pipe), Some(pid)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let Some(name) = sess.strip_prefix("amux-") else { continue };
        let pipe: i64 = pipe.parse().unwrap_or(1); // unparsable -> assume piped, never re-arm blind
        let children = tokio::process::Command::new("pgrep")
            .args(["-P", pid])
            .output()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
            .unwrap_or(0);
        if !should_rearm_pipe(pipe, children) {
            continue;
        }
        let lp = log_path(name);
        let cmd = log_pipe_command(&lp);
        let pt = pane_target(sess);
        let _ = tmux(&["pipe-pane", "-t", &pt, &cmd]).await;
        rearmed += 1;
        tracing::warn!(session = %name, children, "re-armed a lost pipe-pane");
    }
    rearmed
}

/// 60s: a lost pipe is a durable condition, not a race, and this shells out
/// per unpiped pane — polling it hard would cost more than the logging it
/// restores. Named (and pub) so lib.rs registers the cadence it actually
/// sleeps, not a second copy of the number.
pub const PIPE_RECONCILE_SECS: u64 = 60;

/// Background driver.
pub async fn pipe_reconcile_loop() {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(PIPE_RECONCILE_SECS)).await;
        crate::runtime_jobs::registry::tick(crate::runtime_jobs::registry::ids::PIPE_RECONCILE);
        if let Err(e) = tokio::spawn(pipe_reconcile_tick()).await {
            tracing::error!(error = %e, "pipe reconcile tick panicked");
        }
    }
}


/// How often the fleet is swept for rate-limit menus. NOT every steering tick:
/// that is 5s, and a pane capture per running lane across ~113 lanes would be
/// ~22 captures/second forever, paid on the resource the fleet is already
/// contending for.
fn rate_limit_sweep_secs() -> f64 {
    std::env::var("AMUX_RATE_LIMIT_SWEEP_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v >= 10.0)
        .unwrap_or(60.0)
}

/// Stamp (and, unless the policy says otherwise, ANSWER) every lane sitting on a
/// rate-limit menu.
///
/// THE SEND PATH ALONE IS NOT ENOUGH. Detection there only fires when somebody
/// happens to be sending, so a lane that hits its limit with nothing queued is
/// invisible until someone tries to talk to it — which is precisely the state
/// mvs-infra was in. A fleet that only notices a limit when you poke it does not
/// answer "which workers are limited right now", which is the question this
/// exists for.
async fn rate_limit_sweep(state: &AppState) -> usize {
    let dir = sessions_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else { return 0 };
    let mut found = 0usize;
    // One fleet-wide pass, shared with the status derivation — the stuck-
    // composer gate below must agree with derive_status about whether a
    // lane's background agents are live, or a lane reads `active` on the
    // fleet list and `stuck` here in the same breath.
    let sub_activity = crate::api::sessions_legacy::scan_subagent_activity();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|x| x.to_str()) else { continue };
        let cfg = parse_env(name);
        if cfg.get("CC_ARCHIVED") == Some("1") {
            continue;
        }
        // ISOLATED (AMUX-3232): a raw agent gets no status scrape and no
        // rate-limit auto-answer. amux does not press keys into a lane whose
        // whole point is to run untouched. Same skip as archived.
        if env_flag_on(cfg.get("CC_ISOLATED")) {
            continue;
        }
        if !is_running(name).await {
            continue;
        }
        let pane = tmux_capture(name, 30).await;

        // RESUME-MODE SELECTOR: amux's to answer (D2; policy set once by Ethan
        // 2026-08-19 — "resume from summary", fleet-wide, model-agnostic). The
        // send path also answers it, but only when somebody happens to be
        // sending; after a machine reboot a third of the fleet parks here with
        // nothing queued, which is exactly the sweep's reason to exist. Digit 1,
        // not Enter — see resume_mode_action. Answered means the pane is mid-
        // transition, so skip this lane's remaining state derivation this tick.
        // With the policy `off` we fall through and the selector is stamped
        // input-required below, like any other picker.
        if resume_mode_action() != "off" && is_resume_mode_prompt(&pane) {
            let (ok, msg) = send_keys_op(name, "1").await;
            tracing::warn!(session = %name, ok, detail = %msg,
                "swept a resume-mode selector — answered '1. Resume from summary'");
            emit_event(
                state,
                name,
                "session.resume_mode_answered",
                Some(json!({"choice": "resume-from-summary", "ok": ok, "detail": msg, "detected_by": "sweep"})),
                Some(format!("resumemode:{name}:{}", now_i64() / 3600)),
                "status",
            )
            .await;
            if ok {
                continue;
            }
        }

        // INPUT-REQUIRED IS A DISTINCT STATE, and the dashboard has always been
        // able to show it (app.js:2405 renders `needs input`, and there is a
        // filter for it) — no lane ever reached it. The fleet listing derives
        // status from self-reports and tmux activity, neither of which can see a
        // PICKER; only a pane capture can, and this sweep is the one place that
        // already pays for one (AMUX-2834).
        //
        // A lane sitting on an AskUserQuestion is BLOCKED ON A HUMAN and is the
        // opposite of idle: nothing will move it, no deadline will force it, and
        // amux must not answer it (typing there rejects the pending tool — the
        // 2026-07-15 kill). Reading as `idle` is exactly backwards, and it hid
        // mvs-infra behind a menu for 400s+ earlier today.
        //
        // A rate-limit menu is EXCLUDED: it is also a selector, but amux owns it
        // and answers it below, so flagging it would ask a human for something
        // nobody needs to decide.
        let selector_now = !is_rate_limit_menu(&pane) && detect_claude_status(&pane) == "waiting";
        let selector_was = meta_i64(&load_meta(name), "input_required_since") > 0;
        if selector_now != selector_was {
            update_meta(
                name,
                &[("input_required_since", json!(if selector_now { now_i64() } else { 0 }))],
            );
            if selector_now {
                emit_event(
                    state,
                    name,
                    "session.input_required",
                    Some(json!({"detected_by": "sweep"})),
                    Some(format!("inputreq:{name}:{}", now_i64() / 3600)),
                    "status",
                )
                .await;
            }
        }

        // STUCK COMPOSER (AMUX-2904). Genuinely TYPED text sits under `❯`
        // while the main loop is idle and no background agent is live —
        // an Enter that never landed, or a human's committed-but-unsubmitted
        // command. The lane read `idle` throughout, which is how unsubmitted
        // text sits invisible for hours: ghost-rescue presses Enter for
        // amux-prefixed text, but deliberately leaves everything else alone
        // (a false rescue submits a human's half-written thought), and until
        // now "left alone" also meant "invisible". This stamp is the visible
        // half: surface it as `waiting`, decide nothing.
        //
        // MUST go through composer_state().typed(), never a stripped-ANSI
        // read of the ❯ line: Claude Code paints a dim SUGGESTION in the
        // empty composer, and attribute-blind reading of it is exactly the
        // 2026-08-09 incident where 13 idle lanes were reported as holding
        // stuck text ("push it", "continue with the queue", …) and three
        // people pressed Enter on frames with nothing to submit. The
        // composer_state_tests pin that discrimination; this caller inherits
        // it. Text while GENERATING or while agents are hot is the legitimate
        // queue doing its job and is not stamped.
        let agents_live = sub_activity
            .get(name)
            .is_some_and(|m| now_f64() - m < 180.0);
        let typed_pending = (!is_rate_limit_menu(&pane)
            && !selector_now
            && !pane_bar_says_generating(&pane)
            && detect_claude_status(&pane) != "active"
            && !agents_live)
            .then(|| composer_state(&pane).typed().map(|t| t.to_string()))
            .flatten();
        let stuck_now = typed_pending.is_some();
        let meta_now = load_meta(name);
        let stuck_was = meta_i64(&meta_now, "composer_stuck_since") > 0;
        if stuck_now != stuck_was {
            let preview = typed_pending
                .as_deref()
                .map(|t| chars_truncate(t, 120))
                .unwrap_or_default();
            update_meta(
                name,
                &[
                    ("composer_stuck_since", json!(if stuck_now { now_i64() } else { 0 })),
                    ("composer_preview", json!(if stuck_now { preview.clone() } else { String::new() })),
                ],
            );
            if stuck_now {
                tracing::warn!(session = %name, preview = %preview,
                    "unsubmitted text is stuck in the composer with no live turn or agents — the lane will read `waiting` until it is submitted or cleared");
                emit_event(
                    state,
                    name,
                    "session.composer_stuck",
                    Some(json!({"preview": preview, "detected_by": "sweep"})),
                    Some(format!("composerstuck:{name}:{}", now_i64() / 3600)),
                    "status",
                )
                .await;
            }
        }

        // ONLY the interactive rate-limit MENU is a reliable state signal. A
        // banner-word scrape ("your usage limit", "/usage-credits") cannot tell
        // Claude's OWN banner from a lane merely DISCUSSING or CODING a rate limit
        // in its work, and it false-flagged working lanes including `amux` itself
        // (Ethan, 2026-08-16). The menu is a selector amux renders and answers, so
        // it cannot be conversation. Persisting the status past the menu needs a
        // reliable signal amux does not have by scraping (a reset time, or a Claude
        // Code hook reporting rate-limit state, the D2 exit) — not a fuzzy banner.
        // LIMITED = the interactive menu OR Claude's on-credits BANNER in the
        // FOOTER (the status region above the input box). The menu is transient
        // (amux answers it); the credit banner is the persistent state Ethan sees
        // as "workers with a rate limit" — a lane past the menu, still working ON
        // CREDITS. Footer-scoped + full-sentence so a lane CODING the banner (amux
        // itself) is not flagged; the earlier whole-pane scrape was the false
        // positive. pane = tmux_capture(name, 30) ends at the current footer, so
        // its last 8 lines ARE the footer.
        let menu = is_rate_limit_menu(&pane);
        let footer = {
            let ls: Vec<&str> = pane.lines().collect();
            ls[ls.len().saturating_sub(8)..].join("\n")
        };
        let limited = menu || is_rate_limited_credit_banner(&footer);
        if !limited {
            // Neither the menu nor the credit banner is on screen: clear the stamp.
            // Presence-based, and both signals are footer/menu-scoped, so scrollback
            // ABOUT limits never flags and a lane clears as soon as its banner goes.
            if meta_i64(&load_meta(name), "rate_limited_since") > 0 {
                update_meta(name, &[("rate_limited_since", json!(0))]);
            }
            continue;
        }
        found += 1;
        if meta_i64(&load_meta(name), "rate_limited_since") == 0 {
            update_meta(
                name,
                &[
                    ("rate_limited_since", json!(now_i64())),
                    ("rate_limited_model", json!(cfg.get_or("CC_MODEL", ""))),
                ],
            );
            emit_event(
                state,
                name,
                "session.rate_limited",
                Some(json!({"detected_by": if menu { "menu" } else { "credit-banner" }})),
                Some(format!("rl:{name}:{}", now_i64() / 3600)),
                "rate-limit",
            )
            .await;
        }
        // Answer ONLY the interactive menu (a banner has no selector, and a stray
        // Enter into a working lane injects a newline). The presence-based clear
        // above handles everything else.
        if menu && rate_limit_action() != "off" {
            let (ok, msg) = send_keys_op(name, "Enter").await;
            tracing::warn!(session = %name, ok, detail = %msg, "swept a rate-limit menu — answered 'stop and wait'");
        }
    }
    found
}

/// Background driver. Spawned from lib.rs; ticks every STEER_TICK_SECS.
pub async fn steer_deliver_loop(state: AppState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(STEER_TICK_SECS)).await;
        crate::runtime_jobs::registry::tick(crate::runtime_jobs::registry::ids::STEER_DELIVER);
        // A panic in one tick must not kill delivery for the whole fleet.
        let st = state.clone();
        if let Err(e) = tokio::spawn(async move { steer_deliver_tick(&st).await }).await {
            tracing::warn!(error = %e, "steering delivery tick panicked");
        }
        // Time-gated so the 5s steering cadence does not become a 5s fleet-wide
        // pane capture (AMUX-2820).
        let due = {
            static LAST: std::sync::OnceLock<std::sync::Mutex<f64>> = std::sync::OnceLock::new();
            let cell = LAST.get_or_init(|| std::sync::Mutex::new(0.0));
            let mut g = cell.lock().unwrap();
            if now_f64() - *g >= rate_limit_sweep_secs() {
                *g = now_f64();
                true
            } else {
                false
            }
        };
        if due {
            let st2 = state.clone();
            if let Err(e) = tokio::spawn(async move { rate_limit_sweep(&st2).await }).await {
                tracing::warn!(error = %e, "rate-limit sweep panicked");
            }
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/sessions/{name}", any(session_root_handler))
        // A WARN nobody can query is the same gap one layer out: this is
        // where a sweep or an autofix loop asks "did anything get delivered
        // twice, and to whom" without grepping a pane log.
        .route("/api/debug/duplicate-deliveries", axum::routing::get(debug_duplicate_deliveries))
        .route("/api/sessions/{name}/{*verb}", any(session_verb_handler))
        // Why steering is or is not moving. See `steering_debug`.
        .route("/api/debug/steering", axum::routing::get(steering_debug))
        // CANONICAL SPELLING, same dispatcher. `/api/sessions/*` is exempt from
        // the alias layer (aliases.rs: the bare list has a dedicated shape
        // handler and the verbs used to proxy to Python), so nothing was
        // rewriting `/api/workers/<n>/<verb>` onto these — the canonical name
        // for the verbs simply did not exist, and only the legacy one answered.
        //
        // That is not cosmetic: the INSTALLED `amux send` posts to
        // /api/workers/<n>/send. Against Python it worked; after the cutover it
        // got 405 and the CLI fell back to RAW TMUX KEYSTROKES — unstamped,
        // unaudited, delivery unverified. So every session's `amux send` lost
        // the origin stamp that AMUX-1768 exists to provide and that CLAUDE.md
        // instructs every session to rely on ("provenance comes from the server
        // stamp, not the text"). Two long inter-session messages were confirmed
        // LOST through that fallback the same afternoon.
        //
        // Fixed server-side rather than in the CLI deliberately: a CLI fix only
        // reaches machines that reinstall, while the route fixes every already
        // installed copy at once (ethos rule 1 — capability has to actually
        // reach everyone, not just exist).
        .route("/api/workers/{name}/{*verb}", any(session_verb_handler))
        // Long prompts ride /send bodies; axum's 2MB default is Python's cap
        // too (none), so disable rather than invent one.
        .layer(axum::extract::DefaultBodyLimit::disable())
}

async fn session_root_handler(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    dispatch(state, name, String::new(), method, headers, q, body).await
}

async fn session_verb_handler(
    State(state): State<AppState>,
    AxumPath((name, verb)): AxumPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(q): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    dispatch(state, name, verb, method, headers, q, body).await
}

async fn dispatch(
    state: AppState,
    name: String,
    verb: String,
    method: Method,
    headers: HeaderMap,
    q: Option<String>,
    body_bytes: axum::body::Bytes,
) -> Response {
    // Python's route regex allows exactly action(/subid); deeper nesting 404s.
    let mut parts = verb.splitn(3, '/');
    let action = parts.next().unwrap_or("").to_string();
    let subid = parts.next().unwrap_or("").to_string();
    if parts.next().is_some() {
        return not_found();
    }
    // Rust-managed worker? Its LIFECYCLE verbs are the modern API's (kept from
    // the retired proxy's guard — a legacy-path call gets a pointer, never a
    // silent 404).
    //
    // But the pointer only helps when the modern API actually implements the
    // verb, and for `peek`/`send` it did not: `/api/workers` mounts exactly
    // `/`, `/{id}`, `/{id}/start`, `/{id}/stop`, `/{id}/peek`, so a send fell
    // through to THIS dispatcher and answered 501 naming itself — a pointer at
    // the path that just refused. The dashboard reaches every session verb by
    // name through `/api/sessions/<name>/…`, so peek rendered blank and no
    // prompt could be delivered to any worker in the store, herdr or tmux;
    // `amux send` posts to `/api/workers/<name>/send` and hit the same wall,
    // and its 501 carries an `error` field, so the CLI classified it as
    // "server understood and refused" and did not retry.
    //
    // These two verbs are implemented HERE and nowhere else — origin stamping
    // (AMUX-1768), cross-group scoping, msg_id idempotency, board capture and
    // the delivery verdict all live in `send_post`. So they fall through, and
    // everything else keeps the pointer until it has a modern home.
    const NATIVE_ONLY_HERE: [&str; 2] = ["peek", "send"];
    if !NATIVE_ONLY_HERE.contains(&action.as_str()) {
        let is_rust_worker = state
            .store
            .read()
            .ok()
            .and_then(|conn| crate::db::queries::get_worker(&conn, &name).ok().flatten())
            .is_some();
        if is_rust_worker {
            return jresp(
                StatusCode::NOT_IMPLEMENTED,
                json!({
                    "error": "rust-managed worker — use /api/workers",
                    "worker": name,
                    "hint": format!("/api/workers/{name}"),
                }),
            );
        }
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let body: Value = match parse_body(&body_bytes) {
        Ok(v) => v,
        Err(e) => return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})),
    };
    // Validate session exists (py:74882) — for every action, share included.
    // ONE exception: a RETRY of a partially-completed rename addresses the
    // OLD name after its env file already moved; admit it so the convergent
    // cascade can finish the remainder (owner addendum, AMUX-2598).
    if !env_path(&name).exists() {
        // Covers BOTH spellings, or the alias would diverge from the canonical
        // path on exactly the retry this exception exists for: a partially
        // completed rename addresses the OLD name after its env file already
        // moved. An alias that 404s where the original resumes is not an alias.
        let rename_resume = ((method == Method::PATCH && action == "config")
            || (method == Method::POST && action == "rename"))
            && body
                .get("rename")
                .or_else(|| body.get("name"))
                .and_then(|v| v.as_str())
                .map(|r| {
                    let target = sanitize_session_name(r);
                    !target.is_empty() && target != name && env_path(&target).exists()
                })
                .unwrap_or(false);
        if !rename_resume {
            return jresp(StatusCode::NOT_FOUND, json!({"error": format!("session '{name}' not found")}));
        }
    }

    // /share is its own family in Python (py:65953), any method.
    if action == "share" {
        return share_handler(&state, &name, &method, &headers, &body).await;
    }

    if method == Method::GET || method == Method::HEAD {
        return get_dispatch(&state, &name, &action, &subid, &qs).await;
    }
    if action == "tracked-files" && (method == Method::POST || method == Method::DELETE) {
        return tracked_files_mutate(&name, &method, &body);
    }
    if action == "steer" {
        return steer_mutate(&state, &name, &method, &headers, &body).await;
    }
    if method == Method::POST {
        return post_dispatch(&state, &name, &action, &headers, &body).await;
    }
    if method == Method::PATCH {
        // Bare PATCH on the resource aliases the config verb — the fourth
        // instance of the reach-for-the-obvious-verb class documented on the
        // DELETE alias below (found live 2026-08-11: a tags edit sent to
        // PATCH /api/sessions/<n> answered a bare 404 that named nothing,
        // while /config sat one path segment away). Same rule as DELETE: an
        // alias to the SAME function, so the two spellings cannot drift.
        let act = if action.is_empty() { "config" } else { action.as_str() };
        return patch_dispatch(&state, &name, act, &body).await;
    }
    // DELETE on the RESOURCE — the conventional REST spelling (AMUX-2665).
    //
    // Deletion lived only at POST /api/sessions/<n>/delete. The SPA uses that
    // (app.js:4315), so nothing looked broken — but anything reaching for the
    // obvious verb got this function's 405 at the bottom. Third of three such
    // gaps found tonight, and the first one cost real damage:
    //   POST /api/workers/<n>/send    405 -> `amux send` fell back to raw tmux,
    //                                        two inter-session messages lost
    //   POST /api/sessions/<n>/rename 404 -> AMUX-2669, fixed
    //   DELETE /api/sessions/<n>      405 -> this
    //
    // An ALIAS: it calls delete_post, the same function the POST spelling
    // reaches, so the two cannot drift. Guarded on an EMPTY action so
    // `DELETE /api/sessions/<n>/<something>` keeps 405-ing rather than
    // silently deleting the whole worker — a DELETE that ignores its subpath
    // is far worse than one that refuses.
    if method == Method::DELETE && action.is_empty() {
        return delete_post(&state, &name, &headers).await;
    }
    jresp(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "method not allowed"}))
}


/// A lane's subagents, from `~/.claude/projects/<proj>/<conv-uuid>/subagents/`.
///
/// The parent conversation's owner resolves through [`conversation_owner`] —
/// meta claim first, last title record second — the same resolution the
/// token-ledger indexer and the fleet's subagent-activity scan use,
/// deliberately not a second one (it WAS a first-line read here, which
/// attributed the `amux` lane's subagents to its pre-rename name).
///
/// A subagent's own first record is a `fork-context-ref` (agentId,
/// parentSessionId, contextLength); the handed-down task carries
/// `subagent_type` and a human-readable `description`. Both are reported when
/// present and omitted when not — a made-up label is worse than none in a
/// switcher, because it cannot be told from a real one.
fn session_subagents(name: &str) -> Value {
    let projects = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claude/projects");
    let mut out: Vec<Value> = Vec::new();
    let Ok(projs) = std::fs::read_dir(&projects) else {
        return json!({"session": name, "subagents": [], "source": "transcripts"});
    };
    let claims = conversation_claims();
    for proj in projs.flatten() {
        let Ok(convs) = std::fs::read_dir(proj.path()) else { continue };
        for c in convs.flatten() {
            let conv = c.path();
            if conv.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if conversation_owner(&conv, &claims) != name {
                continue;
            }
            let stem = conv.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let dir = conv.with_extension("").join("subagents");
            let Ok(agents) = std::fs::read_dir(&dir) else { continue };
            for a in agents.flatten() {
                let p = a.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let meta = p.metadata().ok();
                let modified = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let (kind, description, turns) = subagent_head(&p);
                out.push(json!({
                    "id": p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                    "conversation": stem,
                    "type": kind,
                    "description": description,
                    "turns": turns,
                    "last_active": modified,
                    "bytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                }));
            }
        }
    }
    // Most recently active first: a switcher is read to jump to what is moving.
    out.sort_by_key(|v| -(v["last_active"].as_i64().unwrap_or(0)));
    json!({"session": name, "subagents": out, "source": "transcripts"})
}

/// (subagent_type, description, turn count). Scans only the head of the file —
/// the fork boilerplate is in the first few records and some of these are tens
/// of MB.
fn subagent_head(path: &Path) -> (Value, Value, i64) {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else { return (Value::Null, Value::Null, 0) };
    let mut kind = Value::Null;
    let mut desc = Value::Null;
    let mut turns = 0i64;
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let Ok(line) = line else { break };
        turns += 1;
        if i > 12 || (!kind.is_null() && !desc.is_null()) {
            // Keep counting turns cheaply once the head is parsed.
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let blob = v.to_string();
        for key in ["subagent_type", "subagentType", "agentType"] {
            if kind.is_null() {
                if let Some(x) = json_str_after(&blob, key) {
                    kind = json!(x);
                }
            }
        }
        if desc.is_null() {
            if let Some(x) = json_str_after(&blob, "description") {
                desc = json!(x);
            }
        }
    }
    (kind, desc, turns)
}

/// `"key": "value"` out of a serialized record. The task metadata is nested
/// inside tool-use content blocks whose shape Claude Code changes freely, so a
/// typed struct here would silently stop matching; this degrades to None
/// instead, which the caller reports as an absent label rather than a wrong one.
fn json_str_after(blob: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let i = blob.find(&pat)? + pat.len();
    let rest = blob[i..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let v = &rest[..end];
    (!v.is_empty() && v.len() <= 200).then(|| v.to_string())
}


/// GET /api/debug/duplicate-deliveries?hours=24 — every lane that received the
/// SAME text twice inside the detector's window, newest first.
///
/// Reads `session_events` rows the detector writes, so it reports what actually
/// happened rather than re-deriving a heuristic. `window_ms` is echoed so a
/// reader knows what "duplicate" meant without going to the source.
async fn debug_duplicate_deliveries(State(state): State<AppState>, RawQuery(q): RawQuery) -> Response {
    let params = parse_qs(q.as_deref().unwrap_or(""));
    let hours: f64 = qs_get(&params, "hours").and_then(|v| v.parse().ok()).unwrap_or(24.0);
    let since = now_f64() - hours * 3600.0;
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return jresp(StatusCode::SERVICE_UNAVAILABLE, json!({"error": e.to_string()})),
    };
    let rows: Vec<Value> = conn
        .prepare(
            "SELECT ts, session, data FROM session_events \
             WHERE type='message.duplicate' AND ts > ?1 ORDER BY ts DESC LIMIT 200",
        )
        .and_then(|mut st| {
            let it = st.query_map(rusqlite::params![since], |r| {
                let data: Option<String> = r.get(2)?;
                Ok(json!({
                    "ts": r.get::<_, f64>(0)?,
                    "session": r.get::<_, String>(1)?,
                    "detail": data
                        .and_then(|d| serde_json::from_str::<Value>(&d).ok())
                        .unwrap_or(Value::Null),
                }))
            })?;
            Ok(it.flatten().collect())
        })
        .unwrap_or_default();
    let mut by_session: BTreeMap<String, i64> = BTreeMap::new();
    for r in &rows {
        *by_session.entry(r["session"].as_str().unwrap_or("").to_string()).or_insert(0) += 1;
    }
    j200(json!({
        "hours": hours,
        "window_ms": DUP_DELIVERY_WINDOW_MS,
        "total": rows.len(),
        "by_session": by_session,
        "events": rows,
        "note": "a duplicate is the SAME text recorded twice for one lane inside window_ms. \
                 Deliveries are never suppressed on this signal — two identical sends can be \
                 deliberate, and dropping one would turn a visible annoyance into silent loss.",
    }))
}

fn qs_first<'a>(qs: &'a [(String, String)], key: &str, default: &'a str) -> &'a str {
    qs_get(qs, key).unwrap_or(default)
}
fn qs_flag(qs: &[(String, String)], key: &str) -> bool {
    matches!(qs_get(qs, key), Some("1") | Some("true") | Some("yes"))
}

// ---------------------------------------------------------------------------
// GET verbs (py:74887-75418).
// ---------------------------------------------------------------------------

async fn get_dispatch(
    state: &AppState,
    name: &str,
    action: &str,
    subid: &str,
    qs: &[(String, String)],
) -> Response {
    match action {
        "" => {
            // Bare GET → the SAME record the list endpoint serves (py:74892).
            let conn = match state.store.read() {
                Ok(c) => c,
                Err(e) => return jresp(StatusCode::SERVICE_UNAVAILABLE, json!({"error": e.to_string()})),
            };
            match crate::api::sessions_legacy::build_array(&conn) {
                Ok(arr) => {
                    match arr.into_iter().find(|x| x["name"] == json!(name)) {
                        Some(rec) => j200(rec),
                        None => jresp(
                            StatusCode::NOT_FOUND,
                            json!({"error": format!("session '{name}' not found")}),
                        ),
                    }
                }
                Err(e) => jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
            }
        }
        "tasks" => j200(session_cc_tasks(name).await),
        // GET /api/sessions/<n>/subagents — this lane's forks/subagents, read
        // from their DURABLE transcripts rather than inferred from the pane
        // (AMUX-2635).
        //
        // The pane-glyph predicate this replaces matched 0 of 50 lanes, twice,
        // confirmed in both directions. The transcripts matched 50 of 50 — same
        // question, opposite answer, because the instrument was wrong and not
        // the feature. That is D1's documented exit: a real interface instead of
        // a scrape of rendered output, and it improves as Claude Code does
        // rather than breaking on the next glyph change.
        "subagents" => j200(session_subagents(name)),
        // Peek "Simple" tab (AMUX-3056): a plain-English summary of what this
        // worker just did, from its last assistant message via the shared
        // fastest/cheapest helper, cached per transcript+prompt. `?prompt=` is
        // the client-resolved standing prompt; `?refresh=1` forces regenerate.
        "simple" => {
            let prompt = qs_first(qs, "prompt", "");
            let generate = qs_flag(qs, "generate") || qs_flag(qs, "refresh");
            crate::api::simple::simple_response(
                name,
                if prompt.is_empty() { None } else { Some(prompt) },
                generate,
            )
            .await
        }
        "peek" => {
            let lines: i64 = qs_first(qs, "lines", "80").parse().unwrap_or(80);
            let live_only = qs_flag(qs, "live");
            let no_trim = qs_flag(qs, "notrim");
            // Geometry is attached HERE rather than inside peek_response because
            // that function has six return points (live_only, alt-screen, normal,
            // short-output, empty…) and a key added to one of them is a key the
            // reader cannot rely on. One injection site covers every shape.
            let mut resp = peek_response(name, lines, live_only, no_trim).await;
            if let (Some((cols, rows)), Some(obj)) =
                (tmux_pane_geometry(name).await, resp.as_object_mut())
            {
                obj.insert("pane_cols".into(), json!(cols));
                obj.insert("pane_rows".into(), json!(rows));
            }
            j200(resp)
        }
        "transcript" => {
            // Codex/Ollama run a native TUI whose raw mirror is what Ethan saw
            // looked nothing like Claude (AMUX-3201). They also write a
            // structured rollout to disk, so return that as the uniform
            // TranscriptEvent model and let the client render it Claude-like;
            // Claude keeps its ANSI `output`. A worker whose rollout can't be
            // resolved falls through to ANSI (empty for non-Claude), so the tab
            // degrades to a message rather than a wrong transcript.
            let provider = provider_of(&parse_env(name));
            if matches!(provider.as_str(), "codex" | "ollama") {
                let evs = codex_transcript_events(name, 400).unwrap_or_default();
                let empty = evs.is_empty();
                return j200(json!({
                    "name": name,
                    "provider": provider,
                    "events": evs,
                    "empty": empty,
                    "source": "codex-rollout",
                }));
            }
            let mx: usize = qs_first(qs, "max", "40000").parse().unwrap_or(40000);
            let txt = render_session_transcript(name, mx);
            if txt.is_empty() {
                j200(json!({"name": name, "provider": provider, "output": "", "empty": true}))
            } else {
                j200(json!({"name": name, "provider": provider, "output": txt, "source": "transcript"}))
            }
        }
        // The worker's most recent full assistant message, clean — what the
        // "read latest message" ellipsis action speaks (AMUX-3021).
        "last-message" => {
            let mx: usize = qs_first(qs, "max", "8000").parse().unwrap_or(8000);
            let txt = last_assistant_message(name, mx);
            j200(json!({
                "name": name,
                "text": txt,
                "chars": txt.chars().count(),
                "empty": txt.is_empty(),
            }))
        }
        "status-explain" => {
            // AMUX-3434: the status derivation's WHY — which rule decided,
            // over what evidence, inside which trust window — from the SAME
            // snapshot and the same code path the session list uses
            // (derive_status_explain IS the derivation; the list discards the
            // explanation). When a badge is wrong, this is one GET instead of
            // a screenshot investigation (AMUX-3426; ethos rule 4).
            let store = state.store.clone();
            let nm = name.to_string();
            let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let conn = store.read()?;
                let mut fs = crate::api::sessions_legacy::FleetSignals::load(&conn);
                fs.capture_panes();
                let running = fs.agent_running(&format!("amux-{nm}"));
                let (status, explain) = fs.derive_status_explain(&nm, running);
                Ok((running, status, explain))
            })
            .await;
            match joined {
                Ok(Ok((running, status, explain))) => j200(json!({
                    "session": name,
                    "running": running,
                    "status": status,
                    "explain": explain,
                    "note": "decided_by names the rule that produced `status`; transition/pane/report carry the evidence and trust windows the rules weighed. Fresh snapshot, same derivation the session list uses.",
                })),
                Ok(Err(e)) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response(),
            }
        }
        "info" => {
            // py:20461 get_session_info.
            let cfg = parse_env(name);
            let raw_dir = cfg.get_or("CC_DIR", "");
            let dir = if raw_dir.is_empty() { String::new() } else { work_dir_of(&cfg) };
            j200(json!({
                "name": name,
                "dir": dir,
                "desc": cfg.get_or("CC_DESC", ""),
                "pinned": cfg.get("CC_PINNED") == Some("1"),
                "tags": cfg.get_or("CC_TAGS", "").split(',').map(str::trim).filter(|t| !t.is_empty()).collect::<Vec<_>>(),
                "flags": cfg.get_or("CC_FLAGS", ""),
                "provider": cfg.get_or("CC_PROVIDER", "claude"),
                "running": is_running(name).await,
                "raw": std::fs::read_to_string(env_path(name)).unwrap_or_default(),
            }))
        }
        "instructions" => j200(json!({
            "name": name,
            "instructions": meta_str(&load_meta(name), "instructions").trim(),
        })),
        "dirty" => {
            let wd = session_work_dir(name);
            let files = if wd.is_empty() { vec![] } else { session_dirty_files(name, &wd).await };
            j200(json!({
                "name": name,
                "dirty": !files.is_empty(),
                "count": files.len(),
                "files": files.iter().take(50).collect::<Vec<_>>(),
            }))
        }
        "commit-guard" => {
            let cfg = parse_env(name);
            let per = cfg.get_or("AMUX_COMMIT_GUARD_SESSION", "").trim().to_lowercase();
            let global = !matches!(
                std::env::var("AMUX_COMMIT_GUARD").unwrap_or_else(|_| "1".into()).trim().to_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            );
            let override_v: Value = if per.is_empty() {
                Value::Null
            } else {
                json!(!matches!(per.as_str(), "0" | "false" | "off" | "no"))
            };
            let enabled = match &override_v {
                Value::Bool(b) => *b,
                _ => global,
            };
            j200(json!({"name": name, "enabled": enabled, "global": global, "override": override_v}))
        }
        "meta" => {
            // py:75162 — merged meta + env-derived fields.
            let cfg = parse_env(name);
            let meta = load_meta(name);
            let provider = provider_of(&cfg);
            let flags = cfg.get_or("CC_FLAGS", "");
            let env_mtime = env_path(name)
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mf = mem_file(name);
            let mem_size = mf.metadata().map(|m| m.len()).unwrap_or(0);
            let mut out = meta.clone();
            if !out.contains_key("creator") {
                out.insert("creator".into(), json!(cfg.get_or("CC_CREATOR", "")));
            }
            let configured = {
                let m = extract_model_from_flags(flags);
                if m.is_empty() { default_model_for_provider(&provider) } else { m }
            };
            out.insert("name".into(), json!(name));
            out.insert("dir".into(), json!(cfg.get_or("CC_DIR", "")));
            out.insert("provider".into(), json!(provider));
            out.insert("flags".into(), json!(flags));
            out.insert("configured_model".into(), json!(configured));
            out.insert("desc".into(), json!(cfg.get_or("CC_DESC", "")));
            out.insert(
                "tags".into(),
                json!(cfg.get_or("CC_TAGS", "").split(',').map(str::trim).filter(|t| !t.is_empty()).collect::<Vec<_>>()),
            );
            out.insert("env_updated".into(), json!(env_mtime));
            out.insert("mem_size".into(), json!(mem_size));
            out.insert("mem_path".into(), json!(mf.to_string_lossy()));
            j200(Value::Object(out))
        }
        "log" => log_get(name, subid, qs),
        "transcripts" => {
            if !subid.is_empty() {
                // Download one backup file.
                let tf = transcripts_dir().join(name).join(subid);
                if !tf.is_file() {
                    return not_found();
                }
                let Ok(data) = std::fs::read(&tf) else { return not_found() };
                return (
                    StatusCode::OK,
                    [
                        ("content-type", "application/x-ndjson".to_string()),
                        ("content-disposition", format!("attachment; filename=\"{subid}\"")),
                    ],
                    data,
                )
                    .into_response();
            }
            j200(json!({"transcripts": list_session_transcripts(name)}))
        }
        "tracked-files" => {
            let meta = load_meta(name);
            j200(json!({"files": meta.get("tracked_files").cloned().unwrap_or(json!([]))}))
        }
        "stats" => {
            let cfg = parse_env(name);
            j200(get_claude_stats(cfg.get_or("CC_DIR", "")))
        }
        "git" => git_get(name, subid, qs).await,
        "memory" => {
            let mf = mem_file(name);
            let content = std::fs::read_to_string(&mf).unwrap_or_default();
            let wd = session_work_dir(name);
            j200(json!({
                "content": content,
                "path": mf.to_string_lossy(),
                "work_dir": wd,
                "claude_project": if wd.is_empty() { String::new() } else { project_name(&wd) },
                "shared_with": memory_shared_with(name),
            }))
        }
        "memory-inherited" => {
            let wd = session_work_dir(name);
            let names: Vec<String> = qs_first(qs, "file", "")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            let effective = if names.is_empty() { mem_inherit_files() } else { names.clone() };
            let inh = inherited_instruction_files(&wd, &effective);
            let found: Vec<&Value> = inh.iter().filter(|l| l["exists"] == json!(true)).collect();
            let missing: Vec<Value> = inh
                .iter()
                .filter(|l| l["exists"] != json!(true))
                .map(|l| {
                    let mut m = l.as_object().cloned().unwrap_or_default();
                    m.remove("text");
                    Value::Object(m)
                })
                .collect();
            let total: u64 = found.iter().map(|l| l["bytes"].as_u64().unwrap_or(0)).sum();
            j200(json!({
                "worker": name,
                "dir": wd,
                "filenames": effective,
                "configured_by": "AMUX_MEMORY_INHERIT_FILES (server.env), or ?file= on this call",
                "note": "Loaded by Claude Code itself, not composed by amux — shown so the inheritance is visible, not duplicated into memory.",
                "found": found,
                "missing": missing,
                "total_bytes": total,
            }))
        }
        "search" => {
            let q = qs_first(qs, "q", "").trim().to_string();
            let lim_raw = qs_first(qs, "limit", "").trim().to_string();
            let lim: i64 = if lim_raw.is_empty() {
                0
            } else {
                lim_raw.parse::<i64>().map(|v| v.clamp(1, 2000)).unwrap_or(0)
            };
            let root = session_work_dir(name);
            if root.is_empty() {
                return j200(json!({
                    "session": name, "query": q, "root": "", "engine": "", "results": [],
                    "files": 0, "matches": 0, "truncated": false,
                    "searched_ignored": qs_flag(qs, "ignored"),
                    "searched_hidden": qs_flag(qs, "ignored"),
                    "limit": if lim != 0 { lim } else { 300 },
                    "error": "worker has no CC_DIR configured",
                }));
            }
            let literal = !matches!(qs_get(qs, "literal"), Some("0") | Some("false") | Some("no"));
            let case = qs_first(qs, "case", "smart").to_lowercase();
            let globs: Vec<String> =
                qs.iter().filter(|(k, v)| k == "glob" && !v.is_empty()).map(|(_, v)| v.clone()).collect();
            let mut out =
                crate::api::fs::fs_search(&root, &q, lim, literal, &case, qs_flag(qs, "ignored"), &globs).await;
            out.insert("session".into(), json!(name));
            let status = if out.get("error").and_then(|e| e.as_str()) == Some("missing query") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            };
            jresp(status, Value::Object(out))
        }
        "steer" => {
            let conn = match state.store.read() {
                Ok(c) => c,
                Err(e) => return jresp(StatusCode::SERVICE_UNAVAILABLE, json!({"error": e.to_string()})),
            };
            if qs_first(qs, "history", "0") == "1" {
                let mut out = vec![];
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT id, text, queued_at, delivered_at FROM steering_history \
                     WHERE session=? ORDER BY delivered_at DESC LIMIT 100",
                ) {
                    if let Ok(rows) = stmt.query_map([name], |r| {
                        Ok(json!({
                            "id": r.get::<_, String>(0)?,
                            "text": r.get::<_, String>(1)?,
                            "queued_at": r.get::<_, Option<f64>>(2)?,
                            "delivered_at": r.get::<_, f64>(3)?,
                        }))
                    }) {
                        out = rows.flatten().collect();
                    }
                }
                return j200(json!(out));
            }
            let mut out: Vec<Value> = vec![];
            if let Ok(mut stmt) = conn.prepare(
                "SELECT id, text, queued_at, COALESCE(guard,''), COALESCE(sender,'') FROM steering_queue \
                 WHERE session=? ORDER BY queued_at ASC",
            ) {
                if let Ok(rows) = stmt.query_map([name], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "text": r.get::<_, String>(1)?,
                        "queued_at": r.get::<_, f64>(2)?,
                        "guard": r.get::<_, String>(3)?,
                        "sender": r.get::<_, String>(4)?,
                    }))
                }) {
                    out = rows.flatten().collect();
                }
            }
            drop(conn);
            // AGE AND REACHABILITY PER ROW (AMUX-2785). A queue listing that
            // shows only text and a timestamp cannot answer the one question a
            // sender staring at it has — "is this coming, or is it stuck?" —
            // and the caller cannot compute it, because whether the lane can
            // receive at all is not in the row. Same predicate as the drain
            // loop, so the list cannot claim what the loop will not do.
            let blocked = lane_block_reason(name).await;
            let max_age = steer_max_age_s();
            let now = now_f64();
            for row in out.iter_mut() {
                let age = now - row["queued_at"].as_f64().unwrap_or(now);
                row["age_s"] = json!(age as i64);
                row["overdue"] = json!(age >= max_age);
                row["deliverable"] = json!(blocked.is_none());
                row["blocked_reason"] = json!(blocked);
            }
            j200(json!(out))
        }
        "env-explain" | "memory-explain" => jresp(
            StatusCode::NOT_IMPLEMENTED,
            json!({
                "error": format!("{action} is not ported to the rust origin yet"),
                "python_source": "amux-server.py:74901 (env-explain) / 74957 (memory-explain)",
                "note": "layered env/memory composition is a named residual gap in api/session_verbs.rs",
            }),
        ),
        _ => not_found(),
    }
}

/// GET log + log/info (py:75187-75250).
fn log_get(name: &str, subid: &str, qs: &[(String, String)]) -> Response {
    let lp = log_path(name);
    let want_plain = matches!(
        qs_first(qs, "plain", "0").to_lowercase().as_str(),
        "1" | "true" | "yes"
    );
    if subid == "info" {
        if want_plain {
            return match write_plain_log(name) {
                None => j200(json!({"exists": false, "size": 0, "path": plain_log_path(name).to_string_lossy(), "plain": true})),
                Some((cp, size)) => {
                    let mtime = cp
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    j200(json!({"exists": true, "size": size, "mtime": mtime, "path": cp.to_string_lossy(), "plain": true}))
                }
            };
        }
        let Ok(md) = lp.metadata() else {
            return j200(json!({"exists": false, "size": 0, "path": lp.to_string_lossy()}));
        };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Rotation is only honest if the reader is TOLD an older generation
        // exists — otherwise `size` reads as "everything this session ever
        // produced" and a roll looks like the log was cleared.
        let rp = rotated_log_path(name);
        let rotated = rp.metadata().map(|m| m.len()).ok();
        return j200(json!({
            "exists": true, "size": md.len(), "mtime": mtime, "path": lp.to_string_lossy(),
            "rotated": rotated.is_some(),
            "rotated_size": rotated.unwrap_or(0),
            "rotated_path": rotated.map(|_| rp.to_string_lossy().into_owned()),
            "rotate_max_bytes": log_rotate_bytes(),
        }));
    }
    if !subid.is_empty() {
        return not_found();
    }
    let Ok(mut data) = std::fs::read(&lp) else {
        return jresp(StatusCode::NOT_FOUND, json!({"error": "no log"}));
    };
    let tail_kb: usize = qs_first(qs, "tail_kb", "0").parse::<usize>().unwrap_or(0).min(1024);
    let before_kb: usize = qs_first(qs, "before_kb", "0").parse::<usize>().unwrap_or(0);
    if before_kb > 0 {
        let keep = data.len().saturating_sub(before_kb * 1024);
        data.truncate(keep);
    }
    let pre_len = data.len();
    if tail_kb > 0 && data.len() > tail_kb * 1024 {
        data = data[data.len() - tail_kb * 1024..].to_vec();
        if let Some(nl) = data.iter().position(|b| *b == b'\n') {
            if nl < 4096 {
                data = data[nl + 1..].to_vec();
            }
        }
    }
    let remaining = pre_len - data.len();
    if want_plain {
        let text = collapse_blank_runs(&strip_ansi(&String::from_utf8_lossy(&data)));
        data = text.into_bytes();
    }
    // Two INDEPENDENT ways this body is not the whole log, and a caller that
    // sees only `x-log-remaining` cannot tell them apart: the tail_kb slice
    // (bytes dropped from this file) and rotation (a whole prior generation
    // sitting in <name>.log.1). Report both, and a single boolean the caller
    // can branch on without arithmetic.
    let rotated_bytes = rotated_log_path(name).metadata().map(|m| m.len()).unwrap_or(0);
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain; charset=utf-8".to_string()),
            ("content-disposition", format!("attachment; filename=\"{name}.log\"")),
            ("x-log-remaining", remaining.to_string()),
            ("x-log-rotated-bytes", rotated_bytes.to_string()),
            (
                "x-log-truncated",
                if remaining > 0 || rotated_bytes > 0 { "1".to_string() } else { "0".to_string() },
            ),
        ],
        data,
    )
        .into_response()
}

/// GET git (+ commits / commit-detail / diff), py:75277-75361. The
/// _install_amux_commit_hook side effect is not ported (Python still owns
/// hook installation during coexistence).
async fn git_get(name: &str, subid: &str, qs: &[(String, String)]) -> Response {
    let wd = session_work_dir(name);
    match subid {
        "commits" => {
            let count: i64 = qs_first(qs, "count", "30").parse().unwrap_or(30);
            let fmt = "%H%x00%an%x00%ai%x00%s%x00%b%x1E";
            let count_arg = format!("-{count}");
            let fmt_arg = format!("--format={fmt}");
            let mut commits = vec![];
            if let Some(out) = git_out(&wd, &["log", &count_arg, &fmt_arg], Duration::from_secs(10)).await {
                let sess_re = cached_re!(r"(?m)^Amux-Session:\s*(.+)$");
                for entry in out.split('\u{1E}') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = entry.splitn(5, '\0').collect();
                    if parts.len() >= 4 {
                        let mut body_txt = parts.get(4).map(|s| s.trim().to_string()).unwrap_or_default();
                        let mut amux_sess = String::new();
                        if let Some(m) = sess_re.captures(&body_txt) {
                            amux_sess = m[1].trim().to_string();
                            body_txt = sess_re.replace_all(&body_txt, "").trim().to_string();
                        }
                        commits.push(json!({
                            "hash": parts[0], "author": parts[1], "date": parts[2],
                            "subject": parts[3], "body": body_txt, "amux_session": amux_sess,
                        }));
                    }
                }
            }
            j200(json!({"commits": commits}))
        }
        "commit-detail" => {
            let sha = qs_first(qs, "sha", "").to_string();
            if sha.is_empty() {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "sha required"}));
            }
            // Commit-ish only — prevents `--output=<path>` arbitrary writes.
            let ok_re =
                regex::Regex::new(r"^(?:[0-9a-fA-F]{4,64}|[A-Za-z0-9][A-Za-z0-9._/\-]{0,120})$").unwrap();
            if !ok_re.is_match(&sha) {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "invalid sha"}));
            }
            let show = git_out(&wd, &["show", &sha, "--stat", "--format=%H%n%an%n%ai%n%s%n%b%x00"], Duration::from_secs(10))
                .await
                .unwrap_or_default();
            let parts: Vec<&str> = show.splitn(2, '\0').collect();
            let meta: Vec<&str> = parts.first().map(|p| p.splitn(5, '\n').collect()).unwrap_or_default();
            let stat = parts.get(1).map(|s| s.trim().to_string()).unwrap_or_default();
            let diff = git_out(&wd, &["show", &sha, "--format="], Duration::from_secs(10)).await.unwrap_or_default();
            j200(json!({
                "hash": meta.first().copied().unwrap_or(sha.as_str()),
                "author": meta.get(1).copied().unwrap_or(""),
                "date": meta.get(2).copied().unwrap_or(""),
                "subject": meta.get(3).copied().unwrap_or(""),
                "body": meta.get(4).map(|s| s.trim()).unwrap_or(""),
                "stat": stat,
                "diff": diff,
            }))
        }
        "diff" => {
            let file_path = qs_first(qs, "file", "").to_string();
            let staged = qs_first(qs, "staged", "0") == "1";
            let base = qs_first(qs, "base", "").to_string();
            if !base.is_empty() {
                let base_re = cached_re!(r"^[A-Za-z0-9][A-Za-z0-9._/\-]{0,120}$");
                if !base_re.is_match(&base) {
                    return jresp(StatusCode::BAD_REQUEST, json!({"error": "invalid base"}));
                }
            }
            let mut args: Vec<String> = vec!["diff".into()];
            if !base.is_empty() {
                args.push(format!("{base}..HEAD"));
            } else if staged {
                args.push("--cached".into());
            }
            if !file_path.is_empty() {
                args.push("--".into());
                args.push(file_path.clone());
            }
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            let diff = git_out(&wd, &args_ref, Duration::from_secs(10)).await.unwrap_or_default();
            j200(json!({"diff": diff, "file": file_path}))
        }
        "" => {
            let detail = qs_first(qs, "detail", "0") == "1";
            let cfg = parse_env(name);
            let mut info = git_info(&wd, detail).await;
            if detail {
                let sb = cfg.get_or("CC_BRANCH", "");
                info["session_branch"] = json!(if sb == "none" { "" } else { sb });
            }
            j200(info)
        }
        _ => not_found(),
    }
}

// ---------------------------------------------------------------------------
// tracked-files POST/DELETE (py:75419) — includes the conversation-id
// adoption guard (cross-link refusal).
// ---------------------------------------------------------------------------

fn conversation_owned_by_other(conv_id: &str, this_session: &str) -> String {
    if conv_id.is_empty() {
        return String::new();
    }
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            let Some(fname) = p.file_name().and_then(|n| n.to_str()) else { continue };
            let Some(other) = fname.strip_suffix(".meta.json") else { continue };
            if other == this_session {
                continue;
            }
            if meta_str(&load_meta(other), "cc_conversation_id") == conv_id {
                return other.to_string();
            }
        }
    }
    String::new()
}

/// Outcome of a lane telling us its own conversation id. Named rather than a
/// bool because the three cases carry different information and exactly one of
/// them is an incident (`Conflict`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConvAdopt {
    /// Nothing usable in the report, or it already matches — no write.
    Unchanged,
    /// Stamped. Carries the previous value ("" when the lane had none), because
    /// "was blind, now visible" and "moved to a new conversation" are different
    /// events and only the first closes an absorption window.
    Adopted { was: String },
    /// Another lane's meta claims this id. REFUSED — see the comment below.
    Conflict { owner: String },
}

impl ConvAdopt {
    /// Rendered into the `/report` response body, not merely logged.
    ///
    /// Because a caller must be able to CONFIRM AT THE FIELD. The hook that
    /// feeds this fires unattended thousands of times a day; if adoption
    /// silently stops working — a payload key renamed upstream, a conflict that
    /// never clears — a 200 with `{"ok":true}` looks exactly like success. That
    /// is the failure this repo paid for twice on 2026-08-07 (a PATCH whose
    /// `ignored_fields` nobody read), and a self-healing mechanism is precisely
    /// the kind nobody watches once it appears to work.
    fn as_json(&self) -> Value {
        match self {
            ConvAdopt::Unchanged => json!({"adopted": false}),
            ConvAdopt::Adopted { was } => json!({
                "adopted": true,
                "previous": was,
                "healed_blind_lane": was.is_empty(),
            }),
            ConvAdopt::Conflict { owner } => json!({
                "adopted": false,
                "conflict_with": owner,
                "error": format!(
                    "another lane ('{owner}') already claims this conversation id; refusing to \
                     cross-link — this lane stays unresolvable to the staged guard until fixed"
                ),
            }),
        }
    }
}

/// Adopt a conversation id a lane REPORTED ABOUT ITSELF (Stop / UserPromptSubmit
/// hook payload -> `POST /api/sessions/<n>/report`).
///
/// WHY THIS EXISTS (AMUX-2936's resume trigger, fired 2026-08-15). The staged
/// guard calls a cotenant BLIND when it cannot resolve that lane's transcript,
/// and blind is the one class through which a work-absorbing commit passes
/// silently: `foreign` exits 1 and blocks, but an invisible lane cannot produce
/// a `foreign` row at all, so its files land in `unclaimed`.
///
/// Measured over 8h51m on 2026-08-15: 321 blind-cotenant warnings on
/// /Users/ethan/Dev/mixpeek, from 29 distinct committing lanes, and 304 of them
/// name ONE lane — `mixpeek-general`, which is RUNNING. Its meta has
/// `cc_conversation_id: ""`, so `session_jsonl_path` falls through to the title
/// match and the single-unclaimed fallback, finds 4 unclaimed transcripts in a
/// shared project dir, and CORRECTLY refuses to guess. The refusal is right; the
/// guessing is what should not have been necessary.
///
/// And it was not necessary: Claude Code hands every hook a payload containing
/// `session_id` and `transcript_path`. amux was discarding it — the Stop hook
/// posted a literal `{"state":"idle","source":"stop-hook"}` and read nothing
/// from stdin — and then spending a 162-file scan downstream trying to
/// re-derive precisely that id. The harness knows its own conversation; ask it
/// (ethos D1) instead of inferring from a shared directory's mtimes.
///
/// WHY NOT `find_latest_session_id` (the tempting fix, and the reason this is a
/// function with a refusal in it rather than an `update_meta` call): that is
/// "newest jsonl in the project dir", and ~/Dev/mixpeek hosts about thirty
/// lanes. On 2026-08-09 it stamped the `amux` lane with `amux-rust`'s LIVE
/// conversation during a model swap and the next start resumed a copy of
/// another lane's brain. Adopting a neighbour's transcript is strictly worse
/// than staying blind: blind under-reports, cross-linked MIS-reports, and the
/// guard would then confidently attribute a peer's edits to the wrong lane.
///
/// So the cross-link refusal is kept, and it is LOUD. A reported id that another
/// lane's meta already claims is the one shape that cannot be a benign restart,
/// and silently dropping it would leave the lane blind with no trace of why.
pub(crate) fn adopt_reported_conv_id(name: &str, reported: &str) -> ConvAdopt {
    let reported = reported.trim();
    // A transcript stem is a UUID. Anything else is a caller mistake, not an id;
    // refusing early keeps a typo out of meta where it would resolve to nothing
    // and look exactly like the blindness this is meant to end.
    let plausible = reported.len() >= 32
        && reported.len() <= 64
        && reported.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if !plausible {
        return ConvAdopt::Unchanged;
    }
    let was = meta_str(&load_meta(name), "cc_conversation_id");
    if was == reported {
        return ConvAdopt::Unchanged;
    }
    let owner = conversation_owned_by_other(reported, name);
    if !owner.is_empty() {
        tracing::warn!(
            target: "staged_guard",
            "[conv-adopt/AMUX-2936] session={} reported conv_id={} but lane '{}' already claims \
             it — REFUSED. Two lanes pointing at one conversation both resume it, and the \
             staged guard would attribute one lane's edits to the other. '{}' stays blind \
             until the conflict is resolved.",
            name, reported, owner, name,
        );
        return ConvAdopt::Conflict { owner: owner.clone() };
    }
    update_meta(name, &[("cc_conversation_id", json!(reported))]);
    // Countable, because "how many lanes healed and how many are still blind" is
    // the question AMUX-2936 could not answer for want of a trace. `was.empty()`
    // is the discriminator that matters: it means a lane the guard could not see
    // is now visible, i.e. an absorption window just closed.
    tracing::info!(
        target: "staged_guard",
        "[conv-adopt/AMUX-2936] session={} adopted self-reported conv_id={} (previous: {}) — {}",
        name,
        reported,
        if was.is_empty() { "none" } else { &was },
        if was.is_empty() {
            "lane was BLIND to the staged guard and is now resolvable"
        } else {
            "lane moved to a new conversation"
        },
    );
    ConvAdopt::Adopted { was }
}

fn tracked_files_mutate(name: &str, method: &Method, body: &Value) -> Response {
    let mut meta = load_meta(name);
    let mut tracked: Vec<String> = meta
        .get("tracked_files")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let files: Vec<String> = match body.get("files") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => vec![],
    };
    if *method == Method::POST {
        let conv_id = body_str(body, "conversation_id").trim().to_string();
        let conv_re = cached_re!(r"^[0-9a-fA-F-]{8,64}$");
        if !conv_id.is_empty() && conv_re.is_match(&conv_id) {
            let owner = conversation_owned_by_other(&conv_id, name);
            if owner.is_empty() {
                meta.insert("cc_conversation_id".into(), json!(conv_id));
            }
            // Owned by another session: refuse to adopt (cross-link guard,
            // py:75437) — silently, matching Python (it only logs).
        }
        let cwd = body_str(body, "cwd").trim().to_string();
        if !cwd.is_empty() && cwd.starts_with('/') {
            meta.insert("cc_cwd".into(), json!(cwd));
        }
        for fp in files {
            if !fp.is_empty() && !tracked.contains(&fp) {
                tracked.push(fp);
            }
        }
    } else {
        let remove: std::collections::BTreeSet<&String> = files.iter().collect();
        tracked.retain(|f| !remove.contains(f));
    }
    meta.insert("tracked_files".into(), json!(tracked));
    save_meta(name, &meta);
    j200(json!({"ok": true, "files": tracked}))
}

// ---------------------------------------------------------------------------
// steer POST/DELETE (py:75463-75533).
// ---------------------------------------------------------------------------

/// Was this queue row enqueued by AMUX ITSELF (board-drive, a schedule,
/// commit-nudge, auto-compact, status-request…) rather than by a human?
///
/// Ethan 2026-08-11, on a scheduler board-push sitting in tubescience's
/// user-facing queue as "1 queued": "board pushes should be system level not
/// queues". The delivery mechanism is the same (turn-boundary steering); the
/// SURFACE must not be — a system push counted with human messages reads as
/// something a person queued, and "Clear all" could silently discard the
/// fleet's own drive prompts alongside a stray draft.
///
/// The discriminator is the guard every system enqueuer already stamps.
/// Human paths enqueue with guard "" — or "selector-answer", which is human
/// INTENT (their answer to a picker) wearing a dedupe guard. The SQL
/// predicate in the clear-all below must stay the mirror of this.
pub(crate) fn steer_guard_is_system(guard: &str) -> bool {
    !guard.is_empty() && guard != "selector-answer"
}

async fn steer_mutate(
    state: &AppState,
    name: &str,
    method: &Method,
    headers: &HeaderMap,
    body: &Value,
) -> Response {
    if *method == Method::DELETE {
        let msg_id = body_str(body, "id");
        let sent = body.get("sent").map(py_truthy).unwrap_or(false);
        let include_system = body.get("include_system").map(py_truthy).unwrap_or(false);
        let session = name.to_string();
        let id2 = msg_id.clone();
        let reply = state
            .store
            .write_async(move |conn| {
                ensure_fleet_tables(conn)?;
                let mut sent_row: Option<(String, f64)> = None;
                let removed: i64;
                if !id2.is_empty() {
                    sent_row = conn
                        .query_row(
                            "SELECT text, queued_at FROM steering_queue WHERE id=? AND session=?",
                            rusqlite::params![id2, session],
                            |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)),
                        )
                        .ok();
                    removed = conn.execute("DELETE FROM steering_queue WHERE id=?", [&id2])? as i64;
                } else if include_system {
                    removed = conn.execute("DELETE FROM steering_queue WHERE session=?", [&session])? as i64;
                } else {
                    // Clear-all spares SYSTEM rows (board pushes, schedules —
                    // mirror of steer_guard_is_system): a human clearing their
                    // queue is discarding THEIR drafts, not amux's drive
                    // prompts. Per-row ✕ still removes anything by id, and
                    // include_system:true asks for the full sweep explicitly.
                    removed = conn.execute(
                        "DELETE FROM steering_queue WHERE session=? \
                         AND (COALESCE(guard,'')='' OR guard='selector-answer')",
                        [&session],
                    )? as i64;
                }
                if let Some((text, queued_at)) = sent_row.filter(|_| sent) {
                    let hid = id2.clone();
                    conn.execute(
                        "INSERT OR REPLACE INTO steering_history(id, session, text, queued_at, delivered_at) VALUES(?,?,?,?,?)",
                        rusqlite::params![hid, session, redact_secrets(&text), queued_at, now_f64()],
                    )?;
                }
                // Smuggle the count through WriteReply.applied? No — recompute
                // is racy; return via a rev-free outcome and count separately.
                Ok(crate::db::WriteOutcome { applied: removed > 0, events: vec![] })
            })
            .await;
        return match reply {
            Ok(r) => j200(json!({"ok": true, "cleared": if r.applied { 1 } else { 0 }})),
            Err(e) => jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
        };
    }
    if *method == Method::POST {
        let mut text = body_str(body, "text");
        if text.is_empty() {
            return jresp(StatusCode::BAD_REQUEST, json!({"error": "missing 'text'"}));
        }
        let client_id: String = body_str(body, "msg_id").trim().chars().take(64).collect();
        if !client_id.is_empty() && send_dedup_seen(state, name, &format!("steer:{client_id}")).await {
            return j200(json!({"ok": true, "deduped": true, "message": "duplicate retry ignored (already queued)"}));
        }
        // Strip [no-board] before ENQUEUE (AC-183): decide, then strip.
        let _skip_board = body.get("no_board").map(py_truthy).unwrap_or(false) || no_board_re().is_match(&text);
        if no_board_re().is_match(&text) {
            text = no_board_re().replace(&text, "").trim().to_string();
            if text.is_empty() {
                return jresp(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "message is empty after removing [no-board]"}),
                );
            }
        }
        // THE POINT OF SEND is where the sender's belief is formed, so it is the
        // only place a correction is free (AMUX-2785). This handler used to
        // answer "queued for next turn boundary" unconditionally — for a lane
        // that was merely busy (true) and for one that was stopped or not a
        // worker at all (false, and false for as long as anyone cared to wait:
        // 15 hours, in the incident this card was cut from).
        let blocked = lane_block_reason(name).await;
        // ARCHIVED IS REFUSED, NOT QUEUED (AMUX-2796). The other blocked
        // reasons are temporary — a stopped lane is routinely started minutes
        // later, so storing the message and telling the truth about it beats
        // dropping it. `archived` is not temporary: nothing wakes an archived
        // lane, un-archiving is a human's call (ethos rule 8), and the row
        // becomes immortal. Two were sitting ~16h old when this was found, each
        // regenerating stall warnings, autofix cards and `steering.stalled`
        // events that could never clear.
        //
        // `auto_deliver` has always refused archived lanes; this path queued
        // them. Two spellings of one rule, and the queue got the wrong one.
        if blocked == Some("archived") {
            return jresp(
                StatusCode::CONFLICT,
                json!({
                    "ok": false,
                    "error": block_reason_explain("archived", name),
                    "blocked_reason": "archived",
                    "deliverable": false,
                    "hint": "Un-archive the worker first if this message should reach it — \
                             that is a human's call, not something a send should do implicitly. \
                             Nothing was queued, so nothing will sit undelivered.",
                }),
            );
        }
        // IS THIS A PICKER ANSWER? Decide NOW, while the picker is still on
        // screen — intent is only knowable at the moment it existed (AMUX-2823).
        // The `selector-answer` guard also dedupes: at most one pending menu
        // answer per lane, which is the right cardinality for a keypress.
        let picker_answer = {
            let pane = tmux_capture(name, 30).await;
            answers_visible_picker(&text, &pane)
        };
        let guard = if picker_answer { "selector-answer" } else { "" };
        let msg_id = steer_enqueue(state, name, &text, guard, &hdr_worker(headers)).await;
        if body.get("record_history").map(py_truthy).unwrap_or(false) {
            let email = headers.get("x-amux-user-email").and_then(|v| v.to_str().ok()).unwrap_or("");
            // QUEUED, not direct (AF-159). `steer_enqueue` above put this on the
            // steering queue; nothing has been submitted to the lane. Recording
            // it as `direct` with a blank verdict is what made 144 of 479
            // `type='user'` rows over 7 days unreadable — blank is the CORRECT
            // value for a queued message and a hole for a direct one, and one
            // label made it mean both.
            cmd_hist_record_full(
                state, name, &text, "user", email, false,
                DeliveryMeta::queued(now_i64() * 1000),
            )
            .await;
            // Autotask/labelling: Python's model-call feature — gap named in
            // the module doc.
        }
        return j200(json!({
            "ok": true,
            "id": msg_id,
            // `ok` stays true and the row is still stored: a stopped lane is
            // routinely started minutes later, and dropping the message would
            // trade a false promise for real data loss. What changes is that the
            // response stops CLAIMING a boundary is coming.
            "deliverable": blocked.is_none(),
            "blocked_reason": blocked,
            "message": match blocked {
                None => format!("queued — delivers to '{name}' at its next turn boundary"),
                Some(r) => block_reason_explain(r, name),
            },
        }));
    }
    jresp(StatusCode::METHOD_NOT_ALLOWED, json!({"error": "method not allowed"}))
}

use super::py_truthy;

// ---------------------------------------------------------------------------
// POST verbs (py:75534-76326).
// ---------------------------------------------------------------------------

async fn post_dispatch(
    state: &AppState,
    name: &str,
    action: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Response {
    match action {
        "transcripts" => match backup_session_jsonl(name, "manual") {
            Some(path) => j200(json!({"ok": true, "path": path})),
            None => j200(json!({"ok": false, "message": "nothing to backup"})),
        },
        "send" => send_post(state, name, headers, body).await,
        "instructions" => {
            let mut saved = false;
            if let Some(instr) = body.get("instructions") {
                let v = instr.as_str().unwrap_or("").trim().to_string();
                update_meta(name, &[("instructions", json!(v))]);
                saved = true;
            }
            let mut applied = false;
            if body.get("apply").map(py_truthy).unwrap_or(false) {
                let instr = meta_str(&load_meta(name), "instructions").trim().to_string();
                if !instr.is_empty() {
                    if is_running(name).await {
                        let _ = send_text(state, name, &instr, false).await;
                    } else {
                        let st2 = state.clone();
                        let n = name.to_string();
                        tokio::spawn(async move { send_after_ready(st2, n, instr, 60).await });
                    }
                    applied = true;
                }
            }
            j200(json!({
                "ok": true,
                "instructions": meta_str(&load_meta(name), "instructions").trim(),
                "saved": saved,
                "applied": applied,
            }))
        }
        "keys" => {
            let keys = body_str(body, "keys");
            if keys.is_empty() {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "missing 'keys'"}));
            }
            let (ok, msg) = send_keys_op(name, &keys).await;
            let code = if ok {
                update_meta(name, &[("last_send", json!(now_i64()))]);
                StatusCode::OK
            } else if msg == "not running" {
                StatusCode::CONFLICT
            } else if msg.contains("not in allowed set") {
                // 400 so the offline queue drops it instead of retrying
                // forever (py:75700, 2026-07-18).
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            jresp(code, json!({"ok": ok, "message": msg}))
        }
        "resize" => {
            let cols = body.get("cols").and_then(|v| v.as_i64());
            let rows = body.get("rows").and_then(|v| v.as_i64()).unwrap_or(50);
            let Some(cols) = cols else {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "cols/rows must be integers"}));
            };
            if cols == 0 {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "cols required"}));
            }
            if !is_running(name).await {
                return jresp(StatusCode::CONFLICT, json!({"ok": false, "message": "not running"}));
            }
            let (ok, msg) = resize_pane(name, cols, rows).await;
            j200(json!({"ok": true, "resized": ok, "message": msg}))
        }
        // "agent-nav" DELETED (ARE-7): it drove Claude Code's subagents panel
        // by pane-verified keystrokes, keyed on a "⏺ main" row the TUI no
        // longer renders — 0 of 50 sessions matched, so the verb was wired
        // end-to-end and reached nobody. The durable replacement is
        // GET .../subagents (AMUX-2635). Resurrection: git log -S agent_nav.
        "memory" => {
            let content = body_str(body, "content");
            let mf = mem_file(name);
            let _ = std::fs::create_dir_all(memory_dir());
            if std::fs::write(&mf, content).is_err() {
                return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write memory file"}));
            }
            let wd = session_work_dir(name);
            if !wd.is_empty() {
                write_claude_memory(name, &wd);
            }
            j200(json!({"ok": true}))
        }
        "git" => {
            let branch = body_str(body, "branch").trim().to_string();
            let create = body.get("create").map(py_truthy).unwrap_or(false)
                || (body.get("worktree").map(py_truthy).unwrap_or(false)
                    && body.get("create").map(py_truthy).unwrap_or(false));
            let wd = session_work_dir(name);
            if wd.is_empty() {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "session has no directory"}));
            }
            if branch.is_empty() {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "branch name required"}));
            }
            let re = cached_re!(r"^[a-zA-Z0-9_./@\-]+$");
            if !re.is_match(&branch) {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "invalid branch name"}));
            }
            let mut args: Vec<&str> = vec!["-C", &wd, "checkout"];
            if create {
                args.push("-b");
            }
            args.push(&branch);
            match run_cmd("git", &args, Duration::from_secs(10)).await {
                Some(o) if o.status.success() => j200(json!({"ok": true, "branch": branch})),
                Some(o) => {
                    let err = String::from_utf8_lossy(if o.stderr.is_empty() { &o.stdout } else { &o.stderr })
                        .trim()
                        .to_string();
                    jresp(StatusCode::BAD_REQUEST, json!({"ok": false, "error": err}))
                }
                None => jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"ok": false, "error": "git timed out"})),
            }
        }
        "git-push" => {
            if !is_running(name).await {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "session not running — start it first"}));
            }
            let cfg = parse_env(name);
            let branch = cfg.get_or("CC_BRANCH", "").to_string();
            let msg = if !branch.is_empty() && branch != "none" {
                format!(
                    "Deploy now. Your branch is `{branch}`. Run these steps:\n\
                     1. `git stash` (if needed to allow checkout)\n\
                     2. `git checkout {branch}` and `git stash pop` (if stashed)\n\
                     3. IMPORTANT: Only stage files YOU changed in this session — do NOT use `git add -A`. Use `git add <specific files>` for each file you modified.\n\
                     4. `git commit` with a good commit message summarizing YOUR changes only\n\
                     5. `git checkout main && git pull --ff-only origin main`\n\
                     6. `git merge {branch}` (resolve conflicts if any)\n\
                     7. `git push origin main`\n\
                     8. `git checkout {branch}` (go back to your branch)\n\
                     Do all steps now. If any step fails, fix it and continue."
                )
            } else {
                "Deploy now. You are on `main`. Run these steps:\n\
                 1. `git pull --ff-only origin main`\n\
                 2. IMPORTANT: Only stage files YOU changed in this session — do NOT use `git add -A`. Use `git add <specific files>` for each file you modified. Review `git diff` and only add files related to your task.\n\
                 3. `git commit` with a good commit message summarizing YOUR changes only\n\
                 4. `git push origin main`\n\
                 Do all steps now. If any step fails, fix it and continue."
                    .to_string()
            };
            let _ = send_text(state, name, &msg, false).await;
            j200(json!({"ok": true, "message": "deploy instructions sent to session"}))
        }
        "start" => {
            // RESPOND BEFORE THE CHOREOGRAPHY (AMUX-2557): validations
            // inline, launch in the background, instant 202.
            let cfg = parse_env(name);
            let wd0 = cfg.get_or("CC_DIR", "").trim().to_string();
            if !wd0.is_empty() && !expanduser(&wd0).is_dir() {
                return jresp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"ok": false, "message": format!("work dir missing: {wd0}")}),
                );
            }
            // A start that CANNOT succeed must say so NOW, not answer an
            // optimistic 202 and no-op in the background (AMUX-3364). The cheap
            // preconditions are knowable synchronously, so answer ok:false and
            // WARN — the refusal is now both told to the caller and greppable in
            // server-rs.log (`session_start_refused`), where before it was
            // invisible on both channels.
            if let Some(reason) = start_block_reason(name, &cfg) {
                tracing::warn!(session = %name, reason = %reason, "session_start_refused: /start cannot launch this session");
                return jresp(StatusCode::CONFLICT, json!({"ok": false, "error": reason}));
            }
            let prompt = body_str(body, "prompt").trim().to_string();
            let st2 = state.clone();
            let n = name.to_string();
            tokio::spawn(async move {
                let (ok, msg) = start_session(&st2, &n, "", false).await;
                if ok {
                    if !prompt.is_empty() {
                        // 60s, matching the auto-wake path: a session created
                        // from the modal is on its FIRST-run boot (fresh Claude
                        // Code, MCP init), the slowest case, so a 30s window was
                        // the tightest one for the very path most likely to
                        // exceed it (AMUX-3055). The loop still exits the instant
                        // the composer appears, so this only widens the ceiling.
                        send_after_ready(st2.clone(), n.clone(), prompt, 60).await;
                    }
                } else {
                    // A background failure must still be SEEN (ethos rule 4).
                    // The event stream alone was not enough (AMUX-3364): a
                    // `server-rs.log` sweep found nothing, so a start that
                    // spawned nothing read as success. WARN to the log too, so
                    // the class is greppable (`session_start_failed`) without
                    // opening the event store.
                    tracing::warn!(session = %n, reason = %chars_truncate(&msg, 200), "session_start_failed: /start spawned nothing");
                    emit_event(
                        &st2,
                        &n,
                        "session.start_failed",
                        Some(json!({"message": chars_truncate(&msg, 200)})),
                        None,
                        "api-start",
                    )
                    .await;
                }
            });
            let meta = load_meta(name);
            let resumed = !meta_str(&meta, "cc_session_name").is_empty()
                || !meta_str(&meta, "cc_conversation_id").is_empty();
            jresp(StatusCode::ACCEPTED, json!({"ok": true, "message": "starting", "resumed": resumed}))
        }
        "stop" => {
            let st2 = state.clone();
            let n = name.to_string();
            tokio::spawn(async move {
                let (ok, _msg) = stop_session(&n).await;
                if ok {
                    emit_event(&st2, &n, "session.stopped", None, None, "api-stop").await;
                    // _complete_session_board_issue is a deliberate no-op in
                    // Python (py:12727) — nothing to port.
                }
            });
            jresp(StatusCode::ACCEPTED, json!({"ok": true, "message": "stopping"}))
        }
        "clear" => {
            let ptq = pt(name);
            match tmux(&["clear-history", "-t", &ptq]).await {
                Some(_) => j200(json!({"ok": true, "message": "cleared"})),
                None => jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"ok": false, "message": "tmux clear-history timed out"})),
            }
        }
        "duplicate" => {
            let new_name = body_str(body, "new_name").trim().to_string();
            if new_name.is_empty() {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "missing new_name"}));
            }
            let re = cached_re!(r"[^a-zA-Z0-9_-]");
            let new_name = re.replace_all(&new_name, "-").into_owned();
            let new_file = env_path(&new_name);
            if new_file.exists() {
                return jresp(StatusCode::CONFLICT, json!({"error": format!("session '{new_name}' already exists")}));
            }
            if std::fs::copy(env_path(name), &new_file).is_err() {
                return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "copy failed"}));
            }
            j200(json!({"ok": true, "message": format!("duplicated as {new_name}")}))
        }
        "clone" => clone_post(state, name, body).await,
        "archive" => {
            if !session_destructive_allowed(state, headers) {
                return jresp(StatusCode::FORBIDDEN, json!({"error": "archiving a session must be initiated by a human in the dashboard; sessions/agents cannot archive sessions (set AMUX_ALLOW_AGENT_SESSION_DELETE=1 to allow automation)"}));
            }
            let cfg = parse_env(name);
            if cfg.get("CC_PINNED") == Some("1") && !is_session_blocked(name) {
                return jresp(StatusCode::FORBIDDEN, json!({"error": "cannot archive pinned session — unpin first"}));
            }
            let (ok, msg) = archive_session(state, name).await;
            verb_resp(ok, msg)
        }
        "wake" => {
            let (ok, msg) = wake_session(state, name).await;
            verb_resp(ok, msg)
        }
        "reset" => {
            let (ok, msg) = reset_session(state, name).await;
            verb_resp(ok, msg)
        }
        "commit-report" => {
            // Attach the commit to the in-flight card (py:76233-76246). The
            // cross-session sweep notice (py:76008-76230) is a named gap.
            let sha: String = body_str(body, "sha").trim().chars().take(16).collect();
            let subj: String = body_str(body, "subject").trim().chars().take(140).collect();
            if sha.is_empty() {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": "sha required"}));
            }
            let session = name.to_string();
            let sha2 = sha.clone();
            let reply = state
                .store
                .write_async(move |conn| {
                    let row: Option<String> = conn
                        .query_row(
                            "SELECT id FROM issues WHERE session=? AND deleted IS NULL \
                             AND COALESCE(archived,0)=0 AND status IN ('doing','review') \
                             AND owner_type='agent' ORDER BY updated DESC LIMIT 1",
                            [&session],
                            |r| r.get(0),
                        )
                        .ok();
                    let Some(issue_id) = row else {
                        return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
                    };
                    let log: String = conn
                        .query_row("SELECT COALESCE(log,'') FROM issues WHERE id=?", [&issue_id], |r| r.get(0))
                        .unwrap_or_default();
                    let ts = chrono::Local::now().format("%H:%M");
                    let new_log = format!("{}\n`{ts}` commit {sha2} — {subj}", log.trim_end()).trim().to_string();
                    conn.execute(
                        "UPDATE issues SET log=?, rev=COALESCE(rev,0)+1, updated=? WHERE id=?",
                        rusqlite::params![new_log, now_i64(), issue_id],
                    )?;
                    Ok(crate::db::WriteOutcome {
                        applied: true,
                        events: vec![crate::db::PendingEvent {
                            entity_type: amux_core::revision::EntityType::Other("issue".into()),
                            entity_id: issue_id,
                            mutation: amux_core::revision::MutationKind::Updated,
                            payload: None,
                        }],
                    })
                })
                .await;
            match reply {
                Ok(r) if !r.applied => j200(json!({"ok": true, "attached": Value::Null})),
                Ok(_) => {
                    // Re-read the card id for the response (the write closure
                    // cannot return it through WriteReply).
                    let attached: Option<String> = state.store.read().ok().and_then(|conn| {
                        conn.query_row(
                            "SELECT id FROM issues WHERE session=? AND deleted IS NULL \
                             AND COALESCE(archived,0)=0 AND status IN ('doing','review') \
                             AND owner_type='agent' ORDER BY updated DESC LIMIT 1",
                            [name],
                            |r| r.get(0),
                        )
                        .ok()
                    });
                    j200(json!({"ok": true, "attached": attached, "sha": sha}))
                }
                Err(e) => jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
            }
        }
        "report" => report_post(state, name, headers, body).await,
        "apply-template" => {
            let re = cached_re!(r"[^a-z0-9\-]");
            let tmpl_id = re.replace_all(&body_str(body, "template_id"), "").into_owned();
            let work_dir = body_str(body, "dir").trim().to_string();
            // Two different failures, two different messages. They shared one
            // string until 2026-08-11, and that is precisely why a dead
            // templates_dir() went unnoticed: "template not found" reads as
            // "you asked for a template that doesn't exist", so nobody
            // suspected the ROOT was missing. Name which one it is.
            let Some(tmpl_root) = templates_dir() else {
                return jresp(
                    StatusCode::NOT_FOUND,
                    json!({"error": "no templates directory on this machine",
                           "hint": "set AMUX_TEMPLATES_DIR, or install templates/ to ~/.amux/templates (install.sh does this)",
                           "tried": [std::env::var("AMUX_TEMPLATES_DIR").unwrap_or_default(),
                                     home().join("templates").display().to_string()]}),
                );
            };
            let tmpl_path = tmpl_root.join(&tmpl_id);
            if tmpl_id.is_empty() || !tmpl_path.is_dir() {
                return jresp(
                    StatusCode::NOT_FOUND,
                    json!({"error": format!("no template '{tmpl_id}'"),
                           "templates_dir": tmpl_root.display().to_string()}),
                );
            }
            if !work_dir.is_empty() {
                let work = expanduser(&work_dir);
                let _ = std::fs::create_dir_all(&work);
                if let Ok(meta_text) = std::fs::read_to_string(tmpl_path.join("template.json")) {
                    if let Ok(meta) = serde_json::from_str::<Value>(&meta_text) {
                        for d in meta["dirs"].as_array().cloned().unwrap_or_default() {
                            if let Some(d) = d.as_str() {
                                let _ = std::fs::create_dir_all(work.join(d));
                            }
                        }
                    }
                }
                let claude_file = tmpl_path.join("CLAUDE.md");
                if claude_file.exists() {
                    let dest = work.join("CLAUDE.md");
                    if !dest.exists() {
                        if let Ok(t) = std::fs::read_to_string(&claude_file) {
                            let _ = std::fs::write(&dest, t);
                        }
                    }
                }
            }
            j200(json!({"ok": true}))
        }
        "delete" => delete_post(state, name, headers).await,
        // CONVENTIONAL SPELLINGS, routed to the SAME handlers (AMUX-2669/2665).
        //
        // Both of these 404'd/405'd while a non-obvious spelling worked:
        //   rename lived only at PATCH /api/sessions/<n>/config {"rename":...}
        //   delete lived only at POST  /api/sessions/<n>/delete
        // The SPA happened to use the working one, so nothing looked broken —
        // but a script, a runbook, the CLI, or a peer session reaching for the
        // obvious verb got a dead end. That is exactly the shape that cost two
        // lost inter-session messages tonight: POST /api/workers/<n>/send 405'd
        // while /api/sessions/<n>/send answered, and `amux send` degraded to
        // raw tmux without anyone noticing.
        //
        // Aliases, not reimplementations — they call the identical functions,
        // so the two spellings cannot drift into different behaviour.
        "rename" => {
            let to = body_str(body, "name");
            let to = if to.is_empty() { body_str(body, "rename") } else { to };
            if to.is_empty() {
                return jresp(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "name is required",
                           "hint": "POST {\"name\": \"<new>\"} (alias of PATCH /config {\"rename\":...})"}),
                );
            }
            rename_session(state, name, &to).await
        }
        _ => not_found(),
    }
}

/// Where the worker templates live: `AMUX_TEMPLATES_DIR`, else
/// `~/.amux/templates` (install.sh syncs the repo's `templates/` there).
///
/// This used to carry a middle rung that canonicalized
/// `~/.local/bin/amux-server.py` and looked beside it — py:143's
/// `TEMPLATES_DIR = Path(__file__).parent / "templates"`. That symlink went
/// away with the Python server at 792ce1f, and nothing replaced the rung, so
/// ALL THREE resolved to nothing and this returned None on every call. The
/// visible effect was `apply-template` answering "template not found" for a
/// perfectly real id — indistinguishable, from the client, from a typo'd one,
/// which is why it sat unnoticed. Verified 2026-08-11 with a control: the real
/// `software-project` and a bogus `zzz-does-not-exist` returned byte-identical
/// 404s.
pub(crate) fn templates_dir() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("AMUX_TEMPLATES_DIR") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
    }
    let fallback = home().join("templates");
    if fallback.is_dir() {
        Some(fallback)
    } else {
        None
    }
}


// ---------------------------------------------------------------------------
// Group scoping for worker-to-worker sends (Ethan, 2026-08-11)
// ---------------------------------------------------------------------------

/// Whether a per-worker env value is switched ON (`1`/`true`/`yes`, quotes and
/// case ignored). The one spelling every boolean CC_* flag is read through here.
pub(crate) fn env_flag_on(v: Option<&str>) -> bool {
    matches!(
        v.map(|s| s.trim().trim_matches('"').to_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Whether a worker runs "isolated" (AMUX-3232): a RAW agent (tmux + the
/// underlying CLI) with the amux harness stripped, and undiscoverable to peers.
///
/// The flag is the per-worker env key `CC_ISOLATED`, the SAME environment
/// primitive that already holds `CC_MCP` / `CC_TAGS` / `CC_ARCHIVED`, so it
/// persists across reload and applies at the next spawn with no new store to
/// keep in step (deliberately not a second spelling in a SQLite column). This
/// function is the single source of truth every isolation decision consults:
/// spawn-env suppression, `--mcp-config`, board auto-capture, the peer fleet
/// list, the fleet roster, the peer-send guard, and the status/rate-limit sweep.
pub(crate) fn session_is_isolated(name: &str) -> bool {
    env_flag_on(parse_env(name).get("CC_ISOLATED"))
}

/// A lane's groups, from `CC_TAGS` in its env file.
pub(crate) fn lane_groups(lane: &str) -> std::collections::BTreeSet<String> {
    parse_env(lane)
        .get("CC_TAGS")
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().trim_matches('"').to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Did `them` send to `us` recently? The evidence is a cmd_history row written
/// at delivery time, so a lane cannot manufacture a reply window for itself.
fn recently_contacted_by(them: &str, us: &str) -> bool {
    let Some(home) = dirs_home() else { return false };
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        home.join("amux.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return false;
    };
    let cutoff = (now_i64() - REPLY_WINDOW_S) * 1000;
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT 1 FROM cmd_history WHERE session=?1 AND origin=?2 AND ts > ?3 LIMIT 1",
        rusqlite::params![us, them, cutoff],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// How long a lane may answer an inbound cross-group message. A working day:
/// long enough for a real exchange, short enough that it is not a standing
/// permission earned once.
const REPLY_WINDOW_S: i64 = 24 * 3600;

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("AMUX_HOME").ok().map(std::path::PathBuf::from).or_else(|| {
        std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".amux"))
    })
}

/// Why a worker-to-worker send is allowed, or `Err(reason)` if it is not.
///
/// Ethan's rule: "worker to worker communication should be limited to intra
/// group unless explicitly stated." The escapes are CONFIG, never something the
/// sending agent can assert about itself in a request body — a flag any caller
/// could set would make the rule advisory.
///
/// - same group (or a self-send)          -> allowed
/// - sender's `CC_SEND_ALLOW`             -> groups it may reach, or `*`
/// - receiver's `CC_RECEIVE_ANY=1`        -> a documented fleet-wide routing
///   target. `amux` is one by construction: the worker roster tells every lane
///   to route amux platform bugs here, which IS the explicit statement, and
///   683 of the 908 worker-to-worker sends in the 24h before this shipped were
///   cross-group — mostly bug reports inbound to this lane. Blocking those
///   would have severed the fleet's only bug channel to fix a broadcast problem.
fn cross_group_send_ok(origin: &str, target: &str) -> Result<&'static str, String> {
    if origin.is_empty() || origin == target {
        return Ok("self-or-human");
    }
    // ISOLATED TARGET (AMUX-3232): a raw agent is not a peer/relay target. The
    // OWNER (empty origin) already returned above, so this refuses ONLY a PEER
    // send; owner peek/send from the dashboard is untouched. The refusal names
    // "isolated" so a sweep of the request log / send.cross_group_refused events
    // can find it (the two-fixes rule: the exclusion is greppable, not silent).
    if session_is_isolated(target) {
        return Err(format!(
            "send refused: '{target}' is an isolated (raw-agent) worker with the amux harness \
             stripped. It is not a peer or relay target and is reachable only by the owner from \
             the dashboard. This applies only to sends carrying a worker origin; a human send is \
             never restricted."
        ));
    }
    let (og, tg) = (lane_groups(origin), lane_groups(target));
    if !og.is_disjoint(&tg) {
        return Ok("same-group");
    }
    let env_t = parse_env(target);
    if matches!(
        env_t.get("CC_RECEIVE_ANY").map(|v| v.trim().trim_matches('"').to_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes")
    ) {
        return Ok("receiver-open");
    }
    // REPLY EXEMPTION. Ethan's objection was to a lane BROADCASTING across groups
    // unsolicited — ts-gke reaching 9 lanes in 5 groups, twice. A reply is not
    // that, and blocking one makes the rule worse than no rule: within minutes
    // of shipping this gate it refused MY OWN answer to a cross-group lane that
    // had just reported a bug to me, leaving the channel one-way — reports in,
    // answers impossible.
    //
    // So: you may answer a lane that contacted you inside the window. It cannot
    // be used to initiate, only to respond, and the evidence is the durable
    // cmd_history row of THEIR send to YOU — not something the replier asserts.
    if recently_contacted_by(target, origin) {
        return Ok("reply-to-inbound");
    }
    let allow: Vec<String> = parse_env(origin)
        .get("CC_SEND_ALLOW")
        .map(|v| v.split(',').map(|t| t.trim().trim_matches('"').to_lowercase()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default();
    if allow.iter().any(|a| a == "*") || allow.iter().any(|a| tg.contains(a)) {
        return Ok("sender-allowlist");
    }
    let fmt = |g: &std::collections::BTreeSet<String>| {
        if g.is_empty() { "(untagged)".to_string() } else { g.iter().cloned().collect::<Vec<_>>().join(",") }
    };
    Err(format!(
        "cross-group send refused: {origin} [{}] -> {target} [{}]. Worker-to-worker \
         messaging is intra-group unless explicitly configured. To allow it: set \
         CC_SEND_ALLOW on {origin} (comma-separated groups, or *), or CC_RECEIVE_ANY=1 \
         on {target} if it is a fleet-wide routing target. A human send is never \
         restricted — this applies only to sends carrying a worker origin. For a \
         cross-group HANDOFF, use the board on a card owned by {target}: \
         `amux board progress <CARD> --stdin` notifies the owner at their next turn, \
         and `amux board ask <CARD>` requests a status update from them (AVE-36: a \
         bare desc PATCH records without notifying).",
        fmt(&og), fmt(&tg)
    ))
}

/// The message id a send answers with (Ethan 2026-08-11: "we should have all
/// messages with an idempotent id"). DERIVED from the caller's msg_id when
/// one is supplied — not stored — so the original send and every deduped
/// retry of it answer the SAME id with no lookup and no row to expire.
/// Without a msg_id there is nothing to be idempotent against; the
/// timestamp form at least remains unique.
fn send_response_id(name: &str, msg_id: &str) -> String {
    if msg_id.is_empty() {
        format!("msg-{}-{}", name, (now_f64() * 1000.0) as i64)
    } else {
        format!("msg-{name}-{msg_id}")
    }
}

async fn send_post(state: &AppState, name: &str, headers: &HeaderMap, body: &Value) -> Response {
    // GROUP SCOPING, before anything is delivered or recorded. The origin is the
    // SERVER-VERIFIED stamp (AMUX-1768), never a body-supplied claim, so a lane
    // cannot talk its way across a group boundary.
    let send_origin: String = hdr_worker(headers).trim().chars().take(64).collect();
    if std::env::var("AMUX_GROUP_SEND_ENFORCE")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no"))
        .unwrap_or(true)
    {
        if let Err(reason) = cross_group_send_ok(&send_origin, name) {
            // LOUD AND QUERYABLE, per CLAUDE.md's two-fixes rule: a refusal that
            // leaves no trace is a rule nobody can audit or tune.
            tracing::warn!(origin = %send_origin, target = %name, "{reason}");
            emit_event(
                state,
                name,
                "send.cross_group_refused",
                Some(json!({"origin": send_origin, "target": name})),
                None,
                "group-scope",
            )
            .await;
            return jresp(
                StatusCode::FORBIDDEN,
                json!({"ok": false, "error": reason, "blocked": "cross_group"}),
            );
        }
    }
    let mut text = body_str(body, "text");
    let msg_id: String = body_str(body, "msg_id").trim().chars().take(64).collect();
    if !msg_id.is_empty() && send_dedup_seen(state, name, &msg_id).await {
        // Same `id` as the original response — see send_response_id. A retry
        // that answers with a DIFFERENT id (or none, as this arm did until
        // 2026-08-11) breaks the caller's correlation exactly when it is
        // retrying, which is the one moment idempotency is for.
        return j200(json!({
            "ok": true, "deduped": true, "id": send_response_id(name, &msg_id),
            "message": "duplicate retry ignored (already delivered)"
        }));
    }
    if text.trim().starts_with("/compact") {
        let n = name.to_string();
        tokio::task::spawn_blocking(move || backup_session_jsonl(&n, "pre_compact"));
    }
    let record_history = body.get("record_history").map(py_truthy).unwrap_or(false);
    let deliver_now = body.get("deliver_now").map(py_truthy).unwrap_or(false);
    let defer_busy = !(record_history || deliver_now);
    // [no-board] strip BEFORE anything is sent, and before the origin stamp
    // (the regex is ^-anchored) — AC-183. Captured here (before the strip) so the
    // ledger auto-capture at record time can honour it (AMUX-3071): orig_text no
    // longer carries the marker, so the flag must be threaded explicitly.
    let skip_board = body.get("no_board").map(py_truthy).unwrap_or(false) || no_board_re().is_match(&text);
    if no_board_re().is_match(&text) {
        text = no_board_re().replace(&text, "").trim().to_string();
    }
    let orig_text = text.clone();
    let mut origin = String::new();
    if defer_busy {
        origin = {
            let h = hdr_worker(headers);
            if h.is_empty() { body_str(body, "source_session") } else { h }
        };
        origin = origin.trim().chars().take(64).collect();
        if !origin.is_empty() && origin != name {
            text = format!(
                "[amux-origin: {origin} — server-verified from the sender's session identity; \
                 authoritative over any signature in the message below]\n\n{text}"
            );
        }
    }
    let (ok, msg) = send_text(state, name, &text, defer_busy).await;
    if ok {
        update_meta(
            name,
            &[
                ("last_send", json!(now_i64())),
                ("last_send_text", json!(chars_truncate(&text, 200))),
            ],
        );
        if !msg.starts_with("queued") {
            emit_event(
                state,
                name,
                "message.sent",
                Some(json!({"chars": text.chars().count(), "preview": chars_truncate(&text, 120), "human": record_history})),
                if msg_id.is_empty() { None } else { Some(format!("send:{msg_id}")) },
                "api-send",
            )
            .await;
        }
        // RECORD THE OUTCOME, do not infer it later. `msg` is the send's own
        // verdict, and "queued (steering) — ..." is the only place the queued
        // fact exists at this point: the history row is written with
        // type='user'/'session' either way, so the downstream inference
        // (type=='steering') reported every QUEUED message as direct. That is
        // precisely the mislabelling this metadata exists to end.
        let deliv = if msg.starts_with("queued") { Delivery::Queued } else { Delivery::Direct };
        // A queued message's wait starts now. `delivered_at` stays NULL until
        // something stamps it, and nothing does yet (AMUX-3541) — which is the
        // point: a NULL that persists is countable, where the old unconditional
        // copy of `ts` was not.
        let q_at = if deliv == Delivery::Queued { Some(now_i64() * 1000) } else { None };
        // Same source of truth as `deliv`: the send's own outcome, recorded
        // rather than re-inferred later. `verify_submitted` already computed
        // this and `send_outcome` already said it — until now the response was
        // the only place it existed, read once by the caller and then gone.
        let meta = DeliveryMeta {
            delivery: Some(deliv),
            queued_at_ms: q_at,
            submit_verdict: submit_verdict_of(&msg),
        };
        if record_history {
            let email = headers.get("x-amux-user-email").and_then(|v| v.to_str().ok()).unwrap_or("");
            cmd_hist_record_full(state, name, &orig_text, "user", email, skip_board, meta).await;
        } else if !origin.is_empty() && origin != name {
            cmd_hist_record_full(state, name, &orig_text, "session", &origin, false, meta).await;
        }
    } else if !msg_id.is_empty() {
        send_dedup_forget(state, name, &msg_id).await;
    }
    // A REFUSAL IS NOT A SERVER ERROR (AMUX-2681). This used to be
    // `if msg == "not running" { 409 } else { 500 }`, so every other honest
    // decline — resume picker, selector, mid-turn, archived, blocked, and the
    // background-conversation view — shipped as 500. `fix` is the next step,
    // so the SPA can render an action instead of a stack-trace shape.
    let (code, fix) = if ok {
        (StatusCode::OK, None)
    } else {
        send_failure_status(&msg)
    };
    let send_id = send_response_id(name, &msg_id);
    let mut resp = json!({"ok": ok, "message": msg, "id": send_id});
    if let Some(fix) = fix {
        resp["fix"] = json!(fix);
    }
    if ok && msg.contains("at a selector") {
        resp["held_at_selector"] = json!(true);
    }
    // WHAT "sent" MEANS, stated in the payload (AMUX-2629). Before this, a
    // caller had one bit — `ok` — covering four different outcomes, and the
    // one that mattered ("the keys landed but Claude Code never took the
    // message") was indistinguishable from success. It is now explicit:
    //   submitted=true   — read back from the composer / the conversation JSONL
    //   submitted=false  — the text is STILL in the input box; NOT delivered
    //   submitted=null   — queued/deferred, or the composer could not be read;
    //                      `submission` names which
    // `ok` keeps its old meaning (did the request do its job) so existing
    // callers do not silently change behaviour; `submitted` is the new,
    // narrower claim. Never widen `ok` to mean "submitted" — a deferred
    // message is a legitimate success that has not been submitted yet.
    //
    // Derived by `submission_verdict`, which the SCHEDULER also reads to decide
    // what a run row may claim — one function, so an audit row and an HTTP
    // response can never disagree about whether the same send was delivered.
    let (submitted, submission) = submission_verdict(ok, &msg);
    resp["submitted"] = submitted.map(Value::from).unwrap_or(Value::Null);
    resp["submission"] = json!(submission);
    // A retry means the FIRST Enter was dropped. Reported even on success:
    // smoothing it into a plain "sent" is how a degrading delivery path stays
    // invisible until it drops a message for ten minutes (AMUX-2629).
    if msg.contains("on retry") {
        resp["retried"] = json!(true);
    }
    // Python additionally reports recipient_gated from its in-memory
    // credit-gate state (_session_auto_actions) — process state this origin
    // does not hold; named gap.
    jresp(code, resp)
}

async fn clone_post(state: &AppState, name: &str, body: &Value) -> Response {
    let new_name = body_str(body, "new_name").trim().to_string();
    if new_name.is_empty() {
        return jresp(StatusCode::BAD_REQUEST, json!({"error": "missing new_name"}));
    }
    let re = cached_re!(r"[^a-zA-Z0-9_-]");
    let new_name = re.replace_all(&new_name, "-").into_owned();
    let new_file = env_path(&new_name);
    if new_file.exists() {
        return jresp(StatusCode::CONFLICT, json!({"error": format!("session '{new_name}' already exists")}));
    }
    if std::fs::copy(env_path(name), &new_file).is_err() {
        return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "copy failed"}));
    }
    let source_meta = load_meta(name);
    let session_id = {
        let sid = meta_str(&source_meta, "cc_conversation_id");
        if !sid.is_empty() {
            sid
        } else {
            // py:20480 _find_latest_session_id — newest jsonl with real turns.
            // Guarded like restart_for_swap: on a shared work dir the newest
            // jsonl can be a NEIGHBOUR lane's live conversation; forking it
            // would clone another session's brain (2026-08-09 cross-link).
            let cfg = parse_env(name);
            let wd = work_dir_of(&cfg);
            let latest = find_latest_session_id(&wd);
            if !latest.is_empty() && conversation_owned_by_other(&latest, name).is_empty() {
                latest
            } else {
                String::new()
            }
        }
    };
    let (ok, msg, method_used) = if !session_id.is_empty() {
        let (ok, msg) =
            start_session(state, &new_name, &format!("--resume {session_id} --fork-session"), true).await;
        (ok, msg, "resume")
    } else {
        let (ok, msg) = start_session(state, &new_name, "", false).await;
        (ok, msg, "scrollback")
    };
    if !ok {
        return jresp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"ok": false, "message": format!("cloned config but failed to start: {msg}")}),
        );
    }
    if method_used == "scrollback" && is_running(name).await {
        sleep_ms(5000).await;
        let ptq = pt(name);
        let mut scrollback = String::new();
        if let Some(o) = run_cmd("tmux", &["capture-pane", "-t", &ptq, "-p", "-S", "-3000"], CAPTURE_TIMEOUT).await {
            let raw = String::from_utf8_lossy(&o.stdout).into_owned();
            let cleaned = strip_ansi(&raw);
            let mut lines: Vec<&str> = cleaned.lines().collect();
            while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.remove(0);
            }
            while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.pop();
            }
            scrollback = lines.join("\n");
        }
        if !scrollback.is_empty() {
            if scrollback.chars().count() > 50000 {
                let chars: Vec<char> = scrollback.chars().collect();
                scrollback = chars[chars.len() - 50000..].iter().collect();
            }
            let prompt = format!(
                "This session was cloned from '{name}'. Below is the recent terminal output \
                 from that session. Please continue the work from where it left off.\n\n```\n{scrollback}\n```"
            );
            let _ = send_literal(&new_name, &prompt).await;
            sleep_ms(1000).await;
            send_key(&new_name, "Enter").await;
        }
    }
    j200(json!({"ok": true, "message": format!("cloned as {new_name} (method: {method_used})"), "started": ok}))
}

fn find_latest_session_id(work_dir: &str) -> String {
    if work_dir.is_empty() {
        return String::new();
    }
    let project_dir = claude_home().join("projects").join(project_name(work_dir));
    let Ok(rd) = std::fs::read_dir(&project_dir) else { return String::new() };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|p| p.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, p)))
        .collect();
    files.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (_, f) in files {
        for entry in iter_jsonl_tail(&f, u64::MAX) {
            if matches!(entry["type"].as_str(), Some("user") | Some("assistant")) {
                return f.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            }
        }
    }
    String::new()
}

async fn delete_post(state: &AppState, name: &str, headers: &HeaderMap) -> Response {
    if !session_destructive_allowed(state, headers) {
        return jresp(StatusCode::FORBIDDEN, json!({"error": "deleting a session must be initiated by a human in the dashboard; sessions/agents cannot delete sessions (set AMUX_ALLOW_AGENT_SESSION_DELETE=1 in ~/.amux/server.env to allow automation)"}));
    }
    let cfg = parse_env(name);
    if cfg.get("CC_PINNED") == Some("1") && !is_session_blocked(name) {
        return jresp(StatusCode::FORBIDDEN, json!({"error": "cannot delete pinned session — unpin first"}));
    }
    if is_running(name).await {
        let _ = stop_session(name).await;
    }
    // Worktree cleanup (py:76300).
    if cfg.get("CC_WORKTREE") == Some("1") {
        let wt_repo = cfg.get_or("CC_WORKTREE_REPO", "").to_string();
        let wt_dir = cfg.get_or("CC_DIR", "").to_string();
        if !wt_repo.is_empty() && !wt_dir.is_empty() {
            let _ = run_cmd(
                "git",
                &["-C", &wt_repo, "worktree", "remove", "--force", &wt_dir],
                Duration::from_secs(15),
            )
            .await;
        }
    }
    // Python leaves the tmux session alive after stop (shell only); the env
    // removal below unregisters it from the fleet. Kill it too so a deleted
    // probe leaves no tmux corpse — Python's delete relies on the archived
    // reaper for that, which this origin does not run.
    kill_tmux_session(name).await;
    let _ = std::fs::remove_file(env_path(name));
    let _ = std::fs::remove_file(mem_file(name));
    let _ = std::fs::remove_file(meta_path(name));
    let _ = std::fs::remove_file(log_path(name));
    // The registry just shrank; drop the cached fleet list so no reader is
    // served a ghost of this worker for the next TTL (AMUX-2960 — the same
    // hole create_session_legacy had on the grow side).
    crate::api::sessions_legacy::invalidate_sessions_cache();
    // DB-side per-session state (Python clears in-memory maps; the durable
    // equivalents here are the steering queue rows).
    let n = name.to_string();
    let _ = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            conn.execute("DELETE FROM steering_queue WHERE session=?", [&n])?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    j200(json!({"ok": true, "message": "deleted"}))
}

/// The model's context window, in tokens. `AMUX_CONTEXT_WINDOW`.
///
/// A knob rather than a constant because it is the one number here amux cannot
/// observe: the harness reports tokens USED and never the ceiling. The default
/// is set from measurement, not a guess — this session's own transcript carried
/// 817,201 tokens, so the window is at least that, and 1M is the smallest round
/// figure consistent with it. Wrong in the LOW direction only costs an early
/// compact; wrong in the HIGH direction means the lane hits the wall, which is
/// the failure this whole card exists to stop, so the default errs low.
pub(crate) fn context_window() -> u64 {
    std::env::var("AMUX_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(1_000_000)
}

/// Tokens out of a report payload, accepting both shapes seen in the wild: a
/// bare integer (what the hook sends) and `{input,output,total}` (what the
/// usage path writes). Returns None rather than 0 for "not reported" — a lane
/// with no token data must not read as an empty context, which would look like
/// 100% remaining and silently disable compaction for it.
pub(crate) fn tokens_used(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(o) = v.as_object() {
        if let Some(t) = o.get("total").and_then(Value::as_u64) {
            return Some(t);
        }
        let i = o.get("input").and_then(Value::as_u64).unwrap_or(0);
        let out = o.get("output").and_then(Value::as_u64).unwrap_or(0);
        if i + out > 0 {
            return Some(i + out);
        }
    }
    None
}

/// Percent of the context window still free, clamped to 0..=100.
pub(crate) fn context_pct_remaining(used: u64, window: u64) -> u8 {
    if window == 0 {
        return 100;
    }
    let used_pct = (used.saturating_mul(100) / window).min(100);
    (100 - used_pct) as u8
}

/// POST report (py:76238-76265) — the D1 report endpoint: harness-reported
/// state into the SHARED prefs store Python reads at boot and
/// sessions_legacy reads live.
/// AMUX-3048: apply one subagent lifecycle event to the lane's live count.
///
/// Reuses the `session_reports` prefs store the main self-report already uses,
/// under a `subagents` sub-key `{count, ts}` — no new table, and the consumer
/// (`FleetSignals::subagents_working`) reads it straight from the `reports`
/// Value it already loads at boot. `start` increments, `stop` decrements with a
/// floor of 0 so a lost start cannot drive the count negative. The write touches
/// ONLY the `subagents` sub-key, preserving state/model/tokens — a subagent
/// starting or stopping says nothing about the main turn's state.
async fn subagent_event_post(state: &AppState, name: &str, ev: &str) -> Response {
    let delta: i64 = match ev {
        "start" => 1,
        "stop" | "done" => -1,
        other => {
            return jresp(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("subagent must be 'start' or 'stop' (got '{other}')")}),
            );
        }
    };
    let name_s = name.to_string();
    let ev_s = ev.to_string();
    let reply = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            let mut reports: Value = conn
                .query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({}));
            let prev = reports[&name_s]["subagents"]["count"].as_i64().unwrap_or(0);
            let next = (prev + delta).max(0);
            if !reports[&name_s].is_object() {
                reports[&name_s] = json!({});
            }
            reports[&name_s]["subagents"] = json!({"count": next, "ts": now_f64()});
            conn.execute(
                "INSERT INTO prefs(key, value) VALUES('session_reports', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value=?1",
                [reports.to_string()],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match reply {
        Ok(_) => j200(json!({"ok": true, "session": name, "subagent": ev_s})),
        Err(e) => jresp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        ),
    }
}

async fn report_post(state: &AppState, name: &str, headers: &HeaderMap, body: &Value) -> Response {
    // ATTRIBUTION (AMUX-2646). A self-report is the one write in amux that is
    // ONLY ever legitimate from inside the session it describes: the hooks
    // that produce it run in that process and post to
    // `/api/sessions/$AMUX_SESSION/report`. Everything downstream then treats
    // it as ground truth — the steering gate, the card, the board's `stale`.
    //
    // It was accepting any state, for any session, from any caller, with no
    // record of who wrote it. A hand-run hook test posted
    // `{"state":"idle","source":"stop-hook-test"}` onto a LIVE working lane
    // and it stuck for 1076s, and the store could not say where it came from:
    // `source` is a free string the CALLER chooses, so it is a label, not
    // provenance.
    //
    // So: the server-verified `X-Amux-Session` stamp (AMUX-1768, the same
    // mechanism `amux send` uses) must name this session or nothing. A
    // mismatch is refused; a stamped write records its origin.
    //
    // Why this and not "reject sources that look synthetic": a `*-test`
    // suffix check is a string match on a field the caller picks, so the
    // identical write named `stop-hook` sails through — a check that cannot
    // fail on a determined caller, which is the failure mode this repo keeps
    // paying for. Test ISOLATION (a throwaway AMUX_HOME) is the other half
    // and is necessary too, but it could not have prevented THIS write: it
    // came from a hand-run curl against the live server, not from a test
    // process, so no amount of test-side sandboxing sees it.
    //
    // Residual, stated plainly: the shipped hooks send no header, so an
    // UNSTAMPED write is still accepted (rejecting it would break every lane
    // on the next hook fire). This closes cross-session writes for every
    // caller that carries the stamp — the CLI, the dashboard, and any agent
    // following the documented curl recipe — and makes the rest attributable.
    // The stronger guarantee is that no report is unfalsifiable any more:
    // `derive_status` now lets physical evidence override a stale idle, so a
    // bad report costs one contradiction window instead of a day.
    let origin: String = hdr_worker(headers).trim().chars().take(64).collect();
    if !origin.is_empty() && origin != name {
        return jresp(
            StatusCode::FORBIDDEN,
            json!({
                "error": format!(
                    "session '{origin}' may not report state for '{name}' — a self-report \
                     is only valid from inside the session it describes"
                ),
                "origin": origin,
                "target": name,
            }),
        );
    }
    // SELF-REPORTED CONVERSATION ID (AMUX-2936). Handled here, above the
    // subagent early-return, so EVERY report shape carries it — a lane that only
    // ever fires PreToolUse:Task would otherwise never heal.
    //
    // Three spellings accepted because three producers exist: `conv_id` (amux's
    // own callers), `session_id` (Claude Code's hook payload field verbatim, so
    // the hook can forward stdin without renaming anything), and the stem of
    // `transcript_path` (the same payload's other field — belt and braces for a
    // provider that sends the path but not the id).
    //
    // Adoption is deliberately NOT gated on `state`: the id is a fact about the
    // lane regardless of what it is doing, and gating it would have made healing
    // depend on which hook fired first.
    let reported_conv = {
        let direct = body_str(body, "conv_id");
        let direct = if direct.trim().is_empty() { body_str(body, "session_id") } else { direct };
        if direct.trim().is_empty() {
            let tp = body_str(body, "transcript_path");
            Path::new(tp.trim())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string()
        } else {
            direct
        }
    };
    let conv_adopt = if reported_conv.trim().is_empty() {
        ConvAdopt::Unchanged
    } else {
        adopt_reported_conv_id(name, &reported_conv)
    };

    // AMUX-3048: subagent LIFECYCLE events maintain an event-driven live count,
    // the durable exit for the mtime-window instruments cluster (AMUX-2646/2904/
    // 2959/2952/3022/3030/3047). A subagent transcript's mtime cannot tell
    // "finished 30s ago" from "thinking, will write in 90s"; a start/stop event
    // pair can. `{"subagent":"start"}` (PreToolUse:Task) increments, `"stop"`
    // (SubagentStop) decrements. Handled BEFORE the main-state parse below so it
    // never touches state/model/tokens — it is orthogonal to the main turn.
    // Attribution (the origin==name check above) applies here too: a subagent
    // count is a self-report like any other.
    let sub_ev = body_str(body, "subagent").trim().to_lowercase();
    if !sub_ev.is_empty() {
        return subagent_event_post(state, name, &sub_ev).await;
    }
    let st_raw = body_str(body, "state").trim().to_lowercase();
    let st = match st_raw.as_str() {
        "working" | "busy" => "active",
        "done" => "idle",
        "blocked" => "waiting",
        other => other,
    }
    .to_string();
    if !matches!(st.as_str(), "active" | "idle" | "waiting" | "error") {
        return jresp(
            StatusCode::BAD_REQUEST,
            json!({"error": format!("state must be one of active|idle|waiting|error (got '{st_raw}')")}),
        );
    }
    let src: String = {
        let s = body_str(body, "source");
        let s = if s.is_empty() { "hook".to_string() } else { s };
        s.chars().take(40).collect()
    };
    let detail: String = body_str(body, "detail").chars().take(200).collect();
    // AMUX-2676. The harness knows its own model and token spend; amux does
    // not, and the card's `active_model`/`tokens` have been hardcoded empty
    // fleet-wide (48/48) since the python scanner that held them in memory was
    // retired. Accepting them HERE rather than adding a scraper is D1's stated
    // exit: a better harness reports better, with no amux change.
    //
    // ABSENT != EMPTY. A heartbeat that carries no model must not wipe one a
    // previous report established, or the field would flicker on every
    // tool-hook fire. Only a present, non-empty value overwrites.
    let model_opt: Option<String> = {
        let m = body_str(body, "model");
        let m: String = m.trim().chars().take(60).collect();
        (!m.is_empty()).then_some(m)
    };
    // Captured BEFORE the writer closure moves `tokens_opt` — the compaction
    // check below runs after the write and would otherwise borrow a moved value.
    // FALL BACK TO THE TRANSCRIPT when the report body carries no tokens.
    //
    // This is the line that decided AMUX-2829. The whole auto-compact chain was
    // live and correct — pref on, compaction_action implemented, thresholds
    // tested — and it never fired once, because its ONE input came from a
    // report body that no lane sends. Measured: 292 report POSTs, 0 carrying
    // tokens; every real lane still runs the predecessor hook, whose body is
    // {state, source}, and hook config is loaded at session start so no edit on
    // disk can change that for a running process.
    //
    // Populating /api/sessions from the transcript (the sibling fix) makes the
    // DASHBOARD honest and does nothing for the trigger — the badge and the
    // decision read different inputs. That split is exactly the "producer with
    // no consumer" shape this repo keeps finding, so the fallback belongs on
    // both or the fix is cosmetic.
    //
    // AND THE REPORTED VALUE IS SANITY-CHECKED AGAINST THE WINDOW, because a
    // wrong token count here does not produce a wrong badge, it produces a
    // FORCED COMPACTION of a healthy lane — a lossy, user-visible action taken
    // on bad evidence. Found while wiring this: the stored report for my own
    // session said 3,156,510 tokens while its transcript said 217,359. Whatever
    // produced the first, a "current context" larger than the whole window is
    // not a context size, and feeding it to context_pct_remaining yields 0%
    // remaining and ForceCompact. So an over-window report is treated as
    // UNKNOWN and the transcript answers instead; if neither is plausible,
    // nothing fires, which is the correct default for a destructive action.
    let used_tokens: Option<u64> = body
        .get("tokens")
        .and_then(tokens_used)
        .filter(|t| *t <= context_window())
        .or_else(|| transcript_evidence(name).1)
        .filter(|t| *t <= context_window());
    let tokens_opt: Option<Value> = body.get("tokens").and_then(|t| {
        let get = |k: &str| t.get(k).and_then(Value::as_i64).unwrap_or(0).max(0);
        let (i, o) = (get("input"), get("output"));
        let total = match t.get("total").and_then(Value::as_i64) {
            Some(v) if v > 0 => v,
            _ => i + o,
        };
        // All-zero is what an uninstrumented caller sends; recording it would
        // replace "not reported" with a confident zero.
        (total > 0).then(|| json!({"input": i, "output": o, "total": total}))
    });
    let name_s = name.to_string();
    let st2 = st.clone();
    let src2 = src.clone();
    let origin2 = origin.clone();
    let reply = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            let mut reports: Value = conn
                .query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({}));
            let prev_state =
                reports[&name_s]["state"].as_str().unwrap_or("").to_string();
            // A HEARTBEAT MUST NOT RESURRECT A FINISHED TURN (AMUX-2538):
            // tool-hook only refreshes an already-active turn.
            if src2 == "tool-hook" && prev_state != "active" {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            // `origin` is the SERVER-VERIFIED writer; `source` is the label
            // the caller chose. Keeping both is the point — when a report is
            // wrong, "who wrote this" must be answerable from the store
            // itself rather than reconstructed from access logs nobody kept.
            // Carry forward anything this report did not carry (see
            // ABSENT != EMPTY above): the model does not change per tool call.
            let prev_model = reports[&name_s]["model"].clone();
            let prev_tokens = reports[&name_s]["tokens"].clone();
            // Did the STATUS actually change? Only then is an SSE push worth it.
            let status_changed = prev_state != st2;
            let new_state = st2.clone();
            reports[&name_s] = json!({
                "model": model_opt.clone().map(Value::from).unwrap_or(prev_model),
                "tokens": tokens_opt.clone().unwrap_or(prev_tokens),
                "state": st2, "detail": detail, "source": src2,
                "origin": origin2, "ts": now_f64(),
            });
            conn.execute(
                "INSERT INTO prefs(key, value) VALUES('session_reports', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value=?1",
                [reports.to_string()],
            )?;
            // SNAPPY STATUS (Ethan, 2026-08-16). A self-report is the fast, exact
            // signal for active/idle/needs-input (the D1 exit), but this write used
            // events:vec![], so a hook state change pushed NO SSE and the dashboard
            // only caught it on the next 15s scan or a poll. Emit a Session event on
            // a real status change so the SSE sessions push fires immediately; the
            // scan stays the fallback for hookless lanes.
            let events = if status_changed {
                vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Session,
                    entity_id: name_s.clone(),
                    mutation: amux_core::revision::MutationKind::StatusChanged {
                        from: prev_state,
                        to: new_state,
                    },
                    payload: None,
                }]
            } else {
                vec![]
            };
            Ok(crate::db::WriteOutcome { applied: true, events })
        })
        .await;
    match reply {
        Ok(r) if !r.applied => {
            // Heartbeat ignored — report the stored state like Python.
            let prev = state
                .store
                .read()
                .ok()
                .and_then(|conn| {
                    conn.query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .ok()
                })
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .map(|v| v[name]["state"].as_str().unwrap_or("").to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".into());
            j200(json!({
                "ok": true, "state": prev,
                "note": "heartbeat ignored — no active turn to refresh",
                "conv_id": conv_adopt.as_json(),
            }))
        }
        Ok(_) => {
            // REACTIVE STEERING DELIVERY: if the session just went idle and has
            // queued steering, deliver the oldest one NOW rather than waiting up
            // to 5s for the poll tick. The report IS the turn boundary — the
            // caller just proved it by reporting "idle". A 5s poll missed a <1s
            // idle window between turns for 2+ hours (9 messages, AMUX-2617).
            if st == "idle" {
                let st_clone = state.clone();
                let name_clone = name.to_string();
                tokio::spawn(async move {
                    steer_deliver_for_session(&st_clone, &name_clone).await;
                });
            }

            // AUTO-COMPACT (AMUX-2829). Ethan: "theres no reason amux should
            // ever stop." It stopped because this consumer did not exist.
            //
            // orchestrator/compaction.rs has held the POLICY since the rust
            // port — four tiers keyed on percent remaining — and had ZERO
            // callers outside its own tests. Nothing emitted ContextLow because
            // nothing knew any lane's context size, because the hooks reported
            // tokens:None. That half shipped earlier today; this is the other.
            //
            // Sent as STEERING rather than typed directly: the queue already
            // delivers at a turn boundary, which is the only moment /compact is
            // meaningful, and it already refuses to type at a selector. The
            // `auto-compact` guard makes it at-most-one-pending per lane — a
            // second report before the first is consumed replaces it rather
            // than stacking, which is what stops this becoming the nag that
            // got the `done` tier removed from the advance loop.
            if let Some(used) = used_tokens {
                let pct = context_pct_remaining(used, context_window());
                let action = crate::orchestrator::compaction::compaction_action(pct);
                use crate::orchestrator::compaction::CompactionAction as CA;
                if matches!(action, CA::Compact | CA::ForceCompact) {
                    let msg = format!(
                        "/compact\n\nContext is at {pct}% remaining ({used} tokens of a \
                         {} window). Compacting now keeps you working — running out is not a \
                         reason to stop. If you are mid-task, compact and continue where you \
                         left off.",
                        context_window()
                    );
                    // HOW LONG HAS IT BEEN PENDING? (AMUX-3557) The trigger
                    // re-evaluates on every state report and the condition
                    // cannot clear until the lane actually compacts, so a lane
                    // that is stuck mid-turn re-fires this forever — measured
                    // 300 times on `backend`, 78 in one hour. Every one of
                    // those logged an identical line, and the EVENT beside it
                    // is deduped into 30-minute buckets, so the ledger showed
                    // 11 where the truth was 300: a 27x under-report, which is
                    // why nobody saw it.
                    //
                    // The age of the already-pending row is the discriminator
                    // and it costs nothing: it is persistent (unlike a counter,
                    // which this process's re-exec would reset — the mistake
                    // the ethos file records against in-memory scan state), and
                    // it separates "fired once, will deliver at the next
                    // boundary" from "this lane has not reached a boundary in
                    // an hour", which are different problems with different
                    // owners.
                    let pending_age_s = {
                        let sess = name.to_string();
                        state
                            .store
                            .read()
                            .ok()
                            .and_then(|c| {
                                c.query_row(
                                    "SELECT queued_at FROM steering_queue \
                                     WHERE session=?1 AND guard='auto-compact' LIMIT 1",
                                    rusqlite::params![sess],
                                    |r| r.get::<_, f64>(0),
                                )
                                .ok()
                            })
                            .map(|q| (now_f64() - q).round() as i64)
                    };
                    steer_enqueue(state, name, &msg, "auto-compact", "").await;
                    tracing::warn!(
                        session = %name, pct, used, ?action,
                        pending_age_s = pending_age_s.unwrap_or(0),
                        already_pending = pending_age_s.is_some(),
                        "auto-compact queued — context low"
                    );
                    emit_event(
                        state,
                        name,
                        "session.auto_compact",
                        Some(json!({"pct_remaining": pct, "tokens": used, "action": format!("{action:?}")})),
                        Some(format!("compact:{name}:{}", now_i64() / 1800)),
                        "compaction",
                    )
                    .await;
                }
            }
            j200(json!({"ok": true, "state": st, "conv_id": conv_adopt.as_json()}))
        }
        Err(e) => jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// PATCH verbs: commit-guard (py:76319) + config (py:76327-76755).
// ---------------------------------------------------------------------------

async fn patch_dispatch(state: &AppState, name: &str, action: &str, body: &Value) -> Response {
    match action {
        "commit-guard" => {
            let global = !matches!(
                std::env::var("AMUX_COMMIT_GUARD").unwrap_or_else(|_| "1".into()).trim().to_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            );
            let raw = body.get("enabled");
            let override_v = match raw {
                None | Some(Value::Null) => None,
                Some(v) => Some(py_truthy(v)),
            };
            let f = env_path(name);
            let mut cfg = parse_env(name);
            match override_v {
                None => cfg.remove("AMUX_COMMIT_GUARD_SESSION"),
                Some(b) => cfg.set("AMUX_COMMIT_GUARD_SESSION", if b { "1" } else { "0" }),
            }
            if cfg.write(&f).is_err() {
                return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
            }
            let enabled = override_v.unwrap_or(global);
            j200(json!({
                "ok": true, "enabled": enabled, "global": global,
                "override": override_v.map(Value::Bool).unwrap_or(Value::Null),
            }))
        }
        "config" => config_patch(state, name, body).await,
        _ => not_found(),
    }
}

/// The provider/model/effort/yolo restart choreography shared by four config
/// keys (py:76470-76500 and friends): stash the live conversation id, stop
/// for restart, start.
async fn restart_for_swap(state: &AppState, name: &str, provider: &str) -> bool {
    if provider == "claude" {
        // Python captures the LIVE conv id from process argv (py:20546
        // _live_conv_id). The argv walk is not ported; the meta id is what
        // start_session resumes from, so a stale meta id falls back to a
        // fresh --name start rather than resuming a neighbour's conversation.
        let cfg = parse_env(name);
        let wd = work_dir_of(&cfg);
        if meta_str(&load_meta(name), "cc_conversation_id").is_empty() {
            let sid = find_latest_session_id(&wd);
            // Cross-link guard (2026-08-09 incident): find_latest_session_id
            // is "newest jsonl in the project dir" — on a SHARED work dir
            // (~/Dev/amux hosts amux, amux-rust, amux-frustrations, ...) that
            // is whichever NEIGHBOUR spoke last. Unguarded, this stamped the
            // amux session with amux-rust's live conversation during a model
            // swap, and the next start resumed a copy of another lane's brain
            // (conv-guard log: "already owned by 'amux'", 19:10:30). Adopt the
            // latest id only when no sibling's meta claims it — the same
            // refusal the tracked-files endpoint applies (py:75437 parity).
            if !sid.is_empty() && conversation_owned_by_other(&sid, name).is_empty() {
                update_meta(name, &[("cc_conversation_id", json!(sid))]);
            }
        }
    }
    let _ = stop_session(name).await;
    kill_tmux_session(name).await;
    let (ok, _msg) = start_session(state, name, "", false).await;
    ok
}

// ---------------------------------------------------------------------------
// Rename — a CONVERGENT cascade, not a one-shot (py:76333-76432, upgraded per
// the owner addendum on AMUX-2598: "we should have some kind of idempotency
// for stuff like that under the hood").
//
// Design, in the addendum's three axes:
// 1. IDEMPOTENT + CONVERGENT: rename-to-self is an honest no-op (nothing
//    written, store rev unmoved — Invariant 37). Every step is
//    skip-if-already-done, and a RETRY of the same rename after a partial
//    failure (old env already moved, stragglers left) is admitted by
//    dispatch's resume exception and completes the remainder.
// 2. ATOMIC WHERE POSSIBLE, JOURNALED WHERE NOT: all DB reference
//    migrations run in ONE writer transaction. The fs/tmux steps cannot
//    join it, so the rename is journaled to session_events BEFORE the first
//    step (`session.rename.started` {old,new,resuming}) and confirmed after
//    (`session.renamed` {old,new,steps}) — a crash mid-cascade is
//    diagnosable from the journal (ethos rule 4), and a step failure
//    returns 500 NAMING the steps that completed.
// 3. COLLISION + CONCURRENCY: both-envs-exist → 409 (python parity, and the
//    one state a resume cannot disambiguate); concurrent renames serialize
//    on RENAME_LOCK; every success names the canonical `name` so callers
//    re-address.
//
// Beyond Python's cascade (issues/schedules/session_gates/saved_messages +
// steering queue/history), this also migrates rows Python ORPHANS on
// rename: share_tokens (share links died), cmd_history (Messages tab
// emptied), the prefs session_reports key (self-reported status lost, so
// the lane fell back to scrape-derived status), the transcripts backup dir
// and the plain-log mirror. Deliberately NOT migrated, named here rather
// than silently inherited: session_events rows (append-only audit — history
// keeps the name it happened under; the rename journal entry links the two)
// and send_dedup rows (600s TTL, self-expiring).
// ---------------------------------------------------------------------------

static RENAME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn sanitize_session_name(raw: &str) -> String {
    let re = cached_re!(r"[^a-zA-Z0-9_-]");
    re.replace_all(raw.trim(), "-").into_owned()
}

/// Move old→new if old exists and new doesn't; report the state either way.
/// An fs error is a hard failure (the caller returns 500 naming the steps).
fn move_if(old: &Path, new: &Path, label: &str, steps: &mut Vec<String>) -> Result<(), String> {
    if old.exists() && !new.exists() {
        std::fs::rename(old, new).map_err(|e| format!("{label}: rename failed: {e}"))?;
        steps.push(format!("{label}: moved"));
    } else if new.exists() {
        steps.push(format!("{label}: already at target"));
    } else {
        steps.push(format!("{label}: nothing to move"));
    }
    Ok(())
}

async fn rename_session(state: &AppState, name: &str, raw_new: &str) -> Response {
    let new_name = sanitize_session_name(raw_new);
    if new_name.is_empty() {
        return jresp(StatusCode::BAD_REQUEST, json!({"error": "invalid name"}));
    }
    if new_name == name {
        // Honest no-op: nothing written anywhere, rev unmoved (Invariant 37).
        return j200(json!({
            "ok": true, "noop": true, "name": name,
            "message": format!("already named {name} — nothing to do"),
        }));
    }
    let _serialize = RENAME_LOCK.lock().await;
    let old_env = env_path(name);
    let new_env = env_path(&new_name);
    let resuming = !old_env.exists() && new_env.exists();
    if old_env.exists() && new_env.exists() {
        return jresp(StatusCode::CONFLICT, json!({"error": format!("'{new_name}' already exists")}));
    }
    if !old_env.exists() && !new_env.exists() {
        return jresp(StatusCode::NOT_FOUND, json!({"error": format!("session '{name}' not found")}));
    }
    let work_dir = if resuming {
        parse_env(&new_name).get_or("CC_DIR", "").to_string()
    } else {
        parse_env(name).get_or("CC_DIR", "").to_string()
    };
    // JOURNAL FIRST: a crash mid-cascade must be diagnosable from the event
    // log, not discovered as orphaned cards weeks later (ethos rule 4).
    emit_event(
        state, name, "session.rename.started",
        Some(json!({"old": name, "new": new_name, "resuming": resuming})),
        None, "config-rename",
    )
    .await;
    let mut steps: Vec<String> = Vec::new();
    let fail = |steps: &[String], err: String| {
        jresp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({
                "error": err,
                "old": name, "new": new_name.clone(),
                "steps_completed": steps,
                "retry": format!("re-run PATCH config {{\"rename\": \"{new_name}\"}} — every step is skip-if-done, the retry completes the remainder"),
            }),
        )
    };
    // 1. tmux — session-level rename, skip-if-done. Runs before the env
    //    moves so a failure here leaves the registry untouched.
    {
        let running = tmux_sessions_set().await;
        if running.contains(&tmux_name(name)) {
            let stq = st(name);
            let new_tmux = tmux_name(&new_name);
            match tmux(&["rename-session", "-t", &stq, &new_tmux]).await {
                Some(o) if o.status.success() => steps.push("tmux: renamed".into()),
                Some(o) => {
                    return fail(&steps, format!(
                        "tmux rename-session failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    ))
                }
                None => return fail(&steps, "tmux rename-session timed out".into()),
            }
        } else if running.contains(&tmux_name(&new_name)) {
            steps.push("tmux: already renamed".into());
        } else {
            steps.push("tmux: not running".into());
        }
    }
    // 2-6. Registry + per-session files, each convergent.
    if let Err(e) = move_if(&old_env, &new_env, "env", &mut steps) {
        return fail(&steps, e);
    }
    if let Err(e) = move_if(&mem_file(name), &mem_file(&new_name), "memory", &mut steps) {
        return fail(&steps, e);
    }
    if let Err(e) = move_if(&meta_path(name), &meta_path(&new_name), "meta", &mut steps) {
        return fail(&steps, e);
    }
    if let Err(e) = move_if(&log_path(name), &log_path(&new_name), "log", &mut steps) {
        return fail(&steps, e);
    }
    if let Err(e) = move_if(&plain_log_path(name), &plain_log_path(&new_name), "plain-log", &mut steps) {
        return fail(&steps, e);
    }
    if let Err(e) = move_if(
        &transcripts_dir().join(name),
        &transcripts_dir().join(&new_name),
        "transcript-backups",
        &mut steps,
    ) {
        return fail(&steps, e);
    }
    // 7. Claude project symlink repair (py:76354) — best-effort, reported.
    if !work_dir.is_empty() {
        let link = claude_home().join("projects").join(project_name(&work_dir)).join("memory/MEMORY.md");
        if link.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(mem_file(&new_name), &link);
            steps.push("claude-memory-symlink: repointed".into());
        }
    }
    // 8. Every DB reference, ONE transaction. Python's four tables may be
    //    absent on a fresh rust-only home — reported as absent, never a
    //    silent skip. UPDATE ... WHERE session=old is naturally convergent
    //    (a retry matches 0 rows).
    let counts: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let counts_c = counts.clone();
    let old_s = name.to_string();
    let new_s = new_name.clone();
    let db_result = state
        .store
        .write_async(move |conn| {
            ensure_fleet_tables(conn)?;
            let mut out = Vec::new();
            // Python's cascade (py:76375-76437). issues: active only —
            // historical/deleted keep the old name, matching Python.
            let python_tables: [(&str, &str); 4] = [
                ("issues", "UPDATE issues SET session=?1 WHERE session=?2 AND deleted IS NULL"),
                ("schedules", "UPDATE schedules SET session=?1 WHERE session=?2"),
                ("session_gates", "UPDATE session_gates SET session=?1 WHERE session=?2"),
                ("saved_messages", "UPDATE saved_messages SET session=?1 WHERE session=?2"),
            ];
            for (table, sql) in python_tables {
                match conn.execute(sql, rusqlite::params![new_s, old_s]) {
                    Ok(n) => out.push(format!("db.{table}: {n} row(s)")),
                    Err(_) => out.push(format!("db.{table}: table absent (fresh home)")),
                }
            }
            for table in ["steering_queue", "steering_history", "share_tokens", "cmd_history"] {
                let n = conn.execute(
                    &format!("UPDATE {table} SET session=?1 WHERE session=?2"),
                    rusqlite::params![new_s, old_s],
                )?;
                out.push(format!("db.{table}: {n} row(s)"));
            }
            // prefs session_reports is keyed by NAME inside a JSON blob —
            // Python orphans it and the renamed lane loses its self-reported
            // status until the next hook fires.
            let reports: Option<String> = conn
                .query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| r.get(0))
                .ok();
            if let Some(raw) = reports {
                if let Ok(mut v) = serde_json::from_str::<Value>(&raw) {
                    if let Some(obj) = v.as_object_mut() {
                        if let Some(rep) = obj.remove(&old_s) {
                            obj.insert(new_s.clone(), rep);
                            conn.execute(
                                "UPDATE prefs SET value=?1 WHERE key='session_reports'",
                                [v.to_string()],
                            )?;
                            out.push("prefs.session_reports: key migrated".into());
                        } else {
                            out.push("prefs.session_reports: no report under old name".into());
                        }
                    }
                }
            }
            *counts_c.lock().unwrap() = out;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("issue".into()),
                    entity_id: new_s.clone(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .await;
    if let Err(e) = db_result {
        return fail(&steps, format!("db reference migration failed (transaction rolled back): {e}"));
    }
    steps.extend(counts.lock().unwrap().iter().cloned());
    steps.push("db.session_events: audit rows keep the old name (deliberate — the rename journal entry links them)".into());
    // 9. Re-export AMUX_SESSION for future panes (py:76416) — best-effort;
    //    the RUNNING shell keeps its env until restart, same as Python.
    if is_running(&new_name).await {
        let stq = st(&new_name);
        let _ = tmux(&["setenv", "-t", &stq, "AMUX_SESSION", &new_name]).await;
        steps.push("tmux-env: AMUX_SESSION re-exported for future panes".into());
    }
    emit_event(
        state, &new_name, "session.renamed",
        Some(json!({"old": name, "new": new_name, "resuming": resuming, "steps": steps})),
        None, "config-rename",
    )
    .await;
    j200(json!({
        "ok": true,
        "name": new_name,
        "message": format!("renamed to {new_name}"),
        "resumed_partial": resuming,
        "steps": steps,
    }))
}

// ---------------------------------------------------------------------------
// Hot model switching (AMUX-2617)
// ---------------------------------------------------------------------------
//
// A model/effort change on a LIVE claude session is delivered as an in-session
// `/model <id>` slash command instead of a restart. The conversation, its
// context, and the loaded logs all survive — which is the whole ask: Ethan
// switched models inside Claude Code mid-conversation and "didn't have to
// re-load all logs and stuff".
//
// Why this had to be built rather than turned on: the capability was DECLARED
// and never reached a session (ethos rule 1). provider/claude.rs has answered
// `hot_model_switch: true` since RR-0043, and amux-core's
// classify_config_change returns `NextTurn` for a same-provider model change
// on a provider with that capability — but every fleet model change still went
// through restart_for_swap (graceful /exit, kill tmux, relaunch --resume),
// paying a restart, a resume, and the whole boot choreography.
//
// SYNTAX, verified against Claude Code v2.1.226 in a throwaway tmux pane on
// 2026-08-09 (transcript on the card) — not read off a doc:
//   /model sonnet                    -> "Set model to Sonnet 5 and saved as
//                                       your default for new sessions"
//   /model claude-haiku-4-5-20251001 -> full dated ids are accepted
//   /model claude-opus-5[1m]         -> "Set model to Opus 5 (1M context)…"
//   /effort high                     -> "Set effort level to high (saved as
//                                       your default for new sessions): …"
//   /model definitely-not-a-model    -> "Model 'definitely-not-a-model' not
//                                       found"
// So the SPA's picker VALUES (crates/amux-dashboard/static/app.js: `opus`,
// `claude-opus-5[1m]`, `claude-haiku-4-5-20251001`, …) go through verbatim and
// no id-mapping table is needed. One would be a second place to keep in step
// with a list amux does not own, and it would silently mistranslate every id
// added after it was written.
//
// The rejection line matters as much as the ack: it is what makes a bad id
// VISIBLE, so a hot switch that the agent refuses falls back to a restart and
// says so, instead of leaving the config and the agent disagreeing about which
// model is running (ethos rule 4).
//
// `/model` with NO argument opens an interactive picker ("Enter to set as
// default · s to use this session only"). amux does not drive it: a
// keystroke-driven picker is exactly the terminal-driving D1 exists to retire,
// and it cannot express an arbitrary model id anyway. A change BACK to the
// provider default (empty value) therefore keeps the restart path.
//
// KNOWN SIDE EFFECT, accepted and named rather than discovered later: the
// argument form of `/model` also writes `model` into ~/.claude/settings.json
// ("saved as your default for new sessions"); the picker's `s` key is the only
// session-scoped alternative and it cannot take an arbitrary id. This is inert
// for the fleet because every amux claude launch passes an explicit --model
// (the session's CC_FLAGS, or CC_DEFAULT_FLAGS when it has none — see
// dedupe_default_flags), which overrides the global default. It does change
// the default for a bare `claude` a human starts by hand.

// THE CONFIRMATION DIALOG, and how it was found. A mid-conversation model
// switch is not silent — Claude Code asks:
//
//     Switch model?                      | Change effort level?
//     Your next response will be slower and use more tokens
//     This conversation is cached for the current model. Switching to Haiku
//     4.5 means the full history gets re-read on your next message.
//   ❯ 1. Yes, switch to Haiku 4.5           | 1. Yes, switch to high
//     2. No, go back                        | 2. No, go back
//
// — one dialog per changed key, and the TITLES differ while the body does not.
//
// It is in no `--help` output and it does not appear on a fresh pane, so the
// only way to find it was to run the shipped path against a session with a
// real conversation. The first two live runs fell back to a restart on
// switches that had ALREADY landed — the delivery was fine, the pane was
// sitting on an unanswered dialog — and the fallback's own log line (the pane
// tail, added for exactly this reason) is what showed it. Without that tail
// the symptom is "hot switching does not work", and the natural next move is
// to widen the timeout, which would never have helped.
//
// amux answers it. That is not amux deciding something that was the user's to
// decide (D2 / ethos rule 8): the switch was requested explicitly through the
// API by whoever operated the picker, and this dialog asks only whether they
// meant the thing they just asked for. Leaving it unanswered would park the
// session on a selector amux itself opened, and would make the feature useless
// on precisely the sessions it exists for — the ones with a conversation worth
// keeping.

/// What Claude Code prints when a slash config command lands.
const CC_MODEL_ACK: &str = "Set model to ";
const CC_EFFORT_ACK: &str = "Set effort level to ";
/// …and when it refuses the id ("Model 'x' not found").
const CC_SLASH_REJECT: &str = "not found";
/// The body every mid-conversation config-change confirmation shares — the
/// model dialog and the effort dialog differ in TITLE but not in this line.
const CC_CONFIG_CONFIRM_BODY: &str = "This conversation is cached for the current";
/// How far below the echoed command the answer may sit. Claude renders it on
/// the very next line (`  ⎿  Set model to …`); the slack absorbs a wrap.
const SLASH_ACK_WINDOW: usize = 6;
/// The slash command is handled locally by the CLI, so the ack renders in well
/// under a second — but a mid-conversation switch interposes the confirmation
/// dialog above, which amux has to see and answer first. Poll rather than
/// sleeping a fixed budget so a loaded machine does not turn a working switch
/// into a restart.
const HOT_ACK_TIMEOUT: Duration = Duration::from_secs(12);
/// How long to watch for the confirmation dialog of a switch that was QUEUED
/// on the steering queue rather than delivered immediately.
const QUEUED_CONFIRM_WATCH: Duration = Duration::from_secs(300);

/// How a model/effort change reaches a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwapMode {
    /// Delivered to the running agent — conversation kept, no restart.
    Hot,
    /// Stop + relaunch with --resume: the pre-AMUX-2617 behaviour, still the
    /// fallback for everything the hot path cannot do honestly.
    Restart,
    /// Nothing is running, so rewriting the env file IS the whole change.
    EnvOnly,
}

impl SwapMode {
    /// The wire tag the SPA toasts on.
    fn tag(self) -> &'static str {
        match self {
            SwapMode::Hot => "hot",
            SwapMode::Restart => "restart",
            SwapMode::EnvOnly => "env_only",
        }
    }
}

/// Pick the weakest mode that honestly applies the change. Pure, so the choice
/// is unit-testable without tmux, a provider process, or a live session.
///
/// `expressible` = "this change can be stated as a slash command with an
/// argument" — false for a reset to the provider default (see the module note
/// on the argument-less picker).
fn plan_config_swap(
    provider: &str,
    caps: &ProviderCapabilities,
    running: bool,
    expressible: bool,
) -> SwapMode {
    if !running {
        return SwapMode::EnvOnly;
    }
    // `provider == "claude"` is NOT redundant with the capability. The
    // capability says "this provider can change model without a restart"; the
    // provider check says "and amux knows the syntax to ask it to". An adapter
    // can legitimately set the flag before anyone has taught session_verbs its
    // slash command, and over-restarting is the honest fallback for a
    // capability nobody has wired a delivery path for.
    if caps.hot_model_switch && provider == "claude" && expressible {
        SwapMode::Hot
    } else {
        SwapMode::Restart
    }
}

/// What the agent did about one delivered slash config command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HotOutcome {
    /// Acknowledged in the agent's own UI.
    Applied,
    /// Accepted but parked on the steering queue — it lands at the next turn
    /// boundary. NOT a failure: restarting a mid-turn session to apply a model
    /// change is precisely the cost this feature removes, and `NextTurn` is
    /// what amux-core classifies this change as anyway.
    Queued,
    /// Not delivered, or delivered and refused. The caller MUST fall back to
    /// the restart path and say that it did.
    Failed(String),
}

/// Count "`cmd` was echoed and answered with `marker`" pairs in a pane.
///
/// Anchored on the ECHOED command, not on a bare substring search for the ack:
/// a pane still showing an earlier "Set model to …" would match a bare search
/// forever, and a filter that matches everything returns a confident wrong
/// answer rather than silence (ethos rule 7). The shape being matched is what
/// Claude Code actually renders:
///     ❯ /model sonnet
///       ⎿  Set model to Sonnet 5 and saved as your default for new sessions
fn slash_answer_count(pane: &str, cmd: &str, marker: &str) -> usize {
    let clean = strip_ansi(pane);
    let lines: Vec<&str> = clean.lines().collect();
    let mut hits = 0;
    for (i, l) in lines.iter().enumerate() {
        if !l.contains(cmd) {
            continue;
        }
        if lines
            .iter()
            .skip(i + 1)
            .take(SLASH_ACK_WINDOW)
            .any(|c| c.contains(marker))
        {
            hits += 1;
        }
    }
    hits
}

/// The rejection line for `cmd`, if the pane shows one.
fn slash_reject_line(pane: &str, cmd: &str) -> Option<String> {
    let clean = strip_ansi(pane);
    let lines: Vec<&str> = clean.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if !l.contains(cmd) {
            continue;
        }
        if let Some(c) = lines
            .iter()
            .skip(i + 1)
            .take(SLASH_ACK_WINDOW)
            .find(|c| c.contains(CC_SLASH_REJECT))
        {
            return Some(c.trim().trim_start_matches(['⎿', '│', ' ']).trim().to_string());
        }
    }
    None
}

/// Read the pane's verdict on a slash config command, comparing AFTER against
/// BEFORE so a stale ack left on screen by an earlier switch cannot be
/// mistaken for this one. `None` = no verdict yet, keep polling.
///
/// The comparison is deliberately asymmetric: a scrolled-off older pair can
/// hide a real ack (count stays level -> `None` -> an unnecessary restart),
/// but nothing can manufacture one. False negatives cost a restart; a false
/// positive would leave the agent on the old model while amux reported
/// success, which is the failure this whole path exists to avoid.
fn slash_verdict(before: &str, after: &str, cmd: &str, ack: &str) -> Option<HotOutcome> {
    if slash_answer_count(after, cmd, ack) > slash_answer_count(before, cmd, ack) {
        return Some(HotOutcome::Applied);
    }
    if slash_answer_count(after, cmd, CC_SLASH_REJECT)
        > slash_answer_count(before, cmd, CC_SLASH_REJECT)
    {
        return Some(HotOutcome::Failed(
            slash_reject_line(after, cmd).unwrap_or_else(|| "rejected by the agent".into()),
        ));
    }
    None
}

/// The key that CONFIRMS a pending config-change dialog, if one is on screen.
///
/// Anchored on the dialog's BODY, not its title. The model dialog is titled
/// "Switch model?" and the effort one "Change effort level?" — pinning the
/// title made `/effort` fall back to a restart on its first live run while
/// `/model` worked, with the two failures looking nothing alike. The body line
/// is the invariant across the family, and it is also what identifies the
/// dialog as "this config change re-reads the conversation, confirm?" rather
/// than some other selector amux must not touch.
///
/// The choice is read from the OPTION TEXT ("Yes"), never hardcoded to `1`, so
/// a reordering of the two options cannot silently turn a confirm into a
/// cancel — the failure mode where amux reports a switch and presses "No".
fn config_switch_confirm_key(pane: &str) -> Option<String> {
    let clean = strip_ansi(pane);
    if !clean.contains(CC_CONFIG_CONFIRM_BODY) {
        return None;
    }
    let re = cached_re!(r"(?i)(?:^|\s)(\d+)\.\s*yes\b");
    clean
        .lines()
        .find_map(|l| re.captures(l).map(|c| c[1].to_string()))
}

/// Watch for the confirmation dialog of a switch that went onto the STEERING
/// QUEUE instead of being delivered inline, and answer it once.
///
/// A queued `/model` lands at the next turn boundary, long after this request
/// has returned — and the dialog then opens with nobody to answer it, parking
/// the session on a selector amux itself asked for. Bounded, single-shot, and
/// narrow: it only ever answers a "Switch model?" dialog. If the window
/// expires the dialog is still on screen and the session reads as `waiting` on
/// the dashboard — stalled and SEEN, never stalled and silent.
fn spawn_switch_confirm_watcher(name: &str) {
    let name = name.to_string();
    tokio::spawn(async move {
        let deadline = std::time::Instant::now() + QUEUED_CONFIRM_WATCH;
        while std::time::Instant::now() < deadline {
            sleep_ms(2000).await;
            let pane = tmux_capture(&name, 40).await;
            if let Some(key) = config_switch_confirm_key(&pane) {
                send_key(&name, &key).await;
                tracing::info!(session = %name, key, "hot config: confirmed a queued config change");
                return;
            }
        }
        tracing::warn!(
            session = %name,
            "hot config: a queued config change never showed its confirmation inside the watch window"
        );
    });
}

/// Deliver one slash config command to a live session and report what the
/// agent did about it.
///
/// Delivery goes through `send_text` — the SAME choreography the send verb
/// uses (resume-picker guard, status-gated Escape discipline with the 1.3s
/// double-Escape rule, C-u clear, the @/slash picker close before Enter,
/// steering enqueue at a selector) — never hand-rolled send-keys. Every one of
/// those guards was paid for by an incident; a second, thinner copy of the
/// choreography here would rediscover all of them.
async fn deliver_hot_config(state: &AppState, name: &str, cmd: &str, ack: &str) -> HotOutcome {
    let before = tmux_capture(name, 40).await;
    let (sent, msg) = send_text(state, name, cmd, true).await;
    tracing::info!(session = name, cmd, sent, result = %msg, "hot config: delivered");
    if !sent {
        return HotOutcome::Failed(msg);
    }
    // send_text says exactly "sent" only when the keys landed and Enter was
    // pressed against a live prompt. Every other success string ("queued
    // (steering) …", "sent (auto-woke)", "sent (waiting for in-flight boot)")
    // means delivery is deferred, so there is nothing to confirm yet — and
    // claiming a verdict we cannot see is what rule 4 forbids.
    if msg != "sent" {
        spawn_switch_confirm_watcher(name);
        return HotOutcome::Queued;
    }
    let deadline = std::time::Instant::now() + HOT_ACK_TIMEOUT;
    let mut last = String::new();
    let mut confirmed = false;
    while std::time::Instant::now() < deadline {
        sleep_ms(250).await;
        last = tmux_capture(name, 40).await;
        if let Some(v) = slash_verdict(&before, &last, cmd, ack) {
            return v;
        }
        // The switch is waiting on its confirmation dialog: answer it once and
        // keep polling for the ack that follows.
        if !confirmed {
            if let Some(key) = config_switch_confirm_key(&last) {
                send_key(name, &key).await;
                confirmed = true;
                tracing::info!(session = name, cmd, key, "hot config: confirmed the config-change dialog");
            }
        }
    }
    // A fallback that leaves no trace is indistinguishable from a switch that
    // never happened (ethos rule 4) — and the first live run of this path fell
    // back on a switch that HAD landed, which was only diagnosable because the
    // restart's --resume replayed the ack into the new pane. Log what the pane
    // actually said, so the next timeout is decidable from the log alone.
    let clean = strip_ansi(&last);
    let tail: String = clean.lines().rev().take(12).collect::<Vec<_>>().join(" | ");
    tracing::warn!(
        session = name,
        cmd,
        ack,
        echo_seen = clean.contains(cmd),
        ack_seen = clean.contains(ack),
        pane_tail = %tail,
        "hot config: no acknowledgement, falling back to a restart"
    );
    HotOutcome::Failed(format!(
        "no acknowledgement in the pane within {}s",
        HOT_ACK_TIMEOUT.as_secs()
    ))
}

/// The verdict on a whole change once every slash command has been delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HotFold {
    /// Every command was acknowledged by the agent.
    AllApplied,
    /// Nothing failed, but at least one command is parked for the next turn.
    SomeQueued,
    /// At least one command did not land. Carries the reason so the caller can
    /// report WHY it fell back, not merely that it did.
    Failed(String),
}

/// Fold per-command outcomes. Failure dominates queued, which dominates
/// applied — a change is only "live" when all of it is.
fn fold_hot_outcomes(outcomes: &[HotOutcome]) -> HotFold {
    if let Some(HotOutcome::Failed(why)) = outcomes
        .iter()
        .find(|o| matches!(o, HotOutcome::Failed(_)))
    {
        return HotFold::Failed(why.clone());
    }
    if outcomes.contains(&HotOutcome::Queued) {
        return HotFold::SomeQueued;
    }
    HotFold::AllApplied
}

/// THE fallback rule, as one pure function the delivery path actually calls:
/// a hot switch that did not land becomes a restart. Never a silent stay on
/// the old model, and never a "hot" report for a switch nobody confirmed.
fn mode_after_delivery(fold: &HotFold) -> SwapMode {
    match fold {
        HotFold::Failed(_) => SwapMode::Restart,
        HotFold::AllApplied | HotFold::SomeQueued => SwapMode::Hot,
    }
}

/// The restart path, with the scrollback capture that makes it survivable.
/// Only ever called when a restart actually happens: `capture_log_tail_for_reload`
/// stops the pane pipe as a side effect, which would be a real harm on a hot
/// switch that never restarts anything.
async fn restart_with_log_reload(
    state: &AppState,
    name: &str,
    provider: &str,
    reason: &str,
) -> bool {
    if capture_log_tail_for_reload(name, reason).await {
        mark_pending_log_reload(name, reason);
    }
    restart_for_swap(state, name, provider).await
}

/// What a config change did, in the shape the API reports it.
struct SwapReport {
    mode: SwapMode,
    /// Is the RUNNING agent on the new config now? False for a change parked
    /// on the steering queue, for a failed restart, and for `EnvOnly` (there
    /// is no agent; the next start picks it up).
    applied: bool,
    /// Appended to the human message so the response says what happened
    /// instead of leaving the caller to infer it.
    note: &'static str,
    /// Set when the hot path was tried and did not land — the fallback must
    /// leave a trace, not just a different outcome (ethos rule 6).
    hot_error: Option<String>,
}

/// Apply a model/effort change whose new CC_FLAGS are already on disk: hot
/// when the provider can take it, restart otherwise, restart as the fallback
/// when the hot delivery does not land.
///
/// `cmds` are the slash commands (with the ack each one prints), in the order
/// they must be delivered. An EMPTY list means nothing has to reach the agent.
/// `expressible` is false when any part of the change has no argument form.
async fn apply_live_config_change(
    state: &AppState,
    name: &str,
    provider: &str,
    running: bool,
    cmds: &[(String, &'static str)],
    expressible: bool,
    reason: &str,
) -> SwapReport {
    let caps = super::workers::provider_caps(provider);
    let mode = plan_config_swap(provider, &caps, running, expressible && !cmds.is_empty());
    match mode {
        SwapMode::EnvOnly => SwapReport {
            mode,
            applied: false,
            note: "",
            hot_error: None,
        },
        SwapMode::Hot => {
            let mut outcomes = Vec::with_capacity(cmds.len());
            for (cmd, ack) in cmds {
                let o = deliver_hot_config(state, name, cmd, ack).await;
                let failed = matches!(o, HotOutcome::Failed(_));
                outcomes.push(o);
                if failed {
                    // Stop delivering: the agent's config and the env file
                    // already disagree, and a second command layered on top of
                    // an unknown state only widens the gap.
                    break;
                }
            }
            let fold = fold_hot_outcomes(&outcomes);
            match mode_after_delivery(&fold) {
                SwapMode::Restart => {
                    let why = match fold {
                        HotFold::Failed(w) => w,
                        _ => String::new(),
                    };
                    let restarted = restart_with_log_reload(state, name, provider, reason).await;
                    SwapReport {
                        mode: SwapMode::Restart,
                        applied: restarted,
                        note: if restarted {
                            " (live switch failed; session restarted to apply it, log reload queued)"
                        } else {
                            " (live switch failed AND the restart failed — the session may still be on the old model)"
                        },
                        hot_error: Some(why),
                    }
                }
                _ => SwapReport {
                    mode: SwapMode::Hot,
                    applied: matches!(fold, HotFold::AllApplied),
                    note: if matches!(fold, HotFold::AllApplied) {
                        " (switched live — conversation kept, no restart)"
                    } else {
                        " (session is mid-turn — queued, applies at the next turn boundary; no restart)"
                    },
                    hot_error: None,
                },
            }
        }
        SwapMode::Restart => {
            let restarted = restart_with_log_reload(state, name, provider, reason).await;
            SwapReport {
                mode,
                applied: restarted,
                note: if restarted {
                    " (session restarted; log reload queued)"
                } else {
                    " (restart failed)"
                },
                hot_error: None,
            }
        }
    }
}

/// Every config write drops the cached session list (AMUX-2926).
///
/// A WRAPPER, not a call at each return: `config_patch_inner` returns from a
/// dozen places — one per field — so invalidating inside it would mean adding
/// the same line to each, and the next field added would silently not get it.
/// One choke point cannot be missed.
///
/// Deliberately unconditional, including on the error paths. A 400 usually
/// means nothing was written, but "usually" is doing load-bearing work there:
/// several branches write one field and then reject a second. Clearing a cache
/// that did not need clearing costs one rebuild; NOT clearing one that did is
/// the bug this fixes.
async fn config_patch(state: &AppState, name: &str, body: &Value) -> Response {
    let out = config_patch_inner(state, name, body).await;
    crate::api::sessions_legacy::invalidate_sessions_cache();
    out
}

async fn config_patch_inner(state: &AppState, name: &str, body: &Value) -> Response {
    if !body.is_object() {
        return jresp(StatusCode::BAD_REQUEST, json!({"error": "payload must be a JSON object"}));
    }
    let f = env_path(name);
    let mut cfg = parse_env(name);

    // Rename — convergent cascade with journaling (owner addendum on
    // AMUX-2598: "if we change a name of a worker nothing happens — we
    // should have some kind of idempotency for stuff like that under the
    // hood"). See rename_session below.
    if let Some(rename) = body.get("rename") {
        return rename_session(state, name, rename.as_str().unwrap_or("")).await;
    }

    // Change provider (py:76434).
    if let Some(pv) = body.get("provider") {
        let Some(pv) = pv.as_str() else {
            return jresp(StatusCode::BAD_REQUEST, json!({"error": "provider must be a string"}));
        };
        let provider_val = pv.trim().to_lowercase();
        if !SESSION_PROVIDERS.contains(&provider_val.as_str()) {
            return jresp(
                StatusCode::BAD_REQUEST,
                json!({"error": "provider must be 'claude', 'codex', or 'gemini'"}),
            );
        }
        let old_provider = provider_of(&cfg);
        if provider_val == old_provider {
            return j200(json!({"ok": true, "message": format!("provider already set to {provider_val}")}));
        }
        let current_flags = cfg.get_or("CC_FLAGS", "").to_string();
        let flags_no_model = match strip_model_from_flags(&current_flags) {
            Ok(v) => v,
            Err(e) => {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": format!("existing CC_FLAGS for session '{name}' is malformed ({e}); fix the .env file manually before updating the provider")}));
            }
        };
        let was_yolo = is_yolo_enabled(&current_flags, &cfg);
        let flags_no_yolo = strip_provider_yolo_flags(&flags_no_model);
        let default_model = default_model_for_provider(&provider_val);
        let mut flags = if flags_no_yolo.is_empty() {
            format!("--model {default_model}")
        } else {
            format!("--model {default_model} {flags_no_yolo}")
        };
        if was_yolo {
            flags = format!("{flags} {}", provider_yolo_flag(&provider_val)).trim().to_string();
            cfg.set("CC_AUTO_CONTINUE", "1");
        }
        cfg.set("CC_PROVIDER", &provider_val);
        cfg.set("CC_FLAGS", &flags);
        let was_running = is_running(name).await;
        if capture_log_tail_for_reload(name, "provider swap").await {
            mark_pending_log_reload(name, "provider swap");
        }
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        let restarted = if was_running { restart_for_swap(state, name, &old_provider).await } else { false };
        let suffix = if restarted { " (session restarted; log reload queued)" } else { "" };
        return j200(json!({"ok": true, "message": format!("provider set to {}{suffix}", provider_label(&provider_val))}));
    }

    // Change model (py:76496), with optional inline effort.
    if let Some(mv) = body.get("model") {
        let model_val = match validate_model_name(mv) {
            Ok(v) => v,
            Err(e) => return jresp(StatusCode::BAD_REQUEST, json!({"error": e})),
        };
        let flags_no_model = match strip_model_from_flags(cfg.get_or("CC_FLAGS", "")) {
            Ok(v) => v,
            Err(e) => {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": format!("existing CC_FLAGS for session '{name}' is malformed ({e}); fix the .env file manually before updating the model")}));
            }
        };
        let old_effort = flag_value(cfg.get_or("CC_FLAGS", ""), "--effort");
        let mut flags = if model_val.is_empty() {
            flags_no_model
        } else if flags_no_model.is_empty() {
            format!("--model {model_val}")
        } else {
            format!("--model {model_val} {flags_no_model}")
        };
        // The slash commands this change needs the LIVE agent to run, in
        // delivery order. `expressible` goes false the moment any part of the
        // change is a reset-to-default, which has no argument form.
        let mut cmds: Vec<(String, &'static str)> = Vec::new();
        let mut expressible = !model_val.is_empty();
        cmds.push((format!("/model {model_val}"), CC_MODEL_ACK));
        if let Some(ev) = body.get("effort") {
            let effort_val = match validate_effort(ev) {
                Ok(v) => v,
                Err(e) => return jresp(StatusCode::BAD_REQUEST, json!({"error": e})),
            };
            flags = match set_effort_flag(&flags, &effort_val) {
                Ok(v) => v,
                Err(e) => {
                    return jresp(StatusCode::BAD_REQUEST, json!({"error": format!("existing CC_FLAGS for session '{name}' is malformed ({e}); fix the .env file manually before updating effort")}));
                }
            };
            // The SPA always sends `effort` alongside `model` (app.js
            // `payload.effort = _effortVal`), usually re-stating the value the
            // session already carries. Asking the agent to re-apply the effort
            // it is already on would add a second delivery — and a second way
            // to fail — for no change at all; worse, an UNCHANGED empty effort
            // would make the whole change inexpressible and force a restart on
            // every model swap from the picker.
            if effort_val != old_effort {
                if effort_val.is_empty() {
                    expressible = false;
                } else {
                    cmds.push((format!("/effort {effort_val}"), CC_EFFORT_ACK));
                }
            }
        }
        cfg.set("CC_FLAGS", &flags);
        let current_provider = provider_of(&cfg);
        let was_running = is_running(name).await;
        // Python also clears its in-memory credit-limit flag here (AF-14) —
        // process state this origin does not hold.
        // The env rewrite is the DURABLE half and happens either way: whatever
        // the live agent does, the next cold start must come up on the new
        // model. Log-tail capture moved into restart_with_log_reload — it is
        // only meaningful when a restart actually discards the scrollback.
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        let rep = apply_live_config_change(
            state, name, &current_provider, was_running, &cmds, expressible, "model swap",
        )
        .await;
        let mut out = json!({
            "ok": true,
            "applied": rep.applied,
            "mode": rep.mode.tag(),
            "model": model_val,
            "message": format!("model set to {model_val}{}", rep.note),
        });
        if let Some(e) = rep.hot_error {
            out["hot_error"] = json!(e);
        }
        return j200(out);
    }

    // Change effort only (py:76570).
    if let Some(ev) = body.get("effort") {
        let effort_val = match validate_effort(ev) {
            Ok(v) => v,
            Err(e) => return jresp(StatusCode::BAD_REQUEST, json!({"error": e})),
        };
        let flags = match set_effort_flag(cfg.get_or("CC_FLAGS", ""), &effort_val) {
            Ok(v) => v,
            Err(e) => {
                return jresp(StatusCode::BAD_REQUEST, json!({"error": format!("existing CC_FLAGS for session '{name}' is malformed ({e}); fix the .env file manually before updating effort")}));
            }
        };
        cfg.set("CC_FLAGS", &flags);
        let current_provider = provider_of(&cfg);
        let was_running = is_running(name).await;
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        // `/effort <level>` is hot on the same slash surface as `/model`
        // (verified 2026-08-09: "Set effort level to high (saved as your
        // default for new sessions)"), so an effort change costs no restart
        // either. Reset-to-default has no argument form, so it does.
        let cmds: Vec<(String, &'static str)> = if effort_val.is_empty() {
            Vec::new()
        } else {
            vec![(format!("/effort {effort_val}"), CC_EFFORT_ACK)]
        };
        let rep = apply_live_config_change(
            state, name, &current_provider, was_running, &cmds, !effort_val.is_empty(),
            "effort change",
        )
        .await;
        let shown = if effort_val.is_empty() { "default".to_string() } else { effort_val };
        let mut out = json!({
            "ok": true,
            "applied": rep.applied,
            "mode": rep.mode.tag(),
            "effort": shown,
            "message": format!("effort set to {shown}{}", rep.note),
        });
        if let Some(e) = rep.hot_error {
            out["hot_error"] = json!(e);
        }
        return j200(out);
    }

    // Toggle YOLO (py:76608).
    if body.get("toggle_yolo").map(py_truthy).unwrap_or(false)
        || body.get("toggle_auto_continue").map(py_truthy).unwrap_or(false)
    {
        let provider = provider_of(&cfg);
        let flags = cfg.get_or("CC_FLAGS", "").to_string();
        let enabled;
        let new_flags;
        if is_yolo_enabled(&flags, &cfg) {
            new_flags = strip_provider_yolo_flags(&flags);
            cfg.set("CC_AUTO_CONTINUE", "0");
            enabled = false;
        } else {
            new_flags = format!("{flags} {}", provider_yolo_flag(&provider)).trim().to_string();
            cfg.set("CC_AUTO_CONTINUE", "1");
            enabled = true;
        }
        cfg.set("CC_FLAGS", &new_flags);
        let was_running = is_running(name).await;
        if was_running && capture_log_tail_for_reload(name, "YOLO mode change").await {
            mark_pending_log_reload(name, "YOLO mode change");
        }
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        let restarted = if was_running { restart_for_swap(state, name, &provider).await } else { false };
        let state_word = if enabled { "enabled" } else { "disabled" };
        let suffix = if restarted { " (session restarted; log reload queued)" } else { "" };
        return j200(json!({"ok": true, "message": format!("yolo {state_word}{suffix}")}));
    }

    // Change directory (py:76646): hard restart in the new dir when running.
    if let Some(dv) = body.get("dir") {
        let new_dir = dv.as_str().unwrap_or("").trim().to_string();
        let old_dir = cfg.get_or("CC_DIR", "").to_string();
        cfg.set("CC_DIR", &new_dir);
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        if new_dir != old_dir && is_running(name).await {
            let st2 = state.clone();
            let n = name.to_string();
            tokio::spawn(async move {
                // py:76651 _restart_in_new_dir: hard-kill then start. The
                // graceful stop records the resumable name first.
                let _ = stop_session(&n).await;
                kill_tmux_session(&n).await;
                sleep_ms(2000).await;
                let _ = start_session(&st2, &n, "", false).await;
            });
            return j200(json!({"ok": true, "message": "directory updated — restarting session"}));
        }
        return j200(json!({"ok": true, "message": "directory updated"}));
    }

    // Task label override (py:76662).
    if let Some(ts) = body.get("task_summary") {
        // Stamp WHEN, not just what (AMUX-2676). The card renders task_name
        // regardless of source, but task_updated was only set for BOARD-sourced
        // tasks — so a summary task showed a label with no age and no way to
        // tell it was stale. Ethan's card read "Review Recent Work" during
        // hours of unrelated work and looked authoritative.
        //
        // The meta file's mtime is NOT a substitute: unrelated writes
        // (last_send on every inbound message) rewrite the file, so mtime would
        // report a stale task as fresh — a confidently wrong answer, which is
        // worse than the missing one.
        update_meta(
            name,
            &[
                ("task_summary", json!(ts.as_str().unwrap_or("").trim())),
                ("task_summary_ts", json!(now_i64())),
            ],
        );
        return j200(json!({"ok": true, "message": "task label updated"}));
    }

    // Description (py:76667).
    if let Some(dv) = body.get("desc") {
        cfg.set("CC_DESC", dv.as_str().unwrap_or("").trim());
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        // A description is AUTO-DISCOVERY DATA, not a private label: every other
        // worker's roster names this one. Refresh the fleet so the change is
        // visible where peers actually read it, rather than only in this
        // worker's env file where nobody looks.
        let refreshed = refresh_fleet_rosters();
        return j200(json!({
            "ok": true,
            "message": "description updated",
            "rosters_refreshed": refreshed,
        }));
    }

    // Toggle pin (py:76673).
    if body.get("toggle_pin").map(py_truthy).unwrap_or(false) {
        let now_pinned = cfg.get("CC_PINNED") == Some("1");
        cfg.set("CC_PINNED", if now_pinned { "" } else { "1" });
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        return j200(json!({"ok": true, "message": "pin toggled"}));
    }

    // Isolated (raw agent, no amux harness), AMUX-3232. Persisted as the env
    // key CC_ISOLATED (the environment primitive, not a new store), so it
    // survives reload and is read on the NEXT spawn: the harness (AMUX_SESSION /
    // AMUX_URL env, the self-report hooks that key on them, --mcp-config) is not
    // injected, and the worker becomes undiscoverable to peers (fleet list,
    // roster, peer-send guard). The owner still controls it from the dashboard.
    // Explicit boolean rather than a toggle, so the UI switch is idempotent.
    if let Some(iv) = body.get("isolated") {
        let on = py_truthy(iv);
        cfg.set("CC_ISOLATED", if on { "1" } else { "" });
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        // The roster peers read is fleet-wide data; toggling isolation adds or
        // removes this worker from it, so refresh like desc/tags do.
        let refreshed = refresh_fleet_rosters();
        tracing::info!(
            session = %name, isolated = on,
            "config: isolated toggled; harness suppression + peer-undiscoverable apply at next spawn"
        );
        return j200(json!({
            "ok": true,
            "isolated": on,
            "rosters_refreshed": refreshed,
            "message": if on {
                "isolated on (raw agent, no amux harness); restart the worker to apply"
            } else {
                "isolated off; restart the worker to apply"
            },
        }));
    }

    // Branch (py:76679).
    if let Some(bv) = body.get("branch") {
        cfg.set("CC_BRANCH", bv.as_str().unwrap_or("").trim());
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        return j200(json!({"ok": true, "message": "branch updated"}));
    }

    // Tags (py:76685). Python invalidates its sessions cache here, and so do
    // we now — see config_patch's wrapper.
    //
    // This comment used to say the opposite: "this origin computes the list per
    // request, so the write IS the refresh". That was TRUE when written and was
    // falsified by a LATER commit (7ca14b5, a 2s cache on GET /api/sessions)
    // which had no reason to look here. Nothing broke loudly — the list just
    // served the pre-write value for up to 2s, while the group-isolation gate
    // read the new tags immediately, so the two disagreed about the same fact
    // (AMUX-2926). A comment asserting the ABSENCE of a mechanism elsewhere is
    // a claim that ages badly; this one is now pointed at the wrapper that
    // makes it true instead.
    //
    // ACCEPT AN ARRAY, WHICH IS WHAT THE DASHBOARD SENDS. This was
    // `tv.as_str().unwrap_or("")`: on the `{"tags":["amux"]}` the client
    // actually sends, `as_str()` on an Array is None, so it wrote CC_TAGS=""
    // and answered {"ok":true,"message":"tags updated"}. Every group edit from
    // the UI silently CLEARED the worker's groups and reported success —
    // Ethan, 2026-08-11: "i just tried to add a group but nothing happened".
    // Worse than a no-op, because groups are DERIVED from CC_TAGS
    // (GET /api/groups: "derived_from: CC_TAGS across workers"), so a wiped
    // tag removes the worker from every group it belonged to.
    //
    // An unrecognised shape is now a 400 rather than a silent clear. Clearing
    // stays reachable, but only by ASKING for it (`[]`, `""` or null) — the
    // rule being that destroying data must be requested, never inferred from a
    // type the handler failed to understand.
    if let Some(tv) = body.get("tags") {
        let joined = match tv {
            Value::Array(items) => {
                let mut bad = None;
                let parts: Vec<String> = items
                    .iter()
                    .filter_map(|x| match x.as_str() {
                        Some(s) => Some(s.trim().to_string()),
                        None => {
                            bad = Some(x.clone());
                            None
                        }
                    })
                    .filter(|s| !s.is_empty())
                    .collect();
                if let Some(b) = bad {
                    return jresp(
                        StatusCode::BAD_REQUEST,
                        json!({"error": format!("tags must be strings; got {b}"), "wrote": false}),
                    );
                }
                parts.join(",")
            }
            Value::String(s) => s.trim().to_string(),
            Value::Null => String::new(),
            other => {
                return jresp(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": format!("tags must be an array of strings, a comma-separated string, or null; got {other}"),
                        "wrote": false,
                    }),
                );
            }
        };
        cfg.set("CC_TAGS", &joined);
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        // Echo what was stored: the caller sent an array and a bare "ok" is what
        // let the silent clear go unnoticed for as long as it did.
        return j200(json!({"ok": true, "message": "tags updated", "tags": joined}));
    }

    // MCP config (py:76731).
    if let Some(mv) = body.get("mcp") {
        let mcp_val = mv.as_str().unwrap_or("").trim().to_lowercase();
        if !mcp_val.is_empty() && mcp_val != "chrome" {
            return jresp(StatusCode::BAD_REQUEST, json!({"error": "mcp must be 'chrome' or '' (empty)"}));
        }
        cfg.set("CC_MCP", &mcp_val);
        if cfg.write(&f).is_err() {
            return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "could not write session env"}));
        }
        let msg = if mcp_val.is_empty() { "mcp disabled".to_string() } else { format!("mcp set to {mcp_val}") };
        return j200(json!({"ok": true, "message": format!("{msg} (restart session to apply)")}));
    }

    // New conversation (py:76741).
    if body.get("new_conversation").map(py_truthy).unwrap_or(false) {
        if is_running(name).await {
            return jresp(
                StatusCode::CONFLICT,
                json!({"error": "stop the session before starting a new conversation"}),
            );
        }
        let mut meta = load_meta(name);
        meta.remove("cc_conversation_id");
        save_meta(name, &meta);
        return j200(json!({"ok": true, "message": "conversation reset — next start will be a fresh conversation"}));
    }

    jresp(StatusCode::BAD_REQUEST, json!({"error": "nothing to update"}))
}

// ---------------------------------------------------------------------------
// share (py:65953-65999) — token CRUD over the shared share_tokens table.
// ---------------------------------------------------------------------------

async fn share_handler(
    state: &AppState,
    name: &str,
    method: &Method,
    headers: &HeaderMap,
    body: &Value,
) -> Response {
    match *method {
        Method::POST => {
            let perms = {
                let p = body_str(body, "perms");
                if p.is_empty() { "output".to_string() } else { p }
            };
            let expires_hours = body.get("expires_hours").and_then(|v| v.as_i64());
            let label = body_str(body, "label");
            let token = {
                // secrets.token_urlsafe(16) parity: 16 random bytes, base64url.
                use base64::Engine as _;
                let mut buf = [0u8; 16];
                getrandom_fill(&mut buf);
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
            };
            let now = now_i64();
            let expires_at = expires_hours.map(|h| now + h * 3600);
            let (t2, s2, p2, l2) = (token.clone(), name.to_string(), perms.clone(), label.clone());
            let reply = state
                .store
                .write_async(move |conn| {
                    ensure_fleet_tables(conn)?;
                    conn.execute(
                        "INSERT INTO share_tokens (token, session, perms, created_at, expires_at, label) VALUES (?,?,?,?,?,?)",
                        rusqlite::params![t2, s2, p2, now, expires_at, l2],
                    )?;
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .await;
            if let Err(e) = reply {
                return jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
            }
            // Fall back to the port this server actually answers on. A share
            // link minted with the retired 8822 literal points a recipient at a
            // bind that is being removed (see crate::legacy_port).
            let self_host = format!("localhost:{}", crate::config::canonical_port());
            let host = headers
                .get("x-forwarded-host")
                .or_else(|| headers.get("host"))
                .and_then(|v| v.to_str().ok())
                .unwrap_or(&self_host)
                .to_string();
            // "It is one of OUR ports, so it is TLS." Both the canonical port
            // and the legacy bind qualify while the latter exists — pinning
            // this to a literal would emit http:// share links the day the
            // canonical port moved.
            let is_own_port = [
                Some(crate::config::canonical_port().to_string()),
                std::env::var("AMUX_RS_LEGACY_PORT").ok(),
            ]
            .into_iter()
            .flatten()
            .any(|p| host.ends_with(&format!(":{p}")));
            let scheme = if headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
                || !host.contains(':')
                || is_own_port
            {
                "https"
            } else {
                "http"
            };
            j200(json!({"token": token, "url": format!("{scheme}://{host}/s/{token}"), "expires_at": expires_at}))
        }
        Method::GET => {
            let conn = match state.store.read() {
                Ok(c) => c,
                Err(e) => return jresp(StatusCode::SERVICE_UNAVAILABLE, json!({"error": e.to_string()})),
            };
            let mut out = vec![];
            if let Ok(mut stmt) = conn.prepare(
                "SELECT token, perms, created_at, expires_at, label FROM share_tokens WHERE session=?",
            ) {
                if let Ok(rows) = stmt.query_map([name], |r| {
                    Ok(json!({
                        "token": r.get::<_, String>(0)?,
                        "perms": r.get::<_, String>(1)?,
                        "created_at": r.get::<_, i64>(2)?,
                        "expires_at": r.get::<_, Option<i64>>(3)?,
                        "label": r.get::<_, String>(4)?,
                    }))
                }) {
                    out = rows.flatten().collect();
                }
            }
            j200(json!(out))
        }
        Method::DELETE => {
            let token = body_str(body, "token");
            let s2 = name.to_string();
            let reply = state
                .store
                .write_async(move |conn| {
                    ensure_fleet_tables(conn)?;
                    if token.is_empty() {
                        conn.execute("DELETE FROM share_tokens WHERE session=?", [&s2])?;
                    } else {
                        conn.execute(
                            "DELETE FROM share_tokens WHERE token=? AND session=?",
                            rusqlite::params![token, s2],
                        )?;
                    }
                    Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
                })
                .await;
            match reply {
                Ok(_) => j200(json!({"ok": true})),
                Err(e) => jresp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
            }
        }
        _ => not_found(),
    }
}

/// Random bytes without a new dependency: /dev/urandom, falling back to a
/// time+pid hash (share tokens are convenience links, not crypto keys — but
/// urandom is present on every platform this server targets).
fn getrandom_fill(buf: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(format!("{}-{}-{:?}", std::process::id(), now_f64(), std::time::Instant::now()).as_bytes());
    let d = h.finalize();
    let n = buf.len().min(d.len());
    buf[..n].copy_from_slice(&d[..n]);
}

// ---------------------------------------------------------------------------
// Tests — hermetic AMUX_HOME + temp store; no tmux, no live fleet. The env
// mutation is process-global, so everything shares one test fn per concern
// group behind a lock (same pattern as proxy_composition.rs).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    // ---- AF-104: concurrent env writes must not share a temp path -----------
    //
    // Rebuilt from the incident's own artifact: two POST /api/sessions at
    // 2026-08-19 13:18:22.734 from one browser (a double submit), one 201 and one
    // 500 "could not write session env: No such file or directory (os error 2)".
    // A standalone reproduction of the pre-fix code reproduced that exact errno.

    /// The invariant, tested deterministically so it cannot flake: two calls
    /// never choose the same temp path. A concurrency test alone can pass by
    /// simply not colliding on a given run.
    #[test]
    fn two_temp_paths_for_the_same_file_are_never_equal() {
        let p = std::path::Path::new("/tmp/amux-af104/foo.env");
        let a = super::unique_tmp_path(p);
        let b = super::unique_tmp_path(p);
        assert_ne!(a, b, "a shared temp path is the whole bug: the first rename \
                          consumes it and the second gets ENOENT");
        assert_ne!(a, p.to_path_buf());
        assert_eq!(a.parent(), p.parent(), "temp must stay on the same filesystem \
                                            or the rename stops being atomic");
    }

    /// Corroboration against the real write path. With the pid-only temp name
    /// this fails with os error 2; it can never false-FAIL, only false-pass by
    /// not colliding — which is why the deterministic test above is the gate.
    #[test]
    fn concurrent_writes_of_one_env_file_all_succeed_and_leave_it_intact() {
        let dir = std::env::temp_dir().join(format!("amux-af104-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("worker.env");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut e = super::EnvFile::default();
                    e.set("CC_DESC", &format!("writer-{i}"));
                    e.write(&path).map_err(|err| err.to_string())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let failures: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        assert!(failures.is_empty(), "concurrent writers failed: {failures:?}");

        // And the survivor must be a WHOLE file, not a torn one — the quiet half
        // of this bug is the loser truncating the winner's temp mid-write.
        let text = std::fs::read_to_string(&path).expect("env file exists");
        assert!(text.starts_with("# updated: "), "header missing: {text:?}");
        assert_eq!(text.matches("CC_DESC=").count(), 1, "expected exactly one CC_DESC: {text:?}");
        assert!(text.ends_with('\n'), "truncated write: {text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }


    // ---- AF-96: /api/debug/logs verdict, now testable ------------------------
    //
    // AMUX-2628's shape, in numbers: pipe-pane reports ON, the writer process is
    // alive holding the right inode, the file exists — and the log has not grown
    // for an hour while the pane painted 30 seconds ago. Every individual signal
    // reads healthy. Only the correlation is wrong, which is why the endpoint
    // computes a verdict instead of printing three columns.

    fn sig(log_age: Option<i64>, act_age: Option<i64>, writer_age: i64) -> super::LogSignals {
        super::LogSignals {
            running: true,
            piped: true,
            log_age_s: log_age,
            pane_activity_age_s: act_age,
            writer_age_s: writer_age,
            stale_s: 300,
        }
    }

    /// The alarm this endpoint exists for, on the incident's own shape.
    #[test]
    fn a_log_that_stopped_growing_while_the_pane_painted_is_stale() {
        let (v, d) = super::log_verdict(&sig(Some(3600), Some(30), 7200));
        assert_eq!(v, "stale");
        assert!(d.contains("3600") && d.contains("30"), "detail must carry both ages: {d}");
    }

    /// The state that otherwise masquerades as `stale`: the pipe was re-armed
    /// AFTER the pane last spoke, so the missing output predates the writer and
    /// there is nothing to act on.
    #[test]
    fn a_pipe_rearmed_after_the_pane_last_spoke_is_recovering_not_stale() {
        assert_eq!(super::log_verdict(&sig(Some(3600), Some(30), 10)).0, "recovering");
    }

    /// The other masquerade, and the one that matters most for false alarms: the
    /// log is old because the PANE is old. `aa < la` is the whole discriminator,
    /// so this case and the stale case differ only in that comparison.
    #[test]
    fn a_quiet_pane_with_an_equally_old_log_is_idle_not_stale() {
        assert_eq!(super::log_verdict(&sig(Some(3600), Some(7200), 7200)).0, "idle");
    }

    #[test]
    fn a_recently_written_log_is_ok() {
        assert_eq!(super::log_verdict(&sig(Some(10), Some(5), 7200)).0, "ok");
    }

    /// A missing log file with piping on is stale regardless of pane activity —
    /// there is no age to compare, and "no file" must never fall through to ok.
    #[test]
    fn piping_on_with_no_log_file_at_all_is_stale() {
        assert_eq!(super::log_verdict(&sig(None, Some(30), 7200)).0, "stale");
        assert_eq!(super::log_verdict(&sig(None, None, 0)).0, "stale");
    }

    #[test]
    fn the_pipe_and_pane_preconditions_short_circuit_before_any_age_comparison() {
        let mut s = sig(Some(3600), Some(30), 7200);
        s.piped = false;
        assert_eq!(super::log_verdict(&s).0, "unpiped");
        s.running = false;
        assert_eq!(super::log_verdict(&s).0, "not-running", "not-running outranks unpiped");
    }

    /// The threshold is a threshold, not decoration: one second either side of
    /// `stale_s` must land in different cells. Without this, raising stale_s to
    /// infinity would keep every other test green.
    #[test]
    fn the_stale_threshold_actually_discriminates() {
        assert_eq!(super::log_verdict(&sig(Some(301), Some(30), 7200)).0, "stale");
        assert_eq!(super::log_verdict(&sig(Some(300), Some(30), 7200)).0, "ok",
            "at exactly stale_s the log is not yet stale — the comparison is strict");
    }

    use super::*;

    /// AC-346: the folder-trust seed must fold `hasTrustDialogAccepted=true` into
    /// `projects[dir]` WITHOUT disturbing the rest of `~/.claude.json`, and must
    /// no-op when it is already set or the shape is not mergeable.
    #[test]
    fn trust_seed_merges_without_clobbering_and_is_idempotent() {
        // Preserves oauthAccount + a sibling project, adds the flag to our dir.
        let existing = json!({
            "oauthAccount": {"emailAddress": "x@y.z"},
            "theme": "dark",
            "projects": {
                "/other/dir": {"hasTrustDialogAccepted": true, "history": [1, 2]}
            }
        });
        let got = trust_seed_merge(existing, "/work/dir").expect("a new dir must merge");
        assert_eq!(got["oauthAccount"]["emailAddress"], "x@y.z", "oauth untouched");
        assert_eq!(got["theme"], "dark", "theme untouched");
        assert_eq!(got["projects"]["/other/dir"]["hasTrustDialogAccepted"], true, "sibling untouched");
        assert_eq!(got["projects"]["/other/dir"]["history"], json!([1, 2]));
        assert_eq!(got["projects"]["/work/dir"]["hasTrustDialogAccepted"], true, "our dir now trusted");

        // A missing file (empty object) seeds cleanly.
        let fresh = trust_seed_merge(json!({}), "/work/dir").expect("empty doc must merge");
        assert_eq!(fresh["projects"]["/work/dir"]["hasTrustDialogAccepted"], true);

        // Already trusted -> None (no pointless rewrite of a large file, and no
        // race window on the common case).
        let already = json!({"projects": {"/work/dir": {"hasTrustDialogAccepted": true}}});
        assert!(trust_seed_merge(already, "/work/dir").is_none(), "no write when already trusted");

        // Unmergeable shape (root is not an object) -> None, leave it alone.
        assert!(trust_seed_merge(json!("not an object"), "/work/dir").is_none());
    }

    /// AMUX-3159 seed direction (codex analog of AC-346): the codex trust seed
    /// only appends when a dir has NO entry, respects an existing decision
    /// (trusted OR untrusted), and REFUSES to act on an unparseable config —
    /// fail-safe, so it never appends a duplicate table or corrupts codex's own
    /// settings. `codex_dir_already_known` is the pure gate; the append is I/O.
    #[test]
    fn codex_dir_known_gates_the_seed() {
        let cfg = "model = \"gpt-5.5\"\n\n[projects.\"/a\"]\ntrust_level = \"trusted\"\n";
        assert!(codex_dir_already_known(cfg, "/a"), "an existing project is known -> no re-seed");
        assert!(!codex_dir_already_known(cfg, "/b"), "a fresh dir has no entry -> seedable");
        // No file (empty) parses to an empty doc: a fresh dir is seedable, and the
        // caller creates the file with the trust table.
        assert!(!codex_dir_already_known("", "/b"), "empty config -> fresh dir seedable");
        // An existing UNTRUSTED decision is respected, not overridden (appending a
        // duplicate [projects."/c"] would be a TOML error anyway).
        let untrusted = "[projects.\"/c\"]\ntrust_level = \"untrusted\"\n";
        assert!(codex_dir_already_known(untrusted, "/c"), "a deliberate untrust is left alone");
        // Unparseable config -> treated as known -> we do NOT append into a file we
        // cannot understand (fail-safe).
        assert!(codex_dir_already_known("[unclosed table", "/b"), "unparseable -> refuse to append");
    }

    /// AMUX-3364: `POST /api/sessions/<n>/start` used to answer an optimistic 202
    /// ok:true and then no-op in the background for a session that could never
    /// launch (the archived cloud scaffold), with NO server-rs.log trace —
    /// indistinguishable from a real start. start_block_reason is the cheap
    /// synchronous predicate the handler now answers ok:false from; every arm
    /// here is a case that reported false success before the fix.
    #[test]
    fn start_block_reason_refuses_the_unlaunchable_synchronously() {
        let dir = tempfile::tempdir().expect("tmp");
        let _g = crate::api::settings::test_env::set_home(dir.path());
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        let write =
            |n: &str, body: &str| std::fs::write(sessions.join(format!("{n}.env")), body).expect("write");

        // A normal, launchable session: nothing cheap blocks it.
        write("worker", "CC_DIR=\"/tmp\"\n");
        assert_eq!(start_block_reason("worker", &parse_env("worker")), None);

        // THE REPORTED CASE: an archived scaffold. Refused synchronously, never
        // an optimistic ok:true that vanishes in the background.
        write("hello-world", "CC_ARCHIVED=\"1\"\n");
        let r = start_block_reason("hello-world", &parse_env("hello-world"))
            .expect("an archived session must block");
        assert!(r.contains("archived"), "{r}");

        // herdr start is not ported to the rust origin — refuse now, do not 202
        // and then fail invisibly.
        write("herd", "CC_BACKEND=\"herdr\"\n");
        assert!(start_block_reason("herd", &parse_env("herd")).unwrap().contains("herdr"));

        // No env file at all: not found. (Also caught upstream; asserted here so
        // the handler's synchronous contract is locked in.)
        assert!(start_block_reason("ghost-xyz", &parse_env("ghost-xyz")).unwrap().contains("not found"));

        // An invalid name never resolves to a launch.
        assert!(start_block_reason("bad/name", &parse_env("bad/name")).unwrap().contains("invalid"));
    }

    /// AMUX-3384: the prompt auto-capture must not write a typed credential into a
    /// fleet-readable board card. redact_prompt_secrets masks the credential
    /// shapes (labelled, known-prefix, bearer, PEM, high-entropy) and leaves
    /// ordinary prose — and a git sha — alone.
    #[test]
    fn redact_prompt_secrets_masks_credentials_and_spares_prose() {
        let r = redact_prompt_secrets;
        // Fake secrets are assembled from PARTS so the repo secret-scanner does
        // not flag this test's source — the strings exist only at runtime.
        // The reported case: a labelled password. Value gone, label kept.
        let out = r("keep the netsuite password: Hunter2Winter! in the private repo");
        assert!(out.contains("[REDACTED]") && !out.contains("Hunter2Winter!"), "{out}");
        assert!(out.contains("password"), "the label stays so the card still reads: {out}");
        // env-var assignment (underscored name, where \b would fail).
        let envval = "wJalrXUtnFEMIabcdefEXAMPLEKEY99";
        assert!(!r(&format!("set AWS_SECRET_ACCESS_KEY={envval} now")).contains(envval), "env-var secret");
        // Known token prefixes.
        let sk = format!("sk-{}", "ABCdef0123456789ABCdef0123");
        assert!(!r(&format!("here is {sk} ok")).contains(&sk), "stripe/openai");
        let ghp = format!("gh{}_{}", "p", "ABCdefGHIjkl0123456789ABCdefGHI");
        assert!(!r(&format!("token {ghp} x")).contains(&ghp), "github pat");
        let aws = format!("AK{}IOSFODNN7EXAMPLE", "IA");
        assert!(!r(&format!("use {aws} for aws")).contains(&aws), "aws key id");
        // Bearer token (with dots).
        let jwt = format!("Bearer ey{}.ey{}.abcDEF123", "JhbGciOi", "JzdWIiOi");
        assert!(!r(&format!("Authorization: {jwt} done")).contains("JhbGciOi"), "bearer/jwt");
        // Ordinary prose is untouched.
        let clean = "fix the login bug in auth.rs and rerun the tests";
        assert_eq!(r(clean), clean, "prose must not be redacted");
        // A 40-hex git sha survives (single-case, no punct, < 44 chars).
        let sha = "revert a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2 please";
        assert_eq!(r(sha), sha, "git sha must survive: {}", r(sha));
    }

    /// Ethan, 2026-08-11: worker-to-worker messaging is intra-group unless
    /// explicitly configured. The escapes must be CONFIG — a body flag any
    /// caller could set would make the rule advisory.
    #[test]
    fn cross_group_sends_are_refused_unless_configured() {
        let dir = tempfile::tempdir().expect("tmp");
        let _g = crate::api::settings::test_env::set_home(dir.path());
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        let write = |n: &str, body: &str| {
            std::fs::write(sessions.join(format!("{n}.env")), body).expect("write");
        };
        write("ts-gke", "CC_TAGS=\"customers\"\n");
        write("tubescience", "CC_TAGS=\"customers\"\n");
        write("gtm-engine", "CC_TAGS=\"gtm\"\n");
        write("amux", "CC_TAGS=\"amux\"\nCC_RECEIVE_ANY=1\n");
        write("broadcaster", "CC_TAGS=\"customers\"\nCC_SEND_ALLOW=\"gtm\"\n");
        write("lonely", "\n"); // untagged

        // THE REPORTED CASE: different groups, no config -> refused.
        let err = cross_group_send_ok("ts-gke", "gtm-engine").expect_err("must refuse");
        assert!(err.contains("cross-group send refused"), "{err}");
        // The refusal must NAME both escapes, or it is a wall rather than a rule.
        assert!(err.contains("CC_SEND_ALLOW"), "{err}");
        assert!(err.contains("CC_RECEIVE_ANY"), "{err}");
        // AVE-36: it must also name the board fallback BY NOTIFYING VERB.
        // "Use a board card" alone left every cross-group handoff with a
        // latent silent-delivery failure: `progress` recorded without
        // notifying, `ask` notified, and the refusal never said which.
        assert!(err.contains("amux board progress"), "{err}");
        assert!(err.contains("amux board ask"), "{err}");

        // Same group is untouched.
        assert_eq!(cross_group_send_ok("ts-gke", "tubescience").unwrap(), "same-group");
        // A human send carries no worker origin and is never restricted.
        assert_eq!(cross_group_send_ok("", "gtm-engine").unwrap(), "self-or-human");
        // Self-send.
        assert_eq!(cross_group_send_ok("ts-gke", "ts-gke").unwrap(), "self-or-human");
        // Documented fleet-wide routing target: bug reports to amux still work.
        assert_eq!(cross_group_send_ok("ts-gke", "amux").unwrap(), "receiver-open");
        // Sender allowlisted for that specific group.
        assert_eq!(cross_group_send_ok("broadcaster", "gtm-engine").unwrap(), "sender-allowlist");
        // ...but not for a group it did not name.
        assert!(cross_group_send_ok("broadcaster", "lonely").is_err());
        // An untagged lane shares no group with anyone — including other
        // untagged lanes. Consistent with "untagged sees itself".
        assert!(cross_group_send_ok("lonely", "gtm-engine").is_err());
    }

    /// ISOLATED (AMUX-3232): a raw-agent worker is undiscoverable to peers and
    /// cannot be targeted by a peer send, while a normal worker in the SAME group
    /// stays reachable (the negative control that lets this test fail). The owner
    /// (empty origin) is never blocked.
    #[test]
    fn isolated_worker_is_not_a_peer_send_target() {
        let dir = tempfile::tempdir().expect("tmp");
        let _g = crate::api::settings::test_env::set_home(dir.path());
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("mkdir");
        let write = |n: &str, body: &str| {
            std::fs::write(sessions.join(format!("{n}.env")), body).expect("write");
        };
        // Two same-group peers plus a same-group ISOLATED worker.
        write("caller", "CC_TAGS=\"amux\"\n");
        write("normal", "CC_TAGS=\"amux\"\n");
        write("secret", "CC_TAGS=\"amux\"\nCC_ISOLATED=\"1\"\n");

        // The flag itself: on for the isolated worker, off for the normal one
        // (the negative control: if session_is_isolated matched everything, the
        // guard below would be vacuous).
        assert!(session_is_isolated("secret"), "CC_ISOLATED=1 must read as isolated");
        assert!(!session_is_isolated("normal"), "a normal worker must NOT read as isolated");

        // A PEER send to the isolated worker is refused, and the reason names it.
        let err = cross_group_send_ok("caller", "secret").expect_err("peer send to isolated must refuse");
        assert!(err.contains("isolated"), "refusal must name the reason: {err}");

        // NEGATIVE CONTROL: the same peer, same group, reaching a NON-isolated
        // worker is allowed, proving the refusal is the isolation, not the group.
        assert_eq!(cross_group_send_ok("caller", "normal").unwrap(), "same-group");

        // The OWNER (empty origin, e.g. the dashboard) is never blocked, so
        // owner peek/send to an isolated worker still works.
        assert_eq!(cross_group_send_ok("", "secret").unwrap(), "self-or-human");
    }
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// The literal shape of a pre-cutover `*.meta.json`, copied from a live
    /// `~/.amux/sessions/` file on the machine where this incident happened.
    /// The load-bearing property is what it does NOT contain: no
    /// `rate_limited_since`. A meta blob invented for a test would have been
    /// written WITH the key and could never have caught this.
    const PRE_CUTOVER_META: &str = r#"{
        "created_at": 1785985494,
        "creator": "Mac",
        "start_count": 2,
        "last_started": 1786451118,
        "task_summary": "Reduce Verbose Output"
    }"#;

    fn pre_cutover_map() -> Map<String, Value> {
        serde_json::from_str::<Value>(PRE_CUTOVER_META)
            .unwrap()
            .as_object()
            .cloned()
            .unwrap()
    }

    /// The regression. Every lane created before the Rust cutover lacks
    /// `rate_limited_since`, so the rate-limit sweep read it on the FIRST tick
    /// after install, panicked, and the server stopped answering entirely —
    /// TCP still accepted from the kernel backlog while nothing serviced the
    /// TLS hello, which is why it presented as a hang rather than a crash.
    #[test]
    fn meta_i64_absent_key_is_zero_not_a_panic() {
        assert_eq!(meta_i64(&pre_cutover_map(), "rate_limited_since"), 0);
    }

    /// Pins the trap itself, so nobody "simplifies" `meta_i64` back into the
    /// indexing form. `load_meta` returns a `Map`, and `Map[key]` forwards to
    /// `BTreeMap::index` — it PANICS on a missing key, where the
    /// near-identical-looking `Value[key]` would have yielded `Null`. If this
    /// test ever stops panicking, serde_json changed and the comment on
    /// `meta_i64` needs revisiting.
    #[test]
    #[should_panic(expected = "no entry found for key")]
    fn map_indexing_panics_on_absent_key() {
        let _ = pre_cutover_map()["rate_limited_since"].as_i64();
    }

    /// A present key still reads back, so the fix did not trade a panic for a
    /// silent zero on the path that actually matters (a genuinely limited lane
    /// must stay flagged).
    #[test]
    fn meta_i64_reads_a_present_value() {
        let mut m = pre_cutover_map();
        m.insert("rate_limited_since".into(), json!(1786451118i64));
        assert_eq!(meta_i64(&m, "rate_limited_since"), 1786451118);
    }

    /// Feed `input` to the real generated pipe command and return the log.
    /// Runs the SHIPPED string through `sh -c`, exactly as tmux's `pipe-pane`
    /// does — not a paraphrase of what the program is believed to do.
    fn run_pipe_writer(input: &[u8], log: &Path) -> Vec<u8> {
        use std::io::Write as _;
        let cmd = log_pipe_command(log);
        let mut ch = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn writer");
        ch.stdin.take().unwrap().write_all(input).unwrap();
        let out = ch.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "writer exited {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::read(log).unwrap_or_default()
    }

    /// AMUX-2628. The incident specimen, not a convenient one: a full-screen
    /// TUI redraw stream is CARRIAGE-RETURN terminated and contains no line
    /// feed at all (measured 106,081 CR against 2,506 LF in the real
    /// `amux-frustrations.log`). The shipped writer was
    /// `for line in sys.stdin.buffer`, which blocks in `readline()` until an
    /// LF, so it wrote NOTHING while tmux reported `pane_pipe=1` and the
    /// writer process sat alive holding the file — logging looked healthy and
    /// the whole fleet's logs were frozen.
    ///
    /// This test fails against that writer (0 bytes) and passes against this
    /// one, which is the only thing that makes it worth having.
    /// AMUX-3106. `/api/scope` advertised env at ["global","group","worker"] and
    /// the UI wrote all three files, but launch sourced only the global one — so
    /// a group- or worker-scoped setting saved and changed nothing. These pin the
    /// two properties the delivery depends on: ORDER (which decides precedence,
    /// because `source` lets the last assignment win) and that a missing layer is
    /// skipped rather than sourced as an empty file.
    #[test]
    fn scope_env_layers_orders_global_then_group_then_worker() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("env")).unwrap();
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::write(home.join("amux.env"), "K=global\n").unwrap();
        std::fs::write(home.join("env").join("gtm.env"), "K=group\n").unwrap();
        std::fs::write(home.join("sessions").join("w1.env"), "CC_TAGS=gtm\nK=worker\n").unwrap();

        let got = scope_env_layers(home, "w1");
        let names: Vec<String> =
            got.iter().map(|p| p.strip_prefix(home).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["amux.env", "env/gtm.env", "sessions/w1.env"],
                   "order IS the precedence — worker must be sourced LAST");

        // And the order it produces must agree with what a gate reading one key
        // resolves, or a key means different things inside and outside the lane.
        assert_eq!(scoped_setting_in(home, "w1", "K").as_deref(), Some("worker"));
    }

    #[test]
    fn scope_env_layers_skips_absent_layers_and_untagged_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("env")).unwrap();
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::write(home.join("amux.env"), "K=global\n").unwrap();
        // Worker names a group that has NO file, and a group file exists for a
        // group the worker is not in — neither may be sourced.
        std::fs::write(home.join("env").join("other.env"), "K=other\n").unwrap();
        std::fs::write(home.join("sessions").join("w2.env"), "CC_TAGS=absent\n").unwrap();

        let got = scope_env_layers(home, "w2");
        let names: Vec<String> =
            got.iter().map(|p| p.strip_prefix(home).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["amux.env", "sessions/w2.env"]);
        assert_eq!(scoped_setting_in(home, "w2", "K").as_deref(), Some("global"));
    }

    #[test]
    fn cr_only_tui_redraws_reach_the_log_without_any_linefeed() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s.log");
        let mut input = Vec::new();
        for i in 0..200 {
            input.extend_from_slice(format!("\r\x1b[2K\x1b[36m* Photosynthesizing ({i}s)\x1b[0m").as_bytes());
        }
        assert!(!input.contains(&b'\n'), "the specimen must contain NO linefeed");

        let got = run_pipe_writer(&input, &log);
        let text = String::from_utf8_lossy(&got);
        assert!(!got.is_empty(), "CR-terminated output never reached the log");
        assert!(text.contains("Photosynthesizing (0s)"), "first frame missing: {text:.200}");
        assert!(text.contains("Photosynthesizing (199s)"), "last frame missing");
        assert!(!got.contains(&0x1b), "ANSI escapes must be stripped by default");
    }

    /// Redaction is the writer's original job and must survive the rewrite,
    /// including for a secret that arrives with only CR terminators.
    #[test]
    fn secrets_are_redacted_in_cr_terminated_output() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s.log");
        let got = run_pipe_writer(
            b"tok sk-ant-abc123DEADBEEFxyz\rANTHROPIC_API_KEY=hunter2hunter2\rghp_abcdefghijklmnopqrstuvwxyz01\r",
            &log,
        );
        let text = String::from_utf8_lossy(&got);
        assert!(!text.contains("hunter2"), "api key leaked: {text}");
        assert!(!text.contains("DEADBEEF"), "anthropic key leaked: {text}");
        assert!(!text.contains("abcdefghijklmnopqrstuvwxyz01"), "github token leaked: {text}");
        assert!(text.contains("REDACTED"), "expected a redaction marker: {text}");
    }

    /// Cursor MOVEMENT becomes whitespace rather than being deleted. Deleting
    /// it is what turned reflowed prose into "whoseoutcomeisartifacts" — the
    /// log was technically ANSI-free and still not readable.
    #[test]
    fn cursor_movement_does_not_jam_words_together() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s.log");
        let got = run_pipe_writer(b"alpha\x1b[20Gbeta\x1b[5Cgamma\r", &log);
        let text = String::from_utf8_lossy(&got);
        assert!(text.contains("alpha beta gamma"), "movement not mapped to space: {text:?}");
    }

    /// Rotation must actually roll, and must leave a marker saying so — a log
    /// that silently restarts at zero reads as "the session did nothing".
    #[test]
    fn the_log_rotates_at_the_configured_cap() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("s.log");
        // 1MB cap via the same env knob the server bakes into the command.
        std::env::set_var("AMUX_LOG_MAX_MB", "1");
        let mut input = Vec::new();
        for i in 0..40_000 {
            input.extend_from_slice(format!("line {i:06} padding-padding-padding\r").as_bytes());
        }
        let got = run_pipe_writer(&input, &log);
        std::env::remove_var("AMUX_LOG_MAX_MB");
        let rolled = dir.path().join("s.log.1");
        assert!(rolled.exists(), "no rotated generation was produced");
        assert!(
            String::from_utf8_lossy(&got).contains("amux log rotated"),
            "rotation left no marker in the live log"
        );
        assert!(got.len() < 2 * 1024 * 1024, "live log did not shrink after rotation");
    }

    /// `-o` is the flag that silently DISABLES an existing pipe (tmux closes
    /// the old pipe and then declines to open a replacement). Re-arming a
    /// running session is routine, so this must never come back.
    #[test]
    fn the_pipe_is_armed_without_the_toggle_flag() {
        let src = include_str!("session_verbs.rs");
        for line in src.lines() {
            let l = line.trim();
            if l.starts_with("//") || !l.contains("\"pipe-pane\"") {
                continue;
            }
            assert!(
                !l.contains("\"-o\""),
                "pipe-pane armed with -o, which toggles an already-piped pane OFF: {l}"
            );
        }
    }

    /// AMUX-3052, BOTH legs (gtm-engine's negative control): a drop-guard that
    /// drops everything would pass a drop-only suite, so the deliver leg is the
    /// real assertion. The discriminator is the card's LIVE status at delivery,
    /// NEVER the queue wait — GE-626 (done before delivery) drops; MS-1188
    /// (still `doing` at delivery after 578s) delivers.
    #[test]
    fn stale_pickup_voids_iff_card_left_doing_not_by_how_long_it_waited() {
        let pick =
            "[amux auto-pickup] Claimed board card GE-626 from your queue — work it now.";
        // DROP leg — card closed/moved in the claim->delivery gap.
        assert_eq!(
            pickup_stale_void("board-drive", pick, Some("done"), Some("gtm-engine"), "gtm-engine").as_deref(),
            Some("GE-626"),
            "a card that went done before delivery must be voided"
        );
        assert_eq!(
            pickup_stale_void("board-drive:reactive", pick, Some("review"), Some("gtm-engine"), "gtm-engine").as_deref(),
            Some("GE-626"),
            "the reactive pickup path voids on the same rule"
        );
        assert_eq!(
            pickup_stale_void("board-drive", pick, None, None, "gtm-engine").as_deref(),
            Some("GE-626"),
            "a card that is gone (deleted) at delivery must be voided"
        );
        // DELIVER leg — still the actionable card, however long it waited.
        assert_eq!(
            pickup_stale_void("board-drive", pick, Some("doing"), Some("gtm-engine"), "gtm-engine"),
            None,
            "a card still 'doing' at delivery MUST deliver — MS-1188's 578s wait does not matter"
        );
        // AF-135 third leg: 'doing' UNDER A DIFFERENT SESSION voids. The
        // AF-132 specimen — composed while the card was the recipient's, a
        // peer reassigned + claimed it before delivery; status-only keying
        // delivered "work it now" for a card the recipient must not touch
        // (a second lane into a file with 174 uncommitted lines in flight).
        assert_eq!(
            pickup_stale_void("board-drive", pick, Some("doing"), Some("amux"), "amux-frustrations")
                .as_deref(),
            Some("GE-626"),
            "doing under a PEER must void — the imperative would dispatch a stolen card"
        );
        // Ownership unreadable at delivery: DELIVER, same posture as the
        // transient-read-failure rule above the call site.
        assert_eq!(
            pickup_stale_void("board-drive", pick, Some("doing"), None, "gtm-engine"),
            None,
            "unknown ownership must not drop a valid pickup"
        );
        // Never void a message that is not a board-drive pickup.
        assert_eq!(
            pickup_stale_void("user", pick, Some("done"), Some("gtm-engine"), "gtm-engine"),
            None,
            "a user/inter-session message is never voided by this guard"
        );
        assert_eq!(
            pickup_stale_void("board-drive", "advance BDQ-1 or move it — a nudge, no card sentinel", Some("done"), Some("x"), "x"),
            None,
            "a board-drive NUDGE is not a single-card pickup and must deliver"
        );
    }

    /// AMUX-3557. A guarded re-enqueue must NOT reset the message's age.
    ///
    /// Delivery is `ORDER BY queued_at ASC`. The old DELETE-then-INSERT gave
    /// the replacement row `now_f64()`, so a guarded message re-queued faster
    /// than the queue drains could never be the oldest and lost to anything
    /// else in the queue forever — the guard written to protect the message is
    /// what starved it.
    ///
    /// Measured on `backend` the day this was fixed: auto-compact re-enqueued
    /// 300 times, consecutive gaps of 4 to 117 seconds, because the trigger
    /// re-evaluates on every state report and the condition cannot clear until
    /// the lane actually compacts.
    ///
    /// The ORDERING assertion is the load-bearing one. Checking only that
    /// `queued_at` is unchanged would pass against an implementation that keeps
    /// the timestamp and still hands delivery the wrong row; what the incident
    /// was actually about is which message comes out first.
    #[tokio::test]
    async fn a_guarded_re_enqueue_keeps_its_place_in_the_queue() {
        let (st, _dir) = state();

        // The guarded message is queued FIRST, so it is the oldest and must
        // stay first no matter how often it is refreshed.
        let first = steer_enqueue(&st, "lane", "/compact v1", "auto-compact", "").await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        steer_enqueue(&st, "lane", "a peer message", "", "peer").await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Re-fire the guarded one, as the auto-compact trigger does every few
        // seconds while the condition holds.
        let again = steer_enqueue(&st, "lane", "/compact v2", "auto-compact", "").await;

        assert_eq!(
            again, first,
            "a guarded re-enqueue must update the EXISTING row, so the id handed back names \
             a message that is really in the queue"
        );

        let rows: Vec<(String, String)> = {
            let conn = st.store.read().unwrap();
            let mut q = conn
                .prepare("SELECT id, text FROM steering_queue WHERE session='lane' ORDER BY queued_at ASC")
                .unwrap();
            let r = q
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .flatten()
                .collect();
            r
        };

        assert_eq!(rows.len(), 2, "the guard must replace, not stack: {rows:?}");
        assert_eq!(
            rows[0].0, first,
            "THE POINT: the refreshed guarded message must still be FIRST out. Resetting \
             queued_at put it last, forever, behind anything else queued: {rows:?}"
        );
        assert_eq!(
            rows[0].1, "/compact v2",
            "the newest TEXT wins — the guard's purpose is one pending message with current \
             content; only the AGE is preserved"
        );
        assert_eq!(rows[1].1, "a peer message");
    }

    fn state() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        (
            AppState {
                store: std::sync::Arc::new(store),
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
            },
            dir,
        )
    }

    // The column ALIGNMENT, which submit_verdict_of's unit tests cannot catch:
    // the INSERT lists 9 columns and 9 placeholders, and getting that pairing
    // wrong writes the verdict into the wrong column silently. Round-trip a
    /// AMUX-3541. `delivered_at` must not be a copy of the insert time.
    ///
    /// Both columns were `now_ms` unconditionally, so delivered_at == ts on
    /// 3854 of 3854 rows with a max gap of 0 — including every `queued` row,
    /// where a real delivery time MUST differ because queueing means waiting
    /// for a turn boundary. A column populated 100% of the time reads as a
    /// working instrument, which is why AF-159 rested "these 144 landed" on it.
    ///
    /// The queued assertion is the load-bearing one. A direct send genuinely
    /// was delivered at insert, so `Some(ts)` there is true and must stay true
    /// — asserting only the NULL half would pass against a version that
    /// blanked the column for everything, which would destroy real information
    /// rather than stop recording false information.
    #[tokio::test]
    async fn delivered_at_is_null_for_a_queued_send_and_set_for_a_direct_one() {
        let (st, _dir) = state();

        cmd_hist_record_full(&st, "lane-q", "queued one", "user", "", false,
                             DeliveryMeta::queued(11)).await;
        let (ts_q, del_q) = last_row(&st);
        assert_eq!(
            del_q, None,
            "a QUEUED message has not been delivered — stamping delivered_at here is \
             what made the column a copy of ts on every row"
        );
        assert!(ts_q > 0, "ts is still recorded; it is delivered_at that must be absent");

        cmd_hist_record_full(&st, "lane-d", "direct one", "user", "", false,
                             DeliveryMeta::direct()).await;
        let (ts_d, del_d) = last_row(&st);
        assert_eq!(
            del_d, Some(ts_d),
            "a DIRECT send really was delivered when it was recorded — blanking this too \
             would throw away true information instead of removing false information"
        );
    }

    fn last_row(st: &AppState) -> (i64, Option<i64>) {
        st.store
            .read()
            .unwrap()
            .query_row(
                "SELECT ts, delivered_at FROM cmd_history ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .unwrap()
    }

    /// AF-159. A message put on the steering queue must NOT be recorded as a
    /// direct delivery.
    ///
    /// The steer branch called a `cmd_hist_record` wrapper that hardcoded
    /// `DeliveryMeta::direct()` for a message it had just handed to
    /// `steer_enqueue`. Measured over 7 days of `type='user'` rows: 313
    /// confirmed, 144 blank, 18 retried — and the 144 blanks were not a hole in
    /// the direct path, they were STEERING wearing the wrong label. That is
    /// worse than a blank, because blank is the CORRECT value for a queued
    /// message and a hole for a direct one, so one label made the same value
    /// mean two things and the column could not be read for either.
    ///
    /// This pins the CONSTRUCTOR rather than the call site because the call
    /// site is one line in a long handler and the constructor is what any
    /// future caller reaches for. The wrapper that made the wrong thing easy is
    /// deleted, so `direct()` can no longer be applied by default.
    #[tokio::test]
    async fn a_queued_send_is_never_recorded_as_a_direct_delivery() {
        let m = DeliveryMeta::queued(4_242);
        assert!(
            matches!(m.delivery, Some(Delivery::Queued)),
            "a queued send must say queued — recording it as direct is AF-159"
        );
        assert_eq!(m.queued_at_ms, Some(4_242), "the wait has to start somewhere");
        assert_eq!(
            m.submit_verdict, None,
            "None is CORRECT here: nothing has been submitted yet. The defect was \
             never the blank, it was the blank arriving under a `direct` label"
        );

        // And the contrast that makes the label load-bearing.
        let d = DeliveryMeta::direct();
        assert!(matches!(d.delivery, Some(Delivery::Direct)));
        assert_eq!(d.queued_at_ms, None, "a direct send never waited");

        // Round-trip through the real store, because a constructor that is
        // right and a column that is wrong look identical from here.
        let (st, _dir) = state();
        cmd_hist_record_full(&st, "lane-q", "steer me", "user", "", false, DeliveryMeta::queued(7))
            .await;
        let (deliv, q_at, verdict) = st
            .store
            .read()
            .unwrap()
            .query_row(
                "SELECT delivery, queued_at, submit_verdict FROM cmd_history ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(deliv.as_deref(), Some("queued"));
        assert_eq!(q_at, Some(7));
        assert_eq!(verdict, None);
    }

    // real write through the real store (AMUX-2643).
    #[tokio::test]
    async fn a_recorded_send_round_trips_its_delivery_metadata() {
        let (st, _dir) = state();
        cmd_hist_record_full(
            &st,
            "lane-a",
            "hello",
            "user",
            "ethan@example.com",
            false,
            DeliveryMeta {
                delivery: Some(Delivery::Queued),
                queued_at_ms: Some(1_000),
                submit_verdict: Some("retried"),
            },
        )
        .await;

        let row = st
            .store
            .read()
            .unwrap()
            .query_row(
                "SELECT session, type, origin, delivery, queued_at, submit_verdict \
                 FROM cmd_history ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .expect("the row must exist");
        assert_eq!(row.0, "lane-a");
        assert_eq!(row.1, "user");
        assert_eq!(row.2, "ethan@example.com");
        assert_eq!(row.3.as_deref(), Some("queued"));
        assert_eq!(row.4, Some(1_000));
        assert_eq!(row.5.as_deref(), Some("retried"));
    }

    // NULL must survive as NULL. Coalescing it to a verdict would turn "we did
    // not look" into "we looked and could not confirm" — inventing a fact.
    #[tokio::test]
    async fn an_unverified_path_records_null_not_a_guess() {
        let (st, _dir) = state();
        cmd_hist_record_full(&st, "lane-b", "hi", "user", "", false, DeliveryMeta::direct()).await;
        let v: Option<String> = st
            .store
            .read()
            .unwrap()
            .query_row(
                "SELECT submit_verdict FROM cmd_history ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, None, "an unverified path must record NULL, not a verdict");
    }

    // AMUX-3071: the send path lost Python's _autotask_from_command at the
    // 792ce1f cutover (2026-08-09), so 330 human prompts recorded card_id=NULL
    // and left no board trace. A real task prompt must now mint a `doing` card
    // and stamp cmd_history.card_id; steering / [no-board] / inter-session must
    // not. This test would have been RED for the whole regression window.
    #[tokio::test]
    async fn a_human_prompt_auto_captures_and_links_a_ledger_card() {
        let (st, _dir) = state();
        let q = |sql: &'static str, s: &'static str| -> Option<String> {
            st.store
                .read()
                .unwrap()
                .query_row(sql, rusqlite::params![s], |r| r.get(0))
                .unwrap()
        };

        // 1. A real task prompt mints a card and links it.
        cmd_hist_record_full(
            &st, "lane-cap", "Refactor the settings sidebar into a tabbed page",
            "user", "", false, DeliveryMeta::direct(),
        )
        .await;
        let card_id = q(
            "SELECT card_id FROM cmd_history WHERE session=?1 ORDER BY id DESC LIMIT 1",
            "lane-cap",
        )
        .expect("a real task prompt must link a board card");
        let (sess, status): (String, String) = st
            .store
            .read()
            .unwrap()
            .query_row(
                "SELECT session, status FROM issues WHERE id=?1",
                rusqlite::params![card_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the minted card must exist");
        assert_eq!(sess, "lane-cap");
        assert_eq!(status, "doing", "capture mints in doing, not todo (AMUX-2613)");

        // 2. A SECOND prompt to a lane that now holds an open card is STEERING,
        //    not a new task: no card, card_id stays NULL, still exactly one card.
        cmd_hist_record_full(
            &st, "lane-cap", "Also make the tabs keyboard-navigable please",
            "user", "", false, DeliveryMeta::direct(),
        )
        .await;
        assert!(
            q("SELECT card_id FROM cmd_history WHERE session=?1 ORDER BY id DESC LIMIT 1", "lane-cap")
                .is_none(),
            "a lane with an open card is steered, not re-carded"
        );
        let n: i64 = st
            .store
            .read()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM issues WHERE session='lane-cap'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "exactly one card for the lane");

        // 3. [no-board] (skip_board=true) mints nothing.
        cmd_hist_record_full(
            &st, "lane-nb", "Do a big refactor of the whole module right now",
            "user", "", true, DeliveryMeta::direct(),
        )
        .await;
        assert!(
            q("SELECT card_id FROM cmd_history WHERE session=?1 ORDER BY id DESC LIMIT 1", "lane-nb")
                .is_none(),
            "[no-board] mints no card"
        );

        // 4. Inter-session ("session") messages are not the recipient's task.
        cmd_hist_record_full(
            &st, "lane-x", "Coordinate the rollout with the other lane and report back",
            "session", "peer-lane", false, DeliveryMeta::direct(),
        )
        .await;
        assert!(
            q("SELECT card_id FROM cmd_history WHERE session=?1 ORDER BY id DESC LIMIT 1", "lane-x")
                .is_none(),
            "inter-session messages must not spam the board"
        );
    }

    // AMUX-3330: a pure status query is answered inline and produces no
    // deliverable, so it must NOT mint a board work card (the old capture minted
    // type=code/doing, which a question can never take through the gate and which
    // held the WIP slot until a manual discard). It STAYS in cmd_history — the
    // Messages ledger keeps every prompt. This tests the shipped path, not just
    // the detector (ethos rule 7).
    #[tokio::test]
    async fn a_status_query_records_in_history_but_mints_no_card() {
        let (st, _dir) = state();
        cmd_hist_record_full(
            &st, "lane-sq", "status on MSG-29602?",
            "user", "", false, DeliveryMeta::direct(),
        )
        .await;
        let card_id: Option<String> = st
            .store
            .read()
            .unwrap()
            .query_row(
                "SELECT card_id FROM cmd_history WHERE session=?1 ORDER BY id DESC LIMIT 1",
                rusqlite::params!["lane-sq"],
                |r| r.get(0),
            )
            .unwrap();
        assert!(card_id.is_none(), "a status query must not mint a work card (AMUX-3330)");
        let cards: i64 = st
            .store
            .read()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM issues WHERE session='lane-sq'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cards, 0, "no board card for a status query");
        let msgs: i64 = st
            .store
            .read()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM cmd_history WHERE session='lane-sq'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msgs, 1, "the prompt is still recorded in cmd_history (Messages ledger)");
    }

    // The stale-pickup guard (AMUX-3052) keys on the EXACT template board_drive.rs
    // mints. This pins that the parser finds the id (anchor + tail both precede
    // the "— work it now" separator, so the dash is irrelevant) and returns None
    // for anything that is not a pickup — a non-pickup steering message must never
    // be voided. If the template is reworded and this test is not, the guard goes
    // dark; that is what this catches.
    #[test]
    fn pickup_card_id_parses_the_template() {
        let real = "[amux auto-pickup] Claimed board card GV-648 from your queue - work it now. \
                    Anything quoted below is the CARD's stored text";
        assert_eq!(pickup_card_id(real).as_deref(), Some("GV-648"));
        assert_eq!(pickup_card_id("hey, can you rebase and push?"), None);
        assert_eq!(pickup_card_id("[Ethan] decision on AMUX-1: APPROVED"), None);
        assert_eq!(pickup_card_id("Claimed board card  from your queue"), None);
    }

    // AMUX-3052: a queued auto-pickup for a card CLOSED after the atomic claim
    // must be VOIDED at the delivery boundary, not dispatched as "work it now"
    // (which makes the lane redo finished work). gtm-engine's GE-626 shape:
    // claimed while 'doing', closed in the queue gap, delivered 18.7s later. This
    // drops that exact shape; a still-'doing' pickup is left untouched. Would have
    // been RED before the guard existed (the row would have stayed for delivery).
    #[tokio::test]
    async fn a_stale_auto_pickup_is_voided_at_the_delivery_boundary() {
        let (st, _dir) = state();
        let now = now_f64();
        let _ = st
            .store
            .write_async(move |conn| {
                ensure_fleet_tables(conn)?;
                // Claimed to 'doing', then CLOSED by the owner (the GE-626 shape).
                conn.execute(
                    "INSERT INTO issues (id, title, status, session, created, updated) \
                     VALUES ('GV-648','t','done','lane-stale',0,0)",
                    [],
                )?;
                // Control: still legitimately 'doing'.
                conn.execute(
                    "INSERT INTO issues (id, title, status, session, created, updated) \
                     VALUES ('AMUX-9','t','doing','lane-live',0,0)",
                    [],
                )?;
                let stale =
                    "[amux auto-pickup] Claimed board card GV-648 from your queue - work it now.";
                let live =
                    "[amux auto-pickup] Claimed board card AMUX-9 from your queue - work it now.";
                conn.execute(
                    "INSERT INTO steering_queue(id, session, text, queued_at, guard) \
                     VALUES('s1','lane-stale',?1,?2,'board-drive')",
                    rusqlite::params![stale, now - 20.0],
                )?;
                conn.execute(
                    "INSERT INTO steering_queue(id, session, text, queued_at, guard) \
                     VALUES('s2','lane-live',?1,?2,'board-drive')",
                    rusqlite::params![live, now - 20.0],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await;

        steer_deliver_tick(&st).await;

        let conn = st.store.read().unwrap();
        let stale_queued: i64 = conn
            .query_row("SELECT COUNT(*) FROM steering_queue WHERE id='s1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stale_queued, 0, "the stale pickup must leave the queue, not be delivered");
        let voided: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM steering_history WHERE id='s1' AND text LIKE '[VOIDED:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(voided, 1, "the stale pickup must be recorded as voided");
        let live_voided: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM steering_history WHERE id='s2' AND text LIKE '[VOIDED:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live_voided, 0, "a still-'doing' pickup must never be voided");
    }

    /// AMUX-3052 fail-open SIGNAL (gtm-engine's blind spot): a DB read error at
    /// the stale-check makes the guard DELIVER (fail-open — dropping a valid pickup
    /// is worse than a spurious one), but it must still EMIT, or a stale pickup let
    /// through on a degraded DB is invisible and the void rate falls exactly when
    /// the guard has stopped running. No prod DB error can honestly exercise this,
    /// so this is the only place the emission is proven to fire. Induces the error
    /// by dropping the table the check reads, then asserts the delivered=true
    /// check-failed event landed AND the row was not voided.
    #[tokio::test]
    async fn a_read_error_at_the_stale_check_signals_and_still_delivers() {
        let (st, _dir) = state();
        let now = now_f64();
        let _ = st
            .store
            .write_async(move |conn| {
                ensure_fleet_tables(conn)?;
                let pick =
                    "[amux auto-pickup] Claimed board card BET-6 from your queue - work it now.";
                conn.execute(
                    "INSERT INTO steering_queue(id, session, text, queued_at, guard) \
                     VALUES('r1','lane-x',?1,?2,'board-drive')",
                    rusqlite::params![pick, now - 20.0],
                )?;
                // Make the stale-check's `SELECT status FROM issues` fail with an
                // Err (not NoRows) — the fail-open path.
                conn.execute("DROP TABLE issues", [])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await;

        steer_deliver_tick(&st).await;

        let conn = st.store.read().unwrap();
        let signalled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_events WHERE type='message.voided' \
                 AND data LIKE '%pickup-check-failed%' AND data LIKE '%\"delivered\":true%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            signalled, 1,
            "a read error on the stale-check must emit a delivered=true check-failed event, not vanish"
        );
        let voided: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM steering_history WHERE id='r1' AND text LIKE '[VOIDED:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(voided, 0, "fail-open: a read error must deliver, never void");
    }

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    #[test]
    fn env_file_roundtrip_preserves_order_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.env");
        std::fs::write(&p, "# updated: old\nCC_DIR=\"/tmp/a b\"\nCC_TAGS='x, y'\nCC_DESC=plain\n").unwrap();
        let mut e = EnvFile::load(&p);
        assert_eq!(e.get("CC_DIR"), Some("/tmp/a b"));
        assert_eq!(e.get("CC_TAGS"), Some("x, y"));
        assert_eq!(e.get("CC_DESC"), Some("plain"));
        e.set("CC_NEW", "v");
        e.write(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        // Key order preserved, new key appended, header present.
        let d = text.find("CC_DIR").unwrap();
        let t = text.find("CC_TAGS").unwrap();
        let n = text.find("CC_NEW").unwrap();
        assert!(d < t && t < n, "{text}");
        assert!(text.starts_with("# updated: "), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn flag_helpers_match_python_semantics() {
        // strip --model both forms, preserve the rest, re-quoted.
        assert_eq!(strip_model_from_flags("--model opus --effort high").unwrap(), "--effort high");
        assert_eq!(strip_model_from_flags("--model=opus -x").unwrap(), "-x");
        // The [1m] model ids must round-trip shell-safe (py:22735 rationale).
        let f = shell_quote_flags("--model claude-opus-4-6[1m]");
        assert_eq!(f, "--model 'claude-opus-4-6[1m]'");
        assert_eq!(extract_model_from_flags(&f), "claude-opus-4-6[1m]");
        // Unbalanced quote errs (never silently wipes flags).
        assert!(strip_model_from_flags("--model 'oops").is_err());
        // effort set/clear.
        assert_eq!(set_effort_flag("--model opus", "high").unwrap(), "--model opus --effort high");
        assert_eq!(set_effort_flag("--model opus --effort low", "").unwrap(), "--model opus");
        // yolo strip covers --approval-mode yolo.
        assert_eq!(strip_provider_yolo_flags("--yolo --model auto"), "--model auto");
        assert_eq!(strip_provider_yolo_flags("--approval-mode yolo -x"), "-x");
        // Value-aware strip: removing a boolean flag never eats its neighbour.
        assert_eq!(
            strip_token_from_flags("--dangerously-skip-permissions --model opus", "--dangerously-skip-permissions")
                .unwrap(),
            "--model opus"
        );
        // A trailing bare flag is stripped rather than kept dangling.
        assert_eq!(strip_token_from_flags("--model opus --effort", "--effort").unwrap(), "--model opus");
    }

    /// Pins the 2026-08-09 incident: defaults.env carried `--model
    /// claude-opus-4-6`, the session carried its own model, and the naive
    /// concat launched `claude --model claude-opus-4-6 --model claude-fable-5
    /// ...` fleet-wide. Defaults lose; the session's flag is the only one.
    #[test]
    fn duplicate_model_incident_defaults_lose_session_wins() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let cfg = EnvFile::default();
        // The exact incident shape (flags from amux.env, defaults from
        // defaults.env, --name from the fresh-start choreography).
        let cmd = build_claude_cmd(
            &cfg,
            "--model claude-fable-5 --dangerously-skip-permissions",
            "--model claude-opus-4-6",
            "--name amux",
            "",
        );
        assert_eq!(cmd.matches("--model").count(), 1, "{cmd}");
        assert!(cmd.contains("--model claude-fable-5"), "{cmd}");
        assert!(!cmd.contains("claude-opus-4-6"), "{cmd}");
        assert!(cmd.contains("--dangerously-skip-permissions"), "{cmd}");
        assert!(cmd.contains("--name amux"), "{cmd}");
        // Generic by token name: --effort/--max-tokens defaults fall to the
        // session's values; defaults the session does not override survive.
        let cmd2 = build_claude_cmd(
            &cfg,
            "--effort high --model opus",
            "--model sonnet --effort low --max-tokens 4096 --verbose",
            "--name s",
            "",
        );
        assert_eq!(cmd2.matches("--model").count(), 1, "{cmd2}");
        assert_eq!(cmd2.matches("--effort").count(), 1, "{cmd2}");
        assert!(cmd2.contains("--effort high") && cmd2.contains("--model opus"), "{cmd2}");
        assert!(cmd2.contains("--max-tokens 4096") && cmd2.contains("--verbose"), "{cmd2}");
        // Boolean dedupe must not eat the default's neighbouring flag.
        let cmd3 = build_claude_cmd(
            &cfg,
            "--dangerously-skip-permissions --model fable",
            "--dangerously-skip-permissions --model claude-opus-4-6",
            "",
            "",
        );
        assert_eq!(cmd3.matches("--dangerously-skip-permissions").count(), 1, "{cmd3}");
        assert_eq!(cmd3.matches("--model").count(), 1, "{cmd3}");
        assert!(cmd3.contains("--model fable"), "{cmd3}");
        // --model= eq form dedupes the same.
        let cmd4 = build_claude_cmd(&cfg, "--model=fable", "--model claude-opus-4-6", "", "");
        assert_eq!(cmd4.matches("--model").count(), 1, "{cmd4}");
        assert!(!cmd4.contains("claude-opus-4-6"), "{cmd4}");
    }

    /// Pins the 2026-08-09 cross-link: on a shared work dir the newest jsonl
    /// belonged to amux-rust, and an unguarded latest-id adoption stamped the
    /// amux session with a NEIGHBOUR's live conversation. The guard must name
    /// the sibling owner (blocking adoption) and stay silent for the owner
    /// itself.
    #[test]
    fn conv_adoption_guard_blocks_neighbour_latest() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        std::fs::write(env_path("rusty"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(
            meta_path("rusty"),
            json!({"cc_conversation_id": "1dd2cd21-c4a7-46b9-9b97-51fccbe721a2"}).to_string(),
        )
        .unwrap();
        assert_eq!(
            conversation_owned_by_other("1dd2cd21-c4a7-46b9-9b97-51fccbe721a2", "amuxy"),
            "rusty",
            "a sibling's conversation must be reported as owned"
        );
        assert!(
            conversation_owned_by_other("1dd2cd21-c4a7-46b9-9b97-51fccbe721a2", "rusty").is_empty(),
            "the owner itself is never blocked"
        );
    }

    #[test]
    fn detectors_read_real_frames() {
        let claude_idle = "some output\n\u{276f} \n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert!(claude_ui_visible(claude_idle));
        assert!(!at_shell_prompt(claude_idle));
        // AMUX-3055: the DEFAULT footer (no --dangerously-skip-permissions) is
        // "manual mode on · ? for shortcuts", NOT the bypass footer. This frame
        // is a real capture from a modal-created worker and returned false from
        // the old detector, so send_after_ready dropped its start prompt. The
        // assertion fails against that old detector, which is the point.
        let claude_manual = "some output\n\u{276f} Try \"fix typecheck errors\"\n────\n⏸ manual mode on · ? for shortcuts · ← 2 agents";
        assert!(claude_ui_visible(claude_manual), "manual-mode idle UI must read as visible");
        assert!(!at_shell_prompt(claude_manual));
        let shell = "Last login: Sat\nmixpeek$ ";
        assert!(!claude_ui_visible(shell));
        assert!(at_shell_prompt(shell));
        // Spinner = active; prompt-glyph lines never count as chrome.
        let active = "\u{273b} Crunching\u{2026} (12s)\n\u{276f} typed text";
        assert_eq!(detect_claude_status(active), "active");
        let echoed = "\u{276f} [amux] VERIFY \u{2014} y\u{2026}\nmixpeek$ ";
        assert_ne!(detect_claude_status(echoed), "active");
        // Resume picker needs the ⌕ search glyph.
        assert!(at_resume_picker("Resume Session \u{2315}\nEnter to select"));
        assert!(!at_resume_picker("Enter to select"));
    }

    /// Codex's trust-directory picker, byte shape captured live 2026-08-11
    /// (AMUX-2913): selector cursor is `›` (U+203A), not Claude's `❯`, so a
    /// lane blocked on it read `idle` — needs-input invisible, the AMUX-2834
    /// class on a second provider. The control half: prose QUOTING a numbered
    /// list with the same cursor but no footer hint must not read as waiting
    /// (the AMUX-2642 self-block class).
    #[test]
    fn a_codex_picker_is_waiting_not_idle() {
        let trust = "> You are in /Users/ethan/Dev/board-exp3\n\
             Do you trust the contents of this directory? Working with untrusted contents comes with higher risk of prompt injection.\n\
             \u{203a} 1. Yes, continue\n  2. No, quit\n  Press enter to continue";
        assert_eq!(detect_claude_status(trust), "waiting");
        let quoted = "the doc says codex renders \u{203a} 1. Yes, continue as its cursor";
        assert_ne!(detect_claude_status(quoted), "waiting");
        // Gemini, captured live the same day: `●` cursor in a `│` box.
        let gemini = " \u{2502} \u{25cf} 1. Yes\n \u{2502}   2. Yes, and remember the directories as trusted\n \u{2502}   3. No";
        assert_eq!(detect_claude_status(gemini), "waiting");
        let gemini_prose = "gemini renders \u{25cf} 1. Yes as its cursor";
        assert_ne!(detect_claude_status(gemini_prose), "waiting");
        // Gemini mid-tool-turn, live frame: braille spinner = ACTIVE. This
        // read as waiting for an entire 20s shell run (a stale picker stamp
        // showed through because nothing recognised the spinner).
        let gemini_working = "\u{2502} \u{22b7}  Shell sleep 15 && echo ROUND2\n \u{2819} Thinking... (esc to cancel, 9s)\n YOLO Ctrl+Y";
        assert_eq!(detect_claude_status(gemini_working), "active");
    }

    /// A 5xx / Overloaded banner sitting at the TAIL is its own status
    /// (Ethan 2026-08-18, @backend). Claude Code ends the turn on a 529 and
    /// returns to the prompt, so `detect_claude_status` reads `idle` — the
    /// exact frame @backend showed while parked on the error. `has_current_api_error`
    /// is the discriminator the status field needs, and the negatives are what
    /// keep it from painting healthy lanes: a normal working frame, and a lane
    /// that hit a 529 EARLIER but has since produced newer output (banner no
    /// longer in the tail) must both read false.
    #[test]
    fn a_tail_5xx_banner_reads_as_api_error_and_a_recovered_lane_does_not() {
        // The live @backend frame (⏺ = U+23FA), the whole reason this exists.
        let stuck = "\u{23fa} Running the migration\n\
             \u{23fa} API Error: 529 Overloaded. This is a server-side issue, usually temporary \u{2014} try again in a moment...\n\
             \u{276f} ";
        assert!(has_current_api_error(stuck), "a 5xx banner in the tail must read as api_error");
        // …and detect_claude_status calls this lane idle, which is the bug.
        assert_eq!(detect_claude_status(stuck), "idle");
        // 503 with no glyph, banner is the last meaningful line.
        let bare = "some earlier output\nAPI Error: 503 Service Unavailable\n\u{276f} ";
        assert!(has_current_api_error(bare));
        // NEGATIVE: an ordinary working frame — no banner at all.
        let working = "\u{273b} Crunching\u{2026} (12s)\n\u{276f} typed text";
        assert!(!has_current_api_error(working));
        // NEGATIVE: recovered — the 529 happened but ≥8 newer non-empty lines
        // followed, so the banner is out of the tail window and the lane is not
        // stuck on it any more.
        let recovered = "\u{23fa} API Error: 529 Overloaded. try again in a moment...\n\
             \u{23fa} Retrying\n\u{23fa} Wrote file a\n\u{23fa} Wrote file b\n\u{23fa} Wrote file c\n\
             \u{23fa} Wrote file d\n\u{23fa} Wrote file e\n\u{23fa} Wrote file f\n\u{23fa} Done\n\u{276f} ";
        assert!(!has_current_api_error(recovered), "a lane that moved past the 529 must not read as errored");
        // NEGATIVE: a 4xx is a client error, not the transient-retryable class
        // this surfaces (and prose merely quoting the string must not match).
        let client_err = "\u{23fa} API Error: 400 Bad Request\n\u{276f} ";
        assert!(!has_current_api_error(client_err));
        let prose = "the docs mention an API Error: 500 sometimes appears";
        assert!(!has_current_api_error(prose), "prose quoting the phrase mid-line must not match (anchored)");
    }

    /// AMUX-3054: an empty send is the user pressing "Enter" at a picker. The
    /// server must ACCEPT the highlighted option with an Enter KEYPRESS, not
    /// paste the ❯ label as text (which a picker reading key events swallows,
    /// so the Enter never lands). The load-bearing decision is the gate
    /// `detect_claude_status(pane) == "waiting" && !is_rate_limit_menu(pane)`;
    /// this asserts the discriminator directly against real picker shapes, so a
    /// regression that reclassifies a selector cannot pass green.
    #[test]
    fn empty_send_at_a_selector_takes_the_enter_path() {
        // Predicate under test: the exact gate shipped in send_text_inner.
        let picker_enter = |pane: &str| detect_claude_status(pane) == "waiting" && !is_rate_limit_menu(pane);

        // AskUserQuestion review/submit screen (AMUX-2952 shape): numbered, no
        // "enter to select" footer. Must take the Enter path.
        let submit = "  Here is my plan.\n\u{2502} \u{276f} 1. Submit answers\n\u{2502}   2. Edit answer 1";
        assert!(picker_enter(submit), "numbered submit screen must press Enter");

        // A FOOTERED selector: previously the footer guard returned "no
        // suggestion found" and a composer empty-send (no client fallback) did
        // nothing. Now it presses Enter here on the server, before that guard.
        let footered = "Do you want to proceed?\n\u{276f} 1. Yes\n  2. No\n  \u{2191}\u{2193} to navigate \u{00b7} enter to select";
        assert!(picker_enter(footered), "footered selector must press Enter, not no-op");

        // A rate-limit menu is a "waiting" selector too, but it owns a dedicated
        // handler that STAMPS credit_limited before pressing 1 (AMUX-2820).
        // Excluding it here keeps that stamp, so the gate must NOT fire.
        let ratelimit = "What do you want to do?\n\u{276f} 1. Stop and wait for limit to reset\n  2. Switch to usage credits\n  3. Switch to Team plan";
        assert_eq!(detect_claude_status(ratelimit), "waiting");
        assert!(!picker_enter(ratelimit), "rate-limit menu must fall through to its own handler");

        // CONTROL: an IDLE composer showing a genuine suggested prompt is NOT a
        // selector, so it must stay on the text path and get the suggestion
        // typed and submitted, not turned into a bare Enter.
        let idle_suggestion = "\u{276f} retry the failing test\n────\n\u{23f8} manual mode on \u{00b7} ? for shortcuts";
        assert_ne!(detect_claude_status(idle_suggestion), "waiting");
        assert!(!picker_enter(idle_suggestion), "an idle suggested prompt must NOT be treated as a picker");
    }

    // ---- AMUX-2612: session identity survives a resume --------------------
    #[test]
    fn resume_carries_the_session_name() {
        // The defect: resume produced ONLY `--resume <id>`, so a renamed
        // session's harness kept its birth name forever.
        let f = claude_session_flag("amux", "1dd2cd21-c4a7-46b9-9b97-51fccbe721a2", true);
        assert!(f.contains("--resume 1dd2cd21-c4a7-46b9-9b97-51fccbe721a2"), "{f}");
        assert!(f.contains("--name amux"), "resume dropped the session name: {f}");
        // Fresh start is unchanged: --name only, never a bare/empty --resume.
        let fresh = claude_session_flag("amux", "", false);
        assert_eq!(fresh, "--name amux");
        // A stale conv id (file gone) must not smuggle --resume in.
        let stale = claude_session_flag("amux", "1dd2cd21-c4a7-46b9-9b97-51fccbe721a2", false);
        assert_eq!(stale, "--name amux", "unresumable id must fall back to fresh: {stale}");
        // Names are quoted, not interpolated raw.
        assert!(claude_session_flag("a b", "x", false).contains("--name 'a b'"));

        // ...and it SURVIVES the splice. Testing the flag string alone would
        // not catch build_claude_cmd's dedupe eating it, which is the same
        // class of bug as the duplicate-model incident below.
        let home = tempfile::tempdir().unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let cmd = build_claude_cmd(
            &EnvFile::default(),
            "--model claude-fable-5 --dangerously-skip-permissions",
            "",
            &claude_session_flag("amux", "1dd2cd21-c4a7-46b9-9b97-51fccbe721a2", true),
            "",
        );
        assert!(cmd.contains("--resume 1dd2cd21-c4a7-46b9-9b97-51fccbe721a2"), "{cmd}");
        assert!(cmd.contains("--name amux"), "the splice dropped the name: {cmd}");
        assert_eq!(cmd.matches("--name").count(), 1, "duplicated --name: {cmd}");
        assert_eq!(cmd.matches("--resume").count(), 1, "duplicated --resume: {cmd}");
    }

    #[test]
    fn transcript_display_name_takes_the_latest_record_not_the_first() {
        // A claude transcript is append-only: `--name` writes a NEW record and
        // line 0 keeps the birth name for the life of the file. Reading line 0
        // is what made the writer-side fix invisible.
        let dir = tempfile::tempdir().unwrap();
        let jf = dir.path().join("conv.jsonl");
        std::fs::write(
            &jf,
            "{\"customTitle\":\"amux-rust\",\"type\":\"summary\"}\n\
             {\"type\":\"user\"}\n\
             {\"customTitle\":\"amux-rust\",\"type\":\"assistant\"}\n\
             {\"customTitle\":\"amux\",\"type\":\"assistant\"}\n",
        )
        .unwrap();
        assert_eq!(
            transcript_display_name(&jf).as_deref(),
            Some("amux"),
            "the RENAMED name must win; line 0 is the dead one"
        );

        // Fallback: no name record inside the tail window still resolves from
        // line 0, so a short or long-silent transcript is not made nameless.
        let only_first = dir.path().join("first.jsonl");
        std::fs::write(&only_first, "{\"customTitle\":\"solo\"}\n{\"type\":\"user\"}\n").unwrap();
        assert_eq!(transcript_display_name(&only_first).as_deref(), Some("solo"));

        // sessionName is honoured alongside customTitle (both are matched by
        // the resolver this feeds).
        let alt = dir.path().join("alt.jsonl");
        std::fs::write(&alt, "{\"sessionName\":\"viaSessionName\"}\n").unwrap();
        assert_eq!(transcript_display_name(&alt).as_deref(), Some("viaSessionName"));

        // No name anywhere is None, not "".
        let none = dir.path().join("none.jsonl");
        std::fs::write(&none, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(transcript_display_name(&none), None);
    }

    /// AF-128. The geometry parse must reject every shape tmux produces when it
    /// CANNOT answer, because a wrong number here is worse than no number: the
    /// whole point of the field is to let a reader tell a narrow pane from a
    /// narrow render, and a fabricated 0 would answer that question falsely.
    #[test]
    fn pane_geometry_parses_tmux_output_and_rejects_every_non_answer() {
        assert_eq!(parse_pane_geometry("220x50\n"), Some((220, 50)));
        assert_eq!(parse_pane_geometry(" 80x24 "), Some((80, 24)));
        assert_eq!(parse_pane_geometry("50x50"), Some((50, 50)));

        // Dead/missing session: tmux writes nothing to stdout.
        assert_eq!(parse_pane_geometry(""), None);
        assert_eq!(parse_pane_geometry("\n"), None);
        // Unknown format specifier: tmux echoes the literal back.
        assert_eq!(parse_pane_geometry("#{window_width}x#{window_height}"), None);
        // Partial or malformed answers.
        assert_eq!(parse_pane_geometry("220"), None);
        assert_eq!(parse_pane_geometry("220x"), None);
        assert_eq!(parse_pane_geometry("x50"), None);
        // Zero is tmux failing to answer, not a pane. Reported as absent.
        assert_eq!(parse_pane_geometry("0x50"), None);
        assert_eq!(parse_pane_geometry("220x0"), None);
        assert_eq!(parse_pane_geometry("-1x50"), None);
    }

    #[test]
    fn peek_text_utils_hold_their_contracts() {
        assert_eq!(collapse_blank_runs("a\n\n\n\nb"), "a\n\nb");
        let noise = "unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT; cd /x; claude --model sonnet --dangerously-skip-permissions --name s\nreal content";
        assert_eq!(strip_launch_noise(noise), "real content");
        // All-scaffolding frame returns unchanged, never blanks the peek.
        let only_noise = "claude --resume abc --name s";
        assert_eq!(strip_launch_noise(only_noise), only_noise);
        // AMUX-2612: a RESUMED pane's launch line carries --resume and (before
        // this card) no --name. Both fixtures above contain "--name", so the
        // pre-filter's missing --resume arm could not fail on either — the
        // whole boot command line reached the state heuristics unstripped.
        let resumed = "unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT; cd /x; claude --model sonnet --dangerously-skip-permissions --resume 0a1b2c3d\nreal content";
        assert_eq!(
            strip_launch_noise(resumed),
            "real content",
            "resume-shaped launch noise must be stripped like the --name shape"
        );
        assert_eq!(
            strip_scroll_pill("before Jump to bottom (click) ↓ after"),
            "before after"
        );
        // trim_live_overlap: ≥3 matching lines trims through the last one.
        let transcript = "alpha line one long enough\nbeta line two long enough\ngamma line three long enough";
        let live = format!("{transcript}\nfresh tail");
        assert_eq!(trim_live_overlap(transcript, &live), "fresh tail");
        // <3 matches keeps the frame whole.
        assert_eq!(trim_live_overlap("only one line here long enough", "x\ny"), "x\ny");
    }

    #[test]
    fn transcript_md_render_basics() {
        let out = md_to_ansi("# Head\n**bold** and `code`");
        assert!(out.contains("\x1b[1mHead\x1b[22m"), "{out:?}");
        assert!(out.contains("\x1b[1mbold\x1b[22m"), "{out:?}");
        assert!(out.contains("\x1b[38;5;153mcode"), "{out:?}");
        let table = md_to_ansi("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(table.contains('\u{250c}') && table.contains('\u{2502}'), "{table}");
    }

    #[test]
    fn redaction_matches_python_pattern_family() {
        assert_eq!(redact_secrets("key sk-ant-abc123XYZ done"), "key SECRET_REDACTED done");
        assert_eq!(redact_secrets("mxp_sk_deadbeef"), "mxp_sk_REDACTED");
        assert_eq!(
            redact_secrets("ANTHROPIC_API_KEY=sk-live-x y"),
            "ANTHROPIC_API_KEY=REDACTED y"
        );
    }

    /// The file/DB-backed verbs, exercised through the full router shape on a
    /// hermetic fleet home — the same dispatch the live composition mounts.
    /// THE INVARIANT the badge bug broke: the value the sessions payload ships
    /// must be the value the toggle acts on. The SPA used to derive its own,
    /// ORing in `auto_continue` — which the payload fills from
    /// `standing_orders_on`, DEFAULT-ON at every level — so a lane with no
    /// skip-permissions flag badged YOLO and then stopped to ask for approval.
    ///
    /// The control is the third case: default-on standing orders must NOT read
    /// as YOLO. Without it this test passes against the very bug it exists for.
    #[test]
    fn yolo_verdict_ignores_default_on_standing_orders() {
        use super::yolo_enabled;
        // A real bypass flag is YOLO.
        assert!(yolo_enabled("--model opus --dangerously-skip-permissions", None));
        // An EXPLICIT CC_AUTO_CONTINUE=1 is YOLO — a worker that never stops
        // implies skip-permissions, which is the documented intent.
        assert!(yolo_enabled("--model opus", Some("1")));
        // CONTROL — the reported bug. No flag, and auto-continue merely
        // default-on (absent from the env, which is what default-on looks like
        // to this function): NOT yolo.
        assert!(
            !yolo_enabled("--model opus", None),
            "a lane with no bypass flag must not read as YOLO — this is the \
             personal-planner case that sat blocked on an approval prompt for 11h"
        );
        // And an explicit off is not yolo either.
        assert!(!yolo_enabled("--model opus", Some("0")));
    }

    /// A rust-managed worker keeps the modern pointer for every verb EXCEPT
    /// `peek` and `send`, which are implemented here and nowhere else.
    ///
    /// The regression this pins: `/api/workers` mounts `/`, `/{id}`,
    /// `/{id}/start`, `/{id}/stop`, `/{id}/peek` — no `send` — so the blanket
    /// guard answered 501 "use /api/workers" for the only send implementation
    /// there is, naming the path that had just refused. Everything the
    /// dashboard does is `/api/sessions/<name>/<verb>`, so peek rendered blank
    /// and no prompt reached any worker in the store; `amux send` posts to
    /// `/api/workers/<name>/send`, hit the same wall, and treated the `error`
    /// field as a considered refusal rather than retrying.
    ///
    /// Both spellings, because the two routes share this dispatcher and an
    /// allow-list that covered one of them would be a coin flip for the CLI.
    #[tokio::test]
    async fn rust_worker_pointer_exempts_peek_and_send() {
        use amux_core::provider::ProviderId;
        use amux_core::session::BackendId;
        use amux_core::worker::WorkerConfig;
        use crate::db::queries::WorkerRow;
        use crate::db::WriteOutcome;

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (state, _dir) = state();

        let id = WorkerId::from_ulid(ulid::Ulid::new());
        let config = WorkerConfig {
            display_name: "hw".into(),
            name_aliases: Vec::new(),
            cwd: "/tmp".into(),
            provider: ProviderId::new("claude"),
            model: None,
            backend: BackendId::from("herdr"),
            environment: Default::default(),
            permissions: Vec::new(),
            group: None,
        };
        let row = WorkerRow::new(&id, &config, &chrono::Utc::now().to_rfc3339());
        state
            .store
            .write(move |conn| {
                crate::db::queries::insert_worker(conn, &row)?;
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .expect("worker insert");

        let app: Router = routes().with_state(state);

        // NO env file for `hw`: peek and send reach their real handler, which
        // applies its OWN session check. 404 is the proof the guard let go —
        // and it keeps the test from auto-waking a session and launching a
        // terminal on the machine running it.
        for (method, path) in [
            ("GET", "/api/sessions/hw/peek"),
            ("POST", "/api/sessions/hw/send"),
            ("GET", "/api/workers/hw/peek"),
            ("POST", "/api/workers/hw/send"),
        ] {
            let body = (method == "POST").then(|| json!({"text": "hi"}));
            let (st, v) = call(&app, method, path, body).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{method} {path} -> {v}");
            assert_eq!(v["error"], json!("session 'hw' not found"), "{path}");
        }

        // Every other verb still points at the modern API.
        for path in ["/api/sessions/hw/meta", "/api/sessions/hw/stats", "/api/sessions/hw"] {
            let (st, v) = call(&app, "GET", path, None).await;
            assert_eq!(st, StatusCode::NOT_IMPLEMENTED, "{path} -> {v}");
            assert_eq!(v["hint"], json!("/api/workers/hw"), "{path}");
        }
    }

    #[tokio::test]
    async fn file_backed_verbs_roundtrip_hermetically() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        std::fs::write(
            home.path().join("sessions/probe.env"),
            "CC_DIR=\"/tmp\"\nCC_DESC=\"a probe\"\nCC_TAGS=\"alpha, beta\"\nCC_FLAGS=\"--model sonnet\"\n",
        )
        .unwrap();
        // Shared AMUX_HOME guard (settings::test_env): the var is
        // process-global and other lib tests set it too — an unserialized
        // set_var raced them and read another test's home mid-assert.
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (state, _dir) = state();
        let app: Router = routes().with_state(state);

        // 404 for a missing session, Python's exact error shape.
        let (st, v) = call(&app, "GET", "/api/sessions/nope/meta", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], json!("session 'nope' not found"));

        // meta merges env-derived fields.
        let (st, v) = call(&app, "GET", "/api/sessions/probe/meta", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["name"], json!("probe"));
        assert_eq!(v["provider"], json!("claude"));
        assert_eq!(v["configured_model"], json!("sonnet"));
        assert_eq!(v["tags"], json!(["alpha", "beta"]));
        assert_eq!(v["desc"], json!("a probe"));

        // info carries the raw env text.
        let (st, v) = call(&app, "GET", "/api/sessions/probe/info", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["raw"].as_str().unwrap().contains("CC_DESC"));
        assert_eq!(v["pinned"], json!(false));

        // config PATCH: desc, tags, pin, branch, mcp validation, task_summary.
        let (st, v) =
            call(&app, "PATCH", "/api/sessions/probe/config", Some(json!({"desc": "new desc"}))).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(parse_env("probe").get("CC_DESC"), Some("new desc"));
        let (st, _) =
            call(&app, "PATCH", "/api/sessions/probe/config", Some(json!({"tags": "x, y"}))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(parse_env("probe").get("CC_TAGS"), Some("x, y"));
        let (st, _) =
            call(&app, "PATCH", "/api/sessions/probe/config", Some(json!({"toggle_pin": true}))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(parse_env("probe").get("CC_PINNED"), Some("1"));
        let (st, v) =
            call(&app, "PATCH", "/api/sessions/probe/config", Some(json!({"mcp": "bogus"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        let (st, v) = call(&app, "PATCH", "/api/sessions/probe/config", Some(json!({}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], json!("nothing to update"));
        // model swap on a NOT-RUNNING session rewrites flags without restart.
        let (st, v) = call(
            &app,
            "PATCH",
            "/api/sessions/probe/config",
            Some(json!({"model": "opus", "effort": "high"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let flags = parse_env("probe").get("CC_FLAGS").unwrap().to_string();
        assert!(flags.contains("--model opus") && flags.contains("--effort high"), "{flags}");
        assert_eq!(v["message"], json!("model set to opus"));
        // yolo toggle writes the provider flag + CC_AUTO_CONTINUE.
        let (st, v) =
            call(&app, "PATCH", "/api/sessions/probe/config", Some(json!({"toggle_yolo": true}))).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let cfg = parse_env("probe");
        assert!(cfg.get_or("CC_FLAGS", "").contains("--dangerously-skip-permissions"));
        assert_eq!(cfg.get("CC_AUTO_CONTINUE"), Some("1"));

        // instructions save + read back.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/instructions",
            Some(json!({"instructions": "stay on task"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["saved"], json!(true));
        let (_, v) = call(&app, "GET", "/api/sessions/probe/instructions", None).await;
        assert_eq!(v["instructions"], json!("stay on task"));

        // tracked-files add/list/remove; conversation-id adoption guard.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/tracked-files",
            Some(json!({"files": ["a.rs", "b.rs"], "conversation_id": "12345678-abcd"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["files"], json!(["a.rs", "b.rs"]));
        assert_eq!(meta_str(&load_meta("probe"), "cc_conversation_id"), "12345678-abcd");
        let (_, v) = call(&app, "GET", "/api/sessions/probe/tracked-files", None).await;
        assert_eq!(v["files"], json!(["a.rs", "b.rs"]));
        let (_, v) = call(
            &app,
            "DELETE",
            "/api/sessions/probe/tracked-files",
            Some(json!({"files": ["a.rs"]})),
        )
        .await;
        assert_eq!(v["files"], json!(["b.rs"]));
        // A sibling claiming the same conversation must NOT be adopted.
        std::fs::write(home.path().join("sessions/sib.env"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(
            home.path().join("sessions/sib.meta.json"),
            json!({"cc_conversation_id": "99999999-aaaa"}).to_string(),
        )
        .unwrap();
        let (_, _) = call(
            &app,
            "POST",
            "/api/sessions/probe/tracked-files",
            Some(json!({"files": [], "conversation_id": "99999999-aaaa"})),
        )
        .await;
        assert_eq!(
            meta_str(&load_meta("probe"), "cc_conversation_id"),
            "12345678-abcd",
            "cross-link guard must refuse adopting a sibling's conversation"
        );

        // steer enqueue → visible in GET → delete clears.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/steer",
            Some(json!({"text": "queued message"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        let (_, v) = call(&app, "GET", "/api/sessions/probe/steer", None).await;
        assert_eq!(v[0]["text"], json!("queued message"));
        assert_eq!(v[0]["id"], json!(id));
        // Identical text replaces, never stacks (dedup-on-enqueue).
        let (_, _) = call(
            &app,
            "POST",
            "/api/sessions/probe/steer",
            Some(json!({"text": "queued message"})),
        )
        .await;
        let (_, v) = call(&app, "GET", "/api/sessions/probe/steer", None).await;
        assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
        // [no-board]-only message is a 400, not an empty enqueue.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/steer",
            Some(json!({"text": "[no-board]"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        let (_, v) = call(&app, "DELETE", "/api/sessions/probe/steer", Some(json!({}))).await;
        assert_eq!(v["ok"], json!(true));
        let (_, v) = call(&app, "GET", "/api/sessions/probe/steer", None).await;
        assert_eq!(v.as_array().unwrap().len(), 0);

        // duplicate copies the env under a sanitized name.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/duplicate",
            Some(json!({"new_name": "probe copy!"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(env_path("probe-copy-").exists());
        let (st, _) = call(
            &app,
            "POST",
            "/api/sessions/probe/duplicate",
            Some(json!({"new_name": "probe copy!"})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);

        // share token CRUD.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/share",
            Some(json!({"perms": "output", "expires_hours": 1, "label": "t"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let token = v["token"].as_str().unwrap().to_string();
        assert!(v["url"].as_str().unwrap().contains(&token));
        assert!(v["expires_at"].as_i64().unwrap() > now_i64());
        let (_, v) = call(&app, "GET", "/api/sessions/probe/share", None).await;
        assert_eq!(v[0]["token"], json!(token));
        let (_, v) =
            call(&app, "DELETE", "/api/sessions/probe/share", Some(json!({"token": token}))).await;
        assert_eq!(v["ok"], json!(true));
        let (_, v) = call(&app, "GET", "/api/sessions/probe/share", None).await;
        assert_eq!(v.as_array().unwrap().len(), 0);

        // report: state write, alias mapping, tool-hook heartbeat rule.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/report",
            Some(json!({"state": "working", "source": "prompt-hook"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["state"], json!("active"));
        let (_, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/report",
            Some(json!({"state": "done", "source": "stop-hook"})),
        )
        .await;
        assert_eq!(v["state"], json!("idle"));
        // A tool-hook heartbeat must NOT resurrect the finished turn.
        let (_, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/report",
            Some(json!({"state": "active", "source": "tool-hook"})),
        )
        .await;
        assert_eq!(v["state"], json!("idle"), "{v}");
        assert!(v["note"].as_str().unwrap().contains("heartbeat ignored"));
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/probe/report",
            Some(json!({"state": "bogus"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");

        // commit-guard PATCH: set/clear override.
        let (_, v) = call(
            &app,
            "PATCH",
            "/api/sessions/probe/commit-guard",
            Some(json!({"enabled": false})),
        )
        .await;
        assert_eq!(v["enabled"], json!(false));
        assert_eq!(parse_env("probe").get("AMUX_COMMIT_GUARD_SESSION"), Some("0"));
        let (_, v) = call(
            &app,
            "PATCH",
            "/api/sessions/probe/commit-guard",
            Some(json!({"enabled": null})),
        )
        .await;
        assert_eq!(v["override"], Value::Null);
        assert_eq!(parse_env("probe").get("AMUX_COMMIT_GUARD_SESSION"), None);

        // delete without the UI token is a 403 (destructive guard); with the
        // automation override it removes the env file.
        let (st, v) = call(&app, "POST", "/api/sessions/probe/delete", None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        std::env::set_var("AMUX_ALLOW_AGENT_SESSION_DELETE", "1");
        // Pinned guard fires first.
        let (st, v) = call(&app, "POST", "/api/sessions/probe/delete", None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        let (_, _) =
            call(&app, "PATCH", "/api/sessions/probe/config", Some(json!({"toggle_pin": true}))).await;
        let (st, v) = call(&app, "POST", "/api/sessions/probe/delete", None).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(!env_path("probe").exists());
        std::env::remove_var("AMUX_ALLOW_AGENT_SESSION_DELETE");

        // Unknown verb 404s; unknown method 405s.
        let (st, _) = call(&app, "GET", "/api/sessions/sib/definitely-not", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = call(&app, "PUT", "/api/sessions/sib/config", Some(json!({}))).await;
        assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);

    }
    /// The owner-addendum rename matrix: noop, happy-path cascade with
    /// attached rows, retry-after-partial convergence, target collision.
    #[tokio::test]
    async fn rename_is_convergent_journaled_and_collision_safe() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (state, _dir) = state();
        // The baseline migration carries the full Python schema, so the
        // attached rows use the REAL issues/schedules tables — the cascade
        // must carry them to the new name.
        state
            .store
            .write_async(|conn| {
                conn.execute_batch(
                    "INSERT INTO issues (id, title, session, status, owner_type, created, updated)
                        VALUES ('I-1', 'card', 'rn-old', 'doing', 'agent', 1, 1);
                     INSERT INTO schedules (id, title, session, command, created, updated)
                        VALUES ('S-1', 'sched', 'rn-old', 'noop', 1, 1);",
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        std::fs::write(env_path("rn-old"), "CC_DESC=\"lane\"\n").unwrap();
        std::fs::write(meta_path("rn-old"), json!({"instructions": "keep"}).to_string()).unwrap();
        std::fs::create_dir_all(logs_dir()).unwrap();
        std::fs::write(log_path("rn-old"), "log body\n").unwrap();
        let app: Router = routes().with_state(state.clone());

        // 1. Rename-to-self: honest no-op — nothing written, rev unmoved.
        let rev_before = state.store.current_rev().unwrap();
        let (st, v) = call(
            &app, "PATCH", "/api/sessions/rn-old/config", Some(json!({"rename": "rn-old"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["noop"], json!(true));
        assert_eq!(v["name"], json!("rn-old"));
        assert_eq!(state.store.current_rev().unwrap(), rev_before, "noop must not move the rev");

        // 2. Happy path: files move, attached board card + schedule +
        //    steering + self-report follow, response names the steps.
        let (_, _) = call(
            &app, "POST", "/api/sessions/rn-old/steer", Some(json!({"text": "queued"})),
        )
        .await;
        let (_, _) = call(
            &app, "POST", "/api/sessions/rn-old/report",
            Some(json!({"state": "idle", "source": "stop-hook"})),
        )
        .await;
        let (st, v) = call(
            &app, "PATCH", "/api/sessions/rn-old/config", Some(json!({"rename": "rn-new"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["name"], json!("rn-new"));
        assert!(!env_path("rn-old").exists() && env_path("rn-new").exists());
        assert!(meta_path("rn-new").exists() && log_path("rn-new").exists());
        let steps = v["steps"].to_string();
        assert!(steps.contains("db.issues: 1 row(s)"), "{steps}");
        assert!(steps.contains("db.schedules: 1 row(s)"), "{steps}");
        assert!(steps.contains("db.steering_queue: 1 row(s)"), "{steps}");
        assert!(steps.contains("prefs.session_reports: key migrated"), "{steps}");
        {
            let conn = state.store.read().unwrap();
            let sess: String = conn
                .query_row("SELECT session FROM issues WHERE id='I-1'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sess, "rn-new");
            let sched: String = conn
                .query_row("SELECT session FROM schedules WHERE id='S-1'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sched, "rn-new");
            // Journal: started + completed events both present (rule 4).
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM session_events WHERE type IN ('session.rename.started','session.renamed')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(n >= 2, "rename must journal start+finish, found {n}");
        }

        // 3. Retry-after-partial: simulate a crash that moved ONLY the env
        //    file, leaving meta/log/DB under the old name — the retry of the
        //    SAME rename converges the stragglers.
        std::fs::write(env_path("rn2-old"), "CC_DESC=\"lane2\"\n").unwrap();
        std::fs::write(meta_path("rn2-old"), json!({"instructions": "x"}).to_string()).unwrap();
        std::fs::write(log_path("rn2-old"), "log2\n").unwrap();
        state
            .store
            .write_async(|conn| {
                conn.execute(
                    "INSERT INTO issues (id, title, session, status, owner_type, created, updated)
                     VALUES ('I-2', 'card2', 'rn2-old', 'todo', 'agent', 1, 1)",
                    [],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
        std::fs::rename(env_path("rn2-old"), env_path("rn2-new")).unwrap(); // the "crash"
        let (st, v) = call(
            &app, "PATCH", "/api/sessions/rn2-old/config", Some(json!({"rename": "rn2-new"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "retry must converge, got {v}");
        assert_eq!(v["resumed_partial"], json!(true), "{v}");
        assert!(meta_path("rn2-new").exists(), "meta straggler must follow on retry");
        assert!(log_path("rn2-new").exists(), "log straggler must follow on retry");
        {
            let conn = state.store.read().unwrap();
            let sess: String = conn
                .query_row("SELECT session FROM issues WHERE id='I-2'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sess, "rn2-new", "DB straggler must follow on retry");
        }

        // 4. Collision: both envs exist → 409 naming the conflict; and a
        //    rename of a missing session to a missing target stays a 404.
        std::fs::write(env_path("rn3"), "CC_DESC=\"third\"\n").unwrap();
        let (st, v) = call(
            &app, "PATCH", "/api/sessions/rn3/config", Some(json!({"rename": "rn-new"})),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], json!("'rn-new' already exists"));
        let (st, _) = call(
            &app, "PATCH", "/api/sessions/ghost/config", Some(json!({"rename": "also-ghost"})),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// AMUX-2936. Driven through the REAL endpoint, not `adopt_reported_conv_id`
    /// directly, because the shipped path is what was broken: the id has to
    /// survive body parsing, the attribution check and the subagent branch to
    /// reach meta. A unit test on the helper would pass with the wiring absent,
    /// which is the exact shape of check this repo keeps paying for.
    ///
    /// The specimen is `mixpeek-general`: RUNNING, `cc_conversation_id: ""`,
    /// four unclaimed transcripts in a shared project dir so no fallback can
    /// resolve it, and therefore blind to the staged guard on all 304 of its
    /// warnings. Case 1 below is that lane.
    #[tokio::test]
    async fn a_lane_reporting_its_own_conv_id_heals_blindness_and_refuses_cross_links() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        let (state, _dir) = state();
        let app: Router = routes().with_state(state.clone());

        // The blind lane, in its exact live shape: an EMPTY cid, not a missing
        // key. (mixpeek-general's meta really does carry `"": ` — it was
        // cleared, never absent, so a test using a missing key would exercise a
        // different branch than the one in production.)
        std::fs::write(env_path("blindy"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(meta_path("blindy"), json!({"cc_conversation_id": ""}).to_string()).unwrap();
        const CONV: &str = "bfee1ec0-f9fa-4c1b-9a77-0d1e2f3a4b5c";

        // 1. HEALS. A stop-hook report carrying Claude Code's own `session_id`
        //    makes the lane resolvable.
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/blindy/report",
            Some(json!({"state": "idle", "source": "stop-hook", "session_id": CONV})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["conv_id"]["adopted"], json!(true), "{v}");
        assert_eq!(
            v["conv_id"]["healed_blind_lane"],
            json!(true),
            "a lane that had no id at all is the case that closes an absorption window: {v}"
        );
        assert_eq!(meta_str(&load_meta("blindy"), "cc_conversation_id"), CONV);

        // 2. NO CHURN. The same hook fires on every turn boundary, fleet-wide;
        //    re-reporting an unchanged id must not rewrite meta.
        let (_, v) = call(
            &app,
            "POST",
            "/api/sessions/blindy/report",
            Some(json!({"state": "idle", "session_id": CONV})),
        )
        .await;
        assert_eq!(v["conv_id"]["adopted"], json!(false), "{v}");

        // 3. CROSS-LINK REFUSED, and the refusal is legible in the BODY. This is
        //    the 2026-08-09 incident mechanised: adopting a neighbour's live
        //    conversation is strictly worse than staying blind, because the
        //    guard would then attribute one lane's edits to another with
        //    confidence.
        const SIB_CONV: &str = "99999999-aaaa-4bbb-8ccc-dddddddddddd";
        std::fs::write(env_path("sib"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(
            meta_path("sib"),
            json!({"cc_conversation_id": SIB_CONV}).to_string(),
        )
        .unwrap();
        let (st, v) = call(
            &app,
            "POST",
            "/api/sessions/blindy/report",
            Some(json!({"state": "idle", "session_id": SIB_CONV})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["conv_id"]["adopted"], json!(false), "{v}");
        assert_eq!(v["conv_id"]["conflict_with"], json!("sib"), "{v}");
        assert_eq!(
            meta_str(&load_meta("blindy"), "cc_conversation_id"),
            CONV,
            "a refused cross-link must leave the previous id intact"
        );

        // 4. A NON-ID IS NOT AN ID. Anything that is not transcript-stem shaped
        //    would resolve to no file and look identical to blindness.
        let (_, v) = call(
            &app,
            "POST",
            "/api/sessions/blindy/report",
            Some(json!({"state": "idle", "session_id": "resuming-session"})),
        )
        .await;
        assert_eq!(v["conv_id"]["adopted"], json!(false), "{v}");
        assert_eq!(meta_str(&load_meta("blindy"), "cc_conversation_id"), CONV);

        // 5. transcript_path is accepted too — same payload, other field, for a
        //    provider that sends the path and no id.
        const CONV2: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
        std::fs::write(env_path("pathy"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(meta_path("pathy"), json!({}).to_string()).unwrap();
        let (_, v) = call(
            &app,
            "POST",
            "/api/sessions/pathy/report",
            Some(json!({
                "state": "active",
                "transcript_path": format!("/Users/x/.claude/projects/-p/{CONV2}.jsonl"),
            })),
        )
        .await;
        assert_eq!(v["conv_id"]["adopted"], json!(true), "{v}");
        assert_eq!(meta_str(&load_meta("pathy"), "cc_conversation_id"), CONV2);

        // 6. A SUBAGENT-ONLY report still heals. It returns early, before the
        //    state parse, so adoption had to be hoisted above that branch — a
        //    lane whose only hook is PreToolUse:Task would otherwise stay blind
        //    forever.
        const CONV3: &str = "5f6e7d8c-9b0a-4123-8456-789abcdef012";
        std::fs::write(env_path("subby"), "CC_DIR=\"/tmp\"\n").unwrap();
        std::fs::write(meta_path("subby"), json!({}).to_string()).unwrap();
        let (st, _v) = call(
            &app,
            "POST",
            "/api/sessions/subby/report",
            Some(json!({"subagent": "start", "session_id": CONV3})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            meta_str(&load_meta("subby"), "cc_conversation_id"),
            CONV3,
            "adoption must happen above the subagent early-return"
        );
    }
}

#[cfg(test)]
mod steer_boundary_tests {
    use super::*;

    fn tstate() -> (AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        (
            AppState {
                store: std::sync::Arc::new(store),
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
            },
            dir,
        )
    }

    async fn set_report(state: &AppState, name: &str, st: &str) {
        let (n, s) = (name.to_string(), st.to_string());
        state
            .store
            .write_async(move |conn| {
                ensure_fleet_tables(conn)?;
                let blob = serde_json::json!({ &n: { "state": s } }).to_string();
                conn.execute(
                    "INSERT INTO prefs(key,value) VALUES('session_reports',?1) \
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    [&blob],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
    }

    /// AMUX-3147: a new user prompt must card even while the session holds an open
    /// MANUAL card — that was the "none of these have board items" bug, the old
    /// dedup skipping on ANY open agent card. Only a RAPID re-send within the
    /// window is deduped; a distinct task past the window cards again.
    #[tokio::test]
    async fn capture_cards_new_tasks_past_a_manual_card_but_dedups_a_rapid_resend() {
        use crate::db::board_store::{create_issue, NewIssue};
        let (state, _tmp) = tstate();
        let now_ms = 1_700_000_000_000i64;
        state
            .store
            .write_async(move |conn| {
                // A manual work card — NOT a capture (its desc has no **Prompt:** marker).
                create_issue(
                    conn,
                    &NewIssue {
                        title: "manual work".into(),
                        desc: "some manual work".into(),
                        status: "doing".into(),
                        session: Some("s".into()),
                        item_type: "code".into(),
                        creator: "amux".into(),
                        owner_type: "agent".into(),
                        due: None,
                        due_time: None,
                        reviewer: None,
                        shepherd: None,
                        gate: vec![],
                        depends_on: vec![],
                        tags: vec![],
                    },
                    now_ms / 1000,
                )?;
                // The open manual card must NOT block a new user task.
                let first = super::mint_capture_card(conn, "s", "build the connectors tab", now_ms)?;
                assert!(first.is_some(), "a new task must card even with an open manual card");
                // A rapid re-send within the window IS deduped.
                let rapid = super::mint_capture_card(conn, "s", "also wire slack", now_ms + 2_000)?;
                assert!(rapid.is_none(), "a rapid re-send within the window must dedup");
                // A distinct task past the window cards again.
                let later = super::mint_capture_card(conn, "s", "now add gmail", now_ms + 60_000)?;
                assert!(later.is_some(), "a distinct task past the dedup window must card");
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
    }

    /// THE REGRESSION. Shipped without this and it went straight to prod:
    /// `from_steering` inside send_text_inner refuses only on a selector, or on
    /// generating-AND-picker-text, so a plain queued message to a merely
    /// GENERATING lane fell through and delivered mid-turn. Ethan hit it within
    /// minutes — "i sent as a queue but it looks like it was sent directly even
    /// though this worker was still working" — and his follow-up was the right
    /// criticism: this should have been caught by CI, not by him.
    ///
    /// `active` is the case that actually regressed. The others are here so a
    /// future "simplification" to `st != "active"` cannot pass: `waiting` (at a
    /// selector) and `error` are equally not-a-boundary, and only `idle` is.
    #[tokio::test]
    async fn a_reporting_lane_is_a_boundary_only_when_it_reports_idle() {
        let (state, _d) = tstate();
        for (reported, want) in [
            ("active", false), // THE BUG: mid-turn. Delivering here is the defect.
            ("waiting", false),
            ("error", false),
            ("idle", true),
        ] {
            set_report(&state, "probe", reported).await;
            assert_eq!(
                steer_lane_at_boundary(&state, "probe").await,
                want,
                "reported state {reported:?} should give at_boundary={want}"
            );
        }
    }

    /// AMUX-3048: subagent start/stop events accumulate a live count in the same
    /// session_reports store, floored at 0, WITHOUT disturbing the main state —
    /// a subagent starting says nothing about the main turn.
    #[tokio::test]
    async fn subagent_events_accumulate_a_floored_live_count() {
        let (state, _d) = tstate();
        // A prior main-state report must survive the subagent events untouched.
        set_report(&state, "probe", "active").await;

        let read = |state: &AppState| -> Value {
            state
                .store
                .read()
                .unwrap()
                .query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .unwrap_or(Value::Null)
        };

        subagent_event_post(&state, "probe", "start").await;
        subagent_event_post(&state, "probe", "start").await;
        let v = read(&state);
        assert_eq!(v["probe"]["subagents"]["count"].as_i64(), Some(2), "two starts -> 2");
        assert_eq!(
            v["probe"]["state"].as_str(),
            Some("active"),
            "subagent events must not touch the main state"
        );

        subagent_event_post(&state, "probe", "stop").await;
        assert_eq!(read(&state)["probe"]["subagents"]["count"].as_i64(), Some(1), "one stop -> 1");

        // FLOOR: more stops than starts (a lost start event) must never underflow
        // into a negative count that would misread once a real start arrives.
        subagent_event_post(&state, "probe", "stop").await;
        subagent_event_post(&state, "probe", "stop").await;
        assert_eq!(
            read(&state)["probe"]["subagents"]["count"].as_i64(),
            Some(0),
            "count floors at 0 on excess stops"
        );
    }

    /// Fail-closed is the whole safety property: an unknown lane must not be
    /// treated as idle. With no report AND no pane (no tmux session under test),
    /// the capture is empty and "cannot tell" must read as "do not deliver" —
    /// notably for herdr, whose history read is REFUSED while working/blocked,
    /// so empty-capture is precisely the mid-turn state.
    #[tokio::test]
    async fn an_unknown_lane_is_never_a_boundary() {
        let (state, _d) = tstate();
        assert!(
            !steer_lane_at_boundary(&state, "no-such-lane-xyz").await,
            "a lane with neither a report nor a readable pane must fail CLOSED"
        );
    }
}

#[cfg(test)]
mod hot_model_switch_tests {
    //! AMUX-2617. Every assertion below pins a decision the SHIPPED path makes
    //! — `plan_config_swap`, `slash_verdict` and `mode_after_delivery` are the
    //! functions config_patch calls, not paraphrases of them (ethos rule 7).
    use super::*;

    /// Capabilities come from the REAL registry, so this test fails if the
    /// claude adapter ever stops claiming the capability — the point of
    /// consulting the registry instead of hardcoding a matrix here.
    #[test]
    fn hot_path_for_a_running_claude_session() {
        let caps = crate::api::workers::provider_caps("claude");
        assert!(caps.hot_model_switch, "claude adapter must claim the capability");
        assert_eq!(
            plan_config_swap("claude", &caps, true, true),
            SwapMode::Hot,
            "a running claude session with an expressible model change switches live"
        );
    }

    #[test]
    fn restart_path_for_a_provider_without_the_capability() {
        for p in ["gemini", "codex"] {
            let caps = crate::api::workers::provider_caps(p);
            assert!(!caps.hot_model_switch, "{p} does not advertise a hot switch");
            assert_eq!(
                plan_config_swap(p, &caps, true, true),
                SwapMode::Restart,
                "{p} must keep the restart path"
            );
        }
        // A provider the registry has never heard of gets the conservative
        // all-false default, which must also mean restart.
        let unknown = crate::api::workers::provider_caps("some-future-cli");
        assert_eq!(plan_config_swap("some-future-cli", &unknown, true, true), SwapMode::Restart);
        // And a provider that claims the capability while amux does not know
        // its slash syntax still restarts — the capability is necessary, not
        // sufficient.
        let claims = ProviderCapabilities { hot_model_switch: true, ..Default::default() };
        assert_eq!(plan_config_swap("gemini", &claims, true, true), SwapMode::Restart);
    }

    #[test]
    fn not_running_is_env_only_and_default_reset_restarts() {
        let caps = crate::api::workers::provider_caps("claude");
        assert_eq!(
            plan_config_swap("claude", &caps, false, true),
            SwapMode::EnvOnly,
            "a stopped session is an env rewrite and nothing else"
        );
        // "Default" (empty model/effort) has no slash argument form, so it is
        // not expressible and keeps the restart.
        assert_eq!(plan_config_swap("claude", &caps, true, false), SwapMode::Restart);
    }

    #[test]
    fn restart_is_the_fallback_when_delivery_fails() {
        assert_eq!(
            mode_after_delivery(&HotFold::Failed("no acknowledgement".into())),
            SwapMode::Restart,
            "a hot switch that did not land MUST fall back to a restart"
        );
        assert_eq!(mode_after_delivery(&HotFold::AllApplied), SwapMode::Hot);
        assert_eq!(mode_after_delivery(&HotFold::SomeQueued), SwapMode::Hot);

        // Failure dominates: a model command that landed does not excuse an
        // effort command that did not.
        let mixed = [
            HotOutcome::Applied,
            HotOutcome::Failed("Model 'nope' not found".into()),
        ];
        assert_eq!(
            fold_hot_outcomes(&mixed),
            HotFold::Failed("Model 'nope' not found".into())
        );
        assert_eq!(
            fold_hot_outcomes(&[HotOutcome::Applied, HotOutcome::Queued]),
            HotFold::SomeQueued
        );
        assert_eq!(
            fold_hot_outcomes(&[HotOutcome::Applied, HotOutcome::Applied]),
            HotFold::AllApplied
        );
    }

    /// The pane strings are the REAL ones captured from Claude Code v2.1.226
    /// on 2026-08-09 — a synthetic paraphrase would certify a parser against a
    /// shape the CLI does not emit.
    #[test]
    fn pane_verdict_reads_the_real_claude_output() {
        let before = "❯ hello\n  ⎿  hi\n";
        let after = "❯ hello\n  ⎿  hi\n❯ /model sonnet\n  ⎿  Set model to Sonnet 5 and saved as your default for new sessions\n";
        assert_eq!(
            slash_verdict(before, after, "/model sonnet", CC_MODEL_ACK),
            Some(HotOutcome::Applied)
        );

        let rejected = "❯ /model definitely-not-a-model\n  ⎿  Model 'definitely-not-a-model' not found\n";
        match slash_verdict(before, rejected, "/model definitely-not-a-model", CC_MODEL_ACK) {
            Some(HotOutcome::Failed(why)) => assert!(why.contains("not found"), "{why}"),
            other => panic!("a rejected id must fall back, got {other:?}"),
        }

        let effort_after = "❯ /effort high\n  ⎿  Set effort level to high (saved as your default for new sessions): Comprehensive implementation\n";
        assert_eq!(
            slash_verdict(before, effort_after, "/effort high", CC_EFFORT_ACK),
            Some(HotOutcome::Applied)
        );
    }

    /// The false-positive guard, which is the whole reason the verdict is a
    /// BEFORE/AFTER comparison: a pane that already shows the ack from an
    /// earlier identical switch must not certify a delivery that never
    /// happened. A false negative here costs one restart; a false positive
    /// would report a switch that did not occur.
    #[test]
    fn a_stale_ack_left_on_screen_is_not_a_verdict() {
        let pane = "❯ /model sonnet\n  ⎿  Set model to Sonnet 5 and saved as your default for new sessions\n❯ do some work\n";
        assert_eq!(
            slash_verdict(pane, pane, "/model sonnet", CC_MODEL_ACK),
            None,
            "an unchanged pane is no evidence at all"
        );
        // Bare-substring matching would have said Applied here; anchoring on
        // the echo of THIS command is what keeps it honest.
        assert_eq!(slash_answer_count(pane, "/model opus", CC_MODEL_ACK), 0);
        // Nothing rendered yet -> keep polling, do not conclude.
        assert_eq!(slash_verdict("", "❯ /model opus\n", "/model opus", CC_MODEL_ACK), None);
    }

    /// The ack must be tied to its own echo, so a command whose answer is out
    /// of the window does not borrow the neighbouring command's ack.
    #[test]
    fn the_ack_window_is_anchored_per_command() {
        let pane = "❯ /model opus\n1\n2\n3\n4\n5\n6\n7\n  ⎿  Set model to Opus 5\n";
        assert_eq!(
            slash_answer_count(pane, "/model opus", CC_MODEL_ACK),
            0,
            "an answer {SLASH_ACK_WINDOW}+ lines away is not this command's answer"
        );
    }

    /// A model id the SPA can produce must survive the round trip into a
    /// slash command unchanged — no mapping table, verified against the real
    /// CLI (see the module note). `[1m]` is the shape that would break a
    /// naive sanitizer.
    #[test]
    fn spa_picker_values_pass_through_verbatim() {
        for id in [
            "opus",
            "sonnet",
            "haiku",
            "claude-opus-5[1m]",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-6[1m]",
        ] {
            assert_eq!(
                validate_model_name(&json!(id)).unwrap(),
                id,
                "the SPA picker value {id} must survive validation untouched"
            );
            let cmd = format!("/model {id}");
            let pane = format!("❯ {cmd}\n  ⎿  Set model to Something\n");
            assert_eq!(slash_verdict("", &pane, &cmd, CC_MODEL_ACK), Some(HotOutcome::Applied));
        }
    }

    /// Delivery failure is reported, not swallowed: `send_text` refuses a
    /// session with no env file ("not running"), and that must arrive as
    /// `Failed` so the caller restarts rather than reporting a hot switch.
    #[tokio::test]
    async fn a_refused_delivery_is_a_failure_not_a_hot_switch() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let out = deliver_hot_config(&state, "amux-no-such-session-2617", "/model sonnet", CC_MODEL_ACK).await;
        match out {
            HotOutcome::Failed(_) => {}
            other => panic!("a session that cannot receive the command must fail, got {other:?}"),
        }
        assert_eq!(
            mode_after_delivery(&fold_hot_outcomes(&[out])),
            SwapMode::Restart
        );
    }
}

#[cfg(test)]
mod hot_switch_dialog_tests {
    //! The confirmation dialog, pinned against the REAL text captured from
    //! Claude Code v2.1.226 on 2026-08-09 (server log, AMUX-2617 probe). Both
    //! variants are here because the first implementation anchored on the
    //! model dialog's TITLE and silently could not answer the effort one.
    use super::*;

    const MODEL_DIALOG: &str = "\
   Switch model?

   Your next response will be slower and use more tokens

   This conversation is cached for the current model. Switching to Haiku 4.5 means the full history gets re-read on your next message.

 ❯ 1. Yes, switch to Haiku 4.5
   2. No, go back
";

    const EFFORT_DIALOG: &str = "\
   Change effort level?

   Your next response will be slower and use more tokens

   This conversation is cached for the current effort level. Switching to high means the full history gets re-read on your next message.

 ❯ 1. Yes, switch to high
   2. No, go back
";

    #[test]
    fn both_config_dialogs_are_answered() {
        assert_eq!(config_switch_confirm_key(MODEL_DIALOG).as_deref(), Some("1"));
        assert_eq!(
            config_switch_confirm_key(EFFORT_DIALOG).as_deref(),
            Some("1"),
            "the effort dialog's title differs from the model one; anchoring on the title \
             made this return None and turned every effort change into a restart"
        );
    }

    /// The confirm must be chosen by TEXT, so reordering the options cannot
    /// make amux press "No" while reporting a switch.
    #[test]
    fn the_yes_option_is_found_by_text_not_by_position() {
        let reordered = MODEL_DIALOG
            .replace("1. Yes, switch to Haiku 4.5", "1. No, go back")
            .replace("2. No, go back", "2. Yes, switch to Haiku 4.5");
        assert_eq!(config_switch_confirm_key(&reordered).as_deref(), Some("2"));
    }

    /// Narrow by construction: an ordinary pane, and any OTHER selector, must
    /// never be answered. amux presses a key here only because it is the one
    /// that asked for the change.
    #[test]
    fn no_other_selector_is_ever_answered() {
        assert_eq!(config_switch_confirm_key("❯ just a prompt\n"), None);
        // A real AskUserQuestion-style selector: numbered, has a "Yes", but
        // carries none of the config-confirmation body.
        let unrelated = " Delete the branch?\n ❯ 1. Yes, delete it\n   2. Cancel\n";
        assert_eq!(
            config_switch_confirm_key(unrelated),
            None,
            "answering an unrelated selector would be amux deciding for the user"
        );
    }
}

// ---------------------------------------------------------------------------
// AMUX-2629 — the submission evidence gate.
//
// Every fixture here is rebuilt from the INCIDENT, not from a convenient shape
// (ethos rule 7): amux-rust, 2026-08-09. The owner's message was typed into a
// mid-turn composer at 20:55:25, `POST /api/sessions/amux-rust/send` answered
// 200 {"ok":true} in 1050ms, and the text entered the conversation transcript
// at 21:06:15 — 10m50s later, when a human pressed a bare Enter. Claude Code
// writes a `queue-operation: enqueue` record for every mid-turn Enter it
// ACCEPTS (10 of them in that same transcript); there is none at 20:55, which
// is what discriminates "the Enter was not accepted" from "it was queued and
// waiting". The pre-fix code could not tell those apart and reported both as
// "sent (queued while generating)".
// ---------------------------------------------------------------------------
#[cfg(test)]
mod submission_gate_tests {
    use super::*;

    /// The real stuck message (142 chars — under the 400-char paste threshold,
    /// so it took the `send-keys -l` path).
    const GHOST: &str = "[06:55 PM] there are 9 queued commands here in this worker but ur idle - figure out why this is and make sure its fixed at the root moving fwd";

    fn tail_sq(text: &str) -> String {
        text.trim().chars().rev().take(16).collect::<Vec<_>>().into_iter().rev().collect::<String>()
            .split_whitespace()
            .collect()
    }

    /// Idle composer holding `text` — the frame the pane actually showed for
    /// ten minutes.
    fn frame_stuck_idle(text: &str) -> String {
        format!(
            "\u{2500}\u{2500}\u{2500}\u{2500} amux-rust \u{2500}\u{2500}\n\
             \u{276f} {text}\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
             \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} \u{2190} 2 agents\n"
        )
    }
    /// Same text still in the box, but the lane is generating: this IS queued
    /// input and must be left alone.
    fn frame_stuck_active(text: &str) -> String {
        format!(
            "\u{2731} Galloping\u{2026} (12s \u{b7} \u{2193} 84 tokens)\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\n\
             \u{276f} {text}\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\n\
             \u{23f5}\u{23f5} bypass permissions on \u{b7} esc to interrupt\n"
        )
    }
    /// A successful submit: composer drawn and empty.
    fn frame_cleared() -> String {
        "\u{2500}\u{2500}\u{2500}\u{2500} amux-rust \u{2500}\u{2500}\n\u{276f} \n\u{2500}\u{2500}\u{2500}\u{2500}\n\u{23f5}\u{23f5} bypass permissions on\n".into()
    }
    /// A cold Claude Code that has not painted its composer yet.
    fn frame_no_ui() -> String {
        "Loading\u{2026}\n\n".into()
    }

    // DRIFT GUARD (AMUX-2643). `submit_verdict_of` reads the outcome STRING, so
    // an innocuous wording change could silently stop classifying and every
    // later message would record a NULL verdict — a metadata column quietly
    // going blank, which nothing else would notice. Enumerate every
    // (Submission, generating, retried) combination through the real
    // send_outcome and assert each one still lands on a verdict.
    #[test]
    fn every_send_outcome_maps_to_a_verdict() {
        let mut seen: std::collections::BTreeSet<&'static str> = Default::default();
        for sub in [Submission::Confirmed, Submission::Stuck, Submission::Unverified] {
            for generating in [false, true] {
                for retried in [false, true] {
                    let (ok, msg) = send_outcome(sub, generating, retried);
                    let verdict = submit_verdict_of(&msg).unwrap_or_else(|| {
                        panic!(
                            "send_outcome({sub:?}, generating={generating}, retried={retried}) \
                             produced {msg:?}, which submit_verdict_of does not classify — \
                             the outcome strings and the classifier have drifted apart"
                        )
                    });
                    seen.insert(verdict);
                    // A verdict must never contradict the send's own ok flag.
                    if verdict == "stuck" {
                        assert!(!ok, "a stuck send must not report ok: {msg:?}");
                    }
                    if verdict == "confirmed" || verdict == "retried" {
                        assert!(ok, "a submitted send must report ok: {msg:?}");
                    }
                }
            }
        }
        // All four are reachable — otherwise a value exists only in the schema
        // comment and nothing can ever produce it.
        assert_eq!(
            seen.iter().copied().collect::<Vec<_>>(),
            vec!["confirmed", "retried", "stuck", "unverified"],
            "every documented verdict must be reachable from a real send"
        );
    }

    #[test]
    fn a_queued_send_has_no_verdict_yet_rather_than_a_false_one() {
        // The queued path has submitted nothing at this point. Recording
        // "confirmed" here would be the exact mislabelling this metadata exists
        // to end; NULL is the honest value, and currently the permanent one —
        // no deliverer stamps a verdict (AMUX-3541).
        assert_eq!(submit_verdict_of("queued (steering) — will deliver at the next boundary"), None);
        assert_eq!(submit_verdict_of(""), None);
    }

    #[test]
    fn the_incident_frame_reads_as_stuck_and_the_send_reports_failure() {
        let t = tail_sq(GHOST);
        assert_eq!(read_frame(&frame_stuck_idle(GHOST), &t), FrameRead::StillThereIdle);
        // ...and that verdict must make the SEND report failure, not success.
        let (ok, msg) = send_outcome(Submission::Stuck, false, false);
        assert!(!ok, "a message still sitting in the box must not report ok");
        assert!(msg.starts_with("not submitted"), "{msg}");
        // The mid-turn spelling, which is the branch the incident took.
        let (ok, msg) = send_outcome(Submission::Stuck, true, true);
        assert!(!ok, "mid-turn: a message the retry could not submit must not report ok");
        assert!(msg.contains("mid-turn Enter was not accepted"), "{msg}");
    }

    #[test]
    fn a_frame_that_is_not_the_incident_still_discriminates() {
        // The probe that would pass against ANY implementation is the danger;
        // these are the controls that make the test above mean something.
        let t = tail_sq(GHOST);
        assert_eq!(read_frame(&frame_cleared(), &t), FrameRead::Cleared);
        assert_eq!(read_frame(&frame_no_ui(), &t), FrameRead::NoUi);
        assert_eq!(read_frame(&frame_stuck_active(GHOST), &t), FrameRead::StillThereGenerating);
        // A DIFFERENT message in the box is not our message.
        assert_eq!(read_frame(&frame_stuck_idle("[06:50 PM] something else"), &t), FrameRead::Cleared);
    }

    #[test]
    fn no_ui_is_never_reported_as_submitted() {
        // AC-271: an empty pane is "not ready", not "delivered". `ok` stays
        // true so nobody double-sends, but `submitted` must not be claimed —
        // the message says so and send_post maps it to submission=unverified.
        let (ok, msg) = send_outcome(Submission::Unverified, false, false);
        assert!(ok);
        assert!(msg.contains("could not be verified"), "{msg}");
        assert!(!msg.contains("not submitted"), "unverified must not read as a failure: {msg}");
    }

    #[test]
    fn wrapped_text_is_matched_across_the_hard_wrap() {
        // The box wraps at the pane width, splitting the tail across visual
        // lines at arbitrary points. Checking only the ❯ line is how a wrapped
        // message's tail escaped verification and got dequeued as "sent" (the
        // "random" ghost, 2026-07-10).
        let wrapped = "\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} [06:55 PM] there are 9 queued commands here in this worker but ur idle - figure\n  out why this is and make sure its fixed at the root moving fwd\n\u{2500}\u{2500}\u{2500}\u{2500}\n\u{23f5}\u{23f5} bypass permissions on\n";
        assert_eq!(read_frame(wrapped, &tail_sq(GHOST)), FrameRead::StillThereIdle);
    }

    #[test]
    fn absent_from_the_evidence_source_is_not_evidence() {
        // "send reports failure when the message never appears in the evidence
        // source": the transcript tail has OTHER user messages and an OLDER
        // copy of ours, but nothing stamped after this send began.
        let sent_at = 1_786_323_326.0; // 2026-08-09 20:55:26 -0400, the real send
        let recs = vec![
            json!({"type":"user","message":{"role":"user","content":"unrelated"},"timestamp":"2026-08-10T00:54:28.000Z"}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":GHOST}]},"timestamp":"2026-08-10T00:56:00.000Z"}),
            // An OLDER identical user message must not count as this send.
            json!({"type":"user","message":{"role":"user","content":GHOST},"timestamp":"2026-08-10T00:40:00.000Z"}),
        ];
        assert!(!jsonl_records_have(&recs, GHOST, sent_at), "absent-after-since must read as no evidence");
        // The positive control: the same text stamped AFTER the send is proof.
        // Without this the negative above could pass on a scanner that never
        // matches anything.
        let mut with_it = recs.clone();
        with_it.push(json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":GHOST}]},"timestamp":"2026-08-10T01:06:15.000Z"}));
        assert!(jsonl_records_have(&with_it, GHOST, sent_at), "post-send user message IS evidence");
    }

    #[test]
    fn iso8601_parses_to_the_real_incident_timestamps() {
        // The evidence window is only as good as this parse; a wrong epoch
        // makes the `since` gate match everything or nothing (the
        // milliseconds-vs-seconds trap, ethos rule 7).
        let t = parse_iso8601("2026-08-10T01:06:15.000Z").unwrap();
        assert!((t - 1_786_323_975.0).abs() < 1.0, "got {t}");
        let z = parse_iso8601("2026-08-09T21:06:15.000-04:00").unwrap();
        assert!((z - t).abs() < 1.0, "offset form must equal the Z form: {z} vs {t}");
        assert!(parse_iso8601("not a timestamp").is_none());
    }

    #[test]
    fn at_picker_regex_matches_a_mention_not_an_email() {
        // py:25538 _AT_PICKER_RE. `contains('@')` force-queued any message with
        // an email in it (2026-07-17); the rust port had reintroduced that.
        assert!(at_picker_text("[04:27 PM] fix @/Users/ethan/.amux/uploads/x.png"));
        assert!(at_picker_text("@session please look"));
        assert!(at_picker_text("/compact"));
        assert!(!at_picker_text("email mhoward@lucihub.com about it"));
        assert!(!at_picker_text("nothing special here"));
        assert!(!at_picker_text(GHOST), "the incident's own text opens no picker");
    }

    #[test]
    fn pending_input_reads_the_composer_not_the_transcript() {
        // The ❯ scan must take the LAST prompt line; earlier ❯ lines in the
        // scrollback are previous turns.
        let frame = format!(
            "\u{276f} an older prompt from the scrollback\n\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f} {GHOST}\n\u{2500}\u{2500}\u{2500}\u{2500}\n\u{23f5}\u{23f5} bypass permissions on\n"
        );
        let p = composer_state(&frame).typed().unwrap().to_string();
        assert!(p.starts_with("[06:55PM]there"), "{p}");
        assert!(!p.contains("scrollback"), "must not fold in an earlier prompt: {p}");
    }
}

#[cfg(test)]
mod send_retry_reporting_tests {
    use super::*;

    #[test]
    fn a_retry_is_never_smoothed_into_a_clean_send() {
        // The first Enter being dropped and then working on retry is a
        // DEGRADED delivery, not a success. If this ever reads identically to
        // a clean send, the signal that the keystroke path is failing is gone
        // and the next ten-minute stall is invisible again.
        let (ok, clean) = send_outcome(Submission::Confirmed, false, false);
        assert!(ok);
        let (ok, retried) = send_outcome(Submission::Confirmed, false, true);
        assert!(ok);
        assert_ne!(clean, retried, "a retried send must not report as a clean send");
        assert!(retried.contains("on retry"), "{retried}");
        // send_post keys `retried` off exactly that substring.
        assert!(!clean.contains("on retry"));
        let (_, mid) = send_outcome(Submission::Confirmed, true, true);
        assert!(mid.contains("on retry"), "{mid}");
    }
}

// ---------------------------------------------------------------------------
// The composer-state discriminator (2026-08-09, the SECOND finding on
// AMUX-2629). Every fixture below is a VERBATIM `tmux capture-pane -e` line
// from a live pane, kept byte-for-byte rather than paraphrased: the whole
// defect was that the paraphrase (ANSI stripped) is identical for two states
// that behave completely differently, so a hand-written fixture would have
// reproduced the blindness instead of catching it.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod composer_state_tests {
    use super::*;

    /// `backend`, captured 2026-08-09 while it was being reported as "holding
    /// unsubmitted text for hours". The composer is EMPTY; `continue with the
    /// queue` is Claude Code's dim suggestion. Three people pressed Enter,
    /// C-m and Escape+Enter on this exact frame and reported all three as
    /// failures — correctly, because there was nothing to submit.
    const LIVE_PLACEHOLDER: &str = "\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500}\u{1b}[38;5;16m\u{1b}[48;5;37m backend \u{1b}[38;5;37m\u{1b}[49m\u{2500}\u{2500}\n\u{1b}[39m\u{276f}\u{a0}\u{1b}[2mcontinue with the queue\u{1b}[0m\n\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500}\u{2500}\n\u{1b}[39m  \u{1b}[38;5;211m\u{23f5}\u{23f5} bypass permissions on\u{1b}[38;5;246m (shift+tab to cycle) \u{b7} \u{2190} 2 agents\u{1b}[39m\n";

    /// A live pane with text genuinely typed into it and not submitted. Note
    /// there is no `\x1b[2m` anywhere in the composer body — that absence is
    /// the entire signal.
    const LIVE_TYPED: &str = "\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500} probe \u{2500}\u{2500}\n\u{1b}[39m\u{276f}\u{a0}[10:20 PM] look at @/Users/ethan/Dev/amux/README.md please\n\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500}\u{2500}\n\u{1b}[39m  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} \u{2190} 2 agents\n";

    /// A slash command is COLOURED, not dimmed — the near-miss that would
    /// break a naive "any SGR means chrome" rule.
    const LIVE_TYPED_SLASH: &str = "\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500} probe \u{2500}\u{2500}\n\u{1b}[39m\u{276f}\u{a0}\u{1b}[38;5;153m/compact\u{1b}[39m\n\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500}\u{2500}\n\u{1b}[39m  \u{23f5}\u{23f5} bypass permissions on\n";

    const LIVE_EMPTY: &str = "\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500} probe \u{2500}\u{2500}\n\u{1b}[39m\u{276f}\u{a0}\n\u{1b}[38;5;37m\u{2500}\u{2500}\u{2500}\u{2500}\n\u{1b}[39m  \u{23f5}\u{23f5} bypass permissions on\n";

    /// The background-conversation manager, opened with `←`. Its composer is
    /// NOT the lane's: the placeholder literally says "describe a task for a
    /// new session", and its status bar replaces the normal one.
    const LIVE_BG_MANAGER: &str = "Your conversation moved to the background \u{2014} enter opens it \u{b7} esc returns to it \u{b7} ctrl+c twice quits\n\nNeeds input\n \u{273b} current session     send a prompt to start\n\u{2500}\u{2500}\u{2500}\u{2500}\n\u{1b}[39m\u{276f}\u{a0}\u{1b}[2mdescribe a task for a new session\u{1b}[0m\n\u{2500}\u{2500}\u{2500}\u{2500}\n  \u{23f5}\u{23f5} bypass permissions \u{b7} enter to collapse \u{b7} ctrl+x to delete all \u{b7} ? for shortcuts\n";

    #[test]
    fn a_dim_suggestion_is_not_pending_input() {
        assert_eq!(
            composer_state(LIVE_PLACEHOLDER),
            ComposerState::Placeholder("continuewiththequeue".into())
        );
        assert_eq!(composer_state(LIVE_PLACEHOLDER).typed(), None);
        // The control that proves the discriminator is doing work rather than
        // rejecting everything: strip the ANSI first — as python did — and the
        // two states become the same string. This assertion is the bug.
        let stripped_ph = strip_ansi(LIVE_PLACEHOLDER);
        let stripped_ty = strip_ansi(LIVE_TYPED);
        assert!(stripped_ph.contains("\u{276f}\u{a0}continue with the queue"));
        assert!(stripped_ty.contains("\u{276f}\u{a0}[10:20 PM] look at"));
        assert_eq!(
            composer_state(&stripped_ph).typed(),
            Some("continuewiththequeue"),
            "a pre-stripped frame re-creates the blindness — callers MUST pass the raw capture"
        );
    }

    #[test]
    fn real_typed_input_is_recognised_including_the_near_misses() {
        assert_eq!(
            composer_state(LIVE_TYPED).typed(),
            Some("[10:20PM]lookat@/Users/ethan/Dev/amux/README.mdplease")
        );
        // Coloured, not dim: must still be real input.
        assert_eq!(composer_state(LIVE_TYPED_SLASH).typed(), Some("/compact"));
        assert_eq!(composer_state(LIVE_EMPTY), ComposerState::Empty);
        assert_eq!(composer_state(""), ComposerState::NotVisible);
    }

    #[test]
    fn the_background_manager_is_not_this_lanes_composer() {
        // Typing here composes a NEW task; the send verb must refuse rather
        // than address the wrong thing. It is NOT reported as Typed even
        // though the manager draws a ❯ line.
        assert_eq!(composer_state(LIVE_BG_MANAGER), ComposerState::BackgroundManager);
        assert_eq!(composer_state(LIVE_BG_MANAGER).typed(), None);
        assert_eq!(composer_state(LIVE_BG_MANAGER).typed(), None);
    }

    #[test]
    fn a_placeholder_can_never_be_mistaken_for_our_unsent_message() {
        // The duplicate-send hazard: if the verifier read a dim suggestion as
        // "our text is still in the box", it would press Escape+Enter and
        // submit the message a second time.
        let tail: String = "continue with the queue".split_whitespace().collect();
        assert_eq!(read_frame(LIVE_PLACEHOLDER, &tail), FrameRead::Cleared);
        // Same words, actually typed → the real stuck state.
        let typed = LIVE_PLACEHOLDER.replace("\u{1b}[2mcontinue with the queue\u{1b}[0m", "continue with the queue");
        assert_eq!(read_frame(&typed, &tail), FrameRead::StillThereIdle);
    }

    #[test]
    fn the_13_lane_false_positive_is_reproduced_and_fixed() {
        // Every one of the 13 lanes reported as stuck on 2026-08-09, by the
        // text their composers showed. All were dim; none had anything to
        // submit. If this ever reads as Typed again, the fleet-wide false
        // alarm is back.
        for text in [
            "check if AC-294 and AC-295 have been deployed yet",
            "continue with the queue",
            "push it",
            "delete the 5 looping rows",
            "./tick_runner.sh rb2b",
            "check on the staging e2e run",
            "keep working FRUSTRATIONS.md",
            "go ahead and exercise the guard against ddd",
            "Run the MVS prod health loop per the runbook",
            "yeah send them",
            "what should i do in crested butte today",
            "give me a real task",
            "run the MS-1030 read",
        ] {
            let frame = LIVE_PLACEHOLDER.replace("continue with the queue", text);
            assert_eq!(composer_state(&frame).typed(), None, "still reads as stuck: {text:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// The four-hour queue freeze (AMUX-2629, third finding). amux-rust sat with 10
// steering messages queued for up to 229 minutes while every other gate
// passed: env file present, tmux running, self-report `idle` 198s old, composer
// EMPTY, status bar `⏵⏵ bypass permissions on (shift+tab to cycle) · ← 2 agents`.
// The refusal came from the "esc to interrupt" re-check matching the lane's own
// PROSE about that string, 20+ lines up in the transcript.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod steer_freeze_tests {
    use super::*;

    /// Verbatim from `tmux capture-pane -p -S -12` on amux-rust, 2026-08-09,
    /// while it was frozen. Lines 26-27 of the real capture are reproduced
    /// exactly — they are the agent's own summary of a status-detection fix.
    const FROZEN_IDLE_PANE: &str = "\
  Two bugs found and fixed:

  1. Status badge (17c5a3c): Workers with \"bypass permissions on\" + \"esc to interrupt\" on the status
  bar were misdetected as IDLE. Fixed by checking \"esc to interrupt\" as an active signal before the
  bottom-up scan.

\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} amux-rust \u{2500}\u{2500}
\u{276f}\u{a0}
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} \u{2190} 2 agents                    /rc failed
";

    /// Genuinely mid-turn: "esc to interrupt" on the bar, NO prompt visible
    /// (the ❯ from the previous turn scrolled off or the capture is too short).
    const REALLY_GENERATING_PANE: &str = "\
  some transcript text with no marker in it

\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt \u{b7} \u{2190} 2 agents
";

    /// Idle with background agents: prompt ❯ visible AND "esc to interrupt"
    /// on the bar (the agents are what can be interrupted, not the main turn).
    const IDLE_WITH_AGENTS_PANE: &str = "\
  some transcript text with no marker in it

\u{2500}\u{2500}\u{2500}\u{2500}\u{2500} amux-rust \u{2500}\u{2500}
\u{276f}\u{a0}
\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}
  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt \u{b7} \u{2190} 2 agents
";

    #[test]
    fn prose_about_esc_to_interrupt_is_not_a_generating_lane() {
        assert!(
            !pane_bar_says_generating(FROZEN_IDLE_PANE),
            "the lane was idle; its own prose must not read as a live turn"
        );
        assert!(
            FROZEN_IDLE_PANE.to_lowercase().contains("esc to interrupt"),
            "fixture must still contain the string, or it proves nothing"
        );
    }

    #[test]
    fn a_status_bar_marker_is_still_a_generating_lane() {
        assert!(
            pane_bar_says_generating(REALLY_GENERATING_PANE),
            "no prompt visible + esc to interrupt on bar = generating"
        );
        // NOT asserted against `detect_claude_status` directly: that function is
        // under active repair by another lane and returned "idle", then "",
        // then "active" for this frame within one evening. What this module
        // needs is the COMPOSED property — that the gate refuses to call it a
        // boundary — and that holds regardless of which way the scraper is
        // currently leaning.
        assert!(
            !pane_is_at_boundary(REALLY_GENERATING_PANE),
            "a bar-marked live turn is never a delivery boundary"
        );
    }

    #[test]
    fn an_empty_composer_does_not_make_a_marked_bar_idle() {
        // Supersedes a peer's `idle_with_agents_is_not_generating`, which
        // asserted the opposite. The fixture below IS the frame a lane shows
        // while generating — measured live, essay in flight — so treating it as
        // a boundary would type into a running turn. The genuinely ambiguous
        // case (idle WITH background agents) is decided by the self-report,
        // which `steer_decide` consults first; this pane fallback fails closed
        // and the max-age deadline bounds the cost of being wrong.
        assert!(
            pane_bar_says_generating(IDLE_WITH_AGENTS_PANE),
            "empty ❯ + esc to interrupt is what a GENERATING lane looks like"
        );
        assert!(!pane_is_at_boundary(IDLE_WITH_AGENTS_PANE));
        // The cost of failing closed is bounded, not indefinite:
        assert_eq!(
            steer_decide(None, Some(pane_is_at_boundary(IDLE_WITH_AGENTS_PANE)), 601.0, 600.0),
            SteerDelivery::OverdueMidTurn
        );
    }

    #[test]
    fn at_shaped_text_is_deliverable_from_steering_when_the_lane_is_idle() {
        // The competing theory for the freeze was the @-picker guard. It cannot
        // be: that branch is reached only when the lane is GENERATING, and this
        // lane was idle. On an idle lane @-text takes the normal path, which
        // closes the picker with a spaced Escape and then submits — and now
        // proves it landed. Five of amux-rust's ten queued rows had no @ at all,
        // which also rules the theory out on the data.
        let at_text = "[04:39 PM] fix the logo @/Users/ethan/.amux/uploads/b7965e0b2a8f-image.png";
        assert!(at_picker_text(at_text), "this text does open the picker");
        assert!(!pane_bar_says_generating(FROZEN_IDLE_PANE), "…but the lane is idle, so the guard is not reached");
        for plain in [
            "make sure we have a scroll to the bottom thing i think we have it sometimes",
            "whwnever this is in the peek or whatever: https://localhost:8824/",
        ] {
            assert!(!at_picker_text(plain), "queued row without an @: {plain:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// AMUX-2642 — a lane that is never idle must not starve.
//
// Measured on the `amux` session, 2026-08-09: status `active` with a 6-second-old
// tool-hook self-report — correctly active, genuinely working — and FIVE steering
// messages queued 22:06..22:28, none delivered. amux-rust: ten, up to 229 minutes.
// `steer_lane_at_boundary` returns true only on `idle`, so a lane that works
// continuously has no boundary and its queue starves while the sender watches
// nothing happen and concludes the lane is hung.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod steer_max_age_tests {
    use super::*;

    const MAX: f64 = 600.0;

    #[test]
    fn a_continuously_active_lane_still_receives_within_the_max_age() {
        // THE REGRESSION TEST. The lane never reports idle — not once, at any
        // age. Under the old gate (`return st == "idle"`) every one of these is
        // "hold", which is the starvation. It must be delivered by the deadline.
        for age in [0.0, 60.0, 599.0] {
            assert_eq!(
                steer_decide(Some("active"), None, age, MAX),
                SteerDelivery::Hold,
                "at {age}s a busy lane should still wait for its boundary"
            );
        }
        for age in [600.0, 601.0, 1380.0 /* the amux specimen: 23 min */, 13740.0 /* amux-rust: 229 min */] {
            assert_eq!(
                steer_decide(Some("active"), None, age, MAX),
                SteerDelivery::OverdueMidTurn,
                "a message {age}s old must go into the running turn rather than wait forever"
            );
        }
    }

    #[test]
    fn the_boundary_is_still_preferred_whenever_there_is_one() {
        // The gate exists because the opposite failure was shipped first: a
        // queued message delivered into a working turn ("i sent as a queue but
        // it looks like it was sent directly even though this worker was still
        // working"). An idle lane must never be reported as an overdue
        // mid-turn delivery, however old the message is.
        assert_eq!(steer_decide(Some("idle"), None, 0.0, MAX), SteerDelivery::AtBoundary);
        assert_eq!(steer_decide(Some("idle"), None, 99_999.0, MAX), SteerDelivery::AtBoundary);
        // A selector is `waiting`, not `idle`: still not a boundary. (The send
        // path refuses a selector even when overdue — answering a pending tool
        // is the user's, not amux's.)
        assert_eq!(steer_decide(Some("waiting"), None, 10.0, MAX), SteerDelivery::Hold);
    }

    #[test]
    fn a_hookless_lane_falls_back_to_the_pane_and_fails_closed() {
        assert_eq!(steer_decide(None, Some(true), 0.0, MAX), SteerDelivery::AtBoundary);
        assert_eq!(steer_decide(None, Some(false), 0.0, MAX), SteerDelivery::Hold);
        // "cannot tell" (empty capture — a herdr lane mid-turn, by design) is
        // NOT idle...
        assert_eq!(steer_decide(None, None, 0.0, MAX), SteerDelivery::Hold);
        // ...but it does not exempt the lane from the deadline either, or a
        // hookless lane would starve exactly like the reported ones.
        assert_eq!(steer_decide(None, None, MAX + 1.0, MAX), SteerDelivery::OverdueMidTurn);
    }

    /// A KEYPRESS IS NOT A PROMPT (AMUX-2823). The risk runs both ways: too
    /// loose and this silently eats real instructions; too tight and Ethan's
    /// menu answer gets typed as prose again.
    #[test]
    fn a_picker_answer_is_recognised_but_an_ordinary_message_is_not() {
        let menu = "\
   What do you want to do?
   ❯ 1. Stop and wait for limit to reset
     2. Switch to usage credits
     3. Switch to Team plan
   Enter to confirm · Esc to cancel";

        // THE LIVE SPECIMEN — exactly what Ethan sent, which was delivered as
        // prose and cost mvs-infra 1m41s.
        assert!(answers_visible_picker("1. Stop and wait for limit to reset", menu));
        // The shapes a person or a UI actually produces.
        assert!(answers_visible_picker("1", menu));
        assert!(answers_visible_picker("2.", menu));
        assert!(answers_visible_picker("Switch to usage credits", menu));
        assert!(answers_visible_picker("  3. Switch to Team plan  ", menu));

        // ORDINARY MESSAGES typed while a picker happens to be up are meant for
        // AFTERWARDS. Voiding these would be real data loss, which is worse than
        // the bug being fixed.
        assert!(!answers_visible_picker("go fix the failing tests", menu));
        assert!(!answers_visible_picker("1 more thing: check the deploy", menu));
        assert!(!answers_visible_picker(
            "when you get a chance, switch to usage-based billing and tell me what it costs", menu));

        // NO PICKER ON SCREEN: nothing can be an answer, so nothing is voided.
        assert!(!answers_visible_picker("1. Stop and wait for limit to reset", "❯ "));
        assert!(!answers_visible_picker("1", ""));
    }

    /// AMUX-2834: which selectors mean "a human must act". Both are pickers to
    /// detect_claude_status; only one of them is anybody's problem.
    #[test]
    fn a_real_question_flags_input_required_but_a_rate_limit_menu_does_not() {
        let ask = "\
   Which database should I migrate?

   ❯ 1. production
     2. staging
     3. Cancel

   Enter to confirm · Esc to cancel";
        let limit = "\
   What do you want to do?

   ❯ 1. Stop and wait for limit to reset
     2. Switch to usage credits
     3. Switch to Team plan

   Enter to confirm · Esc to cancel";

        // The sweep's predicate, verbatim: a selector that is NOT the rate-limit
        // menu is a human's to answer.
        let flags = |p: &str| !is_rate_limit_menu(p) && detect_claude_status(p) == "waiting";

        assert!(flags(ask), "an AskUserQuestion blocks on a human and must read `needs input`");
        assert!(!flags(limit), "amux answers the rate-limit menu itself — flagging it would ask a \
                                human for something nobody needs to decide");

        // A working lane is not waiting for anyone, so it must never be flagged —
        // this is the assertion that keeps `needs input` meaningful. A badge that
        // fires on a busy fleet is one nobody reads.
        let busy = "⏺ Running…\n  ⏵⏵ bypass permissions on · esc to interrupt";
        assert!(!flags(busy));
        let idle = "❯ \n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        assert!(!flags(idle));
    }

    /// AUTO-COMPACT'S ARITHMETIC (AMUX-2829). This decides when amux interrupts
    /// a working lane, so the boundaries are pinned rather than left to a
    /// reading of the policy file.
    #[test]
    fn context_arithmetic_and_the_tiers_it_feeds() {
        use crate::orchestrator::compaction::{compaction_action, CompactionAction as CA};
        let w = 1_000_000u64;

        // Percent REMAINING, not used — the policy is keyed on headroom.
        // An over-window "current context" is not a context size. Before the
        // filter, a report of 3.15M against a 1M window read as 0% remaining
        // and force-compacted a lane sitting at ~22%.
        assert_eq!(context_pct_remaining(3_156_510, w), 0, "over-window reads as empty — why it must be rejected upstream");
        assert_eq!(context_pct_remaining(0, w), 100);
        assert_eq!(context_pct_remaining(700_000, w), 30);
        assert_eq!(context_pct_remaining(850_000, w), 15);
        assert_eq!(context_pct_remaining(950_000, w), 5);
        // Over-full clamps to 0 instead of underflowing — a lane past its window
        // is the case that MOST needs a compact, so this must not wrap to 100.
        assert_eq!(context_pct_remaining(2_000_000, w), 0);
        assert_eq!(context_pct_remaining(1, 0), 100, "unknown window must not force a compact");

        // The tiers this session actually acts on.
        assert_eq!(compaction_action(context_pct_remaining(500_000, w)), CA::None);
        assert!(matches!(compaction_action(context_pct_remaining(900_000, w)), CA::Compact));
        assert!(matches!(
            compaction_action(context_pct_remaining(990_000, w)),
            CA::ForceCompact
        ));
        // THIS SESSION'S OWN MEASURED LOAD, and the assertion is not the one I
        // first wrote. 817,201 of 1M is 18% remaining, which the policy calls
        // PrepareIndicator — NOT Compact. I asserted Compact, the test failed,
        // and the test was right: the session that prompted this card would not
        // have been auto-compacted by it.
        //
        // Left as the truth rather than bent to pass. Two things follow and both
        // are recorded on AMUX-2829 rather than fixed by widening a threshold
        // here: PrepareIndicator has NO consumer either (same producer-without-
        // consumer shape one tier up), and 1M is a floor for the window, not a
        // measurement — if it is really larger, 817k is a smaller fraction and
        // the tier is even further away.
        assert_eq!(
            compaction_action(context_pct_remaining(817_201, w)),
            CA::PrepareIndicator,
            "18% remaining is the prepare tier; amux acts on Compact and below"
        );
    }

    /// A lane with NO token data must read as unknown, never as an empty
    /// context. Returning 0 would compute 100% remaining and silently disable
    /// compaction for exactly the lanes whose harness is not reporting — the
    /// hardcoded-empty failure this repo has now hit five times.
    #[test]
    fn missing_tokens_are_unknown_not_zero() {
        assert_eq!(tokens_used(&json!(null)), None);
        assert_eq!(tokens_used(&json!({})), None);
        assert_eq!(tokens_used(&json!("nonsense")), None);
        // Both shapes seen in the wild.
        assert_eq!(tokens_used(&json!(817_201u64)), Some(817_201));
        assert_eq!(tokens_used(&json!({"total": 12345})), Some(12345));
        assert_eq!(tokens_used(&json!({"input": 100, "output": 23})), Some(123));
    }

    /// THE DISTINCTION, IN BOTH DIRECTIONS. Getting it wrong one way deadlocks a
    /// lane forever; getting it wrong the other way re-enables the 2026-07-15
    /// AskUserQuestion kill, where typing at a picker REJECTS a pending tool.
    #[test]
    fn a_rate_limit_menu_is_answered_but_a_real_question_is_never_touched() {
        // The live pane from mvs-infra, 2026-08-10.
        let limit_menu = "\
   What do you want to do?

   ❯ 1. Stop and wait for limit to reset
     2. Switch to usage credits
     3. Switch to Team plan

   Enter to confirm · Esc to cancel";
        assert!(is_rate_limit_menu(limit_menu), "the live specimen must match");
        // It is still a selector to the status detector — that is the point:
        // both classifications are true, and only this one licenses a keypress.
        assert_eq!(detect_claude_status(limit_menu), "waiting");

        // AN ACTUAL QUESTION FOR THE USER. Same shape, same "Enter to confirm",
        // and amux must NOT press anything: the answer is the user's and the
        // picker-closing Escape would reject the pending tool.
        let ask = "\
   Which database should I migrate?

   ❯ 1. production
     2. staging
     3. Cancel

   Enter to confirm · Esc to cancel";
        assert!(!is_rate_limit_menu(ask), "an AskUserQuestion must never be auto-answered");
        assert_eq!(detect_claude_status(ask), "waiting");

        // A near miss that must not fire: the phrase quoted in ordinary output,
        // e.g. a transcript being discussed. This decides whether amux presses a
        // key, so one matching line is not enough.
        let prose = "I hit the limit and chose 'Stop and wait for limit to reset' yesterday.";
        assert!(!is_rate_limit_menu(prose));

        // Case and ANSI must not defeat it — the real pane is full of colour.
        let ansi = "\x1b[1m   What do you want to do?\x1b[0m\n ❯ 1. STOP AND WAIT FOR LIMIT TO RESET\n 2. Switch to Usage Credits";
        assert!(is_rate_limit_menu(ansi));
    }

    /// The on-credits BANNER (Ethan, 2026-08-16: "workers with a rate limit" the
    /// menu misses). Reliable because the caller passes only the FOOTER and this
    /// matches Claude's FULL sentence. The false positive it replaces: a lane
    /// CODING the banner strings (amux) had them in scrollback, so a whole-pane
    /// scrape flagged it. Curly apostrophe (Claude's actual glyph) must match.
    #[test]
    fn credit_banner_flags_the_real_footer_banner_not_code_that_mentions_it() {
        // The real footer, curly apostrophe, one line above the spinner.
        let footer = "     /usage-credits to finish what you\u{2019}re working on.\n \u{273B} Saut\u{e9}ed for 5m 33s\n \u{2500}\u{2500}\u{2500}\n \u{276f}";
        assert!(is_rate_limited_credit_banner(footer), "the live specimen must match");
        // Straight apostrophe too.
        assert!(is_rate_limited_credit_banner("/usage-credits to finish what you're working on."));

        // amux CODING the banner: the phrase appears as a code string / comment,
        // NOT as Claude's full footer sentence. Must NOT flag.
        assert!(!is_rate_limited_credit_banner(
            "clean.contains(\"usage-credits to finish\") || clean.contains(\"your usage limit\")"
        ));
        assert!(!is_rate_limited_credit_banner("// the /usage-credits offer banner (AMUX-3203)"));
        // A lane DISCUSSING a third party's limit is not itself limited.
        assert!(!is_rate_limited_credit_banner("the ops key hit its usage limit until 2026-09-01"));
        // A normal working footer is clean.
        assert!(!is_rate_limited_credit_banner(" \u{273B} Crunched for 2m\n \u{276f} type a message"));

        // The 2026-08 wording, live specimen from cold-outbound on 2026-08-19 —
        // 11 lanes carried it while credit_limited reported 0, because none of
        // the sentences above appear in it.
        let parked = "\u{276f}\n  \u{26a0} Usage limit reached \u{b7} continuing automatically at 2am \u{b7} esc to cancel\n  \u{26a0} /usage-credits to continue now";
        assert!(is_rate_limited_credit_banner(parked), "the 2am auto-continue banner must flag");
        // Coding/discussing the wording (no interpunct sentence) must not flag.
        assert!(!is_rate_limited_credit_banner(
            "clean.contains(\"usage limit reached\") // continuing automatically"
        ));
    }


    /// The policy is the human's, set once (D2). Default is `wait` — press 1 —
    /// because a human pressing 1 on sixty lanes is not a workflow and the
    /// answer is the same every time.
    #[test]
    fn the_rate_limit_policy_defaults_to_wait_and_can_be_turned_off() {
        assert_eq!(rate_limit_action(), "wait");
        // `off` is the honest opt-out: detect it, report it, touch nothing.
        assert_ne!("off", rate_limit_action(), "default must not be off");
    }

    /// The resume-mode selector (Ethan, 2026-08-19: "resume from summary,
    /// across models, the only prompt I auto answer outside of the yolo").
    /// The specimen is the incident's own artifact — the selector Ethan
    /// pasted, which a third of the fleet parked on after the panic restart.
    #[test]
    fn resume_mode_prompt_matches_the_selector_not_prose_or_its_own_source() {
        let specimen = "  This session is 2d old and 725.5k tokens.\n\n  Resuming the full session will consume a substantial portion of your usage limits. We recommend resuming from a summary.\n\n  \u{276f} 1. Resume from summary (recommended)\n    2. Resume full session as-is\n    3. Don't ask me again";
        assert!(is_resume_mode_prompt(specimen), "the live specimen must match");

        // Model-agnostic: the header (age, token count) varies per lane and per
        // model, and the highlight may sit on any option or render as '>'.
        // Detection must rest only on the option lines.
        let other = "This session is 5h old and 91.2k tokens.\n > 2. Resume full session as-is\n   1. Resume from summary (recommended)\n   3. Don\u{2019}t ask me again";
        assert!(is_resume_mode_prompt(other), "must not depend on header text or highlight position");

        // The rate-limit menu is a DIFFERENT selector with a different answer.
        let rl = "What do you want to do?\n \u{276f} 1. Stop and wait for limit to reset\n   2. Switch to usage credits";
        assert!(!is_resume_mode_prompt(rl));

        // The resume PICKER (conversation list) is a question for a human.
        let picker = "Resume a conversation\n \u{276f} 1. 2h ago fix the auth bug\n   2. 1d ago gallery review";
        assert!(!is_resume_mode_prompt(picker));

        // This source file on screen: the strings live inside quotes, so the
        // line-anchored option shapes must not fire (the credit-banner lesson,
        // 2026-08-16 — a lane CODING the strings is not sitting on them).
        let code = "    let s = \"1. Resume from summary (recommended)\";\n    // \"2. Resume full session as-is\" and don't ask me again";
        assert!(!is_resume_mode_prompt(code));

        // Prose recounting the choice must not press a key into a working lane.
        let prose = "I chose Resume from summary; Resume full session as-is was too costly. Don't ask me again about it.";
        assert!(!is_resume_mode_prompt(prose));
    }

    /// The dead-letter matrix (AMUX-3110). The cell that must NEVER flip:
    /// `not-running` at any age stays queued — the 2026-08-19 panic held 41
    /// registered lanes down 6.5h and every row delivered on restart, so an
    /// age test on not-running would have destroyed a full fleet's queued
    /// messages during an outage.
    #[test]
    fn dead_letter_takes_only_the_permanently_unroutable_past_grace() {
        let grace = steer_dead_letter_s();
        // The measured leak class, past grace: dead-letter.
        assert_eq!(steer_dead_letter_verdict("no-env-file", grace + 1.0), Some("no-env-file"));
        assert_eq!(steer_dead_letter_verdict("archived", grace + 1.0), Some("archived"));
        // Inside the grace window (recreate/rename race): stays queued.
        assert_eq!(steer_dead_letter_verdict("no-env-file", grace - 1.0), None);
        assert_eq!(steer_dead_letter_verdict("archived", 1.0), None);
        // A stopped-but-registered lane NEVER dead-letters, at any age.
        assert_eq!(steer_dead_letter_verdict("not-running", grace * 300.0), None);
        // Unknown reasons fail safe: keep the row.
        assert_eq!(steer_dead_letter_verdict("busy", grace * 300.0), None);
    }

    /// The policy is the human's, set once (D2). Default is `summary` — Ethan
    /// 2026-08-19 — and `off` is the honest opt-out (detect, stamp
    /// input-required, touch nothing).
    #[test]
    fn the_resume_mode_policy_defaults_to_summary() {
        assert_eq!(resume_mode_action(), "summary");
        assert_ne!("off", resume_mode_action(), "default must not be off");
    }

    #[test]
    fn the_deadline_is_configurable_and_defaults_to_ten_minutes() {
        assert_eq!(steer_max_age_s(), 600.0, "default must be 10 minutes unless AMUX_STEER_MAX_AGE_S is set");
        // The warning threshold must be the SAME number the deliverer uses: a
        // view that disagrees with its mechanism is worse than no view.
        assert_eq!(steer_stall_warn_s(), steer_max_age_s());
    }

    /// THE REGRESSION CORPUS IS THE INCIDENT (AMUX-2785), not a fixture built
    /// for convenience: the three lanes that were actually stalled on
    /// 2026-08-10, in the states they were actually in. The convenient fixture
    /// here would be a lane that is simply busy — and it is convenient
    /// *precisely because* it lacks the property that made the incident, which
    /// is a lane that can never receive at all.
    #[test]
    fn the_three_stalled_lanes_are_distinguished_from_a_merely_busy_one() {
        // amux-agent — 15.2h queued, skip reason `no-env-file`.
        assert_eq!(
            lane_block_reason_from(false, false, false),
            Some("no-env-file"),
            "amux-agent: a lane with no env file is not a worker, and no deadline reaches it"
        );
        // amux-rust-execution — 4.3h queued, and mixpeek-orchestrator at 15.2h.
        assert_eq!(
            lane_block_reason_from(true, false, false),
            Some("not-running"),
            "amux-rust-execution / mixpeek-orchestrator: a stopped lane waits to be STARTED, \
             not to be free"
        );
        // The lane that IS merely busy — amux-cloud and mixpeek-finances were in
        // exactly this state at the same moment, and they are the reason the
        // other three were invisible: all five reported the same "queued".
        assert_eq!(
            lane_block_reason_from(true, false, true),
            None,
            "a running, unarchived lane is deliverable — busy is not blocked, and conflating \
             the two is the whole defect"
        );
        // Archived is its own answer rather than being collapsed into
        // `not-running`: un-archiving is a human's call (ethos rule 8), so the
        // sender needs to be told which of the two they are looking at.
        assert_eq!(lane_block_reason_from(true, true, false), Some("archived"));
        assert_eq!(
            lane_block_reason_from(true, true, true),
            Some("archived"),
            "an archived lane that still has a live pane is still refused — the send path \
             refuses archived, so the queue must not promise otherwise"
        );
    }

    /// The reason code is the actionable half, so it must survive into the text
    /// a human reads. A message that says only "queued" is what produced the
    /// original wrong diagnosis ("i think the amux session is stuck").
    /// AMUX-2796: the two blocked reasons that look alike and are not.
    /// `not-running` is temporary — store the message, tell the truth about it.
    /// `archived` is permanent without a human, so storing it manufactures
    /// immortal mail: two rows were ~16h old, each regenerating stall warnings
    /// and autofix cards that could never clear.
    #[test]
    fn archived_is_the_one_blocked_reason_that_must_not_be_queued() {
        assert_eq!(lane_block_reason_from(true, true, false), Some("archived"));
        assert_eq!(lane_block_reason_from(true, true, true), Some("archived"));
        // The refusal has to publish what to do, or the sender hand-rolls
        // something worse to get past it (the AMUX-2325 shape).
        let msg = block_reason_explain("archived", "old-lane");
        assert!(msg.contains("archived") && msg.contains("old-lane"), "{msg}");
        assert!(msg.contains("human"), "un-archiving is a human's call: {msg}");
        // The temporary ones must NOT share the refusal path — dropping a
        // message to a lane that starts two minutes later is real data loss.
        for r in ["not-running", "no-env-file"] {
            assert_ne!(r, "archived", "only archived is refused at send");
        }
    }

    #[test]
    fn the_sender_is_told_what_will_happen_not_just_what_is_wrong() {
        for reason in ["no-env-file", "not-running", "archived"] {
            let s = block_reason_explain(reason, "amux-agent");
            assert!(
                s.contains("NOT DELIVERABLE"),
                "{reason}: the sender must not read this as a normal queue: {s}"
            );
            assert!(s.contains("amux-agent"), "{reason}: must name the lane: {s}");
        }
        // The two that resolve NEVER must say so — this is the distinction the
        // deadline cannot make, and the sender's only cue to act.
        assert!(block_reason_explain("not-running", "x").contains("STARTED"));
        assert!(block_reason_explain("no-env-file", "x").contains("not waiting for a turn boundary"));
        // An unknown reason still produces a truthful sentence rather than a
        // panic or an empty string: the reason vocabulary is the drain loop's,
        // and it will grow.
        assert!(block_reason_explain("wat", "x").contains("NOT DELIVERABLE"));
    }

    #[test]
    fn picker_shaped_text_takes_the_paste_path_at_any_length() {
        // Three of amux's five starved messages carry an `@`. Typed mid-turn
        // those are LOST (measured 1/1); pasted mid-turn they are accepted
        // (measured 4/4). So the overdue delivery is only safe because
        // picker-shaped text no longer goes through send-keys.
        let short_at = "[04:39 PM] fix the logo @/Users/ethan/.amux/uploads/b7965e0b2a8f-image.png";
        assert!(short_at.chars().count() < 400, "fixture must be under the length threshold");
        assert!(at_picker_text(short_at), "…so ONLY the picker rule can route it to paste");
        let short_plain = "make sure we have a scroll to the bottom thing";
        assert!(!at_picker_text(short_plain));
        assert!(at_picker_text("/compact"));
    }
}

#[cfg(test)]
mod plain_spinner_tests {
    use super::*;

    /// The autodesk specimen (AMUX-3426), rebuilt from Ethan's screenshot:
    /// middle-dot spinner phase, 21m48s elapsed, agents hint on the bar. The
    /// bar's esc-to-interrupt is rightly discounted (AMUX-2959), so the
    /// spinner line is the ONLY honest active signal in this frame — and the
    /// pre-fix parser fell through to `❯ present -> idle`.
    const AUTODESK: &str = "  \u{b7} Befuddling\u{2026} (21m 48s \u{b7} \u{2193} 15.0k tokens)\n\
        \u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f}\u{a0}\n\u{2500}\u{2500}\u{2500}\u{2500}\n  \
        \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt \u{b7} \u{2190} 2 agents\n";

    #[test]
    fn the_middle_dot_spinner_phase_reads_active() {
        assert_eq!(detect_claude_status(AUTODESK), "active");
    }

    #[test]
    fn the_asterisk_phase_with_a_short_timer_reads_active() {
        let f = "  * Reticulating\u{2026} (3s \u{b7} esc to interrupt)\n\u{276f}\u{a0}\n";
        assert_eq!(detect_claude_status(f), "active");
    }

    #[test]
    fn a_prose_bullet_with_an_ellipsis_but_no_timer_is_not_a_spinner() {
        // Shape, not glyph: without the elapsed-time suffix the middle dot is
        // a bullet, and reading it as active would label parked lanes busy.
        let f = "  \u{b7} first point\u{2026} continued below\n\u{2500}\u{2500}\n\u{276f}\u{a0}\n";
        assert_eq!(detect_claude_status(f), "idle");
    }
}

#[cfg(test)]
mod gate_agreement_tests {
    use super::*;

    /// A bypass bar that CONTAINS "esc to interrupt" — a genuinely mid-turn
    /// lane that `detect_claude_status` misreads as idle.
    /// No spinner in frame (it has scrolled off, or the turn is between tool
    /// calls) — only the bar says the turn is live. This is the frame the live
    /// probe deadlocked on, not a constructed one.
    const BUSY_BAR: &str = "  some transcript prose\n\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f}\u{a0}\n\u{2500}\u{2500}\u{2500}\u{2500}\n  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle) \u{b7} esc to interrupt \u{b7} \u{2190} 2 agents\n";

    #[test]
    fn the_gate_and_the_send_path_agree_on_one_frame() {
        // The deadlock: the gate said "boundary" (so no overdue delivery was
        // allowed) while the send path said "generating" (so it refused). Both
        // halves were individually defensible and the message never moved.
        assert!(pane_bar_says_generating(BUSY_BAR), "the send path reads this as mid-turn");
        assert_eq!(
            detect_claude_status(BUSY_BAR),
            "idle",
            "…while detect_claude_status still reads it as idle — the disagreement"
        );
        assert!(
            !pane_is_at_boundary(BUSY_BAR),
            "the gate must resolve the disagreement the SAME way the send path does"
        );
        // …and therefore an old message on this lane becomes an overdue
        // mid-turn delivery instead of being held forever.
        assert_eq!(
            steer_decide(None, Some(pane_is_at_boundary(BUSY_BAR)), 1400.0, 600.0),
            SteerDelivery::OverdueMidTurn
        );
    }
}

#[cfg(test)]
mod pipe_reconcile_tests {
    use super::should_rearm_pipe;

    /// THE POSITIVE CASE: the incident that motivated this — rec-gov, a live
    /// `node` agent with 1 child and pane_pipe=0, logging nothing.
    #[test]
    fn a_live_agent_with_no_pipe_is_rearmed() {
        assert!(should_rearm_pipe(0, 1), "rec-gov's exact shape must re-arm");
        assert!(should_rearm_pipe(0, 5));
    }

    /// THE NEGATIVE CASE, and the one that makes this check worth having.
    /// Without it the test passes trivially by re-arming EVERYTHING, which
    /// would spray shell noise into per-worker logs for lanes with no worker —
    /// 10 of the 11 unpiped panes measured were bare shells (smprobe*, zz-*).
    #[test]
    fn a_bare_shell_is_never_rearmed() {
        assert!(!should_rearm_pipe(0, 0), "a pane whose agent is gone has nothing to log");
    }

    /// An already-piped pane is left alone. Re-arming it would be harmless
    /// (plain pipe-pane is idempotent) but pointless, and doing it every tick
    /// would restart 51 writer processes a minute across the fleet.
    #[test]
    fn an_already_piped_pane_is_left_alone() {
        assert!(!should_rearm_pipe(1, 1));
        assert!(!should_rearm_pipe(1, 0));
    }
}

// ---------------------------------------------------------------------------
// AMUX-2681 — a refusal is not a server error.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod delivery_mode_tests {
    use super::*;

    /// AMUX-2909, pinned. Ethan's report: a message typed into a lane that was
    /// visibly working. This FAILS against the pre-fix predicate
    /// (`chars > 400 || picker_shaped`), which is the whole point — a short
    /// plain message to a generating lane is the exact case that shipped wrong,
    /// and it is also the case that looks least alarming.
    #[test]
    fn a_generating_lane_is_never_typed_into() {
        assert!(
            must_paste(true, 12, false),
            "a SHORT plain message to a generating lane must paste — typed \
             mid-turn was measured lost 1/1, pasted was accepted 4/4"
        );
        // Size and shape must not rescue it either.
        assert!(must_paste(true, 0, false), "even empty-ish text");
        assert!(must_paste(true, 5000, true));
    }

    /// AEAB-25. WHY the mode=type warning next to this predicate is currently
    /// unreachable, stated as an assertion instead of left as a comment.
    ///
    /// `must_paste` is `generating || …`, so inside `if generating { … }` the
    /// paste branch is taken for EVERY input. The old single warning there said
    /// "mode=type here is a regression" while the structured field carried the
    /// mode that actually happened — and it could never be `type`, by
    /// construction. 25 of 25 records all-time were mode="paste", i.e. it fired
    /// only on the healthy path and a real regression would have been one line
    /// among them, indistinguishable.
    ///
    /// The warning is now split so only the genuinely impossible case warns. That
    /// leaves a branch that cannot currently fire, which is fine ONLY while this
    /// invariant holds — so if someone widens `must_paste` such that a generating
    /// lane can be typed into, this fails loudly AND the warning becomes live and
    /// correct at the same moment.
    #[test]
    fn a_generating_lane_always_pastes_so_the_type_warning_stays_unreachable() {
        for chars in [0usize, 1, 12, 400, 401, 5000, 100_000] {
            for picker in [false, true] {
                assert!(
                    must_paste(true, chars, picker),
                    "generating must force paste (chars={chars}, picker={picker}) —                      if this ever fails, the mode=type warning is now reachable and                      must be treated as a live regression signal"
                );
            }
        }
    }

    /// The control, without which the test above passes for a `must_paste` that
    /// simply returned true — an unfailable check is worse than none (rule 7).
    /// An IDLE lane keeps typing: that path is unchanged and adds no latency.
    #[test]
    fn an_idle_lane_still_types_short_plain_text() {
        assert!(!must_paste(false, 12, false), "the fix must not paste everything");
        assert!(!must_paste(false, 400, false), "400 is the boundary, not over it");
        // The two pre-existing reasons to paste still hold on an idle lane.
        assert!(must_paste(false, 401, false), "long text still pastes");
        assert!(must_paste(false, 12, true), "picker-shaped text still pastes");
    }
}

#[cfg(test)]
mod refusal_status_tests {
    use super::*;

    /// THE incident, pinned: 15 of the 19 errors in the 6h window on
    /// 2026-08-10 were this one refusal shipping as HTTP 500 — `amux`
    /// unreachable 5.04h across 12 sends from 3 distinct clients, and the
    /// dashboard showing a server error for a lane amux had correctly
    /// declined to type into. Fails against the pre-fix code, which mapped
    /// everything except the literal "not running" to 500.
    #[test]
    fn background_conversation_refusal_is_409_not_500() {
        for generating in [true, false] {
            let msg = bg_view_refusal(generating);
            let (code, fix) = send_failure_status(&msg);
            assert_eq!(code, StatusCode::CONFLICT, "refusal must be 409: {msg}");
            assert!(fix.is_some(), "a refusal must name a next step: {msg}");
            assert!(
                msg.starts_with(BG_VIEW_REFUSAL_PREFIX),
                "every variant keeps the classified prefix: {msg}"
            );
        }
        // The two variants must be DISTINGUISHABLE — "wait for the boundary"
        // and "go look at the pane" are different next steps, and a caller
        // that cannot tell them apart cannot act on either.
        assert_ne!(bg_view_refusal(true), bg_view_refusal(false));
    }

    /// Every other honest decline that used to wear a 500.
    #[test]
    fn state_refusals_are_conflicts() {
        for msg in [
            "not running",
            "session is in resume picker",
            "session at a selector — retry at next idle boundary",
            "session started generating — retry at next turn boundary",
            "session is blocked; remove it from blocked-sessions.txt first",
            "session is archived; wake it first",
            "terminal client attached — its size wins",
            "no agents panel on screen",
        ] {
            let (code, fix) = send_failure_status(msg);
            assert_eq!(code, StatusCode::CONFLICT, "{msg}");
            assert!(fix.is_some(), "{msg} must carry a next step");
        }
    }

    /// The non-409 cells, so "everything became a 409" cannot pass either.
    #[test]
    fn not_found_bad_request_and_absent_capability_keep_their_own_codes() {
        assert_eq!(send_failure_status("session 'nope' not found").0, StatusCode::NOT_FOUND);
        assert_eq!(send_failure_status("invalid session name").0, StatusCode::BAD_REQUEST);
        assert_eq!(
            send_failure_status("key 'Fkey' not in allowed set").0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send_failure_status("iTerm2-backed sessions are not supported by the rust origin yet").0,
            StatusCode::NOT_IMPLEMENTED,
        );
    }

    /// A 500 must still be reachable. If the classifier swallowed everything,
    /// the autofix watcher would go blind — which is the same defect in the
    /// other direction.
    #[test]
    fn real_failures_are_still_500() {
        for msg in [
            "send-keys failed",
            "paste-buffer failed",
            "could not stage paste buffer",
            "tmux not found or timed out",
            "Claude failed to start",
            "could not write session env",
            "auto-wake failed: tmux refused",
            "herdr prompt failed",
        ] {
            let (code, fix) = send_failure_status(msg);
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{msg}");
            assert!(fix.is_none(), "an unhandled failure has no honest next step: {msg}");
        }
    }

    /// The drift check, and the reason deriving from a string is affordable.
    ///
    /// Extracts every `(false, "…")` literal from THIS FILE at compile time
    /// and requires each to be classified deliberately — as a refusal (never
    /// 5xx) or as a hard failure (500). A refusal added later cannot land in
    /// the default 500 bucket unnoticed, which is exactly how the
    /// background-conversation guard became 14 of the night's 19 errors: it
    /// was written correctly and inherited a status code nobody chose.
    ///
    /// This is the check that CAN fail: it is built from the shipped source,
    /// not from a paraphrase of it, so it also fails if someone reworders an
    /// existing literal out from under the classifier.
    #[test]
    fn every_send_failure_literal_is_classified() {
        const SRC: &str = include_str!("session_verbs.rs");
        // Built by concat! so this marker never appears literally in this
        // file — otherwise the scan would find its own needle.
        //
        // NOTE the marker stops at the COMMA, not at the quote. The first
        // version required `(false, "` contiguously and so was blind to the
        // rustfmt-wrapped form —
        //     return (
        //         false,
        //         "herdr-backed session start is not ported ...".into(),
        //     );
        // — which is how EVERY long refusal in this file is written. The scan
        // reported a clean 15 outcomes while silently skipping the wrapped
        // ones: a probe that guessed where the answer lived and missed. It now
        // skips whitespace after the comma before requiring the quote.
        let marker = concat!("fal", "se,");
        // Literals that MUST classify as a hard failure. Everything else the
        // scan finds must be a refusal (non-5xx). Both lists are explicit:
        // "I did not think about it" is not a state this test has.
        // 501, and NOT a bug: the capability is absent, not broken. Kept in
        // its own list because the autofix watcher must never file these —
        // same class as /api/email/search on an unconnected account.
        let degraded: &[&str] = &[
            "iTerm2-backed sessions are not supported by the rust origin yet",
            "herdr-backed session start is not ported to the rust origin yet \
             (gap named in api/session_verbs.rs)",
        ];
        let hard: &[&str] = &[
            "herdr prompt failed",
            "could not stage paste buffer",
            "paste-buffer failed",
            "send-keys failed",
            "timeout sending keys",
            "tmux not found or timed out",
            "Claude failed to start",
            "could not write session env",
        ];
        let mut found = 0usize;
        let mut seen: Vec<String> = Vec::new();
        let mut unclassified: Vec<String> = Vec::new();
        let mut at = 0usize;
        while let Some(i) = SRC[at..].find(marker) {
            let hit = at + i;
            at = hit + marker.len();
            // SKIP COMMENTS. The first run of this test failed on the doc
            // comment above, which quotes its own needle — the "positional
            // match landed on the fix's own comment" failure, self-inflicted
            // within minutes of writing the rule down. That comment is left in
            // place deliberately: it is the fixture proving this skip works.
            let line_start = SRC[..hit].rfind('\n').map_or(0, |n| n + 1);
            if SRC[line_start..hit].trim_start().starts_with("//") {
                continue;
            }
            // IT MUST BE A TUPLE RETURN, not any `false,` followed by a
            // string. Widening the marker to catch the rustfmt-wrapped form
            // also caught `json!({"ok": false, "message": ...})` — 11 JSON
            // KEYS reported as unclassified refusals. The discriminator is the
            // open paren: `(false,` and `(\n false,` both have `(` as the
            // previous non-whitespace char; a JSON literal has `:` or `,`.
            if !SRC[..hit].trim_end().ends_with('(') {
                continue;
            }
            // Skip whitespace/newlines between the comma and the literal.
            let ws = SRC[at..].len() - SRC[at..].trim_start().len();
            if !SRC[at + ws..].starts_with('"') {
                continue;
            }
            let rest = &SRC[at + ws + 1..];
            // Read to the closing quote, honouring \" and \\ escapes.
            let bytes = rest.as_bytes();
            let mut j = 0usize;
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => j += 2,
                    b'"' => break,
                    _ => j += 1,
                }
            }
            if j >= bytes.len() {
                continue;
            }
            let raw = &rest[..j];
            // Rust's `\`-at-end-of-line continuation eats the newline and the
            // following indentation; reproduce that so a wrapped literal
            // compares equal to the runtime string.
            let mut lit = String::new();
            let mut it = raw.chars().peekable();
            while let Some(c) = it.next() {
                if c == '\\' {
                    match it.peek() {
                        Some('\n') => {
                            it.next();
                            while it.peek().is_some_and(|c| c.is_whitespace()) {
                                it.next();
                            }
                        }
                        Some('"') => {
                            it.next();
                            lit.push('"');
                        }
                        Some('\\') => {
                            it.next();
                            lit.push('\\');
                        }
                        _ => lit.push(c),
                    }
                } else {
                    lit.push(c);
                }
            }
            if lit.trim().is_empty() {
                continue;
            }
            found += 1;
            seen.push(lit.clone());
            let (code, _) = send_failure_status(&lit);
            if degraded.contains(&lit.as_str()) {
                assert_eq!(code, StatusCode::NOT_IMPLEMENTED, "absent capability: {lit:?}");
            } else if hard.contains(&lit.as_str()) {
                assert_eq!(
                    code,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "listed as a hard failure but classified {code}: {lit:?}"
                );
            } else if code.is_server_error() {
                // Collected, not asserted one at a time: a future author who
                // adds three refusals should see all three, not rerun the
                // suite three times.
                unclassified.push(lit.clone());
            }
        }
        // The scan itself must have found something — an extraction bug that
        // matched nothing would pass every assertion above in silence.
        assert!(
            unclassified.is_empty(),
            "{} UNCLASSIFIED session-verb outcome(s) fall through to HTTP 500. For each: add a \
             refusal arm to send_failure_status (with a next step), or add the literal to this \
             test's `hard` list if a 500 is genuinely correct.\n{unclassified:#?}",
            unclassified.len()
        );
        // POSITIVE CONTROL. Before trusting that the scan found everything,
        // confirm it found the specimen that motivated widening it — the
        // rustfmt-wrapped herdr refusal, which the contiguous `(false, "`
        // marker could not see. Without this line the scan can silently narrow
        // again and every assertion above still passes.
        assert!(
            seen.iter().any(|l| l.starts_with("herdr-backed session start is not ported")),
            "the scan no longer sees rustfmt-wrapped refusals — it found {} literals: {seen:#?}",
            seen.len()
        );
        assert!(found >= 20, "literal scan found only {found} outcomes — extraction is broken");
    }
}

#[cfg(test)]
mod stall_log_dedupe_tests {
    /// AEAB-13. The stall warning fired every tick with no memory: 921 of 1004
    /// log lines since one restart, 3570 across the preceding 24 hours, all for
    /// ONE undeliverable message to a lane dead six days. It buried a
    /// first-ever `database is locked` line and two false scheduler warnings
    /// during a log review that existed to find exactly those.
    ///
    /// The hourly-dedupe properties MOVED to `crate::log_dedupe::tests` when
    /// AEAB-45 gave the helper a second caller — they are properties of the
    /// mechanism, not of steering, and duplicating them here would let the two
    /// copies drift exactly as the two spellings of the mechanism would have.
    /// What remains here is the wiring: this lane's key shape.
    #[test]
    fn the_stall_key_is_per_lane_and_per_condition() {
        // The key must carry BOTH, because `not-running` and `no-env-file` on
        // the same lane are separately actionable — collapsing them is what made
        // the second one invisible in the first place.
        let k = |session: &str, cond: &str| format!("{session}:{cond}");
        assert_ne!(k("Amux-gtm", "not-running"), k("Amux-gtm", "no-env-file"));
        assert_ne!(k("Amux-gtm", "not-running"), k("other-lane", "not-running"));
    }
}

#[cfg(test)]
mod roster_tests {
    use super::*;
    /// The roster is DERIVED from the session env files, so these assertions
    /// are about shape rather than content — the fleet changes hourly and a
    /// fixture would be a second source of truth (the thing the roster exists
    /// to avoid).
    /// SUPERSEDES `a_worker_never_lists_itself`, which asserted the OPPOSITE and
    /// was wrong (AMUX-2831). Excluding the reader is only expressible when each
    /// reader gets its own file, and they do not: MEMORY.md is keyed on the
    /// PROJECT DIRECTORY, so 17 lanes share the one under ~/Dev/mixpeek. A
    /// self-excluding roster is therefore correct for whichever lane wrote last
    /// and wrong for the other sixteen — they read a list that includes
    /// themselves and omits the writer. That is the last-writer-wins bug this
    /// card is about, and the roster was an instance of it.
    /// Anything written into Claude's MEMORY.md must be true for EVERY reader,
    /// because the file is keyed on the project DIRECTORY and up to 18 lanes
    /// share one. The roster obeys this by listing everyone; the worker-memory
    /// block obeys it by NAMING ITS OWNER (AMUX-2831).
    #[test]
    fn propagated_worker_memory_names_the_lane_that_wrote_it() {
        let b = compose_worker_block("amux", "remember: the thing");
        assert!(b.contains("Worker memory — `amux`"), "must name its owner; got:\n{b}");
        assert!(
            b.contains("If you are not amux"),
            "a peer reading this must be told it is not theirs; got:\n{b}"
        );
        assert!(b.contains("remember: the thing"), "content must survive labelling");
    }

    /// A header over nothing is itself a claim ("this lane recorded nothing").
    #[test]
    fn no_worker_memory_block_when_the_lane_recorded_nothing() {
        assert_eq!(compose_worker_block("amux", "   \n  "), "");
        assert_eq!(compose_worker_block("amux", ""), "");
    }

    #[test]
    fn the_roster_lists_every_live_worker_because_the_file_is_shared() {
        let r = super::fleet_roster();
        if r.is_empty() {
            return; // no live workers on this machine; nothing to assert
        }
        // It must say so, or a reader seeing its own name concludes the roster
        // is buggy — which is what a correct shared roster looks like.
        assert!(
            r.contains("INCLUDING YOU"),
            "a shared roster must tell the reader it lists them too: {r}"
        );
        assert!(r.contains("$AMUX_SESSION"), "and how to identify themselves in it: {r}");
    }

    /// Empty means EMPTY — a single-worker install must not get a table header
    /// with no rows under it, which reads as "the fleet is broken".
    #[test]
    fn a_roster_with_no_peers_is_the_empty_string() {
        let r = super::fleet_roster();
        assert!(r.is_empty() || r.contains("| `"), "header without rows: {r}");
    }

}
