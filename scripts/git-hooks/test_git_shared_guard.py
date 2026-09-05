#!/usr/bin/env python3
"""Test matrix for git-shared-guard.py (MI-4083 extension of the 14-case set).

Axis under test: MENTION vs INVOCATION — a guarded git command merely mentioned
in documentation text (heredoc bodies, quoted strings) must PASS; the same
command actually invoked must BLOCK. Plus the AMUX_AMEND_EXPECT pin protocol
against a real git fixture (bare origin + clone).

Run: python3 ~/.amux/hooks/test_git_shared_guard.py   (exit 0 = all pass)
"""
import json
import os
import subprocess
import sys
import tempfile
import time

HOOK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "git-shared-guard.py")


def run_hook(command, cwd, shared_root, extra_env=None):
    env = dict(os.environ, AMUX_SHARED_CHECKOUTS=shared_root)
    env.update(extra_env or {})
    p = subprocess.run(
        [sys.executable, HOOK],
        input=json.dumps({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": cwd}),
        capture_output=True, text=True, env=env, timeout=30,
    )
    return p.returncode, p.stderr


def git(cwd, *args):
    return subprocess.run(("git", "-C", cwd) + args, capture_output=True, text=True).stdout.strip()


def main():
    tmp = tempfile.mkdtemp(prefix="guardtest-")
    origin = os.path.join(tmp, "origin.git")
    work = os.path.join(tmp, "work")
    subprocess.run(["git", "init", "--bare", "-q", "-b", "main", origin])
    subprocess.run(["git", "clone", "-q", origin, work], capture_output=True)
    git(work, "config", "user.email", "t@t")
    git(work, "config", "user.name", "t")
    open(os.path.join(work, "f.txt"), "w").write("1\n")
    git(work, "add", "f.txt")
    git(work, "commit", "-q", "-m", "c1")
    git(work, "push", "-q", "origin", "main")  # HEAD c1 = pushed

    cases = []  # (name, command, cwd, expect_block)
    A = lambda n, c, b: cases.append((n, c, work, b))

    # mention-vs-invocation: heredoc bodies (the MI-4083 false-positive class)
    A("heredoc-doc amend mention", "cat >> notes.md <<'EOF'\nthe rule: git commit --amend on a PUSHED commit is never ok\nEOF", False)
    A("heredoc-doc reset mention", "cat >> notes.md <<'EOF'\nnever run git reset --hard on shared trees\nEOF", False)
    A("second-heredoc mention", "cat > a <<'EOF'\nhi\nEOF\ncat > b <<'EOF'\ngit reset --hard is bad\nEOF", False)
    A("unquoted-tag heredoc mention", "tee -a x.md <<DOC\ngit clean -fd wipes everything\nDOC", False)
    # executable heredoc bodies must STILL be scanned
    A("bash heredoc real reset", "bash <<'EOF'\ngit reset --hard HEAD~1\nEOF", True),
    A("sh heredoc real clean", "sh <<EOF\ngit clean -fd\nEOF", True),
    # AF-316 — the SHARED INDEX, staged half. `git commit -a` was already
    # blocked; these reach the same hazard one step earlier.
    A("add -A bare", "git add -A", True)
    A("add . bare", "git add .", True)
    A("add --all bare", "git add --all", True)
    # `git add -- .` is the same command as `git add .` and is the obvious next
    # thing to type after being refused once — it must not read as "scoped".
    A("add -- . bypass", "git add -- .", True)
    # BOUNDED forms must PASS, or the rule stops being a scoping rule and
    # becomes a ban. Each of these is someone doing the right thing.
    A("add -A bounded by pathspec", "git add -A -- crates/amux-server/src/lib.rs", False)
    A("add named path", "git add crates/amux-server/src/lib.rs", False)
    A("add relative named path", "git add ./crates/amux-server/src/lib.rs", False)
    A("add interactive", "git add -p", False)
    A("add -u scoped to a dir", "git add -u crates/", False)
    # Mention, not invocation — the same class the heredoc pins above cover.
    A("add -A mentioned in a commit message", 'git commit -m "never git add -A here" -- f.txt', False)

    # quoted mentions (existing behavior, regression pins)
    A("quoted commit-msg mention", 'git commit -m "never git reset --hard again" -- f.txt', False)
    A("echo quoted amend mention", 'echo "recipe: git commit --amend needs a pin"', False)
    # real invocations still block (regression pins)
    A("real reset --hard", "git reset --hard HEAD~1", True)
    A("real mixed reset", "git reset HEAD~2", True)
    A("path-scoped reset ok", "git reset -- f.txt", False)
    A("real clean -fd", "git clean -fd", True)
    A("stash drop", "git stash drop", True)
    A("commit -a", 'git commit -a -m "x"', True)
    A("path-scoped commit ok", 'git commit -m "x" -- f.txt', False)
    # amend pin protocol (real fixture): HEAD c1 is PUSHED
    A("amend on pushed HEAD", 'git commit --amend --no-edit', True)

    # CASE 21 family (2026-07-07 fleet reset incident — mvs-infra): bare/
    # un-scoped stash internally reset --hards the WHOLE shared tree. Only
    # pathspec-scoped pushes + non-destructive subcommands pass.
    A("bare stash", "git stash", True)
    A("stash -q", "git stash -q", True)
    A("stash push unscoped", "git stash push", True)
    A("stash push -m unscoped", 'git stash push -m wip', True)
    A("stash save", "git stash save wip", True)
    # Target the test's OWN isolated tmp, not the literal /tmp: on macOS /tmp is
    # /private/tmp, which the fleet fills with per-session scratchpads, so it has
    # live cotenants and the guard (correctly) blocks a tree-wide stash there.
    # `tmp` is a fresh mkdtemp with no cotenants — line 90 already relies on that —
    # so it exercises the real property (target-scoping exempts an outside dir)
    # without a machine-dependent /tmp.
    A("stash -C outside-checkout ok (guard is target-scoped)", f"git -C {tmp} stash", False)
    A("stash -C shared-checkout", f"git -C {work} stash", True)
    A("stash push pathspec ok", "git stash push -- notes/", False)
    A("stash push untracked pathspec ok", "git stash push --include-untracked -- notes/", False)
    A("stash pop ok", "git stash pop", False)
    A("stash apply ok", "git stash apply", False)
    A("stash list ok", "git stash list", False)
    A("stash show ok", "git stash show -p", False)
    A("reset --hard no-target", "git reset --hard", True)
    # scoping: outside the shared root nothing blocks
    cases.append(("non-shared cwd reset", "git reset --hard", tmp, False))

    # AMUX-3462 (MF-703): a -C path spelled with a shell variable cannot be
    # resolved from command text. The guard must fall back to the cwd
    # inference (still blocking from a shared cwd), must NOT fabricate a
    # literal '<cwd>/$S/...' repo in its message, must keep guarding an
    # absolute shared prefix with a trailing variable even from outside, and
    # the literal-path escape must keep working.
    A("unexpanded -C var from shared cwd still blocks", "git -C $S/wipetest reset --hard", True)
    cases.append(("unexpanded -C var from outside passes",
                  "git -C $S/wipetest reset --hard", tmp, False))
    cases.append(("absolute shared -C with trailing var blocks from outside",
                  f"git -C {work}/$X reset --hard", tmp, True))
    scratch = os.path.join(tmp, "scratch-clone")
    os.makedirs(scratch, exist_ok=True)
    A("literal -C escape still works from shared cwd", f"git -C {scratch} reset --hard", False)

    # AMUX-3893 (tuple from mixpeek-cicd): a depth-limited fetch truncates the
    # SHARED history, and every `merge-base --is-ancestor` past the cut then
    # returns a bare exit 1 that is indistinguishable from a real "not an
    # ancestor". 2026-08-29: rev-list --count on ~/Dev/mixpeek fell ~38,700 -> 50
    # and four hours of "is fix X in sha Y" answered wrongly, silently
    # (TUBES-2339); the same trap produced a false "REVERT DETECTED" in CI the
    # same day (MG-1532).
    A("fetch --depth=", "git fetch --depth=1 origin", True)
    A("fetch --depth space", "git fetch --depth 50 origin", True)
    A("pull --depth=", "git pull --depth=1 origin main", True)
    A("fetch --depth after operands", "git fetch origin main --depth=1", True)
    A("fetch -q --depth with shas", "git fetch -q --depth=1 origin abc123 def456", True)
    A("fetch --shallow-since", "git fetch --shallow-since=2026-01-01 origin", True)
    A("fetch --shallow-exclude", "git fetch --shallow-exclude=v1.0 origin", True)
    A("fetch --depth with -C", f"git -C {work} fetch --depth=1", True)
    # The REMEDY must never be blocked, or the refusal names an action the guard
    # itself refuses — the shape ethos rule 3 is about.
    A("fetch --unshallow is the remedy", "git fetch --unshallow origin", False)
    A("fetch --deepen is the remedy", "git fetch --deepen=100 origin", False)
    A("fetch --deepen space", "git fetch --deepen 100 origin", False)
    A("plain fetch", "git fetch origin", False)
    A("fetch --all --prune", "git fetch --all --prune", False)
    A("pull --rebase", "git pull --rebase origin main", False)
    # `clone --depth` makes a NEW repo and cannot shallow this one. Blocking it
    # would false-positive on real callers that shallow-clone EXTERNAL repos.
    A("clone --depth= is not fetch", "git clone --depth=1 https://github.com/x/y /tmp/y", False)
    A("clone --depth space", "git clone --depth 1 https://github.com/x/y", False)

    failures = []
    for name, cmd, cwd, expect_block in cases:
        code, err = run_hook(cmd, cwd, work)
        blocked = code == 2
        if blocked != expect_block:
            failures.append(f"{name}: expected {'BLOCK' if expect_block else 'PASS'}, got {'BLOCK' if blocked else 'PASS'}\n  {err.strip()[:200]}")

    # AMUX-3462 message contract: the refusal for an unexpanded -C names the
    # real cause and the literal-path escape, and never asserts the
    # fabricated '<cwd>/$S/...' path as the repo.
    code, err = run_hook("git -C $S/wipetest reset --hard", work, work)
    if code != 2:
        failures.append(f"unexpanded -C message case: expected BLOCK, got rc={code}")
    else:
        if "UNEXPANDED" not in err:
            failures.append(f"unexpanded -C refusal must name the cause: {err.strip()[:200]}")
        if "$S/wipetest' is a SHARED checkout" in err or f"{work}/$S" in err:
            failures.append(f"refusal asserts a fabricated literal path as the repo: {err.strip()[:200]}")

    # unpushed-HEAD amend trio (needs a fresh unpushed commit)
    open(os.path.join(work, "f.txt"), "a").write("2\n")
    git(work, "add", "f.txt")
    git(work, "commit", "-q", "-m", "c2")  # unpushed
    head = git(work, "rev-parse", "HEAD")
    trio = [
        ("amend unpushed unpinned", "git commit --amend --no-edit", True),
        ("amend unpushed pinned-match", f"AMUX_AMEND_EXPECT={head} git commit --amend --no-edit", False),
        ("amend unpushed stale pin", "AMUX_AMEND_EXPECT=deadbeefdead git commit --amend --no-edit", True),
        # `-C` IS TWO FLAGS. Every case above spells the amend without one, and
        # that is why the hole below survived: an unanchored `-C\s+(\S+)` search
        # read `git commit --amend -C HEAD` as `git -C HEAD`, resolved run_dir to
        # <cwd>/HEAD, and the amend verdict failed open on the empty rev-parse.
        # Two of these rewrote a peer's unpushed commits on 2026-09-04 without a
        # word from the guard. The forms are otherwise identical to the trio, so
        # a matrix that never names the flag cannot see it.
        ("amend unpushed unpinned -C HEAD", "git commit --amend -C HEAD", True),
        ("amend unpushed unpinned -C sha", f"git commit --amend --no-verify -C {head}", True),
        # A GLOBAL FLAG BEFORE THE SUBCOMMAND HID IT ENTIRELY (AF-489). The two
        # cases above fixed the run_dir RESOLVER; the amend DETECTOR still
        # allowed only `(?:-C\s+\S+\s+)?` in front of `commit`, so any other
        # global flag meant the regex never matched and the verdict never ran.
        # A resolver that mis-resolves gives a wrong answer; a detector that
        # misses is a silent pass. Same reason the flag survived above: every
        # case in this matrix spelled the amend with NO global flag at all.
        ("amend unpushed, -c before subcommand",
         "git -c user.name=x commit --amend --no-edit", True),
        ("amend unpushed, repeated -c",
         "git -c a=b -c c=d commit --amend --no-edit", True),
        ("amend unpushed, --no-pager global",
         "git --no-pager commit --amend --no-edit", True),
        ("amend unpushed, -c then -C",
         f"git -c a=b -C {work} commit --amend --no-edit", True),
        ("amend unpushed, --git-dir= attached",
         f"git --git-dir={work}/.git commit --amend --no-edit", True),
        # NEGATIVE CONTROLS for the widened prefix. It must not start matching a
        # command that is not an amend, or the guard blocks ordinary reads.
        ("plain commit is not an amend", "git commit -m 'no amend here'", False),
        ("git log is not an amend", "git -c a=b log --oneline", False),
        ("diff -C is copy detection, not a global", "git diff -C -- a b", False),
        # THE CELL THAT PINS "STOPS AT THE FIRST BARE WORD". The three negatives
        # above are held by the literal `commit` anchor, not by the prefix, so a
        # prefix loosened to accept ANY token still passes them — measured, by
        # mutating `--?[A-Za-z]...` to `\S+` and watching all 126 stay green.
        # Only a case where a DIFFERENT subcommand precedes the anchor can tell
        # the two apart, which is what this is. git's own grammar ends the global
        # section at the first bare word, and the comment in the guard claims
        # exactly that; without this cell the claim is unenforced prose.
        ("a different subcommand is not a global flag",
         "git log commit --amend", False),
        # THE TREE-WIDE TABLE, which the first GIT_GLOBALS pass left on the
        # narrow prefix (AF-490). Reported by mixpeek-frustrations and
        # reproduced against the live hook before the fix: one global flag and
        # the tree-wide discard of ~50 lanes' uncommitted work was unguarded.
        #   git --no-pager reset --hard          exit 0
        #   git -c a=b reset --hard              exit 0
        #   git --literal-pathspecs reset --hard exit 0
        #   git --no-pager clean -fd             exit 0
        #   git --no-pager checkout -- .         exit 0
        ("reset --hard behind --no-pager", "git --no-pager reset --hard", True),
        ("reset --hard behind -c", "git -c a=b reset --hard", True),
        ("reset --hard behind --literal-pathspecs",
         "git --literal-pathspecs reset --hard", True),
        ("clean -fd behind --no-pager", "git --no-pager clean -fd", True),
        ("checkout -- . behind --no-pager", "git --no-pager checkout -- .", True),
        ("stash drop behind -c", "git -c a=b stash drop", True),
        ("add . behind --no-pager", "git --no-pager add .", True),
        # THE LOOKAHEAD DIRECTION mixpeek-frustrations flagged: GIT_GLOBALS ends
        # in a general dash-token arm that can also match a SUBCOMMAND flag, so
        # `git stash --quiet pop` could have the prefix eat `--quiet` before the
        # negative lookahead reads the verb. The failure direction there is a
        # FALSE REFUSAL on `pop`, which is the one verb people use to RECOVER
        # work, so it gets its own cells rather than a note.
        ("stash pop still passes", "git stash pop", False),
        ("stash pop behind a global still passes", "git --no-pager stash pop", False),
        ("stash pop behind a subcommand flag still passes",
         "git stash --quiet pop", False),
        ("stash apply still passes", "git -c a=b stash apply", False),
        ("amend unpushed pinned -C HEAD", f"AMUX_AMEND_EXPECT={head} git commit --amend -C HEAD", False),
        # Copy detection is the same collision on a read-only verb: harmless in
        # itself, and it proves the fix is about WHERE -C sits, not about amend.
        ("log -C is not a directory flag", "git log -C -M --oneline", False),
    ]
    for name, cmd, expect_block in trio:
        code, err = run_hook(cmd, work, work)
        blocked = code == 2
        if blocked != expect_block:
            failures.append(f"{name}: expected {'BLOCK' if expect_block else 'PASS'}, got {'BLOCK' if blocked else 'PASS'}\n  {err.strip()[:200]}")

    # AF-106 durable half (AMUX-3407): the staged-set ownership check on a
    # pinned BARE amend. The refusal branch is exercised in-process below (it
    # needs a server verdict); these subprocess cases pin the contracts that
    # must hold WITHOUT a server: fail-open on unreachable, pathspec bypass,
    # and the disable knob. All three run with real staged content, because
    # empty-staged short-circuits before any of them.
    open(os.path.join(work, "g.txt"), "w").write("staged\n")
    git(work, "add", "g.txt")
    head2 = git(work, "rev-parse", "HEAD")
    dead_url = {"AMUX_STAGED_GUARD_URL": "https://127.0.0.1:9", "AMUX_SESSION": "testlane"}
    quad = [
        ("amend pinned staged, server unreachable -> fail-open",
         f"AMUX_AMEND_EXPECT={head2} git commit --amend --no-edit", dead_url, False),
        ("amend pinned pathspec, staged -> scoped, no refusal",
         f"AMUX_AMEND_EXPECT={head2} git commit --amend --no-edit -- f.txt", dead_url, False),
        ("amend pinned staged, check disabled",
         f"AMUX_AMEND_EXPECT={head2} git commit --amend --no-edit",
         {**dead_url, "AMUX_AMEND_STAGED_GUARD": "0"}, False),
        ("amend pinned staged, no session -> human, ungated",
         f"AMUX_AMEND_EXPECT={head2} git commit --amend --no-edit",
         {**dead_url, "AMUX_SESSION": ""}, False),
    ]
    for name, cmd, extra, expect_block in quad:
        code, err = run_hook(cmd, work, work, extra_env=extra)
        blocked = code == 2
        if blocked != expect_block:
            failures.append(f"{name}: expected {'BLOCK' if expect_block else 'PASS'}, got {'BLOCK' if blocked else 'PASS'}\n  {err.strip()[:200]}")

    # The refusal decision itself, in-process (pure function, no server).
    import importlib.machinery
    guard = importlib.machinery.SourceFileLoader("_gsg", HOOK).load_module()
    dec = guard._amend_staged_decision
    matrix = [
        ("foreign refuses", {"foreign": [{"path": "a.rs"}]}, True),
        ("no foreign allows", {"foreign": [], "shared": [{"path": "b.rs"}]}, False),
        ("undecided allows", {"undecided": True, "foreign": [{"path": "a.rs"}]}, False),
        ("disabled allows", {"enabled": False, "foreign": [{"path": "a.rs"}]}, False),
        ("non-dict allows", "garbage", False),
    ]
    for name, verdict, expect_refuse in matrix:
        got = dec(verdict)
        if (got is not None) != expect_refuse:
            failures.append(f"decision {name}: expected {'refuse' if expect_refuse else 'allow'}, got {got!r}")
    if dec({"foreign": [{"path": "a.rs"}]}) and "ABSORB" not in dec({"foreign": [{"path": "a.rs"}]}):
        failures.append("decision refusal must name the absorption hazard")

    # AF-156: EVERY POST to /api/git/staged-guard must carry `op`.
    #
    # The server decides whether a caller is a pre-rust hook with
    # `guard_version < 2 && !has_explicit_op` (git_guard.rs `hook_is_outdated`),
    # and its doc comment justifies that by asserting "every modern client sends
    # at least `op`". This file is the client that premise is about, and its
    # cotenant probe sent NEITHER field — so the fix landed at 79e9c89c 06:12 on
    # 2026-08-24 and 212 OUTDATED HOOK WARNs followed it, including one naming
    # this checkout at 16:23:51 whose hook was byte-identical to source.
    #
    # Parsed from the AST, not grepped: a grep for `"op"` is satisfied by the
    # two bodies that already had it while a third omits it, which is exactly
    # how this survived. Keyed on `paths`, which every staged-guard body carries
    # and nothing else here does.
    import ast as _ast
    _src = _ast.parse(open(HOOK).read())
    _bodies = 0
    for _n in _ast.walk(_src):
        if not (isinstance(_n, _ast.Call) and isinstance(_n.func, _ast.Attribute)
                and _n.func.attr == "dumps" and _n.args
                and isinstance(_n.args[0], _ast.Dict)):
            continue
        _keys = [k.value for k in _n.args[0].keys
                 if isinstance(k, _ast.Constant) and isinstance(k.value, str)]
        if "paths" not in _keys:
            continue
        _bodies += 1
        if "op" not in _keys:
            failures.append(
                f"staged-guard POST body at line {_n.lineno} sends no `op` "
                f"(keys: {_keys}) — the server will class it a pre-rust hook and "
                f"warn hourly that a current hook is outdated (AF-156)")
    # Vacuity guard: if the walk matched nothing, the loop above passes against
    # a file that sends no `op` anywhere.
    if _bodies < 3:
        failures.append(
            f"found only {_bodies} staged-guard POST bodies — the AST walk is not "
            f"reaching them, so the `op` check above is vacuous")

    # ── AMUX-3890: a redirect is not a pathspec ────────────────────────────
    #
    # Tested at the helper rather than end-to-end ON PURPOSE. `_discard_verdict`
    # POSTs to /api/git/staged-guard and FAILS OPEN when the server is
    # unreachable, so an end-to-end "this command is not blocked" case passes
    # just as green with the bug present as with it fixed. It would pin nothing.
    # `_strip_redirections` is the seam where the defect actually lives and the
    # only place a check here can genuinely fail (ethos rule 7).
    import importlib.util as _ilu
    _spec = _ilu.spec_from_file_location("_gsg", HOOK)
    _gsg = _ilu.module_from_spec(_spec)
    _spec.loader.exec_module(_gsg)
    _operands = _gsg._discard_operands

    # Called through `_discard_operands`, the WIRED path, not `_strip_redirections`
    # directly. The first version of this test called the helper and passed a
    # mutation that deleted its only call site, which is ethos rule 7 exactly: a
    # check pinning the wrong layer is as green as one pinning the right layer.
    # Inputs are raw command strings, so the operand regex, shlex, the `--` split
    # and the redirection strip are all under test together.
    D = "/Users/ethan/Dev/mixpeek"
    redir_cases = [
        # The reported specimen. `2>&1` is truncated to a bare `2>` by the operand
        # regex (it stops at `&`), and that `2>` was reaching the path list and
        # being reported as another session's uncommitted file.
        ("reported specimen",
         f"git -C {D} checkout origin/main -- docs/platform/syncs.mdx docs/retrieval/cookbook.mdx 2>&1 | head -30",
         ["docs/platform/syncs.mdx", "docs/retrieval/cookbook.mdx"]),
        ("fused 2>/dev/null",
         "git checkout origin/main -- a.mdx 2>/dev/null", ["a.mdx"]),
        ("stdout to file",
         "git checkout origin/main -- a.mdx > out.txt", ["a.mdx"]),
        ("append to file",
         "git checkout origin/main -- a.mdx >> log", ["a.mdx"]),
        ("fused >out",
         "git checkout origin/main -- a.mdx >out.txt", ["a.mdx"]),
        ("both streams",
         "git checkout origin/main -- a.mdx &> log", ["a.mdx"]),
        ("stdin redirect",
         "git checkout origin/main -- a.mdx < in.txt", ["a.mdx"]),
        ("restore with redirect",
         "git restore --worktree --source=origin/main -- a.mdx 2>&1", ["a.mdx"]),
        ("no-dashdash form with redirect",
         "git checkout origin/main a.mdx 2>/dev/null", ["a.mdx"]),
        # Must NOT over-strip: real pathspecs still arrive, redirect or not.
        ("plain, no redirect",
         "git checkout origin/main -- a.mdx b.mdx", ["a.mdx", "b.mdx"]),
        ("path containing >",
         "git checkout origin/main -- 'weird>name.txt'", ["weird>name.txt"]),
    ]
    for _name, _cmd, _want in redir_cases:
        _paths, _ = _operands(_cmd)
        if _paths != _want:
            failures.append(
                f"_discard_operands/{_name}: paths {_paths} != expected {_want} "
                f"(from {_cmd!r}) — a redirection token reaching the path list is "
                f"AMUX-3890, where the guard reported a file literally named `2>` "
                f"as another session's uncommitted work while clearing both real "
                f"paths in the same message")

    # ---- MR-101: a CONSUMED authorization implies the command was ALLOWED ----
    #
    # mixpeek-research saw one tool call produce BOTH "ALLOWED once" and
    # "BLOCKED", the marker gone, the consumption in the audit log, and git never
    # running. The discard branch is straight-line code evaluated once per
    # process, so one process cannot print both: the hook ran TWICE for one tool
    # call. The first run consumed the marker and allowed, the second found no
    # marker and blocked, and the block is what the tool call returned.
    #
    # These run the guard as a SUBPROCESS twice with the same command, against a
    # real HOME, so the marker/audit files are the shipped ones rather than a
    # paraphrase of them.
    import pathlib, time as _time
    _home = tempfile.mkdtemp(prefix="guardhome-")
    (pathlib.Path(_home) / ".amux" / "logs").mkdir(parents=True, exist_ok=True)
    _marker = pathlib.Path(_home) / ".amux" / "guard-allow-once"
    _audit = pathlib.Path(_home) / ".amux" / "logs" / "guard-overrides.jsonl"
    # A command the guard blocks on its own: discarding another session's work.
    # `git checkout HEAD -- f.txt` against a dirty f.txt is the discard shape.
    open(os.path.join(work, "f.txt"), "w").write("dirty\n")
    _discard = "git checkout HEAD -- f.txt"

    def _hook(cmd, env=None):
        # AMUX_URL points at a dead port on purpose. The discard branch fires
        # either when a peer owns the path or when co-tenancy CANNOT BE VERIFIED
        # ("REFUSING an unrecoverable discard rather than guessing"), and the
        # second is reachable from a temp fixture where the first is not — a
        # scratch repo has no peer edit records. It is also the exact shape the
        # reporter saw beside their incident: byo-ray hit "the amux server is
        # unreachable (TimeoutError)" from this guard minutes earlier.
        e = dict(os.environ, AMUX_SHARED_CHECKOUTS=work, HOME=_home,
                 AMUX_URL="https://127.0.0.1:9")
        e.update(env or {})
        p = subprocess.run(
            [sys.executable, HOOK],
            input=json.dumps({"tool_name": "Bash", "tool_input": {"command": cmd}, "cwd": work}),
            capture_output=True, text=True, env=e, timeout=30)
        return p.returncode, p.stderr

    _marker.write_text(_discard)
    rc1, err1 = _hook(_discard)
    consumed = not _marker.exists()
    rc2, err2 = _hook(_discard)

    if consumed and rc1 != 0:
        failures.append(
            "MR-101/consumed-implies-allowed: the marker was CONSUMED but the run "
            f"that consumed it returned {rc1}. A consumption that does not allow "
            "spends the owner's one-off on nothing")
    if consumed and rc2 != 0:
        failures.append(
            "MR-101/replay: second invocation of the SAME command returned "
            f"{rc2} after the first consumed the marker. This is the reported "
            f"bug: one tool call, both verdicts, git never ran. stderr: {err2[:200]}")
    if consumed and rc2 == 0 and "REPLAYED" not in err2:
        failures.append(
            "MR-101/replay-is-loud: the replay was allowed but said nothing. "
            "Allowing twice must be greppable, not silent")

    # CONTROL 1 — a DIFFERENT destructive command inside the window gets nothing.
    # Without this the replay could be 'any recent override allows anything'.
    rc3, _ = _hook("git reset --hard HEAD~1")
    if rc3 == 0:
        failures.append(
            "MR-101/control-different-command: an unrelated destructive command "
            "was allowed inside the replay window — the window must key on the "
            "AUTHORIZED TEXT, not on 'an override happened recently'")

    # CONTROL 2 — the window EXPIRES. Backdate the audit record past the window
    # and the same command must block again, or 'allow once' has become 'allow
    # forever' for any command an owner ever sanctioned.
    if not _audit.exists():
        failures.append(
            "MR-101/premise: the discard branch never fired, so none of these cells "
            "measured anything. The fixture must reach a state where the guard "
            "refuses a discard (a peer-owned path, or an unverifiable co-tenancy)")
        _rows = []
    else:
        _rows = [json.loads(l) for l in _audit.read_text().splitlines() if l.strip()]
    for _r in _rows:
        _r["ts"] = _time.time() - 3600
    if _rows:
        _audit.write_text("".join(json.dumps(r) + "\n" for r in _rows))
    rc4, _ = _hook(_discard)
    if _rows and rc4 == 0:
        failures.append(
            "MR-101/control-window-expires: the same command was still allowed an "
            "hour after its authorization was consumed — the replay window is not "
            "bounded, so a one-off has become permanent")

    # CONTROL 3 — the window can be switched OFF, restoring strict one-shot.
    _audit.write_text("")
    _marker.write_text(_discard)
    _hook(_discard, {"AMUX_GUARD_ALLOW_REPLAY_S": "0"})
    rc5, _ = _hook(_discard, {"AMUX_GUARD_ALLOW_REPLAY_S": "0"})
    if rc5 == 0:
        failures.append(
            "MR-101/control-disable: AMUX_GUARD_ALLOW_REPLAY_S=0 must restore the "
            "strict one-shot behaviour, and did not")
    _mr101 = 6

    # ------------------------------------------------------------------
    # AMUX-3932: COMMAND SUBSTITUTION INSIDE A QUOTED ARGUMENT.
    #
    # THE INCIDENT. A lane built a JSON body whose PROSE quoted the very commands
    # its card was about. Bash substituted them and actually ran `git add -A`,
    # staging 779 paths across the shared checkout, 765 of them other lanes'.
    # Nothing destructive armed and it was repaired with a scoped reset, but the
    # staged-guard was down at that moment and logged NOT ENFORCED, so nothing
    # caught it either.
    #
    # THE DEFECT was not "python3 bodies are treated as prose" (that hypothesis
    # was tested and did not reproduce). It was that the scrubber could not tell
    # an INERT quoted region from one bash will EXPAND. Two independent regex
    # passes ran over the whole string, so single-quote stripping also fired
    # INSIDE double-quoted regions where those quotes are literal characters --
    # which is why the two substitution syntaxes disagreed in exactly one place.
    #
    # THE CONTROLS ARE HALF THE POINT. Stripping quoted bodies is RIGHT: it is
    # what lets a lane file a card whose text mentions a guarded command. A fix
    # that blocked every mention would pass the bypass rows below and make the
    # guard unusable, which is the noise the stripper exists to prevent.
    #
    # Matrix reproduced against the live guard by amux and independently by
    # mixpeek-homepage-claude, each feeding crafted strings on stdin so nothing
    # executed. Their run caught a broken harness first (main() returns 0 unless
    # tool_name == "Bash"), which is why every row here goes through run_hook.
    subst = [
        # (name, command, expect_block)
        ("bare invocation still blocks",
         "git stash", True),
        ("single-quoted mention is INERT and must be allowed",
         "amux board add 'x $(git stash)'", False),
        ("single-quoted backticks are inert too",
         "python3 -c 'x = `git stash`'", False),
        ("unquoted substitution",
         "echo $(git stash)", True),
        # The four rows that bypassed. echo reports to nobody, which is what
        # proves this is about QUOTING and not about which command carries a
        # payload -- the original narrative blamed curl and `amux board add`
        # and was withdrawn on this evidence.
        ("double-quoted $() expands and must block",
         'echo "$(git stash)"', True),
        ("double-quoted backticks expand and must block",
         'echo "`git stash`"', True),
        ("curl payload with $()",
         'curl -d "$(git stash)" http://x', True),
        ("curl payload with backticks",
         'curl -d "`git stash`" http://x', True),
        ("board add with $() -- the exact shape of the incident",
         'amux board add "note: $(git stash)"', True),
        ("board add with backticks",
         'amux board add "note: `git stash`"', True),
        # THE ROW WHERE THE TWO SYNTAXES DIVERGED. $( ) was blocked here by
        # ACCIDENT (escaped inner quotes desynced the regex) while backticks
        # sailed through, and the backtick form is what caused the incident.
        ("python3 -c body, $() form",
         'python3 -c "print(\\"$(git stash)\\")"', True),
        ("python3 -c body, backtick form inside literal triple-quotes",
         "python3 -c \"x = '''note `git stash` here'''\"", True),
        # AMUX-3932 extension found while fixing: an UNQUOTED heredoc delimiter
        # expands regardless of the sink. Verified against bash directly --
        # `python3 <<EOF` prints the EXPANDED value, `python3 <<'EOF'` prints the
        # literal text.
        ("unquoted heredoc delimiter expands, $()",
         'python3 <<EOF\nx = "$(git stash)"\nEOF', True),
        ("unquoted heredoc delimiter expands, backticks",
         'python3 <<EOF\nx = "`git stash`"\nEOF', True),
        ("quoted heredoc delimiter is inert",
         "python3 <<'EOF'\nx = \"$(git stash)\"\nEOF", False),
        # FALSE-POSITIVE CONTROLS. This hook runs on EVERY Bash call in the
        # fleet, so an over-broad match is a worse outage than the bypass: it
        # blocks ~55 lanes at once. Each of these mentions a guarded command in
        # text bash will never run.
        ("prose in a quoted heredoc body",
         "amux board add --stdin <<'EOF'\nfixing the git add -A misuse\nEOF", False),
        ("prose in an unquoted heredoc body with no substitution",
         "python3 <<EOF\n# a note about git stash and git add -A\nEOF", False),
        ("a double-quoted string that mentions a command but expands nothing",
         'amux board add "we should never run git add -A here"', False),
        ("a harmless substitution beside a mention must NOT block",
         'amux board add "mentions git stash, stamped $(date)"', False),
        ("grep for the string in a file",
         "grep -n 'git stash' scripts/*.sh", False),
    ]
    for _name, _cmd, _expect in subst:
        _rc, _err = run_hook(_cmd, work, tmp)
        if (_rc != 0) != _expect:
            failures.append(
                "AMUX-3932/%s: expected %s, got %s for %r"
                % (_name, "BLOCK" if _expect else "ALLOW",
                   "BLOCK" if _rc != 0 else "ALLOW", _cmd))
    _subst = len(subst)

    # ---- .git/index.lock is REPORTED on the ordinary path (AF-503 / MF-842) ----
    # A stale zero-byte lock blocked every index write on ~/Dev/mixpeek for 15+
    # minutes and nothing detected it; two lanes routed around it with temp-index
    # grafts before anyone understood the cause. The guard reports; it never
    # blocks or removes, because removing a lock on a shared checkout is the
    # human's call.
    _lock = os.path.join(work, ".git", "index.lock")
    _lockcases = 0

    def _note_for(command):
        _rc, _err = run_hook(command, work, tmp)
        return _rc, _err

    # 1. NO LOCK: silent. Otherwise every commit on a healthy checkout gets noise.
    _lockcases += 1
    _rc, _err = _note_for("git commit -m x")
    if "index.lock" in _err:
        failures.append("lock-note: fired with no lock present: %r" % _err[:160])

    # 2. STALE (0 bytes, aged past the threshold, no holder): full verdict.
    open(_lock, "w").close()
    _old = time.time() - 1200
    os.utime(_lock, (_old, _old))
    for _n, _c, _want in (
        ("commit sees it", "git commit -m x", True),
        ("add sees it", "git add f.txt", True),
        # A command that does NOT write the index must stay silent, or the note
        # becomes noise on every push in a repo with a live commit in flight.
        ("push does not", "git push origin main", False),
        ("log does not", "git log --oneline", False),
    ):
        _lockcases += 1
        _rc, _err = _note_for(_c)
        if ("index.lock" in _err) != _want:
            failures.append("lock-note/%s: expected note=%s for %r, got %r"
                            % (_n, _want, _c, _err[:160]))

    # 3. THE VERDICT NAMES ITS EVIDENCE, or it is just a restatement of git's own
    #    generic message, which is the thing being fixed.
    _lockcases += 1
    _rc, _err = _note_for("git commit -m x")
    for _needle in ("age ", "0 bytes", "holder:", "YOUR call"):
        if _needle not in _err:
            failures.append("lock-note: verdict omits %r: %r" % (_needle, _err[:220]))
    _lockcases += 1
    if _rc != 0:
        failures.append("lock-note must REPORT, never block: rc=%s" % _rc)

    # 4. A HOLDER PROBE THAT CANNOT RUN MUST SAY SO. mixpeek-frustrations lost ten
    #    minutes to `lsof f 2>/dev/null || echo no holder`, which printed the
    #    reassuring branch because lsof is not on PATH here. An absent tool's
    #    negative must never read as "unheld", or a future reaper deletes live
    #    locks on a box without lsof.
    _lockcases += 1
    _rc, _err = _note_for("git commit -m x")
    if "unheld" in _err and "no process holds it open" not in _err:
        failures.append("lock-note: claimed unheld without the probe having run")

    # 5. THE ABSENT-TOOL BRANCH, reached with AMUX_LSOF pointed at nothing. It
    #    cannot be reached any other way on a box that HAS lsof, and it is the
    #    branch that decides whether a future reaper deletes a LIVE lock. It must
    #    say `unmeasured` and must NOT say `unheld`.
    _lockcases += 1
    _rc, _err = run_hook("git commit -m x", work, tmp,
                         extra_env={"AMUX_LSOF": "/nonexistent/lsof"})
    if "unmeasured" not in _err or "did NOT run" not in _err:
        failures.append("lock-note: an absent lsof did not report unmeasured: %r" % _err[:220])
    if "no process holds it open" in _err:
        failures.append("lock-note: an absent lsof reported the lock UNHELD: %r" % _err[:220])
    os.unlink(_lock)

    # ------------------------------------------------------------------
    # AF-507 — a bare `git commit` that would sweep index-vs-frozen-HEAD drift
    #
    # backend, 2026-09-04: `git add <file>` hit a peer's index.lock and failed,
    # so their file was never staged. The follow-up bare `git commit -m` then
    # committed the whole index-vs-HEAD drift — 1120 files, +67067/-6296 — under
    # their message. `git reset --soft HEAD~1` to undo it was blocked by this
    # same guard, correctly. The guard blocked the fix and not the cause.
    #
    # The fixture reproduces the graft-push shape exactly: HEAD detached at an
    # OLD commit while the index carries origin/main. That is what makes the
    # staged set huge and its files identical to origin.
    _sweep = 0
    _so = os.path.join(tmp, "sweeporigin.git")
    _sw = os.path.join(tmp, "sweepwork")
    subprocess.run(["git", "init", "--bare", "-q", "-b", "main", _so])
    subprocess.run(["git", "clone", "-q", _so, _sw], capture_output=True)
    git(_sw, "config", "user.email", "t@t")
    git(_sw, "config", "user.name", "t")
    open(os.path.join(_sw, "base.txt"), "w").write("base\n")
    git(_sw, "add", "base.txt")
    git(_sw, "commit", "-q", "-m", "base")
    _old = git(_sw, "rev-parse", "HEAD")
    for _i in range(40):
        open(os.path.join(_sw, f"drift{_i}.txt"), "w").write(f"{_i}\n")
    git(_sw, "add", "-A")
    git(_sw, "commit", "-q", "-m", "40 files that are NOT this lane's work")
    git(_sw, "push", "-q", "origin", "main")

    def _sweep_hook(cmd, cwd, env=None):
        e = dict(os.environ, AMUX_SHARED_CHECKOUTS=cwd)
        e.update(env or {})
        p = subprocess.run(
            [sys.executable, HOOK],
            input=json.dumps({"tool_name": "Bash", "tool_input": {"command": cmd}, "cwd": cwd}),
            capture_output=True, text=True, env=e, timeout=40)
        return p.returncode, p.stderr

    # THE SPECIMEN: HEAD frozen at `base`, index carrying origin/main.
    git(_sw, "checkout", "-q", "--detach", _old)
    git(_sw, "read-tree", "origin/main")

    _sweep += 1
    _rc, _err = _sweep_hook("git commit -m 'my one-line fix'", _sw)
    if _rc != 2:
        failures.append(
            "AF-507/sweep: a bare commit over 40 drift files was ALLOWED (rc=%s). "
            "This is the reported incident's exact shape. stderr: %r" % (_rc, _err[:300]))
    elif "already match origin/main" not in _err:
        failures.append(
            "AF-507/sweep-why: the refusal does not say the files already match origin, "
            "which is the whole reason they are not the caller's work: %r" % _err[:300])

    # RULE 1 ALONE — a PATHSPEC commit over the same index must PASS. `git commit
    # <paths>` ignores the index for everything it does not name, so the drift
    # cannot ride along. Without this cell the guard is a ban on committing at
    # all while HEAD lags, which is the state every graft-push lane is in.
    _sweep += 1
    _rc, _err = _sweep_hook("git commit base.txt -m 'my one-line fix'", _sw)
    if _rc != 0:
        failures.append(
            "AF-507/pathspec: a path-scoped commit was blocked (rc=%s). The drift cannot "
            "ride along on a pathspec commit. stderr: %r" % (_rc, _err[:300]))

    _sweep += 1
    _rc, _err = _sweep_hook("git commit -m x -- base.txt", _sw)
    if _rc != 0:
        failures.append(
            "AF-507/dashdash: an explicit `-- <path>` commit was blocked (rc=%s): %r"
            % (_rc, _err[:300]))

    # THE PIN. A number, not a switch, so the escape requires having run the
    # count — the same protocol as AMUX_AMEND_EXPECT.
    _sweep += 1
    _rc, _err = _sweep_hook("git commit -m x", _sw,
                            env={"AMUX_ALLOW_SWEEP_COMMIT": "40"})
    if _rc != 0:
        failures.append(
            "AF-507/pin: the correct drift count did not release the commit (rc=%s): %r"
            % (_rc, _err[:300]))

    _sweep += 1
    _rc, _err = _sweep_hook("git commit -m x", _sw,
                            env={"AMUX_ALLOW_SWEEP_COMMIT": "1"})
    if _rc != 2 or "does not match" not in _err:
        failures.append(
            "AF-507/pin-wrong: a WRONG pin was accepted (rc=%s). A pin that any number "
            "satisfies is a switch, and a switch gets set once and never read: %r"
            % (_rc, _err[:300]))

    # RULE 2 ALONE — THE CONTROL, and the cell that keeps this from being a
    # file-count ban. HEAD == origin/main, 40 genuinely NEW files staged. Same
    # size, no drift, and it must PASS. Without it, a real 40-file refactor is
    # refused and the guard trains people to set the pin reflexively.
    _sweep += 1
    git(_sw, "checkout", "-q", "main")
    git(_sw, "reset", "-q", "--hard", "origin/main")
    for _i in range(40):
        open(os.path.join(_sw, f"mine{_i}.txt"), "w").write(f"real work {_i}\n")
    git(_sw, "add", "-A")
    _rc, _err = _sweep_hook("git commit -m 'a genuine 40-file change'", _sw)
    if _rc != 0:
        failures.append(
            "AF-507/control: a genuine 40-file commit with NO drift was blocked (rc=%s). "
            "The signal is drift, not size. stderr: %r" % (_rc, _err[:300]))

    total = (len(cases) + len(trio) + len(quad) + len(matrix) + _bodies + 1
             + len(redir_cases) + _mr101 + _subst + _lockcases + _sweep)
    if failures:
        print(f"FAIL {len(failures)}/{total}:")
        for f in failures:
            print(" -", f)
        return 1
    print(f"ALL {total} PASS")
    return 0




if __name__ == "__main__":
    sys.exit(main())
