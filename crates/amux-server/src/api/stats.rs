//! GET /api/stats/daily — today's token usage per session (Python
//! amux-server.py:67813, `get_daily_token_stats` ~:17874).
//!
//! Response contract (the About modal's token panel reads exactly these
//! keys): `{today, total_tokens, total_input, total_output, amux_tokens,
//! sessions: [{name, proj_dir, amux, input, output, total}]}`.
//!
//! DATA SOURCE deviation, on purpose: Python re-scans every Claude Code
//! project JSONL on each request; this port aggregates the durable
//! `token_ledger` table, which Python's own observability layer fills from
//! those same JSONLs (one row per turn). `token_ledger.ts` is UNIX SECONDS
//! (0001_baseline.sql: "unix seconds of the turn"; /api/review/week filters
//! it with a seconds cutoff) — NOT the milliseconds trap `interaction_log.ts`
//! carries. The day filter is a half-open [local-midnight, next-midnight)
//! range so it provably excludes yesterday, and the tests assert the
//! exclusion (ethos rule 7: confirm the filter excluded something).
//!
//! Python-parity mappings from ledger rows:
//! - a row's `session` is the owning session (JSONL customTitle); the
//!   `amux` flag is Python's exact rule — the name has a
//!   `<amux_home>/sessions/<name>.env` file;
//! - rows with an empty session are conversations amux does not own.
//!   Python labels those by project dir; the ledger does not keep the dir,
//!   so they aggregate under one "(untitled)" row with `proj_dir: ""`,
//!   keeping total_tokens = ALL Claude Code usage (what the modal's
//!   "All Claude Code" line shows) honest.
//! - the `token_baseline.json` written by POST /api/stats/reset is honored
//!   on read (subtract, clamp at 0, ignore stale dates) so both servers
//!   agree after a reset. The reset endpoint is now ported too (AMUX-2871) —
//!   until 2026-08-11 this file read a baseline nothing could write, so the
//!   Tokens tab's reset button was inert and the newest baseline on this
//!   machine dated from the Python era.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use super::settings::amux_home;
use super::AppState;

/// Half-open local-day bounds [start, end) in unix seconds for "now".
fn local_day_bounds(now: chrono::DateTime<chrono::Local>) -> (i64, i64) {
    let d = now.date_naive();
    let next = d.succ_opt().unwrap_or(d);
    let ts = |nd: chrono::NaiveDate| -> i64 {
        let ndt = nd.and_hms_opt(0, 0, 0).expect("midnight exists");
        match ndt.and_local_timezone(chrono::Local) {
            chrono::LocalResult::Single(t) => t.timestamp(),
            // DST fold: the earlier instant — everything after it is still
            // "today", so nothing is dropped.
            chrono::LocalResult::Ambiguous(a, _) => a.timestamp(),
            // DST gap at midnight (some zones): fall back to UTC midnight.
            chrono::LocalResult::None => ndt.and_utc().timestamp(),
        }
    };
    (ts(d), ts(next))
}

/// Python's `amux_names`: the stems of `<home>/sessions/*.env`.
fn amux_session_names(home: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(home.join("sessions")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("env") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.insert(stem.to_string());
                }
            }
        }
    }
    names
}

/// Python `_load_token_baseline`: `<home>/token_baseline.json`, honored only
/// when its `date` is today.
fn load_token_baseline(home: &Path, today: &str) -> Option<Value> {
    let raw = std::fs::read_to_string(home.join("token_baseline.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    if v.get("date").and_then(Value::as_str) == Some(today) {
        Some(v)
    } else {
        None
    }
}

struct Bucket {
    name: String,
    proj_dir: String,
    amux: bool,
    input: i64,
    output: i64,
}

pub async fn daily(State(state): State<AppState>) -> Response {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let (start, end) = local_day_bounds(now);

    let store = state.store.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, i64, i64)>> {
        let conn = store.read()?;
        let mut stmt = conn.prepare(
            "SELECT session, COALESCE(SUM(input), 0), COALESCE(SUM(output), 0)
             FROM token_ledger WHERE ts >= ?1 AND ts < ?2 GROUP BY session",
        )?;
        let out = stmt
            .query_map([start, end], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    })
    .await;
    let rows = match rows {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
                .into_response()
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
                .into_response()
        }
    };

    let home = amux_home();
    let amux_names = amux_session_names(&home);
    let mut total_in: i64 = 0;
    let mut total_out: i64 = 0;
    // BTreeMap for a deterministic pre-sort order (ties sort stably).
    let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();
    for (session, input, output) in rows {
        total_in += input;
        total_out += output;
        let b = if session.is_empty() {
            Bucket { name: "(untitled)".into(), proj_dir: String::new(), amux: false, input, output }
        } else {
            let amux = amux_names.contains(&session);
            Bucket { name: session.clone(), proj_dir: session.clone(), amux, input, output }
        };
        buckets.insert(b.proj_dir.clone(), b);
    }

    // Baseline subtraction (Python: per-session by proj_dir-then-name key,
    // clamped at zero, totals clamped too).
    if let Some(baseline) = load_token_baseline(&home, &today) {
        let empty = json!({});
        let bl_sessions = baseline.get("sessions").unwrap_or(&empty);
        for b in buckets.values_mut() {
            let bl = bl_sessions
                .get(&b.proj_dir)
                .or_else(|| bl_sessions.get(&b.name))
                .unwrap_or(&empty);
            b.input = (b.input - bl.get("input").and_then(Value::as_i64).unwrap_or(0)).max(0);
            b.output = (b.output - bl.get("output").and_then(Value::as_i64).unwrap_or(0)).max(0);
        }
        total_in = (total_in - baseline.get("total_input").and_then(Value::as_i64).unwrap_or(0)).max(0);
        total_out = (total_out - baseline.get("total_output").and_then(Value::as_i64).unwrap_or(0)).max(0);
    }

    let mut sessions: Vec<&Bucket> =
        buckets.values().filter(|b| b.input + b.output > 0).collect();
    sessions.sort_by_key(|b| -(b.input + b.output));
    let amux_tokens: i64 =
        sessions.iter().filter(|b| b.amux).map(|b| b.input + b.output).sum();

    Json(json!({
        "today": today,
        "total_tokens": total_in + total_out,
        "total_input": total_in,
        "total_output": total_out,
        "amux_tokens": amux_tokens,
        "sessions": sessions.iter().map(|b| json!({
            "name": b.name,
            "proj_dir": b.proj_dir,
            "amux": b.amux,
            "input": b.input,
            "output": b.output,
            "total": b.input + b.output,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// POST /api/stats/reset — "zero today's counter" (py:67954).
///
/// The READER above has been honoring `token_baseline.json` since the cutover
/// while nothing could write it: the route was unmounted, so the Tokens tab's
/// reset button did nothing and the newest baseline on this machine dated from
/// 2026-02-18 (the Python era). A reader whose input can never be produced is
/// the "capability that only exists" shape — the code looks complete and the
/// feature is inert.
///
/// Python recovered the RAW totals by adding its existing baseline back onto
/// already-subtracted stats. Here the ledger is right there, so raw is read
/// directly: same result, and it cannot drift out of step with the subtraction
/// above if either side changes.
pub async fn reset(State(state): State<AppState>) -> Response {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let (start, end) = local_day_bounds(now);

    let store = state.store.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, i64, i64)>> {
        let conn = store.read()?;
        let mut stmt = conn.prepare(
            "SELECT session, COALESCE(SUM(input), 0), COALESCE(SUM(output), 0)
             FROM token_ledger WHERE ts >= ?1 AND ts < ?2 GROUP BY session",
        )?;
        let out = stmt
            .query_map([start, end], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(out)
    })
    .await;
    let rows = match rows {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
                .into_response()
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
                .into_response()
        }
    };

    let mut sessions = serde_json::Map::new();
    let (mut total_in, mut total_out) = (0i64, 0i64);
    for (session, input, output) in rows {
        total_in += input;
        total_out += output;
        // Keyed by proj_dir, which for these rows IS the session key — py:9788
        // keys by proj_dir "not by display label (can change)". A label-keyed
        // baseline silently stops matching the moment a worker is renamed.
        if !session.is_empty() {
            sessions.insert(session, json!({"input": input, "output": output}));
        }
    }
    let baseline = json!({
        "date": today,
        "sessions": Value::Object(sessions),
        "total_input": total_in,
        "total_output": total_out,
    });

    let path = amux_home().join("token_baseline.json");
    let tmp = path.with_extension("json.tmp");
    let text = baseline.to_string();
    match std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
        Ok(()) => Json(json!({"ok": true, "date": today,
                              "total_input": total_in, "total_output": total_out}))
        .into_response(),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("could not write {}: {e}", path.display())})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::settings::test_env;
    use axum::body::Body;
    use axum::http::Request;
    use axum::Router;
    use tower::ServiceExt;

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("stats-test.db")).unwrap();
        std::mem::forget(dir);
        AppState {
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn app(state: AppState) -> Router {
        Router::new()
            .route("/api/stats/daily", axum::routing::get(daily))
            .route("/api/stats/reset", axum::routing::post(reset))
            .with_state(state)
    }

    async fn insert_row(state: &AppState, ts: i64, session: &str, input: i64, output: i64) {
        let (session, ts) = (session.to_string(), ts);
        state
            .store
            .write_async(move |conn| {
                conn.execute(
                    "INSERT INTO token_ledger (ts, session, conversation, model, input, output)
                     VALUES (?1, ?2, '', '', ?3, ?4)",
                    rusqlite::params![ts, session, input, output],
                )?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await
            .unwrap();
    }

    async fn get(app: &Router) -> Value {
        let res = app
            .clone()
            .oneshot(Request::builder().uri("/api/stats/daily").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn python_shaped_rows_round_trip_and_the_day_filter_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        // alpha is a REGISTERED amux session; beta is a titled conversation
        // that is not amux's (Python's exact amux_names rule).
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(dir.path().join("sessions/alpha.env"), "CC_PROVIDER=claude\n").unwrap();

        let state = state();
        let now = chrono::Local::now().timestamp();
        insert_row(&state, now, "alpha", 100, 50).await;
        insert_row(&state, now, "alpha", 10, 5).await; // same session sums
        insert_row(&state, now, "beta", 7, 3).await;
        insert_row(&state, now, "", 1000, 0).await; // unowned conversation
        // Yesterday's row: the control the day filter must EXCLUDE.
        insert_row(&state, now - 3 * 86_400, "alpha", 99_999, 99_999).await;

        let app = app(state);
        let v = get(&app).await;

        // Python's exact response keys.
        assert_eq!(v["today"], json!(chrono::Local::now().format("%Y-%m-%d").to_string()));
        assert_eq!(v["total_input"], json!(1117));
        assert_eq!(v["total_output"], json!(58));
        assert_eq!(v["total_tokens"], json!(1175));
        // amux_tokens counts only registered-session rows (alpha), not the
        // titled-but-foreign beta and not the unowned bucket.
        assert_eq!(v["amux_tokens"], json!(165));

        let sessions = v["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 3, "{v}");
        // Sorted by total desc: unowned 1000, alpha 165, beta 10.
        assert_eq!(sessions[0]["name"], json!("(untitled)"));
        assert_eq!(sessions[0]["proj_dir"], json!(""));
        assert_eq!(sessions[0]["amux"], json!(false));
        assert_eq!(sessions[0]["total"], json!(1000));
        assert_eq!(
            sessions[1].clone(),
            json!({ "name": "alpha", "proj_dir": "alpha", "amux": true,
                     "input": 110, "output": 55, "total": 165 })
        );
        assert_eq!(sessions[2]["name"], json!("beta"));
        assert_eq!(sessions[2]["amux"], json!(false));

        // The excluded row is genuinely excluded — no 99_999 anywhere.
        assert!(!v.to_string().contains("99999"), "day filter failed to exclude: {v}");
    }

    #[tokio::test]
    async fn baseline_subtracts_today_and_ignores_stale_dates() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(dir.path().join("sessions/alpha.env"), "\n").unwrap();

        let state = state();
        let now = chrono::Local::now().timestamp();
        insert_row(&state, now, "alpha", 100, 50).await;
        let app = app(state);

        // A baseline dated today (Python save_token_baseline shape) subtracts.
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        std::fs::write(
            dir.path().join("token_baseline.json"),
            json!({ "date": today, "sessions": { "alpha": { "input": 60, "output": 20 } },
                    "total_input": 60, "total_output": 20 })
            .to_string(),
        )
        .unwrap();
        let v = get(&app).await;
        assert_eq!(v["total_input"], json!(40));
        assert_eq!(v["total_output"], json!(30));
        assert_eq!(v["sessions"][0]["input"], json!(40));
        assert_eq!(v["sessions"][0]["total"], json!(70));
        assert_eq!(v["amux_tokens"], json!(70));

        // Over-subtraction clamps at zero and the zeroed row drops out —
        // Python filters total > 0.
        std::fs::write(
            dir.path().join("token_baseline.json"),
            json!({ "date": today, "sessions": { "alpha": { "input": 500, "output": 500 } },
                    "total_input": 500, "total_output": 500 })
            .to_string(),
        )
        .unwrap();
        let v = get(&app).await;
        assert_eq!(v["total_tokens"], json!(0));
        assert_eq!(v["sessions"].as_array().unwrap().len(), 0);

        // A stale (yesterday's) baseline is ignored entirely.
        std::fs::write(
            dir.path().join("token_baseline.json"),
            json!({ "date": "2001-01-01", "sessions": { "alpha": { "input": 500 } },
                    "total_input": 500, "total_output": 500 })
            .to_string(),
        )
        .unwrap();
        let v = get(&app).await;
        assert_eq!(v["total_tokens"], json!(150));
        assert_eq!(v["sessions"][0]["total"], json!(150));
    }

    /// The pair that had never been exercised together: reset WRITES the file
    /// daily READS. Deliberately tested end-to-end through both handlers rather
    /// than by asserting the JSON on disk — the bug this ports away from was
    /// exactly a writer and a reader that each looked right alone.
    #[tokio::test]
    async fn reset_zeroes_today_and_the_reader_agrees() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(dir.path().join("sessions/alpha.env"), "\n").unwrap();

        let state = state();
        let now = chrono::Local::now().timestamp();
        insert_row(&state, now, "alpha", 100, 50).await;
        let app = app(state.clone());

        assert_eq!(get(&app).await["total_tokens"], json!(150));

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/stats/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["ok"], json!(true));
        // The baseline captures RAW totals, so the response echoes 150 even
        // though the reader will now report 0.
        assert_eq!(v["total_input"], json!(100));

        let after = get(&app).await;
        assert_eq!(after["total_tokens"], json!(0), "reset must zero today");
        assert_eq!(after["sessions"].as_array().unwrap().len(), 0);

        // CONTROL: usage recorded AFTER the reset must still be counted, or the
        // endpoint is a mute button rather than a reset.
        insert_row(&state, now, "alpha", 7, 3).await;
        assert_eq!(get(&app).await["total_tokens"], json!(10));
    }

    /// The baseline is keyed by session, and a session that renamed must not
    /// silently keep subtracting under its old key (py:9788's reason for
    /// keying on proj_dir rather than the display label).
    #[tokio::test]
    async fn reset_keys_the_baseline_by_session_and_skips_the_unowned_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let state = state();
        let now = chrono::Local::now().timestamp();
        insert_row(&state, now, "alpha", 10, 5).await;
        insert_row(&state, now, "", 1, 1).await; // unowned conversation
        let app = app(state);

        let res = app
            .clone()
            .oneshot(
                Request::builder().method("POST").uri("/api/stats/reset").body(Body::empty()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("token_baseline.json")).unwrap())
                .unwrap();
        assert_eq!(written["sessions"]["alpha"]["input"], json!(10));
        assert!(
            written["sessions"].get("").is_none(),
            "the empty-session bucket has no stable key and must not be written"
        );
        // Totals still include it — the "All Claude Code" line stays honest.
        assert_eq!(written["total_input"], json!(11));
    }

    #[tokio::test]
    async fn empty_ledger_serves_the_empty_python_shape() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let app = app(state());
        let v = get(&app).await;
        assert_eq!(v["total_tokens"], json!(0));
        assert_eq!(v["total_input"], json!(0));
        assert_eq!(v["total_output"], json!(0));
        assert_eq!(v["amux_tokens"], json!(0));
        assert_eq!(v["sessions"], json!([]));
        assert!(v["today"].as_str().unwrap().len() == 10);
    }

    #[test]
    fn day_bounds_are_half_open_and_local() {
        let now = chrono::Local::now();
        let (start, end) = local_day_bounds(now);
        let ts = now.timestamp();
        assert!(start <= ts && ts < end, "now must fall inside its own day");
        // A day is 86400s except across DST transitions (23h/25h).
        let span = end - start;
        assert!((82_800..=90_000).contains(&span), "span {span}");
    }
}
