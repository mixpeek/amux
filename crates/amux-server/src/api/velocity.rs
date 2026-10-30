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
    Router::new().route("/", get(fleet)).route("/{worker}", get(worker))
}

#[derive(Deserialize, Default)]
pub struct Q {
    days: Option<f64>,
    card: Option<String>,
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

fn load(conn: &rusqlite::Connection, since: i64) -> rusqlite::Result<Data> {
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
    let mut stmt = conn.prepare(
        "SELECT task, SUM(input + cache_write + output) FROM token_ledger WHERE task <> '' AND ts >= ?1 GROUP BY task",
    )?;
    let tokens = stmt.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?.collect::<Result<HashMap<_, _>, _>>()?;
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
