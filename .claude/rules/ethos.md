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

The three that generalise past "be careful": **name the target before you search for
it** (which of the 28 selects? which of the two branches?), **bound a positional window
on the CODE, not on however much prose precedes it**, and **when you built the broken
fixture yourself, verify it is actually broken** — "I broke it" is a claim, not a
premise, and it fails silently because everything looks like it ran.

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
