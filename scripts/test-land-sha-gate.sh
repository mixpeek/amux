#!/usr/bin/env bash
# test-land-sha-gate.sh [amux]: `amux land --sha <HEAD>` runs the land gate as
# a plain land does, and a --sha naming another commit says the gate did not
# run on it. Before this, any --sha skipped the gate while the pre-push hook
# still refused a commit with no receipt, so `amux land --detach --sha HEAD`
# queued and was then refused at the push (2026-11-25, PR #493's land).
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lsg.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
mkdir -p scripts
# A stand-in gate that only records that it ran.
printf 'import os\nopen(os.environ["LSG_GATE_MARK"], "a").write("gate\\n")\n' > scripts/land_gate.py
: > scripts/land_review.py
echo a > f; git add f scripts; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
run() {
  HOME="$H" LSG_GATE_MARK="$H/mark" AMUX_LAND_NOTIFY=0 AMUX_LAND_CLIENT=1 AMUX_WORKER=lsg \
    bash "$AM" land --tries 1 --no-batch "$@" > "$H/out" 2>&1
}
fail=0

echo b > f; git commit -qm one -- f
: > "$H/mark"
run --sha "$(git rev-parse HEAD)"; rc=$?
grep -q gate "$H/mark" && echo "ok   --sha HEAD runs the land gate" || { echo "FAIL the gate did not run for --sha HEAD (rc $rc): $(tail -3 "$H/out")"; fail=1; }
[ "$rc" = 0 ] && [ "$(git --git-dir="$H/origin.git" show main:f 2>/dev/null)" = b ] && echo "ok   and it lands" || { echo "FAIL rc $rc, not landed: $(tail -3 "$H/out")"; fail=1; }

echo c > f; git commit -qm two -- f
echo d > g; git add g; git commit -qm three -- g
: > "$H/mark"
run --sha "$(git rev-parse HEAD~1)"; rc=$?
grep -q gate "$H/mark" && { echo "FAIL the gate ran for a --sha that is not HEAD"; fail=1; } || echo "ok   a --sha that is not HEAD does not run the gate here"
grep -q 'verdict=land_gate_skipped_sha' "$H/out" && echo "ok   and says so" || { echo "FAIL no land_gate_skipped_sha line: $(tail -3 "$H/out")"; fail=1; }

cd / && rm -rf -- "${H:?}"
exit $fail
