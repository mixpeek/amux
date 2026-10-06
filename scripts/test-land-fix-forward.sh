#!/usr/bin/env bash
# test-land-fix-forward.sh [amux]: `--priority --reason "fix forward <CARD>"`
# is allowed with no grant when that card names the pushing lane as the red's
# owner, and refused when the card names it only as not the owner
# (mixpeek-override, 2026-10-03, GG-41). A gate path matches at any depth.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lff.XXXXXX")"; mkdir -p "$H/.amux/logs/land" "$H/srv/api/board"
python3 - "$H/srv/api/board/GG-41" <<'PY'
import json,sys
json.dump({"title":"Fast Checks red on main","desc":"test_x is routed to lane-mvs\n22:15Z lane-spend owns the red test_y. Fix forward: abc\n22:17Z lane-obs: not mine, no revert from this lane"},open(sys.argv[1],"w"))
PY
port=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
(cd "$H/srv" && exec python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) & srv=$!
for _ in $(seq 1 20); do curl -s "http://127.0.0.1:$port/api/board/GG-41" >/dev/null 2>&1 && break; sleep 0.5; done
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
echo a > f; git add f; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
fail=0
run() { HOME="$H" AMUX_URL="http://127.0.0.1:$port" AMUX_LAND_NOTIFY=0 AMUX_WORKER="$1" bash "$AM" land --tries 1 --no-batch --priority --reason "$2" >/dev/null 2>&1; }
echo b > f; git commit -qm one -- f
run lane-spend "fix forward of a Fast Checks red on main (GG-41)"; rc=$?
git fetch -q origin
[ "$rc" = 0 ] && [ "$(git show origin/main:f)" = b ] && echo "ok   the owner named on GG-41 lands a fix forward with no grant" || { echo "FAIL owner refused rc=$rc"; fail=1; }
grep -q "priority allowed: fix forward, GG-41 names lane-spend" "$H/.amux/logs/land.log" && echo "ok   land.log names the card" || { echo "FAIL no allow line"; fail=1; }
echo c > f; git commit -qm two -- f
run lane-obs "fix forward of a Fast Checks red on main (GG-41)"; rc=$?
[ "$rc" != 0 ] && echo "ok   a lane GG-41 names as not the owner is refused" || { echo "FAIL non-owner allowed"; fail=1; }
grep -q "lane-obs priority fix-forward unproven" "$H/.amux/logs/land.log" && echo "ok   land.log says why" || { echo "FAIL no unproven line"; fail=1; }
run lane-other "fix forward GG-41"; rc=$?
[ "$rc" != 0 ] && echo "ok   a lane the card never names is refused" || { echo "FAIL unnamed lane allowed"; fail=1; }
# gs12-data, 2026-10-06: a gate path matches at any depth, so a monorepo's
# server/scripts/ci/ baseline fix needs no grant and no card.
mkdir -p server/scripts/ci; echo '{}' > server/scripts/ci/baseline.json; git add server/scripts/ci/baseline.json; git commit -qm nested
run lane-other "baseline for a check that reds every commit"
# The gate decision is the claim; the land itself may conflict on f above.
grep -q "lane-other priority allowed: range touches gate path server/scripts/ci/baseline.json" "$H/.amux/logs/land.log" && echo "ok   a nested scripts/ci/ path is a gate path" || { echo "FAIL nested gate path refused"; fail=1; }
kill "$srv" 2>/dev/null; wait 2>/dev/null
cd / && rm -rf -- "${H:?}"
exit $fail
