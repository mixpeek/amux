#!/bin/bash
# The system-health section of scripts/mac-cleanup-tick.sh: CPU hogs, leftover
# test processes, kernel limits and zombies, crash reports, launchd jobs,
# network and thermal (2026-10-13). Every probe is a seam; each cell stubs the
# one it tests, and the processes a cell stops are real children of this script
# so the kill path runs for real. Each cell fails if its guard is removed.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
export AMUX_CLEANUP_FSEVENTSD_CMD=true AMUX_CLEANUP_SQLITE_PS_CMD=true
PIDS=()
cleanup() { local p; for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -KILL "$p" 2>/dev/null || true; done; rm -rf -- "$FIX"; }
trap cleanup EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { printf '%s' "$1" | grep -q -- "$2" && echo yes || echo no; }
alive() { kill -0 "$1" 2>/dev/null && echo alive || echo gone; }
AMUX_CLEANUP_LIB_ONLY=1 AMUX_CLEANUP_SCOPE_FILE=/dev/null . "$TICK"
STATE_DIR="$FIX/state"; HEALTH_PROBES=on
AGENT_VOLUME_CMD="echo agent-volume: TestVol exists=no"   # never the live diskutil from a test
SELF_DB="$FIX/no-self.db"   # the self-check must not read the live run history (scripts/test-mac-cleanup-self.sh tests it)
ME=$(id -u)
# A long-lived real child. Its name and args in each cell come from the fixture
# ps listing, so a plain sleep serves (macOS kills a copied /bin/sleep at exec).
# Never spawned in $(...): the child would hold the substitution's stdout open.
spawn() { # <var> <path named in the fixture>
  sleep 300 >/dev/null 2>&1 &
  eval "$1=$!"; PIDS+=("$!"); disown "$!"   # disowned: the shell would print each kill as a job message
}

echo "1. a CPU hog on two ticks: a listed macOS helper is restarted, anything else is reported"
rm -rf -- "${STATE_DIR:?}"
spawn RC "$FIX/bin/ReportCrash"
spawn OTHER "$FIX/bin/builder"
spawn ROOTISH "$FIX/bin2/ReportCrash"
printf '%s 1 %s 95.0 1-00:00:00 %s\n%s 1 %s 120.0 2:00:00 %s\n%s 1 0 99.0 5:00:00 %s\n' \
  "$RC" "$ME" "$FIX/bin/ReportCrash" "$OTHER" "$ME" "$FIX/bin/builder" "$ROOTISH" "$FIX/bin2/ReportCrash" > "$FIX/ps.cpu"
HEALTH_PS_CMD="cat $FIX/ps.cpu"; HEALTH_VERDICTS=""
out=$(check_cpu_hogs 0)
check "one tick over the line restarts nothing"       alive "$(alive "$RC")"
check "and says so"                                   yes   "$(has "$out" '3 at or over 80% now, 0 also last tick, restarted 0')"
HEALTH_VERDICTS=""; check_cpu_hogs 0 > "$FIX/cpu.out"; out=$(cat "$FIX/cpu.out")
check "the listed helper is restarted on the second tick" gone "$(alive "$RC")"
check "the restart is ledgered"                       yes   "$(grep -q "restarted	$RC	ReportCrash" "$STATE_DIR/cpu-restarts.log" 2>/dev/null && echo yes || echo no)"
check "an unlisted hog is left running"               alive "$(alive "$OTHER")"
check "and reported"                                  yes   "$(has "$out" "cpu hog builder pid=$OTHER at 120.0% for two ticks, reported, not touched")"
check "a listed name owned by another user is never touched" alive "$(alive "$ROOTISH")"
check "under the escalation line nothing escalates"   ""    "$HEALTH_VERDICTS"
printf '%s 1 %s 450.0 2:00:00 %s\n' "$OTHER" "$ME" "$FIX/bin/builder" > "$FIX/ps.cpu"
HEALTH_VERDICTS=""; check_cpu_hogs 0 >/dev/null; HEALTH_VERDICTS=""; check_cpu_hogs 0 >/dev/null
check "a sustained hog over the escalation line is a constraint" yes "$(has "$HEALTH_VERDICTS" "^hog builder pid $OTHER at 450.0% CPU")"
printf '%s 1 %s 95.0 1:00:00 %s\n' "$RC" "$ME" "$FIX/bin/ReportCrash" > "$FIX/ps.cpu"; spawn RC "$FIX/bin/ReportCrash"
printf '%s 1 %s 95.0 1:00:00 %s\n' "$RC" "$ME" "$FIX/bin/ReportCrash" > "$FIX/ps.cpu"
rm -rf -- "${STATE_DIR:?}"; check_cpu_hogs 1 >/dev/null; check_cpu_hogs 1 > "$FIX/cpu.out"
check "a dry run keeps no memory of the last tick"   no    "$([ -e "$STATE_DIR/cpu-hogs.prev" ] && echo yes || echo no)"
check_cpu_hogs 0 >/dev/null; check_cpu_hogs 1 > "$FIX/cpu.out"
check "a dry run restarts nothing"                    alive "$(alive "$RC")"
check "it says it would"                              yes   "$(has "$(cat "$FIX/cpu.out")" 'would be restarted (dry run)')"

echo "2. leftover test processes: only an old orphan running out of a pytest temp dir"
PT="$FIX/T/pytest-of-u/pytest-7/popen-gw3/test_x0/bin"
spawn ORPH "$PT/kubectl"
spawn LIVEP "$PT/kubectl2"
spawn YOUNG "$PT/kubectl3"
spawn PLAIN "$FIX/bin/kubectl"
spawn THEIRS "$PT/kubectl4"
{
  printf '%s 1 %s 3:00:00 %s --context=x get pods\n'    "$ORPH"   "$ME" "$PT/kubectl"
  printf '%s 4242 %s 3:00:00 %s --context=x get pods\n' "$LIVEP"  "$ME" "$PT/kubectl2"
  printf '%s 1 %s 10:00 %s --context=x get pods\n'      "$YOUNG"  "$ME" "$PT/kubectl3"
  printf '%s 1 %s 3:00:00 %s --context=x\n'             "$PLAIN"  "$ME" "$FIX/bin/kubectl"
  printf '%s 1 0 3:00:00 %s --context=x\n'              "$THEIRS"       "$PT/kubectl4"
} > "$FIX/ps.orph"
ORPHAN_PS_CMD="cat $FIX/ps.orph"
out=$(reap_test_orphans 1)
check "a dry run stops nothing"                       alive "$(alive "$ORPH")"
check "and counts the one it would stop"              yes   "$(has "$out" 'found 1, stopped 0 (dry run)')"
reap_test_orphans 0 > "$FIX/orph.out"; out=$(cat "$FIX/orph.out")
check "the old orphan under a pytest dir is stopped"  gone  "$(alive "$ORPH")"
check "it is ledgered"                                yes   "$(grep -q "stopped	$ORPH	" "$STATE_DIR/test-orphans.log" 2>/dev/null && echo yes || echo no)"
check "one whose parent is alive is kept"             alive "$(alive "$LIVEP")"
check "a young one is kept"                           alive "$(alive "$YOUNG")"
check "one outside a pytest dir is kept"              alive "$(alive "$PLAIN")"
check "another user's is kept"                        alive "$(alive "$THEIRS")"
check "the summary counts one stopped"                yes   "$(has "$out" 'found 1, stopped 1')"

echo "3. limits: a limit at or over the share is a constraint, a zombie-holding parent is named"
HEALTH_VERDICTS=""; LIMITS_CMD="echo 900 1000 10 100 5 3 4242 2"; out=$(check_limits; echo "V=$HEALTH_VERDICTS")
check "open files at 90% is a constraint"             yes   "$(has "$out" '^limits open files 900 is 90% of kern.maxfiles 1000')"
check "a few zombies are reported, not escalated"     no    "$(has "$out" '^zombies')"
check "the parent holding them is named"              yes   "$(has "$out" 'pid 4242: 2)')"
HEALTH_VERDICTS=""; LIMITS_CMD="echo 10 1000 10 100 5 250 $$ 250"; check_limits > "$FIX/lim.out"
check "a parent holding ZOMBIE_PARENT_N zombies is a constraint" yes "$(has "$HEALTH_VERDICTS" "^zombies .*(pid $$) holds 250 unreaped children")"
check "nothing else is"                               1     "$(printf '%s\n' "$HEALTH_VERDICTS" | grep -c .)"
HEALTH_VERDICTS=""; LIMITS_CMD="echo 10 1000 95 100 90 0 - 0"; check_limits > "$FIX/lim.out"
check "user processes at 95% is a constraint"         yes   "$(has "$HEALTH_VERDICTS" '^limits this user runs 95 processes')"
check "inodes at 90% is a constraint"                 yes   "$(has "$HEALTH_VERDICTS" '^limits inodes 90% used')"
HEALTH_VERDICTS=""; LIMITS_CMD="echo 10 1000 10 100 5 0 - 0"; out=$(check_limits)
check "under every limit: no constraint"              ""    "$HEALTH_VERDICTS"
check "and the line says so"                          yes   "$(has "$out" 'none at a limit')"
LIMITS_CMD=true; out=$(check_limits)
check "an empty probe is unmeasured, not healthy"     yes   "$(has "$out" 'limits: unmeasured')"

echo "4. crash reports: a crash loop escalates, old reports are pruned, nothing else is"
CRASH_DIR="$FIX/crash"; mkdir -p "$CRASH_DIR"
for i in $(seq 1 12); do : > "$CRASH_DIR/Looper-2026-10-13-0101$(printf '%02d' "$i").ips"; done
: > "$CRASH_DIR/Once-2026-10-13-010101.ips"
: > "$CRASH_DIR/Old-2020-01-01-010101.ips"; : > "$CRASH_DIR/Old-2020-01-02-010101.crash"; : > "$CRASH_DIR/notes.txt"
touch -t 202001010000 "$CRASH_DIR/Old-2020-01-01-010101.ips" "$CRASH_DIR/Old-2020-01-02-010101.crash" "$CRASH_DIR/notes.txt"
HEALTH_VERDICTS=""; out=$(check_crash_reports 1; echo "V=$HEALTH_VERDICTS")
check "a dry run prunes nothing"                      yes   "$([ -e "$CRASH_DIR/Old-2020-01-01-010101.ips" ] && echo yes || echo no)"
check "and says what it would prune"                  yes   "$(has "$out" 'would prune 2, dry run')"
HEALTH_VERDICTS=""; check_crash_reports 0 > "$FIX/crash.out"; out=$(cat "$FIX/crash.out")
check "an app crashing 12 times in 24h is a constraint" yes "$(has "$HEALTH_VERDICTS" '^crashes Looper crashed 12 times in 24h')"
check "one crash is not"                              no    "$(has "$HEALTH_VERDICTS" 'Once')"
check "the line names the crashers with counts"       yes   "$(has "$out" 'Looper x12')"
check "old reports are pruned"                        no    "$([ -e "$CRASH_DIR/Old-2020-01-01-010101.ips" ] && echo yes || echo no)"
check "the count is reported"                         yes   "$(has "$out" 'pruned 2 older than 30d')"
check "a file that is not a crash report is kept, however old" yes "$([ -e "$CRASH_DIR/notes.txt" ] && echo yes || echo no)"
check "a recent report is kept"                       yes   "$([ -e "$CRASH_DIR/Once-2026-10-13-010101.ips" ] && echo yes || echo no)"

echo "5. report-only probes: launchd, network, thermal"
printf 'PID\tStatus\tLabel\n-\t0\tcom.amux.ok\n-\t78\tcom.amux.broken\n123\t1\tcom.amux.running\n-\t1\tcom.apple.noise\n' > "$FIX/launchd"
LAUNCHD_LIST_CMD="cat $FIX/launchd"; out=$(check_launchd_failing)
check "a stopped job with a non-zero exit is named"   yes   "$(has "$out" 'com.amux.broken (last exit 78)')"
check "a running job is not"                          no    "$(has "$out" 'com.amux.running')"
check "com.apple jobs are not"                        no    "$(has "$out" 'com.apple.noise')"
check "a clean exit is not"                           no    "$(has "$out" 'com.amux.ok')"
NET_CMD="echo 10.0.0.1 33 41"; out=$(check_network)
check "network line carries gateway, loss and DNS time" yes "$(has "$out" 'gateway 10.0.0.1 loss 33% over 3 pings, DNS lookup 41ms')"
NET_CMD=true; out=$(check_network)
check "no route is said, not read as healthy"         yes   "$(has "$out" 'no default route')"
printf 'CPU_Speed_Limit \t= 70\n' > "$FIX/therm"; THERMAL_CMD="cat $FIX/therm"; out=$(check_thermal)
check "a speed limit under 100 is called throttled"   yes   "$(has "$out" 'CPU speed limit 70% (throttled)')"
THERMAL_CMD=true; out=$(check_thermal)
check "an empty thermal probe is unmeasured"          yes   "$(has "$out" 'thermal: unmeasured')"

echo "6. the off switch runs no probe at all"
: > "$FIX/probe.hits"
for k in cpu orph lim net; do printf '#!/bin/sh\necho %s >> %s\n' "$k" "$FIX/probe.hits" > "$FIX/probe-$k"; chmod +x "$FIX/probe-$k"; done
HEALTH_PS_CMD="$FIX/probe-cpu"; ORPHAN_PS_CMD="$FIX/probe-orph"; LIMITS_CMD="$FIX/probe-lim"; NET_CMD="$FIX/probe-net"
HEALTH_PROBES=off; out=$(system_health 0)
check "nothing probed"                                ""    "$(cat "$FIX/probe.hits")"
check "and it says the probes are off"                yes   "$(has "$out" 'probes off')"
HEALTH_PROBES=on; system_health 0 >/dev/null
check "control: switched on, every probe runs"        "cpu orph lim net" "$(tr '\n' ' ' < "$FIX/probe.hits" | sed 's/ $//')"

echo "7. the whole tick carries the section, its constraints and its counters (macOS only)"
if [ "$(uname)" = Darwin ]; then
  out=$(AMUX_CLEANUP_HEALTH_PS_CMD=true AMUX_CLEANUP_ORPHAN_PS_CMD=true AMUX_CLEANUP_LAUNCHD_LIST_CMD=true \
        AMUX_CLEANUP_NET_CMD=true AMUX_CLEANUP_THERMAL_CMD=true AMUX_CLEANUP_CRASH_DIR="$FIX/none" \
        AMUX_CLEANUP_LIMITS_CMD="echo 10 1000 10 100 5 300 $$ 300" \
        AMUX_CLEANUP_SELF_DB="$FIX/no-self.db" \
        AMUX_CLEANUP_AGENT_VOLUME_CMD="echo agent-volume: TestVol exists=no" \
        AMUX_CLEANUP_STATE_DIR="$FIX/tick" AMUX_CLEANUP_TARGET_ROOTS="$FIX/none" AMUX_CLEANUP_PURGE_CMD=true \
        AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/none" AMUX_CLEANUP_LANE_TMP_ROOT="$FIX/none" AMUX_CLEANUP_USER_TMP="$FIX/none" \
        AMUX_CLEANUP_CHURN_ROOTS="$FIX/none" AMUX_CLEANUP_VM_LIST_CMD=true AMUX_CLEANUP_SESSIONS_CMD=false \
        AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 AMUX_CLEANUP_HISTORY_CMD=true AMUX_CLEANUP_CARD_CMD=true \
        AMUX_CLEANUP_ESCALATE_CMD=true bash "$TICK" --dry-run 2>&1)
  check "the tick prints the limits line"             yes   "$(has "$out" 'mac-cleanup: limits: open files 10/1000')"
  check "the zombie constraint reaches the assess stage" yes "$(has "$out" "constraint zombies .* would escalate (dry run)")"
  check "the done line carries the health counters"   yes   "$(has "$out" 'cpu_restarted=0 test_orphans=0 crash_pruned=0')"
else
  echo "  skip macOS-only cell (the tick's other probes are macOS commands): not run on $(uname)"
fi

echo "8. the agent volume's state is reported beside the other readings"
printf '#!/bin/sh\necho "agent-volume: TestVol exists=yes mounted=yes fseventsd=on"\necho "next (owner): do it"\n' > "$FIX/agentvol"; chmod +x "$FIX/agentvol"
AGENT_VOLUME_CMD="$FIX/agentvol"; out=$(check_agent_volume)
check "the status line is carried"                    yes   "$(has "$out" 'mac-cleanup: agent volume: TestVol exists=yes mounted=yes fseventsd=on')"
check "and so is the next step"                       yes   "$(has "$out" 'mac-cleanup:   agent volume next (owner): do it')"
out=$(system_health 0 2>&1)
check "system_health runs it"                         yes   "$(has "$out" 'mac-cleanup: agent volume: TestVol exists=yes')"
AGENT_VOLUME_CMD=false; out=$(check_agent_volume)
check "no answer reads unmeasured, not fine"          yes   "$(has "$out" 'agent volume: unmeasured')"
AGENT_VOLUME_CMD="echo agent-volume: TestVol exists=no"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-health — all checks passed"; exit 0; fi
echo "mac-cleanup-health: $fails check(s) FAILED"; exit 1
