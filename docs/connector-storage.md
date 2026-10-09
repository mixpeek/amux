# Connector storage and maintenance

Connector definitions and credentials stay in the existing AMUX_HOME files:

| File | Purpose |
| --- | --- |
| `server.env` | API keys, OAuth client credentials and connector settings |
| `connectors/custom.json` | Owner-declared connector definitions |
| `connectors/<family>/<account>.json` | Google, Slack and other per-account grants |
| `connectors/pending.json`, `gmail-pending.json` | Pending, single-use OAuth state |
| `gmail-tokens/<account>.json` | Legacy Gmail grants and compatibility copies |

Secret-bearing writes create a unique sibling temporary file with mode 0600,
write and sync its contents, atomically replace the destination, then sync the
parent directory before acknowledging success. Newly created store directories
are 0700. Read/modify/write updates take a stable OS sidecar lock, independent
of the data inode replaced by rename. The lock wait is bounded and process
termination releases it. Malformed or unreadable saved configuration is an
error; it is not an empty store that can be silently replaced.

An OAuth refresh holds an account lease across reading the saved grant, calling
the provider and committing its replacement. Consumers re-read after acquiring
the lease. A returned nonblank refresh token replaces the old one; omitted,
null and blank refresh tokens retain it. Refreshes preserve arbitrary metadata,
including identity verification and scopes. A failure to commit produces an
error rather than a successful bearer or health result. Gmail reads committed
state instead of caching an access token ahead of persistence. Its 401 retry
names the rejected token, allowing it to reuse a peer's committed replacement.
Normal Slack bot tokens with neither an expiry nor a refresh token remain
usable; their lifetime is reported as unknown rather than a fabricated TTL.

Google and Gmail consumers take the same account lease. A Gmail compatibility
copy is linked only when its client ID and refresh credential match a Google
grant that covers Gmail. Before rotating the canonical grant, its commit
records a SHA-256 fingerprint of the linked copy's refresh token. After a crash
between the canonical and compatibility writes, Gmail can identify and read
the committed canonical grant. A later independently authorized Gmail grant,
a different OAuth client, or an explicitly disconnected Gmail account stays
independent. Reading does not import, consolidate or recreate grants.

Storage failures appear in logs as `connector_store_commit_failed`,
`connector_store_lock_timeout`, `connector_refresh_lease_failed`,
`connector_grant_unreadable`, `connector_gmail_mirror_deferred` or
`gmail_disconnect_commit_failed`. Successful credential rotation emits
`connector_refresh_rotated`; no token values are logged by these signals.
A deferred compatibility write is recoverable because linked readers use the
committed grant, and the next successful rotation updates the copy.

Recovery tests use private AMUX_HOME directories, synthetic credentials,
mocked OAuth transport, actual SIGKILL at file-publication and compatibility
boundaries, and a separate real server over TLS. They do not use live accounts.
This protects acknowledged local state against process termination; it does
not provide off-host disaster recovery or an atomic transaction with the
remote OAuth provider. Provider revocation still requires reauthorization.
