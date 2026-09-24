#!/bin/bash
# The lima data-disk report in scripts/mac-cleanup-tick.sh (DESKT-47).
#
# On 2026-09-24 ~/.colima held 330.9 GB in nine data disks, six of them (162.7 GB)
# with no registered VM, while the disk sat at 1.8 GB free and no amux instrument
# named the consumer. The report is REPORT-ONLY: these are other lanes' VM data.
# Each cell fails if its guard is removed (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
trap 'rm -rf -- "$FIX"' EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"

# A fake lima tree with REAL allocated bytes (dd, not a sparse file), sized in KB so
# the report's arithmetic is exact: an 8 MB orphan, a 5 MB registered-but-stopped
# disk, a 3 MB disk whose VM is running.
LR="$FIX/lima"
mkdir -p "$LR/_disks/colima-orphan" "$LR/_disks/colima-stop" "$LR/_disks/colima-run" "$LR/colima-stop" "$LR/colima-run"
dd if=/dev/zero of="$LR/_disks/colima-orphan/datadisk" bs=1024 count=8192 2>/dev/null
dd if=/dev/zero of="$LR/_disks/colima-stop/datadisk"   bs=1024 count=5120 2>/dev/null
dd if=/dev/zero of="$LR/_disks/colima-run/datadisk"    bs=1024 count=3072 2>/dev/null
export AMUX_CLEANUP_LIMA_RUNNING="colima-run"

echo "1. a missing lima root says so instead of reading as clean"
out=$(lima_disks_report "$FIX/nope")
case "$out" in *"no lima root at $FIX/nope (not present)"*) echo "  ok   the missing root is named" ;;
  *) echo "  FAIL a missing root printed: $out"; fails=$((fails+1)) ;; esac
mkdir -p "$FIX/empty/_disks"
case "$(lima_disks_report "$FIX/empty")" in *"none under $FIX/empty/_disks"*) echo "  ok   a root with no disks says none" ;;
  *) echo "  FAIL an empty root did not say none"; fails=$((fails+1)) ;; esac

echo "2. registered, running and orphaned disks are told apart"
out=$(lima_disks_report "$LR" 1048576000)      # show threshold huge: only non-stopped listed
check "summary counts all three disks"           "yes" "$(printf '%s' "$out" | grep -q 'lima disks: 3 (16M allocated' && echo yes || echo no)"
check "the orphan total is exactly the orphan"   "yes" "$(printf '%s' "$out" | grep -q '8M ORPHANED with no registered VM' && echo yes || echo no)"
check "the running total is exactly the running" "yes" "$(printf '%s' "$out" | grep -q '3M in running VMs' && echo yes || echo no)"
check "the orphan is listed as ORPHANED"         "yes" "$(printf '%s' "$out" | grep -q 'colima-orphan ORPHANED' && echo yes || echo no)"
check "the running one is listed as running"     "yes" "$(printf '%s' "$out" | grep -q 'colima-run running' && echo yes || echo no)"
check "a registered stopped disk is NOT orphaned" "0"  "$(printf '%s' "$out" | grep -c 'colima-stop ORPHANED' || true)"
check "a small stopped disk is not listed"       "0"   "$(printf '%s' "$out" | grep -c 'colima-stop' || true)"
out=$(lima_disks_report "$LR" 1)               # show threshold tiny: stopped one appears too
check "over the show threshold a stopped disk is listed as stopped" "yes" "$(printf '%s' "$out" | grep -q 'colima-stop stopped' && echo yes || echo no)"

echo "3. instance names come out of a real-shaped hostagent command line"
LINE='/Users/ethan/x/bin/limactl hostagent --pidfile /Users/ethan/.colima/_lima/colima-gs7-e/ha.pid --socket /Users/ethan/.colima/_lima/colima-gs7-e/ha.sock colima-gs7-e'
check "hostagent line yields the instance name" "colima-gs7-e" "$(printf '%s\n' "$LINE" | lima_names_from_ps)"
check "a usernet line is not an instance"       ""             "$(printf '%s\n' '/x/limactl usernet -p /Users/ethan/.colima/_lima/_networks/user-v2/usernet_user-v2.pid' | lima_names_from_ps)"

echo "4. the report deletes nothing"
lima_disks_report "$LR" 1 >/dev/null
check "all three fixture disks still exist" "3" "$(ls "$LR"/_disks/*/datadisk | wc -l | tr -d ' ')"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-lima — all checks passed"; exit 0; fi
echo "mac-cleanup-lima: $fails check(s) FAILED"; exit 1
