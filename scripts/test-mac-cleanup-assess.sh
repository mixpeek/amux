#!/bin/bash
# The assess stage of scripts/mac-cleanup-tick.sh (DESKT-57): after the symptom
# fixes, name what is still constrained, write an RCA bundle, and escalate each
# class to a model turn at most once per cooldown. Each guard has a cell that
# fails without it, and every "not escalated" cell has a control that does
# escalate, or a stage that never sends anything would pass them all.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared by every lane
export AMUX_CLEANUP_FSEVENTSD_CMD=true   # the live fseventsd must not decide a test (DESKT-81)
export AMUX_CLEANUP_SQLITE_PS_CMD=true   # the live process table must not decide a test (DESKT-94)
export AMUX_CLEANUP_HEALTH_PROBES=off    # the health probes must not stop or read live processes in a test
export AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/no-claude-tmp"   # never the live session temp tree (DESKT-77)
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable, no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { if printf '%s' "$1" | grep -q -- "$2"; then echo yes; else echo no; fi; }
export AMUX_CLEANUP_SCOPE_FILE=/dev/null   # the live global scope must not configure a test (DESKT-72)
export AMUX_CLEANUP_SESSIONS_CMD=false        # the VM-reference report must not read the live fleet (AMUX-5491)
export AMUX_CLEANUP_VM_WRITERS_CMD=false      # the bundle's container writers must not read the live VMs
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
# No end-to-end cell here may prune a real VM (DESKT-69) or reap real build output.
export AMUX_CLEANUP_VM_LIST_CMD="true"
# The tick reads live memory pressure; a loaded Mac must not decide these cells.
export AMUX_CLEANUP_PRESSURE_CMD="echo 1"

echo "1. disk trend"
check "no previous reading is unmeasured, not zero"   "- -"      "$(disk_trend 0 0 1000 500 1)"
check "a disk gaining space has no ETA"              "-10.0 -"  "$(disk_trend 1000 490 4600 500 1)"
check "burning 10G/h from 100G is full in 10h"       "10.0 10.0" "$(disk_trend 1000 110 4600 100 1)"
check "under the minimum burn there is no ETA"       "0.5 -"    "$(disk_trend 1000 100.5 4600 100 1)"

echo "1a. a burn that is swap growth is named as swap (disk RCA 20261022-105614)"
# Swap grew 7G to 59G under memory pressure; no file search names it.
check "swap growth explaining the burn is named"     "yes" "$(has "$(swap_burn_note 28.9 30000 59392 1)" 'the writer is macOS swap, grown 29.3G to 58.0G since the last run (28.7G/h)')"
check "swap growth under half the burn is not blamed" ""   "$(swap_burn_note 28.9 50000 59392 1)"
check "no previous reading names nothing"             ""   "$(swap_burn_note 28.9 0 59392 1)"
check "no burn names nothing"                         ""   "$(swap_burn_note - 30000 59392 1)"
check "swap shrinking names nothing"                  ""   "$(swap_burn_note 10 59392 30000 1)"

echo "1b. burn rate from the host history, by least squares"
mkhist() { # <n> <gb per 5 min> [unmeasured rows]
  python3 -c '
import json,sys
n,step=int(sys.argv[1]),float(sys.argv[2]); bad=int(sys.argv[3]) if len(sys.argv)>3 else 0
rows=[{"ts":10000+i*300,"disk_free_gb":500-i*step,"measured":True} for i in range(n)]
rows+= [{"ts":10000+i*300+1,"disk_free_gb":0,"measured":False} for i in range(bad)]
print(json.dumps({"measured":True,"samples":list(reversed(rows))}))' "$@"; }
check "13 samples (1h) losing 1G per 5 min burn 12G/h"     "12.0" "$(mkhist 13 1 | burn_from_history 1 | awk '{print $1}')"
check "full-in uses the NEWEST reading (488G / 12 = 40.7h)" "40.7" "$(mkhist 13 1 | burn_from_history 1 | awk '{print $2}')"
check "oldest-first order gives the same answer"          "12.0" "$(mkhist 13 1 | python3 -c 'import json,sys; d=json.load(sys.stdin); d["samples"].reverse(); print(json.dumps(d))' | burn_from_history 1 | awk '{print $1}')"
check "a dip that has recovered is not a burn (2026-10-05 05:00Z, 12.1 G/h read off it)" "yes" "$(python3 -c '
import json
f=[268]*16+[223]*5+[269.7]*3   # the shape the old slope read as 14.7 G/h, full in 18 h
rows=[{"ts":10000+i*300,"disk_free_gb":v,"measured":True} for i,v in enumerate(f)]
print(json.dumps({"samples":rows}))' | burn_from_history 1 | awk '{print ($2=="-") ? "yes" : "no"}')"
check "fewer than 6 samples is unmeasured"                 "- -"  "$(mkhist 5 1 | burn_from_history 1 | awk '{print $1, $2}')"
check "12 samples (55 min) is under an hour: unmeasured"   "- -"  "$(mkhist 12 1 | burn_from_history 1 | awk '{print $1, $2}')"
check "unmeasured rows (free 0) do not drag the slope"     "12.0" "$(mkhist 13 1 5 | burn_from_history 1 | awk '{print $1}')"
check "a filling-up disk has no ETA"                       "-"    "$(mkhist 13 -1 | burn_from_history 1 | awk '{print $2}')"
# Disk RCA 20261018-135254: history falls 233G -> 206G over 2h, then this run's
# own prune brings it back to 228.4G. The pre-prune fit alone says full in ~15h.
sawtooth() { python3 -c '
import json
rows=[{"ts":10000+i*300,"disk_free_gb":233-i*27/24,"measured":True} for i in range(25)]
print(json.dumps({"measured":True,"samples":rows}))'; }
check "control: history alone (pre-prune) reads a 24h-scale ETA" "yes" "$(sawtooth | burn_from_history 1 | awk '{print ($2!="-" && $2+0<24) ? "yes" : "no"}')"
check "this run's post-cleanup reading is the newest point: no ETA" "no" "$(sawtooth | burn_from_history 1 17620 228.4 | awk '{print ($2!="-" && $2+0<24) ? "yes" : "no"}')"
check "the post-cleanup reading is used and marked"      "25 run" "$(sawtooth | burn_from_history 1 17620 228.4 | awk '{print $3, $5}')"
check "an unmeasured run reading (-1) is not appended"    "-"    "$(sawtooth | burn_from_history 1 17620 -1 | awk '{print $5}')"
check "a run reading older than the history is not appended" "-" "$(sawtooth | burn_from_history 1 9000 228.4 | awk '{print $5}')"
check "a run reading hours after the history is not appended" "-" "$(sawtooth | burn_from_history 1 99999 228.4 | awk '{print $5}')"
check "garbage input is unmeasured, not a crash"           "- -"  "$(echo 'not json' | burn_from_history 1 | awk '{print $1, $2}')"

echo "2. classification"
DISK_FLOOR_GB=150; HOURS_TO_FULL=24; SWAP_FREE_FLOOR_MB=512; CPU_SHARE=0.9; CPU_IDLE_MAX_PCT=25; SWAP_FREE_CRITICAL_PCT=0.10
check "a healthy machine is not constrained"          ""   "$(classify_constraints 600 - - 1 2000 10 28 0 20000)"
check "high load on a busy CPU is a cpu constraint"    "cpu" "$(classify_constraints 600 - - 1 2000 40 28 0 20000 5 | awk '{print $1}')"
check "high load with the CPU 65% idle is not cpu"     ""    "$(classify_constraints 600 - - 1 2000 40 28 0 20000 65)"
check "high load with idle unmeasured keeps the rule"  "cpu" "$(classify_constraints 600 - - 1 2000 40 28 0 20000 -1 | awk '{print $1}')"
check "under the disk floor"                          "disk" "$(classify_constraints 100 - - 1 2000 10 28 0 20000 | awk '{print $1}')"
check "above the floor but full within the window"   "yes" "$(has "$(classify_constraints 300 20 15 1 2000 10 28 0 20000)" 'disk burning 20.0G/h, full in 15.0h')"
check "full outside the window is fine"              ""    "$(classify_constraints 300 5 60 1 2000 10 28 0 20000)"
check "kernel pressure after purge"                   "memory" "$(classify_constraints 600 - - 2 2000 10 28 0 20000 | awk '{print $1}')"
check "swap nearly exhausted, little disk to grow it: trips" "memory" "$(classify_constraints 10 - - 1 100 10 28 0 20000 | grep -o '^memory')"
check "swap nearly used but normal pressure and disk to grow it: not a constraint (AMUX memory RCA 2026-10-13: 385MB of 12288MB, 61G disk)" "" "$(classify_constraints 61.4 - - 1 385 10 28 0 12288 | grep -o '^memory')"
check "same swap at pressure 2 trips whatever the disk" "memory" "$(classify_constraints 600 - - 2 385 10 28 0 12288 | grep -o '^memory')"
check "load over the CPU share"                       "cpu"  "$(classify_constraints 600 - - 1 2000 30 28 0 20000 | awk '{print $1}')"
check "a runaway family"                              "family" "$(classify_constraints 600 - - 1 2000 10 28 1 20000 | awk '{print $1}')"
check "unmeasured inputs (-1) never trip a class"     ""   "$(classify_constraints -1 - - -1 -1 -1 28 0 -1)"

echo "2b. swap_total=0 means no swapfile was ever created, not exhaustion (MO-3629/MO-3638, 3 escalations in one day: swap_free=0.00MB, pressure=1/normal, >=90% memory free every time)"
check "fresh boot, swap free=0 AND total=0: not a constraint"  ""       "$(classify_constraints 600 - - 1 0 10 28 0 0)"
check "same free=0, a real swapfile (total>0) and no disk to grow it: still trips" "memory" "$(classify_constraints 10 - - 1 0 10 28 0 20000 | grep -o '^memory')"
check "swap_total unmeasured (-1) does not trip either"          ""       "$(classify_constraints 600 - - 1 0 10 28 0 -1)"
check "kernel pressure still trips independently of swap_total" "memory" "$(classify_constraints 600 - - 2 0 10 28 0 0 | awk '{print $1}')"

echo "2c. a low swap_free can be STALE, not current, once pressure is confirmed normal (MO-3655/3657: identical 438.06MB free across hours while pressure stayed 1 and load/free% visibly improved)"
check "pr==1 and 43% of swap still free (today's real numbers): not a constraint" "" "$(classify_constraints 600 - - 1 438 10 28 0 1024)"
check "pr unmeasured (-1) with the same ratio: still trips (cannot trust pressure)" "memory" "$(classify_constraints 600 - - -1 438 10 28 0 1024 | awk '{print $1}')"
check "pr==1 under the critical percent (<10% of swap left) with little disk: still trips" "memory" "$(classify_constraints 10 - - 1 50 10 28 0 1024 | grep -o '^memory')"
check "pr==1 under the critical percent but disk unmeasured (-1): still trips" "memory" "$(classify_constraints -1 - - 1 50 10 28 0 1024 | grep -o '^memory')"
check "pr==1 right at the critical percent boundary (exactly 10%): does not trip" "" "$(classify_constraints 600 - - 1 102 10 28 0 1020)"
check "pr==2 still trips via the pressure branch regardless of the percent gate" "memory" "$(classify_constraints 600 - - 2 438 10 28 0 1024 | awk '{print $1}')"

echo "3. state and cooldown"
S="$FIX/st/state"
state_put "$S" a=1 b=2; state_put "$S" b=3
check "state_put keeps other keys"                    "1"  "$(state_get "$S" a)"
check "state_put replaces a key"                      "3"  "$(state_get "$S" b)"
check "a class never escalated is due"                "yes" "$(escalation_due "$S" disk 10000 6 && echo yes || echo no)"
state_put "$S" esc_disk=10000
check "inside the cooldown it is not due"             "no"  "$(escalation_due "$S" disk 20000 6 && echo yes || echo no)"
check "control: past the cooldown it is due again"    "yes" "$(escalation_due "$S" disk 31600 6 && echo yes || echo no)"
check "the cooldown is per class"                     "yes" "$(escalation_due "$S" cpu 20000 6 && echo yes || echo no)"

echo "3b. the hand-off prompt (DESKT-58)"
first=$(escalation_message cpu "cpu load 40" /b.md /c.card "disk=600G load15=40/28" - - -)
check "the first line is the ask, and says symptom fixes are not done" "yes" "$(printf '%s\n' "$first" | head -1 | grep -q '^Ask: find and fix the ROOT CAUSE.*a symptom fix alone is not done' && echo yes || echo no)"
check "a first escalation says so"                    "yes" "$(has "$first" 'History: first cpu escalation on record')"
check "it carries the numbers measured now"           "yes" "$(has "$first" 'Measured now: disk=600G load15=40/28')"
check "it states four done criteria"                  "4"   "$(printf '%s\n' "$first" | grep -c '^[1-4]\. ')"
check "it names where to write the card id"           "yes" "$(has "$first" 'its id written to /c.card')"
check "it carries the boundary"                       "yes" "$(has "$first" "never kill a live lane's workload")"
check "it forbids live session scratchpads"           "yes" "$(has "$first" 'never touch anything under /private/tmp/claude-\*')"
again=$(escalation_message cpu "cpu load 40" /b.md /c.card "disk=600G load15=40/28" 6.5 MO-3622 "load15=46.6/28")
check "a recurrence says RECURRED with its age"       "yes" "$(has "$again" 'this RECURRED. The previous cpu escalation was 6.5h ago')"
check "and quotes the previous card and its numbers"  "yes" "$(has "$again" 'card: MO-3622; measured then: load15=46.6/28')"
check "control: the first-time line is absent then"   "no"  "$(has "$again" 'first cpu escalation')"

echo "4. lane attribution walks the parent chain to a pane"
bash -c 'sleep 30 & wait' & shell=$!
sleep 0.5; child=$(pgrep -P "$shell" sleep | head -1)
check "a grandchild of a pane is attributed to its lane" "lane-under-test" "$(lane_for_pid "$child" "$shell amux-lane-under-test")"
check "control: with no pane in its chain it has no lane" "no lane" "$(lane_for_pid "$child" "999999 amux-other")"
kill "$shell" "$child" 2>/dev/null || true; wait "$shell" 2>/dev/null || true
check "pid 1 has no lane"                             "no lane" "$(lane_for_pid 1 "1 amux-x")"
bash -c 'sleep 30 & wait' & agent=$!
sleep 0.5; achild=$(pgrep -P "$agent" sleep | head -1)
check "a child of a launchd agent is named by its label" "launchd:com.example.agent" "$(lane_for_pid "$achild" "$agent launchd:com.example.agent")"
kill "$agent" "$achild" 2>/dev/null || true; wait "$agent" 2>/dev/null || true
# An orphan (parent exited, reparented to launchd) is named by where it runs.
# Each orphan's pid comes from $! written to a file, never from a pattern
# match: a pattern kill here once SIGTERMed every process with "-n" in its argv.
# Not called inside $(...): an orphan started there dies before it is measured.
orphan() { # <cwd> <pidfile> <argv...>
  local d=$1 f=$2; shift 2
  ( ( cd "$d" && exec nohup "$@" >/dev/null 2>&1 ) & echo $! > "$f" )
}
mkdir -p "$FIX/lanetmp/gs-under-test/w" "$FIX/repo/.worktrees/wt-lane/x"
LANE_TMP_ROOT="$FIX/lanetmp"
orphan "$FIX/lanetmp/gs-under-test/w" "$FIX/o1.pid" sleep 30; orph=$(cat "$FIX/o1.pid"); sleep 0.3
check "an orphan whose cwd is a lane's tmp is named by path" "gs-under-test (orphan, by path)" "$(lane_for_pid "$orph" "999999 amux-other")"
orphan / "$FIX/o2.pid" bash -c 'sleep 30; :' "$FIX/repo/.worktrees/wt-lane/x"; warg=$(cat "$FIX/o2.pid"); sleep 0.3
check "an orphan whose argv names a lane worktree is named by path" "wt-lane (orphan, by path)" "$(lane_for_pid "$warg" "999999 amux-other")"
orphan "$FIX" "$FIX/o3.pid" sleep 30; plain=$(cat "$FIX/o3.pid"); sleep 0.3
check "control: an orphan outside any lane path has no lane" "no lane" "$(lane_for_pid "$plain" "999999 amux-other")"
for p in "$orph" "$warg" "$plain"; do [[ "$p" =~ ^[0-9]+$ ]] && kill "$p" 2>/dev/null; done; true

echo "5. end to end: escalation, cooldown, failure, dry run (macOS only)"
if [ "$(uname)" = Darwin ]; then
  REC="$FIX/rec.sh"; printf '#!/bin/bash\necho "to=$1 file=$2" >> %s/sent.log\n' "$FIX" > "$REC"; chmod +x "$REC"
  FAILCMD="$FIX/fail.sh"; printf '#!/bin/bash\necho "target is an isolated worker" >&2; exit 1\n' > "$FAILCMD"; chmod +x "$FAILCMD"
  CARDREC="$FIX/card.sh"; printf '#!/bin/bash\nf="${@: -1}"; cat "${f#@}" >> %s/cards.log; echo >> %s/cards.log; printf 201\n' "$FIX" "$FIX" > "$CARDREC"; chmod +x "$CARDREC"
  tick() { AMUX_CLEANUP_STATE_DIR="$FIX/tick" AMUX_CLEANUP_TARGET_ROOTS="$FIX/none" AMUX_CLEANUP_PURGE_CMD=true \
           AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=0 \
           AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 AMUX_CLEANUP_CPU_SHARE=99 AMUX_CLEANUP_SWAP_FREE_FLOOR_MB=0 \
           AMUX_CLEANUP_DISK_FLOOR_GB="$1" AMUX_CLEANUP_HISTORY_CMD="cat $FIX/hist.json" AMUX_CLEANUP_CARD_CMD="$CARDREC --data @FILE" AMUX_CLEANUP_ESCALATE_TO=mac-ops-test AMUX_CLEANUP_RECLAIM_TRIGGER_CMD="${RECLAIM_TRIGGER_STUB:-echo 204}" AMUX_CLEANUP_DEMOTE_PS_CMD=true AMUX_CLEANUP_ESCALATE_CMD="$2 TARGET FILE" \
           bash "$TICK" ${3:-} 2>&1; }
  mkhist 13 0 > "$FIX/hist.json"      # a flat disk: the trend must not fire, only the floor knob
  out=$(tick 0 "$REC"); rm -f "$FIX/sent.log"
  check "the done line carries the elapsed time"       "yes" "$(has "$out" 'done elapsed=[0-9]*s')"
  check "a normal run prints no timing WARN"           "no"  "$(has "$out" 'WARN tick took')"
  out=$(AMUX_CLEANUP_TICK_WARN_S=0 tick 0 "$REC")
  check "control: past the warn line it says so"       "yes" "$(has "$out" 'WARN tick took [0-9]*s, over 0s')"
  sleep 60 & holder=$!
  mkdir -p "$FIX/tick/tick.lock"; echo "$holder" > "$FIX/tick/tick.lock/pid"
  out=$(tick 999999 "$REC")
  check "a held lock stops a second tick"              "yes" "$(has "$out" 'previous tick still running')"
  check "and that tick sends nothing"                  "no"  "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  check "and leaves the other run's lock alone"        "yes" "$([ -d "$FIX/tick/tick.lock" ] && echo yes || echo no)"
  kill "$holder" 2>/dev/null || true; wait "$holder" 2>/dev/null || true
  out=$(tick 0 "$REC")
  check "a lock whose PID is dead is taken over AT ONCE (a SIGKILLed tick)" "yes" "$(has "$out" 'constraints none')"
  mkdir -p "$FIX/tick/tick.lock"
  out=$(tick 0 "$REC")
  check "a lock with no PID and under 30m still blocks" "yes" "$(has "$out" 'previous tick still running')"
  touch -t 202001010000 "$FIX/tick/tick.lock"
  out=$(tick 0 "$REC")
  check "control: a stale lock is taken over"          "yes" "$(has "$out" 'constraints none')"
  check "and released at exit"                         "no"  "$([ -d "$FIX/tick/tick.lock" ] && echo yes || echo no)"
  check "the trend line names its source"              "yes" "$(has "$out" 'from history 13 samples over 1.0h')"
  check "with nothing constrained it says none"        "yes" "$(has "$out" 'constraints none')"
  check "and sends nothing"                            "no"  "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  # A disk constraint starts the scheduled reclaim (SCHED-783) instead of only reporting it.
  TRIG="$FIX/trig.sh"; printf '#!/bin/bash\necho x >> %s/trig.log\necho "${TRIG_CODE:-204}"\n' "$FIX" > "$TRIG"; chmod +x "$TRIG"
  ntrig() { [ -f "$FIX/trig.log" ] && grep -c . "$FIX/trig.log" | tr -d ' ' || echo 0; }
  out=$(RECLAIM_TRIGGER_STUB="$TRIG" tick 999999 "$REC" --dry-run)
  check "a dry run says it would start the reclaim"   "yes" "$(has "$out" 'would start the disk reclaim (dry run)')"
  check "and starts nothing"                          "0"   "$(ntrig)"
  out=$(TRIG_CODE=500 RECLAIM_TRIGGER_STUB="$TRIG" tick 999999 "$REC")
  check "a refused start is reported with its code"   "yes" "$(has "$out" 'could not start the disk reclaim (http 500)')"
  check "and does not start the reclaim cooldown"     ""    "$(state_get "$FIX/tick/state" esc_reclaim)"
  out=$(RECLAIM_TRIGGER_STUB="$TRIG" tick 999999 "$REC")
  check "a disk constraint starts the reclaim"        "yes" "$(has "$out" 'started the disk reclaim (SCHED-783) (verdict=disk_reclaim_triggered)')"
  check "the done line counts it"                     "yes" "$(has "$out" 'reclaim_started=1')"
  out=$(RECLAIM_TRIGGER_STUB="$TRIG" tick 999999 "$REC")
  check "inside the cooldown it starts no second one" "yes" "$(has "$out" 'verdict=disk_reclaim_cooldown')"
  check "two starts were attempted in all (fail, ok)" "2"   "$(ntrig)"
  out=$(RECLAIM_TRIGGER_STUB="$TRIG" tick 0 "$REC")
  check "no disk constraint, no start"                "2"   "$(ntrig)"
  rm -f "$FIX/sent.log" "$FIX/tick/state"   # the escalations above must not shade the cells below
  out=$(tick 999999 "$FAILCMD")
  check "a refused send is reported with its reason"   "yes" "$(has "$out" 'FAILED (1 in a row): target is an isolated worker')"
  check "and does not start the cooldown"              ""    "$(state_get "$FIX/tick/state" esc_disk)"
  check "one failure files no card yet"                 "no"  "$([ -f "$FIX/cards.log" ] && echo yes || echo no)"
  out=$(tick 999999 "$FAILCMD")
  check "the second failure in a row files a card"      "yes" "$(has "$out" 'send-failure card: filed (201)')"
  check "the card names the target and the error"       "yes" "$(grep -q 'cannot reach mac-ops-test' "$FIX/cards.log" && grep -q 'isolated worker' "$FIX/cards.log" && echo yes || echo no)"
  out=$(tick 999999 "$FAILCMD")
  check "a third failure inside 24h files no second card" "yes" "$(has "$out" 'already filed within 24h')"
  check "exactly one card was filed"                    "1"   "$(grep -c 'cannot reach mac-ops-test' "$FIX/cards.log" | tr -d ' ')"
  out=$(tick 999999 "$REC" --dry-run)
  check "dry run says it would escalate"               "yes" "$(has "$out" 'would escalate (dry run)')"
  check "and sends nothing"                            "no"  "$([ -f "$FIX/sent.log" ] && echo yes || echo no)"
  out=$(tick 999999 "$REC")
  check "control: a constrained disk escalates"        "yes" "$(has "$out" 'escalated to mac-ops-test')"
  check "to the configured target"                     "1"   "$(grep -c 'to=mac-ops-test file=.*\.disk\.msg' "$FIX/sent.log" | tr -d ' ')"
  msg=$(sed -n 's/.*file=//p' "$FIX/sent.log" | grep '\.disk\.msg$' | head -1)
  check "the message's first line is the ask"          "yes" "$(head -1 "$msg" | grep -q '^Ask: find and fix the ROOT CAUSE' && echo yes || echo no)"
  bundle=$(sed -n 's/^Evidence: \([^ ]*\).*/\1/p' "$msg")
  check "the bundle it names exists"                   "yes" "$([ -f "$bundle" ] && echo yes || echo no)"
  check "the bundle has the disk-writer section"       "yes" "$(grep -q 'written in the last hour' "$bundle" && echo yes || echo no)"
  check "the bundle attributes processes to lanes"     "yes" "$(grep -q 'lane: ' "$bundle" && echo yes || echo no)"
  check "the done line counts it"                      "yes" "$(has "$out" 'escalated=1')"
  out=$(tick 999999 "$REC")
  check "a second tick inside the cooldown is suppressed" "yes" "$(has "$out" 'escalation suppressed')"
  # Count DISK sends only: the tick reads the machine's real memory pressure and
  # load, so on a loaded Mac a memory or cpu escalation can legitimately join in.
  check "and sends nothing more"                       "1"   "$(grep -c 'to=mac-ops-test file=.*\.disk\.msg' "$FIX/sent.log" | tr -d ' ')"
  # Recurrence: the model turn wrote its card id; the cooldown lapses; the next
  # escalation must quote that card. The cooldown is shortened, not faked.
  echo MO-9999 > "${msg%.msg}.card"
  state_put "$FIX/tick/state" "esc_disk=$(( $(date +%s) - 7*3600 ))"
  out=$(tick 999999 "$REC")
  check "after the cooldown a recurrence escalates"     "yes" "$(has "$out" 'recurrence, previous 7.0h ago, card MO-9999')"
  msg2=$(sed -n 's/.*file=//p' "$FIX/sent.log" | grep '\.disk\.msg$' | tail -1)
  check "and the prompt quotes the previous card"       "yes" "$(grep -q 'card: MO-9999; measured then: disk=' "$msg2" && echo yes || echo no)"
else
  echo "  skip macOS-only cells (the tick's probes are macOS commands): not run on $(uname)"
fi

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-assess — all checks passed"; exit 0; fi
echo "mac-cleanup-assess: $fails check(s) FAILED"; exit 1
