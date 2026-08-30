---
description: Use when a Claude Code, Codex, or Gemini CLI worker needs to interact with amux from the command line — read/update the board, inspect or message sessions, read/write memory, or manage notes via the amux API.
allowed-tools: Bash, Read
argument-hint: <family> <verb> [args...]
---

# amux-worker — cross-CLI amux API client (Phase 0 / MVP)

A bash+curl+python3 client any worker CLI can drive — **Claude Code, Codex,
Gemini CLI, or a bare shell**. Unlike `/amux` and `/amux-board` (which are
Claude Code slash commands with `$ARGUMENTS` interpolation), this skill has
no dependency on Claude Code's slash-command machinery: the script at
`skills/amux-worker/scripts/amux-worker.sh` is a standalone CLI. Point any
agent harness at this file and it can run the commands below directly.

No `jq` dependency — everything goes through `python3` (present everywhere
amux runs), following the convention in `skills/amux.md`.

## Quick start

```bash
skills/amux-worker/scripts/amux-worker.sh help
skills/amux-worker/scripts/amux-worker.sh health
skills/amux-worker/scripts/amux-worker.sh board list
```

Run from the amux repo root (or use an absolute path to the script — it has
no other path dependency).

## Auth / identity

- **Base URL**: reads `$AMUX_URL`; falls back to `` `amux url` `` (reads
  `~/.amux/endpoint.json`), then to `https://localhost:8824`. TLS is
  self-signed — the script always uses `curl -sk`, matching every other
  amux skill in this repo. There is no bearer-token auth; amux's request
  attribution is via the `X-Amux-Worker` header, not a credential.
- **Worker identity**: mutating calls (board add/status/send/memory-set)
  carry `X-Amux-Worker: $AMUX_WORKER`, falling back to `$AMUX_SESSION`
  (set for Claude Code lanes running inside amux), falling back to the
  literal `amux-worker-skill`. Set `AMUX_WORKER` explicitly for a
  Codex/Gemini worker that has neither env var, so its writes are
  attributable in `/api/logs/analyze` instead of landing as anonymous.

## Commands

### Board — `board <verb> ...`

```bash
amux-worker.sh board list [status]              # e.g. "board list doing"
amux-worker.sh board get <id>
amux-worker.sh board add <title> [desc] [type]  # status defaults to todo
amux-worker.sh board status <id> <new-status>   # see GATES below
amux-worker.sh board claim <id> [session]       # atomic; defaults session to $AMUX_WORKER
amux-worker.sh board delete <id>
```

**Gates.** Non-`backlog`/`todo` transitions on gated types (the default
type `code`, plus most others) are blocked unless the request acknowledges
the type's gate — `PATCH .../board/{id}` with a bare `{"status": "doing"}`
comes back `409 gate_blocked` naming the unmet criteria. `board status`
auto-sends `gate_ack: true` (acknowledge the whole gate at once) so a
worker's status moves aren't blocked by default. That is deliberately the
loose mechanism: it's `gate_ack`, not a per-criterion `gate_checked` array,
so it satisfies the gate without auditing individual criteria. For a
tighter ack, or to move a card straight to `done` (which additionally
needs a `source_ref` pointing at what was produced — a URL, file path,
commit sha, or PR/issue number, and cannot be satisfied by `gate_ack`),
call the raw API directly:

```bash
curl -sk -X PATCH -H 'Content-Type: application/json' \
  -H "X-Amux-Worker: $AMUX_WORKER" \
  -d '{"status":"done","gate_checked":["criterion 1","criterion 2"],"source_ref":"#163"}' \
  "$AMUX_URL/api/board/ITEM_ID"
```

`GET /api/board/contract?card=ITEM_ID` returns the resolved gate for a
specific card (the bare `/api/board/contract` only lists type defaults).

### Sessions — `sessions <verb> ...`

```bash
amux-worker.sh sessions list
amux-worker.sh sessions status <name>        # full record — same shape as one row of `list`
amux-worker.sh sessions peek <name> [lines]  # default 80 lines
amux-worker.sh sessions send <name> <text>   # injects text into that session's live agent
```

`sessions send` delivers into a **real, running agent session** — use it
deliberately, not for smoke-testing (verified via code read in this
implementation pass, not fired against a live human lane, to avoid
disrupting anyone's work).

`sessions peek`'s `[lines]` arg only bounds the `output` field (the current
terminal viewport). The response's `history` field is a separate, unbounded
backscroll transcript (ANSI codes included) that `lines` does not trim —
confirmed live: `peek amux 5` still returned ~1700 lines of `history`. Read
`output` for "what's on screen now"; only read `history` when you actually
need backscroll, since it can be large.

### Memory — `memory <verb> ...`

Per-session memory files plus one shared global memory doc.

```bash
amux-worker.sh memory get [session]           # default: $AMUX_SESSION
amux-worker.sh memory set [session] <content> # OVERWRITES — read first if appending
amux-worker.sh memory global get
amux-worker.sh memory global set <content>
```

### Notes — `notes <verb> ...`

**There is no `/api/notes` route on this server** — `skills/amux.md`
documents one (`GET/POST/DELETE /api/notes*`, with a pin verb) but it
404s live; see **Gap found** below. Phase 0 backs "notes" with
`/api/memories`, the `memories` primitive listed in the top-level
`CLAUDE.md` — structured, versioned, scope-resolved (org/global/group/
worker) entries with a `memory_type` (`reference` fits documents/runbooks
best; also `project`, `user`, `feedback`).

```bash
amux-worker.sh notes list
amux-worker.sh notes get <id>
amux-worker.sh notes create <name> <content> [type]   # type defaults to "reference"
amux-worker.sh notes update <id> <content>
amux-worker.sh notes delete <id>                       # soft-delete: content stays, deleted_at is set
```

`notes create` always creates at `{"level":"global"}` scope (visible to
everyone) for simplicity. For worker- or group-scoped notes, call
`POST /api/memories` directly with `"scope":{"level":"worker","id":"wrk_..."}`
(a `wrk_`-prefixed id from `/api/workers` — **not** a tmux session name;
see Gaps below) or `{"level":"group","id":"grp_..."}`.

### Health — `health`

```bash
amux-worker.sh health
```

Wraps `GET /health`. For deeper diagnostics, go straight to the endpoints
in the top-level `CLAUDE.md`'s Observability table
(`/api/logs/analyze`, `/api/health/invariants`, `/api/debug/routes`, …) —
Phase 0 wraps only the basic check.

## Gap found while implementing (route drift)

`skills/amux.md` (the existing `/amux` slash command) documents a Notes
API — `GET/POST/DELETE /api/notes[/{slug}]` and `POST /api/notes/{slug}/pin`
— that **does not exist** on the running server: `GET /api/debug/routes`
lists no `/api/notes*` family, and a live `curl` returns `404 {"error":
"not found"}` for both `/api/notes` and `/api/notes/test`. There is no
trace of a notes concept anywhere in `crates/amux-server/src/api/` or
`crates/amux-dashboard/static/app.js` either — it isn't a renamed route,
it's gone. `/api/memories` (the `memories` primitive) is the closest live
analog and is what this skill uses; `skills/amux.md` itself was not
touched by this task (out of scope — see below) but is now known-stale on
this one point and should be corrected or pointed at `/api/memories` in a
follow-up.

## Phase 1 candidates (not implemented here)

- **Worker-scoped notes by session name.** `/api/memories`'s worker scope
  needs a `wrk_`-prefixed id from the Rust-managed `/api/workers` store;
  this repo's actual sessions are tmux-backed (`/api/sessions`, plain
  names like `amux`). There is no bridge from a tmux session name to a
  `wrk_` id today, so `notes create` can't target "this session's notes"
  directly — only global scope is wired up.
  Confirmed live: `curl -sk $AMUX_URL/api/workers` in this run returned an
  empty store while `/api/sessions` listed 9 tmux sessions, i.e. no
  session here currently has a matching worker row — the gap is real, not
  hypothetical, for this deployment.
- **`board update`** for arbitrary PATCH bodies (tags, session, reviewer,
  `depends_on`, …) beyond the `status`/`claim` helpers here.
- **Schedules and CRM** families exist in `skills/amux.md` and were out of
  Phase 0 scope; fold into this script if workers start needing them from
  outside Claude Code.
- **`sessions send` dry-run / confirmation** — right now it fires
  immediately; worth a `--confirm` guard given it injects into a live
  agent's input.
- Fixing `skills/amux.md`'s stale Notes section (or replacing it with a
  pointer to this skill) is a two-minute follow-up once someone signs off
  on retiring the old API description.
