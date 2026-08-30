---
description: Use when you need to interact with the amux system — manage board tasks, check sessions, send emails, automate browsers, or work with CRM contacts
allowed-tools: Bash, Read, Edit, Write
argument-hint: [board|memory|sessions|schedule|notes|email|browser|crm|help] [args...]
---

# /amux — amux Session Integration

You are running inside an **amux** managed Claude Code session. amux is a local multiplexer that manages multiple Claude sessions, a shared kanban board, notes, CRM, scheduler, email, browser automation, and per-session memory.

**Base URL:** `$AMUX_URL` (self-signed TLS — always use `curl -sk`)

---

## Board (tasks / issues)

```bash
# List all items
curl -sk $AMUX_URL/api/board | python3 -m json.tool

# Add item
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"title":"...", "desc":"...", "status":"todo", "session":"SESSION_NAME"}' \
  $AMUX_URL/api/board

# Update item
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"status":"doing"}' $AMUX_URL/api/board/ITEM_ID

# Delete item
curl -sk -X DELETE $AMUX_URL/api/board/ITEM_ID

# Claim a task atomically (prevents two sessions taking same task)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"session":"SESSION_NAME"}' $AMUX_URL/api/board/ITEM_ID/claim
```

Statuses: `backlog` · `todo` · `doing` · `done` (plus any custom columns)

---

## Sessions

```bash
# List sessions
curl -sk $AMUX_URL/api/sessions | python3 -m json.tool

# Send a message to a session
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"text":"your message"}' $AMUX_URL/api/sessions/SESSION_NAME/send

# Peek at a session's terminal output
curl -sk "$AMUX_URL/api/sessions/SESSION_NAME/peek?lines=100"
```

---

## Memory

```bash
# Read this session's memory
curl -sk $AMUX_URL/api/sessions/SESSION_NAME/memory

# Update this session's memory
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"content":"# My Notes\n..."}' $AMUX_URL/api/sessions/SESSION_NAME/memory

# Read/write global memory (shared across all sessions)
curl -sk $AMUX_URL/api/memory/global
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"content":"..."}' $AMUX_URL/api/memory/global
```

---

## Scheduler (recurring / one-time tasks)

Schedule commands to run in sessions at specific times or on a cron schedule.

```bash
# List all schedules
curl -sk $AMUX_URL/api/schedules | python3 -m json.tool

# Create a one-time schedule
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{
    "title": "Weekly analytics",
    "session": "gtm-videos",
    "command": "Run the weekly analytics report",
    "kind": "tmux",
    "sched_type": "once",
    "run_at": "2026-04-10T09:00"
  }' $AMUX_URL/api/schedules

# Create a recurring schedule (cron expression)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{
    "title": "Weekly video check",
    "session": "gtm-videos",
    "command": "Check video pipeline status and post summary to board",
    "kind": "tmux",
    "sched_type": "recurring",
    "schedule_expr": "0 9 * * 1"
  }' $AMUX_URL/api/schedules

# Update a schedule
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"enabled": 1}' $AMUX_URL/api/schedules/SCHED_ID

# Delete a schedule
curl -sk -X DELETE $AMUX_URL/api/schedules/SCHED_ID

# View recent runs
curl -sk $AMUX_URL/api/schedules/runs | python3 -m json.tool

# Trigger a schedule immediately
curl -sk -X POST $AMUX_URL/api/schedules/SCHED_ID/run
```

**Fields:** `title`, `session` (target session name), `command` (text sent to session), `kind` (`tmux`), `sched_type` (`once`|`recurring`), `schedule_expr` (cron: `min hour dom month dow`), `run_at` (ISO datetime for one-time), `watch` (0/1 — watch output after send), `watch_timeout` (seconds), `done_pattern` (regex to detect completion), `done_action` (`disable`|`reschedule`)

---

## Notes (documents / reference material)

**Corrected 2026-08-28** — this section previously documented a
`/api/notes*` family that does not exist on the running server (confirmed
live: 404 on both `/api/notes` and `/api/notes/test`, and `GET
/api/debug/routes` lists no such family). Notes are backed by
`/api/memories` (the `memories` primitive) instead — see
`skills/amux-worker.md`'s "Gap found" section for the full investigation.

```bash
# List all notes
curl -sk $AMUX_URL/api/memories | python3 -m json.tool

# Read a note
curl -sk $AMUX_URL/api/memories/MEMORY_ID

# Create a note (global scope; memory_type "reference" fits documents best)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"scope":{"level":"global"},"name":"my-note","content":"# Title\n\nBody text here","memory_type":"reference"}' \
  $AMUX_URL/api/memories

# Update a note's content
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"content":"updated body"}' \
  $AMUX_URL/api/memories/MEMORY_ID

# Delete a note (soft-delete: content stays, deleted_at is set)
curl -sk -X DELETE $AMUX_URL/api/memories/MEMORY_ID
```

There is no pin verb on `/api/memories`. For a scripted client rather than
raw curl, `skills/amux-worker/scripts/amux-worker.sh notes <verb>` wraps
all of the above.

---

## Email (via Mail.app)

Accounts: ethan@mixpeek.com · esteininger21@gmail.com

```bash
# Read inbox (returns recent messages with subject, from, date, body, message_id)
curl -sk "$AMUX_URL/api/email/inbox?account=ethan@mixpeek.com&count=20&days=7"
# Params: account (filter to one account), count (max messages, default 20), days (lookback, default 7)

# Send email (validates email format, optional from account)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"to":"x@example.com","subject":"Hi","body":"...","from":"ethan@mixpeek.com"}' \
  $AMUX_URL/api/email/send

# Reply to an existing email (by message_id from inbox response)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"message_id":"<msg-id-from-inbox>","body":"Thanks!","reply_all":false}' \
  $AMUX_URL/api/email/reply

# Sync email → calendar events (background AI extraction)
curl -sk -X POST $AMUX_URL/api/email/sync

# Get extracted calendar events
curl -sk $AMUX_URL/api/email/events
```

**Workflow for replying:** call `/api/email/inbox` first to find the message, then use its `message_id` field in `/api/email/reply`.

---

## Browser Automation

**Live backend** — same verbs, executed in YOUR real Chrome (real logins, real IP). Opens a NEW tab (never touches existing tabs); first use needs one "Allow debugging?" click. Use when acting-as-you matters (SSO dashboards, bot-walled sites); the default profile backend is for parallel/unattended work.

```bash
curl -sk -X POST -H 'Content-Type: application/json' -d '{"backend":"live","url":"https://example.com"}' $AMUX_URL/api/browser/start
# navigate/screenshot/state/action/stop then work identically; live click takes {"selector":"..."} or x,y.
```

Shared Playwright instance with saved auth profiles.

```bash
# Start browser
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"profile":"default","url":"https://example.com"}' \
  $AMUX_URL/api/browser/start

# Navigate
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com"}' $AMUX_URL/api/browser/navigate

# Screenshot (returns JSON with path — use Read tool to view)
curl -sk $AMUX_URL/api/browser/screenshot

# Actions: click, type, key, scroll, eval
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"click","x":640,"y":400}' $AMUX_URL/api/browser/action

curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"type","text":"hello"}' $AMUX_URL/api/browser/action

curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"key","key":"Enter"}' $AMUX_URL/api/browser/action

curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"eval","script":"document.title"}' $AMUX_URL/api/browser/action

# AI agent — autonomous browser task
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"task":"Find the latest invoice","profile":"default"}' \
  $AMUX_URL/api/browser/agent

# List auth profiles
curl -sk $AMUX_URL/api/browser/profiles

# Stop browser
curl -sk -X POST $AMUX_URL/api/browser/stop
```

---

## CRM / People

```bash
# Add a contact
amux crm add "Name" company=X email=Y role=Z phone=P linkedin=L
# or: curl -sk -X POST -H 'Content-Type: application/json' \
#   -d '{"name":"Name","company":"X","notes":"context"}' \
#   $AMUX_URL/api/crm/contacts

# Update / view / log interaction / list / follow-ups
amux crm update PPL-1 email=Y company=Z
amux crm get PPL-1
amux crm log PPL-1 "discussed partnership"
amux crm list
amux crm fu
```

**When to use what:**
- Person / contact → `amux crm add`
- Document / reference → `/api/memories` (see Notes section above)
- Task / action item → `/api/board`
- Recurring automation → `/api/schedules`

---

## Determining the Current Session Name

```bash
echo $AMUX_SESSION
# or: tmux display-message -p '#S' | sed 's/^amux-//'
```

## Instructions

The user's request is: **$ARGUMENTS**

Parse the arguments to determine what the user wants:

- **`board`** or **`board list`** → list current board items, grouped by status
- **`board add <title>`** → add an item to the board; infer session from current tmux session
- **`board done <id>`** → mark an item done
- **`memory`** or **`memory show`** → show current session's memory content
- **`memory update`** → read the current MEMORY.md, extract useful facts from recent context, update via API
- **`sessions`** → list all amux sessions with their status
- **`schedule list`** → list all schedules
- **`schedule add <title>`** → create a new schedule interactively
- **`notes`** → list notes
- **`email send`** → compose and send an email
- **`browser`** → browser automation help
- **`crm`** → CRM operations
- **`help`** or empty → show a brief summary of available /amux commands and APIs
- **anything else** → interpret as a natural language amux action and execute it

Always:
1. Determine the current session name first (use `$AMUX_SESSION` or `tmux display-message` or ask)
2. Use `curl -sk` (self-signed cert)
3. Format output clearly — tables for lists, key facts for status
4. After adding/updating anything, confirm with the ID and brief summary
