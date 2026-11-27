#!/usr/bin/env bash
# test-land-stale-hooks.sh [amux]: under the fast gate, land skips its own
# precheck because the push hook runs the same legs. That holds only when the
# checkout's .githooks are the branch's committed ones. A shared checkout far
# behind origin had a stub land-precheck, so a commit landed on held GS-12 rows
# (mixpeek, 2026-11-27 12:33Z). With stale hooks, land must run the committed
# precheck itself and refuse; with fresh hooks it still skips.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lsh.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
mkdir -p .githooks
# The committed check refuses any stack that changes `held`, but only when land
# itself runs it on the stack (batch mode); the enqueue check passes, as it did
# for a commit made after enqueue.
cat > .githooks/land-precheck <<'EOF'
#!/bin/sh
if [ "${AMUX_LAND_PRECHECK_MODE:-}" = batch ] && git diff --name-only "$1" "$2" | grep -qx held; then
  echo "land-precheck: refused: held row changed"; exit 1
fi
echo "land-precheck: passed"; exit 0
EOF
chmod +x .githooks/land-precheck
echo a > held; echo a > free
git add .githooks/land-precheck held free; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
mkdir -p .git/hooks
printf '#!/bin/sh\nexit 0\n' > .git/hooks/pre-push; chmod +x .git/hooks/pre-push
git config amux.landFastGate true
fail=0

# Stale: the checkout's land-precheck is a stub, so the push hook would pass.
printf '#!/bin/sh\necho stub; exit 0\n' > .githooks/land-precheck
echo b > held; git commit -qm "touch held" -- held
HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lsh bash "$AM" land --tries 1 --no-batch >/dev/null 2>&1; rc=$?
git fetch -q origin
if [ "$rc" != 0 ] && [ "$(git show origin/main:held)" = a ]; then
  echo "ok   stale hooks: the committed precheck refused and nothing was pushed"
else
  echo "FAIL stale hooks pushed (rc=$rc)"; fail=1
fi
grep -q "verdict=land_hooks_stale" "$H/.amux/logs/land.log" && echo "ok   land.log names the stale hooks" || { echo "FAIL no land_hooks_stale line"; fail=1; }

# Fresh: the committed hooks again; the skip applies and a free change lands.
git checkout -q -- .githooks; git reset -q --hard origin/main
echo b > free; git commit -qm "touch free" -- free
HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lsh bash "$AM" land --tries 1 --no-batch >/dev/null 2>&1; rc=$?
git fetch -q origin
if [ "$rc" = 0 ] && [ "$(git show origin/main:free)" = b ]; then
  echo "ok   fresh hooks: the land goes through"
else
  echo "FAIL fresh hooks rc=$rc"; fail=1
fi
cd / && rm -rf -- "${H:?}"
exit $fail
