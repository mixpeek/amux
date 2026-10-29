#!/usr/bin/env python3
"""Tests for scripts/wake-watch.py (file and CI wake events).

Runs the script as a subprocess under a temp AMUX_HOME with a controlled clock
(WAKE_WATCH_NOW) and a fake GitHub API (a local http.server). No real
schedule is created (WAKE_WATCH_SCHEDULE=0) and nothing touches ~/.amux.

  python3 scripts/test-wake-watch.py            # all cases
  python3 scripts/test-wake-watch.py -v         # verbose
"""
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest

SCRIPT = os.environ.get("WAKE_WATCH_SCRIPT") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "wake-watch.py")
T0 = 1_800_000_000


class FakeGitHub(http.server.BaseHTTPRequestHandler):
    """Serves runs from `FakeGitHub.runs[path]`; counts every request."""
    runs = {}
    calls = []
    status_code = 200

    schedules = []
    amux_writes = []
    amux_status = 200

    def _reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _amux_write(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"null")
        FakeGitHub.amux_writes.append((self.command, self.path, body, self.headers.get("X-Amux-Session")))
        if FakeGitHub.amux_status != 200:
            return self._reply(FakeGitHub.amux_status, {"error": "injected"})
        if self.command == "POST":
            body = dict(body, id="SCHED-%d" % (len(FakeGitHub.schedules) + 1))
            FakeGitHub.schedules.append(body)
            return self._reply(201, body)
        return self._reply(200, {"ok": True})

    do_POST = _amux_write
    do_PATCH = _amux_write

    def do_GET(self):
        if self.path.startswith("/api/schedules"):
            if FakeGitHub.amux_status != 200:
                return self._reply(FakeGitHub.amux_status, {"error": "injected"})
            return self._reply(200, FakeGitHub.schedules)
        FakeGitHub.calls.append(self.path)
        body = FakeGitHub.runs.get(self.path.split("?")[0] + ("?" + self.path.split("?")[1] if "?" in self.path else ""))
        if body is None:
            body = FakeGitHub.runs.get(self.path.split("?")[0])
        code = FakeGitHub.status_code
        if code != 200:
            body = {"message": "Bad credentials"}
        elif body is None:
            code, body = 404, {"message": "Not Found"}
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *a):
        pass


def put(path, mode, text):
    with open(path, mode) as f:
        f.write(text)


class WakeWatch(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeGitHub)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()
        cls.srv.server_close()

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = os.path.join(self.tmp.name, "amux")
        self.data = os.path.join(self.tmp.name, "data")
        os.makedirs(self.data)
        self.now = T0
        FakeGitHub.runs, FakeGitHub.calls, FakeGitHub.status_code = {}, [], 200
        FakeGitHub.schedules, FakeGitHub.amux_writes, FakeGitHub.amux_status = [], [], 200

    def tearDown(self):
        for root, dirs, _ in os.walk(self.tmp.name):
            for d in dirs:
                os.chmod(os.path.join(root, d), 0o755)
        self.tmp.cleanup()

    def run_ww(self, *args, lane="lane-a", schedule="0"):
        env = dict(os.environ, AMUX_HOME=self.home, WAKE_WATCH_NOW=str(self.now), WAKE_WATCH_SCHEDULE=schedule,
                   AMUX_URL="http://127.0.0.1:%d" % self.srv.server_address[1],
                   AMUX_SESSION=lane, AMUX_GITHUB_API="http://127.0.0.1:%d" % self.srv.server_address[1],
                   WAKE_WATCH_GH_TOKEN="test-token")
        p = subprocess.run([sys.executable, SCRIPT, *args], capture_output=True, text=True, env=env, timeout=60)
        return p.returncode, p.stdout, p.stderr

    def check(self, lane="lane-a", advance=31):
        self.now += advance
        return self.run_ww("check", "--lane", lane, lane=lane)

    def status(self, lane="lane-a"):
        rc, out, err = self.run_ww("status", "--lane", lane, "--json")
        self.assertEqual(rc, 0, err)
        return json.loads(out)

    def log(self):
        with open(os.path.join(self.home, "logs", "wake-watch.log")) as f:
            return f.read()

    # -- file arrival ------------------------------------------------------------

    def test_file_appears_and_the_lane_gets_exactly_one_message(self):
        target = os.path.join(self.data, "ledger-c10.md")
        rc, out, err = self.run_ww("add", "file", "--path", os.path.join(self.data, "ledger-*.md"), "--when", "exists")
        self.assertEqual(rc, 0, err)
        rc, out, _ = self.check()
        self.assertEqual((rc, out), (0, ""), "nothing there yet: no message")
        put(target, "w", "done\n")
        rc, out, _ = self.check()
        self.assertEqual(rc, 10, "exit 10 is the scheduler's wake exit action")
        self.assertEqual(out.count("fired:"), 1, out)
        self.assertIn(target, out)
        for _ in range(3):
            rc, out, _ = self.check()
            self.assertEqual((rc, out), (0, ""), "a fired watch never fires again")
        self.assertEqual(self.log().count("verdict=watch_fired"), 1)

    def test_a_matching_line_appended_fires_with_the_path_and_line(self):
        f = os.path.join(self.data, "events.log")
        put(f, "w", "RUN_DONE c9 was already in the file\n")
        rc, _, err = self.run_ww("add", "file", "--path", f, "--regex", r"RUN_DONE c\d+")
        self.assertEqual(rc, 0, err)
        put(f, "a", "noise\nRUN_DONE c1")  # incomplete line: not yet a line
        rc, out, _ = self.check()
        self.assertEqual((rc, out), (0, ""), "pre-existing and incomplete lines do not fire")
        put(f, "a", "0 finished\n")
        rc, out, _ = self.check()
        self.assertEqual(rc, 10, out)
        self.assertIn("%s: RUN_DONE c10 finished" % f, out)
        self.assertEqual(out.count("fired:"), 1)

    def test_changed_fires_on_modification(self):
        f = os.path.join(self.data, "state.json")
        put(f, "w", "{}")
        self.assertEqual(self.run_ww("add", "file", "--path", f, "--when", "changed")[0], 0)
        self.assertEqual(self.check()[0], 0)
        put(f, "w", '{"a": 1}')
        rc, out, _ = self.check()
        self.assertEqual(rc, 10)
        self.assertIn("modified", out)

    def test_unreadable_path_stays_unmeasured_and_never_reads_as_no_event(self):
        d = os.path.join(self.data, "locked")
        os.makedirs(d)
        f = os.path.join(d, "out.log")
        put(f, "w", "")
        self.assertEqual(self.run_ww("add", "file", "--path", f, "--regex", "READY")[0], 0)
        put(f, "a", "READY\n")
        os.chmod(f, 0o000)
        rc, out, _ = self.check()
        self.assertEqual((rc, out), (0, ""), "an unreadable file must not fire")
        w = self.status()["lanes"]["lane-a"][0]
        self.assertIs(w["measured"], False, w)
        self.assertIn("cannot read", w["why_unmeasured"])
        self.assertIn("WARN verdict=watch_unmeasured", self.log())
        os.chmod(f, 0o644)
        rc, out, _ = self.check()
        self.assertEqual(rc, 10, "the line is still found once readable: nothing was skipped")
        # An unlistable directory: glob() would return [] and look like "nothing there".
        g = os.path.join(self.data, "sealed")
        os.makedirs(g)
        self.assertEqual(self.run_ww("add", "file", "--path", os.path.join(g, "*.done"))[0], 0)
        put(os.path.join(g, "x.done"), "w", "")
        os.chmod(g, 0o000)
        rc, out, _ = self.check()
        self.assertEqual((rc, out), (0, ""))
        w = [x for x in self.status()["lanes"]["lane-a"] if x["state"] == "watching"][0]
        self.assertIs(w["measured"], False)
        self.assertIn("cannot list", w["why_unmeasured"])
        os.chmod(g, 0o755)
        self.assertEqual(self.check()[0], 10)

    def test_registering_an_unreadable_path_is_refused(self):
        d = os.path.join(self.data, "nope")
        os.makedirs(d)
        os.chmod(d, 0o000)
        rc, _, err = self.run_ww("add", "file", "--path", os.path.join(d, "*.md"))
        self.assertEqual(rc, 2, err)
        self.assertIn("cannot list", err)

    # -- bounds -----------------------------------------------------------------

    def test_expiry_tells_the_lane_once_and_closes_the_watch(self):
        self.assertEqual(self.run_ww("add", "file", "--path", os.path.join(self.data, "never"), "--expires", "2m",
                                     "--note", "then land C10")[0], 0)
        self.assertEqual(self.check(advance=60)[0], 0)
        rc, out, _ = self.check(advance=61)
        self.assertEqual(rc, 10)
        self.assertIn("EXPIRED unfired", out)
        self.assertIn("then land C10", out)
        self.assertEqual(self.check()[:2], (0, ""))
        self.assertEqual(self.status()["lanes"]["lane-a"][0]["state"], "expired")
        self.assertEqual(self.log().count("verdict=watch_expired"), 1)
        # Closed watches are pruned after a day: bounded on disk.
        self.check(advance=86400 + 10)
        self.assertEqual(self.status()["lanes"]["lane-a"], [])
        self.assertIn("verdict=watch_pruned", self.log())

    def test_expiry_above_the_ceiling_is_refused(self):
        rc, _, err = self.run_ww("add", "file", "--path", self.data, "--expires", "30d")
        self.assertNotEqual(rc, 0)
        self.assertIn("--expires", err)

    def test_the_per_lane_cap(self):
        for i in range(8):
            rc, _, err = self.run_ww("add", "file", "--path", os.path.join(self.data, "f%d" % i))
            self.assertEqual(rc, 0, err)
        rc, _, err = self.run_ww("add", "file", "--path", os.path.join(self.data, "f9"))
        self.assertEqual(rc, 3, err)
        self.assertIn("cap 8", err)
        self.assertIn("WARN verdict=watch_cap_refused", self.log())
        # Another lane has its own budget.
        self.assertEqual(self.run_ww("add", "file", "--path", self.data, lane="lane-b")[0], 0)

    def test_the_poll_interval_floor(self):
        rc, out, _ = self.run_ww("add", "file", "--path", os.path.join(self.data, "x"), "--every", "1")
        self.assertEqual(rc, 0)
        self.assertIn("raised to the 30s floor", out)
        put(os.path.join(self.data, "x"), "w", "")
        self.assertEqual(self.check(advance=5)[0], 0, "inside the floor: not checked yet")
        self.assertEqual(self.check(advance=30)[0], 10)

    # -- CI ---------------------------------------------------------------------

    def test_a_ci_run_concludes_and_one_poller_serves_every_lane(self):
        path = "/repos/o/r/actions/runs/4021"
        FakeGitHub.runs[path] = {"status": "in_progress", "conclusion": None, "html_url": "https://gh/run/4021", "name": "rust"}
        for lane in ("lane-a", "lane-b"):
            rc, _, err = self.run_ww("add", "ci", "--repo", "o/r", "--run", "4021", lane=lane)
            self.assertEqual(rc, 0, err)
        self.now += 61
        self.assertEqual(self.run_ww("check", "--lane", "lane-a", lane="lane-a")[0], 0)
        self.assertEqual(self.run_ww("check", "--lane", "lane-b", lane="lane-b")[0], 0)
        self.assertEqual(len(FakeGitHub.calls), 1, "two lanes, one GitHub read inside the floor: %s" % FakeGitHub.calls)
        FakeGitHub.runs[path] = {"status": "completed", "conclusion": "failure", "html_url": "https://gh/run/4021", "name": "rust"}
        self.now += 61
        outs = [self.run_ww("check", "--lane", l, lane=l) for l in ("lane-a", "lane-b")]
        self.assertEqual([o[0] for o in outs], [10, 10])
        for o in outs:
            self.assertIn("completed failure", o[1])
            self.assertIn("https://gh/run/4021", o[1])
        self.assertEqual(len(FakeGitHub.calls), 2, FakeGitHub.calls)
        # The shared state file is the one `amux ci wait` reads.
        with open(os.path.join(self.home, "ci-watch", "o_r-4021", "state.json")) as fh:
            st = json.load(fh)
        self.assertEqual((st["status"], st["conclusion"], st["measured"]), ("completed", "failure", True))

    def test_a_rerun_fires_only_when_the_current_attempt_completes(self):
        # 2026-10-29, mixpeek run 40872577418: attempt 1's cached "completed
        # failure" fired two new watches while attempt 2 was in progress.
        path = "/repos/o/r/actions/runs/4022"
        FakeGitHub.runs[path] = {"status": "completed", "conclusion": "failure", "html_url": "u", "name": "rust",
                                 "run_attempt": 1}
        self.assertEqual(self.run_ww("add", "ci", "--repo", "o/r", "--run", "4022", lane="lane-a")[0], 0)
        self.assertEqual(self.check(lane="lane-a", advance=61)[0], 10, "attempt 1 concludes for its own watch")
        FakeGitHub.runs[path] = {"status": "in_progress", "conclusion": None, "html_url": "u", "name": "rust",
                                 "run_attempt": 2}
        self.now += 5
        self.assertEqual(self.run_ww("add", "ci", "--repo", "o/r", "--run", "4022", lane="lane-b")[0], 0)
        self.assertEqual(self.check(lane="lane-b", advance=1)[0], 0, "the cached attempt 1 must not fire a new watch")
        self.assertEqual(self.check(lane="lane-b", advance=61)[0], 0, "attempt 2 still in progress")
        FakeGitHub.runs[path] = {"status": "completed", "conclusion": "success", "html_url": "u", "name": "rust",
                                 "run_attempt": 2}
        rc, out, _ = self.check(lane="lane-b", advance=61)
        self.assertEqual(rc, 10)
        self.assertIn("attempt 2 completed success", out)

    def test_a_sha_with_no_runs_yet_is_pending_never_success(self):
        q = "/repos/o/r/actions/runs?head_sha=abc123&per_page=100"
        FakeGitHub.runs[q] = {"workflow_runs": []}
        self.assertEqual(self.run_ww("add", "ci", "--repo", "o/r", "--sha", "abc123")[0], 0)
        self.assertEqual(self.check(advance=61)[0], 0)
        w = self.status()["lanes"]["lane-a"][0]
        self.assertEqual((w["measured"], w["n_considered"], w["why_unmeasured"]), (True, 0, ""), w)
        FakeGitHub.runs[q] = {"workflow_runs": [
            {"id": 1, "name": "rust", "status": "completed", "conclusion": "success", "html_url": "u1"},
            {"id": 2, "name": "checks", "status": "in_progress", "conclusion": None, "html_url": "u2"}]}
        self.assertEqual(self.check(advance=61)[0], 0, "one run still in progress")
        FakeGitHub.runs[q]["workflow_runs"][1].update(status="completed", conclusion="success")
        rc, out, _ = self.check(advance=61)
        self.assertEqual(rc, 10)
        self.assertIn("checks success u2", out)

    def test_github_unreadable_is_unmeasured_not_concluded(self):
        FakeGitHub.status_code = 401
        self.assertEqual(self.run_ww("add", "ci", "--repo", "o/r", "--run", "77")[0], 0)
        rc, out, _ = self.check(advance=61)
        self.assertEqual((rc, out), (0, ""))
        w = self.status()["lanes"]["lane-a"][0]
        self.assertIs(w["measured"], False)
        self.assertIn("http 401", w["why_unmeasured"])

    # -- the lane's schedule ----------------------------------------------------------

    def test_add_ensures_one_wake_schedule_per_lane(self):
        rc, out, err = self.run_ww("add", "file", "--path", self.data, "--expires", "1h", schedule="1")
        self.assertEqual(rc, 0, err)
        rc, out, err = self.run_ww("add", "file", "--path", self.data, "--expires", "3h", schedule="1")
        self.assertEqual(rc, 0, err)
        posts = [w for w in FakeGitHub.amux_writes if w[0] == "POST"]
        patches = [w for w in FakeGitHub.amux_writes if w[0] == "PATCH"]
        self.assertEqual((len(posts), len(patches)), (1, 1), FakeGitHub.amux_writes)
        body = posts[0][2]
        self.assertEqual((body["kind"], body["session"], body["schedule_expr"], body["exit_actions"], posts[0][3]),
                         ("shell", "lane-a", "every 1m", {"10": "wake"}, "lane-a"))
        self.assertIn("check --lane lane-a", body["command"])
        self.assertEqual(patches[0][1], "/api/schedules/SCHED-1")
        # until covers the latest watch (3h) plus the expiry-notice margin.
        self.assertEqual(patches[0][2]["until"], time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(T0 + 3 * 3600 + 180)))

    def test_a_schedule_that_cannot_be_ensured_fails_the_add_loudly(self):
        FakeGitHub.amux_status = 500
        rc, out, _ = self.run_ww("add", "file", "--path", self.data, schedule="1")
        self.assertEqual(rc, 4)
        self.assertIn("FAILED", out)
        self.assertIn("WARN verdict=watch_schedule_failed", self.log())

    def test_status_publishes_measured_and_n_considered(self):
        s = self.status()
        self.assertEqual((s["measured"], s["n_considered"]), (True, 0))
        self.run_ww("add", "file", "--path", self.data)
        s = self.status()
        self.assertEqual(s["n_considered"], 1)
        self.assertIsNone(s["lanes"]["lane-a"][0]["measured"], "not yet checked is not 'measured'")


if __name__ == "__main__":
    unittest.main()
