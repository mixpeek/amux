#!/usr/bin/env python3
"""Move a VALIDATED frustrations.md entry into frustrations-archive.md.

Ethan, 2026-08-24: "its only verified when the originating session validates and
agrees its complete. once its complete delete from this md."

Deleting is the instruction; this is where the bytes go, and it exists because
`.claude/rules/frustrations.md` records what happens without it. A set-difference
over one file cannot see a MOVE and reports it as a deletion every time --
creative-dna measured 15 of 15 "lost" entries as archive moves, with the
restore/remove cycle run three times before anyone noticed. So the archive is not
sentiment: it is the thing that makes "was this lost or was it finished?"
answerable by a grep instead of by reading git history.

Every archived entry carries a VALIDATED line naming WHO signed it off and when.
The protocol's whole point is that the originating session is the only party who
can say an entry is done (AC-227: an entry marked fixed by somebody who was not
its author, over a card that had only half shipped), so an archive move with no
name on it would launder exactly the thing the protocol forbids.

Usage:
    scripts/frustrations-archive.py <line> <validated-by> <evidence...>
    scripts/frustrations-archive.py <line> <who> --superseded --evidence-stdin

`--superseded` is the THIRD disposition, for an entry whose MECHANISM was wrong
and which a later entry corrects. It stamps SUPERSEDED: instead of VALIDATED:,
because archiving a wrong entry as validated files a false mechanism as history
(AF-243).
    scripts/frustrations-archive.py <line> <validated-by> --evidence-stdin
    scripts/frustrations-archive.py <line> <validated-by> --evidence-file <path>

PREFER --evidence-stdin/--evidence-file whenever the evidence quotes code.
Inline text is evaluated by YOUR shell first, so backticks and $(...) in it are
EXECUTED before this script sees them (AMUX-1888).
    scripts/frustrations-archive.py --list
"""
import json
import time
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER = ROOT / "frustrations.md"
ARCHIVE = ROOT / "frustrations-archive.md"

ARCHIVE_HEADER = """# amux frustrations: archive

Entries retired from [`frustrations.md`](frustrations.md). An entry lands here only
when the session that ORIGINATED it said the friction is gone; the `VALIDATED:` line
names who said so and on what evidence.

This file exists so that "was this entry lost, or was it finished?" is a grep rather
than an archaeology exercise. A set-difference over the ledger alone cannot see a
MOVE and reports it as a deletion every time. Before restoring anything that looks
missing from `frustrations.md`, grep here first: present means it was retired on
purpose, and re-appending it manufactures a duplicate.

Nothing here is live. `frustrations.md` is the live file and the invariants
`frustrations.ledger_agrees_with_board` / `frustrations.cards_are_reachable` read
only that one.

---
"""


def parse(md):
    """Entry spans, keyed by the 1-based line of the `## ` heading.

    Same rule the Rust parser uses (crates/amux-server/src/invariants/checks.rs):
    entries start at a COLUMN-0 `## ` after the `---` that closes the header, so
    the header's deliberately-indented template cannot count itself.
    """
    lines = md.split("\n")
    start = next(i for i, l in enumerate(lines) if l.strip() == "---")
    heads = [i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")]
    out = {}
    for n, i in enumerate(heads):
        end = heads[n + 1] if n + 1 < len(heads) else len(lines)
        out[i + 1] = (i, end, lines[i][3:].strip())
    return lines, out


def field(block, key):
    """One `KEY:` field out of an entry block, continuation lines included.

    THE TERMINATOR ACCEPTS A HYPHEN, and it must (AF-264). It used to be
    `[A-Z_]+:`, so a field whose NAME contained a hyphen was not recognised as
    the start of the next field — and because the body pattern is non-greedy and
    needs the lookahead to stop, the match failed entirely and the field BEFORE
    it came back EMPTY.

    Measured the day it was written: an entry carrying `CARD: AF-242` followed by
    a `NOTE-CARD:` line reported "no CARD field", so the entry was archived with
    its symptom never reaching the card — the AF-38 guarantee that AF-239 exists
    to keep, silently unmet by a field name.

    The failure is one field UPSTREAM of the cause, which is what makes it worth
    a comment: nothing about reading the `NOTE-CARD:` line suggests it could
    blank `CARD:` above it, and the tool's only symptom was a correct-sounding
    "no CARD field".

    THE CONTINUATION IS LINE-SHAPED AND ACCEPTS ANY INDENT (AF-387). It used to
    be `((?:.|\n  )+?)` with a lookahead terminator: one alternation branch for
    ordinary characters and one for the literal two-space newline. `.` does not
    cross a newline without re.S, so an entry indenting its continuations by ONE
    space could not extend the group past its first line, the lookahead then had
    to match a line starting with a space, it did not, and the whole match
    failed. Same symptom as above, one width over: the tool said "entry has no
    SYMPTOM/COST to carry" about an entry that had both.

    Widening that alternation to `\n[ \t]+` fixed the reading and introduced a
    worse bug: two branches that can both consume the same text, under a `+?`,
    with a lookahead that fails, is catastrophic backtracking. It hung for over
    two minutes on the real ledger before it was killed, where the two-space form
    had been fast only because its second branch almost never matched.

    So the shape changed rather than the width. `(.*(?:\n[ \t]+.*)*)` is a first
    line plus any number of indented lines, each alternative anchored to a
    different position, no lookahead, no ambiguity. It needs no terminator
    because the next field and the next `## ` both start at column zero, which is
    the property the format already guarantees. Same 65 entries: 0.00s.

    Measured when this was written: 5 of 65 live entries and 2 of 98 archived
    ones were unreadable this way. The two archived ones had already been retired
    with their symptom and cost never reaching their cards, so the AF-38
    guarantee was quietly unmet on those moves exactly as AF-264 found it unmet
    on every move before it.

    Worth stating as a rule rather than a third patch: a hand-written indent
    width is a guess about how somebody else formats prose, and every wrong guess
    fails silently in the direction of "the field is not there".
    """
    m = re.search(rf"^{key}:[ \t]*(.*(?:\n[ \t]+.*)*)", block, re.M)
    return m.group(1).strip() if m else ""


def _api():
    """The server-written endpoint, so a port move cannot point this at a dead
    port (the same guard frustrations_retire.py grew after a silent failure)."""
    try:
        u = subprocess.run(["amux", "url"], capture_output=True, text=True,
                           timeout=10).stdout.strip()
        if u.startswith("http"):
            return u.split()[0]
    except Exception:
        pass
    return "https://localhost:8824"


def carry_to_card(block, who, superseded=False):
    """Put the SYMPTOM and COST on the entry's CARD before the entry leaves.

    AF-239. This tool and `frustrations_retire.py` each implemented HALF the
    retirement protocol and neither knew about the other. The archive answers
    "was this lost or finished?" for someone diffing the ledger (CD-78). The
    card answers "have we seen this before?" for someone hitting the bug again
    — AF-38's rule, written after 35 entries were deleted and two of that day's
    classes recurred within hours, and its whole point is that the card is where
    that person actually looks, not a thousand-line archive.

    Measured when this was found: AF-178 and AF-106 were archived correctly and
    NEITHER card carried the text, so the AF-38 guarantee had been quietly unmet
    on every archive move since the archive existed.

    Best-effort by design, and that asymmetry is deliberate: the ARCHIVE is what
    makes the move recoverable, so a card write that fails must not block the
    move or leave the entry half-retired. It reports loudly instead, and the
    entry text is in the archive either way — which is exactly the property
    frustrations_retire.py could not rely on, since it deleted outright and had
    to refuse.
    """
    card = field(block, "CARD")
    card = (card.split() or [""])[0].rstrip(",.;")
    if not card or card.lower() == "none":
        return "no CARD field — nothing to carry to"
    sym, cost = field(block, "SYMPTOM"), field(block, "COST")
    if not sym and not cost:
        return f"{card}: entry has no SYMPTOM/COST to carry"
    if superseded:
        note = ("\n\n=== SUPERSEDED-ENTRY TEXT PRESERVED (AF-38's rule) ===\n"
                f"Archived out of frustrations.md by {who}, marked SUPERSEDED — the entry's\n"
                "MECHANISM was WRONG and a later entry carries the corrected diagnosis. It is\n"
                "kept so the wrong theory stays visible as a DEAD HYPOTHESIS (ethos rule 7:\n"
                "record which hypotheses are dead, not only which one was right), and so\n"
                "nobody re-derives it. Do NOT read the text below as a confirmed defect.\n\n"
                f"SYMPTOM (as reported, since shown wrong): {sym}\n\nCOST: {cost}")
    else:
        note = ("\n\n=== RETIRED-ENTRY TEXT PRESERVED (AF-38's rule) ===\n"
                f"Archived out of frustrations.md into frustrations-archive.md, validated by {who}.\n"
                "Kept here so a RECURRENCE is recognisable from this card alone.\n\n"
                f"SYMPTOM: {sym}\n\nCOST: {cost}")
    api = _api()
    # RETRY A TRANSPORT FAILURE, because the common cause is not the card being
    # unreachable, it is the server being MID-RESTART (AF-362).
    #
    # This box rebuilds and swaps the server binary on every commit, so a batch
    # archive — 20 entries in one run tonight — reliably straddles a restart. The
    # ENTRY MOVE is a local file write and always succeeds; only this call can
    # fail. The result is a half-done retirement: the entry is gone from
    # frustrations.md and the card it pointed at never received the SYMPTOM and
    # COST, which is precisely what AF-38's rule keeps them for. Measured live:
    # AMUX-3887 and AMUX-3723 both came back `NOT carried (curl exit 7, 0 bytes)`
    # while /api/health showed uptime_s=11 a moment later.
    #
    # Three tries over ~6s covers a binary swap. Anything longer is a server that
    # is actually down, and the honest answer there is still to report NOT
    # carried rather than to block the archive.
    r = None
    for attempt in range(3):
        r = subprocess.run(["curl", "-sk", "--connect-timeout", "5", "-X", "PATCH",
                            "-H", "Content-Type: application/json",
                            "-H", "X-Amux-Session: amux-frustrations",
                            "-d", json.dumps({"desc_append": note}),
                            f"{api}/api/board/{card}"], capture_output=True, text=True)
        if r.returncode == 0 and r.stdout.strip():
            break
        if attempt < 2:
            time.sleep(2)
    # DO NOT INFER SUCCESS FROM THE ABSENCE OF AN ERROR STRING — with the server
    # unreachable curl exits 7 and prints NOTHING, so a substring test on stdout
    # reports success for a write that never happened (the AF-150 shape that bit
    # frustrations_retire.py at exactly this call).
    if r.returncode != 0 or not r.stdout.strip():
        return f"{card}: NOT carried (curl exit {r.returncode}, {len(r.stdout)} bytes)"
    if '"error"' in r.stdout or '"blocked":true' in r.stdout:
        return f"{card}: NOT carried -> {r.stdout[:120]}"
    # VERIFY THE OPERAND, not the status. A 200 says the request was accepted, not
    # that the text is on the card.
    v = subprocess.run(["curl", "-sk", "--connect-timeout", "5", f"{api}/api/board/{card}"],
                       capture_output=True, text=True)
    try:
        desc = (json.loads(v.stdout) or {}).get("desc") or ""
    except Exception:
        desc = ""
    marker = "SUPERSEDED-ENTRY TEXT PRESERVED" if superseded else "RETIRED-ENTRY TEXT PRESERVED"
    if marker not in desc:
        return f"{card}: NOT carried (card does not read back with the marker)"
    return f"{card}: symptom + cost carried to the card"


def main():
    md = LEDGER.read_text()
    lines, spans = parse(md)
    if len(sys.argv) > 1 and sys.argv[1] == "--list":
        for ln, (_, _, title) in sorted(spans.items()):
            print(f"L{ln:<6} {title[:100]}")
        return 0
    if len(sys.argv) < 4:
        print(__doc__)
        return 2
    argv = [a for a in sys.argv if a != "--superseded"]
    superseded = len(argv) != len(sys.argv)
    sys.argv = argv
    ln, who = int(sys.argv[1]), sys.argv[2]
    # EVIDENCE FROM STDIN OR A FILE, not only from argv (AMUX-1888's shape, hit
    # here on 2026-08-25).
    #
    # Evidence text quotes code, and code contains backticks. Passed as a
    # positional argument inside double quotes, YOUR SHELL evaluates it before
    # this script ever runs: `now` became the empty string, and
    # `grep -c 'WORK ITSELF is at risk'` was EXECUTED and replaced by its own
    # output, so an archived line read "so 0 returned 0 across the whole
    # window". Both silently, in the file that exists to be the durable record
    # of what was verified — the one place a mangled quotation is least
    # recoverable, since the entry it describes has just been deleted from
    # frustrations.md.
    #
    # `amux send` and `amux board add` already learned this and grew
    # --stdin/--file. This tool took the same shape and had not.
    if len(sys.argv) > 3 and sys.argv[3] == "--evidence-stdin":
        evidence = sys.stdin.read().strip()
    elif len(sys.argv) > 4 and sys.argv[3] == "--evidence-file":
        with open(sys.argv[4]) as fh:
            evidence = fh.read().strip()
    else:
        evidence = " ".join(sys.argv[3:])
    if ln not in spans:
        print(f"no entry starts at line {ln}. `--list` shows the heading lines.", file=sys.stderr)
        return 1
    i, end, title = spans[ln]
    body = lines[i:end]
    # Trim trailing blanks so the archive does not accumulate them.
    while body and not body[-1].strip():
        body.pop()
    stamped = [body[0]]
    # THE THIRD DISPOSITION (AF-243, raised by amux during the 2026-08-26 drain).
    # An entry can be RIGHT-AND-FIXED, RIGHT-AND-STILL-LIVE, or WRONG. The first
    # two had exits — archive with a VALIDATED line, or reopen the card. The
    # third had none, and the available moves both lie about it: archiving a
    # wrong entry under `VALIDATED:` files a FALSE MECHANISM as validated
    # history, and reopening its card says a friction is live that was never
    # real. Their words: "a wrong entry that can only be validated or reopened
    # has no honest exit", which is ethos rule 3 exactly — a constraint with no
    # truthful path through it.
    #
    # The specimen: AMUX-3721 claimed browser state could see overlay content
    # but not click it. The selector always contained [onclick] and
    # selector_click_js() already existed; the real defect was a silent
    # 120-element cap, with the two elements at indices 155 and 156, addressable
    # the whole time. Its author superseded it in place with the corrected
    # diagnosis, so the archive needs to carry it as WRONG rather than as fixed.
    stamped.append(f"{'SUPERSEDED' if superseded else 'VALIDATED'}: {who} | {evidence}")
    stamped.extend(body[1:])

    if not ARCHIVE.exists():
        ARCHIVE.write_text(ARCHIVE_HEADER)
    arch = ARCHIVE.read_text().rstrip("\n")

    # AF-417: IS THIS TITLE ALREADY ARCHIVED?
    #
    # An archive move is a DELETION from frustrations.md, and this file is
    # merged across divergent branches. So any merge from a side that predates
    # the archive RESURRECTS the entry, at STATUS: open, with no trace that it
    # was ever retired. Nothing downstream can tell a resurrected entry from a
    # new one: it reads open, its text still says the work is undone, and the
    # archive is a separate file nobody greps before starting.
    #
    # MEASURED. "A shared CARGO_TARGET_DIR is mandated..." was archived
    # 2026-08-29 in 53cafb92, which correctly removed it from the ledger. A
    # human sync commit (7dbab8f6, "sync frustrations.md to fork's current
    # copy") put it back, and two merges from feature/telegram-connector
    # (4216504b, 09dd5024) carried it onto main. On 2026-09-02 it was picked up
    # as open and re-diagnosed from scratch -- ~40 minutes reaching, correctly,
    # the SAME conclusion the 2026-08-29 VALIDATED line already recorded: the
    # builder's disk-pressure `rm -rf` with no in-flight check, cargo GC ruled
    # out. Two independent derivations agreeing is reassuring about the answer
    # and says nothing good about the process.
    #
    # `.claude/rules/frustrations.md` already warns about this in the other
    # direction -- do not re-append something that merely LOOKS lost, grep the
    # archive first, creative-dna measured 15 of 15 "lost" entries as archive
    # moves. That rule asks a human to remember. This is the same check, run by
    # the tool that has both files open anyway.
    #
    # A WARNING, NOT A REFUSAL. A genuine recurrence is legitimate: a friction
    # can return, and its entry may honestly be re-logged and re-retired under
    # the same title. Refusing would be a gate with no truthful path (ethos rule
    # 3) for that case. So it archives, and says loudly what it just noticed --
    # the reader can see the prior sign-off and decide whether they have
    # re-derived it or genuinely re-hit it.
    prior = [l for l in arch.splitlines() if l.strip() == body[0].strip()]
    if prior:
        sys.stderr.write(
            f"\nWARNING: this title is ALREADY in {ARCHIVE.name} "
            f"({len(prior)} prior copy/copies).\n"
            "  An archive move is a deletion, and this file merges across branches, so an\n"
            "  entry retired once can be resurrected by a merge from a side that predates\n"
            "  the archive -- resurfacing at STATUS: open with its prior sign-off invisible.\n"
            "  Before trusting the work you just did: read the earlier VALIDATED line.\n"
            f"  grep -n -A3 {body[0].strip()[:48]!r} {ARCHIVE.name}\n"
            "  If it already answers this entry, you re-derived a retired conclusion and\n"
            "  the finding is the resurrection, not the diagnosis (AF-417).\n\n")

    ARCHIVE.write_text(arch + "\n\n" + "\n".join(stamped) + "\n")

    # Carry BEFORE the ledger write, so a crash between the two leaves the entry
    # in place rather than gone from both the ledger and the card.
    carried = carry_to_card("\n".join(body), who, superseded)

    remaining = lines[:i] + lines[end:]
    LEDGER.write_text("\n".join(remaining))
    print(f"archived L{ln}: {title[:70]}")
    print(f"  {'SUPERSEDED (entry was WRONG)' if superseded else 'validated'} by {who}")
    print(f"  card: {carried}")
    # NAME BOTH FILES, AND THE COMMAND (AF-436).
    #
    # This is a MOVE across two files and the summary above named neither, so
    # the natural next step is `git add frustrations.md` -- the file you were
    # reading, the file whose line number you passed. That stages the DELETION
    # without the APPEND, and the resulting commit holds the entry in neither
    # file, which is precisely the lost-work state AF-430 was filed about.
    #
    # Measured, self-inflicted 2026-09-03: eb552cc1 did exactly that, to MR-44,
    # five hours after AF-430 restored MR-44 from an earlier instance of the
    # same shape. The append-only push guard refused the push and named the 34
    # missing lines, so it never reached origin -- but the guard is the LAST
    # line of defence and it fires minutes to hours later, at push time, on
    # whoever pushes next. This prints at the moment the two files diverge.
    #
    # A pathspec, not `git add -A`: this repo's shared checkout has one index
    # for every lane, and `-A` is refused by its own guard for that reason.
    print("")
    print("  This was a MOVE across TWO files. Stage BOTH or the commit holds the")
    print("  entry in neither (AF-436, and AF-430 is what that costs):")
    print(f"    git add {LEDGER.name} {ARCHIVE.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
