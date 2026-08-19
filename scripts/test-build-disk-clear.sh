#!/usr/bin/env bash
# AEAB-34 — the auto-builder's low-disk clear must free the IDLE cache before
# the one the build it is about to run depends on.
#
# The bug this pins: until 2026-08-19 the clear deleted `rust-build-target`
# unconditionally — the cache filled by the very next line — while
# `rust-build-target-e2e-head` sat untouched at 4.2GB. Measured that day: it
# fired 16 times, free space still reached 1GB, and each pass freed ~2GB that
# the following cold build put straight back. A treadmill costing a cold build
# every 60s while four times the space sat one directory over.
#
# Ordering is the whole fix, so ordering is what this asserts. It runs the
# SHIPPED script through its dry-run seam rather than restating the logic.
#
# Exit 0 = all pass, 1 = a failure. Wired into .github/workflows/checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
SCRIPT="$(pwd)/scripts/rust-auto-build.sh"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# The script redirects its whole build block to $LOG, so stdout is empty by
# design — read the log it actually writes. AMUX_RS_BUILD_LOG is the existing
# seam for that. Getting this wrong the first time made every case report
# "<empty>", which looked like the script doing nothing rather than the harness
# reading the wrong stream.
run() { # $1 = fake HOME
  local lg="$1/build.log"
  HOME="$1" \
  AMUX_RS_BUILD_LOG="$lg" \
  AMUX_BUILD_MIN_FREE_GB=999999 \
  AMUX_RS_DISK_CLEAR_DRYRUN=1 \
  AMUX_RS_DISK_CLEAR_ONLY=1 \
    bash "$SCRIPT" >/dev/null 2>&1
  cat "$lg" 2>/dev/null
}

ok()   { PASS=$((PASS+1)); }
bad()  { FAIL=$((FAIL+1)); echo "FAIL: $1"; echo "  got: ${2:-<empty>}"; }

# --- (a) both caches present: the IDLE one must be named FIRST -------------
H="$TMP/both"; mkdir -p "$H/.amux/rust-build-target" "$H/.amux/rust-build-target-e2e-head"
out=$(run "$H")
idle_line=$(printf '%s\n' "$out" | grep -n "idle e2e target dir" | head -1 | cut -d: -f1)
shared_line=$(printf '%s\n' "$out" | grep -n "SHARED target dir" | head -1 | cut -d: -f1)
if [ -n "$idle_line" ] && [ -n "$shared_line" ] && [ "$idle_line" -lt "$shared_line" ]; then ok
else bad "(a) the idle e2e cache must be cleared BEFORE the shared one" "$out"; fi

# --- (b) the shared dir must still be reachable as a LAST resort -----------
#     A fix that simply never touched it would pass (a) while reintroducing the
#     original disk-full outage this clear exists to prevent.
if printf '%s\n' "$out" | grep -q "SHARED target dir"; then ok
else bad "(b) the shared dir must remain a last-resort candidate" "$out"; fi

# --- (c) the shared dir alone: it is still cleared, not skipped ------------
H2="$TMP/sharedonly"; mkdir -p "$H2/.amux/rust-build-target"
out2=$(run "$H2")
if printf '%s\n' "$out2" | grep -q "SHARED target dir"; then ok
else bad "(c) with only the shared dir present it must still be cleared" "$out2"; fi
if printf '%s\n' "$out2" | grep -q "idle e2e target dir"; then
  bad "(c) must not claim to clear an e2e dir that does not exist" "$out2"
else ok; fi

# --- (d) CONTROL: above the floor, clear NOTHING ---------------------------
#     Without this, a script that cleared unconditionally passes every case
#     above — and would delete both caches on every 60s tick forever.
H3="$TMP/plenty"; mkdir -p "$H3/.amux/rust-build-target" "$H3/.amux/rust-build-target-e2e-head"
out3=$(HOME="$H3" AMUX_RS_BUILD_LOG="$H3/build.log" AMUX_BUILD_MIN_FREE_GB=0 \
  AMUX_RS_DISK_CLEAR_DRYRUN=1 AMUX_RS_DISK_CLEAR_ONLY=1 bash "$SCRIPT" >/dev/null 2>&1; cat "$H3/build.log" 2>/dev/null)
if printf '%s\n' "$out3" | grep -qE "DISK LOW"; then
  bad "(d) with free space above the floor nothing may be cleared" "$out3"
else ok; fi

# --- (e) the dry-run seam must not actually delete -------------------------
#     If it deleted, every case above would be testing a destroyed fixture and
#     (a) would still pass — so this is what makes the others trustworthy.
if [ -d "$H/.amux/rust-build-target" ] && [ -d "$H/.amux/rust-build-target-e2e-head" ]; then ok
else bad "(e) dry-run must not remove anything" "dirs were deleted"; fi

echo
echo "test-build-disk-clear: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
