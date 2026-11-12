# Intent first: describe what you want, amux works out how

Owner request, 2026-11-12. Status: proposal, nothing built.

Today a person has to understand workers, models, execution modes and
configuration before amux does anything for them. The goal: they say what they
want, and amux decides how to get it done, using the workers it already has and
creating new ones only when none fit. Full configuration stays available for
anyone who wants it.

The second half comes from the owner's feedback on Muse (screenshots of its
Ideas and Goals views): it turns everyday requests into suggested next steps
and into standing goals it pursues without being asked.

## What exists today

| Piece | Where | What it does | Gap |
|---|---|---|---|
| Describe what you want | `app.js` `openOutcome`, `api/outcome_plan.rs` | Helper model names a new worker and picks its directory and groups | Always creates a worker; never routes to an existing one. Reached only from the "+" menu |
| Voice orchestrator | `api/orchestrate.rs` | Routes a spoken command to existing workers, validated against the roster | Cannot create a worker; voice only |
| Chat workers | `docs/design/worker-types.md` | Headless Q&A turns with no terminal | Must be chosen by hand |
| Messages | `api/conversations.rs` | Channels, DMs and per-worker threads | No thread per outcome |
| Simple tab | `api/simple.rs` | Plain-English account of a worker's recent work | Per worker, not per outcome or goal |
| Artifacts, card messages | `_amux_task_artifacts`, `cmd_history.card_id` | Outputs and messages linked to a card | Not shown as results |
| Goal intent, goal loop | `api/planning.rs`, `api/goal_loop.rs` | Versioned goal intent; stops a goal looping on an owner step | Goals are set by hand per worker |
| Owner message ledger | `cmd_history` | Every owner message to every worker | Nothing reads it as a whole |

The default screen is the worker list with 31 top-level tabs, so the first thing
a new person meets is the machinery.

## Architecture

No new primitive. One routing call and one conversation record, composed from
workers and messages.

### 1. One router: `POST /api/intent/plan`

Merges `outcome_plan` and `orchestrate/plan` into one decision:

- `answer`: a quick question. A chat worker or the helper answers; no worker is created.
- `send`: an existing worker owns this work (by description, groups, directory, load).
- `create`: nothing fits. Reuses `outcome_plan`'s name, directory and group
  choice, and also picks worker type (chat or coding) and model.
- `continue`: a follow-up in an existing outcome goes to that outcome's worker
  with no routing call.

The helper model makes the judgment. Every worker and directory it names is
validated against what exists. The response carries `why`, `measured` and the
candidates considered.

### 2. Outcomes as the conversation unit

An outcome is a Messages thread (`o:<id>` in `conversations.rs`) with an
auto-generated title, recording the worker or workers it was routed to. When
the worker is board-enabled, its card links through `card_id`, and status,
evidence and artifacts come from the card. One worker can carry many outcomes;
worker names become an implementation detail.

### 3. Results first

The outcome view shows status (working, waiting on you, done, verified), the
latest reply, artifacts as file-viewer links, and the Simple narrative.
Terminal, peek, model and tokens sit behind "Show work", following
`docs/design/project-progressive-disclosure.md`.

### 4. A receipt instead of upfront configuration

Each routed message shows one line, e.g. "Sent to gtm-crm: it owns the
outreach CRM · Change". Change forwards it to another worker, or to a new one
with the full create form filled in. Overrides are logged; the override rate is
the router's accuracy metric.

### 5. Ideas: from a to-do to the next useful step

- After a batch of owner messages (a few new ones, or an hour of quiet), a
  helper pass reads recent owner messages across all workers (`cmd_history`),
  open outcomes, and the email and calendar items they mention.
- It offers a short list, each saying what it can do, from what, and the
  result ("I can pull the Camp Kaylie thread and draft a second nudge, ready
  for your approval").
- One tap starts an idea as an outcome through the router.
- Started, dismissed and ignored ideas are saved as memories and shape the next pass.
- Outside sends arrive as drafts for approval. Nothing spends money.

### 6. Goals: inferred, then pursued

- The same pass looks for recurring themes and proposes standing goals
  ("Stay ahead of the family calendar", "Never miss a school form").
- Two tiers: Tracking (finite, with a deadline) and Goals (standing).
- Each goal shows one live status line: the Simple narrative pointed at a goal.
- A goal is versioned intent in `planning.rs`, worked by a worker on the
  scheduler: a cheap shell check most of the time, a model turn only when
  something changed (the `big-goal` skill pattern). `goal_loop.rs` keeps it
  from looping on an owner step.
- Inferred goals start on their own while they only read and prepare. The
  first time one needs to send, spend or change something outside amux, it
  asks, as any worker does today. Goals can be dismissed or reworded.
- Every idea and goal records the messages it came from.

## UX

- Home becomes four views: Chat (composer and outcomes), Ideas, Goals
  (Tracking and Goals), and Workspace (every existing tab, unchanged).
- A `home=intent|fleet` pref; intent is the default for new people.
- "New worker" stays in the "+" menu for full manual configuration.
- On a phone: one bottom row for the four views, the composer above it.
- Voice goes through the same router.

## Phases

1. **Router.** `/api/intent/plan` on `outcome_plan` and `orchestrate`, with
   roster-fixture tests, each mutation-checked. The existing dialog uses it, so
   it routes as well as creates.
2. **Outcomes and Home.** `o:<id>` threads, auto-titles, the outcome view, the
   Home composer behind the pref, sticky follow-ups.
3. **Ideas.** The cross-worker pass, one-tap start, learning from dismissals.
4. **Goals.** Inference, scheduled cheap checks, status lines, ask on first
   outside action.
5. **Self-configuration.** Model and effort per outcome; expire outcome
   workers when idle (project expiry pattern); feed overrides back into the
   router prompt.

## Decisions (owner agreed 2026-11-12)

- **Isolation default.** The dialog creates isolated workers by default, and
  isolated workers have no board, so their outcomes cannot show card status or
  evidence. Decided: routed and created workers are non-isolated by
  default; isolation stays an explicit setting.
- **Inferred goals start on their own or wait for a tap.** Decided:
  start on their own, limited to reading and drafting.
