#!/usr/bin/env bash
# test-land-regenerate-delete.sh [amux]: a regenerate that leaves a tracked file
# missing is refused and every tracked file kept (mixpeek-dev, 2026-11-17: a land's
# regenerate commit deleted docs/api-reference/openapi.json while a lane landed
# a dates-only change). amux.landRegenerateMayDelete true lets it through. A
# persistent extract another live land holds sends this run to a fresh one.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"; AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
fail=0; ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; fail=1; }
setup() { # <generator command>
  T="$(mktemp -d "${TMPDIR:-/tmp}/lrd.XXXXXX")"; export HOME="$T/home"; mkdir -p "$HOME/.amux/logs/land"; cd "$T"
  git init -q --bare --initial-branch=main origin.git
  git clone -q origin.git lane 2>/dev/null; cd lane
  git config user.email t@t; git config user.name t
  mkdir gen; echo a > in.txt; echo keep > gen/keep.txt; echo api > gen/openapi.json
  git add -A; git commit -qm base; git push -q origin HEAD:main 2>/dev/null
  git config --add amux.landRegenerable 'gen/*'
  git config --add amux.landRegenerateInput in.txt
  git config --add amux.landRegenerateInput gen
  git config amux.landRegenerate "$1"
  echo b > in.txt; git commit -qm "lane: change in.txt" -- in.txt
}
land() { AMUX_LAND_NOTIFY=0 AMUX_WORKER=lrd bash "$AM" land "$@" > "$T/out" 2>&1; }
on_main() { git -C "$T/lane" fetch -q origin main; git -C "$T/lane" cat-file -e "origin/main:$1" 2>/dev/null; }

# 1. A generator that deletes a tracked file: the render is refused and the
# stack lands unrendered, so main keeps the file (the caller's documented
# path for a failed regenerate).
setup 'rm -f gen/openapi.json'
land; rc=$?
{ [ "$rc" = 0 ] && on_main gen/openapi.json; } && ok "a deleting render is refused and main keeps the file" || bad "rc=$rc; $(tail -3 "$T/out")"
grep -q 'verdict=land_regenerate_refused_delete' "$HOME/.amux/logs/land.log" && grep -q 'gen/openapi.json' "$HOME/.amux/logs/land.log" \
  && ok "land.log names the refusal and the file" || bad "no refusal line: $(grep -a regenerate "$HOME/.amux/logs/land.log" | tail -3)"
[ "$(git -C "$T/lane" show origin/main:in.txt)" = b ] && ok "the lane's own change still lands, unrendered" || bad "the lane's change did not land"
rm -rf -- "${T:?}"

# 2. The repository says its generators may delete: it lands without the file.
setup 'rm -f gen/openapi.json'
git config amux.landRegenerateMayDelete true
land; rc=$?
{ [ "$rc" = 0 ] && ! on_main gen/openapi.json && on_main gen/keep.txt; } && ok "amux.landRegenerateMayDelete lets a generator delete" || bad "may-delete rc=$rc; $(tail -3 "$T/out")"
rm -rf -- "${T:?}"

# 3. A generator that rewrites files byte for byte: lands, nothing deleted.
setup 'echo api > gen/openapi.json'
land; rc=$?
{ [ "$rc" = 0 ] && on_main gen/openapi.json && [ "$(git -C "$T/lane" show origin/main:in.txt)" = b ]; } && ok "an identical rewrite lands and keeps the file" || bad "identical rc=$rc; $(tail -3 "$T/out")"
rm -rf -- "${T:?}"

# 4. Another live land holds the persistent extract: this run uses a fresh one.
setup 'echo api > gen/openapi.json'
sleep 300 & holder=$!
key="$(git rev-parse --path-format=absolute --git-common-dir | git hash-object --stdin | cut -c1-12)"
mkdir -p "$HOME/.amux/land-regen/$key-main.lock"; echo "$holder" > "$HOME/.amux/land-regen/$key-main.lock/pid"
land; rc=$?
kill "$holder" 2>/dev/null; wait "$holder" 2>/dev/null
{ [ "$rc" = 0 ] && grep -q 'verdict=land_regen_extract_busy' "$HOME/.amux/logs/land.log"; } && ok "a busy extract sends the run to a fresh one, and it lands" || bad "busy rc=$rc; $(grep -a 'regen' "$HOME/.amux/logs/land.log" | tail -3)"
[ "$(cat "$HOME/.amux/land-regen/$key-main.lock/pid" 2>/dev/null)" = "$holder" ] && ok "the other land's lock is left as it was" || bad "the holder's lock was taken"
rm -rf -- "${T:?}"

[ "$fail" = 0 ] && echo "test-land-regenerate-delete: all checks passed" || { echo "test-land-regenerate-delete: FAILED"; exit 1; }
