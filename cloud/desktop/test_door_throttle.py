"""The door refuses an address after repeated wrong passwords, by form and by Basic, without
locking anyone else out (2026-10-31: Basic on any URL had no limit at all).

  python3 -m unittest cloud/desktop/test_door_throttle.py
"""
import base64, os, socket, subprocess, sys, tempfile, time, unittest, urllib.error, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
OWNER, OWNER_PW = "owner@example.com", "owner-password-1"


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


OPEN = urllib.request.build_opener(NoRedirect)


class DoorThrottle(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp()
        cls.port = free_port()
        etc = os.path.join(cls.tmp, "etc"); os.makedirs(etc)
        with open(os.path.join(etc, "secret"), "w") as f:
            f.write("door-test-secret")
        line = subprocess.run([sys.executable, os.path.join(HERE, "door.py"), "hash", OWNER], input=OWNER_PW + "\n",
                              capture_output=True, text=True, check=True).stdout
        with open(os.path.join(etc, "users"), "w") as f:
            f.write(line)
        for n in ("members", "joined"):
            open(os.path.join(etc, n), "w").close()
        cls.log = os.path.join(cls.tmp, "door.log")
        cls.proc = subprocess.Popen([sys.executable, os.path.join(HERE, "door.py"), "serve"], env=dict(
            os.environ, DOOR_PORT=str(cls.port), DOOR_USERS=os.path.join(etc, "users"), DOOR_SECRET=os.path.join(etc, "secret"),
            DOOR_MEMBERS=os.path.join(etc, "members"), DOOR_JOINED=os.path.join(etc, "joined"),
            DOOR_FLEET_ENV=os.path.join(etc, "none"), DOOR_ENTRY=os.path.join(etc, "none"),
            DOOR_ACTIVITY=os.path.join(etc, "activity.json"), DOOR_UPDATE_STATUS=os.path.join(etc, "status.json"),
            DOOR_FAIL_LIMIT="3", DOOR_FAIL_WINDOW_S="600"),
            stdout=open(cls.log, "w"), stderr=subprocess.STDOUT)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", cls.port), timeout=0.2).close(); break
            except OSError:
                time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate(); cls.proc.wait(timeout=10)

    def req(self, path, ip, data=None, headers=None):
        h = {"X-Real-IP": ip, **(headers or {})}
        r = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}", data=data, headers=h)
        try:
            with OPEN.open(r, timeout=10) as resp:
                return resp.status, resp.read().decode(errors="replace")
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode(errors="replace")

    def basic(self, ip, pw):
        auth = "Basic " + base64.b64encode(f"{OWNER}:{pw}".encode()).decode()
        return self.req("/_door/check", ip, headers={"Authorization": auth, "X-Forwarded-Uri": "/api/sessions"})[0]

    def login(self, ip, pw):
        data = urllib.parse.urlencode({"username": OWNER, "password": pw, "next": "/"}).encode()
        return self.req("/_door/login", ip, data=data)

    def test_basic_guessing_is_refused_and_other_addresses_are_not(self):
        ip = "203.0.113.10"
        self.assertEqual([self.basic(ip, "wrong-%d" % i) for i in range(3)], [401, 401, 401])
        self.assertEqual(self.basic(ip, OWNER_PW), 429)            # even the right password, from that address
        self.assertEqual(self.basic("203.0.113.11", OWNER_PW), 200)  # the owner elsewhere is unaffected
        with open(self.log) as f:
            self.assertIn("door_signin_throttled", f.read())

    def test_form_guessing_is_refused(self):
        ip = "203.0.113.20"
        for i in range(3):
            self.assertEqual(self.login(ip, "wrong-%d" % i)[0], 401)
        status, body = self.login(ip, OWNER_PW)
        self.assertEqual(status, 401)
        self.assertIn("Too many wrong passwords", body)
        self.assertEqual(self.login("203.0.113.21", OWNER_PW)[0], 303)

    def test_failures_by_form_and_basic_share_one_count(self):
        ip = "203.0.113.30"
        self.login(ip, "wrong-a"); self.basic(ip, "wrong-b"); self.login(ip, "wrong-c")
        self.assertEqual(self.basic(ip, OWNER_PW), 429)


if __name__ == "__main__":
    unittest.main()
