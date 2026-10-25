"""Door token mode against a real amux-server (T-SSO-1, T-TEAM-1, T-TEAM-4..6 at the door).

Needs AMUX_SERVER_BIN (an amux-server binary). Without it every test reports "not run" by
failing loudly rather than passing, so a missing binary never reads as green.

  AMUX_SERVER_BIN=~/.local/bin/amux-server-rs python3 -m unittest cloud/desktop/test_door_token.py
"""
import http.client, json, os, socket, ssl, subprocess, sys, tempfile, time, unittest, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "control"))
import tokens  # noqa: E402

BIN = os.path.expanduser(os.environ.get("AMUX_SERVER_BIN", ""))
CTX = ssl.create_default_context()
CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class DoorToken(unittest.TestCase):
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
        cls.secret, cls.slug = tokens.new_secret(), "alice"
        etc = os.path.join(cls.tmp, "etc"); os.makedirs(etc)
        with open(os.path.join(etc, "fleet.env"), "w") as f:
            f.write(f'AMUX_FLEET_SECRET="{cls.secret}"\nAMUX_SLUG="{cls.slug}"\nAMUX_CONTROL_URL="https://cloud.example"\n')
        with open(os.path.join(etc, "secret"), "w") as f:
            f.write("door-test-secret")
        cls.members = os.path.join(etc, "members")
        # The owner becomes a member the way bootstrap.sh does it: invite, accept, keep the token.
        owner_tok = cls.invite("owner@example.com")
        cls.accept(owner_tok, "owner@example.com")
        with open(cls.members, "w") as f:
            f.write(f"owner@example.com {owner_tok}\n")
        cls.door_proc = subprocess.Popen([sys.executable, os.path.join(HERE, "door.py"), "serve"], env=dict(os.environ,
                                    DOOR_PORT=str(cls.door_port), DOOR_USERS=os.path.join(etc, "users"), DOOR_SECRET=os.path.join(etc, "secret"),
                                    DOOR_MEMBERS=cls.members, DOOR_FLEET_ENV=os.path.join(etc, "fleet.env"),
                                    DOOR_AMUX_URL=f"https://127.0.0.1:{cls.amux_port}", DOOR_MEMBER_RECHECK="0.5"),
                                    stdout=open(os.path.join(cls.tmp, "door.log"), "w"), stderr=subprocess.STDOUT)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", cls.door_port), timeout=0.2).close(); break
            except OSError:
                if cls.door_proc.poll() is not None:
                    raise RuntimeError("door exited: " + open(os.path.join(cls.tmp, "door.log")).read())
                time.sleep(0.1)
        else:
            raise RuntimeError("door never listened: " + open(os.path.join(cls.tmp, "door.log")).read())

    @classmethod
    def tearDownClass(cls):
        for p in (getattr(cls, "door_proc", None), getattr(cls, "amux", None)):
            if p:
                p.terminate(); p.wait(timeout=10)

    @classmethod
    def api(cls, method, path, body=None, form=None, cookie=None):
        data, headers = None, {}
        if body is not None:
            data, headers["Content-Type"] = json.dumps(body).encode(), "application/json"
        if form is not None:
            data = urllib.parse.urlencode(form).encode(); headers["Content-Type"] = "application/x-www-form-urlencoded"
        if cookie:
            headers["Cookie"] = f"amux_member={cookie}"
        req = urllib.request.Request(f"https://127.0.0.1:{cls.amux_port}{path}", data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, context=CTX, timeout=10) as r:
                raw = r.read(); return r.status, (json.loads(raw) if raw[:1] in b"{[" else raw)
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    @classmethod
    def invite(cls, email):
        code, d = cls.api("POST", "/api/org/invites", {"email": email, "scope_level": "global", "grant_host_access": True})
        assert code in (200, 201), (code, d)
        return d.get("token") or d.get("url", "").rsplit("/", 1)[-1]

    @classmethod
    def accept(cls, tok, email):
        return cls.api("POST", f"/invite/{tok}", form={"email": email, "name": email.split("@")[0]})

    def door(self, path, cookie=None):
        c = http.client.HTTPConnection("127.0.0.1", self.door_port, timeout=10)
        c.request("GET", path, headers={"Cookie": cookie or "", "Accept": "application/json",
                                        "X-Forwarded-Method": "GET", "X-Forwarded-Uri": "/api/sessions"})
        r = c.getresponse(); r.read()
        return r.status, dict(r.getheaders()), r.getheader("Set-Cookie") or ""

    def enter(self, email, role="owner", invite=None, secret=None, slug=None, ttl=60):
        t = tokens.sign(dict({"aud": slug or self.slug, "sub": email, "role": role}, **({"invite": invite} if invite else {})),
                        secret or self.secret, ttl)
        code, _, setc = self.door(f"/_door/enter?t={t}")
        cookie = "; ".join(p.split(";")[0] for p in [setc] if p)
        return code, cookie

    def check(self, cookie):
        code, headers, _ = self.door("/_door/check", cookie)
        return code, headers.get("X-Door-User"), headers.get("X-Amux-Member")

    def test_1_owner_enters_and_gets_member_header(self):
        code, cookie = self.enter("owner@example.com")
        self.assertEqual(code, 303)
        c, user, member = self.check(cookie)
        self.assertEqual((c, user), (200, "owner@example.com"))
        self.assertTrue(member)

    def test_0_signed_out_browser_is_sent_to_cloud_for_a_fresh_link(self):
        c = http.client.HTTPConnection("127.0.0.1", self.door_port, timeout=10)
        c.request("GET", "/_door/check", headers={"Accept": "text/html", "X-Forwarded-Method": "GET", "X-Forwarded-Uri": "/"})
        r = c.getresponse(); r.read()
        self.assertEqual((r.status, r.getheader("Location")), (302, "https://cloud.example/?server=alice"))

    def test_2_tokens_for_another_server_expired_or_unknown_people_are_refused(self):
        self.assertEqual(self.enter("owner@example.com", secret=tokens.new_secret())[0], 401)     # other server's key
        self.assertEqual(self.enter("owner@example.com", slug="bob")[0], 401)                     # other server's name
        self.assertEqual(self.enter("owner@example.com", ttl=-5)[0], 401)                         # expired
        self.assertEqual(self.enter("stranger@example.com")[0], 401)                              # not a member
        self.assertEqual(self.check("amux_door=forged.1.abc")[0], 401)

    def test_3_teammate_redeems_invite_wrong_email_refused_then_revoked(self):
        inv = self.invite("dana@example.com")
        self.assertEqual(self.enter("eve@example.com", role="member", invite=inv)[0], 401)        # T-TEAM-4
        code, cookie = self.enter("dana@example.com", role="member", invite=inv)                  # T-TEAM-1
        self.assertEqual(code, 303)
        c, user, member = self.check(cookie)
        self.assertEqual((c, user, member), (200, "dana@example.com", inv))
        self.assertEqual(self.enter("dana@example.com", role="member", invite=inv)[0], 303)       # following the link again
        _, ms = self.api("GET", "/api/org/members")
        mid = next(m["id"] for m in (ms.get("members") if isinstance(ms, dict) else ms) if m["email"] == "dana@example.com")
        self.assertIn(self.api("DELETE", f"/api/org/members/{mid}")[0], (200, 204))                # T-TEAM-6
        time.sleep(1.0)
        self.assertEqual(self.check(cookie)[0], 401)
        self.assertEqual(self.enter("dana@example.com", role="member")[0], 401)


if __name__ == "__main__":
    unittest.main()
