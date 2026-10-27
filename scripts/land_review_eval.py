#!/usr/bin/env python3
"""Score scripts/land_review.py against amux's own incidents.

The Claude Code team scores its reviewer daily against a collection of the
PRs that caused incidents (with their fix-forwards and reverts), and measures
noise on PRs known to be clean ("How the Claude Code team does code review",
2026-10-26). Any change to the reviewer's prompts, tools or model is rescored
against the whole collection. This is that loop for amux.

    scripts/land_review_eval.py --build [--days 60]    # refresh the collection from git history
    scripts/land_review_eval.py --score [--n 5] [--clean 5] [--tier standard]

An incident is a commit that a later commit reverted ("This reverts commit
<sha>"), named as the cause ("broke in <sha>", "regressed by <sha>",
"introduced in <sha>"), or cited from a `fix` commit that changes the same
files and says what went wrong (red, broke, missed, failing, wrong). The reviewer HITS an incident when one of its
important findings names a file the fix changed. A clean commit is one from
the same window that nothing reverted or named, and whose files no commit
touched again within 7 days; the reviewer is NOISY on it when it raises any
important finding.

Collection: scripts/fixtures/review-incidents.jsonl (committed, so a rescore
compares like with like). Results: ~/.amux/land-gate/evals/<time>.json and a
`verdict=review_eval` line in ~/.amux/logs/land-gate.log carrying recall,
noise and both denominators.
"""
import argparse, json, os, random, re, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
COLLECTION = os.path.join(HERE, "fixtures", "review-incidents.jsonl")
CAUSE = re.compile(r"\b(?:broke|broken|regress\w*|introduced|caused)\b[^.\n]{0,40}?\b(?:in|by|at)\s+`?([0-9a-f]{9,40})\b", re.I)
# A `fix` commit that cites an earlier commit sharing its files is a
# fix-forward of it, the common shape here ("the entry 9ef5d12fe missed").
FIX_SUBJECT = re.compile(r"^(?:fix|revert)\b", re.I)
SHA = re.compile(r"\b([0-9a-f]{9,40})\b")
# ...and that says something went wrong. A fix that only cites a commit is as
# often a follow-up feature (8db48a46119c's "audit origin/main") as a repair.
BROKE = re.compile(r"\b(red|green main|broke|broken|missed|forgot|regress\w*|fail(?:s|ed|ing)?|wrong|left out|never (?:registered|wired))\b", re.I)
REVERT = re.compile(r"This reverts commit ([0-9a-f]{40})")


def git(*a, check=True):
    r = subprocess.run(["git", "-C", REPO, *a], capture_output=True, text=True, timeout=300)
    if check and r.returncode != 0:
        raise RuntimeError(r.stderr.strip()[:300])
    return r.stdout


def files_of(sha):
    return [f for f in git("show", "--name-only", "--format=", sha).splitlines() if f]


def build(days):
    """One pass over the window's log and one batched object lookup: a commit
    body can cite dozens of hex tokens, and resolving each with its own git
    call took over ten minutes for 60 days."""
    log = git("log", "origin/main", "--since=%d.days" % days, "--no-merges", "--format=%x01%H%x00%B%x02", "--name-only")
    commits = []
    for entry in log.split("\x01")[1:]:
        head, _, names = entry.partition("\x02")
        sha, _, body = head.partition("\x00")
        commits.append((sha.strip(), body, [f for f in names.split("\n") if f.strip()]))
    files = {c[0]: set(c[2]) for c in commits}
    order = {c[0]: i for i, c in enumerate(commits)}  # newest first
    cited = []
    for sha, body, fs in commits:
        found = [(m, "revert") for m in REVERT.findall(body)] + [(m, "fix_forward") for m in CAUSE.findall(body)]
        if FIX_SUBJECT.match(body.strip()) and BROKE.search(body):
            found += [(m, "fix_forward") for m in SHA.findall(body) if not m.isdigit()]
        cited += [(sha, body, fs, tok, kind) for tok, kind in found]
    toks = sorted({c[3] for c in cited})
    r = subprocess.run(["git", "-C", REPO, "cat-file", "--batch-check=%(objectname) %(objecttype)"],
                       input="".join(t + "^{commit}\n" for t in toks), capture_output=True, text=True, timeout=300)
    resolved = {}
    for tok, line in zip(toks, r.stdout.splitlines()):
        parts = line.split()
        if len(parts) == 2 and parts[1] == "commit":
            resolved[tok] = parts[0]
    seen, out = set(), []
    for fix, body, fs, tok, kind in cited:
        full = resolved.get(tok)
        # The incident must be in the window and OLDER than the fix.
        if not full or full == fix or full in seen or full not in order or order[full] <= order[fix]:
            continue
        fixed = sorted(set(fs) & files[full])
        if not fixed:
            continue
        seen.add(full)
        subj = next((c[1].strip().splitlines()[0] for c in commits if c[0] == full and c[1].strip()), "")
        out.append({"incident": full, "fix": fix, "kind": kind, "fix_files": fixed,
                    "subject": subj, "fix_subject": body.strip().splitlines()[0][:160]})
    os.makedirs(os.path.dirname(COLLECTION), exist_ok=True)
    with open(COLLECTION, "w") as f:
        for r in out:
            f.write(json.dumps(r) + "\n")
    print("collection: %d incidents (%d reverts, %d fix-forwards) from %d commits in the last %d days -> %s" % (
        len(out), sum(r["kind"] == "revert" for r in out), sum(r["kind"] == "fix_forward" for r in out),
        len(commits), days, os.path.relpath(COLLECTION, REPO)))
    return 0


def clean_commits(incidents, days, n, rng):
    bad = {r["incident"] for r in incidents}
    shas = git("log", "origin/main", "--since=%d.days" % days, "--until=7.days", "--no-merges", "--format=%H").split()
    rng.shuffle(shas)
    picked = []
    for s in shas:
        if s in bad or len(picked) >= n:
            continue
        fs = files_of(s)
        if not fs or len(git("show", "--format=", s)) > 120_000 or not any(f.endswith((".rs", ".js", ".py", ".sh", ".mjs")) or f == "amux" for f in fs):
            continue
        later = git("log", "origin/main", "--format=%H", "--since=@%s" % git("show", "-s", "--format=%ct", s).strip(),
                    "--until=@%d" % (int(git("show", "-s", "--format=%ct", s).strip()) + 7 * 86400), "--", *fs).split()
        if [x for x in later if x != s]:
            continue
        picked.append(s)
    return picked


def review_one(sha, tier, model):
    tree = tempfile.mkdtemp(prefix="review-eval-")
    git("worktree", "add", "-q", "--detach", tree, sha)
    try:
        import land_review
        key = "eval:%s:%d" % (sha[:12], time.time())
        return land_review.run(tree, sha + "^", sha, tier, key, model, sha + "^")
    finally:
        git("worktree", "remove", "--force", tree, check=False)


def score(a):
    os.environ["AMUX_LAND_GATE_HOME"] = tempfile.mkdtemp(prefix="review-eval-state-")
    sys.path.insert(0, HERE)
    incidents = [json.loads(l) for l in open(COLLECTION) if l.strip()]
    rng = random.Random(a.seed)
    sample = rng.sample(incidents, min(a.n, len(incidents)))
    rows, cost = [], 0.0
    for r in sample:
        out = review_one(r["incident"], a.tier, a.model)
        cost += out.get("cost_usd", 0)
        hit = [f for f in out.get("open", []) if f.get("severity") == "important" and f.get("file") in r["fix_files"]]
        rows.append({"kind": "incident", "sha": r["incident"][:12], "subject": r["subject"][:80],
                     "fix_files": r["fix_files"], "measured": out.get("measured"), "hit": bool(hit),
                     "findings": [(f["file"], f["line"], f["severity"], f["title"]) for f in out.get("open", [])]})
        print("incident %s %-5s %s" % (r["incident"][:12], "HIT" if hit else "miss", r["subject"][:70]))
    for s in clean_commits(incidents, a.days, a.clean, rng):
        out = review_one(s, a.tier, a.model)
        cost += out.get("cost_usd", 0)
        noisy = [f for f in out.get("open", []) if f.get("severity") == "important"]
        rows.append({"kind": "clean", "sha": s[:12], "measured": out.get("measured"), "noisy": bool(noisy),
                     "findings": [(f["file"], f["line"], f["severity"], f["title"]) for f in out.get("open", [])]})
        print("clean    %s %-5s %s" % (s[:12], "NOISY" if noisy else "quiet", git("show", "-s", "--format=%s", s).strip()[:70]))
    inc = [r for r in rows if r["kind"] == "incident" and r["measured"]]
    cln = [r for r in rows if r["kind"] == "clean" and r["measured"]]
    res = {"at": time.time(), "tier": a.tier, "model": a.model or "default", "collection": len(incidents),
           "recall": round(sum(r["hit"] for r in inc) / len(inc), 3) if inc else None, "n_incidents": len(inc),
           "noise": round(sum(r["noisy"] for r in cln) / len(cln), 3) if cln else None, "n_clean": len(cln),
           "unmeasured": sum(1 for r in rows if not r["measured"]), "cost_usd": round(cost, 2), "rows": rows}
    d = os.path.expanduser("~/.amux/land-gate/evals")
    os.makedirs(d, exist_ok=True)
    p = os.path.join(d, time.strftime("%Y%m%dT%H%M%S") + ".json")
    json.dump(res, open(p, "w"), indent=1)
    import land_review
    land_review.log("verdict=review_eval recall=%s n_incidents=%d noise=%s n_clean=%d unmeasured=%d tier=%s cost_usd=%.2f file=%s" % (
        res["recall"], res["n_incidents"], res["noise"], res["n_clean"], res["unmeasured"], a.tier, cost, p))
    print("recall %s over %d incidents, noise %s over %d clean commits, %d unmeasured, $%.2f\nresults: %s" % (
        res["recall"], res["n_incidents"], res["noise"], res["n_clean"], res["unmeasured"], cost, p))
    return 0 if res["n_incidents"] else 2


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--build", action="store_true")
    ap.add_argument("--score", action="store_true")
    ap.add_argument("--days", type=int, default=60)
    ap.add_argument("--n", type=int, default=5)
    ap.add_argument("--clean", type=int, default=5)
    ap.add_argument("--tier", default="standard", choices=["quick", "standard", "deep"])
    ap.add_argument("--model", default=os.environ.get("AMUX_LAND_REVIEW_MODEL", ""))
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    if a.build:
        return build(a.days)
    if a.score:
        return score(a)
    ap.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
