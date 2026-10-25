#!/usr/bin/env bash
# AF-542. `amux board reviewer <id> <session>` and `amux board shepherd <id>
# <session>` handed a card to a named peer with no check that the peer could
# ever act on it. Measured 2026-09-07: 21 of 24 named reviewers on the real
# board were unreachable by construction (isolated or gone) -- each one a
# card silently stuck in `review` forever, since only that named session's
# sign-off satisfies the gate (ethos rule 3: no truthful path through for
# that name).
#
# This pins the fix: a nonexistent session is refused outright, an isolated
# one is refused unless --force, an archived-but-not-isolated one is set with
# a warning, and a lookup failure warns but still proceeds (fail-open, same
# direction as push-consent.sh's lane_state -- a probe that cannot run must
# never manufacture a false "clear", ethos rule 4).
#
# WHAT IS PINNED, both directions: the refusals must not PATCH the board, and
# every accept (ok / --force / archived / lookup-failure) must still reach
# the wire with the right reviewer/shepherd value.
#
# Runs against a throwaway listener on a random port. Nothing here can touch
# the real board.
set -euo pipefail
cd "$(dirname "$0")/.."
AMUX_BIN="${AMUX_BIN:-./amux}"
# An empty CC_HOME: with the real one, the CLI rewrites this stub's localhost
# URL to the live server (_amux_heal_url reads ~/.amux/endpoint.json), so a
# local run wrote to the real board.
CLI_HOME=$(mktemp -d)
PASS=0; FAIL=0
ok(){ echo "  ok   $1"; PASS=$((PASS+1)); }
bad(){ echo "  FAIL $1"; echo "       $2"; FAIL=$((FAIL+1)); }

CAP=$(mktemp); PORTF=$(mktemp)
trap 'kill $LPID 2>/dev/null; rm -rf "$CAP" "$PORTF" "$CLI_HOME"' EXIT

# Serves until killed. GET /api/sessions/<name> answers by name so one
# listener covers every state; PATCH /api/board/<id> APPENDS the body so a
# run making more than one request can't silently drop the one under test.
python3 - "$CAP" "$PORTF" <<'PY' &
import sys, json, http.server, socketserver
cap, portf = sys.argv[1], sys.argv[2]

SESSIONS = {
    "ok-session": {"name": "ok-session", "isolated": False, "archived": False},
    "isolated-session": {"name": "isolated-session", "isolated": True, "archived": False},
    "archived-session": {"name": "archived-session", "isolated": False, "archived": True},
}

class H(http.server.BaseHTTPRequestHandler):
    def _json(self, code, body):
        payload = json.dumps(body).encode()
        self.send_response(code); self.send_header('Content-Type', 'application/json')
        self.end_headers(); self.wfile.write(payload)
    def do_GET(self):
        if self.path.startswith("/api/sessions/"):
            name = self.path.rsplit("/", 1)[-1]
            if name in SESSIONS:
                self._json(200, SESSIONS[name])
            else:
                self._json(404, {"error": f"session '{name}' not found"})
            return
        self._json(200, {"ok": True})
    def do_PATCH(self):
        n = int(self.headers.get('Content-Length') or 0)
        with open(cap, 'ab') as f:
            f.write(self.rfile.read(n) + b'\n')
        self._json(200, {"ok": True, "id": "TEST-1", "status": "review"})
    def log_message(self, *a): pass

class S(socketserver.TCPServer):
    allow_reuse_address = True

with S(("127.0.0.1", 0), H) as s:
    open(portf, "w").write(str(s.server_address[1]))
    s.serve_forever()
PY
LPID=$!
disown $LPID 2>/dev/null || true
for _ in $(seq 1 50); do [ -s "$PORTF" ] && break; sleep 0.1; done
PORT=$(cat "$PORTF")
[ -n "${PORT:-}" ] || { echo "listener never bound"; exit 1; }

run() {  # run <verb> <args...> -> sets RC and OUT, leaves PATCH bodies in $CAP
  : > "$CAP"
  if OUT=$(timeout 20 env -u AMUX_WORKER CC_HOME="$CLI_HOME" AMUX_API="http://127.0.0.1:$PORT" AMUX_SESSION=guard-test \
      bash "$AMUX_BIN" board "$@" 2>&1); then RC=0; else RC=$?; fi
}
patched() { [ -s "$CAP" ]; }
patch_body() { cat "$CAP" 2>/dev/null; }

for verb in reviewer shepherd; do
  field="reviewer"; [ "$verb" = shepherd ] && field="shepherd"

  run "$verb" TEST-1 ok-session
  if [ "$RC" -eq 0 ] && patched && grep -q "\"$field\": *\"ok-session\"\|\"$field\":\"ok-session\"" "$CAP"; then
    ok "$verb: a reachable session is set with no fuss"
  else
    bad "$verb: a reachable session is set with no fuss" "rc=$RC body=$(patch_body) out=$OUT"
  fi

  run "$verb" TEST-1 missing-session
  if [ "$RC" -ne 0 ] && ! patched; then
    ok "$verb: a nonexistent session is refused, nothing PATCHed"
  else
    bad "$verb: a nonexistent session is refused, nothing PATCHed" "rc=$RC body=$(patch_body) out=$OUT"
  fi

  run "$verb" TEST-1 isolated-session
  if [ "$RC" -ne 0 ] && ! patched; then
    ok "$verb: an isolated session is refused by default, nothing PATCHed"
  else
    bad "$verb: an isolated session is refused by default, nothing PATCHed" "rc=$RC body=$(patch_body) out=$OUT"
  fi

  run "$verb" TEST-1 isolated-session --force
  if [ "$RC" -eq 0 ] && patched && [[ "$OUT" == *isolated* ]]; then
    ok "$verb: --force overrides the isolated refusal and still warns"
  else
    bad "$verb: --force overrides the isolated refusal and still warns" "rc=$RC body=$(patch_body) out=$OUT"
  fi

  run "$verb" TEST-1 archived-session
  if [ "$RC" -eq 0 ] && patched && [[ "$OUT" == *archived* ]]; then
    ok "$verb: archived-but-not-isolated is set, with a warning"
  else
    bad "$verb: archived-but-not-isolated is set, with a warning" "rc=$RC body=$(patch_body) out=$OUT"
  fi

  run "$verb" TEST-1 none
  if [ "$RC" -eq 0 ] && patched; then
    ok "$verb: clearing with 'none' still works and skips the lookup"
  else
    bad "$verb: clearing with 'none' still works and skips the lookup" "rc=$RC body=$(patch_body) out=$OUT"
  fi
done

# ── lookup failure fails OPEN, not shut (ethos rule 4: unknown != isolated) ──
# Point AMUX_API at a port nothing listens on so the session GET can't
# complete. The eventual PATCH will *also* fail against a dead board, so RC
# alone can't distinguish "guard refused" from "transport failed downstream" --
# the warning text is the assertion: it must say "could not confirm ...
# reachable" (unknown, fail-open), never claim the session IS isolated.
DEAD_PORT=1
OUT=$(timeout 5 env -u AMUX_WORKER CC_HOME="$CLI_HOME" AMUX_API="http://127.0.0.1:$DEAD_PORT" AMUX_SESSION=guard-test \
    bash "$AMUX_BIN" board reviewer TEST-1 some-unreachable-session 2>&1) && RC=0 || RC=$?
if [[ "$OUT" == *"could not confirm"*"reachable"*"proceeding anyway"* ]] && [[ "$OUT" != *"is isolated"* ]]; then
  ok "reviewer: a lookup failure warns as unknown and still attempts the write, never fabricates 'isolated'"
else
  bad "reviewer: a lookup failure warns as unknown and still attempts the write, never fabricates 'isolated'" "rc=$RC out=$OUT"
fi

# ── discriminating control: the pre-fix shape must NOT show this behaviour ──
git show origin/main:amux > /tmp/prefix-reviewer-$$.sh 2>/dev/null || cp "$AMUX_BIN" /tmp/prefix-reviewer-$$.sh
PREFIX_BIN=/tmp/prefix-reviewer-$$.sh
if grep -q "_board_assignee_guard" "$PREFIX_BIN"; then
  echo "  skip pre-fix control: origin/main already carries the fix"
else
  : > "$CAP"
  if OUT=$(timeout 20 env -u AMUX_WORKER CC_HOME="$CLI_HOME" AMUX_API="http://127.0.0.1:$PORT" AMUX_SESSION=guard-test \
      bash "$PREFIX_BIN" board reviewer TEST-1 isolated-session 2>&1); then RC=0; else RC=$?; fi
  if [ "$RC" -eq 0 ] && patched; then
    ok "control: pre-fix CLI sets an isolated reviewer with no refusal -- confirms the fix is load-bearing"
  else
    bad "control: pre-fix CLI sets an isolated reviewer with no refusal -- confirms the fix is load-bearing" \
      "expected the OLD code to proceed unguarded; got rc=$RC body=$(patch_body)"
  fi
fi
rm -f "$PREFIX_BIN"

echo "  ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
