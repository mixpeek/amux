#!/usr/bin/env python3
"""Report (and optionally prune) remote branches against trunk-based rules.

Owner rule, Ethan 2026-10-27 11:46 ("is it bad to have all these branches?"):
- a branch lives under a day and lands on main behind a default-off flag;
- a branch is deleted once it is merged;
- a branch older than --max-age-h with no merge is REPORTED to its owner, who
  merges it or closes it with a reason. Unmerged branches are never deleted here.

Usage:
  branch-hygiene.py --repo ~/Dev/mixpeek [--remote origin] [--main main]
                    [--max-age-h 48] [--keep REGEX] [--owner PREFIX=LANE ...]
                    [--delete-merged] [--json]

Merged = the branch tip is an ancestor of <remote>/<main>. Deletion is
loss-free for exactly that set: every commit is already on main. --keep names
branches that stay even when merged (long-lived env branches, release images).

Every run prints `measured` and `n_considered`, so an empty report can be told
apart from a fetch that never ran.
"""
import argparse
import json
import re
import subprocess
import sys
import time


def git(repo, *args, check=True):
    r = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True)
    if check and r.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {r.stderr.strip()}")
    return r


def owner_for(name, owners):
    for prefix, lane in owners:
        if name.startswith(prefix):
            return lane
    return "unowned"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--remote", default="origin")
    ap.add_argument("--main", default="main")
    ap.add_argument("--max-age-h", type=float, default=48)
    ap.add_argument("--keep", default=r"^(staging|production|release/.*)$",
                    help="regex of branch names never deleted, even when merged")
    ap.add_argument("--owner", action="append", default=[],
                    help="PREFIX=LANE, first match wins (repeatable)")
    ap.add_argument("--delete-merged", action="store_true")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()

    owners = [tuple(o.split("=", 1)) for o in a.owner]
    keep = re.compile(a.keep)
    out = {"measured": False, "n_considered": 0, "repo": a.repo,
           "max_age_h": a.max_age_h, "merged": [], "stale": [], "fresh": 0,
           "kept": [], "deleted": [], "delete_failed": []}
    try:
        git(a.repo, "fetch", "--prune", "--quiet", a.remote)
    except RuntimeError as e:
        out["why_unmeasured"] = str(e)
        print(json.dumps(out) if a.json else f"UNMEASURED: {e}")
        return 2

    base = f"{a.remote}/{a.main}"
    refs = git(a.repo, "for-each-ref", f"refs/remotes/{a.remote}/",
               "--format=%(refname:short)|%(committerdate:unix)").stdout.split()
    now = time.time()
    t0 = time.monotonic()
    for line in refs:
        ref, ts = line.rsplit("|", 1)
        name = ref[len(a.remote) + 1:]
        if name in ("HEAD", a.main) or not name:
            continue
        out["n_considered"] += 1
        age_h = round((now - int(ts)) / 3600, 1)
        merged = git(a.repo, "merge-base", "--is-ancestor", ref, base,
                     check=False).returncode == 0
        row = {"branch": name, "age_h": age_h, "owner": owner_for(name, owners)}
        if merged:
            (out["kept"] if keep.search(name) else out["merged"]).append(row)
        elif age_h > a.max_age_h:
            out["stale"].append(row)
        else:
            out["fresh"] += 1
    out["measured"] = True
    out["scan_s"] = round(time.monotonic() - t0, 2)

    if a.delete_merged:
        for row in out["merged"]:
            r = git(a.repo, "push", "--quiet", a.remote, "--delete", row["branch"],
                    check=False)
            (out["deleted"] if r.returncode == 0 else out["delete_failed"]).append(
                row["branch"] if r.returncode == 0
                else {"branch": row["branch"], "error": r.stderr.strip()[-300:]})

    if a.json:
        print(json.dumps(out, indent=1))
    else:
        by_owner = {}
        for row in out["stale"]:
            by_owner.setdefault(row["owner"], []).append(row)
        print(f"measured=True n_considered={out['n_considered']} "
              f"merged={len(out['merged'])} kept={len(out['kept'])} "
              f"stale>{a.max_age_h:g}h={len(out['stale'])} fresh={out['fresh']} "
              f"deleted={len(out['deleted'])} delete_failed={len(out['delete_failed'])}")
        for owner in sorted(by_owner):
            rows = sorted(by_owner[owner], key=lambda r: -r["age_h"])
            print(f"\n{owner}: {len(rows)} unmerged older than {a.max_age_h:g}h "
                  "(merge it, or close it with a reason)")
            for r in rows:
                print(f"  {r['age_h']:>7}h  {r['branch']}")
        for f in out["delete_failed"]:
            print(f"WARN delete_failed branch={f['branch']} error={f['error']}",
                  file=sys.stderr)
    return 1 if out["delete_failed"] else 0


if __name__ == "__main__":
    sys.exit(main())
