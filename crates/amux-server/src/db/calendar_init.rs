//! Calendar account initialization from configuration file
//! Loads accounts from ~/.amux/calendar-config.json at server startup

use anyhow::{anyhow, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarAccount {
    pub id: String,
    pub email: String,
    pub display_name: Option<String>,
    pub is_primary: bool,
    #[serde(default)]
    pub sync_window_days: i32,
}

#[derive(Debug, Deserialize)]
pub struct CalendarConfig {
    pub accounts: Vec<CalendarAccount>,
}

/// Load calendar configuration from JSON file
pub fn load_config(config_path: Option<&str>) -> Result<CalendarConfig> {
    let path = if let Some(p) = config_path {
        p.to_string()
    } else {
        // Default location: ~/.amux/calendar-config.json
        let home = std::env::var("HOME")?;
        format!("{}/.amux/calendar-config.json", home)
    };

    if !Path::new(&path).exists() {
        // Config file is optional - no accounts configured is valid state
        return Ok(CalendarConfig {
            accounts: vec![],
        });
    }

    let contents = fs::read_to_string(&path)
        .map_err(|e| anyhow!("Failed to read calendar config from {}: {}", path, e))?;

    let config: CalendarConfig = serde_json::from_str(&contents)
        .map_err(|e| anyhow!("Failed to parse calendar config JSON: {}", e))?;

    tracing::info!(
        "Loaded {} calendar accounts from {}",
        config.accounts.len(),
        path
    );

    Ok(config)
}

/// Load OAuth refresh token for an account
/// This used to have its own token lookup — only the legacy
/// account-id-keyed `.txt` path, never updated when
/// `gcal_sync::load_refresh_token` was fixed to prefer the email-keyed
/// Gmail-connector token. The two silently drifted: accounts that only had
/// an OAuth-Playground-era `.txt` file kept passing this check while never
/// actually working (invalid_grant at sync time), and an account with only
/// the correct email-keyed token — like a freshly-connected one — failed
/// this check and got silently skipped at startup ("Skipping account
/// account-3: OAuth token not found ... refresh_token.txt", while
/// the real token sat right there at
/// connectors/google/<email>.json). Now defers to the same lookup the sync
/// path actually uses, so "does this account have a usable token" can only
/// ever be answered one way.
pub fn load_oauth_token(account_id: &str, email: &str) -> Result<String> {
    crate::integrations::gcal_sync::load_refresh_token(account_id, email)
}

/// Initialize calendar accounts in the database
pub fn initialize_accounts(conn: &mut Connection, config: &CalendarConfig) -> Result<()> {
    if config.accounts.is_empty() {
        tracing::info!("No calendar accounts configured");
        return Ok(());
    }

    let tx = conn.transaction()?;

    for account in &config.accounts {
        // Load OAuth token
        let oauth_token = match load_oauth_token(&account.id, &account.email) {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!("Skipping account {}: {}", account.id, e);
                continue;
            }
        };

        // Insert or update account. Status goes straight to "ok" -- there is
        // no sync job to advance it out of "pending" any more (this module
        // only records that a usable OAuth token was found; see gcal.rs's
        // own header for why there's no local event mirror to sync at all).
        tx.execute(
            "INSERT OR REPLACE INTO calendar_accounts
             (id, email, display_name, is_primary, oauth_refresh_token, sync_status)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                &account.id,
                &account.email,
                account.display_name.as_deref(),
                account.is_primary as i32,
                oauth_token,
                "ok",
            ],
        )
        .map_err(|e| anyhow!("Failed to insert calendar account {}: {}", account.id, e))?;

        tracing::info!(
            "Initialized calendar account: {} ({})",
            account.email,
            account.id
        );
    }

    tx.commit()?;

    tracing::info!(
        "✅ Initialized {} calendar accounts",
        config.accounts.len()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_config_missing_file() {
        let config = load_config(Some("/nonexistent/path.json"));
        // Missing file should be treated as no accounts (not an error)
        assert!(config.is_ok());
        assert_eq!(config.unwrap().accounts.len(), 0);
    }

    #[test]
    fn test_load_oauth_token_missing() {
        let result = load_oauth_token("nonexistent-account", "nonexistent@example.com");
        assert!(result.is_err());
    }
}
