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

    if failures:
        print(f"FAIL {len(failures)}:")
        for f in failures:
            print(" -", f)
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
