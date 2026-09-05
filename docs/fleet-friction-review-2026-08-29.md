# Fleet friction review, 2026-08-29

What the last 1,000 fleet messages and the two frustration ledgers say about where
the next 1,000 sessions will lose time, and where to put the fix.

## Sources

| Source | Size | Window |
|---|---|---|
| `cmd_history` (SQLite, direct) | 1,000 rows | 2026-08-28 07:28 → 2026-08-29 17:56 (34.5h) |
| Ethan's own prompts inside that | 163 | same |
| Session-to-session sends | 254 | same |
| `board_drive` nudges | 496 | same |
| `frustrations.md` | 83 entries, 42 open | 2026-08-08 → 2026-08-28 |
| `frustrations-archive.md` | 60 entries | earlier |
| `/api/board` live | 1,978 cards | all time |

The 34-hour window is narrow, so every claim below is cross-checked against the
ledgers, which cover three weeks.

---

## Theme 1: the board accumulates, and nothing makes it discriminate

**Measured.** 1,978 open cards. 869 `backlog`, 445 `needsyou`, 321 `todo`, 93
`review`, 73 `blocked`, against 48 `done` and 33 `verified`. Age by status:

| status | n | median age | >7 days old |
|---|---|---|---|
| `todo` | 321 | **28.8d** | 282 (88%) |
| `review` | 93 | 26.7d | 74 (80%) |
| `blocked` | 73 | 19.0d | 69 (95%) |
| `backlog` | 869 | 16.2d | 603 (69%) |
| `needsyou` | 445 | 15.1d | 316 (71%) |

`todo` is the dispatch queue. Its median card is a month old. 1,708 of 1,978 cards
are agent-owned; `type: code` is 1,244 of them.

**Mechanism.** Every status is a place a card can rest indefinitely. No status has a
TTL, a WIP limit, or a forced disposition. Ethan named it on 08-29 10:25: "some
workers have an infinite # of growing backlogs and todo then they go idle." The
ethos file already has the rule (rule 5: "If it becomes a log, it needed to split,
not append"). The board is the thing violating it.

**Where to fix.** `crates/amux-server/src/api/board*` plus a new runtime job.

1. **Per-lane WIP limit on `todo`.** A lane may hold N (start at 5) cards in `todo`.
   Card N+1 is refused with the list of what to close first. This is the only change
   that makes the queue mean something.
2. **TTL with forced disposition.** A card 21 days in `todo` or `review` stops being
   dispatchable and becomes a single decision card for its owner: re-commit,
   re-scope, or archive. Three buttons, one card, not 282 nudges.
3. **`blocked` requires a `depends_on` or a `--trigger`.** 95% of `blocked` cards
   are over a week old, which means nobody is watching for the unblock. A `blocked`
   card with no named condition is a `backlog` card with better PR.

---

## Theme 2: `needsyou` is the cheap escape hatch, so the real asks are buried

**Measured.** 445 cards in `needsyou`, median 15 days, oldest 58 days. Classified by
title and description:

- 24% decision-shaped ("approve", "which", "your call")
- 13% access/credential-shaped
- 13% verification-shaped
- **51% (227) match none of these.** Their titles are engineering work: "Compute
  Utilization Audit", "Fix Namespace Pollution", "Batch Workers Blocked", "[studio]
  Styleguide remediation: migrate ~4,469 raw palette-shade usages to semantic
  tokens".

**Mechanism.** `needsyou` is the only status that costs a worker nothing and stops
the nudge. So it collects everything a worker decided to stop doing, and the twenty
or so items that genuinely need Ethan are indistinguishable inside 445 rows that
mostly do not.

**Where to fix.** Board status gate, `crates/amux-server/src/api/board*`.

Make `needsyou` a typed ask. Moving a card there requires:
- `--ask decision|access|credential|external|judgment`
- one sentence stating the question
- one sentence stating what unblocks it

Untyped move is refused with the four types printed. Then give Ethan one view:
`needsyou` sorted by age × blast radius, capped at 10, everything else invisible
until those 10 clear. A queue no human can drain is the same as no queue.

---

## Theme 3: nudging is the fleet's dominant channel, and the loop has no negative feedback

**Measured.** 496 of the last 1,000 messages (50%) are `board_drive` nudges. That is
14.4/hour fleet-wide, 639,992 characters (~160k tokens) pushed into worker contexts
in 34 hours. Per lane:

| lane | nudges | cadence | share of that lane's inbox |
|---|---|---|---|
| `ts-gke` | 28 | every 74 min | **84%** |
| `mvs-infra` | 76 | every 27 min | 71% |
| `backend` | 69 | every 30 min | 67% |
| `byo-ray` | 44 | every 47 min | 64% |
| `mixpeek-cicd` | 35 | every 59 min | 61% |

**Mechanism, and it is deliberate.** `board_drive.rs:191`,
`idle_backlog_drain_cooldown_s()` scales nudge cadence *up* with backlog size: base
2h, halving roughly every 25 cards, floor 20 minutes. The reasoning in the comment
is sound ("a lane idle on 200 un-worked cards should be re-nudged far more often
than one idle on 5"). The measured outcome is that the lanes with the largest
backlogs sit permanently at the 20-minute floor, their backlogs do not shrink
(`todo` median 28.8 days), and two thirds of their inbox becomes nudge text.

Nudge frequency is a function of backlog size. Backlog size is not a function of
nudge frequency. There is no term in the loop that closes it, so it saturates at the
floor and stays there.

The per-card and per-lane cooldowns already in that file (`ADVANCE_COOLDOWN_S`,
the 3-per-24h advance budget, `BACKLOG_TRIAGE_COOLDOWN_S`) are real and are not the
problem. Fleet-wide volume is unbounded because card count is unbounded.

**Where to fix.** `crates/amux-server/src/runtime_jobs/board_drive.rs`.

1. **Cap unheeded repeats.** Track nudges-since-last-card-movement per lane. After
   3, stop nudging the lane and file one escalation card naming the queue. A nudge
   that has fired 76 times without moving a card is evidence about the queue, not a
   message to the worker.
2. **Invert the scaling.** Cadence should scale with *card movement*, not backlog
   size. A lane that closed something yesterday is worth nudging. A lane sitting on
   244 cards of 30-day-old `todo` needs the WIP limit from Theme 1, and no nudge is
   a substitute for it.
3. **Budget the bytes.** Nudges average 1,290 characters. Cap total nudge tokens per
   lane per day and log the overflow, so the cost is visible where it is paid.

---

## Theme 4: verification is something Ethan has to demand, every single time

**Measured.** 82 verification mentions across 254 session-to-session messages. In
Ethan's own 163 prompts, verbatim:

- "make sure u verify test etc"
- "did you verify every single thing we did in the integration md against their studio ui and verify it all e2e"
- "verification should be verified visually (create and use in the studio as well as api) using actual data that mirrors their use case"
- "make sure everything is chaos tested and verified in their ns using their videos"
- "we should have e2e surface areas for whenever we introduce new capability in retriever pipelines"
- "make sure u rview the image and post for clarity when its done this shit is cut off not formatted right"
- "verify via browser when they're done to ensure everything renders properly"

**Mechanism.** The four-part `verified` gate is already written in
`~/.claude/CLAUDE.md`. It is advisory prose, and the board accepts `done` with no
evidence, so `done` is where work stops. The rule and the mechanism disagree, and
the mechanism wins. Board state confirms it: 48 `done` against 33 `verified`.

Every one of Ethan's demands above also names *how* to verify that specific surface,
which is knowledge the worker should not need him to supply.

**Where to fix.** Two places.

1. **`crates/amux-server/src/api/board*`:** `done` requires a non-empty evidence
   field. Command run, or URL exercised, or screenshot path. Prose like "implemented"
   is refused. This is the same shape as the typed `needsyou` in Theme 2.
2. **A `VERIFY.md` per repo surface**, named in that repo's `CLAUDE.md`. One heading
   per surface, and under it the literal command or UI path that constitutes proof:
   Studio UI flows, the retriever e2e battery, the docs render check, the browser
   screenshot check. Ethan has now dictated these individually at least seven times
   in 34 hours. Written down once, they reach every lane by default, which is ethos
   rule 1.

---

## Theme 5: access and credential gaps surface mid-task, never before

**Measured.** 61 `needsyou` cards are access-shaped. From the session traffic:

- `ai-video-editor` → `general-canvas-apps`, 08-29 17:21: "Same result on my end,
  cfat_ token has no purge scope. Both of us are blocked by the same CF access gap.
  Escalating to Ethan now."
- `mixpeek-cicd` → `backend`, 08-29 05:18: "One command with your staging SSH would
  settle MC-1424's last blocker."
- `tubescience`, three separate 403 pastes from Ethan on 08-28, plus "u shouldnt need
  to be an admin to see usage".

Two lanes independently burned time discovering the same missing Cloudflare scope.

**Mechanism.** No lane knows what it can reach until it tries. `docs/credentials.md`
is an inventory of where values live, and it does not answer "does my token carry
the scope this task needs".

**Where to fix.**

1. **A capability manifest per worker**, served from the session payload: which
   credentials it holds and which scopes each carries. A worker reads it before
   starting, not after a 403.
2. **A preflight verb**, `amux caps check <capability>`, that probes the scope and
   returns a real answer. The Cloudflare purge gap costs one lane 20 minutes once,
   instead of two lanes an afternoon each.
3. **One escalation card per gap, fleet-wide**, deduped by capability rather than by
   lane. Both lanes above filed separately.

---

## Theme 6: instruments that lie — the single largest cluster in the ledger

**Measured.** 41 of 83 entries in `frustrations.md` are `AREA: instruments`, 19 of
them still open. Another 24 in the archive. No other area is close (attribution 12,
cli 7, board 5). Open examples:

- "A latency card named an innocent endpoint with a verdict that was confidently backwards" (AMUX-3772)
- "A worker whose pane died at launch reports `running: true` / `idle`" (AMUX-2644, blocks)
- "The rust request log recorded a ~15-second restart choreography as a 76ms request" (AR-111)
- "The disk ranker cannot rank a file, so it could never have named the 1.8 GB one" (AEAB-42)
- "'The tests pass' is load-dependent on this box, so a green suite is a weaker claim than it reads" (AMUX-3853)
- "A probe read a hook file that git never executes, and a correct measurement certified the wrong conclusion" (AMUX-2841)

Same shape every time: the measurement returns a clean value, the value is wrong,
and nothing beside it says the probe could not have produced a right one.

**Mechanism.** Ethos rule 4 already states the remedy: "Any output that can read zero
or empty must publish, in the same payload, whether the measurement ran." It is a
rule that asks people to remember, so half the ledger is people not remembering.

**Where to fix.** Make it mechanical rather than remembered.

1. **A response contract.** Any diagnostic endpoint returning a count, a verdict, or
   a ranking also returns `measured: bool` and `n_considered: int`. A zero with
   `measured: false` reads correctly at a glance; a bare zero never will.
2. **A test that fails on omission.** `tests/` already pins dashboard assets by the
   same logic. Add one that enumerates diagnostic routes from `/api/debug/routes`
   and fails when a new one ships without the two fields. Ethos rule 7: the check has
   to be able to fail.

---

## Theme 7: one checkout, N lanes, and git has one index

**Measured.** 9 open `attribution` entries plus `shared-checkout`:

- "A peer's `git add` swept my uncommitted migration into their commit and it applied to the live DB" (AMUX-2647)
- "A shared checkout has ONE git index, so a peer's `git commit` shipped MY staged work under THEIR message" (DESKT-22, blocks)
- "A peer's `install` shipped my uncommitted, unverified WIP straight to the live server" (AMUX-2637, blocks)
- "A peer's mid-edit fails MY test run, and a rerun is the only way to tell" (AF-182)
- "A peer's half-saved file blocks an unrelated commit's gate, third sighting in one day" (AMUX-1315)
- "The staged-guard was silent on the commit that swept a peer's work, and warned on the clean one" (AC-297, blocks)

Also live: `AMUX-3853`, where the auto-builder rewriting the shared binary produced 8
test failures in a module nobody touched, and 15/15 green on rerun.

**Mechanism.** Every one of these is the same structural fact. N agents share one
working tree and one git index. The staged-guard is a detector bolted on top of that
fact, and the ledger shows it repeatedly firing on the wrong commit.

**Where to fix.** This is the largest single class and the only one with a structural
remedy rather than a better detector.

1. **Per-lane git worktree.** Each lane gets `git worktree add`, its own index, its
   own HEAD. The entire class of "a peer's stage/commit/edit hit my work" stops
   existing. The Agent tool already has `isolation: "worktree"`; the fleet lanes do
   not use it.
2. **Until then, pathspec-scoped commits only.** No lane runs bare `git add -A` or
   `git commit -a`. A commit names the files that lane edited. This is a
   `.claude/settings.json` PreToolUse hook on `Bash` matching `git add` and `git
   commit`, and it is worth shipping this week regardless of item 1.
3. `CARGO_TARGET_DIR` is already shared deliberately. Keep it, and keep
   `scripts/test-contended.sh` as the sanctioned runner, since it is the thing that
   tells the two kinds of red apart.

---

## Theme 8: workers ask for authority they already have

**Measured.** In 34 hours Ethan wrote, to five different lanes:

- "you do it you have my authority" (`mixpeek-cicd`)
- "do whatever you think is best you dont need me. for BACKE-3654" (`backend`)
- "you should be able to execute anything you think is best based on signals you found" (`amux-frustrations`)
- "of the list of outstanding things, which are genuinely blocked by me? of the ones that are not, push them along" (`tubescience`)
- "push all the relevant workers to continue so theyre not blcokers" (`tubescience`)
- "or push whoever is blcoking" (`backend`)
- "you do it" (`byo-ray`)

Plus eleven bare "continue" messages.

The counterweight, from the same window, 08-29 10:41: "i thought we said we wouldnt
introduce any new primitives? why do we need to do a learned fusion dedicated
endpoint? make sure our vision/md/mclaude file rules ensure any new endpoints have
explicit human approval."

**Mechanism.** No lane has a written boundary, so each one guesses, and the guess is
conservative. The result is 445 `needsyou` cards and Ethan hand-granting authority
several times a day, while the one thing he actually wanted gated (a new API
primitive) shipped without asking.

**Where to fix.** `~/.claude/CLAUDE.md`, one short standing-authority section. Inside
the boundary, proceed and report. The boundary is a named list, and it is short:

- spending money or provisioning paid infrastructure
- sending anything to a person outside the company
- deleting or overwriting customer or production data
- **adding a new API primitive, endpoint, or resource type** (his 08-29 10:41 ask)
- `git push` to main when foreign commits are present
- anything irreversible whose blast radius exceeds one lane

Everything else: act, then report. This converts a per-task negotiation into a
one-time rule, and it is the change that most directly reduces Ethan's per-day
message count.

---

## Theme 9: Ethan is the fleet's status poller

**Measured.** "where are we at with all of the items from the ingestion MD?" sent
verbatim three times in twelve hours (08-28 23:32, 08-29 10:00, 08-29 10:14). Also
"what's the status of the ingestion work?", "whats the status", "is the byo-ray
worker making progress?", "where is the board export button".

**Mechanism.** Work is planned in markdown documents
(`2026-08-25-INGESTION-PIPELINE-PLAN.md`, the byo-ray folder) and tracked on a board
that does not know those documents exist. Neither surface can answer "how far
through the plan are we", so the human asks, repeatedly.

**Where to fix.** Bind plan documents to cards. A card gets `plan_ref: <path>#<heading>`;
the worker page renders progress against the document's own headings. Then a
scheduled digest pushes that to Ethan rather than him pulling it three times a night.
The board export he asked for twice on 08-29 (10:00 and 12:07, then "where is the
board export button" at 14:55) is the same need showing up as a feature request; that
one appears to have shipped in `d714512e` / `a8b5bffe`.

---

## Theme 10: the message bus cannot say whether a message landed

**Measured.** Of the 1,000 rows, `submit_verdict` is `confirmed` on 366, `retried` on
5, and **null on 629**. Ethan, twice in one day:

- 08-29 12:06: "why are you sending unsubmitted text? figure out why and fix. look at `@amux-cloud` as an example"
- 08-29 14:04: "workers are still receiving text in their chat that is unsubmitted... it shouldn't be the case if it's coming from another amux worker"

Related open ledger entries: "Ghost-rescue can only rescue the messages that happen
to carry a timestamp prefix" (AMUX-2629), and AC-354, where an `amux send` to a bare
REPL worker submitted the origin header as its own message and left the body sitting
unsubmitted.

He also pasted board evidence on 08-28 showing three of his own instructions landing
as cards MS-1224, MS-1226, MS-1229 and being marked **discarded**.

**Mechanism.** This is Theme 6 aimed at the message bus. 63% of traffic has no
verdict at all, so "did it land" is unanswerable for most messages, and the failure
mode Ethan is reporting is invisible in the data that would diagnose it.

**Where to fix.** Give every send a terminal verdict. `confirmed`, `retried`,
`failed`, or `unknown` explicitly recorded rather than left null, and surface the
`unknown` rate in `/api/debug/sse`-style diagnostics. A null is currently doing the
work of three different outcomes.

---

## Already fixed, noted so nobody re-files it

**Browser as a singleton.** Ethan hit this three times on 08-28 ("the brwoser shit
needs to be more intuitive", "multiple browsers shouldnt be an issue, but it should
clean up automatically afte ridle use", "make sure amux browser closes when the
worker is done with it"). `crates/amux-server/src/runtime_jobs/browser_reaper.rs`
now exists and its header quotes that incident directly, with per-profile idle
tracking and continuous-emptiness rather than age as the reap condition. Verify it
is reaping in practice before closing anything.

---

## What to change, in order

Ranked by time returned across the next 1,000 sessions.

| # | Change | Where | Theme |
|---|---|---|---|
| 1 | Per-lane git worktree; pathspec-only commits until then | `.claude/settings.json` PreToolUse hook, then lane provisioning | 7 |
| 2 | WIP limit on `todo` + 21-day forced disposition | `crates/amux-server/src/api/board*` | 1 |
| 3 | Typed `needsyou` (`--ask`), capped owner view | `crates/amux-server/src/api/board*` | 2 |
| 4 | Cap unheeded nudges at 3, escalate the queue instead | `runtime_jobs/board_drive.rs` | 3 |
| 5 | Standing-authority section with the named boundary list | `~/.claude/CLAUDE.md` | 8 |
| 6 | Evidence required on `done` | `crates/amux-server/src/api/board*` | 4 |
| 7 | `VERIFY.md` per repo, named in that repo's `CLAUDE.md` | each repo | 4 |
| 8 | `measured`/`n_considered` contract + a test that enforces it | `crates/amux-server/src/api/`, `tests/` | 6 |
| 9 | Capability manifest + `amux caps check` preflight | session payload, `amux` CLI | 5 |
| 10 | Terminal `submit_verdict` on every send | message bus | 10 |
| 11 | `plan_ref` binding cards to plan documents | board schema, worker page | 9 |

Items 2, 3 and 6 are one coherent change to board status gates and should ship
together. Item 1 is the largest single class in three weeks of ledger and has a
structural fix rather than a better detector.

## The pattern under all of it

Nine of these ten themes are the same shape. amux writes the correct rule in prose,
the mechanism does not enforce it, and the mechanism wins. The `verified` gate, ethos
rule 4, the `needsyou` semantics, the authority boundary, the frustration-logging
discipline: all documented, all bypassable at zero cost. Every fix above converts a
rule that asks a worker to remember into a gate that cannot be satisfied dishonestly.
That is ethos rule 6 applied to amux itself.
