#!/usr/bin/env python3
"""scripts/ci-checks.py: the shards run all of checks.yml's catalogue, and verify can fail.

Cells:
  1. every plan of the REAL catalogue, 1 to 30 shards, owns each check exactly once
  2. a shard keeps running after a failing check, records it, and exits 1
  3. verify passes a complete, green set of results
  4. verify refuses a missing shard, a duplicate, a failure, a foreign name and a wrong total
  5. after a re-run, the newest attempt of a shard is the one judged
  6. the catalogue refuses a step it could not run faithfully (a `uses` or `if`)
Exit 0 = all pass.
"""
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "ci-checks.py")
fails = 0


def check(label, got, want):
    global fails
    ok = got == want
    print(("ok   " if ok else "FAIL ") + label + ("" if ok else f": got {got!r}, want {want!r}"))
    fails += 0 if ok else 1


def cc(args, root=None):
    env = dict(os.environ)
    env.pop("GITHUB_ACTIONS", None)
    env.pop("GITHUB_RUN_ATTEMPT", None)
    if root:
        env["CI_CHECKS_ROOT"] = root
    return subprocess.run([sys.executable, SCRIPT] + args, env=env, capture_output=True, text=True)


def fake_root(steps, shards):
    root = tempfile.mkdtemp()
    os.makedirs(os.path.join(root, ".github", "workflows"))
    os.makedirs(os.path.join(root, "scripts"))
    import yaml
    wf = {"jobs": {"shard": {"strategy": {"matrix": {"shard": list(range(1, shards + 1))}}},
                   "catalogue": {"if": False, "steps": steps}}}
    with open(os.path.join(root, ".github", "workflows", "checks.yml"), "w") as f:
        yaml.safe_dump(wf, f)
    return root


# 1. the real catalogue
names = None
for n in range(1, 31):
    r = cc(["plan", str(n)])
    plan = json.loads(r.stdout)
    owned = [c for b in plan for c in b["checks"]]
    if names is None:
        names = owned
    if len(owned) != len(set(owned)) or set(owned) != set(names) or len(plan) != n:
        check(f"plan of {n} shards owns each check once", False, True)
        break
else:
    check(f"plans of 1..30 shards each own all {len(names)} catalogue checks exactly once", True, True)

# 2. a shard runs on past a failure
steps = [{"name": "a passes", "run": "true"},
         {"name": "b fails", "run": "echo boom; exit 3"},
         {"name": "c passes", "run": "echo fine"}]
root = fake_root(steps, 1)
out = os.path.join(root, "r1.json")
r = cc(["run", "1/1", "--out", out], root)
check("a shard with a failing check exits 1", r.returncode, 1)
doc = json.load(open(out))
check("every check in the shard still ran", [x["name"] for x in doc["results"]], ["a passes", "b fails", "c passes"])
check("the failure is recorded with its exit code", [x["rc"] for x in doc["results"]], [0, 3, 0])
r = cc(["run", "1/2", "--out", out], root)
check("a shard count the matrix does not have is refused", r.returncode != 0, True)

# 3-5. verify
steps = [{"name": f"c{i}", "run": "true"} for i in range(6)]
root = fake_root(steps, 3)


def results(dirname, docs):
    d = os.path.join(root, dirname)
    os.makedirs(d, exist_ok=True)
    for i, x in enumerate(docs):
        sub = os.path.join(d, f"checks-shard-{x['shard']}-attempt-{x.get('attempt', 1)}")
        os.makedirs(sub, exist_ok=True)
        json.dump(x, open(os.path.join(sub, f"s{i}.json"), "w"))
    return d


def shard(k, names, rc=0, total=3, attempt=1):
    return {"shard": k, "total": total, "attempt": attempt, "seconds": 1,
            "results": [{"name": n, "rc": rc, "seconds": 1} for n in names]}


good = [shard(1, ["c0", "c1"]), shard(2, ["c2", "c3"]), shard(3, ["c4", "c5"])]
check("verify passes a complete green set", cc(["verify", results("good", good)], root).returncode, 0)
bad_cases = {
    "a missing shard": good[:2],
    "a duplicate check": [good[0], shard(2, ["c2", "c3", "c0"]), good[2]],
    "a failed check": [good[0], good[1], shard(3, ["c4", "c5"], rc=1)],
    "a name not in the catalogue": [good[0], good[1], shard(3, ["c4", "c5", "zz"])],
    "a result from another shard count": [good[0], good[1], shard(3, ["c4", "c5"], total=4)],
}
for label, docs in bad_cases.items():
    r = cc(["verify", results(label.replace(" ", "_"), docs)], root)
    check(f"verify refuses {label}", r.returncode, 1)
rerun = good[:2] + [shard(3, ["c4", "c5"], rc=1, attempt=1), shard(3, ["c4", "c5"], attempt=2)]
check("a green re-run of a red shard is the result judged", cc(["verify", results("rerun", rerun)], root).returncode, 0)
rerun_red = good[:2] + [shard(3, ["c4", "c5"], attempt=1), shard(3, ["c4", "c5"], rc=1, attempt=2)]
check("a red re-run of a green shard is the result judged", cc(["verify", results("rerun_red", rerun_red)], root).returncode, 1)
r = cc(["verify", os.path.join(root, "nothing-here")], root)
check("verify with no results at all is red and says unmeasured", (r.returncode, "measured=false" in r.stdout), (1, True))

# 6. a step the runner cannot run faithfully
root = fake_root([{"name": "x", "run": "true", "if": "always()"}], 1)
r = cc(["plan", "1"], root)
check("a catalogue step with `if` is refused", r.returncode != 0 and "exactly `name` and `run`" in r.stderr, True)

print(f"verdict=test_ci_checks failures={fails} measured=true")
sys.exit(1 if fails else 0)
