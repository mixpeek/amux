"""Policy tests for scripts/claude-rotate.py. Run: python3 scripts/tests/test_claude_rotate.py"""
import datetime as dt, importlib.util, io, json, os, sys, tempfile, time, unittest
from contextlib import redirect_stdout
import subprocess as _sp, threading
from http.server import BaseHTTPRequestHandler, HTTPServer
# Some tests below patch the subprocess module itself; keep the real calls.
REAL_RUN, REAL_POPEN = _sp.run, _sp.Popen

TMP = tempfile.mkdtemp()
os.environ.update(CLAUDE_ROTATE_DIR=TMP, CLAUDE_ROTATE_USAGE_LOG="off", CLAUDE_ROTATE_LOG=os.path.join(TMP, "log"),
                  CLAUDE_ROTATE_CLAUDE_JSON=os.path.join(TMP, "claude.json"))
spec = importlib.util.spec_from_file_location("cr", os.path.join(os.path.dirname(__file__), "..", "claude-rotate.py"))
cr = importlib.util.module_from_spec(spec); spec.loader.exec_module(cr)
NOW = dt.datetime(2026, 10, 15, 11, 30).astimezone()
h = lambda hours: (NOW + dt.timedelta(hours=hours)).isoformat()
# record() compares against the real clock.
r = lambda hours: (dt.datetime.now().astimezone() + dt.timedelta(hours=hours)).isoformat()

def acct(five, five_r, week, week_r):
    return {"five_hour": {"used_percentage": five, "resets_at": five_r}, "seven_day": {"used_percentage": week, "resets_at": week_r}}

def setup(accounts, current=None):
    cr.save(cr.ACCOUNTS, {"accounts": accounts})
    json.dump({"oauthAccount": {"emailAddress": current}}, open(cr.CLAUDE_JSON, "w"))

class Policy(unittest.TestCase):
    def test_expiring_capacity_first(self):
        setup({"run": acct(10, h(4), 29, h(57)), "join": acct(0, None, 70, h(15)), "use": acct(0, None, 100, h(104))})
        rows = cr.rank(NOW)
        self.assertEqual([r["email"] for r in rows], ["join", "run", "use"])
        self.assertEqual(cr.decide("run", rows, {}, NOW), ("join", "expiring_capacity_first"))

    def test_exhausted_switches_even_right_after_a_switch(self):
        setup({"join": acct(99.5, h(2), 90, h(15)), "run": acct(10, h(4), 29, h(57))})
        self.assertEqual(cr.decide("join", cr.rank(NOW), {"switched_at": time.time()}, NOW), ("run", "current_exhausted"))

    def test_no_flap_inside_gap(self):
        setup({"run": acct(10, h(4), 29, h(57)), "join": acct(0, None, 70, h(15))})
        self.assertEqual(cr.decide("run", cr.rank(NOW), {"switched_at": time.time()}, NOW), (None, "recent_switch"))

    def test_expiring_target_over_preempt_line_is_not_picked(self):
        # It would be preempted on the next tick and flap straight back.
        setup({"run": acct(10, h(4), 29, h(57)), "join": acct(0, None, 89, h(15))})
        self.assertEqual(cr.decide("run", cr.rank(NOW), {}, NOW), (None, "current_is_best"))

    def test_reset_passed_counts_as_zero(self):
        setup({"amux": acct(0, None, 100, h(-1))})
        r = cr.rank(NOW)[0]
        self.assertTrue(r["usable"]); self.assertEqual(r["weekly"], 0.0)

    def test_all_exhausted_holds_and_names_earliest(self):
        setup({"a": acct(0, None, 100, h(20)), "b": acct(100, h(1), 50, h(90))})
        rows = cr.rank(NOW)
        self.assertEqual(cr.decide("a", rows, {}, NOW), (None, "no_usable_account"))
        self.assertEqual(rows[0]["email"], "b")  # back first, in one hour

    def test_unknown_after_known(self):
        setup({"new": {}, "run": acct(10, h(4), 29, h(57))})
        self.assertEqual([r["email"] for r in cr.rank(NOW)], ["run", "new"])

    def test_five_hour_block_is_temporary(self):
        setup({"vec": acct(99, h(1), 93, h(140))})
        r = cr.rank(NOW)[0]
        self.assertFalse(r["usable"]); self.assertEqual(r["available_at"], h(1))

    def test_statusline_records_current_account_and_prints(self):
        setup({}, current="ethan@joinmixpeek.com")
        cr.save(cr.STATE, {"switched_at": 0})
        cr.save(os.path.join(cr.DIR, "seen-login.json"), {"email": "ethan@joinmixpeek.com", "since": 0})
        payload = {"rate_limits": {"five_hour": {"used_percentage": 12.4, "resets_at": 1792090000},
                                   "seven_day": {"used_percentage": 90, "resets_at": 1792130400}}}
        sys.stdin = io.StringIO(json.dumps(payload)); out = io.StringIO()
        with redirect_stdout(out): cr.cmd_statusline()
        self.assertEqual(out.getvalue().strip(), "joinmixpeek · 5h 12% · wk 90%")
        saved = cr.load(cr.ACCOUNTS, {})["accounts"]["ethan@joinmixpeek.com"]
        self.assertEqual(saved["seven_day"]["used_percentage"], 90)

    def test_stale_lower_reading_does_not_regress(self):
        setup({"a@x.com": acct(30, r(3), 59, r(50))}, current="a@x.com")
        self.assertFalse(cr.record("a@x.com", 20, r(3), 39, r(50)))
        saved = cr.load(cr.ACCOUNTS, {})["accounts"]["a@x.com"]
        self.assertEqual(saved["seven_day"]["used_percentage"], 59)
        self.assertEqual(saved["five_hour"]["used_percentage"], 30)
        # Five-hour rose, weekly is stale: store the rise, keep the weekly.
        self.assertTrue(cr.record("a@x.com", 35, r(3), 40, r(50)))
        saved = cr.load(cr.ACCOUNTS, {})["accounts"]["a@x.com"]
        self.assertEqual((saved["five_hour"]["used_percentage"], saved["seven_day"]["used_percentage"]), (35, 59))
        self.assertTrue(cr.record("a@x.com", 36, r(3), 60, r(50)))
        self.assertEqual(cr.load(cr.ACCOUNTS, {})["accounts"]["a@x.com"]["seven_day"]["used_percentage"], 60)

    def test_other_accounts_window_is_rejected_from_statusline_only(self):
        setup({"a@x.com": acct(30, r(3), 59, r(50))}, current="a@x.com")
        self.assertFalse(cr.record("a@x.com", 10, r(2), 98, r(15)))
        self.assertEqual(cr.load(cr.ACCOUNTS, {})["accounts"]["a@x.com"]["seven_day"]["used_percentage"], 59)
        self.assertTrue(cr.record("a@x.com", 10, r(2), 98, r(15), source="usage-probe"))
        self.assertEqual(cr.load(cr.ACCOUNTS, {})["accounts"]["a@x.com"]["seven_day"]["used_percentage"], 98)

    def test_passed_reset_accepts_a_new_window(self):
        setup({"a@x.com": acct(90, r(-1), 95, r(-2))}, current="a@x.com")
        self.assertTrue(cr.record("a@x.com", 2, r(4), 1, r(166)))
        self.assertEqual(cr.load(cr.ACCOUNTS, {})["accounts"]["a@x.com"]["seven_day"]["used_percentage"], 1)

    def test_no_preempt_to_an_account_whose_5h_is_also_hot(self):
        # 2026-10-16 12:05: mixpeek at 5h 85% preempted back to join at 86%.
        setup({"mix": acct(86, h(1), 22, h(160)), "join": acct(81, h(1), 50, h(140))})
        self.assertEqual(cr.decide("mix", cr.rank(NOW), {}, NOW), (None, "current_is_fine"))

    def test_unreadable_link_backs_off_at_once(self):
        t = time.time()
        self.assertEqual(len(cr.note_failure([], ["tab", "continue-email"], t)), 1)
        times = cr.note_failure([], ["tab", "error: sign-in link unreadable"], t)
        self.assertIn("vec", cr.backed_off({"switch_failures": {"vec": times}}, t))

    def test_preempt_goes_to_a_cool_account(self):
        setup({"mix": acct(86, h(1), 22, h(160)), "join": acct(30, h(1), 50, h(140))})
        self.assertEqual(cr.decide("mix", cr.rank(NOW), {}, NOW), ("join", "preempt_limit"))

    def test_tick_is_off_until_opted_in(self):
        setup({"join": acct(100, h(1), 52, h(140)), "vec": acct(0, None, 80, h(100))}, current="join")
        cr.save(cr.STATE, {})
        calls, real = [], (cr.switch, cr.probe)
        cr.switch = lambda t, r: calls.append(t) or 0
        cr.probe = lambda: None
        try:
            with redirect_stdout(io.StringIO()):
                cr.main(["x", "off"])
                cr.cmd_tick()
                self.assertEqual(calls, [])
                os.environ["CLAUDE_ROTATE_ENABLED"] = "1"
                self.assertTrue(cr.enabled())
                os.environ["CLAUDE_ROTATE_ENABLED"] = "0"
                cr.main(["x", "on"])
                self.assertFalse(cr.enabled())
                del os.environ["CLAUDE_ROTATE_ENABLED"]
                self.assertTrue(cr.enabled())
        finally:
            cr.switch, cr.probe = real
            os.environ.pop("CLAUDE_ROTATE_ENABLED", None)

    def test_exhausted_tick_falls_back_to_the_next_account(self):
        setup({"join": acct(100, h(1), 52, h(140)), "vec": acct(0, None, 80, h(100)),
               "mix": acct(10, h(3), 30, h(160))}, current="join")
        cr.save(cr.STATE, {})
        calls, real = [], (cr.switch, cr.cli_email, cr.probe, cr.rank)
        cr.switch = lambda t, r: calls.append(t) or (1 if t == "vec" else 0)
        cr.cli_email = lambda: "join"
        cr.probe = lambda: None
        cr.rank = lambda at=None: real[3](NOW)
        try:
            with redirect_stdout(io.StringIO()):
                cr.main(["x", "on"])
                cr.cmd_tick()
        finally:
            cr.switch, cr.cli_email, cr.probe, cr.rank = real
        self.assertEqual(calls, ["vec", "mix"])

    def test_login_gap_measures_the_longest_logged_out_run(self):
        self.assertEqual(cr.gap_stats([(0, True), (1, True)])["longest_gap_s"], 0)
        st = cr.gap_stats([(0, True), (1.0, False), (1.4, False), (1.9, True), (3, False), (3.2, True)])
        self.assertEqual((st["n_logged_out"], st["longest_gap_s"]), (3, 0.9))
        # Still logged out at the end: the gap runs to the last sample.
        self.assertEqual(cr.gap_stats([(0, True), (2, False), (5, False)])["longest_gap_s"], 3)

    def test_auth_lost_reads_only_the_tail(self):
        err = "  \u23bf  Not logged in \u00b7 Please run /login"
        self.assertTrue(cr.auth_lost("work\n" + err + "\n\n> "))
        # The same words higher up (a session discussing it) are not a dead turn.
        self.assertFalse(cr.auth_lost(err + "\n" + "\n".join("line %d" % i for i in range(20))))
        self.assertFalse(cr.auth_lost("all good\n> "))

    def test_statusline_ignores_readings_right_after_switch(self):
        setup({}, current="ethan@x.com")
        cr.save(cr.STATE, {"switched_at": time.time()})
        try: os.remove(os.path.join(cr.DIR, "last-record"))
        except OSError: pass
        sys.stdin = io.StringIO(json.dumps({"rate_limits": {"five_hour": {"used_percentage": 50, "resets_at": 1}}}))
        with redirect_stdout(io.StringIO()): cr.cmd_statusline()
        self.assertNotIn("ethan@x.com", cr.load(cr.ACCOUNTS, {"accounts": {}})["accounts"])

    def test_live_trigger_fires_once_past_threshold(self):
        calls = []
        real = cr.subprocess.Popen
        cr.subprocess.Popen = lambda *a, **k: calls.append(a)
        try:
            for f in ("last-trigger",):
                try: os.remove(os.path.join(cr.DIR, f))
                except OSError: pass
            # Fires at MIN_HEADROOM (4%) remaining, i.e. 96% used.
            self.assertFalse(cr.trigger_if_exhausted("x", {"used_percentage": 10}, {"used_percentage": 95}))
            self.assertTrue(cr.trigger_if_exhausted("x", {"used_percentage": 10}, {"used_percentage": 96.5}))
            self.assertFalse(cr.trigger_if_exhausted("x", {"used_percentage": 10}, {"used_percentage": 99}))
            self.assertEqual(len(calls), 1)
            self.assertIn("tick", calls[0][0])
        finally:
            cr.subprocess.Popen = real

    def test_parse_usage_screen(self):
        screen = """   Current session
   ██████████████▍                                    29% used
   Resets 4:20pm (America/New_York)
   Current week (all models)
   ████████████████████████████████████████████████▌  97% used
   Resets Oct 16 at 3am (America/New_York)
   Current week (Fable)
                                                      0% used
"""
        at = dt.datetime(2026, 10, 15, 12, 30).astimezone()
        got = cr.parse_usage(screen, at)
        self.assertEqual(got["five_hour"]["used_percentage"], 29)
        self.assertEqual(got["five_hour"]["resets_at"][:16], "2026-10-15T16:20")
        self.assertEqual(got["seven_day"]["used_percentage"], 97)
        self.assertEqual(got["seven_day"]["resets_at"][:16], "2026-10-16T03:00")

    def test_parse_reset_time_only_rolls_to_tomorrow(self):
        at = dt.datetime(2026, 10, 15, 23, 0).astimezone()
        self.assertEqual(cr.parse_reset("Resets 1:10am (America/New_York)", at)[:16], "2026-10-16T01:10")

    def test_new_login_restarts_settle_window(self):
        setup({}, current="ethan@a.com")
        cr.save(cr.STATE, {"switched_at": 0})
        try: os.remove(os.path.join(cr.DIR, "seen-login.json"))
        except OSError: pass
        try: os.remove(os.path.join(cr.DIR, "last-record"))
        except OSError: pass
        sys.stdin = io.StringIO(json.dumps({"rate_limits": {"seven_day": {"used_percentage": 99, "resets_at": 1}}}))
        with redirect_stdout(io.StringIO()): cr.cmd_statusline()
        self.assertNotIn("ethan@a.com", cr.load(cr.ACCOUNTS, {"accounts": {}})["accounts"])

class Logins(unittest.TestCase):
    """2026-10-20: a fresh account with a signed-in config folder went to the
    browser 47 times while the browser could not be reached."""

    def setUp(self):
        self.kc = {}
        self.saved = {k: getattr(cr, k) for k in ("_kc_read", "_kc_write", "cli_email", "probe", "resume_stranded",
                                                   "resume_auth_lost", "ACCOUNTS_DIR", "relay_upstream")}
        self.real_run, self.real_popen = cr.subprocess.run, cr.subprocess.Popen
        cr._kc_read = lambda svc, acct: self.kc.get((svc, acct))
        cr._kc_write = lambda svc, acct, v: self.kc.__setitem__((svc, acct), v) or True
        cr.cli_email = lambda: (cr.load(cr.CLAUDE_JSON, {}).get("oauthAccount") or {}).get("emailAddress")
        cr.probe = lambda: {"five_hour": 0}
        cr.resume_stranded = lambda force=False: []
        cr.resume_auth_lost = lambda: []
        cr.ACCOUNTS_DIR = tempfile.mkdtemp()
        cr.save(cr.STATE, {})
        json.dump({"oauthAccount": {"emailAddress": "spent@x.com"}}, open(cr.CLAUDE_JSON, "w"))
        self.kc[(cr.CRED_SVC, cr.CRED_ACCT)] = json.dumps({"claudeAiOauth": {"refreshToken": "spent-rt"}})

    def tearDown(self):
        for k, v in self.saved.items():
            setattr(cr, k, v)
        cr.subprocess.run, cr.subprocess.Popen = self.real_run, self.real_popen

    def folder(self, name, email, rt="fresh-rt"):
        d = os.path.join(cr.ACCOUNTS_DIR, name)
        os.makedirs(d)
        json.dump({"oauthAccount": {"emailAddress": email}}, open(os.path.join(d, ".claude.json"), "w"))
        self.kc[(cr._folder_service(d), cr.CRED_ACCT)] = json.dumps({"claudeAiOauth": {"refreshToken": rt}})

    def test_folder_service_matches_the_cli(self):
        # Observed on this Mac: CLAUDE_CONFIG_DIR=~/.amux/claude-accounts/amux
        # stores its login as "Claude Code-credentials-02d1cc0b".
        self.assertEqual(cr._folder_service("/Users/ethan/.amux/claude-accounts/amux"),
                         "Claude Code-credentials-02d1cc0b")
        real = cr.CRED_SVC
        try:
            cr.CRED_SVC = "Claude Code-credentials-sandbox"  # an override must not move folder logins
            self.assertEqual(cr._folder_service("/Users/ethan/.amux/claude-accounts/amux"),
                             "Claude Code-credentials-02d1cc0b")
        finally:
            cr.CRED_SVC = real

    def test_switch_uses_the_account_folder_when_nothing_is_stored(self):
        self.folder("usemixpeek", "fresh@x.com")
        self.assertEqual(cr.login_coverage(["fresh@x.com", "none@x.com"]),
                         {"fresh@x.com": "account_folder", "none@x.com": "browser"})
        with redirect_stdout(io.StringIO()):
            rc = cr.stored_switch("fresh@x.com", "spent@x.com", "current_exhausted", time.time())
        self.assertEqual(rc, 0)
        self.assertEqual(cr.cli_email(), "fresh@x.com")
        cred = json.loads(self.kc[(cr.CRED_SVC, cr.CRED_ACCT)])
        self.assertEqual(cred["claudeAiOauth"]["refreshToken"], "fresh-rt")
        switched = [json.loads(l) for l in open(cr.LOG) if '"claude_rotate_switched"' in l][-1]
        self.assertEqual((switched["to"], switched["via"]), ("fresh@x.com", "account_folder"))

    def test_failed_folder_login_restores_the_previous_one(self):
        self.folder("dead", "dead@x.com")
        cr.probe = lambda: None
        with redirect_stdout(io.StringIO()):
            self.assertIsNone(cr.stored_switch("dead@x.com", "spent@x.com", "current_exhausted", time.time()))
        self.assertEqual(cr.cli_email(), "spent@x.com")
        self.assertEqual(json.loads(self.kc[(cr.CRED_SVC, cr.CRED_ACCT)])["claudeAiOauth"]["refreshToken"], "spent-rt")

    def test_unreachable_browser_fails_fast_and_alerts_once(self):
        calls = []
        cr.relay_upstream = lambda: "awaiting_approval"
        cr.subprocess.Popen = lambda args, **kw: calls.append(list(args))
        cr.subprocess.run = lambda args, **kw: calls.append(list(args)) or type("P", (), {"returncode": 0})()
        t = time.time()
        with redirect_stdout(io.StringIO()):
            self.assertEqual(cr._switch("nologin@x.com", "current_exhausted"), 1)
            self.assertEqual(cr._switch("nologin@x.com", "current_exhausted"), 1)
        self.assertLess(time.time() - t, 5, "no 6-minute sign-in wait")
        self.assertFalse(any("auth" in c for c in calls), "the CLI sign-in never starts: %r" % calls)
        self.assertEqual(sum(1 for c in calls if c[:2] == ["amux", "alert"]), 1, calls)
        self.assertNotIn("nologin@x.com", cr.load(cr.STATE, {}).get("switch_failures") or {},
                         "the browser's outage is not the account's failure")

    def test_unreachable_browser_does_not_alert_for_a_preempt(self):
        calls = []
        cr.relay_upstream = lambda: "disconnected"
        cr.subprocess.Popen = lambda args, **kw: calls.append(list(args))
        cr.subprocess.run = lambda args, **kw: calls.append(list(args)) or type("P", (), {"returncode": 0})()
        with redirect_stdout(io.StringIO()):
            self.assertEqual(cr._switch("nologin@x.com", "preempt_limit"), 1)
        self.assertFalse(any(c[:2] == ["amux", "alert"] for c in calls))


class FakeAmux(BaseHTTPRequestHandler):
    have, posted = {}, []

    def log_message(self, *a):
        pass

    def _reply(self, body):
        data = json.dumps(body).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)

    def do_GET(self):
        self._reply({"measured": True, "accounts": [{"account": a, "latest": {"at": at}} for a, at in FakeAmux.have.items()]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        FakeAmux.posted.append(body)
        self._reply({"ok": True, "recorded": True})


class UsageLog(unittest.TestCase):
    """Readings reach the amux DB history (POST /api/usage/accounts/readings)."""

    def setUp(self):
        cr.subprocess.run, cr.subprocess.Popen = REAL_RUN, REAL_POPEN
        self.srv = HTTPServer(("127.0.0.1", 0), FakeAmux)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        os.environ["CLAUDE_ROTATE_AMUX_URL"] = "http://127.0.0.1:%d" % self.srv.server_port
        cr.USAGE_LOG = "on"
        FakeAmux.have, FakeAmux.posted = {}, []
        open(cr.LOG, "w").close()

    def tearDown(self):
        cr.USAGE_LOG = "off"
        os.environ.pop("CLAUDE_ROTATE_AMUX_URL", None)
        self.srv.shutdown(); self.srv.server_close()

    def verdicts(self):
        return [json.loads(l) for l in open(cr.LOG) if l.strip()]

    def test_an_accepted_reading_is_posted_with_its_observation_time(self):
        setup({})
        self.assertTrue(cr.record("a@x.com", five=12, five_reset=r(3), week=34, week_reset=r(90), source="usage-probe"))
        self.assertEqual(len(FakeAmux.posted), 1)
        body = FakeAmux.posted[0]
        acct = cr.load(cr.ACCOUNTS, {})["accounts"]["a@x.com"]
        self.assertEqual((body["account"], body["source"], body["five_hour_pct"], body["seven_day_pct"]),
                         ("a@x.com", "usage-probe", 12, 34))
        self.assertEqual(body["at"], dt.datetime.fromisoformat(acct["observed_at"]).timestamp())
        self.assertEqual(body["seven_day_resets_at"], acct["seven_day"]["resets_at"])

    def test_sync_backfills_only_what_the_db_lacks(self):
        fresh = dict(acct(5, r(2), 20, r(80)), observed_at=r(0), source="usage-probe")
        stale = dict(acct(6, r(2), 30, r(80)), observed_at=r(0), source="statusline")
        setup({"have@x.com": fresh, "miss@x.com": stale, "never@x.com": {}})
        FakeAmux.have = {"have@x.com": dt.datetime.fromisoformat(fresh["observed_at"]).timestamp()}
        out = cr.usage_log_sync()
        self.assertEqual(out["added"], ["miss@x.com"])
        self.assertEqual([b["account"] for b in FakeAmux.posted], ["miss@x.com"])
        v = [x for x in self.verdicts() if x["verdict"] == "claude_rotate_usage_log_backfilled"]
        self.assertEqual(v[-1]["added"], ["miss@x.com"])
        # Nothing to do on the next tick once the DB has it: no post, no log line.
        FakeAmux.have["miss@x.com"] = dt.datetime.fromisoformat(stale["observed_at"]).timestamp()
        FakeAmux.posted = []
        self.assertEqual(cr.usage_log_sync()["added"], [])
        self.assertEqual(FakeAmux.posted, [])

    def test_unreachable_server_is_logged(self):
        os.environ["CLAUDE_ROTATE_AMUX_URL"] = "http://127.0.0.1:9"
        self.assertIsNone(cr.usage_log_sync())
        self.assertEqual(self.verdicts()[-1]["verdict"], "claude_rotate_usage_log_unreachable")



class FileKeystore(unittest.TestCase):
    """Linux has no Keychain (mixpeek-dev.amux.io, 2026-10-31): the CLI's login is
    <CLAUDE_CONFIG_DIR or ~/.claude>/.credentials.json and snapshots are 0600 files."""
    def setUp(self):
        self.saved = {k: getattr(cr, k) for k in ("USE_KEYCHAIN", "HOME", "KEYRING")}
        self.home = tempfile.mkdtemp()
        cr.USE_KEYCHAIN, cr.HOME, cr.KEYRING = False, self.home, os.path.join(self.home, "keyring")
        os.environ.pop("CLAUDE_CONFIG_DIR", None)

    def tearDown(self):
        for k, v in self.saved.items():
            setattr(cr, k, v)

    def test_the_cli_login_is_its_credentials_file(self):
        login = json.dumps({"claudeAiOauth": {"refreshToken": "PLACEHOLDER_RT"}, "mcpOAuth": {"x": 1}})
        self.assertTrue(cr._kc_write("Claude Code-credentials", cr.CRED_ACCT, login))
        path = os.path.join(self.home, ".claude", ".credentials.json")
        self.assertEqual(open(path).read(), login, "written where Claude Code on Linux reads it")
        self.assertEqual(cr._kc_read("Claude Code-credentials", cr.CRED_ACCT), login)
        self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)

    def test_a_snapshot_is_a_private_file_per_account(self):
        self.assertIsNone(cr._kc_read(cr.STORE_SVC, "a@x.io"))
        self.assertTrue(cr._kc_write(cr.STORE_SVC, "a@x.io", '{"claudeAiOauth": {}}'))
        self.assertEqual(cr._kc_read(cr.STORE_SVC, "a@x.io"), '{"claudeAiOauth": {}}')
        f = os.path.join(cr.KEYRING, cr.STORE_SVC, "a@x.io.json")
        self.assertEqual(os.stat(f).st_mode & 0o777, 0o600)
        self.assertIsNone(cr._kc_read(cr.STORE_SVC, "b@x.io"), "another account is its own file")

    def test_an_account_folder_login_is_read_from_its_own_credentials_file(self):
        acc = os.path.join(self.home, "accounts")
        folder = os.path.join(acc, "use")
        os.makedirs(folder)
        json.dump({"oauthAccount": {"emailAddress": "Use@X.io"}}, open(os.path.join(folder, ".claude.json"), "w"))
        json.dump({"claudeAiOauth": {"refreshToken": "PLACEHOLDER_FOLDER_RT"}}, open(os.path.join(folder, ".credentials.json"), "w"))
        saved_dir = cr.ACCOUNTS_DIR
        cr.ACCOUNTS_DIR = acc
        try:
            got = cr.folder_login("use@x.io")
        finally:
            cr.ACCOUNTS_DIR = saved_dir
        self.assertEqual(got["claudeAiOauth"]["refreshToken"], "PLACEHOLDER_FOLDER_RT")
        self.assertEqual(got["folder"], folder)

if __name__ == "__main__":
    unittest.main(verbosity=1)
