-- Phase: Secret Metadata Store
-- Purpose: Track purpose, ownership, and rotation schedule for encrypted secrets
-- Enables "what is this secret for?" queries and rotation tracking

CREATE TABLE IF NOT EXISTS secret_metadata (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  secret_path TEXT NOT NULL UNIQUE,
  service_name TEXT NOT NULL,
  purpose TEXT NOT NULL,
  used_by TEXT,                -- JSON array of service names: ["dashboard", "api-gateway"]
  owner TEXT,                   -- Team or person responsible
  rotation_days INTEGER,        -- Rotation schedule in days
  last_rotated DATETIME,        -- When it was last rotated
  created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
  updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- Create indexes for common queries
CREATE INDEX IF NOT EXISTS idx_secret_metadata_service ON secret_metadata(service_name);
CREATE INDEX IF NOT EXISTS idx_secret_metadata_owner ON secret_metadata(owner);

-- Pre-populate with Google OAuth metadata
INSERT OR IGNORE INTO secret_metadata
  (secret_path, service_name, purpose, used_by, owner, rotation_days, last_rotated)
VALUES
  ('oauth.google.main',
   'google-oauth-connector',
   'Google OAuth 2.0 client credentials for user sign-in',
   '["dashboard", "api-gateway"]',
   'platform-team',
   90,
   CURRENT_TIMESTAMP);
