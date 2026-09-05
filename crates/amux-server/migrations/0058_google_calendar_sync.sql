-- Migration 0049: Google Calendar account storage.
--
-- Just the connected-account list (which Google accounts have a usable
-- OAuth grant, reusing the same per-account Gmail OAuth flow gmail_auth.rs
-- already has). No local event mirror, no sync metadata, no subscription
-- table for an iCal feed -- see api/gcal.rs's own header comment for why:
-- amux only ever pushes NEW events to Google (create), and reads events
-- live on request (no local storage to reconcile), so there is nothing
-- else here to sync.

CREATE TABLE IF NOT EXISTS calendar_accounts (
  id TEXT PRIMARY KEY,
  email TEXT UNIQUE NOT NULL,
  service_name TEXT NOT NULL DEFAULT 'google-calendar',
  display_name TEXT,
  is_primary BOOLEAN NOT NULL DEFAULT 0,
  oauth_refresh_token TEXT NOT NULL,
  oauth_token_expiry TEXT,
  synced_at TEXT,
  next_sync_at TEXT,
  sync_status TEXT NOT NULL DEFAULT 'pending', -- pending, ok, error
  last_error TEXT,
  event_count INTEGER DEFAULT 0,
  calendar_list_synced_at TEXT,
  sync_window_days INTEGER DEFAULT 90,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_calendar_accounts_email ON calendar_accounts(email);
CREATE INDEX IF NOT EXISTS idx_calendar_accounts_primary ON calendar_accounts(is_primary);

-- Pre-populate with connected accounts -- config-driven, see
-- crates/amux-server/src/db/calendar_init.rs
