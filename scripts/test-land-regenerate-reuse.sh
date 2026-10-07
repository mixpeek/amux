#!/usr/bin/env bash
# test-land-regenerate-reuse.sh [amux]: the regenerate step reuses one sparse
# worktree per repo and branch instead of extracting a fresh tree into TMPDIR
# on every land (fseventsd escalation 20261007-030851: each land wrote and
# deleted tens of thousands of files). Two lands in a row: the second reuses
# the tree, both push a fresh render, nothing is left in TMPDIR, and
# AMUX_LAND_REGEN_REUSE=0 still lands through the old fresh extract.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lrr.XXXXXX")"; mkdir -p "$H/.amux/logs/land" "$H/tmp"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
mkdir gen; echo a > a.txt; echo keep > other.txt
printf 'cksum < a.txt > gen/a.out\n' > gen.sh
cksum < a.txt > gen/a.out
git add a.txt other.txt gen.sh gen/a.out; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
git config --add amux.landRegenerable 'gen/*'
git config --add amux.landRegenerate 'sh gen.sh'
for p in a.txt gen.sh gen; do git config --add amux.landRegenerateInput "$p"; done
land() { HOME="$H" CC_HOME="$H/.amux" AMUX_HOME="$H/.amux" TMPDIR="$H/tmp" AMUX_LAND_NOTIFY=0 AMUX_WORKER=lrr "$@" bash "$AM" land --tries 1 --no-batch >/dev/null 2>&1; }
fail=0
echo a2 > a.txt; git commit -qm one -- a.txt; land env; rc=$?; git fetch -q origin
[ "$rc" = 0 ] && [ "$(git show origin/main:gen/a.out)" = "$(echo a2 | cksum)" ] && echo "ok   first land pushes a fresh render" || { echo "FAIL first land rc=$rc"; fail=1; }
git rebase -q origin/main 2>/dev/null
echo a3 > a.txt; git commit -qm two -- a.txt; land env; rc=$?; git fetch -q origin
[ "$rc" = 0 ] && [ "$(git show origin/main:gen/a.out)" = "$(echo a3 | cksum)" ] && echo "ok   second land pushes a fresh render" || { echo "FAIL second land rc=$rc: $(grep -a regenerate "$H/.amux/logs/land.log" | tail -3)"; fail=1; }
[ "$(grep -ac 'verdict=land_regen_tree_reused' "$H/.amux/logs/land.log")" -ge 2 ] && echo "ok   the regenerate tree is reused across lands" || { echo "FAIL no reuse: $(grep -a 'regenerate tree' "$H/.amux/logs/land.log")"; fail=1; }
[ -z "$(ls -A "$H/tmp" | grep amux-land-regen)" ] && echo "ok   nothing is extracted into TMPDIR" || { echo "FAIL TMPDIR has $(ls "$H/tmp")"; fail=1; }
[ ! -e .git/amux-land-regen/main/other.txt ] && echo "ok   the tree holds only the declared inputs" || { echo "FAIL other.txt materialized"; fail=1; }
git rebase -q origin/main 2>/dev/null
echo a4 > a.txt; git commit -qm three -- a.txt; land env AMUX_LAND_REGEN_REUSE=0; rc=$?; git fetch -q origin
[ "$rc" = 0 ] && [ "$(git show origin/main:gen/a.out)" = "$(echo a4 | cksum)" ] && echo "ok   AMUX_LAND_REGEN_REUSE=0 lands through the fresh extract" || { echo "FAIL opt-out rc=$rc"; fail=1; }
cd / && rm -rf -- "${H:?}"
exit $fail
