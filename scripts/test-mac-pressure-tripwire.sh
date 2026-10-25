#!/usr/bin/env bash
# Tests for mac-pressure-tripwire.sh.
#
# WHAT BROKE (AMUX-4661): `sysctl` lives in /usr/sbin. The script called it bare,
# and the PATH it actually runs under has no /usr/sbin, so the pressure level and
# the swap reading both came back empty and two of the three thresholds could
# never trip. A memory fire alarm that cannot read memory.
#
# THIS SUITE HAS TO STRADDLE TWO PLATFORMS, and saying so is the point. CI is
# `runs-on: ubuntu-latest` (.github/workflows/checks.yml) and this script reads
# Darwin-only sysctls (kern.memorystatus_vm_pressure_level, vm.swapusage). So the
# cell that proves the bug is gone can only run on a Mac. Wiring a Darwin-only
# assertion into a Linux runner is how ea07267a turned a green step red earlier
# the same day; the skip below is deliberate and it PRINTS, so a reader never
# mistakes "did not run" for "passed".
#
# What that leaves on Linux is a source-level cell. It is a weaker instrument
# than the behavioural one and it is not nothing: it fails if anyone deletes the
# export or moves it below the first probe, which is the regression that would
# actually happen.
# -e per the AF-561 ratchet: without it a harness prints PASS after calling a
# helper that does not exist.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SUT="$HERE/mac-pressure-tripwire.sh"
FAIL=0
RAN=0
SKIPPED=0

cell() { # name expected actual
  RAN=$((RAN + 1))
  if [ "$2" = "$3" ]; then
    echo "PASS $1: $3"
  else
    echo "FAIL $1: expected '$2' got '$3'"
    FAIL=$((FAIL + 1))
  fi
}
skip() { # name reason
  SKIPPED=$((SKIPPED + 1))
  echo "SKIP $1: $2"
}

[ -f "$SUT" ] || { echo "FAIL: $SUT not found"; exit 1; }

# ---------------------------------------------------------------------------
# Cell 1 (every platform): the export exists AND precedes the first probe.
#
# Presence alone is not the property. An export BELOW the first `sysctl` call
# would grep as present and fix nothing, so compare line numbers rather than
# asking whether the string is in the file.
# ---------------------------------------------------------------------------
# `|| true` on both, and it is load-bearing rather than defensive. NO MATCH IS
# THE FAILING ANSWER HERE, and under `set -euo pipefail` grep's exit 1 would kill
# this harness at the assignment, before a single cell printed. It would still
# exit non-zero, so it would look like a working test while telling you nothing
# about WHICH property broke. Verified by mutation: without these the whole file
# produced ZERO lines of output under the pre-fix state.
export_ln=$(grep -nE '^[[:space:]]*PATH=.*(/usr/sbin)' "$SUT" | head -1 | cut -d: -f1 || true)
probe_ln=$(grep -nE '(^|[^-[:alnum:]_/])sysctl ' "$SUT" | head -1 | cut -d: -f1 || true)

cell "export_exists" "yes" "$([ -n "$export_ln" ] && echo yes || echo no)"
cell "first_probe_found" "yes" "$([ -n "$probe_ln" ] && echo yes || echo no)"

if [ -n "$export_ln" ] && [ -n "$probe_ln" ]; then
  cell "export_precedes_first_probe" "yes" \
    "$([ "$export_ln" -lt "$probe_ln" ] && echo yes || echo no)"
else
  cell "export_precedes_first_probe" "yes" "no"
fi

# ---------------------------------------------------------------------------
# Cell 2 (every platform): under a restricted PATH the script REPORTS rather
# than crashing or lying. `set -uo pipefail` without -e is deliberate in the SUT
# for exactly this; pin it, because the honest `measured=false` is the only
# reason the original bug was findable at all.
# ---------------------------------------------------------------------------
out=$(env -i PATH=/usr/bin:/bin HOME="${HOME:-/tmp}" \
  AMUX_TRIPWIRE_STATE="${TMPDIR:-/tmp}/mpt-test-$$.last" \
  bash "$SUT" --dry-run 2>&1) && rc=0 || rc=$?
rm -f "${TMPDIR:-/tmp}/mpt-test-$$.last"

cell "restricted_path_exits_zero" "0" "$rc"
cell "restricted_path_reports_measured" "yes" \
  "$(printf '%s' "$out" | grep -q 'measured=' && echo yes || echo no)"

# ---------------------------------------------------------------------------
# Cell 3 (Darwin only): the actual bug. Under the PATH that broke it, every
# probe must measure. This is the cell that would have caught AMUX-4661 and the
# one that goes red if the export is removed.
# ---------------------------------------------------------------------------
if [ "$(uname -s)" = "Darwin" ]; then
  cell "restricted_path_measures" "yes" \
    "$(printf '%s' "$out" | grep -q 'measured=true' && echo yes || echo no)"

  # Named separately from `measured=true` so a failure says WHICH probe died.
  # `level=-1` and an empty `swap_used=MB` are the exact strings the broken
  # version printed.
  cell "pressure_level_is_real" "no" \
    "$(printf '%s' "$out" | grep -q 'level=-1' && echo yes || echo no)"
  cell "swap_used_is_not_empty" "no" \
    "$(printf '%s' "$out" | grep -qE 'swap_used=(MB)?$|swap_used=MB' && echo yes || echo no)"
else
  skip "restricted_path_measures" \
    "kern.memorystatus_vm_pressure_level and vm.swapusage are Darwin-only; on $(uname -s) cell 1 is what guards this"
  skip "pressure_level_is_real" "same reason"
  skip "swap_used_is_not_empty" "same reason"
fi

# ---------------------------------------------------------------------------
# Act tier (every platform; the level is injected): at the warn level the
# tripwire STARTS the cleanup instead of waiting for the page tier, once per
# cooldown, and says so when the start fails. The trigger is a stub that
# records its calls, so a cell counts real starts rather than grepping a label.
# ---------------------------------------------------------------------------
act_dir=$(mktemp -d "${TMPDIR:-/tmp}/mpt-act.XXXXXX")
calls="$act_dir/calls"; : > "$calls"
stub="$act_dir/stub.sh"
printf '#!/bin/sh\necho x >> "%s"\necho "${STUB_CODE:-202}"\n' "$calls" > "$stub"; chmod +x "$stub"
act_run() { # <level> [STUB_CODE]
  STUB_CODE=${2:-202} AMUX_TRIPWIRE_LEVEL=$1 AMUX_TRIPWIRE_SWAP_FREE_MB=0 \
    AMUX_TRIPWIRE_STATE="$act_dir/last" AMUX_TRIPWIRE_ACT_STATE="$act_dir/act" \
    AMUX_TRIPWIRE_ACT_TRIGGER_CMD="$stub" bash "$SUT" 2>&1 || true
}
out=$(act_run 1)
cell "act_level1_no_start" "0" "$(wc -l < "$calls" | tr -d ' ')"
out=$(act_run 2)
cell "act_level2_starts_cleanup" "1" "$(wc -l < "$calls" | tr -d ' ')"
cell "act_level2_verdict" "yes" "$(printf '%s' "$out" | grep -q 'verdict=cleanup_triggered' && echo yes || echo no)"
out=$(act_run 2)
cell "act_cooldown_holds" "1" "$(wc -l < "$calls" | tr -d ' ')"
cell "act_cooldown_verdict" "yes" "$(printf '%s' "$out" | grep -q 'verdict=cleanup_cooldown' && echo yes || echo no)"
rm -f "$act_dir/act"
out=$(act_run 2 500)
cell "act_failure_named" "yes" "$(printf '%s' "$out" | grep -q 'verdict=cleanup_trigger_failed' && echo yes || echo no)"
cell "act_failure_not_recorded" "no" "$([ -s "$act_dir/act" ] && echo yes || echo no)"
rm -rf -- "${act_dir:?}"

echo
echo "ran=$RAN failed=$FAIL skipped=$SKIPPED platform=$(uname -s)"
[ "$FAIL" -eq 0 ] || exit 1
