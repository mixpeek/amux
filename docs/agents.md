# amux for agents

You are talking to an amux server. amux runs a fleet of coding-agent workers
(Claude Code, Codex, Gemini and others) on one machine and gives them, and you,
an HTTP API over everything they share. This page is served at `/llms.txt`, in
`GET /api/discover` (as `guide`) and as the MCP resource `amux://guide`. The
rest of the catalog is generated from the code at build time, so it is never
older than the server you are talking to.

## Find your way

| Need | Ask |
|---|---|
| Every API family with a one-line summary | `GET /api/discover` |
| One family: its routes, methods and the docs of the code that serves them | `GET /api/discover/{family}` (for example `crm`, `board`, `workers`) |
| Where something is, by words | `GET /api/discover/search?q=send+email` |
| The same through MCP | `POST /api/mcp-server` (tools `discover`, `search`, `call`, `troubleshoot`, `events`) |
| Every route with its methods | `GET /api/debug/routes` |
| The events you can subscribe to, with their fields | `GET /api/subscriptions/catalog` |
| The CLI | `amux help` (and `amux help <verb>`) |

Read a family's docs before calling it: they name each route's body, the
refusal codes it returns, and what to do about each.

## Where amux is and who you are

- **Base URL.** Inside a worker: `$AMUX_URL`, or `amux url`. It is HTTPS with a
  self-signed certificate: use `curl -sk`, or skip verification for this host.
- **A worker** sends `X-Amux-Session: <its worker name>` on every call
  (`$AMUX_SESSION`). That is how amux attributes what you did and applies your
  worker's limits. Never send another worker's name.
- **The owner** (the person who runs this amux) authenticates with a bearer
  token. Owner-only actions (approving outbound, signing a deal, owner-tier
  changes) refuse a worker with 403 and say so.
- **From outside this machine**, use the owner's token or a member login; the
  ChatGPT app uses OAuth at `/mcp`.

## The primitives

Everything amux does is one of eight things. Map a task onto them before
looking for a special endpoint:

| Primitive | What it is | Families |
|---|---|---|
| Workers | the agent sessions: start, stop, read, message | `workers` (`sessions` is the old name), `peek`, `messages`, `conversations` |
| Board | tasks with status, evidence and gates | `board`, `board-groom`, `board-lifecycle`, `needs-input` |
| Schedulers | anything at a time or on a cadence (`kind: shell` runs a command with no model) | `schedules` |
| Filesystem | files and folders, and links to them | `fs`, `file`, `files`, `upload`, `pages` |
| Groups | which workers belong together and may message each other | `groups` |
| Memories | what a worker carries across sessions | `memories`, `memory` |
| Environment | settings at global, group or worker scope | `scope`, `env`, `prefs`, `settings` |
| Messages | worker to worker, and the owner's channels | `messages`, `conversations`, `bridge`, `channels` |

On top of them sit integrations (`email`, `gmail`, `browser`, `calls`,
`connectors`, `social`), the CRM (`crm`), and the harness that watches it all
(`health`, `logs`, `debug`, `audit`, `traces`, `land`).

## How every route behaves

- **JSON in, JSON out.** A write returns `{"ok": true, ...}` or the created
  record with its `id`.
- **A refusal is a 4xx with `{"error": "<a sentence>", "code": "<a verdict>"}`.**
  The sentence says why and usually what would let it through. Do not retry a
  refusal unchanged: fix what it names, or tell the owner.
- **Every diagnostic answers `measured`** (did the probe run) **and
  `n_considered`** (how big the population was), and `why_unmeasured` when it
  could not run. A zero with `measured: false` means nobody looked; it is not
  good news.
- **Anything a person outside reads waits for the owner.** Email, DMs,
  comments, posts and calls are held for approval; your send returns a held
  record, and an event says when it was approved. Do not route around a hold.
- **Every write is attributed** to the worker header or the owner, and
  logged with a `verdict` you can find in `/api/logs/analyze`.
- **Events.** Most state changes emit an event (`crm.touch.due`,
  `board.card.moved`, `email.received`, ...). Subscribe with
  `POST /api/subscriptions` or `amux subscribe "<plain words>"` instead of
  polling.

## Troubleshoot

When something does not work, in this order:

1. `GET /health`: is the server up, and which `commit` is it running.
2. `GET /api/pull`: is that the newest main, or is a deploy in progress.
3. `GET /api/health/invariants`: what the server knows is wrong right now.
4. `GET /api/logs/analyze?since_h=1`: error groups with their verdicts.
5. The refusal itself: its `code` names the rule; search it with
   `GET /api/discover/search?q=<code>` to find the family whose docs explain it.
6. `GET /api/debug/routes`: whether the route you called exists, and its methods.
7. Your own worker: `GET /api/workers/{you}` for status, limits and why it is waiting.

The MCP tool `troubleshoot` runs steps 1 to 4 and returns them together.

Raw logs live in `~/.amux/logs/server-rs.log` on the host (grep with `-a`).

## Use it well

- Discover first, then call. The catalog is cheaper than guessing paths.
- Prefer the primitive to a workaround: a schedule over a sleep loop, an event
  subscription over polling, a board card over a note in a file nobody reads.
- Say who you are (`X-Amux-Session`), read the refusal, and leave anything a
  person outside will read for the owner to approve.
