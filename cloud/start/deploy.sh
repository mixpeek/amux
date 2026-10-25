#!/usr/bin/env bash
# Deploy the amux Cloud signup function (cloud/start/worker.mjs) to Cloudflare Workers.
#
#   cloud/start/deploy.sh live    # amux-cloud at https://cloud.amux.io, live Stripe
#   cloud/start/deploy.sh test    # amux-cloud-test at https://amux-cloud-test.<sub>.workers.dev, Stripe test mode
#
# Reads from ~/.amux/server.env: AMUX_CLOUD_WORKER_TOKEN (Workers + DNS on amux.io), HCLOUD_TOKEN,
# STRIPE_LIVE_SECRET_KEY / STRIPE_TEST_SECRET_KEY. Keeps generated secrets in ~/.amux/cloud (600):
# the Stripe webhook signing secret per mode and the test mode's key. Gmail as hello@amux.io comes
# from ~/.amux/gmail-tokens. No secret is printed.
set -euo pipefail
MODE="${1:?usage: deploy.sh live|test}"
HERE="$(cd "$(dirname "$0")" && pwd)"
envv() { grep -E "^(export )?$1=" "$HOME/.amux/server.env" | head -1 | cut -d= -f2- | tr -d '"' || true; }
log() { echo "[start-deploy] $*"; }
CF="$(envv AMUX_CLOUD_WORKER_TOKEN)"; HC="$(envv HCLOUD_TOKEN)"
[[ -n "$CF" && -n "$HC" ]] || { log "FATAL: AMUX_CLOUD_WORKER_TOKEN and HCLOUD_TOKEN must be in server.env"; exit 1; }
cfapi() { curl -fsS -H "Authorization: Bearer $CF" "$@"; }
read -r ZID ACCT < <(cfapi "https://api.cloudflare.com/client/v4/zones?name=amux.io" | python3 -c 'import json,sys;z=json.load(sys.stdin)["result"][0];print(z["id"],z["account"]["id"])')
SUB=$(cfapi "https://api.cloudflare.com/client/v4/accounts/$ACCT/workers/subdomain" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["subdomain"])')
S="$HOME/.amux/cloud"; mkdir -p "$S"; chmod 700 "$S"
if [[ "$MODE" == live ]]; then
  NAME=amux-cloud; KEY="$(envv STRIPE_LIVE_SECRET_KEY)"; URL="https://cloud.amux.io"
  [[ "$KEY" == sk_live_* || "$KEY" == rk_live_* ]] || { log "FATAL: STRIPE_LIVE_SECRET_KEY missing"; exit 1; }
else
  NAME=amux-cloud-test; KEY="$(envv STRIPE_TEST_SECRET_KEY)"; URL="https://amux-cloud-test.$SUB.workers.dev"
  [[ "$KEY" == sk_test_* || "$KEY" == rk_test_* ]] || { log "FATAL: STRIPE_TEST_SECRET_KEY missing"; exit 1; }
  [[ -s "$S/start-test-key" ]] || { python3 -c 'import secrets;print(secrets.token_urlsafe(24))' > "$S/start-test-key"; chmod 600 "$S/start-test-key"; }
fi
# Stripe: products, prices, the FIRSTMONTH code, and this function's webhook (secret shown once).
WF="$S/stripe-$MODE-webhook-secret-worker"; PF="$S/stripe-$MODE-portal-config"
NEW=$(STRIPE_TEST_SECRET_KEY="$KEY" STRIPE_LIVE_SECRET_KEY="$KEY" python3 "$HERE/../control/stripe_setup.py" \
      --control "$URL" --portal-out "$PF" $([[ "$MODE" == live ]] && echo --live) )
if [[ -n "$NEW" ]]; then (umask 077; printf '%s\n' "$NEW" > "$WF"); fi
if [[ ! -s "$WF" && "$MODE" == live && -s "$S/stripe-live-webhook-secret-cloud" ]]; then
  cp "$S/stripe-live-webhook-secret-cloud" "$WF"; chmod 600 "$WF"   # same URL the control plane registered
fi
[[ -s "$WF" ]] || { log "FATAL: the $MODE webhook exists but its secret is not in $WF"; exit 1; }
GOLDEN=$(curl -fsS -H "Authorization: Bearer $HC" 'https://api.hetzner.cloud/v1/images?type=snapshot&label_selector=amux-golden&sort=created:desc' | python3 -c 'import json,sys;print(json.load(sys.stdin)["images"][0]["id"])')
META=$(mktemp); trap 'rm -f "$META"' EXIT; chmod 600 "$META"
APPLE_BUNDLE_ID="$(envv APPLE_BUNDLE_ID)" POSTHOG_KEY="$(envv POSTHOG_PERSONAL_API_KEY)" python3 - "$META" "$MODE" "$URL" "$KEY" "$(cat "$WF")" "$HC" "$CF" "$ZID" "$GOLDEN" "$HERE/../control/plans.json" "$S" "$(cat "$PF")" <<'EOF'
import base64, json, os, sys
meta, mode, url, key, whs, hc, cf, zid, golden, plans, s, portal = sys.argv[1:]
gt = os.path.expanduser("~/.amux/gmail-tokens/hello@amux.io.json")
b = [{"type": "secret_text", "name": "STRIPE_SECRET_KEY", "text": key},
     {"type": "secret_text", "name": "STRIPE_WEBHOOK_SECRET", "text": whs},
     {"type": "secret_text", "name": "HCLOUD_TOKEN", "text": hc},
     {"type": "secret_text", "name": "CF_TOKEN", "text": cf},
     {"type": "plain_text", "name": "CF_ZONE_ID", "text": zid},
     {"type": "plain_text", "name": "MODE", "text": mode},
     {"type": "plain_text", "name": "SELF_URL", "text": url},
     {"type": "plain_text", "name": "GOLDEN_IMAGE", "text": golden},
     {"type": "plain_text", "name": "PORTAL_CONFIG", "text": portal},
     {"type": "plain_text", "name": "PLANS", "text": json.dumps(json.load(open(plans)))}]
# Mail: the service account key (domain-wide delegation, gmail.send as hello@amux.io) never expires.
# The hello@amux.io user grant is the fallback; the amux.io session policy ends it within a day.
sa = os.path.join(s, "gmail-sa.json")
if os.path.exists(sa):
    b.append({"type": "secret_text", "name": "GMAIL_SA", "text": base64.b64encode(open(sa, "rb").read()).decode()})
if os.path.exists(gt):
    b.append({"type": "secret_text", "name": "GMAIL_OAUTH", "text": base64.b64encode(open(gt, "rb").read()).decode()})
if not (os.path.exists(sa) or os.path.exists(gt)):
    print("WARN no mail credential: billing, deletion and operator emails will fail", file=sys.stderr)
# Sign in with Apple from the iOS app: the Worker checks Apple's token against the app's bundle id
# and signs each server's two-minute sign-in link with a key derived from DOOR_ENTRY_SECRET.
de = os.path.join(s, f"door-entry-secret-{mode}")
if not os.path.exists(de):
    import secrets
    fd = os.open(de, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.write(fd, secrets.token_hex(32).encode()); os.close(fd)
b.append({"type": "secret_text", "name": "DOOR_ENTRY_SECRET", "text": open(de).read().strip()})
bundle = os.environ.get("APPLE_BUNDLE_ID", "")
if bundle:
    b.append({"type": "plain_text", "name": "APPLE_BUNDLE_ID", "text": bundle})
else:
    print("WARN APPLE_BUNDLE_ID unset: the iOS app's Sign in with Apple will be refused", file=sys.stderr)
# The operator audit: its report endpoint's key, and PostHog for the signup funnel (one HogQL query a day).
ok = os.path.join(s, f"ops-key-{mode}")
if not os.path.exists(ok):
    import secrets
    fd = os.open(ok, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.write(fd, secrets.token_urlsafe(24).encode()); os.close(fd)
b.append({"type": "secret_text", "name": "OPS_KEY", "text": open(ok).read().strip()})
if os.environ.get("POSTHOG_KEY"):
    b.append({"type": "secret_text", "name": "POSTHOG_KEY", "text": os.environ["POSTHOG_KEY"]})
    b.append({"type": "plain_text", "name": "POSTHOG_PROJECT", "text": "378145"})   # PostHog project "amux"
# Activation emails to customers (no model after a day, no worker after three). Off unless the deploy
# runs with NUDGES=on, because a deploy replaces every binding: say it each time you mean it.
b.append({"type": "plain_text", "name": "NUDGES", "text": "on" if os.environ.get("NUDGES") == "on" else "off"})
if mode == "test":
    b.append({"type": "secret_text", "name": "TEST_KEY", "text": open(os.path.join(s, "start-test-key")).read().strip()})
json.dump({"main_module": "worker.mjs", "compatibility_date": "2026-10-01", "bindings": b,
           "observability": {"enabled": True}}, open(meta, "w"))
EOF
A="https://api.cloudflare.com/client/v4/accounts/$ACCT/workers/scripts/$NAME"
cfapi -X PUT "$A" -F "metadata=@$META;type=application/json" -F "worker.mjs=@$HERE/worker.mjs;type=application/javascript+module" >/dev/null
log "uploaded $NAME (golden image $GOLDEN)"
cfapi -X PUT -H 'Content-Type: application/json' "$A/schedules" -d '[{"cron":"* * * * *"}]' >/dev/null
cfapi -X POST -H 'Content-Type: application/json' "$A/subdomain" -d '{"enabled":true}' >/dev/null
if [[ "$MODE" == live ]]; then
  # cloud.amux.io belongs to this function. A plain DNS record on the name (the old control plane)
  # blocks the custom domain, so it goes first. The custom domain's own record is read-only and
  # Cloudflare refuses to delete it with a 400, which failed every redeploy after the first.
  for rid in $(cfapi "https://api.cloudflare.com/client/v4/zones/$ZID/dns_records?name=cloud.amux.io" | python3 -c 'import json,sys;print(" ".join(r["id"] for r in json.load(sys.stdin)["result"] if r["type"] in ("A","AAAA","CNAME") and not (r.get("meta") or {}).get("read_only")))'); do
    cfapi -X DELETE "https://api.cloudflare.com/client/v4/zones/$ZID/dns_records/$rid" >/dev/null; log "removed DNS record $rid for cloud.amux.io"
  done
  # A few tries before failing the whole deploy over one API error.
  for try in 1 2 3 4 5; do
    cfapi -X PUT -H 'Content-Type: application/json' "https://api.cloudflare.com/client/v4/accounts/$ACCT/workers/domains" \
      -d "{\"hostname\":\"cloud.amux.io\",\"service\":\"$NAME\",\"zone_id\":\"$ZID\",\"environment\":\"production\"}" >/dev/null && break
    [[ $try == 5 ]] && { log "FATAL: custom domain cloud.amux.io not attached after 5 tries"; exit 1; }
    log "custom domain PUT failed (try $try), retrying"; sleep $((try * 5))
  done
fi
for _ in $(seq 1 30); do curl -fsS "$URL/health" >/dev/null 2>&1 && break; sleep 3; done
log "health: $(curl -fsS "$URL/health" || echo UNREACHABLE)"
log "url: $URL"
