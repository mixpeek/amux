"""The door's activation report: token scope, counts only, honest unmeasured sections.

  python3 -m unittest cloud/desktop/test_door_report.py

Needs no amux-server: amux's answers are faked through activation_report(get=...).
"""
import base64, hashlib, hmac, json, os, sys, tempfile, time, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
TMP = tempfile.mkdtemp()
os.environ.update(DOOR_ENTRY=os.path.join(TMP, "entry"), DOOR_FLEET_ENV=os.path.join(TMP, "none"),
                  DOOR_ACTIVITY=os.path.join(TMP, "activity.json"))
sys.path.insert(0, HERE)
import door  # noqa: E402

KEY = "ab" * 32   # a fake per-server key, the shape entryKey() produces
with open(os.environ["DOOR_ENTRY"], "w") as f:
    f.write("AMUX_SLUG=acme\nAMUX_FLEET_SECRET=%s\n" % KEY)


def token(claims, key=KEY):
    b = base64.urlsafe_b64encode(json.dumps(claims).encode()).rstrip(b"=").decode()
    mac = base64.urlsafe_b64encode(hmac.new(key.encode(), b.encode(), hashlib.sha256).digest()).rstrip(b"=").decode()
    return b + "." + mac


SECRET_BITS = ["fix-the-login-bug", "customer@example.com", "/home/amux/secret-project", "sk-fake-not-a-key", "Refactor billing"]


def fake_amux(path, timeout=10):
    return {
        "/api/identity": {"email": SECRET_BITS[1], "model_auth": {"claude": {"api_key": False, "subscription_token": False, "oauth": True},
                                                                   "openai": {"api_key": False, "codex_oauth": False}}},
        "/api/sessions": [{"name": SECRET_BITS[0], "dir": SECRET_BITS[2], "preview": SECRET_BITS[3], "tokens": {"total": 500},
                           "last_activity": time.time() - 60, "running": True},
                          {"name": "idle-one", "tokens": {"total": 0}, "last_activity": time.time() - 60},
                          {"name": "old", "archived": True, "tokens": {"total": 9}}],
        "/api/stats/daily": {"total_tokens": 1234, "sessions": [{"name": SECRET_BITS[0]}]},
        "/api/board?all=1": [{"title": SECRET_BITS[4]}, {"title": "x", "archived": True}],
        "/api/schedules": [{"title": "nightly", "command": SECRET_BITS[3], "enabled": 1}, {"enabled": 0}],
        "/api/logs/analyze?since_h=24": {"measured": True, "groups_total": 3, "total_errors": 17, "n_considered": 17,
                                         "groups": [{"sample": SECRET_BITS[3]}]},
        "/health": {"commit": "abc123def"},
    }[path]


class Tokens(unittest.TestCase):
    def test_a_report_token_is_accepted_and_cannot_sign_anyone_in(self):
        t = token({"aud": "acme", "scope": "report", "exp": int(time.time()) + 120})
        self.assertEqual(door.verify_report_token(t)["scope"], "report")
        with self.assertRaisesRegex(ValueError, "not a sign-in token"):
            door.verify_entry_token(t)

    def test_a_sign_in_link_is_not_a_report_token(self):
        t = token({"sub": "a@b.co", "aud": "acme", "exp": int(time.time()) + 120, "via": "apple"})
        self.assertEqual(door.verify_entry_token(t)["sub"], "a@b.co")
        with self.assertRaisesRegex(ValueError, "not a report token"):
            door.verify_report_token(t)

    def test_wrong_key_wrong_server_expired_and_long_lived_are_refused(self):
        now = int(time.time())
        for claims, key, why in [({"aud": "acme", "scope": "report", "exp": now + 60}, "cd" * 32, "bad signature"),
                                 ({"aud": "other", "scope": "report", "exp": now + 60}, KEY, "wrong server"),
                                 ({"aud": "acme", "scope": "report", "exp": now - 1}, KEY, "expired"),
                                 ({"aud": "acme", "scope": "report", "exp": now + 3600}, KEY, "too long")]:
            with self.assertRaisesRegex(ValueError, why):
                door.verify_report_token(token(claims, key))


class Report(unittest.TestCase):
    def setUp(self):
        door._activity = None
        if os.path.exists(os.environ["DOOR_ACTIVITY"]):
            os.remove(os.environ["DOOR_ACTIVITY"])

    def test_counts_only_and_no_content_leaks(self):
        door.note_signin(False); door.note_signin(False); door.note_signin(True)
        r = door.activation_report(get=fake_amux)
        self.assertTrue(r["measured"], r["unmeasured"])
        self.assertEqual(r["n_considered"], len(door.REPORT_SECTIONS))
        self.assertEqual((r["signin"]["ever"], r["signin"]["count"], r["signin"]["failed_24h"]), (True, 1, 2))
        self.assertEqual(r["model"]["providers"], {"claude": True, "openai": False})
        self.assertTrue(r["model"]["any"])
        w = r["workers"]
        self.assertEqual((w["total"], w["ever_ran"], w["active_24h"], w["running"], w["n_considered"]), (2, 2, 1, 1, 3))
        self.assertEqual((r["board"]["cards"], r["schedules"]["total"], r["schedules"]["enabled"]), (1, 2, 1))
        self.assertEqual((r["errors"]["groups_24h"], r["tokens"]["today"], r["version"]["commit"]), (3, 1234, "abc123def"))
        self.assertGreater(r["disk"]["used_pct"], 0)
        text = json.dumps(r)
        for bit in SECRET_BITS:
            self.assertNotIn(bit, text)

    def test_a_probe_that_fails_says_so_instead_of_reading_zero(self):
        def broken(path, timeout=10):
            if path == "/api/sessions":
                raise ValueError("HTTP 502")
            return fake_amux(path)
        r = door.activation_report(get=broken)
        self.assertFalse(r["measured"])
        self.assertEqual(r["unmeasured"], ["workers"])
        self.assertEqual(r["workers"], {"measured": False, "why_unmeasured": "HTTP 502"})
        self.assertNotIn("ever_ran", r["workers"])

    def test_never_signed_in_and_counts_survive_a_restart(self):
        r = door.activation_report(get=fake_amux)
        self.assertEqual((r["signin"]["ever"], r["signin"]["last_at"]), (False, None))
        door.note_signin(True)
        door._activity = None   # a restart reads the file back
        self.assertEqual(door.activation_report(get=fake_amux)["signin"]["count"], 1)


if __name__ == "__main__":
    unittest.main()
