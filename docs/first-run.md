# First run

What to do after `./install.sh` finishes, in order. Everything here is local to
your machine; amux does not phone home and needs no account.

## 1. Start the server

**macOS** — `install.sh` loads the launchd agent, so it is already running.

**Linux** — the installer writes systemd user units but deliberately does not
start them. Enable what you want:

```bash
systemctl --user enable --now amux-server.service      # the server
systemctl --user enable --now amux-builder.timer       # rebuild on new commits (optional)
systemctl --user enable --now amux-worker-start.service # start workers at boot (optional)
loginctl enable-linger "$USER"                          # keep them running when logged out
```

Check it: `curl -sk https://localhost:8824/api/health` should answer
`"status":"ok"`, or run `amux server status`.

## 2. Open the dashboard

<https://localhost:8824> — the certificate is self-signed and generated on your
machine, so your browser warns once. On the same machine the page picks up
`~/.amux/auth_token` itself; from another device you supply that token.

## 3. Tell amux who you are

`install.sh` records the owner from `git config user.name`, else your login, in
`~/.amux/server.env`:

```
AMUX_OWNER_NAME=Your Name
```

That name appears on cards and in messages to workers, and `NEEDS-<yourname>:`
becomes a marker amux recognises. Change it there and it takes effect without a
restart. Optional, for alerts: `AMUX_OWNER_EMAIL`, `AMUX_OWNER_PHONE` (texting
needs `TWILIO_*` except on macOS, which can use iMessage).

## 4. Log in to at least one agent CLI

amux runs other people's CLIs; it does not ship a model. Install and sign in to
whichever you want **before** creating a worker, or the worker starts and
immediately sits at a login prompt:

| Worker provider | CLI | Sign in with |
|---|---|---|
| `claude` (default) | Claude Code | `claude` then `/login` |
| `codex` | Codex CLI | `codex` then follow its prompt |
| `gemini` | Gemini CLI | `gemini` then follow its prompt |

amux never stores model API keys; it uses whatever login the CLI already has.

## 5. Create your first worker

From the dashboard: **New worker**, give it a name and a directory. Or:

```bash
amux exec myworker --dir ~/code/myproject        # register + start
amux ls                                          # status of every worker
amux attach myworker                             # watch it (detach: Ctrl-b d)
```

A worker is a real agent CLI in a tmux session, working in the directory you
name. `amux board add "…"` puts work on its board.

## 6. Optional extras

- **Browser automation:** `amux-xvfb.service` plus `amux-playwright-mcp@<lane>-<port>.service`.
- **MCP servers:** put them in `~/.amux/mcp.json` (the copy in the repo root is
  an example and is not read).
- **Email, calendar, tunnel, push:** each is off until you set its keys in
  `~/.amux/server.env`. See [credentials.md](credentials.md) for the inventory.

## Troubleshooting

- **Server will not start:** `journalctl --user -u amux-server -n 50` on Linux,
  or `~/.amux/logs/server-rs.log` on either OS.
- **Worker starts then stops:** attach to it; it is usually the CLI asking you
  to log in, or a directory that does not exist.
- **Port already in use:** set `AMUX_RS_PORT` in `~/.amux/server.env` and
  re-run `./install.sh`.
- **Uninstall:** `./uninstall.sh` removes the binaries, services and CLI. It
  never touches `~/.amux`, so your board, sessions and logs survive.
