#!/bin/bash
# The fseventsd arm of scripts/mac-cleanup-tick.sh (DESKT-81). fseventsd grew from
# under 10G to 76G over 2026-10-03..05 while the tick printed its size and never
# its cause. When it is hot the tick now names the busiest directories and
# escalates the class. Each cell fails if its guard is removed (ethos rule 7).
set -euo pipefail
export AMUX_CLEANUP_OPEN_WRITERS_CMD=false   # a disk bundle must not read the live lsof
export AMUX_CLEANUP_TARGET_KEEP=/nonexistent/amux-test-keep   # never prune the live shared target
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
export AMUX_CLEANUP_FSEVENTSD_CMD=true
export AMUX_CLEANUP_SQLITE_PS_CMD=true   # the live process table must not decide a test (DESKT-94)
export AMUX_CLEANUP_HEALTH_PROBES=off    # the health probes must not stop or read live processes in a test
export AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/no-claude-tmp"
export AMUX_CLEANUP_CHURN_SAMPLE_CMD=off   # cells 1-2 pin the census; cell 3 drives the FSEvents sample
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { if printf '%s' "$1" | grep -q -- "$2"; then echo yes; else echo no; fi; }
export AMUX_CLEANUP_SCOPE_FILE=/dev/null AMUX_CLEANUP_SESSIONS_CMD=false AMUX_CLEANUP_VM_LIST_CMD=true AMUX_CLEANUP_PRESSURE_CMD="echo 1"
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"

R="$FIX/roots"
mkfiles() { mkdir -p "$1"; local i; for i in $(seq 1 "$2"); do : > "$1/f$i"; done; }
mkfiles "$R/a/proj/sess/scratchpad/tip/server" 50     # busy, and deeper than the grouping
mkfiles "$R/a/proj/quiet" 5                            # under the floor
mkfiles "$R/a/proj/old/x" 50; find "$R/a/proj/old" -exec touch -t 202001010000 {} +
mkfiles "$R/b/lane/tmp" 30
mkfiles "$R/a/proj/sess/.git/objects" 40               # git internals are pruned

echo "1. the census names the busiest directories, grouped three levels down"
churn_census "$R/a:$R/b" 5 30 10 > "$FIX/c.out"; out=$(cat "$FIX/c.out")
check "the busy session is named with its count"      "50 $R/a/proj/sess/scratchpad" "$(printf '%s\n' "$out" | head -1)"
check "a second root is counted too"                   "yes" "$(has "$out" "30 $R/b/lane/tmp")"
check "a directory under the floor is not named"       "no"  "$(has "$out" "$R/a/proj/quiet")"
check "old files are not churn"                        "no"  "$(has "$out" "$R/a/proj/old")"
check "git internals are not counted"                  "no"  "$(has "$out" "/.git")"
check "a full scan says complete"                      "yes" "$CHURN_COMPLETE"
churn_census "$R/a" 5 0 10 >/dev/null
check "a spent budget says the scan is incomplete"     "no"  "$CHURN_COMPLETE"

echo "2. end to end: a hot fseventsd names its feed and escalates; a quiet one does neither (macOS only)"
if [ "$(uname)" = Darwin ]; then
  REC="$FIX/rec.sh"; printf '#!/bin/bash\ncp "$2" %s/sent.msg; echo "to=$1" >> %s/sent.log\n' "$FIX" "$FIX" > "$REC"; chmod +x "$REC"
  tick() { AMUX_CLEANUP_FSEVENTSD_CMD="$1" AMUX_CLEANUP_CHURN_ROOTS="$R/a" AMUX_CLEANUP_CHURN_FILES=10 \
           AMUX_CLEANUP_STATE_DIR="$FIX/tick" AMUX_CLEANUP_TARGET_ROOTS="$FIX/none" AMUX_CLEANUP_PURGE_CMD=true \
           AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=0 \
           AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 AMUX_CLEANUP_CPU_SHARE=99 AMUX_CLEANUP_SWAP_FREE_FLOOR_MB=0 \
           AMUX_CLEANUP_DISK_FLOOR_GB=0 AMUX_CLEANUP_HISTORY_CMD=true AMUX_CLEANUP_CARD_CMD=true \
           AMUX_CLEANUP_ESCALATE_TO=mac-ops-test AMUX_CLEANUP_RECLAIM_TRIGGER_CMD="${RECLAIM_TRIGGER_STUB:-echo 204}" AMUX_CLEANUP_DEMOTE_PS_CMD=true AMUX_CLEANUP_ESCALATE_CMD="${TICK_REC:-$REC} TARGET FILE" bash "$TICK" 2>&1; }
  out=$(tick "echo 0.01 3")
  check "control: a quiet fseventsd is under its thresholds" "yes" "$(has "$out" 'fseventsd 0.01G 3% CPU, under')"
  check "and escalates nothing"                         "no"  "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  out=$(tick "echo 30 120")
  check "a hot fseventsd names its top feed"            "yes" "$(has "$out" "fseventsd feed: 50 files modified in 5m under $R/a/proj/sess/scratchpad")"
  check "it becomes a constraint"                       "yes" "$(has "$out" 'constraints 1\|constraint fseventsd')"
  check "and is escalated"                              "yes" "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  check "the escalation names the feed"                 "yes" "$(grep -q "Top feed: 50 files in 5m under $R/a/proj/sess/scratchpad" "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"
  # The first reading is the baseline taken before any reap arm, the second the
  # one after. A burst only after this tick's own deletions is not escalated.
  SEQ="$FIX/seq.sh"; printf '#!/bin/bash\nn=$(cat %s/seq.n 2>/dev/null || echo 0); n=$((n+1)); echo $n > %s/seq.n\nsed -n "$( [ "$n" = 1 ] && echo 1 || echo 2 )p" %s/seq.vals\n' "$FIX" "$FIX" "$FIX" > "$SEQ"; chmod +x "$SEQ"
  rm -f "$FIX/sent.log" "$FIX/seq.n"; printf '0.01 4\n0.01 95\n' > "$FIX/seq.vals"
  out=$(tick "$SEQ")
  check "a burst after the tick's own deletions is named as such" "yes" "$(has "$out" 'verdict=fseventsd_burst_self_inflicted')"
  check "and is not escalated"                          "no"  "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  # Fresh state: the first cell's escalation started the cooldown, which would
  # suppress this one for the wrong reason.
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/seq.n"; printf '0.01 92\n0.01 95\n' > "$FIX/seq.vals"
  out=$(tick "$SEQ")
  check "it is not suppressed by an earlier cell's cooldown" "no" "$(has "$out" 'escalation suppressed')"
  check "hot before and after still escalates"          "yes" "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
else
  echo "  skip (the end-to-end tick needs macOS)"
fi

echo "3. with an FSEvents sampler the feed is named whatever the tree's size, deletions included"
SAMP="$FIX/samp.sh"
{ echo '#!/bin/bash'; echo 'echo "$1" > '"$FIX"'/samp.secs'
  echo 'for i in $(seq 1 60); do echo "/Users/x/Dev/cust/runs/eng-R-A/bodies/f$i.resp.txt"; done'
  echo 'for i in $(seq 1 3); do echo "/Users/x/Library/Caches/c$i/f"; done'
  echo 'echo "/Users/x/Library/Application Support/Chrome/Profile 17/a"; echo "/Users/x/Library/Application Support/Chrome/Profile 17/b"'; } > "$SAMP"; chmod +x "$SAMP"
AMUX_CLEANUP_CHURN_SAMPLE_CMD="$SAMP"
CHURN_SAMPLE_CMD="$SAMP" CHURN_COMPLETE=no
churn_sample 20 2 > "$FIX/s.out"; out=$(cat "$FIX/s.out")
check "the busiest directory is named with its event count" "60 /Users/x/Dev/cust/runs/eng-R-A/bodies" "$(head -1 "$FIX/s.out")"
check "a path with spaces keeps its directory"           "yes" "$(has "$out" "2 /Users/x/Library/Application Support/Chrome/Profile 17")"
check "a directory under the floor is not named"         "no"  "$(has "$out" "Library/Caches")"
check "off forces the census"                            "" "$(AMUX_CLEANUP_LIB_ONLY=1 AMUX_CLEANUP_CHURN_SAMPLE_CMD=off bash -c '. "$0" && printf %s "$CHURN_SAMPLE_CMD"' "$TICK")"
check "the sampler gets the window"                      "20"  "$(cat "$FIX/samp.secs")"
check "a sample says complete"                           "yes" "$CHURN_COMPLETE"

echo "3f. a sample that cannot see its own control files says fseventsd is not delivering (RCA fseventsd 20261028-133155)"
# pid 347 at 100% CPU holding 51G delivered 0 of 500 files written during a
# sample, so a wedged fseventsd read as a quiet disk.
SEES="$FIX/sees.sh"; { echo '#!/bin/bash'; echo 'sleep 3; ls "$CHURN_CONTROL_DIR" | sed "s|^|$CHURN_CONTROL_DIR/|"'
  echo 'for i in $(seq 1 60); do echo "/Users/x/Dev/cust/runs/eng-R-A/bodies/f$i.resp.txt"; done'; } > "$SEES"; chmod +x "$SEES"
BLIND="$FIX/blind.sh"; printf '#!/bin/bash\nsleep 3\n' > "$BLIND"; chmod +x "$BLIND"
mkdir -p "$FIX/state"
STATE_DIR="$FIX/state" CHURN_CONTROL=1 CHURN_SAMPLE_CMD="$SEES"
churn_sample 3 2 > "$FIX/s.out"
check "a sampler that sees the control: delivered"      "yes" "$CHURN_DELIVERED"
check "and the control's own events are not counted"   "60 /Users/x/Dev/cust/runs/eng-R-A/bodies" "$(cat "$FIX/s.out")"
check "and the control files are removed"              "no"  "$([ -e "$FIX/state/fsevents-control" ] && echo yes || echo no)"
CHURN_SAMPLE_CMD="$BLIND"
churn_sample 3 2 > "$FIX/s.out"
check "a sampler that sees nothing: not delivered"     "no"  "$CHURN_DELIVERED"
CHURN_CONTROL=auto CHURN_SAMPLE_CMD="$BLIND"
churn_sample 1 2 > "$FIX/s.out"
check "auto leaves a test sampler alone"               "unknown" "$CHURN_DELIVERED"
CHURN_CONTROL=auto STATE_DIR="$FIX/tick"

echo "3g. the fswatch sampler flushes its output when the window closes"
# fswatch block-buffers when piped; SIGALRM killed it with the buffer unwritten.
FAKEFW="$FIX/fakefw.py"
cat > "$FAKEFW" <<'PY'
#!/usr/bin/env python3
# Prints five paths into a block buffer, then waits; only a clean exit flushes it.
import time
for i in range(5):
    print("/a/b/f%d" % i)
time.sleep(60)
PY
chmod +x "$FAKEFW"
check "every line the sampler printed arrives"         "5"   "$(CHURN_FSWATCH="$FAKEFW" CHURN_SAMPLE_ROOTS=/a churn_fswatch 1 | grep -c '^/a/b/f')"
if [ "$(uname)" = Darwin ]; then
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/sent.log" "$FIX/sent.msg"
  out=$(AMUX_CLEANUP_CHURN_SAMPLE_CMD="$SAMP" tick "echo 30 120")
  check "the tick samples rather than walks"             "yes" "$(has "$out" 'verdict=fseventsd_churn_sampled')"
  check "and names the sampled feed"                     "yes" "$(has "$out" "fseventsd feed: 60 FSEvents in 20s under /Users/x/Dev/cust/runs/eng-R-A/bodies")"
  check "the escalation names it too"                    "yes" "$(grep -q "Top feed: 60 FSEvents in 20s under /Users/x/Dev/cust/runs/eng-R-A/bodies" "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"

  echo "3b. hot on CPU with no churn is a stuck thread, not a writer (RCA fseventsd 20261021-135901)"
  # Nothing reaches the floor (the 2026-10-21 sample: the busiest directory had
  # 134 events in 20s against the census floor).
  QUIET="$FIX/quiet.sh"; printf '#!/bin/bash\nexit 0\n' > "$QUIET"; chmod +x "$QUIET"
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/sent.log" "$FIX/sent.msg"
  out=$(AMUX_CLEANUP_CHURN_SAMPLE_CMD="$QUIET" tick "echo 0.81 98.8")
  check "it says spinning, with a verdict"               "yes" "$(has "$out" 'verdict=fseventsd_spin_no_churn')"
  check "the escalation names the stuck thread and the restart" "yes" "$(grep -q 'a stuck fseventsd thread, not a writer.*sudo killall fseventsd' "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"
  check "and does not ask anyone to stop churn"          "no"  "$(grep -q 'stop the file churn' "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/sent.log" "$FIX/sent.msg"
  out=$(AMUX_CLEANUP_CHURN_SAMPLE_CMD="$QUIET" tick "echo 30 98.8")
  # Over the memory threshold with no churn it is still the stuck thread: the
  # restart returns the memory too (RCA 20261021-195930). With churn it stays the
  # churn verdict (cell 3 above, 30G with a feed).
  check "over the memory threshold with no churn it is still a spin" "yes" "$(has "$out" 'verdict=fseventsd_spin_no_churn')"
  check "and the escalation names the memory and the restart" "yes" "$(grep -q 'holding 30G with no file churn.*sudo killall fseventsd' "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"
  check "and does not ask anyone to stop churn or reboot" "no" "$(grep -q 'stop the file churn' "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"

  echo "3e. a census the tick had no time for is reported as unmeasured (RCA fseventsd 20261026-130357)"
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/sent.log" "$FIX/sent.msg"
  out=$(AMUX_CLEANUP_TICK_BUDGET_S=1 AMUX_CLEANUP_TICK_RESERVE_S=0 AMUX_CLEANUP_CHURN_SAMPLE_CMD="$SAMP" tick "echo 0.02 133")
  check "the feed says it was not measured, with a verdict" "yes" "$(has "$out" 'verdict=fseventsd_feed_unmeasured')"
  check "and claims no quiet feed"                       "no"  "$(has "$out" 'fseventsd feed: no directory over')"
  check "the escalation does not claim a quiet feed"     "no"  "$(grep -q 'no directory over' "$FIX/sent.msg" 2>/dev/null && echo yes || echo no)"
  check "and does not call it a stuck thread"            "no"  "$(has "$out" 'verdict=fseventsd_spin_no_churn')"

  echo "3c. the same spinning pid is escalated once; a new pid is escalated again (RCA fseventsd 20261021-195930)"
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/sent.log" "$FIX/sent.msg"
  # Count fseventsd escalations only: a second tick also compares the live disk
  # with the first one's reading and can escalate a burn this cell is not about.
  fse_sent() { grep -c 'Mac fseventsd constraint' "$FIX/sent.log" 2>/dev/null || true; }
  REC3="$FIX/rec3.sh"; printf '#!/bin/bash\nhead -1 "$2" >> %s/sent.log\n' "$FIX" > "$REC3"; chmod +x "$REC3"
  spin() { TICK_REC="$REC3" AMUX_CLEANUP_FSEVENTSD_PID_CMD="echo $1" AMUX_CLEANUP_ESCALATE_COOLDOWN_H=0 AMUX_CLEANUP_CHURN_SAMPLE_CMD="$QUIET" tick "echo 0.81 99"; }
  out=$(spin 34409)
  check "the first spin escalates"                       "1" "$(fse_sent)"
  check "and does not call itself a reboot case"         "no" "$(has "$out" 'a reboot is the only remedy')"
  out=$(spin 34409)
  check "the same pid is the owner's restart pending"    "yes" "$(has "$out" 'verdict=fseventsd_spin_owner_pending')"
  check "and is not escalated again"                     "1" "$(fse_sent)"
  check "and is not a constraint"                        "no" "$(has "$out" 'constraint fseventsd')"
  out=$(spin 51234)
  check "a new pid spinning is escalated"                "2" "$(fse_sent)"
  out=$(AMUX_CLEANUP_FSEVENTSD_SPIN_REMIND_H=0 spin 51234)
  check "past the reminder window the same pid escalates" "3" "$(fse_sent)"

  echo "3c2. memory waits with an fseventsd already with the owner (memory RCAs 20261028-081710, 20261029-003047, 20261029-063250)"
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/sent.log"
  mem_sent() { grep -c 'Mac memory constraint' "$FIX/sent.log" 2>/dev/null || true; }
  hot() { TICK_REC="$REC3" AMUX_CLEANUP_FSEVENTSD_PID_CMD="echo $1" AMUX_CLEANUP_ESCALATE_COOLDOWN_H=0 AMUX_CLEANUP_PRESSURE_CMD="echo 2" AMUX_CLEANUP_CHURN_SAMPLE_CMD="$QUIET" tick "echo $2 99"; }
  out=$(hot 34409 30)
  check "the first tick escalates memory"                "1" "$(mem_sent)"
  out=$(hot 34409 30)
  check "a 30G fseventsd with its restart pending holds memory's escalation" "yes" "$(has "$out" 'verdict=memory_owner_pending_fseventsd')"
  check "and no second memory escalation goes out"       "1" "$(mem_sent)"
  out=$(hot 34409 0.81)
  check "a small fseventsd is not the holder: memory escalates" "2" "$(mem_sent)"

  echo "3d. the escalated pid with ordinary churn stays the owner's; heavy churn is a new cause (RCA fseventsd 20261022-075928)"
  # Cargo builds wrote 228 events in 20s on top of the stuck pid 34409 (14G),
  # and the tick re-escalated it as "stop the churn".
  rm -rf -- "${FIX:?}/tick"; rm -f "$FIX/sent.log"
  churn() { TICK_REC="$REC3" AMUX_CLEANUP_FSEVENTSD_PID_CMD="echo $1" AMUX_CLEANUP_ESCALATE_COOLDOWN_H=0 AMUX_CLEANUP_CHURN_SAMPLE_CMD="$SAMP" tick "echo 14 102"; }
  out=$(AMUX_CLEANUP_FSEVENTSD_CHURN_NEW_X=100 churn 34409)
  check "first sight escalates"                          "1" "$(fse_sent)"
  check "its remedy returns memory without a reboot"     "yes" "$(grep -q 'sudo killall fseventsd returns its memory' "$FIX/sent.log" "$FIX/sent.msg" 2>/dev/null; has "$out" 'sudo killall fseventsd to return its memory')"
  out=$(AMUX_CLEANUP_FSEVENTSD_CHURN_NEW_X=100 churn 34409)
  check "same pid, churn under the bar: owner pending"   "yes" "$(has "$out" 'with churn under 100x the floor.*verdict=fseventsd_spin_owner_pending')"
  check "and not escalated again"                        "1" "$(fse_sent)"
  out=$(churn 34409)
  check "same pid, churn at 5x the floor: a new cause, escalated" "2" "$(fse_sent)"
else
  echo "  skip (the end-to-end tick needs macOS)"
fi

echo "6. a bloated fseventsd is restarted by the tick itself, once per cooldown, and a sudo refusal names the line that fixes it"
RS="$FIX/restart"; mkdir -p "$RS"
restart_try() { # <gb> <cmd> [dry]
  ( STATE_DIR="$RS"; FSEVENTSD_RESTART_GB=12; FSEVENTSD_RESTART_COOLDOWN_H=6; FSEVENTSD_RESTART_CMD="$2"
    FSEVENTSD_CMD="echo 2.0 5"; AMUX_CLEANUP_FSEVENTSD_RESTART_SETTLE_S=0
    if fseventsd_try_restart "$1" "${3:-0}"; then echo "RC=0"; else echo "RC=1"; fi; echo "SUDOERS=$FSE_RESTART_NEEDS_SUDOERS" )
}
out=$(restart_try 8.0 "touch $RS/ran-under")
check "under the threshold: nothing runs"               "no"  "$([ -e "$RS/ran-under" ] && echo yes || echo no)"
check "and it is not a restart"                         "yes" "$(has "$out" 'RC=1')"
out=$(restart_try 30.0 "touch $RS/ran-dry" 1)
check "a dry run only says it would"                    "yes" "$(has "$out" 'verdict=fseventsd_restart_dry')"
check "and runs nothing"                                "no"  "$([ -e "$RS/ran-dry" ] && echo yes || echo no)"
out=$(restart_try 30.0 "touch $RS/ran")
check "over the threshold: the restart runs"            "yes" "$([ -e "$RS/ran" ] && echo yes || echo no)"
check "it reports before and after"                     "yes" "$(has "$out" 'held 30.0G.*back at 2.0G (verdict=fseventsd_restarted')"
check "and returns success so it is not escalated"      "yes" "$(has "$out" 'RC=0')"
rm -f "$RS/ran"; out=$(restart_try 30.0 "touch $RS/ran")
check "inside the cooldown it does not run again"       "no"  "$([ -e "$RS/ran" ] && echo yes || echo no)"
check "and says when it may"                            "yes" "$(has "$out" 'verdict=fseventsd_restart_cooldown')"
rm -f "$RS/state"; printf '#!/bin/sh\necho "sudo: a password is required" >&2\nexit 1\n' > "$RS/nosudo"; chmod +x "$RS/nosudo"
out=$(restart_try 30.0 "$RS/nosudo")
check "a sudo refusal is not a restart"                 "yes" "$(has "$out" 'RC=1')"
check "it prints the sudoers line"                      "yes" "$(has "$out" 'NOPASSWD: /usr/bin/killall fseventsd (verdict=fseventsd_restart_needs_sudoers)')"
check "and flags it for the escalation"                 "yes" "$(has "$out" 'SUDOERS=1')"
out=$( STATE_DIR="$RS"; FSEVENTSD_RESTART_GB=12; FSEVENTSD_RESTART_COOLDOWN_H=6; FSEVENTSD_RESTART_CMD="sudo -n /usr/bin/killall fseventsd"; HEALTH_PROBES=off
  rm -f "$RS/state"; fseventsd_try_restart 30.0 0; echo "RC=$?" )
check "with live probes off the real restart never runs" "yes" "$(has "$out" 'verdict=fseventsd_restart_probes_off')"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-fseventsd — all checks passed"; exit 0; fi
echo "mac-cleanup-fseventsd: $fails check(s) FAILED"; exit 1
