#!/usr/bin/env python3
"""Daily vulnerability watch for this host and its repos (Ethan, 2026-10-26).

Runs as a `kind: shell` schedule, so a quiet day costs no model call. It
collects findings from sources that can be checked mechanically, keeps the
ones it has not reported before, and sends only those to the `security`
worker, which researches each one and lands the patch.

Sources:
  - CISA Known Exploited Vulnerabilities, matched against INVENTORY (the
    software this box runs).
  - Tailscale security bulletins, plus the installed client against the
    newest stable release.
  - cargo audit, npm audit and pip-audit over every lockfile under
    SECURITY_WATCH_REPOS, read from each repo's origin/main. The checkouts
    under ~/Dev are shared and can sit far behind main (mixpeek was 1660
    commits behind on 2026-10-26), so auditing the working tree reports
    advisories main already fixed and misses ones main introduced.
  - macOS software updates.

Every source reports whether it ran. A source that failed is listed as
unmeasured, never as clean, and the run's summary counts both.

Output: ~/.amux/security-watch/<UTC date>.json, and state.json with the ids
already reported. --dry-run prints the findings and sends nothing.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor, wait
from functools import lru_cache
from pathlib import Path

HOME = Path.home()
OUT = HOME / ".amux" / "security-watch"
STATE = OUT / "state.json"
WORKER = os.environ.get("SECURITY_WATCH_WORKER", "security")
REPOS = [Path(p).expanduser() for p in os.environ.get(
    "SECURITY_WATCH_REPOS", "~/Dev/amux:~/Dev/mixpeek:~/Dev/procwarden:~/Dev/amux-GTM").split(":") if p]
KEV_URL = "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json"
KEV_DAYS = int(os.environ.get("SECURITY_WATCH_KEV_DAYS", "14"))
# vendorProject or product substrings (lowercase) for software this box runs.
INVENTORY = [s.strip().lower() for s in os.environ.get("SECURITY_WATCH_INVENTORY", (
    "apple,macos,safari,webkit,chrome,chromium,google,tailscale,openssh,git,node.js,nodejs,"
    "python,golang,go,rust,docker,kubernetes,sqlite,openssl,curl,tmux,bash,homebrew,"
    "github,npm,playwright,visual studio code,microsoft vscode,cloudflare,postgres,redis,nginx"
)).split(",") if s.strip()]
UA = {"User-Agent": "amux-security-watch"}
# A shell schedule is killed at 600s (scheduler.rs SHELL_TIMEOUT_S), which
# would lose the whole report. Every check shares this budget; one still
# running at the deadline is reported unmeasured and the rest still go out.
BUDGET_S = int(os.environ.get("SECURITY_WATCH_BUDGET_S", "480"))
DEADLINE = time.time() + BUDGET_S


def run(cmd, cwd=None, timeout=600):
    left = DEADLINE - time.time()
    if left <= 5:
        raise TimeoutError("budget spent before this command started")
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=min(timeout, left))
    return p.returncode, p.stdout, p.stderr


def each_file(files, fn):
    """Run fn over files four at a time; collect findings and per-file errors."""
    out, errors = [], []
    with ThreadPoolExecutor(4) as pool:
        for found, err in pool.map(fn, files):
            out += found
            if err:
                errors.append(err)
    return out, errors


def fetch(url, timeout=30):
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=timeout) as r:
        return r.read().decode("utf-8", "replace")


def src_kev():
    data = json.loads(fetch(KEV_URL, 60))
    cutoff = time.strftime("%Y-%m-%d", time.gmtime(time.time() - KEV_DAYS * 86400))
    vulns = data.get("vulnerabilities", [])
    out = []
    for v in vulns:
        if v.get("dateAdded", "") < cutoff:
            continue
        hay = (v.get("vendorProject", "") + " " + v.get("product", "")).lower()
        words = set(re.findall(r"[a-z0-9.]+", hay))
        if not any((i in words) if " " not in i else (i in hay) for i in INVENTORY):
            continue
        out.append({"id": "kev:" + v["cveID"], "source": "cisa-kev", "severity": "exploited",
                    "title": f"{v['cveID']} {v.get('vendorProject')} {v.get('product')}: {v.get('vulnerabilityName')}",
                    "added": v.get("dateAdded"), "action": v.get("requiredAction")})
    return out, len(vulns)


def src_tailscale():
    out = []
    html = fetch("https://tailscale.com/security-bulletins")
    ids = sorted(set(re.findall(r"TS-20\d\d-\d{3}", html)))
    year = time.strftime("%Y")
    for i in ids:
        if i.startswith(f"TS-{year}"):
            out.append({"id": "ts:" + i, "source": "tailscale-bulletin", "severity": "advisory",
                        "title": f"Tailscale bulletin {i}: check whether it applies here",
                        "action": "https://tailscale.com/security-bulletins#" + i.lower()})
    tsbin = shutil.which("tailscale")
    if tsbin:
        _, ver, _ = run([tsbin, "version"], timeout=30)
        installed = (ver.split() or [""])[0]
        listing = fetch("https://pkgs.tailscale.com/stable/")
        latest = max(re.findall(r"Tailscale-([0-9.]+)-macos\.pkg", listing) or ["0"],
                     key=lambda s: [int(x) for x in s.split(".")])
        if installed and [int(x) for x in installed.split(".")] < [int(x) for x in latest.split(".")]:
            out.append({"id": f"ts-client:{latest}", "source": "tailscale-client", "severity": "update",
                        "title": f"Tailscale {installed} installed, {latest} is the newest stable release",
                        "action": "Install https://pkgs.tailscale.com/stable/Tailscale-%s-macos.pkg (needs the owner's admin password)" % latest})
    return out, len(ids)


@lru_cache(maxsize=None)
def committed(repo):
    """origin/main of `repo` (fetched first), or HEAD when there is no remote."""
    run(["git", "fetch", "-q", "origin", "main"], cwd=repo, timeout=90)
    for ref in ("origin/main", "HEAD"):
        rc, so, _ = run(["git", "rev-parse", "--verify", "-q", ref + "^{commit}"], cwd=repo, timeout=30)
        if rc == 0:
            return ref, so.strip()[:12]
    raise RuntimeError("no origin/main or HEAD commit")


def tracked(repo, pattern):
    ref, _ = committed(repo)
    _, so, _ = run(["git", "ls-tree", "-r", "--name-only", ref], cwd=repo, timeout=60)
    return [l for l in so.splitlines() if re.search(pattern, l) and "node_modules/" not in l]


def materialize(repo, rels, dest):
    """Write the committed bytes of `rels` under `dest`, keeping their paths."""
    ref, _ = committed(repo)
    for rel in rels:
        p = subprocess.run(["git", "show", f"{ref}:{rel}"], cwd=repo, capture_output=True, timeout=60)
        if p.returncode == 0:
            (dest / rel).parent.mkdir(parents=True, exist_ok=True)
            (dest / rel).write_bytes(p.stdout)


def src_cargo(repo):
    if not shutil.which("cargo-audit") and not (HOME / ".cargo/bin/cargo-audit").exists():
        raise RuntimeError("cargo-audit is not installed")
    out, sha = [], committed(repo)[1]
    td = tempfile.TemporaryDirectory(prefix="secwatch-")
    materialize(repo, ["Cargo.lock"], Path(td.name))
    locks = [p for p in [Path(td.name) / "Cargo.lock"] if p.exists()]
    if not locks:
        raise RuntimeError("no Cargo.lock on origin/main")
    for lock in locks:
        _, so, se = run(["cargo", "audit", "--json", "-f", str(lock)], cwd=td.name)
        d = json.loads(so or "{}")
        if "vulnerabilities" not in d:
            raise RuntimeError("cargo audit gave no report: " + se[-200:])
        for x in d["vulnerabilities"]["list"]:
            a = x["advisory"]
            out.append({"id": f"rustsec:{a['id']}:{repo.name}:{x['package']['name']}@{x['package']['version']}",
                        "source": "cargo-audit", "severity": "advisory", "repo": str(repo), "commit": sha,
                        "title": f"{a['id']} {x['package']['name']} {x['package']['version']}: {a['title']}",
                        "action": "patched in " + ", ".join(x["versions"]["patched"] or ["no release"])})
    return out, len(locks)


def src_npm(repo):
    files = tracked(repo, r"(^|/)package-lock\.json$")
    sha = committed(repo)[1]
    td = tempfile.TemporaryDirectory(prefix="secwatch-")
    root = Path(td.name)
    materialize(repo, files + [str(Path(l).parent / "package.json") for l in files], root)

    def one(rel):
        try:
            cwd = root / Path(rel).parent
            _, j, se = run(["npm", "audit", "--json", "--package-lock-only"], cwd=cwd, timeout=300)
            rep = json.loads(j or "{}")
            # Reachability: what is still flagged with devDependencies left out
            # ships or runs in production; the rest is build and test tooling.
            _, j2, _ = run(["npm", "audit", "--json", "--package-lock-only", "--omit=dev"], cwd=cwd, timeout=300)
            prod = json.loads(j2 or "{}").get("vulnerabilities")
        except Exception as e:
            return [], f"{rel}: {e}"[:200]
        if "vulnerabilities" not in rep:
            return [], f"{rel}: {se.strip()[-160:]}"
        found = []
        for name, v in rep["vulnerabilities"].items():
            if v.get("severity") not in ("high", "critical"):
                continue
            via = [x.get("title") for x in v.get("via", []) if isinstance(x, dict)]
            scope = "unknown" if prod is None else ("runtime" if name in prod else "dev-only")
            found.append({"id": f"npm:{repo.name}/{rel}:{name}:{v.get('range')}", "source": "npm-audit",
                          "severity": v["severity"], "repo": str(repo), "commit": sha, "scope": scope,
                          "title": f"{name} ({v['severity']}, {scope}) in {repo.name}/{rel}: {'; '.join(via)[:160]}",
                          "action": "fix available" if v.get("fixAvailable") else "no fix yet"})
        return found, None

    out, errors = each_file(files, one)
    return out, len(files), errors


def src_pip(repo):
    pa = shutil.which("pip-audit") or str(HOME / ".local/bin/pip-audit")
    if not Path(pa).exists():
        raise RuntimeError("pip-audit is not installed")
    files = tracked(repo, r"requirements[^/]*\.txt$")[:40]
    sha = committed(repo)[1]
    td = tempfile.TemporaryDirectory(prefix="secwatch-")
    # Every requirements/constraints file, so `-r other.txt` includes resolve.
    materialize(repo, tracked(repo, r"(requirements|constraints)[^/]*\.txt$"), Path(td.name))

    def one(rel):
        try:
            _, j, se = run([pa, "-r", rel, "-f", "json", "--progress-spinner", "off"], cwd=td.name, timeout=600)
            rep = json.loads(j)
        except Exception as e:
            return [], f"{rel}: {e}"[:200]
        found = []
        for dep in rep.get("dependencies", []):
            for v in dep.get("vulns", []):
                found.append({"id": f"pip:{repo.name}/{rel}:{v['id']}:{dep['name']}", "source": "pip-audit",
                              "severity": "advisory", "repo": str(repo), "commit": sha,
                              "title": f"{v['id']} {dep['name']} {dep.get('version')} in {repo.name}/{rel}",
                              "action": "fixed in " + ", ".join(v.get("fix_versions") or ["no release"])})
        return found, None

    out, errors = each_file(files, one)
    return out, len(files), errors


def src_macos():
    _, so, se = run(["/usr/sbin/softwareupdate", "-l"], timeout=300)
    text = so + se
    if "Software Update" not in text:
        raise RuntimeError("softwareupdate gave no listing")
    out = []
    for label, title in re.findall(r"\* Label: (.+)\n\s*Title: ([^,]+)", text):
        out.append({"id": "macos:" + label.strip(), "source": "softwareupdate", "severity": "update",
                    "title": f"macOS update available: {title.strip()}",
                    "action": "Owner installs it (needs a restart)"})
    return out, len(out)


def main():
    dry = "--dry-run" in sys.argv
    OUT.mkdir(parents=True, exist_ok=True)
    state = json.loads(STATE.read_text()) if STATE.exists() else {"reported": {}}
    sources = [("cisa-kev", src_kev), ("tailscale", src_tailscale), ("macos", src_macos)]
    for repo in REPOS:
        if (repo / ".git").exists():
            sources += [(f"cargo:{repo.name}", lambda r=repo: src_cargo(r)) if (repo / "Cargo.lock").exists() else None,
                        (f"npm:{repo.name}", lambda r=repo: src_npm(r)),
                        (f"pip:{repo.name}", lambda r=repo: src_pip(r))]
    report = {"ts": int(time.time()), "sources": {}, "findings": [], "budget_s": BUDGET_S}
    for repo in REPOS:  # fetch each repo once, before its three sources run in parallel
        if (repo / ".git").exists():
            try:
                committed(repo)
            except Exception:
                pass  # each source raises it again and is reported unmeasured

    def measure(fn):
        res = fn()
        return res if len(res) == 3 else (*res, [])

    pool = ThreadPoolExecutor(8)
    futs = {name: pool.submit(measure, fn) for name, fn in filter(None, sources)}
    wait(list(futs.values()), timeout=max(1, DEADLINE - time.time() + 10))
    for name, fut in futs.items():
        if not fut.done():
            report["sources"][name] = {"measured": False, "why_unmeasured": f"still running at the {BUDGET_S}s budget"}
            continue
        try:
            found, considered, errors = fut.result()
            # Every file failing is a source that did not run, not a clean one.
            if considered and len(errors) >= considered:
                raise RuntimeError(f"all {considered} file(s) failed, first: {errors[0]}")
            report["sources"][name] = {"measured": True, "n_considered": considered, "findings": len(found),
                                       "files_failed": len(errors), "errors": errors[:10]}
            report["findings"] += found
        except Exception as e:  # a failed source is unmeasured, never clean
            report["sources"][name] = {"measured": False, "why_unmeasured": str(e)[:300]}
    pool.shutdown(wait=False, cancel_futures=True)
    # pip-audit can list one advisory twice for a package (2026-10-26:
    # PYSEC-2026-3552 on cryptography); a finding is its id.
    report["findings"] = list({f["id"]: f for f in report["findings"]}.values())
    new = [f for f in report["findings"] if f["id"] not in state["reported"]]
    unmeasured = [n for n, s in report["sources"].items() if not s["measured"]]
    report["new"] = len(new)
    report["unmeasured"] = unmeasured
    (OUT / time.strftime("%Y-%m-%d.json", time.gmtime())).write_text(json.dumps(report, indent=1))
    summary = (f"security-watch: {len(report['findings'])} finding(s), {len(new)} new, "
               f"{len(report['sources']) - len(unmeasured)} of {len(report['sources'])} source(s) measured")
    print(summary)
    for n in unmeasured:
        print(f"  UNMEASURED {n}: {report['sources'][n]['why_unmeasured']}")
    for n, src in report["sources"].items():
        if src.get("files_failed"):
            print(f"  PARTIAL {n}: {src['files_failed']} of {src['n_considered']} file(s) failed")
    if dry:
        for f in new:
            print("  NEW", f["title"])
        return 0
    if new or unmeasured:
        lines = [f"Ask: triage these and land patches; file a needsyou card for anything that needs Ethan (installs that need his password, paid tiers). {summary}.", ""]
        rank = {"exploited": 0, "critical": 1, "update": 2, "high": 3, "advisory": 4}
        new.sort(key=lambda f: (f.get("scope") == "dev-only", rank.get(f["severity"], 9)))
        by_where = {}
        for f in new:
            k = (f["source"], Path(f.get("repo", "")).name or "host")
            by_where[k] = by_where.get(k, 0) + 1
        dev = sum(1 for f in new if f.get("scope") == "dev-only")
        if dev:
            lines.append(f"{dev} of the {len(new)} new are npm dev-only (build and test tooling), listed last.")
        lines.append("New, by source and repo: " + "; ".join(f"{s}/{r} {n}" for (s, r), n in sorted(by_where.items(), key=lambda x: -x[1])))
        lines.append("")
        lines += [f"- [{f['severity']}] {f['title']} -> {f.get('action') or ''}" for f in new[:40]]
        if len(new) > 40:
            lines.append(f"- ... {len(new) - 40} more in the full report")
        lines += [f"- UNMEASURED {n}: {report['sources'][n]['why_unmeasured']}" for n in unmeasured]
        lines.append(f"\nFull report: {OUT / time.strftime('%Y-%m-%d.json', time.gmtime())}")
        p = subprocess.run(["amux", "send", WORKER, "--stdin"], input="\n".join(lines), text=True,
                           capture_output=True, timeout=60)
        if p.returncode != 0:
            print("send to", WORKER, "failed:", (p.stderr or p.stdout)[-300:], file=sys.stderr)
            return 1
    for f in new:
        state["reported"][f["id"]] = report["ts"]
    STATE.write_text(json.dumps(state, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
