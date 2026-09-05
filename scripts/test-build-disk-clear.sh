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

# --- (g) AF-415: the disk-low arm clears EVEN WITH A PEER BUILD IN FLIGHT --
#     The neighbouring debug-SIZE arm defers while any rustc/cargo runs
#     (AF-303). This one deliberately does not, because below the sacrifice
#     floor ENOSPC breaks every lane anyway — so a peer build dies either way,
#     and the difference is whether it dies diagnosably or with a full disk.
#
#     That asymmetry reads as an oversight, and the obvious "fix" is to add the
#     same gate here. This cell is what makes that a deliberate choice somebody
#     has to argue with rather than one they can quietly reverse: if a peer gate
#     is ever added to the low-disk arm, this fails and says why.
#     ASSERT THE EFFECT, NOT THE LOG LINE. The first version of this cell
#     grepped for "SHARED target dir" — and the echo runs BEFORE the rm, so a
#     peer gate inserted between them left the line intact and the cell passed
#     while the clear was skipped. Verified by mutation: adding
#     `[ -n "$AMUX_BUILD_PEER_PIDS_OVERRIDE" ] && continue` before the rm scored
#     23 passed. So this runs WITHOUT the dry-run seam, against throwaway dirs
#     under $TMP, and asks the filesystem.
H4="$TMP/peerbuilding"; mkdir -p "$H4/.amux/rust-build-target" "$H4/.amux/rust-build-target-e2e-head"
HOME="$H4" AMUX_RS_BUILD_LOG="$H4/build.log" AMUX_BUILD_MIN_FREE_GB=999999 \
  AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB=999999 \
  AMUX_BUILD_PEER_PIDS_OVERRIDE="4242 4243" \
  AMUX_RS_DISK_CLEAR_ONLY=1 bash "$SCRIPT" >/dev/null 2>&1
out4=$(cat "$H4/build.log" 2>/dev/null)
if [ ! -d "$H4/.amux/rust-build-target" ]; then ok
else bad "(g) the low-disk arm must still CLEAR with peers building — below the sacrifice floor ENOSPC breaks them anyway (AF-415)" "$out4"; fi
# PRECONDITION, so (g) cannot pass because the script never reached the arm:
# the low-disk branch must actually have run.
if printf '%s\n' "$out4" | grep -q "DISK LOW"; then ok
else bad "(g) precondition: the low-disk branch must have been reached at all" "$out4"; fi

# --- (h) AF-415: and it must say WHOSE build goes cold ---------------------
#     The line used to read as if the cost landed on the process doing the
#     clearing. It lands on every lane, and it was 11 firings of the 25GB-era
#     version of this arm that produced AMUX-2936's three mid-build failures
#     (diagnosis on AF-416). A log line that understates its blast radius is
#     how the same clear gets re-tuned by someone who thinks it is cheap.
if printf '%s\n' "$out" | grep -q "EVERY lane's next build goes cold"; then ok
else bad "(h) the low-disk clear must name whose build it costs — every lane's, not just this one" "$out"; fi

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

# ---------------------------------------------------------------------------
# THE DEBUG-ARTIFACT ARM (AF-303). It shipped with NO coverage here at all,
# which is how its comment came to say "always safe to remove" two lines below
# its own statement that fleet sessions' cargo check/test land in debug/. Those
# artifacts are what every lane is building against; safe for THIS BUILDER does
# not imply safe. Measured from the live log before the fix: 13 debug clears
# against 1 clear of the shared release cache, because the release cache is
# gated behind severe disk pressure and this fired on SIZE alone, every cycle.
#
# `-1` as the threshold makes an empty fixture dir exceed it, so these need no
# multi-GB fixture. HOME is faked, so the real shared target dir is untouched.
# ---------------------------------------------------------------------------
dbg_run() { # $1 = fake HOME leaf, $2 = MIN_FREE_GB, $3 = peer-pid override
  local h="$TMP/$1"; local lg="$h/build.log"
  mkdir -p "$h/.amux/rust-build-target/debug"
  : > "$h/.amux/rust-build-target/debug/marker"
  HOME="$h" \
  AMUX_RS_BUILD_LOG="$lg" \
  AMUX_BUILD_MIN_FREE_GB="$2" \
  AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB=0 \
  AMUX_BUILD_DEBUG_CLEAR_ABOVE_GB=-1 \
  AMUX_BUILD_PEER_PIDS_OVERRIDE="$3" \
  AMUX_RS_DISK_CLEAR_ONLY=1 \
    bash "$SCRIPT" >/dev/null 2>&1
  cat "$lg" 2>/dev/null
}

# (j) A PEER IS BUILDING and disk is healthy: DEFER. This is the whole card.
out6=$(dbg_run peerbuild 0 "4242")
if printf '%s\n' "$out6" | grep -q "DEFERRED"; then ok
else bad "(j) with a peer build in flight and healthy disk the clear must DEFER" "$out6"; fi
if [ -f "$TMP/peerbuild/.amux/rust-build-target/debug/marker" ]; then ok
else bad "(j) a peer's debug artifacts must SURVIVE the deferral" "$out6"; fi

# (k) NO peer building: clear as before. Without this, a fix that simply never
#     cleared would pass (j) while handing back the 229GB unbounded growth.
out7=$(dbg_run nopeer 0 "")
if printf '%s\n' "$out7" | grep -q "Clearing"; then ok
else bad "(k) with no peer build in flight the clear must still happen" "$out7"; fi
if [ -f "$TMP/nopeer/.amux/rust-build-target/debug/marker" ]; then
  bad "(k) debug/ should have been removed when no peer is building" "$out7"
else ok; fi

# (l) A peer IS building but disk is BELOW the fleet floor: ENOSPC outranks the
#     peer, because running out of disk breaks the lane being protected too.
#     A deferral with no override is a disk-full outage with better manners.
out8=$(dbg_run peerbutfull 999999 "4242")
if printf '%s\n' "$out8" | grep -q "Clearing"; then ok
else bad "(l) below the fleet floor the clear must override a peer build" "$out8"; fi
if printf '%s\n' "$out8" | grep -q "ENOSPC outranks"; then ok
else bad "(l) the override must SAY it overrode a peer, not clear silently" "$out8"; fi

# (m) THE DETECTOR'S PRECISION, exercised for real with NO override. A process
#     whose COMMAND LINE merely mentions cargo must not read as a build: the
#     first cut used `pgrep -f` and matched the very shell that ran it, because
#     that command line contained the word. On this box, where lanes grep for
#     cargo constantly, that detector defers every cycle forever and quietly
#     restores the unbounded growth the clear exists to stop.
#
#     SKIPPED HONESTLY, never passed, when a real toolchain process is running:
#     the precondition cannot be established then, and a green under those
#     conditions would mean nothing.
# READ THE OUTPUT, NOT THE EXIT CODE. `{ a; b; }` has the exit status of `b`
# ALONE, so the first cut of this guard reported "a build is running" purely on
# whether `pgrep -x cargo` matched, ignoring rustc entirely - and rustc is the
# process that is actually running for most of a build. It skipped when it
# should have run and would have run when it should have skipped.
#
# (An earlier version of this comment claimed macOS `pgrep -x` exits 0 with
# empty output. That was wrong: measured on an idle host it exits 1, exactly
# like GNU. The 0 I read came from a sample taken while a build was in flight,
# and I generalised a platform quirk out of one contaminated measurement.
# Recorded rather than deleted, because the next reader will be tempted to
# "fix" this back to an exit-code test.)
#
# The shipped detector consumes the output too, so it is unaffected either way.
_real_builds="$( { pgrep -x rustc; pgrep -x cargo; } 2>/dev/null | tr -d '[:space:]')"
if [ -n "$_real_builds" ]; then
  echo "SKIP (m): a real cargo/rustc is running on this host, so the no-peer"
  echo "         precondition cannot be established. Not counted as a pass."
else
  sh -c 'sleep 5 # cargo rustc build' & DECOY=$!
  sleep 1
  h="$TMP/decoy"; mkdir -p "$h/.amux/rust-build-target/debug"
  : > "$h/.amux/rust-build-target/debug/marker"
  out9=$(HOME="$h" AMUX_RS_BUILD_LOG="$h/build.log" AMUX_BUILD_MIN_FREE_GB=0 \
    AMUX_BUILD_SACRIFICE_CACHE_BELOW_GB=0 AMUX_BUILD_DEBUG_CLEAR_ABOVE_GB=-1 \
    AMUX_RS_DISK_CLEAR_ONLY=1 bash "$SCRIPT" >/dev/null 2>&1; cat "$h/build.log" 2>/dev/null)
  kill "$DECOY" 2>/dev/null; wait "$DECOY" 2>/dev/null
  # PROVE THE PROBE RAN before believing its negative (ethos rule 4). This cell
  # asserted only the ABSENCE of "DEFERRED", and a script that died before
  # reaching the decision produces exactly that log. It did: the first cut of
  # the detector killed the builder under `set -euo pipefail` on an idle host,
  # and this cell stayed green through it. The positive that must appear beside
  # the negative is the clear line itself.
  if printf '%s\n' "$out9" | grep -q "DEBUG ARTIFACTS"; then ok
  else bad "(m) the script must REACH the debug decision, not die before it" "$out9"; fi
  if printf '%s\n' "$out9" | grep -q "DEFERRED"; then
    bad "(m) a command line that merely MENTIONS cargo must not read as a build" "$out9"
  else ok; fi
fi

echo
echo "test-build-disk-clear: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
