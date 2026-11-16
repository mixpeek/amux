# Encryption at rest

What amux encrypts on disk, where the key lives, and what is still plain. The live answer for a
host is `GET /api/security/at-rest` (`amux security at-rest`), which measures the files rather
than reading this page.

## Connector grants: encrypted

OAuth grants are refresh tokens: whoever holds the file holds the account. Every grant file
(`connectors/<family>/<account>.json` and the legacy Gmail mirror) is sealed with AES-256-GCM
(`integrations/at_rest.rs`). The file stays a JSON object, an envelope with `amux_at_rest`,
`purpose`, `fp`, `nonce` and `ct`. The associated data is the purpose plus the file's path under
the home, so a ciphertext copied over another account's file does not open.

- Writes seal before the file touches disk. If the key is unavailable the write fails; a plain
  copy is never the fallback.
- Reads open the envelope. A plain grant is sealed in place on its first read, and a pass at
  every boot seals the rest. The sealed copy replaces the plain one atomically (temp file,
  fsync, rename). The plain bytes may still sit in free disk blocks until reused: full-disk
  encryption is what covers that.
- A grant that does not open is **locked**: the mint answers 503 `status: locked`, Gmail health
  says `locked`, and the report counts it. It never reads as "not connected", which would send
  the owner to reconnect an account that is fine.

## The key

One key source per host (A10). `AMUX_KEY_PROVIDER` chooses it:

| Provider | Key | Colocated with the data |
|---|---|---|
| `file` (Linux default) | the vault master key, a 0600 file in the home | yes: a copy of the home carries both |
| `keychain` (macOS default) | the vault master key in the login Keychain | no |
| `env` | `AMUX_AT_REST_KEY`, 64 hex chars or base64 of 32 bytes, injected by the operator (systemd credential, cloud secret) | no |
| `kms` | an interface stub: refuses, so grants stay locked | no |

The at-rest key is derived from the master with HMAC-SHA256 for this purpose, so it is not the
vault's key even when both come from one master. On a Linux server with `file`, encryption at
rest protects a grant from a stray copy of one file, not from a copy of the whole home: use
`env` (or full-disk encryption) for that.

KMS (AWS, GCP) is not built. The interface is `at_rest::purpose_key`; a provider would fetch or
unwrap a data key there.

## Switches

- `AMUX_ENCRYPT_AT_REST=off` stops sealing new writes and stops migration. Sealed grants still
  open.
- The owner's way back to plain files: set `AMUX_ENCRYPT_AT_REST=off`, restart, then
  `amux security decrypt-grants` (`POST /api/security/at-rest/decrypt`, owner only). It refuses
  while sealing is on, because the next write would seal the grant again.
- Counters: `at_rest` in `GET /api/contract/counters`. Log verdicts: `at_rest_migrated`,
  `at_rest_migration_pass`, `at_rest_migration_failed`, `at_rest_decrypted_by_owner`.

## Not encrypted yet

- **The database (`amux.db`).** rusqlite is built with `bundled` SQLite. SQLCipher would mean
  the `bundled-sqlcipher` feature (a vendored OpenSSL or a system libcrypto on every build host,
  macOS and Linux, and CI), a key at open, and a one-time `sqlcipher_export` of every existing
  database, with Litestream replicating ciphertext. That is its own change. Until then use
  full-disk encryption: LUKS on Linux servers, FileVault on Macs. Cloud tenants' Litestream
  replicas in R2 are encrypted by R2 at rest.
- **Transcripts (`~/.claude/projects`).** Claude Code writes them; amux reports them and does
  not touch them (A2). Full-disk encryption covers them.
- **`gmail-oauth-client.json` and `server.env`.** Client configuration and settings, not grants.
  The values that are secrets belong in the vault (`vault_secrets`), which is encrypted.
- **The cloud deploy script** (`cloud/start/deploy.sh`) can no longer read a sealed hello@amux.io
  grant; it skips it with a warning and uses the service account.
