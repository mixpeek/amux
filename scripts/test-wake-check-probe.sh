#!/usr/bin/env bash
# test-wake-check-probe.sh [amux]: `amux wake --when` runs the check once the
# way the scheduler will (bash -o pipefail from /) and names a pipefail trap,
# a relative path, a broken exit and a timeout (mixpeek-audit, 2026-11-28:
# SCHED-3438's `cat land.log land.log.1 | awk` read "not yet" for ~75 min
# after awk matched). The helper is called directly, so no schedule is made.
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
T="$(mktemp -d "${TMPDIR:-/tmp}/wcp.XXXXXX")"
eval "$(awk '/^_wake_check_probe\(\) \{/{p=1} p{print} p&&/^\}/{exit}' "$AM")"
declare -F _wake_check_probe >/dev/null || { echo "FAIL _wake_check_probe not found in $AM"; exit 1; }
echo landed > "$T/land.log"
fail=0
check() { # <label> <check> <expected stderr fragment>
  local got; got="$(set -e; _wake_check_probe "$2" 2>&1)"
  case "$got" in *"$3"*) echo "ok   $1" ;; *) echo "FAIL $1: $got"; fail=1; return 1 ;; esac
}
check "pipefail: an early stage failing reads as pipefail, not 'not yet'" \
  "cat $T/land.log $T/land.log.1 | awk '/landed/{f=1} END{exit !f}'" "verdict=wake_check_pipefail"
check "pipefail: the failing stage's message is shown" \
  "cat $T/land.log $T/land.log.1 | awk '/landed/{f=1} END{exit !f}'" "land.log.1"
check "a passing check says it passes" "grep -q landed $T/land.log" "the check passes now (exit 0"
# Run from the directory holding land.log: only the probe's own `cd /` makes
# the relative path miss, as it does for the server.
(cd "$T" && check "a relative path runs from / and reads not yet" "test -e land.log" 'reads "not yet" now (exit 1') || fail=1
check "an exit other than 0 or 1 is broken" "exit 3" "verdict=wake_check_broken: the check exited 3"
AMUX_WAKE_PROBE_TIMEOUT_S=1 check "a check past the limit is a timeout" "sleep 5" "verdict=wake_check_timeout"
_wake_check_probe "exit 2" 2>/dev/null; [ $? = 0 ] && echo "ok   the probe never fails the wake (rc 0)" || { echo "FAIL the probe returned non-zero"; fail=1; }
rm -rf -- "${T:?}"
exit $fail
