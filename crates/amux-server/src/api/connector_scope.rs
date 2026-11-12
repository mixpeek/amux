//! Who a connector token is minted for (governance PR 4).
//!
//! `POST /api/connectors/{id}/token` hands out a live bearer for a connected
//! account (Gmail, Drive, YouTube...). It used to scope the bearer by the
//! `X-Amux-Session` header: a request naming a worker got that worker's
//! entitlement, and a request naming nobody was taken for the dashboard and
//! got an unscoped token. Any process on the host could leave the header off.
//!
//! The caller is now resolved from what the server verified (governance PRs 1-3):
//! a worker's minted token, the owner proof, a member cookie, or an in-process
//! job's policy mark. A request that proves nobody is anonymous.
//!
//! `AMUX_CONNECTOR_TOKEN_SCOPE` (A11):
//! - `report` (default): allowed as before; an unproven caller is counted and
//!   logged `connector_token_would_be_refused`.
//! - `enforce`: an unproven caller gets a 403 naming how to prove itself. On a
//!   host with no owner credential the owner's own calls are unproven too, so
//!   there they are allowed and counted, never locked out (as in PR 2).
//! - `off`: the previous header behaviour, unrecorded.
//!
//! Every mint, allowed or refused, writes a receipt to `_amux_policy_receipts`
//! (`GET /api/policy/receipts`): actor, on-behalf-of, connector, account,
//! scopes, verdict. The bearer itself is never written.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::path::Path;

pub use super::worker_identity::Mode;

pub const MODE_ENV: &str = "AMUX_CONNECTOR_TOKEN_SCOPE";

pub fn mode() -> Mode {
    Mode::parse(std::env::var(MODE_ENV).ok().as_deref())
}

/// How a worker's claim to its name was proven.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerProof {
    /// It presented its own minted token.
    Token,
    /// An in-process job acted for it, naming itself with a policy mark the
    /// server strips from inbound requests (`social-publish`, ...).
    Job(String),
    /// A verified owner or member acted on the worker's behalf (the dashboard
    /// opening a worker's connector): the worker's limits still apply.
    Human(String),
    /// The lane has not relaunched since tokens were minted for it.
    Unminted,
    /// Worker identity is switched off (`AMUX_WORKER_IDENTITY=off`).
    Unenforced,
    /// It named a worker and presented no valid token for it.
    Invalid,
}

/// Who the mint is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A worker: the bearer is limited to that worker's connector scope.
    Worker { lane: String, proof: WorkerProof },
    /// A human or an owner policy the server verified: owner scope.
    Human(super::human_proof::Proven),
    /// Nobody the server could verify.
    Anonymous,
}

impl Principal {
    /// The name the receipt records as the actor.
    pub fn actor(&self) -> String {
        match self {
            Principal::Worker { lane, .. } => format!("worker:{lane}"),
            Principal::Human(p) => p.actor(),
            Principal::Anonymous => "anonymous".into(),
        }
    }

    /// Whom the actor acted for, when that differs from the actor itself.
    pub fn on_behalf_of(&self) -> Option<String> {
        match self {
            Principal::Worker { proof: WorkerProof::Job(job), .. } => Some(format!("job:{job}")),
            Principal::Worker { proof: WorkerProof::Human(h), .. } => Some(h.clone()),
            _ => None,
        }
    }

    /// The worker whose entitlement limits the bearer, if any.
    pub fn lane(&self) -> Option<&str> {
        match self {
            Principal::Worker { lane, .. } => Some(lane),
            _ => None,
        }
    }

    fn proven(&self) -> bool {
        match self {
            Principal::Worker { proof, .. } => {
                matches!(proof, WorkerProof::Token | WorkerProof::Job(_) | WorkerProof::Human(_))
            }
            Principal::Human(_) => true,
            Principal::Anonymous => false,
        }
    }
}

/// Resolve the caller from verified facts only. A worker header narrows the
/// bearer to that worker's scope; it never widens anything, so it is honoured
/// even unproven, but only a token, a job mark or a verified human proves it.
pub fn resolve(home: &Path, headers: &HeaderMap) -> Principal {
    let human = super::human_proof::proven(headers);
    let Some(lane) = super::email::hdr_worker(headers) else {
        return human.map(Principal::Human).unwrap_or(Principal::Anonymous);
    };
    let proof = match human {
        Some(super::human_proof::Proven::Policy(job)) => WorkerProof::Job(job),
        Some(h) => WorkerProof::Human(h.actor()),
        None => {
            let token = headers
                .get(super::worker_identity::TOKEN_HEADER)
                .and_then(|v| v.to_str().ok());
            match super::worker_identity::check(home, &lane, token) {
                super::worker_identity::Check::Valid => WorkerProof::Token,
                super::worker_identity::Check::Unminted => WorkerProof::Unminted,
                super::worker_identity::Check::Unenforced => WorkerProof::Unenforced,
                super::worker_identity::Check::Invalid => WorkerProof::Invalid,
            }
        }
    };
    Principal::Worker { lane, proof }
}

/// Counts since this process started, for the A5 diagnostic.
pub(crate) mod counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static CONSIDERED: AtomicU64 = AtomicU64::new(0);
    pub static PROVEN: AtomicU64 = AtomicU64::new(0);
    pub static UNMINTED: AtomicU64 = AtomicU64::new(0);
    pub static UNENFORCEABLE: AtomicU64 = AtomicU64::new(0);
    pub static WOULD_REFUSE: AtomicU64 = AtomicU64::new(0);
    pub static REFUSED: AtomicU64 = AtomicU64::new(0);
    pub fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// The gate's answer for one mint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Proven: mint within the principal's scope.
    Allow,
    /// Not proven, allowed anyway (`report`, an unminted lane, or a host that
    /// cannot enforce). The receipt says which.
    AllowUnproven(&'static str),
    /// Not proven, refused (`enforce`).
    Refuse,
}

impl Gate {
    pub fn verdict(&self) -> &'static str {
        match self {
            Gate::Allow => "proven",
            Gate::AllowUnproven(why) => why,
            Gate::Refuse => "connector_token_refused",
        }
    }
}

/// Log a kind once per process; the counters carry every instance (ethos 5).
fn first_time(kind: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .map(|mut s| s.insert(kind.to_string()))
        .unwrap_or(false)
}

pub fn gate(principal: &Principal) -> Gate {
    #[cfg(test)]
    if let Some((m, c)) = test_override::get() {
        return gate_with(principal, m, c);
    }
    gate_with(principal, mode(), super::human_proof::owner_credential_configured())
}

/// The decision, with mode and "can this host prove its owner" passed in.
pub fn gate_with(principal: &Principal, mode: Mode, credential: bool) -> Gate {
    use counters::*;
    if mode == Mode::Off {
        return Gate::Allow;
    }
    bump(&CONSIDERED);
    if principal.proven() {
        bump(&PROVEN);
        return Gate::Allow;
    }
    // A lane that has not relaunched since tokens were minted, or a host
    // with worker identity off, cannot present a token: refusing it would
    // cut every running worker off its connectors (A11).
    if let Principal::Worker { proof: WorkerProof::Unminted | WorkerProof::Unenforced, .. } = principal {
        bump(&UNMINTED);
        return Gate::AllowUnproven("worker_unminted");
    }
    let anonymous = matches!(principal, Principal::Anonymous);
    match mode {
        Mode::Enforce if !anonymous || credential => {
            bump(&REFUSED);
            if first_time("refused") {
                tracing::warn!(actor = %principal.actor(), measured = true, n_considered = 1,
                    verdict = "connector_token_refused",
                    "a connector token was asked for by a caller that proved nobody; refused ({MODE_ENV}=enforce)");
            }
            Gate::Refuse
        }
        Mode::Enforce => {
            bump(&UNENFORCEABLE);
            if first_time("unenforceable") {
                tracing::warn!(measured = true, n_considered = 1, verdict = "connector_token_enforce_unavailable",
                    "enforce is set but this server cannot prove its owner (no owner token, no front door), so the owner's own mints are unproven too; allowed and counted");
            }
            Gate::AllowUnproven("connector_token_unenforceable")
        }
        _ => {
            bump(&WOULD_REFUSE);
            if first_time("would_refuse") {
                tracing::warn!(actor = %principal.actor(), measured = true, n_considered = 1,
                    verdict = "connector_token_would_be_refused",
                    "a connector token was minted for a caller that proved nobody; allowed in report mode ({MODE_ENV}=enforce refuses it)");
            }
            Gate::AllowUnproven("connector_token_would_be_refused")
        }
    }
}

/// Per-thread (mode, credential) for handler tests: env is process-global and
/// cargo runs tests in parallel.
#[cfg(test)]
pub(crate) mod test_override {
    use super::Mode;
    use std::cell::Cell;
    thread_local!(static OVERRIDE: Cell<Option<(Mode, bool)>> = const { Cell::new(None) });
    pub struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(None));
        }
    }
    #[must_use]
    pub fn set(mode: Mode, credential: bool) -> Guard {
        OVERRIDE.with(|o| o.set(Some((mode, credential))));
        Guard
    }
    pub fn get() -> Option<(Mode, bool)> {
        OVERRIDE.with(|o| o.get())
    }
}

/// The 403, with the truthful way forward (ethos 3).
pub fn refusal(connector: &str, principal: &Principal) -> Response {
    let how = match principal {
        Principal::Worker { lane, .. } => format!(
            "this request named worker '{lane}' without its token: run the call from inside that worker \
             (the amux CLI sends AMUX_WORKER_TOKEN), or restart the worker so it is minted one"
        ),
        _ => "call from the dashboard or phone app (they send the owner credential), sign in as a member, \
              or from inside a worker with the amux CLI (it sends the worker's token)"
            .to_string(),
    };
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "code": "connector_token_refused",
            "connector": connector,
            "actor": principal.actor(),
            "error": "a connector token is a live credential, and this request proved neither a worker nor a person",
            "how_to_fix": how,
            "turn_off": format!("{MODE_ENV}=report (or off) in server.env, then restart"),
        })),
    )
        .into_response()
}

/// The receipt text for one mint. Never holds a bearer.
pub fn rationale(
    principal: &Principal,
    gate: &Gate,
    account: Option<&str>,
    scopes: Option<&str>,
    status: u16,
) -> Value {
    json!({
        "verdict": gate.verdict(),
        "on_behalf_of": principal.on_behalf_of(),
        "worker_scope": principal.lane(),
        "account": account,
        "scopes": scopes.unwrap_or("default"),
        "http_status": status,
        "mode": mode().as_str(),
    })
}

/// Write one mint receipt to `_amux_policy_receipts`. A failed write is
/// logged, never fatal: the mint's answer does not depend on the trail.
pub async fn record(
    state: &super::AppState,
    connector: &str,
    principal: &Principal,
    gate: &Gate,
    account: Option<String>,
    scopes: Option<String>,
    status: u16,
) {
    let id = format!("pol_{}", ulid::Ulid::new().to_string().to_lowercase());
    let actor = principal.actor();
    let resource = match &account {
        Some(a) => format!("connector:{connector}/account:{a}"),
        None => format!("connector:{connector}"),
    };
    let trust = if matches!(gate, Gate::Allow) { "trusted" } else { "untrusted" };
    let effect = if (200..300).contains(&status) { "allow" } else { "deny" };
    let rationale = rationale(principal, gate, account.as_deref(), scopes.as_deref(), status).to_string();
    let trace = super::trace::current();
    let (trace_id, span_id) = (trace.as_ref().map(|t| t.trace_id.clone()), trace.map(|t| t.span_id));
    let res = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO _amux_policy_receipts
                 (id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at,trace_id,span_id)
                 VALUES(?1,?2,'connector_token_mint',?3,?4,1,?5,'connector-token-scope',?6,0,?7,?8,?9)",
                rusqlite::params![id, actor, resource, trust, effect, rationale, chrono::Utc::now().to_rfc3339(),
                    trace_id, span_id],
            )?;
            Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(connector, error = %e, measured = false, verdict = "connector_token_receipt_unwritten",
            "a connector token mint was not recorded");
    }
}

/// The A5 diagnostic, served in `GET /api/contract/counters`.
pub fn report() -> Value {
    use counters::*;
    let considered = get(&CONSIDERED);
    let credential = super::human_proof::owner_credential_configured();
    json!({
        "mode": mode().as_str(),
        "measured": true,
        "n_considered": considered,
        "since": "server start",
        "enforceable": credential,
        "mints": {
            "considered": considered,
            "proven": get(&PROVEN),
            "worker_unminted": get(&UNMINTED),
            "unenforceable": get(&UNENFORCEABLE),
            "would_refuse": get(&WOULD_REFUSE),
            "refused": get(&REFUSED),
        },
        "receipts": "GET /api/policy/receipts (action connector_token_mint)",
        "turn_off": format!("{MODE_ENV}=off"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::human_proof::Proven;

    fn worker(proof: WorkerProof) -> Principal {
        Principal::Worker { lane: "w1".into(), proof }
    }

    #[test]
    fn an_unproven_caller_is_counted_in_report_and_refused_in_enforce() {
        assert_eq!(gate_with(&Principal::Anonymous, Mode::Report, true), Gate::AllowUnproven("connector_token_would_be_refused"));
        assert_eq!(gate_with(&Principal::Anonymous, Mode::Enforce, true), Gate::Refuse);
        assert_eq!(gate_with(&worker(WorkerProof::Invalid), Mode::Enforce, true), Gate::Refuse);
        assert_eq!(gate_with(&worker(WorkerProof::Invalid), Mode::Enforce, false), Gate::Refuse,
            "a forged worker claim is refused even where the owner cannot be proven");
        assert_eq!(gate_with(&Principal::Anonymous, Mode::Off, true), Gate::Allow);
    }

    #[test]
    fn proven_callers_and_running_lanes_are_never_locked_out() {
        for p in [
            worker(WorkerProof::Token),
            worker(WorkerProof::Job("job:social-publish".into())),
            worker(WorkerProof::Human("owner-token".into())),
            Principal::Human(Proven::Owner),
            Principal::Human(Proven::Member("member:a@b.c".into())),
        ] {
            assert_eq!(gate_with(&p, Mode::Enforce, true), Gate::Allow, "{p:?}");
        }
        assert_eq!(gate_with(&worker(WorkerProof::Unminted), Mode::Enforce, true), Gate::AllowUnproven("worker_unminted"));
        assert_eq!(gate_with(&Principal::Anonymous, Mode::Enforce, false), Gate::AllowUnproven("connector_token_unenforceable"),
            "a host that cannot prove its owner does not lock its owner out");
    }

    #[test]
    fn a_header_alone_never_proves_a_worker() {
        let home = tempfile::tempdir().unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-amux-session", "w1".parse().unwrap());
        let _ = super::super::worker_identity::mint(home.path(), "w1");
        let p = resolve(home.path(), &h);
        assert!(!p.proven(), "{p:?}");
        assert_eq!(p.lane(), Some("w1"), "the header still narrows the bearer to the worker's scope");
        assert_eq!(resolve(home.path(), &HeaderMap::new()), Principal::Anonymous);
    }

    #[test]
    fn the_receipt_names_the_principal_and_never_a_bearer() {
        let p = worker(WorkerProof::Job("job:social-publish".into()));
        let r = rationale(&p, &Gate::Allow, Some("x@y.z"), None, 200);
        assert_eq!(r["on_behalf_of"], "job:job:social-publish");
        assert_eq!(r["worker_scope"], "w1");
        assert_eq!(r["account"], "x@y.z");
        assert!(!r.to_string().contains("token\":"), "{r}");
    }
}
