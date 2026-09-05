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

/// GET /api/health/invariants — rollup + counts + producer liveness.
///
/// Serves the monitor's STORED results plus the PRODUCER'S liveness (AMUX-3841),
/// not a per-call re-evaluation. Re-evaluating all 527 invariants cost 2.5-3.9s
/// and varied 0.75-12.4s here, so the endpoint's own latency could not tell a
/// slow eval from a wedged one — it could not answer the question it exists to
/// answer. The monitor already evaluates every TICK_SECS and stores; this reads
/// that in ~16ms and reports whether the monitor is fresh, so stored green is
/// never served as if current while the monitor that produced it is dead.
/// `?live=1` forces a synchronous fresh pass for the caller that needs it.
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
    // ?id= re-evaluates live because only a full pass carries the known-ids set
    // that tells "this id is not wired in" from "it has not run yet". ?live=1 is
    // the explicit escape hatch for a caller that must have a synchronous fresh
    // pass. Everything else serves the monitor's STORED results (AMUX-3841): a
    // per-call re-evaluation of all 527 invariants cost 2.5-3.9s here and, worse,
    // varied 0.75-12.4s on this box, so a caller could not tell a slow eval from
    // a wedged one — an honesty problem, not a speed one. The monitor already
    // evaluates every TICK_SECS and stores; this reads that in ~16ms AND reports
    // the PRODUCER'S liveness, so stored green can never be served as if fresh
    // while the monitor that produced it is dead.
    if let Some(want) = q.get("id").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let results = monitor::evaluate_all(&state).await;
        // AF-320: a filtered body that matches nothing and a pass that never ran
        // both come back empty. n_considered is the invariants evaluated.
        let n = results.len();
        return Json(crate::api::measured::measured(filtered_body(&results, want), n))
            .into_response();
    }
    let want_live = q
        .get("live")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if want_live {
        let results = monitor::evaluate_all(&state).await;
        let n = results.len();
        return Json(crate::api::measured::measured(live_body(&results, &state), n))
            .into_response();
    }

    // DEFAULT: stored + producer liveness.
    let now = crate::config::now_f64();
    let snap = crate::runtime_jobs::registry::snapshot()
        .into_iter()
        .find(|s| s.id == crate::runtime_jobs::registry::ids::INVARIANTS);
    let (mon_state, mon_fresh) = monitor_liveness(snap.as_ref(), now);

    let latest = store::latest_per_invariant(&state.store).unwrap_or_default();
    let (mut pass, mut fail, mut unknown) = (0, 0, 0);
    for l in &latest {
        match l["status"].as_str().unwrap_or("") {
            "pass" => pass += 1,
            "fail" => fail += 1,
            "unknown" => unknown += 1,
            _ => {}
        }
    }
    let has_stored = !latest.is_empty();
    let confidence = stored_confidence(mon_fresh, has_stored, fail, unknown);
    let live = store::live_incidents(&state.store).unwrap_or_default();
    let last_age = snap.as_ref().and_then(|s| s.last_tick_at).map(|t| ((now - t).max(0.0) * 10.0).round() / 10.0);

    let body = json!({
        "source": "stored",
        "confidence": confidence,
        // TOP-LEVEL, not a field a reader has to know to check (AMUX-3841): a
        // monitor that is not `ok`/`alive` makes this endpoint say so here, and
        // `stale` is true whenever the verdict cannot be trusted as current —
        // dead/stalled/hung producer, OR nothing stored yet.
        "stale": !mon_fresh || !has_stored,
        "monitor": {
            "state": mon_state,             // ok|alive|stalled|hung|dead|not_spawned|disabled|starting
            "last_tick_age_s": last_age,    // null = never ticked; distinct from old (starting vs stalled)
            "ticks": snap.as_ref().map(|s| s.ticks).unwrap_or(0),
            "cadence_s": snap.as_ref().and_then(|s| s.interval_s),
            "stall_after_s": snap.as_ref().and_then(|s| s.interval_s)
                .map(crate::runtime_jobs::registry::stall_after_s),
        },
        "checks": {"pass": pass, "fail": fail, "unknown": unknown, "total": latest.len()},
        "live_incidents": live.len(),
        "note": if !has_stored {
            "no invariant results stored yet — the monitor has not completed a pass in this process; this is NOT healthy"
        } else if !mon_fresh {
            "the invariant MONITOR is not running fresh — these results may be stale; see monitor.state (add ?live=1 to force a synchronous re-evaluation)"
        } else if unknown > 0 {
            "some probes could not reach a verdict — unknown is not pass"
        } else { "" },
        "failures": latest.iter().filter(|l| l["status"] == "fail").map(|l| json!({
            "invariant_id": l["invariant_id"], "entity": l["entity"],
            "expected": l["expected"], "observed": l["observed"],
        })).collect::<Vec<_>>(),
        "unknowns": latest.iter().filter(|l| l["status"] == "unknown").map(|l| json!({
            "invariant_id": l["invariant_id"], "why": l["observed"],
        })).collect::<Vec<_>>(),
    });
    // AF-320. `failures: []` is THE reading a sweep takes from this endpoint,
    // and "nothing is failing" and "the monitor has never completed a pass"
    // produce the identical empty list. The prose note below already says so in
    // the !has_stored arm; this is the same clause as a field, so a caller that
    // does not read notes cannot miss it.
    Json(if has_stored {
        crate::api::measured::measured(body, latest.len())
    } else {
        crate::api::measured::unmeasured(
            body,
            "no invariant results are stored — the monitor has not completed a pass in \
             this process, so an empty `failures` is the absence of a measurement",
        )
    })
    .into_response()
}

/// The monitor's liveness verdict and whether it is fresh enough to TRUST its
/// stored results, from a registry snapshot. Takes the snapshot as a parameter
/// (`None` = the monitor is not even registered) so the TRIP arm — the half that
/// prevents serving hours-old green as fresh — has its own can-fail test, rather
/// than only inheriting registry::classify's controls through the global
/// snapshot() the handler reads. AMUX-3845: inheriting a control is not having
/// one; a test that hands this a dead/stalled snapshot and asserts non-fresh is
/// the deliberate trip neither peer could manufacture on a shared host.
fn monitor_liveness(
    snap: Option<&crate::runtime_jobs::registry::Snapshot>,
    now: f64,
) -> (&'static str, bool) {
    let facts = crate::runtime_jobs::registry::Facts {
        spawned: snap.is_some(),
        dead: snap.map(|s| s.dead).unwrap_or(false),
        disabled: snap.map(|s| s.disabled_reason.is_some()).unwrap_or(false),
        interval_s: snap.and_then(|s| s.interval_s),
        spawned_at: snap.map(|s| s.spawned_at),
        last_tick_at: snap.and_then(|s| s.last_tick_at),
        in_flight_since: snap.and_then(|s| s.in_flight_since),
        instrumented: snap.map(|s| s.ticks > 0).unwrap_or(false),
    };
    let state = crate::runtime_jobs::registry::classify(&facts, now);
    (state, matches!(state, "ok" | "alive"))
}

/// The verdict a reader trusts, from the four facts that decide it. A pure
/// function so the load-bearing rule of AMUX-3841 — a stored "healthy" must
/// NEVER be served off a monitor that is not running fresh, nor off an empty
/// store — has a test that can fail on that exact bug, rather than living only
/// inside an async handler that reads global state.
fn stored_confidence(mon_fresh: bool, has_stored: bool, fail: usize, unknown: usize) -> &'static str {
    if !mon_fresh || !has_stored {
        "unknown"
    } else if fail > 0 {
        "unhealthy"
    } else if unknown > 0 {
        "unknown"
    } else {
        "healthy"
    }
}

/// The live (?live=1) full body — a synchronous re-evaluation, the pre-AMUX-3841
/// default kept as an escape hatch. Extracted so the stored path and the live
/// path are two named things, not one function with a branch.
fn live_body(results: &[crate::invariants::InvariantResult], state: &AppState) -> serde_json::Value {
    let conf = rollup(results);
    let (mut pass, mut fail, mut unknown) = (0, 0, 0);
    for r in results {
        match r.status {
            Status::Pass => pass += 1,
            Status::Fail => fail += 1,
            Status::Unknown => unknown += 1,
            Status::Skipped => {}
        }
    }
    let live = store::live_incidents(&state.store).unwrap_or_default();
    json!({
        "source": "live",
        "confidence": conf.as_str(),
        "checks": {"pass": pass, "fail": fail, "unknown": unknown, "total": results.len()},
        "live_incidents": live.len(),
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
    })
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
    // WHERE THE HEALTH ENDPOINT'S SECONDS GO (AMUX-3836). This endpoint is 16ms
    // because it replays stored rows; `/api/health/invariants` re-evaluates and
    // costs 6.5-9.4s, which is why it files as a latency outlier on any load
    // spike. Published as the last run's breakdown, with `at` so a stale one is
    // readable as stale, and as an explicit `null` with a reason when no run has
    // happened in this process yet — never as an empty list, which would read as
    // "measured, and it costs nothing".
    let timing = monitor::last_section_timing().map(|t| {
        let mut secs: Vec<_> = t
            .sections
            .iter()
            .map(|(label, ms, n)| json!({"section": label, "ms": (ms * 10.0).round() / 10.0, "checks": n}))
            .collect();
        secs.sort_by(|a, b| {
            b["ms"].as_f64().unwrap_or(0.0).partial_cmp(&a["ms"].as_f64().unwrap_or(0.0)).unwrap()
        });
        json!({
            "total_ms": (t.total_ms * 10.0).round() / 10.0,
            "measured_age_s": (crate::config::now_f64() - t.at).max(0.0).round(),
            "by_section_slowest_first": secs,
        })
    });
    // AF-320: `latest_per_invariant: []` reads as a quiet system, and is also
    // what an unrun monitor produces. n_considered is the invariants with a
    // stored verdict.
    let n_latest = latest.len();
    Json(crate::api::measured::measured(
        json!({
        "live_incidents": incidents,
        "latest_per_invariant": latest,
        "stale_checks": stale,
        "result_log": { "rows": log_rows, "oldest_age_s": log_oldest },
        "evaluate_all_timing": timing,
        "evaluate_all_timing_absent_because": if timing.is_none() {
            "evaluate_all has not run in this process yet — this is not a zero"
        } else { "" },
        "notes": {
            "dedupe": "one incident per (invariant_id, entity); occurrences counts repeats",
            "stale_checks": "no evaluation in >300s — the check itself may be dead, which \
                             is worse than a failing check because silence reads as health",
            "unknown": "a probe that could not reach a verdict. NOT a pass.",
            "result_log": "the evaluation log itself; store.result_log_bounded fails past \
                           the row budget (AMUX_INVARIANT_RESULT_BUDGET, default 500k)",
            "evaluate_all_timing": "the LAST run's cost per section, slowest first. It is what \
                                    /api/health/invariants pays on every call, and null means no \
                                    run has happened in this process, not that it is free.",
        }
        }),
        n_latest,
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{filtered_body, monitor_liveness, stored_confidence};
    use crate::invariants::InvariantResult;
    use crate::runtime_jobs::registry::Snapshot;

    fn snap(dead: bool, ticks: u64, last_tick_at: Option<f64>, spawned_at: f64) -> Snapshot {
        Snapshot {
            id: "invariants-monitor".into(),
            kind: "loop",
            interval_s: Some(30.0), // stall_after_s(30) = 90
            spawned_at,
            ticks,
            last_tick_at,
            last_tick_ms: None,
            in_flight_since: None,
            dead,
            disabled_reason: None,
        }
    }

    /// The TRIP arm, given its own can-fail test (AMUX-3845). It previously only
    /// inherited registry::classify's controls through the global snapshot the
    /// handler reads — inheriting a control is not having one. This injects the
    /// snapshot directly and asserts the deliberate trip neither peer could
    /// manufacture on a shared host: a monitor that is NOT running fresh is
    /// reported not-fresh, and the whole chain then renders `unknown` even when
    /// every stored check passed. The fresh control must stay fresh, or the arm
    /// would fire on everything and catch nothing.
    #[test]
    fn a_dead_or_stalled_monitor_trips_not_fresh_and_never_renders_stored_green() {
        let now = 1_000_000.0;
        // Control: a live monitor that ticked 2s ago is fresh.
        let (state, fresh) = monitor_liveness(Some(&snap(false, 18, Some(now - 2.0), now - 600.0)), now);
        assert_eq!(state, "ok");
        assert!(fresh, "a monitor ticking on cadence must read fresh (the control)");

        // Trip 1 — DEAD: the driving task exited. Not fresh, whatever it stored.
        let (state, fresh) = monitor_liveness(Some(&snap(true, 18, Some(now - 2.0), now - 600.0)), now);
        assert_eq!(state, "dead");
        assert!(!fresh);

        // Trip 2 — STALLED: last tick is older than stall_after_s(30)=90s.
        let (state, fresh) = monitor_liveness(Some(&snap(false, 18, Some(now - 200.0), now - 600.0)), now);
        assert_eq!(state, "stalled");
        assert!(!fresh);

        // Trip 3 — NOT SPAWNED: no snapshot at all (the AMUX-2647 hours-lost shape).
        let (state, fresh) = monitor_liveness(None, now);
        assert_eq!(state, "not_spawned");
        assert!(!fresh);

        // The end-to-end consequence: a not-fresh monitor renders `unknown` even
        // with an all-pass stored result. This is the exact bug the trip prevents
        // — serving hours-old green as fresh — asserted, not observed.
        assert_eq!(stored_confidence(fresh, true, 0, 0), "unknown");
        // And the control chains through to a real verdict.
        assert_eq!(stored_confidence(true, true, 0, 0), "healthy");
    }

    /// The load-bearing rule of AMUX-3841, with the negative control that is the
    /// whole point: a monitor that is NOT running fresh must not let a stored
    /// all-pass render "healthy" (a dead monitor showing green is the invisible
    /// failure this card exists to kill), and an EMPTY store must not either
    /// (a fresh boot serving healthy off nothing is the same bug's other face).
    /// The control — a fresh monitor with stored passes — must still be healthy,
    /// or the check would be a detector that fires on everything and means nothing.
    #[test]
    fn stale_or_empty_never_renders_healthy_but_fresh_and_clean_does() {
        // Fresh + stored + clean -> healthy (the control that must pass).
        assert_eq!(stored_confidence(true, true, 0, 0), "healthy");
        // Fresh + stored + a failure -> unhealthy; + an unknown -> unknown.
        assert_eq!(stored_confidence(true, true, 1, 0), "unhealthy");
        assert_eq!(stored_confidence(true, true, 0, 3), "unknown");
        // Monitor NOT fresh, yet every stored check passed -> still unknown, NOT
        // healthy. This is the assertion that fails on the exact bug.
        assert_eq!(stored_confidence(false, true, 0, 0), "unknown");
        // Monitor fresh but NOTHING stored yet -> unknown, not healthy.
        assert_eq!(stored_confidence(true, false, 0, 0), "unknown");
        // A dead monitor cannot be waved through even with a stored failure count
        // of zero and no unknowns — the monitor state dominates.
        assert_eq!(stored_confidence(false, false, 0, 0), "unknown");
    }

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
