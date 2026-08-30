#!/usr/bin/env bash
# AEAB-36 — a board write that never reached the server must SAY SO, and must
# never be confused with a write the server refused.
#
# The bug: `set -euo pipefail` (amux line 19) turned a transport failure into a
# silent abort. The outcome write is `curl | python3` whose python deliberately
# exits 0 so a lost outcome does not block the transition — `pipefail` returned
# curl's status instead, and `set -e` killed the script right there, AFTER the
# warning had printed. The transition write was worse: not piped at all, so
# `result=$(_board_transition ...)` aborted the caller and `amux board done <ID>`
# printed ABSOLUTELY NOTHING and exited 7.
#
# Observed live 2026-08-19 closing AEAB-34 while the server was restarting to
# adopt a build: one warning naming only the outcome, status silently unchanged.
# The natural reading — "status moved, prose lost" — was the exact opposite of
# what happened.
#
# ISOLATION: every case points AMUX_API and AMUX_URL at a dead port or a local
# stub. An earlier suite in this repo exported the wrong variable, reached the
# LIVE board and filed three junk cards while reporting green.
#
# Exit 0 = all pass, 1 = a failure. Wired into .github/workflows/checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
CLI="$(pwd)/amux"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"; [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null' EXIT
printf 'outcome text\n' > "$TMP/oc.md"

DEAD="http://127.0.0.1:9"          # discard port: refuses instantly
ok()  { PASS=$((PASS+1)); }
bad() { FAIL=$((FAIL+1)); echo "FAIL: $1"; echo "  got: ${2:-<empty>}"; }

# A stub board. $1 = the JSON body it returns for PATCH.
stub() {
  python3 - "$1" <<'PY' &
import http.server, sys, threading
body = sys.argv[1].encode()
class H(http.server.BaseHTTPRequestHandler):
    def do_PATCH(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", 8897), H).serve_forever()
PY
  STUB_PID=$!
  for _ in $(seq 1 40); do
    (exec 3<>/dev/tcp/127.0.0.1/8897) 2>/dev/null && { exec 3<&- 3>&-; return 0; }
    sleep 0.1
  done
  return 1
}

# --- (a) transition only, server unreachable -------------------------------
out=$(AMUX_API="$DEAD" AMUX_URL="$DEAD" bash "$CLI" board done TEST-1 --checked x 2>&1); rc=$?
case "$out" in *"cannot reach the board"*) ok ;; *) bad "(a) an unreachable server must be named, not silent" "$out";; esac
case "$out" in *"NOT applied"*) ok ;; *) bad "(a) it must say the transition did not happen" "$out";; esac
[ "$rc" -ne 0 ] && ok || bad "(a) must exit non-zero" "rc=$rc"

# --- (b) with an outcome, server unreachable -------------------------------
#     The incident's own shape. The old code printed a warning scoped to the
#     OUTCOME and died, so the reader concluded the status had moved.
out=$(AMUX_API="$DEAD" AMUX_URL="$DEAD" bash "$CLI" board done TEST-1 --checked x --outcome-stdin < "$TMP/oc.md" 2>&1); rc=$?
case "$out" in *"NOTHING was applied"*) ok ;; *) bad "(b) must state that NEITHER write landed" "$out";; esac
case "$out" in
  *"outcome NOT recorded — server sent no JSON"*)
     bad "(b) must not report this as an outcome-only problem" "$out";;
  *) ok ;;
esac
[ "$rc" -ne 0 ] && ok || bad "(b) must exit non-zero" "rc=$rc"

# --- (c) CONTROL: a reachable server still works ---------------------------
#     Without this, a CLI that always reported "cannot reach the board" passes
#     every case above while being completely broken.
if stub '{"id":"TEST-1","status":"done","ok":true}'; then
  out=$(AMUX_API="http://127.0.0.1:8897" AMUX_URL="http://127.0.0.1:8897" \
        bash "$CLI" board done TEST-1 --checked x 2>&1); rc=$?
  case "$out" in *"TEST-1"*) ok ;; *) bad "(c) a reachable server must still transition" "$out";; esac
  case "$out" in *"cannot reach"*) bad "(c) must not claim transport failure when reachable" "$out";; *) ok ;; esac
  [ "$rc" -eq 0 ] && ok || bad "(c) must exit 0 on success" "rc=$rc"
  kill "$STUB_PID" 2>/dev/null; wait "$STUB_PID" 2>/dev/null; STUB_PID=""
else
  bad "(c) stub server never came up" "harness"
fi

# --- (d) THE DISCRIMINATOR: a REFUSED gate is not a transport failure -------
#     This is the case that keeps the fix honest. "The board said no" and "the
#     message never arrived" need opposite responses — retry the second, never
#     the first — and a fix that shouted transport-failure at every error would
#     satisfy (a) and (b) while destroying that distinction.
if stub '{"ok":false,"error":"gate not acknowledged","item":"TEST-1","kind":"gate_blocked"}'; then
  out=$(AMUX_API="http://127.0.0.1:8897" AMUX_URL="http://127.0.0.1:8897" \
        bash "$CLI" board done TEST-1 2>&1)
  case "$out" in *"cannot reach"*) bad "(d) a refused gate must NOT read as a transport failure" "$out";; *) ok ;; esac
  case "$out" in *"gate not acknowledged"*) ok ;; *) bad "(d) the refusal reason must survive" "$out";; esac
  kill "$STUB_PID" 2>/dev/null; wait "$STUB_PID" 2>/dev/null; STUB_PID=""
else
  bad "(d) stub server never came up" "harness"
fi

echo
echo "test-board-transport-failure: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
