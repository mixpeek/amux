#!/usr/bin/env python3
"""A workflow that reads a real secret pins every third-party action to a commit sha.

A tag or branch (`@v3`, `@stable`) is a pointer its owner, or whoever takes their account, can
move. A moved tag runs new code inside our job with every secret that job can read, which is how
the 2025 tj-actions and reviewdog compromises spread. So a workflow that reads any secret other
than GITHUB_TOKEN may use a third-party action only at a full 40-character commit sha (the
version as a trailing comment). Local actions (`./...`) and GitHub's own (`actions/*`,
`github/*`) are exempt. Workflows that read no secret are not judged here.

usage: ci-action-pin-guard.py [WORKFLOW_DIR]     exit 1 and name each offender
       ci-action-pin-guard.py --self-test         prove the guard catches what it should
"""
import os
import re
import sys
import tempfile

SECRET = re.compile(r"secrets\.([A-Za-z0-9_]+)")
USES = re.compile(r"^\s*(?:-\s*)?uses:\s*['\"]?([^\s'\"#]+)", re.M)
PINNED = re.compile(r"@[0-9a-f]{40}$")
EXEMPT = ("./", "actions/", "github/", "docker://")


def offenders(text):
    secrets = {s for s in SECRET.findall(text) if s != "GITHUB_TOKEN"}
    if not secrets:
        return []
    return [u for u in USES.findall(text)
            if not u.startswith(EXEMPT) and not PINNED.search(u)]


def scan(root):
    bad = []
    for name in sorted(os.listdir(root)):
        if name.endswith((".yml", ".yaml")):
            with open(os.path.join(root, name), encoding="utf-8") as fh:
                bad += [f"{name}: {u}" for u in offenders(fh.read())]
    return bad


def self_test():
    cases = {
        "tag with a secret": ("env:\n  K: ${{ secrets.DEPLOY_KEY }}\nsteps:\n"
                              "  - uses: docker/login-action@v3\n", 1),
        "branch with a secret": ("steps:\n  - uses: ruby/setup-ruby@v1\n"
                                 "    with: {k: '${{ secrets.P12 }}'}\n", 1),
        "sha with a secret": ("steps:\n  - uses: docker/login-action@"
                              + "c" * 40 + " # v3.7.0\n    env: {K: '${{ secrets.X }}'}\n", 0),
        "GitHub's own and local actions": ("steps:\n  - uses: actions/checkout@v4\n"
                                           "  - uses: ./.github/actions/x\n"
                                           "    env: {K: '${{ secrets.X }}'}\n", 0),
        "no secret beyond GITHUB_TOKEN": ("steps:\n  - uses: dtolnay/rust-toolchain@stable\n"
                                          "    env: {T: '${{ secrets.GITHUB_TOKEN }}'}\n", 0),
    }
    failed = 0
    with tempfile.TemporaryDirectory() as d:
        for label, (text, want) in cases.items():
            p = os.path.join(d, "w.yml")
            with open(p, "w") as fh:
                fh.write(text)
            got = len(scan(d))
            ok = got == want
            failed += not ok
            print(("ok   " if ok else "FAIL ") + f"{label}: {got} offender(s), want {want}")
    return 1 if failed else 0


def main(argv):
    if argv[1:2] == ["--self-test"]:
        return self_test()
    root = argv[1] if len(argv) > 1 else ".github/workflows"
    bad = scan(root)
    for b in bad:
        print(f"ci-action-pin-guard: {b} reads a secret and is not pinned to a commit sha",
              file=sys.stderr)
    print(f"ci-action-pin-guard: {len(bad)} unpinned third-party action(s) in secret-bearing "
          f"workflows under {root}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
