//! /api/velocity — where each worker's time goes, what wastes it, and what a
//! task is likely to cost, from the worker's own history.
//!
//! The reader is the worker first (Ethan, 2026-10-30: "more metrics for a
//! worker to be able to overcome its own bottlenecks ... figure out where
//! there's waste and automatically improve"), the owner second. Everything is
//! computed from `task_episodes` (one row per attempt at a card: worker,
//! start, end, outcome, reason) and `token_ledger` (tokens and cost per
//! turn, attributed to a card when known). No model call, so it is cheap
//! enough to compute on every read and it updates as each worker works.
//!
//! Waste is split by who can fix it. A release the harness made because the
//! lane was at its usage limit is amux's; a card the worker parked itself is
//! the worker's; a card parked on an unresolved dependency is the board's;
//! waiting in review or for input is the owner's. Measured 2026-10-30 over
//! 30 days: 248 attempts were auto-claim releases on lanes at their usage
//! limit, and tubescience-parity spent 19.2 h on attempts it parked against
//! 10.2 h on attempts it finished.
//!
//! - `GET /api/velocity?days=30` — every worker with enough attempts, most
//!   wasted hours first.
//! - `GET /api/velocity/{worker}?days=30&card=ID` — one worker: throughput,
//!   time by category, waste by owner with example cards, the top bottleneck,
//!   and (with `card`) an estimate for that card from the worker's similar
//!   finished cards, falling back to all its finished cards, then the fleet's.
//!   Every estimate says its basis and how many cards it rests on.
//! - `GET /api/velocity/{worker}/projection?include=backlog` — when the
//!   worker's open cards land: the one in doing, then todo in board order
//!   (and backlog with include=backlog), each estimated as above, ordered so a
//!   card follows the queued cards it depends on. Two answers: work time (the
//!   estimates summed, scaled by how the worker's past cards compared with
//!   their own estimates) and calendar time at the worker's recent pace. A
//!   dependency outside the queue that is not done is named, not dated.

use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(fleet))
        .route("/{worker}", get(worker))
        .route("/{worker}/projection", get(projection))
}

#[derive(Deserialize, Default)]
pub struct Q {
    days: Option<f64>,
    card: Option<String>,
    include: Option<String>,
}

/// Workers with fewer attempts than this are left out of the fleet view.
const MIN_ATTEMPTS: usize = 5;
/// An estimate from similar cards needs at least this many.
const MIN_SIMILAR: usize = 3;
/// Title similarity (Jaccard over words) that counts as similar.
const SIMILAR: f64 = 0.2;

#[derive(Clone, Debug)]
pub(crate) struct Attempt {
    pub card: String,
    pub worker: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub outcome: String,
    pub reason: String,
    /// What the card said it waited on when the attempt closed (or, for an
    /// attempt recorded before that was captured, the card's fields now).
    pub waiting: String,
}

/// What an attempt's time went to, and who can fix it.
pub(crate) fn category(a: &Attempt) -> (&'static str, &'static str) {
    let r = a.reason.trim();
    match a.outcome.as_str() {
        "done" => ("finished", ""),
        "review" => ("review", "owner"),
        "needs_input" => ("needs_input", "owner"),
        _ if r.starts_with("auto-claim released") => ("harness_claim_release", "amux"),
        _ if r.starts_with("Auto-reclaimed: lease expired") => ("lease_expired", "worker"),
        _ if r.starts_with("Auto-parked from Doing") => ("blocked", "board"),
        "parked" | "released" | "discarded" if !a.waiting.trim().is_empty() => ("parked_waiting", "dependency"),
        "parked" | "released" | "discarded" => ("parked_no_reason", "worker"),
        _ => ("abandoned", "worker"),
    }
}

/// What each category means, for the reader who has to act on it.
fn advice(kind: &str) -> &'static str {
    match kind {
        "harness_claim_release" => "amux claimed cards it could not deliver (the lane was held, usually at its usage limit); amux should not claim for a held lane",
        "lease_expired" => "cards were held while the worker sat idle or was not running, until amux reclaimed them; release a card before going idle",
        "blocked" => "cards were started before their dependencies were done; pick cards whose depends_on are done",
        "parked_waiting" => "attempts parked on something the card names (another card, a build, a person); the reasons below say what, and clearing them is where this time comes back",
        "parked_no_reason" => "attempts the worker set aside without saying what it waits on; say it on the card when parking, and split cards that are too big",
        "abandoned" => "attempts that ended without an outcome",
        "needs_input" => "time waiting on the owner; batch questions and keep working on another card meanwhile",
        "review" => "time waiting in review",
        _ => "",
    }
}

fn words(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_lowercase())
        .collect()
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

/// The q-quantile (0..1) of sorted values, nearest rank.
fn quantile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted.get(i.min(sorted.len() - 1)).copied()
}

fn spread(mut v: Vec<f64>) -> Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    json!({"p25": quantile(&v, 0.25), "p50": quantile(&v, 0.5), "p75": quantile(&v, 0.75), "n": v.len()})
}

/// Minutes and tokens per finished card: one card's attempts summed.
pub(crate) struct DoneCard {
    pub card: String,
    pub minutes: f64,
    pub tokens: Option<f64>,
}

pub(crate) fn done_cards(attempts: &[Attempt], tokens: &HashMap<String, f64>) -> Vec<DoneCard> {
    let mut minutes: BTreeMap<&str, f64> = BTreeMap::new();
    let mut finished: BTreeSet<&str> = BTreeSet::new();
    for a in attempts {
        *minutes.entry(&a.card).or_default() += (a.ended_at - a.started_at).max(0) as f64 / 60.0;
        if a.outcome == "done" {
            finished.insert(&a.card);
        }
    }
    finished
        .into_iter()
        .map(|c| DoneCard { card: c.to_string(), minutes: minutes[c], tokens: tokens.get(c).copied() })
        .collect()
}

/// An estimate for a card with `title`, from `own` (the worker's finished
/// cards) and `fleet`, saying which it rests on.
pub(crate) fn estimate(title: &str, own: &[DoneCard], fleet: &[DoneCard], titles: &HashMap<String, String>) -> Value {
    let want = words(title);
    let mut scored: Vec<(f64, &DoneCard)> = own
        .iter()
        .map(|d| (jaccard(&want, &words(titles.get(&d.card).map(String::as_str).unwrap_or(""))), d))
        .filter(|(s, _)| *s >= SIMILAR)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let (basis, set): (&str, Vec<&DoneCard>) = if scored.len() >= MIN_SIMILAR {
        ("similar cards this worker finished", scored.iter().take(10).map(|(_, d)| *d).collect())
    } else if own.len() >= MIN_SIMILAR {
        ("all cards this worker finished", own.iter().collect())
    } else {
        ("cards the whole fleet finished", fleet.iter().collect())
    };
    json!({
        "basis": basis,
        "n": set.len(),
        "minutes": spread(set.iter().map(|d| d.minutes).collect()),
        "tokens": spread(set.iter().filter_map(|d| d.tokens).collect()),
        "examples": set.iter().take(3).map(|d| d.card.clone()).collect::<Vec<_>>(),
    })
}

/// One worker's breakdown over its attempts in the window.
pub(crate) fn summarize(worker: &str, attempts: &[Attempt], tokens: &HashMap<String, f64>, days: f64) -> Value {
    let mine: Vec<&Attempt> = attempts.iter().filter(|a| a.worker == worker).collect();
    let mut by: BTreeMap<&str, (usize, f64, &str, Vec<String>)> = BTreeMap::new();
    let mut waits: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut per_card: BTreeMap<&str, usize> = BTreeMap::new();
    let mut total_h = 0.0;
    for a in &mine {
        let h = (a.ended_at - a.started_at).max(0) as f64 / 3600.0;
        total_h += h;
        let (kind, owner) = category(a);
        let e = by.entry(kind).or_insert((0, 0.0, owner, Vec::new()));
        e.0 += 1;
        e.1 += h;
        if e.3.len() < 3 && !e.3.contains(&a.card) {
            e.3.push(a.card.clone());
        }
        *per_card.entry(&a.card).or_default() += 1;
        if kind == "parked_waiting" {
            let w: String = a.waiting.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(120).collect();
            let e = waits.entry(w).or_default();
            e.0 += 1;
            e.1 += h;
        }
    }
    let mut waited_on: Vec<(String, (usize, f64))> = waits.into_iter().collect();
    waited_on.sort_by(|a, b| b.1 .1.partial_cmp(&a.1 .1).unwrap_or(std::cmp::Ordering::Equal));
    let owned: Vec<Attempt> = mine.iter().map(|a| (*a).clone()).collect();
    let done = done_cards(&owned, tokens);
    let rework: usize = per_card.values().map(|n| n.saturating_sub(1)).sum();
    let waste: Vec<Value> = by
        .iter()
        .filter(|(k, _)| **k != "finished")
        .map(|(k, (n, h, owner, ex))| json!({"kind": k, "owner": owner, "attempts": n, "hours": (h * 10.0).round() / 10.0,
            "share_of_time": if total_h > 0.0 { ((h / total_h) * 100.0).round() } else { 0.0 }, "examples": ex, "what_it_means": advice(k)}))
        .collect();
    // The bottleneck is the waste that cost the most time; a category that
    // costs no time but happens a lot (claim releases) leads only by count.
    let top = waste
        .iter()
        .max_by(|a, b| {
            let key = |v: &Value| (v["hours"].as_f64().unwrap_or(0.0), v["attempts"].as_u64().unwrap_or(0));
            key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();
    let finished_h = by.get("finished").map(|e| e.1).unwrap_or(0.0);
    json!({
        "worker": worker,
        "window_days": days,
        "measured": true,
        "n_considered": mine.len(),
        "attempts": mine.len(),
        "cards": per_card.len(),
        "finished_cards": done.len(),
        "finished_per_day": if days > 0.0 { ((done.len() as f64 / days) * 100.0).round() / 100.0 } else { 0.0 },
        "hours": {"total": (total_h * 10.0).round() / 10.0, "finished": (finished_h * 10.0).round() / 10.0},
        "finished_share_of_time": if total_h > 0.0 { ((finished_h / total_h) * 100.0).round() } else { 0.0 },
        "rework_attempts": rework,
        "attempts_per_card": if per_card.is_empty() { 0.0 } else { ((mine.len() as f64 / per_card.len() as f64) * 100.0).round() / 100.0 },
        "per_finished_card": {"minutes": spread(done.iter().map(|d| d.minutes).collect()),
                              "tokens": spread(done.iter().filter_map(|d| d.tokens).collect())},
        "waste": waste,
        "top_bottleneck": top,
        // What parked attempts were waiting on, costliest first.
        "waited_on": waited_on.iter().take(8).map(|(w, (n, h))| json!({"waiting": w, "attempts": n, "hours": (h * 10.0).round() / 10.0})).collect::<Vec<_>>(),
    })
}

struct Data {
    attempts: Vec<Attempt>,
    tokens: HashMap<String, f64>,
    titles: HashMap<String, String>,
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| Ok(()))
        .is_ok()
}

fn load(conn: &rusqlite::Connection, since: i64) -> rusqlite::Result<Data> {
    // The episodes job creates task_episodes on its first tick, so a new
    // install has no history yet: an empty answer, not a 500.
    if !table_exists(conn, "task_episodes") {
        return Ok(Data { attempts: Vec::new(), tokens: HashMap::new(), titles: HashMap::new() });
    }
    let mut stmt = conn.prepare(
        "SELECT e.card, e.worker, e.started_at, e.ended_at, e.outcome, COALESCE(e.reason, ''),
                COALESCE(e.card_end, ''),
                TRIM(COALESCE(i.blocked_on, '') || ' ' || COALESCE(i.waiting_on, '') || ' ' || COALESCE(i.source_ref, ''))
         FROM task_episodes e LEFT JOIN issues i ON i.id = e.card
         WHERE e.started_at >= ?1 AND e.ended_at IS NOT NULL",
    )?;
    let attempts = stmt
        .query_map([since], |r| {
            let end: String = r.get(6)?;
            let now: String = r.get(7)?;
            Ok(Attempt { card: r.get(0)?, worker: r.get(1)?, started_at: r.get(2)?, ended_at: r.get(3)?,
                outcome: r.get::<_, Option<String>>(4)?.unwrap_or_default(), reason: r.get(5)?,
                waiting: waiting_from(&end).unwrap_or(now) })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let tokens = if table_exists(conn, "token_ledger") {
        let mut stmt = conn.prepare(
            "SELECT task, SUM(input + cache_write + output) FROM token_ledger WHERE task <> '' AND ts >= ?1 GROUP BY task",
        )?;
        let rows = stmt.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?.collect::<Result<HashMap<_, _>, _>>()?;
        rows
    } else {
        HashMap::new()
    };
    let ids: BTreeSet<&str> = attempts.iter().map(|a| a.card.as_str()).collect();
    let mut titles = HashMap::new();
    let mut stmt = conn.prepare("SELECT id, COALESCE(title, '') FROM issues WHERE id = ?1")?;
    for id in ids {
        if let Ok(t) = stmt.query_row([id], |r| r.get::<_, String>(1)) {
            titles.insert(id.to_string(), t);
        }
    }
    Ok(Data { attempts, tokens, titles })
}

/// What a card said it waited on, from the attempt's end snapshot; None when
/// the snapshot predates that capture.
fn waiting_from(card_end: &str) -> Option<String> {
    let v: Value = serde_json::from_str(card_end).ok()?;
    let w = v.get("waiting")?;
    let parts: Vec<&str> = ["blocked_on", "waiting_on", "depends_on", "source_ref"]
        .iter()
        .filter_map(|k| w.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    Some(parts.join(" | "))
}

/// The few lines a worker's scoped context carries about its own velocity
/// (step 4 of docs/design/worker-velocity.md): reach the worker by default,
/// not behind an endpoint it has to know to ask. Composed when the context
/// is (launch, rules change), so it says when; `amux velocity` reads it live.
/// Empty with too little history, so a new worker gets no noise.
pub(crate) fn context_section(worker: &str) -> String {
    let path = crate::config::ServerConfig::from_process_env().db_path;
    let Ok(conn) = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return String::new();
    };
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let days = 30.0;
    let since = chrono::Utc::now().timestamp() - (days * 86400.0) as i64;
    let Ok(d) = load(&conn, since) else { return String::new() };
    context_lines(&summarize(worker, &d.attempts, &d.tokens, days), &chrono::Local::now().format("%Y-%m-%d").to_string())
}

/// `context` with its "# Your velocity" section removed. The section's
/// numbers move whenever any attempt ends, so the after-update reconcile,
/// which re-sends a worker its instructions when they differ from its rules
/// file, compares contexts without it: with it, the first deploy after
/// 140ff3b8b told 14 running workers to re-read their context, and every
/// later deploy would have done the same for nearly all of them.
pub(crate) fn without_section(context: &str) -> String {
    let Some(start) = context.find("# Your velocity (") else { return context.to_string() };
    let rest = &context[start..];
    let end = rest[1..].find("\n# ").map(|i| start + 1 + i + 1).unwrap_or(context.len());
    format!("{}{}", &context[..start], &context[end..])
}

pub(crate) fn context_lines(s: &Value, as_of: &str) -> String {
    if s["attempts"].as_u64().unwrap_or(0) < MIN_ATTEMPTS as u64 {
        return String::new();
    }
    let mut out = format!(
        "# Your velocity (last 30 days, as of {as_of})\n\nYou finished {} of {} cards ({} a day); {}% of your attempt time went to attempts that finished, and you averaged {} attempts per card.",
        s["finished_cards"], s["cards"], s["finished_per_day"], s["finished_share_of_time"], s["attempts_per_card"]
    );
    if let Some(t) = s["top_bottleneck"].as_object() {
        out.push_str(&format!(
            "\nYour biggest waste: {} ({} attempts, {} h, {}% of your time). {}.",
            t.get("kind").and_then(Value::as_str).unwrap_or(""),
            t.get("attempts").cloned().unwrap_or_default(),
            t.get("hours").cloned().unwrap_or_default(),
            t.get("share_of_time").cloned().unwrap_or_default(),
            t.get("what_it_means").and_then(Value::as_str).unwrap_or("")
        ));
    }
    if let Some(w) = s["waited_on"].as_array().and_then(|a| a.first()) {
        out.push_str(&format!("\nParked work waited longest on: \"{}\" ({} h).", w["waiting"].as_str().unwrap_or(""), w["hours"]));
    }
    out.push_str("\n`amux velocity` shows this live with every category; `amux velocity --card <ID>` estimates a card's minutes and tokens from your finished cards like it.");
    out
}

/// An open card in a worker's queue.
#[derive(Clone, Debug)]
pub(crate) struct QCard {
    pub id: String,
    pub title: String,
    pub status: String,
    pub depends_on: Vec<String>,
    pub blocked_on: String,
    /// Start of the card's open attempt, when one is running.
    pub attempt_started_at: Option<i64>,
}

/// How a worker's finished cards compared with what this estimator would have
/// said for each, with that card held out: the median of actual / estimate.
/// None with fewer than MIN_SIMILAR + 1 finished cards.
pub(crate) fn calibration(own: &[DoneCard], titles: &HashMap<String, String>) -> Option<(f64, usize)> {
    if own.len() <= MIN_SIMILAR {
        return None;
    }
    let mut ratios: Vec<f64> = own
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let rest: Vec<DoneCard> = own.iter().enumerate().filter(|(j, _)| *j != i)
                .map(|(_, x)| DoneCard { card: x.card.clone(), minutes: x.minutes, tokens: x.tokens }).collect();
            let e = estimate(titles.get(&d.card).map(String::as_str).unwrap_or(""), &rest, &rest, titles);
            let p50 = e["minutes"]["p50"].as_f64()?;
            (p50 > 0.0).then_some(d.minutes / p50)
        })
        .collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    quantile(&ratios, 0.5).map(|r| (r, ratios.len()))
}

/// Order the queue so a card comes after the queued cards it depends on,
/// keeping board order otherwise.
fn dependency_order(queue: &[QCard]) -> Vec<QCard> {
    let ids: BTreeSet<&str> = queue.iter().map(|c| c.id.as_str()).collect();
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    let mut left: Vec<QCard> = queue.to_vec();
    while !left.is_empty() {
        let at = left
            .iter()
            .position(|c| c.depends_on.iter().all(|d| !ids.contains(d.as_str()) || placed.contains(d)))
            .unwrap_or(0); // a cycle: take the next in board order
        let c = left.remove(at);
        placed.insert(c.id.clone());
        out.push(c);
    }
    out
}

/// The worker's recent pace: how many cards it finished in how many days,
/// and the moment the projection starts from.
#[derive(Clone, Copy)]
pub(crate) struct Pace {
    pub finished_cards: usize,
    pub days: f64,
    pub now: i64,
}

/// The projection for a worker's queue. `open_elsewhere` holds dependency ids
/// outside the queue that are not done.
pub(crate) fn project(
    queue: &[QCard],
    own: &[DoneCard],
    fleet: &[DoneCard],
    titles: &HashMap<String, String>,
    open_elsewhere: &BTreeSet<String>,
    pace_in: &Pace,
) -> Value {
    let Pace { finished_cards, days, now } = *pace_in;
    let order = dependency_order(queue);
    let cal = calibration(own, titles);
    let factor = cal.map(|c| c.0).unwrap_or(1.0);
    let (mut p50_sum, mut p75_sum) = (0.0, 0.0);
    let mut rows = Vec::new();
    for c in &order {
        let e = estimate(&c.title, own, fleet, titles);
        let p50 = e["minutes"]["p50"].as_f64().unwrap_or(0.0);
        let p75 = e["minutes"]["p75"].as_f64().unwrap_or(p50);
        p50_sum += p50;
        p75_sum += p75;
        let blocked_by: Vec<&String> = c.depends_on.iter().filter(|d| open_elsewhere.contains(*d)).collect();
        // Running past what the worker's similar cards took (Devin's session
        // cap, as a flag): the open attempt has run longer than the
        // pessimistic estimate, scaled by the worker's calibration.
        let over = c.attempt_started_at.and_then(|t| {
            let elapsed = (now - t).max(0) as f64 / 60.0;
            let limit = p75 * factor;
            (limit > 0.0 && elapsed > limit).then(|| json!({"elapsed_minutes": elapsed.round(), "expected_minutes_p75": limit.round()}))
        });
        if over.is_some() {
            tracing::info!(card = %c.id, verdict = "velocity_card_over_estimate", measured = true, n_considered = 1,
                "a card's open attempt has run past its estimate");
        }
        rows.push(json!({
            "over_estimate": over,
            "card": c.id, "title": c.title, "status": c.status,
            "minutes_p50": p50, "minutes_p75": p75, "basis": e["basis"], "n": e["n"],
            "work_done_after_minutes": (p50_sum * factor).round(),
            "blocked_by": blocked_by, "blocked_on": c.blocked_on,
        }));
    }
    // Calendar time at the worker's recent pace: days per finished card,
    // which already counts idle time, waiting and parked attempts.
    let pace = (finished_cards > 0).then(|| days / finished_cards as f64);
    let spread_ratio = if p50_sum > 0.0 { p75_sum / p50_sum } else { 1.0 };
    let at = |d: f64| chrono::DateTime::from_timestamp(now + (d * 86400.0) as i64, 0).map(|t| t.to_rfc3339());
    let blocked: BTreeSet<&String> = order.iter().flat_map(|c| c.depends_on.iter()).filter(|d| open_elsewhere.contains(*d)).collect();
    json!({
        "measured": true,
        "n_considered": order.len(),
        "cards": rows,
        "work_minutes": {"p50": (p50_sum * factor).round(), "p75": (p75_sum * factor).round(),
                          "uncalibrated_p50": p50_sum.round()},
        "calibration": cal.map(|(r, n)| json!({"actual_over_estimate": (r * 100.0).round() / 100.0, "cards": n,
            "meaning": "the worker's past finished cards took this many times the estimate, each estimated with itself held out"})),
        "pace": pace.map(|p| json!({"days_per_finished_card": (p * 100.0).round() / 100.0, "window_days": days, "finished_cards": finished_cards})),
        "finish_by": pace.map(|p| json!({"p50": at(p * order.len() as f64), "p75": at(p * order.len() as f64 * spread_ratio)})),
        "blocked_by_outside": blocked,
        "note": if blocked.is_empty() { "" } else { "cards wait on work outside this queue (blocked_by_outside); the dates assume it lands in time" },
    })
}

/// A worker's open cards, dependencies outside the queue that are not done,
/// and the queued cards' titles.
type QueueData = (Vec<QCard>, BTreeSet<String>, HashMap<String, String>);

async fn projection(State(state): State<AppState>, Path(worker): Path<String>, Query(q): Query<Q>) -> Response {
    let (days, since) = window(&q);
    let d = match with_data(&state, since).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let with_backlog = q.include.as_deref() == Some("backlog");
    let store = state.store.clone();
    let w = worker.clone();
    let loaded = tokio::task::spawn_blocking(move || -> anyhow::Result<QueueData> {
        let conn = store.read()?;
        let statuses = if with_backlog { "'doing','todo','backlog'" } else { "'doing','todo'" };
        let mut stmt = conn.prepare(&format!(
            "SELECT id, COALESCE(title,''), status, COALESCE(depends_on,''), COALESCE(blocked_on,''),
                    (SELECT MAX(started_at) FROM task_episodes e WHERE e.card = issues.id AND e.ended_at IS NULL) FROM issues
             WHERE session = ?1 AND status IN ({statuses}) AND COALESCE(archived,0) = 0 AND deleted IS NULL
             ORDER BY CASE status WHEN 'doing' THEN 0 WHEN 'todo' THEN 1 ELSE 2 END, pos"
        ))?;
        let queue: Vec<QCard> = stmt
            .query_map([&w], |r| {
                let deps: String = r.get(3)?;
                Ok(QCard { id: r.get(0)?, title: r.get(1)?, status: r.get(2)?,
                    depends_on: serde_json::from_str::<Vec<String>>(&deps).unwrap_or_default(), blocked_on: r.get(4)?,
                    attempt_started_at: r.get(5)? })
            })?
            .collect::<Result<_, _>>()?;
        let ids: BTreeSet<&str> = queue.iter().map(|c| c.id.as_str()).collect();
        let mut open = BTreeSet::new();
        let mut st = conn.prepare("SELECT status FROM issues WHERE id = ?1")?;
        for dep in queue.iter().flat_map(|c| c.depends_on.iter()).filter(|d| !ids.contains(d.as_str())) {
            let s: Option<String> = st.query_row([dep], |r| r.get(0)).ok();
            if !matches!(s.as_deref(), Some("done") | Some("verified") | Some("discarded")) {
                open.insert(dep.clone());
            }
        }
        let mut titles = HashMap::new();
        for c in &queue {
            titles.insert(c.id.clone(), c.title.clone());
        }
        Ok((queue, open, titles))
    })
    .await;
    let (queue, open, qtitles) = match loaded {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e:#}"), "measured": false, "n_considered": 0}))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string(), "measured": false, "n_considered": 0}))).into_response(),
    };
    let mut titles = d.titles.clone();
    titles.extend(qtitles);
    let mine: Vec<Attempt> = d.attempts.iter().filter(|a| a.worker == worker).cloned().collect();
    let own = done_cards(&mine, &d.tokens);
    let fleet = done_cards(&d.attempts, &d.tokens);
    let mut out = project(&queue, &own, &fleet, &titles, &open, &Pace { finished_cards: own.len(), days, now: chrono::Utc::now().timestamp() });
    out["worker"] = json!(worker);
    out["include_backlog"] = json!(with_backlog);
    if queue.is_empty() {
        out["measured"] = json!(false);
        out["why_unmeasured"] = json!(format!("{worker} has no open cards in doing or todo{}", if with_backlog { " or backlog" } else { "" }));
    }
    Json(out).into_response()
}

fn window(q: &Q) -> (f64, i64) {
    let days = q.days.unwrap_or(30.0).clamp(1.0, 365.0);
    (days, chrono::Utc::now().timestamp() - (days * 86400.0) as i64)
}

async fn with_data(state: &AppState, since: i64) -> Result<Data, Response> {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Data> { let conn = store.read()?;
        Ok(load(&conn, since)?) }).await {
        Ok(Ok(d)) => Ok(d),
        Ok(Err(e)) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e:#}"), "measured": false, "n_considered": 0, "why_unmeasured": "the attempt history could not be read"}))).into_response()),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string(), "measured": false, "n_considered": 0}))).into_response()),
    }
}

async fn fleet(State(state): State<AppState>, Query(q): Query<Q>) -> Response {
    let (days, since) = window(&q);
    let d = match with_data(&state, since).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let workers: BTreeSet<&str> = d.attempts.iter().map(|a| a.worker.as_str()).collect();
    let mut rows: Vec<Value> = workers
        .into_iter()
        .filter(|w| d.attempts.iter().filter(|a| a.worker == *w).count() >= MIN_ATTEMPTS)
        .map(|w| {
            let s = summarize(w, &d.attempts, &d.tokens, days);
            let wasted = s["hours"]["total"].as_f64().unwrap_or(0.0) - s["hours"]["finished"].as_f64().unwrap_or(0.0);
            json!({"worker": w, "attempts": s["attempts"], "finished_cards": s["finished_cards"], "finished_per_day": s["finished_per_day"],
                   "finished_share_of_time": s["finished_share_of_time"], "attempts_per_card": s["attempts_per_card"],
                   "wasted_hours": (wasted * 10.0).round() / 10.0, "top_bottleneck": s["top_bottleneck"]["kind"]})
        })
        .collect();
    rows.sort_by(|a, b| b["wasted_hours"].as_f64().partial_cmp(&a["wasted_hours"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    Json(json!({"window_days": days, "measured": true, "n_considered": d.attempts.len(), "workers": rows,
                "note": "per worker: GET /api/velocity/{worker}?card=ID"})).into_response()
}

async fn worker(State(state): State<AppState>, Path(worker): Path<String>, Query(q): Query<Q>) -> Response {
    let (days, since) = window(&q);
    let d = match with_data(&state, since).await {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut out = summarize(&worker, &d.attempts, &d.tokens, days);
    if out["attempts"].as_u64() == Some(0) {
        out["measured"] = json!(false);
        out["why_unmeasured"] = json!(format!("no finished attempts by {worker} in the last {days} days"));
    }
    if let Some(card) = q.card.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        let mine: Vec<Attempt> = d.attempts.iter().filter(|a| a.worker == worker).cloned().collect();
        let own = done_cards(&mine, &d.tokens);
        let fleet = done_cards(&d.attempts, &d.tokens);
        let title = match d.titles.get(card) {
            Some(t) => t.clone(),
            None => {
                let store = state.store.clone();
                let id = card.to_string();
                tokio::task::spawn_blocking(move || {
                    store.read().ok().and_then(|c| c.query_row("SELECT COALESCE(title,'') FROM issues WHERE id = ?1", [&id], |r| r.get::<_, String>(0)).ok())
                })
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
            }
        };
        let mut e = estimate(&title, &own, &fleet, &d.titles);
        e["card"] = json!(card);
        e["title"] = json!(title);
        out["estimate"] = e;
    }
    Json(out).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(card: &str, worker: &str, minutes: i64, outcome: &str, reason: &str) -> Attempt {
        Attempt { card: card.into(), worker: worker.into(), started_at: 1_000, ended_at: 1_000 + minutes * 60, outcome: outcome.into(), reason: reason.into(), waiting: String::new() }
    }

    #[test]
    fn waste_is_split_by_who_can_fix_it_and_the_bottleneck_is_the_costliest() {
        let rows = vec![
            a("C1", "w", 30, "done", ""),
            a("C2", "w", 90, "parked", ""),
            a("C2", "w", 20, "done", ""),
            a("C3", "w", 0, "released", "auto-claim released: work prompt could not be queued (held: usage limit)"),
            a("C4", "w", 40, "abandoned", "Auto-reclaimed: lease expired (holder_idle_past_ttl), holder 'w'"),
            a("C5", "w", 10, "parked", "Auto-parked from Doing: unresolved depends_on=X-1[todo]"),
            a("C9", "other", 500, "parked", ""),
        ];
        let s = summarize("w", &rows, &HashMap::from([("C1".to_string(), 1000.0)]), 30.0);
        assert_eq!(s["attempts"], 6, "another worker's attempts are not counted");
        assert_eq!(s["finished_cards"], 2);
        assert_eq!(s["rework_attempts"], 1, "C2 took two attempts");
        let kind = |k: &str| s["waste"].as_array().unwrap().iter().find(|w| w["kind"] == k).cloned().unwrap();
        assert_eq!(kind("harness_claim_release")["owner"], "amux");
        assert_eq!(kind("lease_expired")["owner"], "worker");
        assert_eq!(kind("blocked")["owner"], "board");
        assert_eq!(kind("parked_no_reason")["hours"], 1.5);
        assert_eq!(s["top_bottleneck"]["kind"], "parked_no_reason", "the costliest waste leads");
        // A finished card's minutes include its rework: C2 is 90 + 20.
        assert_eq!(s["per_finished_card"]["minutes"]["p25"].as_f64(), Some(30.0));
        assert_eq!(s["per_finished_card"]["minutes"]["p75"].as_f64(), Some(110.0));
    }

    #[test]
    fn a_park_that_names_what_it_waits_on_is_told_apart_from_one_that_does_not() {
        let mut waiting = a("W1", "w", 60, "parked", "");
        waiting.waiting = "after the b13b roll serves on the TS shard".into();
        let rows = vec![waiting, a("W2", "w", 30, "parked", ""), a("W3", "w", 10, "done", "")];
        let s = summarize("w", &rows, &HashMap::new(), 30.0);
        let kind = |k: &str| s["waste"].as_array().unwrap().iter().find(|w| w["kind"] == k).cloned();
        assert_eq!(kind("parked_waiting").unwrap()["owner"], "dependency");
        assert_eq!(kind("parked_no_reason").unwrap()["hours"], 0.5);
        assert_eq!(s["waited_on"][0]["waiting"], "after the b13b roll serves on the TS shard", "what it waited on is listed");
        assert_eq!(waiting_from(r#"{"waiting":{"blocked_on":"MG-2292","source_ref":"shard hold","depends_on":""}}"#).as_deref(), Some("MG-2292 | shard hold"));
        assert_eq!(waiting_from(r#"{"status":"backlog"}"#), None, "an older snapshot falls back to the card's fields now");
    }

    #[test]
    fn the_context_section_names_the_biggest_waste_and_skips_a_thin_history() {
        let mut rows: Vec<Attempt> = (0..4).map(|i| a(&format!("D{i}"), "w", 20, "done", "")).collect();
        let mut parked = a("P1", "w", 180, "parked", "");
        parked.waiting = "MG-2292 shard hold".into();
        rows.push(parked);
        let text = context_lines(&summarize("w", &rows, &HashMap::new(), 30.0), "2026-10-30");
        assert!(text.contains("You finished 4 of 5 cards"), "{text}");
        assert!(text.contains("Your biggest waste: parked_waiting"), "{text}");
        assert!(text.contains("\"MG-2292 shard hold\""), "{text}");
        assert!(text.contains("amux velocity"), "{text}");
        assert_eq!(context_lines(&summarize("w", &rows[..2], &HashMap::new(), 30.0), "2026-10-30"), "", "too little history adds nothing");
    }

    #[test]
    fn a_projection_orders_by_dependency_names_outside_blockers_and_dates_at_the_workers_pace() {
        let q = |id: &str, deps: &[&str]| QCard { id: id.into(), title: format!("move {id}"), status: "todo".into(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(), blocked_on: String::new(), attempt_started_at: None };
        // C depends on B, which comes later in board order; B waits on X outside the queue.
        let queue = vec![q("C", &["B"]), q("B", &["X"]), q("A", &[])];
        let own: Vec<DoneCard> = (0..4).map(|i| DoneCard { card: format!("D{i}"), minutes: 10.0, tokens: None }).collect();
        let titles = HashMap::new();
        let open = BTreeSet::from(["X".to_string()]);
        let p = project(&queue, &own, &own, &titles, &open, &Pace { finished_cards: 4, days: 8.0, now: 0 });
        let ids: Vec<&str> = p["cards"].as_array().unwrap().iter().map(|c| c["card"].as_str().unwrap()).collect();
        assert_eq!(ids, ["B", "C", "A"], "a card follows the queued card it depends on");
        assert_eq!(p["blocked_by_outside"][0], "X", "an outside dependency is named, not dated");
        assert_eq!(p["work_minutes"]["p50"].as_f64(), Some(30.0), "three cards at the worker's 10-minute median, calibration 1");
        assert_eq!(p["pace"]["days_per_finished_card"].as_f64(), Some(2.0), "4 finished in 8 days");
        assert_eq!(p["finish_by"]["p50"].as_str().map(|s| s.starts_with("1970-01-07")), Some(true), "three cards at 2 days each");
        // A card in doing whose open attempt ran 25 min against a 10 min p75 is flagged; 5 min is not.
        let mut doing = q("D", &[]);
        doing.attempt_started_at = Some(-25 * 60);
        let p = project(&[doing.clone()], &own, &own, &titles, &BTreeSet::new(), &Pace { finished_cards: 4, days: 8.0, now: 0 });
        assert_eq!(p["cards"][0]["over_estimate"]["elapsed_minutes"].as_f64(), Some(25.0));
        doing.attempt_started_at = Some(-5 * 60);
        let p = project(&[doing], &own, &own, &titles, &BTreeSet::new(), &Pace { finished_cards: 4, days: 8.0, now: 0 });
        assert!(p["cards"][0]["over_estimate"].is_null(), "inside its estimate");
    }

    #[test]
    fn the_reconcile_compares_contexts_without_the_velocity_numbers() {
        let a = "# Rules\n\nx\n\n# Your velocity (last 30 days, as of 2026-10-30)\n\nYou finished 4 of 5 cards.\n\n# Browser\n\ny";
        let b = "# Rules\n\nx\n\n# Your velocity (last 30 days, as of 2026-10-31)\n\nYou finished 9 of 9 cards.\n\n# Browser\n\ny";
        assert_eq!(without_section(a), without_section(b), "moving numbers are not a change of instructions");
        assert_ne!(without_section(a), without_section(&a.replace("# Browser", "# Browsers")), "a real change still is");
        assert_eq!(without_section("# Rules\n\nx"), "# Rules\n\nx");
    }

    #[test]
    fn a_new_install_with_no_attempt_history_reads_as_empty() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let d = load(&conn, 0).expect("no task_episodes table yet is an empty history, not an error");
        assert!(d.attempts.is_empty() && d.tokens.is_empty());
    }

    #[test]
    fn an_estimate_uses_the_workers_similar_cards_then_its_own_then_the_fleets() {
        let titles: HashMap<String, String> = [
            ("A1", "fix the board drawer layout on phones"),
            ("A2", "fix the board drawer scroll on phones"),
            ("A3", "board drawer phones fix keyboard"),
            ("B1", "write the launch blog post"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let own = vec![
            DoneCard { card: "A1".into(), minutes: 20.0, tokens: None },
            DoneCard { card: "A2".into(), minutes: 30.0, tokens: None },
            DoneCard { card: "A3".into(), minutes: 40.0, tokens: None },
            DoneCard { card: "B1".into(), minutes: 300.0, tokens: None },
        ];
        let e = estimate("fix the board drawer on phones", &own, &own, &titles);
        assert_eq!(e["basis"], "similar cards this worker finished");
        assert_eq!(e["minutes"]["p50"].as_f64(), Some(30.0), "the unrelated 300-minute card is not in it");
        let e = estimate("rotate the TLS certificates", &own, &own, &titles);
        assert_eq!(e["basis"], "all cards this worker finished");
        let e = estimate("anything", &own[..1], &own, &titles);
        assert_eq!(e["basis"], "cards the whole fleet finished", "too little of its own history falls back to the fleet");
    }
}
