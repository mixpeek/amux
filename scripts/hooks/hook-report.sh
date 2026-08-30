#!/bin/bash
# amux report hook (AMUX-2829). Reads Claude Code's hook payload on STDIN,
# extracts the transcript path, and reports REAL context usage with the state.
#
# WHY THIS EXISTS: the hooks used to POST a fixed body with no tokens, so the
# server never knew any lane's context size. Nothing could emit ContextLow, so
# orchestrator/compaction.rs's policy was NEVER CALLED and lanes ran to the wall
# and stopped. Ethan, 2026-08-10: "theres no reason amux should ever stop."
#
# Fails open in every direction: no stdin, no transcript, no python -> report
# the state anyway with tokens omitted. A liveness report is worth more than a
# token count, and this must never be the reason a hook fails.
STATE="${1:-idle}"; SRC="${2:-stop-hook}"
DERIVED=0
if [ -z "$AMUX_SESSION" ]; then
  # MR-43: the var can go missing INSIDE a lane that IS running in its
  # amux-launched pane (spawn always injects it — session_verbs.rs — so this
  # is loss in-process, not absence at launch). Recover it from tmux so the
  # lane is not invisible to its own liveness report, and flag the recovery
  # in the body so /api/logs/analyze can count how often this happens instead
  # of a human noticing a lane that silently never reported.
  TNAME=$(tmux display-message -p '#S' 2>/dev/null)
  case "$TNAME" in
    amux-*) export AMUX_SESSION="${TNAME#amux-}"; DERIVED=1 ;;
  esac
fi
[ -n "$AMUX_SESSION" ] || exit 0
IN=$(cat 2>/dev/null)
E="$HOME/.amux/endpoint.json"
C=$(sed -n 's/.*"canonical_url":"\([^"]*\)".*/\1/p' "$E" 2>/dev/null)
L=$(sed -n 's/.*"legacy_port":\([0-9]*\).*/\1/p' "$E" 2>/dev/null)
U="${AMUX_URL:-$C}"
case "$U" in *localhost:$L|*127.0.0.1:$L) U="${C:-$U}";; esac
BODY=$(printf '%s' "$IN" | /usr/bin/python3 -c '
import json,sys,os
raw=sys.stdin.read()
state,src=sys.argv[1],sys.argv[2]
out={"state":state,"source":src}
h={}; tp=""; nlines=0; err=""
try:
    h=json.loads(raw) if raw.strip() else {}
    tp=h.get("transcript_path") or ""
    m=h.get("model") or {}
    if isinstance(m,dict): m=m.get("id") or m.get("display_name") or ""
    if m: out["model"]=m
    # CONVERSATION ID (AMUX-2936, 2026-08-15). Free: already in the payload we
    # parse. It closes the blind-cotenant window in the staged-commit guard.
    #
    # The guard calls a lane BLIND when it cannot resolve that lane transcript,
    # and blind is the ONLY class where a commit absorbing another session work
    # passes silently: foreign exits 1 and blocks, unclaimed does not. Measured
    # 2026-08-15, 321 blind warnings in 8h53m on ~/Dev/mixpeek, 304 naming ONE
    # running lane whose meta carried an empty cc_conversation_id. The server
    # was scanning 162 transcript files to re-derive an id the harness hands us
    # on stdin and we dropped on the floor.
    #
    # Sent from HERE because the server cannot derive it safely: ~/Dev/mixpeek
    # hosts ~30 lanes, so "newest jsonl in the project dir" is whichever
    # neighbour spoke last. On 2026-08-09 that stamped the amux lane with the
    # LIVE conversation of amux-rust. The harness knows; nothing else does.
    #
    # NO APOSTROPHES ANYWHERE IN THIS BLOCK. The whole python program is a
    # single-quoted shell argument, so one apostrophe ends it and the file dies
    # with "unexpected EOF while looking for matching )". Adding this comment
    # did exactly that on the first attempt, and a broken hook-report.sh breaks
    # state reporting for every already-running lane, since amux-report.sh
    # execs this path.
    sid=h.get("session_id") or ""
    if not sid and tp and tp.endswith(".jsonl"):
        sid=os.path.basename(tp)[:-6]
    if sid: out["session_id"]=sid
except Exception as e:
    err="payload:"+type(e).__name__
# TRANSCRIPT READ GETS ITS OWN try (2026-08-11). It used to sit inside the outer
# one, so a missing or unreadable transcript threw straight past the diagnostic
# below — skipping the log in exactly the case the log exists to explain. Caught
# by building a failing payload and checking the log stayed empty, which is the
# only way that class of hole shows up: everything looked like it ran.
if tp:
    try:
        tot=0
        # MODEL FROM THE TRANSCRIPT, not only from the hook payload: the payload
        # shape is the harness own and may or may not carry it, while every
        # assistant message records the model it was produced by. This is the
        # field /api/sessions turns into active_model (AMUX-2828) — the consumer
        # has always existed (sessions_legacy.rs:1292); nothing ever sent it, so
        # the dashboard fell back to CC_FLAGS, the model amux ASKED for at spawn.
        # social-media showed "opus" while running Fable 5 AND rate-limited on it.
        for l in open(tp):
            nlines+=1
            try: d=json.loads(l)
            except Exception: continue
            msg=d.get("message") or {}
            mm=msg.get("model")
            if mm: out["model"]=mm
            u=msg.get("usage") or d.get("usage")
            if isinstance(u,dict):
                tot=(u.get("input_tokens",0)+u.get("cache_read_input_tokens",0)
                     +u.get("cache_creation_input_tokens",0)+u.get("output_tokens",0))
        if tot: out["tokens"]=tot
    except Exception as e:
        err="transcript:"+type(e).__name__
# WHY-IT-FAILED DIAGNOSTIC. model reached 2 of 42 reporting lanes and tokens 1
# of 42, and nothing could say WHICH step dropped them: absent transcript_path,
# unreadable file, and a transcript carrying neither field all produced the
# identical empty report. Written only when something is missing, so it
# self-extinguishes as this gets fixed instead of growing across 47 lanes.
if not out.get("model") or not out.get("tokens"):
    try:
        d2={"s":os.environ.get("AMUX_SESSION",""),"src":src,"keys":sorted(h.keys())[:12],
            "tp":bool(tp),"tp_exists":bool(tp) and os.path.exists(tp),"lines":nlines,
            "err":err,"got_model":bool(out.get("model")),"got_tokens":bool(out.get("tokens"))}
        lg=os.path.expanduser("~/.amux/logs/hook-extract.jsonl")
        if os.path.exists(lg) and os.path.getsize(lg)>2000000: os.remove(lg)
        with open(lg,"a") as f: f.write(json.dumps(d2)+"\n")
    except Exception: pass
print(json.dumps(out))
' "$STATE" "$SRC" 2>/dev/null)
[ -n "$BODY" ] || BODY="{\"state\":\"$STATE\",\"source\":\"$SRC\"}"
# Surgery, not a third JSON encoder: BODY is always a flat object ending in
# "}" (python's json.dumps above, or the fallback literal on this same line),
# so appending before the final brace is safe and avoids a second place this
# script can break on stdin shape (MR-43).
[ "$DERIVED" = "1" ] && BODY="${BODY%\}}, \"amux_session_derived\": true}"
# X-Amux-Session stamps the write server-side (AMUX-1768). report_post's own
# comment names its absence as the standing residual: "the shipped hooks send no
# header, so an UNSTAMPED write is still accepted". This IS the shipped hook.
# Status, NOT exit code (AF-100). `curl -s` exits 0 for an HTTP 404 — a 404 is a
# successful HTTP transaction — so `if ! curl` caught only TRANSPORT failures and
# treated every server REJECTION as a delivered report. On 2026-08-19 a lane ran
# for 4h15m with AMUX_SESSION=amax-gtm (a typo for amux-gtm): 138 reports, every
# one a 404 "session not found", zero 200s, and nothing logged here. The block
# below calls itself "THE ONE FAILURE NOTHING ELSE CAN SEE" and could not see it.
#
# `%{http_code}` prints 000 when the transfer never completed, so one branch now
# covers both classes: 000 = could not reach the server (the AMUX-3046 stranded-
# port case this was written for), 4xx/5xx = reached it and was refused.
CODE=$(curl -sk -m 3 -o /dev/null -w '%{http_code}' \
  -X POST -H 'Content-Type: application/json' \
  -H "X-Amux-Session: $AMUX_SESSION" -d "$BODY" \
  "$U/api/sessions/$AMUX_SESSION/report" 2>/dev/null) || CODE=000
case "$CODE" in
  2*) ;;
  *)
  # A successful report is visible twice over — in the structured request log and
  # in the stored report's `source` — but a report that never landed reaches no
  # server, so "the hook never ran" and "the hook ran and was refused" are the
  # same silence. That is what a lane stranded on the retired port looks like
  # (AMUX-3046), and it is the reason AMUX-2936 sat unmeasurable: the degraded
  # verdict went back to the hook and was recorded nowhere.
  #
  # Still fails OPEN — this logs and exits 0. A hook that blocks a turn because
  # amux is unreachable would be worse than the silence it replaces.
  D="$HOME/.amux/logs"; [ -d "$D" ] || mkdir -p "$D" 2>/dev/null
  F="$D/hook-report-failures.log"
  printf '%s %s source=%s url=%s http=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    "$AMUX_SESSION" "$SRC" "$U" "$CODE" >> "$F" 2>/dev/null
  if [ "$(wc -l < "$F" 2>/dev/null || echo 0)" -gt 2000 ]; then
    tail -n 500 "$F" > "$F.tmp" 2>/dev/null && mv "$F.tmp" "$F" 2>/dev/null
  fi
  ;;
esac
exit 0
