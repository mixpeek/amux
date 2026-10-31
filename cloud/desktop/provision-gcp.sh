#!/usr/bin/env bash
# Create a dedicated amux host on Google Cloud and install it: static IP, VM, web firewall, a
# Cloudflare DNS record, then cloud/desktop/bootstrap.sh with every setting from one env file.
#
#   bash cloud/desktop/provision-gcp.sh my-box.env
#
# my-box.env (KEY=value lines; values never appear on a command line or in output):
#   AMUX_DOMAIN=dev.example.com          the name Caddy gets a certificate for (required)
#   GCP_PROJECT=my-project               required
#   GCP_ZONE=us-east1-b  GCP_MACHINE=e2-standard-4  GCP_DISK_GB=200   (defaults shown)
#   GCP_ACCOUNT=sa@project.iam.gserviceaccount.com   gcloud account to use (optional)
#   GCP_WEB_TAG=amux-web                 network tag that opens 80/443; created when allowed
#   CLOUDFLARE_API_TOKEN=... CLOUDFLARE_ZONE_ID=...   DNS, proxied through Cloudflare, and the host
#                           then admits web traffic from Cloudflare only (AMUX_BEHIND_CLOUDFLARE=0 opts out)
#   SSH_PUBKEY_FILE=~/.ssh/id_ed25519.pub  SSH_USER=<you>   who can SSH in (defaults: yours)
#   ...plus any bootstrap.sh setting: AMUX_WEB_USERS, CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_API_KEY,
#   AMUX_UPDATES=source, AMUX_UPDATE_FOLLOW_MIN, AMUX_GITHUB_REPOS, GITHUB_TOKEN, AMUX_GIT_NAME,
#   AMUX_GIT_EMAIL, AMUX_CLAUDE_ROTATE=1, TS_AUTHKEY, SWAP_GB.
#
# Written from building mixpeek-dev.amux.io by hand (2026-10-31). Idempotent: an existing
# address, VM, rule or record is reused, so a failed run can be re-run.
set -euo pipefail
ENV_IN="${1:?usage: provision-gcp.sh <env file>}"
[[ -f "$ENV_IN" ]] || { echo "no such env file: $ENV_IN" >&2; exit 2; }
set -a; . "$ENV_IN"; set +a
: "${AMUX_DOMAIN:?AMUX_DOMAIN is required}" "${GCP_PROJECT:?GCP_PROJECT is required}"
ZONE="${GCP_ZONE:-us-east1-b}"; REGION="${ZONE%-*}"
MACHINE="${GCP_MACHINE:-e2-standard-4}"; DISK="${GCP_DISK_GB:-200}"
NAME="${GCP_INSTANCE:-${AMUX_DOMAIN%%.*}}"; TAG="${GCP_WEB_TAG:-amux-web}"
SSH_USER="${SSH_USER:-$(id -un)}"; PUB="${SSH_PUBKEY_FILE:-$HOME/.ssh/id_ed25519.pub}"
G=(gcloud --project="$GCP_PROJECT" --quiet); [[ -n "${GCP_ACCOUNT:-}" ]] && G+=(--account="$GCP_ACCOUNT")
here="$(cd "$(dirname "$0")" && pwd)"
# Behind Cloudflare by default whenever this script manages the DNS record.
BEHIND_CF=0; [[ -n "${CLOUDFLARE_API_TOKEN:-}" && -n "${CLOUDFLARE_ZONE_ID:-}" && "${AMUX_BEHIND_CLOUDFLARE:-1}" == 1 ]] && BEHIND_CF=1
WEB_SRC=0.0.0.0/0; [[ $BEHIND_CF == 1 ]] && WEB_SRC=$(curl -fsS https://www.cloudflare.com/ips-v4 | paste -sd, -)
say() { echo "[provision-gcp] $*"; }

# 1. Static IP
if ! "${G[@]}" compute addresses describe "$NAME-ip" --region="$REGION" >/dev/null 2>&1; then
  "${G[@]}" compute addresses create "$NAME-ip" --region="$REGION" >/dev/null
fi
IP=$("${G[@]}" compute addresses describe "$NAME-ip" --region="$REGION" --format='value(address)')
say "address $NAME-ip: $IP"

# 2. Web firewall: a rule opening 80/443 to the tag. Creating it needs compute.firewalls.create;
#    without it, name an existing tag that already opens them in GCP_WEB_TAG.
if ! "${G[@]}" compute firewall-rules list --format='value(targetTags.list())' | tr ',;' '\n\n' | grep -qx "$TAG"; then
  if "${G[@]}" compute firewall-rules create "$TAG-80-443" --allow=tcp:80,tcp:443,tcp:8443 --target-tags="$TAG" \
       --source-ranges="$WEB_SRC" --description="amux hosts: web (Cloudflare only when proxied)" >/dev/null 2>&1; then
    say "firewall rule $TAG-80-443 created"
  else
    echo "no firewall rule opens 80/443 for tag '$TAG' and this account cannot create one;" \
         "set GCP_WEB_TAG to a tag that already does" >&2; exit 3
  fi
fi

# 3. VM: no service account (the box needs no GCP API access), the caller's SSH key.
if ! "${G[@]}" compute instances describe "$NAME" --zone="$ZONE" >/dev/null 2>&1; then
  KEYS=$(mktemp); trap 'rm -f "$KEYS"' EXIT
  printf '%s:%s\n' "$SSH_USER" "$(cat "$PUB")" > "$KEYS"
  "${G[@]}" compute instances create "$NAME" --zone="$ZONE" --machine-type="$MACHINE" \
    --image-family=ubuntu-2404-lts-amd64 --image-project=ubuntu-os-cloud \
    --boot-disk-size="${DISK}GB" --boot-disk-type=pd-balanced --address="$IP" --tags="$TAG" \
    --no-service-account --no-scopes --labels=managed-by=amux \
    --metadata-from-file=ssh-keys="$KEYS" >/dev/null
  say "instance $NAME created ($MACHINE, ${DISK}GB, $ZONE)"
else
  say "instance $NAME exists"
fi

# 4. DNS: an A record, proxied. Caddy still gets its certificate from Let's Encrypt: Cloudflare
#    passes plain-HTTP challenge requests through to the host (checked on mixpeek-dev, 2026-10-31).
if [[ -n "${CLOUDFLARE_API_TOKEN:-}" && -n "${CLOUDFLARE_ZONE_ID:-}" ]]; then
  CF_DOMAIN="$AMUX_DOMAIN" CF_IP="$IP" CF_PROXIED=$BEHIND_CF python3 - <<'EOF'
import json, os, urllib.request
tok, zone, name, ip = os.environ["CLOUDFLARE_API_TOKEN"], os.environ["CLOUDFLARE_ZONE_ID"], os.environ["CF_DOMAIN"], os.environ["CF_IP"]
def call(method, path, body=None):
    r = urllib.request.Request("https://api.cloudflare.com/client/v4" + path, method=method,
        data=json.dumps(body).encode() if body else None,
        headers={"Authorization": "Bearer " + tok, "Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(r, timeout=30))
proxied = os.environ["CF_PROXIED"] == "1"
rec = {"type": "A", "name": name, "content": ip, "ttl": 1 if proxied else 120, "proxied": proxied}
have = call("GET", f"/zones/{zone}/dns_records?type=A&name={name}")["result"]
if have and have[0]["content"] == ip and have[0]["proxied"] == proxied: print(f"[provision-gcp] dns {name} -> {ip} (exists)")
elif have: call("PUT", f"/zones/{zone}/dns_records/{have[0]['id']}", rec); print(f"[provision-gcp] dns {name} -> {ip} (updated)")
else: call("POST", f"/zones/{zone}/dns_records", rec); print(f"[provision-gcp] dns {name} -> {ip} (created)")
EOF
else
  say "no Cloudflare credentials: point an A record for $AMUX_DOMAIN at $IP before the certificate step"
fi

# 5. Wait for SSH, then bootstrap with the env file (copied root-only; never on a command line).
SSH=(ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 "$SSH_USER@$IP")
for _ in $(seq 1 60); do "${SSH[@]}" true 2>/dev/null && break; sleep 5; done
"${SSH[@]}" 'sudo install -d -m 700 /root/amux-setup'
{ grep -vE '^(GCP_|CLOUDFLARE_|SSH_USER=|SSH_PUBKEY_FILE=|AMUX_BEHIND_CLOUDFLARE=)' "$ENV_IN"; echo "AMUX_BEHIND_CLOUDFLARE=$BEHIND_CF"; } | "${SSH[@]}" 'sudo tee /root/amux-setup/bootstrap.env >/dev/null && sudo chmod 600 /root/amux-setup/bootstrap.env'
"${SSH[@]}" 'sudo tee /root/amux-setup/bootstrap.sh >/dev/null' < "$here/bootstrap.sh"
"${SSH[@]}" 'sudo bash -c "cd /root/amux-setup && nohup bash -c \"set -a; . ./bootstrap.env; set +a; bash ./bootstrap.sh\" > run.log 2>&1 &"'
say "bootstrap started on $IP; follow it with: ssh $SSH_USER@$IP sudo tail -f /root/amux-setup/run.log"
say "then: https://$AMUX_DOMAIN (web users from AMUX_WEB_USERS), ssh $SSH_USER@$IP"
