#!/usr/bin/env bash
# amux cloud VM bootstrap — runs as root via GCP startup script
# Idempotent: safe to re-run.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
AMUX_DIR="/opt/amux"
AMUX_DATA="/root/.amux"

log() { echo "[amux-setup] $(date '+%H:%M:%S') $*" | tee -a /var/log/amux-setup.log; }

log "=== amux cloud setup starting ==="

# ── System packages ──
log "Updating packages..."
apt-get update -qq
apt-get install -y -qq \
  tmux git curl wget unzip jq htop \
  python3 python3-pip \
  build-essential ca-certificates gnupg

# ── Node.js 22 LTS (for Claude Code) ──
if ! command -v node &>/dev/null; then
  log "Installing Node.js 22..."
  curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
  apt-get install -y -qq nodejs
fi

# ── Tailscale ──
if ! command -v tailscale &>/dev/null; then
  log "Installing Tailscale..."
  curl -fsSL https://tailscale.com/install.sh | sh
fi
log "Connecting to Tailscale..."
tailscale up \
  --authkey="${tailscale_auth_key}" \
  --hostname=amux-cloud \
  --accept-routes \
  || true

# Wait for Tailscale to get an IP
for i in $(seq 1 30); do
  TS_IP=$(tailscale ip -4 2>/dev/null || true)
  [ -n "$TS_IP" ] && break
  sleep 2
done
log "Tailscale IP: $${TS_IP:-not connected}"

# Get Tailscale hostname for TLS cert
TS_HOSTNAME=$(tailscale status --self --json 2>/dev/null \
  | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('Self',{}).get('DNSName','').rstrip('.'))" \
  || true)
log "Tailscale hostname: $${TS_HOSTNAME:-unknown}"

# ── amux directory ──
mkdir -p "$AMUX_DIR"
mkdir -p "$AMUX_DATA/tls"
mkdir -p "$AMUX_DATA/sessions"
mkdir -p "$AMUX_DATA/logs"
mkdir -p "$AMUX_DATA/memory"
mkdir -p "$AMUX_DATA/board"

# ── Pre-fetch Tailscale TLS cert (so amux server starts with HTTPS) ──
if [ -n "$TS_HOSTNAME" ]; then
  log "Getting Tailscale TLS cert for $TS_HOSTNAME..."
  tailscale cert \
    --cert-file "$AMUX_DATA/tls/$${TS_HOSTNAME}.crt" \
    --key-file  "$AMUX_DATA/tls/$${TS_HOSTNAME}.key" \
    "$TS_HOSTNAME" 2>&1 | tee -a /var/log/amux-setup.log || true
fi

# ── Claude Code CLI ──
if ! command -v claude &>/dev/null; then
  log "Installing Claude Code..."
  npm install -g @anthropic-ai/claude-code 2>&1 | tail -3 || true
fi

# ── tmux config ──
cat > /root/.tmux.conf <<'TMUX'
set -g mouse on
set -g history-limit 50000
set -g default-terminal "screen-256color"
set -g status-style "bg=colour235,fg=colour248"
set -g status-left "#[fg=colour39,bold] amux-cloud #[default]"
set -g status-right "#[fg=colour245]%H:%M "
set -g base-index 1
setw -g pane-base-index 1
TMUX

# ── amux systemd service ──
# Note: amux-server.py is deployed separately via:
#   scp amux-server.py root@<tailscale-ip>:/opt/amux/amux-server.py
# The service will auto-restart and pick it up once the file arrives.
cat > /etc/systemd/system/amux.service <<SVCEOF
[Unit]
Description=amux server
After=network.target tailscaled.service
Wants=tailscaled.service

[Service]
Type=simple
User=root
WorkingDirectory=$AMUX_DIR
ExecStart=/usr/bin/python3 $AMUX_DIR/amux-server.py
Restart=always
RestartSec=5
Environment=HOME=/root
StandardOutput=append:/var/log/amux.log
StandardError=append:/var/log/amux.log

[Install]
WantedBy=multi-user.target
SVCEOF

systemctl daemon-reload
systemctl enable amux
# Don't start yet — amux-server.py hasn't been deployed

# ── Connectivity watchdog (auto-reboot if GCP metadata server goes dark) ──
cat > /usr/local/bin/amux-watchdog.sh <<'WATCHDOG'
#!/bin/bash
# Reboots if the GCP metadata server is unreachable for 5+ minutes.
# This catches kernel network stack failures before Tailscale/SSH die.
FAIL_COUNT=0
FAIL_THRESHOLD=10  # 10 * 30s = 5 minutes
while true; do
  sleep 30
  if curl -sf --max-time 5 http://169.254.169.254/computeMetadata/v1/instance/hostname \
      -H 'Metadata-Flavor: Google' >/dev/null 2>&1; then
    FAIL_COUNT=0
    continue
  fi
  FAIL_COUNT=$((FAIL_COUNT + 1))
  logger -t amux-watchdog "metadata unreachable (fail ${FAIL_COUNT}/${FAIL_THRESHOLD})"
  if [ "$FAIL_COUNT" -ge "$FAIL_THRESHOLD" ]; then
    logger -t amux-watchdog "network dead 5+ min — rebooting"
    systemctl reboot
  fi
done
WATCHDOG
chmod +x /usr/local/bin/amux-watchdog.sh

cat > /etc/systemd/system/amux-watchdog.service <<WDEOF
[Unit]
Description=amux connectivity watchdog
After=network.target

[Service]
Type=simple
User=root
ExecStart=/usr/local/bin/amux-watchdog.sh
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
WDEOF

systemctl daemon-reload
systemctl enable amux-watchdog
systemctl start amux-watchdog

# ── Cert renewal cron (weekly) ──
if [ -n "$TS_HOSTNAME" ]; then
  cat > /etc/cron.weekly/amux-cert-renew <<CRONEOF
#!/bin/bash
tailscale cert \
  --cert-file /root/.amux/tls/$${TS_HOSTNAME}.crt \
  --key-file  /root/.amux/tls/$${TS_HOSTNAME}.key \
  $TS_HOSTNAME && systemctl restart amux
CRONEOF
  chmod +x /etc/cron.weekly/amux-cert-renew
fi

log "=== Setup complete ==="
log ""
log "  Tailscale hostname : $${TS_HOSTNAME:-run 'tailscale status' to check}"
log "  Tailscale IP       : $${TS_IP:-pending}"
log ""
log "  Next step: deploy amux-server.py"
log "    scp amux-server.py root@$${TS_HOSTNAME:-amux-cloud}:/opt/amux/amux-server.py"
log "    ssh root@$${TS_HOSTNAME:-amux-cloud} systemctl start amux"
log ""
log "  Dashboard: https://$${TS_HOSTNAME:-amux-cloud}:8822"
