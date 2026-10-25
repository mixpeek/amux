#!/usr/bin/env python3
"""Tests for post-push-verify.py (mxp-gs12 review, 2026-10-18).

The hook raised 59 false "POST-PUSH SHA VERIFY FAILED" alarms because it
compared the checkout's HEAD with origin/main after pushes (amux land, grafts,
`git push origin <sha>:main`) that never move HEAD. Each case below runs the
real hook against a bare origin and a clone whose HEAD is deliberately NOT
what was pushed, under a throwaway HOME, with `gh` stubbed out. PPV_HOOK=<old copy>
runs the same cases against the pre-fix hook as a control (it fails all six).

Run: python3 scripts/git-hooks/test_post_push_verify.py   (exit 0 = all pass)
"""
import json
import os
import subprocess
import sys
import tempfile

# PPV_HOOK runs the cases against another copy (the pre-fix one, as a control).
HOOK = os.environ.get("PPV_HOOK") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "post-push-verify.py")


def sh(cwd, *a):
    r = subprocess.run(a, cwd=cwd, capture_output=True, text=True)
    return r.stdout.strip()


def main():
    tmp = tempfile.mkdtemp(prefix="ppvtest-")
    home = os.path.join(tmp, "home")
    stub = os.path.join(tmp, "bin")
    os.makedirs(home)
    os.makedirs(stub)
    with open(os.path.join(stub, "gh"), "w") as f:
        f.write("#!/bin/sh\nexit 1\n")
    os.chmod(os.path.join(stub, "gh"), 0o755)
    env = dict(os.environ, HOME=home, PATH=stub + os.pathsep + os.environ.get("PATH", ""))
    env.pop("AMUX_SESSION", None)
    origin = os.path.join(tmp, "origin.git")
    work = os.path.join(tmp, "work")
    sh(tmp, "git", "init", "-q", "--bare", "--initial-branch=main", origin)
    sh(tmp, "git", "clone", "-q", origin, work)
    for k, v in (("user.email", "t@t"), ("user.name", "t")):
        sh(work, "git", "config", k, v)

    def commit(name):
        with open(os.path.join(work, name), "w") as f:
            f.write(name)
        sh(work, "git", "add", name)
        sh(work, "git", "commit", "-qm", name)
        return sh(work, "git", "rev-parse", "HEAD")

    base = commit("a")
    sh(work, "git", "push", "-q", "origin", "HEAD:main")
    pushed = commit("b")
    out = subprocess.run(["git", "push", "origin", pushed + ":main"], cwd=work,
                         capture_output=True, text=True)
    push_out = out.stdout + out.stderr
    # HEAD now a local-only commit beside what was pushed: the shape amux land
    # leaves (it pushes a REPLAYED copy, so the checkout HEAD is never on main).
    sh(work, "git", "reset", "-q", "--hard", base)
    commit("local-only")

    def run(cmd, response):
        p = subprocess.run([sys.executable, HOOK], cwd=work, env=env, capture_output=True, text=True,
                           input=json.dumps({"tool_name": "Bash", "cwd": work,
                                             "tool_input": {"command": cmd},
                                             "tool_response": response}), timeout=60)
        return p.stdout + p.stderr

    failures, n = [], 0

    # 1. The false-alarm shape: pushed by sha, HEAD not on main. Must confirm.
    n += 1
    o = run(f"git push origin {pushed}:main", {"stdout": "", "stderr": push_out})
    if "FAILED" in o or "confirmed on origin/main" not in o:
        failures.append("sha push with HEAD off main was not confirmed: %r" % o[:300])

    # 2. Same push, output not captured: the explicit refspec names the commit.
    n += 1
    o = run(f"git push origin {pushed}:main > /dev/null 2>&1", {"stdout": "", "stderr": ""})
    if "FAILED" in o or "sha from the push refspec" not in o:
        failures.append("refspec fallback did not confirm: %r" % o[:300])

    # 3. A rejected push is reported as rejected, never as overtaken.
    n += 1
    rej = " ! [rejected]        deadbeef -> main (fetch first)\nerror: failed to push some refs"
    o = run("git push origin HEAD:main", {"stdout": "", "stderr": rej})
    if "REJECTED" not in o or "VERIFY FAILED" in o:
        failures.append("rejected push misreported: %r" % o[:300])

    # 4. Nothing to go on: say NOT VERIFIED, do not guess HEAD.
    n += 1
    o = run('git push origin "$TIP":main >/dev/null 2>&1', {"stdout": "", "stderr": ""})
    if "NOT VERIFIED" not in o or "VERIFY FAILED" in o:
        failures.append("unresolvable push did not say NOT VERIFIED: %r" % o[:300])

    # 5. The alarm still fires when the pushed commit really is not on main:
    #    origin is rewound past it after the push. The positive control.
    n += 1
    sh(work, "git", "push", "-q", "-f", "origin", base + ":main")
    o = run(f"git push origin {pushed}:main", {"stdout": "", "stderr": push_out})
    if "VERIFY FAILED" not in o or "pull --rebase origin main" in o.replace("Do not `git pull --rebase`", ""):
        failures.append("a pushed commit missing from origin did not alarm (or alarmed with unsafe advice): %r" % o[:400])

    # 6. Every run is logged with a verdict.
    n += 1
    try:
        recs = [json.loads(l) for l in open(os.path.join(home, ".amux", "logs", "post-push-verify.log"))]
    except Exception:
        recs = []
    got = {r.get("verdict") for r in recs}
    if not {"confirmed", "rejected", "unmeasured", "not_on_origin"} <= got:
        failures.append("log verdicts missing: got %s" % sorted(got))

    subprocess.run(["rm", "-rf", tmp])
    for f in failures:
        print("FAIL", f)
    print("%d case(s), %s" % (n, "ALL PASS" if not failures else "%d FAILED" % len(failures)))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
