//! A request with no proven identity is anonymous, never the owner.
//!
//! WHAT THIS REPLACES. Four places decided "this is the human" from the
//! ABSENCE of `X-Amux-Session` / `X-Amux-Worker`: grant approve and reject,
//! vault item changes, the policy engine's `api-anonymous` (read as Trusted),
//! and the held-email release (which took a NAME from `X-Amux-Approver` and
//! recorded `approver_verified: false`). A worker produces an absence at will,
//! so each of those was a decision a worker could make by sending fewer
//! headers (AUTOD-48 is the measured instance on the email gate).
//!
//! WHAT COUNTS AS PROOF. Only what the server itself verified on this request:
//! - the owner bearer, compared in constant time by `auth::require_bearer`,
//!   which then stamps [`OWNER_PROOF_HEADER`];
//! - a member cookie verified against `org_members` (`org::local_member_identity`);
//! - an automated owner-configured policy calling a handler in-process
//!   (`org::policy_headers`), such as the needs-input auto-approver or a
//!   scheduled send releasing what a person already approved.
//!
//! All three markers are stripped from inbound requests by the outermost
//! middleware, so a client cannot assert them.
//!
//! WHAT A FAILED CHECK COSTS is `AMUX_ANONYMOUS_IS_OWNER` (A11: this sits on
//! every approval path, so it ships able to report before it refuses, and able
//! to turn off):
//! - `report` (default): allowed as before, counted, logged once per gate
//!   with verdict `anonymous_would_be_refused`;
//! - `enforce`: refused 403 `anonymous_refused`;
//! - `off`: not checked or counted (the previous behaviour).
//!
//! A HOST WITH NO OWNER CREDENTIAL CANNOT ENFORCE. With `AMUX_AUTH_TOKEN=none`
//! (a cloud desktop behind its sign-in door, a cloud container behind the
//! gateway) the owner's own requests arrive with no credential amux can check,
//! so refusing the unproven would refuse the owner. There `enforce` keeps
//! allowing, counts `unenforceable` and logs `anonymous_enforce_unavailable`
//! once, rather than claiming a boundary it cannot hold (ethos 3). Forwarding
//! a verifiable owner proof through the door is governance PR 3.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

pub use super::worker_identity::Mode;

pub const MODE_ENV: &str = "AMUX_ANONYMOUS_IS_OWNER";

/// Stamped by `auth::require_bearer` when the request presented the owner
/// bearer. Stripped from inbound requests (org::local_member_identity and
/// require_bearer itself), so only the server can produce it.
pub(crate) const OWNER_PROOF_HEADER: &str = "x-amux-internal-owner-proof";

/// Whether this server has an owner credential at all. Set once at start
/// from `AppState::auth_token`; read by handlers that hold no state.
static OWNER_CREDENTIAL_CONFIGURED: AtomicBool = AtomicBool::new(false);

pub fn set_owner_credential_configured(configured: bool) {
    OWNER_CREDENTIAL_CONFIGURED.store(configured, Ordering::Relaxed);
}

/// An owner token, or a front door that has proven the owner here
/// (governance PR 3, `loopback_trust.rs`): either way the owner's requests
/// carry something the server checked, so the unproven can be refused.
fn owner_credential_configured() -> bool {
    OWNER_CREDENTIAL_CONFIGURED.load(Ordering::Relaxed) || super::loopback_trust::front_door_active()
}

pub fn mode() -> Mode {
    Mode::parse(std::env::var(MODE_ENV).ok().as_deref())
}

/// A human (or an owner-configured policy acting for one) the server verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proven {
    /// Presented the owner bearer.
    Owner,
    /// A verified invited member: `member:<email>`.
    Member(String),
    /// An automated owner-configured policy, by name.
    Policy(String),
}

impl Proven {
    /// The name the audit records. `owner-token` is `auth::OWNER_TOKEN_ACTOR`,
    /// the spelling the policy receipts already use.
    pub fn actor(&self) -> String {
        match self {
            Proven::Owner => super::auth::OWNER_TOKEN_ACTOR.to_string(),
            Proven::Member(m) | Proven::Policy(m) => m.clone(),
        }
    }
}

/// What the server verified about this request, if anything.
pub fn proven(headers: &HeaderMap) -> Option<Proven> {
    if let Some(p) = super::org::policy_actor(headers) {
        return Some(Proven::Policy(p.to_string()));
    }
    if let Some(m) = super::org::local_member_actor(headers) {
        return Some(Proven::Member(m.to_string()));
    }
    headers
        .get(OWNER_PROOF_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "1")
        .then_some(Proven::Owner)
}

/// Counts since this process started, for the A5 diagnostic.
pub(crate) mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static CONSIDERED: AtomicU64 = AtomicU64::new(0);
    pub static PROVEN: AtomicU64 = AtomicU64::new(0);
    pub static WOULD_REFUSE: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static UNENFORCEABLE: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// Log a (gate, kind) once per process; the counters carry every instance, so
/// a dashboard polling an unproven path does not become a log (ethos 5).
fn first_time(gate: &str, kind: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .map(|mut s| s.insert(format!("{gate}\u{0}{kind}")))
        .unwrap_or(false)
}

/// The outcome of asking "may this unproven request act as the owner here?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The server verified who this is.
    Proven(Proven),
    /// Not proven, and allowed anyway: `off`, `report`, or `enforce` on a host
    /// with no owner credential. The audit must not call this a human.
    Unproven,
    /// Not proven, and refused (`enforce` on a host that has a credential).
    Refused,
}

/// Decide for a request that carries no worker origin. `gate` names the call
/// site in logs and counts (`grant_approve`, `vault_update`, ...).
pub fn check(headers: &HeaderMap, gate: &'static str) -> Verdict {
    #[cfg(test)]
    if let Some((m, c)) = test_override::get() {
        return check_with(headers, gate, m, c);
    }
    check_with(headers, gate, mode(), owner_credential_configured())
}

/// Per-thread (mode, credential) for handler tests: env is process-global and
/// cargo runs tests in parallel; a `#[tokio::test]` runs on one thread.
#[cfg(test)]
pub(crate) mod test_override {
    use super::Mode;
    use std::cell::Cell;
    thread_local!(static OVERRIDE: Cell<Option<(Mode, bool)>> = const { Cell::new(None) });
    /// Clears itself on drop, so a failing test cannot leak its mode into
    /// the next test that runs on this thread.
    pub struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            clear();
        }
    }
    #[must_use]
    pub fn set(mode: Mode, credential: bool) -> Guard {
        OVERRIDE.with(|o| o.set(Some((mode, credential))));
        Guard
    }
    pub fn clear() {
        OVERRIDE.with(|o| o.set(None));
    }
    pub fn get() -> Option<(Mode, bool)> {
        OVERRIDE.with(|o| o.get())
    }
}

pub fn check_with(headers: &HeaderMap, gate: &'static str, mode: Mode, credential: bool) -> Verdict {
    use counters::*;
    if mode == Mode::Off {
        return proven(headers).map(Verdict::Proven).unwrap_or(Verdict::Unproven);
    }
    bump(&CONSIDERED);
    if let Some(p) = proven(headers) {
        bump(&PROVEN);
        return Verdict::Proven(p);
    }
    match (mode, credential) {
        (Mode::Enforce, true) => {
            bump(&REFUSED);
            if first_time(gate, "refused") {
                tracing::warn!(gate, measured = true, n_considered = 1, verdict = "anonymous_refused",
                    "a request with no proven identity tried to act as the owner; refused (AMUX_ANONYMOUS_IS_OWNER=enforce)");
            }
            Verdict::Refused
        }
        (Mode::Enforce, false) => {
            bump(&UNENFORCEABLE);
            if first_time(gate, "unenforceable") {
                tracing::warn!(gate, measured = true, n_considered = 1, verdict = "anonymous_enforce_unavailable",
                    "enforce is set but this server has no owner credential (AMUX_AUTH_TOKEN=none), so the owner's own requests are unproven too; allowed and counted");
            }
            Verdict::Unproven
        }
        _ => {
            bump(&WOULD_REFUSE);
            if first_time(gate, "would_refuse") {
                tracing::warn!(gate, measured = true, n_considered = 1, verdict = "anonymous_would_be_refused",
                    "a request with no proven identity acted as the owner; allowed in report mode (AMUX_ANONYMOUS_IS_OWNER=enforce refuses it)");
            }
            Verdict::Unproven
        }
    }
}

/// The 403 for [`Verdict::Refused`], naming what would have been proof.
pub fn refusal(gate: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "code": "anonymous_refused",
            "gate": gate,
            "error": "this needs the owner or a signed-in member, and the request proved neither",
            "why": "a request with no worker header used to count as the owner, and a worker can send no header at will",
            "how_to_fix": "use the dashboard or the phone app (they send the owner credential), or sign in as a member",
            "turn_off": format!("{MODE_ENV}=report (or off) in server.env, then restart"),
        })),
    )
        .into_response()
}

/// The A5 diagnostic, served in `GET /api/contract/counters`.
pub fn report() -> Value {
    use counters::*;
    let considered = get(&CONSIDERED);
    let credential = owner_credential_configured();
    json!({
        "mode": mode().as_str(),
        "measured": true,
        "n_considered": considered,
        "since": "server start",
        "owner_credential_configured": credential,
        "enforceable": credential,
        "requests": {
            "considered": considered,
            "proven": get(&PROVEN),
            "would_refuse": get(&WOULD_REFUSE),
            "refused": get(&REFUSED),
            "unenforceable": get(&UNENFORCEABLE),
        },
        "how_to_enforce": if credential {
            format!("{MODE_ENV}=enforce in server.env, then restart, once would_refuse stays 0")
        } else {
            "not on this server yet: it has no owner token (AMUX_AUTH_TOKEN=none) and no front door has proven the owner here, so the owner's requests cannot be told from anonymous ones".to_string()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn owner() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(OWNER_PROOF_HEADER, HeaderValue::from_static("1"));
        h
    }

    #[test]
    fn nothing_but_a_server_stamped_marker_is_proof() {
        assert_eq!(proven(&HeaderMap::new()), None, "an absence is not a human");
        let mut named = HeaderMap::new();
        named.insert("x-amux-approver", HeaderValue::from_static("Ethan"));
        named.insert("x-amux-user-email", HeaderValue::from_static("ethan@mixpeek.com"));
        assert_eq!(proven(&named), None, "a name the client chose is not proof");
        assert_eq!(proven(&owner()), Some(Proven::Owner));
        assert_eq!(proven(&super::super::org::policy_headers("policy:test")), Some(Proven::Policy("policy:test".into())));
    }

    #[test]
    fn report_allows_and_enforce_refuses_only_where_it_can() {
        let none = HeaderMap::new();
        assert_eq!(check_with(&none, "t", Mode::Report, true), Verdict::Unproven);
        assert_eq!(check_with(&none, "t", Mode::Enforce, true), Verdict::Refused);
        assert_eq!(check_with(&none, "t", Mode::Enforce, false), Verdict::Unproven,
            "a host with no owner credential cannot tell the owner from anonymous");
        assert_eq!(check_with(&none, "t", Mode::Off, true), Verdict::Unproven);
        assert_eq!(check_with(&owner(), "t", Mode::Enforce, true), Verdict::Proven(Proven::Owner));
    }
}
