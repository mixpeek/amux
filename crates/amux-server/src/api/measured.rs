//! The diagnostic-response contract: did the measurement RUN? (AF-320)
//!
//! `frustrations.md` carries 41 `AREA: instruments` entries out of 83, with 24
//! more archived. Every one has the same shape: a clean value that is wrong,
//! and nothing beside it saying the probe could not have produced a right one.
//! A zero that means "nothing is broken" and a zero that means "the scan never
//! ran" are the same three characters on the screen.
//!
//! Ethos rule 4 already names the remedy — any output that can read zero or
//! empty must publish, in the same payload, whether the measurement ran. That
//! rule asks people to remember, so half the ledger is people not remembering.
//! These two functions are the mechanical half, and
//! `tests/diagnostic_contract.rs` walks `ROUTE_TABLE` and fails when a new
//! diagnostic endpoint ships without them.
//!
//! ```ignore
//! // the probe ran, over 4210 log rows
//! j200(measured(json!({ "total_errors": 0, "groups": [] }), rows_scanned))
//! // the probe could not run
//! j200(unmeasured(json!({ "total_errors": 0, "groups": [] }), "no server log on disk yet"))
//! ```
//!
//! `n_considered` is HOW MANY THINGS THE ANSWER IS DRAWN FROM, not how many it
//! returned. Those come apart exactly where it matters: "0 errors out of 4210
//! rows" and "0 errors out of 0 rows" are different answers and the returned
//! count cannot tell them apart. AEAB-42's disk ranker is the specimen — it
//! could never have named the 1.8GB file, and the ranking it did return looked
//! perfectly well-formed.

use serde_json::{json, Value};

/// The measurement ran. `n_considered` is the size of the population it looked
/// at, which is the number that makes an empty result readable.
///
/// Stamps onto the body rather than wrapping it: a wrapper would move every
/// existing field down a level and break every reader, and the whole point is
/// that this reaches endpoints nobody is going to revisit.
pub fn measured(body: Value, n_considered: usize) -> Value {
    stamp(body, true, n_considered, None)
}

/// The measurement could NOT run, and `why` says what stopped it.
///
/// `n_considered` is 0 here by construction: a probe that did not run
/// considered nothing. The distinguishing field is `why_unmeasured`, which is
/// absent on the measured arm, so the two are told apart by shape and not only
/// by reading a boolean.
pub fn unmeasured(body: Value, why: &str) -> Value {
    stamp(body, false, 0, Some(why))
}

/// The measured arm when the population size is itself unknown.
///
/// Rare and deliberately awkward to reach: it means the probe ran but cannot
/// say over what, which is a weaker claim than either other arm and should
/// prompt the question of whether the count is really unavailable.
pub fn measured_unknown_population(body: Value, why: &str) -> Value {
    let mut v = stamp(body, true, 0, None);
    if let Some(o) = v.as_object_mut() {
        o.insert("n_considered_unknown_because".into(), json!(why));
    }
    v
}

fn stamp(mut body: Value, measured: bool, n_considered: usize, why: Option<&str>) -> Value {
    // A non-object body has nowhere to carry the contract. Rather than silently
    // dropping it — which would reproduce this module's own bug at the level of
    // the module — move the report under `report` and stamp the wrapper.
    if !body.is_object() {
        body = json!({ "report": body });
    }
    if let Some(o) = body.as_object_mut() {
        o.insert("measured".into(), json!(measured));
        o.insert("n_considered".into(), json!(n_considered));
        match why {
            Some(w) => {
                o.insert("why_unmeasured".into(), json!(w));
            }
            // Removed, not left alone: a handler that stamps `unmeasured` on one
            // path and `measured` on another through the same body must not
            // leave a stale reason attached to a successful measurement.
            None => {
                o.remove("why_unmeasured");
            }
        }
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_preserves_the_report_and_both_arms_differ_in_shape() {
        let m = measured(json!({"groups": [], "total_errors": 0}), 4210);
        assert_eq!(m["measured"], json!(true));
        assert_eq!(m["n_considered"], json!(4210));
        assert_eq!(m["total_errors"], json!(0), "the report's own fields survive");
        assert!(m.get("why_unmeasured").is_none());

        let u = unmeasured(json!({"groups": [], "total_errors": 0}), "no log on disk");
        assert_eq!(u["measured"], json!(false));
        assert_eq!(u["n_considered"], json!(0));
        assert_eq!(u["why_unmeasured"], json!("no log on disk"));
    }

    /// THE CASE THE WHOLE MODULE IS FOR. Two reports that are byte-identical
    /// where a reader looks, and different where it matters.
    #[test]
    fn an_empty_result_and_an_unrun_probe_are_distinguishable() {
        let nothing_wrong = measured(json!({"failures": []}), 312);
        let never_ran = unmeasured(json!({"failures": []}), "the invariant monitor is not running");
        assert_eq!(nothing_wrong["failures"], never_ran["failures"], "identical where anyone looks");
        assert_ne!(nothing_wrong["measured"], never_ran["measured"]);
        assert_ne!(nothing_wrong["n_considered"], never_ran["n_considered"]);
    }

    /// A stale reason must not survive onto a successful measurement.
    #[test]
    fn re_stamping_a_body_clears_the_previous_reason() {
        let first = unmeasured(json!({"x": 1}), "tmux was not reachable");
        let second = measured(first, 7);
        assert_eq!(second["measured"], json!(true));
        assert!(
            second.get("why_unmeasured").is_none(),
            "a measured report carrying a why_unmeasured is the ambiguity this module removes: {second}"
        );
    }

    #[test]
    fn a_non_object_report_still_carries_the_contract() {
        let v = measured(json!([1, 2, 3]), 3);
        assert_eq!(v["measured"], json!(true));
        assert_eq!(v["n_considered"], json!(3));
        assert_eq!(v["report"], json!([1, 2, 3]));
    }

    #[test]
    fn the_unknown_population_arm_says_so_rather_than_claiming_zero() {
        let v = measured_unknown_population(json!({"verdict": "ok"}), "the source is a stream");
        assert_eq!(v["measured"], json!(true));
        assert_eq!(v["n_considered"], json!(0));
        assert_eq!(v["n_considered_unknown_because"], json!("the source is a stream"));
    }
}
