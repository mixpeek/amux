#!/usr/bin/env python3
"""Check a change on main-plus-the-change BEFORE it lands, and record a receipt.

amux checks after the fact: CI runs once a commit is already on main, and a red
main holds every lane's deploy. Two days to 2026-10-27 held 30 red `rust` runs.
The Rust failures were all sub-second contract tests (route_table,
timestamp_units_declared, system_jobs) and the e2e failures one spec at a time.
Both are cheap to catch before the push, on the composed tree, which is what a
merge queue buys.

    scripts/land_gate.py                       # gate HEAD against origin/main
    scripts/land_gate.py --resolve F2 "why"     # close a review finding with a reason
    scripts/land_gate.py --pre-push --remote R --local L   # what .git/hooks/pre-push runs

Legs, in order, each with a computed verdict:
  owner    a change to an [owner] path in scripts/review-tiers.txt needs the
           owner's approval of that exact diff (patch-id). The gate files the
           ask; he approves at #land-approve=<patch-id> in the dashboard.
  revert   every commit touching an [owner] or [deep] path, or a migration,
           carries `Revert: <how>`, because `git revert` alone does not undo a
           workflow, an installer or a schema.
  rust     cargo nextest on the composed tree: the unit tests of every changed
           module plus every integration test of a changed crate.
  e2e      the Playwright specs scripts/ci-affected.mjs selects, desktop and
           mobile, built from the composed tree.
  review   scripts/land_review.py, at the change's tier.

A test that fails, passes on one rerun, is flaky and does not block. A test
that also fails on origin/main is inherited and does not block. Either is
named in the receipt.

Receipt: ~/.amux/land-gate/receipts/<patch-id>.json, keyed by the patch-ids of
the change's commits, which survive a clean rebase. The pre-push hook refuses
an agent's push of commits no passing receipt covers (AMUX_LAND_GATE_REQUIRED=0
turns that off; AMUX_SKIP_LAND_GATE=1 with AMUX_SKIP_LAND_GATE_REASON skips it
once, logged). Log: ~/.amux/logs/land-gate.log.
"""
import argparse, glob, hashlib, json, os, random, re, shutil, ssl, subprocess, sys, tempfile, time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
HOME = os.path.expanduser(os.environ.get("AMUX_LAND_GATE_HOME", "~/.amux/land-gate"))
LOG = os.path.expanduser("~/.amux/logs/land-gate.log")
RECEIPT_TTL_S = 14 * 86400
MAX_LOCAL_SPECS = int(os.environ.get("AMUX_LAND_GATE_MAX_SPECS", "12"))
RUST_TIMEOUT_S = int(os.environ.get("AMUX_LAND_GATE_RUST_TIMEOUT_S", "2400"))
E2E_TIMEOUT_S = int(os.environ.get("AMUX_LAND_GATE_E2E_TIMEOUT_S", "2400"))
MIGRATIONS = "crates/amux-server/migrations/"
SMOKE_FALLBACK = ["worker-toolbar-boot.spec.ts", "settings-layout.spec.ts"]


def sh(cmd, cwd=None, env=None, timeout=600, check=False):
    r = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
    if check and r.returncode != 0:
        raise RuntimeError("%s: %s" % (" ".join(cmd[:4]), (r.stderr or r.stdout).strip()[-400:]))
    return r


def git(repo, *a, check=True, timeout=120):
    return sh(["git", "-C", repo, *a], timeout=timeout, check=check).stdout


def log(line):
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    with open(LOG, "a") as f:
        f.write("%s land_gate %s\n" % (time.strftime("%Y-%m-%dT%H:%M:%S"), line))


def lane():
    for k in ("AMUX_SESSION", "AMUX_WORKER", "AMUX_ISOLATED_WORKER"):
        if os.environ.get(k, "").strip():
            return os.environ[k].strip()
    return ""


def patch_id(repo, *diff_args):
    d = git(repo, "diff", *diff_args, check=False)
    if not d.strip():
        return ""
    r = subprocess.run(["git", "-C", repo, "patch-id", "--stable"], input=d, capture_output=True, text=True)
    return (r.stdout.split() or [""])[0]


def commit_patch_ids(repo, base, head):
    out = {}
    for c in git(repo, "rev-list", "--reverse", "--no-merges", "%s..%s" % (base, head)).split():
        show = subprocess.run(["git", "-C", repo, "show", "--format=", c], capture_output=True, text=True)
        r = subprocess.run(["git", "-C", repo, "patch-id", "--stable"], input=show.stdout,
                           capture_output=True, text=True)
        pid = (r.stdout.split() or [""])[0]
        out[c] = pid or "empty:" + git(repo, "rev-parse", c + "^{tree}").strip()
    return out


# ---------- tiers ----------

def glob_re(g):
    out, i = "", 0
    while i < len(g):
        if g.startswith("**/", i):
            out += "(?:.*/)?"; i += 3
        elif g.startswith("**", i):
            out += ".*"; i += 2
        elif g[i] == "*":
            out += "[^/]*"; i += 1
        elif g[i] == "?":
            out += "[^/]"; i += 1
        else:
            out += re.escape(g[i]); i += 1
    return re.compile("^" + out + "$")


def parse_tiers(text):
    tiers, cur = {"owner": [], "deep": []}, None
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        m = re.match(r"^\[(\w+)\]$", line)
        if m:
            cur = m.group(1)
            tiers.setdefault(cur, [])
        elif cur:
            tiers[cur].append(glob_re(line))
    return tiers


def load_tiers(repo):
    """From origin/main, so the change under review cannot lower its own tier.
    Before this file has ever landed, the change's own copy is the only one."""
    text = git(repo, "show", "origin/main:scripts/review-tiers.txt", check=False)
    src = "origin/main"
    if not text:
        text = open(os.path.join(HERE, "review-tiers.txt")).read() if os.path.exists(
            os.path.join(HERE, "review-tiers.txt")) else ""
        src = "this change (not on origin/main yet)"
    return parse_tiers(text), src


def in_tier(tiers, name, path):
    return any(r.match(path) for r in tiers.get(name, []))


def tier_for(tiers, files, lines_changed):
    if any(in_tier(tiers, "owner", f) or in_tier(tiers, "deep", f) for f in files):
        return "deep"
    return "quick" if lines_changed < 80 and len(files) <= 3 else "standard"


# ---------- owner approvals (server: /api/land/approvals) ----------

def api_base():
    try:
        with open(os.path.expanduser("~/.amux/endpoint.json")) as f:
            ep = json.load(f)
        return (ep.get("url") or "https://localhost:%s" % ep.get("canonical_port", 8824)).rstrip("/")
    except (OSError, ValueError):
        return (os.environ.get("AMUX_URL") or "https://localhost:8824").rstrip("/")


def api(method, path, body=None):
    ctx = ssl.create_default_context()
    ctx.check_hostname, ctx.verify_mode = False, ssl.CERT_NONE
    headers = {"Content-Type": "application/json"}
    if lane():
        headers["X-Amux-Session"] = lane()
    req = urllib.request.Request(api_base() + path, method=method, headers=headers,
                                 data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, context=ctx, timeout=20) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"{}")
        except ValueError:
            return e.code, {}
    except (OSError, ValueError) as e:
        return 0, {"error": str(e)}


def owner_leg(repo, base, head, tiers, request=True):
    files = [f for f in git(repo, "diff", "--name-only", base, head).splitlines() if in_tier(tiers, "owner", f)]
    if not files:
        return {"verdict": "owner_not_needed"}
    pid = patch_id(repo, base, head, "--", *files)
    code, body = api("GET", "/api/land/approvals?patch_id=" + pid)
    if code == 200 and body.get("approved"):
        return {"verdict": "owner_approved", "patch_id": pid, "files": files,
                "approved_at": body.get("approved_at"), "by": body.get("by")}
    if code != 200:
        return {"verdict": "owner_unmeasured", "patch_id": pid, "files": files,
                "detail": "GET /api/land/approvals answered %s %s" % (code, body.get("error", ""))}
    out = {"verdict": "owner_pending", "patch_id": pid, "files": files}
    if request:
        stat = git(repo, "diff", "--stat", base, head, "--", *files)
        subj = git(repo, "log", "--format=%s", "%s..%s" % (base, head)).splitlines()
        c2, b2 = api("POST", "/api/land/approvals/request", {
            "patch_id": pid, "files": files, "diffstat": stat[-3000:], "subjects": subj[:20],
            "head": head, "lane": lane()})
        out["request"] = {"code": c2, "card": b2.get("card"), "link": b2.get("link"), "error": b2.get("error")}
    return out


# ---------- revert trailers ----------

def revert_leg(repo, base, head, tiers):
    missing = []
    for c in git(repo, "rev-list", "--no-merges", "%s..%s" % (base, head)).split():
        files = git(repo, "show", "--name-only", "--format=", c).split()
        hit = [f for f in files if f.startswith(MIGRATIONS) or in_tier(tiers, "owner", f) or in_tier(tiers, "deep", f)]
        if not hit:
            continue
        msg = git(repo, "show", "-s", "--format=%B", c)
        if re.search(r"^Revert:\s*\S", msg, re.M | re.I) or "This reverts commit" in msg:
            continue
        missing.append({"commit": c[:12], "subject": msg.splitlines()[0] if msg else "", "files": hit[:5]})
    return {"verdict": "revert_missing" if missing else "revert_ok", "missing": missing}


# ---------- the composed tree ----------

def gate_tree(name, suffix=""):
    return os.path.join(os.path.expanduser("~/.amux/tmp/land-gate"), re.sub(r"[^\w.-]", "_", name or "human") + suffix)


def checkout(repo, tree, ref):
    if not os.path.isdir(os.path.join(tree, ".git")) and not os.path.isfile(os.path.join(tree, ".git")):
        os.makedirs(os.path.dirname(tree), exist_ok=True)
        git(repo, "worktree", "add", "-q", "--detach", tree, ref, timeout=600)
    git(tree, "reset", "-q", "--hard", ref, timeout=600)
    git(tree, "clean", "-fdq", "-e", "node_modules", "-e", "target", timeout=600)


def compose(repo, tree, base, head, main):
    """origin/main with the change's commits replayed on it, hooks off. The
    result's commits are new; their trees are what lands."""
    checkout(repo, tree, main)
    if sh(["git", "-C", repo, "merge-base", "--is-ancestor", main, head]).returncode == 0:
        git(tree, "reset", "-q", "--hard", head)
        return head
    r = sh(["git", "-C", tree, "-c", "core.hooksPath=/dev/null", "cherry-pick", "--allow-empty",
            "--keep-redundant-commits", "%s..%s" % (base, head)], timeout=600)
    if r.returncode != 0:
        sh(["git", "-C", tree, "cherry-pick", "--abort"])
        raise RuntimeError("the change does not apply to origin/main %s; rebase and run again.\n%s"
                           % (main[:12], (r.stderr or r.stdout)[-600:]))
    return git(tree, "rev-parse", "HEAD").strip()


def link_node_modules(repo, tree):
    common = git(repo, "rev-parse", "--path-format=absolute", "--git-common-dir").strip()
    for c in (os.environ.get("AMUX_SPA_GATE_NODE_MODULES", ""), os.path.join(os.path.dirname(common), "node_modules")):
        if c and os.path.isdir(c):
            dst = os.path.join(tree, "node_modules")
            if os.path.islink(dst):
                os.unlink(dst)
            if not os.path.exists(dst):
                os.symlink(c, dst)
            return dst
    return ""


# ---------- rust ----------

def crate_pkgs(tree):
    out = {}
    for toml in glob.glob(os.path.join(tree, "crates", "*", "Cargo.toml")):
        m = re.search(r'^name\s*=\s*"([^"]+)"', open(toml).read(), re.M)
        if m:
            out[os.path.basename(os.path.dirname(toml))] = m.group(1)
    return out


# Integration binaries that took minutes of wall time in a full local run
# (2026-10-27, load ~70 on 28 cores: 49 minutes for 4817 tests). The gate
# leaves them to CI unless the change edits the binary itself, and says so.
SLOW_BINARIES = [b for b in os.environ.get(
    "AMUX_LAND_GATE_SLOW_BINARIES",
    "diagnostic_contract,restart_persistence,dead_pub_api,move_bundle,public_links,board_api").split(",") if b]


def rust_filter(files, pkgs):
    """Unit tests of each changed module, and the integration test binaries of
    each changed crate (the tree-walking contract tests that redden main live
    there and take under a second each), except SLOW_BINARIES. A changed test
    file always runs. A change to Cargo files or a build script runs the
    crate's whole unit suite. Returns (filterset, skipped binary names)."""
    clauses, crates, edited = set(), set(), set()
    for f in files:
        m = re.match(r"^crates/([^/]+)/(.*)$", f)
        if not m or m.group(1) not in pkgs:
            continue
        pkg, rest = pkgs[m.group(1)], m.group(2)
        crates.add(pkg)
        sm = re.match(r"^src/(.*)\.rs$", rest)
        tm = re.match(r"^tests/([^/.]+)", rest)
        if sm:
            parts = sm.group(1).split("/")
            if parts[:1] == ["bin"]:
                clauses.add("(package(=%s) & kind(bin))" % pkg)
            elif parts[-1] in ("mod", "lib", "main"):
                # A module root mostly mounts and declares; its own tests run,
                # and the integration contract tests cover what it wires. Its
                # whole subtree would be most of the crate.
                prefix = "::".join(parts[:-1] + ["tests"])
                clauses.add("(package(=%s) & kind(lib) & test(/^%s::/))" % (pkg, prefix))
            else:
                clauses.add("(package(=%s) & kind(lib) & test(/^%s::/))" % (pkg, "::".join(parts)))
        elif tm:
            edited.add(tm.group(1))
            clauses.add("(package(=%s) & binary(=%s))" % (pkg, tm.group(1)))
        elif not rest.endswith(".md"):
            clauses.add("(package(=%s) & kind(lib))" % pkg)
    skipped = sorted(b for b in SLOW_BINARIES if b not in edited)
    not_slow = "".join(" & !binary(=%s)" % b for b in skipped)
    for pkg in crates:
        clauses.add("(package(=%s) & kind(test)%s)" % (pkg, not_slow))
    return " | ".join(sorted(clauses)), (skipped if crates else [])


NEXTEST_FAIL = re.compile(r"^\s+(?:FAIL|SIGSEGV|SIGABRT|TIMEOUT) \[[^\]]*\] (?:\([^)]*\) )?(\S+) (\S+)\s*$", re.M)


def nextest(tree, expr, extra=(), timeout=RUST_TIMEOUT_S):
    env = dict(os.environ, CARGO_TARGET_DIR=os.path.expanduser(
        os.environ.get("AMUX_LAND_GATE_TARGET", "~/.amux/rust-build-target")))
    cmd = ["cargo", "nextest", "run", "--workspace", "--no-fail-fast", "--no-tests=pass", "-E", expr, *extra]
    t0 = time.time()
    try:
        r = sh(cmd, cwd=tree, env=env, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None, "nextest did not finish in %ds" % timeout, time.time() - t0
    text = r.stdout + r.stderr
    fails = sorted({"%s %s" % m for m in NEXTEST_FAIL.findall(text)})
    summary = re.findall(r"^\s+Summary \[.*$", text, re.M)
    if r.returncode != 0 and not fails:
        return None, text[-2500:], time.time() - t0
    if r.returncode == 0 and not summary:
        return None, "nextest printed no Summary line:\n" + text[-1500:], time.time() - t0
    return fails, (summary[-1].strip() if summary else ""), time.time() - t0


def by_test_expr(names):
    return " | ".join("(binary_id(=%s) & test(=%s))" % tuple(n.split(" ", 1)) for n in names)


def rust_leg(repo, tree, files, base_tree_fn):
    pkgs = crate_pkgs(tree)
    expr, skipped = rust_filter(files, pkgs)
    if not expr:
        return {"verdict": "rust_not_affected"}
    fails, detail, secs = nextest(tree, expr)
    out = {"filter": expr, "secs": round(secs)}
    if skipped:
        out["note"] = "not run here, CI runs them: " + ", ".join(skipped)
    if fails is None:
        return dict(out, verdict="rust_unmeasured", detail=detail)
    out["summary"] = detail
    if not fails:
        return dict(out, verdict="rust_pass")
    again, _, _ = nextest(tree, by_test_expr(fails))
    flaky = [f for f in fails if again is not None and f not in again]
    still = [f for f in fails if f not in flaky]
    inherited = []
    if still:
        btree = base_tree_fn()
        was, _, _ = nextest(btree, by_test_expr(still))
        inherited = [f for f in still if was is not None and f in was]
    mine = [f for f in still if f not in inherited]
    return dict(out, verdict="rust_fail" if mine else "rust_pass", failing=mine, flaky=flaky, inherited=inherited)


# ---------- e2e ----------

def affected(tree, base):
    r = sh(["node", "scripts/ci-affected.mjs", "--event", "pull_request", "--base", base, "--head", "HEAD"],
           cwd=tree, timeout=180)
    try:
        return json.loads(r.stdout)
    except ValueError:
        return None


def playwright(tree, specs, timeout=E2E_TIMEOUT_S):
    nm = os.path.join(tree, "node_modules")
    if not os.path.isdir(nm):
        return None, "no node_modules for the composed tree", 0
    rep = tempfile.mktemp(prefix="land-gate-pw-", suffix=".json")
    env = dict(os.environ, AMUX_E2E_SPECS=json.dumps(specs), PLAYWRIGHT_JSON_OUTPUT_NAME=rep,
               AMUX_E2E_PORT_OFFSET=str(random.randrange(30, 90) * 10))
    for k in ("AMUX_SESSION", "AMUX_WORKER", "TMUX", "TMUX_PANE"):
        env.pop(k, None)
    t0 = time.time()
    try:
        r = sh(["npx", "playwright", "test", "-c", "e2e/playwright.config.ts", "--project=desktop",
                "--project=mobile", "--retries=1", "--reporter=json"], cwd=tree, env=env, timeout=timeout)
    except subprocess.TimeoutExpired:
        return None, "playwright did not finish in %ds" % timeout, time.time() - t0
    try:
        data = json.load(open(rep))
    except (OSError, ValueError):
        return None, (r.stderr or r.stdout)[-2500:], time.time() - t0
    finally:
        if os.path.exists(rep):
            os.unlink(rep)
    res = {"expected": 0, "flaky": [], "unexpected": []}

    def walk(suite, path):
        for s in suite.get("suites", []):
            walk(s, path + [s.get("title", "")])
        for sp in suite.get("specs", []):
            for t in sp.get("tests", []):
                name = "[%s] %s > %s" % (t.get("projectName"), sp.get("file"), sp.get("title"))
                st = t.get("status")
                if st == "expected":
                    res["expected"] += 1
                elif st == "flaky":
                    res["flaky"].append(name)
                elif st == "unexpected":
                    res["unexpected"].append(name)
    for s in data.get("suites", []):
        walk(s, [])
    stats = data.get("stats", {})
    if not res["expected"] and not res["unexpected"] and not res["flaky"]:
        return None, "playwright ran no tests: %s" % json.dumps(stats), time.time() - t0
    return res, stats, time.time() - t0


def e2e_leg(repo, tree, base, base_tree_fn):
    if not link_node_modules(repo, tree):
        return {"verdict": "e2e_unmeasured", "detail": "no node_modules to link"}
    sel = affected(tree, base)
    if not sel:
        return {"verdict": "e2e_unmeasured", "detail": "scripts/ci-affected.mjs gave no verdict"}
    e = sel.get("e2e") or {}
    if not e.get("run"):
        return {"verdict": "e2e_not_affected"}
    specs = list(e.get("specs") or [])
    note = ""
    changed = [sp for sp, why in (e.get("selected") or {}).items() if why == "changed spec"]
    changed += [f[len("e2e/"):] for f in git(tree, "diff", "--name-only", base, "HEAD").splitlines()
                if f.startswith("e2e/") and f.endswith(".spec.ts") and os.path.exists(os.path.join(tree, f))]
    changed = list(dict.fromkeys(changed))
    if e.get("full") or not specs:
        specs = list(dict.fromkeys(changed + SMOKE_FALLBACK))
        note = "CI runs every spec for this change; this runs its changed specs and the smoke set"
    elif len(specs) > MAX_LOCAL_SPECS:
        specs = (changed + [s for s in specs if s not in changed])[:max(MAX_LOCAL_SPECS, len(changed))]
        note = "%d specs selected; the first %d run here (changed specs first), CI runs the rest" % (
            len(e.get("specs")), MAX_LOCAL_SPECS)
    res, stats, secs = playwright(tree, specs)
    out = {"specs": specs, "secs": round(secs), "note": note}
    if res is None:
        return dict(out, verdict="e2e_unmeasured", detail=stats)
    out.update(passed=res["expected"], flaky=res["flaky"])
    if not res["unexpected"]:
        return dict(out, verdict="e2e_pass")
    files = sorted({re.sub(r"^\[[^\]]+\] ", "", n).split(" > ")[0] for n in res["unexpected"]})
    btree = base_tree_fn()
    link_node_modules(repo, btree)
    was, _, _ = playwright(btree, files)
    inherited = [n for n in res["unexpected"] if was is not None and n in was["unexpected"]]
    mine = [n for n in res["unexpected"] if n not in inherited]
    return dict(out, verdict="e2e_fail" if mine else "e2e_pass", failing=mine, inherited=inherited)


# ---------- machine pressure ----------

def wait_for_load():
    """The same pressure the resources boundary refuses heavy commands on:
    wait (bounded, said out loud) while load per core is over the limit, then
    run anyway and record that it ran loaded, since a gate that never runs
    holds the lane forever."""
    limit = float(os.environ.get("AMUX_LAND_GATE_MAX_LOAD_PER_CORE", "2.0"))
    budget = int(os.environ.get("AMUX_LAND_GATE_LOAD_WAIT_S", "1200"))
    cores = os.cpu_count() or 1
    t0 = time.time()
    told = False
    while True:
        per = os.getloadavg()[0] / cores
        if per <= limit:
            return {"verdict": "load_ok", "per_core": round(per, 2), "waited_s": round(time.time() - t0)}
        if time.time() - t0 >= budget:
            log("verdict=land_gate_ran_loaded per_core=%.2f waited_s=%d lane=%s" % (per, budget, lane() or "human"))
            return {"verdict": "load_ran_anyway", "per_core": round(per, 2), "waited_s": budget}
        if not told:
            print("land gate: load %.2f per core is over %.1f; waiting up to %ds before building" % (per, limit, budget))
            told = True
        time.sleep(30)


# ---------- receipts ----------

def receipts_dir():
    d = os.path.join(HOME, "receipts")
    os.makedirs(d, exist_ok=True)
    return d


def write_receipt(rec):
    d = receipts_dir()
    for p in glob.glob(os.path.join(d, "*.json")):
        try:
            if time.time() - os.path.getmtime(p) > RECEIPT_TTL_S:
                os.unlink(p)
        except OSError:
            pass
    p = os.path.join(d, rec["patch_id"] + ".json")
    tmp = p + ".tmp.%d" % os.getpid()
    with open(tmp, "w") as f:
        json.dump(rec, f, indent=1)
    os.replace(tmp, p)
    return p


def covered_commits():
    out = {}
    for p in glob.glob(os.path.join(receipts_dir(), "*.json")):
        try:
            rec = json.load(open(p))
        except (OSError, ValueError):
            continue
        if rec.get("pass"):
            for pid in rec.get("commit_patch_ids", []):
                out[pid] = p
    return out


# ---------- the two entry points ----------

def gate(a):
    repo = git(".", "rev-parse", "--show-toplevel").strip()
    sh(["git", "-C", repo, "fetch", "-q", "origin", "main"], timeout=300)
    main = git(repo, "rev-parse", "origin/main").strip()
    head = git(repo, "rev-parse", a.head).strip()
    base = git(repo, "merge-base", main, head).strip()
    if base == head:
        print("land gate: %s is already on origin/main; nothing to gate" % head[:12])
        return 0
    t0 = time.time()
    files = [f for f in git(repo, "diff", "--name-only", base, head).splitlines() if f]
    numstat = git(repo, "diff", "--numstat", base, head)
    lines_changed = sum(int(x) for l in numstat.splitlines() for x in l.split()[:2] if x.isdigit())
    tiers, tiers_src = load_tiers(repo)
    tier = tier_for(tiers, files, lines_changed)
    cpids = commit_patch_ids(repo, base, head)
    whole = patch_id(repo, base, head)
    print("land gate: %d commit(s), %d file(s), %d lines, tier %s (tiers from %s)"
          % (len(cpids), len(files), lines_changed, tier, tiers_src))
    legs = {}
    legs["owner"] = owner_leg(repo, base, head, tiers)
    legs["revert"] = revert_leg(repo, base, head, tiers)
    tree = gate_tree(lane())
    composed = compose(repo, tree, base, head, main)
    base_cache = {}

    def base_tree():
        if "t" not in base_cache:
            bt = gate_tree(lane(), "-base")
            checkout(repo, bt, main)
            base_cache["t"] = bt
        return base_cache["t"]

    legs["load"] = wait_for_load()
    import threading
    review_out = {}

    def do_review():
        if a.no_review:
            review_out["r"] = {"verdict": "review_skipped", "why": a.no_review}
            return
        import land_review
        try:
            r = land_review.run(tree, main, composed, tier, land_review.change_key(repo, base, head),
                                a.model, "origin/main")
            review_out["r"] = r
        except Exception as e:  # the review is a leg, never the gate's crash
            review_out["r"] = {"verdict": "review_unmeasured", "errors": [str(e)], "open": [], "blocking": []}
    th = threading.Thread(target=do_review)
    th.start()
    legs["rust"] = {"verdict": "rust_skipped", "why": a.no_rust} if a.no_rust else rust_leg(repo, tree, files, base_tree)
    legs["e2e"] = {"verdict": "e2e_skipped", "why": a.no_e2e} if a.no_e2e else e2e_leg(repo, tree, main, base_tree)
    th.join()
    legs["review"] = review_out.get("r") or {"verdict": "review_unmeasured"}
    blocking = {"owner_pending", "owner_unmeasured", "revert_missing", "rust_fail", "rust_unmeasured",
                "e2e_fail", "e2e_unmeasured", "review_blocking", "review_unmeasured"}
    bad = [k for k, v in legs.items() if v.get("verdict") in blocking]
    rec = {"patch_id": whole, "commit_patch_ids": sorted(set(cpids.values())), "head": head, "base": base,
           "main": main, "composed": composed, "files": files, "tier": tier, "lane": lane(),
           "at": time.time(), "secs": round(time.time() - t0), "legs": legs, "pass": not bad, "blocked_by": bad}
    path = write_receipt(rec)
    verdict = "land_gate_pass" if not bad else "land_gate_fail"
    log("verdict=%s head=%s patch_id=%s tier=%s blocked_by=%s %s secs=%d lane=%s" % (
        verdict, head[:12], whole[:12], tier, ",".join(bad) or "-",
        " ".join("%s=%s" % (k, v.get("verdict")) for k, v in legs.items()), rec["secs"], lane() or "human"))
    print(render(legs) if not a.json else json.dumps(rec, indent=1))
    print("receipt: %s\nverdict=%s" % (path, verdict))
    return 0 if not bad else 1


def render(legs):
    import land_review
    out = []
    o = legs["owner"]
    if o["verdict"] == "owner_pending":
        req = o.get("request") or {}
        out.append("owner: PENDING. %s change(s) need the owner's approval of this exact diff (patch-id %s).\n"
                   "  Ask filed: %s\n  He approves at: %s" % (
                       ", ".join(o["files"]), o["patch_id"][:12], req.get("card") or req.get("error") or req.get("code"),
                       req.get("link") or "#land-approve=" + o["patch_id"]))
    else:
        out.append("owner: %s%s" % (o["verdict"], " " + str(o.get("detail", "")) if o.get("detail") else ""))
    r = legs["revert"]
    out.append("revert: %s" % r["verdict"])
    for m in r.get("missing", []):
        out.append("  %s %s touches %s and has no `Revert: <how to undo it>` trailer" % (
            m["commit"], m["subject"][:60], ", ".join(m["files"])))
    ld = legs.get("load") or {}
    if ld.get("waited_s"):
        out.append("load: %s, %.2f per core, waited %ds" % (ld["verdict"], ld["per_core"], ld["waited_s"]))
    for k in ("rust", "e2e"):
        v = legs[k]
        out.append("%s: %s %s %s" % (k, v["verdict"], v.get("summary", ""), ("(%ss)" % v["secs"]) if "secs" in v else ""))
        if v.get("note"):
            out.append("  note: " + v["note"])
        for n in v.get("failing", []):
            out.append("  FAIL      " + n)
        for n in v.get("flaky", []):
            out.append("  flaky     " + n + " (passed on rerun, does not block)")
        for n in v.get("inherited", []):
            out.append("  inherited " + n + " (also fails on origin/main, does not block)")
        if v.get("detail") and v["verdict"].endswith("unmeasured"):
            out.append("  " + str(v["detail"])[-1500:])
    rv = legs["review"]
    if "round" in rv:
        out.append(land_review.render(rv))
    else:
        out.append("review: %s %s" % (rv["verdict"], " ".join(rv.get("errors", [])) or rv.get("why", "")))
    return "\n".join(out)


def pre_push(a):
    """What the pre-push hook runs, from origin/main's copy of this file. Cheap:
    no build. Returns 1 to refuse the push."""
    repo = git(".", "rev-parse", "--show-toplevel").strip()
    local, remote = a.local, a.remote
    base = remote if remote and remote.strip("0") else git(repo, "merge-base", "origin/main", local, check=False).strip()
    if not base:
        return 0
    files = [f for f in git(repo, "diff", "--name-only", base, local).splitlines() if f]
    if not files:
        return 0
    tiers, _ = load_tiers(repo)
    agent = bool(lane())
    rc = 0
    o = owner_leg(repo, base, local, tiers, request=agent)
    if o["verdict"] in ("owner_pending", "owner_unmeasured") and agent:
        sys.stderr.write("\nBLOCKED: this push changes %s, which needs the owner's approval of this exact "
                         "diff (patch-id %s). %s\nAsk filed: %s. He approves at %s in the dashboard.\n" % (
                             ", ".join(o["files"]), o["patch_id"][:12], o.get("detail", ""),
                             (o.get("request") or {}).get("card") or "(could not file: %s)" % (o.get("request") or {}).get("error"),
                             (o.get("request") or {}).get("link") or "#land-approve=" + o["patch_id"]))
        log("verdict=pre_push_owner_refused patch_id=%s lane=%s" % (o["patch_id"][:12], lane()))
        rc = 1
    r = revert_leg(repo, base, local, tiers)
    if r["missing"] and agent:
        sys.stderr.write("\nBLOCKED: these commits change a workflow, installer, guard or schema and do not say "
                         "how to undo it:\n")
        for m in r["missing"]:
            sys.stderr.write("  %s %s (%s)\n" % (m["commit"], m["subject"][:70], ", ".join(m["files"])))
        sys.stderr.write("Add a trailer like `Revert: git revert, then run scripts/install-hooks.sh` "
                         "(git commit --amend, or a fixup and rebase).\n")
        log("verdict=pre_push_revert_refused n=%d lane=%s" % (len(r["missing"]), lane()))
        rc = 1
    if not agent or os.environ.get("AMUX_LAND_GATE_REQUIRED", "1").strip().lower() in ("0", "false", "off", "no"):
        return rc
    cpids = commit_patch_ids(repo, base, local)
    cov = covered_commits()
    uncovered = [c for c, pid in cpids.items() if pid not in cov]
    if not uncovered:
        log("verdict=pre_push_receipt_ok commits=%d lane=%s" % (len(cpids), lane()))
        return rc
    skip = os.environ.get("AMUX_SKIP_LAND_GATE", "").strip().lower() in ("1", "true", "yes", "on")
    reason = os.environ.get("AMUX_SKIP_LAND_GATE_REASON", "").strip()
    if skip and reason:
        log("verdict=pre_push_receipt_skipped commits=%d uncovered=%d lane=%s reason=%s" % (
            len(cpids), len(uncovered), lane(), json.dumps(reason)))
        sys.stderr.write("amux pre-push: land gate skipped for %d commit(s): %s verdict=pre_push_receipt_skipped\n"
                         % (len(uncovered), reason))
        return rc
    sys.stderr.write("\nBLOCKED: %d of %d commit(s) in this push have no passing land-gate receipt:\n" % (
        len(uncovered), len(cpids)))
    for c in uncovered[:10]:
        sys.stderr.write("  %s %s\n" % (c[:12], git(repo, "show", "-s", "--format=%s", c).strip()[:70]))
    sys.stderr.write("\nRun the checks main runs, on origin/main plus your change, before it lands:\n"
                     "  scripts/land_gate.py\n"
                     "then push again. A rebase that applies cleanly keeps the receipt.\n"
                     "Skip once, logged: AMUX_SKIP_LAND_GATE=1 AMUX_SKIP_LAND_GATE_REASON=\"<why>\" git push ...\n")
    log("verdict=pre_push_receipt_missing commits=%d uncovered=%d lane=%s" % (len(cpids), len(uncovered), lane()))
    return 1


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--head", default="HEAD")
    ap.add_argument("--no-review", metavar="REASON")
    ap.add_argument("--no-rust", metavar="REASON")
    ap.add_argument("--no-e2e", metavar="REASON")
    ap.add_argument("--model", default=os.environ.get("AMUX_LAND_REVIEW_MODEL", ""))
    ap.add_argument("--resolve", nargs=2, metavar=("ID", "REASON"))
    ap.add_argument("--pre-push", action="store_true")
    ap.add_argument("--local")
    ap.add_argument("--remote")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    if a.pre_push:
        return pre_push(a)
    if a.resolve:
        import land_review
        repo = git(".", "rev-parse", "--show-toplevel").strip()
        head = git(repo, "rev-parse", a.head).strip()
        base = git(repo, "merge-base", "origin/main", head).strip()
        land_review.resolve(land_review.change_key(repo, base, head), *a.resolve)
        print("resolved %s; run scripts/land_gate.py again" % a.resolve[0])
        return 0
    try:
        return gate(a)
    except RuntimeError as e:
        print("land gate: %s\nverdict=land_gate_error" % e)
        log("verdict=land_gate_error error=%s lane=%s" % (json.dumps(str(e)[:300]), lane() or "human"))
        return 2


if __name__ == "__main__":
    sys.exit(main())
