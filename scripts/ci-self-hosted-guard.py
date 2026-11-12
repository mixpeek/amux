#!/usr/bin/env python3
"""No job that can run on a self-hosted runner may be reachable from a pull request.

The repository is public. A pull request from a fork runs the fork's workflow code, so a job on a
self-hosted runner that a pull request can trigger runs a stranger's code on our machine
(docs/design/cicd-two-minutes.md, step Z9). Self-hosted jobs are allowed only in workflows
triggered by push, schedule or workflow_dispatch alone, which take write access to cause, and a
workflow a pull request triggers may call a reusable workflow only when it is local and hosted.

usage: ci-self-hosted-guard.py [WORKFLOW_DIR]     exit 1 and name each offender
       ci-self-hosted-guard.py --self-test         prove the guard catches what it should
"""
import os
import sys
import tempfile

import yaml

UNSAFE = {"pull_request", "pull_request_target", "workflow_run", "issue_comment",
          "pull_request_review", "pull_request_review_comment", "discussion_comment"}


def triggers(doc):
    # YAML 1.1 reads a bare `on:` key as the boolean True.
    on = doc.get("on", doc.get(True))
    if isinstance(on, str):
        return {on}
    if isinstance(on, list):
        return set(on)
    if isinstance(on, dict):
        return set(on)
    return set()


def self_hosted(runs_on):
    """True when `runs-on` can select a self-hosted runner, or cannot be read statically."""
    if runs_on is None:
        return False
    if isinstance(runs_on, str):
        if "${{" in runs_on:
            return True                    # an expression could pick anything
        return runs_on == "self-hosted" or not runs_on.startswith(("ubuntu-", "macos-", "windows-"))
    if isinstance(runs_on, list):
        return any(self_hosted(r) for r in runs_on)
    if isinstance(runs_on, dict):
        return True                        # group/labels form is for self-hosted runners
    return True


# A self-hosted job may live only in a workflow these alone trigger: each takes write access.
SAFE = {"push", "schedule", "workflow_dispatch"}


def offenders(workflow_dir):
    docs = {}
    for name in sorted(os.listdir(workflow_dir)):
        if name.endswith((".yml", ".yaml")):
            with open(os.path.join(workflow_dir, name)) as f:
                docs[name] = yaml.safe_load(f) or {}

    def has_self_hosted(doc):
        return any(isinstance(spec, dict) and self_hosted(spec.get("runs-on"))
                   for spec in (doc.get("jobs") or {}).values())

    out = []
    for name, doc in docs.items():
        trig = triggers(doc)
        other = trig - SAFE
        for job, spec in (doc.get("jobs") or {}).items():
            if not isinstance(spec, dict):
                continue
            if other and self_hosted(spec.get("runs-on")):
                # Anything beyond push/schedule/dispatch, including workflow_call: a reusable
                # workflow runs with whatever its caller was triggered by (review F1/F2).
                out.append(f"{name}: job '{job}' runs-on {spec.get('runs-on')!r} and the workflow is "
                           f"triggered by {', '.join(sorted(other))}")
            uses = spec.get("uses")
            if uses and trig & UNSAFE:
                local = uses[2:] if uses.startswith("./") else None
                callee = docs.get(os.path.basename(local)) if local else None
                if callee is None:
                    out.append(f"{name}: job '{job}' calls {uses} from a workflow triggered by "
                               f"{', '.join(sorted(trig & UNSAFE))}, and its runners cannot be read here")
                elif has_self_hosted(callee):
                    out.append(f"{name}: job '{job}' calls {uses}, which runs on a self-hosted runner, "
                               f"from a workflow triggered by {', '.join(sorted(trig & UNSAFE))}")
    return out


def self_test():
    cases = {
        "safe.yml": ("on:\n  push:\n    branches: [main]\njobs:\n  a:\n    runs-on: [self-hosted, amux-main]\n", 0),
        "hosted-pr.yml": ("on: [pull_request]\njobs:\n  a:\n    runs-on: ubuntu-latest\n", 0),
        "unsafe-list.yml": ("on: [push, pull_request]\njobs:\n  a:\n    runs-on: [self-hosted, amux-main]\n", 1),
        "unsafe-label.yml": ("on:\n  pull_request_target:\njobs:\n  a:\n    runs-on: amux-main\n", 1),
        "unsafe-expr.yml": ("on: pull_request\njobs:\n  a:\n    runs-on: ${{ matrix.r }}\n", 1),
        # A reusable workflow: flagged on its own trigger, and on the pull request that calls it.
        "callee.yml": ("on: workflow_call\njobs:\n  r:\n    runs-on: [self-hosted, amux-main]\n", 1),
        "caller.yml": ("on: pull_request\njobs:\n  b:\n    uses: ./.github/workflows/callee.yml\n", 1),
        "remote-caller.yml": ("on: pull_request_target\njobs:\n  b:\n    uses: other/repo/.github/workflows/x.yml@v1\n", 1),
        "hosted-callee.yml": ("on: workflow_call\njobs:\n  r:\n    runs-on: ubuntu-latest\n", 0),
        "safe-caller.yml": ("on: pull_request\njobs:\n  b:\n    uses: ./.github/workflows/hosted-callee.yml\n", 0),
    }
    with tempfile.TemporaryDirectory() as d:
        for name, (text, _) in cases.items():
            with open(os.path.join(d, name), "w") as f:
                f.write(text)
        got = offenders(d)
    fail = 0
    for name, (_, want) in cases.items():
        n = sum(1 for o in got if o.startswith(name + ":"))
        if n != want:
            print(f"SELF-TEST FAILED: {name}: {n} offender(s), expected {want}")
            fail = 1
    if not fail:
        print(f"self-test: {len(cases)} cases, each judged as expected")
    return fail


def main(argv):
    if argv[:1] == ["--self-test"]:
        return self_test()
    workflow_dir = argv[0] if argv else os.path.join(os.path.dirname(__file__), "..", ".github", "workflows")
    bad = offenders(workflow_dir)
    for o in bad:
        print("SELF-HOSTED JOB REACHABLE FROM A PULL REQUEST: " + o)
    n = len([x for x in os.listdir(workflow_dir) if x.endswith((".yml", ".yaml"))])
    print(f"self-hosted guard: {n} workflow(s) checked, {len(bad)} offender(s)")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
