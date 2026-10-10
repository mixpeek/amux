//! Google Calendar API endpoints.
//!
//! Deliberately narrow: read + create, no local mirror. `GET /events` is a
//! live read-through (calls Google on every request, stores nothing) and
//! `POST /events` is a stateless write-through. Neither writes an event into
//! amux's own DB, so there is nothing to reconcile and no second calendar
//! competing with amux's own `/api/cal-events` and `/api/calendar.ics`.
//!
//! Accounts and tokens come from the Google family grant the Connectors page
//! manages (`connectors::google_calendar_accounts` /
//! `connectors::google_calendar_token`): one approval per account covers
//! gmail, calendar and drive, and refresh, reconnect and worker entitlement
//! are handled there once for every Google connector.
//!
//! Cost: no local cache means every `GET /events` is a live round trip to
//! Google per account. Revisit only if latency becomes a real complaint.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;
use crate::integrations::gcal_sync;

const CONNECTOR: &str = "google-calendar";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/accounts", get(list_accounts))
        .route("/events", get(list_events).post(create_event))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    /// Filter to one connected account (email); omit for every account.
    account_id: Option<String>,
    /// The visible range (RFC 3339). With both, the read covers exactly that
    /// window, every account and calendar in parallel, cached 30 s. Without
    /// them it keeps the old +/- SYNC_WINDOW_DAYS read for existing callers.
    time_min: Option<String>,
    time_max: Option<String>,
}

/// 30 s per (account, window): a calendar view polls every 60 s and on focus,
/// so this bounds Google calls without letting the view go stale.
type WindowCache = std::sync::Mutex<
    std::collections::HashMap<(String, String, String), (std::time::Instant, Vec<gcal_sync::CalendarMeta>, Vec<gcal_sync::CalendarEvent>)>,
>;
fn window_cache() -> &'static WindowCache {
    static C: std::sync::OnceLock<WindowCache> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// One account's calendars and events for the window, every calendar fetched
/// concurrently. A calendar that fails is reported, never fatal.
async fn account_window(
    token: &str,
    account: &str,
    time_min: &str,
    time_max: &str,
) -> anyhow::Result<(Vec<gcal_sync::CalendarMeta>, Vec<gcal_sync::CalendarEvent>, usize)> {
    let key = (account.to_string(), time_min.to_string(), time_max.to_string());
    if let Ok(c) = window_cache().lock() {
        if let Some((at, cals, evs)) = c.get(&key) {
            if at.elapsed() < std::time::Duration::from_secs(30) {
                return Ok((cals.clone(), evs.clone(), 0));
            }
        }
    }
    let cals = gcal_sync::fetch_calendar_meta(token, account).await?;
    let fetches = cals.iter().map(|c| gcal_sync::fetch_calendar_window(token, c, time_min, time_max));
    let results = futures::future::join_all(fetches).await;
    let mut events = Vec::new();
    let mut failed = 0usize;
    for (cal, r) in cals.iter().zip(results) {
        match r {
            Ok(mut v) => events.append(&mut v),
            Err(e) => {
                failed += 1;
                tracing::warn!(target: "amux::gcal", account = %account, calendar = %cal.id, error = %e,
                    verdict = "gcal_calendar_fetch_failed", "one calendar failed; the rest are shown");
            }
        }
    }
    if let Ok(mut c) = window_cache().lock() {
        c.retain(|_, (at, _, _)| at.elapsed() < std::time::Duration::from_secs(120));
        c.insert(key, (std::time::Instant::now(), cals.clone(), events.clone()));
    }
    Ok((cals, events, failed))
}

#[derive(Deserialize)]
pub struct CreateEventRequest {
    /// The Google account (email) to create the event in.
    account_id: String,
    calendar_id: Option<String>,
    title: String,
    description: Option<String>,
    start_time: String,
    end_time: String,
    attendees: Option<Vec<String>>,
    /// Email the attendees a Google invitation. Owner only; default false.
    #[serde(default)]
    send_invites: bool,
}

type ApiError = (StatusCode, Json<Value>);

fn err(status: StatusCode, body: Value) -> ApiError {
    (status, Json(body))
}

/// A worker (X-Amux-Session / X-Amux-Worker) reads or writes a calendar only
/// when its `connectors` scope names google-calendar (APM-2), the same check
/// the token mint applies. The dashboard sends no worker header.
fn entitled(state: &AppState, headers: &HeaderMap, account: &str) -> Result<(), Value> {
    let Some(lane) = super::email::hdr_worker(headers) else {
        return Ok(());
    };
    let conn = state.store.read().ok();
    super::connectors::connector_entitlement_check(
        conn.as_deref(),
        &crate::config::amux_home(),
        &lane,
        CONNECTOR,
        Some(account),
    )
    .map(|_| ())
}

/// Google's invitation is an email to the attendees, usually people outside
/// the company, so it follows the standing rule for outbound mail: the owner
/// sends it. Every create is written with `sendUpdates=none` unless the owner
/// asks for invites; a worker asking for them is refused before any network
/// call.
fn invite_policy(headers: &HeaderMap, send_invites: bool, attendees: usize) -> Result<bool, Value> {
    if !send_invites {
        if attendees > 0 {
            tracing::info!(target: "amux::gcal", measured = true, n_considered = attendees,
                verdict = "gcal_invites_suppressed", "event created without emailing its attendees");
        }
        return Ok(false);
    }
    if super::standing_approvals::is_owner_request(headers) {
        tracing::info!(target: "amux::gcal", measured = true, n_considered = attendees,
            verdict = "gcal_invites_sent_by_owner", "owner asked Google to email the attendees");
        return Ok(true);
    }
    let worker = super::email::hdr_worker(headers).unwrap_or_default();
    tracing::warn!(target: "amux::gcal", worker = %worker, measured = true, n_considered = attendees,
        verdict = "gcal_invites_refused_worker", "a worker asked to email calendar invitations");
    Err(json!({
        "error": "calendar invitations email the attendees, so only the owner can send them; create the event without send_invites and ask the owner to send the invites",
        "code": "gcal_invites_owner_only",
        "rule": "outbound: anything an outside person reads is drafted by a worker and sent by the owner",
    }))
}

/// GET /api/gcal/accounts — Google accounts with a stored grant, and whether
/// each grant carries the Calendar scope.
pub async fn list_accounts() -> Json<Value> {
    let accounts = super::connectors::google_calendar_accounts(&crate::config::amux_home());
    let n = accounts.len();
    Json(json!({
        "accounts": accounts.into_iter()
            .map(|(email, calendar_granted)| json!({"id": email, "email": email, "calendar_granted": calendar_granted}))
            .collect::<Vec<_>>(),
        "measured": true,
        "n_considered": n,
    }))
}

/// GET /api/gcal/events — live read-through across the connected accounts.
/// One account's failure never empties the others; each account's outcome is
/// reported beside the events so an empty list says why it is empty.
pub async fn list_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    let all = super::connectors::google_calendar_accounts(&crate::config::amux_home());
    let targets: Vec<(String, bool)> = match &q.account_id {
        Some(id) => {
            let hit: Vec<_> = all.into_iter().filter(|(a, _)| a == id).collect();
            if hit.is_empty() {
                return Err(err(StatusCode::NOT_FOUND, json!({
                    "error": format!("no Google grant stored for {id}"),
                    "connect": format!("POST /api/connectors/google/auth?account={id}"),
                })));
            }
            hit
        }
        None => all,
    };

    if let (Some(tmin), Some(tmax)) = (q.time_min.as_deref(), q.time_max.as_deref()) {
        return Ok(Json(list_window(&state, &headers, &targets, tmin, tmax).await));
    }

    let mut events = Vec::new();
    let mut outcomes = Vec::new();
    for (account, calendar_granted) in &targets {
        if !calendar_granted {
            outcomes.push(json!({"account": account, "ok": false, "status": "not_granted",
                "detail": "this account's Google grant does not include Calendar; reconnect it on the Connectors page"}));
            continue;
        }
        if let Err(denial) = entitled(&state, &headers, account) {
            outcomes.push(json!({"account": account, "ok": false, "status": "not_entitled", "detail": denial}));
            continue;
        }
        let token = match super::connectors::google_calendar_token(account).await {
            Ok(t) => t,
            Err(body) => {
                tracing::warn!(target: "amux::gcal", account = %account, verdict = "gcal_token_unavailable",
                    detail = %body, "gcal list_events: skipped account, no usable token");
                outcomes.push(json!({"account": account, "ok": false, "status": "no_token", "detail": body}));
                continue;
            }
        };
        match gcal_sync::fetch_calendar_events(&token, account).await {
            Ok(mut got) => {
                outcomes.push(json!({"account": account, "ok": true, "events": got.len()}));
                events.append(&mut got);
            }
            Err(e) => {
                tracing::warn!(target: "amux::gcal", account = %account, verdict = "gcal_fetch_failed",
                    error = %e, "gcal list_events: fetch failed for account");
                outcomes.push(json!({"account": account, "ok": false, "status": "fetch_failed", "detail": e.to_string()}));
            }
        }
    }

    events.sort_by(|a, b| a.start_time.cmp(&b.start_time));
    let total = events.len();
    let mut body = json!({
        "events": events,
        "total": total,
        "accounts": outcomes,
        "measured": true,
        "n_considered": targets.len(),
        "window_days": gcal_sync::SYNC_WINDOW_DAYS,
    });
    if targets.is_empty() {
        body["why_empty"] = json!("no Google account is connected; connect one on the Connectors page (one approval covers gmail, calendar and drive)");
    }
    Ok(Json(body))
}

/// The windowed read: every eligible account concurrently.
async fn list_window(
    state: &AppState,
    headers: &HeaderMap,
    targets: &[(String, bool)],
    tmin: &str,
    tmax: &str,
) -> Value {
    let mut outcomes = Vec::new();
    let mut ready = Vec::new();
    for (account, calendar_granted) in targets {
        if !calendar_granted {
            outcomes.push(json!({"account": account, "ok": false, "status": "not_granted",
                "detail": "this account's Google grant does not include Calendar; reconnect it on the Connectors page"}));
            continue;
        }
        if let Err(denial) = entitled(state, headers, account) {
            outcomes.push(json!({"account": account, "ok": false, "status": "not_entitled", "detail": denial}));
            continue;
        }
        match super::connectors::google_calendar_token(account).await {
            Ok(t) => ready.push((account.clone(), t)),
            Err(body) => {
                tracing::warn!(target: "amux::gcal", account = %account, verdict = "gcal_token_unavailable",
                    detail = %body, "gcal window: skipped account, no usable token");
                outcomes.push(json!({"account": account, "ok": false, "status": "no_token", "detail": body}));
            }
        }
    }
    let runs = ready.iter().map(|(a, t)| account_window(t, a, tmin, tmax));
    let results = futures::future::join_all(runs).await;
    let mut events = Vec::new();
    let mut calendars = Vec::new();
    for ((account, _), r) in ready.iter().zip(results) {
        match r {
            Ok((mut cals, mut evs, failed)) => {
                outcomes.push(json!({"account": account, "ok": true, "events": evs.len(), "calendars": cals.len(), "calendars_failed": failed}));
                calendars.append(&mut cals);
                events.append(&mut evs);
            }
            Err(e) => {
                tracing::warn!(target: "amux::gcal", account = %account, verdict = "gcal_fetch_failed",
                    error = %e, "gcal window: fetch failed for account");
                outcomes.push(json!({"account": account, "ok": false, "status": "fetch_failed", "detail": e.to_string()}));
            }
        }
    }
    events.sort_by(|a, b| a.start_time.cmp(&b.start_time));
    let total = events.len();
    let mut body = json!({
        "events": events,
        "calendars": calendars,
        "total": total,
        "accounts": outcomes,
        "time_min": tmin,
        "time_max": tmax,
        "measured": true,
        "n_considered": targets.len(),
    });
    if targets.is_empty() {
        body["why_empty"] = json!("no Google account is connected; connect one on the Connectors page (one approval covers gmail, calendar and drive)");
    }
    body
}

/// POST /api/gcal/events — create an event in a connected account's calendar,
/// with optional attendees. Google emails the attendees only when the owner
/// sets `send_invites` (see [`invite_policy`]). Writes straight through to
/// Google; amux stores nothing about it.
pub async fn create_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateEventRequest>,
) -> Result<Json<Value>, ApiError> {
    let n_attendees = req.attendees.as_ref().map_or(0, Vec::len);
    let send_invites = invite_policy(&headers, req.send_invites, n_attendees)
        .map_err(|denial| err(StatusCode::FORBIDDEN, denial))?;
    let accounts = super::connectors::google_calendar_accounts(&crate::config::amux_home());
    match accounts.iter().find(|(a, _)| *a == req.account_id) {
        None => {
            return Err(err(StatusCode::NOT_FOUND, json!({
                "error": format!("no Google grant stored for {}", req.account_id),
                "connect": format!("POST /api/connectors/google/auth?account={}", req.account_id),
            })))
        }
        Some((_, false)) => {
            return Err(err(StatusCode::CONFLICT, json!({
                "error": format!("the Google grant for {} does not include Calendar", req.account_id),
                "status": "needs_reauth",
                "reconnect": format!("POST /api/connectors/google/auth?account={}", req.account_id),
            })))
        }
        Some(_) => {}
    }
    entitled(&state, &headers, &req.account_id).map_err(|denial| err(StatusCode::FORBIDDEN, denial))?;
    let token = super::connectors::google_calendar_token(&req.account_id)
        .await
        .map_err(|body| err(StatusCode::CONFLICT, body))?;

    let calendar_id = req.calendar_id.unwrap_or_else(|| "primary".to_string());
    let attendees: Option<Vec<&str>> = req.attendees.as_ref().map(|a| a.iter().map(String::as_str).collect());
    let event_id = gcal_sync::create_calendar_event(
        &token,
        &calendar_id,
        &req.title,
        req.description.as_deref(),
        &req.start_time,
        &req.end_time,
        attendees,
        send_invites,
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, json!({"error": e.to_string()})))?;

    tracing::info!(target: "amux::gcal", account = %req.account_id, calendar = %calendar_id,
        event_id = %event_id, send_invites, verdict = "gcal_event_created", "created a Google Calendar event");
    Ok(Json(json!({
        "ok": true,
        "event_id": event_id,
        "invites_sent": send_invites,
        "account_id": req.account_id,
        "calendar_id": calendar_id,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn grant(home: &std::path::Path, account: &str, scopes: &str) {
        let dir = home.join("connectors").join("google");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{account}.json")),
            json!({"refresh_token": "fixture", "scopes": scopes}).to_string()).unwrap();
    }

    #[tokio::test]
    async fn accounts_come_from_the_google_grant_and_report_the_calendar_scope() {
        let home = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(home.path());
        grant(home.path(), "cal@example.com",
            "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/calendar");
        grant(home.path(), "mail@example.com", "https://www.googleapis.com/auth/gmail.modify");
        let Json(v) = list_accounts().await;
        assert_eq!(v["measured"], json!(true));
        assert_eq!(v["n_considered"], json!(2));
        let flag = |email: &str| v["accounts"].as_array().unwrap().iter()
            .find(|a| a["email"] == json!(email)).map(|a| a["calendar_granted"].clone());
        assert_eq!(flag("cal@example.com"), Some(json!(true)));
        assert_eq!(flag("mail@example.com"), Some(json!(false)));
    }

    #[tokio::test]
    async fn an_empty_event_list_says_whether_and_why() {
        let home = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(home.path());
        let Json(v) = list_events(State(state()), HeaderMap::new(), Query(EventsQuery { account_id: None, time_min: None, time_max: None }))
            .await.unwrap();
        assert_eq!((v["measured"].clone(), v["n_considered"].clone(), v["total"].clone()), (json!(true), json!(0), json!(0)));
        assert!(v["why_empty"].as_str().is_some_and(|s| s.contains("Connectors")), "{v}");

        // A grant without the Calendar scope is reported, never silently
        // dropped, and costs no network call.
        grant(home.path(), "mail@example.com", "https://www.googleapis.com/auth/gmail.modify");
        let Json(v) = list_events(State(state()), HeaderMap::new(), Query(EventsQuery { account_id: None, time_min: None, time_max: None }))
            .await.unwrap();
        assert_eq!(v["n_considered"], json!(1));
        assert_eq!(v["accounts"][0]["status"], json!("not_granted"), "{v}");
        assert!(v.get("why_empty").is_none());

        let missing = list_events(State(state()), HeaderMap::new(),
            Query(EventsQuery { account_id: Some("nobody@example.com".into()), time_min: None, time_max: None })).await.unwrap_err();
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
    }

    fn create_req(send_invites: bool) -> CreateEventRequest {
        CreateEventRequest {
            account_id: "cal@example.com".into(),
            calendar_id: None,
            title: "Sync".into(),
            description: None,
            start_time: "2026-10-10T10:00:00Z".into(),
            end_time: "2026-10-10T10:30:00Z".into(),
            attendees: Some(vec!["someone@outside.example".into()]),
            send_invites,
        }
    }

    #[test]
    fn every_create_sends_no_invitation_unless_cleared() {
        let quiet = gcal_sync::create_event_url("primary", false).unwrap();
        let pairs: Vec<(String, String)> = quiet.query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
        assert_eq!(pairs, vec![("sendUpdates".to_string(), "none".to_string())], "{quiet}");
        let loud = gcal_sync::create_event_url("primary", true).unwrap();
        assert!(loud.query_pairs().any(|(k, v)| k == "sendUpdates" && v == "all"), "{loud}");
        assert!(!quiet.as_str().contains("sendNotifications"), "the deprecated always-notify flag is gone");
        // No send_invites: attendees are added silently, owner or worker.
        let mut worker = HeaderMap::new();
        worker.insert("x-amux-session", "lane-a".parse().unwrap());
        assert_eq!(invite_policy(&worker, false, 1), Ok(false));
        assert_eq!(invite_policy(&HeaderMap::new(), false, 1), Ok(false));
    }

    #[test]
    fn only_the_owner_can_send_invitations() {
        assert_eq!(invite_policy(&HeaderMap::new(), true, 2), Ok(true), "owner request may send invites");
        for header in ["x-amux-session", "x-amux-worker"] {
            let mut h = HeaderMap::new();
            h.insert(header, "lane-a".parse().unwrap());
            let denial = invite_policy(&h, true, 2).unwrap_err();
            assert_eq!(denial["code"], json!("gcal_invites_owner_only"), "{header}: {denial}");
        }
    }

    #[tokio::test]
    async fn a_worker_asking_for_invites_is_refused_before_any_google_call() {
        let home = tempfile::tempdir().unwrap();
        let _g = crate::api::settings::test_env::set_home(home.path());
        // A real grant exists, so a missing refusal would proceed to the token
        // mint and Google; the 403 must come first.
        grant(home.path(), "cal@example.com",
            "https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/calendar");
        let mut h = HeaderMap::new();
        h.insert("x-amux-session", "lane-a".parse().unwrap());
        let (status, Json(body)) = create_event(State(state()), h, Json(create_req(true))).await.unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], json!("gcal_invites_owner_only"));
    }
}
