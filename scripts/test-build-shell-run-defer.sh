#!/usr/bin/env bash
# The builder holds a deploy while a shell schedule run is in flight (RCA self
# 20261015-182512: 4 of SCHED-465's 25 runs were killed by restarts). Runs the
# shipped shell_runs_in_flight against canned /api/schedules/runs payloads.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
PASS=0; FAIL=0
expect() { if "$@"; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); echo "FAIL: $*"; fi; }
awk '/^shell_runs_in_flight\(\) \{/,/^}$/' "$ROOT/scripts/rust-auto-build.sh" > "$TMP/fn.sh"
expect test -s "$TMP/fn.sh"
now=$(date +%s)
runs() { printf '%s' "$1" > "$TMP/runs.json"; AMUX_RS_RUNS_URL="file://$TMP/runs.json" bash -c "server_api_base() { echo x; }; source '$TMP/fn.sh'; shell_runs_in_flight 900"; }
row() { printf '{"schedule_id":"%s","status":"%s","delivery":%s,"ran_at":%s}' "$1" "$2" "$3" "$4"; }
one="[$(row SCHED-465 running '"shell"' $((now-60)))]"
got=$(runs "$one")
expect test "$got" = "SCHED-465"
# Finished runs and stale running rows do not hold.
# A cron-fired run is delivery=null until it ends: it holds too (the first
# version matched "shell" only and never held a scheduled run).
cron="[$(row SCHED-465 running null $((now-120)))]"
got=$(runs "$cron")
expect test "$got" = "SCHED-465"
others="[$(row SCHED-706 ok null $((now-10))),$(row SCHED-1 ok '"shell"' $((now-10))),$(row SCHED-2 running '"shell"' $((now-5000))),$(row SCHED-3 running null $((now-5000)))]"
got=$(runs "$others")
expect test -z "$got"
# An unreadable scheduler never holds a deploy.
got=$(runs 'not json')
expect test -z "$got"
# The hold itself is wired into the build path with its verdicts.
expect grep -q 'in_flight=$(shell_runs_in_flight "$SHELL_DEFER_MAX_S")' "$ROOT/scripts/rust-auto-build.sh"
expect grep -q 'verdict=activation_deferred_shell_run' "$ROOT/scripts/rust-auto-build.sh"
expect grep -q 'verdict=activation_shell_defer_expired' "$ROOT/scripts/rust-auto-build.sh"
echo "test-build-shell-run-defer: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
