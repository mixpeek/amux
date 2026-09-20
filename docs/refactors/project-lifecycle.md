# Project execution refactor

Branch: `codex/amux-project-lifecycle`  
Baseline: `38262d01` (2026-09-20)  
Status: all six branch stages implemented and validated; branch only. No production board migration or main deployment is part of this branch build.

## Product contract

A person submits an outcome to a project, sees the bounded work and its evidence, and receives a verified result. A worker is an executor, not the owner of a separate backlog. Worktrees, claims, provider processes and integration retries are managed execution resources.

A project is an execution configuration of the existing **group** primitive. Its board is a view of the existing **issues** ledger. This does not add a parallel task database, a second kind of epic, or an always-running model coordinator.

The default is one executor. Explicitly independent work can use at most two additional executors under a configured project budget. One task has one current claim. A coordinator is a model profile invoked for intake or a specific unresolved planning decision, not a continuously polling agent. Executor and coordinator provider/model choices are independent.

## Evidence from the existing implementation

- `api/board_lifecycle.rs` already owns durable interpretation in `cmd_history`, cached model decisions, semantic reconciliation, atomic graph application and bounded interpretation attempts. Reuse it.
- `runtime_jobs/board_drive.rs` handles tmux worker board selection, capture repair, backlog promotion, reminders, blockers, verification, epics and retirement. At the baseline it is 14,003 lines including tests.
- `orchestrator/runtime.rs` drives the registered Rust/protocol worker population through `amux_core::orchestrator::plan_tick`. It is a distinct backend population, not proof that every task is double-dispatched. Unification must preserve both adapters and demonstrate one claim authority.
- `api/board.rs` mixes transport, ownership, lifecycle gates, fan-out and orchestration launch (17,395 lines including tests).
- `api/session_verbs.rs` contains provider and terminal operations as well as task capture and delivery (35,932 lines including tests). Preserve proven provider behavior; remove board-policy decisions from the transport boundary in stages.
- `fanout_workspace.rs` already provides durable isolated checkouts and validated main integration. `fanout_retirement.rs` already proves full verification, current remote-main ancestry, safe disposal and expiration. Preserve these invariants.
- Existing cost controls already bound intake concurrency and attempts. Existing task/checkpoint, handoff, token ledger, revision, delivery and scope primitives should be used instead of introducing overlapping records.
- Earlier lifecycle validation scored 2/10, 3/10 and 5/10 across its three Haiku trials. This refactor must not rename those results as success or start additional paid trials without a new explicit request.
- The baseline has a pre-existing failing dashboard outage-recovery contract: `_peekPollNow` is missing from the shipped JavaScript. Preserve the failing evidence and repair the real polling behavior while validating the new project UI; do not remove the test.

Line counts identify review surfaces, not a performance benchmark. No token-saving percentage is claimed before comparable measurements exist.

## Ownership and state

The issue ledger gains an explicit nullable project/group ownership key. Existing `session` remains execution assignment for migrated project cards; legacy cards retain their existing meaning until an explicit migration. Retirement must never erase project ownership. Tasks keep their IDs, original command links, criteria, evidence, history, and revisions.

Project UI states are **Ready → Working → Verifying → Verified**. Existing storage statuses remain readable during migration: Backlog/To Do project to Ready only when structured and actually executable; Doing projects to Working; Review/Done project to Verifying; Verified remains Verified. Raw requests are pending intake, not executable tasks. Discarded/cancelled/duplicate are explicit dispositions, never successful completion. Waiting is a reason attached to a phase, not an undocumented holding column.

Dependencies name required outputs within the same project. They must not name worker availability or another worker's board status. A missing prerequisite becomes local work linked to the same accepted outcome. Cross-project references can supply evidence, not implicit blocking edges. Existing access restrictions remain real restrictions; removing a dependency does not grant credentials.

Approval policy retains the user's standing categories: increased budget/spend and customer outbound without prior authorization. Missing access, provider limits, failing checks and exhausted recovery are operational waiting reasons with a concrete remedy. They must not become invented human approval requests or falsely completed work.

## One execution path

1. Accept a command durably with a client idempotency key and project identity.
2. Reuse a saved interpretation for the same accepted command. Use the existing bounded semantic intake for new or materially changed intent; search compact candidates within the project, including reusable verified evidence.
3. Apply the outcome graph atomically to existing issues. Preserve provenance and report create/update/merge/reuse decisions. A follow-up does not silently replace a running task's criteria.
4. Compute readiness, resource eligibility, gates and priority without a model. Reserve one claim atomically before starting an executor or delivering a prompt.
5. Deliver one bounded task packet through the existing durable delivery adapter. Include the outcome, criteria, current artifacts, prior failed attempt and remaining budget. Never resend the whole board by default.
6. Reconcile progress from structured results and durable artifacts. A timer can inspect leases/process health; it cannot purchase another model turn merely because time passed.
7. Verify current output against current criteria, integrate code through the existing candidate verifier and remote-main checks, then record Verified. A status change alone is not evidence.
8. Release the claim. Reuse a healthy executor for ready compatible work where safe; otherwise expire it only after its assigned work is fully verified and its clean head is integrated. Preserve dirty/unmerged work on every failure path.

The read model and the scheduler must consume the same readiness and waiting reasons. Once a project uses the new driver, legacy pickup, capture nudges, generic advancement prompts and protocol assignments must not also dispatch that project's work. Legacy workers remain supported by their existing path until migrated. Compatibility code needs an explicit population and removal gate.

## Token and resource accounting

Every model-bearing action has an attribution: project, original command/outcome, task, phase, attempt and provider/model. Reuse the provider-reported input/output/cache counters and measured cost where available. Unknown is not zero; characters are not tokens; subscription quota is not a dollar bill.

Track useful execution separately from intake, verification, recovery and coordination. Count verified accepted outcomes as the denominator; generating extra tasks must not improve the metric. Keep failed and abandoned outcome costs visible.

Default behavior:

- Zero model calls for unchanged idle state, scheduling, dependency readiness, progress display, lease recovery decisions and cleanup eligibility.
- One intake interpretation plus at most one informed repair for malformed/uncertain output, using the existing durable attempt record.
- No periodic whole-board semantic re-triage or generic 'continue' nudges.
- A repeated unchanged failure produces one durable waiting/recovery result. A new attempt needs changed relevant evidence, a configured retry schedule for a concrete transient condition, or an explicit operator action.
- Do not launch extra executors without independent ready work, available project capacity and the configured budget policy.
- Budget limits must distinguish enforceable provider limits from observed usage/estimates. Reserve configured spend before dispatch where the adapter supports it; missing telemetry must remain visible. Never claim a hard dollar cap on a provider that cannot enforce one.
- Pause stops active execution and suppresses dispatch. Archive/isolation and authentication boundaries remain authoritative.

## Stages and completion gates

| Stage | Deliverable | Required proof |
| --- | --- | --- |
| 1. Contract and ownership | Shared project policy, persistent issue ownership/assignment separation, migration preview | Idempotent migration; preserved IDs/history/evidence; explicit ambiguous mappings; no production mutation |
| 2. Intake and reconciliation | Project command endpoint using the existing durable interpreter and ledger | Duplicate/retried command uses one interpretation; refinements reuse canonical tasks; uncertain output creates no executable junk; cross-project mutations refused |
| 3. Execution | One project planner and claim path with existing provider/delivery adapters | Single lane completes; optional independent fan-out; no double dispatch; restart/expired claim recovery; paused/busy/unknown workers excluded |
| 4. Verification and resources | Current evidence, integration and retirement wired to project ownership | Stale evidence cannot verify; merge conflict is recovered locally; dirty/unmerged work preserved; verified clean worker expires and loses worktree |
| 5. Project UI and accounting | Project board shows outcome, phase, current task, waiting reason, budget/usage; worker details secondary | UI reads the scheduler projection; drafts/retries persist; provider telemetry shows coverage; terminal polling contracts pass |
| 6. Migration and deletion | Explicit dry-run/apply/rollback migration, old dispatch excluded for migrated work, obsolete policy branches removed where replaced | Equivalent retained behavior on legacy population; one dispatch owner per migrated task; mixed-mode restart, scope and failure tests; branch CI reports actual results |

The ownership contract is the first implementation commit. Intake, claims, verification and migration share transaction boundaries and land together as the connected backend stage; the mounted UI and acceptance harness form the next stage. This keeps each implementation commit buildable. A stage is complete only when its paths are connected and tested; unused scaffolding and an unmounted demo API do not count. This checklist is updated with actual evidence as work lands.

## Migration

Migration is explicit and project-scoped. Preview maps selected worker boards to a project and names ambiguous membership (multiple groups, shared repositories with distinct outcomes, archived/isolated lanes, missing roots), cross-board prerequisites and stale claims. Repository path alone does not determine project identity.

Apply uses revisions and a transaction to attach project ownership while preserving issue IDs and audit records. Do not move or close cards based on title similarity alone. Semantic merges require a canonical survivor and retained source-command links; historical Done is not promoted to Verified. Paused and isolated populations require explicit inclusion and remain paused/isolated.

A migrated project's old driver is disabled atomically with enabling the new claim path. Rollback stops new dispatch, drains/cancels its owned outbox actions, restores recorded assignments only when revisions still match, and reports changed rows for reconciliation. It never discards new work or resets a live database wholesale. Existing schema remains backward-readable; no destructive schema down migration is required.

## Validation matrix

Run model-free fixtures first, using the shipped planner, store, router and adapters with injected provider outcomes. No new paid workers are required for branch validation.

- One command → structured tasks → one executor → checked result → integration → Verified → retirement.
- Independent tasks fan out within capacity; dependent tasks remain local and acquire their input by evidence.
- Duplicate HTTP delivery, response loss, duplicate provider callbacks, out-of-order events and restart at every persisted boundary.
- Worker dies during execution, provider rate limits, quota reset, empty/unknown status, busy child work and explicit pause.
- Malformed intake, ambiguous dedup, changed criteria during execution/verification, missing artifact, failing tests, remote main advancing and genuine merge conflict.
- Dirty/untracked checkout, failed stop, failed cleanup, new task during retirement and retained history after expiration.
- Attempt/budget exhaustion, missing usage telemetry, unchanged idle ticks and repeated unchanged failure: assert both outcomes and model-call counts.
- Project scope/authentication, archived/isolation boundaries, revoked membership, cross-project writes and forged worker identity.
- Desktop/mobile project board, terminal navigation, composer drafts/uploads, restart recovery and visible true current work.

Record baseline versus refactor on identical fixture workloads: model calls by phase, measured tokens/cost coverage, verified requested outcomes, retries, duplicate tasks, terminal latency, active workers and retained worktrees. Fixture call-count savings are not a claim about live provider billing. Keep at least one failing negative control for each new authority boundary.

## Scope boundaries

Refactor command/board/execution orchestration and its primary UI. Keep the Rust server, SQLite, provider adapters, identity/scope model, durable uploads, terminal transport and integration machinery. Browser management, connectors, email/calendar/files and cloud infrastructure stay attached through existing APIs; rewriting them would not simplify task execution.

Full production rollout and migration of the user's active boards happens after branch validation and an explicit deployment decision. This branch request supersedes the earlier instruction to put prior fixes directly on main.

## Progress

- [x] Branch created from fresh remote main; previous retirement fix retained.
- [x] Current paths and constraints inspected; full staged scope recorded.
- [x] Stage 1: ownership/policy and migration preview. Targeted server `project_` tests: 5 passed; persistent policy CAS, preview conflicts, ownership across executor changes and scoped API covered. Full suite remains the final branch gate.
- [x] Stage 2: durable project intake. Canonical reconciliation, idempotent receipts, bounded interpretation and malformed-input starvation controls exercised by unit and UI tests.
- [x] Stage 3: single execution authority. Atomic claims, shared delivery claims, scoped executors, independent fan-out, pause/resume and restart recovery connected to the existing adapters.
- [x] Stage 4: verification/integration/retirement. Browser fixtures verify real commits on a disposable remote main, bounded failed-check repair, cleanup, and retention of dirty work.
- [x] Stage 5: project UI and usage attribution. Ten isolated browser scenarios pass; mobile bounds and global orchestration membership were inspected and asserted.
- [x] Stage 6: migration, consolidation and branch validation. Explicit migration/rollback pass through the UI; legacy dispatch is excluded at shared boundaries. Full server and core suites, JavaScript regression checks, and strict workspace Clippy pass. See [validation evidence](project-lifecycle-validation.md) for exact counts, build identity and the order of final regression checks.

## Implemented adapter boundaries

The branch reuses the existing provider launcher, durable steering queue, issue table, attempt ledger, token ledger, Git candidate verifier and retirement logic. New execution checkpoints live on the issue, and project settings live on the group. Legacy dispatch reads an explicit legacy-only issue view; protocol planning excludes project-owned issues. Temporary project executors reject legacy automatic prompts at the common enqueue boundary.

The initial coordinator adapter uses Claude's read-only helper mode with tools/hooks disabled and low effort. Executor profiles use the existing Claude/Codex/Gemini/Ollama launch adapters. The UI labels the coordinator limitation explicitly. A provider's reasoning quality or every model's real CLI behavior is not established by fake-provider acceptance tests.

Paused projects retain accepted commands and work. Pause kills the executor process tree; resume replaces the execution delivery generation without purchasing a new attempt. Delivery rechecks current ownership/generation and pause state. Verification pins the reported commit and requires a clean worktree before candidate integration. Requirements, repository identity and current gates cannot silently change underneath active verification.

Custom legacy gates must be translated into explicit project criteria and executable checks before migration; preview refuses an unmapped gate. Apply and rollback are revision checked. Rollback only restores untouched migrated rows and cancels their pending intake. New or changed work is never overwritten.

Dollar values are estimates, with missing telemetry explicitly visible. Stop limits apply before further intake or execution; they are not provider-enforced hard caps on a turn already running. No cost-saving percentage is claimed. The validation harness uses nonbillable provider fixtures and reports exact call counts instead.

The legacy implementation remains for unmigrated workers. Its deletion gate is explicit: all of its owning populations must first migrate with equivalent preserved behavior. Removing those adapters in this branch would break the user's existing fleet, so this refactor removes their authority over project work at shared boundaries rather than deleting needed compatibility code.

## Failure paths found by the isolated browser runs

- An integer command-history timestamp was required by existing worker projections. A fractional project receipt timestamp made those projections fail even though project intake succeeded.
- The UI could say Paused before the process tree had stopped. It now shows Pausing until the executor stop is recorded; resume is held until that stop settles.
- Initial provider launch pinned the worktree directory, but Claude's fresh-start fallback did not. Both use the same fail-closed directory command. A real shell test covers quoted paths and missing directories; fixture providers refuse any directory outside the disposable worktree root.
- Project claims did not publish the existing `task.claimed` marker read by worker details. They now publish that identity in the same transaction as reservation, so project and terminal views agree.
- The timer delivery path did not acquire the queue claim already used by the idle-hook path. Both could read the same row and serialize two copies into the terminal. Both now call one atomic delivery boundary; the real tmux UI test rejects duplicate executor calls, including fast failed-check/repair transitions.
- Exhausted malformed intake and receipts waiting on it could consume the entire recovery batch. The queue now selects only actionable receipts; duplicates inherit their original request's waiting reason. New commands remain eligible without another interpretation of exhausted requests.
- A structured result could survive a restart while its delivery stayed claimed, blocking retirement. A valid report now acknowledges only its exact delivery transactionally. Interrupted deliveries observed back at an idle provider are eligible for bounded recovery; pending or unrelated deliveries are not.
- Startup recovery on a newly migrated database queried a legacy sender column that previously appeared only after the first fleet request. The migration now establishes it before background jobs start.
- Screenshot review found clipped mobile cards and duplicate project executors in the legacy Orchestrations list. Mobile phases now stack within the viewport, and orchestration membership/counts share the project ownership projection. The UI also distinguishes structured outcomes from requests still awaiting intake.
- Partial usage coverage could conceal a later unmeasured attempt or an intake call without cost. Budget checks now retain those gaps instead of using another measured call as coverage for them.

The new provider executions are deterministic fixtures, not additional paid model trials. Earlier failed run artifacts remain separate from the final acceptance evidence. Test-server startup, restart and cleanup are restricted to its own manifest, binary and tmux socket.
