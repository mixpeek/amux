# Workers that improve themselves

Status: **doing** (amux lane, 2026-10-30). Owner ask, Chat tab 18:25: find the
best way for workers to self-improve, by industry practice, in a way that
improves as the models improve; iterate on the approaches; show the scored
comparison and the pick before building. Follows `worker-velocity.md` (steps
1-7 landed). The amux lane's board is off, so this file is the card.

Folded in: mixpeek-meta-worker's two board-drive reports (MSG-101729 and its
follow-up, owner request MSG-101725), because they are the largest waste the
baseline below measures on the two reference workers.

## Baseline (taken 2026-10-30 18:40 local)

Files: `docs/design/self-improve-baseline/`. Two measures, because velocity
counts only time inside attempts and the biggest loss is outside them.

**Busy hours** (sum of gaps under 300 s between a worker's hook events,
`session_events`, last 7 days):

| worker | busy h / 168 | busy h by day, 10-24 .. 10-30 | open cards now |
|---|---|---|---|
| mxp-gs12 | 74.6 | 12.8 12.8 12.6 13.6 8.1 7.0 4.8 | 83 backlog, 1 doing |
| tubescience-parity | 64.2 | 4.6 7.7 13.9 12.5 7.1 11.4 6.2 | 28 backlog, 7 todo, 1 doing |

Both were idle more than half the week with full boards. mxp-gs12 fell from
~13 busy hours a day to 4.8 while 79 of its 83 cards sat parked on
`source_ref` triggers (meta-worker, 21:0xZ), 44 of them prose nobody evaluates.

**Waste inside attempts** (`/api/velocity/{w}?days=7`):

| worker | finished | share of attempt time on finished work | top waste |
|---|---|---|---|
| mxp-gs12 | 9 of 48 cards | 18% of 41.3 h | parked_waiting 14.7 h, review 11.3 h, blocked 6.1 h |
| tubescience-parity | 40 of 79 cards | 21% of 49.1 h | parked_waiting 22.6 h, lease_expired 11.5 h |

Today nothing reads these numbers: no worker called `/api/velocity` or ran
`amux velocity` in today's server log. The "Your velocity" section reaches a
worker only at launch or a rules change. Measurement alone changed nothing.

## What others do

- **Cognition (Devin).** Knowledge: Devin suggests additions from sessions; a
  person reviews them; they are retrieved on demand. Playbooks: reusable
  procedures, improved by "Devin Expert", which compares sessions where a
  playbook worked with ones where it fell short and proposes targeted edits.
  ([docs](https://docs.devin.ai/work-with-devin/devin-expert),
  [Sept '24 update](https://cognition.ai/blog/sept-24-product-update))
- **Factory.** Memory as context, injected at the start of each session;
  compression for long runs. No published loop that rewrites a Droid's own
  instructions from outcomes.
  ([memory](https://docs.factory.ai/user-guides/memory),
  [1.2](https://docs.factory.ai/changelog/1-2))
- **ACE** (Zhang et al., ICLR 2026). Context as an evolving playbook:
  generate, reflect, curate, with *incremental* delta edits to avoid "context
  collapse" from whole rewrites; learns from execution feedback, no labels.
  ([arXiv 2510.04618](https://arxiv.org/html/2510.04618v2))
- **Anthropic.** Long-running agents work from a progress file plus git
  history; start simple. A background "dreaming" pass in Managed Agents
  consolidates memory from past sessions.
  ([harnesses](https://anthropic.com/engineering/effective-harnesses-for-long-running-agents))

Common thread: the *model* writes the lesson (reflection, curation), the
*harness* decides when that happens and keeps the record honest, and the gain
is measured on outcomes.

## Candidates

- **A. Passive numbers** (status quo, velocity step 4): the section in the
  worker's context.
- **B. Reflection at card end** (Reflexion): when an attempt ends parked,
  abandoned or past its estimate, the worker writes one lesson to its memory.
- **C. Periodic curation** (ACE / Devin Expert / dreaming): once a week a
  worker reviews its own episodes and waste and makes delta edits to its
  memory; diff visible to the owner.
- **D. Harness unblocking** (meta-worker's five defects): a trigger must be
  machine-checkable to park a card (card id + status, CI run, wake-watch, a
  file line); drive evaluates it and unparks when it fires; a doing card whose
  turn ends on a wait frees the slot for the next todo; "out of scope until the
  goal" is its own state; no overlapping goal-checker runs.
- **E. Waste at the moment it happens**: when drive finds a lane idle with
  open cards, or an attempt ends in its top waste category, one bounded prompt
  names the card, its trigger text and the lane's own number for that waste,
  and asks the worker to act or to say why not. The model decides.
- **F. Evals / A-B of harness variants** (Factory/Cognition internal practice):
  run two policies on comparable lanes and compare.

## Iterations

Criteria (1-10): **Out of the way** (no hard-coded judgment, model keeps
control), **Compounds** (better model, better result; ethos rule 2 and the
compounding question), **Verified** (a before/after on the two lanes can show
it), **KISS**, **Cost** (tokens and owner attention; 10 = cheapest).

### Iteration 1: each candidate alone

| | Out of way | Compounds | Verified | KISS | Cost | Total /50 |
|---|---|---|---|---|---|---|
| A passive | 9 | 4 | 2 (baseline shows no effect) | 9 | 10 | 34 |
| B reflection at card end | 7 | 8 | 4 (lesson count is not an outcome) | 7 | 6 | 32 |
| C weekly curation | 7 | 9 | 5 | 6 | 7 | 34 |
| D harness unblocking | 8 | 5 | 9 (idle hours with parked cards, directly) | 6 | 9 | 37 |
| E waste at the moment | 8 | 8 | 7 | 7 | 7 | 37 |
| F evals / A-B | 6 | 7 | 9 | 3 | 3 | 28 |

Gap: D removes the measured biggest loss but adds no learning; E learns but
asks the model about blocks the harness could compute itself (rule 2).

### Iteration 2: split the work by who can decide it

Compute what is computable, ask the model only for judgment.

- **D' = D minus the parts that need judgment.** Machine-checkable triggers are
  evaluated by the harness. Prose is not refused (that would force a lie, rule
  3): a prose trigger stops parking after one pass and is handed to E.
- **E' = E, fired only by D's leftovers.** Idle lane with cards whose park
  the harness cannot evaluate: one bounded prompt per lane per idle stretch,
  naming the highest-priority parked card, its trigger text, and the lane's
  parked_waiting hours. The answer is an action: unpark, rewrite the trigger
  as checkable, discard, or say "out of scope until the goal" (its own state).

| | Out of way | Compounds | Verified | KISS | Cost | Total |
|---|---|---|---|---|---|---|
| D'+E' | 9 | 8 | 9 | 6 | 8 | 40 |

Gap: still no memory of *why*; the same prose trigger can come back next week.

### Iteration 3: add the learning loop, but only where outcomes say so

- **C' = C limited to the measured top waste.** Weekly, per worker, a single
  curation turn gets its own velocity breakdown and the cards behind its top
  waste category, and makes delta edits to its worker memory (ACE-style, never a
  whole rewrite). The diff lands in the Memory tab, where the owner already
  looks; no approval queue.

| | Out of way | Compounds | Verified | KISS | Cost | Total |
|---|---|---|---|---|---|---|
| D'+E'+C' | 9 | 9 | 8 | 5 | 7 | 38 |

Lower than iteration 2: C' adds a moving part before D'+E' has been measured,
and its effect would be confounded with D'+E' in a 7-day re-measure.

### Iteration 4: sequence instead of bundle

Build D'+E' now; re-measure at 7 days; add C' only if the waste that remains
is the kind a lesson fixes (parked_no_reason, abandoned, overruns) rather than
the kind the harness fixes. Same scores as iteration 2 for what ships now
(40/50), with a measured gate on the next step.

## Pick

**D'+E' now, C' gated on the 7-day re-measure.**

- Harness: triggers drive can evaluate (card status, CI run, wake-watch, event
  line in a file) unpark the card when they fire; a doing card whose turn ends
  on a wait frees the lane's slot; "out of scope until the goal" is a separate
  state that drain never offers before goal work; a goal checker never runs
  twice at once and its last verdict keeps its timestamp.
- Model: an idle lane whose remaining parks are prose gets one bounded prompt
  per idle stretch (card, trigger text, its own parked_waiting hours) and
  decides.
- Signals: `verdict=trigger_fired_unparked`, `verdict=wait_freed_slot`,
  `verdict=idle_lane_prose_park_prompt`, each with the card.
- Proof: busy hours and parked_waiting hours on mxp-gs12 and tubescience-parity
  against the baseline above, re-measured 2026-11-06.

Why it compounds: the harness stops holding work back on things it can check,
and every remaining decision goes to the model with its own numbers, so a
better model makes better unpark, rewrite and discard calls. C' is the ACE
loop, added once the measurement says lessons are the remaining bottleneck.

## Status

- [x] Baseline taken (files above)
- [x] Research and scored comparison
- [ ] Owner sees the pick (sent 2026-10-30)
- [ ] Build D'+E' with tests, mutations and log signals
- [ ] 7-day re-measure scheduled for 2026-11-06
