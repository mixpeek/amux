#!/usr/bin/env bash
# The only tool a Dana evaluator gets: a phone-sized browser on the household
# demo, driven by what a person sees (visible text), through the amux browser
# API. It never reads code or docs.
#
#   dana.sh open                 open the app (phone size, fresh profile)
#   dana.sh look                 the visible text of the screen, and a screenshot path
#   dana.sh tap "<visible text>" tap the button, link or row showing that text
#   dana.sh type "<text>"        type into the message box (or the focused field)
#   dana.sh enter                press Enter
#   dana.sh wait <seconds>       wait (up to 120), then look
#   dana.sh close
#
# Env: DANA_URL (the household app), DANA_PROFILE (a fresh profile per
# evaluator), DANA_SHOTS (where screenshots are copied).
set -uo pipefail
U="$(amux url)"
H=(-H "Content-Type: application/json" -H "X-Amux-Session: ${AMUX_SESSION:-dana}")
URL="${DANA_URL:-https://localhost:18950}"
PROFILE="${DANA_PROFILE:-dana-persona}"
SHOTS="${DANA_SHOTS:-${TMPDIR:-/tmp}/dana-shots}"
mkdir -p "$SHOTS"

post() { curl -sk --max-time 90 -X POST "${H[@]}" -d "$2" "$U/api/browser/$1"; }
act() { post action "$1"; }
js() { python3 -c 'import json,sys; print(json.dumps({"action":"eval","script":sys.argv[1]}))' "$1"; }

look() {
  sleep 1
  act "$(js "(() => { const t = document.body.innerText || ''; return t.replace(/\n{3,}/g, '\n\n').slice(0, 6000); })()")" \
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
    t="${2:?what to tap}"
    act "$(js "(() => { const want = $(python3 -c 'import json,sys;print(json.dumps(sys.argv[1].strip().lower()))' "$t");
      const els = [...document.querySelectorAll('button, a, [role=button], [role=listitem], label, summary, input[type=radio], input[type=checkbox]')]
        .filter(e => e.offsetParent !== null || e.getClientRects().length);
      const txt = e => (e.innerText || e.value || e.getAttribute('aria-label') || '').trim().toLowerCase();
      const el = els.find(e => txt(e) === want) || els.find(e => txt(e).startsWith(want)) || els.find(e => txt(e).includes(want));
      if (!el) return 'NOT FOUND: nothing on screen says ' + JSON.stringify(want);
      el.scrollIntoView({block: 'center'}); el.click(); return 'tapped: ' + txt(el).slice(0, 80); })()")" \
      | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("result",d))'
    sleep 2; look ;;
  type)
    t="${2:?what to type}"
    act "$(js "(() => { const box = (document.activeElement && /TEXTAREA|INPUT/.test(document.activeElement.tagName) && document.activeElement.offsetParent) ? document.activeElement
        : [...document.querySelectorAll('textarea, input[type=text], input:not([type])')].find(e => e.offsetParent !== null);
      if (!box) return 'NOT FOUND: no text box on screen';
      box.focus(); box.value = $(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$t");
      box.dispatchEvent(new Event('input', {bubbles: true})); return 'typed into: ' + (box.placeholder || box.getAttribute('aria-label') || box.tagName); })()")" \
      | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d.get("result",d))' ;;
  enter) act '{"action":"key","key":"Enter"}' >/dev/null; sleep 2; look ;;
  wait) s="${2:-10}"; [ "$s" -gt 120 ] && s=120; sleep "$s"; look ;;
  close) post stop "$(python3 -c 'import json,sys;print(json.dumps({"profile":sys.argv[1]}))' "$PROFILE")" >/dev/null; echo closed ;;
  *) echo "unknown: $1" >&2; exit 2 ;;
esac
