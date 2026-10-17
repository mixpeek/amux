"""Policy tests for scripts/claude-rotate.py. Run: python3 scripts/tests/test_claude_rotate.py"""
import datetime as dt, importlib.util, io, json, os, sys, tempfile, time, unittest
from contextlib import redirect_stdout

TMP = tempfile.mkdtemp()
os.environ.update(CLAUDE_ROTATE_DIR=TMP, CLAUDE_ROTATE_LOG=os.path.join(TMP, "log"),
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
            self.assertFalse(cr.trigger_if_exhausted("x", {"used_percentage": 10}, {"used_percentage": 96}))
            self.assertTrue(cr.trigger_if_exhausted("x", {"used_percentage": 10}, {"used_percentage": 97.5}))
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

if __name__ == "__main__":
    unittest.main(verbosity=1)
