#!/usr/bin/env python3
"""The pre-push script-tests leg (AF-1073).

land_gate.py runs Rust tests and e2e specs and no scripts/test-*.sh, so a
script whose only test is a checks.yml step landed untested and reddened main
(5d52181bc on 2026-10-30, 8d6057fb7 on 10-31). pre-push now runs the
checks.yml shell tests that name a changed script, on the pushed commit's
bytes. This drives the real hook against a scratch repo whose scripts and
checks.yml are fixtures.

THE CONTROL COMES FIRST. test-other.sh fails whenever README.md says "red",
and the first push makes it say "red" while touching no script. If selection
did not skip the leg, that push would run test-other.sh and could be refused.

Run: python3 scripts/git-hooks/test_pre_push_script_tests.py   (exit 0 = pass)
"""
import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HOOK = os.path.join(ROOT, "scripts", "git-hooks", "pre-push")

CHECKS = """jobs:
  checks:
    steps:
      - run: |
          ./scripts/test-widget.sh
          bash scripts/test-other.sh && bash scripts/test-env.sh
      # ./scripts/test-commented.sh is not a step
      - run: bash scripts/test-unrelated.sh
"""
WIDGET = "#!/usr/bin/env bash\necho GOOD\n"
TEST_WIDGET = """#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
[ "$(bash scripts/widget.sh)" = GOOD ] || { echo "FAIL: widget.sh did not say GOOD"; exit 1; }
echo "test-widget: 1 passed, 0 failed"
"""
# Names widget.sh, so a widget change selects it; red while README says red.
TEST_OTHER = """#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# also exercises scripts/widget.sh
if grep -q red README.md; then echo "FAIL: the readme is red"; exit 1; fi
echo "test-other: 1 passed, 0 failed"
"""
# Fails if the hook handed it the pusher's amux session or home.
TEST_ENV = """#!/usr/bin/env bash
set -euo pipefail
# runs beside scripts/widget.sh
[ -z "${AMUX_URL:-}" ] || { echo "FAIL: AMUX_URL reached the test"; exit 1; }
[ "$HOME" != "$PUSHER_HOME" ] || { echo "FAIL: the pusher's HOME reached the test"; exit 1; }
echo "test-env: 1 passed, 0 failed"
"""

# Names no script; red while README says red. Selection must leave it out.
TEST_UNRELATED = """#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if grep -q red README.md; then echo "FAIL: unrelated is red"; exit 1; fi
echo "test-unrelated: 1 passed, 0 failed"
"""

passed = failed = 0


def check(label, got, want):
    global passed, failed
    if got == want:
        print("  ok    %-62s %s" % (label, got))
        passed += 1
    else:
        print("  FAIL  %-62s got %s want %s" % (label, got, want))
        failed += 1


def git(work, *args, env=None):
    e = {k: v for k, v in os.environ.items()
         if k not in ("AMUX_SESSION", "AMUX_SKIP_SCRIPT_TESTS", "AMUX_SKIP_SPA_GATES")}
    e.update({"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
              "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t", "HOME": work,
              "PUSHER_HOME": work, "AMUX_URL": "https://127.0.0.1:9"})
    if env:
        e.update(env)
    return subprocess.run(["git", "-C", work] + list(args),
                          capture_output=True, text=True, timeout=600, env=e)


def write(work, rel, text):
    p = os.path.join(work, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    open(p, "w").write(text)


def commit(work, msg, *paths):
    git(work, "add", *paths, "--")
    git(work, "commit", "-qm", msg)


def main():
    tmp = tempfile.mkdtemp(prefix="prepush-script-tests-")
    try:
        origin = os.path.join(tmp, "origin.git")
        work = os.path.join(tmp, "work")
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", origin], check=True)
        subprocess.run(["git", "init", "-q", "-b", "main", work], check=True)
        git(work, "remote", "add", "origin", origin)
        write(work, "README.md", "red\n")
        write(work, ".github/workflows/checks.yml", CHECKS)
        write(work, "scripts/widget.sh", WIDGET)
        write(work, "scripts/test-widget.sh", TEST_WIDGET)
        write(work, "scripts/test-other.sh", TEST_OTHER)
        write(work, "scripts/test-env.sh", TEST_ENV)
        write(work, "scripts/test-unrelated.sh", TEST_UNRELATED)
        commit(work, "fixture", ".")
        shutil.copy(HOOK, os.path.join(work, ".git", "hooks", "pre-push"))
        os.chmod(os.path.join(work, ".git", "hooks", "pre-push"), 0o755)
        # A first push has no tip to compare with, so a red cannot be shown to
        # be new (AF-1128: this refused every fixture repo's seed push).
        r = git(work, "push", "origin", "main")
        check("a first push with a red test is allowed", r.returncode, 0)
        check("and says it could not compare", "verdict=script_tests_unmeasured" in r.stderr, True)
        write(work, "README.md", "fine\n")
        commit(work, "greens the fixture", "README.md")
        check("seed push is allowed", git(work, "push", "-q", "origin", "main").returncode, 0)

        # THE CONTROL: touches no script and turns test-other.sh red.
        write(work, "README.md", "red\n")
        commit(work, "unrelated: touches no script", "README.md")
        r = git(work, "push", "origin", "main")
        check("push touching no script is allowed", r.returncode, 0)
        check("and the leg did not run", "script tests on" in r.stderr, False)

        # THE REFUSAL, on the pushed bytes: the worktree goes back to GOOD
        # before pushing, so a leg reading the worktree would pass.
        write(work, "scripts/widget.sh", "#!/usr/bin/env bash\necho BAD\n")
        commit(work, "breaks widget.sh", "scripts")
        write(work, "scripts/widget.sh", WIDGET)
        r = git(work, "push", "origin", "main")
        check("push breaking a script's checks.yml test is refused", r.returncode != 0, True)
        if r.returncode == 0:
            print("  hook stderr:\n    " + "\n    ".join(r.stderr.strip().splitlines()[-12:]))
        tail = r.stderr.split("BLOCKED")[-1]
        check("refusal names the failing test", "scripts/test-widget.sh" in tail, True)
        check("and quotes its output", "widget.sh did not say GOOD" in tail, True)
        check("a red already on the remote is not blamed on this push",
              "scripts/test-other.sh" in tail, False)
        check("it counts that red as already failing",
              "1 failing from this push, 1 already failing on the remote" in r.stderr, True)
        check("the test saw no amux session and a scratch HOME",
              "AMUX_URL reached" in r.stderr or "pusher's HOME reached" in r.stderr, False)
        check("a test naming no changed script is not run", "3 of 4 wired tests" in r.stderr, True)
        check("refusal names the override", "AMUX_SKIP_SCRIPT_TESTS" in r.stderr, True)
        check("verdict field", "verdict=script_tests_fail" in r.stderr, True)

        # THE ESCAPE (ethos rule 3), then a fix that only inherits a red.
        check("override lands it anyway",
              git(work, "push", "origin", "main", env={"AMUX_SKIP_SCRIPT_TESTS": "1"}).returncode, 0)
        git(work, "checkout", "--", ".")
        write(work, "scripts/widget.sh", "#!/usr/bin/env bash\n# fixed\necho GOOD\n")
        commit(work, "fixes widget.sh", "scripts")
        r = git(work, "push", "origin", "main")
        check("a push whose only red is inherited is allowed", r.returncode, 0)
        check("and says the tests ran", "verdict=script_tests_pass" in r.stderr, True)
        check("and how many it selected", "3 of 4 wired tests" in r.stderr, True)

        # A changed hook runs the hook tests that name it (AF-1128), and one
        # that names another hook stays out.
        write(work, "scripts/git-hooks/thing", "#!/bin/sh\nexit 0\n")
        write(work, "scripts/git-hooks/other", "#!/bin/sh\nexit 0\n")
        write(work, "scripts/git-hooks/test_thing.py",
              "import os, sys\n# tests scripts/git-hooks/thing\nsys.exit(1 if open('scripts/git-hooks/thing').read().count('BAD') else 0)\n")
        write(work, "scripts/git-hooks/test_other.py",
              "import sys\n# tests scripts/git-hooks/other\nsys.exit(1 if open('scripts/git-hooks/th' + 'ing').read().count('BAD') else 0)\n")
        commit(work, "adds hooks", "scripts")
        check("adding the hooks is allowed", git(work, "push", "origin", "main").returncode, 0)
        write(work, "scripts/git-hooks/thing", "#!/bin/sh\n# BAD\nexit 0\n")
        commit(work, "breaks thing", "scripts")
        r = git(work, "push", "origin", "main")
        check("a change a hook test rejects is refused", r.returncode != 0, True)
        check("the refusal names that test", "scripts/git-hooks/test_thing.py" in r.stderr.split("BLOCKED")[-1], True)
        check("a test of another hook did not run", "scripts/git-hooks/test_other.py" in r.stderr, False)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print("\npre-push script tests: %d passed, %d failed" % (passed, failed))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
