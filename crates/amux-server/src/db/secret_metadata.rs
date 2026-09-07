//! Secret metadata store — tracks purpose, ownership, rotation schedule
//!
//! Enables querying "what is this secret for?" and "when should it be rotated?"
//! Separate from secret values to keep concerns clean.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use rusqlite::{Connection, OptionalExtension, Row};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretMetadata {
    pub id: Option<i64>,
    pub secret_path: String,
    pub service_name: String,
    pub purpose: String,
    pub used_by: Vec<String>, // Parsed from JSON
    pub owner: Option<String>,
    pub rotation_days: Option<i32>,
    pub last_rotated: Option<DateTime<Utc>>,
}

fn metadata_from_row(r: &Row<'_>) -> rusqlite::Result<SecretMetadata> {
    let id: i64 = r.get(0)?;
    let used_by_json: String = r.get(4)?;
    let last_rotated_str: Option<String> = r.get(7)?;

    Ok(SecretMetadata {
        id: Some(id),
        secret_path: r.get(1)?,
        service_name: r.get(2)?,
        purpose: r.get(3)?,
        used_by: serde_json::from_str(&used_by_json).unwrap_or_default(),
        owner: r.get(5)?,
        rotation_days: r.get(6)?,
        last_rotated: last_rotated_str.and_then(|ts| {
            DateTime::parse_from_rfc3339(&ts)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }),
    })
}

/// Get metadata for a specific secret
pub fn get_metadata(conn: &Connection, secret_path: &str) -> rusqlite::Result<Option<SecretMetadata>> {
    let mut stmt = conn.prepare(
        "SELECT id, secret_path, service_name, purpose, used_by, owner, rotation_days, last_rotated
         FROM secret_metadata
         WHERE secret_path = ?1"
    )?;

    stmt.query_row([secret_path], metadata_from_row).optional()
}

/// List all secret metadata
pub fn list_all(conn: &Connection) -> rusqlite::Result<Vec<SecretMetadata>> {
    let mut stmt = conn.prepare(
        "SELECT id, secret_path, service_name, purpose, used_by, owner, rotation_days, last_rotated
         FROM secret_metadata
         ORDER BY service_name, secret_path"
    )?;

    let rows = stmt.query_map([], metadata_from_row)?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Create or update secret metadata
pub fn set_metadata(conn: &Connection, metadata: &SecretMetadata) -> rusqlite::Result<()> {
    let used_by_json = serde_json::to_string(&metadata.used_by)
        .unwrap_or_else(|_| "[]".to_string());
    let last_rotated = metadata
        .last_rotated
        .map(|dt| dt.to_rfc3339());

    conn.execute(
        "INSERT INTO secret_metadata
          (secret_path, service_name, purpose, used_by, owner, rotation_days, last_rotated)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ON CONFLICT(secret_path) DO UPDATE SET
          service_name = excluded.service_name,
          purpose = excluded.purpose,
          used_by = excluded.used_by,
          owner = excluded.owner,
          rotation_days = excluded.rotation_days,
          last_rotated = excluded.last_rotated,
          updated_at = CURRENT_TIMESTAMP",
        rusqlite::params![
            metadata.secret_path,
            metadata.service_name,
            metadata.purpose,
            used_by_json,
            metadata.owner,
            metadata.rotation_days,
            last_rotated
        ],
    )?;

    Ok(())
}

/// Check if a secret needs rotation based on last_rotated and rotation_days
pub fn needs_rotation(metadata: &SecretMetadata) -> bool {
    match (metadata.last_rotated, metadata.rotation_days) {
        (Some(last_rotated), Some(rotation_days)) => {
            let now = Utc::now();
            let days_since = now
                .signed_duration_since(last_rotated)
                .num_days();
            days_since >= rotation_days as i64
        }
        _ => false,
    }
}

/// Calculate days until rotation is due
pub fn days_until_rotation(metadata: &SecretMetadata) -> Option<i64> {
    match (metadata.last_rotated, metadata.rotation_days) {
        (Some(last_rotated), Some(rotation_days)) => {
            let now = Utc::now();
            let days_since = now
                .signed_duration_since(last_rotated)
                .num_days();
            let days_until = rotation_days as i64 - days_since;
            Some(days_until.max(0))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_needs_rotation() {
        let metadata = SecretMetadata {
            id: Some(1),
            secret_path: "test.key".to_string(),
            service_name: "test-service".to_string(),
            purpose: "test".to_string(),
            used_by: vec![],
            owner: None,
            rotation_days: Some(90),
            last_rotated: Some(Utc::now() - chrono::Duration::days(91)),
        };

        assert!(needs_rotation(&metadata));
    }

    #[test]
    fn test_does_not_need_rotation() {
        let metadata = SecretMetadata {
            id: Some(1),
            secret_path: "test.key".to_string(),
            service_name: "test-service".to_string(),
            purpose: "test".to_string(),
            used_by: vec![],
            owner: None,
            rotation_days: Some(90),
            last_rotated: Some(Utc::now() - chrono::Duration::days(30)),
        };

        assert!(!needs_rotation(&metadata));
    }
}
