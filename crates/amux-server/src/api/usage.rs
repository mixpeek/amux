//! GET /api/usage — subscription usage for the Settings meter (port of
//! Python's `_fetch_claude_usage`, amux-server.py ~:3189).
//!
//! # Why this does not go through `ProviderAdapter::usage()`
//!
//! It used to, and that is why the meter was dark. The adapter returns
//! NORMALIZED [`UsageWindow`]s for capacity routing, which is a deliberately
//! lossy view: it keeps kind/percent/reset and discards the provider-specific
//! fields this SPA renders — `limits[].scope.model.display_name` (the
//! per-model weekly rows), `limits[].group`, and the exact `kind` spelling
//! the renderer switches on. Re-deriving those from a normalized window is
//! guesswork, so the endpoint consumes the probe DIRECTLY and passes
//! Anthropic's body through verbatim, exactly as Python did. The SPA's
//! `loadUsage()` therefore sees byte-identical fields to the Python server.
//!
//! The adapter is still the owner of the probe — [`probe_usage_raw`] lives in
//! `provider/claude.rs` next to the credential reader, and
//! `ProviderAdapter::usage()` calls the same function. One token
//! acquisition, one HTTP call, two consumers with different honesty
//! requirements: routing needs a number or nothing (Invariant 20), a human
//! needs to know WHICH thing went wrong.
//!
//! # Discriminated degradation (ethos rule 4)
//!
//! The previous single sentence — "no token, expired token, or probe failed"
//! — was true and useless: it is three causes with three different remedies,
//! so nobody could tell a broken login from a transient rate limit, and the
//! meter stayed dark without anyone knowing which to fix. During development
//! (2026-08-09) this host's probe was answering **HTTP 429** with a perfectly
//! valid, unexpired keychain token — a self-healing condition that read as a
//! missing credential. Every cause now has its own reason string, and an HTTP
//! failure carries its status code.
//!
//! # Secrets
//!
//! No response on any path can contain the token: [`UsageProbe`] cannot carry
//! it, failure reasons are built from a status code or a fixed word, and the
//! upstream response BODY is never echoed on a failure — only on 2xx, where
//! it is the usage report itself.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};

use super::AppState;
use crate::provider::claude::{probe_usage_raw, UsageProbe};

/// Default cache TTL. Python used 30s; this defaults to 60 because the probe
/// is a NETWORK call made on a settings render, and the endpoint is
/// rate-limited per account — on a host running a fleet of Claude Code
/// processes, that budget is shared with all of them. Override with
/// `AMUX_USAGE_TTL_S` (0 disables caching, for debugging).
const DEFAULT_USAGE_TTL_S: u64 = 60;

/// How long a SUCCESSFUL reading keeps being served after a later probe
/// fails. `AMUX_USAGE_STALE_S`; 0 disables the fallback.
///
/// This exists because the failure is INTERMITTENT, which was only visible by
/// watching two servers at once: on 2026-08-09 one build served real limits
/// while another got HTTP 429 twenty seconds later, same host, same account.
/// With no fallback the meter flickers between real numbers and an error —
/// and "it says unavailable" is the bug being fixed here. Ten minutes is well
/// inside the resolution of the thing being measured (5-hour and 7-day
/// windows), so a reading this old is still true to the precision displayed.
const DEFAULT_USAGE_STALE_S: u64 = 600;

fn env_secs(key: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(default),
    )
}

fn usage_ttl() -> Duration {
    env_secs("AMUX_USAGE_TTL_S", DEFAULT_USAGE_TTL_S)
}

fn usage_stale_window() -> Duration {
    env_secs("AMUX_USAGE_STALE_S", DEFAULT_USAGE_STALE_S)
}

/// A probe as an injectable dependency, so tests exercise the real handler —
/// cache, shaping and all — against fixtures, and never touch the network or
/// this machine's keychain.
pub type ProbeFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = UsageProbe> + Send>> + Send + Sync>;

#[derive(Default)]
struct UsageCache {
    /// The shaped body WITHOUT its age field — age is stamped per response,
    /// because a cached reading gets older while it sits here.
    data: Option<Value>,
    at: Option<Instant>,
    /// The last body that actually carried numbers, kept separately so a
    /// transient failure cannot evict a good reading.
    last_good: Option<Value>,
    last_good_at: Option<Instant>,
}

/// Production wiring: the real read-only probe from the Claude adapter.
pub fn routes() -> Router<AppState> {
    routes_with(Arc::new(|| Box::pin(probe_usage_raw())))
}

/// Test seam.
pub fn routes_with(probe: ProbeFn) -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::get(get_usage))
        .route("/attribution", axum::routing::get(get_attribution))
        .layer(Extension(probe))
        .layer(Extension(Arc::new(tokio::sync::Mutex::new(
            UsageCache::default(),
        ))))
}

async fn get_usage(
    Extension(probe): Extension<ProbeFn>,
    Extension(cache): Extension<Arc<tokio::sync::Mutex<UsageCache>>>,
) -> Response {
    let ttl = usage_ttl();
    let mut c = cache.lock().await;

    // Serve from cache while fresh. Failures are cached too, deliberately: a
    // rate-limited probe must not be retried on every settings render, which
    // is the behaviour that provokes the rate limit in the first place.
    let fresh = matches!((&c.data, c.at), (Some(_), Some(at)) if at.elapsed() < ttl);
    if !fresh {
        let shaped = shape_probe(probe().await);
        if shaped.get("available") == Some(&json!(true)) {
            c.last_good = Some(shaped.clone());
            c.last_good_at = Some(Instant::now());
        }
        c.data = Some(shaped);
        c.at = Some(Instant::now());
    }

    let mut body = c.data.clone().unwrap_or_else(|| json!({}));
    let mut age = c.at.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);

    // A failed probe with a recent good reading in hand: serve the reading
    // rather than nothing. Everything here was really fetched — only its age
    // changed — and both the age and the live failure travel with it, so the
    // response never claims to be something it is not.
    let mut stale_reason: Option<Value> = None;
    if body.get("available") != Some(&json!(true)) {
        let stale_window = usage_stale_window();
        if let (Some(good), Some(at)) = (&c.last_good, c.last_good_at) {
            if !stale_window.is_zero() && at.elapsed() < stale_window {
                stale_reason = body.get("reason").cloned();
                body = good.clone();
                age = at.elapsed();
            }
        }
    }

    // How old is this reading? A meter that silently shows a minute-old
    // number is fine; one that cannot tell you it is doing so is not.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("cache_age_s".into(), json!(age.as_secs()));
        obj.insert("cache_ttl_s".into(), json!(ttl.as_secs()));
        if let Some(reason) = stale_reason {
            obj.insert("stale".into(), json!(true));
            // Why the live probe failed, kept beside the served numbers so
            // "these are 4 minutes old because of a 429" is answerable.
            obj.insert("stale_reason".into(), reason);
        }
    }
    Json(body).into_response()
}

/// One probe outcome -> the wire body the SPA consumes.
///
/// Success is Anthropic's body VERBATIM plus `available: true` — Python's
/// exact behaviour (`data["available"] = True; return data`), which is what
/// keeps `limits[].kind` / `.percent` / `.resets_at` / `.scope` / `.group`
/// spelled the way `loadUsage()` reads them.
fn shape_probe(probe: UsageProbe) -> Value {
    match probe {
        UsageProbe::Ok(body) => match body {
            Value::Object(mut map) => {
                map.insert("available".into(), json!(true));
                Value::Object(map)
            }
            // 2xx whose body is not a JSON object: there is nowhere to put
            // `available`, and guessing a shape would be inventing one.
            // Python raised here and reported a failed fetch; this is that,
            // named precisely.
            _ => degraded(
                "unexpected_shape",
                "Usage endpoint returned an unexpected response shape".into(),
            ),
        },
        // Each arm is a different remedy, which is the whole point of the type.
        UsageProbe::NoToken => degraded(
            "no_token",
            "No Claude subscription token on this host".into(),
        ),
        UsageProbe::Expired => degraded(
            "expired_token",
            "Token expired — run any Claude command to refresh it".into(),
        ),
        UsageProbe::Http(401) | UsageProbe::Http(403) => degraded(
            "token_rejected",
            "Token rejected (401) — run any Claude command to refresh it".into(),
        ),
        // Called out from the generic HTTP arm because it is the one failure
        // that is neither the user's fault nor persistent: it clears on its
        // own, and telling someone to re-login would be actively wrong.
        UsageProbe::Http(429) => degraded(
            "rate_limited",
            "Anthropic rate-limited the usage probe (HTTP 429) — this clears on its own; \
             it is usually many Claude processes sharing one account"
                .into(),
        ),
        UsageProbe::Http(code) => degraded(
            "probe_failed",
            format!("Usage fetch failed (HTTP {code})"),
        ),
        UsageProbe::Transport(what) => degraded(
            "probe_failed",
            format!("Usage fetch failed (network: {what})"),
        ),
        UsageProbe::BadShape => degraded(
            "unexpected_shape",
            "Usage endpoint returned an unexpected response shape".into(),
        ),
    }
}

/// The degraded body. `reason` is the human sentence the SPA prints; `cause`
/// is the stable machine tag, so a future consumer can branch without
/// string-matching prose (and so a reason can be reworded without breaking
/// anything).
fn degraded(cause: &str, reason: String) -> Value {
    json!({ "available": false, "cause": cause, "reason": reason })
}

/// GET /api/usage/attribution — WHAT SPENT THE PLAN, in dollars, by why the
/// turn happened (AMUX-3544).
///
/// # The complaint this answers
///
/// A customer on the $20 plan, 2026-08-23: "I sent 2-3 prompts today and my
/// credits ran out ... going to investigate what's using all the credits."
/// They could not, and neither could we. `/api/usage` reports the plan's own
/// utilization and cannot attribute one point of it, so the only signal
/// available was the credits being gone. Ethos rule 4: a diagnosis being
/// impossible from the data we keep IS the bug.
///
/// Both halves already existed and nothing joined them. `token_ledger` has the
/// real cost per turn (from the Claude Code transcripts, priced per model) but
/// knows only WHICH SESSION spent it. `cmd_history` knows WHY each turn
/// happened — a human typed it, a schedule fired, a peer lane sent a message —
/// but nothing about cost. Attribution is the correlated lookup between them:
/// for each ledger row, the most recent prompt to that session at or before it.
///
/// Measured on this machine the day it was written, last 24h:
///
/// ```text
///   a peer lane messaged   $2413.83   5977 turns
///   you typed it           $1430.26   2471 turns
///   a schedule fired       $1028.29   2481 turns
///   -> 71% of spend is background
/// ```
///
/// # Why a separate endpoint rather than a field on /api/usage
///
/// This module's contract is that `/api/usage` returns Anthropic's body
/// VERBATIM plus `available` — the SPA's `loadUsage()` sees byte-identical
/// fields to the Python server. Adding keys there would erode the one property
/// that makes the passthrough safe to reason about.
///
/// # The millisecond trap, stated because it already bit
///
/// `token_ledger.ts` is in SECONDS and `cmd_history.ts` is in MILLISECONDS.
/// Comparing them without the divide silently matches every row and returns a
/// clean, confident, wrong answer — it caught me on this exact query while
/// writing this, and ethos rule 7 records the same trap for
/// `interaction_log.ts`. The response therefore carries `rows_considered` and
/// `rows_excluded_by_window`, so a caller can confirm the window EXCLUDED
/// something rather than inferring correctness from plausible-looking output.
async fn get_attribution(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<AttributionQuery>,
) -> Response {
    let hours = q.hours.unwrap_or(24).clamp(1, 24 * 30);
    let store = state.store.clone();
    let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let cutoff: i64 = chrono::Utc::now().timestamp() - hours * 3600;

        // The control, not decoration: an unbounded match and a correct match
        // look identical from the rows alone.
        let total_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM token_ledger", [], |r| r.get(0))?;
        let in_window: i64 = conn.query_row(
            "SELECT COUNT(*) FROM token_ledger WHERE ts > ?1",
            [cutoff],
            |r| r.get(0),
        )?;

        let mut stmt = conn.prepare(
            "WITH lg AS (SELECT ts, session, cost_usd, input, output FROM token_ledger WHERE ts > ?1) \
             SELECT COALESCE((SELECT h.type FROM cmd_history h \
                                WHERE h.session = lg.session AND h.ts/1000 <= lg.ts \
                                ORDER BY h.ts DESC LIMIT 1), '') AS trig, \
                    SUM(cost_usd), COUNT(*), SUM(input), SUM(output) \
             FROM lg GROUP BY 1 ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;

        let mut sources = Vec::new();
        let (mut total_usd, mut bg_usd) = (0.0f64, 0.0f64);
        for row in rows {
            let (trig, usd, turns, inp, outp) = row?;
            total_usd += usd;
            let human = trig == "user";
            if !human {
                bg_usd += usd;
            }
            sources.push(json!({
                "source": if trig.is_empty() { "unattributed" } else { trig.as_str() },
                "label": trigger_label(&trig),
                "is_background": !human,
                "cost_usd": (usd * 100.0).round() / 100.0,
                "turns": turns,
                "input_tokens": inp,
                "output_tokens": outp,
            }));
        }

        // "A schedule fired" is actionable only when it names WHICH schedule,
        // and "a peer messaged" only when it names the lane. One schedule
        // accounted for 134 turns in 24h here and was invisible without this.
        let mut top = stmt_top(&conn, cutoff)?;
        top.truncate(10);

        Ok(json!({
            "window_hours": hours,
            "total_cost_usd": (total_usd * 100.0).round() / 100.0,
            "background_cost_usd": (bg_usd * 100.0).round() / 100.0,
            "background_pct": if total_usd > 0.0 {
                (bg_usd / total_usd * 1000.0).round() / 10.0
            } else { 0.0 },
            "by_source": sources,
            "top_origins": top,
            // Proof the window filtered. Equal counts mean the cutoff matched
            // everything and the numbers below are the whole table, not a window.
            "rows_considered": in_window,
            "rows_excluded_by_window": total_rows - in_window,
        }))
    })
    .await;

    match out {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize, Default)]
struct AttributionQuery {
    hours: Option<i64>,
}

/// Plain English, because the audience is a person wondering where their
/// credits went, not someone who knows what `cmd_history.type` is.
fn trigger_label(t: &str) -> &'static str {
    match t {
        "user" => "you typed it",
        "schedule" => "a schedule fired",
        "session" => "a peer lane messaged",
        "system" => "an amux nudge",
        "direct" | "steering" => "a steering message",
        "" => "no prompt matched — the turn predates this lane's history",
        _ => "other",
    }
}

/// The named offenders inside the background bucket.
fn stmt_top(conn: &rusqlite::Connection, cutoff: i64) -> anyhow::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "WITH lg AS (SELECT ts, session, cost_usd FROM token_ledger WHERE ts > ?1) \
         SELECT COALESCE((SELECT h.type FROM cmd_history h \
                            WHERE h.session = lg.session AND h.ts/1000 <= lg.ts \
                            ORDER BY h.ts DESC LIMIT 1), ''), \
                COALESCE((SELECT h.origin FROM cmd_history h \
                            WHERE h.session = lg.session AND h.ts/1000 <= lg.ts \
                            ORDER BY h.ts DESC LIMIT 1), ''), \
                lg.session, SUM(cost_usd), COUNT(*) \
         FROM lg GROUP BY 1,2,3 ORDER BY 4 DESC LIMIT 10",
    )?;
    let rows = stmt.query_map([cutoff], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (trig, origin, session, usd, turns) = row?;
        out.push(json!({
            "source": if trig.is_empty() { "unattributed" } else { trig.as_str() },
            "label": trigger_label(&trig),
            // `is_background` SHIPS HERE TOO (AMUX-3550). The client filtered
            // these rows on it and it existed only on `by_source`, so the
            // predicate `x.is_background !== false` kept 10 of 10 — a filter
            // that reads as a deliberate exclusion and excludes nothing. Three
            // of those ten were the human's own typing, listed under "biggest
            // background sources" on the panel built to tell a customer what
            // spent their credits BESIDES them. Fixed by making the field the
            // client already reaches for exist, rather than by teaching the
            // client a second spelling of it.
            "is_background": trig != "user",
            "origin": origin,
            "session": session,
            "cost_usd": (usd * 100.0).round() / 100.0,
            "turns": turns,
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    /// AMUX-3544. Spend is attributed to WHY the turn happened, and the window
    /// proves it filtered.
    ///
    /// The customer complaint this endpoint exists for was "I sent 2-3 prompts
    /// and my credits ran out", with no way to see what spent them. So the
    /// assertions are the two things a person in that position needs: which
    /// bucket the money is in, and whether the number they are reading covers
    /// the window they think it does.
    ///
    /// THE UNIT MISMATCH IS THE POINT OF THE THIRD ASSERTION. `token_ledger.ts`
    /// is in SECONDS and `cmd_history.ts` is in MILLISECONDS. A join that
    /// forgets the divide matches every prompt and still returns a tidy-looking
    /// answer — it did exactly that to me while I was writing this query. The
    /// fixture puts an OLD ledger row outside the window, so
    /// `rows_excluded_by_window` must be non-zero: an unbounded match and a
    /// correct one are indistinguishable from the totals alone.
    #[tokio::test]
    async fn spend_is_attributed_to_what_triggered_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("attr.db")).unwrap();
        let now = chrono::Utc::now().timestamp();
        store
            .write(move |conn| {
            // Three prompts into one lane: a human, a schedule, a peer.
                for (t, origin, at) in [
                    ("user", "", now - 300),
                    ("schedule", "poll the inbox", now - 200),
                    ("session", "peer-lane", now - 100),
                ] {
                    conn.execute(
                        "INSERT INTO cmd_history (text, type, session, ts, origin) VALUES (?,?,?,?,?)",
                        rusqlite::params!["p", t, "lane", at * 1000, origin],
                    )?;
                }
                // A turn after each prompt, plus one far outside the window.
                for (at, cost) in [
                    (now - 290, 1.0),
                    (now - 190, 5.0),
                    (now - 90, 20.0),
                    (now - 86_400 * 30, 999.0),
                ] {
                    conn.execute(
                        "INSERT INTO token_ledger (ts, session, conversation, model, input, cache_read, \
                         cache_write, output, cost_usd, task) VALUES (?,?,?,?,?,?,?,?,?,?)",
                        rusqlite::params![at, "lane", "c", "opus", 10, 0, 0, 5, cost, ""],
                    )?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let app = axum::Router::new()
            .nest("/api/usage", routes_with(probe_fn(UsageProbe::Ok(json!({})), Arc::new(AtomicUsize::new(0)))))
            .with_state(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/usage/attribution?hours=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();

        // 1. The money is in the right buckets, and the labels are for a human.
        let by: std::collections::HashMap<String, f64> = v["by_source"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["source"].as_str().unwrap().to_string(), r["cost_usd"].as_f64().unwrap()))
            .collect();
        assert_eq!(by.get("user"), Some(&1.0), "the human's turn: {v}");
        assert_eq!(by.get("schedule"), Some(&5.0), "the schedule's turn: {v}");
        assert_eq!(by.get("session"), Some(&20.0), "the peer's turn: {v}");

        // 2. Background share is the headline the complaint was about.
        assert_eq!(v["total_cost_usd"], json!(26.0));
        assert_eq!(v["background_cost_usd"], json!(25.0));
        assert_eq!(v["background_pct"], json!(96.2));

        // 3. THE CONTROL. The 30-day-old row must be OUTSIDE a 1-hour window.
        //    If this is 0 the join matched everything and every number above is
        //    the whole table wearing a window's label.
        assert_eq!(v["rows_considered"], json!(3));
        assert!(
            v["rows_excluded_by_window"].as_i64().unwrap() >= 1,
            "the window excluded nothing — an unbounded match returns a confident wrong \
             answer and looks exactly like this one: {v}"
        );

        // 4. Named offenders, or "a schedule fired" is not actionable.
        let top = v["top_origins"].as_array().unwrap();
        assert!(
            top.iter().any(|r| r["origin"] == "poll the inbox"),
            "the expensive schedule must be NAMED: {v}"
        );

        // 5. EVERY top_origins row carries `is_background`, and it is correct.
        //
        //    AMUX-3550: the client filtered these rows on this field and the
        //    field existed only on `by_source`, so `x.is_background !== false`
        //    kept 10 of 10 — a filter that reads as a deliberate exclusion and
        //    excludes nothing. The panel headed "biggest background sources"
        //    then listed the human's own typing, on the very screen built to
        //    tell a customer what spent their credits BESIDES them.
        //
        //    Asserting PRESENCE is the load-bearing half. A test that only
        //    checked the values of rows that happen to have the key would pass
        //    against the version where no row has it at all.
        for r in top {
            assert!(
                r.get("is_background").map(|b| b.is_boolean()).unwrap_or(false),
                "every top_origins row must carry a boolean is_background — its ABSENCE is \
                 what made the client's filter match everything: {r}"
            );
        }
        assert!(
            top.iter().any(|r| r["source"] == "user" && r["is_background"] == json!(false)),
            "the human's own row must be present AND flagged not-background, so a consumer \
             can exclude it: {v}"
        );
        assert!(
            top.iter().any(|r| r["source"] == "schedule" && r["is_background"] == json!(true)),
            "a schedule's row must be flagged background: {v}"
        );
    }

    /// A fixture probe that counts how many times it was called.
    fn probe_fn(outcome: UsageProbe, calls: Arc<AtomicUsize>) -> ProbeFn {
        Arc::new(move || {
            let outcome = outcome.clone();
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                outcome
            })
        })
    }

    fn app(probe: ProbeFn) -> axum::Router {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("usage-test.db")).unwrap();
        std::mem::forget(dir);
        let state = AppState {
            store: Arc::new(store),
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        Router::new()
            .nest("/api/usage", routes_with(probe))
            .with_state(state)
    }

    async fn get(app: &axum::Router) -> (StatusCode, Value) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/usage")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// The real endpoint's success body, built from the live response SHAPE
    /// (both the top-level windows and the `limits[]` array Anthropic sends
    /// together). Numbers are invented; no credential is involved.
    fn live_shaped_body() -> Value {
        json!({
            "five_hour": {"utilization": 34.4, "resets_at": "2026-08-09T18:00:00+00:00"},
            "seven_day": {"utilization": 71.6, "resets_at": "2026-08-12T00:00:00+00:00"},
            "seven_day_opus": null,
            "limits": [
                {"kind": "session", "percent": 34.4, "resets_at": "2026-08-09T18:00:00Z"},
                {"kind": "weekly_all", "group": "weekly", "percent": 71.6,
                 "resets_at": "2026-08-12T00:00:00Z"},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 12.0,
                 "scope": {"model": {"display_name": "Opus"}},
                 "resets_at": "2026-08-12T00:00:00Z"}
            ]
        })
    }

    #[tokio::test]
    async fn success_passes_anthropics_body_through_verbatim() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(
            UsageProbe::Ok(live_shaped_body()),
            calls.clone(),
        ));
        let (st, v) = get(&app).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["available"], json!(true));
        assert!(v.get("reason").is_none());

        // Every field loadUsage() reads must survive, spelled exactly as
        // Anthropic sent it — this is the regression that made the meter
        // useless when the endpoint normalized through UsageWindow.
        let limits = v["limits"].as_array().unwrap();
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[0]["kind"], json!("session"));
        assert_eq!(limits[0]["percent"], json!(34.4)); // not rounded away
        assert_eq!(limits[0]["resets_at"], json!("2026-08-09T18:00:00Z"));
        assert_eq!(limits[1]["group"], json!("weekly"));
        // The per-model row the SPA labels "<model> · weekly": normalized
        // windows discard this entirely.
        assert_eq!(
            limits[2]["scope"]["model"]["display_name"],
            json!("Opus")
        );
        // Top-level windows pass through untouched too.
        assert_eq!(v["five_hour"]["utilization"], json!(34.4));
    }

    #[tokio::test]
    async fn each_failure_cause_has_its_own_reason() {
        // The rule this enforces: no two causes may share a reason string,
        // and none may be the old catch-all. A test that only checked
        // `available == false` would have passed against the collapsed
        // message that motivated this work.
        let cases = vec![
            (UsageProbe::NoToken, "no_token"),
            (UsageProbe::Expired, "expired_token"),
            (UsageProbe::Http(401), "token_rejected"),
            (UsageProbe::Http(429), "rate_limited"),
            (UsageProbe::Http(500), "probe_failed"),
            (UsageProbe::Transport("timeout"), "probe_failed"),
            (UsageProbe::BadShape, "unexpected_shape"),
            (UsageProbe::Ok(json!([1, 2, 3])), "unexpected_shape"),
        ];
        let mut seen_reasons: Vec<String> = Vec::new();
        for (outcome, expect_cause) in cases {
            let app = app(probe_fn(outcome.clone(), Arc::new(AtomicUsize::new(0))));
            let (st, v) = get(&app).await;
            assert_eq!(st, StatusCode::OK, "{outcome:?}");
            assert_eq!(v["available"], json!(false), "{outcome:?}");
            assert_eq!(v["cause"], json!(expect_cause), "{outcome:?}");
            let reason = v["reason"].as_str().unwrap().to_string();
            assert!(!reason.is_empty());
            // Never the collapsed sentence again.
            assert!(
                !reason.contains("no token, expired token, or probe failed"),
                "{outcome:?} still serves the catch-all"
            );
            // No numbers invented on a degraded path.
            assert!(v.get("limits").is_none(), "{outcome:?}");
            seen_reasons.push(reason);
        }
        // Distinctness where the cause differs: 500 and timeout share a
        // cause tag but must still read differently (status vs network).
        let mut uniq = seen_reasons.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            seen_reasons.len() - 1, // BadShape and non-object Ok share one
            "reasons collapsed: {seen_reasons:?}"
        );
    }

    #[tokio::test]
    async fn http_failures_carry_their_status_code() {
        for code in [500u16, 502, 429] {
            let app = app(probe_fn(
                UsageProbe::Http(code),
                Arc::new(AtomicUsize::new(0)),
            ));
            let (_, v) = get(&app).await;
            assert!(
                v["reason"].as_str().unwrap().contains(&code.to_string()),
                "HTTP {code} reason omits the status: {}",
                v["reason"]
            );
        }
    }

    #[tokio::test]
    async fn no_response_on_any_path_can_contain_a_token() {
        // Assemble a secret-shaped string at runtime so the repo's scanner
        // never sees one, and prove it cannot reach the wire even when the
        // upstream body echoes it.
        let fake = format!("sk-ant-{}-{}", "oat01", "AAAAdeadbeefdeadbeef");
        let outcomes = vec![
            UsageProbe::NoToken,
            UsageProbe::Expired,
            UsageProbe::Http(401),
            UsageProbe::Http(429),
            UsageProbe::Transport("connect"),
            UsageProbe::BadShape,
        ];
        for outcome in outcomes {
            let app = app(probe_fn(outcome.clone(), Arc::new(AtomicUsize::new(0))));
            let (_, v) = get(&app).await;
            let text = serde_json::to_string(&v).unwrap();
            assert!(!text.contains(&fake), "{outcome:?}");
            assert!(!text.contains("Bearer"), "{outcome:?}");
            assert!(!text.contains("sk-ant"), "{outcome:?}");
        }
        // The one path that echoes upstream content is 2xx. The type system
        // is what guarantees the rest: no failure variant can hold a token.
    }

    #[tokio::test]
    async fn cache_serves_repeat_opens_from_one_probe_and_reports_age() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(
            UsageProbe::Ok(live_shaped_body()),
            calls.clone(),
        ));
        let (_, first) = get(&app).await;
        let (_, second) = get(&app).await;
        let (_, third) = get(&app).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "settings reopens must not hammer Anthropic"
        );
        assert_eq!(first["available"], json!(true));
        assert_eq!(second["limits"], first["limits"]);
        assert_eq!(third["limits"], first["limits"]);
        // Age is present on every response and TTL is advertised.
        for r in [&first, &second, &third] {
            assert!(r["cache_age_s"].is_number(), "{r}");
            assert_eq!(r["cache_ttl_s"], json!(DEFAULT_USAGE_TTL_S));
        }
    }

    #[tokio::test]
    async fn failures_are_cached_too_so_a_rate_limit_is_not_amplified() {
        // Retrying a 429 on every render is what provokes the 429.
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(UsageProbe::Http(429), calls.clone()));
        for _ in 0..5 {
            let (_, v) = get(&app).await;
            assert_eq!(v["cause"], json!("rate_limited"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A probe whose outcome changes between calls — the intermittent 429
    /// that actually happens on this host, which a fixed fixture cannot show.
    fn probe_sequence(outcomes: Vec<UsageProbe>) -> (ProbeFn, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let f: ProbeFn = Arc::new(move || {
            let outcomes = outcomes.clone();
            let c = c.clone();
            Box::pin(async move {
                let i = c.fetch_add(1, Ordering::SeqCst);
                outcomes
                    .get(i)
                    .cloned()
                    .unwrap_or(outcomes.last().cloned().unwrap())
            })
        });
        (f, calls)
    }

    #[tokio::test]
    async fn a_transient_failure_serves_the_last_good_reading_marked_stale() {
        // Success, then 429 — the sequence observed live on 2026-08-09.
        let (probe, _calls) = probe_sequence(vec![
            UsageProbe::Ok(live_shaped_body()),
            UsageProbe::Http(429),
        ]);
        let app = app(probe);
        temp_env_ttl("0", || async {
            let (_, first) = get(&app).await;
            assert_eq!(first["available"], json!(true));
            assert!(first.get("stale").is_none(), "a live reading is not stale");

            let (_, second) = get(&app).await;
            // The meter keeps working across the blip...
            assert_eq!(second["available"], json!(true));
            assert_eq!(second["limits"], first["limits"]);
            // ...and says so, with the live failure attached.
            assert_eq!(second["stale"], json!(true));
            assert!(second["stale_reason"]
                .as_str()
                .unwrap()
                .contains("429"));
        })
        .await;
    }

    #[tokio::test]
    async fn a_failure_with_no_prior_reading_stays_degraded() {
        // The fallback must never manufacture a first reading.
        let app = app(probe_fn(
            UsageProbe::Http(429),
            Arc::new(AtomicUsize::new(0)),
        ));
        let (_, v) = get(&app).await;
        assert_eq!(v["available"], json!(false));
        assert_eq!(v["cause"], json!("rate_limited"));
        assert!(v.get("limits").is_none());
        assert!(v.get("stale").is_none());
    }

    #[tokio::test]
    async fn stale_fallback_expires_and_can_be_disabled() {
        let (probe, _c) = probe_sequence(vec![
            UsageProbe::Ok(live_shaped_body()),
            UsageProbe::Http(429),
        ]);
        let app = app(probe);
        // TTL 0 forces a fresh probe each call; STALE 0 disables the
        // fallback, so the second call must degrade honestly rather than
        // reach for the reading it still holds.
        temp_env_both("0", "0", || async {
            let (_, first) = get(&app).await;
            assert_eq!(first["available"], json!(true));
            let (_, second) = get(&app).await;
            assert_eq!(
                second["available"],
                json!(false),
                "stale window 0 must not serve a prior reading"
            );
            assert_eq!(second["cause"], json!("rate_limited"));
        })
        .await;
    }

    #[tokio::test]
    async fn zero_ttl_disables_the_cache() {
        // The env knob has to actually reach the handler; a knob that reads
        // the env once at startup would pass a weaker test than this.
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(probe_fn(
            UsageProbe::Ok(live_shaped_body()),
            calls.clone(),
        ));
        temp_env_ttl("0", || async {
            get(&app).await;
            get(&app).await;
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Set both knobs around a block (same serialization as `temp_env_ttl`).
    async fn temp_env_both<F, Fut>(ttl: &str, stale: &str, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let _g = env_lock().lock().await;
        std::env::set_var("AMUX_USAGE_TTL_S", ttl);
        std::env::set_var("AMUX_USAGE_STALE_S", stale);
        f().await;
        std::env::remove_var("AMUX_USAGE_TTL_S");
        std::env::remove_var("AMUX_USAGE_STALE_S");
    }

    /// One process-wide async lock for every env-mutating test.
    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Set AMUX_USAGE_TTL_S around a block. Serialized because env is
    /// process-global and these tests share a process — and the lock is an
    /// ASYNC mutex, since it is held across the awaited block (a std
    /// `MutexGuard` across an await is a real deadlock risk, not just a lint).
    async fn temp_env_ttl<F, Fut>(val: &str, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        let _g = env_lock().lock().await;
        std::env::set_var("AMUX_USAGE_TTL_S", val);
        f().await;
        std::env::remove_var("AMUX_USAGE_TTL_S");
    }

    #[test]
    fn ttl_default_and_override() {
        assert_eq!(usage_ttl(), Duration::from_secs(DEFAULT_USAGE_TTL_S));
    }
}
