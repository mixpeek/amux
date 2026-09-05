# End-to-end acceptance: does a task actually go all the way round?

`VERIFY.md` says what proves a single change. This file asks a different
question: does the whole loop work, from a sentence a human typed to a closed
card carrying its own evidence?

Every claim below is checkable from the dashboard plus the read-only API. The
prompt at the bottom is written to be pasted, unedited, into an agent with
browser access.

## What the loop is made of

The seven claims map onto real mechanisms. Knowing which one you are testing is
what separates "it did not happen" from "it cannot happen":

| Claim | Mechanism | Where to look |
|---|---|---|
| Prompts decompose into cards | amux captures each prompt as one card whose `desc` begins `**Prompt:** `; `board_drive` asks for a split rather than working a shell | `Pickup::Decompose`, `/api/debug/board-drive` |
| Backlog is picked up | `promote_ready_backlog` (deps cleared) and the self-owned dep promotion | `board_drive.rs`, card `log` |
| Due items, oldest first | `promote_due_backlog` sorts most-overdue first, then by id so the order is total | `due_drain_plan` |
| Assets land on the card | `has_asset_link` is checked against the card's own text and **cannot** be satisfied by acking the gate | `board_store.rs` |
| Evidence is required | AF-321: `done` refuses without it | `done_evidence_required` |
| A card says where it stands | `status`, `next_action`, `blocked_on`, `waiting_on`, `depends_on`, `log`, `evidence` | `GET /api/board/{id}` |
| A message links to its card | `cmd_history.card_id`, rendered as a clickable chip | `msg-card-chip` |

## Reading a result honestly

Two failure shapes look identical from outside and call for opposite responses,
so the prompt asks the agent to distinguish them every time:

- **Not yet**: the mechanism is real, its interval has not elapsed. `board_drive`
  ticks every ~60s and skips any lane that is mid-turn.
- **Never**: the mechanism cannot fire for this card. A gate refuses it, a
  dependency is parked, the lane is at its WIP cap.

`GET /api/board/ready?session=<lane>` answers this directly: `ready` lists what
passes every precondition, `claimable_now` accounts for the WIP cap, and
`excluded` breaks down the rest, including `blocked_by_parked_dep` for
dependencies that will never clear on their own.

---

## The prompt

Paste everything below into an agent that has browser access.

---

You are auditing a running amux instance end to end. Your job is to determine
whether a unit of work travels the full board cycle and arrives with the
evidence to prove it. Report what you observe, not what the design intends.

**Base URL:** the dashboard and API are both at `$AMUX_URL` (default
`https://localhost:8824`, self-signed, so use `curl -sk`). The board is the
`Board` tab, the jobs are under `Scheduler` in the `SYSTEM` section.

### Rules that keep this audit from breaking production

This instance drives roughly 50 live workers doing real work. Violating any of
these invalidates the audit and causes damage:

1. **Create your own lane.** Use a dedicated scratch worker for anything you
   write. Never move, edit, close or delete a card belonging to another lane.
2. **Read-only on everything else.** `GET` freely. Restrict `POST`/`PATCH` to
   cards you created yourself.
3. **Never use the column ellipsis "Migrate all"** on a real column. It moves
   every card in that column.
4. **Do not press "Run now"** on a system job more than once per job, and never
   on `board-drive` more than once, since it dispatches work to real lanes.
5. If something looks broken, **record it and continue**. Do not repair it.

### What to produce

One table, seven rows, one per claim. Each row: **PASS**, **FAIL**, or
**CANNOT TELL**, plus the specific evidence (card id, field value, screenshot,
API response). `CANNOT TELL` is a real verdict and is better than a guess.

For every FAIL, say which of these it is, because they call for opposite fixes:

- **not yet** (the interval has not elapsed; the lane is mid-turn)
- **never** (a gate, a parked dependency, or the WIP cap forbids it)

Before calling anything a FAIL, confirm the probe could have produced a PASS.
State what a passing result would have looked like.

### The seven claims

**1. A typed prompt becomes worked cards, not one lump.**

Send a multi-part instruction to your scratch lane containing at least three
distinct tasks. Then:

- `GET /api/board?session=<lane>`: is there one card per task, or a single card
  whose `desc` begins `**Prompt:** `?
- A card still carrying that prefix is a *capture shell*: amux recorded the
  prompt but nobody split it. Check `/api/debug/board-drive` for your lane; a
  `decompose` outcome means the system asked for the split.
- PASS requires cards that each have a `title` describing one unit of work and
  a status that can honestly be true or false.

**2. Backlog is picked up without being asked.**

- Create a card in `backlog` on your lane with a clear `title` and a
  `next_action`.
- Note the time. Watch `/api/debug/board-drive` for your lane each minute.
- PASS: within a few ticks it reaches `todo` and then `doing`, and the card's
  `log` names what promoted it.
- If it stays put, read `GET /api/board/ready?session=<lane>`. Report
  `claimable_now`, `wip`, and `excluded` verbatim. A lane at `wip.cap` with a
  card in `doing` cannot claim anything, and that is the reason.

**3. Due items are picked up oldest first.**

- Create three `backlog` cards on your lane with `due` dates in the past:
  seven days ago, three days ago, one day ago.
- PASS: they promote most-overdue first. The documented order is by due date,
  ties broken by card id, so the sequence is total and repeatable.
- Record the actual order observed and the order expected. If only some
  promoted, that may be the per-lane rate limit rather than a bug; say so and
  report how many moved.

**4. The asset is on the card.**

- Take a card of yours through to `done`.
- PASS: the card's own text contains a link to what was produced (a URL, a repo
  path, a commit sha, or `#PR`), and it is **visible in the card detail in the
  UI**, not only in the API.
- This gate reads the card's text directly and cannot be satisfied by
  acknowledging it, so a `done` card with no link anywhere is a real failure.
  Try closing one without a link and record the refusal verbatim.

**5. The card carries its evidence.**

- `GET /api/board/{id}` for the card you closed. Read the `evidence` field.
- PASS: it names a command and its result, not a claim that something was run.
  "tests pass" is a claim. `scripts/test-contended.sh -p amux-server -> ok. 1787
  passed` is evidence.
- Attempt to close a second card with empty evidence and record what happens.
  A close that succeeds with no evidence is a FAIL of this claim.

**6. A card says where it stands, at any moment.**

Pick three cards in different states, at least one belonging to another lane
(read-only). For each, using **only the card detail in the UI**, answer:

- What is being done, and what is the next concrete action?
- Is it blocked, and by what?
- Who owns it, and is anyone waiting on someone else?
- What has happened to it so far?

The fields backing these are `status`, `next_action`, `blocked_on`,
`waiting_on`, `depends_on`, `reviewer`, `log`, `acceptance_criteria`. PASS
requires answering all four from the card alone. If you had to open the
terminal, read another card, or guess, that is a FAIL, and name which question
you could not answer.

**7. A message links to its task.**

- Open `Messages` for a lane. Find a message that references work.
- PASS: the message shows a clickable card chip; clicking it opens that card's
  detail directly.
- From that card alone, state its status, its blocker if any, and the asset it
  produced. If the chip is absent, check `GET /api/history/MSG-<id>` for a
  `card_id`: present in the API but missing in the UI is a different failure
  from absent in both, so say which.

### Finish with

- The seven-row table.
- The three findings that would most change how this system behaves, ordered by
  what they cost.
- Anything you could not test, and what blocked you.
- Every card id you created, so it can be cleaned up.
