#!/bin/bash
# host-responder.sh — hand a host trip to the host-health worker first, and page
# the owner only when a worker cannot help.
#
# WHY THIS EXISTS. On 2026-10-30 the memory tripwire paged Ethan URGENT for a
# reading that was wrong (swap "0 free of 0 total" with 72% of RAM free). He
# forwarded it to the host-health worker, which diagnosed and fixed it in
# minutes. Every step after the page was work a worker can do unasked, and the
# page itself was the one step that needed a human for nothing. Ethan: "this
# worker and/or amux core needs to be proactive with these things".
#
# So a trip goes to the RESPONDER first: a tmux schedule whose title starts
# with "Host tripwire responder" (found by title, so nothing has to be wired by
# id). Running it wakes that worker with the trip file below; the worker
# verifies, remedies inside the 24/7 rule, and pages only if it needs Ethan.
#
# This script still pages directly in three cases, because a worker cannot
# help in them:
#   - severity critical: jetsam or ENOSPC is minutes away, no time for a turn
#   - the responder could not be started (no schedule, run refused, no server)
#   - the same source is still tripping ESCALATE_S after the responder started
#     (the worker had its chance and the condition did not clear)
#
# Usage: host-responder.sh <source> <warn|critical> <reasons> [page message]
#        host-responder.sh clear <source>
# Prints one line ending in verdict=<...>. Exit 0 always: a watchdog must not
# fail its schedule because the pager was unhappy.
set -uo pipefail
PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"
export PATH

# A source whose condition cleared calls `host-responder.sh clear <source>`, so
# its next trip starts a fresh episode instead of escalating an old one.
if [ "${1:-}" = "clear" ]; then
  f="${AMUX_RESPONDER_DIR:-$HOME/.amux/host-trips}/${2:?source}"
  if [ -f "$f" ]; then
    mv "$f" "$f.cleared" && echo "host-responder: source=$2 verdict=cleared"
  fi
  exit 0
fi

src=${1:?source}
sev=${2:?severity}
reasons=${3:?reasons}
msg=${4:-"Host trip from $src: $reasons"}

DIR=${AMUX_RESPONDER_DIR:-$HOME/.amux/host-trips}
ESCALATE_S=${AMUX_RESPONDER_ESCALATE_S:-1800}    # 30 min for the worker to clear it
RESPOND_COOLDOWN_S=${AMUX_RESPONDER_COOLDOWN_S:-1800}
PAGE_COOLDOWN_S=${AMUX_RESPONDER_PAGE_COOLDOWN_S:-3600}
TITLE=${AMUX_RESPONDER_TITLE:-Host tripwire responder}
# Test seams: the two outward calls are commands, so a test counts real calls.
RUN_CMD=${AMUX_RESPONDER_RUN_CMD:-run_default}     # prints the HTTP status
PAGE_CMD=${AMUX_RESPONDER_PAGE_CMD:-page_default}  # exit 0 = paged

run_default() {
  local u id
  u=$(amux url 2>/dev/null) || { echo 000; return; }
  id=$(curl -sk --max-time 20 "$u/api/schedules" 2>/dev/null | python3 -c "
import json,sys
t=sys.argv[1]
try: rows=json.load(sys.stdin)
except Exception: rows=[]
ids=[s['id'] for s in rows if str(s.get('title','')).startswith(t) and not s.get('deleted')]
print(ids[0] if ids else '')" "$TITLE")
  [ -n "$id" ] || { echo none; return; }
  curl -sk --max-time 20 -o /dev/null -w '%{http_code}' -X POST \
    -H "X-Amux-Session: host-responder" "$u/api/schedules/$id/run"
}
page_default() { amux alert "$1" "$2" >/dev/null 2>&1; }

mkdir -p "$DIR"
now=${AMUX_RESPONDER_NOW:-$(date +%s)}  # injectable for tests
f="$DIR/$src"
first=$(sed -n 's/^first=//p' "$f" 2>/dev/null); first=${first:-$now}
responded=$(sed -n 's/^responded=//p' "$f" 2>/dev/null); responded=${responded:-0}
paged=$(sed -n 's/^paged=//p' "$f" 2>/dev/null); paged=${paged:-0}
# started = the FIRST successful worker start this episode. Escalation counts
# from it, so restarting the worker can never postpone the page.
started=$(sed -n 's/^started=//p' "$f" 2>/dev/null); started=${started:-0}
case "$first$responded$paged$started" in *[!0-9]*) first=$now; responded=0; paged=0; started=0 ;; esac

verdict=""
page_now=""
if [ "$sev" = "critical" ]; then
  page_now="critical"
fi

if [ -z "$page_now" ]; then
  if [ "$started" -gt 0 ] && [ $((now - started)) -ge "$ESCALATE_S" ]; then
    page_now="unresolved $(( (now - started) / 60 )) min after the worker was started"
  elif [ $((now - responded)) -ge "$RESPOND_COOLDOWN_S" ] || [ "$responded" = 0 ]; then
    code=$($RUN_CMD 2>/dev/null)
    case "$code" in
      2??) responded=$now; [ "$started" = 0 ] && started=$now; verdict="responder_started" ;;
      *) page_now="responder could not start ($code)"; verdict="responder_failed" ;;
    esac
  else
    verdict="responder_running"
  fi
fi

if [ -n "$page_now" ]; then
  if [ "$paged" = 0 ] || [ $((now - paged)) -ge "$PAGE_COOLDOWN_S" ]; then
    if $PAGE_CMD "$msg" "$src: $page_now" ; then
      paged=$now; verdict="${verdict:+$verdict+}paged_owner"
    else
      verdict="${verdict:+$verdict+}page_failed"
    fi
  else
    verdict="${verdict:+$verdict+}page_cooldown"
  fi
fi

# The trip file is what the worker reads: what tripped, how badly, since when.
{
  echo "source=$src"
  echo "severity=$sev"
  echo "reasons=$reasons"
  echo "message=$msg"
  echo "first=$first"
  echo "last=$now"
  echo "responded=$responded"
  echo "started=$started"
  echo "paged=$paged"
  echo "verdict=$verdict"
} > "$f.tmp" && mv "$f.tmp" "$f"

echo "host-responder: source=$src severity=$sev escalation=${page_now:-none} verdict=$verdict"
exit 0
