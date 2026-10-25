#!/usr/bin/env bash
# The Caddyfile bootstrap.sh writes must hand amux the door's member token and never
# a client's. Builds the config from bootstrap.sh itself, points it at a stand-in door
# and an echo upstream, and checks three requests. Needs CADDY_BIN; prints "not run"
# and exits 2 without it, so a missing binary never reads as a pass.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
CADDY="${CADDY_BIN:-}"
[[ -x "$CADDY" ]] || { echo "not run: set CADDY_BIN to a caddy binary"; exit 2; }
T="$(mktemp -d)"; P=$((20000 + RANDOM % 20000)); DP=$((P + 1)); UP=$((P + 2))
awk '/^cat > \/etc\/caddy\/Caddyfile <<EOF/{f=1;next} /^EOF$/{f=0} f' "$HERE/bootstrap.sh" \
  | sed -e "s/^\$DOMAIN {/:$P {/" -e 's/^\$TLS_LINE$//' -e "s#127.0.0.1:8890#127.0.0.1:$DP#g" \
        -e "s#reverse_proxy https://127.0.0.1:8824#reverse_proxy 127.0.0.1:$UP#" \
        -e '/transport http {/,/}/d' -e '/output file/,/}/d' > "$T/Caddyfile"
printf '{\n  admin off\n  auto_https off\n}\n' | cat - "$T/Caddyfile" > "$T/C2" && mv "$T/C2" "$T/Caddyfile"
PORT=$DP python3 "$HERE/testdata/fake_door.py" & D=$!
PORT=$UP python3 "$HERE/testdata/echo_upstream.py" & U=$!
"$CADDY" run --config "$T/Caddyfile" --adapter caddyfile > "$T/caddy.log" 2>&1 & C=$!
trap 'kill $D $U $C 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do curl -s -o /dev/null "127.0.0.1:$P/" && break; sleep 0.1; done
fail=0
m=$(curl -s -H 'Cookie: amux_door=member' -H 'X-Amux-Member: FORGED' "127.0.0.1:$P/api/x")
[[ "$m" == *'"cookie": "amux_member=REAL-TOKEN"'* ]] || { echo "FAIL member: $m"; fail=1; }
p=$(curl -s -H 'Cookie: amux_door=password' -H 'X-Amux-Member: FORGED' "127.0.0.1:$P/api/x")
[[ "$p" != *FORGED* ]] || { echo "FAIL forged header reached amux: $p"; fail=1; }
a=$(curl -s -o /dev/null -w '%{http_code}' -H 'X-Amux-Member: FORGED' "127.0.0.1:$P/api/x")
[[ "$a" == 401 ]] || { echo "FAIL anonymous got $a"; fail=1; }
[[ $fail == 0 ]] && echo "PASS 3 checks (member token forwarded, forged header dropped, anonymous refused)"
exit $fail
