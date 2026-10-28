#!/bin/bash
# mac-health-pass.sh - the pass check of the hourly Mac system health check
# (SCHED-465). Exit 0 when the last tick ran to the end recently; the first
# line says, in plain words, what it found, because the schedule's detail page
# shows that line (Ethan, 2026-10-28: pass/fail "for the mac one in an
# intuitive way").
#
# Pass means the tick did its job: it finished within MAC_HEALTH_MAX_AGE_MIN.
# It does not mean the Mac is under every floor: a constraint whose cause
# belongs to another lane or to the owner is escalated by the tick itself, and
# re-running a cleanup cannot fix it, so it is reported here and not failed on.
set -uo pipefail
LOG=${MAC_HEALTH_LOG:-$HOME/.amux/logs/mac-cleanup-tick.last}
LOCK=${MAC_HEALTH_LOCK:-$HOME/.amux/logs/mac-cleanup/tick.lock}
MAX_AGE_MIN=${MAC_HEALTH_MAX_AGE_MIN:-120}
if [ ! -f "$LOG" ]; then echo "No health check has run yet ($LOG is missing)."; exit 1; fi
# GNU forms first: on Linux `stat -f` is --file-system and prints a block of
# text before failing, which broke the arithmetic below on CI (run
# 40694973070); on macOS the GNU flags fail with no output, so the BSD form runs.
now=$(date +%s); mtime=$(stat -c %Y "$LOG" 2>/dev/null || stat -f %m "$LOG")
case "$mtime" in ''|*[!0-9]*) echo "Cannot read when $LOG was written."; exit 1 ;; esac
age_min=$(( (now - mtime) / 60 ))
at=$(date -d "@$mtime" '+%H:%M' 2>/dev/null || date -r "$mtime" '+%H:%M')
# A tick writes its log as it goes, so mid-run there is no done line yet. A
# run in progress (its lock's pid alive) is not a failure; a killed run
# leaves no live pid and fails below at the next check.
lock_pid=$(cat "$LOCK/pid" 2>/dev/null || true)
if [ -n "$lock_pid" ] && kill -0 "$lock_pid" 2>/dev/null; then
  echo "A health check is running now (started $at); its result is read when it finishes."
  exit 0
fi
done_line=$(grep -a '^mac-cleanup: done ' "$LOG" | tail -1)
if [ -z "$done_line" ]; then
  echo "The check at $at did not finish (no 'mac-cleanup: done' line); it is run again."
  tail -3 "$LOG"; exit 1
fi
if [ "$age_min" -gt "$MAX_AGE_MIN" ]; then
  echo "The last finished check was at $at, ${age_min}m ago (over ${MAX_AGE_MIN}m); it is run again."
  exit 1
fi
over=$(grep -a '^mac-cleanup: constraint ' "$LOG" | sed -E 's/^mac-cleanup: constraint //; s/ — .*$//; s/ (escalated|reported).*$//' | paste -sd ';' - | sed 's/;/; /g')
if [ -z "$over" ]; then
  echo "Ran $at: all clear."
else
  echo "Ran $at and cleaned up. Still over a floor, already escalated to its owner: $over"
fi
echo "$done_line"
exit 0
