#!/bin/bash
# The vm cpu caps arm of scripts/mac-cleanup-tick.sh (Mac hog RCA 20261020-115243).
# An uncapped container in a colima VM drove the VM to 500-1580% on the Mac; a
# one-off `docker update --cpus` was lost when its launcher recreated it. The arm
# reapplies a cap every tick to running containers that have none. Each cell
# fails if its guard is removed (ethos rule 7).
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

# A fake docker: `ps -q` lists ids, `inspect` prints "/name nanocpus", `update` records.
D="$FIX/docker"
cat > "$D" <<'F'
#!/bin/bash
shift 2   # --context <ctx>
case "$1" in
  ps) printf 'a\nb\nc\nd\n' ;;
  inspect) shift 3; for id in "$@"; do case $id in a) echo "/ts-dropin-mx 0";; b) echo "/ffe-pg 2000000000";; c) echo "/keep-me 0";; d) echo "/broken 0";; esac; done ;;
  update) [ "$4" = broken ] && exit 1; echo "$3 $4" >> "$(dirname "$0")/updates" ;;
esac
F
chmod +x "$D"
VM_DOCKER="$D"; VM_LIST_CMD="echo '{\"name\":\"goal-shared\",\"status\":\"Running\"}'"
running_vm_profiles() { echo goal-shared; }

echo "1. an uncapped container is capped, a capped one and an exempt one are left alone"
VM_CPU_CAP=4 VM_CPU_CAP_EXEMPT="keep-me"
out=$(cap_vm_container_cpus 0)
check "the uncapped container is capped"            "4 ts-dropin-mx" "$(grep ts-dropin-mx "$FIX/updates" 2>/dev/null)"
check "a container with its own cap is not touched" "no"  "$(grep -q ffe-pg "$FIX/updates" 2>/dev/null && echo yes || echo no)"
check "an exempt container is not touched"         "no"  "$(grep -q keep-me "$FIX/updates" 2>/dev/null && echo yes || echo no)"
check "the cap is logged with its undo"             "yes" "$(has "$out" 'capped ts-dropin-mx at 4 CPUs.*undo:.*verdict=vm_container_cpu_capped')"
check "a failed update says so"                     "yes" "$(has "$out" 'FAILED to cap broken.*verdict=vm_container_cpu_cap_failed')"
check "the summary counts every kind"               "yes" "$(has "$out" 'goal-shared capped=1 already_capped=1 exempt=1 failed=1 at 4 CPUs (verdict=vm_cpu_caps)')"

echo "2. 0 turns it off; a dry run changes nothing"
: > "$FIX/updates"
out=$(VM_CPU_CAP=0 cap_vm_container_cpus 0)
check "off says so"                                 "yes" "$(has "$out" 'vm cpu caps: off')"
check "and updates nothing"                         ""    "$(cat "$FIX/updates")"
out=$(cap_vm_container_cpus 1)
check "a dry run says what it would do"             "yes" "$(has "$out" 'would cap uncapped containers in colima VM goal-shared at 4 CPUs')"
check "and updates nothing"                         ""    "$(cat "$FIX/updates")"

echo "3. the tick runs the arm by default"
check "the default cap is 4"                        "4"   "$(AMUX_CLEANUP_LIB_ONLY=1 bash -c '. "$0" && echo $VM_CPU_CAP' "$TICK")"
check "the main flow calls it, gated only by the probes switch" "1" "$(grep -cxF '[ "$HEALTH_PROBES" != off ] && tick_arm "vm cpu caps" && cap_vm_container_cpus "$DRY"' "$TICK")"

echo "4. a VM the tick caps is judged against that cap, in the same tick (hog RCA 20261101-004551)"
check "the main flow caps before the health probes" "yes" "$(awk '/^\[ "\$HEALTH_PROBES" != off \] && tick_arm "vm cpu caps"/{c=NR} /^system_health "\$DRY"$/{h=NR} END{print (c && h && c < h) ? "yes" : "no"}' "$TICK")"
S="$FIX/vm-state"; printf '0\n' > "$S"
cat > "$D" <<F
#!/bin/bash
shift 2   # --context <ctx>
case "\$1" in
  ps) echo a ;;
  stats) echo 'mixpeek-hikes 426%' ;;
  inspect) if [ "\$2" = -f ]; then cat "$S"; else echo "/mixpeek-hikes \$(cat "$S")"; fi ;;
  update) echo 4000000000 > "$S" ;;
esac
F
VM_CPU_CAP_EXEMPT=""; STATE_DIR="$FIX/state"; mkdir -p "$STATE_DIR"
AV=com.apple.Virtualization.VirtualMachine
echo "4242 $AV" > "$STATE_DIR/cpu-hogs.prev"
HEALTH_PS_CMD="echo 4242 1 $(id -u) 444.0 09:00:00 /x/$AV"
owner_label_for_pid() { echo vm; }
hogs() { HEALTH_VERDICTS=""; VM_BUDGET=""; echo "4242 $AV" > "$STATE_DIR/cpu-hogs.prev"; check_cpu_hogs 0 > "$FIX/hog.out"; printf '%s' "$HEALTH_VERDICTS"; }
check "uncapped, the VM escalates"                  "yes" "$(has "$(hogs)" 'hog .*budget=0 uncapped=1')"
cap_vm_container_cpus 0 >/dev/null
check "capped by the arm, the same VM does not"     ""    "$(hogs)"
check "and the tick says why"                       "yes" "$(has "$(cat "$FIX/hog.out")" 'verdict=vm_hog_within_caps')"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-vmcpu — all checks passed"; exit 0; fi
echo "mac-cleanup-vmcpu: $fails check(s) FAILED"; exit 1
