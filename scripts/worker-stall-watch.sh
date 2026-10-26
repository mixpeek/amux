#!/usr/bin/env bash
# worker-stall-watch.sh: notice when an orchestrating worker has stopped acting
# while work is still arriving for it, and do something about it.
#
# Why (2026-10-24/25): amux-helper asked the owner a question with Claude Code's
# AskUserQuestion tool, which blocks the whole turn until answered. The owner was
# asleep, so from ~04:2xZ to 12:15Z peer messages queued unread, its ScheduleWakeup
# chain could not fire (the turn never ended), and nothing steered while the
# TubeScience chain failed three times and aborted. Nothing in amux noticed.
#
# Detection is by symptom, so it covers every cause (a blocking prompt, a turn that
# ended without re-arming, a usage limit): the worker's last_progress_at is older
# than STALL_MIN minutes AND it has inbound messages newer than that progress, OR
# the oldest message in its steering_queue has waited longer than QUEUE_MIN
# minutes (the 2026-10-25 case: 37 messages queued from 05:31Z to ~12:23Z).
#   - turn idle    -> send it a wake message (it will read its queue).
#   - turn running -> it is blocked inside a turn; a message cannot reach it, so
#                     alert the owner, once per stall.
# Every check writes one line to ~/.amux/logs/worker-stall-watch.log; a stall also
# writes a WARN line, so a log sweep finds it.
#
# Usage: worker-stall-watch.sh <worker> [<worker>...]
# Env:   STALL_MIN (default 20), STALL_FIXTURE_SESSION / STALL_FIXTURE_HISTORY /
#        STALL_FIXTURE_OUTBOUND (JSON files used instead of the API, for tests),
#        STALL_DRY_RUN=1 (decide only), STALL_CONFIRM_SLEEP (seconds between
#        delivery re-checks, default 10).
#
# Progress is the NEWER of last_progress_at and the worker's own latest message to
# another session. For an isolated worker last_progress_at lags its turns by
# minutes, so on 2026-10-26 13:47Z a worker that had replied to a peer 23 s
# earlier read as idle for 21 min and the owner was paged. A message the worker
# sent is direct evidence it acted.
set -uo pipefail
STALL_MIN="${STALL_MIN:-20}"
QUEUE_MIN="${QUEUE_MIN:-15}"   # a steering-queue message older than this is a stall on its own
LOG="${STALL_LOG:-$HOME/.amux/logs/worker-stall-watch.log}"
STATE_DIR="${STALL_STATE_DIR:-$HOME/.amux/tmp/worker-stall-watch}"
mkdir -p "$(dirname "$LOG")" "$STATE_DIR"
U="$(amux url 2>/dev/null)"
now=$(date -u +%s)
ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
rc=0

# send_wake <worker> <text> <max-time>: prints the HTTP code, or <code>-not-ok when
# a 2xx body lacks the server's ok:true.
send_wake() {
  local body code
  body=$(python3 -c 'import json,sys;print(json.dumps({"text":sys.argv[1]}))' "$2")
  code=$(curl -sk --max-time "$3" -o "$STATE_DIR/$1.wake.out" -w '%{http_code}' -X POST \
    -H 'Content-Type: application/json' -H "X-Amux-Session: ${STALL_WAKE_ORIGIN:-${AMUX_SESSION:-}}" \
    -d "$body" "$U/api/sessions/$1/send") || code=000
  case "$code" in 2??) grep -q '"ok":true' "$STATE_DIR/$1.wake.out" 2>/dev/null || code="${code}-not-ok" ;; esac
  echo "$code"
}

# wake_delivered <worker> <since-epoch>: true when the worker's history holds a
# watchdog wake recorded at or after <since>, with 5 s of clock slack.
wake_delivered() {
  local h
  h=$(curl -sk --max-time 15 "$U/api/history?session=$1&limit=10") || return 1
  python3 -c '
import json,sys
try: rows=json.loads(sys.argv[1])
except Exception: sys.exit(1)
since=float(sys.argv[2])-5
ok=any(str(r.get("text","")).startswith("Stall watchdog:") and int(r.get("ts") or 0)/1000>=since for r in (rows if isinstance(rows,list) else []))
sys.exit(0 if ok else 1)' "$h" "$2"
}

for w in "$@"; do
  if [ -n "${STALL_FIXTURE_SESSION:-}" ]; then
    sess=$(cat "$STALL_FIXTURE_SESSION"); hist=$(cat "$STALL_FIXTURE_HISTORY")
  else
    sess=$(curl -sk --max-time 15 "$U/api/sessions/$w") || sess=""
    hist=$(curl -sk --max-time 15 "$U/api/history?session=$w&limit=50") || hist=""
  fi
  if [ -n "${STALL_FIXTURE_OUTBOUND:-}" ]; then
    outb=$(cat "$STALL_FIXTURE_OUTBOUND")
  elif [ -n "${STALL_FIXTURE_SESSION:-}" ]; then
    outb="[]"
  else
    outb=$(curl -sk --max-time 15 "$U/api/history?sender=$w&limit=20") || outb="[]"
  fi
  verdict=$(python3 -c '
import json,sys
w,now,stall=sys.argv[1],float(sys.argv[2]),float(sys.argv[3])*60
try:
    s=json.loads(sys.argv[4]); h=json.loads(sys.argv[5])
except Exception as e:
    print("unmeasured unreadable_api", e.__class__.__name__); sys.exit()
if not isinstance(s,dict) or "name" not in s:
    print("unmeasured no_session"); sys.exit()
prog=float(s.get("last_progress_at") or s.get("last_activity") or 0)
try: ob=json.loads(sys.argv[7])
except Exception: ob=[]
# sends by the worker to OTHER sessions are progress; a send to itself is a relay into it
sent=[int(r["ts"])/1000 for r in (ob if isinstance(ob,list) else []) if r.get("ts") and r.get("sender")==w and r.get("session")!=w]
prog=max([prog]+sent)
turn=s.get("turn_state") or ""
qmax=float(sys.argv[6])*60
inbound=[int(r["ts"])/1000 for r in (h if isinstance(h,list) else []) if r.get("ts")]
q=s.get("steering_queue") or []
qts=[float(x.get("queued_at")) for x in q if isinstance(x,dict) and x.get("queued_at")]
qts=[t/1000 if t>1e11 else t for t in qts]
qage=(now-min(qts)) if qts else 0
unread=sum(1 for t in inbound if t>prog)+len(q)
age=now-prog
if prog==0 and not qts:
    print("unmeasured no_progress_field"); sys.exit()
why=[]
if age>stall and unread>0: why.append("no_progress")
if qts and qage>qmax: why.append("queue_age_%dmin" % int(qage//60))
print("STALLED" if why else "ok", turn or "unknown", int(age//60), unread, ",".join(why) or "-")
' "$w" "$now" "$STALL_MIN" "$sess" "$hist" "$QUEUE_MIN" "$outb")
  read -r state turn age_min unread why <<<"$verdict"
  echo "$ts worker=$w verdict=$state turn=$turn idle_min=${age_min:-?} unread=${unread:-?} why=${why:--} stall_min=$STALL_MIN queue_min=$QUEUE_MIN" >> "$LOG"
  stamp="$STATE_DIR/$w.alerted"
  if [ "$state" = STALLED ]; then
    rc=1
    echo "$ts WARN worker=$w STALLED ($why) turn=$turn no progress for ${age_min} min with $unread unread or queued message(s)" >> "$LOG"
    if [ "$turn" = running ]; then
      # Blocked inside a turn (a prompt, a question): only the owner can unblock it.
      if [ ! -f "$stamp" ]; then
        [ "${STALL_DRY_RUN:-}" = 1 ] || amux alert "Worker $w is stuck inside a turn: no progress for ${age_min} min while $unread message(s) wait. It is probably blocked on a prompt or question in its pane; answer or dismiss it." "orchestrator stalled" >/dev/null 2>&1
        echo "$ts worker=$w action=owner_alert" >> "$LOG"; date -u +%s > "$stamp"
      fi
    else
      # Wake through the API with an explicit origin. A plain `amux send` from a
      # scheduler shell carries no session identity, and the server refuses it for
      # an isolated target ("amux automation is not delivered into it"), so on
      # 2026-10-26 a shell schedule's wakes failed silently. STALL_WAKE_ORIGIN must
      # be a session the target accepts (a paired peer); the result is checked.
      ORIGIN="${STALL_WAKE_ORIGIN:-${AMUX_SESSION:-}}"
      WAKE_TEXT="Stall watchdog: no progress for ${age_min} min and $unread unread message(s). Read your queue and act on whatever is blocked; re-arm your wake."
      code=000; wverdict=wake_sent
      if [ "${STALL_DRY_RUN:-}" = 1 ]; then
        code=dry
      else
        sent_at=$(date -u +%s)
        code=$(send_wake "$w" "$WAKE_TEXT" 15)
        case "$code" in 2??) ;; *)
          # A slow or dropped HTTP reply does not mean the wake was lost: a send
          # to an isolated pane can confirm after curl's deadline (2026-10-26
          # 13:47Z: http 000, yet the wake was in the worker's history, and the
          # owner was paged). Look for it, retry once, and only then call it failed.
          echo "$ts WARN worker=$w verdict=wake_unconfirmed http=$code origin=${ORIGIN:-none}" >> "$LOG"
          if wake_delivered "$w" "$sent_at"; then wverdict=wake_confirmed_late
          else
            sleep "${STALL_CONFIRM_SLEEP:-10}"
            if wake_delivered "$w" "$sent_at"; then wverdict=wake_confirmed_late
            else
              code=$(send_wake "$w" "$WAKE_TEXT" 45)
              case "$code" in 2??) wverdict=wake_sent_on_retry ;; *)
                sleep "${STALL_CONFIRM_SLEEP:-10}"
                if wake_delivered "$w" "$sent_at"; then wverdict=wake_confirmed_late; else wverdict=wake_failed; fi ;;
              esac
            fi
          fi ;;
        esac
      fi
      case "$wverdict" in
        wake_failed)
          echo "$ts WARN worker=$w action=wake_failed verdict=wake_failed http=$code origin=${ORIGIN:-none} body=$(head -c 200 "$STATE_DIR/$w.wake.out" 2>/dev/null | tr '\n' ' ')" >> "$LOG"
          rc=2
          # Only a wake still absent from the worker's history after a retry
          # reaches the owner, once per stall.
          if [ ! -f "$stamp" ]; then
            amux alert "Worker $w is idle and stalled (${age_min} min, $unread unread) and the watchdog could not deliver a wake after a retry (http $code, not in its history)." "stall watchdog wake failed" >/dev/null 2>&1
            echo "$ts worker=$w action=owner_alert reason=wake_failed" >> "$LOG"; date -u +%s > "$stamp"
          fi ;;
        *) echo "$ts worker=$w action=wake_sent verdict=$wverdict http=$code origin=${ORIGIN:-none}" >> "$LOG" ;;
      esac
    fi
  else
    rm -f "$stamp"   # recovered or healthy: the next stall alerts again
  fi
done
exit $rc
