//! Outcomes: the conversation unit of intent-first amux
//! (docs/design/intent-first.md, phase 2).
//!
//! An outcome is a Messages thread, `o:<id>`, routed to one worker. The owner
//! says what they want; `/api/intent/plan` decides where it goes; the request
//! is posted into the outcome thread, which delivers it to the worker, and the
//! worker's answer comes back into the same thread (`conversations::relay_turn`
//! posts a turn's final text when the worker did not post itself). A follow-up
//! in the thread goes to the same worker with no routing call. "Change" moves
//! the outcome to another worker, re-delivers the request there, and is logged
//! as an override: the override rate is the router's accuracy.
//!
//! No new primitive (A1): the thread is `conversation_posts`, the worker is a
//! worker, and `intent_outcomes` only links them and keeps the decision and the
//! messages it came from.
//!
//! Routes:
//! - `POST /api/intent/outcomes {text, outcome?}`: start one, or follow up.
//! - `GET  /api/intent/outcomes`: every outcome, newest first, with status.
//! - `GET  /api/intent/outcomes/{id}`: one, with its posts.
//! - `POST /api/intent/outcomes/{id}/change {worker? | new: true}`.
//! - `GET  /api/intent/stats`: decisions, overrides and the override rate.
//! - `GET  /api/intent/home`: which first screen this person gets.

use super::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/intent/outcomes", get(list).post(start))
        .route("/api/intent/outcomes/{id}", get(one))
        .route("/api/intent/outcomes/{id}/change", post(change))
        .route("/api/intent/stats", get(stats))
        .route("/api/intent/home", get(home))
        .route("/api/intent/helpers", get(helpers))
        .route("/api/intent/tidy", post(tidy))
}

/// Which helpers amux created for outcomes have gone idle: every outcome
/// routed to them is older than `idle_days` and no active goal uses them.
/// Pure, so the rule is tested without a fleet.
pub(crate) fn idle_helpers(created_by_amux: &[String], outcomes: &[OutcomeRow], goal_workers: &[String], now_ms: i64, idle_days: i64)
    -> Vec<String> {
    let cutoff = now_ms - idle_days * 86_400_000;
    created_by_amux.iter().filter(|w| {
        !goal_workers.contains(w)
            && !outcomes.iter().any(|o| &o.worker == *w && o.updated_ms >= cutoff)
    }).cloned().collect()
}

/// Phase 5, self-configuration: stop helpers amux created for outcomes once
/// they have been idle a while (a stopped chat helper holds nothing and
/// starts again on the next request; archiving stays a person's call). Run
/// daily by a shell schedule.
async fn tidy(State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>) -> Response {
    let idle_days: i64 = q.get("idle_days").and_then(|v| v.parse().ok()).unwrap_or(7).max(1);
    let home = crate::config::amux_home();
    let mine: Vec<String> = super::orchestrate::roster_entries(&home).into_iter()
        .filter(|w| !w.isolated)
        .filter(|w| crate::config::parse_env_file(&home.join("sessions").join(format!("{}.env", w.name)))
            .get("CC_CREATOR").map(|c| c == "amux").unwrap_or(false))
        .map(|w| w.name).collect();
    let rows = match all_outcomes(&state) {
        Ok(r) => r,
        Err(e) => return Json(json!({"stopped": [], "measured": false, "n_considered": 0, "why_unmeasured": e})).into_response(),
    };
    let goal_workers: Vec<String> = state.store.read().ok().and_then(|c| {
        let mut st = c.prepare("SELECT worker FROM intent_items WHERE kind='goal' AND status='active'").ok()?;
        let r = st.query_map([], |r| r.get::<_, String>(0)).ok()?;
        Some(r.flatten().collect())
    }).unwrap_or_default();
    let idle = idle_helpers(&mine, &rows, &goal_workers, now_ms(), idle_days);
    let mut stopped = vec![];
    for w in &idle {
        if super::session_verbs::is_running(w).await {
            let (ok, msg) = super::session_verbs::stop_session_process(w).await;
            stopped.push(json!({"worker": w, "ok": ok, "detail": msg}));
        }
    }
    tracing::info!(considered = mine.len(), idle = idle.len(), stopped = stopped.len(), measured = true, n_considered = mine.len(),
        verdict = "intent_tidy", "intent: idle helpers created for outcomes were stopped");
    Json(json!({"stopped": stopped, "idle": idle, "idle_days": idle_days, "measured": true, "n_considered": mine.len()})).into_response()
}

/// The workers an outcome may be moved to ("Change"): every routable worker
/// (never an isolated one, A9), with its description.
async fn helpers() -> Response {
    let roster = super::orchestrate::roster_entries(&crate::config::amux_home());
    let n = roster.len();
    let list: Vec<Value> = roster.into_iter().filter(|w| !w.isolated)
        .map(|w| json!({"name": w.name, "desc": w.desc, "worker_type": w.worker_type})).collect();
    Json(json!({"helpers": list, "measured": true, "n_considered": n})).into_response()
}

// ---- the outcome -> worker registry -----------------------------------------

/// Which worker each outcome is routed to, read by `Conv::parse` (which has no
/// store). Loaded from `intent_outcomes` at boot and written through on every
/// create and change, so the table stays the one source (A10).
fn registry() -> &'static RwLock<HashMap<String, String>> {
    static R: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(crate) fn outcome_worker(id: &str) -> Option<String> {
    registry().read().ok()?.get(id).cloned().filter(|w| !w.is_empty())
}

fn remember(id: &str, worker: &str) {
    if let Ok(mut m) = registry().write() {
        m.insert(id.to_string(), worker.to_string());
    }
}

#[cfg(test)]
pub(crate) fn test_route(id: &str, worker: &str) {
    remember(id, worker);
}

/// Fill the registry from the table. Called once at boot.
pub(crate) fn load_registry(state: &AppState) {
    let rows: Vec<(String, String)> = state
        .store
        .read()
        .ok()
        .and_then(|c| {
            let mut st = c.prepare("SELECT id, worker FROM intent_outcomes").ok()?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).ok()?;
            Some(rows.flatten().collect())
        })
        .unwrap_or_default();
    let n = rows.len();
    for (id, w) in rows {
        remember(&id, &w);
    }
    tracing::info!(measured = true, n_considered = n, verdict = "intent_registry_loaded",
        "intent: outcome routing loaded from intent_outcomes");
}

// ---- storage -----------------------------------------------------------------

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    format!("o{}", ulid::Ulid::new().to_string().to_lowercase())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OutcomeRow {
    pub id: String,
    pub title: String,
    pub worker: String,
    pub decision: Value,
    pub sources: Value,
    pub overrides: Value,
    pub item_id: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

fn parse_json(s: String, empty: Value) -> Value {
    serde_json::from_str(&s).unwrap_or(empty)
}

fn row_of(r: &rusqlite::Row) -> rusqlite::Result<OutcomeRow> {
    Ok(OutcomeRow {
        id: r.get(0)?,
        title: r.get(1)?,
        worker: r.get(2)?,
        decision: parse_json(r.get(3)?, json!({})),
        sources: parse_json(r.get(4)?, json!([])),
        overrides: parse_json(r.get(5)?, json!([])),
        item_id: r.get(6)?,
        created_ms: r.get(7)?,
        updated_ms: r.get(8)?,
    })
}

const COLS: &str = "id, title, worker, decision, sources, overrides, item_id, created_ms, updated_ms";

pub(crate) fn get_outcome(state: &AppState, id: &str) -> Option<OutcomeRow> {
    let c = state.store.read().ok()?;
    c.query_row(&format!("SELECT {COLS} FROM intent_outcomes WHERE id=?1"), [id], row_of).ok()
}

pub(crate) fn all_outcomes(state: &AppState) -> Result<Vec<OutcomeRow>, String> {
    let c = state.store.read().map_err(|e| e.to_string())?;
    let mut st = c
        .prepare(&format!("SELECT {COLS} FROM intent_outcomes ORDER BY updated_ms DESC LIMIT 500"))
        .map_err(|e| e.to_string())?;
    let rows = st.query_map([], row_of).map_err(|e| e.to_string())?;
    Ok(rows.flatten().collect())
}

pub(crate) async fn insert_outcome(state: &AppState, o: OutcomeRow) -> Result<(), String> {
    remember(&o.id, &o.worker);
    state
        .store
        .write_async(move |c| {
            c.execute(
                "INSERT INTO intent_outcomes (id, title, worker, decision, sources, overrides, item_id, created_ms, updated_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                rusqlite::params![o.id, o.title, o.worker, o.decision.to_string(), o.sources.to_string(),
                    o.overrides.to_string(), o.item_id, o.created_ms, o.updated_ms],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn update_outcome(state: &AppState, o: OutcomeRow) -> Result<(), String> {
    remember(&o.id, &o.worker);
    state
        .store
        .write_async(move |c| {
            c.execute(
                "UPDATE intent_outcomes SET title=?2, worker=?3, decision=?4, sources=?5, overrides=?6, updated_ms=?7 WHERE id=?1",
                rusqlite::params![o.id, o.title, o.worker, o.decision.to_string(), o.sources.to_string(),
                    o.overrides.to_string(), now_ms()],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The posts of one outcome thread, oldest first.
pub(crate) fn posts_of(state: &AppState, id: &str) -> Vec<Value> {
    let Ok(c) = state.store.read() else { return vec![] };
    let Ok(mut st) = c.prepare(
        "SELECT id, author_key, author_name, body, ts FROM conversation_posts \
         WHERE conv=?1 AND deleted_at IS NULL ORDER BY id ASC LIMIT 400",
    ) else {
        return vec![];
    };
    st.query_map([format!("o:{id}")], |r| {
        Ok(json!({"id": r.get::<_, i64>(0)?, "author_key": r.get::<_, String>(1)?,
            "author_name": r.get::<_, String>(2)?, "body": r.get::<_, String>(3)?, "ts": r.get::<_, i64>(4)?}))
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// What the owner sees about an outcome: working (the request or a follow-up
/// is the newest post), ready (the helper answered last), or answered (amux
/// answered it directly). Computed from the thread, so it cannot disagree with it.
pub(crate) fn status_of(posts: &[Value]) -> &'static str {
    match posts.last().and_then(|p| p["author_key"].as_str()) {
        None => "working",
        Some(a) if a.starts_with("worker:") => "ready",
        Some("amux:intent") => "ready",
        Some(_) => "working",
    }
}

fn summary(o: &OutcomeRow, posts: &[Value]) -> Value {
    let latest_reply = posts.iter().rev().find(|p| !p["author_key"].as_str().unwrap_or("").starts_with("person:"));
    json!({
        "id": o.id,
        "conv": format!("o:{}", o.id),
        "title": o.title,
        "worker": o.worker,
        "status": status_of(posts),
        "posts": posts.len(),
        "latest_reply": latest_reply.map(|p| p["body"].as_str().unwrap_or("").chars().take(400).collect::<String>()),
        "action": o.decision["action"],
        "why": o.decision["why"],
        "item_id": o.item_id,
        "overridden": o.overrides.as_array().is_some_and(|a| !a.is_empty()),
        "created_ms": o.created_ms,
        "updated_ms": o.updated_ms,
    })
}

// ---- creating a worker for a `create` decision -------------------------------

/// Create the worker a `create` decision describes: not isolated (decided
/// 2026-11-12), started, with no first prompt (the request arrives through the
/// outcome thread so the answer comes back there). Returns its actual name.
pub(crate) async fn create_worker(state: &AppState, spec: &Value) -> Result<String, String> {
    let mut body = json!({
        "name": spec["name"], "desc": spec["desc"], "creator": "amux",
        "worker_type": spec["worker_type"].as_str().filter(|t| !t.is_empty()).unwrap_or("chat"),
        "start": true,
    });
    if let Some(d) = spec["dir"].as_str().filter(|d| !d.is_empty()) {
        body["dir"] = json!(d);
    }
    if let Some(m) = spec["model"].as_str().filter(|m| !m.is_empty()) {
        body["model"] = json!(m);
    }
    if let Some(e) = spec["effort"].as_str().filter(|e| super::intent::CREATE_EFFORTS.contains(e)) {
        body["flags"] = json!(format!("--effort {e}"));
    }
    if let Some(g) = spec["groups"].as_array().filter(|g| !g.is_empty()) {
        body["tags"] = json!(g);
    }
    let resp = super::sessions_legacy::create_session_legacy(State(state.clone()), HeaderMap::new(), Some(Json(body))).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    if status.is_success() {
        Ok(v["name"].as_str().map(str::to_string).unwrap_or_else(|| spec["name"].as_str().unwrap_or("").to_string()))
    } else {
        Err(v["error"].as_str().unwrap_or("the new helper could not be created").to_string())
    }
}

// ---- handlers ----------------------------------------------------------------

fn bad(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

/// Start an outcome, or follow up in one. A follow-up is posted into the
/// thread and reaches the same worker; a new request is planned, the worker is
/// created when the plan says so, and the request is posted into a new thread.
pub(crate) async fn start_outcome(state: &AppState, text: &str, outcome: Option<&str>, item_id: Option<String>,
    title_hint: Option<String>, extra_sources: Value) -> Result<Value, (StatusCode, String)> {
    let text = text.trim();
    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "say what you want first".into()));
    }
    if let Some(oid) = outcome.filter(|o| !o.is_empty()) {
        let Some(mut o) = get_outcome(state, oid) else {
            return Err((StatusCode::NOT_FOUND, format!("no outcome {oid}")));
        };
        let plan = super::intent::plan_request(text, Some((&o.id, &o.worker)), "").await;
        if plan["action"] == "continue" {
            let post = super::conversations::owner_post_to_outcome(state, &o.id, text).await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            push_source(&mut o.sources, json!({"kind": "post", "id": post["id"], "text": text}));
            update_outcome(state, o.clone()).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
            return Ok(json!({"ok": true, "outcome": summary(&o, &posts_of(state, &o.id)), "decision": plan,
                "receipt": receipt(&plan, &o.worker), "measured": true, "n_considered": 1}));
        }
        // The outcome's worker is gone: re-route this follow-up and move the
        // outcome to wherever it goes, keeping one thread.
        let (worker, plan) = resolve_target(state, text, plan).await?;
        o.worker = worker.clone();
        o.decision = plan.clone();
        update_outcome(state, o.clone()).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let post = super::conversations::owner_post_to_outcome(state, &o.id, text).await
            .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        push_source(&mut o.sources, json!({"kind": "post", "id": post["id"], "text": text}));
        update_outcome(state, o.clone()).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        return Ok(json!({"ok": true, "outcome": summary(&o, &posts_of(state, &o.id)), "decision": plan,
            "receipt": receipt(&plan, &worker), "measured": true, "n_considered": 1}));
    }
    let plan = super::intent::plan_request(text, None, &override_hints(state)).await;
    let id = new_id();
    let title = title_hint.filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| plan["title"].as_str().unwrap_or("").to_string());
    let title = if title.trim().is_empty() { super::intent::fallback_title(text) } else { title };
    let mut sources = json!([{"kind": "request", "text": text}]);
    for s in extra_sources.as_array().into_iter().flatten() {
        push_source(&mut sources, s.clone());
    }
    let (worker, plan) = if plan["action"] == "answer" {
        (String::new(), plan)
    } else {
        resolve_target(state, text, plan).await?
    };
    let mut o = OutcomeRow {
        id: id.clone(), title, worker: worker.clone(), decision: plan.clone(), sources,
        overrides: json!([]), item_id, created_ms: now_ms(), updated_ms: now_ms(),
    };
    insert_outcome(state, o.clone()).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let post = super::conversations::owner_post_to_outcome(state, &id, text).await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
    push_source(&mut o.sources, json!({"kind": "post", "id": post["id"], "text": text}));
    if plan["action"] == "answer" {
        if let Err(e) = super::conversations::amux_post_to_outcome(state, &id, plan["answer"].as_str().unwrap_or("")).await {
            tracing::warn!(outcome = %id, error = %e, measured = true, n_considered = 1, verdict = "intent_answer_not_posted",
                "intent: the router answered but the answer could not be posted to the outcome");
        }
    }
    update_outcome(state, o.clone()).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    tracing::info!(outcome = %id, action = %plan["action"], worker = %worker, measured = true, n_considered = 1,
        verdict = "intent_outcome_started", "intent: an outcome was started and its request delivered");
    Ok(json!({"ok": true, "outcome": summary(&o, &posts_of(state, &id)), "decision": plan,
        "receipt": receipt(&plan, &worker), "measured": true, "n_considered": 1}))
}

fn push_source(sources: &mut Value, s: Value) {
    if let Some(a) = sources.as_array_mut() {
        a.push(s);
    } else {
        *sources = json!([s]);
    }
}

/// The worker a `send`/`create` plan lands on, creating it when needed. A
/// create that fails falls back to a send to the plan's first candidate, if
/// any, and otherwise is an error the caller shows.
async fn resolve_target(state: &AppState, _text: &str, mut plan: Value) -> Result<(String, Value), (StatusCode, String)> {
    match plan["action"].as_str() {
        Some("send") | Some("continue") => Ok((plan["worker"].as_str().unwrap_or("").to_string(), plan)),
        _ => {
            let spec = plan["create"].clone();
            match create_worker(state, &spec).await {
                Ok(name) => {
                    // A request posted while the helper is still starting used
                    // to be lost; wait (bounded) for it to run. Delivery retries
                    // a failure too (conversations::delivery_retryable).
                    let mut waited_ms = 0u64;
                    while !super::session_verbs::is_running(&name).await && waited_ms < 10_000 {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        waited_ms += 250;
                    }
                    tracing::info!(worker = %name, waited_ms, measured = true, n_considered = 1,
                        verdict = "intent_helper_ready_wait", "intent: waited for a new helper before its first request");
                    plan["worker"] = json!(name);
                    Ok((name, plan))
                }
                Err(e) => {
                    tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "intent_create_failed",
                        "intent: the router asked for a new worker and it could not be created");
                    Err((StatusCode::BAD_GATEWAY, format!("could not start a helper for this: {e}")))
                }
            }
        }
    }
}

/// One plain line about where a request went, e.g. "Sent to family: it
/// handles school and camp errands". The dashboard adds "Change".
/// A helper's name as words: `family-admin` reads "Family admin".
pub(crate) fn plain_name(worker: &str) -> String {
    let w = worker.replace(['-', '_'], " ");
    let mut c = w.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

pub(crate) fn receipt(plan: &Value, worker: &str) -> String {
    let worker = &plain_name(worker);
    match plan["action"].as_str().unwrap_or("") {
        "answer" => "Answered right away".to_string(),
        "continue" => format!("{worker} is already on this"),
        // The router's reason stays in the decision (and the Change sheet): read
        // aloud to the person it explained the plumbing ("no existing helper
        // handles family admin yet"), which the fourth Dana run tripped on.
        _ => format!("{worker} is on it"),
    }
}

async fn start(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let text = body["text"].as_str().unwrap_or("");
    let outcome = body["outcome"].as_str();
    match start_outcome(&state, text, outcome, None, None, json!([])).await {
        Ok(v) => {
            // A new request is what Ideas and Goals read: look now, in the
            // background, so they appear while the person is still here.
            if outcome.is_none_or(str::is_empty) {
                super::intent_items::pass_after_outcome(&state);
            }
            Json(v).into_response()
        }
        Err((code, msg)) => bad(code, msg),
    }
}

async fn list(State(state): State<AppState>) -> Response {
    match all_outcomes(&state) {
        Ok(rows) => {
            let items: Vec<Value> = rows.iter().map(|o| summary(o, &posts_of(&state, &o.id))).collect();
            let n = items.len();
            Json(json!({"outcomes": items, "measured": true, "n_considered": n})).into_response()
        }
        Err(e) => Json(json!({"outcomes": [], "measured": false, "n_considered": 0, "why_unmeasured": e})).into_response(),
    }
}

async fn one(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(o) = get_outcome(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no outcome {id}")) };
    let posts = posts_of(&state, &id);
    let mut v = summary(&o, &posts);
    v["thread"] = json!(posts);
    v["decision"] = o.decision.clone();
    v["sources"] = o.sources.clone();
    v["overrides"] = o.overrides.clone();
    v["receipt"] = json!(receipt(&o.decision, &o.worker));
    Json(v).into_response()
}

/// "Change": move an outcome to another worker (or to a new one), re-deliver
/// its request there, and log the override.
async fn change(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(mut o) = get_outcome(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no outcome {id}")) };
    let from = o.worker.clone();
    let request = o.sources.as_array().and_then(|a| a.iter().find(|s| s["kind"] == "request"))
        .and_then(|s| s["text"].as_str()).unwrap_or("").to_string();
    let to = if body["new"].as_bool().unwrap_or(false) {
        let taken = super::outcome_plan::taken_names(&crate::config::amux_home());
        let spec = super::intent::fallback_decision(&request, &taken).create.unwrap_or(json!({}));
        let mut spec = spec;
        if let Some(n) = body["name"].as_str().and_then(super::outcome_plan::clean_name) {
            spec["name"] = json!(super::outcome_plan::unique(&n, &taken));
        }
        match create_worker(&state, &spec).await {
            Ok(n) => n,
            Err(e) => return bad(StatusCode::BAD_GATEWAY, e),
        }
    } else {
        let w = body["worker"].as_str().unwrap_or("").trim().to_string();
        let roster = super::orchestrate::roster_entries(&crate::config::amux_home());
        match roster.iter().find(|r| r.name == w) {
            Some(r) if r.isolated => return bad(StatusCode::BAD_REQUEST, format!("{w} is isolated; amux does not route to it")),
            Some(_) => w,
            None => return bad(StatusCode::BAD_REQUEST, format!("no worker {w}")),
        }
    };
    if to == from {
        return bad(StatusCode::BAD_REQUEST, "it is already there");
    }
    if let Some(a) = o.overrides.as_array_mut() {
        a.push(json!({"from": from, "to": to, "ts": now_ms(), "request": request}));
    }
    o.worker = to.clone();
    if let Err(e) = update_outcome(&state, o.clone()).await {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    let note = format!("{request}\n\n(This was first sent to {from}; the owner moved it to you.)");
    let delivered = super::conversations::owner_post_to_outcome(&state, &id, &note).await;
    tracing::info!(outcome = %id, from = %from, to = %to, measured = true, n_considered = 1,
        verdict = "intent_override", "intent: the owner moved an outcome to another worker (router override)");
    Json(json!({"ok": delivered.is_ok(), "outcome": summary(&o, &posts_of(&state, &id)),
        "receipt": format!("Moved to {}", plain_name(&to)), "error": delivered.err()})).into_response()
}

/// Routed decisions and the owner's overrides of them. `rate` is null when
/// nothing was routed, never 0 (A5).
pub(crate) fn override_stats(rows: &[OutcomeRow]) -> Value {
    let routed: Vec<&OutcomeRow> = rows.iter().filter(|o| matches!(o.decision["action"].as_str(), Some("send" | "create"))).collect();
    let overridden = routed.iter().filter(|o| o.overrides.as_array().is_some_and(|a| !a.is_empty())).count();
    let n = routed.len();
    json!({
        "routed": n,
        "overridden": overridden,
        "rate": if n == 0 { Value::Null } else { json!(overridden as f64 / n as f64) },
        "measured": true,
        "n_considered": rows.len(),
    })
}

async fn stats(State(state): State<AppState>) -> Response {
    match all_outcomes(&state) {
        Ok(rows) => Json(override_stats(&rows)).into_response(),
        Err(e) => Json(json!({"routed": 0, "overridden": 0, "rate": null, "measured": false, "n_considered": 0,
            "why_unmeasured": e})).into_response(),
    }
}

/// The owner's past corrections, as lines for the router prompt (phase 5:
/// overrides feed back into routing). The newest 12.
pub(crate) fn override_hints(state: &AppState) -> String {
    let Ok(rows) = all_outcomes(state) else { return String::new() };
    let mut lines = vec![];
    for o in rows {
        for ov in o.overrides.as_array().into_iter().flatten() {
            let req: String = ov["request"].as_str().unwrap_or("").chars().take(140).collect();
            lines.push(format!("- \"{req}\" belongs with {} (not {})", ov["to"].as_str().unwrap_or(""), ov["from"].as_str().unwrap_or("")));
        }
    }
    lines.truncate(12);
    lines.join("\n")
}

/// Which first screen: the `home` pref when set (`intent` or `fleet`), then
/// the `AMUX_HOME_DEFAULT` environment default (a test harness or a hosted
/// install sets it), otherwise intent for a new person (fewer than five
/// workers) and the fleet for someone already running one.
pub(crate) fn home_choice(pref: Option<&str>, env_default: Option<&str>, n_workers: usize) -> (&'static str, &'static str) {
    match (pref.map(str::trim), env_default.map(str::trim)) {
        (Some("intent"), _) => ("intent", "pref"),
        (Some("fleet"), _) => ("fleet", "pref"),
        (_, Some("intent")) => ("intent", "env"),
        (_, Some("fleet")) => ("fleet", "env"),
        _ if n_workers < 5 => ("intent", "default_new"),
        _ => ("fleet", "default_existing"),
    }
}

async fn home(State(state): State<AppState>) -> Response {
    let pref: Option<String> = state.store.read().ok().and_then(|c| {
        c.query_row("SELECT value FROM prefs WHERE key='home'", [], |r| r.get::<_, String>(0)).ok()
    });
    let n = super::orchestrate::roster_entries(&crate::config::amux_home()).len();
    let env_default = std::env::var("AMUX_HOME_DEFAULT").ok();
    let (home, source) = home_choice(pref.as_deref(), env_default.as_deref(), n);
    Json(json!({"home": home, "source": source, "measured": true, "n_considered": n})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(author: &str) -> Value {
        json!({"author_key": author, "body": "x"})
    }

    #[test]
    fn status_follows_the_newest_post() {
        assert_eq!(status_of(&[]), "working");
        assert_eq!(status_of(&[post("person:owner")]), "working");
        assert_eq!(status_of(&[post("person:owner"), post("worker:family")]), "ready");
        assert_eq!(status_of(&[post("person:owner"), post("amux:intent")]), "ready");
        assert_eq!(status_of(&[post("person:owner"), post("worker:family"), post("person:owner")]), "working");
    }

    fn row(action: &str, overridden: bool) -> OutcomeRow {
        OutcomeRow {
            decision: json!({"action": action}),
            overrides: if overridden { json!([{"from": "a", "to": "b"}]) } else { json!([]) },
            ..Default::default()
        }
    }

    #[test]
    fn the_override_rate_counts_only_routed_decisions() {
        let rows = vec![row("send", true), row("create", false), row("answer", false), row("send", false)];
        let s = override_stats(&rows);
        assert_eq!(s["routed"], 3);
        assert_eq!(s["overridden"], 1);
        assert!((s["rate"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(override_stats(&[row("answer", false)])["rate"], Value::Null, "nothing routed is not a zero rate");
    }

    #[test]
    fn the_home_pref_wins_and_new_people_get_intent() {
        assert_eq!(home_choice(Some("fleet"), None, 0), ("fleet", "pref"));
        assert_eq!(home_choice(Some("intent"), Some("fleet"), 90), ("intent", "pref"), "the pref beats the environment");
        assert_eq!(home_choice(None, Some("fleet"), 0), ("fleet", "env"));
        assert_eq!(home_choice(None, Some("intent"), 90), ("intent", "env"));
        assert_eq!(home_choice(None, None, 2), ("intent", "default_new"));
        assert_eq!(home_choice(None, None, 40), ("fleet", "default_existing"));
        assert_eq!(home_choice(Some("garbage"), Some(""), 40), ("fleet", "default_existing"));
    }

    #[test]
    fn receipts_say_where_it_went_in_plain_words() {
        assert_eq!(receipt(&json!({"action": "send", "why": "It handles family errands."}), "family"),
            "Family is on it", "the reason stays out of the receipt");
        assert_eq!(receipt(&json!({"action": "create", "why": ""}), "meal-planner"), "Meal planner is on it");
        assert_eq!(receipt(&json!({"action": "answer"}), ""), "Answered right away");
    }

    #[test]
    fn only_idle_amux_helpers_without_a_goal_are_tidied() {
        let day = 86_400_000;
        let now = 100 * day;
        let o = |w: &str, at: i64| OutcomeRow { worker: w.into(), updated_ms: at, ..Default::default() };
        let rows = vec![o("meals", now - 10 * day), o("family", now - day), o("school", now - 30 * day)];
        let made: Vec<String> = ["meals", "family", "school", "never-used"].iter().map(|s| s.to_string()).collect();
        let idle = idle_helpers(&made, &rows, &["school".to_string()], now, 7);
        assert_eq!(idle, vec!["meals".to_string(), "never-used".to_string()],
            "recent work keeps family, an active goal keeps school");
    }

    #[test]
    fn the_registry_names_an_outcomes_worker() {
        remember("o-test-1", "family");
        assert_eq!(outcome_worker("o-test-1").as_deref(), Some("family"));
        remember("o-test-1", "");
        assert_eq!(outcome_worker("o-test-1"), None, "no worker is None, never an empty name");
    }
}
