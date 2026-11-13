//! Encryption at rest for connector grants (governance PR 18).
//!
//! A grant is an OAuth refresh token: whoever holds the file holds the account.
//! These files (`connectors/<family>/<account>.json` and the Gmail mirror
//! under the legacy token directory) were plain JSON. They are now sealed with
//! AES-256-GCM in an envelope that keeps the file a JSON object:
//!
//! `{"amux_at_rest":1,"purpose":"connector-grants","fp":"..","nonce":"..","ct":".."}`
//!
//! The associated data is the purpose plus the file's path under the home, so
//! a ciphertext copied over another account's file does not open.
//!
//! The key: one source per host (A10). `AMUX_KEY_PROVIDER` picks it:
//! - `file` / `keychain`: the vault master key (`vault_secrets::master_key_on`),
//!   from which a separate key is derived for this purpose (HMAC-SHA256);
//! - `env`: `AMUX_AT_REST_KEY` (64 hex chars or base64 of 32 bytes), injected
//!   by the operator, so a copy of the home alone cannot open the grants;
//! - `kms`: an interface stub. It refuses (locked) rather than pretending.
//!
//! Reads go through `secure_store`, which opens an envelope and migrates a
//! plain grant in place. A grant that cannot be opened is LOCKED, an error the
//! callers report as `locked`, never as "not connected" (that would send the
//! owner to reconnect an account that is fine).
//!
//! `AMUX_ENCRYPT_AT_REST=off` stops sealing new writes and migrations; sealed
//! grants still open. The owner's way back to plain files is
//! `POST /api/security/at-rest/decrypt` (`amux security decrypt-grants`), which
//! refuses while sealing is on.
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const PURPOSE: &str = "connector-grants";
const ENVELOPE_KEY: &str = "amux_at_rest";
const LOCKED_PREFIX: &str = "at_rest_locked: ";
/// The legacy Gmail token directory, spelled once.
pub(crate) const GMAIL_DIR: &str = concat!("gmail", "-tokens");

pub(crate) mod counters {
    use super::*;
    pub static SEALED: AtomicU64 = AtomicU64::new(0);
    pub static OPENED: AtomicU64 = AtomicU64::new(0);
    pub static MIGRATED: AtomicU64 = AtomicU64::new(0);
    pub static LOCKED: AtomicU64 = AtomicU64::new(0);
    pub static PLAIN_READ: AtomicU64 = AtomicU64::new(0);
    pub static DECRYPTED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    File,
    Keychain,
    Env,
    Kms,
}

impl Provider {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Provider::File => "file",
            Provider::Keychain => "keychain",
            Provider::Env => "env",
            Provider::Kms => "kms",
        }
    }
}

pub(crate) fn provider() -> Provider {
    match std::env::var("AMUX_KEY_PROVIDER").ok().as_deref().map(str::trim) {
        Some("env") => Provider::Env,
        Some("kms") | Some("kms-stub") => Provider::Kms,
        Some("file") => Provider::File,
        Some("keychain") => Provider::Keychain,
        _ if crate::api::vault_secrets::keystore_is_file() => Provider::File,
        _ => Provider::Keychain,
    }
}

/// Sealing new writes and migrating plain grants. Default on.
pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var("AMUX_ENCRYPT_AT_REST").ok().as_deref().map(str::trim),
        Some("off") | Some("0") | Some("false")
    )
}

/// The amux home a grant file belongs to, if `path` is a grant file:
/// `<home>/connectors/<family>/<account>.json` or `<home>/<gmail dir>/<account>.json`.
/// Lock sidecars, temps and the connectors registry files are not grants.
pub(crate) fn grant_home(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".json") || name.starts_with('.') {
        return None;
    }
    let parent = path.parent()?;
    if parent.file_name()?.to_str()? == GMAIL_DIR {
        return Some(parent.parent()?.to_path_buf());
    }
    let grand = parent.parent()?;
    if grand.file_name()?.to_str()? == "connectors" {
        return Some(grand.parent()?.to_path_buf());
    }
    None
}

pub(crate) fn is_envelope(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(64)];
    head.windows(ENVELOPE_KEY.len() + 2)
        .any(|w| w == format!("\"{ENVELOPE_KEY}\"").as_bytes())
        && serde_json::from_slice::<Value>(bytes)
            .ok()
            .is_some_and(|v| v.get(ENVELOPE_KEY).is_some())
}

/// An error a caller can tell apart from "no grant".
pub(crate) fn locked(reason: impl std::fmt::Display) -> io::Error {
    counters::bump(&counters::LOCKED);
    io::Error::new(io::ErrorKind::PermissionDenied, format!("{LOCKED_PREFIX}{reason}"))
}

pub(crate) fn is_locked(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied && e.to_string().starts_with(LOCKED_PREFIX)
}

fn decode_key(raw: &str) -> Option<[u8; 32]> {
    let raw = raw.trim();
    if raw.len() == 64 {
        if let Ok(b) = hex::decode(raw) {
            return <[u8; 32]>::try_from(b.as_slice()).ok();
        }
    }
    let b = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    <[u8; 32]>::try_from(b.as_slice()).ok()
}

/// The key this purpose seals with. `create` mints the vault master key when
/// none exists (a write); a read never creates one.
fn purpose_key(home: &Path, create: bool) -> Result<[u8; 32], String> {
    purpose_key_with(home, create, provider(), std::env::var("AMUX_AT_REST_KEY").ok())
}

fn purpose_key_with(home: &Path, create: bool, prov: Provider, env_key: Option<String>) -> Result<[u8; 32], String> {
    let master = match prov {
        Provider::Env => {
            let raw = env_key
                .ok_or_else(|| "AMUX_KEY_PROVIDER=env but AMUX_AT_REST_KEY is not set".to_string())?;
            decode_key(&raw).ok_or("AMUX_AT_REST_KEY is not 32 bytes (64 hex chars or base64)")?
        }
        Provider::Kms => {
            // TODO(gov-18): AWS KMS / GCP KMS envelope keys. The interface is
            // this function; until a provider exists, refuse rather than fake.
            return Err("AMUX_KEY_PROVIDER=kms is an interface stub with no provider yet".into());
        }
        p @ (Provider::File | Provider::Keychain) => {
            crate::api::vault_secrets::master_key_on(home, create, p == Provider::File)?
                .ok_or("no master key exists yet for this home")?
        }
    };
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&master).map_err(|e| e.to_string())?;
    mac.update(format!("amux-at-rest-v1|{PURPOSE}").as_bytes());
    Ok(mac.finalize().into_bytes().into())
}

fn aad(home: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(home).unwrap_or(path);
    format!("amux-at-rest-v1|{PURPOSE}|{}", rel.to_string_lossy())
}

/// Seal `plain` for the grant file at `path`.
pub(crate) fn seal(home: &Path, path: &Path, plain: &[u8]) -> Result<Vec<u8>, String> {
    let key = purpose_key(home, true)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("OS random source");
    let a = aad(home, path);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: a.as_bytes() })
        .map_err(|_| "encryption failed".to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD;
    counters::bump(&counters::SEALED);
    Ok(json!({
        ENVELOPE_KEY: 1,
        "purpose": PURPOSE,
        "provider": provider().name(),
        "fp": crate::api::vault_secrets::fingerprint(&key),
        "nonce": b64.encode(nonce),
        "ct": b64.encode(ct),
    })
    .to_string()
    .into_bytes())
}

/// Open an envelope read from the grant file at `path`. Any failure is LOCKED.
pub(crate) fn open(home: &Path, path: &Path, bytes: &[u8]) -> io::Result<Vec<u8>> {
    let env: Value = serde_json::from_slice(bytes).map_err(|_| locked("envelope is not JSON"))?;
    let key = purpose_key(home, false).map_err(locked)?;
    if env["fp"].as_str().is_some_and(|fp| fp != crate::api::vault_secrets::fingerprint(&key)) {
        return Err(locked("sealed under a different key (the key was replaced or the provider changed)"));
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let nonce = b64.decode(env["nonce"].as_str().unwrap_or("")).map_err(|_| locked("bad nonce"))?;
    let ct = b64.decode(env["ct"].as_str().unwrap_or("")).map_err(|_| locked("bad ciphertext"))?;
    if nonce.len() != 12 {
        return Err(locked("bad nonce length"));
    }
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(locked)?;
    let a = aad(home, path);
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: a.as_bytes() })
        .map_err(|_| locked("ciphertext does not open here (tampered, or copied from another file)"))?;
    counters::bump(&counters::OPENED);
    Ok(pt)
}

/// Every grant file under `home`.
pub(crate) fn grant_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push_dir = |dir: PathBuf| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_file() && grant_home(&p).is_some() {
                    out.push(p);
                }
            }
        }
    };
    if let Ok(rd) = std::fs::read_dir(home.join("connectors")) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                push_dir(e.path());
            }
        }
    }
    push_dir(home.join(GMAIL_DIR));
    out.sort();
    out
}

/// Seal every plain grant under `home` (server start). Returns (considered, migrated, failed).
pub(crate) fn migrate_home(home: &Path) -> (usize, usize, usize) {
    if !enabled() {
        return (0, 0, 0);
    }
    let files = grant_files(home);
    let (mut migrated, mut failed) = (0, 0);
    for p in &files {
        let Ok(bytes) = std::fs::read(p) else { continue };
        if is_envelope(&bytes) {
            continue;
        }
        match super::secure_store::migrate_plain(p, &bytes) {
            Ok(true) => migrated += 1,
            Ok(false) => {}
            Err(e) => {
                failed += 1;
                tracing::warn!(path = %p.display(), error = %e, measured = true, n_considered = 1,
                    verdict = "at_rest_migration_failed", "a plain connector grant could not be sealed; it stays readable");
            }
        }
    }
    tracing::info!(considered = files.len(), migrated, failed, provider = provider().name(), measured = true,
        n_considered = files.len(), verdict = "at_rest_migration_pass", "connector grants checked for encryption at rest");
    (files.len(), migrated, failed)
}

/// What is and is not encrypted at rest, measured from the files.
pub(crate) fn status(home: &Path) -> Value {
    let files = grant_files(home);
    let (mut sealed, mut plain, mut locked_n) = (0usize, 0usize, 0usize);
    for p in &files {
        let Ok(bytes) = std::fs::read(p) else { continue };
        if is_envelope(&bytes) {
            sealed += 1;
            if open(home, p, &bytes).is_err() {
                locked_n += 1;
            }
        } else {
            plain += 1;
        }
    }
    let state = match (sealed, plain) {
        (0, 0) => "none",
        (_, 0) => "yes",
        (0, _) => "no",
        _ => "partial",
    };
    let prov = provider();
    let key_file = crate::api::vault_secrets::key_file(home);
    let (colocated, key_available) = match prov {
        Provider::File => (key_file.starts_with(home), key_file.exists()),
        Provider::Keychain => (false, purpose_key(home, false).is_ok()),
        Provider::Env => (false, purpose_key(home, false).is_ok()),
        Provider::Kms => (false, false),
    };
    let claude_projects = dirs_home().map(|h| h.join(".claude").join("projects"));
    json!({
        "measured": true,
        "n_considered": files.len(),
        "sealing": if enabled() { "on" } else { "off" },
        "key": {
            "provider": prov.name(),
            "available": key_available,
            "colocated_with_data": colocated,
            "note": match prov {
                Provider::File => "the key is a 0600 file inside this home: a copy of the home carries both",
                Provider::Keychain => "the key is in the login Keychain, not in the home",
                Provider::Env => "the key comes from AMUX_AT_REST_KEY, injected by the operator; it is not in the home",
                Provider::Kms => "kms is an interface stub: no provider, so grants stay locked",
            },
        },
        "stores": {
            "connector_grants": {
                "encrypted": state, "files": files.len(), "sealed": sealed, "plain": plain, "locked": locked_n,
            },
            "vault_secrets": {"encrypted": "yes", "note": "values sealed with the vault master key (AES-256-GCM)"},
            "database": {
                "encrypted": "no",
                "path": home.join("amux.db").to_string_lossy(),
                "plan": "SQLCipher; see docs/ops/encryption-at-rest.md. Until then use full-disk encryption (LUKS, FileVault).",
            },
            "transcripts": {
                "encrypted": "no",
                "owner": "claude-code",
                "path": claude_projects.map(|p| p.to_string_lossy().into_owned()),
                "note": "Claude Code writes these; amux reports and does not touch them (A2). Full-disk encryption covers them.",
            },
            "gmail_oauth_client": {"encrypted": "no", "note": "the OAuth client config is not a grant; deferred (docs/ops/encryption-at-rest.md)"},
        },
        "counters": report(),
    })
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Process counters, for /api/contract/counters.
pub(crate) fn report() -> Value {
    use counters::*;
    json!({
        "measured": true,
        "sealing": if enabled() { "on" } else { "off" },
        "provider": provider().name(),
        "sealed": get(&SEALED),
        "opened": get(&OPENED),
        "migrated": get(&MIGRATED),
        "locked": get(&LOCKED),
        "plain_read": get(&PLAIN_READ),
        "decrypted_by_owner": get(&DECRYPTED),
        "n_considered": get(&SEALED) + get(&OPENED) + get(&PLAIN_READ),
    })
}

/// Rewrite every sealed grant as plain JSON. Only with sealing off, or the
/// next write would seal it again. Returns (considered, decrypted, locked).
pub(crate) fn decrypt_all(home: &Path) -> Result<(usize, usize, usize), String> {
    if enabled() {
        return Err("set AMUX_ENCRYPT_AT_REST=off first, or the next write seals the grant again".into());
    }
    let files = grant_files(home);
    let (mut done, mut locked_n) = (0, 0);
    for p in &files {
        let Ok(bytes) = std::fs::read(p) else { continue };
        if !is_envelope(&bytes) {
            continue;
        }
        match open(home, p, &bytes) {
            Ok(plain) => {
                super::secure_store::write(p, &plain).map_err(|e| e.to_string())?;
                counters::bump(&counters::DECRYPTED);
                done += 1;
            }
            Err(_) => locked_n += 1,
        }
    }
    tracing::warn!(considered = files.len(), decrypted = done, locked = locked_n, measured = true,
        n_considered = files.len(), verdict = "at_rest_decrypted_by_owner",
        "connector grants rewritten as plain files at the owner's request");
    Ok((files.len(), done, locked_n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::secure_store;

    fn grant() -> Value {
        json!({"token": "fixture-access", "refresh_token": "fixture-refresh-SECRET", "client_id": "cid"})
    }

    fn contains(hay: &[u8], needle: &str) -> bool {
        hay.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    #[test]
    fn grant_paths_are_recognised_and_registry_files_are_not() {
        let h = Path::new("/h");
        assert_eq!(grant_home(&h.join("connectors/google/a@b.c.json")).as_deref(), Some(h));
        assert_eq!(grant_home(&h.join(GMAIL_DIR).join("a@b.c.json")).as_deref(), Some(h));
        assert!(grant_home(&h.join("connectors/canary.json")).is_none());
        assert!(grant_home(&h.join("connectors/google/.a@b.c.json.lock")).is_none());
        assert!(grant_home(&h.join("notes/google/a.json")).is_none());
    }

    #[test]
    fn a_written_grant_is_sealed_and_reads_back_plain() {
        let home = tempfile::tempdir().unwrap();
        let p = home.path().join("connectors/google/a@b.c.json");
        secure_store::write(&p, grant().to_string().as_bytes()).unwrap();
        let disk = std::fs::read(&p).unwrap();
        assert!(is_envelope(&disk), "on disk the grant is an envelope");
        assert!(!contains(&disk, "fixture-refresh-SECRET"), "no plaintext refresh token on disk");
        let back: Value = secure_store::read_json(&p).unwrap();
        assert_eq!(back, grant());
    }

    #[test]
    fn a_plain_grant_is_migrated_on_read_and_no_plain_copy_remains() {
        let home = tempfile::tempdir().unwrap();
        let p = home.path().join(GMAIL_DIR).join("a@b.c.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, grant().to_string()).unwrap();
        let read: Value = secure_store::read_json(&p).unwrap();
        assert_eq!(read, grant());
        let disk = std::fs::read(&p).unwrap();
        assert!(is_envelope(&disk), "the plain grant was sealed in place");
        for e in std::fs::read_dir(p.parent().unwrap()).unwrap().flatten() {
            assert!(!contains(&std::fs::read(e.path()).unwrap(), "fixture-refresh-SECRET"),
                "no plain copy left beside it: {}", e.path().display());
        }
    }

    #[test]
    fn the_boot_pass_migrates_every_plain_grant() {
        let home = tempfile::tempdir().unwrap();
        for p in ["connectors/google/x@y.z.json", "connectors/slack/Team.json"] {
            let p = home.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, grant().to_string()).unwrap();
        }
        std::fs::write(home.path().join("connectors/canary.json"), "{}").unwrap();
        let (n, migrated, failed) = migrate_home(home.path());
        assert_eq!((n, migrated, failed), (2, 2, 0));
        assert_eq!(std::fs::read_to_string(home.path().join("connectors/canary.json")).unwrap(), "{}");
        assert_eq!(status(home.path())["stores"]["connector_grants"]["encrypted"], "yes");
    }

    #[test]
    fn a_grant_under_the_wrong_key_is_locked_not_missing() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let rel = "connectors/google/a@b.c.json";
        secure_store::write(&a.path().join(rel), grant().to_string().as_bytes()).unwrap();
        std::fs::create_dir_all(b.path().join("connectors/google")).unwrap();
        std::fs::copy(a.path().join(rel), b.path().join(rel)).unwrap();
        // b has its own key: the copied ciphertext must not open, and must not
        // read as an empty grant (that would say "not connected").
        let err = secure_store::read_json::<Value>(&b.path().join(rel)).unwrap_err();
        assert!(is_locked(&err), "{err}");
        assert_eq!(status(b.path())["stores"]["connector_grants"]["locked"], 1);
    }

    #[test]
    fn a_ciphertext_moved_to_another_account_does_not_open() {
        let home = tempfile::tempdir().unwrap();
        let a = home.path().join("connectors/google/a@b.c.json");
        let b = home.path().join("connectors/google/other@b.c.json");
        secure_store::write(&a, grant().to_string().as_bytes()).unwrap();
        std::fs::copy(&a, &b).unwrap();
        assert!(is_locked(&secure_store::read_json::<Value>(&b).unwrap_err()));
    }

    #[test]
    fn the_env_provider_needs_its_key_and_a_wrong_one_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let k = hex::encode([7u8; 32]);
        let one = purpose_key_with(home.path(), false, Provider::Env, Some(k.clone())).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert_eq!(purpose_key_with(home.path(), false, Provider::Env, Some(b64)).unwrap(), one);
        assert!(purpose_key_with(home.path(), false, Provider::Env, None).is_err());
        assert!(purpose_key_with(home.path(), false, Provider::Env, Some("short".into())).is_err());
        assert!(purpose_key_with(home.path(), false, Provider::Kms, None).is_err(), "kms is a stub that refuses");
        // The env key never lands in the home.
        assert!(!crate::api::vault_secrets::key_file(home.path()).exists());
    }

    #[test]
    fn decrypt_all_refuses_while_sealing_is_on() {
        let home = tempfile::tempdir().unwrap();
        assert!(decrypt_all(home.path()).is_err());
    }
}
