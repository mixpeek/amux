# amux-remote: Quick Start (5 minutes)

Connect to a remote amux server over SSH or Tailscale and manage it from your local machine.

## Setup (One Time)

### Step 1: Get the Token

SSH into the remote machine:

```bash
ssh user@remote-server
cat ~/.amux/auth_token
```

Copy the token (looks like: `aAbBcCdDeE...`)

### Step 2: Find the Server IP

On the remote machine:

**Option A: Via Tailscale (recommended)**
```bash
tailscale ip -4
# Output: 100.64.0.1
```

**Option B: Direct IP**
```bash
hostname -I
# Output: 192.168.1.50
```

### Step 3: Configure Local Machine

Create `~/.amux/remote.env`:

```bash
cat > ~/.amux/remote.env << 'EOF'
AMUX_URL=https://100.64.0.1:8824
AMUX_TOKEN=aAbBcCdDeE...
EOF

chmod 600 ~/.amux/remote.env
```

Replace:
- `100.64.0.1` with actual server IP
- `aAbBcCdDeE...` with the token you copied

### Step 4: Test Connection

```bash
amux-remote ls
```

Should show a list of sessions running on the remote machine.

## Usage

### List Sessions

```bash
amux-remote ls
```

Shows all active sessions.

### View Output

```bash
amux-remote peek session-name
```

Shows last 80 lines of output.

### Send Commands

```bash
amux-remote send session-name "prompt"
```

Sends text to a session.

### SSH into a Session

```bash
amux-remote attach session-name
```

SSH in and attach to the tmux session (native iTerm2 tabs if in iTerm2).

### Get Full Info

```bash
amux-remote info session-name
```

Shows session status as JSON.

## Troubleshooting

### "Connection refused"

- Check AMUX_URL is correct
- Verify network connectivity
- Ensure server is running: `systemctl --user status amux-server` (on remote)

### "Token invalid"

- Verify token: `cat ~/.amux/auth_token` (on remote)
- Update `~/.amux/remote.env` with correct token
- Token must match exactly

### "Server doesn't recognize this session"

- Session may have been deleted
- Try: `amux-remote ls` to see valid sessions

## Next Steps

- Full guide: [docs/amux-remote-guide.md](amux-remote-guide.md)
- Multiple servers: Use `~/.amux/remote.d/` for different configs
- Tailscale: [amux-remote-guide.md#tailscale-setup](amux-remote-guide.md#tailscale-setup)
