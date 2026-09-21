# Required project outputs and continuation (AAB-3)

## Current acceptance status — 2026-09-21

The real extractor case is complete on Amux runtime `182be3316eb4`: PAA-1 through
PAA-4 Verified, disposable main `dc483a023e74d9a35d7d99dd3607cf94c7c1f99b`, three
executor worktrees retired and 26 retained outputs. Source and merged candidates
each passed 11 fresh browser checks. After the retained 600-second failure,
the parent's actual UI verification-only retry used the same attempt 3,
generation 3 and `f303...` report with an 1800-second command bound and no new
model grant. Initial direct Codex prototype/service testing is separate from
Amux Astra adoption/repair/verification/integration under parent supervision.
See [current validation, exact audit paths and limits](project-lifecycle-validation.md).

The implementation notes below preserve historical failures and their original
handoffs. Their pending statements describe earlier checkpoints. The 182 full
UI passed 13 scenarios; the 424 full UI later failed after ten because of a
shared launcher defect, and fb31 failed after two due to slow setup. Final ab2
passed all 13 isolated UI scenarios, focused gates, both entry/late mutations,
restoration, workspace check and strict Clippy. Its private post-deploy audit
passed 48/48 and the UI retained four Verified cards/report preview. Original
real source/merged acceptance remains the 182 execution, not an ab2 rerun. No
blanket full-suite pass or production deployment is implied.

The private live acceptance exposed a protocol gap: PAA-3's normal browser
verification failed because its candidate lacked PAA-2's backend output.
The second execution attempt explicitly reported an operational wait. That
free-text wait was durable but could never wake when the output arrived.
PAA-3's Studio source was reported as
`8534d7faf8bc6d3a995b1da99b848f8445fb7f27`; the failed gate and original wait
remain evidence, not a browser pass. This repair does not mutate that project,
its worktrees, or its executor state.

## Explicit declaration

A current executor can POST to
`/api/projects/{project}/tasks/{id}/required-outputs`, using its normal
`X-Amux-Session` identity and this JSON:

```json
{
  "generation": 2,
  "input_hash": "the current execution input_hash",
  "idempotency_key": "one stable key for this exact declaration",
  "required_outputs": ["PAA-2"],
  "reason": "The candidate-relative browser gate needs the accepted backend source",
  "replaces_wait": "the exact current execution.waiting string"
}
```

For an active execution with no wait, `replaces_wait` is null. For an existing
operational wait it must exactly acknowledge the old string; that string is
retained in `execution.output_wait.previous_wait` and the transition history.
No background migration parses task IDs from prose or clears old waits.
Legacy operational waits are recognized only by the previous endpoint's exact
machine-written `operational: ` category prefix. The prose after that prefix
is opaque. Newly posted waits retain their category separately.
Spend/customer waits cannot be converted, and tasks or dependencies carrying
spend/budget/customer authorization holds are refused.

The declaration adds IDs to the existing `depends_on` graph, never to another
worker's board. References must exist in the same active project, with no self
reference, duplicate reference, cycle or authorization hold. The body is strict;
there is no alternative category or untyped condition escape. At least one new
edge is required, preventing repeated declarations of the same satisfied output
from generating endless continuations. Executor, generation and requirements
are checked in the serialized writer. Exact key/body replay is a no-op; changed
payloads, foreign executors and stale new declarations are refused. The added
edges update the requirements hash; old reports no longer match.

## Deterministic continuation

Only a structured output wait can select `resume_outputs`. Every referenced
output must be Verified, not merely reported, Review or Done. The shared planner
rechecks project pause/enabled state, requirements hash, budget and available
executor capacity; the writer re-evaluates that same plan atomically before
reserving. The driver also waits for the existing executor's safe boundary.

Continuation increments the delivery generation and creates a fresh steering
identity, but does not increment/reset the execution attempt counter or create
another repair attempt. Ended attempt rows and their timestamps/outcomes remain
unchanged. `project.outputs_continued` records the new reservation, and the
existing `task.claimed` projection carries `continuation: true` for active-card
status without inventing a new `project.claimed` attempt. Ledger attribution uses
explicit continuation intervals derived from durable execution events, ending
at a stopped/suspended or changed-generation event and capped by the existing
maximum claim window. Project tasks do not use the unbounded-until-next-claim
historical fallback. The regression attributes a turn inside the continuation
and leaves turns before/after it unassigned; original attempt rows stay intact.
The original browser failure remains `last_failure`. The output handoff retains
the previous wait and snapshots of the Verified producers' reports and
integration evidence. Repeating the resume after reservation is a no-op.

The packet includes explicit required outputs and their accepted reports.
The executor must fetch the accepted local origin/main and compose the required
commits into its own candidate without resetting its work. It then reports the
current HEAD and one executable check per criterion as before. Dependency
availability is rechecked before delivery, during verification permits and in
the final Verified write. Independent candidate checks, project gate and normal
main integration remain mandatory. Output arrival is not task verification.
An arbitrary operational wait remains waiting indefinitely until a deliberate
supported action changes it.

Logs expose `project.outputs_declared`, `project.outputs_continued` and
`project.outputs_refused`, with measured/population fields. Session events retain
execution snapshots, so the original failed report, wait and continuation can
be inspected without parsing prose.

## Historical private follow-up and proof

`logs/aab3-required-output-request.json` in the isolated test home is an
unsubmitted example made from the then-observed PAA-3 generation/hash/wait. The
original follow-up required the **actual waiting PAA-3 executor** to re-read
current project state and POST using its own identity after parent deployment.
That example is historical, not a request to mutate the now-Verified task.
If the snapshot changed, form a new explicit declaration from current state;
do not force old generation/hash values or reset attempts. Reuse the same key
for retransmission of the same body. This worker performs no live project write,
restart, merge, paid call or worker launch.

Focused regression commands are in `logs/aab3-verification-handoff.json`.
They cover the original exhausted-attempt shape, no wake from an unstructured
wait, reported-vs-Verified output, idempotence, retained attempt/failure evidence,
new delivery identity, stale report refusal, output regression before delivery,
foreign/missing/self/cyclic dependencies, authorization holds, and pause,
capacity, budget and requirement changes. These are fixture proofs, not live
extractor acceptance or upstream-main merge evidence.

## Explicit retained report assets

New completed project leaf reports must include at least one explicit
human-reviewable asset: `assets: [{"path":"reports/result.md",
"sha256":"64 lowercase hex characters"}]`. Historical completed reports without
assets remain readable as history, but new report admission refuses an empty
asset list before mutating task state. Paths are explicit, candidate-relative
normal components. Traversal, absolute paths, symlink escapes and all formats
except `.md`, `.json`, `.png`, `.webm` are refused. Markdown/JSON bytes must
match the exact reported commit; JSON must parse. Captured media may be ignored
files inside the candidate and must carry the expected hash and format signature.
There are limits of 16 assets, 64 MiB per asset and 256 MiB total.

Before main integration/Verified, the driver retains copies in the existing
private artifact area under content-addressed names. It rechecks hashes and
registers each in `_amux_task_artifacts`, with original relative path, candidate
HEAD and SHA256. Execution projects those explicit retained references into
Projects. Retirement checks the retained bytes again and logs
`project.asset_retention_failed` while refusing disposal if they are missing or
changed. `project.assets_retained` records successful retention. Failed candidate
verification never publishes links as Verified output; unused retained copies
may remain after a concurrent verification refusal.

Projects links only explicit assets, using the existing file preview. Reports
open as read-only raw text, so embedded HTML and Markdown file links cannot gain
execution/navigation authority. PNG/WebM reuse the image/video viewers. The
read-only path disables editing, folder navigation, mode switching and offline
fallback. Missing files remain visibly unavailable. No `.mdai`, arbitrary-path
allowlist widening or parsing of prose paths is introduced. Current legacy
reports must be explicitly re-reported by their executor to add assets; this
change does not guess or migrate paths from their summaries.
`project.report_contract_refused` logs missing or malformed report contracts
with the task, worker and generation so a sweep can find attempts that tried to
complete without reviewable evidence.

## Bounded operator task retry

POST `/api/projects/{project}/tasks/{task}/retry`, from operator scope, with:

```json
{"idempotency_key":"one explicit click", "expect_generation":2,
 "expect_revision":7, "input_hash":"current execution input_hash"}
```

Read the generation/hash from `execution_plan.execution` and revision from the
current card. A grant authorizes exactly `attempt + 1`; it never resets counters.
The previous failure/report and grant preconditions remain in `retry_grants` and
execution events. Exact replay is a no-op; key reuse for another body, stale
requirements/revision/generation, foreign tasks, working/reported/Verified tasks,
paused/disabled projects and authorization/output holds are refused. Budget
limits still apply. The UI retains an uncertain request's key until a conclusive
response, and `{}` from an older client fails closed. A subsequent failure stops
at that extra attempt rather than entering another automatic repair loop.

Parent follow-up: after the backend worker repairs its own repository/hook,
use **Authorize one retry** once on its current waiting task. Stop if it fails.
No live retry was granted by this bootstrap worker.

## One project execution authority

`CC_PROJECT` disables legacy command lifecycle at the shared routing boundary
for existing and future project workers. Worker-detail sends enqueue durable
within-task steering and attach it to the current task, with `type=steering`.
They do not invoke intake or create a task/helper. Genuine outcome receipts are
`type=user`, `session=project:<name>`; receipts, recovery, retry and usage queries
require that identity. Repeated steering therefore leaves original outcome and
interpretation counts unchanged. New outcomes go through Projects intake.
Normal nonproject routing remains unchanged. Logs distinguish
`project.within_task_steering` and `project.legacy_intake_held`.

The original live steering is still queued; no delivery is asserted here.
A redundant, unclaimed legacy receipt can be settled by an operator using:

```text
POST /api/projects/{project}/legacy-receipts/{numeric-message-id}/cancel
```

```json
{"idempotency_key":"one cancellation", "expect_attempts":0,
 "reason":"Duplicate of the original queued repair instruction",
 "superseded_by_steering":"the exact original steering queue/history ID"}
```

Use current attempts from the retained receipt. The supplied original delivery
must exist for the same current project executor; foreign scope, a claimed
receipt, mismatched attempts or a live retry lease is refused. Cancellation
preserves the previous result and the original queue/history row; exact replay
is a no-op. Parent may use this for redundant MSG-15 only after identifying the
original receipt. This worker has not cancelled or delivered it.

## Usage ownership and verification efficiency

Codex rollout ownership prefers validated workspace records over shared parent
`CC_DIR`. The expected worktree location, branch, base and matching active or
`.env.reaped` repository identity must agree. Retired worktrees need not still
exist. Exact unambiguous matches repair previously unowned ledger rows even
when the file cursor is complete; existing ownership/task labels are untouched.
Claim windows still decide task attribution, and the repair logs
`codex_workspace_usage_recovered`. Shared/ambiguous directories remain unowned.

Byte-identical check commands execute once per immutable candidate verification
phase, including the project gate. Every criterion-to-command mapping remains
in the report; any distinct command still fails independently. The integrated
candidate is a separate phase and is checked again. The diagnostic verdict is
`project.verification_commands`, with total mappings and distinct command count.

## Waiting display and resumed Codex boundaries

The planner now projects blocked execution into the Waiting phase, consumed by
both Projects and orchestration. A stored Doing status no longer puts an idle
`execution.stage=waiting` task in Working. The existing waiting reason remains
visible, and reservations only become working after the planner authorizes them.

The resumed Codex footer has a session label after its middle path segment.
The adapter had required the last middle-dot segment to be a path, unlike the
composer parser fixed in AAB-2. It therefore returned an unrecognized generation
boundary for the same frame that displayed idle. The adapter now reads the
structural identity/location segments with an optional trailing session label.
Interrupted-idle, spinner, typed draft and other-provider controls exercise the
actual shared boundary function. Busy/draft frames do not permit delivery.
Existing `idle_boundary_measured_without_current_hook` diagnostics announce a
recognized fallback; the parent bootstrap Send now remains an explicit manual
intervention, not proof of automatic delivery. The zero token-coverage launcher
configuration reported by the parent is separate and is not changed here.

## Live accounting and boot-delivery follow-up to 9d0681b2

The private acceptance observed 77 execution turns / 6,536,939 tokens while
unpriced Astra usage incorrectly displayed an estimated $0. Project totals now
use the ledger's existing price table and pricing predicates. Missing execution
rates or incomplete intake cost withhold the aggregate estimate (`null`), with
`cost_measured`, `cost_reason`, `execution_cost_turns_measured` and
`unpriced_models` alongside observed tokens. Known free local rates remain a
measured zero. Configured dollar budgets hold execution and queued project
steering when cost coverage is unknown; no Astra rate was guessed or added.
Estimates use current configured rates without rewriting historical ledger rows.

Rows already attributed to a project task remain authoritative. Additional rows
with an empty task and the identity of a validated dedicated executor workspace
count toward project totals, including after `.env.reaped`. The workspace record,
repository provenance, project environment and durable execution identity must
agree, without another project's claim to that executor. These rows appear as
`executor_unattributed_turns_measured` and `executor_unattributed_tokens`, while
`execution_attempt_turns_measured` identifies task-attributed coverage. Neither
ledger task labels nor attempt windows are changed. Foreign/unowned rows remain
excluded. Steering still creates no original outcome receipt.

The existing steering claim and typing boundary share the same current project
identity/pause/budget predicate. Legacy blocked-Doing normalization shares one
read/write predicate that excludes project rows before any attempted mutation.
Codex ownership repair reads the existing session index once for at most 1,000
eligible row IDs, skips the writer entirely when none need repair, and performs
conditional updates by primary key. Later exact ownership can still recover old
rows; conflicting conversation owners are refused.

The observed bootstrap failure returned `confirmed` for a deferred boot send,
then left a pasted-content draft requiring one recorded manual Enter. Boot/start
prompts now enter the existing durable steering queue instead of a fire-and-forget
poll/paste timer. Deferred responses are not confirmation. One claim owns delivery;
a refusal releases that same row, retaining exact bytes. Start records its prior
frame fingerprint; an unchanged pre-start frame, unknown UI, busy frame or draft
cannot establish fresh readiness. Boot deliveries never become forced mid-turn
sends merely by aging, and do not trust a stale idle hook. Existing drafts are
held, never cleared/re-pasted by the queue. Start failure retains the queued
message for explicit recovery. Existing project task steering remains on its
project route. No new worker, provider call, live restart or project write was
performed for this follow-up.

Log signals include `project.cost_unmeasured`, `boot_delivery_queued`,
`boot_delivery_enqueue_failed`, `boot_delivery_start_failed`,
`steering_draft_preserved`, and existing project delivery/ownership verdicts.

Parent's bounded native verification plan (after normal checks and private-only
build): use the existing private bootstrap when quiescent, not a running project
executor. Bracket observations with `/health` build/commit. For the next already
authorized instruction, save exact request bytes/hash and one stable client ID;
resume then send during startup. Expect deferred/no submitted=true until one
queue claimant actually confirms submission. Inspect native UI plus queue/history
and provider user-event bytes, not only `/api/sessions` or DOM state. Replay the
same request ID: one queue/history delivery, no duplicated/truncated text. In a
separate no-send observation, leave an existing draft (including a paste chip):
queue must remain held and draft bytes unchanged. Observe a busy/background frame:
no boot forced paste. A failed startup must leave the same message pending for
explicit recovery, not an orphaned timer. Stop at any unexpected draft, duplication
or uncertain submission; do not clear it or press Enter automatically. Record any
manual intervention separately. No extra paid call or new worker is authorized by
this plan; use only an otherwise-authorized next instruction.

The final review correction also exposes cost coverage in Projects: unknown cost
includes its reason; a fully priced free model displays a measured zero estimate.
Executor turns outside attempt windows have a separate turn/token label. The
existing Chromium intake/retry fixture covers both labels without provider calls.
Usage selects disjoint task-owned and unassigned session-owned rows through the
existing task/session indexes. Canonical workspace aliases are rejected as
ambiguous ownership, including retained executor identities. A synthetic 842,035-row
SQLite check verified indexed SEARCH plans and the expected two included rows;
its single warm-query timing is not an end-to-end latency claim.

Boot readiness accepts a changed idle frame or a positively observed childless to
live-child provider launch, then still requires an empty composer and no generation.
Thus an identical transcript after a new launch can drain, while the old process
cannot qualify merely by age. The /start route sets a scheduled-start hold before
enqueuing; the operation lock protects the launch after that hold is consumed.
Tests cover same-text fresh versus stale frames, scheduled-start holds, drafts and
busy states. Parent native acceptance must additionally observe the actual pane,
durable row and receipt across restart; parser tests are not native delivery proof.

Final follow-up source and pending parent commands are recorded in
`/private/tmp/amux-astra-20260920/logs/aab3-live-followup-handoff.json` and its
source manifest. Rust runtime tests and Chromium/native acceptance for these exact
bytes remain pending. No production/main integration or live project mutation is
claimed by this follow-up.

The broad transport follow-up classifies intentional boot, draft and project
budget/pause holds as HTTP 409 with a concrete recovery hint; it does not exempt
them as hard failures. The durable message remains queued. Fully ANSI-free Codex
captures recognize the exact placeholder followed by a final provider footer;
partially styled captures still require the dim placeholder. This is a rendered
layout contract (plain text cannot distinguish a deliberately identical user
string). Actual other drafts, extra lines and pasted text stay typed, and both
foreground and background working frames still fail the shared boundary gate.
The existing finished-background regression is retained with negative controls.
The recognition emits `codex_plain_capture_placeholder` once per process.

### Final shared acceptance correction (after 1b9c71ac)

Failed integration may leave board status Review while execution is waiting.
The shared retry eligibility predicate now admits Doing or Review with a real
waiting reason, retaining pause, budget, requirements and authorization checks.
Projects renders the server's `retry_available`; grant still requires exact
revision/generation/hash and an idempotency key. New claim restores Doing with
monotonic attempt/generation and keeps the prior report/failure in grant history.
The lifecycle regression covers report → failed Review → grant → claim → new
report; terminal/active/stale/budget/pause controls remain. This durable-state
regression does not replace the parent's full integration UI scenario.

The deterministic CLI fixture now disables canonical input and advertises
bracketed paste before idle. It collects a complete submitted packet before
interpreting task lines; raw receipts go to fixture-input.jsonl. Real dispatch,
claims, Git and verification remain unchanged. `python3
e2e/project-lifecycle/tty-input-test.py` received exactly 12,560 bytes through a
real PTY with multiline Unicode; the old canonical contract failed to submit
its oversized-line control within two seconds on macOS. The prior live fixture
truncation evidence is retained by the parent.

Worker action menus project lifecycle pending state as disabled, including the
pause response → slow sessions refresh interval, and refresh after settling.
Terminal overlays clear the existing measured service-worker warning height.
Chromium regressions cover slow refresh and warning visibility/reachability at
390px and 1280px. Parent must run these plus the full 11-scenario lifecycle UI
against the next exact build. The earlier native acknowledgement proved one
intact settled-resume delivery; it did not exercise boot deferral.

Retry packets preview only `previous_result` when it exceeds 2,048 Unicode
characters, reusing the existing head/tail elision helper. The preview includes
1,024 characters from each end, explicit truncation, original character/byte
counts, and GET `/api/projects/{name}` with the matching card's
`execution_plan.execution.last_failure` field. Short errors remain exact strings;
full failures remain unchanged in durable state/read responses. Requirements,
criteria, and output handoffs are not capped. The packet logs
`project.retry_diagnostic_preview` without duplicating the diagnostic body.
The focused test checks Unicode boundaries, short errors, complete read-model
retrieval, preserved criteria and no mutation. Parent Rust execution is pending.

### Durable dependency ownership (AAB-3 live re-verification correction)

The strengthened full lifecycle fixture found an exhausted recheck whose failure
was `cross_board_dependency_forbidden`: a completed project epic still depended
on two Verified tasks assigned to different retired executors. The dependency
validator treated `session` as ownership even though projects retain ownership
in `project_group` and use session only for assignment.

`board_store::BoardOwner` now represents Project or legacy Worker ownership
(including the unassigned legacy board). Dependency and incoming-dependent checks
use that identity in shared storage and board API paths. New/reopened edges must
remain within that owner; missing/deleted targets and project/legacy boundaries
remain refused. Executor reassignment within a project preserves the graph.
Ownership migration and rollback validate connected changes against their final
owners before writing, so an outside incoming dependent cannot be stranded.
Project intake adoption uses the same batch predicate. Existing IDs, assignment,
criteria, evidence and edges are not rewritten by this correction.

Regression coverage uses actual project intake over a real temporary database:
two created outcomes and their epic are marked Verified with retained evidence
and distinct retired executor records, then a distinct recheck reopens the same
canonical task/epic with one interpretation and no retry. DB/API legacy tests
retain worker isolation; negative controls cover foreign projects, legacy scope,
missing/deleted edges and incoming ownership changes, including rollback.
Logs use `project_dependency_owner_validated` for accepted project edges and the
existing `cross_board_dependency_refused` marker for rejected owner crossings.
For runtime `7ee3eb6f313b`, parent project tests and the full isolated UI now pass;
see [revision validation](project-lifecycle-validation.md#aab-3-validated-runtime-revision-7ee3eb6f313b)
for exact commands, evidence and limits. The prior full fixture and live failure
evidence remain untouched.


### AAB-3: legacy sweep cadence and complete command admission

The retained `lifecycle-ui-f1277e53bf18` run passed ten scenarios before the
90-second dirty-checkout observation expired. Read-only fixture evidence shows
both PU-8 packets delivered and both reports received, with a dirty-checkout
rejection between them. This was not lost boot delivery. The same periodic job
awaited legacy provider starts that failed about every 14 seconds before it
could tick projects again. One owned legacy sweep now runs alongside project
ticks at the existing configured cadence, without another detached task or
concurrent legacy sweep. Cancellation drops that sweep; existing project runner
exclusion, claims and pause checks remain authoritative. The signal is
`project_tick_during_legacy_wait`. Nonbillable regressions cover report progress,
pause, failure, cancellation and the former serial-starvation negative control.

The migration-only fixture now disables pickup and standing orders through the
supported worker config API before seeding its card, and checks these settings
survive rollback. It is deliberately NOT paused: migration continues refusing
paused, archived and isolated sources. The dirty scenario must still execute
its provider and retain its dirty checkout; its timeout was not increased.

Backend generation 5 was a **late candidate-command rejection**, not a Git push
failure: criterion 6 named the original checkout's venv after earlier expensive
checks had already run. Report admission now uses the registered worker workspace
and the existing source-path validator after caller/generation/requirements and
criteria checks, before report or status mutation. Repository aliases compare
canonical filesystem identity; the recorded worker branch remains bound.
Invalid commands return a same-generation resubmission instruction and log
`project.report_commands_refused`, alongside `fanout_verification_source_path`.
The verifier preflights the whole distinct command set, including the project
gate, before running any check, covering old persisted reports and changed
configuration. Integration retains its existing validation. No new shell parser
or path exception was added.

API/DB coverage rejects stale/foreign/bad-command reports without mutation or
verification eligibility, then accepts a corrected report in the same attempt
through an aliased repository policy. A real temporary Git candidate regression
puts a sentinel command before an invalid later check (and separately an invalid
project gate): neither may execute. Existing source-path, home-spelling and
symlink controls remain unchanged. Parent checks on runtime `7ee3eb6f313b` passed
50 project tests, the existing source-path guard, workspace check and strict
all-target Clippy. Both mutation controls failed as expected; exact restoration
passed all 50 project tests, and the new-server UI passed all 11 scenarios. See
[revision validation](project-lifecycle-validation.md#aab-3-validated-runtime-revision-7ee3eb6f313b)
for evidence and limits. Original failed runs and live project state remain
untouched; the actual extractor project is not complete or Verified.


### AAB-3: owner input requires an active claim (historical implementation checkpoint)

The parent observed generation-6 owner steering execute while the task was
waiting after failure. The shared steering predicate previously checked project
policy/budget but not execution authorization. Queuing input must not grant an
attempt. New owner notes use the existing steering queue's task precondition
without a revision expiry; changing the assigned task cannot redirect them.
Unbound historical project notes fail closed and stay retained, rather than
silently acquiring a task identity. An explicit cancel/re-submit is needed for
those old unbound notes. Stable-ID replay cannot change a note's task or text.

The queue writer and final typing path now use one hold predicate: current task
identity and requirements, an active working claim, unchanged generation during
awaited pane preparation, existing policy/budget, and no suspension, required
output or authorization hold. Reserved, repair, reported, waiting and terminal
states cannot consume owner input. The finite claim packet must settle before
its queued notes can follow. Queued input survives legitimate holds and becomes
eligible only after a sanctioned retry/resume reaches a working claim. There is
no automatic grant, attempt reset, new scheduler or model poll. Project execution
packets retain their existing delivery-current authority.

Existing queue/session reads expose `blocked_reason`; the Steering panel shows
it beside retained owner input. `message.held` records a stable message/reason
idempotency key; `project_steering_held` logs measured refusal only on its first
recording. Current claim state is rechecked after queue claim and awaited pane
preparation; a revoked claim returns to the queue. Nonproject delivery retains
its existing policy. Owner notes are human queue entries, not system prompts.

Projects now uses a harness-owned `waiting_label` instead of splitting arbitrary
stdout at a colon. A retained report with failed verification displays
**Verification failed**, while the exact multi-line diagnostic remains in
Waiting details. The exact harness tokens `token_budget_reached`,
`cost_budget_reached`, `budget_usage_unmeasured`, and `budget_cost_unmeasured`
retain specific budget labels. Unknown output receives a generic hold label;
no prose parser or model classifier is involved. New failed-verification
transitions log `project_verification_failed`; old records are not rewritten.

New regressions exercise actual send/enqueue, delivery claim, final gate,
idempotent hold signals, explicit retry/claim, identity and hold negatives. The
existing full isolated UI adds a retained owner note after the dirty task
exhausts attempts: zero deliveries/work/new cards while held, then one explicit
retry and exactly one note delivery, still bounded at the new attempt limit.
The fake provider exposes an idle composer during that active fixture retry;
all runtime/queue/Git paths remain real. A separate UI case preserves a detailed
failure starting `tree-revert: OK` while rendering Verification failed. The
dirty scenario waits for attached detail text plus the visible stable label,
not visibility inside collapsed details. Parent Rust, mutation and browser
checks are pending for these bytes; previous green revision evidence is not
reused as a pass. No live project state, historical attempts or provider retries
were changed by this implementation.


### AAB-3: current-turn evidence before no-result recovery

The retained `lifecycle-ui-f557-held-note-boundary` failure showed one extra
execution: PU-2's delivery completed at 1789964246.649 and a stale boot-idle
observation triggered repair at 1789964246.706. Its first valid report then lost
the claim. Fixture calls record attempts 1 and 2, not duplicate packet 1.

Observation now reads delivery evidence before a fresh scoped provider probe,
and revalidates receipt, provider report, working claim/input identity, holds
and in-flight delivery in the writer. `project.delivery_started` records the
current packet immediately before submission, after composer preparation. This
excludes boot idle after enqueue but before submission, while accepting a real
stop after submission that precedes the final sent receipt. Older receipts
without this event conservatively use receipt time. No grace period increased.
A missing receipt still permits liveness observation: a stopped executor with
no delivery in flight enters bounded recovery, retaining the unsent queue row
without inventing a sent receipt. The old packet fails the existing claim gate;
only a new bounded claim prepares another execution. Interrupted delivery can
recover at a fresh safe boundary. Concurrent accepted reports win the writer
comparison; active work/drafts do not authorize an idle transition.

Measured signals are `project_stale_idle_held` (deduplicated) and
`project_current_turn_ended_without_result`. Deterministic `project_observation_`
tests inject timing changes at the production observation seam, including
acquisition of an unsent packet during the probe, a concurrent valid report,
fast completion before receipt, and exhaustion after stopped-before-submit.
Parent Rust/mutation/full UI validation is pending for these bytes; see
`/private/tmp/amux-astra-20260920/logs/aab3-stale-idle-handoff.json` and its sibling
manifest. Previous failures and the two pending UI/fake-provider corrections
remain retained. No live state, grants, builds, commits or model calls changed.


### AAB-3: project notes have next-turn delivery only

Project-steering queue rows retain Cancel, their exact text/identity and any
held reason, but do not offer Send now. They show Automatic next turn. Project
`POST /api/sessions/:worker/send` with `deliver_now=true` returns HTTP 409,
`code=project_next_turn_only`, before deduplication, enqueue or command history
writes. The error explains automatic delivery during an authorized working
claim and cancellation. Explicit refusals log `project_send_now_refused` with
measured=true and n_considered=1. Legacy Send now is unchanged. No new endpoint,
queue, scheduler, forced delivery or implicit attempt grant was introduced.

The API regression compares exact records before/after repeated unsupported
requests in both held and working states (including reused message IDs). The
full owner-note UI asserts Send now is absent and Cancel remains usable, then
retains automatic exactly-once delivery following its explicit bounded retry.
Parent observation run `aab3-stale-idle-focused.log` passed two timing tests and
failed only the stopped-case fixture's expectation that a refused claim throws;
that assertion now requires applied=false and unchanged execution, attempt
history, original queue identity and delivery history. Runtime cap unchanged.
The refreshed frozen source and parent commands are in
`/private/tmp/amux-astra-20260920/logs/aab3-observation-and-queue-handoff.json`
and its sibling manifest. Final Rust/browser results remain pending.


### AAB-3: superseded unsent packets no longer prevent retirement

A stopped-before-submission packet remains durable during bounded recovery.
Once a later claim permanently supersedes that delivery, the normal steering
tick settles it into existing history with the exact ID/text and explicit
`void:project-execution-superseded` outcome. One serialized writer proves the
old structured execution identity and current task/project/worker/generation
and input, checks history conflicts and the in-flight marker, then moves the
row atomically. It never treats a temporary hold as supersession. Owner notes,
current packets, in-flight deliveries, foreign/unproven identities and conflicting
receipts remain untouched. The actual retirement predicate is unchanged.
Soft-deleted tasks are excluded by `board_store::get_issue`'s SQL
`deleted IS NULL` filter within that same transaction; its regression retains
the old packet. The compile correction removes an invalid IssueRow.deleted
access, not the soft-delete protection.

Actual settlements emit `message.voided` with delivered=false and measured
`project_execution_packet_superseded`. Cleanup failure logs
`project_packet_reconciliation_failed` with measured=false, retains unsafe
input, and continues unrelated normal queue processing under existing per-row
guards. A trigger-injected writer failure regression verifies a legacy queue
row still reaches its existing void/history path while the project packet is
retained. Shared-helper negatives and the actual retirement predicate cover
supersession, exact history retention and owner-note disposal protection.

Parent reports the prior observation/queue gates passed. The later cleanup
compile failed E0609; its original `aab3-final-runtime-project.log` is retained.
Cleanup tests and both negative controls still require parent validation after
this narrow compile correction. Complete frozen source and commands are in
`/private/tmp/amux-astra-20260920/logs/aab3-final-runtime2-handoff.json` and its
sibling manifest. No live packet, project state, attempt or server was changed
by the worker; parent controls deployment and normal reconciliation.


### AAB-3: bounded command timeout and explicit retained-report verification

The retained PAA-4 generation 3 API snapshot reports waiting at attempt 3 with
`Command timed out after 600 seconds` and report head
`f303abcd9ebda8e994e462111bee542412a3f603`. Parent observed backend and Studio/build
checks before the timeout; fresh browser proof did not run. This is failed
verification, not accepted output. Evidence remains at
`/private/tmp/amux-astra-20260920/logs/paa4-g3-independent-observation.json`.
No retry or live project mutation was performed for this patch.

Project policy now has `verification_timeout_secs`: default 600, integer 1–3600,
configured through existing execution settings. A shared runner preflights all
distinct commands, runs each once per immutable source/merged candidate phase
with that same per-command bound, and retains process-group cancellation and
HEAD/clean-tree checks. Legacy integration retains its 600-second default.
The bound is per command, not a combined shell bundle or an environment override.
`candidate_verification_command` records command, candidate, configured bound,
elapsed time and measured success. Git operation/pre-push limits are unchanged.

The existing operator-only task retry endpoint also accepts:
`{ "action": "verify", "request": { "idempotency_key": "<stable click ID>",
"expect_generation": <current>, "expect_revision": <current task rev>,
"input_hash": "<current>" }, "report": <exact retained report object> }`.
Only a failed waiting review with a valid retained report, unchanged requirements,
verified required outputs and no pause/budget/authorization hold can start it.
Replay of the identical accepted request is a no-op; stale/foreign/changed
requests fail. The complete prior failure stays in `verification_retries` and
`last_failure`. Generation, execution attempt/history, delivery identity and
report are preserved. No model grant, command intake or steering row is created.
Actual HEAD/cleanliness and every gate run normally; checks against changed
policy/candidate identity are refused. A failed explicit verification rerun stays
waiting, even below the model-repair cap, until another explicit operator action.

Projects shows **Rerun checks** (no model execution) separately from the existing
worker repair retry (one additional model attempt). Request identity persists
across uncertain responses in the existing UI retry storage. The measured
`project.verification_retry_granted` event/log distinguishes this action from a
model repair grant; existing retry refusal logs retain admission errors.

The originally snapshot-only tests cover timeout bounds, per-command source/merged consistency,
timeout descendant cleanup, preflight, stale/dirty candidates, real API replay
and scope refusal, no model queue/attempt growth, and failed-rerun no-loop behavior.
The full isolated UI adds a one-second timeout, configuration update, explicit
rerun of the retained report, zero additional provider calls, Verified and normal
retirement. They were pending in `logs/aab3-verification-retry-handoff.json`;
subsequently the 182 focused gates and 13-scenario UI passed, and the real
verification-only retry completed with the original report and attempt intact.
The later 424 and fb31 startup failures, followed by the ab2 isolated UI pass
and read-only post-deploy audit, remain separate in the current validation
evidence linked above. The full server suite was not rerun on ab2.
