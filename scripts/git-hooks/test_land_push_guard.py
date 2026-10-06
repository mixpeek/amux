#!/usr/bin/env python3
"""Contract rule 5 (AH-380): the shared git guard refuses a worker push to main
on a lane whose landing the server owns, and fails open everywhere else.

A stub server answers GET /api/land/policy the way the real one does."""
import http.server
import json
import os
import subprocess
import sys
import threading

HOOK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "git-shared-guard.py")
QUEUE = {"on": True}
ASKED = []


class Policy(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ASKED.append(self.path)
        body = json.dumps({"queue": QUEUE["on"]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def run(command, url, session="gs12-extra-1"):
    env = dict(os.environ, AMUX_URL=url, AMUX_SESSION=session, AMUX_HOME="/nonexistent-amux-home")
    p = subprocess.run([sys.executable, HOOK],
                       input=json.dumps({"tool_name": "Bash", "tool_input": {"command": command}, "cwd": "/tmp"}),
                       capture_output=True, text=True, env=env, timeout=30)
    return "contract rule 5" in p.stdout


def main():
    srv = http.server.HTTPServer(("127.0.0.1", 0), Policy)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    url = "http://127.0.0.1:%d" % srv.server_address[1]
    fails = []

    def check(name, got, want):
        if got != want:
            fails.append(f"{name}: refused={got}, expected {want}")

    for cmd in ["git push origin HEAD:main", "git push -q origin abc123:refs/heads/main",
                "scripts/graft-push.sh --from abc a.txt"]:
        check(cmd, run(cmd, url), True)
    if not any("push=1" in p and "lane=gs12-extra-1" in p for p in ASKED):
        fails.append(f"the guard never asked the policy with push=1: {ASKED}")
    check("a feature branch push", run("git push origin feature-x", url), False)
    check("a non-push command", run("git log --oneline -3 main", url), False)
    check("no session", run("git push origin HEAD:main", url, session=""), False)
    QUEUE["on"] = False
    check("rule 5 off for the lane", run("git push origin HEAD:main", url), False)
    srv.shutdown()
    check("server down fails open", run("git push origin HEAD:main", "http://127.0.0.1:9"), False)
    for f in fails:
        print("FAIL", f)
    print("land push guard:", "ok" if not fails else f"{len(fails)} failure(s)")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
