//! Who may see and decide a held external email (governance PR 6).
//!
//! WHAT WAS OPEN. `GET /api/email/approvals` answered every caller with every
//! pending id and the worker that queued it. A worker could read another
//! worker's draft ids, and an app or a headerless curl could read all of them.
//! Approve and reject decided "is this a person" from headers: a member's own
//! session marker (`member:<email>`) read as a WORKER origin, so a signed-in
//! member could not approve at all, while reject took whatever name
//! `X-Amux-Approver` carried and logged `approver_verified: false` even for the
//! owner. Two standing bypasses, `AMUX_EMAIL_EXTERNAL_ALLOW` and the older
//! `AMUX_EMAIL_EXTERNAL_EXEMPT`, let a worker's external send skip the hold
//! with no row saying a rule had let it (A4).
//!
//! WHAT IT IS NOW.
//! - The caller is resolved from what the server verified: a proven human
//!   (owner token, a member, or an owner policy, `human_proof`), a published
//!   app (`app_identity`), a worker, or nobody.
//! - The list is scoped: the owner, a policy and a Global member see every
//!   draft; a scoped member sees the drafts of the workers in its scope; a
//!   worker sees only its own; an app or an unproven caller sees none.
//! - Approve and reject: a proven human, recorded under the identity the
//!   server proved, `approver_verified: true`, and never a name the client
//!   chose. A scoped member may decide only for workers in its scope. A worker
//!   may withdraw its own draft and nothing else, and can never approve. An
//!   app can do neither.
//! - Every use of a standing bypass leaves a receipt naming the rule and the
//!   layer it was set on, and is counted.
//!
//! A11. Approve and reject sit on the path that releases mail to customers, so
//! the new refusals ship able to report before they refuse, and able to turn
//! off, with `AMUX_EMAIL_APPROVAL_SCOPE`:
//! - `report` (default): workers, apps and unproven callers are let through as
//!   before and counted (`email_scope_would_refuse`, logged once per action);
//! - `enforce`: refused 403 with a next step;
//! - `off`: not checked (the previous behaviour).
//!
//! A scoped member's limits apply in every mode but `off`: that is access a
//! scoped member never had (the member route guard refused all of
//! `/api/email/` to them), so there is no previous behaviour to keep.
//! An unproven caller on a server with no owner credential is the owner's own
//! dashboard as far as the server can tell, so there `enforce` still lets it
//! through and counts it (`unenforceable`), as `human_proof` does.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::path::Path;

use super::human_proof::Proven;
use super::org::MemberScope;
pub use super::worker_identity::Mode;

pub const MODE_ENV: &str = "AMUX_EMAIL_APPROVAL_SCOPE";

pub fn mode() -> Mode {
    #[cfg(test)]
    if let Some(m) = test_override::get() {
        return m;
    }
    Mode::parse(std::env::var(MODE_ENV).ok().as_deref())
}

/// Per-thread mode for handler tests (env is process-global, tests run in parallel).
#[cfg(test)]
pub(crate) mod test_override {
    use super::Mode;
    use std::cell::Cell;
    thread_local!(static OVERRIDE: Cell<Option<Mode>> = const { Cell::new(None) });
    pub struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(None));
        }
    }
    #[must_use]
    pub fn set(mode: Mode) -> Guard {
        OVERRIDE.with(|o| o.set(Some(mode)));
        Guard
    }
    pub fn get() -> Option<Mode> {
        OVERRIDE.with(|o| o.get())
    }
}

/// Who is asking, from verified facts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Caller {
    /// Proven by the server. `scope` is `None` for the owner, a policy, or a
    /// Global member: every worker.
    Human { proven: Proven, scope: Option<MemberScope> },
    /// A published app (`app:<id>`).
    App(String),
    /// Named a worker. Whether the name was proven does not matter here: a
    /// worker name only ever narrows what the caller may see or do.
    Worker(String),
    /// Nothing the server could verify.
    Unproven,
}

impl Caller {
    /// The name a ledger row records for this caller.
    pub fn actor(&self) -> String {
        match self {
            Caller::Human { proven, .. } => proven.actor(),
            Caller::App(a) => a.clone(),
            Caller::Worker(w) => w.clone(),
            Caller::Unproven => "unproven".into(),
        }
    }

    /// Whether this caller may see and decide drafts queued by `worker`.
    fn covers(&self, worker: &str) -> bool {
        match self {
            Caller::Human { scope: None, .. } => true,
            Caller::Human { scope: Some(s), .. } => s.allows_worker(worker),
            Caller::Worker(w) => w.eq_ignore_ascii_case(worker),
            Caller::App(_) | Caller::Unproven => false,
        }
    }
}

pub(crate) fn resolve(headers: &HeaderMap) -> Caller {
    if let Some(app) = super::app_identity::actor(headers) {
        return Caller::App(app.to_string());
    }
    if let Some(proven) = super::human_proof::proven(headers) {
        let scope = match &proven {
            Proven::Member(_) => super::org::local_member_scope(headers).filter(|s| !s.is_global()),
            _ => None,
        };
        return Caller::Human { proven, scope };
    }
    match super::email::hdr_worker(headers) {
        Some(lane) => Caller::Worker(lane),
        None => Caller::Unproven,
    }
}

/// Counts since this process started, for the A5 diagnostic: one counter per
/// (action, verdict), plus bypass uses per rule.
pub(crate) mod counters {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};
    fn map() -> &'static Mutex<BTreeMap<String, u64>> {
        static M: OnceLock<Mutex<BTreeMap<String, u64>>> = OnceLock::new();
        M.get_or_init(Default::default)
    }
    pub fn bump(key: &str) {
        if let Ok(mut m) = map().lock() {
            *m.entry(key.to_string()).or_insert(0) += 1;
        }
    }
    pub fn snapshot() -> BTreeMap<String, u64> {
        map().lock().map(|m| m.clone()).unwrap_or_default()
    }
}

fn first_time(key: &str) -> bool {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .map(|mut s| s.insert(key.to_string()))
        .unwrap_or(false)
}

/// The answer for one list, approve or reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// In scope, or proven with no scope limit.
    Allow,
    /// Outside the caller's scope, let through by `report` (or by `enforce`
    /// on a host that cannot tell the owner from anonymous).
    WouldRefuse(&'static str),
    /// Refused.
    Refuse(&'static str),
}

/// Decide whether `caller` may act on (or, for `list`, see) a draft queued by
/// `creator`. `action` is `list`, `approve` or `reject`.
pub(crate) fn gate(caller: &Caller, action: &'static str, creator: &str) -> Gate {
    #[cfg(test)]
    if let Some((_, credential)) = super::human_proof::test_override::get() {
        return gate_with(caller, action, creator, mode(), credential);
    }
    gate_with(caller, action, creator, mode(), super::human_proof::owner_credential_configured())
}

pub(crate) fn gate_with(caller: &Caller, action: &'static str, creator: &str, mode: Mode, credential: bool) -> Gate {
    if mode == Mode::Off {
        return Gate::Allow;
    }
    let reason: &'static str = match caller {
        Caller::Human { scope: Some(_), .. } if !caller.covers(creator) => {
            // New access (scoped members were refused all of /api/email/), so
            // limited in every mode: there is nothing previous to preserve.
            counters::bump(&format!("{action}.out_of_scope"));
            return Gate::Refuse("member_out_of_scope");
        }
        Caller::Human { .. } => return Gate::Allow,
        // A worker approving is refused by the handler in every mode
        // (worker_cannot_approve); here it is listing or withdrawing.
        Caller::Worker(_) if caller.covers(creator) => return Gate::Allow,
        Caller::Worker(_) => "not_your_draft",
        Caller::App(_) => "app_cannot_decide",
        Caller::Unproven => "unproven_caller",
    };
    let refuse = match mode {
        Mode::Enforce => !matches!(caller, Caller::Unproven) || credential,
        _ => false,
    };
    let key = format!("{action}.{}", if refuse { "refused" } else if mode == Mode::Enforce { "unenforceable" } else { "would_refuse" });
    counters::bump(&key);
    if first_time(&key) {
        tracing::warn!(action, reason, caller = %caller.actor(), creator, measured = true, n_considered = 1,
            verdict = if refuse { "email_scope_refused" } else { "email_scope_would_refuse" },
            "a held-email {action} outside the caller's scope ({MODE_ENV}={})", mode.as_str());
    }
    if refuse { Gate::Refuse(reason) } else { Gate::WouldRefuse(reason) }
}

/// The 403 for a refusal, with a truthful next step (ethos 3).
pub fn refusal(action: &str, reason: &str) -> Response {
    let how = match reason {
        "member_out_of_scope" => "ask the owner or a member whose scope includes the worker that queued this draft",
        "not_your_draft" => "a worker can see and withdraw only the drafts it queued itself",
        "app_cannot_decide" => "a published app cannot see or decide held email; a person decides from the dashboard",
        _ => "use the dashboard or the phone app (they send the owner credential), or sign in as a member",
    };
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "code": reason,
            "action": action,
            "error": format!("this held-email {action} is outside what this caller may do"),
            "how_to_fix": how,
            "turn_off": format!("{MODE_ENV}=report (or off) in server.env, then restart"),
        })),
    )
        .into_response()
}

// ── Standing bypasses ───────────────────────────────────────────────────────

/// Which standing rule lets `session` send external email without a hold, if
/// one does: the key and the layer it was found on. Same resolution as
/// `email_approval::external_email_allowed`, which this backs.
pub fn bypass_rule(home: &Path, session: &str) -> Option<String> {
    const KEY: &str = "AMUX_EMAIL_EXTERNAL_ALLOW";
    let truthy = |v: &str| matches!(v.trim().trim_matches('"').to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    let worker = super::session_verbs::EnvFile::load(&home.join("sessions").join(format!("{session}.env")));
    if let Some(v) = worker.get(KEY).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        return truthy(&v).then(|| format!("{KEY} (worker {session})"));
    }
    if let Some(v) = super::session_verbs::lane_setting_in(home, session, KEY) {
        return truthy(&v).then(|| format!("{KEY} (group of {session})"));
    }
    if let Some(v) = super::session_verbs::scoped_setting_in(home, session, KEY) {
        return truthy(&v).then(|| format!("{KEY} (global amux.env)"));
    }
    super::email_approval::exempt_sessions(home)
        .contains(&session.trim().to_lowercase())
        .then(|| "AMUX_EMAIL_EXTERNAL_EXEMPT".to_string())
}

/// Record one external send that a standing rule let skip the hold: counted,
/// logged once per rule, a row in the email ledger, and a policy receipt.
pub async fn record_bypass(
    state: Option<&super::AppState>,
    home: &Path,
    session: &str,
    rule: &str,
    endpoint: &str,
    recipients: &[String],
) {
    let key_rule = rule.split(' ').next().unwrap_or(rule).to_string();
    counters::bump(&format!("bypass.{key_rule}"));
    if first_time(&format!("bypass\u{0}{rule}")) {
        tracing::warn!(session, rule, endpoint, measured = true, n_considered = 1, verdict = "email_hold_bypassed_by_rule",
            "a worker's external email skipped the approval hold under a standing rule");
    }
    crate::integrations::email::email_log(
        home,
        json!({
            "endpoint": endpoint, "hold_bypassed": true, "bypass_rule": rule,
            "session": session, "external_recipients": recipients,
        }),
    );
    let id = format!("pol_{}", ulid::Ulid::new().to_string().to_lowercase());
    let actor = format!("worker:{session}");
    let resource = format!("email:{endpoint}");
    let rule_id = if key_rule == "AMUX_EMAIL_EXTERNAL_EXEMPT" { "email-external-exempt" } else { "email-external-allow" };
    let rationale = json!({"rule": rule, "external_recipients": recipients, "endpoint": endpoint}).to_string();
    let Some(state) = state else {
        tracing::warn!(session, rule, measured = false, verdict = "email_bypass_receipt_unwritten",
            "an email hold bypass had no store to record its policy receipt in (in-process caller)");
        return;
    };
    let trace = super::trace::current();
    let (trace_id, span_id) = (trace.as_ref().map(|t| t.trace_id.clone()), trace.map(|t| t.span_id));
    let res = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "INSERT INTO _amux_policy_receipts
                 (id,actor,action,resource,trust,reversible,effect,rule_id,rationale,cost_microusd,created_at,trace_id,span_id)
                 VALUES(?1,?2,'email_external_bypass',?3,'trusted',0,'allow',?4,?5,0,?6,?7,?8)",
                rusqlite::params![id, actor, resource, rule_id, rationale, chrono::Utc::now().to_rfc3339(),
                    trace_id, span_id],
            )?;
            Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(session, rule, error = %e, measured = false, verdict = "email_bypass_receipt_unwritten",
            "an email hold bypass was not recorded in the policy receipts");
    }
}

/// The A5 diagnostic, served in `GET /api/contract/counters` as `email_gate`.
pub fn report() -> Value {
    let counts = counters::snapshot();
    let n: u64 = counts.values().sum();
    let mut bypass = serde_json::Map::new();
    let mut decisions = serde_json::Map::new();
    for (k, v) in &counts {
        match k.strip_prefix("bypass.") {
            Some(rule) => {
                bypass.insert(rule.to_string(), json!(v));
            }
            None => {
                decisions.insert(k.clone(), json!(v));
            }
        }
    }
    json!({
        "mode": mode().as_str(),
        "measured": true,
        "n_considered": n,
        "since": "server start",
        "enforceable_for_unproven": super::human_proof::owner_credential_configured(),
        "decisions": decisions,
        "bypass_uses": bypass,
        "note": "decisions count only callers outside their scope (would_refuse, refused, unenforceable, out_of_scope); bypass_uses counts external sends a standing rule let skip the hold",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(scope: MemberScope) -> Caller {
        Caller::Human { proven: Proven::Member("member:m@example.com".into()), scope: Some(scope) }
    }

    #[test]
    fn a_worker_sees_and_withdraws_only_its_own_drafts() {
        let w = Caller::Worker("alpha".into());
        assert_eq!(gate_with(&w, "list", "alpha", Mode::Enforce, true), Gate::Allow);
        assert_eq!(gate_with(&w, "list", "beta", Mode::Enforce, true), Gate::Refuse("not_your_draft"));
        assert_eq!(gate_with(&w, "reject", "beta", Mode::Report, true), Gate::WouldRefuse("not_your_draft"));
        assert_eq!(gate_with(&w, "reject", "beta", Mode::Off, true), Gate::Allow);
    }

    #[test]
    fn an_app_or_unproven_caller_decides_nothing_where_the_server_can_tell() {
        let app = Caller::App("app:abc".into());
        assert_eq!(gate_with(&app, "approve", "alpha", Mode::Enforce, false), Gate::Refuse("app_cannot_decide"),
            "an app is identified positively, so it is refused even with no owner credential");
        assert_eq!(gate_with(&Caller::Unproven, "list", "alpha", Mode::Enforce, true), Gate::Refuse("unproven_caller"));
        assert_eq!(gate_with(&Caller::Unproven, "list", "alpha", Mode::Enforce, false), Gate::WouldRefuse("unproven_caller"),
            "with no owner credential the owner's own dashboard is unproven too");
        assert_eq!(gate_with(&Caller::Unproven, "approve", "alpha", Mode::Report, true), Gate::WouldRefuse("unproven_caller"));
    }

    #[test]
    fn a_scoped_member_is_held_to_its_scope_in_every_mode_but_off() {
        let m = member(MemberScope::Worker("alpha".into()));
        assert_eq!(gate_with(&m, "approve", "alpha", Mode::Report, true), Gate::Allow);
        assert_eq!(gate_with(&m, "approve", "beta", Mode::Report, true), Gate::Refuse("member_out_of_scope"));
        assert_eq!(gate_with(&m, "approve", "beta", Mode::Off, true), Gate::Allow);
        let owner = Caller::Human { proven: Proven::Owner, scope: None };
        assert_eq!(gate_with(&owner, "approve", "beta", Mode::Enforce, true), Gate::Allow);
    }

    #[test]
    fn a_member_session_marker_is_a_human_not_a_worker() {
        let mut h = super::super::org::test_member_headers("m@example.com", false);
        // The member middleware also stamps x-amux-session = member:<email>,
        // which every handler used to read as a worker origin.
        h.insert("x-amux-session", axum::http::HeaderValue::from_static("member:m@example.com"));
        assert!(matches!(resolve(&h), Caller::Human { proven: Proven::Member(_), .. }), "{:?}", resolve(&h));
    }

    #[test]
    fn the_bypass_rule_names_its_layer() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        assert_eq!(bypass_rule(home.path(), "alpha"), None);
        std::fs::write(home.path().join("sessions/alpha.env"), "AMUX_EMAIL_EXTERNAL_ALLOW=1\n").unwrap();
        assert_eq!(bypass_rule(home.path(), "alpha").as_deref(), Some("AMUX_EMAIL_EXTERNAL_ALLOW (worker alpha)"));
        std::fs::write(home.path().join("sessions/alpha.env"), "AMUX_EMAIL_EXTERNAL_ALLOW=0\n").unwrap();
        std::fs::write(home.path().join("server.env"), "AMUX_EMAIL_EXTERNAL_EXEMPT=alpha\n").unwrap();
        assert_eq!(bypass_rule(home.path(), "alpha"), None, "an explicit worker-level no wins over the legacy list");
        std::fs::write(home.path().join("sessions/beta.env"), "CC_TAGS=x\n").unwrap();
        std::fs::write(home.path().join("server.env"), "AMUX_EMAIL_EXTERNAL_EXEMPT=beta\n").unwrap();
        assert_eq!(
            bypass_rule(home.path(), "beta"),
            super::super::email_approval::external_email_allowed(home.path(), "beta")
                .then(|| "AMUX_EMAIL_EXTERNAL_EXEMPT".to_string()),
            "the rule and the allow check agree"
        );
    }
}
