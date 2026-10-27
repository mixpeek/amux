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
#                     climb the recovery ladder below. The owner is the last rung.
# Every check writes one line to ~/.amux/logs/worker-stall-watch.log; a stall also
# writes a WARN line, so a log sweep finds it.
#
# RECOVERY LADDER (Ethan 2026-10-27, MSG-98201/MSG-98259: "i don't want a human in
# the loop"). Paging the owner at once left a blocked worker blocked until he woke.
# Now a stalled running turn climbs one rung per run (the schedule fires every 5
# min), and a rung is verified before the next one fires:
#   answer     the pane shows one of the two prompts amux answers by policy (the
#              rate-limit menu, AMUX_RATE_LIMIT_ACTION; the resume-mode selector,
#              AMUX_RESUME_MODE_ACTION), so press the server's own key for it.
#              Verified at once by re-reading the pane, then by progress.
#   interrupt  press Escape through POST /api/sessions/<w>/keys (the turn ends,
#              queued messages deliver at the boundary).
#   restart    POST /stop, wait until it is not running, POST /start: start
#              resumes the stored conversation, which is how amux keeps context.
#              One per stall, and at most one per worker per
#              STALL_RESTART_COOLDOWN_H hours.
#   page       amux alert, once per stall.
# A rung that fired waits STALL_VERIFY_MIN minutes for progress. Progress newer
# than the rung logs verdict=ladder_<rung> result=verified and ends the stall. No
# progress logs WARN verdict=ladder_failed and the next rung fires. Each rung is
# skippable: STALL_RUNG_ANSWER / STALL_RUNG_INTERRUPT / STALL_RUNG_RESTART = 0.
#
# ISOLATED WORKERS. amux does not press keys into, or restart, an isolated worker
# (CLAUDE.md "Isolated workers"; the server's own prompt sweep skips them,
# AMUX-3232). The only automation that reaches one is an owner-configured
# exception, so rungs answer, interrupt and restart are skipped for an isolated
# worker (logged reason=isolated) unless the schedule sets STALL_LADDER_ISOLATED=1,
# which is the owner opting that schedule in. Without it an isolated worker gets
# the page, as before.
#
# SUBAGENT AGE. Every run also reads the worker's running subagents from Claude
# Code's transcript files with the boundary guard's own reader
# (scripts/hooks/boundary-guard.py count_running_subagents), so the guard and this
# watch can never disagree about what is running. A subagent silent for more than
# STALL_SUBAGENT_IDLE_MIN (60) or running for more than STALL_SUBAGENT_MAX_H (4)
# logs WARN verdict=subagent_stale and the worker gets one wake per new stale set
# asking it to check and stop them.
#
# Usage: worker-stall-watch.sh <worker> [<worker>...]
#        worker-stall-watch.sh --status <worker>...   (ladder state, read-only)
# Env:   STALL_MIN (default 20), STALL_FIXTURE_SESSION / STALL_FIXTURE_HISTORY /
#        STALL_FIXTURE_OUTBOUND (JSON files used instead of the API, for tests),
#        STALL_FIXTURE_TRANSCRIPT (a conversation .jsonl, for tests),
#        STALL_NOW (epoch, for tests), STALL_DRY_RUN=1 (decide only),
#        STALL_CONFIRM_SLEEP (seconds between delivery re-checks, default 10),
#        STALL_VERIFY_MIN (default 8), STALL_RESTART_COOLDOWN_H (default 6),
#        STALL_STOP_WAIT_S (default 60), STALL_GUARD_PY (the guard to import).
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
VERIFY_MIN="${STALL_VERIFY_MIN:-8}"            # a fired rung gets this long to show progress
RESTART_COOLDOWN_H="${STALL_RESTART_COOLDOWN_H:-6}"  # at most one ladder restart per worker per window
SUB_IDLE_MIN="${STALL_SUBAGENT_IDLE_MIN:-60}"   # a running subagent silent this long is stale
SUB_MAX_H="${STALL_SUBAGENT_MAX_H:-4}"          # a running subagent older than this is stale...
SUB_AGE_IDLE_MIN="${STALL_SUBAGENT_AGE_IDLE_MIN:-15}"  # ...only if also silent this long (a reused agent still writing is working)
U="$(amux url 2>/dev/null)"
now="${STALL_NOW:-$(date -u +%s)}"
ts=$(date -u -r "$now" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "@$now" +%Y-%m-%dT%H:%M:%SZ)
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

logl() { echo "$ts $*" >> "$LOG"; }

# ---- recovery ladder (a stalled turn: answer, interrupt, restart, page) -------
LADDER_RUNGS="answer interrupt restart page"
lpath() { echo "$STATE_DIR/$1.ladder"; }
lget() { sed -n "s/^$2=//p" "$(lpath "$1")" 2>/dev/null | tail -1; }
lset() {
  local f; f=$(lpath "$1")
  { grep -v "^$2=" "$f" 2>/dev/null; echo "$2=$3"; } > "$f.tmp" && mv "$f.tmp" "$f"
}
rung_verdict() {
  case "$1" in answer) echo ladder_answered ;; interrupt) echo ladder_interrupted ;;
    restart) echo ladder_restarted ;; page) echo ladder_paged ;; *) echo ladder_unknown ;; esac
}

# post_api <path> <json> <outfile>: POST to the amux API, prints the HTTP code
# (000 when curl got no answer).
post_api() {
  local code
  code=$(curl -sk --max-time 20 -o "$3" -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    -H "X-Amux-Session: ${STALL_WAKE_ORIGIN:-${AMUX_SESSION:-}}" -d "$2" "$U$1") || code=000
  echo "$code"
}

# amux_pref <KEY> <default>: the server's D2 policy value, read the way the server
# reads it (its env, which ~/.amux/server.env seeds), lower-cased.
amux_pref() {
  local v="${!1:-}"
  [ -n "$v" ] || v=$(sed -n "s/^$1=//p" "${AMUX_HOME:-$HOME/.amux}/server.env" 2>/dev/null | tail -1 | tr -d "\"' ")
  echo "${v:-$2}" | tr '[:upper:]' '[:lower:]'
}

# pane_prompt <worker>: rate_limit | resume_mode | none | unmeasured. The two
# prompts amux answers by policy (ethos D2), matched exactly as the server's
# is_rate_limit_menu and is_resume_mode_prompt in session_verbs.rs match them;
# tests/worker-stall-watch.test.mjs fails if those phrases leave the Rust source.
pane_prompt() {
  local body
  body=$(curl -sk --max-time 15 "$U/api/sessions/$1/peek?lines=40") || { echo unmeasured; return; }
  python3 -c '
import json,re,sys
try: raw=str(json.loads(sys.argv[1]).get("output") or "")
except Exception: print("unmeasured"); sys.exit()
if not raw.strip(): print("unmeasured"); sys.exit()
clean=re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07","",raw).lower()
lines=clean.split("\n")
marks=[i for i,l in enumerate(lines) if l.strip() in ("❯","›")]
tail="\n".join(lines[marks[-1] if marks else max(0,len(lines)-12):])
if "what do you want to do?" in tail and "stop and wait for limit to reset" in tail and "switch to usage credits" in tail:
    print("rate_limit"); sys.exit()
c=clean.replace("’","'"'"'")
if (re.search(r"(?m)^\s*(?:[❯>]\s*)?1\.\s+resume from summary",c)
        and re.search(r"(?m)^\s*(?:[❯>]\s*)?2\.\s+resume full session",c)
        and "don'"'"'t ask me again" in c):
    print("resume_mode"); sys.exit()
print("none")' "$body"
}

# fire_rung <worker> <rung>: 0 = fired (verify later), 1 = failed (next rung now),
# 2 = skipped (next rung now). Every outcome writes one log line.
fire_rung() {
  local w=$1 r=$2 v code out="$STATE_DIR/$1.rung.out"
  v=$(rung_verdict "$r")
  if [ "$r" != page ]; then
    local sw; case "$r" in answer) sw="${STALL_RUNG_ANSWER:-1}" ;; interrupt) sw="${STALL_RUNG_INTERRUPT:-1}" ;; restart) sw="${STALL_RUNG_RESTART:-1}" ;; esac
    if [ "$sw" = 0 ]; then logl "worker=$w verdict=ladder_skip rung=$r reason=disabled"; return 2; fi
    if [ "$isolated" = true ] && [ "${STALL_LADDER_ISOLATED:-0}" != 1 ]; then
      logl "worker=$w verdict=ladder_skip rung=$r reason=isolated (set STALL_LADDER_ISOLATED=1 on the schedule to opt in)"; return 2
    fi
  fi
  case "$r" in
  answer)
    local prompt key policy
    prompt=$(pane_prompt "$w")
    case "$prompt" in
      rate_limit) policy=$(amux_pref AMUX_RATE_LIMIT_ACTION wait); key=Enter ;;   # the server's own answer: option 1, stop and wait
      resume_mode) policy=$(amux_pref AMUX_RESUME_MODE_ACTION summary); key=1 ;;  # the digit, never Enter (resume_mode_action)
      unmeasured) logl "worker=$w verdict=ladder_skip rung=answer reason=pane_unreadable"; return 2 ;;
      *) logl "worker=$w verdict=ladder_skip rung=answer reason=no_known_prompt"; return 2 ;;
    esac
    if [ "$policy" = off ]; then logl "worker=$w verdict=ladder_skip rung=answer reason=policy_off prompt=$prompt"; return 2; fi
    if [ "${STALL_DRY_RUN:-}" = 1 ]; then logl "worker=$w verdict=$v result=dry prompt=$prompt key=$key"; return 0; fi
    code=$(post_api "/api/sessions/$w/keys" "{\"keys\":\"$key\"}" "$out")
    case "$code" in 2??) ;; *) logl "WARN worker=$w verdict=ladder_failed rung=answer reason=keys_http_$code prompt=$prompt"; return 1 ;; esac
    sleep "${STALL_CONFIRM_SLEEP:-10}"
    if [ "$(pane_prompt "$w")" = "$prompt" ]; then
      logl "WARN worker=$w verdict=ladder_failed rung=answer reason=prompt_still_shown prompt=$prompt key=$key"; return 1
    fi
    logl "worker=$w verdict=$v result=fired prompt=$prompt key=$key http=$code"; return 0 ;;
  interrupt)
    if [ "${STALL_DRY_RUN:-}" = 1 ]; then logl "worker=$w verdict=$v result=dry key=Escape"; return 0; fi
    code=$(post_api "/api/sessions/$w/keys" '{"keys":"Escape"}' "$out")
    case "$code" in 2??) ;; *) logl "WARN worker=$w verdict=ladder_failed rung=interrupt reason=keys_http_$code"; return 1 ;; esac
    logl "worker=$w verdict=$v result=fired key=Escape http=$code effect=$(sed -n 's/.*"effect":"\([a-z_]*\)".*/\1/p' "$out" | head -1)"; return 0 ;;
  restart)
    local last waited=0 running resumed
    last=$(tail -1 "$STATE_DIR/$w.restarts" 2>/dev/null)
    if [ -n "$last" ] && [ $((now - last)) -lt $((RESTART_COOLDOWN_H * 3600)) ]; then
      logl "worker=$w verdict=ladder_skip rung=restart reason=cooldown last_restart_min_ago=$(((now - last) / 60)) cooldown_h=$RESTART_COOLDOWN_H"; return 2
    fi
    if [ "${STALL_DRY_RUN:-}" = 1 ]; then logl "worker=$w verdict=$v result=dry"; return 0; fi
    echo "$now" >> "$STATE_DIR/$w.restarts"   # counted before acting: a crash mid-restart still uses the allowance
    code=$(post_api "/api/sessions/$w/stop" '{}' "$out")
    case "$code" in 2??) ;; *) logl "WARN worker=$w verdict=ladder_failed rung=restart reason=stop_http_$code"; return 1 ;; esac
    while :; do
      running=$(curl -sk --max-time 15 "$U/api/sessions/$w" | python3 -c 'import json,sys
try: print(str(json.load(sys.stdin).get("running")).lower())
except Exception: print("unknown")')
      [ "$running" = false ] && break
      [ "$waited" -ge "${STALL_STOP_WAIT_S:-60}" ] && break
      sleep "${STALL_POLL_S:-3}"; waited=$((waited + ${STALL_POLL_S:-3}))
    done
    if [ "$running" != false ]; then
      logl "WARN worker=$w verdict=ladder_failed rung=restart reason=stop_unconfirmed running=$running waited_s=$waited"; return 1
    fi
    code=$(post_api "/api/sessions/$w/start" '{}' "$out")
    case "$code" in 2??) grep -q '"ok":true' "$out" 2>/dev/null || code="${code}-not-ok" ;; esac
    case "$code" in 2??) ;; *) logl "WARN worker=$w verdict=ladder_failed rung=restart reason=start_http_$code waited_s=$waited"; return 1 ;; esac
    resumed=$(grep -q '"resumed":true' "$out" && echo true || echo false)
    logl "worker=$w verdict=$v result=fired resumed=$resumed stop_wait_s=$waited http=$code"
    # start resumes the stored conversation; one without it starts fresh, so say so.
    [ "$resumed" = true ] || logl "WARN worker=$w verdict=ladder_restarted note=no_stored_conversation_started_fresh"
    return 0 ;;
  page)
    if [ -f "$stamp" ]; then return 0; fi
    [ "${STALL_DRY_RUN:-}" = 1 ] || amux alert "Worker $w is stuck inside a turn: no progress for ${age_min} min while $unread message(s) wait, and the recovery ladder did not clear it (tried: ${tried:-none}). See ~/.amux/logs/worker-stall-watch.log; answer or dismiss whatever its pane shows." "orchestrator stalled" >/dev/null 2>&1
    logl "WARN worker=$w verdict=$v action=owner_alert tried=${tried:-none}"; date -u +%s > "$stamp"
    return 0 ;;
  esac
}

# ladder_step <worker>: one step per run for a worker stalled inside a turn.
ladder_step() {
  local w=$1 rung fired r res
  rung=$(lget "$w" rung); fired=$(lget "$w" fired_at); tried=$(lget "$w" tried)
  [ "$rung" = page ] && return   # the last rung fired: nothing left but the owner
  if [ -n "$rung" ]; then
    if [ $((now - fired)) -lt $((VERIFY_MIN * 60)) ]; then
      logl "worker=$w verdict=ladder_wait rung=$rung waited_s=$((now - fired)) verify_min=$VERIFY_MIN"; return
    fi
    logl "WARN worker=$w verdict=ladder_failed rung=$rung reason=no_progress_after_${VERIFY_MIN}min"
    rc=2
  fi
  for r in $LADDER_RUNGS; do
    case ",$tried," in *",$r,"*) continue ;; esac
    tried="${tried:+$tried,}$r"
    fire_rung "$w" "$r"; res=$?
    if [ "$res" = 0 ]; then
      if [ "${STALL_DRY_RUN:-}" != 1 ]; then lset "$w" rung "$r"; lset "$w" fired_at "$now"; lset "$w" tried "$tried"; fi
      return
    fi
    [ "$res" = 1 ] && rc=2
  done
}

# ladder_settle <worker> <progress-epoch>: the stall cleared. Verified means the
# worker made progress after the last rung fired.
ladder_settle() {
  local w=$1 prog=${2%.*} rung fired res
  rung=$(lget "$w" rung); fired=$(lget "$w" fired_at)
  if [ -n "$rung" ]; then
    if [ "${prog:-0}" -gt "${fired:-0}" ]; then res=verified; else res=cleared_without_progress; fi
    logl "worker=$w verdict=$(rung_verdict "$rung") result=$res progress_after_s=$(( ${prog:-0} - ${fired:-0} )) tried=$(lget "$w" tried)"
  fi
  rm -f "$(lpath "$w")"
}

# ---- subagent age -----------------------------------------------------------
# The boundary guard's reader, imported rather than copied: the dir this script
# runs from (tests, a checkout), else the installed hook.
GUARD_PY="${STALL_GUARD_PY:-}"
if [ -z "$GUARD_PY" ]; then
  for c in "$(dirname "$0")/hooks/boundary-guard.py" "${AMUX_HOME:-$HOME/.amux}/hooks/boundary-guard.py"; do
    [ -f "$c" ] && { GUARD_PY=$c; break; }
  done
fi

subagent_check() {
  local w=$1 res status fields ids summary code prev
  res=$(python3 - "$GUARD_PY" "$w" "$now" "$((SUB_IDLE_MIN * 60))" "$((SUB_MAX_H * 3600))" "${STALL_FIXTURE_TRANSCRIPT:-}" "${AMUX_HOME:-$HOME/.amux}" "$((SUB_AGE_IDLE_MIN * 60))" <<'PY'
import glob, importlib.util, json, os, sys
guard, w, now, idle_s, max_s, tp, home = sys.argv[1], sys.argv[2], float(sys.argv[3]), float(sys.argv[4]), float(sys.argv[5]), sys.argv[6], sys.argv[7]
age_idle_s = float(sys.argv[8])
def out(status, fields, ids="", summary=""):
    print(status); print(fields); print(ids); print(summary); sys.exit()
def unmeasured(why):
    out("unmeasured", 'measured=false n_considered=0 why_unmeasured="%s"' % why.replace('"', "'"))
if not guard or not os.path.isfile(guard):
    unmeasured("boundary guard not found")
try:
    spec = importlib.util.spec_from_file_location("amux_boundary_guard", guard)
    g = importlib.util.module_from_spec(spec); spec.loader.exec_module(g)
except Exception as e:  # noqa: BLE001
    unmeasured("guard import failed: %s" % e.__class__.__name__)
if not tp:
    try:
        meta = json.load(open(os.path.join(home, "sessions", w + ".meta.json")))
    except (OSError, ValueError):
        unmeasured("no session meta")
    conv = str(meta.get("cc_conversation_id") or "")
    if not conv:
        unmeasured("no cc_conversation_id in session meta")
    base = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
    hits = glob.glob(os.path.join(base, "projects", "*", conv + ".jsonl"))
    if not hits:
        unmeasured("transcript %s.jsonl not found" % conv)
    tp = hits[0]
# stale_s widened to the max age: an agent silent past the idle threshold must
# still count as running here, which the guard's 30 min default would drop.
c = g.count_running_subagents(tp, {"stale_s": max_s, "no_meta_warm_s": max_s}, now=now, host_start=None)
if not c.get("measured"):
    unmeasured(str(c.get("why_unmeasured") or "guard reader unmeasured"))
stale = [a for a in c.get("running_agents") or []
         if a.get("idle_s", 0) > idle_s or (a.get("age_s") is not None and a["age_s"] > max_s and a.get("idle_s", 0) > age_idle_s)]
fields = "measured=true n_considered=%d running=%d stale=%d" % (c.get("n_considered", 0), c.get("running", 0), len(stale))
summary = "; ".join("%s (%s) running %s, last wrote %s ago" % (a["id"], a.get("description") or "no description",
                     g.fmt_age(a.get("age_s")), g.fmt_age(a.get("idle_s"))) for a in stale)
out("stale" if stale else "ok", fields, ",".join(sorted(a["id"] for a in stale)), summary)
PY
)
  { read -r status; read -r fields; read -r ids; read -r summary; } <<<"$res"
  prev="$STATE_DIR/$w.subagent_stale"
  case "$status" in
    ok) logl "worker=$w verdict=subagent_ok $fields"; rm -f "$prev" ;;
    stale)
      logl "WARN worker=$w verdict=subagent_stale $fields idle_min=$SUB_IDLE_MIN max_h=$SUB_MAX_H ids=$ids"
      [ "$rc" = 0 ] && rc=1
      # One wake per new stale set, not one every run.
      if [ "$(cat "$prev" 2>/dev/null)" != "$ids" ]; then
        if [ "${STALL_DRY_RUN:-}" = 1 ]; then code=dry
        else code=$(send_wake "$w" "Stall watchdog: subagent(s) look stale (silent over ${SUB_IDLE_MIN} min, or running over ${SUB_MAX_H} h and silent over ${SUB_AGE_IDLE_MIN} min): $summary. Check them and TaskStop any that are stuck or no longer needed." 15)
        fi
        case "$code" in
          2??|dry) logl "worker=$w verdict=subagent_stale action=wake_sent http=$code ids=$ids"; echo "$ids" > "$prev" ;;
          *) logl "WARN worker=$w verdict=subagent_stale action=wake_failed http=$code ids=$ids"; rc=2 ;;
        esac
      fi ;;
    *) logl "worker=$w verdict=subagent_unmeasured ${fields:-measured=false why_unmeasured=\"no output\"}" ;;
  esac
}

if [ "${1:-}" = --status ]; then
  shift
  for w in "$@"; do
    echo "worker=$w ladder=[$(tr '\n' ' ' < "$(lpath "$w")" 2>/dev/null | sed 's/ $//')] restarts=$(wc -l < "$STATE_DIR/$w.restarts" 2>/dev/null | tr -d ' ' || echo 0) last_restart=$(tail -1 "$STATE_DIR/$w.restarts" 2>/dev/null) paged=$([ -f "$STATE_DIR/$w.alerted" ] && echo yes || echo no) subagent_stale=[$(cat "$STATE_DIR/$w.subagent_stale" 2>/dev/null)]"
  done
  exit 0
fi

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
print("STALLED" if why else "ok", turn or "unknown", int(age//60), unread, ",".join(why) or "-", int(prog), str(bool(s.get("isolated"))).lower())
' "$w" "$now" "$STALL_MIN" "$sess" "$hist" "$QUEUE_MIN" "$outb")
  read -r state turn age_min unread why prog_at isolated <<<"$verdict"
  echo "$ts worker=$w verdict=$state turn=$turn idle_min=${age_min:-?} unread=${unread:-?} why=${why:--} stall_min=$STALL_MIN queue_min=$QUEUE_MIN" >> "$LOG"
  stamp="$STATE_DIR/$w.alerted"
  if [ "$state" = STALLED ]; then
    rc=1
    echo "$ts WARN worker=$w STALLED ($why) turn=$turn no progress for ${age_min} min with $unread unread or queued message(s)" >> "$LOG"
    if [ "$turn" = running ]; then
      # Blocked inside a turn (a prompt, a question): a message cannot reach it.
      # Climb the recovery ladder; the owner is its last rung.
      ladder_step "$w"
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
  elif [ "$state" = ok ]; then
    ladder_settle "$w" "$prog_at"
    rm -f "$stamp"   # recovered or healthy: the next stall alerts again
  fi
  # An unmeasured run keeps the ladder and the page stamp: it saw nothing.
  subagent_check "$w"
done
exit $rc
