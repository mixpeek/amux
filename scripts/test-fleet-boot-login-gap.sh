#!/usr/bin/env bash
# AF-498 — fleet-boot reports how long the fleet was DOWN before anyone logged in.
#
# A LaunchAgent loads at GUI LOGIN, not at boot, so an unattended reboot (a macOS
# auto-update overnight is the specimen) leaves every worker down until a human
# sits down. Reported live: "an iOS update automatically at like 2 a.m. So
# everything stopped." Nothing was broken and nothing could have said so —
# fleet-boot's log begins when it RUNS, so the hours before it ran left no trace.
#
# The gap block is only reachable by actually rebooting unless its source is
# injectable, which is what AMUX_FLEET_BOOT_EPOCH is for. This drives all three
# arms and asserts exactly one fires.
set -uo pipefail
BOOT="${FLEET_BOOT_SH:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/scripts/fleet-boot.sh}"
[[ -x "$BOOT" ]] || { echo "FAIL: $BOOT not executable"; exit 1; }

fails=0
cells=0

# Run fleet-boot far enough to emit the gap line, then stop it. AMUX_BIN points
# at a stub that is not executable, so it exits at the FATAL check immediately
# after the gap block — no worker is ever started by this test.
run_gap() {
  local epoch="$1" home
  home="$(mktemp -d)"
  mkdir -p "$home/logs"
  AMUX_FLEET_BOOT_LOG="$home/logs/fleet-boot.log" \
  AMUX_HOME="$home" AMUX_BIN="$home/no-such-amux" AMUX_FLEET_BOOT_EPOCH="$epoch" \
    "$BOOT" >/dev/null 2>&1
  cat "$home/logs/fleet-boot.log" 2>/dev/null
  rm -rf "$home"
}

check() {
  local label="$1" out="$2" want="$3" notwant="$4"
  cells=$((cells+1))
  if ! printf '%s' "$out" | grep -q "$want"; then
    echo "FAIL [$label]: expected /$want/ in the log"
    printf '%s\n' "$out" | sed 's/^/    /'
    fails=$((fails+1))
    return
  fi
  if [[ -n "$notwant" ]] && printf '%s' "$out" | grep -q "$notwant"; then
    echo "FAIL [$label]: did not expect /$notwant/ in the log"
    fails=$((fails+1))
    return
  fi
  echo "ok   [$label]"
}

now=$(date +%s)

# CELL 1 — the specimen. Booted 6 hours before the agent ran: the fleet was down
# for six hours and the log has to say so, in minutes, computed.
out="$(run_gap $((now - 21600)))"
check "an overnight reboot reports the window it was down" "$out" \
      "LOGIN GAP: the machine booted 360m before" ""

# CELL 2 — a prompt login is NOT reported as a gap. Without this the warning is
# a constant that fires on every boot, which is exactly as green as one that
# measures, and would be ignored within a week.
out="$(run_gap $((now - 30)))"
check "a prompt login is not reported as a gap" "$out" \
      "login gap: " "LOGIN GAP:"

# CELL 3 — an unreadable boot time says UNMEASURED. A missing number must not
# read as zero, which would be the reassuring answer and the wrong one.
out="$(run_gap "not-an-epoch")"
check "an unreadable boot time is unmeasured, not zero" "$out" \
      "LOGIN GAP UNMEASURED" "login gap: "

echo
if (( fails )); then
  echo "test-fleet-boot-login-gap: $fails of $cells cell(s) FAILED"
  exit 1
fi
echo "test-fleet-boot-login-gap: $cells/$cells cells passed"
