# GitHub Connector Setup

Configure these secrets for GitHub integration (Phase 7):

## 1. Create OAuth Application

Visit: https://github.com/settings/developers/new

- **Application name:** amux-github-connector
- **Homepage URL:** https://localhost:8824
- **Authorization callback URL:** http://localhost:8824/api/github/callback
- **Description:** Local amux GitHub integration

## 2. Store Credentials in Secrets

Update `~/.amux/server.env` with your GitHub OAuth credentials:

```bash
# GitHub OAuth (for Phase 7 connector)
EXTERNAL_SERVICES_GITHUB_CLIENT_ID=Ov23liXXXXXXX
EXTERNAL_SERVICES_GITHUB_CLIENT_SECRET=<client_secret>
EXTERNAL_SERVICES_GITHUB_REDIRECT_URI=http://localhost:8824/api/github/callback
```

Or encrypt in secrets store:

```bash
# Add to secrets/amux-secrets.yaml
oauth:
  github:
    client_id: Ov23liXXXXXXX
    client_secret: <secret>
    redirect_uri: http://localhost:8824/api/github/callback
```

## 3. Test Connection

```bash
# Check GitHub connector status
curl -sk https://localhost:8824/api/github/status

# Start OAuth flow
curl -sk https://localhost:8824/api/github/auth/start
```

## 4. Features Enabled

With Phase 7 complete:
- ✓ GitHub issue sync to amux board
- ✓ Pull request automation
- ✓ Repository organization
- ✓ Webhook integration
- ✓ Credentials stored encrypted in secrets

All credentials are stored using the Phase 1-4 secrets infrastructure (encrypted at rest, decrypted once at startup).
