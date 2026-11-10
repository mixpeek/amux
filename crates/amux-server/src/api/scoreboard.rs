//! `GET /api/debug/scoreboard`: the owner's one page on the fleet (Z14).
//!
//! Per worker: the goal number now, 1 h ago, at the start of today and at its
//! peak, the worker's state and what it is waiting on. For the fleet: rates, CI,
//! spend, host and the needs-you list.
//!
//! This module adds no probe. Every section reads a source that already exists,
//! so a number here cannot disagree with the page that owns it (A10):
//!
//! | section  | source |
//! |---|---|
//! | workers  | the `GET /api/sessions` array, with its caller filters |
//! | goal now | `goal_check::view` |
//! | history  | `goal_samples`, written by `goal_check` on every run with a number |
//! | needsyou | `board::needsyou_payload`, the owner view's own ranking |
//! | spend    | `token_ledger`, classified by the price table as `/api/observability` does |
//! | host     | the host guard's latest sample and the newest `host_metrics` row |
//! | ci       | `self_update::deploy_status` and main-ci-watch's state file |
//! | rates    | board state entries and the spend above |
//!
//! Every section carries `measured`, `n_considered` and, when it could not run,
//! `why_unmeasured` (A5). A field with no source is null beside a reason, and
//! `unmeasured` at the top level lists them all, so the page can say why without
//! a zero that means "not read".

use super::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashMap;

/// The newest needs-you cards the page lists. `needsyou.total` is the whole
/// queue; the owner view holds the rest.
const NEEDSYOU_SHOWN: usize = 25;
/// How far a sample may sit before the moment it stands for. Past this the
/// value is a guess about a gap, so it reads as unmeasured.
const SAMPLE_TOLERANCE_S: f64 = 2.0 * 3600.0;
const HOUR_S: f64 = 3600.0;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/debug/scoreboard", axum::routing::get(scoreboard))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub at: f64,
    pub done: u64,
    pub total: Option<u64>,
}

fn point(s: &Sample) -> Value {
    json!({"measured": true, "done": s.done, "total": s.total, "at": s.at})
}

fn missing(why: impl Into<String>) -> Value {
    json!({"measured": false, "why_unmeasured": why.into()})
}

/// The reading that stood at `at`: the newest sample at or before it, unless
/// that is older than the tolerance. Samples are oldest first.
fn sample_at(samples: &[Sample], at: f64) -> Option<&Sample> {
    samples.iter().rev().find(|s| s.at <= at).filter(|s| at - s.at <= SAMPLE_TOLERANCE_S)
}

/// Change per hour between two readings, when they are apart in time.
fn rate(from: &Value, to: &Value) -> Value {
    let read = |v: &Value| Some((v["done"].as_f64()?, v["at"].as_f64()?));
    match (read(from), read(to)) {
        (Some((a, at_a)), Some((b, at_b))) if at_b > at_a => {
            json!({"measured": true, "per_h": ((b - a) / ((at_b - at_a) / HOUR_S) * 100.0).round() / 100.0})
        }
        (Some(_), Some(_)) => missing("the two readings are at the same moment"),
        _ => missing("one of the two readings is unmeasured"),
    }
}

/// The worker's goal row: the checker's reading now plus its history. Pure.
pub fn goal_row(checker: &Value, goal: &Value, samples: &[Sample], now: f64, day_start: f64) -> Value {
    let state = goal["state"].clone();
    if checker["configured"] != true {
        let why = "no goal checker: this worker has no AMUX_GOAL_CHECK";
        return json!({"configured": false, "measured": false, "why_unmeasured": why, "state": state,
            "state_why": goal["state_why"]});
    }
    let current = match (checker["measured"] == true, checker["done"].as_u64()) {
        (true, Some(done)) => {
            json!({"measured": true, "done": done, "total": checker["total"], "at": checker["checked_at"]})
        }
        (true, None) => missing("the checker gave a verdict with no N/M or done number"),
        (false, _) => missing(checker["why_unmeasured"].as_str().unwrap_or("the checker produced no verdict")),
    };
    let none_yet = "no reading retained yet: samples start when a checker run prints a number";
    let hour_ago = match sample_at(samples, now - HOUR_S) {
        Some(s) => point(s),
        None if samples.is_empty() => missing(none_yet),
        None => missing("no reading retained within 2 h before that moment"),
    };
    let today_start = match sample_at(samples, day_start).or_else(|| samples.iter().find(|s| s.at >= day_start)) {
        Some(s) => point(s),
        None if samples.is_empty() => missing(none_yet),
        None => missing("no reading retained since midnight"),
    };
    let mut peak = samples.iter().max_by_key(|s| s.done).map(point);
    if let (Some(d), true) = (current["done"].as_u64(), current["measured"] == true) {
        if peak.as_ref().and_then(|p| p["done"].as_u64()).is_none_or(|p| d > p) {
            peak = Some(current.clone());
        }
    }
    json!({
        "configured": true,
        "measured": current["measured"],
        "why_unmeasured": current["why_unmeasured"],
        "state": state,
        "state_why": goal["state_why"],
        "met": checker["met"],
        "summary": checker["summary"],
        "stalled": checker["stalled"],
        "now": current,
        "hour_ago": hour_ago,
        "today_start": today_start,
        "peak": peak.unwrap_or_else(|| missing(none_yet)),
        "rate_1h": rate(&hour_ago, &current),
        "rate_today": rate(&today_start, &current),
        "n_samples": samples.len(),
    })
}

/// What the worker is waiting on, from what its session row records. Pure.
/// `detail` is the sentence the page shows.
pub fn waiting_on(row: &Value, needsyou: usize, now: f64) -> Value {
    let say = |kind: &str, detail: String| json!({"measured": true, "kind": kind, "detail": detail});
    if row["running"] != true {
        return say("stopped", "not running".into());
    }
    if row["credit_limited"] == true {
        return say("limit", "credit limit".into());
    }
    if let Some(until) = row["rate_limited_until"].as_f64().filter(|u| *u > now) {
        return say("limit", format!("rate limit, {} min left", ((until - now) / 60.0).ceil()));
    }
    let status = row["status"].as_str().unwrap_or("");
    if status == "waiting" {
        let reason = row["waiting_reason"].as_str().unwrap_or("a prompt in its pane");
        return say("prompt", reason.replace('_', " "));
    }
    if row["agent_state"] == "unknown" {
        return json!({"measured": false, "kind": "unknown",
            "why_unmeasured": "only a screen guess exists for this worker's state"});
    }
    if status == "active" {
        return match row["agents_working"].as_u64().filter(|n| *n > 0) {
            Some(n) => say("subagents", format!("{n} subagent(s) working")),
            None => say("working", "nothing, it is working".into()),
        };
    }
    if needsyou > 0 {
        return say("owner", format!("you: {needsyou} needs-you card(s)"));
    }
    let jobs: Vec<&Value> = row["background_jobs"].as_array().map(|j| j.iter().collect()).unwrap_or_default();
    let label = |j: &Value| j["label"].as_str().unwrap_or("background job").to_string();
    if let Some(j) = jobs.iter().find(|j| j["waiting"] != true) {
        return say("job", format!("own job: {}", label(j)));
    }
    if let Some(j) = jobs.first() {
        return say("job", format!("watch loop: {}", label(j)));
    }
    say("idle", "nothing queued".into())
}

/// Does this row belong on the page? Running workers, plus any worker with a
/// goal someone is still counting on. Helper rows (chat companions, ephemeral
/// children) carry no work of their own.
fn on_page(row: &Value, has_checker: bool) -> bool {
    if row["archived"] == true || row["ephemeral"] == true {
        return false;
    }
    if row["companion_of"].as_str().is_some_and(|c| !c.is_empty()) {
        return false;
    }
    row["running"] == true || has_checker || row["goal"]["active"] == true
}

fn load_samples(conn: &rusqlite::Connection) -> rusqlite::Result<HashMap<String, Vec<Sample>>> {
    let mut stmt = conn.prepare("SELECT session, at, done, total FROM goal_samples ORDER BY session, at")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Sample {
                at: r.get(1)?,
                done: r.get::<_, i64>(2)? as u64,
                total: r.get::<_, Option<i64>>(3)?.map(|t| t as u64),
            },
        ))
    })?;
    let mut out: HashMap<String, Vec<Sample>> = HashMap::new();
    for row in rows {
        let (name, s) = row?;
        out.entry(name).or_default().push(s);
    }
    Ok(out)
}

/// Spend from the ledger. Only rows the price table rates above zero count as
/// dollars, the same rule as `/api/observability`'s headline (AMUX-4806).
fn spend_section(
    conn: &rusqlite::Connection,
    now: f64,
    day_start: f64,
    prices: &[(String, [f64; 4])],
) -> rusqlite::Result<(Value, HashMap<String, f64>)> {
    let newest: Option<i64> = conn.query_row("SELECT MAX(ts) FROM token_ledger", [], |r| r.get(0))?;
    let Some(newest) = newest else {
        return Ok((crate::api::measured::unmeasured(json!({}), "the token ledger has no rows"), HashMap::new()));
    };
    let hour = now - HOUR_S;
    let mut stmt = conn.prepare(
        "SELECT session, model, \
         COALESCE(SUM(CASE WHEN ts >= ?1 THEN cost_usd END), 0), \
         COALESCE(SUM(CASE WHEN ts >= ?2 THEN cost_usd END), 0), \
         SUM(CASE WHEN ts >= ?1 THEN 1 ELSE 0 END) \
         FROM token_ledger WHERE ts >= ?3 GROUP BY session, model",
    )?;
    let rows = stmt.query_map(rusqlite::params![day_start, hour, day_start.min(hour)], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?, r.get::<_, i64>(4)?))
    })?;
    let (mut today, mut last_hour, mut turns, mut unmetered_turns) = (0.0f64, 0.0f64, 0i64, 0i64);
    let mut by_worker: HashMap<String, f64> = HashMap::new();
    for row in rows {
        let (session, model, day_cost, hour_cost, n) = row?;
        turns += n;
        match crate::runtime_jobs::token_ledger::model_rate(prices, &model) {
            Some(rate) if rate.iter().any(|r| *r > 0.0) => {
                today += day_cost;
                last_hour += hour_cost;
                *by_worker.entry(session).or_default() += day_cost;
            }
            Some(_) => {}
            None => unmetered_turns += n,
        }
    }
    let cents = |v: f64| (v * 100.0).round() / 100.0;
    let by_worker = by_worker.into_iter().map(|(k, v)| (k, cents(v))).collect();
    Ok((
        crate::api::measured::measured(
            json!({
                "today_usd": cents(today),
                "last_hour_usd": cents(last_hour),
                "turns_today": turns,
                "unmetered_turns_today": unmetered_turns,
                "ledger_newest_at": newest,
                "basis": "token_ledger at list price for priced models; unpriced and local models add no dollars",
            }),
            turns as usize,
        ),
        by_worker,
    ))
}

fn host_section(conn: Result<&rusqlite::Connection, String>) -> Value {
    let pressure = crate::runtime_jobs::host_guard::current()
        .and_then(|p| serde_json::to_value(p).ok())
        .filter(|p| p["measured"] == true);
    let metrics = conn.and_then(|c| {
        c.query_row(
            "SELECT ts, load_per_core, mem_percent, mem_pressure, swap_used_mb, swap_total_mb, disk_free_gb, disk_total_gb, proc_total \
             FROM host_metrics WHERE measured = 1 ORDER BY ts DESC LIMIT 1",
            [],
            |r| {
                Ok(json!({
                    "at": r.get::<_, i64>(0)?, "load_per_core": r.get::<_, Option<f64>>(1)?,
                    "mem_percent": r.get::<_, Option<f64>>(2)?, "mem_pressure": r.get::<_, Option<String>>(3)?,
                    "swap_used_mb": r.get::<_, Option<f64>>(4)?, "swap_total_mb": r.get::<_, Option<f64>>(5)?,
                    "disk_free_gb": r.get::<_, Option<f64>>(6)?, "disk_total_gb": r.get::<_, Option<f64>>(7)?,
                    "proc_total": r.get::<_, Option<i64>>(8)?,
                }))
            },
        )
        .map_err(|e| e.to_string())
    });
    let (metrics, metrics_why) = match metrics {
        Ok(m) => (Some(m), None),
        Err(why) => (None, Some(why)),
    };
    if pressure.is_none() && metrics.is_none() {
        let why = format!(
            "the host guard has no measured sample and host_metrics has no measured row{}",
            metrics_why.map(|w| format!(" ({w})")).unwrap_or_default()
        );
        return crate::api::measured::unmeasured(json!({}), &why);
    }
    let p = pressure.unwrap_or(Value::Null);
    let n = 1 + usize::from(metrics.is_some());
    let mut body = metrics.unwrap_or_else(|| json!({}));
    body["level"] = p["level"].clone();
    body["reasons"] = p["reasons"].clone();
    body["load1"] = p["readings"]["load1"].clone();
    body["ncpu"] = p["readings"]["ncpu"].clone();
    body["pressure_at"] = p["sampled_at"].clone();
    crate::api::measured::measured(body, n)
}

/// Cards that ENTERED done or verified, so a card moving from done to verified
/// counts twice across a day. Archived and deleted cards are left out.
fn cards_done(conn: &rusqlite::Connection, now: f64) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT COALESCE(SUM(entered_state_at >= ?1), 0), COUNT(*) FROM issues \
         WHERE status IN ('done','verified') AND COALESCE(archived,0) = 0 AND deleted IS NULL \
         AND entered_state_at >= ?2",
        rusqlite::params![(now - HOUR_S) as i64, (now - 24.0 * HOUR_S) as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// The latest verdict per workflow from main-ci-watch's state file.
async fn ci_section(home: &std::path::Path) -> Value {
    let dir = crate::api::self_update::repo_dir();
    let deploy = crate::api::self_update::deploy_status(dir.as_deref()).await;
    let state_path = home.join("state/main-ci-watch.json");
    let watch: Option<Value> = std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let watch_at = std::fs::metadata(&state_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64());
    let tip = deploy["tip"].as_str().unwrap_or("").to_string();
    let mut workflows = Vec::new();
    for (name, w) in watch.as_ref().and_then(|v| v.as_object()).into_iter().flatten() {
        let green = w["green"].as_str().unwrap_or("");
        let commits = match (&dir, green.is_empty() || tip.is_empty()) {
            (Some(d), false) => crate::api::self_update::commits_between(d, green, &tip).await,
            _ => None,
        };
        workflows.push(json!({"name": name, "last_green": green, "commits_since_green": commits}));
    }
    let gate = &deploy["gate"];
    let body = json!({
        "state": deploy["state"],
        "summary": deploy["summary"],
        "live": deploy["live"],
        "tip": deploy["tip"],
        "behind": deploy["behind"],
        "tip_state": gate["tip_state"],
        "gate_verdict": gate["verdict"],
        "gate_held": gate["held"],
        "gate_at": gate["at"],
        "workflows": workflows,
        "watch_at": watch_at,
        "pass_rate": missing("no CI run history is retained: main-ci-watch keeps only the last run seen and the last green commit per workflow"),
    });
    if deploy["measured"] == true {
        let n = 1 + workflows_len(&body);
        crate::api::measured::measured(body, n)
    } else {
        crate::api::measured::unmeasured(body, "main could not be read from this server's checkout")
    }
}

fn workflows_len(body: &Value) -> usize {
    body["workflows"].as_array().map_or(0, Vec::len)
}

/// Every null on the page with its reason, grouped so 40 workers with no
/// checker are one line. Pure.
pub fn collect_unmeasured(sections: &[(&str, &Value)], workers: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, v) in sections {
        if v["measured"] != true {
            out.push(json!({"field": name, "why": v["why_unmeasured"], "workers": []}));
        }
    }
    if let Some(why) = sections.iter().find(|(n, _)| *n == "ci").and_then(|(_, v)| v["pass_rate"]["why_unmeasured"].as_str()) {
        out.push(json!({"field": "ci pass rate", "why": why, "workers": []}));
    }
    let mut grouped: Vec<(String, String, Vec<Value>)> = Vec::new();
    let mut add = |field: &str, why: &str, name: &Value| match grouped.iter_mut().find(|g| g.0 == field && g.1 == why) {
        Some(g) => g.2.push(name.clone()),
        None => grouped.push((field.to_string(), why.to_string(), vec![name.clone()])),
    };
    for w in workers {
        let goal = &w["goal"];
        if goal["configured"] != true {
            add("goal", goal["why_unmeasured"].as_str().unwrap_or(""), &w["name"]);
        } else {
            for (field, key) in [("goal now", "now"), ("1 h ago", "hour_ago"), ("today start", "today_start"), ("peak", "peak")] {
                if goal[key]["measured"] != true {
                    add(field, goal[key]["why_unmeasured"].as_str().unwrap_or(""), &w["name"]);
                }
            }
        }
        if w["waiting_on"]["measured"] != true {
            add("waiting on", w["waiting_on"]["why_unmeasured"].as_str().unwrap_or(""), &w["name"]);
        }
    }
    out.extend(grouped.into_iter().map(|(f, why, names)| json!({"field": f, "why": why, "workers": names})));
    out
}

fn worker_row(row: &Value, goal: Value, needsyou: usize, spend_today: Option<f64>, now: f64) -> Value {
    json!({
        "name": row["name"],
        "isolated": row["isolated"],
        "running": row["running"],
        "status": row["status"],
        "agent_state": row["agent_state"],
        "turn_state": row["turn_state"],
        "activity": row["activity"],
        "idle_reason": row["idle_reason"],
        "waiting_on": waiting_on(row, needsyou, now),
        "needsyou": needsyou,
        "spend_today_usd": spend_today,
        "goal": goal,
    })
}

fn sort_rank(w: &Value) -> u8 {
    match (w["goal"]["configured"] == true, w["running"] == true) {
        (true, _) => 0,
        (false, true) => 1,
        (false, false) => 2,
    }
}

async fn scoreboard(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if crate::api::org::local_member_scope(&headers).is_some_and(|s| !s.is_global()) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "the scoreboard covers the whole fleet, so a scoped member cannot read it"})),
        )
            .into_response();
    }
    let now = crate::config::now_f64();
    let day_start = chrono::Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|d| d.and_local_timezone(chrono::Local).earliest())
        .map_or(now - 86_400.0, |d| d.timestamp() as f64);
    let home = crate::api::session_verbs::home();

    let caller = crate::api::groups::hdr_worker(&headers);
    let needsyou = if !caller.is_empty()
        && crate::api::session_verbs::session_is_isolated(&caller)
        && !crate::api::session_verbs::is_admin_worker(&caller)
    {
        crate::api::measured::unmeasured(json!({"queue": []}), "isolated workers have no board")
    } else {
        let (_, mut body) = crate::api::board::needsyou_payload(&state, true, NEEDSYOU_SHOWN);
        if let Some(q) = body["queue"].as_array().cloned() {
            let shown: Vec<Value> = q
                .iter()
                .take(NEEDSYOU_SHOWN)
                .map(|c| json!({"id": c["id"], "title": c["title"], "session": c["session"],
                    "age_days": c["age_days"], "due": c["due"]}))
                .collect();
            let mut by_session: HashMap<String, usize> = HashMap::new();
            for c in &q {
                *by_session.entry(c["session"].as_str().unwrap_or("").to_string()).or_default() += 1;
            }
            body["by_session"] = json!(by_session);
            body["shown"] = json!(shown.len());
            body["hidden"] = json!(q.len().saturating_sub(shown.len()));
            body["queue"] = json!(shown);
            body["note"] = Value::Null;
            body["ranked_by"] = Value::Null;
        }
        body
    };

    // Scoped so the store connection is released before the first await.
    let (spend, spend_by_worker, host, samples, samples_why, mut rates) = {
    let conn = state.store.read().map_err(|e| e.to_string());
    let conn_ref = conn.as_ref().map_err(String::clone).map(|c| &**c);
    let prices = crate::runtime_jobs::token_ledger::prices(&crate::config::amux_home());
    let (spend, spend_by_worker) = match &conn_ref {
        Ok(c) => spend_section(c, now, day_start, &prices).unwrap_or_else(|e| {
            (crate::api::measured::unmeasured(json!({}), &format!("token_ledger query failed: {e}")), HashMap::new())
        }),
        Err(e) => (crate::api::measured::unmeasured(json!({}), &format!("store unavailable: {e}")), HashMap::new()),
    };
    let host = host_section(conn_ref.clone().map(|c| c as &rusqlite::Connection));
    let (samples, samples_why) = match &conn_ref {
        Ok(c) => match load_samples(c) {
            Ok(s) => (s, None),
            Err(e) => (HashMap::new(), Some(e.to_string())),
        },
        Err(e) => (HashMap::new(), Some(e.clone())),
    };
    if let Some(why) = &samples_why {
        tracing::warn!(target: "amux::scoreboard", verdict = "scoreboard_samples_unread", measured = false,
            n_considered = 0, why_unmeasured = %why, "scoreboard: goal history could not be read");
    }
    let rates = match &conn_ref {
        Ok(c) => match cards_done(c, now) {
            Ok((hour, day)) => crate::api::measured::measured(
                json!({"cards_done_1h": hour, "cards_done_24h": day,
                       "basis": "cards that entered done or verified, excluding archived and deleted"}),
                day as usize,
            ),
            Err(e) => crate::api::measured::unmeasured(json!({}), &format!("board query failed: {e}")),
        },
        Err(e) => crate::api::measured::unmeasured(json!({}), &format!("store unavailable: {e}")),
    };
    (spend, spend_by_worker, host, samples, samples_why, rates)
    };
    rates["spend_per_h_usd"] = spend["last_hour_usd"].clone();
    let ci = ci_section(&home).await;

    let by_session = needsyou["by_session"].clone();
    let (mut workers, sessions_why, excluded) = match crate::api::sessions_legacy::sessions_for_caller(state.store.clone(), &headers).await {
        Err(e) => (Vec::new(), Some(format!("sessions could not be built: {e}")), 0),
        Ok(rows) => {
            let total = rows.len();
            let mut out = Vec::new();
            for row in &rows {
                let Some(name) = row["name"].as_str() else { continue };
                let checker = crate::runtime_jobs::goal_check::view(&home, name, now);
                if !on_page(row, checker["configured"] == true) {
                    continue;
                }
                let empty = Vec::new();
                let history = samples.get(name).unwrap_or(&empty);
                let goal = goal_row(&checker, &row["goal"], history, now, day_start);
                let cards = by_session[name].as_u64().unwrap_or(0) as usize;
                let spend_today = (spend["measured"] == true)
                    .then(|| spend_by_worker.get(name).copied().unwrap_or(0.0));
                out.push(worker_row(row, goal, cards, spend_today, now));
            }
            let excluded = total - out.len();
            (out, None, excluded)
        }
    };
    workers.sort_by(|a, b| {
        sort_rank(a).cmp(&sort_rank(b)).then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });

    let mut unmeasured = collect_unmeasured(
        &[("rates", &rates), ("ci", &ci), ("spend", &spend), ("host", &host), ("needs-you", &needsyou)],
        &workers,
    );
    if let Some(why) = &samples_why {
        unmeasured.push(json!({"field": "goal history", "why": why, "workers": []}));
    }
    let n = workers.len();
    let body = json!({
        "generated_at": now,
        "day_start": day_start,
        "retention_days": crate::runtime_jobs::goal_check::SAMPLE_RETENTION_DAYS,
        "workers": workers,
        "excluded": excluded,
        "rates": rates,
        "ci": ci,
        "spend": spend,
        "host": host,
        "needsyou": needsyou,
        "unmeasured": unmeasured,
    });
    Json(match sessions_why {
        Some(why) => crate::api::measured::unmeasured(body, &why),
        None => crate::api::measured::measured(body, n),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 100_000.0;
    const DAY: f64 = 60_000.0;

    fn s(at: f64, done: u64) -> Sample {
        Sample { at, done, total: Some(56) }
    }
    fn checker(done: u64) -> Value {
        json!({"configured": true, "measured": true, "done": done, "total": 56, "checked_at": NOW, "met": false})
    }

    #[test]
    fn history_reads_the_reading_that_stood_at_each_moment() {
        let samples = [s(DAY - 600.0, 40), s(DAY + 600.0, 41), s(NOW - 4000.0, 50), s(NOW - 3000.0, 49)];
        let g = goal_row(&checker(44), &json!({"state": "unmet"}), &samples, NOW, DAY);
        assert_eq!(g["now"]["done"], 44);
        assert_eq!(g["hour_ago"]["done"], 50, "newest at or before now minus 1 h, {g}");
        assert_eq!(g["today_start"]["done"], 40, "the value standing at midnight");
        assert_eq!(g["peak"]["done"], 50, "the peak may be above the current number");
        assert_eq!(g["rate_1h"]["per_h"], -5.4, "6 down over 4000 s, {g}");
        // The current reading can be the new peak.
        let g = goal_row(&checker(70), &json!({}), &samples, NOW, DAY);
        assert_eq!((g["peak"]["done"].as_u64(), g["peak"]["at"].as_f64()), (Some(70), Some(NOW)));
    }

    #[test]
    fn a_gap_reads_as_unmeasured_never_as_a_number() {
        // Nothing near 1 h ago, and nothing at midnight, so today's start is the first reading today.
        let samples = [s(DAY + 600.0, 41), s(NOW - 100.0, 44)];
        let g = goal_row(&checker(44), &json!({}), &samples, NOW + 9.0 * 3600.0, DAY);
        assert_eq!(g["hour_ago"]["measured"], false, "{g}");
        assert!(g["hour_ago"]["done"].is_null());
        assert_eq!(g["rate_1h"]["measured"], false);
        assert_eq!(g["today_start"]["done"], 41);
        // No samples at all: every historic field says so, and the live one still reads.
        let g = goal_row(&checker(44), &json!({}), &[], NOW, DAY);
        assert_eq!((g["hour_ago"]["measured"].clone(), g["today_start"]["measured"].clone()), (json!(false), json!(false)));
        assert!(g["hour_ago"]["why_unmeasured"].as_str().unwrap().contains("no reading retained yet"));
        assert_eq!(g["peak"]["done"], 44);
    }

    #[test]
    fn a_failed_checker_shows_its_reason_and_no_stale_number() {
        let c = json!({"configured": true, "measured": false, "why_unmeasured": "checker exited 2", "done": null});
        let g = goal_row(&c, &json!({}), &[s(NOW - 4000.0, 44)], NOW, DAY);
        assert_eq!(g["measured"], false);
        assert_eq!(g["now"]["why_unmeasured"], "checker exited 2");
        assert_eq!(g["hour_ago"]["done"], 44, "history is still true");
        let g = goal_row(&json!({"configured": false}), &json!({"state": "none"}), &[], NOW, DAY);
        assert_eq!((g["configured"].clone(), g["measured"].clone()), (json!(false), json!(false)));
    }

    #[test]
    fn waiting_on_names_each_cause() {
        let base = json!({"running": true, "status": "idle", "agent_state": "idle"});
        let with = |k: &str, v: Value| {
            let mut r = base.clone();
            r[k] = v;
            r
        };
        assert_eq!(waiting_on(&with("running", json!(false)), 0, NOW)["kind"], "stopped");
        assert_eq!(waiting_on(&with("credit_limited", json!(true)), 0, NOW)["kind"], "limit");
        assert_eq!(waiting_on(&with("rate_limited_until", json!(NOW + 90.0)), 0, NOW)["detail"], "rate limit, 2 min left");
        assert_eq!(waiting_on(&with("rate_limited_until", json!(NOW - 90.0)), 0, NOW)["kind"], "idle", "a past limit is over");
        let mut r = with("status", json!("waiting"));
        assert_eq!(waiting_on(&r, 0, NOW)["kind"], "prompt");
        r["waiting_reason"] = json!("permission_prompt");
        assert_eq!(waiting_on(&r, 0, NOW)["detail"], "permission prompt");
        assert_eq!(waiting_on(&with("status", json!("active")), 0, NOW)["kind"], "working");
        assert_eq!(waiting_on(&base, 3, NOW)["detail"], "you: 3 needs-you card(s)");
        let jobs = with("background_jobs", json!([{"label": "etl.sh", "waiting": false}]));
        assert_eq!(waiting_on(&jobs, 0, NOW)["detail"], "own job: etl.sh");
        let jobs = with("background_jobs", json!([{"label": "watch", "waiting": true}]));
        assert_eq!(waiting_on(&jobs, 0, NOW)["detail"], "watch loop: watch");
        assert_eq!(waiting_on(&base, 0, NOW)["kind"], "idle");
        let unknown = with("agent_state", json!("unknown"));
        assert_eq!(waiting_on(&unknown, 0, NOW)["measured"], false);
    }

    #[test]
    fn spend_counts_only_priced_rows_and_says_when_the_ledger_is_empty() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE token_ledger (ts INTEGER, session TEXT, model TEXT, cost_usd REAL)").unwrap();
        let prices = vec![("opus".to_string(), [15.0, 1.5, 18.75, 75.0]), ("qwen".to_string(), [0.0; 4])];
        let (v, _) = spend_section(&c, NOW, DAY, &prices).unwrap();
        assert_eq!(v["measured"], false, "an empty ledger is unmeasured, never $0: {v}");
        for (ts, session, model, cost) in [
            (DAY as i64 + 5, "a", "opus", 2.0),
            (NOW as i64 - 60, "a", "opus", 1.25),
            (NOW as i64 - 60, "b", "gpt-unpriced-xyz", 99.0),
            (NOW as i64 - 60, "b", "qwen", 50.0),
            (DAY as i64 - 500, "a", "opus", 7.0),
        ] {
            c.execute("INSERT INTO token_ledger VALUES (?1,?2,?3,?4)", rusqlite::params![ts, session, model, cost]).unwrap();
        }
        let (v, by) = spend_section(&c, NOW, DAY, &prices).unwrap();
        assert_eq!((v["today_usd"].as_f64(), v["last_hour_usd"].as_f64()), (Some(3.25), Some(1.25)), "{v}");
        assert_eq!((v["turns_today"].as_i64(), v["unmetered_turns_today"].as_i64()), (Some(4), Some(1)), "{v}");
        assert_eq!(by.get("a"), Some(&3.25));
        assert_eq!(by.get("b"), None, "b has no priced spend");
    }

    #[test]
    fn collect_unmeasured_groups_workers_and_names_sections() {
        let no_checker = goal_row(&json!({"configured": false}), &json!({}), &[], NOW, DAY);
        let wait = json!({"measured": true});
        let workers = [
            json!({"name": "a", "goal": no_checker, "waiting_on": wait}),
            json!({"name": "b", "goal": no_checker, "waiting_on": wait}),
        ];
        let spend = crate::api::measured::unmeasured(json!({}), "the token ledger has no rows");
        let ci = json!({"measured": true, "pass_rate": missing("no history")});
        let out = collect_unmeasured(&[("spend", &spend), ("ci", &ci)], &workers);
        assert_eq!(out[0]["field"], "spend");
        assert_eq!(out[0]["why"], "the token ledger has no rows");
        assert_eq!(out[1]["field"], "ci pass rate");
        assert_eq!(out[2]["workers"], json!(["a", "b"]), "{out:?}");
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn the_page_keeps_running_and_goal_workers_and_drops_helpers() {
        assert!(on_page(&json!({"running": true}), false));
        assert!(on_page(&json!({"running": false}), true), "a stopped worker with a checker stays visible");
        assert!(on_page(&json!({"running": false, "goal": {"active": true}}), false));
        assert!(!on_page(&json!({"running": false}), false));
        assert!(!on_page(&json!({"running": true, "archived": true}), true));
        assert!(!on_page(&json!({"running": true, "ephemeral": true}), false));
        assert!(!on_page(&json!({"running": true, "companion_of": "amux"}), false));
    }
}
