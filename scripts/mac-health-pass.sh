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
MAX_AGE_MIN=${MAC_HEALTH_MAX_AGE_MIN:-120}
if [ ! -f "$LOG" ]; then echo "No health check has run yet ($LOG is missing)."; exit 1; fi
now=$(date +%s); mtime=$(stat -f %m "$LOG" 2>/dev/null || stat -c %Y "$LOG")
age_min=$(( (now - mtime) / 60 ))
at=$(date -r "$mtime" '+%H:%M' 2>/dev/null || date -d "@$mtime" '+%H:%M')
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
