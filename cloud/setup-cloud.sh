#!/bin/bash
# amux cloud multi-tenant bootstrap — Hetzner Ubuntu 22.04
# Run as root after SSH-ing into the new server.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

log() { echo "[$(date '+%H:%M:%S')] $*"; }
log "=== amux cloud setup ==="

# ── Deps ──────────────────────────────────────────────────────────────────────
apt-get update -qq
apt-get install -y -qq curl git python3 python3-pip nginx certbot python3-certbot-nginx

# ── Docker ────────────────────────────────────────────────────────────────────
if ! command -v docker &>/dev/null; then
  log "Installing Docker..."
  curl -fsSL https://get.docker.com | sh
fi
systemctl enable --now docker

# ── Python deps for gateway ───────────────────────────────────────────────────
pip3 install -q "PyJWT[crypto]" cryptography

# ── Directories ───────────────────────────────────────────────────────────────
mkdir -p /var/amux/users /opt/amux/cloud

# ── Clone / copy amux repo ────────────────────────────────────────────────────
if [ ! -d /opt/amux/.git ]; then
  git clone https://github.com/mixpeek/amux.git /opt/amux
else
  git -C /opt/amux pull --ff-only
fi

# ── Build and push Docker image ───────────────────────────────────────────────
log "Building amux Docker image..."
cp /opt/amux/amux-server.py /opt/amux/cloud/docker/
docker build -t ghcr.io/mixpeek/amux:latest /opt/amux/cloud/docker/

# ── Gateway env ───────────────────────────────────────────────────────────────
mkdir -p /etc/amux
if [ ! -f /etc/amux/gateway.env ]; then
  cat > /etc/amux/gateway.env << 'EOF'
CLERK_PUBLISHABLE_KEY=pk_test_cmVzb2x2ZWQtY3Jvdy00OS5jbGVyay5hY2NvdW50cy5kZXYk
CLERK_SECRET_KEY=sk_test_Hk4eWnixlC1W3U1tHMqu1TeUNfO8tRxPrOtNKM0AOQ
R2_ACCESS_KEY=5256526335d2ee72f4995ba580f5f3fb
R2_SECRET_KEY=4f5e01fe0418108caa3b3ef5a4497b04d0759964edeaf80a6b09c229a1566c3c
CF_ACCOUNT_ID=4507e1d25a7f5ebec509c3e4d4b39074
GATEWAY_PORT=8080
AMUX_CLOUD_DATA=/var/amux/users
GATEWAY_DB=/var/amux/gateway.db
IDLE_TIMEOUT=600
EOF
fi

# ── Gateway systemd service ───────────────────────────────────────────────────
cat > /etc/systemd/system/amux-gateway.service << 'EOF'
[Unit]
Description=amux cloud gateway
After=network.target docker.service

[Service]
Type=simple
WorkingDirectory=/opt/amux/cloud/gateway
EnvironmentFile=/etc/amux/gateway.env
ExecStart=/usr/bin/python3 /opt/amux/cloud/gateway/gateway.py
Restart=always
RestartSec=3
StandardOutput=append:/var/log/amux-gateway.log
StandardError=append:/var/log/amux-gateway.log

[Install]
WantedBy=multi-user.target
EOF

# ── nginx ─────────────────────────────────────────────────────────────────────
cp /opt/amux/cloud/gateway/nginx.conf /etc/nginx/sites-available/amux-cloud
ln -sf /etc/nginx/sites-available/amux-cloud /etc/nginx/sites-enabled/amux-cloud
rm -f /etc/nginx/sites-enabled/default

# Start nginx with HTTP only first (certbot needs it to verify domain)
sed -i 's/listen 443 ssl;//; s/ssl_certificate.*//; s/ssl_protocols.*//; s/ssl_ciphers.*//' \
  /etc/nginx/sites-available/amux-cloud 2>/dev/null || true
nginx -t && systemctl reload nginx

# ── TLS — run after DNS A record points here ──────────────────────────────────
SERVER_IP=$(curl -s ifconfig.me)
log ""
log "=== Setup complete ==="
log "Server IP: $SERVER_IP"
log ""
log "Next steps:"
log "  1. Add DNS A record:  cloud.amux.io → $SERVER_IP"
log "  2. Once DNS propagates, run:"
log "     certbot --nginx -d cloud.amux.io --non-interactive --agree-tos -m hello@mixpeek.com"
log "  3. Then start the gateway:"
log "     systemctl daemon-reload && systemctl enable --now amux-gateway"
log ""
log "  Check logs: tail -f /var/log/amux-gateway.log"

systemctl daemon-reload
systemctl enable amux-gateway
