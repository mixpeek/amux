//! Google Calendar API endpoints.
//!
//! Deliberately narrow: read + create, no local mirror. Reuses the existing
//! per-account Gmail OAuth grants in ~/.amux/connectors/google/<email>.json
//! (same as gmail_auth.rs) rather than minting a second credential flow.
//!
//! Scoped down from an earlier, wider draft that pulled+STORED every event
//! locally (a second `calendar_events` table) on a periodic sync job, and
//! published a second `/api/gcal/unified.ics` feed alongside amux's existing
//! one-way `/api/calendar.ics` (source of truth: amux; Google/Apple
//! subscribe to it). That shape meant Google became a read/write peer that
//! could delete events amux itself created — a real ownership question, not
//! a review nitpick. This version sidesteps it entirely: `list_events` is a
//! LIVE READ-THROUGH (calls Google's API on every request, stores nothing),
//! and `create_event` is a thin, stateless write-through (calls Google's API
//! directly). Neither ever writes an event into amux's own DB, so there is
//! nothing to reconcile, nothing for a sync job to prune, and no second
//! calendar competing with the existing one.
//!
//! Cost of that: no local cache means every `GET /events` is a live round
//! trip to Google (typically 1-3 calendars, well under the account's own
//! rate limit for a dashboard-refresh cadence) rather than a DB read. Worth
//! it for the ownership property; revisit only if latency becomes a real
//! complaint, not preemptively.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use crate::db::calendar;
use super::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/accounts", get(list_accounts))
        .route("/events", get(list_events))
        .route("/events", post(create_event))
}

#[derive(Serialize)]
pub struct AccountResponse {
    accounts: Vec<calendar::CalendarAccount>,
}

#[derive(Serialize)]
pub struct EventsResponse {
    events: Vec<calendar::CalendarEvent>,
    total: usize,
}

#[derive(Deserialize)]
pub struct EventsQuery {
    /// Filter to one connected account; omit for every connected account.
    account_id: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateEventRequest {
    account_id: String,
    calendar_id: Option<String>,
    title: String,
    description: Option<String>,
    start_time: String,
    end_time: String,
    attendees: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct CreateEventResponse {
    event_id: String,
    message: String,
}

/// GET /api/gcal/accounts
/// List connected Google Calendar accounts (which OAuth grants exist).
pub async fn list_accounts(
    State(state): State<AppState>,
) -> Result<Json<AccountResponse>, (StatusCode, String)> {
    let conn = state
        .store
        .read()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let accounts = calendar::get_accounts(&conn)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(AccountResponse { accounts }))
}

/// GET /api/gcal/events
/// Live read-through: fetches events directly from Google's Calendar API
/// for the given account (or every connected account if none is given).
/// Nothing here is stored in amux's own DB -- see this file's own header.
pub async fn list_events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, (StatusCode, String)> {
    let accounts = {
        let conn = state
            .store
            .read()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        match &q.account_id {
            Some(id) => vec![calendar::get_account(&conn, id)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .ok_or_else(|| (StatusCode::NOT_FOUND, "Account not found".to_string()))?],
            None => calendar::get_accounts(&conn)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        }
    };

    let mut events = Vec::new();
    for account in &accounts {
        let oauth_token =
            match crate::integrations::gcal_sync::load_refresh_token(&account.id, &account.email) {
                Ok(t) => t,
                // One account's missing/expired grant shouldn't 401 the
                // whole multi-account list -- skip it, the caller still
                // gets every account that DOES have a usable token.
                Err(e) => {
                    tracing::warn!(account = %account.id, error = %e, "gcal list_events: skipping account, no usable OAuth token");
                    continue;
                }
            };

        match crate::integrations::gcal_sync::fetch_calendar_events(&oauth_token, &account.id).await {
            Ok(mut acc_events) => events.append(&mut acc_events),
            Err(e) => {
                tracing::warn!(account = %account.id, error = %e, "gcal list_events: fetch failed for account");
            }
        }
    }

    events.sort_by(|a, b| a.start_time.cmp(&b.start_time));
    let total = events.len();
    Ok(Json(EventsResponse { events, total }))
}

/// POST /api/gcal/events
/// Create a new Google Calendar event with optional attendees/invites.
/// Writes straight through to Google's API; amux stores nothing about it.
pub async fn create_event(
    State(state): State<AppState>,
    Json(req): Json<CreateEventRequest>,
) -> Result<Json<CreateEventResponse>, (StatusCode, String)> {
    let conn = state
        .store
        .read()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let account = calendar::get_account(&conn, &req.account_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Account not found".to_string()))?;

    // Load OAuth token for this account
    let oauth_token = crate::integrations::gcal_sync::load_refresh_token(&account.id, &account.email)
        .map_err(|e| (StatusCode::UNAUTHORIZED, format!("Failed to load OAuth token: {}", e)))?;

    let calendar_id = req.calendar_id.unwrap_or_else(|| "primary".to_string());
    let attendee_refs: Option<Vec<&str>> = req.attendees.as_ref().map(|a| a.iter().map(|s| s.as_str()).collect());

    let event_id = crate::integrations::gcal_sync::create_calendar_event(
        &oauth_token,
        &calendar_id,
        &req.title,
        req.description.as_deref(),
        &req.start_time,
        &req.end_time,
        attendee_refs,
    )
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    Ok(Json(CreateEventResponse {
        event_id: event_id.clone(),
        message: format!("Event created successfully. ID: {}", event_id),
    }))
}
