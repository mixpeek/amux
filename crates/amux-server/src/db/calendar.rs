//! Google Calendar account storage.
//!
//! Just the connected-account list (which Google accounts have a usable
//! OAuth grant). No local event MIRROR, no sync metadata, no subscription
//! feed — see api/gcal.rs's own header for why: amux only ever pushes NEW
//! events to Google, it never mirrors or deletes anything on Google's side,
//! so there's nothing to sync or reconcile persistently.

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarAccount {
    pub id: String,
    pub email: String,
    pub service_name: String,
    pub display_name: Option<String>,
    pub is_primary: bool,
    pub sync_status: String, // pending, syncing, ok, error
    pub event_count: i32,
    pub synced_at: Option<String>,
    pub created_at: String,
}

/// A Google Calendar event, shaped for a LIVE read-through response —
/// NOT a database row. `crate::integrations::gcal_sync::fetch_calendar_events`
/// builds these fresh from Google's API on every `GET /api/gcal/events`
/// call and nothing here is ever persisted, which is deliberate: storing a
/// local copy is exactly the "amux mirrors Google, then has to reconcile
/// deletes" shape that made an earlier version of this feature a second,
/// competing calendar system (see api/gcal.rs's own header).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub id: String,
    pub event_id: String,
    pub account_id: String,
    pub calendar_id: String,
    pub title: String,
    pub description: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub all_day: bool,
    pub location: Option<String>,
    pub status: String, // confirmed, tentative, cancelled
    pub synced_at: String,
    pub attendees: Option<String>,  // JSON array: [{email,display_name,response_status,organizer,is_self}]
    pub organizer: Option<String>,  // organizer's email
    pub html_link: Option<String>,  // "open in Google Calendar" URL
    pub meeting_url: Option<String>, // Meet/conference join URL, if any
}

/// Get all calendar accounts
pub fn get_accounts(conn: &Connection) -> anyhow::Result<Vec<CalendarAccount>> {
    let mut stmt = conn.prepare(
        "SELECT id, email, service_name, display_name, is_primary, sync_status,
                event_count, synced_at, created_at
         FROM calendar_accounts
         ORDER BY is_primary DESC, email ASC"
    )?;

    let accounts = stmt.query_map([], |row| {
        Ok(CalendarAccount {
            id: row.get(0)?,
            email: row.get(1)?,
            service_name: row.get(2)?,
            display_name: row.get(3)?,
            is_primary: row.get::<_, i32>(4)? != 0,
            sync_status: row.get(5)?,
            event_count: row.get(6)?,
            synced_at: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    let mut result = Vec::new();
    for account in accounts {
        result.push(account?);
    }
    Ok(result)
}

/// Get account by ID
pub fn get_account(conn: &Connection, id: &str) -> anyhow::Result<Option<CalendarAccount>> {
    let mut stmt = conn.prepare(
        "SELECT id, email, service_name, display_name, is_primary, sync_status,
                event_count, synced_at, created_at
         FROM calendar_accounts
         WHERE id = ?1"
    )?;

    stmt.query_row([id], |row| {
        Ok(CalendarAccount {
            id: row.get(0)?,
            email: row.get(1)?,
            service_name: row.get(2)?,
            display_name: row.get(3)?,
            is_primary: row.get::<_, i32>(4)? != 0,
            sync_status: row.get(5)?,
            event_count: row.get(6)?,
            synced_at: row.get(7)?,
            created_at: row.get(8)?,
        })
    })
    .optional()
    .map_err(|e| e.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calendar_schema_creation() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut conn).unwrap();

        // Verify the account table exists -- the only one this module owns.
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'calendar_%'"
        ).unwrap();

        let tables: Vec<String> = stmt.query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"calendar_accounts".to_string()));
    }
}
