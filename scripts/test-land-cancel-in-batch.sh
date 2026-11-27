#!/usr/bin/env bash
# test-land-cancel-in-batch.sh [amux]: a cancel of a waiter whose commits a
# holder has already composed into its batch says so, and --status names the
# tickets a batch in flight carries (mxp-gs12, 2026-11-27: `--cancel 3109689`
# printed "cancelled" and its five commits landed in 9accd0304a minutes later).
# Three lanes share one repository: `first` holds the lock in a push hook,
# `holder` queues next, `waiter` queues detached behind it. When `first`
# lands, `holder` composes a batch with `waiter` and blocks in its own push
# hook; `waiter` cancels then.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
T="$(mktemp -d "${TMPDIR:-/tmp}/lcb.XXXXXX")"
export HOME="$T/home"; mkdir -p "$HOME"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export AMUX_LAND_NOTIFY=0 AMUX_LAND_REMOTE_LEASE=off
cd "$T" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git a 2>/dev/null
echo base > a/base; git -C a add base; git -C a commit -qm base; git -C a push -q origin HEAD:main 2>/dev/null
git -C a worktree add -q --detach "$T/b" origin/main
git -C a worktree add -q --detach "$T/c" origin/main
# Push n waits while $T/gate<n> exists: push 1 is `first`, push 2 the batch.
cat > a/.git/hooks/pre-push <<EOF
#!/bin/sh
n=\$(( \$(cat "$T/n" 2>/dev/null || echo 0) + 1 )); echo \$n > "$T/n"
while [ -e "$T/gate\$n" ]; do sleep 0.2; done
EOF
chmod +x a/.git/hooks/pre-push
touch "$T/gate1" "$T/gate2"
echo 1 > a/one; git -C a add one; git -C a commit -qm "first change"
echo 2 > b/two; git -C b add two; git -C b commit -qm "holder change"
echo 3 > c/three; git -C c add three; git -C c commit -qm "waiter change"
wsha="$(git -C c rev-parse --short=10 HEAD)"
fail=0
cleanup() { rm -f "$T/gate1" "$T/gate2"; sleep 1; pkill -f "$T/" 2>/dev/null; wait 2>/dev/null; rm -rf -- "${T:?}"; }
waitfor() { local i; for i in $(seq 1 120); do eval "$1" && return 0; sleep 0.5; done; return 1; }

(cd a && AMUX_WORKER=first bash "$AM" land --no-batch > "$T/first.out" 2>&1) &
waitfor '[ "$(cat "$T/n" 2>/dev/null)" = 1 ]' || { echo "FAIL setup: first never reached its push"; cat "$T/first.out"; cleanup; exit 1; }
(cd b && AMUX_WORKER=holder bash "$AM" land > "$T/holder.out" 2>&1) &
waitfor 'q="$(ls -d "$HOME"/.amux/locks/land-*.q 2>/dev/null | head -1)"; [ -n "$q" ] && ls "$q" | grep -q .' \
  || { echo "FAIL setup: holder never queued"; cat "$T/holder.out"; cleanup; exit 1; }
q="$(ls -d "$HOME"/.amux/locks/land-*.q | head -1)"
sleep 1.1   # tickets sort by arrival time; keep the waiter strictly behind
(cd c && AMUX_WORKER=waiter bash "$AM" land --detach > "$T/waiter.out" 2>&1)
waitfor '[ "$(ls "$q" | wc -l | tr -d " ")" -ge 2 ]' || { echo "FAIL setup: waiter never queued"; cat "$T/waiter.out"; cleanup; exit 1; }
dpid="$(ls "$HOME/.amux/locks/detached" | head -1)"
[ -n "$dpid" ] || { echo "FAIL setup: no detached record"; cat "$T/waiter.out"; cleanup; exit 1; }

rm -f "$T/gate1"
waitfor '[ "$(cat "$T/n" 2>/dev/null)" = 2 ]' || { echo "FAIL setup: holder never reached its push"; tail -5 "$T/holder.out"; cleanup; exit 1; }
ls -a "$q" | grep -q '^\.inbatch-' && echo "ok   the holder recorded the waiter it batched" || { echo "FAIL no .inbatch record in the queue"; fail=1; }

st="$(cd c && AMUX_WORKER=waiter bash "$AM" land --status 2>&1)"
printf '%s\n' "$st" | grep -q "in holder's batch in flight: waiter (pid .*, $wsha)" \
  && echo "ok   --status names the batched ticket, its lane and commit" || { echo "FAIL --status: $st"; fail=1; }

out="$(cd c && AMUX_WORKER=waiter bash "$AM" land --cancel "$dpid" 2>&1)"
printf '%s\n' "$out" | grep -q "cancelled detached land pid $dpid" && echo "ok   the cancel stopped the waiter" || { echo "FAIL cancel: $out"; fail=1; }
printf '%s\n' "$out" | grep -q "verdict=land_cancel_in_batch: its commits ($wsha) are already in holder's batch in flight" \
  && echo "ok   the cancel says the commits ride holder's batch" || { echo "FAIL the cancel did not name the batch: $out"; fail=1; }
grep -q "WARN verdict=land_cancel_in_batch: cancelled waiter pid $dpid's ticket" "$HOME/.amux/logs/land.log" \
  && echo "ok   land.log records it" || { echo "FAIL land.log line"; fail=1; }

rm -f "$T/gate2"
waitfor 'git --git-dir="$T/origin.git" cat-file -e main:three 2>/dev/null' \
  && echo "ok   (as the warning said) the waiter's change landed with the batch" || { echo "FAIL the batch did not land the waiter's change"; tail -5 "$T/holder.out"; fail=1; }
waitfor '! ls -a "$q" 2>/dev/null | grep -q "^\.inbatch-"' && echo "ok   the record is gone once the batch reports" || { echo "FAIL stale .inbatch left: $(ls -a "$q")"; fail=1; }
cleanup
exit $fail
