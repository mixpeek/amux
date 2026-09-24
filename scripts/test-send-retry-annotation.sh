#!/usr/bin/env bash
# Proof for AF-556: `amux send`'s retry annotation names the attempt and the wait.
#
# EXTRACTS THE PYTHON FROM THE SHIPPED `amux`, rather than restating it. A
# control built from the same text the code is built from cannot fail — which is
# exactly the defect being fixed here, one layer up: the old line read `$_R` with
# an empty-string default, so no input could change its output and it still read
# as a measured attempt number.
#
# The retry path only runs when the server is unreachable on the first try, so
# there is no way to exercise it end to end without taking the server down. This
# drives the block's own bytes with the environment the shell hands it.
set -euo pipefail
cd "$(dirname "$0")/.."

CELLS=0
FAILED=0
check() {  # check <label> <expected-substring> <haystack>
  CELLS=$((CELLS + 1))
  if printf '%s' "$3" | grep -qF -- "$2"; then
    echo "  ok    $1"
  else
    echo "  FAIL  $1"
    echo "        wanted: $2"
    echo "        got:    $3"
    FAILED=$((FAILED + 1))
  fi
}
check_not() {  # check_not <label> <forbidden-substring> <haystack>
  CELLS=$((CELLS + 1))
  if printf '%s' "$3" | grep -qF -- "$2"; then
    echo "  FAIL  $1"
    echo "        must NOT contain: $2"
    echo "        got:              $3"
    FAILED=$((FAILED + 1))
  else
    echo "  ok    $1"
  fi
}

# The retry block's python, taken from the shipped file: the LAST PYEOF-delimited
# block whose body mentions "origin-stamped, retry".
PY=$(awk '
  /python3 - <<.PYEOF./ { collecting=1; buf=""; next }
  collecting && /^PYEOF$/ { if (buf ~ /origin-stamped, retry/) print buf; collecting=0; next }
  collecting { buf = buf $0 "\n" }
' amux)

if [ -z "$PY" ]; then
  echo "FAIL: could not extract the retry block from amux — the test cannot run,"
  echo "      and a test that cannot find its subject must not report PASS."
  exit 1
fi
echo "extracted $(printf '%s' "$PY" | wc -l | tr -d ' ') lines of the shipped retry block"

RESP='{"message":"sent (queued while generating)"}'

# 1. Both operands interpolate, and they are the values the shell passed.
out=$(_R=2 _R_WAITED=4 RESP="$RESP" TGT="somelane" python3 -c "$PY" 2>&1)
check "names the attempt number"        "retry 2"        "$out"
check "names the elapsed wait"          "after 4s"       "$out"
check "still names the target"          "sent to somelane" "$out"
check "still carries the server message" "sent (queued while generating)" "$out"
check "warns the binary may have changed" "check /health"  "$out"
check_not "no empty operand survives"   "retry )"        "$out"
check_not "no bare 'retry' with nothing" "retry :"       "$out"

# 2. A DIFFERENT attempt must produce DIFFERENT output. This is the cell the old
#    code could never pass: its output was identical for every input.
out1=$(_R=1 _R_WAITED=2 RESP="$RESP" TGT="somelane" python3 -c "$PY" 2>&1)
CELLS=$((CELLS + 1))
if [ "$out" = "$out1" ]; then
  echo "  FAIL  attempt 1 and attempt 2 print the same line — the field cannot fail"
  FAILED=$((FAILED + 1))
else
  echo "  ok    a different attempt prints a different line"
fi

# 3. A MISSING operand must CRASH, not print an empty field. The old
#    `.get('_R','')` default is what turned a dead variable into a formatting
#    nit nobody looked at for as long as it existed.
# THIS COMMAND IS SUPPOSED TO FAIL — its non-zero status IS the assertion.
# Under `set -e` a bare `out2=$(...)` would abort the suite here, so the status
# is captured through `if`, which set -e does not treat as an error. NOT `|| true`:
# that would discard the very value the next three lines test (AF-562).
if out2=$(env -u _R _R_WAITED=2 RESP="$RESP" TGT="somelane" python3 -c "$PY" 2>&1); then
  rc2=0
else
  rc2=$?
fi
CELLS=$((CELLS + 1))
if [ "$rc2" -eq 0 ]; then
  echo "  FAIL  a missing _R printed instead of crashing: $out2"
  FAILED=$((FAILED + 1))
else
  echo "  ok    a missing attempt number crashes rather than printing an empty field"
fi

# 3b. A REFUSAL ON THE RETRY PATH MUST SPEAK (AMUX-5028).
#
# This arm shipped as a bare `sys.exit(2)`, which the shell turns into
# `return 1`. Measured on lat-probe 2026-09-23: two transport failures
# (curl_exit 16, confirmed in the cli-transport beacon) followed by a server
# refusal on the third attempt produced NO output on either stream and no
# cmd_history row. The operator's only evidence was an absent line, which is
# indistinguishable from not having run the command.
#
# Cells 1-3 above cannot see this: they feed a SUCCESS response, so they never
# enter the refusal arm at all.
REFUSAL='{"error":"worker is paused"}'
if out3=$(_R=2 _R_WAITED=4 RESP="$REFUSAL" TGT="somelane" python3 -c "$PY" 2>&1); then
  rc3=0
else
  rc3=$?
fi
check "a refused retry names the target"   "send to somelane FAILED" "$out3"
check "a refused retry quotes the reason"  "worker is paused"        "$out3"
check "a refused retry names the attempt"  "retry 2"                 "$out3"
check_not "a refusal is never reported as sent" "sent to somelane"   "$out3"
CELLS=$((CELLS + 1))
if [ "$rc3" -eq 2 ]; then
  echo "  ok    a refusal still exits 2, so the shell does not fall back"
else
  echo "  FAIL  a refusal exited $rc3; the shell reads anything but 2 as retryable"
  FAILED=$((FAILED + 1))
fi

# The refusal must reach STDERR, not stdout. `amux send` output gets piped and
# tail'd; a refusal on stdout is one `| tail -1` away from invisible, and a
# caller capturing the send's output reads it as a value.
if out4=$(_R=2 _R_WAITED=4 RESP="$REFUSAL" TGT="somelane" python3 -c "$PY" 2>/dev/null); then :; fi
CELLS=$((CELLS + 1))
if [ -z "$out4" ]; then
  echo "  ok    nothing about a refusal is written to stdout"
else
  echo "  FAIL  the refusal went to stdout: $out4"
  FAILED=$((FAILED + 1))
fi

# And the SAME property one layer up, on the first-attempt block, which had the
# opposite defect: it spoke, but on stdout.
PY_FIRST=$(awk '
  /python3 - <<.PYEOF./ { collecting=1; buf=""; next }
  collecting && /^PYEOF$/ { if (buf ~ /origin-stamped\)/ && buf !~ /retry/) print buf; collecting=0; next }
  collecting { buf = buf $0 "\n" }
' amux)
CELLS=$((CELLS + 1))
if [ -z "$PY_FIRST" ]; then
  echo "  FAIL  could not extract the first-attempt block; this cell cannot run"
  FAILED=$((FAILED + 1))
else
  if out5=$(RESP="$REFUSAL" TGT="somelane" python3 -c "$PY_FIRST" 2>/dev/null); then :; fi
  if [ -z "$out5" ]; then
    echo "  ok    the first attempt's refusal is on stderr too"
  else
    echo "  FAIL  the first attempt's refusal went to stdout: $out5"
    FAILED=$((FAILED + 1))
  fi
fi

# 4. THE SHELL HALF. Cells 1-3 drive the python directly, so deleting the
#    assignment that FEEDS it leaves them all green — measured: mutating
#    `_R="$_retry" _R_WAITED=... RESP=` down to `RESP=` kept 9 of 9 passing.
#    Half the original bug lived in the shell, so a suite that only exercises
#    the python cannot see it. Asserted against the shipped bytes.
assign=$(grep -c '_R="\$_retry" _R_WAITED="\$(( _retry \* 2 ))" RESP=' amux)
CELLS=$((CELLS + 1))
if [ "$assign" -eq 1 ]; then
  echo "  ok    the shell passes the loop variable into the block (1 call site)"
else
  echo "  FAIL  the python reads _R/_R_WAITED but the shell sets them $assign time(s)"
  echo "        without the assignment the required-read crashes every retry"
  FAILED=$((FAILED + 1))
fi

# And the variable it passes must be the loop's own, not a name nothing sets —
# which is precisely how this shipped broken: `_retry` was the loop variable and
# `_R` was what the python read.
CELLS=$((CELLS + 1))
if grep -q 'for _retry in 1 2; do' amux; then
  echo "  ok    _retry is still the loop variable the assignment reads"
else
  echo "  FAIL  the retry loop no longer defines _retry, so the assignment is dead"
  FAILED=$((FAILED + 1))
fi

echo
if [ "$FAILED" -eq 0 ]; then
  echo "PASS ($CELLS outcome cells)"
  exit 0
fi
echo "FAIL ($FAILED of $CELLS outcome cells)"
exit 1
