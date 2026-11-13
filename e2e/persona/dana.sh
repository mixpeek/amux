#!/usr/bin/env bash
# The only tool a Dana evaluator gets: a phone-sized browser on the household
# demo, driven by what a person sees (visible text), through the amux browser
# API. It never reads code or docs.
#
#   dana.sh open                 open the app (phone size, fresh profile)
#   dana.sh look                 the visible text of the screen, and a screenshot path
#   dana.sh tap "<visible text>" tap the button, link or row showing that text
#   dana.sh type "<text>"        type into the message box (or the focused field)
#   dana.sh scroll [up|down]     scroll the screen
#   dana.sh enter                press Enter
#   dana.sh wait <seconds>       wait (up to 120), then look
#   dana.sh close
#
# Env: DANA_URL (the household app), DANA_PROFILE (a fresh profile per
# evaluator), DANA_SHOTS (where screenshots are copied).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
U="$(amux url)"
H=(-H "Content-Type: application/json" -H "X-Amux-Session: ${AMUX_SESSION:-dana}")
URL="${DANA_URL:-https://localhost:18950}"
PROFILE="${DANA_PROFILE:-dana-persona}"
SHOTS="${DANA_SHOTS:-${TMPDIR:-/tmp}/dana-shots}"
mkdir -p "$SHOTS"

post() { curl -sk --max-time 90 -X POST "${H[@]}" -d "$2" "$U/api/browser/$1"; }
act() { post action "$1"; }

look() {
  sleep 1
  act "$(python3 "$HERE/dana_js.py" look)" \
    | python3 -c 'import json,sys
d=json.load(sys.stdin); r=d.get("result", d.get("value", d))
print(r if isinstance(r,str) else json.dumps(r)[:6000])'
  local shot n
  shot=$(curl -sk --max-time 60 "${H[@]}" "$U/api/browser/screenshot" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("path",""))' 2>/dev/null)
  if [ -n "$shot" ] && [ -f "$shot" ]; then
    n=$(ls "$SHOTS" | wc -l | tr -d ' ')
    cp "$shot" "$SHOTS/step-$(printf %02d "$n").jpg"
    echo "[screenshot: $SHOTS/step-$(printf %02d "$n").jpg]"
  fi
}

case "${1:-look}" in
  open)
    post start "$(python3 -c 'import json,sys;print(json.dumps({"profile":sys.argv[1],"url":sys.argv[2]}))' "$PROFILE" "$URL")" >/dev/null
    act '{"action":"viewport","width":390,"height":844}' >/dev/null
    post navigate "$(python3 -c 'import json,sys;print(json.dumps({"url":sys.argv[1]}))' "$URL")" >/dev/null
    sleep 3; look ;;
  look) look ;;
  tap)
    act "$(python3 "$HERE/dana_js.py" tap "${2:?what to tap}")" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("result",d))'
    sleep 2; look ;;
  type)
    act "$(python3 "$HERE/dana_js.py" type "${2:?what to type}")" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("result",d))' ;;
  scroll)
    act "$(python3 "$HERE/dana_js.py" scroll "${2:-down}")" >/dev/null; sleep 1; look ;;
  enter) act '{"action":"key","key":"Enter"}' >/dev/null; sleep 2; look ;;
  wait) s="${2:-10}"; [ "$s" -gt 120 ] && s=120; sleep "$s"; look ;;
  close) post stop "$(python3 -c 'import json,sys;print(json.dumps({"profile":sys.argv[1]}))' "$PROFILE")" >/dev/null; echo closed ;;
  *) echo "unknown: $1" >&2; exit 2 ;;
esac
