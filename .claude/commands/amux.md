---
description: Interact with the amux multiplexer — board, memory, sessions, and more
allowed-tools: Bash, Read, Edit, Write
argument-hint: [board|memory|sessions|schedule|notes|email|browser|crm|help] [args...]
---

# /amux — amux Session Integration

You are running inside an **amux** managed Claude Code session. amux is a local multiplexer that manages multiple Claude sessions, a shared kanban board, notes, CRM, scheduler, email, browser automation, and per-session memory.

**Base URL:** `https://localhost:8822` (self-signed TLS — always use `curl -sk`)

---

## Board (tasks / issues)

```bash
# List all items
curl -sk https://localhost:8822/api/board | python3 -m json.tool

# Add item
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"title":"...", "desc":"...", "status":"todo", "session":"SESSION_NAME"}' \
  https://localhost:8822/api/board

# Update item
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"status":"doing"}' https://localhost:8822/api/board/ITEM_ID

# Delete item
curl -sk -X DELETE https://localhost:8822/api/board/ITEM_ID

# Claim a task atomically (prevents two sessions taking same task)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"session":"SESSION_NAME"}' https://localhost:8822/api/board/ITEM_ID/claim
```

Statuses: `backlog` · `todo` · `doing` · `done` (plus any custom columns)

---

## Sessions

```bash
# List sessions
curl -sk https://localhost:8822/api/sessions | python3 -m json.tool

# Send a message to a session
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"text":"your message"}' https://localhost:8822/api/sessions/SESSION_NAME/send

# Peek at a session's terminal output
curl -sk "https://localhost:8822/api/sessions/SESSION_NAME/peek?lines=100"
```

---

## Memory

```bash
# Read this session's memory
curl -sk https://localhost:8822/api/sessions/SESSION_NAME/memory

# Update this session's memory
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"content":"# My Notes\n..."}' https://localhost:8822/api/sessions/SESSION_NAME/memory

# Read/write global memory (shared across all sessions)
curl -sk https://localhost:8822/api/memory/global
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"content":"..."}' https://localhost:8822/api/memory/global
```

---

## Scheduler (recurring / one-time tasks)

Schedule commands to run in sessions at specific times or on a cron schedule.

```bash
# List all schedules
curl -sk https://localhost:8822/api/schedules | python3 -m json.tool

# Create a one-time schedule
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{
    "title": "Weekly analytics",
    "session": "gtm-videos",
    "command": "Run the weekly analytics report",
    "kind": "tmux",
    "sched_type": "once",
    "run_at": "2026-04-10T09:00"
  }' https://localhost:8822/api/schedules

# Create a recurring schedule (cron expression)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{
    "title": "Weekly video check",
    "session": "gtm-videos",
    "command": "Check video pipeline status and post summary to board",
    "kind": "tmux",
    "sched_type": "recurring",
    "schedule_expr": "0 9 * * 1"
  }' https://localhost:8822/api/schedules

# Update a schedule
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"enabled": 1}' https://localhost:8822/api/schedules/SCHED_ID

# Delete a schedule
curl -sk -X DELETE https://localhost:8822/api/schedules/SCHED_ID

# View recent runs
curl -sk https://localhost:8822/api/schedules/runs | python3 -m json.tool

# Trigger a schedule immediately
curl -sk -X POST https://localhost:8822/api/schedules/SCHED_ID/run
```

**Fields:** `title`, `session` (target session name), `command` (text sent to session), `kind` (`tmux`), `sched_type` (`once`|`recurring`), `schedule_expr` (cron: `min hour dom month dow`), `run_at` (ISO datetime for one-time), `watch` (0/1 — watch output after send), `watch_timeout` (seconds), `done_pattern` (regex to detect completion), `done_action` (`disable`|`reschedule`)

---

## Notes (documents / reference material)

```bash
# List all notes
curl -sk https://localhost:8822/api/notes | python3 -m json.tool

# Read a note
curl -sk https://localhost:8822/api/notes/my-note

# Create or update a note (use slug as path)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"content":"# Title\n\nBody text here"}' \
  https://localhost:8822/api/notes/my-note

# Delete a note (moves to trash)
curl -sk -X DELETE https://localhost:8822/api/notes/my-note

# Pin/unpin a note
curl -sk -X POST https://localhost:8822/api/notes/my-note/pin
```

---

## Email (via Mail.app)

Accounts: ethan@mixpeek.com · esteininger21@gmail.com · beefinethan@icloud.com

```bash
# Read inbox (returns recent messages with subject, from, date, body, message_id)
curl -sk "https://localhost:8822/api/email/inbox?account=ethan@mixpeek.com&count=20&days=7"
# Params: account (filter to one account), count (max messages, default 20), days (lookback, default 7)

# Send email (validates email format, optional from account)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"to":"x@example.com","subject":"Hi","body":"...","from":"ethan@mixpeek.com"}' \
  https://localhost:8822/api/email/send

# Reply to an existing email (by message_id from inbox response)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"message_id":"<msg-id-from-inbox>","body":"Thanks!","reply_all":false}' \
  https://localhost:8822/api/email/reply

# Sync email → calendar events (background AI extraction)
curl -sk -X POST https://localhost:8822/api/email/sync

# Get extracted calendar events
curl -sk https://localhost:8822/api/email/events
```

**Workflow for replying:** call `/api/email/inbox` first to find the message, then use its `message_id` field in `/api/email/reply`.

---

## Browser Automation

Shared Playwright instance with saved auth profiles.

```bash
# Start browser
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"profile":"default","url":"https://example.com"}' \
  https://localhost:8822/api/browser/start

# Navigate
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com"}' https://localhost:8822/api/browser/navigate

# Screenshot (returns JSON with path — use Read tool to view)
curl -sk https://localhost:8822/api/browser/screenshot

# Actions: click, type, key, scroll, eval
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"click","x":640,"y":400}' https://localhost:8822/api/browser/action

curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"type","text":"hello"}' https://localhost:8822/api/browser/action

curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"key","key":"Enter"}' https://localhost:8822/api/browser/action

curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"action":"eval","script":"document.title"}' https://localhost:8822/api/browser/action

# AI agent — autonomous browser task
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"task":"Find the latest invoice","profile":"default"}' \
  https://localhost:8822/api/browser/agent

# List auth profiles
curl -sk https://localhost:8822/api/browser/profiles

# Stop browser
curl -sk -X POST https://localhost:8822/api/browser/stop
```

---

## CRM / People

```bash
# Add a contact
amux crm add "Name" company=X email=Y role=Z phone=P linkedin=L
# or: curl -sk -X POST -H 'Content-Type: application/json' \
#   -d '{"name":"Name","company":"X","notes":"context"}' \
#   https://localhost:8822/api/crm/contacts

# Update / view / log interaction / list / follow-ups
amux crm update PPL-1 email=Y company=Z
amux crm get PPL-1
amux crm log PPL-1 "discussed partnership"
amux crm list
amux crm fu
```

**When to use what:**
- Person / contact → `amux crm add`
- Document / reference → `/api/notes`
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
