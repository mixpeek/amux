#!/usr/bin/env bash
# test-land-precheck-first.sh [amux]: `amux land` runs the repository's land
# precheck BEFORE the land gate, so a range the precheck refuses comes back in
# seconds with the gate never started, and a range it passes is gated and is
# not prechecked a second time at enqueue (2026-11-12: PR #448 passed a full
# gate, then was refused at push in 2 s by the slop ratchet).
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
H="$(mktemp -d "${TMPDIR:-/tmp}/lpf.XXXXXX")"; mkdir -p "$H/.amux/logs/land"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
mkdir -p scripts
# A stand-in gate that only records that it ran.
printf 'import os\nopen(os.environ["LPF_GATE_MARK"], "a").write("gate\\n")\n' > scripts/land_gate.py
: > scripts/land_review.py
echo a > f; git add f scripts; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
printf '#!/bin/sh\necho pre >> "%s/order"\n[ -f "%s/refuse" ] && { echo "leg: REFUSED"; exit 1; }\necho "land-precheck: passed"; exit 0\n' "$H" "$H" > "$H/chk.sh"
chmod +x "$H/chk.sh"
run() {
  HOME="$H" AMUX_LAND_PRECHECK="$H/chk.sh" LPF_GATE_MARK="$H/order" AMUX_LAND_NOTIFY=0 \
    AMUX_LAND_CLIENT=1 AMUX_WORKER=lpf bash "$AM" land --tries 1 --no-batch >/dev/null 2>&1
}
order() { tr '\n' ' ' < "$H/order"; }
fail=0

echo b > f; git commit -qm one -- f
touch "$H/refuse"; : > "$H/order"
run; rc=$?
[ "$rc" = 3 ] && echo "ok   a refused precheck stops the land (rc 3)" || { echo "FAIL rc $rc, want 3"; fail=1; }
[ "$(order)" = "pre " ] && echo "ok   and the gate never started" || { echo "FAIL order: $(order)"; fail=1; }

rm -f "$H/refuse"; : > "$H/order"
run; rc=$?
[ "$(order)" = "pre gate " ] && echo "ok   a passing precheck runs once, before the gate" || { echo "FAIL order: $(order)"; fail=1; }
[ "$rc" = 0 ] && [ "$(git --git-dir="$H/origin.git" show main:f 2>/dev/null)" = b ] && echo "ok   and the range lands" || { echo "FAIL rc $rc, not landed"; fail=1; }

cd / && rm -rf -- "${H:?}"
exit $fail
