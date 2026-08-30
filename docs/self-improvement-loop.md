# Recursive self-improvement from usage

Asked by Ethan, 2026-08-23: "figure out how to make amux recursively self improve as a
harness based on usage (messages and board items)".

The short answer is that the loop already runs, it ran hard today, and exactly one of its
five steps compounds. This document measures what actually happened, names the step that
compounds, and proposes wiring only the parts that are computable. It deliberately proposes
no new primitive.

## What the loop produced today, measured

    64 commits to amux            35 `fix:`  vs  4 `feat:`
    28 distinct defects fixed     cards from 4 lanes (AMUX 18, AF 13, DESKT 7, AEAB 2)
    273 board cards created in 24h
    81 frustrations.md entries    66 open, AREA clusters: instruments 35, attribution 10,
                                  gates 7, cli 7, board 5, notices 4
    2,523 nudges in 7 days        ~879k tokens; real peer relay 0.62% of the characters

A repo whose own commit log is 35 fixes to 4 features is not building a product that day.
It is repairing its harness, from its own usage, which is the thing being asked about.

## The five steps, and which one compounds

    1. a lane does normal work
    2. it hits friction: a guard, a nudge, a gate, a number that disagrees with another
    3. it files a card and a frustrations.md entry
    4. A DIFFERENT LANE INDEPENDENTLY RE-MEASURES THE CLAIM
    5. the fix ships, and the same peer verifies the fix

Step 4 is the one that compounds, and today gives the evidence rather than the intuition:
in one session between two lanes there were SIX mutual corrections, three each way.
amux corrected me on a cluster counter-example, a scope error, and a measurement taken on a
dirty tree. I corrected amux on a gate predicate that would have refused correct
transitions, a repointing target that could not serve one of its two readers, and a
mechanism claim whose own third specimen refuted it.

Every one of those six was a CONFIDENT finding from a capable agent, and every one was
killed by somebody else re-running the measurement rather than re-reading the summary. That
is the property that improves as the models improve: a better model finds subtler defects
AND produces sharper refutations. Detection alone does not compound, because a better model
also generates more plausible wrong findings.

## Step 4 already has its mechanism, and it was made enforceable today

`reviewer` on a card, plus the `verified` gate's "peer-reviewed by a DIFFERENT worker (name
them)". Before today that gate asked for a name its own ack could not carry, and 18 of 25
verified cards recorded none (AF-160). It now requires the field and refuses without it.

So the honest state is: the compounding step exists, is gated, and is one day old.

## What is missing, in primitives that already exist

**1. Every instrument reports a LEVEL. The loop needs a DELTA.**

Nothing today could answer "did yesterday's fixes reduce the friction?". 2,523 nudges, 72%
background spend, 81 entries: all levels. A level tells you the machine is on. The
recursion needs the same instrument re-run on a cadence with the change reported, which is
a SCHEDULER plus instruments that already exist. amux set exactly this up for AMUX-3568
(re-measure in a week, and quote the nudge table and `/api/usage/attribution` together, so
that fewer nudges at unchanged spend reads as turns moving rather than going away).

Generalise that: a weekly delta tick per instrument, reporting change and never level.

**2. The discriminator exists and nothing reads it.**

frustrations.md already states its own rule: one entry is a complaint, three sharing an
AREA is an argument. `instruments` is at 35. That is the loudest ranked signal in the
system and no scheduler, view or nudge consumes it. Computing it is `grep -c`, not a model
call (ethos rule 2).

Wire it: when an AREA crosses the threshold, open ONE epic for the cluster with the entries
as children. Board plus scheduler. No new concept.

**3. The rarest step is the one that actually recurses: a finding becoming a RULE.**

Today produced 28 fixes and exactly ONE rule: 54755ee added the wrong-layer finding to
`ethos.md` ("a check pinning the wrong layer is exactly as green as one pinning the right
layer"). That single commit changes what every future session does, including sessions run
by a model that has never read this conversation.

The other 27 fixes changed code. Code fixes stop one bug. A rule kills a class, and it is
the only artifact here that survives the context window it was learned in.

So the highest-leverage change is small: when a CLUSTER closes, the closing gate should ask
"what rule does this produce, or why does it produce none?", with "none" a truthful and
common answer. Not per fix, which would flood the file; per cluster, which is where a class
is visible.

## What must not be automated, and why

- **Auto-filing a card per signal.** 273 cards a day already. More detectors without a
  discriminator is ethos rule 5: it becomes a log, and no gate can govern a log.
- **Auto-verification.** It is the only step that compounds, and it compounds precisely
  because a second party re-measured. Automating it deletes the mechanism and keeps the
  word.
- **Model calls for what is computable.** AREA counts, nudge tallies, delta arithmetic are
  all `grep` and SQL. Rule 2.
- **Deciding what a cluster means.** Whether the tubescience gate override should exist, or
  whether 25 orphaned entries may be retired by a successor, are the human's (rule 8, and
  both are live on AF-155 and AF-168 right now).

## How to know it is working

Three numbers, each a delta against a named baseline captured at the same instant:

    frustrations AREA cluster sizes      falling for a wired AREA, flat for unwired ones
    nudge volume AND background_pct      read together, per AMUX-3568
    rules added to ethos.md per cluster  today: 1 rule per 28 fixes

The third is the recursion rate. If it stays near zero while fixes climb, the harness is
repairing itself without learning, and every future session re-derives what this one knew.

## The counter-evidence, recorded deliberately

Two things today argue against automating more of this.

Six confident findings between two competent lanes were wrong. Volume of findings is not
the constraint; SURVIVAL of findings is, and survival is bought by a second lane spending
real effort. Doubling detection without doubling adversarial capacity lowers the hit rate.

And a measurement taken on a shared checkout is only as clean as `git status --porcelain`
at the moment it ran. One finding today was an artifact of a peer's uncommitted file. Any
unattended tick must record the worktree state beside its numbers, or it will publish that
class of artifact on a schedule.
