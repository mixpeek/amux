#!/usr/bin/env bash
# test-land-gate-slots.sh [amux]: at most AMUX_LAND_GATE_SLOTS land gates run at
# once on a machine (Mac CPU RCAs 20261117-152949 and 20261123-103607). Runs the
# CLI's own land_gate_run against stand-in gates that record when they ran.
set -euo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }
H="$(mktemp -d "${TMPDIR:-/tmp}/lgs.XXXXXX")"; trap 'rm -rf "$H"' EXIT
# The two functions, from the CLI as written.
# A file, not <(...): macOS's bash 3.2 cannot source a process substitution.
sed -n '/^land_gate_slot_take() {/,/^}/p;/^land_gate_run() {/,/^}/p' "$AM" > "$H/fns.sh"
source "$H/fns.sh"
type land_gate_run >/dev/null 2>&1 || { echo "FAIL land_gate_run not found in $AM"; exit 1; }
export HOME="$H" AMUX_LAND_GATE_SLOT_POLL_S=1 AMUX_WORKER=lgs
gate() { echo "start $1 $(date +%s)" >> "$H/runs"; sleep 3; echo "end $1 $(date +%s)" >> "$H/runs"; }
export -f gate; export H

# 1. One slot: two gates run one after the other, and the second says it waited.
AMUX_LAND_GATE_SLOTS=1 land_gate_run bash -c 'gate a' 2> "$H/a.err" &
sleep 0.5
AMUX_LAND_GATE_SLOTS=1 land_gate_run bash -c 'gate b' 2> "$H/b.err" &
wait
a_end=$(awk '$1=="end" && $2=="a"{print $3}' "$H/runs"); b_start=$(awk '$1=="start" && $2=="b"{print $3}' "$H/runs")
{ [ -n "$a_end" ] && [ -n "$b_start" ] && [ "$b_start" -ge "$a_end" ]; } && ok "one slot: the second gate starts after the first ends" || bad "overlap: $(cat "$H/runs")"
grep -q 'verdict=land_gate_slot_wait' "$H/b.err" && grep -q 'lgs (pid' "$H/b.err" && ok "the waiting gate says it waits and who holds the slot" || bad "no wait line: $(cat "$H/b.err")"
[ -z "$(ls "$H/.amux/locks/land-gate" 2>/dev/null)" ] && ok "both slots are released after the gates" || bad "slots left: $(ls "$H/.amux/locks/land-gate")"

# 2. Two slots: two gates run together.
: > "$H/runs"
AMUX_LAND_GATE_SLOTS=2 land_gate_run bash -c 'gate c' 2>/dev/null &
AMUX_LAND_GATE_SLOTS=2 land_gate_run bash -c 'gate d' 2>/dev/null &
wait
c_end=$(awk '$1=="end" && $2=="c"{print $3}' "$H/runs"); d_start=$(awk '$1=="start" && $2=="d"{print $3}' "$H/runs")
[ "$d_start" -lt "$c_end" ] && ok "two slots: two gates run at once" || bad "two slots serialized: $(cat "$H/runs")"

# 2b. A slot made by another gate whose owner line is not written yet (the
# race between its mkdir and its write) is waited out, not a crash of this
# gate (checks red 2026-11-23 on Linux, where `stat -f` is the file system's).
mkdir -p "$H/.amux/locks/land-gate/slot-1"
( sleep 2; rm -rf "$H/.amux/locks/land-gate/slot-1" ) &
rc=0; AMUX_LAND_GATE_SLOTS=1 timeout 20 bash -c 'source "$0"; land_gate_run true' "$H/fns.sh" 2> "$H/r.err" || rc=$?
wait
[ "$rc" = 0 ] && ok "a slot with no owner yet is waited out, and the gate runs" || bad "unwritten owner rc=$rc: $(cat "$H/r.err")"

# 3. A slot held by a dead process is taken back.
mkdir -p "$H/.amux/locks/land-gate/slot-1"; echo "999999 ghost 1" > "$H/.amux/locks/land-gate/slot-1/owner"
rc=0; AMUX_LAND_GATE_SLOTS=1 timeout 20 bash -c 'source "$0"; land_gate_run true' "$H/fns.sh" 2> "$H/g.err" || rc=$?
{ [ "$rc" = 0 ] && grep -q 'verdict=land_gate_slot_reclaimed' "$H/g.err"; } && ok "a dead holder's slot is reclaimed" || bad "dead slot rc=$rc: $(cat "$H/g.err")"
if ! grep -q 'verdict=land_gate_slot_wait' "$H/g.err"; then ok "and taken at once, with no false wait"; else bad "a reclaimed slot waited: $(cat "$H/g.err")"; fi

# 4. The gate's exit status comes through, and 0 slots means no limit.
rc=0; AMUX_LAND_GATE_SLOTS=1 land_gate_run bash -c 'exit 7' 2>/dev/null || rc=$?
[ "$rc" = 7 ] && ok "the gate's exit status comes through" || bad "exit status lost"
AMUX_LAND_GATE_SLOTS=0 land_gate_run true 2> "$H/z.err"
if ! grep -q 'slot' "$H/z.err"; then ok "0 slots: no limit, no slot"; else bad "0 slots took one: $(cat "$H/z.err")"; fi

[ "$fail" = 0 ] && echo "PASS: land gate slots" || echo "FAIL: land gate slots"
exit $fail
