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

# Pruned for CORRECTNESS, not for speed (AMUX-3920, mixpeek-cicd's specimen).
#
# The speed argument does not survive measurement and I am recording that rather
# than quietly acting on it: broadening this set drops ~/Dev/mixpeek from 640,355
# files to 589,713, only 7.9%, and once the filesystem cache is warm the walk
# takes ~1.0s either way. My earlier 2.54s figure was a COLD walk; alternating
# the two arms three times gives 0.94/1.01s broadened against 1.04/1.08s current,
# i.e. noise.
#
# The reason to prune these is that a cache write is not a lane's edit. On
# 2026-08-30 mixpeek-cicd logged `n=3` in which TWO of the three recorded paths
# were `.pytest_cache` and `.ruff_cache` entries. Those become edit records, and
# an edit record is what the staged guard reads to decide who touched a file — so
# a test run mints attribution for files no guard should care about. `.cache`-
# prefixed names were already excluded; `.pytest_cache` and `.ruff_cache` are not
# `.cache`-prefixed, which is why they slipped through.
PRUNE = {
    ".git", "node_modules", "target", ".venv", "__pycache__", ".next", "dist",
    # Tool caches whose writes are churn, not authorship.
    ".pytest_cache", ".ruff_cache", ".mypy_cache", ".tox", ".turbo",
    ".parcel-cache", ".nyc_output", ".ipynb_checkpoints", ".gradle",
    # Vendored/derived trees.
    "site-packages", ".terraform", ".pnpm-store", ".yarn", ".worktrees",
}


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
    derived = not session
    if not session:
        session = _derive_session_from_tmux()
    if not session:
        return
    home = os.environ.get("AMUX_HOME") or os.path.expanduser("~/.amux")
    # ISOLATED WORKERS (AMUX-3232): if the session was derived (not injected),
    # check the session env file. Isolated workers have CC_ISOLATED=1 and must
    # not emit edit records — their file edits are not attributable to a fleet
    # lane and the staged-guard must not see them as such.
    if derived:
        sf = os.path.join(home, "sessions", f"{session}.env")
        try:
            with open(sf) as _f:
                if any("CC_ISOLATED" in ln and '"1"' in ln or "CC_ISOLATED=1" in ln
                       for ln in _f):
                    return
        except OSError:
            pass
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

    # WALK THE REPO, NOT ONLY THE SESSION'S CORNER OF IT (AMUX-3920, MHC-527).
    #
    # The harness reports the SESSION cwd. When that is a SUBDIRECTORY of a
    # shared checkout, every edit above it produced no record at all — not a
    # partial one, none. MHC-527's positive control, one Bash call writing two
    # files in one repo: `homepage/scripts/.probe` recorded, `scripts/.probe`
    # not, hook logged n=1.
    #
    # The cost is not friction. That session committed 16 paths, 13 above its
    # cwd, so 81% of the work was structurally unobservable — and the staged
    # guard, correctly reading "NO session has an edit record for this", asked
    # for AMUX_VERIFIED_SOLO on three commits and AMUX_ALLOW_FOREIGN on two.
    # Routinely overriding a guard that is right most of the time trains the
    # fleet to wave it through, and the next time it correctly names a peer's
    # work the muscle memory is already there.
    #
    # CWD FIRST, THEN THE REST OF THE REPO. Measured 2026-08-30 with this file's
    # own PRUNE set: amux is 2,222 files in 0.03s, but ~/Dev/mixpeek — the
    # monorepo this card is about — is 640,353 files in 2.54s against a 1.5s
    # budget. A naive widening therefore TRUNCATES on the repo it exists to fix,
    # and a truncated walk that starts at the repo root can run out of budget
    # before it ever reaches the session's own directory. That would trade a
    # known blind spot for an unpredictable one.
    #
    # Ordering the roots fixes it without touching the budget: cwd is walked
    # first, so the coverage that worked before is never lost, and the rest of
    # the repo is walked with whatever budget remains. Strictly more than before,
    # never less, and the shortfall is named rather than silent.
    # realpath BOTH roots. On macOS /var is a symlink to /private/var, so the
    # cwd the harness reports and the toplevel git prints can name the same
    # directory with different strings — and then the `seen` dedupe below misses
    # and every file under cwd is counted twice. Found by the n=3 in a two-file
    # control.
    _cwd_real = os.path.realpath(cwd)
    # WITHDRAWN: THE REPO-ROOT WALK (AMUX-3920 / AMUX-3933, reverted 2026-08-30).
    #
    # Widening the root beyond cwd is UNSOUND, and the coverage it bought and the
    # corruption it caused are the same operation. mtime says a file was written
    # in this window; it does not say BY WHOM. cwd was the only thing bounding
    # that smear, and removing it let every lane claim every other lane's
    # uncommitted files.
    #
    # Live specimen, mixpeek-homepage-claude, with the git enumeration that made
    # the widening complete:
    #   byo-ray n=2 sent src=git paths=FRUSTRATIONS.md,research/shared-checkout/...md
    # byo-ray's cwd is research/byo-ray/. The second path is another lane's file,
    # written 22s earlier and never touched by byo-ray. The staged guard reads
    # exactly these records to decide ownership, so this is worse than the blind
    # spot it fixed — the module's own doctrine already says cross-linking is
    # strictly worse than staying blind.
    #
    # Reverting the git enumeration alone was NOT enough; reproduced afterwards
    # with a two-lane scratch repo, laneA claiming laneB/peers.txt through the
    # os.walk widening. So the root goes back to cwd.
    #
    # WHAT SURVIVES from that work, because none of it depends on the wider root:
    # the TRUNCATED marker, the tool-cache prune, realpath'd roots, repo-relative
    # log paths. What does not survive is the coverage claim, and AMUX-3933 now
    # owns the honest version of the problem: a bound that is about SESSION
    # IDENTITY rather than about directory depth.
    walk_roots = [_cwd_real]
    walk_root = walk_roots[-1]

    deadline = time.monotonic() + FIND_BUDGET_S
    hits = []
    seen = set()
    # A TRUNCATED WALK MUST NOT LOOK LIKE A CLEAN ONE (ethos rule 4). Widening
    # the root makes the budget bite where it never did, and a walk that stops
    # early reports fewer paths with no sign it stopped — the same blindness this
    # card fixes, wearing a different hat. On the mixpeek measurement above this
    # is the EXPECTED path, not a corner case, so it has to be legible: a reader
    # must be able to tell "found 3" from "found 3 so far".
    truncated = ""
    for _wr in walk_roots:
        if truncated:
            break
        for root, dirs, files in os.walk(_wr):
            if time.monotonic() > deadline:
                truncated = " TRUNCATED=budget"
                break
            if len(hits) >= MAX_PATHS:
                truncated = " TRUNCATED=cap"
                break
            dirs[:] = [x for x in dirs if x not in PRUNE and not x.startswith(".cache")]
            for f in files:
                p = os.path.join(root, f)
                # The second root CONTAINS the first, so dedupe. Without this a
                # file under cwd is reported twice and n= overcounts.
                if p in seen:
                    continue
                seen.add(p)
                try:
                    mt = os.stat(p).st_mtime
                    if mt >= t0:
                        # AF-130: send the mtime we just read, not merely the
                        # path. This hook fires after the WHOLE Bash command, so
                        # for edit-and-commit in one compound call a hook-time
                        # stamp postdates the commit and the guard's
                        # SettledByOwner can never fire — the false at-risk
                        # notice on every such commit. The server accepts bare
                        # strings too (older installed copies), stamping those
                        # with its own clock.
                        hits.append({"path": p, "mtime": mt})
                        if len(hits) >= MAX_PATHS:
                            # MARK IT HERE, not only at the top of the next
                            # directory. Hitting the cap inside the LAST
                            # directory used to break out with no marker, so a
                            # capped run and a complete one printed the same
                            # line — the exact ambiguity the marker exists to
                            # remove, surviving in the one case where the cap
                            # actually bit.
                            truncated = " TRUNCATED=cap"
                            break
                except OSError:
                    continue
    if not hits:
        # AF-124's fourth case: "ran and found nothing" must be
        # distinguishable from "never ran" (AMUX-2538) — the quiet path logs.
        # `truncated` matters MOST here: "walked the whole tree and nothing
        # moved" and "ran out of budget before reaching anything" are opposite
        # facts and both used to print n=0.
        log_line(home, session, f"n=0 no-moved-mtimes{truncated}")
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
    # Relative to the WALK ROOT, not the session cwd (AMUX-3920). The paths SENT
    # to the server are absolute and unaffected, but once the walk root moved
    # above cwd this line rendered every hit as `../../../../..` — the record was
    # correct and unreadable, which for an observability hook is most of the
    # defect. Repo-relative also matches what `git status` prints.
    _rel = [os.path.relpath(h["path"], walk_root) for h in hits]
    _shown = _rel[:LOG_PATHS]
    _more = "" if len(_rel) <= LOG_PATHS else f" +{len(_rel) - LOG_PATHS} more"
    log_line(home, session, f"n={len(hits)} {outcome}{truncated} paths={','.join(_shown)}{_more}")


try:
    main()
except Exception:
    pass
sys.exit(0)
