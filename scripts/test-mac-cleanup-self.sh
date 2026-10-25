#!/usr/bin/env bash
# test-mac-cleanup-self.sh: the cleanup tick checks ITSELF each run.
#
# 2026-10-14/15: six runs in a row were killed at the scheduler's 600s limit,
# then five fires recorded as "ok" did no work because a duplicate tick from
# the launchd fallback held the lock, and nothing anywhere said so.
# check_self_runs reads the schedule's run rows, its run logs and the fallback's
# log, and adds a "self ..." constraint for each failing rule. Every input is a
# fixture here: no cell reads the live database or logs.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d "${TMPDIR:-/tmp}/self.XXXXXX")
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { printf '%s' "$1" | grep -qE -- "$2" && echo yes || echo no; }
age() { perl -e 'my $t = time - shift; utime $t, $t, @ARGV' "$1" "${@:2}"; }   # <seconds ago> <file>...
stamp() { date -r "$1" '+%F %T' 2>/dev/null || date -d "@$1" '+%F %T'; }       # <epoch> as the fallback logs it
NOW=$(date +%s)
DB="$FIX/amux.db"; LOGS="$FIX/runs"; FB="$FIX/fallback.log"
newdb() { # <enabled 0|1|missing>, then rows "<seconds ago>|<status>|<note>" on stdin
  rm -f -- "${DB:?}"
  sqlite3 "$DB" "CREATE TABLE schedules (id TEXT, enabled INTEGER, deleted INTEGER);
    CREATE TABLE schedule_runs (id INTEGER PRIMARY KEY, schedule_id TEXT, ran_at INTEGER, status TEXT, note TEXT);"
  [ "$1" = missing ] || sqlite3 "$DB" "INSERT INTO schedules VALUES ('SCHED-T', $1, NULL)"
  local ago st note
  while IFS='|' read -r ago st note; do
    [ -n "$ago" ] || continue
    sqlite3 "$DB" "INSERT INTO schedule_runs (schedule_id, ran_at, status, note) VALUES ('SCHED-T', $((NOW-ago)), '$st', '$note')"
  done
}
newlogs() { rm -rf -- "${LOGS:?}"; mkdir -p "$LOGS"; : > "$FB"; }
addlog() { printf '%s\n' "$3" > "$LOGS/$1.log"; age "$2" "$LOGS/$1.log"; }   # <name> <seconds ago> <content>
probe() { # the self line, a blank line, then the verdicts
  # Env, never positional args: the tick parses "$@" as its own flags when sourced.
  TICK_PATH="$TICK" AMUX_CLEANUP_SELF_SCHED_ID=SCHED-T AMUX_CLEANUP_SELF_DB="$DB" AMUX_CLEANUP_SELF_RUNS_DIR="$LOGS" \
  AMUX_CLEANUP_SELF_FALLBACK_LOG="$FB" AMUX_CLEANUP_STATE_DIR="$FIX/state" bash -c '
    AMUX_CLEANUP_LIB_ONLY=1 AMUX_CLEANUP_SCOPE_FILE=/dev/null . "$TICK_PATH"; HEALTH_VERDICTS=""
    check_self_runs; echo; printf "%s\n" "$HEALTH_VERDICTS" | grep . || true'; }
DONE="mac-cleanup: done elapsed=300s purge=no skipped=none constraints=0"
LOCKED="mac-cleanup: previous tick still running (pid 1 holds /x/tick.lock), not starting a second one"

echo "1. a healthy day raises nothing and says what it read"
newdb 1 <<<"$(printf '%s\n' '600|ok|' '4200|ok|' '7800|ok|')"; newlogs
addlog a 900 "$DONE"; addlog b 4500 "$DONE"
out=$(probe)
check "the self line counts the runs"                 yes "$(has "$out" 'SCHED-T last 24h: 3 finished runs, 3 ok, 0 not ok;')"
check "and the logs"                                  yes "$(has "$out" '2 run log\(s\), 0 killed before the done line, 0 found another tick running')"
check "no self constraint"                            no  "$(has "$out" '^self ')"

echo "2. two failures in a row raise one constraint naming the cause"
newdb 1 <<<"$(printf '%s\n' '600|error|delivery failed: timed out after 600s' '4200|error|server restarted before this fire recorded a delivery outcome' '7800|ok|' '9000|running|')"; newlogs
out=$(probe)
check "the streak is a constraint"                    yes "$(has "$out" '^self 2 runs in a row of SCHED-T did not finish ok')"
check "it names the time-limit kill"                  yes "$(has "$out" "killed at the scheduler's time limit x1")"
check "and the server-restart kill"                   yes "$(has "$out" 'killed by an amux server restart x1')"
check "a running row is not counted"                  yes "$(has "$out" '3 finished runs, 1 ok, 2 not ok')"

echo "3. scattered failures count over 24h, and older rows do not"
newdb 1 <<<"$(printf '%s\n' '600|ok|' '4200|error|x' '7800|ok|' '11400|error|x' '15000|error|x' '18600|error|x' '90000|error|old')"; newlogs
out=$(probe)
check "four in 24h is a constraint"                   yes "$(has "$out" '^self 4 of 6 runs of SCHED-T in 24h did not finish ok')"
check "a newest ok means no streak"                   no  "$(has "$out" 'in a row of SCHED-T')"
check "a row over 24h old is not read"                no  "$(has "$out" 'old')"

echo "3a. failed runs already escalated are not raised again (RCA self 20261021-125823)"
mkdir -p "$FIX/state"; printf 'esc_self=%s\n' "$((NOW-3000))" > "$FIX/state/state"
out=$(probe)
check "every failure predates the last escalation: no constraint" no "$(has "$out" '^self 4 of 6 runs')"
check "it says so, with a verdict"                    yes "$(has "$out" 'all 4 failed runs of SCHED-T in 24h predate the last self escalation.*verdict=self_failures_already_escalated')"
printf 'esc_self=%s\n' "$((NOW-5000))" > "$FIX/state/state"
out=$(probe)
check "a failure after the escalation raises it again" yes "$(has "$out" '^self 4 of 6 runs of SCHED-T in 24h did not finish ok')"
rm -f "$FIX/state/state"

echo "4. the run logs: slow, skipped, killed and blocked runs"
newdb 1 <<<"600|ok|"; newlogs
addlog slow 900 "mac-cleanup: done elapsed=500s purge=no skipped=none constraints=0"
for n in 1 2 3; do addlog "skip$n" $((900+n*60)) "mac-cleanup: done elapsed=200s purge=no skipped=lane_tmp constraints=0"; done
addlog killed 3000 "mac-cleanup: measured=true"
addlog lock1 3600 "$LOCKED"; addlog lock2 7200 "$LOCKED"
addlog live 60 "mac-cleanup: measured=true"
addlog ancient 90000 "mac-cleanup: measured=true"
out=$(probe)
check "a slow run is a constraint"                    yes "$(has "$out" '^self the slowest run of SCHED-T in the last 6h took 500s, over the 480s warning')"
check "three runs that skipped arms are one"          yes "$(has "$out" '^self 3 runs of SCHED-T in 24h skipped cleanup arms')"
check "blocked fires are one, though recorded ok"     yes "$(has "$out" '^self 2 fires of SCHED-T in 24h found another tick holding the lock')"
check "a killed run is counted, a blocked one is not" yes "$(has "$out" '1 killed before the done line, 2 found another tick running')"
check "a log still being written is not judged"       yes "$(has "$out" ' 7 run log\(s\)')"

echo "4a2. skipped runs already escalated are not raised again (RCA self 20261020-005306)"
mkdir -p "$FIX/state"; printf 'esc_self=%s\n' "$((NOW-600))" > "$FIX/state/state"
out=$(probe)
check "every skip predates the last escalation: no constraint" no "$(has "$out" '^self 3 runs of SCHED-T in 24h skipped cleanup arms')"
check "it says so, with a verdict"                    yes "$(has "$out" 'all 3 runs that skipped arms in 24h predate the last self escalation.*verdict=self_skips_already_escalated')"
printf 'esc_self=%s\n' "$((NOW-1500))" > "$FIX/state/state"
out=$(probe)
check "a skip after the escalation raises it again"   yes "$(has "$out" '^self 3 runs of SCHED-T in 24h skipped cleanup arms')"
rm -f "$FIX/state/state"

echo "4b. a slow run older than 6h is reported, not escalated (RCA self 20261016-082126)"
newdb 1 <<<"600|ok|"; newlogs
addlog old 61200 "mac-cleanup: done elapsed=497s purge=no skipped=none constraints=0"
addlog new 900 "mac-cleanup: done elapsed=402s purge=no skipped=none constraints=0"
out=$(probe)
check "the 24h line still names it and its age"      yes "$(has "$out" 'slowest 497s 17h ago, slowest in the last 6h 402s')"
check "no speed constraint from a pre-fix run"        no  "$(has "$out" '^self the slowest run')"
check "it says why, with a verdict"                   yes "$(has "$out" 'verdict=self_slow_run_aged_out')"
addlog new2 1800 "mac-cleanup: done elapsed=481s purge=no skipped=none constraints=0"
out=$(probe)
check "a recent slow run still escalates"             yes "$(has "$out" '^self the slowest run of SCHED-T in the last 6h took 481s')"

echo "4c. GNU stat (the Linux CI runner): -f means --file-system, so the age read must try -c first"
# The tick puts /usr/bin first on PATH, so on a Mac only a shell function can stand in
# for GNU stat. 86d4bd589 read "stat -f %m" first and reddened CI with "File: unbound variable".
gnu_stat() { case "$1" in -f) printf '  File: "%s"\n    ID: 0 Type: ext4\n' "${3:-}"; return 1 ;; -c) /usr/bin/perl -e 'print +(stat shift)[9], "\n"' "$3" ;; esac; }
export -f gnu_stat
out=$(stat() { gnu_stat "$@"; }; export -f stat; probe 2>&1 || true)
check "an age read under GNU stat does not crash"     no  "$(has "$out" 'unbound variable')"
check "and still judges the recent run"               yes "$(has "$out" 'slowest in the last 6h 481s')"

echo "4d. a fire that met an earlier run of the same schedule lost nothing (RCA self 20261023-145529)"
# Run logs are named after their start in ms. A manual run started 5 min before
# the cron fire and still writing when it came is the hour's check.
newdb 1 <<<"600|ok|"; newlogs
for h in 1 2; do
  c=$((NOW - h*3600)); addlog "$((c - 300))000" $((h*3600 - 60)) "$DONE"; addlog "${c}000" $((h*3600)) "$LOCKED"
done
out=$(probe)
check "covered fires raise no constraint"             no  "$(has "$out" '^self 2 fires of SCHED-T in 24h found another tick')"
check "they are counted apart, with a verdict"        yes "$(has "$out" '0 found another tick running and did nothing, 2 met an earlier run of this schedule still working')"
check "and the verdict names why"                     yes "$(has "$out" 'verdict=self_collision_covered')"
# The same two fires, but each earlier run had already finished: something
# outside the schedule held the lock, which is the 2026-10-15 duplicate tick.
newlogs
for h in 1 2; do
  c=$((NOW - h*3600)); addlog "$((c - 900))000" $((h*3600 + 300)) "$DONE"; addlog "${c}000" $((h*3600)) "$LOCKED"
done
out=$(probe)
check "a holder outside the schedule still counts"    yes "$(has "$out" '^self 2 fires of SCHED-T in 24h found another tick holding the lock')"
check "and is not called covered"                     no  "$(has "$out" 'verdict=self_collision_covered')"

echo "5. the fallback's own log"
newdb 1 <<<"600|ok|"; newlogs
for h in 1 2 3; do echo "mac-cleanup-fallback: $(stamp $((NOW-h*3600))) last tick output is 90m old (over 80m): the amux scheduler is not running SCHED-T, so launchd is" >> "$FB"; done
echo "mac-cleanup-fallback: $(stamp $((NOW-100000))) x: the amux scheduler is not running SCHED-T, so launchd is" >> "$FB"
echo "mac-cleanup-fallback: $(stamp "$NOW") scheduler ran within 80m, nothing to do" >> "$FB"
out=$(probe)
check "three fallback runs in 24h are a constraint"   yes "$(has "$out" '^self the launchd fallback ran this check 3 times in 24h')"
check "older runs and no-op lines are not counted"    yes "$(has "$out" 'the launchd fallback ran it 3x')"
# Self RCA 20261018-165251: runs already escalated are not raised again.
mkdir -p "$FIX/state"; echo "esc_self=$((NOW-1800))" > "$FIX/state/state"; out=$(probe)
check "runs older than the last self escalation are not raised again" no "$(has "$out" '^self the launchd fallback')"
check "and it says why"                               yes "$(has "$out" 'verdict=self_fallback_already_escalated')"
echo "mac-cleanup-fallback: $(stamp $((NOW-60))) the last tick did not finish: the amux scheduler is not running SCHED-T, so launchd is" >> "$FB"; out=$(probe)
check "control: one newer fallback run raises it again" yes "$(has "$out" '^self the launchd fallback ran this check 4 times in 24h')"
rm -f -- "$FIX/state/state"

echo "6. a disabled or missing schedule"
newdb 0 <<<""; newlogs; out=$(probe)
check "disabled is a constraint"                      yes "$(has "$out" '^self SCHED-T is disabled')"
newdb missing <<<""; out=$(probe)
check "missing is a constraint"                       yes "$(has "$out" '^self SCHED-T is missing')"

echo "7. an unreadable history judges nothing and says so"
rm -f -- "${DB:?}"; mkdir -p "$DB"; out=$(probe)
check "it says unmeasured"                            yes "$(has "$out" 'verdict=self_unmeasured')"
check "and raises nothing"                            no  "$(has "$out" '^self ')"
rmdir -- "${DB:?}"

echo "8. system_health runs it and carries its constraints"
newdb 0 <<<""; newlogs
out=$(TICK_PATH="$TICK" FIXD="$FIX" AMUX_CLEANUP_AGENT_VOLUME_CMD=true AMUX_CLEANUP_UTIL_CMD=true AMUX_CLEANUP_SELF_SCHED_ID=SCHED-T AMUX_CLEANUP_SELF_DB="$DB" AMUX_CLEANUP_SELF_RUNS_DIR="$LOGS" \
  AMUX_CLEANUP_SELF_FALLBACK_LOG="$FB" bash -c '
  AMUX_CLEANUP_LIB_ONLY=1 AMUX_CLEANUP_SCOPE_FILE=/dev/null . "$TICK_PATH"
  HEALTH_PROBES=on HEALTH_PS_CMD=true ORPHAN_PS_CMD=true LIMITS_CMD=true LAUNCHD_LIST_CMD=true NET_CMD=true THERMAL_CMD=true
  CRASH_DIR="$FIXD/none" STATE_DIR="$FIXD/state"
  system_health 0; printf "%s\n" "$HEALTH_VERDICTS"' 2>&1)
check "the self line is in the health section"        yes "$(has "$out" 'mac-cleanup: self: SCHED-T last 24h')"
check "its constraint reaches HEALTH_VERDICTS"        yes "$(has "$out" '^self SCHED-T is disabled')"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-self — all checks passed"; exit 0; fi
echo "mac-cleanup-self: $fails check(s) FAILED"; exit 1
