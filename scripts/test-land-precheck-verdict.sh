#!/usr/bin/env bash
# test-land-precheck-verdict.sh [amux]: a precheck killed at its budget is
# UNMEASURED even though its kill line ends "(the legs before it passed)".
# The old verdict matched "passed" and sent a killed check's batch to a full
# gate (gs12-retrievers 18:23Z, 2026-10-02). Each case names what it pins.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
F="$(mktemp "${TMPDIR:-/tmp}/verdict.XXXXXX")"
{ sed -n '/^_grep_all() {/,/^}/p' "$AM"; sed -n '/^_land_precheck_verdict() {/,/^}/p' "$AM"; } > "$F"
# shellcheck disable=SC1090
. "$F"; rm -f "$F"
fail=0
check() { local want="$1" rc="$2" out="$3" what="$4" got; got="$(_land_precheck_verdict "$rc" "$out")"
  if [ "$got" = "$want" ]; then echo "ok   $what -> $got"; else echo "FAIL $what: got $got, want $want"; fail=1; fi; }
check unmeasured 142 'land-precheck: UNMEASURED: stopped by SIGTERM after 419s during an unlabelled leg; no verdict was reached (the legs before it passed)' "killed check whose kill line says the legs before it passed"
check pass 142 'land-precheck: all legs passed in 300s' "killed after a genuine pass line"
check refuse 143 'FAIL qdrant-refs: stale tip' "killed after a FAIL"
check pass 0 'land precheck stops here' "clean exit"
check refuse 1 'qdrant-refs: refused' "non-zero exit, not killed"
exit $fail
