# Encrypted Secrets Management

**Central documentation for managing encrypted secrets with metadata tracking.**

All workers should know how to add, fetch, update, and manage secrets. This is the authoritative guide.

---

## Quick Start

```bash
# View all secrets with metadata
curl -sk https://localhost:8824/api/secrets/manifest | python3 -m json.tool

# Get a specific secret
curl -sk https://localhost:8824/api/secrets/oauth.google.main

# Get metadata for a secret
curl -sk https://localhost:8824/api/secrets/oauth.google.main/metadata

# Update metadata
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{
    "secret_path":"oauth.google.main",
    "service_name":"google-oauth",
    "purpose":"OAuth 2.0 credentials",
    "used_by":["dashboard","api"],
    "owner":"platform-team",
    "rotation_days":90
  }' https://localhost:8824/api/secrets/oauth.google.main/metadata
```

---

## Architecture

### Two-Layer Design

```
┌─────────────────────────────────────────┐
│         REST API (Port 8824)            │
│  /api/secrets/manifest                  │
│  /api/secrets/{path}                    │
│  /api/secrets/{path}/metadata           │
└──────────┬──────────────────────────────┘
           │
    ┌──────┴──────┐
    │             │
    ▼             ▼
[Secrets]    [Metadata]
    │             │
    ▼             ▼
amux-secrets   secret_metadata
.yaml (age)    table (SQLite)
[encrypted]    [queryable]
```

**Layer 1: Encrypted Secrets (SOPS/age)**
- Location: `~/secrets/amux-secrets.yaml` (i.e. `/home/<user>/secrets/amux-secrets.yaml`)
- Format: Binary (age-encrypted)
- Access: `/api/secrets/{path}` → returns decrypted value
- Content: Pure key-value pairs, no metadata

**Layer 2: Metadata (SQLite)**
- Table: `secret_metadata` in `~/.amux/amux.db`
- Access: `/api/secrets/manifest` → returns all with metadata
- Content: Service, purpose, owner, rotation schedule, dependencies
- Benefits: Discoverable without decryption, enables queries

---

## Managing Secrets

### Add a New Secret

**Step 1: Encrypt and persist the secret**

```bash
# Via API (recommended)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"value":"<actual-secret-value>"}' \
  https://localhost:8824/api/secrets/my.new.secret
```

**Step 2: Add metadata**

```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{
    "secret_path": "my.new.secret",
    "service_name": "my-service",
    "purpose": "What this secret is for",
    "used_by": ["service1", "service2"],
    "owner": "team-name",
    "rotation_days": 90
  }' https://localhost:8824/api/secrets/my.new.secret/metadata
```

### Fetch a Secret

**Get the actual secret value:**
```bash
curl -sk https://localhost:8824/api/secrets/oauth.google.main
# Returns: {"value":"<decrypted-secret>"}
```

**Get metadata about a secret:**
```bash
curl -sk https://localhost:8824/api/secrets/oauth.google.main/metadata
# Returns: service, purpose, owner, rotation info, dependencies
```

**List all secrets with metadata:**
```bash
curl -sk https://localhost:8824/api/secrets/manifest
# Returns: Array of all secrets with full metadata
```

### Update Secret Metadata

**Edit service info, owner, or rotation schedule:**

```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{
    "secret_path": "oauth.google.main",
    "service_name": "google-oauth-connector",
    "purpose": "Updated purpose if needed",
    "used_by": ["dashboard", "api-gateway", "mobile"],
    "owner": "security-team",
    "rotation_days": 60
  }' https://localhost:8824/api/secrets/oauth.google.main/metadata
```

**Note:** Metadata updates do NOT change the secret value itself. To change a secret value, use the POST to `/api/secrets/{path}` endpoint.

---

## Current Secrets in Use

| Path | Service | Purpose | Owner | Rotation |
|------|---------|---------|-------|----------|
| `oauth.google.main` | google-oauth-connector | Google OAuth 2.0 credentials | platform-team | 90 days |
| `amux.server-auth-token` | amux-server | API authentication token | platform-team | 365 days |
| `ntfy.notification-topic` | ntfy-notifications | Push notifications | ops-team | 180 days |
| `github.alice` | github-cli | Example: GitHub PAT for the `alice` account | platform-team | 90 days |
| `github.bob` | github-cli | Example: GitHub PAT for the `bob` account | platform-team | 90 days |
| `gitlab.main` | gitlab-cli | Example: GitLab PAT for gitlab.com - `alice` account | platform-team | 90 days |
| `gitlab.example1` | gitlab-cli | Example: GitLab PAT for a self-hosted GitLab - `bob` account | platform-team | 90 days |
| `gitlab.example2` | gitlab-cli | Example: GitLab PAT for another self-hosted GitLab - `bob` account | platform-team | 90 days |
| `gitlab.example3` | gitlab-cli | Example: GitLab PAT for a third self-hosted GitLab - `alice` account | platform-team | 90 days |

---

## Rotation Tracking

The system automatically calculates:
- `needs_rotation`: Boolean - is the secret overdue for rotation?
- `days_until_rotation`: Number - days remaining before due

**Example response:**
```json
{
  "path": "oauth.google.main",
  "rotation_days": 90,
  "last_rotated": "2026-08-25T00:00:00Z",
  "needs_rotation": false,
  "days_until_rotation": 80
}
```

---

## Dashboard UI

The web dashboard (https://localhost:8824) shows:
- Secret names and paths
- Associated service name
- Purpose and description
- Owner and rotation schedule
- "Used by" dependencies
- ⚠️ Warning badge if rotation is overdue
- Search: filters by path, service, purpose, or owner

---

## Search and Discovery

**Find secrets by service:**
```bash
curl -sk https://localhost:8824/api/secrets/manifest | \
  python3 -c "import json,sys; d=json.load(sys.stdin); \
  [print(s['path']) for s in d['secrets'] if 'oauth' in s['service']]"
```

**Find secrets needing rotation:**
```bash
curl -sk https://localhost:8824/api/secrets/manifest | \
  python3 -c "import json,sys; d=json.load(sys.stdin); \
  [print(s['path']) for s in d['secrets'] if s.get('needs_rotation')]"
```

**Search by owner:**
```bash
curl -sk https://localhost:8824/api/secrets/manifest | \
  python3 -c "import json,sys; d=json.load(sys.stdin); \
  [print(s['path']) for s in d['secrets'] if 'security' in s.get('owner','').lower()]"
```

---

## For Claude Agents (Phase 6+)

Agents can discover and use secrets via REST API:

```python
# Discover available secrets
GET /api/secrets/manifest
# Returns array of secrets with full metadata

# Fetch a specific secret
GET /api/secrets/{path}
# Returns decrypted value (requires auth token)

# Check rotation status
GET /api/secrets/{path}/metadata
# Returns rotation tracking info
```

---

## Security Best Practices

1. **Never commit secrets to git** - they are encrypted in SOPS, not in version control
2. **Metadata is not encrypted** - don't put sensitive details in purpose/owner fields
3. **Rotate regularly** - set `rotation_days` to appropriate interval (90-180 for API keys)
4. **Track dependencies** - use `used_by` to know what breaks if a secret fails
5. **Use descriptive names** - path format: `service.credential-type.variant` (e.g., `oauth.google.main`)
6. **Auth required** - all secret mutations require `AMUX_AUTH_TOKEN`

---

## Troubleshooting

**"metadata not found for this secret"**
- The secret value exists but has no metadata entry
- Add metadata via POST to `/api/secrets/{path}/metadata`

**"secret not found"**
- Neither value nor metadata exists
- Add the secret value first via POST to `/api/secrets/{path}`

**Port 8824 not responding**
- Check: `ps aux | grep amux-server-rs`
- Restart: `pkill -9 amux-server-rs && ~/.local/bin/amux-server-rs &`
- Verify: `curl -sk https://localhost:8824/health`

**"Permission denied" during build**
- Server binary lost execute permissions
- Fix: `chmod +x ~/.local/bin/amux-server-rs`

---

## Implementation Details

- **Encryption**: age (X25519 elliptic curve)
- **Secret Storage**: YAML file (age-encrypted)
- **Metadata Storage**: SQLite table with indexes on service_name, owner
- **API**: Axum REST endpoints with full CRUD
- **Caching**: In-memory cache with reload capability
- **Rotation**: Chrono UTC timestamps, automatic calculation

---

## References

- **Code**: `crates/amux-server/src/db/secret_metadata.rs` (CRUD operations)
- **API**: `crates/amux-server/src/api/secrets.rs` (REST endpoints)
- **UI**: `crates/amux-dashboard/static/secrets-ui.js` (Web dashboard)
- **Migration**: `crates/amux-server/migrations/0033_secret_metadata.sql` (Database schema)
- **Config**: `~/.amux/server.env` (Secrets that can be managed here)

---

**Last Updated**: 2026-08-25  
**Maintainer**: Platform Team  
**Questions**: Check CLAUDE.md or create an issue
