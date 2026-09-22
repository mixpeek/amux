# Fan-out lifecycle

Fan-out execution uses the existing board, worker configuration and git worktree lifecycle. Orchestrations is a compact read-only projection (`GET /api/board/orchestrations`), not another task store. Its response declares measurement and the number of rows considered, includes full child boards and standalone ephemeral workers, and omits long prompt/history fields. Worker lifecycle controls pause classification; the shared type-specific terminal predicate controls progress. Code Done is not Verified. The compact response includes configuration-derived worker metadata; the page renders it immediately and reuses the normal single-flight status refresh without waiting for cold runtime probes.

## Global Orchestrations view

The global tab groups actual fan-out workers under their recorded coordinator
(`CC_PARENT`), with one worker row per group. Ordinary board epics are not
orchestrations. Dedicated coordinators also appear before their first child is
provisioned; unassigned and retired fan-outs remain visible. Each row shows its
role/model, lifecycle, active task, board progress and worktree/integration state.
Expand the worker's board to inspect tasks without duplicating the worker for
every task. Nested child epics remain within that board.

The compact API selects the complete boards of fan-out workers and dedicated
coordinators, plus linked ancestors for context. It excludes unrelated ordinary
parent backlog and epics. `n_considered` and `n_excluded_unrelated` expose the
population behind the projection; `orchestration_projection` emits the same
counts at debug level. Configured parent/model/lifecycle metadata is available
before runtime status finishes loading. Current configuration takes precedence
over an older retained retirement record, and scoped views hide foreign parents.

## Ownership and execution

The Board's Launch priorities form creates one real orchestrator worker and one
fan-out worker for each priority. Choose a workspace source, then configure the
orchestrator provider/model separately from the fan-out defaults. Individual
priorities may override the fan-out provider/model. The orchestrator owns the
epic, answers child questions and checks the combined outcome. Each child owns
its implementation board and worktree. Questions use the existing message path
(`amux send <orchestrator> --no-board --stdin`); they do not create tasks on a
peer's board or become cross-worker dependency gates. Existing completion
callbacks route outcomes to the orchestrator. No model polling is added.

`POST /api/board/launch` accepts the following role settings:

```json
{
  "launch_id": "a-unique-request-id-retained-on-retry",
  "parent_session": "workspace-source-worker",
  "orchestrator": {"provider": "claude", "model": "opus"},
  "provider": "claude",
  "model": "haiku",
  "priorities": [
    "Repair the parser",
    {"text": "Verify rendering", "profile": {"provider": "gemini", "model": "gemini-2.5-flash"}}
  ]
}
```

Model IDs remain open strings within validated identifier syntax; discovery is
guidance, not an availability guarantee. Without `orchestrator`, existing API
callers retain `parent_session` as their coordinator. With it, that worker only
supplies the workspace configuration; its own model and lifecycle are unchanged.
The new coordinator is an ordinary worker marked `CC_ORCHESTRATOR=1`. Children
retain `CC_PARENT` and inherit the coordinator's groups for guidance delivery.
Orchestrations shows each role's configured provider/model and includes both the
coordinator's and the children's full boards in progress.
Epic completion also checks unlinked execution tasks on those owned boards,
including retired child configurations. Required linked outcomes must succeed;
discarding an unrelated follow-up does not manufacture success for a required
outcome. Ordinary existing parent workers keep their unrelated backlog outside
the orchestration's completion scope.

The launcher retains its exact pending request in local storage across reloads.
Retries reuse the coordinator, epic and child assignments, including a graph
that completed before its response arrived. A new intentional launch gets a
fresh `launch_id`. Existing worker configurations, paused/archive/isolation
states and board revisions are preserved on retries. Provision/start failures
are reported per role; durable graph creation alone does not claim workers
started. Logs expose `orchestrator_provisioned`, `orchestrator_start_result`,
`launch_reused` and the existing per-child provisioning outcomes.

The canonical provisioner creates independent workers with board delegation disabled and backlog draining enabled. The worker owns all follow-up tasks on its board, not just the initial assignment. Normal prerequisites are implemented there; peer artifacts may be referenced without adding a cross-worker scheduling dependency. Real authorization and column gates remain enforced. Existing explicitly paused/archived/isolated workers stay excluded.

A fan-out starts only in its own durable `amux/fanout/<worker>` branch and `~/.amux/worktrees/<worker>` directory. A failed checkout refuses launch. Restart reuses that workspace; stop and pause never dispose files, index or commits. Explicit deletion remains a separate action.

## Automatic integration

The worker configures its repository check through `PATCH /api/sessions/<worker>/config` with `worktree_verify` (a shell command). New children inherit their parent's configured command. No model is called to discover commands or poll for progress.

At a confirmed turn boundary, integration becomes eligible when there is no active implementation and the completed candidates in Review/Done/Verified have evidence and resolved prerequisite edges. This allows completed prerequisites to be integrated and verified before their same-board successors run. Automatic decommissioning requires every non-archived card to be Verified, including epics and non-code tasks. Done alone does not authorize disposal. The harness then:

1. Captures the board revisions and clean immutable branch head.
2. Fetches main and creates a separate temporary merge candidate.
3. Runs the configured checks on the combined candidate, with git hooks enabled.
4. Rechecks lifecycle, board revisions, candidate and worker checkout.
5. Uses a normal, non-force push to remote main and verifies ancestry by fetch.

One candidate runs at a time. A remote race refuses the push and retains all work. Pause or changed board admission cancels validation and its subprocess group; validation output is bounded. Conflicts and failed checks return a deduplicated instruction to that same worker. Successful integration does not manufacture board evidence, acknowledge acceptance criteria, or bypass Verified gates.
## Automatic decommissioning

After every non-archived card is Verified, the completion sweep checks for an
idle provider boundary with no live child/background work or queued input. It
requires an integration receipt for the exact clean worker head and fetches
remote main again to prove that head is still included. Paused, archived and
isolated workers remain excluded. Empty boards and Done-only boards do not
qualify, and no model calls are used to decide retirement.

The sweep stops the provider through the normal lifecycle path, rechecks the
board/configuration and checkout, and removes only this worker's worktree using
non-force Git removal. Both the directory and registration must be gone before
the worker configuration becomes `.env.reaped` (Expired in Orchestrations).
Board records, output references, integration evidence, conversation metadata
and the branch ref remain available. Existing retired workers with leftover
worktrees receive the same checks before cleanup.

New tasks, queued input, drafts, unmerged commits, inaccessible main or failed
cleanup defer retirement. A board change or failed finalization during cleanup
restores the clean worktree instead of expiring a worker that received new work.
Git/network operations stay outside the SQLite writer; the final board check,
audit event and configuration rename are serialized against board/input writes.
Logs expose `fanout_decommissioned`, `fanout_retirement_deferred`,
`fanout_retirement_changed` and `fanout_retirement_scan_failed`. The board-drive
report counts only completed disposal as `reaped_done`.

Stop and pause continue to preserve worktrees. Automatic completion cleanup is
conditional on these proofs; it is not the forceful manual-delete path.

## Existing workspaces

At a provider boundary, healthy legacy worktrees can acquire their own branch without changing files. Their original creation base was never recorded. The worker must inspect its unmerged commits and explicitly supply a reviewed exact `worktree_base` through the same configuration API. The base must be an ancestor of both HEAD and origin/main. The harness never guesses that unrelated old history belongs in main.

An empty index over a populated commit is reported as an interrupted checkout and preserved. Missing workspaces require preservation of the running worker's changes before restart. Recovery stays with that worker and does not create a peer-board dependency or an ordinary Needs You item.

## Verification

`fanout_workspace::tests` exercises two independent workspaces, restarts with dirty files, main advancing concurrently, merge conflicts, failing checks, cancellation of validation descendants, incomplete checkout preservation, and whole-board admission. `api::orchestrations::tests` checks full child-board projection, scoped visibility, omitted history, and type-specific terminal states. `e2e/orchestrations.spec.ts` checks live endpoint rendering and deterministic failure/retry, orphan, pause, active-card and mobile layout cases.

`tests/fan_out_e2e.rs` exercises role creation, independent provider/model routing,
per-child overrides, concurrent retries, completed receipt retries, paused state,
validation before mutation and full coordinator-board projection. It uses an
isolated test home that refuses actual provider starts. `e2e/orchestration-roles.spec.ts`
covers the form, partial launch/reload/retry and role rendering on desktop/phone
with explicit API fixtures. These checks do not claim a paid live-model run.

`fanout_retirement::tests` uses real local Git remotes/worktrees and a provider lifecycle seam to exercise disposal, idle stop, complete-board gates, stale main/receipts, protected and busy lanes, queued input, drafts, stop failures, finalization rollback and restart recovery. No paid models are started.
