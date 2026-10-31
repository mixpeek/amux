#!/usr/bin/env bash
# test-land-queue-policy.sh [amux]: amux land asks GET /api/land/policy before
# it queues on the server (AF-1103), against a stub server and throwaway repos
# under a temp HOME, so no live ~/.amux state is read or written.
#
#   1. policy says queue:false: no POST /api/land, the client land runs
#   2. policy says queue:true: the land is POSTed and reported as queued
#   3. policy unreadable (404): the POST is still tried, as before
set -euo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"; AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
T="$(mktemp -d "${TMPDIR:-/tmp}/lqpol.XXXXXX")"
STUB_PID=""
trap '[ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null; rm -rf -- "${T:?}"' EXIT
export HOME="$T/home"; mkdir -p "$HOME/.amux/logs"
# Every inherited AMUX_ and CC_ variable goes: a worker's AMUX_ENV_FROM_FILE makes the
# CLI reload the worker's own session and server, and this test must reach only
# the stub.
for _v in $(compgen -e | grep -E '^(AMUX_|CC_)'); do unset "$_v"; done   # CC_HOME would read the live ~/.amux
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE TMUX TMUX_PANE   # a pane name replaces AMUX_SESSION
export AMUX_SESSION=lqpol AMUX_LAND_NOTIFY=0 AMUX_WORKER=lqpol GIT_CONFIG_NOSYSTEM=1
pass=0; fail=0
ok()  { pass=$((pass + 1)); echo "ok   $*"; }
bad() { fail=$((fail + 1)); echo "FAIL $*"; }

# The stub answers the policy from $T/policy (a body, or "404"), answers a
# POST /api/land with 202, and appends every request line to $T/requests.
cat > "$T/stub.py" <<'PY'
import http.server, os, sys
T = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _rec(self):
        with open(os.path.join(T, "requests"), "a") as f:
            f.write("%s %s\n" % (self.command, self.path.split("?")[0]))
    def _send(self, code, body):
        self.send_response(code); self.send_header("Content-Type", "application/json"); self.end_headers()
        self.wfile.write(body.encode())
    def do_GET(self):
        self._rec()
        pol = open(os.path.join(T, "policy")).read().strip()
        if self.path.startswith("/api/land/policy") and pol != "404":
            return self._send(200, pol)
        self._send(404, '{"error":"not found"}')
    def do_POST(self):
        self._rec()
        n = int(self.headers.get("Content-Length") or 0); self.rfile.read(n)
        if self.path == "/api/land":
            return self._send(202, '{"ok":true,"id":1,"state":"queued"}')
        self._send(404, '{"error":"not found"}')
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(os.path.join(T, "port"), "w").write(str(s.server_port))
s.serve_forever()
PY
echo '{"queue":false}' > "$T/policy"
python3 "$T/stub.py" "$T" & STUB_PID=$!
for _ in $(seq 1 50); do [ -s "$T/port" ] && break; sleep 0.1; done
export AMUX_API="http://127.0.0.1:$(cat "$T/port")"
# The CLI heals an AMUX_API that differs from endpoint.json's canonical URL, so
# the stub is the canonical URL here, or the live server would be asked.
printf '{"canonical_url":"%s","retired_ports":[]}\n' "$AMUX_API" > "$HOME/.amux/endpoint.json"

cd "$T"
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane
git config user.email t@t; git config user.name t
echo base > a.txt; git add a.txt; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
land() { : > "$T/requests"; bash "$AM" land "$@" >"$T/out" 2>&1; echo $?; }
step() { git fetch -q origin main; git reset -q --hard origin/main; echo "$1" >> a.txt; git add a.txt; git commit -qm "test: $1"; git rev-parse HEAD; }

# 1. queue off: no POST, the client land lands it
c1="$(step one)"
echo '{"lane":"lqpol","queue":false,"measured":true}' > "$T/policy"
rc="$(land --sha "$c1")"
git fetch -q origin main
if ! grep -q '^POST /api/land$' "$T/requests" && grep -q '^GET /api/land/policy$' "$T/requests" && git show origin/main:a.txt | grep -qx one; then
  ok "queue off: policy read, no POST /api/land, landed by the client (rc $rc)"
else bad "queue off: rc=$rc requests=[$(tr '\n' ' ' < "$T/requests")] $(tail -3 "$T/out")"; fi

# 2. queue on: POSTed and reported queued
c2="$(step two)"
echo '{"lane":"lqpol","queue":true,"measured":true}' > "$T/policy"
rc="$(land --sha "$c2")"
if [ "$rc" = 0 ] && grep -q '^POST /api/land$' "$T/requests" && grep -q 'queued on the server' "$T/out"; then
  ok "queue on: the land is posted and reported queued"
else bad "queue on: rc=$rc requests=[$(tr '\n' ' ' < "$T/requests")] $(tail -3 "$T/out")"; fi

# 3. policy unreadable: the POST is still tried
echo 404 > "$T/policy"
rc="$(land --sha "$c2")"
if grep -q '^POST /api/land$' "$T/requests"; then ok "policy unreadable: the POST is still tried"
else bad "policy unreadable: rc=$rc requests=[$(tr '\n' ' ' < "$T/requests")] $(tail -3 "$T/out")"; fi

echo "test-land-queue-policy: $pass passed, $fail failed"
[ "$fail" = 0 ]
