# Intent first: describe what you want, amux works out how

Owner request, 2026-11-12. Status: built; see [What shipped](#what-shipped) at the end.

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

## Built from primitives (owner, 2026-11-13)

The owner's ruling: "It should be simple, just using amux worker -> all the
amux primitives from intent." A chat request goes to an amux worker, and that
worker uses the primitives directly. Intent adds no store a primitive already
holds.

| What Dana asks for | Where it lives |
|---|---|
| A conversation | Messages: the thread `o:<id>` with one worker |
| Which worker a conversation is with, and why | a post in that same thread by `amux:record` (its structured part is an `intent` attachment Messages does not draw); a re-route or a "Change" is another one |
| "Remind me…" | the scheduler: the worker runs `amux wake` (`--every` for a repeating one) |
| An event | the amux calendar (`/api/cal-events`) |
| "Remember that…" | memories (`amux memory add`) |
| A draft email | the email API: the worker sends it with `/api/email/send`, which holds any outside send for the owner's OK |
| A goal | a board card on its helper's board, tagged `goal`, `owner_type: human` (a due date makes it Tracking; the status line is the helper's newest `STATUS:` line in its conversation) |
| A to-do | a card on her own board (unassigned, `owner_type: human`, tagged `todo`), made through the board's own create; Goals lists it with Done. A helper has no board of its own (new workers have their board switches off since 2026-11-02), so it asks `POST /api/intent/todos` (owner, 2026-11-13). |
| An idea | a backlog board card tagged `idea`; "Do it" starts its request, "Not now" discards it |
| What Dana started or dismissed | memories in the `intent-home` group |
| Checks for goals and ideas | the scheduler (shell kind, no model unless inputs changed) |

Chat, Ideas and Goals are tabs in the normal tab bar next to the worker list,
and are views over those stores. There is no full-screen Home and no second
bottom bar. The `home` preference only picks which tab opens first, and for a
new person it hides the worker tabs, so the default path still shows no
worker, model, terminal or setting.

`intent_outcomes` and `intent_items` (migration 0159) are dropped: an
outcome's routing moves to its thread, ideas and goals to the board.

Checked on this change: the nine use-case specs plus a to-do spec, 20 of 20 at
desktop and 375px, on two consecutive runs
([P](../../e2e/intent/results/2026-11-13-run-P.txt),
[Q](../../e2e/intent/results/2026-11-13-run-Q.txt)), each asserting no outside
send. Each new rule was broken with `scripts/mutate.sh` and its test named it:
a record post notifies nobody; a file-mailbox draft is held and its release
refused; an ASK closes on her answer; a Change moves the outcome; ideas and
to-dos are hers (`owner_type: human`, her board); helpers made for her requests
do not count as a fleet; simple mode returns on reload from the server's answer;
the other tabs hide in simple mode; a held draft shows in its conversation; Done
closes a to-do. One rule is unpinned: applying the remembered choice before the
server answers only prevents a flash.

The specs run against a private copy of the server binary
(`AMUX_E2E_SERVER_BIN`): the shared debug build is replaced by any lane's
build, and two runs on 2026-11-13 silently tested another lane's binary.

Chat's conversation uses the Messages thread view's post (owner, 2026-11-13);
runs [R](../../e2e/intent/results/2026-11-13-run-R.txt) and
[S](../../e2e/intent/results/2026-11-13-run-S.txt) are the specs on it, and
the plain post, the hidden tab list in simple mode and "Everything else" ending
the bar are each mutation-checked. A markdown table in any post keeps whole
words and scrolls sideways (shared `.cv-md` style), which is not pinned by a
test.

### What is still intent-only, and why

No intent table is left (0160 drops both). What remains:

| Left | Kind | Why a primitive does not hold it |
|---|---|---|
| `amux:record` posts in each `o:<id>` thread | Messages, with an `intent` attachment | It is Messages. Which worker a conversation is with has to live somewhere the next request can find; the thread itself is the natural place, and the structured part rides in an attachment rather than in prose. |
| `prefs` keys `intent_last_pass_ms` and `intent_goal_fp_<card>` | the settings store | When the ideas pass last ran, and a fingerprint of each goal's inputs, so the scheduled checks skip the model when nothing changed. The scheduler's own pass state belongs to one schedule; these are read across them. |
| The outcome-to-worker map in memory | a cache | Rebuilt from the record posts at boot (`load_registry`); `Conv::parse` has no store to read. |
| `AMUX_HOME/mailboxes/*.json` | filesystem | A draft-only mailbox for the household demo and the specs; nothing reads it on a real install. |

Endpoints under `/api/intent/`, each a decision or a view over primitives:

| Endpoint | Why it is not a primitive's own route |
|---|---|
| `POST plan` | The router's one model call and the validation of what it names (workers, groups, isolation). |
| `outcomes` (list, start, one, change) | Starting one is plan, then create or pick a worker, then a Messages post; the list joins threads with their records. |
| `stats` | The override rate, computed from the records. |
| `home`, `helpers`, `tidy` | Which tab opens first; which workers Change may pick; stopping idle helpers amux made. |
| `ideas`, `goals` (and start, dismiss, patch, check, approve) | Views over cards tagged `idea` and `goal`, plus the pass that proposes them and the check that keeps a goal current. A card is created and changed through the board's own handlers. |
| `reminders` | A view over the scheduler: the `amux wake` entries of the helpers her conversations went to. |
| `POST todos` | A helper cannot write to its own board (its board switches are off), so it asks here, and the card is made on her board through the board's own create. |

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

## What shipped

Every phase, composed from existing primitives (A1), as the section above
maps: an outcome is a Messages thread, ideas and goals are board cards,
reminders and checks are scheduler entries, drafts are held email-API sends.

| Piece | Where | What it does |
|---|---|---|
| Router | `api/intent.rs`, `POST /api/intent/plan` | One decision: `answer`, `send`, `create` or `continue`. The helper model judges; every worker, directory, group, type, model and effort it names is validated, an isolated worker is never a target (A9), and each correction is reported in `corrections`. Returns `why`, `candidates`, `measured`, `n_considered` and `sources`. Past "Change" overrides are fed back into its prompt. |
| Roster | `orchestrate::roster_entries` | The voice router and the intent router read one roster (name, groups, description, type, directory, isolation). |
| Dialog | `app.js` `_outcomeStart` | "Describe what you want" routes first: it answers, sends to the worker that owns the work, or creates one. Created workers are not isolated by default. |
| Outcomes | `api/intent_store.rs`, `intent_record.rs`, `conversations.rs` `Conv::Outcome` | `o:<id>` Messages threads. Where one went and why is a record post in that thread (author `amux:record`, an `intent` attachment Messages does not draw, never a notification); a re-route or a Change is another. A request is posted into the thread, which delivers it to the worker; the answer comes back into the same thread (`relay_turn`, also straight from a chat turn). A follow-up goes to the same worker with no routing call. `GET /api/intent/stats` is the override rate. |
| Chat, Ideas, Goals | `index.html` `#home-view`, `#ideas-view`, `#goals-view`; `app.js` `_home*` | Three tabs in the normal tab bar. Chat is a composer and the requests it started; a conversation is drawn with the Messages thread view's own post (`_cvPostHtml`, its `plain` option: "You" and the helper's plain name, no actions or delivery chips) and shows the reminders its helper set and the emails it wrote, held by the email API, with the approvals banner's own Send and Discard. Goals shows the next week of the calendar, reminders, to-dos and goals. For a new person (`GET /api/intent/home`: the `home` pref, then `AMUX_HOME_DEFAULT`, else intent below five workers) only these tabs and "Everything else" show. |
| Worker view | `_peekConvsLoad`, `_homeCardOutcomes` | Conversations are organized by conversation, not by worker: no per-worker tab. A worker's Messages tab starts with the conversations from Chat routed to it (each opens in Chat), and its card has one "From Chat" line naming them. Chat lists every conversation across workers. |
| Ideas | `api/intent_items.rs` | One pass reads the owner's messages to every worker (`cmd_history`), their conversations, related email, the calendar, and what they started or dismissed (memories in the `intent-home` group). Each idea must cite sources that exist. An idea is a backlog board card tagged `idea`, `owner_type: human`; "Do it" starts its request and closes the card, "Not now" discards it. Passes run one at a time. |
| Goals | `api/intent_items.rs` | Inferred goals are board cards tagged `goal` on the helper's board (a due date makes one Tracking), each with its conversation and a shell scheduler check that asks the helper only when its inputs changed. The status line is the helper's newest `STATUS:` line. Goals read and draft only: an email waits in the email API for the owner's OK, any other outside step is an `ASK:` line answered in the conversation. |
| Self-configuration | `intent.rs`, `intent_store.rs` | The router picks type, model and effort per outcome; overrides feed the router prompt; a helper amux creates gets a board of its own (`AMUX_BOARD_DEFAULT` at its scope, no automatic pickup); a daily shell schedule stops idle helpers amux created. |
| File mailbox | `integrations/local_mailbox.rs`, `api/email.rs` | A mailbox in `AMUX_HOME/mailboxes/*.json`, read by `/api/email/inbox`, `/search` and `/message` beside Gmail. It is draft-only: a worker's send from it is held like any other, and its release is refused (`draft_only_mailbox`), so nothing can be sent from it. |

### The acceptance set

The owner's inbox has no kids, camp or school mail, so the nine use cases run on
a seeded household (`e2e/intent/household.mjs`: Dana, Rafi, Ezra and Noa, a camp
waitlist, a school application, conferences, a car rental split with Tova, open
enrollment, a family calendar). Two ways to run them:

- `e2e/intent/intent.config.ts`: isolated servers at 1280px and 375px with a
  stand-in helper model and chat helper. Every spec asserts no outside send
  (`GET /api/email/log`) and no jargon on the default path.
- `AMUX_INTENT_URL=<url>`: the same specs against the household demo
  (`scripts/intent-household.sh start`), the live binary with the real model.

Persona runs (`e2e/persona/README.md`): a fresh evaluator plays Dana on the
household demo at a phone size, with only `e2e/persona/dana.sh`.

### Results (2026-11-13)

**Persona scores.** The newest fresh-evaluator run per case, on the household
demo served by the deployed binary at 8dd33f1f1 with this checkout's
dashboard, at 390px. Each evaluator had its own new browser profile and only
`e2e/persona/dana.sh`. Earlier rounds are kept in
[`e2e/persona/2026-11-13/history/`](../../e2e/persona/2026-11-13/history/), and the
iteration rounds that drove the fixes are in
[`e2e/persona/2026-11-12/`](../../e2e/persona/2026-11-12/).

| case | round | finished | correct | no jargon | steps like ChatGPT | trust | banned words seen | run |
|---|---|---|---|---|---|---|---|---|
| uc1 | 2 | 5 | 5 | 5 | 4 | 5 | none | [uc1.json](../../e2e/persona/2026-11-13/uc1.json) |
| uc2 | 1 | 5 | 4 | 5 | 5 | 4 | none | [uc2.json](../../e2e/persona/2026-11-13/uc2.json) |
| uc3 | 1 | 5 | 4 | 5 | 4 | 5 | none | [uc3.json](../../e2e/persona/2026-11-13/uc3.json) |
| uc4 | 1 | 5 | 4 | 5 | 4 | 4 | none | [uc4.json](../../e2e/persona/2026-11-13/uc4.json) |
| uc5 | 2 | 5 | 4 | 5 | 4 | 5 | none | [uc5.json](../../e2e/persona/2026-11-13/uc5.json) |
| uc6 | 1 | 5 | 5 | 5 | 5 | 4 | none | [uc6.json](../../e2e/persona/2026-11-13/uc6.json) |
| uc7 | 2 | 5 | 5 | 5 | 5 | 4 | none | [uc7.json](../../e2e/persona/2026-11-13/uc7.json) |
| uc8 | 1 | 5 | 5 | 5 | 5 | 4 | none | [uc8.json](../../e2e/persona/2026-11-13/uc8.json) |
| uc9 | 1 | 5 | 4 | 5 | 4 | 5 | none | [uc9.json](../../e2e/persona/2026-11-13/uc9.json) |

Router overrides: 0 of 71 routed requests (0%); evaluators who used Change: 0.

Checked with `python3 e2e/persona/summarize.py e2e/persona/2026-11-13 --check`
(exit 0). The overrides are summed from the household's `/api/intent/stats`
snapshots taken after each run
([`stats/`](../../e2e/persona/2026-11-13/stats/)). No evaluator used Change,
and `GET /api/email/log` on the demo shows no send.

What moved the three cases that failed a first final round:

- uc1 (trust 3, steps 3): the reminder was promised but never shown, and was set
  at 5am because the demo ran in UTC. The demo now passes `TZ` only when it is
  set, the helper says when a reminder is set in its first line, and the
  conversation shows a "Reminder set" line for every reminder its helper set.
- uc5 (correct 3): the first round hit a stale demo; rerun on a reset demo.
- uc7 (no jargon 2): "More" opened the full workspace. More now starts with
  plain choices (your calendar and reminders, ideas), and the workspace is the
  last row.

Caveat on the 2026-11-12 rounds: until `dana.sh` created each evaluator's
profile and failed loudly, several of those runs shared one browser profile, so
they were not fully fresh. Every 2026-11-13 run used a fresh profile.

**Use-case specs.** All nine specs, at desktop (1280px) and mobile (375px), each
asserting no outside send and no jargon on the default path: 18 of 18 on two
consecutive runs, [run F](../../e2e/intent/results/2026-11-13-run-F.txt) and
[run G](../../e2e/intent/results/2026-11-13-run-G.txt). On the deployed
8dd33f1f1 binary, [run H](../../e2e/intent/results/2026-11-13-run-H.txt) passed
and [run I](../../e2e/intent/results/2026-11-13-run-I.txt) failed one case:
two requests at once both asked for a new `family-helper`, the second create
failed with "already exists", and the person saw "Not sent". A failed create
now goes to the helper of that name when it exists, else the first routable
candidate (`create_fallback`, verdict `intent_create_failed` with `fallback`).
With that fix, [run J](../../e2e/intent/results/2026-11-13-run-J.txt) and
[run K](../../e2e/intent/results/2026-11-13-run-K.txt) passed 18 of 18 back to
back.

**Mutation checks.** Every new rule was broken one at a time with
`scripts/mutate.sh` and the suite named it: 19 of 19 in the first batch
(router validation, isolation, outcome routing, receipts, delivery retry, source
citation, goal tiers, approvals) and 13 of 13 in the second (reminders, home
choice, STATUS and ASK parsing, the relay guards, drafts), and 2 of 2 for
the create fallback (isolation, the name match).

**Found by the walkthrough recording (fixed after the results above).**

- A conversation showed every reminder its helper had set since the first
  request, so the camp thread listed all 13 household reminders (the helper
  also runs the household's goals). It now shows only reminders set inside its
  own turns, from each request to the answer (`_homeTurnReminders`); uc1 pins it
  with a later, unrelated reminder by the same helper.
- Two ideas passes ran 200ms apart and offered the same ideas twice. Passes now
  run one at a time, and asks that overlap fold into one more run that sees the
  first run's ideas (`Coalescer`, verdict `intent_pass_coalesced`).
- Runs [L](../../e2e/intent/results/2026-11-13-run-L.txt) and
  [M](../../e2e/intent/results/2026-11-13-run-M.txt): the nine specs on these
  fixes.

**Open gaps.**

- Goal checks and idea passes each set their own reminders, so one deadline can
  get several overlapping reminders (three for the Hudson Prep statement in the
  uc1 run). Reminders need a dedupe by subject and day.
- A first answer takes about a minute and a follow-up up to three; evaluators
  scored steps 4 rather than 5 mostly on waiting.
- The "helper is on it" line with Change and Details gave two evaluators a
  moment's doubt.
- The owner's own inbox has no household mail, so the persona runs use the
  seeded household, read through the file mailbox. Gmail goes through the same
  inbox, search and message routes, but no persona run used it.

