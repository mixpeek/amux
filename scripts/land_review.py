#!/usr/bin/env python3
"""The review a change gets before it lands on main (scripts/land_gate.py).

amux had no review layer: everything goes straight to main, and CI only runs
after the push. This is the pre-merge review the Claude Code team describes in
"How the Claude Code team does code review" (2026-10-26), cut to what fits a
repo with no pull requests:

- Fresh-context hunters, in parallel, each with one focus (1 quick, 2
  standard, 3 deep). They read the diff, the commit messages (the intent) and
  REVIEW.md, and may read any file, but cannot edit.
- A verifier votes on every candidate. Only findings it confirms survive, and
  style is dropped. Severity is important, nit or pre-existing; only an
  important finding introduced by this change blocks.
- Convergence. Their CLI repo saw one PR cycle more than fifty times overnight
  between an agent author and an agent reviewer, and ~59% of repeat flags were
  code from the first pass. So, per change: a finding is never reported twice,
  rounds 2 and 3 review only what changed since the last round, and after
  three rounds the review stops hunting and tells the author to rethink the
  approach. Open findings must still be fixed or resolved with a reason.

    scripts/land_review.py --repo DIR --base SHA --head SHA [--tier quick|standard|deep]
                           [--key KEY] [--model M] [--resolve ID REASON] [--json]

State: ~/.amux/land-gate/reviews/<key hash>.json. Log: ~/.amux/logs/land-gate.log.
The verdict line is computed from the run: `review_pass`, `review_blocking`,
`review_capped` or `review_unmeasured` (a hunter or the verifier did not run).
"""
import argparse, concurrent.futures, fnmatch, hashlib, json, os, re, subprocess, sys, time

MAX_ROUNDS = 3
HUNTERS = {"quick": 1, "standard": 2, "deep": 3}
VOTERS = {"quick": 1, "standard": 1, "deep": 2}
CALL_TIMEOUT_S = int(os.environ.get("AMUX_LAND_REVIEW_TIMEOUT_S", "900"))
MAX_DIFF_CHARS = 180_000
STATE_DIR = os.path.expanduser(os.environ.get("AMUX_LAND_GATE_HOME", "~/.amux/land-gate"))
LOG = os.path.expanduser("~/.amux/logs/land-gate.log")
# Never reviewed: the same list REVIEW.md names under "Ignored paths".
IGNORED = ["frustrations.md", "frustrations-archive.md", "docs/**", "e2e/test-results/**",
           "**/*.snap", "crates/amux-dashboard/static/vendor/**"]

FOCUS = [
    "CORRECTNESS. Logic errors, wrong conditions, unhandled errors and edge cases, races, "
    "resource leaks, and the change failing to do what its commit message says it does.",
    "CONTRACTS AND BLAST RADIUS. Every failure class REVIEW.md names. Then follow the changed "
    "code outward: who calls it, what reads what it writes, which tree-wide contract or test "
    "it must still satisfy, and what other part of amux breaks.",
    "ROOT CAUSE AND INPUTS. For a bug fix, whether the root cause is fixed or only the symptom, "
    "and whether the fix surfaces in logs as CLAUDE.md requires. For new inputs, whether a "
    "worker, a remote browser or a malformed value can make it misbehave or leak.",
]

FINDINGS_SCHEMA = {
    "type": "object",
    "properties": {
        "findings": {"type": "array", "items": {"type": "object", "properties": {
            "file": {"type": "string"}, "line": {"type": "integer"},
            "severity": {"type": "string", "enum": ["important", "nit", "pre-existing"]},
            "title": {"type": "string"},
            "failure_scenario": {"type": "string"},
            "bug_class": {"type": "string"},
        }, "required": ["file", "line", "severity", "title", "failure_scenario", "bug_class"]}},
        "summary": {"type": "string"},
    },
    "required": ["findings", "summary"],
}
VOTE_SCHEMA = {
    "type": "object",
    "properties": {"votes": {"type": "array", "items": {"type": "object", "properties": {
        "id": {"type": "string"},
        "verdict": {"type": "string", "enum": ["real", "false_positive", "style", "fixed", "still_open"]},
        "why": {"type": "string"},
    }, "required": ["id", "verdict", "why"]}}},
    "required": ["votes"],
}


def git(repo, *a, check=True):
    r = subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True, timeout=120)
    if check and r.returncode != 0:
        raise RuntimeError("git %s: %s" % (" ".join(a), r.stderr.strip()[:300]))
    return r.stdout


def ignored(path):
    return any(fnmatch.fnmatch(path, g) or (g.endswith("/**") and path.startswith(g[:-2])) for g in IGNORED)


def log(line):
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "a") as f:
        f.write("%s land_review %s\n" % (time.strftime("%Y-%m-%dT%H:%M:%S"), line))


def change_key(repo, base, head):
    """One key per change, stable across rebases and fix-up commits: the
    Amux-Request ids it cites, else its first commit's subject."""
    msgs = git(repo, "log", "--reverse", "--format=%B%x00", "%s..%s" % (base, head))
    reqs = sorted(set(re.findall(r"^Amux-Request:\s*(.+)$", msgs, re.M)))
    reqs = sorted({x.strip() for r in reqs for x in r.split(",") if x.strip()})
    if reqs:
        return "req:" + ",".join(reqs)
    first = msgs.split("\x00", 1)[0].strip().splitlines()
    return "subject:" + (first[0].strip().lower() if first else head)


def state_path(key):
    return os.path.join(STATE_DIR, "reviews", hashlib.sha1(key.encode()).hexdigest()[:16] + ".json")


def load_state(key):
    try:
        with open(state_path(key)) as f:
            return json.load(f)
    except (OSError, ValueError):
        return {"key": key, "rounds": [], "findings": {}}


def save_state(st):
    p = state_path(st["key"])
    os.makedirs(os.path.dirname(p), exist_ok=True)
    tmp = p + ".tmp.%d" % os.getpid()
    with open(tmp, "w") as f:
        json.dump(st, f, indent=1)
    os.replace(tmp, p)


def fingerprint(f):
    words = re.findall(r"[a-z0-9_]+", f.get("title", "").lower())
    return f.get("file", ""), set(w for w in words if len(w) > 2)


def same_finding(a, b):
    """The same file, lines within 8 of each other, and at least half the
    title's words shared. Errs toward SAME: a re-report costs an agent author
    a whole cycle, a merged pair costs one line of output."""
    fa, wa = fingerprint(a)
    fb, wb = fingerprint(b)
    if fa != fb or abs(int(a.get("line", 0)) - int(b.get("line", 0))) > 8:
        return False
    if not wa or not wb:
        return True
    return len(wa & wb) / min(len(wa), len(wb)) >= 0.5


def review_md(repo, ref, files):
    """REVIEW.md at the root and the nearest one above each changed file,
    read from `ref` (origin/main), so a change cannot rewrite its own rubric."""
    seen, out = set(), []
    for f in [""] + sorted(files):
        d = os.path.dirname(f)
        while True:
            p = (d + "/REVIEW.md") if d else "REVIEW.md"
            if p not in seen:
                seen.add(p)
                body = git(repo, "show", "%s:%s" % (ref, p), check=False)
                if body:
                    out.append("## %s\n%s" % (p, body))
                    break
            if not d:
                break
            d = os.path.dirname(d)
    return "\n\n".join(out)


def claude(repo, prompt, schema, model):
    """One fresh-context, read-only Claude Code run. Returns (parsed or None,
    cost, error). The child gets no amux session identity, so its hooks do not
    report as the lane that called it."""
    env = {k: v for k, v in os.environ.items()
           if k not in ("AMUX_SESSION", "AMUX_WORKER", "TMUX", "TMUX_PANE", "CLAUDECODE")}
    cmd = [os.environ.get("AMUX_LAND_REVIEW_CLAUDE", "claude"), "-p", prompt,
           "--setting-sources", "project", "--strict-mcp-config", "--no-session-persistence",
           "--allowedTools", "Read", "Grep", "Glob", "Bash(git diff:*)", "Bash(git show:*)",
           "Bash(git log:*)", "Bash(git blame:*)", "Bash(git grep:*)",
           "--disallowedTools", "Edit", "Write", "NotebookEdit",
           "--output-format", "json", "--json-schema", json.dumps(schema)]
    if model:
        cmd += ["--model", model]
    try:
        r = subprocess.run(cmd, cwd=repo, env=env, capture_output=True, text=True, timeout=CALL_TIMEOUT_S)
    except (OSError, subprocess.TimeoutExpired) as e:
        return None, 0.0, "claude did not run: %s" % e
    try:
        d = json.loads(r.stdout)
    except ValueError:
        return None, 0.0, "claude printed no JSON (exit %d): %s" % (r.returncode, (r.stderr or r.stdout)[-400:])
    cost = float(d.get("total_cost_usd") or 0)
    if d.get("is_error") or not isinstance(d.get("structured_output"), dict):
        return None, cost, "claude returned no structured result: %s" % str(d.get("result"))[:400]
    return d["structured_output"], cost, None


def hunt_prompt(focus, ctx):
    return (
        "You are reviewing a change to amux before it lands on main. Fresh context: you did not "
        "write it. Your one focus:\n\n" + focus + "\n\n"
        "Report only real defects that this change introduces or exposes, each with a file and "
        "line that exist, the concrete input or state that goes wrong, and the wrong result. "
        "Read the code the diff calls and the code that calls it before claiming anything; do not "
        "guess filenames. Style, naming, comments and refactors are not findings. A bug that was "
        "already there before this change is severity pre-existing. Zero findings is a normal "
        "answer: say \"no issues\" in the summary.\n\n" + ctx)


def vote_prompt(cands, prior, ctx):
    lines = ["You are verifying review findings on a change to amux before it lands. For each "
             "NEW finding, read the code and vote: real (a defect this change introduces, as "
             "described), false_positive (the code does not behave that way), or style (true but "
             "only style, naming or cleanup). For each PRIOR finding, vote fixed (the current code "
             "no longer has it) or still_open. Be strict: a finding you cannot confirm from the "
             "code is a false_positive.", "", "NEW findings:"]
    for c in cands:
        lines.append("- id %s: %s:%s [%s] %s. Failure: %s" % (
            c["id"], c["file"], c["line"], c["severity"], c["title"], c["failure_scenario"]))
    if prior:
        lines += ["", "PRIOR findings (reported in an earlier round, still open):"]
        for c in prior:
            lines.append("- id %s: %s:%s %s. Failure: %s" % (
                c["id"], c["file"], c["line"], c["title"], c["failure_scenario"]))
    return "\n".join(lines) + "\n\n" + ctx


def context(repo, base, head, files, since, rubric_ref):
    log_text = git(repo, "log", "--format=commit %h%n%B", "%s..%s" % (base, head))
    paths = [f for f in files if not ignored(f)]
    diff = git(repo, "diff", "--stat", "-p", base, head, "--", *paths) if paths else ""
    parts = ["# Commit messages (the author's stated intent)\n" + log_text.strip()]
    if since:
        inc = git(repo, "diff", since, head, "--", *paths, check=False) if paths else ""
        parts.append("# This is review round 2 or 3. Review ONLY what changed since the last "
                     "round (below). The whole change is given after it for reference; do not "
                     "re-review unchanged code.\n```diff\n" + inc[:MAX_DIFF_CHARS // 2] + "\n```")
    if len(diff) > MAX_DIFF_CHARS:
        diff = diff[:MAX_DIFF_CHARS] + "\n... (diff truncated at %d characters; read the files)" % MAX_DIFF_CHARS
    parts.append("# The change: git diff %s %s\n```diff\n%s\n```" % (base[:12], head[:12], diff))
    rubric = review_md(repo, rubric_ref, paths)
    if rubric:
        parts.append("# REVIEW.md (what counts as a failure here)\n" + rubric)
    return "\n\n".join(parts)


def run(repo, base, head, tier, key, model, rubric_ref):
    t0 = time.time()
    files = [f for f in git(repo, "diff", "--name-only", base, head).splitlines() if f]
    st = load_state(key)
    rnd = len(st["rounds"]) + 1
    prior_open = [f for f in st["findings"].values() if f["status"] == "open"]
    out = {"key": key, "round": rnd, "tier": tier, "files": len(files), "cost_usd": 0.0,
           "measured": True, "errors": [], "new": [], "closed": []}
    reviewable = [f for f in files if not ignored(f)]
    if not reviewable:
        out.update(mode="nothing_to_review")
        return finish(st, out, head, t0)
    since = None
    if 1 < rnd <= MAX_ROUNDS:
        prev = st["rounds"][-1].get("head")
        if prev and git(repo, "cat-file", "-t", prev, check=False).strip() == "commit":
            since = prev
    ctx = context(repo, base, head, files, since, rubric_ref)
    cands = []
    if rnd > MAX_ROUNDS:
        out["mode"] = "capped"
    else:
        out["mode"] = "incremental" if since else "full"
        n = HUNTERS[tier]
        focuses = [FOCUS[0] + "\n\n" + FOCUS[1]] if n == 1 else FOCUS[:n]
        with concurrent.futures.ThreadPoolExecutor(len(focuses)) as ex:
            results = list(ex.map(lambda fo: claude(repo, hunt_prompt(fo, ctx), FINDINGS_SCHEMA, model), focuses))
        for parsed, cost, err in results:
            out["cost_usd"] += cost
            if err:
                out["errors"].append(err)
                continue
            out.setdefault("summaries", []).append(str(parsed.get("summary", ""))[:600])
            for f in parsed.get("findings", []):
                if ignored(f.get("file", "")):
                    continue
                known = [p for p in st["findings"].values() if same_finding(f, p)]
                dup = [c for c in cands if same_finding(f, c)]
                if not known and not dup:
                    cands.append(f)
    if out["errors"] and len(out["errors"]) == HUNTERS[tier]:
        out["measured"] = False
        return finish(st, out, head, t0)
    base_id = len(st["findings"])
    for i, c in enumerate(cands):
        c["id"] = "F%d" % (base_id + i + 1)
    votes = {}
    if cands or prior_open:
        prompt = vote_prompt(cands, prior_open, ctx)
        n = VOTERS[tier]
        with concurrent.futures.ThreadPoolExecutor(n) as ex:
            results = list(ex.map(lambda _: claude(repo, prompt, VOTE_SCHEMA, model), range(n)))
        ran = 0
        for parsed, cost, err in results:
            out["cost_usd"] += cost
            if err:
                out["errors"].append(err)
                continue
            ran += 1
            for v in parsed.get("votes", []):
                votes.setdefault(v["id"], []).append(v)
        if not ran:
            out["measured"] = False
            return finish(st, out, head, t0)
        need = ran
    for c in cands:
        vs = votes.get(c["id"], [])
        real = sum(1 for v in vs if v["verdict"] == "real")
        c["votes"] = ["%s: %s" % (v["verdict"], v["why"][:200]) for v in vs]
        if real >= need:
            c.update(status="open", round=rnd)
            st["findings"][c["id"]] = c
            out["new"].append(c)
        else:
            st.setdefault("rejected", []).append({"title": c["title"], "file": c["file"],
                                                  "line": c["line"], "votes": c["votes"], "round": rnd})
    for p in prior_open:
        vs = votes.get(p["id"], [])
        if vs and all(v["verdict"] == "fixed" for v in vs):
            p.update(status="fixed", fixed_round=rnd)
            out["closed"].append(p["id"])
    return finish(st, out, head, t0)


def finish(st, out, head, t0):
    out["secs"] = round(time.time() - t0, 1)
    out["cost_usd"] = round(out["cost_usd"], 4)
    if out.get("mode") != "nothing_to_review":
        st["rounds"].append({"head": head, "at": time.time(), "mode": out.get("mode"),
                             "new": len(out["new"]), "measured": out["measured"]})
    save_state(st)
    open_ = [f for f in st["findings"].values() if f["status"] == "open"]
    out["open"] = open_
    out["blocking"] = [f for f in open_ if f.get("severity") == "important"]
    if not out["measured"]:
        out["verdict"] = "review_unmeasured"
    elif out["blocking"]:
        out["verdict"] = "review_blocking"
    elif out.get("mode") == "capped":
        out["verdict"] = "review_capped"
    else:
        out["verdict"] = "review_pass"
    log("verdict=%s key=%s round=%d mode=%s new=%d open=%d blocking=%d closed=%d cost_usd=%.2f secs=%.0f measured=%s"
        % (out["verdict"], st["key"], out["round"], out.get("mode"), len(out["new"]), len(open_),
           len(out["blocking"]), len(out["closed"]), out["cost_usd"], out["secs"], str(out["measured"]).lower()))
    return out


def resolve(key, fid, reason):
    st = load_state(key)
    f = st["findings"].get(fid)
    if not f:
        raise SystemExit("no finding %s for %s (open: %s)" % (
            fid, key, ", ".join(k for k, v in st["findings"].items() if v["status"] == "open") or "none"))
    f.update(status="resolved", reason=reason, resolved_at=time.time())
    save_state(st)
    log("verdict=review_resolved key=%s id=%s reason=%s" % (key, fid, json.dumps(reason)))


def render(out):
    lines = []
    mode = out.get("mode")
    if mode == "capped":
        lines.append("review: round %d. Three rounds are done, so it no longer hunts. Re-evaluate the "
                     "approach rather than patching finding by finding." % out["round"])
    elif mode == "nothing_to_review":
        lines.append("review: nothing reviewable changed (only ignored paths)")
    else:
        lines.append("review: round %d (%s, %s tier), %d new, %d closed as fixed, $%.2f, %.0fs"
                     % (out["round"], mode, out["tier"], len(out["new"]), len(out["closed"]),
                        out["cost_usd"], out["secs"]))
    for e in out["errors"]:
        lines.append("  UNMEASURED  %s" % e)
    for f in out["open"]:
        lines.append("  %s %-12s %s:%s %s\n      %s" % (f["id"], f["severity"], f["file"], f["line"],
                                                       f["title"], f["failure_scenario"]))
    if out["blocking"]:
        lines.append("  Fix each important finding, or close it with a reason:\n"
                     "    scripts/land_gate.py --resolve <id> \"<why it is not a bug>\"")
    if not out["open"] and out["measured"]:
        lines.append("  no open findings")
    lines.append("verdict=%s" % out["verdict"])
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", default=".")
    ap.add_argument("--base")
    ap.add_argument("--head", default="HEAD")
    ap.add_argument("--rubric-ref", default="origin/main")
    ap.add_argument("--tier", choices=list(HUNTERS), default="standard")
    ap.add_argument("--key")
    ap.add_argument("--model", default=os.environ.get("AMUX_LAND_REVIEW_MODEL", ""))
    ap.add_argument("--resolve", nargs=2, metavar=("ID", "REASON"))
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    head = git(a.repo, "rev-parse", a.head).strip()
    base = a.base or git(a.repo, "merge-base", "origin/main", head).strip()
    key = a.key or change_key(a.repo, base, head)
    if a.resolve:
        resolve(key, *a.resolve)
        return 0
    out = run(a.repo, base, head, a.tier, key, a.model, a.rubric_ref)
    print(json.dumps(out, indent=1) if a.json else render(out))
    return {"review_pass": 0, "review_capped": 0, "review_blocking": 1}.get(out["verdict"], 2)


if __name__ == "__main__":
    sys.exit(main())
