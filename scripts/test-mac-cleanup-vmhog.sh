#!/bin/bash
# The VM-aware hog line of scripts/mac-cleanup-tick.sh (Mac hog RCA 20261020-225151).
# A VM process is the sum of its guest's work: with every container capped,
# goal-shared read 380-530% (its busy container at its 3-CPU cap, plus
# virtualization overhead) and escalated as a hog every tick. A VM escalates
# only when its guest escapes the caps. Each cell fails if its guard is removed
# (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { if printf '%s' "$1" | grep -q -- "$2"; then echo yes; else echo no; fi; }
export AMUX_CLEANUP_SCOPE_FILE=/dev/null AMUX_CLEANUP_SESSIONS_CMD=false AMUX_CLEANUP_PRESSURE_CMD="echo 1" AMUX_CLEANUP_HEALTH_PROBES=off
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
STATE_DIR="$FIX/state"; mkdir -p "$STATE_DIR"

# A fake docker: `stats` prints "name cpu%" from $FIX/stats, `inspect` the cap from $FIX/caps.
D="$FIX/docker"
cat > "$D" <<'F'
#!/bin/bash
shift 2   # --context <ctx>
case "$1" in
  stats) cat "$(dirname "$0")/stats" ;;
  inspect) awk -v n="${@: -1}" '$1==n{print $2}' "$(dirname "$0")/caps" ;;
esac
F
chmod +x "$D"
VM_DOCKER="$D"
running_vm_profiles() { echo goal-shared; }
printf 'ts-dropin-mx 3000000000\nsupabase 4000000000\nidle 0\n' > "$FIX/caps"

# Two ticks of the same VM pid at <pcpu>; prints the second tick's log and the verdicts.
hog_tick() { # <pcpu>
  VM_BUDGET=""; HEALTH_VERDICTS=""; rm -f -- "${STATE_DIR:?}/cpu-hogs.prev"
  HEALTH_PS_CMD="echo 1083 1 0 $1 01:00:00 /System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine"
  check_cpu_hogs 0 >/dev/null
  check_cpu_hogs 0
  printf '%s\n' "$HEALTH_VERDICTS" | sed -n 's/^hog /VERDICT hog /p'
}

echo "1. a VM at its guest's caps plus overhead is not a hog (the 20261020-225151 shape)"
printf 'ts-dropin-mx 299.5%%\nsupabase 3.1%%\nidle 0.0%%\n' > "$FIX/stats"
out=$(hog_tick 527.0)
check "the budget counts only busy containers' caps" "yes" "$(has "$out" 'budget=300 uncapped=0 busy=ts-dropin-mx:300%/3')"
check "it logs why it did not escalate"              "yes" "$(has "$out" 'verdict=vm_hog_within_caps')"
check "and adds no hog verdict"                      "no"  "$(has "$out" '^VERDICT hog ')"

echo "2. a VM over its caps plus overhead still escalates, naming its guest"
out=$(hog_tick 600.0)
check "over budget+overhead escalates"               "yes" "$(has "$out" '^VERDICT hog com.apple.Virtualization.VirtualMachine pid 1083 at 600.0% CPU.*(guest: budget=300')"

echo "3. a busy container with no cap escalates at any VM figure"
printf 'ts-dropin-mx 120%%\nidle 90.0%%\n' > "$FIX/stats"
out=$(hog_tick 400.0)
check "an uncapped busy container escalates"         "yes" "$(has "$out" '^VERDICT hog com.apple.Virtualization.VirtualMachine.*uncapped=1')"

echo "4. a guest nobody could measure never clears a hog"
running_vm_profiles() { :; }
out=$(VM_HOG_OVERHEAD_PCT=1000 hog_tick 400.0)   # even a line it would clear by size
check "no VM answered: escalates, says unmeasured"   "yes" "$(has "$out" '^VERDICT hog com.apple.Virtualization.VirtualMachine.*guest: unmeasured')"
running_vm_profiles() { echo goal-shared; }

echo "5. a non-VM process keeps the plain line"
HEALTH_VERDICTS=""; VM_BUDGET=""; rm -f -- "${STATE_DIR:?}/cpu-hogs.prev"
HEALTH_PS_CMD="echo 777 1 0 400.0 01:00:00 /usr/bin/burner"
check_cpu_hogs 0 >/dev/null; check_cpu_hogs 0 >/dev/null
check "a plain process over 300% escalates"          "yes" "$(has "$HEALTH_VERDICTS" 'hog burner pid 777 at 400.0% CPU on two ticks in a row (over 300%)$')"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-vmhog — all checks passed"; exit 0; fi
echo "mac-cleanup-vmhog: $fails check(s) FAILED"; exit 1
