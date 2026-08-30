#!/usr/bin/env python3
# UTILITY, not the old Python server: audits frustrations.md CARD: pointers against the live board over HTTP (server-agnostic).
"""Audit frustrations.md's CARD: pointers against the live board (AF-28).

The protocol rests on this field. `.claude/rules/frustrations.md` requires every entry to
link a card ("a frustration without a CARD: is a complaint, with one it is work somebody
can pick up"), and the deletion protocol keys an author's confirmation to the entry->card
pair. Nothing validated it, so on 2026-08-09 five of thirty-four entries queued for
deletion pointed at cards about something else entirely — one of them another session's
OPEN card, which was seconds from receiving "validated, deleting" text.

Why the field rots by construction, rather than by carelessness:
  - ids are hand-typed into markdown, with no write path that could check them
  - boards are per-instance, so an id valid on one board silently names a different card
    on another (AC-*, AMUX-*, AH-*, MS-* are not one namespace)
  - supersede entries get filed under the ORIGINAL entry's card id, so one id legitimately
    covers several entries and "delete AC-300" is ambiguous
From the file alone, a stale id and a colliding id are indistinguishable.

Exit codes: 0 clean, 1 problems found, 2 could not reach the board AND nothing
structural was wrong (NOT a pass — an unreachable board means unchecked, and this says
so rather than exiting 0).

The "AND nothing structural was wrong" is the AEAB-19 fix, and it is the whole contract:
structural problems are decidable without a board, so they exit 1 whether or not the
board answered. Previously the unreachable branch returned a bare 2 and threw the
structural verdict away — and since CI never has a board and treats 2 as a pass, the
gate could not fail there at all.

    python3 scripts/frustrations_audit.py [--quiet]
"""
import json
import os
import re
import ssl
import sys
import urllib.request
from collections import defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
FRUST = REPO / "frustrations.md"
REQUIRED = ["AREA", "SEVERITY", "STATUS", "DATE", "SESSION", "CARD", "SYMPTOM", "COST", "FIX"]


def parse(text):
    """Entries are '## ' at column 0, AFTER the header's `---` rule.

    Two separate false positives live here and both make the audit useless in the same
    way — by crying wolf on every run, which is how a check gets ignored:

      - the field TEMPLATE inside the header is indented two spaces precisely so it does
        not match (the file says so itself: "an instrument that measures itself is the
        bug this file exists to record"), and
      - the header's own SECTION HEADING, `## Format — fixed fields so this greps`, IS at
        column 0 and is not an entry. The first cut of this audit reported it as an entry
        missing all nine required fields, on every run, forever.

    Anchoring on the `---` that closes the header fixes both structurally rather than by
    string-matching a heading that someone will eventually reword."""
    body = text.split("\n---\n", 1)
    text = body[1] if len(body) > 1 else text
    out = []
    for blk in re.findall(r'(?ms)^## .*?(?=\n## |\Z)', text):
        e = {"title": blk.split("\n", 1)[0][3:].strip(), "_raw": blk}
        for f in REQUIRED:
            m = re.search(r'(?m)^%s:\s*(.*)$' % f, blk)
            e[f] = m.group(1).strip() if m else None
        out.append(e)
    return out


def structure_check(text, entries):
    """Cross-check the entry count against an INDEPENDENT signal, and fail loud.

    Added 2026-08-14 at amux-cloud's suggestion, after their catch. A session
    (me) audited this file with an ad-hoc parser that split entries on `DATE:`.
    Field ORDER varies here — plenty of entries put STATUS: above DATE: — so
    every such entry's STATUS bound to the PREVIOUS entry and it inherited the
    NEXT one's. The error ran in the only direction that costs something: OPEN
    entries reading as `fixed`, i.e. proposed for DELETION, which is the single
    irreversible step in the validate-and-delete loop. An open entry recording a
    live, thrice-regressed incident was on that list.

    The discriminator existed the whole time and nobody was routed to it: this
    script said 122 entries, the ad-hoc parse said 127. Both numbers were read in
    the same session and never compared. So the fix is not "write better
    parsers" — it is to make the disagreement ANNOUNCE ITSELF from the canonical
    tool, because the next person will also write an ad-hoc parse and will also
    have no reason to suspect it.

    One DATE: and one STATUS: per entry is the file's own contract. If either
    tally drifts from the '## ' heading count, something is malformed OR someone
    is about to be misled, and both are worth stopping for.
    """
    body = text.split("\n---\n", 1)
    body = body[1] if len(body) > 1 else text
    problems = []
    for field in ("DATE", "STATUS"):
        n = len(re.findall(r'(?m)^%s:' % field, body))
        if n != len(entries):
            problems.append(
                "  %s: %d occurrence(s) vs %d entries (delta %+d)"
                % (field, n, len(entries), n - len(entries))
            )
    if problems:
        print("STRUCTURE DRIFT — the entry count disagrees with its own fields:")
        print("\n".join(problems))
        print("  Entries are '## ' headings. Do NOT split on DATE: — field order")
        print("  varies, and a DATE-split silently shifts STATUS by one entry")
        print("  (open -> fixed), which proposes live entries for deletion.")
        print("  Canonical count from this script: %d" % len(entries))
        return False
    return True


def fetch_board():
    # The DEFAULT here is already correct; the hazard is the ENV VAR overriding it
    # with a dead address. 8822 was the Python compatibility bind, removed
    # 2026-08-11, and every session spawned before that still carries the old
    # AMUX_URL in its process env — which a live process cannot re-read. So the
    # override has to be ignored when it names the retired port.
    #
    # This fails SILENTLY otherwise: fetch_board() raises, main() prints "Structural
    # checks only" and exits 2, which reads like a deliberate offline mode rather
    # than a broken probe. It ran that way for a full sweep before anyone noticed.
    base = os.environ.get("AMUX_URL", "") or "https://localhost:8824"
    if base.rstrip("/").endswith(":8822"):
        base = "https://localhost:8824"
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    ids = {}
    # done_limit matters: the default GET caps done items, so a plain fetch reports live
    # cards as missing. That cap is what made a first pass at this audit claim 48 absent
    # cards when the real number was 1.
    for q in ("?done_limit=100000", "?done_limit=100000&archived=1"):
        req = urllib.request.Request(base + "/api/board" + q)
        for i in json.load(urllib.request.urlopen(req, context=ctx, timeout=30)):
            ids[i["id"]] = i
    return ids


def fetch_sessions():
    """Live session names on THIS server, or None if that cannot be established.

    Exists to split one advisory into two (AF-229). `not on this board` was printed
    identically for AC-227 (amux-cloud, a live lane here — ask them and the entry
    drains) and AEAB-18 (amux-errors-and-bugs, absent from all 120 sessions, working
    out of a `~/Developer/amux` that does not exist on this machine). The deletion
    protocol keys removal to the ORIGINATING SESSION's sign-off, so those two states
    need opposite handling and the ledger could not tell them apart.

    Returns None rather than an empty set when the fetch fails: an empty set would
    make EVERY entry look stranded, which is the loud-wrong-probe failure — a
    confident answer produced by a broken instrument. Callers must treat None as
    "unknown" and claim neither state.
    """
    base = os.environ.get("AMUX_URL", "") or "https://localhost:8824"
    if base.rstrip("/").endswith(":8822"):
        base = "https://localhost:8824"
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    try:
        req = urllib.request.Request(base + "/api/sessions")
        data = json.load(urllib.request.urlopen(req, context=ctx, timeout=30))
    except Exception:
        return None
    names = {s["name"] for s in data if isinstance(s, dict) and s.get("name")}
    return names or None


def citing_session(raw):
    """The session NAME out of a SESSION: field, or "" if there isn't one.

    The field is prose in practice and always has been: `amux-rust (lifecycle-fix
    subagent)`, `amux (hit it, twice), amux-frustrations (verified the mechanism)`,
    `(agent, AMUX-2629)`, `(Claude Code in iTerm - not a fleet lane, hence no session
    stamp)`. Taking the leading bare token handles the first two and correctly yields
    "" for the last two, which are genuinely not lane names.

    Deliberately NOT a model call (ethos rule 2): this is string manipulation, and a
    helper-model call here would be the 12-15k-token label mistake again.
    """
    m = re.match(r"\s*([A-Za-z][A-Za-z0-9_-]*)", raw or "")
    return m.group(1) if m else ""


def overlap(a, b):
    """Loose word overlap. Deliberately crude and deliberately only ADVISORY: card titles
    get rewritten as understanding improves, so a low score is 'a human should look',
    never 'this is wrong'. Reporting it as an error would train people to ignore it."""
    w1 = {w.lower().strip('`",.:;()') for w in a.split() if len(w) > 4}
    w2 = {w.lower().strip('`",.:;()') for w in b.split() if len(w) > 4}
    if not w1 or not w2:
        return 1.0
    return len(w1 & w2) / min(len(w1), len(w2))


# AF-172. THE FILE'S OWN DISCRIMINATOR, READ BY SOMETHING.
#
# frustrations.md states the rule in its own header: "a single frustration is a
# complaint and a cluster is an argument. No one entry proves a subsystem needs
# rebuilding; three entries sharing an AREA do, and that pattern is invisible
# unless the entries are counted."
#
# Nothing counted them. On 2026-08-23 `instruments` stood at 35 open-or-fixed
# entries, the loudest ranked signal in the system, and no scheduler, view or
# nudge consumed it. The rule was written down and never run, which is the shape
# ethos rule 7 records about rules generally.
#
# It lives HERE rather than in a new script on purpose (ethos rule 1: a
# capability nobody is enrolled in is decoration). The audit already parses AREA,
# already runs in CI via scripts/test-frustrations-audit.sh, and is already what
# people run before touching this file. A separate `frustration_clusters.py`
# would have to be remembered; this cannot be, because it prints on every run.
#
# IT REPORTS AND DOES NOT ACT. No card is filed, nothing is prioritised for
# anyone. 273 board cards were created in the 24h before this was written, and
# more detectors without a human deciding is ethos rule 5 — it becomes a log, and
# no gate can govern a log. The OPEN count is what ranks, because a cluster whose
# entries are all fixed is a solved argument, not a live one.
def cluster_counts(entries):
    """{AREA: (total, open)} — the counting half, so --since can reuse it."""
    by = {}
    for e in entries:
        area = (e.get("AREA") or "").strip() or "(none)"
        st = (e.get("STATUS") or "").strip().split()[0] if (e.get("STATUS") or "").strip() else ""
        tot, op = by.get(area, (0, 0))
        by[area] = (tot + 1, op + (1 if st.startswith("open") else 0))
    return by


def print_cluster_rank(entries, threshold=3):
    by = cluster_counts(entries)
    ranked = sorted(by.items(), key=lambda kv: (-kv[1][1], -kv[1][0], kv[0]))
    argued = [(a, t, o) for a, (t, o) in ranked if o >= threshold]
    singles = sum(1 for _, (t, _) in by.items() if t == 1)
    print("  AREA CLUSTERS — %d areas, %d singleton(s); >=%d OPEN is an argument"
          % (len(by), singles, threshold))
    if not argued:
        print("              no AREA has %d open entries — no cluster argues for a rebuild"
              % threshold)
    for area, tot, op in argued:
        print("              %-16s %2d open / %2d total" % (area, op, tot))
    return argued



# AF-173. THE DELTA, WITHOUT A SCHEDULER AND WITHOUT NEW STORAGE.
#
# The proposal on AF-173 said the delta needs "a scheduler plus the instruments
# that already exist". For this instrument it needs neither: frustrations.md is
# in git, so its history is already recorded and the change is a query.
#
# Why that matters more than the convenience: every instrument here reports a
# LEVEL (81 entries, 25 open in `instruments`, 2,523 nudges), and a level tells
# you the machine is on. Only the delta answers "did the fixes reduce the
# friction". Its first run said something a level could not: on 2026-08-23,
# across ~28 shipped fixes and 11 retired entries, every cluster was flat or
# GROWING (instruments 22->25, attribution 8->10, cli 3->6). Detection is
# outpacing repair.
#
# READ-ONLY and on demand: no cadence, nothing scheduled, no fleet-wide cost.
# Whether this is ever reported on a schedule is the owner's call (AF-173 is
# needs:you for exactly that), and adding another scheduled emitter to a fleet
# already spending 72% of its plan on background turns is not a decision an
# agent should make for them.
def print_cluster_delta(entries, rev):
    import subprocess
    r = subprocess.run(["git", "show", f"{rev}:frustrations.md"],
                       capture_output=True, text=True)
    if r.returncode != 0 or not r.stdout.strip():
        # REFUSE rather than print a delta against nothing. An unreadable rev
        # silently treated as "no entries" would render every open cluster as
        # pure growth, which is a confident wrong answer in the alarming
        # direction.
        print("  DELTA     cannot read frustrations.md at %r — no delta computed" % rev)
        return None
    then = cluster_counts(parse(r.stdout))
    now = cluster_counts(entries)
    areas = sorted(set(then) | set(now))
    rows = []
    for a in areas:
        t_open = then.get(a, (0, 0))[1]
        n_open = now.get(a, (0, 0))[1]
        if t_open or n_open:
            rows.append((a, t_open, n_open, n_open - t_open))
    rows.sort(key=lambda r: (-abs(r[3]), -r[2], r[0]))
    print("  DELTA vs %s — open entries per AREA (+ is growing, which means"
          " detection is outpacing repair)" % rev)
    for a, t, n, d in rows:
        print("              %-16s %2d -> %2d  %+d" % (a, t, n, d))
    tot = sum(r[2] for r in rows) - sum(r[1] for r in rows)
    print("              %-16s %+d overall" % ("", tot))
    return rows


def main():
    quiet = "--quiet" in sys.argv
    since = None
    if "--since" in sys.argv:
        i = sys.argv.index("--since")
        since = sys.argv[i + 1] if i + 1 < len(sys.argv) else None
    raw = FRUST.read_text()
    entries = parse(raw)
    # Before any per-entry finding: does the file's own shape agree with itself?
    # A structural drift makes every downstream verdict suspect, so it is
    # reported FIRST rather than buried under 122 lines of per-entry output.
    structure_ok = structure_check(raw, entries)
    # BEFORE the board fetch, because it does not need one. The first version
    # printed this only on the board-reachable path, so the one signal that is
    # pure text parsing went silent exactly when the server was down — and the
    # test cells caught it, which is the whole reason they exist.
    print_cluster_rank(entries)
    if since:
        print_cluster_delta(entries, since)
    problems, advisories = [], []
    # AEAB-19, second instance in this same file. `structure_ok` was assigned here
    # and NEVER READ — the drift check printed its finding and had no effect on the
    # exit code. Its own docstring says it exists because an ad-hoc DATE-split
    # shifted STATUS by one entry and put a live, thrice-regressed incident on a
    # DELETION list, "seconds from receiving 'validated, deleting' text". That is
    # the most expensive failure this file has, and the check guarding it was
    # advisory by accident.
    #
    # It was firing on main at the time of this fix, and had been since 18590ca8
    # landed a frustrations entry with NO `## ` heading — so the parser folded it
    # into the preceding entry, which is exactly the count disagreement this check
    # reports (DATE: 120 vs 119 entries). It reached main because the OTHER half of
    # this bug meant `checks` could not fail. One defect hid the other.
    #
    # Structural drift is decidable without a board, so it fails in both branches.

    for e in entries:
        miss = [f for f in REQUIRED if not e.get(f)]
        if miss:
            problems.append("%-52s missing field(s): %s" % (e["title"][:52], ", ".join(miss)))

    # Duplicate ids are NOT automatically wrong — a supersede entry under the original's
    # id is legitimate and amux-cloud does it deliberately. But it means the id cannot be
    # used as a delete key, which is the trap that nearly destroyed the wrong two of four
    # AC-300 entries. Report it so anyone scripting against this file knows.
    dupes = defaultdict(list)
    for e in entries:
        if e.get("CARD") and e["CARD"].lower() != "none":
            dupes[e["CARD"]].append(e["title"])
    shared_ids = {k: v for k, v in dupes.items() if len(v) > 1}

    # AF-229. Fetched even though only the unresolved-id branch reads it, because
    # `sessions is None` is a THIRD state that branch has to report honestly, and
    # deciding that per-entry would re-ask a dead endpoint once per entry.
    sessions = fetch_sessions()
    stranded = []
    board_prefixes = set()

    try:
        board = fetch_board()
    except Exception as ex:
        print("CANNOT REACH BOARD: %s" % ex)
        print("Structural checks only. %d entries, %d structural problem(s)."
              % (len(entries), len(problems)))
        for p in problems:
            print("  PROBLEM  " + p)
        # AEAB-19. This returned a bare 2, discarding whether `problems` is
        # non-empty — and the board is ALWAYS unreachable in CI, which
        # .github/workflows/checks.yml says in its own comment while treating 2 as
        # a pass. So the structural half ran, printed its findings, and had its
        # verdict thrown away on every push. `checks` is the only required status
        # check on main, and this, its frustrations gate, could not fail. There
        # was a live specimen the whole time (one entry missing SEVERITY and
        # SYMPTOM) with the check green over it.
        #
        # Worse than a silent probe: a LOUD one whose output is correct and whose
        # exit code contradicts it. The PROBLEM line was in the CI log every run,
        # read by nobody, because the step was green — ethos rule 4's "a tag in a
        # store the reader never opens", one layer out, where the check itself is
        # what stops the reader opening it.
        #
        # The two halves are independent and now exit independently: a structural
        # problem is decidable WITHOUT a board and so must fail; the CARD: half
        # genuinely could not be checked and stays 2. Note the module docstring
        # already said "2 ... (NOT a pass)" — the script and the workflow
        # disagreed, and only the script knows whether `problems` is empty.
        return 1 if (problems or not structure_ok) else 2

    # Every id namespace this board actually holds — the discriminator for "foreign
    # instance" vs "id I mistyped or someone deleted" (AF-229). Derived from the board
    # rather than hardcoded, so a new lane's prefix is known the moment it files a card.
    board_prefixes = {i.rsplit("-", 1)[0] for i in board if "-" in i}

    for e in entries:
        c = e.get("CARD")
        if not c or c.lower() == "none":
            continue
        # CARD fields are not always a bare id. Real ones in this file:
        #   "AR-114, AR-115, AR-116, AR-118, AR-119, AR-120"
        #   "AF-69 (investigation, signed off) + AMUX-3221 (the FIX, open)"
        # `board.get(c)` on the whole string missed every one of them, so multi-id
        # entries have ALWAYS reported as unresolved. That was invisible while the
        # branch printed a mild "other instance, or deleted"; the moment AF-229 made
        # the branch say something specific, it said something specific and WRONG
        # (six live AR-* cards, all HTTP 200, announced as unreachable). Extract the
        # ids and judge on those.
        ids = re.findall(r"\b([A-Z][A-Z0-9]{1,9}-\d+)\b", c)
        if not ids:
            advisories.append("%-10s CARD names no parseable id :: %s" % (c[:10], e["title"][:46]))
            continue
        found = [i for i in ids if i in board]
        if found and len(found) < len(ids):
            advisories.append("%-10s %d of %d ids resolve (%s missing) :: %s"
                              % (ids[0], len(found), len(ids),
                                 ", ".join(i for i in ids if i not in board), e["title"][:40]))
        if not found:
            # Cross-instance ids are expected (AC-* live on amux-cloud's board). Flag as
            # advisory rather than error, but SAY it, so "not on this board" is a known
            # state rather than a silent hole in the protocol.
            #
            # AF-229: it stays advisory, but it must not be ONE OUTPUT FOR TWO STATES.
            # The discriminator is the PREFIX NAMESPACE, not whether the author happens
            # to be running. amux-rust is not live either, yet AR-114 answers HTTP 200 —
            # same board, a lane that simply is not up, and its entries are drainable
            # whenever it is. AEAB-* resolve nowhere: 0 of 9,296 cards carry that prefix
            # while DESKT-*, also a non-fleet lane, carries 25. Judging on liveness alone
            # would have called six drainable AR-* entries permanently stranded.
            who = citing_session(e.get("SESSION", ""))
            prefixes = {i.rsplit("-", 1)[0] for i in ids}
            foreign = prefixes - board_prefixes
            if foreign == prefixes and sessions is not None and who and who not in sessions:
                where = ("STRANDED: prefix %s exists nowhere on this board and author %s "
                         "is not in this fleet" % ("/".join(sorted(foreign)), who))
                stranded.append((ids[0], who, e["title"]))
            elif sessions is not None and who and who in sessions:
                where = "id missing; author %s IS live here — ask them" % who
            else:
                where = "not on this board (other instance, or deleted)"
            advisories.append("%-10s %s :: %s" % (ids[0], where, e["title"][:46]))
            continue
        card = board[found[0]]
        ov = overlap(e["title"], card.get("title", ""))
        if ov <= 0.3:
            advisories.append("%-10s TITLE MISMATCH (%.2f)\n              entry: %s\n              card:  %s"
                              % (c, ov, e["title"][:70], card.get("title", "")[:70]))

    if not quiet or problems or advisories:
        print("frustrations.md audit — %d entries" % len(entries))
        for p in problems:
            print("  PROBLEM   " + p)
        for a in advisories:
            print("  CHECK     " + a)
        if shared_ids:
            print("  NOTE      %d card id(s) cover multiple entries — id is NOT a delete key:"
                  % len(shared_ids))
            for k, v in sorted(shared_ids.items()):
                print("              %s x%d" % (k, len(v)))
        if stranded:
            # ROLLED UP, not left as N scattered CHECK lines (AF-229). The argument
            # this file makes is a COUNT — "three entries sharing an AREA" — so the
            # thing a reader needs is how much of the open set is permanently
            # undrainable, which no per-entry line delivers. By session, because the
            # remedy is per-session (reach that lane, or retire its entries), not
            # per-entry.
            by_who = defaultdict(list)
            for cid, who, _t in stranded:
                by_who[who].append(cid)
            print("  STRANDED  %d entr(ies) cite a card no one in this fleet can reach."
                  % len(stranded))
            for who, ids in sorted(by_who.items()):
                print("              %-24s %d :: %s" % (who, len(ids), ", ".join(sorted(set(ids)))))
            print("              These cannot leave the file by the sanctioned path: deletion")
            print("              needs the ORIGINATING session's sign-off and it is not here.")
        elif sessions is None:
            # Absence of a STRANDED block must not read as "none stranded" when the
            # question was never asked (ethos rule 7 — a passing check and an absent
            # check look identical).
            print("  NOTE      sessions unreachable — no entry was classified as stranded")
        if not problems and not advisories:
            print("  clean — every CARD: resolves and plausibly matches its entry")
    return 1 if (problems or not structure_ok) else 0


if __name__ == "__main__":
    sys.exit(main())
