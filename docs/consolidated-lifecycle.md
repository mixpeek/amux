# Amux consolidated lifecycle acceptance

One suite, one run directory, one evidence ledger. This consolidates the golden
scenarios, RR-0133 all-subsystem acceptance, the seven claims in
`docs/e2e-acceptance.md`, worker configurations, browser regressions and UX discovery.
Same-group and cross-group peer discovery, task awareness, request/reply boundaries,
changes-requested review, independent re-review and dependent integration are
first-class scenarios. Existing specs remain independently runnable; no coverage is deleted or duplicated
by importing test files into a giant order-dependent test.

## Run

```bash
npm ci
npx playwright install chromium webkit
python3 scripts/lifecycle/run.py plan
python3 scripts/lifecycle/run.py browser
python3 scripts/lifecycle/run.py full
```

`plan` lists every browser case and inventories supporting tests without starting a
server. `browser` builds this checkout, pins a private executable, boots separate
temporary homes for desktop Chromium, 375px Chromium and iPhone WebKit, and runs all
browser specs plus the connected journey. `full` also runs workspace syntax and
Rust unit/integration tests, existing opt-in real-provider/backend scenarios, and
the new autonomous worker journey. The shell/node regression files are inventoried;
only those invoked by existing Rust contracts or CI equivalents are automated here.

For the autonomous journey configure a dedicated amux test installation with a
working provider, an EMPTY scratch repository accessible to that installation,
the normal harness and board driver enabled, and these environment variables:

```bash
export AMUX_LIFECYCLE_LAB_URL=https://localhost:YOUR_TEST_PORT
export AMUX_LIFECYCLE_LAB_WORKSPACE=/absolute/path/to/empty/scratch-repo
export AMUX_LIFECYCLE_LAB_ACK=dedicated-test-instance
# Optional: AMUX_LIFECYCLE_PROVIDER=claude|codex|gemini|ollama
# Optional: AMUX_LIFECYCLE_STORAGE_STATE=/path/to/test-browser-auth.json
python3 scripts/lifecycle/run.py live
```

The live phase also runs two three-worker coordination journeys: all peers in one
group, then implementation in one group and review/integration in another. The
reviewer must reject a seeded defect, the author must revise, the reviewer must
approve, and the consumer must finish its dependent task. Durable message origin
and ordering, actual peer task IDs, final board state and independent artifact
checks are required; an operator writing a review file cannot substitute for the
peer-message evidence. The deterministic policy test separately checks discovery,
peer task reads, explicit cross-group denial and same-group isolation.

The lab is separate because the ordinary browser harness disables host-wide fleet
jobs and seeds a fake provider key. Those prerequisites cannot prove autonomous
work. Do not use a production URL. The live test creates one uniquely named worker
through the UI with three real deliverables. After submission the observer only
reads state: it does not PATCH tasks forward or supply their evidence. It records
the timeline, final card details, actual file bytes, and a rendered HTML artifact.
A timeout is a failure, with the unfinished tasks retained for diagnosis.

A focused development run is `browser --project desktop --grep LC-BOARD`.
`--binary /path/to/server` saves build time but records unverified source provenance;
it does not prove that binary corresponds to this checkout. Every invocation gets
a fresh output directory. Never combine stale evidence from previous runs.

## Coverage and verdicts

The runner emits `index.html`, `summary.json`, per-stage logs, the Playwright HTML
report, traces, videos, screenshots, discovered control inventories, and a copy of
`cases.json` initialized to NOT_RUN. PASS, FAIL, INCOMPLETE and NOT_RUN are distinct.
A skipped or unavailable real provider is not a pass. Exit 1 means failure, 2 means
incomplete, 0 means only that the selected automated scope passed. A `full` run
stays INCOMPLETE until a reviewer finishes the guided/visual ledger; there is no
automatic claim of total UI coverage. The ledger is a review artifact, not a way
to override automated failures.

For each case below record viewport/provider, exact action, entity IDs, screenshot,
read-only API or file proof, result and any failure reason. Follow each discovered
control into its dialog/popover and add a ledger row for new controls. A control
merely being present, or a mocked response rendering, is not a successful effect.
Do not equate crawler fixture coverage with shipped-dashboard coverage.

For **every button, menu item, tab, link, input, select, switch, draggable row and
keyboard shortcut** in each surface: open/use it, check the visible outcome, verify
persisted state where applicable, reload, cancel without mutation, try invalid and
empty input, and repeat at phone width. Also check keyboard focus, Tab order,
Enter/Escape, screen-reader name, disabled/loading state, clipping, touch reach,
long text and back navigation. Do not force-click or replace DOM state to make a
journey pass. For external sends use only a configured test sink and explicit
recipient authorization; otherwise leave the send case INCOMPLETE.

Inspect the images: desktop, 375px, real WebKit, light/dark, empty/populated,
loading/error, long text and on-screen keyboard. Record actual defects, including
which control is obscured. Retain the screenshot before changing anything.

## Ordered acceptance cases

Run in the order below within a scratch installation. The matrix is a canonical
operator script for coverage that is not yet fully automated. Automated tests are
supporting evidence; their existence does not pre-mark any of these rows PASS.

### LC-01 — Fresh install and onboarding

Open a fresh home; exercise setup, provider-key entry, walkthrough Next/Back/Skip/reopen, and empty-state Create.

Pass requires: No blank or permanently connecting screen; missing prerequisites are actionable; setup persists.

Supporting coverage: `e2e/settings.spec.ts`, `e2e/phase0.spec.ts`.

### LC-02 — Navigation and customization

Open every top-level tab, overflow menu, hide/show/reorder tabs, reload, use Back/Forward and a direct entity link.

Pass requires: Selected view and scope match the URL; hidden tabs remain discoverable; no offscreen controls.

Supporting coverage: `e2e/lifecycle/journey.spec.ts`, `e2e/tab-customizer.spec.ts`, `e2e/browser-history.spec.ts`.

### LC-03 — Worker creation

Create a worker from New worker; exercise name validation, provider/model, cwd autocomplete, template, branch and worktree choices; cancel a second creation.

Pass requires: Exactly one durable worker; correct provider/cwd/branch; cancellation creates nothing; no false successful start.

Supporting coverage: `e2e/lifecycle/live-journey.spec.ts`, `e2e/control-plane.spec.ts`.

### LC-04 — Worker start/stop/restart

Start, send a short task, observe running/working/idle, stop, restart and reopen terminal.

Pass requires: Real process and displayed state agree; output resumes once; unrelated workers are untouched.

Supporting coverage: `crates/amux-server/tests/golden_live.rs`.

### LC-05 — Worker menu and identity

Exercise every card-menu and peek-menu action, rename, duplicate/clone, pin/unpin, archive/restore and search; verify aliases and copied links.

Pass requires: Actions produce the named effect; rename preserves board/history identity; copies do not silently share identity.

Supporting coverage: `e2e/worker-action-parity.spec.ts`, `crates/amux-server/tests/rename_covers_every_session_table.rs`.

### LC-06 — Worker configuration changes

Edit description, label, cwd, branch, model, provider, effort, permissions, isolation and MCP; reload and restore.

Pass requires: Durable values match UI; restart/apply timing is explained; prior task context is retained.

Supporting coverage: `e2e/worker-configurations.spec.ts`, `crates/amux-server/tests/golden_remaining.rs`.

### LC-07 — Configuration inheritance

At global/group/worker levels edit memory, instructions, environment, rules, connectors, skin and gates; override then Inherit.

Pass requires: Source and effective value agree; sibling scope is unaffected; Inherit removes only the override.

Supporting coverage: `e2e/worker-configurations.spec.ts`.

### LC-08 — Isolated worker boundary

Create an isolated test worker and compare discovery, incoming peer messaging, harness and automation with a normal worker.

Pass requires: Isolation effects are visible and enforced; it cannot auto-pick up normal board work.

Supporting coverage: `e2e/isolated-worker.spec.ts`.

### LC-09 — Worker lists and working indicators

Filter/search/group/sort/freeze the worker list; put one worker on one task then let it finish.

Pass requires: Exactly the claimed task is Working now; counts, ordering and final idle state agree with authoritative data.

Supporting coverage: `e2e/working-now-accuracy.spec.ts`, `e2e/worker-card-counts.spec.ts`, `e2e/worker-status-order.spec.ts`.

### LC-10 — Groups lifecycle

Create/rename a group, add/remove workers, edit scoped configuration, switch scope, then remove the empty test group.

Pass requires: Membership and inherited settings persist; no cross-group data leakage or accidental mass mutation.

Supporting coverage: `crates/amux-server/tests/golden_remaining.rs`.

### LC-11 — Prompt to task decomposition

Submit one instruction with at least three distinct deliverables and dependencies through the worker composer.

Pass requires: Separate actionable tasks are created and linked to the source message; no untouched capture shell counts as work.

Supporting coverage: `e2e/lifecycle/live-journey.spec.ts`, `docs/e2e-acceptance.md`.

### LC-12 — Board create/edit/reload/export

Create through the UI with notes, owner, group, due time and gate; reopen, edit, search, filter, switch views and export Markdown/JSON.

Pass requires: Exactly one task persists, all fields and export match, empty search has a useful state, cancel preserves old values.

Supporting coverage: `e2e/lifecycle/journey.spec.ts`, `e2e/board-search-slim.spec.ts`, `e2e/board-slim-consumers.spec.ts`.

### LC-13 — Card detail and navigation

Open old/new deep links; inspect status, next action, owner, blockers, history, evidence and outputs; follow worker and file links.

Pass requires: The card alone explains work and next action; the exact linked entity opens; details fit every viewport.

Supporting coverage: `e2e/card-details.spec.ts`, `e2e/peek-path-links.spec.ts`.

### LC-14 — Column lifecycle and gates

Create custom test columns and gates; reorder them; edit a gate; transition a scratch task with a missing condition, then supply real proof.

Pass requires: The refusal names the unmet gate; permitted transition records actor/time; custom terminal state is honored.

Supporting coverage: `e2e/worker-configurations.spec.ts`, `crates/amux-server/tests/board_api.rs`.

### LC-15 — Backlog autonomous pickup

Create backlog work with a clear next action; leave the observer idle for several driver ticks.

Pass requires: Work progresses to todo and doing without observer nudges; history attributes the promotion.

Supporting coverage: `docs/e2e-acceptance.md`, `crates/amux-server/tests/golden_scenarios.rs`.

### LC-16 — Overdue ordering

Create three scratch backlog cards due seven, three and one days ago, with a deterministic tied-due pair.

Pass requires: Promotion is oldest due first, then stable ID order; partial promotion explains rate limit or WIP.

Supporting coverage: `docs/e2e-acceptance.md`.

### LC-17 — WIP and ownership

Queue work behind a claimed task; attempt a second claim and a competing-worker claim.

Pass requires: One owner/lease wins; blocked work explains WIP; after completion the next task starts once.

Supporting coverage: `e2e/queued-behind-wip.spec.ts`, `crates/amux-server/tests/task_graph.rs`.

### LC-18 — Dependency chain and cycle

Create parent/children and a blocked dependency; attempt a cycle; complete children through real work.

Pass requires: Cycles are rejected; parents remain blocked until all prerequisites meet their required terminal conditions.

Supporting coverage: `crates/amux-server/tests/golden_scenarios.rs`, `crates/amux-server/tests/task_graph.rs`.

### LC-19 — Review and negative gates

Attempt done without an artifact, without evidence, and with a false gate acknowledgment; then supply real results.

Pass requires: All invalid closures are refused visibly; legitimate closure preserves command, result and accessible output.

Supporting coverage: `crates/amux-server/tests/board_api.rs`, `docs/e2e-acceptance.md`.

### LC-20 — Blocked/needs-you/recovery

Create a genuine missing-input or dependency blocker; inspect card and worker; provide the input through normal UI.

Pass requires: Blocked state names reason/owner/next action; work resumes without resetting unrelated tasks.

Supporting coverage: `crates/amux-server/tests/golden_scenarios.rs`.

### LC-21 — Code change to finished artifact

Run the three-part live journey: implement sum, test it, add non-finite validation, retest, produce HTML and Markdown results.

Pass requires: Actual files change, independent tests pass, tasks finish with evidence; generated HTML renders its unique run marker.

Supporting coverage: `e2e/lifecycle/live-journey.spec.ts`.

### LC-22 — Verification and reopen

Independently run the artifact tests, inspect its rendered result, verify task, then reopen with a concrete correction and repeat.

Pass requires: Verified is backed by fresh checks; reopened work is driven back to a terminal state; history retains both cycles.

Supporting coverage: `crates/amux-server/tests/golden_scenarios.rs`, `crates/amux-server/tests/golden_remaining.rs`.

### LC-23 — Board archive/discard/cleanup

Archive/restore and discard only test cards; inspect terminal availability; try the column migration dialog then Cancel.

Pass requires: No unintended card moves; cancelled migration changes nothing; archived/discarded items remain traceable.

Supporting coverage: `crates/amux-server/tests/board_api.rs`.

### LC-24 — Composer and delivery

Type/paste multiline text, expand composer, switch send modes, send once, inspect pending/sent/failed, retry a controlled failure.

Pass requires: Exact text and origin reach the right worker once; drafts survive failures and worker switching.

Supporting coverage: `e2e/message-resend.spec.ts`, `e2e/suggestion-control-verdict.spec.ts`.

### LC-25 — Message-to-task links

Send multipart work and inspect message card chips; open each child task and navigate back.

Pass requires: Every durable card link opens the exact child and its state/artifacts; source kind and MSG ID remain truthful.

Supporting coverage: `e2e/terminal-message-navigation.spec.ts`, `e2e/msg-id-copy.spec.ts`, `e2e/messages-default-kind.spec.ts`.

### LC-26 — Peer requests and callbacks

In a test-only group make one worker request work from another, wait for completion/callback, inspect both boards.

Pass requires: Own-board boundary enforced; links and callbacks preserve attribution; no duplicate unowned task escape.

Supporting coverage: `e2e/worker-request-callback.spec.ts`.

### LC-27 — Terminal reading and controls

Open terminal/transcript; navigate earlier/later messages, source/content filters, Find, copy, path links and focus mode; test long output.

Pass requires: Scroll anchors and filter selection survive refresh; no cross-worker output/draft/card mixing; toolbar is reachable.

Supporting coverage: `e2e/terminal-message-navigation.spec.ts`, `e2e/terminal-scroll-accuracy.spec.ts`, `e2e/peek-default-tab.spec.ts`.

### LC-28 — Read-aloud and media controls

Start/pause/resume/seek/close read-aloud, change worker mid-playback, and interrupt playback.

Pass requires: Player controls match audio state, stale playback stops appropriately, and no inaccessible overlay remains.

Supporting coverage: `e2e/read-aloud-player.spec.ts`.

### LC-29 — File upload and retry

Upload a text file, close peek mid-transfer, cancel another, force failure, retry and download.

Pass requires: File bytes match; transfer survives closing; cancel is not an error; failed chip retains its retryable file.

Supporting coverage: `e2e/upload-chip-escape.spec.ts`.

### LC-30 — Files and previews

Browse scratch directories, breadcrumb, search, open text/image/PDF/HTML, edit/save/reopen, create/rename/delete scratch files.

Pass requires: Correct file and content open; binary/large/missing files explain limitations; previews fit phone screens.

Supporting coverage: `e2e/peek-path-links.spec.ts`, `e2e/worker-action-parity.spec.ts`.

### LC-31 — Memories and instructions

Create/read/edit/delete worker memory, inspect inherited memory, search and reload; have worker use a unique fact.

Pass requires: Version/content and effective scope persist; subsequent worker output uses the saved fact.

Supporting coverage: `crates/amux-server/tests/golden_live.rs`.

### LC-32 — Scheduler lifecycle

Create schedule, choose worker/cadence/timezone, edit, disable/enable, run now, wait for natural fire, then delete.

Pass requires: One attributed run per trigger; disabled schedule does not fire; history includes edited/deleted jobs.

Supporting coverage: `e2e/scheduler-audit.spec.ts`, `crates/amux-server/tests/system_jobs.rs`.

### LC-33 — System jobs and stalled work

Open SYSTEM jobs, expand details, compare healthy/stalled/disabled jobs; use Run now only on the dedicated lab.

Pass requires: UI agrees with registry/timestamps; stalled job is distinct; automation actually advances eligible tasks.

Supporting coverage: `e2e/system-jobs.spec.ts`.

### LC-34 — Calendar and timezone

Create/edit/move/delete a scratch calendar event; inspect all-day/timezone/day boundaries and exported iCal.

Pass requires: UI and feed represent the same event and timezone; deletion removes only that event.

Supporting coverage: `docs/rust-rebuild-plan.md`.

### LC-35 — Browser profiles and history

Create test profile, start, navigate local artifact, view live screen, screenshot, history Back/Forward, open/close tab and stop.

Pass requires: Real browser output updates; active/locked/unavailable states are distinguishable; process ends after Stop.

Supporting coverage: `e2e/browser-history.spec.ts`, `e2e/browser-liveview-fail.spec.ts`.

### LC-36 — Email draft and test send

Create/edit/discard a draft; with explicit test-recipient authorization send to a controlled sink and inspect Sent.

Pass requires: Draft persists; actual test delivery and Sent agree; failure preserves draft and visible error.

Supporting coverage: `docs/rust-rebuild-plan.md`.

### LC-37 — Connectors and authentication

Add a test connector, configure scope, connect/disconnect, deny and expire auth, test and retry.

Pass requires: Auth health and scoped availability are accurate; credentials are masked; revoked connector cannot act.

Supporting coverage: `e2e/settings.spec.ts`.

### LC-38 — Environment and provider keys

Edit a test env variable/key through supported UI, reload, clear override, enter invalid value and test unavailable provider.

Pass requires: Saved/effective config agree without stale refresh overwrite; absent/invalid key has a recoverable state.

Supporting coverage: `e2e/settings.spec.ts`.

### LC-39 — Search and cross-entity discovery

Create uniquely named worker/task/group/memory/file; search globally and within tabs, filters, zero results and deep links.

Pass requires: Every supported entity type is findable and opens correct scope; unsupported search domains are recorded as gaps.

Supporting coverage: `crates/amux-server/tests/search_index.rs`, `e2e/board-search-slim.spec.ts`.

### LC-40 — Offline write/reconnect

Go offline, create three cards and edit existing work, reload offline, reconnect and wait for replay.

Pass requires: Durable queue retains exact intent, server receives each mutation once, UI converges and queue drains.

Supporting coverage: `e2e/golden.spec.ts`, `e2e/outbox-connectivity.spec.ts`.

### LC-41 — Concurrent clients and conflicts

Open two clients; edit the same task and distinct tasks concurrently; reconnect a stale client.

Pass requires: Visible revision conflict protects both drafts; unrelated changes converge with no lost update.

Supporting coverage: `e2e/local-multiplayer.spec.ts`, `e2e/golden.spec.ts`.

### LC-42 — Connection failures and recovery

Interrupt SSE without a clean close, cut network, restore; provoke bad auth and unavailable API.

Pass requires: Status reflects usable data, zombie stream recovers, auth failure differs from retryable outage, no endless silent spinner.

Supporting coverage: `e2e/golden.spec.ts`, `e2e/conn-history-classify.spec.ts`.

### LC-43 — Service worker and PWA

Install/open PWA, offline launch, cached-version upgrade, failure bar dismissal, save below banner.

Pass requires: Upgrade does not lose drafts; failure explains offline limits; Save stays tappable on phone.

Supporting coverage: `e2e/sw-fail-bar.spec.ts`.

### LC-44 — Settings appearance and device

Exercise every settings tab and control: theme, zoom, tabs, offline limits, device name, defaults, connections, About, devtools and walkthrough.

Pass requires: Persisted controls survive reload; local-only settings remain local; modals close by keyboard and pointer.

Supporting coverage: `e2e/settings.spec.ts`.

### LC-45 — Usage, cost and limits

Run real work; inspect token/cost ledger, worker/task attribution, limits, budget exhaustion and recovery.

Pass requires: Nonzero real usage appears under correct identity; unknown/unmeasured is not zero; budget stops and recovery are clear.

Supporting coverage: `e2e/settings.spec.ts`, `crates/amux-server/tests/golden_remaining.rs`.

### LC-46 — Logs and diagnostics

Inspect logs search/filter/details, measured diagnostic endpoints, invariant failures and client action error.

Pass requires: Failed actions produce an attributable diagnostic signal; measured/n_considered distinguish empty from unmeasured.

Supporting coverage: `crates/amux-server/tests/diagnostic_contract.rs`, `e2e/worker-toolbar-boot.spec.ts`.

### LC-47 — Workspace, map and metrics

Open Workspace grid, add/remove panes, resize/focus, switch worker; open map/graph and metrics filters.

Pass requires: Pane identity remains correct; graph/metrics reflect scratch tasks and terminal states; no stale cross-worker data.

Supporting coverage: `e2e/lifecycle/journey.spec.ts`.

### LC-48 — Auxiliary tabs and empty capabilities

Inspect MDAI, Proxies, Skills, Database, Torrents, journal/habits where enabled; use their creation/edit/cancel controls on test fixtures.

Pass requires: Each advertised control works or explains its prerequisite; missing capabilities are recorded, not silently skipped.

Supporting coverage: `e2e/lifecycle/journey.spec.ts`.

### LC-49 — Multiplayer scope and revocation

Invite test members/devices, restrict group access, edit concurrently, revoke one member and retest both clients.

Pass requires: Authorized client continues; revoked client loses access; no hidden mutation leaks across scope.

Supporting coverage: `e2e/local-multiplayer.spec.ts`.

### LC-50 — Provider/model continuity

Repeat core task flow on installed providers; switch model/provider mid-work and inspect continuation, rate-limit recovery and cancellation.

Pass requires: Same durable task context and identity survive supported swaps; unavailable providers remain INCOMPLETE.

Supporting coverage: `crates/amux-server/tests/golden_remaining.rs`.

### LC-51 — Restart and persistence

Restart only dedicated lab server and worker process during queued and active work; reconnect UI.

Pass requires: Tasks/messages/files/gates persist; ownership reconciles; no duplicate execution; build identity brackets evidence.

Supporting coverage: `crates/amux-server/tests/restart_persistence.rs`, `crates/amux-server/tests/backend_conformance.rs`.

### LC-52 — Visual and accessibility sweep

Inspect each checkpoint and every open dialog/popover across desktop, 375px, iPhone WebKit, light/dark, empty/populated/error/long text.

Pass requires: No clipping/overlap or inaccessible action; focus/labels/keyboard/touch work; discovered controls have explicit effect-verification rows.

Supporting coverage: `e2e/lifecycle/journey.spec.ts`, `e2e/ux-discovery/crawler.ts`.

### LC-54 — Same-group peer awareness

Create author, reviewer and consumer in one test group. Each reads its own and peers’ actual tasks, identifies ownership/status/blockers and names the exact IDs in messages. Repeat discovery and peer task reads with a worker that has no group, in both directions.

Pass requires: Roster and board facts are correct without operator-pasted task context; same-group access is a positive control.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`, `e2e/lifecycle/ungrouped.spec.ts`.

### LC-55 — Cross-group awareness and delivery

Place author in build and reviewer/consumer in quality. Discover peers, inspect permitted task context, request work and wait for response with normal open defaults.

Pass requires: Actual originated peer messages cross groups; request and response identify the right cards and artifacts.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-56 — Cross-group deny/allow/inherit

Set a worker/group deny, attempt a new cross-group request, allow one destination, retry, clear override and inspect effective policy.

Pass requires: Denied request is visibly refused with no delivered work; allowed destination succeeds; inheritance restores the documented default, not blanket access by accident.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-57 — Reply exception and isolation

With initiation restricted, test a reply to an actual incoming request, an unsolicited new message, and a message to an isolated worker in the same group.

Pass requires: Documented reply path works only with genuine inbound history; unsolicited initiation respects policy; isolation cannot be bypassed by group membership.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-58 — Peer review requests changes

Author supplies a deliberate empty-input defect and requests review. Reviewer discovers the task, independently runs the failing test, owns a review task and sends REVIEW_CHANGES.

Pass requires: Reviewer is a different worker, cites actual task/file/failure, and the implementation cannot be treated as approved before repair.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-59 — Revision and independent re-review

Author revises after reviewer feedback, records the changed artifact and tests, asks for re-review. Reviewer reruns tests and sends REVIEW_APPROVED.

Pass requires: Durable message timestamps prove rejection precedes approval; approval refers to the revised work and actual successful tests, not the old artifact.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-60 — Dependent integration and callback

Consumer reads author/reviewer tasks, creates its own dependent integration task, waits for approval, tests the artifact, and sends HANDOFF_DONE.

Pass requires: Integration finishes after approval; linked task IDs belong to the correct peers; callback wakes requester and all three workers reach evidenced terminal states.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-61 — No peer ownership hijack or duplicate dispatch

Two workers attempt to claim one task; repeat request/callback delivery; ask a peer for work without changing its owned task directly.

Pass requires: One claim wins; one callback effect; task ownership and verified message origin stay accurate; duplicate work is not manufactured.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-62 — Unavailable peer and recovery

Stop reviewer before request, resume it later; separately test archived, isolated, error and rate-limited reviewers.

Pass requires: Requester shows who/what it awaits, retains its next action, avoids false completion, and resumes when the actual reviewer returns.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-63 — Group changes during coordination

Move reviewer between test groups or change its scope while a review is pending, then refresh both workers and both dashboard clients.

Pass requires: Effective policy and peer awareness refresh; pending work remains attributable and either completes through permitted routing or visibly explains the new restriction.

Supporting coverage: `e2e/lifecycle/coordination.spec.ts`, `e2e/lifecycle/live-coordination.spec.ts`, `e2e/worker-request-callback.spec.ts`, `e2e/isolated-worker.spec.ts`.

### LC-53 — Final state and run-owned cleanup

Inventory every run-owned entity; inspect all deliverables, verify results, stop/archive test workers and disable/delete test schedules.

Pass requires: All requested work is terminal with proof; no active leases, duplicate dispatches or orphan jobs; unresolved work is explicitly failed/incomplete.

Supporting coverage: `e2e/lifecycle/live-journey.spec.ts`.

## End-state and cleanup record

For every created worker, card, dependency, schedule, browser profile, group,
file, memory and external fixture, record its ID and final state. Completed work
must have a real artifact and command/result evidence. Review/blocked/needsyou is
not completion; report the exact blocker and next action. Confirm leases/active
indicators settle, no duplicate dispatch occurs, and no abandoned recurring job
continues running. Stop/archive only the run-owned workers and schedules after
capturing evidence. Keep the run directory and outputs until reviewed.

When a task stalls, distinguish a pending interval/mid-turn from a gate,
dependency, WIP cap, missing provider, failed delivery or exhausted budget. Capture
`/api/board/ready?session=...`, `/api/debug/board-drive`, health build, and the card
history. Do not repair the state from the observer to turn a failure green.

Source inventory is generated on every run with SHA-256 hashes. New browser specs
are automatically included. Review uncommitted tests in other checkouts separately;
a stable run of this checkout cannot certify concurrent drafts. The inherited
Playwright startup banner describes configured browser targets; the report’s
Selection field and executed test counts are the authoritative scope of this run.

## Two Sonnet workers and uploads

Run the focused real-provider scenario with:

```bash
python3 scripts/lifecycle/run.py live --grep LC-SONNET
```

It creates exactly two Claude workers using `--model sonnet` in one group. The
reviewer must reproduce the seeded failure before approval, the author must revise,
and both must finish their own work. Message origin, ordering, real card IDs,
terminal search/navigation, final evidence, independent execution of the resulting
module, and desktop/phone rendering are checked. A final report's boolean about
outstanding changes does not replace the durable changes-requested message.

Use a dedicated server/home, a private `TMUX_TMPDIR`, and a scratch Git repository.
Export both `CC_HOME` and `AMUX_HOME` to that home and `AMUX_API` and `AMUX_URL` to
that server in the worker environment. Before launching workers, run the checkout's
`amux url --verify` with those variables and verify the printed endpoint. The CLI
now resolves its configured home's endpoint consistently for every verb. Put this
checkout's CLI first on the lab PATH; testing an older installed client measures
that older client instead.

For subscription authentication, retain the provider's existing login. Isolate the
server's transcript/usage discovery to the scratch project; importing the host's
entire history can trip the lab's spend circuit. If the user's login shell changes
cwd, use a lab-only `CLAUDE_ENV_FILE` to restore the scratch directory and lab
variables before Bash commands. Do not edit the user's shell profile.

`AMUX_LIFECYCLE_PAIR_RUN=<existing run>` and
`AMUX_LIFECYCLE_PAIR_OBSERVE=1` resume read-only observation after diagnosis. The
report identifies observation mode; it is not evidence of a clean autonomous run.
Retain earlier failed runs and record any operator steering separately.

The ordinary browser phase now includes real multi-chunk text upload, Unicode and
long filenames, image preview, SHA-256 verification of downloaded bytes, attachment
removal and worker switching. Controlled delivery tests separately exercise text
and attachment retention until acceptance, permanent refusals, durable offline
queuing, duplicate-tap protection, and typing the next draft while a send is pending.
These controlled transport tests do not substitute for live model execution.

Browser discovery uses a private tmux socket. Focused invocations start only the
selected project's server. `AMUX_LIFECYCLE_PORT` changes the base port when running
separate projects concurrently; each invocation still needs its own output directory.


The Sonnet selection runs the review pair first, then reuses the author in a fresh
conversation for a real `fruit-counts.csv` upload. It requires the worker to read
the uploaded path, produce a JSON receipt (two rows, total six), and finish its
own task with the receipt as evidence. No receipt or peer review is fabricated by
the observer. The pair's HTML must fit both 375px and 1280px.

`AMUX_LIFECYCLE_PAIR_RUN` is generated once per live invocation and shared by the
pair and upload cases. To inspect an existing upload without submitting new work,
set `AMUX_LIFECYCLE_UPLOAD_OBSERVE=1`, the existing pair run, and optionally
`AMUX_LIFECYCLE_UPLOAD_RECEIPT` to its receipt filename. Observation is recorded
explicitly and is not a claim that this invocation created or drove the workers.
Observe the pair before resetting its author for upload: a new conversation does
not retain the old terminal's searchable content.

`LC-FILES-UPLOAD` also exercises the Files tab's upload, preview, rename, download,
and delete controls, including the mobile More menu. Downloaded bytes must equal
the original uploaded bytes. Composer refusal tests cover peek and card Send /
Queue and the Control+Enter retry shortcut.

On a machine with other active checkouts, use a private `node_modules` installed
with `npm ci` and a private browser cache. Do not symlink another lane's mutable
dependencies: browser revisions can disappear mid-run when that lane upgrades.
For example, set `PLAYWRIGHT_BROWSERS_PATH` to a task-owned directory and run
`npx playwright install chromium webkit` before starting the suite. Keep the same
environment for the suite, and do not replace its pinned executable during a run.

The live pair uses Amux's Bash `amux send` transport explicitly. Claude's native
`SendMessage` can reach another Claude session while bypassing Amux's history,
which does not prove Amux routing, verified origins, or policy. The Messages
check selects the Session filter and searches the visible message list; text in
an inactive Terminal panel cannot satisfy it.

For a manually provisioned tmux lab, create `TMUX_TMPDIR` before starting the
server, unset inherited `TMUX`/`TMUX_PANE`, and verify the actual socket before
creating workers. A nonexistent `TMUX_TMPDIR` can make tmux fall back to its shared
socket. The bundled browser harness creates its private directory itself.

The final `LC-SONNET-CROSSGROUP` phase reuses those same two workers, moves the
reviewer into a different group through the UI, and requires each worker to read
the other's real completed task metadata. Each writes an independently checked
receipt, sends Amux messages with verified origins, and finishes its own new chore.
Both terminal search and the visible Session messages are checked at desktop and
phone widths. `AMUX_LIFECYCLE_CROSSGROUP_OBSERVE=1` observes an existing completed
phase without changing groups or sending new prompts.

`LC-SONNET-QUEUE` is the final acceptance boundary. It creates four real chore
cards on the existing pair: each worker gets one Backlog and one To Do card,
with dependencies in opposite directions. After creation the observer only reads;
it never sends a wake-up, claims work, changes status, writes receipts or completes
cards. Every seeded card must reach done/verified with a correct worker-written
receipt and evidence. Every remaining run-owned capture must also be resolved by
its worker. A completed handoff alone does not satisfy this boundary.
`AMUX_LIFECYCLE_QUEUE_OBSERVE=1` checks an existing run without creating cards.

### Linked work, revised verification, and reliable sending

The suite also includes these connected acceptance cases:

- **LC-LINKED-RECORD** opens an epic, children and dependencies, follows a source
  message into Messages, previews a produced file and `file://` URL, opens a real
  web URL and inspects the actual Git commit. It repeats on desktop, phone and Safari.
- **LC-GATE-REVISION** verifies a task, changes its gate in the task editor, checks
  that retained evidence is labeled as covering the older criteria, refuses an old
  checklist and explicitly rechecks the new one. Typed criteria have server-owned
  versions; independent verification must rerun the current version. A checklist
  acknowledgement is displayed separately from an independently executed test.
- **LC-COMPLEX-VERIFIED** creates two Claude Sonnet workers in one private group.
  They decompose an invoice reconciliation project into two linked epics and at
  least five dependent children, implement a CLI and responsive report, exchange
  review messages, register artifacts and commits, and finish phase one at Done.
  The observer then changes the Verified gate to add duplicate-ID and negative
  amount rejection. The peers implement the amendment and independently execute
  verification of each other's work. Every real deliverable and epic must reach
  Verified with current criteria and no remaining open run-owned work. The
  observer never supplies completion evidence or advances those cards.
- **LC-SEMANTIC-INTAKE** uses the real configured helper model to append a
  paraphrase, update refined requirements and create distinct deliverables.
  It checks the resulting IDs and preserved context, not only the classifier's
  explanation. Comparison is scoped to open work with the same ownership.
  Explicit graph, gate, callback or scheduling metadata is preserved as its own
  record; ambiguous or unavailable comparison preserves the incoming request.
  The intake receipt records whether comparison ran and the candidate count.
- **LC-LOCAL-OUTBOX** uploads a file and sends a multiline draft while the server
  is delayed. Local persistence must clear only the accepted draft immediately,
  preserve attachment references and one message ID through reload/retry, and
  keep newer edits. Refused or ambiguous delivery stays in the outbox for review.
  Storage failure must retain the draft and cause zero network submissions.

Focused live commands still require the dedicated lab variables above. Set
`AMUX_HELPER_MODEL=sonnet` on that lab server to exercise semantic comparison
with Sonnet too. Run the browser cases with the consolidated configuration, and
run both live cases with `e2e/lifecycle/live.config.ts`. They are automatically
included by the consolidated runner's existing discovery. Real worker tests
may take substantially longer than fixture tests; their timeout is a failure,
not permission to manufacture a terminal state.

The browser runner now checks the **served** `app.js`, `app.css` and `sw.js`
hashes before executing cases. Each isolated server writes an
`asset-provenance-<port>.json` receipt with its `/health` build identity and the
expected/actual hashes. A stale shared-build embed refuses the run immediately;
a successful cargo exit alone does not establish dashboard provenance.

Install the current Bash client into the dedicated lab with `make install-cli
BIN_DIR=<lab>/bin` before starting real workers. An old installed client can have
different retry behavior even when the server is current. Keep that installation
receipt with the live evidence. `scripts/test-board-help.py` covers read-only
artifact/decomposition discovery; help must never register an output.

If a provider limit interrupts the complex case before the gate amendment, keep
its failed run and resume the **same** workers and files using
`AMUX_LIFECYCLE_COMPLEX_RESUME_PHASE1=1` with the original
`AMUX_LIFECYCLE_COMPLEX_RUN`. This sends `continue` to each existing worker and
still requires all phase-one work, the real gate amendment, and independent
verification. Use `AMUX_LIFECYCLE_COMPLEX_OBSERVE=1` only after the amendment was
actually delivered. Record any permission-dialog cancellation, environment repair,
or resume separately; resumed work is not an uninterrupted autonomy result.

Each browser project has its own server, home and tmux socket. The consolidated
runner permits the three projects to run concurrently but limits each project
to one worker, preserving serialization of global settings within that server.
The report names the actual selected scope; a focused run remains partial.

The complex observer follows the workers' registered output paths, including
subdirectories, and reads the resulting bytes through the file API. Additional
epics created for a criteria amendment are legitimate work: they must also
finish, with current independent verification. The seven original minimum task
IDs are checked in the completion receipt, and every additional Verified card
must meet the same verification assertions. Preserve extra operator review
findings and interventions alongside the run rather than describing a resumed
or steered run as uninterrupted.

Semantic intake currently considers up to 80 recent open candidates within the
same worker/human ownership scope; receipts expose both considered and available
counts. It does not merge across owners or reopen completed tasks automatically.

Message retries must also distinguish a server reservation from an acceptance
receipt. `message_acceptance_` server tests cover simultaneous retries, refused
steering, unavailable identity storage, old rows without receipts, and long
response-loss windows. A pending attempt returns a retryable failure; after two
minutes an unresolved reservation requires terminal review. It cannot become
"already delivered" merely by existing or aging out. Confirmed receipts retain
the original response ID for 30 days. Logs use `amux::message_acceptance` to
identify pending, uncertain, or unrecorded acceptance.

### Expanded verification receipts (2026-09-10)

See [the recorded validation report](lifecycle-validation-2026-09-10.md) for the
actual Sonnet run, retained failures, repairs, and scope boundaries. Set
`AMUX_LIFECYCLE_INTERVENTIONS` to a local JSON file to attach operator actions to
the complex run's proof. Completion receipts may name one epic or an array of
epics per worker; every named epic must have current independent verification,
linked messages, and direct output references. After workers stop, the observer
loads saved terminal records through the UI before searching peer messages.

The consolidated browser suite also covers delayed nonempty history snapshots,
identical messages sent to different peers, full phone Verified headers, and
worker menus surviving scroll events from unrelated panels. Queue unit contracts
execute the shipped functions and check automatic replay while connectivity is
believed offline, plus quiet normal sends and visible stuck-send status.
