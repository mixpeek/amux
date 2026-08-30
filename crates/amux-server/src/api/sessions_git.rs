//! `GET /api/sessions-git` (AMUX-2599) — the bulk git map the session cards use.
//!
//! One request instead of 80. The SPA calls this on every session refresh
//! (throttled to 20s client-side) and reads three things per session: `branch`,
//! `repo`, and the `_conflict` flag it synthesizes itself by grouping on
//! `repo + branch`.
//!
//! # Shape (python parity, py:66429)
//!
//! An OBJECT keyed by session name, NOT an array:
//! `{"<name>": {"name": "<name>", "branch": "<str>", "repo": "<abs toplevel>"}}`
//! Sessions whose directory has no branch are OMITTED entirely — the client's
//! conflict grouping does a hard `continue` on a falsy `repo`, so a row with an
//! empty branch would be dead weight in every payload.
//!
//! # Why this does not shell git for the branch
//!
//! `sessions_legacy::build_array` already resolves a branch per session,
//! deduped by directory, for the session list the SPA renders alongside this.
//! Computing it a second time here would (a) double the git subprocess load on
//! a 50-session fleet and (b) create two answers to the same question that can
//! disagree — the SPA reads `sess.branch` preferentially and falls back to
//! `gi.branch`, so a disagreement would surface as a badge that changes
//! depending on which fetch landed last. So the branch is REUSED and only
//! `repo` (the one field the session list does not carry) is computed here,
//! one `rev-parse --show-toplevel` per DISTINCT directory.
//!
//! # The cache is not an optimization, it is a load ceiling
//!
//! Every open client polls this. Without the 30s TTL the git subprocess count
//! scales with (clients x sessions), which is the shape that made python add
//! the same TTL. A stale-by-30s branch is correct: branches change on commit
//! cadence, not seconds.

use super::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// py:21339 `_SESSIONS_GIT_CACHE_TTL`.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// py's `threading.Semaphore(4)` — a cap on concurrent git subprocesses so a
/// 50-session fleet cannot fork-bomb the box on one refresh.
const GIT_CONCURRENCY: usize = 4;
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

fn cached() -> Option<Value> {
    let g = CACHE.lock().ok()?;
    let (at, v) = g.as_ref()?;
    (at.elapsed() < CACHE_TTL).then(|| v.clone())
}

/// How long an EXPIRED value may still be served while a refresh runs behind it
/// (AMUX-3730). Past this, a caller waits for real data rather than being handed
/// something arbitrarily old.
///
/// The ceiling is the whole reason this is safe. Stale-while-revalidate turns a
/// visible slowness into an invisible wrongness if the background refresh keeps
/// failing — the endpoint stays fast and quietly serves last week's branches,
/// which is worse than the 2s wait it replaces. 5 minutes is ten TTLs: long
/// enough that a transient git failure or a loaded box never reaches it, short
/// enough that a persistently broken refresh becomes a wait (and a WARN) rather
/// than a lie.
const STALE_CEILING: Duration = Duration::from_secs(300);

/// The cached value and its AGE, regardless of freshness.
fn cached_any() -> Option<(Duration, Value)> {
    let g = CACHE.lock().ok()?;
    let (at, v) = g.as_ref()?;
    Some((at.elapsed(), v.clone()))
}

async fn git_toplevel(dir: &str) -> Option<String> {
    let out = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .args(["-C", dir, "rev-parse", "--show-toplevel"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// ONE REFRESH AT A TIME (AMUX-3684). The TTL bounds how OFTEN the work runs;
/// this bounds how MANY run at once, and without it the second number was the
/// number of clients polling.
///
/// A `tokio::sync::Mutex` because it is held across the `.await` on the git
/// fan-out below; the std one would block the runtime thread.
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn sessions_git(State(state): State<AppState>) -> Response {
    if let Some(v) = cached() {
        return ok(v, "hit");
    }

    // STALE-WHILE-REVALIDATE (AMUX-3730). Single-flight (below) collapsed the
    // WORK; it did not collapse the WAIT. The refresh is synchronous, so every
    // client arriving during one still blocked for the whole fan-out:
    //
    //     121 sessions, 92 DISTINCT dirs -> one `git rev-parse` each
    //     GIT_CONCURRENCY = 4            -> 23 sequential waves
    //     ~30ms per rev-parse, warm      -> ~700ms floor before any contention
    //     GIT_TIMEOUT = 2s per call      -> a loaded box stretches a wave, which
    //                                       is where the logged 13-16s came from
    //
    // That floor scales with the FLEET and nothing bounded it. So: hand back the
    // expired value at once and refresh behind it. The header of this module
    // already makes the staleness argument ("a stale-by-30s branch is correct:
    // branches change on commit"); this widens the worst case to ~2x TTL in the
    // normal case, on the same reasoning.
    //
    // `try_lock`, deliberately: if a refresh is already running we do NOT queue a
    // second one, we just serve stale. Spawning per request would rebuild the
    // stampede this endpoint was fixed for, one layer over.
    if let Some((age, v)) = cached_any() {
        if age < STALE_CEILING {
            if let Ok(guard) = REFRESH.try_lock() {
                let st = state.clone();
                tokio::spawn(async move {
                    // Hold the single-flight guard for the refresh's lifetime, so
                    // a synchronous caller arriving mid-refresh waits for THIS
                    // one instead of starting a second.
                    let _g = guard;
                    if let Err(e) = recompute(&st).await {
                        tracing::warn!(
                            marker = "sessions_git_bg_refresh_failed",
                            error = %e,
                            "background refresh failed; stale data will be served until the ceiling"
                        );
                    }
                });
            }
            return ok(v, "stale");
        }
        // Past the ceiling. Fall through and WAIT — and say so, because an
        // endpoint that silently went from 30ms to 2s is a support ticket
        // nobody can diagnose.
        tracing::warn!(
            marker = "sessions_git_stale_ceiling",
            age_s = age.as_secs(),
            "cached value is past the stale ceiling; making the caller wait for fresh data"
        );
    }

    // CACHE STAMPEDE (AMUX-3684). The TTL made this cheap on a HIT and did
    // nothing about a MISS: every concurrent request that arrived while the
    // entry was expired ran the whole thing independently — `build_array` over
    // ~120 sessions plus a `git rev-parse` per distinct checkout.
    //
    // Measured 2026-08-24, and the log makes it unarguable. Slow requests
    // arrive in tight groups of FOUR at the same second, 30 seconds apart:
    //   15:54:15  2463 2510 2372 2142 ms
    //   15:54:45  1905 1923 1793 1693 ms
    //   15:59:03  2300 1984 1939 1769 ms
    // Four dashboard clients, one TTL expiry, four full recomputes — and only
    // one of them was needed. Over 6h that is 707 of 1723 requests (41%) taking
    // over a second, max 20.2s, on an endpoint the dashboard polls constantly.
    //
    // So: the first miss does the work, the rest WAIT and serve its result. The
    // re-check after acquiring is what makes them cheap rather than merely
    // serialised — without it the losers would each still do the full work,
    // just politely one after another, which is slower than the stampede.
    let _refresh = REFRESH.lock().await;
    if let Some(v) = cached() {
        return ok(v, "hit-after-wait");
    }
    match recompute(&state).await {
        Ok(v) => ok(v, "miss"),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// The body, and the ONE place the cache disposition is reported.
///
/// The disposition rides a HEADER, never the body. The body is a bare
/// `{session: {...}}` map and the dashboard does `Object.entries()` over it, so
/// a top-level `_stale` key would render as a PHANTOM SESSION — the identical
/// shape as the incident app.js records in its own comment, where a 404's
/// `{"error":"not found"}` body became one entry whose `.repo` was undefined and
/// the branch column just went blank with no throw and no console line.
fn ok(v: Value, disposition: &str) -> Response {
    let mut r = (StatusCode::OK, Json(v)).into_response();
    if let Ok(hv) = axum::http::HeaderValue::from_str(disposition) {
        r.headers_mut().insert("x-amux-cache", hv);
    }
    r
}

/// Rebuild the map and store it. Callable from the request path AND from a
/// background task, which is the whole point: the two must not drift, so there
/// is one function rather than a handler and a copy of it.
///
/// The CALLER owns the single-flight guard. This does not take it, so a future
/// reader cannot accidentally make the background path re-enter it.
async fn recompute(state: &AppState) -> Result<Value, String> {
    // (name, dir, branch) from the SAME source the session list renders.
    let rows: Vec<(String, String, String)> = {
        let conn = state.store.read().map_err(|e| format!("store unreadable: {e}"))?;
        match super::sessions_legacy::build_array(&conn) {
            Ok(arr) => arr
                .iter()
                .filter_map(|v| {
                    let name = v["name"].as_str()?.to_string();
                    let dir = v["dir"].as_str().unwrap_or("").to_string();
                    let branch = v["branch"].as_str().unwrap_or("").to_string();
                    (!name.is_empty() && !dir.is_empty() && !branch.is_empty())
                        .then_some((name, dir, branch))
                })
                .collect(),
            Err(e) => return Err(format!("session list unavailable: {e}")),
        }
    };

    // One git call per DISTINCT directory, then fan the answer out to every
    // session sharing it (many sessions share one checkout).
    let dirs: Vec<String> = rows
        .iter()
        .map(|(_, d, _)| d.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut repos: BTreeMap<String, String> = BTreeMap::new();
    for chunk in dirs.chunks(GIT_CONCURRENCY) {
        let results = futures::future::join_all(chunk.iter().map(|d| async move {
            (d.clone(), git_toplevel(d).await)
        }))
        .await;
        for (d, top) in results {
            if let Some(t) = top {
                repos.insert(d, t);
            }
        }
    }

    let mut out = Map::new();
    for (name, dir, branch) in rows {
        let Some(repo) = repos.get(&dir) else { continue };
        out.insert(
            name.clone(),
            json!({"name": name, "branch": branch, "repo": repo}),
        );
    }
    let body = Value::Object(out);

    if let Ok(mut g) = CACHE.lock() {
        *g = Some((Instant::now(), body.clone()));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SERIALISE THE CELLS THAT DRIVE THE GLOBAL STATICS (AMUX-3684 follow-up).
    ///
    /// `CACHE` and `REFRESH` are process-global by design — they are a load
    /// ceiling for the whole server, not per-request state — and cargo runs the
    /// tests in one binary on many threads. So three cells that each set
    /// `CACHE` to a known value and then assert on it were racing: the stampede
    /// cell caches a result and asserts it is there, while a sibling clears the
    /// same static a microsecond later.
    ///
    /// It failed once in roughly forty runs, which is the worst frequency: rare
    /// enough to read as a fluke and re-run, common enough to eventually land in
    /// CI on somebody else's PR. Caught here on a full-suite run and fixed at
    /// the shared resource rather than by retrying.
    ///
    /// A TOKIO mutex, not a std one: the guard is held across the `.await`s in
    /// the async cells below, and clippy rightly refuses that for a blocking
    /// guard. It also has no poisoning, so one cell panicking does not turn its
    /// siblings into unwrap failures that hide which one actually broke.
    static TEST_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// AMUX-3684. Four concurrent misses must produce ONE recompute, not four.
    ///
    /// The specimen, from the request log: slow requests arrived in groups of
    /// four at the same second, 30s apart — four dashboard clients, one TTL
    /// expiry, four full recomputes of `build_array` + a git call per checkout.
    /// Only one was needed.
    ///
    /// WHAT THIS COVERS, AND WHAT IT DOES NOT. It drives the real `REFRESH` and
    /// `cached()` statics, so the single-flight PATTERN is genuinely exercised
    /// — including the double-check, which is the part that makes the losers
    /// cheap rather than merely serialised.
    ///
    /// It does NOT drive `sessions_git()` itself: the handler needs an
    /// `AppState` with a populated store and shells out to git, so the body is
    /// stubbed by a counter. That means this cell CANNOT go red if the handler
    /// stops calling the pattern. Verified by mutation, not assumed: deleting
    /// the handler's re-check leaves this green.
    ///
    /// Said out loud because a cell named for a stampede reads as if it guards
    /// the endpoint, and this repo keeps finding tests that pin a real property
    /// one layer above where the defect would be introduced. The handler's use
    /// of the pattern is currently held by review alone; closing that needs the
    /// single-flight extracted behind a seam the test can call, which is
    /// tracked on AMUX-3684 rather than done here.
    #[tokio::test]
    async fn a_stampede_of_misses_recomputes_once() {
        let _gate = TEST_GATE.lock().await;
        *CACHE.lock().unwrap() = None;
        let computes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let one = |computes: std::sync::Arc<std::sync::atomic::AtomicUsize>| async move {
            // Exactly the handler's shape: check, take the lock, CHECK AGAIN.
            if cached().is_some() {
                return;
            }
            let _g = REFRESH.lock().await;
            if cached().is_some() {
                return;
            }
            computes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(40)).await; // the git fan-out
            *CACHE.lock().unwrap() = Some((Instant::now(), json!({"a": 1})));
        };

        let hs: Vec<_> = (0..4).map(|_| tokio::spawn(one(computes.clone()))).collect();
        for h in hs {
            h.await.unwrap();
        }
        assert_eq!(
            computes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "four concurrent misses must recompute ONCE — the other three exist to \
             serve the winner's result, not to queue behind it"
        );
        assert!(cached().is_some(), "and the result must be cached for the next caller");
        *CACHE.lock().unwrap() = None;
    }

    /// CONTROL: single-flight must not become a permanent lock. After the entry
    /// expires, the NEXT caller has to be able to recompute — a mutex that
    /// serialised forever while the cache stayed stale would pass the cell
    /// above and freeze the branch map, which is the failure the TTL exists to
    /// prevent.
    #[tokio::test]
    async fn the_refresh_lock_is_released_so_a_later_miss_still_recomputes() {
        let _gate = TEST_GATE.lock().await;
        *CACHE.lock().unwrap() = None;
        {
            let _g = REFRESH.lock().await;
            *CACHE.lock().unwrap() = Some((
                Instant::now() - CACHE_TTL - Duration::from_secs(1),
                json!({"stale": true}),
            ));
        } // lock dropped here
        assert!(cached().is_none(), "premise: the entry is expired");
        // The lock must be acquirable again, or every later miss deadlocks.
        let g = tokio::time::timeout(Duration::from_secs(2), REFRESH.lock()).await;
        assert!(g.is_ok(), "REFRESH was never released — later misses would hang");
        drop(g);
        *CACHE.lock().unwrap() = None;
    }

    // The cache must actually expire — a TTL that never lets go turns a live
    // branch map into a frozen one, and the failure is invisible (the payload
    // still looks well-formed).
    #[test]
    fn cache_is_a_ttl_not_a_permanent_store() {
        // Sync cell, so there is no runtime to await on; `blocking_lock` is the
        // right call precisely because nothing async is running here.
        let _gate = TEST_GATE.blocking_lock();
        {
            let mut g = CACHE.lock().unwrap();
            *g = Some((Instant::now(), json!({"a": 1})));
        }
        assert!(cached().is_some(), "fresh entry should be served");
        {
            let mut g = CACHE.lock().unwrap();
            *g = Some((Instant::now() - CACHE_TTL - Duration::from_secs(1), json!({"a": 1})));
        }
        assert!(cached().is_none(), "expired entry must not be served");
        *CACHE.lock().unwrap() = None;
    }

    /// AMUX-3730: an EXPIRED entry is served at once, and only up to a ceiling.
    ///
    /// `cached_any` is the whole discriminator between "fast and correct" and
    /// "fast and lying", so it gets pinned in both directions. The ceiling is
    /// not decoration: without it a background refresh that keeps failing turns
    /// a visible 2s wait into an invisible week-old answer.
    #[test]
    fn an_expired_entry_is_still_servable_until_the_stale_ceiling() {
        let _gate = TEST_GATE.blocking_lock();
        let set_age = |d: Duration| {
            *CACHE.lock().unwrap() = Some((Instant::now() - d, json!({"a": 1})));
        };

        // Just past the TTL: NOT fresh, but servable as stale.
        set_age(CACHE_TTL + Duration::from_secs(1));
        assert!(cached().is_none(), "past the TTL it is not FRESH");
        let (age, _) = cached_any().expect("but it is still readable as stale");
        assert!(age < STALE_CEILING, "and it is inside the ceiling: {age:?}");

        // Past the ceiling: still readable, and the handler's guard must reject
        // it. Asserted as the COMPARISON the handler makes, not just presence —
        // `cached_any` returning Some is true on both sides of the ceiling, so
        // presence alone would pass against a build with no ceiling at all.
        set_age(STALE_CEILING + Duration::from_secs(1));
        let (age, _) = cached_any().expect("still readable");
        assert!(
            age >= STALE_CEILING,
            "past the ceiling the caller must WAIT rather than be served: {age:?}"
        );

        // CONTROL: a FRESH entry never takes the stale path at all.
        set_age(Duration::from_secs(0));
        assert!(cached().is_some(), "a fresh entry is served by the hit path");

        *CACHE.lock().unwrap() = None;
    }

    /// THE CELL THAT GOES WRONG QUIETLY (AMUX-3730).
    ///
    /// Every other cell here passes against a build that serves stale FOREVER
    /// and never refreshes — that version is fast, returns data, and honours the
    /// ceiling. What it does not do is ever change its answer. So this asserts
    /// the value the cache holds actually MOVES when a refresh runs, which is
    /// the property "revalidate" names and the only one that separates the fix
    /// from a cache that quietly froze.
    #[tokio::test]
    async fn a_refresh_actually_replaces_the_cached_value() {
        let _gate = TEST_GATE.lock().await;
        // A real store and a real temp home, because `recompute` is the SHIPPED
        // path — the point of this cell is that it drives the same function the
        // background task and the handler both call. The store is empty, so the
        // fresh value is an empty map; that is fine and is the harder case,
        // since it means the assertion below cannot pass by accident on
        // coincidental content.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _h = crate::api::settings::test_env::set_home(home.path());
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };

        // Seed a KNOWN-WRONG value with an old timestamp, the way a stale entry
        // looks in production.
        let sentinel = json!({"stale-sentinel": {"name": "stale-sentinel"}});
        *CACHE.lock().unwrap() = Some((
            Instant::now() - CACHE_TTL - Duration::from_secs(1),
            sentinel.clone(),
        ));
        assert!(cached().is_none(), "set-up: the seeded value must be EXPIRED");

        // Run the same function the background task runs.
        let fresh = recompute(&state).await.expect("recompute");

        let (age, now_cached) = cached_any().expect("cache must be populated");
        assert!(age < CACHE_TTL, "recompute must stamp it FRESH, not leave it stale");
        assert_ne!(
            now_cached, sentinel,
            "the cached value did not move — 'serve stale' degenerated into 'never refresh', \
             which passes every other cell in this module"
        );
        assert_eq!(now_cached, fresh, "the cache must hold what recompute returned");

        *CACHE.lock().unwrap() = None;
    }

    /// THE HANDLER ITSELF, which the cells above cannot reach.
    ///
    /// `an_expired_entry_is_still_servable_until_the_stale_ceiling` pins
    /// `cached_any` and the constant — a real property, one layer ABOVE where
    /// the defect would live. Delete the handler's ceiling comparison and that
    /// cell stays green, which is the same wrong-layer shape this module's
    /// stampede cell already confesses to ("it does NOT drive `sessions_git()`
    /// itself... this cell CANNOT go red if the handler stops calling the
    /// pattern"). That confession named the fix: extract the work behind a seam
    /// the test can call. AMUX-3730's refactor is that seam, so this closes it.
    ///
    /// The `x-amux-cache` header is what makes the disposition assertable at
    /// all. It rides a header rather than the body deliberately: the body is a
    /// bare `{session: ...}` map the SPA runs `Object.entries()` over, so a
    /// marker key in it would render as a phantom session.
    #[tokio::test]
    async fn the_handler_serves_stale_inside_the_ceiling_and_waits_past_it() {
        let _gate = TEST_GATE.lock().await;
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let _h = crate::api::settings::test_env::set_home(home.path());
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("t.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let disposition = |r: &Response| {
            r.headers()
                .get("x-amux-cache")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string()
        };

        // FRESH -> served from the hit path, no stale involved.
        *CACHE.lock().unwrap() = Some((Instant::now(), json!({"a": 1})));
        let r = sessions_git(State(state.clone())).await;
        assert_eq!(disposition(&r), "hit");

        // EXPIRED but inside the ceiling -> served IMMEDIATELY as stale. This is
        // the whole point of the card: the caller does not wait on the fan-out.
        *CACHE.lock().unwrap() =
            Some((Instant::now() - CACHE_TTL - Duration::from_secs(1), json!({"a": 1})));
        let r = sessions_git(State(state.clone())).await;
        assert_eq!(
            disposition(&r),
            "stale",
            "an expired entry inside the ceiling must be served at once, not recomputed"
        );

        // PAST the ceiling -> the caller WAITS for real data. Never "stale".
        // Without this the fix is "serve whatever we have, forever".
        *CACHE.lock().unwrap() =
            Some((Instant::now() - STALE_CEILING - Duration::from_secs(1), json!({"a": 1})));
        let r = sessions_git(State(state.clone())).await;
        assert_ne!(
            disposition(&r),
            "stale",
            "past the ceiling the caller must wait for fresh data, not be handed stale"
        );

        // COLD START -> nothing to serve stale, so it must block and compute.
        // The exemption has to be real; a build that answered "stale" here would
        // be serving a value it does not have.
        *CACHE.lock().unwrap() = None;
        let r = sessions_git(State(state.clone())).await;
        assert_eq!(disposition(&r), "miss", "a cold start has no stale value to serve");

        *CACHE.lock().unwrap() = None;
    }

    // The SPA does `if (!gi.repo) continue`, so a row without a repo is dead
    // weight. This pins the omission rule rather than trusting the comment.
    #[tokio::test]
    async fn a_directory_that_is_not_a_repo_yields_no_row() {
        let tmp = std::env::temp_dir().join(format!("amux-nonrepo-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(
            git_toplevel(tmp.to_str().unwrap()).await.is_none(),
            "a non-repo dir must not produce a repo"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
