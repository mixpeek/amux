"""A second factor at the door (governance PR 20): `door.py totp <user>` turns one on; the form then
needs the current code, a code is good once, Basic carries it after the password, and
DOOR_MFA=required refuses an account with no factor. The code is computed here the way an
authenticator app does.

  python3 -m unittest cloud/desktop/test_door_totp.py
"""
import base64, hashlib, hmac, os, socket, subprocess, sys, tempfile, time, unittest, urllib.error, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
OWNER, OWNER_PW = "owner@example.com", "owner-password-1"
OTHER, OTHER_PW = "other@example.com", "other-password-1"


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


OPEN = urllib.request.build_opener(NoRedirect)


def totp(secret, t):
    pad = "=" * (-len(secret) % 8)
    h = hmac.new(base64.b32decode(secret + pad), int(t // 30).to_bytes(8, "big"), hashlib.sha1).digest()
    o = h[-1] & 15
    return "%06d" % ((int.from_bytes(h[o:o + 4], "big") & 0x7FFFFFFF) % 1000000)


class Door:
    def __init__(self, mfa):
        self.tmp = tempfile.mkdtemp()
        self.port = free_port()
        self.etc = etc = os.path.join(self.tmp, "etc"); os.makedirs(etc)
        with open(os.path.join(etc, "secret"), "w") as f:
            f.write("door-test-secret")
        with open(os.path.join(etc, "users"), "w") as f:
            for u, pw in ((OWNER, OWNER_PW), (OTHER, OTHER_PW)):
                f.write(subprocess.run([sys.executable, os.path.join(HERE, "door.py"), "hash", u], input=pw + "\n",
                                       capture_output=True, text=True, check=True).stdout)
        for n in ("members", "joined"):
            open(os.path.join(etc, n), "w").close()
        self.env = dict(os.environ, DOOR_PORT=str(self.port), DOOR_USERS=os.path.join(etc, "users"),
                        DOOR_SECRET=os.path.join(etc, "secret"), DOOR_MEMBERS=os.path.join(etc, "members"),
                        DOOR_JOINED=os.path.join(etc, "joined"), DOOR_FLEET_ENV=os.path.join(etc, "none"),
                        DOOR_ENTRY=os.path.join(etc, "none"), DOOR_ACTIVITY=os.path.join(etc, "activity.json"),
                        DOOR_UPDATE_STATUS=os.path.join(etc, "status.json"), DOOR_TOTP=os.path.join(etc, "totp"), DOOR_ALLOWLIST=os.path.join(etc, "allowlist.json"),
                        DOOR_FAIL_LIMIT="50", DOOR_MFA=mfa)
        self.log = os.path.join(self.tmp, "door.log")
        self.proc = subprocess.Popen([sys.executable, os.path.join(HERE, "door.py"), "serve"], env=self.env,
                                     stdout=open(self.log, "w"), stderr=subprocess.STDOUT)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=0.2).close(); break
            except OSError:
                time.sleep(0.1)

    def enroll(self, user):
        uri = subprocess.run([sys.executable, os.path.join(HERE, "door.py"), "totp", user], env=self.env,
                             capture_output=True, text=True, check=True).stdout.strip()
        return urllib.parse.parse_qs(urllib.parse.urlsplit(uri).query)["secret"][0], uri

    def stop(self):
        self.proc.terminate(); self.proc.wait(timeout=10)

    def req(self, path, data=None, headers=None):
        r = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}", data=data, headers={"X-Real-IP": "203.0.113.5", **(headers or {})})
        try:
            with OPEN.open(r, timeout=10) as resp:
                return resp.status, resp.read().decode(errors="replace")
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode(errors="replace")

    def login(self, user, pw, code=""):
        return self.req("/_door/login", data=urllib.parse.urlencode({"username": user, "password": pw, "code": code, "next": "/"}).encode())

    def basic(self, user, pw):
        auth = "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode()
        return self.req("/_door/check", headers={"Authorization": auth, "X-Forwarded-Uri": "/api/sessions"})[0]


class Optional(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.d = Door("optional")

    @classmethod
    def tearDownClass(cls):
        cls.d.stop()

    def test_an_account_with_a_factor_needs_the_code_once(self):
        d = self.d
        self.assertIn("one-time-code", d.req("/_door/login")[1], "the form asks for a code")
        secret, uri = d.enroll(OWNER)
        self.assertTrue(uri.startswith("otpauth://totp/amux:owner%40example.com?secret="))
        mode = os.stat(os.path.join(d.etc, "totp")).st_mode & 0o777
        self.assertEqual(mode, 0o600)
        status, body = d.login(OWNER, OWNER_PW)
        self.assertEqual(status, 401)
        self.assertIn("authenticator", body)
        self.assertEqual(d.login(OWNER, OWNER_PW, "000000")[0], 401)
        now = time.time()
        code = totp(secret, now)
        self.assertEqual(d.login(OWNER, "wrong-password", code)[0], 401, "a code does not stand in for the password")
        self.assertEqual(d.login(OWNER, OWNER_PW, code)[0], 303)
        self.assertEqual(d.login(OWNER, OWNER_PW, code)[0], 401, "a code is good once")
        # Basic: the code rides after the password.
        nxt = totp(secret, now + 30)
        self.assertEqual(d.basic(OWNER, OWNER_PW), 401)
        self.assertEqual(d.basic(OWNER, f"{OWNER_PW}:{nxt}"), 200)
        with open(d.log) as f:
            log = f.read()
        self.assertIn("door_signin_second_factor_refused", log)
        self.assertIn("door_basic_second_factor_refused", log)

    def test_an_account_without_one_is_not_asked(self):
        self.assertEqual(self.d.login(OTHER, OTHER_PW)[0], 303)
        self.assertEqual(self.d.basic(OTHER, OTHER_PW), 200)


class Required(unittest.TestCase):
    def test_required_refuses_an_account_with_no_factor(self):
        d = Door("required")
        try:
            status, body = d.login(OTHER, OTHER_PW)
            self.assertEqual(status, 401)
            self.assertIn("requires a second factor", body)
            self.assertEqual(d.basic(OTHER, OTHER_PW), 401)
            secret, _ = d.enroll(OTHER)
            self.assertEqual(d.login(OTHER, OTHER_PW, totp(secret, time.time()))[0], 303)
        finally:
            d.stop()


if __name__ == "__main__":
    unittest.main()
