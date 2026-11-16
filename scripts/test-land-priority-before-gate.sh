#!/usr/bin/env bash
# test-land-priority-before-gate.sh [amux]: `amux land --priority` is refused
# before the land gate runs when it has no --reason or names no grant
# (2026-11-16: the refusal came after a full 7-minute gate).
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lpg.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
mkdir -p scripts
# A stand-in gate: it counts its runs and passes.
printf 'import os\nopen(os.path.join(os.environ["HOME"], "gate-runs"), "a").write("1\\n")\n' > scripts/land_gate.py
cp scripts/land_gate.py scripts/land_review.py
echo a > f; git add f scripts; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
echo b > f; git commit -qm one -- f
fail=0
runs() { grep -c 1 "$H/gate-runs" 2>/dev/null || echo 0; }
run() { HOME="$H" CC_HOME="$H/.amux" AMUX_HOME="$H/.amux" AMUX_URL="http://127.0.0.1:9" AMUX_LAND_NOTIFY=0 \
  AMUX_LAND_PRECHECK=true AMUX_WORKER=lane-x bash "$AM" land --tries 1 --no-batch "$@" 2>"$H/err"; }
run --priority; rc=$?
[ "$rc" = 2 ] && echo "ok   --priority with no --reason is refused (rc 2)" || { echo "FAIL rc=$rc"; fail=1; }
grep -q 'needs --reason' "$H/err" && echo "ok   the refusal says --reason is needed" || { echo "FAIL no reason message"; cat "$H/err"; fail=1; }
[ "$(runs)" = 0 ] && echo "ok   the gate did not run first" || { echo "FAIL the gate ran $(runs) time(s) before the refusal"; fail=1; }
run --priority --reason "restores some site files"; rc=$?
[ "$rc" = 2 ] && [ "$(runs)" = 0 ] && echo "ok   a reason that cites no grant is refused before the gate" || { echo "FAIL rc=$rc gate runs=$(runs)"; fail=1; }
run; rc=$?
[ "$(runs)" -ge 1 ] && echo "ok   control: an ordinary land runs the gate (rc $rc)" || { echo "FAIL control: the gate never ran, so the checks above prove nothing"; cat "$H/err"; fail=1; }
cd / && rm -rf -- "${H:?}"
exit $fail
