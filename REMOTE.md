# Remote amux over Tailscale

Drive an amux server running on another machine — list, peek, message, and **attach**
to its sessions from your laptop, with the attach rendered as **native iTerm2 tabs**
so a remote session feels local.

The `amux` CLI is local-only (`amux ls` reads `~/.amux` on the machine it runs on).
`amux-remote` is the companion that talks to a remote amux server over its REST API
and SSHes in for `attach`.

---

## Quick start

On your **laptop** (the client), create `~/.amux/remote.env`:

```bash
AMUX_URL=https://desktop.tail5ce8f5.ts.net:8822   # server: HTTPS, port 8822
AMUX_TOKEN=<contents of the server's ~/.amux/auth_token>
AMUX_SSH_HOST=desktop.tail5ce8f5.ts.net            # SSH target for `attach`
# AMUX_SSH_USER=ethan                              # default: $USER
# AMUX_CC=1                                         # force native attach everywhere
```

Then:

```bash
amux-remote ls                          # list the server's sessions
amux-remote attach mixpeek-frustrations # SSH in + attach (native iTerm2 tabs in iTerm2)
```

> The amux server listens on **HTTPS, port 8822** with a self-signed cert
> (`amux-remote` uses `curl -sk`). Plain `http://host/` (port 80) is **not** the
> server unless you've set up `tailscale serve` separately.

---

## Configuration

Read from the environment, or from `~/.amux/remote.env` (process env always wins):

| Var | Required | Meaning |
|-----|----------|---------|
| `AMUX_URL` | yes | Base URL of the remote server, e.g. `https://desktop.tail5ce8f5.ts.net:8822` |
| `AMUX_TOKEN` | yes | Auth token — copy from the server's `~/.amux/auth_token` |
| `AMUX_SSH_HOST` | for `attach` | SSH hostname (defaults to the host parsed from `AMUX_URL`) |
| `AMUX_SSH_USER` | no | SSH user for `attach` (default: `$USER`) |
| `AMUX_CC` | no | Native iTerm2 attach: `1`/`0` to force, `auto` (default) = on in iTerm2 |

---

## Commands

```
amux-remote ls                          list sessions
amux-remote attach <name> [--cc|--plain] SSH in + attach (native iTerm2 tabs in -CC mode)
amux-remote peek <name> [lines]         print recent output (default 80)
amux-remote send <name> <text...>       send text/prompt to a session
amux-remote start <name>                start a session
amux-remote stop <name>                 stop a session
amux-remote info <name>                 session status/meta as JSON
amux-remote url                         print the configured server URL
amux-remote curl <path> [args...]       raw authenticated GET against the API
```

---

## Native iTerm2 attach (the "feels local" part)

`amux-remote attach` uses **tmux control mode** (`tmux -CC`) when your local terminal
is **iTerm2**. Instead of tmux drawing its own panes inside one terminal window,
iTerm2 renders the remote session's windows/panes as **native macOS tabs and splits**:

- trackpad scrollback (no tmux copy-mode)
- copy/paste straight to your **local** clipboard
- `Cmd+T` / `Cmd+D` for new tab / split, mapped to the remote tmux
- the session persists server-side — close the lid, reconnect, re-attach, it's all there

**Detection** (`AMUX_CC`):

| `AMUX_CC` | Behavior |
|-----------|----------|
| `auto` (default) | native `-CC` when `TERM_PROGRAM=iTerm.app` or `LC_TERMINAL=iTerm2`, else classic |
| `1` / `yes` / `on` | force native `-CC` (use for other control-mode terminals) |
| `0` / `no` / `off` | force classic `tmux attach` |

Per-invocation overrides (before or after the name):

```bash
amux-remote attach mixpeek-frustrations --plain   # force classic, e.g. over a flaky link
amux-remote attach mixpeek-frustrations --cc       # force native
```

Outside iTerm2 (Terminal.app, Ghostty, plain ssh) it transparently falls back to
classic `tmux attach-session`.

---

## SSH setup over Tailscale

`attach` runs `ssh <user>@<AMUX_SSH_HOST> -t "tmux … attach-session -t amux-<name>"`,
so the SSH hop must work. Two options:

**Tailscale SSH (no key management) — recommended:**
```bash
# on the SERVER:
tailscale up --ssh
```
Then `amux-remote attach <name>` from any tailnet device just works.

**Or normal SSH keys:** ensure your laptop's public key is in the server's
`~/.ssh/authorized_keys` and Remote Login is enabled.

Tip: add a host alias so you don't type the full name:
```
# ~/.ssh/config
Host desktop
    HostName desktop.tail5ce8f5.ts.net
    User ethan
```

---

## Troubleshooting

| Symptom | Cause / fix |
|---------|-------------|
| `HTTP 000` / connection refused | Wrong URL — use `https://host:8822`, not `http://host/`. The server is HTTPS on 8822. |
| `could not parse response (check AMUX_URL / AMUX_TOKEN)` | Bad/missing token, or URL not pointing at the amux server. |
| `Permission denied (publickey)` on attach | SSH not set up — run `tailscale up --ssh` on the server, or add your key to `authorized_keys`. |
| Attach opens classic tmux, not native tabs | You're not in iTerm2, or `AMUX_CC=0`. Use `--cc` to force, or set `AMUX_CC=1`. |
| `Session amux-<name> not found` | The session isn't running on the server. `amux-remote start <name>` first, or `amux-remote ls` to check names. |

---

## How it relates to the local CLI

| | runs on | reads | use for |
|--|---------|-------|---------|
| `amux` | the machine with the sessions | local `~/.amux` | day-to-day local control |
| `amux-remote` | any client (laptop, phone-shell) | a remote server's REST API + SSH | controlling another box over Tailscale |

Remote tmux session names are `amux-<name>` — `amux-remote attach` prefixes this for you.
