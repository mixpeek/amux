#!/bin/bash
# mac-cleanup-fallback.sh — run the cleanup tick when the amux scheduler has not.
#
# SCHED-465 fires scripts/mac-cleanup-tick.sh from inside the amux server, and
# the server is one of the first things to stall or restart when this Mac runs
# out of memory or disk: the rescue went down with the patient. On 2026-09-26 one
# of eight fires read "server restarted before this fire recorded a delivery
# outcome" (DESKT-58). This script is run by launchd every 15 minutes, needs no
# amux server, and does nothing unless the tick's output is older than the
# staleness window, so in the normal case it costs one stat.
#
# When it does run it uses the committed tick (origin/main, the same bytes the
# schedule runs) with the same knobs as SCHED-465, and files one board card per
# 24h saying the scheduler went quiet. If the server is down that POST fails; the
# log says so and the next fallback run retries it.
#
# Installed as ~/Library/LaunchAgents/com.amux.mac-cleanup-fallback.plist
# (StartInterval 900). Log: ~/.amux/logs/mac-cleanup-fallback.log.
set -uo pipefail
PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"
export PATH

LAST=${AMUX_CLEANUP_LAST:-$HOME/.amux/logs/mac-cleanup-tick.last}
# Longer than the schedule's own gap. SCHED-465 is hourly and writes this file
# when a run ends, so between two healthy runs it is ~50-55 minutes old; at the
# old 45 the fallback ran a duplicate tick before most scheduled fires (6 times
# on 2026-10-15, 05:00-10:00). 80 = the 60-minute interval plus a whole tick.
STALE_MIN=${AMUX_CLEANUP_FALLBACK_STALE_MIN:-80}
REPO=${AMUX_REPO_DIR:-$HOME/Dev/amux}
# Seam: the test replaces the tick with a stub so this script's decision can be
# observed without a real multi-minute cleanup run.
TICK_CMD=${AMUX_CLEANUP_FALLBACK_TICK:-}

# A tick in progress holds this lock (mac-cleanup-tick.sh, tick_lock).
LOCK="${AMUX_CLEANUP_STATE_DIR:-$HOME/.amux/logs/mac-cleanup}/tick.lock"
# Held only while its PID is alive (a SIGKILLed tick leaves the directory behind);
# with no PID recorded, fall back to the 30-minute age rule the tick uses.
lock_pid=$(cat "$LOCK/pid" 2>/dev/null)
if [ -d "$LOCK" ] && { { [ -n "$lock_pid" ] && kill -0 "$lock_pid" 2>/dev/null; } || { [ -z "$lock_pid" ] && [ -z "$(find "$LOCK" -maxdepth 0 -mmin +30 2>/dev/null)" ]; }; }; then
  echo "mac-cleanup-fallback: $(date '+%F %T') a tick is running (lock held), nothing to do"
  exit 0
fi
# FRESH IS NOT ENOUGH: IT HAS TO HAVE FINISHED. The scheduler runs the tick as a
# child of the amux server, which restarts on every deploy, and on 2026-09-26 a
# restart killed a tick after its cargo arm. Its output was 10 lines, a minute
# old, and read as "the scheduler ran" to a check that only looked at the age.
if [ -f "$LAST" ] && [ -z "$(find "$LAST" -mmin "+$STALE_MIN" 2>/dev/null)" ] && grep -q '^mac-cleanup: done' "$LAST"; then
  echo "mac-cleanup-fallback: $(date '+%F %T') scheduler ran within ${STALE_MIN}m, nothing to do"
  exit 0
fi
if [ -f "$LAST" ]; then
  age_min=$(( ( $(date +%s) - $(stat -c %Y "$LAST" 2>/dev/null || stat -f %m "$LAST") ) / 60 ))
  if grep -q '^mac-cleanup: done' "$LAST"; then why="last tick output is ${age_min}m old (over ${STALE_MIN}m)"
  else why="the last tick (${age_min}m ago) did not finish: no done line and no tick running, so it was killed"; fi
else
  why="no tick output at $LAST"
fi
echo "mac-cleanup-fallback: $(date '+%F %T') $why: the amux scheduler is not running SCHED-465, so launchd is"

if [ -z "$TICK_CMD" ]; then
  # Fetch with a short timeout: a dead network must not stop the cleanup, it only
  # means running the last-fetched committed bytes.
  perl -e 'alarm 30; exec @ARGV' git -C "$REPO" fetch -q origin main 2>/dev/null || echo "mac-cleanup-fallback: fetch failed, using the last-fetched origin/main"
  R=$(mktemp "${TMPDIR:-/tmp}/mac-cleanup-tick.XXXXXX") || exit 1
  git -C "$REPO" show origin/main:scripts/mac-cleanup-tick.sh > "$R" || { echo "mac-cleanup-fallback: could not read the committed tick"; rm -f -- "${R:?}"; exit 1; }
  TICK_CMD="bash $R"
fi
# The same knobs SCHED-465 passes (keep these two in step with its command).
AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=1000000 AMUX_CLEANUP_SNAPSHOT_RECLAIM_GB=2000 $TICK_CMD 2>&1 | tee "$LAST"
rc=${PIPESTATUS[0]}
[ -n "${R:-}" ] && rm -f -- "${R:?}"
echo "mac-cleanup-fallback: tick exited $rc"

# Tell someone the scheduler is silent. Uses the tick's own file_card, so the
# dedupe and the body shape are the same as every other card the tick files.
if [ -z "${AMUX_CLEANUP_FALLBACK_TICK:-}" ] || [ -n "${AMUX_CLEANUP_FALLBACK_LIB:-}" ]; then
  # Seam: AMUX_CLEANUP_FALLBACK_LIB names the tick to load file_card from (the
  # test's checkout copy); otherwise the committed one, as for the run itself.
  # A template with X's: GNU mktemp refuses `-t name` ("too few X's"), which left
  # L empty, file_card unloaded and $STATE_DIR unbound on Linux.
  L=$(mktemp "${TMPDIR:-/tmp}/mac-cleanup-lib.XXXXXX")
  if [ -n "${AMUX_CLEANUP_FALLBACK_LIB:-}" ]; then cp "$AMUX_CLEANUP_FALLBACK_LIB" "$L"
  else git -C "$REPO" show origin/main:scripts/mac-cleanup-tick.sh > "$L" 2>/dev/null; fi
  # shellcheck source=/dev/null
  AMUX_CLEANUP_LIB_ONLY=1 . "$L"
  rm -f -- "${L:?}"
  echo "mac-cleanup-fallback: scheduler-silent card: $(file_card "$STATE_DIR/state" "scheduler_silent" "SCHED-465 did not complete: launchd ran the Mac cleanup tick instead" "The fallback found that $why, so the amux scheduler (inside the server) is not firing SCHED-465. launchd ran the tick; its output is $LAST. Check amux server health and the scheduler.")"
fi
exit "$rc"
