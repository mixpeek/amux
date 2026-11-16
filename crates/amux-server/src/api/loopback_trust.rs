//! Loopback is not a credential (governance PR 3).
//!
//! WHAT THIS REPLACES. `auth::require_bearer` let every loopback peer through
//! with no credential (Python parity: "local sessions and CLI tools
//! authenticate by locality"). Every worker runs on the same machine, so
//! locality said nothing about WHO was asking: a worker's bare `curl` and the
//! owner's terminal were the same request. Worse, on a host behind a sign-in
//! door (a cloud desktop) or the cloud gateway there is no owner token at all
//! (`AMUX_AUTH_TOKEN=none`), so every request passed and the owner could not be
//! told from anything else (`human_proof` reported `enforceable: false`).
//!
//! WHAT PROVES A CALLER NOW, on a loopback (or, on a token-less host, any)
//! socket:
//! - the owner bearer (unchanged; the CLI, hooks and dashboard send it);
//! - a verified member cookie (unchanged);
//! - a worker's minted token for the lane it names (governance PR 1);
//! - the FRONT-DOOR OWNER PROOF: a secret the server mints at
//!   `~/.amux/front-door-owner` (0600), that the sign-in door (cloud/desktop)
//!   copies at install and returns to Caddy for the signed-in owner, and that
//!   Caddy forwards as `X-Amux-Front-Door-Owner` after stripping any copy the
//!   client sent. `AMUX_FRONT_DOOR_SECRET` supplies it instead, for a gateway
//!   that sets it in the container's environment. The owner's own CLI on such
//!   a host reads the same file. A valid proof stamps the owner mark that
//!   `human_proof` reads, so the anonymous-owner gate becomes enforceable
//!   there the first time the door proves the owner (persisted in
//!   `front-door-owner.seen`, so a restart does not forget it).
//!
//! WHAT A FAILED CHECK COSTS is `AMUX_LOOPBACK_TRUST` (A11: this is the auth
//! path every caller shares, so it ships able to report before it refuses, and
//! able to turn off):
//! - `report` (default): allowed as before, counted, and broken down by caller
//!   (user agent family, path, claimed lane) in `GET /api/contract/counters`
//!   under `loopback`, so a week of report mode names every caller still
//!   relying on locality; each new caller is logged once,
//!   `loopback_would_be_refused`;
//! - `enforce`: refused 401 `loopback_not_trusted`. `AMUX_RS_NO_LOOPBACK_BYPASS=1`
//!   (the e2e knob that predates this) means `enforce`;
//! - `off`: loopback trusted, nothing counted (the previous behaviour).
//!
//! Public paths and non-API GETs (the dashboard shell) pass as before and are
//! not counted: they need no credential from anyone. The server's own
//! in-process calls have no socket and are not gated.
//!
//! A HOST WITH NO OWNER TOKEN AND NO FRONT DOOR YET keeps passing everything:
//! there is nothing a caller could present, so refusing would refuse the
//! owner. The report says `enforceable: false` there rather than claiming a
//! boundary it cannot hold (ethos 3).

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

pub use super::worker_identity::Mode;

pub const MODE_ENV: &str = "AMUX_LOOPBACK_TRUST";
pub const LEGACY_ENFORCE_ENV: &str = "AMUX_RS_NO_LOOPBACK_BYPASS";
pub const FRONT_DOOR_HEADER: &str = "x-amux-front-door-owner";
pub const FRONT_DOOR_SECRET_ENV: &str = "AMUX_FRONT_DOOR_SECRET";
pub const FRONT_DOOR_FILE: &str = "front-door-owner";
const FRONT_DOOR_SEEN_FILE: &str = "front-door-owner.seen";

pub fn mode() -> Mode {
    #[cfg(test)]
    if let Some(m) = test_state::MODE.with(|c| c.get()) {
        return m;
    }
    mode_from(
        std::env::var(MODE_ENV).ok().as_deref(),
        std::env::var(LEGACY_ENFORCE_ENV).ok().as_deref(),
    )
}

fn mode_from(v: Option<&str>, legacy: Option<&str>) -> Mode {
    if legacy.is_some_and(|v| v.trim() == "1") {
        return Mode::Enforce;
    }
    Mode::parse(v)
}

// ── Front-door owner proof ──────────────────────────────────────────────────

struct FrontDoor {
    secret: Option<String>,
    home: Option<PathBuf>,
}

#[cfg_attr(test, allow(dead_code))]
static FRONT_DOOR: OnceLock<Mutex<FrontDoor>> = OnceLock::new();
#[cfg_attr(test, allow(dead_code))]
static FRONT_DOOR_SEEN: AtomicBool = AtomicBool::new(false);

/// The process's front-door state. Tests get their own per thread (tokio
/// tests run on one thread each), so one test's door never proves the owner
/// for another test's server.
fn with_front_door<R>(f: impl FnOnce(&mut FrontDoor) -> R) -> R {
    #[cfg(test)]
    {
        test_state::DOOR.with(|d| f(&mut d.borrow_mut()))
    }
    #[cfg(not(test))]
    {
        let m = FRONT_DOOR.get_or_init(|| Mutex::new(FrontDoor { secret: None, home: None }));
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut g)
    }
}

fn seen_flag() -> bool {
    #[cfg(test)]
    {
        test_state::SEEN.with(|c| c.get())
    }
    #[cfg(not(test))]
    {
        FRONT_DOOR_SEEN.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Set the flag; returns its previous value.
fn set_seen(v: bool) -> bool {
    #[cfg(test)]
    {
        test_state::SEEN.with(|c| c.replace(v))
    }
    #[cfg(not(test))]
    {
        FRONT_DOOR_SEEN.swap(v, std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
pub(crate) mod test_state {
    use super::{FrontDoor, Mode};
    use std::cell::{Cell, RefCell};
    thread_local! {
        pub static MODE: Cell<Option<Mode>> = const { Cell::new(None) };
        pub static SEEN: Cell<bool> = const { Cell::new(false) };
        pub static DOOR: RefCell<FrontDoor> = const { RefCell::new(FrontDoor { secret: None, home: None }) };
    }
    pub fn set_mode(m: Option<Mode>) {
        MODE.with(|c| c.set(m));
    }
}

fn random_secret() -> Option<String> {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Load (or mint, 0600) the front-door secret at start. The env value wins, for
/// a gateway that cannot write the file. Minting on every host is harmless: a
/// secret no door returns proves nothing and is never seen.
pub fn init(home: &Path, env_secret: Option<&str>) {
    let secret = match env_secret.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(s.to_string()),
        None => load_or_mint(&home.join(FRONT_DOOR_FILE)),
    };
    let seen = home.join(FRONT_DOOR_SEEN_FILE).exists();
    set_seen(seen);
    with_front_door(|fd| {
        fd.secret = secret;
        fd.home = Some(home.to_path_buf());
    });
}

fn load_or_mint(path: &Path) -> Option<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let s = random_secret()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    crate::integrations::secure_store::write(path, format!("{s}\n").as_bytes()).ok()?;
    Some(s)
}

/// Whether a front door has ever proven the owner to this server.
pub fn front_door_active() -> bool {
    seen_flag()
}

/// Remove any `X-Amux-Front-Door-Owner` from the request and say whether it
/// matched the secret. Removed either way: handlers never see it, and only
/// `require_bearer` (which calls this first) can turn it into the owner mark.
pub fn take_front_door_proof(headers: &mut HeaderMap) -> bool {
    let Some(v) = headers.remove(FRONT_DOOR_HEADER) else { return false };
    let presented = v.to_str().unwrap_or("").trim().to_string();
    if presented.is_empty() {
        return false;
    }
    let (ok, home) = with_front_door(|fd| {
        (
            fd.secret.as_deref().is_some_and(|s| super::auth::constant_time_eq(s.as_bytes(), presented.as_bytes())),
            fd.home.clone(),
        )
    });
    if !ok {
        counters::bump(&counters::FRONT_DOOR_INVALID);
        if first_time("front_door_invalid") {
            tracing::warn!(target: "amux::auth", measured = true, n_considered = 1,
                verdict = "front_door_owner_proof_invalid",
                "a request carried a front-door owner proof that does not match this server's secret; ignored");
        }
        return false;
    }
    counters::bump(&counters::FRONT_DOOR_VALID);
    if !set_seen(true) {
        if let Some(h) = home {
            let _ = std::fs::write(h.join(FRONT_DOOR_SEEN_FILE), b"1\n");
        }
        tracing::info!(target: "amux::auth", measured = true, n_considered = 1,
            verdict = "front_door_owner_proof_first_seen",
            "the sign-in door proved the owner to this server; loopback and anonymous-owner checks are now enforceable here");
    }
    true
}

// ── Counters ────────────────────────────────────────────────────────────────

pub(crate) mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static CONSIDERED: AtomicU64 = AtomicU64::new(0);
    pub static WORKER_TOKEN: AtomicU64 = AtomicU64::new(0);
    pub static WOULD_REFUSE: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static FRONT_DOOR_VALID: AtomicU64 = AtomicU64::new(0);
    pub static FRONT_DOOR_INVALID: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// Uncredentialed callers by (agent, path, lane, peer). Capped: past the cap a
/// new caller is counted under `other`, so the map discriminates instead of
/// growing into a log (ethos 5).
const CALLERS_CAP: usize = 200;
static CALLERS: OnceLock<Mutex<std::collections::BTreeMap<String, u64>>> = OnceLock::new();

fn first_time(key: &str) -> bool {
    static SEEN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(Default::default).lock().map(|mut s| s.insert(key.to_string())).unwrap_or(false)
}

/// A short name for the client: `curl`, `python-urllib`, `amux-cli`, `browser`...
fn agent_family(headers: &HeaderMap) -> String {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let fam = if ua.is_empty() {
        "none"
    } else if ua.starts_with("curl/") {
        "curl"
    } else if ua.starts_with("python-urllib") {
        "python-urllib"
    } else if ua.starts_with("python-requests") {
        "python-requests"
    } else if ua.contains("reqwest") {
        "reqwest"
    } else if ua.starts_with("node") || ua.contains("undici") || ua.contains("node-fetch") {
        "node"
    } else if ua.contains("mozilla/") {
        "browser"
    } else {
        ua.split(['/', ' ']).next().unwrap_or("other")
    };
    fam.chars().take(32).collect()
}

/// `/api/sessions/x/report` -> `/api/sessions/*/report`: the shape, not the id.
fn path_shape(path: &str) -> String {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).take(4).collect();
    let mut out = String::new();
    for (i, s) in segs.iter().enumerate() {
        out.push('/');
        if i >= 2 && (s.len() > 24 || s.chars().any(|c| c.is_ascii_digit()) || i == 2 && segs.first() == Some(&"api") && segs.get(1).is_some_and(|p| matches!(*p, "sessions" | "workers" | "board" | "page-links" | "conversations"))) {
            out.push('*');
        } else {
            out.push_str(s);
        }
    }
    if out.is_empty() { "/".into() } else { out }
}

fn claimed_lane(headers: &HeaderMap) -> Option<String> {
    ["x-amux-session", "x-amux-worker"].iter().find_map(|h| {
        headers.get(*h).and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
    })
}

fn record_caller(headers: &HeaderMap, path: &str, loopback: bool) -> (String, bool) {
    let lane = claimed_lane(headers).unwrap_or_else(|| "-".into());
    let key = format!("{} {} lane={} peer={}", agent_family(headers), path_shape(path), lane,
        if loopback { "loopback" } else { "remote" });
    let mut new = false;
    if let Ok(mut m) = CALLERS.get_or_init(Default::default).lock() {
        if m.contains_key(&key) || m.len() < CALLERS_CAP {
            let e = m.entry(key.clone()).or_insert_with(|| {
                new = true;
                0
            });
            *e += 1;
        } else {
            *m.entry("other".into()).or_insert(0) += 1;
        }
    }
    (key, new)
}

// ── The decision ────────────────────────────────────────────────────────────

/// Whether the request presents a worker's minted token for the lane it names.
pub fn worker_token_valid(headers: &HeaderMap) -> bool {
    let Some(lane) = claimed_lane(headers).filter(|l| super::session_verbs::valid_session_name(l)) else {
        return false;
    };
    let token = headers.get(super::worker_identity::TOKEN_HEADER).and_then(|v| v.to_str().ok());
    if token.is_none_or(|t| t.trim().is_empty()) {
        return false;
    }
    super::worker_identity::check_with(&crate::config::amux_home(), &lane, token, Mode::Report)
        == super::worker_identity::Check::Valid
}

#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Let it through (credentialed, exempt, `off`, or counted in `report`).
    Admit,
    /// Refuse it: `enforce`, and nothing proved the caller.
    Refuse,
}

/// Decide for a socket request that presented neither the owner bearer, a
/// front-door proof, nor a verified member cookie. `exempt` is a public path or
/// a non-API GET, which needs no credential from anyone.
pub fn gate(headers: &HeaderMap, path: &str, method: &str, loopback: bool, exempt: bool) -> Gate {
    gate_in(mode(), headers, path, method, loopback, exempt)
}

pub fn gate_in(mode: Mode, headers: &HeaderMap, path: &str, method: &str, loopback: bool, exempt: bool) -> Gate {
    if mode == Mode::Off || exempt {
        return Gate::Admit;
    }
    counters::bump(&counters::CONSIDERED);
    if worker_token_valid(headers) {
        counters::bump(&counters::WORKER_TOKEN);
        return Gate::Admit;
    }
    let (caller, new) = record_caller(headers, path, loopback);
    if mode == Mode::Enforce {
        counters::bump(&counters::REFUSED);
        if new {
            tracing::warn!(target: "amux::auth", caller, method, path, measured = true, n_considered = 1,
                verdict = "loopback_refused", "a local request presented no credential; refused (AMUX_LOOPBACK_TRUST=enforce)");
        }
        return Gate::Refuse;
    }
    counters::bump(&counters::WOULD_REFUSE);
    if new {
        tracing::warn!(target: "amux::auth", caller, method, path, measured = true, n_considered = 1,
            verdict = "loopback_would_be_refused",
            "a local request presented no credential; allowed in report mode (AMUX_LOOPBACK_TRUST=enforce refuses it)");
    }
    Gate::Admit
}

pub fn refused() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "unauthorized",
            "reason": "loopback_not_trusted",
            "why": "a request from this machine no longer counts as the owner by being local: every worker is local too",
            "how_to_fix": "the amux CLI and hooks send the right credential (the owner token, or a worker's AMUX_WORKER_TOKEN); a script can use `amux get <path>` or send X-Amux-Worker-Token with X-Amux-Session",
            "turn_off": format!("{MODE_ENV}=report (or off) in server.env, then restart"),
        })),
    )
        .into_response()
}

/// Stamp the owner mark `human_proof` reads. Only `require_bearer` calls this.
pub fn stamp_owner(headers: &mut HeaderMap) {
    headers.insert(super::human_proof::OWNER_PROOF_HEADER, HeaderValue::from_static("1"));
}

/// The A5 diagnostic, served in `GET /api/contract/counters` under `loopback`.
pub fn report(owner_token_configured: bool) -> Value {
    use counters::*;
    let considered = get(&CONSIDERED);
    let door = front_door_active();
    let enforceable = owner_token_configured || door;
    let callers: Value = CALLERS
        .get()
        .and_then(|m| m.lock().ok().map(|m| {
            let mut v: Vec<(String, u64)> = m.iter().map(|(k, n)| (k.clone(), *n)).collect();
            v.sort_by_key(|e| std::cmp::Reverse(e.1));
            Value::Array(v.into_iter().map(|(k, n)| json!({"caller": k, "count": n})).collect())
        }))
        .unwrap_or_else(|| json!([]));
    json!({
        "mode": mode().as_str(),
        "measured": true,
        "n_considered": considered,
        "since": "server start",
        "owner_token_configured": owner_token_configured,
        "front_door_active": door,
        "enforceable": enforceable,
        "requests": {
            "considered": considered,
            "worker_token": get(&WORKER_TOKEN),
            "would_refuse": get(&WOULD_REFUSE),
            "refused": get(&REFUSED),
            "front_door_owner_proofs": get(&FRONT_DOOR_VALID),
            "front_door_invalid_proofs": get(&FRONT_DOOR_INVALID),
        },
        "uncredentialed_callers": callers,
        "how_to_enforce": if enforceable {
            format!("{MODE_ENV}=enforce in server.env, then restart, once would_refuse stays 0 (uncredentialed_callers names who is left)")
        } else {
            "not on this server yet: it has no owner token and no front door has proven the owner, so local callers cannot be told apart".to_string()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn the_legacy_knob_means_enforce_and_the_default_is_report() {
        assert_eq!(mode_from(None, None), Mode::Report);
        assert_eq!(mode_from(Some("enforce"), None), Mode::Enforce);
        assert_eq!(mode_from(Some("off"), None), Mode::Off);
        assert_eq!(mode_from(Some("off"), Some("1")), Mode::Enforce);
    }

    #[test]
    fn an_uncredentialed_local_request_is_counted_in_report_and_refused_in_enforce() {
        let hdr = h(&[("user-agent", "curl/8.5.0")]);
        assert_eq!(gate_in(Mode::Report, &hdr, "/api/board", "POST", true, false), Gate::Admit);
        assert_eq!(gate_in(Mode::Enforce, &hdr, "/api/board", "POST", true, false), Gate::Refuse);
        assert_eq!(gate_in(Mode::Off, &hdr, "/api/board", "POST", true, false), Gate::Admit);
        // Exempt (public path or the dashboard shell) passes in every mode.
        assert_eq!(gate_in(Mode::Enforce, &hdr, "/pub/x/", "GET", true, true), Gate::Admit);
        let r = report(true);
        let callers = r["uncredentialed_callers"].as_array().unwrap();
        assert!(callers.iter().any(|c| c["caller"].as_str().unwrap().starts_with("curl /api/board lane=- peer=loopback")), "{r}");
    }

    #[test]
    fn path_shapes_hide_ids() {
        assert_eq!(path_shape("/api/sessions/wexus/report"), "/api/sessions/*/report");
        assert_eq!(path_shape("/api/board"), "/api/board");
        assert_eq!(path_shape("/api/board/AMUX-36"), "/api/board/*");
    }
}
