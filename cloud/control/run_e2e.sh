#!/usr/bin/env bash
# Start the control plane in fake mode on a throwaway DB and drive the journey in Chromium.
# Needs node with the playwright package resolvable (run from a checkout with node_modules).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d)"
PORT="${CONTROL_E2E_PORT:-18990}"
CONTROL_FAKE=1 CONTROL_DB="$TMP/control.db" CONTROL_PORT="$PORT" CONTROL_PUBLIC_URL="http://127.0.0.1:$PORT" \
  AMUX_SITE_URL="http://127.0.0.1:18977" CONTROL_ADMIN_TOKEN=adm python3 "$HERE/app.py" > "$TMP/control.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:$PORT/health" >/dev/null && break; sleep 0.1; done
node "$HERE/e2e_fake_journey.mjs" "$TMP/control.db" "$TMP/progress.png"
if grep -q Traceback "$TMP/control.log"; then echo "FAIL control plane logged a traceback:"; grep -A12 Traceback "$TMP/control.log"; exit 1; fi
