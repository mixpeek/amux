//! Google Calendar API client.
//!
//! Every call takes a ready access token. Tokens come from the connectors
//! user-grant mint (`api::connectors::google_calendar_token`), which owns
//! refresh and reconnect for the whole Google family, so this module never
//! reads a grant file or a client secret itself.
//! Handles timezone parsing correctly (RFC 3339 with offset preservation).

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// A Google Calendar event shaped for a live read-through response. Built
/// fresh from Google's API on every `GET /api/gcal/events`; never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub event_id: String,
    /// The Google account (email) the event was read from.
    pub account_id: String,
    pub calendar_id: String,
    /// Always "google": tells these apart from amux's own /api/cal-events.
    pub source: String,
    pub title: String,
    pub description: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day: bool,
    pub location: Option<String>,
    pub status: String, // confirmed, tentative, cancelled
    pub synced_at: String,
    pub attendees: Option<String>,  // JSON array of Google attendee objects
    pub organizer: Option<String>,  // organizer's email
    pub html_link: Option<String>,  // "open in Google Calendar" URL
    pub meeting_url: Option<String>, // Meet/conference join URL, if any
}

/// Build a `.../calendars/{calendar_id}[/events[/{event_id}]]` URL with each
/// ID properly percent-encoded as a path segment. Every call site here used
/// to build this with `format!(".../calendars/{}/...", calendar_id)` — plain
/// string interpolation, no encoding — which breaks for any calendar_id
/// containing a character with meaning to a URL. The built-in Google
/// "Holidays in <country>" calendar is exactly this case:
/// `en.ch#holiday@group.v.calendar.google.com` — the unencoded `#` is read
/// as a URL FRAGMENT delimiter, so everything after it (`holiday@group...`)
/// never reached the server at all; the request silently hit a
/// nonexistent `.../calendars/en.ch` resource instead.
fn calendar_events_url(calendar_id: &str, event_id: Option<&str>) -> anyhow::Result<reqwest::Url> {
    // No trailing slash on the base: url's path_segments_mut() treats a
    // trailing "/" as an empty final segment and APPENDS after it rather
    // than replacing it, so calendars/ + .push(id) produced
    // ".../calendars//michael%40floads.io/events" (a doubled slash) —
    // which Google could not parse, and reqwest could not decode the
    // response body for. Every calendar failed identically, not just the
    // ones with special characters, which is what actually gave this away.
    let mut url = reqwest::Url::parse("https://www.googleapis.com/calendar/v3/calendars")?;
    {
        let mut segs = url.path_segments_mut().map_err(|_| anyhow::anyhow!("cannot-be-a-base URL"))?;
        segs.push(calendar_id).push("events");
        if let Some(eid) = event_id {
            segs.push(eid);
        }
    }
    Ok(url)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleCalendarEvent {
    pub id: String,
    pub summary: String,
    pub description: Option<String>,
    pub start: GoogleDateTime,
    pub end: GoogleDateTime,
    pub location: Option<String>,
    pub status: String, // "confirmed", "tentative", "cancelled"
    // Google's events.list already returns these (no `fields=` restriction on
    // the request) — they were simply never deserialized, so serde silently
    // dropped them and no meeting link/attendees/organizer ever reached the
    // DB or the dashboard.
    #[serde(rename = "htmlLink")]
    pub html_link: Option<String>,
    #[serde(rename = "hangoutLink")]
    pub hangout_link: Option<String>,
    #[serde(rename = "conferenceData")]
    pub conference_data: Option<GoogleConferenceData>,
    pub attendees: Option<Vec<GoogleAttendee>>,
    pub organizer: Option<GoogleOrganizer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleDateTime {
    #[serde(rename = "dateTime")]
    pub date_time: Option<String>, // RFC 3339 with timezone
    pub date: Option<String>, // YYYY-MM-DD for all-day events
    #[serde(rename = "timeZone")]
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleConferenceData {
    #[serde(rename = "entryPoints")]
    pub entry_points: Option<Vec<GoogleEntryPoint>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleEntryPoint {
    #[serde(rename = "entryPointType")]
    pub entry_point_type: String, // "video" | "phone" | "sip" | "more"
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleAttendee {
    pub email: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "responseStatus")]
    pub response_status: Option<String>, // "needsAction" | "declined" | "tentative" | "accepted"
    #[serde(default)]
    pub organizer: bool,
    #[serde(rename = "self", default)]
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleOrganizer {
    pub email: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

/// Parse Google DateTime to RFC 3339 string, preserving timezone
fn parse_google_datetime(gdt: &GoogleDateTime, all_day: bool) -> Option<String> {
    if let Some(dt_str) = &gdt.date_time {
        // Google returns RFC 3339: "2026-08-25T10:00:00+02:00"
        // Keep it as-is (preserves timezone offset)
        Some(dt_str.clone())
    } else if let Some(date_str) = &gdt.date {
        // All-day events: "2026-08-25", kept ISO so the dashboard calendar
        // parses it (the compact iCal form belonged to a feed that is gone).
        if all_day {
            Some(date_str.clone())
        } else {
            None
        }
    } else {
        None
    }
}

/// The insert URL for a new event. `sendUpdates` is always explicit: `none`
/// unless the caller has already cleared `send_invites` with the owner gate
/// in `api::gcal`, because Google's invitation is an email to the attendees,
/// who are usually outside the company.
pub(crate) fn create_event_url(calendar_id: &str, send_invites: bool) -> anyhow::Result<reqwest::Url> {
    let mut url = calendar_events_url(calendar_id, None)?;
    url.query_pairs_mut().append_pair("sendUpdates", if send_invites { "all" } else { "none" });
    Ok(url)
}

/// Create a new Google Calendar event
#[allow(clippy::too_many_arguments)]
pub async fn create_calendar_event(
    access_token: &str,
    calendar_id: &str,
    title: &str,
    description: Option<&str>,
    start_time: &str,
    end_time: &str,
    attendees: Option<Vec<&str>>,
    send_invites: bool,
) -> anyhow::Result<String> {
    let mut event = serde_json::json!({
        "summary": title,
        "start": {
            "dateTime": start_time,
        },
        "end": {
            "dateTime": end_time,
        },
    });

    if let Some(desc) = description {
        event["description"] = serde_json::json!(desc);
    }

    if let Some(attendee_list) = attendees {
        event["attendees"] = serde_json::json!(
            attendee_list.iter().map(|email| {
                serde_json::json!({
                    "email": email,
                    "responseStatus": "needsAction"
                })
            }).collect::<Vec<_>>()
        );
    }

    let client = reqwest::Client::new();
    let url = create_event_url(calendar_id, send_invites)?;

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", access_token))
        .json(&event)
        .send()
        .await?;

    if !response.status().is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Failed to create event: {}", error_text));
    }

    let data: serde_json::Value = response.json().await?;
    let event_id = data.get("id")
        .and_then(|id| id.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No event ID in response"))?;

    Ok(event_id)
}

/// Fetch events from every calendar the account can see, within
/// [`SYNC_WINDOW_DAYS`] either side of now. `account_id` is the Google
/// account (email) and is stamped on each event.
pub async fn fetch_calendar_events(access_token: &str, account_id: &str) -> anyhow::Result<Vec<CalendarEvent>> {
    let calendars = fetch_calendar_list(access_token).await?;

    let mut all_events = Vec::new();

    // Fetch events from each calendar
    for calendar_id in calendars {
        match fetch_events_for_calendar(access_token, &calendar_id).await {
            Ok(events) => {
                let event_count = events.len();
                // Convert events to our format
                for event in events {
                    all_events.push(convert_event(&event, account_id, &calendar_id));
                }
                tracing::debug!("Fetched {} events from calendar {}", event_count, calendar_id);
            }
            Err(e) => {
                tracing::warn!("Failed to fetch events from calendar {}: {}", calendar_id, e);
            }
        }
    }

    Ok(all_events)
}

/// Fetch list of calendar IDs
async fn fetch_calendar_list(access_token: &str) -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::new();
    let response = client
        .get("https://www.googleapis.com/calendar/v3/users/me/calendarList")
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await?;

    // Status was never checked here — an error response (e.g. a scope
    // problem) still has a JSON body, .get("items") on it is None, and
    // unwrap_or(&vec![]) silently produced an empty Vec with no error
    // anywhere. Surfacing status + raw item count so a future gap here
    // (fewer calendars synced than the account can actually see) is
    // diagnosable from the log instead of requiring a manual API replay.
    let status = response.status();
    let data: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(anyhow::anyhow!("calendarList request failed: {} {}", status, data));
    }

    let items = data.get("items").and_then(|items| items.as_array()).cloned().unwrap_or_default();
    let calendar_ids: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("id").and_then(|id| id.as_str()).map(|s| s.to_string()))
        .collect();

    tracing::info!(
        "calendarList: {} calendar(s) visible, {} carried an id: {:?}. nextPageToken present: {}",
        items.len(), calendar_ids.len(), calendar_ids,
        data.get("nextPageToken").is_some(),
    );

    Ok(calendar_ids)
}

/// How far back/forward `GET /api/gcal/events` looks on each live call.
pub const SYNC_WINDOW_DAYS: i64 = 90;

/// Fetch events from a specific calendar
async fn fetch_events_for_calendar(access_token: &str, calendar_id: &str) -> anyhow::Result<Vec<GoogleCalendarEvent>> {
    let client = reqwest::Client::new();

    let now = chrono::Utc::now();
    let start_time = (now - chrono::Duration::days(SYNC_WINDOW_DAYS)).to_rfc3339();
    let end_time = (now + chrono::Duration::days(SYNC_WINDOW_DAYS)).to_rfc3339();

    let url = calendar_events_url(calendar_id, None)?;

    let response = client
        .get(url)
        .header("Authorization", format!("Bearer {}", access_token))
        .query(&[("timeMin", start_time.as_str()), ("timeMax", end_time.as_str()), ("maxResults", "250")])
        .send()
        .await?;

    // Same silent-empty-on-error gap as fetch_calendar_list: an error
    // response body still parses as JSON, .get("items") on it is None, and
    // this returned Ok(vec![]) with no trace anywhere that anything had
    // gone wrong for that specific calendar.
    let status = response.status();
    let data: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(anyhow::anyhow!("events.list for calendar {} failed: {} {}", calendar_id, status, data));
    }

    let events: Vec<GoogleCalendarEvent> = data
        .get("items")
        .and_then(|items| items.as_array())
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect();

    Ok(events)
}

/// Convert Google Calendar event to our CalendarEvent format
pub fn convert_event(
    google_event: &GoogleCalendarEvent,
    account_id: &str,
    calendar_id: &str,
) -> CalendarEvent {
    let all_day = google_event.start.date.is_some();

    // Prefer the modern conferenceData video entry point; hangoutLink is the
    // older/deprecated field Google still populates for many Meet events.
    let meeting_url = google_event
        .conference_data
        .as_ref()
        .and_then(|cd| cd.entry_points.as_ref())
        .and_then(|eps| eps.iter().find(|e| e.entry_point_type == "video"))
        .and_then(|e| e.uri.clone())
        .or_else(|| google_event.hangout_link.clone());

    let attendees = google_event
        .attendees
        .as_ref()
        .and_then(|a| serde_json::to_string(a).ok());

    CalendarEvent {
        id: format!("{}:{}:{}", account_id, calendar_id, google_event.id),
        event_id: google_event.id.clone(),
        account_id: account_id.to_string(),
        calendar_id: calendar_id.to_string(),
        source: "google".to_string(),
        title: google_event.summary.clone(),
        description: google_event.description.clone(),
        start_time: parse_google_datetime(&google_event.start, all_day),
        end_time: parse_google_datetime(&google_event.end, all_day),
        all_day,
        location: google_event.location.clone(),
        status: match google_event.status.as_str() {
            "cancelled" => "cancelled",
            "tentative" => "tentative",
            _ => "confirmed",
        }
        .to_string(),
        synced_at: Utc::now().to_rfc3339(),
        html_link: google_event.html_link.clone(),
        meeting_url,
        organizer: google_event.organizer.as_ref().and_then(|o| o.email.clone()),
        attendees,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_google_datetime_with_timezone() {
        let gdt = GoogleDateTime {
            date_time: Some("2026-08-25T10:00:00+02:00".to_string()),
            date: None,
            time_zone: None,
        };

        let result = parse_google_datetime(&gdt, false);
        assert_eq!(result, Some("2026-08-25T10:00:00+02:00".to_string()));
        // Verify timezone offset is preserved (not stripped)
        assert!(result.unwrap().contains("+02:00"));
    }

    #[test]
    fn test_parse_google_datetime_allday() {
        let gdt = GoogleDateTime {
            date_time: None,
            date: Some("2026-08-25".to_string()),
            time_zone: None,
        };

        let result = parse_google_datetime(&gdt, true);
        assert_eq!(result, Some("2026-08-25".to_string()));
    }

    #[test]
    fn test_convert_event() {
        let google_event = GoogleCalendarEvent {
            id: "evt123".to_string(),
            summary: "Meeting".to_string(),
            description: Some("Team sync".to_string()),
            start: GoogleDateTime {
                date_time: Some("2026-08-25T10:00:00Z".to_string()),
                date: None,
                time_zone: None,
            },
            end: GoogleDateTime {
                date_time: Some("2026-08-25T11:00:00Z".to_string()),
                date: None,
                time_zone: None,
            },
            location: Some("Room 101".to_string()),
            status: "confirmed".to_string(),
            html_link: None,
            hangout_link: None,
            conference_data: None,
            attendees: None,
            organizer: None,
        };

        let event = convert_event(&google_event, "someone@example.com", "primary");
        assert_eq!(event.event_id, "evt123");
        assert_eq!(event.account_id, "someone@example.com");
        assert_eq!(event.source, "google");
        assert_eq!(event.title, "Meeting");
        assert_eq!(event.location, Some("Room 101".to_string()));
        assert!(!event.all_day);
        assert_eq!(event.status, "confirmed");
    }
}
