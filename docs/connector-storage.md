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
The compatibility-copy fault uses a read-only directory. When the Linux test
controller is root, only that fixture's server drops to uid/gid 65534 in a
private temporary tree: root otherwise bypasses the fault. The test copies its
executable into that tree, verifies the child UID/GID, and retains the stale-copy,
committed-token, one-refresh and SIGKILL assertions. Shared caches and production
paths keep their permissions.
This protects acknowledged local state against process termination; it does
not provide off-host disaster recovery or an atomic transaction with the
remote OAuth provider. Provider revocation still requires reauthorization.

## Idle connection upkeep

The registered `connector-maintenance` server job runs immediately at startup
and every 300 seconds, independently of the board/autofix job. It refreshes saved
Google, rotating Slack and declared OAuth grants with at most ten minutes of
access-token lifetime remaining, through the same account lease and atomic
commit as worker token minting. Gmail-only legacy grants with unknown expiry get
a bounded daily idle refresh; successful minting now records their access-token
expiry. Long-lived keys and nonrenewable sessions are canaried, not replaced.

Maintenance preserves definitions, credentials not being rotated, account
identities, scope pins and owner metadata. It cannot reconnect a disconnected
account, select another account as a fallback, expand consent, or perform an
outbound business action. Configured API-key canaries feed the existing Test
status; token/account canaries use the consolidated account-health view.
`AMUX_CONNECTOR_MAINTENANCE_SECS=0` and the existing global isolated-server
switches explicitly disable the job and leave its disabled registry row visible.

`GET /api/connectors/maintenance` reports a private, atomically saved receipt:
`measured`, `n_considered`, `checked_at`, age/staleness, grant verdicts and canary
statuses. Provider response details and token values are excluded. Live Test
receipts now use a stable lock plus atomic private publication, retaining corrupt
previous state instead of overwriting it. Canary snapshots are also published
atomically. Failures log `connector_maintenance_failed`,
`connector_maintenance_report_not_durable` or `connector_test_record_not_durable`;
completed passes log `connector_maintenance_pass` with measurement and failure
counts. No saved-connection deletion or account substitution is a recovery action.

No OAuth client can promise access forever. Google can revoke refresh grants,
expire unused refresh tokens, or impose a seven-day lifetime for external users
of a Testing app. Slack rotating access tokens expire and require renewable
credentials. Keep the OAuth app in an appropriate production/internal state;
maintenance cannot change provider policy or bypass owner consent. See
[Google OAuth lifetime rules](https://developers.google.com/identity/protocols/oauth2#expiration)
and [Slack rotation](https://docs.slack.dev/authentication/using-token-rotation/).
Browser cookie syncing is separate and still needs live account/site validation.
Atomic local persistence is not off-host disaster recovery: the current narrow
credential-backup script does not back up the whole connector vault. Do not treat
a stale backup of a rotated refresh token as guaranteed recoverable authentication.
