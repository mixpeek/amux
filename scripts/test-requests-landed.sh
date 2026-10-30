#!/usr/bin/env bash
# AF-1058: `amux requests` read a commit in a repo the server never deploys as
# "on main, not live" forever (amux-gtm: 10 requests). Such a commit is
# "landed"; one in the amux repo that the running build lacks is still
# "on main, not live". A stub API stands in for the server; the repos are real.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d)
trap 'kill "${SRV:-0}" 2>/dev/null || true; rm -rf "${T:?}"' EXIT
pass=0
fail() { echo "FAIL: $*"; exit 1; }

mkrepo() {  # $1 dir, $2 "amux" makes it look like the server's repo
  git init -q --bare "$1.git"
  git clone -q "$1.git" "$1" 2>/dev/null
  [ "$2" = amux ] && mkdir -p "$1/crates/amux-server" && touch "$1/crates/amux-server/.keep"
  echo x > "$1/f"
  git -C "$1" add -A
  git -C "$1" -c user.name=t -c user.email=t@e commit -qm "work" -m "Amux-Request: MSG-$3"
  git -C "$1" push -q origin HEAD:main 2>/dev/null
  git -C "$1" fetch -q origin main
}
mkrepo "$T/other" plain 101
mkrepo "$T/srv" amux 202
NOW=$(($(date +%s) * 1000))
cat > "$T/api.py" <<PY
import http.server, json, sys
import os
SESS = [{"name": "w-other", "dir": "$T/other"}, {"name": "w-srv", "dir": "$T/srv"}]
def health():  # read per request: the test flips it between runs
    return {} if os.path.exists("$T/no-commit") else {"commit": "0" * 40}
HIST = [{"id": 101, "ts": $NOW - 3600000, "session": "w-other", "sender": "owner", "kind": "human", "text": "post the launch thread"},
        {"id": 202, "ts": $NOW - 3600000, "session": "w-srv", "sender": "owner", "kind": "human", "text": "fix the card count"},
        {"id": 303, "ts": $NOW - 1800000, "session": "w-other", "sender": "owner", "kind": "human", "text": "hows mac perf?"},
        {"id": 404, "ts": $NOW - 1700000, "session": "w-other", "sender": "owner", "kind": "human", "text": "make sure u fix it"}]
# The server's answered rule (AF-1059), as GET /api/task-history/requests reports it.
REQS = {"w-other": [{"id": "MSG-303", "status": "answered", "evidence": "replied: The Mac is healthy"}]}
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        p = self.path
        body = SESS if p.startswith("/api/sessions") else health() if p.startswith("/health") \
            else (HIST if "offset=0" in p else []) if p.startswith("/api/history") \
            else {"measured": True, "tasks": {}} if p.startswith("/api/task-history/at") \
            else {"measured": True, "requests": REQS.get(p.split("session=")[1].split("&")[0], [])} if p.startswith("/api/task-history/requests") else {}
        b = json.dumps(body).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(b)
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open("$T/port", "w").write(str(s.server_port)); s.serve_forever()
PY
python3 "$T/api.py" & SRV=$!
for _ in $(seq 50); do [ -s "$T/port" ] && break; sleep 0.1; done
API="http://127.0.0.1:$(cat "$T/port")"
# CC_HOME with no endpoint.json: the CLI otherwise "heals" a localhost API on an
# unknown port to the real server's address, and this stub would never be asked.
mkdir -p "$T/home"
OUT=$(CC_HOME="$T/home" AMUX_HOME="$T/home" AMUX_API="$API" bash "$ROOT/amux" requests w-other w-srv 2>&1)

echo "$OUT" | grep -E '^landed +w-other +MSG-101' >/dev/null || fail "a commit in a repo the server does not deploy must read landed: $OUT"
pass=$((pass+1))
# "held" is the same state, labelled when main's CI is still running.
echo "$OUT" | grep -E '^(on-main|held) +w-srv +MSG-202' >/dev/null || fail "an amux-repo commit the build lacks must stay on main, not live (control): $OUT"
pass=$((pass+1))
echo "$OUT" | grep -q '1 on main not live, 0 live, 1 landed in a repo this server does not deploy, 1 answered' || fail "the summary counts them apart: $OUT"
pass=$((pass+1))
# AF-1059: a question the server marked answered reads answered with the reply;
# an ask that produced neither a commit nor an answer stays open.
echo "$OUT" | grep -E '^answered +w-other +MSG-303 ' >/dev/null && echo "$OUT" | grep -q 'replied: The Mac is healthy' || fail "an answered question reads answered with its reply: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -E '^open +w-other +MSG-404' >/dev/null || fail "an unanswered ask stays open: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -q ', 1 answered, ' || fail "the summary counts answered: $OUT"
pass=$((pass+1))
# Land review F1: /health with no commit makes live() None for EVERY repo; the
# amux repo's work must stay on main, not read "landed elsewhere".
touch "$T/no-commit"
OUT2=$(CC_HOME="$T/home" AMUX_HOME="$T/home" AMUX_API="$API" bash "$ROOT/amux" requests w-other w-srv 2>&1)
echo "$OUT2" | grep -E '^(on-main|held) +w-srv +MSG-202' >/dev/null || fail "with no build commit, amux-repo work must stay on main: $OUT2"
pass=$((pass+1))
echo "$OUT2" | grep -E '^landed +w-other +MSG-101' >/dev/null || fail "and another repo's work still reads landed: $OUT2"
pass=$((pass+1))
echo "test-requests-landed: $pass passed, 0 failed"
