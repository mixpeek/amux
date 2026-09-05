#!/usr/bin/env python3
"""amux PreToolUse guard — block IRREVERSIBLE tree-wide git in SHARED checkouts.

A shared checkout (e.g. ~/Dev/mixpeek) holds many sessions' uncommitted work in
one tree. A tree-wide discard sweeps up or destroys EVERY session's work (two
data-loss incidents 2026-07-02). We block only the irreversible discards;
path-scoped git and recoverable ops (bare stash, pull --rebase --autostash) pass.

Matching is scoped to ACTUAL git invocations: quoted-string contents are stripped
before matching, so a subcommand merely MENTIONED in prose/JSON (e.g. a curl body)
never trips the guard.

Owner-sanctioned override: write the exact authorized command to
~/.amux/guard-allow-once (after owner sign-off). The next matching invocation is
allowed once, the marker is consumed, and the override is audit-logged. This
gives authorized cleanups a clean path instead of training reflog evasion.

Configure guarded roots via AMUX_SHARED_CHECKOUTS (colon-separated; default
~/Dev/mixpeek). Fail-open: any error lets the command through.
"""
import sys, json, os, re, time, pathlib

# GIT'S GLOBAL-OPTION PREFIX, shared by every detector that must find a
# SUBCOMMAND in command text (AF-489).
#
# mixpeek-frustrations fixed the run_dir resolver on 2026-09-04 (2815f442) after
# `-C` was read as git's global `-C <dir>` when it was `git commit -C <commit>`,
# and handed the rest of the file over. Two DETECTORS had the mirror defect: they
# allowed only `' + GIT_GLOBALS + r'` before the subcommand, so any OTHER global flag
# hid the subcommand and the guard never fired at all. Measured before the fix:
#
#   git -c user.name=x commit --amend         amend detector MISS
#   git -c a=b -c c=d commit --amend          MISS
#   git --no-pager commit --amend             MISS
#   git -c a=b -C /repo commit --amend        MISS
#   git -c protocol.version=2 checkout -- .   discard detector MISS
#   git --literal-pathspecs checkout -- .     MISS
#
# A resolver that mis-resolves gives a wrong answer. A DETECTOR that misses is a
# SILENT PASS, which is the worse direction and the one this closes.
#
# Arg-taking globals are enumerated because they consume the next token; every
# other leading dash-token is a no-arg global. The group stops at the first bare
# word, which is git's own rule for where the subcommand begins, so `git log
# --oneline` and `git diff -C -- a b` still do not match.
GIT_GLOBALS = (
    r'(?:'
    r'-(?:c|C)\s+\S+\s+'
    r'|--(?:exec-path|git-dir|work-tree|namespace|super-prefix)(?:=\S+|\s+\S+)\s+'
    r'|--?[A-Za-z][-A-Za-z0-9]*\s+'
    r')*'
)

# The same prefix MINUS `-C`, for the resolver that must CAPTURE the `-C <dir>`.
# GIT_GLOBALS contains a `-C` arm, so reusing it there lets the prefix eat the
# very flag being captured. The lookahead keeps a bare `-C` out of the no-arg arm
# for the same reason.
# A SUBCOMMAND FLAG BETWEEN THE VERB AND ITS OBJECT (AF-490). The bare-stash
# rule's negative lookahead required the recovery verb IMMEDIATELY after `stash`,
# so `git stash --quiet pop` and `git stash -q apply` were REFUSED. Pre-existing,
# measured against the unfixed copy, and the worst direction this guard has: a
# false refusal on `pop`, the one verb people reach for to RECOVER work they
# thought they had lost. The lookahead now steps over `-flag` tokens.
GIT_GLOBALS_NOT_C = (
    r'(?:'
    r'-c\s+\S+\s+'
    r'|--(?:exec-path|git-dir|work-tree|namespace|super-prefix)(?:=\S+|\s+\S+)\s+'
    r'|--?(?!C\b)[A-Za-z][-A-Za-z0-9]*\s+'
    r')*'
)

# Entries are (pattern, why) or (pattern, why, remedy).
#
# The 3-tuple exists because the shared refusal tail below hard-codes ONE hazard
# model: "this discards or sweeps up EVERY session's uncommitted work — scope to
# YOUR OWN paths instead". That is true of every rule here except the history
# ones, and telling someone whose `git fetch --depth=1` was blocked to scope it
# with `git stash push -- <yourpath>` is advice that cannot be followed for a
# problem they do not have. A guard that prints an impossible remedy teaches
# people to stop reading it. When a rule supplies a remedy, it replaces that
# paragraph rather than being appended to it.
DANGER = [
    (r'\bgit\s+' + GIT_GLOBALS + r'reset\s+--hard\b',
     'git reset --hard — discards ALL uncommitted tracked changes tree-wide'),
    # 2026-07-05: a MIXED `git reset HEAD~2` slipped this guard (not --hard) and
    # decapitated another session's two PUSHED commits from the shared branch
    # (content survived only as staged residue). ANY reset that moves shared
    # HEAD — bare/--soft/--mixed/--keep/--merge/<commit-ish> — rewrites every
    # session's branch state. Only the explicit path form (` -- <paths>`) is
    # safe, and it is the only form allowed through.
    (r'\bgit\s+' + GIT_GLOBALS + r'reset\b(?![^;&|\n]*\s--\s)',
     "git reset (HEAD-moving or bare) — moves/unstages the SHARED branch state for every "
     "session (a mixed `reset HEAD~N` decapitates other sessions' commits). Unstage only "
     "your paths with `git reset -- <your files>`; never move shared HEAD"),
    (r'\bgit\s+' + GIT_GLOBALS + r'checkout\s+(?:\S+\s+)?--\s+\.(?=\s|$|[;&|])',
     'git checkout -- . — discards ALL working-tree changes'),
    (r'\bgit\s+' + GIT_GLOBALS + r'checkout\s+\.(?=\s|$|[;&|])',
     'git checkout . — discards ALL working-tree changes'),
    (r'\bgit\s+' + GIT_GLOBALS + r'restore\s+(?:--\S+\s+)*\.(?=\s|$|[;&|])',
     'git restore . — discards ALL working-tree changes'),
    (r'\bgit\s+' + GIT_GLOBALS + r'clean\s+-[a-wyz]*f',
     'git clean -f — deletes untracked files tree-wide'),
    (r'\bgit\s+' + GIT_GLOBALS + r'stash\s+(?:drop|clear)\b',
     "git stash drop/clear — permanently discards a stash that may hold OTHER sessions' work"),
    # 2026-07-07 CASE 21 (fleet reset incident): bare/un-scoped `git stash` was
    # deliberately allowed as "recoverable" — invalidated in practice: stash
    # internally `reset --hard`s the WHOLE shared tree (the reflog "reset:
    # moving to HEAD" signature), sweeping every session's uncommitted work;
    # a conflicted pop strands the sweep and mid-flight readers see a wiped
    # tree. Allow only pathspec-scoped pushes + non-destructive subcommands.
    (r'\bgit\s+' + GIT_GLOBALS + r'stash\b(?!(?:\s+-\S+)*\s+(?:pop\b|apply\b|list\b|show\b|branch\b|drop\b|clear\b|push\b[^;&|\n]*\s--\s))',
     "bare/un-scoped git stash — internally reset --hards the WHOLE shared tree "
     "(sweeps every session's uncommitted work; a conflicted pop strands it). "
     "Scope it: `git stash push -- <your paths>`"),
    # Shared INDEX hazard: `-a`/`--all` stages+commits every modified tracked file
    # in the one shared tree, sweeping up other sessions' unstaged edits into your
    # commit (wrong-attribution incidents). Bare `git commit` (no -a) is NOT blocked
    # here — too frequent to gate fleet-wide — but the fix is the same: name paths.
    (r'\bgit\s+' + GIT_GLOBALS + r'commit\b[^\n;&|]*?(?:\s--all\b|\s-[a-zA-Z]*a[a-zA-Z]*(?=[\s;&|]|$))',
     'git commit -a/--all — commits EVERY modified tracked file in this SHARED tree, '
     'sweeping up other sessions\' edits; commit only your paths: `git commit -m "msg" -- <your files>`'),
    # THE SHARED INDEX, staged half (AF-316). `git commit -a` is blocked above;
    # `git add -A` / `git add .` reach the SAME hazard one step earlier and were
    # not covered. They stage every modified file in the one shared tree, so a
    # peer's in-flight edit becomes YOURS to commit — and it poisons the index
    # for everyone else too, because the next lane's plain `git commit` takes
    # whatever is staged.
    #
    # Largest open frustration class: 9 open `attribution` entries plus
    # `shared-checkout`, all one structural fact. Live instances: a peer's
    # `git add` sweeping an uncommitted migration into someone else's commit
    # (AMUX-2647); a commit shipping another lane's staged work under its own
    # message (DESKT-22); a graft from a stale index silently reverting two
    # landed changes (backend 2026-08-29, MC-1441).
    #
    # TWO RULES, because one regex could not keep `-A -- <path>` legal.
    # `git add -A -- src/foo.rs` is SCOPED and must pass: the flag is bounded by
    # the pathspec. Only the unbounded forms are the hazard.
    (r'\bgit\s+' + GIT_GLOBALS + r'add\b(?![^;&|\n]*\s--\s+\S)[^\n;&|]*?'
     r'(?:\s-A\b|\s--all\b|\s--no-ignore-removal\b)',
     'git add -A/--all — stages EVERY modified file in this SHARED checkout, '
     'including other sessions\' in-flight edits, and leaves them staged for the '
     'next lane\'s commit too. Name your own paths: `git add <your files>`, or '
     'bound the flag: `git add -A -- <your dir>` (AF-316; `git add -p` passes)'),
    # A bare `.` pathspec, with or without `--`. `git add -- .` is the same
    # command as `git add .` and would otherwise read as "scoped" to the rule
    # above — the obvious next thing to type after being refused once.
    # `git add ./src/foo.rs` is a real path and is NOT matched.
    (r'\bgit\s+' + GIT_GLOBALS + r'add\b[^\n;&|]*?\s(?:--\s+)?\.(?=[\s;&|]|$)',
     'git add . — stages EVERY modified file under this directory in a SHARED '
     'checkout, including other sessions\' in-flight edits. Name your own paths: '
     '`git add <your files>` (AF-316)'),
    # HISTORY TRUNCATION (AMUX-3893, tuple supplied and pre-tested by mixpeek-cicd).
    #
    # 2026-08-29 20:19 ET: something ran a depth-limited fetch against the shared
    # ~/Dev/mixpeek. `git rev-list --count origin/main` fell from ~38,700 to 50 and
    # a 15:41 commit became a parentless root. For four hours every lane asking "is
    # fix X in sha Y" from that tree got a wrong NO for anything older than that
    # afternoon — with no error and no output — while GitHub's compare said
    # ahead=163/141/318 for the same three pairs (tubescience, TUBES-2339).
    #
    # The caller is still unknown, and that is exactly why this belongs in the
    # guard: nothing in the repo does this (scripts/, .githooks/, server/scripts/
    # and tools/ were grepped; the only local --depth is a `clone --depth=1` of an
    # EXTERNAL repo into its own directory, which cannot shallow this checkout). So
    # it came from a session, and the guard is the only layer that sees those.
    #
    # The same trap hit CI independently the same day: a
    # `git fetch -q --depth=1 origin <sha> <sha>` followed by `merge-base
    # --is-ancestor` produced a false "REVERT DETECTED" for hours (MG-1532). The
    # fetch added to guarantee the commits were present is what removed the
    # ancestors the walk needed.
    #
    # Why this is a guard rule rather than a fix at one consumer: mixpeek-cicd
    # already made `scripts/graft-push.sh` refuse to run on a shallow repo (leg 23,
    # 4de6dbeb8e). That is detection at ONE consumer. It does not stop the next
    # depth-limited fetch and does not help the other ~49 lanes doing ancestry by
    # hand.
    #
    # SCOPE, each part deliberate and independently re-tested here (8 block / 10
    # pass, zero false positives):
    #   * fetch|pull only. `clone --depth` creates a NEW repo and cannot shallow
    #     this one; blocking it false-positives on real callers.
    #   * `--unshallow` and `--deepen` stay allowed — they are the remedy, and
    #     "deepen" does not contain "depth" so there is no overlap.
    #   * `[^;&|\n]*?` keeps the match inside one command, like the tuples above.
    (r'\bgit\s+' + GIT_GLOBALS + r'(?:fetch|pull)\b[^;&|\n]*?\s(?:--depth[=\s]|--shallow-since\b|--shallow-exclude\b)',
     "git fetch/pull --depth (or --shallow-since/--shallow-exclude) — truncates history in "
     "this SHARED checkout, and every `merge-base --is-ancestor` past the cut then returns a "
     "bare exit 1 with no error, which is indistinguishable from a real 'not an ancestor'",
     "Fetch fully (`git fetch origin`), or heal an already-shallow tree with "
     "`git fetch --unshallow origin`. Check with `git rev-parse --is-shallow-repository`. "
     "This does NOT touch anyone's uncommitted work — it truncates shared HISTORY, so "
     "scoping to your own paths is not the remedy here."),
]
_ALLOW_ONCE = pathlib.Path.home() / ".amux" / "guard-allow-once"
_AUDIT = pathlib.Path.home() / ".amux" / "logs" / "guard-overrides.jsonl"

_HEREDOC_INTRO = re.compile(r'<<-?\s*([\'"]?)(\w+)\1')
# A heredoc whose intro line feeds a SHELL is executable content — its body must
# still be scanned (bash <<EOF ... git reset --hard ... EOF really runs). Only
# non-executable sinks (cat/tee/python/etc.) get their bodies stripped.
_SHELL_SINK = re.compile(r'\b(?:ba|z|da|k)?sh\b|\beval\b|\bsource\b')

def amux_base_url():
    """Where to reach the amux server, self-healing a STALE INHERITED PORT.

    This hook is a PreToolUse hook: it runs once per Bash tool call, as a child
    of the `claude` process, and so inherits that process's `AMUX_URL`. ~55 live
    sessions predate the 8822 -> 8824 port cutover and carry
    `AMUX_URL=https://localhost:8822` in a process env that cannot be rotated
    without killing them, which made this file the single busiest caller of the
    retired port on the machine (~743 req/h). Editing the DEFAULT below could
    never have helped: a default only fires when the variable is UNSET.

    `~/.amux/endpoint.json` is written by the amux server itself at boot, so
    neither port is hardcoded here (they are REVERSED in the cloud image). Only
    a localhost URL sitting on the port the server itself calls retired is
    swapped; a deliberate dev port or a remote amux is left alone. Missing file
    -> previous behaviour exactly.

    Mirrors amux:scripts/git-hooks/amux-staged-guard. This file is NOT tracked
    in the amux repo, which is why the same helper exists twice; see the
    frustrations entry (AREA: hooks).
    """
    import json as _json, os as _os
    url = (_os.environ.get("AMUX_URL") or "").strip().rstrip("/")
    home = _os.environ.get("AMUX_HOME") or _os.path.expanduser("~/.amux")
    try:
        with open(_os.path.join(home, "endpoint.json")) as fh:
            ep = _json.load(fh)
        legacy, canonical = ep.get("legacy_port"), (ep.get("canonical_url") or "").rstrip("/")
        if legacy and canonical:
            from urllib.parse import urlsplit
            sp = urlsplit(url)
            if sp.hostname in ("localhost", "127.0.0.1", "::1") and sp.port == legacy:
                return canonical
    except Exception:
        pass
    return url or "https://localhost:8824"


def _strip_heredoc_bodies(cmd):
    """MI-4083 (2026-07-05): remove heredoc BODIES so documentation text that
    merely MENTIONS a guarded git command (run-log blocks, memory notes, commit
    recipes) never pattern-matches. The intro line is kept (it is the real
    command); bodies feeding a shell interpreter are kept too (executable).

    AMUX-3932: an UNQUOTED delimiter is also executable, whatever the sink.
    `python3 <<EOF` expands $(...) and backticks in the body before python ever
    sees it -- verified against bash, which prints the EXPANDED value for <<EOF
    and the literal text for <<'EOF'. Only the substitution bodies are kept, on
    the same reasoning as the quoted-string path: keeping the whole body would
    refuse a heredoc that merely mentions a command, which is what this function
    exists to allow.
    """
    lines = cmd.split("\n")
    out, i = [], 0
    while i < len(lines):
        line = lines[i]
        out.append(line)
        intros = [(m.group(1), m.group(2)) for m in _HEREDOC_INTRO.finditer(line)]
        i += 1
        if intros and _SHELL_SINK.search(line):
            continue  # executable heredoc -- leave the body in place for scanning
        for quote, tag in intros:
            body = []
            while i < len(lines) and lines[i].strip() != tag:
                body.append(lines[i])  # dropped from output, kept for inspection
                i += 1
            if not quote:
                # <<EOF (unquoted): bash EXPANDS the body. Inert prose still goes,
                # but anything it would RUN is surfaced for matching.
                subs = _substitutions("\n".join(body))
                if subs:
                    out.append(" ; ".join(subs))
            if i < len(lines):
                out.append(lines[i])  # keep the terminator (inert)
                i += 1
    return "\n".join(out)

def _substitutions(text, _depth=0):
    """The parts of `text` bash will EXECUTE: $(...) and backtick bodies.

    AMUX-3932. Stripping quoted regions is RIGHT -- it is what stops a card whose
    description merely mentions a guarded command from being refused. The defect
    was that a body-stripper cannot tell an inert quoted string from one bash
    will expand: a double-quoted region and a single-quoted one look alike to it
    and are opposite facts to a shell.

    So the discriminator is a property of the QUOTING, which is knowable here,
    rather than of the surrounding command name, which the guard used to key on.

    Returns only the SUBSTITUTION BODIES, never the whole region, because only
    those execute. Keeping the whole region would refuse
    `amux board add "mentions git stash, ran $(date)"` -- banning mentions is
    exactly the noise the stripper exists to prevent.

    Bodies are scrubbed RECURSIVELY, so a substitution containing its own quoted
    region has that region judged by the same rule instead of being matched as
    prose. Bodies get strictly shorter, so this terminates; the depth cap is belt
    and braces against a pathological input.
    """
    if _depth > 8:
        return [text]
    out = []
    i, n = 0, len(text)
    while i < n:
        if text[i] == "\\" and i + 1 < n:
            i += 2
            continue
        if text.startswith("$(", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == "(":
                    depth += 1
                elif text[j] == ")":
                    depth -= 1
                j += 1
            body = text[i + 2 : j - 1] if depth == 0 else text[i + 2 :]
            out.append(_scrub_quotes(body, _depth + 1))
            i = j
            continue
        if text[i] == "`":
            j = text.find("`", i + 1)
            body = text[i + 1 : j] if j > 0 else text[i + 1 :]
            out.append(_scrub_quotes(body, _depth + 1))
            i = (j + 1) if j > 0 else n
            continue
        i += 1
    return out


def _scrub_quotes(s, _depth=0):
    """Remove INERT quoted text; keep what bash would execute.

    A left-to-right scanner rather than two independent regex passes. The old
    re.sub for single quotes ran over the WHOLE string, so it also stripped
    single quotes sitting INSIDE a double-quoted region -- where bash treats them
    as literal characters, not quoting operators.

    That is why a python3 -c body using triple-single-quotes around a backticked
    command was ALLOWED while the same body using $( ) was BLOCKED: the two
    substitution syntaxes were being handled in different places, which is the
    bug in miniature (mixpeek-homepage-claude's matrix, AMUX-3932). The blocked
    row was blocked by accident -- escaped inner quotes happened to desync the
    regex -- not by design.

    Quoting state has to be tracked to get this right, so it is tracked.
    """
    out = []
    i, n = 0, len(s)
    while i < n:
        c = s[i]
        if c == "\\" and i + 1 < n:
            out.append(s[i : i + 2])
            i += 2
            continue
        if c == "'":
            # Single quotes are INERT in bash: no expansion of any kind.
            j = s.find("'", i + 1)
            out.append(" ")
            i = (j + 1) if j >= 0 else n
            continue
        if c == '"':
            j = i + 1
            while j < n:
                if s[j] == "\\" and j + 1 < n:
                    j += 2
                    continue
                if s[j] == '"':
                    break
                j += 1
            inner = s[i + 1 : j] if j < n else s[i + 1 :]
            subs = _substitutions(inner, _depth)
            # Prose is dropped; only what bash would RUN survives. The separator
            # keeps two substitutions from fusing into one token.
            out.append(" " + " ; ".join(subs) + " " if subs else " ")
            i = (j + 1) if j < n else n
            continue
        out.append(c)
        i += 1
    return "".join(out)


def _scrub(cmd):
    # strip heredoc bodies FIRST (their intro quotes must still be visible to the
    # tag matcher), then quoted-string contents -- so a subcommand merely
    # mentioned in prose/JSON/docs isn't matched. What a shell would EXPAND
    # inside those quotes survives (AMUX-3932).
    return _scrub_quotes(_strip_heredoc_bodies(cmd))

# How long a CONSUMED authorization still answers for the SAME command string
# (MR-101). Seconds. 0 disables the replay window and restores strict one-shot.
#
# SIZED FROM THE MEASURED GAP, not guessed. `_consume_override` has FOUR call
# sites in this file, all straight-line in main(), and an unreachable amux server
# sets `discard_why` at more than one of them — so one process reaches the
# authorization check several times, separated by however long each co-tenancy
# probe takes to give up. Measured here against a refused connection (the fast
# failure): 4.4s between the first branch and the second. The reported incident
# was a TimeoutError, which is much slower, and AC-287's retry loop sleeps up to
# ~6s on top.
#
# 60 covers that with room and is still bounded: the expiry is a tested control,
# so a one-off cannot become standing permission. The risk it accepts is narrow —
# re-allowing THE SAME command string the owner already sanctioned, within a
# minute, and every replay is audited with `replay_of`.
_ALLOW_REPLAY_S = float(os.environ.get("AMUX_GUARD_ALLOW_REPLAY_S", "60") or 0)


def _authorization_matches(want, cmd):
    """Does the marker text `want` authorize `cmd`?

    ONE predicate, used by both the marker path and the replay path below. Two
    copies would let a command be authorized by the marker and then refused on
    replay by a subtly different rule, which is the bug MR-101 reports wearing a
    different cause.
    """
    if not want:
        return False
    return want in cmd or " ".join(want.split()) in " ".join(cmd.split())


def _recently_authorized(cmd):
    """Was this exact command authorized and consumed moments ago?

    MR-101: mixpeek-research saw one tool call produce BOTH "ALLOWED once" and
    "BLOCKED", with the marker gone and the consumption in the audit log, and
    git never running. The discard branch is straight-line code evaluated once
    per process, so one process cannot print both — the hook ran TWICE for one
    tool call. The first run consumed the marker and allowed; the second found
    no marker and blocked, and the block is what the tool call returned.

    So a consumed authorization has to keep answering for a moment. The evidence
    needed is already durable: every consumption is appended to the audit log
    with its timestamp and the authorized text, which is the same record that
    proved the double-invocation. Reading it back costs no new state and makes
    that log load-bearing, so it cannot quietly stop being written.

    Deliberately keyed on the AUTHORIZED TEXT rather than on "any recent
    override": a different destructive command inside the window gets nothing.
    """
    if _ALLOW_REPLAY_S <= 0:
        return None
    try:
        if not _AUDIT.exists():
            return None
        # Tail only — this file is append-only and grows forever.
        with open(_AUDIT, "rb") as f:
            try:
                f.seek(-65536, os.SEEK_END)
            except OSError:
                f.seek(0)
            tail = f.read().decode("utf-8", "replace")
        now = time.time()
        for line in reversed(tail.splitlines()):
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except Exception:
                continue
            ts = rec.get("ts") or 0
            if now - ts > _ALLOW_REPLAY_S:
                # Records are append-only and time-ordered, so the first one
                # outside the window ends the search.
                return None
            if _authorization_matches(rec.get("authorized") or "", cmd):
                return rec
    except Exception:
        pass
    return None


def _consume_override(cmd):
    """If an owner-sanctioned marker matches this command, consume it (one-time),
    audit-log, and allow. Returns True to allow, False to keep blocking."""
    try:
        if _ALLOW_ONCE.exists():
            want = _ALLOW_ONCE.read_text().strip()
            if _authorization_matches(want, cmd):
                _ALLOW_ONCE.unlink()  # one-time use
                _audit_override({"ts": time.time(), "authorized": want, "command": cmd[:600]})
                return True
    except Exception:
        pass
    # The marker is gone or does not match. Before blocking, ask whether THIS
    # command was authorized moments ago — a re-invocation of the same tool call
    # must not be refused by the consumption its own first invocation performed.
    prior = _recently_authorized(cmd)
    if prior is not None:
        _audit_override({
            "ts": time.time(),
            "authorized": prior.get("authorized"),
            "command": cmd[:600],
            # Distinct field, so "allowed twice" is greppable rather than
            # indistinguishable from two separate owner authorizations.
            "replay_of": prior.get("ts"),
            "replay_window_s": _ALLOW_REPLAY_S,
        })
        sys.stderr.write(
            "amux guard: allow-once REPLAYED (%.1fs after it was consumed) — same command "
            "string, same authorization. The hook ran more than once for one tool call; "
            "refusing the second run would block the command the owner sanctioned (MR-101).\n"
            % (time.time() - (prior.get("ts") or time.time()))
        )
        return True
    return False


def _audit_override(rec):
    """Append one override record. Best-effort, like the original inline write:
    an audit failure must not turn an ALLOW into a BLOCK."""
    try:
        _AUDIT.parent.mkdir(parents=True, exist_ok=True)
        with open(_AUDIT, "a") as f:
            f.write(json.dumps(rec) + "\n")
    except Exception:
        pass


# Commands that take the index lock. `push`, `log`, `show` and friends are absent
# on purpose: they do not write the index, so a lock is irrelevant to them and a
# note there is noise.
_INDEX_WRITERS = ("commit", "add", "stash", "reset", "checkout", "restore",
                  "rm", "mv", "merge", "rebase", "pull", "am", "cherry-pick",
                  "revert", "apply", "read-tree", "update-index")


def _lock_holder(lock):
    """(verdict, detail) for whether anything holds `lock` open.

    NEVER returns "nobody" from a probe that could not run. mixpeek-frustrations
    lost ten minutes to exactly that: `lsof <file> 2>/dev/null || echo no holder`
    printed the reassuring branch because `lsof` IS NOT ON PATH on this box and
    the `command not found` went to the suppressed stderr. A negative from an
    absent tool reads identically to a negative from a working one.

    So: absolute path, and a POSITIVE CONTROL. If lsof cannot see this very
    process, it cannot see anything, and the answer is `unmeasured`.
    """
    import subprocess
    # AMUX_LSOF exists so the ABSENT-TOOL branch is reachable in a test. It is the
    # branch that decides whether a future reaper deletes live locks on a box
    # without lsof, and on a box that HAS lsof there is no other way to reach it.
    # Same idiom as AMUX_SHARED_CHECKOUTS and AMUX_AMEND_EXPECT elsewhere here.
    cands = [os.environ["AMUX_LSOF"]] if os.environ.get("AMUX_LSOF") else [
        "/usr/sbin/lsof", "/usr/bin/lsof", "/bin/lsof"]
    exe = next((c for c in cands if os.path.exists(c)), None)
    if exe is None:
        return ("unmeasured", "lsof not found; holder unknown, NOT unheld")
    try:
        ctl = subprocess.run([exe, "-p", str(os.getpid())],
                             capture_output=True, text=True, timeout=5)
        if not (ctl.stdout or "").strip():
            return ("unmeasured", "lsof produced nothing for this very process, "
                                  "so a negative on the lock means nothing")
        out = subprocess.run([exe, "--", lock],
                             capture_output=True, text=True, timeout=5)
        holders = [l for l in (out.stdout or "").splitlines()[1:] if l.strip()]
        if holders:
            return ("held", holders[0][:120])
        return ("unheld", "no process holds it open")
    except Exception as e:
        return ("unmeasured", f"holder probe failed: {e}")


def _index_lock_note(cmd, run_dir):
    """A verdict on .git/index.lock for a command that would take it."""
    if not re.search(r"\bgit\s+" + GIT_GLOBALS + r"(?:" + "|".join(_INDEX_WRITERS) + r")\b", cmd):
        return ""
    import subprocess
    try:
        gd = subprocess.run(["git", "-C", run_dir, "rev-parse", "--absolute-git-dir"],
                            capture_output=True, text=True, timeout=5).stdout.strip()
    except Exception:
        return ""
    lock = os.path.join(gd, "index.lock") if gd else ""
    if not lock or not os.path.exists(lock):
        return ""
    try:
        st = os.stat(lock)
    except Exception:
        return ""
    age = int(time.time() - st.st_mtime)
    verdict, detail = _lock_holder(lock)
    # SIZE IS THE SHARPEST SIGNAL AND IT IS FREE. git writes the new index INTO
    # the lock and then renames, so a LIVE writer's lock grows. Zero bytes with a
    # static mtime is the stale shape.
    shape = ("0 bytes and not growing, which is the STALE shape"
             if st.st_size == 0 else f"{st.st_size} bytes, so a writer has been filling it")
    head = ("amux guard: NOTE — .git/index.lock exists and your command writes the index.\n")
    body = (f"  age {age}s · {shape} · holder: {verdict} ({detail})\n")
    if verdict == "held":
        tail = "  A live writer holds it. Wait; it will clear when their command finishes.\n"
    elif verdict == "unheld" and age > 900:
        tail = ("  Older than 15m with no holder: this looks STALE, not contention. Removing it\n"
                "  is destructive on a shared checkout and is YOUR call, not this guard's:\n"
                f"    ls -l {lock}   # confirm the mtime is still not advancing\n"
                f"    rm {lock}\n")
    elif verdict == "unmeasured":
        tail = ("  The holder probe did NOT run, so this is unknown rather than unheld.\n"
                "  Do not remove the lock on the strength of this line.\n")
    else:
        tail = "  No holder and recently touched: most likely ordinary contention. Retry.\n"
    return head + body + tail

# A bare `git commit` with no pathspec, on a checkout whose HEAD lags origin,
# commits the DRIFT (AF-507).
SWEEP_THRESHOLD = int(os.environ.get("AMUX_SWEEP_COMMIT_THRESHOLD", "20") or "20")


def _commit_has_pathspec(scrubbed):
    """True when a `git commit` names paths, so it cannot sweep the whole index.

    Only the FORM matters here, not what the paths are: `git commit <paths>`
    takes those paths' worktree state and ignores the index for everything else,
    which is precisely what a drift-sweep is not. Two spellings count — an
    explicit `--` separator, and trailing bare operands after the flags.
    """
    m = re.search(r'\bgit\s+' + GIT_GLOBALS + r'commit\b([^\n;&|]*)', scrubbed)
    if not m:
        return False
    rest = m.group(1)
    if re.search(r'(^|\s)--(\s|$)', rest):
        return True
    # Flags that CONSUME the next token; anything else bare is an operand.
    takes_arg = {"-m", "--message", "-C", "--reuse-message", "-c", "--reedit-message",
                 "-F", "--file", "--author", "--date", "-S", "--gpg-sign",
                 "--cleanup", "--template", "-t", "--fixup", "--squash",
                 "--trailer", "--pathspec-from-file"}
    toks = rest.split()
    i = 0
    while i < len(toks):
        t = toks[i]
        if t.startswith("-"):
            if "=" in t:
                i += 1
                continue
            if t in takes_arg:
                i += 2
                continue
            i += 1
            continue
        return True   # a bare operand: this commit is path-scoped
    return False


def _sweep_commit_verdict(cmd, scrubbed, run_dir):
    """Refuse a no-pathspec `git commit` that would sweep index-vs-frozen-HEAD drift.

    Reported by `backend`, near-miss 2026-09-04. `git add <file>` hit a peer's
    index.lock and failed, so their file was never staged. The follow-up bare
    `git commit -m` then committed the ENTIRE index-vs-HEAD drift — 1120 files,
    +67067/-6296 — under their message, not containing their change. Their
    `git reset --soft HEAD~1` to undo it was then blocked by this same guard,
    correctly. So the guard blocked the FIX and not the CAUSE, and the cause is
    not recoverable from inside the checkout.

    WHY THE COUNT IS TAKEN TWICE, AND WHY THE SECOND ONE IS THE SIGNAL.
    `git diff --cached` is against HEAD, and graft-push freezes HEAD ~1846
    commits behind origin while the index tracks current. So a sweep shows a
    huge staged-vs-HEAD set whose files ALREADY MATCH ORIGIN — they are not
    anybody's work, they are the frozen baseline. A genuine large commit is the
    opposite: its files differ from origin too, because that is what makes them
    work. Subtracting the origin-relative set from the HEAD-relative one leaves
    exactly the drift, which is why a plain file-count threshold would refuse
    real refactors and this does not.

    On an ordinary checkout HEAD and origin/main agree, the drift set is empty,
    and this never fires. That is correct: the failure needs a lagging HEAD.

    Returns None to allow, or a block-reason string."""
    if not re.search(r'\bgit\s+' + GIT_GLOBALS + r'commit\b', scrubbed):
        return None
    if re.search(r'\bgit\s+' + GIT_GLOBALS + r'commit\b[^\n;&|]*--amend\b', scrubbed):
        return None   # amend has its own verdict, with its own pin
    if _commit_has_pathspec(scrubbed):
        return None
    import subprocess

    def _names(*args):
        out = subprocess.run(("git", "-C", run_dir, "diff", "--cached", "--name-only") + args,
                             capture_output=True, text=True, timeout=15).stdout
        return {l for l in out.splitlines() if l.strip()}

    staged = _names()
    # AN EARLY-OUT, NOT A RULE. `drift` below is a SUBSET of `staged`, so
    # `len(drift) > SWEEP_THRESHOLD` already implies this — removing this line
    # cannot change any verdict, and a mutation that deletes it stays green BY
    # CONSTRUCTION rather than for want of a test. It is here to skip a second
    # `git diff` and a rev-parse loop on every ordinary small commit, which is
    # the overwhelmingly common case for a hook on every Bash call.
    #
    # Written out because a reader who mutates it, sees green, and concludes the
    # threshold is untested would be drawing the wrong lesson from a correct
    # observation (ethos rule 7 — a green mutation can mean redundancy).
    if len(staged) <= SWEEP_THRESHOLD:
        return None
    base = None
    for ref in ("origin/main", "origin/HEAD"):
        r = subprocess.run(("git", "-C", run_dir, "rev-parse", "--verify", "--quiet", ref),
                           capture_output=True, text=True, timeout=10)
        if r.returncode == 0 and r.stdout.strip():
            base = ref
            break
    if base is None:
        return None   # no origin to compare against: fail-open, as everywhere here
    drift = staged - _names(base)
    if len(drift) <= SWEEP_THRESHOLD:
        return None
    pin = os.environ.get("AMUX_ALLOW_SWEEP_COMMIT", "").strip()
    if pin:
        # A PIN, NOT A FLAG, for the same reason AMUX_AMEND_EXPECT is one: the
        # number can only be supplied by someone who ran the count, so the
        # escape requires having LOOKED. A bare on/off switch would be set once
        # in a shell profile and never read again.
        if pin == str(len(drift)):
            return None
        return (f"AMUX_ALLOW_SWEEP_COMMIT={pin} does not match the {len(drift)} drift file(s) "
                f"this commit would sweep — re-read the count and pin THAT, or the escape is "
                f"authorizing a commit you have not looked at")
    return (
        f"bare `git commit` would sweep {len(drift)} file(s) of index-vs-HEAD DRIFT "
        f"({len(staged)} staged against HEAD, and {len(drift)} of them already match {base}). "
        f"Those are not your work: HEAD lags {base} on this checkout, so the index carries "
        f"the whole gap and a no-pathspec commit takes all of it under YOUR message. "
        f"Commit your own paths instead: `git commit <your files> -m ...`. "
        f"If you really mean to commit all {len(drift)}, pin the count you just read: "
        f"AMUX_ALLOW_SWEEP_COMMIT={len(drift)} git commit ...")


def _amend_verdict(cmd, scrubbed, run_dir):
    """Case 15/16 (2026-07-05 near-miss): `git commit --amend` rewrites shared
    HEAD — which may be ANOTHER session's just-landed commit (author identity
    can't discriminate: every session commits as the same git user). Rule:
    amend requires PROOF OF INSPECTION — the caller must have looked at HEAD
    and pinned it: `AMUX_AMEND_EXPECT=<head-sha> git commit --amend ...`.
    Allowed iff the pinned sha == actual current HEAD AND HEAD is not already
    pushed (published history is never amended on shared trunk).

    THE PIN NARROWS THE RACE, IT DOES NOT KILL IT, and this docstring used to
    claim otherwise ("kills the race where a foreign commit lands between your
    commit and your amend"). That claim is false whenever anything DELAYS the
    command between this hook admitting it and git executing it, because the
    check happens at ADMISSION and nothing re-verifies at execution.

    Measured 2026-09-03 (mixpeek MC-1624, self-disclosed by mvs-research against
    themselves): they pinned correctly, their command then waited 30 iterations
    on .git/index.lock held by another lane's commit, and by the time the amend
    ran HEAD was that other lane's commit, which it rewrote. They had read this
    docstring, concluded the pin was sufficient, and followed the documented
    procedure exactly. A guard that overstates its coverage turns a careful
    operator into a confident one.

    So the lock check below is a LARGE, CHEAP REDUCTION, not a proof. Closing
    the class needs a re-verify at execution time, and git has no atomic
    compare-and-amend to build it from. Do not restore the "kills the race"
    wording.
    Returns None to allow, or a block-reason string."""
    if not re.search(r'\bgit\s+' + GIT_GLOBALS + r'commit\b[^\n;&|]*--amend\b', scrubbed):
        return None
    import subprocess
    def _git(*args):
        return subprocess.run(("git", "-C", run_dir) + args, capture_output=True,
                              text=True, timeout=10).stdout.strip()
    head = _git("rev-parse", "HEAD")
    if not head:
        return None  # fail-open
    # case 16: never amend published history
    for ref in ("origin/main", "origin/HEAD"):
        base = _git("merge-base", head, ref)
        if base == head:
            return ("git commit --amend on a PUSHED commit — published shared history is "
                    "never rewritten; make a follow-up commit instead")
    # A PRESENT INDEX LOCK MEANS HEAD IS ABOUT TO MOVE. That is the window this
    # guard cannot otherwise see: the pin is checked HERE, at admission, and a
    # caller that then blocks waiting for the lock executes its amend against a
    # HEAD this hook never looked at. Refusing while the lock is held converts
    # the dangerous case into "retry in a moment", costs one stat, and needs no
    # cooperation from the caller.
    #
    # ORDERED AFTER THE PUSHED CHECK ON PURPOSE. "Wait for the lock and re-run"
    # is useless advice if the amend is going to be refused anyway for touching
    # published history, and telling a caller to retry against an absolute rule
    # sends them into a loop. The transient reason must not mask the permanent
    # one.
    #
    # NOT COMPLETE, deliberately stated: a command admitted while the lock is
    # ABSENT can still block on a lock taken a millisecond later. That
    # refinement is mvs-research's, made against their own proposal. This
    # removes the case that fired, not the class.
    # A LOCK IS NOT ALWAYS A LIVE PEER, which is why this AGES it. Git's own
    # error says so: "a git process may have crashed in this repository earlier
    # ... remove the file manually to continue". On a ~40-lane checkout a
    # crashed or SIGKILLed git is not rare, and a naive refuse-on-present turns
    # a stale lock into "every lane's amend is refused forever, blaming a peer
    # who is not there" (mvs-research, who hit the lock state twice in one
    # evening while reviewing this patch).
    #
    # THE THRESHOLD IS GENEROUS ON PURPOSE. `git commit` holds this lock across
    # its hooks, and on the mixpeek checkout pre-commit routinely runs for
    # MINUTES: commits of 2 to 9 minutes were measured on 2026-09-03 and one
    # pre-push gate ran 917s. A 120s cutoff would call a legitimately-held lock
    # stale, which is the failure that matters, because it re-opens the exact
    # window this check exists to close. 900s is longer than any commit observed
    # here and far shorter than a lock nobody has noticed.
    #
    # Deliberately NOT "is a git process running": pgrep matching text it was
    # handed has already produced one wrong conclusion on this box today, and a
    # process check is the part most likely to be wrong in a way that fails
    # CLOSED (mvs-research's caution).
    _LOCK_FRESH_S = 900
    try:
        git_dir = _git("rev-parse", "--absolute-git-dir")
        lock = os.path.join(git_dir, "index.lock") if git_dir else ""
        if lock and os.path.exists(lock):
            age = time.time() - os.stat(lock).st_mtime
            if age < _LOCK_FRESH_S:
                return ("git commit --amend while .git/index.lock is HELD (age "
                        f"{int(age)}s) — another session has a commit in flight, so HEAD "
                        "is about to move and your AMUX_AMEND_EXPECT pin is checked "
                        "BEFORE your command runs, not after. That is exactly how a "
                        "correctly-pinned amend rewrote a peer's commit on 2026-09-03. "
                        "Wait for the lock and re-run; the pin is then re-checked "
                        "against the new HEAD.\n"
                        "  If it never clears, the lock may be STALE from a crashed git "
                        "rather than a live peer. Distinguish before removing it:\n"
                        f"    ls -l {lock}\n"
                        "  A lock whose mtime is not advancing and older than 15m is "
                        "treated as stale by this guard and stops blocking you.")
            # Stale: not evidence of a peer. Allow rather than block every lane
            # indefinitely; the pin check above still applies.
    except Exception:
        pass  # fail-open: a stat we cannot do must not block a legitimate amend
    m = re.search(r'AMUX_AMEND_EXPECT=([0-9a-f]{7,40})\b', cmd)
    if m and head.startswith(m.group(1)):
        # AF-106 durable half (AMUX-3407): the pin proves the COMMIT BEING
        # REWRITTEN is yours; the check below proves the STAGED SET being
        # absorbed is too. On 2026-08-20 a correctly-pinned bare amend swept
        # 139 lines of a peer's staged work (their migration and a 132-line
        # handler change) into a commit carrying an unrelated message — the
        # pin was satisfied and protected the wrong operand. The ownership
        # question is the one the pre-commit staged-guard already answers;
        # this asks the SAME server endpoint at the second door (AMUX-2325).
        return _amend_staged_check(scrubbed, run_dir)
    got = f"pinned {m.group(1)} != HEAD {head[:12]}" if m else "no AMUX_AMEND_EXPECT pin"
    return _amend_pin_refusal(got)


def _amend_pin_refusal(got):
    return ("git commit --amend without verified HEAD pin (" + got + ") — HEAD on this SHARED "
            "branch may be ANOTHER session's commit (2026-07-05 near-miss: an amend silently "
            "rewrote a foreign unpushed commit). Inspect first (`git log -1 --format=%H`), then "
            "re-run pinned: `AMUX_AMEND_EXPECT=<that-sha> git commit --amend -- <your paths>` "
            "— the guard allows it only if HEAD still matches when the amend runs. SCOPE IT "
            "WITH A PATHSPEC: the pin protects the commit you are rewriting, NOT the staged "
            "set you are absorbing, and a bare `--amend` takes everything staged including a "
            "peer's in-flight work (AF-106, 139 lines swept on 2026-08-20). If HEAD is not "
            "yours, use a follow-up commit instead")


def _amend_staged_check(scrubbed, run_dir):
    """AF-106 durable half (AMUX-3407): a pinned BARE amend absorbs the whole
    staged set, so the staged set's ownership is checked against the same
    server endpoint the pre-commit staged-guard uses — one predicate, two
    doors (AMUX-2325). Fail-OPEN on every error (the guard's standing
    contract: an outage must not brick commits); only a POSITIVE foreign
    verdict refuses. A pathspec amend absorbs only what it names and passes
    without asking; the sanctioned escapes from a refusal are the pathspec
    form (your own work) and ~/.amux/guard-allow-once (deliberate absorption,
    owner-sanctioned, audit-logged) — the same two doors every other verdict
    here offers. AMUX_AMEND_STAGED_GUARD=0 disables just this check."""
    if re.search(r'\bcommit\b[^\n;&|]*\s--\s', scrubbed):
        return None  # pathspec amend: scoped by construction
    if os.environ.get("AMUX_AMEND_STAGED_GUARD", "1").strip().lower() in ("0", "false", "off", "no"):
        return None
    sess = os.environ.get("AMUX_SESSION", "")
    if not sess:
        return None  # a human's amend is not amux's to gate
    try:
        import subprocess, ssl, urllib.request
        staged = subprocess.run(
            ("git", "-C", run_dir, "diff", "--cached", "--name-only"),
            capture_output=True, text=True, timeout=10).stdout.split()
        if not staged:
            return None  # message-only amend absorbs nothing
        body = json.dumps({"session": sess, "dir": run_dir, "paths": staged,
                           "op": "amend", "guard_version": 1}).encode()
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        # Base URL: explicit override first (the test suite points it at an
        # unreachable port to prove fail-open deterministically — the sibling
        # resolver below self-heals to the LIVE server, which would silently
        # turn a fail-open test into a live-server test); then the
        # staged-guard's own resolver (it self-heals a stale AMUX_URL port);
        # env fallback last. Any failure falls through to fail-open.
        base = os.environ.get("AMUX_STAGED_GUARD_URL")
        if not base:
            try:
                import importlib.machinery
                sib = os.path.join(os.path.dirname(os.path.realpath(__file__)), "amux-staged-guard")
                mod = importlib.machinery.SourceFileLoader("_asg", sib).load_module()
                base = mod.amux_base_url()
            except Exception:
                base = os.environ.get("AMUX_URL") or "https://localhost:8824"
        req = urllib.request.Request(
            base + "/api/git/staged-guard", data=body, method="POST",
            headers={"Content-Type": "application/json", "X-Amux-Session": sess})
        with urllib.request.urlopen(req, timeout=5, context=ctx) as r:
            d = json.loads(r.read().decode())
        return _amend_staged_decision(d)
    except Exception:
        return None  # fail-open


def _amend_staged_decision(d):
    """Pure, so the matrix is testable without a server. Only FOREIGN paths
    refuse — `shared` (both edited) matches the pre-commit guard's own
    non-blocking policy, and `undecided`/disabled are not verdicts. The
    2026-08-20 specimen was foreign: the absorbed migration + handler change
    had exactly one editing session, and it was not the amender."""
    if not isinstance(d, dict) or d.get("undecided") or d.get("enabled") is False:
        return None
    foreign = [(f.get("path") or "?") for f in (d.get("foreign") or [])]
    if not foreign:
        return None
    shown = ", ".join(foreign[:6]) + (" …" if len(foreign) > 6 else "")
    return ("git commit --amend would ABSORB another session's staged work — %d staged path(s) "
            "were last edited by a different session (%s), and a bare --amend takes the whole "
            "staged set into your commit under your message. This is AF-106's exact incident: "
            "139 lines swept on 2026-08-20 by an amend whose pin was VALID — the pin protects "
            "the commit, not the absorbed content. Scope it to your own work: "
            "`git commit --amend -- <your paths>`. If absorbing their staged work is genuinely "
            "intended, coordinate with that session, then use the owner-sanctioned one-off "
            "(~/.amux/guard-allow-once, audit-logged)." % (len(foreign), shown))


def _content_is_committed(top, rel, base=None):
    """Are this file's CURRENT bytes already inside a commit? (AMUX-3859)

    The discard guard exists to stop a restore destroying work that exists
    NOWHERE ELSE. When the on-disk blob is already committed, a restore cannot
    destroy anything: content equal to a commit's blob is by definition not
    unsaved keystrokes. That holds regardless of WHO edited the file or whether
    their session resolved, which is what makes it stronger than the attribution
    it overrules.

    This is the guard's OWN prescribed restore-safety recipe. It printed the
    recipe and never ran it, so an operator could pass the test the guard
    recommended and still be refused — CD-79, where a fleet-wide `graft-push.sh`
    sat two commits behind because the update the guard recommends was the update
    the guard blocked.

    Conservative on every failure: an unreadable blob, a timeout, or a git error
    returns False, which keeps the block. A guard that opens on an error is worse
    than one that is occasionally too strict.
    """
    # LOCAL import, matching this file's convention (see the ones at :177 and
    # :236). There is no module-level `import subprocess`, so a module-level
    # helper using it NameErrors on first call — and neither py_compile nor the
    # 51-test suite catches that, because neither one calls this function.
    import subprocess
    try:
        blob = subprocess.run(["git", "-C", top, "hash-object", "--", rel],
                              capture_output=True, text=True, timeout=10).stdout.strip()
        if not blob:
            return False
        out = subprocess.run(
            ["git", "-C", top, "log", "--all", "--format=%H", "--find-object", blob,
             "--", rel],
            capture_output=True, text=True, timeout=30)
        if out.returncode != 0 or not out.stdout.strip():
            return False
        # A RESTORE OVERWRITES CONTENT *AND MODE*, and this check only compared
        # content (amux-frustrations' review of AMUX-3859, two repros). A file
        # can be modified without its bytes changing, so a matching blob is not
        # sufficient:
        #
        #   chmod +x run.sh          :100644 100755 ... M   -> exec bit reverted
        #   symlink -> regular file  :120000 100644 ... T   -> symlink restored
        #
        # Neither is unsaved keystrokes, so the original sentence stayed
        # technically true while the guard's PROMISE — "a restore destroys
        # nothing" — became false. Ask git for the mode pair instead of
        # reasoning about it: `git diff --raw` reports old and new mode, and a
        # difference means the restore would overwrite an uncommitted change git
        # itself is calling modified. Catches M and T together.
        # THE BASE DEPENDS ON THE RESTORE FORM (amux-frustrations, second review).
        # `git diff --raw` compares worktree vs INDEX, but `git checkout <ref> --
        # <path>` restores from the REF and overwrites index AND worktree. So a
        # STAGED mode change was invisible: `chmod +x && git add` leaves
        # worktree-vs-index empty while worktree-vs-ref still reports M, and the
        # restore killed the staged exec bit.
        #
        #   git checkout -- <path>        source is the index -> base None
        #   git checkout <ref> -- <path>  source is <ref>      -> base <ref>
        #
        # `__AMBIGUOUS__` is what the caller passes when one command names more
        # than one ref: we cannot say which base applies, so keep the block.
        if base == "__AMBIGUOUS__":
            return False
        raw = subprocess.run(["git", "-C", top, "diff", "--raw"]
                             + ([base] if base else []) + ["--", rel],
                             capture_output=True, text=True, timeout=10)
        if raw.returncode != 0:
            return False
        line = (raw.stdout.strip().splitlines() or [""])[0]
        if line:
            parts = line.split()
            if len(parts) < 2:
                return False          # unparseable -> fail closed, like everything here
            if parts[0].lstrip(":") != parts[1]:
                return False          # mode or type change: the restore would revert it
        return True
    except Exception:
        return False


# A REDIRECT IS NOT A PATHSPEC (AMUX-3890, filed by mixpeek-docs 2026-08-29).
#
# The operand scan splits on `--` and treats everything after it as a path. Shell
# redirection survives that split, so a redirect token becomes a phantom pathspec
# and vetoes the whole Bash call. The reported specimen:
#
#   git -C ~/Dev/mixpeek checkout origin/main -- docs/platform/syncs.mdx \
#       docs/retrieval/cookbook.mdx 2>&1 | head -30
#
#   -> "2 path(s) NOT blocked ... syncs.mdx, cookbook.mdx"
#      "BLOCKED ... another session -- 2> (recently edited ...)"
#
# The blocked path is the literal string `2>`, in the same message that explicitly
# cleared both real paths. Dropping `2>&1 | head -30` let the identical command
# through. The `&` inside `2>&1` truncates the tail regex mid-token, which is what
# leaves a bare `2>` behind; a fused `2>/dev/null` survives whole and is just as
# wrong.
#
# Two costs, and the second is the one that makes this worth a helper. The guard
# stops the entire Bash call, so one phantom token vetoes a command whose every
# real path the guard already cleared. And the refusal names a nonexistent file as
# another session's work, which reads as a genuine ownership conflict and invites a
# guard-allow-once that was never needed — a false positive that actively teaches
# people to bypass the guard is worse than one that merely annoys them.
#
# FAIL-OPEN BY CONSTRUCTION, matching the rest of this file: this only ever REMOVES
# candidate paths. A pathological filename that genuinely looks like a redirect goes
# unchecked rather than being falsely blocked, which is the same direction every
# other fallback here takes.
_REDIR_DUP = re.compile(r'^[0-9]*[<>]&[0-9]*-?$')                    # 2>&1  >&2  2>&-
_REDIR_OP = r'(?:[0-9]*(?:>>|>|<<<|<<|<)|&>>|&>)'
_REDIR_BARE = re.compile(r'^' + _REDIR_OP + r'$')                    # >  2>  >>  <  &>
_REDIR_FUSED = re.compile(r'^' + _REDIR_OP + r'\S+$')                # 2>/dev/null  >out.txt


def _strip_redirections(toks):
    """Drop shell redirection tokens (and their targets) from an operand list.

    The three tests are ORDERED, and both orderings that look fine are wrong:

      DUP before BARE   — `2>&1` is self-contained. Let BARE see it first and it
                          matches the `2>` prefix, sets skip, and eats the NEXT
                          token, which is a real pathspec.
      BARE before FUSED — `>>` is two operator characters. FUSED reads that as
                          operator `>` plus target `>`, drops it as self-contained,
                          and orphans the `log` in `>> log` back into the path list.
                          Caught by the table below, which is why it is a table."""
    out, skip = [], False
    for t in toks:
        if skip:
            skip = False          # this token is the previous operator's target
            continue
        if _REDIR_DUP.match(t):
            continue              # `2>&1` — self-contained, nothing follows
        if _REDIR_BARE.match(t):
            skip = True           # `> out.txt` — drop the operator AND its target
            continue
        if _REDIR_FUSED.match(t):
            continue              # `2>/dev/null` — operator and target in one token
        out.append(t)
    return out




def _discard_operands(cmd):
    """Extract (paths, src_refs) from every checkout/restore invocation in `cmd`.

    LIFTED OUT OF `_discard_verdict` SO A TEST CAN REACH IT (AMUX-3890). The
    verdict function POSTs to /api/git/staged-guard and fails open when the server
    is unreachable, so nothing that goes through it can pin operand parsing: the
    "not blocked" assertion is green with the parser broken. This is the layer the
    bug lives at, so this is the layer the test has to call.

    That distinction is not hypothetical here. The first version of the AMUX-3890
    test called `_strip_redirections` directly and passed a mutation that deleted
    its only call site — pinning the helper while leaving the wiring untested,
    which is exactly the failure ethos rule 7 names: a check on the wrong layer is
    exactly as green as one on the right layer."""
    import shlex
    paths = []
    src_refs = set()
    for m in re.finditer(r'\bgit\s+' + GIT_GLOBALS + r'(checkout|restore)\b([^\n;&|]*)', cmd):
        sub, tail = m.group(1), m.group(2)
        try:
            toks = shlex.split(tail)
        except ValueError:
            continue
        toks = _strip_redirections(toks)
        if sub == "checkout":
            if "--" not in toks:
                # `git checkout <tree-ish> <paths>...` WITHOUT `--` IS A PATH
                # RESTORE and was skipped entirely (amux-frustrations, AMUX-3859
                # round 4 — pre-existing since ea2a5731, 2026-08-14). The old
                # comment here said "`git checkout <branch>` — switches, destroys
                # nothing", which is true of ONE operand and false of two: with
                # two, git reads the first as a tree-ish and the rest as paths,
                # overwriting index and worktree. Measured: `git checkout
                # origin-main run.sh` reverted a staged 100755 to 100644 while
                # the `--` spelling of the same operation was correctly blocked.
                #
                # A comment describing a different command is why it sat for two
                # weeks. One operand still switches and is still skipped.
                ops = [t for t in toks if not t.startswith("-")]
                if len(ops) < 2:
                    continue
                src_refs.add(ops[0])
                cand = ops[1:]
            else:
                # Anything non-flag BEFORE the `--` is the source ref.
                pre = [t for t in toks[:toks.index("--")] if not t.startswith("-")]
                if pre:
                    src_refs.add(pre[-1])
                cand = toks[toks.index("--") + 1:]
        else:
            staged = any(t in ("--staged", "-S") for t in toks)
            worktree = any(t in ("--worktree", "-W") for t in toks)
            if staged and not worktree:
                continue            # unstage only; the worktree copy survives
            for i, t in enumerate(toks):
                if t.startswith("--source="):
                    src_refs.add(t.split("=", 1)[1])
                elif t in ("-s", "--source") and i + 1 < len(toks):
                    src_refs.add(toks[i + 1])
            cand = (toks[toks.index("--") + 1:] if "--" in toks
                    else [t for t in toks if not t.startswith("-")])
        paths += [p for p in cand if p != "." and not p.startswith("-")]
    return paths, src_refs


def _discard_verdict(cmd, scrubbed, run_dir):
    """AC-212 (2026-08-04): block a PATH-SCOPED discard that would destroy ANOTHER
    session's uncommitted work.

    The guard above covers only TREE-WIDE destroys and its own docstring says
    path-scoped ops "pass". That exemption was written for multi-directory repos,
    where naming a path really does narrow the blast radius to your own work. It is
    exactly inverted in a SINGLE-FILE repo: `git checkout -- amux-server.py` names
    one path, and that one path holds every session's edits. I destroyed a peer's
    uncommitted fix that way while reverting my own ~20 lines; `git diff --stat`
    had said 63 insertions and I never asked whose the other 43 were.

    Why this direction deserves a guard MORE than the two that already have one:
    committing or pushing a peer's work is RECOVERABLE — the content is in the
    object store, revertable, and the PASSENGER convention names it. Destroying
    unstaged work leaves no object and no reflog entry. amux guarded both
    recoverable directions and left the unrecoverable one open.

    Detection is deliberately narrow, because a false block on `git checkout <branch>`
    would be worse than the gap:
      - checkout: only with an explicit `--` separator (a branch switch has none)
      - restore:  skipped when --staged/-S is present WITHOUT --worktree/-W, since
                  that only unstages and the worktree content survives
      - `.` is left to the tree-wide patterns above
    Attribution is not guessed here: it reuses POST /api/git/staged-guard, which is
    already generic ({session,dir,paths} -> foreign) rather than commit-specific, and
    derives ownership from each session's own JSONL transcript.

    Fail-open on anything unexpected, same posture as the rest of this guard.
    Returns a block-reason string, or None to allow."""
    import urllib.request, ssl, subprocess
    if not re.search(r'\bgit\s+' + GIT_GLOBALS + r'(?:checkout|restore)\b', scrubbed):
        return None
    # Detect on `scrubbed` (so prose/docs that merely mention the command never
    # match), but extract the operands from the ORIGINAL cmd — scrubbing removes
    # quoted strings, which is where a filename with a space would live.
    paths, src_refs = _discard_operands(cmd)
    if not paths:
        return None
    top = subprocess.run(["git", "-C", run_dir, "rev-parse", "--show-toplevel"],
                         capture_output=True, text=True, timeout=10).stdout.strip()
    if not top:
        return None
    rel = []
    for p in paths:
        ap = os.path.realpath(os.path.join(run_dir, os.path.expanduser(p)))
        try:
            rel.append(os.path.relpath(ap, top))
        except Exception:
            pass
    if not rel:
        return None
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    base = amux_base_url()
    sess = os.environ.get("AMUX_SESSION", "")
    body = json.dumps({"session": sess, "dir": top, "paths": rel, "op": "discard"}).encode()
    req = urllib.request.Request(base + "/api/git/staged-guard", data=body, method="POST",
                                 headers={"Content-Type": "application/json",
                                          "X-Amux-Session": sess})
    res = json.load(urllib.request.urlopen(req, timeout=4, context=ctx))
    foreign = res.get("foreign") or []
    shared = res.get("shared") or []
    # BLOCK ON shared AS WELL AS foreign (AC-221). The two consumers of this
    # endpoint cannot share one verdict:
    #   COMMIT  a co-edited file -> you have a real claim, and the peer's work
    #           survives in the object store either way. `shared` = warn is right.
    #   DESTROY a co-edited file -> the peer's uncommitted half is gone whether or
    #           not you also touched it. No object, no reflog entry. Having edited
    #           it too grants NO claim to delete it.
    # "Both of us wrote this" is a reason to let you commit it and a reason to stop
    # you deleting it. f85b162 correctly widened `mine` to count shell writes, which
    # moved co-edited files from foreign -> shared, and this consumer — keyed on
    # foreign alone — silently stopped blocking the exact command that motivated
    # AC-212. Keying on both is what makes the widening safe here.
    if not foreign and not shared:
        return None
    hits = foreign + shared
    # A COMMITTED BLOB IS NOT UNSAVED WORK (AMUX-3859). Drop any hit whose
    # current bytes are already in a commit before deciding to block: the
    # attribution says who touched it, the blob says whether anything is at
    # risk, and only the second speaks to what this guard protects.
    base = (src_refs.pop() if len(src_refs) == 1
            else ("__AMBIGUOUS__" if src_refs else None))
    safe = [h for h in hits
            if h.get("path") and _content_is_committed(top, h.get("path"), base)]
    if safe:
        hits = [h for h in hits if h not in safe]
        foreign = [h for h in foreign if h not in safe]
        shared = [h for h in shared if h not in safe]
        sys.stderr.write(
            "amux shared-guard: %d path(s) NOT blocked — their on-disk bytes are "
            "already committed, so a restore cannot destroy unsaved work: %s\n"
            % (len(safe), ", ".join(h.get("path", "?") for h in safe[:5])))
    if not hits:
        return None
    # AF-423: "(unknown)" IS A PLACEHOLDER, NOT A SESSION NAME.
    #
    # The server sends `owner: "(unknown)"` on the shared branch when there is no
    # peer record at all, and it sends `peer: false` beside it to say so. The
    # staged-guard has branched on that since AF-24, in its own words: rendering
    # the placeholder as a co-editor "asserts a co-editor who does not exist —
    # the real fact is that YOU edited it and it has uncommitted changes". This
    # guard never learned it, so its refusal read "(recently edited by
    # (unknown))" and, worse, closed with "or ask (unknown) first".
    #
    # Measured 2026-09-02: it named a phantom co-editor on git_guard.rs whose
    # every hunk was the committer's own, written minutes earlier.
    #
    # THE SAME BUG WAS ALREADY HALF-FIXED HERE and the fix made the tail
    # nonsense: an empty owner rendered as "an edit record with no session
    # attached", which is right in the first slot and absurd in "or ask <that>
    # first". Both slots now come off one decision.
    def _is_named_peer(h):
        owner = (h.get("owner") or "").strip()
        if owner in ("", "(unknown)"):
            return False
        # OLD SERVERS SEND NO `peer` KEY. Absent means "cannot answer", not
        # "answer is no" — treating it as no would silently drop a real peer's
        # name from every refusal against an older server. A real-looking name
        # with no flag is taken at face value, exactly as before.
        return bool(h["peer"]) if "peer" in h else True

    named = sorted({(h.get("owner") or "").strip() for h in hits if _is_named_peer(h)})
    what = ", ".join(f.get("path", "?") for f in hits[:5])
    # THE BLOCK IS UNCHANGED EITHER WAY. It is about recoverability — `git
    # checkout --` leaves no object and no reflog entry — and that does not
    # depend on who edited the file. Only the attribution is in question here.
    tail = ("Unlike a bad commit or push, this is UNRECOVERABLE — no object, no "
            "reflog entry. Make it recoverable instead: `git stash push -- <paths>` "
            "keeps the content, or revert only your own hunks (`git diff` then a "
            "sliced `git apply -R`)")
    if named:
        who = ", ".join(named)
        # Distinct wording: "also edited" is a different fact from "is theirs",
        # and a guard that says the wrong one gets argued with instead of obeyed.
        lead = ("discarding UNCOMMITTED work that belongs to another session"
                if foreign else
                "discarding a file ANOTHER SESSION HAS ALSO EDITED")
        return (lead + " — "
                f"{what} (recently edited by {who}). Naming a path does NOT make this "
                "yours in a shared checkout: in a single-file repo that one path holds "
                "every session's edits, and editing it too is not a claim to destroy "
                "their half. " + tail + f", or ask {who} first")
    # No nameable peer: say what IS known and stop. No accusation, and no
    # remedy that tells the reader to go ask somebody who does not exist.
    return ("discarding UNCOMMITTED CHANGES — "
            f"{what} has uncommitted content and NO other session's edit record names "
            "it, so nothing here says a peer wrote any of it. That does not make the "
            "discard safe: the changes are still unsaved, and on a shared checkout "
            "they may be yours, a peer's with no record, or both. " + tail)


def _has_cotenants(run_dir):
    """True if another live session shares this repo root. Fail-CLOSED to False
    (allow) on any error, matching the guard's standing fail-open contract: a
    guard that blocks when the server is down would wedge every lane."""
    try:
        import urllib.request, ssl
        # `op` IS REQUIRED, and its absence here is what kept AF-156 alive
        # after the server-side fix. git_guard.rs `hook_is_outdated` is
        # `guard_version < 2 && !has_explicit_op`, and its doc comment justifies
        # keying on `op` with "every modern client sends at least `op`" — a
        # premise this file's own third POST contradicted, 170 lines below the
        # path that fix was written for.
        #
        # Measured 2026-08-24: 212 OUTDATED HOOK WARNs AFTER the fix landed at
        # 79e9c89c 06:12, including this checkout at 16:23:51 with a hook
        # byte-identical to source. The warning's printed remedy is "Reinstall:
        # scripts/install-hooks.sh", which reinstalls hooks that were already
        # current — so a lane following it exactly sees no change and the
        # warning returns within the hour (the AMUX-2140 shape, and the reason
        # the server-side comment calls it worse than merely noisy).
        body = json.dumps({"session": os.environ.get("AMUX_SESSION", ""),
                           "dir": run_dir, "paths": [], "op": "cotenant-probe"}).encode()
        req = urllib.request.Request(
            amux_base_url() + "/api/git/staged-guard",
            data=body, method="POST",
            headers={"Content-Type": "application/json",
                     "X-Amux-Session": os.environ.get("AMUX_SESSION", "")})
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        with urllib.request.urlopen(req, timeout=4, context=ctx) as r:
            return bool((json.loads(r.read().decode()) or {}).get("cotenants"))
    except Exception as _e:
        # Fail-open stays (see docstring): blocking every lane when the server
        # is down is worse than missing a warning. What changes is that the skip
        # is no longer SILENT. A quiet False is indistinguishable from "checked,
        # and you have no co-tenants" — so a session reads clean output as
        # verification it never received.
        #
        # This is not hypothetical and the coincidence is the point: the server
        # re-execs on every save of amux-server.py, so editing THAT file takes
        # the guard offline for a few seconds at a time. The one file where a
        # co-edit sweep is most likely on this checkout is the one whose editing
        # disables the check. On 2026-08-06 a commit swept ~93 lines of a peer's
        # work with no warning shown, during exactly such a window.
        try:
            sys.stderr.write("amux staged-guard: SKIPPED — could not reach the "
                             "amux server (" + type(_e).__name__ + "). Co-tenant "
                             "attribution was NOT checked; if you are committing a "
                             "shared file, run `git diff --cached --numstat` and "
                             "confirm the size matches what you wrote.\n")
        except Exception:
            pass
        return False



# AF-151 (AREA silent-partial): a block stops the WHOLE Bash call, not just the
# git verb that tripped it. When the command joined other work to that verb --
# the reported specimen was a heredoc writing a commit-message file followed by
# `git commit --amend -F` that file -- the other half is skipped too, and the
# refusal said nothing about it. The operator fixes the named git complaint,
# re-runs, and the retry reads a file the heredoc never wrote: rc=0 from an
# amend that changed nothing. Same family as the rest of AF-150 -- a compound
# operation whose silent half is invisible because the loud half was answered.
#
# DISCRIMINATES, deliberately: a lone `git ...` invocation gets no note (it
# would be noise on the common case, and a notice that always fires is one
# nobody reads). Runs on the SCRUBBED command so a heredoc BODY mentioning
# `&&` cannot manufacture a phantom second half.
_LAST_CMD = ""
_SHELL_JOINERS = re.compile(r"&&|\|\||;|\n|(?<!\|)\|(?!\|)")


def _skipped_half_note(cmd):
    """The NOTE text when a blocked command had non-git work, else ''."""
    if not cmd:
        return ""
    # Heredoc TERMINATORS survive the body strip and are not work: without
    # this, a lone `git commit -F - <<'EOF' ... EOF` reported a skipped
    # segment called `EOF`, which is both wrong and confusing at exactly the
    # moment the reader is deciding what to re-run.
    delims = set(re.findall(r"<<-?\s*['\"]?([A-Za-z_][A-Za-z0-9_]*)", cmd))
    segments = [seg.strip() for seg in _SHELL_JOINERS.split(_scrub(cmd))]
    segments = [seg for seg in segments if seg and seg not in delims]
    if len(segments) < 2:
        return ""
    # A segment is "git work" when the verb is git, after leading env
    # assignments (FOO=bar git ...) which are part of the same invocation.
    def _is_git(seg):
        words = seg.split()
        while words and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", words[0]):
            words = words[1:]
        return bool(words) and words[0] == "git"
    others = [(i, seg) for i, seg in enumerate(segments) if not _is_git(seg)]
    if not others:
        return ""
    # AF-153: DECIDE on the scrubbed text (that is what stops a heredoc body
    # manufacturing a phantom half) but DISPLAY the original. Scrubbing blanks
    # quoted content, so `echo "hello world" > /tmp/m.txt` rendered as
    # `echo   > /tmp/m.txt` — a command with its content removed, shown at the
    # exact moment the reader is deciding what to re-run. Split the ORIGINAL
    # the same way and show the segment at the same index; if the two do not
    # line up (an unbalanced quote, say), fall back to the scrubbed text rather
    # than show a mismatched segment, which would be worse than a blanked one.
    raw_segments = [seg.strip() for seg in _SHELL_JOINERS.split(cmd)]
    raw_segments = [seg for seg in raw_segments if seg and seg not in delims]
    idx, scrubbed_seg = others[0]
    display = raw_segments[idx] if len(raw_segments) == len(segments) else scrubbed_seg
    shown = display[:80] + ("..." if len(display) > 80 else "")
    return (
        "NOTE: the rest of this command did not run either — the block stops the whole "
        f"Bash call, not just the git verb. Skipped {len(others)} non-git segment(s), "
        f"first: `{shown}`.\n"
        "      If a later step reads what an earlier one was supposed to write, re-run "
        "the WHOLE command after fixing the complaint above; do not re-run only the git "
        "half against stale state.\n"
    )

def main():
    data = json.load(sys.stdin)
    if data.get("tool_name") != "Bash":
        return 0
    cmd = (data.get("tool_input") or {}).get("command", "") or ""
    global _LAST_CMD
    _LAST_CMD = cmd
    scrubbed = _scrub(cmd)                       # match only real invocations
    cwd = data.get("cwd") or os.getcwd()
    shared = [os.path.realpath(os.path.expanduser(p)) for p in
              os.environ.get("AMUX_SHARED_CHECKOUTS", "~/Dev/mixpeek").split(":") if p.strip()]
    # `-C` IS TWO DIFFERENT FLAGS AND ONLY ONE OF THEM IS A DIRECTORY.
    # `git -C <dir> <cmd>` changes directory; `git commit -C <commit>` reuses a
    # commit message, and `git log -C` / `git diff -C` ask for copy detection.
    # An unanchored search took the FIRST -C anywhere in the command, so
    # `git commit --amend -C HEAD` resolved run_dir to <cwd>/HEAD, a path with
    # no repo in it. `_amend_verdict` then ran `git -C <that> rev-parse HEAD`,
    # got nothing, and hit its `if not head: return None` fail-open. The amend
    # guard was therefore OFF for exactly the amend forms that name a commit,
    # while every other form blocked correctly and made it look present.
    #
    # Measured 2026-09-04: two amends with `-C <sha>` rewrote a peer's unpushed
    # commits on the mixpeek checkout, one of them replacing their message with
    # a different commit's. Neither was blocked. `git commit --amend --no-edit`
    # and `--reuse-message=HEAD` both blocked in the same session, which is what
    # made the gap read as a considered scope rather than a hole.
    #
    # Git's grammar puts -C among the GLOBAL options, before the subcommand, so
    # anchoring to `git` with only `-c <k=v>` allowed in between accepts every
    # real `git -C` and rejects every subcommand `-C`. A global flag this does
    # not list (`git --no-pager -C /x ...`) falls back to cwd inference, which
    # is the safe direction: a false refusal naming the escape hatch, never a
    # silent pass.
    mC = re.search(r'\bgit\s+' + GIT_GLOBALS_NOT_C + r'-C\s+(\S+)', scrubbed)
    # AMUX-3462 (MF-703): this hook reads the command TEXT, before the shell
    # expands it. A -C path spelled with a variable (`git -C $S/wipetest ...`)
    # therefore cannot be resolved here — the old code realpath'd the raw
    # token anyway, fabricating a literal '<cwd>/$S/wipetest' that
    # prefix-matched the shared checkout and produced a refusal naming a repo
    # that does not exist. Discard the capture and fall back to the documented
    # cwd inference; the refusal note below names the real cause and the
    # LITERAL-path escape (which is what actually works).
    _unexpanded_c = None
    if mC and re.search(r'[$`]', mC.group(1)):
        _unexpanded_c = mC.group(1)
        mC = None
    run_dir = os.path.realpath(os.path.expanduser(mC.group(1))) if mC else os.path.realpath(cwd)
    # WHERE run_dir CAME FROM, said out loud in the refusal (AF-23). Otherwise the
    # message asserts a repo path as FACT, and when the inference is wrong it still
    # reads as a true positive: a compound command that `cd`s into a throwaway clone
    # and commits THERE is blocked as though it targeted the shared checkout, naming
    # a repo the command never touched. Hit 2026-08-09 building a scratch clone.
    #
    # Deliberately NOT parsing `cd` out of the command, and that is the whole design
    # decision. A wrong parse fails OPEN on a real cross-session sweep — the exact
    # thing this guard exists to catch — and `cd a; cd b && git ...` is not safely
    # parseable. The asymmetry is decisive: a false positive costs one retry, a false
    # negative costs another session's uncommitted work.
    #
    # `-C` already resolves one line up, so a precise escape ALREADY EXISTS and the
    # refusal simply never named it. That was the real defect — an escape nobody is
    # told about is not an escape. Point at the path that works rather than guessing.
    _dir_note = ("" if mC else
                 "\n  (Repo INFERRED from this session's working directory — the guard cannot "
                 "see a `cd` inside a compound command. If you meant a different repo, such as "
                 "a scratch clone, re-run as `git -C <path> ... -- <paths>` and the guard will "
                 "evaluate THAT repo instead.)")
    if _unexpanded_c:
        _dir_note = (
            "\n  (The -C path %r contains an UNEXPANDED shell construct — this guard reads the "
            "command text BEFORE your shell expands it, so it cannot resolve that path; it "
            "evaluated the session cwd instead. The precise -C escape needs a LITERAL path: "
            "`git -C /full/path ...` — AMUX-3462.)" % _unexpanded_c)
    # The discard check runs BEFORE the static-scope gate below, and deliberately so.
    # AMUX_SHARED_CHECKOUTS is unset in every session env, in the shell, and in
    # amux-server.py, so `shared` is the hardcoded default ~/Dev/mixpeek — while
    # ~/Dev/amux is a documented shared checkout with 6 lanes whose ENTIRE codebase
    # is one file. The incident this check exists for happened there, so gating it on
    # that list would have produced a guard that cannot fire on its own motivating
    # case. A static list of shared checkouts drifts by construction; it needs a human
    # to notice a repo became shared.
    # This check does not need the list: it self-scopes on REAL cotenant data from
    # /api/git/staged-guard (repo-root paired, AMUX-2337). No cotenants -> no foreign
    # paths -> allow. In a genuinely private repo it costs one rev-parse and one
    # localhost POST on `git checkout -- <paths>` / `git restore <paths>`, and nothing
    # at all on any other command.
    discard_why = None
    _dv_err = None
    # RETRY BEFORE REFUSING (AC-287). The amux server re-execs on every save of
    # amux-server.py, which on this shared checkout happens many times an hour, so
    # a single 4s timeout is a routine event rather than an outage. Failing closed
    # on the first miss would refuse legitimate discards during every restart —
    # caught by a control in review: the guard blocked with a reachable server
    # purely because the call landed in a reload window. Three tries over ~6s costs
    # nothing on an operation this rare and removes that whole false-refusal class.
    for _dv_try in range(3):
        try:
            discard_why = _discard_verdict(cmd, scrubbed, run_dir)
            _dv_err = None
            break
        except Exception as _e:
            _dv_err = _e
            if _dv_try < 2:
                import time as _t   # inline, matching this hook's import style
                _t.sleep(2)
    if _dv_err is not None:
        # FAIL CLOSED HERE, unlike everywhere else in this guard (AC-287).
        # The standing contract is fail-OPEN: a guard that blocks when the server
        # is down wedges every lane, and for a COMMIT that trade is right — the
        # peer's work survives in the object store either way, so the cost of a
        # missed warning is a glance.
        #
        # A DISCARD is not that. It is UNRECOVERABLE: no object, no reflog entry.
        # A missed block is permanent data loss belonging to someone who is not
        # even at the keyboard. Measured 2026-08-07 with the server unreachable:
        # exit 0, no output, the destroy proceeded — and the only thing printed
        # was the co-tenant SKIP notice, which talks about COMMITTING a shared
        # file while the user is DISCARDING one, so it named the wrong operation
        # for the one case where the stakes are highest.
        #
        # This is the same asymmetry that motivated AC-221 — "both of us wrote
        # this" is a reason to let you commit and a reason to stop you deleting —
        # applied one level up, to the FAILURE MODE rather than the verdict. One
        # fail-open policy cannot serve a recoverable and an unrecoverable
        # consumer. The override below is the sanctioned escape and stays
        # available, so this is refusable, not a wedge.
        discard_why = ("cannot verify co-tenancy — the amux server is unreachable "
                       f"({type(_dv_err).__name__}). REFUSING an unrecoverable "
                       "discard rather than guessing: unlike a commit, this leaves "
                       "no object and no reflog entry, so a wrong guess is "
                       "permanent and the work destroyed may not be yours. Re-run "
                       "when the server answers, or use `git stash push -- <paths>` "
                       "which is reversible")
    if discard_why:
        if _consume_override(cmd):
            sys.stderr.write(f"amux guard: ALLOWED once (owner-sanctioned): {discard_why}\n")
        else:
            sys.stderr.write(
                f"BLOCKED by amux shared-checkout guard: {discard_why}.\n"
                f"'{run_dir}' is a SHARED checkout used by multiple agent sessions.{_dir_note}\n"
                f"OWNER-AUTHORIZED one-off: write the exact command to ~/.amux/guard-allow-once "
                f"and re-run (consumed once, audit-logged).\n")
            return 2
    # SELF-SCOPE the tree-wide checks too, instead of trusting a static list.
    # AMUX_SHARED_CHECKOUTS is unset everywhere, so `shared` was the hardcoded
    # ~/Dev/mixpeek and `git reset --hard` / `git clean -fd` were ALLOWED in
    # ~/Dev/amux — 6 lanes, entire codebase in one file, and the repo holding
    # this guard's own source. Measured, control-first: reset --hard BLOCKED in
    # mixpeek, allowed in amux. A list that must be edited when a repo becomes
    # shared drifts by construction, and nobody edited it for ~/Dev/amux.
    #
    # A checkout is SHARED if another live session actually resolves to the same
    # repo root — real data, repo-root paired (AMUX-2337), the same source the
    # discard check above already uses. The list stays as an ADDITIVE override so
    # an explicitly-named root is still guarded even with no cotenants online.
    _scope_dirs = [run_dir]
    if _unexpanded_c:
        # The naive resolution of the unexpanded token — what the old code used
        # as run_dir outright. Kept ONLY as an extra shared-scope candidate so
        # this fix never fails open relative to the old behavior: an ABSOLUTE
        # prefix with a trailing variable (`-C /shared/root/$X reset --hard`)
        # must stay guarded even when the session cwd is elsewhere.
        _scope_dirs.append(os.path.realpath(os.path.expanduser(_unexpanded_c)))
    if not any(d == s or d.startswith(s + os.sep) for d in _scope_dirs for s in shared):
        if not _has_cotenants(run_dir):
            return 0
    # THE LOCK IS REPORTED ON THE ORDINARY PATH, not only inside the amend
    # verdict (AF-503, reported by mixpeek-frustrations as MF-842).
    #
    # A stale zero-byte .git/index.lock blocked every index write on ~/Dev/mixpeek
    # for 15+ minutes on 2026-09-04. Git's own message is generic and correct, and
    # on a 50-lane checkout it is indistinguishable from healthy contention, so
    # two lanes routed around it with GIT_INDEX_FILE temp-index grafts before
    # anyone understood the cause. Routing around a blockage is rational and it is
    # also how a 15-minute fleet stall produces no report.
    #
    # This REPORTS and never blocks or removes. Removing a lock is destructive on
    # a shared checkout and is the human's call (ethos rule 8); what the guard can
    # do is turn a generic message into a verdict, which costs one stat.
    try:
        sys.stderr.write(_index_lock_note(cmd, run_dir))
    except Exception:
        pass  # a note that cannot be produced must never block a command
    # AF-507: a no-pathspec commit that would sweep index-vs-frozen-HEAD drift.
    # Checked BEFORE the amend verdict because they are disjoint (the sweep
    # verdict returns None for --amend) and this one is the cheaper miss to
    # catch: an amend rewrites one commit, a sweep buries a peer's whole tree
    # under someone else's message.
    sweep_why = None
    try:
        sweep_why = _sweep_commit_verdict(cmd, scrubbed, run_dir)
    except Exception:
        sweep_why = None  # fail-open, same posture as the rest of the guard
    if sweep_why:
        if _consume_override(cmd):
            sys.stderr.write(f"amux guard: ALLOWED once (owner-sanctioned): {sweep_why}\n")
        else:
            sys.stderr.write(
                f"BLOCKED by amux shared-checkout guard: {sweep_why}.\n"
                f"'{run_dir}' is a SHARED checkout used by multiple agent sessions.{_dir_note}\n")
            return 2
    amend_why = None
    try:
        amend_why = _amend_verdict(cmd, scrubbed, run_dir)
    except Exception:
        amend_why = None  # fail-open, same posture as the rest of the guard
    if amend_why:
        if _consume_override(cmd):
            sys.stderr.write(f"amux guard: ALLOWED once (owner-sanctioned): {amend_why}\n")
        else:
            sys.stderr.write(
                f"BLOCKED by amux shared-checkout guard: {amend_why}.\n"
                f"'{run_dir}' is a SHARED checkout used by multiple agent sessions.{_dir_note}\n"
                f"OWNER-AUTHORIZED one-off: write the exact command to ~/.amux/guard-allow-once "
                f"and re-run (consumed once, audit-logged).\n")
            return 2
    # Path-scoped discard of a PEER's uncommitted work (AC-212). Runs before the
    # tree-wide table below, whose own remedy line tells callers to "scope to YOUR
    # OWN paths" — this is what makes "your own" enforced rather than advisory.
    discard_why = None
    _dv_err = None
    # RETRY BEFORE REFUSING (AC-287). The amux server re-execs on every save of
    # amux-server.py, which on this shared checkout happens many times an hour, so
    # a single 4s timeout is a routine event rather than an outage. Failing closed
    # on the first miss would refuse legitimate discards during every restart —
    # caught by a control in review: the guard blocked with a reachable server
    # purely because the call landed in a reload window. Three tries over ~6s costs
    # nothing on an operation this rare and removes that whole false-refusal class.
    for _dv_try in range(3):
        try:
            discard_why = _discard_verdict(cmd, scrubbed, run_dir)
            _dv_err = None
            break
        except Exception as _e:
            _dv_err = _e
            if _dv_try < 2:
                import time as _t   # inline, matching this hook's import style
                _t.sleep(2)
    if _dv_err is not None:
        # FAIL CLOSED HERE, unlike everywhere else in this guard (AC-287).
        # The standing contract is fail-OPEN: a guard that blocks when the server
        # is down wedges every lane, and for a COMMIT that trade is right — the
        # peer's work survives in the object store either way, so the cost of a
        # missed warning is a glance.
        #
        # A DISCARD is not that. It is UNRECOVERABLE: no object, no reflog entry.
        # A missed block is permanent data loss belonging to someone who is not
        # even at the keyboard. Measured 2026-08-07 with the server unreachable:
        # exit 0, no output, the destroy proceeded — and the only thing printed
        # was the co-tenant SKIP notice, which talks about COMMITTING a shared
        # file while the user is DISCARDING one, so it named the wrong operation
        # for the one case where the stakes are highest.
        #
        # This is the same asymmetry that motivated AC-221 — "both of us wrote
        # this" is a reason to let you commit and a reason to stop you deleting —
        # applied one level up, to the FAILURE MODE rather than the verdict. One
        # fail-open policy cannot serve a recoverable and an unrecoverable
        # consumer. The override below is the sanctioned escape and stays
        # available, so this is refusable, not a wedge.
        discard_why = ("cannot verify co-tenancy — the amux server is unreachable "
                       f"({type(_dv_err).__name__}). REFUSING an unrecoverable "
                       "discard rather than guessing: unlike a commit, this leaves "
                       "no object and no reflog entry, so a wrong guess is "
                       "permanent and the work destroyed may not be yours. Re-run "
                       "when the server answers, or use `git stash push -- <paths>` "
                       "which is reversible")
    if discard_why:
        if _consume_override(cmd):
            sys.stderr.write(f"amux guard: ALLOWED once (owner-sanctioned): {discard_why}\n")
        else:
            sys.stderr.write(
                f"BLOCKED by amux shared-checkout guard: {discard_why}.\n"
                f"'{run_dir}' is a SHARED checkout used by multiple agent sessions.{_dir_note}\n"
                f"OWNER-AUTHORIZED one-off: write the exact command to ~/.amux/guard-allow-once "
                f"and re-run (consumed once, audit-logged).\n")
            return 2
    for _entry in DANGER:
        # 2- or 3-tuple; the third element, when present, REPLACES the
        # sweeps-up-uncommitted-work remedy below rather than adding to it.
        pat, why = _entry[0], _entry[1]
        remedy = _entry[2] if len(_entry) > 2 else None
        if re.search(pat, scrubbed):
            if _consume_override(cmd):
                sys.stderr.write(f"amux guard: ALLOWED once (owner-sanctioned via ~/.amux/guard-allow-once): {why}\n")
                return 0
            _default_remedy = (
                "this discards or sweeps up EVERY session's uncommitted work. Scope to YOUR OWN "
                "paths instead: `git checkout -- <yourfile>`, `git stash push -- <yourpath>`, or "
                "commit your files. For pulls, fetch+rebase on committed state or verify the "
                "autostash popped.")
            sys.stderr.write(
                f"BLOCKED by amux shared-checkout guard: {why}.\n"
                f"'{run_dir}' is a SHARED checkout used by multiple agent sessions — "
                f"{remedy or _default_remedy}{_dir_note}\n"
                f"OWNER-AUTHORIZED one-off: after sign-off, write the exact command to "
                f"~/.amux/guard-allow-once and re-run (consumed once, audit-logged) — do NOT route around "
                f"the guard via reflog.\n")
            return 2
    return 0

if __name__ == "__main__":
    # The gate matters beyond convention: the test suite imports this module
    # to reach the pure decision functions, and an unconditional module-level
    # sys.exit made that import EXIT THE TEST PROCESS with 0 mid-run — a
    # whole suite reporting green while its tail never executed (AMUX-3407,
    # caught because the PASS line went missing, not because anything failed).
    try:
        _rc = main()
        # AF-151: one emission point for every block path — the individual
        # refusals each write their own reason, and none of them knew whether
        # the caller had joined other work to the git verb.
        if _rc == 2:
            try:
                sys.stderr.write(_skipped_half_note(_LAST_CMD))
            except Exception:
                pass  # the note must never turn a clean block into a crash
        sys.exit(_rc)
    except Exception:
        sys.exit(0)  # fail-open: a guard bug must never break tool calls
