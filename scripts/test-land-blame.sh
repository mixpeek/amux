#!/usr/bin/env bash
# test-land-blame.sh [amux]: a gate refusal names the waiter whose commit it
# refused, including the batch TIP when the tip is the subject of a detail line
# ("  <tip> drops 8 of the 8 lines ..."), and never the tip when it is only
# context ("red at <tip>"). gs12-retrievers' 18:52Z batch on 2026-10-02 dropped
# all three waiters because the tip was skipped and the indented detail lines
# never reached blame.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
F="$(mktemp "${TMPDIR:-/tmp}/blame.XXXXXX")"
n="$(grep -n '  _land_named_waiters() {' "$AM" | cut -d: -f1)"
{ echo '_land_file_named_waiters() { :; }'; sed -n '/^_grep_all() {/,/^}/p' "$AM"; sed -n "$n,/^  }/p" "$AM"; sed -n '/^  _land_blame_lines() {/,/^  }/p' "$AM"; } > "$F"
# shellcheck disable=SC1090
. "$F"; rm -f "$F"
fail=0
check() { local want="$1" out="$2" tip="$3" what="$4" got
  got="$(_land_named_waiters "$(_land_blame_lines "$out")" "$tip" | tr -s ' ' | sed 's/ $//')"
  if [ "$got" = "$want" ]; then echo "ok   $what -> [${got}]"; else echo "FAIL $what: got [$got], want [$want]"; fail=1; fi; }
# shellcheck disable=SC2034  # read by the sourced _land_named_waiters
stack_map="tkA aaaaaaaaaa11 1111111111aa
tkTip 675c52705d00 9999999999bb"
check tkTip 'tree-revert: ✗ REFUSED — a commit in this push reverts work its own parent introduced:

stale-copy-drop: ✗ a pushed commit drops lines a recent commit added, and does not say so:
  675c52705d drops 8 of the 8 lines b11ebc0af8 added to packages/javascript-sdk/api/byodocuments-api.ts
      b11ebc0af8 chore: regenerate JavaScript SDK v0.81.10
  A deliberate removal passes when the commit message names the commit it removes' 675c52705d00 "tip is the subject of an indented detail line"
check "" 'mirror-tests: ✗ FAILED on the bytes you are pushing — a module you changed has
mirror-tests:   tests, and they are red at 675c52705d00.' 675c52705d00 "tip only as context"
check tkA 'qdrant-refs: ✗ REFUSED
  aaaaaaaaaa11 adds 3 references' 675c52705d00 "a non-tip commit named on a detail line"
check "" 'ops-registry: ⚠ the registry is ALREADY failing at the base, so this is not a refusal
  aaaaaaaaaa11 touched it' 675c52705d00 "a line that says it is not a refusal"
exit $fail
