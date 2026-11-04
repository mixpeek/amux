#!/usr/bin/env python3
"""Advisory watch: new advisories against the exact versions amux locks.

Runs as a `kind: shell` schedule every 6 h, so a quiet run costs no model call.
It is the intake of docs/security/RED-TEAM-LOOP.md. Reads committed bytes
(origin/main of each repo), never a working tree: the shared checkouts sit far
behind main.

It messages the amux-security lane about an alert key it has not seen when:
  - an advisory is a malicious-package record, on ANY lockfile, dev tooling too;
  - an advisory is in CISA KEV, or rated CRITICAL or HIGH, on a SHIPPED package;
  - an advisory on a shipped package has no usable rating (it cannot be ruled
    out as high, so it is reported once rather than dropped);
  - a new KEV entry names a shipped package or a component in INVENTORY by
    product name, because KEV lists products and OSV may not map them.

The alert key includes the tag, so an advisory already seen as HIGH alerts again
when it later enters KEV, and a dev-only advisory alerts when it becomes shipped.

Fail closed. Exit codes:
  0  the check ran; anything new was delivered
  2  the check could not run: a source, a repo fetch, or a lockfile it cannot
     read. Nothing is reported as clean and state is not advanced.
  3  the check ran and found something new, but the message was not delivered.
     State is not advanced, so the next run tries again.

The scheduler sends a non-zero exit to the owning worker as a failure notice.

Advisory text is attacker-influenced and this message lands in an agent's turn,
so only ids, severity, package names and paths are sent. Summaries are not.

--dry-run prints what it would send and records nothing.
"""

from __future__ import annotations

import json
import math
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

REPOS = [
    Path(p).expanduser()
    for p in os.environ.get("ADVISORY_WATCH_REPOS", "~/Dev/amux:~/Dev/amux-GTM").split(":")
    if p
]
# A repo whose listed lockfile vanishes must not read as a smaller clean run.
REQUIRED = [
    r
    for r in os.environ.get(
        "ADVISORY_WATCH_REQUIRED", "amux:Cargo.lock,amux:package-lock.json"
    ).split(",")
    if r
]
STATE = Path(
    os.environ.get(
        "ADVISORY_WATCH_STATE", Path.home() / ".amux/tmp/amux-security/advisory-watch"
    )
)
SEEN = STATE / "seen.json"
WORKER = os.environ.get("ADVISORY_WATCH_WORKER", "amux-security")
OSV = os.environ.get("ADVISORY_WATCH_OSV", "https://api.osv.dev/v1")
KEV = os.environ.get(
    "ADVISORY_WATCH_KEV",
    "https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json",
)
KEV_MIN_ROWS = int(os.environ.get("ADVISORY_WATCH_KEV_MIN_ROWS", "1000"))
ATTEMPTS = int(os.environ.get("ADVISORY_WATCH_ATTEMPTS", "3"))
UA = {"User-Agent": "amux-advisory-watch"}

LOCK = re.compile(r"(^|/)(package-lock\.json|Cargo\.lock|Gemfile\.lock|requirements[^/]*\.txt)$")
# Lockfiles this script cannot read. Finding one fails the run, because skipping
# it would report a tree as checked when part of it was not.
UNREAD = re.compile(
    r"(^|/)(poetry\.lock|uv\.lock|Pipfile\.lock|pnpm-lock\.yaml|yarn\.lock|bun\.lockb?|go\.sum|gradle\.lockfile|Package\.resolved|Podfile\.lock)$"
)
# ios/Gemfile.lock is fastlane: build tooling, so a malware record still alerts.
NOT_SHIPPED = re.compile(r"(^|/)(e2e|tests?|fixtures|testdata|site|research)/|(^|/)Gemfile\.lock$")
# Components amux runs that no lockfile names (Dockerfiles, cloud VMs, install.sh).
# Matched against the whole KEV product name, lowercased. The `security` lane's
# daily watch covers macOS, Safari and the rest of this host's software.
INVENTORY = {
    s.strip().lower()
    for s in os.environ.get(
        "ADVISORY_WATCH_INVENTORY",
        "tmux,chromium v8,chromium,chrome,tailscale,git,sqlite,openssl,node.js,nginx,"
        "caddy,litestream,cloudflared,docker,playwright,openssh,curl,containerd,runc",
    ).split(",")
    if s.strip()
}
# Crate and npm names that equal a product name by accident.
NAME_COLLISIONS = {"windows", "kernel", "server", "core", "http", "time", "log", "url"}
MALWARE_WORDS = re.compile(r"\b(malicious|malware|backdoor(ed)?|typosquat\w*)\b", re.I)
SAFE = re.compile(r"[^ A-Za-z0-9@/_.+:,\-]")


class CouldNotRun(Exception):
    pass


def clean(text: str) -> str:
    return SAFE.sub("?", str(text))[:120]


def git(repo: Path, *args: str) -> str:
    p = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, timeout=180
    )
    if p.returncode != 0:
        raise CouldNotRun(f"git {args[0]} in {repo}: {p.stderr.strip()[:200]}")
    return p.stdout


def packages(path: str, text: str) -> tuple[list[tuple[tuple[str, str, str], bool]], list[str]]:
    """Locked packages as ((ecosystem, name, version), is_dev), and unpinned names."""
    out: list[tuple[tuple[str, str, str], bool]] = []
    unpinned: list[str] = []
    if path.endswith("package-lock.json"):
        doc = json.loads(text)
        rows = doc.get("packages")
        if not isinstance(rows, dict):
            raise ValueError("no `packages` map (lockfileVersion 1 is not supported)")
        for key, meta in rows.items():
            if key and "version" in meta and not meta.get("link"):
                name = meta.get("name") or key.split("node_modules/")[-1]
                out.append((("npm", name, meta["version"]), bool(meta.get("dev"))))
    elif path.endswith("Cargo.lock"):
        # The lockfile does not separate dev-dependencies, so all of it counts as shipped.
        for name, version in re.findall(
            r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', text
        ):
            out.append((("crates.io", name, version), False))
    elif path.endswith("Gemfile.lock"):
        for name, version in re.findall(r"^    ([A-Za-z0-9_.\-]+) \(([^)\s]+)\)$", text, re.M):
            out.append((("RubyGems", name, version), False))
    else:
        for raw in text.splitlines():
            line = raw.split("#")[0].strip()
            if not line or line.startswith("-"):
                continue
            m = re.match(r"^([A-Za-z0-9_.\-]+)(?:\[[^\]]*\])?==([^\s;,]+)$", line.split(";")[0].strip())
            if m:
                out.append((("PyPI", m.group(1).lower(), m.group(2)), False))
            else:
                unpinned.append(re.split(r"[<>=!~;\[ ]", line)[0])
    return out, unpinned


def _with_retry(fn):
    last: Exception | None = None
    for attempt in range(ATTEMPTS):
        try:
            return fn()
        except urllib.error.HTTPError as exc:
            last = exc
            if exc.code < 500 and exc.code != 429:
                break
        except (urllib.error.URLError, TimeoutError, ValueError, OSError) as exc:
            last = exc
        if attempt + 1 < ATTEMPTS:
            time.sleep(2 * (attempt + 1))
    raise CouldNotRun(f"{type(last).__name__}: {last}")


def get(url: str) -> dict:
    def go():
        req = urllib.request.Request(url, headers=UA)
        with urllib.request.urlopen(req, timeout=60) as r:
            return json.load(r)

    return _with_retry(go)


def post(url: str, body: dict) -> dict:
    def go():
        req = urllib.request.Request(
            url, json.dumps(body).encode(), {"Content-Type": "application/json", **UA}
        )
        with urllib.request.urlopen(req, timeout=120) as r:
            return json.load(r)

    return _with_retry(go)


def query(pkgs: list[tuple[str, str, str]]) -> dict[str, set[tuple[str, str, str]]]:
    """Advisory id -> packages it hits. Follows OSV's per-query next_page_token."""
    hits: dict[str, set[tuple[str, str, str]]] = {}
    pending: list[tuple[tuple[str, str, str], str | None]] = [(p, None) for p in pkgs]
    while pending:
        chunk, pending = pending[:1000], pending[1000:]
        queries = []
        for (e, n, v), token in chunk:
            q: dict = {"package": {"ecosystem": e, "name": n}, "version": v}
            if token:
                q["page_token"] = token
            queries.append(q)
        res = post(f"{OSV}/querybatch", {"queries": queries}).get("results")
        if not isinstance(res, list) or len(res) != len(chunk):
            raise CouldNotRun(f"OSV answered {len(res or [])} results for {len(chunk)} queries")
        for (pkg, _), r in zip(chunk, res):
            for v in (r or {}).get("vulns") or []:
                hits.setdefault(v["id"], set()).add(pkg)
            token = (r or {}).get("next_page_token")
            if token:
                pending.append((pkg, token))
    return hits


def cvss3_score(vector: str) -> float | None:
    """Base score of a CVSS 3.x vector, or None when it cannot be read."""
    try:
        m = dict(part.split(":") for part in vector.split("/")[1:])
        changed = m["S"] == "C"
        av = {"N": 0.85, "A": 0.62, "L": 0.55, "P": 0.2}[m["AV"]]
        ac = {"L": 0.77, "H": 0.44}[m["AC"]]
        pr = {"N": 0.85, "L": 0.68 if changed else 0.62, "H": 0.5 if changed else 0.27}[m["PR"]]
        ui = {"N": 0.85, "R": 0.62}[m["UI"]]
        cia = {"H": 0.56, "L": 0.22, "N": 0.0}
        iss = 1 - (1 - cia[m["C"]]) * (1 - cia[m["I"]]) * (1 - cia[m["A"]])
    except (KeyError, ValueError):
        return None
    impact = 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02) ** 15 if changed else 6.42 * iss
    if impact <= 0:
        return 0.0
    raw = impact + 8.22 * av * ac * pr * ui
    return math.ceil(min(raw * (1.08 if changed else 1), 10) * 10 - 1e-9) / 10


def rating(vuln: dict) -> str:
    """CRITICAL, HIGH, MODERATE, LOW, or "" when the record carries no usable rating."""
    for src in [vuln.get("database_specific") or {}] + [
        a.get("database_specific") or {} for a in vuln.get("affected") or []
    ]:
        sev = str(src.get("severity") or "").upper()
        if sev:
            return "MODERATE" if sev == "MEDIUM" else sev
    for s in vuln.get("severity") or []:
        if str(s.get("type", "")).startswith("CVSS_V3"):
            score = cvss3_score(str(s.get("score", "")))
            if score is not None:
                if score >= 9:
                    return "CRITICAL"
                if score >= 7:
                    return "HIGH"
                return "MODERATE" if score >= 4 else "LOW"
    return ""


def is_informational(vuln: dict) -> bool:
    for a in vuln.get("affected") or []:
        if (a.get("database_specific") or {}).get("informational"):
            return True
    return bool((vuln.get("database_specific") or {}).get("informational"))


def is_malware(vid: str, vuln: dict) -> bool:
    ids = {vid, *(vuln.get("aliases") or [])}
    return any(i.startswith("MAL-") for i in ids) or bool(
        MALWARE_WORDS.search(str(vuln.get("summary") or ""))
    )


def collect() -> dict:
    where: dict[tuple[str, str, str], dict[str, bool]] = {}
    unpinned: dict[str, list[str]] = {}
    files = 0
    for repo in REPOS:
        git(repo, "fetch", "-q", "origin", "main")
        tree = git(repo, "ls-tree", "-r", "--name-only", "origin/main").splitlines()
        tree = [f for f in tree if "node_modules/" not in f]
        for need in REQUIRED:
            rname, _, rpath = need.partition(":")
            if rname == repo.name and rpath not in tree:
                raise CouldNotRun(f"required lockfile {need} is gone from origin/main")
        unread = [f for f in tree if UNREAD.search(f)]
        if unread:
            raise CouldNotRun(f"{repo.name}: cannot read lockfile format of {unread[:5]}")
        for f in filter(LOCK.search, tree):
            try:
                found, loose = packages(f, git(repo, "show", f"origin/main:{f}"))
            except (ValueError, KeyError, AttributeError) as exc:
                raise CouldNotRun(f"{repo.name}:{f} did not parse: {exc}") from exc
            if loose:
                unpinned[f"{repo.name}:{f}"] = loose
            if not found and not loose:
                raise CouldNotRun(f"{repo.name}:{f} parsed to zero packages")
            files += 1
            label = f"{repo.name}:{f}"
            for pkg, dev in found:
                where.setdefault(pkg, {})[label] = not dev and not NOT_SHIPPED.search(f)
    if not where:
        raise CouldNotRun("zero locked packages across all repos")
    pkgs = sorted(where)
    hits = query(pkgs)
    with ThreadPoolExecutor(8) as pool:
        details = dict(zip(sorted(hits), pool.map(lambda v: get(f"{OSV}/vulns/{v}"), sorted(hits))))
    kev_rows = get(KEV).get("vulnerabilities")
    if not isinstance(kev_rows, list) or len(kev_rows) < KEV_MIN_ROWS:
        raise CouldNotRun(f"KEV feed returned {len(kev_rows or [])} rows, floor {KEV_MIN_ROWS}")
    return {
        "where": where,
        "pkgs": pkgs,
        "hits": hits,
        "details": details,
        "kev": kev_rows,
        "unpinned": unpinned,
        "files": files,
    }


def evaluate(data: dict) -> list[dict]:
    """Every currently alertable condition, one row per alert key."""
    where, hits, details = data["where"], data["hits"], data["details"]
    kev_ids = {v.get("cveID") for v in data["kev"]}
    # An advisory and its aliases (a RUSTSEC record and its rated GHSA) are one finding.
    groups: dict[str, set[str]] = {}
    for vid in hits:
        linked = {vid} | (set(details[vid].get("aliases") or []) & set(hits))
        groups.setdefault(min(linked), set()).update(linked)
    rows: list[dict] = []
    for canon, members in sorted(groups.items()):
        live = {m for m in members if not details[m].get("withdrawn")}
        if not live:
            continue
        pkgs = {p for m in live for p in hits[m]}
        shipped = sorted({f for p in pkgs for f, s in where[p].items() if s})
        anywhere = sorted({f for p in pkgs for f in where[p]})
        aliases = set(live)
        for m in live:
            aliases |= set(details[m].get("aliases") or [])
        ratings = [rating(details[m]) for m in live]
        best = next((r for r in ("CRITICAL", "HIGH") if r in ratings), "")
        unrated = not any(ratings) and not all(is_informational(details[m]) for m in live)
        tag = ""
        if any(is_malware(m, details[m]) for m in live):
            tag = "MALWARE"
        elif shipped and aliases & kev_ids:
            tag = "KEV"
        elif shipped and best:
            tag = best
        elif shipped and unrated:
            tag = "UNRATED"
        if not tag:
            continue
        rows.append(
            {
                "key": f"{tag}|{canon}|{','.join(sorted(f'{e}/{n}' for e, n, _ in pkgs))}",
                "tag": tag,
                "id": canon,
                "pkgs": sorted(f"{n}@{v}" for _, n, v in pkgs),
                "files": shipped or anywhere,
            }
        )
    locked_names = {
        n.lower() for (_, n, _), locs in where.items() if any(locs.values())
    } - NAME_COLLISIONS
    named = locked_names | INVENTORY
    for v in data["kev"]:
        product = str(v.get("product") or "").strip().lower()
        if product in named:
            rows.append(
                {
                    "key": f"KEV-NAME|{v.get('cveID')}",
                    "tag": "KEV-NAME",
                    "id": str(v.get("cveID")),
                    "pkgs": [product],
                    "files": ["product name match, trace reachability"],
                    "added": str(v.get("dateAdded") or ""),
                }
            )
    return rows


def line(row: dict) -> str:
    files = ", ".join(clean(f) for f in row["files"][:4])
    pkgs = ", ".join(clean(p) for p in row["pkgs"][:6])
    return f"{row['tag']} {clean(row['id'])} {pkgs} in {files}"


def save(seen: set[str]) -> None:
    STATE.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=STATE, prefix=".seen.")
    with os.fdopen(fd, "w") as fh:
        json.dump(sorted(seen), fh)
    os.replace(tmp, SEEN)


def main() -> int:
    dry = "--dry-run" in sys.argv
    try:
        first = not SEEN.exists()
        seen = set(json.loads(SEEN.read_text())) if not first else set()
        data = collect()
    except (CouldNotRun, subprocess.SubprocessError, ValueError, OSError) as exc:
        print(f"advisory watch could not run: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2
    rows = evaluate(data)
    current = {r["key"] for r in rows}
    fresh = [r for r in rows if r["key"] not in seen]
    # The first run records today's backlog as the baseline. Malware and KEV still
    # alert on it, and a recent named KEV entry does; the rest is in the printed list.
    if first:
        cutoff = time.strftime("%Y-%m-%d", time.gmtime(time.time() - 30 * 86400))
        alert = [
            r
            for r in fresh
            if r["tag"] in ("MALWARE", "KEV")
            or (r["tag"] == "KEV-NAME" and r.get("added", "") >= cutoff)
        ]
    else:
        alert = fresh
    print(
        f"{data['files']} lockfiles, {len(data['pkgs'])} locked packages (distinct), "
        f"{len(data['hits'])} advisory ids, {len(data['kev'])} KEV rows, "
        f"{len(rows)} alertable, {len(fresh)} unseen, {len(alert)} to report"
        + (" (first run, baseline)" if first else "")
    )
    for label, names in sorted(data["unpinned"].items()):
        print(f"unpinned, not checked: {label}: {', '.join(names)}")
    if first or dry:
        for r in sorted(fresh, key=lambda r: r["tag"]):
            print("  " + line(r))
    if alert:
        body = "Advisory watch: new advisories against locked versions on origin/main.\n" + "\n".join(
            line(r) for r in alert[:40]
        )
        if len(alert) > 40:
            body += f"\n... and {len(alert) - 40} more"
        body += "\nTrace reachability per docs/security/RED-TEAM-LOOP.md, intake step."
        if dry:
            print(body)
            return 0
        try:
            p = subprocess.run(
                ["amux", "send", WORKER, "--no-board", "--stdin"],
                input=body,
                text=True,
                capture_output=True,
                timeout=60,
            )
            sent, why = p.returncode == 0, p.stderr.strip()[:200]
        except (OSError, subprocess.SubprocessError) as exc:
            sent, why = False, str(exc)
        if not sent:
            print(f"advisory watch found {len(alert)} but could not deliver: {why}", file=sys.stderr)
            return 3
    if not dry:
        # Only what is true now: a package that was fixed and later regresses alerts again.
        save(current)
    return 0


if __name__ == "__main__":
    sys.exit(main())
