#!/usr/bin/env python3
# observed-edits PRE half (AF-123). Marks t0 before every Bash command so the
# POST half can report files whose mtime moved during it. Observed, not
# parsed: 75% of AF-27 staged-guard blocks hit lanes with firsthand=0, because
# bypass-permissions lanes are told to edit through Bash and no pathlike regex
# can see a write spelled inside a heredoc or aimed at an extensionless file
# (the specimen: a python heredoc rewriting `amux`).
#
# TRACKED SOURCE: scripts/claude-hooks/observed-edits-pre.py. Installed to
# ~/.amux/hooks/ and wired in ~/.claude/settings.json (PreToolUse, matcher
# "Bash"). Fail-open always: a hook that can block Bash fleet-wide must never
# have a failure mode of its own.
import os
import subprocess
import sys


def _derive_session_from_tmux():
    """Fallback identity for MR-43: $AMUX_SESSION can be empty inside a lane
    that IS running in its amux-launched pane (spawn always injects it —
    session_verbs.rs — so this is loss in-process, not absence at launch).
    Scoped to amux- prefixed panes, so a human's own tmux session (or no tmux
    at all) still resolves to "" and takes the existing no-op path.

    The tmux CALL FAILING is a different case from tmux CLEANLY SAYING "not an
    amux- pane" (amux-frustrations auditing MR-43, 2026-08-25): both used to
    return "" identically, so a real lane whose tmux call merely errored or
    timed out lost its PRE marker exactly like a human shell, with no trace to
    count. Logged to the same observed-edits.log the POST half writes, under
    session "UNKNOWN", so pre- and post-half failures of this kind land in one
    place.
    """
    try:
        name = subprocess.run(["tmux", "display-message", "-p", "#S"],
                              capture_output=True, text=True, timeout=3).stdout.strip()
    except Exception as e:
        _warn_derive_failed(e)
        return ""
    return name[len("amux-"):] if name.startswith("amux-") else ""


def _warn_derive_failed(exc):
    """Countable trace for the one case that must not be silent: the tmux call
    itself failed, which could be masking a real lane rather than confirming a
    human shell. Best-effort — a logging failure must never break the hook."""
    try:
        home = os.environ.get("AMUX_HOME") or os.path.expanduser("~/.amux")
        # The directory usually exists by the time this runs (a successful
        # derivation creates it on this very path below) — but the failure
        # this exists to catch can happen on the FIRST call in a fresh/broken
        # environment, exactly when it would not yet.
        state_dir = os.path.join(home, "hooks", "state")
        os.makedirs(state_dir, exist_ok=True)
        with open(os.path.join(state_dir, "observed-edits.log"), "a") as fh:
            import time as _time
            fh.write(f"{int(_time.time())} UNKNOWN tmux derivation failed "
                      f"({type(exc).__name__}: {exc}) - cannot tell a human "
                      "shell from a real lane\n")
    except Exception:
        pass


try:
    session = (os.environ.get("AMUX_SESSION") or "").strip()
    if not session:
        session = _derive_session_from_tmux()
    if session:
        state = os.path.join(
            os.environ.get("AMUX_HOME") or os.path.expanduser("~/.amux"),
            "hooks", "state")
        os.makedirs(state, exist_ok=True)
        # touch: the marker's MTIME is t0
        with open(os.path.join(state, f"observed-{session}.t0"), "w") as fh:
            fh.write("")
except Exception:
    pass
sys.exit(0)
