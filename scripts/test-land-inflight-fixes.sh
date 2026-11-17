#!/usr/bin/env bash
# test-land-inflight-fixes.sh [amux]: a precheck refusal that names a file a
# queued ticket from another lane already changes says so ("already being
# fixed"), so the refused lane does not write the same fix (2026-10-04: four
# lanes each fixed one unmarked test).
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
H="$(mktemp -d "${TMPDIR:-/tmp}/lif.XXXXXX")"
cd "$H" || exit 1
git init -q --bare --initial-branch=main origin.git
git clone -q origin.git lane 2>/dev/null; cd lane || exit 1
git config user.email t@t; git config user.name t
echo a > t.py; echo a > other.py; git add t.py other.py; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
echo b > t.py; git commit -qm "fix t.py" -- t.py; FIX="$(git rev-parse HEAD)"
git fetch -q origin
key="$(printf '%s %s' "$(git remote get-url origin)" main | shasum | cut -c1-16)"
q="$H/.amux/locks/land-$key.q"; mkdir -p "$q"
printf 'lane-cicd\n%s\n%s\n1\n' "$FIX" "$(git rev-parse --git-common-dir)" > "$q/001791000000-111"
F="$(mktemp "${TMPDIR:-/tmp}/liff.XXXXXX")"
{ sed -n '/^_grep_all() {/,/^}/p' "$AM"; sed -n '/^_land_inflight_fixes() {/,/^}/p' "$AM"; } > "$F"
fail=0
[ -s "$F" ] || { echo "FAIL _land_inflight_fixes not found"; exit 1; }
run() { HOME="$H" bash -c ". '$F'; _land_inflight_fixes origin main \"\$1\" \"\$2\"" _ "$1" "$2"; }
out="$(run 'unit-marker: server/x/t.py has no marker; t.py refused' lane-obs)"
printf '%s' "$out" | grep -q "lane-cicd's queued ticket 001791000000-111 changes t.py" && echo "ok   a refusal naming the file points at the queued fix" || { echo "FAIL: $out"; fail=1; }
out="$(run 'leg: other.py refused' lane-obs)"
[ -z "$out" ] && echo "ok   a refusal about another file names nothing" || { echo "FAIL unrelated: $out"; fail=1; }
out="$(run 'unit-marker: t.py refused' lane-cicd)"
[ -z "$out" ] && echo "ok   the lane's own ticket is not reported to it" || { echo "FAIL self: $out"; fail=1; }
rm -f "$F"; cd / && rm -rf -- "${H:?}"
exit $fail
