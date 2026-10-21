//! AR-135's single-flight guard on `legacy_sessions_array` only ever covered the
//! WARM path (TTL expired, cache non-empty). A COLD cache — true on every
//! restart — hit `try_lock`'s `Err` arm, found `c.json` empty, and fell through
//! to an INDEPENDENT build: one per concurrent caller, each holding a pooled
//! read connection across ~100 tmux/git subprocesses at once. That is the exact
//! N-builders-one-pool failure AR-135 exists to prevent, just gated on "cache
//! empty" instead of "TTL expired" — and worse, because a restart is exactly
//! when every dashboard/fleet client reconnects and hits this endpoint at once.
//!
//! FIRST FIX (2026-09-09 morning): a loser on a cold cache waits (bounded 3s)
//! for the in-flight build instead of racing it, falling back to an
//! independent build past the deadline. Confirmed live the SAME day,
//! afternoon: that bound alone does not bound the builder COUNT. Under
//! SUSTAINED reconnect pressure (not one instantaneous burst — the real shape
//! of a restart), every new wave of waiters can independently miss the same
//! 3s deadline and each spin up its own build, stacking faster than any of
//! them finish. read_pool_exhausted recurred in bursts for minutes; box load
//! average hit 62 on 4 cores; amux-server-rs itself sat at 400%+ CPU.
//!
//! FINAL FIX: permit exactly ONE builder. A fallback that performs the same
//! expensive fleet scrape against the same tmux server cannot rescue a slow
//! primary; it makes the shared substrate slower. Waiters serve the last
//! structurally safe snapshot, or wait up to the configured deadline on a
//! truly cold start, then fail safely instead of starting duplicate work.
//! The builder also uses a dedicated read-only SQLite connection so it cannot
//! starve cheap board requests of request-pool readers while probing tmux/git.
//!
//! Asserted on the SOURCE, matching this file's sibling
//! `sessions_list_off_runtime.rs`: a timing test for "did the stampede
//! collapse under sustained load" needs a controllable hang across ~100
//! subprocesses sustained over many seconds and would be the flakiest thing
//! in the suite. The property that regressed both times was textual — the
//! branch built an unbounded number of independent copies — and that is
//! exactly what this catches.

const SRC: &str = include_str!("../src/api/sessions_legacy.rs");

#[test]
fn cold_sessions_cache_waits_for_the_inflight_builder_instead_of_racing_it() {
    // CONTROL FIRST: if this moves or gets renamed the assertions below would
    // pass vacuously against a file that no longer contains the thing at all.
    assert!(
        SRC.contains("pub fn legacy_sessions_array"),
        "premise gone: the sync builder is not in this file any more"
    );
    assert!(
        SRC.contains("static FLIGHT: std::sync::Mutex<()>"),
        "premise gone: the single-flight guard is not in this file any more"
    );

    assert!(
        !SRC.contains("Cold start with a builder already in flight: fall through and build"),
        "the ORIGINAL cold-start failure is back: a loser on an empty cache must not build \
         independently the instant try_lock fails"
    );
}

#[test]
fn cold_sessions_cache_has_exactly_one_builder_and_does_not_lease_the_request_pool() {
    assert!(
        SRC.contains("pub fn legacy_sessions_array"),
        "premise gone: the sync builder is not in this file any more"
    );
    assert!(
        !SRC.contains("FALLBACK_FLIGHT"),
        "a fallback builder duplicates the same slow fleet scrape and recreates the overload loop"
    );
    assert!(
        SRC.contains("let conn = store.dedicated_read()?;"),
        "the heavyweight projection must not lease a request-pool reader while probing tmux/git"
    );
    assert_eq!(
        SRC.matches("build_array(&conn)").count(),
        1,
        "there must be exactly one build call site, protected by the single flight"
    );
    // AMUX-4764 / AMUX-4826: this used to assert `SRC.contains("anyhow::bail!")`,
    // and that assertion pinned the SPELLING rather than the property. When the
    // busy path was given a typed `BuilderBusy` error so the handler could answer
    // 503 instead of 500, the last `bail!` in the file disappeared and this went
    // red over a change that preserved everything it cared about.
    //
    // The property is: when the single builder is still busy past the bound, the
    // loser RETURNS AN ERROR and does not build. Assert that on the arm itself,
    // so a regression that falls through to a duplicate build is caught and a
    // rename of the error machinery is not.
    let busy_arm = {
        let at = SRC.find("match acquired {").expect("the single-flight acquire is in this file");
        let tail = &SRC[at..];
        &tail[..tail.find("\n        }\n").map(|i| i + 10).unwrap_or(tail.len())]
    };
    assert!(
        busy_arm.contains("None => {"),
        "the acquire must still have a loser arm: {busy_arm}"
    );
    assert!(
        busy_arm.contains("return Err("),
        "when the single builder is still busy past the overall bound, the loser must fail \
         safely rather than start duplicate work on an already-struggling substrate"
    );
    assert!(
        !busy_arm.contains("build_array("),
        "the loser arm must NOT build: that is the N-builders-one-pool failure this file exists \
         to prevent"
    );
    assert!(
        SRC.contains("sessions_cache_stuck"),
        "the fail-safe bail-out must log a verdict a sweep can grep for — silent failure here \
         is how the first version of this guard regressed unnoticed"
    );
    assert!(
        SRC.contains("sessions_flight_poison_recovered"),
        "a panicked builder must self-announce when the flight lock recovers"
    );
}

#[test]
fn runtime_updates_preserve_the_last_structurally_safe_snapshot() {
    let runtime_fn = SRC
        .split("pub fn invalidate_sessions_runtime_cache()")
        .nth(1)
        .and_then(|tail| tail.split("/// Git branch cache").next())
        .expect("runtime invalidation function moved or disappeared");
    assert!(runtime_fn.contains("SESSIONS_RUNTIME_EPOCH.fetch_add"));
    assert!(runtime_fn.contains("c.stamp = 0.0"));
    assert!(
        !runtime_fn.contains("c.json.clear()"),
        "a worker heartbeat must not erase the stale-while-revalidate snapshot"
    );

    let snapshot = SRC
        .split("let epoch_start = SESSIONS_EPOCH.load")
        .nth(1)
        .and_then(|tail| tail.split("let json = serde_json::to_string").next())
        .expect("session build epoch snapshot moved or disappeared");
    assert!(snapshot.contains("let runtime_epoch_start ="));
    assert!(snapshot.contains("let arr = build_array(&conn)?;"));
    assert!(
        snapshot.find("let runtime_epoch_start =") < snapshot.find("let arr = build_array(&conn)?;"),
        "runtime epoch must be captured before the data it describes is read"
    );
    assert!(
        SRC.contains("runtime_epoch: runtime_epoch_start"),
        "write-back must not tag pre-report JSON with an epoch loaded after the build"
    );
}

#[test]
fn structural_changes_fail_closed_instead_of_returning_the_raced_snapshot() {
    let writeback = SRC
        .split("let json = serde_json::to_string(&arr)?;")
        .nth(1)
        .and_then(|tail| tail.split("Ok(json)").next())
        .expect("sessions cache write-back moved or disappeared");
    // The verdict distinguishes measured structural changes from an unreadable
    // registry. A measured race refuses both the response and cache writeback.
    assert!(writeback.contains("race_verdict("));
    assert!(writeback.contains("SESSIONS_EPOCH.load"));
    assert!(writeback.contains("registry_fingerprint()"));
    let raced_arm = writeback
        .split("RaceVerdict::Raced => {")
        .nth(1)
        .and_then(|tail| tail.split("RaceVerdict::Unverifiable => {").next())
        .expect("measured race refusal arm moved or disappeared");
    assert!(raced_arm.contains("return Err(DiscoveryRaced.into());"));
    assert!(raced_arm.contains("sessions_build_raced"));
    assert!(!raced_arm.contains("ListSnapshot {"));
    assert!(!raced_arm.contains("Ok(json)"));
    let verdict = SRC
        .split("fn race_verdict(")
        .nth(1)
        .and_then(|tail| tail.split("\n}\n").next())
        .expect("race_verdict moved or disappeared");
    let epoch_changed_arm = verdict
        .split("if epoch_now != epoch_start {")
        .nth(1)
        .and_then(|tail| tail.split("\n    }").next())
        .expect("changed epoch guard moved or disappeared");
    assert!(epoch_changed_arm.contains("return RaceVerdict::Raced;"));
    assert!(verdict.contains("(Some(a), Some(b)) if a == b => RaceVerdict::Fresh"));
    assert!(verdict.contains("(Some(_), Some(_)) => RaceVerdict::Raced"));
    assert!(verdict.contains("_ => RaceVerdict::Unverifiable"));
    assert!(
        !writeback.contains("caller still gets"),
        "a response that raced an isolation/delete/config change must not be returned"
    );
}
