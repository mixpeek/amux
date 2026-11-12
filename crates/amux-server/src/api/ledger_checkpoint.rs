//! Signed ledger checkpoints (governance PR 10; plan 4.4 and decision 2:
//! hash chain plus write-once checkpoints; Vision A4, A5, A7, A11).
//!
//! The ledger's hash chain (`ledger.rs`) shows a changed or removed row. It
//! cannot show a rewrite by someone holding the database file: they can change
//! a row and re-hash every row after it, and the chain verifies again. A
//! checkpoint closes that. Every interval the head of the chain, `(seq, hash)`,
//! is signed with an Ed25519 key that lives outside the database and exported:
//!
//! - always to `<home>/ledger-checkpoints.jsonl`, appended;
//! - to S3-compatible object storage (R2, S3, MinIO) when
//!   `AMUX_LEDGER_CHECKPOINT_S3_*` is set, with object-lock headers when
//!   `AMUX_LEDGER_CHECKPOINT_S3_LOCK_DAYS` is set. Whether the object really is
//!   write-once is MEASURED after the write (GET `?retention`), never assumed:
//!   `worm` is `true`, `false` or `unknown`;
//! - to a webhook (`AMUX_LEDGER_CHECKPOINT_WEBHOOK`) when set.
//!
//! `GET /api/ledger/verify` then checks every checkpoint: its fields hash to
//! its `checkpoint_hash`, its signature verifies under this host's key, it
//! links to the checkpoint before it, and the ledger still holds the hash it
//! signed at its seq. A re-hashed rewrite fails the last check. With an
//! external sink it also compares the newest copy there with the newest here,
//! so deleting local checkpoints shows too.
//!
//! What this does not do, stated so nobody reads more into it: someone holding
//! both the database AND the signing key (the same host, same user) can re-sign
//! a rewrite. Only a copy outside the host (the S3 sink with object lock, or a
//! webhook to a system they don't control) catches that, which is why `worm`
//! is reported and not assumed.
//!
//! A11: `AMUX_LEDGER_CHECKPOINTS=off` stops the job; a sink failure never
//! touches a request and is counted (`ledger_checkpoint_sink_failed`).

use super::AppState;
use crate::db::WriteOutcome;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const ENABLED_ENV: &str = "AMUX_LEDGER_CHECKPOINTS";
pub const INTERVAL_ENV: &str = "AMUX_LEDGER_CHECKPOINT_INTERVAL_S";
pub const DEFAULT_INTERVAL_S: u64 = 3600;
pub const WEBHOOK_ENV: &str = "AMUX_LEDGER_CHECKPOINT_WEBHOOK";
const S3_ENDPOINT_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_ENDPOINT";
const S3_BUCKET_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_BUCKET";
const S3_KEY_ID_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_ACCESS_KEY_ID";
const S3_SECRET_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_SECRET_ACCESS_KEY";
const S3_REGION_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_REGION";
const S3_PREFIX_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_PREFIX";
const S3_LOCK_DAYS_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_LOCK_DAYS";
const S3_LOCK_MODE_ENV: &str = "AMUX_LEDGER_CHECKPOINT_S3_LOCK_MODE";

/// The signed message's first line. Changing the format means a new version
/// here and in `amux ledger verify`; old checkpoints keep verifying as v1.
pub const MESSAGE_FORMAT: &str = "amux-ledger-checkpoint-v1";
const KEYCHAIN_SERVICE: &str = "amux-ledger-signing";
const JSONL: &str = "ledger-checkpoints.jsonl";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ledger/checkpoints", get(list_route).post(create_route))
        .route("/api/ledger/checkpoints/key", get(key_route))
        .route("/api/ledger/at/{seq}", get(at_route))
}

/// `AMUX_LEDGER_CHECKPOINTS` is on unless it says off (and the ledger is on).
pub fn enabled() -> bool {
    let own = !matches!(
        std::env::var(ENABLED_ENV).ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    );
    own && super::ledger::enabled()
}

pub fn interval_s() -> u64 {
    std::env::var(INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_INTERVAL_S)
}

pub(crate) mod counters {
    use super::{AtomicU64, Ordering};
    pub static CREATED: AtomicU64 = AtomicU64::new(0);
    pub static SKIPPED_NO_NEW_ROWS: AtomicU64 = AtomicU64::new(0);
    pub static CREATE_FAILED: AtomicU64 = AtomicU64::new(0);
    pub static SINK_OK: AtomicU64 = AtomicU64::new(0);
    pub static SINK_FAILED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn ledger_dir(home: &Path) -> PathBuf {
    home.join("ledger")
}

// ── Key ────────────────────────────────────────────────────────────────────

fn key_file(home: &Path) -> PathBuf {
    ledger_dir(home).join("signing.key")
}

fn read_key_file(home: &Path) -> Result<Option<[u8; 32]>, String> {
    match std::fs::read_to_string(key_file(home)) {
        Ok(s) => hex::decode(s.trim())
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
            .map(Some)
            .ok_or_else(|| "ledger/signing.key is not a 32-byte hex seed".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("could not read ledger/signing.key: {e}")),
    }
}

fn write_key_file(home: &Path, seed: &[u8; 32]) -> Result<(), String> {
    let dir = ledger_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".signing.key.{}", std::process::id()));
    std::fs::write(&tmp, hex::encode(seed)).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, key_file(home)).map_err(|e| e.to_string())
}

fn seed_cache() -> &'static std::sync::Mutex<BTreeMap<PathBuf, [u8; 32]>> {
    static C: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, [u8; 32]>>> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Where the signing key lives: the vault's keystore choice (Keychain on a
/// Mac, a 0600 file elsewhere; `AMUX_VAULT_KEYSTORE` overrides).
pub fn keystore() -> &'static str {
    if super::vault_secrets::keystore_is_file() { "file" } else { "keychain" }
}

/// This host's signing seed. `create` mints one when there is none; a read
/// (verify) never creates a key.
fn seed(home: &Path, create: bool) -> Result<Option<[u8; 32]>, String> {
    if let Some(s) = seed_cache().lock().ok().and_then(|c| c.get(home).copied()) {
        return Ok(Some(s));
    }
    let file = super::vault_secrets::keystore_is_file();
    let read = |h: &Path| if file { read_key_file(h) } else { super::vault_secrets::read_keychain_item(KEYCHAIN_SERVICE, h) };
    let s = match read(home)? {
        Some(s) => s,
        None if create => {
            let mut s = [0u8; 32];
            getrandom::fill(&mut s).map_err(|e| format!("OS random source: {e}"))?;
            if file {
                write_key_file(home, &s)?
            } else {
                super::vault_secrets::write_keychain_item(KEYCHAIN_SERVICE, "amux ledger checkpoint signing key", home, &s)?
            }
            if read(home)? != Some(s) {
                return Err("the new ledger signing key did not read back from the keystore".into());
            }
            tracing::info!(keystore = keystore(), verdict = "ledger_signing_key_created", measured = true,
                n_considered = 1, "ledger checkpoints: signing key created");
            s
        }
        None => return Ok(None),
    };
    if let Ok(mut c) = seed_cache().lock() {
        c.insert(home.to_path_buf(), s);
    }
    Ok(Some(s))
}

fn keypair(seed: &[u8; 32]) -> Result<ring::signature::Ed25519KeyPair, String> {
    ring::signature::Ed25519KeyPair::from_seed_unchecked(seed).map_err(|e| format!("ed25519 key: {e}"))
}

/// This host's public key, hex. `None` when no key exists yet.
pub fn public_key(home: &Path, create: bool) -> Result<Option<String>, String> {
    use ring::signature::KeyPair;
    Ok(match seed(home, create)? {
        Some(s) => Some(hex::encode(keypair(&s)?.public_key().as_ref())),
        None => None,
    })
}

/// The public key as an SPKI PEM, which `openssl pkeyutl -verify` reads.
pub fn public_key_pem(pk_hex: &str) -> Option<String> {
    use base64::Engine;
    let pk = hex::decode(pk_hex).ok().filter(|b| b.len() == 32)?;
    let mut der = hex::decode("302a300506032b6570032100").ok()?;
    der.extend_from_slice(&pk);
    Some(format!(
        "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
        base64::engine::general_purpose::STANDARD.encode(der)
    ))
}

/// This host's stable id (`<home>/ledger/host-id`, 16 hex), minted once. The
/// one source for "which amux server": checkpoints sign it and agent commits
/// carry it as `Amux-Host` (agent_commit.rs).
pub(crate) fn host_id(home: &Path) -> String {
    let p = ledger_dir(home).join("host-id");
    if let Ok(s) = std::fs::read_to_string(&p) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    let id = hex::encode(b);
    let _ = std::fs::create_dir_all(ledger_dir(home));
    let _ = std::fs::write(&p, &id);
    id
}

// ── Checkpoint ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub seq: i64,
    pub head_hash: String,
    pub ts: i64,
    pub n_rows: i64,
    pub prev_checkpoint_hash: String,
    pub host_id: String,
    pub public_key: String,
    pub signature: String,
    pub checkpoint_hash: String,
}

impl Checkpoint {
    /// The exact bytes that are signed and hashed. `amux ledger verify`
    /// rebuilds the same string.
    pub fn message(&self) -> String {
        format!(
            "{MESSAGE_FORMAT}\nhost_id={}\nseq={}\nhead_hash={}\nts={}\nn_rows={}\nprev_checkpoint_hash={}\npublic_key={}",
            self.host_id, self.seq, self.head_hash, self.ts, self.n_rows, self.prev_checkpoint_hash, self.public_key
        )
    }

    pub fn signature_ok(&self) -> bool {
        let (Ok(pk), Ok(sig)) = (hex::decode(&self.public_key), hex::decode(&self.signature)) else {
            return false;
        };
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pk)
            .verify(self.message().as_bytes(), &sig)
            .is_ok()
    }

    /// What every sink receives, one object per checkpoint.
    pub fn export(&self) -> Value {
        json!({
            "format": MESSAGE_FORMAT,
            "algorithm": "ed25519",
            "seq": self.seq,
            "head_hash": self.head_hash,
            "ts": self.ts,
            "n_rows": self.n_rows,
            "prev_checkpoint_hash": self.prev_checkpoint_hash,
            "host_id": self.host_id,
            "public_key": self.public_key,
            "signature": self.signature,
            "checkpoint_hash": self.checkpoint_hash,
        })
    }

    pub fn from_export(v: &Value) -> Option<Checkpoint> {
        Some(Checkpoint {
            seq: v.get("seq")?.as_i64()?,
            head_hash: v.get("head_hash")?.as_str()?.to_string(),
            ts: v.get("ts")?.as_i64()?,
            n_rows: v.get("n_rows")?.as_i64()?,
            prev_checkpoint_hash: v.get("prev_checkpoint_hash")?.as_str()?.to_string(),
            host_id: v.get("host_id")?.as_str()?.to_string(),
            public_key: v.get("public_key")?.as_str()?.to_string(),
            signature: v.get("signature")?.as_str()?.to_string(),
            checkpoint_hash: v.get("checkpoint_hash")?.as_str()?.to_string(),
        })
    }

    /// Object key in the S3 sink: sorts by seq, so the newest is last.
    pub fn object_key(&self, prefix: &str) -> String {
        format!("{}/{}/{:012}-{}.json", prefix.trim_matches('/'), self.host_id, self.seq,
            &self.checkpoint_hash[..16.min(self.checkpoint_hash.len())])
    }
}

const CP_COLS: &str =
    "seq, head_hash, ts, n_rows, prev_checkpoint_hash, host_id, public_key, signature, checkpoint_hash";

fn cp_row(r: &rusqlite::Row) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        seq: r.get(0)?,
        head_hash: r.get(1)?,
        ts: r.get(2)?,
        n_rows: r.get(3)?,
        prev_checkpoint_hash: r.get(4)?,
        host_id: r.get(5)?,
        public_key: r.get(6)?,
        signature: r.get(7)?,
        checkpoint_hash: r.get(8)?,
    })
}

pub fn all(conn: &Connection) -> rusqlite::Result<Vec<Checkpoint>> {
    let mut st = conn.prepare(&format!("SELECT {CP_COLS} FROM _amux_ledger_checkpoints ORDER BY id"))?;
    let v = st.query_map([], cp_row)?.collect();
    v
}

pub fn latest(conn: &Connection) -> rusqlite::Result<Option<Checkpoint>> {
    conn.query_row(&format!("SELECT {CP_COLS} FROM _amux_ledger_checkpoints ORDER BY id DESC LIMIT 1"), [], cp_row)
        .optional()
}

/// Sign the current head. `None` when there is nothing new to sign (no sealed
/// row, or the head has not moved since the last checkpoint and `force` is
/// off). Pure over a connection and a home, for tests.
/// Where a blocking checkpoint write hands its result back to the async caller.
type CheckpointSlot = std::sync::Arc<std::sync::Mutex<Option<Result<Option<Checkpoint>, String>>>>;

pub fn create(conn: &Connection, home: &Path, now_ms: i64, force: bool) -> Result<Option<Checkpoint>, String> {
    super::ledger::seal(conn).map_err(|e| e.to_string())?;
    let head: Option<(i64, String)> = conn
        .query_row("SELECT seq, hash FROM _amux_ledger WHERE hash IS NOT NULL ORDER BY seq DESC LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()
        .map_err(|e| e.to_string())?;
    let (seq, head_hash) = match head {
        Some(h) => h,
        None => {
            let (upto, anchor_hash) = super::ledger::anchor(conn).map_err(|e| e.to_string())?;
            if upto == 0 {
                return Ok(None);
            }
            (upto, anchor_hash)
        }
    };
    let last = latest(conn).map_err(|e| e.to_string())?;
    if !force && last.as_ref().is_some_and(|l| l.seq == seq && l.head_hash == head_hash) {
        counters::bump(&counters::SKIPPED_NO_NEW_ROWS);
        return Ok(None);
    }
    let n_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM _amux_ledger WHERE seq <= ?1", [seq], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let seed = seed(home, true)?.ok_or("no signing key")?;
    let kp = keypair(&seed)?;
    let mut cp = Checkpoint {
        seq,
        head_hash,
        ts: now_ms,
        n_rows,
        prev_checkpoint_hash: last.map(|l| l.checkpoint_hash).unwrap_or_else(|| "genesis".into()),
        host_id: host_id(home),
        public_key: {
            use ring::signature::KeyPair;
            hex::encode(kp.public_key().as_ref())
        },
        signature: String::new(),
        checkpoint_hash: String::new(),
    };
    let msg = cp.message();
    cp.signature = hex::encode(kp.sign(msg.as_bytes()).as_ref());
    cp.checkpoint_hash = sha256_hex(msg.as_bytes());
    conn.execute(
        &format!("INSERT INTO _amux_ledger_checkpoints ({CP_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"),
        rusqlite::params![cp.seq, cp.head_hash, cp.ts, cp.n_rows, cp.prev_checkpoint_hash, cp.host_id,
            cp.public_key, cp.signature, cp.checkpoint_hash],
    )
    .map_err(|e| e.to_string())?;
    counters::bump(&counters::CREATED);
    tracing::info!(verdict = "ledger_checkpoint_created", seq = cp.seq, checkpoint = %cp.checkpoint_hash,
        measured = true, n_considered = 1, "ledger head signed");
    Ok(Some(cp))
}

// ── Verify ─────────────────────────────────────────────────────────────────

fn bad(cp: &Checkpoint, reason: &str) -> Value {
    json!({"checkpoint_seq": cp.seq, "checkpoint_hash": cp.checkpoint_hash, "ts": cp.ts, "reason": reason})
}

/// Check every stored checkpoint against itself, this host's key, the one
/// before it and the chain. `captured` is the chain's hash at each checkpoint
/// seq from the same walk as `chain` (ledger::verify_capturing); `walked` is
/// the last seq that walk verified.
pub fn check(
    cps: &[Checkpoint],
    pinned_key: Option<&str>,
    captured: &BTreeMap<i64, String>,
    walked: i64,
    anchor: &(i64, String),
) -> Value {
    let mut first_bad: Option<Value> = None;
    let (mut ok_n, mut below_anchor, mut not_reached) = (0u64, 0u64, 0u64);
    let mut prev = "genesis".to_string();
    let mut prev_seq = 0i64;
    let first_key = cps.first().map(|c| c.public_key.clone());
    for cp in cps {
        let reason: Option<&str> = if sha256_hex(cp.message().as_bytes()) != cp.checkpoint_hash {
            Some("the checkpoint's fields no longer hash to its checkpoint_hash (it was edited)")
        } else if pinned_key.is_some_and(|k| k != cp.public_key)
            || (pinned_key.is_none() && first_key.as_deref() != Some(cp.public_key.as_str()))
        {
            Some("signed by a different key than this host's ledger signing key")
        } else if !cp.signature_ok() {
            Some("the signature does not verify")
        } else if cp.prev_checkpoint_hash != prev {
            Some("does not link to the checkpoint before it (one was removed, added or reordered)")
        } else if cp.seq < prev_seq {
            Some("its seq is lower than the checkpoint before it")
        } else if cp.seq > anchor.0 {
            match captured.get(&cp.seq) {
                Some(h) if *h == cp.head_hash => None,
                Some(_) => Some("the ledger no longer holds the hash this checkpoint signed at its seq (rows up to it were rewritten)"),
                None if cp.seq > walked => {
                    not_reached += 1;
                    None
                }
                None => Some("the ledger row this checkpoint signed is gone"),
            }
        } else if cp.seq == anchor.0 {
            (anchor.1 != cp.head_hash).then_some("the prune anchor does not hold the hash this checkpoint signed")
        } else {
            below_anchor += 1;
            None
        };
        if let Some(r) = reason {
            first_bad = Some(bad(cp, r));
            break;
        }
        ok_n += 1;
        prev = cp.checkpoint_hash.clone();
        prev_seq = cp.seq;
    }
    if first_bad.is_none() {
        if let Some(last) = cps.last() {
            if last.seq < anchor.0 {
                first_bad = Some(bad(last, "the prune anchor moved past the newest checkpoint (rows it covered are gone)"));
            }
        }
    }
    json!({
        "ok": first_bad.is_none(),
        "first_bad": first_bad,
        "verified": ok_n,
        "below_prune_anchor": below_anchor,
        "not_reached_by_chain_walk": not_reached,
        "latest": cps.last().map(|c| json!({"seq": c.seq, "ts": c.ts, "checkpoint_hash": c.checkpoint_hash})),
        "pinned_key": pinned_key,
        "measured": true,
        "n_considered": cps.len(),
    })
}

/// Compare the local append-only export file with the database's checkpoints.
/// The file is written in the same order, so its lines must be a prefix of the
/// stored checkpoints (it may trail by the ones not yet exported).
pub fn check_jsonl(home: &Path, cps: &[Checkpoint]) -> Value {
    let path = home.join(JSONL);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({"ok": cps.is_empty(), "present": false, "measured": true, "n_considered": 0,
                "reason": if cps.is_empty() { Value::Null } else { json!("checkpoints exist but the export file is missing") }});
        }
        Err(e) => return json!({"ok": false, "measured": false, "n_considered": 0, "why_unmeasured": e.to_string()}),
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut first_bad: Option<Value> = None;
    for (i, l) in lines.iter().enumerate() {
        let parsed = serde_json::from_str::<Value>(l).ok().and_then(|v| Checkpoint::from_export(&v));
        let reason = match (parsed, cps.get(i)) {
            (None, _) => Some("not a checkpoint"),
            (Some(f), _) if !f.signature_ok() => Some("signature does not verify"),
            (Some(_), None) => Some("the file has a checkpoint the database does not (checkpoints were removed from the database)"),
            (Some(f), Some(d)) if f != *d => Some("differs from the database's checkpoint at the same position"),
            _ => None,
        };
        if let Some(r) = reason {
            first_bad = Some(json!({"line": i + 1, "reason": r}));
            break;
        }
    }
    json!({
        "ok": first_bad.is_none(),
        "present": true,
        "path": path.to_string_lossy(),
        "first_bad": first_bad,
        "lines": lines.len(),
        "behind_by": cps.len().saturating_sub(lines.len()),
        "measured": true,
        "n_considered": lines.len(),
    })
}

/// The full verify: the chain walk plus the checkpoints and the local export.
pub fn verify_all(conn: &Connection, home: &Path) -> Value {
    let cps = match all(conn) {
        Ok(c) => c,
        Err(e) => {
            let mut v = super::ledger::verify(conn);
            v["checkpoints"] = json!({"ok": false, "measured": false, "n_considered": 0, "why_unmeasured": e.to_string()});
            v["ok"] = json!(false);
            return v;
        }
    };
    let want: BTreeSet<i64> = cps.iter().map(|c| c.seq).collect();
    let (mut v, captured, walked) = super::ledger::verify_capturing(conn, &want);
    let anchor = super::ledger::anchor(conn).unwrap_or((0, "genesis".into()));
    let pinned = public_key(home, false).ok().flatten();
    let c = check(&cps, pinned.as_deref(), &captured, walked, &anchor);
    let j = check_jsonl(home, &cps);
    let bad = c["ok"] != json!(true) || j["ok"] != json!(true);
    if bad {
        v["ok"] = json!(false);
        if v["first_bad"].is_null() {
            v["first_bad"] = c["first_bad"].clone();
            if v["first_bad"].is_null() {
                v["first_bad"] = json!({"reason": "the local checkpoint export does not match", "export": j["first_bad"].clone()});
            }
            v["first_bad_event"] = json!(null);
        }
    }
    v["checkpoints"] = c;
    v["checkpoints"]["export_file"] = j;
    v["checkpoints"]["enabled"] = json!(enabled());
    v
}

/// Compare the S3 sink's copy with the local newest checkpoint (network, so
/// outside the DB read). Leaves `external.checked=false` when no sink is set.
pub async fn verify_external(v: &mut Value) {
    let Some(s3) = S3::from_env() else {
        v["checkpoints"]["external"] = json!({"configured": false, "checked": false, "measured": true, "n_considered": 0});
        return;
    };
    let Some(latest) = v["checkpoints"]["latest"]["checkpoint_hash"].as_str().map(str::to_string) else {
        v["checkpoints"]["external"] = json!({"configured": true, "checked": false, "measured": true, "n_considered": 0,
            "reason": "no local checkpoint yet"});
        return;
    };
    let home = crate::config::amux_home();
    let Some(local) = ({
        let latest = latest.clone();
        tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open_with_flags(home.join("amux.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
            conn.query_row(&format!("SELECT {CP_COLS} FROM _amux_ledger_checkpoints WHERE checkpoint_hash=?1"), [latest], cp_row).ok()
        })
        .await
        .ok()
        .flatten()
    }) else {
        v["checkpoints"]["external"] = json!({"configured": true, "checked": false, "measured": false, "n_considered": 0,
            "why_unmeasured": "could not reread the newest checkpoint"});
        return;
    };
    let ext = s3.compare(&local).await;
    if ext["ok"] == json!(false) {
        v["ok"] = json!(false);
        if v["first_bad"].is_null() {
            v["first_bad"] = json!({"reason": "the external checkpoint sink disagrees with this host", "external": ext.clone()});
        }
    }
    v["checkpoints"]["external"] = ext;
}

// ── Sinks ──────────────────────────────────────────────────────────────────

fn record_export(conn: &Connection, cp: &str, sink: &str, ok: bool, worm: &str, location: &str, detail: &str) {
    let _ = conn.execute(
        "INSERT OR REPLACE INTO _amux_ledger_checkpoint_exports (checkpoint_hash, sink, ok, worm, location, detail, ts)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![cp, sink, ok as i64, worm, location, detail, chrono::Utc::now().timestamp_millis()],
    );
}

fn sink_result(sink: &str, ok: bool, detail: &str) {
    if ok {
        counters::bump(&counters::SINK_OK);
    } else {
        counters::bump(&counters::SINK_FAILED);
        tracing::warn!(verdict = "ledger_checkpoint_sink_failed", sink, detail, measured = true, n_considered = 1,
            "a ledger checkpoint was not exported; the next tick retries");
    }
}

/// Append every checkpoint the export file does not have yet, in order.
pub fn export_jsonl(home: &Path, cps: &[Checkpoint]) -> Result<usize, String> {
    use std::io::Write;
    let path = home.join(JSONL);
    let have = std::fs::read_to_string(&path).map(|t| t.lines().filter(|l| !l.trim().is_empty()).count()).unwrap_or(0);
    if have >= cps.len() {
        return Ok(0);
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    let mut n = 0;
    for cp in &cps[have..] {
        writeln!(f, "{}", cp.export()).map_err(|e| e.to_string())?;
        n += 1;
    }
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(n)
}

pub struct S3 {
    endpoint: String,
    bucket: String,
    key_id: String,
    secret: String,
    region: String,
    pub prefix: String,
    lock_days: Option<u32>,
    lock_mode: String,
}

type HmacSha256 = hmac::Hmac<Sha256>;

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    use hmac::Mac;
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding as SigV4 wants it; `/` is kept in paths, encoded in query values.
fn uri_enc(s: &str, keep_slash: bool) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) || (keep_slash && c == '/') {
            o.push(c);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl S3 {
    /// Configured only when endpoint, bucket and both key halves are set.
    /// Values are read by name from the environment and never logged.
    pub fn from_env() -> Option<S3> {
        Some(S3 {
            endpoint: env_nonempty(S3_ENDPOINT_ENV)?.trim_end_matches('/').to_string(),
            bucket: env_nonempty(S3_BUCKET_ENV)?,
            key_id: env_nonempty(S3_KEY_ID_ENV)?,
            secret: env_nonempty(S3_SECRET_ENV)?,
            region: env_nonempty(S3_REGION_ENV).unwrap_or_else(|| "auto".into()),
            prefix: env_nonempty(S3_PREFIX_ENV).unwrap_or_else(|| "amux-ledger-checkpoints".into()),
            lock_days: env_nonempty(S3_LOCK_DAYS_ENV).and_then(|v| v.parse().ok()).filter(|d| *d > 0),
            lock_mode: env_nonempty(S3_LOCK_MODE_ENV).unwrap_or_else(|| "COMPLIANCE".into()).to_ascii_uppercase(),
        })
    }

    fn host(&self) -> String {
        reqwest::Url::parse(&self.endpoint)
            .ok()
            .and_then(|u| u.host_str().map(|h| match u.port() { Some(p) => format!("{h}:{p}"), None => h.to_string() }))
            .unwrap_or_default()
    }

    /// Path-style: `<endpoint>/<bucket>/<key>`. Returns the headers to send,
    /// Authorization included.
    fn sign(&self, method: &str, uri: &str, query: &str, extra: &[(String, String)], payload_hash: &str)
        -> Vec<(String, String)> {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let mut h: Vec<(String, String)> = vec![
            ("host".into(), self.host()),
            ("x-amz-content-sha256".into(), payload_hash.into()),
            ("x-amz-date".into(), amz_date.clone()),
        ];
        h.extend(extra.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())));
        h.sort_by(|a, b| a.0.cmp(&b.0));
        let canon_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
        let signed: String = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
        let creq = format!("{method}\n{uri}\n{query}\n{canon_headers}\n{signed}\n{payload_hash}");
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let sts = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(creq.as_bytes()));
        let k = hmac(format!("AWS4{}", self.secret).as_bytes(), &date);
        let k = hmac(&k, &self.region);
        let k = hmac(&k, "s3");
        let k = hmac(&k, "aws4_request");
        let sig = hex::encode(hmac(&k, &sts));
        h.push(("authorization".into(), format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", self.key_id)));
        h.retain(|(k, _)| k != "host");
        h
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().unwrap_or_default()
    }

    async fn send(&self, method: reqwest::Method, key: Option<&str>, query: &[(&str, &str)], body: Vec<u8>,
        extra: Vec<(String, String)>) -> Result<(u16, String), String> {
        let uri = match key {
            Some(k) => format!("/{}/{}", uri_enc(&self.bucket, false), uri_enc(k, true)),
            None => format!("/{}", uri_enc(&self.bucket, false)),
        };
        let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_enc(k, false), uri_enc(v, false))).collect();
        q.sort();
        let qs = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let headers = self.sign(method.as_str(), &uri, &qs, &extra, &sha256_hex(&body));
        let url = if qs.is_empty() { format!("{}{uri}", self.endpoint) } else { format!("{}{uri}?{qs}", self.endpoint) };
        let mut rb = Self::client().request(method, url).body(body);
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await.map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// Write one checkpoint, then measure whether it is write-once.
    pub async fn put(&self, cp: &Checkpoint) -> (bool, String, String, String) {
        use base64::Engine;
        let key = cp.object_key(&self.prefix);
        let body = cp.export().to_string().into_bytes();
        let mut extra = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-amz-checksum-sha256".to_string(), base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&body))),
            ("x-amz-sdk-checksum-algorithm".to_string(), "SHA256".to_string()),
        ];
        if let Some(days) = self.lock_days {
            let until = (chrono::Utc::now() + chrono::Duration::days(days as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
            extra.push(("x-amz-object-lock-mode".into(), self.lock_mode.clone()));
            extra.push(("x-amz-object-lock-retain-until-date".into(), until));
        }
        match self.send(reqwest::Method::PUT, Some(&key), &[], body, extra).await {
            Ok((s, _)) if (200..300).contains(&s) => {
                let worm = self.measure_worm(&key).await;
                (true, worm, key, format!("HTTP {s}"))
            }
            Ok((s, t)) => (false, "unknown".into(), key, format!("HTTP {s}: {}", t.chars().take(200).collect::<String>())),
            Err(e) => (false, "unknown".into(), key, e),
        }
    }

    /// `true` only when the store answers with a retention mode for the
    /// object; `false` when it says there is none; `unknown` otherwise.
    async fn measure_worm(&self, key: &str) -> String {
        match self.send(reqwest::Method::GET, Some(key), &[("retention", "")], vec![], vec![]).await {
            Ok((200, t)) if t.contains("<Mode>") => "true".into(),
            Ok((s, t)) if s == 404 || t.contains("NoSuchObjectLockConfiguration") || t.contains("ObjectLockConfigurationNotFound")
                || t.contains("InvalidRequest") || t.contains("NotImplemented") => "false".into(),
            Ok((200, _)) => "false".into(),
            _ => "unknown".into(),
        }
    }

    /// Does the sink hold this host's newest checkpoint, and nothing newer?
    pub async fn compare(&self, local: &Checkpoint) -> Value {
        let key = local.object_key(&self.prefix);
        let got = self.send(reqwest::Method::GET, Some(&key), &[], vec![], vec![]).await;
        let (latest_matches, why) = match got {
            Ok((200, t)) => {
                let remote = serde_json::from_str::<Value>(&t).ok().and_then(|v| Checkpoint::from_export(&v));
                match remote {
                    Some(r) if r == *local && r.signature_ok() => (Some(true), None),
                    Some(_) => (Some(false), Some("the sink's copy of the newest checkpoint differs from this host's")),
                    None => (Some(false), Some("the sink's copy is not a checkpoint")),
                }
            }
            Ok((404, _)) => (Some(false), Some("the sink does not have this host's newest checkpoint")),
            Ok((s, _)) => return json!({"configured": true, "checked": false, "measured": false, "n_considered": 0,
                "why_unmeasured": format!("sink answered HTTP {s}")}),
            Err(e) => return json!({"configured": true, "checked": false, "measured": false, "n_considered": 0,
                "why_unmeasured": e}),
        };
        let host_prefix = format!("{}/{}/", self.prefix.trim_matches('/'), local.host_id);
        let newer: Vec<String> = match self
            .send(reqwest::Method::GET, None, &[("list-type", "2"), ("prefix", &host_prefix), ("start-after", &key), ("max-keys", "10")], vec![], vec![])
            .await
        {
            Ok((200, t)) => t.split("<Key>").skip(1).filter_map(|s| s.split("</Key>").next()).map(str::to_string).collect(),
            _ => vec![],
        };
        let ok = latest_matches == Some(true) && newer.is_empty();
        let reason = why.or(if newer.is_empty() { None } else {
            Some("the sink holds checkpoints newer than this host's newest (checkpoints were removed here)")
        });
        json!({
            "configured": true,
            "checked": true,
            "ok": ok,
            "latest_key": key,
            "latest_matches": latest_matches,
            "newer_in_sink": newer,
            "reason": reason,
            "measured": true,
            "n_considered": 1 + newer.len(),
        })
    }
}

async fn post_webhook(url: &str, cp: &Checkpoint) -> (bool, String) {
    let c = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap_or_default();
    match c.post(url).json(&cp.export()).send().await {
        Ok(r) if r.status().is_success() => (true, format!("HTTP {}", r.status().as_u16())),
        Ok(r) => (false, format!("HTTP {}", r.status().as_u16())),
        Err(e) => (false, format!("request failed: {e}")),
    }
}

/// Export to every configured sink. The newest checkpoint is retried each
/// tick until a sink takes it; an older one a sink missed stays missed there
/// (the next one chains past it, and verify compares the newest).
pub async fn export(state: &AppState, home: &Path) -> Value {
    let home_b = home.to_path_buf();
    let cps = state.store.read_async(|conn| Ok(all(conn)?)).await.unwrap_or_default();
    let mut out = json!({});
    match export_jsonl(&home_b, &cps) {
        Ok(n) => {
            sink_result("file", true, "");
            out["file"] = json!({"ok": true, "appended": n});
        }
        Err(e) => {
            sink_result("file", false, &e);
            out["file"] = json!({"ok": false, "error": e});
        }
    }
    let Some(last) = cps.last().cloned() else { return out };
    let done: BTreeMap<String, bool> = {
        let h = last.checkpoint_hash.clone();
        state
            .store
            .read_async(move |conn| {
                let mut st = conn.prepare("SELECT sink, ok FROM _amux_ledger_checkpoint_exports WHERE checkpoint_hash=?1")?;
                let m = st.query_map([h], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? == 1)))?
                    .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
                Ok(m)
            })
            .await
            .unwrap_or_default()
    };
    let mut results: Vec<(String, bool, String, String, String)> = vec![];
    if let Some(s3) = S3::from_env() {
        if !done.get("s3").copied().unwrap_or(false) {
            let (ok, worm, key, detail) = s3.put(&last).await;
            sink_result("s3", ok, &detail);
            out["s3"] = json!({"ok": ok, "worm": worm, "key": key, "detail": detail});
            results.push(("s3".into(), ok, worm, key, detail));
        }
    }
    if let Some(url) = env_nonempty(WEBHOOK_ENV) {
        if !done.get("webhook").copied().unwrap_or(false) {
            let (ok, detail) = post_webhook(&url, &last).await;
            sink_result("webhook", ok, &detail);
            out["webhook"] = json!({"ok": ok, "detail": detail});
            results.push(("webhook".into(), ok, "unknown".into(), String::new(), detail));
        }
    }
    if !results.is_empty() {
        let h = last.checkpoint_hash.clone();
        let _ = state
            .store
            .write_async(move |conn| {
                for (sink, ok, worm, loc, detail) in &results {
                    record_export(conn, &h, sink, *ok, worm, loc, detail);
                }
                Ok(WriteOutcome { applied: false, events: vec![] })
            })
            .await;
    }
    out
}

/// Sign the head when the interval has passed (or `force`), then export.
pub async fn tick(state: &AppState, force: bool) -> Value {
    if !enabled() {
        return json!({"ok": true, "skipped": "disabled", "enabled": false});
    }
    let home = crate::config::amux_home();
    let now = chrono::Utc::now().timestamp_millis();
    let due_after_ms = (interval_s() as i64) * 1000;
    let slot: CheckpointSlot = Default::default();
    let (s2, h2) = (slot.clone(), home.clone());
    let res = state
        .store
        .write_async(move |conn| {
            let due = force
                || latest(conn)?.map(|l| now - l.ts >= due_after_ms).unwrap_or(true);
            let r = if due { create(conn, &h2, now, force) } else { Ok(None) };
            let applied = matches!(r, Ok(Some(_)));
            *s2.lock().expect("slot") = Some(r);
            Ok(WriteOutcome { applied, events: vec![] })
        })
        .await;
    let outcome: Result<Option<Checkpoint>, String> = match (res, slot.lock().ok().and_then(|mut s| s.take())) {
        (Err(e), _) => Err(e.to_string()),
        (_, Some(r)) => r,
        (_, None) => Ok(None),
    };
    let made = match outcome {
        Err(e) => {
            counters::bump(&counters::CREATE_FAILED);
            tracing::warn!(verdict = "ledger_checkpoint_failed", error = %e, measured = true, n_considered = 1,
                "ledger checkpoint not created; the next tick retries");
            return json!({"ok": false, "error": e});
        }
        Ok(c) => c,
    };
    let exports = export(state, &home).await;
    json!({"ok": true, "created": made.as_ref().map(Checkpoint::export), "exports": exports})
}

// ── Routes ─────────────────────────────────────────────────────────────────

async fn list_route(State(state): State<AppState>) -> Response {
    match state
        .store
        .read_async(|conn| {
            let cps = all(conn)?;
            let mut st = conn.prepare("SELECT checkpoint_hash, sink, ok, worm, location, detail, ts FROM _amux_ledger_checkpoint_exports")?;
            let ex: Vec<Value> = st
                .query_map([], |r| Ok(json!({"checkpoint_hash": r.get::<_, String>(0)?, "sink": r.get::<_, String>(1)?,
                    "ok": r.get::<_, i64>(2)? == 1, "worm": r.get::<_, Option<String>>(3)?,
                    "location": r.get::<_, Option<String>>(4)?, "detail": r.get::<_, Option<String>>(5)?,
                    "ts": r.get::<_, i64>(6)?})))?
                .collect::<rusqlite::Result<_>>()?;
            Ok((cps, ex))
        })
        .await
    {
        Ok((cps, ex)) => Json(json!({
            "checkpoints": cps.iter().rev().take(200).map(Checkpoint::export).collect::<Vec<_>>(),
            "exports": ex,
            "measured": true,
            "n_considered": cps.len(),
            "enabled": enabled(),
            "interval_s": interval_s(),
        }))
        .into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string(), "measured": false, "n_considered": 0}))).into_response(),
    }
}

/// Sign the head now and export it. A person only: a worker or an app has no
/// business moving the checkpoint schedule.
async fn create_route(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let p = super::ledger::principal(&crate::config::amux_home(), &headers);
    if p.actor.starts_with("worker:") || p.actor.starts_with("app:") {
        return (StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": "checkpoint_needs_a_person",
            "error": "only the owner or a member can force a ledger checkpoint; the job signs one every interval on its own"}))).into_response();
    }
    if super::human_proof::check(&headers, "ledger_checkpoint") == super::human_proof::Verdict::Refused {
        return super::human_proof::refusal("ledger_checkpoint");
    }
    Json(tick(&state, true).await).into_response()
}

async fn key_route() -> Response {
    let home = crate::config::amux_home();
    // A read never mints the key: on a Mac that is a Keychain write, which
    // blocked this handler (tests/route_table.rs probes every route). The
    // first checkpoint creates it.
    match public_key(&home, false) {
        Ok(Some(pk)) => Json(json!({
            "algorithm": "ed25519",
            "public_key": pk,
            "public_key_pem": public_key_pem(&pk),
            "host_id": host_id(&home),
            "keystore": keystore(),
            "message_format": MESSAGE_FORMAT,
        }))
        .into_response(),
        Ok(None) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "no signing key yet; the first checkpoint (POST /api/ledger/checkpoints) creates it",
                "keystore": keystore(), "measured": true})),
        )
            .into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e}))).into_response(),
    }
}

/// The hash the ledger holds at one seq, for an auditor comparing an exported
/// checkpoint file against this server (`amux ledger verify --against`).
async fn at_route(State(state): State<AppState>, axum::extract::Path(seq): axum::extract::Path<i64>) -> Response {
    match state
        .store
        .read_async(move |conn| {
            let h: Option<Option<String>> = conn
                .query_row("SELECT hash FROM _amux_ledger WHERE seq=?1", [seq], |r| r.get(0))
                .optional()?;
            let anchor = super::ledger::anchor(conn)?;
            Ok((h, anchor))
        })
        .await
    {
        Ok((Some(h), _)) => Json(json!({"seq": seq, "present": true, "hash": h})).into_response(),
        Ok((None, (upto, ah))) => Json(json!({"seq": seq, "present": false, "pruned": seq <= upto,
            "anchor": {"upto_seq": upto, "hash": ah}})).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// The A5 block for `GET /api/contract/counters`.
pub fn report() -> Value {
    let created = counters::get(&counters::CREATED);
    json!({
        "enabled": enabled(),
        "interval_s": interval_s(),
        "keystore": keystore(),
        "created": created,
        "skipped_no_new_rows": counters::get(&counters::SKIPPED_NO_NEW_ROWS),
        "create_failed": counters::get(&counters::CREATE_FAILED),
        "sink_ok": counters::get(&counters::SINK_OK),
        "ledger_checkpoint_sink_failed": counters::get(&counters::SINK_FAILED),
        "sinks": {
            "file": true,
            "s3": S3::from_env().is_some(),
            "s3_lock_requested": env_nonempty(S3_LOCK_DAYS_ENV).is_some(),
            "webhook": env_nonempty(WEBHOOK_ENV).is_some(),
        },
        "measured": true,
        "n_considered": created + counters::get(&counters::CREATE_FAILED),
        "list": "GET /api/ledger/checkpoints",
        "verify": "GET /api/ledger/verify",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ledger::{self, Event};

    fn db() -> Connection {
        crate::db::migrate::test_memdb()
    }

    fn ev(id: &str, ts: i64) -> Event {
        Event { event_id: id.into(), ts_ms: ts, kind: "interaction".into(), actor: "owner".into(),
            action: Some("POST /api/x".into()), resource: Some("worker:w".into()),
            outcome: Some("applied".into()), ..Default::default() }
    }

    fn file_keystore() {
        // SAFETY of the env write: every test here sets the same value.
        std::env::set_var("AMUX_VAULT_KEYSTORE", "file");
    }

    /// Re-hash every row from `from_seq` on after editing one, the way someone
    /// holding the file would: the chain verifies again.
    fn rewrite_and_rehash(conn: &Connection, seq: i64, new_resource: &str) {
        conn.execute_batch("DROP TRIGGER amux_ledger_no_update; DROP TRIGGER amux_ledger_seal_once;").unwrap();
        conn.execute("UPDATE _amux_ledger SET resource=?2 WHERE seq=?1", rusqlite::params![seq, new_resource]).unwrap();
        conn.execute("UPDATE _amux_ledger SET hash=NULL, prev_hash=NULL WHERE seq>=?1", [seq]).unwrap();
        ledger::seal(conn).unwrap();
    }

    fn verify(conn: &Connection, home: &Path) -> Value {
        verify_all(conn, home)
    }

    #[test]
    fn a_signed_checkpoint_verifies_and_survives_new_rows() {
        file_keystore();
        let home = tempfile::tempdir().unwrap();
        let c = db();
        ledger::append(&c, &ev("a", 1));
        ledger::append(&c, &ev("b", 2));
        let cp = create(&c, home.path(), 10, false).unwrap().expect("signed");
        assert!(cp.signature_ok());
        assert_eq!(cp.prev_checkpoint_hash, "genesis");
        export_jsonl(home.path(), &all(&c).unwrap()).unwrap();
        ledger::append(&c, &ev("c", 3));
        let v = verify(&c, home.path());
        assert_eq!(v["ok"], json!(true), "{v}");
        assert_eq!(v["checkpoints"]["verified"], json!(1));
        // Nothing new: no second checkpoint unless forced.
        let c2 = create(&c, home.path(), 20, false).unwrap().expect("head moved, so a new one");
        assert_eq!(c2.prev_checkpoint_hash, cp.checkpoint_hash);
        assert!(create(&c, home.path(), 30, false).unwrap().is_none());
    }

    #[test]
    fn a_rehashed_rewrite_passes_the_chain_and_fails_the_checkpoint() {
        file_keystore();
        let home = tempfile::tempdir().unwrap();
        let c = db();
        for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
            ledger::append(&c, &ev(id, i as i64));
        }
        create(&c, home.path(), 10, false).unwrap().expect("signed");
        export_jsonl(home.path(), &all(&c).unwrap()).unwrap();
        rewrite_and_rehash(&c, 2, "worker:someone-else");
        let chain = ledger::verify(&c);
        assert_eq!(chain["chain_ok"], json!(true), "the re-hash fools the chain alone: {chain}");
        let v = verify(&c, home.path());
        assert_eq!(v["ok"], json!(false), "{v}");
        let why = v["checkpoints"]["first_bad"]["reason"].as_str().unwrap();
        assert!(why.contains("no longer holds the hash this checkpoint signed"), "{why}");
        assert_eq!(v["checkpoints"]["first_bad"]["checkpoint_seq"], json!(4));
    }

    #[test]
    fn a_forged_signature_or_a_foreign_key_is_refused() {
        let c = Checkpoint { seq: 1, head_hash: "h".into(), ts: 1, n_rows: 1, prev_checkpoint_hash: "genesis".into(),
            host_id: "x".into(), public_key: String::new(), signature: String::new(), checkpoint_hash: String::new() };
        let kp = keypair(&[7u8; 32]).unwrap();
        let other = keypair(&[9u8; 32]).unwrap();
        use ring::signature::KeyPair;
        let mut good = Checkpoint { public_key: hex::encode(kp.public_key().as_ref()), ..c.clone() };
        good.signature = hex::encode(kp.sign(good.message().as_bytes()).as_ref());
        good.checkpoint_hash = sha256_hex(good.message().as_bytes());
        assert!(good.signature_ok());
        let captured: BTreeMap<i64, String> = [(1, "h".to_string())].into();
        let anchor = (0, "genesis".to_string());
        assert_eq!(check(std::slice::from_ref(&good), Some(&good.public_key), &captured, 1, &anchor)["ok"], json!(true));
        // A changed signature byte.
        let mut forged = good.clone();
        let flip = if &forged.signature[0..2] == "00" { "11" } else { "00" };
        forged.signature.replace_range(0..2, flip);
        assert!(!forged.signature_ok());
        let v = check(std::slice::from_ref(&forged), Some(&good.public_key), &captured, 1, &anchor);
        assert_eq!(v["first_bad"]["reason"], json!("the signature does not verify"));
        // Re-signed with another key: the signature is valid, the key is not this host's.
        let mut resigned = Checkpoint { public_key: hex::encode(other.public_key().as_ref()), ..c };
        resigned.signature = hex::encode(other.sign(resigned.message().as_bytes()).as_ref());
        resigned.checkpoint_hash = sha256_hex(resigned.message().as_bytes());
        assert!(resigned.signature_ok());
        let v = check(std::slice::from_ref(&resigned), Some(&good.public_key), &captured, 1, &anchor);
        assert_eq!(v["first_bad"]["reason"], json!("signed by a different key than this host's ledger signing key"));
    }

    #[test]
    fn removing_a_checkpoint_breaks_the_checkpoint_chain() {
        file_keystore();
        let home = tempfile::tempdir().unwrap();
        let c = db();
        ledger::append(&c, &ev("a", 1));
        create(&c, home.path(), 10, false).unwrap().expect("1");
        ledger::append(&c, &ev("b", 2));
        create(&c, home.path(), 20, false).unwrap().expect("2");
        ledger::append(&c, &ev("c", 3));
        create(&c, home.path(), 30, false).unwrap().expect("3");
        let mut cps = all(&c).unwrap();
        let (captured, walked) = {
            let want: BTreeSet<i64> = cps.iter().map(|c| c.seq).collect();
            let (_, cap, w) = ledger::verify_capturing(&c, &want);
            (cap, w)
        };
        let pk = cps[0].public_key.clone();
        let anchor = (0, "genesis".to_string());
        assert_eq!(check(&cps, Some(&pk), &captured, walked, &anchor)["ok"], json!(true));
        cps.remove(1);
        let v = check(&cps, Some(&pk), &captured, walked, &anchor);
        assert_eq!(v["first_bad"]["checkpoint_seq"], json!(3), "{v}");
        assert!(v["first_bad"]["reason"].as_str().unwrap().contains("does not link"));
        // The table itself refuses the delete.
        assert!(c.execute("DELETE FROM _amux_ledger_checkpoints WHERE seq=2", []).is_err());
        assert!(c.execute("UPDATE _amux_ledger_checkpoints SET head_hash='x'", []).is_err());
    }

    #[test]
    fn the_export_file_must_match_the_database() {
        file_keystore();
        let home = tempfile::tempdir().unwrap();
        let c = db();
        ledger::append(&c, &ev("a", 1));
        create(&c, home.path(), 10, false).unwrap().expect("1");
        ledger::append(&c, &ev("b", 2));
        create(&c, home.path(), 20, false).unwrap().expect("2");
        let cps = all(&c).unwrap();
        assert_eq!(export_jsonl(home.path(), &cps).unwrap(), 2);
        assert_eq!(export_jsonl(home.path(), &cps).unwrap(), 0, "already exported");
        assert_eq!(check_jsonl(home.path(), &cps)["ok"], json!(true));
        // The database lost its newest checkpoint; the file still has it.
        let v = check_jsonl(home.path(), &cps[..1]);
        assert_eq!(v["ok"], json!(false));
        assert!(v["first_bad"]["reason"].as_str().unwrap().contains("the database does not"));
    }

    #[test]
    fn the_public_key_pem_is_the_ed25519_spki_openssl_reads() {
        let pem = public_key_pem(&"ab".repeat(32)).unwrap();
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA"), "{pem}");
        assert!(public_key_pem("abcd").is_none());
    }

    #[test]
    fn sigv4_matches_the_aws_documented_example_shape() {
        // A fixed-input check that the signing chain is deterministic and
        // scoped to the configured region.
        let s3 = S3 { endpoint: "https://example.r2.cloudflarestorage.com".into(), bucket: "b".into(),
            key_id: "AKID".into(), secret: "SECRET".into(), region: "auto".into(), prefix: "p".into(),
            lock_days: None, lock_mode: "COMPLIANCE".into() };
        let h = s3.sign("GET", "/b/k", "", &[], &sha256_hex(b""));
        let auth = h.iter().find(|(k, _)| k == "authorization").unwrap().1.clone();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"), "{auth}");
        assert!(auth.contains("/auto/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="), "{auth}");
        assert_eq!(uri_enc("a/b c", true), "a/b%20c");
        assert_eq!(uri_enc("a/b", false), "a%2Fb");
    }
}
