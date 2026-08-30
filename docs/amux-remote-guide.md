# amux-remote: Complete Guide

Manage amux servers remotely without opening inbound ports. Works over Tailscale, SSH tunnels, or direct network access.

## Quick Reference

| Task | Command |
|---|---|
| List sessions | `amux-remote ls` |
| View output | `amux-remote peek <name> [lines]` |
| Send command | `amux-remote send <name> <text>` |
| SSH attach | `amux-remote attach <name>` |
| Session info | `amux-remote info <name>` |
| Start session | `amux-remote start <name>` |
| Stop session | `amux-remote stop <name>` |
| Server URL | `amux-remote url` |
| Raw API call | `amux-remote curl <path>` |
| Help | `amux-remote help` |

## Configuration

### Environment Variables

```bash
AMUX_URL       # https://100.64.0.1:8824 or similar
AMUX_TOKEN     # From remote ~/.amux/auth_token
AMUX_SSH_HOST  # Hostname for `attach` command (optional)
AMUX_SSH_USER  # SSH username for `attach` (default: $USER)
AMUX_CC        # iTerm2 native tabs: 1/0/auto (default: auto)
```

### Config File

Create `~/.amux/remote.env`:

```bash
AMUX_URL=https://server.example.com:8824
AMUX_TOKEN=abc123def456...
AMUX_SSH_USER=ubuntu
AMUX_CC=auto
```

Environment variables override the file.

## Setup Methods

### Method 1: Tailscale (Recommended)

**Simplest for internal/team use.**

Both machines must have Tailscale installed:

```bash
# On remote machine
tailscale up
tailscale ip -4  # e.g., 100.64.0.1

# On local machine
tailscale up
```

Configure local machine:

```bash
cat > ~/.amux/remote.env << 'EOF'
AMUX_URL=https://100.64.0.1:8824
AMUX_TOKEN=$(ssh remote-user@100.64.0.1 cat ~/.amux/auth_token)
EOF
```

**Benefits:**
- ✓ No port exposure
- ✓ Encrypted end-to-end
- ✓ Works from anywhere
- ✓ Zero configuration on server side

### Method 2: SSH Tunneling

**Good for one-off access.**

```bash
ssh -L 8824:localhost:8824 user@remote-server

# In another terminal:
export AMUX_URL=https://localhost:8824
export AMUX_TOKEN=$(ssh user@remote-server cat ~/.amux/auth_token)
amux-remote ls
```

**Or in config:**

```bash
cat > ~/.ssh/config << 'EOF'
Host amux-remote
  HostName example.com
  User ubuntu
  LocalForward 8824 localhost:8824
EOF

ssh amux-remote  # in one terminal

# In another:
export AMUX_URL=https://localhost:8824
amux-remote ls
```

**Benefits:**
- ✓ Works over slow/unreliable connections
- ✓ Encrypted tunneling
- ✓ No VPN needed

### Method 3: Direct IP (Internal Network Only)

**For internal networks only — no encryption.**

```bash
cat > ~/.amux/remote.env << 'EOF'
AMUX_URL=https://192.168.1.50:8824
AMUX_TOKEN=abc123...
EOF
```

**Use case:**
- ✓ Private internal network (no external access)
- ✓ High-speed local connection

**Do NOT use for internet access** — use Tailscale or SSH tunnel instead.

## Common Tasks

### List All Remote Sessions

```bash
amux-remote ls
```

Shows status (active/idle/stopped) and description.

### Monitor a Session in Real-Time

```bash
amux-remote peek <session> 100
```

Shows last 100 lines. Doesn't tail in real-time; poll manually:

```bash
watch 'amux-remote peek session-name 80'
```

Or use SSH attach for true real-time (see below).

### SSH + Attach (Interactive)

```bash
amux-remote attach session-name
```

SSH in and attach to the tmux session. Press `Ctrl+B D` to detach.

**macOS + iTerm2:** Opens native tabs (run in iTerm2 for automatic detection):

```bash
amux-remote attach session-name --cc  # Force native tabs
amux-remote attach session-name --plain  # Force plain SSH
```

### Send a Command to a Session

```bash
amux-remote send session-name "your command here"
```

Text is typed into the session (use Enter at the end if needed).

### Start/Stop a Session

```bash
amux-remote start session-name
amux-remote stop session-name
```

Useful for background workers or scheduled jobs.

### Get JSON Metadata

```bash
amux-remote info session-name
```

Returns full session data (status, created_at, description, etc.) as JSON.

### Raw API Calls

```bash
amux-remote curl /api/board
amux-remote curl /api/schedules
```

Bypasses the friendly CLI and goes straight to REST API. Requires Token auth (automatic).

## Troubleshooting

### Connection Issues

```bash
# Test configuration
amux-remote url

# Verbose curl output
amux-remote curl /api/health -v

# Check token
amux-remote info <any-session>
```

**Common errors:**

| Error | Cause | Fix |
|---|---|---|
| `Could not resolve host` | DNS failure | Check AMUX_URL hostname |
| `Connection refused` | Server not running | SSH to remote, check `systemctl --user status amux-server` |
| `SSL certificate error` | TLS issue | Use `-k` flag (already done) or check cert date |
| `401 Unauthorized` | Bad token | Verify `AMUX_TOKEN` matches remote file |
| `404 not found` | Wrong AMUX_URL or session doesn't exist | Check URL and session name with `amux-remote ls` |

### Token Management

**Generate a new token:**

```bash
ssh user@remote-server
rm ~/.amux/auth_token  # Invalidates all remote connections
systemctl --user restart amux-server  # New token generated on restart
cat ~/.amux/auth_token
```

Then update `~/.amux/remote.env` with the new token.

### Multi-Host Setup

Store different server configs:

```bash
mkdir -p ~/.amux/remote.d

# Development server
cat > ~/.amux/remote.d/dev.env << 'EOF'
AMUX_URL=https://dev-server:8824
AMUX_TOKEN=dev-token-here
EOF

# Production server
cat > ~/.amux/remote.d/prod.env << 'EOF'
AMUX_URL=https://prod-server:8824
AMUX_TOKEN=prod-token-here
EOF

# Use:
source ~/.amux/remote.d/dev.env
amux-remote ls  # connects to dev

source ~/.amux/remote.d/prod.env
amux-remote ls  # connects to prod
```

### Security Notes

**Token handling:**
- ✓ Stored in `~/.amux/remote.env` (chmod 600, readable by you only)
- ✓ Never printed to logs or shell history
- ✓ Transmitted over HTTPS only (via curl -sk)
- ✓ Server-side: verify token before any operation

**Network:**
- ✓ Always use HTTPS (http://) is rejected
- ✓ Use Tailscale or SSH tunnel for untrusted networks
- ✓ Token is session-scoped (doesn't grant file system access)

**SSH attach:**
- ✓ Still requires SSH key authentication
- ✓ AMUX_TOKEN is not used for SSH, only for HTTP API

## Integration Examples

### CI/CD: Deploy and Verify

```bash
#!/bin/bash
export AMUX_URL=https://prod-server:8824
export AMUX_TOKEN=$AMUX_PROD_TOKEN

# Deploy
./deploy.sh

# Check amux server is healthy
amux-remote curl /api/health || exit 1

# Start a verification job
amux-remote send verify-worker "run integration-tests"

# Poll for completion
while amux-remote info verify-worker | grep -q '"status":"active"'; do
  sleep 5
done

echo "Done!"
```

### Kubernetes: Run amux-remote in a Pod

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: amux-monitor
spec:
  containers:
  - name: monitor
    image: alpine:latest
    env:
    - name: AMUX_URL
      value: "https://amux.default.svc.cluster.local:8824"
    - name: AMUX_TOKEN
      valueFrom:
        secretKeyRef:
          name: amux-token
          key: token
    command: ["/bin/sh"]
    args:
      - -c
      - |
        apk add --no-cache curl bash
        while true; do
          sessions=$(wget -qO- --no-check-certificate https://amux.default.svc.cluster.local:8824/api/sessions)
          echo "Active sessions: $(echo $sessions | wc -l)"
          sleep 60
        done
```

### Monitoring: Periodic Health Check

```bash
#!/bin/bash
# Run every 5 minutes via cron

export AMUX_URL=https://prod-server:8824
export AMUX_TOKEN=$AMUX_PROD_TOKEN

if ! amux-remote curl /api/health >/dev/null 2>&1; then
  echo "ALERT: amux server is down" | mail -s "amux Alert" admin@example.com
fi
```

## See Also

- [Quick Start](amux-remote-quick-start.md) — 5-minute setup guide
- [SSH Tunneling Details](#method-2-ssh-tunneling)
- [Tailscale Setup](#method-1-tailscale-recommended)
- `amux-remote help` — built-in command reference
