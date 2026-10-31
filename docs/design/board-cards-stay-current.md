# Keeping board cards current

Owner, 2026-10-31: "im finding that workers have board tasks that become obsolete over time -
whats the best way to solve this?", then "come up with the best plan and /iterate based on scores
you think are valuable especially in regards to amux vision". The plan and how it scores
against `VISION.md` are below; the iteration history is at the end. Nothing in it is built yet.

## The problem, measured on 2026-10-31

- **733 cards are open** (not done, verified or discarded, and not archived).
- **293 of them (40%) have not been touched in 7 days.**
  - By status: 134 backlog, 107 todo, 27 doing, 20 needsyou, 5 review.
  - By source: 114 written by a worker, 127 are the receipts amux files for owner messages
    (78 `capture`, 49 `command`), 15 come from automated alarms (10 `schedule_failure`,
    5 `disk_watch`), 11 from turn ends, and 25 have no source recorded.
  - By owner: 267 belong to a worker; 26 have a human owner.
- **One lane holds 61 of them** (gtm-engine). 28 belong to no lane at all.

Stale is not the same as obsolete: some of these wait on something real. But a board where 40%
of open work has not moved in a week cannot answer "what is everyone doing", and auto-pickup
keeps offering cards nobody needs.

## What already exists, and is extended rather than repeated (A1, A8, A10)

| Mechanism | What it does today |
|---|---|
| `runtime_jobs/capture_reconcile.rs` | Closes a capture card when the lane cites its `MSG-<n>` with proof (a landed commit, a reachable sha, or a closing card), card by card when a message was split. Tags one still open after 24h `unreconciled`. |
| `board_drive` needs-you recovery | Puts an old needs-you question up for review and never approves a budget or customer-outbound hold. |
| `archive_outcome` | Archives a card with a stated outcome, such as "superseded by a later card". An archived card can be restored. |
| `live` / `live_reason` | Says whether a card can still be worked, and which rule decided. |
| Intake folding | Folds a request that repeats an open card into that card. |
| `amux requests` | Links an owner message to the commit that landed it (`Amux-Request:` trailers, `amux requests close`). |
| Needs You digest (AF-510) | One batched place for what is waiting on the owner. |
| Lease reaper | Frees a card whose holder went silent. |

## The plan

### 1. Whoever files a card can prove it finished, and closes it (no model call)

Each rule extends the job that already owns that kind of card. Every close writes an
`archive_outcome` and evidence onto the card and logs `verdict=board_card_closed_by_rule` with the
rule. Every close can be undone by restoring the card.

- **Alarm cards close when their alarm clears.** The job that filed a `schedule_failure` or
  `disk_watch` card checks the same condition on its next run, and closes the card when it is
  healthy again: "the failing schedule SCHED-n succeeded at <time>".
- **Owner-message receipts close when the request does.** Capture reconcile gains a fourth proof:
  the message is linked to a landed commit or closed with a reason in `amux requests` (one
  source: the requests ledger already knows). This covers the 127 receipts whose lanes did the
  work but never cited it.
- **A card left in `doing` with no lease heartbeat and no activity for 7 days** goes back to
  `todo` through the lease reaper, with the reason on the card. It is not closed: nothing
  proves the work is finished.
- **A card whose lane is gone** (deleted or archived worker) moves to the lane that created it, if
  that lane is live, or else into the owner's weekly list (step 3).

### 2. The worker judges the rest, in one grooming turn (A2)

Only the worker knows whether its card still matters: it knows what shipped, what changed and
what replaced what. A rule on age or text would close live work, and a central janitor worker
would lack each lane's context (the workers-managing-workers lesson).

- **When:** `board_drive` offers the turn when a lane is idle, has cards untouched for 7 days,
  and has not groomed in the last 24 hours. It is delivered the way a pickup is.
- **Not for:** isolated workers and workers with the board off (A9).
- **What amux gives the worker:** context, never a verdict. That means the stale cards (oldest
  first, at most 20), each with its "done when" criteria, age, links and log tail, plus the
  lane's commits on main since each card was created.
- **What the worker does:** for each card, one of:
  - keep it, with one line on why it still matters (written to the card, so the next turn sees
    it was judged);
  - close it as obsolete, with the reason as its `archive_outcome`;
  - mark it done, with evidence;
  - fold it into the card that replaced it.

  These are the existing verbs, nothing new.
- **What it may not touch:** a card with a human owner, and a needs-you card that holds a budget
  or customer-outbound decision. Those go to the owner (step 3).
- **Cost:** at most one turn per lane per day, only when there is something stale.

### 3. The owner sees one list a week, through the digest that exists (A3)

The 26 cards with a human owner, the cards of lanes that are gone, and anything a worker wanted
to close but could not, go into the Needs You digest as one weekly item: "N cards may be
obsolete". It has a keep or close choice per card, and close all. Nothing of the owner's is
closed without that choice.

### 4. Fewer cards go stale in the first place

- **Every card states when it is done.** Intake already plans `acceptance_criteria` for the cards
  it creates. A card created without them gets the `no-done-when` tag, and the grooming turn asks
  for them first, so a card is judged against its own outcome.
- **A card can carry a revisit date** (the existing `due` field). A backlog card past its date
  is groomed first.

### 5. One measured loop, so the board says whether this works (A4, A5, closed loops)

- **`GET /api/board/staleness?since_h=168`**, per lane and in total:
  - open cards, and cards untouched 7d+;
  - closed by rule, closed by grooming, kept by grooming;
  - **reopened within 7 days of a close**, the false-close rate;
  - `measured` and `n_considered`.
- **A health invariant, `board.cards_current`:** red when the fleet's stale ratio is above 15%,
  or the reopen rate is above 5%. Red files one card on the `amux` lane per red stretch, the way
  a red main does. The loop closes on its own result, not on effort.
- **Targets:** the stale ratio from 40% to under 15% within two weeks of rollout, with reopens
  under 5%.

## Rollout

1. **Measure first.** Ship the staleness endpoint, and run the step 1 rules in dry run for a day:
   they log what they would close, and nothing is closed. Check that sample by hand before
   turning them on.
2. **Turn on step 1.**
3. **Turn on grooming,** on by default, off per lane with `AMUX_BOARD_GROOM=0`. Watch the reopen
   rate for a week before widening the 20-card cap.
4. **Turn on the owner's weekly list.**

## How it scores against the vision

Scored 1-10 against the invariants it touches, weighted by how much each decides whether this
works.

| Criterion | Weight | Score | Why |
|---|---|---|---|
| A2: out of the model's way | 25% | 9 | Judgment stays with the worker; amux supplies context and a trigger, and only closes what a job can prove. |
| A3: owner attention | 20% | 9 | No new per-card asks: one weekly digest item, nothing of the owner's closed without a choice. |
| A4/A5: nothing silent, measured | 20% | 9 | Every close has an outcome, evidence, a verdict line and an undo; the endpoint reports the false-close rate with `measured`. |
| A1/A8/A10: reuse, one source | 15% | 9 | Every step extends a job that already owns that card type; the requests ledger is the one source for "did this land". |
| Closed loop | 20% | 9 | A target and an invariant that files its own card when missed; dry run first. |
| **Weighted** | | **9.0** | |

## Iteration history

| Iteration | Score | Changes | Remaining gaps |
|---|---|---|---|
| 0 | 6.4 | First answer: computed closes, a per-lane grooming turn, a weekly owner list, stale counts. Scores: A2 8, A3 7, A4/A5 6, reuse 4, loop 6. | Ignored capture reconcile, `archive_outcome`, needs-you recovery and the requests ledger. No undo and no false-close measure. No target. |
| 1 | 7.2 | Mapped every step onto the job that owns that card type; the requests ledger as the proof for receipts (127 of 293). Reuse 4 → 9. | No way to tell a wrong close; no target. |
| 2 | 7.8 | Every close carries an `archive_outcome`, evidence, a verdict line and an undo; reopen-within-7-days as the false-close rate; the staleness endpoint with `measured`. A4/A5 6 → 9. | No target or self-filing invariant; stale cards keep being created. |
| 3 | 8.4 | `board.cards_current` invariant with targets (15% stale, 5% reopens) that files its own card; prevention through "done when" and revisit dates; a dry run before any close. Loop 6 → 9. | `doing` cards and gone lanes unhandled; the grooming turn's context was unspecified, which risked amux steering the model's judgment. |
| 4 | 9.0 | `doing` without a heartbeat goes back to `todo` (never closed); gone lanes route to their creator or the owner; the grooming payload is context only, with the existing verbs; isolated and board-off lanes skipped. A2 8 → 9, A3 7 → 9. | None blocking. To watch in rollout: the 20-card cap and the 7-day threshold. |
