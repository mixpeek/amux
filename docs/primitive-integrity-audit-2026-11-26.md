# Primitive integrity audit, 2026-11-26

Audited at `origin/main` 6ee5d73d8, which was also the live server's commit at the time
(`/health` commit 6ee5d73d8af5). Read-only: no code was changed. Row counts come from
read-only queries against the live `~/.amux/amux.db`.

Request: MSG-116838.

## 1. The canonical primitives

VISION.md A1 and root CLAUDE.md name eight. Below is each primitive, the store it
actually runs on today, and its job.

| Primitive | Canonical store (live) | Responsibility |
|---|---|---|
| Workers | `~/.amux/sessions/<w>.env` (227 files) + tmux/herdr panes | identity, provider, cwd, lifecycle |
| Board | `issues` (+ `statuses`, `session_gates`, `board_change_log` CDC) | units of work, status, leases, evidence |
| Messages | `conversation_posts` (threads), `cmd_history` (every delivery to a worker, the `MSG-` ids), `steering_queue` (pending delivery) | owner and peer communication, delivery |
| Schedules | `schedules`, `schedule_runs`, `schedule_audit` | anything that runs later or repeatedly for someone |
| Files | `$HOME` through `/api/fs` and `/api/file` | documents, artifacts, the file view |
| Groups | `CC_TAGS` on worker env + `group_config` | membership, channels, scope layer |
| Memories | `~/.amux/memory/{_global,tags/<g>,<w>}.md` (237 files) | durable context per scope |
| Environment | `amux.env` / `env/<group>.env` / `sessions/<w>.env` / `server.env`, resolved worker > group > global | configuration and credentials |

Two cross-cutting records sit under all eight and are legitimate infrastructure. The
first is `_amux_ledger`, the hash-chained record of decisions and effects. The second
is `runtime_jobs::registry`, amux's own internal loops.

## 2. Findings, prioritized

Each finding gives where, why it deviates, how to express it with existing
primitives, and whether it is accidental or a defensible exception.

### P0. A second, dormant domain model runs beside the live one (A10, A1)

The Rust rebuild plan (RR-00xx) introduced a typed model with its own tables. The
live system never moved onto it. Both exist, and the typed half still runs a 3-second
loop.

| Typed table (rebuild) | Live rows | Live equivalent | Live rows |
|---|---|---|---|
| `_amux_workers` (`migrations/0003_workers.sql:10`) | 6 | worker env files | 227 |
| `_amux_sessions` (`0003_workers.sql:28`) | 0 open | tmux panes | fleet |
| `_amux_leases` (`0003_workers.sql:43`) | 0 | `issues.lease_*` (`0068_task_lease.sql:19`) | 22 |
| `_amux_attempts` (`0003_workers.sql:54`) | 0 | `task_attempts` (`db/attempts.rs:41`) | 5748 |
| `_amux_task_budgets` (`db/harness_store.rs:591`) | 0 | `card_budgets` (`api/runner.rs:242`) | 33 |
| `_amux_memories` (`api/memories.rs:1`) | 0 | `~/.amux/memory/*.md` (`api/scope.rs:26`) | 237 files |
| `_amux_messages` (`api/messages.rs:65`) | 25 | `conversation_posts` / `cmd_history` | live |
| `_amux_planning_nodes`, `_amux_current_plans`, `_amux_goal_contracts` (`0062_self_driving_control_plane.sql`) | 0 each | board cards | live |
| `_amux_decompositions` (`orchestrator/events.rs`) | 0 | board cards | live |
| `tasks` (`0001_baseline.sql`) | 1 | `issues` | live; only a `DELETE` remains (`api/session_verbs.rs:34098`) |

- **The loop runs on nothing.** `lib.rs:942-1060` builds `orchestrator::runtime::Runtime`
  and spawns it every `AMUX_RS_TICK_SECS` (default 3). Its own header says the planner
  "runs over an empty task list" (`orchestrator/runtime.rs:6-8`). Live fleet names get a
  `foreign_worker_id` "so the planner can SEE the task without ever being able to assign
  it" (`orchestrator/runtime.rs:35-38`). Dispatch actually happens in
  `runtime_jobs/board_drive.rs`, 19,412 lines.
- **No client calls the typed APIs.** `app.js` and the bash CLI have zero calls to
  `/api/memories`, `/api/messages`, `/api/planning`, `/api/harness`, `/api/orchestrate`
  or `/api/orchestrations`. `/api/workers` has 3 dashboard uses; `/api/sessions/` has 136.
- **Why it matters.** Every reader has to know which half is real. An instrument built on
  the typed half reads an honest-looking zero: `FleetProgress.live_leases` comes from
  `_amux_leases`, which is always 0, while 22 cards hold leases. That breaks A5 in
  practice even though the field exists.
- **Remedy.** Declare the live stores canonical and delete the typed duplicates. Keep the
  pure state machines in `amux-core`, but point them at the live stores:
  - `plan_tick` reads `issues` leases.
  - `memory` types parse the `.md` files.
  - Drop the 3 s runtime loop, or let board_drive call the planner.
  - Write one migration that drops the empty tables.
  - Move `amux-core`'s vocabulary (`Worker`, `Lease`, `Attempt`) onto the live records'
    shapes, so a type and its table say the same thing.
- **Verdict.** Accidental. It is the remains of a migration that went the other way.

### P0. Messaging has five stores and two delivery queues (A1, A10)

| Store | Where | What uses it |
|---|---|---|
| `conversation_posts` | `api/conversations.rs:1651`, `bridge/mod.rs:772` | Messages tab, bridge |
| `cmd_history` | 38 files write or read it, e.g. `runtime_jobs/board_drive.rs:1432` (`type='pickup'`) | delivery record and `MSG-` ids |
| `steering_queue` | 9 writer files, e.g. `api/session_verbs.rs:10620, 40433, 41130, 41168, 44640`, `api/signals.rs`, `runtime_jobs/autofix.rs:12212` | pending delivery |
| `_amux_messages` | `api/messages.rs:65`, `orchestrator/runtime.rs:3858` | 25 rows, no client |
| `~/.amux/channels/<a>__<b>.jsonl` | `api/channels.rs:15-31` | `app.js:27193, 27234, 27259` |

- **Channels is a third DM system.** `api/channels.rs` is a pairwise worker-to-worker
  transcript in flat JSONL files under `~/.amux/channels` (5 files live). It sits beside
  Messages DMs and has no attribution, no delivery state and no ledger row. Remedy:
  - A channel is a Messages DM conversation between two workers.
  - Migrate the 5 files into it.
  - Point the 3 `app.js` call sites at `/api/conversations`.
  - Delete `channels.rs`.
- **`_amux_messages` is the typed duplicate from P0 above.** Remedy:
  - Delete it and `api/messages.rs`.
  - `WorkerCommand::DeliverMessage` should carry a `cmd_history` id instead.
- **There are two queues: `steering_queue` and `DeliverMessage` at turn boundary.** Writers
  reach `steering_queue` through raw `INSERT`s in 9 files, and `api/runner.rs` names
  `steer_enqueue_precond_with_id` as "the chokepoint". Remedy:
  - Make that function the only writer.
  - Make the raw inserts a compile-time impossibility: a private table module with one
    pub fn, and a test that greps for `INSERT INTO steering_queue` outside it.
- **Message and delivery receipt.** `conversation_posts` + `cmd_history` together are
  defensible as message and delivery receipt, if each post carries the `cmd_history` id
  it produced. Verify that link exists on every delivery path (not checked in this
  audit; see section 4).
- **Verdict.** Channels and `_amux_messages` are accidental. Posts + delivery log is a
  legitimate two-record design.

### P1. A retired Telegram path still runs and still owns a table (A1, A10)

- `lib.rs:751` spawns `runtime_jobs::telegram_relay` (903 lines). Its loop
  (`telegram_relay.rs:31-45`) is retired: it logs `telegram_relay_retired`, then sleeps
  300 s forever unless `AMUX_TELEGRAM_SCREEN_RELAY=1`.
- `telegram_relay.rs:3-10` describes a screen-scraping relay. That is ethos deviation D1,
  which `bridge/mod.rs` already replaced.
- `telegram_mappings` (`0050_telegram.sql`) is still read and written by the bridge as a
  second link table (`bridge/mod.rs:324, 400`), beside `bridge_links`. Both have 0 rows live.
- `api/telegram.rs` keeps its own mapping CRUD.
- Remedy:
  - Fold Telegram links into `bridge_links`.
  - Delete `telegram_relay.rs`, the mapping CRUD in `api/telegram.rs` and `telegram_mappings`.
  - Keep `telegram_poll` as the adapter, which is what `bridge/mod.rs:4-7` designs:
    "a new platform is an adapter, never a new subsystem".
- Verdict: accidental, migration left half-done.

### P1. Schema created outside migrations (lifecycle violation)

Tables made with `CREATE TABLE IF NOT EXISTS` at the point of use:

- `task_attempts` (`db/attempts.rs:41`)
- `task_episodes` (`runtime_jobs/episodes.rs:84`)
- `card_budgets` (`api/runner.rs:242`)
- `group_config` (`api/groups.rs:375`)
- `file_visits` (`api/file_visits.rs:17`)
- `nav_visits` and `nav_picks` (`api/palette_history.rs:33-36`)
- `event_dispatch_cursor` (`api/event_subs.rs:57`)
- `_amux_brex_spend` (`api/brex.rs:175`)

- **Root cause, stated in `db/attempts.rs:12-21`.** `migrate.rs` skips a version it already
  applied "without reading the name". So a dirty build that claimed version 69 makes every
  later real 0069 silently never run. Every module above works around that one defect.
- **Remedy: fix the root.**
  - `migrate.rs` records name and checksum, refuses or renames a collision, and logs
    `migration_name_mismatch`.
  - Then move these nine tables into numbered migrations and delete the ensure functions.
- Verdict: accidental, and it multiplies. Each new module copies the workaround because
  the comment tells it to.

### P1. The goal checker reimplements pass-condition schedules (A1)

- `runtime_jobs/goal_check.rs:18-24` runs a scoped `AMUX_GOAL_CHECK` shell command every
  600 s per worker.
- `runtime_jobs/schedule_check.rs:1-9` already gives a schedule a `pass_check` shell
  command, with run history, audit and failure routing (`schedule_failure.rs`).
- Today a goal check has no run history, no audit row, and no place in the Schedules tab.
- Remedy:
  - A goal check is a `kind: shell` schedule with `pass_check`, owned by the worker.
  - `goal_keeper` reads its last run.
  - `AMUX_GOAL_CHECK` stays as the way to create it.
- Verdict: accidental.

### P1. Owner-facing cadences hand-rolled inside jobs (A1, A4)

`runtime_jobs/mod.rs:8-26` draws a correct line. Internal plumbing is a `PeriodicTask`;
user intent is a `DurableSchedule` with history and audit. Several jobs are user intent
by that module's own definition, yet they keep their own clocks:

- **Duplicated digest timer.** `email_approval_digest.rs:47` and `api/decision_digest.rs:505`
  each implement `digest_hour`, the same "daily at hour H" logic in two places.
- **Send queues with private clocks.** Each due-time queue below has its own job polling it:
  - `email_scheduled_send.rs` (held email sent at its approved time)
  - `social_publish.rs` over `social_queue.next_attempt_at` (`0138_social_queue.sql:15`)
  - `crm_sequences` and `crm_enrollments` (`0171_crm_sequences.sql`, driven from
    `api/crm_outbound.rs`)
- Remedy: each becomes a `schedules` row, `once` for a single send or recurring for a
  digest, of `kind: shell` calling the existing endpoint. The owner then sees, pauses and
  edits these cadences in one place, with history, and missed-run policy comes free.
- Verdict: accidental. The reapers, heartbeats, samplers and the CDC poller are the
  legitimate `PeriodicTask` exception and should stay.

### P1. Environment has two resolvers and 325 raw reads (A10)

- **Two resolvers.**
  - `api/settings.rs:66` `effective_env` reads `server.env` over the process (global only).
  - `api/session_verbs.rs:1096` `scope_env_layers` and `:1156` `scoped_setting_in` do
    worker > group > global.
- **Most reads skip both.** There are 325 `std::env::var("AMUX_…")` reads in
  `crates/amux-server/src`, against 49 `effective_env` calls. The top files:
  - `session_verbs.rs` 34
  - `invariants/monitor.rs` 13
  - `mac_health.rs` 12
  - `autofix.rs` 11
  - `mdai.rs` 11
  - `board_drive.rs` 10
- Any of these that CLAUDE.md documents as "scoped" (e.g. `AMUX_DISPATCH_BACKLOG_WHEN_IDLE`,
  `AMUX_WORKSPACE_ISOLATION`) silently ignores the Scope tab if read raw.
- Remedy:
  - One `environment::resolve(scope, key)` in its own module.
  - `effective_env` becomes `resolve(Global, key)`.
  - A registry of keys that declares each one's scopability, rendered by the Scope tab.
  - A test that fails on a raw read of a registered scoped key.
- Exception: boot-time process config read once in `config.rs`.

### P1. Files: three upload paths and two browse APIs (A8, A10)

- **Three uploads:**
  - `api/files.rs:45` `/api/files/upload` (raw body, RR-0093)
  - `api/fs.rs:61` `/api/fs/upload`
  - `api/upload.rs`, the chunked protocol `app.js uploadAndAttach` uses
- **Two browse APIs.** `/api/files?path=` (`files.rs:4-6`) duplicates `/api/fs`. The
  dashboard calls `/api/files` only under `/api/files/mdai/*` and one upload.
- Remedy:
  - Keep `/api/fs` plus chunked upload.
  - Alias `/api/files` listing and download onto `fs`.
  - Move `/api/files/mdai/*` under `/api/mdai`.
- Verdict: accidental.

### P2. Audit trails: one declared record, and a dead table still written to

- **The current trails, by live row count:**

  | Trail | Rows |
  |---|---|
  | `_amux_ledger` | 1,458,926 |
  | `_amux_interactions` | 885,601 |
  | `_amux_state_events` | 423,445 |
  | `board_change_log` | 86,776 |
  | `schedule_audit` | 8,283 |
  | `guard_verdicts` | 10,553 |
  | `_amux_request_log` | 17,659,146 |

- **Reconciled by design.** `api/ledger.rs:3-10` reconciles interactions and policy receipts
  into the ledger (working view vs record). That is a legitimate design.
- **The dead table.** `interaction_log` (Python era, 10 rows) is still written by
  `runtime_jobs/board_drive.rs:17970, 17987` and `api/why.rs:1815`.
  `api/scope.rs:33-37` claims every scope write audits there "where the existing audit
  consumers already look". With 10 rows total, nobody looks there, and the claim does not
  hold (ethos rule 6).
- Remedy: route those writes through the ledger, then drop `interaction_log`.
- **Other trails.**
  - Fold `schedule_audit` into the ledger, keyed by schedule id.
  - `board_change_log` is a CDC feed for SSE (`runtime_jobs/cdc_poller.rs`) and stays.
  - `_amux_request_log` at 17.6M rows is most of a 16.6 GB database. Retention, not
    architecture, but it belongs with this cleanup.

### P2. Worker list: two APIs and a 12,911-line compatibility layer (A10)

- `api/sessions_legacy.rs` serves the "Python-shaped" `/api/sessions` and computes status
  in `derive_status` (`:3672`). `/api/workers` is the "modern envelope" with almost no
  callers.
- Route aliases (`api/aliases.rs`) are a legitimate compatibility contract. Two response
  shapes computed by two code paths are not.
- Remedy: one projection function produces the worker record, and both routes serialize it.
- Verdict: mixed. The alias is legitimate; the duplicate computation is accidental.
- Done (step 9): `api/worker_projection.rs` holds the status derivation and the record
  builder. `/api/sessions` serves the projection rows; `GET /api/workers[/{id}]` serializes
  the same rows (`worker_record`) and lists the same fleet, with the typed store adding only
  `id`, `version`, `state` and the other typed-only fields. `SHARED_FIELDS` names what both
  carry and every read logs `worker_projection_agrees` or `worker_projection_diverged`.

### P2. Two CLIs

- The bash `amux` is 14,838 lines.
- `crates/amux-cli/src/main.rs` (918 lines) "ships beside the bash amux until Phase 11
  cutover" (`main.rs:1-3`), and that cutover has no date.
- Remedy: owner decision. Either finish the cutover or delete `amux-cli`. A parallel CLI
  that mirrors "the core surface" drifts with every verb added to bash.

### P2. Dashboard dialogs bypass the shared one (A8)

- `app.js:4086` states the reason for `showConfirm`: native `confirm()` and `alert()` are
  "blocked in PWA standalone mode". Native dialogs still appear:
  - 30 native `confirm(` calls, e.g. `app.js:8367, 8383, 27090`
  - 16 `prompt(` calls, e.g. `app.js:15012, 39322`
- These sit beside 53 `showConfirm` calls.
- On the iOS PWA these controls silently do nothing.
- Remedy: replace each with `showConfirm` / `showPrompt`, and add a test in
  `tests/dashboard_assets.rs` that fails on a bare `confirm(` or `prompt(`.
- Verdict: accidental.

### P3. Product features with their own stores (A1, owner's call)

These hold domain data rather than re-expressing a primitive, so they are not violations
of A10. They are worth naming against A1 ("a new feature is configuration of these plus
the surface that shows it"):

- CRM: 14 `crm_*` tables
- Map: `map_pins`, `map_meta`, `map_shares`
- `graph_nodes` / `graph_edges` (68 rows)
- `journal_entries`
- `~/.amux/habits.json` (`api/habits.rs:31`)
- `location_*`
- `social_*`
- `calls`
- photos
- torrents

Recommendation, for the owner to decide (ethos rule 8): no migration of existing data. New
features of this kind ship as Pages (`api/pages.rs`: a worker plus its own SQLite) or
published apps, so the server stops growing a table family per feature. The scheduling
parts of CRM and social are already covered in P1 above.

## 3. Legitimate exceptions (keep)

- **`PeriodicTask` vs `DurableSchedule`** (`runtime_jobs/mod.rs:8-26`). Reapers, samplers,
  heartbeats and the CDC poller are internal plumbing and should never appear in the
  owner's schedule list.
- **Approval cards as a link table** (`api/approval_cards.rs:3-16`). Five approval kinds
  file a needs-you card, and `_amux_approval_cards` only links item to card. Approval
  state stays in each item's own record. This is the model composition to copy.
- **Intent built on primitives.** Migration `0160_intent_on_primitives.sql` dropped the
  intent tables, and outcomes became Messages threads and cards. This is the remediation
  pattern for most findings above.
- **The ledger** as the one hash-chained record, with interactions and policy receipts as
  short-lived working views (`api/ledger.rs:3-10`).
- **Route aliases** for the session-to-worker and issue-to-task rename (`api/aliases.rs`).
- **`search_firehose`**: a derived, regenerable index in its own database.
- **Pages' own `asks` and `_amux` tables** (`api/pages.rs:122-132`): app data inside a
  page's database, if that is where they are created. Confirm the connection is the
  page's DB.

## 4. Coverage

All 204 tables defined in `crates/amux-server/migrations` and `src` were enumerated and
mapped to a primitive. The inspection methods were:

| Scope | Method |
|---|---|
| Table writers | located by grep |
| 31 candidate tables | live row counts |
| All 73 `runtime_jobs` modules | header read |
| About 60 `api/` modules | header read |
| `lib.rs` job wiring (lines 735-1060) | read |
| Dashboard and CLI usage of 34 API prefixes | counted |
| Raw env reads, `CREATE TABLE` sites, `send-keys` sites, `INSERT INTO` for message tables | grepped across all of `crates/amux-server/src` |

Not inspected line by line. Findings in these areas are limited to what greps and module
headers show:

- **The bodies of the largest server files**:
  - `api/session_verbs.rs` (59,959 lines)
  - `api/board.rs` (20,975)
  - `runtime_jobs/board_drive.rs` (19,412)
  - `runtime_jobs/autofix.rs` (19,201)
  - `api/sessions_legacy.rs` (12,911)
  - `invariants/checks.rs` (12,350)
  - `db/board_store.rs` (9,061)
  - `api/request_log.rs` (9,030)
  - `api/git_guard.rs` (7,992)

  These files probably hold more special-case logic than this audit found.
- **About 140 `api/` modules** beyond their headers.
- **Other crates**: `amux-core` beyond `lib.rs`, `memory.rs` and `orchestrator` types;
  `amux-cli`.
- **Clients and other code**:
  - `crates/amux-dashboard/static` beyond targeted greps (83,481 lines of JS)
  - the bash `amux` CLI beyond greps
  - `desktop/` (17 Swift files, 9,966 lines)
  - `ios/`, `android/`, `cloud/`, `business-ui/`, `site/`, `integrations/procwarden`
  - `scripts/`, `e2e/`, `skills/`
- **The `conversation_posts` to `cmd_history` link** on every delivery path (P0 messaging).
- **Whether `pages.rs` creates its tables in the page's own database** (section 3).

A follow-up pass should take the nine large files one at a time with the same rubric,
starting with `session_verbs.rs`. It holds 34 raw env reads, 6 `send-keys` sites, 5
`steering_queue` inserts and the scoped resolver, so it is where most P1 findings converge.

## 5. Remediation plan

Ordered by architectural payoff per unit of risk. Each step is its own landing, with an
off switch where it touches shared infrastructure (A11).

1. **Fix `migrate.rs` name and checksum handling, then move the nine point-of-use tables
   into migrations.** Small, removes a workaround every new module copies. Log
   `migration_name_mismatch`.
2. **Retire the dormant typed model.**
   - Stop the 3 s orchestrator loop (keep `AMUX_RS_ORCH=1` to re-enable for one release).
   - Point `FleetProgress` at `issues` leases.
   - Drop `_amux_leases`, `_amux_attempts`, `_amux_task_budgets`, `_amux_memories`,
     `_amux_planning_nodes`, `_amux_current_plans`, `_amux_goal_contracts`,
     `_amux_decompositions` and `tasks`, plus their APIs. All are empty or near-empty live.
   - Reconcile `_amux_workers` (6 rows) against the env files and delete it, or make it a
     derived cache with a stated refresh.
3. **Messaging consolidation.**
   - Migrate the 5 channel files into Messages DMs and delete `channels.rs`.
   - Delete `_amux_messages` and `api/messages.rs`.
   - Make `steer_enqueue_precond_with_id` the only `steering_queue` writer, with a grep test.
4. **Delete the retired Telegram relay** and fold `telegram_mappings` into `bridge_links`.
5. **Goal checks and owner cadences become schedules**: goal check, both digests, scheduled
   email, social publish and CRM sequence steps. One `digest_hour` goes away.
6. **One environment resolver** with a scopable-key registry and a raw-read test. Burn down
   the 325 raw reads file by file, starting with `session_verbs.rs`.
7. **Files**: one browse API, one upload path.
8. **Audit**: route `interaction_log` writers to the ledger and drop it, fold
   `schedule_audit`, and set retention on `_amux_request_log`.
9. **One worker projection** behind `/api/sessions` and `/api/workers`.
10. **Dashboard**: replace 46 native dialogs, plus the test.
11. **Owner decisions**: the Rust CLI's fate, and whether new domain features default to
    Pages (P3).

Steps 2 to 4 remove the most code and the most "which one is real" questions. Step 1 goes
first because step 2 needs a trustworthy migration to drop tables.
