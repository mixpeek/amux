#!/usr/bin/env python3
# observed-edits POST half (AF-123). After every Bash command: report files
# under the command's cwd whose mtime moved since the PRE marker, as OBSERVED
# edit records the staged-guard merges at firsthand rank. See the PRE half's
# header for why this exists (the firsthand=0 lane bias) and why observation
# beats parsing (heredocs and extensionless paths are invisible to a regex,
# never to an mtime).
#
# TRACKED SOURCE: scripts/claude-hooks/observed-edits-post.py. Installed to
# ~/.amux/hooks/ and wired in ~/.claude/settings.json (PostToolUse, matcher
# "Bash"). Fail-open always; hard wall-clock budget so a huge tree cannot
# slow every command. Writes one line per report to
# ~/.amux/hooks/state/observed-edits.log — the verify-by-what-it-WROTE marker
# (AMUX-2538's lesson: a hook that looks wired and never ran is invisible
# without one).
import json
import os
import ssl
import subprocess
import sys
import time
import urllib.request

PRUNE = {".git", "node_modules", "target", ".venv", "__pycache__", ".next", "dist"}


def _derive_session_from_tmux():
    """Fallback identity for MR-43: $AMUX_SESSION can be empty inside a lane
    that IS running in its amux-launched pane (spawn always injects it —
    session_verbs.rs — so this is loss in-process, not absence at launch).
    Scoped to amux- prefixed panes, so a human's own tmux session (or no tmux
    at all) still resolves to "" and takes the existing no-op path. Mirrors
    the PRE half's helper of the same name — kept duplicated rather than
    imported since every hook here is a standalone TRACKED SOURCE file.

    The tmux CALL FAILING is a different case from tmux CLEANLY SAYING "not an
    amux- pane", and conflating them was a real gap (amux-frustrations auditing
    MR-43, 2026-08-25): both returned "" identically, so a real lane whose tmux
    call merely errored or timed out vanished exactly like a human shell — the
    original MR-43 symptom (edit record missing, guard names a peer as sole
    editor) reproduced under a new cause, and silently, since the old return ""
    left no trace to count. That case logs a WARN before falling through to the
    same "" — a human shell (tmux succeeds, name isn't amux-*) still logs
    nothing, so this does not add a row for every ordinary command.
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
        # The directory usually exists by the time this runs (the PRE half
        # creates it on every successful derivation) — but the failure this
        # exists to catch can happen on the FIRST call in a fresh/broken
        # environment, exactly when it would not yet.
        os.makedirs(os.path.join(home, "hooks", "state"), exist_ok=True)
        log_line(home, "UNKNOWN",
                 f"tmux derivation failed ({type(exc).__name__}: {exc}) - "
                 "cannot tell a human shell from a real lane")
    except Exception:
        pass
MAX_PATHS = 80
FIND_BUDGET_S = 1.5

# AF-124: the walk observes EVERY moved mtime under cwd, including a peer's
# concurrent write — and a pure READ of one file must never claim another
# file's write at firsthand rank (amux-frustrations' live control: POST fired
# for `cat mine_untouched.rs` and claimed a peer's peer_file.rs). So the walk
# is gated on the COMMAND the way the inferred path already is: a command
# whose every segment is read-only reports nothing. This is a conservative
# PYTHON PORT of is_pure_read_command (git_guard.rs is canonical); drift is
# asymmetric by design — a verb the port misses stays reported (today's
# behavior), never silently unreported.
READ_ONLY_VERBS = {
    "ls", "cat", "head", "tail", "less", "more", "grep", "egrep", "fgrep",
    "rg", "ag", "wc", "stat", "file", "find", "cmp", "diff", "sort", "uniq",
    "cut", "column", "od", "xxd", "hexdump", "tree", "du", "basename",
    "dirname", "realpath", "readlink", "sha256sum", "md5sum", "nl", "tac",
    "pwd", "echo", "printf", "cd", "which", "type", "env", "sleep",
}
GIT_READ_SUBCMDS = {
    "show", "log", "diff", "status", "blame", "grep", "cat-file", "shortlog",
    "describe", "rev-parse", "rev-list", "ls-files", "ls-tree", "reflog",
    "whatchanged", "annotate", "name-rev", "show-ref", "for-each-ref",
}


def has_output_redirection(cmd):
    i, n = 0, len(cmd)
    while i < n:
        if cmd[i] == ">":
            j = i + 1
            if j < n and cmd[j] == ">":
                j += 1
            while j < n and cmd[j] in " \t":
                j += 1
            if j < n and cmd[j] != "&":
                return True
        i += 1
    return False


def is_pure_read_command(cmd):
    if has_output_redirection(cmd):
        return False
    saw = False
    for seg in __import__("re").split(r"[|;&\n()`]", cmd):
        seg = seg.strip()
        if not seg or seg.startswith("#"):
            # AF-126: a comment segment writes nothing and must not force the
            # command non-read (same fix as the rust canonical).
            continue
        saw = True
        tok = seg.split()[0] if seg.split() else ""
        verb = os.path.basename(tok)
        if verb == "git":
            rest = iter(seg.split()[1:])
            sub = None
            for t in rest:
                if t in ("-C", "-c"):
                    next(rest, None)
                    continue
                if t.startswith("-"):
                    continue
                sub = t
                break
            if sub in GIT_READ_SUBCMDS:
                continue
            return False
        if verb not in READ_ONLY_VERBS:
            return False
    return saw


# How many paths a single report names in the log. The count is authoritative
# and the tail says how many were elided, so a wide walk cannot flood the file
# while still leaving the claim auditable (AF-179).
LOG_PATHS = 12


def log_line(home, session, text):
    try:
        with open(os.path.join(home, "hooks", "state", "observed-edits.log"), "a") as fh:
            fh.write(f"{int(time.time())} {session} {text}\n")
    except Exception:
        pass


def main():
    session = (os.environ.get("AMUX_SESSION") or "").strip()
    if not session:
        session = _derive_session_from_tmux()
    if not session:
        return
    home = os.environ.get("AMUX_HOME") or os.path.expanduser("~/.amux")
    marker = os.path.join(home, "hooks", "state", f"observed-{session}.t0")
    try:
        t0 = os.stat(marker).st_mtime
    except OSError:
        return
    # Stale marker (no PRE fired, or > 30 min old command): report nothing
    # rather than attributing a peer's writes from a dead window.
    if time.time() - t0 > 1800:
        return
    try:
        d = json.load(sys.stdin)
    except Exception:
        return
    cwd = (d.get("cwd") or "").strip()
    if not cwd or not os.path.isdir(cwd):
        return
    # AF-124: a pure-read command claims nothing, whatever moved meanwhile.
    cmd = ((d.get("tool_input") or {}).get("command") or "")
    if cmd and is_pure_read_command(cmd):
        log_line(home, session, "n=0 pure-read")
        return

    deadline = time.monotonic() + FIND_BUDGET_S
    hits = []
    for root, dirs, files in os.walk(cwd):
        if time.monotonic() > deadline or len(hits) >= MAX_PATHS:
            break
        dirs[:] = [x for x in dirs if x not in PRUNE and not x.startswith(".cache")]
        for f in files:
            p = os.path.join(root, f)
            try:
                mt = os.stat(p).st_mtime
                if mt >= t0:
                    # AF-130: send the mtime we just read, not merely the path.
                    # This hook fires after the WHOLE Bash command, so for
                    # edit-and-commit in one compound call a hook-time stamp
                    # postdates the commit and the guard's SettledByOwner can
                    # never fire — the false at-risk notice on every such
                    # commit. The server accepts bare strings too (older
                    # installed copies), stamping those with its own clock.
                    hits.append({"path": p, "mtime": mt})
                    if len(hits) >= MAX_PATHS:
                        break
            except OSError:
                continue
    if not hits:
        # AF-124's fourth case: "ran and found nothing" must be
        # distinguishable from "never ran" (AMUX-2538) — the quiet path logs.
        log_line(home, session, "n=0 no-moved-mtimes")
        return

    url = os.environ.get("AMUX_URL") or "https://localhost:8824"
    try:
        with open(os.path.join(home, "endpoint.json")) as fh:
            ep = json.load(fh)
        stale = list(ep.get("retired_ports") or [])
        if ep.get("legacy_port"):
            stale.append(ep["legacy_port"])
        from urllib.parse import urlsplit
        sp = urlsplit(url)
        if sp.hostname in ("localhost", "127.0.0.1", "::1") and sp.port in stale:
            url = (ep.get("canonical_url") or url).rstrip("/")
    except Exception:
        pass
    try:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        req = urllib.request.Request(
            url.rstrip("/") + "/api/git/observed-edits",
            data=json.dumps({"paths": hits}).encode(),
            headers={"Content-Type": "application/json", "X-Amux-Session": session},
            method="POST")
        urllib.request.urlopen(req, timeout=2, context=ctx).read()
        outcome = "sent"
    except Exception as e:
        outcome = f"send-failed:{e.__class__.__name__}"
    # LOG WHAT WAS CLAIMED, NOT ONLY HOW MANY (AF-179). This said `n=3 sent`,
    # so the log built to verify this hook by what it WROTE could not say what
    # it wrote. When the guard named a session as co-editor of a file it had
    # never opened, the only way to establish which report carried the claim was
    # to reconstruct it from file mtimes by hand. A count is not an audit trail.
    # Paths are repo-relative and capped so a wide walk cannot flood the log;
    # the count stays authoritative and the tail says how many were elided.
    _rel = [os.path.relpath(h["path"], cwd) for h in hits]
    _shown = _rel[:LOG_PATHS]
    _more = "" if len(_rel) <= LOG_PATHS else f" +{len(_rel) - LOG_PATHS} more"
    log_line(home, session, f"n={len(hits)} {outcome} paths={','.join(_shown)}{_more}")


try:
    main()
except Exception:
    pass
sys.exit(0)
