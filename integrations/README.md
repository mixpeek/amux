# amux Integrations

Integrations connect external services to amux sessions via MCP (Model Context Protocol).

## How integrations work

1. **MCP server** — each integration is an MCP server entry in `/amux/mcp.json`
2. **Credentials** — stored in `~/.amux/server.env` (never committed, loaded at startup)
3. **Sessions** — every amux session inherits the MCP config; new sessions pick it up automatically

```
integrations/
└── <service-name>/
    ├── README.md    ← setup instructions and required env vars
    └── setup.sh     ← one-time credential/OAuth flow (optional)
```

## Adding a new integration

### 1. Create the directory

```bash
mkdir integrations/<service-name>
```

### 2. Add the MCP entry to `mcp.json`

```json
"<service-name>": {
  "type": "stdio",
  "command": "npx",
  "args": ["-y", "<mcp-package-name>"],
  "env": {
    "SOME_API_KEY": "${SOME_API_KEY}"
  }
}
```

Use `${ENV_VAR}` syntax — Claude Code substitutes values from the environment at runtime.

### 3. Add credentials to `~/.amux/server.env`

```bash
echo 'SOME_API_KEY=your-key-here' >> ~/.amux/server.env
```

`server.env` is loaded by the amux server on startup via `os.environ.setdefault`, so all sessions see these vars without any restart.

### 4. Reload

```bash
launchctl kickstart -k gui/$(id -u)/com.amux.server-rs   # restart the server to reload
```

New sessions started after this point will have the integration available.

### 5. Write a `README.md`

Document:
- What the integration does
- Required env vars (names only, never values)
- Where to get credentials
- Any one-time setup steps

## Rules

- **Never commit credentials.** `~/.amux/server.env` is outside the repo.
- **`mcp.json` uses placeholders only.** `${VAR_NAME}` — no real values.
- **One directory per integration.** Keep each integration self-contained.
- **`setup.sh` is optional but encouraged** for OAuth flows or multi-step credential setup.

## Available integrations

| Integration | Status | MCP package |
|-------------|--------|-------------|
| [Google Docs](./google-docs/) | deprecated: use the built-in Google connector | `@modelcontextprotocol/server-gdrive` (archived upstream) |
