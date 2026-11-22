//! Execution-pickup admission against version-bound failure receipts (CW-2983a).
//!
//! PURE and not yet wired: nothing here reads the database or changes pickup.
//! Wiring is CW-2983c; recording the version on receipts is CW-2983b.
//!
//! Contract (mirrors the factory retry gate, AMUX-366):
//! * A receipt is binding only if it carries a well-formed 64-hex card version.
//! * Two failures of the SAME version refuse the third attempt.
//! * A revised version starts a fresh count.
//! * READY outcomes (review/done) are never failures.
//! * Review and recovery routing requests are always admitted: the gate limits
//!   re-execution, it must never strand a card that needs review or recovery.
//! * A failure receipt with a missing or malformed version is UNCERTAIN and
//!   holds. It is never treated as "a different version".
//! * Duplicate receipts (same non-empty run id) count once; conflicting
//!   outcomes for one run id hold.

use std::collections::HashMap;

/// Failures of one version that exhaust execution.
pub const MAX_SAME_VERSION_FAILURES: usize = 2;

/// How a pickup was routed. Only `Execution` is gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Execution,
    Review,
    Recovery,
}

/// One recorded attempt result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub run_id: Option<String>,
    pub version: Option<String>,
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Admit,
    Refuse { reason: &'static str },
}

fn is_version(v: &str) -> bool {
    v.len() == 64 && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Outcomes that are a failure of the attempt. Everything else (done, review,
/// READY, released, ...) is not counted against the version.
fn is_failure(outcome: &str) -> bool {
    matches!(
        outcome,
        "failed" | "abandoned" | "orphaned" | "WORKER_ERROR" | "INFRA" | "NO_CHANGE" | "DEFERRED"
    )
}

fn is_ready(outcome: &str) -> bool {
    matches!(outcome, "READY" | "READY_FOR_REVIEW" | "review" | "done")
}

/// Decide whether `route` may start an execution of `current_version`.
pub fn admit(receipts: &[Receipt], current_version: &str, route: Route) -> Admission {
    if route != Route::Execution {
        return Admission::Admit;
    }
    if !is_version(current_version) {
        return Admission::Refuse { reason: "card-version-uncertain" };
    }
    let mut by_run: HashMap<&str, &str> = HashMap::new();
    let mut anonymous = 0usize;
    for r in receipts {
        if is_ready(&r.outcome) || !is_failure(&r.outcome) {
            continue;
        }
        match r.version.as_deref() {
            Some(v) if is_version(v) => {
                if v != current_version {
                    continue;
                }
            }
            _ => return Admission::Refuse { reason: "failure-history-uncertain" },
        }
        match r.run_id.as_deref().filter(|id| !id.is_empty()) {
            Some(id) => {
                if let Some(prev) = by_run.insert(id, r.outcome.as_str()) {
                    if prev != r.outcome {
                        return Admission::Refuse { reason: "conflicting-run-results" };
                    }
                }
            }
            None => anonymous += 1,
        }
    }
    // A READY receipt for the same run id conflicts with a failure of it.
    for r in receipts {
        if let (Some(id), true) = (r.run_id.as_deref(), is_ready(&r.outcome)) {
            if by_run.contains_key(id) {
                return Admission::Refuse { reason: "conflicting-run-results" };
            }
        }
    }
    // Receipts with no run id cannot be de-duplicated: count each (fail closed).
    if by_run.len() + anonymous >= MAX_SAME_VERSION_FAILURES {
        return Admission::Refuse { reason: "failed-twice-same-version" };
    }
    Admission::Admit
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const V2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn rc(run: &str, version: Option<&str>, outcome: &str) -> Receipt {
        Receipt { run_id: Some(run.into()), version: version.map(Into::into), outcome: outcome.into() }
    }

    #[test]
    fn one_failure_admits_two_same_version_failures_refuse_third() {
        let one = [rc("a", Some(V1), "failed")];
        assert_eq!(admit(&one, V1, Route::Execution), Admission::Admit);
        let two = [rc("a", Some(V1), "failed"), rc("b", Some(V1), "WORKER_ERROR")];
        assert_eq!(
            admit(&two, V1, Route::Execution),
            Admission::Refuse { reason: "failed-twice-same-version" }
        );
    }

    #[test]
    fn revised_version_is_admitted_after_two_failures() {
        let two = [rc("a", Some(V1), "failed"), rc("b", Some(V1), "failed")];
        assert_eq!(admit(&two, V2, Route::Execution), Admission::Admit);
    }

    #[test]
    fn ready_outcomes_are_not_failures() {
        let rs = [
            rc("a", Some(V1), "READY_FOR_REVIEW"),
            rc("b", Some(V1), "review"),
            rc("c", Some(V1), "failed"),
        ];
        assert_eq!(admit(&rs, V1, Route::Execution), Admission::Admit);
    }

    #[test]
    fn review_and_recovery_routing_is_always_admitted() {
        let two = [rc("a", Some(V1), "failed"), rc("b", Some(V1), "failed")];
        assert_eq!(admit(&two, V1, Route::Review), Admission::Admit);
        assert_eq!(admit(&two, V1, Route::Recovery), Admission::Admit);
        assert_eq!(admit(&two, "bad", Route::Recovery), Admission::Admit);
    }

    #[test]
    fn missing_or_malformed_version_holds() {
        let rs = [rc("a", None, "failed")];
        assert_eq!(
            admit(&rs, V1, Route::Execution),
            Admission::Refuse { reason: "failure-history-uncertain" }
        );
        let rs = [rc("a", Some("zz"), "failed")];
        assert!(matches!(admit(&rs, V1, Route::Execution), Admission::Refuse { .. }));
        assert!(matches!(admit(&[], "short", Route::Execution), Admission::Refuse { .. }));
    }

    #[test]
    fn duplicate_receipts_count_once_and_conflicts_hold() {
        let dup = [rc("a", Some(V1), "failed"), rc("a", Some(V1), "failed")];
        assert_eq!(admit(&dup, V1, Route::Execution), Admission::Admit);
        let conflict = [rc("a", Some(V1), "failed"), rc("a", Some(V1), "READY")];
        assert!(matches!(admit(&conflict, V1, Route::Execution), Admission::Refuse { .. }));
    }
}
