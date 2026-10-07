# Worker-powered apps

Status: design, 2026-10-06. Owner: `amux` lane. Asked by Ethan, 16:03 the same day.

## The idea

Any HTML page becomes a web application whose contents a worker produces. The
page carries instructions (markdown or scripts) for how to fill it. Opening it
shows the last stored result immediately; "Run" executes the worker, which
pulls data through connectors, transforms it, writes it to the app's tables and
re-renders the page. The worker checks the rendered page against the app's
acceptance criteria, including visual ones. Any block can be edited by hand or
regenerated on its own, and a rerun only recomputes what changed.

## Built from the primitives amux already has

It is configuration over the eight existing primitives.

| Need | Primitive today |
|---|---|
| Page and instructions | filesystem (a folder in a repo) |
| Execution | workers, including ephemeral ones (`CC_EPHEMERAL`) |
| Data | SQL (`/api/sql`, `/api/sql/schema`, `/api/sql/rows`) |
| Recurrence | scheduler (`kind: shell` runs at zero model cost) |
| Credentials | vault (`/api/vault/secrets`, injected by name) |
| Outside data and actions | connectors (Gmail, Calendar, Drive, Slack, Telegram, Granola, Mattermost) and MCP servers |
| Outbound gates | approvals and standing approvals |
| Checks | contract acceptance criteria and a server-run verify command; headless browser |
| Prior art | `.mdai`: a document a worker computes on open, with run history |

## What is missing

### 1. Isolation (blocking)

A page served by amux runs with the owner's session, so its scripts could call
every API: send email, read the vault, start workers. Each app gets:

- its own origin (a subdomain or a sandboxed iframe with no amux cookie), and
- a scoped token naming only what its manifest declares: its tables, its
  connectors, its outbound rules, its run budget.

The server refuses anything outside the token, and logs the refusal with a
verdict, so a page cannot act as the owner.

### 2. Manifest, route and client

`app.toml` in the app folder: the HTML entry, the worker instructions, tables,
connectors, outbound rules, acceptance criteria and a cost cap. Served at
`/apps/<name>`. A small client, `amux-app.js`, exposes `run()`, `subscribe()`,
`read()` and `write()`; `subscribe` rides the existing SSE stream.

### 3. Runs are jobs

A worker today is a long conversation. A run needs structured input, writes
into the app's tables, a timeout, a cost cap, a receipt, and progress pushed to
the page. Same questions as the chat-job proposal (AMUX-5431, declined
2026-10-06): where permissions come from, how results return, how cost is
bounded. Answered here by the manifest and the scoped token.

### 4. Per-app data

`/api/sql` reaches the harness database. Apps get their own schema (a separate
SQLite file per app is the simplest isolation) and a change feed, so a write
re-renders the page.

### 5. Deltas

Each rendered block is stored with a hash of its inputs and instructions. A run
recomputes only blocks whose hash changed; "redo this block" sends only that
block's inputs. A hand edit is stored as an owner override that a rerun keeps.

### 6. Code first, model second

The worker writes and maintains the code that fetches and transforms data, and
the model is called only for judgment. Most runs then cost no tokens at all;
the model returns when code breaks or a decision is needed. This saves more
than caching does.

### 7. Visual acceptance

After a run: a headless screenshot, checked against the manifest's visual
criteria. A failure goes back to the worker with the screenshot, the way card
verification sends findings back.

### 8. Connectors

One generic kind covers most needs: an API key in the vault or an MCP server,
granted per app. Exa ships an MCP server. LinkedIn has no general API, so it
would mean browser automation with a saved profile, which carries
terms-of-service risk and is the owner's call.

### 9. Other users

Using apps beyond the owner needs accounts, per-user data and per-user run
limits. cloud.amux.io has accounts; per-user data separation is new.

### 10. Latency

A worker start takes seconds to minutes. Pages load from stored data at once
and treat a run as a background refresh.

## Stages

1. Isolation (1) and manifest, route and client (2), proven with one real app
   end to end: a lead-research page (Exa search, per-app tables, Gmail drafts
   behind owner approval).
2. Run jobs (3), per-app data (4), deltas and overrides (5).
3. Code-first runs (6) and visual acceptance (7).
4. Other users (9).

## Boundaries

Sending anything an outside person reads stays behind owner approval in every
stage; a standing approval can widen it per app. Spending money on a new
connector is the owner's call.
