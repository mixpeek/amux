#!/usr/bin/env bash
# test-grep-all-pipefail.sh [amux]: `printf | grep -q` under pipefail reads a
# match as a miss when grep -q exits before the writer is done (SIGPIPE).
# mixpeek-cicd, 2026-11-17: in _land_regenerate that removed files the
# generators had kept (5 of 153 regenerate commits in 30 days). The amux CLI
# routes every `| grep -q` through _grep_all, which reads all of its input.
# The control loop shows a plain grep -q missing, so the check can fail.
set -euo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
F="$(mktemp)"; trap 'rm -f "$F"' EXIT
sed -n '/^_grep_all() {/,/^}/p' "$AM" > "$F"
[ -s "$F" ] || { echo "FAIL _grep_all is not in $AM"; exit 1; }
. "$F"
# 200 KB of paths, the wanted one first: a writer this long is almost always
# still writing when grep -q has its match.
list="target.json"; for i in $(seq 1 12000); do list="$list
gen/file-$i.json"; done
N=${N:-300}; plain=0; fixed=0
for _ in $(seq 1 "$N"); do
  { printf '%s\n' "$list" 2>/dev/null; } | grep -qxF -- target.json || plain=$((plain+1))
  printf '%s\n' "$list" | _grep_all -qxF -- target.json || fixed=$((fixed+1))
done
echo "of $N: plain grep -q missed $plain, _grep_all missed $fixed"
fail=0
[ "$plain" -gt 0 ] && echo "ok   control: the race is real here (plain grep -q missed a match)" || { echo "FAIL control: plain grep -q never missed, so this test cannot fail"; fail=1; }
[ "$fixed" = 0 ] && echo "ok   _grep_all never reads a match as a miss" || { echo "FAIL _grep_all missed $fixed"; fail=1; }
# Flags: -q in any position of the first word is dropped, others kept.
printf 'Authorization: x\n' | _grep_all -qi '^authorization:' && echo "ok   -qi keeps -i" || { echo "FAIL -qi"; fail=1; }
printf 'a\n' | _grep_all -q b && { echo "FAIL a non-match read as a match"; fail=1; } || echo "ok   a real non-match is still a miss"
# Nothing in the CLI pipes into a bare grep -q any more.
left="$(grep -v '^[[:space:]]*#' "$AM" | grep -c '| *grep -q' || true)"
[ "$left" = 0 ] && echo "ok   no '| grep -q' is left in the CLI" || { echo "FAIL $left '| grep -q' left in the CLI"; fail=1; }
[ "$fail" = 0 ] && echo "test-grep-all-pipefail: all checks passed" || { echo "test-grep-all-pipefail: FAILED"; exit 1; }
