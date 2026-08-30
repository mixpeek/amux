---
description: Gut-check for every new feature or enhancement in amux. Read before building.
---

# The amux ethos

**The harness gets better as the models get better. Get out of the model's way.**

amux is scaffolding around a model, not a cage for it. Every feature either compounds
with model capability or fights it. This file is the gut check. Each rule below is
here because it was violated in this repo and cost something real.

Run the checks before you build, and again before you call something done.

---

## 1. Does capability reach the model, or only exist?

A feature the model is never handed does not improve when the model improves.

`mcp.json` shipped six MCP servers. The launcher only passed `--mcp-config` when
`CC_MCP=chrome`, and **0 of 101 sessions had `CC_MCP` set**. Six configured servers
reached no agent at all, for months. MCP is the single biggest lever for amux getting
better as models get better at tool use, and it was wired to nothing.

**Check:** who actually receives this, by default, without opting in? An extension
point nobody is enrolled in is decoration. Prefer opt-OUT over opt-IN for anything
that expands what a session can do.

**The code that makes something cheap can be the same code that makes it
unreachable** (amux + amux-cloud, 2026-08-03). The `watch` type was excluded from
auto-pickup and from rot detection, both correctly: a dormant card should not eat a
lane's WIP-1 budget or be force-advanced. But those two exclusions were the only
things that ever surfaced a card, so an armed watch became findable solely by
scrolling past it — no view, no evaluator, and the pickup query's own comment
promising "a human or the firing event moves it" while no firing event existed. Three
were already inert, including the follow-up to the incident that motivated the type;
one lane had restored and re-armed its card and been unmonitored since. The type
promised monitoring and delivered a note.

So **exemption lists deserve the same "who receives this by default" question as
feature flags.** When you exempt something from a loop, name what still reaches it. If
the answer is nothing, the exemption did not make it cheap, it made it invisible.

**A SAFETY NOTE ATTACHED TO THE HEALTHY BRANCH OF A CONDITIONAL CANNOT REACH THE
UNHEALTHY ONE** (AMUX-3718, 2026-08-25; the generalisation is
mixpeek-frustrations'). The commit nudge's union-merge directive carried CD-78's
archive check inline and correctly, and it was emitted from `commit_worthy_body`,
which by definition receives only the paths that are NOT stale/diverged/revived.
So a DIVERGED `frustrations.md` was structurally excluded from the one piece of
text that says how to merge it safely. The state that prescribes the dangerous
operation was the state that could not be told how to perform it, and a lane
followed the destructive half verbatim because that half was all it received.

Reviewing that fix, the same shape was found one instance over and confirmed with
a control before anything was changed: `ATTRIBUTION IS PARTIAL` came from the same
function, so a DIVERGED-only nudge dropped it — while that arm's prescribed exit
is "hand the path to its owner" and the caveat is the one saying the ownership
axis is blind. Two independent warnings, same emitter, both scoped to the arm
that needed them least.

Two things generalise. **A caveat about the whole SET belongs at the top level,
never inside an arm** — an arm-scoped emitter silently scopes the warning to that
arm, and nothing about reading it reveals that. And **the audit is per-STATE, not
per-string**: "is the warning present and correct" was yes throughout, so the
question has to be *which states actually receive it*. Where the arms are
enumerable, assert the matrix rather than the case, so a third caveat added to the
wrong function fails the moment its arm is listed.

Note what did NOT catch either one. A delivery-time check on the rendered bytes
catches the text going missing, which is a different failure; it is blind to a new
note born in the wrong scope. That gap was found by a reviewer asking what the new
instrument still could not see, which is the question worth asking after adding
one.

The trap nests, which is how you know it is structural rather than a slip: the fix
(`is:armed`) ran on a payload filtered to `archived=0`, and so did the review sweep
built alongside it — so three ARCHIVED armed watches remained invisible to both the
view meant to expose them and the sweep meant to fire them. Two independent authors,
same blind spot, one layer down. After adding a surfacing mechanism, ask what the
mechanism itself filters out.

**The sign does not matter; the disagreement does.** Five instances in one night, and
the fifth is what fixes the rule's shape. Four over-filtered and hid real work
(`is:armed`, the watch sweep, the advance re-nag, the owner digest — the last written
*after* its author committed the rule about the first three). One UNDER-filtered and
manufactured phantom work: the startup banner selected on status and session while
auto-pickup also required `owner_type='agent'` and `archived=0`, so a lane was greeted
with 199 queued items when 9 were real, most of the rest being cards it was
specifically not permitted to touch. Same root, opposite sign — which means "be careful
with archived filters" is the wrong lesson, since adding and removing the filter were
each wrong half the time. **The invariant is narrower: a view must share the predicate
of the mechanism it claims to describe.** A queue view that disagrees with the queue is
wrong in whichever direction it disagrees, and it is worse than no view, because it is
trusted and it is read first. When you write a view, do not re-derive its filter from
what seems sensible — copy it from the code that acts, or the two drift the moment
either changes.

The corollary is about REMOVING a filter, which looks safe and is not: correcting one
reclassifies a whole backlog as new. The owner digest dropped `archived` for good
reasons and its next run pushed 92 cards — the entire archived backlog — into a single
SMS, because "new since last time" has no cap and a delta can be a backlog-discharge
rather than a day's work. Fixing a filter is a migration event; ask what the first run
after the fix will emit.

## 2. Are you calling the model for something you could just compute?

Getting out of the model's way is not the same as calling it more often.

Auto-creating a board card labelled every task with `claude -p`. That pays a full CLI
boot, roughly 12-15k input tokens, for a three-word label. It was the most wasteful
per-call touchpoint in the 07-13 token audit, so it got throttled to one per ten
minutes per session, which is why most commands never reached the board at all. The
fix was not a bigger budget. It was deriving the title from the prompt's own first
clause, for free, and letting the model improve it later if it wants to.

**Check:** is the model doing judgment here, or string manipulation? Spend model calls
on judgment. A throttle on a model call is usually a signal that the call was wrong.

## 3. Can the model comply honestly, or does the design force a lie?

Constraints are good. Constraints that cannot be satisfied truthfully are corrosive,
because a capable model will find a way to satisfy them anyway.

Board gates derive from item type. Anything typed `code` is gated on "Implemented and
merged" and "Tests / lint pass". **1,143 of 1,215 open cards were typed `code`**,
including cards that were pure decisions awaiting a human and contained no code. The
only exits were `--force`, a false acknowledgement, or rot.

The design that works: make the escape honest. When a gate does not fit, the fix is to
correct the item's **type**, not to bypass the gate. Fix the type, not the truth.

**Check:** for every constraint, is there a truthful path forward in every legitimate
state? If not, the constraint will teach the model to assert things that are not true.

## 4. Would a wrong answer be detectable from the data you keep?

> A diagnosis being IMPOSSIBLE from the available data IS the bug.

A schedule appeared to re-fire three times in 100 minutes. It had not. Two of the
three were hand-pressed Run-now taps, but `schedule_runs` recorded no source, so a
manual run and a cron fire were byte-identical rows. The reporting session reached the
only conclusion the data supported, and it was wrong. The defect was not the
scheduler. It was that the instrument could not express the discriminator.

There is a second layer, and it is easier to miss. Once `source` existed, it lived in
a database column the consuming session had no reason to poll, while the delivered
message stayed identical. **A tag in a store the reader never opens is the same
failure as no tag.** The blindness just moves.

**Check:** when this goes wrong, what will someone see? Then: will they see it *where
they already look*? Verify from the consumer's vantage, not the producer's.

**A SET-DIFFERENCE OVER ONE FILE CANNOT SEE A MOVE, AND REPORTS IT AS A DELETION EVERY
TIME.** Five instances across four subsystems in three weeks, which is why this is here
rather than in a card. The operation is not careless; it has no vocabulary for what
happened, so it answers confidently in the one direction that reads as data loss —
"these lines are gone" — and the natural remedy, restoring them, manufactures
duplicates.

- creative-dna measured **15 of 15** "lost" `FRUSTRATIONS.md` entries as deliberate
  archive moves, after the restore/remove cycle had been run three times on origin
  before anyone noticed (CD-78, correcting AMUX-3367).
- The append-only push guard compared a FORK branch against the fork's stale
  `origin/main`; entries archived upstream since the mirror last synced read as 228
  deleted lines, and it refused an outside contributor's first branch with an
  accusation of destroying other lanes' work (AF-234).
- A drain read 17 ledger entries as missing from `origin/main`. All 17 were in the
  archive.
- The guard's own `archive_for` probed only `<stem>_ARCHIVE.md` and `<stem>_archive.md`.
  This repo spells it `frustrations-archive.md`, with a HYPHEN, so the lookup returned
  "none" **for the one file it exists to serve** — and the refusal text then asserted
  that as a fact about the repo ("this repo has none"), which was false the whole time.
- The same guard's union never called `archive_for` at all, so **every archive move in
  this repo read as a deletion.**

The last two are the ones worth sitting with: the instrument built to stop a move being
read as a deletion was doing exactly that, in the repo that wrote the rule. Proximity to
the lesson is not protection — the two authors who hit it most recently had each written
about it that same week.

Three things generalise past the file format. **Ask what your comparison cannot
express**, not merely whether it ran: a diff over one file has no MOVE, a count has no
identity, a status code has no operand. **A lookup that fails must not report its
failure as a fact about the world** — "none found" and "this repo has none" are
different claims, and the second one ends the investigation. And where a companion store
exists (an archive, a tombstone table, a moved-to pointer), the check is
absent-from-BOTH, never absent-from-one.

### The idiom, named — because it keeps being reinvented

This rule's failures share one shape, and @tsukimiya (external contributor) named it
from the outside before anyone here did: **outputs that read the same whether things are
healthy or broken.** Zero raw-tmux-fallback rows means "never happens" AND "the ledger
is broken". A blank peek panel meant "the agent printed nothing" AND "addressing never
matched". Their point was not that these are bugs — we were fixing them — but that we
were fixing each one alone.

Measured 2026-08-26, across the whole board: **16 cards, 8 lanes.** backend 5, amux 3,
amux-frustrations 2, mvs-infra 2, and one each from mvs-research, gtm-engine and
cold-outbound. Evaluations reporting SUCCESS when every query hard-failed; `cwd` exposed
as `dir`, where a wrong field name is indistinguishable from an empty one;
`durability_live` reading 0 for 3.5 minutes of every cycle; a dead Apollo
indistinguishable from no matches. Sixteen authors, sixteen separate diagnoses.

**And the fix is already an idiom here, reinvented every time.** Six independent
authors, converging: `scan_truncated` and `actual_window_h` on `analyze`/`stats`;
`truncated` + `page_span_h` on `/api/logs`; `ran` on `/api/health/invariants?id=`;
`latest_per_invariant`, which is the only place a PASS is visible at all;
`ignored_fields` + `applied:false` on board PATCH, with 422 when EVERY key was
unwritable; and autofix's blindness check routing its HEALTHY ZERO through `suppressed`
with the comment *"a zero here is a measurement; silence would not be."*

So state it once, as a prescription rather than sixteen discoveries:

> **Any output that can read ZERO or EMPTY must publish, in the same payload, whether
> the measurement RAN.**

In the same payload is load-bearing — this rule's own second layer is that a tag in a
store the reader never opens is the same failure as no tag. A `/api/debug/` endpoint
that could have answered it does not count.

**The review question, which is cheaper than any check and catches it before it ships:**
*if this reported zero, could the reader tell healthy from broken?* Most of the 16 would
have been caught by asking it once, at the point of writing.

## 5. Does it accumulate, or does it discriminate?

Automation that appends without deciding degrades as volume grows, no matter how good
the model is.

Every inbound prompt was folded into whatever card a session already had open. One
card reached **451 folded tasks**. At that point it is not a task, it is a journal:
nothing about it is done or not-done, so no gate can govern it, and no model can
reason about it. 421 cards were in that state.

**Check:** at 100x the current volume, is this still coherent? If the answer is "it
becomes a log", it needed to split, not append.

## 6. Is the audit trail real, or just claimed?

The board contract advertised `force` as "bypass (judgment stays with you; **logged**)"
in two separate places. Nothing anywhere logged it. The one escape hatch from the
entire gate system was the one action leaving no trace, while telling you it left one.

An unauditable bypass that claims to be audited is worse than an honest one, because
it gets trusted.

**Check:** grep for the thing the docstring promises. If the promise is not implemented,
either implement it or delete the claim.

**A constraint whose sanctioned escape is unwalkable from the audited path will be
walked from an unaudited one** (AMUX-2325, 2026-08-04). `amux board <status>` sent only
`{"status":...}` — no `gate_ack`, no `gate_checked`, no way to set `type`. So the moment
a gate fired, which is most cards, the only way forward was a hand-rolled
`curl -X PATCH`, and a hand-rolled curl omits `X-Amux-Session`. **The gate was
manufacturing the unattributed writes the gate system depends on being attributed** —
the same system whose one tolerable bypass is tolerable only because judgment stays with
a NAMED party. The 409 body was well-designed and did publish the escape, but purely in
HTTP terms (`gate_ack: true`, "GET /api/board/contract"), never naming an `amux board`
command; an agent following it *literally and correctly* ended up off-trail. Reading the
error did not help, because complying with it required leaving.

Two lessons that generalize past this bug. First, the fix is never a rule telling people
to remember the header — **make the honest path the easy path**, and route agents back
onto the audited command rather than teaching them to hand-roll it better. That closes
the whole class at once: mixpeek-orchestrator hit the same defect from the other side the
same day, hand-rolling the *response* handling (`d.get('ok', True)` defaults True, so a
`{"error":..., "blocked":true}` body read as success and a card was reported closed while
untouched). Dropping to curl loses attribution AND outcome verification; restoring only
one leaves the worse half. Second, **check whether the refusal destroys the evidence
needed to satisfy it**: a PATCH is atomic, so `{"desc":...,"status":"done"}` that trips
the gate discards the outcome text too, and the retry then fails for a *new* reason —
which reads as the gate being capricious when it is doing exactly what it says. Record
the outcome as its own write, before the transition.

**Check:** for every constraint, walk its documented escape using ONLY the sanctioned
tooling. If you cannot, the constraint has an unaudited back door and it is already in
use. Related: rule 3 (can the model comply honestly) and AMUX-2140 (following the
sanctioned instruction exactly is what produced the failure).

## 7. Can your check actually fail?

A green check that cannot detect the bug is theatre, and it is worse than no check,
because it confers false confidence.

Removing the notes feature left `closePeek()` calling a deleted function. The X button
in session peek silently did nothing, and every later click hit an overlay that never
closed. Both standing checks passed the whole time:

- `python -c "import ast; ast.parse(...)"` is **blind to the client**, which lives
  inside a Python string literal.
- `node --check` proves the script **parses**, not that every name it calls **exists**.

The check that finds it: enumerate every function defined in the client, diff against
every function called, and inspect the callers.

**AND THE MIRROR, which cost a live regression on 2026-08-25: every name you CALL
must exist, and every name you DEFINE must not already.** AMUX-3715 added a
`_renderArchivedSection` for the board; the sessions view had owned that name for
eleven thousand lines. Function declarations hoist and the LAST one wins, so the new
one silently replaced the old, every sessions call site started running a body
expecting an argument it never passes, and the main dashboard view died on
`container.appendChild` of `undefined`. A peer diagnosed it (gtm-research, 7607ee46).

That commit message had claimed the check was done — "every function the new code
CALLS was verified to exist" — which is the direction that was already covered. Half a
bidirectional check reads exactly like the whole one when you are the person who ran it.

**The language hid it, and that generalises past JavaScript.** A duplicate `let`/`const`
is a SyntaxError `node --check` catches; a duplicate `function` is legal. So the parse
check gave real coverage on one shape and none on its twin, with nothing from the
outside distinguishing them. When a tool covers a class, ask which members of that class
the LANGUAGE makes legal — those are the ones it silently does not cover.
`tests/dashboard_assets.rs` now enumerates duplicate top-level declarations; verified by
restoring the collision, at which point `node --check` still passes and the guard fails.

**Check after any deletion:** what would still be green if I had broken this? Test the
shipped code path, not a paraphrase of it. Simulating what you believe a function does
cannot catch that function doing something else.

**Record which hypotheses are DEAD, not only which one was right.** A root-cause card
that names the live cause is worth less than one that also names what was ruled out,
because the ruled-out theories are the ones the next person will independently re-run.
amux-cloud's AC-194 carried two of their own disproved theories — reviewer-routing
returning first (only 2 of 19 cards carried a reviewer) and a wrong sort order (real, but
a follow-on hazard rather than the cause) — and explicitly superseded an earlier note
where they had reported the first as likely. That is what stopped the ordering bug being
mistaken for the fix, and stopped a third session re-measuring reviewer routing at 1am.
The same applies to a hypothesis that was WRONG BUT SPECIFIC: creative-dna's "the list
serializer chokes on a legacy row" was false, and ruling it out required comparing both
read paths — which is where the actual defect (one path scoped, one not) was sitting. A
vague correct suspicion would not have produced that. Kill hypotheses in writing; a dead
one is evidence, not embarrassment.

**A filter that silently matches EVERYTHING is the same defect as one that matches
nothing — except it returns a confident wrong answer instead of silence.**
`interaction_log.ts` is in MILLISECONDS. Two sessions the same evening wrote
`datetime(ts,'unixepoch')` and compared against a seconds cutoff, so the filter was
~1000x too small and matched the entire table. One of them nearly reported the whole
historical backlog as post-fix regressions. The tell in both cases was the rendered
timestamp column coming back empty — and it only caught one of us, because for that
session the timestamp was load-bearing for the claim being made, while for the other it
was decoration next to an actor tally that happened to be right. A broken instrument
that hands you a usable answer is the most dangerous kind: nothing prompts the recheck,
because the part you were looking at was fine. Before trusting a filtered query, confirm
the filter EXCLUDED something — an unbounded match and a correct match look identical
from the rows alone.

**Test the fix against the incident's own artifact, not against the case that is easy
to construct.** ts-gke reported a live watch card force-discarded by an unattributed
caller. The fix — require attribution for `force` — was first written as
`if eff_gate and force`, which passes every test built from a convenient card and would
have let the reported specimen straight through: a `watch` card's todo->discarded has no
gate, so `eff_gate` was empty while `force` still stamped the History line and skipped
the dirt/WIP/reviewer checks. The convenient case is convenient *precisely because it
lacks the property that made the incident*. Rebuild the specimen from the log line, then
run the check against it — a check that cannot fail on the case that motivated it is the
purest form of theatre, because the incident report itself is what certified it.

Verification habits do not transfer between operands. A session that learned to
re-read STATUS after the exit-code bug kept re-reading status while its DESC
writes were being silently destroyed twenty times over (desc_append, AMUX-2161)
— the habit gave the feeling of rigour while pointing at the wrong field. Verify
the operand you just wrote, not the one that burned you last time.

A fresh read of the artifact beats being more careful (MG + amux, 2026-08-02):
neither session caught its own error by reasoning — the 12-vs-16 undercount fell
to a re-measure instead of a re-quote, and the false "passes clean" (verdict
tested on a SYNTHETIC shape while the real card carried 33 fold-residue lines)
fell to the pickup notice arriving and being checked against known cards. Test
against the real operand, and when a report arrives, re-read the artifact it
names before defending the code. Related: a right answer via the wrong mechanism
(a prose match latching a CITATION id instead of the dependency) stays right only
until the coincidence lapses — verify the mechanism, not the verdict. And the
session running a test is often not the one holding the discriminating
instrument: three tests in one day were undecidable from the tester's side and
instant from the log-holder's — say so early instead of polling harder.

A silent probe is dangerous; a LOUD WRONG probe is worse (amux-cloud, 2026-08-03).
Two sessions the same night concluded from a probe's SILENCE (a grep for
`use_reloader|watchdog|reload=True` could not match an mtime watcher, so its
no-hit was uninformative and got read as evidence). The spin-catcher failed the
other way: it answered. It fired 625 times and named functions — all of which
were `time.sleep(...)` lines, because `faulthandler` dumps every thread and
ranks none, so on a 10-thread process the nine sleepers are printed with the
same authority as the one that matters. Its `tail -c 4000` cap then discarded
the working threads and KEPT the idle ones, so the truncation actively favoured
a wrong answer. Nothing looked broken at any step. Ask not just "could this
check fail" but "if it fires, does its output DISCRIMINATE?" — an instrument
that always produces a plausible-looking answer will be believed, and evidence
caps must be checked for which end they keep. The fix was to capture the
measurement that ranks (`ps -M`, per-thread CPU) alongside the one that
describes.

**What does the detector COST, and is the cost paid in the same resource as the
fault?** (orch's formulation of amux-cloud's spin-catcher, 2026-08-03. If yes, the
detector is part of the incident.) The catcher tripped on `cpu >= 70` and each trip
sent two SIGUSR1 stack dumps and wrote ~20KB into `server.log` — while the fault under
investigation was contention on the `server.log` lock. 625 trips of self-inflicted log
pressure, aimed precisely at the resource whose starvation it was hunting. This is
worse than an ordinary false positive: a probe that matches itself in a `ps` listing
manufactures a signal you can filter out, but this one AMPLIFIES the real fault, so the
system genuinely gets sicker the harder you watch it and the resulting signal is REAL.
The more it fires, the more it is right; the more it is right, the more it fires —
unfalsifiable from the inside.

Two rules fall out. First, **a threshold below the baseline is not a detector**: this
server idles at 102.5% CPU with `store=ok`, so `>= 70` was reporting that the machine
was ON. Adding a sustain requirement cut 625 trips to 53 without touching that — it
made an uninformative level fire less often, which is not the same as making it
informative. Second, **prefer the structurally-absent signal over the tuned
parameter**. The fix was not a better threshold; it was DELETING the CPU trigger and
keeping only what is absent in the healthy state — `/health` unanswered, `store=hung`,
`degraded`. Picking a window or a threshold at all is the tell that you are guessing.

**An empty grep FEELS like a measurement, and that is why silent probes get believed**
(amux-cloud, three times in one night). The mechanism is not carelessness: running a
command feels like doing an experiment, so the no-hit inherits the authority of the
act. But a grep you typed and a grep that COULD have found the thing are different
objects, and only the second is evidence. The three: `head -6` on a 16-line commit body
concluded a passenger section did not exist (it was at line 10); `interaction_log.ts`
read as seconds made a cutoff ~1000x too small so the filter matched the whole table;
and a 44-line window anchored on a log line searched 40 lines UP when the code lived 11
lines DOWN, producing a filed defect against a cap that was there the whole time. The
third happened hours after writing the rule above, which is the part worth keeping:
authoring a rule does not install the habit.

**The precondition, which is cheaper than the prohibition: before believing a negative,
say what a POSITIVE would have looked like, and confirm the probe could have produced
it.** "If the cap existed, where would it be?" answers before "is it there?" does. Where
a positive is cheap to construct, construct one — run the highlighter on text that
should match before trusting that it did not match; check that a control row appears
before concluding the treatment row was filtered.

**The failure is not carelessness, it is that a hand-written probe is a GUESS about
where the answer lives, and a guess that misses is indistinguishable from an answer
that is absent.** Two sessions logged NINE instances in one day (2026-08-08,
amux-cloud + amux), and the value is in the count rather than any one of them, because
each looked like a different mistake and every single one would have reported working
code as broken or missing:

- a positional slice matched the fix's own COMMENT, which quoted the string it removed
- a grep for `:amux-server.py` against code that says `":${FILE}"`
- BSD grep read the `$` in `${FILE}` as an anchor (`grep -F` finds it)
- an `if True:` fixture built to "break" a file, which is valid Python — the following
  indented lines just became its block, so the probe could not fail
- a pattern missing backticks: `Do NOT reach for force` against
  ``Do NOT reach for `"force":true` `` — on a security-adjacent check, where the false
  negative reads as the vulnerability having returned
- a slice window too small for the verbose comment that preceded the code, so the test
  failed against the CORRECT fix
- the first `<select>` matching a string, out of 28 on the page, three of which matched
- an env value read straight out of a file with its quotes still attached, so `[ -d ]`
  reported an existing directory as missing
- a latency measured across a server restart, which makes any number meaningless
- `git log -S'TIMESTAMP_COLUMNS'` returning nothing while `git diff` plainly showed the
  added row — the pickaxe reports commits where the COUNT of the string changed, and
  adding a row inside an existing const does not change how many times the const's name
  appears. Structurally invisible to `-S`, obvious to `-G`
- a literal ADJACENT-TOKEN pattern, `git worktree add`, against callers that write
  `git -C "$REPO" worktree add --detach` — the option sits between the two tokens, so the
  pattern cannot match. It returned a comment and a test fixture, which read as a clean
  negative, and the entry built on it (AF-190) claimed "nothing builds the COMMIT" while
  the auto-builder had been building exactly that for fifteen days. The blindness is not
  incidental: a tool that builds a detached snapshot MUST operate on a repo it is not
  cd'd into, so `-C <repo>` is the form this whole CLASS of caller takes, and the probe
  excluded the class it was searching for

The three that generalise past "be careful": **name the target before you search for
it** (which of the 28 selects? which of the two branches?), **bound a positional window
on the CODE, not on however much prose precedes it**, and **when you built the broken
fixture yourself, verify it is actually broken** — "I broke it" is a claim, not a
premise, and it fails silently because everything looks like it ran.

The `-S` case earns its own line because it is the first of these answerable BEFORE
running anything, and that is the direction to push: *would a positive change the COUNT
of this string, or only the lines around it?* If only the lines, `-S` cannot answer and
`-G` is the tool. Every other instance above needed a second instrument to disagree in
front of you, and "I happened to notice two instruments disagreeing" is not a habit — it
is luck. A precondition you can state in advance is.

The ADJACENT-TOKEN case is the second one answerable in advance, and it is worth stating
separately because it fails on the most ordinary thing you can type: *a multi-word
pattern asserts the words are adjacent in the source, and a command's words usually are
not.* `git -C <dir> worktree add`, `docker --context x compose up`, `curl -sk -X POST`
all put an option between the tokens someone would grep for. Before believing a negative
from a multi-word probe, ask which words could have something between them, and search
on the token pair that cannot be split. The sharper version of the precondition: the
form a probe excludes is often the form the thing you are looking for MUST take, because
the option you left out is what makes the caller the kind of caller you want.

**The tell is the MISSING ACCOMPANIMENT, not the answer** (amux-frustrations, SIX in one
night, 2026-08-20). The count is the argument: any one of these reads as carelessness, and
six in a night is a property of the surfaces. Every one returned something answer-SHAPED —
an empty string, a reversed label, `ok:true`, `exit 0`, a plausible zero — and in no case
was the result itself the tell.

- an `until [ "$(curl .../health | py 'print(d["build"])')" != "$OLD" ]` wait loop: the
  health call failed mid-restart, python raised, the expression was EMPTY, empty != old,
  and the loop exited printing "ADOPTED". A WARN storm was then measured against the OLD
  binary. *Missing: it never printed the hash it had supposedly adopted.*
- `git diff --numstat origin/main...main -- <file>` labelled "what origin added that I
  lack". Three dots diffs merge-base -> main, so those were MY changes with the label
  reversed, and another session was seconds from being told their gate was still live.
  *Missing: `behind=0`, already on screen, said origin had nothing.*
- filtering `/api/logs` rows on `ts` inside an outage window and getting zero, from a page
  that is newest-first and capped at 2000, every row of which post-dated the window.
  *Missing: no count of how many rows the page could even span.*
- reading a schedule's `last_run_at` when the field is `last_run`: three schedules
  reported `None` and a 12.6h outage was briefly believed. *Missing: no key listing next
  to the value.*
- grepping `/api/debug/boundary` for a `families` key that does not exist, and printing
  "families tracked: 0" against a live, correct response.
- importing `git-shared-guard.py` to A/B it. A module-level `sys.exit(main())` exits the
  IMPORTER with code 0; a peer's whole test suite printed NOTHING and exited 0 with every
  assertion unreached. *Missing: no PASS line, from a suite that "passed".*

Four of the six had already produced a stated conclusion about to be acted on. None was
caught by instrumentation; each was caught by a second look, which is a habit rather than
a property. So the precondition is not "be careful" and not "check the result" — it is
rule 4's accompaniment test: name what should appear BESIDE the answer if the probe really
ran, and check for that.

Two of the six were amux defects and have their own fixes (the module-level `sys.exit`, now
`__name__`-gated, and `/api/browser/start` silently accepting unknown fields, AMUX-3403).
The `/api/logs` surface is fixed too: it now publishes `truncated`, `page_span_h`,
`total_matched` and a `note` naming the `until` parameter to page backward, so the capped
newest-first page states its own span instead of letting a zero pass for an answer. The
remaining two are field names differing by a suffix, which no surface can currently tell a
caller they misread.

**A COMPOUND OPERATION TAKES ITS SUCCESS SIGNAL FROM THE PARTS THAT WORKED** (three defects
in two days, 2026-08-23). The sibling of the accompaniment test, and the reason it needs its
own name: here nothing is missing from the output. The operation genuinely succeeded, one
step inside it did nothing, and the success of the whole is what got reported.

- mutation-testing a guard by disabling `if !p.is_absolute()`: the test then did what the
  unguarded code says and CREATED a directory in the shared checkout. The FILE was reverted
  and the mutation reported clean; the directory outlived the revert and failed every later
  local run while CI stayed green. The revert succeeded at its visible half.
- a version bump written as a literal replace, `'0.9.701' -> '0.9.702'`, matched nothing
  because a peer had moved both files to 0.9.708 between the read and the write. The same
  edit pass made the functional changes and printed "patched", and those were what got
  asserted on. The UI fix reached no browser holding the cached script.
- a recovery sweep classifying on `desc` after the default board list went slim, which does
  not carry it. `.get("desc") or ""` was empty for every row, so it printed "0 to do" on a
  schedule while 76 unowned reports sat there. The FETCH succeeded, and the fetch is what
  the sweep reported on.

Three habits, and all three are listed because each catches exactly one of the above and
none of the others:

- **assert the WRITE changed something**, not that the code ran (`assert new != old`, per
  file) — a literal replace that matches nothing is indistinguishable from one that matched;
- **after mutating a guard OFF, ask what the code does WITHOUT it.** That is precisely what
  the guard prevents, so the answer is never "nothing", and the side effect outlives the
  revert of the file;
- **when classifying on a field, confirm the field is PRESENT before concluding from its
  absence** — an empty classification over a non-empty fetch is the loud-wrong-probe shape,
  answering confidently from a column that was never there.

All three have shipped fixes (7759b36 the APP_VER/CACHE mover, c207339 the sweep refusing an
absent desc, 1998c75 `scripts/test-tree-clean.sh`). The last is the one with a design point
worth keeping: `git status --porcelain` reports ZERO LINES for an empty-directory residue,
and so does `-uall`, because git does not track empty directories — so the OBVIOUS guard
would have been green and unable to fail on its own motivating incident. `git clean -nd`
sees it and cannot see a modification to a tracked file, so the snapshot is the union of
both. It ships a `--self-test` negative control, and .github/workflows/rust.yml runs that
control FIRST, before the wrapped `cargo test`, because a guard nobody has watched fail is
a guard nobody should trust — this one's self-test shipped once asserting merely "non-zero",
which passed while carrying the exact bug the control existed to catch.

**The fixture must live in the same DOMAIN as the defect, not merely exhibit its shape.**
The mutation-strength rule above says to mutate the arithmetic rather than the wording;
this is its companion, and it is the one that lets a green suite coexist with a live bug.
AF-161: the board's slim payload dropped a column in `list_body`, one layer ABOVE the
snapshot that `snapshot_slim_is_snapshot_minus_prose` pins — a real property, correctly
asserted, in a place the shipped path does not flow through. Nothing about READING that
test reveals which layer it holds. So ask where the defect would be INTRODUCED and confirm
your fixture flows through that code rather than an ancestor of it. Corollary for the
opposite error: when you assert a failure will be LOUD, name the idiom that makes it loud
and check it is the one your callers write — `row["desc"]` raises and `row.get("desc")`
returns `None`, and the safety argument was made for the first while every consumer wrote
the second.

The tell that beats all of them: a red test on code you just verified by hand, or a
clean result you did not expect. Both mean the instrument is a candidate before the code
is.

The sharpest variant: the sanctioned instruction itself can be the theatre. Every
assignment notification told sessions to run `amux board claim <id>`; the command did
not exist, fell through to the help text, and exited 0 — so following the instruction
EXACTLY produced a success signal and no claim (AMUX-2140). When the instruction and
the failure are the same action, no amount of care catches it; only using the result
does. Anything a notification or doc tells an agent to run must itself be exercised.

**"Can it fail" is not the whole question. "Does it sit where the thing happens" is the
other half, and this rule was missing it** (AF-161, 2026-08-23). The board's slim list
payload dropped the `reviewer` column, so an audit of verified cards read 25 of 25 as
unreviewed when the truth was 7 named and 18 absent — 100% wrong, in the direction that
looks like a finding. There WAS a guard: `snapshot_slim_is_snapshot_minus_prose`, whose
doc comment says the two snapshots "cannot drift" and pins it anyway. Both statements are
true. It was green the entire time the bug was live, because the removal happens one layer
UP, in `list_body`, and the guard pins the snapshot. **A check pinning the wrong layer is
exactly as green as one pinning the right layer, and nothing about READING it reveals which
you have.** That check could fail — on a real property, in a place the shipped path does
not go through. So the question is two questions, and we only had a habit for the first.
Ask where the defect would be INTRODUCED, and confirm your test's fixture flows through
that code, not through an ancestor of it.

**WHEN A CONSUMER READS A PREFIX, VERIFY THE PREFIX — correctness of the whole
collection is not the property under load** (AMUX-3695, 2026-08-25). The
commit-nudge samples dirty paths under a time budget, so it consumes the first ~4
of an ordering it builds. Two successive attempts were correct about the whole
list and wrong about the part actually read. One-slot-per-directory is unbiased
across directories and gave 17 singleton groups 41% of the weight for 3% of the
files. Replacing it with a proportional key fixed the aggregate and still put
every singleton at exactly 0.5, so with one group of 20 and eight singletons the
first TEN picks came from one group and no singleton appeared at all — a budget
stopping at four would have seen a single directory.

This is a step past "a check pinning the wrong layer", and mixpeek-frustrations
named the difference: it is not a premise that stopped holding, it is a premise
that was never about the consumed part in the first place. Every whole-list
assertion passes, honestly, while the property the program depends on goes
untested — because the test looks at the artifact and the program looks at its
first N items. Sorting, ranking, prioritising, batching, paginating and
budget-capped scans are all this shape. Assert on `&order[..n]`, not on `order`.

**A TOTALIZING WORD IN YOUR OWN DESCRIPTION IS THE TRIGGER: test at the scope of the
MECHANISM, not the scope of the bug** (AMUX-3719, 2026-08-26; the trigger is
gtm-media-assets'). A test-isolation flake was fixed by having `HomeGuard` snapshot the
whole process env and restore it on drop — total, no key list, cannot go stale. The
targeted test passed and the flake it was aimed at disappeared. The full suite then
failed an unrelated test that sets an env var with a bare `set_var` outside any guard and
runs twice expecting the same cap: the blanket restore deleted that variable between the
two runs. One flake traded for a broader one, and the broader one could clobber any
variable any test owns. (The scoped fix reads the fixture's own `server.env` keys, which
is the single path that exports into the process env.)

What generalises is not "be careful with env". It is that **the properties which made the
mechanism feel safe — whole, no list, cannot go stale — are the same properties that put
its blast radius outside anything the targeted test could express.** A total mechanism
reaches everything by definition, so a test scoped to the motivating bug is structurally
incapable of covering it, no matter how carefully it is written. Being more careful was
not available; the targeted test was green and correct about its own claim.

The trigger is mechanical, which is the point: when the sentence describing your fix
contains *whole, all, every, blanket, total, cannot go stale*, the check must run at the
widest scope the mechanism touches. The word is already in your own commit message.

Note the direction, too, because it is the same blind spot as committing an untracked path
without checking origin (gtm-media-assets, same day): removing or overwriting invites
verification, since you can visibly destroy something. This change was framed internally as
"restore MORE", which felt additive and safe, so the leak was verified closed and nothing
asked what else the restore now reached.

**When you argue that a failure will be loud, NAME THE IDIOM that makes it loud, and check
it is the one your callers write.** The same slimming was defended in its own comment as
safe because `.desc` on a slim row is "a KeyError, which is loud, not silently empty". That
is true for `row["desc"]` and false for `row.get("desc")`, which returns `None` and says
nothing — and `.get` is what every consumer here actually writes. A safety property that
holds only under a calling convention nobody uses is not a safety property, which is why
this same discovery got made twice, one column at a time (c207339 fixed the caller for
`desc`; AF-161 found `reviewer` weeks later). The fix that ends the class is to make the
payload SELF-DESCRIBING about what it omits, so a consumer can refuse instead of reading
absence as emptiness — rather than restoring one column and waiting for the next report.

**A rule you have written down is not a rule you run, and the moment of highest
risk is when the result matches what you expected** (amux + cold-outbound,
2026-08-07). Two sessions, one morning, the same shape twice each. cold-outbound
reported that a PATCH "silently ignores" a field — the response carried
`ignored_fields` plus an explanatory note the whole time; they read the 200 and the
bumped `rev` and never opened the body, against a rule they had written for
themselves in almost those words ("confirm at the FIELD, never at the status code").
Hours later I did the identical thing to three cloud customer cards, reporting them
un-archived when the same body said otherwise. Then I twice cited a commit sha
written into prose BEFORE the commit existed, while actively writing about
unverified citations.

The predictive half is not "read the body". It is that a CONFIRMING result is where
the check gets skipped: nothing about an expected answer feels like the moment to
verify, so the habit fires on surprises and sleeps on agreement — which is exactly
backwards, because a wrong expected answer is the one nobody else will catch either.
The counterpart is that writing the rule down buys nothing, since the rule was
written and then not run by its own author within hours, twice.

Corollary, and the more generalisable half: **when you kill a misleading signal, ask
which signal people will reach for next, and whether it can carry the weight.**
Making no-op PATCHes return 400 fixed the trap and immediately created a new one —
callers would switch to "did `rev` move?", and `rev` did not move for tag writes.
cold-outbound caught that before it cost anything, which is the first time this week
that substitution was spotted in advance rather than after. Tracing WHY rev was
ambiguous then found the real defect: `expect_rev` is checked against `rev`, so tag
writes sat outside optimistic-concurrency control entirely and two clients could
clobber each other silently. The reporting bug was the visible edge of a correctness
bug.

Make the answer space match the shape of the claim (fleet-converged,
2026-08-02, four instances in one day — orch's MO-3000 the clearest): a prompt
offering exactly `done` or `todo` about a STANDING-ROLE card forces a false
statement either way, and the less-wrong pick (`todo`) recycles the card into
the rot queue forever — rot detection that cannot express "this should not
exist" manufactures permanent rot. Before shipping any N-cell question, ask
which cell a partial, contradictory, or mis-shaped reading lands in; if the
honest answer is "none", the question is missing a cell, and the operator
following instructions literally will never reach the truthful exit.

**A control that changes the STRING an assertion greps for cannot tell you the
assertion works** (AF-172, 2026-08-23). Two mutations were run against the same
cluster-rank cells. One renamed the "AREA CLUSTERS" header; the cell went red, and that
was cited as proof the cell discriminates. It is not proof. Any grep-based assertion
goes red when you change the string it greps, INCLUDING one that is mis-specified and
fails against every implementation. That cell's first draft was exactly that: it counted
spaces against a `%-16s` field, came up one short, and failed on the CORRECT
implementation. A wording mutation would have turned that red too and looked like a
working control. The load-bearing mutation was the other one, ranking solved clusters
instead of open ones, because it changes what the tool CONCLUDES rather than how it
phrases it, so only an assertion reading the logic can catch it. Mutate the arithmetic or
the predicate; a text mutation measures coupling, not correctness.

**And confirm the mutation LANDED before reading the suite's colour.** The same review
produced the inverse failure minutes later: an attempt to zero the open count used a
regex that matched NOTHING, the harness printed `count expr found: False`, and the
still-green suite was read as evidence about the TESTS rather than evidence that the
mutation had never applied. One sentence away from filing "this feature has no
discriminating coverage" against cells carrying a proper positive AND negative. An
unapplied mutation and a test that cannot fail produce the identical green, and the
mutation is the cheaper of the two to check.

**A TEST THAT MINTS ITS OWN INPUT PINS ITSELF, NOT THE PRODUCER** (AF-268,
2026-08-27). The auto-pickup prompt and the guard that parses it lived in two files.
The parser held a hand-copied literal of the prompt's wording, and BOTH sides carried a
comment saying to change them together. `03ed2b6c` shortened the prompt for token cost
— a correct change — and the parser was not touched, so `pickup_card_id` returned None
for every real pickup and the AMUX-3052 stale-pickup guard voided nothing for 17 hours.
The warning comment was three lines above the edit, inside the diff the author was
looking at.

Every one of the guard's tests stayed green, because each one built its input by
hand-writing the same retired wording. They were not testing the producer; they were
testing that the parser agreed with a copy of the parser. The evidence is precise:
under a mutant restoring the old wording, the new round-trip test fails and the other
six pickup tests still pass — which IS the state that shipped.

Three checks, in order of strength:
- Build the input with the PRODUCER, not a literal. If the fixture is a string you
  typed, you have pinned your own typing.
- Delete the second copy. The parser also held a TAIL literal purely to find where the
  id ended; a card id has no spaces, so the first token after the anchor is the id, and
  the tail was one more thing that could drift alone. Fewer literals, fewer seams.
- Put the pin in the PRODUCER's file. A test that fails in the file you are editing is
  a mechanism; one that fails in a file you have never opened is a hope.

**And zero can be the healthy reading of a dead instrument.** The guard's only
observable was a void event, and a guard that never runs emits exactly what a fleet
with no stale pickups emits. What made it measurable was the discontinuity: 126 voids
over 12 days, the last 31 minutes before the reword, then 0 across 100 deliveries.
Rule 4's demand for an accompaniment applies to guards as much as to reports — a
detector whose silence is indistinguishable from success needs a rate, a last-fired
timestamp, or a signal on the path it declines to act on.

## 8. Are you deciding something that is the human's to decide?

Getting out of the model's way includes getting out of the user's way.

Twenty-one cards sat in `doing` with no session. The obvious automated fix was to
reassign or discard them. All twenty-one were `owner_type=human`: the user's own
in-flight work. Reassigning or closing them would have been an agent deciding a
person's work was abandoned.

**Check:** whose data is this, and would they recognise the change as theirs? Report
and recommend; do not sweep. Never bulk-delete user content as a side effect of a
refactor.

---

## Applying this

Before building, answer 1, 2, 3 and 5. Before claiming done, answer 4, 6 and 7.
Before touching anything you did not create, answer 8.

If a proposed feature fails one of these, that is not automatically a veto. It is a
signal that the design is carrying a cost you should name out loud in the commit
message, so the next person can weigh it.

**The compounding question, above all of them:** when the next model is meaningfully
better than this one, does this feature get better with it, or does it become the
ceiling?

---

# Known deviations — tracked, not re-discovered

Live places where amux still fights the ethos, found in the 2026-07-30 audit
("any capability that acts as a stop-gap on top of a weaker model needs to not
exist"). Each has a STATUS and an EXIT CONDITION. When you touch one of these
systems, move it toward its exit, and update this section when a row changes.

## D1 — Terminal-scraping as the control plane
14 of 40 compiled regexes parsed Claude Code's rendered UI to infer state. None
improve with a better model; all break when a string changes (the API-error
detector was fixed twice in one day).
**Status: mitigated.** `POST /api/sessions/<n>/report` + global Stop /
UserPromptSubmit hooks let the harness report its own state; a fresh report
outranks the scrape in the status loop. Scrapers remain the FALLBACK (crashes,
subagents, hookless providers).
**Exit:** every consumer reads reported state; scrapers demoted to a
liveness check only.

## D2 — amux answering prompts on the model's behalf
`_RATE_LIMIT_PROMPTS` matches the rate-limit menu and presses 1 fleet-wide — a
scraper pretending to be a user.
**Status: mitigated.** The POLICY is now the human's, set once: pref
`rate_limit_action` = `wait` (default, today's behavior) or `off` (detect but
leave the menu for a human). The scrape stays only because Claude Code exposes
this state nowhere else.
2026-08-19: a SECOND selector joined it by Ethan's explicit instruction — the
resume-mode prompt ("Resume from summary / full session / don't ask again"),
answered with the digit 1 (`resume_mode_action`, default `summary`, `off` to
leave it). He also set the boundary: these two are the ONLY prompts amux
auto-answers (yolo's own flags aside) — do not generalize the mechanism.
**Exit:** Claude Code exposes rate-limit state via hook/JSON; delete the
pattern table.

## D3 — Hardcoded weak-model helpers
Six call sites pinned `haiku` for helper one-shots. Pinning a weak model is a
bet that cannot improve; the 12–15k-token label call it produced forced a
throttle, which is why most commands never reached the board.
**Status: fixed.** One knob: `AMUX_HELPER_MODEL` / `AMUX_HELPER_MODEL_API` in
`~/.amux/server.env`; all sites read it. (The audit said 5 sites; fixing it
found a 6th.)
**Exit condition met** — the helper tier moves with one line of config.

## D4 — Caps on what the model may see
`_OBS_EVAL_CAP`/`_OBS_STATE_CAP` were code constants — context-scarcity policy
hardcoded where it silently becomes the ceiling as windows grow.
**Status: fixed.** `AMUX_OBS_EVAL_CAP` / `AMUX_OBS_STATE_CAP` in server.env;
defaults unchanged.
**Exit:** revisit defaults upward as model windows grow; policy now lives in
config where that takes one line.

## D5 — Auto-compact at a hardcoded 50%
amux decided WHEN the model should summarize — preempting a judgment models
increasingly make better, with a lossy operation.
**Status: mitigated.** Pref `auto_compact_threshold` (default 50 = today's
behavior; 0 disables the proactive path while keeping resume-dialog handling).
**Exit:** models manage their own context; amux only surfaces the number.

## D6 — Two terminal backends to keep in step
tmux and herdr (#79/#80, 2026-08-06) both host sessions, so every future change
to session lifecycle must be made twice, and the herdr half cannot be verified
by anyone without herdr installed — its tests mock `subprocess` and CI proves
only the backend-SELECTION logic. Accepted anyway: the seam is one resolver
(`_session_backend`), the change is additive with tmux paths untouched, and
structured agent lifecycle state is what D1 names as its own exit.
**Status: accepted with a named cost.** The README says plainly that CI does not
cover the herdr path, so a green build is never mistaken for an integration test.
**Exit:** when the AgentRuntime seam (#47/#48) lands, backends resolve through
it rather than through per-call-site branches — one dispatch point instead of
two families of code paths.

The pattern under all five: amux WATCHED the model and acted on inference. The
durable inverse — the model reporting its own state through a real interface —
is D1's report endpoint; prefer extending it over adding any new scraper.

**D1 exit, extended (2026-08-02):** the board status-request flow
(`POST /api/board/<id>/status-request` -> session authors a status-update onto
the card) is the report endpoint applied to WORK STATUS, not just liveness. The
board is the source of truth because activity flows to it from the session's own
model; amux never scrapes a terminal or summarizes with a pinned helper to fill
a card. It compounds: better model -> better status, no harness change.

**D1 exit, applied to the scan (2026-08-03):** the rate-limit/status loop
captured every lane's tmux pane every ~13s, *including lanes whose hooks had
just reported their state*. That is the poll the report endpoint exists to
replace, running anyway. A lane with live hooks is now pane-captured at most
once per 60s (`AMUX_SCAN_DEMOTE_S`), with hookless lanes — gemini, codex, or a
lane whose hooks broke — silently restored to full-rate scraping, because for
them the scraper is the only voice.

Two things that pass a code read and fail the ethos, both caught by measuring
the shipped path rather than reasoning about it:

- **The first gate tested the wrong property.** It demoted while a report was
  *fresh* (25s). But freshness is the right test for TRUSTING a report's
  contents, not for licensing the demotion: an idle lane reports once on Stop
  and is then silent for hours, so a freshness gate demoted the 25 seconds
  after a turn and full-rate scanned the entire parked period — the inverse of
  the intent, and the majority of the fleet. The property that licenses
  demotion is *this lane's harness reports at all*. And an `idle` report does
  not decay: the only exit from idle is a prompt, and every prompt fires
  UserPromptSubmit, so idle gets a 24h window while active/waiting keep 30 min.
- **In-memory state is fiction.** The report table lived only in memory, and
  this process re-execs on every save of `amux-server.py`. A restart would have
  dropped all 41 lanes back to full-rate capture until each happened to take a
  turn — i.e. removed the optimisation most of the time, invisibly. Reports are
  now persisted and hydrated at boot.

`GET /api/debug/scan` exists because of rule 4: a skip that leaves no trace is
indistinguishable from a scan that found nothing. When demotion eventually
hides a transition, whoever looks must SEE which lanes were skipped and on what
gate, not infer it from silence.

---

# Decisions taken, with the reasoning — so they are not re-litigated

## Board state changes are delivered at turn boundaries, NOT via a global pub-sub (2026-08-03)

Ethan asked, looking at cards stuck reading "captured" instead of decomposed:
"because board issue statuses are so critical across amux maybe we have events
and listen in on those events so everything can be updated/changed in real
time." The answer is yes to events, no to a global bus. Recorded here because
it is the kind of decision that looks obviously right the second time someone
proposes it.

**A session cannot consume an event faster than its next turn boundary.** A
running agent is not an event loop; it is mid-turn, and anything delivered to
it arrives when it next reads its input. So sub-turn delivery latency buys
literally nothing at the consumer, while a bus costs a delivery guarantee, an
ordering guarantee, a replay story, and a new class of "the listener was
wedged" failure. The correct grain is the turn, and `_steer_enqueue` already
delivers at exactly that grain.

**What was actually missing was not transport but triggers.** The board write
already happens; nothing was hanging a consequence off it. So the fix is
per-case conversions — a card closing now nudges the lanes whose dependents it
just freed; 2+ capture shells now provoke one decompose ask — each one a
specific event with a named consumer and a dedupe key, rather than a firehose
every lane must filter. Each conversion deletes a poll.

**A global bus would also have to re-implement tag isolation, and would get it
wrong.** Sessions see only same-tag lanes (untagged sees itself). A broadcast
bus is scope-blind by construction, so isolation would have to be re-derived at
every subscriber — the exact shape of leak that is easy to ship and hard to
notice.

**What to do instead, when this comes up again:** find the write that already
happens, and hang the consequence off it, addressed to a named consumer, with a
durable dedupe key. If you cannot name the consumer, you do not have an event —
you have a log.

## A CRDT is the wrong trade for the board, and the right one for text (2026-08-26)

Recorded because the opposite is an easy conclusion to reach from the outside, and
because the person best placed to argue FOR it argued against it.

@tsukimiya (external contributor) had starred yjs as the "don't reinvent this wheel"
option for collaborative editing, and volunteered the limit unprompted: *"For a
field-level board, a CRDT is probably the wrong trade and you'd lose the audit
trail."*

That is right, and the audit trail is the reason. The board's load-bearing property is
not that concurrent writes merge — it is that **every mutation is attributed and
gated**. `force` names the party holding the judgment, `reviewer != owner` is enforced,
the desc-clobber guard tests authorship and survival, and `expect_rev` is what makes a
lost update loud. A CRDT converges silently by construction; converging silently is the
one behaviour this board must not have. `rev` is not a poor man's CRDT here, it is a
concurrency check whose FAILURE is the product.

**When to revisit, and it is a real case:** genuine concurrent TEXT editing — shared
instructions, a steer queue two lanes both edit, a document with no field boundaries.
There the merge is the product and there is no per-field authorship question to lose.
That is the case to reach for yjs, and it is not the board.

The general form, worth more than the instance: **ask what the mechanism's FAILURE mode
is for, before replacing it with one that cannot fail.** An optimistic-concurrency
conflict is not friction to be engineered away; it is the moment the system tells you
two parties disagreed, which is exactly what an attributed ledger exists to surface.
