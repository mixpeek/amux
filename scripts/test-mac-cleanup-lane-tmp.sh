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
# The cells below use a few small files to stand for idle build output, so they
# pin the directory keep at 1.5 MB; the lane7 cell checks the 64 MB default.
export LANE_TMP_KEEP_DIR_KB=1500
# Reapable content is BIG: a build artifact. A directory of a few small files
# is now kept (next cell), so a one-byte file no longer stands for "idle junk".
head -c 2000000 /dev/zero > "$R/lane1/oldidle/sub/f"; echo y > "$R/lane1/oldtopfreshinside/deep/new"
# mxp-gs12, 2026-10-17: a manual gate a production promote waits on, in a dir.
mkdir -p "$R/lane6/mvs-drill"; echo ok > "$R/lane6/mvs-drill/standby_ok"; echo 'echo go' > "$R/lane6/mvs-drill/promote.sh"
touch -t 202609300000 "$R/lane6/mvs-drill/standby_ok" "$R/lane6/mvs-drill/promote.sh" "$R/lane6/mvs-drill"
# Many small files is a dependency or build tree, not a gate: still reaped.
mkdir -p "$R/lane6/many"; for i in $(seq 1 80); do echo $i > "$R/lane6/many/f$i"; done
touch -t 202609300000 "$R/lane6/many"/* "$R/lane6/many"
touch -t 202609300000 "$R/lane1/oldidle/sub/f" "$R/lane1/oldidle/sub" "$R/lane1/oldidle" "$R/lane1/oldtopfreshinside" "$R/lane2/inuse"
# The holder lives until the end of the test, which kills it: a run takes 22s
# on a quiet Mac and 53s on a loaded one, and a 30s holder ended mid-run and
# failed the in-use checks (the pre-push of 519c18cf1).
(cd "$R/lane2/inuse" && exec sleep 600) & holder=$!
trap 'kill "$holder" 2>/dev/null' EXIT
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
printf '%s' "$out" | grep -q 'lane tmp: removed 3 entries' && echo "ok   the tick logs the count" || { echo "FAIL log: $out"; fail=1; }
[ -f "$R/lane6/mvs-drill/standby_ok" ] && echo "ok   a directory of a few small files (a manual gate) is kept" || { echo "FAIL small gate dir reaped"; fail=1; }
[ ! -e "$R/lane6/many" ] && echo "ok   a directory of many small files is still reaped" || { echo "FAIL many-small-files dir kept"; fail=1; }
printf '%s' "$out" | grep -q '1 small dir(s)' && echo "ok   kept small dirs are counted in the log" || { echo "FAIL small dir count: $out"; fail=1; }
[ -f "$R/lane4/sched.sh" ] && [ -d "$R/lane4/scheddir" ] && echo "ok   entries an enabled schedule names are kept (~ and \$HOME forms)" || { echo "FAIL a schedule's file was reaped"; fail=1; }
[ -f "$R/lane4/kc" ] && echo "ok   a small old file is kept" || { echo "FAIL small file reaped"; fail=1; }
[ ! -e "$R/lane4/big.bin" ] && echo "ok   a large old file is still reaped" || { echo "FAIL large idle file kept"; fail=1; }
printf '%s' "$out" | grep -q 'kept 2 named by an enabled schedule, 1 small file' && echo "ok   the keeps are counted in the log" || { echo "FAIL keep counts: $out"; fail=1; }
# 2026-11-16: S1-investigation results, a few files over 1 MB in a 9-12 MB dir,
# were reaped. With the default keep they stay; a dir past 64 MB still goes.
mkdir -p "$R/lane7/s1-blast" "$R/lane7/extract"
head -c 3000000 /dev/urandom > "$R/lane7/s1-blast/annotated.json"; head -c 3000000 /dev/urandom > "$R/lane7/s1-blast/api-view.jsonl"
head -c 70000000 /dev/zero > "$R/lane7/extract/blob"
touch -t 202609300000 "$R/lane7/s1-blast"/* "$R/lane7/s1-blast" "$R/lane7/extract/blob" "$R/lane7/extract"
out7="$(env -u LANE_TMP_KEEP_DIR_KB LANE_TMP_ROOT="$R" LANE_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_lane_tmp 0")"
[ -f "$R/lane7/s1-blast/annotated.json" ] && echo "ok   a few results files over 1 MB in a small dir are kept" || { echo "FAIL results dir reaped: $out7"; fail=1; }
[ ! -e "$R/lane7/extract" ] && echo "ok   a dir past 64 MB is still reaped" || { echo "FAIL big extract kept"; fail=1; }
printf '%s' "$out7" | grep -q 'under 65536K in all' && echo "ok   the log states the directory keep" || { echo "FAIL keep wording: $out7"; fail=1; }
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
# backend, 2026-11-03: hand labels beside big embeddings were removed with only
# a count logged. Each removal names its path, size and floor, in the output
# and in a ledger; a .amux-keep file at an entry's top keeps the entry.
R8="$T/root8"; ST8="$T/state8"; mkdir -p "$R8/w/evalset2" "$R8/w/scratchbuild"
echo '[]' > "$R8/w/evalset2/labelled.json"; head -c 2000000 /dev/zero > "$R8/w/evalset2/emb.npz"; : > "$R8/w/evalset2/.amux-keep"
head -c 2000000 /dev/zero > "$R8/w/scratchbuild/out.o"
touch -t 202609300000 "$R8/w/evalset2"/* "$R8/w/evalset2/.amux-keep" "$R8/w/evalset2" "$R8/w/scratchbuild/out.o" "$R8/w/scratchbuild"
outd="$(STATE_DIR="$ST8" LANE_TMP_ROOT="$R8" LANE_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_lane_tmp 1")"
printf '%s' "$outd" | grep -qF "lane tmp would remove $R8/w/scratchbuild (1M" && [ -d "$R8/w/scratchbuild" ] && [ ! -s "$ST8/lane-tmp-removed.log" ] && echo "ok   a dry run names each entry it would remove and touches nothing" || { echo "FAIL dry run: $outd"; fail=1; }
out8="$(STATE_DIR="$ST8" LANE_TMP_ROOT="$R8" LANE_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_lane_tmp 0")"
[ -f "$R8/w/evalset2/labelled.json" ] && printf '%s' "$out8" | grep -q '1 marked .amux-keep' && echo "ok   a directory marked .amux-keep is kept and counted" || { echo "FAIL keep marker: $out8"; fail=1; }
[ ! -e "$R8/w/scratchbuild" ] && printf '%s' "$out8" | grep -qF "lane tmp removed $R8/w/scratchbuild (1M, nothing changed in 1440 min) (verdict=lane_tmp_entry_removed)" && echo "ok   each removal is logged with its path, size and floor" || { echo "FAIL per-entry line: $out8"; fail=1; }
grep -qF "$(printf 'w/scratchbuild\t')" "$ST8/lane-tmp-removed.log" 2>/dev/null && ! grep -q evalset2 "$ST8/lane-tmp-removed.log" && echo "ok   and appended to the removal ledger" || { echo "FAIL ledger: $(cat "$ST8/lane-tmp-removed.log" 2>&1)"; fail=1; }
# amux, 2026-11-12: a worktree with a tested, uncommitted fix idled past the
# floor and was removed. A checkout with edited tracked files or commits on no
# remote is kept and named; a clean checkout of pushed commits still goes.
R9="$T/root9"; mkdir -p "$R9/w"
# The CI runner has no git identity, so without one the seed commit fails and
# root9 stays empty: the gate's only reds on 8eefef59f, 4b22ba9c4, 2857482b7.
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
git init -q "$T/seed9" && git -C "$T/seed9" commit -q --allow-empty -m base && git clone -q --bare "$T/seed9" "$T/origin9"
for d in dirty ahead clean; do git clone -q "$T/origin9" "$R9/w/$d"; head -c 2000000 /dev/zero > "$R9/w/$d/big.bin"; done
git -C "$R9/w/dirty" add big.bin && git -C "$R9/w/dirty" commit -q -m big && git -C "$R9/w/dirty" push -q origin HEAD 2>/dev/null
echo edit >> "$R9/w/dirty/big.bin"
git -C "$R9/w/ahead" add big.bin && git -C "$R9/w/ahead" commit -q -m local
find "$R9/w" -exec touch -t 202609300000 {} +
out9="$(LANE_TMP_ROOT="$R9" LANE_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_lane_tmp 0")"
[ -d "$R9/w/dirty" ] && printf '%s' "$out9" | grep -qF "lane tmp kept $R9/w/dirty: a git checkout with unsaved work (uncommitted changes) (verdict=lane_tmp_unsaved_work_kept)" && echo "ok   a checkout with uncommitted changes is kept and named" || { echo "FAIL dirty checkout: $out9"; fail=1; }
[ -d "$R9/w/ahead" ] && printf '%s' "$out9" | grep -qF "(commits on no remote)" && echo "ok   a checkout with commits on no remote is kept" || { echo "FAIL unpushed checkout: $out9"; fail=1; }
[ ! -e "$R9/w/clean" ] && printf '%s' "$out9" | grep -q '2 git checkout(s) with unsaved work' && echo "ok   a clean checkout of pushed commits is still reaped, and the kept ones counted" || { echo "FAIL clean checkout: $out9"; fail=1; }
# mixpeek-research, 2026-11-15: a 1.3 GB read-only cache was removed three
# times; reads change no mtime. An idle entry at or over LANE_TMP_NOTICE_KB is
# given notice first, its lane is told, and it goes only after the notice.
R10="$T/root10"; ST10="$T/state10"; SENT="$T/sent10"; mkdir -p "$R10/lab/cache" "$R10/lab/build" "$R10/lab/live" "$SENT"
head -c 2000000 /dev/zero > "$R10/lab/cache/sim.bin"; head -c 1300000 /dev/zero > "$R10/lab/build/o.bin"; head -c 2000000 /dev/zero > "$R10/lab/live/w.bin"
find "$R10" -exec touch -t 202609300000 {} +
rec="cp FILE $SENT/TARGET.\$\$.msg"
tick10() { LANE_TMP_KEEP_DIR_KB=1000 LANE_TMP_ROOT="$R10" LANE_TMP_IDLE_MIN=1440 STATE_DIR="$ST10" LANE_TMP_NOTICE_KB=1500 LANE_TMP_NOTICE_CMD="$rec" "$@" bash -c ". '$F'; reap_lane_tmp 0"; }
o1="$(tick10)"
[ -d "$R10/lab/cache" ] && [ -d "$R10/lab/live" ] && printf '%s' "$o1" | grep -qF "lane tmp gave notice for $R10/lab/cache (1M): removed after 360 min if still idle (verdict=lane_tmp_notice_given)" \
  && echo "ok   a large idle entry is given notice, not removed" || { echo "FAIL notice: $o1"; fail=1; }
[ ! -e "$R10/lab/build" ] && echo "ok   an idle entry under the notice size is removed at once" || { echo "FAIL small entry kept: $o1"; fail=1; }
m1=$(cat "$SENT"/lab.*.msg 2>/dev/null)
[ "$(ls "$SENT" | wc -l | tr -d ' ')" = 1 ] && printf '%s' "$m1" | grep -qF -- "- $R10/lab/cache (1M" && printf '%s' "$m1" | grep -qF -- "- $R10/lab/live (1M" && printf '%s' "$m1" | grep -qF '.amux-keep' \
  && printf '%s' "$o1" | grep -qF 'lane tmp told lab about 2 entr(ies) (verdict=lane_tmp_notice_sent)' && echo "ok   the lane gets one message naming each entry and how to keep it" || { echo "FAIL message: $(ls "$SENT") $m1 $o1"; fail=1; }
o2="$(tick10)"
[ -d "$R10/lab/cache" ] && [ "$(ls "$SENT" | wc -l | tr -d ' ')" = 1 ] && printf '%s' "$o2" | grep -qF '2 large entr(ies) inside their 360 min notice' \
  && echo "ok   inside its notice an entry is kept and the lane is not told again" || { echo "FAIL second tick: $o2"; fail=1; }
touch "$R10/lab/live/w.bin"; tick10 >/dev/null
! grep -q '^lab/live' "$ST10/lane-tmp-notice.tsv" && grep -q '^lab/cache' "$ST10/lane-tmp-notice.tsv" && echo "ok   a write during the notice withdraws it" || { echo "FAIL withdraw: $(cat "$ST10/lane-tmp-notice.tsv")"; fail=1; }
o4="$(tick10 env LANE_TMP_NOTICE_MIN=0)"
[ ! -e "$R10/lab/cache" ] && [ -d "$R10/lab/live" ] && ! grep -q '^lab/cache' "$ST10/lane-tmp-notice.tsv" && printf '%s' "$o4" | grep -qF "lane tmp removed $R10/lab/cache" \
  && echo "ok   after its notice a still-idle entry is removed and its notice cleared" || { echo "FAIL after notice: $o4"; fail=1; }
mkdir -p "$R10/iso/cache"; head -c 2000000 /dev/zero > "$R10/iso/cache/x.bin"; find "$R10/iso" -exec touch -t 202609300000 {} +
o5="$(LANE_TMP_ROOT="$R10" LANE_TMP_IDLE_MIN=1440 STATE_DIR="$ST10" LANE_TMP_NOTICE_KB=1500 LANE_TMP_NOTICE_CMD=false bash -c ". '$F'; reap_lane_tmp 0")"
[ -d "$R10/iso/cache" ] && printf '%s' "$o5" | grep -qF 'could not tell iso about 1 entr(ies); they still wait 360 min (verdict=lane_tmp_notice_undelivered)' \
  && echo "ok   a lane that cannot be told is logged and its entry still waits" || { echo "FAIL undelivered: $o5"; fail=1; }
kill "$holder" 2>/dev/null; wait 2>/dev/null
rm -f "$F"; rm -rf -- "${T:?}"
exit $fail
