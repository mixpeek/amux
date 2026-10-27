#!/usr/bin/env python3
"""The land gate's push legs and the review's convergence rules.

Part 1 drives the REAL pre-push hook against a scratch repo whose origin/main
carries scripts/land_gate.py, land_review.py and review-tiers.txt from this
checkout, with a fake amux server for /api/land/approvals:
  - a person's push needs no receipt; an agent's push of unreceipted commits
    is refused, and a receipt for the same patches (even rebased) lets it in
  - the logged skip needs a reason
  - an [owner] path needs the owner's approval of that exact diff
  - a [deep] path needs a `Revert:` trailer
  - the tiers come from origin/main, so a push cannot drop its own tier
Part 2 runs scripts/land_review.py against a fake `claude` that answers from
fixtures, so the round logic is tested without a model:
  - an important finding blocks; the same finding is never reported twice
  - a prior finding the verifier calls fixed is closed
  - after three rounds the review stops hunting (no hunter call is made)
  - a finding the verifier rejects never reaches the author

Run: python3 scripts/git-hooks/test_land_gate.py   (exit 0 = pass)
"""
import http.server, json, os, shutil, subprocess, sys, tempfile, threading

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HOOK = os.path.join(ROOT, "scripts", "git-hooks", "pre-push")
GATE_FILES = ["scripts/land_gate.py", "scripts/land_review.py", "scripts/review-tiers.txt"]
passed = failed = 0


def check(label, got, want, detail=""):
    global passed, failed
    if got != want and detail:
        print(detail[-1500:])
    if got == want:
        print("  ok    %-66s %s" % (label, got))
        passed += 1
    else:
        print("  FAIL  %-66s got %r want %r" % (label, got, want))
        failed += 1


class Approvals(http.server.BaseHTTPRequestHandler):
    approved = set()
    requests = []

    def log_message(self, *a):
        pass

    def _send(self, code, body):
        b = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        pid = self.path.split("patch_id=", 1)[-1]
        self._send(200, {"patch_id": pid, "approved": pid in self.approved, "known": True})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        Approvals.requests.append(body)
        self._send(200, {"ok": True, "card": "AMUX-T1", "link": "/#land-approve=" + body.get("patch_id", "")})


def main():
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Approvals)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    tmp = tempfile.mkdtemp(prefix="land-gate-test-")
    gate_home = os.path.join(tmp, "gate-home")
    base_env = {k: v for k, v in os.environ.items() if not k.startswith(("AMUX_", "GIT_"))}
    base_env.update(HOME=tmp, AMUX_URL="http://127.0.0.1:%d" % srv.server_port, AMUX_LAND_GATE_HOME=gate_home,
                    GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")

    def run(work, *a, env=None, input=None):
        e = dict(base_env, **(env or {}))
        return subprocess.run(["git", "-C", work, *a], capture_output=True, text=True, timeout=300, env=e, input=input)

    def write(work, rel, text):
        p = os.path.join(work, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "w").write(text)

    def commit(work, msg, *paths):
        run(work, "add", "--", *paths)
        return run(work, "commit", "-qm", msg)

    # AMUX_PUSH_GUARD=0: the fixture's commits carry no Amux-Session trailer,
    # so the attribution guard (a different leg) would refuse every agent push.
    agent = {"AMUX_SESSION": "lane-t", "AMUX_PUSH_GUARD": "0"}
    try:
        origin, work = os.path.join(tmp, "origin.git"), os.path.join(tmp, "work")
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", origin], check=True)
        subprocess.run(["git", "init", "-q", "-b", "main", work], check=True)
        run(work, "remote", "add", "origin", origin)
        for f in GATE_FILES:
            shutil.copy(os.path.join(ROOT, f), os.path.join(work, f)) if os.path.isdir(os.path.dirname(os.path.join(work, f))) \
                else (os.makedirs(os.path.dirname(os.path.join(work, f))), shutil.copy(os.path.join(ROOT, f), os.path.join(work, f)))
        write(work, "src/app.txt", "one\n")
        write(work, "boundaries.toml", "[payments]\n")
        write(work, ".github/workflows/ci.yml", "on: push\n")
        commit(work, "fixture", ".")
        check("seed push by a person (gate not on origin/main yet) is allowed",
              run(work, "push", "-q", "origin", "main").returncode, 0)
        hook = os.path.join(work, ".git", "hooks", "pre-push")
        shutil.copy(HOOK, hook)
        os.chmod(hook, 0o755)
        run(work, "fetch", "-q", "origin")

        write(work, "src/app.txt", "two\n")
        commit(work, "feat: two", "src")
        r = run(work, "push", "origin", "main")
        check("a person's push needs no receipt", r.returncode, 0)

        write(work, "src/app.txt", "three\n")
        commit(work, "feat: three", "src")
        r = run(work, "push", "origin", "main", env=agent)
        check("an agent's push with no receipt is refused", r.returncode != 0, True)
        check("the refusal names the command that makes one", "scripts/land_gate.py" in r.stderr, True)
        r = run(work, "push", "origin", "main", env=dict(agent, AMUX_SKIP_LAND_GATE="1"))
        check("the skip without a reason is still refused", r.returncode != 0, True)

        # A receipt for these patches, written by the gate's own functions.
        sys.path.insert(0, os.path.join(ROOT, "scripts"))
        os.environ["AMUX_LAND_GATE_HOME"] = gate_home
        import land_gate
        land_gate.HOME = gate_home

        def receipt(passed_=True):
            run(work, "fetch", "-q", "origin")
            cwd = os.getcwd()
            os.chdir(work)
            try:
                base = land_gate.git(work, "rev-parse", "origin/main").strip()
                cp = land_gate.commit_patch_ids(work, base, "HEAD")
                land_gate.write_receipt({"patch_id": land_gate.patch_id(work, base, "HEAD"), "pass": passed_,
                                         "commit_patch_ids": sorted(set(cp.values()))})
            finally:
                os.chdir(cwd)
        receipt(passed_=False)
        check("a FAILED receipt does not let it in",
              run(work, "push", "origin", "main", env=agent).returncode != 0, True)
        receipt()
        # Rebase onto a moved main: new shas, same patches.
        other = os.path.join(tmp, "other")
        subprocess.run(["git", "clone", "-q", origin, other], check=True, env=base_env)
        write(other, "src/other.txt", "peer\n")
        commit(other, "feat: peer", "src")
        run(other, "push", "-q", "origin", "main")
        run(work, "fetch", "-q", "origin")
        run(work, "rebase", "-q", "origin/main")
        r = run(work, "push", "origin", "main", env=agent)
        check("a receipt covers the same patches after a rebase", r.returncode, 0, r.stderr)

        write(work, "src/app.txt", "four\n")
        commit(work, "feat: four", "src")
        r = run(work, "push", "origin", "main", env=dict(agent, AMUX_SKIP_LAND_GATE="1",
                                                        AMUX_SKIP_LAND_GATE_REASON="test of the logged skip"))
        check("the skip with a reason lands it", r.returncode, 0)
        check("and says so", "verdict=pre_push_receipt_skipped" in r.stderr, True)
        lg = open(os.path.join(tmp, ".amux", "logs", "land-gate.log")).read()
        check("the skip and its reason are in the log", "test of the logged skip" in lg, True)

        # [owner] path: needs the owner's approval of this exact diff.
        write(work, "boundaries.toml", "[payments]\ncap = 1\n")
        commit(work, "feat: cap payments\n\nRevert: git revert", "boundaries.toml")
        receipt()
        r = run(work, "push", "origin", "main", env=agent)
        check("an owner-gated change without approval is refused", r.returncode != 0, True)
        check("the refusal names the approval link", "#land-approve=" in r.stderr, True)
        check("an ask was filed with the server", bool(Approvals.requests) and
              Approvals.requests[-1].get("files") == ["boundaries.toml"], True)
        Approvals.approved.add(Approvals.requests[-1]["patch_id"])
        r = run(work, "push", "origin", "main", env=agent)
        check("once that diff is approved, it lands", r.returncode, 0)

        # [deep] path: needs a Revert: trailer.
        write(work, ".github/workflows/ci.yml", "on: [push]\n")
        commit(work, "ci: list form", ".github")
        receipt()
        r = run(work, "push", "origin", "main", env=agent)
        check("a workflow change with no Revert: trailer is refused", r.returncode != 0, True)
        check("the refusal names the commit", "ci: list form" in r.stderr, True)
        run(work, "commit", "-q", "--amend", "-m", "ci: list form\n\nRevert: git revert; nothing else reads it")
        receipt()
        check("with the trailer it lands", run(work, "push", "origin", "main", env=agent).returncode, 0)

        # Tiers come from origin/main: dropping boundaries.toml from [owner] in
        # the same push does not lower that push's tier.
        tiers = open(os.path.join(work, "scripts/review-tiers.txt")).read().replace("boundaries.toml\n", "")
        write(work, "scripts/review-tiers.txt", tiers)
        write(work, "boundaries.toml", "[payments]\ncap = 2\n")
        commit(work, "chore: relax\n\nRevert: git revert", "scripts", "boundaries.toml")
        receipt()
        n = len(Approvals.requests)
        r = run(work, "push", "origin", "main", env=agent)
        check("a push cannot drop its own tier", r.returncode != 0, True)
        check("and the ask lists both owner files", len(Approvals.requests) > n and
              sorted(Approvals.requests[-1]["files"]) == ["boundaries.toml", "scripts/review-tiers.txt"], True)
    finally:
        srv.shutdown()

    review_convergence(tmp)
    shutil.rmtree(tmp, ignore_errors=True)
    print("\nland gate: %d passed, %d failed" % (passed, failed))
    return 1 if failed else 0


FAKE_CLAUDE = r'''#!/usr/bin/env python3
import json, os, re, sys
prompt = sys.argv[sys.argv.index("-p") + 1]
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(("vote" if "verifying review findings" in prompt else "hunt") + "\n")
if "verifying review findings" in prompt:
    plan = json.loads(os.environ.get("FAKE_VOTES", "{}"))
    ids = re.findall(r"^- id (F\d+):", prompt, re.M)
    out = {"votes": [{"id": i, "verdict": plan.get(i, "real"), "why": "fixture"} for i in ids]}
else:
    out = {"findings": json.loads(os.environ.get("FAKE_FINDINGS", "[]")), "summary": "fixture"}
print(json.dumps({"structured_output": out, "total_cost_usd": 0.01, "is_error": False}))
'''


def review_convergence(tmp):
    repo = os.path.join(tmp, "rv")
    subprocess.run(["git", "init", "-q", "-b", "main", repo], check=True)
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
    open(os.path.join(repo, "a.py"), "w").write("x = 1\n")
    subprocess.run(["git", "-C", repo, "add", "."], check=True, env=env)
    subprocess.run(["git", "-C", repo, "commit", "-qm", "base"], check=True, env=env)
    base = subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()

    def change(text, msg):
        open(os.path.join(repo, "a.py"), "w").write(text)
        subprocess.run(["git", "-C", repo, "commit", "-qam", msg], check=True, env=env)
        return subprocess.run(["git", "-C", repo, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()

    fake = os.path.join(tmp, "fake-claude")
    open(fake, "w").write(FAKE_CLAUDE)
    os.chmod(fake, 0o755)
    log = os.path.join(tmp, "fake.log")
    os.environ.update(AMUX_LAND_REVIEW_CLAUDE=fake, FAKE_LOG=log, AMUX_LAND_GATE_HOME=os.path.join(tmp, "rv-state"))
    sys.path.insert(0, os.path.join(ROOT, "scripts"))
    import importlib
    import land_review
    importlib.reload(land_review)
    land_review.LOG = os.path.join(tmp, "rv.log")
    bug = {"file": "a.py", "line": 1, "severity": "important", "title": "x is never validated before division",
           "failure_scenario": "x=0 divides by zero", "bug_class": "input"}
    nit = {"file": "a.py", "line": 1, "severity": "important", "title": "rename variable for clarity",
           "failure_scenario": "none", "bug_class": "style"}

    def calls():
        return open(log).read().split() if os.path.exists(log) else []

    h1 = change("x = 2\n", "feat: two\n\nAmux-Request: MSG-1")
    os.environ.update(FAKE_FINDINGS=json.dumps([bug, nit]), FAKE_VOTES=json.dumps({"F2": "style"}))
    out = land_review.run(repo, base, h1, "standard", "req:MSG-1", "", "HEAD")
    check("round 1: the confirmed important finding blocks", out["verdict"], "review_blocking")
    check("round 1: the style finding the verifier rejected is dropped", [f["title"] for f in out["open"]], [bug["title"]])
    check("round 1: two hunters and one verifier ran", calls(), ["hunt", "hunt", "vote"])

    h2 = change("x = 3\n", "fix: three\n\nAmux-Request: MSG-1")
    os.environ.update(FAKE_FINDINGS=json.dumps([dict(bug, line=2)]), FAKE_VOTES=json.dumps({"F1": "still_open"}))
    out = land_review.run(repo, base, h2, "standard", "req:MSG-1", "", "HEAD")
    check("round 2: reviews only the change since round 1", out["mode"], "incremental")
    check("round 2: the same finding is not reported again", len(out["new"]), 0)
    check("round 2: still blocking while it is open", out["verdict"], "review_blocking")

    h3 = change("x = 4\n", "fix: four\n\nAmux-Request: MSG-1")
    os.environ.update(FAKE_FINDINGS="[]", FAKE_VOTES=json.dumps({"F1": "fixed"}))
    out = land_review.run(repo, base, h3, "standard", "req:MSG-1", "", "HEAD")
    check("round 3: the verifier closes the fixed finding", out["closed"], ["F1"])
    check("round 3: passes", out["verdict"], "review_pass")

    n = len(calls())
    h4 = change("x = 5\n", "fix: five\n\nAmux-Request: MSG-1")
    os.environ.update(FAKE_FINDINGS=json.dumps([dict(bug, title="a brand new thing in round four")]))
    out = land_review.run(repo, base, h4, "standard", "req:MSG-1", "", "HEAD")
    check("round 4: capped, the author is told to rethink", out["verdict"], "review_capped")
    check("round 4: no hunter ran", "hunt" in calls()[n:], False)

    land_review.resolve("req:MSG-9", "F1", "x") if False else None
    h5 = change("x = 6\n", "feat: other\n\nAmux-Request: MSG-2")
    os.environ.update(FAKE_FINDINGS=json.dumps([bug]), FAKE_VOTES="{}")
    out = land_review.run(repo, h4, h5, "quick", "req:MSG-2", "", "HEAD")
    check("a different change starts at round 1 with its own findings", (out["round"], out["verdict"]),
          (1, "review_blocking"))
    land_review.resolve("req:MSG-2", out["blocking"][0]["id"], "x is checked by the caller")
    out = land_review.run(repo, h4, h5, "quick", "req:MSG-2", "", "HEAD")
    check("a finding resolved with a reason stops blocking", out["verdict"], "review_pass")

    os.environ["AMUX_LAND_REVIEW_CLAUDE"] = os.path.join(tmp, "no-such-claude")
    out = land_review.run(repo, h4, h5, "quick", "req:MSG-3", "", "HEAD")
    check("a review that could not run says unmeasured, never pass", out["verdict"], "review_unmeasured")


if __name__ == "__main__":
    sys.exit(main())
