#!/usr/bin/env python3
"""Run checks.yml's catalogue as parallel shards, and prove the shards ran all of it.

checks.yml keeps every check as a plain `name` + `run` step under the `catalogue`
job, which never runs itself (`if: false`). Every tool that reads the workflow as
text (the pre-push leg, select-checks-cells.sh, test-harness-wired.sh,
ci-fast-gate.sh) still finds each check where it always was. The `shard` matrix
runs this script once per shard; the `checks` job runs `verify`.

  ci-checks.py plan  N                   which checks each of N shards runs, as JSON
  ci-checks.py run   K/N --out FILE      run shard K of N, record each check's result
  ci-checks.py verify DIR                every catalogue check ran exactly once and passed
  ci-checks.py durations DIR             measured seconds per check, for ci-checks-durations.json

The plan is longest-first bin packing over ci-checks-durations.json (a check with
no measurement counts as the median), so it is a pure function of the catalogue
and that file: every shard computes the same plan without talking to the others.
`verify` does not trust that. It recomputes the catalogue and refuses a check that
ran nowhere, ran twice, or failed, and a result file for a shard count other than
the `shard` job's matrix. A shard that died before writing its file reads as
"ran nowhere" for every check it owned, which is the honest reading. After
"Re-run failed jobs", only the failed shards run again, so `verify` keeps the
newest attempt of each shard (GITHUB_RUN_ATTEMPT, recorded in the file).

Each check runs as GitHub runs a `run:` step that names no `shell:`: `bash -e`
(no pipefail; that is only the `shell: bash` form), from the repository root, one after another within a shard, in
catalogue order. A failing check does not stop the shard; the rest still run, so
one red run names every broken check instead of the first.
"""
import json
import os
import statistics
import subprocess
import sys
import tempfile
import time

# CI_CHECKS_ROOT points the script at another tree (scripts/test-ci-checks.py uses it).
ROOT = os.environ.get("CI_CHECKS_ROOT") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORKFLOW = os.path.join(ROOT, ".github", "workflows", "checks.yml")
DURATIONS = os.path.join(ROOT, "scripts", "ci-checks-durations.json")


def workflow(path=WORKFLOW):
    import yaml  # PyYAML ships on GitHub's ubuntu runners; the workflow installs it if not
    with open(path) as f:
        return yaml.safe_load(f)


def shard_count(path=WORKFLOW):
    return len(workflow(path)["jobs"]["shard"]["strategy"]["matrix"]["shard"])


def catalogue(path=WORKFLOW):
    wf = workflow(path)
    steps = wf["jobs"]["catalogue"]["steps"]
    out, seen = [], set()
    for i, s in enumerate(steps):
        extra = set(s) - {"name", "run"}
        if extra or "name" not in s or "run" not in s:
            sys.exit(f"ci-checks: catalogue step {i} must have exactly `name` and `run` (has {sorted(s)}); "
                     "a `uses`, `if` or `env` step cannot run from this script")
        if s["name"] in seen:
            sys.exit(f"ci-checks: two catalogue steps are named {s['name']!r}; names identify checks")
        seen.add(s["name"])
        out.append({"name": s["name"], "run": s["run"]})
    return out


def durations(path=DURATIONS):
    try:
        with open(path) as f:
            return {k: float(v) for k, v in json.load(f).get("seconds", {}).items()}
    except FileNotFoundError:
        return {}


def plan(checks, n, secs):
    known = [secs[c["name"]] for c in checks if c["name"] in secs]
    default = statistics.median(known) if known else 10.0
    cost = {c["name"]: secs.get(c["name"], default) for c in checks}
    order = {c["name"]: i for i, c in enumerate(checks)}
    bins = [[0.0, k, []] for k in range(n)]
    # Longest first, ties by catalogue position, so the plan is deterministic.
    for name in sorted(cost, key=lambda x: (-cost[x], order[x])):
        b = min(bins, key=lambda b: (b[0], b[1]))
        b[0] += cost[name]
        b[2].append(name)
    return [{"shard": b[1] + 1, "estimate_s": round(b[0], 1),
             "checks": sorted(b[2], key=order.get)} for b in bins]


def run_shard(k, n, out):
    checks = catalogue()
    mine = plan(checks, n, durations())[k - 1]["checks"]
    runs = {c["name"]: c["run"] for c in checks}
    results, t0 = [], time.time()
    gh = os.environ.get("GITHUB_ACTIONS") == "true"
    for name in mine:
        with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
            f.write(runs[name])
            script = f.name
        if gh:
            print(f"::group::{name}", flush=True)
        else:
            print(f"== {name}", flush=True)
        start = time.time()
        rc = subprocess.call(["bash", "-e", script], cwd=ROOT)
        took = round(time.time() - start, 1)
        os.unlink(script)
        if gh:
            print("::endgroup::", flush=True)
        verdict = "pass" if rc == 0 else "fail"
        print(f"{verdict} {took}s rc={rc} {name}", flush=True)
        if rc != 0 and gh:
            print(f"::error title=check failed::{name} (exit {rc})", flush=True)
        results.append({"name": name, "rc": rc, "seconds": took})
    doc = {"shard": k, "total": n, "attempt": int(os.environ.get("GITHUB_RUN_ATTEMPT") or 1),
           "seconds": round(time.time() - t0, 1),
           "planned": len(mine), "results": results}
    with open(out, "w") as f:
        json.dump(doc, f, indent=1)
    failed = [r["name"] for r in results if r["rc"] != 0]
    print(f"verdict=checks_shard shard={k}/{n} ran={len(results)} failed={len(failed)} "
          f"seconds={doc['seconds']} measured=true")
    return 1 if failed else 0


def load_results(d):
    """The newest attempt of each shard's result file under d."""
    newest = {}
    for root, _, files in os.walk(d):
        for fn in files:
            if fn.endswith(".json"):
                with open(os.path.join(root, fn)) as f:
                    x = json.load(f)
                k = x.get("shard")
                if k not in newest or x.get("attempt", 1) > newest[k].get("attempt", 1):
                    newest[k] = x
    return list(newest.values())


def verify(d, n=None):
    n = n or shard_count()
    names = [c["name"] for c in catalogue()]
    docs = load_results(d)
    problems = []
    shards = sorted(x["shard"] for x in docs)
    for x in docs:
        if x.get("total") != n:
            problems.append(f"shard {x.get('shard')} ran a {x.get('total')}-shard plan, not {n}")
    missing_shards = sorted(set(range(1, n + 1)) - set(shards))
    if missing_shards:
        problems.append(f"no result from shard(s) {missing_shards}: the job died or never uploaded")
    ran = {}
    for x in docs:
        for r in x["results"]:
            ran.setdefault(r["name"], []).append(r)
    for name in names:
        rs = ran.get(name, [])
        if not rs:
            problems.append(f"ran nowhere: {name}")
        elif len(rs) > 1:
            problems.append(f"ran {len(rs)} times: {name}")
        elif rs[0]["rc"] != 0:
            problems.append(f"failed (exit {rs[0]['rc']}): {name}")
    for name in sorted(set(ran) - set(names)):
        problems.append(f"not in the catalogue: {name}")
    passed = sum(1 for nm in names if len(ran.get(nm, [])) == 1 and ran[nm][0]["rc"] == 0)
    slow = sorted((r for rs in ran.values() for r in rs), key=lambda r: -r["seconds"])[:5]
    for x in sorted(docs, key=lambda x: x["shard"]):
        print(f"shard {x['shard']}/{x['total']}: {len(x['results'])} checks in {x['seconds']}s")
    for r in slow:
        print(f"slowest: {r['seconds']}s {r['name']}")
    for p in problems:
        print(f"::error title=checks::{p}" if os.environ.get("GITHUB_ACTIONS") == "true" else f"FAIL: {p}")
    longest = max((x["seconds"] for x in docs), default=0)
    print(f"verdict=checks_verify catalogue={len(names)} passed={passed} problems={len(problems)} "
          f"shards={len(docs)}/{n} longest_shard_s={longest} measured={'true' if docs else 'false'}")
    return 1 if problems or not docs else 0


def main(argv):
    if len(argv) >= 2 and argv[0] == "plan":
        print(json.dumps(plan(catalogue(), int(argv[1]), durations()), indent=1))
        return 0
    if len(argv) >= 4 and argv[0] == "run" and argv[2] == "--out":
        k, n = (int(x) for x in argv[1].split("/"))
        if not 1 <= k <= n:
            sys.exit(f"ci-checks: shard {k} is outside 1..{n}")
        if n != shard_count():
            sys.exit(f"ci-checks: asked for a {n}-shard plan; the shard job's matrix has {shard_count()}")
        return run_shard(k, n, argv[3])
    if len(argv) == 2 and argv[0] == "verify":
        return verify(argv[1])
    if len(argv) == 2 and argv[0] == "durations":
        secs = {}
        for x in load_results(argv[1]):
            for r in x["results"]:
                secs[r["name"]] = r["seconds"]
        print(json.dumps({"seconds": dict(sorted(secs.items()))}, indent=1))
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
