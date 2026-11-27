#!/usr/bin/env bash
# test-land-batch-other-clone.sh [amux]: a holder batches a waiter that lands
# from ANOTHER CLONE of the same remote, by fetching its commit from that
# clone (mixpeek-meta-worker, 2026-11-27: on mixpeek-dev every lane lands from
# its own clone, every batch skipped them as "another repository", and a queue
# 26 deep drained one ticket per hold).
# `first` (clone a) holds the lock in a push hook, `holder` (worktree b of a)
# queues next, `waiter` (clone c) queues behind it. When `first` lands,
# `holder` must carry the waiter's commit in its batch.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
T="$(mktemp -d "${TMPDIR:-/tmp}/lbo.XXXXXX")"
T="$(cd "$T" && pwd -P)"
export HOME="$T/home"; mkdir -p "$HOME"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export AMUX_LAND_NOTIFY=0 AMUX_LAND_REMOTE_LEASE=off
cd "$T" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q "$T/origin.git" a 2>/dev/null
echo base > a/base; git -C a add base; git -C a commit -qm base; git -C a push -q origin HEAD:main 2>/dev/null
git -C a worktree add -q --detach "$T/b" origin/main
git clone -q "$T/origin.git" c 2>/dev/null
cat > a/.git/hooks/pre-push <<EOF
#!/bin/sh
n=\$(( \$(cat "$T/n" 2>/dev/null || echo 0) + 1 )); echo \$n > "$T/n"
while [ -e "$T/gate\$n" ]; do sleep 0.2; done
EOF
chmod +x a/.git/hooks/pre-push
touch "$T/gate1"
echo 1 > a/one; git -C a add one; git -C a commit -qm "first change"
echo 2 > b/two; git -C b add two; git -C b commit -qm "holder change"
echo 3 > c/three; git -C c add three; git -C c commit -qm "waiter change"
wsha="$(git -C c rev-parse --short=10 HEAD)"
fail=0
cleanup() { rm -f "$T/gate1"; sleep 1; pkill -f "$T/" 2>/dev/null; wait 2>/dev/null; rm -rf -- "${T:?}"; }
waitfor() { local i; for i in $(seq 1 120); do eval "$1" && return 0; sleep 0.5; done; return 1; }

(cd a && AMUX_WORKER=first bash "$AM" land --no-batch > "$T/first.out" 2>&1) &
waitfor '[ "$(cat "$T/n" 2>/dev/null)" = 1 ]' || { echo "FAIL setup: first never reached its push"; cat "$T/first.out"; cleanup; exit 1; }
(cd b && AMUX_WORKER=holder bash "$AM" land > "$T/holder.out" 2>&1) &
waitfor 'q="$(ls -d "$HOME"/.amux/locks/land-*.q 2>/dev/null | head -1)"; [ -n "$q" ] && ls "$q" | grep -q .' \
  || { echo "FAIL setup: holder never queued"; cat "$T/holder.out"; cleanup; exit 1; }
q="$(ls -d "$HOME"/.amux/locks/land-*.q | head -1)"
sleep 1.1   # tickets sort by arrival time; keep the waiter strictly behind
(cd c && AMUX_WORKER=waiter bash "$AM" land > "$T/waiter.out" 2>&1) &
waitfor '[ "$(ls "$q" | wc -l | tr -d " ")" -ge 2 ]' || { echo "FAIL setup: waiter never queued in the same queue"; ls -d "$HOME"/.amux/locks/*; cat "$T/waiter.out"; cleanup; exit 1; }

rm -f "$T/gate1"
waitfor 'grep -q "holder landed .* on main" "$HOME/.amux/logs/land.log"' || { echo "FAIL holder never landed"; tail -5 "$T/holder.out"; cleanup; exit 1; }
log="$HOME/.amux/logs/land.log"
grep -q "verdict=land_batch_fetched_other_clone" "$log" \
  && echo "ok   the holder fetched the waiter's commit from its clone" || { echo "FAIL no fetch verdict"; grep -a 'batch' "$log"; fail=1; }
grep -q "holder landed .* on main .*with waiter" "$log" \
  && echo "ok   the holder's batch carried the waiter" || { echo "FAIL the waiter was not batched: $(grep -a 'holder landed\|batch skipped' "$log")"; fail=1; }
git --git-dir="$T/origin.git" cat-file -e main:three 2>/dev/null \
  && echo "ok   the waiter's change is on main" || { echo "FAIL main lacks the waiter's change"; fail=1; }
waitfor 'grep -q "waiter landed in a batch" "$log"' \
  && echo "ok   the waiter reports it landed in the batch" || { echo "FAIL waiter did not report the batch: $(tail -3 "$T/waiter.out")"; fail=1; }
cleanup
exit $fail
