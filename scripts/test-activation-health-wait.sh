#!/usr/bin/env bash
# The builder waits for a new build to go live before keeping it. The server
# holds adoption while a shell schedule run is in flight, so past the health
# limit the wait continues while a run is in flight, up to the server's hold
# cap, and a build that goes live in that time is kept. With no run in flight
# a build that never goes live is still rolled back at the limit. Runs the
# shipped function lifted from rust-auto-build.sh, not a paraphrase.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
expect() { if "$@"; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); echo "FAIL: $*"; fi; }

awk '/^wait_live_identity\(\) \{/,/^}$/' "$ROOT/scripts/rust-auto-build.sh" > "$TMP/fn.sh"
expect test -s "$TMP/fn.sh"

# $1 live after this many polls (never = -1), $2 the in-flight runs, $3 limit, $4 hold.
run() {
  printf '%s' "$2" > "$TMP/runs"
  AMUX_ADOPT_SHELL_DEFER_MAX_S="$4" bash -c '
    calls=0
    sleep() { :; }   # one poll is one second in the builder; the test does not wait them out
    measure_live_identity() { calls=$((calls + 1)); live=old111; if [ "'"$1"'" -ge 0 ] && [ "$calls" -gt "'"$1"'" ]; then live=abc123; fi; }
    shell_runs_in_flight() { cat "'"$TMP/runs"'"; }
    source "'"$TMP/fn.sh"'"
    if wait_live_identity abc123def "'"$3"'"; then echo "rc=0 waited=$waited"; else echo "rc=1 waited=$waited"; fi'
}

# Live at once: kept, no wait.
out=$(run 0 "" 5 20)
expect test "$(echo "$out" | tail -1)" = "rc=0 waited=0"

# Never live, nothing in flight: rolled back at the limit, as before.
out=$(run -1 "" 5 20)
expect test "$(echo "$out" | tail -1)" = "rc=1 waited=5"
expect test -z "$(echo "$out" | grep activation_wait_shell_hold || true)"

# Never live, a shell run in flight: waits out the hold, says so once, then gives up.
out=$(run -1 "SCHED-662" 5 20)
expect test "$(echo "$out" | tail -1)" = "rc=1 waited=25"
expect test "$(echo "$out" | grep -c 'verdict=activation_wait_shell_hold')" = 1
expect grep -q 'SCHED-662' <<<"$out"

# The c71df3ef9 case: the server adopts after the limit because a shell run held it. Kept.
out=$(run 12 "SCHED-662" 5 20)
expect test "$(echo "$out" | tail -1)" = "rc=0 waited=12"

echo "activation health wait: $PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
