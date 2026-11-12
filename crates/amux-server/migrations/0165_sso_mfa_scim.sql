-- Governance PR 20: single sign-on (OIDC), a second factor (TOTP), and
-- provisioning from an identity provider (SCIM 2.0). All three feed the one
-- identity model amux already has: a person is an `org_members` row and a
-- signed-in browser is an `org_invites` row whose token is the member cookie.
-- No second session store.

-- How a session (a used org_invites row) was made, and when its holder last
-- proved a second factor. `via` is NULL for invite links made before this,
-- which read as 'invite'. An SSO session expires at expires_at; an invite
-- session never did and still does not.
-- ADDCOL: org_invites via TEXT
-- ADDCOL: org_invites mfa_at INTEGER

-- The identity provider's own id for a provisioned person (SCIM externalId).
-- ADDCOL: org_members external_id TEXT
-- ADDCOL: org_members provisioned_by TEXT

-- One TOTP factor per subject (`owner` or `member:<email>`). The secret is
-- sealed with the at-rest key (integrations/at_rest.rs), bound to its subject,
-- so a row copied to another subject does not open. last_step is the last
-- 30-second step accepted: a code is good once.
CREATE TABLE IF NOT EXISTS mfa_factors (
    subject       TEXT PRIMARY KEY,
    secret_sealed TEXT NOT NULL,
    created_at    INTEGER NOT NULL,   -- epoch seconds
    confirmed_at  INTEGER,            -- epoch seconds; NULL until a first code is shown
    last_step     INTEGER NOT NULL DEFAULT 0
);

-- Recovery codes, stored as SHA-256 of the code; each works once.
CREATE TABLE IF NOT EXISTS mfa_recovery_codes (
    subject    TEXT NOT NULL,
    code_hash  TEXT NOT NULL,
    created_at INTEGER NOT NULL,      -- epoch seconds
    used_at    INTEGER,               -- epoch seconds
    PRIMARY KEY (subject, code_hash)
);

-- Bearer tokens an identity provider uses for /scim/v2. Only a SHA-256 of the
-- token is kept; the owner sees the token once, when it is issued.
CREATE TABLE IF NOT EXISTS scim_tokens (
    id           TEXT PRIMARY KEY,
    token_hash   TEXT NOT NULL UNIQUE,
    label        TEXT NOT NULL DEFAULT '',
    created_by   TEXT NOT NULL,
    created_at   INTEGER NOT NULL,    -- epoch seconds
    last_used_at INTEGER,             -- epoch seconds
    revoked_at   INTEGER              -- epoch seconds
);
