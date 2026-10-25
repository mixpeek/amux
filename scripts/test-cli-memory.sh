#!/usr/bin/env bash
# `amux memory` (Ethan, 2026-10-19). gainz was told "add to this workers
# memory", found no verb for its amux memory, and saved the rule into Claude's
# directory auto-memory, which the Memory tab never shows.
#
# Pinned here: `add` sends the note as `append` with its bytes intact (stdin
# keeps backticks literal), also spells out `content` as prior + note so a
# server that predates `append` does not write an empty memory, targets
# $AMUX_SESSION or --worker, and an empty note is refused without a POST.
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
    def _reply(self, body):
        self.send_response(200); self.send_header('Content-Type','application/json')
        self.end_headers(); self.wfile.write(json.dumps(body).encode())
    def do_GET(self):
        with open(cap, 'a') as f: f.write(json.dumps({"m": "GET", "path": self.path}) + "\n")
        self._reply({"content": "OWNER TEXT\n"})
    def do_POST(self):
        n = int(self.headers.get('Content-Length') or 0)
        body = json.loads(self.rfile.read(n) or b'{}')
        with open(cap, 'a') as f: f.write(json.dumps({"m": "POST", "path": self.path, "body": body}) + "\n")
        self._reply({"ok": True, "bytes": 42, "appended": True})
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
# The CLI rewrites a localhost AMUX_API whose port endpoint.json does not name
# to the live server (_amux_heal_url). Its own CC_HOME, naming the stub, keeps
# every request here: a first draft of this test appended to two real memories.
mkdir -p "$H/.amux"
printf '{"canonical_url":"http://127.0.0.1:%s"}\n' "$PORT" > "$H/.amux/endpoint.json"

run() {  # run <stdin> <args...> -> RC, OUT; requests land in $CAP
  local input="$1"; shift
  : > "$CAP"
  if OUT=$(printf '%s' "$input" | timeout 20 env -u AMUX_WORKER -u AMUX_URL HOME="$H" CC_HOME="$H/.amux" \
      AMUX_API="http://127.0.0.1:$PORT" AMUX_SESSION=memcli \
      bash "$AMUX_BIN" memory "$@" 2>&1); then RC=0; else RC=$?; fi
}
field() {  # field <python expr over the list of requests `r`>
  python3 -c "import json; r=[json.loads(l) for l in open('$CAP') if l.strip()]; print($1, end='')"
}

echo "amux memory"

NOTE='Same lifts every week; run `progress` first. $(not run)'
run "$NOTE" add --stdin
[ "$RC" -eq 0 ] && ok "add --stdin exits 0" || bad "add --stdin exits 0" "rc=$RC out=$OUT"
[ "$(field "[x['body']['append'] for x in r if x['m']=='POST'][0]")" = "$NOTE" ] \
  && ok "the note reaches the wire byte for byte as append" || bad "append bytes" "$(cat "$CAP")"
[ "$(field "repr([x['body']['content'] for x in r if x['m']=='POST'][0])")" = "$(python3 -c 'import sys; print(repr("OWNER TEXT\n\n" + sys.argv[1] + "\n"), end="")' "$NOTE")" ] && ok "content is prior + note for an older server" || bad "content fallback" "$(cat "$CAP")"
[ "$(field "[x['path'] for x in r if x['m']=='POST'][0]")" = "/api/sessions/memcli/memory" ] \
  && ok "targets AMUX_SESSION" || bad "target" "$(cat "$CAP")"
case "$OUT" in *"saved to memcli's amux memory"*) ok "says where it saved" ;; *) bad "says where it saved" "$OUT" ;; esac

run "" add "plain words" --worker gainz
[ "$(field "[x['path'] for x in r if x['m']=='POST'][0]")" = "/api/sessions/gainz/memory" ] \
  && [ "$(field "[x['body']['append'] for x in r if x['m']=='POST'][0]")" = "plain words" ] \
  && ok "--worker and inline text" || bad "--worker and inline text" "$(cat "$CAP")"

run "   " add --stdin
[ "$RC" -ne 0 ] && [ "$(field "len([x for x in r if x['m']=='POST'])")" = "0" ] \
  && ok "an empty note is refused without a POST" || bad "empty note" "rc=$RC $(cat "$CAP")"

run "" 
[ "$RC" -eq 0 ] && [ "$OUT" = "OWNER TEXT" ] && ok "bare 'amux memory' prints it" || bad "show" "rc=$RC out=$OUT"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
