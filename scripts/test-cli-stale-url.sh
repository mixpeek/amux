#!/usr/bin/env bash
# test-cli-stale-url.sh [amux]: a localhost AMUX_API/AMUX_URL on a port that
# endpoint.json does not name is replaced by endpoint.json's canonical_url, and
# logged once; a remote override is left alone (2026-10-05: a reboot race left
# every lane on 8823 and every CLI verb failed with curl exit 7).
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/csu.XXXXXX")"; mkdir -p "$H/.amux/state" "$H/srv/api"
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
DEAD=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
echo '[]' > "$H/srv/api/board"
( cd "$H/srv" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 ) 2> "$H/srv.log" &
SRV=$!
trap 'kill $SRV 2>/dev/null; cd /; rm -rf -- "${H:?}"' EXIT
for _ in $(seq 1 50); do curl -s "http://127.0.0.1:$PORT/api/board" >/dev/null 2>&1 && break; sleep 0.1; done
printf '{"canonical_port":%s,"canonical_url":"http://127.0.0.1:%s","legacy_port":8822,"retired_ports":[8822]}\n' "$PORT" "$PORT" > "$H/.amux/endpoint.json"
fail=0
: > "$H/srv.log"
HOME="$H" CC_HOME="$H/.amux" AMUX_SESSION=csu AMUX_API="http://127.0.0.1:$DEAD" AMUX_URL="http://127.0.0.1:$DEAD" \
  bash "$AM" board ls >/dev/null 2>&1
grep -q 'GET /api/board' "$H/srv.log" && echo "ok   a stale localhost AMUX_API reaches endpoint.json's server" || { echo "FAIL the CLI did not reach the canonical server"; fail=1; }
n=$(grep -c 'stale_env_url_healed' "$H/.amux/cli-transport.jsonl" 2>/dev/null || echo 0)
[ "$n" -ge 1 ] && echo "ok   the heal is logged ($n line(s))" || { echo "FAIL no stale_env_url_healed line"; fail=1; }
HOME="$H" CC_HOME="$H/.amux" AMUX_SESSION=csu AMUX_API="http://127.0.0.1:$DEAD" bash "$AM" board ls >/dev/null 2>&1
n2=$(grep -c "\"var\":\"AMUX_API\"" "$H/.amux/cli-transport.jsonl" 2>/dev/null || echo 0)
[ "$n2" = 1 ] && echo "ok   logged once per lane and port, not per call" || { echo "FAIL AMUX_API heal logged $n2 times"; fail=1; }
out=$(grep -n '_amux_heal_url()' -A12 "$AM" | grep -c 'https://localhost:\*|http://localhost:\*|https://127.0.0.1:\*|http://127.0.0.1:\*) ;; \*) continue')
[ "$out" = 1 ] && echo "ok   only localhost URLs are healed; a remote override is left alone" || { echo "FAIL the localhost guard is missing"; fail=1; }
exit $fail
