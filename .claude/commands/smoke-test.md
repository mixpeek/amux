---
description: Use after making changes to amux-server.py to verify the dashboard and APIs are working correctly
allowed-tools: Bash, Read
argument-hint: [quick|full]
context: fork
---

# /smoke-test — Product Verification

Quick health check of the amux server and dashboard after code changes.

The user's request is: **$ARGUMENTS**

## Quick (default)

Run these checks and report pass/fail for each:

```bash
# 1. Server is up
curl -sk -o /dev/null -w '%{http_code}' https://localhost:8822/ | grep -q 200

# 2. Dashboard HTML is valid (contains expected markers)
curl -sk https://localhost:8822/ | grep -q 'id="app"'

# 3. Sessions API responds
curl -sk https://localhost:8822/api/sessions | python3 -c "import json,sys; d=json.load(sys.stdin); print(f'{len(d)} sessions')"

# 4. Board API responds
curl -sk https://localhost:8822/api/board | python3 -c "import json,sys; d=json.load(sys.stdin); print(f'{len(d)} items')"

# 5. SSE endpoint connects (grab first event within 15s)
timeout 15 curl -sk -N https://localhost:8822/api/events 2>/dev/null | head -5

# 6. Python syntax valid
python3 -c "import ast; ast.parse(open('amux-server.py').read())" && echo "syntax ok"
```

## Full

Run all quick checks plus:

```bash
# 7. Notes API
curl -sk https://localhost:8822/api/notes | python3 -c "import json,sys; json.load(sys.stdin); print('notes ok')"

# 8. Schedules API
curl -sk https://localhost:8822/api/schedules | python3 -c "import json,sys; json.load(sys.stdin); print('schedules ok')"

# 9. CRM API
curl -sk https://localhost:8822/api/crm/contacts | python3 -c "import json,sys; json.load(sys.stdin); print('crm ok')"

# 10. Email inbox API
curl -sk "https://localhost:8822/api/email/inbox?count=1" | python3 -c "import json,sys; json.load(sys.stdin); print('email ok')"

# 11. Calendar feed
curl -sk https://localhost:8822/api/calendar.ics | head -1 | grep -q 'BEGIN:VCALENDAR' && echo "ical ok"
```

Report a summary table: check name, status (pass/fail), and any error details for failures.

## Gotchas

- Always use `curl -sk` — self-signed TLS cert.
- The SSE check may hang if the server is down — the `timeout` command handles this.
- A syntax check pass does not mean the server reloaded — it auto-restarts on file save, but if the save was recent, give it 1-2 seconds.
