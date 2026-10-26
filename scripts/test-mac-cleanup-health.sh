#!/bin/bash
# The system-health section of scripts/mac-cleanup-tick.sh: CPU hogs, leftover
# test processes, kernel limits and zombies, crash reports, launchd jobs,
# network and thermal (2026-10-13). Every probe is a seam; each cell stubs the
# one it tests, and the processes a cell stops are real children of this script
# so the kill path runs for real. Each cell fails if its guard is removed.
set -euo pipefail
export AMUX_CLEANUP_OPEN_WRITERS_CMD=false   # a disk bundle must not read the live lsof
export AMUX_CLEANUP_TARGET_KEEP=/nonexistent/amux-test-keep   # never prune the live shared target
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
STATE_DIR="$FIX/state"; CRASH_STATE="$FIX/state/state"; HEALTH_PROBES=on
AGENT_VOLUME_CMD="echo agent-volume: TestVol exists=no"   # never the live diskutil from a test
UTIL_CMD=true   # never the live server (scripts/test-mac-cleanup-utilization.sh tests it)
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

echo "2b. leftover cargo tests: an orphaned cargo test and its test binary are stopped, a build is not"
spawn CT  "$FIX/cargo-test"
spawn TB  "$FIX/test-bin"
spawn CB  "$FIX/cargo-build"
spawn CTP "$FIX/cargo-test-parented"
{
  printf '%s 1 %s 10:50:04 %s/.rustup/toolchains/1.94/bin/cargo test --lib -j 3 -- --test-threads 4\n' "$CT" "$ME" "$FIX"
  printf '%s 1 %s 10:50:06 %s/scratch/mvs-ne/target/debug/deps/mvs_shard-3c13 --test-threads 4\n'     "$TB" "$ME" "$FIX"
  printf '%s 1 %s 10:50:04 %s/.rustup/toolchains/1.94/bin/cargo build --release\n'                       "$CB" "$ME" "$FIX"
  printf '%s 4242 %s 10:50:04 %s/.rustup/toolchains/1.94/bin/cargo test --lib\n'                        "$CTP" "$ME" "$FIX"
} > "$FIX/ps.orph"
reap_test_orphans 0 > "$FIX/orph.out"; out=$(cat "$FIX/orph.out")
check "an orphaned cargo test is stopped"             gone  "$(alive "$CT")"
check "an orphaned test binary under target/*/deps is stopped" gone "$(alive "$TB")"
check "an orphaned cargo build is kept"               alive "$(alive "$CB")"
check "a cargo test whose parent is alive is kept"    alive "$(alive "$CTP")"
check "the summary counts two stopped"                yes   "$(has "$out" 'found 2, stopped 2')"

echo "2c. background daemons: a listed one of ours at normal priority goes to background policy"
{
  printf '87198 %s 58 270.0 /System/Library/PrivateFrameworks/MediaAnalysis.framework/Versions/A/mediaanalysisd\n' "$ME"
  printf '59525 %s 4 120.0 /System/Library/PrivateFrameworks/PhotoAnalysis.framework/Versions/A/Support/photoanalysisd\n' "$ME"
  printf '88481 0 46 90.0 /System/Library/PrivateFrameworks/MediaAnalysis.framework/Versions/A/mediaanalysisd\n'
  printf '40001 %s 46 10.0 /System/Library/PrivateFrameworks/PhotoAnalysis.framework/Versions/A/Support/photoanalysisd\n' "$ME"
  printf '40002 %s 46 300.0 /Applications/Busy.app/Contents/MacOS/Busy\n' "$ME"
} > "$FIX/ps.demote"
DEMOTE_PS_CMD="cat $FIX/ps.demote"; TASKPOLICY_CMD="$FIX/tp.sh -b -p"
printf '#!/bin/bash\necho "$@" >> %s/tp.log\n' "$FIX" > "$FIX/tp.sh"; chmod +x "$FIX/tp.sh"; rm -f "$FIX/tp.log"
out=$(demote_background_daemons 1)
check "a dry run demotes nothing"                     no    "$([ -s "$FIX/tp.log" ] && echo yes || echo no)"
check "and says what it would do"                     yes   "$(has "$out" 'mediaanalysisd pid 87198 at 270.0% pri 58: would set background policy (dry run)')"
demote_background_daemons 0 > "$FIX/demote.out"; out=$(cat "$FIX/demote.out")
check "ours, listed, busy, normal priority: demoted"  "-b -p 87198" "$(head -1 "$FIX/tp.log" 2>/dev/null)"
check "exactly one process was touched"               1     "$(grep -c . "$FIX/tp.log" | tr -d ' ')"
check "one already at background priority is left"   yes   "$(has "$out" 'photoanalysisd pid 59525 at 120.0%: already background')"
check "the summary counts one demoted"                yes   "$(has "$out" 'found 1, demoted 1')"
check "it is ledgered"                                yes   "$(grep -q "background	87198	mediaanalysisd" "$STATE_DIR/daemon-demote.log" 2>/dev/null && echo yes || echo no)"
DEMOTE_PS_CMD=true

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
stamp() { python3 -c 'import sys,time; print(time.strftime("%Y-%m-%d-%H%M%S", time.localtime(time.time() - float(sys.argv[1]))))' "$1"; }   # a name stamp <s> ago
for i in $(seq 1 12); do : > "$CRASH_DIR/Looper-$(stamp $((i * 60))).ips"; done
ONCE="Once-$(stamp 120).ips"; : > "$CRASH_DIR/$ONCE"
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
check "a recent report is kept"                       yes   "$([ -e "$CRASH_DIR/$ONCE" ] && echo yes || echo no)"

# Mac crashes RCA 20261025-060129: the line says what the reports say about who
# crashed, so "bash crashed 10 times" names its coalition, parent and Rosetta.
CRASH_DIR="$FIX/crash-ctx"; mkdir -p "$CRASH_DIR"
for i in $(seq 1 11); do
  python3 -c 'import json,sys; print(json.dumps({"bug_type":"309"})); print(json.dumps({"pid":100+int(sys.argv[1]),"parentProc":"bash","coalitionName":"com.amux.tmux-watchdog","translated":True,"exception":{"type":"EXC_CRASH","signal":"SIGSEGV"},"termination":{"byPid":100+int(sys.argv[1])},"procStartAbsTime":1000,"procExitAbsTime":1000+4800000}))' "$i" > "$CRASH_DIR/bash-$(stamp $((i * 60))).ips"
done
: > "$CRASH_DIR/bash-$(stamp 900).ips"   # an empty report is skipped, not counted as context
HEALTH_VERDICTS=""; check_crash_reports 0 >/dev/null
check "the crashes line carries what the reports say" yes "$(has "$HEALTH_VERDICTS" '^crashes bash crashed 12 times in 24h (at or over 10) (reports: coalition com.amux.tmux-watchdog x11; parent bash x11; signal SIGSEGV x11; 11/11 under Rosetta; 11 raised by the process itself; 11 alive under 1s (the watchdog started the tmux server: this means a process under tmux, a worker.s shell))$')"
check "a report with no body adds no context"         ""    "$(crash_context "$FIX/crash" Looper)"

echo "4b. a loop that stopped crashing 6h+ ago is reported, not escalated (RCA crashes 20261017-022150)"
CRASH_DIR="$FIX/crash-ended"; mkdir -p "$CRASH_DIR"
for i in $(seq 10 21); do : > "$CRASH_DIR/git-$(stamp $((8 * 3600 + i * 60))).ips"; done
perl -e 'my $t = time - 8*3600; utime $t, $t, @ARGV' "$CRASH_DIR"/git-*.ips
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b.out"; out=$(cat "$FIX/crash4b.out")
check "no constraint for a loop that ended"           no    "$(has "$HEALTH_VERDICTS" '^crashes git')"
check "it says so, with a verdict"                    yes   "$(has "$out" 'git crashed 12 times in 24h but none in the last 6h')"
# macOS rewrote an old report just now: its mtime is fresh, its crash is 8h old.
touch "$(ls "$CRASH_DIR"/git-*.ips | head -1)"
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b.out"; out=$(cat "$FIX/crash4b.out")
check "a rewritten old report does not revive an ended loop" no "$(has "$HEALTH_VERDICTS" '^crashes git')"
: > "$CRASH_DIR/git-$(stamp 30).ips"   # one fresh crash after the gap: a new episode of one
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b.out"; out=$(cat "$FIX/crash4b.out")
check "one fresh crash does not revive the ended loop (RCA crashes 20261020-085229)" no "$(has "$HEALTH_VERDICTS" '^crashes git')"
check "it says so, with a verdict"                    yes   "$(has "$out" 'git crashed 13 times in 24h, but only 1 since its last 6h without a crash.*verdict=crash_new_episode_under_threshold')"
for i in $(seq 2 10); do : > "$CRASH_DIR/git-$(stamp $((30 + i * 20))).ips"; done   # a fresh loop after the gap
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b.out"; out=$(cat "$FIX/crash4b.out")
check "a fresh loop after the gap is a constraint again" yes "$(has "$HEALTH_VERDICTS" '^crashes git crashed 22 times in 24h')"

echo "4b2. a live loop with no crash since the last crashes escalation is not escalated again (RCA crashes 20261020-015312)"
CRASH_DIR="$FIX/crash-handed"; mkdir -p "$CRASH_DIR" "$FIX/state"
# 12 crashes 5h50m to 6h ago, all before an escalation 4h ago: the 00:02Z ChatGPT
# loop at the 01:53 local tick, 10 minutes inside the 6h quiet window.
for i in $(seq 1 12); do : > "$CRASH_DIR/browser_crashpad_handler-$(stamp $((21000 + i * 30))).ips"; done
perl -e 'my $t = time - 21000; utime $t, $t, @ARGV' "$CRASH_DIR"/browser_crashpad_handler-*.ips
printf 'esc_crashes=%s\n' "$(( $(date +%s) - 4 * 3600 ))" > "$CRASH_STATE"
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b2.out"; out=$(cat "$FIX/crash4b2.out")
check "no constraint: every crash predates the last escalation" no "$(has "$HEALTH_VERDICTS" '^crashes browser_crashpad_handler')"
check "it says so, with a verdict"                    yes   "$(has "$out" 'browser_crashpad_handler crashed 12 times in 24h, the newest .* before the last crashes escalation .*verdict=crash_loop_already_escalated')"
: > "$CRASH_DIR/browser_crashpad_handler-$(stamp 60).ips"   # one crash after the escalation
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b2.out"; out=$(cat "$FIX/crash4b2.out")
# RCA crashes 20261021-165921: Muse's handler crashed once 8 minutes after the
# escalation of the 10:54 burst, and that one crash re-escalated all 25.
check "one crash after the escalation does not re-raise the burst" no "$(has "$HEALTH_VERDICTS" '^crashes browser_crashpad_handler')"
check "it says how many are new, with a verdict"     yes   "$(has "$out" 'crashed 13 times in 24h, 1 since the last crashes escalation .*verdict=crash_loop_already_escalated_new_under_threshold')"
for i in $(seq 2 10); do : > "$CRASH_DIR/browser_crashpad_handler-$(stamp $((60 + i * 20))).ips"; done   # a new loop of 10 since
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b2.out"
check "a new loop of 10 after the escalation is a constraint again" yes "$(has "$HEALTH_VERDICTS" '^crashes browser_crashpad_handler crashed 22 times in 24h')"
rm -f "$CRASH_STATE"
HEALTH_VERDICTS=""; check_crash_reports 1 > "$FIX/crash4b2.out"
check "control: never escalated, the live loop is a constraint" yes "$(has "$HEALTH_VERDICTS" '^crashes browser_crashpad_handler')"

echo "4b3. a crash handler relaunched every second is a respawn loop, counted by launches (RCA crashes 20261021-235935)"
# Muse's crash handler died on launch once a second for 13 hours; ReportCrash
# wrote 25 reports per burst and stopped, so the reports read as small bursts.
CRASH_DIR="$FIX/crash-respawn"; mkdir -p "$CRASH_DIR"; rm -f "$CRASH_STATE"
for i in $(seq 1 25); do printf '{"app_name":"EndoCrashpadHandler"}\n{"responsibleProc":"Muse","procPath":"/Applications/Muse.app/Contents/MacOS/EndoCrashpadHandler"}\n' \
  > "$CRASH_DIR/EndoCrashpadHandler-$(stamp $((600 + i * 2))).ips"; done
printf '#!/bin/bash\necho "597 EndoCrashpadHan"; echo "111 nohup"\n' > "$FIX/respawn.sh"
printf '#!/bin/bash\necho "40 EndoCrashpadHan"\n' > "$FIX/respawn-low.sh"; chmod +x "$FIX/respawn.sh" "$FIX/respawn-low.sh"
RESPAWN_CMD="$FIX/respawn.sh" RESPAWN_PID_CMD="echo 51370"
HEALTH_VERDICTS=""; { check_respawn_loops; check_crash_reports 1; } > "$FIX/crash4b3.out"; out=$(cat "$FIX/crash4b3.out")
check "the loop is a constraint, by launches"           yes "$(has "$HEALTH_VERDICTS" '^crashes EndoCrashpadHan relaunched 597 times in 5m (over 100)')"
check "it names the app holding it and the remedy"      yes "$(has "$HEALTH_VERDICTS" 'held by Muse (pid 51370).*quits and reopens Muse')"
check "a busy but ordinary launcher is not a loop"      no  "$(has "$HEALTH_VERDICTS" 'nohup')"
# RCA crashes 20261022-105614: nohup, 111 launches in 5m and no crash report,
# escalated as a crash loop held by an unknown app.
check "a launcher over 100 with no crash reports is named as busy, not a loop" yes "$(has "$out" 'nohup launched 111 times in 5m with no crash report in 24h.*verdict=respawn_busy_no_crashes')"
check "its crash reports are not raised a second time"  1   "$(printf '%s\n' "$HEALTH_VERDICTS" | grep -c '^crashes Endo')"
check "they are named as the loop's reports"            yes "$(has "$out" 'EndoCrashpadHandler crashed 25 times in 24h, reports of the live respawn loop EndoCrashpadHan .*verdict=crash_reports_from_respawn_loop')"
check "the escalation key is the app and its pid"       "EndoCrashpadHan:Muse:51370" "$RESPAWN_ESC_KEY"
printf 'esc_crashes=%s\nesc_respawn_key=EndoCrashpadHan:Muse:51370\n' "$(( $(date +%s) - 3600 ))" > "$CRASH_STATE"
HEALTH_VERDICTS=""; { check_respawn_loops; check_crash_reports 1; } > "$FIX/crash4b3.out"; out=$(cat "$FIX/crash4b3.out")
check "escalated, the same Muse still looping waits on its owner" yes "$(has "$out" 'verdict=respawn_loop_owner_pending')"
check "and is not a constraint again"                    no  "$(has "$HEALTH_VERDICTS" '^crashes Endo')"
HEALTH_VERDICTS=""; RESPAWN_PID_CMD="echo 60001" check_respawn_loops > "$FIX/crash4b3.out"
check "a reopened Muse that loops again is a new constraint" yes "$(has "$HEALTH_VERDICTS" '^crashes EndoCrashpadHan relaunched')"
HEALTH_VERDICTS=""; RESPAWN_CMD="$FIX/respawn-low.sh" check_respawn_loops > "$FIX/crash4b3.out"
check "control: under 100 launches is no loop"           no  "$(has "$HEALTH_VERDICTS" '^crashes')"
RESPAWN_CMD=true; RESPAWN_LOOPS=""; rm -f "$CRASH_STATE"

echo "4c. an orphaned crash handler whose executable is gone is stopped (RCA crashes 20261019-195257)"
# Real processes, each pid from $! (never a pattern match), each orphaned to
# launchd. A compiled stand-in: macOS kills a copied system binary on launch.
H="$FIX/App.app/Versions/154/Helpers"; mkdir -p "$H"
if printf '#include <unistd.h>\nint main(void){sleep(60);return 0;}\n' | cc -x c - -o "$FIX/handler.bin" 2>/dev/null; then
cp "$FIX/handler.bin" "$H/browser_crashpad_handler"; cp "$FIX/handler.bin" "$FIX/kept_crashpad_handler"; cp "$FIX/handler.bin" "$FIX/other_tool"
printf '#!/bin/bash\nsleep 60\n' > "$FIX/names_crashpad_handler.sh"; chmod +x "$FIX/names_crashpad_handler.sh"
# Not inside $(...): an orphan started there dies before it is measured.
spawn() { local f=$1; shift; ( ( exec "$@" >/dev/null 2>&1 ) & echo $! > "$f" ); }
spawn "$FIX/gone.pid" "$H/browser_crashpad_handler" --monitor-self; gone=$(cat "$FIX/gone.pid")
spawn "$FIX/kept.pid" "$FIX/kept_crashpad_handler" --monitor-self; kept=$(cat "$FIX/kept.pid")
spawn "$FIX/named.pid" /bin/bash "$FIX/names_crashpad_handler.sh" "$FIX/gone/browser_crashpad_handler"; named=$(cat "$FIX/named.pid")
# Any other orphan whose binary an upgrade deleted is not a crash handler, even if its args name one.
spawn "$FIX/other.pid" "$FIX/other_tool" --report-to=browser_crashpad_handler; other=$(cat "$FIX/other.pid")
sleep 0.5; rm -f "$H/browser_crashpad_handler" "$FIX/other_tool"
ORPHAN_PS_CMD="ps -o pid=,ppid=,uid=,etime=,args= -p $gone,$kept,$named,$other"
out=$(reap_orphan_crash_handlers 1)
check "a dry run names the orphan whose executable is gone" yes "$(has "$out" "pid $gone .*executable is gone")"
check "and stops nothing"                             yes   "$(kill -0 "$gone" 2>/dev/null && echo yes || echo no)"
reap_orphan_crash_handlers 0 > "$FIX/orph.out"; out=$(cat "$FIX/orph.out")
check "the orphan with a deleted executable is stopped" no  "$(kill -0 "$gone" 2>/dev/null && echo yes || echo no)"
check "control: a handler whose executable exists is kept" yes "$(kill -0 "$kept" 2>/dev/null && echo yes || echo no)"
check "control: a script that only names a missing handler is kept" yes "$(kill -0 "$named" 2>/dev/null && echo yes || echo no)"
check "control: another orphan with a deleted binary is kept" yes "$(kill -0 "$other" 2>/dev/null && echo yes || echo no)"
check "the line counts it, with its verdict"          yes   "$(has "$out" 'found 1, stopped 1 (verdict=orphan_crash_handlers)')"
for p in "$gone" "$kept" "$named" "$other"; do if [[ "$p" =~ ^[0-9]+$ ]]; then kill "$p" 2>/dev/null || true; fi; done
else echo "  skip: no C compiler to build a stand-in crash handler"; fi

echo "4d. an orphaned multiprocessing worker is stopped, other orphans are kept (RCA memory 20261019-205313)"
# Real python processes started by exact pid ($!), each orphaned to launchd.
mpspawn() { local f=$1; shift; ( ( exec "$@" >/dev/null 2>&1 ) & echo $! > "$f" ); }   # not inside $(...)
mpspawn "$FIX/mp.pid" python3 -c 'import time; time.sleep(60)  # from multiprocessing.spawn import spawn_main; spawn_main(tracker_fd=9, pipe_handle=21)' --multiprocessing-fork
mpspawn "$FIX/py.pid" python3 -c 'import time; time.sleep(60)'
mp=$(cat "$FIX/mp.pid"); py=$(cat "$FIX/py.pid"); sleep 0.5
ORPHAN_PS_CMD="ps -o pid=,ppid=,uid=,etime=,args= -p $mp,$py"
out=$(MP_ORPHAN_MIN=600; reap_orphan_mp_workers 0)
check "control: a worker younger than the threshold is kept" yes "$(kill -0 "$mp" 2>/dev/null && echo yes || echo no)"
out=$(MP_ORPHAN_MIN=0; reap_orphan_mp_workers 1)
check "a dry run names it"                            yes   "$(has "$out" "orphaned multiprocessing worker pid $mp")"
check "and stops nothing"                             yes   "$(kill -0 "$mp" 2>/dev/null && echo yes || echo no)"
MP_ORPHAN_MIN=0 reap_orphan_mp_workers 0 > "$FIX/mp.out"; out=$(cat "$FIX/mp.out")
check "an orphaned multiprocessing worker is stopped" no    "$(kill -0 "$mp" 2>/dev/null && echo yes || echo no)"
check "control: an orphaned plain python is kept"     yes   "$(kill -0 "$py" 2>/dev/null && echo yes || echo no)"
check "the line counts it, with its verdict"          yes   "$(has "$out" 'found 1, stopped 1 (verdict=mp_orphans)')"
check "it is ledgered"                                yes   "$(grep -q "stopped	$mp	" "$STATE_DIR/mp-orphans.log" 2>/dev/null && echo yes || echo no)"
for p in "$mp" "$py"; do if [[ "$p" =~ ^[0-9]+$ ]]; then kill "$p" 2>/dev/null || true; fi; done

echo "4e. a process in a Claude session scratchpad is named by that session's worker"
SDIR="$FIX/claude-501/-proj/0123abcd-0000-4000-8000-0123456789ab/work"; mkdir -p "$SDIR"
( ( cd "$SDIR" && exec sleep 30 ) & echo $! > "$FIX/s.pid" ); spid=$(cat "$FIX/s.pid"); sleep 0.3
check "an orphan in a live session's scratchpad is named by its worker" "worker-x (orphan, by path)" "$(AMUX_CLEANUP_SESSION_MAP="0123abcd-0000-4000-8000-0123456789ab worker-x" lane_for_pid "$spid" "999999 amux-other")"
check "control: an unknown session id has no lane"    "no lane" "$(AMUX_CLEANUP_SESSION_MAP="ffffffff-0000-4000-8000-0123456789ab worker-x" lane_for_pid "$spid" "999999 amux-other")"
if [[ "$spid" =~ ^[0-9]+$ ]]; then kill "$spid" 2>/dev/null || true; fi

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
# orph three times: leftover test processes, multiprocessing orphans and crash handlers all read the process list.
check "control: switched on, every probe runs"        "cpu orph orph orph lim net" "$(tr '\n' ' ' < "$FIX/probe.hits" | sed 's/ $//')"

echo "7. the whole tick carries the section, its constraints and its counters (macOS only)"
if [ "$(uname)" = Darwin ]; then
  out=$(AMUX_CLEANUP_HEALTH_PS_CMD=true AMUX_CLEANUP_ORPHAN_PS_CMD=true AMUX_CLEANUP_LAUNCHD_LIST_CMD=true \
        AMUX_CLEANUP_NET_CMD=true AMUX_CLEANUP_THERMAL_CMD=true AMUX_CLEANUP_CRASH_DIR="$FIX/none" \
        AMUX_CLEANUP_LIMITS_CMD="echo 10 1000 10 100 5 300 $$ 300" \
        AMUX_CLEANUP_SELF_DB="$FIX/no-self.db" \
        AMUX_CLEANUP_AGENT_VOLUME_CMD="echo agent-volume: TestVol exists=no" AMUX_CLEANUP_UTIL_CMD=true \
        AMUX_CLEANUP_STATE_DIR="$FIX/tick" AMUX_CLEANUP_TARGET_ROOTS="$FIX/none" AMUX_CLEANUP_PURGE_CMD=true \
        AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/none" AMUX_CLEANUP_LANE_TMP_ROOT="$FIX/none" AMUX_CLEANUP_USER_TMP="$FIX/none" \
        AMUX_CLEANUP_CHURN_ROOTS="$FIX/none" AMUX_CLEANUP_VM_LIST_CMD=true AMUX_CLEANUP_SESSIONS_CMD=false \
        AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 AMUX_CLEANUP_HISTORY_CMD=true AMUX_CLEANUP_CARD_CMD=true \
        AMUX_CLEANUP_RECLAIM_TRIGGER_CMD="${RECLAIM_TRIGGER_STUB:-echo 204}" AMUX_CLEANUP_DEMOTE_PS_CMD=true AMUX_CLEANUP_ESCALATE_CMD=true bash "$TICK" --dry-run 2>&1)
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
