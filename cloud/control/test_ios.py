"""iOS sign-in, door links and account deletion over real HTTP (T-IOS-1, T-IOS-3, T-IOS-4 server side, T-DEL-1)."""
import http.client, json, os, socket, sys, tempfile, threading, time, unittest, urllib.parse
from http.server import ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
os.environ.setdefault("CONTROL_FAKE", "1")
import accounts, apple, app, deletion  # noqa: E402
from fakes import FakeBilling, FakeInfra  # noqa: E402
from store import Store  # noqa: E402


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class IOS(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        app.STORE = Store(os.path.join(tempfile.mkdtemp(), "c.db"))
        app.BILLING, app.INFRA = FakeBilling(), FakeInfra()
        app.BILLING.sessions = {}
        app.SECRET = "test-secret"
        app.FAKE = True
        cls.port = free_port()
        srv = ThreadingHTTPServer(("127.0.0.1", cls.port), app.Handler)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        cls.identities = {}
        cls._real_verify = apple.verify_identity_token
        apple.verify_identity_token = lambda tok, **kw: cls.identities[tok] if tok in cls.identities else (_ for _ in ()).throw(ValueError("bad"))
        # A paying customer with a ready server.
        app.BILLING.sessions["cs_1"] = {"id": "cs_1", "status": "complete", "payment_status": "paid", "mode": "subscription",
                                        "customer_details": {"email": "pat@example.com"}, "customer": "cus_1", "subscription": "sub_1",
                                        "amount_total": 3900, "payment_intent": "pi_1", "metadata": {"plan": "solo"}}
        app.BILLING.subs["sub_1"] = {"id": "sub_1", "status": "active", "current_period_end": time.time() + 9e5}
        r = accounts.complete_checkout(app.STORE, app.BILLING, "cs_1")
        cls.server_id = r["server"]["id"]
        h = app.INFRA.create_server("amux-pat", "cx33", "fsn1", "g", {"amux-cloud": "1"}, "")
        app.STORE.run("UPDATE servers SET status='ready', hetzner_id=?, dns_record_id='rec1', backup_bucket='amux-pat-x' WHERE id=?",
                      h["id"], cls.server_id)

    @classmethod
    def tearDownClass(cls):
        apple.verify_identity_token = cls._real_verify

    def call(self, method, path, body=None, token=None, cookie=None, ctype="application/json"):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        headers = {}
        if body is not None:
            headers["Content-Type"] = ctype
        if token:
            headers["Authorization"] = f"Bearer {token}"
        if cookie:
            headers["Cookie"] = cookie
        c.request(method, path, body=(json.dumps(body) if ctype == "application/json" and body is not None else body), headers=headers)
        r = c.getresponse(); raw = r.read()
        try:
            data = json.loads(raw)
        except ValueError:
            data = raw.decode(errors="replace")
        return r.status, data, r.getheader("Location"), r.getheader("Set-Cookie")

    def test_1_unlinked_apple_id_gets_a_link_never_an_account(self):
        self.identities["relay"] = {"sub": "apple-1", "email": "x@privaterelay.appleid.com", "email_verified": True, "private_relay": True}
        n_users = app.STORE.one("SELECT COUNT(*) n FROM users")["n"]
        code, d, _, _ = self.call("POST", "/api/apple/signin", {"identity_token": "relay"})
        self.assertEqual(code, 404)
        self.assertTrue(d["link_required"])
        self.assertEqual(app.STORE.one("SELECT COUNT(*) n FROM users")["n"], n_users)       # T-IOS-3: the app creates nothing
        # Linking: web sign-in with the link, from the app
        link = urllib.parse.parse_qs(urllib.parse.urlparse(d["link_url"]).query)["link"][0]
        _, _, _, setc = self.call("GET", f"/signin?from=ios&link={urllib.parse.quote(link)}")
        ios_cookie = setc.split(";")[0]
        code, _, loc, _ = self.call("POST", "/api/session", "email=pat%40example.com", cookie=ios_cookie,
                                    ctype="application/x-www-form-urlencoded")
        self.assertEqual(code, 303)
        self.assertTrue(loc.startswith("amux://signed-in?token="))
        self.assertEqual(app.STORE.one("SELECT apple_sub FROM users WHERE email='pat@example.com'")["apple_sub"], "apple-1")
        # Now the same Apple ID signs straight in
        code, d, _, _ = self.call("POST", "/api/apple/signin", {"identity_token": "relay"})
        self.assertEqual(code, 200)
        type(self).app_token = d["token"]

    def test_2_app_token_lists_servers_and_gets_a_door_link(self):
        code, me, _, _ = self.call("GET", "/api/me", token=self.app_token)
        self.assertEqual(code, 200)
        self.assertEqual([s["id"] for s in me["servers"]], [self.server_id])
        code, d, _, _ = self.call("GET", f"/api/go-url/{self.server_id}", token=self.app_token)
        self.assertEqual(code, 200)
        self.assertRegex(d["url"], r"^https://pat\.amux\.cloud/_door/enter\?t=")
        self.assertEqual(self.call("GET", f"/api/go-url/{self.server_id}", token="forged")[0], 401)
        # A web session token is not an app token
        web = app.tokens.sign({"sub": me and app.STORE.one("SELECT id FROM users")["id"]}, app.SECRET, 60)
        self.assertEqual(self.call("GET", "/api/me", token=web)[0], 401)

    def test_25_cli_device_login(self):
        code, d, _, _ = self.call("POST", "/api/device/start", {})
        self.assertEqual(code, 200)
        self.assertRegex(d["user_code"], r"^[A-Z2-9]{4}-[A-Z2-9]{4}$")
        self.assertEqual(self.call("POST", "/api/device/poll", {"device_code": d["device_code"]})[0], 202)
        web = app.tokens.sign({"sub": app.STORE.one("SELECT id FROM users WHERE email='pat@example.com'")["id"]}, app.SECRET, 600)
        cookie = f"{app.COOKIE}={web}"
        code, _, _, _ = self.call("POST", "/device", f"code={d['user_code']}&decision=approve", cookie=cookie,
                                  ctype="application/x-www-form-urlencoded")
        self.assertEqual(code, 200)
        code, r, _, _ = self.call("POST", "/api/device/poll", {"device_code": d["device_code"]})
        self.assertEqual(code, 200)
        self.assertEqual(self.call("GET", "/api/me", token=r["token"])[0], 200)
        self.assertEqual(self.call("POST", "/api/device/poll", {"device_code": d["device_code"]})[0], 410,
                         "a used approval does not hand out a second token")

    def test_3_bad_apple_token_is_refused(self):
        self.assertEqual(self.call("POST", "/api/apple/signin", {"identity_token": "nope"})[0], 401)

    def test_4_account_deletion_removes_everything_then_sends_a_receipt(self):
        app.STORE.run("UPDATE users SET apple_refresh='rt-1' WHERE email='pat@example.com'")
        self.assertEqual(self.call("POST", "/api/account/delete", {"confirm": "no"}, token=self.app_token)[0], 400)
        code, d, _, _ = self.call("POST", "/api/account/delete", {"confirm": "delete"}, token=self.app_token)
        self.assertEqual(code, 202)
        deletion.run(app.STORE, app.BILLING, app.INFRA, revoke_apple=None)             # Apple not configured yet
        d1 = app.STORE.one("SELECT * FROM deletions")
        self.assertIsNone(d1["done_at"], "a step that could not run must not count as done")
        revoked = []
        deletion.run(app.STORE, app.BILLING, app.INFRA, revoke_apple=revoked.append)
        d2 = app.STORE.one("SELECT * FROM deletions")
        self.assertIsNotNone(d2["done_at"])
        self.assertEqual(revoked, ["rt-1"])
        self.assertEqual(app.INFRA.servers, {})
        self.assertIn("amux-pat-x", app.INFRA.deleted_buckets)
        self.assertEqual(app.BILLING.canceled, ["sub_1"])
        self.assertIsNone(app.STORE.one("SELECT * FROM users WHERE email='pat@example.com'"))
        self.assertNotIn("rt-1", d2["steps"], "the Apple token is not kept after revocation")
        receipt = app.STORE.one("SELECT * FROM outbox WHERE template='deleted'")
        self.assertIn("server pat", receipt["params"])
        self.assertEqual(self.call("GET", "/api/me", token=self.app_token)[0], 401)   # T-IOS-4: the token no longer works


if __name__ == "__main__":
    unittest.main()
