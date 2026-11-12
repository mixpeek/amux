//! A second factor (governance PR 20; plan 4.1 and phase 4): TOTP (RFC 6238)
//! for members and the owner, recovery codes, a per-session "this browser
//! proved a second factor" mark, and step-up for the owner's governance powers.
//!
//! | piece | where |
//! |---|---|
//! | enroll, confirm, verify, recovery codes | `/api/mfa/*`, and the page at `/mfa` |
//! | "this session proved a factor" | `org_invites.mfa_at` on the session row the member cookie names |
//! | an SSO sign-in whose `amr` says it already did | set at sign-in (sso.rs) |
//! | step-up | `POST /api/mfa/step-up {code}` returns a five-minute token, sent back as `X-Amux-Step-Up` on the kill switch, a hold, an erasure or a capability change |
//!
//! The secret is sealed with the at-rest key (integrations/at_rest.rs), bound
//! to its subject; recovery codes are stored as SHA-256 only. A code is good
//! once: the step it matched is kept and an older or equal step is refused.
//!
//! The owner's policy is `AMUX_MFA_POLICY`:
//! - `off`: nobody enrolls and nothing is checked;
//! - `optional` (default): anyone may enroll; a member who has must prove it
//!   once per session;
//! - `required`: every member session must have proved a factor.
//!
//! What a session that has not proved one costs is `AMUX_MFA_ENFORCE` (A11):
//! `report` (default) counts and lets it through, `enforce` sends a browser to
//! `/mfa` and refuses an API call 403 `mfa_required`, `off` checks nothing.
//! Step-up has its own switch, `AMUX_MFA_STEP_UP`, the same three values. The
//! owner's bearer is never locked out by the session check; step-up applies
//! to the owner because those powers are the owner's.

use super::AppState;
use crate::db::WriteOutcome;
use axum::{
    extract::State,
    http::{header, HeaderMap, Method, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

pub use super::worker_identity::Mode;

pub const POLICY_ENV: &str = "AMUX_MFA_POLICY";
pub const ENFORCE_ENV: &str = "AMUX_MFA_ENFORCE";
pub const STEP_UP_ENV: &str = "AMUX_MFA_STEP_UP";
pub const STEP_UP_HEADER: &str = "x-amux-step-up";
const STEP_SECS: i64 = 30;
const DIGITS: u32 = 6;
/// One step either side: a phone clock 30 s off still works.
const SKEW: i64 = 1;
const STEP_UP_TTL_SECS: i64 = 300;
const RECOVERY_CODES: usize = 10;
const ISSUER: &str = "amux";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Off,
    Optional,
    Required,
}

impl Policy {
    pub fn parse(v: Option<&str>) -> Policy {
        match v.map(|v| v.trim().trim_matches('"').to_ascii_lowercase()).as_deref() {
            Some("off") | Some("0") | Some("false") => Policy::Off,
            Some("required") | Some("require") => Policy::Required,
            _ => Policy::Optional,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Optional => "optional",
            Policy::Required => "required",
        }
    }
}

pub fn policy() -> Policy {
    Policy::parse(std::env::var(POLICY_ENV).ok().as_deref())
}
pub fn enforce_mode() -> Mode {
    Mode::parse(std::env::var(ENFORCE_ENV).ok().as_deref())
}
pub fn step_up_mode() -> Mode {
    Mode::parse(std::env::var(STEP_UP_ENV).ok().as_deref())
}

pub(crate) mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static ENROLLED: AtomicU64 = AtomicU64::new(0);
    pub static VERIFIED: AtomicU64 = AtomicU64::new(0);
    pub static FAILED: AtomicU64 = AtomicU64::new(0);
    pub static REPLAY_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static RECOVERY_USED: AtomicU64 = AtomicU64::new(0);
    pub static RESET: AtomicU64 = AtomicU64::new(0);
    pub static STEP_UP_ISSUED: AtomicU64 = AtomicU64::new(0);
    pub static STEP_UP_PASSED: AtomicU64 = AtomicU64::new(0);
    pub static STEP_UP_WOULD_REFUSE: AtomicU64 = AtomicU64::new(0);
    pub static STEP_UP_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static SESSION_CONSIDERED: AtomicU64 = AtomicU64::new(0);
    pub static SESSION_WOULD_REFUSE: AtomicU64 = AtomicU64::new(0);
    pub static SESSION_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Bytes from the OS random source. A secret is never made without one.
pub(crate) fn random_bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).map_err(|e| format!("the OS random source failed: {e}"))?;
    Ok(b)
}

fn rng_sql(e: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(e.into())
}

pub(crate) fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

// ── TOTP (RFC 4226 / 6238) ──────────────────────────────────────────────────

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let v = B32.iter().position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// HOTP over HMAC-SHA1, as every authenticator app computes it.
pub fn hotp(secret: &[u8], counter: u64) -> u32 {
    // HMAC takes a key of any length; u32::MAX matches no six-digit code.
    let Ok(mut mac) = <Hmac<sha1::Sha1> as Mac>::new_from_slice(secret) else {
        return u32::MAX;
    };
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[h.len() - 1] & 0x0f) as usize;
    let bin = ((h[off] as u32 & 0x7f) << 24) | ((h[off + 1] as u32) << 16) | ((h[off + 2] as u32) << 8) | h[off + 3] as u32;
    bin % 10u32.pow(DIGITS)
}

pub fn totp_at(secret: &[u8], unix_secs: i64) -> u32 {
    hotp(secret, (unix_secs / STEP_SECS) as u64)
}

/// The step `code` matches within one step of `now`, if it is newer than
/// `last_step` (a code is good once).
pub fn match_step(secret: &[u8], code: &str, now: i64, last_step: i64) -> Option<i64> {
    let code = code.trim().replace(' ', "");
    if code.len() != DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let want: u32 = code.parse().ok()?;
    let cur = now / STEP_SECS;
    (-SKEW..=SKEW)
        .map(|d| cur + d)
        .find(|&step| step >= 0 && constant_eq(hotp(secret, step as u64), want))
        .filter(|&step| step > last_step)
}

fn constant_eq(a: u32, b: u32) -> bool {
    (a ^ b) == 0
}

pub fn otpauth_uri(subject: &str, secret_b32: &str) -> String {
    let label = subject.strip_prefix("member:").unwrap_or(subject);
    format!(
        "otpauth://totp/{ISSUER}:{}?secret={secret_b32}&issuer={ISSUER}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        crate::integrations::email::urlencode(label)
    )
}

// ── Sealing ─────────────────────────────────────────────────────────────────

/// The virtual path a subject's secret is bound to (the at-rest AAD).
fn secret_path(home: &Path, subject: &str) -> std::path::PathBuf {
    home.join("mfa").join(subject)
}

fn seal_secret(home: &Path, subject: &str, secret: &[u8]) -> Result<String, String> {
    let sealed = crate::integrations::at_rest::seal(home, &secret_path(home, subject), secret)?;
    String::from_utf8(sealed).map_err(|e| e.to_string())
}

fn open_secret(home: &Path, subject: &str, sealed: &str) -> Result<Vec<u8>, String> {
    crate::integrations::at_rest::open(home, &secret_path(home, subject), sealed.as_bytes()).map_err(|e| e.to_string())
}

// ── Store ───────────────────────────────────────────────────────────────────

/// Why a code was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotEnrolled,
    AlreadyEnrolled,
    BadCode,
    Replayed,
    Locked(String),
}

impl Refusal {
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NotEnrolled => "mfa_not_enrolled",
            Refusal::AlreadyEnrolled => "mfa_already_enrolled",
            Refusal::BadCode => "mfa_bad_code",
            Refusal::Replayed => "mfa_code_used",
            Refusal::Locked(_) => "mfa_locked",
        }
    }
    fn message(&self) -> String {
        match self {
            Refusal::NotEnrolled => "no second factor is set up for you yet; open /mfa".into(),
            Refusal::AlreadyEnrolled => "a second factor is already set up; reset it first".into(),
            Refusal::BadCode => "that code is not right".into(),
            Refusal::Replayed => "that code was already used; wait for the next one".into(),
            Refusal::Locked(e) => format!("the second-factor secret cannot be opened here: {e}"),
        }
    }
}

pub fn enrolled(conn: &Connection, subject: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM mfa_factors WHERE subject=?1 AND confirmed_at IS NOT NULL",
        [subject],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

/// Begin enrolment: a fresh secret, unconfirmed until a first code is shown.
pub fn start(conn: &Connection, home: &Path, subject: &str) -> Result<(String, String), Refusal> {
    if enrolled(conn, subject).map_err(|e| Refusal::Locked(e.to_string()))? {
        return Err(Refusal::AlreadyEnrolled);
    }
    let secret = random_bytes(20).map_err(Refusal::Locked)?;
    let sealed = seal_secret(home, subject, &secret).map_err(Refusal::Locked)?;
    conn.execute(
        "INSERT INTO mfa_factors (subject, secret_sealed, created_at, confirmed_at, last_step) VALUES (?1,?2,?3,NULL,0) \
         ON CONFLICT(subject) DO UPDATE SET secret_sealed=excluded.secret_sealed, created_at=excluded.created_at, last_step=0",
        rusqlite::params![subject, sealed, now()],
    )
    .map_err(|e| Refusal::Locked(e.to_string()))?;
    let b32 = base32_encode(&secret);
    let uri = otpauth_uri(subject, &b32);
    Ok((b32, uri))
}

fn factor(conn: &Connection, subject: &str) -> rusqlite::Result<Option<(String, Option<i64>, i64)>> {
    conn.query_row(
        "SELECT secret_sealed, confirmed_at, last_step FROM mfa_factors WHERE subject=?1",
        [subject],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

fn check_totp(conn: &Connection, home: &Path, subject: &str, code: &str, at: i64, want_confirmed: bool) -> Result<(), Refusal> {
    let Some((sealed, confirmed, last)) = factor(conn, subject).map_err(|e| Refusal::Locked(e.to_string()))? else {
        return Err(Refusal::NotEnrolled);
    };
    if want_confirmed && confirmed.is_none() {
        return Err(Refusal::NotEnrolled);
    }
    let secret = open_secret(home, subject, &sealed).map_err(Refusal::Locked)?;
    match match_step(&secret, code, at, last) {
        Some(step) => {
            conn.execute("UPDATE mfa_factors SET last_step=?1 WHERE subject=?2", rusqlite::params![step, subject])
                .map_err(|e| Refusal::Locked(e.to_string()))?;
            Ok(())
        }
        None if match_step(&secret, code, at, -1).is_some() => Err(Refusal::Replayed),
        None => Err(Refusal::BadCode),
    }
}

fn new_recovery_codes(conn: &Connection, subject: &str) -> rusqlite::Result<Vec<String>> {
    conn.execute("DELETE FROM mfa_recovery_codes WHERE subject=?1", [subject])?;
    let t = now();
    let mut codes = Vec::with_capacity(RECOVERY_CODES);
    for _ in 0..RECOVERY_CODES {
        let h = hex::encode(random_bytes(5).map_err(rng_sql)?);
        let code = format!("{}-{}", &h[..5], &h[5..]);
        conn.execute(
            "INSERT INTO mfa_recovery_codes (subject, code_hash, created_at) VALUES (?1,?2,?3)",
            rusqlite::params![subject, sha256_hex(&normalize_recovery(&code)), t],
        )?;
        codes.push(code);
    }
    Ok(codes)
}

fn normalize_recovery(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// Confirm enrolment with a first code; returns the recovery codes, shown once.
pub fn confirm(conn: &Connection, home: &Path, subject: &str, code: &str, at: i64) -> Result<Vec<String>, Refusal> {
    if enrolled(conn, subject).map_err(|e| Refusal::Locked(e.to_string()))? {
        return Err(Refusal::AlreadyEnrolled);
    }
    check_totp(conn, home, subject, code, at, false)?;
    conn.execute("UPDATE mfa_factors SET confirmed_at=?1 WHERE subject=?2", rusqlite::params![at, subject])
        .map_err(|e| Refusal::Locked(e.to_string()))?;
    new_recovery_codes(conn, subject).map_err(|e| Refusal::Locked(e.to_string()))
}

/// Verify a TOTP code or a recovery code. Returns which one it was.
pub fn verify(conn: &Connection, home: &Path, subject: &str, code: &str, at: i64) -> Result<&'static str, Refusal> {
    let digits = code.trim().replace(' ', "");
    if digits.len() == DIGITS as usize && digits.bytes().all(|b| b.is_ascii_digit()) {
        return check_totp(conn, home, subject, &digits, at, true).map(|_| "totp");
    }
    if !enrolled(conn, subject).map_err(|e| Refusal::Locked(e.to_string()))? {
        return Err(Refusal::NotEnrolled);
    }
    let n = conn
        .execute(
            "UPDATE mfa_recovery_codes SET used_at=?1 WHERE subject=?2 AND code_hash=?3 AND used_at IS NULL",
            rusqlite::params![at, subject, sha256_hex(&normalize_recovery(code))],
        )
        .map_err(|e| Refusal::Locked(e.to_string()))?;
    if n == 1 {
        Ok("recovery")
    } else {
        Err(Refusal::BadCode)
    }
}

pub fn reset(conn: &Connection, subject: &str) -> rusqlite::Result<bool> {
    let n = conn.execute("DELETE FROM mfa_factors WHERE subject=?1", [subject])?;
    conn.execute("DELETE FROM mfa_recovery_codes WHERE subject=?1", [subject])?;
    Ok(n > 0)
}

/// Mark the session row a member cookie names as having proved a factor now.
pub fn mark_session(conn: &Connection, token: &str, at: i64) -> rusqlite::Result<usize> {
    conn.execute("UPDATE org_invites SET mfa_at=?1 WHERE token=?2 AND used_at IS NOT NULL", rusqlite::params![at, token])
}

// ── Ledger ──────────────────────────────────────────────────────────────────

/// One `identity` row: sign-ins, factors, step-ups and provisioning.
pub(crate) fn record(conn: &Connection, actor: &str, authority: &str, action: &str, resource: &str, decision: &str, outcome: &str) {
    super::ledger::append(conn, &super::ledger::Event {
        event_id: format!("identity:{action}:{}", ulid::Ulid::new()),
        ts_ms: chrono::Utc::now().timestamp_millis(),
        kind: "identity".into(),
        actor: actor.into(),
        authority: Some(authority.into()),
        action: Some(action.into()),
        resource: Some(resource.into()),
        decision: Some(decision.into()),
        outcome: Some(outcome.into()),
        ..Default::default()
    });
}

/// Run a write that answers with a value.
pub(crate) async fn write_value<F>(state: &AppState, f: F) -> Result<Value, String>
where
    F: FnOnce(&Connection) -> rusqlite::Result<Value> + Send + 'static,
{
    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let s2 = slot.clone();
    state
        .store
        .write_async(move |conn| {
            let v = f(conn)?;
            let applied = v["ok"] == json!(true);
            *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
            Ok(WriteOutcome { applied, events: vec![] })
        })
        .await
        .map_err(|e| e.to_string())?;
    let v = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    v.ok_or_else(|| "write returned nothing".to_string())
}

// ── Who ─────────────────────────────────────────────────────────────────────

/// The person this request proved to be: `owner` or `member:<email>`.
pub fn subject_of(headers: &HeaderMap) -> Option<String> {
    match super::human_proof::proven(headers)? {
        super::human_proof::Proven::Owner => Some("owner".into()),
        super::human_proof::Proven::Member(m) => Some(if m.starts_with("member:") { m } else { format!("member:{m}") }),
        super::human_proof::Proven::Policy(_) => None,
    }
}

fn authority(subject: &str) -> &'static str {
    if subject == "owner" {
        "owner_credential"
    } else {
        "member_session"
    }
}

// ── Step-up ─────────────────────────────────────────────────────────────────

fn step_ups() -> &'static Mutex<HashMap<String, (String, i64)>> {
    static M: OnceLock<Mutex<HashMap<String, (String, i64)>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// A five-minute step-up token for `subject`. Only its hash is kept. `None`
/// when the OS random source fails.
pub fn issue_step_up(subject: &str, at: i64) -> Option<(String, i64)> {
    let token = format!("su_{}", hex::encode(random_bytes(24).ok()?));
    let exp = at + STEP_UP_TTL_SECS;
    let mut m = step_ups().lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, (_, e)| *e > at);
    m.insert(sha256_hex(&token), (subject.to_string(), exp));
    counters::bump(&counters::STEP_UP_ISSUED);
    Some((token, exp))
}

pub fn step_up_valid(headers: &HeaderMap, subject: &str, at: i64) -> bool {
    let Some(tok) = headers.get(STEP_UP_HEADER).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let m = step_ups().lock().unwrap_or_else(|e| e.into_inner());
    m.get(&sha256_hex(tok.trim())).is_some_and(|(s, exp)| s == subject && *exp > at)
}

/// Step-up for one of the owner's governance powers (`action` names it).
/// `None` lets the call through; the caller's own owner check still runs.
pub fn step_up_gate(headers: &HeaderMap, action: &str) -> Option<Response> {
    step_up_gate_with(headers, action, step_up_mode(), now())
}

pub fn step_up_gate_with(headers: &HeaderMap, action: &str, mode: Mode, at: i64) -> Option<Response> {
    if mode == Mode::Off {
        return None;
    }
    let subject = subject_of(headers)?;
    if step_up_valid(headers, &subject, at) {
        counters::bump(&counters::STEP_UP_PASSED);
        return None;
    }
    if mode == Mode::Report {
        counters::bump(&counters::STEP_UP_WOULD_REFUSE);
        tracing::info!(action, subject = %subject, verdict = "step_up_would_be_refused", measured = true, n_considered = 1,
            "an owner-only power ran without a fresh second factor (AMUX_MFA_STEP_UP=report)");
        return None;
    }
    counters::bump(&counters::STEP_UP_REFUSED);
    tracing::warn!(action, subject = %subject, verdict = "step_up_refused", "an owner-only power was refused for want of a fresh second factor");
    Some(
        (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "code": "step_up_required",
                "action": action,
                "error": format!("{action} needs a fresh second factor"),
                "how": "POST /api/mfa/step-up {\"code\":\"123456\"} (amux mfa step-up <code>), then send the token as X-Amux-Step-Up",
            })),
        )
            .into_response(),
    )
}

// ── The session check for members ───────────────────────────────────────────

/// What `org::local_member_identity` knows about the session it resolved.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionFacts {
    pub via_sso: bool,
    pub mfa_ok: bool,
    pub enrolled: bool,
}

/// Paths a member who still owes a sign-in step must reach to pay it.
pub fn exempt(path: &str) -> bool {
    path == "/mfa"
        || path == "/api/mfa"
        || path.starts_with("/api/mfa/")
        || path.starts_with("/sso/")
        || path == "/api/identity"
        || path.starts_with("/invite/")
        || matches!(path, "/manifest.json" | "/icon.svg" | "/icon.png" | "/sw.js")
}

/// What this session still owes, in the order it is asked for.
pub fn owed(facts: SessionFacts, pol: Policy, sso_configured: bool) -> Vec<&'static str> {
    let mut out = Vec::new();
    if sso_configured && !facts.via_sso {
        out.push("sso");
    }
    let mfa_needed = match pol {
        Policy::Off => false,
        Policy::Optional => facts.enrolled,
        Policy::Required => true,
    };
    if mfa_needed && !facts.mfa_ok {
        out.push("mfa");
    }
    out
}

/// The member gate. `None` lets the request through.
pub fn member_gate(method: &Method, path: &str, facts: SessionFacts) -> Option<Response> {
    member_gate_with(method, path, facts, policy(), enforce_mode(), super::sso::required_mode(), super::sso::configured())
}

pub fn member_gate_with(
    method: &Method,
    path: &str,
    facts: SessionFacts,
    pol: Policy,
    mfa_mode: Mode,
    sso_mode: Mode,
    sso_configured: bool,
) -> Option<Response> {
    if exempt(path) {
        return None;
    }
    let owed: Vec<&str> = owed(facts, pol, sso_configured)
        .into_iter()
        .filter(|o| match *o {
            "sso" => sso_mode != Mode::Off,
            _ => mfa_mode != Mode::Off,
        })
        .collect();
    counters::bump(&counters::SESSION_CONSIDERED);
    let first = *owed.first()?;
    let mode = if first == "sso" { sso_mode } else { mfa_mode };
    if mode == Mode::Report {
        if first == "sso" {
            super::sso::counters::bump(&super::sso::counters::REQUIRED_WOULD_REFUSE);
        } else {
            counters::bump(&counters::SESSION_WOULD_REFUSE);
        }
        return None;
    }
    if first == "sso" {
        super::sso::counters::bump(&super::sso::counters::REQUIRED_REFUSED);
    } else {
        counters::bump(&counters::SESSION_REFUSED);
    }
    let (to, code, error) = if first == "sso" {
        ("/sso/login", "sso_required", "this workspace signs members in through its identity provider")
    } else {
        ("/mfa", "mfa_required", "this session has not proved a second factor")
    };
    if path.starts_with("/api/") || *method != Method::GET {
        return Some((StatusCode::FORBIDDEN, Json(json!({"ok": false, "code": code, "error": error, "go": to}))).into_response());
    }
    let loc = format!("{to}?next={}", crate::integrations::email::urlencode(path));
    Some((StatusCode::SEE_OTHER, [(header::LOCATION, loc)]).into_response())
}

// ── HTTP ────────────────────────────────────────────────────────────────────

/// Mounted in the protected router; `org` admits members to `/api/mfa/`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/mfa/status", get(status))
        .route("/api/mfa/enroll", post(enroll))
        .route("/api/mfa/enroll/confirm", post(enroll_confirm))
        .route("/api/mfa/verify", post(verify_handler))
        .route("/api/mfa/step-up", post(step_up))
        .route("/api/mfa/recovery-codes", post(regenerate))
        .route("/api/mfa/reset", post(reset_handler))
}

/// The enrolment and sign-in page. Outside auth like `/invite/`: it holds no
/// data and every action it takes is an `/api/mfa/` call made as the caller.
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/mfa", get(page))
}

fn refuse(status: StatusCode, code: &str, error: &str) -> Response {
    (status, Json(json!({"ok": false, "code": code, "error": error}))).into_response()
}

fn who(headers: &HeaderMap) -> Result<String, Box<Response>> {
    subject_of(headers).ok_or_else(|| Box::new(refuse(StatusCode::FORBIDDEN, "person_required", "only a signed-in person has a second factor")))
}

#[derive(Deserialize, Default)]
pub struct CodeBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub subject: Option<String>,
}

fn answer(res: Result<Value, String>) -> Response {
    match res {
        Ok(v) if v["ok"] == json!(true) => Json(v).into_response(),
        Ok(v) => {
            let status = v["status"].as_u64().and_then(|s| StatusCode::from_u16(s as u16).ok()).unwrap_or(StatusCode::FORBIDDEN);
            let mut v = v;
            if let Some(o) = v.as_object_mut() {
                o.remove("status");
            }
            (status, Json(v)).into_response()
        }
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, "mfa_store_error", &e),
    }
}

fn refusal_value(r: &Refusal) -> Value {
    let status = match r {
        Refusal::Locked(_) => 503,
        Refusal::AlreadyEnrolled => 409,
        _ => 403,
    };
    json!({"ok": false, "code": r.code(), "error": r.message(), "status": status})
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let subject = match who(&headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let s2 = subject.clone();
    let enrolled = state.store.read_async(move |c| Ok(enrolled(c, &s2)?)).await.unwrap_or(false);
    Json(json!({
        "ok": true,
        "subject": subject,
        "enrolled": enrolled,
        "session_mfa": super::org::local_member_session_mfa(&headers),
        "policy": policy().as_str(),
        "enforce": enforce_mode().as_str(),
        "step_up": step_up_mode().as_str(),
        "step_up_fresh": step_up_valid(&headers, &subject, now()),
    }))
    .into_response()
}

async fn enroll(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let subject = match who(&headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if policy() == Policy::Off {
        return refuse(StatusCode::CONFLICT, "mfa_off", "second factors are turned off here (AMUX_MFA_POLICY=off)");
    }
    let home = crate::config::amux_home();
    answer(
        write_value(&state, move |c| {
            Ok(match start(c, &home, &subject) {
                Ok((secret, uri)) => json!({"ok": true, "subject": subject, "secret": secret, "otpauth_uri": uri,
                    "next": "POST /api/mfa/enroll/confirm {\"code\":\"<6 digits>\"}"}),
                Err(r) => refusal_value(&r),
            })
        })
        .await,
    )
}

async fn enroll_confirm(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<CodeBody>) -> Response {
    let subject = match who(&headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let home = crate::config::amux_home();
    let session = super::org::local_member_session_token(&headers);
    answer(
        write_value(&state, move |c| {
            let at = now();
            Ok(match confirm(c, &home, &subject, &b.code, at) {
                Ok(codes) => {
                    if let Some(t) = &session {
                        mark_session(c, t, at)?;
                    }
                    counters::bump(&counters::ENROLLED);
                    record(c, &subject, authority(&subject), "mfa_enrolled", &subject, "allowed", "applied");
                    json!({"ok": true, "subject": subject, "recovery_codes": codes,
                        "note": "each recovery code works once; keep them somewhere other than this device"})
                }
                Err(r) => {
                    counters::bump(&counters::FAILED);
                    record(c, &subject, authority(&subject), "mfa_enroll_confirm", &subject, "refused", r.code());
                    refusal_value(&r)
                }
            })
        })
        .await,
    )
}

async fn verify_handler(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<CodeBody>) -> Response {
    let subject = match who(&headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let home = crate::config::amux_home();
    let session = super::org::local_member_session_token(&headers);
    answer(
        write_value(&state, move |c| {
            let at = now();
            Ok(match verify(c, &home, &subject, &b.code, at) {
                Ok(kind) => {
                    if let Some(t) = &session {
                        mark_session(c, t, at)?;
                    }
                    counters::bump(&counters::VERIFIED);
                    if kind == "recovery" {
                        counters::bump(&counters::RECOVERY_USED);
                    }
                    record(c, &subject, authority(&subject), "mfa_verified", &subject, "allowed", kind);
                    json!({"ok": true, "subject": subject, "with": kind, "session_marked": session.is_some()})
                }
                Err(r) => {
                    if r == Refusal::Replayed {
                        counters::bump(&counters::REPLAY_REFUSED);
                    }
                    counters::bump(&counters::FAILED);
                    record(c, &subject, authority(&subject), "mfa_verify", &subject, "refused", r.code());
                    refusal_value(&r)
                }
            })
        })
        .await,
    )
}

async fn step_up(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<CodeBody>) -> Response {
    let subject = match who(&headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let home = crate::config::amux_home();
    answer(
        write_value(&state, move |c| {
            let at = now();
            Ok(match verify(c, &home, &subject, &b.code, at) {
                Ok(kind) => {
                    let Some((token, exp)) = issue_step_up(&subject, at) else {
                        return Ok(refusal_value(&Refusal::Locked("the OS random source failed".into())));
                    };
                    record(c, &subject, authority(&subject), "mfa_step_up", &subject, "allowed", kind);
                    json!({"ok": true, "subject": subject, "token": token, "header": "X-Amux-Step-Up", "expires_at": exp})
                }
                Err(r) => {
                    counters::bump(&counters::FAILED);
                    record(c, &subject, authority(&subject), "mfa_step_up", &subject, "refused", r.code());
                    refusal_value(&r)
                }
            })
        })
        .await,
    )
}

async fn regenerate(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<CodeBody>) -> Response {
    let subject = match who(&headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let home = crate::config::amux_home();
    answer(
        write_value(&state, move |c| {
            Ok(match check_totp(c, &home, &subject, &b.code, now(), true) {
                Ok(()) => {
                    let codes = new_recovery_codes(c, &subject)?;
                    record(c, &subject, authority(&subject), "mfa_recovery_codes", &subject, "allowed", "applied");
                    json!({"ok": true, "subject": subject, "recovery_codes": codes})
                }
                Err(r) => refusal_value(&r),
            })
        })
        .await,
    )
}

/// Reset a factor. A person resets their own with a current code; the owner
/// resets a member's (a lost phone), under step-up.
async fn reset_handler(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<CodeBody>) -> Response {
    let subject = match who(&headers) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let target = b.subject.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| subject.clone());
    let for_other = target != subject;
    if for_other {
        if subject != "owner" {
            return refuse(StatusCode::FORBIDDEN, "owner_only", "only the owner resets another person's second factor");
        }
        if let Some(r) = step_up_gate(&headers, "resetting a member's second factor") {
            return r;
        }
    }
    let home = crate::config::amux_home();
    answer(
        write_value(&state, move |c| {
            if !for_other {
                if let Err(r) = verify(c, &home, &subject, &b.code, now()) {
                    return Ok(refusal_value(&r));
                }
            }
            let removed = reset(c, &target)?;
            if removed {
                counters::bump(&counters::RESET);
            }
            record(c, &subject, authority(&subject), "mfa_reset", &target, "allowed", if removed { "applied" } else { "nothing_to_reset" });
            Ok(json!({"ok": true, "subject": target, "reset": removed}))
        })
        .await,
    )
}

/// `mfa` in `/api/contract/counters`.
pub fn report() -> Value {
    use counters::*;
    json!({
        "policy": policy().as_str(),
        "policy_env": POLICY_ENV,
        "enforce": enforce_mode().as_str(),
        "enforce_env": ENFORCE_ENV,
        "step_up": step_up_mode().as_str(),
        "step_up_env": STEP_UP_ENV,
        "measured": true,
        "n_considered": get(&SESSION_CONSIDERED),
        "counts": {
            "enrolled": get(&ENROLLED),
            "verified": get(&VERIFIED),
            "failed": get(&FAILED),
            "replay_refused": get(&REPLAY_REFUSED),
            "recovery_used": get(&RECOVERY_USED),
            "reset": get(&RESET),
            "step_up_issued": get(&STEP_UP_ISSUED),
            "step_up_passed": get(&STEP_UP_PASSED),
            "step_up_would_refuse": get(&STEP_UP_WOULD_REFUSE),
            "step_up_refused": get(&STEP_UP_REFUSED),
            "session_would_refuse": get(&SESSION_WOULD_REFUSE),
            "session_refused": get(&SESSION_REFUSED),
        },
    })
}

const PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Second factor · amux</title>
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:28rem;margin:3rem auto;padding:0 1rem;color:#222}
h1{font-size:1.3rem}input{font:inherit;padding:.5rem;width:100%;box-sizing:border-box;margin:.4rem 0}
button{font:inherit;padding:.5rem 1rem;cursor:pointer}code{word-break:break-all;background:#f3f3f3;padding:.1rem .3rem}
.err{color:#b00}.ok{color:#070}ol{padding-left:1.2rem}</style></head><body>
<h1>Second factor</h1><div id="out">Loading…</div>
<script>
const out=document.getElementById('out');
const next=new URLSearchParams(location.search).get('next')||'/';
const safeNext=(next.startsWith('/')&&!next.startsWith('//'))?next:'/';
const esc=s=>String(s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
async function call(path,body){const r=await fetch(path,{method:body===undefined?'GET':'POST',credentials:'same-origin',
 headers:{'Content-Type':'application/json'},body:body===undefined?undefined:JSON.stringify(body)});
 let j={};try{j=await r.json()}catch(e){}return [r.status,j];}
function form(label,btn,fn){out.insertAdjacentHTML('beforeend','<form id="f"><label>'+label+'<input id="code" name="code" autocomplete="one-time-code" inputmode="text" autofocus></label><button id="go" type="submit">'+btn+'</button><p id="msg" class="err"></p></form>');
 document.getElementById('f').onsubmit=async e=>{e.preventDefault();await fn(document.getElementById('code').value.trim());};}
async function main(){
 const [s,st]=await call('/api/mfa/status');
 if(s!==200){out.innerHTML='<p class="err">Sign in first.</p>';return;}
 out.innerHTML='<p>Signed in as <b>'+esc(st.subject)+'</b>.</p>';
 if(st.enrolled){
  if(st.session_mfa){out.insertAdjacentHTML('beforeend','<p class="ok" id="done">This session has proved its second factor.</p><p><a href="'+esc(safeNext)+'">Continue</a></p>');return;}
  form('Code from your authenticator app, or a recovery code','Verify',async code=>{const [c,j]=await call('/api/mfa/verify',{code});
   if(c===200){location.href=safeNext;}else{document.getElementById('msg').textContent=j.error||('error '+c);}});
  return;}
 if(st.policy==='off'){out.insertAdjacentHTML('beforeend','<p>Second factors are turned off on this server.</p>');return;}
 const [c,en]=await call('/api/mfa/enroll',{});
 if(c!==200){out.insertAdjacentHTML('beforeend','<p class="err">'+esc(en.error||('error '+c))+'</p>');return;}
 out.insertAdjacentHTML('beforeend','<p>Add this account to an authenticator app (1Password, Google Authenticator, Authy…) with the link or the key, then enter the code it shows.</p>'+
  '<p><a id="uri" href="'+esc(en.otpauth_uri)+'">Open in authenticator</a></p><p>Key: <code id="secret">'+esc(en.secret)+'</code></p>');
 form('Six-digit code','Turn on',async code=>{const [c2,j]=await call('/api/mfa/enroll/confirm',{code});
  if(c2!==200){document.getElementById('msg').textContent=j.error||('error '+c2);return;}
  out.innerHTML='<p class="ok" id="done">Second factor is on.</p><p>Recovery codes. Each works once; keep them somewhere safe:</p><ol id="codes">'+
   j.recovery_codes.map(x=>'<li><code>'+esc(x)+'</code></li>').join('')+'</ol><p><a href="'+esc(safeNext)+'">Continue</a></p>';});
}
main();
</script></body></html>"#;

async fn page() -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Html(PAGE)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    #[test]
    fn hotp_matches_rfc4226_vectors() {
        // RFC 4226 appendix D, secret "12345678901234567890".
        let k = b"12345678901234567890";
        let want = [755224, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(hotp(k, i as u64), *w, "counter {i}");
        }
        // RFC 6238 appendix B, SHA-1, 8 digits truncated to our 6: T=59 -> 94287082.
        assert_eq!(totp_at(k, 59), 287082);
        assert_eq!(totp_at(k, 1111111109), 81804); // 07081804
    }

    #[test]
    fn base32_round_trips() {
        for n in 0..25 {
            let b: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37)).collect();
            assert_eq!(base32_decode(&base32_encode(&b)).unwrap(), b);
        }
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn a_code_is_good_once_and_only_near_now() {
        let k = b"12345678901234567890";
        let t = 1_700_000_000;
        let code = format!("{:06}", totp_at(k, t));
        let step = match_step(k, &code, t, 0).expect("current code");
        assert_eq!(match_step(k, &code, t, step), None, "the same code twice");
        assert!(match_step(k, &code, t + 30, 0).is_some(), "one step late is fine");
        assert_eq!(match_step(k, &code, t + 120, 0), None, "four steps late is not");
        assert_eq!(match_step(k, "12345", t, 0), None);
    }

    #[test]
    fn enrol_confirm_verify_recovery_and_replay() {
        let home = tempfile::tempdir().unwrap();
        let c = db();
        let s = "member:a@x.io";
        let (b32, uri) = start(&c, home.path(), s).unwrap();
        assert!(uri.starts_with("otpauth://totp/amux:a%40x.io?secret="));
        let sealed: String = c.query_row("SELECT secret_sealed FROM mfa_factors", [], |r| r.get(0)).unwrap();
        assert!(!sealed.contains(&b32), "the secret is not stored in the clear");
        let secret = base32_decode(&b32).unwrap();
        let t = 1_700_000_000;
        assert_eq!(verify(&c, home.path(), s, &format!("{:06}", totp_at(&secret, t)), t), Err(Refusal::NotEnrolled),
            "an unconfirmed factor does not sign anyone in");
        let codes = confirm(&c, home.path(), s, &format!("{:06}", totp_at(&secret, t)), t).unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert!(enrolled(&c, s).unwrap());
        let next = format!("{:06}", totp_at(&secret, t + 30));
        assert_eq!(verify(&c, home.path(), s, &next, t + 30), Ok("totp"));
        assert_eq!(verify(&c, home.path(), s, &next, t + 30), Err(Refusal::Replayed));
        assert_eq!(verify(&c, home.path(), s, "000000", t + 60).unwrap_err(), Refusal::BadCode);
        assert_eq!(verify(&c, home.path(), s, &codes[0].to_uppercase(), t), Ok("recovery"));
        assert_eq!(verify(&c, home.path(), s, &codes[0], t), Err(Refusal::BadCode), "a recovery code works once");
        let stored: i64 = c.query_row("SELECT COUNT(*) FROM mfa_recovery_codes WHERE code_hash LIKE ?1", [format!("%{}%", &codes[1][..5])], |r| r.get(0)).unwrap();
        assert_eq!(stored, 0, "recovery codes are stored hashed");
        assert!(reset(&c, s).unwrap());
        assert!(!enrolled(&c, s).unwrap());
    }

    #[test]
    fn a_secret_sealed_for_one_subject_does_not_open_for_another() {
        let home = tempfile::tempdir().unwrap();
        let c = db();
        start(&c, home.path(), "member:a@x.io").unwrap();
        let sealed: String = c.query_row("SELECT secret_sealed FROM mfa_factors", [], |r| r.get(0)).unwrap();
        c.execute("INSERT INTO mfa_factors (subject, secret_sealed, created_at, confirmed_at) VALUES ('owner', ?1, 0, 1)", [&sealed]).unwrap();
        assert!(matches!(verify(&c, home.path(), "owner", "123456", 1_700_000_000), Err(Refusal::Locked(_))));
    }

    #[test]
    fn the_member_gate_asks_for_what_is_owed_by_mode() {
        let get = Method::GET;
        let none = SessionFacts::default();
        let enrolled = SessionFacts { enrolled: true, ..none };
        // optional + not enrolled owes nothing
        assert!(member_gate_with(&get, "/api/sessions", none, Policy::Optional, Mode::Enforce, Mode::Enforce, false).is_none());
        // optional + enrolled, enforce: API refused, page redirected, exempt paths pass
        let r = member_gate_with(&get, "/api/sessions", enrolled, Policy::Optional, Mode::Enforce, Mode::Off, false).unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = member_gate_with(&get, "/", enrolled, Policy::Optional, Mode::Enforce, Mode::Off, false).unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[header::LOCATION], "/mfa?next=%2F");
        assert!(member_gate_with(&get, "/api/mfa/verify", enrolled, Policy::Optional, Mode::Enforce, Mode::Off, false).is_none());
        // report lets it through
        assert!(member_gate_with(&get, "/api/sessions", enrolled, Policy::Optional, Mode::Report, Mode::Off, false).is_none());
        // proved: through
        let proved = SessionFacts { mfa_ok: true, ..enrolled };
        assert!(member_gate_with(&get, "/api/sessions", proved, Policy::Required, Mode::Enforce, Mode::Off, false).is_none());
        // required: owed even when not enrolled
        assert!(member_gate_with(&get, "/api/sessions", none, Policy::Required, Mode::Enforce, Mode::Off, false).is_some());
        // SSO configured and required: an invite session is sent to /sso/login first
        let r = member_gate_with(&get, "/", proved, Policy::Optional, Mode::Enforce, Mode::Enforce, true).unwrap();
        assert_eq!(r.headers()[header::LOCATION], "/sso/login?next=%2F");
        let sso = SessionFacts { via_sso: true, ..proved };
        assert!(member_gate_with(&get, "/", sso, Policy::Optional, Mode::Enforce, Mode::Enforce, true).is_none());
        // off is off
        assert!(member_gate_with(&get, "/api/sessions", none, Policy::Off, Mode::Enforce, Mode::Off, true).is_none());
    }

    #[test]
    fn step_up_tokens_are_per_subject_and_expire() {
        let (tok, exp) = issue_step_up("owner", 1000).unwrap();
        let mut h = HeaderMap::new();
        h.insert(STEP_UP_HEADER, tok.parse().unwrap());
        assert!(step_up_valid(&h, "owner", 1000));
        assert!(!step_up_valid(&h, "member:a@x.io", 1000));
        assert!(!step_up_valid(&h, "owner", exp));
    }
}
