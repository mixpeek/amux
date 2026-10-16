#!/usr/bin/env bash
# test-mac-cleanup-lane-tmp.sh: the cleanup tick's reap_lane_tmp removes
# ~/.amux/tmp/<worker>/<entry> only when nothing inside changed past the floor
# and no process works inside it (2026-10-04: 101 GB idle in lane TMPDIRs),
# and never an entry a live schedule names or a small file (2026-10-06).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/lt.XXXXXX")"; R="$T/root"
mkdir -p "$R/lane1/oldidle/sub" "$R/lane1/fresh" "$R/lane1/oldtopfreshinside/deep" "$R/lane2/inuse" "$R/lane4/scheddir"
# gs12-data, 2026-10-06: a schedule's script and a small kubeconfig, both old.
echo 'echo hi' > "$R/lane4/sched.sh"; echo 'apiVersion: v1' > "$R/lane4/kc"
head -c 2000000 /dev/zero > "$R/lane4/big.bin"
echo x > "$R/lane4/scheddir/run.sh"
touch -t 202609300000 "$R/lane4/sched.sh" "$R/lane4/kc" "$R/lane4/big.bin" "$R/lane4/scheddir/run.sh" "$R/lane4/scheddir"
DB="$T/amux.db"
sqlite3 "$DB" "CREATE TABLE schedules (id TEXT, command TEXT, enabled INTEGER, deleted INTEGER);
  INSERT INTO schedules VALUES ('S1', 'bash ~/.amux/tmp/lane4/sched.sh >> ~/.amux/tmp/lane4/out.jsonl 2>&1', 1, NULL);
  INSERT INTO schedules VALUES ('S2', 'cd \$HOME/.amux/tmp/lane4/scheddir; ./run.sh', 1, NULL);
  INSERT INTO schedules VALUES ('S3', 'bash ~/.amux/tmp/lane1/oldidle/x.sh', 0, NULL);"
export LANE_TMP_SCHED_DB="$DB"
echo x > "$R/lane1/oldidle/sub/f"; echo y > "$R/lane1/oldtopfreshinside/deep/new"
touch -t 202609300000 "$R/lane1/oldidle/sub/f" "$R/lane1/oldidle/sub" "$R/lane1/oldidle" "$R/lane1/oldtopfreshinside" "$R/lane2/inuse"
(cd "$R/lane2/inuse" && exec sleep 30) & holder=$!
sleep 1
F="$(mktemp "${TMPDIR:-/tmp}/ltf.XXXXXX")"
sed -n '/^reap_lane_tmp() {/,/^}/p' "$ROOT/scripts/mac-cleanup-tick.sh" > "$F"
fail=0
[ -s "$F" ] || { echo "FAIL reap_lane_tmp is not in the tick"; exit 1; }
out="$(LANE_TMP_ROOT="$R" LANE_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_lane_tmp 0")"
[ ! -e "$R/lane1/oldidle" ] && echo "ok   an entry with nothing changed in a day is removed" || { echo "FAIL old idle entry kept"; fail=1; }
[ -d "$R/lane1/fresh" ] && echo "ok   a fresh entry is kept" || { echo "FAIL fresh entry removed"; fail=1; }
[ -d "$R/lane1/oldtopfreshinside" ] && echo "ok   an old directory with a fresh file deep inside is kept" || { echo "FAIL in-use cargo-style dir removed"; fail=1; }
[ -d "$R/lane2/inuse" ] && echo "ok   an old entry a process works in is kept" || { echo "FAIL cwd entry removed"; fail=1; }
printf '%s' "$out" | grep -q 'lane tmp: removed 2 entries' && echo "ok   the tick logs the count" || { echo "FAIL log: $out"; fail=1; }
[ -f "$R/lane4/sched.sh" ] && [ -d "$R/lane4/scheddir" ] && echo "ok   entries an enabled schedule names are kept (~ and \$HOME forms)" || { echo "FAIL a schedule's file was reaped"; fail=1; }
[ -f "$R/lane4/kc" ] && echo "ok   a small old file is kept" || { echo "FAIL small file reaped"; fail=1; }
[ ! -e "$R/lane4/big.bin" ] && echo "ok   a large old file is still reaped" || { echo "FAIL large idle file kept"; fail=1; }
printf '%s' "$out" | grep -q 'kept 2 named by an enabled schedule, 1 small file' && echo "ok   the keeps are counted in the log" || { echo "FAIL keep counts: $out"; fail=1; }
mkdir -p "$R/lane5/old"; touch -t 202609300000 "$R/lane5/old"
# DESKT-95: lsof that cannot run must remove NOTHING. An empty in-use list used
# to read as "nothing is in use" and lane2/inuse was deleted.
outn="$(LANE_TMP_ROOT="$R" LANE_TMP_IDLE_MIN=1440 AMUX_CLEANUP_LSOF_BIN=/nonexistent/lsof bash -c ". '$F'; reap_lane_tmp 0")"
[ -d "$R/lane5/old" ] && [ -d "$R/lane2/inuse" ] && printf '%s' "$outn" | grep -q 'lane_tmp_cwd_unmeasured' && echo "ok   with no lsof nothing is removed, and it says so" || { echo "FAIL no-lsof: $outn"; fail=1; }
out3="$(LANE_TMP_SCHED_DB="$T/missing.db" LANE_TMP_ROOT="$R" LANE_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_lane_tmp 0")"
[ -d "$R/lane5/old" ] && printf '%s' "$out3" | grep -q 'lane_tmp_schedules_unmeasured' && echo "ok   unreadable schedules reap nothing and say so" || { echo "FAIL unreadable schedules: $out3"; fail=1; }
mkdir -p "$R/lane3/a" "$R/lane3/b" "$R/lane3/c"; touch -t 202609300000 "$R/lane3/a" "$R/lane3/b" "$R/lane3/c"
out2="$(LANE_TMP_ROOT="$R" LANE_TMP_IDLE_MIN=1440 LANE_TMP_MAX=2 bash -c ". '$F'; reap_lane_tmp 0")"
left=$(ls "$R/lane3" | wc -l | tr -d ' ')
[ "$left" = 1 ] && printf '%s' "$out2" | grep -q 'capped at 2' && echo "ok   a pass removes at most LANE_TMP_MAX entries and says it was capped" || { echo "FAIL cap: left=$left out=$out2"; fail=1; }
grep -q 'AMUX_CLEANUP_LANE_TMP=0' "$ROOT/scripts/test-mac-cleanup-tick.sh" && echo "ok   the full-tick test never reaps the live lane temp dirs" || { echo "FAIL full-tick test does not disable the lane reaper"; fail=1; }
# 2026-10-14: an unbounded find over a lane's 23 build checkouts ran the whole
# tick past the scheduler's 600s kill, six ticks in a row. A `find` stub that is
# slow for chosen entries stands in for a huge tree under load.
STUB="$T/stub"; mkdir -p "$STUB"
cat > "$STUB/find" <<'SH'
#!/bin/bash
case "$*" in *SLOW*) sleep "${SLOW_FIND_S:-5}" ;; esac
exec /usr/bin/find "$@"
SH
chmod +x "$STUB/find"
R6="$T/root6"; mkdir -p "$R6/lane/aSLOW" "$R6/lane/bold"; touch -t 202609300000 "$R6/lane/aSLOW" "$R6/lane/bold"
out6="$(PATH="$STUB:$PATH" LANE_TMP_ROOT="$R6" LANE_TMP_IDLE_MIN=1440 LANE_TMP_WALK_S=1 bash -c ". '$F'; reap_lane_tmp 0")"
[ -d "$R6/lane/aSLOW" ] && echo "ok   an entry whose idle walk runs past the walk limit is kept" || { echo "FAIL a slow-walk entry was removed"; fail=1; }
[ ! -e "$R6/lane/bold" ] && echo "ok   and the pass goes on to judge the next entry" || { echo "FAIL the pass stopped at the slow entry: $out6"; fail=1; }
printf '%s' "$out6" | grep -q 'kept 1 whose idle walk ran past 1s' && echo "ok   the unknown entry is counted in the log" || { echo "FAIL unknown count: $out6"; fail=1; }
R7="$T/root7"; ST7="$T/state7"; mkdir -p "$R7/lane/aSLOW" "$R7/lane/bSLOW" "$R7/lane/cSLOW"
touch -t 202609300000 "$R7/lane/aSLOW" "$R7/lane/bSLOW" "$R7/lane/cSLOW"
out7="$(PATH="$STUB:$PATH" SLOW_FIND_S=2 STATE_DIR="$ST7" LANE_TMP_ROOT="$R7" LANE_TMP_IDLE_MIN=1440 LANE_TMP_WALK_S=10 LANE_TMP_BUDGET_S=1 bash -c ". '$F'; reap_lane_tmp 0")"
[ ! -e "$R7/lane/aSLOW" ] && [ -d "$R7/lane/bSLOW" ] && [ -d "$R7/lane/cSLOW" ] && echo "ok   a pass stops judging once its budget is spent" || { echo "FAIL budget: $(ls "$R7/lane" | tr '\n' ' ') $out7"; fail=1; }
printf '%s' "$out7" | grep -q '2 not reached inside the 1s budget, the next tick starts at lane/bSLOW' && echo "ok   and says where the next tick resumes" || { echo "FAIL budget log: $out7"; fail=1; }
mkdir -p "$R7/lane/0first"; touch -t 202609300000 "$R7/lane/0first"
PATH="$STUB:$PATH" SLOW_FIND_S=2 STATE_DIR="$ST7" LANE_TMP_ROOT="$R7" LANE_TMP_IDLE_MIN=1440 LANE_TMP_WALK_S=10 LANE_TMP_BUDGET_S=1 bash -c ". '$F'; reap_lane_tmp 0" >/dev/null
[ ! -e "$R7/lane/bSLOW" ] && [ -d "$R7/lane/0first" ] && echo "ok   the next pass resumes at the cursor, ahead of an earlier entry" || { echo "FAIL cursor: $(ls "$R7/lane" | tr '\n' ' ')"; fail=1; }
kill "$holder" 2>/dev/null; wait 2>/dev/null
rm -f "$F"; rm -rf -- "${T:?}"
exit $fail
