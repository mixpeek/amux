#!/usr/bin/env python3
"""Say WHOSE work a workspace lint failure is (AF-182).

The pre-commit gate lints `--workspace --all-targets`, which is right: a denial
anywhere turns the whole workspace red for every lane on this shared checkout and
queues a CI failure for whoever pushes next. But it lints the WORKING TREE, so
another session's uncommitted work refuses YOUR commit, and clippy names their
file while nothing names them.

That output is TRUE about the repo and FALSE about the committer, and it cannot
say which was meant. Both natural readings are wrong: "I broke this" and "this
gate is noise". The second is the expensive one, because the cheap wrong move is
right there — fix the peer's file — and that is how a session ends up committing
another session's half-finished work.

This never changes the verdict. The caller prints clippy's raw output, calls
this, swallows its failure, and exits 1 regardless. A diagnostic aid that can
suppress a refusal is a worse bug than the one it explains.
"""
import os
import re
import subprocess
import sys

# `  --> crates/amux-server/src/x.rs:3620:9`. Clippy also emits `::: path` for
# secondary spans; both name a file, and for blame either is enough.
SPAN = re.compile(r"^\s*(?:-->|:::)\s+([^\s:][^:]*):\d+:\d+", re.M)


def git(*args):
    try:
        return subprocess.run(
            ["git", *args], capture_output=True, text=True, timeout=10
        ).stdout.split("\n")
    except Exception:
        return []


def main():
    out = os.environ.get("CLIPPY_OUT") or ""
    staged = {p.strip() for p in (os.environ.get("STAGED") or "").split("\n") if p.strip()}

    # Preserve first-seen order: the first diagnostic is the one a reader reads.
    offenders, seen = [], set()
    for m in SPAN.finditer(out):
        f = m.group(1).strip()
        if f and f not in seen:
            seen.add(f)
            offenders.append(f)
    if not offenders:
        # NOT "everything is fine" — it means the parse found no file spans, and
        # saying so is the difference between a silent aid and a broken one.
        print("\namux lint-blame: could not attribute this failure — no file spans "
              "in clippy's output. The refusal above stands.")
        return

    dirty = {p.strip() for p in git("diff", "--name-only") if p.strip()}
    mine = [f for f in offenders if f in staged]
    theirs = [f for f in offenders if f not in staged and f in dirty]
    onhead = [f for f in offenders if f not in staged and f not in dirty]

    n = len(offenders)
    print()
    if mine:
        # THE COUNT MATTERS AND IT IS THE HALF THAT GOES WRONG QUIETLY. "1 of 1
        # is not yours" and "3 of 4 are yours" are different situations, and a
        # partition that reports only the peer's share reads as exonerating.
        print(f"amux lint-blame: {len(mine)} of {n} offending file(s) ARE in your commit.")
        for f in mine:
            print(f"    {f}  (staged — yours to fix)")
    if theirs:
        if not mine:
            print("amux lint-blame: BLOCKED BY ANOTHER SESSION'S IN-FLIGHT WORK — not your commit.")
        else:
            print(f"  ...and {len(theirs)} of {n} is another session's in-flight work:")
        for f in theirs:
            print(f"    {f}  (unstaged in the worktree — NOT in your commit)")
        if not mine:
            print("    Your staged files are clean. Wait for them, or ask them to fix it.")
        print("    Do NOT fix their file to unblock yourself: committing it would sweep in "
              "work they are mid-way through, which is the class the staged-guard prevents.")
        print("    The staged-guard output earlier in this run names the sessions holding "
              "files you are touching.")
    if onhead:
        # Prefix when this is the ONLY branch that fires, or the line has no
        # visible source and a reader cannot tell which tool said it.
        lead = "" if (mine or theirs) else "amux lint-blame: "
        print(f"  {lead}{len(onhead)} of {n} is already broken on HEAD (not staged, not dirty) — "
              "someone committed a denial:")
        for f in onhead:
            print(f"    {f}")
    print("\n  The commit is refused either way: a red workspace is red for every lane, "
          "and CI denies warnings on the next push of main.")

    # NAME THE NARROW ESCAPE, but only when the failure is provably not yours
    # (AF-182, third instance 2026-08-26). Refusing without an exit is what sent
    # three commits through `--no-verify`, which ALSO drops the security scan,
    # the staged-guard, the append-only guard and the JS checks. The gate that
    # was doing real work is the one that gets disabled, because it is bundled
    # with the one that was wrong.
    #
    # This still does not change the verdict — the hook exits 1 regardless, and
    # a human or agent has to make the call. It replaces a dead end with a
    # precise alternative to the nuclear one. Deliberately silent when `mine` is
    # non-empty: printing an escape beside your OWN denial would be handing you
    # a false green.
    if not mine and (theirs or onhead):
        print("\n  If you are certain none of the offenders are yours, skip THIS gate only:")
        print("      AMUX_SKIP_RUST_GATE=1 git commit ...")
        print("  Every other gate still runs, and the skip prints itself in the commit output.")
        print("  Prefer that to --no-verify, which disables the security scan and the "
              "staged-guard as well.")
        # EXIT 10 = "attributed successfully, and NONE of the offenders are
        # staged" (AMUX-3726). The hook uses this to decide whether it is worth
        # building the INDEX to answer the question the tree cannot.
        #
        # A DISTINCT code, and only this one, because of the rule this aid is
        # already bound by: it must never be able to turn a refusal into a pass.
        # Every other outcome — 0, a crash, a missing interpreter, an unparseable
        # clippy dump — is NOT 10, and the hook treats not-10 as "refuse, exactly
        # as before". So a BROKEN aid degrades to today's behaviour and can only
        # cost a wait; it can never grant an allow.
        return 10


# The exit code carries the verdict; `main` returns 10 only when it positively
# established that no offender is staged. `or 0` keeps every other path at 0, so
# the hook's existing `|| true` semantics are unchanged for them.
import sys as _sys
_sys.exit(main() or 0)
