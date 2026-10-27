#!/usr/bin/env bash
# scripts/pr-watch.sh wakes the PR handler once per change, and never on a
# read it could not make. A stub gh and a stub amux /run endpoint stand in
# for GitHub and the server.
set -euo pipefail
cd "$(dirname "$0")/.."

tmp=$(mktemp -d)
trap '{ kill "$srv"; wait "$srv"; } 2>/dev/null || true; rm -rf "$tmp"' EXIT
pass=0 fail=0
check() { if [ "$2" = "$3" ]; then echo "  ok    $1"; pass=$((pass+1)); else echo "  FAIL  $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }

cat > "$tmp/gh" <<'EOF'
#!/usr/bin/env bash
[ -f "$STUB/fail" ] && { echo "HTTP 401: Bad credentials" >&2; exit 1; }
# Apply the caller's own --jq program to a JSON fixture, so the filter that
# ships is the filter under test.
prog=; while [ $# -gt 0 ]; do [ "$1" = --jq ] && prog=$2; shift; done
case "$prog" in
  *headRefOid*) jq -r "$prog" "$STUB/prs.json" ;;
  *) cat "$STUB/runs" 2>/dev/null || true ;;
esac
EOF
chmod +x "$tmp/gh"

cat > "$tmp/srv.py" <<'EOF'
import http.server, sys, os
d = sys.argv[1]
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        code = int(open(os.path.join(d, "code")).read()) if os.path.exists(os.path.join(d, "code")) else 200
        open(os.path.join(d, "hits"), "a").write(self.path + "\n")
        self.send_response(code); self.end_headers(); self.wfile.write(b"{}")
    def log_message(self, *a): pass
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(os.path.join(d, "port"), "w").write(str(s.server_port))
s.serve_forever()
EOF
python3 "$tmp/srv.py" "$tmp" & srv=$!
for _ in $(seq 50); do [ -s "$tmp/port" ] && break; sleep 0.1; done

pr() {  # number head [author:id ...]
  local n=$1 h=$2; shift 2; local c=
  for a in "$@"; do c="$c${c:+,}{\"author\":{\"login\":\"${a%%:*}\"},\"id\":\"${a#*:}\"}"; done
  echo "[{\"number\":$n,\"headRefOid\":\"$h\",\"comments\":[$c],\"reviews\":[]}]" > "$tmp/prs.json"
}

export STUB=$tmp PR_WATCH_GH=$tmp/gh PR_WATCH_HANDLER=SCHED-T PR_WATCH_SELF=handler \
  PR_WATCH_STATE=$tmp/state PR_WATCH_URL=http://127.0.0.1:$(cat "$tmp/port")
hits() { wc -l < "$tmp/hits" 2>/dev/null | tr -d ' ' || echo 0; }
run() { bash scripts/pr-watch.sh 2>&1 | grep -o 'verdict=[a-z_]*' || true; }

echo '[]' > "$tmp/prs.json"
check "no open PRs: no wake" "$(run)" "verdict=no_prs"

pr 391 abc
check "a new PR wakes the handler" "$(run)" "verdict=woke"
check "  and ran the named schedule" "$(cat "$tmp/hits")" "/api/schedules/SCHED-T/run"
check "the same PRs again: no wake" "$(run)" "verdict=unchanged"

echo "run 999 abc" > "$tmp/runs"
check "a fork CI run awaiting approval wakes it" "$(run)" "verdict=woke"

pr 391 abc handler:c1
check "the handler's own review does not wake it again" "$(run)" "verdict=unchanged"
pr 391 abc handler:c1 pedro:c2
check "the contributor's reply wakes it" "$(run)" "verdict=woke"

pr 391 def handler:c1 pedro:c2
echo 503 > "$tmp/code"
check "a refused wake says so" "$(run)" "verdict=wake_refused"
rc=0; bash scripts/pr-watch.sh >/dev/null 2>&1 || rc=$?
check "  and exits non-zero, so the schedule records a failure" "$rc" "1"
rm "$tmp/code"
check "  and is retried next tick" "$(run)" "verdict=woke"

touch "$tmp/fail"
check "a failed read is unmeasured, never quiet" "$(run)" "verdict=unmeasured"
check "  and wakes nothing" "$(hits)" "6"

echo
echo "pr-watch: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
