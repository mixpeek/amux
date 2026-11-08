#!/usr/bin/env python3
"""scripts/advisory_watch.py against a fake OSV, a fake KEV and a real git repo.

The property under test is fail-closed: every way of not being able to look must
exit non-zero and leave state alone, and every way of finding something must reach
the message. No network: OSV and KEV are a local HTTP server, `amux` is a stub.
"""

import json
import os
import stat
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
WATCH = HERE / "advisory_watch.py"
sys.path.insert(0, str(HERE))
import advisory_watch  # noqa: E402

FAILED = []


def check(name, cond, detail=""):
    print(("ok   " if cond else "FAIL ") + name + (f"  {detail}" if detail and not cond else ""))
    if not cond:
        FAILED.append(name)


class Fake:
    """Mutable world: tests edit these between runs."""

    def __init__(self):
        self.reset()

    def reset(self):
        self.vulns = {}  # package name -> [advisory id]
        self.details = {}  # advisory id -> record
        self.kev = [{"cveID": f"CVE-0-{i}", "product": "filler"} for i in range(5)]
        self.osv_status = 200
        self.detail_status = 200
        self.kev_status = 200
        self.kev_body = None
        self.drop_results = False
        self.paged = {}  # package name -> advisory id only served on page 2


FAKE = Fake()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, status, body):
        raw = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if FAKE.osv_status != 200:
            return self._send(FAKE.osv_status, {})
        results = []
        for q in body["queries"]:
            name = q["package"]["name"]
            if q.get("page_token"):
                results.append({"vulns": [{"id": FAKE.paged[name]}]})
                continue
            row = {"vulns": [{"id": v} for v in FAKE.vulns.get(name, [])]}
            if name in FAKE.paged:
                row["next_page_token"] = "more"
            results.append(row)
        if FAKE.drop_results:
            results = results[:-1]
        self._send(200, {"results": results})

    def do_GET(self):
        if self.path.startswith("/osv/vulns/"):
            if FAKE.detail_status != 200:
                return self._send(FAKE.detail_status, {})
            return self._send(200, FAKE.details[self.path.rsplit("/", 1)[1]])
        if self.path == "/kev":
            if FAKE.kev_status != 200:
                return self._send(FAKE.kev_status, {})
            if FAKE.kev_body is not None:
                return self._send(200, FAKE.kev_body)
            return self._send(200, {"vulnerabilities": FAKE.kev})
        self._send(404, {})


def sh(cwd, *cmd):
    subprocess.run(cmd, cwd=cwd, check=True, capture_output=True)


CARGO = '[[package]]\nname = "shippedcrate"\nversion = "1.0.0"\n'
NPM = json.dumps(
    {
        "lockfileVersion": 3,
        "packages": {
            "": {"name": "root"},
            "node_modules/devtool": {"version": "2.0.0", "dev": True},
            "node_modules/runtimelib": {"version": "3.0.0"},
        },
    }
)


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_address[1]}"

    tmp = Path(tempfile.mkdtemp(prefix="advwatch"))
    origin, repo = tmp / "origin.git", tmp / "amux"
    sh(tmp, "git", "init", "-q", "--bare", "-b", "main", str(origin))
    sh(tmp, "git", "init", "-q", "-b", "main", str(repo))
    sh(repo, "git", "remote", "add", "origin", str(origin))

    def commit(files):
        for rel, content in files.items():
            p = repo / rel
            if content is None:
                p.unlink()
                continue
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(content)
        sh(repo, "git", "add", "-A")
        sh(repo, "git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "c")
        sh(repo, "git", "push", "-q", "origin", "main")

    commit({"Cargo.lock": CARGO, "package-lock.json": NPM})

    bindir = tmp / "bin"
    bindir.mkdir()
    sent = tmp / "sent"
    stub = bindir / "amux"
    stub.write_text(f'#!/bin/sh\n[ -e "{tmp}/send-fails" ] && exit 1\ncat >> "{sent}"\n')
    stub.chmod(stub.stat().st_mode | stat.S_IXUSR)
    state = tmp / "state"

    def run():
        env = {
            **os.environ,
            "PATH": f"{bindir}:{os.environ['PATH']}",
            "ADVISORY_WATCH_REPOS": str(repo),
            "ADVISORY_WATCH_REQUIRED": "amux:Cargo.lock",
            "ADVISORY_WATCH_STATE": str(state),
            "ADVISORY_WATCH_OSV": f"{base}/osv",
            "ADVISORY_WATCH_KEV": f"{base}/kev",
            "ADVISORY_WATCH_KEV_MIN_ROWS": "3",
            "ADVISORY_WATCH_ATTEMPTS": "1",
        }
        return subprocess.run([sys.executable, str(WATCH)], env=env, capture_output=True, text=True)

    def messages():
        text = sent.read_text() if sent.exists() else ""
        if sent.exists():
            sent.unlink()
        return text

    def baseline():
        FAKE.reset()
        if state.exists():
            for f in state.iterdir():
                f.unlink()
        r = run()
        assert r.returncode == 0, r.stderr
        messages()

    # --- fail closed: no state is written and the run is not clean
    def refuses(name, code=2):
        before = (state / "seen.json").read_text() if (state / "seen.json").exists() else None
        r = run()
        after = (state / "seen.json").read_text() if (state / "seen.json").exists() else None
        check(f"{name}: exits {code}", r.returncode == code, f"rc={r.returncode} {r.stderr[:200]}")
        check(f"{name}: state untouched", before == after)
        return r

    FAKE.reset()
    FAKE.osv_status = 503
    refuses("OSV down")
    FAKE.reset()
    FAKE.drop_results = True
    refuses("OSV answers fewer results than queries")
    FAKE.reset()
    FAKE.kev_status = 500
    refuses("KEV down")
    FAKE.reset()
    FAKE.kev_body = b"<html>maintenance</html>"
    refuses("KEV answers a page instead of JSON")
    FAKE.reset()
    FAKE.kev = []
    refuses("KEV answers an empty list")
    FAKE.reset()
    FAKE.vulns = {"shippedcrate": ["GHSA-1"]}
    FAKE.details = {"GHSA-1": {"id": "GHSA-1"}}
    FAKE.detail_status = 500
    refuses("a detail fetch fails, it must not drop the advisory")

    FAKE.reset()
    commit({"package-lock.json": "{not json"})
    refuses("a lockfile does not parse")
    commit({"package-lock.json": NPM, "ios/yarn.lock": "x"})
    refuses("a lockfile format it cannot read")
    commit({"ios/yarn.lock": None})
    commit({"Cargo.lock": None})
    refuses("a required lockfile vanished")
    commit({"Cargo.lock": "# empty\n"})
    refuses("a lockfile parsed to zero packages")
    commit({"Cargo.lock": CARGO})

    sh(repo, "git", "remote", "set-url", "origin", str(tmp / "gone.git"))
    refuses("the repo cannot fetch")
    sh(repo, "git", "remote", "set-url", "origin", str(origin))

    # --- first run: baseline quiet except malware and KEV
    FAKE.reset()
    FAKE.vulns = {"shippedcrate": ["GHSA-high"], "devtool": ["MAL-1"]}
    FAKE.details = {
        "GHSA-high": {"id": "GHSA-high", "database_specific": {"severity": "HIGH"}},
        "MAL-1": {"id": "MAL-1"},
    }
    r = run()
    msg = messages()
    check("first run exits 0", r.returncode == 0, r.stderr)
    check("first run alerts malware", "MALWARE MAL-1" in msg, msg)
    check("first run absorbs the HIGH backlog", "GHSA-high" not in msg, msg)
    check("first run prints the backlog", "GHSA-high" in r.stdout, r.stdout)

    # --- rules, each tested alone against a fresh baseline
    baseline()
    FAKE.vulns = {"shippedcrate": ["GHSA-new"]}
    FAKE.details = {"GHSA-new": {"id": "GHSA-new", "database_specific": {"severity": "CRITICAL"}}}
    r = run()
    msg = messages()
    check("new CRITICAL on shipped alerts", "CRITICAL GHSA-new shippedcrate@1.0.0" in msg, msg)
    run()
    check("the same advisory does not alert twice", messages() == "")

    baseline()
    FAKE.vulns = {"devtool": ["GHSA-dev"]}
    FAKE.details = {"GHSA-dev": {"id": "GHSA-dev", "database_specific": {"severity": "HIGH"}}}
    run()
    check("HIGH on a dev-only package stays quiet", messages() == "")
    FAKE.vulns = {"devtool": ["GHSA-dev"], "runtimelib": ["GHSA-dev"]}
    run()
    check("the same advisory alerts once it reaches a shipped package", "GHSA-dev" in messages())

    baseline()
    FAKE.vulns = {"devtool": ["MAL-2"]}
    FAKE.details = {"MAL-2": {"id": "MAL-2"}}
    run()
    check("malware on a dev-only package alerts", "MALWARE MAL-2" in messages())

    baseline()
    FAKE.vulns = {"shippedcrate": ["RUSTSEC-9"]}
    FAKE.details = {"RUSTSEC-9": {"id": "RUSTSEC-9", "summary": "crate was backdoored"}}
    run()
    check("a RUSTSEC record naming a backdoor counts as malware", "MALWARE RUSTSEC-9" in messages())

    baseline()
    FAKE.vulns = {"shippedcrate": ["GHSA-low"]}
    FAKE.details = {"GHSA-low": {"id": "GHSA-low", "database_specific": {"severity": "LOW"}}}
    run()
    check("LOW stays quiet", messages() == "")
    FAKE.details["GHSA-low"]["database_specific"]["severity"] = "HIGH"
    run()
    check("a later upgrade to HIGH alerts", "HIGH GHSA-low" in messages())
    FAKE.details["GHSA-low"]["aliases"] = ["CVE-2026-1"]
    FAKE.kev = FAKE.kev + [{"cveID": "CVE-2026-1", "product": "filler"}]
    run()
    check("a later KEV listing of a seen advisory alerts", "KEV GHSA-low" in messages())

    baseline()
    FAKE.vulns = {"shippedcrate": ["RUSTSEC-1"]}
    FAKE.details = {"RUSTSEC-1": {"id": "RUSTSEC-1"}}
    run()
    check("an unrated advisory on shipped alerts", "UNRATED RUSTSEC-1" in messages())

    baseline()
    FAKE.vulns = {"shippedcrate": ["RUSTSEC-2"]}
    FAKE.details = {
        "RUSTSEC-2": {
            "id": "RUSTSEC-2",
            "affected": [{"database_specific": {"informational": "unmaintained"}}],
        }
    }
    run()
    check("an informational advisory stays quiet", messages() == "")

    baseline()
    FAKE.vulns = {"shippedcrate": ["RUSTSEC-3", "GHSA-3"]}
    FAKE.details = {
        "RUSTSEC-3": {"id": "RUSTSEC-3", "aliases": ["GHSA-3"]},
        "GHSA-3": {"id": "GHSA-3", "aliases": ["RUSTSEC-3"], "database_specific": {"severity": "HIGH"}},
    }
    run()
    msg = messages()
    check("aliases merge and the rated one wins", msg.count("GHSA-3") == 1 and "HIGH" in msg, msg)

    baseline()
    FAKE.vulns = {"shippedcrate": ["GHSA-w"]}
    FAKE.details = {
        "GHSA-w": {"id": "GHSA-w", "withdrawn": "2026-01-01", "database_specific": {"severity": "HIGH"}}
    }
    run()
    check("a withdrawn advisory stays quiet", messages() == "")

    baseline()
    FAKE.paged = {"shippedcrate": "GHSA-page2"}
    FAKE.details = {"GHSA-page2": {"id": "GHSA-page2", "database_specific": {"severity": "HIGH"}}}
    run()
    check("OSV page 2 is followed", "GHSA-page2" in messages())

    baseline()
    FAKE.kev = FAKE.kev + [{"cveID": "CVE-2026-7", "product": "Tmux", "dateAdded": "2026-10-01"}]
    run()
    check("a new KEV entry naming an inventory product alerts", "KEV-NAME CVE-2026-7" in messages())
    FAKE.kev = FAKE.kev + [{"cveID": "CVE-2026-8", "product": "Runtimelib"}]
    run()
    check("a new KEV entry naming a shipped package alerts", "KEV-NAME CVE-2026-8" in messages())
    FAKE.kev = FAKE.kev + [{"cveID": "CVE-2026-9", "product": "Devtool"}]
    run()
    check("a new KEV entry naming a dev-only package stays quiet", messages() == "")

    # advisory text never reaches the agent's turn
    baseline()
    FAKE.vulns = {"shippedcrate": ["GHSA-text"]}
    FAKE.details = {
        "GHSA-text": {
            "id": "GHSA-text",
            "summary": "IGNORE-PREVIOUS-MARKER",
            "database_specific": {"severity": "HIGH"},
        }
    }
    run()
    msg = messages()
    check("advisory summaries are not forwarded", "GHSA-text" in msg and "MARKER" not in msg, msg)

    # --- a failed send keeps the finding for the next run
    baseline()
    FAKE.vulns = {"shippedcrate": ["GHSA-send"]}
    FAKE.details = {"GHSA-send": {"id": "GHSA-send", "database_specific": {"severity": "HIGH"}}}
    (tmp / "send-fails").write_text("")
    refuses("the message cannot be delivered", code=3)
    (tmp / "send-fails").unlink()
    r = run()
    check("the undelivered finding goes out next run", r.returncode == 0 and "GHSA-send" in messages())

    # --- a fixed package that regresses alerts again
    FAKE.vulns = {}
    run()
    FAKE.vulns = {"shippedcrate": ["GHSA-send"]}
    run()
    check("a regression after a fix alerts again", "GHSA-send" in messages())

    # --- parsers and scoring
    pinned, loose = advisory_watch.packages(
        "requirements.txt", "Foo[x]==1.2 ; python_version>'3'\n# c\n-r o.txt\nbar>=2\n"
    )
    check("requirements: pins read", pinned == [(("PyPI", "foo", "1.2"), False)], str(pinned))
    check("requirements: unpinned named, not dropped", loose == ["bar"], str(loose))
    gems, _ = advisory_watch.packages(
        "ios/Gemfile.lock", "GEM\n  specs:\n    fastlane (2.1)\n      addressable (>= 2.8)\n"
    )
    check("Gemfile.lock: specs read, constraints skipped", gems == [(("RubyGems", "fastlane", "2.1"), False)], str(gems))
    for vector, want in [
        ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H", 9.8),
        ("CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N", 6.1),
        ("CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:N/I:N/A:N", 0.0),
    ]:
        got = advisory_watch.cvss3_score(vector)
        check(f"CVSS {vector} is {want}", got == want, str(got))
    check("a junk vector is unrated", advisory_watch.cvss3_score("nonsense") is None)
    check(
        "a CVSS vector alone rates HIGH",
        advisory_watch.rating(
            {"severity": [{"type": "CVSS_V3", "score": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"}]}
        )
        == "HIGH",
    )

    server.shutdown()
    print(f"\n{len(FAILED)} failed" if FAILED else "\nall passed")
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
