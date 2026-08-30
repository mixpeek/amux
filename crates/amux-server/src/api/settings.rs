//! Settings API: the four `/api/settings/*` endpoints the SPA's Settings tab
//! calls, ported from amux-server.py (`/api/settings/default-model`,
//! `/api/settings/commit-guard`, `/api/settings/task-guard`,
//! `/api/settings/env`).
//!
//! All four read/write env FILES under the amux home — `server.env` and
//! `defaults.env` — not the database, so none of them touch AppState. The
//! home is resolved per-request from `$AMUX_HOME` (legacy `$CC_HOME`), the
//! same rule `config.rs` uses, which is also how tests point writes at a
//! temp home instead of the live `~/.amux`.
//!
//! Python parity decisions, recorded so they are not "fixed" later:
//! - Python loads server.env into `os.environ` at boot (non-empty values
//!   OVERRIDE process env) and every PATCH mutates `os.environ` for "live
//!   effect". Rust has no safe process-global env mutation story, so reads
//!   go file-first instead: a non-empty server.env value wins, then process
//!   env, then the default. Observable behavior matches Python: a PATCH is
//!   immediately visible to the next GET without a restart.
//! - The Python ANTHROPIC_API_KEY PATCH also re-inits claude config and
//!   pushes the key into every running tmux session. That is the Python
//!   server's runtime to manage — while it owns the tmux fleet (Phase 11
//!   cutover pending), duplicating the push here would race it. Not ported.
//! - `defaults.env` writes are atomic with mode 0600 (Python's
//!   `_atomic_write_secure`); `server.env` writes are plain rewrites
//!   (Python's are too).
//! - Flag surgery (`--model X` / `--model=X`) uses a POSIX shlex
//!   split/quote port so quoted multi-word values survive, and malformed
//!   flags fail loudly with Python's exact 400 message instead of wiping
//!   the user's other flags.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::path::Path;

use super::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/default-model", get(get_default_model_h).patch(patch_default_model))
        .route("/commit-guard", get(get_commit_guard).patch(patch_commit_guard))
        .route("/task-guard", get(get_task_guard).patch(patch_task_guard))
        .route("/env", get(get_env).patch(patch_env))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

pub(crate) use crate::config::amux_home;

/// Effective value of a server-config key: non-empty `server.env` entry
/// first (Python's boot loader only overrides `os.environ` with non-empty
/// values), then process env, then None. File-first is what makes a PATCH
/// visible to the next GET without mutating process env.
/// `pub(crate)`: the alert-config endpoints (api/alerts.rs) read the same
/// keys the same way — one resolver, not two spellings of it.
pub(crate) fn effective_env(home: &Path, key: &str) -> Option<String> {
    let file_env = crate::config::parse_env_file(&home.join("server.env"));
    // PRESENT-BUT-EMPTY MEANS CLEARED, and it is not the same as ABSENT.
    //
    // This used to fall through to the process env whenever the file value was
    // empty, which made clearing a key impossible: config.rs exports server.env
    // into the PROCESS env at startup (setdefault), so a key that was ever saved
    // and survived one restart is in std::env for the life of the process. The
    // clear then wrote `ANTHROPIC_API_KEY=` to the file — correctly — and the
    // GET kept serving the old key from the process env.
    //
    // Reproduced end to end on a scratch home 2026-08-11: seed a key, GET masks
    // it, PATCH {"ANTHROPIC_API_KEY":""} returns {"ok":true}, the file is
    // emptied, and the very next GET still returns *******************wxyz.
    // Nothing anywhere reported a failure — the write succeeded, the read lied.
    //
    // That is a security defect, not a papercut: rotating or revoking a key is
    // the one operation you must be able to trust, and amux would keep using the
    // old value while telling you it was gone.
    if let Some(v) = file_env.get(key) {
        return (!v.is_empty()).then(|| v.clone());
    }
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Python's server.env line-replace: rewrite the first `KEY=`/`KEY =` line,
/// else append. Non-atomic plain write, matching Python (`_env_set`).
/// `pub(crate)`: shared with the alert-config PATCH (api/alerts.rs), which
/// is Python's `_env_set` on the same file.
/// Does this config VALUE point into storage the box REAPS BY AGE? Such a value
/// is a time bomb: the file is cleaned up long after the config stops being
/// looked at, so a working config silently breaks later — exactly the
/// GOOGLE_SA_KEY_FILE 502 (AMUX-3383). "Ephemeral" is DEFINED by the storage
/// reaper (`storage::AGE_PRUNED_DIRS`), not duplicated here (nissan, AMUX-3386):
/// whatever it age-prunes is by definition unsafe to persist durable config into,
/// so adding a scratch dir there extends this guard automatically instead of
/// leaving a second copy of the knowledge to drift. Today that is media-cache,
/// uploads and spin-dumps — the original guard knew only uploads and silently
/// passed the other two.
pub(crate) fn is_ephemeral_path(home: &Path, val: &str) -> bool {
    let v = val.trim().trim_matches('"');
    if v.is_empty() {
        return false;
    }
    let expanded = match v.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|h| Path::new(&h).join(rest))
            .unwrap_or_else(|_| Path::new(v).to_path_buf()),
        None => Path::new(v).to_path_buf(),
    };
    crate::runtime_jobs::storage::AGE_PRUNED_DIRS
        .iter()
        .any(|(name, _, _)| expanded.starts_with(home.join(name)))
}

pub(crate) fn set_server_env_key(home: &Path, key: &str, val: &str) -> std::io::Result<()> {
    // Never persist a config value that points into ephemeral uploads/ storage —
    // the file gets cleaned up and the config silently breaks (AMUX-3386/3383).
    if is_ephemeral_path(home, val) {
        tracing::warn!(
            key = %key,
            "refusing to persist an ephemeral ~/.amux/uploads/ path into server.env (AMUX-3386) — copy the file to a stable location first"
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to write {key} = a path under ~/.amux/uploads/ into server.env: \
                 files there are cleaned up and the config would silently break later \
                 (AMUX-3383). Copy it to a stable location (e.g. ~/.amux/gcp/) and set {key} to that."
            ),
        ));
    }
    let file = home.join("server.env");
    let mut lines: Vec<String> = std::fs::read_to_string(&file)
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default();
    let mut found = false;
    for line in lines.iter_mut() {
        if line.starts_with(&format!("{key}=")) || line.starts_with(&format!("{key} =")) {
            *line = format!("{key}={val}");
            found = true;
            break;
        }
    }
    if !found {
        lines.push(format!("{key}={val}"));
    }
    std::fs::create_dir_all(home)?;
    std::fs::write(&file, lines.join("\n") + "\n")
}

/// Python's `_atomic_write_secure`: temp file in the same dir, chmod 0600,
/// rename over the target — no TOCTOU window, no partially-written file.
fn atomic_write_secure(path: &Path, content: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("env"),
        std::process::id()
    ));
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// JSON truthiness with Python `bool()` semantics — the guard PATCHes run
/// the body value through `bool(...)`, so `0`, `""`, `[]`, `{}` disable.
pub(crate) use super::py_truthy as truthy;

// ---- shlex port (Python shlex.split / shlex.quote, POSIX mode) ------------

/// POSIX-mode `shlex.split`. Errors with Python's message on an unclosed
/// quote so the 400 the user sees names the same problem.
pub(crate) fn shlex_split(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_token = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_token {
                    out.push(std::mem::take(&mut cur));
                    in_token = false;
                }
            }
            '\'' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '"' => {
                in_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        // Inside double quotes, backslash escapes only \" and
                        // \\ (Python shlex posix rules); otherwise it is kept.
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\')) => cur.push(e),
                            Some(other) => {
                                cur.push('\\');
                                cur.push(other);
                            }
                            None => return Err("No closing quotation".into()),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '\\' => {
                in_token = true;
                match chars.next() {
                    Some(ch) => cur.push(ch),
                    None => return Err("No escaped character".into()),
                }
            }
            ch => {
                in_token = true;
                cur.push(ch);
            }
        }
    }
    if in_token {
        out.push(cur);
    }
    Ok(out)
}

/// Python `shlex.quote`: safe charset passes through, everything else gets
/// single-quoted with the `'"'"'` dance.
pub(crate) fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Python `_strip_model_from_flags`: remove `--model X` / `--model=X`,
/// re-quote the rest. Err on malformed input — the caller MUST surface it
/// rather than silently wiping the user's flags.
pub(crate) fn strip_model_from_flags(flags: &str) -> Result<String, String> {
    if flags.is_empty() {
        return Ok(String::new());
    }
    let tokens = shlex_split(flags)?;
    let mut filtered: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "--model" && i + 1 < tokens.len() {
            i += 2;
            continue;
        }
        if t.starts_with("--model=") {
            i += 1;
            continue;
        }
        filtered.push(t.clone());
        i += 1;
    }
    Ok(filtered.iter().map(|t| shlex_quote(t)).collect::<Vec<_>>().join(" "))
}

/// Python `_extract_model_from_flags`: read-only, so malformed input
/// silently yields "" (display fallback, not surgery).
pub(crate) fn extract_model_from_flags(flags: &str) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let Ok(tokens) = shlex_split(flags) else {
        return String::new();
    };
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "--model" && i + 1 < tokens.len() {
            return tokens[i + 1].clone();
        }
        if let Some(v) = t.strip_prefix("--model=") {
            return v.to_string();
        }
        i += 1;
    }
    String::new()
}

const MODEL_ID_MAX_LEN: usize = 255;

/// Python `_validate_model_name`: string, <=255 chars, `[A-Za-z0-9._:\[\]@/+-]+`
/// with no leading hyphen (the regex's `(?!-)` lookahead, expressed directly
/// since the regex crate has no lookahead). Empty is allowed — it means
/// "clear the override".
pub(crate) fn validate_model_name(v: &Value) -> Result<String, String> {
    let Value::String(s) = v else {
        return Err("model must be a string".into());
    };
    let normalized = s.trim().to_string();
    if normalized.len() > MODEL_ID_MAX_LEN {
        return Err(format!("model name too long (max {MODEL_ID_MAX_LEN} chars)"));
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || "._:[]@/+-".contains(c);
    if !normalized.is_empty() && (normalized.starts_with('-') || !normalized.chars().all(allowed)) {
        return Err(
            "invalid model name (allowed: alphanumeric and ._:[]@/+-, no leading hyphen)".into(),
        );
    }
    Ok(normalized)
}

/// Python `_get_default_model`: `--model` out of defaults.env's
/// CC_DEFAULT_FLAGS, falling back to "sonnet".
pub(crate) fn get_default_model(home: &Path) -> String {
    let defaults = home.join("defaults.env");
    if defaults.exists() {
        let cfg = crate::config::parse_env_file(&defaults);
        let model = extract_model_from_flags(cfg.get("CC_DEFAULT_FLAGS").map(String::as_str).unwrap_or(""));
        if !model.is_empty() {
            return model;
        }
    }
    "sonnet".into()
}

/// The PATCH body's model applied to defaults.env — Python's handler, line
/// for line: single read (no TOCTOU), strip the old `--model` while
/// PRESERVING every other flag, quote-wrap, atomic 0600 write.
pub(crate) fn patch_default_model_file(home: &Path, model: &str) -> Result<(), (StatusCode, Value)> {
    let defaults = home.join("defaults.env");
    let mut lines: Vec<String> = if defaults.exists() {
        std::fs::read_to_string(&defaults)
            .map(|s| s.lines().map(String::from).collect())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })))?
    } else {
        Vec::new()
    };
    let mut existing_flags = String::new();
    for line in &lines {
        if let Some(value) = line.strip_prefix("CC_DEFAULT_FLAGS=") {
            let mut value = value;
            // Strip outer matching quotes (mirrors parse_env_file).
            let bytes = value.as_bytes();
            if bytes.len() >= 2
                && bytes[0] == bytes[bytes.len() - 1]
                && (bytes[0] == b'"' || bytes[0] == b'\'')
            {
                value = &value[1..value.len() - 1];
            }
            existing_flags = value.to_string();
            break;
        }
    }
    let flags_no_model = strip_model_from_flags(&existing_flags).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": format!(
                "existing CC_DEFAULT_FLAGS in defaults.env is malformed ({e}); fix the file manually before updating the model via API"
            ) }),
        )
    })?;
    let new_flag_value = if !model.is_empty() {
        if !flags_no_model.is_empty() {
            format!("--model {model} {flags_no_model}").trim().to_string()
        } else {
            format!("--model {model}")
        }
    } else {
        flags_no_model
    };
    let new_line = format!("CC_DEFAULT_FLAGS=\"{new_flag_value}\"");
    let mut found = false;
    for line in lines.iter_mut() {
        if line.starts_with("CC_DEFAULT_FLAGS=") {
            *line = new_line.clone();
            found = true;
            break;
        }
    }
    if !found {
        lines.push(new_line);
    }
    let content = lines.join("\n") + "\n";
    atomic_write_secure(&defaults, &content)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })))
}

// ---- /api/settings/default-model ------------------------------------------

async fn get_default_model_h() -> Response {
    Json(json!({ "model": get_default_model(&amux_home()) })).into_response()
}

async fn patch_default_model(Json(body): Json<Value>) -> Response {
    if !body.is_object() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "payload must be a JSON object" }));
    }
    // Python: body.get("model", "") — absent means "clear the override".
    let model_v = body.get("model").cloned().unwrap_or_else(|| Value::String(String::new()));
    let model = match validate_model_name(&model_v) {
        Ok(m) => m,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    match patch_default_model_file(&amux_home(), &model) {
        Ok(()) => Json(json!({ "ok": true, "model": model })).into_response(),
        Err((status, body)) => err(status, body),
    }
}

// ---- /api/settings/commit-guard and /api/settings/task-guard ---------------

/// Python `_commit_guard_enabled`: default ON, disabled only by an explicit
/// falsy spelling.
pub(crate) fn commit_guard_enabled(home: &Path) -> bool {
    let val = effective_env(home, "AMUX_COMMIT_GUARD").unwrap_or_else(|| "1".into());
    !matches!(val.trim().to_lowercase().as_str(), "0" | "false" | "off" | "no")
}

/// Python `_task_guard_enabled`: default OFF, opt-in spelling required.
pub(crate) fn task_guard_enabled(home: &Path) -> bool {
    let val = effective_env(home, "AMUX_TASK_GUARD").unwrap_or_else(|| "0".into());
    matches!(val.trim().to_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

async fn get_commit_guard() -> Response {
    Json(json!({ "enabled": commit_guard_enabled(&amux_home()) })).into_response()
}

async fn patch_commit_guard(Json(body): Json<Value>) -> Response {
    // Python: bool(body.get("enabled", True)).
    let enabled = body.get("enabled").map(truthy).unwrap_or(true);
    let val = if enabled { "1" } else { "0" };
    match set_server_env_key(&amux_home(), "AMUX_COMMIT_GUARD", val) {
        Ok(()) => Json(json!({ "ok": true, "enabled": enabled })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    }
}

async fn get_task_guard() -> Response {
    Json(json!({ "enabled": task_guard_enabled(&amux_home()) })).into_response()
}

async fn patch_task_guard(Json(body): Json<Value>) -> Response {
    // Python: bool(body.get("enabled", False)).
    let enabled = body.get("enabled").map(truthy).unwrap_or(false);
    let val = if enabled { "1" } else { "0" };
    match set_server_env_key(&amux_home(), "AMUX_TASK_GUARD", val) {
        Ok(()) => Json(json!({ "ok": true, "enabled": enabled })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    }
}

// ---- /api/settings/env ------------------------------------------------------

/// The only keys the settings UI may read (masked) or write. Fixed array,
/// not a set: response key order is stable.
const ALLOWED_ENV_KEYS: [&str; 4] =
    ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY", "GOOGLE_API_KEY"];

/// Python's mask: >8 chars shows stars + last 4; short-but-set shows "set";
/// unset shows "". Never the value itself.
pub(crate) fn mask_secret(v: &str) -> String {
    let n = v.chars().count();
    if n > 8 {
        let last4: String = v.chars().skip(n - 4).collect();
        format!("{}{last4}", "*".repeat(n - 4))
    } else if n > 0 {
        "set".into()
    } else {
        String::new()
    }
}

async fn get_env() -> Response {
    let home = amux_home();
    let mut out = Map::new();
    for k in ALLOWED_ENV_KEYS {
        let v = effective_env(&home, k).unwrap_or_default();
        out.insert(k.to_string(), Value::String(mask_secret(&v)));
    }
    Json(Value::Object(out)).into_response()
}

async fn patch_env(Json(body): Json<Value>) -> Response {
    let updates: Vec<(String, String)> = body
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, v)| ALLOWED_ENV_KEYS.contains(&k.as_str()) && v.is_string())
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    if updates.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "no valid keys" }));
    }
    let home = amux_home();
    for (key, val) in &updates {
        if let Err(e) = set_server_env_key(&home, key, val) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
        }
    }
    // Python also mutates os.environ and pushes ANTHROPIC_API_KEY into
    // running tmux sessions; see the module doc for why neither is ported.
    Json(json!({ "ok": true })).into_response()
}

// ---------------------------------------------------------------------------
// Shared test plumbing: AMUX_HOME is process-global, so every test that sets
// it (here, journal media, history group) must hold this lock, and the RAII
// guard restores the previous value even on panic. NEVER point it at the
// real ~/.amux.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard};

    pub static LOCK: Mutex<()> = Mutex::new(());

    /// Keys that leak from the REAL machine into a temp-home test.
    ///
    /// `ServerConfig::load` exports server.env into the PROCESS env
    /// (config.rs — deliberate, it is the python setdefault parity that made
    /// server.env flags actually work). But `effective_env` falls back to the
    /// process env when a key is absent from the home's file, so once ANY test
    /// loads the real ~/.amux, that machine's values are visible to every later
    /// test — whatever home they set.
    ///
    /// That made `owner_alert_respects_channel_config` fail roughly 1 run in 3
    /// under `cargo test --workspace`: it asserts "no channels configured", and
    /// found the developer's real AMUX_OWNER_PHONE, so the alert went out over
    /// sms. Order-dependent, hence intermittent, hence read as "flaky test"
    /// rather than "the test can see the machine".
    ///
    /// A temp home must mean a clean slate. Cleared here rather than in each
    /// test because the guard already holds LOCK, so this is the one place the
    /// mutation is race-free. Restored on drop.
    ///
    /// THE FLOOR, not the list (AMUX-2675). This used to BE the list — a single
    /// hand-maintained key — and the next two leaks were already sitting in the
    /// same file on the same machine: `AMUX_URGENT_PUSH` and `AMUX_URGENT_SMS`
    /// are read through the identical `effective_env` fallback at alerts.rs:254,
    /// so a machine with `AMUX_URGENT_PUSH=0` silently disabled the push channel
    /// inside temp-home tests. That is what the residual flake actually was:
    /// `owner_alert_60s_dedupe_and_ledger_visibility` (0 pushes, expected 1) and
    /// `owner_alert_reports_channel_failures_per_contract` (channels.push
    /// absent, expected the vapid error) — NOT the single test AMUX-2675 named.
    ///
    /// Enumerated from the LEAK SOURCE instead: see [`leaky_keys`]. Widening a
    /// hand list one key at a time is how this recurs, because the list and the
    /// thing it models are maintained in different places by different people.
    const LEAKY_KEYS_FLOOR: &[&str] = &["AMUX_OWNER_PHONE", "AMUX_URGENT_PUSH", "AMUX_URGENT_SMS"];

    /// EVERY key the real machine can leak, derived from the file that leaks
    /// them rather than from a list someone must remember to update.
    ///
    /// The leak path is exactly one: `ServerConfig::load` exports
    /// `$HOME/.amux/server.env` into the process env, and `effective_env` falls
    /// back to the process env whenever a key is absent from the TEMP home's
    /// file. So the set of keys that can leak IS the set of keys in that file —
    /// 39 of them on this machine, of which the old list covered one. Reading
    /// the file makes the fix cover the 40th key nobody has added yet.
    ///
    /// Only the NAMES are read; values are never touched, logged, or compared
    /// (that file holds credentials — docs/credentials.md). Cached because
    /// `set_home` is called by ~30 tests and the answer cannot change during a
    /// run. Absent file (CI) yields just the floor, which is correct: with no
    /// server.env there is nothing to leak, which is why CI never saw this.
    fn leaky_keys() -> &'static [String] {
        static KEYS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        KEYS.get_or_init(|| {
            let mut keys: Vec<String> = LEAKY_KEYS_FLOOR.iter().map(|s| s.to_string()).collect();
            // The REAL home, never AMUX_HOME — AMUX_HOME may already point at a
            // previous test's temp dir, and the machine's file is what leaks.
            if let Some(home) = std::env::var_os("HOME") {
                let f = std::path::Path::new(&home).join(".amux").join("server.env");
                if let Ok(text) = std::fs::read_to_string(f) {
                    for line in text.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        if let Some((k, _)) = line.split_once('=') {
                            let k = k.trim();
                            if !k.is_empty() && !keys.iter().any(|e| e == k) {
                                keys.push(k.to_string());
                            }
                        }
                    }
                }
            }
            keys
        })
    }

    pub struct HomeGuard {
        prev: Option<String>,
        prev_leaky: Vec<(&'static str, Option<String>)>,
        /// The whole process env as it stood when the guard was taken, used to
        /// restore the fixture's OWN keys. See the Drop impl: a static key list
        /// cannot bound what a fixture home may export, and a blanket restore
        /// over-reaches into keys other tests own.
        snapshot: Vec<(String, String)>,
        /// The fixture home, so Drop can ask its `server.env` which keys this
        /// guard's window could have exported.
        home: std::path::PathBuf,
        _g: MutexGuard<'static, ()>,
    }

    /// Point AMUX_HOME at a fixture for this guard's lifetime.
    ///
    /// COVERAGE IS ONE-DIRECTIONAL, and the honest scope matters
    /// (AMUX-3415): the process-wide LOCK serializes home-MUTATING tests
    /// against each other — two guards can never interleave — but nothing
    /// makes the ~79 `amux_home()` READ sites take it, so a guardless test
    /// reading a home concurrently with a guard's window sees the fixture
    /// home. Accepted with ~43 users; the promotion path is routing test reads
    /// through the injected-lookup seam `config::resolve_home(get)` already
    /// provides (built for exactly this), not a bigger lock. Until then: prefer
    /// that seam over this guard for NEW tests when the code under test can
    /// take an injected lookup — every test that does shrinks the exposure.
    ///
    /// THE "ZERO OBSERVED BITES" CLAUSE IS SPENT (AMUX-3719, 2026-08-25). One
    /// was observed: `owner_alert_full_send_shape_channels_and_ledger` read a
    /// pin that only exists in another test's fixture home, once in 4 full-suite
    /// runs. That is the trigger this comment named, so the exit condition is
    /// live rather than hypothetical.
    ///
    /// It does not match the exposure described above, which is what makes it
    /// worth writing down instead of just fixing: BOTH tests involved hold a
    /// guard, and two guards cannot interleave. Ruled out with the code, so
    /// nobody re-runs them: `set_server_env_key` writes only the file and never
    /// the process env; every unguarded `set_var("AMUX_HOME")` is in `tests/`,
    /// which are separate binaries. The mechanism is still unknown and the flake
    /// did not reproduce in three subsequent full runs. The failing assertion now
    /// prints the fixture home, `AMUX_HOME`, and the resolved home, so the next
    /// occurrence identifies its own cause instead of costing another
    /// investigation that ends here.
    pub fn set_home(path: &std::path::Path) -> HomeGuard {
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_leaky: Vec<(&'static str, Option<String>)> = leaky_keys()
            .iter()
            .map(|k| {
                let k: &'static str = k.as_str();
                let was = std::env::var(k).ok();
                std::env::remove_var(k);
                (k, was)
            })
            .collect();
        // FULL SNAPSHOT, taken AFTER the leaky strip so the strip is what the
        // guard restores to. See the Drop impl for why a key list cannot do
        // this job.
        let snapshot: Vec<(String, String)> = std::env::vars().collect();
        let prev = std::env::var("AMUX_HOME").ok();
        std::env::set_var("AMUX_HOME", path);
        HomeGuard {
            prev,
            prev_leaky,
            snapshot,
            home: path.to_path_buf(),
            _g: g,
        }
    }

    impl Drop for HomeGuard {
        /// Restore the process env EXACTLY, not just the keys we thought could leak.
        ///
        /// AMUX-3719, diagnosed 2026-08-26 after the flake reproduced on a
        /// SECOND test in the module (`owner_alert_respects_channel_config`,
        /// where the first was `owner_alert_full_send_shape_channels_and_ledger`
        /// — which is itself the tell that the defect is the guard, not either
        /// test).
        ///
        /// `leaky_keys()` derives its set from the MACHINE's `~/.amux/server.env`,
        /// and that is the right derivation for the direction it was built for:
        /// stopping the real machine's config from leaking INTO a fixture. It is
        /// blind to the opposite direction. A test writes a key into its OWN temp
        /// home (`set_server_env_key`), `ServerConfig::load` exports that file
        /// into the PROCESS env (config.rs:225 — `std::env::set_var(k, v)`, and
        /// its own comment notes it runs on a timer, not just at boot), and the
        /// guard then cannot restore a key it was never told about. On this
        /// machine `AMUX_OWNER_EMAIL` is absent from `~/.amux/server.env`
        /// (verified: zero matching lines), so `pinned@example.com` survived its
        /// test's guard and every later test that did not set the key in its own
        /// fixture read it from the process env.
        ///
        /// The previous investigation ruled out the writer and stopped: "
        /// `set_server_env_key` writes only the file and never the process env"
        /// is TRUE, and irrelevant, because the export happens later in the
        /// READER. Checking the writer and not the re-exporter is what left this
        /// open for a day.
        ///
        /// A key list cannot fix this, and adding `AMUX_OWNER_EMAIL` to the floor
        /// would fix exactly one test. Any fixture may write any key, so the set
        /// is unbounded and unknowable in advance — the same "someone must
        /// remember to add a row" shape `leaky_keys()` was written to escape.
        /// Snapshot-and-restore is derived from behaviour instead of enumeration,
        /// so a fixture that invents a new key tomorrow is covered today.
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("AMUX_HOME", v),
                None => std::env::remove_var("AMUX_HOME"),
            }
            for (k, was) in &self.prev_leaky {
                match was {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
            // Remove anything that appeared during the guard's window, then
            // restore anything that was changed or deleted. Order matters only
            // in that removals must not undo a restore, so restores go last.
            // SCOPE THE RESTORE TO THE LEAK CHANNEL, not to "every key that
            // changed". The first version of this fix restored the WHOLE env
            // diff and immediately broke
            // `the_budgeted_sample_is_spread_across_directories_not_taken_alphabetically`:
            // that test sets AMUX_NUDGE_REVIVED_MAX_PATHS with a bare set_var,
            // outside any guard, and runs twice under the cap. A concurrent
            // guard whose snapshot predated the set_var deleted the key between
            // the two runs, so the second used the default 40 and disagreed —
            // which is the exact failure that test's own comment already
            // describes as its first draft's bug. Trading one flake for a
            // broader one is not a fix; the blanket restore could clobber any
            // env var any test owns.
            //
            // The channel is exactly one file. config.rs:225 exports the
            // resolved home's `server.env` into the process env, so the keys
            // this guard's window could have exported ARE that file's keys.
            // Reading them at drop (not at set_home) is deliberate: the fixture
            // is usually written AFTER the guard is taken.
            let mut scoped: Vec<String> = crate::config::parse_env_file(&self.home.join("server.env"))
                .into_keys()
                .collect();
            // The marker config.rs writes alongside the export belongs to the
            // same mechanism, so it leaks the same way.
            scoped.push(crate::config::ENV_FROM_FILE_MARKER.to_string());

            let mut leaked: Vec<String> = Vec::new();
            for k in scoped {
                if k == "AMUX_HOME" || self.prev_leaky.iter().any(|(lk, _)| *lk == k) {
                    continue; // already handled above
                }
                let want = self.snapshot.iter().find(|(sk, _)| *sk == k).map(|(_, v)| v.clone());
                let have = std::env::var(&k).ok();
                if have == want {
                    continue;
                }
                leaked.push(k.clone());
                match want {
                    Some(v) => std::env::set_var(&k, v),
                    None => std::env::remove_var(&k),
                }
            }
            // SAY THAT IT HAPPENED. Containing the leak silently would make the
            // next leak path indistinguishable from no leak path at all, and
            // this bug already cost two investigations that ended in "mechanism
            // unknown". The restore above is now total, so a name appearing here
            // is not a failure — it is the only evidence that a fixture home is
            // exporting into the shared process env, which is the thing that was
            // invisible. The first diagnostic for this flake was attached to ONE
            // test's assertion and the flake then reproduced on a DIFFERENT test,
            // printing nothing; an instrument on the mechanism does not care
            // which test trips it.
            if !leaked.is_empty() {
                leaked.sort();
                leaked.dedup();
                tracing::warn!(
                    marker = "fixture_home_env_leak",
                    keys = %leaked.join(","),
                    "a fixture home mutated the shared process env; HomeGuard restored it (AMUX-3719)"
                );
            }
        }
    }

    /// The invariant, stated against the LEAK SOURCE (AMUX-2675).
    ///
    /// Deliberately not written as "poison the process env, then call
    /// set_home": that would have to mutate process-global state OUTSIDE the
    /// lock in order to set up, which is the very race this file is about — the
    /// test would have been a new flake aimed at an old one. This asserts the
    /// coverage relation instead, which is what actually failed: every key the
    /// machine's server.env can export must be cleared by a temp home.
    ///
    /// On this machine it fails against the pre-fix `LEAKY_KEYS` at
    /// `AMUX_URGENT_PUSH`. On CI there is no server.env, the loop body never
    /// runs, and the floor assertions still hold — which is honest, because
    /// with no server.env there is nothing to leak and CI never saw the flake.
    #[test]
    fn a_temp_home_clears_every_key_the_machine_could_leak() {
        let keys = leaky_keys();
        for k in LEAKY_KEYS_FLOOR {
            assert!(
                keys.iter().any(|x| x == k),
                "{k} must always be cleared, even with no server.env present"
            );
        }
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let file = std::path::Path::new(&home).join(".amux").join("server.env");
        let Ok(text) = std::fs::read_to_string(&file) else {
            return; // no server.env (CI): nothing can leak
        };
        let mut checked = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, _)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            if k.is_empty() {
                continue;
            }
            checked += 1;
            // NAME only — never the value; that file holds credentials.
            assert!(
                keys.iter().any(|x| x == k),
                "{k} is in the machine's server.env, so ServerConfig::load exports it into the \
                 process env and effective_env falls back to it inside a TEMP home — but set_home \
                 does not clear it. That is the AMUX-2675 flake, one key at a time."
            );
        }
        // The loop must have had something to check, or this passes vacuously
        // — an empty-match filter and a correct one look identical from a green
        // result alone (ethos rule 7).
        assert!(
            checked > 0,
            "server.env exists at {} but parsed 0 keys — the parser, not the coverage, is wrong",
            file.display()
        );
    }

    /// AMUX-3719: THE OTHER DIRECTION — a fixture home must not leak OUT.
    ///
    /// The test above covers machine -> fixture. This covers fixture -> process,
    /// which is the one that actually bit, twice, on two different tests in
    /// `api::alerts`. A test writes a key into its own temp `server.env`,
    /// `ServerConfig::load` exports the file into the PROCESS env, and the key
    /// outlives the guard because `leaky_keys()` was derived from the machine's
    /// file and has never heard of it.
    ///
    /// THE PROBE KEY IS DELIBERATELY UNIQUE TO THIS TEST. Using the real
    /// specimen (`AMUX_OWNER_EMAIL`) would read a process-global key that
    /// another test legitimately sets, so the assertion after the guard drops
    /// would race the very tests this is about — a new flake aimed at an old
    /// one, which is the trap the sibling test's comment names. A key nobody
    /// else touches tests the identical property with no interference.
    ///
    /// Set-up mutates the process env only THROUGH the shipped export path
    /// while the guard is held, so nothing global happens outside the lock.
    #[test]
    fn a_fixture_home_cannot_leak_a_key_past_its_guard() {
        const PROBE: &str = "AMUX_TEST_LEAK_PROBE_3719";
        assert!(
            std::env::var(PROBE).is_err(),
            "{PROBE} is meant to be unique to this test; something else set it"
        );

        let dir = tempfile::tempdir().unwrap();
        {
            let _guard = set_home(dir.path());
            crate::api::settings::set_server_env_key(dir.path(), PROBE, "from-a-fixture").unwrap();
            // The SHIPPED export path, not a paraphrase of it: this is the
            // function that puts server.env into the process env, and pinning a
            // hand-rolled set_var here would test something the product does not
            // do (ethos rule 7 — the fixture must flow through the code where
            // the defect is introduced).
            let _ = crate::config::ServerConfig::load(
                dir.path().to_path_buf(),
                &std::collections::BTreeMap::new(),
            );
            assert_eq!(
                std::env::var(PROBE).ok().as_deref(),
                Some("from-a-fixture"),
                "set-up failed: ServerConfig::load did not export the fixture key, so the \
                 assertion below would pass without the leak ever existing"
            );
        }

        assert!(
            std::env::var(PROBE).is_err(),
            "{PROBE} survived its HomeGuard. A fixture home exported it into the process env \
             and the guard restored only leaky_keys(), which is derived from the MACHINE's \
             server.env and cannot know about it. Every later test that does not set this key \
             in its own fixture now reads it — that is AMUX-3719."
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;


    /// AMUX-2904. Clearing an API key must actually clear it. `effective_env`
    /// fell through to the PROCESS env whenever the file value was empty, and
    /// config.rs exports server.env into the process env at startup — so a key
    /// that survived one restart could never be removed. The write succeeded,
    /// the read lied, and nothing anywhere reported a failure.
    #[test]
    fn an_emptied_key_reads_as_cleared_even_when_the_process_env_still_has_it() {
        let _lock = test_env::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tmp");
        let home = dir.path();
        let key = "AMUX_TEST_EFFECTIVE_ENV_KEY";

        // The process env holds a value — exactly what config.rs's startup
        // setdefault produces for any key ever saved to server.env.
        std::env::set_var(key, "from-process-env");

        // 1. ABSENT from the file -> the process env is the answer.
        std::fs::write(home.join("server.env"), "OTHER=1\n").expect("write");
        assert_eq!(effective_env(home, key).as_deref(), Some("from-process-env"));

        // 2. PRESENT and non-empty -> the file wins.
        std::fs::write(home.join("server.env"), format!("{key}=from-file\n")).expect("write");
        assert_eq!(effective_env(home, key).as_deref(), Some("from-file"));

        // 3. PRESENT but EMPTY -> CLEARED. This is the assertion that fails on
        //    the pre-fix code, which returned Some("from-process-env").
        std::fs::write(home.join("server.env"), format!("{key}=\n")).expect("write");
        assert_eq!(
            effective_env(home, key),
            None,
            "an explicitly emptied key must read as cleared, not fall back to the process env"
        );

        std::env::remove_var(key);
    }
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("settings-test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let router = Router::new().nest("/api/settings", routes()).with_state(state);
        (router, dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let req = match body {
            Some(v) => b
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    #[test]
    fn shlex_split_matches_python_shapes() {
        assert_eq!(shlex_split("--model opus --max-tokens 8000").unwrap(),
                   vec!["--model", "opus", "--max-tokens", "8000"]);
        assert_eq!(shlex_split(r#"--append-system-prompt "be very terse""#).unwrap(),
                   vec!["--append-system-prompt", "be very terse"]);
        assert_eq!(shlex_split("--x 'a b'").unwrap(), vec!["--x", "a b"]);
        assert_eq!(shlex_split("").unwrap(), Vec::<String>::new());
        assert_eq!(shlex_split(r#"a\ b"#).unwrap(), vec!["a b"]);
        // Unbalanced quote errors with Python's message.
        assert_eq!(shlex_split(r#"--x "unclosed"#).unwrap_err(), "No closing quotation");
    }

    #[test]
    fn shlex_quote_matches_python() {
        assert_eq!(shlex_quote("opus"), "opus");
        assert_eq!(shlex_quote("--max-tokens"), "--max-tokens");
        assert_eq!(shlex_quote("a b"), "'a b'");
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(shlex_quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn model_flag_surgery_preserves_other_flags() {
        assert_eq!(strip_model_from_flags("--model opus --max-tokens 8000").unwrap(),
                   "--max-tokens 8000");
        assert_eq!(strip_model_from_flags("--model=opus --effort high").unwrap(),
                   "--effort high");
        assert_eq!(strip_model_from_flags("").unwrap(), "");
        // Quoted multi-word values survive re-quoting.
        assert_eq!(
            strip_model_from_flags(r#"--model opus --append-system-prompt "be terse""#).unwrap(),
            "--append-system-prompt 'be terse'"
        );
        assert!(strip_model_from_flags(r#"--model "unclosed"#).is_err());

        assert_eq!(extract_model_from_flags("--model opus --x y"), "opus");
        assert_eq!(extract_model_from_flags("--model=claude-fable-5"), "claude-fable-5");
        assert_eq!(extract_model_from_flags("--x y"), "");
        assert_eq!(extract_model_from_flags(r#"--model "unclosed"#), "");
    }

    #[test]
    fn model_name_validation_matches_python() {
        assert_eq!(validate_model_name(&json!("  opus  ")).unwrap(), "opus");
        assert_eq!(validate_model_name(&json!("")).unwrap(), "");
        assert_eq!(validate_model_name(&json!("us.anthropic.claude-3[1m]@x/+y")).unwrap(),
                   "us.anthropic.claude-3[1m]@x/+y");
        assert!(validate_model_name(&json!(3)).is_err());
        assert!(validate_model_name(&json!("-leading-hyphen")).is_err());
        assert!(validate_model_name(&json!("has space")).is_err());
        assert!(validate_model_name(&json!("x".repeat(256))).is_err());
    }

    #[test]
    fn mask_matches_python() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("short"), "set");
        assert_eq!(mask_secret("12345678"), "set");
        assert_eq!(mask_secret("sk-ant-api03-abcd"), "*************abcd");
    }

    #[test]
    fn default_model_file_helpers_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Empty home: sonnet fallback.
        assert_eq!(get_default_model(home), "sonnet");
        // Patch a model in.
        patch_default_model_file(home, "opus").unwrap();
        assert_eq!(get_default_model(home), "opus");
        assert_eq!(
            std::fs::read_to_string(home.join("defaults.env")).unwrap(),
            "CC_DEFAULT_FLAGS=\"--model opus\"\n"
        );
        // Other flags and other lines survive a model change.
        std::fs::write(
            home.join("defaults.env"),
            "OTHER=1\nCC_DEFAULT_FLAGS=\"--model sonnet --max-tokens 8000\"\n",
        )
        .unwrap();
        patch_default_model_file(home, "opus").unwrap();
        let content = std::fs::read_to_string(home.join("defaults.env")).unwrap();
        assert_eq!(content, "OTHER=1\nCC_DEFAULT_FLAGS=\"--model opus --max-tokens 8000\"\n");
        // Clearing the model keeps the rest.
        patch_default_model_file(home, "").unwrap();
        let content = std::fs::read_to_string(home.join("defaults.env")).unwrap();
        assert_eq!(content, "OTHER=1\nCC_DEFAULT_FLAGS=\"--max-tokens 8000\"\n");
        // 0600 like Python's _atomic_write_secure.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.join("defaults.env")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Malformed existing flags: loud 400, file untouched.
        std::fs::write(home.join("defaults.env"), "CC_DEFAULT_FLAGS=\"--model 'unclosed\"\n").unwrap();
        let e = patch_default_model_file(home, "opus").unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        assert!(e.1["error"].as_str().unwrap().contains("malformed"), "{:?}", e.1);
        assert!(e.1["error"].as_str().unwrap().contains("fix the file manually"));
    }

    #[test]
    fn guard_helpers_defaults_and_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // Defaults: commit ON, task OFF.
        assert!(commit_guard_enabled(home));
        assert!(!task_guard_enabled(home));
        // Explicit falsy spellings disable commit-guard.
        set_server_env_key(home, "AMUX_COMMIT_GUARD", "off").unwrap();
        assert!(!commit_guard_enabled(home));
        // Junk is NOT a falsy spelling — commit-guard stays on.
        set_server_env_key(home, "AMUX_COMMIT_GUARD", "banana").unwrap();
        assert!(commit_guard_enabled(home));
        // Task-guard needs an explicit truthy spelling.
        set_server_env_key(home, "AMUX_TASK_GUARD", "banana").unwrap();
        assert!(!task_guard_enabled(home));
        set_server_env_key(home, "AMUX_TASK_GUARD", "yes").unwrap();
        assert!(task_guard_enabled(home));
        // Line-replace, not append-forever.
        let content = std::fs::read_to_string(home.join("server.env")).unwrap();
        assert_eq!(content.matches("AMUX_TASK_GUARD").count(), 1, "{content}");
    }

    #[tokio::test]
    async fn settings_endpoints_end_to_end_in_temp_home() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let (app, _dbdir) = app();

        // default-model GET (fallback) / PATCH / GET.
        let (st, v) = send(&app, "GET", "/api/settings/default-model", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["model"], json!("sonnet"));
        let (st, v) =
            send(&app, "PATCH", "/api/settings/default-model", Some(json!({ "model": "opus" }))).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v, json!({ "ok": true, "model": "opus" }));
        let (_, v) = send(&app, "GET", "/api/settings/default-model", None).await;
        assert_eq!(v["model"], json!("opus"));
        // Bad payloads.
        let (st, v) = send(&app, "PATCH", "/api/settings/default-model", Some(json!(["x"]))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], json!("payload must be a JSON object"));
        let (st, _) =
            send(&app, "PATCH", "/api/settings/default-model", Some(json!({ "model": "-x" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);

        // Guards: GET defaults, PATCH, GET reflects the file immediately.
        let (_, v) = send(&app, "GET", "/api/settings/commit-guard", None).await;
        assert_eq!(v, json!({ "enabled": true }));
        let (_, v) =
            send(&app, "PATCH", "/api/settings/commit-guard", Some(json!({ "enabled": false }))).await;
        assert_eq!(v, json!({ "ok": true, "enabled": false }));
        let (_, v) = send(&app, "GET", "/api/settings/commit-guard", None).await;
        assert_eq!(v, json!({ "enabled": false }));
        // Python default when the key is absent from the body: commit=true, task=false.
        let (_, v) = send(&app, "PATCH", "/api/settings/commit-guard", Some(json!({}))).await;
        assert_eq!(v["enabled"], json!(true));
        let (_, v) = send(&app, "PATCH", "/api/settings/task-guard", Some(json!({}))).await;
        assert_eq!(v["enabled"], json!(false));
        let (_, v) =
            send(&app, "PATCH", "/api/settings/task-guard", Some(json!({ "enabled": true }))).await;
        assert_eq!(v, json!({ "ok": true, "enabled": true }));
        let (_, v) = send(&app, "GET", "/api/settings/task-guard", None).await;
        assert_eq!(v, json!({ "enabled": true }));

        // env: masked GET, allow-listed PATCH.
        //
        // This endpoint reports the PROCESS environment, not just the temp
        // home's server.env — deliberately, since that is what a worker would
        // actually receive. So a bare `assert_eq!(.., "")` is not a statement
        // about the code, it is a statement about whoever ran the test: it
        // passed in CI (no OPENAI_API_KEY) and failed on a dev machine that had
        // one exported, which reads as "main is red" when nothing is broken.
        //
        // Assert the real contract instead — unset reads empty, set reads
        // MASKED and never leaks the value — and branch on the ambient env
        // rather than mutating it, because `set_var` is process-global and this
        // suite runs threaded (the alerts tests already race on exactly that).
        let (_, v) = send(&app, "GET", "/api/settings/env", None).await;
        match std::env::var("OPENAI_API_KEY") {
            Err(_) => assert_eq!(v["OPENAI_API_KEY"], json!("")),
            Ok(real) => {
                let shown = v["OPENAI_API_KEY"].as_str().unwrap_or_default();
                assert!(
                    shown.starts_with('*'),
                    "a configured key must come back masked, got {shown:?}"
                );
                assert!(
                    !shown.contains(&real),
                    "the masked form must never contain the real value"
                );
            }
        }
        let (st, v) = send(
            &app,
            "PATCH",
            "/api/settings/env",
            Some(json!({ "ANTHROPIC_API_KEY": "sk-ant-api03-abcd", "NOT_ALLOWED": "x" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v, json!({ "ok": true }));
        let (_, v) = send(&app, "GET", "/api/settings/env", None).await;
        assert_eq!(v["ANTHROPIC_API_KEY"], json!("*************abcd"));
        // The disallowed key never reached the file.
        let content = std::fs::read_to_string(dir.path().join("server.env")).unwrap();
        assert!(content.contains("ANTHROPIC_API_KEY=sk-ant-api03-abcd"));
        assert!(!content.contains("NOT_ALLOWED"));
        // Only disallowed / non-string keys: Python's 400.
        let (st, v) =
            send(&app, "PATCH", "/api/settings/env", Some(json!({ "NOT_ALLOWED": "x" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], json!("no valid keys"));
        let (st, _) =
            send(&app, "PATCH", "/api/settings/env", Some(json!({ "OPENAI_API_KEY": 42 }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    /// AMUX-3386: a config value pointing into ephemeral ~/.amux/uploads/ must be
    /// refused at the persist site — that path is the AMUX-3383 time bomb (the
    /// file is cleaned up and the config silently breaks later).
    #[test]
    fn set_server_env_refuses_an_ephemeral_uploads_path() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let uploads = home.join("uploads").join("db76-key.json");
        let stable = home.join("gcp").join("dpa-sa.json");

        // EVERY age-reaped dir is ephemeral, not just uploads — the guard reads
        // storage::AGE_PRUNED_DIRS, so media-cache and spin-dumps (which the
        // original guard silently passed) are caught too.
        assert!(is_ephemeral_path(home, uploads.to_str().unwrap()), "uploads path is ephemeral");
        assert!(is_ephemeral_path(home, home.join("media-cache").join("x").to_str().unwrap()), "media-cache");
        assert!(is_ephemeral_path(home, home.join("spin-dumps").join("x").to_str().unwrap()), "spin-dumps");
        // Negative control — a path-prefix check must not widen into refusing
        // everything: a stable dir and an opaque non-path value are NOT flagged.
        assert!(!is_ephemeral_path(home, stable.to_str().unwrap()), "gcp path is stable");
        assert!(!is_ephemeral_path(home, "some-opaque-token-value"), "a non-path value is not ephemeral");

        // The persist site refuses the ephemeral path and writes nothing.
        assert!(
            set_server_env_key(home, "GOOGLE_SA_KEY_FILE", uploads.to_str().unwrap()).is_err(),
            "must refuse an uploads path"
        );
        let after = std::fs::read_to_string(home.join("server.env")).unwrap_or_default();
        assert!(!after.contains("uploads"), "the ephemeral path must not reach server.env: {after}");

        // A stable path persists normally.
        set_server_env_key(home, "GOOGLE_SA_KEY_FILE", stable.to_str().unwrap()).unwrap();
        let after = std::fs::read_to_string(home.join("server.env")).unwrap();
        assert!(after.contains("gcp/dpa-sa.json"), "stable path persists: {after}");
    }
}
