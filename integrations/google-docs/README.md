# Google Docs / Google Drive Integration

> **Deprecated (2026-10-09).** `@modelcontextprotocol/server-gdrive` was archived by the MCP project in 2025 and gets no security fixes, and this setup hands it a Drive refresh token. Use the built-in Google connector instead: Connectors → Google → Add account. One approval covers Drive, Docs, Gmail and Calendar, and workers mint short-lived tokens from it. The steps below are kept for reference only.

Gives amux sessions read/write access to Google Docs, Sheets, and Drive via MCP.

## What sessions can do

- Read and write Google Docs and Sheets
- Search Drive for files by name or content
- Create new documents
- Export docs as markdown/plain text

## Required env vars

Add these to `~/.amux/server.env`:

```bash
GDRIVE_CLIENT_ID=...
GDRIVE_CLIENT_SECRET=...
GDRIVE_REFRESH_TOKEN=...
```

## Setup (one-time)

### 1. Create OAuth credentials

1. Go to [Google Cloud Console](https://console.cloud.google.com/)
2. Create a project (or use an existing one)
3. Enable the **Google Drive API** and **Google Docs API**
4. Create OAuth 2.0 credentials → Desktop App
5. Download the credentials JSON

### 2. Run the setup script

```bash
./integrations/google-docs/setup.sh /path/to/credentials.json
```

This opens a browser for the OAuth flow and writes the three env vars to `~/.amux/server.env`.

### 3. Activate

```bash
launchctl kickstart -k gui/$(id -u)/com.amux.server-rs   # restart the server to reload
```

Start a new session — it will have Google Docs tools available.

## mcp.json entry

Already present in `mcp.json` (uses `${...}` placeholders — credentials come from `server.env`):

```json
"gdocs": {
  "type": "stdio",
  "command": "npx",
  "args": ["-y", "@modelcontextprotocol/server-gdrive"],
  "env": {
    "GDRIVE_CLIENT_ID": "${GDRIVE_CLIENT_ID}",
    "GDRIVE_CLIENT_SECRET": "${GDRIVE_CLIENT_SECRET}",
    "GDRIVE_REFRESH_TOKEN": "${GDRIVE_REFRESH_TOKEN}"
  }
}
```

## Scopes requested

- `https://www.googleapis.com/auth/drive` — full Drive access
- `https://www.googleapis.com/auth/documents` — read/write Docs

Reduce to `drive.readonly` / `documents.readonly` if you only need read access.
