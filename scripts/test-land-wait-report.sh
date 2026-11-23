#!/usr/bin/env bash
# test-land-wait-report.sh [amux]: a waiter says again where it stands when its
# holder changes and every AMUX_LAND_WAIT_REPORT_S, and a holder whose batch is
# full names the waiters it leaves for the next one. On 2026-11-23 studio-plg's
# log held one "waiting" line for 5h13m, naming a holder that had exited hours
# before, while 7-waiter batches went ahead of it with nothing saying why.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lwr.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git base 2>/dev/null
git -C base config user.email t@t; git -C base config user.name t
echo a > base/f; git -C base add f; git -C base commit -qm base; git -C base push -q origin HEAD:main 2>/dev/null
# Worktrees of one repository: a batch takes only waiters that share its
# object store.
for l in l1 l2 l3; do
  git -C base worktree add -q --detach "$H/$l" origin/main 2>/dev/null
  echo "$l" > "$l/$l"; git -C "$l" add "$l"; git -C "$l" commit -qm "$l change"
done
key="$(printf '%s %s' "$(git -C l1 remote get-url origin)" main | shasum | cut -c1-16)"
lock="$H/.amux/locks/land-$key"; mkdir -p "$lock"
sleep 300 & holder=$!
echo "$holder" > "$lock/pid"; echo first-holder > "$lock/who"; date +%s > "$lock/since"
ps -o lstart= -p "$holder" | tr -s ' ' > "$lock/start"
git -C base config amux.landBatchMax 1
tickets() { local t c=0; for t in "$lock.q"/*; do [ -e "$t" ] && c=$((c + 1)); done; echo "$c"; }
pids=()
for l in l1 l2 l3; do
  n=$(tickets)
  (cd "$l" && HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_LAND_WAIT_REPORT_S=2 AMUX_LAND_WAIT_POLL_S=1 AMUX_WORKER="lwr-$l" \
     exec bash "$AM" land --tries 1 > "$H/$l.out" 2>&1) & pids+=($!)
  for _ in $(seq 1 30); do [ "$(tickets)" -gt "$n" ] && break; sleep 0.5; done
  sleep 1   # distinct arrival seconds, so the queue order is l1, l2, l3
done
fail=0
[ "$(tickets)" = 3 ] && echo "ok   three waiters queued" || { echo "FAIL setup: $(ls "$lock.q" 2>/dev/null)"; fail=1; }
sleep 7
grep -q 'still waiting after .*holder first-holder.*verdict=land_waiting' "$H/l3.out" \
  && echo "ok   a waiter reports its place again while it waits" || { echo "FAIL no repeat report: $(cat "$H/l3.out")"; fail=1; }
echo second-holder > "$lock/who"; sleep 7
grep -q 'holder second-holder (pid [0-9]*, new holder)' "$H/l3.out" \
  && echo "ok   a change of holder is reported" || { echo "FAIL holder change not reported: $(tail -3 "$H/l3.out")"; fail=1; }
kill "$holder" 2>/dev/null
for p in "${pids[@]}"; do
  for _ in $(seq 1 90); do kill -0 "$p" 2>/dev/null || break; sleep 1; done
done
grep -q 'batch full at 1 waiter(s); 1 more keep their places for the next holder: lwr-l3 verdict=land_batch_full max=1 left=1' "$H/l1.out" \
  && echo "ok   the holder names the waiter its full batch left behind" || { echo "FAIL no batch-full line: $(grep -a 'batch\|stack' "$H/l1.out")"; fail=1; }
git -C base fetch -q origin
for l in l1 l2 l3; do git -C base cat-file -e "origin/main:$l" 2>/dev/null || { echo "FAIL $l never landed"; fail=1; }; done
[ "$fail" = 0 ] && echo "ok   all three lanes landed"
kill "$holder" "${pids[@]}" 2>/dev/null; wait 2>/dev/null
cd / && rm -rf -- "${H:?}"
exit $fail
