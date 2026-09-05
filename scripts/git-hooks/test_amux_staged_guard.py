#!/usr/bin/env python3
"""Test for MR-43: amux-staged-guard derives a session identity from the tmux
pane name when $AMUX_SESSION is empty, instead of silently no-opping and
leaving the lane's edits absent from the cross-session record.

Run: python3 ~/.amux/hooks/test_amux_staged_guard.py   (exit 0 = all pass)
"""
import importlib.machinery
import os
import subprocess
import sys

HOOK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "amux-staged-guard")
mod = importlib.machinery.SourceFileLoader("_asg_test", HOOK).load_module()


def _fake_run(stdout):
    def run(args, **kw):
        class R:
            pass
        r = R()
        r.stdout = stdout
        return r
    return run


def main():
    failures = []
    real_run = subprocess.run

    # CONTROL FIRST: an amux-prefixed pane name DOES resolve, so a matcher that
    # silently always returns "" cannot hide behind an all-negative suite.
    subprocess.run = _fake_run("amux-mixpeek-research\n")
    got = mod._derive_session_from_tmux()
    if got != "mixpeek-research":
        failures.append(
            f"control: 'amux-mixpeek-research' should derive 'mixpeek-research', got {got!r}")

    # A human's own tmux session (no amux- prefix) must never be claimed as a
    # lane — the whole point of scoping the fallback to the prefix.
    subprocess.run = _fake_run("main\n")
    got = mod._derive_session_from_tmux()
    if got != "":
        failures.append(f"a bare tmux session name must not resolve to a session: got {got!r}")

    # Outside tmux entirely (or tmux missing from PATH): fail closed to "",
    # never raise — this runs inside a git hook, which must not crash a commit.
    def _raise(*a, **kw):
        raise FileNotFoundError("no tmux")
    subprocess.run = _raise
    try:
        got = mod._derive_session_from_tmux()
        raised = None
    except Exception as e:
        got, raised = None, e
    if raised is not None:
        failures.append(f"must not raise when tmux is unavailable: {raised!r}")
    elif got != "":
        failures.append(f"tmux unavailable should derive '', got {got!r}")

    subprocess.run = real_run

    # GUARD_VERSION must have moved off the pre-fix baseline, or every already-
    # installed copy on this machine reads as current and never re-syncs (the
    # file's own header: "the installed-copy inventory greps" on this number).
    if mod.GUARD_VERSION <= 8:
        failures.append(f"GUARD_VERSION is {mod.GUARD_VERSION} — bump it, every install checks this")

    # AF-410: THE DOCUMENTED EXTRACTION MUST RETURN THE VALUE THE INTERPRETER
    # LOADS. This file's header carries an inventory command built on `grep -m1`
    # over this constant, and the header itself records that an UNANCHORED match
    # once found a comment line first. That was fixed by anchoring and verified
    # by hand — "checked: it returned empty" — which is a one-time observation,
    # not a check that can fail. It is now a check that can fail.
    #
    # The failure this pins is live RIGHT NOW in another checkout. Mixpeek's
    # vendored copy carries the pre-fix comment naming `GUARD_VERSION = 8` as a
    # literal while its own constant is 9, so the obvious extraction returns 8.
    # ts-gke hit exactly that on 2026-09-02 and reported the file as v8. The
    # direction happened to be harmless there (too LOW still reads as stale), but
    # a comment quoting a HIGHER number would make every stale copy read as
    # current — a staleness check that passes forever, which is the bug this
    # whole card is about.
    import re as _re
    _src = open(HOOK).read()
    _anchored = _re.search(r"^GUARD_VERSION *= *(\d+)", _src, _re.M)
    if not _anchored:
        failures.append("no anchored `GUARD_VERSION = N` line — the inventory grep finds nothing")
    elif int(_anchored.group(1)) != mod.GUARD_VERSION:
        failures.append(
            f"the anchored extraction reads {_anchored.group(1)} but the module loads "
            f"{mod.GUARD_VERSION} — the documented inventory command lies")
    # And no line ABOVE the assignment may look like one, or a first-match read
    # (anchored or not) lands on the decoy instead.
    _lines = _src.splitlines()
    _assign_at = next((i for i, l in enumerate(_lines)
                       if _re.match(r"^GUARD_VERSION *= *\d+", l)), len(_lines))
    for _i, _l in enumerate(_lines[:_assign_at]):
        if _re.search(r"GUARD_VERSION *= *\d+", _l):
            failures.append(
                f"line {_i + 1} quotes a literal `GUARD_VERSION = N` above the real "
                f"assignment on line {_assign_at + 1}; an unanchored `grep -m1` returns it "
                f"instead: {_l.strip()!r}. Derive the number in prose, do not repeat it.")

    # AF-190: the server emits `split_risk`; this asserts a HOOK actually prints
    # it. A field nobody renders reaches nobody, and nothing about reading the
    # server code would say so — that gap is ethos rule 1, and it is why the
    # renderer was pulled out of main() into a function a test can call.
    out = []
    mod._render_split_risk({
        "split_risk": [{
            "owner": "amux",
            "staged": ["crates/amux-server/src/api/board.rs"],
            "left_dirty": ["/repo/crates/amux-server/src/db/board_store.rs"],
            "why": "a symbol added on one side may be missing from the other",
        }]
    }, out.append)
    txt = "".join(out)
    for needle, what in [
        ("amux", "the peer whose work is being split"),
        ("board.rs", "the staged file"),
        ("board_store.rs", "the file left behind — naming it is the whole point"),
        ("NOT committed", "that the second half is not in this commit"),
    ]:
        if needle not in txt:
            failures.append(f"split_risk render omits {what} ({needle!r}): {txt!r}")

    # CONTROL: silent when there is nothing to say. A warning that prints on
    # every commit is one nobody reads, which is exactly how the insertion-count
    # line this replaces came to be ignored.
    for empty in ({}, {"split_risk": []}, {"split_risk": None}):
        out = []
        mod._render_split_risk(empty, out.append)
        if out:
            failures.append(f"split_risk must print NOTHING for {empty!r}, got {out!r}")

    # AF-414: an MTIME-ONLY record must not carry the possessive header. The
    # server sends `authored: false` when it cannot support an ownership claim;
    # the row still prints, because the BUILD hazard is real whoever owns the
    # bytes, and only "X's work is being cut in half" goes.
    out = []
    mod._render_split_risk({
        "split_risk": [{
            "owner": "amux",
            "authored": False,
            "staged": ["crates/amux-server/src/api/board.rs"],
            "left_dirty": ["/repo/crates/amux-server/src/db/board_store.rs"],
            "why": "the only record linking these paths to 'amux' is an mtime",
        }]
    }, out.append)
    txt = "".join(out)
    if "'s work is being cut in half" in txt:
        failures.append(f"authored=false must drop the possessive header, got: {txt!r}")
    for needle, what in [
        ("SPLIT COMMIT WARNING", "the warning still fires — downgrade, not suppression"),
        ("board.rs", "the staged file is still named"),
        ("board_store.rs", "the file left behind is still named"),
        ("mtime", "the server's sentence still reaches the reader"),
    ]:
        if needle not in txt:
            failures.append(f"authored=false render dropped {what} ({needle!r}): {txt!r}")

    # CONTROL: an OLD SERVER sends no `authored` key at all. It must keep the
    # possessive rather than be silently downgraded by a client that assumes the
    # worst — a missing field is "cannot answer", not "answer is no".
    out = []
    mod._render_split_risk({
        "split_risk": [{
            "owner": "amux",
            "staged": ["a.rs"],
            "left_dirty": ["/repo/b.rs"],
            "why": "why",
        }]
    }, out.append)
    if "amux's work is being cut in half" not in "".join(out):
        failures.append(f"a server sending no `authored` must keep the old wording: {out!r}")

    # AF-365: the BLOCKED remedy must offer the non-destructive exit FIRST.
    #
    # On a shared index `git restore --staged <their path>` mutates state that
    # belongs to the other lane: their file is staged because THEY staged it, and
    # unstaging is an edit to someone else's in-flight work made by a party who
    # cannot see what they intended. `git commit <your paths>` ignores the index
    # for everything it does not name, so both lanes commit whole in either order.
    #
    # HONEST ABOUT WHAT THIS PROVES. It reads the SHIPPED hook file rather than
    # executing the branch, because that text is emitted inline in main() and
    # reaching it needs a full multi-session git fixture. So this pins that the
    # advice EXISTS and is ORDERED, not that the branch runs. That is weaker than
    # the cells above and is worth saying rather than leaving the reader to assume
    # parity. It still cannot pass against a paraphrase: it reads the artifact that
    # ships, so deleting the advice reddens it.
    hook_src = open(HOOK).read()
    blocked_at = hook_src.find("COMMIT BLOCKED")
    if blocked_at < 0:
        failures.append("cannot find the COMMIT BLOCKED section in the shipped hook")
    else:
        tail = hook_src[blocked_at:]
        pathspec_at = tail.find("COMMIT ONLY YOUR OWN PATHS")
        # Anchor on strings that appear only in the EMITTED advice, never in a
        # comment. The first version of this cell searched for "git restore
        # --staged" and matched the explanatory comment above the code, which
        # made the ordering assertion measure prose instead of output.
        restore_at = tail.find("Or unstage theirs")
        if pathspec_at < 0:
            failures.append("the blocked remedy no longer offers a pathspec commit")
        elif restore_at < 0:
            failures.append("the blocked remedy no longer offers the unstage exit")
        elif pathspec_at > restore_at:
            failures.append(
                "the DESTRUCTIVE remedy is listed before the non-destructive one; "
                "`git restore --staged` edits the peer's staged work and should not "
                "be the first thing a blocked lane reaches for")
        # And the reason must travel with it. An unexplained ordering gets
        # 'tidied' back by the next person who thinks restore reads better first.
        # No arbitrary window: search from the pathspec advice to the end of the
        # unstage line. A fixed byte bound silently stops covering the text it
        # was chosen to cover as soon as anyone adds a comment above it, which
        # is what a [:4000] bound did on the first run of this cell.
        if "EDITS THE SHARED INDEX" not in tail[pathspec_at:restore_at + 400]:
            failures.append(
                "the restore remedy no longer says it edits the peer's index; "
                "without the reason, the ordering above is arbitrary and reversible")

    # AF-357: a staged DELETION from an area the commit does not otherwise touch.
    #
    # SPECIMEN, replayed exactly: 26c45798 deleted
    # crates/amux-server/migrations/0035_reclaim_skipped_hits_repair.sql inside a
    # commit whose other seven files are all under site/ and whose subject is
    # "feat(seo)". The removal was correct, so nothing broke; the cost is that
    # `git log` on that path now names a commit that cannot account for it.
    #
    # Measured before building: over the last 500 commits only TWO contain a
    # deletion at all, and this predicate fires on exactly one, the incident. So
    # the controls below matter more than the positive case, because a check this
    # rare is only worth having if it stays silent otherwise.
    incident = [
        ("D", "crates/amux-server/migrations/0035_reclaim_skipped_hits_repair.sql"),
        ("M", "site/AEO_BACKLOG.md"),
        ("M", "site/changelog/index.html"),
        ("A", "site/guides/splitting-work-across-ai-agents/index.html"),
    ]
    got = mod._orphan_deletions(incident)
    if [p for p, _ in got] != ["crates/amux-server/migrations/0035_reclaim_skipped_hits_repair.sql"]:
        failures.append(f"orphan_deletions missed the 26c45798 specimen: {got!r}")

    # CONTROL 1: a deletion IN the area the commit works in is ordinary. Without
    # this, a predicate that flagged every deletion would pass the case above.
    same_area = [
        ("D", "crates/amux-server/migrations/0035_old.sql"),
        ("M", "crates/amux-server/src/db/migrate.rs"),
    ]
    if mod._orphan_deletions(same_area):
        failures.append("a deletion inside the commit's own area must not fire")

    # CONTROL 2: a commit with NO deletions is silent.
    if mod._orphan_deletions([("M", "a/b.rs"), ("A", "a/c.rs")]):
        failures.append("a commit with no deletions must not fire")

    # CONTROL 3: a DELETION-ONLY commit is a deliberate removal, not a sweep.
    # This is the arm that would make the check obnoxious on a real cleanup.
    if mod._orphan_deletions([("D", "old/one.md"), ("D", "old/two.md")]):
        failures.append("a deletion-only commit is deliberate and must not fire")

    # The renderer must stay SILENT when there is nothing to say, for the same
    # reason as split_risk: a notice on the normal path is one people scroll past,
    # and it takes the real signal with it (AF-342).
    out = []
    mod._render_orphan_deletions(same_area, out.append)
    if out:
        failures.append(f"renderer must print NOTHING for an ordinary commit, got {out!r}")

    out = []
    mod._render_orphan_deletions(incident, out.append)
    txt = "".join(out)
    for needle, what in [
        ("0035_reclaim_skipped_hits_repair.sql", "the deleted path"),
        ("crates/", "the area nothing else touches"),
        ("git restore --staged", "the remedy"),
    ]:
        if needle not in txt:
            failures.append(f"orphan-deletion render omits {what} ({needle!r})")

    if failures:
        print(f"FAIL {len(failures)}:")
        for f in failures:
            print(" -", f)
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
