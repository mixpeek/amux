#!/usr/bin/env bash
# The household demo instance for the intent-first persona runs and live specs
# (docs/design/intent-first.md). It runs the LIVE server binary (the build
# /health reports) on its own AMUX_HOME holding a seeded family (the
# e2e/intent/household.mjs mailbox and calendar), with the real helper model
# and real chat helpers, so Dana meets real answers without ever touching
# Ethan's own mail, workers or Messages.
#
#   scripts/intent-household.sh start   # prints the URL
#   scripts/intent-household.sh stop
#   scripts/intent-household.sh reset   # stop, wipe the home, start fresh
#
# Safety:
# - The mailbox is a file mailbox: amux cannot send from it.
# - ANTHROPIC_API_KEY is cleared, so the helper model runs on the signed-in
#   Claude subscription (the CLI path), never a billed API key.
# - CC_HOME and AMUX_HOME both point at the demo home, so a helper's
#   `amux get ...` reads this instance, not the owner's server.
# - TMUX is unset with a private TMUX_TMPDIR: it never reaches the fleet's panes.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
HOME_DIR="${AMUX_HOUSEHOLD_HOME:-$HOME/.amux/tmp/${AMUX_SESSION:-intent}/household}"
PORT="${AMUX_HOUSEHOLD_PORT:-18950}"
BIN="${AMUX_HOUSEHOLD_BIN:-$HOME/.local/bin/amux-server-rs}"
PIDF="$HOME_DIR/server.pid"
URL="https://localhost:$PORT"

up() { curl -sk --max-time 3 "$URL/health" >/dev/null 2>&1; }

stop() {
  if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
    kill "$(cat "$PIDF")"
    for _ in $(seq 1 40); do up || break; sleep 0.25; done
  fi
  rm -f "$PIDF"
}

seed() {
  mkdir -p "$HOME_DIR/mailboxes" "$HOME_DIR/tmux"
  : > "$HOME_DIR/server.env"
  node --input-type=module -e "
    import { buildHousehold } from '$REPO/e2e/intent/household.mjs';
    import fs from 'fs';
    const h = buildHousehold();
    fs.writeFileSync('$HOME_DIR/mailboxes/household.json', JSON.stringify(h.mailbox, null, 1));
    fs.writeFileSync('$HOME_DIR/household-events.json', JSON.stringify(h.events));
  "
}

start() {
  if up; then echo "$URL"; return 0; fi
  [ -f "$HOME_DIR/mailboxes/household.json" ] || seed
  env -u TMUX -u ANTHROPIC_API_KEY -u AMUX_URL -u AMUX_API -u AMUX_SESSION \
    AMUX_HOME="$HOME_DIR" CC_HOME="$HOME_DIR" AMUX_RS_PORT="$PORT" AMUX_RS_BIND=127.0.0.1 \
    AMUX_NO_SELF_ADOPT=1 TMUX_TMPDIR="$HOME_DIR/tmux" AMUX_VAULT_KEYSTORE=file \
    ${AMUX_DASHBOARD_DIR:+AMUX_DASHBOARD_DIR="$AMUX_DASHBOARD_DIR"} \
    nohup "$BIN" >"$HOME_DIR/server.log" 2>&1 &
  echo $! > "$PIDF"
  for _ in $(seq 1 120); do up && break; sleep 0.5; done
  up || { echo "household server did not come up; see $HOME_DIR/server.log" >&2; exit 1; }
  # The family calendar, once.
  have=$(curl -sk "$URL/api/cal-events" || echo '[]')
  node -e "
    const have = JSON.parse(process.argv[1]); const titles = new Set((Array.isArray(have) ? have : have.events || []).map(e => e.title));
    const ev = JSON.parse(require('fs').readFileSync('$HOME_DIR/household-events.json', 'utf8'));
    (async () => { for (const e of ev) if (!titles.has(e.title))
      await fetch('$URL/api/cal-events', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(e) }); })();
  " "$have" 2>/dev/null || NODE_TLS_REJECT_UNAUTHORIZED=0 true
  echo "$URL"
}

case "${1:-start}" in
  start) start ;;
  stop) stop ;;
  reset) stop; rm -rf "$HOME_DIR"; start ;;
  *) echo "usage: $0 start|stop|reset" >&2; exit 2 ;;
esac
