#!/usr/bin/env python3
"""The owner's kill switch stops a worker's push (governance PR 19).

Runs the real pre-push hook under a throwaway AMUX_HOME:
  1. engaged, inside a worker ($AMUX_SESSION set): refused, exit 1, says why;
  2. engaged, a person pushing ($AMUX_SESSION unset): not stopped by the switch;
  3. a corrupt switch file reads as engaged (fails closed);
  4. lifted: not stopped by the switch;
  5. AMUX_KILL_SWITCH=off: inert;
  6. AMUX_ALLOW_FOREIGN does not skip it.
"Not stopped by the switch" means the verdict line is absent; the hook's other
gates may still have their own say, which this test does not judge.

Run: python3 scripts/git-hooks/test_pre_push_kill_switch.py   (exit 0 = all pass)
"""
import json
import os
import subprocess
import sys
import tempfile

HOOK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pre-push")


def run(home, cwd, **env_over):
    env = dict(os.environ, AMUX_HOME=home, HOME=os.path.dirname(home), AMUX_URL="https://127.0.0.1:9")
    for k in ("AMUX_SESSION", "AMUX_KILL_SWITCH", "AMUX_ALLOW_FOREIGN"):
        env.pop(k, None)
    env.update(env_over)
    r = subprocess.run([sys.executable, HOOK, "origin", "file:///dev/null"], cwd=cwd, input="",
                       capture_output=True, text=True, env=env, timeout=120)
    return r.returncode, r.stderr


def main():
    tmp = tempfile.mkdtemp(prefix="killtest-")
    home = os.path.join(tmp, ".amux")
    repo = os.path.join(tmp, "repo")
    os.makedirs(os.path.join(home, "governance"))
    os.makedirs(repo)
    subprocess.run(["git", "init", "-q", repo], check=True)
    sw = os.path.join(home, "governance", "kill-switch.json")
    fails = []

    def check(name, ok, detail=""):
        print(("ok - " if ok else "FAIL - ") + name)
        if not ok:
            fails.append(name)
            print("   ", detail[-600:])

    with open(sw, "w") as f:
        json.dump({"engaged": True, "reason": "incident 42"}, f)
    code, err = run(home, repo, AMUX_SESSION="w1")
    check("1. engaged: a worker's push is refused and names the reason",
          code == 1 and "kill_switch_refused" in err and "incident 42" in err, err)
    code, err = run(home, repo)
    check("2. engaged: a person's push is not stopped by the switch", "kill_switch_refused" not in err, err)
    with open(sw, "w") as f:
        f.write("{not json")
    code, err = run(home, repo, AMUX_SESSION="w1")
    check("3. a corrupt switch file reads as engaged", code == 1 and "kill_switch_refused" in err, err)
    with open(sw, "w") as f:
        json.dump({"engaged": False}, f)
    code, err = run(home, repo, AMUX_SESSION="w1")
    check("4. lifted: a worker's push is not stopped by the switch", "kill_switch_refused" not in err, err)
    with open(sw, "w") as f:
        json.dump({"engaged": True}, f)
    code, err = run(home, repo, AMUX_SESSION="w1", AMUX_KILL_SWITCH="off")
    check("5. AMUX_KILL_SWITCH=off makes it inert", "kill_switch_refused" not in err, err)
    code, err = run(home, repo, AMUX_SESSION="w1", AMUX_ALLOW_FOREIGN="1")
    check("6. AMUX_ALLOW_FOREIGN does not skip it", code == 1 and "kill_switch_refused" in err, err)
    print("%d/%d passed" % (6 - len(fails), 6))
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
