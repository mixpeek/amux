"""The door's address allowlist: off until turned on; when on, an address not on the list reaches
nothing behind the door, signed in or not, except an account's first sign-in, which adds its
address; the owner edits the list at /_door/allowlist; `door.py allowlist seed` lists the addresses
that signed in before (Ethan, 2026-10-31).

  python3 -m unittest cloud/desktop/test_door_allowlist.py
"""
import base64, http.server, importlib.util, json, os, re, socket, subprocess, sys, tempfile, threading, time, unittest, urllib.error, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
DOOR = os.path.join(HERE, "door.py")
OWNER, OWNER_PW = "owner@example.com", "owner-password-1"
MATE, MATE_PW = "mate@example.com", "mate-password-1"
HOME, AWAY, MATE_IP = "198.51.100.1", "203.0.113.50", "203.0.113.60"


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


OPEN = urllib.request.build_opener(NoRedirect)


class DoorAllowlist(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp()
        cls.port = free_port()
        etc = cls.etc = os.path.join(cls.tmp, "etc"); os.makedirs(etc)
        with open(os.path.join(etc, "secret"), "w") as f:
            f.write("door-test-secret")
        lines = "".join(subprocess.run([sys.executable, DOOR, "hash", u], input=pw + "\n", capture_output=True,
                                       text=True, check=True).stdout for u, pw in ((OWNER, OWNER_PW), (MATE, MATE_PW)))
        with open(os.path.join(etc, "users"), "w") as f:
            f.write(lines)
        for n in ("members", "joined"):
            open(os.path.join(etc, n), "w").close()
        with open(os.path.join(etc, "cloudflare-ips"), "w") as f:
            f.write("173.245.48.0/20\n2400:cb00::/32\n")
        cls.allow = os.path.join(etc, "allowlist.json")
        cls.env = dict(os.environ, DOOR_PORT=str(cls.port), DOOR_USERS=os.path.join(etc, "users"),
                       DOOR_SECRET=os.path.join(etc, "secret"), DOOR_MEMBERS=os.path.join(etc, "members"),
                       DOOR_JOINED=os.path.join(etc, "joined"), DOOR_FLEET_ENV=os.path.join(etc, "none"),
                       DOOR_ENTRY=os.path.join(etc, "none"), DOOR_ACTIVITY=os.path.join(etc, "activity.json"),
                       DOOR_UPDATE_STATUS=os.path.join(etc, "status.json"), DOOR_ALLOWLIST=cls.allow,
                       DOOR_FAIL_LIMIT="50")
        alerts = cls.alerts = []
        cards, patches = cls.cards, cls.patches = [], []
        class FakeAmux(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                out = b"{}"
                if self.path == "/api/alert/owner":
                    alerts.append((self.headers.get("Content-Type"), json.loads(body)))
                if self.path == "/api/board":
                    cards.append(json.loads(body))
                    out = json.dumps({"id": "T-%d" % len(cards)}).encode()
                self.send_response(200); self.send_header("Content-Length", str(len(out))); self.end_headers(); self.wfile.write(out)
            def do_PATCH(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                patches.append((self.path, json.loads(body)))
                self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(b"{}")
            def log_message(self, *a):
                pass
        cls.fake = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeAmux)
        threading.Thread(target=cls.fake.serve_forever, daemon=True).start()
        cls.env.update(DOOR_AMUX_URL="http://127.0.0.1:%d" % cls.fake.server_port, DOOR_REFUSED_ALERTS_PER_DAY="1000")
        cls.log = os.path.join(cls.tmp, "door.log")
        cls.proc = subprocess.Popen([sys.executable, DOOR, "serve"], env=cls.env,
                                    stdout=open(cls.log, "w"), stderr=subprocess.STDOUT)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", cls.port), timeout=0.2).close(); break
            except OSError:
                time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate(); cls.proc.wait(timeout=10)
        cls.fake.shutdown()

    def set_list(self, data):
        if data is None:
            if os.path.exists(self.allow):
                os.remove(self.allow)
            return
        with open(self.allow, "w") as f:
            json.dump(data, f)
        t = time.time_ns() + 10**9 * len(data.get("entries", [])) + 7   # a new mtime every write
        os.utime(self.allow, ns=(t, t))

    def get_list(self):
        with open(self.allow) as f:
            return json.load(f)

    def req(self, path, ip, data=None, headers=None):
        h = {"X-Real-IP": ip, **(headers or {})}
        r = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}", data=data, headers=h)
        try:
            with OPEN.open(r, timeout=10) as resp:
                return resp.status, resp.read().decode(errors="replace"), resp.headers
        except urllib.error.HTTPError as e:
            return e.code, e.read().decode(errors="replace"), e.headers

    def check(self, ip, user=OWNER, pw=OWNER_PW, cookie=None, html=False):
        h = {"X-Forwarded-Uri": "/api/sessions"}
        if cookie:
            h["Cookie"] = cookie
        elif user:
            h["Authorization"] = "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode()
        if html:
            h["Accept"] = "text/html"
        return self.req("/_door/check", ip, headers=h)

    def login(self, ip, user=OWNER, pw=OWNER_PW, **extra):
        data = urllib.parse.urlencode({"username": user, "password": pw, "next": "/", **extra}).encode()
        return self.req("/_door/login", ip, data=data)

    def cookie(self, ip, user=OWNER, pw=OWNER_PW):
        status, _, headers = self.login(ip, user, pw)
        self.assertEqual(status, 303)
        return "; ".join(c.split(";")[0] for c in headers.get_all("Set-Cookie"))

    @staticmethod
    def without_device(cookie):
        return "; ".join(c for c in cookie.split("; ") if not c.startswith("amux_door_device="))

    @staticmethod
    def device_only(cookie):
        return "; ".join(c for c in cookie.split("; ") if c.startswith("amux_door_device="))

    def test_a_device_that_signed_in_on_a_listed_address_works_from_any_address(self):
        # Ethan, 2026-11-06: "doron is stuck again, maybe we need a better approach?" Doron's phone
        # was refused from three new cellular IPv6 addresses overnight.
        self.on((HOME, OWNER), (MATE_IP, MATE))
        cookie = self.cookie(MATE_IP, MATE, MATE_PW)
        self.assertTrue(self.device_only(cookie), "a sign-in on a listed address trusts the device")
        for ip in (AWAY, "2600:382:5989:84cf:4521:d152:2bae:1b0e", "2605:59ca:8010:8da8:f132:3da3:c755:69bf"):
            self.assertEqual(self.check(ip, cookie=cookie)[0], 200, ip)
            self.assertEqual(self.req("/_door/ipcheck", ip, headers={"Cookie": cookie})[0], 200, ip)
        # Signed out, the device still signs in from a new address, and files no request.
        n = len(self.cards)
        data = urllib.parse.urlencode({"username": MATE, "password": MATE_PW, "next": "/"}).encode()
        status = self.req("/_door/login", "203.0.113.120", data=data, headers={"Cookie": self.device_only(cookie)})[0]
        self.assertEqual(status, 303)
        self.assertEqual(self.check("203.0.113.120", cookie=self.without_device(cookie))[0], 403,
                         "the session alone is not the device")
        time.sleep(0.3)
        self.assertEqual(len(self.cards), n)
        self.assertNotIn("203.0.113.120", [e["ip"] for e in self.get_list()["entries"]], "the device is trusted, not the address")

    def test_a_new_device_with_the_right_password_is_still_refused_and_a_forged_device_is_nobody(self):
        self.on((HOME, OWNER), (MATE_IP, MATE))
        self.assertEqual(self.login("203.0.113.121", MATE, MATE_PW)[0], 401)
        forged = "amux_door_device=device:%s:0.%d.%s" % (MATE, int(time.time()) + 999, "0" * 64)
        self.assertEqual(self.check(AWAY, user=None, cookie=forged)[0], 403)
        real = self.device_only(self.cookie(MATE_IP, MATE, MATE_PW))
        other = real.replace(MATE, OWNER, 1)
        self.assertEqual(self.req("/_door/ipcheck", AWAY, headers={"Cookie": other})[0], 403, "the account is signed")

    def test_a_device_is_trusted_for_its_own_account_only(self):
        self.on((HOME, OWNER), (MATE_IP, MATE))
        device = self.device_only(self.cookie(MATE_IP, MATE, MATE_PW))
        data = urllib.parse.urlencode({"username": OWNER, "password": OWNER_PW, "next": "/"}).encode()
        self.assertNotEqual(self.req("/_door/login", AWAY, data=data, headers={"Cookie": device})[0], 303,
                            "the owner's password on the mate's device is a new sign-in from a new address")
        self.assertNotIn(AWAY, [e["ip"] for e in self.get_list()["entries"]])
        owner = self.cookie(HOME)
        on_mates = self.without_device(owner) + "; " + device
        self.assertEqual(self.req("/_door/allowlist", AWAY, headers={"Cookie": on_mates})[0], 403,
                         "the allowlist page needs a listed address or the admin's own device")
        self.assertEqual(self.req("/_door/allowlist", AWAY, headers={"Cookie": owner})[0], 200)

    def test_refresh_on_an_unlisted_address_does_not_trust_the_device(self):
        self.on((HOME, OWNER))
        session = self.without_device(self.cookie(HOME))
        status, _, headers = self.req("/_door/refresh?next=/", AWAY, headers={"Cookie": session})
        self.assertEqual(status, 302)
        self.assertFalse([c for c in headers.get_all("Set-Cookie") or [] if c.startswith("amux_door_device=")])
        status, _, headers = self.req("/_door/refresh?next=/", HOME, headers={"Cookie": session})
        self.assertTrue([c for c in headers.get_all("Set-Cookie") or [] if c.startswith("amux_door_device=")],
                        "a session from before devices existed picks one up on a listed address")

    def test_the_owner_forgets_an_accounts_devices(self):
        self.on((HOME, OWNER), (MATE_IP, MATE))
        device = self.device_only(self.cookie(MATE_IP, MATE, MATE_PW))
        self.assertEqual(self.req("/_door/ipcheck", AWAY, headers={"Cookie": device})[0], 200)
        owner = self.cookie(HOME)
        self.assertIn("Trusted devices", self.req("/_door/allowlist", HOME, headers={"Cookie": owner})[1])
        self.assertEqual(self.admin_post(HOME, owner, action="forget_devices", user=MATE)[0], 200)
        self.assertEqual(self.req("/_door/ipcheck", AWAY, headers={"Cookie": device})[0], 403)
        again = self.device_only(self.cookie(MATE_IP, MATE, MATE_PW))
        self.assertEqual(self.req("/_door/ipcheck", AWAY, headers={"Cookie": again})[0], 200, "signing in on a listed address trusts it again")

    def on(self, *entries, first=True):
        self.set_list({"enabled": True, "first_signin": first, "admins": [OWNER],
                       "entries": [{"ip": ip, "user": u, "how": "test", "added": 0} for ip, u in entries]})

    def test_off_lets_any_address_sign_in(self):
        self.set_list(None)
        self.assertEqual(self.check(AWAY)[0], 200)
        self.set_list({"enabled": False, "entries": []})
        self.assertEqual(self.check(AWAY)[0], 200)

    def test_on_blocks_an_unlisted_address_even_with_the_right_password_or_a_cookie(self):
        self.on((HOME, OWNER))
        self.assertEqual(self.check(HOME)[0], 200)
        cookie = self.without_device(self.cookie(HOME))
        status, body, _ = self.check(AWAY)
        self.assertEqual(status, 403)
        self.assertIn("allowlist", body)
        self.assertEqual(self.check(AWAY, cookie=cookie)[0], 403)        # a session cookie carried to a new address
        self.assertEqual(self.check(HOME, cookie=cookie)[0], 200)
        status, _, headers = self.check(AWAY, user=None, html=True)     # a browser is sent to the sign-in
        self.assertEqual(status, 302)
        self.assertIn("/_door/login", headers["Location"])
        self.assertEqual(self.req("/_door/ipcheck", AWAY)[0], 403)      # the apps origin (:8443)
        self.assertEqual(self.req("/_door/ipcheck", HOME)[0], 200)
        self.on(("198.51.100.0/24", OWNER))                              # a range covers its addresses
        self.assertEqual(self.check("198.51.100.77")[0], 200)
        with open(self.log) as f:
            self.assertIn("door_allowlist_blocked", f.read())

    def test_an_unlisted_address_gets_one_answer_whatever_the_password(self):
        self.on((HOME, OWNER))
        right = self.login(AWAY)
        wrong = self.login(AWAY, pw="not-it-1234")
        self.assertEqual((right[0], wrong[0]), (401, 401))
        self.assertIn("not on this server&#x27;s allowlist", right[1])
        self.assertEqual(right[1], wrong[1])
        self.assertNotIn(AWAY, [e["ip"] for e in self.get_list()["entries"]])

    def test_an_unlisted_address_pays_the_same_time_and_throttle_whatever_the_password(self):
        self.on((HOME, OWNER))
        t0 = time.monotonic(); self.login("203.0.113.70"); right = time.monotonic() - t0
        t0 = time.monotonic(); self.login("203.0.113.71", pw="not-it-1234"); wrong = time.monotonic() - t0
        self.assertGreater(right, 0.9)   # a right password must not come back faster than a wrong one
        self.assertLess(abs(right - wrong), 0.5)
        with open(self.log) as f:
            refused = [json.loads(l) for l in f if "door_signin_allowlist_refused" in l]
        self.assertIn("203.0.113.70", [r["ip"] for r in refused])

    def alerts_for(self, ip):
        return [body for _, body in self.alerts if ip in body["message"]]

    def test_a_valid_sign_in_refused_tells_the_owner_once_and_a_guess_tells_nobody(self):
        self.on((HOME, OWNER))
        ip = "203.0.113.80"
        self.login(ip)
        for _ in range(50):
            if self.alerts_for(ip):
                break
            time.sleep(0.1)
        got = self.alerts_for(ip)
        self.assertEqual(len(got), 1)
        self.assertIn(OWNER, got[0]["message"])
        self.assertNotIn(OWNER_PW, json.dumps(got[0]))
        self.assertEqual(got[0]["session"], "amux-door")
        self.assertEqual(self.alerts[-1][0], "application/json")
        self.login(ip)
        guess = "203.0.113.81"
        self.login(guess, pw="not-it-1234")
        time.sleep(0.5)
        self.assertEqual(len(self.alerts_for(ip)), 1, "the second refusal in the hour stays quiet")
        self.assertEqual(self.alerts_for(guess), [], "a wrong password pages nobody")

    def wait_for(self, fn, what):
        for _ in range(50):
            got = fn()
            if got:
                return got
            time.sleep(0.1)
        self.fail("never saw " + what)

    def cards_for(self, ip):
        return [c for c in self.cards if ip in c["title"]]

    def test_a_valid_sign_in_refused_becomes_one_needs_you_card_and_a_guess_files_none(self):
        # Ethan, 2026-11-05: "add alerts here so i receive it. should be a needs you".
        self.on((HOME, OWNER))
        ip, guess = "203.0.113.90", "203.0.113.91"
        self.login(ip)
        card = self.wait_for(lambda: self.cards_for(ip), "the card")[0]
        self.assertEqual(card["status"], "needsyou")
        self.assertEqual(card["ask_type"], "access")
        self.assertNotIn(card["ask_actor"].strip().lower(), ("human", "user", "owner", "someone", "you", "me"))
        self.assertIn("owner", card["ask_actor"], "the needs-you queue lists asks addressed to the owner")
        self.assertTrue(card["ask_question"].endswith("?"))
        self.assertIn(OWNER, card["ask_question"])
        self.assertIn("(/_door/allowlist)", card["desc"], "a relative link: no Host header builds it")
        self.assertNotIn(OWNER_PW, json.dumps(card))
        self.wait_for(lambda: any(r.get("card") for r in self.get_list().get("requests", [])), "the card id stored")
        self.login(ip)
        self.login(guess, pw="not-it-1234")
        time.sleep(0.5)
        self.assertEqual(len(self.cards_for(ip)), 1, "one card per account and address")
        self.assertEqual(self.cards_for(guess), [], "a wrong password files nothing")
        req = [r for r in self.get_list()["requests"] if r["ip"] == ip]
        self.assertEqual([(r["user"], r["count"]) for r in req], [(OWNER, 2)])
        self.assertEqual([r for r in self.get_list()["requests"] if r["ip"] == guess], [])

    def test_the_owner_allows_a_waiting_request_with_one_press_and_its_card_closes(self):
        self.on((HOME, OWNER), (MATE_IP, MATE))
        new = "203.0.113.92"
        self.assertEqual(self.login(new, MATE, MATE_PW)[0], 401)
        self.wait_for(lambda: any(r.get("card") for r in self.get_list().get("requests", []) if r["ip"] == new), "the card")
        cookie = self.cookie(HOME)
        page = self.req("/_door/allowlist", HOME, headers={"Cookie": cookie})[1]
        self.assertIn("Waiting for you", page)
        self.assertIn(new, page)
        status, body, _ = self.admin_post(HOME, cookie, action="allow_request", ip=new, user=MATE)
        self.assertEqual(status, 200, body)
        self.assertIn("They can sign in now", body)
        self.assertEqual(self.login(new, MATE, MATE_PW)[0], 303)
        self.assertNotIn(new, [r["ip"] for r in self.get_list().get("requests", [])])
        entry = [e for e in self.get_list()["entries"] if e["ip"] == new][0]
        self.assertEqual((entry["user"], entry["how"]), (MATE, "owner_approved"))
        card = [c for c in self.cards if new in c["title"]]
        path, patch = self.wait_for(lambda: [p for p in self.patches if p[0] == "/api/board/T-%d" % (self.cards.index(card[0]) + 1)], "the close")[0]
        self.assertEqual(patch["status"], "done")
        self.assertTrue(patch["evidence"].startswith("none: "))
        self.assertEqual(self.admin_post(HOME, cookie, action="allow_request", ip=new, user=MATE)[0], 400, "answered once")

    def test_a_dismissed_request_stays_out_and_a_forged_allow_is_refused(self):
        self.on((HOME, OWNER), (MATE_IP, MATE))
        new = "203.0.113.93"
        self.login(new, MATE, MATE_PW)
        self.wait_for(lambda: [r for r in self.get_list().get("requests", []) if r["ip"] == new], "the request")
        cookie = self.cookie(HOME)
        data = urllib.parse.urlencode({"csrf": "0" * 64, "action": "allow_request", "ip": new, "user": MATE}).encode()
        self.assertEqual(self.req("/_door/allowlist", HOME, data=data, headers={"Cookie": cookie})[0], 403)
        mate = self.cookie(MATE_IP, MATE, MATE_PW)
        self.assertEqual(self.req("/_door/allowlist", MATE_IP, headers={"Cookie": mate})[0], 403, "only the owner answers")
        self.assertEqual(self.admin_post(HOME, cookie, action="dismiss_request", ip=new, user=MATE)[0], 200)
        self.assertEqual(self.login(new, MATE, MATE_PW)[0], 401)
        self.assertNotIn(new, [e["ip"] for e in self.get_list()["entries"]])

    def test_simultaneous_additions_all_survive(self):
        import importlib.util, threading
        path = os.path.join(self.tmp, "race.json")
        with open(path, "w") as f:
            json.dump({"enabled": True, "entries": []}, f)
        spec = importlib.util.spec_from_file_location("door_race", DOOR)
        door = importlib.util.module_from_spec(spec)
        os.environ["DOOR_ALLOWLIST"] = path
        try:
            spec.loader.exec_module(door)
        finally:
            del os.environ["DOOR_ALLOWLIST"]
        door.log = lambda **kv: None
        ips = ["192.0.2.%d" % i for i in range(1, 41)]
        threads = [threading.Thread(target=door.allow_add, args=(ip, "u%s" % ip, "first_signin")) for ip in ips]
        for t in threads: t.start()
        for t in threads: t.join()
        with open(path) as f:
            self.assertEqual(sorted(e["ip"] for e in json.load(f)["entries"]), sorted(ips))

    def test_first_sign_in_adds_the_address_once(self):
        self.on((HOME, OWNER))
        status, body, _ = self.req("/_door/login", MATE_IP)
        self.assertIn("first sign-in", body)
        self.assertEqual(self.login(MATE_IP, MATE, MATE_PW)[0], 303)
        self.assertIn({"ip": MATE_IP, "user": MATE, "how": "first_signin"},
                      [{k: e[k] for k in ("ip", "user", "how")} for e in self.get_list()["entries"]])
        self.assertEqual(self.check(MATE_IP, MATE, MATE_PW)[0], 200)
        self.assertEqual(self.login("203.0.113.61", MATE, MATE_PW)[0], 401)   # the second address is the owner's call

    def test_first_sign_in_can_be_turned_off(self):
        self.on((HOME, OWNER), first=False)
        self.assertEqual(self.login(MATE_IP, MATE, MATE_PW)[0], 401)
        self.assertEqual(len(self.get_list()["entries"]), 1)

    def test_owner_can_lock_the_server_at_first_sign_in(self):
        self.set_list(None)
        self.assertIn('name="lock"', self.req("/_door/login", HOME)[1])
        self.assertEqual(self.login(HOME, lock="1")[0], 303)
        al = self.get_list()
        self.assertTrue(al["enabled"])
        self.assertEqual([e["ip"] for e in al["entries"]], [HOME])
        self.assertEqual(self.check(AWAY)[0], 403)
        self.assertNotIn('name="lock"', self.req("/_door/login", HOME)[1])   # decided once

    def admin_post(self, src, cookie, **form):
        page = self.req("/_door/allowlist", src, headers={"Cookie": cookie})
        self.assertEqual(page[0], 200, page[1][:200])
        csrf = re.search(r'name="csrf" value="([0-9a-f]+)"', page[1]).group(1)
        data = urllib.parse.urlencode({"csrf": csrf, **form}).encode()
        return self.req("/_door/allowlist", src, data=data, headers={"Cookie": cookie})

    def test_owner_page_adds_and_removes_and_will_not_lock_the_owner_out(self):
        self.on((HOME, OWNER))
        cookie = self.cookie(HOME)
        status, body, _ = self.admin_post(HOME, cookie, action="add", ip="192.0.2.9", note="phone")
        self.assertEqual(status, 200, body)
        self.assertEqual(self.check("192.0.2.9")[0], 200)
        self.assertEqual(self.admin_post(HOME, cookie, action="remove", ip="192.0.2.9")[0], 200)
        self.assertEqual(self.check("192.0.2.9")[0], 403)
        status, body, _ = self.admin_post(HOME, cookie, action="remove", ip=HOME)
        self.assertEqual(status, 400)
        self.assertIn("lock out your own address", body)
        self.assertEqual(self.admin_post(HOME, cookie, action="add", ip="0.0.0.0/0")[0], 400)
        self.assertEqual(self.admin_post(HOME, cookie, action="off")[0], 200)
        self.assertFalse(self.get_list()["enabled"])
        self.assertEqual(self.admin_post(HOME, cookie, action="on")[0], 200)
        self.assertTrue(self.get_list()["enabled"])

    def test_owner_page_refuses_forged_posts_other_people_and_unlisted_addresses(self):
        self.on((HOME, OWNER), (MATE_IP, MATE))
        cookie = self.cookie(HOME)
        data = urllib.parse.urlencode({"csrf": "0" * 64, "action": "off"}).encode()
        self.assertEqual(self.req("/_door/allowlist", HOME, data=data, headers={"Cookie": cookie})[0], 403)
        mate = self.cookie(MATE_IP, MATE, MATE_PW)
        self.assertEqual(self.req("/_door/allowlist", MATE_IP, headers={"Cookie": mate})[0], 403)
        self.assertEqual(self.req("/_door/allowlist", AWAY, headers={"Cookie": self.without_device(cookie)})[0], 403)
        self.assertTrue(self.get_list()["enabled"])

    def test_seed_lists_only_addresses_that_signed_in(self):
        logf = os.path.join(self.tmp, "access.log")
        rows = [("198.51.100.5", "ethan", 200), ("198.51.100.5", "ethan", 200), ("198.51.100.6", "", 200),
                ("198.51.100.7", "ethan", 401), ("173.245.48.9", "ethan", 200), ("2001:db8::5", "doron", 200)]
        with open(logf, "w") as f:
            for ip, user, st in rows:
                f.write(json.dumps({"request": {"client_ip": ip}, "door_user": user, "status": st}) + "\n")
        self.set_list({"enabled": False, "entries": []})
        env = dict(self.env, PYTHONPATH="")
        script = ("import sys; sys.argv=['door.py','allowlist','seed','--apply',%r]; sys.path.insert(0,%r); import door; "
                  "door.allowlist_cli(sys.argv[2:])" % (logf, HERE))
        out = subprocess.run([sys.executable, "-c", script.replace("import door; ", "import door; door.seed_from_logs.__defaults__ = (%r,); " % os.path.join(self.etc, "cloudflare-ips"))],
                             env=env, capture_output=True, text=True, check=True).stdout
        self.assertIn("2 address(es)", out)
        self.assertEqual(sorted(e["ip"] for e in self.get_list()["entries"]), ["198.51.100.5", "2001:db8::5"])


class RefusedSigninAlertLimits(unittest.TestCase):
    """The limits, on the shipped functions: per address per hour, then per day."""
    def setUp(self):
        spec = importlib.util.spec_from_file_location("door_under_test", DOOR)
        self.door = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.door)
        self.sent = []
        self.door._send_refused_alert = lambda user, ip: self.sent.append(ip)
        self.door.REFUSED_ALERTS_PER_DAY = 2

    def settle(self):
        for t in threading.enumerate():
            if t is not threading.current_thread() and t.daemon:
                t.join(1)

    def test_one_per_address_per_hour_then_a_daily_cap(self):
        a = self.door.alert_refused_signin
        t = 1_000_000.0
        self.assertTrue(a("u", "1.1.1.1", now=t))
        self.assertFalse(a("u", "1.1.1.1", now=t + 3599), "same address inside the hour")
        self.assertTrue(a("u", "1.1.1.1", now=t + 3601), "same address after the hour")
        self.assertFalse(a("u", "2.2.2.2", now=t + 3602), "the day's cap")
        self.assertTrue(a("u", "2.2.2.2", now=t + 86400 + 3602), "the cap is a rolling day")
        self.settle()
        self.assertEqual(self.sent, ["1.1.1.1", "1.1.1.1", "2.2.2.2"])


if __name__ == "__main__":
    unittest.main()
