# AI-native state management for amux

Date: 2026-09-11

## Summary

amux already has the right raw material for better state management: a Rust API with a monotonic global revision, `/api/sync`, `/api/events`, diagnostic endpoints with `measured`/`n_considered`, a durable offline outbox, client-debug beacons, lifecycle e2e coverage, and a dashboard that already tries to surface refused/queued/error states. The weakness is that those pieces are not one contract. Each surface still hand-rolls loading flags, toasts, optimistic writes, retries, local caches, and render decisions in `crates/amux-dashboard/static/app.js`.

I would make state management a first-class amux concern, but not by introducing a ninth domain primitive. The primary abstraction should be a command and interaction contract over the existing primitives: workers, board, scheduler, filesystem, groups, memories, environment, and messages.

The architecture should read this way:

```text
AMUX domain state
  -> command contract
  -> authoritative mutation or run
  -> interaction receipt
  -> domain effects
  -> query/cache reconciliation
  -> UI feedback
```

TanStack Query, XState, and any local store are implementation machinery. The receipt and effects contract should survive replacing any of them.

The target invariant:

> Every consequential human, agent, or system command produces immediate feedback and a machine-readable interaction receipt that eventually resolves to a known state. Every resulting domain change is linked back to that interaction. If nothing changed, the receipt says that. If the measurement did not run, the receipt says that too.

This makes the UI better for humans and better for agents. A model should be able to ask, "what happened to my click/send/save?" and get a structured answer instead of scraping a toast or guessing from a re-render.

## Current repo findings

Dashboard shape:

- `app.js` is a large classic-script SPA with direct DOM rendering.
- It uses many ad hoc module globals for state: sessions, board, connection state, peek state, notifications, uploads, drafts, offline queue, tab settings, graph positions, etc.
- It already wraps mutation fetches through `apiCall` and a global `window.fetch` interceptor for offline queuing.
- It has visible feedback in several places: `showToast`, connection pill, sync banner, notification banners, per-upload chips, modal confirmations, and read/write notices.
- It has explicit diagnostics for client-only failures through `/api/client-debug`, and several beacons already include `measured` and `n_considered`.

Server shape:

- `amux-core/src/revision.rs` defines `StateRevision`, `StateEvent`, and `MutationResult`.
- `/api/events` sends SSE `hello`, `state`, `invalidate`, `lagged`, and `ping` events.
- `/api/sync?since_rev=N` returns bounded revision deltas and says when a full sync is required.
- `/api/logs/analyze`, `/api/debug/sse`, `/api/health/invariants`, `/api/debug/routes`, and `/api/client-debug` already embody the "observable, not guessed" ethos.
- `api/measured.rs` encodes the diagnostic contract that empty results must say whether the probe ran.

Testing shape:

- `e2e/feedback-smoke.spec.ts` is already the seed of the "no silent action" contract.
- `e2e/lifecycle/cases.json` enumerates the end-to-end product lifecycle.
- `e2e/lifecycle/journey.spec.ts` verifies creation, persistence, search, export, and viewport behavior.
- `e2e/fixtures.ts` wraps `page.route` so a stub that never matched fails instead of silently testing live data.
- `e2e/ux-discovery/crawler.ts` can inventory reachable controls and attach control graphs as evidence.

## Design principle

Separate state into five layers:

1. **Authoritative state**
   The database and revision journal. Source of truth. Already Rust/SQLite.

2. **Command and interaction contract**
   The stable envelope for intent, acknowledgement, progress, refusal, and causality. This is the center of the proposal.

3. **Server-state cache**
   Browser cache for GET data keyed by endpoint/domain, updated by fetch, SSE invalidations, `/api/sync`, and mutation acknowledgements.

4. **Interaction ledger**
   Local and eventually durable records of user/agent/system intent: click, save, send, upload, start, stop, retry, discard, approval, autonomous pickup. This is the missing layer.

5. **Ephemeral UI state**
   Open modal, selected tab, focused input, panel width. Local only, never confused with authority.

The mistake to avoid is making the UI store the source of truth. amux's existing `rev` and `MutationResult` work is exactly the right server-side center of gravity.

## Proposed libraries

The libraries should sit under the interaction contract, not define it.

### Use TanStack Query core for server state

Use `@tanstack/query-core`, not the React adapter. The official docs describe Query as async server-state management with cache lifecycle, request deduplication, retries, background refresh, mutation state, optimistic paths, invalidation, and rollback. TanStack also publishes Vanilla/headless libraries, so this fits the current frameworkless dashboard.

Why it fits amux:

- It handles the boring hard parts currently scattered through `app.js`: request lifecycle, dedupe, cache freshness, retries, mutation status, and targeted invalidation.
- It can live under the current classic SPA before any framework migration.
- It gives a shared vocabulary for "fetching", "stale", "pending mutation", "error", and "settled".
- SSE `invalidate.keys` can map directly to query invalidations.

Do not use it as the authority. It is a cache over server truth.

### Use XState selectively for interaction workflows

Use `xstate` only for multi-step workflows where illegal states are a real bug: upload, browser-control session, auth/setup, deploy/update flows, and possibly message sending when delivery, queueing, waiting, and acknowledgement branch enough to warrant a statechart. XState's actor/state-machine model is specifically good at event-driven logic, statecharts, visualization, inspection, and generated test paths.

Why selective:

- A global app state machine would be too heavy and would become the ceiling.
- Per-workflow machines make "what can happen next?" explicit for humans and models.
- Statecharts are inspectable artifacts: useful for AI-native reasoning and test generation.

Do not automatically start with machines for every save operation. A simple board edit that is only `editing -> saving -> saved/error` can be handled by the interaction ledger. A board edit that grows real branches, such as `dirty -> validating -> saving -> queued_offline -> conflicting -> reconciling -> applied`, deserves a machine.

### Use a tiny vanilla store for ephemeral UI state

Write a tiny local store, roughly `createStore({ getState, setState, subscribe, select })`. Do not add Zustand yet. amux already has TanStack Query for server cache, the interaction manager for commands/mutations, XState for complex workflows, and the server revision journal for authority. Adding a fourth state abstraction before it is needed would make simple UI state look more important than it is.

Use this for:

- active tab
- overlay open/closed state
- selected worker/card
- local preferences mirrored from storage
- transient input state that is not a durable draft

Do not put server data, interaction receipts, command effects, or authoritative entity state here.

### Do not add Redux

Redux would give structure, but it does not solve amux's main problem by itself: async authority, mutation receipts, SSE invalidation, offline replay, and diagnostic evidence. It would add ceremony around the wrong center.

### Do not adopt a frontend framework as phase one

React/Svelte/Solid may eventually be worthwhile, but a framework migration would conflate two projects:

- state semantics
- rendering architecture

The state semantics can be added under the existing static app and tested first. A future framework can subscribe to the same state kernel.

Sources checked:

- TanStack Query docs: https://tanstack.com/query/latest
- TanStack Vanilla libraries: https://tanstack.com/libraries/vanilla
- XState/Stately overview: https://stately.ai/

## The core addition: commands, receipts, and effects

Add a browser-side `interactionManager` and, eventually, a server-side interaction log. Keep three concepts separate:

```text
Command
  -> Interaction receipt
  -> Effects
```

The command is the intent: "approve this task", "send this message", "start this worker".

The interaction receipt answers: "what happened to that request?"

Effects answer: "what changed because of it?"

One command can produce multiple effects. A single approval might move a task, resume a worker, emit a message, finalize an artifact, and unblock a dependent task. The receipt should not become a grab bag for all of that. It should link to effects.

Receipt shape:

```ts
type CommandEnvelope = {
  id: string;
  kind: string;
  target: {
    primitive: 'worker' | 'board' | 'scheduler' | 'filesystem' | 'group' | 'memory' | 'environment' | 'message';
    id?: string;
    label?: string;
  };
  payload?: unknown;
};

type InteractionPhase =
  | 'accepted'
  | 'queued'
  | 'sending'
  | 'running'
  | 'waiting'
  | 'blocked'
  | 'applied'
  | 'noop'
  | 'refused'
  | 'failed'
  | 'reconciled'
  | 'unknown';

type InteractionReceipt = {
  id: string;
  command: CommandEnvelope;
  origin: {
    actor: 'human' | 'agent' | 'system';
    surface: string;
    session?: string;
  };
  phase: InteractionPhase;
  feedback: {
    required: boolean;
    persistence: 'transient' | 'until-settled' | 'durable';
    severity: 'info' | 'success' | 'warning' | 'error';
    message?: string;
  };
  request?: {
    method: string;
    path: string;
    outbox_id?: string;
    msg_id?: string;
  };
  acknowledgement?: {
    status?: number;
    applied?: boolean;
    rev?: number;
    version?: number;
    ignored_fields?: string[];
    error?: string;
  };
  effects: CommandEffect[];
  measured: boolean;
  n_considered: number;
  why_unmeasured?: string;
  created_at: number;
  updated_at: number;
};

type CommandEffect = {
  id: string;
  kind: string;
  entity: {
    primitive: string;
    id: string;
  };
  from?: unknown;
  to?: unknown;
  rev?: number;
};
```

Immediate browser-only implementation:

- Create a receipt before every command button/mutation.
- Render feedback from the receipt, not separately from the call site.
- Attach receipt id to the outgoing request header, for example `X-Amux-Interaction-Id`.
- When the server answers, update the receipt from the response body and status.
- When offline queue accepts it, phase becomes `queued`, not "success".
- When replay confirms it, phase becomes `applied` or `refused`.
- Long-running agent work can move through `running`, `waiting`, and `blocked` before it reaches `applied`, `failed`, or `refused`.
- Post a compact diagnostic to `/api/client-debug` on `failed`, `unknown`, unmeasured, or "no visible feedback" cases.

Server follow-up:

- Add `_amux_interactions` and `_amux_interaction_effects` tables, or extend the request log plus event journal with interaction id, command kind, outcome, rev, target, acknowledgement summary, and effect links.
- Expose `GET /api/interactions/recent`, `GET /api/interactions/{id}`, `GET /api/interactions/{id}/effects`, and `GET /api/interactions/{id}/why`.
- Include interaction ids in `/api/why` timelines and board/card histories where relevant.

The UI can still choose presentation channels. The domain contract should say whether feedback is required, how persistent it must be, and how severe it is. The renderer can decide whether that becomes an inline error, banner, toast, badge, modal, or notification.

Example:

```json
{
  "command": {
    "kind": "task.approve",
    "target": {"primitive": "board", "id": "task_123"}
  },
  "interaction": {
    "id": "int_789",
    "phase": "running"
  },
  "effects": [
    {"kind": "task.state_changed", "entity": {"primitive": "board", "id": "task_123"}, "from": "review", "to": "done"},
    {"kind": "worker.resumed", "entity": {"primitive": "worker", "id": "finance-agent"}}
  ]
}
```

## AI-native behavior

"AI native" should mean structured, inspectable state for models, not just more model calls.

Add one agent-readable endpoint:

```http
GET /api/state/summary?scope=worker:amux&since_rev=123
```

Response shape:

```json
{
  "measured": true,
  "n_considered": 12,
  "rev": 456,
  "health": {
    "connection": "live",
    "outbox_pending": 0,
    "sse_live_connections": 3
  },
  "recent_interactions": [
    {
      "id": "int_...",
      "kind": "board.save",
      "target": {"primitive": "board", "id": "AF-123"},
      "phase": "applied",
      "feedback": {"persistence": "until-settled", "severity": "success", "message": "Saved"}
    }
  ],
  "recent_effects": [
    {
      "interaction_id": "int_...",
      "kind": "task.updated",
      "entity": {"primitive": "board", "id": "AF-123"},
      "rev": 456
    }
  ],
  "next_actions": [
    {
      "target": {"primitive": "board", "id": "AF-123"},
      "reason": "gate refused",
      "fix": "provide evidence command and result line"
    }
  ]
}
```

This endpoint should compute from existing primitives. It should not create a new planning substrate. Models get better when the harness gives them truthful, structured context.

Useful affordances:

- Every visible control has a stable `data-action` and, for mutations, a declared `data-interaction-kind`.
- Every mutation result can be explained by id.
- Every domain change caused by a command links back to the interaction id.
- Every refused action has a machine-readable fix when one exists.
- Every queued action has an idempotency/dedupe key.
- Every "nothing happened" has a receipt saying whether nothing changed, it queued, it was refused, or the measurement failed.

Add these direct reads:

```http
GET /api/interactions/{id}
GET /api/interactions/{id}/effects
GET /api/interactions/{id}/why
```

Together they answer:

- Did it happen?
- Is it still happening?
- What changed?
- Why?
- What blocked it?
- What should happen next?

## AG-UI compatibility

The proposal does not require AG-UI, and AG-UI should not become amux's internal domain model. amux's model is richer than a chat protocol because it has board tasks, gates, workers, artifacts, autonomous pickup, verification, and local operational diagnostics.

Design the event system so amux can emit an AG-UI-compatible projection where the concepts overlap:

```text
AMUX command
  -> InteractionManager
  -> domain mutation or run
  -> event stream
       -> AMUX domain event
       -> AG-UI projection
```

Suggested projection:

```text
AMUX event            AG-UI event

run.started       ->  RUN_STARTED
message.delta     ->  TEXT_MESSAGE_CONTENT
tool.started      ->  TOOL_CALL_START
tool.completed    ->  TOOL_CALL_RESULT
state.changed     ->  STATE_DELTA
artifact.created  ->  CUSTOM
task.blocked      ->  CUSTOM
approval.required ->  CUSTOM
```

This gives compatibility with assistant-ui or other AG-UI clients without designing amux around chat. The internal stream remains amux-native; AG-UI is an interoperability adapter.

## Migration plan

### Phase 0: inventory and contract

Create an interaction registry beside `app.js`:

- `interaction-kinds.json` or `static/state/interactions.js`
- List every mutation/control kind, primitive target, feedback requirement, queue policy, and idempotency key.
- Use the UX crawler to compare interactive controls against this registry.

Definition of done:

- Every mutating button has `data-interaction-kind`.
- Every command-producing control has `data-action`, `data-interaction-kind`, stable target identity, and a declared feedback requirement.
- A test fails if a mutating control is not registered.

### Phase 1: state kernel under the current SPA

Add:

- `static/state/query.js`
- `static/state/interactions.js`
- `static/state/store.js`
- `static/state/sync.js`

Keep loaded from `index.html` before `app.js`, or concatenate/build them into the existing asset pipeline. Do not rewrite rendering yet.

Move only the shared substrate first:

- `fetchSessions`
- `fetchBoard`
- `/api/events` handling
- `/api/sync` catch-up
- mutation wrapper
- offline queue status
- toast/banner rendering from receipts

Definition of done:

- `apiCall` and raw mutation `fetch` paths create/update receipts.
- SSE invalidation updates query keys instead of each surface doing its own bespoke refetch logic.
- Existing UI still renders.

### Phase 2: workflow machines for high-risk interactions

Start with one XState machine only where the need is clear:

- `uploadMachine`

Consider adding `sendMessageMachine` after receipt-driven send/queue/delivery has been implemented and the remaining branches are still hard to reason about. Add `boardEditMachine` only if board editing develops meaningful branching beyond `editing -> saving -> saved/error`.

Each machine emits receipts and consumes acknowledgements. Keep rendering outside the machine.

Definition of done:

- Illegal states are impossible: for example, a message cannot be both "delivered" and "queued"; an upload cannot disappear after local accept without either `applied`, `queued`, `failed`, or `refused`.
- Tests cover generated state paths for the machines plus real browser behavior.

### Phase 3: durable server interaction log

Add a Rust request/interaction correlation:

- Header: `X-Amux-Interaction-Id`
- Request log columns or new table: `interaction_id`, `command_kind`, `target_kind`, `target_id`, `mutation_applied`, `rev`, `ack_status`, `feedback_required`
- Effect table or event-journal link: `interaction_id`, `effect_kind`, `entity_kind`, `entity_id`, `from`, `to`, `rev`
- Diagnostic endpoint: `/api/debug/interactions`
- User/agent endpoint: `/api/interactions/recent`

Definition of done:

- `/api/logs/analyze` can group failed/refused interactions by kind.
- `/api/why` can answer "why does the UI say this is pending?"
- `/api/interactions/{id}/effects` names each domain change caused by a command.
- A sweep can catch controls with frequent `unknown` or `failed_before_feedback` outcomes.

### Phase 4: surface-by-surface renderer cleanup

Once state is centralized, migrate rendering in slices:

1. connection/outbox/sync banner
2. worker list and peek send
3. board list/detail/editor
4. uploads/files
5. scheduler
6. settings/config

Keep each slice small enough that the lifecycle tests can identify regressions.

## Testing plan

### Unit tests

Rust:

- `MutationResult` already pins no-op/applied semantics.
- Add tests for interaction request-log extraction.
- Add tests for command-to-effect linkage in the event journal or effect table.
- Add tests that diagnostic interaction endpoints use `measured` and `n_considered`.
- Add tests that a mutation with `X-Amux-Interaction-Id` records the id even on 4xx/5xx.

JavaScript:

- Interaction reducer transitions:
  - accepted -> sending -> applied
  - accepted -> queued -> sending -> applied
  - accepted -> queued -> running -> waiting -> running -> applied
  - accepted -> running -> blocked
  - accepted -> sending -> refused
  - accepted -> queued -> failed
  - accepted -> noop
  - any unresolved receipt older than threshold -> unknown plus client-debug beacon
- Command/effect reducer:
  - one command can append several effects
  - effects retain `interaction_id`
  - receipt phase remains about the command, not the number of effects
- Query invalidation mapping from SSE:
  - `keys:["board"]` invalidates board only
  - lagged event triggers `/api/sync` or full refresh
  - ping version mismatch remains separate from data invalidation
- Receipt rendering:
  - every phase maps to a feedback requirement, or to an explicit no-feedback reason for non-consequential reads

### Browser tests

Extend `e2e/feedback-smoke.spec.ts` from five examples to a registry-driven contract:

- discover controls with `data-interaction-kind`
- perform a representative action per kind
- assert a receipt appears in `window.__amuxInteractions`
- assert required feedback appears within a budget
- assert the receipt reaches a terminal or queued phase
- assert any domain change caused by the action is linked back to the receipt
- assert no horizontal overflow after feedback

The crawler should publish a product-quality metric like:

```text
327 interactive elements discovered
84 produce commands
84 registered
84 produced receipts
84 displayed feedback
84 linked effects when state changed
0 silent mutations
```

Add failure injection tests:

- force 500: feedback says not saved, receipt phase `failed`, `/api/client-debug` records it
- force 409 gate refusal: feedback includes server remedy, phase `refused`
- offline before send: phase `queued`, not `applied`
- replay acknowledgement mismatch: phase `refused` or `unknown`, not success
- stale SSE/lost events: `/api/sync` catch-up either applies deltas or says full sync required

Use existing route-stub guard in `e2e/fixtures.ts` so unhit stubs fail loudly.

### Lifecycle tests

Tie to `e2e/lifecycle/cases.json`:

- LC-03 worker creation: receipt covers validation, create, start, cancellation.
- LC-12 board create/edit/reload/export: every save/delete/export has a receipt and visible feedback.
- LC-14 column lifecycle/gates: refused transition receipt names unmet gate.
- LC-24 composer/delivery: pending/sent/failed/retry are receipt phases, not separate UI guesses.
- LC-27 terminal controls: input/resize/connect produce visible connection or failure receipts.
- Long-running/autonomous work: accepted commands can move through `running`, `waiting`, and `blocked` without losing their original interaction id.

### Observability tests

Add a sweep test:

- call `/api/debug/interactions?since_h=24`
- assert response has `measured` and `n_considered`
- assert groups include `kind`, `phase`, `count`, and a sample
- assert effect coverage counts interactions with zero linked effects, one linked effect, and many linked effects
- inject one synthetic failed receipt and prove it appears

Add a request-log test:

- send mutation with interaction id
- force a 405/409/500
- verify `/api/logs/analyze` sample carries or links the id
- verify `/api/interactions/{id}/why` can cite the request log row and any effects

### Mutation testing

Use `scripts/mutate.sh` for the high-risk predicates:

- remove the receipt creation call from the mutation wrapper: feedback contract test must fail
- change queued 202 to look like applied: offline test must fail
- drop `ignored_fields` handling: ignored-fields test must fail
- remove `measured` stamping from debug endpoint: diagnostic contract must fail

## Implementation details

### Query keys

Use stable domain keys:

```js
['sessions']
['board', { done_limit: 0, filters }]
['messages', session]
['history', session]
['schedules']
['groups']
['files', cwd]
['metrics']
['prefs', key]
```

SSE invalidation mapping:

```js
board -> invalidate ['board']
sessions -> invalidate ['sessions']
messages -> invalidate ['messages'] and affected ['history', session] when known
lagged -> call /api/sync, then invalidate all affected keys
```

### Mutation wrapper

Every mutation goes through one function:

```js
async function runInteraction(command, feedback, requestFn) {
  const receipt = interactions.accept({ command, feedback });
  try {
    interactions.sending(receipt.id);
    const response = await requestFn({ interactionId: receipt.id });
    return await interactions.acknowledge(receipt.id, response);
  } catch (error) {
    interactions.failed(receipt.id, error);
    throw error;
  }
}
```

Raw `fetch` stays allowed for GETs and explicitly skipped endpoints, but mutating raw `fetch` without an interaction kind should log a warning in development and produce a client-debug beacon in production.

### Feedback rendering

Feedback requirements should be chosen by interaction kind, not sprinkled at call sites:

- destructive action requiring consent -> `feedback.required=true`, `persistence=durable`, `severity=warning`; renderer may choose a modal
- accepted local intent -> `persistence=until-settled`; renderer may choose inline pending row/chip
- background retry -> `persistence=until-settled`; renderer may choose sync banner
- completion -> `severity=success`; renderer may choose inline settled state plus optional toast
- refusal -> `severity=error`, usually durable until user action; renderer may choose inline error near the control plus toast for global actions
- lost/unknown -> `severity=warning` or `error`; renderer should include connection/status surface plus client-debug

Toasts are not enough. They disappear and are hard for agents to inspect. Toasts can supplement, but the receipt needs a persistent inspectable home.

### Agent-readable state

Expose `window.__amuxState` for tests and local agent browser automation:

```js
window.__amuxState = {
  query: { get: key => ... },
  interactions: { recent: (n = 50) => ... },
  effects: { forInteraction: id => ... },
  connection: () => ...,
  explain: id => ...
};
```

This is not authority. It is an inspectable projection of browser state, useful for Playwright, agents, and debugging.

## Risks

- **Over-centralization:** a giant state kernel could become another `app.js`. Keep it as four small modules and migrate by surface.
- **Receipt spam:** not every hover/tab switch needs a durable receipt. The contract is for commands and mutations; read/navigation feedback can be lighter.
- **False success:** synthetic queued responses must never look applied. Preserve the existing `X-Amux-Outbox: queued` distinction.
- **Three answers to one question:** avoid Query state + receipt state + XState state for simple saves. XState is for workflows whose branches justify a statechart.
- **Framework creep:** do not let library adoption become a React migration by accident.
- **Agent overreach:** AI-native summaries should report and recommend, not decide human-owned actions.

## Recommendation

Start with the command/receipt/effects contract. Then add TanStack Query core under it for server cache reconciliation. That gives the highest leverage with the least rewrite. Add XState machines only where the workflow has enough branches that another boolean flag would be dishonest.

The first user-visible milestone should be:

> Every consequential command produces immediate feedback and a machine-readable interaction receipt that eventually resolves to a known state. Every resulting domain change is linked back to that interaction.

That milestone is small enough to ship incrementally and large enough to change how amux feels: no more silent clicks, no more "did it work?", and a much better substrate for the next model to operate on.

The intended causality graph is:

```text
user click or agent command
  -> int_123
       -> task updated
       -> artifact generated
       -> worker resumed
       -> message emitted
```

Once amux can always explain that chain, it becomes genuinely AI-native rather than merely better state managed.
