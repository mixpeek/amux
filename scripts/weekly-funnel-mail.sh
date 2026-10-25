#!/usr/bin/env bash
# Weekly amux.io funnel report by email (owner request MSG-91014, 2026-10-21).
# Runs scripts/site-funnel.py for the last 7 days from origin/main's committed
# copy and mails the markdown to the owner through the amux email API.
# Exit status is non-zero when the report or the send fails, so the scheduler
# shows the run as failed instead of silently skipping a week.
set -euo pipefail
REPO="${AMUX_REPO:-$HOME/Dev/amux}"
TO="${FUNNEL_MAIL_TO:-ethan@mixpeek.com}"
FROM="${FUNNEL_MAIL_FROM:-ethan@mixpeek.com}"
AMUX_BIN="${AMUX_BIN:-$(command -v amux || echo "$HOME/.local/bin/amux")}"
U="$("$AMUX_BIN" url)"
WORK="$(mktemp -d -t funnel.XXXXXX)"; trap 'rm -rf "$WORK"' EXIT
git -C "$REPO" fetch -q origin main
git -C "$REPO" show origin/main:scripts/site-funnel.py > "$WORK/site-funnel.py"
# PostHog answers 503/504 now and then; three tries a minute apart.
for try in 1 2 3; do
  if python3 "$WORK/site-funnel.py" --days 7 > "$WORK/report.md" 2> "$WORK/err.txt"; then break; fi
  echo "weekly-funnel-mail: report attempt $try failed: $(tail -1 "$WORK/err.txt")" >&2
  [ "$try" = 3 ] && exit 1
  sleep 60
done
[ -s "$WORK/report.md" ] || { echo "weekly-funnel-mail: empty report" >&2; exit 1; }
python3 - "$WORK/report.md" "$TO" "$FROM" > "$WORK/body.json" <<'EOF'
import json, sys, datetime
report, to, frm = open(sys.argv[1]).read(), sys.argv[2], sys.argv[3]
head = ("Weekly amux.io funnel, last 7 days.\n\n"
        "Paid Cloud signups come first; GitHub clicks second. Full numbers below.\n\n")
print(json.dumps({"to": to, "from": frm, "signature": False,
                  "subject": "amux.io funnel, week of " + datetime.date.today().isoformat(),
                  "body": head + report}))
EOF
code=$(curl -sk --max-time 60 -o "$WORK/resp.json" -w '%{http_code}' -X POST \
  -H 'Content-Type: application/json' -H "X-Amux-Session: ${AMUX_SESSION:-weekly-funnel-mail}" \
  --data @"$WORK/body.json" "$U/api/email/send")
case "$code" in 2??) echo "weekly-funnel-mail: sent to $TO (HTTP $code)";;
  *) echo "weekly-funnel-mail: send failed HTTP $code: $(head -c 300 "$WORK/resp.json")" >&2; exit 1;; esac
