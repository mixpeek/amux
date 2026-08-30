//! `/api/health/invariants` + `/api/debug/invariants` (AMUX-2622).
//!
//! Two surfaces with different jobs, deliberately not one:
//!
//! * **health** — a rollup a human or a probe reads in one glance, carrying
//!   evidence FRESHNESS. "184/184 pass" is worthless without "checked 4s ago";
//!   a monitor that died an hour ago also reports 184/184.
//! * **debug** — live incidents with their full evidence and the raw latest
//!   evaluation per invariant, for the person actually diagnosing.

use super::AppState;
use crate::invariants::{monitor, rollup, store, Status};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/health/invariants", axum::routing::get(health))
        .route("/api/debug/invariants", axum::routing::get(debug))
}

/// GET /api/health/invariants — rollup + counts + freshness.
///
/// Evaluates FRESH rather than replaying the last stored pass: a health
/// endpoint that can only report the previous poll cannot answer "is it broken
/// right now", which is the only question worth asking it.
///
/// `?id=<invariant_id>` answers the OTHER question, which the rollup cannot
/// (AF-55): "did check X run at all?" Passes are a bare count here — by design,
/// since 409 ids would drown the one-glance rollup — so a green invariant and
/// one never wired into `evaluate_all` produce byte-identical bodies. Adding a
/// check and then confirming it is live therefore sent its own author to the
/// wrong endpoint for eight polls. The filtered form reports `ran` explicitly,
/// and on a miss it returns `known_ids` so a typo cannot masquerade as "not
/// wired in" — the two are indistinguishable from an empty result alone, and
/// the wrong one of them prompts you to go re-edit working code.
async fn health(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let results = monitor::evaluate_all(&state).await;
    if let Some(want) = q.get("id").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return Json(filtered_body(&results, want)).into_response();
    }
    let conf = rollup(&results);
    let (mut pass, mut fail, mut unknown) = (0, 0, 0);
    for r in &results {
        match r.status {
            Status::Pass => pass += 1,
            Status::Fail => fail += 1,
            Status::Unknown => unknown += 1,
            Status::Skipped => {}
        }
    }
    let live = store::live_incidents(&state.store).unwrap_or_default();
    // 200 even when unhealthy: this endpoint's job is to REPORT, and a non-2xx
    // makes generic uptime tooling retry/alert on the reporter rather than read
    // the report. The verdict is in the body, where it can be precise.
    Json(json!({
        "confidence": conf.as_str(),
        "checks": {"pass": pass, "fail": fail, "unknown": unknown, "total": results.len()},
        "live_incidents": live.len(),
        // Named explicitly so a caller cannot mistake "no failures" for
        // "everything verified" when probes could not run.
        "note": if unknown > 0 {
            "some probes could not reach a verdict — unknown is not pass"
        } else { "" },
        "failures": results.iter().filter(|r| r.status == Status::Fail).map(|r| json!({
            "invariant_id": r.invariant_id, "entity": r.entity_key,
            "expected": r.expected, "observed": r.observed,
        })).collect::<Vec<_>>(),
        "unknowns": results.iter().filter(|r| r.status == Status::Unknown).map(|r| json!({
            "invariant_id": r.invariant_id, "why": r.observed,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// The `?id=` response, split out so the test drives THE SHIPPED CODE rather
/// than a paraphrase of it.
///
/// The first version of this test re-implemented the filter inside the test and
/// asserted on that — which proves the test's own closure works and cannot catch
/// the handler doing something else. Ethos rule 7 names that exact shape, and
/// writing it down did not stop me writing it; extracting the function did.
fn filtered_body(results: &[crate::invariants::InvariantResult], want: &str) -> serde_json::Value {
    let rows: Vec<_> = results
        .iter()
        .filter(|r| r.invariant_id == want)
        .map(|r| {
            json!({
                "invariant_id": r.invariant_id, "status": r.status.as_str(),
                "entity": r.entity_key, "expected": r.expected,
                "observed": r.observed, "evidence": r.evidence,
            })
        })
        .collect();
    let ran = !rows.is_empty();
    let mut known: Vec<&str> = results.iter().map(|r| r.invariant_id.as_str()).collect();
    known.sort_unstable();
    known.dedup();
    json!({
        "invariant_id": want,
        // The load-bearing field: `results: []` alone is exactly the ambiguity
        // this parameter exists to remove.
        "ran": ran,
        "results": rows,
        "note": if ran { "" } else {
            "NOT EVALUATED this tick — either this id is not wired into evaluate_all, \
             or it is misspelled. Check known_ids. This is NOT a pass."
        },
        "known_ids": if ran { Vec::new() } else { known },
    })
}

/// GET /api/debug/invariants — live incidents + last evaluation per invariant.
async fn debug(State(state): State<AppState>) -> Response {
    let incidents = store::live_incidents(&state.store).unwrap_or_default();
    let latest = store::latest_per_invariant(&state.store).unwrap_or_default();
    // A check whose newest row is old has STOPPED RUNNING, which is a different
    // and more alarming fact than its last verdict. Surfaced as an explicit
    // list rather than left for the reader to compute from timestamps.
    let stale: Vec<_> = latest
        .iter()
        .filter(|l| l["age_s"].as_f64().unwrap_or(0.0) > 300.0)
        .cloned()
        .collect();
    // The log's own size, in the payload a human already reads (AMUX-3489:
    // it reached 8M rows / ~2GB with no surface anywhere — rule 4, the tag
    // in a store the reader never opens).
    let (log_rows, log_oldest) = store::result_log_stats(&state.store).unwrap_or((0, 0.0));
    Json(json!({
        "live_incidents": incidents,
        "latest_per_invariant": latest,
        "stale_checks": stale,
        "result_log": { "rows": log_rows, "oldest_age_s": log_oldest },
        "notes": {
            "dedupe": "one incident per (invariant_id, entity); occurrences counts repeats",
            "stale_checks": "no evaluation in >300s — the check itself may be dead, which \
                             is worse than a failing check because silence reads as health",
            "unknown": "a probe that could not reach a verdict. NOT a pass.",
            "result_log": "the evaluation log itself; store.result_log_bounded fails past \
                           the row budget (AMUX_INVARIANT_RESULT_BUDGET, default 500k)",
        }
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::filtered_body;
    use crate::invariants::InvariantResult;

    /// The `?id=` body, from the shipped function. The case that matters is the
    /// MISS: an unmatched id and a passing id must not produce the same body,
    /// because that identity is the whole defect (AF-55) — eight polls spent on
    /// a rollup that could not have answered.
    #[test]
    fn a_filtered_lookup_distinguishes_passed_from_never_ran() {
        let results = [
            InvariantResult::pass("hooks.report_hooks_wired"),
            InvariantResult::fail("queue.has_live_consumer", "a", "b"),
        ];
        let matched = |want: &str| {
            let b = filtered_body(&results, want);
            let ran = b["ran"].as_bool().unwrap();
            let known: Vec<String> = b["known_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            (ran, known, b)
        };

        let (ran, known, body) = matched("hooks.report_hooks_wired");
        assert!(ran, "a PASSING invariant must report ran=true — it is invisible in the rollup");
        assert!(known.is_empty(), "known_ids is noise once the id resolved");
        assert_eq!(body["results"][0]["status"], "pass");
        assert_eq!(body["note"], "", "a resolved id needs no caveat");

        // A typo and a genuinely-unwired check are both misses, and telling them
        // apart is the point: without known_ids the reader concludes "not wired
        // in" and goes to re-edit code that works.
        let (ran, known, body) = matched("hooks.report_hooks_wire");
        assert!(!ran, "a near-miss id must NOT be reported as having run");
        assert!(
            known.iter().any(|k| k == "hooks.report_hooks_wired"),
            "a miss must return the real ids so a typo is self-evident: {known:?}"
        );
        assert!(
            body["note"].as_str().unwrap().contains("NOT a pass"),
            "a miss must say so in words, not only by an empty array: {}",
            body["note"]
        );

        // And a FAILING invariant still RAN — `ran` is about evaluation, not verdict.
        let (ran, _, body) = matched("queue.has_live_consumer");
        assert!(ran, "a failing check ran; ran must not be a synonym for passed");
        assert_eq!(body["results"][0]["status"], "fail");
    }
}
