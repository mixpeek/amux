//! Continuous invariant checking (AMUX-2622).
//!
//! > amux should continuously prove to itself that its UI, API, runtime,
//! > queues, workers, providers, backends, persistence and derived views
//! > agree. When they do not, detect the contradiction, preserve evidence,
//! > surface the root cause, and where safe repair automatically.
//!
//! # Why this exists in this shape
//!
//! Every recent cutover incident had the same structure: two subsystems each
//! looked healthy on its own and disagreed with each other, and nothing was
//! looking at the SEAM. `amux send` returned exit 0 while delivering raw
//! keystrokes; the steering queue accepted writes forever with no consumer;
//! `server.env` held values that never reached `std::env::var`; a route
//! answered 200 on one spelling and 405 on the other. None of those is
//! detectable by a component health check, because no component was broken.
//!
//! So the unit here is a CONTRADICTION between two sources of truth, and every
//! check must name both sides (`expected` / `observed`) rather than returning a
//! bare bool.
//!
//! # Three rules this module enforces on itself
//!
//! 1. **`Unknown` is not `Pass`.** A probe that could not run reports
//!    [`Status::Unknown`], never a cheerful pass. Turning a failed observation
//!    into healthy-looking empty state is the single most repeated bug in this
//!    codebase's history (`running=0` indistinguishable from an empty fleet;
//!    `unwrap_or_default()` in observability paths). [`Status::Unknown`] is
//!    surfaced distinctly all the way to the dashboard.
//!
//! 2. **A check that cannot fail is not a check.** Every invariant registered
//!    here must ship with a negative-control test that INJECTS the failure and
//!    asserts detection (AMUX-2624). `cargo test invariant_negative_control`
//!    is the gate; see the tests at the bottom of each check module.
//!
//! 3. **Evaluations are recorded, not just failures.** `_amux_invariant_result`
//!    gets a row per evaluation so "this check stopped running" is visible.
//!    A monitor that dies silently is worse than no monitor, because its
//!    silence is read as health.

pub mod checks;
pub mod monitor;
pub mod store;

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Outcome of one invariant evaluation.
///
/// `Unknown` is deliberately NOT an error and NOT a pass: it means the probe
/// could not reach a verdict (dependency down, data absent, feature disabled).
/// Collapsing it into either direction is how a broken observer starts
/// reporting health — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    Unknown,
    Skipped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Unknown => "unknown",
            Status::Skipped => "skipped",
        }
    }
    /// Only an outright `Fail` opens an incident. `Unknown` is surfaced in the
    /// health payload but does not page: an unreachable probe is a gap in
    /// observation, not proof of a broken system, and treating it as a failure
    /// would make every transient dependency blip look like corruption.
    pub fn opens_incident(self) -> bool {
        matches!(self, Status::Fail)
    }
}

/// One evaluation of one invariant, for one entity.
///
/// `expected` and `observed` are mandatory in spirit: a `Fail` that does not
/// say what disagreed with what leaves the reader to re-derive the comparison,
/// which is exactly the manual correlation this system exists to remove.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantResult {
    pub invariant_id: String,
    pub status: Status,
    /// Stable key for the thing being checked ("" for fleet-wide). Part of the
    /// incident identity, so two workers failing the same check are two
    /// incidents rather than one flapping one.
    pub entity_key: String,
    pub expected: String,
    pub observed: String,
    /// JSON blob: the causal slice a human needs to not re-investigate.
    pub evidence: serde_json::Value,
}

impl InvariantResult {
    pub fn pass(id: &str) -> Self {
        Self::new(id, Status::Pass)
    }
    pub fn new(id: &str, status: Status) -> Self {
        Self {
            invariant_id: id.to_string(),
            status,
            entity_key: String::new(),
            expected: String::new(),
            observed: String::new(),
            evidence: serde_json::Value::Null,
        }
    }
    /// A failure that names both sides of the contradiction.
    pub fn fail(id: &str, expected: impl Into<String>, observed: impl Into<String>) -> Self {
        Self {
            invariant_id: id.to_string(),
            status: Status::Fail,
            entity_key: String::new(),
            expected: expected.into(),
            observed: observed.into(),
            evidence: serde_json::Value::Null,
        }
    }
    /// The probe could not reach a verdict. `why` is required because an
    /// unexplained Unknown is indistinguishable from a check nobody wrote.
    pub fn unknown(id: &str, why: impl Into<String>) -> Self {
        Self {
            invariant_id: id.to_string(),
            status: Status::Unknown,
            entity_key: String::new(),
            expected: String::new(),
            observed: why.into(),
            evidence: serde_json::Value::Null,
        }
    }
    pub fn entity(mut self, key: impl Into<String>) -> Self {
        self.entity_key = key.into();
        self
    }
    pub fn evidence(mut self, ev: serde_json::Value) -> Self {
        self.evidence = ev;
        self
    }

    /// DECLARE THAT THIS FAILURE'S ONLY EXIT IS A CLOCK (AMUX-3645).
    ///
    /// A DWELL-WINDOW check holds itself red on purpose for a fixed period, so
    /// the thing it names cannot scroll away unacknowledged, and then heals by
    /// the calendar. `no_fresh_kernel_panic` is the specimen: a `.panic`
    /// artifact fails the check for `AMUX_PANIC_FRESH_S` (7 days) and no action
    /// by anyone shortens that.
    ///
    /// Nothing distinguished such a failure from an escalating one, and the
    /// difference is the whole disposition. AMUX-3640 was auto-filed reading
    /// "has been failing across 5301 evaluations and has not self-healed" —
    /// two claims that are TRUE and both read as a fault getting worse. It has
    /// not self-healed because it is not due to; the occurrence count is the
    /// evaluation rate times the dwell, so it measures how often we looked, not
    /// how bad it is. Establishing that took reading the check's source. The
    /// card could not express the discriminator (ethos rule 4), so the only
    /// dispositions left were to assert an outcome for something that has not
    /// concluded, or to discard the card — which is exactly the scrolling away
    /// the 7-day window exists to prevent.
    ///
    /// `ts` is the unix epoch at which the check will next pass ON ITS OWN.
    /// Declared by the check because the check is the only thing that knows;
    /// read back with [`heals_at_of`] so the producer and every consumer share
    /// one accessor instead of a stringly key each spells for itself.
    pub fn heals_at(mut self, ts: f64) -> Self {
        match &mut self.evidence {
            serde_json::Value::Object(map) => {
                map.insert(EVIDENCE_HEALS_AT.into(), json!(ts));
            }
            serde_json::Value::Null => {
                self.evidence = json!({ EVIDENCE_HEALS_AT: ts });
            }
            // A non-object blob is nested rather than dropped: losing a check's
            // evidence to add a field would trade the diagnosis for the label.
            other => {
                let prev = std::mem::replace(other, serde_json::Value::Null);
                self.evidence = json!({ EVIDENCE_HEALS_AT: ts, "evidence": prev });
            }
        }
        self
    }
}

/// Evidence key for [`InvariantResult::heals_at`]. Private spelling, public
/// accessors: `heals_at()` writes it and [`heals_at_of`] reads it, so a rename
/// is one edit rather than a hunt through every consumer.
const EVIDENCE_HEALS_AT: &str = "heals_at";

/// The declared self-heal epoch of a dwell-window failure, if the check
/// declared one. `None` for every ordinary failure, which is the answer that
/// means "an action is what closes this".
pub fn heals_at_of(evidence: &serde_json::Value) -> Option<f64> {
    evidence.get(EVIDENCE_HEALS_AT)?.as_f64()
}

/// Overall confidence for a health surface (spec §25).
///
/// Ordering matters: `Unhealthy` outranks `Degraded` outranks `Unknown`
/// outranks `Healthy` when rolling up, so a single hard failure is never
/// averaged away by a majority of passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Healthy,
    Unknown,
    Degraded,
    Unhealthy,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Healthy => "healthy",
            Confidence::Unknown => "unknown",
            Confidence::Degraded => "degraded",
            Confidence::Unhealthy => "unhealthy",
        }
    }
}

/// Last confidence the monitor rolled up, with the epoch it was computed at
/// (AMUX-2625 item 2). `/health` reads this — a cheap mutex, no DB, no
/// dependency on the monitor being alive — so the ONE endpoint every consumer
/// polls can express the four states. The staleness gate below is the whole
/// point: an old verdict is `Unknown`, never a stale `Healthy`, because a dead
/// monitor rendering "all good" is exactly the invisible-failure this card is
/// about.
fn last_conf_cell() -> &'static std::sync::Mutex<Option<(Confidence, f64)>> {
    static C: std::sync::OnceLock<std::sync::Mutex<Option<(Confidence, f64)>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(None))
}

/// Called by the monitor after each roll-up.
pub fn record_confidence(c: Confidence, ts: f64) {
    if let Ok(mut g) = last_conf_cell().lock() {
        *g = Some((c, ts));
    }
}

/// `(confidence, age_s)` for `/health`. Evidence older than
/// `AMUX_HEALTH_CONF_STALE_S` (default 120s — a few monitor ticks) collapses to
/// `Unknown` while still reporting its age, so a wedged monitor shows as
/// `unknown` with a growing age rather than a frozen `healthy`. Never recorded
/// yet -> `Unknown` with no age.
pub fn health_confidence(now: f64) -> (Confidence, Option<f64>) {
    let stale = std::env::var("AMUX_HEALTH_CONF_STALE_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(120.0);
    match last_conf_cell().lock().ok().and_then(|g| *g) {
        Some((c, ts)) => {
            let age = (now - ts).max(0.0);
            if age > stale {
                (Confidence::Unknown, Some(age))
            } else {
                (c, Some(age))
            }
        }
        None => (Confidence::Unknown, None),
    }
}

/// Roll a set of results into one confidence verdict.
///
/// An empty set is `Unknown`, NOT `Healthy` — "nothing ran" must never render
/// as "all good", which is the failure mode that makes a dead monitor
/// invisible.
pub fn rollup(results: &[InvariantResult]) -> Confidence {
    if results.is_empty() {
        return Confidence::Unknown;
    }
    let mut worst = Confidence::Healthy;
    for r in results {
        let c = match r.status {
            Status::Fail => Confidence::Unhealthy,
            Status::Unknown => Confidence::Unknown,
            Status::Pass | Status::Skipped => Confidence::Healthy,
        };
        if c > worst {
            worst = c;
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rollup's whole job is to not average a failure away.
    #[test]
    fn one_failure_outranks_a_majority_of_passes() {
        let mut rs: Vec<InvariantResult> = (0..50).map(|_| InvariantResult::pass("x")).collect();
        assert_eq!(rollup(&rs), Confidence::Healthy);
        rs.push(InvariantResult::fail("x", "a", "b"));
        assert_eq!(
            rollup(&rs),
            Confidence::Unhealthy,
            "50 passes must not dilute 1 failure"
        );
    }

    /// "Nothing ran" is the dead-monitor case and must not read as healthy.
    #[test]
    fn empty_is_unknown_not_healthy() {
        assert_eq!(rollup(&[]), Confidence::Unknown);
    }

    /// /health's confidence read (AMUX-2625 item 2): fresh verdict passes
    /// through, a STALE one collapses to Unknown (a wedged monitor must not
    /// freeze on `healthy`), and the age is always reported once anything has
    /// been recorded. Serialised because it mutates the process-global cell.
    #[test]
    fn health_confidence_is_fresh_through_stale_unknown() {
        std::env::set_var("AMUX_HEALTH_CONF_STALE_S", "120");
        record_confidence(Confidence::Healthy, 1_000.0);
        // 30s old -> passes through.
        let (c, age) = health_confidence(1_030.0);
        assert_eq!(c, Confidence::Healthy);
        assert_eq!(age, Some(30.0));
        // 200s old (> 120 stale) -> Unknown, but the age still shows so a
        // wedged monitor is visible rather than silently green.
        let (c, age) = health_confidence(1_200.0);
        assert_eq!(c, Confidence::Unknown);
        assert_eq!(age, Some(200.0));
        std::env::remove_var("AMUX_HEALTH_CONF_STALE_S");
    }

    /// Unknown must survive the rollup rather than being rounded to a pass —
    /// this is the `unwrap_or_default()` failure class in miniature.
    #[test]
    fn unknown_is_not_rounded_to_healthy() {
        let rs = vec![
            InvariantResult::pass("a"),
            InvariantResult::unknown("b", "probe unreachable"),
        ];
        assert_eq!(rollup(&rs), Confidence::Unknown);
    }

    /// ...but a real failure still outranks an unknown, so a degraded probe
    /// cannot mask a proven contradiction.
    #[test]
    fn failure_outranks_unknown() {
        let rs = vec![
            InvariantResult::unknown("b", "probe unreachable"),
            InvariantResult::fail("a", "1", "2"),
        ];
        assert_eq!(rollup(&rs), Confidence::Unhealthy);
    }
}
