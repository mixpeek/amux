# AR-142: interaction state implementation

Date: 2026-09-12

## Deployment integration

The owner requested deployment after independent review. Initial integration
used a clean detached worktree on upstream `6a284869`, without the
shared checkout's uncommitted work. It remains one feature commit. Integration
preserves upstream's removal of automatic uncertain-send deletion: those requests
remain blocked and reviewable. The service worker includes the state assets, and
APP_VER/CACHE move together to 0.9.922. Receipt/effect reducers and Rust interaction
storage are unchanged from the independently reviewed implementation.

Clean-snapshot checks: 27 state tests passed; SPA lint had 0 errors and 48 existing
warnings; workspace/all-target Clippy passed. The full serial server run returned
2,651 passed, 8 failed and 33 ignored across 61 result groups. Seven failures were
the host-admission assertion and worker-start dependencies returning the host's
memory-pressure 503. One exposed an outdated timestamp declaration test: it
counted exactly two `_at` millisecond columns despite the two new receipt columns.
That test now requires the receipt columns explicitly rather than rejecting any
future increase in the count. The production timestamp invariant already declares
both columns and reports undeclared or incorrectly scaled timestamps.

The full run's 11 interaction API tests passed, as did the board and restart
persistence suites. The contention wrapper observed concurrent builder activity
but confirmed the tested worktree was clean at both ends. This is not a claim
that the full suite was green or that memory-dependent worker starts were tested
successfully. Final targeted reruns, browser checks and live adoption evidence
are recorded on AR-142; a commit alone does not prove deployment.

The browser matrix then passed all 72 receipt, feedback, outbox and blocked-outbox
cases across desktop, mobile and Safari. Desktop/mobile pending-state screenshots
were visually inspected. While these gates ran, upstream advanced to `d7aa170f`.
The final clean rebase preserves its isolated-worker refusal and resource-cleanup
changes. Dashboard files and the receipt/effect implementation are byte-identical
to the browser-tested candidate; the final upstream integration is checked with
workspace/all-target Clippy, interaction API tests and isolated-peer refusal tests.

Companion to [the architecture proposal](ai-native-state-management.md). This
records what the implementation actually covers, not a claim that the entire
migration plan or every lifecycle case has been exhaustively exercised.

## Implemented contract

Commands, interaction receipts, and effects are separate. The Rust store remains
authoritative. TanStack Query core handles browser server-cache reconciliation;
a small local store handles ephemeral UI state; XState is used for uploads only.
No frontend framework migration, Redux, or Zustand is included.

The dashboard's shared fetch boundary creates an inspectable receipt before API
mutations, including non-queueable commands. Receipts drive immediate control
busy state, a persistent recent-actions surface, status labels, indeterminate
progress, upload progress, failure/remedy details, and reload recovery. Direct
user-triggered reads say Loading/Loaded without claiming a domain mutation.
Background reads do not produce a receipt on every poll. Local navigation gets
lightweight control feedback rather than a durable command record.

The existing outbox remains responsible for durable request payloads, ordering,
deduplication and replay. Receipt correlation does not introduce a second outbox.
Locally synthesized 202 responses remain Queued, never Completed. Replay carries
the original interaction header. Correlation IDs are NOT general-purpose
idempotency keys: endpoints still own their existing dedupe semantics.

Server mutation middleware records the method, path, caller, target, outcome and
compact acknowledgement, but not request payloads or returned configuration.
Effects are inserted in the same SQLite transaction as each state event, using
its event ID and revision. Concurrent requests cannot acquire each other's
effects through an approximate revision window. Applied writes without events
increment `unjournaled_writes`; they are not invented into complete effects.

Read surfaces:

- `/api/interactions/recent`
- `/api/interactions/{id}`
- `/api/interactions/{id}/effects`
- `/api/interactions/{id}/why`, including correlated request-log rows
- `/api/debug/interactions`, including kind/phase groups and effect counts
- `/api/state/summary?scope=worker:amux&since_rev=N`
- `window.__amuxState` for browser receipts, effects, queries and explanations

The AG-UI adapter projects compatible events only when their required identities
exist. Rich AMUX commands/effects remain CUSTOM events. This is an adapter, not a
new chat-run authority or a complete AG-UI network transport.

## Review decisions

The `amux` worker reviewed the drafts read-only and approved the transaction-level
causality model, retention pairing, reload uncertainty, and AG-UI boundary. Its
actionable findings were addressed as follows:

1. Control parsing ignores `event.stopPropagation()`/`preventDefault()` member
   calls and identifies the registered command handler after event housekeeping.
2. Scoped async/blocking spawn helpers preserve causality in the known board,
   worker, message, filesystem, memory and scheduler API paths.
3. Cloud isolation is per user: `cloud/docker/docker-compose.template.yml` names
   `amux-user-${USER_ID}` and `amux-data-${USER_ID}`. Locally, the existing member
   authorization layer denies workspace-wide ledger reads to worker/group-scoped
   members. A regression covers all six read routes with a worker-scoped cookie.
   Global-workspace members can read workspace receipts, as intended. If storage
   is ever shared between cloud tenants, this design needs tenant keys and row
   authorization before that change ships.
4. Receipt acceptance remains fail-closed: a storage error returns 503 before the
   command runs. This is an explicit availability tradeoff for the no-unrecorded-
   mutation contract. Queueable browser commands keep their retryable intent.
   A failure-injection test proves that no domain revision changes on this path.
   An alternating in-process benchmark measures the extra SQLite/middleware cost;
   it is not a fleet-load or network-latency benchmark.
5. A bare 2xx without an outcome signal is Unknown on both client and server.
   HTTP success, an entity ID, or a revision alone never manufactures Applied.
6. `scripts/build-state.mjs --check` rebuilds and byte-compares the actual browser
   bundle as well as checking the generated control registry. SPA lint invokes it.
7. Effect `kind` is a stable string such as `task.updated`; the original mutation
   object and available from/to values remain separate fields.

A browser test also exposed case-duplicate Authorization headers introduced by
wrapping requests. `_authHeaders` and correlation stamping now use the platform
Headers API so casing cannot create two bearer values or two interaction IDs on
replay. Non-stubbed mutation and offline replay tests guard both paths, including
reading the replay's receipt back from the real server by its original ID.

## Coverage and boundaries

Continued audit corrected reconciliation starvation: the original poller always
selected the newest eight receipts from a 200-record history window. The poller
now rotates bounded batches across the complete unresolved backlog, includes
server-queued commands, and leaves device-local queues to outbox replay. Each
failed status read emits `interaction_status_poll_failed` with the receipt ID
and pending population, without aborting other reads. Same-phase responses can
refresh effects, while a newer local acknowledgement cannot be overwritten by
an older status request. A 205-receipt unit fixture and a real-browser timer test
cover the selection and integration paths.

The server also now recognizes explicit `queued`, `sending`, `applied`, and
`noop` phases, matching the browser contract. Both sides test the complete phase
list, and HTTP refusal still overrides a contradictory success phase. The
existing server `interaction_outcome` signal continues to announce uncertain or
refused outcomes; a bare 2xx still cannot manufacture success.

The generated registry is a conservative call graph of named SPA functions,
not a fabricated count of executed workflows. It currently considers 1,916
functions and declares 648 command-capable handlers. Runtime coverage separately
reports discovered controls, declarations, observed command controls and receipts.
Deleting required metadata, including the interaction kind itself, makes the
browser contract test fail. Previously undeclared controls that issue mutations
emit `unregistered_command_control` diagnostics and still receive a receipt.

These boundaries remain important:

- Synchronous event dispatch can attribute a command to its originating control.
  A command issued after unrelated asynchronous work still gets global receipt
  feedback, but is not falsely assigned to the last clicked button.
- Detached work outside the migrated spawn paths must explicitly carry the
  interaction scope. Domain changes that bypass the existing writer/journal are
  not magically covered. The explanation endpoint states this measurement scope.
- Long-running phases are supported, but not every autonomous runtime job has
  been migrated to emit explicit progress/completion. Unobserved completion
  becomes Unknown after 120 seconds; the UI does not guess success or percentage.
- Browser upload parent receipts coordinate chunk requests and survive local
  reload. They are not yet a durable server-side parent-run hierarchy.
- Legacy endpoints without explicit acknowledgement signals now visibly report
  Unknown. They need domain-specific acknowledgements, not a return to 2xx=Applied.
- Query migration starts with sessions/board and shared SSE/sync invalidation.
  Existing surface-specific caches and feedback remain during migration.
- Interaction lists/effects are bounded and advertise truncation. Device-local
  outbox counts in the server summary are unmeasured, not zero.
- This is representative desktop/mobile/Safari coverage, not proof that every
  lifecycle case or every statically discovered handler was executed.

## Observability

Client verdicts include unknown/refused/failed, persistence failures, reconciliation
failures and unregistered command controls. Server verdicts include
`interaction_accept_failed`, `interaction_completion_unrecorded`,
`interaction_ack_read_failed`, `interaction_read_failed` and `interaction_outcome`.
Request-log samples carry the interaction ID and command kind; `/why` can join
back to those rows. New read/diagnostic responses use measured/n_considered.

## Verification

Reproduction commands:

```sh
npm run test:state
npm run lint:spa
scripts/safe-cargo.sh test -p amux-server --test interactions_api -- --nocapture
scripts/test-contended.sh -p amux-server --no-fail-fast
scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings
AMUX_E2E_WORKING_TREE=1 npx playwright test -c e2e/playwright.config.ts e2e/interaction-receipts.spec.ts e2e/feedback-smoke.spec.ts e2e/upload-reliability.spec.ts e2e/upload-chip-escape.spec.ts e2e/outbox-connectivity.spec.ts --workers=1
```

Mutation checks remove the queued acknowledgement distinction, ignored-fields
handling, and change state-module bytes without rebuilding. Each must turn the
relevant test/check red; the mutation tool restores only its own exact edit.

Observed results:

- `npm run test:state`: 15 passed, 0 failed, including repeated request-header stamping.
- `npm run lint:spa`: 0 errors, 48 pre-existing warnings; registry and bundle
  freshness checks passed.
- Focused Rust interaction integration suite: 10 passed, 0 failed. Includes
  concurrent effect isolation, rollback, detached work, refusal/500/405 recording,
  fail-closed storage, invalid scope, and no inferred success from bare 2xx.
- Workspace/all-target Clippy with `-D warnings`: passed.
- Corrected route-table suite: 2 passed. Timestamp-unit suite: 2 passed.
  Scoped-member authorization regression: 1 passed. Serial explanation API
  suite: 17 passed. Serial board suite: 85 passed, 4 failed; the remaining
  failures expect non-terminal archival without the archive outcome the existing
  handler requires. They are not suppressed or represented as green.
- In-process SQLite/middleware latency, 50 measured samples per population after
  10 warmups, interleaved: existing request-log baseline p50 0.396 ms / p95 1.004
  ms; receipts plus request log p50 1.581 ms / p95 3.893 ms. These distributions
  support an incremental cost decision, not a production p95 guarantee.
- Broad server run: 2,597 passed, 35 failed, 32 ignored across 61 test binaries
  and doc-test groups. Build contention and source changes during the run were
  explicitly reported by `test-contended.sh`. Two route-table checks and one
  timestamp-unit declaration check exposed omissions in this change; those
  registrations were corrected. Other failures included memory-admission 503s,
  fixture connection-pool timeouts, legacy archive-outcome assertions and a
  quoted-status assertion. This is NOT a claim that the broad suite is green.
- The 81-case browser run passed 80 cases, including all upload/outbox cases on
  desktop, mobile and Safari. The new missing-kind negative control failed only
  on the desktop process built before that fix. HTTP bundle hashes confirmed the
  stale desktop bytes and current mobile/Safari bytes. After adding real-server
  replay and two additional refusal/uncertainty cases, the final receipt and
  feedback run passed all 48 cases against the shipping UI sources. Desktop,
  375px mobile and Safari pending-state screenshots were visually inspected;
  the tests also assert no horizontal overflow.
- Mutation controls: deleting queued handling and ignored-field handling each
  failed the named acknowledgement test. Changing a state-module return value
  without rebuilding failed with `State bundle is stale`; each edit was restored.

Visual evidence: [desktop](state-management-evidence/desktop-pending.png),
[mobile](state-management-evidence/mobile-pending.png),
[Safari](state-management-evidence/safari-pending.png). The test adds a synthetic
Save trigger to exercise pending feedback; the status renderer is the shipped
bundle, not a mockup.

### Continuation verification

- `npm run test:state`: 20 passed, including five new polling regressions.
- `npm run lint:spa`: 0 errors, 48 existing warnings; generated bundle and
  registry freshness passed. Dashboard/service-worker version is 0.9.920.
- The receipt/feedback browser matrix, using the same command above restricted
  to `interaction-receipts.spec.ts` and `feedback-smoke.spec.ts`: 51 passed in
  5.8 minutes across desktop, mobile, and Safari. All three isolated servers
  reported build `8216f8c2d97cc5f2`; this run covers the new frontend polling
  code, before the subsequent Rust phase-parity correction.
- Mutation check: replacing `queue.push(...batch)` with `queue.unshift(...batch)`
  via `scripts/mutate.sh run` failed the named rotation test; the tool restored
  the edit, and the rebuilt-bundle freshness check passed afterward.
- The new Rust exhaustive-phase test first failed on `sending` being classified
  as `unknown`, demonstrating that it detects a missing explicit phase.
- After correction, `scripts/safe-cargo.sh test -p amux-server --test
  interactions_api -- --test-threads=1`: 11 passed, 0 failed.
- Unchanged status replies still refresh effects but do not repeat outcome
  diagnostics or rewrite receipt timestamps on every poll.
- Final receipt-only browser rerun after both corrections: 36 passed in 3.8
  minutes across desktop, mobile, and Safari, with all three isolated servers
  reporting build `10b2b882dc3aaa0a`. This supersedes the earlier run for the
  receipt changes; the 15 feedback-smoke cases passed in the earlier matrix.
- Final `scripts/safe-cargo.sh check --workspace` and
  `scripts/safe-cargo.sh clippy --workspace --all-targets -- -D warnings`: passed.

## Independent review corrections (AF-727)

The independent review of `b9a69b20` requested three corrections. These are
implemented in the same amended implementation commit, not a separate change:

1. Effects reads now have a five-second abort deadline covering fetch and JSON
   body consumption. The poller also bounds each injected read/reconcile
   operation at six seconds, so even a callback that ignores cancellation cannot
   retain the global flight flag indefinitely. Failure diagnostics identify the
   receipt; subsequent receipts continue through the bounded batch.
2. `effect_sync` is persisted separately from command phase. Applied/noop and
   other settled receipts remain eligible for effects reconciliation. Failed
   probes retry with backoff from five seconds up to five minutes; successful
   probes become eligible again after sixty seconds to pick up late scoped
   writes. Immediate and polling reads deduplicate in-flight work. Reload makes
   interrupted effect reads eligible again. Recovery never resends a command.
   This automatic recovery is bounded to the browser's retained receipt history
   and eight operations per polling batch; it is not permanent background
   tracking of every historical command. Authoritative reads remain available
   directly by interaction ID. The UI distinguishes unavailable/retrying effects
   from confirmed command completion and labels cached counts as last checked;
   capped server responses remain explicitly partial.
3. Each new acknowledgement replaces measurement state and its explanation.
   Successful replay or recovery clears obsolete reload/timeout warnings, while
   unknown or explicitly unmeasured acknowledgements keep a current explanation.

New signals are `interaction_reconcile_failed`, `interaction_effects_recovered`,
and `interaction_measurement_recovered`; the status poller's timeout/error signal
remains `interaction_status_poll_failed`. Effects failure does not falsify an
already confirmed command outcome or reset its progress timestamp.

All three review reproductions were added as failing tests before the changes.
The expanded state suite passes 27 tests, including stalled response bodies,
failure/reload recovery, later detached effects, capped backoff, truthful
unmeasured effects, and measurement-recovery diagnostics. Bundle freshness and
SPA lint pass (0 errors, 48 existing warnings). The registry now declares 648
handlers: the internal acknowledgement helper no longer directly emits a
diagnostic POST, so it is no longer miscounted as a command-producing handler.
Dashboard/service-worker version is 0.9.921. This records author verification,
not independent approval of these corrections.

The final receipt/feedback browser matrix passed all 60 cases in 5.8 minutes
across desktop, mobile, and Safari, against isolated build `6483b8d12b6278b5`.
This includes a never-ending effects response body with a verified failure
beacon and a healthy second receipt, failed effects recovery after reload with
zero command POSTs, and real-server replay clearing the old measurement warning.
The retry state was visually checked with no horizontal overflow:
[desktop](state-management-evidence/desktop-effects-retry.png),
[mobile](state-management-evidence/mobile-effects-retry.png),
[Safari](state-management-evidence/safari-effects-retry.png).
These review corrections change only the browser implementation and tests;
the earlier Rust implementation, its focused test results, and the documented
broad-suite limitations are unchanged.

## Rollback

All new implementation changes belong in one commit. Revert that commit to
restore the previous UI/server behavior. Migration 0065 is additive: the two
interaction tables and indexes may remain unused after a code revert; do not
delete receipt history or reset the domain database as part of rollback.
Browser v2 receipt storage is separate from the previous ledger key and outbox.
The existing earlier receipt/outbox commits are not rewritten or squashed.
