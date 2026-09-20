# Required project outputs and continuation (AAB-3)

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

## Private follow-up and proof

`logs/aab3-required-output-request.json` in the isolated test home is an
unsubmitted example made from the observed PAA-3 generation/hash/wait. After
parent review, build and restart, the **actual waiting PAA-3 executor** should
re-read its current project state and POST that structure using its own identity.
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

Structured reports accept optional `assets: [{"path":"reports/result.md",
"sha256":"64 lowercase hex characters"}]`. Paths are explicit, candidate-relative
normal components. Traversal, absolute paths, symlink escapes and all formats
except `.md`, `.json`, `.png`, `.webm` are refused. Markdown/JSON bytes must match
the exact reported commit; JSON must parse. Captured media may be ignored files
inside the candidate and must carry the expected hash and format signature.
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
