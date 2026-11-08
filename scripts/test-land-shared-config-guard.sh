#!/usr/bin/env bash
# test-land-shared-config-guard.sh [amux]: amux land refuses to queue when the
# repository's SHARED git config sets core.worktree (a lane's scratch script
# wrote one into ~/Dev/mixpeek/.git/config on 2026-10-03, and every lane's git
# read another lane's scratchpad as its tree), and names the value. A clean
# config still queues.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lsc.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
echo a > f; git add f; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
echo b > f; git commit -qm change -- f
fail=0
git config core.worktree /tmp/some-other-lanes-scratch
out="$(HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lsc bash "$AM" land 2>&1)"; rc=$?
[ "$rc" = 2 ] && printf '%s' "$out" | grep -q 'core.worktree=/tmp/some-other-lanes-scratch' \
  && echo "ok   refused, naming core.worktree's value" || { echo "FAIL rc=$rc: $(printf '%s' "$out" | tail -2)"; fail=1; }
git fetch -q origin; [ "$(git rev-parse origin/main)" != "$(git rev-parse HEAD)" ] && echo "ok   nothing pushed" || { echo "FAIL it pushed"; fail=1; }
git config --unset core.worktree
# core.bare=true (found on the amux checkout 2026-10-31): refused, and the
# remedy it prints is the one that clears it, not --unset core.worktree.
git config core.bare true
out="$(HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lsc bash "$AM" land 2>&1)"; rc=$?
fix="$(printf '%s' "$out" | sed -n 's/.*Fix it (\(.*\)) and land again.*/\1/p' | head -1)"
[ "$rc" = 2 ] && printf '%s' "$fix" | grep -q 'core.bare false' && ! printf '%s' "$fix" | grep -q 'core.worktree' \
  && echo "ok   core.bare=true is refused and the remedy names core.bare" || { echo "FAIL bare rc=$rc fix=[$fix]: $(printf '%s' "$out" | tail -2)"; fail=1; }
eval "$fix"
[ "$(git config --get core.bare)" = false ] && echo "ok   running the printed remedy clears it" || { echo "FAIL the remedy left core.bare=$(git config --get core.bare)"; fail=1; }
HOME="$H" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lsc bash "$AM" land >/dev/null 2>&1; rc2=$?
git fetch -q origin
# Compare content, not shas: land replays (a new sha) rather than rebasing the
# lane's checkout since .41.
[ "$rc2" = 0 ] && [ "$(git show origin/main:f)" = b ] && echo "ok   a clean shared config lands" || { echo "FAIL clean config rc=$rc2"; fail=1; }
cd / && rm -rf -- "${H:?}"
exit $fail
