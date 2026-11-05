# Systemd Setup Guide (Linux)

This guide explains how to run amux as a systemd user service on Linux.

## Overview

amux on Linux runs as a **user-level systemd service** — no sudo required, and the service persists through login sessions.

**What you get:**
- ✓ Automatic startup when you log in
- ✓ Automatic restart if the process crashes
- ✓ Automatic rebuild when code changes (via amux-builder.timer)
- ✓ Structured logs in journalctl
- ✓ Lifecycle management via systemctl

## Quick Start

### 1. Install amux

```bash
cd /path/to/amux
./install.sh
```

On systemd-based Linux, this automatically:
- Creates service files in `~/.config/systemd/user/`
- Runs `systemctl --user daemon-reload`
- Prints next steps

### 2. Enable and Start Services

```bash
# Enable server to start on login
systemctl --user enable amux-server

# Enable builder timer to watch for code changes
systemctl --user enable amux-builder.timer

# Start the server now
systemctl --user start amux-server
```

### 3. Verify It's Running

```bash
# Check service status
systemctl --user status amux-server

# View logs
journalctl --user -u amux-server -f
```

### 4. Access the Dashboard

Open your browser and navigate to:
```
https://localhost:8824
```

Token is in: `~/.amux/auth_token`

## Managing the Service

### Check Status

```bash
# Server status
systemctl --user status amux-server

# Builder status
systemctl --user status amux-builder.timer
systemctl --user status amux-builder.service

# All amux-related units
systemctl --user list-units | grep amux
```

### View Logs

```bash
# Server logs (last 50 lines)
journalctl --user -u amux-server -n 50

# Follow logs in real-time
journalctl --user -u amux-server -f

# Builder logs
journalctl --user -u amux-builder -n 50

# Combined (server + builder)
journalctl --user -u amux-server -u amux-builder -f
```

### Stop / Restart

```bash
# Stop the server
systemctl --user stop amux-server

# Restart the server
systemctl --user restart amux-server

# Stop everything (server + builder)
systemctl --user stop amux-server amux-builder.timer
```

### Disable Services

```bash
# Disable auto-start
systemctl --user disable amux-server
systemctl --user disable amux-builder.timer

# Services won't start on next login, but you can still run manually:
systemctl --user start amux-server
```

## Service Files

The installation creates three service files:

### `~/.config/systemd/user/amux-server.service`

Main amux server process. 

**Key settings:**
- `Type=simple` — straightforward daemon
- `Restart=always` — restart on crash
- `RestartSec=5` — wait 5 seconds between restarts
- `StandardOutput=journal` — logs to journalctl

### `~/.config/systemd/user/amux-builder.service`

Watches git commits and rebuilds the server binary.

**Runs:** `scripts/rust-auto-build.sh`  
**Triggers:** Timer (every 60s) or on manual start  

### `~/.config/systemd/user/amux-builder.timer`

Periodic timer for the builder service.

**Schedule:**
- First check: 30 seconds after system boot
- Subsequent checks: every 60 seconds while running

## Environment Variables

Service files use these substitutions (set during `install.sh`):

| Variable | Example | Purpose |
|---|---|---|
| `$HOME` | `/home/username` | User home directory |
| `$PORT` | `8824` | Server port (from AMUX_RS_PORT) |
| `$BIN_DIR` | `~/.local/bin` | Where amux-server-rs binary lives |
| `$SCRIPT_DIR` | `/path/to/amux` | amux checkout directory |
| `$AMUX_HOME` | `~/.amux` | amux data directory |

**To override:** Edit the service files in `~/.config/systemd/user/`

```bash
# Edit server service
systemctl --user edit amux-server

# Edit builder service
systemctl --user edit amux-builder

# Reload after changes
systemctl --user daemon-reload
systemctl --user restart amux-server
```

## Troubleshooting

### Service won't start

```bash
# Check status and error messages
systemctl --user status amux-server

# View full logs
journalctl --user -u amux-server -n 100

# Common issues:
# - Port already in use: change AMUX_RS_PORT
# - Binary not found: check $BIN_DIR path
# - Permission denied: check file permissions
```

### Builder not rebuilding code

```bash
# Check if timer is enabled
systemctl --user list-timers | grep amux-builder

# Check builder logs
journalctl --user -u amux-builder -n 50

# Manual trigger (test)
systemctl --user start amux-builder.service

# Check if it ran
journalctl --user -u amux-builder -f
```

### Logs not appearing

```bash
# Journalctl can be finicky with paths. Try:
journalctl --user --all -u amux-server

# Or use grep:
journalctl --user -u amux-server | grep "pattern"

# Last 100 entries across all amux services:
journalctl --user -u amux-server -u amux-builder -n 100
```

### Port already in use

```bash
# Find what's using the port
lsof -i :8824

# Change port (edit service file)
systemctl --user edit amux-server
# Add or change: Environment="AMUX_RS_PORT=8825"

systemctl --user daemon-reload
systemctl --user restart amux-server
```

### Service doesn't survive logout, or doesn't come back after a reboot

Both symptoms are the same cause, and it has a one-command fix:

```bash
sudo loginctl enable-linger $(id -un)
```

A systemd **user** unit runs inside a session. Without lingering, systemd starts
your user manager at login and tears it down at logout, so every unit this
installer writes — all of them `WantedBy=default.target` — stops with it and does
not come back at boot. On a headless box that means nothing starts until someone
logs in, which reads as "amux does not work".

`install.sh` now checks this and says so when lingering is off (AF-527). Verify:

```bash
loginctl show-user "$(id -un)" --property=Linger   # want: Linger=yes
```

**Do not reach for a system-level unit for this.** `/etc/systemd/system/amux.service`
needs root, runs as root unless you add `User=`, and puts `~` at `/root` — which
silently breaks every `$HOME`-relative path in these templates. Issue #92's
reporter hand-wrote exactly that unit after a headless install came up dead, and
they only did it because `enable-linger` was documented nowhere in this repo.

The other options below are real but heavier, and none of them is needed just to
survive a logout:
- A Docker container, if you want the whole box's amux isolated
- A dedicated deployment machine (VPS, physical server)

### Multiple Users

Each user can run their own amux:

```bash
# User A
mkdir -p ~/amux-a
cd ~/amux-a
/path/to/amux/install.sh

# User B (same machine, different homedir)
mkdir -p ~/amux-b
cd ~/amux-b
/path/to/amux/install.sh

# Each has independent services:
systemctl --user status amux-server  # Shows User A's service
sudo -u userb systemctl --user status amux-server  # Shows User B's service
```

## Comparison with macOS (launchd)

| Feature | Systemd (Linux) | launchd (macOS) |
|---|---|---|
| Service scope | User-level | User-level |
| Auto-start | Login | Login |
| Restart on crash | ✓ (Restart=always) | ✓ (KeepAlive=true) |
| Auto-rebuild | ✓ (Timer) | ✓ (Agent pair) |
| Logging | journalctl | system log / file |
| Config location | `~/.config/systemd/user/` | `~/Library/LaunchAgents/` |
| Management | `systemctl --user` | `launchctl` |

## Next Steps

- **Logs:** Monitor with `journalctl --user -u amux-server -f`
- **Updates:** Code changes auto-rebuild (via timer)
- **Backups:** Config lives in `~/.amux/` (back it up)
- **Remote:** Use `amux-remote` to manage from another machine

## See Also

- [amux-remote guide](amux-remote-guide.md) — manage remote deployments
- [Install](../README.md#install) — general setup
- systemd docs: `man systemd.service`
