# Pages: agentic HTML in amux

Status: built (v1), 2026-10-21. See [What v1 built](#what-v1-built) below.

Asked by Ethan: a global **Pages** tab, "like apps
but powered by an LLM (worker?) + db (amux db) and maybe anything else", scored
against three tests:

- **(a) Minimal.** The fewest new parts.
- **(b) Out of the model's way.** Only a database and a model; the harness
  decides nothing the model could decide.
- **(c) Any application.** Any app you would otherwise build with a backend can
  be built as a page.

The input was a long requirements list (determinism, caching, provenance,
reactivity, extensibility, and so on). The plan below meets those requirements
with a database and a worker, and builds almost none of them as features. The
mapping is in [Requirements, one by one](#requirements-one-by-one).

## The plan

**A page is a worker whose output is an HTML page backed by its own SQLite
file.**

```text
~/.amux/pages/<name>/
  index.html     the page; the worker writes it
  page.db        the page's data; the page and the worker both read and write it
  (anything else the worker wants: scripts, assets, notes)
```

That is the whole format. There is no manifest, no section schema, no
dependency graph and no cache layer. The page renders from `page.db`, so opening
it costs no model call. The worker decides what to store, what to recompute and
when.

### Five parts

1. **A `page` worker type.** Worker types are "an execution adapter and an
   output renderer" and nothing else (`amux-core/src/worker_type.rs`). A page
   uses the existing coding adapter, with the page folder as its working
   directory, and a new renderer: its `index.html`. Identity, messages, memory,
   schedules, groups, budgets, boundaries and approvals all come with the worker
   already. Creating a page means creating a worker with `worker_type = page`.

2. **The Pages tab.** It lists `page` workers. Opening one shows `index.html` in
   a sandboxed iframe, with the worker's chat beside it ("add a chart of
   signups by week", "this number is wrong"). It is the same chat every worker
   has.

3. **One bridge, three calls.** The page script gets `amux.sql(query, params)`,
   `amux.ask(text)` and `amux.on("change", fn)`.
   - `sql` runs against this page's `page.db` and nothing else. It allows
     reads and writes, so ordinary app actions (add a row, tick a box, save a
     form) are instant and need no model.
   - `ask` sends text to the page's worker, the same as typing in its chat.
   - `on("change")` fires when `page.db` changes, whoever wrote it.

4. **A write can wake the worker.** When page code inserts rows into a table
   named `asks`, amux delivers one message to the page's worker naming the new
   row ids. The worker reads them, does the work, and writes its results back
   to the database. This gives page code a backend without a server: the table
   is the API. The worker chooses each time whether to answer with a model
   turn, or to write a script and leave it to the scheduler.

5. **A snapshot per turn.** Before each worker turn, amux copies `page.db` and
   `index.html` (`VACUUM INTO`, cheap for a page-sized file). The page shows a
   version list, and Restore rolls back to any of them. A turn that fails or
   misbehaves leaves the last good state one click away.

### What already exists and is reused unchanged

| Need | Existing |
|---|---|
| An agent with tools, connectors, vault, MCP, a browser | a worker |
| Recurrence and zero-model-cost jobs | scheduler, `kind: shell` |
| Credentials by name | vault |
| Outbound gates (email, money, public posts) | worker boundaries and approvals |
| Live push to the dashboard | the SSE stream (`api/sse.rs`) |
| Cost and run visibility | usage, the worker's peek and messages |
| Prior art for computed documents | `.mdai`: DAG runs, inputs-hash memoization, single flight, resume after restart |

### What is new

- A `page` worker type: a descriptor and a renderer.
- `GET /pages/<name>/*`: serves the folder to the sandboxed frame.
- `POST /api/pages/<name>/sql`: runs SQL against that folder's `page.db`.
  Opened with `SQLITE_LIMIT_ATTACHED = 0` so `ATTACH` cannot reach `amux.db`
  or another page. That needs rusqlite's `limits` feature, which the workspace
  does not enable yet (`Cargo.toml` has `bundled` and `functions`); an
  authorizer refusing `SQLITE_ATTACH` (the `hooks` feature) is the
  alternative. Each page's file is the isolation boundary, so full SQL is
  safe on it.
- A watcher on the `asks` table that wakes the worker (debounced, one message
  per batch, logged `verdict=page_ask_delivered`).
- A pre-turn snapshot and a Restore action.
- The tab.

Rough size: one new API module, one renderer, one tab. No migration to
`amux.db` beyond what worker types already store.

### Security

- **The frame.** It runs with `sandbox="allow-scripts allow-forms"` and no
  `allow-same-origin`, so it has an opaque origin, no amux cookie and no access
  to the dashboard.
- **The bridge.** The dashboard relays calls by `postMessage`, and only for
  that page's name. A page cannot reach the vault, other workers, the board or
  email, so it cannot act as the owner.
- **The worker.** It acts with the same boundaries as any worker. Sending email,
  spending money and posting publicly stay behind the existing gates.

## Examples, to check (c)

| App | How it is built as a page |
|---|---|
| CRM | Contacts and deals tables. CRUD through `sql`, instant. "Enrich this contact" inserts into `asks`; the worker researches and writes the fields. |
| Live dashboard of an outside API | The worker writes `fetch.py`, which fills `page.db`, and schedules it every 15 minutes as `kind: shell` (no model cost). It steps in only when the script breaks. |
| Research report that keeps itself current | Sections are rows. The worker refreshes stale rows on a schedule and records sources in a column it chose. |
| Support inbox triage | A Gmail connector, run by the worker. Draft replies are written to `page.db`; sending goes through the approval gate. |
| Internal tool over Postgres or any other API | The worker connects with vault credentials and syncs what the page shows. Or it writes the endpoint script and answers `asks` rows with it. |
| Game, calculator, form | Plain JavaScript and `sql`. The worker writes the code once and is never called again. |

Where it stops: many concurrent outside users, heavy compute in a request, and
sub-second backend logic that the browser cannot run itself. Each is handled
the same way, with the worker writing code and the scheduler running it, but a
page is an owner-scale app. Sharing a page with other people is a later stage
(cloud.amux.io accounts).

## Requirements, one by one

Built means amux implements it. Model means the worker does it, inside the
page, when the page needs it. Most of the list is the second kind.

| Requirement | How it is met | Built or model |
|---|---|---|
| HTML, CSS, JS, interactive | `index.html` in a frame | built (serve) |
| LLM populates content | the worker writes the db and the html | model |
| Progressive rendering | the page renders rows as they land, via `on("change")` | built (event) |
| Plan, reason, multi-step tools | a worker | existing |
| Dependencies between sections | the worker keeps them in its own tables or code | model |
| Identical output for identical input | the page renders from stored data; a reload makes no model call | built (by construction) |
| Replay any historical version | per-turn snapshots | built |
| Never publish invalid results | the worker writes in a transaction and validates before commit; Restore if not | model + built |
| Preserve last valid on failure | snapshot before each turn | built |
| Avoid redundant inference and tool calls | the db is the cache; code-first scripts cost no tokens | model |
| Share results across users and loads | one `page.db` | built (by construction) |
| Freshness and invalidation | the worker stores timestamps and refreshes on schedule or event | model |
| Incremental recompute | the worker updates only the rows that changed | model |
| Provenance, evidence, verified vs inferred | columns the worker keeps, when the page needs them | model |
| Audit of agent decisions | the worker's messages and transcript; snapshots show what changed | existing |
| Scheduled and event-driven updates | scheduler; `asks` wakes the worker | existing + built |
| Progress visibility | the worker's chat beside the page | existing |
| Arbitrary tools, APIs, models | worker tools, connectors, MCP, model choice | existing |
| Reusable templates, composable pages | copy a folder; a page may read another page's exported file | model |
| Isolation, permissions, credentials | sandboxed frame, per-page db, worker boundaries, vault | built + existing |
| Budgets and limits | worker cost caps and boundaries | existing |
| Idempotency, atomicity | SQLite transactions | existing |
| Convergence, boundedness | the worker's turn and budget limits | existing |

Nothing in the "model" rows is a schema amux enforces. A better model makes
each of them better without a harness change. That is test (b).

## Iteration history

Each version was scored 1 to 10 on the three tests, equally weighted.

| Iteration | (a) | (b) | (c) | Score | Changes | Remaining gaps |
|---|---|---|---|---|---|---|
| 0 | 2 | 3 | 6 | 3.7 | The requirements built as features: a document format with declared sections, a dependency graph, a cache layer, provenance records, a validation schema, a reactivity engine. | Every feature is a harness decision the model could make (b). Roughly ten new subsystems (a). |
| 1 | 5 | 6 | 7 | 6.0 | The 2026-10-06 design (`docs/design/worker-powered-apps.md`): `app.toml` manifest, scoped token, `/apps` route, `amux-app.js` with run/subscribe/read/write, per-app SQLite, per-block input hashes, visual acceptance. | The manifest declares tables and budgets ahead of the model; block hashing is the harness deciding what to recompute (b). Six new parts (a). |
| 2 | 8 | 8 | 6 | 7.3 | A folder with `index.html` and `page.db`, and a worker in it. No manifest and no block hashing; the db is the cache. The page reads through `sql`; all writes go through the worker. | Every click that saves data waits for a model turn, which rules out ordinary apps (c). |
| 3 | 8 | 9 | 8 | 8.3 | The page writes its own db directly; it is the isolation boundary, so full SQL is safe on it. The model is needed only for judgment. An `asks` table plus a wake gives page code a backend; the worker may answer with a script and the scheduler. | A page is a new kind of entity beside workers (a). History and rollback are unanswered (c). |
| 4 | 9 | 9 | 9 | 9.0 | A page is a worker of type `page`: worker types are already "adapter + renderer", so the page is a renderer and nothing else is new. Per-turn snapshots and Restore. `ATTACH` disabled on page connections. | See [Open questions](#open-questions). |

What moved the score:
- **Iteration 2 to 3:** letting the page write its own database. That single
  change turned "a document an LLM fills" into "an app with an LLM backend".
- **Iteration 3 to 4:** noticing that the worker-type seam already exists.

Iteration 5 was not needed. The remaining gaps are the owner decisions below,
which no redesign can settle.

### Why not 10

- **(a)** The `asks` wake is the one piece that is new behaviour rather than
  configuration. Polling from the page (`ask` after every insert) would remove
  it, at the cost of making every page author remember to call it.
- **(b)** Per-turn snapshots are the harness choosing when to save state. They
  stay because "restore the last good page" has to work even when the model
  wrote the bad state.
- **(c)** Many concurrent outside users is a different product (accounts,
  per-user data, rate limits). It is deferred, and nothing in this design
  blocks adding it.

## How the claims were checked

- **The worker-type seam is adapter plus renderer:** `crates/amux-core/src/worker_type.rs`
  module doc. Coding and chat adapters live in `api/worker_exec.rs`.
- **Memoization, single flight, resume and keeping failures out of the cache
  already exist in `.mdai`:** `api/mdai.rs` (`inputs_hash`, the
  `a_failed_run_is_recorded_and_never_served_from_cache` test) and
  `api/mdai/runs.rs` (one run per entry node, joinable, cancellable, resumed
  after a restart).
- **Read-only enforcement belongs in SQLite, and the existing SQL tab already
  does it that way:** `api/sql.rs` opens non-write queries
  `SQLITE_OPEN_READ_ONLY`. Pages go further with one file per page.
- **The dashboard already has a server push to extend:** `api/sse.rs` (the store
  subscription carried on `/api/events`).
- **`ATTACH` can be switched off, with one dependency change:**
  `Connection::set_limit` is behind rusqlite's `limits` feature, not enabled
  in the workspace `Cargo.toml` today.
- **`/apps` from the 2026-10-06 design was never built:** no `/apps`, `app.toml`
  or `amux-app.js` anywhere under `crates/`.

## Stages

1. **One page end to end.** Worker type, serve route, `sql` bridge, the tab.
   Proven with a CRM page: CRUD from the browser, "enrich" through chat.
2. **Backend and safety.** `asks` wake and per-turn snapshots with Restore.
   Proven by a page whose button triggers research the worker writes back, and
   by restoring after a deliberately broken turn.
3. **Code-first refresh.** A dashboard page whose worker writes a fetch script
   and schedules it. Proven by 24 hours of refreshes with zero model calls.
4. **Sharing** (later). A read-only public link, then per-user data through
   cloud accounts.

Each stage ships with e2e tests: a real page in a browser, the bridge refusing
`ATTACH` and other pages' files, the frame unable to reach `/api/vault`.

## Open questions

These are the owner's calls:

1. **Coding or chat adapter.** A coding worker can run scripts, edit files and
   use every tool; a chat worker starts faster and costs less. The recommendation
   is coding, so a page can always write its own backend code.
2. **Where pages live.** `~/.amux/pages/<name>` (private, backed up) or a folder
   in a repo (versioned in git, shareable).
3. **Relationship to `.mdai`.** The recommendation is to keep both:
   - `.mdai` is a document computed from sources;
   - a page is an app with state.

   A page's worker can still read a `.mdai` output like any other file.

## What v1 built

Asked by Ethan the same day: build it, with a real proposal page as the
criteria, in the Pages tab and at `<amux_url>/pages/<name>`, behind whatever
sign-in the server has.

| Piece | Where |
|---|---|
| Page folder, database, asks wake, snapshots, standalone URL | `crates/amux-server/src/api/pages.rs` |
| Sandboxed frame and the `amux` bridge, shared by the tab and the URL | `crates/amux-dashboard/static/pages-host.js` |
| Pages tab | `crates/amux-dashboard/static/app.js` (`_pages*`), `index.html` (`#pages-view`) |
| Browser test | `e2e/pages.spec.ts` |

Routes:

- **`/api/pages`:** list, create (`name`, `html`, `instructions`, `seed_sql`, `worker`).
- **`/api/pages/<name>`:**
  - the page and its HTML: get, `PUT /html`, delete (moves the folder to `.trash`);
  - its data: `POST /sql` (`{sql, params}` or `{batch}`), `POST /ask`;
  - change detection and files: `GET /version`, `GET /file?path=`;
  - history: `GET|POST /snapshots`, `POST /restore`.
- **`/pages/<name>`:** the standalone page.

Sign-in on the standalone URL is the dashboard shell's own decision
(`static_files::owner_access`):

| Who opens `/pages/<name>` | Result |
|---|---|
| This machine | the page |
| A browser with the owner session cookie | the page |
| A request carrying the owner token | the page |
| A server with no token of its own, such as a cloud server behind its gateway, which signed the visitor in | the page |
| Anyone else | a sign-in page; no page content or token is sent |

Where v1 differs from the plan above:

- **The worker is an ordinary worker, without a `page` worker type.** Creating a page creates
  `page-<name>` through the same path as the dashboard's New worker, with the
  page folder as its directory; `page.json` records it, and `worker` at create
  time attaches an existing one instead. A type would add a renderer, but the
  page already has one: the frame.
- **One file per page.** The frame is `srcdoc`, so relative URLs do not reach the folder.
  Pages are self-contained (inline CSS and JS, CDNs work), and
  `amux.file(path)` reads anything else in the folder.
- **Snapshots are taken before each delivery to the worker, before an HTML edit
  and before a restore.** They are not taken before every worker turn: amux only
  knows about the turns it starts for the page.
- **Change detection polls the file signature every 1.5 s while the page is
  visible.** The worker writes with the `sqlite3` CLI, which the server never
  sees, so the file is the one thing every writer moves.

Isolation, tested in `e2e/pages.spec.ts`: from the page, `fetch('/api/workers')`,
`parent.document` and `localStorage` are all blocked. Giving the frame
`allow-same-origin` turns the test red. A page connection refuses `ATTACH` and
`VACUUM INTO` (unit tests in `pages.rs`).
