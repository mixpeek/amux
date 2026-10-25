#!/usr/bin/env bash
# End-to-end check of /codex: start a named Codex thread, have it do a browser task and post the
# result to this amux worker, and observe it with codex-skill from discovery to idle.
#
#   skills/codex/tests/e2e-browser-roundtrip.sh [--timeout 900]
#
# Spends model tokens (one short Codex turn), so it is not part of the unit run. Needs $AMUX_SESSION.
# Exit 0 only when: list --active found the thread, watch saw running -> idle, and the thread's
# final message carries the result marker. Whether the POST reached this worker shows up here as a
# new owner message; this script cannot read that, so it checks the thread's curl exit instead.
set -euo pipefail
TIMEOUT=900
[ "${1:-}" = "--timeout" ] && TIMEOUT="$2"
HERE="$(cd "$(dirname "$0")/../scripts" && pwd)"
CS="$HERE/codex-skill.py"
: "${AMUX_SESSION:?run inside an amux worker}"
U="$(amux url)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/codex-e2e.XXXXXX")"
NAME="#amux codex-skill e2e $(date +%m%d-%H%M%S)"

out="$("$HERE/codex-new.py" "$NAME" - --cwd "$WORK" --full-access <<MSG
This is an automated end-to-end test from the amux worker $AMUX_SESSION. Do these steps in order:

1. Use a real browser (your browser tool, or headless Chromium/Playwright) to open https://example.com. Record the page title and the H1 text.

2. Follow the page's "Learn more" (or "More information...") link. Record the URL it lands on and that page's H1.

3. Save a screenshot of the second page as $WORK/page2.png.

4. Send the result to the amux worker with this command, filling in what you observed (one line of JSON):

curl -sk -X POST -H 'Content-Type: application/json' -d '{"text":"[codex-skill e2e result] title=<title> h1=<h1> page2_url=<url> page2_h1=<h1> screenshot=$WORK/page2.png browser=<tool used>"}' $U/api/sessions/$AMUX_SESSION/send

5. Reply in this chat with the same result and the HTTP status the curl returned.

Do not do anything else.
MSG
)"
echo "$out"
TID="$(printf '%s\n' "$out" | sed -n 's/^Started thread \([0-9a-f-]*\).*/\1/p')"
[ -n "$TID" ] || { echo "FAIL: no thread id"; exit 1; }

found=""
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
  sleep 5
  if "$CS" list --active | python3 -c "
import json,sys; d=json.load(sys.stdin); ids=[t['id'] for t in d['threads']]
hit=[t for t in d['threads'] if t['id']=='$TID']
print('poll $i: active', len(ids), 'listed_by', d['n_listed_by'], 'state', hit and hit[0]['status']['state'])
sys.exit(0 if hit else 1)"; then found=1; break; fi
done
[ -n "$found" ] || { echo "FAIL: new thread never appeared in list --active within 60s"; exit 1; }

"$CS" watch "$TID" --since 0 --interval 3 --until-idle --duration "$TIMEOUT" > "$WORK/watch.ndjson"
python3 - "$WORK/watch.ndjson" <<'PY'
import json, sys
ev = [json.loads(l) for l in open(sys.argv[1])]
st = [(e["from"], e["to"]) for e in ev if e.get("event") == "status"]
items = [e["item"] for e in ev if e.get("event") == "item"]
end = ev[-1]
final = next((i for i in reversed(items) if i["kind"] == "agentMessage"), {})
print("status transitions:", st)
print("items:", len(items), "end:", end.get("reason"), end.get("status"))
ok = any(t == "running" for _, t in st) and list(end["status"].values()) == ["idle"] \
     and "[codex-skill e2e result]" in (final.get("text") or "")
print("final:", (final.get("text") or "")[:300])
sys.exit(0 if ok else "FAIL: did not see running -> idle with a result message")
PY
echo "PASS thread=$TID work=$WORK"
