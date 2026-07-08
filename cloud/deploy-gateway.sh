#!/usr/bin/env bash
# Deploy the amux cloud GATEWAY (cloud/gateway/gateway.py) to the VM and restart it.
# deploy.sh only ships the per-user amux-server.py; this ships the host gateway.
#
# Usage:
#   ./deploy-gateway.sh                      # deploy gateway.py + restart
#   ./deploy-gateway.sh --mint-token <email> # also mint a tunnel token for that org's owner
#
# Requires: gcloud with an account that can `gcloud compute ssh amux-dev` via IAP.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
GATEWAY="$SCRIPT_DIR/gateway/gateway.py"
ZONE="us-central1-a"
VM="amux-dev"
IAP_USER="$(gcloud config get-value account 2>/dev/null | tr '@.' '_')"
SSH() { gcloud compute ssh "${IAP_USER}@${VM}" --zone="$ZONE" --tunnel-through-iap --quiet --command="$1"; }

log() { echo "→ $*"; }
[ -f "$GATEWAY" ] || { echo "gateway.py not found at $GATEWAY"; exit 1; }

log "Copying gateway.py to VM…"
gcloud compute scp "$GATEWAY" "${IAP_USER}@${VM}:/tmp/gateway.py" \
  --zone="$ZONE" --tunnel-through-iap --quiet

log "Discovering gateway install path + service…"
GW_PATH="$(SSH 'sudo find /opt /srv /root /home -maxdepth 4 -name gateway.py 2>/dev/null | head -1' | tr -d "\r")"
SVC="$(SSH 'systemctl list-units --type=service --all 2>/dev/null | grep -oiE "[a-z-]*gateway[a-z-]*\.service" | head -1' | tr -d "\r")"
[ -z "$GW_PATH" ] && { echo "could not locate gateway.py on the VM"; exit 1; }
log "  path=$GW_PATH  service=${SVC:-<none found>}"

log "Backing up + installing…"
SSH "sudo cp '$GW_PATH' '${GW_PATH}.bak.$(date +%s)' && sudo cp /tmp/gateway.py '$GW_PATH'"

if [ -n "$SVC" ]; then
  log "Restarting $SVC…"
  SSH "sudo systemctl restart '$SVC' && sleep 2 && sudo systemctl is-active '$SVC'"
else
  echo "⚠ No gateway service found — restart it manually (check how gateway.py is run)."
fi

log "Smoke test (tunnel routes should 402/404, not 500)…"
SSH "curl -s -o /dev/null -w 'POST /tunnel/register → %{http_code}\n' -X POST http://127.0.0.1:8080/tunnel/register || true"

if [ "${1:-}" = "--mint-token" ] && [ -n "${2:-}" ]; then
  EMAIL="$2"
  log "Minting tunnel token for owner email: $EMAIL"
  TOKEN="$(SSH "
    ORG=\$(sudo sqlite3 /var/amux/gateway.db \"SELECT o.id FROM orgs o JOIN users u ON u.id=o.owner_id WHERE u.email='$EMAIL' LIMIT 1;\")
    [ -z \"\$ORG\" ] && ORG=\$(sudo sqlite3 /var/amux/gateway.db \"SELECT id FROM orgs WHERE plan='pro' LIMIT 1;\")
    TOK=\$(python3 -c 'import secrets;print(secrets.token_urlsafe(24))')
    sudo sqlite3 /var/amux/gateway.db \"INSERT INTO tunnel_tokens (token,org_id,email,label,created_at) VALUES ('\$TOK','\$ORG','$EMAIL','cli',strftime('%s','now'));\"
    echo \$TOK
  " | tr -d '\r' | tail -1)"
  echo ""
  echo "  Tunnel token: $TOKEN"
  echo "  Configure locally:  echo 'AMUX_TUNNEL_TOKEN=$TOKEN' >> ~/.amux/server.env && touch amux-server.py"
fi

log "Done."
