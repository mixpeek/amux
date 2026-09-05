---
description: Use when you need to interact with the amux system — manage board tasks, check sessions/workers, send emails, message via Telegram, automate browsers, work with CRM contacts, or use journal/files/org/torrents/graph/SQL/dictation
allowed-tools: Bash, Read, Edit, Write
argument-hint: [board|memory|sessions|workers|schedule|notes|email|gmail|calendar|telegram|journal|files|org|groups|torrents|graph|map|sql|dictation|tts|habits|review|connectors|browser|crm|help] [args...]
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

**Modern equivalent:** `/api/workers/*` is the canonical, still-growing
replacement for parts of this API — `send` in particular is "the canonical
spelling... promoted out of the catch-all" per its own source comment.
Prefer it for new code; `/api/sessions/*` still works (legacy alias) and
isn't going anywhere yet.

```bash
curl -sk $AMUX_URL/api/workers | python3 -m json.tool          # list
curl -sk $AMUX_URL/api/workers/WORKER_ID                       # get one
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"desc":"..."}' $AMUX_URL/api/workers/WORKER_ID           # update
curl -sk -X POST $AMUX_URL/api/workers/WORKER_ID/start          # start
curl -sk -X POST $AMUX_URL/api/workers/WORKER_ID/stop           # stop
curl -sk $AMUX_URL/api/workers/WORKER_ID/peek                   # peek
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"text":"your message"}' $AMUX_URL/api/workers/WORKER_ID/send
curl -sk $AMUX_URL/api/workers/WORKER_ID/dead-letters            # undelivered sends
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

## Gmail (OAuth — distinct from the Email/Mail.app section above)

This talks to the Gmail API directly via OAuth, not Mail.app — a separate,
per-account credential path. Use this when you specifically need Gmail
(labels, thread view) rather than whatever's in Mail.app. Live on this
server (`crates/amux-server/src/api/gmail.rs` + `gmail_auth.rs`); undocumented
here until 2026-08-30 despite being real — verify against `GET
/api/debug/routes` before trusting any *other* connector section below, since
several (Calendar, Secrets, GitHub, Mattermost) describe features that exist
only on still-open PR branches and are NOT live on this checkout.

```bash
# Connected accounts / start OAuth flow (returns a URL to open) / disconnect
curl -sk $AMUX_URL/api/gmail/accounts
curl -sk $AMUX_URL/api/gmail/auth
curl -sk -X DELETE $AMUX_URL/api/gmail/account

# Inbox / labels / a specific thread
curl -sk $AMUX_URL/api/gmail/inbox
curl -sk $AMUX_URL/api/gmail/labels
curl -sk $AMUX_URL/api/gmail/thread/THREAD_ID

# Send
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"to":"x@example.com","subject":"Hi","body":"..."}' \
  $AMUX_URL/api/gmail/send
```

---

## Calendar (plain events store)

A CRUD store for calendar events (`crates/amux-server/src/api/calendar.rs`),
independent of any external calendar account — not to be confused with
the email-sync-extracted events under `/api/email/events` above, or with
a **full two-way Google Calendar sync**, which does not exist on this
server yet (see "Not yet deployed" below). This one just holds events you
create directly and publishes them as a read-only `.ics` feed real
calendars (Google, Apple) can subscribe to — one-way out, nothing syncs
back in.

```bash
# List / create / update / delete
curl -sk $AMUX_URL/api/cal-events
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"title":"Standup","start":"2026-09-01T09:00:00Z","end":"2026-09-01T09:15:00Z","location":"...","description":"...","rrule":"...","all_day":false}' \
  $AMUX_URL/api/cal-events
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"location":"Room 2"}' $AMUX_URL/api/cal-events/EVT_ID
curl -sk -X DELETE $AMUX_URL/api/cal-events/EVT_ID
```

`title` and `start` are the only required fields on create; everything
else (`end`, `location`, `description`, `rrule`, `all_day`) is optional.
`PATCH` accepts any subset of the same fields.

**The `.ics` feed:** `GET /api/calendar.ics` — see CLAUDE.md's "iCal sync"
section. Events only (not schedules/board), and its real subscription URL
(S3-hosted, random key) lives only in `server.env` — never commit the
actual URL, the repo is public.

---

## Not yet deployed (exists in code, not live on this server)

Three connectors have real, merged-or-in-review code but **are not
reachable on the currently running server** — verify against `GET
/api/debug/routes` before trusting any of this, and don't assume "merged
to `main`" means "live": this checkout's build source only advances when
someone explicitly moves it there (CLAUDE.md's Deploy section) — a PR
merging to `main` does not redeploy the running binary.

- **Mattermost** (`/api/connectors/mattermost/*`) — login/password auth
  against a self-hosted server, via the generic `/api/connectors/*`
  family (also used for Google/Slack-shaped auth). Code merged to `main`
  as of PR #164 (2026-08-30) but not yet built into the running server.
- **Encrypted secrets store** (`/api/secrets/*`) — age/X25519 at rest,
  decrypted once at startup. Still an open PR (#163) as of 2026-08-30.
- **Full two-way Google Calendar sync** (`/api/gcal/*`) — distinct from
  the plain `/api/cal-events` store above; syncs read/write against the
  real Google Calendar API, multi-account. Still an open PR (#160) as of
  2026-08-30, and also an open product question (amux already has a
  one-way `.ics` feed — whether it should also own two-way write access
  to a real calendar is not yet decided).

Once any of these actually lands on the running server, give it its own
section above (following the Gmail/Telegram pattern) rather than just
deleting this note — the next person needs to know it changed, not just
that it now works.

---

## Telegram

Bot connector — **inbound** via long-polling
(`runtime_jobs::telegram_poll`; `GET /api/telegram/status` reports
last-poll time/error and routing counts), **outbound** via
`POST /api/telegram/send`. The bot token itself is a connector credential
(`api/connectors.rs`'s `telegram` row, `TELEGRAM_BOT_TOKEN` env — set it via
`POST /api/connectors/telegram/credentials`), not managed by this section.

**Linking a chat to a session** happens two ways:
- From Telegram itself: send `/link <session-name>` to the bot in that chat.
  The bot validates the session exists (against the live lane list) and
  confirms in-chat.
- From the API — e.g. pre-linking a `chat_id` found via Telegram's own
  `getUpdates` before the bot has received anything from it:
  `POST /api/telegram/mappings`.

Once linked, any other text sent to the bot in that chat is routed into the
mapped session, stamped `[from Telegram @username]: ...` so the session can
tell a Telegram message apart from other input arriving in its pane. A chat
that sends text before linking gets a one-line nudge back (`/link
<session-name>` first), never silently dropped.

**Routing to a specific lane from Telegram** — start a message with `@lane_name` to
route it to that lane instead of your default mapped session. Example: `@frontstage
what's the status?` sends the message to the `frontstage` session. If the lane name
is unknown or misspelled, the message still routes to your mapped session (never
silently dropped) but you get an inline reply back naming the bad mention and
listing known lanes — e.g. `@fronstage` (typo for `frontstage`) replies with
`Note: '@fronstage' isn't a known lane — delivered to 'amux' instead. Known lanes: ...`
rather than silently landing in the wrong place with no signal (fixed 2026-08-30,
found via a real typo'd message during testing).

```bash
curl -sk $AMUX_URL/api/telegram/status                          # bot_token_set, mapping_count, last_poll_at, last_error, messages_routed/unlinked
curl -sk $AMUX_URL/api/telegram/mappings                        # list chat<->session links
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"chat_id":123456,"session":"SESSION_NAME"}' \
  $AMUX_URL/api/telegram/mappings                                # manually link a chat_id to a session
curl -sk -X DELETE $AMUX_URL/api/telegram/mappings/CHAT_ID        # unlink

# Outbound: session -> Telegram. Exactly one of session (resolved to its
# most-recently-linked chat) or chat_id.
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"session":"SESSION_NAME","text":"your message"}' \
  $AMUX_URL/api/telegram/send
```

**A session's outbound target is whichever chat linked it most recently** —
if two chats `/link` the same session, the newer link wins for
`{"session":...}` sends (both chats still deliver inbound either way
regardless). Pass `chat_id` directly in `/send` to disambiguate or to reach
a chat that never linked a session at all.

There is no automatic "every session message also goes to Telegram" wiring —
`POST /api/telegram/send` is an explicit call a session or hook makes, not a
background forwarder (ethos rule 8: which events go to which chat is the
operator's call, not a default this connector should assume).

**Any worker can reply to Telegram** — the connector is available to all sessions
equally. If you want your session to send replies back to Telegram (e.g., in response
to messages routed via `@your-session-name` mentions), call the send endpoint:

```bash
# From your session: reply to a message routed from Telegram
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"session":"your-session-name","text":"Your reply here"}' \
  $AMUX_URL/api/telegram/send

# Or target a specific chat directly (if you know the chat_id)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"chat_id":123456,"text":"Direct message"}' \
  $AMUX_URL/api/telegram/send

# With HTML formatting (bold, italic, code, etc.)
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"session":"your-session-name","text":"<b>Bold</b> reply","parse_mode":"HTML"}' \
  $AMUX_URL/api/telegram/send
```

Messages sent from your session resolve to the most-recently-linked chat for that
session (if multiple chats linked the same session, the newest one wins). The API
returns `{"sent": true}` on success or an error if the chat doesn't exist or the
network fails. Ethos rule 8 applies: **you decide when and what to send** — no
automatic forwarding.

If `/link` or a manual mapping ever comes back with a write/database error,
that is a real bug, not transient contention — every DB write in this
codebase goes through the single writer thread (`Store::write_async`, see
`db/mod.rs`); a `state.store.read()` connection has `query_only=ON`
permanently and cannot legitimately succeed on retry where it just failed.

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
- Telegram chat <-> session link, or a message out to Telegram → `/api/telegram/*`
- Gmail specifically (not Mail.app) → `/api/gmail/*`
- Standalone calendar event (not a board/schedule item) → `/api/cal-events/*`
- Mattermost, secrets, or full two-way Google Calendar sync → not live yet, see "Not yet deployed" above
- Journal entry, org/team, files, torrents, graph/map, SQL, dictation/TTS, habits/review/local-models → see the sections below

---

## Journal

`/api/journal` over the live `journal_entries`/`journal_media` tables —
distinct from Notes (`/api/memories`, reference documents) and from CRM
interaction logs (`amux crm log`). Entry ids are `JRN-N`.

```bash
curl -sk $AMUX_URL/api/journal                          # list
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"title":"...","body":"..."}' $AMUX_URL/api/journal
curl -sk $AMUX_URL/api/journal/JRN_ID                    # get one
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"body":"..."}' $AMUX_URL/api/journal/JRN_ID
curl -sk -X DELETE $AMUX_URL/api/journal/JRN_ID           # soft delete
curl -sk $AMUX_URL/api/journal/tags                       # tag counts
```

**Media upload is JSON + base64, NOT multipart** (a different shape from
the Files section below — this matches what the dashboard's journal tab
actually sends):

```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"data":"data:image/png;base64,iVBORw0K...","name":"photo.jpg"}' \
  $AMUX_URL/api/journal/JRN_ID/media
curl -sk $AMUX_URL/api/journal/media/MEDIA_ID              # serve
curl -sk -X DELETE $AMUX_URL/api/journal/media/MEDIA_ID     # hard delete (file + row)
```

---

## Files — two DIFFERENT contracts, both intentionally mounted

`/api/fs/*` and `/api/files/*` are NOT duplicates — different roots,
different upload shapes, kept side by side on purpose
(`docs/rust-migration/server-boundary.md`). Pick by shape, not habit.

```bash
# /api/fs — Python-parity, path-containment-checked, MULTIPART upload
curl -sk "$AMUX_URL/api/fs/list?path=."
curl -sk -X POST -H 'Content-Type: application/json' -d '{"path":"..."}' $AMUX_URL/api/fs/mkdir
curl -sk -X POST -F "file=@local.txt" -F "dir=/some/dir" $AMUX_URL/api/fs/upload
curl -sk -X POST -H 'Content-Type: application/json' -d '{"path":"..."}' $AMUX_URL/api/fs/open
curl -sk -X POST -H 'Content-Type: application/json' -d '{"from":"a","to":"b"}' $AMUX_URL/api/fs/rename

# /api/files — rooted at $HOME (override AMUX_FILES_ROOT), RAW BODY upload,
# 50MB cap both ways (413 beyond)
curl -sk "$AMUX_URL/api/files?path=some/dir"                       # listing
curl -sk "$AMUX_URL/api/files/download?path=some/file" -o out      # raw bytes
curl -sk -X POST --data-binary @local.txt "$AMUX_URL/api/files/upload?path=dir/file"
```

---

## Org / Team

`/api/org` — a single-workspace org model (multi-workspace is not a thing
here, one org per install). `GET` lazily creates the singleton row on
first call. Invites expire in 7 days.

```bash
curl -sk $AMUX_URL/api/org                               # gets or creates
curl -sk -X PATCH -H 'Content-Type: application/json' -d '{"name":"..."}' $AMUX_URL/api/org
curl -sk $AMUX_URL/api/org/members
curl -sk -X DELETE $AMUX_URL/api/org/members/MEMBER_ID
curl -sk $AMUX_URL/api/org/invites                        # excludes used/expired
curl -sk -X POST -H 'Content-Type: application/json' -d '{"email":"..."}' $AMUX_URL/api/org/invites
curl -sk -X DELETE $AMUX_URL/api/org/invites/TOKEN
```

## Groups

Fleet groups — **derived live from each session's `CC_TAGS` env var, never
a stored list** (a second table would drift the moment someone edited one
side and not the other). A session caller (`X-Amux-Worker` header) only
sees same-tag sessions; the dashboard (no header) sees everything.

```bash
curl -sk $AMUX_URL/api/groups                              # list + config-usage hint
curl -sk $AMUX_URL/api/groups/GROUP_NAME/config
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -d '{"goal":"...","department":"sales","kpis":[...],"human_cost":300000}' \
  $AMUX_URL/api/groups/GROUP_NAME/config
```

---

## Torrents

`/api/torrents` drives a local aria2c JSON-RPC daemon. **Every route
honestly 503s with the exact start command if aria2c isn't running** —
that's the "how do I use this" answer on a fresh box, not an error to work
around.

```bash
curl -sk $AMUX_URL/api/torrents                            # list
curl -sk -X POST -H 'Content-Type: application/json' -d '{"uri":"magnet:..."}' $AMUX_URL/api/torrents
curl -sk $AMUX_URL/api/torrents/config                      # download dir etc
curl -sk -X DELETE $AMUX_URL/api/torrents/GID
curl -sk "$AMUX_URL/api/torrents/GID/file" -o out            # range-request streaming
curl -sk -X POST $AMUX_URL/api/torrents/GID/pause            # or resume, etc (action)
```

---

## Graph (mind-map) / Map (location)

Two unrelated "map" concepts sharing naming — `/api/graph` is a node/edge
mind-map (Obsidian-vault-importable); `/api/map` is a location-pin map
backed by a plain JSON file, not a table.

```bash
# Graph: /api/graph/fleet is PROJECTED live from sessions, never stored
curl -sk $AMUX_URL/api/graph/fleet
curl -sk $AMUX_URL/api/graph/GRAPH_ID                       # {"nodes":[...],"edges":[...]}
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"path":"/path/to/vault"}' $AMUX_URL/api/graph/GRAPH_ID/import-vault  # REPLACES the graph

# Map: whole-document GET/POST, additive pin POST, place search
curl -sk $AMUX_URL/api/map
curl -sk -X POST -H 'Content-Type: application/json' -d '{"pins":[...],...}' $AMUX_URL/api/map
curl -sk -X POST -H 'Content-Type: application/json' -d '{"lat":0,"lng":0,"label":"..."}' $AMUX_URL/api/map/pins
curl -sk "$AMUX_URL/api/map/search?q=coffee+near+me"          # Google Places, falls back to Nominatim
```

---

## SQL browser — raw access to the live database, use with care

`/api/sql` runs against the SAME SQLite database backing board/sessions/
everything else in this file. **The read/write split is enforced by
SQLite itself, not by inspecting your query text**: a non-`write` request
runs on a connection opened `SQLITE_OPEN_READ_ONLY`, so the engine refuses
any write outright — there's no keyword blocklist to route around, but
also no correctness guarantee beyond "did you set `write` honestly."

```bash
curl -sk $AMUX_URL/api/sql/schema                          # table+column list
curl -sk "$AMUX_URL/api/sql/rows?table=board"                # browse one table
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT * FROM board LIMIT 5","write":false}' $AMUX_URL/api/sql
```

Prefer the purpose-built API (`/api/board`, `/api/cal-events`, etc.) for
anything that has one — this is for the cases nothing else covers.

---

## Dictation / Text-to-speech

Both are **honest-503 if their engine isn't configured** — audio/text is
never fabricated. Dictation: Whisper (local, warm subprocess) preferred,
Gemini as fallback/AI-edit engine. TTS: `AMUX_TTS_ENGINE` (`say`|`piper`|
`auto`) — macOS `say` by default where present, Piper elsewhere.

```bash
curl -sk -X POST --data-binary @audio.wav $AMUX_URL/api/dictate    # one-shot transcribe
curl -sk $AMUX_URL/api/dictation/history
curl -sk $AMUX_URL/api/dictation/dict                        # custom-word dictionary
curl -sk $AMUX_URL/api/dictation/config

curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"text":"Hello","voice_id":"..."}' $AMUX_URL/api/tts    # -> {url,size,engine,voice}
curl -sk $AMUX_URL/api/tts/voices
```

---

## Small utility APIs

Thin, single-purpose — one JSON blob or a couple of routes each.

```bash
# Habits — one JSON array, whole-document replace (PUT overwrites everything)
curl -sk $AMUX_URL/api/habits
curl -sk -X PUT -H 'Content-Type: application/json' -d '[{"name":"...","...":"..."}]' $AMUX_URL/api/habits

# Weekly review — trend data + a markdown digest
curl -sk "$AMUX_URL/api/review/week?days=7"
curl -sk "$AMUX_URL/api/review/digest?file=..."

# Locally installed Ollama models — empty array (never an error) if the
# daemon/binary isn't present
curl -sk $AMUX_URL/api/ollama/models
```

---

## Connectors (generic OAuth/credential framework)

The framework Mattermost/Gmail-adjacent auth sits on top of — see "Not yet
deployed" above for which providers are actually live. `GET
/api/connectors` shows every registered provider's connection status and
which env vars it needs (masked, never the value).

```bash
curl -sk $AMUX_URL/api/connectors                          # every provider + status
curl -sk $AMUX_URL/api/connectors/accounts                  # connected accounts, all providers
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"key":"..."}' $AMUX_URL/api/connectors/PROVIDER_ID/credentials
curl -sk -X POST $AMUX_URL/api/connectors/PROVIDER_ID/test
```

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
- **`workers`** → list/get/update workers via the modern `/api/workers` API
- **`schedule list`** → list all schedules
- **`schedule add <title>`** → create a new schedule interactively
- **`notes`** → list notes
- **`email send`** → compose and send an email
- **`gmail`** → Gmail OAuth account status / inbox / send
- **`calendar`** or **`cal`** → list/create/update/delete a standalone event via `/api/cal-events`
- **`telegram`** → Telegram bot status / link a chat to a session / send a message via `/api/telegram`
- **`journal`** → journal entries via `/api/journal`
- **`files`** → browse/upload/download via `/api/fs` or `/api/files` (see Files section for which)
- **`org`** → org/team/members/invites via `/api/org`
- **`groups`** → fleet group list/config via `/api/groups`
- **`torrents`** → torrent status/add/control via `/api/torrents` (needs aria2c running)
- **`graph`** or **`map`** → mind-map (`/api/graph`) or location map (`/api/map`)
- **`sql`** → browse schema/rows or run a query via `/api/sql`
- **`dictation`** or **`tts`** → transcribe audio or synthesize speech
- **`habits`** or **`review`** → habit tracking / weekly review data
- **`connectors`** → generic connector framework status via `/api/connectors`
- **`browser`** → browser automation help
- **`crm`** → CRM operations
- **`help`** or empty → show a brief summary of available /amux commands and APIs
- **anything else** → interpret as a natural language amux action and execute it

Always:
1. Determine the current session name first (use `$AMUX_SESSION` or `tmux display-message` or ask)
2. Use `curl -sk` (self-signed cert)
3. Format output clearly — tables for lists, key facts for status
4. After adding/updating anything, confirm with the ID and brief summary
