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

    total = len(cases) + len(trio) + len(quad) + len(matrix) + _bodies + 1
    if failures:
        print(f"FAIL {len(failures)}/{total}:")
        for f in failures:
            print(" -", f)
        return 1
    print(f"ALL {total} PASS")
    return 0




if __name__ == "__main__":
    sys.exit(main())
