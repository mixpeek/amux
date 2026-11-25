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
  [ "$2" = amux ] && mkdir -p "$1/crates/amux-server" "$1/scripts" && touch "$1/crates/amux-server/.keep" \
    && printf 'SMOKE_FALLBACK = ["smoke-a.spec.ts"]\n' > "$1/scripts/land_gate.py"
  echo x > "$1/f"
  git -C "$1" add -A
  git -C "$1" -c user.name=t -c user.email=t@e commit -qm "work" -m "Amux-Request: MSG-$3"
  git -C "$1" push -q origin HEAD:main 2>/dev/null
  git -C "$1" fetch -q origin main
}
mkrepo "$T/other" plain 101
mkrepo "$T/srv" amux 202
# AF-1304: a request committed and never pushed. MSG-505 (inside the window) is
# on a local branch; MSG-606 (30 days old, outside it) is on a detached
# worktree's HEAD, like 47722b4fe on the shared checkout.
git -C "$T/other" checkout -q -b local-only
echo y > "$T/other/g"; git -C "$T/other" add -A
git -C "$T/other" -c user.name=t -c user.email=t@e commit -qm "export button" -m "Amux-Request: MSG-505"
# MSG-101 landed on main; this second commit for it changes something else and
# was never pushed: a stray, while the request itself reads landed.
echo w > "$T/other/k"; git -C "$T/other" add -A
git -C "$T/other" -c user.name=t -c user.email=t@e commit -qm "launch thread follow-up" -m "Amux-Request: MSG-101"
git -C "$T/other" checkout -q -
git -C "$T/other" worktree add -q --detach "$T/other-wt" origin/main
# A rebased copy of the commit that landed MSG-202: same change, new sha. It is
# not stray, because its change is on main.
git -C "$T/srv" checkout -q -b rebased
git -C "$T/srv" -c user.name=t -c user.email=t@e commit -q --amend -m "work, rebased" -m "Amux-Request: MSG-202"
git -C "$T/srv" checkout -q -
echo z > "$T/other-wt/h"; git -C "$T/other-wt" add -A
git -C "$T/other-wt" -c user.name=t -c user.email=t@e commit -qm "gate fix" -m "Amux-Request: MSG-606"
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
        {"id": 404, "ts": $NOW - 1700000, "session": "w-other", "sender": "owner", "kind": "human", "text": "make sure u fix it"},
        {"id": 505, "ts": $NOW - 1600000, "session": "w-other", "sender": "owner", "kind": "human", "text": "add the export button"}]
OLD = {"606": {"id": 606, "ts": $NOW - 30 * 86400000, "session": "w-other", "sender": "owner", "kind": "human", "text": "the gate re-creates its checkout"}}
# The server's answered rule (AF-1059), as GET /api/task-history/requests reports it.
REQS = {"w-other": [{"id": "MSG-303", "status": "answered", "evidence": "replied: The Mac is healthy"}]}
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        p = self.path
        body = SESS if p.startswith("/api/sessions") else health() if p.startswith("/health") \
            else (OLD.get(p.split("/api/history/")[1].split("?")[0]) or next((h for h in HIST if str(h["id"]) == p.split("/api/history/")[1].split("?")[0]), {})) if p.startswith("/api/history/") \
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
mkdir -p "$T/home/land-gate/receipts"
# AF-1384: what a landed request's land tested, from the gate's receipt, matched
# by patch id. MSG-202's land ran a spec of its own beside the smoke set (read
# from the repo's own scripts/land_gate.py); MSG-101's commit has no receipt.
PID202=$(git -C "$T/srv" show origin/main | git patch-id --stable | cut -d' ' -f1)
receipt() { printf '{"commit_patch_ids":["%s"],"pass":true,"at":1,"legs":{"e2e":{"specs":%s,"verdict":"e2e_pass"}}}' "$PID202" "$1" > "$T/home/land-gate/receipts/r202.json"; }
receipt '["card-count.spec.ts","smoke-a.spec.ts"]'
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
# AF-1304: committed but not on origin/main reads stranded, not open, and one
# older than the window is still listed, with where its commit is.
echo "$OUT" | grep -E '^stranded +w-other +MSG-505 ' >/dev/null || fail "a request committed on a local branch reads stranded: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -E '^stranded +w-other +MSG-606 ' >/dev/null || fail "a stranded request older than the window is still listed: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -q 'stranded: 2 request(s) committed but not on origin/main' && echo "$OUT" | grep -qF "MSG-606 w-other" && echo "$OUT" | grep -qF "$T/other-wt" || fail "the stranded lines name the commit and the worktree: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -q 'stray: 1 commit(s) citing a request that landed' && echo "$OUT" | grep -qE '^  MSG-101 w-other [0-9a-f]{9} in local-only' || fail "a landed request's unpushed commit is listed as a stray: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -E '^landed +w-other +MSG-101' >/dev/null || fail "and the request itself still reads landed: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -qE '^  MSG-202 ' && fail "a rebased copy of a landed commit is not a stray: $OUT"
pass=$((pass+1))
# Land review F1: the summary's parts add up to its total, stranded included.
echo "$OUT" | python3 -c '
import re,sys
line=[l for l in sys.stdin.read().splitlines() if "requests from the owner" in l][0]
total=int(line.split()[0]); parts=[int(x) for x in re.findall(r"(\d+) (?:open|stranded|on main not live|live|landed|answered|closed)", line.split(":",1)[1])]
sys.exit(0 if sum(parts)==total else "summary %d != parts %s: %s" % (total, parts, line))' || fail "the summary parts must add up to the total: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -A3 -E '^(on-main|held) +w-srv +MSG-202' | grep -q 'e2e: passed: card-count.spec.ts$' || fail "a landed request names the specs of its own its land passed, without the smoke set: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -A3 -E '^landed +w-other +MSG-101' | grep -q 'e2e: no land receipt for this commit' || fail "a landed commit no receipt names says so: $OUT"
pass=$((pass+1))
echo "$OUT" | grep -q '^e2e of the 2 landed: 1 passed specs of their own in their land, 0 ran none of their own, 0 failed there, 1 have no land receipt' || fail "the e2e summary counts them apart: $OUT"
pass=$((pass+1))
receipt '["smoke-a.spec.ts"]'
SMOKE=$(CC_HOME="$T/home" AMUX_HOME="$T/home" AMUX_API="$API" bash "$ROOT/amux" requests w-srv 2>&1)
echo "$SMOKE" | grep -q 'e2e: only the smoke set ran (smoke-a.spec.ts); no spec of this change' || fail "a land that ran only the smoke set is not read as tested: $SMOKE"
pass=$((pass+1))
OPEN=$(CC_HOME="$T/home" AMUX_HOME="$T/home" AMUX_API="$API" bash "$ROOT/amux" requests w-other --open 2>&1)
echo "$OPEN" | grep -E '^stranded +w-other +MSG-606 ' >/dev/null || fail "--open keeps a stranded request: $OPEN"
pass=$((pass+1))
# Land review F1: /health with no commit makes live() None for EVERY repo; the
# amux repo's work must stay on main, not read "landed elsewhere".
touch "$T/no-commit"
OUT2=$(CC_HOME="$T/home" AMUX_HOME="$T/home" AMUX_API="$API" bash "$ROOT/amux" requests w-other w-srv 2>&1)
echo "$OUT2" | grep -E '^(on-main|held) +w-srv +MSG-202' >/dev/null || fail "with no build commit, amux-repo work must stay on main: $OUT2"
pass=$((pass+1))
echo "$OUT2" | grep -E '^landed +w-other +MSG-101' >/dev/null || fail "and another repo's work still reads landed: $OUT2"
pass=$((pass+1))
# AF-1401: the stranded pass is cached by the refs it reads. An unchanged repo
# answers from the cache; a ref that moved is read again.
J=$(CC_HOME="$T/home" AMUX_HOME="$T/home" AMUX_API="$API" bash "$ROOT/amux" requests w-other --json 2>/dev/null)
echo "$J" | python3 -c 'import json,sys; c=json.load(sys.stdin)["stranded_cache"]; sys.exit(0 if c["hit"] >= 1 and c["miss"] == 0 else 1)' || fail "an unchanged repo answers the stranded pass from its cache: $J"
pass=$((pass+1))
git -C "$T/other" branch -q -D local-only
OUT3=$(CC_HOME="$T/home" AMUX_HOME="$T/home" AMUX_API="$API" bash "$ROOT/amux" requests w-other 2>&1)
echo "$OUT3" | grep -E '^open +w-other +MSG-505 ' >/dev/null || fail "a deleted branch is read again, not answered from the cache: $OUT3"
pass=$((pass+1))
echo "test-requests-landed: $pass passed, 0 failed"
