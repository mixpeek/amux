#!/usr/bin/env bash
# MHC-377 — regression coverage for a silent data-loss bug in
# `amux board status-update`.
#
# THE BUG. The verb took `local text="$*"` with no flag guard, so a flag handed
# to it became the message:
#
#   amux board status-update MHC-376 --outcome-file /tmp/notes.txt
#     -> prints "MHC-376 -> status update posted", exits 0
#     -> records the literal string "--outcome-file /tmp/notes.txt"
#     -> the file's contents are never read and never sent
#
# Four writes were lost that way in one session, and twelve in an earlier one
# (MG-1441, 2026-08-13). Nothing in the output separates it from a real write,
# which is what makes it expensive: the ledger looks written-to and a reviewer
# believes it.
#
# The help invited it, printing "Every status verb above takes the
# gate-satisfying flags ... --outcome-file <path>" in a block listing this verb,
# when those flags belong only to the status-CHANGE verbs. Both halves are
# fixed; this pins the behaviour half.
#
# WHAT IS PINNED, and why each direction is here. A guard that only ever refuses
# is as broken as one that never does, so the accept cases carry equal weight:
# the refusals must not POST, and prose/--stdin/--file must reach the wire with
# their bytes intact.
#
# Runs against a throwaway listener on a random port. Nothing here can touch the
# real board.
set -eeuo pipefail
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

# Serves until killed and APPENDS every POST body, so a run that makes more than
# one request cannot silently drop the one being asserted on.
python3 - "$CAP" "$PORTF" <<'PY' &
import sys, json, http.server, socketserver
cap, portf = sys.argv[1], sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def _reply(self):
        self.send_response(200); self.send_header('Content-Type','application/json')
        self.end_headers(); self.wfile.write(b'{"ok":true,"id":"TEST-1","status":"todo"}')
    def do_POST(self):
        n = int(self.headers.get('Content-Length') or 0)
        with open(cap, 'ab') as f:
            f.write(self.rfile.read(n) + b'\n')
        self._reply()
    def do_GET(self):
        self._reply()
    def do_PATCH(self):
        self._reply()
    def log_message(self, *a): pass
class S(socketserver.TCPServer):
    allow_reuse_address = True
with S(("127.0.0.1", 0), H) as s:
    open(portf, "w").write(str(s.server_address[1]))
    s.serve_forever()
PY
LPID=$!
# Detach it, otherwise job control prints "Terminated: 15" plus the whole
# heredoc when the trap kills it, and a clean run ends looking like a crash.
disown $LPID 2>/dev/null || true
for _ in $(seq 1 50); do [ -s "$PORTF" ] && break; sleep 0.1; done
PORT=$(cat "$PORTF")
[ -n "${PORT:-}" ] || { echo "listener never bound"; exit 1; }

run_su() {  # run_su <args...>  -> sets RC, leaves POST bodies in $CAP
  : > "$CAP"
  # RC IS THE ASSERTION for several cells (a refused flag must exit non-zero),
  # so the status is captured through `if` rather than lost to `set -e`.
  # `|| true` would set RC=0 and silently pass every refusal cell (AF-562).
  if timeout 20 env -u AMUX_WORKER CC_HOME="$CLI_HOME" AMUX_API="http://127.0.0.1:$PORT" AMUX_SESSION=su-flag-test \
      bash "$AMUX_BIN" board status-update "$@" >/dev/null 2>&1; then RC=0; else RC=$?; fi
}
posted_text() {
  python3 -c "
import json,sys
out=[]
for line in open('$CAP','rb').read().splitlines():
    if not line.strip(): continue
    try: out.append(json.loads(line).get('text',''))
    except Exception: pass
print('\n'.join(out), end='')
"
}

echo "amux board status-update flag handling (MHC-377)"

# ── refusals: the data-loss shapes ──────────────────────────────────────────
for flag in --outcome-file --outcome-stdin --checked --ack; do
  run_su TEST-1 "$flag" /tmp/whatever
  BODY=$(posted_text)
  if [ "$RC" -eq 0 ]; then
    bad "$flag is refused" "exited 0; it should die rather than record the flag"
  elif [ -n "$BODY" ]; then
    bad "$flag is refused" "refused but still POSTed: $BODY"
  else
    ok "$flag is refused and nothing is POSTed"
  fi
done

# The precise regression: the flag string must never become the message.
run_su TEST-1 --outcome-file /tmp/whatever
case "$(posted_text)" in
  *"--outcome-file"*) bad "flag never becomes the message" "the flag string reached the wire" ;;
  *) ok "flag never becomes the message" ;;
esac

# ── accepts: all three input paths must reach the wire intact ───────────────
PAYLOAD='status with `backticks` and $(whoami) and ${HOME}'

printf '%s' "$PAYLOAD" > /tmp/su-flag-test-file.txt
run_su TEST-1 --file /tmp/su-flag-test-file.txt
[ "$(posted_text)" = "$PAYLOAD" ] \
  && ok "--file sends the file's bytes" \
  || bad "--file sends the file's bytes" "got: $(posted_text)"

: > "$CAP"
printf '%s' "$PAYLOAD" | timeout 20 env -u AMUX_WORKER CC_HOME="$CLI_HOME" AMUX_API="http://127.0.0.1:$PORT" AMUX_SESSION=su-flag-test \
  bash "$AMUX_BIN" board status-update TEST-1 --stdin >/dev/null 2>&1
[ "$(posted_text)" = "$PAYLOAD" ] \
  && ok "--stdin carries bytes literally" \
  || bad "--stdin carries bytes literally" "got: $(posted_text)"

run_su TEST-1 "ordinary positional prose"
[ "$(posted_text)" = "ordinary positional prose" ] \
  && ok "plain prose still works" \
  || bad "plain prose still works" "got: $(posted_text)"

run_su TEST-1 --file /tmp/definitely-not-here-$$
[ "$RC" -ne 0 ] && ok "--file with an unreadable path refuses" \
  || bad "--file with an unreadable path refuses" "exited 0"

rm -f /tmp/su-flag-test-file.txt
echo "  ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
