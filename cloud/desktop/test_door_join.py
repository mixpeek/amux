"""Standalone amux Cloud server: the owner and teammates at the door, against a real amux-server.

The paying customer is the host owner (AMUX_WEB_USERS ...:owner, no member token), so amux lets
them administer the organization. A teammate opens the owner's invite link, picks a password at
/_door/join, and is then a revocable member with their own login (codex e2e C8-1).

Needs AMUX_SERVER_BIN (an amux-server binary); without it every test fails loudly as "not run".

  AMUX_SERVER_BIN=~/.local/bin/amux-server-rs python3 -m unittest cloud/desktop/test_door_join.py
"""
import base64, http.client, json, os, socket, ssl, subprocess, sys, tempfile, time, unittest, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.expanduser(os.environ.get("AMUX_SERVER_BIN", ""))
CTX = ssl.create_default_context()
CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
OWNER, OWNER_PW = "owner@example.com", "owner-password-1"


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class DoorJoin(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BIN or not os.path.exists(BIN):
            raise RuntimeError("not run: set AMUX_SERVER_BIN to an amux-server binary")
        cls.tmp = tempfile.mkdtemp()
        cls.amux_port, cls.door_port = free_port(), free_port()
        home = os.path.join(cls.tmp, "home"); os.makedirs(home)
        cls.amux = subprocess.Popen([BIN], env={"HOME": home, "PATH": os.environ["PATH"], "AMUX_HOME": os.path.join(cls.tmp, "amux"),
                                    "AMUX_RS_PORT": str(cls.amux_port), "AMUX_NO_SELF_ADOPT": "1"},
                                    stdout=open(os.path.join(cls.tmp, "amux.log"), "w"), stderr=subprocess.STDOUT)
        for _ in range(120):
            try:
                urllib.request.urlopen(f"https://127.0.0.1:{cls.amux_port}/health", context=CTX, timeout=2); break
            except Exception:
                time.sleep(0.5)
        etc = os.path.join(cls.tmp, "etc"); os.makedirs(etc)
        with open(os.path.join(etc, "secret"), "w") as f:
            f.write("door-test-secret")
        line = subprocess.run([sys.executable, os.path.join(HERE, "door.py"), "hash", OWNER], input=OWNER_PW + "\n",
                              capture_output=True, text=True, check=True).stdout
        with open(os.path.join(etc, "users"), "w") as f:
            f.write(line)                                    # the owner: a password, and no member token
        cls.entry_secret = "ab" * 32                          # what cloud.amux.io sends in user_data (bootstrap.sh)
        with open(os.path.join(etc, "entry"), "w") as f:
            f.write(f"AMUX_SLUG=acme\nAMUX_FLEET_SECRET={cls.entry_secret}\n")
        cls.members, cls.joined = os.path.join(etc, "members"), os.path.join(etc, "joined")
        open(cls.members, "w").close()
        cls.door_proc = subprocess.Popen([sys.executable, os.path.join(HERE, "door.py"), "serve"], env=dict(os.environ,
                                    DOOR_PORT=str(cls.door_port), DOOR_USERS=os.path.join(etc, "users"), DOOR_SECRET=os.path.join(etc, "secret"),
                                    DOOR_MEMBERS=cls.members, DOOR_JOINED=cls.joined, DOOR_FLEET_ENV=os.path.join(etc, "none"), DOOR_ENTRY=os.path.join(etc, "entry"),
                                    DOOR_UPDATE_STATUS=os.path.join(etc, "status.json"),
                                    DOOR_AMUX_URL=f"https://127.0.0.1:{cls.amux_port}", DOOR_MEMBER_RECHECK="0.5"),
                                    stdout=open(os.path.join(cls.tmp, "door.log"), "w"), stderr=subprocess.STDOUT)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", cls.door_port), timeout=0.2).close(); break
            except OSError:
                time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        for p in (getattr(cls, "door_proc", None), getattr(cls, "amux", None)):
            if p:
                p.terminate(); p.wait(timeout=10)

    def amux_as(self, member, method, path, body=None):
        """A request the way Caddy forwards it: the member cookie is the door's X-Amux-Member, empty for the owner."""
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(f"https://127.0.0.1:{self.amux_port}{path}", data=data, method=method,
                                     headers={"Cookie": f"amux_member={member or ''}", "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, context=CTX, timeout=10) as r:
                return r.status, json.loads(r.read() or b"null")
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def door(self, method, path, headers=None, form=None):
        c = http.client.HTTPConnection("127.0.0.1", self.door_port, timeout=15)
        body = urllib.parse.urlencode(form).encode() if form else None
        h = dict(headers or {}, **({"Content-Type": "application/x-www-form-urlencoded"} if form else {}))
        c.request(method, path, body=body, headers=h)
        r = c.getresponse(); data = r.read()
        return r.status, r, data

    def check(self, cookie=None, basic=None, uri="/api/sessions", accept="application/json"):
        h = {"Accept": accept, "X-Forwarded-Method": "GET", "X-Forwarded-Uri": uri}
        if cookie:
            h["Cookie"] = cookie
        if basic:
            h["Authorization"] = "Basic " + base64.b64encode(basic.encode()).decode()
        code, r, _ = self.door("GET", "/_door/check", h)
        return code, r.getheader("X-Door-User"), r.getheader("X-Amux-Member"), r.getheader("Location")

    def test_1_owner_has_no_member_token_and_administers_the_org(self):
        code, user, member, _ = self.check(basic=f"{OWNER}:{OWNER_PW}")
        self.assertEqual((code, user, member), (200, OWNER, None))
        self.assertEqual(self.amux_as(member, "GET", "/api/org/invites")[0], 200)
        self.assertEqual(self.check(basic=f"{OWNER}:wrong-password")[0], 401)

    def test_2_teammate_joins_with_own_password_then_is_revoked(self):
        code, d = self.amux_as(None, "POST", "/api/org/invites", {"email": "dana@example.com", "scope_level": "global", "grant_host_access": True})
        self.assertIn(code, (200, 201), d)
        inv = d.get("token") or d.get("url", "").rsplit("/", 1)[-1]
        # The invite link, opened signed out, goes to the join page.
        code, _, _, loc = self.check(uri=f"/invite/{inv}", accept="text/html")
        self.assertEqual((code, loc), (302, f"/_door/join?invite={inv}"))
        code, _, page = self.door("GET", f"/_door/join?invite={inv}")
        self.assertEqual(code, 200); self.assertIn(b'name="password2"', page)
        # Mismatched passwords, and a forged invite, are refused without creating anything.
        self.assertEqual(self.door("POST", "/_door/join", form={"username": "dana@example.com", "password": "dana-password-1",
                                                                  "password2": "other-password", "invite": inv})[0], 400)
        self.assertEqual(self.door("POST", "/_door/join", form={"username": "dana@example.com", "password": "dana-password-1",
                                                                  "password2": "dana-password-1", "invite": "forged"})[0], 400)
        code, r, _ = self.door("POST", "/_door/join", form={"username": "Dana@example.com", "password": "dana-password-1",
                                                             "password2": "dana-password-1", "invite": inv})
        self.assertEqual(code, 303)
        cookie = "; ".join(v.split(";")[0] for k, v in r.getheaders() if k.lower() == "set-cookie")
        code, user, member, _ = self.check(cookie=cookie)
        self.assertEqual((code, user, member), (200, "dana@example.com", inv))
        self.assertEqual(self.check(basic="dana@example.com:dana-password-1")[0], 200)     # her own login, e.g. the CLI
        self.assertEqual(self.amux_as(member, "GET", "/api/org/invites")[0], 403)           # a member, not the owner
        # The used invite cannot make a second account.
        self.assertEqual(self.door("POST", "/_door/join", form={"username": "eve@example.com", "password": "eve-password-12",
                                                                  "password2": "eve-password-12", "invite": inv})[0], 400)
        # The owner removes her in Settings: both her cookie and her password stop working.
        _, ms = self.amux_as(None, "GET", "/api/org/members")
        mid = next(m["id"] for m in (ms.get("members") if isinstance(ms, dict) else ms) if m["email"] == "dana@example.com")
        self.assertIn(self.amux_as(None, "DELETE", f"/api/org/members/{mid}")[0], (200, 204))
        time.sleep(1.0)
        self.assertEqual(self.check(cookie=cookie)[0], 401)
        self.assertEqual(self.check(basic="dana@example.com:dana-password-1")[0], 401)

    def test_3_status_page_reports_dashboard_and_commit(self):
        code, _, body = self.door("GET", "/_door/status")
        st = json.loads(body)
        self.assertEqual(code, 200)
        self.assertTrue(st["live_commit"]); self.assertIs(st["dashboard"], True); self.assertEqual(st["verdict"], "unknown")


    def entry(self, sub, aud="acme", exp=None, secret=None):
        """A sign-in link token signed the way cloud/start/worker.mjs entryLink signs it."""
        import hashlib, hmac
        b64 = lambda x: base64.urlsafe_b64encode(x).decode().rstrip("=")
        body = b64(json.dumps({"sub": sub, "aud": aud, "exp": exp or time.time() + 120, "via": "apple"}).encode())
        return body + "." + b64(hmac.new((secret or self.entry_secret).encode(), body.encode(), hashlib.sha256).digest())

    def test_4_owner_signs_in_from_the_app_link(self):
        code, r, _ = self.door("GET", "/_door/enter?t=" + self.entry(OWNER))
        self.assertEqual((code, r.getheader("Location")), (303, "/"))
        cookie = "; ".join(v.split(";")[0] for k, v in r.getheaders() if k.lower() == "set-cookie")
        self.assertEqual(self.check(cookie=cookie)[:3], (200, OWNER, None))
        # Expired, forged, another server's, and a stranger's links do not sign anyone in.
        for t in (self.entry(OWNER, exp=time.time() - 1), self.entry(OWNER, secret="cd" * 32),
                  self.entry(OWNER, aud="other"), self.entry("stranger@example.com")):
            code, r, _ = self.door("GET", "/_door/enter?t=" + t)
            self.assertNotIn("amux_door", r.getheader("Set-Cookie") or "", t)
            self.assertNotEqual(code, 303, t)


if __name__ == "__main__":
    unittest.main()
