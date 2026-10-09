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

/// POST /api/gcal/events — create an event in a connected account's calendar,
/// with optional attendees (Google emails them an invitation). Writes straight
/// through to Google; amux stores nothing about it.
pub async fn create_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateEventRequest>,
) -> Result<Json<Value>, ApiError> {
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
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, json!({"error": e.to_string()})))?;

    tracing::info!(target: "amux::gcal", account = %req.account_id, calendar = %calendar_id,
        event_id = %event_id, verdict = "gcal_event_created", "created a Google Calendar event");
    Ok(Json(json!({
        "ok": true,
        "event_id": event_id,
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
        let Json(v) = list_events(State(state()), HeaderMap::new(), Query(EventsQuery { account_id: None }))
            .await.unwrap();
        assert_eq!((v["measured"].clone(), v["n_considered"].clone(), v["total"].clone()), (json!(true), json!(0), json!(0)));
        assert!(v["why_empty"].as_str().is_some_and(|s| s.contains("Connectors")), "{v}");

        // A grant without the Calendar scope is reported, never silently
        // dropped, and costs no network call.
        grant(home.path(), "mail@example.com", "https://www.googleapis.com/auth/gmail.modify");
        let Json(v) = list_events(State(state()), HeaderMap::new(), Query(EventsQuery { account_id: None }))
            .await.unwrap();
        assert_eq!(v["n_considered"], json!(1));
        assert_eq!(v["accounts"][0]["status"], json!("not_granted"), "{v}");
        assert!(v.get("why_empty").is_none());

        let missing = list_events(State(state()), HeaderMap::new(),
            Query(EventsQuery { account_id: Some("nobody@example.com".into()) })).await.unwrap_err();
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
    }
}
