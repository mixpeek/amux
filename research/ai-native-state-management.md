# AI-native state management for amux

Date: 2026-09-11

## Summary

amux already has the right raw material for better state management: a Rust API with a monotonic global revision, `/api/sync`, `/api/events`, diagnostic endpoints with `measured`/`n_considered`, a durable offline outbox, client-debug beacons, lifecycle e2e coverage, and a dashboard that already tries to surface refused/queued/error states. The weakness is that those pieces are not one contract. Each surface still hand-rolls loading flags, toasts, optimistic writes, retries, local caches, and render decisions in `crates/amux-dashboard/static/app.js`.

I would make state management a first-class amux primitive, but not by introducing a ninth domain concept. It should be a thin interaction ledger over the existing primitives: workers, board, scheduler, filesystem, groups, memories, environment, and messages.

The target invariant:

> Every user or agent interaction creates an interaction receipt with an id, phase, target, visible feedback, server acknowledgement or refusal, and diagnostic trail. If nothing changed, the receipt says that. If the measurement did not run, the receipt says that too.

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

Separate state into four layers:

1. **Authoritative state**
   The database and revision journal. Source of truth. Already Rust/SQLite.

2. **Server-state cache**
   Browser cache for GET data keyed by endpoint/domain, updated by fetch, SSE invalidations, `/api/sync`, and mutation acknowledgements.

3. **Interaction ledger**
   Local durable records of user/agent intent: click, save, send, upload, start, stop, retry, discard. This is the missing layer.

4. **Ephemeral UI state**
   Open modal, selected tab, focused input, panel width. Local only, never confused with authority.

The mistake to avoid is making the UI store the source of truth. amux's existing `rev` and `MutationResult` work is exactly the right server-side center of gravity.

## Proposed libraries

### Use TanStack Query core for server state

Use `@tanstack/query-core`, not the React adapter. The official docs describe Query as async server-state management with cache lifecycle, request deduplication, retries, background refresh, mutation state, optimistic paths, invalidation, and rollback. TanStack also publishes Vanilla/headless libraries, so this fits the current frameworkless dashboard.

Why it fits amux:

- It handles the boring hard parts currently scattered through `app.js`: request lifecycle, dedupe, cache freshness, retries, mutation status, and targeted invalidation.
- It can live under the current classic SPA before any framework migration.
- It gives a shared vocabulary for "fetching", "stale", "pending mutation", "error", and "settled".
- SSE `invalidate.keys` can map directly to query invalidations.

Do not use it as the authority. It is a cache over server truth.

### Use XState selectively for interaction workflows

Use `xstate` only for multi-step workflows where illegal states are a real bug: worker create/start/send, board edit/save with offline fallback, upload, browser-control session, auth/setup, scheduler run, and deploy/update flows. XState's actor/state-machine model is specifically good at event-driven logic, statecharts, visualization, inspection, and generated test paths.

Why selective:

- A global app state machine would be too heavy and would become the ceiling.
- Per-workflow machines make "what can happen next?" explicit for humans and models.
- Statecharts are inspectable artifacts: useful for AI-native reasoning and test generation.

### Use a tiny vanilla store for ephemeral UI state

Either add `zustand/vanilla` or write a 100-line local store. Zustand's vanilla store plus selector subscriptions is a good fit if we want a maintained package, but amux may not need it immediately. The first pass can use a local `createStore({ getState, setState, subscribe, select })` to avoid dependency churn.

Use this for:

- active tab
- overlay open/closed state
- selected worker/card
- local preferences mirrored from storage
- transient input state that is not a durable draft

Do not put server data, interaction receipts, or authoritative entity state here.

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
- Zustand docs: https://zustand.docs.pmnd.rs/

## The core addition: interaction receipts

Add a browser-side `interactionManager` and, eventually, a server-side interaction log.

Receipt shape:

```ts
type InteractionPhase =
  | 'accepted'
  | 'queued'
  | 'sending'
  | 'applied'
  | 'noop'
  | 'refused'
  | 'failed'
  | 'reconciled'
  | 'unknown';

type InteractionReceipt = {
  id: string;
  kind: string;
  target: {
    primitive: 'worker' | 'board' | 'scheduler' | 'filesystem' | 'group' | 'memory' | 'environment' | 'message';
    id?: string;
    label?: string;
  };
  origin: {
    actor: 'human' | 'agent' | 'system';
    surface: string;
    session?: string;
  };
  phase: InteractionPhase;
  visible_feedback: {
    channel: 'inline' | 'toast' | 'banner' | 'modal' | 'badge' | 'notification' | 'none';
    selector?: string;
    text?: string;
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
  measured: boolean;
  n_considered: number;
  why_unmeasured?: string;
  created_at: number;
  updated_at: number;
};
```

Immediate browser-only implementation:

- Create a receipt before every command button/mutation.
- Render feedback from the receipt, not separately from the call site.
- Attach receipt id to the outgoing request header, for example `X-Amux-Interaction-Id`.
- When the server answers, update the receipt from the response body and status.
- When offline queue accepts it, phase becomes `queued`, not "success".
- When replay confirms it, phase becomes `applied` or `refused`.
- Post a compact diagnostic to `/api/client-debug` on `failed`, `unknown`, unmeasured, or "no visible feedback" cases.

Server follow-up:

- Add `_amux_interactions` table or extend request log with interaction id, outcome, rev, target, and acknowledgement summary.
- Expose `GET /api/interactions/recent` and `GET /api/why/interaction/{id}`.
- Include interaction ids in `/api/why` timelines and board/card histories where relevant.

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
      "visible_feedback": {"channel": "inline", "text": "Saved"}
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
- Every refused action has a machine-readable fix when one exists.
- Every queued action has an idempotency/dedupe key.
- Every "nothing happened" has a receipt saying whether nothing changed, it queued, it was refused, or the measurement failed.

## Migration plan

### Phase 0: inventory and contract

Create an interaction registry beside `app.js`:

- `interaction-kinds.json` or `static/state/interactions.js`
- List every mutation/control kind, primitive target, expected visible feedback channel, queue policy, and idempotency key.
- Use the UX crawler to compare interactive controls against this registry.

Definition of done:

- Every mutating button has `data-interaction-kind`.
- Every registered kind declares a feedback channel.
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

Start with three XState machines:

- `sendMessageMachine`
- `boardEditMachine`
- `uploadMachine`

Each machine emits receipts and consumes acknowledgements. Keep rendering outside the machine.

Definition of done:

- Illegal states are impossible: for example, a message cannot be both "delivered" and "queued"; an upload cannot disappear after local accept without either `applied`, `queued`, `failed`, or `refused`.
- Tests cover generated state paths for the machines plus real browser behavior.

### Phase 3: durable server interaction log

Add a Rust request/interaction correlation:

- Header: `X-Amux-Interaction-Id`
- Request log columns or new table: `interaction_id`, `interaction_kind`, `target_kind`, `target_id`, `mutation_applied`, `rev`, `ack_status`, `visible_feedback_claim`
- Diagnostic endpoint: `/api/debug/interactions`
- User/agent endpoint: `/api/interactions/recent`

Definition of done:

- `/api/logs/analyze` can group failed/refused interactions by kind.
- `/api/why` can answer "why does the UI say this is pending?"
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
- Add tests that diagnostic interaction endpoints use `measured` and `n_considered`.
- Add tests that a mutation with `X-Amux-Interaction-Id` records the id even on 4xx/5xx.

JavaScript:

- Interaction reducer transitions:
  - accepted -> sending -> applied
  - accepted -> queued -> sending -> applied
  - accepted -> sending -> refused
  - accepted -> queued -> failed
  - accepted -> noop
  - any unresolved receipt older than threshold -> unknown plus client-debug beacon
- Query invalidation mapping from SSE:
  - `keys:["board"]` invalidates board only
  - lagged event triggers `/api/sync` or full refresh
  - ping version mismatch remains separate from data invalidation
- Receipt rendering:
  - every phase maps to visible feedback or an explicit `none` with reason

### Browser tests

Extend `e2e/feedback-smoke.spec.ts` from five examples to a registry-driven contract:

- discover controls with `data-interaction-kind`
- perform a representative action per kind
- assert a receipt appears in `window.__amuxInteractions`
- assert one visible feedback channel changed within a budget
- assert the receipt reaches a terminal or queued phase
- assert no horizontal overflow after feedback

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

### Observability tests

Add a sweep test:

- call `/api/debug/interactions?since_h=24`
- assert response has `measured` and `n_considered`
- assert groups include `kind`, `phase`, `count`, and a sample
- inject one synthetic failed receipt and prove it appears

Add a request-log test:

- send mutation with interaction id
- force a 405/409/500
- verify `/api/logs/analyze` sample carries or links the id

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
async function runInteraction(kind, target, visibleFeedback, requestFn) {
  const receipt = interactions.accept({ kind, target, visibleFeedback });
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

Feedback should be chosen by interaction kind, not sprinkled at call sites:

- destructive action requiring consent -> modal
- accepted local intent -> inline pending row/chip
- background retry -> sync banner
- completion -> inline settled state, optional toast
- refusal -> inline error near the control plus toast for global actions
- lost/unknown -> connection/status surface plus client-debug

Toasts are not enough. They disappear and are hard for agents to inspect. Toasts can supplement, but the receipt needs a persistent inspectable home.

### Agent-readable state

Expose `window.__amuxState` for tests and local agent browser automation:

```js
window.__amuxState = {
  query: { get: key => ... },
  interactions: { recent: (n = 50) => ... },
  connection: () => ...,
  explain: id => ...
};
```

This is not authority. It is an inspectable projection of browser state, useful for Playwright, agents, and debugging.

## Risks

- **Over-centralization:** a giant state kernel could become another `app.js`. Keep it as four small modules and migrate by surface.
- **Receipt spam:** not every hover/tab switch needs a durable receipt. The contract is for commands and mutations; read/navigation feedback can be lighter.
- **False success:** synthetic queued responses must never look applied. Preserve the existing `X-Amux-Outbox: queued` distinction.
- **Framework creep:** do not let library adoption become a React migration by accident.
- **Agent overreach:** AI-native summaries should report and recommend, not decide human-owned actions.

## Recommendation

Start with the interaction ledger and TanStack Query core. That gives the highest leverage with the least rewrite. Then add XState machines only where the workflow has enough branches that another boolean flag would be dishonest.

The first user-visible milestone should be:

> In the dashboard, every save/send/start/stop/delete/upload action immediately creates a persistent visible receipt, and the receipt settles to applied, queued, noop, refused, or failed. `/api/client-debug` and the request log can show any receipt that fails or remains unknown.

That milestone is small enough to ship incrementally and large enough to change how amux feels: no more silent clicks, no more "did it work?", and a much better substrate for the next model to operate on.
