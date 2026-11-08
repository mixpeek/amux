#!/usr/bin/env bash
# test-land-holder-pid-reused.sh [amux]: a land lock whose holder died and whose
# pid now belongs to another process is taken over at once (implicit state
# machines inventory 2026-10-31, #23). `kill -0` alone passed for the new
# process, so the queue waited out --max-hold behind nobody. The holder's start
# time is written with the lock and must still match.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
T="$(mktemp -d "${TMPDIR:-/tmp}/lpr.XXXXXX")"; cd "$T" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
echo a > f; git add f; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
mkdir -p .git/hooks; printf '#!/bin/sh\nsleep 120\n' > .git/hooks/pre-push; chmod +x .git/hooks/pre-push
echo b > f; git commit -qm "lane change" -- f
AMUX_LAND_NOTIFY=0 AMUX_WORKER=lpr-holder bash "$AM" land --no-batch > "$T/holder.out" 2>&1 &
key=""; for _ in $(seq 1 40); do
  for d in "$HOME"/.amux/locks/land-*; do [ "$(cat "$d/who" 2>/dev/null)" = lpr-holder ] && key="$d"; done
  # The push itself, by its pid file: a child process alone is no sign, the
  # remote-lease heartbeat is a child from before the gate (2026-11-06).
  [ -n "$key" ] && [ -s "$key/push.pid" ] && kill -0 "$(cat "$key/push.pid")" 2>/dev/null && break; sleep 0.5
done
fail=0
[ -n "$key" ] || { echo "FAIL setup: the holder never took the lock"; rm -rf -- "${T:?}"; exit 1; }
[ -s "$key/start" ] && echo "ok   the lock records its holder's start time" || { echo "FAIL no start time in the lock"; fail=1; }
hp="$(cat "$key/pid")"
# Kill only the holder's own tree (its push and hook), never by pattern.
tree() { local p; for p in $(pgrep -P "$1" 2>/dev/null); do tree "$p"; done; echo "$1"; }
for p in $(tree "$hp"); do kill -9 "$p" 2>/dev/null; done; sleep 1
# The dead holder's pid now names an unrelated live process.
sleep 300 & other=$!
echo "$other" > "$key/pid"; date +%s > "$key/since"
rm -f .git/hooks/pre-push
start=$(date +%s)
AMUX_LAND_NOTIFY=0 AMUX_WORKER=lpr-next timeout 90 bash "$AM" land --no-batch --max-hold 3600 > "$T/next.out" 2>&1
took=$(( $(date +%s) - start ))
grep -q 'verdict=land_holder_pid_reused' "$T/next.out" && echo "ok   the reused pid is named" || { echo "FAIL no pid-reuse verdict: $(tail -3 "$T/next.out")"; fail=1; }
[ "$took" -lt 80 ] && echo "ok   taken over in ${took}s, not after --max-hold" || { echo "FAIL waited ${took}s"; fail=1; }
git fetch -q origin main; [ "$(git show origin/main:f)" = b ] && echo "ok   the next land pushed" || { echo "FAIL nothing pushed"; fail=1; }
kill "$other" 2>/dev/null; wait 2>/dev/null
rm -rf -- "${T:?}"
exit $fail
