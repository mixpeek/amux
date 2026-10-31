# Implicit state machines in amux

Owner ask, 2026-10-31 (MSG-102697): "find all of the implicit state machines in
amux". Inventory taken at `d9695a0d9` from committed code, in three sweeps
(server `api/`; server `runtime_jobs/`, `db/`, `orchestrator/`, migrations and
`amux-core`; dashboard, Bash CLI, scripts, hooks and apps). Line numbers are as
of that commit and drift; the function names are the stable handle.

An **implicit** state machine is an entity whose lifecycle is held as string
literals in a status column or JSON key, or as a combination of flags,
timestamps, nullable columns and marker files, with transitions written at
scattered sites instead of through one enum and one transition function.

Background: this is the groundwork for the worker journal and single status
model proposed in the pi-durable-subagents review (2026-10-30). It says which
machines that model should absorb first.

## What cuts across all of them

1. **Free-text state columns.** Across 144 migrations only three CHECK
   constraints exist; `board_overlap_callbacks.state` is the only state column
   with one. Every other status, state and phase column accepts any string, so
   legacy spellings (`resolved`, `needs_you`), hand-cleanup values
   (`card_contracts.state='superseded'`) and free-form refusal codes in
   `cmd_history.submit_verdict` all live on disk.
2. **Explicit enums that the writers walk around.** `amux-core` defines
   `TaskStatus` + `apply_transition`, `DeliveryState` + `advance_delivery`,
   `WorkerLifecycle`, `WorkerState` and `CommandState`. The API PATCH path uses
   the board one; `db/advance.rs` (the server's main transition path) does not
   call it, background jobs write `issues.status` with raw SQL, the orchestrator
   writes message delivery as hand-built JSON, and `WorkerState` has no
   transition function at all.
3. **Guarded claims, blind finishes.** Queues almost always claim with
   `WHERE state='queued'` and then finish with `WHERE id=?`. After a stale
   reclaim, the original finisher can overwrite the new run. This shape repeats
   in the land queue, social queue, media jobs, schedule pass-check, bridge
   outbound and reclaim purge.
4. **One fact, several encodings.** Needs-you (status vs tag vs decision card),
   blocked (status vs `blocked_on`), archived (`lifecycle` column vs
   `CC_ARCHIVED` env vs `.env.reaped` file), disabled schedule (`enabled` vs lock
   vs reason), "terminal" (three different sets across readers), "deleted"
   (`deleted IS NULL` vs `COALESCE(deleted,0)=0`).
5. **Derived more than once.** Worker status is computed on the server from
   hooks, pane scraping, transcripts and process trees, then re-derived by the
   dashboard at least six times with different precedence, and a third and
   fourth time by `worker-stall-watch.sh` and the boundary guard's subagent
   count. Two hook producers map the same Claude Code events differently.
6. **In-memory state that a restart forgets.** The goal keeper's no-progress
   budget and exhausted flag, the client's notification edge detector (one per
   tab), pending pause/resume projections.

## The machines, ranked

### Tier 1: workers and cards (where the owner reads state)

| # | Machine | Stored in | Main hazard |
|---|---|---|---|
| 1 | **Worker activity status** | Not stored. Derived in `sessions_legacy.rs` (~3880-4300) from `prefs.session_reports` hook rows, codex rollout, transcript interrupt, child processes and pane-text scraping; re-derived in `app.js` by `_sessStatusKey`, `_workerExecutionBadge`, `_waitingLabel`, the grid dot, the graph dot and the board `working/waiting/needsyou` predicates | Precedence chain where later rules overwrite earlier ones; client functions disagree (a stopped worker still carrying `waiting/owner` shows "Needs input" on its card and `stopped` in the filter; a paused-but-running worker filters as `working`; grid and graph dots show a guessed `unknown` as green) |
| 2 | **Hook-reported status** | `hook-report.sh` (queue + latest-wins row per worker) and `native-status.py` (numbered spool + `counter.json`) | Two producers with different event maps (SessionStart, AskUserQuestion, elicitation); `blocked` clears only on PostToolUse/Stop, so an interrupted turn stays blocked; a lost lock drops a Stop edge and leaves the lane `active`; a lost counter makes the next edge a replay |
| 3 | **Worker execution state** | `_amux_workers.state` (`WorkerState` JSON) | `update_worker_state` is a blind overwrite with four independent writers (bootstrap, orchestrator events, runtime, scan), so a late `Idle` overwrites `Starting` or `Error` |
| 4 | **Worker lifecycle** | `_amux_workers.lifecycle` (explicit, CAS-guarded `lifecycle_transition`) plus `CC_PAUSED`/`CC_ARCHIVED` env flags and `.env.reaped` files | `CC_ARCHIVED` is still written and read in ~50 sites after migration 0069 said the column replaces it; nothing ties `lifecycle != active` to `state == stopped`; with no DB row the lifecycle comes from env flags alone |
| 5 | **Session** | `_amux_sessions`: live = `ended_at IS NULL` | No unique index for one live session per worker; readers pick the newest |
| 6 | **Board card** | `issues.status` plus about eight more fields: `archived`, `pre_archive_status`, `deleted`, `blocked_on`, `waiting_on`, lease columns, the `needs:you` tag, `type='decision'` | `db/advance.rs` has no from-to edge check, so terminal states can be left; PATCH falls back to `BoardTransition::Force` for any pair; raw SQL writes from `board_hygiene` (discarded), `draft_hold` (done), `schedule_failure` (done, needsyou) skip the lease release and attempt record; orchestrator quarantine calls `apply_transition` then `save_patched`, skipping `advance()`; `save_patched` is last-writer-wins (bumps `version`, never checks it); unarchive restores `pre_archive_status` blindly; `clear_done` archives with no event |
| 7 | **Card leases (two systems)** | `issues.lease_*` and the orchestrator's `_amux_leases` table | Neither reads the other, so one card can be held twice; `_amux_leases.generation` is always 0 (a reclaim deletes the row, and `INSERT OR IGNORE ... COALESCE((SELECT generation+1 ...),0)` only runs when no row exists), so the generation guard never tells leases apart; lease enforcement is off by default (`AMUX_LEASE_ENFORCE`) |
| 8 | **Task attempts** | `task_attempts` (running = `ended_at IS NULL`, outcome derived from destination status) | Only moves as a side effect of lease changes; `reconcile_orphans` exists to clean up after the raw-SQL bypasses in #6 |
| 9 | **Card completion callback** | `issues.callback_state` (armed, pending, dispatching, queued, refused, suppressed) | `armed->pending` is hidden inside `save_patched`; `queued` is terminal here while real delivery lives in `cmd_history` |
| 10 | **Card contract / review / deploy** | `card_contracts.state`, `.review_state`, `.deploy_state`, plus `card_prereviews.state` | Four columns over two tables move together (a failed review writes `state=frozen`, `deploy_state=NULL` and `issues.status`); `set_state` and `note_on_card` are blind; `superseded` exists only from a hand cleanup |

### Tier 2: delivery, schedules and queues

| # | Machine | Stored in | Main hazard |
|---|---|---|---|
| 11 | **Message delivery receipt** | `cmd_history.delivery`, `submit_verdict`, `delivered_at`, `capture_pending`, `card_id`, `intake_result.state`; `steering_queue.delivering_since`; `steering_history.outcome` | `uncertain -> queued+confirmed` is blind; `steering_history.outcome` is prose matched with `LIKE` (two spellings of delivered); `submit_verdict` value set grows with refusal codes; `intake_result.state` duplicates `capture_pending`+`card_id` and several writes update one without the other; `delivered_at` mixes seconds and ms |
| 12 | **Worker messages** | `_amux_messages.delivery` (core `DeliveryState`, forward-only) | Orchestrator writes `{"state":"delivered"}` by hand with `LIKE '%queued%'`, and one API write is unguarded, so the forward-only rule is not enforced in the DB |
| 13 | **Schedule** | About 20 columns: `enabled`, `deleted`, `disabled_reason`, `locked_*`, bounds, `failure_streak`, `failure_notice`, `last_delivery`, `next_run`, `version` | "Disabled" has three encodings; two of five disable writers are unguarded; the reserve/skip re-arm checks `version` but not `disabled_reason`, so it can re-enable a schedule disabled for another reason |
| 14 | **Schedule pass-check** | `schedules.pass_state` (probe, pending, reconciling, fail, pass, unrunnable) | Async check: a fire that sets `pending` mid-check is overwritten by `record()` |
| 15 | **Schedule run** | `schedule_runs.status` (`RunOutcome` plus raw `running`, `interrupted`, `skipped`, legacy `ok`), `delivery`, `submission` | `queued` is written and never advanced when the steering item lands |
| 16 | **Land queue (server)** | `land_queue.state` | Blind `record()` and `requeue()`; a reclaimed slow batch can be finished twice; `merged_sha` means "push candidate" while queued |
| 17 | **Social publish queue** | `social_queue.status` | Blind retry/fail and posted writes can overwrite a sweep's `uncertain`; a `publishing` row that hangs within the same boot never times out |
| 18 | **Bridge outbound** | `bridge_outbound.state` (held, approved, sent, failed) | UPSERT overwrites state; no `sending` state, so two overlapping passes can both send |
| 19 | **Board overlap callbacks** | `board_overlap_callbacks.state` (the one CHECK-constrained column) | Updates are blind; `delivered` exists only as a derived SELECT CASE |
| 20 | **Goal keeper** | In memory (`Keep`: no-progress budget, exhausted, held-for-reset) | A restart resets the budget and the exhausted state; `goal_check` persists, the keeper does not |

### Tier 3: build, land and host tooling (scripts and files)

| # | Machine | Stored in | Main hazard |
|---|---|---|---|
| 21 | **Builder activation** | `~/.amux/rust-build-stamp` + `.adopted_at`, `.shell_defer_since`, `.failed`, `.rolled_back`; install identity and rollback files; provenance JSON | Stamp keys on the last `crates/` commit, identity on repo HEAD; a leftover `.shell_defer_since` makes the next defer start expired |
| 22 | **Build lock (builder + prebuild)** | `~/.amux/rust-build.lock/pid`, `prebuild-done`, `prebuild-done.failed` | Builder breaks a lock with an empty pid file, and prebuild writes its pid only after `mkdir`, so two builds can run in that gap |
| 23 | **CLI land lock and queue** | `~/.amux/locks/land-<key>/` (pid, since, who), tickets, results, `.regen-fail` | Takeover after 3x max-hold while the holder may be alive (two pushers); `kill -0` liveness passes on a reused pid; `.regen-fail` never expires |
| 24 | **Land gate receipts** | `~/.amux/land-gate/receipts/<patch-id>.json`, 14-day TTL | Coverage by patch id ignores how far main moved; any passing receipt covers a commit even after a later failing one |
| 25 | **CI watch red stretch** | `~/.amux/state/main-ci-watch.json` | A board read timeout reads as "card closed": on red it files a duplicate, on green it clears `card` and leaves the card open; only the newest run is read |
| 26 | **Worker stall ladder** | `~/.amux/tmp/worker-stall-watch/<w>.ladder` and markers | Decides "stalled" on its own instead of from the server's status; lives under a tree the cleanup tick reaps (not verified) |
| 27 | **Mac cleanup tick** | `~/.amux/logs/mac-cleanup/state` key=value file, tick lock, RCA files | Escalation cooldowns are not reset when a constraint clears, so a new incident inside the cooldown is suppressed |
| 28 | **Staged-guard block marker** | `~/.amux/hooks/state/guard-block-*.json` | No TTL; a leftover marker resolves a commit days later as trimmed or reallowed |
| 29 | **Guard verdict outcomes** | `guard_verdicts.resolution` | Attaching an outcome by id is blind and can overwrite an existing one |

### Tier 4: dashboard client state

| # | Machine | Stored in | Main hazard |
|---|---|---|---|
| 30 | **Notification edge detector** | `_prevSessionState` per tab | Diffs snapshots instead of receiving edges: notifies once per open tab, misses fast `active->waiting->active`, can fire a stale "finished" after reconnect |
| 31 | **Optimistic pause/resume** | `_workerLifecyclePending` + direct writes into the server's `sessions` entries | A lagging snapshot flips the badge back; other tabs see nothing |
| 32 | **SSE connection** | `_sse`, `_liveSSE`, `_sseRetries`, `_sseFallback`, `_sseGraceUntil`, `_pollTimer`, `online` | `_connState` is computed from seven flags; a forced reconnect during polling waits for the 30 s retry; `setOnline(true)` can run stream and poll together |
| 33 | **Offline outbox** | `localStorage.amux_offline_queue` entries with state + five flags; `amux_drafts` | Contradictory flag combinations (blocked + delivery_uncertain); replay order holds only within one pass and one tab |
| 34 | **Service worker / version reload** | `CACHE` name, `amux_ver_reload` throttle, `amux_app_html`, `amux_sw_error` | A second deploy within 10 minutes leaves a tab on old code; `amux_sw_error` is never cleared |
| 35 | **Peek overlay** | Global, DOM flags, sessionStorage, localStorage, URL hash (five places) | A save without a close reopens the peek on a later launch |
| 36 | **Composer send/queue mode** | `localStorage.amux_send_mode` | Queue quietly becomes send whenever a possibly stale snapshot says `waiting` |
| 37 | **Board column collapse** | `amux_col_collapsed` and three sibling keys | Entries for removed columns stay; unguarded `JSON.parse` throws at startup on a corrupt value |

### Tier 5: smaller lifecycles

Email send approvals (filename suffix as state; `approval_fate` scrapes its own
prose), connector status (derived from credentials and an old test file),
interactions phase (guessed from HTTP response keys), media transcode jobs
(stale takeover lets the old process overwrite the new row), reclaim scans and
quarantine (`interrupted` vs `cancelled` for the same reap; purge relabels
restored items), ChatGPT OAuth grants (expiry derived, token revocation a
separate timestamp), page asks (the worker is told to run raw `UPDATE`),
recordings (manifest state inferred from sidecar presence), invariant incidents
(`status` mixes verdict and close reason), turns, standing approvals,
conversation members, owner alerts, board signals, journal entries, desktop
recorder (`recovering` and `failed` documented and never set).

## Explicit ones, for contrast

`CommandState` + `QueuedCommand::apply` (used, persisted through
`db/commands.rs::transition`); `WorkerLifecycle` + `lifecycle_transition`
(CAS-guarded); `DeliveryState` + `advance_delivery` (correct, bypassed in the
DB); `TaskStatus` + `apply_transition` (correct, bypassed by `advance()`,
`Force` and raw SQL); tiering `Op { phase }`; `deploy_wake` `BootDecision`.
The core enums are right. The gap is that the writers do not go through them.

## Recommendation

1. **Worker status first (#1-#5).** This is the "proper handle of worker
   statuses" the owner asked for. One durable per-worker journal of edges
   (hook, process, transcript, lifecycle) and one reducer that produces a single
   status with its reason and source, stored and served; the dashboard renders
   it and stops deriving. Retire the second hook producer's divergent mapping.
   Run it beside the current badges for a day and count disagreements before
   switching.
2. **Make the board's core rules binding (#6-#8).** Route `db/advance.rs`, the
   raw-SQL writers and orchestrator quarantine through `apply_transition`; make
   `save_patched` check `version`; fix the `_amux_leases` generation; pick one
   lease system.
3. **One pattern for every queue (#11, #16-#19).** Claim and finish both
   compare-and-swap on `(state, claim generation)`; a finish that loses the
   race logs a verdict instead of overwriting.
4. **CHECK constraints on state columns** as each machine is touched, so a new
   spelling fails at write time instead of becoming a silent fourth state.
