#!/usr/bin/env bash
# test-mac-cleanup-utilization.sh: the cleanup tick says who is using the CPU and
# lowers the priority of detached process trees when the machine is busy.
#
# 2026-10-17, load 87 on 28 cores: the tick escalated "cpu 15-min load" with no
# name attached, and two `amux land` trees and a pytest whose shells had exited
# ran at full priority because host_guard only renices processes under a
# worker's pane. Every input here is a fixture: the /api/resources payload, the
# ps snapshot, and renice (which records instead of acting).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d "${TMPDIR:-/tmp}/util.XXXXXX")
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { printf '%s' "$1" | grep -qE -- "$2" && echo yes || echo no; }
ME=$(id -u)

payload() { # <cpu_total> <capacity>
  cat > "$FIX/res.json" <<JSON
{"measured":true,"top_by_cpu":[{"name":"homepage","cpu_percent":310,"rss_mb":5000,"n_processes":7},{"name":"gs12","cpu_percent":190,"rss_mb":4000,"n_processes":148},{"name":"idle","cpu_percent":2,"rss_mb":100,"n_processes":2}],
 "machine":{"measured":true,"capacity_percent":$2,"cpu_percent_total":$1,"workers_cpu_percent":502,"unattributed_cpu_percent":700,
  "by_kind":{"vm":250,"detached":330,"apps":60,"system":40,"amux_server":20},
  "top_unattributed":[{"kind":"vm","root_pid":400,"command":"/System/x/com.apple.Virtualization.VirtualMachine","cpu_percent":250,"rss_mb":8000,"n_processes":1},
   {"kind":"detached","root_pid":500,"command":"bash","cpu_percent":150,"rss_mb":200,"n_processes":3},
   {"kind":"detached","root_pid":600,"command":"/usr/local/bin/tmux","cpu_percent":120,"rss_mb":20,"n_processes":1},
   {"kind":"detached","root_pid":700,"command":"bash","cpu_percent":110,"rss_mb":50,"n_processes":2},
   {"kind":"detached","root_pid":800,"command":"python3","cpu_percent":40,"rss_mb":50,"n_processes":1}]}}
JSON
}
# pid ppid uid nice args. 500 is a land tree (bash > python > python, one
# already low); 600 tmux; 700 a gs12 rehearsal; 800 under the floor; 900 a
# process of another user inside tree 500.
cat > "$FIX/ps" <<EOF2
  500     1 $ME  0 bash /Users/x/.local/bin/amux land --sha abc
  501   500 $ME  0 /usr/local/bin/python3 -m pytest -q
  502   501 $ME 10 /usr/local/bin/python3 worker
  503   501    0  0 /usr/sbin/helper
  600     1 $ME  0 /usr/local/bin/tmux -L amux new-session
  700     1 $ME  0 bash /Users/x/.amux/tmp/mxp-gs12/zb/mvs_rehearsal.sh
  800     1 $ME  0 python3 backfill.py
  999     1 $ME  0 unrelated
EOF2
cat > "$FIX/renice" <<EOF2
#!/bin/bash
echo "\$*" >> "$FIX/renice.calls"
EOF2
chmod +x "$FIX/renice"
probe() { # [dry]
  rm -f "$FIX/renice.calls"
  TICK_PATH="$TICK" FIXD="$FIX" PROBE_DRY="${1:-0}" bash -c '
    AMUX_CLEANUP_LIB_ONLY=1 AMUX_CLEANUP_SCOPE_FILE=/dev/null . "$TICK_PATH"
    STATE_DIR="$FIXD/state" UTIL_CMD="cat $FIXD/res.json" UTIL_TREE_PS_CMD="cat $FIXD/ps" RENICE_CMD="$FIXD/renice" UTIL_IDLE_CMD="cat $FIXD/top"
    check_utilization "$PROBE_DRY"; echo "SUMMARY=$UTIL_SUMMARY"; echo "RENICED=$UTIL_RENICED"' 2>&1; }
idle_top() { echo "CPU usage: 1.0% user, 1.0% sys, $1% idle " > "$FIX/top"; }
idle_top 98
calls() { cat "$FIX/renice.calls" 2>/dev/null | tr '\n' ' ' | sed 's/ $//'; }

echo "1. busy: it names the consumers and lowers detached trees, nothing else"
payload 2700 2800; out=$(probe)
check "the split is printed"                          yes "$(has "$out" 'utilization: 2700% of 2800% busy, workers 502%, everything else 700%')"
check "the summary names the heaviest workers"        yes "$(has "$out" 'SUMMARY=workers homepage 310%, gs12 190%; rest detached 330%, vm 250%, apps 60%')"
check "a worker under 50% is not named"               no  "$(has "$out" 'idle 2%')"
check "the land tree's own, not-yet-low pids are lowered" "-n 10 -p 500 -n 10 -p 501" "$(calls)"
check "and it says so with a verdict"                 yes "$(has "$out" 'pid 500 \(bash\) at 150%: lowered 2 process\(es\) to nice 10 \(verdict=util_detached_reniced\)')"
check "tmux is excluded: new panes would inherit it"  yes "$(has "$out" 'pid 600 \(tmux\) at 120%: excluded, not touched')"
check "gs12 is excluded"                              yes "$(has "$out" 'pid 700 \(bash\) at 110%: excluded, not touched')"
check "a tree under the floor is not listed"          no  "$(has "$out" 'pid 800')"
check "the VM is never reniced"                       no  "$(calls | grep -q ' 400' && echo yes || echo no)"
check "the count reaches the done line"               yes "$(has "$out" 'RENICED=2')"
check "the action is logged"                          yes "$(grep -q $'\treniced\t500\t150%\t2 procs' "$FIX/state/util-renice.log" && echo yes || echo no)"

echo "2. not busy: it reports and touches nothing"
payload 1200 2800; out=$(probe)
check "the tree is reported"                          yes "$(has "$out" 'pid 500 \(bash\) at 150%: reported \(CPU under 0.75 busy\)')"
check "no renice ran"                                 "" "$(calls)"

echo "3. a dry run says what it would do"
payload 2700 2800; out=$(probe 1)
check "would lower"                                   yes "$(has "$out" 'pid 500 \(bash\) at 150%: would lower 2 process\(es\) to nice 10 \(dry run\)')"
check "and lowers nothing"                            "" "$(calls)"

echo "4. an unreadable census judges nothing and says so"
echo 'not json' > "$FIX/res.json"; out=$(probe)
check "unmeasured, with a verdict"                    yes "$(has "$out" 'utilization: unmeasured: the server did not answer \(verdict=util_unmeasured\)')"
check "no summary to attach"                          yes "$(has "$out" '^SUMMARY=$')"
echo '{"machine":{"measured":false,"why_unmeasured":"ps did not run"}}' > "$FIX/res.json"; out=$(probe)
check "the server's own reason is carried"            yes "$(has "$out" 'unmeasured: ps did not run')"

echo "5. the cpu constraint carries the names into the escalation"
out=$(UTIL_SUMMARY="workers homepage 310%; rest vm 250%" bash -c '
  verdicts=$(printf "cpu 15-min load 82.0 is 293%% of 28 cores (over 150%%)\nmemory x\n")
  if [ -n "${UTIL_SUMMARY:-}" ]; then '"$(grep -F 'verdicts=$(printf '"'"'%s\n'"'"' "$verdicts" | U=' "$TICK" | sed 's/^if \[ -n "\${UTIL_SUMMARY:-}" \]; then //; s/; fi$//')"'; fi
  printf "%s\n" "$verdicts"')
check "the cpu line names who"                        yes "$(has "$out" '^cpu 15-min load 82.0 is 293% of 28 cores \(over 150%\) \(who: workers homepage 310%; rest vm 250%\)$')"
check "other lines are untouched"                     yes "$(has "$out" '^memory x$')"

echo "7. the census misses short-lived children: top's busier reading decides"
# RCA 20261024-170152: one ffmpeg after another, census 71% busy, top 88%.
payload 1200 2800; idle_top 12; out=$(probe)
check "it says top was used, with a verdict"          yes "$(has "$out" 'top reads the CPU 88% busy, above the census; judging busy by top \(verdict=util_census_under_top\)')"
check "and the tree is lowered"                       "-n 10 -p 500 -n 10 -p 501" "$(calls)"
idle_top 60; out=$(probe)
check "top idle too: still only reported"             yes "$(has "$out" 'pid 500 \(bash\) at 150%: reported')"
check "no top line when top is not busier"            no  "$(has "$out" 'util_census_under_top')"
# The live reading that still escalated after the first fix: top 79% busy,
# over the constraint's 75% bar and under the arm's old 0.85.
idle_top 21; out=$(probe)
check "79% busy is busy: the arm's bar matches the constraint's" "-n 10 -p 500 -n 10 -p 501" "$(calls)"
echo "garbage" > "$FIX/top"; out=$(probe)
check "an unreadable top falls back to the census"    yes "$(has "$out" 'pid 500 \(bash\) at 150%: reported')"
idle_top 98

echo "6. system_health runs it"
check "wired between the volume and self checks"      yes "$(grep -q 'check_agent_volume; check_utilization "\$1"; demote_background_daemons "\$1"; check_self_runs' "$TICK" && echo yes || echo no)"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-utilization — all checks passed"; exit 0; fi
echo "mac-cleanup-utilization: $fails check(s) FAILED"; exit 1
