#!/usr/bin/env bash
# test-mac-cleanup-user-tmp.sh: the cleanup tick's reap_user_tmp removes tmp.*
# dirs in the per-user temp dir idle past the floor and not used as any
# process's cwd, and keeps fresh ones and in-use ones (2026-10-03: 1,674
# leaked repo extracts, 49 GB, in /var/folders/.../T).
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/ut.XXXXXX")"; R="$T/root"; mkdir -p "$R"
mkdir -p "$R/tmp.oldidle1" "$R/tmp.fresh0001" "$R/tmp.oldinuse1" "$R/other.keepme"
echo x > "$R/tmp.oldidle1/f"
touch -t 202609300000 "$R/tmp.oldidle1" "$R/tmp.oldinuse1" "$R/other.keepme"
(cd "$R/tmp.oldinuse1" && exec sleep 30) & holder=$!
sleep 1
F="$(mktemp "${TMPDIR:-/tmp}/utf.XXXXXX")"
sed -n '/^reap_user_tmp() {/,/^}/p' "$ROOT/scripts/mac-cleanup-tick.sh" > "$F"
fail=0
[ -s "$F" ] || { echo "FAIL reap_user_tmp is not in the tick"; exit 1; }
out="$(USER_TMP_ROOT="$R" USER_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_user_tmp 0")"
[ ! -e "$R/tmp.oldidle1" ] && echo "ok   an idle day-old tmp dir is removed" || { echo "FAIL old idle dir kept"; fail=1; }
[ -d "$R/tmp.fresh0001" ] && echo "ok   a fresh tmp dir is kept" || { echo "FAIL fresh dir removed"; fail=1; }
[ -d "$R/tmp.oldinuse1" ] && echo "ok   an old tmp dir in use as a cwd is kept" || { echo "FAIL in-use dir removed"; fail=1; }
[ -d "$R/other.keepme" ] && echo "ok   a non-tmp.* dir is never touched" || { echo "FAIL other dir removed"; fail=1; }
printf '%s' "$out" | grep -q 'removed 1 tmp.\* dir' && echo "ok   the tick logs the count" || { echo "FAIL log: $out"; fail=1; }
grep -q 'AMUX_CLEANUP_USER_TMP=0' "$ROOT/scripts/test-mac-cleanup-tick.sh" && echo "ok   the full-tick test never reaps the live temp dir" || { echo "FAIL test-mac-cleanup-tick.sh does not disable the reaper"; fail=1; }
# DESKT-95: lsof that cannot run must remove NOTHING. An empty in-use list used
# to read as "nothing is in use" and the in-use dir above was deleted.
mkdir -p "$R/tmp.oldidle2"; touch -t 202609300000 "$R/tmp.oldidle2"
out="$(USER_TMP_ROOT="$R" USER_TMP_IDLE_MIN=1440 AMUX_CLEANUP_LSOF_BIN=/nonexistent/lsof bash -c ". '$F'; reap_user_tmp 0")"
[ -d "$R/tmp.oldidle2" ] && [ -d "$R/tmp.oldinuse1" ] && echo "ok   with no lsof nothing is removed" || { echo "FAIL removed with no lsof"; fail=1; }
printf '%s' "$out" | grep -q 'UNMEASURED.*verdict=user_tmp_cwd_unmeasured' && echo "ok   and it says the in-use check did not run" || { echo "FAIL no unmeasured line: $out"; fail=1; }
# Disk RCA 20261029-103955: Rust test helpers leaked tempfile .tmpXXXXXX dirs
# (29 GB). An old one whose holder is gone goes; one whose holder-lock pid is
# alive stays, and so does a fresh one.
mkdir -p "$R/.tmpDeadA1" "$R/.tmpLiveB2" "$R/.tmpFresh3"
echo db > "$R/.tmpDeadA1/t.db"; printf '{"pid":999999,"port":8823}' > "$R/.tmpDeadA1/t.db.holder-lock"
printf '{"pid":%s,"port":8823}' "$$" > "$R/.tmpLiveB2/t.db.holder-lock"
touch -t 202609300000 "$R/.tmpDeadA1" "$R/.tmpLiveB2"
out="$(USER_TMP_ROOT="$R" USER_TMP_IDLE_MIN=1440 bash -c ". '$F'; reap_user_tmp 0")"
[ ! -e "$R/.tmpDeadA1" ] && echo "ok   an old .tmp test dir whose holder is gone is removed" || { echo "FAIL dead .tmp dir kept: $out"; fail=1; }
[ -d "$R/.tmpLiveB2" ] && echo "ok   one whose holder-lock pid is alive is kept" || { echo "FAIL live .tmp dir removed"; fail=1; }
[ -d "$R/.tmpFresh3" ] && echo "ok   a fresh .tmp dir is kept" || { echo "FAIL fresh .tmp dir removed"; fail=1; }
printf '%s' "$out" | grep -q 'and 1 .tmp\* test dir(s).*kept 1 whose holder-lock pid is alive.*verdict=user_tmp_reaped' && echo "ok   the tick logs both counts" || { echo "FAIL log: $out"; fail=1; }
# The per-tick cap: with a cap of 1 and two old dead dirs, one goes now.
mkdir -p "$R/.tmpCapC1" "$R/.tmpCapC2"; touch -t 202609300000 "$R/.tmpCapC1" "$R/.tmpCapC2"
out="$(USER_TMP_ROOT="$R" USER_TMP_IDLE_MIN=1440 USER_TMP_MAX=1 bash -c ". '$F'; reap_user_tmp 0")"
left=$(ls -d "$R"/.tmpCapC* 2>/dev/null | wc -l | tr -d ' ')
[ "$left" = 1 ] && printf '%s' "$out" | grep -q 'capped at 1 this tick' && echo "ok   the cap leaves the rest for the next tick" || { echo "FAIL cap: left=$left $out"; fail=1; }
kill "$holder" 2>/dev/null; wait 2>/dev/null
rm -f "$F"; rm -rf -- "${T:?}"
exit $fail
