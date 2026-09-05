#!/usr/bin/env bash
# Tests for `scripts/mutate.sh run` (AF-284).
#
# WHY THIS FILE EXISTS: amux verified `run` by using it, and scoped their
# sign-off precisely — they exercised apply, the failing-command path, the
# revert, and the ambiguity refusal, but NOT the killed-by-timeout path or the
# trap-disarm on double-fire. Those two are exactly the cells the author got
# wrong on the first attempt, and they are the reason the tool is safe on a
# shared checkout: a killed command must still revert, exactly once.
#
# Author-only verification of the most safety-critical path is what the peer
# gate exists to prevent, so the fix is not to argue about it — it is to make
# the cells runnable by anyone.
set -uo pipefail
# Overridable so the cells below can be run against a DELIBERATELY BROKEN copy,
# which is how they get shown to fail. Never mutate mutate.sh in place to do
# that: bash reads a running script incrementally, so editing the file being
# executed corrupts the running shell ("unexpected EOF"), and the test then
# passes for a reason that has nothing to do with the code.
MUT="${MUTATE_SH:-$(cd "$(dirname "$0")" && pwd)/mutate.sh}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail=0
ok()   { printf '  ok   %s\n' "$1"; }
bad()  { printf '  FAIL %s\n' "$1"; fail=1; }

fresh() { printf 'alpha\nbeta\ngamma\n' > "$TMP/f.txt"; }

# 1. HAPPY PATH: applies, runs, reverts, file identical.
fresh; before=$(cat "$TMP/f.txt")
out=$("$MUT" run "$TMP/f.txt" beta BETA -- true 2>&1); rc=$?
[ "$rc" -eq 0 ] && ok "happy path exits 0" || bad "happy path exit $rc"
[ "$(cat "$TMP/f.txt")" = "$before" ] && ok "happy path restores the file" || bad "happy path left residue"

# 2. FAILING COMMAND: the command's exit status is preserved, file restored.
fresh
"$MUT" run "$TMP/f.txt" beta BETA -- sh -c 'exit 7' >/dev/null 2>&1; rc=$?
[ "$rc" -eq 7 ] && ok "failing command preserves exit 7" || bad "expected 7, got $rc"
[ "$(cat "$TMP/f.txt")" = "$before" ] && ok "failing command restores the file" || bad "residue after failure"

# 3. THE TOOL ITSELF IS KILLED — the cell amux did not exercise, and the whole
#    reason the trap exists. A killed `mutate.sh` must still revert, and the trap
#    must DISARM so it reverts exactly ONCE: without the disarm, TERM fires the
#    trap and then EXIT fires it again, the second pass finds 0 occurrences and
#    prints "NOT applied" — a failed-restore message after a SUCCESSFUL restore.
#    The file is correct either way; only the message lies, which is the failure
#    this tool exists to prevent, one layer up.
#
#    THE FIRST VERSION OF THIS CELL DID NOT TEST THE TRAP AT ALL. It ran
#    `-- timeout 1 sleep 5`, which kills the CHILD; mutate.sh survives, sees exit
#    124 and takes its ordinary revert path. It passed against a copy with the
#    disarm deleted, which is how it was caught. Killing the tool means signalling
#    the tool.
fresh
"$MUT" run "$TMP/f.txt" beta BETA -- sleep 5 >"$TMP/out" 2>&1 &
mpid=$!
sleep 1
[ "$(cat "$TMP/f.txt")" != "$before" ] && ok "mutation is in place while the command runs" || bad "mutation never landed, so the kill below proves nothing"
kill -TERM "$mpid" 2>/dev/null
wait "$mpid" 2>/dev/null; rc=$?
out=$(cat "$TMP/out")
[ "$(cat "$TMP/f.txt")" = "$before" ] && ok "killed TOOL still restores the file" || bad "KILLING THE TOOL LEFT THE MUTATION IN PLACE"
n=$(printf '%s\n' "$out" | grep -c 'revert: LANDED')
[ "$n" -eq 1 ] && ok "killed tool reverts exactly once (trap disarmed)" || bad "reverted $n times — the trap did not disarm"
printf '%s\n' "$out" | grep -q 'NOT applied' && bad "printed a failed-restore message after a successful restore" || ok "no misleading NOT-applied line"

# 3b. A child that dies on its own is the ORDINARY path, not the trap path:
#     exit status preserved, file restored.
fresh
"$MUT" run "$TMP/f.txt" beta BETA -- timeout 1 sleep 5 >/dev/null 2>&1; rc=$?
[ "$rc" -eq 124 ] && ok "child killed by timeout preserves exit 124" || bad "expected 124, got $rc"
[ "$(cat "$TMP/f.txt")" = "$before" ] && ok "child killed by timeout restores the file" || bad "residue after child timeout"

# 4. ABSENT TARGET: refuses, and the command must never run.
fresh; rm -f "$TMP/ran"
"$MUT" run "$TMP/f.txt" nosuchstring X -- touch "$TMP/ran" >/dev/null 2>&1
[ ! -f "$TMP/ran" ] && ok "absent target never runs the command" || bad "ran the command despite refusing"

# 5. AMBIGUOUS TARGET: refuses rather than half-mutating (the cell amux hit for real).
printf 'dup\ndup\n' > "$TMP/g.txt"; rm -f "$TMP/ran"
"$MUT" run "$TMP/g.txt" dup X -- touch "$TMP/ran" >/dev/null 2>&1
[ ! -f "$TMP/ran" ] && ok "ambiguous target never runs the command" || bad "ran the command on an ambiguous target"
[ "$(cat "$TMP/g.txt")" = "$(printf 'dup\ndup\n')" ] && ok "ambiguous target leaves the file untouched" || bad "half-mutated an ambiguous file"

[ "$fail" -eq 0 ] && echo "PASS" || echo "FAIL"
exit "$fail"
