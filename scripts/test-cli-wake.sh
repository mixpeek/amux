#!/usr/bin/env bash
# `amux wake` (Ethan, 2026-10-29: "make sure our amux scheduler has all the
# capabilities as claude loop/wakeup thing").
#
# Pinned here: a one-off posts `once` with the prompt's bytes intact (stdin
# keeps backticks and $(...) literal), `in 20m` given as two words is one
# time, --every posts a recurring expr with max_runs, --when posts `when`,
# `when_once` and a default `until` of 24h, every write carries
# X-Amux-Session, a refusal prints the server's error and exits non-zero,
# --list scopes to $AMUX_SESSION, and --cancel is an attributed DELETE.
#
# Runs against a throwaway listener on a random port; nothing real is written.
set -euo pipefail
cd "$(dirname "$0")/.."
AMUX_BIN="${AMUX_BIN:-./amux}"
PASS=0; FAIL=0
ok(){ echo "  ok   $1"; PASS=$((PASS+1)); }
bad(){ echo "  FAIL $1"; echo "       $2"; FAIL=$((FAIL+1)); }

CAP=$(mktemp); PORTF=$(mktemp); H=$(mktemp -d)
trap 'kill $LPID 2>/dev/null; rm -rf "$CAP" "$PORTF" "$H"' EXIT
python3 - "$CAP" "$PORTF" <<'PY' &
import sys, json, http.server, socketserver
cap, portf = sys.argv[1], sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def _reply(self, code, body):
        self.send_response(code); self.send_header('Content-Type','application/json')
        self.end_headers(); self.wfile.write(json.dumps(body).encode())
    def _log(self, body=None):
        with open(cap, 'a') as f:
            f.write(json.dumps({"m": self.command, "path": self.path, "who": self.headers.get("X-Amux-Session"), "body": body}) + "\n")
    def do_GET(self):
        self._log()
        self._reply(200, [
            {"id": "SCHED-1", "enabled": 1, "kind": "tmux", "next_run": "2026-10-29T10:21", "sched_type": "once", "title": "wake: a"},
            {"id": "SCHED-2", "enabled": 0, "kind": "tmux", "next_run": "", "title": "off"},
            {"id": "SCHED-3", "enabled": 1, "kind": "tmux", "next_run": "2026-10-29T10:02", "schedule_expr": "every 2m", "title": "ci", "when": "test -f /x"},
        ])
    def do_POST(self):
        n = int(self.headers.get('Content-Length') or 0)
        body = json.loads(self.rfile.read(n) or b'{}')
        self._log(body)
        if body.get("once") == "soon":
            return self._reply(400, {"error": "once: unparseable datetime \"soon\"", "accepted": {"once": "a delay like 'in 20m'"}})
        out = dict(body, id="SCHED-9", next_run="2026-10-29T10:21")
        if "when" in body:
            out["until"] = "2026-10-30T10:00"
        self._reply(201, out)
    def do_DELETE(self):
        self._log()
        if self.path.endswith("SCHED-404"):
            return self._reply(404, {"error": "schedule not found"})
        self._reply(200, {"ok": True})
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
# Its own home naming the stub keeps every request here (see test-cli-memory.sh).
mkdir -p "$H/.amux"
printf '{"canonical_url":"http://127.0.0.1:%s"}\n' "$PORT" > "$H/.amux/endpoint.json"

run() {  # run <stdin> <args...> -> RC, OUT; requests land in $CAP
  local input="$1"; shift
  : > "$CAP"
  if OUT=$(printf '%s' "$input" | timeout 20 env -u AMUX_WORKER -u AMUX_URL HOME="$H" CC_HOME="$H/.amux" \
      AMUX_API="http://127.0.0.1:$PORT" AMUX_SESSION=wakecli AMUX_CURL_RESTART_WAIT=0 \
      bash "$AMUX_BIN" wake "$@" 2>&1); then RC=0; else RC=$?; fi
}
field() {  # field <python expr over the list of requests `r`>
  python3 -c "import json; r=[json.loads(l) for l in open('$CAP') if l.strip()]; print($1, end='')"
}

echo "amux wake"

P='Check `gh run view 1` and $(not run) the result.'
run "$P" 20m --stdin
[ "$RC" -eq 0 ] && ok "one-off exits 0" || bad "one-off exits 0" "rc=$RC out=$OUT"
[ "$(field "r[0]['body']['command']")" = "$P" ] && ok "the prompt reaches the wire byte for byte" || bad "prompt bytes" "$(cat "$CAP")"
[ "$(field "r[0]['body']['once'] + '|' + r[0]['body']['kind'] + '|' + r[0]['body']['session'] + '|' + r[0]['who']")" = "20m|tmux|wakecli|wakecli" ] \
  && ok "once, kind tmux, for and attributed to AMUX_SESSION" || bad "one-off body" "$(cat "$CAP")"
case "$OUT" in *"SCHED-9 fires once at 2026-10-29T10:21"*) ok "prints the id and when it fires" ;; *) bad "prints id" "$OUT" ;; esac

run "" in 20m "look again"
[ "$(field "r[0]['body']['once'] + '|' + r[0]['body']['command']")" = "in 20m|look again" ] \
  && ok "'in 20m' as two words is one time; an inline prompt works" || bad "in 20m" "$(cat "$CAP")"

run "loop body" --every 5m --max-runs 3 --stdin
[ "$(field "r[0]['body']['schedule_expr'] + '|' + str(r[0]['body']['max_runs']) + '|' + str('once' in r[0]['body'])")" = "every 5m|3|False" ] \
  && ok "--every posts a recurring expr with max_runs" || bad "--every" "$(cat "$CAP")"

run "ci is done" --when 'gh run view 1 --json status -q .status | grep -qx completed' --stdin
[ "$(field "r[0]['body']['when'] + '|' + r[0]['body']['schedule_expr'] + '|' + str(r[0]['body']['when_once']) + '|' + r[0]['body']['until']")" \
  = "gh run view 1 --json status -q .status | grep -qx completed|every 2m|True|24h" ] \
  && ok "--when posts when, every 2m, when_once, until 24h" || bad "--when" "$(cat "$CAP")"
case "$OUT" in *"fires once when it exits 0"*) ok "says it fires once" ;; *) bad "--when output" "$OUT" ;; esac

run "x" --when 'true' --check-every 10m --until 2026-11-01T00:00 --keep --stdin
[ "$(field "r[0]['body']['schedule_expr'] + '|' + str(r[0]['body']['when_once']) + '|' + r[0]['body']['until']")" = "every 10m|False|2026-11-01T00:00" ] \
  && ok "--check-every, --until and --keep" || bad "--keep" "$(cat "$CAP")"

run "x" soon --stdin
[ "$RC" -ne 0 ] && ok "a refusal exits non-zero" || bad "refusal rc" "rc=$RC"
case "$OUT" in *"HTTP 400"*"unparseable datetime"*) ok "prints the server's error" ;; *) bad "refusal text" "$OUT" ;; esac

run "" 20m --stdin
[ "$RC" -ne 0 ] && [ "$(field "len(r)")" = "0" ] && ok "an empty prompt is refused without a POST" || bad "empty prompt" "rc=$RC $(cat "$CAP")"

run "x" --every 5m --when true --stdin
[ "$RC" -ne 0 ] && [ "$(field "len(r)")" = "0" ] && ok "--every with --when is refused" || bad "exclusive" "rc=$RC"

run "" --list
[ "$(field "r[0]['path']")" = "/api/schedules?session=wakecli" ] && ok "--list scopes to AMUX_SESSION" || bad "--list path" "$(cat "$CAP")"
case "$OUT" in *SCHED-1*SCHED-3*"when: test -f /x"*) ok "--list shows enabled rows with the condition" ;; *) bad "--list output" "$OUT" ;; esac
case "$OUT" in *SCHED-2*) bad "--list hides disabled rows" "$OUT" ;; *) ok "--list hides disabled rows" ;; esac

run "" --cancel SCHED-9
[ "$(field "r[0]['m'] + ' ' + r[0]['path'] + ' ' + r[0]['who']")" = "DELETE /api/schedules/SCHED-9 wakecli" ] && [ "$RC" -eq 0 ] \
  && ok "--cancel is an attributed DELETE" || bad "--cancel" "rc=$RC $(cat "$CAP")"
run "" --cancel SCHED-404
[ "$RC" -ne 0 ] && case "$OUT" in *"HTTP 404"*"not found"*) true ;; *) false ;; esac && ok "a failed cancel says why" || bad "cancel 404" "rc=$RC $OUT"

run "" help
case "$OUT" in *ScheduleWakeup*"/loop"*) ok "help has the Claude table" ;; *) bad "help" "$OUT" ;; esac

echo "passed $PASS, failed $FAIL"
[ "$FAIL" -eq 0 ]
