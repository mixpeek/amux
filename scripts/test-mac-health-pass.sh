#!/bin/bash
# scripts/mac-health-pass.sh, the Mac health check's pass check: it passes
# when the last tick finished recently and says what is still over a floor.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); S="$HERE/mac-health-pass.sh"
FIX=$(mktemp -d); trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
run() { local rc=0; MAC_HEALTH_LOG="$FIX/tick.last" MAC_HEALTH_MAX_AGE_MIN=120 bash "$S" > "$FIX/out" 2>&1 || rc=$?; echo "$rc"; }

check "no tick yet fails" 1 "$(run)"
printf 'mac-cleanup: disk 80G\nmac-cleanup: constraint memory kernel pressure 2 after the purge arm — escalated to amux\n' > "$FIX/tick.last"
check "a tick that did not finish fails" 1 "$(run)"
check "and says so" yes "$(grep -q 'did not finish' "$FIX/out" && echo yes || echo no)"
printf 'mac-cleanup: constraint disk free 83.4G is under the 150G floor — escalation suppressed\nmac-cleanup: constraint memory kernel pressure 2 after the purge arm — escalated to amux (x)\nmac-cleanup: done elapsed=1s constraints=2\n' > "$FIX/tick.last"
check "a finished tick passes even with constraints" 0 "$(run)"
check "its first line names each floor still exceeded" "Still over a floor, already escalated to its owner: disk free 83.4G is under the 150G floor; memory kernel pressure 2 after the purge arm" "$(head -1 "$FIX/out" | sed 's/^Ran [0-9:]* and cleaned up\. //')"
printf 'mac-cleanup: done elapsed=1s constraints=0\n' > "$FIX/tick.last"
check "a clean tick passes" 0 "$(run)"
check "and says all clear" yes "$(head -1 "$FIX/out" | grep -q 'all clear' && echo yes || echo no)"
touch -t 202001010000 "$FIX/tick.last"
check "a finished tick that is too old fails" 1 "$(run)"
echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-health-pass — all checks passed"; exit 0; fi
echo "mac-health-pass: $fails check(s) FAILED"; exit 1
