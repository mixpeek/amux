#!/usr/bin/env bash
# test-mac-cleanup-deadline.sh: the cleanup tick keeps inside one deadline.
#
# 2026-10-14: the scheduler killed six ticks in a row at 600s under load 133,
# each before the verdicts ran, because the per-arm budgets alone add up past
# 600s. tick_arm shrinks each arm's budget to what the tick has left before its
# verdict reserve, and skips (and names) an arm with nothing left.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d "${TMPDIR:-/tmp}/dl.XXXXXX")
trap 'rm -rf -- "${FIX:?}"' EXIT
fails=0
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }
has() { printf '%s' "$1" | grep -qE -- "$2" && echo yes || echo no; }

export AMUX_CLEANUP_SCOPE_FILE=/dev/null AMUX_CLEANUP_HEALTH_PROBES=off
echo "1. tick_arm shrinks a budget to what is left, and skips an arm once nothing is"
got=$(TICK_PATH="$TICK" bash -c '
  AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"
  TICK_BUDGET_S=100; TICK_RESERVE_S=20; SECONDS=50; BIG=200; SMALL=10
  tick_arm "first arm" BIG SMALL; rc1=$?
  SECONDS=90; out=$(tick_arm "late arm" BIG); rc2=$?
  tick_arm "late arm" BIG >/dev/null
  printf "%s|%s|%s|%s|%s|%s" "$rc1" "$BIG" "$SMALL" "$rc2" "$TICK_SKIPPED" "$out"')
IFS='|' read -r rc1 big small rc2 skipped out <<<"$got"
check "an arm with time left runs"                    0   "$rc1"
check "its budget is cut to the time left"            yes "$([ "$big" -le 30 ] && [ "$big" -ge 28 ] && echo yes || echo no)"
check "a budget already inside the time left stays"   10  "$small"
check "an arm past the reserve does not run"          1   "$rc2"
check "the skipped arm is recorded by name"           late_arm "$skipped"
check "and the skip is said, with its verdict"        yes "$(has "$out" 'late arm skipped: .*verdict=tick_out_of_time')"

echo "1a. under load the last budgeted arm still runs (Mac self RCA 20261020-005306)"
# Replays the 23:54 tick of 2026-10-19: 25s before the first arm, every budgeted
# arm spending its whole cap, and the unbudgeted ones their measured seconds. A
# cap of "whatever is left" let the first arms take it all and skipped worktrees.
got=$(TICK_PATH="$TICK" bash -c '
  AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH" >/dev/null
  SECONDS=25; t=25
  spend() { t=$(( t + $1 )); SECONDS=$t; }
  tick_arm "cargo targets" TARGET_SCAN_S TARGET_BUDGET_S >/dev/null && spend $(( TARGET_SCAN_S + TARGET_BUDGET_S ))
  c=$TICK_ARM_SHARE
  tick_arm "superseded artifacts" TARGET_SCAN_S TARGET_BUDGET_S >/dev/null && spend $(( TARGET_SCAN_S + TARGET_BUDGET_S ))
  tick_arm "vm images" >/dev/null && spend 2
  tick_arm "vm trim" >/dev/null && spend 70
  tick_arm "claude tmp" >/dev/null && spend "$TICK_ARM_SHARE"
  ct=$TICK_ARM_SHARE
  tick_arm "user tmp" >/dev/null && spend 4
  tick_arm "lane tmp" LANE_TMP_BUDGET_S >/dev/null && spend "$LANE_TMP_BUDGET_S"
  out=$(tick_arm "worktrees" WORKTREE_BUDGET_S); rc=$?
  tick_arm "worktrees" WORKTREE_BUDGET_S >/dev/null
  printf "%s|%s|%s|%s|%s|%s" "$rc" "${TICK_SKIPPED:-none}" "$WORKTREE_BUDGET_S" "$c" "$t" "$ct"')
IFS='|' read -r rc skipped wt cargo used ct <<<"$got"
check "the last budgeted arm runs"                    0    "$rc"
check "no arm is skipped"                             none "$skipped"
check "it keeps a working budget (its runs took 3-13s)" yes "$([ "$wt" -ge 8 ] && echo yes || echo no)"
check "the first arm keeps its measured 54-95s"     yes "$([ "$cargo" -ge 70 ] && echo yes || echo no)"
check "claude tmp is not squeezed below the arms after it" yes "$([ "$ct" -ge 45 ] && echo yes || echo no)"
check "and the arms still end inside the verdict reserve" yes "$([ "$used" -le 330 ] && echo yes || echo no)"
out=$(TICK_PATH="$TICK" bash -c 'AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH" >/dev/null; SECONDS=25; TARGET_SCAN_S=90; tick_arm "cargo targets" TARGET_SCAN_S TARGET_BUDGET_S')
check "a binding share is said, with its verdict"     yes  "$(has "$out" 'cargo targets capped at [0-9]+s of the 305s left.*verdict=tick_arm_share')"

echo "1b. the deadline sits under the warning, and the done line names the slowest section"
got=$(TICK_PATH="$TICK" bash -c '
  AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"
  printf "%s|%s" "$TICK_BUDGET_S" "$TICK_WARN_S"')
IFS='|' read -r budget warn <<<"$got"
check "the default deadline is under the warning"      yes "$([ "$budget" -lt "$warn" ] && echo yes || echo no)"
got=$(TICK_PATH="$TICK" AMUX_CLEANUP_TICK_BUDGET_S=540 bash -c 'AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH" >/dev/null; echo "$TICK_BUDGET_S"')
check "a deadline at or over the warning is clamped"   450 "$got"
out=$(TICK_PATH="$TICK" AMUX_CLEANUP_TICK_BUDGET_S=540 bash -c 'AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"' | head -1)
check "and the clamp says so"                         yes "$(has "$out" 'verdict=tick_budget_clamped')"
got=$(TICK_PATH="$TICK" bash -c '
  AMUX_CLEANUP_LIB_ONLY=1 . "$TICK_PATH"
  SECONDS=0;  tick_mark "cargo targets"
  SECONDS=40; tick_mark "claude tmp"
  SECONDS=55; tick_mark verdicts
  SECONDS=200; tick_mark end
  echo "$TICK_SLOWEST ${TICK_SECTIONS:-}"')
check "the slowest section is named with its seconds"  verdicts:145s "${got%% *}"
check "every timed section is listed for the done line" "cargo_targets:40,claude_tmp:15,verdicts:145" "${got#* }"

echo "2. a tick out of time still reaches its verdicts and names what it skipped (macOS only)"
if [ "$(uname)" = Darwin ]; then
  run() { AMUX_CLEANUP_HEALTH_PS_CMD=true AMUX_CLEANUP_ORPHAN_PS_CMD=true AMUX_CLEANUP_SQLITE_PS_CMD=true \
        AMUX_CLEANUP_FSEVENTSD_CMD=true AMUX_CLEANUP_STATE_DIR="$FIX/tick" AMUX_CLEANUP_TARGET_ROOTS="$FIX/none" \
        AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_CLAUDE_TMP_ROOT="$FIX/none" AMUX_CLEANUP_LANE_TMP_ROOT="$FIX/none" \
        AMUX_CLEANUP_USER_TMP=0 AMUX_CLEANUP_WORKTREE_ROOTS="$FIX/none" AMUX_CLEANUP_CHURN_ROOTS="$FIX/none" \
        AMUX_CLEANUP_VM_LIST_CMD=true AMUX_CLEANUP_VM_IMAGE_PRUNE_CMD=true AMUX_CLEANUP_SESSIONS_CMD=false \
        AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 AMUX_CLEANUP_HISTORY_CMD=true AMUX_CLEANUP_CARD_CMD=true \
        AMUX_CLEANUP_RECLAIM_TRIGGER_CMD="${RECLAIM_TRIGGER_STUB:-echo 204}" AMUX_CLEANUP_DEMOTE_PS_CMD=true AMUX_CLEANUP_ESCALATE_CMD=true "$@" bash "$TICK" --dry-run 2>&1; }
  out=$(run env AMUX_CLEANUP_TICK_BUDGET_S=1 AMUX_CLEANUP_TICK_RESERVE_S=1); rc=$?
  check "the tick exits cleanly"                      0   "$rc"
  for arm in "cargo targets" "lane tmp" "worktrees" "claude tmp"; do
    check "the $arm arm is skipped and says so"       yes "$(has "$out" "mac-cleanup: $arm skipped: ")"
  done
  check "the verdicts still run"                      yes "$(has "$out" 'mac-cleanup: assess disk=')"
  check "the done line names the skipped arms"        yes "$(has "$out" 'skipped=cargo_targets,.*claude_tmp,.*lane_tmp,worktrees')"
  # Session scratch is reached before the cursor-resumable lane tmp (Mac self RCA 20261019-175342).
  check "claude tmp is reached before user and lane tmp" yes "$(printf '%s\n' "$out" | grep -o 'mac-cleanup: [a-z ]* skipped: ' | tr '\n' '|' | grep -q 'claude tmp skipped: |.*lane tmp skipped: ' && echo yes || echo no)"
  out=$(run env); rc=$?
  check "with the default budget nothing is skipped"  yes "$(has "$out" ' skipped=none ')"
  check "and no arm says it ran out of time"          no  "$(has "$out" 'verdict=tick_out_of_time')"
else
  echo "  skip macOS-only cell (the tick's other probes are macOS commands): not run on $(uname)"
fi

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-deadline — all checks passed"; exit 0; fi
echo "mac-cleanup-deadline: $fails check(s) FAILED"; exit 1
