//! Vault secrets: encrypted key/value items scoped to the whole fleet, a group
//! or one worker, delivered into a worker's environment at launch (AMUX-5375,
//! Ethan 2026-09-30: "an encrypted, local, editable secrets vault scoped per
//! key/value at global, group or worker level").
//!
//! # Where it fits
//!
//! The same vault as the payment cards in `vault.rs`: same `/api/vault` family,
//! same `~/.amux/vault/` directory, same `logs/vault-audit.jsonl`. Scoping is
//! the scope env files' model (`scope_env_layers`): global, then each group the
//! worker is in (sorted, later wins), then the worker. A worker gets one value
//! per key, the most specific one. The difference from those files is storage:
//! they are plaintext, these are not.
//!
//! # Encryption
//!
//! Each value is sealed with AES-256-GCM under a random 96-bit nonce. The
//! associated data binds the ciphertext to its item id, key and scope, so a
//! ciphertext copied onto another item (say, from a worker scope to global)
//! fails to open instead of delivering a value to the wrong place.
//!
//! The 32-byte master key never touches the vault directory on macOS: it lives
//! in the login Keychain (service `amux-vault-master`, one account per
//! AMUX_HOME so a throwaway test home can never read or replace the real key).
//! It is written through `security -i` on stdin, so the key is never an argv
//! element. Elsewhere, or with `AMUX_VAULT_KEYSTORE=file`, it is a 0600 file
//! beside the store, which is the same trust level as `server.env`, and every
//! load of that backend says so in the log.
//!
//! What this protects against: a copied or backed-up `~/.amux` (the restic
//! backup ships it to B2), a stray `cat`, a grep over the home directory, a
//! worker reading the store file. What it does not: a process running as this
//! user that asks the Keychain, which is also what the server itself does.
//!
//! # Delivery
//!
//! `deliver` resolves a worker's items and the start path hands them to
//! its existing `deferred_secrets` route (AMUX-4803): `tmux set-environment`
//! after the session exists, imported by a typed command that carries no
//! value. So a value never appears in the tmux server's argv, the pane's
//! command line, the board, a log line or history. The audit line names keys
//! and scopes, never values.
//!
//! # Who can write, and the one read
//!
//! Only the owner: no worker origin, not an invited member, and on a host
//! with an auth token the owner bearer itself (the dashboard sends it). A
//! worker origin is refused on every write and sees only the metadata of
//! items that apply to it. Exactly one endpoint returns a value: `reveal`,
//! owner only, one item per call, audited, `Cache-Control: no-store`, and
//! the dashboard masks it again after `REVEAL_REMASK_S`.
//!
//! # Rotation and revocation
//!
//! A process keeps the environment it started with, so a rotated or deleted
//! value reaches a RUNNING terminal worker at its next start or restart, never
//! mid-turn. `delivered.json` (ids and versions, never values) records what
//! each launch handed over, which is how the API reports "running, holds the
//! previous version, restart to apply" instead of claiming a change is live.
//! One-shot paths (Chat turns, delegates, shell schedules) resolve at every
//! run, so they always get the current value.

use crate::config::{amux_home, now_f64};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::extract::{Path as AxPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const STORE_VERSION: u64 = 1;
const KEYCHAIN_SERVICE: &str = "amux-vault-master";
const MAX_VALUE_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Global,
    Group(String),
    Worker(String),
}

impl Scope {
    fn kind(&self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Group(_) => "group",
            Scope::Worker(_) => "worker",
        }
    }
    fn target(&self) -> &str {
        match self {
            Scope::Global => "",
            Scope::Group(g) | Scope::Worker(g) => g,
        }
    }
    fn parse(kind: &str, target: &str) -> Result<Scope, String> {
        let target = target.trim();
        let name_ok = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        match kind.trim() {
            "global" => Ok(Scope::Global),
            // Group names are compared lowercased, as scope_env_layers does.
            "group" if name_ok(target) => Ok(Scope::Group(target.to_lowercase())),
            "worker" if name_ok(target) => Ok(Scope::Worker(target.to_string())),
            "group" | "worker" => Err(format!("scope {kind:?} needs a valid target name")),
            other => Err(format!("scope must be global, group or worker, not {other:?}")),
        }
    }
    /// Order within a worker's resolution: higher wins.
    fn rank(&self) -> u8 {
        match self {
            Scope::Global => 0,
            Scope::Group(_) => 1,
            Scope::Worker(_) => 2,
        }
    }
}

/// A key a worker's environment can carry. Harness variables are refused: a
/// vault value named CC_DIR or AMUX_URL would silently re-route the worker.
pub(crate) fn valid_key(k: &str) -> Result<(), String> {
    let ok = !k.is_empty()
        && k.len() <= 128
        && k.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
        && k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        return Err("key must be an environment variable name: A-Z, 0-9 and _, not starting with a digit".into());
    }
    if k.starts_with("CC_") || k.starts_with("AMUX_") || ["PATH", "HOME", "SHELL", "USER", "TMPDIR"].contains(&k) {
        return Err(format!("{k} is a harness or shell variable; a vault value there would change how the worker runs, not what it can reach"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Master key
// ---------------------------------------------------------------------------

fn keystore_is_file() -> bool {
    match std::env::var("AMUX_VAULT_KEYSTORE").ok().as_deref().map(str::trim) {
        Some("file") => true,
        Some("keychain") => false,
        _ => !cfg!(target_os = "macos"),
    }
}

/// One Keychain account per home, so a test home never shares the real key.
fn keychain_account(home: &Path) -> String {
    let canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let h = Sha256::digest(canon.to_string_lossy().as_bytes());
    format!("home-{}", hex::encode(&h[..8]))
}

fn key_file(home: &Path) -> PathBuf {
    super::vault::vault_dir(home).join("master.key")
}

/// Short fingerprint of a key, stored on each item so a replaced Keychain key
/// is reported as "the key changed", not as a generic decryption failure.
fn fingerprint(key: &[u8; 32]) -> String {
    hex::encode(&Sha256::digest(key)[..4])
}

fn parse_key_hex(s: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(s.trim()).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

fn read_keychain(home: &Path) -> Result<Option<[u8; 32]>, String> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", &keychain_account(home), "-w"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run security: {e}"))?;
    if out.status.code() == Some(44) {
        return Ok(None); // errSecItemNotFound
    }
    if !out.status.success() {
        return Err(format!(
            "security find-generic-password exited {:?}: {} (is the login keychain locked?)",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_key_hex(&String::from_utf8_lossy(&out.stdout))
        .map(Some)
        .ok_or_else(|| "the Keychain item is not a 32-byte hex key".to_string())
}

fn write_keychain(home: &Path, key: &[u8; 32]) -> Result<(), String> {
    use std::io::Write;
    // `security -i` reads commands from stdin: the key never becomes an argv
    // element of any process.
    let mut child = std::process::Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run security: {e}"))?;
    let line = format!(
        "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {} -l \"amux vault master key\" -w {}\n",
        keychain_account(home),
        hex::encode(key)
    );
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("security add-generic-password failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

fn read_key_file(home: &Path) -> Result<Option<[u8; 32]>, String> {
    match std::fs::read_to_string(key_file(home)) {
        Ok(s) => parse_key_hex(&s).map(Some).ok_or_else(|| "master.key is not a 32-byte hex key".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("could not read master.key: {e}")),
    }
}

fn write_key_file(home: &Path, key: &[u8; 32]) -> Result<(), String> {
    let dir = super::vault::vault_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".master.key.{}", std::process::id()));
    std::fs::write(&tmp, hex::encode(key)).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, key_file(home)).map_err(|e| e.to_string())
}

fn key_cache() -> &'static std::sync::Mutex<BTreeMap<PathBuf, [u8; 32]>> {
    static C: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, [u8; 32]>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// The master key for this home. `create` mints one when none exists; only a
/// write asks for that, so reading an empty vault never touches the Keychain.
pub(crate) fn master_key(home: &Path, create: bool) -> Result<Option<[u8; 32]>, String> {
    if let Some(k) = key_cache().lock().ok().and_then(|c| c.get(home).copied()) {
        return Ok(Some(k));
    }
    let file = keystore_is_file();
    let found = if file { read_key_file(home)? } else { read_keychain(home)? };
    let key = match found {
        Some(k) => k,
        None if create => {
            let mut k = [0u8; 32];
            OsRng.fill_bytes(&mut k);
            if file { write_key_file(home, &k)? } else { write_keychain(home, &k)? }
            // Read back: a write the store silently dropped must not become
            // a vault whose values nothing can open.
            let back = if file { read_key_file(home)? } else { read_keychain(home)? };
            if back != Some(k) {
                return Err("the new master key did not read back from the keystore".into());
            }
            tracing::info!(backend = if file { "file" } else { "keychain" }, measured = true,
                n_considered = 1, verdict = "vault_key_created", "vault: master key created (AMUX-5375)");
            k
        }
        None => return Ok(None),
    };
    if file {
        tracing::warn!(path = %key_file(home).display(), measured = true, n_considered = 1,
            verdict = "vault_key_file_backend",
            "vault: master key is a 0600 file beside the store (no Keychain); a copy of ~/.amux carries both (AMUX-5375)");
    }
    if let Ok(mut c) = key_cache().lock() {
        c.insert(home.to_path_buf(), key);
    }
    Ok(Some(key))
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

fn store_path(home: &Path) -> PathBuf {
    super::vault::vault_dir(home).join("secrets.json")
}

fn load(home: &Path) -> Result<Vec<Value>, String> {
    match std::fs::read_to_string(store_path(home)) {
        Ok(s) => {
            let v: Value = serde_json::from_str(&s).map_err(|e| format!("secrets.json is not valid JSON: {e}"))?;
            Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("could not read secrets.json: {e}")),
    }
}

fn save(home: &Path, items: &[Value]) -> Result<(), String> {
    let dir = super::vault::vault_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".secrets.json.{}", std::process::id()));
    let body = serde_json::to_vec_pretty(&json!({"version": STORE_VERSION, "items": items})).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, store_path(home)).map_err(|e| e.to_string())
}

fn item_scope(item: &Value) -> Option<Scope> {
    Scope::parse(item["scope"].as_str()?, item["target"].as_str().unwrap_or("")).ok()
}

fn aad(id: &str, key: &str, scope: &Scope) -> String {
    format!("amux-vault-v1|{id}|{key}|{}|{}", scope.kind(), scope.target())
}

fn seal(master: &[u8; 32], id: &str, key: &str, scope: &Scope, value: &str) -> Result<(String, String), String> {
    let cipher = Aes256Gcm::new_from_slice(master).map_err(|e| e.to_string())?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let a = aad(id, key, scope);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: value.as_bytes(), aad: a.as_bytes() })
        .map_err(|_| "encryption failed".to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok((b64.encode(nonce), b64.encode(ct)))
}

fn open(master: &[u8; 32], item: &Value) -> Result<String, String> {
    let scope = item_scope(item).ok_or("item has no valid scope")?;
    let id = item["id"].as_str().unwrap_or("");
    let key = item["key"].as_str().unwrap_or("");
    if item["key_fp"].as_str().is_some_and(|fp| fp != fingerprint(master)) {
        return Err("sealed under a different master key (the Keychain key was replaced)".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let nonce = b64.decode(item["nonce"].as_str().unwrap_or("")).map_err(|_| "bad nonce")?;
    let ct = b64.decode(item["ct"].as_str().unwrap_or("")).map_err(|_| "bad ciphertext")?;
    if nonce.len() != 12 {
        return Err("bad nonce length".into());
    }
    let cipher = Aes256Gcm::new_from_slice(master).map_err(|e| e.to_string())?;
    let a = aad(id, key, &scope);
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: a.as_bytes() })
        .map_err(|_| "ciphertext does not open for this item (tampered, or moved between items)".to_string())?;
    String::from_utf8(pt).map_err(|_| "value is not UTF-8".into())
}

fn audit(home: &Path, entry: Value) {
    super::vault::audit_line(home, entry);
}

/// Items written before rotation was tracked have no version: they are 1.
fn item_version(item: &Value) -> u64 {
    item["version"].as_u64().unwrap_or(1)
}

/// Metadata only. There is deliberately no field that could carry a value.
fn public_view(item: &Value) -> Value {
    json!({
        "id": item["id"], "key": item["key"], "scope": item["scope"], "target": item["target"],
        "created": item["created"], "updated": item["updated"],
        "rotated": item.get("rotated").cloned().unwrap_or(Value::Null),
        "version": item_version(item), "by": item["by"], "note": item["note"], "source": item["source"],
    })
}

/// One writer at a time for the store and the delivery ledger. The temp file
/// name is per process, so two writes from this process must not interleave.
fn store_lock() -> &'static std::sync::Mutex<()> {
    static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(Default::default)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

fn worker_groups(home: &Path, worker: &str) -> std::collections::BTreeSet<String> {
    super::session_verbs::EnvFile::load(&home.join("sessions").join(format!("{worker}.env")))
        .get("CC_TAGS")
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().trim_matches('"').to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Does this item apply to `worker`?
fn applies(scope: &Scope, worker: &str, groups: &std::collections::BTreeSet<String>) -> bool {
    match scope {
        Scope::Global => true,
        Scope::Group(g) => groups.contains(g),
        Scope::Worker(w) => w == worker,
    }
}

/// Per key, the winning item for a worker plus the ones it shadows. Same
/// precedence as `scope_env_layers`: worker > group > global, and among the
/// worker's groups the one that sorts LAST wins, because that is the file
/// sourced last.
pub(crate) fn resolve<'a>(items: &'a [Value], home: &Path, worker: &str) -> BTreeMap<String, (&'a Value, Vec<&'a Value>)> {
    let groups = worker_groups(home, worker);
    resolve_with(items, worker, &groups)
}

fn resolve_with<'a>(
    items: &'a [Value],
    worker: &str,
    groups: &std::collections::BTreeSet<String>,
) -> BTreeMap<String, (&'a Value, Vec<&'a Value>)> {
    let mut by_key: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for it in items {
        let Some(scope) = item_scope(it) else { continue };
        if applies(&scope, worker, groups) {
            by_key.entry(it["key"].as_str().unwrap_or("").to_string()).or_default().push(it);
        }
    }
    by_key
        .into_iter()
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, mut v)| {
            v.sort_by(|a, b| {
                let (sa, sb) = (item_scope(a).unwrap(), item_scope(b).unwrap());
                sa.rank().cmp(&sb.rank()).then_with(|| sa.target().cmp(sb.target()))
            });
            let win = v.pop().expect("non-empty");
            (k, (win, v))
        })
        .collect()
}

/// Every worker this home knows, from `sessions/*.env`. A Chat tab
/// (`CC_COMPANION_OF`) is not listed: it runs with its worker's secrets.
fn known_workers(home: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(home.join("sessions"))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".env")).map(str::to_string))
                .filter(|n| super::session_verbs::valid_session_name(n))
                .filter(|n| {
                    super::session_verbs::EnvFile::load(&home.join("sessions").join(format!("{n}.env")))
                        .get("CC_COMPANION_OF")
                        .is_none_or(|v| v.trim().trim_matches('"').is_empty())
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn worker_isolated(home: &Path, worker: &str) -> bool {
    let env = super::session_verbs::EnvFile::load(&home.join("sessions").join(format!("{worker}.env")));
    super::session_verbs::env_flag_on(env.get("CC_ISOLATED").map(|v| v.trim_matches('"')))
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// Every execution path that hands a worker its secrets. Named in the audit
/// line so "did the shell schedule get it" is answerable from the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryPath {
    /// A terminal worker's start or restart (tmux).
    Launch,
    /// A Chat tab's headless turn.
    ChatTurn,
    /// A read-only delegate job asked of a worker.
    Delegate,
    /// A `kind: shell` schedule owned by a worker.
    ShellSchedule,
}

impl DeliveryPath {
    fn as_str(self) -> &'static str {
        match self {
            DeliveryPath::Launch => "launch",
            DeliveryPath::ChatTurn => "chat_turn",
            DeliveryPath::Delegate => "delegate",
            DeliveryPath::ShellSchedule => "shell_schedule",
        }
    }
}

/// What a worker receives: the values, and a manifest of item ids and
/// versions that carries no value.
pub(crate) struct Delivery {
    pub values: Vec<(String, String)>,
    pub manifest: BTreeMap<String, (String, u64)>,
}

/// Resolve and open a worker's secrets for one execution path, and audit the
/// delivery by key, scope, item and version. `Err` means the store could not
/// be read or has items and no key; the caller logs it and runs without them.
pub(crate) fn deliver(home: &Path, worker: &str, path: DeliveryPath) -> Result<Delivery, String> {
    let mut out = Delivery { values: Vec::new(), manifest: BTreeMap::new() };
    let items = load(home)?;
    if items.is_empty() {
        return Ok(out);
    }
    let resolved = resolve(&items, home, worker);
    if resolved.is_empty() {
        return Ok(out);
    }
    let master = master_key(home, false)?.ok_or("the vault has items but no master key (Keychain item missing)")?;
    let mut delivered = Vec::new();
    let mut failed = Vec::new();
    for (key, (win, shadowed)) in &resolved {
        match open(&master, win) {
            Ok(v) => {
                out.values.push((key.clone(), v));
                out.manifest.insert(key.clone(), (win["id"].as_str().unwrap_or("").to_string(), item_version(win)));
                delivered.push(json!({"key": key, "item": win["id"], "version": item_version(win),
                    "scope": win["scope"], "target": win["target"],
                    "shadowed": shadowed.iter().map(|s| json!({"scope": s["scope"], "target": s["target"]})).collect::<Vec<_>>()}));
            }
            Err(e) => {
                tracing::warn!(session = worker, key = %key, item = %win["id"], error = %e, measured = true,
                    n_considered = 1, verdict = "vault_decrypt_failed",
                    "vault: an item did not open, so the worker runs without it (AMUX-5375)");
                failed.push(json!({"key": key, "item": win["id"], "error": e}));
            }
        }
    }
    audit(home, json!({"ts": now_f64(), "event": "secrets_delivered", "session": worker, "path": path.as_str(),
        "delivered": delivered, "failed": failed}));
    tracing::info!(session = worker, path = path.as_str(), delivered = out.values.len(), failed = failed.len(),
        measured = true, n_considered = resolved.len(), verdict = "vault_delivered",
        "vault: secrets resolved for a worker (keys only in vault-audit.jsonl) (AMUX-5375)");
    Ok(out)
}

/// Prefix for a value staged in a one-shot process's environment. `valid_key`
/// refuses `AMUX_` keys, so a staged name can never collide with a real one.
pub(crate) const STAGE_PREFIX: &str = "AMUX_VAULT_STAGE_";

/// For a one-shot process whose shell sources the scope env files before its
/// command runs (Chat turns, delegates): stage each value under a private
/// name in the process environment, and return the shell line that moves it
/// to its real name AFTER the sourcing. The vault then wins over a plaintext
/// line for the same key exactly as it does at launch, and the line names
/// keys only, never values.
pub(crate) fn staged_env(values: &[(String, String)]) -> (Vec<(String, String)>, String) {
    let env = values.iter().map(|(k, v)| (format!("{STAGE_PREFIX}{k}"), v.clone())).collect();
    let line = values
        .iter()
        .map(|(k, _)| format!("export {k}=\"${STAGE_PREFIX}{k}\"; unset {STAGE_PREFIX}{k}; "))
        .collect();
    (env, line)
}

/// Values shorter than this are not redacted from output: a 3-character
/// value would replace ordinary words.
const REDACT_MIN_LEN: usize = 4;

/// Replace each delivered value in `buf` with `[vault:KEY]`. Used on output
/// the server itself stores (shell-schedule run logs and notes), so a command
/// that echoes a credential does not write it into amux's records.
pub(crate) fn redact_bytes(buf: &[u8], values: &[(String, String)]) -> Vec<u8> {
    let mut out = buf.to_vec();
    for (k, v) in values {
        let needle = v.as_bytes();
        if needle.len() < REDACT_MIN_LEN {
            continue;
        }
        let mask = format!("[vault:{k}]").into_bytes();
        let mut next = Vec::with_capacity(out.len());
        let mut i = 0;
        while i < out.len() {
            if out[i..].starts_with(needle) {
                next.extend_from_slice(&mask);
                i += needle.len();
            } else {
                next.push(out[i]);
                i += 1;
            }
        }
        out = next;
    }
    out
}

pub(crate) fn redact(text: &str, values: &[(String, String)]) -> String {
    if values.is_empty() {
        return text.to_string();
    }
    String::from_utf8_lossy(&redact_bytes(text.as_bytes(), values)).into_owned()
}

fn ledger_path(home: &Path) -> PathBuf {
    super::vault::vault_dir(home).join("delivered.json")
}

/// What each terminal worker's CURRENT process was launched with: item id and
/// version per key, never a value. A running process keeps the environment
/// it started with, so this is what tells "rotated, restart to apply" apart
/// from "current".
fn load_ledger(home: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(ledger_path(home))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_private(path: &Path, body: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(
        ".{}.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp"),
        std::process::id()
    ));
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Record what a terminal launch handed to the worker's process.
pub(crate) fn record_running(home: &Path, worker: &str, manifest: &BTreeMap<String, (String, u64)>) {
    let _g = store_lock().lock().unwrap_or_else(|p| p.into_inner());
    let mut ledger = load_ledger(home);
    if manifest.is_empty() && !ledger.contains_key(worker) && load(home).map(|i| i.is_empty()).unwrap_or(true) {
        return; // an empty vault never creates the file
    }
    let keys: serde_json::Map<String, Value> = manifest
        .iter()
        .map(|(k, (id, ver))| (k.clone(), json!({"id": id, "version": ver})))
        .collect();
    ledger.insert(worker.to_string(), json!({"at": now_f64(), "keys": keys}));
    match serde_json::to_vec_pretty(&Value::Object(ledger)) {
        Ok(body) => {
            if let Err(e) = write_private(&ledger_path(home), &body) {
                tracing::warn!(session = worker, error = %e, measured = true, n_considered = 1,
                    verdict = "vault_ledger_write_failed",
                    "vault: could not record what this launch delivered; the provisioning view will say unknown");
            }
        }
        Err(e) => tracing::warn!(error = %e, verdict = "vault_ledger_write_failed", "vault: ledger did not serialize"),
    }
}

/// How a worker's running process relates to the value it should have.
fn process_state(running: bool, held: Option<&serde_json::Map<String, Value>>, key: &str, win: &Value) -> &'static str {
    if !running {
        return "next_launch";
    }
    let Some(h) = held else { return "unknown" };
    match h.get(key) {
        None => "not_in_process",
        Some(x) if x["id"] == win["id"] && x["version"].as_u64() == Some(item_version(win)) => "current",
        Some(_) => "stale",
    }
}

/// One worker's provisioned secrets: names, source scope, version and whether
/// its running process holds the current value. Never a value.
fn provisioned(home: &Path, items: &[Value], worker: &str, running: bool, ledger: &serde_json::Map<String, Value>) -> Value {
    let groups = worker_groups(home, worker);
    let res = resolve_with(items, worker, &groups);
    let held = ledger.get(worker).and_then(|e| e["keys"].as_object()).filter(|_| running);
    let resolved: Vec<Value> = res
        .iter()
        .map(|(k, (win, sh))| {
            json!({
                "key": k, "id": win["id"], "scope": win["scope"], "target": win["target"],
                "version": item_version(win), "note": win["note"],
                "state": process_state(running, held, k, win),
                "shadows": sh.iter().map(|s| json!({"id": s["id"], "scope": s["scope"], "target": s["target"]})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let revoked_held: Vec<String> = held
        .map(|h| h.keys().filter(|k| !res.contains_key(*k)).cloned().collect())
        .unwrap_or_default();
    json!({
        "worker": worker, "running": running, "isolated": worker_isolated(home, worker),
        "groups": groups.iter().collect::<Vec<_>>(),
        "tracked": ledger.get(worker).is_some(),
        "launched_at": ledger.get(worker).map(|e| e["at"].clone()).unwrap_or(Value::Null),
        "resolved": resolved, "revoked_held": revoked_held,
    })
}

/// Running workers whose process holds `id` at a version other than `version`
/// (or holds a key whose item is now gone, when `version` is None).
fn holders(ledger: &serde_json::Map<String, Value>, running: &std::collections::BTreeSet<String>, id: &str, version: Option<u64>) -> Vec<String> {
    ledger
        .iter()
        .filter(|(w, _)| running.contains(*w))
        .filter(|(_, e)| {
            e["keys"].as_object().is_some_and(|keys| {
                keys.values().any(|x| x["id"] == id && version.is_none_or(|v| x["version"].as_u64() != Some(v)))
            })
        })
        .map(|(w, _)| w.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn worker_origin(headers: &HeaderMap) -> Option<String> {
    ["x-amux-session", "x-amux-worker"].iter().find_map(|h| {
        headers
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn err(code: StatusCode, v: Value) -> Response {
    (code, Json(v)).into_response()
}

fn refuse_worker(home: &Path, who: &str, action: &str) -> Response {
    tracing::warn!(session = who, action, measured = true, n_considered = 1,
        verdict = "vault_write_refused_worker_origin",
        "vault: a worker origin tried to change or read a secret; only the owner can (AMUX-5375)");
    audit(home, json!({"ts": now_f64(), "event": "secret_write_refused", "action": action, "by": who}));
    err(StatusCode::FORBIDDEN, json!({"error": "only the owner can add, change, reveal or delete vault secrets",
        "why": "a worker that could write a secret could re-point another lane's credentials"}))
}

/// The owner, and only the owner: no worker origin, not an invited member,
/// and, on a host with an auth token, the owner bearer itself. A request that
/// merely OMITS the worker headers is not the owner, because a worker shell
/// can do that (the same rule as `worker_identity::owner_allowed`). The
/// dashboard sends the bearer on every call (`_authHeaders`).
fn owner_only(state: &super::AppState, headers: &HeaderMap, uri: &Uri, home: &Path, action: &str) -> Result<(), Box<Response>> {
    if let Some(w) = worker_origin(headers) {
        return Err(Box::new(refuse_worker(home, &w, action)));
    }
    let refuse = |who: &str, verdict: &'static str, why: &str| {
        tracing::warn!(action, who, measured = true, n_considered = 1, verdict,
            "vault: a request that is not the owner was refused");
        audit(home, json!({"ts": now_f64(), "event": "secret_write_refused", "action": action, "by": who}));
        err(StatusCode::FORBIDDEN, json!({"error": "only the owner can manage vault secrets", "why": why}))
    };
    if super::org::is_verified_local_member(headers) {
        return Err(Box::new(refuse("member", "vault_refused_member",
            "an invited member can use workers but never manage or read the owner's credentials")));
    }
    if state.auth_token.is_some() && !super::auth::has_owner_token(state, headers, uri) {
        return Err(Box::new(refuse("no-owner-credential", "vault_owner_credential_missing",
            "send the owner bearer (the dashboard does); omitting the worker headers is not proof of ownership")));
    }
    if state.auth_token.is_none() {
        tracing::info!(action, measured = true, n_considered = 1, verdict = "vault_owner_by_absence",
            "vault: this host has no auth token, so any non-worker, non-member request is the owner");
    }
    Ok(())
}

fn qs(q: &Option<String>, key: &str) -> Option<String> {
    crate::api::fs::parse_qs(q.as_deref().unwrap_or(""))
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

fn store_unreadable(e: String) -> Response {
    tracing::warn!(error = %e, measured = false, n_considered = 0, verdict = "vault_store_unreadable",
        "vault: secrets.json could not be read (AMUX-5375)");
    err(StatusCode::INTERNAL_SERVER_ERROR, json!({"measured": false, "n_considered": 0, "why_unmeasured": e}))
}

async fn list(headers: HeaderMap, RawQuery(q): RawQuery) -> Response {
    let home = amux_home();
    let items = match load(&home) {
        Ok(i) => i,
        Err(e) => return store_unreadable(e),
    };
    // A worker sees the metadata of what applies to it, nothing else.
    let origin = worker_origin(&headers);
    let viewer = origin.clone().or_else(|| qs(&q, "worker"));
    let visible: Vec<&Value> = match &origin {
        Some(w) => {
            let groups = worker_groups(&home, w);
            items.iter().filter(|i| item_scope(i).is_some_and(|s| applies(&s, w, &groups))).collect()
        }
        None => items.iter().collect(),
    };
    let mut views: Vec<Value> = visible.iter().map(|i| public_view(i)).collect();
    // Owner view: how many workers each item actually reaches, and for how
    // many a more specific item overrides it.
    let workers = if origin.is_none() { known_workers(&home) } else { Vec::new() };
    if origin.is_none() {
        let mut eff: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for w in &workers {
            let groups = worker_groups(&home, w);
            for (win, shadowed) in resolve_with(&items, w, &groups).values() {
                eff.entry(win["id"].as_str().unwrap_or("").to_string()).or_default().0 += 1;
                for s in shadowed {
                    eff.entry(s["id"].as_str().unwrap_or("").to_string()).or_default().1 += 1;
                }
            }
        }
        for v in &mut views {
            let (e, o) = eff.get(v["id"].as_str().unwrap_or("")).copied().unwrap_or((0, 0));
            v["effective_for"] = json!(e);
            v["overridden_for"] = json!(o);
        }
    }
    let mut body = json!({
        "measured": true,
        "n_considered": items.len(),
        "items": views,
        "keystore": if keystore_is_file() { "file" } else { "keychain" },
    });
    if origin.is_none() {
        body["n_workers"] = json!(workers.len());
        body["all_groups"] = json!(workers
            .iter()
            .flat_map(|w| worker_groups(&home, w))
            .collect::<std::collections::BTreeSet<_>>());
        body["workers"] = json!(workers);
    }
    if let Some(w) = viewer.filter(|w| !w.is_empty()) {
        let running = super::session_verbs::running_workers().await.contains(&w);
        let p = provisioned(&home, &items, &w, running, &load_ledger(&home));
        body["resolved_for"] = json!(w);
        for k in ["resolved", "running", "isolated", "tracked", "launched_at", "revoked_held", "groups"] {
            body[k] = p[k].clone();
        }
    }
    Json(body).into_response()
}

fn body_value(body: &Value) -> Result<String, String> {
    let v = body.get("value").and_then(Value::as_str).ok_or("value is required")?;
    if v.is_empty() {
        return Err("value is empty".into());
    }
    if v.len() > MAX_VALUE_BYTES {
        return Err(format!("value is over {MAX_VALUE_BYTES} bytes"));
    }
    if v.contains('\0') {
        return Err("value contains a NUL byte, which no environment variable can carry".into());
    }
    Ok(v.to_string())
}

/// The description, from `note` or its UI spelling `description`.
fn body_note(body: &Value) -> Option<Result<String, String>> {
    let n = body.get("note").or_else(|| body.get("description"))?.as_str().unwrap_or("");
    Some(if n.chars().count() > 200 {
        Err("the description is over 200 characters".into())
    } else {
        Ok(n.trim().to_string())
    })
}

fn new_id() -> String {
    let mut b = [0u8; 6];
    OsRng.fill_bytes(&mut b);
    format!("sec_{}", hex::encode(b))
}

fn key_unavailable(e: &str) -> (StatusCode, Value) {
    tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "vault_key_unavailable",
        "vault: no master key, so nothing can be sealed or opened (AMUX-5375)");
    (StatusCode::SERVICE_UNAVAILABLE, json!({"error": format!("vault key unavailable: {e}")}))
}

fn create_item(home: &Path, key: &str, scope: &Scope, value: &str, by: &str, note: &str, source: Option<&str>) -> Result<Value, (StatusCode, Value)> {
    let _g = store_lock().lock().unwrap_or_else(|p| p.into_inner());
    let mut items = load(home).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
    if let Some(dup) = items.iter().find(|i| i["key"] == key && item_scope(i).as_ref() == Some(scope)) {
        return Err((StatusCode::CONFLICT, json!({"error": "this key already has a value at this scope; edit or rotate it instead",
            "id": dup["id"]})));
    }
    let master = master_key(home, true)
        .map_err(|e| key_unavailable(&e))?
        .expect("create=true returns a key or an error");
    let id = new_id();
    let (nonce, ct) = seal(&master, &id, key, scope, value).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
    let now = now_f64();
    let item = json!({"id": id, "key": key, "scope": scope.kind(), "target": scope.target(),
        "nonce": nonce, "ct": ct, "key_fp": fingerprint(&master), "created": now, "updated": now,
        "version": 1, "by": by, "note": note, "source": source});
    items.push(item.clone());
    save(home, &items).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
    Ok(item)
}

fn parse_body_scope(home: &Path, body: &Value, default_kind: &str, default_target: &str) -> Result<Scope, (StatusCode, Value)> {
    let scope = Scope::parse(
        body["scope"].as_str().unwrap_or(default_kind),
        body["target"].as_str().unwrap_or(default_target),
    )
    .map_err(|e| (StatusCode::BAD_REQUEST, json!({"error": e})))?;
    if let Scope::Worker(w) = &scope {
        if !home.join("sessions").join(format!("{w}.env")).exists() {
            return Err((StatusCode::NOT_FOUND, json!({"error": format!("no worker named {w}")})));
        }
    }
    Ok(scope)
}

async fn create(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "create") {
        return *r;
    }
    let key = body.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if let Err(e) = valid_key(&key) {
        return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "key"}));
    }
    let scope = match parse_body_scope(&home, &body, "global", "") {
        Ok(s) => s,
        Err((c, v)) => return err(c, v),
    };
    let value = match body_value(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "value"})),
    };
    let note = match body_note(&body).unwrap_or(Ok(String::new())) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "description"})),
    };
    match create_item(&home, &key, &scope, &value, "owner", &note, None) {
        Ok(item) => {
            audit(&home, json!({"ts": now_f64(), "event": "secret_added", "item": item["id"], "key": key,
                "scope": scope.kind(), "target": scope.target(), "version": 1, "by": "owner"}));
            tracing::info!(key = %key, scope = scope.kind(), measured = true, n_considered = 1,
                verdict = "vault_secret_added", "vault: a secret was added (no value logged)");
            (StatusCode::CREATED, Json(json!({"ok": true, "item": public_view(&item),
                "applies": "workers receive it at their next start or restart"}))).into_response()
        }
        Err((code, v)) => err(code, v),
    }
}

/// Seal `value` onto the item at `pos` under its (possibly new) key and scope.
fn reseal(items: &mut [Value], pos: usize, master: &[u8; 32], key: &str, scope: &Scope, value: &str) -> Result<(), String> {
    let id = items[pos]["id"].as_str().unwrap_or("").to_string();
    let (nonce, ct) = seal(master, &id, key, scope, value)?;
    let it = &mut items[pos];
    it["key"] = json!(key);
    it["scope"] = json!(scope.kind());
    it["target"] = json!(scope.target());
    it["nonce"] = json!(nonce);
    it["ct"] = json!(ct);
    it["key_fp"] = json!(fingerprint(master));
    Ok(())
}

/// Rotate: a new value for the same item. The version goes up, so a running
/// worker still holding the old one is visible as stale until it restarts.
async fn replace(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, AxPath(id): AxPath<String>, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "replace") {
        return *r;
    }
    let value = match body_value(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "value"})),
    };
    let note = match body_note(&body).transpose() {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "description"})),
    };
    let result = {
        let _g = store_lock().lock().unwrap_or_else(|p| p.into_inner());
        (|| -> Result<(Value, String, Scope, u64), (StatusCode, Value)> {
            let mut items = load(&home).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
            let pos = items.iter().position(|i| i["id"] == id.as_str())
                .ok_or((StatusCode::NOT_FOUND, json!({"error": "no such secret"})))?;
            let master = master_key(&home, true).map_err(|e| key_unavailable(&e))?.ok_or_else(|| key_unavailable("missing"))?;
            let (key, scope) = (items[pos]["key"].as_str().unwrap_or("").to_string(), item_scope(&items[pos]).unwrap_or(Scope::Global));
            reseal(&mut items, pos, &master, &key, &scope, &value).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
            let version = item_version(&items[pos]) + 1;
            let now = now_f64();
            let it = &mut items[pos];
            it["version"] = json!(version);
            it["updated"] = json!(now);
            it["rotated"] = json!(now);
            it["by"] = json!("owner");
            if let Some(n) = &note {
                it["note"] = json!(n);
            }
            let view = public_view(it);
            save(&home, &items).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
            Ok((view, key, scope, version))
        })()
    };
    let (view, key, scope, version) = match result {
        Ok(x) => x,
        Err((c, v)) => return err(c, v),
    };
    let running = super::session_verbs::running_workers().await;
    let stale = holders(&load_ledger(&home), &running, &id, Some(version));
    audit(&home, json!({"ts": now_f64(), "event": "secret_replaced", "item": id, "key": key,
        "scope": scope.kind(), "target": scope.target(), "version": version, "by": "owner",
        "running_stale": stale.len()}));
    tracing::info!(key = %key, version, running_stale = stale.len(), measured = true, n_considered = running.len(),
        verdict = "vault_secret_rotated", "vault: a secret was rotated; running workers keep the old value until restarted");
    Json(json!({"ok": true, "item": view, "running_stale": stale,
        "applies": "running workers keep the previous value until they restart; new starts get this one"})).into_response()
}

/// Edit: description, name and scope, and optionally a new value. A name or
/// scope change re-seals the same item (the associated data binds both), so
/// the ciphertext can never be moved without the key.
async fn update(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, AxPath(id): AxPath<String>, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "update") {
        return *r;
    }
    let new_value = match body.get("value").and_then(Value::as_str) {
        Some("") | None => None,
        Some(_) => match body_value(&body) {
            Ok(v) => Some(v),
            Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "value"})),
        },
    };
    let note = match body_note(&body).transpose() {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "description"})),
    };
    let new_key = body.get("key").and_then(Value::as_str).map(|k| k.trim().to_string());
    if let Some(k) = &new_key {
        if let Err(e) = valid_key(k) {
            return err(StatusCode::BAD_REQUEST, json!({"error": e, "field": "key"}));
        }
    }
    let result = {
        let _g = store_lock().lock().unwrap_or_else(|p| p.into_inner());
        (|| -> Result<(Value, Vec<&'static str>, Value), (StatusCode, Value)> {
            let mut items = load(&home).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
            let pos = items.iter().position(|i| i["id"] == id.as_str())
                .ok_or((StatusCode::NOT_FOUND, json!({"error": "no such secret"})))?;
            let old_key = items[pos]["key"].as_str().unwrap_or("").to_string();
            let old_scope = item_scope(&items[pos]).unwrap_or(Scope::Global);
            let scope = if body.get("scope").is_some() {
                parse_body_scope(&home, &body, "global", "")?
            } else {
                old_scope.clone()
            };
            let key = new_key.clone().unwrap_or_else(|| old_key.clone());
            let moved = key != old_key || scope != old_scope;
            if moved {
                if let Some(dup) = items.iter().find(|i| i["id"] != id.as_str() && i["key"] == key.as_str() && item_scope(i).as_ref() == Some(&scope)) {
                    return Err((StatusCode::CONFLICT, json!({"error": "another secret already has this name at this scope", "id": dup["id"], "field": "key"})));
                }
            }
            let mut changes: Vec<&'static str> = Vec::new();
            if moved || new_value.is_some() {
                let master = master_key(&home, true).map_err(|e| key_unavailable(&e))?.ok_or_else(|| key_unavailable("missing"))?;
                let value = match &new_value {
                    Some(v) => v.clone(),
                    None => open(&master, &items[pos]).map_err(|e| (StatusCode::CONFLICT, json!({"error": format!("the current value does not open, so it cannot be moved: {e}. Set a new value.")})))?,
                };
                reseal(&mut items, pos, &master, &key, &scope, &value).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
                if key != old_key { changes.push("key"); }
                if scope != old_scope { changes.push("scope"); }
            }
            let now = now_f64();
            if new_value.is_some() {
                let v = item_version(&items[pos]) + 1;
                items[pos]["version"] = json!(v);
                items[pos]["rotated"] = json!(now);
                changes.push("value");
            }
            if let Some(n) = &note {
                if items[pos]["note"].as_str().unwrap_or("") != n {
                    items[pos]["note"] = json!(n);
                    changes.push("description");
                }
            }
            if changes.is_empty() {
                return Ok((public_view(&items[pos]), changes, Value::Null));
            }
            items[pos]["updated"] = json!(now);
            items[pos]["by"] = json!("owner");
            let view = public_view(&items[pos]);
            save(&home, &items).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})))?;
            Ok((view, changes, json!({"key": old_key, "scope": old_scope.kind(), "target": old_scope.target()})))
        })()
    };
    let (view, changes, from) = match result {
        Ok(x) => x,
        Err((c, v)) => return err(c, v),
    };
    if changes.is_empty() {
        return Json(json!({"ok": true, "item": view, "changes": []})).into_response();
    }
    let running = super::session_verbs::running_workers().await;
    let affects_process = changes.iter().any(|c| *c != "description");
    let stale = if affects_process { holders(&load_ledger(&home), &running, &id, None) } else { Vec::new() };
    audit(&home, json!({"ts": now_f64(), "event": "secret_updated", "item": id, "key": view["key"],
        "scope": view["scope"], "target": view["target"], "version": view["version"], "changes": changes,
        "from": from, "by": "owner", "running_stale": stale.len()}));
    tracing::info!(key = %view["key"], changes = ?changes, measured = true, n_considered = 1,
        verdict = "vault_secret_updated", "vault: a secret's metadata or value changed (no value logged)");
    Json(json!({"ok": true, "item": view, "changes": changes, "running_stale": stale})).into_response()
}

async fn remove(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "delete") {
        return *r;
    }
    let gone = {
        let _g = store_lock().lock().unwrap_or_else(|p| p.into_inner());
        let mut items = match load(&home) {
            Ok(i) => i,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})),
        };
        let Some(pos) = items.iter().position(|i| i["id"] == id.as_str()) else {
            return err(StatusCode::NOT_FOUND, json!({"error": "no such secret"}));
        };
        let gone = items.remove(pos);
        if let Err(e) = save(&home, &items) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e}));
        }
        gone
    };
    let running = super::session_verbs::running_workers().await;
    let held = holders(&load_ledger(&home), &running, &id, None);
    audit(&home, json!({"ts": now_f64(), "event": "secret_deleted", "item": id, "key": gone["key"],
        "scope": gone["scope"], "target": gone["target"], "by": "owner", "still_held_by": held.len()}));
    tracing::info!(key = %gone["key"], still_held_by = held.len(), measured = true, n_considered = running.len(),
        verdict = "vault_secret_deleted", "vault: a secret was deleted; running workers holding it lose it at restart");
    Json(json!({"ok": true, "deleted": id, "still_held_by": held,
        "applies": "running workers keep it until they restart; no new start receives it"})).into_response()
}

/// Seconds the dashboard shows a revealed value before masking it again.
const REVEAL_REMASK_S: u64 = 20;

/// The one endpoint that returns a value: owner only, one item, audited, never
/// cached. The dashboard masks it again after `remask_after_s`.
async fn reveal(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "reveal") {
        return *r;
    }
    let items = match load(&home) {
        Ok(i) => i,
        Err(e) => return store_unreadable(e),
    };
    let Some(item) = items.iter().find(|i| i["id"] == id.as_str()) else {
        return err(StatusCode::NOT_FOUND, json!({"error": "no such secret"}));
    };
    let master = match master_key(&home, false) {
        Ok(Some(k)) => k,
        Ok(None) => { let (c, v) = key_unavailable("missing"); return err(c, v); }
        Err(e) => { let (c, v) = key_unavailable(&e); return err(c, v); }
    };
    let value = match open(&master, item) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::CONFLICT, json!({"error": e})),
    };
    audit(&home, json!({"ts": now_f64(), "event": "secret_revealed", "item": id, "key": item["key"],
        "scope": item["scope"], "target": item["target"], "version": item_version(item), "by": "owner"}));
    tracing::info!(key = %item["key"], measured = true, n_considered = 1, verdict = "vault_secret_revealed",
        "vault: the owner revealed a secret in the dashboard (no value logged)");
    let mut resp = Json(json!({"id": id, "key": item["key"], "version": item_version(item), "value": value,
        "remask_after_s": REVEAL_REMASK_S})).into_response();
    let h = resp.headers_mut();
    h.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store, max-age=0"));
    h.insert(axum::http::header::PRAGMA, axum::http::HeaderValue::from_static("no-cache"));
    resp
}

/// Effective access for one item: every worker it reaches, why, and whether
/// the worker's running process holds the current version.
async fn access(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, AxPath(id): AxPath<String>) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "access") {
        return *r;
    }
    let items = match load(&home) {
        Ok(i) => i,
        Err(e) => return store_unreadable(e),
    };
    let Some(item) = items.iter().find(|i| i["id"] == id.as_str()) else {
        return err(StatusCode::NOT_FOUND, json!({"error": "no such secret"}));
    };
    let Some(scope) = item_scope(item) else {
        return err(StatusCode::CONFLICT, json!({"error": "item has no valid scope"}));
    };
    let key = item["key"].as_str().unwrap_or("");
    let workers = known_workers(&home);
    let running = super::session_verbs::running_workers().await;
    let ledger = load_ledger(&home);
    let mut rows = Vec::new();
    let (mut effective, mut overridden, mut stale) = (0usize, 0usize, 0usize);
    for w in &workers {
        let groups = worker_groups(&home, w);
        if !applies(&scope, w, &groups) {
            continue;
        }
        let res = resolve_with(&items, w, &groups);
        let Some((win, _)) = res.get(key) else { continue };
        let is_running = running.contains(w);
        let held = ledger.get(w.as_str()).and_then(|e| e["keys"].as_object()).filter(|_| is_running);
        let wins = win["id"] == id.as_str();
        let state = if wins { process_state(is_running, held, key, win) } else { "overridden" };
        if wins { effective += 1 } else { overridden += 1 }
        if state == "stale" || state == "not_in_process" { stale += 1 }
        rows.push(json!({
            "worker": w, "groups": groups, "running": is_running, "isolated": worker_isolated(&home, w),
            "via": match &scope { Scope::Global => "global".to_string(), Scope::Group(g) => format!("group:{g}"), Scope::Worker(_) => "worker".to_string() },
            "effective": wins,
            "overridden_by": if wins { Value::Null } else { json!({"id": win["id"], "scope": win["scope"], "target": win["target"]}) },
            "state": state,
        }));
    }
    Json(json!({
        "measured": true, "n_considered": workers.len(),
        "item": public_view(item),
        "precedence": "worker > group > global; among a worker's groups the one that sorts last wins",
        "workers": rows,
        "summary": {"effective": effective, "overridden": overridden, "running_needs_restart": stale},
    }))
    .into_response()
}

/// Fields an audit line may carry out of this endpoint. The writer never puts
/// a value in the file; this is the second wall.
const AUDIT_FIELDS: [&str; 16] = [
    "ts", "event", "item", "key", "scope", "target", "by", "version", "path", "session", "action",
    "changes", "from", "running_stale", "still_held_by", "error",
];

fn audit_view(line: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for f in AUDIT_FIELDS {
        if let Some(v) = line.get(f) {
            out.insert(f.to_string(), v.clone());
        }
    }
    // A delivery names keys and items, never values.
    for f in ["delivered", "failed"] {
        if let Some(arr) = line[f].as_array() {
            out.insert(f.to_string(), json!(arr.iter().map(|d| json!({"key": d["key"], "item": d["item"],
                "version": d["version"], "scope": d["scope"], "target": d["target"]})).collect::<Vec<_>>()));
        }
    }
    Value::Object(out)
}

fn is_secret_event(line: &Value) -> bool {
    line["event"].as_str().is_some_and(|e| e.starts_with("secret"))
}

async fn audit_log(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, RawQuery(q): RawQuery) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "audit") {
        return *r;
    }
    let limit = qs(&q, "limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(200).clamp(1, 1000);
    let want_id = qs(&q, "id").filter(|s| !s.is_empty());
    let want_key = qs(&q, "key").filter(|s| !s.is_empty());
    let path = home.join("logs").join("vault-audit.jsonl");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({"measured": false, "n_considered": 0,
                "why_unmeasured": format!("could not read the vault audit log: {e}"), "entries": []}));
        }
    };
    let mut n_considered = 0usize;
    let mut unparsed = 0usize;
    let mut entries: Vec<Value> = Vec::new();
    for l in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            unparsed += usize::from(!l.trim().is_empty());
            continue;
        };
        if !is_secret_event(&v) {
            continue;
        }
        n_considered += 1;
        let key_hit = |k: &str| {
            v["key"].as_str() == Some(k)
                || v["delivered"].as_array().is_some_and(|a| a.iter().any(|d| d["key"].as_str() == Some(k)))
        };
        let id_hit = |i: &str| {
            v["item"].as_str() == Some(i)
                || v["delivered"].as_array().is_some_and(|a| a.iter().any(|d| d["item"].as_str() == Some(i)))
        };
        if want_key.as_deref().is_some_and(|k| !key_hit(k)) || want_id.as_deref().is_some_and(|i| !id_hit(i)) {
            continue;
        }
        if entries.len() < limit {
            entries.push(audit_view(&v));
        }
    }
    Json(json!({"measured": true, "n_considered": n_considered, "unparsed_lines": unparsed,
        "limit": limit, "entries": entries})).into_response()
}

/// The plaintext file a scope's key lives in today, for an opt-in import.
fn env_source(home: &Path, scope: &Scope, source: Option<&str>) -> PathBuf {
    match scope {
        Scope::Global if source == Some("server.env") => home.join("server.env"),
        Scope::Global => home.join("amux.env"),
        Scope::Group(g) => home.join("env").join(format!("{g}.env")),
        Scope::Worker(w) => home.join("sessions").join(format!("{w}.env")),
    }
}

/// Opt-in, one key at a time: copy a value that lives in a plaintext env file
/// into the vault at the matching scope. The file is not edited: removing the
/// plaintext line is the owner's step, once the worker has relaunched with the
/// vault value (which wins over the file for that key).
async fn import(State(state): State<super::AppState>, uri: Uri, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let home = amux_home();
    if let Err(r) = owner_only(&state, &headers, &uri, &home, "import") {
        return *r;
    }
    let key = body.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if let Err(e) = valid_key(&key) {
        return err(StatusCode::BAD_REQUEST, json!({"error": e}));
    }
    let scope = match Scope::parse(body["scope"].as_str().unwrap_or("global"), body["target"].as_str().unwrap_or("")) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({"error": e})),
    };
    let src = env_source(&home, &scope, body.get("source").and_then(Value::as_str));
    let env = super::session_verbs::EnvFile::load(&src);
    let Some(value) = env.get(&key).map(str::to_string).filter(|v| !v.is_empty()) else {
        return err(StatusCode::NOT_FOUND, json!({"error": format!("{key} is not set in {}", src.display())}));
    };
    let src_s = src.to_string_lossy().into_owned();
    match create_item(&home, &key, &scope, &value, "owner", "", Some(&src_s)) {
        Ok(item) => {
            audit(&home, json!({"ts": now_f64(), "event": "secret_imported", "item": item["id"], "key": key,
                "scope": scope.kind(), "target": scope.target(), "from": src_s, "by": "owner"}));
            (StatusCode::CREATED, Json(json!({"ok": true, "item": public_view(&item),
                "still_in": src_s,
                "next": "the plaintext line is untouched; the vault value wins for this key from the worker's next launch. Remove the line yourself when you are ready."}))).into_response()
        }
        Err((code, v)) => err(code, v),
    }
}

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/vault/secrets", get(list).post(create))
        .route("/api/vault/secrets/import", post(import))
        .route("/api/vault/secrets/audit", get(audit_log))
        .route("/api/vault/secrets/{id}", axum::routing::put(replace).patch(update).delete(remove))
        .route("/api/vault/secrets/{id}/reveal", post(reveal))
        .route("/api/vault/secrets/{id}/access", get(access))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sessions")).unwrap();
        d
    }

    fn put(items: &mut Vec<Value>, master: &[u8; 32], key: &str, scope: Scope, value: &str) {
        let id = format!("sec_{}", items.len());
        let (nonce, ct) = seal(master, &id, key, &scope, value).unwrap();
        items.push(json!({"id": id, "key": key, "scope": scope.kind(), "target": scope.target(),
            "nonce": nonce, "ct": ct, "key_fp": fingerprint(master)}));
    }

    #[test]
    fn keys_are_env_names_and_harness_variables_are_refused() {
        assert!(valid_key("GITHUB_TOKEN").is_ok());
        assert!(valid_key("_X1").is_ok());
        for bad in ["", "1X", "lower", "A-B", "A B", "CC_DIR", "AMUX_URL", "PATH", "HOME", "AMUX_VAULT_STAGE_X"] {
            assert!(valid_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_value_opens_only_on_its_own_item() {
        let master = [7u8; 32];
        let mut items = Vec::new();
        put(&mut items, &master, "TOKEN", Scope::Worker("w1".into()), "s3cret");
        assert_eq!(open(&master, &items[0]).unwrap(), "s3cret");
        // The ciphertext is not the value.
        assert!(!items[0].to_string().contains("s3cret"));
        // Moving it to another scope must fail, not deliver it there.
        let mut moved = items[0].clone();
        moved["scope"] = json!("global");
        moved["target"] = json!("");
        assert!(open(&master, &moved).is_err());
        // A different master key is reported as a key change.
        let err = open(&[8u8; 32], &items[0]).unwrap_err();
        assert!(err.contains("different master key"), "{err}");
    }

    #[test]
    fn reseal_moves_an_item_and_the_old_scope_no_longer_opens() {
        let master = [3u8; 32];
        let mut items = Vec::new();
        put(&mut items, &master, "TOKEN", Scope::Group("ops".into()), "v1");
        let before = items[0].clone();
        reseal(&mut items, 0, &master, "TOKEN2", &Scope::Worker("w9".into()), "v1").unwrap();
        assert_eq!(items[0]["id"], before["id"], "a move keeps the item's identity");
        assert_eq!((items[0]["key"].as_str(), items[0]["scope"].as_str(), items[0]["target"].as_str()),
            (Some("TOKEN2"), Some("worker"), Some("w9")));
        assert_eq!(open(&master, &items[0]).unwrap(), "v1");
        // The OLD ciphertext under the NEW metadata must not open.
        let mut spliced = items[0].clone();
        spliced["ct"] = before["ct"].clone();
        spliced["nonce"] = before["nonce"].clone();
        assert!(open(&master, &spliced).is_err());
    }

    #[test]
    fn worker_beats_group_beats_global_and_the_last_group_wins() {
        let h = home();
        std::fs::write(h.path().join("sessions/w1.env"), "CC_TAGS=\"alpha,beta\"\n").unwrap();
        let master = [1u8; 32];
        let mut items = Vec::new();
        put(&mut items, &master, "A", Scope::Global, "g");
        put(&mut items, &master, "A", Scope::Group("alpha".into()), "alpha");
        put(&mut items, &master, "A", Scope::Group("beta".into()), "beta");
        put(&mut items, &master, "B", Scope::Global, "g");
        put(&mut items, &master, "B", Scope::Worker("w1".into()), "mine");
        put(&mut items, &master, "C", Scope::Group("gamma".into()), "not mine");
        put(&mut items, &master, "D", Scope::Worker("w2".into()), "not mine");
        let r = resolve(&items, h.path(), "w1");
        let win = |k: &str| open(&master, r[k].0).unwrap();
        assert_eq!(win("A"), "beta", "beta sorts after alpha, as its file is sourced later");
        assert_eq!(r["A"].1.len(), 2, "global and alpha are shadowed");
        assert_eq!(win("B"), "mine");
        assert!(!r.contains_key("C") && !r.contains_key("D"), "other groups and workers never apply");
        // The order items were written in never changes the answer.
        let mut rev = items.clone();
        rev.reverse();
        let r2 = resolve(&rev, h.path(), "w1");
        assert_eq!(r2["A"].0["id"], r["A"].0["id"]);
        assert_eq!(r2["B"].0["id"], r["B"].0["id"]);
    }

    #[test]
    fn delivery_hands_over_resolved_values_and_audits_keys_only() {
        let h = home();
        std::env::set_var("AMUX_VAULT_KEYSTORE", "file");
        std::fs::write(h.path().join("sessions/w1.env"), "CC_TAGS=\"ops\"\n").unwrap();
        let s = Scope::Group("ops".into());
        create_item(h.path(), "API_TOKEN", &s, "tok-123", "owner", "", None).unwrap();
        create_item(h.path(), "API_TOKEN", &Scope::Global, "tok-global", "owner", "", None).unwrap();
        let dup = create_item(h.path(), "API_TOKEN", &Scope::Global, "again", "owner", "", None);
        assert_eq!(dup.unwrap_err().0, StatusCode::CONFLICT);
        let store = std::fs::read_to_string(store_path(h.path())).unwrap();
        assert!(!store.contains("tok-123") && !store.contains("tok-global"), "no plaintext at rest");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [store_path(h.path()), key_file(h.path())] {
                assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600, "{}", f.display());
            }
        }
        let got = deliver(h.path(), "w1", DeliveryPath::Launch).unwrap();
        assert_eq!(got.values, vec![("API_TOKEN".to_string(), "tok-123".to_string())]);
        assert_eq!(got.manifest["API_TOKEN"].1, 1, "a new item is version 1");
        let other = deliver(h.path(), "stranger", DeliveryPath::ShellSchedule).unwrap();
        assert_eq!(other.values, vec![("API_TOKEN".to_string(), "tok-global".to_string())]);
        let audit = std::fs::read_to_string(h.path().join("logs/vault-audit.jsonl")).unwrap();
        assert!(audit.contains("secrets_delivered") && audit.contains("API_TOKEN"));
        assert!(audit.contains("\"path\":\"shell_schedule\""), "the path is named");
        assert!(!audit.contains("tok-123") && !audit.contains("tok-global"), "no value in the audit");
    }

    #[test]
    fn staged_env_moves_values_after_sourcing_and_names_keys_only() {
        let (env, line) = staged_env(&[("GH_TOKEN".into(), "v@lue $(rm -rf /)".into())]);
        assert_eq!(env, vec![("AMUX_VAULT_STAGE_GH_TOKEN".to_string(), "v@lue $(rm -rf /)".to_string())]);
        assert_eq!(line, "export GH_TOKEN=\"$AMUX_VAULT_STAGE_GH_TOKEN\"; unset AMUX_VAULT_STAGE_GH_TOKEN; ");
        assert!(!line.contains("v@lue"), "the shell line never carries a value");
        // And bash really does take the staged value, unexpanded, after a
        // sourced line set the same key.
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(format!("GH_TOKEN=plaintext; {line}printf %s \"$GH_TOKEN\"; printf '|%s' \"${{AMUX_VAULT_STAGE_GH_TOKEN-unset}}\""))
            .env("AMUX_VAULT_STAGE_GH_TOKEN", "v@lue $(rm -rf /)")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "v@lue $(rm -rf /)|unset");
    }

    #[test]
    fn redaction_masks_every_occurrence_and_skips_short_values() {
        let vals = vec![("GH_TOKEN".to_string(), "ghp_abcdef".to_string()), ("PIN".to_string(), "12".to_string())];
        assert_eq!(redact("a ghp_abcdef b ghp_abcdefghp_abcdef 12", &vals),
            "a [vault:GH_TOKEN] b [vault:GH_TOKEN][vault:GH_TOKEN] 12");
        assert_eq!(redact("nothing here", &[]), "nothing here");
    }

    #[test]
    fn process_state_tells_current_from_stale_and_unknown() {
        let win = json!({"id": "sec_a", "version": 2});
        let mut held = serde_json::Map::new();
        held.insert("K".into(), json!({"id": "sec_a", "version": 2}));
        assert_eq!(process_state(false, Some(&held), "K", &win), "next_launch");
        assert_eq!(process_state(true, None, "K", &win), "unknown");
        assert_eq!(process_state(true, Some(&held), "K", &win), "current");
        assert_eq!(process_state(true, Some(&held), "OTHER", &win), "not_in_process");
        held.insert("K".into(), json!({"id": "sec_a", "version": 1}));
        assert_eq!(process_state(true, Some(&held), "K", &win), "stale", "rotated since launch");
        held.insert("K".into(), json!({"id": "sec_b", "version": 2}));
        assert_eq!(process_state(true, Some(&held), "K", &win), "stale", "a different item won since launch");
    }

    #[test]
    fn the_ledger_records_ids_and_versions_and_finds_stale_holders() {
        let h = home();
        std::env::set_var("AMUX_VAULT_KEYSTORE", "file");
        let it = create_item(h.path(), "K", &Scope::Global, "secret-value-1", "owner", "", None).unwrap();
        let id = it["id"].as_str().unwrap().to_string();
        let d = deliver(h.path(), "w1", DeliveryPath::Launch).unwrap();
        record_running(h.path(), "w1", &d.manifest);
        let ledger = load_ledger(h.path());
        assert!(!std::fs::read_to_string(ledger_path(h.path())).unwrap().contains("secret-value-1"));
        let running: std::collections::BTreeSet<String> = ["w1".to_string()].into();
        assert!(holders(&ledger, &running, &id, Some(1)).is_empty(), "same version: not stale");
        assert_eq!(holders(&ledger, &running, &id, Some(2)), vec!["w1"], "rotated: stale");
        assert_eq!(holders(&ledger, &running, &id, None), vec!["w1"], "deleted: still held");
        assert!(holders(&ledger, &Default::default(), &id, None).is_empty(), "a stopped worker holds nothing");
    }

    #[test]
    fn the_audit_view_carries_only_named_fields() {
        let line = json!({"ts": 1.0, "event": "secrets_delivered", "session": "w1", "path": "launch",
            "value": "LEAK", "delivered": [{"key": "K", "item": "sec_1", "version": 1, "value": "LEAK2"}]});
        let v = audit_view(&line).to_string();
        assert!(!v.contains("LEAK"), "{v}");
        assert!(v.contains("\"key\":\"K\"") && v.contains("\"path\":\"launch\""));
    }
}
