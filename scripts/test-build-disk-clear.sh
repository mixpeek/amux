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
# BOTH thresholds are pinned, never inherited. AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB
# defaults to 8, so with only the fleet floor forced these ordering cases would
# pass on a nearly-full dev machine (4GB free -> below 8 -> shared dir cleared)
# and FAIL on a CI runner with tens of GB free, where the keep-warm branch fires
# and the shared dir is spared. That is the same host-dependence that made an
# earlier suite in this repo green locally and red in CI. Pin it high so these
# cases exercise the sacrifice path deterministically; the keep-warm path gets
# its own cases below.
run() { # $1 = fake HOME
  local lg="$1/build.log"
  HOME="$1" \
  AMUX_RS_BUILD_LOG="$lg" \
  AMUX_BUILD_MIN_FREE_GB=999999 \
  AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB=999999 \
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
  AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB=0 \
  AMUX_RS_DISK_CLEAR_DRYRUN=1 AMUX_RS_DISK_CLEAR_ONLY=1 bash "$SCRIPT" >/dev/null 2>&1; cat "$H3/build.log" 2>/dev/null)
if printf '%s\n' "$out3" | grep -qE "DISK LOW"; then
  bad "(d) with free space above the floor nothing may be cleared" "$out3"
else ok; fi

# --- (f) the free-space figure must actually PARSE -------------------------
#     This is why (a)-(c) failed on their first CI run, and they failed only
#     INCIDENTALLY: the script used `df -g`, which is BSD-only, so GNU coreutils
#     printed "df: invalid option -- 'g'", FREE_GB came back empty and the guard
#     silently never fired. A disk guard that is a no-op on Linux while reporting
#     nothing is exactly the shape this repo keeps finding.
#
#     Ordering assertions cannot catch that on their own — they force the branch
#     with a huge floor, so an unparsed figure still reaches the loop. Assert the
#     NUMBER, or the portability regression is only ever caught by luck.
if printf '%s\n' "$out" | grep -qE "DISK LOW: [0-9]+GB free"; then ok
else bad "(f) the free-space figure must parse to a number (df -Pk, not the BSD-only df -g)" "$out"; fi

# --- (g) the per-candidate size must parse too -----------------------------
#     Same defect one line over: `du -sg` is BSD-only in the same way.
if printf '%s\n' "$out" | grep -qE "Clearing the [0-9]+GB"; then ok
else bad "(g) the candidate size must parse to a number (du -sk, not the BSD-only du -sg)" "$out"; fi

# --- (e) the dry-run seam must not actually delete -------------------------
#     If it deleted, every case above would be testing a destroyed fixture and
#     (a) would still pass — so this is what makes the others trustworthy.
if [ -d "$H/.amux/rust-build-target" ] && [ -d "$H/.amux/rust-build-target-e2e-head" ]; then ok
else bad "(e) dry-run must not remove anything" "dirs were deleted"; fi

# ---------------------------------------------------------------------------
# (h)(i) AEAB-35 — the KEEP-WARM branch must actually EXECUTE.
#
# The version this replaces had that branch present, sensible-looking, and DEAD:
# it exited early only once free space reached the FLEET floor (25GB) on a volume
# sitting at 4GB, so the shared cache was destroyed every single time. Zero
# "reclaimed to" lines, ever. A stop condition above the achievable maximum is
# not a stop condition, and every test above passes with it dead — they assert
# ordering and parsing, not that the exit is reachable.
#
# So these drive the script with a FAKE `df` earlier in PATH, reporting a value
# BETWEEN the two thresholds. No test-only branch is added to the script itself:
# it calls `df` unqualified, so a shim is enough, and the shipped code path is
# what runs. Dry-run is OFF here because the decision depends on re-measuring
# after a real delete — which is exactly the interaction that was broken.
# ---------------------------------------------------------------------------
shim() { # $1 = fake HOME, $2 = GB the fake df should report
  # The script EXPORTS its own minimal PATH (line 17) — correct for launchd, and
  # it means prepending to PATH from here has no effect. But that PATH starts
  # with "$HOME/.cargo/bin", and $HOME is already faked for these cases, so the
  # shim goes there and is found first. No test-only branch in the shipped code:
  # the script calls `df` unqualified and the real code path runs.
  mkdir -p "$1/.cargo/bin"
  {
    echo '#!/bin/sh'
    echo '# Fake df in the -Pk shape the script parses.'
    echo 'echo "Filesystem 1024-blocks Used Available Capacity Mounted on"'
    echo "echo \"/dev/fake 100000000 0 $(( $2 * 1048576 )) 1% /\""
  } > "$1/.cargo/bin/df"
  chmod +x "$1/.cargo/bin/df"
}

keepwarm_run() { # $1 name  $2 reported free GB
  local d="$TMP/$1"; rm -rf "$d"
  mkdir -p "$d/.amux/rust-build-target" "$d/.amux/rust-build-target-e2e-head"
  echo x > "$d/.amux/rust-build-target/marker"
  shim "$d" "$2"
  PATH="$d/bin:$PATH" HOME="$d" AMUX_RS_BUILD_LOG="$d/build.log" \
    AMUX_BUILD_MIN_FREE_GB=25 AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB=8 \
    AMUX_RS_DISK_CLEAR_ONLY=1 bash "$SCRIPT" >/dev/null 2>&1
  cat "$d/build.log" 2>/dev/null
}

# (h) 12GB free: under the fleet floor (25) but ABOVE the sacrifice line (8).
#     Reclaim the idle cache, KEEP the shared one, and the build stays warm.
out4=$(keepwarm_run keepwarm 12)
if printf '%s\n' "$out4" | grep -q "stays warm"; then ok
else bad "(h) between the thresholds the keep-warm branch must FIRE" "$out4"; fi
if [ -f "$TMP/keepwarm/.amux/rust-build-target/marker" ]; then ok
else bad "(h) the shared target dir must SURVIVE between the thresholds" "$out4"; fi
if [ -d "$TMP/keepwarm/.amux/rust-build-target-e2e-head" ]; then
  bad "(h) the idle cache must still have been cleared" "$out4"
else ok; fi

# (i) 3GB free: below BOTH. The shared cache is genuinely worth sacrificing, or
#     the fix would have traded dead code for a guard that never protects.
out5=$(keepwarm_run sacrifice 3)
if printf '%s\n' "$out5" | grep -q "SHARED target dir"; then ok
else bad "(i) below the sacrifice line the shared dir must still be cleared" "$out5"; fi
if [ -f "$TMP/sacrifice/.amux/rust-build-target/marker" ]; then
  bad "(i) the shared target dir should have been deleted below the sacrifice line" "$out5"
else ok; fi

echo
echo "test-build-disk-clear: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
