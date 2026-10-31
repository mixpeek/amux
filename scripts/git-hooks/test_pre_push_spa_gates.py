#!/usr/bin/env python3
"""The pre-push SPA gates and AF-561 harness guard (velocity audit 2026-10-26).

pre-push runs the dashboard node:test suites that rust.yml's spa-gates job
runs, on the pushed commit's bytes, when the push touches what they test.
This drives the real hook against a scratch repo whose suites are fixtures.

THE CONTROL COMES FIRST. The fixture's readme suite fails whenever README.md
says "red", and the first push makes it say "red" while touching no dashboard
file. If the range selection did not skip the gates, the suite would fail on
the pushed tip and pass on the remote, and the push would be refused. So an
allowed push there can only mean the gates did not run.

Run: python3 scripts/git-hooks/test_pre_push_spa_gates.py   (exit 0 = pass)
"""
import os
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HOOK = os.path.join(ROOT, "scripts", "git-hooks", "pre-push")

APP_SUITE = """import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
test('app.js says GOOD', () => {
  assert.match(fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8'), /GOOD/);
});
"""
README_SUITE = """import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
test('the readme is not red', () => {
  assert.doesNotMatch(fs.readFileSync('README.md', 'utf8'), /red/);
});
"""

SPA_LINT = """#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if grep -q LINTBAD crates/amux-dashboard/static/app.js; then
  echo "docs/ux/button-feedback-audit.md is stale: run node scripts/button-audit.mjs" >&2; exit 1
fi
"""

HARNESS_GUARD = """#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
bad=$(grep -L 'set -e' scripts/test-*.sh || true)
[ -z "$bad" ] && { echo "  ok    all guarded"; exit 0; }
echo "  FAIL  every verdict-printing harness carries set -e"; echo "$bad"; exit 1
"""

passed = failed = 0


def check(label, got, want):
    global passed, failed
    if got == want:
        print("  ok    %-58s %s" % (label, got))
        passed += 1
    else:
        print("  FAIL  %-58s got %s want %s" % (label, got, want))
        failed += 1


def git(work, *args, env=None):
    e = {k: v for k, v in os.environ.items()
         if k not in ("AMUX_SESSION", "AMUX_SKIP_SPA_GATES", "AMUX_SPA_GATE_NODE_MODULES")}
    e.update({"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
              "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t", "HOME": work})
    if env:
        e.update(env)
    return subprocess.run(["git", "-C", work] + list(args),
                          capture_output=True, text=True, timeout=300, env=e)


def write(work, rel, text):
    p = os.path.join(work, rel)
    os.makedirs(os.path.dirname(p), exist_ok=True)
    open(p, "w").write(text)


def commit(work, msg, *paths):
    git(work, "add", *paths, "--")
    git(work, "commit", "-qm", msg)


def main():
    tmp = tempfile.mkdtemp(prefix="prepush-spa-")
    try:
        origin = os.path.join(tmp, "origin.git")
        work = os.path.join(tmp, "work")
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", origin], check=True)
        subprocess.run(["git", "init", "-q", "-b", "main", work], check=True)
        git(work, "remote", "add", "origin", origin)
        os.makedirs(os.path.join(work, "node_modules"))
        write(work, ".gitignore", "node_modules\n")
        write(work, "README.md", "fine\n")
        write(work, "crates/amux-dashboard/static/app.js", "// GOOD\n")
        write(work, "tests/dashboard-outage-recovery.mjs", APP_SUITE)
        write(work, "e2e/readme.test.mjs", README_SUITE)
        commit(work, "fixture", ".")
        shutil.copy(HOOK, os.path.join(work, ".git", "hooks", "pre-push"))
        os.chmod(os.path.join(work, ".git", "hooks", "pre-push"), 0o755)
        check("seed push (both suites green) is allowed",
              git(work, "push", "-q", "origin", "main").returncode, 0)

        # THE CONTROL: touches no dashboard file and turns the readme suite red.
        write(work, "README.md", "red\n")
        commit(work, "unrelated: touches no dashboard file", "README.md")
        r = git(work, "push", "origin", "main")
        check("push touching no dashboard file is allowed", r.returncode, 0)
        check("and the gates did not run", "SPA gates on" in r.stderr, False)

        # THE REFUSAL, on the pushed commit's bytes: the worktree is put back
        # to GOOD before pushing, so a gate reading the worktree would pass.
        write(work, "crates/amux-dashboard/static/app.js", "// BAD\n")
        commit(work, "dashboard: breaks app.js", "crates")
        write(work, "crates/amux-dashboard/static/app.js", "// GOOD\n")
        r = git(work, "push", "origin", "main")
        check("push breaking a dashboard suite is refused", r.returncode != 0, True)
        if r.returncode == 0:
            # Say WHY, where CI shows it: on 2026-10-31 this failed in CI only
            # (09:38Z on) with every later check red and nothing to read.
            print("  hook stderr on the push that should have been refused:\n    "
                  + "\n    ".join(r.stderr.strip().splitlines()[-12:]))
        check("refusal names the failing test", "app.js says GOOD" in r.stderr, True)
        check("a red already on the remote is not blamed on this push",
              "the readme is not red" in r.stderr.split("BLOCKED")[-1], False)
        check("it counts that red as already failing",
              "1 failing from this push, 1 already failing on the remote" in r.stderr, True)
        check("refusal names the override", "AMUX_SKIP_SPA_GATES" in r.stderr, True)
        check("verdict field", "verdict=spa_gates_fail" in r.stderr, True)

        # THE ESCAPE (ethos rule 3), then a fix that only inherits a red.
        check("override lands it anyway",
              git(work, "push", "origin", "main",
                  env={"AMUX_SKIP_SPA_GATES": "1"}).returncode, 0)
        git(work, "checkout", "--", ".")
        write(work, "crates/amux-dashboard/static/app.js", "// GOOD again\n")
        commit(work, "dashboard: fixes app.js", "crates")
        r = git(work, "push", "origin", "main")
        check("a push whose only red is inherited is allowed", r.returncode, 0)
        check("and says the suites ran", "verdict=spa_gates_pass" in r.stderr, True)

        # No node_modules anywhere: say so and let CI decide.
        shutil.rmtree(os.path.join(work, "node_modules"))
        write(work, "crates/amux-dashboard/static/app.js", "// GOOD, third\n")
        commit(work, "dashboard: another edit", "crates")
        r = git(work, "push", "origin", "main")
        check("with no node_modules the push is allowed", r.returncode, 0)
        check("and says the gates were not measured", "verdict=spa_gates_unmeasured" in r.stderr, True)

        # AF-561's harness guard, the same way. The fixture's guard fails while
        # any scripts/test-*.sh lacks `set -e`.
        write(work, "scripts/test-harness-guard.sh", HARNESS_GUARD)
        commit(work, "fixture: harness guard", "scripts")
        check("a push adding the guard itself, all guarded, is allowed",
              git(work, "push", "origin", "main").returncode, 0)
        write(work, "scripts/test-new.sh", "#!/bin/bash\necho PASS\n")
        commit(work, "adds an unguarded harness", "scripts")
        write(work, "scripts/test-new.sh", "#!/bin/bash\nset -euo pipefail\necho PASS\n")
        r = git(work, "push", "origin", "main")
        check("a push adding an unguarded harness is refused (committed bytes)", r.returncode != 0, True)
        check("refusal names the script", "scripts/test-new.sh" in r.stderr, True)
        check("refusal names the override", "AMUX_SKIP_HARNESS_GUARD" in r.stderr, True)
        commit(work, "guards it", "scripts")
        check("guarding it lets the push through",
              git(work, "push", "origin", "main").returncode, 0)

        # spa-gates' first step, scripts/spa-lint.sh, which e0b64b05f failed
        # in CI after passing this gate (velocity audit 2026-10-27).
        os.makedirs(os.path.join(work, "node_modules"))
        write(work, "scripts/spa-lint.sh", SPA_LINT)
        commit(work, "adds the lint", "scripts")
        r = git(work, "push", "origin", "main")
        check("a push adding a passing spa-lint is allowed", r.returncode, 0)
        check("  and counts the lint as a test", "SPA gates on" in r.stderr and "verdict=spa_gates_pass" in r.stderr, True)
        write(work, "crates/amux-dashboard/static/app.js", "// GOOD LINTBAD\n")
        commit(work, "dashboard: trips the lint", "crates")
        r = git(work, "push", "origin", "main")
        check("a push failing spa-lint is refused", r.returncode != 0, True)
        check("  and the refusal names spa-lint and its reason",
              "scripts/spa-lint.sh: docs/ux/button-feedback-audit.md is stale" in r.stderr, True)
        write(work, "crates/amux-dashboard/static/app.js", "// GOOD lint fixed\n")
        commit(work, "dashboard: fixes the lint", "crates")
        check("fixing it lets the push through",
              git(work, "push", "origin", "main").returncode, 0)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print("\npre-push SPA gates: %d passed, %d failed" % (passed, failed))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
