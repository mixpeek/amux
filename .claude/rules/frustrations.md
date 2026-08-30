# Log amux-level friction to `frustrations.md`

You run *inside* amux. When amux itself gets in your way — a command that lies, a
notice that misattributes, a gate you cannot satisfy honestly, a probe that cannot
express the answer, a nudge that fires forever — **append an entry to
`frustrations.md` at the repo root.**

**"The repo root" means the checkout that can actually PUSH.** On a machine with
more than one clone this is not a pedantic distinction, it is the whole value of
the file: on 2026-08-17 four entries were appended to a checkout that was ~1000
commits behind with unpushed local commits and an hourly sync job that had failed
80+ runs. That copy held 25 entries; the real one held 124. The appends
SUCCEEDED — no error, nothing to notice — and reached nobody. The argument this
file exists to make is that one frustration is a complaint and a cluster is an
argument; a cluster only forms in the file everyone reads.

So before appending, confirm the checkout is not stranded:

```bash
git rev-list --count origin/main..HEAD   # unpushed commits here
git rev-list --count HEAD..origin/main   # how far behind
```

If BOTH are non-zero the checkout has diverged and cannot fast-forward. That is a
REPORT, not a verdict, and the difference cost a review pass on 2026-08-27
(AF-272): the two counts cannot tell a stranded clone from the canonical one.
A checkout that MERGES A PR ON GITHUB goes "behind" by that merge commit
immediately, so the canonical repo — the one whose 255 unpushed commits every
lane is working in — reads as stranded by exactly this test, and the honest
reading is the opposite one.

Ask what put you behind. Commits that are peers' work landing on origin while
yours sit unpushed means stranded, and the entry belongs in a current clone.
Commits that are YOUR OWN merges of PRs you just reviewed mean the opposite:
this is the checkout everything flows through. When the answer is not obvious,
`git log --oneline HEAD..origin/main` names them, and a name settles it where a
count cannot. The
SessionStart freshness hook now says this out loud when it applies, because a
rule that only asks you to remember is the kind ethos rule 6 warns about.

And if this file itself has already diverged both ways — your local appends AND
origin-only entries a peer landed (AMUX-3367, seen live on the Mixpeek
FRUSTRATIONS.md) — do NOT reach for either single-arm git remedy: `git add` +
commit REVERTS the peer's entries, `git checkout origin/main -- <file>` DELETES
yours, and the direction test cannot separate them because BOTH are true at once.
UNION-MERGE, with the ARCHIVE CHECK (CD-78 corrected AMUX-3367): `git checkout
origin/main -- <file>` to take origin's version, then re-append ONLY YOUR OWN
entries — and before re-appending anything that merely looks lost, grep the
file's companion archive (e.g. FRUSTRATIONS_ARCHIVE.md) for it. Present there
means the deletion was a deliberate archive move and re-appending it
manufactures a duplicate; creative-dna measured 15 of 15 "lost" entries as
archive moves, with the restore/remove cycle run three times on origin before
anyone noticed. Only an entry absent from BOTH files is lost work. The general
form: a set-difference over one file cannot see a MOVE and reports it as a
deletion every time. The idle commit-nudge prints this directive by name when a
dirty append-only file is in the set, but the operation is yours to run.

This is not a diary. It is the input to deciding what to fix next, so it has to be
greppable and it has to be honest about cost.

## When to log

Log it when amux cost you something you would not have paid with a better harness:

- a command reported success and did nothing, or reported the wrong thing
- an instrument could not express the failure you were looking at
- a gate could not be satisfied truthfully, so the honest move was to stop
- a notice/nudge sent you at the wrong card, the wrong session, or fired forever
- you had to leave the sanctioned path (raw curl, manual edit) to get work done
- two components disagreed about the same fact

Do **not** log: your own mistakes with no amux involvement, one-off environment
noise, or anything you fixed in the same breath with no cost to anyone. A frustration
is friction the NEXT session will also hit.

## How to log it

Append at the bottom. Never rewrite someone else's entry — add a new one that
supersedes it and say so. One entry per distinct friction; if it has two causes it is
two entries.

Use the field block exactly as written in `frustrations.md`'s own header — the fields
are fixed so `grep '^STATUS: open'` and `grep '^AREA: cli'` work. If you invent a
field, nobody's grep finds it.

**Link the card.** A frustration without a `CARD:` is a complaint; with one it is a
work item someone can pick up. If there is no card yet, file one.

**Record the COST in what it actually cost** — minutes, a wrong conclusion shipped, a
push blocked, a card closed that should not have been. "Annoying" is not a cost.

## Retiring an entry — the three dispositions

An entry leaves `frustrations.md` only when the session that ORIGINATED it says the
friction is gone. Use `scripts/frustrations-archive.py`, which moves it to
`frustrations-archive.md`, stamps who signed off, and carries the SYMPTOM and COST onto
the card (AF-38's rule — the card is where someone hitting it again looks).

There are **three** states, not two, and the third had no honest exit until AF-243:

- **Right, and fixed** → archive with `VALIDATED: <who> | <evidence>`.
- **Right, and still live** → do NOT touch the entry. **Reopen the CARD.** A card reading
  `done` over a live friction is the disagreement `frustrations.ledger_agrees_with_board`
  flags, and the honest correction is the card.
- **Wrong** → archive with `--superseded`, which stamps `SUPERSEDED:` instead. Archiving a
  wrong entry as validated files a FALSE MECHANISM as history, and reopening its card
  says a friction is live that was never real. Both available moves lie about it, which
  is ethos rule 3 — a constraint with no truthful path through. The text is still kept,
  as a DEAD HYPOTHESIS, so nobody re-derives it.

**A VALIDATION IS A CLAIM ABOUT THE ENTRY'S TEXT, NOT ABOUT THE SUBSYSTEM** (amux,
2026-08-26). They validated `AMUX-2777`'s narrow claim — "cannot tell MY broken change
from a PEER's", genuinely closed by lint-blame partitioning offenders — while that same
entry's COST line describes the structural defect that is still open as `AF-182`. Both
verdicts are correct. The two come apart exactly when a subsystem carries two entries at
different depths, and the shallower one can be honestly retired while the deeper one
stays live. So read an archived entry as *this sentence stopped being true*, never as
*this area is done* — and when you retire the shallow one, say beside it that the deep
one is open, or the next reader finds a "fixed" entry describing a live bug.

The same edge exists on the CARD side, and it is worth stating because neither half
would make you check the other. **A REVIEW is a claim about what was BUILT, not about
the card's title** (amux, 2026-08-26). They signed off `AF-182` on a fix that did
exactly what it claimed — the notice stopped saying a peer's error was yours — while
the card's headline, "blocks your commit over a peer's uncommitted file", stayed true
and recurred after the fix. The card held two units of work and no status was a true
statement about it. Where an entry can be validated at the wrong DEPTH, a card can be
reviewed at the wrong SCOPE; the remedy for both is to name which clause you tested.

**Ask the author; do not infer from the card.** Card status is not evidence — this drain
found entries whose card read `done` because the FEATURE had been deleted, because the
lesson was encoded in a replacement, and because the card "closed on something else"
(three independent confirmations of that last shape). None of those are readable from a
status field.

**Some entries have no validatable author, and that is not yours to resolve.** A session
that was a subagent, a one-off `claude` invocation, or a lane on another machine cannot
sign anything off. Do not retire those on your own judgement — that is deciding another
party's work is finished (ethos rule 8). Surface them and let the owner decide.

## Then act on it

Logging is not the fix. If the friction is cheap to fix and it is yours to fix, fix
it and set `STATUS: fixed` with the sha. If it belongs to another session's
subsystem, file the card and route it to them. The file exists so the pattern across
entries becomes visible — three entries with `AREA: attribution` is an argument that
one thing needs rebuilding, which no single entry makes on its own.
