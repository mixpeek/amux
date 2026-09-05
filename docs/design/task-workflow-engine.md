# A workflow engine for the board: review and plan

Status: proposal. Nothing here is built.
Written 2026-08-30 by the `amux` lane, reviewing an external design note.

## Verdict

The design note is right about the shape of the problem and roughly 70% of it
already exists in amux under different names. Two of its ten primitives are
genuinely missing, one is missing in a way that has cost real work this week,
and two should be refused.

The single most valuable sentence in the note is its own closing one: a
nonterminal task must carry either a next executable action or an explicit wait
condition. That is the gap. Everything else is refinement of machinery that is
already here.

## Why this is credible rather than theoretical

This document was written at the end of a session in which this lane picked up
eight cards cold, with no prior context, straight from auto-pickup. The
difference between the ones that moved and the one that could not is exactly the
note's thesis, observed rather than argued:

- **AMUX-3932** (guard bypass) carried a full reproduction matrix, a stated fix
  shape, and a named control in its `desc`. Closed in one pass.
- **AMUX-3927** (steering detector) carried a distribution, a rejected
  hypothesis, and a check that can fail. Closed in one pass.
- **AMUX-3854** carried the text `make it so this is all automatic` and a path
  to a screenshot that had been deleted. It has no next action, no wait
  condition, and no recoverable referent. It is parked on the owner because
  nobody can work it, including its author.

Three cards, one system, and the only variable that mattered was whether the
card could tell a stranger what to do next. The two that could were written by
a peer who had already internalised the discipline. Nothing in the board
required it of them.

The failure is not rare. Measured on this board:

- 20 open cards across 7 lanes point at attachments the uploads reaper deleted
  (AMUX-3937). Each is unworkable for the same reason as AMUX-3854.
- 281 cards sit stranded behind 14 lanes idle at the WIP cap (AMUX-3758).
- `~/.claude/CLAUDE.md` records 445 cards in `needsyou` with a median age of 15
  days, "most of which never needed me at all".
- The `451-folds` incident: one card holding several units of work, so no single
  status was a true statement about it.

## What amux already has

Read this before building anything. The primitives rule in `CLAUDE.md` is
explicit: if a request decomposes into board / workers / schedulers / filesystem
/ groups / memories / environment / messages, the work is configuration and UX,
and adding a ninth thing that re-expresses them is the failure mode.

| Design note calls it | amux has it as | Notes |
|---|---|---|
| Work item | `issues` row | 33 columns, incl. `type`, `epic`, `reviewer`, `source_ref` |
| Workflow FSM | `status` vocabulary | todo, doing, review, needsyou, backlog, done, verified, discarded, blocked, armed |
| Stage contract | gate per (`type`, target status) | `GET /api/board/contract` |
| Gates as predicates | `gate_ack` / `gate_checked` | A wrong `--checked` is refused, not warned |
| Gate evidence | `evidence` column, AF-321 | `--evidence-stdin`; `done` is refused without it |
| Dependency graph | `depends_on` + `board_drive` | Auto-promotes when deps reach terminal; prose fallback |
| WIP limits | per-lane cap on `doing` | Returns 409 with the holding set; `--override-doing` is explicit |
| Triage separation | `type` + capture-shell detection | Flags a captured prompt as not a unit of work |
| Waiting with a wake-up | `ask_type` / `ask_question` / `ask_unblocks` | `amux board needsyou --ask judgment` |
| Event stream | `log` column, `rev` | Human-readable, not machine-queryable |
| Attribution | `X-Amux-Session`, server-stamped | The note has no equivalent and needs one |

Two things amux does **better** than the note proposes:

1. **Gates carry evidence and refuse a claim without it.** The note treats this
   as an enhancement. Here it is load-bearing already, and `none: <reason>` is a
   supported honest answer rather than a bypass.
2. **`done` and `verified` are separate states with different contracts.** The
   note collapses verification into a stage. Splitting "implemented" from
   "confirmed in production" is the distinction that stops a green local suite
   from being reported as a shipped fix.

## The four real gaps

### G1. No continuation contract (highest value)

`ask_*` covers the WAITING case only. A card in `doing` has nowhere to put
"what I did, what happened, what to do next". So the next reader reconstructs it
from `desc`, or cannot.

This is the note's section 7 and its own stated answer. It is also the direct
cause of AMUX-3854 being unworkable.

### G2. No per-state timestamps

`issues` has `created`, `updated`, `closed_at`, `last_verified_at`. There is no
`entered_state_at`, so **time-in-state cannot be computed at all**. Every aging
signal on this board is total card age, which is why a card that moved to
`review` an hour ago and one that has sat there nine days look identical.

Confirmed by grep: no `entered_state_at`, `time_in_state` or `state_entered`
anywhere in `crates/amux-server/src`.

### G3. Dependencies drive promotion but answer no query

`board_drive` already computes `_deps_blocking` and auto-promotes cards whose
`depends_on` have gone terminal. What is missing is the read side: no endpoint
answers "what can this lane work right now". Auto-pickup selects by queue age,
not by executability, which is how a lane gets handed a card whose blocker is
still open.

### G4. `blocked` is a status, so it destroys position

The note is right and amux has the bug: `blocked` is one of the ten statuses. A
card blocked during `review` and one blocked during `doing` collapse to the same
state, and the lifecycle position is lost on the way in. Same argument applies
to `needsyou`, which conflates "parked on a human" (a dimension) with a
lifecycle position.

## What I would refuse to build

**Event sourcing / a Temporal-style history.** The note hedges on this itself.
`log` plus `rev` already gives ordering and a concurrency check, and the ethos
file already carries a settled decision against pub-sub board state ("a session
cannot consume an event faster than its next boundary"). Rebuilding the board on
an event log is a large change whose payoff is mostly already obtained.

**The seven-state FSM as written.** TRIAGE, DEFINED, READY, EXECUTING,
VERIFYING, REVIEW, DONE is five renames and two new states. Only one of the two
earns its place (READY, meaning gates passed and claimable). Renaming statuses
across ~1,900 live cards and 52 lanes buys vocabulary and costs every existing
gate, nudge and query. Add `READY`; leave the rest alone.

**A ninth primitive.** Everything below is a change to the board, not a new
system beside it.

## Plan

Four phases. Each ships independently, each is useful alone, and each has a
check that can fail. Ordered by value per unit of risk.

### Phase 1: the continuation contract (G1)

Add three columns and one invariant.

```
next_action     TEXT   what the next actor should do, one sentence
last_result     TEXT   what the previous attempt produced, one sentence
unresolved      TEXT   open questions, newline-separated
```

The invariant: a card in a non-terminal, non-waiting state must have
`next_action`, or the transition into that state is refused. Cards already
waiting on a human satisfy it through `ask_unblocks`, which exists.

Deliberately NOT a transcript. Budget it in the schema comment at 300 to 800
tokens, and have the gate refuse an empty one rather than a short one.

Check that can fail: a card moved to `doing` with no `next_action` is refused;
a card with `ask_unblocks` set is allowed into `needsyou` without one. Control:
a card WITH `next_action` still transitions, or the gate is a blanket refusal.

Risk: this adds friction to every `doing` transition across 52 lanes. Mitigate
by seeding `next_action` from the card's own `desc` on first transition and
letting the author correct it, so the honest path is the easy path.

### Phase 2: per-state timing (G2)

Add `entered_state_at`, set it on every status change, backfill from `updated`
for existing rows with a recorded caveat that the backfill is an approximation.

Then the aging signals that already exist become per-state, and the board can
answer "review for 2 days" rather than "card is 15 days old".

Check that can fail: two cards created at the same time, one moved to `review`
an hour ago and one nine days ago, must report different time-in-state.
Control: total card age is unchanged for both.

### Phase 3: the ready-frontier query (G3)

One endpoint, `GET /api/board/ready?session=<lane>`, returning cards where
state permits execution AND `depends_on` are all terminal AND entry gates pass
AND the lane is under its WIP cap. Reuse `_deps_blocking`; do not re-derive it.

Then auto-pickup selects from that set instead of by queue age.

Check that can fail: a card whose blocker is open must not appear, and must
appear the moment the blocker closes. Control: a card with no dependencies
appears, or the filter is returning empty for the wrong reason. Publish
`measured` and `n_considered` on the response, per the diagnostic contract.

### Phase 4: blocked and needsyou as dimensions (G4)

Add `blocked_on` (nullable) as a dimension orthogonal to status. Migrate the
existing `blocked` status to `(prior_status, blocked_on=...)`, which requires
inferring the prior status from `log`. Where the log cannot say, park the card
and ask rather than guessing.

This is last because it is the only phase that rewrites existing rows, and its
migration is the one that can lose information.

Check that can fail: a card blocked in `review` and one blocked in `doing` must
report different positions. Control: both must still be excluded from the ready
frontier.

## Decisions I need from you

1. **Phase 1 friction.** Requiring `next_action` on every `doing` transition
   will annoy 52 lanes on day one. Worth it, or seed-and-warn for a week first?
2. **`READY` as a state.** Adds one status and one gate evaluation point. It is
   the only new state I think earns its keep, and it is also the one that makes
   the frontier query trivial. In or out?
3. **Scope.** This is a change to `amux`'s own board that every other lane
   inherits. I can build it behind a per-lane flag so `amux` eats it first, or
   ship it fleet-wide. Behind a flag is slower and safer.

## What this does not solve

Attribution. This session produced a commit absorbed by a peer through the
shared git index, a card mis-attributed to a lane that did not create it, and
three board notes that were silently coalesced in delivery. None of those are
task-model problems and none of them get better because the board has a nicer
state machine. They are named here so the plan is not read as covering them.
